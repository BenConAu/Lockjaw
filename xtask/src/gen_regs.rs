//! `cargo xtask gen-regs` — generate `user/lockjaw-regs/src/<device>.rs`
//! from declarative TOML specs in `user/regspecs/`.
//!
//! Phase 2 establishes the pipeline: parser understands the full schema
//! every later phase needs (so the schema can't strand a future device);
//! emitter handles the basic features PL011 uses (plain RO/RW/WO/W1C,
//! single-bit flags, multi-bit fields, enum-valued fields). Later
//! phases extend the emitter to handle stream, big-endian, aliased,
//! combined_trigger, passwd_protected, and `[[descriptors]]`.
//!
//! Schema risk lives here: if PL011's exercise of the schema reveals
//! gaps, fix the schema NOW — before Phase 3 drives the first
//! production driver through generated code.
//!
//! Drift: generated files are committed. `--check` regenerates
//! in-memory and `diff`s against committed; non-zero exit on drift.

use serde::Deserialize;
use std::collections::BTreeSet;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::process;

const REGSPECS_DIR: &str = "user/regspecs";
const OUTPUT_DIR: &str = "user/lockjaw-regs/src";

pub fn run(check: bool) {
    let specs = load_all_specs(REGSPECS_DIR);
    if specs.is_empty() {
        eprintln!("FAIL: no specs found under {}", REGSPECS_DIR);
        process::exit(1);
    }

    let mut drifted = Vec::new();
    let mut emitted = 0usize;
    let mut skipped = 0usize;

    for (path, spec) in &specs {
        validate(spec, path);
        if !spec.device.emit {
            println!(
                "[gen-regs] skip {} (emit=false; schema parsed OK)",
                spec.device.name
            );
            skipped += 1;
            continue;
        }

        let generated = emit_device(spec);
        let out_path = PathBuf::from(OUTPUT_DIR)
            .join(format!("{}.rs", to_snake(&spec.device.name)));

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
            println!("[gen-regs] emit {}", out_path.display());
        }
        emitted += 1;
    }

    if check && !drifted.is_empty() {
        eprintln!(
            "FAIL: {} generated file(s) out of date — run `cargo xtask gen-regs`",
            drifted.len()
        );
        process::exit(1);
    }

    println!(
        "[gen-regs] {} ({} emitted, {} parsed-only)",
        if check { "OK — no drift" } else { "OK" },
        emitted,
        skipped
    );
}

// ---------------------------------------------------------------------------
// Spec data model — full schema. Parser accepts every feature any phase
// needs. Emitter implements only what PL011 uses in Phase 2; later
// phases turn on the extras incrementally as drivers convert.
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
struct Spec {
    device: DeviceMeta,
    #[serde(default)]
    registers: Vec<Register>,
    #[serde(default)]
    descriptors: Vec<Descriptor>,
}

#[derive(Deserialize, Debug)]
struct DeviceMeta {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default = "default_true")]
    emit: bool,
    // Optional pointer at a lockjaw-types module whose constants the
    // emitter should cross-check offsets against. Reserved for a later
    // phase; parsing it now keeps specs forward-compatible.
    #[serde(default)]
    #[allow(dead_code)]
    verify_against: Option<String>,
}

fn default_true() -> bool { true }

#[derive(Deserialize, Debug)]
struct Register {
    name: String,
    offset: u64,
    width: u8,
    access: Access,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    kind: Option<RegisterKind>,
    #[serde(default)]
    flags: Vec<Flag>,
    #[serde(default)]
    fields: Vec<Field>,
    #[serde(default)]
    parts: Vec<Part>,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    endian: Option<Endian>,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Access {
    Ro,
    Rw,
    Wo,
    W1c,
    Trigger,
}

#[derive(Deserialize, Debug, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum RegisterKind {
    CombinedTrigger,
    Aliased,
    Stream,
    PasswdProtected,
}

#[derive(Deserialize, Debug, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum Endian {
    Big,
    #[allow(dead_code)]
    Little,
}

#[derive(Deserialize, Debug)]
struct Flag {
    name: String,
    bit: u8,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Deserialize, Debug)]
struct Field {
    name: String,
    bits: String,
    #[serde(default)]
    #[allow(dead_code)]
    description: Option<String>,
    #[serde(default)]
    enum_values: Vec<EnumValue>,
}

#[derive(Deserialize, Debug)]
struct EnumValue {
    name: String,
    value: u64,
}

#[derive(Deserialize, Debug)]
struct Part {
    #[allow(dead_code)]
    name: String,
    #[allow(dead_code)]
    bits: String,
}

