//! `dml`'s unsafe half (rule R6, the module's raw home): the write and lock host entries and the raw site reads
//! and stores they make, moved from dml.rs; `writer_at!`, `owns` and `value` stay there.
#![allow(unsafe_code)]

use gravity_abi::PtrWord;
use super::*;

gravity_abi::host! {
    /// `Program.write(ctx, op, site)`: the row in site `site`'s `values` / `isnull` at its `pos`.
    /// # Safety — `ctx` is a bound ctx of a writing program.
    pub unsafe fn write(ctx: *mut Ctx, op: u32, site: u64) -> i64 {
        if op == gravity_abi::program::WRITE_SUB_ARGS {
            return guarded((*(*ctx).program.ptr()).env.ptr(), || sub_in(ctx, site as usize), |status| status);
        }
        // C9: the row op is one labelled Step of the kernel's write call; the kernel's row loop polls between two calls
        let _step = cancelpoll::step(match op & 0xff {
            gravity_abi::program::WRITE_INSERT => "dml insert",
            gravity_abi::program::WRITE_UPDATE => "dml update",
            gravity_abi::program::WRITE_DELETE => "dml delete",
            _ => "dml write",
        });
        guarded((*(*ctx).program.ptr()).env.ptr(), || write_in(ctx, op, site), |status| status)
    }
}

/// D1112: the parked check's SubPlan call, its words' address (nonnegative) for the IR.
/// # Safety — As `write`.
unsafe fn sub_in(ctx: *mut Ctx, site: usize) -> i64 {
    let n = inner_of(ctx);
    let Some(w) = writer_at!(n, site) else { return fail(n, Error::host("write host on a read-only program")) };
    match w.sub_call() {
        Ok(p) => p as i64,
        Err(e) => fail(n, e),
    }
}

/// # Safety — As `write`.
#[inline(always)]
unsafe fn write_in(ctx: *mut Ctx, op: u32, site: u64) -> i64 {
    let n = inner_of(ctx);
    let i = site as usize;
    if i >= n.descs.len() {
        return fail(n, Error::BadReturn(site as i64));
    }
    let d = &n.descs[i];
    let s = site_ptr(ctx, i);
    if op == gravity_abi::program::WRITE_EPQ_ROW {
        // D655 Row C: the newest version the last `Recheck` locked, into the site's cells and position
        let Some(w) = writer_at!(n, i) else { return fail(n, Error::host("write host on a read-only program")) };
        let values = core::slice::from_raw_parts_mut((*s).values.ptr() as *mut _, d.natts());
        let isnull = core::slice::from_raw_parts_mut((*s).isnull.ptr() as *mut _, d.natts());
        return match w.epq_row(values, isnull) {
            Ok(p) => {
                (*s).pos = p;
                WRITE_DONE
            }
            Err(e) => fail(n, e),
        };
    }
    if op == gravity_abi::program::WRITE_CONFLICT_CHECK {
        let Some(w) = writer_at!(n, i.wrapping_sub(1)) else { return fail(n, Error::host("write host on a read-only program")) };
        return match w.conflict_check() {
            Ok(()) => WRITE_DONE,
            Err(e) => fail(n, e),
        };
    }
    // D1112: the row op's resume after its check's SubPlan read (the site's row and position unchanged)
    let resume = op & gravity_abi::program::WRITE_RESUME != 0;
    let Some(op) = WriteOp::from_u32(op & !gravity_abi::program::WRITE_RESUME) else { return fail(n, Error::BadReturn(op as i64)) };
    let pos = (*s).pos;
    // D1687: only the program's own write counts its rows for the tag and stops for the runner's recheck
    let main = !n.writers.iter().any(|(s, _)| owns(*s, i));
    let Some(w) = writer_at!(n, i) else { return fail(n, Error::host("write host on a read-only program")) };
    let o = {
        let values = core::slice::from_raw_parts((*s).values.ptr(), d.natts());
        let isnull = core::slice::from_raw_parts((*s).isnull.ptr(), d.natts());
        match if resume { w.resume(op, pos, values, isnull) } else { w.write(op, pos, values, isnull) } {
            Ok(o) => o,
            Err(e) => return fail(n, e),
        }
    };
    if matches!(o, Outcome::Done | Outcome::Conflict) {
        // the written row back into the site's cells when it is not the kernel's (a BEFORE ROW trigger's, the stored
        // generated values): RETURNING reads them; after a Conflict they are ON CONFLICT DO UPDATE's EXCLUDED, the row
        // ExecInsert formed before its arbiter check
        let wv = core::slice::from_raw_parts_mut((*s).values.ptr() as *mut _, d.natts());
        let wn = core::slice::from_raw_parts_mut((*s).isnull.ptr() as *mut _, d.natts());
        w.written(wv, wn);
    }
    let rpos = w.recheck_pos();
    if o == Outcome::Conflict {
        // the conflict site is the write site's successor: the locked row's cells and position
        let j = i + 1;
        if j >= n.descs.len() {
            return fail(n, Error::BadReturn(j as i64));
        }
        let dj = &n.descs[j];
        let c = site_ptr(ctx, j);
        let cv = core::slice::from_raw_parts_mut((*c).values.ptr() as *mut _, dj.natts());
        let cn = core::slice::from_raw_parts_mut((*c).isnull.ptr() as *mut _, dj.natts());
        match w.existing(cv, cn) {
            Ok(p) => (*c).pos = p,
            Err(e) => return fail(n, e),
        }
    }
    if o == Outcome::Done && op == WriteOp::Update && i + 1 < n.descs.len() {
        // a plan that fetches the replaced version (WritePlan.old: RETURNING OLD, the host's flag) fills its
        // site, the write site's successor, which the lowerer declared; any other writer answers false and writes nothing
        let (j, c) = (i + 1, site_ptr(ctx, i + 1));
        let nj = n.descs[j].natts();
        let cv = core::slice::from_raw_parts_mut((*c).values.ptr() as *mut _, nj);
        let cn = core::slice::from_raw_parts_mut((*c).isnull.ptr() as *mut _, nj);
        if let Err(e) = w.old(cv, cn) {
            return fail(n, e);
        }
    }
    let so = w.sys_off();
    if o == Outcome::Done && so > 0 && i + so < n.descs.len() {
        // D774: the written row's system columns (WritePlan.sys: RETURNING ctid / xmin / ..) into their site, which the
        // lowerer declared after the write site's others (write/rowop_step.rs)
        let (j, c) = (i + so, site_ptr(ctx, i + so));
        let nj = n.descs[j].natts();
        let cv = core::slice::from_raw_parts_mut((*c).values.ptr() as *mut _, nj);
        let cn = core::slice::from_raw_parts_mut((*c).isnull.ptr() as *mut _, nj);
        if let Err(e) = w.sys(cv, cn) {
            return fail(n, e);
        }
    }
    if o == Outcome::Done && !n.counts_pushed && main {
        n.binding().add_rows(1);
    }
    if o == Outcome::Recheck && main {
        n.recheck_pos = rpos;
    }
    value(o)
}

