//! M8's host entries (M8-plan.md §1.4 / §6, cards/M8.md L1 / L2): the C-ABI functions M8's lowered IR calls for nests
//! and subqueries. The runs: an InitPlan's range at its first read (`m8_init_run`, its by-reference results copied by
//! `m8_init_copy`), a rerun set per param change
//! (`m8_rerun`; its InitPlans re-armed ahead of a gated re-open by raw.rs `RerunRearm`), a CTE producer resumed for more
//! rows (`m8_cte_pull`); the stores: a Memoize miss (`m8_memo_miss`), the
//! hashed SubPlan's set build (`m8_subhash_insert` / `m8_subhash_seal` / `m8_subhash_reset`, a LAZY set's at its first
//! probe by `m8_subhash_run`; the probe stays IR), a by-reference EXPR result's copy (`m8_sub_copy`), the ARRAY sublink's
//! accumulator (`m8_array_begin` / `_push` / `_end`), a spool region's growth from the grant (`m8_spool_grow`) and its
//! by-reference copies (`m8_spool_copy`); and
//! PostgreSQL's subquery errors an M8 end raises in IR (`m8_raise`).
//!
//! The handle (host_abi/v4/m5.rs's store-head pattern: this crate cannot name the exec or hosts crates): every entry's
//! second argument `h` is the address of one runtime object of this execution, laid by its protocol (exec v4/m8.rs) into
//! the object's kernel word once per execution, before any pipeline runs; the IR loads the word and passes it. The object
//! starts with a `Head`: `magic` names its kind while it lives (a null, stale or foreign handle is an error by name, never
//! a wild call) and `op` is its one runtime function, called with the entry's operation code and arguments. A status
//! below zero is PostgreSQL's error, stashed (`ERROR`), or a status the driver holds.
//!
//! Every entry runs its body under the landing pad (`contract::host!`'s `#[guard]`): a bug's panic in the object's `op`
//! never crosses the `extern "C"` frame. The entries and the `Head` live in raw.rs (rules R6 / R8); their paths here are
//! unchanged. Each entry is listed in the `hosts:` rows of the lowerv4 file that calls it.
pub use super::raw::{Head, m8_array_begin, m8_array_end, m8_array_push, m8_cte_pull, m8_init_copy, m8_init_epq, m8_init_run, m8_memo_done, m8_memo_miss, m8_memo_store, m8_raise, m8_rerun, m8_spool_copy, m8_spool_grow, m8_sub_copy, m8_subhash_exact, m8_subhash_insert, m8_subhash_probe, m8_subhash_reset, m8_subhash_run, m8_subhash_seal};

