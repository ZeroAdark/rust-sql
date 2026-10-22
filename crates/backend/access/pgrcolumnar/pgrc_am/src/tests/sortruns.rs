//! SORTRUNS-E0 L1 (pgrc-plans/SORTRUNS-E0-STEP1.md §1, D2): a generation's sorted runs home their IDs. A run-homed ID's
//! home is the run (`Generation::home`), found through the run's reverse (`runcache::RunCell`, its pages in the row-map
//! cache) and its member's map (`Generation::locate`); a bad reverse page is an error, never an answer. The runs are
//! the test-only writer's (`pgrc_write::testruns::build`).

use std::sync::Arc;

use pgrc_format::manifest::{with_next_row_id, with_ranges, IdRanges, Manifest, ManifestHeader, PartRecord};
use pgrc_format::rowmap::{encode, GranMap, Opts};
use pgrc_format::sortrun::{with_runs, RankForm, RunRevForm, SortRun};
use pgrc_write::testruns::{build, HomeRun};

use crate::catalog::{Generation, GranuleMap, IdHome, RowMap};
use crate::runcache::RunCell;

const B: u64 = 291;

/// A part record that decodes (the geometry echo and the footer bound hold).
fn rec(part_no: u32, rows: u64) -> PartRecord {
    use pgrc_format::geom::{band_count_at, granule_count_at, GranuleGrain};
    let g = GranuleGrain::DEFAULT;
    PartRecord { rows, file_len: 4096, footer_off: 2048, dv_gen: 0, dv_len: 0, part_no, flags: 0, granule_count: granule_count_at(rows, g) as u32, band_count: band_count_at(rows, g) as u32, dv_crc: 0, granule_rows: 0 }
}

fn header(gen: u64, nparts: u32) -> ManifestHeader {
    ManifestHeader { gen, prev_gen: 0, publisher_fxid: 1, relfilenumber: 9, schema_fingerprint: 1, magic: pgrc_format::manifest::MANIFEST_MAGIC, format_version: pgrc_format::FORMAT_VERSION, spc: 1663, db: 5, part_count: nparts, next_part_no: 40, flags: 0, reserved: 0 }
}

fn granules(map: &[u8], rows: u64) -> Arc<RowMap> {
    Arc::new(RowMap::Granules(Arc::new(GranuleMap::new(GranMap::decode(map, rows).unwrap()))))
}

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// A table of two parts: part 3, a bulk part homing `[B, 3B)` (an implicit map), and part 7, the sole member of run 9
/// that homes `ranges`; part 7's rows hold `ids` in ordinal order.
struct Table {
    gen: Generation,
    home: HomeRun,
    maps: Vec<Arc<RowMap>>,
}

fn table(form: RunRevForm, rank: Option<RankForm>, ids: &[u64], ranges: IdRanges) -> Table {
    let home = build(form, rank, 9, 7, ids, &ranges).unwrap();
    let ext = with_next_row_id(&with_ranges(&[], &[(3, vec![(B, 3 * B)])]).unwrap(), 4000 * B).unwrap();
    let ext = with_runs(&ext, std::slice::from_ref(&home.run)).unwrap();
    let m = Manifest::new(header(9, 2), vec![rec(3, 2 * B), rec(7, ids.len() as u64)]).with_ext(ext);
    // the one-home law over the part and the run, as every reader decodes it
    let m = Manifest::decode(&m.encode()).unwrap();
    let mut gen = Generation::detached(m).unwrap();
    gen.bind_run_cell(9, RunCell::image(&home.run, home.file.clone()));
    let maps = vec![Arc::new(RowMap::Implicit(B)), granules(&home.member_map, ids.len() as u64)];
    Table { gen, home, maps }
}

impl Table {
    fn locate(&self, id: u64) -> types_error::PgResult<Option<(usize, u32)>> {
        Ok(self.gen.locate(id, &mut |i| Ok(Arc::clone(&self.maps[i])))?.map(|(i, o, _)| (i, o)))
    }
}

