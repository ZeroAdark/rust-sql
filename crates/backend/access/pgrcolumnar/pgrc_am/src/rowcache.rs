//! F2-2: the per-backend cache of decoded granules a refetch by position reads (UPDATE's old row, an EPQ rowmark
//! source, a row trigger's tuple). All columns of a granule, keyed by `(relid, gen, part, granule)`, two entries per
//! relation. A scan of an armed relation offers each granule as it leaves it: its decoded buffers are swapped in and
//! a spare set swapped back, so nothing is copied or allocated once warm. A miss decodes the granule here: through
//! `Residency::Private` cursors when a bulk scan of the relation began in the statement (PostgreSQL's BAS_BULKREAD
//! posture: a bulk operation never floods the cache), else `Cached` ones, bounded by the decoded-column cap. Each
//! `ColBuf` carries the guard over what its datums alias (C1 L5), so an entry needs nothing else to stay valid.

use std::cell::RefCell;
use std::sync::{Arc, Weak};

use mcx::Mcx;
use pgrc_format::class::ColSchema;
use pgrc_read::{OpenPart, Residency};
use types_core::Oid;
use types_error::{PgError, PgResult};
use types_slot::SlotData;

use crate::catalog::OpenedPart;
use crate::scan::{codec_binding, decode_granule, store_cols, ColBuf, PartCursors, SliceBuf};

/// A granule of a generation's part (`part` is the index in manifest order).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GranuleKey {
    /// The view's `Generation::view_key` (a composed own view's part indexes are not its generation's).
    pub gen: u64,
    pub part: u32,
    pub granule: u32,
}

/// Entries kept per relation: the granule being read and the one before it.
const PER_REL: usize = 2;

struct Entry {
    relid: Oid,
    key: GranuleKey,
    rows: u32,
    cols: Vec<ColBuf>,
}

#[derive(Default)]
struct Cache {
    armed: Vec<Oid>,
    /// Oldest first.
    entries: Vec<Entry>,
    /// C1 L5: relations a bulk scan of this statement reads (`scan::read_residency`); their refetch misses read
    /// `Private`.
    bulk: Vec<Oid>,
    /// C1 L5: per relation, the cursors of the part its refetch misses last read, kept until another part's miss (a
    /// bulk statement's `Private` refetches re-read an extent only when they leave it).
    cursors: Vec<(Oid, PartCursors)>,
}

thread_local! {
    static CACHE: RefCell<Cache> = RefCell::new(Cache::default());
}

/// Scans of `relid` offer their granules until [`disarm_all`] (a DML target, an EPQ rowmark source).
pub fn arm(relid: Oid) {
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if !c.armed.contains(&relid) {
            c.armed.push(relid);
        }
    });
}

/// Statement end: no relation is armed and the decoded granules are dropped (IX1 L0: the column cache's too).
pub fn disarm_all() {
    CACHE.with(|c| *c.borrow_mut() = Cache::default());
    COLS.with(|c| {
        if let Ok(mut c) = c.try_borrow_mut() {
            c.clear();
        }
    });
}

/// C1 L5: a bulk scan of `relid` began (until [`disarm_all`]).
pub(crate) fn note_bulk(relid: Oid) {
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if !c.bulk.contains(&relid) {
            c.bulk.push(relid);
        }
    });
}

/// A refetch miss's residency: `Private` in a statement whose scan of `relid` is bulk.
fn refetch_residency(c: &Cache, relid: Oid) -> Residency {
    if c.bulk.contains(&relid) {
        Residency::Private
    } else {
        Residency::Cached
    }
}

/// `relid`'s refetch cursors, (re)opened on `part` under the statement's residency.
fn cursors_for<'c>(c: &'c mut Cache, relid: Oid, part: &Arc<OpenPart>) -> &'c mut PartCursors {
    let residency = refetch_residency(c, relid);
    let i = c.cursors.iter().position(|(r, _)| *r == relid).unwrap_or_else(|| {
        c.cursors.push((relid, PartCursors::new(part, residency)));
        c.cursors.len() - 1
    });
    if !c.cursors[i].1.reads(part, residency) {
        c.cursors[i].1 = PartCursors::new(part, residency);
    }
    &mut c.cursors[i].1
}

