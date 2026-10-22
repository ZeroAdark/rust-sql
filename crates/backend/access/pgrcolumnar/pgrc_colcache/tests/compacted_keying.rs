use std::collections::HashMap;
use std::sync::Arc;

use pgrc_am::catalog::{Generation, GranuleMap, RowMap};
use pgrc_colcache::tomb::{key, GenKeyer, Keyed, Keyer};
use pgrc_am::rowid_alloc::Extents;
use pgrc_compact::ident::{build_map, union_ranges, OutMap};
use pgrc_compact::order::{concat_order, Input};
use pgrc_compact::JobKind;
use pgrc_format::manifest::{encode_segments, with_next_row_id, with_ranges, IdRanges, Manifest, ManifestHeader, PartRecord, SegState, SegmentRecord};
use pgrc_format::rowid::{align_up, SegExtent, SEG_EXTENT_IDS};

const GRAIN: u32 = 16;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

struct P {
    part_no: u32,
    map: Arc<RowMap>,
    ids: Vec<u64>,
    dead: Vec<bool>,
    ranges: IdRanges,
}

fn granules(body: &[u8], rows: u64) -> RowMap {
    RowMap::Granules(Arc::new(GranuleMap::new(pgrc_format::rowmap::GranMap::decode(body, rows).expect("granules decode"))))
}

fn loaded(out: &OutMap, rows: u64) -> RowMap {
    let OutMap::Granules(b) = out;
    granules(b, rows)
}

fn compact(parts: &mut Vec<P>, s: usize, n: usize, kind: JobKind, next_no: &mut u32, rng: &mut Rng) -> JobKind {
    let views: Vec<_> = parts[s..s + n].iter().map(|p| p.map.view()).collect();
    let deads: Vec<_> = parts[s..s + n].iter().map(|p| move |o: u32| p.dead[o as usize]).collect();
    let inputs: Vec<Input<'_>> = parts[s..s + n].iter().zip(&views).zip(&deads).map(|((p, v), d)| Input { part_no: p.part_no, rows: p.ids.len() as u32, deleted: d, map: v }).collect();
    let mut order = concat_order(&inputs).unwrap();
    if kind == JobKind::SortedMerge {
        for i in (1..order.len()).rev() {
            order.swap(i, rng.below(i as u64 + 1) as usize);
        }
    }
    let ids: Vec<u64> = order.iter().map(|&(k, o)| parts[s + k as usize].ids[o as usize]).collect();
    let ranges = union_ranges(parts[s..s + n].iter().map(|p| &p.ranges[..]));
    let out = (!order.is_empty()).then(|| build_map(&kind, &inputs, &order).expect("the output map encodes"));
    drop(inputs);
    drop(deads);
    drop(views);
    parts.drain(s..s + n);
    if let Some(out) = out {
        let no = *next_no;
        *next_no += 1;
        let map = Arc::new(loaded(&out, ids.len() as u64));
        parts.push(P { part_no: no, dead: vec![false; ids.len()], ids, map, ranges });
    }
    kind
}

fn generation(parts: &[P], segs: &[SegmentRecord], next_no: u32, floor: u64) -> Arc<Generation> {
    let recs: Vec<PartRecord> = parts
        .iter()
        .map(|p| {
            let rows = p.ids.len() as u64;
            PartRecord { rows, file_len: 0, footer_off: 0, dv_gen: 0, dv_len: 0, part_no: p.part_no, flags: 0, granule_count: rows.div_ceil(u64::from(GRAIN)) as u32, band_count: 1, dv_crc: 0, granule_rows: GRAIN }
        })
        .collect();
    let header = ManifestHeader { gen: 7, prev_gen: 6, publisher_fxid: 1, relfilenumber: 9, schema_fingerprint: 1, magic: 0, format_version: 0, spc: 1663, db: 5, part_count: recs.len() as u32, next_part_no: next_no, flags: 0, reserved: 0 };
    let mut ext = Vec::new();
    encode_segments(segs, &mut ext);
    let table: Vec<(u32, IdRanges)> = parts.iter().map(|p| (p.part_no, p.ranges.clone())).collect();
    let ext = with_next_row_id(&with_ranges(&ext, &table).expect("EXT_RANGES"), floor).expect("EXT_ROWID");
    Arc::new(Generation::detached(Manifest::new(header, recs).with_ext(ext)).expect("generation"))
}