/// `nest.initplan`: an `InitCell` (the Init proto's object).
pub const INIT_MAGIC: u64 = 0x8a11_7e11_0000_0001;
/// `nest.rerun` / a nested `nest.subplan`: a rerun set.
pub const RERUN_MAGIC: u64 = 0x8a11_7e11_0000_0002;
/// `nest.cte`: a CTE's shared spool and its producer.
pub const CTE_MAGIC: u64 = 0x8a11_7e11_0000_0003;
/// `nest.memo`: a Memoize table.
pub const MEMO_MAGIC: u64 = 0x8a11_7e11_0000_0004;
/// `nest.subplan`, hashed: a key set.
pub const SUBHASH_MAGIC: u64 = 0x8a11_7e11_0000_0005;
/// The IR's load of a hashed set's sealed `KeyTable` header (RT-2a): the word after the `Head`, null until the build's
/// serial end seals the set (and again after a reset); a probe at any width reads it with no host call.
pub const SUBHASH_TABLE_AT: i32 = core::mem::size_of::<Head>() as i32;
/// The SubHash proto's `arg` bit of a LAZY set (V4R-369): its build runs at the first probe (`m8_subhash_run`), as PG's
/// `buildSubPlanHash`, not in program order (a set whose build may raise or holds a VOLATILE call). The program crate's
/// constant of the same name is the lowering's (this crate names no crate past the ABI); exec asserts the two agree.
pub const SUBHASH_LAZY: u64 = 1 << 16;
/// The SubHash proto's `arg` bit of an `unknownEqFalse` SubPlan's set: as PG's `buildSubPlanHash` without `hashnulls`, a
/// build row holding a NULL key is dropped, so a set of only such rows is empty (FALSE before the left side is evaluated).
pub const SUBHASH_NO_NULLS: u64 = 1 << 17;
/// The SubHash proto's `arg` bit of a LAZY set the lowering could have built before its reader, kept for its first probe
/// because that probe may never run: its one reader is the proto's `peer`, which keeps its width (no D1035 pin). At that
/// reader's start exec runs the build ahead when the reader runs on workers, and leaves it to the first probe on the leader.
pub const SUBHASH_AHEAD: u64 = 1 << 18;
/// D3674: the SubHash proto's `arg` bit of a set over more than eight key words (the wide form, exec subplan/multi.rs):
/// ncols in bits 0-7 (9 to `SUBHASH_WIDE_MAX`), from bit 24 two bits per word (log2 of its width), no byte key.
pub const SUBHASH_WIDE: u64 = 1 << 19;
/// The SubHash proto's `arg` bit where the key words' widths start: two bits per word (log2 of its width), word 0 lowest.
pub const SUBHASH_WIDTHS_AT: u32 = 24;
/// D3674: the most key words the wide form lays, the layout's own bound: two width bits each from `SUBHASH_WIDTHS_AT` fill
/// the `arg` word.
pub const SUBHASH_WIDE_MAX: usize = (u64::BITS - SUBHASH_WIDTHS_AT) as usize / 2;
/// A SubPlan's per-call result object (`ProtoKind::SubResult`): an ARRAY sublink's accumulator, a by-reference EXPR's copy.
pub const ARRAY_MAGIC: u64 = 0x8a11_7e11_0000_0006;
/// `nest.spool`: a spool run's region.
pub const SPOOL_MAGIC: u64 = 0x8a11_7e11_0000_0007;

/// The operation codes `Head::op` receives, one per entry.
pub const OP_INIT_RUN: u64 = 1;
pub const OP_RERUN: u64 = 2;
pub const OP_CTE_PULL: u64 = 3;
pub const OP_MEMO_MISS: u64 = 4;
pub const OP_SUBHASH_INSERT: u64 = 5;
pub const OP_SUBHASH_SEAL: u64 = 6;
pub const OP_ARRAY_BEGIN: u64 = 7;
pub const OP_ARRAY_PUSH: u64 = 8;
pub const OP_ARRAY_END: u64 = 9;
pub const OP_SPOOL_GROW: u64 = 10;
pub const OP_INIT_COPY: u64 = 11;
pub const OP_SPOOL_COPY: u64 = 12;
pub const OP_SUBHASH_RESET: u64 = 13;
pub const OP_SUB_COPY: u64 = 14;
pub const OP_SUBHASH_RUN: u64 = 15;
// 16 and 17 are nest.memo's
pub const OP_SUBHASH_PROBE: u64 = 18;
/// D2611: raw.rs `RerunRearm`'s code on a rerun set's object.
pub const OP_RERUN_REARM: u64 = 19;
/// D2034: raw.rs `RerunPull`'s code on a rerun set's object (a recursive union's streamed term).
pub const OP_RERUN_PULL: u64 = 20;
/// D3272: `m8_init_epq`'s code on an Init cell (an IR recheck arming it afresh, then putting it back).
pub const OP_INIT_EPQ: u64 = 21;
/// D2849: `m8_subhash_exact`'s code on a hashed set's object.
pub const OP_SUBHASH_EXACT: u64 = 22;
/// D3274: `m8_init_epq`'s operand: bits 0..8 the operation (`INIT_EPQ_BACK`, `INIT_EPQ_ARM`, `INIT_EPQ_END`), bits 8.. the
/// recheck's EvalPlanQual state key (`INIT_EPQ_KEY_SHIFT`; 0: D3272's, one state for the whole execution).
pub const INIT_EPQ_BACK: u64 = 0;
/// D3274: arm the cell for the recheck.
pub const INIT_EPQ_ARM: u64 = 1;
/// D3274: the keyed state ends (PostgreSQL's EvalPlanQualEnd): its next recheck arms the cell afresh.
pub const INIT_EPQ_END: u64 = 2;
/// D3274: the shift of the state key in `m8_init_epq`'s operand.
pub const INIT_EPQ_KEY_SHIFT: u32 = 8;

