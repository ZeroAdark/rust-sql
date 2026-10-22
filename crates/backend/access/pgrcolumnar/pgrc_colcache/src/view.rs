//! Per-statement views, built at O(recent rows + recent tombstones + dirty blocks). Settled rows cost nothing.

use std::collections::HashMap;
use std::sync::Arc;

use mcx::Mcx;
use pgrc_am::catalog::Generation;
use pgrc_am::scan::DeletionFilter;
use pgrc_format::manifest::SegmentRecord;
use pgrc_format::rowid::seg_local;
use pgrc_front::filter::TombFilter;
use types_core::Oid;
use types_error::PgResult;
use types_rel::{AccessShareLock, NoLock, Relation};
use types_snapshot::{SnapshotData, SnapshotType};

use crate::image::{is_dead, CompanionImage, Kind};
use crate::pagesrc::{BufPages, Procs};
use crate::tomb::{is_recent, key, settled, tombstone, GenKeyer, Keyed, Keyer, Settled};
use crate::vis::{seg_visible, tomb_visible, Mvcc, SnapView};

pub struct ColView {
    pub gen: Option<Arc<Generation>>,
    /// `(Generation::view_key, Σ image seqs)`: a composed view keys apart.
    pub identity: (u64, u64),
    pub segs: Vec<SegView>,
    pub filter: ViewFilter,
}

pub struct SegView {
    pub slot: u16,
    pub part_no: u32,
    pub relid: Oid,
    pub src: SegSource,
}

pub enum SegSource {
    Image {
        img: Arc<CompanionImage>,
        recent_ok: Box<[u64]>,
        /// Settled tombstones' positions in this segment, sorted.
        deleted: Arc<[u32]>,
        /// Recent tombstones' positions the snapshot sees, sorted.
        own_deleted: Vec<u32>,
    },
    Heap,
}

impl SegView {
    pub fn rows(&self) -> Option<Vec<(u32, u16)>> {
        let SegSource::Image { img, recent_ok, deleted, own_deleted } = &self.src else { return None };
        let mut out = Vec::new();
        let mut r = 0usize;
        for c in img.chunks.iter().flatten() {
            for i in 0..c.rows() {
                let vis = if c.all_settled || c.is_settled(i) {
                    true
                } else {
                    while r < img.recent.len() && img.recent[r] < (c.block, i as u16) {
                        r += 1;
                    }
                    r < img.recent.len() && img.recent[r] == (c.block, i as u16) && recent_ok[r / 64] & (1 << (r % 64)) != 0
                };
                if !vis {
                    continue;
                }
                if let Some(pos) = seg_local(c.block, c.hdr[i].off) {
                    if deleted.binary_search(&pos).is_ok() || own_deleted.binary_search(&pos).is_ok() {
                        continue;
                    }
                }
                out.push((c.block, i as u16));
            }
        }
        Some(out)
    }
}

pub struct ViewFilter {
    settled: Vec<Arc<TombFilter>>,
    recent: TombFilter,
    /// Per ring slot: settled ∪ recent deleted positions, sorted.
    segs: HashMap<u16, Vec<u32>>,
}

impl ViewFilter {
    pub fn empty() -> ViewFilter {
        ViewFilter { settled: Vec::new(), recent: TombFilter::default(), segs: HashMap::new() }
    }
}

impl DeletionFilter for ViewFilter {
    fn granule_into(&self, part: usize, g: u32, mask: &mut Option<Box<[u64; pgrc_scan::DV_GRANULE_WORDS]>>) {
        for s in &self.settled {
            s.granule_into(part, g, mask);
        }
        self.recent.granule_into(part, g, mask);
    }

    fn segment_positions(&self, slot: u16) -> &[u32] {
        self.segs.get(&slot).map_or(&[], |v| v)
    }
}

pub struct SegIn {
    pub rec: SegmentRecord,
    pub img: Option<Arc<CompanionImage>>,
}

pub struct TombIn<'a> {
    pub img: &'a CompanionImage,
    pub settled: Arc<Settled>,
}

