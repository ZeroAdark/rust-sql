//! The by-rowid fetch site (docs/gravity/fetch.md §1.2): `Source::fetch(rowids, lanes)` over pgrc
//! parts — the request sorted by `RowId` (order-embedding: one sort of the words groups it by
//! `(part, granule)`), each touched granule decoded ONCE per requested column through the reader's
//! survivor-only face (`decode_sel`), the values scattered back by request index, the lanes
//! published as `KIND_BYVAL` datum words at width 8 (every by-value storage class: ints of any width,
//! floats, bool) or `KIND_BYREF` varlena pointers (the image in this source's arena, or the writer
//! dictionary's resident entry). The leader fetches (the survivors exist after the merge); no file is
//! opened when the catalog holds the part.
use std::sync::Arc;

use gravity_abi::ctx::{lane, Datum, Lane, ScanPos};
use gravity_driver::{BindingRef, Cursor, Error, Residency, Result, Source, Window};
use pgrc_format::abi::{ByteArena, DecodeOut, Selection, ValidityVerdict};
use pgrc_format::class::StorageClass;
use pgrc_format::rowid::{rowid_granule, rowid_part, rowid_row};
use pgrc_format::FormatError;
use pgrc_read::{OpenPart, PartPin, PartRegistry, ReadError, StreamCursor};
use pgrc_scan::PartDeletes;
use crate::ops::scan::heap_source::HeldCharge;
use types_core::Oid;
use types_error::PgResult;
use types_rel::Relation;

use crate::ops::scan::pgrc::granule_source::{open_io, PartList, Resolved};

/// Rows one fetch may request (the ABI's cap; the fetch emit asks its winners this many at a time, so no top's bound waits
/// on it).
pub const MAX_FETCH_ROWS: usize = gravity_abi::site::MAX_FETCH_ROWS;
/// The process census of every fetch site: `[rows fetched, (granule, column) decodes]` — the served line's `fetch=` witness.
pub static FETCH_DIAG: [core::sync::atomic::AtomicU64; 2] = [const { core::sync::atomic::AtomicU64::new(0) }; 2];
const VALIDITY_WORDS: usize = 128;
const ARENA_MIN: usize = gravity_program::cost::pgrc::FETCH_ARENA_MIN_BYTES;
/// A column read as a by-value datum word (`decode_sel` writes the datum itself).
pub const KIND_INT: u8 = 0;
/// A column read as a pointer to its image: a varlena, or a fixed-length by-reference type (SRC-26).
pub const KIND_TEXT: u8 = 1;

/// The fetch site's column set: `attnos` of `rel`, each a by-value datum (`ByvalWord` of any width,
/// `F32`, `F64`, `Bool`: the reader's decode writes the datum PostgreSQL keeps, SRC-26: the storage
/// class decides, not a type list) or a varlena column — the site's lanes in this order, then the
/// offset lane the descriptor carries, then the rowid lane the kernel fills with its request. A
/// fixed-length by-reference image (`Fixed`: uuid, interval) refuses: the fetch emit's cells are words
/// or varlena pointers.
#[derive(Clone)]
pub struct FetchRecipe {
    pub rel: Oid,
    pub natts: usize,
    pub attnos: Arc<[i16]>,
    pub kinds: Arc<[u8]>,
    registry: Arc<PartRegistry>,
}

