//! `cargo xtask gen-wires` — generate `lockjaw-types/src/wire/<family>.rs`
//! from declarative TOML specs in `user/wirespecs/`.
//!
//! Phase 7's DMA codegen, parallel to Phase 2's `gen-regs` MMIO codegen.
//! The two systems own different things:
//!
//! - `gen-regs`  → MMIO register layout in `lockjaw-regs::<device>`
//! - `gen-wires` → DMA-shared struct layout in `lockjaw-types::wire::<family>`
//!
//! The MMIO codegen emits typed cells over `#[repr(C)]` struct fields;
//! drivers express intent through accessor methods that handle volatile
//! load/store + byte order. The DMA codegen mirrors that pattern at the
//! shared-memory layer: each DTO becomes a `#[repr(transparent)]`
//! newtype over `[u8; N]` (size always matches the wire layout, padding
//! is structurally impossible), and field accessors handle byte-order
//! conversion via `to_be_bytes`/`from_be_bytes`/`to_le_bytes`/
//! `from_le_bytes`. The `dma_value_impl!` invocation is emitter output
//! so the sealed-trait + const_assert protection rides along by
//! construction.
//!
//! Drivers stop hand-writing wire formats. The seven hand-written DTOs
//! migrated in Phase 7 (5 virtio + 2 fwcfg) are the proof of concept;
//! Phase 9 (emmc2) adds ADMA2-32 descriptors through the same pipeline.

use serde::Deserialize;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::process;

const WIRESPECS_DIR: &str = "user/wirespecs";
const OUTPUT_DIR: &str = "lockjaw-types/src/wire";

pub fn run(check: bool) {
    let specs = load_all_specs(WIRESPECS_DIR);
    if specs.is_empty() {
        eprintln!("FAIL: no wirespecs found under {}", WIRESPECS_DIR);
        process::exit(1);
    }

    let mut drifted = Vec::new();
    let mut emitted = 0usize;

    for (path, spec) in &specs {
        validate(spec, path);
        let generated = emit_family(spec, path);
        let out_path = PathBuf::from(OUTPUT_DIR).join(format!("{}.rs", spec.family));

        if check {
            let existing = std::fs::read_to_string(&out_path).unwrap_or_default();
            if existing != generated {
                eprintln!(
                    "DRIFT: {} differs from spec ({})",
                    out_path.display(),
                    path.display()
                );
                drifted.push(out_path.clone());
            }
        } else {
            std::fs::write(&out_path, &generated)
                .unwrap_or_else(|e| panic!("write {}: {}", out_path.display(), e));
            println!("[gen-wires] emit {}", out_path.display());
        }
        emitted += 1;
    }

    if check && !drifted.is_empty() {
        eprintln!(
            "FAIL: {} generated file(s) out of date — run `cargo xtask gen-wires`",
            drifted.len()
        );
        process::exit(1);
    }

    println!(
        "[gen-wires] {} ({} emitted)",
        if check { "OK — no drift" } else { "OK" },
        emitted
    );
}

// ---------------------------------------------------------------------------
// Spec data model. One file per device family, multiple `[[wire]]` entries.
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
struct Spec {
    /// Family name — matches the output file `lockjaw-types/src/wire/<family>.rs`.
    family: String,
    /// Free-form description for the generated file's header comment.
    #[serde(default)]
    description: Option<String>,
    /// All DTOs in this family. Order is preserved in emitted code.
    wire: Vec<Wire>,
}

#[derive(Deserialize, Debug)]
struct Wire {
    /// Type name (PascalCase, e.g. `VirtqDesc`).
    name: String,
    /// Free-form description for the generated struct's doc comment.
    #[serde(default)]
    description: Option<String>,
    /// Total wire size in bytes. Validated against the sum of field
    /// sizes — they must equal so the resulting `[u8; size]` newtype
    /// has zero padding by construction.
    size: usize,
    /// Per-DTO default byte order. Per-field `endian` overrides this.
    endian: Endian,
    /// Fields in offset order.
    fields: Vec<Field>,
}

#[derive(Deserialize, Debug)]
struct Field {
    /// Field name (snake_case).
    name: String,
    /// Byte offset within the DTO. Validated for monotonic non-overlap.
    offset: usize,
    /// Field width in bits. Must be 8 / 16 / 32 / 64.
    width: u8,
    /// Optional per-field endian override (defaults to the wire's endian).
    #[serde(default)]
    endian: Option<Endian>,
    /// If set, the constructor omits this field as a parameter and
    /// always writes this value. The accessor is still emitted for
    /// reads (useful for fields the device may update vs fields the
    /// driver always sets to a fixed value, e.g. virtio-blk header's
    /// `reserved` field is always 0 from the driver side).
    #[serde(default)]
    default: Option<u64>,
    /// Free-form description for the field-accessor doc comments.
    #[serde(default)]
    description: Option<String>,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Endian {
    Big,
    Little,
}

impl Endian {
    fn to_bytes_method(&self) -> &'static str {
        match self {
            Endian::Big => "to_be_bytes",
            Endian::Little => "to_le_bytes",
        }
    }
    fn from_bytes_method(&self) -> &'static str {
        match self {
            Endian::Big => "from_be_bytes",
            Endian::Little => "from_le_bytes",
        }
    }
    fn label(&self) -> &'static str {
        match self {
            Endian::Big => "BE",
            Endian::Little => "LE",
        }
    }
}

