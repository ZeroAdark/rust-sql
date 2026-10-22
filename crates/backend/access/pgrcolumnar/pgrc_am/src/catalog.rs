//! The process-wide catalog of columnar relations (the user's ruling 2026-09-13, "load the
//! manifest at server startup"): per relation, the manifest generations a snapshot can still
//! reach with their `(gen, prev_gen, publisher_fxid)`, the decoded manifest, the parts open and
//! pinned for the process's life, the generation's stats (P-1: its stats runs and loose parts,
//! `pgrc_write::stats`, shared by reference with the generations before and after it), and from
//! them the planner facts image, the facts as served and the part-grain answers. It is loaded
//! for every `pgrc_*` directory before the first connection is accepted ([`load_dir`]:
//! read-only, after end-of-recovery cleanup; a generation's runs are read once, its loose parts'
//! footers from the open parts; a generation written before P-1 is served by its sections file,
//! `pgrc_format::sections`, or completed in memory from its open parts) and kept current by the
//! writers in-process: a publish appends its generation with the publisher's stats ([`publish`],
//! which writes nothing), a directory's creation adds an empty entry ([`create`]), its removal drops
//! the entry ([`remove_dir`]). On a hot standby it is loaded at the consistency point and redo
//! keeps it current the same way ([`set_live`], [`redo_publish`], [`redo_truncate`]).
//! Plan time ([`visible`]) walks the in-memory chain from the head
//! under the caller's `CommitCheck` — exactly `resolve_effective`'s law over records instead of
//! files — and opens nothing, lists nothing, reads nothing. A relation absent from the table
//! is an error with a clear message, never a walk of the directory.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use pgrc_format::dirlayout::{manifest_file_name, part_file_name, sections_file_name, CURRENT_FILE_NAME};
use pgrc_format::facts::FactsImage;
use pgrc_format::manifest::{flat_ranges, segments, stats_ref, under, CommitPointer, FlatRanges, IdRanges, Manifest, PartRecord, SegmentRecord, UnderEntry};
use pgrc_format::sections::{self, SectionsFile, SECTION_FACTS};
use pgrc_format::sortkey::SortKeyEntry;
use crate::answers::Answers;
use pgrc_read::{CommitCheck, OpenPart, PartExpect, PartPin, VfsPartIo};
use pgrc_scan::PartDeletes;
use pgrc_write::publish::{TxnProbe, TxnVerdict};
use pgrc_write::stats::{FullTest, StatsState, LOOSE_MAX};
use pgrc_write::wvfs::{RealVfs, WriteVfs};
use types_core::Oid;
use types_error::{PgError, PgResult};

use crate::footer::FooterFacts;
pub use crate::slot_events::{on_slot_event, SlotEvent};
use crate::slot_events::fire;

/// One relation's physical slot: tablespace, database, relfilenumber.
pub type RelSlot = (u32, u32, u64);

/// One part of a generation as the catalog holds it: open, pinned, its delete vector decoded. Shared by `Arc` with
/// the generations that carry the part unchanged (its record and DV equal): a publish carries a part with one count,
/// not a pin, a path copy and four counts per part (P-1 L4a). Readers see the fields through `Deref`.
#[derive(Clone)]
pub struct OpenedPart(Arc<OpenedPartBlock>);

/// D4537 T18b: the `Arc` block of an [`OpenedPart`]: its bytes (the block, the data's path copy) on the part cache, whichever
/// generation or thread drops the last count. The charge is declared FIRST: its drop moves the bytes to the dropping
/// thread's account before the fields below free (ledger_floor, D4627).
struct OpenedPartBlock {
    _charge: memledger::Charge,
    data: OpenedPartData,
}

impl std::ops::Deref for OpenedPart {
    type Target = OpenedPartData;

    fn deref(&self) -> &OpenedPartData {
        &self.0.data
    }
}

impl OpenedPart {
    /// Test witness: the block's charge.
    #[cfg(test)]
    fn _block(&self) -> &memledger::Charge {
        &self.0._charge
    }

    fn new(data: OpenedPartData) -> OpenedPart {
        let bytes = (2 * size_of::<usize>() + size_of::<OpenedPartBlock>() + data.path.as_bytes_with_nul().len()) as u64;
        let mut block = Arc::new(OpenedPartBlock { _charge: charge_on((OWNER, memledger::Release::Evict)), data }); // alloc: Cold charged OWNER (OpenedPartBlock::_charge)
        if let Some(b) = Arc::get_mut(&mut block) {
            b._charge.commit(bytes);
        }
        OpenedPart(block)
    }

    /// This part with another record's expectations or delete vector: the same open part, pinned again.
    fn with(&self, expect: PartExpect, deletes: Option<Arc<PartDeletes>>) -> OpenedPart {
        OpenedPart::new(OpenedPartData { part: Arc::clone(&self.part), pin: PartPin::pin(&self.part), path: self.path.clone(), expect, deletes, row_map: Arc::clone(&self.row_map) })
    }
}

/// What an [`OpenedPart`] holds.
pub struct OpenedPartData {
    pub part: Arc<OpenPart>,
    pub pin: PartPin,
    pub path: CString,
    pub expect: PartExpect,
    pub deletes: Option<Arc<PartDeletes>>,
    /// The part's `RowIdentMap`, decoded on first use; shared with the copies later generations carry (ROWMAP2:
    /// [`crate::mapcache`]).
    pub row_map: Arc<crate::mapcache::MapCell>,
}

/// A part's `RowIdentMap` (design §4.3; IX0 L2): the row ID of each row ordinal. A bulk part's rows take its one
/// range in order; a sealed segment's keep the IDs their heap positions had; a compacted part's (K1) the IDs they had
/// in its inputs.
pub enum RowMap {
    /// A bulk part: row i has ID `base + i`.
    Implicit(u64),
    /// ROWMAP2 M1: per-granule ID encodings (`pgrc_format::rowmap`), the one form a sidecar holds.
    Granules(Arc<GranuleMap>),
}

/// A [`RowMap::Granules`] map: the decoded body, and a FRAMEIX part's reverse once an ID → ordinal lookup has built it
/// (main 10-06: forward-only on disk, rebuilt on the first use into the map's cache cell, whose bytes then count it).
pub struct GranuleMap {
    map: pgrc_format::rowmap::GranMap,
    rev: pgsync::OnceLock<Option<pgrc_format::rowmap::Reverse>>,
    /// The built reverse charged to `owner` (or its failure reported): once, after the build.
    settled: AtomicBool,
    /// The cache cell holding this map (set at admission), charged for a reverse built later.
    owner: pgsync::Mutex<std::sync::Weak<crate::mapcache::MapCell>>,
}

impl GranuleMap {
    pub fn new(map: pgrc_format::rowmap::GranMap) -> GranuleMap {
        GranuleMap { map, rev: pgsync::OnceLock::new(), settled: AtomicBool::new(false), owner: pgsync::Mutex::new(std::sync::Weak::new()) }
    }

    pub fn map(&self) -> &pgrc_format::rowmap::GranMap {
        &self.map
    }

    pub(crate) fn set_owner(&self, cell: std::sync::Weak<crate::mapcache::MapCell>) {
        *pgsync::lock(&self.owner) = cell;
    }

    /// The built reverse (a FRAMEIX part's, on the first ask; `None` for the other forms, which need none). The init
    /// only builds (the map's own IDs, a clock read, counters); the cache charge, or a failure's WARNING, follows it
    /// once, outside the once-guard.
    fn rev(&self) -> Option<&pgrc_format::rowmap::Reverse> {
        if !self.map.needs_rev() {
            return None;
        }
        let rev = self.rev.get_or_init(|| {
            let t = pg_clock::MonoStamp::now();
            let built = self.map.build_frameix().ok();
            crate::mapcache::rev_built(t.elapsed_ns(), u64::from(self.map.rows()));
            built
        });
        if !self.settled.load(Ordering::Acquire) && !self.settled.swap(true, Ordering::AcqRel) {
            match rev {
                Some(r) => {
                    let cell = pgsync::lock(&self.owner).upgrade();
                    if let Some(cell) = cell {
                        cell.grew(self, r.mem_bytes() as u64);
                    }
                }
                None => {
                    let _ = elog::elog(types_error::WARNING, "pgrcolumnar: a RowIdentMap's reverse could not be built (its forward frames repeat an ID): its rows have no ID lookup".to_string()); // alloc: Cold
                }
            }
        }
        rev.as_ref()
    }

    /// The bytes held: the map and a built reverse.
    pub fn mem_bytes(&self) -> u64 {
        (self.map.mem_bytes() + self.rev.get().and_then(Option::as_ref).map_or(0, pgrc_format::rowmap::Reverse::mem_bytes)) as u64
    }
}

impl RowMap {
    /// Row `ordinal`'s ID. The caller bounds `ordinal` by the part's rows (the load checks the map holds exactly that
    /// many).
    pub fn ident(&self, ordinal: u32) -> u64 {
        match self {
            RowMap::Implicit(base) => base + u64::from(ordinal),
            RowMap::Granules(m) => m.map.ident(ordinal),
        }
    }

    /// The IDs of ordinals `ordinal..ordinal + out.len()` (the caller bounds them by the part's rows): the scan's
    /// stamp buffer, one pass per granule beside its columns.
    pub fn decode_ids(&self, ordinal: u32, out: &mut [u64]) {
        match self {
            RowMap::Implicit(base) => out.iter_mut().enumerate().for_each(|(i, x)| *x = base + u64::from(ordinal) + i as u64),
            RowMap::Granules(m) => m.map.decode_ids(ordinal, out),
        }
    }

    /// [`Self::ordinal_of`] for many IDs of one part of `rows` rows (tombstones, folds, index bitmaps): `out[i]` answers
    /// `ids[i]`. A permuted part decodes each frame the IDs hit once when they are many.
    pub fn ordinals_of(&self, rows: u64, ids: &[u64], out: &mut [Option<u32>]) {
        assert_eq!(ids.len(), out.len(), "one answer per ID");
        match self {
            RowMap::Granules(m) => {
                m.map.ordinals_of(ids, m.rev(), out);
                out.iter_mut().for_each(|o| *o = o.filter(|&x| u64::from(x) < rows));
            }
            RowMap::Implicit(_) => out.iter_mut().zip(ids).for_each(|(o, &id)| *o = self.ordinal_of(rows, id)),
        }
    }

    /// [`Self::ident`] for the scan's stamp; `hint` carried the run or group the last call ended in for the forms
    /// ROWMAP2 M2 retired, and is unused.
    #[inline]
    pub fn ident_from(&self, ordinal: u32, _hint: &mut usize) -> u64 {
        self.ident(ordinal)
    }

    /// The ordinal holding ID `id` in a part of `rows` rows, `None` when the part has no such row.
    pub fn ordinal_of(&self, rows: u64, id: u64) -> Option<u32> {
        let o = match self {
            RowMap::Implicit(base) => u32::try_from(id.checked_sub(*base)?).ok()?,
            RowMap::Granules(m) => m.map.ordinal_of(id, m.rev())?,
        };
        (u64::from(o) < rows).then_some(o)
    }

    /// The bytes the decoded map holds (ROWMAP2 stats and budget): an implicit map holds none.
    pub fn mem_bytes(&self) -> u64 {
        match self {
            RowMap::Implicit(_) => 0,
            RowMap::Granules(m) => m.mem_bytes(),
        }
    }

    /// The map as compaction reads it (K1 L3).
    pub fn view(&self) -> pgrc_compact::RowMapView<'_> {
        match self {
            RowMap::Implicit(base) => pgrc_compact::RowMapView::Implicit(*base),
            RowMap::Granules(m) => pgrc_compact::RowMapView::Granules(&m.map),
        }
    }

    /// SORTRUNS-E0: the ordinal holding `id` among virtual frames `va..vb` of a sorted run member's map (a LABELS
    /// run's group), `None` when the member holds no such row there.
    pub fn run_find(&self, va: u32, vb: u32, id: u64) -> pgrc_format::FormatResult<Option<u32>> {
        match self {
            RowMap::Granules(m) => m.map.run_find(va, vb, id),
            RowMap::Implicit(_) => Err(pgrc_format::FormatError::Corrupt { at: "a sorted run member without a run map" }),
        }
    }

    /// SORTRUNS-E0: the ordinal holding `id` in virtual frame `v` of a sorted run member's map (a FRAMEIX run's frame,
    /// or an exception's), `None` when the member holds no such row there.
    pub fn run_find_in(&self, v: u32, id: u64) -> pgrc_format::FormatResult<Option<u32>> {
        match self {
            RowMap::Granules(m) => m.map.run_find_in(v, id),
            RowMap::Implicit(_) => Err(pgrc_format::FormatError::Corrupt { at: "a sorted run member without a run map" }),
        }
    }
}

/// How a run reverse's answer is searched in the member's map.
enum RunFind {
    Group(u32, u32),
    Frame(u32),
}

/// SORTRUNS-E0: the sorted runs of `m` (`EXT_RUNS`) as a generation holds them, each slot's part index. A live slot
/// naming no part of `m`, or a member of a run that homes its IDs under an in-progress creator's tag (`EXT_UNDER`: a
/// member is a merge's output, never a transaction's own part), is corruption.
fn run_homes(m: &Manifest, under: &[UnderEntry]) -> PgResult<Box<[RunHome]>> {
    if !pgrc_format::sortrun::has_runs(&m.ext) {
        return Ok(Box::new([]));
    }
    let runs = pgrc_format::sortrun::runs_in(&m.ext).map_err(|e| crate::read_error(pgrc_read::ReadError::Format(e)))?;
    let bad = |run: &pgrc_format::sortrun::SortRun, why: &str| crate::corrupt(&format!("generation {} sorted run {}: {why}", m.header.gen, run.run_id));
    runs.into_iter()
        .map(|run| {
            let mut slots = crate::alloc::try_vec(run.members.len(), "catalog")?;
            for &p in &run.members {
                slots.push(match p {
                    0 => None,
                    p => Some(m.parts.binary_search_by_key(&p, |x| x.part_no).map_err(|_| bad(&run, &format!("member {p} is not a part of the generation")))?),
                });
            }
            if run.homes() && run.live().any(|p| under.iter().any(|e| e.part_no == p)) {
                return Err(bad(&run, "a member under an in-progress creator's tag"));
            }
            Ok(RunHome { run, slots: slots.into(), cell: None })
        })
        .collect()
}

/// OC-5: `PART_FLAG_ROWMAP` makes the sidecar authoritative (absent = corruption) and its form bits name the body;
/// without the flag and without a sidecar the map is implicit (a bulk part). ROWMAP2 M2 (main 10-07): the one form left
/// is the granules map that every writer writes since M1. A flagged map of another form (sorted, runs, pairs), and an
/// unflagged sidecar (a seal written before M1, which set no flag), are retired: refused with SQLSTATE 55000
/// ([`retired_map`]) without being decoded, and an unflagged sidecar is never taken for an implicit map (a seal of one
/// extent has one range holding its rows, as a bulk part does). IX0: `ids` is the part's `EXT_RANGES` entry: a bulk
/// part's one range; a part without one (written before IX0) has no row IDs, and its implicit map is refused.
fn load_row_map(dir: &str, rec: &pgrc_format::manifest::PartRecord, ids: &[(u64, u64)]) -> PgResult<RowMap> {
    use pgrc_format::manifest::{PART_FLAG_ROWMAP, PART_ROWMAP_FORM_MASK, PART_ROWMAP_FORM_SHIFT, PART_ROWMAP_GRANULES};
    use pgrc_format::sidecar::SidecarKind;
    use pgrc_read::sidecar::SidecarConsult;
    let corrupt = |why: &str| Box::new(PgError::error(format!("pgrcolumnar: part {} RowIdentMap: {why}", rec.part_no)).with_sqlstate(types_error::ERRCODE_DATA_CORRUPTED));
    let flagged = rec.flags & PART_FLAG_ROWMAP != 0;
    let form = (rec.flags & PART_ROWMAP_FORM_MASK) >> PART_ROWMAP_FORM_SHIFT;
    if form != 0 && !flagged {
        return Err(corrupt("a map form without the map flag"));
    }
    if flagged && form != PART_ROWMAP_GRANULES {
        return Err(retired_map(rec));
    }
    let dirio = pgrc_read::VfsTableDir::new(dir.to_string()); // alloc: Cold
    let payload = match pgrc_read::sidecar::read_sidecar(&dirio, rec.part_no, SidecarKind::RowIdentMap, 1, None, None).map_err(crate::read_error)? {
        SidecarConsult::Absent if flagged => return Err(corrupt("sidecar absent (the manifest flags it)")),
        SidecarConsult::Absent => return implicit_map(rec, ids).map_err(|why| corrupt(&why)),
        SidecarConsult::Stale(why) => return Err(corrupt(why)),
        SidecarConsult::Hit { .. } if !flagged => return Err(retired_map(rec)),
        SidecarConsult::Hit { payload, .. } => payload,
    };
    decode_row_map(&payload, rec.rows).map_err(|why| corrupt(&why))
}

/// ROWMAP2 M2: a part whose map is in a form before M1. Not corruption (main 10-07): a format this binary no longer
/// reads, so SQLSTATE 55000 (object_not_in_prerequisite_state); the table's analytic scans still read (they load no map).
fn retired_map(rec: &pgrc_format::manifest::PartRecord) -> Box<PgError> {
    Box::new(
        PgError::error("pgrcolumnar: row-ID map format retired (written before ROWMAP2-M1)".to_string())
            .with_sqlstate(types_error::ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE)
            .with_detail(format!("part {} keeps its row IDs in a sorted, runs or pairs map; this binary reads only the granules map", rec.part_no))
            .with_hint("recreate the table (CREATE TABLE AS / COPY)"),
    )
}

/// A bulk part's map: its one range holds its rows.
fn implicit_map(rec: &pgrc_format::manifest::PartRecord, ids: &[(u64, u64)]) -> Result<RowMap, String> {
    match ids {
        [(lo, hi)] if hi - lo >= rec.rows => Ok(RowMap::Implicit(*lo)),
        [] => Err("no row IDs (a part written before IX0)".into()),
        _ => Err(format!("{} ranges for a bulk part of {} rows", ids.len(), rec.rows)),
    }
}

/// A granules `RowIdentMap` body (ROWMAP2 M1, `pgrc_format::rowmap`) for a part of `rows` rows.
fn decode_row_map(payload: &[u8], rows: u64) -> Result<RowMap, String> {
    Ok(RowMap::Granules(Arc::new(GranuleMap::new(pgrc_format::rowmap::GranMap::decode(payload, rows).map_err(|e| format!("{e:?}"))?))))
}

/// Where a generation keeps a row ID (the one-home law: a part or a sorted run).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdHome {
    /// The part at this index of the manifest, through its `EXT_RANGES` entry and its map.
    Part(usize),
    /// SORTRUNS-E0: the sorted run at this index of [`Generation::runs`], through its reverse and a member's map.
    Run(usize),
}

impl IdHome {
    /// The part index of a part home.
    pub fn part(self) -> Option<usize> {
        match self {
            IdHome::Part(i) => Some(i),
            IdHome::Run(_) => None,
        }
    }
}

/// A run home's tag in [`Directory`]'s entries (a part's is its index).
const RUN_HOME: u32 = 1 << 31;

/// IX0 L2: a generation's row-ID directory: every range of every part, by first ID, with its part's index; SORTRUNS-E0:
/// and every range a sorted run homes, with the run's index tagged [`RUN_HOME`].
#[derive(Debug, Default)]
pub struct Directory {
    ranges: Box<[(u64, u64, u32)]>,
}

impl Directory {
    /// The ranges of `nparts` parts and of the HOMES runs of `runs`, sorted only when they are out of part order (bulk
    /// parts take theirs in order).
    fn new(ids: &FlatRanges, nparts: usize, runs: &[RunHome]) -> PgResult<Directory> {
        let mut ranges: Vec<(u64, u64, u32)> = crate::alloc::try_vec(ids.ids.len() + runs.iter().map(|r| r.run.ranges.len()).sum::<usize>(), "catalog")?;
        for i in 0..nparts {
            ranges.extend(ids.of(i).iter().map(|&(lo, hi)| (lo, hi, i as u32)));
        }
        for (r, h) in runs.iter().enumerate() {
            ranges.extend(h.run.ranges.iter().map(|&(lo, hi)| (lo, hi, RUN_HOME | r as u32)));
        }
        if !ranges.is_sorted() {
            ranges.sort_unstable();
        }
        Ok(Directory { ranges: ranges.into() })
    }

    /// The home an entry's tag names.
    #[inline]
    fn home_of(i: u32) -> IdHome {
        if i & RUN_HOME != 0 { IdHome::Run((i & !RUN_HOME) as usize) } else { IdHome::Part(i as usize) }
    }

    /// The range holding `id` and its home.
    pub fn find_range(&self, id: u64) -> Option<(u64, u64, IdHome)> {
        let at = self.ranges.partition_point(|r| r.0 <= id).checked_sub(1)?;
        let (lo, hi, i) = self.ranges[at];
        (id < hi).then(|| (lo, hi, Self::home_of(i)))
    }

    /// The home of `id`: the part or the sorted run whose ranges hold it.
    #[inline]
    pub fn find(&self, id: u64) -> Option<IdHome> {
        let at = self.ranges.partition_point(|r| r.0 <= id).checked_sub(1)?;
        let (_, hi, i) = self.ranges[at];
        (id < hi).then(|| Self::home_of(i))
    }

    /// The ranges of part index `i`, in ID order.
    pub fn of(&self, i: usize) -> Vec<(u64, u64)> {
        self.ranges.iter().filter(|r| r.2 as usize == i).map(|r| (r.0, r.1)).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }
}

/// SORTRUNS-E0 L1: a sorted run of a generation (`pgrc_format::sortrun`): its `EXT_RUNS` entry, the manifest index of
/// each slot's part (`None` for an emptied slot), and (a run that homes its IDs) the cell of its reverse, bound by
/// the catalog to the table's directory ([`Generation::bind_runs`]).
pub struct RunHome {
    pub run: pgrc_format::sortrun::SortRun,
    pub slots: Box<[Option<usize>]>,
    cell: Option<Arc<crate::runcache::RunCell>>,
}

impl RunHome {
    /// The cell of the run's reverse (a HOMES run bound to its directory).
    pub fn cell(&self) -> Option<&Arc<crate::runcache::RunCell>> {
        self.cell.as_ref()
    }
}

/// One manifest generation still on disk.
pub struct Generation {
    /// D4537: [`Generation::held_bytes`] on the part cache, a composed view's on its reader (declared FIRST: its drop
    /// moves the bytes to the dropping thread's account before the fields below free; ledger_floor, D4627).
    charge: pgsync::Mutex<memledger::Charge>,
    pub gen: u64,
    pub prev_gen: u64,
    pub publisher_fxid: u64,
    pub schema_fp: u64,
    pub manifest: Arc<Manifest>,
    /// Manifest order.
    pub parts: Vec<OpenedPart>,
    /// The ring's segment records (F1), by slot.
    pub segments: Vec<SegmentRecord>,
    /// IX0 L2: the row-ID directory (`EXT_RANGES`): which part holds an ID; and each part's ranges, by part index
    /// (IXCOPYPATH: one list, not one per part).
    dir: Directory,
    ids: FlatRanges,
    /// SORTRUNS-E0: the generation's sorted runs (`EXT_RUNS`), by `run_id`.
    runs: Box<[RunHome]>,
    /// P-1: the generation's stats runs and loose parts (the publisher's, or read at load), its image and answers'
    /// source; a run is one `Arc` across every generation that holds it. `None`: a generation written before P-1
    /// (its sections file or a completion holds its image and answers), one whose stats were refused (completed), a
    /// composed view (RR-VIEW) or a detached one.
    pub stats: Option<Arc<StatsState>>,
    /// K1 L7's full test of the table (the publisher's `hold`, P-1 L2), from the first part on first ask (`Some`:
    /// asked).
    full: pgsync::Mutex<Option<Option<Arc<FullTest>>>>,
    /// `CURRENT`'s record for this generation (the publisher's check that its base is still the head), on first ask.
    pointer: pgsync::Mutex<Option<CommitPointer>>,
    /// Assembled from `stats` on first ask, or set by a load (a sections file, a completion) or the plan rig.
    image: pgsync::Mutex<Option<Arc<FactsImage>>>,
    facts: pgsync::Mutex<Option<Arc<FooterFacts>>>,
    answers: pgsync::Mutex<Option<Arc<Answers>>>,
    /// The first part's SortKey entries (the planner's `sorted_by` witness), read once per process.
    sort_key: pgsync::Mutex<Option<Arc<[SortKeyEntry]>>>,
    /// RR-VIEW: a snapshot reader's own view composed by [`visible`] (not a published generation; the newest own
    /// generation's number and header).
    composed: bool,
    /// SEAL-OFFHEAD: the manifest's `EXT_UNDER` tags (the parts a stacked maintenance publish carries from a
    /// transaction in progress when it published), by `part_no`. Empty for every generation but a stacked one.
    under: Box<[UnderEntry]>,
    /// COMPOSED: a composed view's key ([`Self::view_key`]), interned per content ([`shared_view`]), top bit set.
    key: u64,
    /// A composed view's content hash: [`shared_view`]'s prefilter before its exact compare.
    hash: u64,
    /// COMPOSED: what a reader built from this view ([`Self::reader_state`]), dying with it.
    reader: pgsync::OnceLock<Arc<dyn std::any::Any + Send + Sync>>,
    /// COMPOSED: a composed view's stats ([`view_stats`]), its image's and answers' source, built once.
    vstats: pgsync::OnceLock<Option<Arc<StatsState>>>,
    /// SEAL-OFFHEAD: a view [`visible`] resolved through `EXT_UNDER` tags: the parts it hid (creators the check does
    /// not see), plus one when a tag's creator is the reader's own transaction (its fate is not settled).
    stacked: u32,
    /// SEAL-OFFHEAD: a composed view's source, the published generation whose stats runs and loose parts seed the
    /// view's ([`view_stats`]); `None` for a published generation (and a view without one: every part's footer).
    source: Option<Arc<Generation>>,
    /// FASTADD: the manifest lists parts under an older schema (`EXT_SCHEMAS`): readers fill the columns they lack.
    lagged: bool,
    /// COMPOSED: a [`filtered`] view's hidden parts and own-tag flag: with `source`, its whole content, so a lookup
    /// finds it kept before building it ([`kept_filtered`]).
    filtered_by: Option<(Box<[u32]>, bool)>,
}

/// D4537: the catalog's bytes are the part cache's: its generations go with their parts, and nothing evicts them (they
/// leave at prune, invalidation or drop). No cache scope: they are never part of an evictable image.
const OWNER: memledger::Owner = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::PartCache));

fn charge_on((owner, release): (memledger::Owner, memledger::Release)) -> memledger::Charge {
    memledger::Charge::open(owner, release, memledger::Kind::Region, memledger::charge::Door::Heap, 0)
}

/// D4537 1d: a writing transaction's own bytes go to its session (a thread with no backend: its own holder).
fn session_owner() -> (memledger::Owner, memledger::Release) {
    match memledger::charge::context().0 {
        o @ memledger::Owner::Cache(_) => (o, memledger::Release::Evict),
        o => (memledger::Owner::Session(o.acct()), memledger::Release::Session),
    }
}

const ARC: usize = 2 * size_of::<usize>();

fn image_bytes(i: &FactsImage) -> u64 {
    let cols: usize = i.cols.iter().map(|c| c.ndv_body.capacity() + ARC + c.zones.bytes().len()).sum();
    (ARC + size_of::<FactsImage>() + i.cols.capacity() * size_of::<pgrc_format::facts::ColFacts>() + cols) as u64
}

/// A served zone column's map shares the image's bytes ([`image_bytes`]); its summary and expansion are its own.
fn served_bytes(f: &FooterFacts) -> u64 {
    let n = |cap: usize, w: usize| cap * w;
    let cols = f.cols.as_ref().map_or(0, |c| n(c.disk_bytes.capacity() + c.ncodes.capacity(), 8) + n(c.avg_width.capacity() + c.max_len.capacity(), 4) + n(c.qual_class.capacity() + c.codec.capacity(), 1) + n(c.nonnull.capacity(), size_of::<Option<u64>>()));
    let zones: usize = f.zones.iter().map(crate::footer::ZoneFacts::heap_bytes).sum();
    (ARC + size_of::<FooterFacts>() + f.ndv.as_ref().map_or(0, |v| n(v.capacity(), 8)) + cols + n(f.zones.capacity(), size_of::<crate::footer::ZoneFacts>()) + zones) as u64
}

fn answers_bytes(a: &Answers) -> u64 {
    let cols = a.columns().unwrap_or(&[]);
    (ARC + size_of::<Answers>() + cols.len() * size_of::<(u32, Vec<u8>)>() + cols.iter().map(|(_, p)| p.capacity()).sum::<usize>()) as u64
}

fn paths_bytes(parts: &[OpenedPart]) -> usize {
    parts.iter().map(|p| p.path.as_bytes_with_nul().len()).sum()
}

/// [`Generation::election_digest`] of `g`: its parts' shred digests (`OpenPart::shred_digest`, read once per part) in
/// manifest order, as the readers' part lists are built; a part that shredded nothing is skipped and a run of equal
/// digests folds once.
fn election_digest(g: &Generation) -> Option<u64> {
    if g.parts.len() != g.manifest.parts.len() {
        return None;
    }
    let (mut h, mut last) = (0xcbf2_9ce4_8422_2325u64, 0);
    for i in 0..g.manifest.parts.len() {
        let d = g.parts.get(i)?.part.shred_digest().ok()?;
        if d != 0 && d != last {
            h = d.to_le_bytes().iter().fold(h, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3));
            last = d;
        }
    }
    Some(h)
}

/// D4804: generations `a` and `b` carry the same open parts in the same order (what an election read answers is a
/// function of that list).
pub fn same_parts(a: &Generation, b: &Generation) -> bool {
    a.parts.len() == b.parts.len() && a.parts.iter().zip(&b.parts).all(|(x, y)| Arc::ptr_eq(&x.part, &y.part))
}

impl Drop for Generation {
    /// The parts' `Arc` counts are scattered lines: each is hinted a few parts ahead of its release (P-1 L4a; a
    /// reclaim retires several generations of every part at once).
    fn drop(&mut self) {
        let parts = std::mem::take(&mut self.parts);
        let lines: Vec<*const OpenedPartBlock> = parts.iter().map(|p| Arc::as_ptr(&p.0)).collect(); // alloc: Cold
        for (k, p) in parts.into_iter().enumerate() {
            if let Some(&q) = lines.get(k + PREFETCH_AHEAD) {
                prefetch_write(q);
            }
            drop(p);
        }
    }
}

impl Generation {
    /// D4804: what the lowering-time election reads can answer over this generation's parts (Gravity's
    /// `shred_election`: the first part in manifest order that elected a jsonb path fixes its lane kind and scale;
    /// `shred_dict` / `shred_object_roots`: every electing part's seal and object-roots flag), as one digest: in
    /// manifest order, each part's digest of the stream entries those reads consult, a run of equal parts counted
    /// once. Two generations of one digest answer each of those reads alike (a merge of unlike parts moves it). `None`: a part's path table or
    /// stream directory did not read, or the open parts are not the manifest's.
    pub fn election_digest(&self) -> Option<u64> {
        election_digest(self)
    }

