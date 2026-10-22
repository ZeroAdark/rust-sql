//! Positions: what a pgrc row's `tts_tid` names. It is `tid_of` the row's ID, which never changes: a live segment's
//! row's ID comes from its heap position through the segment's extents, a part's row's from the part's `RowIdentMap`.

use std::sync::Arc;

use mcx::Mcx;
use pgrc_am::catalog::{Generation, IdHome, RowMap};
use pgrc_am::rowid_alloc::Extents;
use pgrc_format::geom::GRANULE_ROWS;
use pgrc_format::manifest::{PartRecord, SegState};
use pgrc_format::rowid::{id_of, tid_of, IDS_PER_BLOCK, SEG_EXTENT_BLOCKS, SEG_EXTENT_IDS};
use types_core::Oid;
use types_error::{PgError, PgResult};
use types_rel::Relation;
use types_tuple::{ItemPointerData, ItemPointerGetBlockNumberNoCheck, ItemPointerGetOffsetNumberNoCheck, ItemPointerSet};

pub enum Located<'g> {
    Segment { slot: u16, part_no: u32, relid: Oid, tid: ItemPointerData, id: u64 },
    Part { gen: &'g Arc<Generation>, part: usize, granule: u32, row: u32, map: Arc<RowMap>, id: u64 },
}

#[derive(Default)]
pub struct ViewRef(Option<Arc<Generation>>);

fn not_valid() -> Box<PgError> {
    Box::new(PgError::error("pgrcolumnar: position not valid in this statement").with_sqlstate(types_error::ERRCODE_INVALID_PARAMETER_VALUE))
}

fn id_of_tid(name: &str, block: u32, off: u16) -> PgResult<u64> {
    if block == types_core::InvalidBlockNumber && off == 0 {
        return Err(pgrc_am::no_row_ids(name));
    }
    id_of(block, off).ok_or_else(not_valid)
}

/// The row ID at `tid` of `rel`, without a [`locate`]: for a position the caller knows is valid (its row was
/// located in this command, `pathcache::was_located`).
pub(crate) fn id_at(rel: &Relation<'_>, tid: &ItemPointerData) -> PgResult<u64> {
    id_of_tid(&rel.name(), ItemPointerGetBlockNumberNoCheck(tid), ItemPointerGetOffsetNumberNoCheck(tid))
}

fn grain(rec: &PartRecord) -> u32 {
    if rec.granule_rows == 0 { GRANULE_ROWS } else { rec.granule_rows }
}

/// A live statement holds the horizon, so a segment it saw has not been reclaimed and its slot reused since.
pub fn locate<'g>(_mcx: Mcx<'_>, rel: &Relation<'_>, tid: &ItemPointerData, scratch: &'g mut ViewRef) -> PgResult<Located<'g>> {
    let id = id_of_tid(&rel.name(), ItemPointerGetBlockNumberNoCheck(tid), ItemPointerGetOffsetNumberNoCheck(tid))?;
    let path = crate::pathcache::path(rel)?;
    let held = |g: Option<Arc<Generation>>| g.and_then(|g| home(&g, &path.ext, id).map(|h| (g, h)));
    let (gen, at) = match held(crate::ring::statement_view(rel)?) {
        Some(x) => x,
        None => held(crate::ring::newest(rel)?).ok_or_else(not_valid)?,
    };
    let gen = scratch.0.insert(gen);
    match at {
        Home::Segment { slot, part_no, relid, tid } => Ok(Located::Segment { slot, part_no, relid, tid, id }),
        Home::Gen(h) => gen_at(gen, h, id, &|p| gen.row_map(p, &path.dir))?.ok_or_else(not_valid),
    }
}

#[derive(Clone, Copy)]
enum Home {
    Segment { slot: u16, part_no: u32, relid: Oid, tid: ItemPointerData },
    Gen(IdHome),
}

fn home(gen: &Generation, ext: &Extents, id: u64) -> Option<Home> {
    match segment_in(gen, ext, id) {
        Some((rec, tid)) => Some(Home::Segment { slot: rec.slot, part_no: rec.part_no, relid: rec.relid, tid }),
        None => gen.home(id).map(Home::Gen),
    }
}