/// A slot for a new entry of `relid`: its oldest entry's buffers when it holds [`PER_REL`], else fresh ones.
fn take_slot(c: &mut Cache, relid: Oid, ncols: usize) -> Vec<ColBuf> {
    let mine: Vec<usize> = c.entries.iter().enumerate().filter(|(_, e)| e.relid == relid).map(|(i, _)| i).collect();
    match mine.first() {
        Some(&oldest) if mine.len() >= PER_REL => {
            let e = c.entries.remove(oldest);
            if e.cols.len() == ncols {
                return e.cols;
            }
            (0..ncols).map(|_| ColBuf::new()).collect()
        }
        _ => (0..ncols).map(|_| ColBuf::new()).collect(),
    }
}

/// The scan's decoded granule `key` (`rows` rows in `cols`) as it leaves it: when `relid` is armed and the granule is
/// not held, `cols` go into the cache and a spare set of the same width comes back.
pub(crate) fn offer(relid: Oid, key: GranuleKey, rows: u32, cols: &mut Vec<ColBuf>) {
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if !c.armed.contains(&relid) || c.entries.iter().any(|e| e.relid == relid && e.key == key) {
            return;
        }
        let spare = take_slot(&mut c, relid, cols.len());
        let full = std::mem::replace(cols, spare);
        c.entries.push(Entry { relid, key, rows, cols: full });
    });
}

/// Row `row` of granule `key` of `part` into `slot`, decoding the granule on a miss. The slot is materialized in
/// `mcx` (one row copy): the cache may evict the granule before the slot dies. An absent row is `22023`. FASTADD: a
/// column the part lacks reads its missing value under `desc` (the relation's descriptor).
pub fn fetch_row<'mcx>(mcx: Mcx<'mcx>, relid: Oid, part: &OpenedPart, schemas: &[ColSchema], desc: &types_tuple::tupdesc::TupleDescData<'_>, key: GranuleKey, row: u32, slot: &mut SlotData<'mcx>) -> PgResult<()> {
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        let at = match c.entries.iter().position(|e| e.relid == relid && e.key == key) {
            Some(i) => i,
            None => {
                let mut cols = take_slot(&mut c, relid, schemas.len());
                let fp = pgrc_format::ident::schema_fingerprint(schemas);
                let part_fp = part.expect.schema_fingerprint.unwrap_or(fp);
                // D2189: a stamp a column dropped with no ALTER publish left (`schema::book_dropped_stamp`)
                if part_fp != fp {
                    crate::schema::book_dropped_stamp_of(None, &desc.attrs, part_fp)?;
                }
                let present = crate::fastadd::presence(schemas, fp, part_fp)?;
                let fills = present.as_ref().map(|_| crate::fastadd::fills(desc, schemas)).transpose()?;
                let lacks = present.as_deref().zip(fills.as_deref()).map(|(present, fills)| crate::scan::Lacks { present, fills });
                let rows = decode_granule(cursors_for(&mut c, relid, &part.part), codec_binding(), schemas, key.granule, &mut cols, lacks)?;
                c.entries.push(Entry { relid, key, rows, cols });
                c.entries.len() - 1
            }
        };
        let e = &c.entries[at];
        if row >= e.rows {
            return Err(Box::new(
                PgError::error(format!("pgrcolumnar: position (granule {}, row {row}) not valid in this statement", key.granule))
                    .with_sqlstate(types_error::ERRCODE_INVALID_PARAMETER_VALUE),
            ));
        }
        exectuples::exec_clear_tuple(slot, mcx);
        let natts = slot.base().tts_values.len();
        store_cols(&e.cols, schemas, natts, row as usize, slot);
        exectuples::exec_materialize_slot(slot, mcx)
    })
}

// --- IX1 L0 (choice 6): the column-selective cache index and TID fetches read ---------------------------------

/// One column of one granule of a part file (its `OpenPart::uuid`: device, inode, length, footer). D4674: the uuid
/// alone does not name one part: once every holder of a part lets go its inode can be reused, and a new part of the
/// same length and footer offset derives the same uuid (pgrc_format `ident.rs`), so an entry also keeps the part it
/// was decoded from ([`ColEntry::part`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ColKey {
    part: [u8; 16],
    granule: u32,
    attno: u32,
}

