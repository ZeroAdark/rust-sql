//! The registry and the installed compiler are process-wide: this file is its own test binary, its tests one at a time.

use std::sync::{Arc, Barrier};

use gravity_abi::breaker::{fits_inline_at, Candidates, HashAgg, KeyKind, Rec, RecordLayout, SlotKind, SubSplit, HASHAGG_HDR, MAX_RECORD_KEYS, MAX_RECORD_SLOTS};
use gravity_abi::hash::KeyHash;
use gravity_kernels::table::fold_jit::{self, SETS, WAYS};
use gravity_kernels::table::owner_fold::{owner_fold, Fold};
use std::sync::atomic::Ordering;

use SlotKind::*;

fn lcg(s: &mut u64) -> u64 {
    *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    *s >> 17
}

const KINDS: &[SlotKind] = &[Count, SumI64, SumI128, AvgI128, SumF64, MinCode, MaxCode, MinI64, MaxI64];

fn layouts(count: usize, seed: u64) -> Vec<RecordLayout> {
    let mut rng = seed;
    let mut out: Vec<RecordLayout> = Vec::new();
    let mut keys = std::collections::BTreeSet::new();
    while out.len() < count {
        let nw = 1 + (lcg(&mut rng) % MAX_RECORD_KEYS as u64) as usize;
        let ns = 1 + (lcg(&mut rng) % (MAX_RECORD_SLOTS as u64 - 1)) as usize;
        let slots: Vec<SlotKind> = (0..ns).map(|_| KINDS[(lcg(&mut rng) % KINDS.len() as u64) as usize]).collect();
        let keys_k = vec![KeyKind::I64; nw];
        if !fits_inline_at(&keys_k, &slots, true, None) {
            continue;
        }
        let l = RecordLayout::with_sentinel(&keys_k, &slots, true, None);
        if keys.insert(fold_jit::key(&l)) {
            out.push(l);
        }
    }
    out
}

fn records(layout: &RecordLayout, n: usize, domain: u64, seed: u64) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rng = seed;
    let mut buf = [0u8; 256];
    for _ in 0..n {
        buf.fill(0);
        let keys: Vec<u64> = (0..layout.nkeys()).map(|_| lcg(&mut rng) % domain).collect();
        let r = unsafe { Rec::at(buf.as_mut_ptr(), layout) };
        r.open(&keys);
        for j in 0..layout.nslots() {
            let s = lcg(&mut rng);
            if layout.slot_off(j) != layout.sentinel_off() && s % 4 == 0 {
                continue;
            }
            match layout.slot_kind(j) {
                Count => r.add_count(j, 1 + (s % 3) as i64),
                SumI64 => r.add_i64(j, (s % 1000) as i64 - 500),
                SumI128 => r.add_i128(j, (s % (1 << 40)) as i128 * 1_000_000_007),
                MinI64 => r.min_i64(j, (s % 5000) as i64 - 2500),
                MaxI64 => r.max_i64(j, (s % 5000) as i64 - 2500),
                AvgI128 => r.add_avg(j, (s % 5000) as i128, 1),
                SumF64 => r.add_f64(j, (s % 100) as f64 / 7.0).expect("finite"),
                MinCode => r.min_code(j, (s % 4) as u32, (s % 977) as u32),
                MaxCode => r.max_code(j, (s % 4) as u32, (s % 977) as u32),
                k => unreachable!("{k:?}"),
            }
        }
        out.extend_from_slice(&buf[..layout.size() as usize]);
    }
    out
}

fn fold_into(layout: &RecordLayout, recs: &[u8], n: usize, loops: bool) -> (i64, Vec<u64>, Vec<u32>) {
    let cap = 8192u32;
    let mut region = vec![0u64; (HASHAGG_HDR + cap as usize * layout.size() as usize) / 8 + 1];
    let mut cand = vec![0u32; 4096];
    let t = unsafe { HashAgg::at(region.as_mut_ptr() as *mut u8, layout, cap, (region.len() * 8) as u32) } as *mut HashAgg;
    let mut c = unsafe { Candidates::at(cand.as_mut_ptr() as *mut u8, 4096, layout.sentinel_off() as usize, 3) };
    let v = unsafe {
        if loops {
            Fold::of(layout).expect("a generated fold").run(t, recs.as_ptr(), n, SubSplit::NONE, &mut c)
        } else {
            owner_fold(core::ptr::null(), t, recs.as_ptr(), n, SubSplit::NONE, &mut c)
        }
    };
    cand.truncate(c.len);
    (v, region, cand)
}