fn gen_at<'g>(gen: &'g Arc<Generation>, h: IdHome, r: u64, map_of: &dyn Fn(usize) -> PgResult<Arc<RowMap>>) -> PgResult<Option<Located<'g>>> {
    Ok(gen.locate_at(h, r, &mut |p| map_of(p))?.map(|(part, ord, map)| {
        let rec = &gen.manifest.parts[part];
        Located::Part { gen, part, granule: ord / grain(rec), row: ord % grain(rec), map, id: r }
    }))
}

fn segment_in<'a>(gen: &'a Generation, ext: &Extents, id: u64) -> Option<(&'a pgrc_format::manifest::SegmentRecord, ItemPointerData)> {
    live_segment_of(&gen.segments, ext, id)
}

fn live_segment_of<'a>(recs: &'a [pgrc_format::manifest::SegmentRecord], ext: &Extents, id: u64) -> Option<(&'a pgrc_format::manifest::SegmentRecord, ItemPointerData)> {
    let (part_no, local) = ext.seg_of(id)?;
    let rec = recs.iter().find(|s| s.part_no == part_no && s.state != SegState::Sealed)?;
    let mut tid = ItemPointerData::default();
    ItemPointerSet(&mut tid, local / IDS_PER_BLOCK as u32, (local % IDS_PER_BLOCK as u32) as u16 + 1);
    Some((rec, tid))
}

pub(crate) fn home_in<'g>(
    gen: Option<&'g Arc<Generation>>,
    newer: &[pgrc_format::manifest::SegmentRecord],
    ext: &Extents,
    id: u64,
    map_of: &dyn Fn(usize) -> PgResult<Arc<RowMap>>,
) -> PgResult<Option<Located<'g>>> {
    let seg = gen.and_then(|g| segment_in(g, ext, id)).or_else(|| live_segment_of(newer, ext, id));
    if let Some((rec, tid)) = seg {
        return Ok(Some(Located::Segment { slot: rec.slot, part_no: rec.part_no, relid: rec.relid, tid, id }));
    }
    let Some(gen) = gen else { return Ok(None) };
    let Some(h) = gen.home(id) else { return Ok(None) };
    gen_at(gen, h, id, map_of)
}

pub fn rowident(at: &Located<'_>) -> PgResult<u64> {
    Ok(match at {
        Located::Segment { id, .. } | Located::Part { id, .. } => *id,
    })
}

pub fn find<'g>(_mcx: Mcx<'_>, rel: &Relation<'_>, r: u64, scratch: &'g mut ViewRef) -> PgResult<Option<Located<'g>>> {
    let Some(newest) = crate::ring::newest(rel)? else { return Ok(None) };
    let gen = scratch.0.insert(newest);
    let path = crate::pathcache::path(rel)?;
    find_in(gen, &path.ext, r, &|p| gen.row_map(p, &path.dir))
}

pub(crate) fn find_in<'g>(gen: &'g Arc<Generation>, ext: &Extents, r: u64, map_of: &dyn Fn(usize) -> PgResult<Arc<RowMap>>) -> PgResult<Option<Located<'g>>> {
    Finder::new(gen, ext).find(r, map_of)
}

pub(crate) struct Finder<'g, 'e> {
    gen: &'g Arc<Generation>,
    ext: &'e Extents,
    last: Option<(u64, u64, IdHome)>,
    map: Option<(usize, Arc<RowMap>)>,
}

impl<'g, 'e> Finder<'g, 'e> {
    pub(crate) fn new(gen: &'g Arc<Generation>, ext: &'e Extents) -> Finder<'g, 'e> {
        Finder { gen, ext, last: None, map: None }
    }