struct ColEntry {
    key: ColKey,
    /// D4674: the part this entry was decoded from, checked on every hit ([`ColCache::held`]). A `Weak` holds no fd
    /// (the part's file may go and its inode be reused); while it lives the part's allocation stays, so no other part
    /// opens at its address.
    part: Weak<OpenPart>,
    rows: u32,
    buf: ColBuf,
    bytes: usize,
    /// The fetch that last read it ([`ColCache::tick`]): an entry the current fetch reads is never evicted by it.
    used: u64,
}

/// What a backend's part-row fetches decoded (`pgrc_ixfetch_stats`): fetches, column granules decoded, column
/// granules found decoded, and (IXPERF F1) single column values decoded without their granule.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ColStats {
    pub fetches: u64,
    pub decoded: u64,
    pub hits: u64,
    pub sliced: u64,
}

#[derive(Default)]
struct ColCache {
    index: std::collections::HashMap<ColKey, usize>,
    entries: Vec<ColEntry>,
    bytes: usize,
    tick: u64,
    /// The cursors of the part the last miss read (as [`Cache::cursors`]: a run of misses in one part opens each
    /// column's cursor once).
    cursors: Option<PartCursors>,
    /// Buffers of evicted entries, reused by the next miss.
    spare: Vec<ColBuf>,
    /// FASTADD: the buffer a column a part lacks is filled into (never cached: its fill is the descriptor's).
    fill: Option<ColBuf>,
    /// IXPERF F1: per column (in `schemas` order), what its last slice decoded into.
    slices: Vec<SliceBuf>,
    stats: ColStats,
}

impl ColCache {
    /// Transaction end: the entries go (what their datums alias with them), the counters stay.
    /// IXPERF F1: the slice buffers stay (their guards let go).
    fn clear(&mut self) {
        let stats = self.stats;
        let mut slices = std::mem::take(&mut self.slices);
        slices.iter_mut().for_each(SliceBuf::release);
        *self = ColCache { stats, slices, ..ColCache::default() };
    }

    /// Evict the least recently used entries no part of the current fetch read until `need` more bytes fit `cap`.
    fn make_room(&mut self, need: usize, cap: usize) {
        while self.bytes + need > cap {
            let tick = self.tick;
            let Some(victim) = self.entries.iter().enumerate().filter(|(_, e)| e.used < tick).min_by_key(|(_, e)| e.used).map(|(i, _)| i) else { return };
            self.evict(victim);
        }
    }

    /// Entry `victim` goes; the entry moved into its place keeps its index; its buffers become a spare.
    fn evict(&mut self, victim: usize) {
        let e = self.entries.swap_remove(victim);
        self.index.remove(&e.key);
        if let Some(moved) = self.entries.get(victim) {
            self.index.insert(moved.key, victim);
        }
        self.bytes -= e.bytes;
        if self.spare.len() < 8 {
            self.spare.push(e.buf);
        }
    }

    /// D4674: the entry of `key` if `part` itself decoded it. An entry of the key another part decoded (a part of the
    /// same uuid: a reused inode) goes, and the caller decodes `part`'s own.
    fn held(&mut self, key: &ColKey, part: &Arc<OpenPart>) -> Option<usize> {
        let i = *self.index.get(key)?;
        if std::ptr::eq(self.entries[i].part.as_ptr(), Arc::as_ptr(part)) {
            return Some(i);
        }
        self.evict(i);
        None
    }