fn history(seed: u64) -> ([usize; 3], usize) {
    let mut rng = Rng(seed);
    let mut parts = Vec::new();
    let mut born = Vec::new();
    let mut next_id = 291u64;
    let mut reserve = |n: u64, rng: &mut Rng| {
        next_id += align_up(1 + rng.below(900));
        let lo = next_id;
        next_id += align_up(n);
        lo
    };
    let (s0, s401, s403) = (reserve(SEG_EXTENT_IDS, &mut rng), reserve(SEG_EXTENT_IDS, &mut rng), reserve(SEG_EXTENT_IDS, &mut rng));
    for i in 0..(3 + rng.below(8)) as u32 {
        let (no, rows) = (2 * i, 1 + rng.below(70) as u32);
        let sealed = i == 0 || rng.below(2) == 1;
        let (map, ids, ranges): (RowMap, Vec<u64>, IdRanges) = if !sealed {
            let b = reserve(u64::from(rows), &mut rng);
            (RowMap::Implicit(b), (0..u64::from(rows)).map(|x| b + x).collect(), vec![(b, b + align_up(u64::from(rows)))])
        } else {
            let b = if i == 0 { s0 } else { reserve(SEG_EXTENT_IDS, &mut rng) };
            let mut p = Vec::new();
            let mut at = rng.below(5) as u32;
            for _ in 0..rows {
                p.push(at);
                at += 1 + rng.below(4) as u32;
            }
            let ids: Vec<u64> = p.iter().map(|&x| b + u64::from(x)).collect();
            let body = pgrc_format::rowmap::encode(&ids, pgrc_format::rowmap::Opts::default()).expect("the map");
            (granules(&body, u64::from(rows)), ids, vec![(b, b + SEG_EXTENT_IDS)])
        };
        born.extend(&ids);
        parts.push(P { part_no: no, map: Arc::new(map), dead: vec![false; ids.len()], ids, ranges });
    }
    let floor = next_id + 291;
    let mut next_no = 500;
    let mut jobs = [0usize; 3];
    for _ in 0..(1 + rng.below(12)) {
        if parts.is_empty() {
            break;
        }
        match rng.below(3) {
            0 => {
                let p = rng.below(parts.len() as u64) as usize;
                for _ in 0..=rng.below(parts[p].ids.len() as u64) {
                    let o = rng.below(parts[p].ids.len() as u64) as usize;
                    parts[p].dead[o] = true;
                }
            }
            _ => {
                let s = rng.below(parts.len() as u64) as usize;
                let n = 1 + rng.below((parts.len() - s).min(5) as u64) as usize;
                let kind = match (n, rng.below(2)) {
                    (1, _) => JobKind::DvRewrite,
                    (_, 0) => JobKind::Concat,
                    _ => JobKind::SortedMerge,
                };
                let k = compact(&mut parts, s, n, kind, &mut next_no, &mut rng);
                jobs[k as usize] += 1;
            }
        }
    }
    let segs = vec![
        SegmentRecord { slot: 1, part_no: 0, relid: 16001, state: SegState::Sealed, fxid: 1 },
        SegmentRecord { slot: 2, part_no: 401, relid: 16002, state: SegState::Active, fxid: 1 },
        SegmentRecord { slot: 3, part_no: 403, relid: 16003, state: SegState::Sealing, fxid: 1 },
    ];
    let ext = Arc::new(Extents::new(vec![
        SegExtent { part_no: 0, idx: 0, base: s0 },
        SegExtent { part_no: 401, idx: 0, base: s401 },
        SegExtent { part_no: 403, idx: 0, base: s403 },
    ]));
    let g = generation(&parts, &segs, next_no, floor);
    let mut k = GenKeyer::with_maps(&g, Arc::clone(&ext), parts.iter().map(|p| Arc::clone(&p.map)).collect());
    assert_eq!(k.nparts(), parts.len());
    let mut checked = 0;
    let mut at: HashMap<u64, Keyed> = HashMap::new();
    for (i, p) in parts.iter().enumerate() {
        for (o, &id) in p.ids.iter().enumerate() {
            assert_eq!(p.map.ident(o as u32), id, "seed {seed}: part {} ordinal {o}: the map's ID", p.part_no);
            at.insert(id, Keyed::Part { part: i, granule: o as u32 / GRAIN, row: o as u32 % GRAIN });
        }
    }
    let home = |id: u64| parts.iter().position(|p| p.ranges.iter().any(|&(lo, hi)| (lo..hi).contains(&id)));
    for &id in &born {
        let want = match at.get(&id) {
            Some(&w) => w,
            None if home(id).is_some() => Keyed::Moot,
            None if (s0..s0 + SEG_EXTENT_IDS).contains(&id) => Keyed::Segment { part_no: 0, pos: (id - s0) as u32 },
            None => Keyed::Moot,
        };
        assert_eq!(key(&mut k, id).unwrap(), want, "seed {seed}: row {id}");
        assert_eq!(k.homes(id), home(id).is_some(), "seed {seed}: the home of {id}");
        checked += 1;
    }
    // An ID no row ever held, inside a part's range: moot. Live segments key by part_no and local position; an ID
    // nothing holds is moot.
    for p in &parts {
        let &(lo, hi) = p.ranges.last().unwrap();
        if let Some(free) = (lo..hi).rev().find(|x| !at.contains_key(x)) {
            assert_eq!(key(&mut k, free).unwrap(), Keyed::Moot, "seed {seed}: part {}, no such row", p.part_no);
        }
    }
    for (no, b) in [(401, s401), (403, s403)] {
        assert_eq!(key(&mut k, b + 7).unwrap(), Keyed::Segment { part_no: no, pos: 7 });
    }
    assert_eq!(key(&mut k, floor + 5000).unwrap(), Keyed::Moot, "an ID past every reservation");
    assert_eq!(key(&mut k, 5).unwrap(), Keyed::Moot);
    (jobs, checked)
}

