# Regspec TOML Format

Reference for the TOML schema consumed by `cargo xtask gen-regs`.
Inputs live under `user/regspecs/*.toml`; outputs are committed to
`user/lockjaw-regs/src/<device>.rs`. `--check` regenerates in memory
and diffs against the committed file; non-zero exit on drift.

Every spec is parsed and validated even when `[device].emit = false`,
so latent schema bugs surface immediately rather than at the moment
a future commit flips emit on.

## Top-level keys

| Key                  | Required | Type                  | Meaning                                                      |
|----------------------|----------|-----------------------|--------------------------------------------------------------|
| `device`             | yes      | table                 | Device metadata (see below).                                 |
| `registers`          | no       | array of tables       | Register declarations.                                       |
| `u64_pairs`          | no       | array of tables       | 32-bit low/high registers synthesized as a u64 accessor.     |
| `windowed`           | no       | array of tables       | Selector + value pairs synthesized as a single helper.       |
| `verify_offsets`     | no       | array of tables       | Cross-checks for register offsets against external consts.   |

## `[device]` table

| Key              | Required | Type   | Default | Meaning                                                                                  |
|------------------|----------|--------|---------|------------------------------------------------------------------------------------------|
| `name`           | yes      | string | —       | Pascal-case device type name (becomes the `pub struct <name>` in the emitted module).    |
| `description`    | no       | string | —       | Free-form text rendered into the generated module's header doc comment.                  |
| `emit`           | no       | bool   | `true`  | When `false`, the spec is parsed and validated but no `.rs` file is written or diffed.   |
| `verify_against` | no       | string | —       | Path to a module whose constants are cross-checked via `[[verify_offsets]]` bindings.    |

Example:

```toml
[device]
name = "Pl011"
description = "PrimeCell PL011 UART (ARM)"
emit = true
```

## `[[registers]]` entries

| Key           | Required | Type            | Default | Meaning                                                                                  |
|---------------|----------|-----------------|---------|------------------------------------------------------------------------------------------|
| `name`        | yes      | string          | —       | snake_case register name. Becomes the `#[repr(C)]` struct field and the accessor stem.   |
| `offset`      | yes      | integer         | —       | Byte offset of the register within the device's MMIO region.                             |
| `width`       | yes      | integer         | —       | Cell width in bits. Must be one of `8`, `16`, `32`, `64`.                                |
| `access`      | yes      | enum            | —       | One of `ro`, `rw`, `wo`, `w1c`, `rw1c`, `trigger` (see below).                           |
| `description` | no       | string          | —       | Rendered into the generated struct field's doc comment.                                  |
| `kind`        | no       | enum            | —       | One of `combined_trigger`, `aliased`, `stream`, `passwd_protected` (see below).          |
| `flags`       | no       | array of tables | `[]`    | Single-bit flag declarations (see `[[flags]]`). Mutually exclusive with `fields`.        |
| `fields`      | no       | array of tables | `[]`    | Multi-bit field declarations (see `[[fields]]`). Mutually exclusive with `flags`.        |
| `parts`       | no       | array of tables | `[]`    | Named bit-range parts for `combined_trigger` setters (see `[[parts]]`).                  |
| `aliases`     | no       | array of string | `[]`    | Alternate accessor names over the same cell. Requires `kind = "aliased"`.                |
| `endian`      | no       | enum            | `little`| One of `little`, `big`. BE wraps reads with `from_be` and writes with `to_be`.           |

### `access` values

- `ro` — emit `read_<reg>()` / `<reg>()` only.
- `rw` — emit reader + `write_<reg>(v)` + `modify_<reg>(f)`.
- `wo` — emit writer only.
- `w1c` — write-1-to-clear with no readable counterpart. Emits only
  `clear_<reg>(mask)`. The canonical case is PL011 ICR; the readable
  RIS/MIS lives at a different offset as a separate `ro` register.