impl FetchRecipe {
    /// The plan's columns: a pgrc relation's attnos, each one of its storage classes.
    pub fn from_plan(rel: &Relation<'_>, attnos: &[i16]) -> PgResult<FetchRecipe> {
        use tableam_vocab::TableAm;
        use types_rel::{RELKIND_MATVIEW, RELKIND_RELATION};
        // D2989: a pgrcolumnar materialized view too (the same storage)
        if !matches!(rel.rd_rel.relkind, RELKIND_RELATION | RELKIND_MATVIEW) || TableAm::of(rel) != Some(TableAm::Pgrcolumnar) || rel.rd_islocaltemp {
            return Err(crate::pg_err("gravity: fetch needs a pgrc relation"));
        }
        if attnos.is_empty() {
            return Err(crate::pg_err("gravity: fetch site shape"));
        }
        let schemas = pgrc_am::schema::col_schemas(rel)?;
        let mut kinds = vec![KIND_INT; attnos.len()]; // alloc: Program
        // bounded("key-columns: one fetched column's class at plan time, found among the table's columns, at most MaxTupleAttributeNumber (1664) columns")
        for (k, &a) in attnos.iter().enumerate() {
            let Some(c) = schemas.iter().find(|c| a > 0 && c.attno == a as u32) else { return Err(crate::pg_err("gravity: fetch attno")) };
            kinds[k] = match c.class {
                StorageClass::ByvalWord { .. } | StorageClass::F32 | StorageClass::F64 | StorageClass::Bool => KIND_INT,
                // SRC-26: a fixed-length by-reference image is published as a pointer to its arena copy, as a varlena's
                StorageClass::VarlenaVerbatim | StorageClass::Fixed { .. } => KIND_TEXT,
            };
        }
        Ok(FetchRecipe::for_test(rel.rd_id, attnos, &kinds, pgrc_am::inval::registry()))
    }

    /// The recipe without a relation: the part list comes through the statement's `Resolved`.
    pub fn for_test(rel: Oid, attnos: &[i16], kinds: &[u8], registry: Arc<PartRegistry>) -> FetchRecipe {
        assert!(attnos.len() == kinds.len() && !attnos.is_empty(), "fetch shape");
        FetchRecipe { rel, natts: attnos.len(), attnos: Arc::from(attnos), kinds: Arc::from(kinds), registry } // alloc: Program
    }
}

struct Slot {
    part: Option<Arc<OpenPart>>,
    cursors: Vec<Option<StreamCursor<'static>>>, // alloc: Node
}

pub struct FetchSource {
    /// CONVERT wave 2: the kept buffers' charge (`kept_bytes`), first so it drops before them.
    held: HeldCharge,
    recipe: FetchRecipe,
    list: Option<Arc<PartList>>,
    slots: Vec<Slot>,
    held_generation: u64,
    /// The published cells: `natts × MAX_FETCH_ROWS` datum words and their validity words.
    datums: Vec<u64>,
    valid: Vec<u64>,
    /// Text images the decode wrote (the varlena copies of verbatim columns), one fetch at a time.
    arena: Vec<u64>,
    /// The chunks this fetch's earlier groups and columns wrote images into before the arena last grew: a grown arena is a
    /// fresh chunk, never the old one moved, since the datum words published so far point into them. Freed at the next
    /// fetch. Growing in place moved every image the fetch had written (a request of many rows, or one row's texts past
    /// `ARENA_MIN`), and their published pointers read the freed buffer.
    spent: Vec<Vec<u64>>,
    order: Vec<u32>,
    sel: Vec<u16>,
    /// Per request of the current group, its row's index in `sel` (a rowid asked twice decodes once).
    didx: Vec<u16>,
    dense: Vec<u64>,
    vmask: Vec<u64>,
    fetched: u64,
    granule_decodes: u64,
    bound: bool,
    cur: Cursor,
}

fn read_err(e: ReadError) -> Error {
    crate::stash(pgrc_am::read_error(e))
}

impl FetchSource {
    pub fn new(recipe: FetchRecipe) -> FetchSource {
        FetchSource { held: HeldCharge::new(), recipe, list: None, slots: Vec::new(), held_generation: 0, datums: Vec::new(), valid: Vec::new(), arena: Vec::new(), spent: Vec::new(), order: Vec::new(), sel: Vec::new(), didx: Vec::new(), dense: Vec::new(), vmask: Vec::new(), fetched: 0, granule_decodes: 0, bound: false, cur: Cursor::new() } // alloc: Node
    }

    /// The census: rows fetched, `(granule, column)` decodes.
    pub fn census(&self) -> (u64, u64) {
        (self.fetched, self.granule_decodes)
    }

