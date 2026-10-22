//! `ops::subplan`'s unsafe code, each piece behind the contract its caller keeps:
//! - the hashed set's and the result object's runtime (`Head::op`: `set_op`, `result_op`), entered through a laid object's
//!   head whose magic the kernels' `call` checked;
//! - the ARRAY accumulators (`Arrays`), one per lane, each touched only by its own lane's thread;
//! - a LAZY build run through the runner's frame (`Lazy::ensure`);
//! - the several-column set's row reads (`Multi`, `SetObj::probe`), over the key and null words the lowering laid;
//! - a rerun set's re-arm of its LAZY sets (`rearm_sets`), found through the handle words in the caller's context params
//!   (exec rerun/mod.rs `run`, after `decide`).
#![allow(unsafe_code)]
use core::cell::Cell;

use gravity_abi::status::ERROR;
use gravity_abi::Ctx;
use gravity_fn::arena::OpArena;
use gravity_hosts::ops::subplan::stores::ArrayAcc;
use gravity_kernels::ops::shared::object::entries as abi;
use types_error::PgResult;

use crate::glue;
use crate::ops::subplan::array::Arrays;
use crate::ops::subplan::hash::{fail, ResultObj, SetObj};
use crate::ops::subplan::keyset::KeySet;
use crate::ops::subplan::lazy::{bad, Lazy, DONE, FAILED, RUNNING, STALE};
use crate::ops::subplan::multi::{Multi, Refused, FALSE, NONE, NULL, SYNC_SLACK, TRUE};

/// Each LAZY hashed set whose handle word is in `sets` re-armed (`SetObj::rearm`): emptied, its next probe runs its build
/// again (the members the run skips). `ctx` is the caller's context, bound for this call (`m8_rerun`'s first argument),
/// its params holding the handle words, each nonzero one the set exec's prepare laid (`lay_set`), live until the
/// execution's end. No set: no read.
pub(crate) fn rearm_sets(ctx: *mut Ctx, sets: &[usize]) -> PgResult<()> {
    if sets.is_empty() {
        return Ok(());
    }
    if ctx.is_null() {
        return Err(glue::internal("gravity: m8_rerun: a LAZY hashed set to re-arm with no context"));
    }
    for &h in sets {
        // SAFETY: the caller's bound context (above); its params hold the handle words, a nonzero one the object's head,
        // its magic checked before any other read
        let o = unsafe { *(*(ctx as *const u8).add(gravity_abi::ctx::hdr::PARAMS).cast::<*const u64>()).add(h) } as *const abi::Head;
        // SAFETY: as above: a nonzero handle word is a laid object's head
        if o.is_null() || unsafe { (*o).magic } != abi::SUBHASH_MAGIC {
            return Err(glue::internal("gravity: m8_rerun: a LAZY hashed set's handle word is not its set"));
        }
        // SAFETY: the head is a SubHash object's (magic checked), the first field of a live `SetObj`
        unsafe { &*o.cast::<SetObj>() }.rearm()?;
    }
    Ok(())
}

