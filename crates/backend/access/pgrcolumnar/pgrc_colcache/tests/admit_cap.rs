//! One test: the cap and the counters are process-wide. `PGRC_ADMIT_STRESS_SECS` (default 3) sets the run.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pgrc_read::mem::{self, Charge};

const K: usize = 64 << 10;
/// A re-decoded block, and one that grew: 2N < N2 <= K, so a check that loses K admits a version the cap refuses.
const N: usize = 4 << 10;
const N2: usize = 32 << 10;
/// Published once and pinned: keeps the counters clear of 0.
const BG: usize = 256 << 10;
const PAIRS: usize = 2;

struct Img(Vec<Arc<Charge>>);

impl Img {
    fn publish(&self) {
        self.0.iter().for_each(|c| c.publish());
    }

    fn unpublish(&self, keep: Option<&Img>) {
        for (i, c) in self.0.iter().enumerate() {
            if !keep.is_some_and(|k| Arc::ptr_eq(&k.0[i], c)) {
                c.unpublish();
            }
        }
    }
}

type Slot = pgsync::Mutex<Option<Arc<Img>>>;

fn unmap(slot: &Slot) {
    let old = pgsync::lock(slot).take();
    if let Some(o) = old {
        o.unpublish(None);
    }
}

fn owner(slot: &Slot, stop: &AtomicBool, admits: &AtomicU64) {
    let mut i = 0u64;
    while !stop.load(Ordering::Relaxed) {
        i += 1;
        let base = pgsync::lock(slot).clone();
        let k = base.as_ref().map_or_else(|| Arc::new(Charge::new(K)), |b| Arc::clone(&b.0[0]));
        let img = Arc::new(Img(vec![k, Arc::new(Charge::new(if i % 2 == 0 { N } else { N2 }))]));
        if !mem::make_room() {
            unmap(slot);
            continue;
        }
        img.publish();
        let old = pgsync::lock(slot).replace(Arc::clone(&img));
        if let Some(o) = old {
            o.unpublish(Some(&img));
        }
        admits.fetch_add(1, Ordering::Relaxed);
        drop(base);
    }
}

fn unmapper(slot: &Slot, stop: &AtomicBool, mut x: u64) {
    while !stop.load(Ordering::Relaxed) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        for _ in 0..x % 256 {
            std::hint::spin_loop();
        }
        unmap(slot);
    }
}

#[test]
fn concurrent_admissions_and_unmaps_keep_cached_under_the_cap() {
    let secs = std::env::var("PGRC_ADMIT_STRESS_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(3u64);
    let bg = Charge::new(BG);
    assert!(mem::make_room(), "the default cap has room");
    bg.publish();
    let s0 = mem::stats();
    assert_eq!((s0.cached, s0.held), (BG as u64, 0), "this process holds nothing else");
    // Room for every line at its small version plus one more re-decode: a refresh of N is admitted beside its pinned
    // base, one of N2 is refused.
    let cap = s0.cached + (PAIRS * (K + 2 * N)) as u64;
    mem::set_cap(cap);

    let slots: Vec<Slot> = (0..PAIRS).map(|_| pgsync::Mutex::new(None)).collect();
    let stop = AtomicBool::new(false);
    let admits = AtomicU64::new(0);
    let (mut samples, mut overs, mut peak) = (0u64, 0u64, 0u64);
    std::thread::scope(|sc| {
        for (p, slot) in slots.iter().enumerate() {
            let (stop, admits) = (&stop, &admits);
            sc.spawn(move || owner(slot, stop, admits));
            sc.spawn(move || unmapper(slot, stop, 0x9e37_79b9_7f4a_7c15 ^ (p as u64 + 1)));
        }
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            let s = mem::stats();
            samples += 1;
            peak = peak.max(s.cached);
            overs += u64::from(s.cached > s.cap);
        }
        stop.store(true, Ordering::Relaxed);
    });
    let s1 = mem::stats();
    eprintln!(
        "admit_cap: {secs} s, samples {samples}, over {overs}, peak cached {peak}, cap {cap}, admits {}, refused {}",
        admits.load(Ordering::Relaxed),
        s1.over_cap - s0.over_cap
    );
    assert_eq!(overs, 0, "cached over the cap in {overs} of {samples} samples (peak {peak}, cap {cap})");
    assert!(admits.load(Ordering::Relaxed) > 0 && s1.over_cap > s0.over_cap, "the lines ran at the cap: some admitted, some refused");

    slots.iter().for_each(unmap);
    let s2 = mem::stats();
    assert_eq!((s2.cached, s2.held), (s0.cached, s0.held), "every line's byte is credited");
    drop(bg);
    mem::set_cap(mem::DEFAULT_CAP);
}