/// A row is deleted if any heap holds a tombstone for it that the snapshot sees.
pub fn build(k: &mut dyn Keyer, tombs: &[TombIn<'_>], segs: Vec<SegIn>, snap: &dyn SnapView) -> PgResult<(u64, Vec<SegView>, ViewFilter)> {
    let live: HashMap<u32, u16> = segs.iter().map(|s| (s.rec.part_no, s.rec.slot)).collect();
    let nparts = k.nparts();
    let mut recent = TombFilter::with_parts(nparts);
    let mut recent_segs: HashMap<u16, Vec<u32>> = HashMap::new();
    let mut seq_sum = 0u64;
    for t in tombs.iter().map(|t| t.img) {
        seq_sum = seq_sum.wrapping_add(t.seq);
        for &(blk, i) in &t.recent {
            let c = t.chunk(blk).expect("a recent row's chunk");
            let i = i as usize;
            if !is_recent(c, i) || !tomb_visible(&c.hdr[i], snap)? {
                continue;
            }
            let rowident = tombstone(c, i)?.rowident;
            match key(k, rowident)? {
                Keyed::Part { part, granule, row } => recent.add_part_row(part, granule, row),
                Keyed::Segment { part_no, pos } => {
                    if let Some(&slot) = live.get(&part_no) {
                        recent_segs.entry(slot).or_default().push(pos);
                    }
                }
                Keyed::Moot => {}
            }
        }
    }
    for v in recent_segs.values_mut() {
        v.sort_unstable();
        v.dedup();
    }
    let empty: Arc<[u32]> = Arc::from(Vec::new());
    let mut filter_segs: HashMap<u16, Vec<u32>> = HashMap::new();
    let mut out = ::mcx::stdvec::try_vec(segs.len(), "pgrc column view")?;
    for s in segs {
        let deleted = settled_positions(tombs, s.rec.part_no).unwrap_or_else(|| Arc::clone(&empty));
        let own_deleted = recent_segs.remove(&s.rec.slot).unwrap_or_default();
        let mut merged: Vec<u32> = deleted.iter().copied().chain(own_deleted.iter().copied()).collect();
        merged.sort_unstable();
        merged.dedup();
        if !merged.is_empty() {
            filter_segs.insert(s.rec.slot, merged);
        }
        let src = match s.img {
            Some(img) => {
                seq_sum = seq_sum.wrapping_add(img.seq);
                let mut recent_ok = ::mcx::stdvec::try_filled(img.recent.len().div_ceil(64), 0u64, "pgrc column view")?;
                for (r, &(blk, i)) in img.recent.iter().enumerate() {
                    let c = img.chunk(blk).expect("a recent row's chunk");
                    let h = &c.hdr[i as usize];
                    if !is_dead(h) && seg_visible(h, snap)? {
                        recent_ok[r / 64] |= 1 << (r % 64);
                    }
                }
                SegSource::Image { img, recent_ok: recent_ok.into_boxed_slice(), deleted, own_deleted }
            }
            None => SegSource::Heap,
        };
        out.push(SegView { slot: s.rec.slot, part_no: s.rec.part_no, relid: s.rec.relid, src });
    }
    let filter = ViewFilter { settled: tombs.iter().map(|t| Arc::clone(&t.settled.parts)).collect(), recent: recent.finished(), segs: filter_segs };
    Ok((seq_sum, out, filter))
}

fn settled_positions(tombs: &[TombIn<'_>], part_no: u32) -> Option<Arc<[u32]>> {
    let mut lists = tombs.iter().filter_map(|t| t.settled.segs.get(&part_no)).peekable();
    let first = lists.next()?;
    if lists.peek().is_none() {
        return Some(Arc::clone(first));
    }
    let mut all: Vec<u32> = first.iter().chain(lists.flat_map(|l| l.iter())).copied().collect();
    all.sort_unstable();
    all.dedup();
    Some(Arc::from(all))
}

/// `None` = the provider is off for this statement: the caller runs the F2 path.
pub fn view<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, gen: Option<&Arc<Generation>>, segs: &[SegmentRecord], snap: &SnapshotData<'mcx>) -> PgResult<Option<ColView>> {
    build_view(mcx, rel, gen, segs, snap, true)
}

/// The segments are keyed but not imaged: a fetch reads a segment row by heap visibility.
pub fn deletes<'a, 'mcx>(mcx: Mcx<'mcx>, rel: &'a Relation<'mcx>, gen: Option<&'a Arc<Generation>>, segs: &'a [SegmentRecord], snap: &'a SnapshotData<'mcx>) -> PgResult<Option<Box<dyn DeletionFilter>>> {
    Ok(build_view(mcx, rel, gen, segs, snap, false)?.map(|v| Box::new(v.filter) as Box<dyn DeletionFilter>))
}

fn build_view<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, gen: Option<&Arc<Generation>>, segs: &[SegmentRecord], snap: &SnapshotData<'mcx>, images: bool) -> PgResult<Option<ColView>> {
    if snap.snapshot_type != SnapshotType::SNAPSHOT_MVCC || rel.uses_local_buffers() || pgrc_read::mem::cap() == 0 {
        return Ok(None);
    }
    let sv = Mvcc { snap };
    let dir = pgrc_am::dirpath::table_dir_path(rel.rd_locator.get(), rel.rd_backend);
    let ext = pgrc_am::rowid_alloc::extents(&dir)?;
    let mut keyer = gen.map(|g| GenKeyer::new(g, dir, std::sync::Arc::clone(&ext)));
    // Two with rows (a retired heap the view has unfolded, or the newest current heap past a switch) are a union.
    let slots = pgrc_front::tombs::read_slots(&pgrc_front::tombs::state(gen.map(|g| &**g))?, &pgrc_front::tombs::newest_state(rel)?);
    let heaps = pgrc_front::tombs::open_read(mcx, rel.rd_id, &slots)?;
    let imgs: PgResult<Option<Vec<Arc<CompanionImage>>>> = (|| {
        let mut out = ::mcx::stdvec::try_vec(heaps.len(), "pgrc column view")?;
        for h in &heaps {
            if bufmgr::RelationGetNumberOfBlocksInFork(&h.rel, types_core::ForkNumber::MAIN_FORKNUM)? == 0 {
                continue;
            }
            match crate::image::current(crate::locator(&h.rel), h.rel.rd_id, Kind::Tomb, &BufPages { rel: &h.rel }, &Procs { rel: &h.rel })? {
                Ok(img) => out.push(img),
                Err(_) => return Ok(None),
            }
        }
        Ok(Some(out))
    })();
    pgrc_front::tombs::close_all(heaps)?;
    let Some(imgs) = imgs? else { return Ok(None) };
    let mut nogen = NoGen(ext);
    let k: &mut dyn Keyer = match keyer.as_mut() {
        Some(k) => k,
        None => &mut nogen,
    };
    let mut tombs = ::mcx::stdvec::try_vec(imgs.len(), "pgrc column view")?;
    for img in &imgs {
        tombs.push(TombIn { img, settled: settled(img, k)? });
    }
    let mut ins = ::mcx::stdvec::try_vec(segs.len(), "pgrc column view")?;
    for rec in segs {
        if !images {
            ins.push(SegIn { rec: *rec, img: None });
            continue;
        }
        let s = table::table_open(mcx, rec.relid, AccessShareLock)?;
        let out = crate::image::current(crate::locator(&s), rec.relid, Kind::Segment { slot: rec.slot, part_no: rec.part_no }, &BufPages { rel: &s }, &Procs { rel: &s });
        s.close(NoLock)?;
        ins.push(SegIn { rec: *rec, img: out?.ok() });
    }
    let (seq_sum, segs, filter) = build(k, &tombs, ins, &sv)?;
    Ok(Some(ColView { gen: gen.cloned(), identity: (gen.map_or(0, |g| g.view_key()), seq_sum), segs, filter }))
}

pub struct NoGen(pub std::sync::Arc<pgrc_am::rowid_alloc::Extents>);

impl Keyer for NoGen {
    fn gen(&self) -> u64 {
        0
    }
    fn nparts(&self) -> usize {
        0
    }
    fn seg_of(&self, id: u64) -> Option<(u32, u32)> {
        self.0.seg_of(id)
    }
    fn live_segment(&self, _: u32) -> bool {
        false
    }
    fn homes(&self, _: u64) -> bool {
        false
    }
    fn locate(&mut self, _: u64) -> PgResult<Option<(usize, u32, u32)>> {
        Ok(None)
    }
}