/// IDs of the run's ranges in a shuffled order (a sorted run's member holds its rows in key order), some dropped.
fn shuffled(ranges: &IdRanges, keep_one_in: u64, seed: u64) -> Vec<u64> {
    let mut rng = Rng(seed);
    let mut ids: Vec<u64> = ranges.iter().flat_map(|&(lo, hi)| lo..hi).filter(|_| keep_one_in <= 1 || rng.below(keep_one_in) != 0).collect();
    for i in (1..ids.len()).rev() {
        let j = rng.below(i as u64 + 1) as usize;
        ids.swap(i, j);
    }
    ids
}

fn check_every(t: &Table, ids: &[u64], ranges: &IdRanges) {
    for (o, &id) in ids.iter().enumerate() {
        assert_eq!(t.gen.home(id), Some(IdHome::Run(0)), "{id} is the run's");
        assert_eq!(t.locate(id).unwrap(), Some((1, o as u32)), "{id} at its member's ordinal");
    }
    let held: std::collections::BTreeSet<u64> = ids.iter().copied().collect();
    for id in ranges.iter().flat_map(|&(lo, hi)| lo..hi).filter(|id| !held.contains(id)) {
        assert_eq!(t.locate(id).unwrap(), None, "{id}: in the run's ranges, no row");
    }
    // the bulk part answers as before; outside every home, none
    assert_eq!(t.gen.home(B + 5), Some(IdHome::Part(0)));
    assert_eq!(t.locate(B + 5).unwrap(), Some((0, 5)));
    assert_eq!(t.gen.home(3 * B), None);
    assert_eq!(t.locate(3 * B).unwrap(), None);
}

/// D4537 T18c: a cell's open reverse and the pages it holds are the part cache's bytes, as many as the cell holds.
#[test]
fn sortruns_t18c_a_cell_charges_its_reverse_and_pages_to_the_part_cache() {
    let ranges = vec![(10 * B, 40 * B)];
    let ids = shuffled(&ranges, 1, 17);
    let t = table(RunRevForm::FrameIx, None, &ids, ranges);
    let cell = RunCell::image(&t.home.run, t.home.file.clone());
    assert_eq!(cell.charged(), (0, 0), "an unread cell charges nothing");
    cell.lookup(&t.home.run, ids[0]).unwrap();
    let rev = cell.rev(&t.home.run).unwrap();
    let (r, p) = cell.charged();
    assert_eq!(r, rev.mem_bytes() as u64, "the open reverse");
    assert!(cell.held() > 0);
    assert_eq!(p, cell.held() as u64 * (pgrc_format::runrev::PAGE + size_of::<pgrc_format::runrev::Page>()) as u64, "each held page");
}

#[test]
fn sortruns_l1_frameix_run_homes_its_ids() {
    let ranges = vec![(10 * B, 30 * B), (40 * B, 41 * B)];
    for (keep, seed) in [(1, 3), (9, 5)] {
        let ids = shuffled(&ranges, keep, seed);
        let t = table(RunRevForm::FrameIx, None, &ids, ranges.clone());
        assert_eq!(t.home.run.rank, RankForm::Dense);
        check_every(&t, &ids, &ranges);
        assert_eq!(t.gen.run_member(1), Some((0, 0)));
        assert_eq!(t.gen.run_member(0), None);
        assert!(!t.gen.pinned_member(1), "a run's sole member is a part like any other");
        assert!(t.gen.ids_of(1).is_empty(), "a member has no EXT_RANGES entry");
    }
}

#[test]
fn sortruns_l1_labels_run_homes_its_ids_through_groups_and_exceptions() {
    let ranges = vec![(100 * B, 160 * B)];
    // mostly ascending (the groups' fast path), with every 37th row swapped far away (exceptions)
    let mut ids: Vec<u64> = (100 * B..160 * B).collect();
    let n = ids.len();
    for i in (0..n).step_by(37) {
        ids.swap(i, n - 1 - i);
    }
    let t = table(RunRevForm::Labels, None, &ids, ranges.clone());
    let rev = t.gen.runs()[0].cell().unwrap().rev(&t.home.run).unwrap();
    assert!(rev.exceptions() > 0, "the swaps are exceptions");
    check_every(&t, &ids, &ranges);
    // shuffled: every row an exception or a lucky group hit
    let ids = shuffled(&ranges, 4, 11);
    let t = table(RunRevForm::Labels, None, &ids, ranges.clone());
    check_every(&t, &ids, &ranges);
}