#[derive(Deserialize, Debug)]
struct Descriptor {
    #[allow(dead_code)]
    name: String,
    #[allow(dead_code)]
    width: u8,
    #[serde(default)]
    #[allow(dead_code)]
    description: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    fields: Vec<Field>,
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
    // Track every offset already claimed, AND whether it was claimed by an
    // aliased register. Aliased registers may overlap with other aliased
    // registers at the same offset (that's the point); a plain register
    // overlapping an aliased one — or vice versa — is a bug.
    let mut seen_offsets: std::collections::BTreeMap<u64, /* aliased = */ bool> =
        std::collections::BTreeMap::new();
    for reg in &spec.registers {
        if !matches!(reg.width, 8 | 16 | 32 | 64) {
            panic!(
                "{}: register `{}` has unsupported width {}; must be 8/16/32/64",
                path.display(), reg.name, reg.width
            );
        }
        let byte_width = reg.width as u64 / 8;
        if reg.offset % byte_width != 0 {
            panic!(
                "{}: register `{}` offset 0x{:x} not aligned to {} bytes",
                path.display(), reg.name, reg.offset, byte_width
            );
        }
        let is_aliased = matches!(reg.kind, Some(RegisterKind::Aliased));
        match seen_offsets.get(&reg.offset).copied() {
            Some(prev_aliased) => {
                if !(prev_aliased && is_aliased) {
                    panic!(
                        "{}: register `{}` offset 0x{:x} collides with an earlier register; \
                         only aliased registers may share an offset (and BOTH sides must declare \
                         kind = \"aliased\")",
                        path.display(), reg.name, reg.offset
                    );
                }
            }
            None => {
                seen_offsets.insert(reg.offset, is_aliased);
            }
        }
        // Flags and fields are mutually exclusive: a register is EITHER a
        // bag of single-bit flags OR has multi-bit fields. (Mixing makes
        // accessor naming and reserved-bit semantics ambiguous.)
        if !reg.flags.is_empty() && !reg.fields.is_empty() {
            panic!(
                "{}: register `{}` has both `flags` and `fields` — pick one",
                path.display(), reg.name
            );
        }
        for f in &reg.flags {
            if (f.bit as u32) >= reg.width as u32 {
                panic!(
                    "{}: register `{}` flag `{}` bit {} >= width {}",
                    path.display(), reg.name, f.name, f.bit, reg.width
                );
            }
        }
        for f in &reg.fields {
            validate_field(f, reg.width, &format!("register `{}`", reg.name), path);
        }
        // Combined-trigger parts: bit ranges must fit in register width.
        // Used by Phase 7 SDHCI (TRANSFER_MODE + COMMAND fired as one u32).
        for p in &reg.parts {
            let (hi, lo) = parse_bits(&p.bits).unwrap_or_else(|| {
                panic!(
                    "{}: register `{}` part `{}` bad bits {:?}",
                    path.display(), reg.name, p.name, p.bits
                )
            });
            if hi >= reg.width || lo > hi {
                panic!(
                    "{}: register `{}` part `{}` bits {}:{} out of range for width {}",
                    path.display(), reg.name, p.name, hi, lo, reg.width
                );
            }
        }
    }
    // Descriptor sections (Phase 7 SDHCI ADMA2): bit ranges within the
    // descriptor's declared width. Validated even when emit=false so future
    // descriptor specs catch out-of-range bits at `gen-regs --check` time.
    for desc in &spec.descriptors {
        if !matches!(desc.width, 8 | 16 | 32 | 64) {
            panic!(
                "{}: descriptor `{}` has unsupported width {}; must be 8/16/32/64",
                path.display(), desc.name, desc.width
            );
        }
        for f in &desc.fields {
            validate_field(f, desc.width, &format!("descriptor `{}`", desc.name), path);
        }
    }
}

fn validate_field(f: &Field, container_width: u8, container_label: &str, path: &Path) {
    let (hi, lo) = parse_bits(&f.bits).unwrap_or_else(|| {
        panic!(
            "{}: {} field `{}` bad bits {:?}",
            path.display(), container_label, f.name, f.bits
        )
    });
    if hi >= container_width || lo > hi {
        panic!(
            "{}: {} field `{}` bits {}:{} out of range for width {}",
            path.display(), container_label, f.name, hi, lo, container_width
        );
    }
    let field_width = (hi - lo + 1) as u32;
    let max_val = if field_width >= 64 { u64::MAX } else { (1u64 << field_width) - 1 };
    for ev in &f.enum_values {
        if ev.value > max_val {
            panic!(
                "{}: {} field `{}` enum `{}` value 0x{:x} > max 0x{:x}",
                path.display(), container_label, f.name, ev.name, ev.value, max_val
            );
        }
    }
}

fn parse_bits(s: &str) -> Option<(u8, u8)> {
    let (h, l) = s.split_once(':')?;
    let hi: u8 = h.trim().parse().ok()?;
    let lo: u8 = l.trim().parse().ok()?;
    Some((hi, lo))
}

// ---------------------------------------------------------------------------
// Emission — string-based, deterministic. Indentation is hand-managed
// because the generated file is committed and reviewers should read it.
// ---------------------------------------------------------------------------