static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn installed() -> std::sync::MutexGuard<'static, ()> {
    let g = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    fold_jit::install(gravity_exec::table::fold_code::compile);
    g
}

#[test]
fn eight_owners_meeting_one_new_layout_fold_alike() {
    let _one = installed();
    let layout = RecordLayout::with_sentinel(&[KeyKind::I64; 3], &[Count, SumI64, MinI64, SumI128, MaxCode], true, None);
    let (n, rec) = (20_000usize, layout.size() as usize);
    let recs = Arc::new(records(&layout, n, 14, 0x8));
    let want = fold_into(&layout, &recs, n, true);
    let before = fold_jit::COMPILED.load(Ordering::Relaxed);
    let go = Arc::new(Barrier::new(8));
    let owners: Vec<_> = (0..8)
        .map(|_| {
            let (go, recs) = (go.clone(), recs.clone());
            std::thread::spawn(move || {
                go.wait();
                let cap = 8192u32;
                let mut region = vec![0u64; (HASHAGG_HDR + cap as usize * rec) / 8 + 1];
                let mut cand = vec![0u32; 4096];
                let t = unsafe { HashAgg::at(region.as_mut_ptr() as *mut u8, &layout, cap, (region.len() * 8) as u32) } as *mut HashAgg;
                let mut c = unsafe { Candidates::at(cand.as_mut_ptr() as *mut u8, 4096, layout.sentinel_off() as usize, 3) };
                let mut v = 0;
                for chunk in recs.chunks(rec * 1000) {
                    v += unsafe { owner_fold(core::ptr::null(), t, chunk.as_ptr(), chunk.len() / rec, SubSplit::NONE, &mut c) };
                }
                cand.truncate(c.len);
                (v, region, cand)
            })
        })
        .collect();
    for o in owners {
        let got = o.join().expect("an owner");
        assert_eq!(got.0, want.0, "the records folded");
        assert!(got.1 == want.1, "the table bytes");
        assert!(got.2 == want.2, "the candidate list");
    }
    assert_eq!(fold_jit::COMPILED.load(Ordering::Relaxed) - before, 1, "one compile for the layout");
    assert!(fold_jit::find(&fold_jit::key(&layout)).is_some_and(|p| p.entry().is_some()), "the layout's fold filed");
}

#[test]
fn more_layouts_than_slots_evict_and_fold_right() {
    let _one = installed();
    let all = layouts(SETS * WAYS + 200, 0xe1);
    let evicted = fold_jit::EVICTED.load(Ordering::Relaxed);
    let mut max_bytes = 0u64;
    for round in 0..2 {
        for (i, layout) in all.iter().enumerate() {
            let n = 600;
            let recs = records(layout, n, 200, i as u64 ^ round);
            assert!(fold_into(layout, &recs, n, false) == fold_into(layout, &recs, n, true), "layout {i} round {round}");
            max_bytes = max_bytes.max(fold_jit::LIVE_BYTES.load(Ordering::Relaxed));
        }
    }
    assert!(fold_jit::EVICTED.load(Ordering::Relaxed) > evicted, "the registry evicted");
    let live = fold_jit::LIVE_BYTES.load(Ordering::Relaxed);
    eprintln!("registry: compiled {} evicted {} refused {} live bytes {} (max {})", fold_jit::COMPILED.load(Ordering::Relaxed), fold_jit::EVICTED.load(Ordering::Relaxed), fold_jit::REFUSED.load(Ordering::Relaxed), live, max_bytes);
    assert!(max_bytes <= (SETS * WAYS) as u64 * (64 << 10), "live code bounded by the slots: {max_bytes}");
}