- `rw1c` — read + write-1-to-clear in the same cell (SDHCI's
  `NORMAL_INT_STATUS` / `ERROR_INT_STATUS`). Emits a typed snapshot
  reader and a `clear_<reg>(mask)` writer. No `set_*` or `modify_*` —
  a value-store has no defined meaning where writes ack bits.
- `trigger` — write-only register where the write itself kicks an
  action. Emitter treats it identically to `wo` (`Wo<T>` cell type,
  same accessor shape); the distinction is documentary at the
  hardware-spec layer.

### `kind` values

- `stream` — read and write touch independent state (FIFOs). Forbids
  `flags`/`fields` and suppresses `modify_*` (RMW on a stream is
  incoherent).
- `passwd_protected` — every write OR-s the spec-mandated PASSWD byte
  `0x5A` into bits[31:24]. BCM2711 CPRMAN silently drops writes that
  omit it. Only `width = 32` and `access` in `{rw, wo}` are supported;
  combining with `endian` is rejected.
- `aliased` — pairs with a non-empty `aliases` list to emit
  alternate-name accessors over the same cell. The cell remains a
  single `#[repr(C)]` field at the primary name; aliases delegate.
- `combined_trigger` — `parts` are composed into a single u32 store
  via the synthesized `set_<reg>(part_a, part_b, ...)`. Required for
  registers like SDHCI's `TRANSFER_MODE` + `COMMAND` which BCM2711
  silently drops if written as two halves. Requires `width = 32`,
  `access` in `{wo, trigger}`, and at least two parts that tile
  `[0..32)` exactly.

### `[[flags]]` entries

| Key           | Required | Type    | Meaning                                                  |
|---------------|----------|---------|----------------------------------------------------------|
| `name`        | yes      | string  | Constant name (uppercased into `Reg::NAME`).             |
| `bit`         | yes      | integer | Bit position. Must be `< width`.                         |
| `description` | no       | string  | Doc comment on the emitted associated constant.          |

Emits a `#[repr(transparent)] struct <Reg>(pub u<width>)` with
`empty()`, `bits()`, `contains()`, `|`, `&`, `!`, and one associated
constant per flag.

### `[[fields]]` entries

| Key           | Required | Type            | Default | Meaning                                                                |
|---------------|----------|-----------------|---------|------------------------------------------------------------------------|
| `name`        | yes      | string          | —       | snake_case field name.                                                 |
| `bits`        | yes      | string          | —       | Inclusive `"hi:lo"` range within the register.                         |
| `description` | no       | string          | —       | Currently parsed but not rendered (the struct field is `#[allow(dead_code)]` — intentionally kept).         |
| `enum_values` | no       | array of tables | `[]`    | Named bit patterns for the field (see below). Field becomes enum-typed.|
| `access`      | no       | enum            | `rw`    | One of `ro`, `rw`. `ro` suppresses the `with_<field>` setter.          |

Field width 1 with no `enum_values` becomes a bool accessor;
multi-bit without enum values becomes a scalar; any `enum_values` set
becomes a `Result<<Reg><Field>, ReservedBits>` decoder plus enum
setter.

#### `enum_values` entries

| Key     | Required | Type    | Meaning                                            |
|---------|----------|---------|----------------------------------------------------|
| `name`  | yes      | string  | Pascal-case enum variant name.                     |
| `value` | yes      | integer | Bit pattern. Must fit in the field's width.        |

The emitted enum's `#[repr]` is the narrowest of `u8`/`u16`/`u32`/`u64`
that holds every declared value; reserved patterns decode to
`Err(ReservedBits(v))`.

### `[[parts]]` entries (combined_trigger only)

| Key    | Required | Type   | Meaning                                                       |
|--------|----------|--------|---------------------------------------------------------------|
| `name` | yes      | string | snake_case part name. Becomes a setter argument name and a typed `#[repr(transparent)]` newtype. |
| `bits` | yes      | string | Inclusive `"hi:lo"` range. Width must be 8, 16, or 32.        |