fn emit_device(spec: &Spec) -> String {
    // Reject features the Phase 2 emitter doesn't implement yet. Skeletal
    // specs that need these set emit=false and we never get here for them.
    // The one exception is `kind = "stream"`, which we implement now
    // because PL011 DATA needs it: read and write target independent
    // FIFOs, so `modify_*` would corrupt UART traffic. (fw_cfg in Phase 5
    // will reuse the same emitter path at u8 width.)
    for reg in &spec.registers {
        if let Some(kind) = &reg.kind {
            if !matches!(kind, RegisterKind::Stream) {
                panic!(
                    "Phase 2 emitter does not support register kind {:?} (used by `{}` in device `{}`). \
                     Mark the spec emit=false until the corresponding phase lands the emitter extension.",
                    kind, reg.name, spec.device.name
                );
            }
            if !reg.flags.is_empty() || !reg.fields.is_empty() {
                panic!(
                    "stream register `{}` in `{}` cannot have flags or fields: read and write target \
                     different state, so a typed-snapshot value would be meaningless",
                    reg.name, spec.device.name
                );
            }
        }
        if reg.endian.is_some() {
            panic!(
                "Phase 2 emitter does not support per-register endian (used by `{}` in device `{}`). \
                 Endian support lands in Phase 5.",
                reg.name, spec.device.name
            );
        }
        if !reg.parts.is_empty() || !reg.aliases.is_empty() {
            panic!(
                "Phase 2 emitter does not support parts/aliases (used by `{}` in device `{}`).",
                reg.name, spec.device.name
            );
        }
    }
    if !spec.descriptors.is_empty() {
        panic!(
            "Phase 2 emitter does not support [[descriptors]] (used by `{}`). Lands in Phase 7.",
            spec.device.name
        );
    }

    let mut out = String::new();
    emit_header(&mut out, spec);
    emit_layout(&mut out, spec);
    for reg in &spec.registers {
        if !reg.flags.is_empty() {
            emit_flags_newtype(&mut out, reg);
        }
        if !reg.fields.is_empty() {
            emit_fields_newtype(&mut out, reg);
        }
    }
    emit_reserved_bits(&mut out, spec);
    emit_accessors(&mut out, spec);
    emit_tests(&mut out, spec);
    out
}

fn emit_header(out: &mut String, spec: &Spec) {
    let desc = spec.device.description.as_deref().unwrap_or("");
    writeln!(out, "//! {}", desc).unwrap();
    writeln!(out, "//!").unwrap();
    writeln!(out, "//! GENERATED FILE — do not edit by hand.").unwrap();
    writeln!(out, "//! Source: user/regspecs/{}.toml", to_kebab(&spec.device.name)).unwrap();
    writeln!(out, "//! Regenerate with: `cargo xtask gen-regs`.").unwrap();
    writeln!(out, "//! Drift is caught by: `cargo xtask gen-regs --check` (CI).").unwrap();
    writeln!(out).unwrap();
    // dead_code: not every accessor is used by every driver.
    // missing_docs: the spec is the source of truth for documentation; mirroring
    // descriptions onto every generated item (constants, enum variants, mask
    // constants) would inflate diffs without adding meaning.
    writeln!(out, "#![allow(dead_code, missing_docs)]").unwrap();
    writeln!(out).unwrap();
    // Only emit imports for cell types this device actually uses, to avoid
    // unused-import warnings under crate-level deny(warnings) configs.
    let mut needed: BTreeSet<&'static str> = BTreeSet::new();
    for reg in &spec.registers {
        match reg.access {
            Access::Ro => { needed.insert("Ro"); }
            Access::Rw => { needed.insert("Rw"); }
            Access::Wo | Access::Trigger => { needed.insert("Wo"); }
            Access::W1c => { needed.insert("W1c"); }
        }
    }
    let imports: Vec<&str> = needed.iter().copied().collect();
    writeln!(out, "use lockjaw_mmio::cell::{{{}}};", imports.join(", ")).unwrap();
    writeln!(out).unwrap();
}

fn emit_layout(out: &mut String, spec: &Spec) {
    let dev = &spec.device.name;
    if let Some(desc) = &spec.device.description {
        writeln!(out, "/// {}", desc).unwrap();
    }
    writeln!(out, "#[repr(C)]").unwrap();
    writeln!(out, "pub struct {} {{", dev).unwrap();
    let mut cursor = 0u64;
    let mut pad_idx = 0usize;
    let mut regs: Vec<&Register> = spec.registers.iter().collect();
    regs.sort_by_key(|r| r.offset);
    for reg in &regs {
        if reg.offset > cursor {
            let gap = reg.offset - cursor;
            writeln!(
                out,
                "    _pad{}: [u8; 0x{:x}],",
                pad_idx, gap
            ).unwrap();
            pad_idx += 1;
        } else if reg.offset < cursor {
            panic!(
                "register `{}` offset 0x{:x} < cursor 0x{:x} (overlap)",
                reg.name, reg.offset, cursor
            );
        }
        if let Some(desc) = &reg.description {
            writeln!(out, "    /// {}", desc).unwrap();
        }
        writeln!(
            out,
            "    {}: {},",
            reg.name,
            cell_type(reg.access, reg.width)
        ).unwrap();
        cursor = reg.offset + (reg.width as u64 / 8);
    }
    writeln!(out, "}}").unwrap();
    writeln!(out).unwrap();
}

