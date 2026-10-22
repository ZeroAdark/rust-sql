//! The run time's own sizes (the driver's floors, windows and chunks): values as the driver had them, each with what
//! it bounds and where it comes from; the driver names them from here.

/// The least window an ordered section gets (`ClaimOrder::for_section`: the window is 2 x W, at least this): 2 x W
/// alone left a W2 / W4 ordered emit parking most of its claims, at most this many more claims buffered.
/// MEASURED: a selective filter scan, 284 claims of ~6 us: W2 3.4 ms, W4 1.8 against the unordered twin's 2.1 / 1.35.
/// MEASURED-HELDOUT (DR-20; shapecheck sweep-DR-20-w2.tsv / sweep-DR-20-w4.tsv: the ordered-emit corpus, a scan returning
/// 0 / 1 / ~250 / ~2k / ~20k / ~200k rows straight to the client, narrow and wide, cheap and costly claims, floors 4 / 8 /
/// 16 / 32 / 64, one take each, the same answers every arm): W2 ran 327-346 ms (5.7 %, not monotonic), W4 195-216 ms,
/// where floors 4 and 8 open one window at full width (2 x W = 8) and ran 216 / 197 ms: take noise. The value stays.
pub const WINDOW_FLOOR: usize = 16;

/// D2985: an ordered emit's leg checks a pushed block for sparseness (its rows' by-reference images under half the block's
/// region, `legs.rs sparse_region`) only past this many region bytes and this many region bytes per row: below them the
/// region is copied whole, one memcpy, as before. A floor, not a tuned value: a 64 KiB region is under 1 % of the regions
/// that held the leak (~66 MB per claim of a wide text table read at 1 % selectivity), and 1 KiB per row is past any row
/// whose images fill its region.
pub const LEG_SPARSE_MIN_BYTES: usize = 64 << 10;
pub const LEG_SPARSE_ROW_BYTES: usize = 1 << 10;

/// The EXPLAIN ANALYZE pruning pins a failed statement can leave behind (exec ops/append/prune.rs `pin_key`: no
/// ExecutorFinish / ExecutorEnd takes them back), per backend: the oldest goes first past this many.
/// Chosen capacity, not measured: it bounds the pins a backend leaks / live pins are the EXPLAIN ANALYZE statements
/// running nested at once, far below it; a dropped pin only loses that statement's check counts, never an answer or a plan.
pub const PRUNE_PINS_MAX: usize = 64;

/// The least share a part of a grant gets (`MemoryGrant::split`'s per-part clamp; `image_cut` leaves it to the part).
/// Derived: PostgreSQL's least work_mem (64 kB), the least budget a node of PostgreSQL's own sorts or hashes in.
/// Held-out (DR-9; shapecheck sweep-DR-9-trunk-caf99850909.tsv, sweep-DR-9-past-chain.tsv: the grouping corpus at 200 k rows
/// with its estimate misses under a 1 MiB grant at W8, floors 16 KiB .. 1 MiB): the time falls with every floor past the
/// grant's own split (16 / 32 / 64 KiB 3507-3718 ms, 128 KiB 3099-3166, 192 KiB 2829, 256 KiB 2663-2716, 512 KiB 2418,
/// 1 MiB 2174) with no knee at a format size, so a larger floor only spends memory past the grant.
pub const SHARE_FLOOR: u32 = 64 << 10;

/// wasm32 (lane wasmjit stage 2, gravity_backend raw.rs `wasm::stack_guard`): generated frames stop this far above the
/// shadow stack's lowest address, so the hosts a generated frame calls and the overflow host's own unwind always have
/// room below the last generated frame. Derived, not measured: 1/64 of the server's 64 MiB shadow stack (wasm-build.sh),
/// inside the ~4 MiB the boot GUC max_stack_depth = 60000 kB leaves unbudgeted, so the hosts' own guard
/// (stack_depth_core) still fires first for Rust recursion above a generated frame.
pub const WASM_STACK_FLOOR_MARGIN: u32 = 1 << 20;

/// D651: the leader region a share past 8 GiB keeps (its u32 cap, page-rounded as the depot maps it). Derived: the
/// range of a u32 share.
pub const LEADER_CAP: u64 = u32::MAX as u64 + 1;

