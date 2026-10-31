# Aggregate validity-byte forwarding

Mixed-workload CPU profiles identified a dependency in generated aggregation code:
marking a count valid stored a byte, then the adjacent sum loaded that same byte.
The validity byte packs several aggregate states; counts must retain their bits
because partial-record merging uses them. Removing validity bookkeeping would
change NULL, FILTER and merge behavior.

`ops/shared/slots/validity.rs` reuses a byte already loaded or stored in the current
IR basic block. It examines at most 16 preceding instructions, making each lookup
O(1) with a fixed bound. It allocates no cache and adds no per-row metadata. The
scan limit also bounds the distance over which the reused value stays live.
Every validity store remains in the program.

Forwarding requires the same SSA pointer, offset and I8 memory type. A different
SSA pointer is treated as possibly aliasing. A scalar store can be crossed only
when its byte interval is disjoint, relative to the same pointer; interval
arithmetic widens signed offsets before adding the store width. Calls, vector
writes and any instruction outside the explicit reviewed whitelist stop the
search. Nothing is reused across a block boundary. Narrow values are explicitly
zero-extended, including byte values 128–255. This applies to aggregate record
validity, which is private during updates or immutable during reads, not to
concurrent synchronization flags.

Interpreter tests cover all 256 byte values, signed narrow loads, adjacent words,
overlapping wide stores, distinct pointers that alias at runtime, calls that
modify a record, block boundaries and the search limit. The SQL probe compares
88 executions against native PostgreSQL, including typed NULL-only and empty
results, FILTER, more than eight aggregate slots, numeric transitions, outer
joins, grouping sets, DISTINCT, nested/correlated aggregation and many groups.
It runs both ordinary and prepared execution at 64 KiB and 16 MiB work_mem.
The low work_mem cases are correctness coverage, not proof that every engine
used a particular spill algorithm.

The profile's group-only producer previously contained 15 validity-byte loads
across its three row bodies; it now contains six. Its drain functions remain
unchanged. Disassembly confirms the store-followed-by-reload dependency is
replaced by a register read. Profiling is diagnostic only: resource claims use
separate unprofiled runs with the [fixed-work method](resource-benchmark.md).
See [resource measurements](performance.md) for CPU, latency and memory results;
fewer loads alone do not establish a performance improvement.
