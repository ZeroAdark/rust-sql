//! D4802 (2): the part cache's content trim lets the unused parts' faces go last (`OpenPart::evict_faces`); the bytes a
//! running statement's holds keep stay in memory until it drops them, yet each trim that reached that tier returned them
//! as freed and counted them as evicted again. With trim_real_frees on, a trim returns and counts only what left memory,
//! so the callers that read its return read real frees: C3's cut (memruntime's purger) frees nothing and is over, the
//! memory ladder's R2a takes nothing from the part cache (no room credit) and walks on to the tail holders, and the trim
//! counters (the ladder's raise demand for the part cache) do not move. A process of its own: the part cache's census,
//! budget and hooks, the knob and the budget's registry are process-wide.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use gravity_driver::mem::budget::{self, Consumer, Hooks};
use gravity_driver::mem::reclaim;
use mempolicy::consts::{set, K};
use pgrc_format::dirlayout::part_file_name;
use pgrc_qa::corpus::{build_fixture, int8_fixture};
use pgrc_read::io::{MemPartIo, PartIo};
use pgrc_read::{partcache, FaceHold, PartExpect};
use pgrc_write::writer::PartCutPolicy;

const MIB: u64 = 1 << 20;
/// The faces a running statement holds, past the cache's 1 MiB budget.
const FACES: u64 = 4 * MIB;

/// A tail holder after every cache (the compiled-code cache's shape: a reclaim only the ladder runs, nothing in use).
static CODE: AtomicU64 = AtomicU64::new(0);

fn code() -> u64 {
    CODE.load(Ordering::Relaxed)
}

fn none_in_use() -> u64 {
    0
}

fn code_trim(to: u64) -> u64 {
    let cur = code();
    if to >= cur {
        return 0;
    }
    CODE.store(to, Ordering::Relaxed);
    cur - to
}

#[test]
fn held_faces_free_nothing_so_the_cut_is_over_and_the_ladder_walks_on_to_the_tail() {
    // the server's part cache setting, 1 MiB (read once, at the first arming)
    std::env::set_var("PGRUST_GRAVITY_PART_CACHE_MB", "1");
    set(K::TrimRealFrees, 1);
    gravity_hosts::memory_budget::register_all();
    assert!(budget::register(Consumer::ProgramCaches, Hooks { current: code, floor: code, cap: code, reclaim: None }));
    budget::register_ladder_reclaim(Consumer::ProgramCaches, none_in_use, code_trim);
    memledger::bind_backend(198, 9802);
    // the server's arming at a statement's start: the cache's hooks (its faces hook drops only the lists' stale holds)
    gravity_hosts::ops::scan::pgrc::residency::arm_part_cache();

    let bf = build_fixture(int8_fixture("gravity_d4802_trim_real", 4802, 20_000, vec![], PartCutPolicy::default(), |i| Some(i as i64)));
    let rec = &bf.manifest.parts[0];
    let io = MemPartIo::new(bf.files.get(&part_file_name(rec.part_no)).expect("part file").clone(), 7, 4802);
    let reg = pgrc_am::inval::registry();
    let pin = reg.open_pinned(&PartExpect::none(), move || Ok(Box::new(io) as Box<dyn PartIo>)).expect("open part");
    let part = Arc::clone(pin.part());
    part.prefault_all_sections().expect("warm");
    // the part's data goes, and that trim meets its target: the cache holds nothing
    reg.trim_contents(0);
    assert_eq!((part.resident(), partcache::bytes()), (0, 0), "the cache is empty");

    // a running statement reads the part's faces: its hold is no list's, so no trim drops it
    let mut held = FaceHold::new(&part, true);
    held.add(FACES);
    assert_eq!(partcache::bytes(), FACES, "the held faces are the cache's bytes");
    let counted = partcache::counters();

    // C3: the cut finds the cache past its budget and trims it to the last tier; the faces stay held, so the cut freed
    // nothing and is over (off: it freed FACES and stayed due, and its next look found the same bytes past the budget)
    let cut = mcx::cache_cut();
    assert_eq!((cut.freed, cut.held), (0, FACES - MIB), "the cut freed nothing; the held faces are past the budget");
    let next = mcx::cache_cut();
    assert_eq!((next.freed, next.held), (0, 0), "the cut is over: the purger's next look trims nothing");

    // R2a with no tail bytes: the part cache's trim frees nothing, so R2a freed nothing and the ladder credits no room
    // (off: R2a returned FACES)
    assert_eq!(reclaim::ladder_reclaim(FACES), 0, "the held faces free nothing");
    // with a tail holder: R2a walks past the part cache and takes the bytes from it (off: the part cache's FACES met the
    // want, and the walk stopped before the tail)
    CODE.store(64 * MIB, Ordering::Relaxed);
    assert_eq!(reclaim::ladder_reclaim(FACES), FACES, "the tail holder gave what R2a asked");
    assert_eq!(code(), 64 * MIB - FACES, "the walk reached the tail");

    assert_eq!(part.face_bytes(), FACES, "the faces stayed held through every trim");
    assert_eq!(partcache::counters(), counted, "no trim counted an eviction: the part cache's raise demand did not move");
    drop(held);
    assert_eq!(partcache::bytes(), 0, "the statement's drop lets them go");
    drop(pin);
}
