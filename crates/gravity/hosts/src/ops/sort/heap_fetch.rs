//! The heap's by-rowid fetch site (road topk.fetch over a heap relation; lowerv4 ops/sort/strategies/fetch.rs): a wide top
//! keeps `(key, locator)` rows, the locator its row's TID word (`block << 16 | offset`, kernels ops/scan entries.rs
//! `tid_word`), and its emit asks this source for the winners' columns, at most `MAX_FETCH_ROWS` a request. Each TID is
//! fetched once, in block order, under the statement's snapshot (the version the scan read: same snapshot, same
//! statement), and its columns deformed: a by-value column published as its datum word, a varlena as a pointer to its
//! image copied into this source's arena (the fetched tuple's buffer is released at the next row's fetch). A request's
//! images stay where its lanes point until the next request. A TID the snapshot cannot see raises. The leader fetches: a
//! worker's bind holds nothing.
use std::rc::{Rc, Weak};
use std::sync::Arc;

use gravity_abi::ctx::{lane, Lane, ScanPos};
use gravity_abi::limits::HEAP_FETCH_ARENA_MIN;
use gravity_abi::site::MAX_FETCH_ROWS;
use gravity_abi::Datum as Word;
use gravity_driver::{box_source, ArenaRef, BindingRef, Cursor, Error, Result, Source, SourceBox, Window};
use gravity_program::SiteDesc;
use types_core::Oid;
use types_error::PgResult;
use types_rel::{Relation, RelationData, RELKIND_MATVIEW, RELKIND_RELATION};
use types_slot::SlotData;
use types_snapshot::SnapshotData;
use types_storage::lock::NoLock;
use types_tuple::ItemPointerData;

use crate::ops::shared::heap_fetch::fetch_by_tid;
use crate::ops::shared::raw::{open_heap, snapshot_of};
use crate::ops::shared::rel::park;
use crate::recipe::HostRecipe;
use crate::scratch::NodeScratch;

mod raw;

const VALID_WORDS: usize = MAX_FETCH_ROWS / 64;

/// The fetch site's columns: `attnos` of a heap relation, each by value (its datum word) or a varlena (`lens` -1).
#[derive(Clone)]
pub struct HeapFetchRecipe {
    pub heap: Oid,
    heap_natts: i32,
    pub attnos: Arc<[i16]>,
    lens: Arc<[i16]>,
}

impl HeapFetchRecipe {
    pub fn from_plan(heap: &Relation<'_>, attnos: &[i16]) -> PgResult<HeapFetchRecipe> {
        use tableam_vocab::TableAm;
        if !matches!(heap.rd_rel.relkind, RELKIND_RELATION | RELKIND_MATVIEW) || TableAm::of(heap) != Some(TableAm::Heap) || attnos.is_empty() {
            return Err(crate::pg_err("gravity: a heap fetch site needs a heap relation and its columns"));
        }
        let mut lens = Vec::with_capacity(attnos.len()); // alloc: Program
        // bounded("key-columns: one attribute test per fetched column, at plan time, at most MaxTupleAttributeNumber (1664) columns")
        for &a in attnos {
            let Some(att) = heap.rd_att.attrs.get((a as usize).wrapping_sub(1)).filter(|t| a > 0 && !t.attisdropped) else {
                return Err(crate::pg_err("gravity: heap fetch attno"));
            };
            if !att.attbyval && att.attlen != -1 {
                return Err(crate::pg_err("gravity: a heap fetch reads no fixed-length by-reference column"));
            }
            lens.push(att.attlen); // alloc: Program
        }
        Ok(HeapFetchRecipe { heap: heap.rd_id, heap_natts: heap.rd_att.natts, attnos: Arc::from(attnos), lens: Arc::from(lens) }) // alloc: Program
    }
}

impl HostRecipe for HeapFetchRecipe {
    fn rel(&self) -> Oid {
        self.heap
    }
    fn size_hint(&self) -> usize {
        core::mem::size_of::<HeapFetchSource>()
    }
    fn derive(&self, desc: &SiteDesc, a: ArenaRef) -> SourceBox {
        assert!(self.heap == desc.relation, "gravity: heap fetch recipe does not match its site descriptor");
        box_source(HeapFetchSource::new(self.clone()), a) // alloc: Node
    }
    fn clone_box(&self) -> Box<dyn HostRecipe> {
        Box::new(self.clone()) // alloc: Program
    }
}

