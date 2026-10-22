//! `source`'s unsafe steps (rule R6, the module's raw home): the window's lane array written and read through the
//! site's pointer, a cursor's selection / offset-lane / lane reads, the bound node's sources by ctx, and the blank site.
#![allow(unsafe_code)]

use gravity_abi::PtrWord;
use gravity_abi::site::Site;
use gravity_abi::{Datum, Lane, ScanPos};

use super::{Cursor, Source, Window};

impl Window<'_> {
    /// Publish lane `k` (growing `nlanes` to cover it).
    #[inline]
    pub fn set(&mut self, k: usize, l: Lane) {
        unsafe { *self.s.lanes.ptr().add(k) = l };
        if k as u16 >= self.s.nlanes {
            self.s.nlanes = k as u16 + 1;
        }
    }

    pub fn lanes(&self) -> &[Lane] {
        unsafe { core::slice::from_raw_parts(self.s.lanes.ptr(), self.s.nlanes as usize) }
    }

    pub fn lanes_mut(&mut self) -> &mut [Lane] {
        unsafe { core::slice::from_raw_parts_mut(self.s.lanes.ptr(), self.s.nlanes as usize) }
    }

    /// CONVERT wave 2 (R2): the ledger owner and kind of the breaker slot the pipeline driving this window PRODUCES into
    /// (the site env's `Inner::feed_owner`, set at the leader's pipeline open and at a worker's section bind): the owner of
    /// what a source holds for that pipeline. `None` for a pipeline that ends in a sink (its source's bytes are the
    /// statement's: `stmt_owner`), a slot with no owner, or a site with no env (the row protocol's blank site, a rig).
    pub fn op_owner(&self) -> Option<(memledger::Owner, memledger::Kind)> {
        let env = self.s.env.ptr() as *const crate::node::Inner;
        // SAFETY: a bound site's env is the `Inner` its block was bound with (bind.rs), live while a host call runs; one
        // Copy field read through the raw pointer (no reference to the whole `Inner` made)
        if env.is_null() { None } else { unsafe { (*env).feed_owner } }
    }

    /// CONVERT wave 2 (R3): the statement's (or the portal's) ledger owner and release, as the site's Node captured it at its
    /// bind (`Node::apply`; a worker's copy at its section bind); `None` with no env or outside a statement.
    pub fn stmt_owner(&self) -> Option<(memledger::Owner, memledger::Release)> {
        let env = self.s.env.ptr() as *const crate::node::Inner;
        // SAFETY: as `op_owner`
        if env.is_null() { None } else { unsafe { (*env).stmt_owner } }
    }
}

unsafe impl Send for Cursor {}

impl Cursor {
    /// The window row behind iteration index `j`.
    #[inline]
    pub fn index(&self, j: u32) -> usize {
        if self.sel.is_null() {
            j as usize
        } else {
            unsafe { *self.sel.add(j as usize) as usize }
        }
    }

    /// The row's position: the base itself when it is a row's (`POS_ROW`), else the base's block
    /// at the offset lane's entry or `off + i + 1`.
    pub fn pos_of(&self, i: usize) -> ScanPos {
        let b = self.base.pos;
        if b.is_row() {
            return b;
        }
        let off = if self.offset_lane >= 0 { unsafe { *(self.lanes[self.offset_lane as usize].ptr.ptr() as *const u16).add(i) } } else { b.off + i as u16 + 1 };
        ScanPos::new(b.rel, b.block, off)
    }

    /// Serve iteration index `row` into the row buffers and advance.
    #[inline]
    pub fn serve(&mut self, values: &mut [Datum], isnull: &mut [u8]) {
        let i = self.index(self.row);
        for (k, (v, n)) in values.iter_mut().zip(isnull.iter_mut()).enumerate() {
            let l = &self.lanes[k];
            unsafe {
                *v = l.datum(i);
                *n = !l.is_valid(i) as u8;
            }
        }
        self.pos = self.pos_of(i);
        self.row += 1;
    }
}

/// Site `site`'s source on `ctx` (the node its program was bound with), shared; `None` for a site with no source.
/// # Safety
/// `ctx` is bound, and nothing writes that source while the result is live.
pub unsafe fn site_source<'a>(ctx: *mut gravity_abi::Ctx, site: usize) -> Option<&'a (dyn Source + 'static)> {
    crate::hostimpl::inner_of(ctx).sources.get(site)?.as_deref()
}

/// `site_source` for the table open's offer (`entry_runs_offer`), on the claim's own thread.
/// # Safety
/// `ctx` is the bound ctx of the running claim, and no other reference to that source is live while the result is.
pub unsafe fn site_source_mut<'a>(ctx: *mut gravity_abi::Ctx, site: usize) -> Option<&'a mut (dyn Source + 'static)> {
    crate::hostimpl::inner_of(ctx).sources.get_mut(site)?.as_deref_mut()
}

/// A stack `Site` for the row protocol's default `next()`: only `lanes` is set.
pub fn blank_site(lanes: *mut Lane) -> Site {
    // SAFETY: every field of `Site` is a pointer, an integer or `Option<fn>`, all valid zeroed.
    let mut s: Site = unsafe { core::mem::zeroed() };
    s.lanes = gravity_abi::word(lanes);
    s
}