    pub(crate) fn find(&mut self, r: u64, map_of: &dyn Fn(usize) -> PgResult<Arc<RowMap>>) -> PgResult<Option<Located<'g>>> {
        let gen = self.gen;
        if let Some((rec, tid)) = segment_in(gen, self.ext, r) {
            return Ok(Some(Located::Segment { slot: rec.slot, part_no: rec.part_no, relid: rec.relid, tid, id: r }));
        }
        let h = match self.last {
            Some((lo, hi, h)) if (lo..hi).contains(&r) => h,
            _ => {
                let Some((lo, hi, h)) = gen.home_range(r) else { return Ok(None) };
                self.last = Some((lo, hi, h));
                h
            }
        };
        let last_map = &mut self.map;
        let mut cached = |p: usize| -> PgResult<Arc<RowMap>> {
            if let Some((q, m)) = last_map.as_ref().filter(|(q, _)| *q == p) {
                debug_assert_eq!(*q, p);
                return Ok(Arc::clone(m));
            }
            let m = map_of(p)?;
            *last_map = Some((p, Arc::clone(&m)));
            Ok(m)
        };
        Ok(gen.locate_at(h, r, &mut cached)?.map(|(part, ord, map)| {
            let rec = &gen.manifest.parts[part];
            Located::Part { gen, part, granule: ord / grain(rec), row: ord % grain(rec), map, id: r }
        }))
    }
}

pub fn stamp(_rel: &Relation<'_>, at: &Located<'_>) -> PgResult<ItemPointerData> {
    let (block, off) = tid_of(rowident(at)?);
    let mut tid = ItemPointerData::default();
    ItemPointerSet(&mut tid, block, off);
    Ok(tid)
}

const _: () = assert!(SEG_EXTENT_IDS == SEG_EXTENT_BLOCKS as u64 * IDS_PER_BLOCK, "a segment row's heap TID is its local position's");

#[cfg(test)]
mod tests {
    use super::*;
    use pgrc_format::manifest::{encode_segments, with_next_row_id, with_ranges, IdRanges, Manifest, ManifestHeader, SegmentRecord};
    use pgrc_format::rowid::{seg_id, seg_local, SegExtent};

    #[test]
    fn an_invalid_tid_is_a_row_written_before_row_ids() {
        let e = id_of_tid("readings", u32::MAX, 0).unwrap_err();
        assert_eq!(e.sqlstate(), types_error::ERRCODE_FEATURE_NOT_SUPPORTED);
        assert!(e.message().contains("\"readings\"") && e.message().contains("written before row IDs"), "{}", e.message());
        assert!(e.hint().is_some_and(|h| h.contains("VACUUM FULL")));
        assert_eq!(id_of_tid("readings", 1, 1).unwrap(), 291);
        assert_eq!(id_of_tid("readings", 0, 0).unwrap_err().sqlstate(), types_error::ERRCODE_INVALID_PARAMETER_VALUE, "offset 0: no scan stamped it");
        assert_eq!(id_of_tid("readings", 7, 292).unwrap_err().sqlstate(), types_error::ERRCODE_INVALID_PARAMETER_VALUE);
    }

    fn part(part_no: u32, rows: u64) -> PartRecord {
        let granule_count = rows.div_ceil(u64::from(GRANULE_ROWS)) as u32;
        PartRecord { rows, file_len: 0, footer_off: 0, dv_gen: 0, dv_len: 0, part_no, flags: 0, granule_count, band_count: 1, dv_crc: 0, granule_rows: 0 }
    }

    fn seg(slot: u16, part_no: u32, state: SegState) -> SegmentRecord {
        SegmentRecord { slot, part_no, relid: 16000 + u32::from(slot), state, fxid: 1 }
    }

    fn gen(g: u64, parts: Vec<PartRecord>, ranges: &[(u32, IdRanges)], segs: &[SegmentRecord]) -> Arc<Generation> {
        let header = ManifestHeader { gen: g, prev_gen: 0, publisher_fxid: 1, relfilenumber: 9, schema_fingerprint: 1, magic: 0, format_version: 0, spc: 1663, db: 5, part_count: parts.len() as u32, next_part_no: 100, flags: 0, reserved: 0 };
        let mut ext = with_next_row_id(&with_ranges(&[], ranges).unwrap(), 1 << 36).unwrap();
        if !segs.is_empty() {
            encode_segments(segs, &mut ext);
        }
        Arc::new(Generation::detached(Manifest::new(header, parts).with_ext(ext)).expect("generation"))
    }

    fn tid(block: u32, off: u16) -> ItemPointerData {
        let mut t = ItemPointerData::default();
        ItemPointerSet(&mut t, block, off);
        t
    }