fn cell_type(access: Access, width: u8) -> String {
    let wty = format!("u{}", width);
    match access {
        Access::Ro => format!("Ro<{}>", wty),
        Access::Rw => format!("Rw<{}>", wty),
        Access::Wo | Access::Trigger => format!("Wo<{}>", wty),
        Access::W1c => format!("W1c<{}>", wty),
    }
}

fn emit_flags_newtype(out: &mut String, reg: &Register) {
    let ty = to_pascal(&reg.name);
    let wty = format!("u{}", reg.width);
    writeln!(out, "// ---------- {} ----------", ty).unwrap();
    writeln!(out).unwrap();
    writeln!(out, "/// {} register — typed snapshot.", ty).unwrap();
    writeln!(out, "#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]").unwrap();
    writeln!(out, "#[repr(transparent)]").unwrap();
    writeln!(out, "pub struct {ty}(pub {wty});").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "impl {ty} {{").unwrap();
    writeln!(out, "    /// Empty (no bits set).").unwrap();
    writeln!(out, "    pub const fn empty() -> Self {{ Self(0) }}").unwrap();
    writeln!(out, "    /// Underlying bit pattern.").unwrap();
    writeln!(out, "    pub const fn bits(self) -> {wty} {{ self.0 }}").unwrap();
    writeln!(out, "    /// True if every bit set in `other` is set in `self`.").unwrap();
    writeln!(out, "    pub const fn contains(self, other: Self) -> bool {{").unwrap();
    writeln!(out, "        (self.0 & other.0) == other.0").unwrap();
    writeln!(out, "    }}").unwrap();
    for f in &reg.flags {
        if let Some(desc) = &f.description {
            writeln!(out, "    /// {}", desc).unwrap();
        }
        writeln!(
            out,
            "    pub const {}: Self = Self(1 << {});",
            f.name.to_uppercase(),
            f.bit
        ).unwrap();
    }
    writeln!(out, "}}").unwrap();
    writeln!(out).unwrap();
    // Bit ops — let drivers compose flags ergonomically.
    for (trait_name, method, op) in [
        ("BitOr", "bitor", "|"),
        ("BitAnd", "bitand", "&"),
    ] {
        writeln!(out, "impl core::ops::{trait_name} for {ty} {{").unwrap();
        writeln!(out, "    type Output = Self;").unwrap();
        writeln!(out, "    fn {method}(self, rhs: Self) -> Self {{ Self(self.0 {op} rhs.0) }}").unwrap();
        writeln!(out, "}}").unwrap();
    }
    writeln!(out, "impl core::ops::Not for {ty} {{").unwrap();
    writeln!(out, "    type Output = Self;").unwrap();
    writeln!(out, "    fn not(self) -> Self {{ Self(!self.0) }}").unwrap();
    writeln!(out, "}}").unwrap();
    writeln!(out).unwrap();
}

