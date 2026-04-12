// Re-export ELF parser from lockjaw-types.
// The parser is pure logic (no unsafe, no pointer casts) — tested on host.
pub use lockjaw_types::elf::*;