Parts must tile `[0..32)` exactly with no gaps and no overlaps. Each
part gets its own newtype so a positional swap of two same-width
parts is a compile error.

## `[[u64_pairs]]` entries

| Key      | Required | Type   | Default    | Meaning                                                        |
|----------|----------|--------|------------|----------------------------------------------------------------|
| `name`   | yes      | string | —          | Logical name for the pair. Synthesized accessor uses this stem (`read_<name>` / `write_<name>`). Must not collide with any register name. |
| `low`    | yes      | string | —          | Existing register supplying the least-significant 32 bits.     |
| `high`   | yes      | string | —          | Existing register supplying the most-significant 32 bits.      |
| `endian` | no       | enum   | `little`   | Currently only `little` is supported (`big` is reserved).      |

Both halves must be `width = 32`, share the same `access`, and that
access must be `ro`, `rw`, or `wo`.

Example:

```toml
[[u64_pairs]]
name = "blk_capacity"
low = "blk_capacity_low"
high = "blk_capacity_high"
```

## `[[windowed]]` entries

| Key                | Required | Type    | Default | Meaning                                                                      |
|--------------------|----------|---------|---------|------------------------------------------------------------------------------|
| `name`             | yes      | string  | —       | Stem for the synthesized accessor (`read_<name>_<bits>` / `write_<name>_<bits>`). |
| `selector`         | yes      | string  | —       | Register that the helper writes before each chunk access. Must be writable.  |
| `value`            | yes      | string  | —       | Register read from (or written to) once the selector is set.                 |
| `chunk_width`      | yes      | integer | —       | Width in bits of each chunk. Must equal the value register's `width`.        |
| `chunk_count`      | yes      | integer | —       | Number of chunks the accessor walks. Must be `> 0`. Today only `2` is exercised. |
| `direction`        | yes      | enum    | —       | `read` or `write`. Constrains the required access mode on `value`.           |
| `requires_barrier` | no       | bool    | `false` | When `true`, insert `dmb_ish()` between selector write and value access.     |

For `direction = "read"`, `value` must be `ro` or `rw`. For
`direction = "write"`, `value` must be `wo` or `rw`. The selector
must always be writable.

## `[[verify_offsets]]` entries

| Key     | Required | Type   | Meaning                                                                      |
|---------|----------|--------|------------------------------------------------------------------------------|
| `reg`   | yes      | string | Name of a register in `[[registers]]`.                                       |
| `const` | yes      | string | Constant name in `[device].verify_against`'s module. Emits a `const_assert_eq!(offset_of!(Dev, reg), <verify_against>::<const>)`. |

Requires `[device].verify_against` to be set. The generated module's
header lists coverage (`N/M registers cross-checked`) and any
registers without a binding.

## Validator messages

Validation runs over every spec — including ones with `emit = false` —
so schema bugs cannot lie latent. All failures `panic!` with the
spec's file path prefixed.

### Name collisions

```
<path>: register name `<name>` declared twice
<path>: alias `<alias>` (on register `<owner>`) collides with primary register name `<alias>`
<path>: primary register name `<name>` collides with an alias already declared on `<prior>`
<path>: alias `<alias>` declared on both `<prior>` and `<owner>`
```

The name registry treats primary names and aliases as one global
namespace.

### Register shape

```
<path>: register `<name>` has unsupported width <w>; must be 8/16/32/64
<path>: register `<name>` offset 0x<o> not aligned to <n> bytes
<path>: register `<name>` offset 0x<o> collides with an earlier register. If both names refer to the same hardware cell, use the primary register's `aliases = ["..."]` list (with `kind = "aliased"`) — two `[[registers]]` entries at the same offset are not supported.
<path>: register `<name>` has both `flags` and `fields` — pick one
<path>: register `<name>` flag `<f>` bit <b> >= width <w>
```

