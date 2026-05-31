# Wirespec TOML Format

Reference for the TOML schema consumed by `cargo xtask gen-wires`.
Inputs live under `user/wirespecs/*.toml`; outputs are committed to
`lockjaw-types/src/wire/<family>.rs`. `--check` regenerates in memory
and diffs against the committed file; non-zero exit on drift.

Each spec describes one device family of DMA-shared wire DTOs. Every
DTO becomes a `#[repr(transparent)]` newtype over `[u8; N]` with
per-field byte-order-converting accessors and a `dma_value_impl!`
invocation that registers it with the sealed `DmaValue` trait.

## Top-level keys

| Key           | Required | Type            | Meaning                                                                  |
|---------------|----------|-----------------|--------------------------------------------------------------------------|
| `family`      | yes      | string          | Family name. Becomes the output file basename (`wire/<family>.rs`).      |
| `description` | no       | string          | Free-form text rendered into the generated module's header doc comment.  |
| `wire`        | yes      | array of tables | DTO declarations. Order is preserved in the emitted code.                |

## `[[wire]]` entries

| Key           | Required | Type            | Meaning                                                                                              |
|---------------|----------|-----------------|------------------------------------------------------------------------------------------------------|
| `name`        | yes      | string          | Pascal-case DTO type name (e.g. `VirtqDesc`). Becomes `pub struct <name>([u8; size])`.               |
| `description` | no       | string          | Rendered into the generated struct's doc comment.                                                    |
| `size`        | yes      | integer         | Total wire size in bytes. Must equal the sum of field sizes — no implicit padding.                   |
| `endian`      | yes      | enum            | DTO-wide default byte order: `big` or `little`. Per-field `endian` overrides.                        |
| `fields`      | yes      | array of tables | Fields in offset order (see below).                                                                  |

The inner byte array on the emitted newtype is private; external
consumers must go through `new()` and the field accessors. Tests in
the same module access `.0` through private-field visibility.

## `[[wire.fields]]` entries

| Key           | Required | Type    | Default              | Meaning                                                                                                 |
|---------------|----------|---------|----------------------|---------------------------------------------------------------------------------------------------------|
| `name`        | yes      | string  | —                    | snake_case field name. Becomes the accessor method name and the `new()` parameter name.                 |
| `offset`      | yes      | integer | —                    | Byte offset within the DTO. Must equal the running cursor (fields tile `[0..size)` strictly in order).  |
| `width`       | yes      | integer | —                    | Field width in bits. Must be one of `8`, `16`, `32`, `64`.                                              |
| `endian`      | no       | enum    | DTO's `endian`       | Per-field override: `big` or `little`.                                                                  |
| `default`     | no       | integer | —                    | If set, `new()` omits this field as a parameter and always writes this value. Accessor still emitted.   |
| `description` | no       | string  | —                    | Rendered into the generated accessor's doc comment.                                                     |

A `default` value must fit in the field's `width`.

The constructor signature includes every field *without* a `default`,
in declaration order. Accessors read host-order values back from the
byte array via `u<width>::from_<endian>_bytes`.

## Validator messages

Validation runs over every spec; failures `panic!` with the spec's
file path prefixed.

### DTO-level

```
<path>: duplicate wire DTO name `<name>`
<path>: wire `<name>` has size = 0; must be > 0
<path>: wire `<name>` has duplicate field `<f>`
<path>: wire `<name>` size = <s> but fields sum to <t> bytes — the resulting [u8; size] newtype would have padding, violating the DmaValue safety contract
```

### Field-level

```
<path>: wire `<name>` field `<f>` has unsupported width <w>; must be 8/16/32/64
<path>: wire `<name>` field `<f>` offset <o> is not the expected <e> (fields must tile [0..size) with no gaps and no overlaps — padding violates the `dma_value_impl!` size invariant)
<path>: wire `<name>` field `<f>` extends past declared size <s>
<path>: wire `<name>` field `<f>` default <d> > max <m> for width <w>
```

The strict cursor check means fields are declared in monotonic offset
order; gaps and overlaps are both rejected by the same message.

## Examples

Mixed-width little-endian DTO (virtio split descriptor):

```toml
family = "virtio"
description = "VirtIO split-virtqueue + virtio-blk wire DTOs"

[[wire]]
name = "VirtqDesc"
description = "Split virtqueue descriptor (spec 2.7.5)."
size = 16
endian = "little"

  [[wire.fields]]
  name = "addr"
  offset = 0
  width = 64

  [[wire.fields]]
  name = "len"
  offset = 8
  width = 32

  [[wire.fields]]
  name = "flags"
  offset = 12
  width = 16

  [[wire.fields]]
  name = "next"
  offset = 14
  width = 16
```

Fixed-default field omitted from the constructor (virtio-blk header):

```toml
[[wire]]
name = "VirtioBlkReqHeader"
size = 16
endian = "little"

  [[wire.fields]]
  name = "req_type"
  offset = 0
  width = 32

  [[wire.fields]]
  name = "reserved"
  offset = 4
  width = 32
  default = 0

  [[wire.fields]]
  name = "sector"
  offset = 8
  width = 64
```

`new()` here takes `(req_type, sector)`; the `reserved` field is
always written as `0` and its accessor returns `0`.

Big-endian DTO (fwcfg DMA header):

```toml
family = "fwcfg"

[[wire]]
name = "FwCfgDmaAccess"
size = 16
endian = "big"

  [[wire.fields]]
  name = "control"
  offset = 0
  width = 32

  [[wire.fields]]
  name = "length"
  offset = 4
  width = 32

  [[wire.fields]]
  name = "address"
  offset = 8
  width = 64
```

Multi-DTO family with a reserved-default field (ramfb config):

```toml
[[wire]]
name = "RamfbConfig"
size = 28
endian = "big"

  [[wire.fields]]
  name = "addr"
  offset = 0
  width = 64

  [[wire.fields]]
  name = "fourcc"
  offset = 8
  width = 32

  [[wire.fields]]
  name = "flags"
  offset = 12
  width = 32
  default = 0

  [[wire.fields]]
  name = "width"
  offset = 16
  width = 32

  [[wire.fields]]
  name = "height"
  offset = 20
  width = 32

  [[wire.fields]]
  name = "stride"
  offset = 24
  width = 32
```
