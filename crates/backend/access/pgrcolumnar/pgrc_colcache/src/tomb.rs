//! Settled tombstones are keyed once per (generation, settled version); recent ones are keyed per statement.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use pgrc_am::catalog::{Generation, RowMap};
use pgrc_format::dml::Tombstone;
use pgrc_format::geom::GRANULE_ROWS;
use pgrc_format::manifest::SegState;
use pgrc_am::rowid_alloc::Extents;
use pgrc_front::filter::TombFilter;
use pgrc_marks::Locator;
use types_error::{PgError, PgResult};

use crate::block::BlockImage;
use crate::image::{is_dead, CompanionImage};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keyed {
    Part { part: usize, granule: u32, row: u32 },
    Segment { part_no: u32, pos: u32 },
    Moot,
}

pub trait Keyer {
    /// The settled keying's generation key (`Generation::view_key`: a composed RR view is not its generation).
    fn gen(&self) -> u64;
    fn nparts(&self) -> usize;
    fn seg_of(&self, id: u64) -> Option<(u32, u32)>;
    fn live_segment(&self, part_no: u32) -> bool;
    fn homes(&self, id: u64) -> bool;
    fn locate(&mut self, id: u64) -> PgResult<Option<(usize, u32, u32)>>;
}

pub struct GenKeyer<'a> {
    gen: &'a Arc<Generation>,
    dir: String,
    ext: Arc<Extents>,
    live: HashSet<u32>,
    maps: Vec<Option<Arc<RowMap>>>,
}