    /// RR-VIEW: a snapshot reader's own view that [`visible`] composed: its snapshot's generation plus the parts its
    /// own publishes installed. It carries the newest own generation's number but not all of its parts.
    pub fn composed(&self) -> bool {
        self.composed
    }

    /// FASTADD: some part was written under an older schema than the header's (it lacks a column added since, and
    /// readers fill it): a reader of stored streams only (SQE) declines the table.
    pub fn lagged(&self) -> bool {
        self.lagged
    }

    /// The identity of the view for what a backend or the process keys by generation (positions, the row cache,
    /// Gravity's part lists): the number, or for a composed view (RR-VIEW, SEAL-OFFHEAD) a key interned per content
    /// from a process-wide counter with the top bit set ([`shared_view`]): never a published generation's, never
    /// another content's, the same for one content while any reader holds it. Two composed views of one number differ
    /// in content (an own view before and after a ROLLBACK TO, a stacked view that hides one writer or two), so the
    /// number alone cannot key them.
    pub fn view_key(&self) -> u64 {
        if self.composed {
            self.key
        } else {
            self.gen
        }
    }

    /// SEAL-OFFHEAD: the `EXT_UNDER` tags of this generation (none for a view).
    pub fn under(&self) -> &[UnderEntry] {
        &self.under
    }

    /// SEAL-OFFHEAD: a view [`visible`] resolved through `EXT_UNDER` tags ([`Self::composed`] as well): how many parts
    /// it hid, plus one when it keeps a part of the reader's own unsettled transaction. 0 otherwise.
    pub fn stacked(&self) -> u32 {
        self.stacked
    }

    /// COMPOSED: a reader's state for this composed view (Gravity's part list), `build` run once per view: concurrent
    /// first readers wait for the one build, which holds no catalog lock. It lives as long as the view, so nothing keys
    /// it by number. `None` when the slot holds another type (a bug: one reader kind per process).
    pub fn reader_state<T: std::any::Any + Send + Sync>(&self, build: impl FnOnce() -> Arc<T>) -> Option<Arc<T>> {
        Arc::clone(self.reader.get_or_init(|| -> Arc<dyn std::any::Any + Send + Sync> { build() })).downcast::<T>().ok()
    }

    /// COMPOSED: the composed view's stats ([`view_stats`]), once per view; `None` for a published generation and a
    /// view whose stats were refused (logged).
    fn view_state(&self) -> Option<Arc<StatsState>> {
        if !self.composed {
            return None;
        }
        self.vstats
            .get_or_init(|| match view_stats(self) {
                Ok(st) => Some(Arc::new(st)),
                Err(e) => {
                    log(true, format!("pgrcolumnar: a view of generation {} stats from its parts: {e}", self.gen)); // alloc: Cold
                    None
                }
            })
            .clone()
    }

    fn seal_hash(&mut self) {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut mix = |x: u64| {
            for b in x.to_le_bytes() {
                h = (h ^ b as u64).wrapping_mul(0x100_0000_01b3);
            }
        };
        mix(self.gen);
        for p in &self.manifest.parts {
            mix(p.part_no as u64);
            mix(p.dv_gen);
        }
        for r in &self.segments {
            mix((r.slot as u64) << 40 | r.state as u64);
            mix(r.part_no as u64);
        }
        self.hash = h;
    }

    /// The planner facts image: assembled from the stats on the first ask of this process (the zones in manifest
    /// order, `serve`-equal to the fold over the parts), then held.
    pub fn image(&self) -> Option<Arc<FactsImage>> {
        let mut slot = pgsync::lock(&self.image);
        if slot.is_none() {
            if let Some(s) = &self.stats {
                match s.image(&self.manifest) {
                    Ok(i) => *slot = Some(Arc::new(i)),
                    // the stats were checked against the manifest at publish or load: a bug, never a path
                    Err(e) => log(true, format!("pgrcolumnar: generation {} image from its stats: {e}", self.gen)), // alloc: Cold
                }
            } else if let Some(st) = self.view_state() {
                // SEAL-OFFHEAD: a composed view's facts (the planner asks under its committed-or-own check, which
                // composes while a stacked generation's creator is in progress): from its own stats, once per view
                match st.image(&self.manifest) {
                    Ok(i) => *slot = Some(Arc::new(i)),
                    Err(e) => log(true, format!("pgrcolumnar: a view of generation {} image from its parts: {e}", self.gen)), // alloc: Cold
                }
            }
        }
        slot.clone()
    }

    /// K1 L7's full test of the table, read from the first part's stream directory on the first ask (a publish
    /// carries its base generation's): the publisher's `hold` (P-1 L2). `None` for a generation without parts.
    pub fn full_test(&self) -> Option<Arc<FullTest>> {
        let mut slot = pgsync::lock(&self.full);
        if slot.is_none() {
            *slot = Some(self.parts.first().and_then(|p| FullTest::from_part(&p.part).ok().map(Arc::new)));
        }
        slot.clone().flatten()
    }

    /// `CURRENT`'s record naming this generation: its publisher's ([`Self::set_pointer`]), else computed once (one
    /// encode of the manifest).
    pub fn pointer(&self) -> CommitPointer {
        *pgsync::lock(&self.pointer).get_or_insert_with(|| pgrc_write::publish::commit_pointer(&self.manifest))
    }

    /// `CURRENT`'s record as this generation's publisher wrote it (the manifest's length and the plan's crc): P-1 L4a,
    /// the next publish's check encodes nothing.
    pub fn set_pointer(&self, p: CommitPointer) {
        debug_assert_eq!(p, pgrc_write::publish::commit_pointer(&self.manifest), "the publisher's pointer is its manifest's");
        *pgsync::lock(&self.pointer) = Some(p);
    }

    /// What a publish on this generation passes its publisher ([`BaseHold::stats`]).
    pub fn base_hold(self: &Arc<Generation>) -> BaseHold {
        let folded = self.stats.is_none();
        BaseHold {
            gen: Arc::clone(self),
            full: if folded { None } else { self.full_test() },
            image: if folded { self.image() } else { None },
            answers: if folded { self.answers() } else { None },
        }
    }

    /// The full test of `base` (a generation of the same schema) for this one: a publish does not re-read the first
    /// part. COMPACTP: a base without parts has none to carry (its answer is "no parts", not the table's test): a
    /// table's first generation is empty, and its carried `None` left every later one without a test (the L0 ledger
    /// counted none, so the part-count trigger never fired before a restart; every merging publish opened a base part).
    fn carry_from(&self, base: Option<&Generation>) {
        if let Some(f) = base.filter(|b| b.schema_fp == self.schema_fp).and_then(|b| pgsync::lock(&b.full).clone()).filter(Option::is_some) {
            *pgsync::lock(&self.full) = Some(f);
        }
    }

    pub fn set_image(&self, image: Arc<FactsImage>) {
        *pgsync::lock(&self.image) = Some(image);
        self.refit();
    }

    /// The facts as served, computed once per generation per process by `serve` from the image.
    pub fn facts(&self, serve: impl FnOnce(&FactsImage) -> PgResult<FooterFacts>) -> PgResult<Option<Arc<FooterFacts>>> {
        if let Some(f) = pgsync::lock(&self.facts).clone() {
            return Ok(Some(f));
        }
        let Some(image) = self.image() else { return Ok(None) };
        let f = Arc::new(serve(&image)?); // alloc: Cold charged self.charge (held_bytes, refit below)
        let served = {
            let mut slot = pgsync::lock(&self.facts);
            if slot.is_none() {
                *slot = Some(Arc::clone(&f));
            }
            slot.clone().expect("set")
        };
        self.refit();
        Ok(Some(served))
    }

    /// The plan rig's external publish: these facts, whatever the image says.
    pub fn install_facts(&self, facts: Arc<FooterFacts>) {
        *pgsync::lock(&self.facts) = Some(facts);
        self.refit();
    }

    /// The part-grain answers (every part's Stats body per column), from memory. COMPOSED: a composed view's own,
    /// over its parts (its stats, once per view), never its number's generation's.
    pub fn answers(&self) -> Option<Arc<Answers>> {
        if let Some(a) = pgsync::lock(&self.answers).clone() {
            return Some(a);
        }
        let st = self.view_state()?;
        let a = match Answers::from_state(st, &self.manifest) {
            Ok(a) => Arc::new(a),
            Err(why) => {
                log(true, format!("pgrcolumnar: a view of generation {} answers from its stats: {why}", self.gen)); // alloc: Cold
                return None;
            }
        };
        Some(Arc::clone(pgsync::lock(&self.answers).get_or_insert(a)))
    }

    fn set_answers(&self, a: Arc<Answers>) {
        *pgsync::lock(&self.answers) = Some(a);
        self.refit();
    }

    /// The first part's SortKey entries (empty = no part, or an unordered part), `read` from the
    /// part on the first ask of this process and kept with the generation.
    pub fn sort_key(&self, read: impl FnOnce(&OpenPart) -> PgResult<Vec<SortKeyEntry>>) -> PgResult<Arc<[SortKeyEntry]>> {
        if let Some(k) = pgsync::lock(&self.sort_key).clone() {
            return Ok(k);
        }
        let k: Arc<[SortKeyEntry]> = match self.parts.first() {
            Some(p) => read(&p.part)?.into(),
            None => Arc::from(Vec::new()),
        };
        *pgsync::lock(&self.sort_key) = Some(Arc::clone(&k));
        Ok(k)
    }

    /// Part `part`'s `RowIdentMap`, read from `dir` on the first ask of this process and kept in the part's cell
    /// ([`crate::mapcache::MapCell`]).
    pub fn row_map(&self, part: usize, dir: &str) -> PgResult<Arc<RowMap>> {
        self.parts[part].row_map.get_or_load(|| load_row_map(dir, &self.manifest.parts[part], self.ids.of(part)))
    }

    /// IX0 L2: the home of row ID `id` (the one-home law: at most one): the part whose ranges hold it, or (SORTRUNS-E0)
    /// the sorted run whose ranges do. `None`: no home in this generation (a live segment's row, or a row whose part
    /// is gone).
    #[inline]
    pub fn home(&self, id: u64) -> Option<IdHome> {
        self.dir.find(id)
    }

    /// [`Self::home`] with the range of the home that holds `id` (a caller locating many IDs remembers it).
    pub fn home_range(&self, id: u64) -> Option<(u64, u64, IdHome)> {
        self.dir.find_range(id)
    }

    /// The row IDs part `part` homes (its `EXT_RANGES` entry), in ID order. A member of a sorted run that homes its
    /// IDs has none: the run homes them ([`Self::run_member`]).
    pub fn ids_of(&self, part: usize) -> &[(u64, u64)] {
        self.ids.of(part)
    }

    /// SORTRUNS-E0: the generation's sorted runs (`EXT_RUNS`), by `run_id`.
    pub fn runs(&self) -> &[RunHome] {
        &self.runs
    }

    /// SORTRUNS-E0: `(run index, slot)` when part `part` is a member of a sorted run that homes its IDs (its rows' IDs
    /// are the run's, not an `EXT_RANGES` entry's).
    pub fn run_member(&self, part: usize) -> Option<(usize, usize)> {
        self.runs.iter().enumerate().filter(|(_, h)| h.run.homes()).find_map(|(r, h)| h.slots.iter().position(|&s| s == Some(part)).map(|k| (r, k)))
    }

    /// SORTRUNS-E0 L1: whether part `part` is a member of a sorted run that homes its IDs and has other live members:
    /// no L0 job takes it (the run pass merges and rewrites such members, L3). The sole member of its run is a job's
    /// input like any part, bringing the run's ranges (`crate::compact::member_ids`).
    pub fn pinned_member(&self, part: usize) -> bool {
        self.run_member(part).is_some_and(|(r, _)| self.runs[r].run.live().count() > 1)
    }

    /// Where row `id` is: `(part index, ordinal, the part's map)`, `None` when this generation holds no such row (no
    /// home; SORTRUNS-E0: an emptied slot, or a row the member no longer holds). A part home answers through the part's
    /// map; a run home through the run's reverse (its cell, [`Self::bind_runs`]) and the member's map. `map_of` gives
    /// a part's map by index (a caller locating many IDs caches them).
    pub fn locate(&self, id: u64, map_of: &mut dyn FnMut(usize) -> PgResult<Arc<RowMap>>) -> PgResult<Option<(usize, u32, Arc<RowMap>)>> {
        match self.home(id) {
            None => Ok(None),
            Some(h) => self.locate_at(h, id, map_of),
        }
    }

    /// [`Self::locate`] in home `h` (the home of `id`).
    pub fn locate_at(&self, h: IdHome, id: u64, map_of: &mut dyn FnMut(usize) -> PgResult<Arc<RowMap>>) -> PgResult<Option<(usize, u32, Arc<RowMap>)>> {
        let part = match h {
            IdHome::Part(i) => {
                let map = map_of(i)?;
                return Ok(map.ordinal_of(self.manifest.parts[i].rows, id).map(|o| (i, o, map)));
            }
            IdHome::Run(r) => r,
        };
        let home = &self.runs[part];
        let cell = home.cell.as_ref().ok_or_else(|| crate::corrupt(&format!("sorted run {} has no reverse bound in generation {}", home.run.run_id, self.gen)))?;
        let (slot, find) = match cell.lookup(&home.run, id)? {
            pgrc_format::runrev::Hit::Absent => return Ok(None),
            pgrc_format::runrev::Hit::Group { slot, va, vb } => (slot, RunFind::Group(va, vb)),
            pgrc_format::runrev::Hit::Frame { slot, vframe } => (slot, RunFind::Frame(vframe)),
        };
        let at = home.slots.get(usize::from(slot)).ok_or_else(|| cell.error(pgrc_format::FormatError::Corrupt { at: "run reverse slot" }))?;
        // an emptied slot: every row of the member was dropped
        let Some(i) = *at else { return Ok(None) };
        let map = map_of(i)?;
        let rows = self.manifest.parts[i].rows;
        let o = match find {
            RunFind::Group(va, vb) => map.run_find(va, vb, id),
            RunFind::Frame(v) => map.run_find_in(v, id),
        }
        .map_err(|e| crate::corrupt(&format!("part {} (member {slot} of sorted run {}) RowIdentMap: {e}", self.manifest.parts[i].part_no, home.run.run_id)))?;
        Ok(o.filter(|&o| u64::from(o) < rows).map(|o| (i, o, map)))
    }

    /// SORTRUNS-E0: bind each sorted run that homes its IDs to its reverse in table directory `dir`: the cell of
    /// `carried` (the generation this one builds on) for a run it holds, else a new one.
    pub fn bind_runs(&mut self, dir: &str, carried: Option<&Generation>) {
        self.bind_runs_from(carried.map(|c| &c.runs[..]).unwrap_or(&[]), Some(dir));
    }

    /// Bind the runs to the cells `from` holds for them; with `dir`, a run `from` lacks gets a new cell.
    fn bind_runs_from(&mut self, from: &[RunHome], dir: Option<&str>) {
        for h in self.runs.iter_mut().filter(|h| h.run.homes() && h.cell.is_none()) {
            h.cell = from.iter().filter_map(|f| f.cell.as_ref()).find(|c| c.is_of(&h.run)).cloned().or_else(|| dir.map(|d| crate::runcache::RunCell::open(d, &h.run)));
        }
    }

    /// Tests: bind run `run_id` to `cell`.
    #[doc(hidden)]
    pub fn bind_run_cell(&mut self, run_id: u64, cell: Arc<crate::runcache::RunCell>) {
        if let Some(h) = self.runs.iter_mut().find(|h| h.run.run_id == run_id) {
            h.cell = Some(cell);
        }
    }

    /// A generation of `m` with no part open (position tests).
    #[doc(hidden)]
    pub fn detached(m: Manifest) -> PgResult<Generation> {
        Generation::new(m, Vec::new(), None, None)
    }

    /// COMPOSED (tests of a view's readers): a composed view of `m` with no part open, keyed as [`shared_view`] keys one.
    #[doc(hidden)]
    pub fn detached_view(m: Manifest) -> PgResult<Arc<Generation>> {
        let mut v = Generation::new(m, Vec::new(), None, None)?;
        v.into_view();
        v.seal_hash();
        v.key = 1 << 63 | NEXT_VIEW_KEY.fetch_add(1, Ordering::Relaxed);
        v.refit();
        Ok(Arc::new(v)) // alloc: Cold charged self.charge (held_bytes)
    }

    fn new(m: Manifest, parts: Vec<OpenedPart>, image: Option<Arc<FactsImage>>, answers: Option<Arc<Answers>>) -> PgResult<Generation> {
        let prev = m.header.prev_gen;
        Generation::linked(m, prev, parts, image, answers)
    }

    /// RECLAIMDEFER (i′): this generation again under another `Arc`: its manifest, open parts and stats ([`perturb_below`]).
    pub(crate) fn twin(&self) -> PgResult<Generation> {
        let mut g = Generation::linked(self.manifest.as_ref().clone(), self.prev_gen, self.parts.clone(), None, None)?;
        g.bind_runs_from(&self.runs, None);
        g.stats = self.stats.clone();
        g.refit();
        Ok(g)
    }

    /// Tests: this generation's open parts under another manifest (GENID: an impostor base for the carry guard).
    #[cfg(test)]
    pub(crate) fn with_manifest(&self, m: Manifest) -> PgResult<Generation> {
        Generation::new(m, self.parts.clone(), None, None)
    }

    /// `prev_gen` as the catalog chains it: a load drops aborted generations, so a kept one links to the next kept.
    fn linked(m: Manifest, prev_gen: u64, parts: Vec<OpenedPart>, image: Option<Arc<FactsImage>>, answers: Option<Arc<Answers>>) -> PgResult<Generation> {
        let h = m.header;
        let format = |e| crate::read_error(pgrc_read::ReadError::Format(e));
        let segments = segments(&m).map_err(format)?;
        let ids = flat_ranges(&m).map_err(format)?;
        let under = under(&m).map_err(format)?.into_boxed_slice();
        let runs = run_homes(&m, &under)?;
        let dir = Directory::new(&ids, m.parts.len(), &runs)?;
        crate::fastadd::harvest(&m).map_err(format)?;
        let lagged = h.flags & pgrc_format::manifest::MANIFESTF_SCHEMAS != 0;
        let manifest = Arc::new(m); // alloc: Cold charged self.charge (held_bytes)
        let g = Generation {
            gen: h.gen,
            prev_gen,
            publisher_fxid: h.publisher_fxid,
            schema_fp: h.schema_fingerprint,
            manifest,
            parts,
            segments,
            dir,
            ids,
            runs,
            stats: None,
            full: pgsync::Mutex::new(None),
            pointer: pgsync::Mutex::new(None),
            image: pgsync::Mutex::new(image),
            facts: pgsync::Mutex::new(None),
            answers: pgsync::Mutex::new(answers),
            sort_key: pgsync::Mutex::new(None),
            composed: false,
            under,
            key: 0,
            hash: 0,
            reader: pgsync::OnceLock::new(),
            vstats: pgsync::OnceLock::new(),
            stacked: 0,
            source: None,
            lagged,
            filtered_by: None,
            charge: pgsync::Mutex::new(charge_on((OWNER, memledger::Release::Evict))),
        };
        g.refit();
        Ok(g)
    }

    /// D4537 1c: this generation as a reader's composed view (RR-VIEW, SEAL-OFFHEAD, COMPOSED): its bytes are the reader's
    /// (`memledger::charge::context()`), not the part cache's.
    fn into_view(&mut self) {
        self.composed = true;
        *pgsync::lock(&self.charge) = charge_on(memledger::charge::context());
    }

    /// D4537 1b: the record (its Arc block), manifest with its parts and extension bytes, part list, the directory, ranges,
    /// runs, segments and `EXT_UNDER` tags, and the image, facts and answers it keeps. The parts' contents are the part
    /// cache's own, and so are the open-part records (one `Arc` shared by every generation that lists the part).
    fn held_bytes(&self) -> u64 {
        let own = ARC + size_of::<Generation>() + ARC + size_of::<Manifest>() + self.manifest.parts.capacity() * size_of::<PartRecord>() + self.manifest.ext.capacity();
        let parts = self.parts.capacity() * size_of::<OpenedPart>();
        let index = self.segments.capacity() * size_of::<SegmentRecord>() + self.dir.ranges.len() * size_of::<(u64, u64, u32)>() + self.ids.ids.capacity() * size_of::<(u64, u64)>() + self.ids.at.capacity() * 4;
        let runs = self.runs.len() * size_of::<RunHome>() + self.runs.iter().map(|r| r.slots.len() * size_of::<Option<usize>>()).sum::<usize>() + self.under.len() * size_of::<UnderEntry>();
        let view = self.filtered_by.as_ref().map_or(0, |(h, _)| h.len() * 4);
        let image = pgsync::lock(&self.image).as_deref().map_or(0, image_bytes);
        let facts = pgsync::lock(&self.facts).as_deref().map_or(0, served_bytes);
        let answers = pgsync::lock(&self.answers).as_deref().map_or(0, answers_bytes);
        (own + parts + index + runs + view) as u64 + image + facts + answers
    }

    /// D4537 1b (R2): after each slot this generation fills, on the filling thread (it allocated them).
    fn refit(&self) {
        let mut c = pgsync::lock(&self.charge);
        c.fit_committed(self.held_bytes());
    }
}

/// A publish's base generation as the catalog holds it, kept alive for the publish (P-1).
pub struct BaseHold {
    gen: Arc<Generation>,
    full: Option<Arc<FullTest>>,
    image: Option<Arc<FactsImage>>,
    answers: Option<Arc<Answers>>,
}

impl BaseHold {
    /// The base as the publisher takes it: the generation's stats (the runs by reference) with its manifest and
    /// `CURRENT` record, or for a generation without stats (written before P-1, or completed) its image and answers
    /// columns, the seed of the legacy run.
    pub fn stats(&self) -> pgrc_write::publish::BaseStats<'_> {
        use pgrc_write::publish::{BaseGen, BaseStats};
        let g = &self.gen;
        match &g.stats {
            Some(stats) => BaseStats::Gen(BaseGen { manifest: &g.manifest, pointer: g.pointer(), stats, full: self.full.as_deref() }),
            None => BaseStats::Folded(self.image.as_deref(), self.answers.as_ref().and_then(|a| a.columns())),
        }
    }
}

/// The facts image a sections file carries, when it is this generation's.
fn image_of(file: &SectionsFile, m: &Manifest) -> Option<FactsImage> {
    let i = file.find(SECTION_FACTS, 0)?;
    let img = FactsImage::decode(file.section(i).ok()?).ok()?;
    let h = img.header;
    ((h.gen, h.publisher_fxid, h.schema_fingerprint) == (m.header.gen, m.header.publisher_fxid, m.header.schema_fingerprint) && h.parts == m.parts.len() as u64).then_some(img)
}

/// Why a sections file was not taken.
enum Refused {
    /// An older version's form (the zone maps of version 1): rewritten, not a fault.
    OlderVersion(u32),
    Bad(String),
}

/// The generation's sections file, when present and its manifest's (`mcrc` = the manifest
/// file's trailing crc). `Err` names a refused file.
fn read_sections(dir: &str, m: &Manifest, mcrc: u32) -> Result<Option<SectionsFile>, Refused> {
    let mut vfs = RealVfs;
    let bytes = match vfs.read_full(&format!("{dir}/{}", sections_file_name(m.header.gen))) {
        Ok(b) => b,
        Err(pgrc_write::WriteError::Io { errno: libc::ENOENT, .. }) => return Ok(None),
        Err(e) => return Err(Refused::Bad(format!("{e}"))), // alloc: Cold
    };
    let f = match SectionsFile::parse(bytes) {
        Ok(f) => f,
        Err(pgrc_format::FormatError::BadVersion { got, .. }) if got < sections::SECTIONS_VERSION => return Err(Refused::OlderVersion(got)),
        Err(e) => return Err(Refused::Bad(format!("{e}"))), // alloc: Cold
    };
    if !f.names(m, mcrc) {
        return Err(Refused::Bad("sections file names another manifest".to_string())); // alloc: Cold
    }
    Ok(Some(f))
}

/// What a generation lacks, computed from its open parts in memory (the walk, paid once): a generation written before
/// P-1 of `LOOSE_MAX` parts or more without its sections file, or one whose stats were refused. Nothing is written
/// (P-1: the next publish writes the generation's legacy run, OC-5).
fn complete(gen: &Generation, fold: &dyn Fn(&Generation) -> PgResult<FactsImage>) -> PgResult<()> {
    if gen.image().is_none() {
        let i = Arc::new(fold(gen)?); // alloc: Cold charged gen.charge (held_bytes, refit by set_image)
        gen.set_image(i);
    }
    if gen.answers().is_none() {
        let parts: Vec<&OpenPart> = gen.parts.iter().map(|p| &*p.part).collect(); // alloc: Cold
        let cols = crate::answers::build_from_parts(&parts)?;
        let a = Answers::new(gen.parts.len(), cols).map_err(|e| crate::read_error(pgrc_read::ReadError::Format(e)))?;
        let a = Arc::new(a); // alloc: Cold charged gen.charge (held_bytes, refit by set_answers)
        gen.set_answers(a);
    }
    Ok(())
}

struct RelEntry {
    /// D4537 1a: [`RelEntry::held_bytes`] on the part cache (declared FIRST: its drop moves the bytes to the dropping thread's account before the fields below free (ledger_floor, D4627)).
    charge: memledger::Charge,
    dir: String,
    /// Ascending by `gen`.
    gens: Vec<Arc<Generation>>,
    /// The generation `CURRENT` names (0 = none).
    head: u64,
    /// The plan rig's facts for a relation without generations.
    override_facts: Option<Arc<FooterFacts>>,
    /// GENID-2: the part numbers of generations that left the entry as rolled back (a publish, or its redo, built
    /// on an older generation), which no held generation names: files the next reclaim unlinks ([`dead_parts`]).
    /// Before GENID a successor usually renamed its own part over a dead one's name; its numbers are new now.
    dead_parts: BTreeSet<u32>,
    /// K1-RECLAIM: when the table's next reclaim lists the whole directory ([`sweep_due`]).
    sweep: SweepMemo,
}

impl RelEntry {
    fn new(dir: &str) -> RelEntry {
        let dir = dir.to_string(); // alloc: Cold charged OWNER (RelEntry::held_bytes)
        let mut e = RelEntry { dir, gens: Vec::new(), head: 0, override_facts: None, dead_parts: BTreeSet::new(), sweep: SweepMemo::default(), charge: charge_on((OWNER, memledger::Release::Evict)) };
        e.refit();
        e
    }

    /// D4537 1a: the entry, its node in each of the catalog's tables (`dirs` keys a copy of `dir`), its lists' blocks, the
    /// plan rig's facts, and the reclaim's memos (`dead_parts`, `sweep`: a tree node's links counted at a word each way).
    /// The generations charge their own bytes.
    fn held_bytes(&self) -> u64 {
        let nodes = size_of::<(RelSlot, RelEntry)>() + size_of::<(String, RelSlot)>() + size_of::<(u32, RelSlot)>() + size_of::<RelSlot>();
        let lists = self.gens.capacity() * size_of::<Arc<Generation>>();
        let memos = self.dead_parts.len() * (size_of::<u32>() + 2 * size_of::<usize>())
            + self.sweep.pending.len() * (size_of::<u32>() + 2 * size_of::<usize>())
            + self.sweep.dead.capacity() * size_of::<(u64, String)>()
            + self.sweep.dead.iter().map(|(_, n)| n.capacity()).sum::<usize>();
        (nodes + 2 * self.dir.capacity() + lists + memos) as u64 + self.override_facts.as_deref().map_or(0, served_bytes)
    }

    /// D4537 1a (R2): after each change to its lists, on the changing thread.
    fn refit(&mut self) {
        let n = self.held_bytes();
        self.charge.fit_committed(n);
    }
}

/// K1-RECLAIM: a reclaim names the files it unlinks from what changed (the held generations below the kept one, the
/// dead parts and the rolled-back generations' own files), not from a listing of the directory. What that cannot see
/// (another server life's generations, a publish that failed before the catalog held it, an aborted writer's DV, the
/// optional sidecars of a part it retired) a full sweep, the listing, unlinks by the same rules: the first reclaims of
/// the entry (or after end-of-recovery dropped generations) until one keeps a generation above them, and every
/// [`SWEEP_EVERY`]-th or once [`PENDING_MAX`] retired parts wait.
#[derive(Debug)]
struct SweepMemo {
    /// A full sweep is due until one keeps a generation above this (a new entry: its directory holds whatever an
    /// earlier life left).
    due: Option<u64>,
    /// Incremental reclaims since the last full sweep.
    since: u32,
    /// Parts an incremental reclaim unlinked, whose optional sidecars (stats, fp128, ...) the next sweep unlinks.
    pending: BTreeSet<u32>,
    /// The files rolled-back generations wrote under their own number (manifest, sections, bank stats, runs, DVs),
    /// `(gen, name)`: a reclaim keeping a generation above `gen` judges (and unlinks) them.
    dead: Vec<(u64, String)>,
}

impl Default for SweepMemo {
    fn default() -> SweepMemo {
        SweepMemo { due: Some(0), since: 0, pending: BTreeSet::new(), dead: Vec::new() }
    }
}

/// K1-RECLAIM: the files dead generation `g` wrote under its own number.
fn own_files(g: &Generation) -> impl Iterator<Item = (u64, String)> + '_ {
    use pgrc_format::sidecar::SidecarKind;
    let n = g.gen;
    let runs = g.stats.iter().flat_map(|st| st.root.iter().chain(st.runs.iter().map(|r| &r.rref))).filter(move |r| r.gen == n).map(|r| pgrc_format::dirlayout::run_file_name(r.gen, r.seq));
    let dvs = g.manifest.parts.iter().filter(move |p| p.dv_gen == n).map(|p| pgrc_format::dirlayout::sidecar_file_name(p.part_no, SidecarKind::Dv, p.dv_gen));
    // SORTRUNS-E0: the reverse of a run this generation published (its run ID is the generation's)
    let runrev = pgrc_format::sortrun::runs_in(&g.manifest.ext).unwrap_or_default().into_iter().filter(move |r| r.run_id == n && r.homes()).map(|r| pgrc_format::dirlayout::runrev_file_name(r.run_id));
    [manifest_file_name(n), sections_file_name(n), pgrc_format::bankstats::bankstats_file_name(n)].into_iter().chain(runs).chain(dvs).chain(runrev).map(move |f| (n, f))
}

/// K1-RECLAIM: a full sweep at least every this many reclaims of a table.
pub const SWEEP_EVERY: u32 = 256;
/// K1-RECLAIM: a full sweep once this many retired parts wait for their optional sidecars' unlink.
pub const PENDING_MAX: usize = 4096;