#[test]
fn three_hundred_layouts_three_rounds_recompile_only_where_a_set_overflows() {
    let _one = installed();
    let set = |l: &RecordLayout| KeyHash::of(&fold_jit::key(l)).part(SETS.trailing_zeros());
    let all = layouts(300, 0xc7c);
    let mut per_set = std::collections::HashMap::new();
    for l in &all {
        *per_set.entry(set(l)).or_insert(0usize) += 1;
    }
    let crowded: usize = per_set.values().filter(|&&m| m > WAYS).sum();
    for round in 0..3u64 {
        let (compiled, evicted) = (fold_jit::COMPILED.load(Ordering::Relaxed), fold_jit::EVICTED.load(Ordering::Relaxed));
        for (i, layout) in all.iter().enumerate() {
            let n = 400;
            let recs = records(layout, n, 120, i as u64 ^ (round << 32));
            assert!(fold_into(layout, &recs, n, false) == fold_into(layout, &recs, n, true), "layout {i} round {round}");
        }
        let compiled = fold_jit::COMPILED.load(Ordering::Relaxed) - compiled;
        let evicted = fold_jit::EVICTED.load(Ordering::Relaxed) - evicted;
        eprintln!("registry: 300 layouts round {round}: compiled {compiled} evicted {evicted} (layouts in sets past {WAYS} ways: {crowded})");
        if round > 0 {
            assert!(compiled as usize <= crowded, "a recompile only in a set holding more than its ways: {compiled}");
        }
    }
}

#[test]
fn a_pinned_fold_outlives_its_set_turning_over() {
    let _one = installed();
    let set = |l: &RecordLayout| KeyHash::of(&fold_jit::key(l)).part(SETS.trailing_zeros());
    let all = layouts(SETS * WAYS * 6, 0x5e7);
    let first = all[0];
    let same: Vec<&RecordLayout> = all.iter().skip(1).filter(|l| set(l) == set(&first)).take(3 * WAYS).collect();
    assert!(same.len() >= 2 * WAYS, "layouts sharing a set");
    let recs = records(&first, 300, 50, 1);
    let _ = fold_into(&first, &recs, 300, false);
    let pin = fold_jit::find(&fold_jit::key(&first)).expect("filed");
    for l in &same {
        let r = records(l, 300, 50, 2);
        assert!(fold_into(l, &r, 300, false) == fold_into(l, &r, 300, true));
    }
    assert!(fold_jit::find(&fold_jit::key(&first)).is_some(), "the pinned fold stays");
    drop(pin);
    assert!(fold_into(&first, &recs, 300, false) == fold_into(&first, &recs, 300, true));
}

#[test]
fn a_sub_split_folds_through_the_loops() {
    let _one = installed();
    let layout = RecordLayout::with_sentinel(&[KeyKind::I64; 2], &[Count, SumI64, MaxCode], true, None);
    let n = 3000;
    let recs = records(&layout, n, 1 << 20, 0x5b);
    let _ = fold_into(&layout, &recs, n, false);
    let mut kept = 0;
    for sub in 0..64u64 {
        let split = SubSplit { pbits: 2, depth: 2, reread: 0, sub };
        let (mut got, mut want) = (Vec::new(), Vec::new());
        for (generic, out) in [(false, &mut got), (true, &mut want)] {
            let cap = 4096u32;
            let mut region = vec![0u64; (HASHAGG_HDR + cap as usize * layout.size() as usize) / 8 + 1];
            let mut cand = vec![0u32; 64];
            let t = unsafe { HashAgg::at(region.as_mut_ptr() as *mut u8, &layout, cap, (region.len() * 8) as u32) };
            t.set_route_bits(split.table_depth());
            let t = t as *mut HashAgg;
            let mut c = unsafe { Candidates::at(cand.as_mut_ptr() as *mut u8, 64, layout.sentinel_off() as usize, 2) };
            let v = unsafe {
                if generic {
                    (*t).fold_run(recs.as_ptr(), n, split, &mut c)
                } else {
                    owner_fold(core::ptr::null(), t, recs.as_ptr(), n, split, &mut c)
                }
            };
            cand.truncate(c.len);
            *out = vec![(v, region, cand)];
        }
        assert!(got == want, "sub {sub}");
        kept += got[0].0;
    }
    assert_eq!(kept, n as i64);
}
