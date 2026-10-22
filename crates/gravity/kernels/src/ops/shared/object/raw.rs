//! subplan's raw half (rules R6 / R8: the family's unsafe code and its host entries live here): the subquery runtime
//! objects' entries the lowered IR calls, through the contract's landing pad: `contract::entryN::<H>` (`RerunRearm`) or
//! `contract::host!`'s `#[guard(ERROR)]` (the C-ABI entries below, the object `Head` they call through and their `call`).
//! entries.rs re-exports them under its paths, so the lowerers' addresses and the IR's host names stay as they were.
use gravity_abi::status::ERROR;
use gravity_abi::Ctx;

use super::entries::{ARRAY_MAGIC, CTE_MAGIC, INIT_MAGIC, MEMO_MAGIC, OP_ARRAY_BEGIN, OP_ARRAY_END, OP_ARRAY_PUSH, OP_CTE_PULL, OP_INIT_COPY, OP_INIT_EPQ, OP_INIT_RUN, OP_MEMO_DONE, OP_MEMO_MISS, OP_MEMO_STORE, OP_RERUN, OP_RERUN_PULL, OP_RERUN_REARM, OP_SPOOL_COPY, OP_SPOOL_GROW, OP_SUBHASH_EXACT, OP_SUBHASH_INSERT, OP_SUBHASH_PROBE, OP_SUBHASH_RESET, OP_SUBHASH_RUN, OP_SUBHASH_SEAL, OP_SUB_COPY, RAISE_CARDINALITY, RERUN_MAGIC, SPOOL_MAGIC, SUBHASH_MAGIC};
use crate::contract::Host;

/// The first field of every subquery runtime object a handle word points at.
#[repr(C)]
pub struct Head {
    /// The object's kind (`*_MAGIC`) while it lives; anything else is a stale or foreign handle.
    pub magic: u64,
    /// The object's runtime: `(h, ctx, code, a, b)` with the entry's `OP_*` code and its two argument words.
    pub op: unsafe fn(h: *mut Head, ctx: *mut Ctx, code: u64, a: u64, b: u64) -> i64,
}

/// Object `h` of kind `magic` runs operation `code`; a null or foreign handle is an error by `entry`'s name.
#[inline(always)]
unsafe fn call(ctx: *mut Ctx, h: u64, magic: u64, code: u64, a: u64, b: u64, entry: &str) -> i64 {
    let head = h as *mut Head;
    if head.is_null() || (*head).magic != magic {
        return stale(entry, h);
    }
    ((*head).op)(head, ctx, code, a, b)
}

#[cold]
#[inline(never)]
fn stale(entry: &str, h: u64) -> i64 {
    crate::host_abi::bare(&format!("{entry}: a stale or foreign M8 handle {h:#x}")) // alloc: Cold
}

#[cold]
#[inline(never)]
fn raise(code: u64) -> i64 {
    let (msg, state) = match code {
        RAISE_CARDINALITY => ("more than one row returned by a subquery used as an expression", *b"21000"),
        _ => return crate::host_abi::bare(&format!("m8_raise: no error for code {code}")), // alloc: Cold
    };
    crate::contract::raise(msg, None, state)
}

