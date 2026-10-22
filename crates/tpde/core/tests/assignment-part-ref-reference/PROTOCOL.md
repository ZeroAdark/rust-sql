# AssignmentPartRef reference protocol

The source manifest records pristine TPDE revision
`116b2b83eb8491fd63c017285c91b007b8a26a67`, including required headers,
ValueAssignment.cpp, SmallVector.cpp, and both license notices. The oracle
compiles the actual header and allocator implementation. It contains no
replacement implementation of the packed-word operations. Rust uses the shared
oracle compilation helper; this component defines its own protocol and compares
actual source outputs. All execution is runner-only.

`exhaustive` emits 65,536 fixed-size binary records, one for each initial u16
packed word. Each 140-byte record contains seven little-endian u32 getter
results: bank, register, modified, fixed assignment, register valid, stack valid,
and part size. It then contains 56 little-endian u16 mutation results, restoring
the initial word before each operation: reset; banks 0–7; all 32 register IDs in
the initial bank; both modified, fixed and register-valid values; stack-valid;
and all eight powers of two from 1 through 128. Thus every setter is compared for
every initial reserved-bit and field combination, including register 255/bank 7.
Each side checks that the header and neighboring parts remain unchanged. The
comparison is exact, with no checksum collision risk; failure reports identify
the original word, API operation, argument and differing result bytes. Per C++
mode this is 458,752 getter results and 3,670,016 setter results.

`offsets` emits 18,432 labeled text rows: all 256 maximum-part-size values,
12 indices spanning zero, live parts, out-of-range but header-only indices,
16-bit boundaries, the first multiplication-wrap boundary for max size 255,
2^31 and u32::MAX; and six positive/negative/extreme nonzero frame offsets.
Neither implementation accesses a packed part for these header-only queries.
The source constructor admits arbitrary u32 indices, and this domain is
preserved. Unsigned multiplication/addition wrapping and the target's signed
result conversion are compared directly. Another 24 rows exercise every
variable/stack flag combination, both variable forms, and the same frame-bit
patterns. Actual packed accesses outside the allocation are unsafe in C++ and
explicitly checked in Rust; they are not executed as a parity case.

`reuse` emits three epochs using actual three-part assignments. Two copied views
observe reset, all state transitions, bank/register/size changes, and live
header changes between ordinary, custom-variable and stack-variable forms.
Each epoch emits the normalized allocation-reuse decision and 14 full snapshots
including both neighboring parts. The second epoch reuses its free-list node;
the third follows allocator reset and reuses the retained first slab. Views stop
being usable before the Rust owner is recycled. Compile-fail ownership cases
live in the production owner's accompanying documentation.

`control NAME good|bad` implements twelve assertion-controlled invalid forms,
each paired with a successful operation changing the relevant precondition:
bank 8, wrong register bank, non-power-of-two size, size 256, ordinary-frame access
on a variable reference, uninitialized frame offset, and each variable accessor's
reference-kind/stack-kind/part-zero requirements. C++ assertions are explicitly
enabled with `-UNDEBUG`; a negative process must report an assertion failure,
not merely any unsuccessful exit. Corresponding Rust calls must reject the same
invalid forms. The sanitized size 128 positive control runs before `zero-size`,
which intentionally diagnoses source `__builtin_ctz(0)` undefined behavior. This
is separate from defined-input parity: Rust explicitly rejects size zero before
any count-trailing-zeros operation.

The three integration tests share process-local compiled oracle executables.
Ordinary qualification compares assertions and NDEBUG, plus an independent
ASAN+UBSAN binary for the source-UB control. During Rust ASAN qualification, both
ordinary C++ modes also enable ASAN+UBSAN; all three tests run with the same
minimum floor. Required source builds have flags `-UNDEBUG`, `-DNDEBUG`, and each
of those plus `-fsanitize=address,undefined -fno-sanitize-recover=all`.

Target: `assignment_part_ref`; feature: `upstream-oracle`; minimum tests: 3 both
normally and under ASAN. The principal marker starts
`TPDE_ASSIGNMENT_PART_REF_ORACLE PASS active=0` normally, or `active=1` with ASAN.
Additional markers describe offset/reuse cases and positive/assertion/UB controls.
Performance qualification remains unrun; these are correctness comparisons.
