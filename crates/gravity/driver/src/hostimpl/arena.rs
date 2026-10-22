use gravity_abi::Ctx;

use super::{guarded, inner_of};

mod raw;
pub use raw::{grow, object_map, object_overdraw, object_region, pin_host, spill_epoch, spill_region};
pub(crate) use raw::{file_slot_pin, give_owner_pin, map_owners, owner_wall_of, pin, pin_batch, pin_owner, pin_room, pin_room_one, spill_region_now, spill_region_spills, spill_region_unzeroed, spill_region_within, unmap_owners};

pub use gravity_program::cost::runtime::HOST_ARENA_CHUNK as CHUNK;

/// D642 (Michael's free-memory preference): a statement's arena chunks at its end. Its pins (`[0..pinned)`, never under a
/// chunk count) and the first `PARK_CHUNKS` grow chunks after them go to the depot as before (the next statement's pins
/// and grows reuse them: a text `min()` group-by's images re-fault 18 MB a run otherwise); every grow chunk past them, growth only
/// the old 64-chunk cap refused, has its pages returned to the kernel and is dropped. H3 step (4) C1-b (give_back_spill):
/// through the give-back service as one job each when it takes them (`mem::giveback::give`), else here.
pub(crate) fn settle_grow_chunks(n: &mut crate::node::Inner) {
    let keep = n.pinned + crate::node::PARK_CHUNKS;
    // a label-only step (no tick): a long settle is checker 2's under its own label
    let _step = cancelpoll::step("arena settle_grow_chunks: a statement's grown arena chunks parked or returned");
    // bounded("other: the statement's end settles every arena chunk the ctx owner grew: each pass only parks one chunk or returns its pages (one madvise) and reads no row (a chunk is CHUNK bytes or more); a teardown runs to its end (a stop would leave chunks neither parked nor returned), as PG's MemoryContextReset at the executor's end polls no deeper")
    for (i, c) in n.chunks.drain(..).enumerate() {
        if i < keep {
            crate::park::REGIONS.put(c);
        } else {
            crate::mem::giveback::give(c);
        }
    }
}

/// D664: every spill epoch is drawn from one process-wide counter, so a Node re-created at a dropped Node's address
/// never shows an epoch a lane recorded on the old one (the counter starts at 1: a fresh lane's 0 is never current).
static SPILL_EPOCHS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// D664: a spill epoch no owner has shown before.
pub(crate) fn next_spill_epoch() -> u64 {
    SPILL_EPOCHS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// D641 as amended by D2687 (the free-memory preference's MADV_FREE-on-park form): a statement's spill regions at its end.
/// `spill_epoch` moves first, so every holder of a spill address forgets it before its next use (`spill_epoch`'s
/// readers); then each region's pages are handed back lazily (MADV_FREE: the kernel takes them under memory pressure)
/// and the region parks in the depot, so the next statement's spill or arena draw reuses resident pages instead of
/// faulting fresh ones (D641 returned and dropped them: every spill-heavy statement re-faulted its whole spill). The
/// depot holds parked bytes under the grant pool's park budget (D405: least recently put evicted first) and
/// `REGION_DEPOT_BYTES`, so a session keeps at most that much resident between statements. Debug builds make the parked
/// pages inaccessible until a take (`guard_parked`): a holder that kept a spill address faults at its first touch.
pub(crate) fn drop_result_spills(n: &mut crate::node::Inner) {
    n.spill_epoch = next_spill_epoch();
    n.bigs.clear();
    // D491: the serial owners' mapped temp files, read by nothing past the statement
    n.owner_maps.clear();
    n.work_maps.clear();
    n.pin_map = (0, 0);
    // a label-only step (no tick): a long hand-back is checker 2's under its own label
    let _step = cancelpoll::step("arena drop_result_spills: a statement's spill regions handed back and parked");
    // bounded("other: the statement's end hands back every spill region the ctx owner drew: each pass only frees one region's pages lazily (one MADV_FREE) and parks it, and reads no row; a teardown runs to its end (a stop would leave regions unparked past their epoch), as PG's MemoryContextReset at the executor's end polls no deeper")
    for mut c in n.spills.drain(..) {
        c.park_free();
        crate::node::guard_parked(&c);
        crate::park::REGIONS.put(c);
    }
}

#[cold]
fn refused(n: &crate::node::Inner, want: usize) {
    if crate::env::set("PGRUST_GRAVITY_REPORT") {
        let pool = &crate::mem::grant::POOL;
        eprintln!("gravity: pin refused: want={} w={} reserve={} held={} capacity={} chunks={} pinned={}", want, n.w, n.stmt().map_or(0, |s| s.left()), pool.held(), pool.capacity(), n.chunks.len(), n.pinned);
    }
}