/// The set's runtime (`Head::op`).
pub(super) unsafe fn set_op(h: *mut abi::Head, ctx: *mut Ctx, code: u64, a: u64, _b: u64) -> i64 {
    // SAFETY: `h` is the first field of a live `SetObj` (`call` checked its magic), laid for this execution.
    let o = &*h.cast::<SetObj>();
    // D3506 SETLOCK: a twin's build runs this set's members, whose end fills the twin (`twin_run`)
    let to = o.redirect.get();
    if to != 0 && matches!(code, abi::OP_SUBHASH_INSERT | abi::OP_SUBHASH_SEAL | abi::OP_SUBHASH_RESET) {
        return set_op(to as *mut abi::Head, ctx, code, a, _b);
    }
    if code == abi::OP_SUBHASH_RUN && o.twin_of.get() != 0 {
        return twin_run(o).unwrap_or_else(|e| {
            gravity_fn::stash_error(e);
            ERROR
        });
    }
    if code == abi::OP_SUBHASH_RUN {
        // before the borrow: the build's end inserts into the set while this call runs it
        return o.lazy.ensure(o.seal_word()).unwrap_or_else(|e| {
            gravity_fn::stash_error(e);
            ERROR
        });
    }
    if code == abi::OP_SUBHASH_PROBE {
        return o.probe(a);
    }
    if code == abi::OP_SUBHASH_EXACT {
        return o.exact();
    }
    if !ctx.is_null() && (*ctx).width > 1 {
        return fail("nest.subplan: a hashed SubPlan's set filled by more than one worker (its build is serial)");
    }
    let Ok(mut set) = o.set.try_borrow_mut() else { return fail("nest.subplan: a hashed set entered inside its own call") };
    match code {
        abi::OP_SUBHASH_INSERT if o.sealed.get() => fail("m8_subhash_insert: a row into a sealed set (a rebuild resets it first)"),
        abi::OP_SUBHASH_INSERT if a == 0 => fail("m8_subhash_insert: a null row"),
        abi::OP_SUBHASH_INSERT => {
            // SAFETY: `a` is the build end's row: one key word, then one null word (the layout `new_set` admitted).
            let row = a as *const u64;
            // CONVERT wave 2 (sites 6 + 7): the set's bytes go to the build pipeline's breaker slot under its kind (the
            // hashed SubPlan's Distinct, lowerv4 subplan/hashed.rs; the statement's, Region, when none), looked up at the
            // set's first insert; a growth the allocator refuses is the statement's 53200.
            if let Some(m) = &o.multi {
                let mut m = m.borrow_mut();
                if m.needs_owner() {
                    m.own(OpArena::slot_owner_kind(ctx));
                }
                return match m.insert(row, o.no_nulls) {
                    Ok(()) => 0,
                    Err(Refused::Row(e)) => fail(&e),
                    Err(Refused::Memory(n)) => oom(n),
                };
            }
            if !(o.no_nulls && *row.add(1) != 0) {
                if set.needs_owner() {
                    set.own(OpArena::slot_owner_kind(ctx));
                }
                if set.insert(*row, *row.add(1) != 0).is_err() {
                    return oom(set.grow_bytes() as u64);
                }
            }
            0
        }
        abi::OP_SUBHASH_SEAL if o.multi.is_some() => {
            if let Some(m) = &o.multi {
                m.borrow_mut().sync(0);
            }
            o.sealed.set(true);
            o.mseal.set(1);
            0
        }
        abi::OP_SUBHASH_SEAL => {
            o.sealed.set(true);
            o.table.set(set.word());
            o.table.get() as i64
        }
        abi::OP_SUBHASH_RESET if o.lazy.ran() => fail("m8_subhash_reset: a LAZY set's rebuild its rerun set did not re-arm"),
        abi::OP_SUBHASH_RESET => {
            o.table.set(0);
            o.mseal.set(0);
            if let Some(m) = &o.multi {
                m.borrow_mut().clear();
            }
            *set = KeySet::new(o.width);
            o.sealed.set(false);
            0
        }
        _ => fail(&format!("nest.subplan: no hashed-set operation {code}")), // alloc: Cold
    }
}

/// A set's growth the allocator refused: the statement's 53200 (`Error::out_of_memory` of the bytes asked), stashed for
/// the call's ERROR.
fn oom(bytes: u64) -> i64 {
    gravity_fn::stash_error(glue::pg_error(gravity_driver::Error::out_of_memory(bytes)));
    ERROR
}

/// D3506 SETLOCK: `OP_SUBHASH_RUN` on a twin (exec subplan hash.rs `SubHashEpq`): its sealed header, built now by its
/// statement set's members (run again: `Lazy::ensure` over a STALE road), whose build end the statement set redirects
/// here while they run. A statement set never run is left STALE, so its own first probe runs them again too.
unsafe fn twin_run(o: &SetObj) -> types_error::PgResult<i64> {
    use crate::ops::subplan::lazy::{NOT_RUN, RUNNING, STALE};
    if o.table.get() != 0 {
        return Ok(o.table.get() as i64);
    }
    // SAFETY: `twin_of` is the statement set's head (`lay_twin`), laid for this execution before this one
    let main = &*(o.twin_of.get() as *const SetObj);
    if main.lazy.run.get() == RUNNING || main.redirect.get() != 0 {
        return Err(glue::internal("gravity: nest.subplan.epq: a recheck's set built inside the statement set's build"));
    }
    main.redirect.set(&o.head as *const abi::Head as u64);
    let r = o.lazy.ensure(o.seal_word());
    main.redirect.set(0);
    if main.lazy.run.get() == NOT_RUN {
        main.lazy.run.set(STALE);
    }
    r
}