    fn pin(&mut self, p: usize) -> Result<()> {
        let list = Arc::clone(self.list.as_ref().ok_or_else(|| Error::host("gravity: fetch on an unbound source"))?);
        if p >= list.len() {
            return Err(Error::host("gravity: a rowid names a part outside the plan"));
        }
        if self.slots[p].part.is_some() {
            return Ok(());
        }
        let spec = &list.parts()[p];
        let part = match &spec.part {
            Some(part) => Arc::clone(part),
            None => Arc::clone(self.recipe.registry.open_pinned(&spec.expect, || open_io(&spec.io)).map_err(read_err)?.part()),
        };
        if part.footer().granule_count != spec.granules {
            return Err(Error::host("gravity: pgrc part geometry changed under the plan"));
        }
        let _ = PartPin::pin(&part);
        self.slots[p].part = Some(part);
        Ok(())
    }

    fn cursor(&mut self, p: usize, k: usize) -> Result<&mut StreamCursor<'static>> {
        let slot = &mut self.slots[p];
        if slot.cursors.len() < self.recipe.natts {
            slot.cursors.resize_with(self.recipe.natts, || None); // alloc: Node
        }
        if slot.cursors[k].is_none() {
            let attno = self.recipe.attnos[k] as u32;
            let real = slot.part.as_ref().expect("pinned");
            // FASTADD: a column the part lacks reads its stand-in (granule_source/fastadd.rs)
            let part = match self.list.as_deref() {
                Some(l) => Arc::clone(l.lane_part(p, attno, real).map_err(crate::stash)?),
                None => Arc::clone(real),
            };
            let mut cur = StreamCursor::open(part, pgrc_am::scan::codec_binding(), attno, 0).map_err(read_err)?;
            cur.set_owner(self.held.owner());
            slot.cursors[k] = Some(cur); // alloc: Node
        }
        Ok(slot.cursors[k].as_mut().expect("cursor"))
    }

    /// The arena grown to at least `bytes`: a fresh chunk, the current one kept in `spent` (its images stay where the
    /// fetch's published datum words point) and the fresh chunk's use reset.
    fn grow(&mut self, bytes: usize) {
        let fresh = vec![0u64; bytes.div_ceil(8)]; // alloc: Node
        self.spent.push(core::mem::replace(&mut self.arena, fresh)); // alloc: Node
        self.dense[MAX_FETCH_ROWS] = 0;
    }

    /// One `(part, granule)` group of the sorted request: every column decoded once for the group's rows.
    fn group(&mut self, rowids: &[u64], from: usize, to: usize) -> Result<()> {
        let id = rowids[self.order[from] as usize];
        let (p, lg) = (rowid_part(id) as usize, rowid_granule(id));
        self.pin(p)?;
        let list = Arc::clone(self.list.as_ref().expect("bound"));
        let deletes = list.parts()[p].deletes.as_ref().and_then(|d| d.granule_mask(lg));
        self.sel.clear();
        self.didx.clear();
        // bounded("small-fixed: one fetch request's rows, at most MAX_FETCH_ROWS (1024)")
        for &i in &self.order[from..to] {
            let row = rowid_row(rowids[i as usize]);
            if deletes.is_some_and(|m| PartDeletes::is_deleted(m, row)) {
                return Err(Error::host("gravity: a fetched row is deleted under the snapshot"));
            }
            if self.sel.last() != Some(&(row as u16)) {
                self.sel.push(row as u16); // alloc: Node charged self.held.owner()
            }
            self.didx.push(self.sel.len() as u16 - 1); // alloc: Node charged self.held.owner()
        }
        let n = self.sel.len();
        // a tick per column: each decodes its granule's selected rows
        let mut step = cancelpoll::step("fetch_site group: a granule's columns decoded");
        for k in 0..self.recipe.natts {
            step.tick()?;
            let rows = self.cursor(p, k)?.rows_in_granule(lg);
            if self.sel.last().is_some_and(|&r| r as u32 >= rows) {
                return Err(Error::host("gravity: a rowid names a row outside its granule"));
            }
            // bounded("small-fixed: one retry per doubling of the fetch's arena chunk, at most 64 (a usize's bits)")
            loop {
                let arena_len = self.arena.len() * 8;
                let mut arena = ByteArena::new(super::raw::words_as_bytes_mut(&mut self.arena));
                // the arena continues past the images earlier groups wrote in this fetch
                let used = self.dense[MAX_FETCH_ROWS] as usize;
                if arena.alloc(used).is_err() {
                    self.grow((used + ARENA_MIN).div_ceil(8) * 16);
                    continue;
                }
                let mut out = DecodeOut { datums: &mut self.dense[..n], arena };
                let sel = Selection { rows: &self.sel };
                let cur = self.slots[p].cursors[k].as_mut().expect("cursor");
                match cur.decode_sel(lg, &sel, &mut out) {
                    Ok(got) if got as usize == n => {
                        self.dense[MAX_FETCH_ROWS] = out.arena.used() as u64;
                        break;
                    }
                    Ok(_) => return Err(Error::host("gravity: fetch decoded count")),
                    Err(ReadError::Format(FormatError::ArenaExhausted { needed })) => {
                        // the column's images (this group's) are decoded again into the fresh chunk
                        let want = (arena_len + needed).max(arena_len * 2).max(ARENA_MIN);
                        self.grow(want);
                    }
                    Err(e) => return Err(read_err(e)),
                }
            }
            let cur = self.slots[p].cursors[k].as_mut().expect("cursor");
            let all = !cur.has_validity_stream() || matches!(cur.validity(lg, &mut self.vmask).map_err(read_err)?, ValidityVerdict::AllValid);
            self.granule_decodes += 1;
            FETCH_DIAG[1].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            // bounded("small-fixed: one fetch request's rows, at most MAX_FETCH_ROWS (1024)")
            for (j, &i) in self.order[from..to].iter().enumerate() {
                let (i, j) = (i as usize, self.didx[j] as usize);
                self.datums[k * MAX_FETCH_ROWS + i] = self.dense[j];
                let row = self.sel[j] as usize;
                if all || (self.vmask[row / 64] >> (row % 64)) & 1 == 1 {
                    self.valid[k * VALIDITY_WORDS / 8 + i / 64] |= 1 << (i % 64);
                }
            }
        }
        Ok(())
    }
}

