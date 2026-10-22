//! `nest.memo`'s Cache protocol (M8-plan §3.4; MC-1, M8-MEMO-CTE): PostgreSQL's Memoize cache in a v4
//! program. The object's head is the handle's target (kernels host_abi/v4/m8.rs, `MEMO_MAGIC`); its store is hosts
//! v4/m8_memo.rs's `MemoTable`, made at the first lookup from the key block the IR lays (the key count, the flags, the
//! row's columns, each key's hash equality operator and collation), and dropped at the execution's end. The limit is
//! PG's `get_hash_memory_limit()`: `work_mem` × `hash_mem_multiplier`, read at every execution's prepare.
//! - `OP_MEMO_MISS` (the lookup): a complete entry's rows address, else 0 (the inner runs; its rows are stored as they go).
//! - `OP_MEMO_STORE`: 1 stored, 0 in bypass (the entry is gone).
//! - `OP_MEMO_DONE`: the entry filled is complete.
//!
//! Under `!binary_mode` a key compares with its operator's function and hashes with the operator's hash function
//! (`get_op_hash_functions`, as ExecInitMemoize resolves them), each with the key's collation.
//!
//! D3014 M2: a widened nest (nest.loop `wide`, lowerv4 ops/nestloop/widen.rs) keeps its Memoize, and each participant has
//! its own table, as each PostgreSQL parallel worker has its own Memoize: ordinal 0's runtime is every serial run's (the
//! one table as before) and a widened nest's worker 0; the other worker ordinals' runtimes are made at prepare, on the
//! leader, when the nest's pipeline runs wide, each with the same limit, each touched by its own participant only (each
//! table is made at its participant's first lookup, as before). Their bytes are charged to the
//! grant at each pipeline end (`Reservation::charge_growth`, the hosted stores' road, D1421) once the nest ran wide. A
//! pool worker reads no catalog, so a wide run's tables (every ordinal's, 0 too) compare and hash with the functions the
//! leader resolved at prepare (`wide_key`, from the one key the lowering put in the proto's `arg`); functions a pool
//! thread may not run (`gravity_fn::session::pool_hosted`) keep the nest's pipeline on the leader (width 1). A serial
//! run resolves its keys at its first lookup, as before.
//! Owner M8-MEMO-CTE.
use std::cell::{Cell, RefCell};

use gravity_abi::status::ERROR;
use gravity_fn::fmgr_call::FmgrCall;
use gravity_hosts::ops::memoize::store::{Col, KeyFns, MemoTable, Probe};
use gravity_kernels::ops::shared::object::entries as abi;
use gravity_program::protos::{Proto, ProtoKind};
use memledger::{ChargedVec, Kind, Owner};
use types_error::PgResult;

use super::super::{Live, Prepare, Protocol};
use crate::glue;
use crate::inputs::hash_mem::hash_memory_limit;

/// The object's runtime and its key / row block reads (lint R6): the raw half.
mod raw;
use raw::op;

pub struct Memo;

/// The object: the head (the handle's target, first) and the runtimes behind it.
#[repr(C)]
struct MemoObj {
    head: abi::Head,
    /// Ordinal 0's runtime: every serial run's, and a widened nest's worker 0.
    rt: RefCell<MemoRt>,
    /// D3014 M2: a wide run's runtimes of the worker ordinals past 0 (ordinal o's at index o - 1; 0's is `rt`), made at
    /// prepare on the leader (`MemoObj::new`); empty for a serial run.
    wide: ChargedVec<RefCell<MemoRt>>,
    /// D3014 M2: a wide run's key functions, resolved on the leader at prepare (`None`: a serial run, or binary mode).
    key: Option<WideKey>,
}

/// D3014 M2: a widened nest's memo key: its hash and equality functions resolved on the leader (`gravity_fn::resolved`
/// ids, each bound again by the participant that makes a table, with no catalog read), its operator and its collation.
#[derive(Clone, Copy)]
struct WideKey {
    op: u32,
    coll: u32,
    hash: u32,
    eq: u32,
    word_eq: bool,
}