    /// A miss: column `c` (of `schemas`) of `granule` of `part` decoded whole into a new entry of `key`, its `rows`
    /// checked, room made under `cap`; the entry's index.
    #[allow(clippy::too_many_arguments)]
    fn decode(&mut self, part: &Arc<OpenPart>, schemas: &[ColSchema], c: usize, granule: u32, rows: u32, key: ColKey, cap: usize) -> PgResult<usize> {
        let mut buf = self.spare.pop().unwrap_or_else(ColBuf::new);
        if !self.cursors.as_ref().is_some_and(|pc| pc.reads(part, Residency::Cached)) {
            self.cursors = Some(PartCursors::new(part, Residency::Cached));
        }
        let pc = self.cursors.as_mut().expect("just set");
        let got = crate::scan::decode_one_column(pc, codec_binding(), schemas.len(), c, &schemas[c], granule, &mut buf)?;
        if got != rows {
            return Err(Box::new(
                PgError::error(format!("pgrcolumnar: column {} of granule {granule} holds {got} rows, the part {rows}", schemas[c].attno))
                    .with_sqlstate(types_error::ERRCODE_DATA_CORRUPTED),
            ));
        }
        self.stats.decoded += 1;
        let bytes = buf.mem_bytes();
        self.make_room(bytes, cap);
        self.entries.push(ColEntry { key, part: Arc::downgrade(part), rows, buf, bytes, used: 0 });
        self.bytes += bytes;
        self.index.insert(key, self.entries.len() - 1);
        Ok(self.entries.len() - 1)
    }
}

thread_local! {
    static COLS: RefCell<ColCache> = RefCell::new(ColCache::default());
}

/// The column cache's byte cap per backend.
const COL_CAP: usize = 32 << 20;

/// IXPERF F1: the column granules one [`Touches`] remembers having sliced; past it the set forgets (a long scan of
/// random keys keeps slicing).
const SEEN_CAP: usize = 1 << 16;

/// IXPERF F1 teeth (pgrc-ixfetch-e2e): while attached, a slice reads the granule's next row.
const SLICE_SKEW_POINT: &str = "pgrc-rowcache-slice-skew";

/// IXPERF F1: the column granules one fetch state (an index or TID scan, a one-off fetch: `pgrc_front::ixfetch`'s
/// `PgrcIndexFetch`) sliced. [`fetch_cols`] slices a granule's column on the scan's first touch and decodes the granule
/// into the column cache on its second, so a point fetch reads one row and a scan that keeps returning to a granule
/// (a range of clustered keys, a nested loop's inner side) decodes it once. The set lives with the scan, not with the
/// column cache: the cache lasts until `disarm_all`, which a session that never scanned or wrote a pgrc table does not
/// reach at transaction end, so a cache-lived set would turn every later point fetch into a granule decode.
#[derive(Default)]
pub struct Touches {
    seen: std::collections::HashSet<ColKey>,
}

impl Touches {
    pub fn new() -> Touches {
        Touches::default()
    }
}

/// IXPERF F1: whether this is the scan's first touch of `key` (recorded); a full set forgets first.
fn first_touch(seen: &mut std::collections::HashSet<ColKey>, key: ColKey) -> bool {
    if seen.len() >= SEEN_CAP && !seen.contains(&key) {
        seen.clear();
    }
    seen.insert(key)
}

/// This backend's [`ColStats`].
pub fn col_stats() -> ColStats {
    COLS.with(|c| c.try_borrow().map_or(ColStats::default(), |c| c.stats))
}