impl RelEntry {
    /// GENID: the generations above `prev` leave (rolled back: the publisher of `new`, whose base is `prev`, passed
    /// them), returned (their open parts leave the registry: `parts_only_in`); GENID-2: their parts that neither `new`
    /// nor a kept generation names are kept for the next reclaim.
    fn drop_dead(&mut self, prev: u64, new: &Manifest) -> Vec<Arc<Generation>> {
        if self.gens.iter().all(|g| g.gen <= prev) {
            return Vec::new();
        }
        let (dead, kept): (Vec<Arc<Generation>>, Vec<Arc<Generation>>) = std::mem::take(&mut self.gens).into_iter().partition(|g| g.gen > prev); // alloc: Cold charged OWNER (RelEntry::held_bytes, the entry refits after)
        self.gens = kept;
        // K1-RECLAIM: the dead generations' own files (manifest, runs, DVs) are named by no held one
        for g in &dead {
            self.sweep.dead.extend(own_files(g)); // alloc: Cold charged OWNER (RelEntry::held_bytes, the entry refits after)
        }
        let names = |m: &Manifest, n: u32| m.parts.binary_search_by_key(&n, |p| p.part_no).is_ok();
        for p in dead.iter().flat_map(|g| g.manifest.parts.iter()) {
            if !names(new, p.part_no) && !self.gens.iter().any(|g| names(&g.manifest, p.part_no)) {
                self.dead_parts.insert(p.part_no); // alloc: Cold charged OWNER (RelEntry::held_bytes, the entry refits after)
            }
        }
        dead
    }
}

struct Tables {
    rels: BTreeMap<RelSlot, RelEntry>,
    dirs: BTreeMap<String, RelSlot>,
    relids: BTreeMap<u32, RelSlot>,
    fold_vintage: BTreeSet<RelSlot>,
    /// GENID: per table directory, the highest generation number and the highest `next_part_no` any generation of it
    /// held in this process ([`used_numbers`]). Kept when the entry goes and the directory stays (a same-transaction
    /// TRUNCATE, its redo, an unlogged reset); dropped only with the directory ([`forget_dir`], [`remove_under`]).
    used: BTreeMap<String, (u64, u32)>,
    /// D4537 T18b: the bytes of `used` (each entry's node and key), on [`USED_CHARGE`].
    used_bytes: usize,
}

pgsync::process_global! {
    static CATALOG: pgsync::Mutex<Tables> = pgsync::Mutex::new(Tables { rels: BTreeMap::new(), dirs: BTreeMap::new(), relids: BTreeMap::new(), fold_vintage: BTreeSet::new(), used: BTreeMap::new(), used_bytes: 0 });
}

pgsync::process_global! {
    /// D4537 T18b: the bytes of the process-wide registries below, on the part cache (they leave with their tables'
    /// entries and directories), refit after each change: [`Tables::used`], [`VIEWS`]' map and lists, [`SETTLE`].
    static USED_CHARGE: pgsync::Mutex<memledger::Charge> = pgsync::Mutex::new(memledger::Charge::NONE);
    static VIEWS_CHARGE: pgsync::Mutex<memledger::Charge> = pgsync::Mutex::new(memledger::Charge::NONE);
    static SETTLE_CHARGE: pgsync::Mutex<memledger::Charge> = pgsync::Mutex::new(memledger::Charge::NONE);
}

/// A tree node's links, at two words.
const NODE: usize = 2 * size_of::<usize>();

/// D4537 T18b (R2): `charge` fitted to `bytes` on the part cache (opened at the first bytes).
fn refit_cache(charge: &pgsync::Mutex<memledger::Charge>, bytes: usize) {
    let mut c = pgsync::lock(charge);
    if c.owner() != OWNER {
        *c = charge_on((OWNER, memledger::Release::Evict));
    }
    c.fit_committed(bytes as u64);
}

fn used_entry_bytes(dir: &str) -> usize {
    size_of::<(String, (u64, u32))>() + NODE + dir.len()
}

/// GENID: the numbers generation `(gen, next_part_no)` of `dir` takes are used from now on in this process.
fn note_used(c: &mut Tables, dir: &str, gen: u64, next_part_no: u32) {
    match c.used.get_mut(dir) {
        Some(u) => *u = (u.0.max(gen), u.1.max(next_part_no)),
        None => {
            c.used.insert(dir.to_string(), (gen, next_part_no)); // alloc: Cold charged the part cache (USED_CHARGE, refit below)
            c.used_bytes += used_entry_bytes(dir);
            refit_cache(&USED_CHARGE, c.used_bytes);
        }
    }
}

/// GENID (pgrc-plans/GENID.done): the highest generation number and the highest `next_part_no` any generation of the
/// table directory `dir` held in this process, `(0, 0)` for none. Every cache of the process keys by (slot,
/// generation) or by part path, and a same-transaction TRUNCATE (the directory emptied, numbering restarted) or a
/// rolled-back publish (its successor's base does not know its numbers) would otherwise give a second content the
/// numbers of a first: the publisher numbers above these ([`pgrc_write::publish::TxnProbe::used_numbers`]).
pub fn used_numbers(dir: &str) -> (u64, u32) {
    pgsync::lock(&CATALOG).used.get(dir).copied().unwrap_or((0, 0))
}

/// SORTRUNS-E0 L2: a generation number for a run job's run ID, reserved before the job writes the run's reverse (the
/// file names it, and so does its page 0): above every generation of `dir` this process held and above `at_least`
/// (the newest generation the job read), and used from now on. It relies on GENID's invariant: every publish numbers
/// its generation above both its base and [`used_numbers`] of its directory (`pgrc_write::publish::next_gen`), so
/// once noted used here, the run's own publish is numbered above the ID (`publish_runs` checks `run_id < gen`) and no
/// later run job of this process takes it again. Across processes the maintenance lock serializes run jobs (and the
/// gates' knob runs, whose ID is their publish's generation), and every earlier run's ID is at or below the newest
/// generation this job read, so a published run ID is never another run's. A job that never publishes leaves a gap
/// (its staged reverse is the transaction's temp: unlinked on abort, or by the end-of-recovery cleanup).
pub fn reserve_gen(dir: &str, at_least: u64) -> u64 {
    let mut c = pgsync::lock(&CATALOG);
    let g = c.used.get(dir).map_or(0, |u| u.0).max(at_least) + 1;
    note_used(&mut c, dir, g, 0);
    g
}

/// GENID: the directory itself is gone (DROP, an aborted creation, a rewrite's old locator): its numbers go with it.
pub fn forget_dir(dir: &str) {
    let mut c = pgsync::lock(&CATALOG);
    if c.used.remove(dir).is_some() {
        c.used_bytes -= used_entry_bytes(dir);
        refit_cache(&USED_CHARGE, c.used_bytes);
    }
}

static LIVE: AtomicBool = AtomicBool::new(false);

/// The load is starting: from here on redo keeps the catalog current.
pub fn set_live() {
    LIVE.store(true, Ordering::SeqCst);
}

pub fn is_live() -> bool {
    LIVE.load(Ordering::SeqCst)
}

/// What a plan-time lookup finds.
pub enum Lookup {
    /// The visible generation.
    Gen(Arc<Generation>),
    /// The relation is known and has no generation visible to the caller.
    Empty,
    /// The relation is not in the catalog.
    Unknown,
}

/// The visible generation of `slot` under `check`: the chain walked from the head by
/// `prev_gen` until a committed publisher.
///
/// RR-VIEW (pgrc-plans/RR-VIEW-design.md): the reader's own generations (`check.own`, a snapshot reader's own
/// publishes) are walked past to the base, the newest other generation its snapshot sees. When the oldest own one
/// chained on the base, the newest own one is the view as it stands (every READ COMMITTED statement after a publish).
/// When it chained over generations the snapshot does not see, their parts ride in every own generation, and the view
/// is [`compose`]d: the base plus what the own generations installed.
pub fn visible(slot: RelSlot, check: &dyn CommitCheck) -> PgResult<Lookup> {
    visible_at_head(slot, check).map(|(found, _)| found)
}

/// IXDMLPATH2 r4: [`visible`] and the head generation its walk started from, read under the same lock (`None`: the
/// relation is not in the catalog), so a cache of the view needs no second [`head`].
pub fn visible_at_head(slot: RelSlot, check: &dyn CommitCheck) -> PgResult<(Lookup, Option<u64>)> {
    let (own, base, prevs, head) = {
        let c = pgsync::lock(&CATALOG);
        let Some(e) = c.rels.get(&slot) else { return Ok((Lookup::Unknown, None)) };
        let find = |g: u64| {
            e.gens.iter().find(|r| r.gen == g).ok_or_else(|| Box::new(PgError::error(format!("pgrcolumnar: manifest generation {g} of relfilenumber {} is not in the catalog", slot.2)))) // alloc: Cold
        };
        let mut own: Vec<Arc<Generation>> = Vec::new();
        let mut base = None;
        let mut g = e.head;
        while g != 0 {
            let rec = find(g)?;
            if check.own(rec.publisher_fxid) {
                // D339 (trunk 0a14b58f683, ported): an own generation published at or after the snapshot's command (a
                // cursor declared before the COPY) is not in its view: the walk goes on to its predecessor
                if check.sees_command(rec.publisher_fxid, (slot.0, slot.1, slot.2, rec.gen)) {
                    own.push(Arc::clone(rec));
                }
            } else if check.committed(rec.publisher_fxid) {
                base = Some(Arc::clone(rec));
                break;
            }
            g = rec.prev_gen;
        }
        let Some(oldest) = own.last() else {
            return match base {
                Some(b) => filtered(slot, e.head, b, check, &BTreeSet::new()).map(|g| (Lookup::Gen(g), Some(e.head))),
                None => Ok((Lookup::Empty, Some(e.head))),
            };
        };
        // SEAL-OFFHEAD: the own generations are the view as it stands only when they chain on each other and on the
        // base; a maintenance publish stacked between two of them (A's gen, X's seal, A's next gen) is a generation
        // the snapshot may not see, whose seal the newest own one carries
        let contiguous = own.windows(2).all(|w| w[0].prev_gen == w[1].gen);
        if contiguous && oldest.prev_gen == base.as_ref().map_or(0, |b| b.gen) {
            // CONCLOAD: hidden by the base's and the older own generations' tags too. A publish drops the tag of a
            // creator committed by then, so the newest own generation can list untagged a part whose creator committed
            // after this snapshot; the generation it was tagged in says whose it is
            let also: BTreeSet<u32> = own[1..].iter().map(|o| &**o).chain(base.as_deref()).flat_map(|g| hidden(g, check).0).collect(); // alloc: Cold
            return filtered(slot, e.head, Arc::clone(&own[0]), check, &also).map(|g| (Lookup::Gen(g), Some(e.head)));
        }
        let prevs = own.iter().map(|o| if o.prev_gen == 0 { Ok(None) } else { find(o.prev_gen).map(|p| Some(Arc::clone(p))) }).collect::<PgResult<Vec<_>>>()?; // alloc: Cold
        (own, base, prevs, e.head)
    };
    let hide: BTreeSet<u32> = own.iter().map(|o| &**o).chain(base.as_deref()).flat_map(|g| hidden(g, check).0).collect();
    let mut view = compose(&own, base.as_deref(), &prevs, &hide)?;
    view.source = base;
    Ok((Lookup::Gen(shared_view(slot, head, view)), Some(head)))
}

/// SEAL-OFFHEAD: the `EXT_UNDER` parts of `g` whose creator `check` does not see, and whether a tag's creator is the
/// checker's own transaction (a part it sees whose fate is not settled).
fn hidden(g: &Generation, check: &dyn CommitCheck) -> (Vec<u32>, bool) {
    let mut own = false;
    let hide = g
        .under
        .iter()
        .filter(|e| {
            own |= check.own(e.fxid);
            !check.committed(e.fxid)
        })
        .map(|e| e.part_no)
        .collect(); // alloc: Cold
    (hide, own)
}

/// SEAL-OFFHEAD: `g` as `check` sees it. A generation without tags, or whose tags' creators the check sees and are not
/// its own, is itself (a published number names one content: a stacked generation is itself once every creator it
/// tags committed). Otherwise a composed view: `g` without the parts of creators the check does not see (their rows
/// are another transaction's uncommitted or rolled-back ones) and without the tags. A view that hid a rolled-back
/// writer's parts queues the table's settle ([`settle_due`]). CONCLOAD: `also`, parts other generations' tags hide
/// from `check` (an own view's base), hide as well where `g` lists them.
fn filtered(slot: RelSlot, head: u64, g: Arc<Generation>, check: &dyn CommitCheck, also: &BTreeSet<u32>) -> PgResult<Arc<Generation>> {
    let lists = |n: &u32| g.manifest.parts.binary_search_by_key(n, |p| p.part_no).is_ok();
    if g.under.is_empty() && !also.iter().any(lists) {
        return Ok(g);
    }
    let (mut hide, own) = hidden(&g, check);
    hide.extend(also.iter().copied().filter(lists));
    hide.sort_unstable();
    hide.dedup();
    if hide.is_empty() && !own {
        if g.gen == head {
            release_kept(slot, head);
        }
        return Ok(g);
    }
    if !hide.is_empty() && crate::probe::clog_available() {
        let probe = crate::probe::ClogTxnProbe::new();
        if g.under.iter().any(|e| hide.contains(&e.part_no) && probe.verdict(e.fxid) == TxnVerdict::Aborted) {
            queue_settle(slot);
        }
        // a settle is an optimisation: a creator the probe cannot adjudicate leaves the table unqueued, never the read
        let _ = probe.take_error();
    }
    if let Some(v) = kept_filtered(slot, head, &g, &hide, own) {
        return Ok(v);
    }
    let format = |e| crate::read_error(pgrc_read::ReadError::Format(e));
    let keep: Vec<usize> = (0..g.manifest.parts.len()).filter(|&i| !hide.contains(&g.manifest.parts[i].part_no)).collect(); // alloc: Cold
    let mut ext = Vec::new();
    for (tag, bytes) in pgrc_format::manifest::ext_records(&g.manifest.ext).map_err(format)? {
        if tag != pgrc_format::manifest::EXT_UNDER {
            pgrc_format::manifest::put_ext_record(&mut ext, tag, bytes);
        }
    }
    let mut header = g.manifest.header;
    header.part_count = keep.len() as u32;
    let records: Vec<PartRecord> = keep.iter().map(|&i| g.manifest.parts[i]).collect(); // alloc: Exec charged context() (the view's held_bytes)
    // IX0: a hidden part's EXT_RANGES entry leaves with it (the view homes none of its IDs); the floor stays
    let ext = pgrc_format::manifest::rebuild_ranges(&ext, &records, &[]).map_err(format)?;
    let parts = keep.iter().filter_map(|&i| g.parts.get(i)).cloned().collect(); // alloc: Exec charged context() (the view's held_bytes)
    let mut view = Generation::linked(Manifest::new(header, records).with_ext(ext), g.prev_gen, parts, None, None)?;
    view.bind_runs_from(&g.runs, None);
    view.into_view();
    view.stacked = hide.len() as u32 + own as u32;
    view.seal_hash();
    view.source = Some(g);
    view.filtered_by = Some((hide.into_boxed_slice(), own)); // alloc: Exec charged context() (the view's held_bytes)
    view.refit();
    Ok(shared_view(slot, head, view))
}

/// COMPOSED: the kept view of `g` without `hide` (own: `own`) under head `head`, found without building it (a lookup
/// of a composed view is then O(tags), not O(parts)).
fn kept_filtered(slot: RelSlot, head: u64, g: &Arc<Generation>, hide: &[u32], own: bool) -> Option<Arc<Generation>> {
    SHARED_CALLS.with(|c| c.set(c.get() + 1));
    let c = pgsync::lock(&VIEWS);
    let views = c.get(&slot)?;
    let hit = |v: &Generation| v.source.as_ref().is_some_and(|s| Arc::ptr_eq(s, g)) && v.filtered_by.as_ref().is_some_and(|(x, o)| **x == *hide && *o == own);
    views.kept.iter().find(|(h, v)| *h == head && hit(v)).map(|(_, v)| Arc::clone(v))
}

/// SEAL-OFFHEAD: a composed view's stats: its source's runs and loose parts that name only parts the view holds with
/// the same identity (one `Arc` each, not copied), and every other part's from its footer (the open part; a view
/// hides a few recent parts, so few). Covers the view's manifest exactly (checked).
fn view_stats(view: &Generation) -> Result<StatsState, String> {
    use pgrc_write::stats::{ident_of, part_stats_from_open, read_error};
    let m = &view.manifest;
    let want: BTreeMap<u32, _> = m.parts.iter().map(|p| (p.part_no, ident_of(p))).collect(); // alloc: Cold
    let src = view.source.as_ref().and_then(|s| s.stats.as_deref());
    let mut st = StatsState::default();
    let mut have = BTreeSet::new();
    if let Some(src) = src {
        for r in src.runs.iter().filter(|r| !r.parts.is_empty() && r.parts.iter().all(|p| want.get(&p.part_no) == Some(p))) {
            have.extend(r.parts.iter().map(|p| p.part_no));
            st.runs.push(Arc::clone(r));
        }
        for p in src.loose.iter().filter(|p| want.get(&p.ident.part_no) == Some(&p.ident)) {
            have.insert(p.ident.part_no);
            st.loose.push(Arc::clone(p));
        }
    }
    for (i, rec) in m.parts.iter().enumerate().filter(|(_, r)| !have.contains(&r.part_no)) {
        let part = view.parts.get(i).ok_or_else(|| format!("part {} of the view is not open", rec.part_no))?;
        let mut ps = part_stats_from_open(&part.part, rec).map_err(|e| read_error(e).to_string())?;
        if let Some(r) = src.and_then(|s| s.run_of(rec.part_no)) {
            ps.ext = r.ext.iter().filter(|e| e.0 == rec.part_no).map(|e| (e.1, e.2.clone())).collect();
        }
        st.loose.push(Arc::new(ps));
    }
    st.check(m).map_err(|e| e.to_string())?;
    Ok(st)
}

/// SEAL-OFFHEAD: composed views kept per table while its head stands (at most [`VIEWS_MAX`]): one `Arc` per view
/// content per process, so its facts, and what a backend keys by the view's pointer, are built once, not per lookup.
const VIEWS_MAX: usize = 4;

/// A table's composed views: `kept` (strong, the head they were resolved under), and COMPOSED's `live` (weak, every
/// view a reader may still hold), so one content is one `Arc` and one key while any reader holds it.
#[derive(Default)]
struct Views {
    kept: Vec<(u64, Arc<Generation>)>,
    live: Vec<std::sync::Weak<Generation>>,
}

pgsync::process_global! {
    /// Taken after `CATALOG` (never the other way).
    static VIEWS: pgsync::Mutex<BTreeMap<RelSlot, Views>> = pgsync::Mutex::new(BTreeMap::new());
}

fn views_bytes(m: &BTreeMap<RelSlot, Views>) -> usize {
    m.len() * (size_of::<(RelSlot, Views)>() + NODE)
        + m.values().map(|v| v.kept.capacity() * size_of::<(u64, Arc<Generation>)>() + v.live.capacity() * size_of::<std::sync::Weak<Generation>>()).sum::<usize>()
}

fn refit_views(m: &BTreeMap<RelSlot, Views>) {
    refit_cache(&VIEWS_CHARGE, views_bytes(m));
}

/// COMPOSED: the next composed view key (top bit clear; [`shared_view`] sets it). Never reused in the process.
static NEXT_VIEW_KEY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
/// COMPOSED: views kept over every table: a fresh reader's release ([`release_kept`]) skips the lock at 0.
static KEPT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