impl WideKey {
    /// The key's functions bound for the participant making its table (`FmgrCall::bind_resolved`: no catalog access).
    fn bind(&self) -> PgResult<KeyFns> {
        let (Some(hash), Some(eq)) = (FmgrCall::bind_resolved(self.hash, 1, self.coll), FmgrCall::bind_resolved(self.eq, 2, self.coll)) else {
            return Err(bad("nest.memo: a widened memo's key function is not resolved"));
        };
        Ok(KeyFns { hash, eq, word_eq: self.word_eq })
    }
}

/// D3014 M2: the worker ordinals a widened nest's runtimes cover: every ordinal a ctx carries (nested lanes count down
/// from `gravity_driver::NESTED_LANE_TOP`).
const ORDINALS: usize = 256;

impl MemoRt {
    fn new(limit: u64) -> MemoRt {
        MemoRt { limit, table: None, probe: Vec::new(), row: Vec::new() } // alloc: Exec
    }
}

impl MemoObj {
    /// The object of one execution: `limit` each table's (PG's per-participant `get_hash_memory_limit()`); `wide` (D3014
    /// M2) when the nest's pipeline runs wide: every worker ordinal's runtime made here, on the leader, and `key` its key
    /// functions (`None` in binary mode).
    fn new(limit: u64, wide: bool, key: Option<WideKey>, owner: Option<(Owner, Kind)>) -> MemoObj {
        let wide = match wide {
            true => {
                let mut runtimes: ChargedVec<RefCell<MemoRt>> = ChargedVec::new(owner.map(|o| o.0), owner.map_or(Kind::Region, |o| o.1)); // alloc: Exec charged owner.map(|o| o.0)
                runtimes.with_vec(|v| v.extend((1..ORDINALS).map(|_| RefCell::new(MemoRt::new(limit))))); // alloc: Exec charged owner
                runtimes
            }
            false => ChargedVec::none(),
        };
        MemoObj { head: abi::Head { magic: abi::MEMO_MAGIC, op }, rt: RefCell::new(MemoRt::new(limit)), wide, key }
    }

    /// D3014 M2: worker `ordinal`'s runtime of a wide run (ordinal 0's is `rt`); `None` past the runtimes made.
    fn wide_rt(&self, ordinal: u16) -> Option<&RefCell<MemoRt>> {
        match ordinal {
            0 => Some(&self.rt),
            o => self.wide.get(o as usize - 1),
        }
    }

    /// D3014 M2: every table's bytes in a wide run (`None` in a serial run: it charges nothing, as before). The leader's,
    /// after the workers finished.
    fn wide_bytes(&self) -> Option<u64> {
        if self.wide.is_empty() {
            return None;
        }
        let used = |c: &RefCell<MemoRt>| c.try_borrow().ok().and_then(|r| r.table.as_ref().map(MemoTable::mem_used)).unwrap_or(0);
        Some(used(&self.rt) + self.wide.iter().map(used).sum::<u64>())
    }
}

struct MemoRt {
    limit: u64,
    table: Option<MemoTable>,
    /// The lookup's probe, one per key: any key count, as PG's (the key block is the lowering's stack slot of that many
    /// keys); kept across lookups, so it grows once, at the first.
    probe: Vec<Probe>,
    /// D3075: the stored row's cells, kept across stores (no allocation per stored row).
    row: Vec<Col>,
}

struct MemoLive {
    obj: Box<MemoObj>,
    /// The handle word the object is laid in (`Proto::words`): EXPLAIN ANALYZE's cells name the object by it.
    base: u32,
    /// D3014 M2: the tables' bytes charged to the grant so far.
    charged: Cell<u64>,
}

impl Live for MemoLive {
    fn answers(&self) -> u16 {
        crate::ops::ask::AFTER
    }