/// IX1 L0 (choice 6): row `row` of granule `granule` of `part` into `slot`, decoding only the columns `needed` names
/// (by `attno - 1`; `None`, or an attribute past the slice's end, = needed), each through the per-backend column
/// cache; the others read NULL. IXPERF F1: a column granule the cache lacks is sliced on `touches`' first touch (the
/// row alone, [`crate::scan::decode_one_row`]) and decoded whole into the cache on the second ([`Touches`]). The slot
/// is materialized in `mcx` (an entry may be evicted after the call). An absent row is `22023`. FASTADD: a column the part lacks reads its missing value under `desc`. `fp` is
/// `schema_fingerprint(schemas)` (IXPERF F3a: the caller keeps it with the schemas).
#[allow(clippy::too_many_arguments)]
pub fn fetch_cols<'mcx>(
    mcx: Mcx<'mcx>,
    part: &OpenedPart,
    schemas: &[ColSchema],
    fp: u64,
    desc: &types_tuple::tupdesc::TupleDescData<'_>,
    granule: u32,
    row: u32,
    needed: Option<&[bool]>,
    touches: &mut Touches,
    slot: &mut SlotData<'mcx>,
) -> PgResult<()> {
    let rows = pgrc_format::geom::rows_in_granule_at(part.part.rows(), part.part.grain(), granule);
    if row >= rows {
        return Err(Box::new(
            PgError::error(format!("pgrcolumnar: position (granule {granule}, row {row}) not valid in this statement"))
                .with_sqlstate(types_error::ERRCODE_INVALID_PARAMETER_VALUE),
        ));
    }
    let part_fp = part.expect.schema_fingerprint.unwrap_or(fp);
    // D2189: a stamp a column dropped with no ALTER publish left (`schema::book_dropped_stamp`)
    if part_fp != fp {
        crate::schema::book_dropped_stamp_of(None, &desc.attrs, part_fp)?;
    }
    let present = crate::fastadd::presence(schemas, fp, part_fp)?;
    let uuid = part.part.uuid();
    exectuples::exec_clear_tuple(slot, mcx);
    let natts = slot.base().tts_values.len();
    {
        let base = slot.base_mut();
        base.tts_isnull[..natts].fill(true);
        base.tts_values[..natts].fill(datum::Datum::null());
    }
    let cap = COL_CAP;
    COLS.with(|cc| -> PgResult<()> {
        let mut cc = cc.borrow_mut();
        let cc = &mut *cc;
        cc.tick += 1;
        cc.stats.fetches += 1;
        for (c, schema) in schemas.iter().enumerate() {
            let a = schema.attno as usize - 1;
            if a >= natts || needed.is_some_and(|n| !n.get(a).copied().unwrap_or(true)) {
                continue;
            }
            let base = slot.base_mut();
            if present.as_deref().is_some_and(|p| !p[c]) {
                let fills = crate::fastadd::fills(desc, std::slice::from_ref(schema))?;
                let buf = cc.fill.get_or_insert_with(ColBuf::new);
                buf.fill_const(rows, &fills[0]);
                if buf.is_valid(row as usize) {
                    base.tts_isnull[a] = false;
                    base.tts_values[a] = datum::Datum::from_u64(buf.datums[row as usize]);
                }
                continue;
            }
            let key = ColKey { part: uuid, granule, attno: schema.attno };
            let held = cc.held(&key, &part.part);
            if held.is_none() && first_touch(&mut touches.seen, key) {
                if !cc.cursors.as_ref().is_some_and(|pc| pc.reads(&part.part, Residency::Cached)) {
                    cc.cursors = Some(PartCursors::new(&part.part, Residency::Cached));
                }
                if cc.slices.len() < schemas.len() {
                    let more = schemas.len() - cc.slices.len();
                    cc.slices.try_reserve(more).map_err(|_| ::mcx::oom_named("pgrc row cache", more * core::mem::size_of::<SliceBuf>()))?;
                    cc.slices.resize_with(schemas.len(), SliceBuf::new);
                }
                let at = if injection_point::is_attached(SLICE_SKEW_POINT) { (row + 1).min(rows - 1) } else { row };
                let pc = cc.cursors.as_mut().expect("just set");
                let got = crate::scan::decode_one_row(pc, codec_binding(), schemas.len(), c, schema, granule, at, &mut cc.slices[c])?;
                cc.stats.sliced += 1;
                if let Some(w) = got {
                    base.tts_isnull[a] = false;
                    base.tts_values[a] = datum::Datum::from_u64(w);
                }
                continue;
            }
            let at = match held {
                Some(i) => {
                    cc.stats.hits += 1;
                    i
                }
                None => cc.decode(&part.part, schemas, c, granule, rows, key, cap)?,
            };
            let e = &mut cc.entries[at];
            e.used = cc.tick;
            debug_assert_eq!(e.rows, rows);
            if e.buf.is_valid(row as usize) {
                base.tts_isnull[a] = false;
                base.tts_values[a] = datum::Datum::from_u64(e.buf.datums[row as usize]);
            }
        }
        Ok(())
    })?;
    let base = slot.base_mut();
    base.tts_nvalid = natts as types_core::AttrNumber;
    base.mark_not_empty();
    exectuples::exec_materialize_slot(slot, mcx)
}