/// The least block a program arena maps instead of allocating (arena.rs `Block`: a mapping's pages are zero and fault in
/// only where the arena is used; a smaller block is the allocator's, zeroed by it).
/// Derived: one 2 MiB huge page (the PMD size under a 4 KiB page granule).
pub const ARENA_MAPPED_MIN: usize = 2 << 20;

/// The least images cut of a share (`image_cut`: an eighth of the share, at least this, never into `SHARE_FLOOR`).
/// Chosen capacity, not measured: it bounds the images part from below / past the share's room the cut is that room.
pub const IMAGES_MIN: u32 = 1 << 20;

/// The grant floor and the statement pool fraction are admission's (the memory policy's `mempolicy::admit`, with their
/// sources); the cost model reads them here.
pub use mempolicy::admit::{GRANT_FLOOR, POOL_FRACTION_DEFAULT, POOL_FRACTION_WHOLE};

/// D2894: the part of a hash breaker's PostgreSQL floor (D2587) charged whatever its model (`grant::charged_floor`); past
/// it a floor is charged up to the modeled need and the rest of it is its region's address space, committed as it fills.
/// Derived: PostgreSQL's default work_mem (4 MB) x its default hash_mem_multiplier (2.0), the hash floor of every node
/// at the default settings: a floor at the defaults is charged whole, as D2587 had it.
pub const FLOOR_CHARGED_MIN: u64 = 8 << 20;

/// GENP-1 (D3004): the plan cost below which a generic plan's parameter-reading scan is not estimated again for its bound
/// values (`gravity_exec` inputs/genp.rs). DERIVED: a sequential scan's cost is about its pages (seq_page_cost 1), and a
/// scan of at most `FLOOR_CHARGED_MIN` bytes of pages runs under the floor every hash breaker is charged whatever its rows,
/// so its estimate cannot move a grant.
pub const GENP_MIN_SCAN_COST: f64 = (FLOOR_CHARGED_MIN / 8192) as f64;
/// GENP-1: the parameter values an admission keeps its re-estimates for (`gravity_exec` inputs/genp.rs `Genp::now`).
/// CHOSEN, not measured: it bounds an admission's cache / past it the oldest values' answer goes.
pub const GENP_CACHE_MAX: usize = 16;

/// The plans a backend keeps admitted at once (`gravity_exec` lib.rs `Backend`: its plan slots, the set-associative plan
/// memo's keys and the program cache's entries; D1789's e2e knob sizes the slots down from it).
/// CHOSEN, not measured: it bounds a backend's admitted plans / past it a new plan displaces a victim's slot.
pub const MAX_PLANS: usize = 256;

/// The executions a backend holds open at once (`gravity_exec` `Backend::execs`: one slot per live portal or execution).
/// CHOSEN, not measured: it bounds a backend's open executions / past it a new one is not served ("no free exec slot").
pub const MAX_EXECS: usize = 16;

/// The (statement, pipeline) group counts `observe` keeps process-wide (`gravity_exec` statement.rs `OBSERVED`).
/// CHOSEN, not measured: it bounds the counts kept / a new key at the bound clears them all first (memo.rs's `GROUPS`
/// rule).
pub const OBSERVED_CAP: usize = 4096;

/// KEYED-22: the rows an open of a dense `table.small` must fold per slot of the span it opens (the open zeroes the span
/// and the finish walks it, once per worker; a hashed open touches only the groups its rows make).
/// MEASURED (agg k22p, 1M-row pgrcolumnar table, spans 4901-14001, a dense open at the span against the hashed one at the
/// groups): 1M rows per open over the span ran 0.76-0.80x; 10000 rows over a 5000 span (2 per slot) 1.00x at W0;
/// 1250 per worker over 5000 (W8) 1.15x, 1000 over 5000 1.02x (W0) / 1.23x (W8), 64 rows 1.12x: the dense open pays from
/// about 2 rows per slot.
pub const DENSE_ROWS_PER_SLOT: u64 = 2;

/// KEYED-22: a dense `table.small` open at the span pays when an open's share of the rows its table folded (`rows` over
/// the pipeline's `width` workers, at least one) covers `DENSE_ROWS_PER_SLOT` per slot of `span`; a span of 0 (no domain) never.
pub const fn dense_span_pays(span: u64, rows: u64, width: u16) -> bool {
    let w = if width == 0 { 1 } else { width as u64 };
    span != 0 && rows / w / DENSE_ROWS_PER_SLOT >= span
}