pub struct HeapFetchSource {
    recipe: HeapFetchRecipe,
    heap: Option<Relation<'static>>,
    parked: Option<Weak<RelationData<'static>>>,
    snapshot: Option<Rc<SnapshotData<'static>>>,
    slot: Option<SlotData<'static>>,
    /// The published cells: `natts × MAX_FETCH_ROWS` datum words and their validity words.
    datums: Vec<u64>,
    valid: Vec<u64>,
    /// The current request's varlena images: `arena` from byte 0 to `used`, and the chunks it outgrew in `spent` (a grown
    /// arena is a fresh chunk, never the old one moved: the request's published pointers stay where they point).
    arena: Vec<u64>,
    used: usize,
    spent: Vec<Vec<u64>>,
    /// The request's rows in block order. memsig E-pub t3: in the host's node scratch (declared after it), so it moves with
    /// the scratch (`transfer_held`, the bind's home); room for a full request taken at the first bind.
    order: mcx::PgVec<'static, u32>,
    row: Vec<Word>,
    nulls: Vec<u8>,
    bound: bool,
    cur: Cursor,
    scratch: NodeScratch,
}

impl HeapFetchSource {
    pub fn new(recipe: HeapFetchRecipe) -> HeapFetchSource {
        let natts = recipe.attnos.len();
        let scratch = NodeScratch::new("GravityHeapFetch");
        let order = mcx::PgVec::new_in(scratch.mcx());
        HeapFetchSource { recipe, heap: None, parked: None, snapshot: None, slot: None, datums: Vec::new(), valid: Vec::new(), arena: Vec::new(), used: 0, spent: Vec::new(), order, row: vec![0; natts], nulls: vec![0; natts], bound: false, cur: Cursor::new(), scratch } // alloc: Node
    }

    fn bind_pg(&mut self, snapshot: Rc<SnapshotData<'static>>) -> PgResult<()> {
        let mcx = self.scratch.mcx();
        let heap = open_heap(mcx, &self.parked, self.recipe.heap)?;
        if heap.rd_att.natts != self.recipe.heap_natts {
            heap.close(NoLock)?;
            return Err(crate::pg_err("gravity: relation changed under the parked plan"));
        }
        if self.slot.is_none() {
            self.slot = Some(tableam::table_slot_create(mcx, &heap)?);
        }
        let natts = self.recipe.attnos.len();
        if self.datums.len() < natts * MAX_FETCH_ROWS {
            self.datums.resize(natts * MAX_FETCH_ROWS, 0); // alloc: Node
            self.valid.resize(natts * VALID_WORDS, 0); // alloc: Node
            mcx::GrowIn::resize_in(&mut self.order, MAX_FETCH_ROWS, 0);
            self.order.clear();
            self.arena.resize(HEAP_FETCH_ARENA_MIN / 8, 0); // alloc: Node
        }
        (self.heap, self.snapshot, self.bound) = (Some(heap), Some(snapshot), true);
        Ok(())
    }

    fn unbind_pg(&mut self) -> PgResult<()> {
        if !core::mem::replace(&mut self.bound, false) {
            return Ok(());
        }
        if let Some(slot) = self.slot.as_mut() {
            exectuples::exec_clear_tuple(slot, self.scratch.mcx());
        }
        // the last request's images stay: a call stopped inside its rows reads them again on its resume
        (self.parked, self.snapshot) = (park(self.heap.take())?, None);
        self.scratch.reset_exec();
        Ok(())
    }

    /// Request entry `i`: the row at TID word `w`, its columns into entry `i` of each lane.
    fn row_at(&mut self, w: u64, i: usize) -> Result<()> {
        let (Some(heap), Some(slot), Some(snapshot)) = (self.heap.as_ref(), self.slot.as_mut(), self.snapshot.as_ref()) else {
            return Err(Error::host("gravity: heap fetch on an unbound source"));
        };
        let tid = ItemPointerData::new((w >> 16) as u32, w as u16);
        if !fetch_by_tid(self.scratch.exec_mcx(), heap, &tid, snapshot, slot, &self.recipe.attnos, &mut self.row, &mut self.nulls).map_err(crate::stash)? {
            return Err(Error::host("gravity: a fetched heap row is not visible under the statement's snapshot"));
        }
        // bounded("key-columns: one cell per fetched column of a row, at most MaxTupleAttributeNumber (1664) columns")
        for k in 0..self.recipe.attnos.len() {
            let at = k * MAX_FETCH_ROWS + i;
            if self.nulls[k] != 0 {
                self.datums[at] = 0;
                continue;
            }
            self.datums[at] = if self.recipe.lens[k] == -1 { self.copy_image(self.row[k]) } else { self.row[k] };
            self.valid[k * VALID_WORDS + i / 64] |= 1 << (i % 64);
        }
        Ok(())
    }