/// The cache's decoded rows of `relid` (tests).
#[cfg(test)]
fn held(relid: Oid) -> Vec<GranuleKey> {
    CACHE.with(|c| c.borrow().entries.iter().filter(|e| e.relid == relid).map(|e| e.key).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(granule: u32) -> GranuleKey {
        GranuleKey { gen: 7, part: 0, granule }
    }

    fn cols(tag: u64) -> Vec<ColBuf> {
        let mut c = vec![ColBuf::new(), ColBuf::new()];
        c[0].datums[0] = tag;
        c
    }

    #[test]
    fn an_unarmed_relation_keeps_its_buffers() {
        disarm_all();
        let mut c = cols(11);
        offer(1, key(0), 5, &mut c);
        assert_eq!(c[0].datums[0], 11);
        assert!(held(1).is_empty());
    }

    #[test]
    fn an_armed_relation_swaps_buffers_and_keeps_two_granules() {
        disarm_all();
        arm(2);
        let mut c = cols(21);
        offer(2, key(0), 5, &mut c);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].datums[0], 0, "a spare set came back");
        c[0].datums[0] = 22;
        offer(2, key(1), 5, &mut c);
        c[0].datums[0] = 23;
        offer(2, key(2), 5, &mut c);
        assert_eq!(held(2), vec![key(1), key(2)]);
        assert_eq!(c[0].datums[0], 21, "the evicted granule's buffers are the spare: nothing allocated once warm");
        offer(2, key(2), 5, &mut c);
        assert_eq!(held(2), vec![key(1), key(2)], "a held granule is not offered twice");
        disarm_all();
        assert!(held(2).is_empty());
    }

    #[test]
    fn a_bulk_scans_relation_refetches_private_until_statement_end() {
        disarm_all();
        note_bulk(3);
        CACHE.with(|c| {
            let c = c.borrow();
            assert_eq!(refetch_residency(&c, 3), Residency::Private);
            assert_eq!(refetch_residency(&c, 4), Residency::Cached);
        });
        disarm_all();
        CACHE.with(|c| assert_eq!(refetch_residency(&c.borrow(), 3), Residency::Cached));
    }

    fn col_entry(cc: &mut ColCache, granule: u32, bytes: usize, used: u64) {
        let key = ColKey { part: [7; 16], granule, attno: 3 };
        cc.entries.push(ColEntry { key, part: Weak::new(), rows: 1, buf: ColBuf::unallocated(), bytes, used });
        cc.bytes += bytes;
        cc.index.insert(key, cc.entries.len() - 1);
    }

    /// IX1 L0: the column cache evicts least recently used first, never what the current fetch read, and keeps its
    /// index in step with the entries it moves.
    #[test]
    fn the_column_cache_evicts_lru_but_never_the_current_fetchs() {
        let mut cc = ColCache { tick: 10, ..ColCache::default() };
        col_entry(&mut cc, 0, 100, 4);
        col_entry(&mut cc, 1, 100, 2);
        col_entry(&mut cc, 2, 100, 10);
        col_entry(&mut cc, 3, 100, 7);
        cc.make_room(150, 400);
        let mut left: Vec<u32> = cc.entries.iter().map(|e| e.key.granule).collect();
        left.sort_unstable();
        assert_eq!(left, vec![2, 3], "granules 1 then 0 (the oldest) went");
        assert_eq!(cc.bytes, 200);
        for (i, e) in cc.entries.iter().enumerate() {
            assert_eq!(cc.index[&e.key], i, "the index follows a swap_remove");
        }
        cc.make_room(1000, 400);
        assert_eq!(cc.entries.iter().map(|e| e.key.granule).collect::<Vec<_>>(), vec![2], "the current fetch's entry stays past the cap");
        assert_eq!(cc.spare.len(), 3);
    }

    /// The column cache empties at transaction end (`disarm_all`) and keeps its counters.
    #[test]
    fn the_column_cache_clears_at_disarm_and_keeps_its_counters() {
        COLS.with(|c| {
            let mut cc = c.borrow_mut();
            col_entry(&mut cc, 0, 64, 1);
            cc.stats.decoded += 2;
        });
        let before = col_stats();
        disarm_all();
        COLS.with(|c| assert!(c.borrow().entries.is_empty() && c.borrow().index.is_empty() && c.borrow().bytes == 0));
        assert_eq!(col_stats(), before);
    }

    /// IXPERF F1: a scan's first touch of a column granule slices it, the second decodes it; a new scan (its own
    /// [`Touches`]) slices again; the cache's clear keeps the slice buffers and counters; the set forgets when full.
    #[test]
    fn a_first_touch_slices_and_a_second_decodes() {
        let k = |granule: u32, attno: u32| ColKey { part: [9; 16], granule, attno };
        let mut t = Touches::new();
        assert!(first_touch(&mut t.seen, k(0, 1)));
        assert!(first_touch(&mut t.seen, k(0, 2)), "another column of the granule");
        assert!(first_touch(&mut t.seen, k(1, 1)), "another granule of the column");
        assert!(!first_touch(&mut t.seen, k(0, 1)), "the second touch");
        assert!(first_touch(&mut Touches::new().seen, k(0, 1)), "a new scan slices again");
        let mut cc = ColCache::default();
        cc.slices.resize_with(2, crate::scan::SliceBuf::new);
        cc.stats.sliced = 3;
        cc.clear();
        assert_eq!(cc.slices.len(), 2, "the slice buffers stay");
        assert_eq!(cc.stats.sliced, 3, "the counters stay");
        let mut seen = std::collections::HashSet::new();
        for g in 0..SEEN_CAP as u32 {
            assert!(first_touch(&mut seen, k(g, 1)));
        }
        assert!(!first_touch(&mut seen, k(7, 1)), "a held key at the cap is a second touch");
        assert!(first_touch(&mut seen, k(SEEN_CAP as u32, 1)), "a new key at the cap: the set forgets, then records it");
        assert_eq!(seen.len(), 1);
        assert!(first_touch(&mut seen, k(7, 1)), "forgotten: sliced again");
    }

    /// D4674 (born red): a part's uuid is its file's (device, inode, length, footer). A part that opens where a dropped
    /// part's inode was, of the same length and footer offset, derives the same uuid; the column cache must not answer
    /// its fetch with the dropped part's column. Before, an entry answered by uuid alone: pgrc-gs-index-seal's bitmap
    /// session (its cache never cleared, no transaction-end hook on the bitmap road) read a dropped table's ids.
    #[test]
    fn d4674_an_entry_answers_only_for_the_part_that_decoded_it() {
        use pgrc_format::class::{CollationClass, StorageClass, TypeSemantics};
        use pgrc_read::{MemPartIo, PartExpect};
        // a distinctive device: the uuid is the identity's hash; no registry here
        const DEV: u64 = 0x4D33_4801_0000_4674;
        const INO: u64 = 4674;
        let open = |v: i64| {
            let bytes = crate::tests::ino_reuse::part_bytes(v);
            Arc::new(OpenPart::open(Box::new(MemPartIo::new(bytes, DEV, INO)), &PartExpect::none()).expect("open part"))
        };
        let schemas = [ColSchema {
            attno: 1,
            class: StorageClass::ByvalWord { width: 8, signed: true },
            typlen: 8,
            typbyval: true,
            typalign: b'd',
            collation_class: CollationClass::C,
            semantics: TypeSemantics::SignedInt,
        }];
        let mut cc = ColCache::default();
        let a = open(11);
        let key = ColKey { part: a.uuid(), granule: 0, attno: 1 };
        let i = cc.decode(&a, &schemas, 0, 0, 1, key, COL_CAP).expect("decode a");
        assert_eq!(cc.entries[i].buf.datums[0] as i64, 11);
        assert_eq!(cc.held(&key, &a), Some(i), "a hit for the part that decoded it");
        drop(a);
        // the dropped part's inode reused by a part of the same length and footer: the same uuid, other rows
        let b = open(22);
        assert_eq!(b.uuid(), key.part, "the same device, inode, length and footer derive the same uuid");
        assert_eq!(cc.held(&key, &b), None, "never a hit for another part of the uuid");
        assert!(cc.entries.is_empty() && cc.index.is_empty() && cc.bytes == 0, "the other part's entry went");
        let j = cc.decode(&b, &schemas, 0, 0, 1, key, COL_CAP).expect("decode b");
        assert_eq!(cc.entries[j].buf.datums[0] as i64, 22, "b's own column");
        assert_eq!(cc.held(&key, &b), Some(j));
    }
}
