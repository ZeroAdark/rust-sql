# ValueAssignment pinned reference protocol

The reference files and license notices are pristine TPDE revision
`116b2b83eb8491fd63c017285c91b007b8a26a67`; manifest.json records their hashes.
`oracle.cpp` includes the actual ValueAssignment.cpp into its translation unit,
which makes its anonymous allocation-info helper directly testable without a
second implementation. SmallVector.cpp is compiled separately. No production
Rust code calls C++.

Each command emits a labeled text line; the Rust comparator reports the precise
line and both values on mismatch. `L` reports fixed header layout. `I count`
reports actual upstream logical size, rounded allocation size, and free-list
index. This covers both sides of every source-defined transition, classes 0–27,
without actually allocating gigabytes. `U count` intentionally reaches class28
under ASAN+UBSAN; shift-by-32 must fail with its precise sanitizer diagnostic.
Rust defines this arithmetic as a 4GiB allocation request, whose actual admission
still follows the mcx allocation limit. This is explicitly not source parity for
undefined arithmetic.

`A slot count mode seed flags` allocates normally (mode0), through the explicit
slow path (mode1), or with skip-free-list (mode2), then writes every live header
field and part. A mode2 `Z` line inspects the actual freshly zero-initialized fixed
header before Rust publishes its safe assignment token. Uninitialized fast-path
storage and free-list contents are never read as live fields. `M slot field
value` writes all flag forms, both intentional scalar unions, maximum part size,
and selected packed parts. Snapshots include normalized slab identity and offset,
slab size, all live slab accounting, exact header fields and a wrapping checksum
of every part. Slab allocation decisions come from actual aligned-new interception
in C++ and the independently ported BumpAllocator diagnostics in Rust.

`F slot slow` consumes a live assignment through normal or explicit slow free;
`R` resets only after all slot tokens are gone. Slab identities remain monotonic
across resets while the first regular slab is retained. Five epochs cover zero
parts, all tested allocation class boundaries, LIFO and cross-count reuse,
skip-free-list bypass with populated lists, oversized allocations, 40 simultaneous
allocations spanning slab growth, 600 deterministic randomized operations, and
reuse after repeated reset. Freed node payloads are not observed except ASAN
shadow. `P slot offset retired` compares actual active shadow at header/live-tail
and freed-header addresses; all queried underlying slabs remain live.

The ordinary differential runs in assertion-enabled and NDEBUG modes. A separate
ASAN+UBSAN reference runs a defined-input control before diagnosing class28; its
intentional failure does not count as an ordinary parity pass. Under the Rust
ASAN qualification, both ordinary C++ modes also carry ASAN+UBSAN, and an extra
shadow test covers nine representative small/large counts. Debug/dist/Miri
library tests independently cover ownership, allocation failure, panic cleanup,
reentrancy and source size() wrapping without huge storage.

Test target `value_assignment`: two normal tests, three with active ASAN.
Markers: `TPDE_VALUE_ASSIGNMENT_ORACLE PASS` and
`TPDE_VALUE_ASSIGNMENT_SOURCE_UB PASS`; the active shadow test emits
`TPDE_VALUE_ASSIGNMENT_ASAN PASS ... active=1`.