crate::contract::host! {
    /// `nest.initplan` (PG `ExecSetParamPlan`): Init cell `h`'s range runs once for this execution (the first reader runs it
    /// on its thread; a concurrent reader waits on the once-lock), its end writing the pairs. The address of exec param
    /// `paramid`'s pair once the values are set and visible to the calling thread, else a status below zero.
    #[guard(ERROR)]
    pub unsafe fn m8_init_run(ctx: *mut Ctx, h: u64, paramid: u64) -> i64 {
        call(ctx, h, INIT_MAGIC, OP_INIT_RUN, paramid, 0, "m8_init_run")
    }

    /// `nest.initplan`'s end: `len` bytes at `ptr` (a by-reference result: a varlena image as it is, short header, external
    /// pointer or compressed image alike, or a fixed-length value) copied verbatim into Init cell `h`'s arena, which lives for
    /// the execution. The copy's address, else a status below zero.
    #[guard(ERROR)]
    pub unsafe fn m8_init_copy(ctx: *mut Ctx, h: u64, ptr: *const u8, len: u64) -> i64 {
        call(ctx, h, INIT_MAGIC, OP_INIT_COPY, ptr as u64, len, "m8_init_copy")
    }

    /// D3272 (an EvalPlanQual recheck in the IR): Init cell `h` armed for the recheck (`arm` nonzero: a done cell's next
    /// read runs its members again, as EvalPlanQualStart initializes the SubPlan holding the InitPlan afresh) or put back
    /// after it (`arm` zero: the statement's own pairs and state again; no arming kept, nothing). D3274: `arm`'s bits 8..
    /// name the recheck's EvalPlanQual state (entries.rs `INIT_EPQ_KEY_SHIFT`), and `INIT_EPQ_END` ends that state. 0,
    /// else a status below zero.
    #[guard(ERROR)]
    pub unsafe fn m8_init_epq(ctx: *mut Ctx, h: u64, arm: u64) -> i64 {
        call(ctx, h, INIT_MAGIC, OP_INIT_EPQ, arm, 0, "m8_init_epq")
    }

    /// A subquery end's error raised in IR (its `l_raise` with `raise::bit(HOST)` after this call): PostgreSQL's error for `code`
    /// (`RAISE_*`) stashed; always a status below zero. Cold path only.
    #[guard(ERROR)]
    pub unsafe fn m8_raise(_ctx: u64, code: u64) -> i64 {
        raise(code)
    }

    /// `nest.rerun` / a nested `nest.subplan` (PG `ExecReScan` with `chgParam`): rerun set `h`'s pipeline range runs again on
    /// the calling thread over the param words the caller wrote. 0 when it ran, else a status below zero.
    #[guard(ERROR)]
    pub unsafe fn m8_rerun(ctx: u64, h: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let ctx = ctx as *mut Ctx;
        call(ctx, h, RERUN_MAGIC, OP_RERUN, 0, 0, "m8_rerun")
    }

    /// `nest.cte` (PG `CteScanNext`): CTE `h`'s producer resumes for at most `n` more rows into its shared spool. The spool's
    /// stored row count after the pull (the count before it once the producer is done), else a status below zero.
    #[guard(ERROR)]
    pub unsafe fn m8_cte_pull(ctx: u64, h: u64, n: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let ctx = ctx as *mut Ctx;
        call(ctx, h, CTE_MAGIC, OP_CTE_PULL, n, 0, "m8_cte_pull")
    }

    /// `nest.memo` (PG `cache_lookup`): memo `h`'s complete entry for the key block at `key`: its rows' address, else 0 (the
    /// inner runs, filling it), least-recently-used entries evicted under `get_hash_memory_limit`; below zero on an error.
    #[guard(ERROR)]
    pub unsafe fn m8_memo_miss(ctx: *mut Ctx, h: u64, key: *const u64) -> i64 {
        call(ctx, h, MEMO_MAGIC, OP_MEMO_MISS, key as u64, 0, "m8_memo_miss")
    }

    /// `nest.subplan`, hashed (PG `buildSubPlanHash`): the key row at `row` (the set's declared key words and null bytes)
    /// added to set `h`; a row with a NULL key goes to the set's null side. 0, else a status below zero.
    #[guard(ERROR)]
    pub unsafe fn m8_subhash_insert(ctx: *mut Ctx, h: u64, row: *const u64) -> i64 {
        call(ctx, h, SUBHASH_MAGIC, OP_SUBHASH_INSERT, row as u64, 0, "m8_subhash_insert")
    }

    /// `nest.subplan`, hashed: set `h` complete; the address of the table header the IR probes, else a status below zero.
    #[guard(ERROR)]
    pub unsafe fn m8_subhash_seal(ctx: *mut Ctx, h: u64) -> i64 {
        call(ctx, h, SUBHASH_MAGIC, OP_SUBHASH_SEAL, 0, 0, "m8_subhash_seal")
    }

    /// `nest.subplan`, hashed: set `h` emptied for a rebuild (PG `buildSubPlanHash` after a rescan changed the SubPlan's
    /// params: the rerun path). 0, else a status below zero.
    #[guard(ERROR)]
    pub unsafe fn m8_subhash_reset(ctx: *mut Ctx, h: u64) -> i64 {
        call(ctx, h, SUBHASH_MAGIC, OP_SUBHASH_RESET, 0, 0, "m8_subhash_reset")
    }

    /// `nest.subplan`, hashed and LAZY (PG `ExecHashSubPlan`'s `buildSubPlanHash` at the first probe): set
    /// `h`'s build runs now unless this execution ran it: its build pipeline and that pipeline's exclusive producers, nested in
    /// the prober's call through the runner's frame, the build's end sealing the set. A probe calls it only while the word at
    /// `SUBHASH_TABLE_AT` is null. The sealed header's address, else a status below zero (the build's error, for the prober
    /// to raise where PG raises it).
    #[guard(ERROR)]
    pub unsafe fn m8_subhash_run(ctx: *mut Ctx, h: u64) -> i64 {
        call(ctx, h, SUBHASH_MAGIC, OP_SUBHASH_RUN, 0, 0, "m8_subhash_run")
    }

    /// `nest.subplan`, hashed over several key columns (PG `ExecHashSubPlan`): `row` (the set's key words, then one null
    /// word per column: 0, 1 NULL, 2 a cross-type value no key equals) against set `h`, a LAZY set built first. 1 TRUE, 0 FALSE,
    /// 2 NULL, else a status below zero.
    #[guard(ERROR)]
    pub unsafe fn m8_subhash_probe(ctx: *mut Ctx, h: u64, row: *const u64) -> i64 {
        call(ctx, h, SUBHASH_MAGIC, OP_SUBHASH_PROBE, row as u64, 0, "m8_subhash_probe")
    }

    /// D2849: `nest.subplan`, hashed over several key columns: whether set `h` holds a row with no NULL key (PG's
    /// `havehashrows`, its `hashtable` not empty: `ExecHashSubPlan` hashes a probe with no NULL column only then), a LAZY set
    /// built first. 1 yes, 0 no, else a status below zero.
    #[guard(ERROR)]
    pub unsafe fn m8_subhash_exact(ctx: *mut Ctx, h: u64) -> i64 {
        call(ctx, h, SUBHASH_MAGIC, OP_SUBHASH_EXACT, 0, 0, "m8_subhash_exact")
    }

    /// `nest.subplan`: `len` bytes at `ptr` (a by-reference EXPR result, PG's `curTuple`) copied into the
    /// calling thread's slot of SubPlan object `h`, valid until that thread's next `m8_sub_copy` on `h`. The copy's 8-aligned
    /// address, else a status below zero.
    #[guard(ERROR)]
    pub unsafe fn m8_sub_copy(ctx: u64, h: u64, ptr: u64, len: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let ctx = ctx as *mut Ctx;
        let ptr = ptr as *const u8;
        call(ctx, h, ARRAY_MAGIC, OP_SUB_COPY, ptr as u64, len, "m8_sub_copy")
    }

    /// An ARRAY sublink (PG `initArrayResult`): accumulator `h` emptied for a new result on the calling thread. 0, else a
    /// status below zero.
    #[guard(ERROR)]
    pub unsafe fn m8_array_begin(ctx: *mut Ctx, h: u64) -> i64 {
        call(ctx, h, ARRAY_MAGIC, OP_ARRAY_BEGIN, 0, 0, "m8_array_begin")
    }

    /// An ARRAY sublink (PG `accumArrayResult`): one element appended (`value`, `null` nonzero for NULL; a by-reference value
    /// is copied). 0, else a status below zero.
    #[guard(ERROR)]
    pub unsafe fn m8_array_push(ctx: *mut Ctx, h: u64, value: u64, null: u64) -> i64 {
        call(ctx, h, ARRAY_MAGIC, OP_ARRAY_PUSH, value, null, "m8_array_push")
    }

    /// An ARRAY sublink (PG `makeArrayResult`): accumulator `h`'s array datum (an empty array without elements), valid until
    /// the calling lane's next `m8_array_begin` on `h` (PG's per-call `curArray`), else a status below zero.
    #[guard(ERROR)]
    pub unsafe fn m8_array_end(ctx: *mut Ctx, h: u64) -> i64 {
        call(ctx, h, ARRAY_MAGIC, OP_ARRAY_END, 0, 0, "m8_array_end")
    }

    /// `nest.spool` / `nest.cte`: spool `h`'s region is full; it grows from the statement's grant (a named grant growth past
    /// it until the spill exit lands, never `SPOOL_SPILLED_WHAT`). The region's new capacity in rows, else a status below zero.
    #[guard(ERROR)]
    pub unsafe fn m8_spool_grow(ctx: u64, h: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let ctx = ctx as *mut Ctx;
        call(ctx, h, SPOOL_MAGIC, OP_SPOOL_GROW, 0, 0, "m8_spool_grow")
    }

    /// `nest.spool`: `len` bytes at `ptr` (a spooled record's by-reference datum) copied into spool
    /// `h`'s arena, which never moves and lives until the execution's end. The copy's 8-aligned address, else a status below
    /// zero.
    #[guard(ERROR)]
    pub unsafe fn m8_spool_copy(ctx: *mut Ctx, h: u64, ptr: *const u8, len: u64) -> i64 {
        call(ctx, h, SPOOL_MAGIC, OP_SPOOL_COPY, ptr as u64, len, "m8_spool_copy")
    }

    /// `nest.memo` (PG `cache_store_tuple`): 1 stored, 0 in bypass (the entry is gone until the next lookup), else below zero.
    #[guard(ERROR)]
    pub unsafe fn m8_memo_store(ctx: *mut Ctx, h: u64, row: *const u64) -> i64 {
        call(ctx, h, MEMO_MAGIC, OP_MEMO_STORE, row as u64, 0, "m8_memo_store")
    }

    /// `nest.memo`: the entry being filled, if any, is complete. 0, else a status below zero.
    #[guard(ERROR)]
    pub unsafe fn m8_memo_done(ctx: *mut Ctx, h: u64) -> i64 {
        call(ctx, h, MEMO_MAGIC, OP_MEMO_DONE, 0, 0, "m8_memo_done")
    }
}