impl Source for FetchSource {
    fn bind(&mut self, b: &BindingRef<'_>) -> Result<()> {
        // shapecheck fire signal (REGISTRY SRC-26): the by-rowid fetch site serves this statement (from_plan took its
        // columns: any by-value class or varlena); here, not in from_plan, so a program-cache hit fires too
        fires::fire_shared("pgrc.fetch_site");
        // the relation's list as the statement's leader resolved it (the scan site's, §4.2)
        let list = Resolved::of(b).map_err(crate::stash)?.list(self.recipe.rel).cloned().ok_or_else(|| Error::host("gravity: fetch source bound without a part list"))?;
        if list.generation() != self.held_generation {
            self.slots.clear();
            self.held_generation = list.generation();
        }
        // D3994 / D3204 (B.1 R): the kept cursors' part uses are the binding statement's while it runs (`unbind` gives them
        // back to the session)
        let (to, release) = memledger::charge::context();
        // bounded("other: one O(1) owner move per kept cursor, the read columns of the parts this source keeps")
        for c in self.slots.iter_mut().flat_map(|s| s.cursors.iter_mut()).flatten() {
            c.rejoin(to, release);
        }
        // CONVERT wave 2: a fetch site has no window to name its breaker: its buffers are the statement's (or portal's)
        if self.held.home(HeldCharge::of_statement()) {
            let to = self.held.owner();
            // bounded("other: one O(1) owner set per kept cursor, the read columns of the parts this source keeps")
            for c in self.slots.iter_mut().flat_map(|s| s.cursors.iter_mut()).flatten() {
                c.set_owner(to);
            }
        }
        if self.slots.len() < list.len() {
            self.slots.resize_with(list.len(), || Slot { part: None, cursors: Vec::new() }); // alloc: Node
        }
        if self.datums.len() < (self.recipe.natts + 1) * MAX_FETCH_ROWS {
            self.datums.resize(self.recipe.natts * MAX_FETCH_ROWS, 0); // alloc: Node charged self.held.owner()
            self.valid.resize(self.recipe.natts * VALIDITY_WORDS / 8, 0); // alloc: Node charged self.held.owner()
            self.dense.resize(MAX_FETCH_ROWS + 1, 0); // alloc: Node charged self.held.owner()
            self.vmask.resize(VALIDITY_WORDS, 0); // alloc: Node charged self.held.owner()
            self.order.reserve(MAX_FETCH_ROWS); // alloc: Node charged self.held.owner()
            self.sel.reserve(MAX_FETCH_ROWS); // alloc: Node charged self.held.owner()
            self.didx.reserve(MAX_FETCH_ROWS); // alloc: Node charged self.held.owner()
            self.arena.resize(ARENA_MIN / 8, 0); // alloc: Node charged self.held.owner()
        }
        self.list = Some(list);
        self.bound = true;
        self.held.sync(self.kept_bytes() as u64);
        Ok(())
    }