// ---------------------------------------------------------------------------
// Discovery + parse
// ---------------------------------------------------------------------------

fn load_all_specs(dir: &str) -> Vec<(PathBuf, Spec)> {
    let mut out = Vec::new();
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {}", dir, e));
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("toml"))
        .collect();
    paths.sort();
    for path in paths {
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
        let spec: Spec = toml::from_str(&text)
            .unwrap_or_else(|e| panic!("parse {}: {}", path.display(), e));
        out.push((path, spec));
    }
    out
}

// ---------------------------------------------------------------------------
// Validation — fail loudly on schema misuse before emitting bad code.
// ---------------------------------------------------------------------------

fn validate(spec: &Spec, path: &Path) {
    let mut seen_names = std::collections::HashSet::new();
    for w in &spec.wire {
        if !seen_names.insert(w.name.clone()) {
            panic!(
                "{}: duplicate wire DTO name `{}`",
                path.display(), w.name
            );
        }
        validate_wire(w, path);
    }
}

fn validate_wire(w: &Wire, path: &Path) {
    if w.size == 0 {
        panic!(
            "{}: wire `{}` has size = 0; must be > 0",
            path.display(), w.name
        );
    }
    let mut field_names = std::collections::HashSet::new();
    let mut cursor = 0usize;
    let mut total_field_bytes = 0usize;
    for f in &w.fields {
        if !field_names.insert(f.name.clone()) {
            panic!(
                "{}: wire `{}` has duplicate field `{}`",
                path.display(), w.name, f.name
            );
        }
        if !matches!(f.width, 8 | 16 | 32 | 64) {
            panic!(
                "{}: wire `{}` field `{}` has unsupported width {}; must be 8/16/32/64",
                path.display(), w.name, f.name, f.width
            );
        }
        let bytes = f.width as usize / 8;
        if f.offset != cursor {
            panic!(
                "{}: wire `{}` field `{}` offset {} is not the expected {} \
                 (fields must tile [0..size) with no gaps and no overlaps — \
                 padding violates the `dma_value_impl!` size invariant)",
                path.display(), w.name, f.name, f.offset, cursor
            );
        }
        if f.offset + bytes > w.size {
            panic!(
                "{}: wire `{}` field `{}` extends past declared size {}",
                path.display(), w.name, f.name, w.size
            );
        }
        // Validate default fits in field width.
        if let Some(d) = f.default {
            let max = if f.width == 64 { u64::MAX } else { (1u64 << f.width) - 1 };
            if d > max {
                panic!(
                    "{}: wire `{}` field `{}` default {} > max {} for width {}",
                    path.display(), w.name, f.name, d, max, f.width
                );
            }
        }
        cursor += bytes;
        total_field_bytes += bytes;
    }
    if total_field_bytes != w.size {
        panic!(
            "{}: wire `{}` size = {} but fields sum to {} bytes \
             — the resulting [u8; size] newtype would have padding, \
             violating the DmaValue safety contract",
            path.display(), w.name, w.size, total_field_bytes
        );
    }
}

// ---------------------------------------------------------------------------
// Emission — string-based, deterministic. Indentation is hand-managed
// because the generated file is committed and reviewers should read it.
// ---------------------------------------------------------------------------

fn emit_family(spec: &Spec, path: &Path) -> String {
    let mut out = String::new();
    let desc = spec.description.as_deref().unwrap_or("Wire DTOs");
    writeln!(out, "//! {}", desc).unwrap();
    writeln!(out, "//!").unwrap();
    writeln!(out, "//! GENERATED FILE — do not edit by hand.").unwrap();
    writeln!(out, "//! Source: {}", path.display()).unwrap();
    writeln!(out, "//! Regenerate with: `cargo xtask gen-wires`.").unwrap();
    writeln!(out, "//! Drift is caught by: `cargo xtask gen-wires --check` (CI).").unwrap();
    writeln!(out).unwrap();
    // dead_code: not every accessor is used by every consumer.
    // missing_docs: each generated item carries its own doc; the
    // crate-level missing_docs allow is for the few items where the
    // spec didn't supply a description.
    writeln!(out, "#![allow(dead_code, missing_docs)]").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "use crate::dma_value_impl;").unwrap();
    writeln!(out).unwrap();

    for w in &spec.wire {
        emit_wire(&mut out, w);
    }

    emit_tests(&mut out, spec);
    out
}