/// D2611 (`m8_rerun_rearm`; PG's ExecReScan -> ExecReScanSetParamPlan at the rescan, before the re-opened node's first
/// call): rerun set `h`'s InitPlans re-armed now. A gated nest inner's re-open calls it before its one-time gates read an
/// InitPlan's param, and the `m8_rerun` after a passing gate re-arms nothing. 0, else a status below zero with the error
/// stashed.
pub struct RerunRearm;

impl Host for RerunRearm {
    type Args = (u64,);
    /// # Safety
    /// `h` is a rerun set's handle word (exec lays the set's object per execution) or a stale one, which its magic refuses by
    /// name; `ctx` is the caller's context, bound for the call (the set's object reads its frame words).
    unsafe fn run(ctx: *mut Ctx, (h,): (u64,)) -> u64 {
        let head = h as *mut Head;
        if head.is_null() || (*head).magic != RERUN_MAGIC {
            return crate::host_abi::bare(&format!("m8_rerun_rearm: a stale or foreign M8 handle {h:#x}")) as u64; // alloc: Cold
        }
        ((*head).op)(head, ctx, OP_RERUN_REARM, 0, 0) as u64
    }
}

/// D2034 (`m8_rerun_pull`; PG's ExecRecursiveUnion pulls its term one row per call): a recursive union's term set `h`
/// pulled on until its body's end took the rows its want word allows (lowerv4 ops/setop/recursive.rs, the streamed
/// generation). `first` nonzero at a generation's first pull: the set is re-armed first. 1 when the body ended, 0 when it
/// stopped, else a status below zero with the error stashed.
pub struct RerunPull;