    fn seek(&mut self, _keys: &[Datum], _keys_null: &[u8]) -> Result<()> {
        Ok(())
    }

    /// A fetch site has no windows.
    fn window(&mut self, _w: &mut Window<'_>) -> Result<u32> {
        Ok(0)
    }

    fn fetch(&mut self, rowids: &[u64], lanes: &mut [Lane]) -> Result<u32> {
        if !self.bound {
            return Err(Error::host("gravity: fetch on an unbound source"));
        }
        let n = rowids.len();
        if n > MAX_FETCH_ROWS {
            return Err(Error::host("gravity: fetch request past MAX_FETCH_ROWS"));
        }
        let natts = self.recipe.natts;
        if lanes.len() < natts {
            return Err(Error::host("gravity: fetch site lanes"));
        }
        // the last fetch's images are dead (its lanes with them): only the current chunk is kept, the largest
        self.spent.clear();
        self.order.clear();
        self.order.extend(0..n as u32); // alloc: Node charged self.held.owner()
        // bounded("small-fixed: one fetch request's rows, at most MAX_FETCH_ROWS (1024)")
        self.order.sort_unstable_by_key(|&i| rowids[i as usize]);
        self.valid[..natts * VALIDITY_WORDS / 8].fill(0);
        self.dense[MAX_FETCH_ROWS] = 0;
        let mut from = 0;
        // a tick per (part, granule) group: each pins its part and decodes its columns
        let mut step = cancelpoll::step("fetch_site fetch: a request's granule groups");
        while from < n {
            step.tick()?;
            let head = rowids[self.order[from] as usize] >> 13;
            let mut to = from + 1;
            // bounded("small-fixed: one fetch request's rows, at most MAX_FETCH_ROWS (1024)")
            while to < n && rowids[self.order[to] as usize] >> 13 == head {
                to += 1;
            }
            self.group(rowids, from, to)?;
            from = to;
        }
        // bounded("key-columns: one lane per fetched column, its validity over at most MAX_FETCH_ROWS rows, at most MaxTupleAttributeNumber (1664) columns")
        for k in 0..natts {
            let words = n.div_ceil(64);
            let all = self.valid[k * VALIDITY_WORDS / 8..][..words].iter().enumerate().all(|(w, &v)| v == if w + 1 == words && n % 64 != 0 { (1u64 << (n % 64)) - 1 } else { !0 });
            let valid = if all { core::ptr::null() } else { self.valid[k * VALIDITY_WORDS / 8..].as_ptr() };
            let mut l = Lane::byval(self.datums[k * MAX_FETCH_ROWS..].as_ptr() as *const u8, valid, 8, lane::REGION_ARENA, lane::id(lane::ID_ATTR, self.recipe.attnos[k] as u32));
            if self.recipe.kinds[k] == KIND_TEXT {
                l.kind = lane::KIND_BYREF;
            }
            lanes[k] = l;
        }
        self.fetched += n as u64;
        FETCH_DIAG[0].fetch_add(n as u64, core::sync::atomic::Ordering::Relaxed);
        self.held.sync(self.kept_bytes() as u64);
        Ok(n as u32)
    }

