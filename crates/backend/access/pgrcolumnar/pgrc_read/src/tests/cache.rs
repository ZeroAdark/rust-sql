//! Part cache gates (§5 M3-F): equal identity ⇒ identical bytes (shared
//! entries), pin + LRU janitor under budget pressure, no eviction of pinned
//! parts, eviction safety for in-flight holders.

use std::sync::Arc;

use pgrc_format::abi::{ByteArena, DecodeOut};
use pgrc_format::part::StreamRole;

use crate::cursor::{reference_binding_leaked, StreamCursor};
use crate::io::PartIo;
use crate::openpart::{PartExpect, SegBuf};
use crate::registry::{PartPin, PartRegistry};
use crate::testpart::{build_part, seq_i64_col, BuiltPart, PartSpec};

use super::ArenaBuf;

fn small_part() -> BuiltPart {
    build_part(&PartSpec::new(9_000, vec![seq_i64_col(1, 9_000)]))
}

fn opener(b: &BuiltPart, dev: u64, ino: u64) -> impl FnOnce() -> crate::ReadResult<Box<dyn PartIo>> {
    let io = b.mem_io(dev, ino);
    move || Ok(Box::new(io) as Box<dyn PartIo>)
}

/// D4164: the tests that move the process-wide statement epoch (`partcache::statement_boundary`) run one at a time. cargo
/// runs one binary's tests on parallel threads, and a boundary from another test between one test's uses put its later
/// use in a newer statement (d3109's trim order read [3, 2, 0, 1] for [3, 2, 1, 0]). Every test that calls
/// `statement_boundary` holds this for its whole body.
static EPOCH_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn epoch_test() -> std::sync::MutexGuard<'static, ()> {
    EPOCH_TESTS.lock().unwrap_or_else(|p| p.into_inner())
}

#[test]
fn equal_identity_shares_one_entry() {
    let b = small_part();
    let reg = PartRegistry::new(u64::MAX);
    let p1 = reg
        .open_pinned(&PartExpect::none(), opener(&b, 1, 10))
        .expect("open 1");
    let p2 = reg
        .open_pinned(&PartExpect::none(), opener(&b, 1, 10))
        .expect("open 2");
    assert!(
        Arc::ptr_eq(p1.part(), p2.part()),
        "equal identity must share one parsed entry"
    );
    assert_eq!(reg.len(), 1);
    let c = reg.counters();
    assert_eq!((c.hits, c.misses), (1, 1), "second open is a stat-only hit");
}

#[test]
fn copied_part_gets_a_new_identity() {
    let b = small_part();
    let reg = PartRegistry::new(u64::MAX);
    let p1 = reg
        .open_pinned(&PartExpect::none(), opener(&b, 1, 10))
        .expect("open");
    // Same bytes, different ino: a COPY — spec §11: new identity, own entry.
    let p2 = reg
        .open_pinned(&PartExpect::none(), opener(&b, 1, 11))
        .expect("open copy");
    assert!(!Arc::ptr_eq(p1.part(), p2.part()));
    assert_ne!(p1.part().uuid(), p2.part().uuid());
    assert_eq!(reg.len(), 2);
}

#[test]
fn janitor_evicts_lru_unpinned_and_never_pinned() {
    let b = small_part();
    let reg = PartRegistry::new(u64::MAX);
    let p1 = reg
        .open_pinned(&PartExpect::none(), opener(&b, 1, 21))
        .expect("p1");
    let p2 = reg
        .open_pinned(&PartExpect::none(), opener(&b, 1, 22))
        .expect("p2");
    let p3 = reg
        .open_pinned(&PartExpect::none(), opener(&b, 1, 23))
        .expect("p3");
    for p in [&p1, &p2, &p3] {
        p.part().prefault_all_sections().expect("warm");
    }
    assert!(reg.resident_bytes() > 0);
    // Unpin p1 and p2; touch p1 so p2 is the LRU victim.
    let key1 = {
        let i = p1.part().ident();
        (i.dev, i.ino, i.len)
    };
    let key2 = {
        let i = p2.part().ident();
        (i.dev, i.ino, i.len)
    };
    let key3 = {
        let i = p3.part().ident();
        (i.dev, i.ino, i.len)
    };
    drop(p1);
    drop(p2);
    reg.get(key1).expect("touch p1");
    // Budget forces eviction of ALL unpinned; pinned p3 must survive.
    reg.set_budget(0);
    reg.maintain();
    assert!(reg.get(key2).is_none(), "LRU unpinned p2 evicted");
    assert!(reg.get(key1).is_none(), "then p1");
    assert!(reg.get(key3).is_some(), "pinned p3 NEVER evicted");
    let c = reg.counters();
    assert_eq!(c.evictions, 2);
    assert!(
        c.over_budget_events >= 1,
        "running over budget under pin pressure is witnessed"
    );
    // After the pin drops, the janitor may reclaim it.
    drop(p3);
    reg.maintain();
    assert!(reg.get(key3).is_none(), "unpinned p3 reclaimed");
    assert_eq!(reg.len(), 0);
}

#[test]
fn partial_budget_keeps_recently_used() {
    let b = small_part();
    let reg = PartRegistry::new(u64::MAX);
    let pins: Vec<PartPin> = (0..3)
        .map(|i| {
            let p = reg
                .open_pinned(&PartExpect::none(), opener(&b, 2, 30 + i))
                .expect("open");
            p.part().prefault_all_sections().expect("warm");
            p
        })
        .collect();
    let per_part = reg.resident_bytes() / 3;
    let keys: Vec<_> = pins
        .iter()
        .map(|p| {
            let i = p.part().ident();
            (i.dev, i.ino, i.len)
        })
        .collect();
    drop(pins);
    // Budget for two parts: exactly one (the LRU) goes.
    reg.set_budget(per_part * 2 + per_part / 2);
    reg.get(keys[1]).expect("touch");
    reg.get(keys[2]).expect("touch");
    reg.maintain();
    assert!(reg.get(keys[0]).is_none(), "oldest evicted");
    assert!(reg.get(keys[1]).is_some());
    assert!(reg.get(keys[2]).is_some());
    assert_eq!(reg.counters().evictions, 1);
}

#[test]
fn eviction_is_safe_for_inflight_holders() {
    // Eviction removes the map entry, never the bytes: an in-flight holder
    // keeps decoding, and its section pointers stay generation-stable.
    let b = small_part();
    let reg = PartRegistry::new(u64::MAX);
    let pin = reg
        .open_pinned(&PartExpect::none(), opener(&b, 3, 40))
        .expect("open");
    let part = pin.part().clone();
    let binding = reference_binding_leaked();
    let mut cur = StreamCursor::open(part.clone(), binding, 1, 0).expect("cursor");
    let key = {
        let i = part.ident();
        (i.dev, i.ino, i.len)
    };
    drop(pin);
    reg.set_budget(0);
    reg.maintain();
    assert!(reg.get(key).is_none(), "entry evicted");
    // The held Arc still decodes correctly.
    let mut d = vec![0u64; 8192];
    let mut ab = ArenaBuf::new(1 << 20);
    let mut out = DecodeOut {
        datums: &mut d,
        arena: ByteArena::new(ab.bytes_mut()),
    };
    let n = cur.decode_full(0, &mut out).expect("decode after eviction");
    assert_eq!(n, 8192);
}

#[test]
fn entry_count_cap_bounds_zero_resident_entries() {
    // Regression: entries that are opened but never decoded have
    // resident()==0, so the byte-budget janitor never reclaims them even
    // though each pins a live kernel fd. A stream of such opens (e.g. a
    // self-scan of an aborted publish, or a zero-row scan) must not grow the
    // registry without bound: the entry-count cap reclaims LRU-unpinned
    // entries regardless of resident bytes.
    let b = small_part();
    let reg = PartRegistry::new(u64::MAX); // byte budget can never bite
    reg.set_max_entries(2);
    assert_eq!(reg.resident_bytes(), 0, "no part decoded: zero resident");
    // Open many distinct identities, dropping each pin so it is reclaimable.
    for ino in 0..64u64 {
        let pin = reg
            .open_pinned(&PartExpect::none(), opener(&b, 7, 1000 + ino))
            .expect("open");
        drop(pin);
        assert!(
            reg.len() <= 2,
            "entry count stays bounded by the cap ({} entries)",
            reg.len()
        );
    }
    assert_eq!(reg.resident_bytes(), 0, "still nothing decoded");
    assert!(
        reg.counters().evictions >= 62,
        "the cap drove count-based eviction of zero-resident entries"
    );
}