/// The result object's runtime (`Head::op`).
pub(super) unsafe fn result_op(h: *mut abi::Head, ctx: *mut Ctx, code: u64, a: u64, b: u64) -> i64 {
    // SAFETY: `h` is the first field of a live `ResultObj` (`call` checked its magic), laid for this execution.
    let o = &*h.cast::<ResultObj>();
    let lane = if ctx.is_null() { 0 } else { (*ctx).worker_ordinal as usize };
    let Some(slot) = o.slots.get(lane) else { return fail("nest.subplan: a SubPlan result read on a lane past the table") };
    match code {
        // SAFETY: the slot is the calling lane's alone (one thread per worker ordinal; a nested lane is its own ordinal).
        abi::OP_SUB_COPY => copy(&mut *slot.get(), a as *const u8, b as usize),
        // SAFETY: as the copy: the lane's accumulator is its own thread's.
        abi::OP_ARRAY_BEGIN | abi::OP_ARRAY_PUSH | abi::OP_ARRAY_END => o.arrays.op(lane, code, a, b),
        _ => fail(&format!("nest.subplan: no result operation {code}")), // alloc: Cold
    }
}

/// `len` bytes at `src` into `buf` (8-aligned), replacing its previous contents; the copy's address. A source inside the
/// buffer (the previous copy) is moved, not read after a free.
unsafe fn copy(buf: &mut Vec<u64>, src: *const u8, len: usize) -> i64 {
    if src.is_null() && len > 0 {
        return fail("m8_sub_copy: a null datum of nonzero length");
    }
    let n = len.div_ceil(8).max(1);
    if buf.capacity() >= n {
        if buf.len() < n {
            buf.resize(n, 0);
        }
        core::ptr::copy(src, buf.as_mut_ptr().cast::<u8>(), len);
    } else {
        let mut next = vec![0u64; n]; // alloc: Exec
        core::ptr::copy_nonoverlapping(src, next.as_mut_ptr().cast::<u8>(), len);
        *buf = next; // the previous buffer drops after the copy
    }
    buf.as_ptr() as i64
}

impl Arrays {
    /// D1421 (leader, after a pipeline: no lane runs): every lane's accumulator growth charged to the grant; no step.
    pub(crate) fn charge(&self, exec: &mut gravity_driver::Exec) -> Option<gravity_driver::Result<()>> {
        // SAFETY: each lane is touched only by its own thread while a pipeline runs; none runs here.
        let now = self.lanes.iter().map(|l| unsafe { (*l.get()).as_ref().map_or(0, |a| a.bytes() as u64) }).sum();
        exec.reservation().charge_growth(&self.charged, now);
        None
    }

    /// `m8_array_begin` / `_push` / `_end` for `lane`.
    ///
    /// # Safety
    /// Called only from `lane`'s own thread (one thread per worker ordinal; a nested lane is its own ordinal).
    pub unsafe fn op(&self, lane: usize, code: u64, a: u64, b: u64) -> i64 {
        let Some(e) = self.elem.as_ref() else { return fail("nest.subplan: an ARRAY entry on a copy-only SubPlan result") };
        let Some(cell) = self.lanes.get(lane) else { return fail("nest.subplan: an ARRAY sublink on a lane past the table") };
        let acc = (*cell.get()).get_or_insert_with(ArrayAcc::default);
        let r = match code {
            abi::OP_ARRAY_BEGIN => acc.begin(e).map(|()| 0),
            abi::OP_ARRAY_PUSH => acc.push(a, b != 0, e).map(|()| 0),
            _ => acc.end().map(|p| p as i64),
        };
        r.unwrap_or_else(|err| {
            gravity_fn::stash_error(err);
            gravity_abi::status::ERROR
        })
    }
}

