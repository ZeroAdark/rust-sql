//! The kernels (shelf, central lists, runs) over a tiny pool, one sequential test (the pool is process-wide). Miri runs
//! these: `cargo +nightly-2026-07-17 miri test -p pagepool --lib`.

use crate::central::{self, Key};
use crate::run::{self, RUN};
use crate::*;
use core::ptr::NonNull;

const SHELF: usize = 64 << 10;

fn tiny() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let cfg = Config {
            va: 16 << 20,
            chunk: 8 << 20,
            lazy_va: 0,
            frame_va: 0,
            shelf_max: SHELF,
            warm_zero: 0,
            warm_zero_max: 0,
            warm_cap: 0,
            decay_ms: 0,
            purge_batch: 2 << 20,
            lazy_min: 32 << 20,
            idle_drain_ms: 1000,
            block_min: 1024,
            release_mode: 2,
            lazy_decay_ms: 0,
            lazy_cap: 0,
            age_pass_max: 0,
            thp_partial: 0,
            host_age_ms: 0,
        };
        assert!(init(&cfg));
    });
}

fn take_n(n: usize, bytes: usize, class: Class) -> Vec<NonNull<u8>> {
    (0..n).map(|_| take(bytes, 16, Hint::new(class, Tag::OTHER)).expect("take").cast::<u8>()).collect()
}

fn give_all(v: Vec<NonNull<u8>>, bytes: usize) {
    for p in v {
        // SAFETY: taken above, not used again.
        unsafe { give(p, bytes) };
    }
}

fn balanced() {
    if let Err(e) = stats::check() {
        panic!("{e}: {:?}", stats());
    }
}

#[test]
fn pool_kernels() {
    tiny();
    let rounds = if cfg!(miri) { 1 } else { 20 };
    for _ in 0..rounds {
        shelf_spills_half_to_central();
        blocks_are_distinct_and_aligned();
        runs_grow_and_shrink_in_place();
        a_thread_frees_what_another_took();
        dead_pages_round_trip();
        #[cfg(feature = "global")]
        crate::small::tests::small_kernels();
        purge_homes_everything();
    }
}

fn shelf_spills_half_to_central() {
    let key = Key::new(Class::Task, central::bucket_slot(0));
    let v = take_n(40, 1024, Class::Task);
    let before = central::bytes(key);
    give_all(v, 1024);
    let cap = (SHELF / 8 / 1024) as usize;
    assert!(shelf::mine() <= SHELF, "shelf past its max: {}", shelf::mine());
    assert!(central::bytes(key) >= before + (40 - cap) * 1024 / 2, "half lists went to central");
    flush_shelf();
    assert_eq!(shelf::mine(), 0);
    balanced();
}

fn blocks_are_distinct_and_aligned() {
    let mut all = Vec::new();
    for (i, &b) in run::BUCKETS.iter().enumerate().filter(|&(i, _)| run::selectable(i as u8)) {
        let n = if cfg!(miri) { 2 } else { 6 };
        for _ in 0..n {
            let p = take(b as usize, run::natural_align(i as u8), Hint::new(Class::Session, Tag::MCX)).unwrap();
            assert_eq!(p.len(), b as usize);
            let a = p.cast::<u8>().as_ptr() as usize;
            assert_eq!(a % run::natural_align(i as u8), 0);
            let info = run_of(a as *const u8).unwrap();
            assert_eq!((info.kind, info.base, info.bytes, info.tag), (Kind::Blocks { bucket: i as u8 }, a, b as usize, Tag::MCX));
            assert_eq!(run_of((a + b as usize - 1) as *const u8).unwrap().base, a, "an interior pointer names its block");
            all.push((a, b as usize));
        }
    }
    all.sort();
    for w in all.windows(2) {
        assert!(w[0].0 + w[0].1 <= w[1].0, "blocks overlap: {:?}", w);
    }
    for (i, (a, b)) in all.into_iter().enumerate() {
        let p = NonNull::new(a as *mut u8).unwrap();
        // SAFETY: taken above with this size, alignment and hint.
        unsafe {
            if i % 2 == 0 {
                give_hinted(p, b, run::natural_align(run::bucket_of(b, 1).unwrap()), Hint::new(Class::Session, Tag::MCX));
            } else {
                give(p, b);
            }
        }
    }
    flush_shelf();
    balanced();
}