#[test]
fn a_registry_of_pinned_parts_skips_its_janitor_until_a_pin_drops() {
    // P-1 L4a: a generation pinning more parts than the entry cap made every
    // open walk every entry twice. A pass that finds every entry pinned is
    // remembered until a pin drops to zero anywhere; the next pass then
    // evicts as before.
    let b = small_part();
    let reg = PartRegistry::new(u64::MAX);
    reg.set_max_entries(2);
    let mut pins: Vec<PartPin> = (0..5u64)
        .map(|i| {
            reg.open_pinned(&PartExpect::none(), opener(&b, 8, 2000 + i))
                .expect("open")
        })
        .collect();
    assert_eq!(
        (reg.len(), reg.counters().evictions),
        (5, 0),
        "every entry pinned: none evicted"
    );
    let events = reg.counters().over_budget_events;
    reg.maintain();
    assert_eq!(
        reg.counters().over_budget_events,
        events + 1,
        "a pass over the cap witnesses the overrun"
    );
    let clone = pins[1].clone();
    drop(clone);
    reg.maintain();
    assert_eq!(
        (reg.len(), reg.counters().evictions),
        (5, 0),
        "a pin dropped to one leaves its entry pinned"
    );
    drop(pins.remove(0));
    reg.maintain();
    assert_eq!(
        (reg.len(), reg.counters().evictions),
        (4, 1),
        "the unpinned entry goes at the next pass"
    );
    drop(pins);
    reg.maintain();
    assert_eq!(reg.len(), 2, "and the rest down to the cap once unpinned");
}

/// D4537 T18c, under trunk's D4552a form (r15): the registry's own bytes are the part cache's by construction. The in-use
/// buffer leaves the building thread's account at once (its holder charge); an entry's map node is allocated and freed
/// on the part cache's home, so an invalidation on another thread moves no bytes on that thread's account (D4552's class:
/// a free elsewhere drove a session's account below zero). The parts' contents are their own charges.
#[test]
fn the_registrys_own_bytes_are_on_the_part_cache() {
    let b = small_part();
    std::thread::spawn(move || {
        memledger::bind_backend(4537, 9000 + 4537);
        let a = memledger::Acct::backend(4537);
        let ((), _) = super::door1(|| {
            memledger::fold();
            let before = memledger::account_row(a).account_bytes;
            let reg = PartRegistry::new(u64::MAX);
            memledger::fold();
            let built = memledger::account_row(a).account_bytes;
            assert!((built - before).abs() < 1024, "the in-use buffer is the part cache's: {} B on the builder", built - before);
            let pin = reg.open_pinned(&PartExpect::none(), opener(&b, 4, 51)).expect("open");
            let key = {
                let i = pin.part().ident();
                (i.dev, i.ino, i.len)
            };
            memledger::fold();
            let open = memledger::account_row(a).account_bytes;
            assert!(reg.invalidate(key));
            memledger::fold();
            let gone = memledger::account_row(a).account_bytes;
            assert_eq!(gone, open, "the entry's node was the part cache's, and so is its free");
            drop(pin);
            drop(reg);
        });
        memledger::unbind_backend();
    })
    .join()
    .unwrap();
}

#[test]
fn invalidate_and_clear() {
    let b = small_part();
    let reg = PartRegistry::new(u64::MAX);
    let pin = reg
        .open_pinned(&PartExpect::none(), opener(&b, 4, 50))
        .expect("open");
    let key = {
        let i = pin.part().ident();
        (i.dev, i.ino, i.len)
    };
    drop(pin);
    assert!(reg.invalidate(key), "invalidate drops the entry");
    assert!(reg.get(key).is_none());
    let _ = reg
        .open_pinned(&PartExpect::none(), opener(&b, 4, 50))
        .expect("reopen");
    assert_eq!(reg.counters().misses, 2, "reopen after invalidate is a miss");
    reg.clear();
    assert!(reg.is_empty());
}

#[test]
fn forget_drops_only_that_part_and_its_holders_keep_reading() {
    // A retired part leaves the registry (its entry would hold the unlinked file's fd until eviction, which a
    // zero-resident entry below the count cap never meets); a scan still holding it keeps reading; an entry another
    // open put under the same key is not that part and stays.
    let b = small_part();
    let reg = PartRegistry::new(u64::MAX);
    let pin = reg.open_pinned(&PartExpect::none(), opener(&b, 6, 70)).expect("open");
    let part = pin.part().clone();
    let key = {
        let i = part.ident();
        (i.dev, i.ino, i.len)
    };
    assert!(reg.forget(&part), "the part leaves the registry, pinned or not");
    assert!(reg.get(key).is_none());
    assert!(!reg.forget(&part), "once");
    let mut cur = StreamCursor::open(part.clone(), reference_binding_leaked(), 1, 0).expect("cursor");
    let mut d = vec![0u64; 8192];
    let mut ab = ArenaBuf::new(1 << 20);
    let mut out = DecodeOut { datums: &mut d, arena: ByteArena::new(ab.bytes_mut()) };
    assert_eq!(cur.decode_full(0, &mut out).expect("a holder decodes after forget"), 8192);
    drop((cur, pin));
    assert_eq!(Arc::strong_count(&part), 1, "the holder's is the last reference: its drop closes the part");
    let other = reg.open_pinned(&PartExpect::none(), opener(&b, 6, 70)).expect("reopen");
    assert!(!Arc::ptr_eq(other.part(), &part));
    assert!(!reg.forget(&part), "an entry under the same key that is another part stays");
    assert!(reg.get(key).is_some());
}

/// D3003: the content trim evicts the cached images of parts no statement uses, even pinned ones (the catalog pins every
/// live part), keeps a used part's, and a re-fault returns the same bytes. D3113: an image a holder still shares stays
/// cached and counted (dropping the cache's copy would free nothing), and goes at the first trim after its holder drops.
#[test]
fn d3003_trim_evicts_contents_of_unused_parts_only() {
    let _serial = EPOCH_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    let b = small_part();
    let reg = PartRegistry::new(u64::MAX);
    let p1 = reg.open_pinned(&PartExpect::none(), opener(&b, 1, 31)).expect("p1");
    let p2 = reg.open_pinned(&PartExpect::none(), opener(&b, 1, 32)).expect("p2");
    for p in [&p1, &p2] {
        p.part().prefault_all_sections().expect("warm");
        assert!(p.part().cache_bytes() > 0);
    }
    let used = p2.part().use_part();
    let held = p1.part().section_bytes(0).expect("section 0");
    let before = held.bytes().to_vec();
    assert!(p1.part().resident() > held.len() as u64, "more than the held section is cached");
    assert!(reg.trim_contents(0) > 0, "the unused part's contents go");
    assert_eq!(p1.part().resident(), held.len() as u64, "the held section stays cached and counted");
    assert!(p2.part().cache_bytes() > 0, "a part in use keeps its contents");
    assert_eq!(held.bytes(), &before[..], "a holder's clone stays valid");
    drop(held);
    reg.trim_contents(0);
    assert_eq!(p1.part().cache_bytes(), 0, "its holder gone: pinned entry, contents evicted");
    assert_eq!(p1.part().resident(), 0);
    let again = p1.part().section_bytes(0).expect("re-fault");
    assert_eq!(again.bytes(), &before[..], "a re-fault returns the same bytes");
    assert!(p1.part().resident() > 0);
    drop(used);
    reg.trim_contents(0);
    assert_eq!(p2.part().cache_bytes(), 0, "unused now: evicted");
    assert_eq!(reg.len(), 2, "entries stay: eviction is by contents");
}

/// D4487 t2: a part's charge is its cached images' bytes and each image's block and map entry, plus each passed extent's
/// entry while it is listed; a content eviction takes all of it off.
#[test]
fn d4487_a_parts_charge_counts_each_cached_images_block_and_entry() {
    use crate::openpart::{PassKey, IMAGE_FIXED, PASSED_FIXED};
    let b = small_part();
    let reg = PartRegistry::new(u64::MAX);
    let p = reg.open_pinned(&PartExpect::none(), opener(&b, 1, 4487)).expect("p");
    p.part().evict_contents();
    assert_eq!(p.part().held_and_images(), (0, 0), "nothing cached, nothing charged");
    p.part().prefault_all_sections().expect("warm");
    let (held, n) = p.part().held_and_images();
    assert!(n > 0);
    assert_eq!(held, p.part().resident() + n * IMAGE_FIXED, "the images' bytes and each one's block and entry");
    let key = p.part().a_cached_key().expect("a cached segment");
    p.part().pass_extents((1, 0, 0), None, &[PassKey { key, g_end: 1 }]);
    assert_eq!(p.part().held_and_images().0, held + PASSED_FIXED, "a passed extent's entry");
    p.part().unpass(key);
    assert_eq!(p.part().held_and_images().0, held, "gone with the entry");
    p.part().pass_extents((1, 0, 0), None, &[PassKey { key, g_end: 1 }]);
    p.part().evict_contents();
    assert_eq!(p.part().resident(), 0);
    assert_eq!(p.part().held_and_images(), (0, 0), "the eviction takes the blocks', the entries' and the passed entry's bytes off");
}