impl Lazy {
    /// `OP_SUBHASH_RUN`: the sealed header's address (`table`) once this execution's build ran: now (nested, through the
    /// runner's frame, as `m8_init_run` runs an Init's members), or earlier.
    pub(crate) unsafe fn ensure(&self, table: &Cell<u64>) -> PgResult<i64> {
        match (table.get(), self.run.get()) {
            (t, _) if t != 0 => return Ok(t as i64),
            _ if self.members.is_empty() => return Err(bad("m8_subhash_run: a set built in program order (not LAZY) probed before its seal")),
            (_, RUNNING) => return Err(bad("m8_subhash_run: a LAZY set probed inside its own build")),
            (_, FAILED) => return Err(bad("m8_subhash_run: the set's build failed earlier in this execution")),
            (_, DONE) => return Err(bad("m8_subhash_run: a LAZY set reset after its build but not re-armed")),
            _ => {}
        }
        let again = self.run.get() == STALE;
        let Some(mut f) = self.frame.get() else {
            return Err(bad("m8_subhash_run: no runner frame (bug: a probe on a worker; D1035: the lowering pins every probe of a LAZY set to the leader)"));
        };
        self.run.set(RUNNING);
        // SAFETY: the frame is the runner's, published on this thread while it runs the program (gravity_parallel
        // `publish`); this probe is inside one of that runner's calls, and nothing else uses the frame while the members run.
        let r = if again { f.as_mut().rerun_members(&self.members) } else { f.as_mut().run_members(&self.members) };
        let t = table.get();
        self.run.set(if r.is_ok() && t != 0 { DONE } else { FAILED });
        match r {
            Ok(()) if t != 0 => Ok(t as i64),
            Ok(()) => Err(bad("m8_subhash_run: the LAZY set's build ran without sealing it")),
            Err(e) => Err(glue::pg_error(e)),
        }
    }
}

impl Multi {
    /// A row's key, encoded: per column its masked word (8 bytes), or a byte key's length (8 bytes) then its bytes; a column
    /// whose null word is not 0 contributes an empty key. The null words, and the row's error by name.
    ///
    /// # Safety
    /// `row` holds the key words (one per column, two per byte-key column) then one null word per column; a byte key's
    /// pointer, when its length is not 0, addresses that many readable bytes.
    unsafe fn encode(&self, row: *const u64, op: &str, most: u64) -> Result<(Vec<u8>, [u64; super::multi::MAX_COLS]), String> {
        let n = self.masks.len();
        let nw = n + self.bytes.count_ones() as usize;
        let (mut enc, mut null, mut at) = (Vec::with_capacity(8 * nw), [0u64; super::multi::MAX_COLS], 0); // alloc: Exec
        for (c, nc) in null.iter_mut().enumerate().take(n) {
            let by = self.bytes >> c & 1 != 0;
            // SAFETY: the caller's.
            let (k, w) = unsafe { (*row.add(at), *row.add(nw + c)) };
            if w > most {
                return Err(format!("{op}: null word {w} in column {c} ({})", if most == 1 { "0 or 1" } else { "0, 1 or 2" })); // alloc: Cold
            }
            *nc = w;
            match (by, w) {
                (false, 0) => enc.extend_from_slice(&(k & self.masks[c]).to_le_bytes()),
                (true, 0) => {
                    // SAFETY: the caller's.
                    let len = unsafe { *row.add(at + 1) };
                    if len != 0 && k == 0 {
                        return Err(format!("{op}: a null pointer for the {len} bytes of column {c}")); // alloc: Cold
                    }
                    enc.extend_from_slice(&len.to_le_bytes());
                    // SAFETY: the caller's: `len` readable bytes at `k`.
                    enc.extend_from_slice(if len == 0 { &[] } else { unsafe { core::slice::from_raw_parts(k as *const u8, len as usize) } });
                }
                _ => enc.extend_from_slice(&[0; 8]),
            }
            at += 1 + by as usize;
        }
        Ok((enc, null))
    }