thread_local! {
    /// COMPOSED: this thread's view registry visits ([`shared_view`], [`kept_filtered`]; the cost guard's witness: a
    /// plain lookup makes none).
    static SHARED_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// COMPOSED: view registry visits on this thread so far.
#[doc(hidden)]
pub fn shared_view_calls() -> u64 {
    SHARED_CALLS.with(|c| c.get())
}

/// The view equal to `view` (content and stacked count): a kept one, else one a reader still holds, else `view` with a
/// new key; kept under head `head`. COMPOSED: content equality is exact (the hash only filters).
fn shared_view(slot: RelSlot, head: u64, mut view: Generation) -> Arc<Generation> {
    SHARED_CALLS.with(|c| c.set(c.get() + 1));
    let mut c = pgsync::lock(&VIEWS);
    let views = c.entry(slot).or_default();
    let before = views.kept.len();
    views.kept.retain(|(h, _)| *h == head);
    let same = |v: &Generation| {
        v.hash == view.hash && v.gen == view.gen && v.stacked == view.stacked && v.manifest.parts == view.manifest.parts && v.segments == view.segments
            // FASTADD: the schemas the view lists its parts under (EXT_SCHEMAS) and its header's are content too
            && v.schema_fp == view.schema_fp && v.lagged == view.lagged && v.manifest.ext == view.manifest.ext
    };
    let found = views.kept.iter().find(|(_, v)| same(v)).map(|(_, v)| Arc::clone(v));
    let out = match found {
        Some(v) => v,
        None => {
            let mut held = None;
            views.live.retain(|w| match w.upgrade() {
                Some(v) => {
                    if held.is_none() && same(&v) {
                        held = Some(v);
                    }
                    true
                }
                None => false,
            });
            let v = held.unwrap_or_else(|| {
                view.key = 1 << 63 | NEXT_VIEW_KEY.fetch_add(1, Ordering::Relaxed);
                // D4537 1c (memacct ruling): a view the registry keeps is the cache side's, not its first reader's statement's
                pgsync::lock(&view.charge).transfer(OWNER, memledger::Release::Evict);
                let v = Arc::new(view);
                views.live.push(Arc::downgrade(&v)); // alloc: Cold
                v
            });
            if views.kept.len() >= VIEWS_MAX {
                views.kept.remove(0);
            }
            views.kept.push((head, Arc::clone(&v))); // alloc: Cold
            v
        }
    };
    let after = views.kept.len();
    note_kept(before, after);
    refit_views(&c);
    out
}

fn note_kept(before: usize, after: usize) {
    if after >= before {
        KEPT.fetch_add(after - before, Ordering::Relaxed);
    } else {
        KEPT.fetch_sub(before - after, Ordering::Relaxed);
    }
}

/// The table's kept views leave (its head moved); a view a reader holds keeps its key for its content.
fn drop_views(slot: RelSlot) {
    let mut c = pgsync::lock(&VIEWS);
    if let Some(v) = c.get_mut(&slot) {
        note_kept(v.kept.len(), 0);
        v.kept.clear();
        v.live.retain(|w| w.strong_count() > 0);
    }
    refit_views(&c);
}

/// The table left the catalog: its views go (a re-created slot numbers its generations over).
fn forget_views(slot: RelSlot) {
    let gone = {
        let mut c = pgsync::lock(&VIEWS);
        let gone = c.remove(&slot);
        refit_views(&c);
        gone
    };
    if let Some(v) = &gone {
        note_kept(v.kept.len(), 0);
    }
}

/// COMPOSED: a fresh reader saw the head `head` whole, its tags' creators all committed: the views kept under it leave
/// (an older snapshot that still needs one composes it again).
fn release_kept(slot: RelSlot, head: u64) {
    if KEPT.load(Ordering::Relaxed) == 0 {
        return;
    }
    let mut c = pgsync::lock(&VIEWS);
    if let Some(v) = c.get_mut(&slot) {
        let before = v.kept.len();
        v.kept.retain(|(h, _)| *h != head);
        note_kept(before, v.kept.len());
    }
    refit_views(&c);
}

/// COMPOSED: the composed views of `slot` alive in the process (kept, or held by a reader); `pgrc_views`.
pub fn live_views(slot: RelSlot) -> usize {
    let mut c = pgsync::lock(&VIEWS);
    let n = c.get_mut(&slot).map_or(0, |v| {
        v.live.retain(|w| w.strong_count() > 0);
        v.live.len()
    });
    refit_views(&c);
    n
}

pgsync::process_global! {
    /// SEAL-OFFHEAD: tables whose newest generation carries a rolled-back writer's parts under its tags (a reader hid
    /// them): the front launcher queues each for a settle publish, after which readers read it whole again.
    static SETTLE: pgsync::Mutex<BTreeSet<RelSlot>> = pgsync::Mutex::new(BTreeSet::new());
}

/// SEAL-OFFHEAD: the tables queued for a settle since the last call ([`filtered`]).
pub fn settle_due() -> Vec<RelSlot> {
    let mut s = pgsync::lock(&SETTLE);
    let due = std::mem::take(&mut *s).into_iter().collect(); // alloc: Cold
    refit_settle(&s);
    due
}

fn settle_bytes(s: &BTreeSet<RelSlot>) -> usize {
    s.len() * (size_of::<RelSlot>() + NODE)
}

fn refit_settle(s: &BTreeSet<RelSlot>) {
    refit_cache(&SETTLE_CHARGE, settle_bytes(s));
}

/// SEAL-OFFHEAD: `slot` is queued for a settle publish.
fn queue_settle(slot: RelSlot) {
    let mut s = pgsync::lock(&SETTLE);
    s.insert(slot); // alloc: Cold charged the part cache (SETTLE_CHARGE, refit below)
    refit_settle(&s);
}

fn index_of_parts(parts: &[PartRecord], part_no: u32) -> Option<usize> {
    parts.binary_search_by_key(&part_no, |p| p.part_no).ok()
}

/// RR-VIEW: the own view of a snapshot reader whose publishes chained over generations its snapshot does not see.
/// `own` = its own generations, newest first; `prevs[i]` = the generation `own[i]` chained on (the last one's is the
/// chained-over generation); `base` = the newest other generation the snapshot sees. The view has the newest own
/// generation's number and header, and:
/// - the base's parts, each with the newest own generation's record for it when one lists it (a DV folds only
///   horizon-passed tombstones, visible to every live snapshot; and the reader's own compile deleted the tombstones it
///   folded), else the base's;
/// - the parts an own generation installed: added over the generation it chained on (CONCLOAD: another loader's
///   parts, committed after the snapshot, sit between own generations), not a compaction output and not a segment's
///   seal, each with the newest own generation's record for it;
/// - the base's segments, plus each segment an own generation sealed that the base does not list live, as
///   `Sealing`: its heap rows (the reader's own among them) are read under the snapshot, not its sealed part;
/// - each part's row-ID ranges as its generation lists them, under the highest `EXT_ROWID` floor of them all.
///
/// Other transactions' parts, seals and compactions after the snapshot are not in it: their inputs stay on disk while
/// the snapshot holds the horizon (`ingest::keep_generation`), as for any older-snapshot reader.
fn compose(own: &[Arc<Generation>], base: Option<&Generation>, prevs: &[Option<Arc<Generation>>], hide: &BTreeSet<u32>) -> PgResult<Generation> {
    use pgrc_format::manifest::{encode_segments, ext_records, put_ext_record, SegState, EXT_SEGMENTS, PART_FLAG_COMPACTED};
    let head = &own[0];
    let index_of = |g: &Generation, part_no: u32| g.manifest.parts.binary_search_by_key(&part_no, |p| p.part_no).ok();
    let newest_own = |part_no: u32| own.iter().find_map(|o| index_of(o, part_no).map(|i| (&**o, i)));
    let mut picks: Vec<(&Generation, usize)> = Vec::new(); // alloc: Cold
    if let Some(b) = base {
        picks.extend(b.manifest.parts.iter().enumerate().filter(|(_, r)| !hide.contains(&r.part_no)).map(|(i, r)| newest_own(r.part_no).unwrap_or((b, i))));
    }
    let seal = |g: &Generation, part_no: u32| g.segments.iter().any(|s| s.part_no == part_no);
    let mut installed: BTreeSet<u32> = BTreeSet::new(); // alloc: Cold
    for (o, prev) in own.iter().zip(prevs) {
        let before = prev.as_ref().map_or(&[][..], |p| &p.manifest.parts[..]);
        for r in o.manifest.parts.iter().filter(|r| index_of_parts(before, r.part_no).is_none()) {
            if r.flags & PART_FLAG_COMPACTED == 0 && !seal(o, r.part_no) && !hide.contains(&r.part_no) {
                installed.insert(r.part_no);
            }
        }
    }
    picks.extend(installed.into_iter().filter(|&n| base.is_none_or(|b| index_of(b, n).is_none())).filter_map(newest_own));
    picks.sort_by_key(|(g, i)| g.manifest.parts[*i].part_no);
    let mut segs: Vec<SegmentRecord> = base.map_or_else(Vec::new, |b| b.segments.clone());
    for (o, prev) in own.iter().zip(prevs) {
        let before = prev.as_ref().map_or(&[][..], |p| &p.segments[..]);
        for r in o.segments.iter().filter(|r| r.state == SegState::Sealed) {
            let same = |x: &&SegmentRecord| x.part_no == r.part_no && x.relid == r.relid;
            // sealed before this own generation, or a segment the view lists already (live: read; sealed: in a part)
            if before.iter().filter(same).any(|x| x.state == SegState::Sealed) || segs.iter().any(|x| same(&x)) {
                continue;
            }
            // its slot's record in the base, if any, is an earlier segment sealed there (a live one would still hold
            // the horizon-kept slot)
            if segs.iter().any(|x| x.slot == r.slot && x.state != SegState::Sealed) {
                continue;
            }
            segs.retain(|x| x.slot != r.slot);
            segs.push(SegmentRecord { state: SegState::Sealing, ..*r });
        }
    }
    segs.sort_by_key(|r| r.slot);
    let format = |e| crate::read_error(pgrc_read::ReadError::Format(e));
    let mut ext = Vec::new();
    for (tag, bytes) in ext_records(base.map_or(&[][..], |b| &b.manifest.ext)).map_err(format)? {
        if ![EXT_SEGMENTS, pgrc_format::manifest::EXT_UNDER, pgrc_format::manifest::EXT_SCHEMAS, pgrc_format::manifest::EXT_RANGES, pgrc_format::manifest::EXT_ROWID].contains(&tag) {
            put_ext_record(&mut ext, tag, bytes);
        }
    }
    if !segs.is_empty() {
        encode_segments(&segs, &mut ext);
    }
    // IX0: each part's ranges as the generation it comes from lists them, and the highest floor of any of them
    let mut table: Vec<(u32, IdRanges)> = crate::alloc::try_vec(picks.len(), "catalog")?;
    for (g, i) in picks.iter().filter(|(g, i)| !g.ids_of(*i).is_empty()) {
        table.push((g.manifest.parts[*i].part_no, crate::alloc::try_to_vec(g.ids_of(*i), "catalog")?));
    }
    let floor = own.iter().map(|g| &**g).chain(base).map(|g| pgrc_format::manifest::next_row_id(&g.manifest)).collect::<Result<Vec<u64>, _>>().map_err(format)?.into_iter().max().unwrap_or(0);
    ext = pgrc_format::manifest::with_next_row_id(&pgrc_format::manifest::with_ranges(&ext, &table).map_err(format)?, floor).map_err(format)?;
    // FASTADD: the view's header is the newest own generation's schema; each part keeps the one its generation lists
    // it under (an own ALTER's view holds the base's parts under the old schema)
    let header_fp = head.manifest.header.schema_fingerprint;
    if picks.iter().any(|(g, _)| g.lagged || g.schema_fp != header_fp) {
        let mut fps_of: Vec<(*const Generation, Vec<u64>)> = Vec::new(); // alloc: Cold
        let mut parts_fp = crate::alloc::try_vec(picks.len(), "catalog")?; // alloc: Cold
        for (g, i) in &picks {
            let at = match fps_of.iter().position(|(p, _)| std::ptr::eq(*p, *g)) {
                Some(at) => at,
                None => {
                    fps_of.push((*g as *const Generation, pgrc_format::manifest::part_fingerprints(&g.manifest).map_err(format)?));
                    fps_of.len() - 1
                }
            };
            parts_fp.push((g.manifest.parts[*i].part_no, fps_of[at].1[*i]));
        }
        let mut oom = None;
        let entries = pgrc_format::manifest::entries_of(parts_fp, header_fp, &mut |f| crate::fastadd::schema_of(f).and_then(|c| crate::alloc::try_to_vec(&c[..], "catalog").map_err(|e| oom = Some(e)).ok()));
        if let Some(e) = oom {
            return Err(e);
        }
        let entries = entries.map_err(|_| crate::read_error(pgrc_read::ReadError::OpenMismatch { field: "manifest schema_fingerprint" }))?;
        ext = pgrc_format::manifest::with_schemas(&ext, &entries).map_err(format)?;
    }
    let mut header = head.manifest.header;
    header.part_count = picks.len() as u32;
    let records = picks.iter().map(|(g, i)| g.manifest.parts[*i]).collect(); // alloc: Exec charged context() (the view's held_bytes)
    // Detached generations (unit tests) have no open parts: the view has none either.
    let parts = picks
        .iter()
        .filter_map(|(g, i)| g.parts.get(*i))
        .cloned()
        .collect(); // alloc: Exec charged context() (the view's held_bytes)
    let mut view = Generation::linked(Manifest::new(header, records).with_ext(ext), head.prev_gen, parts, None, None)?;
    // SORTRUNS-E0: the view's runs are its base's (the reader's own publishes add parts, never runs)
    if let Some(b) = base {
        view.bind_runs_from(&b.runs, None);
    }
    view.into_view();
    view.stacked = hide.len() as u32;
    view.seal_hash();
    view.refit();
    Ok(view)
}

/// The plan path's lookup: [`visible`], and for a relation the table does not hold, one `stat`
/// of its directory — no directory and no main fork (a relation never written, from any lifetime) registers
/// the relation empty and serves `Empty`, no directory beside its main fork is 58P01 (P304); a directory that exists but was not loaded is the
/// error ([`not_loaded`]), never a walk. D4853: no directory but a v0.3.x segment file is refused, never registered.
pub fn resolve(rel: &types_rel::Relation<'_>, slot: RelSlot, dir: &str, check: &dyn CommitCheck) -> PgResult<Lookup> {
    drain_deferred_now()?;
    match visible(slot, check)? {
        Lookup::Unknown => {
            if crate::dirpath::dir_exists(dir)? {
                return Ok(Lookup::Unknown);
            }
            crate::refuse_v1_format(rel)?;
            // P304: the directory is made with the relfilenumber (`create_table_dir`), so its main fork without it is
            // damage (a removed directory), never an empty table: C's 58P01 for a relation file that is gone
            if crate::dirpath::main_fork_exists(rel.rd_locator.get(), rel.rd_backend) {
                return Err(Box::new( // alloc: Cold
                    PgError::error(format!("could not open directory \"{dir}\": No such file or directory")) // alloc: Cold
                        .with_sqlstate(types_error::ERRCODE_UNDEFINED_FILE),
                ));
            }
            create(slot, dir);
            Ok(Lookup::Empty)
        }
        found => Ok(found),
    }
}

/// A directory the load at server start refused: its error's sqlstate and message, and whether a CRC mismatch refused it.
struct LoadFailure {
    sqlstate: types_error::SqlState,
    message: String,
    crc: bool,
}

/// The directories the load at server start refused ([`note_load_failed`]), by directory path.
static LOAD_FAILED: pgsync::Mutex<BTreeMap<String, LoadFailure>> = pgsync::Mutex::new(BTreeMap::new());

/// The load at server start refused `dir` with `e` (`crc`: a CRC mismatch refused it). A statement that reads the
/// relation gets that error ([`not_loaded`]) instead of an internal one, and a CRC mismatch counts as a checksum failure
/// in the statement's database each time, as a heap page that fails verification does at each read.
pub fn note_load_failed(dir: &str, e: &PgError, crc: bool) {
    let f = LoadFailure { sqlstate: e.sqlstate(), message: e.message().to_string(), crc }; // alloc: Cold
    pgsync::lock(&LOAD_FAILED).insert(dir.to_string(), f); // alloc: Cold
}

/// The error for a relation the table does not hold. A directory the load at server start refused
/// ([`note_load_failed`]) reports that refusal; otherwise it is a bug (the load at server start or a
/// writer missed it), never a reason to walk the directory.
pub fn not_loaded(rel: &types_rel::Relation<'_>, dir: &str) -> Box<PgError> {
    let name = String::from_utf8_lossy(rel.rd_rel.relname.name_str()); // alloc: Cold
    if let Some(f) = pgsync::lock(&LOAD_FAILED).get(dir) {
        if f.crc {
            crate::report_checksum_failure();
        }
        return Box::new(PgError::error(format!("pgrcolumnar: relation \"{name}\" ({dir}) could not be loaded at server start: {}", f.message)).with_sqlstate(f.sqlstate)); // alloc: Cold
    }
    Box::new(PgError::error(format!("pgrcolumnar: relation \"{name}\" ({dir}) is not in the columnar catalog loaded at server start and kept by the writers"))) // alloc: Cold
}

/// The facts the plan rig installed for a relation without generations.
pub fn override_facts(slot: RelSlot) -> Option<Arc<FooterFacts>> {
    pgsync::lock(&CATALOG).rels.get(&slot).and_then(|e| e.override_facts.clone())
}

pub fn set_override_facts(slot: RelSlot, facts: Arc<FooterFacts>) {
    if let Some(e) = pgsync::lock(&CATALOG).rels.get_mut(&slot) {
        e.override_facts = Some(facts);
        e.refit();
    }
}

pub fn contains(slot: RelSlot) -> bool {
    pgsync::lock(&CATALOG).rels.contains_key(&slot)
}

/// The generations of `slot` (ascending) and its head.
pub fn generations(slot: RelSlot) -> Option<(Vec<Arc<Generation>>, u64)> {
    pgsync::lock(&CATALOG).rels.get(&slot).map(|e| (e.gens.clone(), e.head))
}

/// Every relation slot the catalog holds (the pre-warm's list).
pub fn slots() -> Vec<RelSlot> {
    pgsync::lock(&CATALOG).rels.keys().copied().collect() // alloc: Cold
}

/// The head generation number of `slot` (0 = registered, nothing published).
pub fn head(slot: RelSlot) -> Option<u64> {
    pgsync::lock(&CATALOG).rels.get(&slot).map(|e| e.head)
}

/// A directory created this lifetime (the first ingest's `mkdir`): an entry with no generation.
pub fn create(slot: RelSlot, dir: &str) {
    let mut c = pgsync::lock(&CATALOG);
    c.rels.entry(slot).or_insert_with(|| RelEntry::new(dir)); // alloc: Cold charged OWNER (RelEntry::held_bytes)
    c.dirs.insert(dir.to_string(), slot); // alloc: Cold charged OWNER (RelEntry::held_bytes)
    drop(c);
    fire(&SlotEvent::Created(slot, dir));
}

/// A directory removed (DROP / TRUNCATE / a rewrite's old locator / an aborted creation). Its parts leave the shared
/// part registry ([`forget_parts`]).
pub fn remove_dir(dir: &str) {
    let mut c = pgsync::lock(&CATALOG);
    let gone = c.dirs.remove(dir).map(|slot| {
        c.fold_vintage.remove(&slot);
        c.relids.retain(|_, s| *s != slot);
        (slot, c.rels.remove(&slot))
    });
    drop(c);
    if let Some((slot, entry)) = gone {
        crate::l0::forget(slot);
        forget_views(slot);
        run_removed_hook(&|s| s == slot);
        let parts = entry.map_or_else(Vec::new, |e| parts_only_in(&e.gens, &[]));
        forget_relid_keys(&parts);
        forget_parts(parts);
        fire(&SlotEvent::Removed(slot, dir));
    }
}

/// Every entry whose directory was under a database directory just removed (DROP DATABASE, a failed or moved copy).
pub fn remove_under(db_path: &str) {
    let gone: Vec<(String, RelSlot)> = { let p = format!("{db_path}/"); pgsync::lock(&CATALOG).dirs.iter().filter(|(d, _)| d.starts_with(&p)).map(|(d, s)| (d.clone(), *s)).collect() }; // alloc: Cold
    let n = drop_under(db_path);
    crate::rowid_alloc::forget_under(db_path);
    gone.iter().for_each(|(d, s)| fire(&SlotEvent::Removed(*s, d)));
    if n > 0 {
        log(false, format!("pgrcolumnar: {n} catalog entries dropped under {db_path}")); // alloc: Cold
    }
}

/// The entries under `db_path` leave the catalog, and their parts the shared part registry.
fn drop_under(db_path: &str) -> usize {
    let prefix = format!("{db_path}/"); // alloc: Cold
    let mut c = pgsync::lock(&CATALOG);
    c.used.retain(|d, _| !d.starts_with(&prefix));
    c.used_bytes = c.used.keys().map(|d| used_entry_bytes(d)).sum();
    refit_cache(&USED_CHARGE, c.used_bytes);
    let n = c.dirs.keys().filter(|d| d.starts_with(&prefix)).count();
    let (mut gone, mut entries, mut slots) = (Vec::new(), Vec::new(), Vec::new());
    if gone.try_reserve_exact(n).is_err() || entries.try_reserve_exact(n).is_err() || slots.try_reserve_exact(n).is_err() {
        drop(c);
        return drop_under_each(db_path, &prefix);
    }
    gone.extend(c.dirs.keys().filter(|d| d.starts_with(&prefix)).cloned());
    for d in &gone {
        if let Some(slot) = c.dirs.remove(d) {
            entries.extend(c.rels.remove(&slot));
            c.fold_vintage.remove(&slot);
            forget_views(slot);
            slots.push(slot);
        }
    }
    c.relids.retain(|_, s| !slots.contains(s));
    drop(c);
    crate::l0::forget_where(|s| slots.contains(s));
    run_removed_hook(&|s| slots.contains(&s));
    let parts: Vec<Arc<OpenPart>> = entries.iter().flat_map(|e| parts_only_in(&e.gens, &[])).collect(); // alloc: Cold
    forget_relid_keys(&parts);
    forget_parts(parts);
    // D4581: the publish mutexes of the dropped entries' numbers (a directory created and written this lifetime is in no other map)
    crate::inval::forget_dirs_under(db_path, slots.iter().map(|s| s.2));
    gone.len()
}

/// [`drop_under`] one directory per lock, when its buffers for the whole database cannot be reserved.
fn drop_under_each(db_path: &str, prefix: &str) -> usize {
    let mut n = 0;
    loop {
        let mut c = pgsync::lock(&CATALOG);
        let Some(d) = c.dirs.range::<str, _>((std::ops::Bound::Included(prefix), std::ops::Bound::Unbounded)).next().filter(|(d, _)| d.starts_with(prefix)).map(|(d, _)| d.clone()) else { break };
        let Some(slot) = c.dirs.remove(&d) else { break };
        let entry = c.rels.remove(&slot);
        c.fold_vintage.remove(&slot);
        forget_views(slot);
        c.relids.retain(|_, s| *s != slot);
        drop(c);
        n += 1;
        crate::l0::forget(slot);
        run_removed_hook(&|s| s == slot);
        let parts = entry.map_or_else(Vec::new, |e| parts_only_in(&e.gens, &[]));
        forget_relid_keys(&parts);
        forget_parts(parts);
        crate::inval::forget_dirs_under(db_path, std::iter::once(slot.2));
    }
    n
}

/// The parts `removed` name that no generation of `kept` names (by file identity, the registry's key).
/// K1-RECLAIM: each removed generation's parts are merged by number against each kept generation's (a generation's
/// parts are in manifest order, ascending by `part_no`), not looked up in a set of every kept part. Every publish
/// built that set, with nothing removed too: ~30 ms per publish at 10,000 parts (K1-MEM / GENID, after P-1's bars).
/// A generation whose open parts do not follow its manifest (none today) takes the set.
fn parts_only_in(removed: &[Arc<Generation>], kept: &[Arc<Generation>]) -> Vec<Arc<OpenPart>> {
    let key = |p: &Arc<OpenPart>| {
        let i = p.ident();
        (i.dev, i.ino, i.len)
    };
    let aligned = |g: &Generation| g.parts.len() == g.manifest.parts.len();
    if !removed.iter().chain(kept).all(|g| aligned(g)) {
        let kept: BTreeSet<(u64, u64, u64)> = kept.iter().flat_map(|g| g.parts.iter().map(|p| key(&p.part))).collect();
        return removed.iter().flat_map(|g| g.parts.iter()).filter(|p| !kept.contains(&key(&p.part))).map(|p| Arc::clone(&p.part)).collect();
    }
    let mut out = Vec::new();
    for g in removed {
        let mut held = Vec::new();
        if held.try_reserve_exact(g.parts.len()).is_ok() {
            held.resize(g.parts.len(), false);
            for k in kept {
                let mut j = 0;
                for (i, r) in g.manifest.parts.iter().enumerate() {
                    while j < k.manifest.parts.len() && k.manifest.parts[j].part_no < r.part_no {
                        j += 1;
                    }
                    if !held[i] && j < k.manifest.parts.len() && k.manifest.parts[j].part_no == r.part_no {
                        let (a, b) = (&g.parts[i].part, &k.parts[j].part);
                        held[i] = Arc::ptr_eq(a, b) || key(a) == key(b);
                    }
                }
            }
            out.extend(g.parts.iter().zip(held).filter(|(_, h)| !h).map(|(p, _)| Arc::clone(&p.part)));
            continue;
        }
        for (i, r) in g.manifest.parts.iter().enumerate() {
            let held = kept.iter().any(|k| {
                let j = k.manifest.parts.partition_point(|p| p.part_no < r.part_no);
                j < k.manifest.parts.len() && k.manifest.parts[j].part_no == r.part_no && {
                    let (a, b) = (&g.parts[i].part, &k.parts[j].part);
                    Arc::ptr_eq(a, b) || key(a) == key(b)
                }
            });
            if !held {
                out.push(Arc::clone(&g.parts[i].part));
            }
        }
    }
    out
}

/// `parts` left the catalog for good (their generations retired or rolled back, their directory removed): they leave
/// the shared part registry too. Such a part's file is unlinked (or about to be), so no open hits its entry again, but
/// the entry would hold the fd, and with it the file's disk space, until the janitor evicted it, which an entry with
/// nothing resident only meets at the entry cap; and a later file that reuses the inode at the same length would be
/// served the stale part. A reader still holding a part keeps its `Arc`, and so the fd, until it lets go; an entry a
/// later open put under the same key is another part and stays. Called without the catalog lock.
fn forget_parts(parts: Vec<Arc<OpenPart>>) {
    if parts.is_empty() {
        return;
    }
    let reg = crate::inval::registry();
    for p in &parts {
        reg.forget(p);
    }
}

/// D4581: the removed relations' part keys leave the relid map (DROP DATABASE sends no relcache message for its
/// relations); a relid with none left goes. Called without the catalog lock, before the parts leave the registry.
fn forget_relid_keys(parts: &[Arc<OpenPart>]) {
    if parts.is_empty() {
        return;
    }
    let keys: BTreeSet<(u64, u64, u64)> = parts // alloc: Cold
        .iter()
        .map(|p| {
            let id = p.ident();
            (id.dev, id.ino, id.len)
        })
        .collect();
    crate::inval::forget_part_keys(&|k| keys.contains(k));
}

/// D4570 (trunk, ported at r15): the hook a removal of relation entries runs after the catalog's lock, before their parts
/// leave the registry: its owner lets go what it keeps for each removed relation slot (`removed(slot)`; Gravity: the
/// per-generation part lists and, for the relations they were built for, the parked sources' kept cursors). It runs where
/// a relation's storage goes ([`remove_dir`]: DROP TABLE, TRUNCATE, a rewrite's old locator; [`drop_under`]: DROP DATABASE).
static REMOVED_HOOK: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub fn set_removed_hook(f: fn(&dyn Fn(RelSlot) -> bool)) {
    REMOVED_HOOK.store(f as usize, std::sync::atomic::Ordering::Release);
}

fn run_removed_hook(removed: &dyn Fn(RelSlot) -> bool) {
    let p = REMOVED_HOOK.load(std::sync::atomic::Ordering::Acquire);
    if p != 0 {
        // SAFETY: only set_removed_hook stores here, always from a fn(&dyn Fn(RelSlot) -> bool).
        let f: fn(&dyn Fn(RelSlot) -> bool) = unsafe { std::mem::transmute::<usize, fn(&dyn Fn(RelSlot) -> bool)>(p) };
        f(removed);
    }
}

/// Record `relid` → `slot` (every ask); the relcache callback's currency.
pub fn record_relid(relid: Oid, slot: RelSlot) {
    pgsync::lock(&CATALOG).relids.insert(relid, slot); // alloc: Cold charged OWNER (its relation's RelEntry::held_bytes)
}

/// The recorded `(slot, relid)` pairs, by slot: the compaction shadow's sweep names a slot's table from them, with no
/// catalog read (SCANSTATS S4).
pub fn relids_by_slot() -> Vec<(RelSlot, Oid)> {
    let mut v: Vec<(RelSlot, Oid)> = pgsync::lock(&CATALOG).relids.iter().map(|(&r, &s)| (s, r)).collect(); // alloc: Cold
    v.sort_unstable();
    v
}

/// The relcache-invalidation face: hygiene only (the relid map, the fold vintage). The entries
/// themselves live and die with their directories.
pub fn invalidate_relid(relid: Oid) {
    fire(&SlotEvent::Relid(relid));
    let mut c = pgsync::lock(&CATALOG);
    if relid == 0 {
        c.relids.clear();
        return;
    }
    if let Some(slot) = c.relids.remove(&relid) {
        c.fold_vintage.remove(&slot);
    }
}

/// D3110 (b): every part a held generation keeps open, each passed to `f` under the catalog lock (a part carried by several
/// generations comes once per generation). A relcache invalidation (VACUUM's relstats update,
/// `inval::invalidate_relid`) takes a relation's keys out of the shared part registry while its generations keep these
/// parts, and the scans read the generations' instances: the registry's content trim walks them too
/// (`PartRegistry::set_held_parts`).
pub fn held_parts(f: &mut dyn FnMut(&Arc<OpenPart>)) {
    let c = pgsync::lock(&CATALOG);
    c.rels.values().flat_map(|e| e.gens.iter()).flat_map(|g| g.parts.iter()).for_each(|p| f(&p.part));
}

pub fn record_fold_vintage(slot: RelSlot) {
    pgsync::lock(&CATALOG).fold_vintage.insert(slot); // alloc: Cold charged OWNER (its relation's RelEntry::held_bytes)
}

pub fn is_fold_vintage(slot: RelSlot) -> bool {
    pgsync::lock(&CATALOG).fold_vintage.contains(&slot)
}

/// For each record of `m` (manifest order: `part_no` ascending), the position of `carried`'s record of the same part
/// file (`part_no`, `file_len` and `footer_off` equal), by one merge join: `steps` counts its comparisons, at most
/// `m.len() + carried.len()` (P-1 L2; a linear find per part was quadratic in the part count).
fn try_carry_index(m: &[PartRecord], carried: &[PartRecord], steps: &mut u64) -> PgResult<Vec<Option<usize>>> {
    let mut out = crate::alloc::try_vec(m.len(), "catalog")?; // alloc: Cold
    let mut j = 0;
    for rec in m {
        while j < carried.len() && carried[j].part_no < rec.part_no {
            j += 1;
            *steps += 1;
        }
        *steps += 1;
        let same = carried.get(j).is_some_and(|c| c.part_no == rec.part_no && c.file_len == rec.file_len && c.footer_off == rec.footer_off);
        out.push(same.then_some(j));
    }
    Ok(out)
}

#[cfg(test)]
fn carry_index(m: &[PartRecord], carried: &[PartRecord], steps: &mut u64) -> Vec<Option<usize>> {
    try_carry_index(m, carried, steps).expect("carry index")
}

/// How many records ahead [`open_parts`] hints a carried part's line.
const PREFETCH_AHEAD: usize = 16;

/// A hint that the line holding `p`'s `Arc` counts (just before `p`) is written soon. Never faults, changes nothing.
#[inline(always)]
fn prefetch_write<T>(p: *const T) {
    let line = (p as *const u8).wrapping_sub(16);
    #[cfg(target_arch = "aarch64")]
    // SAFETY: a prefetch hint: no access is made, a bad address is ignored, and no register or flag changes.
    unsafe {
        core::arch::asm!("prfm pstl1keep, [{0}]", in(reg) line, options(nostack, preserves_flags, readonly));
    }
    #[cfg(target_arch = "x86_64")]
    // SAFETY: as above.
    unsafe {
        core::arch::x86_64::_mm_prefetch::<{ core::arch::x86_64::_MM_HINT_T0 }>(line as *const i8);
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    let _ = line;
}

/// Hint the tail (StreamDir, section table, footer) of every part the opens will read, so a cold load of a many-part
/// generation (a standby's publish redo, a restart) reads them in parallel instead of a round trip per read.
fn prefetch_tails(dir: &str, m: &Manifest, carry: &[Option<usize>]) {
    const TAIL: u64 = 64 << 10;
    for (rec, _) in m.parts.iter().zip(carry).filter(|(_, c)| c.is_none()) {
        let Ok(p) = CString::new(format!("{dir}/{}", part_file_name(rec.part_no))) else { continue }; // alloc: Cold
        let fd = vfs::open(&p, libc::O_RDONLY | libc::O_CLOEXEC, 0);
        if fd >= 0 {
            let off = rec.footer_off.saturating_sub(TAIL);
            let _ = vfs::fadvise_willneed(fd, off as libc::off_t, rec.file_len.saturating_sub(off) as libc::off_t);
            vfs::close(fd);
        }
    }
}

/// Open every part of `m` through the registry, with its delete vector. The parts `carried` (the generation this one
/// built on) holds are reused by `part_no`, and so is a part's delete vector when its DV triplet is unchanged; only
/// the new parts are opened and only the changed DVs read.
///
/// GENID (D1133): a part is carried only from the generation `m` names as its `prev_gen`, and only below that
/// generation's `next_part_no`; any other match opens the part again. A part at or above the base's counter was
/// installed after the base, whatever its record says: by this publish, or (before GENID's numbering) by a
/// rolled-back publish whose part number and length this one repeated, whose open file would have been carried.
/// `verify` also checks that each carried part's name still names the file held open (dev, ino and size by one
/// `stat`; a name `stat` cannot see was unlinked, not replaced, and is no mismatch); a mismatch opens it again. That is
/// a `stat` per carried part: the cold load checks, a publish and its redo only in debug builds ([`VERIFY_CARRIES`]).
pub fn open_parts(slot: RelSlot, dir: &str, m: &Manifest, carried: Option<&Generation>, verify: bool) -> PgResult<Vec<OpenedPart>> {
    let reg = crate::inval::registry();
    // FASTADD: each part is opened against the schema it was written with (its footer's), the header's or an older one
    let fps = match m.header.flags & pgrc_format::manifest::MANIFESTF_SCHEMAS {
        0 => None,
        _ => Some(pgrc_format::manifest::part_fingerprints(m).map_err(|e| crate::read_error(pgrc_read::ReadError::Format(e)))?),
    };
    let fp_of = |k: usize| fps.as_ref().map_or(m.header.schema_fingerprint, |f| f[k]);
    let carried = carried.filter(|c| c.gen == m.header.prev_gen && m.header.prev_gen != 0);
    let bound = carried.map_or(0, |c| c.manifest.header.next_part_no);
    let old = carried.map_or(&[][..], |c| &c.manifest.parts[..]);
    let carry = try_carry_index(&m.parts, old, &mut 0)?;
    prefetch_tails(dir, m, &carry);
    let mut out = crate::alloc::try_vec(m.parts.len(), "catalog")?; // alloc: Cold
    let mut fresh = Vec::new(); // alloc: Cold
    let held = carried.map_or(&[][..], |c| &c.parts[..]);
    for (k, (rec, &at)) in m.parts.iter().zip(&carry).enumerate() {
        // the carried parts' counts are scattered lines: hint the one a few records ahead (P-1 L4a)
        if let Some(p) = carry.get(k + PREFETCH_AHEAD).copied().flatten().and_then(|j| held.get(j)) {
            prefetch_write(Arc::as_ptr(&p.0));
        }
        let expect = PartExpect::from_manifest(rec, fp_of(k), slot.2);
        let base_part = at.filter(|_| rec.part_no < bound).and_then(|j| Some((held.get(j)?, &old[j])));
        if let Some((c, was)) = base_part.filter(|(c, _)| !verify || names_same_file(dir, c)) {
            // its record (the DV triplet included) and the schema unchanged, in the same slot (`base_of`): the carried
            // part as it is, its expectations and delete vector equal
            if rec == was && c.expect.schema_fingerprint == Some(fp_of(k)) {
                out.push(c.clone()); // alloc: Cold charged OWNER (the generation built from them: Generation::held_bytes)
                continue;
            }
            let deletes = match (rec.dv_gen, rec.dv_len, rec.dv_crc) {
                (0, ..) => None,
                dv if dv == (was.dv_gen, was.dv_len, was.dv_crc) => c.deletes.clone(),
                _ => crate::scan::load_part_deletes(dir, rec)?,
            };
            out.push(c.with(expect, deletes)); // alloc: Cold charged OWNER (the generation built from them: Generation::held_bytes)
            continue;
        }
        let path = format!("{dir}/{}", part_file_name(rec.part_no)); // alloc: Cold
        let cpath = CString::new(path.clone()).map_err(|_| nul_path(&path))?; // alloc: Cold charged OWNER (the generation built from them: Generation::held_bytes)
        let pin = reg.open_pinned(&expect, || Ok(Box::new(VfsPartIo::open(&cpath)?) as Box<dyn pgrc_read::PartIo>)).map_err(crate::read_error)?;
        let part = Arc::clone(pin.part());
        fresh.push(out.len());
        out.push(OpenedPart::new(OpenedPartData { part, pin, path: cpath, expect, deletes: crate::scan::load_part_deletes(dir, rec)?, row_map: Arc::default() })); // alloc: Cold charged OWNER (the generation built from them: Generation::held_bytes)
    }
    warm_stream_directories(fresh.iter().map(|&i| &out[i]));
    Ok(out)
}

/// GENID: carried parts are checked by `stat` on a publish and its redo in debug builds; a load always checks
/// ([`open_parts`]).
pub const VERIFY_CARRIES: bool = cfg!(debug_assertions);

/// GENID (D1133): `c`'s path still names the file `c` holds open: the same device, inode and size. A name `stat`
/// cannot see (unlinked, never replaced) is not a mismatch; a mismatch is logged, and the caller opens the part again.
fn names_same_file(dir: &str, c: &OpenedPart) -> bool {
    let mut st = vfs::FileInfo::zeroed();
    if vfs::stat(&c.path, &mut st) != 0 {
        return true;
    }
    let id = c.part.ident();
    let same = (st.dev, st.ino, st.size as u64) == (id.dev, id.ino, id.len);
    if !same {
        log(true, format!("pgrcolumnar: {} of {dir} is not the file the catalog held open (dev/ino/size {}/{}/{} vs {}/{}/{}); opened again", c.path.to_string_lossy(), st.dev, st.ino, st.size, id.dev, id.ino, id.len)); // alloc: Cold
    }
    same
}

#[cold]
fn nul_path(path: &str) -> Box<PgError> {
    Box::<PgError>::new(PgError::error(format!("pgrcolumnar: part path contains NUL: {path}"))) // alloc: Cold
}

/// Every open part's StreamDir parsed at open — metadata, like the footer and the section table
/// the open itself reads (ClickHouse loads part metadata at startup the same way). Measured on
/// a wide table before this: a statement's first touch of a part faulted the 17–26 KB section at the
/// file's tail before its first lane read — one of the three cold preads per part (one query: 413 of
/// 1,239, 51 % of the summed read time). Hint every section first so the kernel reads them in
/// parallel, then parse; a directory the open cannot parse is reported typed by the first read
/// that needs it, as before.
fn warm_stream_directories<'a>(parts: impl Iterator<Item = &'a OpenedPart> + Clone) {
    for op in parts.clone() {
        op.part.advise_stream_directory();
    }
    for op in parts {
        let _ = op.part.stream_directory();
    }
}

/// The generation a publish built on (`prev_gen` of its manifest), whose open parts it carries. Never the newest
/// held one: after a rolled-back publish that is the dead generation. (Before GENID the next publish took the dead
/// generation's number and part numbers, so a same-sized part would have carried the dead part's unlinked file; a
/// primary restarted since the rollback still can, and redo meets it.)
fn base_of(gens: &[Arc<Generation>], prev_gen: u64) -> Option<Arc<Generation>> {
    if prev_gen == 0 {
        return None;
    }
    gens.iter().find(|g| g.gen == prev_gen).cloned()
}

/// A publish (this process): the new generation appended and made the head, its parts open (carried from the
/// generation it built on: only the new ones are opened), its stats as the publisher planned them (P-1: its answers
/// index built now, which checks the stats cover the manifest; its image assembled on the first ask). Nothing is read
/// or written. Stats that do not cover the manifest are a bug guard, not a path: logged, and the generation is
/// completed from its parts in memory. `fold` is the facts fold over a generation's open parts.
pub fn publish(slot: RelSlot, dir: &str, manifest: Manifest, stats: StatsState, fold: &dyn Fn(&Generation) -> PgResult<FactsImage>) -> PgResult<Arc<Generation>> {
    let carried = { pgsync::lock(&CATALOG).rels.get(&slot).and_then(|e| base_of(&e.gens, manifest.header.prev_gen)) };
    let parts = open_parts(slot, dir, &manifest, carried.as_deref(), VERIFY_CARRIES)?;
    let h = manifest.header;
    let mut gen = Generation::new(manifest, parts, None, None)?;
    gen.carry_from(carried.as_deref());
    gen.bind_runs(dir, carried.as_deref());
    let stats = Arc::new(stats);
    match Answers::from_state(Arc::clone(&stats), &gen.manifest) {
        Ok(a) => {
            gen.stats = Some(stats);
            gen.set_answers(Arc::new(a));
        }
        Err(pgrc_write::stats::OrderError::Alloc(e)) => return Err(crate::format_error(e)),
        Err(why) => {
            let t0 = std::time::Instant::now();
            complete(&gen, fold)?;
            log(false, format!("pgrcolumnar: generation {} of {dir} completed in-process (the publisher's stats refused: {why}): {} parts walked, {} us", h.gen, gen.parts.len(), t0.elapsed().as_micros())); // alloc: Cold
        }
    }
    let gen = Arc::new(gen); // alloc: Cold charged OWNER (Generation::held_bytes)
    let mut c = pgsync::lock(&CATALOG);
    note_used(&mut c, dir, h.gen, h.next_part_no);
    let e = c.rels.entry(slot).or_insert_with(|| RelEntry::new(dir)); // alloc: Cold charged OWNER (RelEntry::held_bytes)
    // GENID: a generation above the base is a rolled-back publish's (the publisher's base walk passed it; a same-number
    // one is among them): it goes, and the parts only it named leave the registry (GENID-2: and are reclaimed)
    let replaced = e.drop_dead(h.prev_gen, &gen.manifest);
    debug_assert!(!e.gens.iter().any(|g| g.gen == h.gen));
    e.gens.push(Arc::clone(&gen)); // alloc: Cold charged OWNER (RelEntry::held_bytes)
    e.gens.sort_unstable_by_key(|g| g.gen); // numbers are unique in `gens` (`drop_dead` took the same number out)
    e.head = h.gen;
    drop_views(slot);
    let gone = parts_only_in(&replaced, &e.gens);
    e.refit();
    c.dirs.insert(dir.to_string(), slot); // alloc: Cold charged OWNER (RelEntry::held_bytes)
    drop(c);
    forget_parts(gone);
    fire(&SlotEvent::Published(slot, h.gen));
    Ok(gen)
}

/// GENID-2: the part numbers of `slot`'s rolled-back generations whose files the next reclaim unlinks (ascending).
pub fn dead_parts(slot: RelSlot) -> Vec<u32> {
    pgsync::lock(&CATALOG).rels.get(&slot).map_or_else(Vec::new, |e| e.dead_parts.iter().copied().collect())
}

/// GENID-2: dead parts a reclaim unlinked (or found gone) leave `slot`'s list.
pub fn forget_dead_parts(slot: RelSlot, parts: &[u32]) {
    if let Some(e) = pgsync::lock(&CATALOG).rels.get_mut(&slot) {
        parts.iter().for_each(|p| {
            e.dead_parts.remove(p);
        });
        e.refit();
    }
}

/// K1-RECLAIM: `slot`'s next reclaim lists its directory (see [`SweepMemo`]), and the parts earlier incremental
/// reclaims unlinked whose optional sidecars it unlinks, ascending. The reclaim's plan drops any a held generation from
/// its keep on names (`ingest::plan_from`), outside `CATALOG`: RECLAIMDEFER's probe measured that check here, every
/// pending part against every held generation, at ~115 ms per call at ~4k pending parts and ~2-3k generations, twice
/// per sweeping reclaim, the second time under the table's publish lock too.
pub fn sweep_due(slot: RelSlot) -> Option<Vec<u32>> {
    let c = pgsync::lock(&CATALOG);
    let s = &c.rels.get(&slot)?.sweep;
    (s.due.is_some() || s.since >= SWEEP_EVERY || s.pending.len() >= PENDING_MAX).then(|| s.pending.iter().copied().collect())
}

/// K1-RECLAIM: the rolled-back generations' own files that wait for a reclaim (see [`SweepMemo`]).
pub fn dead_files(slot: RelSlot) -> Vec<String> {
    pgsync::lock(&CATALOG).rels.get(&slot).map_or_else(Vec::new, |e| e.sweep.dead.iter().map(|d| d.1.clone()).collect())
}

/// K1-RECLAIM: `slot`'s reclaim keeping `keep` ran; `full`: it listed the directory (nothing waits any more), else
/// `retired` = the parts it unlinked, whose optional sidecars wait for the next sweep. The dead generations' files
/// below `keep` were judged.
pub fn note_reclaim(slot: RelSlot, keep: u64, full: bool, retired: &mut dyn Iterator<Item = u32>) {
    if let Some(e) = pgsync::lock(&CATALOG).rels.get_mut(&slot) {
        let s = &mut e.sweep;
        s.dead.retain(|d| d.0 >= keep);
        if full {
            s.since = 0;
            s.pending.clear();
            s.due = s.due.filter(|&d| keep <= d);
        } else {
            s.since = s.since.saturating_add(1);
            s.pending.extend(retired); // alloc: Cold charged OWNER (RelEntry::held_bytes, refit below)
        }
        e.refit();
    }
}

/// GENID-2: the part files among `names` (a replayed `RECLAIM`'s unlinks) leave `slot`'s dead parts.
pub fn forget_dead_names(slot: RelSlot, names: &[String]) {
    use pgrc_format::dirlayout::{classify, FileClass};
    let parts: Vec<u32> = names.iter().filter_map(|n| match classify(n) {
        Some(FileClass::Part(pn)) => Some(pn),
        _ => None,
    }).collect();
    if !parts.is_empty() {
        forget_dead_parts(slot, &parts);
    }
}

/// Generations below `keep_from` leave the table (their files were reclaimed), and the parts only they name leave the
/// shared part registry. Such a part's file is unlinked, so no open hits its entry again, but the entry holds the fd,
/// and with it the file's disk space, until the janitor evicts it, which for an entry nothing decoded (resident 0,
/// every compaction input read through a private cursor) only the entry cap does. A reader still holding the part
/// keeps its `Arc` and so the fd; no snapshot can resolve these generations any more (`keep_from` is past the
/// horizon), and a part a kept generation carries stays. Returns how many generations left: [`take_below`], then
/// [`Retirement::finish`].
pub fn retire_below(slot: RelSlot, keep_from: u64) -> usize {
    let r = take_below(slot, keep_from);
    let n = r.len();
    r.finish();
    n
}

/// RECLAIMDEFER (i′): the generations below `keep_from` out of `slot`'s entry, under `CATALOG` (every catalog lookup of
/// every table waits on it) and nothing else: no part-registry walk, no drop of a generation (each holds its parsed
/// manifest: the 8-client probe's retirements held `CATALOG` and the publish lock 124-425 ms at 1k-5k parts). A
/// reclaim takes them under its table's publish lock, in the hold it planned its names in, so a later plan never
/// sees them; [`Retirement::finish`] does the rest with no lock held. The hold is counted ([`crate::holds`]).
pub fn take_below(slot: RelSlot, keep_from: u64) -> Retirement {
    let mut c = pgsync::lock(&CATALOG);
    let t0 = pg_clock::MonoStamp::now();
    let (retired, kept) = match c.rels.get_mut(&slot) {
        Some(e) => {
            let (retired, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut e.gens).into_iter().partition(|g| g.gen < keep_from);
            e.gens = kept;
            e.refit();
            let kept = if e.gens.is_empty() || retired.is_empty() { Vec::new() } else { e.gens.clone() };
            (retired, kept)
        }
        None => (Vec::new(), Vec::new()),
    };
    crate::holds::catalog_held(t0.elapsed_ns());
    drop(c);
    Retirement { slot, keep_from, retired, kept }
}

/// RECLAIMDEFER (i′): generations [`take_below`] took out of the catalog, with the ones it kept then.
#[must_use = "a retirement is finished (its parts leave the registry, its listeners hear of it)"]
pub struct Retirement {
    slot: RelSlot,
    keep_from: u64,
    retired: Vec<Arc<Generation>>,
    kept: Vec<Arc<Generation>>,
}

impl Retirement {
    pub fn len(&self) -> usize {
        self.retired.len()
    }

    pub fn is_empty(&self) -> bool {
        self.retired.is_empty()
    }

    /// The retired generations' numbers, ascending.
    pub fn gens(&self) -> Vec<u64> {
        self.retired.iter().map(|g| g.gen).collect()
    }

    /// With no lock held: the parts only the retired generations name (against the generations kept when they were
    /// taken: a part a later publish names is a new part, carried only from what was kept) leave the shared part
    /// registry, the slot's listeners hear `Retired`, and the generations go (the catalog held their last reference,
    /// unless a reader still holds one).
    pub fn finish(self) {
        let Retirement { slot, keep_from, retired, kept } = self;
        forget_parts(parts_only_in(&retired, &kept));
        drop(kept);
        fire(&SlotEvent::Retired(slot, keep_from));
        drop(retired);
    }
}

/// RECLAIMDEFER (i′) forced-mismatch injection (the hint e2e, through [`crate::ingest::HINT_STALE_POINT`]): the newest
/// generation of `slot` below `keep_from` is replaced in the entry by a twin of itself (the same manifest, parts and
/// stats under another `Arc`), as a change of a below generation between a reclaim's hint and its publish lock would.
/// What a plan names from the catalog is unchanged; the hint's identity check is not. False when there is none.
pub fn perturb_below(slot: RelSlot, keep_from: u64) -> PgResult<bool> {
    let old = {
        let c = pgsync::lock(&CATALOG);
        let Some(e) = c.rels.get(&slot) else { return Ok(false) };
        let Some(g) = e.gens.iter().rev().find(|g| g.gen < keep_from) else { return Ok(false) };
        Arc::clone(g)
    };
    let twin = Arc::new(old.twin()?); // alloc: Cold
    let mut c = pgsync::lock(&CATALOG);
    let Some(e) = c.rels.get_mut(&slot) else { return Ok(false) };
    let Some(at) = e.gens.iter().position(|g| Arc::ptr_eq(g, &old)) else { return Ok(false) };
    e.gens[at] = twin;
    drop(c);
    drop(old);
    Ok(true)
}

/// RECLAIMDEFER (i′): a reclaim that took its generations out of the catalog failed before its unlinks: the next
/// reclaim of `slot` sweeps (lists the directory), as one keeping `keep` must; no held generation names those files any
/// more.
pub fn sweep_after(slot: RelSlot, keep: u64) {
    if let Some(e) = pgsync::lock(&CATALOG).rels.get_mut(&slot) {
        let d = keep.saturating_sub(1);
        e.sweep.due = Some(e.sweep.due.map_or(d, |x| x.max(d)));
    }
}

pgsync::process_global! {
    /// K1 L5 (OC-7 a): retirements a standby's redo deferred until its horizon passes `xid`, with the record's unlinks.
    static DEFERRED: pgsync::Mutex<Vec<Deferred>> = pgsync::Mutex::new(Vec::new());
}

struct Deferred {
    slot: RelSlot,
    keep_from: u64,
    xid: u32,
    dir: String,
    names: Vec<String>,
}

/// The queue is not empty (the readers' check: one relaxed load).
static DEFERRED_ANY: AtomicBool = AtomicBool::new(false);

/// K1 L5: a live standby's redo of a `RECLAIM` that retires without a conflict queues the retirement and the record's
/// unlinks; no reader is cancelled. A reader of an older generation reopens its parts by path (the scan pins through
/// the part registry), so the files stay until [`drain_deferred`] finds the standby's horizon past `xid`. A standby
/// restarted before that leaves them to end-of-recovery cleanup (none of them is in an effective set).
pub fn defer_retire(slot: RelSlot, keep_from: u64, xid: u32, dir: String, names: Vec<String>) {
    pgsync::lock(&DEFERRED).push(Deferred { slot, keep_from, xid, dir, names });
    DEFERRED_ANY.store(true, Ordering::Release);
}

/// The deferred retirements whose `xid` precedes `horizon` run: [`retire_below`], then the unlinks (a missing file is
/// fine). No snapshot on this server can still resolve a generation below their `keep_from`. FREEZEKEEP: in
/// `TransactionIdPrecedes`'s order, so a frozen keep's retirement (xid 0: every snapshot sees it) runs at once.
pub fn drain_deferred(horizon: u32) {
    if !DEFERRED_ANY.load(Ordering::Relaxed) {
        return;
    }
    let precedes = |x: u32| types_core::xact::TransactionIdPrecedes(x, horizon);
    let due: Vec<Deferred> = {
        let mut q = pgsync::lock(&DEFERRED);
        let (due, held): (Vec<_>, Vec<_>) = q.drain(..).partition(|e| precedes(e.xid));
        *q = held;
        DEFERRED_ANY.store(!q.is_empty(), Ordering::Release);
        due
    };
    for d in due {
        retire_below(d.slot, d.keep_from);
        forget_dead_names(d.slot, &d.names);
        for n in &d.names {
            let Ok(p) = CString::new(format!("{}/{n}", d.dir)) else { continue };
            if vfs::unlink(&p) != 0 {
                let errno = vfs::get_errno();
                if errno != libc::ENOENT {
                    log(true, format!("pgrcolumnar: deferred unlink of {}/{n}: {}", d.dir, std::io::Error::from_raw_os_error(errno)));
                }
            }
        }
    }
}

/// [`drain_deferred`] under this server's horizon (every backend's; on a standby, its hot-standby readers').
pub fn drain_deferred_now() -> PgResult<()> {
    if DEFERRED_ANY.load(Ordering::Relaxed) {
        drain_deferred(procarray::GetOldestNonRemovableTransactionIdShared()?);
    }
    Ok(())
}

/// How many deferred retirements of `slot` wait (`pgrc_compact_stats`).
pub fn deferred_count(slot: RelSlot) -> usize {
    match DEFERRED_ANY.load(Ordering::Relaxed) {
        true => pgsync::lock(&DEFERRED).iter().filter(|e| e.slot == slot).count(),
        false => 0,
    }
}

/// RECLAIMRACE C: the parts of every generation of `slot` the catalog holds whose publisher `probe` finds committed.
/// End-of-recovery cleanup at a hot standby's promotion keeps them: a read-only session that outlives the promotion
/// may still resolve an older generation (its deferred retirement, K1 L5, unlinks them once no snapshot can).
pub fn committed_parts(slot: RelSlot, probe: &dyn TxnProbe) -> std::collections::BTreeSet<u32> {
    let gens = pgsync::lock(&CATALOG).rels.get(&slot).map_or_else(Vec::new, |e| e.gens.clone());
    gens.iter()
        .filter(|g| probe.verdict(g.publisher_fxid) == TxnVerdict::Committed)
        .flat_map(|g| g.manifest.parts.iter().map(|p| p.part_no))
        .collect()
}

/// End-of-recovery cleanup unlinked, without WAL, the files of every generation whose publisher `probe` calls
/// aborted (an in-flight publish at promotion): those generations leave the entry too, and `head` becomes the newest
/// one kept, the generation `CURRENT` names again. Returns how many left.
pub fn drop_uncommitted(slot: RelSlot, probe: &dyn TxnProbe) -> u32 {
    let plock = crate::inval::publish_lock(slot.2);
    let _guard = pgsync::lock(&plock);
    let mut c = pgsync::lock(&CATALOG);
    let Some(e) = c.rels.get_mut(&slot) else { return 0 };
    let (aborted, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut e.gens).into_iter().partition(|g| probe.verdict(g.publisher_fxid) == TxnVerdict::Aborted);
    e.gens = kept;
    e.refit();
    e.head = e.gens.last().map_or(0, |g| g.gen);
    drop_views(slot);
    if let Some(top) = aborted.iter().map(|g| g.gen).max() {
        e.sweep.due = Some(e.sweep.due.map_or(top, |d| d.max(top)));
    }
    let gone = parts_only_in(&aborted, &e.gens);
    drop(c);
    forget_parts(gone);
    aborted.len() as u32
}

/// What [`load_dir`] did for one directory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Loaded {
    pub gens: u32,
    pub parts: u32,
    /// Generations served from their stats (P-1: runs and loose parts) or from their sections file (written before
    /// P-1).
    pub complete: u32,
    /// Generations completed from their parts in memory (the one-time walk): written before P-1 without their
    /// sections file, or with refused stats.
    pub completed: u32,
    /// P-1: the stats runs and the loose parts the generations with stats hold (a run carried from the generation
    /// before is counted again, and is not read again).
    pub runs: u32,
    pub loose: u32,
    /// Bytes of the facts image and the answers held in memory.
    pub section_bytes: u64,
    pub completion_micros: u64,
}