#[test]
fn sortruns_l1_set_rank_run_homes_sparse_ids() {
    let ranges = vec![(1000 * B, 1100 * B)];
    let ids = shuffled(&ranges, 7, 13);
    // one ID in three: the ranges span more than twice the rows
    let sparse: Vec<u64> = ids.iter().copied().filter(|id| id % 3 == 0).collect();
    for form in [RunRevForm::FrameIx, RunRevForm::Labels] {
        let t = table(form, Some(RankForm::Set), &ids, ranges.clone());
        check_every(&t, &ids, &ranges);
        let t = table(form, None, &sparse, ranges.clone());
        assert_eq!(t.home.run.rank, RankForm::Set, "{form:?}: a sparse run ranks its ID set");
        check_every(&t, &sparse, &ranges);
    }
}

#[test]
fn sortruns_l1_an_emptied_slot_and_an_unbound_run() {
    use pgrc_format::rowmap::{encode_run_member, RUN_FRAME_SMALL};
    use pgrc_format::runrev::Spec;
    // a FRAMEIX run of two members: slot 0 held the first 300 IDs (every row since dropped: the slot emptied), slot 1,
    // part 7, holds the rest
    let ranges = vec![(10 * B, 12 * B)];
    let all: Vec<u64> = (10 * B..12 * B).collect();
    let (a, b) = all.split_at(300);
    let f = RUN_FRAME_SMALL as usize;
    let (nv0, nv1) = (a.len().div_ceil(f) as u32, b.len().div_ceil(f) as u32);
    let vstarts: Vec<u32> = (0..b.len()).step_by(f).map(|o| o as u32).collect();
    let member_map = encode_run_member(b, &vstarts, None).unwrap();
    let labels: Vec<u32> = (0..all.len()).map(|r| if r < a.len() { (r / f) as u32 } else { nv0 + ((r - a.len()) / f) as u32 }).collect();
    let spec = Spec { run_id: 9, form: RunRevForm::FrameIx, rank: RankForm::Dense, ranks: all.len() as u64, rows: all.len() as u64, nsym: nv0 + nv1, members: 2 };
    let (file, fin) = pgrc_format::runrev::encode(spec, &[], &labels, &[], &[], &[0, nv0, nv0 + nv1]).unwrap();
    let run = SortRun { run_id: 9, level: 1, rev: RunRevForm::FrameIx, rank: RankForm::Dense, rows: all.len() as u64, rev_len: fin.rev_len, rev_crc: fin.rev_crc, members: vec![0, 7], ranges };
    let ext = with_runs(&with_next_row_id(&[], 4000 * B).unwrap(), std::slice::from_ref(&run)).unwrap();
    let m = Manifest::decode(&Manifest::new(header(9, 1), vec![rec(7, b.len() as u64)]).with_ext(ext).encode()).unwrap();
    let mut gen = Generation::detached(m).unwrap();
    let map = granules(&member_map, b.len() as u64);
    let e = gen.locate(b[0], &mut |_| Ok(Arc::clone(&map))).err().expect("no cell bound");
    assert_eq!(e.sqlstate(), types_error::ERRCODE_DATA_CORRUPTED, "{}", e.message());
    gen.bind_run_cell(9, RunCell::image(&run, file));
    assert!(!gen.pinned_member(0), "the run has one live member");
    for &id in a {
        assert_eq!(gen.home(id), Some(IdHome::Run(0)));
        assert!(gen.locate(id, &mut |_| Ok(Arc::clone(&map))).unwrap().is_none(), "{id}: its slot was emptied");
    }
    for (o, &id) in b.iter().enumerate() {
        assert_eq!(gen.locate(id, &mut |_| Ok(Arc::clone(&map))).unwrap().map(|x| (x.0, x.1)), Some((0, o as u32)), "{id}");
    }
}