impl<'a> GenKeyer<'a> {
    pub fn new(gen: &'a Arc<Generation>, dir: String, ext: Arc<Extents>) -> GenKeyer<'a> {
        GenKeyer {
            live: gen.segments.iter().filter(|r| r.state != SegState::Sealed).map(|r| r.part_no).collect(),
            maps: Vec::new(),
            gen,
            dir,
            ext,
        }
    }

    #[doc(hidden)]
    pub fn with_maps(gen: &'a Arc<Generation>, ext: Arc<Extents>, maps: Vec<Arc<RowMap>>) -> GenKeyer<'a> {
        let mut k = GenKeyer::new(gen, String::new(), ext);
        k.maps = maps.into_iter().map(Some).collect();
        k
    }
}

impl Keyer for GenKeyer<'_> {
    fn gen(&self) -> u64 {
        self.gen.view_key()
    }
    fn nparts(&self) -> usize {
        self.gen.manifest.parts.len()
    }
    fn seg_of(&self, id: u64) -> Option<(u32, u32)> {
        self.ext.seg_of(id)
    }
    fn live_segment(&self, part_no: u32) -> bool {
        self.live.contains(&part_no)
    }
    fn homes(&self, id: u64) -> bool {
        self.gen.home(id).is_some()
    }
    fn locate(&mut self, id: u64) -> PgResult<Option<(usize, u32, u32)>> {
        if self.maps.is_empty() {
            self.maps = ::mcx::stdvec::try_filled(self.gen.manifest.parts.len(), None, "pgrc tombstone keying")?;
        }
        let (gen, dir, maps) = (self.gen, &self.dir, &mut self.maps);
        let mut map_of = |i: usize| -> PgResult<Arc<RowMap>> {
            Ok(match &maps[i] {
                Some(m) => Arc::clone(m),
                None => Arc::clone(maps[i].insert(gen.row_map(i, dir)?)),
            })
        };
        let Some((i, ord, _)) = gen.locate(id, &mut map_of)? else { return Ok(None) };
        let grain = match gen.manifest.parts[i].granule_rows {
            0 => GRANULE_ROWS,
            g => g,
        };
        Ok(Some((i, ord / grain, ord % grain)))
    }
}

/// A live segment first, then the view part whose ranges hold it, else a segment by `part_no` the scan may list as newer.
pub fn key(k: &mut dyn Keyer, id: u64) -> PgResult<Keyed> {
    let seg = k.seg_of(id);
    if let Some((part_no, pos)) = seg.filter(|s| k.live_segment(s.0)) {
        return Ok(Keyed::Segment { part_no, pos });
    }
    if k.homes(id) {
        return Ok(match k.locate(id)? {
            Some((part, granule, row)) => Keyed::Part { part, granule, row },
            None => Keyed::Moot,
        });
    }
    Ok(seg.map_or(Keyed::Moot, |(part_no, pos)| Keyed::Segment { part_no, pos }))
}

pub fn tombstone(c: &BlockImage, i: usize) -> PgResult<Tombstone> {
    let t = c.tuple_bytes(i);
    let hoff = t[22] as usize;
    let natts = u16::from_ne_bytes([t[18], t[19]]) & 0x07ff;
    let nulls = u16::from_ne_bytes([t[20], t[21]]) & 0x0001 != 0;
    if natts != 2 || nulls || t.len() < hoff + 16 {
        return Err(corrupt("row shape".into()));
    }
    let rd = |at: usize| i64::from_ne_bytes(t[at..at + 8].try_into().expect("8 bytes"));
    Tombstone::from_datums([rd(hoff), rd(hoff + 8)]).map_err(|e| corrupt(format!("{e}")))
}

fn corrupt(what: String) -> Box<PgError> {
    Box::new(PgError::error(format!("pgrcolumnar: tombstone {what}")).with_sqlstate(types_error::ERRCODE_DATA_CORRUPTED))
}

pub struct Settled {
    charge: pgrc_read::mem::Charge,
    pub gen: u64,
    pub ver: u64,
    pub parts: Arc<TombFilter>,
    /// Segment rows by `part_no`: sorted positions.
    pub segs: HashMap<u32, Arc<[u32]>>,
    pub count: usize,
}

pub fn settle(t: &CompanionImage, k: &mut dyn Keyer) -> PgResult<Settled> {
    let mut parts = TombFilter::with_parts(k.nparts());
    let mut segs: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut count = 0;
    for c in t.chunks.iter().flatten() {
        for i in 0..c.rows() {
            if !(c.all_settled || c.is_settled(i)) {
                continue;
            }
            count += 1;
            match key(k, tombstone(c, i)?.rowident)? {
                Keyed::Part { part, granule, row } => parts.add_part_row(part, granule, row),
                Keyed::Segment { part_no, pos } => segs.entry(part_no).or_default().push(pos),
                Keyed::Moot => {}
            }
        }
    }
    Ok(Settled {
        gen: k.gen(),
        ver: t.settled_ver,
        parts: Arc::new(parts.finished()),
        segs: segs
            .into_iter()
            .map(|(p, mut v)| {
                v.sort_unstable();
                v.dedup();
                (p, Arc::from(v))
            })
            .collect(),
        count,
        charge: pgrc_read::mem::Charge::new(count * 4),
    })
}

/// Cached per tombstone locator, two entries (the newest generations in use), only once admitted under the cap.
pub fn settled(t: &CompanionImage, k: &mut dyn Keyer) -> PgResult<Arc<Settled>> {
    let key_of = (t.loc, k.gen(), t.settled_ver);
    if let Some(s) = pgsync::lock(&SETTLED).iter().find(|(l, s)| *l == t.loc && s.gen == key_of.1 && s.ver == key_of.2).map(|(_, s)| Arc::clone(s)) {
        return Ok(s);
    }
    let s = Arc::new(settle(t, k)?);
    if !pgrc_read::mem::make_room() {
        return Ok(s);
    }
    let mut c = pgsync::lock(&SETTLED);
    // Only an image its map publishes: an unmapped one's keyings are forgotten by its unmapper (after it
    // clears `mapped`, before or after this lock), and a private one has no unmapper.
    if !t.mapped.load(std::sync::atomic::Ordering::Relaxed) {
        return Ok(s);
    }
    s.charge.publish();
    let mut kept = 0;
    c.retain(|(l, s)| {
        if *l == t.loc {
            kept += 1;
            if kept > 1 {
                s.charge.unpublish();
            }
            kept <= 1
        } else {
            true
        }
    });
    c.insert(0, (t.loc, Arc::clone(&s)));
    Ok(s)
}

static SETTLED: pgsync::Mutex<Vec<(Locator, Arc<Settled>)>> = pgsync::Mutex::new(Vec::new());

pub fn forget_settled(loc: Locator) {
    pgsync::lock(&SETTLED).retain(|(l, s)| {
        if *l == loc {
            s.charge.unpublish();
        }
        *l != loc
    });
}

pub fn is_recent(c: &BlockImage, i: usize) -> bool {
    !(c.all_settled || c.is_settled(i)) && !is_dead(&c.hdr[i])
}