    /// The build's row: the key words, then one null word per column (0 or 1). A row with a NULL column is dropped when
    /// `no_nulls` (unknownEqFalse: PG builds no `hashnulls`).
    ///
    /// # Safety
    /// As `encode`.
    ///
    /// CONVERT wave 2 (site 7): the table grows fallibly (`try_reserve`: a refusal is `Refused::Memory`), a new key's box
    /// counts in `key_bytes`, and the charge re-syncs once it lags by `SYNC_SLACK`.
    pub(crate) unsafe fn insert(&mut self, row: *const u64, no_nulls: bool) -> Result<(), Refused> {
        let (enc, null) = unsafe { self.encode(row, "m8_subhash_insert", 1).map_err(Refused::Row)? };
        let nullmask = null.iter().enumerate().fold(0u64, |m, (c, &w)| m | (w & 1) << c);
        let len = enc.len() as u64;
        let grow = |cap: usize, entry: usize| Refused::Memory(len + (cap.max(4) * 2 * (entry + 1)) as u64);
        let new = match nullmask {
            0 => {
                let cap = self.exact.capacity();
                self.exact.try_reserve(1).map_err(|_| grow(cap, core::mem::size_of::<Box<[u8]>>()))?; // alloc: Exec charged self.charge
                self.exact.insert(enc.into_boxed_slice()) // alloc: Exec charged self.charge
            }
            _ if no_nulls => false,
            m => {
                let cap = self.nulls.capacity();
                self.nulls.try_reserve(1).map_err(|_| grow(cap, core::mem::size_of::<(Box<[u8]>, u64)>()))?; // alloc: Exec charged self.charge
                self.nulls.insert((enc.into_boxed_slice(), m)) // alloc: Exec charged self.charge
            }
        };
        if new {
            self.key_bytes += len;
            self.sync(SYNC_SLACK);
        }
        Ok(())
    }

    /// `ExecHashSubPlan`'s verdict for the probe row: `TRUE`, `FALSE` or `NULL`.
    ///
    /// # Safety
    /// As `encode`.
    pub(crate) unsafe fn probe(&self, row: *const u64) -> Result<i64, String> {
        let n = self.masks.len();
        let (enc, null) = unsafe { self.encode(row, "m8_subhash_probe", NONE)? };
        if self.exact.is_empty() && self.nulls.is_empty() {
            return Ok(FALSE);
        }
        let nulls = null[..n].iter().filter(|&&w| w == 1).count();
        if nulls == 0 {
            if null[..n].iter().all(|&w| w == 0) && self.exact.contains(enc.as_slice()) {
                return Ok(TRUE);
            }
            return Ok(if self.nulls.iter().any(|(k, m)| self.partial(&enc, &null, k, *m)) { NULL } else { FALSE });
        }
        if nulls == n {
            return Ok(NULL);
        }
        let hit = self.nulls.iter().any(|(k, m)| self.partial(&enc, &null, k, *m)) || self.exact.iter().any(|k| self.partial(&enc, &null, k, 0));
        Ok(if hit { NULL } else { FALSE })
    }
}

impl SetObj {
    /// `m8_subhash_probe` of the several-column set (any width: the sealed set is read, never written).
    pub(crate) unsafe fn probe(&self, row: u64) -> i64 {
        let Some(m) = &self.multi else { return fail("m8_subhash_probe: a one-column set (its probe is the IR's, over its KeyTable)") };
        if self.mseal.get() == 0 {
            if let Err(e) = self.lazy.ensure(&self.mseal) {
                gravity_fn::stash_error(e);
                return ERROR;
            }
        }
        if row == 0 {
            return fail("m8_subhash_probe: a null row");
        }
        // SAFETY: sealed: no insert or reset runs until the statement's next rebuild; `row` is the probe's (the layout `new_set` admitted).
        (*m.as_ptr()).probe(row as *const u64).unwrap_or_else(|e| fail(&e))
    }

    /// D2849: `m8_subhash_exact` of the several-column set: 1 when it holds a row with no NULL key (PG's `havehashrows`:
    /// `ExecHashSubPlan` hashes a probe with no NULL column only then), else 0; a LAZY set is built first.
    pub(crate) unsafe fn exact(&self) -> i64 {
        let Some(m) = &self.multi else { return fail("m8_subhash_exact: a one-column set (its KeyTable is the IR's)") };
        if self.mseal.get() == 0 {
            if let Err(e) = self.lazy.ensure(&self.mseal) {
                gravity_fn::stash_error(e);
                return ERROR;
            }
        }
        // SAFETY: sealed, as `probe`'s read
        i64::from(!(*m.as_ptr()).exact.is_empty())
    }
}