#[test]
fn sortruns_l1_a_member_must_be_a_part_and_never_under_a_creator() {
    let ranges = vec![(10 * B, 12 * B)];
    let ids: Vec<u64> = (10 * B..12 * B).collect();
    let home = build(RunRevForm::FrameIx, None, 9, 7, &ids, &ranges).unwrap();
    let ext = with_runs(&with_next_row_id(&[], 4000 * B).unwrap(), std::slice::from_ref(&home.run)).unwrap();
    // a composed view's manifest is built, not decoded: the generation checks its members itself
    let e = Generation::detached(Manifest::new(header(9, 1), vec![rec(8, 3)]).with_ext(ext.clone())).err().expect("member 7 is no part");
    assert_eq!(e.sqlstate(), types_error::ERRCODE_DATA_CORRUPTED);
    let under = pgrc_format::manifest::with_under(&ext, &[pgrc_format::manifest::UnderEntry { part_no: 7, fxid: 77 }]).unwrap();
    let e = Generation::detached(Manifest::new(header(9, 1), vec![rec(7, ids.len() as u64)]).with_ext(under)).err().expect("a member under a creator");
    assert_eq!(e.sqlstate(), types_error::ERRCODE_DATA_CORRUPTED);
}

/// D2: every page read is checked; a bad page fails the lookup closed (DATA_CORRUPTED), never a wrong answer; a short
/// file likewise.
#[test]
fn sortruns_l1_a_bad_reverse_page_fails_closed() {
    let ranges = vec![(10 * B, 40 * B)];
    let ids = shuffled(&ranges, 1, 17);
    let t = table(RunRevForm::FrameIx, None, &ids, ranges.clone());
    let pages = t.home.file.len() / pgrc_format::runrev::PAGE;
    assert!(pages >= 3, "page 0, labels and the prefix");
    for p in 0..pages {
        let mut bad = t.home.file.clone();
        bad[p * pgrc_format::runrev::PAGE + 100] ^= 0x10;
        let mut gen = Generation::detached((*t.gen.manifest).clone()).unwrap();
        gen.bind_run_cell(9, RunCell::image(&t.home.run, bad));
        let mut errors = 0;
        for &id in ids.iter().step_by(7) {
            match gen.locate(id, &mut |i| Ok(Arc::clone(&t.maps[i]))) {
                Ok(Some((1, o, _))) => assert_eq!(ids[o as usize], id, "page {p}: an answer is the right one"),
                Ok(other) => panic!("page {p}: {id} answered {:?}", other.map(|x| (x.0, x.1))),
                Err(e) => {
                    assert_eq!(e.sqlstate(), types_error::ERRCODE_DATA_CORRUPTED, "page {p}: {}", e.message());
                    errors += 1;
                }
            }
        }
        assert!(errors > 0, "page {p}: some lookup reads the bad page");
    }
    let mut gen = Generation::detached((*t.gen.manifest).clone()).unwrap();
    gen.bind_run_cell(9, RunCell::image(&t.home.run, t.home.file[..t.home.file.len() - pgrc_format::runrev::PAGE].to_vec()));
    let e = gen.locate(ids[0], &mut |i| Ok(Arc::clone(&t.maps[i]))).err().expect("a short file");
    assert_eq!(e.sqlstate(), types_error::ERRCODE_DATA_CORRUPTED);
}

/// The cell reads a page once and keeps it (the row-map cache's budget); generations that hold the run share it.
#[test]
fn sortruns_l1_reverse_pages_are_read_once_and_shared() {
    let ranges = vec![(10 * B, 20 * B)];
    let ids = shuffled(&ranges, 1, 19);
    let t = table(RunRevForm::FrameIx, None, &ids, ranges);
    let cell = Arc::clone(t.gen.runs()[0].cell().unwrap());
    for &id in &ids {
        t.locate(id).unwrap();
    }
    let reads = cell.reads();
    assert!(reads >= 2 && cell.held() as u64 == reads, "{reads} reads, {} held", cell.held());
    for &id in &ids {
        t.locate(id).unwrap();
    }
    assert_eq!(cell.reads(), reads, "a held page is not read again");
    // a later generation of the table binds the same cell (a publish or load carries it)
    let mut next = Generation::detached((*t.gen.manifest).clone()).unwrap();
    next.bind_runs(".", Some(&t.gen));
    assert!(Arc::ptr_eq(next.runs()[0].cell().unwrap(), &cell));
}

