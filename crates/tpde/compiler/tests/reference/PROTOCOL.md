# CompilerBase component oracle, protocol 1

Reference: TPDE `116b2b83eb8491fd63c017285c91b007b8a26a67`.
The 39 pristine reference files and licenses are listed in `manifest.json`.
`oracle.cpp` instantiates the actual upstream CompilerBase, its nested guards,
Analyzer, parsed TestIR, AssemblerElfA64 and FunctionWriter templates. Target
callbacks record effects. Their one-byte marker output is an observer target;
it is not executable A64 code or native encoder evidence.

## Build

Use the runner's C++20 compiler, `-stdlib=libc++`, both assertion configurations,
and include both this directory and `tpde/include`. Compile and link:

- `oracle.cpp`
- `tpde/src/Assembler.cpp`
- `tpde/src/AssemblerElf.cpp`
- `tpde/src/FunctionWriter.cpp`
- `tpde/src/StringTable.cpp`
- `tpde/src/ValueAssignment.cpp`
- `tpde/src/base.cpp`
- `tpde/src/test/TestIR.cpp`
- `tpde/src/util/SmallVector.cpp`

The normal reference uses no TPDE_LOGGING. Sanitized runs add address/undefined
sanitizers with no recover and verify the explicit sanitizer mode. base.cpp is
required for the real register-exhaustion fatal_error implementation.

## Requests

With no arguments, stdin begins with exactly five fields and a newline:

```
MODE NEGATIVE FIXED STRIDE TABLES
```

NEGATIVE, FIXED and TABLES are 0/1. STRIDE is an unsigned integer. The shared
test-support compare_bytes helper can pass this entire request without a custom
process interface. For manual debugging on a runner, the executable also accepts
`MODE [NEGATIVE [FIXED [STRIDE [TABLES]]]]`; omitted values default to one.
No laptop execution is permitted.

Every successful request starts `tpde.compiler.state 1 NEGATIVE`. All subsequent
records are ASCII tags followed by decimal unsigned 64-bit fields. Signed
values are converted to uint64_t using standard C++ integral conversion; a
negative i32 stack offset is therefore sign-extended before conversion unless
the target hook signature itself receives upstream u32. Rust must preserve the
actual hook's u32 offset boundary before comparing. No pointer, allocator
address, container capacity or unstable hash-table iteration order is emitted.

### sanitizer

No payload. Emits `sanitizer 0|1` from the actual upstream sanitizer activation
constant. This must agree with the separately observed Rust allocator cfg.

### frame

Payload is whitespace-separated `alloc SIZE` and `free SIGNED_OFFSET SIZE`
commands. The source frame starts at 16 so offset zero remains reserved.
Each operation records input and result/frame size. Final fixed free lists are
reported in bank order; nonempty dynamic size lists are sorted by size,
preserving each list's source insertion/LIFO order. Both observers omit empty
dynamic classes: Rust reserves class/free-list capacity before allocation and
retains empty class metadata across resets so release cannot allocate; C++
creates classes on free and clears them on reset. This deliberate storage and
admission difference does not change active free slots. No nonempty class,
offset, or within-class order is normalized. Cover size zero, each small size class,
alignment gaps, sizes above sixteen, repeated reuse, and both frame directions.

### guards

No payload. Actual source guards exercise both register banks, empty release,
same-bank allocation, moves, retained clobbers, default and borrowed ValuePart
shape, all scalar immediate sizes, scratch/value destination materialization,
assignment metadata after salvage, fixed destination refusal, assigned
destination relock, and expression fast-path/materialization reuse.

`specific.overwrite` records the source's defined but inconsistent bookkeeping
when alloc_specific(reload=false) replaces a different assignment register:
both old and new register bits remain used. The harness then explicitly removes
the old bit to permit ordinary fixture cleanup; the port is judged on the
recorded state before that cleanup.

`specific.after.eviction` exercises the need to inspect the source assignment
after target eviction: the recording spill hook for target register 7 allocates
a real ScratchReg at 2, invoking actual eviction/spill of the source. The outer
load_to_specific must observe that its old source register was invalidated and
reload from its stack slot. The hook does not replace the core eviction logic.

These assignment fixtures use actual AssignmentAllocator allocations and fully
initialized assignment/register metadata. Non-liveness guard paths can be
exercised without inventing Analyzer state. All guard resources are reset
before release_assignment. No invalid pointers survive fixture cleanup.

### compile

Payload is a complete TestIR module. Parser failure emits `parse 0`; accepted
source emits `parse 1`, function symbols and target event/state records, then
`compile 1|0` and reset sizes. Actual compile(), compile_func(), compile_block(),
allocation, call builders, return builder, branches, switch selection and PHI
algorithms run from the pinned header.

Fixture convention:

- Two banks of sixteen allocatable registers; first four of each bank are
  argument registers; registers 8–15 are callee-saved. Four fixed assignments
  are permitted per bank, selected from registers 8–11, unless FIXED=0.
- Eight-byte parts. Names beginning `p2_` or `p3_` select two or three parts.
  Names beginning `v_` select vector bank one. All other values have one GP part.
- Static allocas are at most 256 bytes with alignment at most sixteen. Both
  recording adaptors ignore only these allocas in liveness; larger or more
  aligned allocas receive normal liveness for the base's dynamic lowering.
  This extends the standalone TestIR adaptor's all-allocas-ignored policy to
  match the recording target's smaller static limit.
- Argument names `byval_` use 24-byte/eight-aligned caller slots; `sret_` uses
  dedicated register 3. Corresponding call operands use the same flags.
  `sext_`/`zext_` call operands request extension from eight bits.
- Non-argument-prefix behavior is deliberately supplied by this fixture;
  ordinary TestIR has no such ABI or multipart name semantics.
- Target operations `any`, add/sub, conditional-select and zerofill read their
  declared operands, then allocate all result parts. Same-bank still-loaded
  operands may be salvaged; exhausted or different-bank sources allocate fresh.
- A TestIR `jump` models a switch: first successor is default and each later
  successor has key `(index-1)*STRIDE`; width is 63. This tests the real Base
  compare/binary/jump-table choices while retaining the actual TestIR CFG.
  TABLES=0 declines the target's jump-table hook. Cases require nonempty jump
  successor lists, positive stride and sorted nonoverflowing keys.
- All source target hooks emit one marker byte, preserving actual writer
  offsets, alignment, labels, symbol definition and module cleanup. The writer
  records final jump-table labels. It has no native relocation/fixup output.

`programs/` contains deterministic starting witnesses; the coordinator owns
the final case roster, Rust recording target and positive case-count floors.
No fixture file or source review alone counts as a differential PASS. Full
CompilerBase coverage still needs explicit audit accounting for special values,
custom variable-reference policy, failure/cancellation and remaining structural
boundaries; those are not implied by a successful basic program trace.