    /// D3014 M2: a widened nest's tables (every participant's) charged to the grant at each pipeline end; no step.
    fn after_pipeline(&self, _pipeline: usize, exec: &mut gravity_driver::Exec) -> Option<gravity_driver::Result<()>> {
        if let Some(bytes) = self.obj.wide_bytes() {
            exec.reservation().charge_growth(&self.charged, bytes); // no worker runs here
        }
        None
    }

    /// EXPLAIN ANALYZE: the tables' counters (a wide run's every participant's summed, its peak the largest one's), as
    /// ExecMemoize keeps them: a purge keeps them, the table lives across the re-runs.
    fn memo_stats(&self) -> Option<(u32, [u64; 5])> {
        let mut s = [0u64; 5];
        for rt in core::iter::once(&self.obj.rt).chain(self.obj.wide.iter()) {
            let Ok(rt) = rt.try_borrow() else { continue };
            let Some(t) = rt.table.as_ref() else { continue };
            let st = t.stats;
            s[0] += st.hits;
            s[1] += st.misses;
            s[2] += st.evictions;
            s[3] += st.overflows;
            s[4] = s[4].max(st.mem_peak);
        }
        Some((self.base, s))
    }
}

impl Protocol for Memo {
    fn kind(&self) -> ProtoKind {
        ProtoKind::Cache
    }

    fn prepare(&self, p: &Proto, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        // D3014 M2: a widened nest's key functions, resolved here on the leader (its pipeline runs wide: width 2 or more);
        // a serial run reads none of this
        let k = p.pipeline as usize;
        let mut wide = cx.widths.get(k).is_some_and(|&w| w >= 2);
        let mut key = None;
        if wide && p.arg != 0 {
            key = wide_key(p.arg)?;
            if key.is_none() {
                cx.widths[k] = 1; // functions a pool thread may not run: the nest's pipeline on the leader
                wide = false;
                fires::fire("nest.memo.serial");
            }
        }
        lay(p, cx.words, cx.nulls, cx.nparams, wide, key, cx.owner).map(Some)
    }
}

/// D3014 M2: the one key of a widened nest's memo (`arg`: its hash equality operator, its collation in the high half;
/// lowerv4 ops/memoize/lower.rs `wide_key`) resolved on the leader, as `key_fns` resolves it; `None` when its hash or
/// equality function is one a pool thread may not run (`gravity_fn::session::pool_hosted`) or does not resolve.
fn wide_key(arg: u64) -> PgResult<Option<WideKey>> {
    let (op, coll) = (arg as u32, (arg >> 32) as u32);
    let Some((hash, _)) = gravity_fn::typesem::memoize::op_hash_functions(op)? else {
        return Err(bad(&format!("nest.memo: could not find hash function for hash operator {op}"))); // alloc: Cold
    };
    let eq = lsyscache::get_opcode(op)?;
    if !gravity_fn::session::pool_hosted(hash) || !gravity_fn::session::pool_hosted(eq) {
        return Ok(None);
    }
    let (Ok(hash_id), Ok(eq_id)) = (gravity_fn::resolved::resolve(hash)?, gravity_fn::resolved::resolve(eq)?) else { return Ok(None) };
    Ok(Some(WideKey { op, coll, hash: hash_id, eq: eq_id, word_eq: word_equality(eq) }))
}

/// The memo lives of `program` alone, laid as prepare lays them: the crate rig's (lowerv4 tests/common).
pub fn prepare_bare(program: &gravity_program::Program, words: &mut [u64], nulls: &mut [u8], nparams: &mut usize, out: &mut crate::ops::LiveList) -> PgResult<()> {
    let protos = program.v4.as_ref().map_or(&[][..], |d| &d.protos[..]);
    for p in protos.iter().filter(|p| p.kind == ProtoKind::Cache) {
        let l = lay(p, words, nulls, nparams, false, None, None)?;
        out.with_vec(|v| v.push(l)); // alloc: Exec charged out
    }
    Ok(())
}

