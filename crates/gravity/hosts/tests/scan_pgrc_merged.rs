//! sortmerge T2: `MergedRangeSource` over in-process pgrc tables whose parts are each sorted on a cluster key (the seal's
//! sort, as a `sorted_by` table's writers seal them): the merged rows are every part's rows in key order, ties to the lower
//! part, at 1 / 2 / 17 / 200 parts, with every stream live and with a pool of 2 live windows.

use std::sync::Arc;

use gravity_abi::Lane;
use gravity_driver::{blank_site, Source, Window};
use gravity_hosts::ops::scan::pgrc::granule_source::{GranuleRecipe, PartList, Resolved};
use gravity_hosts::ops::scan::pgrc::merged::MergedRangeSource;
use gravity_program::{Arena, SiteDesc, ZoneOp, ZonePred};
use pgrc_format::class::StorageClass;
use pgrc_qa::corpus::{append_rows, finish, open_writer, BuiltFixture, Fixture, OracleVal};
use pgrc_qa::{int8_col, text_col};
use pgrc_read::PartRegistry;
use pgrc_write::structural::{ClusterKeyDecl, StructuralPolicy};
use pgrc_write::writer::PartCutPolicy;

#[path = "common/granule_fixtures.rs"]
mod granule_fixtures;
use granule_fixtures::*;

const REL: u32 = 4251;

/// A driver result's value, else a panic naming the statement's stashed PostgreSQL error.
trait Pg<T> {
    fn pg(self, what: &str) -> T;
}

impl<T> Pg<T> for gravity_driver::Result<T> {
    fn pg(self, what: &str) -> T {
        self.unwrap_or_else(|e| panic!("{what}: {e:?} ({:?})", gravity_hosts::take_error().map(|p| p.message().to_string())))
    }
}

/// A two-column table (`k` int8 or C text, `i` int8 = the input row) written by ONE writer that cuts a part every
/// `per_part` rows, each part sealed sorted on `k` (ASC NULLS LAST) when `sorted`.
fn table(name: &'static str, relf: u64, keys: &[Option<OracleVal>], text: bool, per_part: u64, sorted: bool) -> BuiltFixture {
    use pgrc_qa::adapters::Probe;
    use pgrc_write::publish::{effective_manifest, TxnVerdict};
    use pgrc_write::wvfs::WriteVfs;
    let rows = keys.len() as u64;
    let cut = if per_part % 64 == 0 { 64 } else { per_part as u32 };
    let fx = Fixture {
        name,
        dir: format!("/qa/t{relf}"),
        spc: 1663,
        db: 5,
        relfilenumber: relf,
        schema: vec![if text { text_col(1) } else { int8_col(1) }, int8_col(2)],
        oracle: vec![keys.to_vec(), (0..rows).map(|i| Some(OracleVal::Word(i))).collect()],
        plans: vec![],
        policy: PartCutPolicy { max_rows: per_part, max_bytes: 1 << 40, cut_granule_rows: cut },
    };
    let mut vfs = pgrc_qa::simvfs::SimVfs::new();
    vfs.mkdir_path(&fx.dir).expect("mkdir");
    let fxid = 900 + relf;
    let mut probe = Probe::new(TxnVerdict::Aborted);
    probe.mark(fxid, TxnVerdict::InProgress);
    let mut w = open_writer(&fx, fxid).expect("writer");
    if sorted {
        w.set_structural(StructuralPolicy::new().with_cluster_key(vec![ClusterKeyDecl::column(1)]));
    }
    append_rows(&mut vfs, &mut w, &fx, 0, rows).expect("append");
    finish(&mut vfs, &mut w, &fx).expect("finish");
    w.publish(&mut vfs, &probe).expect("publish");
    probe.mark(fxid, TxnVerdict::Committed);
    let files = vfs.snapshot_dir(&fx.dir);
    let manifest = effective_manifest(&mut vfs, &fx.dir, &probe, None).expect("effective").expect("committed generation");
    BuiltFixture { fx, files, manifest }
}

fn int_keys(rows: u64, f: impl Fn(u64) -> Option<i64>) -> Vec<Option<OracleVal>> {
    (0..rows).map(|i| f(i).map(|v| OracleVal::Word(v as u64))).collect()
}

fn lcg(s: &mut u64) -> u64 {
    *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    *s >> 33
}