fn emit_fields_newtype(out: &mut String, reg: &Register) {
    let ty = to_pascal(&reg.name);
    let wty = format!("u{}", reg.width);
    writeln!(out, "// ---------- {} ----------", ty).unwrap();
    writeln!(out).unwrap();
    writeln!(out, "/// {} register — typed snapshot with field accessors.", ty).unwrap();
    writeln!(out, "#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]").unwrap();
    writeln!(out, "#[repr(transparent)]").unwrap();
    writeln!(out, "pub struct {ty}(pub {wty});").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "impl {ty} {{").unwrap();
    writeln!(out, "    /// Underlying bit pattern.").unwrap();
    writeln!(out, "    pub const fn bits(self) -> {wty} {{ self.0 }}").unwrap();

    for f in &reg.fields {
        let (hi, lo) = parse_bits(&f.bits).expect("validated");
        let field_width = (hi - lo + 1) as u32;
        let mask = if field_width >= reg.width as u32 {
            // Whole-register field.
            mask_for_width(reg.width)
        } else {
            (((1u64 << field_width) - 1) << lo) as u64
        };
        let shift = lo;
        let mask_const = format!("{}_MASK", f.name.to_uppercase());
        let shift_const = format!("{}_SHIFT", f.name.to_uppercase());
        writeln!(out, "    /// Mask for the `{}` field (bits {}:{}).", f.name, hi, lo).unwrap();
        writeln!(out, "    pub const {mask_const}: {wty} = 0x{:x};", mask).unwrap();
        writeln!(out, "    /// Right-shift to access the `{}` field.", f.name).unwrap();
        writeln!(out, "    pub const {shift_const}: u32 = {shift};").unwrap();

        if field_width == 1 && f.enum_values.is_empty() {
            // Boolean field.
            writeln!(
                out,
                "    /// Read `{}` as bool.", f.name
            ).unwrap();
            writeln!(
                out,
                "    pub const fn {}(self) -> bool {{ (self.0 & Self::{mask_const}) != 0 }}",
                f.name
            ).unwrap();
            writeln!(
                out,
                "    /// Return a new value with `{}` set to `v`.", f.name
            ).unwrap();
            writeln!(
                out,
                "    pub const fn with_{}(self, v: bool) -> Self {{",
                f.name
            ).unwrap();
            writeln!(
                out,
                "        if v {{ Self(self.0 | Self::{mask_const}) }} else {{ Self(self.0 & !Self::{mask_const}) }}"
            ).unwrap();
            writeln!(out, "    }}").unwrap();
        } else if !f.enum_values.is_empty() {
            let enum_ty = format!("{}{}", ty, to_pascal(&f.name));
            writeln!(out, "    /// Decode the `{}` field as `{}`.", f.name, enum_ty).unwrap();
            writeln!(
                out,
                "    pub const fn {}(self) -> Result<{enum_ty}, ReservedBits> {{",
                f.name
            ).unwrap();
            writeln!(
                out,
                "        {enum_ty}::from_bits((self.0 & Self::{mask_const}) >> Self::{shift_const})"
            ).unwrap();
            writeln!(out, "    }}").unwrap();
            writeln!(out, "    /// Return a new value with `{}` set to `v`.", f.name).unwrap();
            writeln!(out, "    pub const fn with_{}(self, v: {enum_ty}) -> Self {{", f.name).unwrap();
            writeln!(
                out,
                "        Self((self.0 & !Self::{mask_const}) | ((v.into_bits() as {wty}) << Self::{shift_const}))"
            ).unwrap();
            writeln!(out, "    }}").unwrap();
        } else {
            // Multi-bit scalar.
            writeln!(out, "    /// Read the `{}` field as a scalar.", f.name).unwrap();
            writeln!(
                out,
                "    pub const fn {}(self) -> {wty} {{ (self.0 & Self::{mask_const}) >> Self::{shift_const} }}",
                f.name
            ).unwrap();
            writeln!(out, "    /// Return a new value with `{}` set to `v` (truncated to field width).", f.name).unwrap();
            writeln!(out, "    pub const fn with_{}(self, v: {wty}) -> Self {{", f.name).unwrap();
            writeln!(
                out,
                "        Self((self.0 & !Self::{mask_const}) | ((v << Self::{shift_const}) & Self::{mask_const}))"
            ).unwrap();
            writeln!(out, "    }}").unwrap();
        }
    }
    writeln!(out, "}}").unwrap();
    writeln!(out).unwrap();

    // Emit enum types for any enum_values fields.
    for f in &reg.fields {
        if f.enum_values.is_empty() {
            continue;
        }
        emit_enum(out, ty.as_str(), reg.width, f);
    }
}

fn emit_enum(out: &mut String, reg_ty: &str, reg_width: u8, f: &Field) {
    let enum_ty = format!("{}{}", reg_ty, to_pascal(&f.name));
    let wty = format!("u{}", reg_width);
    // Use u8 as repr if values fit, else fall back to the register width.
    // PL011 enums all fit u8.
    let repr = if f.enum_values.iter().all(|v| v.value <= u8::MAX as u64) {
        "u8"
    } else if f.enum_values.iter().all(|v| v.value <= u16::MAX as u64) {
        "u16"
    } else if f.enum_values.iter().all(|v| v.value <= u32::MAX as u64) {
        "u32"
    } else {
        "u64"
    };

    writeln!(out, "/// Enum for the `{}` field of `{}`.", f.name, reg_ty).unwrap();
    writeln!(out, "#[derive(Copy, Clone, Debug, PartialEq, Eq)]").unwrap();
    writeln!(out, "#[repr({repr})]").unwrap();
    writeln!(out, "pub enum {enum_ty} {{").unwrap();
    for ev in &f.enum_values {
        writeln!(out, "    {} = 0x{:x},", ev.name, ev.value).unwrap();
    }
    writeln!(out, "}}").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "impl {enum_ty} {{").unwrap();
    writeln!(out, "    /// Decode from raw bits. Returns `Err(ReservedBits)` if the").unwrap();
    writeln!(out, "    /// pattern is not a defined variant.").unwrap();
    writeln!(out, "    pub const fn from_bits(v: {wty}) -> Result<Self, ReservedBits> {{").unwrap();
    writeln!(out, "        match v {{").unwrap();
    for ev in &f.enum_values {
        writeln!(out, "            0x{:x} => Ok(Self::{}),", ev.value, ev.name).unwrap();
    }
    writeln!(out, "            _ => Err(ReservedBits(v as u64)),").unwrap();
    writeln!(out, "        }}").unwrap();
    writeln!(out, "    }}").unwrap();
    writeln!(out, "    /// Encode to raw bits.").unwrap();
    writeln!(out, "    pub const fn into_bits(self) -> {wty} {{ self as {wty} }}").unwrap();
    writeln!(out, "}}").unwrap();
    writeln!(out).unwrap();
}

