# Fresh Disarm runtime

`tpde_disarm` rewrites the complete pinned Disarm header, encoder, classifier,
decoder, and formatter in allocation-free Rust. The build uses the fresh Rust
generator. Its default `all` profile matches upstream CMake; enable `really-all`
to include the upstream incomplete-extension profile.

```rust
use tpde_disarm::{da64_format, de64_ADDx, decode, GReg};

let register = GReg::new(3).unwrap();
let word = de64_ADDx(register.into(), register.into(), register.into());
let instruction = decode(word);
let mut text = [0; 128];
let length = da64_format(&instruction, &mut text).unwrap();
assert_eq!(&text[..length], b"add x3, x3, x3, lsl #0");
```

Generated encoders retain upstream's `u32` result and zero failure sentinel.
`encode_word` copies any word as four little-endian bytes. `de64_MOVconst` writes
the source's complete one-to-four-instruction sequence into a checked word slice.
Formatting writes a NUL-terminated byte prefix and returns its logical length
excluding the final NUL; source table quirks can leave earlier embedded NULs.
Mnemonic copies retain upstream's twelve-byte limit: longer mnemonic entries
preserve the caller's existing bytes in the gap before the operands.
Short output and malformed formatter operands return errors without mutation.

`FORM_MANIFEST` identifies every generated feature, group, decoder pattern, and
encoder form; `GENERATION_PROFILE` identifies the actual build profile. The typed
SP/ZR carriers preserve operand classes. Decoded operands expose their compact
C-layout fields and safe accessors, including retained immediate/float union data.

`upstream-oracle` enables the compiled-C differential tests. Run them only through
the registered runner interface, with and without `really-all`. Source provenance
and licenses are in `reference/manifest.json` and `NOTICE`. Tests are component
evidence; the ARM64 writer/compiler/installer and performance gates remain open.