/// The Init proto's object (PG's `ParamExecData` of the InitPlan's `setParam`s, once per execution): the head, the
/// run's state (the runtime's, under its once-lock: never tested by IR), the pair count, then `n` pairs at
/// `init_pair(k)`: the value word, then the null word (an i64, 0 or 1). The InitPlan's end writes the pairs through the
/// handle (a by-reference value copied at the row by `m8_init_copy`). A reader calls `m8_init_run` before its thread's
/// first read (its per-thread frame pair, cards/M8.md L0-b / L0-c), which returns the pair's address once the values
/// are set and visible to the calling thread; from then on plain loads (never written again in the execution). No IR
/// load reads the cell to decide whether it ran.
#[repr(C)]
pub struct InitCell {
    pub head: Head,
    pub state: u32,
    pub n: u32,
}

/// D1035 step 2 (V4R-444): bit 30 of an Init proto's `arg` (whose low word is the pair count, 1 to `INIT_MAX_PAIRS`): the InitPlan is
/// parallel-safe and not VOLATILE (PostgreSQL's SubPlan.parallel_safe, F5's volatility), so lowerv4 may leave its one
/// reader (the proto's `peer`) eligible and the runtime runs the members at that reader's start when it runs at width >= 2,
/// as PostgreSQL's Gather evaluates its `initParam` before launching workers.
pub const INIT_EAGER: u64 = 1 << 30;

/// D2028 / D2112 phase B: bit 31 of an Init proto's `arg`: the InitPlan is attached at the statement's write or row lock (or
/// above it) and that node's subplan reads it (decompose `PipelineGraph::recheck_inits`), so a recheck forces it when it is
/// still pending, as PostgreSQL's EvalPlanQualStart / EvalPlanQualBegin do (ExecSetParamPlanMulti over the subplan's
/// `extParam`), before the recheck runs (exec rerun/epq.rs).
pub const INIT_RECHECK: u64 = 1 << 31;

/// D2673: bit 29 of an Init proto's `arg`: the InitPlan is attached below the statement's write or row lock (inside a
/// SubPlan's plan: decompose `PipelineGraph::epq_fresh`), so PostgreSQL's EvalPlanQualStart arms it afresh in the recheck's
/// EState: the recheck that first reads it runs it again, and the statement's run keeps its own value (exec rerun/epq.rs).
pub const INIT_EPQ_FRESH: u64 = 1 << 29;

pub use gravity_abi::limits::INIT_EXISTS;

/// An Init proto's pair count (its `arg`'s low word without `INIT_EAGER` / `INIT_RECHECK` / `INIT_EPQ_FRESH` /
/// `INIT_EXISTS`): every reader of the arg decodes through these.
pub const fn init_pairs(arg: u64) -> u32 {
    (arg & !(INIT_EAGER | INIT_RECHECK | INIT_EPQ_FRESH | INIT_EXISTS)) as u32
}

/// An Init proto's first exec param (its `arg`'s high word).
pub const fn init_first(arg: u64) -> i32 {
    (arg >> 32) as u32 as i32
}

/// Whether exec param `x` is one of the Init proto's pairs (`init_first .. init_first + init_pairs`).
pub const fn init_holds(arg: u64, x: i32) -> bool {
    let k = x as i64 - init_first(arg) as i64;
    k >= 0 && k < init_pairs(arg) as i64
}

/// The offset of pair `k` (its value word; the null word follows at +8) in an `InitCell`.
pub const fn init_pair(k: usize) -> usize {
    core::mem::size_of::<InitCell>() + 16 * k
}

/// `nest.spool`'s object: the head, then the region's first record (`base`, null before the first grow), its capacity
/// in records (`cap`) and one record's bytes (`rec`, the proto's `arg`). The IR appends record `n` at `base + n * rec`
/// while `n < cap`, else calls `m8_spool_grow` and reads `base` and `cap` again: a grow may move the region, so no
/// pointer into it outlives the next append (readers address records by index and read `base` again after any call that
/// can fill). A record's by-reference datum points into `m8_spool_copy`'s arena, never into the region.
#[repr(C)]
pub struct SpoolCell {
    pub head: Head,
    pub base: u64,
    pub cap: u64,
    pub rec: u64,
}