/// mergecmd tw-core-197 (D1933 MEMO): an empty memo object laid over a recheck's own copy of the words (exec rerun/epq.rs
/// `recheck_on`): PostgreSQL's EvalPlanQualStart builds a fresh planstate tree, so the recheck's Memoize starts with an
/// empty cache and never answers from the statement's entries (a cached target row would be its old version); the
/// statement keeps its own object and words. The object lives until the recheck returns.
pub fn fresh(p: &Proto, words: &mut [u64], nulls: &mut [u8]) -> PgResult<Box<dyn Live>> {
    let mut n = 0usize;
    lay(p, words, nulls, &mut n, false, None, None)
}

/// One Cache proto's object laid in its handle word (`wide`, `key`: a wide run's runtimes and key functions, D3014 M2).
fn lay(p: &Proto, words: &mut [u64], nulls: &mut [u8], nparams: &mut usize, wide: bool, key: Option<WideKey>, owner: Option<(Owner, Kind)>) -> PgResult<Box<dyn Live>> {
    let base = p.words.base as usize;
    if p.words.n != 1 || base >= words.len() || base >= nulls.len() {
        return Err(glue::internal("gravity: nest.memo's declaration is not its memo object's (one handle word)"));
    }
    let obj = Box::new(MemoObj::new(hash_memory_limit(), wide, key, owner)); // alloc: Exec
    words[base] = &obj.head as *const abi::Head as u64;
    nulls[base] = 0;
    *nparams = (*nparams).max(base + 1);
    Ok(Box::new(MemoLive { obj, base: base as u32, charged: Cell::new(0) })) // alloc: Exec
}

/// A key's hash function and equality function, from its hash equality operator (ExecInitMemoize:
/// `get_op_hash_functions` and `get_opcode`), bound with its collation.
fn key_fns(opno: u32, coll: u32) -> PgResult<KeyFns> {
    let Some((hash, _)) = gravity_fn::typesem::memoize::op_hash_functions(opno)? else {
        return Err(bad(&format!("nest.memo: could not find hash function for hash operator {opno}"))); // alloc: Cold
    };
    let eq = lsyscache::get_opcode(opno)?;
    Ok(KeyFns { hash: FmgrCall::bind(hash, 1, coll)?, eq: FmgrCall::bind(eq, 2, coll)?, word_eq: word_equality(eq) })
}

/// D2988: an equality function that is word equality on its by-value type (the probe and the stored key are the same
/// type's Datum words): int2eq, int4eq, int8eq, oideq, booleq, chareq, date_eq. Cross-type operators (int48eq ..) are
/// not, nor any float (-0 = 0, NaN = NaN), numeric or collation-aware equality: those keep the call.
fn word_equality(eq: u32) -> bool {
    const F_INT8EQ: u32 = 467;
    const F_DATE_EQ: u32 = 1086;
    use types_core::fmgr::{F_BOOLEQ, F_CHAREQ, F_INT2EQ, F_INT4EQ, F_OIDEQ};
    [F_INT2EQ, F_INT4EQ, F_INT8EQ, F_OIDEQ, F_BOOLEQ, F_CHAREQ, F_DATE_EQ].contains(&eq)
}

#[cold]
#[inline(never)]
fn fail(what: &str) -> i64 {
    gravity_fn::stash_error(bad(what));
    ERROR
}