    fn cursor(&self) -> &Cursor {
        &self.cur
    }

    fn cursor_mut(&mut self) -> &mut Cursor {
        &mut self.cur
    }

    fn lanes_hint(&self) -> usize {
        self.recipe.natts + 2
    }

    fn refetch(&mut self, _values: &mut [Datum], _isnull: &mut [u8]) -> Result<bool> {
        Err(crate::unsupported())
    }

    fn substitute(&mut self, _pos: ScanPos) {}

    fn unbind(&mut self) {
        self.bound = false;
        self.list = None;
        // C8: the kept cursors' part uses pass to the backend's session (D1507: they outlive the statement)
        // bounded("other: one O(1) owner move per kept cursor, the read columns of the parts this source keeps")
        for c in self.slots.iter_mut().flat_map(|s| s.cursors.iter_mut()).flatten() {
            c.keep_past_statement();
        }
        self.held.park(self.kept_bytes() as u64);
    }

    /// D3213: the published cells and the decode buffers, kept between statements (`slots` is not: `release_rel` takes it).
    fn kept_bytes(&self) -> usize {
        use gravity_driver::vec_bytes as v;
        v(&self.datums) + v(&self.valid) + v(&self.arena) + v(&self.spent) + self.spent.iter().map(v).sum::<usize>() + v(&self.order) + v(&self.sel) + v(&self.didx) + v(&self.dense) + v(&self.vmask)
    }

    /// D1507: an unbound source's kept parts and cursors (each holds its part file open) moved out; the next bind grows the
    /// slots again (the generation reset).
    fn release_rel(&mut self, rel: u32) -> Option<Box<dyn core::any::Any>> {
        if self.bound || (rel != 0 && rel != self.recipe.rel) || self.slots.is_empty() {
            return None;
        }
        self.held_generation = 0;
        Some(Box::new(core::mem::take(&mut self.slots))) // alloc: Exec
    }

    /// D4768: the relation of its recipe.
    fn rel_read(&self) -> u32 {
        self.recipe.rel
    }

    /// The slot depot's hook: the kept slots' heap (`held`) to `to` (the depot's holder at a park, the taking statement at
    /// a take), and the kept cursors' own tables with it (cvpread's `transfer_kept`; their part uses stay where
    /// `kept_uses_to` put them: `Cache(Parked)`).
    fn transfer_held(&mut self, to: memledger::Owner, release: memledger::Release) {
        if self.bound {
            return;
        }
        self.held.transfer(to, release);
        // bounded("other: one O(1) owner move per kept cursor, the read columns of the parts this source keeps")
        for c in self.slots.iter_mut().flat_map(|s| s.cursors.iter_mut()).flatten() {
            c.transfer_kept(to, release);
        }
    }

    /// C8 (D4087): the kept slots' cursors' part uses to `to` (a parked worker set's: the depot's).
    fn kept_uses_to(&mut self, to: memledger::Owner, release: memledger::Release) {
        if self.bound {
            return;
        }
        // bounded("other: one O(1) owner move per kept cursor, the read columns of the parts this source keeps")
        for c in self.slots.iter_mut().flat_map(|s| s.cursors.iter_mut()).flatten() {
            c.transfer_use(to, release);
        }
    }

    fn residency(&self) -> Residency {
        let held = self.slots.iter().filter_map(|s| s.part.as_ref());
        Residency { parts: held.clone().count() as u32, bytes: held.map(|p| p.resident()).sum() }
    }
}