fn emit_wire(out: &mut String, w: &Wire) {
    let name = &w.name;
    let size = w.size;
    writeln!(out, "// ---------- {} ----------", name).unwrap();
    writeln!(out).unwrap();
    if let Some(desc) = &w.description {
        writeln!(out, "/// {}", desc).unwrap();
    }
    writeln!(out, "/// Wire layout: {} bytes, {}-endian default.", size, w.endian.label()).unwrap();
    // Debug derived so consumers that include this DTO in their own
    // Debug-derived types compile; format prints raw bytes (semantic
    // pretty-print would need a hand impl per type).
    writeln!(out, "#[derive(Clone, Copy, Debug)]").unwrap();
    writeln!(out, "#[repr(transparent)]").unwrap();
    // Inner byte array is PRIVATE — exposing it would let driver code
    // do `cfg.0[12..16].copy_from_slice(&junk)` and bypass the
    // constructor's byte-order + default-field discipline (e.g.
    // overwrite RamfbConfig's spec-mandated flags = 0). The newtype
    // is construction-safe only if the bytes can't be back-doored.
    // Generated tests live in the same module and access .0 through
    // private-field visibility; external consumers go through
    // accessors + new() exclusively.
    writeln!(out, "pub struct {name}([u8; {size}]);").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "impl {name} {{").unwrap();
    emit_constructor(out, w);
    writeln!(out).unwrap();
    emit_accessors(out, w);
    writeln!(out, "}}").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "dma_value_impl!({name}, size = {size});").unwrap();
    writeln!(out).unwrap();
}

fn emit_constructor(out: &mut String, w: &Wire) {
    // Constructor signature includes every field WITHOUT a `default = N`.
    let ctor_fields: Vec<&Field> = w.fields.iter().filter(|f| f.default.is_none()).collect();
    writeln!(out, "    /// Construct from host-order values. Byte-order").unwrap();
    writeln!(out, "    /// conversion to the on-wire layout is applied").unwrap();
    writeln!(out, "    /// per field at construction time; the resulting").unwrap();
    writeln!(out, "    /// bytes can be written through `DmaCell::write` to").unwrap();
    writeln!(out, "    /// reach the device.").unwrap();
    write!(out, "    pub fn new(").unwrap();
    for (i, f) in ctor_fields.iter().enumerate() {
        if i > 0 {
            write!(out, ", ").unwrap();
        }
        write!(out, "{}: u{}", f.name, f.width).unwrap();
    }
    writeln!(out, ") -> Self {{").unwrap();
    writeln!(out, "        let mut b = [0u8; {}];", w.size).unwrap();
    for f in &w.fields {
        let bytes = f.width as usize / 8;
        let start = f.offset;
        let end = start + bytes;
        let endian = f.endian.unwrap_or(w.endian);
        let to_bytes = endian.to_bytes_method();
        let value = if let Some(d) = f.default {
            format!("{}u{}", d, f.width)
        } else {
            f.name.clone()
        };
        writeln!(
            out,
            "        b[{}..{}].copy_from_slice(&{}.{}());",
            start, end, value, to_bytes
        ).unwrap();
    }
    writeln!(out, "        Self(b)").unwrap();
    writeln!(out, "    }}").unwrap();
}

fn emit_accessors(out: &mut String, w: &Wire) {
    for (i, f) in w.fields.iter().enumerate() {
        if i > 0 {
            writeln!(out).unwrap();
        }
        let bytes = f.width as usize / 8;
        let start = f.offset;
        let end = start + bytes;
        let endian = f.endian.unwrap_or(w.endian);
        let from_bytes = endian.from_bytes_method();
        if let Some(desc) = &f.description {
            writeln!(out, "    /// {}", desc).unwrap();
        } else {
            writeln!(out, "    /// Read the `{}` field as a host-order `u{}`.", f.name, f.width).unwrap();
        }
        writeln!(out, "    #[inline(always)]").unwrap();
        writeln!(out, "    pub fn {}(&self) -> u{} {{", f.name, f.width).unwrap();
        writeln!(out, "        let mut buf = [0u8; {}];", bytes).unwrap();
        writeln!(out, "        buf.copy_from_slice(&self.0[{}..{}]);", start, end).unwrap();
        writeln!(out, "        u{}::{}(buf)", f.width, from_bytes).unwrap();
        writeln!(out, "    }}").unwrap();
    }
}

// ---------------------------------------------------------------------------
// Test emission — one #[cfg(test)] mod tests block per file with per-DTO
// roundtrip, size, alignment, and default-field assertions.
// ---------------------------------------------------------------------------