/// The merged order the source must produce: every row by key (NULLS LAST), a tie to the lower part, then the part's
/// own order (the seal's stable sort: input order).
fn expected<K: Ord + Clone>(keys: &[Option<K>], list: &PartList) -> Vec<u64> {
    let mut part_of = Vec::with_capacity(keys.len());
    for (p, spec) in list.parts().iter().enumerate() {
        part_of.extend(std::iter::repeat_n(p, spec.rows as usize));
    }
    assert_eq!(part_of.len(), keys.len(), "the parts hold every input row");
    let mut rows: Vec<u64> = (0..keys.len() as u64).collect();
    rows.sort_by(|&a, &b| {
        let (ka, kb) = (&keys[a as usize], &keys[b as usize]);
        let k = match (ka, kb) {
            (None, None) => core::cmp::Ordering::Equal,
            (None, Some(_)) => core::cmp::Ordering::Greater,
            (Some(_), None) => core::cmp::Ordering::Less,
            (Some(x), Some(y)) => x.cmp(y),
        };
        k.then(part_of[a as usize].cmp(&part_of[b as usize])).then(a.cmp(&b))
    });
    rows
}

/// The source (boxed before its bind, as a Node's arena box holds it: a bound source never moves), its statement's
/// `Resolved` and its Node arena, dropped in that order (the source unbound first).
struct Merge {
    src: Box<MergedRangeSource>,
    _resolved: Box<Resolved>,
    _arena: Arena,
}

impl Drop for Merge {
    fn drop(&mut self) {
        self.src.unbind();
    }
}