gravity_abi::host! {
    /// `Program.lock(ctx, arg)`: lock the row at site `arg`'s `pos` (a LockRows over several locking marks: `arg` also names
    /// the mark's lock, `LockSite::arg`).
    /// # Safety — `ctx` is a bound ctx of a locking program.
    pub unsafe fn lock(ctx: *mut Ctx, site: u64) -> i64 {
        guarded((*(*ctx).program.ptr()).env.ptr(), || lock_in(ctx, site), |status| status)
    }
}

/// # Safety — As `lock`.
#[inline(always)]
unsafe fn lock_in(ctx: *mut Ctx, arg: u64) -> i64 {
    let n = inner_of(ctx);
    // LockRows REFFETCH (`LockSite::FETCH`): the REFERENCE-marked row at the site's `pos`, fetched by the lock host
    // (EvalPlanQualFetchRowMark) into the site's cells, its position stored; nothing is locked
    let fetch = arg & gravity_program::program::LockSite::FETCH != 0;
    let arg = arg & !gravity_program::program::LockSite::FETCH;
    let (i, mark) = ((arg & 0xffff_ffff) as usize, (arg >> gravity_program::program::LockSite::SHIFT) as usize);
    if i >= n.descs.len() || fetch && !matches!(n.descs[i].kind, gravity_program::SiteKind::Write { .. }) {
        return fail(n, Error::BadReturn(arg as i64));
    }
    let s = site_ptr(ctx, i);
    let pos = (*s).pos;
    // only a write-kind lock site (write.lock's SiteDesc::write) takes the traversed version's cells; a lock over a
    // scan site keeps the scan's cells
    let natts = if matches!(n.descs[i].kind, gravity_program::SiteKind::Write { .. }) { n.descs[i].natts() } else { 0 };
    let Some(l) = n.lockers.get_mut(mark.saturating_sub(1)).filter(|l| (mark == 0) == l.rel.is_none()).map(|l| &mut l.host) else { return fail(n, Error::host("lock host on a program without that lock")) };
    if fetch {
        let cv = core::slice::from_raw_parts_mut((*s).values.ptr() as *mut _, natts);
        let cn = core::slice::from_raw_parts_mut((*s).isnull.ptr() as *mut _, natts);
        return match l.fetch(pos, cv, cn) {
            Ok(p) => {
                (*s).pos = p;
                WRITE_DONE
            }
            Err(e) => fail(n, e),
        };
    }
    match l.lock(pos) {
        Ok(o) => {
            let rpos = l.recheck_pos();
            if o == Outcome::Recheck {
                n.recheck_pos = rpos;
                // a lock site with cells takes the traversed newest version (the EPQ test tuple), as write_in's
                // Conflict fills the conflict site; the host never locks again for it (D604)
                if natts > 0 {
                    let cv = core::slice::from_raw_parts_mut((*s).values.ptr() as *mut _, natts);
                    let cn = core::slice::from_raw_parts_mut((*s).isnull.ptr() as *mut _, natts);
                    match l.existing(cv, cn) {
                        Ok(p) => (*s).pos = p,
                        Err(e) => return fail(n, e),
                    }
                } else if mark > 0 {
                    // multilock: the site's position is the mark's EPQ test tuple (the newest version), which the stopped
                    // node's settle records for the runner's recheck (the one-lock form's is `recheck_pos`)
                    (*s).pos = rpos;
                }
            }
            value(o)
        }
        Err(e) => fail(n, e),
    }
}

gravity_abi::host! {
    /// A worker's program has no write or lock.
    /// # Safety — `ctx` is a bound ctx.
    pub unsafe fn trap_write(ctx: *mut Ctx, _op: u32, _site: u64) -> i64 {
        fail(inner_of(ctx), Error::Unsupported(gravity_abi::status::UNSUPPORTED_ON_WORKER))
    }
}

gravity_abi::host! {
    /// # Safety — `ctx` is a bound ctx.
    pub unsafe fn trap_lock(ctx: *mut Ctx, _site: u64) -> i64 {
        fail(inner_of(ctx), Error::Unsupported(gravity_abi::status::UNSUPPORTED_ON_WORKER))
    }
}