#[test]
fn keying_over_compacted_generations() {
    let (mut jobs, mut rows) = ([0usize; 3], 0);
    for seed in 0..2000 {
        let (j, r) = history(seed);
        (0..3).for_each(|i| jobs[i] += j[i]);
        rows += r;
    }
    assert!(jobs.iter().all(|&n| n > 500), "jobs by kind (concat, sorted merge, DV rewrite): {jobs:?}");
    assert!(rows > 100_000, "rows checked: {rows}");
}

#[test]
fn sortruns_l1_keying_of_run_homed_rows() {
    use pgrc_format::sortrun::{with_runs, RunRevForm};
    let ranges = vec![(291 * 50, 291 * 60)];
    let ids: Vec<u64> = (291 * 50..291 * 60).rev().filter(|id| id % 7 != 0).collect();
    for form in [RunRevForm::FrameIx, RunRevForm::Labels] {
        let home = pgrc_write::testruns::build(form, None, 8, 5, &ids, &ranges).unwrap();
        let rec = |part_no: u32, rows: u64| PartRecord { rows, file_len: 0, footer_off: 0, dv_gen: 0, dv_len: 0, part_no, flags: 0, granule_count: rows.div_ceil(u64::from(GRAIN)) as u32, band_count: 1, dv_crc: 0, granule_rows: GRAIN };
        let header = ManifestHeader { gen: 8, prev_gen: 0, publisher_fxid: 1, relfilenumber: 9, schema_fingerprint: 1, magic: 0, format_version: 0, spc: 1663, db: 5, part_count: 2, next_part_no: 9, flags: 0, reserved: 0 };
        let ext = with_runs(&with_next_row_id(&with_ranges(&[], &[(2, vec![(291, 582)])]).unwrap(), 291 * 100).unwrap(), std::slice::from_ref(&home.run)).unwrap();
        let mut g = Generation::detached(Manifest::new(header, vec![rec(2, 291), rec(5, ids.len() as u64)]).with_ext(ext)).unwrap();
        g.bind_run_cell(8, pgrc_am::runcache::RunCell::image(&home.run, home.file.clone()));
        let g = Arc::new(g);
        let maps = vec![Arc::new(RowMap::Implicit(291)), Arc::new(granules(&home.member_map, ids.len() as u64))];
        let mut k = GenKeyer::with_maps(&g, Arc::new(Extents::default()), maps);
        for (o, &id) in ids.iter().enumerate() {
            assert!(k.homes(id));
            assert_eq!(key(&mut k, id).unwrap(), Keyed::Part { part: 1, granule: o as u32 / GRAIN, row: o as u32 % GRAIN }, "{form:?} {id}");
        }
        let dropped = (291 * 50..).find(|id| id % 7 == 0).unwrap();
        assert_eq!(key(&mut k, dropped).unwrap(), Keyed::Moot, "a dropped row of the run's ranges");
        assert_eq!(key(&mut k, 294).unwrap(), Keyed::Part { part: 0, granule: 3 / GRAIN, row: 3 % GRAIN });
        assert!(!k.homes(291 * 60));
    }
}