/// A merged source over `bf` (lanes: `k` then `i`), keyed on lane 0, with `live` windows at most (`None`: the site's own).
fn merge(bf: &BuiltFixture, ino: u64, text: bool, live: Option<usize>, preds: &[ZonePred]) -> (Merge, Arc<PartList>) {
    let list = part_list_at(bf, ino);
    let arena = Arena::new(64 << 20);
    let a = arena.handle();
    let mut desc = SiteDesc::range_source(a, REL, &[1, 2]);
    assert!(desc.set_merge(&[1]));
    let classes = [if text { StorageClass::VarlenaVerbatim } else { StorageClass::ByvalWord { width: 8, signed: true } }, StorageClass::ByvalWord { width: 8, signed: true }];
    let recipe = GranuleRecipe::for_test_classes(REL, &[1, 2], &classes, Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Box::new(Resolved::for_recipe(&recipe, list.clone()));
    let mut src = MergedRangeSource::new(recipe, usize::MAX, &desc, a);
    if let Some(n) = live {
        src = src.with_live(n);
    }
    let mut src = Box::new(src);
    if !preds.is_empty() {
        src.zone(preds, &[0], &[0]);
    }
    src.bind(&binding_ref(&resolved)).pg("bind");
    (Merge { src, _resolved: resolved, _arena: arena }, list)
}

/// Every row the merge serves: (key cell, key null, i).
fn drain(src: &mut MergedRangeSource) -> Vec<(u64, bool, u64)> {
    src.seek(&[0, u64::MAX], &[0, 0]).pg("seek");
    let (mut v, mut n) = ([0u64; 2], [0u8; 2]);
    let mut out = Vec::new();
    while src.next(&mut v, &mut n).pg("next") {
        assert_eq!(n[1], 0, "the row number is never NULL");
        out.push((v[0], n[0] != 0, v[1]));
    }
    out
}

fn check_ints(name: &'static str, relf: u64, rows: u64, per_part: u64, live: Option<usize>, f: impl Fn(u64) -> Option<i64>) -> usize {
    let keys = int_keys(rows, &f);
    let bf = table(name, relf, &keys, false, per_part, true);
    let (mut m, list) = merge(&bf, relf * 1000, false, live, &[]);
    let want = expected(&(0..rows).map(&f).collect::<Vec<_>>(), &list);
    let got = drain(&mut m.src);
    let ids: Vec<u64> = got.iter().map(|r| r.2).collect();
    assert_eq!(ids.len(), want.len(), "{name}: every row once ({} parts)", list.len());
    assert!(ids == want, "{name}: the merged order over {} parts (live {}): first difference at row {:?}", list.len(), m.src.live(), ids.iter().zip(want.iter()).position(|(a, b)| a != b));
    // the key cells are the rows' own
    for &(cell, null, i) in &got {
        assert_eq!(if null { None } else { Some(cell as i64) }, f(i), "{name}: row {i}'s key");
    }
    // a rescan reads the same rows again
    let again: Vec<u64> = drain(&mut m.src).iter().map(|r| r.2).collect();
    assert!(again == want, "{name}: a second seek reads the same merge");
    m.src.unbind();
    list.len()
}

#[test]
fn one_part_reads_in_its_own_order() {
    let mut s = 7;
    let v: Vec<i64> = (0..5_000).map(|_| (lcg(&mut s) % 1000) as i64).collect();
    assert_eq!(check_ints("sm_one", 7601, 5_000, 1 << 20, None, |i| Some(v[i as usize])), 1);
}

#[test]
fn interleaved_parts_merge_at_2_17_and_200_parts() {
    let mut s = 11;
    let v2: Vec<i64> = (0..20_000).map(|_| (lcg(&mut s) % 50_000) as i64).collect();
    assert_eq!(check_ints("sm_two", 7602, 20_000, 10_048, None, |i| Some(v2[i as usize])), 2);
    let v17: Vec<i64> = (0..17 * 640).map(|_| (lcg(&mut s) % 3_000) as i64 - 1_500).collect();
    assert_eq!(check_ints("sm_seventeen", 7603, 17 * 640, 640, None, |i| Some(v17[i as usize])), 17);
    let v200: Vec<i64> = (0..200 * 64).map(|_| (lcg(&mut s) % 100_000) as i64).collect();
    assert_eq!(check_ints("sm_twohundred", 7604, 200 * 64, 64, None, |i| Some(v200[i as usize])), 200);
}

#[test]
fn a_pool_of_two_live_windows_merges_the_same_rows() {
    let mut s = 13;
    let v17: Vec<i64> = (0..17 * 640).map(|_| (lcg(&mut s) % 3_000) as i64).collect();
    assert_eq!(check_ints("sm_pool17", 7605, 17 * 640, 640, Some(2), |i| Some(v17[i as usize])), 17);
    let v200: Vec<i64> = (0..200 * 64).map(|_| (lcg(&mut s) % 100_000) as i64).collect();
    assert_eq!(check_ints("sm_pool200", 7606, 200 * 64, 64, Some(2), |i| Some(v200[i as usize])), 200);
    // multi-granule parts: a stream's window moves on inside its part while evicted streams reopen theirs
    let v5: Vec<i64> = (0..5 * 20_032).map(|_| (lcg(&mut s) % 1_000_000) as i64).collect();
    assert_eq!(check_ints("sm_pool_granules", 7607, 5 * 20_032, 20_032, Some(2), |i| Some(v5[i as usize])), 5);
}

#[test]
fn nulls_sort_last_and_ties_go_to_the_lower_part() {
    let mut s = 17;
    let v: Vec<Option<i64>> = (0..17 * 640).map(|_| match lcg(&mut s) % 10 { 0 => None, x => Some((x % 4) as i64 - 2) }).collect();
    assert_eq!(check_ints("sm_nulls_ties", 7608, 17 * 640, 640, None, |i| v[i as usize]), 17);
    assert_eq!(check_ints("sm_nulls_ties_pool", 7609, 17 * 640, 640, Some(3), |i| v[i as usize]), 17);
    assert_eq!(check_ints("sm_all_null", 7610, 4 * 640, 640, None, |_| None), 4);
}

#[test]
fn disjoint_parts_publish_whole_windows() {
    // ascending parts: each part's keys after the last part's, so every run is a whole window (a granule)
    let rows = 6 * 20_032;
    let keys = int_keys(rows, |i| Some(i as i64 * 3));
    let bf = table("sm_disjoint", 7611, &keys, false, 20_032, true);
    let (mut m, list) = merge(&bf, 7_611_000, false, None, &[]);
    m.src.seek(&[0, u64::MAX], &[0, 0]).pg("seek");
    let mut lanes = [Lane::NONE; MAX_LANES];
    let (mut windows, mut rows_out, mut last) = (0u64, 0u64, i64::MIN);
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = m.src.window(&mut w).pg("window");
        if n == 0 {
            break;
        }
        assert_eq!((w.cursor, w.rows), (0, n), "a disjoint part's window is published whole");
        let l = w.lanes()[0];
        for r in w.cursor as usize..w.rows as usize {
            let k = unsafe { l.datum(r) } as i64;
            assert!(k > last, "keys ascend across windows");
            last = k;
        }
        windows += 1;
        rows_out += n as u64;
    }
    assert_eq!((windows, rows_out), (list.granules(), rows), "one window per granule, every row");
    // descending parts: the last part's keys first; still one run per granule
    let desc_key = |i: u64| Some((5 - i as i64 / 20_032) * 1_000_000 + (i % 20_032) as i64);
    let keys = int_keys(rows, desc_key);
    let bf = table("sm_disjoint_desc", 7612, &keys, false, 20_032, true);
    let (mut m, list) = merge(&bf, 7_612_000, false, None, &[]);
    let want = expected(&(0..rows).map(desc_key).collect::<Vec<_>>(), &list);
    assert_eq!(drain(&mut m.src).iter().map(|r| r.2).collect::<Vec<_>>(), want);
    assert_eq!(m.src.granules_decoded(), list.granules(), "each granule decoded once: no stream reopened");
}

