//! `runs::Heap`: the k-way merge over sorted sources, ties broken by source number (the caller's `less`).

use crate::runs::Heap;

fn lcg(state: &mut u64) -> u64 {
    *state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    *state >> 33
}

/// Sorted sources of (key, payload) rows: `k` sources, each of up to `max_rows` rows with keys below `span`.
fn sources(k: usize, max_rows: u64, span: u64, seed: u64) -> Vec<Vec<(u64, u64)>> {
    let mut st = seed;
    (0..k)
        .map(|s| {
            let n = lcg(&mut st) % (max_rows + 1);
            let mut v: Vec<(u64, u64)> = (0..n).map(|i| (lcg(&mut st) % span, (s as u64) << 32 | i)).collect();
            v.sort_by_key(|r| r.0);
            v
        })
        .collect()
}

/// Merge `srcs` through the heap (stopping and resuming it every `pause` rows when non-zero): the rows in merge order.
fn merge(srcs: &[Vec<(u64, u64)>], pause: usize) -> Vec<(u64, u64)> {
    let mut pos = vec![0usize; srcs.len()];
    let mut slots: Vec<u32> = (0..srcs.len() as u32).filter(|&s| !srcs[s as usize].is_empty()).collect();
    let live = slots.len();
    let key = |pos: &[usize], s: u32| srcs[s as usize][pos[s as usize]].0;
    let mut out = Vec::new();
    let mut len = Heap::new(&mut slots, live, &mut |a, b| (key(&pos, a), a) < (key(&pos, b), b)).len();
    loop {
        let mut h = Heap::resume(&mut slots, len);
        let mut taken = 0usize;
        while let Some(s) = h.top() {
            out.push(srcs[s as usize][pos[s as usize]]);
            pos[s as usize] += 1;
            let done = pos[s as usize] == srcs[s as usize].len();
            let mut less = |a: u32, b: u32| (key(&pos, a), a) < (key(&pos, b), b);
            if done {
                h.pop_top(&mut less);
            } else {
                h.fix_top(&mut less);
            }
            taken += 1;
            if pause > 0 && taken == pause {
                break;
            }
        }
        len = h.len();
        if len == 0 {
            return out;
        }
    }
}

/// The oracle: every row, stably by key in source order (a tie goes to the lower source, then its row order).
fn oracle(srcs: &[Vec<(u64, u64)>]) -> Vec<(u64, u64)> {
    let mut all: Vec<(u64, u64)> = srcs.concat();
    all.sort_by_key(|r| (r.0, r.1));
    all
}

#[test]
fn heap_merges_sorted_sources_in_order() {
    let srcs: Vec<Vec<(u64, u64)>> = vec![vec![(1, 5), (3, 1), (3, 9)], vec![], vec![(0, 7), (3, 2), (9, 0)], vec![(2, 2)]];
    let out: Vec<u64> = merge(&srcs, 0).iter().map(|r| r.0).collect();
    assert_eq!(out, vec![0, 1, 2, 3, 3, 3, 9]);
    for (k, rows, span, seed) in [(1, 50, 10, 1u64), (2, 300, 40, 2), (17, 200, 1000, 3), (64, 40, 5, 4), (200, 30, 1 << 20, 5)] {
        let srcs = sources(k, rows, span, seed);
        assert_eq!(merge(&srcs, 0), oracle(&srcs), "k {k}");
    }
}

#[test]
fn heap_ties_go_to_the_lower_source_in_row_order() {
    let srcs: Vec<Vec<(u64, u64)>> = (0..9u64).map(|s| (0..4).map(|i| (7, s << 32 | i)).collect()).collect();
    assert_eq!(merge(&srcs, 0), oracle(&srcs));
}

#[test]
fn heap_resumes_over_its_lent_slots() {
    for pause in [1, 2, 7, 64] {
        let srcs = sources(33, 120, 300, 0x5EED + pause as u64);
        assert_eq!(merge(&srcs, pause), oracle(&srcs), "pause {pause}");
    }
}

#[test]
fn heap_runner_up_is_the_least_source_below_the_top() {
    let mut st = 99u64;
    for live in 0..40usize {
        let heads: Vec<u64> = (0..live).map(|_| lcg(&mut st) % 13).collect();
        let mut slots: Vec<u32> = (0..live as u32).rev().collect();
        let mut less = |a: u32, b: u32| (heads[a as usize], a) < (heads[b as usize], b);
        let h = Heap::new(&mut slots, live, &mut less);
        let mut order: Vec<u32> = (0..live as u32).collect();
        order.sort_by_key(|&s| (heads[s as usize], s));
        assert_eq!(h.top(), order.first().copied(), "live {live}");
        assert_eq!(h.runner_up(&mut less), order.get(1).copied(), "live {live}");
    }
}

#[test]
fn heap_takes_more_than_u16_sources() {
    let k = 70_001usize;
    let srcs: Vec<Vec<(u64, u64)>> = (0..k as u64).map(|s| vec![((s * 7919) % 70_001, s << 32)]).collect();
    let keys: Vec<u64> = merge(&srcs, 0).iter().map(|r| r.0).collect();
    assert_eq!(keys, (0..k as u64).collect::<Vec<u64>>(), "7919 is a unit mod 70,001: each key once");
}

#[test]
fn heap_pop_keeps_every_source_in_the_slots() {
    let heads = [5u64, 1, 4, 1, 3];
    let mut slots: Vec<u32> = (0..5).collect();
    let mut less = |a: u32, b: u32| (heads[a as usize], a) < (heads[b as usize], b);
    let mut h = Heap::new(&mut slots, 5, &mut less);
    let mut popped = Vec::new();
    while let Some(s) = h.top() {
        popped.push(s);
        h.pop_top(&mut less);
    }
    assert_eq!(popped, vec![1, 3, 4, 2, 0]);
    let mut all = slots.clone();
    all.sort_unstable();
    assert_eq!(all, vec![0, 1, 2, 3, 4]);
}