impl Host for RerunPull {
    type Args = (u64, u64, u64);
    /// # Safety
    /// As `RerunRearm`'s: `h` is a rerun set's handle word or a stale one, which its magic refuses by name; `ctx` is the
    /// caller's context, bound for the call.
    unsafe fn run(ctx: *mut Ctx, (h, budget, first): (u64, u64, u64)) -> u64 {
        call(ctx, h, RERUN_MAGIC, OP_RERUN_PULL, budget, first, "m8_rerun_pull") as u64
    }
}

#[cfg(test)]
mod tests {
    use gravity_abi::status::ERROR;

    use super::{RerunPull, RerunRearm};
    use crate::contract::{entry1, entry3};
    use crate::ops::shared::object::entries::{Head, MEMO_MAGIC, OP_RERUN_PULL, OP_RERUN_REARM, RERUN_MAGIC};

    unsafe fn record(_h: *mut Head, _ctx: *mut gravity_abi::Ctx, code: u64, a: u64, b: u64) -> i64 {
        (code * 100 + a + b) as i64
    }

    /// The set's object runs with the re-arm code; a null or foreign handle is an error by name, never a call through it.
    #[test]
    fn the_rearm_runs_the_sets_op_and_refuses_a_foreign_handle_by_name() {
        let _ = gravity_fn::take_error();
        let set = Box::new(Head { magic: RERUN_MAGIC, op: record }); // alloc: Cold
        // SAFETY: a live head whose op reads no pointer.
        assert_eq!(unsafe { entry1::<RerunRearm>(0, &*set as *const Head as u64) }, OP_RERUN_REARM * 100);
        // SAFETY: a null handle: nothing is read.
        assert_eq!(unsafe { entry1::<RerunRearm>(0, 0) } as i64, ERROR);
        assert!(gravity_fn::take_error().is_some_and(|e| e.message().contains("m8_rerun_rearm: a stale or foreign M8 handle 0x0")));
        let memo = Box::new(Head { magic: MEMO_MAGIC, op: record }); // alloc: Cold
        // SAFETY: a live head of another kind: only its magic is read.
        assert_eq!(unsafe { entry1::<RerunRearm>(0, &*memo as *const Head as u64) } as i64, ERROR);
        assert!(gravity_fn::take_error().is_some_and(|e| e.message().contains("stale or foreign")));
    }