/// A run map as a member's: its forward map is a granules map, so a scan reads it as any part's.
#[test]
fn sortruns_l1_a_member_map_scans_as_any_map() {
    let ranges = vec![(10 * B, 15 * B)];
    let ids = shuffled(&ranges, 1, 23);
    let t = table(RunRevForm::Labels, None, &ids, ranges);
    let mut out = vec![0u64; ids.len()];
    t.maps[1].decode_ids(0, &mut out);
    assert_eq!(out, ids);
    // and a plain granules map of the same IDs answers its own ordinals (the member's own lookup builds a FRAMEIX)
    let plain = granules(&encode(&ids, Opts::default()).unwrap(), ids.len() as u64);
    assert_eq!(plain.ordinal_of(ids.len() as u64, ids[3]), Some(3));
    assert_eq!(t.maps[1].ordinal_of(ids.len() as u64, ids[3]), Some(3));
}

/// TEETH (main 10-08, `compact.rs:90`): a compaction input that is a sorted run's member keeps the run's IDs, never
/// fresh ones (fresh IDs would renumber its rows: every ctid and index entry of them dangling); a member of a run with
/// others is refused. A part written before row IDs still gets fresh ones.
#[test]
fn sortruns_teeth_a_run_member_input_never_gets_fresh_ids() {
    use crate::compact::{input_ids, InputIds};
    let ranges = vec![(10 * B, 12 * B)];
    let ids = shuffled(&ranges, 3, 29);
    let t = table(RunRevForm::FrameIx, None, &ids, ranges.clone());
    assert_eq!(input_ids(&t.gen, 1, false).unwrap(), InputIds::Run(ranges.clone()), "the sole member brings its run's ranges");
    assert_eq!(input_ids(&t.gen, 1, true).unwrap(), InputIds::Run(ranges.clone()), "a renumbering job too");
    assert_eq!(input_ids(&t.gen, 0, false).unwrap(), InputIds::Own(vec![(B, 3 * B)]));
    // a run of two members: neither is an L0 job's input
    let run = SortRun { members: vec![7, 8], ..t.home.run.clone() };
    let ext = with_runs(&with_next_row_id(&[], 4000 * B).unwrap(), &[run]).unwrap();
    let two = Generation::detached(Manifest::new(header(9, 2), vec![rec(7, 5), rec(8, 5)]).with_ext(ext)).unwrap();
    assert!(two.pinned_member(0) && two.pinned_member(1));
    assert!(input_ids(&two, 0, false).is_err(), "a member of a run with others");
    // a part written before row IDs (no entry, no run)
    let old = Generation::detached(Manifest::new(header(9, 1), vec![rec(5, 10)]).with_ext(with_next_row_id(&[], 4000 * B).unwrap())).unwrap();
    assert_eq!(input_ids(&old, 0, false).unwrap(), InputIds::Fresh);
    assert_eq!(input_ids(&old, 0, true).unwrap(), InputIds::StandIn);
}

/// SORTRUNS-E0 L2 (main 10-08: K1's call site unchanged): `job_input_ids` without `whole_runs`, K1's jobs' call, is
/// `input_ids` for every input of every shape (a run's sole member, a part with its own ranges, a member of a run with
/// others, a part written before row IDs; renumbering or not), so a K1 job's inputs, and its output, are as before. A
/// run job's member of a homing run brings no ranges of its own; any other input is as K1's.
#[test]
fn sortruns_l2_k1_jobs_take_their_input_ids_as_before() {
    use crate::compact::{input_ids, job_input_ids, InputIds};
    let ranges = vec![(10 * B, 12 * B)];
    let ids = shuffled(&ranges, 3, 29);
    let t = table(RunRevForm::FrameIx, None, &ids, ranges.clone());
    let run = SortRun { members: vec![7, 8], ..t.home.run.clone() };
    let ext = with_runs(&with_next_row_id(&[], 4000 * B).unwrap(), &[run]).unwrap();
    let two = Generation::detached(Manifest::new(header(9, 2), vec![rec(7, 5), rec(8, 5)]).with_ext(ext)).unwrap();
    let old = Generation::detached(Manifest::new(header(9, 1), vec![rec(5, 10)]).with_ext(with_next_row_id(&[], 4000 * B).unwrap())).unwrap();
    let mut seen = 0;
    for g in [&t.gen, &two, &old] {
        for idx in 0..g.manifest.parts.len() {
            for renumber in [false, true] {
                let k1 = job_input_ids(g, idx, renumber, false).map_err(|e| format!("{e:?}"));
                assert_eq!(k1, input_ids(g, idx, renumber).map_err(|e| format!("{e:?}")), "gen {} input {idx} renumber {renumber}", g.gen);
                let run_job = job_input_ids(g, idx, renumber, true).map_err(|e| format!("{e:?}"));
                match g.ids_of(idx).is_empty() && g.run_member(idx).is_some() {
                    true => assert_eq!(run_job, Ok(InputIds::Run(Vec::new())), "a run job's member brings no ranges"),
                    false => assert_eq!(run_job, k1, "any other input is as K1's"),
                }
                seen += 1;
            }
        }
    }
    assert_eq!(seen, 2 * (t.gen.manifest.parts.len() + 3));
}