    fn round_trip(g: &Arc<Generation>, ext: &Extents, maps: &[Arc<RowMap>]) -> Vec<Vec<u64>> {
        let map_of = |p: usize| Ok(Arc::clone(&maps[p]));
        (0..maps.len())
            .map(|p| {
                let rec = g.manifest.parts[p];
                (0..rec.rows as u32)
                    .map(|o| {
                        let id = maps[p].ident(o);
                        let (b, off) = tid_of(id);
                        assert_eq!(id_of(b, off), Some(id));
                        let at = find_in(g, ext, id, &map_of).unwrap().expect("found");
                        let Located::Part { part, granule, row, .. } = &at else { panic!("a part row") };
                        assert_eq!((*part, granule * grain(&rec) + row), (p, o));
                        assert_eq!(rowident(&at).unwrap(), id);
                        id
                    })
                    .collect()
            })
            .collect()
    }

    const E: u64 = SEG_EXTENT_IDS;

    fn granules(ids: &[u64]) -> Arc<RowMap> {
        let body = pgrc_format::rowmap::encode(ids, pgrc_format::rowmap::Opts::default()).expect("the map");
        Arc::new(RowMap::Granules(Arc::new(pgrc_am::catalog::GranuleMap::new(pgrc_format::rowmap::GranMap::decode(&body, ids.len() as u64).expect("the map decodes")))))
    }

    #[test]
    fn implicit_and_sealed_parts_round_trip() {
        let rows0 = u64::from(GRANULE_ROWS) * 2 + 10;
        let (b0, b1) = (291 * 4000, 291 * 4000 + 3 * E);
        let g = gen(300, vec![part(0, rows0), part(7, 5)], &[(0, vec![(291, 291 + 291 * 2000)]), (7, vec![(b0, b0 + E), (b1, b1 + E)])], &[]);
        let local = [seg_local(0, 3).unwrap(), seg_local(0, 9).unwrap(), seg_local(2, 1).unwrap(), seg_local(1023, 291).unwrap(), seg_local(1024, 1).unwrap()];
        let maps = vec![Arc::new(RowMap::Implicit(291)), granules(&local.map(|l| seg_id(&[b0, b1], l).unwrap()))];
        let ext = Extents::default();
        let ids = round_trip(&g, &ext, &maps);
        assert_eq!(ids[0][0], 291);
        assert_eq!(ids[0][GRANULE_ROWS as usize + 5], 291 + u64::from(GRANULE_ROWS) + 5);
        assert_eq!(ids[1], vec![b0 + 2, b0 + 8, b0 + 582, b0 + E - 1, b1], "the fifth row is the second extent's first");
        let map_of = |p: usize| Ok(Arc::clone(&maps[p]));
        assert!(find_in(&g, &ext, 291 + rows0, &map_of).unwrap().is_none(), "past the part's rows, inside its range");
        assert!(find_in(&g, &ext, b0 + 3, &map_of).unwrap().is_none(), "a position the seal's map lacks");
        assert!(find_in(&g, &ext, b0 + E, &map_of).unwrap().is_none(), "between the extents");
        assert!(find_in(&g, &ext, 1 << 35, &map_of).unwrap().is_none(), "an ID no part homes");
    }