/// D4834: a registry-opened part's drop gives its charge to this thread first; an image a holder keeps takes its bytes and
/// block into itself before that, so its holder's drop gives them back where its free lands.
#[test]
fn d4834_a_registry_parts_drop_hands_a_kept_image_its_bytes() {
    use crate::openpart::IMG_BLOCK;
    let b = small_part();
    let reg = PartRegistry::new(u64::MAX);
    let p = reg.open_pinned(&PartExpect::none(), opener(&b, 1, 4834)).expect("p");
    p.part().prefault_all_sections().expect("warm");
    let kept = p.part().section_bytes(0).expect("section 0");
    assert_eq!(kept.owed_bytes(), 0, "cached: on the part's charge");
    drop(p);
    drop(reg);
    assert_eq!(kept.owed_bytes(), kept.len() as u64 + IMG_BLOCK, "its bytes and block, off the dropped part's charge");
}

/// D4487 t3: a reader's image (one no cache keeps) carries its `Arc<Img>` block in its charge beside its bytes, on the
/// reader's owner, so a cursor that keeps it keeps all of it there.
#[test]
fn d4487_a_readers_image_carries_its_block_in_its_charge() {
    use crate::openpart::{SegBuf, IMG_BLOCK};
    std::thread::spawn(|| {
        memledger::bind_backend(4489, 9489);
        let owner = Some((memledger::Owner::Session(memledger::Acct::backend(4489)), memledger::Kind::Region));
        let img = SegBuf::from_bytes_for(owner, &[7u8; 5000]).expect("an image");
        assert_eq!(img.carried_bytes(), img.cap_bytes() + IMG_BLOCK, "its bytes and its block");
        drop(img);
        memledger::unbind_backend();
    })
    .join()
    .unwrap();
}

/// The part cache's value counters (memledger::value): a fault is a miss of its bytes, a cached read a hit, and a re-fault
/// of bytes a content eviction freed a re-miss of them. The counters are process-wide and the tests run in parallel: each
/// delta is at least this test's own.
#[test]
fn value_counters_count_faults_hits_and_re_misses() {
    use memledger::{value, Consumer::PartCache};
    let b = small_part();
    let reg = PartRegistry::new(u64::MAX);
    let p = reg.open_pinned(&PartExpect::none(), opener(&b, 1, 61)).expect("p");
    p.part().evict_contents();
    value::flush();
    let v0 = value::stats(PartCache);
    let n = p.part().section_bytes(0).expect("fault").len() as u64;
    let v1 = value::stats(PartCache);
    assert!(v1.misses > v0.misses && v1.miss_bytes >= v0.miss_bytes + n, "the fault: a miss of its bytes");
    drop(p.part().section_bytes(0).expect("cached"));
    value::flush();
    assert!(value::stats(PartCache).hits > v1.hits, "the cached read: a hit");
    assert!(p.part().evict_contents().0 >= n, "section 0 evicted");
    let r0 = value::stats(PartCache);
    drop(p.part().section_bytes(0).expect("re-fault"));
    let r1 = value::stats(PartCache);
    assert!(r1.re_misses > r0.re_misses && r1.re_miss_bytes >= r0.re_miss_bytes + n, "the re-fault: a re-miss of the bytes evicted");
}

/// Broker (b), `broker_value=1`: a content eviction moves the part cache's byte clock and leaves the part's ghost, which its
/// re-fault takes (a ghost hit); the published tail is the coldest tenth's use stamp and bytes, and a use of a resident
/// part stamped at or below it is a tail hit. The only test that publishes a tail (other tests' doors may add counts
/// meanwhile: the counters are process-wide).
#[test]
fn broker_value_counts_part_ghosts_and_tail_hits() {
    use memledger::{value, Consumer::PartCache};
    let _epoch = epoch_test();
    value::set_ext(true);
    let b = small_part();
    let reg = PartRegistry::new(u64::MAX);
    let p = reg.open_pinned(&PartExpect::none(), opener(&b, 1, 64)).expect("p");
    drop(p.part().section_bytes(0).expect("fault"));
    let e0 = value::ext_stats(PartCache);
    let (s, d, _) = p.part().evict_contents();
    assert!(s + d > 0, "section 0 evicted");
    let e1 = value::ext_stats(PartCache);
    assert!(e1.evicted_bytes >= e0.evicted_bytes + s + d, "the byte clock moves by the bytes evicted");
    drop(p.part().section_bytes(0).expect("re-fault"));
    assert!(value::ext_stats(PartCache).ghost_hits > e1.ghost_hits, "the re-fault takes the part's ghost");

    crate::partcache::statement_boundary();
    drop(p.part().use_part_in(0));
    reg.publish_tail();
    assert_eq!(crate::partcache::tail_stamp(), p.part().cache_used(), "one cached part: the tail is its stamp");
    assert_eq!(value::ext_stats(PartCache).tail_bytes, p.part().resident(), "its segment bytes");
    let t0 = value::ext_stats(PartCache).tail_hits;
    drop(p.part().use_part_in(0));
    assert!(value::ext_stats(PartCache).tail_hits > t0, "a use of the tail's part: a tail hit");
}

/// D3823: the debug poison of a uses-only release evicts as the trim does and fills only the images no holder shares
/// before they free (a stray pointer into one would read 0xDB bytes); an image a holder shares keeps its bytes, cached and
/// counted, and a re-fault reads the part's bytes again.
#[test]
fn d3823_debug_poison_fills_only_the_images_no_holder_shares() {
    let b = small_part();
    let reg = PartRegistry::new(u64::MAX);
    let p = reg.open_pinned(&PartExpect::none(), opener(&b, 1, 3823)).expect("p");
    p.part().prefault_all_sections().expect("warm");
    let held = p.part().section_bytes(0).expect("section 0");
    let kept = held.bytes().to_vec();
    let other = p.part().section_bytes(1).expect("section 1").bytes().to_vec();
    let cached = p.part().resident();
    assert!(cached > held.len() as u64, "more than the held section is cached");
    let poisoned = p.part().debug_poison_contents();
    assert!(poisoned > 0 && poisoned <= cached - held.len() as u64, "only the sole images: {poisoned} of {cached}");
    assert_eq!(p.part().resident(), held.len() as u64, "the held section stays cached and counted");
    assert_eq!(held.bytes(), &kept[..], "a holder's image keeps its bytes");
    assert_eq!(p.part().section_bytes(1).expect("re-fault").bytes(), &other[..], "a re-fault reads the part's bytes");
}

/// Memory step 5 (lane membroker): the part cache's floor counts the cached bytes of the parts in use, and only those.
#[test]
fn memory_budget_floor_is_the_cached_bytes_of_parts_in_use() {
    let b = small_part();
    let reg = PartRegistry::new(u64::MAX);
    let p1 = reg.open_pinned(&PartExpect::none(), opener(&b, 1, 51)).expect("p1");
    let p2 = reg.open_pinned(&PartExpect::none(), opener(&b, 1, 52)).expect("p2");
    for p in [&p1, &p2] {
        p.part().prefault_all_sections().expect("warm");
    }
    assert_eq!(reg.in_use_bytes(), 0, "no part in use");
    let used = p2.part().use_part();
    assert_eq!(reg.in_use_bytes(), p2.part().cache_bytes());
    assert!(reg.in_use_bytes() > 0);
    let both = p1.part().use_part();
    assert_eq!(reg.in_use_bytes(), p1.part().cache_bytes() + p2.part().cache_bytes());
    drop((used, both));
    assert_eq!(reg.in_use_bytes(), 0);
}