/// D485: the least table a kept-run form's region starts with past its run.
/// Chosen capacity, not measured: it bounds the form's first table from below / a partition that overflows the table
/// doubles it.
pub const KEPT_TABLE_MIN: usize = 1 << 20;

/// D3902: the most one spill write or read of the pool's spill files moves before the statement's cancel is polled again
/// (`cancelpoll::canceled`). Chosen, not measured: a 1 MiB gathered write is still one syscall, and its 256 pages bound
/// what a cancel waits behind under the cgroup's reclaim, where each page can stall (a whole epoch's write took up to 1.9 s).
pub const SPILL_IO_CHUNK: usize = 1 << 20;

/// D2034: the term rows a streamed recursive union's first pull of a generation asks for; each later pull of it asks twice
/// the last, up to the working table's window (lowerv4 ops/setop/recursive.rs `Scan::streamed`).
/// DERIVED, not measured: PostgreSQL's RecursiveUnion hands one term row per call (nodeRecursiveunion.c), so a first ask of
/// one runs the term no further ahead of a reader that stops at once; the ask moves how far the term runs ahead of its
/// reader, never an answer (a term error past the reader's stop is held, never raised).
pub const STREAM_FIRST_PULL: i64 = 1;

/// The least chunk a host arena grows by (`grow` / `pin_zeroed` ask at least this).
/// Chosen capacity, not measured: it bounds a chunk from below / a larger ask gets a chunk of its own size.
pub const HOST_ARENA_CHUNK: usize = 1 << 16;

/// D608 (debug builds): the chunk a write's per-row context clobber allocates over the rewound window with (hosts
/// ops/modifytable write.rs `clobber_row_context`, PostgreSQL's CLOBBER_FREED_MEMORY for the row's context).
/// CHOSEN, not measured: it bounds one poison chunk / the window is covered while the chunks stay contiguous.
pub const CLOBBER_CHUNK: usize = 1024;

/// The chunk a spool's copies arena (exec ops/material raw.rs, `m8_spool_copy`) grows by; a datum past a quarter of it
/// gets a region of its own.
/// CHOSEN, not measured: it bounds the arena's chunk count for small by-reference cells / a large datum is not packed.
pub const SPOOL_COPY_CHUNK: usize = 64 << 10;

/// D4628 (lowerv4 road spool.seal): the chunk one claim of an owned spool's wide seal copies whole (exec ops/material
/// raw.rs `copy_wide`): a few hundred page first-touches, so a claim's hand-off is lost in its copy.
/// CHOSEN, not measured: it bounds the claims an owner's copy takes / a copy of a few MiB spreads over the pool.
pub const SEAL_COPY_CHUNK: usize = 1 << 20;

/// D4628: the chunks an owner's records hold at least for the wide seal to copy them on the pool; under it the leader copies
/// them (exec ops/material raw.rs `copy_wide`).
/// CHOSEN, not measured: under two chunks the pool's hand-off costs about what the leader's one copy does.
pub const SEAL_COPY_FLOOR: usize = 2;

/// The region depot's smallest size class, as a power of two (`gravity_driver` park.rs: `REGION_MIN` is one shifted by
/// it, and the classes run from there up to 2^40).
/// Derived: one 4 KiB page, the smallest page a target maps.
pub const REGION_MIN_SHIFT: usize = 12;

/// The most columns a worker formats into DataRow bytes itself (`gravity_driver` sink.rs `WireFormat`: its per-column
/// kinds array is this long; `gravity_wire` datarow.rs gives a wider row no wire format).
/// CHOSEN, not measured: it bounds the columns of a row a worker formats / a wider row is formatted by the leader.
pub const MAX_WIRE_ATTS: usize = 64;

/// The relations a relcache invalidation records one by one between two drains of the slot depot (`gravity_driver`
/// slots.rs `RELEASED`: a fixed set the lock-free callback writes into).
/// CHOSEN, not measured: it bounds the relations recorded one by one / past it the record's release-all flag is set and
/// every relation's kept cursors are released, reopening lazily.
pub const RELEASED_CAP: usize = 32;