/// The generations a load keeps, newest first: walking `CURRENT`'s chain (`link`: a
/// generation's publisher and `prev_gen`), every generation whose publisher is still in
/// progress (a prepared transaction's) and then the effective one, the first committed; aborted
/// ones are skipped (a publish chains only on the effective generation, so they sit at the top).
pub(crate) fn kept_chain(head: u64, probe: &dyn TxnProbe, link: &mut dyn FnMut(u64) -> PgResult<(u64, u64)>) -> PgResult<Vec<u64>> {
    let mut out = Vec::new();
    let mut g = head;
    while g != 0 {
        let (publisher, prev) = link(g)?;
        match probe.verdict(publisher) {
            TxnVerdict::Committed => {
                out.push(g);
                break;
            }
            TxnVerdict::InProgress => out.push(g),
            TxnVerdict::Aborted => {}
        }
        g = prev;
    }
    Ok(out)
}

/// Load one directory into the catalog, read-only (end-of-recovery cleanup already ran): the
/// generations [`kept_chain`] keeps under clog, chained to each other, their parts open and
/// their sections read (a generation lacking them is completed in memory; `fold` runs the facts
/// fold). Under the table's publish lock, merged into an entry a publish may already hold. An
/// absent `CURRENT` = a relation with no generation.
pub fn load_dir(slot: RelSlot, dir: &str, fold: &dyn Fn(&Generation) -> PgResult<FactsImage>, serve: &dyn Fn(&Generation) -> PgResult<Option<FooterFacts>>) -> PgResult<Loaded> {
    let probe = crate::probe::ClogTxnProbe::new();
    let r = load_dir_with(slot, dir, &probe, fold, serve);
    probe.take_error()?;
    r
}

pub(crate) fn load_dir_with(slot: RelSlot, dir: &str, probe: &dyn TxnProbe, fold: &dyn Fn(&Generation) -> PgResult<FactsImage>, serve: &dyn Fn(&Generation) -> PgResult<Option<FooterFacts>>) -> PgResult<Loaded> {
    let plock = crate::inval::publish_lock(slot.2);
    let _guard = pgsync::lock(&plock);
    let mut vfs = RealVfs;
    let head = match vfs.read_full(&format!("{dir}/{CURRENT_FILE_NAME}")) {
        Ok(bytes) => CommitPointer::decode(&bytes).map_err(|e| crate::read_error(pgrc_read::ReadError::Format(e)))?.gen,
        Err(pgrc_write::WriteError::Io { errno: libc::ENOENT, .. }) => 0,
        Err(e) => return Err(crate::write_error(e)),
    };
    let mut manifests: BTreeMap<u64, (Manifest, u32)> = BTreeMap::new(); // alloc: Cold
    let mut keep = kept_chain(head, probe, &mut |g| {
        let bytes = match vfs.read_full(&format!("{dir}/{}", manifest_file_name(g))) {
            Ok(b) => b,
            Err(pgrc_write::WriteError::Io { errno: libc::ENOENT, .. }) => return Err(crate::corrupt(&format!("manifest generation {g} of {dir} is missing"))),
            Err(e) => return Err(crate::write_error(e)),
        };
        let m = Manifest::decode(&bytes).map_err(|e| crate::read_error(pgrc_read::ReadError::Format(e)))?;
        if m.header.gen != g || m.header.relfilenumber != slot.2 {
            return Err(Box::new(PgError::error(format!("pgrcolumnar: manifest {g} in {dir} names another relation")))); // alloc: Cold
        }
        let link = (m.header.publisher_fxid, m.header.prev_gen);
        manifests.insert(g, (m, sections::manifest_crc(&bytes)));
        Ok(link)
    })?;
    keep.reverse();
    // GENID: every number the directory shows, a rolled-back generation's too (CURRENT can name one)
    let used = manifests.values().fold((head, 0u32), |u, (m, _)| (u.0.max(m.header.gen), u.1.max(m.header.next_part_no)));
    let mut loaded = Loaded::default();
    let mut records: Vec<Arc<Generation>> = crate::alloc::try_vec(keep.len(), "catalog")?; // alloc: Cold
    let mut prev = 0u64;
    for g in &keep {
        let (m, mcrc) = manifests.remove(g).expect("kept generations are listed");
        let gen = load_generation(slot, dir, m, Some(mcrc), prev, records.last().map(|r| &**r), true, fold, serve, &mut loaded)?;
        prev = *g;
        records.push(Arc::new(gen)); // alloc: Cold charged OWNER (Generation::held_bytes: the Arc; RelEntry::held_bytes: the list)
    }
    let mut c = pgsync::lock(&CATALOG);
    // a directory dropped meanwhile (redo on a hot standby) removed its entry after the directory
    if !crate::dirpath::dir_exists(dir)? {
        return Ok(loaded);
    }
    note_used(&mut c, dir, used.0, used.1);
    let e = c.rels.entry(slot).or_insert_with(|| RelEntry::new(dir)); // alloc: Cold charged OWNER (RelEntry::held_bytes)
    for r in records {
        if !e.gens.iter().any(|g| g.gen == r.gen) {
            e.gens.push(r); // alloc: Cold charged OWNER (RelEntry::held_bytes)
        }
    }
    e.gens.sort_unstable_by_key(|g| g.gen); // numbers are unique in `gens` (the loop above skips a held number)
    e.refit();
    e.head = e.head.max(keep.last().copied().unwrap_or(0));
    drop_views(slot);
    // SEAL-OFFHEAD: a head loaded with EXT_UNDER tags is settled by the next sweep (at server start every creator has
    // ended): a restart and a clean stop leave no stacked head behind, which a binary without EXT_UNDER refuses
    let head = e.head;
    if e.gens.iter().any(|g| g.gen == head && !g.under.is_empty()) {
        queue_settle(slot);
    }
    c.dirs.insert(dir.to_string(), slot); // alloc: Cold charged OWNER (RelEntry::held_bytes)
    Ok(loaded)
}

/// P-1: a generation's stats from its root run and the runs it lists and its loose parts' footers (the open parts),
/// what `carried` (the generation before, or the one a redone publish built on) holds taken by reference and not
/// read; with the answers index, which checks them against the manifest. `Ok(None)`: a generation without a root of
/// `LOOSE_MAX` parts or more, written before P-1 (its sections file serves it, or a completion).
fn load_stats(dir: &str, gen: &Generation, carried: Option<&Generation>) -> Result<Option<(Arc<StatsState>, Answers)>, String> {
    let m = &gen.manifest;
    if stats_ref(m).map_err(|e| e.to_string())?.is_none() && m.parts.len() >= LOOSE_MAX {
        return Ok(None);
    }
    let held = carried.and_then(|c| c.stats.as_deref());
    let footer = |rec: &PartRecord| -> pgrc_write::WriteResult<pgrc_write::stats::PartStats> {
        let i = m.parts.binary_search_by_key(&rec.part_no, |p| p.part_no).map_err(|_| pgrc_write::WriteError::Contract { detail: "stats: a loose part not in the manifest" })?;
        let part = gen.parts.get(i).ok_or(pgrc_write::WriteError::Contract { detail: "stats: a loose part not open" })?;
        pgrc_write::stats::part_stats_from_open(&part.part, rec).map_err(pgrc_write::stats::read_error)
    };
    let state = StatsState::load(&mut RealVfs, dir, m, held, &mut |_, rec| footer(rec)).map_err(|e| e.to_string())?;
    let state = Arc::new(state);
    let answers = Answers::from_state(Arc::clone(&state), m).map_err(|e| e.to_string())?;
    Ok(Some((state, answers)))
}

/// One generation into memory: its parts open (reusing `carried`'s), its stats loaded (P-1; the runs `carried` holds
/// by reference), or for a generation written before P-1 its sections file read or a completion in memory, and its
/// facts as served. `mcrc`: the manifest file's trailing crc (computed here when a sections file is looked for).
/// `verify`: [`open_parts`]' identity check of the carried parts.
#[allow(clippy::too_many_arguments)]
fn load_generation(slot: RelSlot, dir: &str, m: Manifest, mcrc: Option<u32>, prev: u64, carried: Option<&Generation>, verify: bool, fold: &dyn Fn(&Generation) -> PgResult<FactsImage>, serve: &dyn Fn(&Generation) -> PgResult<Option<FooterFacts>>, loaded: &mut Loaded) -> PgResult<Generation> {
    // P-1 L4b: the root run is read after the parts open; hinted now, a cold load reads it beside their tails
    if let Ok(Some(root)) = stats_ref(&m) {
        if carried.and_then(|c| c.stats.as_deref()).is_none_or(|s| s.root != Some(root)) {
            RealVfs.willneed(&format!("{dir}/{}", pgrc_format::dirlayout::run_file_name(root.gen, root.seq)), 0, root.len);
        }
    }
    let parts = open_parts(slot, dir, &m, carried, verify)?;
    let mut gen = Generation::linked(m, prev, parts, None, None)?;
    gen.carry_from(carried);
    gen.bind_runs(dir, carried);
    let why = match load_stats(dir, &gen, carried) {
        Ok(Some((stats, answers))) => {
            loaded.runs += stats.runs.len() as u32;
            loaded.loose += stats.loose.len() as u32;
            gen.stats = Some(stats);
            gen.set_answers(Arc::new(answers)); // alloc: Cold charged gen.charge (held_bytes, refit by set_answers)
            None
        }
        Ok(None) => match read_sections(dir, &gen.manifest, mcrc.unwrap_or_else(|| sections::manifest_crc(&gen.manifest.encode()))) {
            Ok(Some(f)) => {
                if let Some(i) = image_of(&f, &gen.manifest) {
                    gen.set_image(Arc::new(i)); // alloc: Cold charged gen.charge (held_bytes, refit by set_image)
                }
                if let Some(a) = Answers::from_file(&f, gen.manifest.parts.len()).map_err(|e| crate::read_error(pgrc_read::ReadError::Format(e)))? {
                    gen.set_answers(Arc::new(a)); // alloc: Cold charged gen.charge (held_bytes, refit by set_answers)
                }
                Some("a section missing")
            }
            Ok(None) => Some("no sections file"),
            Err(Refused::OlderVersion(v)) => {
                log(false, format!("pgrcolumnar: sections file of {dir} gen {} is version {v}; completing it in memory", gen.gen)); // alloc: Cold
                Some("sections file of an older version")
            }
            Err(Refused::Bad(e)) => {
                log(true, format!("pgrcolumnar: sections file of {dir} gen {} refused: {e}", gen.gen)); // alloc: Cold
                Some("sections file refused")
            }
        },
        // a root that does not describe its generation is never trusted: the generation is completed from its parts
        Err(e) => {
            log(false, format!("pgrcolumnar: stats of {dir} gen {} refused: {e}; completing it in memory", gen.gen)); // alloc: Cold
            Some("stats refused")
        }
    };
    if gen.image().is_some() && gen.answers().is_some() {
        loaded.complete += 1;
    } else {
        let t0 = std::time::Instant::now();
        complete(&gen, fold)?;
        loaded.completed += 1;
        loaded.completion_micros += t0.elapsed().as_micros() as u64;
        log(false, format!("pgrcolumnar: generation {} of {dir} completed in memory at load ({}): {} parts walked, {} us", gen.gen, why.unwrap_or("stats without an image"), gen.parts.len(), t0.elapsed().as_micros())); // alloc: Cold
    }
    loaded.section_bytes += gen.image().map_or(0, |i| i.encode().map_or(0, |b| b.len() as u64)) + gen.answers().map_or(0, |a| a.bytes());
    // the facts as served, when the image decides them without a catalog (footer.rs)
    if let Some(f) = serve(&gen)? {
        gen.install_facts(Arc::new(f)); // alloc: Cold charged gen.charge (held_bytes, refit by install_facts)
    }
    loaded.gens += 1;
    loaded.parts += gen.parts.len() as u32;
    Ok(gen)
}

/// Redo of a publish while the catalog is live: the generation joins the entry as a publish on
/// the primary would, under the table's publish lock. A generation the entry holds from the same
/// publisher (a load that read it first) stays; one from another publisher is a rolled-back
/// publish's, whose number this one uses again, and is replaced. P-1: only the publish's new runs
/// and new loose parts' footers are read; the base generation's runs are carried by reference.
pub fn redo_publish(slot: RelSlot, dir: &str, m: Manifest, fold: &dyn Fn(&Generation) -> PgResult<FactsImage>, serve: &dyn Fn(&Generation) -> PgResult<Option<FooterFacts>>) -> PgResult<()> {
    let plock = crate::inval::publish_lock(slot.2);
    let _guard = pgsync::lock(&plock);
    let g = m.header.gen;
    let carried = {
        let c = pgsync::lock(&CATALOG);
        let held = c.rels.get(&slot);
        if held.is_some_and(|e| e.gens.iter().any(|r| r.gen == g && r.publisher_fxid == m.header.publisher_fxid)) {
            return Ok(());
        }
        held.and_then(|e| base_of(&e.gens, m.header.prev_gen))
    };
    let prev = m.header.prev_gen;
    let gen = load_generation(slot, dir, m, None, prev, carried.as_deref(), VERIFY_CARRIES, fold, serve, &mut Loaded::default())?;
    let next_part_no = gen.manifest.header.next_part_no;
    let mut c = pgsync::lock(&CATALOG);
    note_used(&mut c, dir, g, next_part_no);
    let e = c.rels.entry(slot).or_insert_with(|| RelEntry::new(dir)); // alloc: Cold charged OWNER (RelEntry::held_bytes)
    // the primary's publisher passed any generation above its base: rolled back (GENID); one with this number is a
    // rolled-back publish's from before a restart of the primary, whose numbers then start from its directory again
    let replaced = e.drop_dead(prev, &gen.manifest);
    e.gens.push(Arc::new(gen)); // alloc: Cold charged OWNER (Generation::held_bytes: the Arc; RelEntry::held_bytes: the list)
    e.gens.sort_unstable_by_key(|r| r.gen); // numbers are unique in `gens` (`drop_dead` took the same number out)
    e.refit();
    e.head = e.head.max(g);
    drop_views(slot);
    let gone = parts_only_in(&replaced, &e.gens);
    c.dirs.insert(dir.to_string(), slot); // alloc: Cold charged OWNER (RelEntry::held_bytes)
    drop(c);
    forget_parts(gone);
    fire(&SlotEvent::Published(slot, g));
    Ok(())
}

/// Redo of `TRUNCATE` while the catalog is live: the entry is empty again.
pub fn redo_truncate(slot: RelSlot, dir: &str) {
    let plock = crate::inval::publish_lock(slot.2);
    let _guard = pgsync::lock(&plock);
    remove_dir(dir);
    create(slot, dir);
}

/// The directory of `slot` (a test / report face).
pub fn dir_of(slot: RelSlot) -> Option<String> {
    pgsync::lock(&CATALOG).rels.get(&slot).map(|e| e.dir.clone())
}

/// A server-log line (the hosts crate has no `elog` dependency).
pub fn log(level_warning: bool, text: String) {
    let _ = elog::elog(if level_warning { types_error::WARNING } else { types_error::LOG }, text);
}