    /// The varlena at datum `d` (the slot's current tuple) copied into the arena, 8-byte aligned: its address.
    fn copy_image(&mut self, d: Word) -> Word {
        let img = raw::varlena_bytes(d as usize as *const u8);
        let len = img.len();
        if self.used + len > self.arena.len() * 8 {
            let fresh = vec![0u64; (len + HEAP_FETCH_ARENA_MIN).div_ceil(8).max(self.arena.len())]; // alloc: Node
            self.spent.push(core::mem::replace(&mut self.arena, fresh)); // alloc: Node
            self.used = 0;
        }
        let dst = &mut raw::bytes_mut(&mut self.arena)[self.used..self.used + len];
        dst.copy_from_slice(img);
        let at = dst.as_ptr() as usize as Word;
        self.used = (self.used + len).next_multiple_of(8);
        at
    }
}

impl Source for HeapFetchSource {
    fn bind(&mut self, b: &BindingRef<'_>) -> Result<()> {
        if b.ordinal.is_some() {
            return Ok(());
        }
        self.scratch.home_statement();
        self.bind_pg(snapshot_of(b)).map_err(crate::stash)
    }

    fn seek(&mut self, _keys: &[Word], _keys_null: &[u8]) -> Result<()> {
        Ok(())
    }

    /// A fetch site has no windows.
    fn window(&mut self, _w: &mut Window<'_>) -> Result<u32> {
        Ok(0)
    }

    fn fetch(&mut self, rowids: &[u64], lanes: &mut [Lane]) -> Result<u32> {
        let (n, natts) = (rowids.len(), self.recipe.attnos.len());
        if !self.bound {
            return Err(Error::host("gravity: heap fetch on an unbound source"));
        }
        if n > MAX_FETCH_ROWS || lanes.len() < natts {
            return Err(Error::host("gravity: heap fetch request shape"));
        }
        // the last request's images are dead with its lanes
        self.spent.clear();
        self.used = 0;
        self.valid[..natts * VALID_WORDS].fill(0);
        self.order.clear();
        mcx::GrowIn::extend_in(&mut self.order, 0..n as u32);
        // bounded("small-fixed: one request's rows, at most MAX_FETCH_ROWS (1024), an integer sort")
        self.order.sort_unstable_by_key(|&i| rowids[i as usize]);
        // a row's heap fetch is the unit (a page read each: a request's 1024 rows can be as many reads from storage)
        let mut step = cancelpoll::step("heap_fetch fetch: one row fetched by its TID");
        for o in 0..n {
            step.tick()?;
            let i = self.order[o] as usize;
            self.row_at(rowids[i], i)?;
        }
        let words = n.div_ceil(64);
        // bounded("key-columns: one lane per fetched column, at most MaxTupleAttributeNumber (1664) columns")
        for k in 0..natts {
            let v = &self.valid[k * VALID_WORDS..][..words];
            let all = v.iter().enumerate().all(|(w, &x)| x == if w + 1 == words && n % 64 != 0 { (1u64 << (n % 64)) - 1 } else { !0 });
            let valid = if all { core::ptr::null() } else { v.as_ptr() };
            let mut l = Lane::byval(self.datums[k * MAX_FETCH_ROWS..].as_ptr() as *const u8, valid, 8, lane::REGION_ARENA, lane::id(lane::ID_ATTR, self.recipe.attnos[k] as u32));
            if self.recipe.lens[k] == -1 {
                l.kind = lane::KIND_BYREF;
            }
            lanes[k] = l;
        }
        Ok(n as u32)
    }

    fn cursor(&self) -> &Cursor {
        &self.cur
    }

    fn cursor_mut(&mut self) -> &mut Cursor {
        &mut self.cur
    }

    fn lanes_hint(&self) -> usize {
        self.recipe.attnos.len() + 2
    }

    fn refetch(&mut self, _values: &mut [Word], _isnull: &mut [u8]) -> Result<bool> {
        Err(crate::unsupported())
    }

    fn substitute(&mut self, _pos: ScanPos) {}

    fn unbind(&mut self) {
        self.unbind_pg().expect("gravity: heap fetch source unbind")
    }

    /// D4464: the slot depot's hook: the source's scratch to `to` (the depot's holder at a worker set's park, the taking
    /// statement at its take).
    fn transfer_held(&mut self, to: memledger::Owner, _release: memledger::Release) {
        self.scratch.home(to);
    }

    /// D3213: the published cells and the image arena, kept between statements.
    fn kept_bytes(&self) -> usize {
        use gravity_driver::vec_bytes as v;
        v(&self.datums) + v(&self.valid) + v(&self.arena) + v(&self.spent) + self.spent.iter().map(v).sum::<usize>() + self.order.capacity() * core::mem::size_of::<u32>() + v(&self.row) + v(&self.nulls)
    }
}

impl Drop for HeapFetchSource {
    fn drop(&mut self) {
        let _ = self.unbind_pg();
        self.slot = None;
    }
}
