//! `nest.memo`'s Cache protocol, its raw half (lint R6; construct bundle memoize-04): the object's runtime behind the
//! handle (`op`) and its reads of the key and row blocks the IR lays, moved as is from ops/memoize/proto.rs with their
//! safety contracts. A child module of proto.rs, so it reads the object's own fields with no visibility change.
#![allow(unsafe_code)]
use gravity_abi::status::ERROR;
use gravity_abi::Ctx;
use gravity_hosts::ops::memoize::store::{Col, Lookup, MemoTable, Probe};
use types_error::PgResult;

use super::{abi, bad, fail, key_fns, MemoObj, MemoRt, WideKey};

/// The object's runtime (`Head::op`).
pub(super) unsafe fn op(h: *mut abi::Head, ctx: *mut Ctx, code: u64, a: u64, _b: u64) -> i64 {
    // SAFETY: `h` is the first field of a live `MemoObj` (`call` checked its magic), laid for this execution.
    let obj = &*h.cast::<MemoObj>();
    // D3014 M2: a serial run (no ctx, or width 1) uses ordinal 0's runtime as before; a widened nest's worker its own,
    // which only that participant touches (one thread at a time: a worker slot's claims run one after another)
    let wide = !ctx.is_null() && (*ctx).width > 1;
    let cell = match !wide {
        true => &obj.rt,
        false => match obj.wide_rt((*ctx).worker_ordinal) {
            Some(c) => c,
            None => return fail("nest.memo: a wide run's worker past the runtimes its prepare made"),
        },
    };
    let Ok(mut rt) = cell.try_borrow_mut() else { return fail("nest.memo: a memo entered inside its own call") };
    let r = match code {
        abi::OP_MEMO_MISS => rt.lookup(a as *const u64, wide.then_some(&obj.key)),
        abi::OP_MEMO_STORE => rt.store(a as *const u64),
        abi::OP_MEMO_DONE => {
            if let Some(t) = rt.table.as_mut() {
                t.done();
            }
            Ok(0)
        }
        _ => Err(bad(&format!("nest.memo: no operation {code}"))), // alloc: Cold
    };
    // CONVERT wave 2 (site 11): the cache's bytes go to the breaker this pipeline runs for (else the statement), from the
    // lookup that made it
    if let Some(t) = rt.table.as_mut().filter(|t| t.needs_owner()) {
        t.own(gravity_fn::arena::OpArena::slot_owner_kind(ctx));
    }
    r.unwrap_or_else(|e| {
        gravity_fn::stash_error(e);
        ERROR
    })
}

impl MemoRt {
    /// MEMO_CACHE_LOOKUP over the key block at `k` (the table made at the first). `wide`: a wide run's (D3014 M2), whose
    /// table compares with the leader's resolution (a pool worker reads no catalog).
    unsafe fn lookup(&mut self, k: *const u64, wide: Option<&Option<WideKey>>) -> PgResult<i64> {
        let n = *k as usize;
        let flags = *k.add(1);
        if n == 0 {
            return Err(bad("m8_memo_miss: a key block of no keys"));
        }
        let key = |i: usize, w: usize| *k.add(abi::MEMO_KEY_HEAD + i * abi::MEMO_KEY_WORDS + w);
        // the first lookup after a re-run plan re-opened the memo's nest, or every lookup when the child reads its own nest's
        // param outside the keys (D3836) (ExecReScanMemoize's cache_purge_all; none made yet: nothing to drop)
        if let Some(t) = self.table.as_mut().filter(|_| flags & abi::MEMO_PURGE != 0) {
            t.purge_all();
        }
        if self.table.is_none() {
            let binary = flags & abi::MEMO_BINARY != 0;
            let mut fns = Vec::new(); // alloc: Exec
            match wide {
                _ if binary => {}
                None => {
                    for i in 0..n {
                        let (opno, coll) = (key(i, 3) as u32, key(i, 4) as u32);
                        fns.push(key_fns(opno, coll)?); // alloc: Exec
                    }
                }
                Some(w) => match w.filter(|w| n == 1 && (w.op, w.coll) == (key(0, 3) as u32, key(0, 4) as u32)) {
                    Some(w) => fns.push(w.bind()?), // alloc: Exec
                    None => return Err(bad("nest.memo: a widened memo's key with no resolution on the leader")),
                },
            }
            self.table = Some(MemoTable::new(binary, flags & abi::MEMO_SINGLEROW != 0, self.limit, fns));
        }
        self.probe.clear();
        self.probe.extend((0..n).map(|i| Probe { word: key(i, 0), null: key(i, 1) != 0, len: key(i, 2) })); // alloc: Exec (grows at the first lookup only)
        let t = self.table.as_mut().expect("made above");
        Ok(match t.lookup(&self.probe)? {
            Lookup::Hit(at) => at as i64,
            Lookup::Miss { .. } => 0,
        })
    }

    /// `cache_store_tuple` of the row block at `r`.
    unsafe fn store(&mut self, r: *const u64) -> PgResult<i64> {
        let Some(t) = self.table.as_mut() else { return Err(bad("m8_memo_store: a store before any lookup")) };
        let n = *r as usize;
        let col = |i: usize, w: usize| *r.add(abi::MEMO_ROW_HEAD + i * abi::MEMO_ROW_WORDS + w);
        self.row.clear();
        self.row.extend((0..n).map(|i| Col { word: col(i, 0), null: col(i, 1) != 0, len: col(i, 2) })); // alloc: Exec (grows at the first store only)
        Ok(t.store(&self.row)? as i64)
    }
}