/// The IR's loads of a `SpoolCell`: its region's first record and its capacity in records.
pub const SPOOL_BASE: usize = core::mem::offset_of!(SpoolCell, base);
pub const SPOOL_CAP: usize = core::mem::offset_of!(SpoolCell, cap);

/// D3040: bit 40 of a Spool proto's `arg` (the record's bytes below it): an OWNED spool, filled by a parallel pipeline. Each
/// owner (`Ctx::worker_ordinal`: the section's workers, or the leader alone at width 1; never two at once on one ordinal)
/// appends to its own spool cell in its slot (`OwnedSlot`), its count in the slot too (the execution's parameter words
/// are read-only to workers); after the fill pipeline the leader seals them (exec ops/material/raw.rs `seal`): every
/// owner's records copied, in ordinal order, into the merged cell's region and `total` set. The handle word names the
/// merged cell (the struct's first field), so a reader reads records as from any spool and takes `total` as the count.
pub const SPOOL_OWNED: u64 = 1 << 40;

/// D4628 (lowerv4 road `spool.seal`): bit 41 of an owned spool's proto `arg`: exec seals it off the leader's copy. One owner
/// holding every record lends its region to the merged cell (no copy); records from several owners are copied into the
/// merged region by the pool's threads, each a disjoint byte range. The records and the readers' view are as `SPOOL_OWNED`'s.
pub const SPOOL_SEAL_WIDE: u64 = 1 << 41;

/// One owner's slot of an `OwnedSpools`: its spool cell's handle (a `SpoolCell`'s address) and its record count; a cache
/// line each, so two owners' counts never share one.
#[repr(C)]
pub struct OwnedSlot {
    pub cell: u64,
    pub n: u64,
    pub pad: [u64; 6],
}

/// D3040: an owned spool's object (`SPOOL_OWNED`): the merged cell the readers read, the sealed record count, then one slot
/// per owner ordinal.
#[repr(C)]
pub struct OwnedSpools {
    pub cell: SpoolCell,
    pub total: u64,
    pub pad: [u64; 2],
    pub slots: [OwnedSlot; gravity_abi::breaker::MAX_OWNERS],
}

/// The IR's loads of an `OwnedSpools`: the sealed count, the first slot, a slot's bytes and its fields.
pub const OWNED_TOTAL: usize = core::mem::offset_of!(OwnedSpools, total);
pub const OWNED_SLOTS: usize = core::mem::offset_of!(OwnedSpools, slots);
pub const OWNED_SLOT_BYTES: usize = core::mem::size_of::<OwnedSlot>();
pub const OWNED_SLOT_CELL: usize = core::mem::offset_of!(OwnedSlot, cell);
pub const OWNED_SLOT_N: usize = core::mem::offset_of!(OwnedSlot, n);

/// `m8_raise` code 0 (PG `ExecSetParamPlan` / `ExecScanSubPlan`): a second row from an EXPR / ROWCOMPARE / MULTIEXPR
/// subquery. Codes from 1 are reserved for B2's SubPlan errors.
pub const RAISE_CARDINALITY: u64 = 0;