/// D4353: the ladder's R2a count for the part cache is what a content trim frees. A part only a deferred use (D4315) or a
/// parked executor's use (D3994) holds is out of the floor, yet no trim evicts it while its statement keeps it and the
/// parked gate is closed: counted, it sent every climb past R2a with nothing freed (2-3 GB said, 0-15 MB freed).
#[test]
fn d4353_the_trimmable_bytes_are_what_a_trim_frees() {
    use memledger::{Holder, Owner, Release};
    let b = small_part();
    let reg = PartRegistry::new(u64::MAX);
    let [free, used, deferred, parked] = [61, 62, 63, 64].map(|ino| {
        let p = reg.open_pinned(&PartExpect::none(), opener(&b, 1, ino)).expect("open");
        p.part().prefault_all_sections().expect("warm");
        p
    });
    let u = used.part().use_part();
    let mut d = deferred.part().use_part();
    d.defer();
    let mut k = parked.part().use_part();
    k.transfer(Owner::Cache(Holder::Parked), Release::Evict);
    assert_eq!(reg.in_use_bytes(), used.part().cache_bytes(), "the floor: the live claim's part only");
    let (counted, unused) = (reg.trimmable_bytes(), free.part().cache_bytes());
    let freed = reg.trim_contents(0);
    assert_eq!(freed, unused, "the trim evicts the unused part only");
    assert!(deferred.part().cache_bytes() > 0 && parked.part().cache_bytes() > 0, "the deferred and the parked part stay");
    assert_eq!(counted, freed, "R2a counted {counted} bytes, the trim freed {freed}");
    drop((u, d, k));
}

/// D4407: R2a's count is what a trim frees now. An image another clone holds (a Gravity part list's handle) stays cached
/// when the trim drops the cache's copy (D3113), so it counts nothing until its holder lets it go; then it counts and the
/// trim frees it. Counted while held, R2a promised gigabytes and freed under a tenth (take 3: 99.8 % of the unused parts'
/// bytes shared, 21.8 of 204 GB freed over 303 trims).
#[test]
fn d4407_an_image_a_holder_clones_is_not_trimmable() {
    let b = small_part();
    let reg = PartRegistry::new(u64::MAX);
    let p = reg.open_pinned(&PartExpect::none(), opener(&b, 1, 65)).expect("open");
    p.part().prefault_all_sections().expect("warm");
    let held = p.part().section_bytes(0).expect("section 0");
    let len = held.len() as u64;
    assert!(len > 0 && p.part().cache_bytes() > len, "the held section is one of the part's cached images");
    let counted = reg.trimmable_bytes();
    let freed = reg.trim_contents(0);
    assert_eq!(p.part().resident(), len, "the trim keeps the held section");
    assert_eq!(counted, freed, "R2a counted {counted} bytes, the trim freed {freed}");
    assert_eq!(reg.trimmable_bytes(), 0, "only the held section is left: no trim frees it");
    drop(held);
    assert_eq!(reg.trimmable_bytes(), len, "its holder let it go: it counts");
    assert_eq!(reg.trim_contents(0), len, "and the trim frees it");
    assert_eq!(p.part().resident(), 0);
}

/// D3999: a part two statements use counts on each account by uses (the views) and on neither alone (the backstop's
/// ranking: canceling one frees none of it); a part one statement uses alone counts on it whole, and once its use drops
/// the cache's reclaim (`trim_contents`) evicts it while the shared one stays.
#[test]
fn d3999_a_part_counts_alone_only_on_its_one_user() {
    let b = small_part();
    let reg = PartRegistry::new(u64::MAX);
    let shared = reg.open_pinned(&PartExpect::none(), opener(&b, 1, 61)).expect("shared");
    let own = reg.open_pinned(&PartExpect::none(), opener(&b, 1, 62)).expect("own");
    for p in [&shared, &own] {
        p.part().prefault_all_sections().expect("warm");
    }
    let (sb, ob) = (shared.part().cache_bytes(), own.part().cache_bytes());
    let on = |procno: u32, part: &PartPin| {
        memledger::bind_backend(procno, 9000 + procno);
        let u = part.part().use_part();
        memledger::unbind_backend();
        u
    };
    let uses = [on(61, &shared), on(62, &shared), on(63, &own)];
    let acct = |procno| memledger::Acct::backend(procno).0;
    let by = |alone: bool| {
        let mut v = Vec::new();
        reg.in_use_by_account(alone, |a, n| v.push((a, n)));
        v.sort();
        v
    };
    let mut split = vec![(acct(61), sb / 2), (acct(62), sb / 2), (acct(63), ob)];
    split.sort();
    assert_eq!(by(false), split, "the views: a shared part split by uses");
    assert_eq!(by(true), vec![(acct(63), ob)], "the ranking: only the part one statement alone pins");
    // D4029 (d): the ranking's pass runs on the memory watchdog's thread: no allocation
    let mut got = [(0u32, 0u64); 2];
    let ((), allocs) = super::counted(|| {
        let mut i = 0;
        reg.in_use_by_account(true, |a, n| {
            if let Some(g) = got.get_mut(i) {
                *g = (a, n);
                i += 1;
            }
        });
    });
    assert_eq!((allocs, got), (0, [(acct(63), ob), (0, 0)]), "the ranking's pass allocated");
    let [a, b2, c] = uses;
    drop(c);
    assert!(reg.trim_contents(0) >= ob, "the released part's contents evicted");
    assert_eq!((shared.part().cache_bytes(), own.part().cache_bytes()), (sb, 0), "the shared part, still in use, stays");
    drop((a, b2));
}

/// C8 (D4087, the per-owner rank): the backstop's ranking counts a part only on the account whose running statement and
/// the portal it executes own every use of it. A use its session keeps (a parked Node's kept cursor, D1507), a parked
/// worker set's (the depot's) or an ended statement's frees on no cancel, so it never ranks the account's next statement
/// nor the next backend on its slot; the running owners' own uses count whole.
#[test]
fn d4087_the_ranking_counts_only_the_running_owners_parts() {
    use memledger::{charge, Acct, Holder, Owner, Release};
    let b = small_part();
    let reg = PartRegistry::new(u64::MAX);
    let [kept, parked, own, mine] = [87, 88, 89, 90].map(|ino| {
        let p = reg.open_pinned(&PartExpect::none(), opener(&b, 1, ino)).expect("open");
        p.part().prefault_all_sections().expect("warm");
        p
    });
    let bytes = |p: &PartPin| p.part().cache_bytes();
    let acct = Acct::backend(187).0;
    let sorted = |mut v: Vec<(u32, u64)>| {
        v.sort();
        v
    };
    let ranked = || {
        let mut v = Vec::new();
        reg.in_use_by_running(|a, n| v.push((a, n)));
        sorted(v)
    };
    memledger::bind_backend(187, 9187);
    memledger::statement_begin();
    let (mut k, mut d) = (kept.part().use_part(), parked.part().use_part());
    let token = charge::new_owner_id();
    charge::portal_begin(token);
    let m = {
        let _in = charge::enter_portal(token);
        let m = mine.part().use_part();
        assert_eq!(
            ranked(),
            sorted(vec![(acct, bytes(&kept)), (acct, bytes(&parked)), (acct, bytes(&mine))]),
            "the running statement's and the portal it executes: their own uses"
        );
        m
    };
    assert_eq!(
        ranked(),
        sorted(vec![(acct, bytes(&kept)), (acct, bytes(&parked))]),
        "a portal it does not execute now: not its rank"
    );
    // the statement's end: the scan's Node parks with its cursor kept (its session's), a worker set parks in the depot
    k.transfer(Owner::Session(Acct(acct)), Release::Session);
    d.transfer(Owner::Cache(Holder::Parked), Release::Evict);
    drop(m);
    let _ = charge::portal_end(Acct(acct), token);
    memledger::statement_end();
    assert_eq!(ranked(), vec![], "no statement runs: nothing ranks");
    memledger::statement_begin();
    let (o, again, again_d) = (own.part().use_part(), kept.part().use_part(), parked.part().use_part());
    assert_eq!(ranked(), vec![(acct, bytes(&own))], "beside a kept or a parked use, its own new use of that part frees nothing");
    // D4029 (d): the ranking's pass runs on the memory watchdog's thread: no allocation
    let mut got = [(0u32, 0u64); 2];
    let ((), allocs) = super::counted(|| {
        let mut i = 0;
        reg.in_use_by_running(|a, n| {
            if let Some(g) = got.get_mut(i) {
                *g = (a, n);
                i += 1;
            }
        });
    });
    assert_eq!((allocs, got), (0, [(acct, bytes(&own)), (0, 0)]), "the ranking's pass allocated");
    drop((o, again, again_d));
    let left = own.part().use_part();
    memledger::statement_end();
    memledger::unbind_backend();
    memledger::bind_backend(187, 9287);
    memledger::statement_begin();
    assert_eq!(ranked(), vec![], "the next backend on the slot: the exited one's kept and left uses");
    let mut alone = Vec::new();
    reg.in_use_by_account(true, |a, n| alone.push((a, n)));
    let views = sorted(vec![(acct, bytes(&kept)), (acct, bytes(&own)), (Acct::PARKED.0, bytes(&parked))]);
    assert_eq!(sorted(alone), views, "the views: the kept and the left use on the slot's account, the parked on the depot's");
    memledger::statement_end();
    memledger::unbind_backend();
    drop((k, d, left));
}