### Field / part bit-range

```
<path>: register `<name>` field `<f>` bad bits "<bits>"
<path>: register `<name>` field `<f>` bits <hi>:<lo> out of range for width <w>
<path>: register `<name>` field `<f>` enum `<ev>` value 0x<v> > max 0x<m>
<path>: register `<name>` part `<p>` bad bits "<bits>"
<path>: register `<name>` part `<p>` bits <hi>:<lo> out of range for width <w>
```

### `[[u64_pairs]]`

```
<path>: u64_pair `<name>` low register `<r>` not found in [[registers]]
<path>: u64_pair `<name>` high register `<r>` not found in [[registers]]
<path>: u64_pair `<name>` requires both halves to be 32 bits (got low=<lw>, high=<hw>)
<path>: u64_pair `<name>` halves disagree on access (<low> vs <high>); a u64 helper must be uniformly readable or uniformly writable
<path>: u64_pair `<name>` access <a> not supported (must be ro/rw/wo)
<path>: u64_pair `<name>` collides with a register of the same name
```

### `[[windowed]]`

```
<path>: windowed `<name>` selector `<r>` not found
<path>: windowed `<name>` value `<r>` not found
<path>: windowed `<name>` selector `<r>` must be writable (got <a>)
<path>: windowed `<name>` chunk_width <cw> doesn't match value register `<r>` width <w>
<path>: windowed `<name>` direction=read but value `<r>` is not readable (<a>)
<path>: windowed `<name>` direction=write but value `<r>` is not writable (<a>)
<path>: windowed `<name>` chunk_count must be > 0
```

### `[[verify_offsets]]`

```
<path>: [[verify_offsets]] requires [device].verify_against to be set
<path>: verify_offset register `<r>` not found
```

### Per-`kind` invariants (raised at emit time, before any output is written)

```
stream register `<name>` in `<dev>` cannot have flags or fields: read and write target different state, so a typed-snapshot value would be meaningless
passwd_protected register `<name>` in `<dev>` has width <w>; only u32 is supported (PASSWD lives in bits[31:24])
passwd_protected register `<name>` in `<dev>` has access <a>; only rw/wo are supported (passwd is a write-time concern)
passwd_protected register `<name>` in `<dev>` combines passwd with `endian`; not supported (no current device needs both)
```

### `endian` constraints

```
<dev>: register `<name>` has endian set but access <a> is not a plain ro/rw/wo — BE+trigger / BE+w1c semantics aren't defined yet
<dev>: register `<name>` combines `endian` with `flags`/`fields`; emitter doesn't support BE-typed snapshots yet (add when a device needs it)
```

BE is supported on plain scalars only. BE + u8 is parsed but has no
test coverage; add one before a spec relies on it.

### `aliases` / `kind = "aliased"`

```
<dev>: register `<name>` declares `aliases` but kind is not `aliased` — set `kind = "aliased"` to enable the alias-accessor emit, or drop the aliases field if they're unintended
<dev>: register `<name>` has `kind = "aliased"` but declares no `aliases` — add at least one alternate name in the `aliases` field, or drop the kind
<dev>: register `<name>` lists itself as an alias
```

### `combined_trigger`

```
register `<name>` in `<dev>` has `parts` but kind is not `combined_trigger` — parts are meaningful only for combined_trigger registers
combined_trigger register `<name>` in `<dev>` has width <w>; only u32 is supported (the M7 ordering fix on BCM2711 specifically requires a single u32 store)
combined_trigger register `<name>` in `<dev>` has access <a>; only wo or trigger are supported (combined_trigger is fire-and-forget; reading is meaningless)
combined_trigger register `<name>` in `<dev>` declares <n> part(s) — needs at least two (a single-part combined_trigger has no value over a plain Wo<u32>)
combined_trigger register `<name>` in `<dev>`: part `<p>` starts at bit <b> but next expected bit is <e> (gaps + overlaps not allowed)
combined_trigger register `<name>` in `<dev>`: parts cover [0..<n>) but register width is <w> (parts must tile the full register)
combined_trigger register `<name>` in `<dev>`: part `<p>` has width <w> bits; only 8/16/32 are supported
```

