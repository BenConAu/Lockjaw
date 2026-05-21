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
    // Phase 4A.2: paired 32-bit low/high registers exposed as a
    // synthesized u64 accessor. Avoids manual driver-side composition.
    #[serde(default)]
    u64_pairs: Vec<U64Pair>,
    // Phase 4A.3: selector + value register pair. Generated helper
    // sequences write-sel + read/write-value internally so the driver
    // never sees the windowed protocol directly.
    #[serde(default)]
    windowed: Vec<Windowed>,
    // Phase 4A.4: per-register binding of generated offset to a
    // hand-written constant in `verify_against` module. Emitter
    // produces `static_assertions::const_assert_eq!` so moving the
    // constant breaks the generated module's build.
    #[serde(default)]
    verify_offsets: Vec<VerifyOffset>,
}

#[derive(Deserialize, Debug)]
struct DeviceMeta {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default = "default_true")]
    emit: bool,
    // Optional pointer at a lockjaw-types module whose constants the
    // emitter cross-checks offsets against via [[verify_offsets]]
    // bindings. Phase 4A.4: parsed AND consumed.
    #[serde(default)]
    verify_against: Option<String>,
}

#[derive(Deserialize, Debug)]
struct U64Pair {
    /// Logical name for the pair (e.g. `blk_capacity` for the
    /// `blk_capacity_low`/`blk_capacity_high` pair). Synthesized
    /// accessor names use this stem: `read_<name>() -> u64` and
    /// `write_<name>(u64)`.
    name: String,
    /// Existing register name supplying the least-significant 32 bits.
    low: String,
    /// Existing register name supplying the most-significant 32 bits.
    high: String,
    /// Reserved for future big-endian halves. Today only `"little"` is
    /// supported; the field exists so per-pair endian travels with the
    /// spec when Phase 6 lands big-endian.
    #[serde(default = "default_endian_little")]
    #[allow(dead_code)]
    endian: U64PairEndian,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum U64PairEndian {
    Little,
    // Big — adds in Phase 6 alongside ramfb/fwcfg endian work.
}

fn default_endian_little() -> U64PairEndian { U64PairEndian::Little }

#[derive(Deserialize, Debug)]
struct Windowed {
    /// Logical name for the windowed pair (e.g. `device_features`).
    /// Synthesized accessor uses this stem: `read_<name>_64()` for
    /// `direction = "read"` or `write_<name>_64(u64)` for `direction
    /// = "write"`.
    name: String,
    /// Register that the synthesized helper writes to before each
    /// access to set the window index.
    selector: String,
    /// Register the synthesized helper reads from or writes to once
    /// the selector is set.
    value: String,
    /// Width of each chunk. Today only 32 (driver_features /
    /// device_features both u32). Other widths reserved for future
    /// devices.
    chunk_width: u8,
    /// Number of chunks the synthesized accessor walks. Today 2
    /// produces a 64-bit accessor; other counts are reserved.
    chunk_count: u8,
    /// Either `"read"` (synthesize `read_<name>_<64*count>()`) or
    /// `"write"` (synthesize `write_<name>_<64*count>(value)`).
    direction: WindowedDirection,
    /// When `true`, the emitter inserts a `dmb_ish` between selector
    /// write and value access. Defaults `false` — virtio doesn't need
    /// it; some chips (cprman, ?) will.
    #[serde(default)]
    requires_barrier: bool,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum WindowedDirection {
    Read,
    Write,
}

#[derive(Deserialize, Debug)]
struct VerifyOffset {
    /// Register name on this device (must exist in `[[registers]]`).
    reg: String,
    /// Constant name in the `verify_against` module that should equal
    /// the register's offset. Emitter produces:
    ///   const_assert_eq!(offset_of!(Dev, reg), <verify_against>::<const>);
    #[serde(rename = "const")]
    const_name: String,
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
    /// Per-field access mode. Defaults to `rw` (driver reads and
    /// writes). `ro` marks hardware-set status fields like SDHCI's
    /// `CARD_INSERTED` or cprman's `BUSY` — the emitter skips the
    /// `with_*` setter and the related test cases so driver code
    /// literally cannot construct a meaningless write to the field.
    ///
    /// Field-level access is independent of the parent register's
    /// access: a `rw` register can carry RO status fields the
    /// hardware updates asynchronously (the typical pattern). When
    /// not specified, behaves as `rw` to keep historical regspecs
    /// emitting unchanged.
    #[serde(default)]
    access: FieldAccess,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
enum FieldAccess {
    /// Field is hardware-set; driver reads only. Emitter omits the
    /// `with_*` setter and the related test cases.
    Ro,
    /// Field is driver-writable (the default).
    #[default]
    Rw,
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
    // Phase 4A.2: u64_pairs cross-reference registers and require both
    // halves to be plain 32-bit, same access direction. Names must
    // not collide with existing register names (synthesized accessor
    // would shadow).
    for pair in &spec.u64_pairs {
        let low = spec.registers.iter().find(|r| r.name == pair.low)
            .unwrap_or_else(|| panic!(
                "{}: u64_pair `{}` low register `{}` not found in [[registers]]",
                path.display(), pair.name, pair.low
            ));
        let high = spec.registers.iter().find(|r| r.name == pair.high)
            .unwrap_or_else(|| panic!(
                "{}: u64_pair `{}` high register `{}` not found in [[registers]]",
                path.display(), pair.name, pair.high
            ));
        if low.width != 32 || high.width != 32 {
            panic!(
                "{}: u64_pair `{}` requires both halves to be 32 bits (got low={}, high={})",
                path.display(), pair.name, low.width, high.width
            );
        }
        if low.access != high.access {
            panic!(
                "{}: u64_pair `{}` halves disagree on access ({:?} vs {:?}); \
                 a u64 helper must be uniformly readable or uniformly writable",
                path.display(), pair.name, low.access, high.access
            );
        }
        if !matches!(low.access, Access::Ro | Access::Rw | Access::Wo) {
            panic!(
                "{}: u64_pair `{}` access {:?} not supported (must be ro/rw/wo)",
                path.display(), pair.name, low.access
            );
        }
        if spec.registers.iter().any(|r| r.name == pair.name) {
            panic!(
                "{}: u64_pair `{}` collides with a register of the same name",
                path.display(), pair.name
            );
        }
    }
    // Phase 4A.3: windowed pairs cross-reference selector + value; both
    // must exist; selector must be writable; value's access matches the
    // declared direction; chunk_width must match the value register's
    // width.
    for win in &spec.windowed {
        let sel = spec.registers.iter().find(|r| r.name == win.selector)
            .unwrap_or_else(|| panic!(
                "{}: windowed `{}` selector `{}` not found",
                path.display(), win.name, win.selector
            ));
        let val = spec.registers.iter().find(|r| r.name == win.value)
            .unwrap_or_else(|| panic!(
                "{}: windowed `{}` value `{}` not found",
                path.display(), win.name, win.value
            ));
        if !matches!(sel.access, Access::Wo | Access::Rw) {
            panic!(
                "{}: windowed `{}` selector `{}` must be writable (got {:?})",
                path.display(), win.name, win.selector, sel.access
            );
        }
        if win.chunk_width != val.width {
            panic!(
                "{}: windowed `{}` chunk_width {} doesn't match value register `{}` width {}",
                path.display(), win.name, win.chunk_width, win.value, val.width
            );
        }
        match win.direction {
            WindowedDirection::Read => {
                if !matches!(val.access, Access::Ro | Access::Rw) {
                    panic!(
                        "{}: windowed `{}` direction=read but value `{}` is not readable ({:?})",
                        path.display(), win.name, win.value, val.access
                    );
                }
            }
            WindowedDirection::Write => {
                if !matches!(val.access, Access::Wo | Access::Rw) {
                    panic!(
                        "{}: windowed `{}` direction=write but value `{}` is not writable ({:?})",
                        path.display(), win.name, win.value, val.access
                    );
                }
            }
        }
        if win.chunk_count == 0 {
            panic!(
                "{}: windowed `{}` chunk_count must be > 0",
                path.display(), win.name
            );
        }
    }
    // Phase 4A.4: verify_offsets reference real registers AND the spec
    // declares the verify_against module. (Cannot validate the const's
    // value here — that's the const_assert_eq! at compile time.)
    if !spec.verify_offsets.is_empty() && spec.device.verify_against.is_none() {
        panic!(
            "{}: [[verify_offsets]] requires [device].verify_against to be set",
            path.display()
        );
    }
    for vo in &spec.verify_offsets {
        if !spec.registers.iter().any(|r| r.name == vo.reg) {
            panic!(
                "{}: verify_offset register `{}` not found",
                path.display(), vo.reg
            );
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
    // Reject features the emitter doesn't implement yet. Skeletal specs
    // that need these set emit=false and we never get here for them.
    //
    // Allowed compounds:
    // - `kind = "stream"` (Phase 2): independent FIFOs; no flags/fields,
    //   no modify_ (RMW is incoherent on a stream).
    // - `kind = "passwd_protected"` (Phase 8): every write OR-s in the
    //   spec-mandated password byte in bits[31:24]; supports plain RW/WO
    //   and typed snapshots (fields). The codegen mechanically enforces
    //   PASSWD on every write so drivers cannot forget — BCM2711 CPRMAN
    //   silently drops writes without PASSWD, which is a debugging
    //   nightmare drivers should never face.
    for reg in &spec.registers {
        if let Some(kind) = &reg.kind {
            match kind {
                RegisterKind::Stream => {
                    if !reg.flags.is_empty() || !reg.fields.is_empty() {
                        panic!(
                            "stream register `{}` in `{}` cannot have flags or fields: read and write target \
                             different state, so a typed-snapshot value would be meaningless",
                            reg.name, spec.device.name
                        );
                    }
                }
                RegisterKind::PasswdProtected => {
                    // CPRMAN's PASSWD lives in bits[31:24] of a 32-bit
                    // register. Smaller widths have no current consumer
                    // and the mask/shift logic would need a per-width
                    // branch in the emitter — defer until a real device
                    // surfaces.
                    if reg.width != 32 {
                        panic!(
                            "passwd_protected register `{}` in `{}` has width {}; only u32 is supported \
                             (PASSWD lives in bits[31:24])",
                            reg.name, spec.device.name, reg.width
                        );
                    }
                    if !matches!(reg.access, Access::Rw | Access::Wo) {
                        panic!(
                            "passwd_protected register `{}` in `{}` has access {:?}; only rw/wo are supported \
                             (passwd is a write-time concern)",
                            reg.name, spec.device.name, reg.access
                        );
                    }
                    if reg.endian.is_some() {
                        panic!(
                            "passwd_protected register `{}` in `{}` combines passwd with `endian`; not supported \
                             (no current device needs both)",
                            reg.name, spec.device.name
                        );
                    }
                }
                _ => {
                    panic!(
                        "emitter does not support register kind {:?} (used by `{}` in device `{}`). \
                         Mark the spec emit=false until the corresponding phase lands the emitter extension.",
                        kind, reg.name, spec.device.name
                    );
                }
            }
        }
        // Phase 6: per-register `endian = "big"` is supported on
        // u16/u32/u64. The emitter wraps reads with `uN::from_be(...)`
        // and writes with `v.to_be()`. LE is the default; specs omit
        // `endian` for LE. BE+u8 is not exercised by any current
        // device and not tested — if a future spec needs it, add a
        // codegen test before relying on it.
        if reg.endian.is_some() && !matches!(reg.access, Access::Ro | Access::Rw | Access::Wo) {
            panic!(
                "{}: register `{}` has endian set but access {:?} is not a plain ro/rw/wo — \
                 BE+trigger / BE+w1c semantics aren't defined yet",
                spec.device.name, reg.name, reg.access
            );
        }
        // BE + typed (flags/fields) compound is non-trivial: the typed
        // accessor's `modify` would need a swap-on-read AND a swap-on-
        // write inside one closure. No current device combines them
        // (fwcfg's BE registers are plain scalars; SDHCI/cprman/virtio
        // are all LE). Defer until a real consumer surfaces — the
        // alternative is shipping codegen that's never been exercised.
        if reg.endian.is_some() && (!reg.flags.is_empty() || !reg.fields.is_empty()) {
            panic!(
                "{}: register `{}` combines `endian` with `flags`/`fields`; emitter doesn't \
                 support BE-typed snapshots yet (add when a device needs it)",
                spec.device.name, reg.name
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
    emit_verify_coverage_comment(&mut out, spec);
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
    emit_u64_pairs(&mut out, spec);
    emit_windowed(&mut out, spec);
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
    // Phase 5 design decision: NO named `insert`/`remove`/`union`
    // methods. The bit-operator family (`|`, `&`, `!`) above is enough
    // and reads as the operation it actually performs. `insert` read
    // as a BTreeSet-style mutation when it actually returned a new
    // value — wrong mental model, would have propagated as more
    // device families used it. Drivers compose with `a | b` and clear
    // bits with `a & !b`. If a future use case wants a method-style
    // surface, add it deliberately at that point — not speculatively.
    writeln!(out).unwrap();
}

// ---------------------------------------------------------------------------
// Phase 4A.2 — paired 32-bit registers exposed as synthesized u64.
// ---------------------------------------------------------------------------

fn emit_u64_pairs(out: &mut String, spec: &Spec) {
    if spec.u64_pairs.is_empty() {
        return;
    }
    let dev = &spec.device.name;
    writeln!(out, "// ---------- {} u64-pair accessors ----------", dev).unwrap();
    writeln!(out).unwrap();
    writeln!(out, "impl {dev} {{").unwrap();
    for pair in &spec.u64_pairs {
        let low_reg = spec.registers.iter().find(|r| r.name == pair.low).expect("validated");
        // Both halves share access by validation.
        match low_reg.access {
            Access::Ro | Access::Rw => {
                writeln!(out, "    /// Composed read of `{}` (low={}, high={}, le).",
                    pair.name, pair.low, pair.high).unwrap();
                writeln!(out, "    #[inline(always)]").unwrap();
                writeln!(out, "    pub fn read_{}(&self) -> u64 {{", pair.name).unwrap();
                writeln!(out, "        self.read_{}() as u64 | ((self.read_{}() as u64) << 32)",
                    pair.low, pair.high).unwrap();
                writeln!(out, "    }}").unwrap();
            }
            _ => {}
        }
        match low_reg.access {
            Access::Wo | Access::Rw => {
                writeln!(out, "    /// Composed write of `{}` (writes low first, then high; le).",
                    pair.name).unwrap();
                writeln!(out, "    #[inline(always)]").unwrap();
                writeln!(out, "    pub fn write_{}(&self, v: u64) {{", pair.name).unwrap();
                writeln!(out, "        self.write_{}(v as u32);", pair.low).unwrap();
                writeln!(out, "        self.write_{}((v >> 32) as u32);", pair.high).unwrap();
                writeln!(out, "    }}").unwrap();
            }
            _ => {}
        }
    }
    writeln!(out, "}}").unwrap();
    writeln!(out).unwrap();
}

// ---------------------------------------------------------------------------
// Phase 4A.3 — windowed selector+value access synthesized as one helper.
// ---------------------------------------------------------------------------

fn emit_windowed(out: &mut String, spec: &Spec) {
    if spec.windowed.is_empty() {
        return;
    }
    let dev = &spec.device.name;
    writeln!(out, "// ---------- {} windowed accessors ----------", dev).unwrap();
    writeln!(out).unwrap();
    let needs_barrier = spec.windowed.iter().any(|w| w.requires_barrier);
    if needs_barrier {
        writeln!(out, "use lockjaw_mmio::barrier::dmb_ish;").unwrap();
        writeln!(out).unwrap();
    }
    writeln!(out, "impl {dev} {{").unwrap();
    for win in &spec.windowed {
        let bits = win.chunk_width as u32 * win.chunk_count as u32;
        match win.direction {
            WindowedDirection::Read => {
                writeln!(out, "    /// Walk `{}` (selector={}, value={}) across {} chunks of {} bits;",
                    win.name, win.selector, win.value, win.chunk_count, win.chunk_width).unwrap();
                writeln!(out, "    /// compose into a u{} (chunk 0 supplies the least-significant bits).",
                    bits.max(64)).unwrap();
                writeln!(out, "    #[inline(always)]").unwrap();
                writeln!(out, "    pub fn read_{}_{}(&self) -> u{} {{", win.name, bits, bits.max(64)).unwrap();
                writeln!(out, "        let mut acc: u{} = 0;", bits.max(64)).unwrap();
                writeln!(out, "        for i in 0..{} {{", win.chunk_count).unwrap();
                writeln!(out, "            self.write_{}(i as u32);", win.selector).unwrap();
                if win.requires_barrier {
                    writeln!(out, "            dmb_ish();").unwrap();
                }
                writeln!(out, "            let chunk = self.read_{}() as u{};", win.value, bits.max(64)).unwrap();
                writeln!(out, "            acc |= chunk << (i * {});", win.chunk_width).unwrap();
                writeln!(out, "        }}").unwrap();
                writeln!(out, "        acc").unwrap();
                writeln!(out, "    }}").unwrap();
            }
            WindowedDirection::Write => {
                writeln!(out, "    /// Walk `{}` writing {} chunks of {} bits;",
                    win.name, win.chunk_count, win.chunk_width).unwrap();
                writeln!(out, "    /// chunk 0 carries the least-significant bits of `v`.").unwrap();
                writeln!(out, "    #[inline(always)]").unwrap();
                writeln!(out, "    pub fn write_{}_{}(&self, v: u{}) {{", win.name, bits, bits.max(64)).unwrap();
                writeln!(out, "        for i in 0..{} {{", win.chunk_count).unwrap();
                writeln!(out, "            self.write_{}(i as u32);", win.selector).unwrap();
                if win.requires_barrier {
                    writeln!(out, "            dmb_ish();").unwrap();
                }
                writeln!(out, "            let chunk = (v >> (i * {})) as u{};", win.chunk_width, win.chunk_width).unwrap();
                writeln!(out, "            self.write_{}(chunk);", win.value).unwrap();
                writeln!(out, "        }}").unwrap();
                writeln!(out, "    }}").unwrap();
            }
        }
    }
    writeln!(out, "}}").unwrap();
    writeln!(out).unwrap();
}

// ---------------------------------------------------------------------------
// Phase 4A.4 — verify_against coverage comment header.
// ---------------------------------------------------------------------------

fn emit_verify_coverage_comment(out: &mut String, spec: &Spec) {
    let Some(against) = &spec.device.verify_against else {
        return;
    };
    let total = spec.registers.len();
    let bound: std::collections::BTreeSet<&str> =
        spec.verify_offsets.iter().map(|v| v.reg.as_str()).collect();
    let matched = bound.len();
    let unmatched: Vec<&str> = spec.registers.iter()
        .map(|r| r.name.as_str())
        .filter(|n| !bound.contains(n))
        .collect();
    writeln!(out, "//").unwrap();
    writeln!(out, "// verify_against: {}", against).unwrap();
    writeln!(out, "// Coverage: {}/{} registers cross-checked against constants.",
        matched, total).unwrap();
    if !unmatched.is_empty() {
        writeln!(out, "// Unmatched (no constant binding in [[verify_offsets]]):").unwrap();
        for name in &unmatched {
            writeln!(out, "//   - {}", name).unwrap();
        }
    }
    writeln!(out, "//").unwrap();
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

        // RO fields get the reader + MASK/SHIFT constants but NO
        // `with_*` setter — hardware ignores writes to status bits,
        // and exposing a constructor that the device silently drops
        // is the discipline-based pattern the typed codegen exists
        // to retire. The construction-safety win is structural:
        // driver code that tries `cm_emmc2_ctl().with_busy(true)`
        // fails to compile rather than producing a no-op write.
        let emit_setter = !matches!(f.access, FieldAccess::Ro);
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
            if emit_setter {
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
            }
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
            if emit_setter {
                writeln!(out, "    /// Return a new value with `{}` set to `v`.", f.name).unwrap();
                writeln!(out, "    pub const fn with_{}(self, v: {enum_ty}) -> Self {{", f.name).unwrap();
                writeln!(
                    out,
                    "        Self((self.0 & !Self::{mask_const}) | ((v.into_bits() as {wty}) << Self::{shift_const}))"
                ).unwrap();
                writeln!(out, "    }}").unwrap();
            }
        } else {
            // Multi-bit scalar.
            writeln!(out, "    /// Read the `{}` field as a scalar.", f.name).unwrap();
            writeln!(
                out,
                "    pub const fn {}(self) -> {wty} {{ (self.0 & Self::{mask_const}) >> Self::{shift_const} }}",
                f.name
            ).unwrap();
            if emit_setter {
                writeln!(out, "    /// Return a new value with `{}` set to `v` (truncated to field width).", f.name).unwrap();
                writeln!(out, "    pub const fn with_{}(self, v: {wty}) -> Self {{", f.name).unwrap();
                writeln!(
                    out,
                    "        Self((self.0 & !Self::{mask_const}) | ((v << Self::{shift_const}) & Self::{mask_const}))"
                ).unwrap();
                writeln!(out, "    }}").unwrap();
            }
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
        // Phase 6 BE handling: BE registers wrap reads in `uN::from_be`
        // and writes in `v.to_be()`. LE is the default (no wrap; matches
        // the AArch64 host byte order Lockjaw runs on). The byte-swap
        // call site is uniform whether the wrapped expression is on a
        // raw cell or a typed snapshot — the typed-snapshot accessors
        // call read_X()/set_X() which apply the conversion exactly once.
        let is_be = matches!(reg.endian, Some(Endian::Big));
        let is_passwd = matches!(reg.kind, Some(RegisterKind::PasswdProtected));
        let read_expr = if is_be {
            format!("{wty}::from_be(self.{}.read())", reg.name)
        } else {
            format!("self.{}.read()", reg.name)
        };
        // Passwd-protected write transform: clear bits[31:24] of the
        // caller-supplied value (which may carry undef/junk from a
        // prior read) and OR in (CM_PASSWORD << 24). The hardware
        // silently drops writes without PASSWD, so doing this in
        // codegen means drivers can never forget. The transform
        // applies to both raw writes and typed `set_*` / `modify_*`
        // paths — every byte that reaches the cell goes through it.
        // BE + passwd is rejected at validation time; only one of
        // the two transforms fires.
        let wrap_write = |val: &str| -> String {
            if is_be {
                // Method-call syntax: `v.to_be()` parses cleanly without
                // parens, and so does `f(v).to_be()`. Keep the emitted
                // shape stable with what pre-Phase-8 codegen produced
                // so the existing fwcfg/pl011/virtio files don't churn.
                format!("{val}.to_be()")
            } else if is_passwd {
                // 0x00FF_FFFFu32 = (1u32 << 24) - 1; 0x5Au32 = CM_PASSWORD
                // for BCM2711 CPRMAN. Currently the only passwd_protected
                // consumer; if another device with a different password
                // emerges, lift the constants into the spec.
                format!("(({val}) & 0x00FF_FFFFu32) | (0x5Au32 << 24)")
            } else {
                val.to_string()
            }
        };
        let write_expr = wrap_write("v");
        let endian_note = if is_be {
            " (big-endian on the wire)"
        } else if is_passwd {
            " (PASSWD prefix injected automatically)"
        } else {
            ""
        };
        let raw_read = |out: &mut String| {
            writeln!(out, "    /// Volatile read of `{}` as `{wty}`{}.", reg.name, endian_note).unwrap();
            writeln!(out, "    #[inline(always)]").unwrap();
            writeln!(out, "    pub fn read_{}(&self) -> {wty} {{ {read_expr} }}", reg.name).unwrap();
        };
        let raw_write = |out: &mut String| {
            writeln!(out, "    /// Volatile write of `{}`{}.", reg.name, endian_note).unwrap();
            writeln!(out, "    #[inline(always)]").unwrap();
            writeln!(out, "    pub fn write_{}(&self, v: {wty}) {{ self.{}.write({write_expr}); }}", reg.name, reg.name).unwrap();
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
                    writeln!(out, "    /// Write the value back to `{}`{}.", reg.name, endian_note).unwrap();
                    writeln!(out, "    #[inline(always)]").unwrap();
                    let set_val = wrap_write("v.0");
                    writeln!(out, "    pub fn set_{}(&self, v: {ty}) {{ self.{}.write({set_val}); }}", reg.name, reg.name).unwrap();
                    writeln!(out, "    /// Read-modify-write `{}` via a typed closure{}.", reg.name, endian_note).unwrap();
                    writeln!(out, "    #[inline(always)]").unwrap();
                    writeln!(out, "    pub fn modify_{}<F: FnOnce({ty}) -> {ty}>(&self, f: F) {{", reg.name).unwrap();
                    // No-transform path stays direct (`f(...).0`) so the
                    // generated file doesn't churn for non-passwd / non-BE
                    // registers. The wrap_write-applied path needs an
                    // explicit closure to host the transform expression.
                    if is_passwd || is_be {
                        let inner = wrap_write("f({ty}(v)).0").replace("{ty}", &ty);
                        writeln!(out, "        self.{}.modify(|v| {inner});", reg.name).unwrap();
                    } else {
                        writeln!(out, "        self.{}.modify(|v| f({ty}(v)).0);", reg.name).unwrap();
                    }
                    writeln!(out, "    }}").unwrap();
                } else if matches!(reg.kind, Some(RegisterKind::Stream)) {
                    // Stream port: read and write touch independent state.
                    // Emit read_/write_ but NOT modify_ — RMW is incoherent.
                    raw_read(out);
                    raw_write(out);
                } else {
                    raw_read(out);
                    raw_write(out);
                    writeln!(out, "    /// Read-modify-write `{}`{}.", reg.name, endian_note).unwrap();
                    writeln!(out, "    #[inline(always)]").unwrap();
                    writeln!(out, "    pub fn modify_{}<F: FnOnce({wty}) -> {wty}>(&self, f: F) {{", reg.name).unwrap();
                    if is_passwd || is_be {
                        let inner = wrap_write("f(v)");
                        writeln!(out, "        self.{}.modify(|v| {inner});", reg.name).unwrap();
                    } else {
                        writeln!(out, "        self.{}.modify(f);", reg.name).unwrap();
                    }
                    writeln!(out, "    }}").unwrap();
                }
            }
            Access::Wo | Access::Trigger => {
                if has_typed {
                    writeln!(out, "    /// Write a typed value to `{}`{}.", reg.name, endian_note).unwrap();
                    writeln!(out, "    #[inline(always)]").unwrap();
                    let set_val = wrap_write("v.0");
                    writeln!(out, "    pub fn set_{}(&self, v: {ty}) {{ self.{}.write({set_val}); }}", reg.name, reg.name).unwrap();
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
    let has_be = spec.registers.iter().any(|r| matches!(r.endian, Some(Endian::Big)));
    let has_passwd = spec.registers.iter().any(|r| matches!(r.kind, Some(RegisterKind::PasswdProtected)));
    let needs_mock =
        !spec.u64_pairs.is_empty() || !spec.windowed.is_empty() || has_be || has_passwd;
    writeln!(out, "#[cfg(test)]").unwrap();
    writeln!(out, "mod tests {{").unwrap();
    writeln!(out, "    use super::*;").unwrap();
    writeln!(out, "    use core::mem::offset_of;").unwrap();
    if needs_mock {
        writeln!(out, "    use lockjaw_mmio::mock::MockMmioRegion;").unwrap();
    }
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

        // Per-field roundtrip. RO fields have no `with_*` setter, so
        // a `with_*().field()` roundtrip is meaningless for them —
        // they get a dedicated reader test below instead.
        writeln!(out, "    #[test]").unwrap();
        writeln!(out, "    fn {}_field_roundtrip() {{", reg.name).unwrap();
        for f in &reg.fields {
            if matches!(f.access, FieldAccess::Ro) {
                writeln!(out, "        // RO field `{}` — see {}_ro_field_read", f.name, reg.name).unwrap();
                continue;
            }
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
        // verify bits outside the field mask are unchanged. RO fields
        // have no `with_*` to call so they're skipped — their bits
        // never move via the typed surface, so reserved-bit preservation
        // is trivially preserved for them.
        writeln!(out, "    #[test]").unwrap();
        writeln!(out, "    fn {}_preserves_reserved_bits() {{", reg.name).unwrap();
        writeln!(out, "        let all_ones: {wty} = !0;").unwrap();
        for f in &reg.fields {
            if matches!(f.access, FieldAccess::Ro) { continue; }
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

        // Per-RO-field read tests. Construct the typed wrapper from
        // a raw bit pattern that places the field's bits at known
        // values; verify the accessor decodes to the expected result.
        // Replaces what the `with_*().field()` roundtrip would cover
        // for RW fields.
        let has_ro = reg.fields.iter().any(|f| matches!(f.access, FieldAccess::Ro));
        if has_ro {
            writeln!(out, "    #[test]").unwrap();
            writeln!(out, "    fn {}_ro_field_read() {{", reg.name).unwrap();
            for f in &reg.fields {
                if !matches!(f.access, FieldAccess::Ro) { continue; }
                let (hi, lo) = parse_bits(&f.bits).expect("validated");
                let width = (hi - lo + 1) as u32;
                let mask = if width >= reg.width as u32 {
                    mask_for_width(reg.width)
                } else {
                    (((1u64 << width) - 1) << lo) as u64
                };
                if width == 1 && f.enum_values.is_empty() {
                    writeln!(out, "        // RO bool `{}` — bit {} reads as the field state", f.name, lo).unwrap();
                    writeln!(out, "        assert!({ty}(0x{mask:x}{wty}).{}());", f.name).unwrap();
                    writeln!(out, "        assert!(!{ty}(!0x{mask:x}{wty}).{}());", f.name).unwrap();
                } else if !f.enum_values.is_empty() {
                    let enum_ty = format!("{}{}", ty, to_pascal(&f.name));
                    let first = &f.enum_values[0];
                    let placed: u64 = (first.value as u64) << lo;
                    writeln!(out, "        // RO enum `{}` — decode a known variant placed at bits {}:{}", f.name, hi, lo).unwrap();
                    writeln!(out, "        assert_eq!({ty}(0x{placed:x}{wty}).{}().unwrap(), {enum_ty}::{});", f.name, first.name).unwrap();
                } else {
                    let value: u64 = mask;
                    let extracted: u64 = (mask >> lo) & (((1u64 << width) - 1) as u64);
                    writeln!(out, "        // RO scalar `{}` — read extracts bits {}:{}", f.name, hi, lo).unwrap();
                    writeln!(out, "        assert_eq!({ty}(0x{value:x}{wty}).{}(), 0x{extracted:x}{wty});", f.name).unwrap();
                }
            }
            writeln!(out, "    }}").unwrap();
            writeln!(out).unwrap();
        }

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

    // Phase 4A.2 — u64_pair cross-validation: synthesized read_X()
    // must equal manual composition of the two halves; synthesized
    // write_X(v) must land low at the low register and high at the
    // high register.
    for pair in &spec.u64_pairs {
        let low_reg = spec.registers.iter().find(|r| r.name == pair.low).expect("validated");
        let low_off = low_reg.offset;
        let high_off = spec.registers.iter().find(|r| r.name == pair.high).expect("validated").offset;

        writeln!(out, "    #[test]").unwrap();
        writeln!(out, "    fn {}_pair_roundtrip() {{", pair.name).unwrap();
        writeln!(out, "        let region = MockMmioRegion::for_layout::<{dev}>();").unwrap();
        writeln!(out, "        let regs = region.as_mapped_regs::<{dev}>();").unwrap();
        writeln!(out, "        let dev_ref = regs.regs();").unwrap();
        match low_reg.access {
            Access::Ro | Access::Rw => {
                // Seed both halves at the device side and verify the
                // composed read matches manual composition.
                writeln!(out, "        region.poke_u32(0x{:x}, 0x1122_3344);", low_off).unwrap();
                writeln!(out, "        region.poke_u32(0x{:x}, 0xAABB_CCDD);", high_off).unwrap();
                writeln!(out, "        let composed = dev_ref.read_{}();", pair.name).unwrap();
                writeln!(out, "        let manual = dev_ref.read_{}() as u64 | ((dev_ref.read_{}() as u64) << 32);", pair.low, pair.high).unwrap();
                writeln!(out, "        assert_eq!(composed, manual);").unwrap();
                writeln!(out, "        assert_eq!(composed, 0xAABB_CCDD_1122_3344);").unwrap();
            }
            _ => {}
        }
        match low_reg.access {
            Access::Wo | Access::Rw => {
                writeln!(out, "        dev_ref.write_{}(0xDEAD_BEEF_CAFE_BABE);", pair.name).unwrap();
                writeln!(out, "        assert_eq!(region.peek_u32(0x{:x}), 0xCAFE_BABE);", low_off).unwrap();
                writeln!(out, "        assert_eq!(region.peek_u32(0x{:x}), 0xDEAD_BEEF);", high_off).unwrap();
            }
            _ => {}
        }
        writeln!(out, "    }}").unwrap();
        writeln!(out).unwrap();
    }

    // Phase 4A.3 — windowed accessor cross-validation. End-state
    // assertions only (no per-call sequence log; see mock.rs).
    for win in &spec.windowed {
        let sel_off = spec.registers.iter().find(|r| r.name == win.selector).expect("validated").offset;
        let val_off = spec.registers.iter().find(|r| r.name == win.value).expect("validated").offset;
        let bits = win.chunk_width as u32 * win.chunk_count as u32;
        match win.direction {
            WindowedDirection::Read => {
                // Trick: the value register is RO from the driver side
                // but writable from device side via poke. The test
                // changes the poked value between window indices to
                // simulate the device's response, asserting that the
                // synthesized helper visits every index in order.
                //
                // The mock is non-reactive (writing the selector does
                // not auto-swap the value), so we cannot script a
                // multi-step sequence in one call. Instead test the
                // single-chunk case: when both chunks return the same
                // value, the composed value equals that value
                // replicated in both halves. This validates the helper
                // reads chunks 0 AND 1 (not just chunk 0).
                writeln!(out, "    #[test]").unwrap();
                writeln!(out, "    fn {}_windowed_read_visits_all_chunks() {{", win.name).unwrap();
                writeln!(out, "        let region = MockMmioRegion::for_layout::<{dev}>();").unwrap();
                writeln!(out, "        let regs = region.as_mapped_regs::<{dev}>();").unwrap();
                writeln!(out, "        region.poke_u32(0x{:x}, 0xDEAD_BEEF);", val_off).unwrap();
                writeln!(out, "        let composed = regs.regs().read_{}_{}();", win.name, bits).unwrap();
                writeln!(out, "        // Both chunks read the same value -> composed value has it in every position.").unwrap();
                writeln!(out, "        let expected = (0xDEAD_BEEFu64) | (0xDEAD_BEEFu64 << 32);").unwrap();
                writeln!(out, "        assert_eq!(composed, expected);").unwrap();
                writeln!(out, "        // Selector ended at chunk_count - 1 (proves helper walked all chunks).").unwrap();
                writeln!(out, "        assert_eq!(region.peek_u32(0x{:x}), {});", sel_off, win.chunk_count as u32 - 1).unwrap();
                writeln!(out, "    }}").unwrap();
                writeln!(out).unwrap();
            }
            WindowedDirection::Write => {
                // Write helper: write a 64-bit value; verify selector
                // ended at the last chunk index AND the value cell
                // holds the most-significant chunk (last write wins).
                writeln!(out, "    #[test]").unwrap();
                writeln!(out, "    fn {}_windowed_write_visits_all_chunks() {{", win.name).unwrap();
                writeln!(out, "        let region = MockMmioRegion::for_layout::<{dev}>();").unwrap();
                writeln!(out, "        let regs = region.as_mapped_regs::<{dev}>();").unwrap();
                writeln!(out, "        regs.regs().write_{}_{}(0xDEAD_BEEF_CAFE_BABE);", win.name, bits).unwrap();
                writeln!(out, "        // Selector ended at chunk_count - 1 (proves helper walked all chunks).").unwrap();
                writeln!(out, "        assert_eq!(region.peek_u32(0x{:x}), {});", sel_off, win.chunk_count as u32 - 1).unwrap();
                writeln!(out, "        // Value register holds the most-significant chunk (last write wins).").unwrap();
                writeln!(out, "        assert_eq!(region.peek_u32(0x{:x}), 0xDEAD_BEEF);", val_off).unwrap();
                writeln!(out, "    }}").unwrap();
                writeln!(out).unwrap();
            }
        }
    }

    // Phase 6 — BE byte-swap roundtrip for big-endian registers.
    // Writes a host-order value through the typed accessor; peeks
    // the underlying memory and asserts the bytes are the BE
    // representation (proves to_be() is applied). Then reads back
    // through the typed accessor and asserts the host value
    // matches (proves from_be() is applied on the read side too).
    // Tests only emit for writable BE registers since the read-back
    // check requires writing first.
    for reg in &spec.registers {
        if !matches!(reg.endian, Some(Endian::Big)) { continue; }
        if !matches!(reg.access, Access::Rw | Access::Wo) { continue; }
        let off = reg.offset;
        // Choose a test value whose BE byte pattern differs visibly
        // from its LE pattern, so a missing swap shows up clearly.
        let (test_val, peek_method, expected_raw) = match reg.width {
            8  => ("0x12u8",                "peek_u8",  "0x12u8".to_string()),
            16 => ("0x1234u16",             "peek_u16", "0x3412u16".to_string()),
            32 => ("0x1234_5678u32",        "peek_u32", "0x7856_3412u32".to_string()),
            64 => ("0x1122_3344_5566_7788u64", "peek_u64", "0x8877_6655_4433_2211u64".to_string()),
            _ => unreachable!(),
        };
        writeln!(out, "    #[test]").unwrap();
        writeln!(out, "    fn {}_be_roundtrip() {{", reg.name).unwrap();
        writeln!(out, "        let region = MockMmioRegion::for_layout::<{dev}>();").unwrap();
        writeln!(out, "        let regs = region.as_mapped_regs::<{dev}>();").unwrap();
        writeln!(out, "        regs.regs().write_{}({test_val});", reg.name).unwrap();
        writeln!(out, "        // Underlying memory holds the BE byte pattern (write applied to_be()).").unwrap();
        writeln!(out, "        assert_eq!(region.{peek_method}(0x{off:x}), {expected_raw});").unwrap();
        if matches!(reg.access, Access::Rw) {
            writeln!(out, "        // Typed read recovers the host value (read applied from_be()).").unwrap();
            writeln!(out, "        assert_eq!(regs.regs().read_{}(), {test_val});", reg.name).unwrap();
        }
        writeln!(out, "    }}").unwrap();
        writeln!(out).unwrap();
    }

    // Phase 8 — passwd_protected: every write must carry the PASSWD
    // byte in bits[31:24] or the BCM2711 CPRMAN silently drops it. The
    // test writes 0 through the typed/raw setter and asserts the
    // underlying memory has 0x5A in the top byte; then writes a
    // 24-bit payload and asserts both the PASSWD prefix AND the
    // payload survived. The wrap is mechanical in codegen — once
    // these tests pass, every register marked passwd_protected
    // inherits the same guarantee for free.
    for reg in &spec.registers {
        if !matches!(reg.kind, Some(RegisterKind::PasswdProtected)) { continue; }
        let off = reg.offset;
        let has_typed_w = !reg.flags.is_empty() || !reg.fields.is_empty();
        let ty = to_pascal(&reg.name);
        // Two payloads exercise both "preserve zero" and "preserve
        // a non-trivial 24-bit value"; both expect PASSWD in the top
        // byte.
        let write_call_zero = if has_typed_w {
            format!("regs.regs().set_{}({ty}(0u32))", reg.name)
        } else {
            format!("regs.regs().write_{}(0u32)", reg.name)
        };
        let write_call_payload = if has_typed_w {
            format!("regs.regs().set_{}({ty}(0x00AB_CDEFu32))", reg.name)
        } else {
            format!("regs.regs().write_{}(0x00AB_CDEFu32)", reg.name)
        };
        writeln!(out, "    #[test]").unwrap();
        writeln!(out, "    fn {}_passwd_wrap_zero_payload() {{", reg.name).unwrap();
        writeln!(out, "        let region = MockMmioRegion::for_layout::<{dev}>();").unwrap();
        writeln!(out, "        let regs = region.as_mapped_regs::<{dev}>();").unwrap();
        writeln!(out, "        {write_call_zero};").unwrap();
        writeln!(out, "        // PASSWD (0x5A) must occupy bits[31:24]; low 24 bits stay 0.").unwrap();
        writeln!(out, "        assert_eq!(region.peek_u32(0x{off:x}), 0x5A00_0000u32);").unwrap();
        writeln!(out, "    }}").unwrap();
        writeln!(out).unwrap();
        writeln!(out, "    #[test]").unwrap();
        writeln!(out, "    fn {}_passwd_wrap_preserves_payload() {{", reg.name).unwrap();
        writeln!(out, "        let region = MockMmioRegion::for_layout::<{dev}>();").unwrap();
        writeln!(out, "        let regs = region.as_mapped_regs::<{dev}>();").unwrap();
        writeln!(out, "        {write_call_payload};").unwrap();
        writeln!(out, "        // PASSWD prefix AND the 24-bit payload survive intact.").unwrap();
        writeln!(out, "        assert_eq!(region.peek_u32(0x{off:x}), 0x5AAB_CDEFu32);").unwrap();
        writeln!(out, "    }}").unwrap();
        writeln!(out).unwrap();
        // Modify (RW only): seed PASSWD-junk-as-if-from-hardware via
        // poke, run a modify that clears bits[31:24] in the closure,
        // and assert the resulting memory still has PASSWD intact.
        // This proves the wrap_write fires INSIDE modify_, not just
        // raw write_/set_.
        if matches!(reg.access, Access::Rw) {
            writeln!(out, "    #[test]").unwrap();
            writeln!(out, "    fn {}_passwd_wrap_in_modify() {{", reg.name).unwrap();
            writeln!(out, "        let region = MockMmioRegion::for_layout::<{dev}>();").unwrap();
            writeln!(out, "        let regs = region.as_mapped_regs::<{dev}>();").unwrap();
            writeln!(out, "        // Seed memory with a payload — modify_ must keep low bits, replace top.").unwrap();
            writeln!(out, "        region.poke_u32(0x{off:x}, 0x0000_00FFu32);").unwrap();
            if has_typed_w {
                writeln!(out, "        regs.regs().modify_{}(|v| {ty}(v.0 | 0xFF00u32));", reg.name).unwrap();
            } else {
                writeln!(out, "        regs.regs().modify_{}(|v| v | 0xFF00u32);", reg.name).unwrap();
            }
            writeln!(out, "        // Closure produced 0x0000_FFFFu32; codegen OR-ed PASSWD on top.").unwrap();
            writeln!(out, "        assert_eq!(region.peek_u32(0x{off:x}), 0x5A00_FFFFu32);").unwrap();
            writeln!(out, "    }}").unwrap();
            writeln!(out).unwrap();
        }
    }

    // Phase 4A.4 — verify_against const_assert_eq! per binding. Lives
    // in a nested #[cfg(test)] module so the import path resolves at
    // test compile time.
    if let Some(against) = &spec.device.verify_against {
        if !spec.verify_offsets.is_empty() {
            writeln!(out, "    mod _verify {{").unwrap();
            writeln!(out, "        use super::*;").unwrap();
            writeln!(out, "        use static_assertions::const_assert_eq;").unwrap();
            for vo in &spec.verify_offsets {
                writeln!(out, "        const_assert_eq!(").unwrap();
                writeln!(out, "            offset_of!({dev}, {}) as u64,", vo.reg).unwrap();
                writeln!(out, "            {}::{}", against, vo.const_name).unwrap();
                writeln!(out, "        );").unwrap();
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