/// C8 move 5: a use names where it ends. A scan's per-part release takes its uses by row (`use_part_by_row`): its
/// context's owner as any use, counted on the same cell; every other take ends where its context does (a portal's at
/// the portal, a statement's at the statement).
#[test]
fn c8_a_use_taken_by_row_is_its_contexts_owners_released_by_row() {
    use memledger::{charge, Acct, Owner, Release};
    let b = small_part();
    let reg = PartRegistry::new(u64::MAX);
    let p = reg.open_pinned(&PartExpect::none(), opener(&b, 1, 91)).expect("open");
    memledger::bind_backend(188, 9188);
    memledger::statement_begin();
    let acct = Acct::backend(188);
    let (held, by_row) = (p.part().use_part_in(7), p.part().use_part_by_row(7));
    assert_eq!((held.owner(), by_row.owner()), (charge::context().0, charge::context().0));
    assert!(matches!(held.owner(), Owner::Statement { .. }), "{:?}", held.owner());
    assert_eq!((held.release(), by_row.release()), (Release::Statement, Release::Row));
    assert_eq!((p.part().users(), charge::owner_row(held.owner()).uses), (2, 2), "both count on the part and on their owner");
    let token = charge::new_owner_id();
    charge::portal_begin(token);
    {
        let _in = charge::enter_portal(token);
        let (held, by_row) = (p.part().use_part_in(7), p.part().use_part_by_row(7));
        assert_eq!((held.owner(), by_row.owner()), (Owner::Portal { acct, portal: token }, Owner::Portal { acct, portal: token }));
        assert_eq!((held.release(), by_row.release()), (Release::Portal, Release::Row));
    }
    let _ = charge::portal_end(acct, token);
    drop((held, by_row));
    assert_eq!(p.part().users(), 0);
    memledger::statement_end();
    memledger::unbind_backend();
}

/// D3109: the content trim's order (ascending use stamp, `trim_contents`) keeps a repeat statement's FIRST-touched parts:
/// inside one statement its latest-touched parts go first, and every part an older statement used goes before any part
/// a newer statement used. (It moves the statement epoch under `epoch_test`, so its stamps are its own.)
#[test]
fn d3109_trim_order_keeps_a_statements_first_touched_parts() {
    let _epoch = epoch_test();
    let b = small_part();
    let reg = PartRegistry::new(u64::MAX);
    let pins: Vec<PartPin> =
        (0..4).map(|i| reg.open_pinned(&PartExpect::none(), opener(&b, 3, 40 + i)).expect("open")).collect();
    let order = |pins: &[PartPin]| {
        let mut v: Vec<(u64, usize)> = pins.iter().enumerate().map(|(i, p)| (p.part().cache_used(), i)).collect();
        v.sort();
        v.into_iter().map(|(_, i)| i).collect::<Vec<_>>()
    };
    // statement 1 scans parts 0..4 in order: a trim after it evicts 3, 2, 1, 0 (keeps the head a repeat reads first)
    crate::partcache::statement_boundary();
    for p in &pins {
        drop(p.part().use_part());
    }
    assert_eq!(order(&pins), vec![3, 2, 1, 0], "inside a statement the latest-touched part goes first");
    // a second use of the head in the same statement (another lane's cursor, a late read) keeps its first-touch place
    drop(pins[0].part().use_part());
    drop(pins[1].part().use_part());
    assert_eq!(order(&pins), vec![3, 2, 1, 0], "a later use in the same statement does not move a part");
    // its repeat touches them in the same order: the same parts stay
    crate::partcache::statement_boundary();
    for p in &pins {
        drop(p.part().use_part());
    }
    assert_eq!(order(&pins), vec![3, 2, 1, 0], "a repeat keeps the same head");
    // statement 3 touches part 3 only: the older statement's parts go first (least recently used across statements)
    crate::partcache::statement_boundary();
    drop(pins[3].part().use_part());
    assert_eq!(order(&pins), vec![2, 1, 0, 3], "an older statement's parts go before a newer one's");
    // a trim to 0 still evicts every unused part's contents in that order, and none in use
    for p in &pins {
        p.part().prefault_all_sections().expect("warm");
    }
    let used = pins[0].part().use_part();
    reg.trim_contents(0);
    assert!(pins[0].part().cache_bytes() > 0, "a part in use keeps its contents");
    for p in &pins[1..] {
        assert_eq!(p.part().cache_bytes(), 0, "unused: evicted");
    }
    drop(used);
}

/// D3109: a part's streams its last statement did not open are stale: `evict_stale_streams` (a content trim's first pass,
/// before whole parts) drops another statement's column and keeps the live one resident.
#[test]
fn d3109_stale_streams_go_first_the_live_column_stays() {
    let _epoch = epoch_test();
    let b = build_part(&PartSpec::new(9_000, vec![seq_i64_col(1, 9_000), seq_i64_col(2, 9_000)]));
    let reg = PartRegistry::new(u64::MAX);
    let pin = reg.open_pinned(&PartExpect::none(), opener(&b, 3, 60)).expect("open");
    let part = pin.part().clone();
    let binding = reference_binding_leaked();
    let mut d = vec![0u64; 8192];
    let mut ab = ArenaBuf::new(1 << 20);
    let mut decode = |attno: u32| {
        let mut cur = StreamCursor::open(part.clone(), binding, attno, 0).expect("cursor");
        let mut out = DecodeOut { datums: &mut d, arena: ByteArena::new(ab.bytes_mut()) };
        assert_eq!(cur.decode_full(0, &mut out).expect("decode"), 8192);
    };
    // statement 1 reads columns 1 and 2, statement 2 column 1 only
    crate::partcache::statement_boundary();
    decode(1);
    let one = part.cache_bytes();
    decode(2);
    let both = part.cache_bytes();
    assert!(one > 0 && both > one, "each column faults its own extents");
    crate::partcache::statement_boundary();
    decode(1);
    assert_eq!(part.cache_bytes(), both, "column 1 reads its cached extents");
    assert_eq!(part.evict_stale_streams(), both - one, "column 2 is stale: its extents go");
    assert_eq!(part.cache_bytes(), one, "column 1 stays resident");
    decode(1);
    assert_eq!(part.cache_bytes(), one, "column 1 still reads from the cache");
    assert_eq!(part.evict_stale_streams(), 0, "nothing stale is left");
    drop(pin);
}

/// An io whose identity carries a modification time (the fd pool's `VfsPartIo` does; `MemPartIo` reports none).
struct TimedIo {
    io: crate::io::MemPartIo,
    mtime: (i64, i64),
}

impl PartIo for TimedIo {
    fn len(&self) -> u64 {
        self.io.len()
    }

    fn dev_ino(&self) -> (u64, u64) {
        self.io.dev_ino()
    }

    fn pread_exact(&self, off: u64, buf: &mut [u8], at: &'static str) -> crate::ReadResult<()> {
        self.io.pread_exact(off, buf, at)
    }

    fn mtime(&self) -> Option<(i64, i64)> {
        Some(self.mtime)
    }
}

/// GENID's rule under the fd pool (D2873): an entry no longer holds its file open, so `(dev, ino, len)` can name a new
/// file at a dropped part's reused inode. A hit with another mtime is not the entry's file: the entry goes and the open
/// parses its own; the same mtime still shares one entry.
#[test]
fn a_hit_with_another_mtime_is_a_miss() {
    let b = small_part();
    let reg = PartRegistry::new(u64::MAX);
    let timed = |m: i64| {
        let io = b.mem_io(7, 80);
        move || Ok(Box::new(TimedIo { io, mtime: (m, 0) }) as Box<dyn PartIo>)
    };
    let p1 = reg.open_pinned(&PartExpect::none(), timed(1)).expect("open 1");
    let p2 = reg.open_pinned(&PartExpect::none(), timed(1)).expect("open 2");
    assert!(Arc::ptr_eq(p1.part(), p2.part()), "the same file shares its entry");
    assert_eq!((reg.counters().hits, reg.counters().misses), (1, 1));
    let p3 = reg.open_pinned(&PartExpect::none(), timed(2)).expect("open 3");
    assert!(!Arc::ptr_eq(p1.part(), p3.part()), "another mtime at the same (dev, ino, len) is another file");
    assert_eq!((reg.counters().hits, reg.counters().misses), (1, 2));
    assert_eq!(reg.len(), 1, "the stale entry left; the new file's took its key");
    let p4 = reg.open_pinned(&PartExpect::none(), timed(2)).expect("open 4");
    assert!(Arc::ptr_eq(p3.part(), p4.part()));
}

