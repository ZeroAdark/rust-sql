# Fadec encode2 generated Rust contract

Reference: Fadec `3994d89500985491b1a7ccc17827a22058b3de49`,
`parseinstrs.py` functions `unique`, `encode2_gen_legacy`, `encode2_gen_vex`,
and `encode2_table`. The generated artifacts contain Rust. The runtime helper
implementations in upstream `encode2.c` and the operand constructors in
`fadec-enc2.h` remain separate, unported dependencies. This generator does not
provide a C fallback or claim that generated Rust is a working native encoder.

`public_rs` and `private_rs` are concatenated in one Rust module. Its containing
crate supplies `crate::runtime` with the operand shapes and flag constants below.
`public_rs` defines `Encode2Runtime`; `private_rs` defines ordinary generic Rust
functions. Static trait calls resolve at monomorphization, with no trait objects
or per-instruction lookup.

Each generated entry point has the shape
`pub fn fe64_NAME<R: Encode2Runtime>(buf: &mut [u8; 32], address: usize,
flags: u32, operands...) -> usize`. A masked form inserts a `FeRegMASK` argument
before the ordinary operands. `address` is the intended instruction address in
the final output, not the scratch allocation's address. Relative branch operands
are target addresses represented as `usize`; subtraction wraps at 64 bits to
preserve machine address arithmetic without signed-overflow undefined behavior.

The caller retains a 32-byte scratch array and commits only the returned prefix
when the return value is nonzero and the final destination has capacity. A zero
return leaves final output untouched; scratch may have been modified, matching
the upstream helper and generated-function failure behavior. Scratch bytes beyond
the returned prefix are unspecified. This interface is bounded and requires no
raw-pointer writes. The eventual runtime owner must validate typed operand
construction, scratch helper bounds, admission, and destination capacity. This
contract does not add a runtime implementation or a production caller.

## Operand shapes

`FeRegGP`, `FeRegGPH`, `FeRegGPLH`, `FeRegSREG`, `FeRegST`, `FeRegMM`,
`FeRegXMM`, `FeRegMASK`, `FeRegTMM`, `FeRegCR`, and `FeRegDR` are `Copy`
values with a public `idx: u8` member. `FeMem` is `Copy`, with
`flags: u8`, `base: FeRegGP`, `scale: u8`, `idx: FeRegGP`, `off: i32`.
`FeMemV` has the same members with `idx: FeRegXMM`.

High/low byte register arguments accept `impl Into<FeRegGPLH>`.
`From<FeRegGP>` preserves `idx`; `From<FeRegGPH>` stores `idx | 0x20`.
This replaces upstream's `_Generic` forwarding macro and its C++ conversion
constructors. The generated function directly checks the resulting high-register
tag when a REX prefix is needed. Other register and memory argument types preserve
the generated C signature's operand class. Immediate widths map to `i8`, `i16`,
`i32`, or `i64`; ENTER's three-byte immediate uses `i32`. Absolute address and
branch target operands use `usize` instead of raw C pointers.

The runtime exports `u32` constants `FE_JMPL = 0x8`, `FE_ADDR32 = 0x10`,
`FE_SEG_MASK = 0x7`, `FE_CC_MASK = 0xf0000`, and `FE_RC_MASK = 0x60`.

## Helper boundary

The complete helper signatures are emitted in `Encode2Runtime`. `op_imm_n`
checks the upstream signed immediate range, with the special unsigned 24-bit
case. `enc_seg67` returns the number of segment/address prefixes; `enc_rex_mem`
returns the REX bits. `enc_imm` stores the specified low-order bytes. `enc_mem`
and VEX/EVEX memory helpers use `ripoff` as the length already accounted for,
including trailing immediate bytes. Their nonzero result is the instruction
length including that accounted-for portion. Register and prefix-only helpers
return their local encoded length. Every helper must respect the supplied slice;
no successful helper may report a length beyond the enclosing scratch capacity.

Memory `off` already expresses the upstream displacement relative to the intended
instruction start. The upstream memory helpers use `off - ripoff - 5` for RIP
addressing and never inspect the output pointer, so their slice interface needs
no separate address argument. Only generated relative branches use `address`.

The generator preserves prefix order, speculative REX-byte stores, conditional
variant selection, fixed ModRM forms, short-jump preference, high-byte rejection,
mask-zero rejection, VEX downgrade bits, broadcast/rounding fields, AMX forced
SIB, VSIB routing, displacement scale, and helper-return offset adjustments.
Assertions in the upstream generator become explicit generator errors.

## Semantic trace protocol

For each mnemonic in generator insertion order, `semantic_tsv` contains
`function<TAB>fe64_NAME<TAB>TRACE<LF>`. TRACE is the upstream C function
signature and its complete brace-delimited body with all ASCII whitespace
removed. It includes every condition, expression, byte write, helper call, and
return. Whitespace removal also removes spaces inside the signature; it performs
no token rewriting or C parsing. If a high/low-register forwarding macro exists,
the next row is `macro<TAB>fe64_NAME<TAB>TRACE<LF>`, where TRACE is its entire
`#define` line with the same whitespace removal. Public C declaration semicolons
are not part of function traces. Generated Rust receives its expressions directly
from the Rust generator; it does not translate or execute the trace.

The test oracle extracts these traces independently from the original Python
generator's actual C outputs. Matching traces establish source-form generator
parity, including signatures and branches. The test-only `SHAPE_RUNTIME` in
`tests/encode2.rs` defines operand shapes and constants without implementing any
runtime helper; compiling it with all generated Rust verifies their type and
syntax contract. These are distinct checks. Neither establishes encoded-byte
equivalence or generated-code performance, which require the complete runtime
port and actual compiled upstream encoder on the runners.

The stronger execution harness in `tests/generated-execution/` compiles unchanged
Python-generated C bodies with matching test recording helpers, and executes the
emitted Rust bodies through `Encode2Runtime`. Its independent C-signature roster
must equal the Rust function-name set, and every form is invoked. The selected
cases vary register/byte-register boundaries, immediate widths, segment/address
prefixes, condition/rounding codes, masks, relative targets, memory fields, helper
lengths and injected helper failures. Ordered helper arguments, return values,
all 32 scratch bytes and generated instruction lengths compare. C canaries and
undefined-behavior/bounds instrumentation, plus checked Rust execution, constrain
the observation harness. C signed-address overflow is explicitly excluded.

Equivalent profile contents share an execution artifact only after hashing both
the actual C body/declaration output and emitted Rust source; a durable map lists
every alias. Family counts describe source-pattern-selected cases, not measured
branch coverage. The helper fingerprint bytes do not represent real memory or
VEX/EVEX encoding. `disp8scale: i32` is observed as its u32 bit pattern to match
the original helper's unsigned argument, including the all-ones sentinel.
Generator-owned execution and actual encoder-runtime qualification remain
separate claims.

The generator is a cold build tool. Its owned `String`, `Vec`, and ordered sets
are deliberate process-lifetime tooling allocations; generated instruction
functions allocate nothing and require no heap collection.
