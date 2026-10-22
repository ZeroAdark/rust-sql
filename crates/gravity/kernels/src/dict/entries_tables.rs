//! M1-TABLES' host entry: road `dict.entries`' runs handshake. The plain Fp128 pair's table open
//! (lowerv4 keyed/table_size.rs `open_with`) offers the pair layout and the radix bits to the driving site's source; hosts
//! granule_source/entry_runs.rs installs the source side. An offer never fails the claim: no hook, no source or a rejected offer leaves
//! every part publishing its rows. Created by M1 (owner M1-TABLES).
use core::sync::atomic::{AtomicUsize, Ordering};

use gravity_abi::breaker::RecordLayout;
use gravity_abi::Ctx;

/// The source side of the offer: site `site` of `ctx`'s source sees the layout and bits. It never unwinds: it only
/// records whether the source accepts.
pub type Offer = unsafe fn(*mut Ctx, usize, &RecordLayout, u8);

/// The installed `Offer` as an address (0: none), set once (no lock: D238).
static ENTRY_RUNS_OFFER: AtomicUsize = AtomicUsize::new(0);

/// Install the source side (hosts granule_source/entry_runs.rs `install`); the first install wins.
pub fn set_entry_runs_offer(f: Offer) {
    let _ = ENTRY_RUNS_OFFER.compare_exchange(0, f as usize, Ordering::AcqRel, Ordering::Acquire);
}

crate::contract::host! {
    /// `entry_runs_offer(ctx, site, layout, bits)`: the claim's table over `layout` (a stack slot live for the call) offers
    /// it at `bits` radix bits to site `site`'s source. Always 0.
    /// # Safety
    /// `ctx` is bound; `layout` points at a live `RecordLayout`.
    pub unsafe fn entry_runs_offer(ctx: *mut Ctx, site: u64, layout: *const RecordLayout, bits: u64) -> i64 {
        let p = ENTRY_RUNS_OFFER.load(Ordering::Acquire);
        if p != 0 {
            // SAFETY: a non-zero word is an `Offer` `set_entry_runs_offer` stored. The source's offer runs under `guard!` (V4R-16).
            let f = core::mem::transmute::<usize, Offer>(p);
            guard!((), f(ctx, site as usize, &*layout, bits as u8));
        }
        0
    }
}