static D3113_HELD: std::sync::Mutex<Vec<crate::openpart::SegBuf>> = std::sync::Mutex::new(Vec::new());
static D3113_RELEASE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn d3113_hook() {
    if D3113_RELEASE.load(std::sync::atomic::Ordering::Relaxed) {
        D3113_HELD.lock().unwrap().clear();
    }
}

/// D3113: a trim past its target with images a handle owner holds runs the evicted hook (Gravity's part lists drop),
/// then those images go and the census falls by their bytes; while the owner keeps them, they stay counted.
#[test]
fn d3113_trim_runs_the_hook_for_shared_images() {
    let _serial = EPOCH_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    let b = small_part();
    let reg = PartRegistry::new(u64::MAX);
    let p = reg.open_pinned(&PartExpect::none(), opener(&b, 1, 61)).expect("open");
    p.part().prefault_all_sections().expect("warm");
    let held = p.part().section_bytes(0).expect("section 0");
    let len = held.len() as u64;
    D3113_HELD.lock().unwrap().push(held);
    crate::partcache::set_evicted_hook(d3113_hook);
    reg.trim_contents(0);
    assert_eq!(p.part().resident(), len, "the owner keeps the section: it stays counted");
    D3113_RELEASE.store(true, std::sync::atomic::Ordering::Relaxed);
    reg.trim_contents(0);
    assert!(D3113_HELD.lock().unwrap().is_empty(), "the trim ran the hook");
    assert_eq!(p.part().resident(), 0, "then the section went");
}

/// What the owed-image tests' handle owner keeps (a part list's dictionary handle, say), dropped by [`owner_hook`].
static OWNER_KEPT: std::sync::Mutex<Vec<SegBuf>> = std::sync::Mutex::new(Vec::new());