#[test]
fn text_keys_merge_in_c_byte_order() {
    let mut s = 19;
    let words: Vec<Vec<u8>> = (0..17 * 640).map(|_| {
        let n = 1 + lcg(&mut s) % 12;
        (0..n).map(|_| b'a' + (lcg(&mut s) % 26) as u8).collect()
    }).collect();
    let keys: Vec<Option<OracleVal>> = words.iter().enumerate().map(|(i, w)| (i % 11 != 0).then(|| OracleVal::Bytes(w.clone()))).collect();
    let bf = table("sm_text", 7613, &keys, true, 640, true);
    for live in [None, Some(2)] {
        let (mut m, list) = merge(&bf, 7_613_000 + live.unwrap_or(0) as u64 * 100, true, live, &[]);
        assert_eq!(list.len(), 17);
        let want = expected(&words.iter().enumerate().map(|(i, w)| (i % 11 != 0).then(|| w.clone())).collect::<Vec<_>>(), &list);
        let got: Vec<u64> = drain(&mut m.src).iter().map(|r| r.2).collect();
        assert!(got == want, "text merge (live {live:?}): first difference at {:?}", got.iter().zip(want.iter()).position(|(a, b)| a != b));
        m.src.unbind();
    }
}

#[test]
fn zone_skipped_granules_keep_the_order() {
    let mut s = 23;
    let rows = 5 * 20_032;
    let v: Vec<i64> = (0..rows).map(|_| (lcg(&mut s) % 1_000_000) as i64).collect();
    let keys = int_keys(rows, |i| Some(v[i as usize]));
    let bf = table("sm_zone", 7614, &keys, false, 20_032, true);
    let (mut m, list) = merge(&bf, 7_614_000, false, Some(2), &[ZonePred::word(0, ZoneOp::Lt, 300_000)]);
    let got = drain(&mut m.src);
    let want: Vec<u64> = expected(&v.iter().map(|&k| Some(k)).collect::<Vec<_>>(), &list).into_iter().filter(|&i| v[i as usize] < 300_000).collect();
    // the zone verdicts skip granules (and select rows) only: every matching row comes, in order; rows past the bound
    // that share a kept granule may come too (the program's filter drops them)
    let kept: Vec<u64> = got.iter().map(|r| r.2).filter(|&i| v[i as usize] < 300_000).collect();
    assert_eq!(kept, want, "every matching row, in merged order");
    assert!(got.windows(2).all(|p| p[0].0 as i64 <= p[1].0 as i64), "the served keys ascend");
}

#[test]
fn a_part_without_a_sort_witness_is_refused() {
    let keys = int_keys(2 * 640, |i| Some(i as i64 % 97));
    let bf = table("sm_unsorted", 7615, &keys, false, 640, false);
    let list = part_list_at(&bf, 7_615_000);
    let arena = Arena::new(16 << 20);
    let a = arena.handle();
    let mut desc = SiteDesc::range_source(a, REL, &[1, 2]);
    assert!(desc.set_merge(&[1]));
    let recipe = GranuleRecipe::for_test(REL, &[1, 2], Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Resolved::for_recipe(&recipe, list);
    let mut src = MergedRangeSource::new(recipe, usize::MAX, &desc, a);
    assert!(src.bind(&binding_ref(&resolved)).is_err(), "an unattested part is the statement's error, never a wrong order");
}

#[test]
fn a_claim_of_part_of_the_relation_is_refused() {
    let keys = int_keys(2 * 640, |i| Some(i as i64));
    let bf = table("sm_claim", 7616, &keys, false, 640, true);
    let (mut m, _) = merge(&bf, 7_616_000, false, None, &[]);
    assert!(m.src.seek(&[1, 2], &[0, 0]).is_err(), "a merged site is read whole");
    m.src.unbind();
}

#[test]
fn a_stop_mid_run_releases_and_resumes() {
    let mut s = 29;
    let rows = 17 * 640;
    let v: Vec<i64> = (0..rows).map(|_| (lcg(&mut s) % 500) as i64).collect();
    let keys = int_keys(rows, |i| Some(v[i as usize]));
    let bf = table("sm_stop", 7617, &keys, false, 640, true);
    let (mut m, list) = merge(&bf, 7_617_000, false, Some(4), &[]);
    let want = expected(&v.iter().map(|&k| Some(k)).collect::<Vec<_>>(), &list);
    m.src.seek(&[0, u64::MAX], &[0, 0]).pg("seek");
    let (mut val, mut n) = ([0u64; 2], [0u8; 2]);
    let mut got = Vec::new();
    while m.src.next(&mut val, &mut n).pg("next") {
        got.push(val[1]);
        if got.len() % 37 == 0 {
            m.src.release();
        }
    }
    assert!(got == want, "release between rows keeps the merge");
    m.src.unbind();
}