    /// D2034: the pull runs the set's op with its code, budget and first flag; a null or foreign handle is an error by name.
    #[test]
    fn the_pull_runs_the_sets_op_and_refuses_a_foreign_handle_by_name() {
        let _ = gravity_fn::take_error();
        let set = Box::new(Head { magic: RERUN_MAGIC, op: record }); // alloc: Cold
        // SAFETY: a live head whose op reads no pointer.
        assert_eq!(unsafe { entry3::<RerunPull>(0, &*set as *const Head as u64, 7, 1) }, OP_RERUN_PULL * 100 + 8);
        // SAFETY: a null handle: nothing is read.
        assert_eq!(unsafe { entry3::<RerunPull>(0, 0, 1, 0) } as i64, ERROR);
        assert!(gravity_fn::take_error().is_some_and(|e| e.message().contains("m8_rerun_pull: a stale or foreign M8 handle 0x0")));
        let memo = Box::new(Head { magic: MEMO_MAGIC, op: record }); // alloc: Cold
        // SAFETY: a live head of another kind: only its magic is read.
        assert_eq!(unsafe { entry3::<RerunPull>(0, &*memo as *const Head as u64, 1, 0) } as i64, ERROR);
        assert!(gravity_fn::take_error().is_some_and(|e| e.message().contains("stale or foreign")));
    }
}
