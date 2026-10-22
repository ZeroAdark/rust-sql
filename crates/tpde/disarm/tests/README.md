The `upstream` integration target executes four differential tests against
pristine Disarm C at `2d13d3f410a52daff1c5d8ef07d623332f372560`.
Production Rust generation and the C oracle both use the selected profile:
`all` by default, matching pinned CMake, or explicit `really-all`.
The C reference includes all four actual runtime translation units and generated
headers from the pinned Python implementation. No recording helper replaces a
runtime implementation. The reference manifest includes all ten input files,
the license, and pinned CMake configuration.

Each test first prepares inputs and compiles all four C builds before executing
Rust correctness cases: assertions/NDEBUG crossed with native and ASAN+UBSAN.
`c-compile-verdicts.tsv` records their independent compilation results. The later
comparison phase consumes these exact executables without rebuilding. All four
compilation and comparison modes are attempted; `c-mode-verdicts.tsv` retains their
separate results before the test's aggregate assertion. Artifact directories
include profile, suite, and process identity. All execution belongs on the runners.

| Test | Case source and observations |
|---|---|
| Generated encoders | Every emitted function from the independent pinned model; sorted names must equal the actual Rust signatures. All functions require at least one nonzero encoding witness. Full operand domains include coupled register and immediate boundaries, signed/address arithmetic, logical immediates, all floating immediates and byte-mask SIMD constants. `really-all` has exactly 3,911 functions, 217 calling handwritten helpers. Upstream-omitted functions have their own roster. |
| Decoder and formatter | Every trie description: base/all-free words, each free-bit set/clear, fixed-bit flips, eight deterministic samples, then 262,144 random words and four explicit words. All has 9,102 patterns and really-all has 9,112, matching retained pinned-generator runner rosters. Compares classifier, mnemonic, all five operand fields, retained/partially overwritten immediate state, and both formatting entry points at varying addresses. |
| Helpers and MOVconst | 236,188 cases: sign extension 8,694; arithmetic immediate 24,612; logical immediate 23,028; fmov32 5,637; fmov64 16,645; SIMD immediate 12,804; MOVconst 144,768. MOV words are also independently reconstructed, covering all four output lengths and all four first-instruction classes. Every shorter capacity refuses atomically, then the same buffer is reused. |
| Formatter states | All 27 operand kinds, all 65,536 system-register values, all conditions/prefetch operations/floating literals, offset and register/table boundaries, scaled extensions, separators, NONE termination, and out-of-table mnemonic behavior. These constructed states isolate formatter branches from decoder admission. |

Binary protocols are little-endian. Encoder rows contain an id and eight u64
argument bit patterns (68 bytes); outputs are actual u32 instruction words.
Decoder rows contain word, initial immediate, and absolute address (20 bytes).
Outputs contain classifier, mnemonic, semantic operand fields, final immediate,
and two u16-length-prefixed NUL-terminated strings. Operand padding bits are
normalized by named C fields rather than used as a semantic observation.
Manual formatter rows contain mnemonic, five operands, immediate, and address
(38 bytes), producing the same two string records. Helper rows contain opcode,
parameter, two reserved zero bytes, and u64 input bits (12 bytes); helper results
are u64, while MOVconst returns a u32 count and four canary-retaining u32 slots.

Formatting compares the complete C-visible string through its first NUL.
Rust's returned cursor, final NUL, capacity and canaries are checked separately:
pinned scaled-LSL formatting intentionally embeds an earlier NUL. Writes beyond
the logical result caused by upstream fixed-width copies are not part of the
bounded Rust output contract. The C differential REGVIDX corpus stops at element
63: element 64 makes upstream's four-byte decimal copy read one byte past its
source table. Rust's safe decimal-64 extension is checked separately without
calling undefined C behavior; other decimal-64 paths remain in the C corpus.

Both C formatting buffers are initialized to the same sentinel as Rust. Upstream
copies only twelve mnemonic bytes even when the encoded length is thirteen,
leaving a caller-owned byte inside the logical text for `msr svcrsmza,`.
The complete decoder corpus retains both affected words and compares this
defined initialized-buffer behavior. Targeted Rust assertions additionally check
both formatting APIs with zero, `Z` and `?` gap bytes, exact capacity and every
shorter capacity, preserving the gap and atomic refusal.

These tests establish component behavior and encoding bytes, not native execution
of emitted AArch64 instructions or performance parity. Until exact-source runner
receipts are retrieved, their execution status is UNRUN.
