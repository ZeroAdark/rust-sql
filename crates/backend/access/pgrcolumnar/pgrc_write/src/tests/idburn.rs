//! IDBURN (pgrc-plans/IDBURN.done, main 10-05): a bulk writer's row-ID grants start at one virtual block (291 IDs) and
//! grow 4x, capped at what the part's cut leaves; a grant that starts where the last ended extends one range (an
//! implicit map, no sidecar), and grants that land apart make a part homing several ranges, published with a
//! `RowIdentMap` (ROWMAP2 M1: the granules form, MONO, the ranges' runs).

use super::*;
use pgrc_format::manifest::{range_entries, MANIFESTF_ROWMAP2, PART_FLAG_ROWMAP, PART_ROWMAP_FORM_SHIFT, PART_ROWMAP_GRANULES};
use pgrc_format::rowmap::{GranMap, RevForm};
use pgrc_format::sidecar::SidecarFileHeader;

/// Append `rows` rows, granting each grant the writer asks for at `at(k, next)` for its k-th grant; the grant sizes.
fn load(w: &mut TableWriter, vfs: &mut MemVfs, kit: &mut Kit, rows: u64, next: &mut u64, at: impl Fn(usize, u64) -> u64) -> Vec<(u64, u64)> {
    let mut grants = Vec::new();
    for i in 0..rows {
        if let Some(n) = w.needs_ids() {
            let lo = at(grants.len(), *next);
            w.grant_ids(lo, lo + n).expect("grant");
            grants.push((lo, n));
            *next = lo + n;
        }
        assert!(w.next_id().is_some(), "row {i} has an ID");
        append_int8_rows(w, vfs, kit, 1, |_| Some(i as i64));
    }
    grants
}

#[test]
fn idburn_grants_grow_4x_to_the_cut_and_extend_one_range() {
    let mut vfs = mem_with_dir();
    let mut kit = Kit::new();
    let policy = PartCutPolicy { max_rows: 30_000, max_bytes: u64::MAX, cut_granule_rows: 1_000 };
    let mut w = open_writer_policy(vec![int8_col(1)], stamp(100, 1), policy);
    let mut next = 291;
    let grants = load(&mut w, &mut vfs, &mut kit, 30_010, &mut next, |_, n| n);
    let sizes: Vec<u64> = grants.iter().map(|g| g.1).collect();
    assert_eq!(sizes, vec![291, 1_164, 4_656, 18_624, 5_529, 291], "291, then 4x, the last capped at align_up(30,000 - 24,735); a new part starts over");
    assert_eq!(w.take_released(), vec![], "part 0's last grant was used to its aligned end");
    let out = finish_and_publish(&mut w, &mut vfs, &mut kit, &Probe::new(TxnVerdict::Committed));
    assert_eq!(w.take_released(), vec![], "10 rows fill one block");
    assert_eq!(out.part_nos, vec![0, 1]);
    assert_eq!(range_entries(&out.manifest).expect("ranges"), vec![(0, vec![(291, 291 + 30_264)]), (1, vec![(30_555, 30_555 + 291)])], "contiguous grants are one range");
    assert!(out.manifest.parts.iter().all(|p| p.flags == 0), "implicit maps");
    assert_eq!(out.manifest.header.flags & MANIFESTF_ROWMAP2, 0, "no granules map: a pre-M1 binary still reads the table");
    let names = vfs.list_dir(DIR).expect("list");
    assert!(!names.iter().any(|n| n.contains("rowmap")), "no RowIdentMap sidecar: {names:?}");
}

#[test]
fn idburn_grants_that_land_apart_make_a_part_with_a_runs_map() {
    let mut vfs = mem_with_dir();
    let mut kit = Kit::new();
    let policy = PartCutPolicy { max_rows: 2_000, max_bytes: u64::MAX, cut_granule_rows: 100 };
    let mut w = open_writer_policy(vec![int8_col(1)], stamp(100, 1), policy);
    let mut next = 291;
    // another writer took [582, 10,185) between this writer's two grants
    let grants = load(&mut w, &mut vfs, &mut kit, 1_000, &mut next, |k, n| if k == 1 { 10_185 } else { n });
    assert_eq!(grants, vec![(291, 291), (10_185, 1_164)]);
    let out = finish_and_publish(&mut w, &mut vfs, &mut kit, &Probe::new(TxnVerdict::Committed));
    assert_eq!(w.take_released(), vec![(10_185 + 873, 10_185 + 1_164)], "the unused tail");
    assert_eq!(w.take_released(), vec![], "taken once");
    assert_eq!(range_entries(&out.manifest).expect("ranges"), vec![(0, vec![(291, 582), (10_185, 10_185 + 873)])], "the last range cut to align_up(709)");
    assert_eq!(out.manifest.parts[0].flags, PART_FLAG_ROWMAP | PART_ROWMAP_GRANULES << PART_ROWMAP_FORM_SHIFT);
    assert_ne!(out.manifest.header.flags & MANIFESTF_ROWMAP2, 0, "a granules map sets the manifest's ROWMAP2 bit");
    let bytes = vfs.read_full(&format!("{DIR}/part-0-rowmap-1.pgrcs")).expect("the map");
    let (_, body) = SidecarFileHeader::validate_file(&bytes).expect("sidecar");
    let map = GranMap::decode(body, 1_000).expect("granules");
    assert_eq!(map.rev_form(), RevForm::Mono);
    let mut ids = vec![0u64; 1_000];
    map.decode_ids(0, &mut ids);
    let want: Vec<u64> = (291..582).chain(10_185..10_185 + 709).collect();
    assert_eq!(ids, want);
}

/// IDBURN (d): an aborted writer gives nothing back (its grants stay burned, never handed out again), and a writer that
/// cut a part mid-statement releases that part's tail at the cut, before the statement ends.
#[test]
fn idburn_a_cut_releases_its_tail_and_an_abort_releases_nothing() {
    let mut vfs = mem_with_dir();
    let mut kit = Kit::new();
    let policy = PartCutPolicy { max_rows: 10_000, max_bytes: 1, cut_granule_rows: 1_000 };
    let mut w = open_writer_policy(vec![int8_col(1)], stamp(100, 1), policy);
    let mut next = 291;
    let grants = load(&mut w, &mut vfs, &mut kit, 1_500, &mut next, |_, n| n);
    assert_eq!(grants, vec![(291, 291), (582, 1_164), (1_746, 291), (2_037, 1_164)], "the byte budget cut at 1,000 rows; the next part starts over at 291 IDs");
    assert_eq!(w.sealed_parts()[0].ids, vec![(291, 291 + 1_164)], "1,000 rows: align_up = 1,164");
    assert_eq!(w.take_released(), vec![(291 + 1_164, 291 + 1_455)]);
    w.abort(&mut vfs).expect("abort");
    assert_eq!(w.take_released(), vec![], "an abort gives nothing back");
    assert_eq!(w.needs_ids(), Some(291), "a fresh part's first grant");
    assert_eq!(w.next_id(), None);
}
