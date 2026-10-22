//! `source`'s unsafe half (rule R6, the module's raw home): its host entries and the raw-pointer steps they take,
//! moved verbatim from source.rs (private ones pub(super)); it imports them back.
#![allow(unsafe_code)]

use gravity_abi::PtrWord;
use super::*;

gravity_abi::host! {
    /// `Site.next` over the concrete `S` behind `src`: the seek when the site is not open, then the
    /// next window in place.
    /// # Safety — `s` is a site bound by `bind::sites` with `src` an `S`.
    pub unsafe fn next<S: Source>(s: *mut Site) -> i64 {
        guarded((*s).env.ptr(), || {
            let st = &mut *s;
            let src = &mut *(st.src.ptr() as *mut S);
            if st.state != site::STATE_OPEN {
                let keys = core::slice::from_raw_parts(st.keys.ptr(), st.nkeys as usize);
                let nulls = core::slice::from_raw_parts(st.keys_null.ptr(), st.nkeys as usize);
                if let Err(e) = src.seek(keys, nulls) {
                    return fail(inner(st.env.ptr()), e);
                }
                // skewsplit (D3320): a range site sought in a pool claim that carries a cut (the claim's node's own)
                let n = inner(st.env.ptr());
                if let Some(drive) = n.claim_cut {
                    let site = (s as usize - (*n.ctx).sites.ptr() as usize) / gravity_abi::SITE_SIZE;
                    if n.descs[site].is_range() {
                        if let Some(Err(e)) = crate::claimcut::with_current(|cut| src.claim_cut(cut, site as u32 == drive)) {
                            return fail(inner(st.env.ptr()), e);
                        }
                    }
                }
                st.state = site::STATE_OPEN;
            }
            let mut w = Window::over(st);
            match src.window(&mut w) {
                Ok(rows) => {
                    w.rows_in += rows as u64;
                    rows as i64
                }
                Err(e) => fail(inner(st.env.ptr()), e),
            }
        }, |status| status)
    }
}

gravity_abi::host! {
    /// # Safety — As `next`.
    pub unsafe fn lane<S: Source>(s: *mut Site, k: u32) -> i64 {
        let st = &mut *s;
        let src = &mut *(st.src.ptr() as *mut S);
        let n = inner(st.env.ptr());
        let site = (s as usize - (*n.ctx).sites.ptr() as usize) / gravity_abi::SITE_SIZE;
        // `LANE_DONE` (a swept program table published by the program) and `LANE_CLAIM` (D3115: its next sweep chunk) ride
        // above the lane index
        let at = (k & !(gravity_abi::site::LANE_DONE | gravity_abi::site::LANE_CLAIM)) as usize;
        if at >= n.descs[site].nlanes() {
            return fail(n, Error::BadReturn(k as i64));
        }
        let sel = if st.sel.ptr().is_null() { &[][..] } else { core::slice::from_raw_parts(st.sel.ptr(), st.sel_n as usize) };
        let out = &mut *st.lanes.ptr().add(at);
        guarded(st.env.ptr(), || match src.lane(k as u16, sel, out) {
            Ok(n) => n as i64,
            Err(e) => fail(inner(st.env.ptr()), e),
        }, |status| status)
    }
}

gravity_abi::host! {
    /// # Safety — As `next`.
    pub unsafe fn fetch<S: Source>(s: *mut Site, nrows: u32) -> i64 {
        let st = &mut *s;
        let src = &mut *(st.src.ptr() as *mut S);
        let lanes = core::slice::from_raw_parts_mut(st.lanes.ptr() as *mut Lane, st.nlanes.max(1) as usize);
        // the request: `nrows` RowId words at the site's rowid lane (`ID_ROWID`; the kernel points it at its array)
        let req = lanes.iter().find(|l| gravity_abi::ctx::lane::id_kind(l.id) == gravity_abi::ctx::lane::ID_ROWID && !l.ptr.ptr().is_null()).map_or(core::ptr::null(), |l| l.ptr.ptr() as *const u64);
        let rowids = if req.is_null() { &[][..] } else { core::slice::from_raw_parts(req, nrows as usize) };
        guarded(st.env.ptr(), || match src.fetch(rowids, lanes) {
            Ok(n) => n as i64,
            Err(e) => fail(inner(st.env.ptr()), e),
        }, |status| status)
    }
}

gravity_abi::host! {
    /// # Safety — `s` is a bound site.
    pub unsafe fn trap_next(s: *mut Site) -> i64 {
        fail(inner((*s).env.ptr()), Error::Unsupported(UNSUPPORTED_ON_WORKER));
        unsupported(UNSUPPORTED_ON_WORKER)
    }
}

gravity_abi::host! {
    /// # Safety — `s` is a bound site.
    pub unsafe fn trap_lane(s: *mut Site, _k: u32) -> i64 {
        trap_next(s)
    }
}

gravity_abi::host! {
    /// # Safety — As `trap_next`.
    pub unsafe fn trap_dict(s: *mut Site, _part: u32, _lane: u32, _code: u32, _out: *mut u64) -> i64 {
        trap_next(s)
    }
}

gravity_abi::host! {
    /// The `Site.dict` slot: `Source::dict_value` for an emit's winners (the datum into `out`).
    /// # Safety — As `next`; `out` is writable.
    pub unsafe fn dict_value<S: Source>(s: *mut Site, part: u32, lane: u32, code: u32, out: *mut u64) -> i64 {
        let st = &mut *s;
        let src = &mut *(st.src.ptr() as *mut S);
        guarded(st.env.ptr(), || match src.dict_value(part, lane as u16, code) {
            Ok((d, _)) => {
                *out = d;
                0
            }
            Err(e) => fail(inner(st.env.ptr()), e),
        }, |status| status)
    }
}