/// The catalog's census: `(relations, generations, parts held)`.
pub fn counters() -> (u64, u64, u64) {
    let c = pgsync::lock(&CATALOG);
    let gens: u64 = c.rels.values().map(|e| e.gens.len() as u64).sum();
    let parts: u64 = c.rels.values().flat_map(|e| e.gens.iter()).map(|g| g.parts.len() as u64).sum();
    (c.rels.len() as u64, gens, parts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgrc_format::manifest::{ManifestHeader, SegState};

    struct Committed(Vec<u64>);
    impl CommitCheck for Committed {
        fn committed(&self, fxid: u64) -> bool {
            self.0.contains(&fxid)
        }
    }

    fn gen(g: u64, prev: u64, fxid: u64) -> Arc<Generation> {
        let header = ManifestHeader { gen: g, prev_gen: prev, publisher_fxid: fxid, relfilenumber: 9, schema_fingerprint: 1, magic: 0, format_version: 0, spc: 1663, db: 5, part_count: 0, next_part_no: 0, flags: 0, reserved: 0 };
        Arc::new(Generation::new(Manifest::new(header, Vec::new()), Vec::new(), None, None).unwrap())
    }

    fn install(slot: RelSlot, gens: Vec<Arc<Generation>>, head: u64) {
        install_at(slot, &format!("t/{}", slot.2), gens, head);
    }

    fn install_at(slot: RelSlot, dir: &str, gens: Vec<Arc<Generation>>, head: u64) {
        let mut c = pgsync::lock(&CATALOG);
        let mut e = RelEntry { gens, head, ..RelEntry::new(dir) };
        e.refit();
        c.rels.insert(slot, e);
        c.dirs.insert(dir.to_string(), slot);
    }

    /// The engine bridge's reading of a lookup: a visible generation without parts (F1's CREATE publishes one) is the
    /// empty table, as no visible generation is; a relation the catalog does not hold is the caller's error.
    #[test]
    fn the_bridge_serves_no_generation_without_parts() {
        let slot = (1663, 5, 9_006);
        install(slot, vec![gen(1, 0, 10)], 1);
        let Lookup::Gen(g) = visible(slot, &Committed(vec![10])).unwrap() else { panic!("gen 1 visible") };
        assert!(g.manifest.parts.is_empty());
        assert!(matches!(crate::scan::bridge_generation(Lookup::Gen(g)), Ok(None)), "part-less: the empty table");
        assert!(matches!(crate::scan::bridge_generation(visible(slot, &Committed(vec![])).unwrap()), Ok(None)), "none visible");
        assert!(crate::scan::bridge_generation(visible((1663, 5, 9_998), &Committed(vec![])).unwrap()).is_err(), "unknown");
        remove_dir("t/9006");
    }

    /// P-1 L2: the part list of a publish is a merge join with its base's, linear in the part count (a count of
    /// compares, not a timer): 10,000 carried records, sixteen retired in the middle, one rewritten in place (another
    /// file length), two installed at the end.
    #[test]
    fn open_parts_carries_by_one_merge_join() {
        let rec = |part_no: u32, file_len: u64| PartRecord { rows: 1, file_len, footer_off: file_len - 64, dv_gen: 0, dv_len: 0, part_no, flags: 0, granule_count: 1, band_count: 1, dv_crc: 0, granule_rows: 0 };
        let old: Vec<PartRecord> = (0..10_000).map(|n| rec(n, 4096)).collect();
        let mut new: Vec<PartRecord> = old.iter().copied().filter(|r| !(5_000..5_016).contains(&r.part_no)).collect();
        new[7_000].file_len = 8192;
        new[7_000].footer_off = 8192 - 64;
        new.push(rec(10_000, 4096));
        new.push(rec(10_001, 4096));
        let mut steps = 0;
        let carry = carry_index(&new, &old, &mut steps);
        assert!(steps <= (old.len() + new.len()) as u64, "{steps} compares");
        assert_eq!(carry.iter().filter(|c| c.is_none()).count(), 3, "the rewritten part and the two new ones open");
        assert!(new.iter().zip(&carry).all(|(r, c)| c.is_none_or(|j| old[j].part_no == r.part_no && old[j].file_len == r.file_len)));
        assert_eq!(carry[7_000], None);
        // a base that is not the new list's (another table's, empty): nothing carried, still linear
        let mut steps = 0;
        assert!(carry_index(&new, &[], &mut steps).iter().all(Option::is_none));
        assert_eq!(steps, new.len() as u64);
    }

    #[test]
    fn the_visible_generation_is_the_newest_committed_on_the_chain() {
        let slot = (1663, 5, 9_001);
        install(slot, vec![gen(1, 0, 10), gen(2, 1, 11), gen(3, 2, 12)], 3);
        let pick = |c: &[u64]| match visible(slot, &Committed(c.to_vec())).unwrap() {
            Lookup::Gen(g) => Some(g.gen),
            Lookup::Empty => None,
            Lookup::Unknown => panic!("known"),
        };
        assert_eq!(pick(&[10, 11, 12]), Some(3));
        assert_eq!(pick(&[10, 11]), Some(2), "the head's publisher not yet visible: its predecessor");
        assert_eq!(pick(&[10]), Some(1));
        assert_eq!(pick(&[]), None, "no committed generation: the never-loaded posture");
        assert_eq!(pick(&[12]), Some(3));
        assert!(matches!(visible((1663, 5, 9_999), &Committed(vec![])).unwrap(), Lookup::Unknown));
        remove_dir("t/9001");
        assert!(matches!(visible(slot, &Committed(vec![10])).unwrap(), Lookup::Unknown), "a removed directory leaves the table");
    }

    /// K1-MEM: a part opened through the process's part registry under device id `dev` (distinctive per test: the
    /// registry is process-global and tests run in parallel), its one row `value`.
    fn reg_part(dev: u64, value: i64, ino: u64) -> OpenedPart {
        let bytes = crate::tests::ino_reuse::part_bytes(value);
        let pin = crate::inval::registry().open_pinned(&PartExpect::none(), || Ok(Box::new(pgrc_read::MemPartIo::new(bytes, dev, ino)) as Box<dyn pgrc_read::PartIo>)).expect("open");
        OpenedPart::new(OpenedPartData { part: Arc::clone(pin.part()), pin, path: CString::new(format!("part-{ino}")).unwrap(), expect: PartExpect::none(), deletes: None, row_map: Arc::default() })
    }

    /// `p` carried into a later generation (as `open_parts` carries a base's parts).
    fn reg_carried(p: &OpenedPart) -> OpenedPart {
        OpenedPart::new(OpenedPartData { part: Arc::clone(&p.part), pin: PartPin::pin(&p.part), path: p.path.clone(), expect: p.expect, deletes: None, row_map: Arc::clone(&p.row_map) })
    }

    /// Generation `g` (publisher `10 + g`) of `parts`.
    fn reg_gen(g: u64, parts: Vec<OpenedPart>) -> Arc<Generation> {
        let header = ManifestHeader { gen: g, prev_gen: g - 1, publisher_fxid: 10 + g, relfilenumber: 9, schema_fingerprint: 1, magic: 0, format_version: 0, spc: 1663, db: 5, part_count: 0, next_part_no: 0, flags: 0, reserved: 0 };
        Arc::new(Generation::new(Manifest::new(header, Vec::new()), parts, None, None).unwrap())
    }

    fn reg_key(p: &OpenedPart) -> (u64, u64, u64) {
        let i = p.part.ident();
        (i.dev, i.ino, i.len)
    }

    /// COMPACTP: a generation without parts has no full test to carry (asked, as a publish's base hold asks): the next
    /// one reads its own first part's, and carries that on. A table's first generation is empty.
    #[test]
    fn a_partless_base_carries_no_full_test() {
        const DEV: u64 = 0x4B31_4D45_0000_00C1;
        let empty = gen(1, 0, 11);
        assert!(empty.full_test().is_none(), "no parts: no test");
        let g2 = reg_gen(2, vec![reg_part(DEV, 5, 1)]);
        g2.carry_from(Some(&*empty));
        let t2 = g2.full_test().expect("its first part's test, not the empty base's none");
        let g3 = reg_gen(3, vec![reg_carried(&g2.parts[0]), reg_part(DEV, 6, 2)]);
        g3.carry_from(Some(&*g2));
        assert!(Arc::ptr_eq(&t2, &g3.full_test().expect("carried")), "a base's test carries");
    }

    #[test]
    fn retiring_generations_drops_the_parts_only_they_name_from_the_part_registry() {
        use crate::tests::ino_reuse::decode_single;
        const DEV: u64 = 0x4B31_4D45_0000_0003;
        let reg = crate::inval::registry();
        let (a, b, c) = (reg_part(DEV, 11, 1), reg_part(DEV, 22, 2), reg_part(DEV, 33, 3));
        let (ka, kb, kc) = (reg_key(&a), reg_key(&b), reg_key(&c));
        let reader = Arc::clone(&a.part);
        let b_carried = reg_carried(&b);
        // Its own slot and directory (K1-MEM's 9_3xx): the catalog is process-global and tests run in parallel.
        let slot = (1663, 5, 9_301);
        install(slot, vec![reg_gen(1, vec![a, b]), reg_gen(2, vec![b_carried, c])], 2);
        retire_below(slot, 2);
        assert!(reg.get(ka).is_none(), "a part only the retired generation names leaves the registry (its entry held the unlinked file's fd)");
        assert!(reg.get(kb).is_some(), "a part the kept generation carries stays");
        assert!(reg.get(kc).is_some());
        assert_eq!(decode_single(&reader), 11, "a reader still holding the retired part reads it");
        assert_eq!(Arc::strong_count(&reader), 1, "and holds its last reference: the part closes when the reader lets go");
        remove_dir("t/9301");
        assert!(reg.get(kb).is_none() && reg.get(kc).is_none(), "the directory's removal takes the rest");
    }

    /// RECLAIMDEFER (i′): under `CATALOG` (every catalog lookup of every table waits on it) a retirement only takes the
    /// generations below `keep_from` out of the entry; its part-registry walk (O(retired x kept x parts)) and the drops of
    /// the retired generations (each with its parsed manifest) run with no lock held. The registry still loses the parts
    /// only they named.
    #[test]
    fn a_retirement_walks_the_registry_and_drops_its_generations_outside_the_catalog_lock() {
        const DEV: u64 = 0x4B31_4D45_0000_0007;
        let reg = crate::inval::registry();
        let (a, b, c) = (reg_part(DEV, 71, 1), reg_part(DEV, 72, 2), reg_part(DEV, 73, 3));
        let (ka, kb) = (reg_key(&a), reg_key(&b));
        let b_carried = reg_carried(&b);
        let slot = (1663, 5, 9_306);
        install(slot, vec![reg_gen(1, vec![a]), reg_gen(2, vec![b]), reg_gen(3, vec![b_carried, c])], 3);
        let gen1 = Arc::downgrade(&generations(slot).unwrap().0[0]);
        let locked = || crate::holds::get()[crate::holds::LOCKED_RETIRE];
        let before = locked();
        assert_eq!(retire_below(slot, 3), 2);
        assert_eq!(locked() - before, 0, "no retirement walked the registry or dropped its generations under a lock");
        assert!(gen1.upgrade().is_none(), "the retired generation is gone once the retirement returns");
        assert!(reg.get(ka).is_none(), "a part only a retired generation names leaves the registry");
        assert!(reg.get(kb).is_some(), "a part the kept generation carries stays");
        assert_eq!(generations(slot).unwrap().0.iter().map(|g| g.gen).collect::<Vec<_>>(), vec![3]);
        remove_dir("t/9306");
    }

    /// RECLAIMDEFER (i′): a taken generation is out of the catalog at once (no lookup or plan sees it), and the retirement
    /// holds it, with the registry entries of the parts only it names, until it finishes.
    #[test]
    fn a_taken_generation_leaves_the_catalog_at_once_and_goes_when_its_retirement_finishes() {
        const DEV: u64 = 0x4B31_4D45_0000_0008;
        let reg = crate::inval::registry();
        let (a, b) = (reg_part(DEV, 81, 1), reg_part(DEV, 82, 2));
        let ka = reg_key(&a);
        let slot = (1663, 5, 9_307);
        install(slot, vec![reg_gen(1, vec![a]), reg_gen(2, vec![b])], 2);
        let gen1 = Arc::downgrade(&generations(slot).unwrap().0[0]);
        let r = take_below(slot, 2);
        assert_eq!((r.len(), r.gens()), (1, vec![1]));
        assert_eq!(generations(slot).unwrap().0.iter().map(|g| g.gen).collect::<Vec<_>>(), vec![2], "out of the catalog");
        assert!(take_below(slot, 2).is_empty(), "a second retirement finds nothing to take");
        assert!(gen1.upgrade().is_some() && reg.get(ka).is_some(), "held by the retirement until it finishes");
        r.finish();
        assert!(gen1.upgrade().is_none() && reg.get(ka).is_none());
        remove_dir("t/9307");
    }

    /// RECLAIMDEFER (i′) forced-mismatch injection: the newest generation below keep is replaced by a twin (its number,
    /// manifest, parts and stats under another `Arc`), so a plan from the catalog names the same files but a hint's
    /// identity check fails.
    #[test]
    fn perturb_below_replaces_the_newest_below_generation_by_a_twin() {
        const DEV: u64 = 0x4B31_4D45_0000_0009;
        let (a, b, c) = (reg_part(DEV, 91, 1), reg_part(DEV, 92, 2), reg_part(DEV, 93, 3));
        let slot = (1663, 5, 9_310);
        install(slot, vec![reg_gen(1, vec![a]), reg_gen(2, vec![b]), reg_gen(3, vec![c])], 3);
        let before = generations(slot).unwrap().0;
        assert!(perturb_below(slot, 3).unwrap());
        let after = generations(slot).unwrap().0;
        assert_eq!(after.iter().map(|g| g.gen).collect::<Vec<_>>(), vec![1, 2, 3]);
        assert!(Arc::ptr_eq(&before[0], &after[0]) && Arc::ptr_eq(&before[2], &after[2]));
        assert!(!Arc::ptr_eq(&before[1], &after[1]), "generation 2 is another object now");
        let nos = |g: &Generation| g.manifest.parts.iter().map(|p| p.part_no).collect::<Vec<_>>();
        assert_eq!(nos(&before[1]), nos(&after[1]), "the same manifest");
        assert!(Arc::ptr_eq(&before[1].parts[0].part, &after[1].parts[0].part), "the same open part");
        assert!(!perturb_below(slot, 1).unwrap(), "nothing below 1");
        remove_dir("t/9310");
    }

    /// RECLAIMDEFER (i′) condition 2: reclaims plan and take their generations in one hold of the table's publish lock
    /// and finish after letting go of it, so a reclaim that plans while another has taken but not yet finished never
    /// sees the other's generations (it cannot name their files from the catalog again, only meet them gone): four
    /// threads, each reclaiming at its own rising keeps with a yield between take and finish; every generation is
    /// taken exactly once, by the plan that saw it.
    #[test]
    fn reclaims_plan_and_take_in_one_publish_lock_hold_and_never_plan_from_a_stale_catalog() {
        const TOP: u64 = 400;
        let slot = (1663, 5, 9_308);
        install(slot, (1..=TOP).map(|g| gen(g, g - 1, 10 + g)).collect(), TOP);
        let taken = std::sync::Mutex::new(BTreeSet::<u64>::new());
        let stale = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|s| {
            for t in 0..4u64 {
                let (taken, stale) = (&taken, &stale);
                s.spawn(move || {
                    let mut keep = 1 + t;
                    while keep <= TOP {
                        let lock = crate::inval::publish_lock(slot.2);
                        let r = {
                            let _hold = pgsync::lock(&lock);
                            let planned: Vec<u64> = generations(slot).unwrap().0.iter().map(|g| g.gen).filter(|&g| g < keep).collect();
                            let r = take_below(slot, keep);
                            let mut seen = taken.lock().unwrap();
                            stale.fetch_add(planned.iter().filter(|g| seen.contains(g)).count(), std::sync::atomic::Ordering::Relaxed);
                            assert_eq!(r.gens(), planned, "the retirement takes what the plan saw");
                            for g in r.gens() {
                                assert!(seen.insert(g), "generation {g} taken twice");
                            }
                            r
                        };
                        std::thread::yield_now();
                        r.finish();
                        keep += 1 + (keep * 7 + t) % 9;
                    }
                });
            }
        });
        assert_eq!(stale.into_inner(), 0, "no plan saw a generation another reclaim had taken");
        let all = taken.into_inner().unwrap();
        let top = all.iter().max().copied().unwrap_or(0);
        assert_eq!(all.len() as u64, top, "every generation below the highest keep taken once: {top}");
        remove_dir("t/9308");
    }

    /// RECLAIMDEFER (i′) condition 3 (number reuse): a generation that rolled back (a publish built past it) leaves its
    /// dead parts and its own files in the entry, and every reclaim takes the locked path until one judged them; only
    /// then does the next go incremental. The incremental path's names come only from committed generations.
    #[test]
    fn a_rolled_back_generations_numbers_keep_the_reclaim_on_the_locked_path() {
        let slot = (1663, 5, 9_309);
        let path = |s: RelSlot| crate::ingest::incremental_facts(&dead_parts(s), &dead_files(s), sweep_due(s).is_some(), true);
        install(slot, vec![gen_of(1, 0, 10, 2, &[(0, 0, 0), (1, 0, 0)], &[], &[])], 1);
        assert!(!path(slot), "a new entry's first reclaim sweeps");
        note_reclaim(slot, 1, true, &mut std::iter::empty::<u32>());
        assert!(path(slot), "nothing rolled back, no sweep due: incremental");
        // gen 2 (publisher 11) adds part 2; its publisher rolls back, and gen 3 (publisher 12) builds on 1 with part 3
        let g2 = gen_of(2, 1, 11, 3, &[(0, 0, 0), (1, 0, 0), (2, 0, 0)], &[], &[]);
        let g3 = gen_of(3, 1, 12, 4, &[(0, 0, 0), (1, 0, 0), (3, 0, 0)], &[], &[]);
        {
            let mut c = pgsync::lock(&CATALOG);
            let e = c.rels.get_mut(&slot).unwrap();
            e.gens.push(g2);
            let dead = e.drop_dead(1, &g3.manifest);
            assert_eq!(dead.iter().map(|g| g.gen).collect::<Vec<_>>(), vec![2]);
            e.gens.push(g3);
            e.head = 3;
        }
        assert_eq!(dead_parts(slot), vec![2], "the rolled-back generation's part");
        assert!(dead_files(slot).contains(&manifest_file_name(2)), "and its own files");
        assert!(!path(slot), "its numbers send the reclaim to the locked path");
        // a reclaim below 1 judges neither (they wait for one keeping a generation above 2): still locked
        note_reclaim(slot, 1, false, &mut std::iter::empty::<u32>());
        assert!(!path(slot));
        // the locked reclaim keeping 3 unlinks them
        forget_dead_parts(slot, &[2]);
        note_reclaim(slot, 3, false, &mut std::iter::empty::<u32>());
        assert!(dead_parts(slot).is_empty() && dead_files(slot).is_empty());
        assert!(path(slot), "nothing rolled back waits: incremental again");
        sweep_after(slot, 3);
        assert!(!path(slot), "a reclaim that failed after its take leaves a sweep");
        note_reclaim(slot, 3, true, &mut std::iter::empty::<u32>());
        assert!(path(slot), "the sweep keeping 3 clears it");
        remove_dir("t/9309");
    }

    #[test]
    fn removing_a_directory_or_rolled_back_generations_drops_their_parts_from_the_part_registry() {
        use crate::tests::ino_reuse::decode_single;
        const DEV: u64 = 0x4B31_4D45_0000_0004;
        let reg = crate::inval::registry();
        // DROP / TRUNCATE / a rewrite's old locator: every part of the directory's entry, carried ones once; a reader
        // still holding one reads it and holds its last reference.
        let (a, b) = (reg_part(DEV, 41, 1), reg_part(DEV, 42, 2));
        let (ka, kb) = (reg_key(&a), reg_key(&b));
        let reader = Arc::clone(&b.part);
        let a_carried = reg_carried(&a);
        install((1663, 5, 9_302), vec![reg_gen(1, vec![a]), reg_gen(2, vec![a_carried, b])], 2);
        remove_dir("t/9302");
        assert!(reg.get(ka).is_none() && reg.get(kb).is_none(), "a removed directory's parts leave the registry (their entries held the unlinked files' fds)");
        assert_eq!(decode_single(&reader), 42, "a reader still holding a removed part reads it");
        assert_eq!(Arc::strong_count(&reader), 1, "and holds its last reference");
        // DROP DATABASE: every entry under the database directory.
        let c = reg_part(DEV, 43, 3);
        let kc = reg_key(&c);
        install_at((1663, 9_303, 9_303), "k1mem9303/pgrc_9303", vec![reg_gen(1, vec![c])], 1);
        remove_under("k1mem9303");
        assert!(reg.get(kc).is_none(), "an entry under a removed database directory takes its parts along");
        // End-of-recovery cleanup: an aborted publish's generation goes, and the parts only it named.
        let (d, e) = (reg_part(DEV, 44, 4), reg_part(DEV, 45, 5));
        let (kd, ke) = (reg_key(&d), reg_key(&e));
        let d_carried = reg_carried(&d);
        let slot = (1663, 5, 9_304);
        install(slot, vec![reg_gen(1, vec![d]), reg_gen(2, vec![d_carried, e])], 2);
        let probe = Verdicts(BTreeMap::from([(11, TxnVerdict::Committed), (12, TxnVerdict::Aborted)]));
        assert_eq!(drop_uncommitted(slot, &probe), 1);
        assert!(reg.get(ke).is_none(), "a part only the aborted generation named leaves the registry");
        assert!(reg.get(kd).is_some(), "a part the kept generation names stays");
        remove_dir("t/9304");
        assert!(reg.get(kd).is_none());
    }

    #[test]
    fn a_retired_generation_on_the_walk_is_an_error_and_retire_keeps_the_rest() {
        let slot = (1663, 5, 9_002);
        install(slot, vec![gen(1, 0, 10), gen(2, 1, 11), gen(3, 2, 12)], 3);
        retire_below(slot, 2);
        assert_eq!(generations(slot).unwrap().0.iter().map(|g| g.gen).collect::<Vec<_>>(), vec![2, 3]);
        assert!(matches!(visible(slot, &Committed(vec![11, 12])).unwrap(), Lookup::Gen(g) if g.gen == 3));
        assert!(visible(slot, &Committed(vec![])).is_err(), "walking past a retired generation is a bug, not a silent empty");
        remove_dir("t/9002");
    }

    struct Verdicts(BTreeMap<u64, TxnVerdict>);
    impl TxnProbe for Verdicts {
        fn verdict(&self, fxid: u64) -> TxnVerdict {
            self.0[&fxid]
        }
    }

    #[test]
    fn a_load_keeps_in_progress_publishers_over_the_effective_generation_and_skips_aborted_ones() {
        use TxnVerdict::*;
        let links: BTreeMap<u64, (u64, u64)> = [(1, (10, 0)), (2, (11, 1)), (3, (12, 2)), (4, (13, 3))].into();
        let walk = |head: u64, v: &[TxnVerdict]| {
            let probe = Verdicts((10..).zip(v.iter().copied()).collect());
            kept_chain(head, &probe, &mut |g| links.get(&g).copied().ok_or_else(|| crate::corrupt("missing")))
        };
        assert_eq!(walk(4, &[Committed, Committed, InProgress, Aborted]).unwrap(), vec![3, 2]);
        assert_eq!(walk(4, &[InProgress; 4]).unwrap(), vec![4, 3, 2, 1], "no committed generation: every in-progress one");
        assert_eq!(walk(4, &[Aborted; 4]).unwrap(), Vec::<u64>::new());
        assert_eq!(walk(0, &[]).unwrap(), Vec::<u64>::new());
        assert!(walk(5, &[InProgress; 4]).is_err(), "a chained generation without a manifest");
    }

    fn manifest(g: u64, prev: u64, fxid: u64) -> Manifest {
        gen(g, prev, fxid).manifest.as_ref().clone()
    }

    /// RP B1: a rolled-back publish leaves its generation in the catalog and a publish that uses its number again
    /// (before GENID; now a primary restarted since the rollback) replaces the dead one at its redo (a standby read the
    /// dead one's rows before).
    #[test]
    fn a_republished_generation_replaces_the_aborted_one() {
        let slot = (1663, 5, 9_004);
        let dir = std::env::temp_dir().join(format!("pgrc_am_republish_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_str().unwrap();
        install(slot, vec![gen(1, 0, 10), gen(2, 1, 11)], 2);
        let fold = |_: &Generation| Ok(FactsImage::default());
        let serve = |_: &Generation| Ok(None);
        let held = |g: u64| generations(slot).unwrap().0.into_iter().find(|r| r.gen == g).unwrap();
        let dead = held(2);
        redo_publish(slot, d, manifest(2, 1, 11), &fold, &serve).unwrap();
        assert!(Arc::ptr_eq(&held(2), &dead), "the same publisher's generation (a load read it first) stays");
        redo_publish(slot, d, manifest(2, 1, 12), &fold, &serve).unwrap();
        assert_eq!(generations(slot).unwrap().0.iter().map(|g| (g.gen, g.publisher_fxid)).collect::<Vec<_>>(), vec![(1, 10), (2, 12)]);
        assert!(matches!(visible(slot, &Committed(vec![10, 12])).unwrap(), Lookup::Gen(g) if g.gen == 2 && g.publisher_fxid == 12));
        remove_dir("t/9004");
        remove_dir(d);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// D3110 (b): a relcache invalidation of the relation (VACUUM's relstats update) takes its parts' keys out of the shared
    /// registry while the generation keeps the parts open, and a later registry open of the same file (ANALYZE's) is another
    /// instance. A registry whose holder is the catalog still reaches the generation's part: `cache_parts` names it and
    /// the content trim evicts its contents.
    #[test]
    fn the_trim_reaches_a_generations_part_after_a_relcache_invalidation() {
        use pgrc_read::{MemPartIo, PartExpect, PartRegistry};
        const DEV: u64 = 0x4D33_4801_0000_3110; // the registry is process-global: identities of this test only
        fn mine(f: &mut dyn FnMut(&Arc<OpenPart>)) {
            held_parts(&mut |p| {
                if p.ident().dev == DEV {
                    f(p)
                }
            });
        }
        let reg = PartRegistry::new(u64::MAX);
        reg.set_held_parts(mine);
        let bytes = crate::tests::ino_reuse::part_bytes(7);
        let open = || reg.open_pinned(&PartExpect::none(), || Ok(Box::new(MemPartIo::new(bytes.clone(), DEV, 1)) as Box<dyn pgrc_read::PartIo>)).expect("open");
        let pin = open();
        let part = Arc::clone(pin.part());
        let id = part.ident();
        let key = (id.dev, id.ino, id.len);
        let opened = OpenedPart::new(OpenedPartData { part: Arc::clone(&part), pin, path: CString::new("t/part").unwrap(), expect: PartExpect::none(), deletes: None, row_map: Arc::default() });
        install((1663, 5, 9_062), vec![reg_gen(1, vec![opened])], 1);
        reg.invalidate(key); // what `inval::invalidate_relid` does to the relation's recorded keys
        assert!(reg.get(key).is_none(), "the invalidation took the key out of the entries");
        let again = open();
        assert!(!Arc::ptr_eq(again.part(), &part), "a later open of the same file is another instance");
        assert!(reg.cache_parts().iter().any(|p| Arc::ptr_eq(p, &part)), "the holder still names the generation's part");
        part.prefault_all_sections().expect("warm");
        assert!(part.resident() > 0);
        reg.trim_contents(0);
        assert_eq!(part.resident(), 0, "the trim evicted the generation's part's contents");
        drop(again);
        remove_dir("t/9062");
        let mut held = false;
        held_parts(&mut |p| held |= Arc::ptr_eq(p, &part));
        assert!(!held);
    }

    /// RP B2: parts carry from the generation the publish built on, never from a newer dead one whose part numbers
    /// the publish reuses.
    #[test]
    fn parts_are_carried_from_the_base_generation_only() {
        let gens = vec![gen(1, 0, 10), gen(2, 1, 11), gen(3, 2, 12)];
        assert_eq!(base_of(&gens, 2).map(|g| g.gen), Some(2), "gen 3 is the rolled-back one: gen 3 again builds on 2");
        assert_eq!(base_of(&gens, 0).map(|g| g.gen), None, "the first generation carries nothing");
        assert_eq!(base_of(&gens, 7).map(|g| g.gen), None, "a base the catalog does not hold carries nothing");
    }

    /// RP B3: end-of-recovery cleanup's aborted generations leave the live catalog, and the head is the newest kept.
    #[test]
    fn drop_uncommitted_takes_the_aborted_generations_and_resets_the_head() {
        use TxnVerdict::*;
        let slot = (1663, 5, 9_005);
        install(slot, vec![gen(1, 0, 10), gen(2, 1, 11), gen(3, 2, 12), gen(4, 3, 13)], 4);
        let probe = Verdicts([(10, Committed), (11, Aborted), (12, InProgress), (13, Aborted)].into());
        assert_eq!(drop_uncommitted(slot, &probe), 2);
        let (gens, head) = generations(slot).unwrap();
        assert_eq!((gens.iter().map(|g| g.gen).collect::<Vec<_>>(), head), (vec![1, 3], 3));
        assert_eq!(drop_uncommitted((1663, 5, 9_999), &probe), 0, "a relation the catalog does not hold");
        remove_dir("t/9005");
    }

    #[test]
    fn create_is_empty_and_facts_serve_once_from_the_image() {
        let slot = (1663, 5, 9_003);
        create(slot, "t/9003");
        assert!(matches!(visible(slot, &Committed(vec![1])).unwrap(), Lookup::Empty));
        let g = gen(1, 0, 1);
        assert!(g.facts(|_| unreachable!()).unwrap().is_none(), "no image: no facts");
        g.set_image(Arc::new(FactsImage::default()));
        let mut calls = 0;
        let mut serve = |_: &FactsImage| {
            calls += 1;
            Ok(FooterFacts { gen: 1, rows: 7, bytes: 0, granules: 0, parts: 0, ndv: Some(vec![]), cols: None, zones: Vec::new() })
        };
        assert_eq!(g.facts(&mut serve).unwrap().unwrap().rows, 7);
        assert_eq!(g.facts(|_| panic!("served once")).unwrap().unwrap().rows, 7);
        assert_eq!(calls, 1);
        remove_dir("t/9003");
    }

    #[test]
    fn drop_under_takes_one_database_directory_only() {
        create((1663, 9_101, 7), "base/9101/pgrc_7");
        create((1663, 9_101, 8), "base/9101/pgrc_8");
        create((1663, 91_010, 7), "base/91010/pgrc_7");
        assert_eq!(drop_under("base/9101"), 2);
        assert!(!contains((1663, 9_101, 7)) && !contains((1663, 9_101, 8)));
        assert!(contains((1663, 91_010, 7)));
        assert_eq!(drop_under("base/91010"), 1);
    }

    /// GENID: the used numbers of a directory are the maxima of what its generations held; a database directory
    /// removed takes its tables' numbers, and only its own.
    #[test]
    fn used_numbers_go_with_their_database_directory() {
        {
            let mut c = pgsync::lock(&CATALOG);
            note_used(&mut c, "genid/9401/pgrc_1", 4, 7);
            note_used(&mut c, "genid/94010/pgrc_1", 5, 8);
            note_used(&mut c, "genid/9401/pgrc_1", 2, 9);
        }
        assert_eq!(used_numbers("genid/9401/pgrc_1"), (4, 9), "the max of each");
        remove_under("genid/9401");
        assert_eq!(used_numbers("genid/9401/pgrc_1"), (0, 0));
        assert_eq!(used_numbers("genid/94010/pgrc_1"), (5, 8));
        forget_dir("genid/94010/pgrc_1");
        assert_eq!(used_numbers("genid/94010/pgrc_1"), (0, 0));
    }

    /// OC-5: flag clear and no sidecar is implicit; flag set and none is corruption, as are form bits without the flag.
    /// IX0: a bulk part needs one range holding its rows; a part without ranges (before IX0) has no map. ROWMAP2 M2: a
    /// flagged sidecar is the granules form, the IDs its own (`retired_row_map_forms_are_refused_not_misread`: the rest).
    #[test]
    fn row_maps_follow_the_flag_the_sidecar_and_the_ranges() {
        use pgrc_format::manifest::{PartRecord, PART_FLAG_ROWMAP, PART_ROWMAP_FORM_SHIFT, PART_ROWMAP_GRANULES};
        use pgrc_format::rowid::SEG_EXTENT_IDS;
        use pgrc_format::sidecar::SidecarKind;
        let dir = std::env::temp_dir().join(format!("pgrc_am_rowmap_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_str().unwrap();
        let mut rec = PartRecord { rows: 3, file_len: 0, footer_off: 0, dv_gen: 0, dv_len: 0, part_no: 4, flags: 0, granule_count: 1, band_count: 1, dv_crc: 0, granule_rows: 0 };
        let bulk = [(582, 873)];
        assert!(matches!(load_row_map(d, &rec, &bulk).unwrap(), RowMap::Implicit(582)));
        assert!(load_row_map(d, &rec, &[]).is_err(), "no ranges: no IDs");
        assert!(load_row_map(d, &rec, &[(582, 584)]).is_err(), "a range short of the rows");
        assert!(load_row_map(d, &rec, &[(582, 873), (1164, 1455)]).is_err(), "a bulk part has one range");
        rec.flags = PART_FLAG_ROWMAP | PART_ROWMAP_GRANULES << PART_ROWMAP_FORM_SHIFT;
        assert_eq!(load_row_map(d, &rec, &bulk).err().unwrap().sqlstate(), types_error::ERRCODE_DATA_CORRUPTED, "flagged, no sidecar");
        // a seal's map over two extents: rows (0, 3), (1, 1) and (1030, 7) in TID order
        let (e0, e1) = (291, 291 + 5 * SEG_EXTENT_IDS);
        let ids = [e0 + 2, e0 + 291, e1 + 6 * 291 + 6];
        let body = pgrc_format::rowmap::encode(&ids, pgrc_format::rowmap::Opts::default()).unwrap();
        std::fs::write(dir.join(pgrc_format::dirlayout::sidecar_file_name(4, SidecarKind::RowIdentMap, 1)), pgrc_write::sidecar::sidecar_file_image(SidecarKind::RowIdentMap, [0; 16], 1, &body)).unwrap();
        let ext = [(e0, e0 + SEG_EXTENT_IDS), (e1, e1 + SEG_EXTENT_IDS)];
        let m = load_row_map(d, &rec, &ext).unwrap();
        assert!(matches!(m, RowMap::Granules(_)));
        assert_eq!(resolves_both_ways(&m, 3), ids.to_vec(), "two extents");
        assert_eq!((m.ordinal_of(3, e0 + 3), m.ordinal_of(3, e1 + SEG_EXTENT_IDS)), (None, None));
        rec.rows = 4;
        assert!(load_row_map(d, &rec, &ext).is_err(), "a map short of the part's rows");
        rec.rows = 3;
        rec.flags = PART_ROWMAP_GRANULES << PART_ROWMAP_FORM_SHIFT;
        assert_eq!(load_row_map(d, &rec, &ext).err().unwrap().sqlstate(), types_error::ERRCODE_DATA_CORRUPTED, "a form without the flag");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ROWMAP2 M2 (main 10-07): the map forms before M1 are retired. A seal's unflagged sorted sidecar, a flagged sorted
    /// one, and the runs and pairs a pre-M1 compaction flagged are refused at the load with SQLSTATE 55000, never
    /// decoded; and a legacy seal of one extent (its one range holds its rows, as a bulk part's would) is never read as
    /// an implicit map. The bodies are written byte by byte (the encoders are gone): sorted = `u32 count` and varint
    /// deltas; runs = `u32 count`, then `u64 id_lo, u32 ord_start, u32 len, u8 form` a run; pairs = `u32 rows, u32
    /// nbases`, the `u64` bases, `u8 ix_width, u8 pos_width`, then the packed lanes (LSB first).
    #[test]
    fn retired_row_map_forms_are_refused_not_misread() {
        use pgrc_format::manifest::{PartRecord, PART_FLAG_ROWMAP, PART_ROWMAP_FORM_SHIFT};
        use pgrc_format::rowid::SEG_EXTENT_IDS;
        use pgrc_format::sidecar::SidecarKind;
        let dir = std::env::temp_dir().join(format!("pgrc_am_rowmap_retired_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_str().unwrap();
        let path = dir.join(pgrc_format::dirlayout::sidecar_file_name(4, SidecarKind::RowIdentMap, 1));
        let write = |body: &[u8]| std::fs::write(&path, pgrc_write::sidecar::sidecar_file_image(SidecarKind::RowIdentMap, [0; 16], 1, body)).unwrap();
        let retired = |r: PgResult<RowMap>, what: &str| {
            let Err(e) = r else { panic!("{what}: loaded") };
            assert_eq!(e.sqlstate(), types_error::ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE, "{what}: {}", e.message());
            assert!(e.message().contains("row-ID map format retired (written before ROWMAP2-M1)"), "{what}: {}", e.message());
            assert_eq!(e.hint(), Some("recreate the table (CREATE TABLE AS / COPY)"), "{what}");
        };
        let mut rec = PartRecord { rows: 3, file_len: 0, footer_off: 0, dv_gen: 0, dv_len: 0, part_no: 4, flags: 0, granule_count: 1, band_count: 1, dv_crc: 0, granule_rows: 0 };
        // a pre-M1 seal of one extent: no flag, a sorted body (positions 2, 5, 9)
        let one_extent = [(291, 291 + SEG_EXTENT_IDS)];
        write(&[3, 0, 0, 0, 2, 3, 4]);
        let m = load_row_map(d, &rec, &one_extent);
        assert!(!matches!(m, Ok(RowMap::Implicit(_))), "a legacy seal read as a bulk part's implicit map");
        retired(m, "an unflagged sorted sidecar");
        rec.flags = PART_FLAG_ROWMAP;
        retired(load_row_map(d, &rec, &one_extent), "a flagged sorted sidecar");
        // runs (form 1): one range run, IDs 7000..7003
        let mut runs = vec![1, 0, 0, 0];
        runs.extend(7000u64.to_le_bytes());
        runs.extend(0u32.to_le_bytes());
        runs.extend(3u32.to_le_bytes());
        runs.push(0);
        write(&runs);
        rec.flags = PART_FLAG_ROWMAP | 1 << PART_ROWMAP_FORM_SHIFT;
        retired(load_row_map(d, &rec, &[(7000, 7003)]), "a runs map");
        // pairs (form 2): one base 0, index width 0, positions 7, 3, 5 at width 3 (bits 111 110 101 -> 0x5f 0x01)
        let mut pairs = vec![3, 0, 0, 0, 1, 0, 0, 0];
        pairs.extend(0u64.to_le_bytes());
        pairs.extend([0, 3, 0x5f, 0x01]);
        write(&pairs);
        rec.flags = PART_FLAG_ROWMAP | 2 << PART_ROWMAP_FORM_SHIFT;
        retired(load_row_map(d, &rec, &[]), "a pairs map");
        // a bulk part (no flag, no sidecar) keeps its implicit map
        std::fs::remove_file(&path).unwrap();
        rec.flags = 0;
        assert!(matches!(load_row_map(d, &rec, &[(582, 873)]).unwrap(), RowMap::Implicit(582)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every ordinal's ID resolves back to it; the IDs are unique.
    fn resolves_both_ways(m: &RowMap, rows: u32) -> Vec<u64> {
        let ids: Vec<u64> = (0..rows).map(|o| m.ident(o)).collect();
        for (o, &id) in ids.iter().enumerate() {
            assert_eq!(m.ordinal_of(u64::from(rows), id), Some(o as u32), "ordinal {o}");
        }
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "IDs are unique");
        // ROWMAP2 M1: the range decode and the batch lookup agree with the one-at-a-time calls
        let mut dec = vec![0u64; rows as usize];
        m.decode_ids(0, &mut dec);
        assert_eq!(dec, ids, "decode_ids");
        if rows > 2 {
            let mut tail = vec![0u64; rows as usize - 2];
            m.decode_ids(2, &mut tail);
            assert_eq!(tail, ids[2..], "decode_ids from ordinal 2");
        }
        let mut probe: Vec<u64> = ids.iter().rev().copied().collect();
        probe.push((1 << 34) - 1);
        let mut out = vec![None; probe.len()];
        m.ordinals_of(u64::from(rows), &probe, &mut out);
        let want: Vec<Option<u32>> = probe.iter().map(|&id| m.ordinal_of(u64::from(rows), id)).collect();
        assert_eq!(out, want, "ordinals_of");
        ids
    }

    #[test]
    fn every_form_resolves_both_ways() {
        use pgrc_format::rowmap::{encode, encode_runs, Opts, RevForm};
        // the IDs the retired runs and pairs maps held here before ROWMAP2 M2, now as granules maps
        let runs: [u64; 8] = [9000, 9001, 9002, 10_000, 10_005, 10_000 + (1 << 20), 9100, 9101];
        let m = decode_row_map(&encode(&runs, Opts::default()).unwrap(), 8).unwrap();
        assert_eq!(resolves_both_ways(&m, 8), runs);
        for miss in [8999, 9003, 9099, 9102, 10_001, 10_000 + (1 << 20) + 1, 0, u64::MAX] {
            assert_eq!(m.ordinal_of(8, miss), None, "{miss}");
        }
        assert_eq!(m.ordinal_of(7, 9101), None, "an ordinal past the rows");
        let pairs: [u64; 5] = [(1 << 32) + 100, 7, (1 << 32) + 3, 1 << 20, 2];
        let m = decode_row_map(&encode(&pairs, Opts::default()).unwrap(), 5).unwrap();
        assert_eq!(resolves_both_ways(&m, 5), pairs);
        assert_eq!(m.ordinal_of(5, 4), None);
        let m = RowMap::Implicit(582);
        assert_eq!(resolves_both_ways(&m, 4), vec![582, 583, 584, 585]);
        assert_eq!((m.ordinal_of(4, 581), m.ordinal_of(4, 586)), (None, None), "below the base, past the rows");
        // ROWMAP2 M1: the granules form through the load path's decoder, MONO and each PERM reverse
        let mono = encode_runs(&[(9000, 3), (2000, 2), (40_000, 300)], Opts::default()).unwrap();
        let m = decode_row_map(&mono, 305).unwrap();
        assert!(matches!(&m, RowMap::Granules(g) if g.map().rev_form() == RevForm::Mono));
        assert_eq!(resolves_both_ways(&m, 305)[3..5], [2000, 2001]);
        assert!(decode_row_map(&mono, 306).is_err(), "a map short of the rows");
        let perm: Vec<u64> = (0..3000u64).map(|i| 291 + (i * 7919) % 3000 * 2).collect();
        for f in [RevForm::Fences, RevForm::Labels, RevForm::FrameIx] {
            let m = decode_row_map(&encode(&perm, Opts { force: Some(f) }).unwrap(), 3000).unwrap();
            assert!(matches!(&m, RowMap::Granules(g) if g.map().rev_form() == f), "{f:?}");
            assert_eq!(resolves_both_ways(&m, 3000), perm, "{f:?}");
        }
    }

    /// The ID ranges a test gives part (or birth) `b`.
    fn rng(b: u32) -> (u64, u64) {
        let lo = 291 * (1 + 10 * u64::from(b));
        (lo, lo + 291)
    }

    #[test]
    fn home_follows_ext_ranges() {
        use pgrc_format::manifest::{with_next_row_id, with_ranges, PartRecord};
        let part = |part_no| PartRecord { rows: 3, file_len: 64, footer_off: 0, dv_gen: 0, dv_len: 0, part_no, flags: 0, granule_count: 1, band_count: 1, dv_crc: 0, granule_rows: 0 };
        let header = ManifestHeader { gen: 4, prev_gen: 0, publisher_fxid: 1, relfilenumber: 9, schema_fingerprint: 1, magic: 0, format_version: 0, spc: 1663, db: 5, part_count: 3, next_part_no: 20, flags: 0, reserved: 0 };
        let m = Manifest::new(header, vec![part(2), part(5), part(11)]);
        let plain = Generation::detached(m.clone()).unwrap();
        assert_eq!(plain.home(rng(2).0), None, "a generation without ranges homes no ID");
        // Part 11 is the compaction of parts 3, 4 and 7; part 5 of part 0; part 2 is a bulk part.
        let table = vec![(2, vec![rng(2)]), (5, vec![rng(0)]), (11, vec![rng(3), rng(4), rng(7)])];
        let ext = with_next_row_id(&with_ranges(&[], &table).unwrap(), rng(9).1).unwrap();
        let g = Generation::detached(m.clone().with_ext(ext)).unwrap();
        let at = |b: u32, off: u64| g.home(rng(b).0 + off).and_then(IdHome::part);
        assert_eq!([at(2, 0), at(5, 0), at(11, 0), at(3, 0), at(4, 290), at(7, 1), at(0, 7), at(1, 0)], [Some(0), None, None, Some(2), Some(2), Some(2), Some(1), None]);
        assert_eq!(g.home(rng(4).1), None, "a range's end is not in it");
        assert_eq!(g.ids_of(2), &[rng(3), rng(4), rng(7)]);
        let two_homes = with_next_row_id(&with_ranges(&[], &[(2, vec![rng(3)]), (11, vec![rng(3)])]).unwrap(), rng(9).1).unwrap();
        // the one-home check is the manifest's (Manifest::decode, check_row_ids): every generation is read through it
        assert!(Manifest::decode(&m.with_ext(two_homes).encode()).is_err(), "an ID with two homes");
    }

    /// IX bar B4 (IX.md §Bars, L5): the row-ID directory against part count. Per size, `ranges / 10` parts of ten
    /// compaction-fragmented ranges each (part i homes ranges i, i + P, i + 2P, ...): lookup ns (1,024-lookup batches
    /// of random IDs over the homed space, p50 and p99 of the batch means), the EXT_RANGES bytes and the in-memory
    /// directory bytes per range, and the generation build (decode + directory). A timing run, not a check:
    /// `cargo test -p pgrc_am --profile fast-profile -- --ignored --nocapture b4_` on box 22.
    #[test]
    #[ignore]
    fn b4_directory_lookup_and_build() {
        use pgrc_format::manifest::{with_next_row_id, with_ranges, PartRecord};
        use std::time::Instant;
        const W: u64 = 291 * 64; // one range: 64 rows' worth of IDs, gaps of the same width between ranges
        for &nr in &[1_000usize, 10_000, 100_000] {
            let np = nr / 10;
            let table: Vec<(u32, IdRanges)> = (0..np)
                .map(|p| (p as u32, (0..10).map(|k| {
                    let r = (k * np + p) as u64;
                    (291 + 2 * W * r, 291 + 2 * W * r + W)
                }).collect()))
                .collect();
            let parts: Vec<PartRecord> = (0..np as u32).map(|part_no| PartRecord { rows: 640, file_len: 64, footer_off: 0, dv_gen: 0, dv_len: 0, part_no, flags: 0, granule_count: 1, band_count: 1, dv_crc: 0, granule_rows: 0 }).collect();
            let top = 291 + 2 * W * nr as u64;
            let ranges_only = with_ranges(&[], &table).unwrap();
            let ext = with_next_row_id(&ranges_only, top).unwrap();
            let header = ManifestHeader { gen: 4, prev_gen: 0, publisher_fxid: 1, relfilenumber: 9, schema_fingerprint: 1, magic: 0, format_version: 0, spc: 1663, db: 5, part_count: np as u32, next_part_no: np as u32, flags: 0, reserved: 0 };
            let m = Manifest::new(header, parts).with_ext(ext);
            let mut builds = Vec::new();
            let mut g = None;
            for _ in 0..21 {
                let mc = m.clone();
                let t = Instant::now();
                let x = Generation::detached(mc).unwrap();
                builds.push(t.elapsed().as_secs_f64() * 1e3);
                g = Some(x);
            }
            let g = g.unwrap();
            builds.sort_by(|a, b| a.total_cmp(b));
            let mut s = 0x9E37_79B9_7F4A_7C15u64;
            let mut next = || {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                291 + s % (top - 291)
            };
            let (mut hits, mut batch) = (0usize, Vec::new());
            for _ in 0..2_000 {
                let ids: Vec<u64> = (0..1024).map(|_| next()).collect();
                let t = Instant::now();
                for &id in &ids {
                    hits += usize::from(std::hint::black_box(g.home(std::hint::black_box(id))).is_some());
                }
                batch.push(t.elapsed().as_nanos() as f64 / 1024.0);
            }
            batch.sort_by(|a, b| a.total_cmp(b));
            let pct = |v: &[f64], q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
            let disk = ranges_only.len() as f64 / nr as f64;
            // the directory's entry, and the flat per-part list (`ids`: its range, and `at`'s offset per part) the
            // generation keeps beside it
            let mem_dir = std::mem::size_of::<(u64, u64, u32)>();
            let mem_ids = std::mem::size_of::<(u64, u64)>() as f64 + 4.0 * (np + 1) as f64 / nr as f64;
            println!("B4|ranges={nr}|parts={np}|lookup_p50_ns={:.1}|lookup_p99_ns={:.1}|hit_share={:.3}|ext_bytes_per_range={disk:.1}|mem_dir_bytes_per_range={mem_dir}|mem_ids_bytes_per_range={mem_ids:.1}|build_p50_ms={:.3}|build_max_ms={:.3}",
                pct(&batch, 0.5), pct(&batch, 0.99), hits as f64 / (2_000.0 * 1024.0), pct(&builds, 0.5), builds[builds.len() - 1]);
        }
    }

    /// IXCOPYPATH: a generation's row-ID view (each part's ranges, the part that holds an ID) is its `EXT_RANGES` table
    /// read naively, for random tables: parts with and without an entry, several ranges, IDs out of part order.
    #[test]
    fn the_row_id_view_is_the_ranges_table() {
        use pgrc_format::manifest::{with_next_row_id, with_ranges, PartRecord};
        let mut s = 0x9e37_79b9_7f4a_7c15u64;
        let mut rnd = |n: u64| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s % n.max(1)
        };
        let part = |part_no| PartRecord { rows: 3, file_len: 64, footer_off: 0, dv_gen: 0, dv_len: 0, part_no, flags: 0, granule_count: 1, band_count: 1, dv_crc: 0, granule_rows: 0 };
        for case in 0..300 {
            let (mut nos, mut no) = (Vec::new(), rnd(3) as u32);
            for _ in 0..rnd(40) {
                nos.push(no);
                no += 1 + rnd(3) as u32;
            }
            let mut order: Vec<usize> = (0..nos.len()).collect();
            if rnd(2) == 0 {
                for i in (1..order.len()).rev() {
                    order.swap(i, rnd(i as u64 + 1) as usize);
                }
            }
            let mut next = 291u64;
            let mut table: Vec<(u32, IdRanges)> = Vec::new();
            for i in order {
                if rnd(5) == 0 {
                    continue;
                }
                let mut rs = Vec::new();
                for _ in 0..1 + rnd(3) {
                    let lo = next + 291 * rnd(3);
                    next = lo + 291 * (1 + rnd(4));
                    rs.push((lo, next));
                }
                table.push((nos[i], rs));
            }
            table.sort_by_key(|e| e.0);
            let header = ManifestHeader { gen: 4, prev_gen: 0, publisher_fxid: 1, relfilenumber: 9, schema_fingerprint: 1, magic: 0, format_version: 0, spc: 1663, db: 5, part_count: nos.len() as u32, next_part_no: no, flags: 0, reserved: 0 };
            let ext = with_next_row_id(&with_ranges(&[], &table).unwrap(), next).unwrap();
            let g = Generation::detached(Manifest::new(header, nos.iter().map(|&n| part(n)).collect()).with_ext(ext)).unwrap();
            let entry = |i: usize| table.iter().find(|e| e.0 == nos[i]).map_or(&[][..], |e| &e.1[..]);
            for i in 0..nos.len() {
                assert_eq!(g.ids_of(i), entry(i), "case {case}: part {i}'s ranges");
            }
            let owner = |id: u64| (0..nos.len()).find(|&i| entry(i).iter().any(|&(lo, hi)| lo <= id && id < hi));
            let mut probes = vec![0, 290, 291, next, next + 1, u64::MAX];
            for e in &table {
                for &(lo, hi) in &e.1 {
                    probes.extend([lo - 1, lo, lo + 1, hi - 1, hi]);
                }
            }
            for id in probes {
                assert_eq!(g.home(id).and_then(IdHome::part), owner(id), "case {case}: home of {id}");
            }
        }
    }

    #[test]
    fn deferred_retirements_wait_for_the_horizon() {
        let (a, b) = ((1663, 77, 901), (1663, 77, 902));
        let dir = std::env::temp_dir().join(format!("k1-deferred-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("part-3.pgrc"), b"x").unwrap();
        let d = dir.to_string_lossy().into_owned();
        defer_retire(a, 5, 1000, d.clone(), vec!["part-3.pgrc".into(), "part-4.pgrc".into()]);
        defer_retire(b, 9, 2000, d, Vec::new());
        assert_eq!((deferred_count(a), deferred_count(b)), (1, 1));
        drain_deferred(1000);
        assert_eq!((deferred_count(a), deferred_count(b)), (1, 1), "xid 1000 does not precede horizon 1000");
        assert!(dir.join("part-3.pgrc").exists());
        drain_deferred(1001);
        assert_eq!((deferred_count(a), deferred_count(b)), (0, 1));
        assert!(!dir.join("part-3.pgrc").exists(), "unlinked with the retirement (a missing name is fine)");
        drain_deferred(2001);
        assert_eq!(deferred_count(b), 0);
    }

    /// FREEZEKEEP: a frozen keep's retirement (its `RECLAIM`'s xid 0) is due at any horizon (it waited forever: the
    /// xid order put 0 after every one).
    #[test]
    fn freezekeep_a_frozen_keeps_deferred_retirement_drains_at_once() {
        let a = (1663, 77, 903);
        let dir = std::env::temp_dir().join(format!("freezekeep-deferred-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("manifest-6.pgrcm"), b"x").unwrap();
        defer_retire(a, 7, 0, dir.to_string_lossy().into_owned(), vec!["manifest-6.pgrcm".into()]);
        drain_deferred(3);
        assert_eq!(deferred_count(a), 0, "xid 0 precedes every normal horizon");
        assert!(!dir.join("manifest-6.pgrcm").exists());
    }

    /// D4537 1c (memacct ruling): a composed view starts on its reader's statement; filed in the shared registry it moves to the
    /// part cache, so the first reader's statement ending leaves that account at its base while a second reader holds the view.
    #[test]
    fn a_shared_view_leaves_its_first_readers_statement() {
        let slot = (1663, 5, 9_010);
        let keeper = std::thread::spawn(move || {
            let _serial = crate::session::tests::bind_serial();
            const PROCNO: u32 = 3_947;
            memledger::bind_backend(PROCNO, 9_000 + PROCNO);
            memledger::statement_begin();
            let reader = memledger::charge::context().0;
            let base = memledger::charge::owner_row(reader).committed;
            let header = ManifestHeader { gen: 1, prev_gen: 0, publisher_fxid: 10, relfilenumber: 9, schema_fingerprint: 1, magic: 0, format_version: 0, spc: 1663, db: 5, part_count: 0, next_part_no: 0, flags: 0, reserved: 0 };
            let mut v = Generation::new(Manifest::new(header, Vec::new()), Vec::new(), None, None).unwrap();
            v.into_view();
            v.seal_hash();
            v.refit();
            assert!(memledger::charge::owner_row(reader).committed > base, "a view on its reader is the reader's");
            let shared = shared_view(slot, 1, v);
            assert_eq!(memledger::charge::owner_row(reader).committed, base, "filed in the registry, the view left the reader's account");
            memledger::statement_end();
            memledger::unbind_backend();
            shared
        })
        .join()
        .unwrap();
        let c = pgsync::lock(&shared_charge_of(&keeper));
        assert_eq!(c.owner(), OWNER, "reader B holds a view the part cache owns");
        assert!(c.committed() > 0);
        drop(c);
        forget_views(slot);
    }

    fn shared_charge_of(g: &Generation) -> &pgsync::Mutex<memledger::Charge> {
        &g.charge
    }

    /// D4581: a removed table directory leaves the process-wide maps: its publish mutex once nothing holds it, and its
    /// part keys leave the relid map (no relcache message needed; another part's key under the relid stays); a database's
    /// removal takes every dropped entry's number.
    #[test]
    fn a_removed_directory_leaves_the_publish_mutexes_and_the_relid_part_keys() {
        const DEV: u64 = 0x4B31_4D45_0000_0058;
        let rel = 9_910_000_000 + u64::from(std::process::id());
        let dir = format!("d4581/base/5/pgrc_{rel}");
        let slot = (1663, 5, rel);
        let a = reg_part(DEV, 51, 1);
        let ka = reg_key(&a);
        let relid: Oid = 4_281_000_001;
        install_at(slot, &dir, vec![reg_gen(1, vec![a])], 1);
        crate::inval::record_part_key(relid, ka);
        crate::inval::record_part_key(relid, (u64::MAX, u64::MAX, 7));
        assert_eq!(crate::inval::relid_part_keys(relid), 2);
        let held = crate::inval::publish_lock(rel);
        remove_dir(&dir);
        assert_eq!(crate::inval::relid_part_keys(relid), 1, "the removed entry's part key left the relid map");
        crate::dirpath::remove_table_dir(&dir).expect("drop");
        assert!(crate::inval::publish_lock_kept(rel), "a held mutex stays");
        drop(held);
        crate::dirpath::remove_table_dir(&dir).expect("drop again");
        assert!(!crate::inval::publish_lock_kept(rel), "unheld: the mutex went");
        crate::inval::invalidate_relid(relid);
        assert_eq!(crate::inval::relid_part_keys(relid), 0);
        // DROP DATABASE: a directory created and published this lifetime
        let rel2 = rel + 1;
        let db = "d4581/base/7";
        create((1663, 7, rel2), &format!("{db}/pgrc_{rel2}"));
        drop(crate::inval::publish_lock(rel2));
        assert!(crate::inval::publish_lock_kept(rel2));
        remove_under(db);
        assert!(!crate::inval::publish_lock_kept(rel2), "its unheld publish mutex went with the database");
    }

    /// D4537 T18b: the process-wide registries (the used numbers, the composed views' map, the settle queue) are the part
    /// cache's bytes, refit after each change (compared under each registry's lock, where the refit runs).
    #[test]
    fn the_used_numbers_the_views_registry_and_the_settle_queue_are_on_the_part_cache() {
        let dir = format!("t18b/base/5/pgrc_{}", 9_940_000_000 + u64::from(std::process::id()));
        let slot = (1663, 5, 9_940_000_001);
        {
            let mut c = pgsync::lock(&CATALOG);
            note_used(&mut c, &dir, 3, 9);
        }
        {
            let c = pgsync::lock(&CATALOG);
            let ch = pgsync::lock(&USED_CHARGE);
            assert_eq!((ch.owner(), ch.committed()), (OWNER, c.used_bytes as u64));
            assert!(ch.committed() >= used_entry_bytes(&dir) as u64);
        }
        forget_dir(&dir);
        {
            let c = pgsync::lock(&CATALOG);
            assert_eq!(pgsync::lock(&USED_CHARGE).committed(), c.used_bytes as u64, "a forgotten directory's numbers went");
        }
        queue_settle(slot);
        {
            let s = pgsync::lock(&SETTLE);
            let ch = pgsync::lock(&SETTLE_CHARGE);
            assert_eq!((ch.owner(), ch.committed()), (OWNER, settle_bytes(&s) as u64));
            assert!(ch.committed() > 0);
        }
        let _ = settle_due();
        {
            let s = pgsync::lock(&SETTLE);
            assert_eq!(pgsync::lock(&SETTLE_CHARGE).committed(), settle_bytes(&s) as u64, "a taken queue went");
        }
        let header = ManifestHeader { gen: 1, prev_gen: 0, publisher_fxid: 10, relfilenumber: 9, schema_fingerprint: 1, magic: 0, format_version: 0, spc: 1663, db: 5, part_count: 0, next_part_no: 0, flags: 0, reserved: 0 };
        let mut v = Generation::new(Manifest::new(header, Vec::new()), Vec::new(), None, None).unwrap();
        v.into_view();
        v.seal_hash();
        let shared = shared_view(slot, 1, v);
        {
            let m = pgsync::lock(&VIEWS);
            let ch = pgsync::lock(&VIEWS_CHARGE);
            assert_eq!((ch.owner(), ch.committed()), (OWNER, views_bytes(&m) as u64));
            assert!(ch.committed() > 0);
        }
        drop(shared);
        forget_views(slot);
        let m = pgsync::lock(&VIEWS);
        assert_eq!(pgsync::lock(&VIEWS_CHARGE).committed(), views_bytes(&m) as u64, "a forgotten table's views went");
    }

    /// D4537 T18b: an open part's `Arc` block is the part cache's, shared by every generation that carries it, and goes with
    /// the last count.
    #[test]
    fn an_opened_parts_block_is_on_the_part_cache() {
        let p = reg_part(0x4B31_4D45_0000_0059, 61, 1);
        let want = (2 * size_of::<usize>() + size_of::<OpenedPartBlock>() + p.path.as_bytes_with_nul().len()) as u64;
        assert_eq!((p._block().owner(), p._block().committed()), (OWNER, want));
        let carried = reg_carried(&p);
        let q = p.clone();
        assert_eq!(q._block().committed(), want, "a carried count is the same block, not a second charge");
        drop((p, q, carried));
    }

    /// D4537: a generation's and an entry's bytes are charged to the part cache from the build, refit as they grow.
    #[test]
    fn the_catalogs_bytes_are_on_the_part_cache() {
        let g = gen(1, 0, 10);
        let at = |g: &Generation| {
            let c = pgsync::lock(&g.charge);
            (c.owner(), c.committed())
        };
        let (owner, built) = at(&g);
        assert_eq!((owner, built), (OWNER, g.held_bytes()));
        g.set_image(Arc::new(FactsImage::default()));
        assert!(at(&g).1 > built && at(&g).1 == g.held_bytes(), "the image is the generation's");
        let slot = (1663, 5, 9_009);
        create(slot, "t/9009");
        let entry = || pgsync::lock(&CATALOG).rels.get(&slot).map(|e| (e.charge.owner(), e.charge.committed(), e.held_bytes())).expect("created");
        let (owner, empty, held) = entry();
        assert!(owner == OWNER && empty == held && empty > 0);
        install(slot, vec![g, gen(2, 1, 11)], 2);
        let (_, two, held) = entry();
        assert!(two == held && two > empty, "the generation list is the entry's");
        remove_dir("t/9009");
    }


    /// RR-VIEW: a snapshot that sees `seen` in a transaction that published under `own`.
    struct Snap {
        own: Vec<u64>,
        seen: Vec<u64>,
    }
    impl CommitCheck for Snap {
        fn committed(&self, fxid: u64) -> bool {
            self.own.contains(&fxid) || self.seen.contains(&fxid)
        }
        fn own(&self, fxid: u64) -> bool {
            self.own.contains(&fxid)
        }
    }

    /// A detached generation: `parts` as `(part_no, dv_gen, flags)`, `segs` as `(slot, part_no, state)` (relid 500 +
    /// slot), births as `(compacted part, births)`: IX0 ranges, each part homing [`rng`] of its births, else its own.
    fn gen_of(g: u64, prev: u64, fxid: u64, next: u32, parts: &[(u32, u64, u32)], segs: &[(u16, u32, SegState)], births: &[(u32, Vec<u32>)]) -> Arc<Generation> {
        use pgrc_format::manifest::{encode_segments, with_next_row_id, with_ranges, PartRecord};
        let recs: Vec<PartRecord> = parts.iter().map(|&(part_no, dv_gen, flags)| PartRecord { rows: 3, file_len: 64 + part_no as u64, footer_off: 0, dv_gen, dv_len: 0, part_no, flags, granule_count: 1, band_count: 1, dv_crc: 0, granule_rows: 0 }).collect();
        let header = ManifestHeader { gen: g, prev_gen: prev, publisher_fxid: fxid, relfilenumber: 9, schema_fingerprint: 1, magic: 0, format_version: 0, spc: 1663, db: 5, part_count: recs.len() as u32, next_part_no: next, flags: 0, reserved: 0 };
        let mut table: Vec<(u32, IdRanges)> = parts
            .iter()
            .map(|&(no, ..)| {
                let mut r: IdRanges = births.iter().find(|b| b.0 == no).map_or_else(|| vec![rng(no)], |b| b.1.iter().map(|&x| rng(x)).collect());
                r.sort_unstable();
                (no, r)
            })
            .collect();
        table.sort_unstable_by_key(|e| e.0);
        let mut ext = with_next_row_id(&with_ranges(&[], &table).unwrap(), rng(1000).1).unwrap();
        let segs: Vec<SegmentRecord> = segs.iter().map(|&(slot, part_no, state)| SegmentRecord { slot, part_no, relid: 500 + slot as u32, state, fxid }).collect();
        if !segs.is_empty() {
            encode_segments(&segs, &mut ext);
        }
        Arc::new(Generation::detached(Manifest::new(header, recs).with_ext(ext)).unwrap())
    }

    fn view(slot: RelSlot, own: &[u64], seen: &[u64]) -> Arc<Generation> {
        match visible(slot, &Snap { own: own.to_vec(), seen: seen.to_vec() }).unwrap() {
            Lookup::Gen(g) => g,
            _ => panic!("a generation"),
        }
    }

    fn part_nos(g: &Generation) -> Vec<(u32, u64)> {
        g.manifest.parts.iter().map(|p| (p.part_no, p.dv_gen)).collect()
    }

    fn segs_of(g: &Generation) -> Vec<(u16, u32, SegState)> {
        g.segments.iter().map(|r| (r.slot, r.part_no, r.state)).collect()
    }

    /// The bug (PUBLISH-WAIT-design.md "Risks"): an own bulk publish chained on another transaction's generation
    /// committed after the snapshot carried that generation's part into the reader's view.
    #[test]
    fn an_own_publish_over_a_post_snapshot_generation_sees_the_snapshot_plus_its_own_parts() {
        let slot = (1663, 5, 9_101);
        let g1 = gen_of(1, 0, 10, 1, &[(0, 0, 0)], &[], &[]);
        let g2 = gen_of(2, 1, 11, 2, &[(0, 0, 0), (1, 0, 0)], &[], &[]);
        let g3 = gen_of(3, 2, 20, 3, &[(0, 0, 0), (1, 0, 0), (2, 0, 0)], &[], &[]);
        let g4 = gen_of(4, 3, 20, 4, &[(0, 0, 0), (1, 0, 0), (2, 0, 0), (3, 0, 0)], &[], &[]);
        install(slot, vec![g1, g2, g3], 3);
        let v = view(slot, &[20], &[10]);
        assert!(v.composed() && v.gen == 3 && v.view_key() & 1 << 63 != 0, "the own generation's number, a view of its own key");
        assert_eq!(part_nos(&v), vec![(0, 0), (2, 0)], "not part 1: generation 2 committed after the snapshot");
        let whole = view(slot, &[20], &[10, 11]);
        assert!(!whole.composed() && whole.gen == 3 && whole.view_key() == 3, "the snapshot sees what it chained on");
        assert_eq!(part_nos(&whole), vec![(0, 0), (1, 0), (2, 0)]);
        let writer = match visible(slot, &Committed(vec![10, 11, 20])).unwrap() {
            Lookup::Gen(g) => g,
            _ => unreachable!(),
        };
        assert!(!writer.composed() && part_nos(&writer).len() == 3, "no own(): the newest committed-or-own generation whole");
        assert_eq!(view(slot, &[], &[10]).gen, 1, "a reader without publishes: its snapshot's generation");
        // two own publishes: both installed parts
        install(slot, vec![gen_of(1, 0, 10, 1, &[(0, 0, 0)], &[], &[]), gen_of(2, 1, 11, 2, &[(0, 0, 0), (1, 0, 0)], &[], &[]), gen_of(3, 2, 20, 3, &[(0, 0, 0), (1, 0, 0), (2, 0, 0)], &[], &[]), g4], 4);
        assert_eq!(part_nos(&view(slot, &[20], &[10])), vec![(0, 0), (2, 0), (3, 0)]);
        // the snapshot sees no generation (the table's first publish came after it)
        assert_eq!(part_nos(&view(slot, &[20], &[])), vec![(2, 0), (3, 0)]);
        // an aborted generation on the walk is skipped
        install(slot, vec![gen_of(1, 0, 10, 1, &[(0, 0, 0)], &[], &[]), gen_of(2, 1, 11, 2, &[(0, 0, 0), (1, 0, 0)], &[], &[]), gen_of(3, 2, 20, 3, &[(0, 0, 0), (1, 0, 0), (2, 0, 0)], &[], &[]), gen_of(4, 3, 30, 4, &[(0, 0, 0), (1, 0, 0), (2, 0, 0), (3, 0, 0)], &[], &[])], 4);
        assert_eq!((view(slot, &[20], &[10]).gen, part_nos(&view(slot, &[20], &[10]))), (3, vec![(0, 0), (2, 0)]));
        remove_dir("t/9101");
    }

    /// SEAL-OFFHEAD: a maintenance seal stacked between two of the reader's own publishes (A publishes, X seals rows
    /// committed after A's snapshot on top of A's generation, A publishes again): A's view is its snapshot's base plus
    /// its own parts, with the segment read as a heap under the snapshot. Not the newest own generation whole, which
    /// carries X's seal part (the contiguity check; before it `visible` returned generation 4 as it stands).
    #[test]
    fn a_seal_stacked_between_own_publishes_stays_out_of_the_own_view() {
        use SegState::*;
        let slot = (1663, 5, 9_130);
        let g1 = gen_of(1, 0, 10, 6, &[(0, 0, 0)], &[(0, 5, Sealing)], &[]);
        let g2 = gen_of(2, 1, 20, 7, &[(0, 0, 0), (6, 0, 0)], &[(0, 5, Sealing)], &[]);
        let g3 = gen_of(3, 2, 30, 7, &[(0, 0, 0), (5, 0, 0), (6, 0, 0)], &[(0, 5, Sealed)], &[]);
        let g4 = gen_of(4, 3, 20, 8, &[(0, 0, 0), (5, 0, 0), (6, 0, 0), (7, 0, 0)], &[(0, 5, Sealed)], &[]);
        install(slot, vec![g1, g2, g3, g4], 4);
        let v = view(slot, &[20], &[10]);
        assert!(v.composed() && v.gen == 4, "a composed own view");
        assert_eq!(part_nos(&v), vec![(0, 0), (6, 0), (7, 0)], "not the seal's part 5: X committed after the snapshot");
        assert_eq!(segs_of(&v), vec![(0, 5, Sealing)], "segment 5 is read under the snapshot");
        let seen = view(slot, &[20], &[10, 30]);
        assert_eq!(part_nos(&seen), vec![(0, 0), (5, 0), (6, 0), (7, 0)], "a snapshot that sees the seal reads its part");
        assert_eq!(segs_of(&seen), vec![(0, 5, Sealed)]);
        remove_dir("t/9130");
    }

    fn tagged(g: Arc<Generation>, tags: &[(u32, u64)]) -> Arc<Generation> {
        let e: Vec<UnderEntry> = tags.iter().map(|&(part_no, fxid)| UnderEntry { part_no, fxid }).collect();
        let m = &g.manifest;
        let ext = pgrc_format::manifest::with_under(&m.ext, &e).unwrap();
        Arc::new(Generation::detached(Manifest::new(m.header, m.parts.clone()).with_ext(ext)).unwrap())
    }

    /// SEAL-OFFHEAD: X's seal (fxid 30) stacked on A's bulk publish (fxid 40, in progress), A's part 2 tagged in X's
    /// generation. Each reader resolves A's part by its own snapshot: hidden (a composed view) unless it sees A; the
    /// generation itself once A is seen; A's own reader keeps its part, composed while A is unsettled.
    #[test]
    fn a_stacked_generation_hides_the_parts_of_creators_the_snapshot_does_not_see() {
        use SegState::*;
        let slot = (1663, 5, 9_140);
        let g1 = gen_of(1, 0, 10, 2, &[(0, 0, 0)], &[(0, 1, Sealing)], &[]);
        let g2 = gen_of(2, 1, 40, 3, &[(0, 0, 0), (2, 0, 0)], &[(0, 1, Sealing)], &[]);
        let g3 = tagged(gen_of(3, 2, 30, 3, &[(0, 0, 0), (1, 0, 0), (2, 0, 0)], &[(0, 1, Sealed)], &[]), &[(2, 40)]);
        assert_eq!(g3.manifest.header.flags & pgrc_format::manifest::MANIFESTF_UNDER, pgrc_format::manifest::MANIFESTF_UNDER);
        install(slot, vec![g1, g2, g3], 3);
        let x = view(slot, &[], &[10, 30]);
        assert!(x.composed() && x.gen == 3 && x.stacked() == 1, "the seal without A's part");
        assert_eq!((part_nos(&x), segs_of(&x)), (vec![(0, 0), (1, 0)], vec![(0, 1, Sealed)]));
        assert!(x.under().is_empty() && x.manifest.header.flags & pgrc_format::manifest::MANIFESTF_UNDER == 0, "a view carries no tags");
        let both = view(slot, &[], &[10, 30, 40]);
        assert!(!both.composed() && both.gen == 3 && part_nos(&both).len() == 3, "A seen: generation 3 as published");
        let neither = view(slot, &[], &[10]);
        assert!(!neither.composed() && neither.gen == 1, "neither: the committed base, segment 1 read as a heap");
        let a_before = view(slot, &[40], &[10]);
        assert!(!a_before.composed() && a_before.gen == 2, "A before X's commit: its own generation");
        let a_after = view(slot, &[40], &[10, 30]);
        assert!(a_after.composed() && a_after.stacked() == 1 && part_nos(&a_after).len() == 3, "A after X's commit: all three, unsettled");
        assert_ne!(x.view_key(), a_after.view_key(), "two contents of generation 3, two keys");
        assert_eq!(x.view_key(), view(slot, &[], &[10, 30]).view_key(), "one content, one key");
        let w = match visible(slot, &Committed(vec![10, 30])).unwrap() {
            Lookup::Gen(g) => g,
            _ => unreachable!(),
        };
        assert_eq!(part_nos(&w), vec![(0, 0), (1, 0)], "a writer's no-snapshot lookup hides the uncommitted part too");
        remove_dir("t/9140");
    }

    /// SEAL-OFFHEAD x RR-VIEW: a reader's own publish (fxid 20) chained on a stacked generation: the composed own view
    /// leaves out the parts of a creator (40) its snapshot does not see, carried in both the base and its own.
    #[test]
    fn an_own_view_over_a_stacked_generation_leaves_out_hidden_creators() {
        let slot = (1663, 5, 9_141);
        let g1 = gen_of(1, 0, 10, 2, &[(0, 0, 0)], &[], &[]);
        let g2 = gen_of(2, 1, 40, 3, &[(0, 0, 0), (2, 0, 0)], &[], &[]);
        let g3 = tagged(gen_of(3, 2, 30, 4, &[(0, 0, 0), (2, 0, 0)], &[(0, 3, SegState::Active)], &[]), &[(2, 40)]);
        let g4 = tagged(gen_of(4, 3, 20, 5, &[(0, 0, 0), (2, 0, 0), (4, 0, 0)], &[(0, 3, SegState::Active)], &[]), &[(2, 40)]);
        install(slot, vec![g1, g2, g3, g4], 4);
        let v = view(slot, &[20], &[10, 30]);
        assert_eq!(part_nos(&v), vec![(0, 0), (4, 0)], "the own part, not A's");
        let v = view(slot, &[20], &[10]);
        assert_eq!(part_nos(&v), vec![(0, 0), (4, 0)], "a snapshot before the rotation too");
        remove_dir("t/9141");
    }

    /// CONCLOAD unit 1: X's seal (30) stacked over C's bulk publish (60, in progress then), C's part 2 tagged; C commits
    /// after A's REPEATABLE READ snapshot and before A's own publish (20), which drops the committed creator's tag, so
    /// A's generation lists part 2 untagged. A's own view still hides it by the base's tag: its snapshot does not see C.
    #[test]
    fn an_own_view_hides_what_its_base_tags_for_a_creator_committed_after_the_snapshot() {
        let slot = (1663, 5, 9_142);
        let g1 = gen_of(1, 0, 10, 2, &[(0, 0, 0)], &[], &[]);
        let g2 = gen_of(2, 1, 60, 3, &[(0, 0, 0), (2, 0, 0)], &[], &[]);
        let g3 = tagged(gen_of(3, 2, 30, 4, &[(0, 0, 0), (2, 0, 0), (3, 0, 0)], &[], &[]), &[(2, 60)]);
        let g4 = gen_of(4, 3, 20, 5, &[(0, 0, 0), (2, 0, 0), (3, 0, 0), (4, 0, 0)], &[], &[]);
        install(slot, vec![g1, g2, g3, g4], 4);
        let v = view(slot, &[20], &[10, 30]);
        assert!(v.composed() && v.gen == 4, "a composed own view");
        assert_eq!(part_nos(&v), vec![(0, 0), (3, 0), (4, 0)], "not C's part 2: C committed after the snapshot");
        let older = view(slot, &[20], &[10, 30, 60]);
        assert!(!older.composed() && older.gen == 4 && part_nos(&older).len() == 4, "a snapshot that sees C: generation 4 whole");
        remove_dir("t/9142");
    }

    /// CONCLOAD: another loader (50) stacked between two of the reader's own bulk publishes and committed after the
    /// reader's snapshot. The own view takes the parts each own generation added over the one it chained on, not every
    /// part numbered past the base: part 2 stays out.
    #[test]
    fn an_own_view_leaves_out_a_loader_stacked_between_own_publishes() {
        let slot = (1663, 5, 9_143);
        let g1 = gen_of(1, 0, 10, 1, &[(0, 0, 0)], &[], &[]);
        let g2 = gen_of(2, 1, 20, 2, &[(0, 0, 0), (1, 0, 0)], &[], &[]);
        let g3 = tagged(gen_of(3, 2, 50, 3, &[(0, 0, 0), (1, 0, 0), (2, 0, 0)], &[], &[]), &[(1, 20)]);
        let g4 = gen_of(4, 3, 20, 4, &[(0, 0, 0), (1, 0, 0), (2, 0, 0), (3, 0, 0)], &[], &[]);
        install(slot, vec![g1, g2, g3, g4], 4);
        let v = view(slot, &[20], &[10]);
        assert!(v.composed() && v.gen == 4, "a composed own view");
        assert_eq!(part_nos(&v), vec![(0, 0), (1, 0), (3, 0)], "not part 2: its loader committed after the snapshot");
        let sees = view(slot, &[20], &[10, 50]);
        assert!(!sees.composed() && sees.gen == 4 && part_nos(&sees).len() == 4, "a snapshot that sees 50: generation 4 whole");
        remove_dir("t/9143");
    }

    /// RR-VIEW x P-1 (coordinator 09-26): a composed view's stats, image and answers are its own object's, from its own
    /// parts (none open in a detached view, so none here), never the generation's of the same number; the generation of
    /// that number keeps its answers, and ANALYZE's answers lookup does not serve the view's manifest with them.
    #[test]
    fn a_composed_view_carries_no_stats_and_the_generation_of_its_number_keeps_its_answers() {
        let slot = (1663, 5, 9_120);
        let g3 = gen_of(3, 2, 20, 3, &[(0, 0, 0), (1, 0, 0), (2, 0, 0)], &[], &[]);
        g3.set_answers(Arc::new(Answers::new(3, Vec::new()).unwrap()));
        install(slot, vec![gen_of(1, 0, 10, 1, &[(0, 0, 0)], &[], &[]), gen_of(2, 1, 11, 2, &[(0, 0, 0), (1, 0, 0)], &[], &[]), Arc::clone(&g3)], 3);
        let v = view(slot, &[20], &[10]);
        assert!(v.composed() && v.gen == 3 && part_nos(&v) == vec![(0, 0), (2, 0)], "generation 3's number, two of its parts");
        assert!(v.stats.is_none() && v.image().is_none() && v.answers().is_none(), "a composed view carries no stats");
        assert!(v.facts(|_| unreachable!()).unwrap().is_none(), "no image: no facts");
        assert_eq!(g3.answers().map(|a| a.parts()), Some(3), "generation 3's answers are its own, for its three parts");
        assert!(crate::analyze::catalog_answers(slot, &v.manifest).is_none(), "the view's manifest is not generation 3's");
        assert!(crate::analyze::catalog_answers(slot, &g3.manifest).is_some(), "generation 3's own manifest is served");
        remove_dir("t/9120");
    }

    /// A seal, compile and rotation by another transaction after the snapshot, then the reader's own bulk load, own
    /// rotation and own seal (pgrc_flush in the transaction): the pre-snapshot front rows stay in their segment, read
    /// under the snapshot; the newest DVs apply; the segment the reader sealed is read as a heap too.
    #[test]
    fn seals_after_the_snapshot_leave_the_view_on_its_segments_and_the_newest_dvs_apply() {
        use SegState::*;
        let slot = (1663, 5, 9_102);
        let g1 = gen_of(1, 0, 10, 2, &[(0, 0, 0)], &[(0, 1, Active)], &[]);
        let g2 = gen_of(2, 1, 11, 3, &[(0, 2, 0), (1, 0, 0)], &[(0, 1, Sealed), (1, 2, Active)], &[]);
        let g3 = gen_of(3, 2, 20, 4, &[(0, 2, 0), (1, 0, 0), (3, 0, 0)], &[(0, 1, Sealed), (1, 2, Active)], &[]);
        let g4 = gen_of(4, 3, 20, 5, &[(0, 2, 0), (1, 0, 0), (2, 0, 0), (3, 0, 0)], &[(0, 1, Sealed), (1, 2, Sealed), (2, 4, Active)], &[]);
        install(slot, vec![g1, g2, g3], 3);
        let v = view(slot, &[20], &[10]);
        assert_eq!(part_nos(&v), vec![(0, 2), (3, 0)], "part 0 with the compile's DV; not the seal part 1");
        assert_eq!(segs_of(&v), vec![(0, 1, Active)], "the base's segments: slot 0 is read as a heap");
        install(slot, vec![gen_of(1, 0, 10, 2, &[(0, 0, 0)], &[(0, 1, Active)], &[]), gen_of(2, 1, 11, 3, &[(0, 2, 0), (1, 0, 0)], &[(0, 1, Sealed), (1, 2, Active)], &[]), gen_of(3, 2, 20, 4, &[(0, 2, 0), (1, 0, 0), (3, 0, 0)], &[(0, 1, Sealed), (1, 2, Active)], &[]), g4], 4);
        let v = view(slot, &[20], &[10]);
        assert_eq!(part_nos(&v), vec![(0, 2), (3, 0)], "not the own seal part 2 either");
        assert_eq!(segs_of(&v), vec![(0, 1, Active), (1, 2, Sealing)], "the segment the reader sealed is read under the snapshot");
        remove_dir("t/9102");
    }

    /// Compactions: another transaction's after the snapshot is not in the view (its inputs are, from the base); the
    /// reader's own compaction of its own parts and of the base's keeps the inputs.
    #[test]
    fn compaction_outputs_stay_out_and_their_inputs_stay_in() {
        let slot = (1663, 5, 9_103);
        let c = pgrc_format::manifest::PART_FLAG_COMPACTED;
        let g1 = gen_of(1, 0, 10, 2, &[(0, 0, 0), (1, 0, 0)], &[], &[]);
        let g2 = gen_of(2, 1, 11, 3, &[(2, 0, c)], &[], &[(2, vec![0, 1])]);
        let g3 = gen_of(3, 2, 20, 4, &[(2, 0, c), (3, 0, 0)], &[], &[(2, vec![0, 1])]);
        let g4 = gen_of(4, 3, 20, 5, &[(2, 0, c), (3, 0, 0), (4, 0, 0)], &[], &[(2, vec![0, 1])]);
        let g5 = gen_of(5, 4, 20, 6, &[(2, 0, c), (5, 0, c)], &[], &[(2, vec![0, 1]), (5, vec![3, 4])]);
        install(slot, vec![g1, g2, g3, g4, g5], 5);
        let v = view(slot, &[20], &[10]);
        assert_eq!(part_nos(&v), vec![(0, 0), (1, 0), (3, 0), (4, 0)]);
        assert_eq!([0, 1, 3, 4, 2, 5].map(|b| v.home(rng(b).0).and_then(IdHome::part)), [Some(0), Some(1), Some(2), Some(3), None, None], "each row at the part that holds it");
        assert_eq!((0..4).map(|i| v.ids_of(i).to_vec()).collect::<Vec<_>>(), [0, 1, 3, 4].map(|b| vec![rng(b)]), "no compaction output in the view: each part its own range");
        remove_dir("t/9103");
    }

    /// The stacked head of `a_stacked_generation_hides_the_parts_of_creators_the_snapshot_does_not_see`: X's seal (30)
    /// over A's bulk publish (40), A's part 2 tagged in generation 3.
    fn stacked_head(slot: RelSlot) {
        use SegState::*;
        let g1 = gen_of(1, 0, 10, 2, &[(0, 0, 0)], &[(0, 1, Sealing)], &[]);
        let g2 = gen_of(2, 1, 40, 3, &[(0, 0, 0), (2, 0, 0)], &[(0, 1, Sealing)], &[]);
        let g3 = tagged(gen_of(3, 2, 30, 3, &[(0, 0, 0), (1, 0, 0), (2, 0, 0)], &[(0, 1, Sealed)], &[]), &[(2, 40)]);
        install(slot, vec![g1, g2, g3], 3);
    }

    /// COMPOSED: a composed view's key is interned per content: one content resolved again is the same `Arc` and key
    /// (kept, or held by a reader across a head move: positions compare keys within a statement), another content of
    /// the same number another key, and a content no reader holds any more comes back under a new key, never a reused
    /// one. No key is a generation number (top bit).
    #[test]
    fn composed_view_keys_are_interned_per_content() {
        let slot = (1663, 5, 9_150);
        stacked_head(slot);
        let x = view(slot, &[], &[10, 30]);
        let again = view(slot, &[], &[10, 30]);
        assert!(Arc::ptr_eq(&x, &again) && x.view_key() == again.view_key(), "one content: one view, one key");
        let a = view(slot, &[40], &[10, 30]);
        assert!(x.view_key() != a.view_key() && x.gen == a.gen, "another content of generation 3: another key");
        assert!(x.view_key() & 1 << 63 != 0 && a.view_key() & 1 << 63 != 0, "never a generation's number");
        drop_views(slot);
        let held = view(slot, &[], &[10, 30]);
        assert!(Arc::ptr_eq(&x, &held), "a view a reader holds keeps its key after the kept ones left (a head move)");
        let k = x.view_key();
        drop((x, again, held, a));
        drop_views(slot);
        assert_eq!(live_views(slot), 0, "no reader holds a view, none kept");
        let fresh = view(slot, &[], &[10, 30]);
        assert!(fresh.view_key() != k && fresh.view_key() & 1 << 63 != 0, "a content resolved again after every holder left: a new key");
        remove_dir("t/9150");
    }

    /// COMPOSED cost guard: a lookup that returns a published generation (untagged, or tagged with every creator seen)
    /// never reaches the view registry's compare (`shared_view`); a composed lookup does, once.
    #[test]
    fn a_plain_lookup_never_touches_the_view_registry() {
        let slot = (1663, 5, 9_151);
        let plain = (1663, 5, 9_152);
        install(plain, vec![gen_of(1, 0, 10, 1, &[(0, 0, 0)], &[], &[]), gen_of(2, 1, 11, 2, &[(0, 0, 0), (1, 0, 0)], &[], &[])], 2);
        stacked_head(slot);
        let n0 = shared_view_calls();
        for _ in 0..100 {
            assert!(!view(plain, &[], &[10, 11]).composed());
            assert!(!view(plain, &[11], &[10]).composed(), "an own generation chained on the base");
            assert!(!view(slot, &[], &[10, 30, 40]).composed(), "a tagged head whose creators are all seen");
            assert!(!view(slot, &[], &[10]).composed(), "an older committed generation");
        }
        assert_eq!(shared_view_calls(), n0, "400 plain lookups, no view registry visit");
        let v = view(slot, &[], &[10, 30]);
        assert!(v.composed());
        assert_eq!(shared_view_calls(), n0 + 2, "a composed lookup: the kept lookup misses, the view is built and kept");
        assert!(Arc::ptr_eq(&v, &view(slot, &[], &[10, 30])));
        assert_eq!(shared_view_calls(), n0 + 3, "again: found kept before it is built");
        remove_dir("t/9151");
        remove_dir("t/9152");
    }

    /// COMPOSED: the views kept under a head leave when a fresh reader resolves that head whole (every tag's creator
    /// committed): once their readers finish, the process holds none.
    #[test]
    fn kept_views_leave_once_a_fresh_reader_sees_the_head_whole() {
        let slot = (1663, 5, 9_153);
        stacked_head(slot);
        drop((view(slot, &[], &[10, 30]), view(slot, &[40], &[10, 30])));
        assert_eq!(live_views(slot), 2, "two views kept, no reader holds them");
        assert!(!view(slot, &[], &[10, 30, 40]).composed());
        assert_eq!(live_views(slot), 0, "a fresh reader saw generation 3 whole: the kept views left");
        let old = view(slot, &[], &[10, 30]);
        assert!(!view(slot, &[], &[10, 30, 40]).composed());
        assert_eq!(live_views(slot), 1, "a view its reader still holds stays alive");
        drop(old);
        assert_eq!(live_views(slot), 0);
        remove_dir("t/9153");
    }

    /// COMPOSED: a reader's state of a view is built once when two readers ask at once; both get the one build.
    #[test]
    fn a_views_reader_state_is_built_once_by_concurrent_readers() {
        let slot = (1663, 5, 9_154);
        stacked_head(slot);
        let v = view(slot, &[], &[10, 30]);
        let builds = std::sync::atomic::AtomicUsize::new(0);
        let gate = std::sync::Barrier::new(2);
        let got: Vec<Arc<u64>> = std::thread::scope(|s| {
            let hs: Vec<_> = (0..2)
                .map(|_| {
                    s.spawn(|| {
                        gate.wait();
                        v.reader_state(|| {
                            builds.fetch_add(1, Ordering::Relaxed);
                            std::thread::sleep(std::time::Duration::from_millis(50));
                            Arc::new(v.view_key())
                        })
                        .expect("one type")
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(builds.load(Ordering::Relaxed), 1, "one build");
        assert!(Arc::ptr_eq(&got[0], &got[1]) && *got[0] == v.view_key());
        assert!(v.reader_state(|| Arc::new(0u32)).is_none(), "another type: none, the slot keeps its state");
        remove_dir("t/9154");
    }

    /// COMPOSED (measure, `--ignored --nocapture`): a lookup over a 2,000-part generation, plain (the published
    /// generation) and composed (one tagged part hidden: the view built, then found among the kept ones).
    #[test]
    #[ignore]
    fn view_build_cost_2000_parts() {
        let slot = (1663, 5, 9_155);
        let parts: Vec<(u32, u64, u32)> = (0..2_000).map(|n| (n, 0, 0)).collect();
        let base = gen_of(1, 0, 10, 1_999, &parts[..1_999], &[], &[]);
        let top = tagged(gen_of(2, 1, 30, 2_000, &parts, &[], &[]), &[(1_999, 40)]);
        install(slot, vec![base, top], 2);
        let time = |own: &[u64], seen: &[u64], n: u32| {
            let t0 = std::time::Instant::now();
            for _ in 0..n {
                std::hint::black_box(view(slot, own, seen));
            }
            t0.elapsed().as_nanos() as f64 / n as f64
        };
        let plain = time(&[], &[10, 30, 40], 2_000);
        drop_views(slot);
        let first = time(&[], &[10, 30], 1);
        let kept = time(&[], &[10, 30], 500);
        let mut fresh = 0.0;
        for _ in 0..50 {
            drop_views(slot);
            fresh += time(&[], &[10, 30], 1);
        }
        println!("COMPOSED view_build_cost_2000_parts: plain {plain:.0} ns; composed first {first:.0} ns, again (kept) {kept:.0} ns, rebuilt after a release {:.0} ns", fresh / 50.0);
        remove_dir("t/9155");
    }

    /// SORTRUNS-E0 L1 (was L0's refusal): a generation whose sorted run homes its IDs loads, and those IDs' home is the
    /// run; a self-homed run's members home their own.
    #[test]
    fn sortruns_l1_loads_a_generation_whose_runs_home_ids() {
        use pgrc_format::manifest::{with_next_row_id, with_ranges};
        use pgrc_format::sortrun::{with_runs, RankForm, RunRevForm, SortRun};
        let recs: Vec<PartRecord> = (1..=2).map(|part_no| PartRecord { rows: 3, file_len: 64, footer_off: 0, dv_gen: 0, dv_len: 0, part_no, flags: 0, granule_count: 1, band_count: 1, dv_crc: 0, granule_rows: 0 }).collect();
        let header = ManifestHeader { gen: 4, prev_gen: 3, publisher_fxid: 14, relfilenumber: 9, schema_fingerprint: 1, magic: 0, format_version: 0, spc: 1663, db: 5, part_count: 2, next_part_no: 8, flags: 0, reserved: 0 };
        let run = |rev: RunRevForm, ranges: Vec<(u64, u64)>| SortRun { run_id: 4, level: 1, rev, rank: RankForm::Dense, rows: 3, rev_len: if ranges.is_empty() { 0 } else { 4096 }, rev_crc: 0, members: vec![1, 2], ranges };
        let ext = with_next_row_id(&[], 291 * 10).unwrap();
        let homing = Manifest::new(header, recs.clone()).with_ext(with_runs(&ext, &[run(RunRevForm::Labels, vec![(291, 582)])]).unwrap());
        let g = Generation::detached(homing).unwrap();
        assert_eq!((g.home(291), g.home(581), g.home(582)), (Some(IdHome::Run(0)), Some(IdHome::Run(0)), None));
        assert_eq!((g.run_member(0), g.run_member(1)), (Some((0, 0)), Some((0, 1))));
        assert!(g.pinned_member(0) && g.pinned_member(1), "members of a run with others");
        let selfhomed = Manifest::new(header, recs).with_ext(with_runs(&with_ranges(&ext, &[(1, vec![(291, 582)])]).unwrap(), &[run(RunRevForm::None, vec![])]).unwrap());
        let g = Generation::detached(selfhomed).unwrap();
        assert_eq!((g.home(300), g.run_member(0)), (Some(IdHome::Part(0)), None), "a self-homed run's member homes its own");
    }
}