fn emit_reserved_bits(out: &mut String, _spec: &Spec) {
    writeln!(out, "/// Returned when an enum decode sees a bit pattern that does not").unwrap();
    writeln!(out, "/// correspond to any declared variant.").unwrap();
    writeln!(out, "#[derive(Copy, Clone, Debug, PartialEq, Eq)]").unwrap();
    writeln!(out, "pub struct ReservedBits(pub u64);").unwrap();
    writeln!(out).unwrap();
}

fn emit_accessors(out: &mut String, spec: &Spec) {
    let dev = &spec.device.name;
    writeln!(out, "// ---------- {} accessors ----------", dev).unwrap();
    writeln!(out).unwrap();
    writeln!(out, "impl {dev} {{").unwrap();
    for reg in &spec.registers {
        let wty = format!("u{}", reg.width);
        let has_typed = !reg.flags.is_empty() || !reg.fields.is_empty();
        let ty = if has_typed { to_pascal(&reg.name) } else { wty.clone() };
        let raw_read = |out: &mut String| {
            writeln!(out, "    /// Volatile read of `{}` as `{wty}`.", reg.name).unwrap();
            writeln!(out, "    #[inline(always)]").unwrap();
            writeln!(out, "    pub fn read_{}(&self) -> {wty} {{ self.{}.read() }}", reg.name, reg.name).unwrap();
        };
        let raw_write = |out: &mut String| {
            writeln!(out, "    /// Volatile write of `{}`.", reg.name).unwrap();
            writeln!(out, "    #[inline(always)]").unwrap();
            writeln!(out, "    pub fn write_{}(&self, v: {wty}) {{ self.{}.write(v); }}", reg.name, reg.name).unwrap();
        };
        match reg.access {
            Access::Ro => {
                if has_typed {
                    writeln!(out, "    /// Read a typed snapshot of `{}`.", reg.name).unwrap();
                    writeln!(out, "    #[inline(always)]").unwrap();
                    writeln!(out, "    pub fn {}(&self) -> {ty} {{ {ty}(self.{}.read()) }}", reg.name, reg.name).unwrap();
                } else {
                    raw_read(out);
                }
            }
            Access::Rw => {
                if has_typed {
                    writeln!(out, "    /// Read a typed snapshot of `{}`.", reg.name).unwrap();
                    writeln!(out, "    #[inline(always)]").unwrap();
                    writeln!(out, "    pub fn {}(&self) -> {ty} {{ {ty}(self.{}.read()) }}", reg.name, reg.name).unwrap();
                    writeln!(out, "    /// Write the value back to `{}`.", reg.name).unwrap();
                    writeln!(out, "    #[inline(always)]").unwrap();
                    writeln!(out, "    pub fn set_{}(&self, v: {ty}) {{ self.{}.write(v.0); }}", reg.name, reg.name).unwrap();
                    writeln!(out, "    /// Read-modify-write `{}` via a typed closure.", reg.name).unwrap();
                    writeln!(out, "    #[inline(always)]").unwrap();
                    writeln!(out, "    pub fn modify_{}<F: FnOnce({ty}) -> {ty}>(&self, f: F) {{", reg.name).unwrap();
                    writeln!(out, "        self.{}.modify(|v| f({ty}(v)).0);", reg.name).unwrap();
                    writeln!(out, "    }}").unwrap();
                } else if matches!(reg.kind, Some(RegisterKind::Stream)) {
                    // Stream port: read and write touch independent state.
                    // Emit read_/write_ but NOT modify_ — RMW is incoherent.
                    raw_read(out);
                    raw_write(out);
                } else {
                    raw_read(out);
                    raw_write(out);
                    writeln!(out, "    /// Read-modify-write `{}`.", reg.name).unwrap();
                    writeln!(out, "    #[inline(always)]").unwrap();
                    writeln!(out, "    pub fn modify_{}<F: FnOnce({wty}) -> {wty}>(&self, f: F) {{", reg.name).unwrap();
                    writeln!(out, "        self.{}.modify(f);", reg.name).unwrap();
                    writeln!(out, "    }}").unwrap();
                }
            }
            Access::Wo | Access::Trigger => {
                if has_typed {
                    writeln!(out, "    /// Write a typed value to `{}`.", reg.name).unwrap();
                    writeln!(out, "    #[inline(always)]").unwrap();
                    writeln!(out, "    pub fn set_{}(&self, v: {ty}) {{ self.{}.write(v.0); }}", reg.name, reg.name).unwrap();
                } else {
                    raw_write(out);
                }
            }
            Access::W1c => {
                // W1C in the spec means "clear-only". Many W1C registers in
                // hardware (notably PL011's ICR) have undefined read semantics
                // — the readable counterpart is a separate RO status register
                // (RIS/MIS on PL011, NORMAL_INT_STATUS on SDHCI). The codegen
                // does NOT expose a typed read accessor for W1C: a `clear_*`
                // is all the typed surface a driver needs. If a future device
                // needs read+clear in one register, it'll model the readable
                // side as a separate `ro` entry at the same offset (via the
                // `aliased` kind landing in Phase 7).
                if has_typed {
                    writeln!(out, "    /// Clear bits in `{}` (write-1-to-clear).", reg.name).unwrap();
                    writeln!(out, "    #[inline(always)]").unwrap();
                    writeln!(out, "    pub fn clear_{}(&self, mask: {ty}) {{ self.{}.clear(mask.0); }}", reg.name, reg.name).unwrap();
                } else {
                    writeln!(out, "    /// Clear bits in `{}` (write-1-to-clear).", reg.name).unwrap();
                    writeln!(out, "    #[inline(always)]").unwrap();
                    writeln!(out, "    pub fn clear_{}(&self, mask: {wty}) {{ self.{}.clear(mask); }}", reg.name, reg.name).unwrap();
                }
            }
        }
    }
    writeln!(out, "}}").unwrap();
    writeln!(out).unwrap();
}

