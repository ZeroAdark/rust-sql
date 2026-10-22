# BumpAllocator reference protocol

The oracle compiles `../bump_allocator_oracle.cpp` and the pinned
`tpde/src/util/SmallVector.cpp` with this directory's `tpde/include` on the
include path. Build separately with assertions and `-DNDEBUG`; ASAN builds add
`-fsanitize=address -fno-omit-frame-pointer`. Exceptions remain enabled.
`manifest.json` identifies the unchanged upstream files and licenses.

Input is whitespace-separated text. Start with slab size `64`, `128`, or `4096`,
then any sequence of the following commands, ending at EOF.

| Command | Output |
| --- | --- |
| `A size align` | `A slab_id offset slab_size live_slabs live_bytes` |
| `S size align` | `S slab_id offset slab_size live_slabs live_bytes` |
| `R` | `R live_slabs live_bytes` |
| `D` | `D move_ok released_value array_sum caught_exceptions destructors live_slabs live_bytes` |
| `P slab_id offset` (ASAN only) | `P poisoned` |

`A` calls upstream `allocate`; `S` calls upstream `allocate_slab` directly.
Sizes must be positive; alignments are powers of two no greater than 16.
The oracle fills each returned allocation with `0x5a`, exercising its entire
unpoisoned extent. `R` calls upstream `reset`. `P` queries one byte inside a
live slab using `__asan_address_is_poisoned`; `poisoned` is 0 or 1.

Slab IDs start at 0 and increase monotonically, including after reset. The
oracle intercepts the aligned global allocation/deallocation functions called
by upstream and records the real returned pointer and requested slab size.
Offsets are relative to those pointers. Regular and oversized slabs share the
same ID space; internal SmallVector bookkeeping uses malloc/realloc and does not
enter this ledger. It retains only observation data and does not choose any
allocator branch or infer slab boundaries from a separate allocation model.

`D` executes five allocations on the active allocator:

1. Placement-new an 8-byte object, move its `BumpAllocUniquePtr`, and reset the
   new owner, invoking its destructor once.
2. Placement-new another 8-byte object containing 43, release its unique pointer,
   and invoke `BumpAllocatorDeleter` explicitly, invoking its destructor once.
3. Placement-new a trivial 24-byte array containing `[2, 3, 5]`, sum its values,
   and invoke the matching no-op placement array delete explicitly.
4. Placement-new an 8-byte object whose constructor throws, exercising the
   matching no-op placement delete.
5. Placement-new a 16-byte array whose first constructor throws, exercising the
   matching no-op placement array delete. The element destructor is trivial.

Allocation alignments are respectively 8, 8, 16, 8, and 16. The first five
output values are `1 43 10 2 2`. The allocator retains all five allocations;
subsequent `A` commands expose the resulting cursor position. Native Rust
constructor-failure tests should exercise the corresponding error/unwind
cleanup through Rust ownership APIs, with any differences recorded explicitly.

EOF destroys the allocator, requires all observed slabs to have been freed,
and emits `E 0 0`. Invalid protocol inputs abort. The fixed observation ledger
supports 65,536 total slab allocations per process.