    #[test]
    fn a_finder_answers_as_find_in() {
        let rows0 = u64::from(GRANULE_ROWS) + 20;
        let b7 = 291 * 9000;
        let g = gen(9, vec![part(0, rows0), part(7, 5)], &[(0, vec![(291, 291 * 600)]), (7, vec![(b7, b7 + E)])], &[seg(2, 11, SegState::Active)]);
        let s11 = 291 * 20_000;
        let ext = Extents::new(vec![SegExtent { part_no: 11, idx: 0, base: s11 }]);
        let maps = vec![Arc::new(RowMap::Implicit(291)), granules(&[3, 9, 700, 701, 90_000].map(|l| b7 + l))];
        let calls = std::cell::Cell::new(0);
        let map_of = |p: usize| {
            calls.set(calls.get() + 1);
            Ok(Arc::clone(&maps[p]))
        };
        let seg_r = s11 + u64::from(seg_local(1, 2).unwrap());
        let rs = [296, 291 + u64::from(GRANULE_ROWS) + 19, 291 + rows0, b7 + 700, b7 + 4, seg_r, b7 + 90_000, 1 << 35, 292];
        fn key(at: Option<Located<'_>>) -> Option<(u8, u32, u32, u32)> {
            match at {
                None => None,
                Some(Located::Part { part, granule, row, .. }) => Some((0, part as u32, granule, row)),
                Some(Located::Segment { slot, tid, .. }) => Some((1, u32::from(slot), ItemPointerGetBlockNumberNoCheck(&tid), u32::from(ItemPointerGetOffsetNumberNoCheck(&tid)))),
            }
        }
        let mut f = Finder::new(&g, &ext);
        calls.set(0);
        let batched: Vec<_> = rs.iter().map(|&r| key(f.find(r, &map_of).unwrap())).collect();
        assert_eq!(calls.get(), 3, "part 0, part 7, part 0 again: one map fetch per run of a range");
        let single: Vec<_> = rs.iter().map(|&r| key(find_in(&g, &ext, r, &map_of).unwrap())).collect();
        assert_eq!(batched, single);
        assert_eq!(batched[0], Some((0, 0, 0, 5)));
        assert_eq!(batched[3], Some((0, 1, 0, 2)));
        assert_eq!(batched[5], Some((1, 2, 1, 2)), "the live segment's heap TID");
        assert_eq!((batched[2], batched[4], batched[7]), (None, None, None));
    }

    #[test]
    fn segments_round_trip_and_a_seal_between_keeps_the_id() {
        let (b0, b1) = (291 * 50, 291 * 50 + 2 * E);
        let ext = Extents::new(vec![SegExtent { part_no: 9, idx: 0, base: b0 }, SegExtent { part_no: 9, idx: 1, base: b1 }]);
        let before = gen(4, vec![part(0, 10)], &[(0, vec![(291 * 10, 291 * 11)])], &[seg(3, 9, SegState::Active)]);
        let map_of = |_: usize| Ok(Arc::new(RowMap::Implicit(291 * 10)));
        let heap = [(2u32, 7u16), (1030, 5)];
        let ids: Vec<u64> = heap.iter().map(|&(b, o)| seg_id(&[b0, b1], seg_local(b, o).unwrap()).unwrap()).collect();
        assert_eq!(ids[1], b1 + 6 * 291 + 4);
        for (&(b, o), &id) in heap.iter().zip(&ids) {
            let Some(Located::Segment { slot: 3, part_no: 9, tid: t, id: got, .. }) = find_in(&before, &ext, id, &map_of).unwrap() else { panic!("the segment row") };
            assert_eq!((t, got), (tid(b, o), id));
        }
        let pos: Vec<u32> = [(0u32, 1u16), (2, 7), (5, 3), (1030, 5)].iter().map(|&(b, o)| seg_local(b, o).unwrap()).collect();
        let after = gen(5, vec![part(0, 10), part(9, 4)], &[(0, vec![(291 * 10, 291 * 11)]), (9, vec![(b0, b0 + E), (b1, b1 + E)])], &[seg(3, 9, SegState::Sealed)]);
        let maps = [Arc::new(RowMap::Implicit(291 * 10)), granules(&pos.iter().map(|&l| seg_id(&[b0, b1], l).unwrap()).collect::<Vec<_>>())];
        let map_of = |p: usize| Ok(Arc::clone(&maps[p]));
        let found = find_in(&after, &ext, ids[0], &map_of).unwrap().expect("the sealed row");
        let Located::Part { part: 1, granule: 0, row: 1, .. } = found else { panic!("ordinal 1 of the sealed part") };
        assert_eq!(rowident(&found).unwrap(), ids[0]);
        let Located::Part { part: 1, row: 3, .. } = find_in(&after, &ext, ids[1], &map_of).unwrap().unwrap() else { panic!("ordinal 3") };
        assert!(find_in(&after, &Extents::default(), ids[0], &map_of).unwrap().is_some(), "a pruned extent table: the sealed part still homes it");
    }