/// `nest.memo`: `m8_memo_miss` is the lookup; a miss's rows are stored as the inner hands them on (`m8_memo_store`) and
/// `m8_memo_done` completes the entry at the inner's end. Key block (`key`): the key count, the flags (`MEMO_BINARY`,
/// `MEMO_SINGLEROW`, `MEMO_PURGE`), then per key the datum word, NULL, the by-reference image's bytes (0: by value),
/// the hash equality operator, the collation, the typlen. Row block (`row`): the column count, then per column the
/// datum word, NULL, the image's bytes. A hit: the row count, the column count, then per row per column the word and
/// NULL.
pub const OP_MEMO_STORE: u64 = 16;
pub const OP_MEMO_DONE: u64 = 17;
pub const MEMO_KEY_HEAD: usize = 2;
pub const MEMO_KEY_WORDS: usize = 6;
pub const MEMO_BINARY: u64 = 1;
pub const MEMO_SINGLEROW: u64 = 2;
/// The first lookup after a re-run plan re-opened the memo's nest (ExecReScanMemoize's `cache_purge_all`, a param outside
/// the keys changed): every entry is dropped before the lookup.
pub const MEMO_PURGE: u64 = 4;
pub const MEMO_ROW_HEAD: usize = 1;
pub const MEMO_ROW_WORDS: usize = 3;
pub const MEMO_HIT_HEAD: usize = 2;

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_abi::status::ERROR;
    use gravity_abi::Ctx;
    use crate::contract::sqlstate;
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    // only `each_entry_runs_its_objects_op_...` runs `record` (the foreign-handle test never reaches it); no thread-local (D375)
    static SEEN: [AtomicU64; 3] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];

    unsafe fn record(_h: *mut Head, _ctx: *mut Ctx, code: u64, a: u64, b: u64) -> i64 {
        SEEN.iter().zip([code, a, b]).for_each(|(s, v)| s.store(v, Relaxed));
        (code * 100 + a + b) as i64
    }

    unsafe fn panics(_h: *mut Head, _ctx: *mut Ctx, _code: u64, _a: u64, _b: u64) -> i64 {
        panic!("m8 object bug")
    }

    fn head(magic: u64, op: unsafe fn(*mut Head, *mut Ctx, u64, u64, u64) -> i64) -> Box<Head> {
        Box::new(Head { magic, op }) // alloc: Cold
    }

    fn message() -> String {
        gravity_fn::take_error().map(|e| e.message().to_owned()).unwrap_or_default()
    }

    /// D2112 phase B: the recheck bit, beside the eager one, is no pair: every reader decodes the same pairs and first param.
    #[test]
    fn d2112_a_recheck_flagged_arg_decodes_its_own_pairs() {
        let arg = 2 | INIT_RECHECK | INIT_EAGER | 9u64 << 32;
        assert_eq!((init_pairs(arg), init_first(arg)), (2, 9));
        assert!(init_holds(arg, 9) && init_holds(arg, 10) && !init_holds(arg, 11));
    }

    /// D2673: the fresh bit, beside the others, is no pair either.
    #[test]
    fn d2673_an_epq_fresh_flagged_arg_decodes_its_own_pairs() {
        let arg = 3 | INIT_EPQ_FRESH | INIT_RECHECK | INIT_EAGER | 4u64 << 32;
        assert_eq!((init_pairs(arg), init_first(arg)), (3, 4));
        assert!(init_holds(arg, 6) && !init_holds(arg, 7));
    }

    #[test]
    fn each_entry_runs_its_objects_op_with_its_code_and_arguments() {
        let ctx: *mut Ctx = core::ptr::null_mut();
        let key = [7u64, 0];
        let h = |magic| Box::into_raw(head(magic, record)) as u64;
        let (init, rerun, cte, memo, set, acc, spool) = (h(INIT_MAGIC), h(RERUN_MAGIC), h(CTE_MAGIC), h(MEMO_MAGIC), h(SUBHASH_MAGIC), h(ARRAY_MAGIC), h(SPOOL_MAGIC));
        let kp = key.as_ptr() as u64;
        // SAFETY: every handle is a live `Head` whose op reads no pointer.
        let cases = unsafe {
            [
                (m8_init_run(ctx, init, 3), (OP_INIT_RUN, 3, 0)),
                (m8_init_copy(ctx, init, key.as_ptr() as *const u8, 16), (OP_INIT_COPY, kp, 16)),
                (m8_rerun(ctx as u64, rerun), (OP_RERUN, 0, 0)),
                (m8_cte_pull(ctx as u64, cte, 64), (OP_CTE_PULL, 64, 0)),
                (m8_memo_miss(ctx, memo, key.as_ptr()), (OP_MEMO_MISS, kp, 0)),
                (m8_subhash_insert(ctx, set, key.as_ptr()), (OP_SUBHASH_INSERT, kp, 0)),
                (m8_subhash_seal(ctx, set), (OP_SUBHASH_SEAL, 0, 0)),
                (m8_subhash_reset(ctx, set), (OP_SUBHASH_RESET, 0, 0)),
                (m8_subhash_run(ctx, set), (OP_SUBHASH_RUN, 0, 0)),
                (m8_subhash_probe(ctx, set, key.as_ptr()), (OP_SUBHASH_PROBE, kp, 0)),
                (m8_subhash_exact(ctx, set), (OP_SUBHASH_EXACT, 0, 0)),
                (m8_sub_copy(ctx as u64, acc, (key.as_ptr() as *const u8) as u64, 16), (OP_SUB_COPY, kp, 16)),
                (m8_array_begin(ctx, acc), (OP_ARRAY_BEGIN, 0, 0)),
                (m8_array_push(ctx, acc, 42, 1), (OP_ARRAY_PUSH, 42, 1)),
                (m8_array_end(ctx, acc), (OP_ARRAY_END, 0, 0)),
                (m8_spool_grow(ctx as u64, spool), (OP_SPOOL_GROW, 0, 0)),
                (m8_spool_copy(ctx, spool, key.as_ptr() as *const u8, 16), (OP_SPOOL_COPY, kp, 16)),
            ]
        };
        for (i, (got, (code, a, b))) in cases.into_iter().enumerate() {
            assert_eq!(got, (code * 100 + a + b) as i64, "case {i}");
        }
        // SAFETY: as above.
        unsafe { m8_array_push(ctx, acc, 5, 0) };
        assert_eq!(SEEN.each_ref().map(|s| s.load(Relaxed)), [OP_ARRAY_PUSH, 5, 0]);
        for p in [init, rerun, cte, memo, set, acc, spool] {
            // SAFETY: each came from `Box::into_raw` above.
            drop(unsafe { Box::from_raw(p as *mut Head) });
        }
    }

    #[test]
    fn a_null_or_foreign_handle_is_an_error_by_the_entrys_name() {
        let ctx: *mut Ctx = core::ptr::null_mut();
        // SAFETY: a null handle is never dereferenced.
        assert_eq!(unsafe { m8_init_run(ctx, 0, 1) }, ERROR);
        assert_eq!(message(), "gravity: m8_init_run: a stale or foreign M8 handle 0x0");
        let memo = head(MEMO_MAGIC, record);
        let h = &*memo as *const Head as u64;
        // SAFETY: a live head of another kind: only its magic is read.
        assert_eq!(unsafe { m8_rerun(ctx as u64, h) }, ERROR);
        assert!(message().starts_with("gravity: m8_rerun: a stale or foreign M8 handle 0x"));
    }

    #[test]
    fn an_objects_panic_is_parked_and_the_entry_returns_error() {
        let ctx: *mut Ctx = core::ptr::null_mut();
        let h = head(INIT_MAGIC, panics);
        // SAFETY: a live head.
        let r = unsafe { m8_init_run(ctx, &*h as *const Head as u64, 1) };
        assert_eq!(r, ERROR);
        let resumed = std::panic::catch_unwind(gravity_abi::status::unwind::resume);
        let payload = resumed.expect_err("the parked panic resumes at the caller");
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"m8 object bug"));
    }

    #[test]
    fn m8_raise_stashes_the_cardinality_violation_and_names_an_unknown_code() {
        let ctx: *mut Ctx = core::ptr::null_mut();
        // SAFETY: the entry reads no argument but the code.
        assert_eq!(unsafe { m8_raise(ctx as u64, RAISE_CARDINALITY) }, ERROR);
        let e = gravity_fn::take_error().expect("the stashed error");
        assert_eq!(e.message(), "more than one row returned by a subquery used as an expression");
        assert_eq!(e.sqlstate().0, sqlstate(*b"21000"));
        // SAFETY: as above.
        assert_eq!(unsafe { m8_raise(ctx as u64, 99) }, ERROR);
        assert_eq!(message(), "gravity: m8_raise: no error for code 99");
    }

    #[test]
    fn the_init_cells_pairs_follow_its_head_sixteen_bytes_each() {
        assert_eq!(init_pair(0), 24);
        assert_eq!(init_pair(2), 24 + 32);
    }

    #[test]
    fn the_spool_cells_words_follow_its_head() {
        assert_eq!((SPOOL_BASE, SPOOL_CAP, core::mem::size_of::<SpoolCell>()), (16, 24, 40));
    }
}