fn emit_tests(out: &mut String, spec: &Spec) {
    let dev = &spec.device.name;
    writeln!(out, "#[cfg(test)]").unwrap();
    writeln!(out, "mod tests {{").unwrap();
    writeln!(out, "    use super::*;").unwrap();
    writeln!(out, "    use core::mem::offset_of;").unwrap();
    writeln!(out).unwrap();

    writeln!(out, "    #[test]").unwrap();
    writeln!(out, "    fn layout_offsets() {{").unwrap();
    for reg in &spec.registers {
        writeln!(
            out,
            "        assert_eq!(offset_of!({dev}, {}), 0x{:x}, \"{} offset\");",
            reg.name, reg.offset, reg.name
        ).unwrap();
    }
    writeln!(out, "    }}").unwrap();
    writeln!(out).unwrap();

    // For each flags register: validate bit positions.
    for reg in &spec.registers {
        if reg.flags.is_empty() {
            continue;
        }
        let ty = to_pascal(&reg.name);
        writeln!(out, "    #[test]").unwrap();
        writeln!(out, "    fn {}_flag_bits() {{", reg.name).unwrap();
        for f in &reg.flags {
            writeln!(
                out,
                "        assert_eq!({ty}::{}.bits(), 1 << {}, \"{}\");",
                f.name.to_uppercase(),
                f.bit,
                f.name
            ).unwrap();
        }
        writeln!(out, "    }}").unwrap();
        writeln!(out).unwrap();
        writeln!(out, "    #[test]").unwrap();
        writeln!(out, "    fn {}_flag_compose() {{", reg.name).unwrap();
        if reg.flags.len() >= 2 {
            let a = reg.flags[0].name.to_uppercase();
            let b = reg.flags[1].name.to_uppercase();
            writeln!(out, "        let v = {ty}::{a} | {ty}::{b};").unwrap();
            writeln!(out, "        assert!(v.contains({ty}::{a}));").unwrap();
            writeln!(out, "        assert!(v.contains({ty}::{b}));").unwrap();
            writeln!(out, "        assert!(!{ty}::{a}.contains({ty}::{b}));").unwrap();
        } else {
            writeln!(out, "        // Only one flag declared; compose test trivial.").unwrap();
            writeln!(out, "        let v = {ty}::empty();").unwrap();
            writeln!(out, "        assert_eq!(v.bits(), 0);").unwrap();
        }
        writeln!(out, "    }}").unwrap();
        writeln!(out).unwrap();
    }

    // For each fields register: roundtrip + reserved-bit preservation.
    for reg in &spec.registers {
        if reg.fields.is_empty() {
            continue;
        }
        let ty = to_pascal(&reg.name);
        let wty = format!("u{}", reg.width);

        // Per-field roundtrip.
        writeln!(out, "    #[test]").unwrap();
        writeln!(out, "    fn {}_field_roundtrip() {{", reg.name).unwrap();
        for f in &reg.fields {
            let (hi, lo) = parse_bits(&f.bits).expect("validated");
            let width = (hi - lo + 1) as u32;
            if width == 1 && f.enum_values.is_empty() {
                writeln!(out, "        // bool field `{}`", f.name).unwrap();
                writeln!(out, "        let on = {ty}::default().with_{}(true);", f.name).unwrap();
                writeln!(out, "        assert!(on.{}());", f.name).unwrap();
                writeln!(out, "        let off = on.with_{}(false);", f.name).unwrap();
                writeln!(out, "        assert!(!off.{}());", f.name).unwrap();
            } else if !f.enum_values.is_empty() {
                let enum_ty = format!("{}{}", ty, to_pascal(&f.name));
                writeln!(out, "        // enum field `{}`", f.name).unwrap();
                for ev in &f.enum_values {
                    writeln!(
                        out,
                        "        assert_eq!({ty}::default().with_{}({enum_ty}::{}).{}().unwrap(), {enum_ty}::{});",
                        f.name, ev.name, f.name, ev.name
                    ).unwrap();
                }
            } else {
                writeln!(out, "        // scalar field `{}`", f.name).unwrap();
                let max = if width >= reg.width as u32 {
                    mask_for_width(reg.width)
                } else {
                    (1u64 << width) - 1
                };
                writeln!(
                    out,
                    "        for v in [0 as {wty}, 1, 0x{max:x}] {{",
                ).unwrap();
                writeln!(
                    out,
                    "            assert_eq!({ty}::default().with_{}(v).{}(), v);",
                    f.name, f.name
                ).unwrap();
                writeln!(out, "        }}").unwrap();
            }
        }
        writeln!(out, "    }}").unwrap();
        writeln!(out).unwrap();

        // Reserved-bit preservation: set every bit to 1, modify one field,
        // verify bits outside the field mask are unchanged.
        writeln!(out, "    #[test]").unwrap();
        writeln!(out, "    fn {}_preserves_reserved_bits() {{", reg.name).unwrap();
        writeln!(out, "        let all_ones: {wty} = !0;").unwrap();
        for f in &reg.fields {
            let mask_const = format!("{}::{}_MASK", ty, f.name.to_uppercase());
            if (parse_bits(&f.bits).unwrap().0 - parse_bits(&f.bits).unwrap().1 + 1) as u32 == 1
                && f.enum_values.is_empty()
            {
                writeln!(out, "        // toggle bool `{}` without disturbing other bits", f.name).unwrap();
                writeln!(out, "        let v = {ty}(all_ones).with_{}(false);", f.name).unwrap();
                writeln!(out, "        assert_eq!(v.0 & !{mask_const}, all_ones & !{mask_const});").unwrap();
            } else if !f.enum_values.is_empty() {
                let enum_ty = format!("{}{}", ty, to_pascal(&f.name));
                let first = &f.enum_values[0].name;
                writeln!(out, "        // set enum `{}` to a known variant", f.name).unwrap();
                writeln!(out, "        let v = {ty}(all_ones).with_{}({enum_ty}::{first});", f.name).unwrap();
                writeln!(out, "        assert_eq!(v.0 & !{mask_const}, all_ones & !{mask_const});").unwrap();
            } else {
                writeln!(out, "        // set scalar `{}` to 0", f.name).unwrap();
                writeln!(out, "        let v = {ty}(all_ones).with_{}(0);", f.name).unwrap();
                writeln!(out, "        assert_eq!(v.0 & !{mask_const}, all_ones & !{mask_const});").unwrap();
            }
        }
        writeln!(out, "    }}").unwrap();
        writeln!(out).unwrap();

        // Enum-specific decode/reserved tests.
        for f in &reg.fields {
            if f.enum_values.is_empty() {
                continue;
            }
            let enum_ty = format!("{}{}", ty, to_pascal(&f.name));
            writeln!(out, "    #[test]").unwrap();
            writeln!(out, "    fn {}_{}_enum_decode() {{", reg.name, f.name).unwrap();
            for ev in &f.enum_values {
                writeln!(
                    out,
                    "        assert_eq!({enum_ty}::from_bits(0x{:x}), Ok({enum_ty}::{}));",
                    ev.value, ev.name
                ).unwrap();
            }
            // Find a bit pattern outside the declared variants but within field width.
            let (hi, lo) = parse_bits(&f.bits).unwrap();
            let fw = (hi - lo + 1) as u32;
            let max = if fw >= 64 { u64::MAX } else { (1u64 << fw) - 1 };
            let known: std::collections::HashSet<u64> =
                f.enum_values.iter().map(|e| e.value).collect();
            let reserved = (0..=max).find(|v| !known.contains(v));
            if let Some(r) = reserved {
                writeln!(
                    out,
                    "        assert_eq!({enum_ty}::from_bits(0x{:x}), Err(ReservedBits(0x{:x})));",
                    r, r
                ).unwrap();
            }
            writeln!(out, "    }}").unwrap();
            writeln!(out).unwrap();
        }
    }

    writeln!(out, "}}").unwrap();
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn mask_for_width(w: u8) -> u64 {
    match w {
        8 => u8::MAX as u64,
        16 => u16::MAX as u64,
        32 => u32::MAX as u64,
        64 => u64::MAX,
        _ => unreachable!(),
    }
}

fn to_pascal(s: &str) -> String {
    let mut out = String::new();
    let mut cap = true;
    for c in s.chars() {
        if c == '_' || c == '-' {
            cap = true;
        } else if cap {
            for u in c.to_uppercase() {
                out.push(u);
            }
            cap = false;
        } else {
            out.push(c);
        }
    }
    out
}

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

fn to_kebab(s: &str) -> String {
    to_snake(s).replace('_', "-")
}