/// The records of the process-wide log of released relations that every backend's park reads from its own generation
/// (`gravity_driver` slots.rs `ReleaseLog`, D1507).
/// CHOSEN, not measured: it bounds how far behind a park may read the log / a park further behind (or one whose word was
/// overwritten) releases every relation, reopening lazily.
pub const RELEASED_LOG_CAP: usize = 64;

/// P2 W1: the relations a parked Node's entry lists as its sources' own (`gravity_driver` park/index.rs `RelsRead`), so a
/// release record that names none of them passes the Node.
/// CHOSEN, not measured: an OLTP statement's sources read one to three relations / a Node reading more is reached by every
/// record, as every parked Node was before.
pub const PARK_RELS_READ: usize = 3;

/// P2 W1: the slots a park grows before its index (`gravity_driver` park/index.rs) takes over its take, put and eviction
/// from the walks over every slot: a smaller park (a deep statement level's, `DEEP_PARK_NODES`) walks them and holds no
/// index at all.
/// CHOSEN, not measured: a walk over this many slots costs about what the index's hash and probe do.
pub const PARK_INDEX_FROM: usize = 16;

/// The most pools a process installs, one per NUMA node (`gravity_parallel::install_pools`; its per-pool count of
/// statements in flight is an array of this length).
/// CHOSEN, not measured: it bounds the pools a machine installs / a machine with more nodes installs none and runs serial
/// on the leader.
pub const MAX_POOLS: usize = 8;

/// Tier 0's asynchronous promotion (`gravity_backend` promote.rs): the threads compiling a tier-0 miss's module beside its
/// interpreted run. CHOSEN, not measured: one module compiles on one thread, and a miss that finds the pool busy still
/// promotes by compiling inline at its next execution, as every promotion did before the pool.
pub const COMPILE_POOL_THREADS: usize = 1;

/// The compile pool's queue, in modules waiting. CHOSEN, not measured: a memory bound, not a speed knob (each waiting job
/// keeps its tier-0 IR copy alive until compiled); a miss past it is not queued and promotes inline.
pub const COMPILE_QUEUE_MAX: usize = 16;

/// The JIT perf map (`gravity_backend` perfmap.rs, `pgrust.gravity_jit_perfmap`): the lowerings' notes it keeps, the
/// oldest replaced first. CHOSEN, not measured: a note waits for its module's compile, inline in its own admission or on
/// tier 0's pool, where at most `COMPILE_QUEUE_MAX` modules wait and `COMPILE_POOL_THREADS` compile; eight times that
/// covers the backends lowering at the same time besides. A note replaced before its compile costs the names their plan
/// node and tags, never the code.
pub const PERFMAP_NOTES: usize = 8 * (COMPILE_QUEUE_MAX + COMPILE_POOL_THREADS);

/// The pipeline functions a perf map note names the plan node of (a program's first ones past it are named without).
/// CHOSEN, not measured: a fixed slot of the notes' static ring (no allocation); the largest programs measured lower to
/// far fewer pipeline functions.
pub const PERFMAP_NOTE_FUNCS: usize = 96;

/// The bytes one process writes to the perf map and its sidecar, at most. CHOSEN, not measured: a bound on what a
/// diagnostic leaves in /tmp on a server that keeps compiling (evictions compile again), not a speed knob.
pub const PERFMAP_WRITE_BYTES: u64 = 256 << 20;

/// D3811's fault injection (`gravity_backend` TIER0_HOLD_ENV, read once at server start): the most ms the compile pool
/// holds a module's key before it compiles. CHOSEN, not measured: a bound on a test lever (the e2e witness holds 4 s, the
/// fast-shutdown check 20 s), so no setting stalls the pool's compiles for longer than a minute.
pub const TIER0_HOLD_MAX_MS: u64 = 60_000;

/// The most sorted runs one k-way merge takes (`gravity_parallel::kway_merge`: its heap and run cursors are arrays of
/// this length).
/// CHOSEN, not measured: it bounds the runs of one merge / more runs are refused by its assert.
pub const KWAY_MERGE_RUNS: usize = 64;