/// SORTRUNS-E0 L2, DV carry's routing (`runmerge::route`): the rows an input's DV marked while a run job ran, by ID,
/// reach their member slot and ordinal through the job's new reverse (built by `pgrc_compact::runs::RevBuilder`, both
/// forms) and its members' maps; an ID the run does not hold refuses the publish.
#[test]
fn sortruns_l2_dv_carry_routes_ids_to_member_ordinals() {
    use pgrc_compact::runs::{Force, MemSpill, RevBuilder};
    use pgrc_format::rowmap::encode_run_member;
    let ranges = vec![(10 * B, 40 * B)];
    let ids = shuffled(&ranges, 3, 41);
    let members: Vec<Vec<u64>> = ids.chunks(2000).map(|c| c.to_vec()).collect();
    for form in [RunRevForm::Labels, RunRevForm::FrameIx] {
        let mut b = RevBuilder::new(77, ranges.clone(), 1000, MemSpill::default(), Force { form: Some(form), rank: None }).unwrap();
        let maps: Vec<Vec<u8>> = members
            .iter()
            .map(|m| {
                let fr = b.member(m).unwrap();
                encode_run_member(m, &fr.vstarts, Some(&fr.vmin[..])).unwrap()
            })
            .collect();
        let rows: Vec<u64> = members.iter().map(|m| m.len() as u64).collect();
        let mut plan = b.finish().unwrap();
        let spec = plan.spec();
        let mut file = Vec::new();
        let (fin, _) = plan
            .write(&mut |at, bytes| {
                let end = at as usize + bytes.len();
                file.resize(file.len().max(end), 0);
                file[at as usize..end].copy_from_slice(bytes);
                Ok(())
            })
            .unwrap();
        let run = SortRun { run_id: 77, level: 1, rev: spec.form, rank: spec.rank, rows: spec.rows, rev_len: fin.rev_len, rev_crc: fin.rev_crc, members: (1..=members.len() as u32).collect(), ranges: ranges.clone() };
        assert_eq!(run.rev, form);
        let cell = RunCell::image(&run, file);
        // every 37th row of each member, in ID order (as the carry collects them, input by input)
        let mut want: Vec<(usize, Vec<u32>)> = Vec::new();
        let mut marked: Vec<u64> = Vec::new();
        for (s, m) in members.iter().enumerate() {
            let ords: Vec<u32> = (0..m.len() as u32).step_by(37).collect();
            marked.extend(ords.iter().map(|&o| m[o as usize]));
            want.push((s, ords));
        }
        marked.sort_unstable();
        let mut got = crate::runmerge::route(marked, &cell, &run, &maps, &rows).unwrap();
        for (_, o) in got.iter_mut() {
            o.sort_unstable();
        }
        assert_eq!(got, want, "{form:?}");
        let held: std::collections::BTreeSet<u64> = ids.iter().copied().collect();
        let absent = (10 * B..40 * B).find(|id| !held.contains(id)).unwrap();
        assert!(crate::runmerge::route(vec![absent], &cell, &run, &maps, &rows).is_err(), "{form:?}: an ID the run does not hold");
    }
}
