# Disarm generation in Rust

`tpde_disarm_gen` rewrites the complete pinned Disarm `parse.py` generator.
It reads the pristine descriptor and feature inputs in `reference/` and emits
Rust source. Python and generated C are isolated oracle/inspection artifacts.

The public API is `generate(descfiles, feature_desc, &Options) -> Result<Generated,
String>`. Inputs are borrowed UTF-8 text; results own their generated text.
Every call starts with fresh parser, feature, decoder and encoder state. Errors
discard the incomplete result and the API can be called again immediately.

The CLI has the upstream positional layout:

```text
disarm-gen --feature-desc feat.txt --features all \
  --manifest forms.tsv public.rs private.rs desc.txt
```

`--coverage` generates classifier matches in place of lookup tables.
`--encode-in-header` puts inline encoder definitions in `public.rs`; the default
puts their implementation module in `private.rs` and reexports it publicly.
`--reference-c` emits the original C header/table layout for oracle inspection;
production integration consumes the default Rust output. The `--features`
selector and `--feature-desc` input preserve upstream feature/profile rules,
including ignored `requires`/`availopt` properties, incomplete features, and the
literal `armv8.4` availability spelling in the pinned input.
Value-taking flags reject a following option token; use `--features=VALUE` when
a literal value begins with an option spelling. Numeric syntax uses ASCII
digits and option names use their complete spelling.
The pinned oracle's Python 3.11 argument handling treats `--features=--` and
`--feature-desc=--` as unset options. An empty feature-description value also
disables feature selection. Use `--feature-desc=./--` to read a file named `--`.

Include the generated files in this arrangement:

```rust,ignore
mod runtime {
    // The separately ported encoder runtime types and helpers live here.
}
mod generated {
    include!("public.rs");
    include!("private.rs");
}
```

An encoder build script can call the library directly and write these strings
to `OUT_DIR`; no Python or C generator runs in that path. Register classes are
`runtime::{GReg,GRegZr,GRegSp,VReg}`, each exposing `value(self) -> u8`.
`Cond` and `PrfOp` must cast to their unsigned numeric encoding. Checked
constructors and GP-to-SP/ZR conversions belong to the runtime. The generator
emits distinct concrete parameter types; callers use an explicit conversion
where C used its macro conversion.

The generated encoder additionally requires these exact runtime signatures:

| Helper | Signature |
|---|---|
| `da_sext` | `(i64, u32) -> i64` |
| `da_immadd` | `(i64) -> u32` |
| `da_immlogical` | `(u64, u32) -> u32` |
| `da_immfmov32` | `(f32) -> u32` |
| `da_immfmov64` | `(f64) -> u32` |
| `da_immsimdmovi` | `(u64) -> u32` |

The Rust classifier, group/mnemonic constants and packed decoder strings need
no runtime dependency. Operand decoding emits a `DecoderHelpers` trait with
every actual helper signature and `da64_decode_operands<H>` specializations.
The separately ported decoder must implement those helpers and operand writes.
The generated trait is a direct monomorphized interface; it has no dynamic
callback table. Generator completion does not claim completion of
`disarm64.h`, `encode.c`, `decode.c`, `classify.c`, or `format.c`.
Packed string fields preserve upstream Python character counts and indices,
including its treatment of multibyte UTF-8. Expression lowering uses declared
helper/operand types and generated constants; unknown external C symbols and
ABI-dependent plain `char` casts produce errors. Explicit signed and unsigned
integer types use the 64-bit LP64 target model.

`manifest` is protocol version 1, deterministic TSV. Rows record the pin and
output modes, feature booleans, group/mnemonic IDs, every classifier pattern,
decoder strings, and every encoder form. A `form` row contains group, category,
name, fixed value, fixed mask, enabled flag, feature expression, operands and
condition. `emitted` rows number the generated functions; `omitted` rows identify
the upstream missing-field omission behavior. This distinction prevents a
source omission from disappearing into the form count.

This is a standalone build tool with cold process-lifetime ownership. Ordinary
`Vec`, `String`, `Box` and ordered maps are deliberate: these objects never enter
an engine arena or per-instruction compiler path and their destruction reclaims
the generation process's working data. Descriptor/format inputs, expansion
work and generated-format bytes have explicit limits, and malformed or
ambiguous inputs produce errors. The tool does not claim query-context memory
accounting or an engine no-drop contract.

All execution belongs on the runners. The `upstream-oracle` feature enables the
`upstream` integration target. It checks the pinned Python generator's full
outputs across features/profiles and modes, independently derived form rows,
deterministic Rust generation, and compiled generated consumers. Test helper
contracts that panic when called provide type checking only; they do not count
as implementations or runtime encoder evidence. Runner registration, images and
source submission are owned by the coordinator.