fn owner_hook() {
    OWNER_KEPT.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// D3113 on the line's class at the trim's first pass: a stale stream's image a handle owner keeps leaves its part there
/// but stays charged until the owner drops it, so it frees nothing yet; the trim, still past its target, runs the evicted
/// hook for it (the owner's handles go, and the image with them).
#[test]
fn a_trim_runs_the_hook_for_a_stale_streams_image_its_owner_keeps() {
    let _serial = EPOCH_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    let b = build_part(&PartSpec::new(9_000, vec![seq_i64_col(1, 9_000), seq_i64_col(2, 9_000)]));
    let reg = PartRegistry::new(u64::MAX);
    let pin = reg.open_pinned(&PartExpect::none(), opener(&b, 3, 63)).expect("open");
    let part = pin.part().clone();
    let binding = reference_binding_leaked();
    let mut d = vec![0u64; 8192];
    let mut ab = ArenaBuf::new(1 << 20);
    let mut decode = |attno: u32| {
        let mut cur = StreamCursor::open(part.clone(), binding, attno, 0).expect("cursor");
        let mut out = DecodeOut { datums: &mut d, arena: ByteArena::new(ab.bytes_mut()) };
        assert_eq!(cur.decode_full(0, &mut out).expect("decode"), 8192);
    };
    // statement 1 reads columns 1 and 2, and the owner keeps column 2's first image; statement 2 reads column 1 only
    crate::partcache::statement_boundary();
    decode(1);
    decode(2);
    let dir = part.stream_directory().expect("stream directory");
    let ps = dir.lookup(2, 0, StreamRole::Values).expect("column 2's values");
    let kept = part.extent_bytes(&ps.entry, &ps.extents[0], 0).expect("column 2's first image");
    assert!(kept.is_shared(), "the owner shares the cached image");
    OWNER_KEPT.lock().unwrap_or_else(|e| e.into_inner()).push(kept);
    crate::partcache::statement_boundary();
    decode(1);
    crate::partcache::set_evicted_hook(owner_hook);
    reg.trim_contents(0);
    assert!(OWNER_KEPT.lock().unwrap_or_else(|e| e.into_inner()).is_empty(), "the trim ran the evicted hook for the owner's image");
    assert_eq!(part.resident(), 0, "both columns left the part");
    drop(pin);
}

/// D3113: an image a key eviction (the prefetcher's cooling) let go of while its owner kept it is owed before any trim,
/// and no part's map has it: a trim past its target still runs the evicted hook for it.
#[test]
fn a_trim_runs_the_hook_for_an_image_owed_before_it() {
    let _serial = EPOCH_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    let b = small_part();
    let reg = PartRegistry::new(u64::MAX);
    let p = reg.open_pinned(&PartExpect::none(), opener(&b, 1, 64)).expect("open");
    p.part().prefault_all_sections().expect("warm");
    let kept = p.part().section_bytes(0).expect("section 0");
    let key = (p.part().sections()[0].off, kept.len() as u64);
    assert_eq!(p.part().evict_extent_images(&[key]), (1, kept.len() as u64), "the map lets it go");
    OWNER_KEPT.lock().unwrap_or_else(|e| e.into_inner()).push(kept);
    crate::partcache::set_evicted_hook(owner_hook);
    reg.trim_contents(0);
    assert!(OWNER_KEPT.lock().unwrap_or_else(|e| e.into_inner()).is_empty(), "the trim ran the evicted hook for the owed image");
    assert_eq!(p.part().resident(), 0);
}

static D4504_HOLDS: std::sync::Mutex<Vec<crate::FaceHold>> = std::sync::Mutex::new(Vec::new());

/// The faces hook and its holds are process-wide: the tests that let faces go take turns.
static FACES_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn d4504_hook() {
    // the owner drops its stale holds (their bytes leave the census as they drop)
    D4504_HOLDS.lock().unwrap().retain(|h| !h.stale());
}

/// D4504: a holder's faces of a part are that part's part-cache contents, counted (`face_bytes`, the census) from
/// `FaceHold::add` to the drop: a part in use keeps them through a trim; unused, a trim lets them go after its data (the
/// faces hook drops the stale holds); an off-switch hold is charged only, never counted or stale.
#[test]
fn d4504_a_parts_faces_are_its_contents_and_go_after_its_data() {
    let _one = FACES_TESTS.lock().unwrap_or_else(|p| p.into_inner());
    let b = small_part();
    let reg = PartRegistry::new(u64::MAX);
    let p = reg.open_pinned(&PartExpect::none(), opener(&b, 1, 64)).expect("open");
    let part = p.part().clone();
    part.prefault_all_sections().expect("warm");
    let mut off = crate::FaceHold::new(&part, false);
    off.add(512);
    assert_eq!(part.face_bytes(), 0, "an off-switch hold is charged only");
    let mut h = crate::FaceHold::new(&part, true);
    h.add(4096);
    assert_eq!((part.face_bytes(), h.bytes()), (4096, 4096), "a counted hold is its part's faces");
    D4504_HOLDS.lock().unwrap().push(h);
    crate::partcache::set_faces_hook(d4504_hook);
    let used = part.use_part();
    reg.trim_contents(0);
    assert_eq!(part.face_bytes(), 4096, "a part in use keeps its faces");
    assert!(!D4504_HOLDS.lock().unwrap()[0].stale(), "and its holds stay live");
    drop(used);
    reg.trim_contents(0);
    assert_eq!(part.resident(), 0, "the unused part's data went");
    assert_eq!(part.face_bytes(), 0, "then its faces: the hook dropped the stale hold");
    assert!(D4504_HOLDS.lock().unwrap().is_empty(), "the trim ran the faces hook");
    assert!(!off.stale(), "an off-switch hold is never stale");
    drop(p);
}

/// D4802: the real evictions count a part's faces once, as they leave the census. A running statement's hold keeps them
/// through any number of trims: each trim's return and the faces tier count them again, the real evictions not at all.
/// Once the hold is the faces hook owner's (its statement ended), the trim that drops it counts them once, the next
/// nothing.
#[test]
fn d4802_held_faces_are_not_real_evictions() {
    let _one = FACES_TESTS.lock().unwrap_or_else(|p| p.into_inner());
    let b = small_part();
    let reg = PartRegistry::new(u64::MAX);
    let p = reg.open_pinned(&PartExpect::none(), opener(&b, 1, 66)).expect("open");
    let part = p.part().clone();
    part.prefault_all_sections().expect("warm");
    crate::partcache::set_faces_hook(d4504_hook);
    // a running statement's hold: the hook's owner does not have it, so no trim drops it
    let mut held = crate::FaceHold::new(&part, true);
    held.add(4096);
    reg.trim_contents(0);
    assert_eq!(part.resident(), 0, "the unused part's data went first");
    let real = reg.real_evictions();
    assert_eq!(reg.faces_trims(), (1, 4096, 0), "the faces tier let the held faces go; none left the census");
    for _ in 0..3 {
        assert_eq!(reg.trim_contents(0), 4096, "each trim returns the held faces again");
    }
    assert_eq!(part.face_bytes(), 4096, "they stay");
    assert_eq!(reg.faces_trims(), (4, 4 * 4096, 0), "the faces tier counts them at every trim");
    assert_eq!(reg.real_evictions(), real, "the real evictions never");
    // the statement ended: its stale hold goes to the hook's owner, who drops it at the next trim
    D4504_HOLDS.lock().unwrap().push(held);
    assert_eq!(reg.trim_contents(0), 4096);
    assert_eq!(part.face_bytes(), 0, "the hook dropped the hold");
    assert_eq!(reg.faces_trims(), (5, 5 * 4096, 4096), "they left the census at that trim");
    assert_eq!(reg.real_evictions(), (real.0 + 1, real.1 + 4096), "the real evictions count them once");
    assert_eq!(reg.trim_contents(0), 0, "nothing left to let go");
    assert_eq!(reg.real_evictions(), (real.0 + 1, real.1 + 4096), "and nothing more");
    assert!(D4504_HOLDS.lock().unwrap().is_empty());
    drop(p);
}

/// D4802 (2), trim_real_frees on: a trim returns the bytes that left memory, as the real evictions count them. A running
/// statement's held faces free nothing at any trim (the faces tier still lets them go, counted apart); the trim that drops
/// the hook owner's hold returns them once.
#[test]
fn d4802_trim_real_frees_returns_no_held_faces() {
    use mempolicy::consts::{set, K};
    struct Off;
    impl Drop for Off {
        fn drop(&mut self) {
            set(K::TrimRealFrees, 0);
        }
    }
    // the knob is the process's: the other faces tests read it off, and a failing assert restores it
    let _one = FACES_TESTS.lock().unwrap_or_else(|p| p.into_inner());
    let _off = Off;
    set(K::TrimRealFrees, 1);
    let b = small_part();
    let reg = PartRegistry::new(u64::MAX);
    let p = reg.open_pinned(&PartExpect::none(), opener(&b, 1, 67)).expect("open");
    let part = p.part().clone();
    part.prefault_all_sections().expect("warm");
    crate::partcache::set_faces_hook(d4504_hook);
    let mut held = crate::FaceHold::new(&part, true);
    held.add(4096);
    let data = reg.trim_contents(0);
    assert!(data > 0, "the unused part's data went");
    assert_eq!(part.resident(), 0);
    assert_eq!(reg.real_evictions(), (1, data), "the trim returned its real evictions alone (off: the held faces too)");
    for _ in 0..3 {
        assert_eq!(reg.trim_contents(0), 0, "the held faces free nothing (off: 4096 at every trim)");
    }
    assert_eq!(part.face_bytes(), 4096, "they stay");
    assert_eq!(reg.faces_trims(), (4, 4 * 4096, 0), "the faces tier let them go at every trim; none left");
    assert_eq!(reg.real_evictions(), (1, data));
    // the statement ended: the hook's owner drops its stale hold at the next trim, which returns the faces once
    D4504_HOLDS.lock().unwrap().push(held);
    assert_eq!(reg.trim_contents(0), 4096);
    assert_eq!(part.face_bytes(), 0);
    assert_eq!(reg.real_evictions(), (2, data + 4096));
    assert_eq!(reg.trim_contents(0), 0, "nothing left");
    assert!(D4504_HOLDS.lock().unwrap().is_empty());
    drop(p);
}

/// Memory step 6a: the part cache's re-use counter counts statements, not epochs or threads: one
/// statement's many uses (any thread) never count; a second statement's use shifts the first's second into `prev`, which
/// the census sums per window at the part's NEED; an eviction keeps NEED at what it freed until the next statement; a
/// use without a token counts at its first use in an epoch 2 s or more after the latest; with the broker off nothing is
/// recorded. The only test that sets the broker clock.
#[test]
fn memory_broker_reuse_counts_statements_not_epochs_or_threads() {
    use crate::partcache::{next_stmt_token, reused_evicted_at, set_now_sec};
    let b = small_part();
    let reg = PartRegistry::new(u64::MAX);
    let p1 = reg.open_pinned(&PartExpect::none(), opener(&b, 1, 61)).expect("p1");
    let p2 = reg.open_pinned(&PartExpect::none(), opener(&b, 1, 62)).expect("p2");
    for p in [&p1, &p2] {
        p.part().prefault_all_sections().expect("warm");
    }
    set_now_sec(0);
    drop(p1.part().use_part_in(next_stmt_token()));
    assert_eq!(p1.part().reuse_secs(), (0, 0), "the broker off records nothing");

    set_now_sec(1000);
    let a = next_stmt_token();
    for _ in 0..5 {
        drop(p1.part().use_part_in(a));
    }
    std::thread::scope(|s| {
        s.spawn(|| drop(p1.part().use_part_in(a)));
    });
    assert_eq!(p1.part().reuse_secs(), (1000, 0), "one statement is never a re-use");
    assert_eq!(reg.reuse_census(1000).reused, [0; 3]);

    set_now_sec(1100);
    drop(p1.part().use_part_in(next_stmt_token()));
    assert_eq!(p1.part().reuse_secs(), (1100, 1000));
    let need = p1.part().need_bytes();
    assert!(need > 0 && need == p1.part().cache_bytes());
    let c = reg.reuse_census(1100);
    assert_eq!((c.reused, c.reused_parts, c.parts), ([0, need, need], [0, 1, 1], 2), "100 s: inside 300 and 900, not 60");
    assert_eq!(reg.reuse_census(2000).reused, [0; 3], "past 900 s");

    let (s, d, _) = p1.part().evict_contents();
    assert!(s + d > 0);
    assert_eq!(p1.part().cache_bytes(), 0);
    assert_eq!(p1.part().need_bytes(), s + d, "NEED keeps what the eviction freed");
    assert_eq!(reused_evicted_at(), 1100, "a re-used part's eviction stamps the no-shrink second");
    assert_eq!(reg.reuse_census(1100).reused[2], s + d);
    set_now_sec(1200);
    drop(p1.part().use_part_in(next_stmt_token()));
    assert_eq!(p1.part().need_bytes(), 0, "a new statement starts NEED over from what is cached");

    let q = p2.part();
    q.note_reuse(0, true, 1300);
    assert_eq!(q.reuse_secs(), (1300, 0));
    q.note_reuse(0, true, 1301);
    q.note_reuse(0, false, 1400);
    assert_eq!(q.reuse_secs(), (1300, 0), "inside 2 s, or not the epoch's first use: the same statement");
    q.note_reuse(0, true, 1400);
    assert_eq!(q.reuse_secs(), (1400, 1300));
    set_now_sec(0);
}

/// Memory step 6a: the janitor's removals are counted with the segment bytes the removed entries held (C2: the census
/// is a lower bound while they grow).
#[test]
fn memory_broker_registry_removals_count_their_bytes() {
    let b = small_part();
    let reg = PartRegistry::new(0);
    reg.set_max_entries(1);
    let held = {
        let p1 = reg.open_pinned(&PartExpect::none(), opener(&b, 1, 71)).expect("p1");
        p1.part().prefault_all_sections().expect("warm");
        p1.part().resident()
    };
    assert!(held > 0);
    assert_eq!(reg.removed(), (0, 0));
    let _p2 = reg.open_pinned(&PartExpect::none(), opener(&b, 1, 72)).expect("p2");
    assert_eq!(reg.removed(), (1, held));
}

static D3268_HELD: std::sync::Mutex<Vec<crate::openpart::PartUse>> = std::sync::Mutex::new(Vec::new());

/// The short hook's owner: it lets go every use it keeps (Gravity's parked worker sets' kept cursors).
fn d3268_hook() -> bool {
    let mut held = D3268_HELD.lock().unwrap();
    let any = !held.is_empty();
    held.clear();
    any
}

/// D3268: a part a holder keeps in use for no running statement (a parked set's kept cursor) is never evicted, so a trim
/// stops short of its target; with the short hook set, the trim lets the owner drop those uses and then evicts the part.
#[test]
fn d3268_a_short_trim_runs_the_hook_and_evicts_the_parts_it_let_go() {
    let b = small_part();
    let reg = PartRegistry::new(u64::MAX);
    let p = reg.open_pinned(&PartExpect::none(), opener(&b, 1, 62)).expect("open");
    // in use before it is warm: a concurrent test's trim evicts any part no one uses
    D3268_HELD.lock().unwrap().push(p.part().use_part());
    p.part().prefault_all_sections().expect("warm");
    let warm = p.part().resident();
    assert!(warm > 0, "the part is cached");
    reg.trim_contents(0);
    assert_eq!(p.part().resident(), warm, "in use: the trim stops short");
    crate::partcache::set_short_hook(d3268_hook);
    reg.trim_contents(0);
    assert!(D3268_HELD.lock().unwrap().is_empty(), "the short trim ran the hook");
    assert_eq!(p.part().users(), 0);
    // the hook is process-wide: a concurrent test's short trim may have run it between this trim's passes and its check,
    // so this registry's trim saw the part in use and found it free only after; the next trim's first pass evicts it then
    if p.part().resident() != 0 {
        reg.trim_contents(0);
    }
    assert_eq!(p.part().resident(), 0, "then the part's contents went");
}

/// D4570 + D4554: a dropped part whose last running uses move to a parked executor in one group of use moves
/// (`UseMoves`: a source's kept cursors parked a part at a time, at its statement's end) gives its contents back at the
/// group's end, as one use's park does (`PartUse::transfer`). Before, the group's parked count moved with no end of the
/// running uses, and the dropped part kept its contents in the part cache.
#[test]
fn d4554_a_dropped_parts_last_running_uses_parked_in_a_group_give_its_contents_back() {
    use memledger::{Acct, Owner, Release};
    let b = small_part();
    let reg = PartRegistry::new(u64::MAX);
    let p = reg.open_pinned(&PartExpect::none(), opener(&b, 1, 91)).expect("open");
    p.part().prefault_all_sections().expect("warm");
    let cached = p.part().cache_bytes();
    assert!(cached > 0, "the part's contents cached");
    memledger::bind_backend(191, 9191);
    memledger::statement_begin();
    let session = Owner::Session(Acct::backend(191));
    // two kept cursors' uses of the part, the running statement's
    let (mut u1, mut u2) = (p.part().use_part(), p.part().use_part());
    let (_, freed0) = crate::partcache::dropped_counters();
    assert_eq!(crate::partcache::drop_parts([p.part()]), 0, "a running statement uses the dropped part: its contents stay");
    assert_eq!(p.part().cache_bytes(), cached);
    {
        let mut group = crate::UseMoves::new(p.part());
        u1.transfer_in(&mut group, session, Release::Session);
        u2.transfer_in(&mut group, session, Release::Session);
        assert_eq!(p.part().cache_bytes(), cached, "inside the group: its parked count moves at its end");
    }
    assert_eq!((p.part().users(), p.part().live_users()), (2, 0), "both uses parked");
    assert_eq!(p.part().cache_bytes(), 0, "the group's end parked the last running use: the dropped part's contents went");
    assert!(crate::partcache::dropped_counters().1 - freed0 >= cached, "counted as given back by a running use's end");
    memledger::statement_end();
    drop((u1, u2));
    memledger::unbind_backend();
}

/// D4554: a part keeps the code counts code.heads' emit asked as the part cache's bytes (its census and its charge), answers
/// only what it keeps, never more than `mempolicy::keep::CODE_COUNTS_KEPT`, and drops them with its contents.
#[test]
fn d4554_a_parts_kept_code_counts_are_cache_bytes_dropped_with_its_contents() {
    use mempolicy::keep::CODE_COUNTS_KEPT;
    let b = small_part();
    let reg = PartRegistry::new(u64::MAX);
    let p = reg.open_pinned(&PartExpect::none(), opener(&b, 1, 4554)).expect("p");
    p.part().evict_contents();
    assert_eq!((p.part().resident(), p.part().held_and_images()), (0, (0, 0)), "nothing cached");
    let mut out = [0u32; 2];
    assert!(!p.part().code_counts(1, &[5, 7], &mut out), "nothing kept: a miss");
    p.part().keep_code_counts(1, &[(5, 10), (7, 20)]);
    assert!(p.part().code_counts(1, &[5, 7], &mut out));
    assert_eq!(out, [10, 20], "the kept rows");
    assert!(!p.part().code_counts(1, &[5, 8], &mut out), "a code not kept misses");
    assert!(!p.part().code_counts(2, &[5], &mut out[..1]), "another column's code misses");
    let kept = p.part().resident();
    assert!(kept >= 2 * core::mem::size_of::<(i16, u32, u32)>() as u64, "the list's bytes are the cache's ({kept})");
    assert_eq!(p.part().held_and_images(), (kept, 0), "and on its charge");
    p.part().keep_code_counts(1, &[(5, 99)]);
    assert!(p.part().code_counts(1, &[5], &mut out[..1]));
    assert_eq!((out[0], p.part().resident()), (10, kept), "a kept code keeps its count: no growth");
    let many: Vec<(u32, u32)> = (100..100 + 2 * CODE_COUNTS_KEPT as u32).map(|c| (c, 1)).collect();
    p.part().keep_code_counts(1, &many);
    let mut last = [0u32; 1];
    assert!(!p.part().code_counts(1, &[100 + CODE_COUNTS_KEPT as u32], &mut last), "at most CODE_COUNTS_KEPT kept");
    assert_eq!(p.part().held_and_images().0, p.part().resident(), "the charge follows the growth");
    p.part().evict_contents();
    assert_eq!((p.part().resident(), p.part().held_and_images()), (0, (0, 0)), "the eviction takes them and their bytes");
    assert!(!p.part().code_counts(1, &[5], &mut out[..1]), "gone with the contents");
}

/// D4659: N threads trimming one over-budget cache to the same target at the same moment evict the excess once, at worst
/// one part per trimmer past it. Before, each counted down its own snapshot of the cache's bytes (only what it evicted
/// itself), so together they evicted up to N times the excess (986 + 988 MiB in one millisecond on an 8-worker scan run).
/// 30 rounds of 4 trimmers over 40 warm parts (~2 MB each, an i64 and a text column); a round's excess is half of them.
#[test]
fn d4659_concurrent_trims_evict_the_excess_once() {
    const TRIMMERS: usize = 4;
    let _epoch = epoch_test();
    let b = build_part(&PartSpec::new(80_000, vec![seq_i64_col(1, 80_000), crate::testpart::text_col(2, 80_000, None)]));
    let reg = Arc::new(PartRegistry::new(u64::MAX));
    let pins: Vec<PartPin> =
        (0..40).map(|i| reg.open_pinned(&PartExpect::none(), opener(&b, 46, 5900 + i)).expect("open")).collect();
    let mut worst = 0f64;
    for _round in 0..30 {
        for p in &pins {
            p.part().prefault_all_sections().expect("warm");
        }
        let per = pins[0].part().cache_bytes();
        assert!(per > 0);
        let excess = per * pins.len() as u64 / 2;
        let target = crate::partcache::bytes().saturating_sub(excess);
        let go = Arc::new(std::sync::Barrier::new(TRIMMERS));
        let trims: Vec<_> = (0..TRIMMERS)
            .map(|_| {
                let (reg, go) = (reg.clone(), go.clone());
                std::thread::spawn(move || {
                    go.wait();
                    reg.trim_contents(target)
                })
            })
            .collect();
        let freed: u64 = trims.into_iter().map(|t| t.join().expect("trim thread")).sum();
        let ratio = freed as f64 / excess as f64;
        worst = worst.max(ratio);
        assert!(freed <= excess + TRIMMERS as u64 * per, "{TRIMMERS} concurrent trims evicted {freed} bytes for an excess of {excess} ({ratio:.2}x; parts of {per})");
    }
    eprintln!("d4659: worst round evicted {worst:.2}x the excess");
}