fn emit_tests(out: &mut String, spec: &Spec) {
    writeln!(out, "#[cfg(test)]").unwrap();
    writeln!(out, "mod tests {{").unwrap();
    writeln!(out, "    use super::*;").unwrap();
    writeln!(out).unwrap();
    for w in &spec.wire {
        emit_wire_tests(out, w);
    }
    writeln!(out, "}}").unwrap();
}

fn emit_wire_tests(out: &mut String, w: &Wire) {
    let name = &w.name;
    let name_snake = to_snake(name);

    // Size + alignment. The size assert is redundant with
    // dma_value_impl!'s const_assert; the alignment assert documents
    // the #[repr(transparent)] over [u8; N] invariant.
    writeln!(out, "    #[test]").unwrap();
    writeln!(out, "    fn {}_size_and_align() {{", name_snake).unwrap();
    writeln!(out, "        assert_eq!(core::mem::size_of::<{name}>(), {});", w.size).unwrap();
    writeln!(out, "        assert_eq!(core::mem::align_of::<{name}>(), 1);").unwrap();
    writeln!(out, "    }}").unwrap();
    writeln!(out).unwrap();

    // Per-field roundtrip: construct with deliberate values, peek raw
    // bytes at the field offset, assert byte pattern matches the
    // declared endian, then read via accessor and assert recovery.
    writeln!(out, "    #[test]").unwrap();
    writeln!(out, "    fn {}_roundtrip() {{", name_snake).unwrap();
    // Construct test value: deterministic per-field pattern.
    let ctor_fields: Vec<&Field> = w.fields.iter().filter(|f| f.default.is_none()).collect();
    let mut ctor_call = format!("{name}::new(");
    let mut expected_reads: Vec<(String, String)> = Vec::new();
    for (i, f) in ctor_fields.iter().enumerate() {
        if i > 0 {
            ctor_call.push_str(", ");
        }
        // Pick a value whose byte pattern differs visibly between BE
        // and LE so a missing or wrong byte-order conversion shows up.
        let val = test_value_for_width(f.width, i);
        ctor_call.push_str(&format!("{}u{}", val, f.width));
        expected_reads.push((f.name.clone(), format!("{}u{}", val, f.width)));
    }
    ctor_call.push(')');
    writeln!(out, "        let v = {};", ctor_call).unwrap();
    writeln!(out).unwrap();
    // Peek raw bytes at each field offset and assert BE/LE pattern.
    for f in &w.fields {
        let bytes = f.width as usize / 8;
        let start = f.offset;
        let end = start + bytes;
        let endian = f.endian.unwrap_or(w.endian);
        let val = if let Some(d) = f.default {
            format!("{}u{}", d, f.width)
        } else {
            let idx = ctor_fields.iter().position(|cf| cf.name == f.name).unwrap();
            format!("{}u{}", test_value_for_width(f.width, idx), f.width)
        };
        writeln!(out, "        // {} field — {}-endian raw bytes", f.name, endian.label()).unwrap();
        writeln!(out, "        let mut expected_{} = [0u8; {}];", f.name, bytes).unwrap();
        writeln!(out, "        expected_{}.copy_from_slice(&{}.{}());", f.name, val, endian.to_bytes_method()).unwrap();
        writeln!(out, "        assert_eq!(&v.0[{}..{}], &expected_{}[..]);", start, end, f.name).unwrap();
    }
    writeln!(out).unwrap();
    // Read back via accessor and confirm host-order recovery.
    for f in &w.fields {
        let expected = if let Some(d) = f.default {
            format!("{}u{}", d, f.width)
        } else {
            let idx = ctor_fields.iter().position(|cf| cf.name == f.name).unwrap();
            format!("{}u{}", test_value_for_width(f.width, idx), f.width)
        };
        writeln!(out, "        assert_eq!(v.{}(), {});", f.name, expected).unwrap();
    }
    writeln!(out, "    }}").unwrap();
    writeln!(out).unwrap();
}

/// Pick a deterministic test value for a field that produces a visibly
/// different byte pattern under BE vs LE encoding. We use width-sized
/// repeating-nibble patterns plus an index nudge so multiple fields
/// in the same DTO don't all share the same value.
fn test_value_for_width(width: u8, field_idx: usize) -> String {
    let nudge = (field_idx as u64) & 0xF;
    match width {
        8 => format!("0x{:02X}", 0x12 + nudge),
        16 => format!("0x{:04X}", 0x1234 + (nudge << 4)),
        32 => format!("0x{:08X}", 0x1234_5678u32 as u64 + (nudge << 16)),
        64 => format!("0x{:016X}", 0x1122_3344_5566_7788u64 + (nudge << 32)),
        _ => unreachable!(),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn to_snake(s: &str) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            out.push('_');
        }
        for l in c.to_lowercase() {
            out.push(l);
        }
    }
    out
}
