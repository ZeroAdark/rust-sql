# SegmentedVector reference

`manifest.json` pins the unmodified upstream files and their SHA-256/Git blob
identities to TPDE `116b2b83eb8491fd63c017285c91b007b8a26a67`. These copies are
test-only; the Rust implementation has no C++ dependency.

The differential runner compiles `SegmentedVector.hpp` and its real
`SmallVector.cpp` dependency in assertion-enabled, `NDEBUG`, and
ASAN+UBSAN modes. Segment sizes 1, 3, and 64 cover empty construction, repeated
empty clear, partial and full segments, more than 32 allocated segments, retained
segment reuse, immutable/mutable indexing, lvalue copy insertion, rvalue move
insertion, variadic emplacement, and final destruction without an explicit clear.

The element type requires 128-byte alignment and records construction, copy
construction, and destruction as ordered `(kind, id, value)` events. C++ move
construction transfers ownership without another logical construction event;
destruction of that moved-from object produces no logical destruction event.
This normalization corresponds to Rust's move semantics. Both final value and
event sequence are compared, including copied-source lifetimes and mutations
observed by element destructors. Constructor exceptions are outside the C++
valid-input oracle because upstream increments its live cursor before
construction succeeds; Rust panic/allocation rollback belongs to Rust unit tests.

Addresses are compared as booleans for alignment, contiguous elements within
each segment, equality of const/mutable indexing, stable addresses after growth,
and equality with the original addresses after clear/reuse. Raw process addresses
never cross the protocol. A live reference is also read after appending across
segment boundaries.

The input is one byte of segment size, a little-endian `u32` operation count, and
operations encoded as `(u8 opcode, u32 count-or-index, u64 value)`. Opcodes are
0 no-op, 1 clear, 2 repeated moves, 3 repeated emplacement, 4 repeated copies of
one source, 5 indexed mutation, and 6 append while retaining a live reference.
Snapshots follow construction and every operation. Each snapshot contains a
`u64` length, seven `u64` fields per element (id, value, and five address checks),
and the entire event history (count followed by event triples). The final event
history follows vector destruction. All output integers are little-endian.

Both non-sanitized modes separately reject C++ copy/move construction and
assignment. Rust supports ownership moves and does not expose clone/copy of
the vector; Rust lifetime and aliasing rejection cases are maintained separately.

Run through the runners with `upstream-oracle` and test target `segmented_vector`.
The completion marker is `TPDE_SEGMENTED_VECTOR_ORACLE PASS`.