    #[test]
    fn compaction_keeps_every_id() {
        let (bulk, s9) = ((291u64, 582u64), 291 * 100);
        let before = gen(10, vec![part(0, 3), part(9, 2)], &[(0, vec![bulk]), (9, vec![(s9, s9 + E)])], &[seg(3, 9, SegState::Sealed)]);
        let maps = vec![Arc::new(RowMap::Implicit(291)), granules(&[s9 + 4, s9 + 8])];
        let ext = Extents::default();
        let ids: Vec<u64> = round_trip(&before, &ext, &maps).concat();
        assert_eq!(ids, vec![291, 292, 293, s9 + 4, s9 + 8]);
        let homes = vec![bulk, (s9, s9 + E)];
        let after = gen(11, vec![part(20, 4)], &[(20, homes.clone())], &[seg(3, 9, SegState::Sealed)]);
        let rmaps = vec![granules(&[ids[0], ids[2], ids[3], ids[4]])];
        assert_eq!(round_trip(&after, &ext, &rmaps).concat(), vec![ids[0], ids[2], ids[3], ids[4]]);
        let map_of = |p: usize| Ok(Arc::clone(&rmaps[p]));
        assert!(find_in(&after, &ext, ids[1], &map_of).unwrap().is_none(), "the dropped row");
        let after = gen(12, vec![part(21, 4)], &[(21, homes)], &[]);
        let pmaps = vec![granules(&[ids[4], ids[2], ids[3], ids[0]])];
        assert_eq!(round_trip(&after, &ext, &pmaps).concat(), vec![ids[4], ids[2], ids[3], ids[0]]);
    }

    #[test]
    fn sortruns_l1_run_homed_rows_round_trip() {
        use pgrc_format::sortrun::{with_runs, RunRevForm};
        for form in [RunRevForm::FrameIx, RunRevForm::Labels] {
            let ranges = vec![(291 * 100, 291 * 140)];
            let mut ids: Vec<u64> = (291 * 100..291 * 140).filter(|id| id % 5 != 0).collect();
            let half = ids.len() / 2;
            let (a, b) = ids.split_at(half);
            ids = a.iter().zip(b).flat_map(|(x, y)| [*y, *x]).chain(b[a.len()..].iter().copied()).collect();
            let home = pgrc_write::testruns::build(form, None, 12, 21, &ids, &ranges).unwrap();
            let header = ManifestHeader { gen: 12, prev_gen: 0, publisher_fxid: 1, relfilenumber: 9, schema_fingerprint: 1, magic: 0, format_version: 0, spc: 1663, db: 5, part_count: 2, next_part_no: 100, flags: 0, reserved: 0 };
            let ext = with_runs(&with_next_row_id(&with_ranges(&[], &[(4, vec![(291, 291 * 3)])]).unwrap(), 1 << 36).unwrap(), std::slice::from_ref(&home.run)).unwrap();
            let mut g = Generation::detached(Manifest::new(header, vec![part(4, 291 * 2), part(21, ids.len() as u64)]).with_ext(ext)).expect("generation");
            g.bind_run_cell(12, pgrc_am::runcache::RunCell::image(&home.run, home.file.clone()));
            let g = Arc::new(g);
            let member = Arc::new(RowMap::Granules(Arc::new(pgrc_am::catalog::GranuleMap::new(pgrc_format::rowmap::GranMap::decode(&home.member_map, ids.len() as u64).unwrap()))));
            let maps = vec![Arc::new(RowMap::Implicit(291)), member];
            let ext = Extents::default();
            let back = round_trip(&g, &ext, &maps);
            assert_eq!(back[1], ids, "{form:?}");
            let map_of = |p: usize| Ok(Arc::clone(&maps[p]));
            for &id in ids.iter().step_by(11) {
                let Some(Located::Part { part, granule, row, .. }) = home_in(Some(&g), &[], &ext, id, &map_of).unwrap() else { panic!("{id} located") };
                assert_eq!((part, maps[1].ident(granule * GRANULE_ROWS + row)), (1, id));
            }
            assert!(find_in(&g, &ext, 291 * 100, &map_of).unwrap().is_none(), "{form:?}: a dropped row of the run's ranges");
            assert!(home_in(Some(&g), &[], &ext, 291 * 105, &map_of).unwrap().is_none());
        }
    }
}