fn runs_grow_and_shrink_in_place() {
    let h = Hint::new(Class::Cache, Tag::PART);
    let p = take(3 * RUN, RUN, h).unwrap();
    assert_eq!(p.len(), 3 * RUN);
    let a = p.cast::<u8>();
    assert_eq!(run_of(a.as_ptr()).unwrap().kind, Kind::Run { runs: 3 });
    // SAFETY: ours.
    let g = unsafe { resize(a, 3 * RUN, 5 * RUN, RUN) };
    if let Some(g) = g {
        assert_eq!((g.cast::<u8>(), g.len()), (a, 5 * RUN));
        assert_eq!(run_of((a.as_ptr() as usize + 4 * RUN) as *const u8).unwrap().base, a.as_ptr() as usize);
    }
    // SAFETY: ours.
    let s = unsafe { resize(a, 5 * RUN, RUN + 1, RUN) }.unwrap();
    assert_eq!(s.len(), 2 * RUN);
    // SAFETY: ours, not used again.
    unsafe { give(a, 2 * RUN) };
    let big = take(20 * RUN, 4 * RUN, h).unwrap();
    assert_eq!(big.cast::<u8>().as_ptr() as usize % (4 * RUN), 0, "runs start aligned");
    // SAFETY: ours.
    unsafe { give(big.cast(), 20 * RUN) };
    flush_shelf();
    balanced();
}

fn a_thread_frees_what_another_took() {
    let n = if cfg!(miri) { 24 } else { 400 };
    let (tx, rx) = std::sync::mpsc::sync_channel::<(usize, usize)>(8);
    let taker = std::thread::spawn(move || {
        for i in 0..n {
            let b = [1024, 3072, 8192, 65536, 2 * RUN][i % 5];
            let p = take(b, 16, Hint::new(Class::General, Tag::OP)).unwrap_or_else(|| panic!("take {b}: {:?}", stats()));
            tx.send((p.cast::<u8>().as_ptr() as usize, b)).unwrap();
        }
    });
    let giver = std::thread::spawn(move || {
        for (a, b) in rx {
            // SAFETY: taken by the other thread, handed over, not used again.
            unsafe { give(NonNull::new(a as *mut u8).unwrap(), b) };
        }
    });
    taker.join().unwrap();
    giver.join().unwrap();
    balanced();
}

fn dead_pages_round_trip() {
    let p = take(8 * RUN, RUN, Hint::new(Class::Process, Tag::REGION)).unwrap().cast::<u8>();
    let s0 = stats();
    // SAFETY: inside our own block.
    unsafe { give_pages(p, RUN, 4 * RUN) };
    assert_eq!(stats().dead, s0.dead + 4 * RUN);
    assert_eq!(run_of((p.as_ptr() as usize + 2 * RUN) as *const u8).unwrap().kind, Kind::Dead);
    // SAFETY: as above.
    assert_eq!(unsafe { take_pages(p, RUN, 4 * RUN) }, Zeroed::No);
    // SAFETY: as above.
    unsafe {
        core::ptr::write_bytes(p.as_ptr().add(RUN), 0x5a, 4 * RUN);
        give_pages(p, RUN, 4 * RUN);
    }
    assert!(api::release_dead(usize::MAX, false) >= 4 * RUN);
    // SAFETY: as above.
    assert_eq!(unsafe { take_pages(p, RUN, 4 * RUN) }, Zeroed::Yes);
    // SAFETY: as above: released pages read zero.
    assert!(unsafe { core::slice::from_raw_parts(p.as_ptr().add(RUN), 4 * RUN) }.iter().all(|&b| b == 0));
    // SAFETY: ours, not used again.
    unsafe { give_pages(p, 0, 2 * RUN) };
    // SAFETY: a give revives what is dead.
    unsafe { give(p, 8 * RUN) };
    flush_shelf();
    balanced();
}

fn purge_homes_everything() {
    flush_shelf();
    let got = purge_now(usize::MAX, None);
    let s = stats();
    assert_eq!(s.central, 0, "the return pass took every central list");
    balanced();
    assert_eq!(s.claimed, 0, "every superpage went back ({got} B): {s:?}");
}

#[test]
fn debug_poison_is_checked() {
    let mut buf = vec![0u8; 4096];
    let p = buf.as_mut_ptr() as usize;
    api::poison(p, 4096);
    api::check_poison(p, 4096);
    // SAFETY: inside `buf`, through the pointer the checks read.
    unsafe { *(p as *mut u8).add(100) = 1 };
    let r = std::panic::catch_unwind(|| api::check_poison(p, 4096));
    assert!(r.is_err(), "a write after give is caught");
}