#[cold]
#[inline(never)]
fn bad(what: &str) -> Box<types_error::PgError> {
    glue::internal(&format!("gravity: {what}")) // alloc: Cold
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use super::*;
    use gravity_abi::Ctx;

    fn obj() -> Box<MemoObj> {
        Box::new(MemoObj::new(1 << 20, false, None, None)) // alloc: Cold
    }

    fn h(o: &MemoObj) -> u64 {
        &o.head as *const abi::Head as u64
    }

    /// A binary key block of one int key and a one-column row.
    fn key(v: u64) -> Vec<u64> {
        vec![1, abi::MEMO_BINARY, v, 0, 0, 0, 0, 8] // alloc: Cold
    }

    fn ctx(ordinal: u16, width: u16) -> Ctx {
        // SAFETY: a Ctx of null pointers and zero words is a valid value; the op reads only the ordinal and the width.
        let mut c: Ctx = unsafe { core::mem::zeroed() };
        (c.worker_ordinal, c.width) = (ordinal, width);
        c
    }

    /// D3014 M2: a widened nest's workers each fill and hit their own table; a serial run's is ordinal 0's, which worker
    /// 0 shares; a purge drops only its own participant's entries; the bytes count once the nest ran wide.
    #[test]
    fn each_worker_ordinal_has_its_own_table_and_purge() {
        assert!(obj().wide_bytes().is_none(), "a serial run is not charged");
        let o = Box::new(MemoObj::new(1 << 20, true, None, None)); // alloc: Cold
        let (mut w1, mut w2, mut serial) = (ctx(1, 8), ctx(2, 8), ctx(5, 1));
        let (w1, w2, serial) = (&mut w1 as *mut Ctx, &mut w2 as *mut Ctx, &mut serial as *mut Ctx);
        let purge = |v: u64| vec![1, abi::MEMO_BINARY | abi::MEMO_PURGE, v, 0, 0, 0, 0, 8]; // alloc: Cold
        let row = [1u64, 42, 0, 0];
        unsafe {
            assert_eq!(abi::m8_memo_miss(serial, h(&o), key(4).as_ptr()), 0);
            assert_eq!(abi::m8_memo_store(serial, h(&o), row.as_ptr()), 1);
            assert_eq!(abi::m8_memo_done(serial, h(&o)), 0);
            assert!(abi::m8_memo_miss(serial, h(&o), key(4).as_ptr()) > 0, "a serial run (any ordinal at width 1) hits ordinal 0's table");
            assert_eq!(abi::m8_memo_miss(w1, h(&o), key(4).as_ptr()), 0, "worker 1 has its own table: a miss");
            assert_eq!(abi::m8_memo_store(w1, h(&o), row.as_ptr()), 1);
            assert_eq!(abi::m8_memo_done(w1, h(&o)), 0);
            assert!(abi::m8_memo_miss(w1, h(&o), key(4).as_ptr()) > 0, "and hits it");
            assert_eq!(abi::m8_memo_miss(w2, h(&o), key(4).as_ptr()), 0, "worker 2 misses: its own table");
            assert_eq!(abi::m8_memo_store(w2, h(&o), row.as_ptr()), 1);
            assert_eq!(abi::m8_memo_done(w2, h(&o)), 0);
            assert_eq!(abi::m8_memo_miss(w2, h(&o), purge(4).as_ptr()), 0, "worker 2's purge drops its own entry");
            assert!(abi::m8_memo_miss(w1, h(&o), key(4).as_ptr()) > 0, "worker 1's entry stays");
            let mut w0 = ctx(0, 8);
            assert!(abi::m8_memo_miss(&mut w0, h(&o), key(4).as_ptr()) > 0, "worker 0 shares ordinal 0's table");
        }
        assert!(o.wide_bytes().is_some_and(|b| b > 0), "a wide run's tables count");
    }

    #[test]
    fn the_entries_run_a_miss_a_fill_and_a_hit() {
        let o = obj();
        let ctx: *mut Ctx = core::ptr::null_mut();
        unsafe {
            assert_eq!(abi::m8_memo_miss(ctx, h(&o), key(4).as_ptr()), 0, "a miss");
            let row = [1u64, 42, 0, 0];
            assert_eq!(abi::m8_memo_store(ctx, h(&o), row.as_ptr()), 1);
            assert_eq!(abi::m8_memo_done(ctx, h(&o)), 0);
            let at = abi::m8_memo_miss(ctx, h(&o), key(4).as_ptr()) as *const u64;
            assert!(!at.is_null());
            assert_eq!((*at, *at.add(1), *at.add(2), *at.add(3)), (1, 1, 42, 0));
        }
    }

    #[test]
    fn a_purge_lookup_drops_the_entries_first_and_a_first_one_makes_the_table() {
        let o = obj();
        let ctx: *mut Ctx = core::ptr::null_mut();
        let purge = |v: u64| vec![1, abi::MEMO_BINARY | abi::MEMO_PURGE, v, 0, 0, 0, 0, 8]; // alloc: Cold
        unsafe {
            assert_eq!(abi::m8_memo_miss(ctx, h(&o), purge(4).as_ptr()), 0, "no table yet: a plain miss");
            let row = [1u64, 42, 0, 0];
            assert_eq!(abi::m8_memo_store(ctx, h(&o), row.as_ptr()), 1);
            assert_eq!(abi::m8_memo_done(ctx, h(&o)), 0);
            assert!(abi::m8_memo_miss(ctx, h(&o), key(4).as_ptr()) > 0, "no purge flag: the entry hits");
            assert_eq!(abi::m8_memo_miss(ctx, h(&o), purge(4).as_ptr()), 0, "the purge drops it: a miss");
            assert_eq!(abi::m8_memo_store(ctx, h(&o), row.as_ptr()), 1, "and the miss fills a new entry");
            assert_eq!(abi::m8_memo_done(ctx, h(&o)), 0);
            assert!(abi::m8_memo_miss(ctx, h(&o), key(4).as_ptr()) > 0);
            let rt = o.rt.borrow();
            let t = rt.table.as_ref().expect("made");
            assert_eq!((t.len(), t.stats.evictions), (1, 1));
        }
    }

    /// PG's Memoize takes any number of keys (NEST-02: no 32-key cap): a block of 40 binary keys misses, fills and hits,
    /// and one differing in its 40th key only is another entry.
    #[test]
    fn a_key_block_of_forty_keys_misses_fills_and_hits() {
        let o = obj();
        let ctx: *mut Ctx = core::ptr::null_mut();
        let wide = |last: u64| {
            let mut k = vec![40, abi::MEMO_BINARY]; // alloc: Cold
            for i in 0..40u64 {
                k.extend_from_slice(&[if i == 39 { last } else { i }, 0, 0, 0, 0, 8]); // alloc: Cold
            }
            k
        };
        unsafe {
            assert_eq!(abi::m8_memo_miss(ctx, h(&o), wide(7).as_ptr()), 0, "a miss");
            let row = [1u64, 42, 0, 0];
            assert_eq!(abi::m8_memo_store(ctx, h(&o), row.as_ptr()), 1);
            assert_eq!(abi::m8_memo_done(ctx, h(&o)), 0);
            assert!(abi::m8_memo_miss(ctx, h(&o), wide(7).as_ptr()) > 0, "the same 40 keys hit");
            assert_eq!(abi::m8_memo_miss(ctx, h(&o), wide(8).as_ptr()), 0, "the 40th key differs: a miss");
        }
    }

    #[test]
    fn a_store_before_a_lookup_and_a_bad_block_error_by_name() {
        let o = obj();
        let ctx: *mut Ctx = core::ptr::null_mut();
        unsafe {
            let row = [0u64];
            assert_eq!(abi::m8_memo_store(ctx, h(&o), row.as_ptr()), ERROR);
            assert_eq!(gravity_fn::take_error().map(|e| e.message().to_owned()).unwrap_or_default(), "gravity: m8_memo_store: a store before any lookup");
            let k = [0u64, 0];
            assert_eq!(abi::m8_memo_miss(ctx, h(&o), k.as_ptr()), ERROR);
            assert!(gravity_fn::take_error().is_some());
        }
    }
}