### Layout overlap (emit time)

```
register `<name>` offset 0x<o> < cursor 0x<c> (overlap)
```

Fires when, after the emitter sorts registers by offset, a register
starts before the previous register's end — e.g. a `width = 64`
register at `0x0` and a `width = 32` register at `0x4` are in
increasing offset order but still trip this because the width-64
cell extends to `0x8`. The spec-level collision check in `validate`
catches exact-same-offset duplicates earlier with a more helpful
message; this emit-time panic catches the width-overlap case.

## Examples

Plain RW with flags (PL011 CR):

```toml
[[registers]]
name = "control"
offset = 0x30
width = 32
access = "rw"
description = "Control register"
flags = [
    { name = "uart_enable", bit = 0 },
    { name = "tx_enable", bit = 8 },
    { name = "rx_enable", bit = 9 },
]
```

Enum-valued field (PL011 LCR_H):

```toml
fields = [
    { name = "parity_select", bits = "2:2", enum_values = [
        { name = "Odd", value = 0 },
        { name = "Even", value = 1 },
    ] },
    { name = "word_length", bits = "6:5", enum_values = [
        { name = "Bits5", value = 0 },
        { name = "Bits6", value = 1 },
        { name = "Bits7", value = 2 },
        { name = "Bits8", value = 3 },
    ] },
]
```

Aliased cell (SDHCI sysaddr / argument2):

```toml
[[registers]]
name = "sysaddr"
offset = 0x000
width = 32
access = "rw"
kind = "aliased"
aliases = ["argument2"]
```

Combined trigger (SDHCI transfer_mode + command):

```toml
[[registers]]
name = "transfer_mode_command"
offset = 0x00c
width = 32
access = "wo"
kind = "combined_trigger"
parts = [
    { name = "transfer_mode", bits = "15:0" },
    { name = "command", bits = "31:16" },
]
```

Per-field RO inside an RW register (SDHCI clock_control):

```toml
fields = [
    { name = "int_clk_en", bits = "0:0" },
    { name = "int_clk_stable", bits = "1:1", access = "ro" },
    { name = "sd_clk_en", bits = "2:2" },
]
```

Big-endian write-only (fwcfg selector):

```toml
[[registers]]
name = "selector"
offset = 0x008
width = 16
access = "wo"
endian = "big"
```

Passwd-protected (CPRMAN cm_emmc2_ctl):

```toml
[[registers]]
name = "cm_emmc2_ctl"
offset = 0x1d0
width = 32
access = "rw"
kind = "passwd_protected"
fields = [
    { name = "src", bits = "3:0", enum_values = [
        { name = "Gnd",         value = 0 },
        { name = "Osc",         value = 1 },
        { name = "PllDPerCore", value = 6 },
    ] },
    { name = "enable", bits = "4:4" },
    { name = "kill",   bits = "5:5" },
    { name = "busy",   bits = "7:7", access = "ro" },
]
```

Windowed read (virtio-mmio device_features):

```toml
[[windowed]]
name = "device_features"
selector = "device_features_sel"
value = "device_features"
chunk_width = 32
chunk_count = 2
direction = "read"
```

u64 pair (virtio-mmio blk_capacity):

```toml
[[u64_pairs]]
name = "blk_capacity"
low = "blk_capacity_low"
high = "blk_capacity_high"
```

Verify offsets bound to external constants:

```toml
[device]
name = "Sdhci"
verify_against = "lockjaw_types::sdhci"

[[verify_offsets]]
reg = "sysaddr"
const = "SDHCI_SYSADDR"
```
