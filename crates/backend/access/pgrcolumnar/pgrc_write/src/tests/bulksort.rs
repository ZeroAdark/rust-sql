//! IXBULKSORT (pgrc-plans/FOLLOWUPS.done, main 10-08): a bulk row takes the ID of its arrival position at its append
//! (`TableWriter::next_id`), and a declared cluster key sorts its part at the seal, so the part's IDs follow the sealed
//! order and a row's handed ID can be another row's. [`Placed`] maps a handed ID to the row's own: through the inverse
//! of the seal's sort permutation over the part's ID ranges.

use super::*;
use crate::structural::{ClusterKeyDecl, NullsOrder, SortDir, StructuralPolicy};
use crate::writer::Placed;

const ASC: ClusterKeyDecl = ClusterKeyDecl { attno: 1, dir: SortDir::Asc, nulls: NullsOrder::Last, path: Vec::new() };

/// Deterministic shuffle (LCG, the in-suite idiom).
fn shuffled(n: u64, seed: u64) -> Vec<u64> {
    let mut v: Vec<u64> = (0..n).collect();
    let mut state = seed;
    for i in (1..v.len()).rev() {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        v.swap(i, (state >> 33) as usize % (i + 1));
    }
    v
}

#[test]
fn placed_maps_a_handed_id_through_the_inverse_permutation_over_the_ranges() {
    // two ranges (another writer took [582, 10,185) between the grants); 1,000 rows sealed in reverse arrival order
    let ids = vec![(291, 582), (10_185, 10_185 + 873)];
    let handed = |a: u64| if a < 291 { 291 + a } else { 10_185 + (a - 291) };
    let p = Placed::new(ids.clone(), 1_000, Some((0..1_000u32).rev().collect()));
    assert_eq!(p.place(handed(0)), Some(handed(999)), "the first arrival is sealed last");
    assert_eq!(p.place(handed(999)), Some(291), "the last arrival is sealed first");
    assert_eq!(p.place(handed(290)), Some(handed(709)), "across the ranges' seam");
    assert_eq!(p.place(handed(1_000)), None, "past the part's rows: the aligned tail no row was handed");
    assert_eq!(p.place(290), None);
    assert_eq!(p.place(582), None, "between the ranges");
    let mut placed: Vec<u64> = (0..1_000).map(|a| p.place(handed(a)).expect("a row of the part")).collect();
    placed.sort_unstable();
    assert_eq!(placed, (0..1_000).map(handed).collect::<Vec<_>>(), "a bijection onto the part's first 1,000 IDs");
    let identity = Placed::new(ids, 1_000, None);
    assert!((0..1_000).all(|a| identity.place(handed(a)) == Some(handed(a))), "a seal in arrival order keeps them");
}

/// Load `keys` (one int8 row each) through a writer that grants IDs from 291 on as it asks; each row's handed ID.
fn load(w: &mut TableWriter, vfs: &mut MemVfs, kit: &mut Kit, keys: &[u64]) -> Vec<u64> {
    let mut next = 291;
    let mut handed = Vec::with_capacity(keys.len());
    for &k in keys {
        if let Some(n) = w.needs_ids() {
            w.grant_ids(next, next + n).expect("grant");
            next += n;
        }
        handed.push(w.next_id().expect("an ID under the grant"));
        append_int8_rows(w, vfs, kit, 1, |_| Some(k as i64));
    }
    let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
    let mut env = SealEnv { vfs, sources: &sources, resolver: &kit.resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
    w.finish(&mut env).expect("finish");
    handed
}

/// The born-red shape at the writer: a `sorted_by` table's bulk rows, two parts. A row's placed ID is the one of its
/// key's rank in its part (the seal's ascending order: `sorted_ingest`'s law), and most handed IDs are not.
#[test]
fn a_sorted_bulk_row_is_placed_at_its_keys_rank_in_its_part() {
    let mut vfs = mem_with_dir();
    let mut kit = Kit::new();
    let policy = PartCutPolicy { max_rows: 2_000, max_bytes: u64::MAX, cut_granule_rows: 100 };
    let mut w = open_writer_policy(vec![int8_col(1)], stamp(100, 1), policy);
    w.set_structural(StructuralPolicy::new().with_cluster_key(vec![ASC]));
    w.keep_placements();
    let keys: Vec<u64> = shuffled(3_000, 0x1B5_0127).into_iter().map(|v| v * 7).collect();
    let handed = load(&mut w, &mut vfs, &mut kit, &keys);
    let placed = w.take_placements();
    assert_eq!(placed.iter().map(Placed::rows).collect::<Vec<_>>(), vec![2_000, 1_000], "one per cut part");
    assert_eq!(placed.len(), w.sealed_parts().len());
    assert!(w.take_placements().is_empty(), "taken once");
    let mut moved = 0;
    for (part, rows) in [(0usize, 0..2_000usize), (1, 2_000..3_000)] {
        let ids = &w.sealed_parts()[part].ids;
        let mut order: Vec<u64> = keys[rows.clone()].to_vec();
        order.sort_unstable();
        for r in rows {
            let id = placed[part].place(handed[r]).expect("placed by its own part");
            assert_eq!(placed[1 - part].place(handed[r]), None, "and by no other");
            let rank = order.binary_search(&keys[r]).expect("its key") as u64;
            let mut pos = rank;
            let want = ids.iter().find_map(|&(lo, hi)| if pos < hi - lo { Some(lo + pos) } else { pos -= hi - lo; None });
            assert_eq!(Some(id), want, "row {r} (key {}) is sealed at its key's rank", keys[r]);
            moved += usize::from(id != handed[r]);
        }
    }
    assert!(moved > 2_900, "the handed IDs name other rows ({moved} of 3,000 moved)");
}

#[test]
fn placements_are_kept_only_when_asked_and_an_unsorted_part_keeps_its_handed_ids() {
    let policy = PartCutPolicy { max_rows: 2_000, max_bytes: u64::MAX, cut_granule_rows: 100 };
    let keys: Vec<u64> = shuffled(500, 7).into_iter().collect();
    let (mut vfs, mut kit) = (mem_with_dir(), Kit::new());
    let mut w = open_writer_policy(vec![int8_col(1)], stamp(100, 1), policy);
    w.set_structural(StructuralPolicy::new().with_cluster_key(vec![ASC]));
    load(&mut w, &mut vfs, &mut kit, &keys);
    assert!(w.take_placements().is_empty(), "not kept");
    assert!(w.seal_reports()[0].cluster_perm.is_none(), "the report never holds the permutation");
    let (mut vfs, mut kit) = (mem_with_dir(), Kit::new());
    let mut w = open_writer_policy(vec![int8_col(1)], stamp(100, 1), policy);
    w.keep_placements();
    let handed = load(&mut w, &mut vfs, &mut kit, &keys);
    let placed = w.take_placements();
    assert_eq!(placed.len(), 1);
    assert!(handed.iter().all(|&h| placed[0].place(h) == Some(h)), "no cluster key: arrival order");
    let (mut vfs, mut kit) = (mem_with_dir(), Kit::new());
    let mut w = open_writer_policy(vec![int8_col(1)], stamp(100, 1), policy);
    w.set_structural(StructuralPolicy::new().with_cluster_key(vec![ASC]));
    w.keep_placements();
    let sorted: Vec<u64> = (0..500).collect();
    let handed = load(&mut w, &mut vfs, &mut kit, &sorted);
    let placed = w.take_placements();
    assert!(handed.iter().all(|&h| placed[0].place(h) == Some(h)), "presorted input: the seal's verify-only path");
}
