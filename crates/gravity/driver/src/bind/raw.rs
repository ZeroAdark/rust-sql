//! `bind`'s unsafe step (rule R6, the module's raw home): the per-site targets written into a built block's sites.
#![allow(unsafe_code)]

use gravity_abi::site::Site;

use crate::hostimpl;
use crate::node::{Inner, Role};

/// A worker's non-range site traps (an index probe is not served on a worker), unless its source serves a worker (D3014O:
/// a frozen-image btree probe, `Source::serves_worker`).
/// # Safety — `inner.ctx` has `descs.len()` sites and `inner.sources` is filled.
pub(crate) unsafe fn sites(inner: *mut Inner, role: Role) {
    // D1706: the Inner's own pointer (`build_image`'s), which each site hands its hosts as `env`
    let env = inner as *mut u8;
    let inner = &mut *inner;
    for i in 0..inner.descs.len() {
        let d = &inner.descs[i];
        let s: *mut Site = gravity_abi::ctx::site_ptr(inner.ctx, i);
        (*s).env = gravity_abi::word(env);
        (*s).nkeys = d.nkeys() as u16;
        (*s).natts = d.natts() as u16;
        let Some(src) = inner.sources[i].as_mut() else { continue };
        (*s).src = gravity_abi::word(src.data_ptr());
        if matches!(role, Role::Worker) && !d.is_range() && !src.serves_worker() {
            (*s).next = gravity_abi::word(Some(hostimpl::source::trap_next));
            (*s).lane = gravity_abi::word(Some(hostimpl::source::trap_lane));
            (*s).fetch = gravity_abi::word(Some(hostimpl::source::trap_lane));
            (*s).dict = gravity_abi::word(Some(hostimpl::source::trap_dict));
        } else {
            (*s).next = gravity_abi::word(src.calls.next);
            (*s).lane = gravity_abi::word(src.calls.lane);
            (*s).fetch = gravity_abi::word(src.calls.fetch);
            (*s).dict = gravity_abi::word(src.calls.dict);
        }
    }
}