/// The buffer the pre-warm worker reads the process's mapped files through (`gravity_prewarm` lib.rs `warm_image`: every
/// file the process maps, read through once).
/// CHOSEN, not measured: it bounds the worker's one read buffer / a larger file is read in more chunks.
pub const PREWARM_IMAGE_CHUNK: usize = 1 << 20;

/// The share of the machine's RAM the process-shared code cache keeps compiled modules in (`gravity_backend` cache.rs
/// `budget_bytes`, gap-optoverfit KN-07): `code_cache_bytes`. CHOSEN, not measured: 1/512 keeps the 64 MiB the cache had on
/// the 32 GiB reference box (`gravity_driver::machine::REFERENCE_RAM`) and grows with the machine; it bounds the compiled
/// code a process keeps / past it the least recently used module is evicted, and a later caller compiles it again (an
/// evicted module lives on in every caller that holds it).
pub const CODE_CACHE_RAM_DIV: u64 = 512;

/// The code cache's budget on a machine of `ram` bytes: its `CODE_CACHE_RAM_DIV` share, never under the reference box's
/// (`reference_ram`): a smaller machine keeps the reference's budget, so no machine compiles again what the reference keeps.
pub const fn code_cache_bytes(ram: u64, reference_ram: u64) -> u64 {
    (if ram > reference_ram { ram } else { reference_ram }) / CODE_CACHE_RAM_DIV
}

/// D3203: the spilled bytes from which a statement's end deletes its spill files across the pool's idle workers
/// (driver mem/spill.rs `delete_across`) instead of on the leader alone. A floor, not a tuned value: under it the leader's
/// unlinks (the kernel freeing the files' page cache) take about a millisecond, the order of a pool section's own open and
/// wake; past it the leader's share falls as 1 / W.
pub const END_FAN_OUT_BYTES: u64 = 8 << 20;

/// The least writable extent a dropped mapped reservation goes to the reaper thread with (`gravity_driver` mem/reap.rs
/// `defer`, `RegionBuf`'s `rw`): under it the dropping thread unmaps it itself. A floor, not a tuned value: two 2 MiB
/// huge pages; under it the unmap tears down at most a few page tables, the order of the hand-off's own lock and wake.
pub const REAP_MIN_BYTES: usize = 4 << 20;

/// The most dropped reservations waiting for the reaper (`gravity_driver` mem/reap.rs: both of its buffers are this long,
/// so the queue never allocates once the thread runs). CHOSEN, not measured: it bounds the reaper's buffers / past it the
/// dropping thread unmaps its own, as it did before the reaper.
pub const REAP_QUEUE_MAX: usize = 1024;

/// The most regions waiting for the give-back service to give their committed pages back before they park
/// (`gravity_driver` mem/giveback.rs, H3 step (4) C1-c: a fixed ring, so a put never allocates). CHOSEN, not measured: a
/// statement's end parks a few regions per worker, and only those that committed in place make a job; past it the putting
/// thread gives them back as before.
pub const PARK_QUEUE_MAX: usize = 128;

#[cfg(test)]
mod tests {
    use super::code_cache_bytes;

    #[test]
    fn the_code_cache_is_a_share_of_ram_never_under_the_reference() {
        const REF: u64 = 32 << 30;
        assert_eq!(code_cache_bytes(REF, REF), 64 << 20, "the reference box keeps the 64 MiB it had");
        assert_eq!(code_cache_bytes(128 << 30, REF), 256 << 20);
        assert_eq!(code_cache_bytes(4 << 30, REF), 64 << 20, "a smaller machine keeps the reference's budget");
    }

    #[test]
    fn a_dense_span_pays_from_two_rows_per_slot_per_worker() {
        use super::dense_span_pays;
        assert!(dense_span_pays(5000, 1_000_000, 0), "1M rows over 5000 slots, serial");
        assert!(dense_span_pays(5000, 10_000, 1), "2 rows a slot, the k22p neutral point");
        assert!(!dense_span_pays(5000, 9_999, 1));
        assert!(!dense_span_pays(5000, 10_000, 8), "1250 rows a worker over 5000 slots");
        assert!(dense_span_pays(14001, 1_000_000, 8), "125000 a worker over 14001");
        assert!(!dense_span_pays(5000, 1000, 1), "a filter left 1000 rows");
        assert!(!dense_span_pays(0, u64::MAX, 1), "no domain");
    }
}
