//! PostgreSQL's `get_hash_memory_limit()` (nodeHash.c): a plain run-time read of `work_mem` and `hash_mem_multiplier`,
//! not a recorded program input and never cached, and the one reader of that product: exec ops/memoize's Cache object
//! reads it at every execution's prepare, as ExecInitMemoize (nodeMemoize.c) does, and the execution's memory grant
//! (knobs/room.rs `granted_memory`) takes it as each participant's hash budget. `work_mem_bytes` is the same plain read of
//! `work_mem` alone: EXPLAIN ANALYZE's account of a sort's or a tuplestore's memory (tuplesort.c / tuplestore.c
//! `allowedMem`, entry instrument.rs).

/// PG's `get_hash_memory_limit()`: `work_mem` kB × `hash_mem_multiplier`, as bytes.
pub(crate) fn hash_memory_limit() -> u64 {
    let kb = guc_tables::vars::work_mem.read() as f64;
    let m = guc_tables::vars::hash_mem_multiplier.read();
    let b = kb * m * 1024.0;
    if b >= u64::MAX as f64 {
        u64::MAX
    } else {
        b.max(0.0) as u64
    }
}

/// `work_mem` kB as bytes (tuplesort.c / tuplestore.c `allowedMem`).
pub(crate) fn work_mem_bytes() -> u64 {
    (guc_tables::vars::work_mem.read().max(0) as u64) << 10
}
