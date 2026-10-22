use gravity_abi::PtrWord;
use core::ops::{Deref, DerefMut};

use gravity_abi::ctx::lane;
use gravity_abi::site::{self as abi_site, Site};
use gravity_abi::status::Status;
use gravity_abi::{Answer, Datum, Lane, ScanPos};
use gravity_program::ZonePred;

use crate::binding::BindingRef;
use crate::{Error, Result};

mod raw;
pub use raw::{blank_site, site_source, site_source_mut};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WindowBase {
    pub pos: ScanPos,
    pub part_id: u32,
    pub dict_epoch: u32,
    pub granule: u32,
    pub claim_id: u32,
}

impl WindowBase {
    pub const fn at(pos: ScanPos) -> WindowBase {
        WindowBase { pos, part_id: 0, dict_epoch: 0, granule: 0, claim_id: 0 }
    }
}

/// The window a source fills: the `Site` block itself (abi-v2 §2.2), written in place. `over`
/// clears the per-window cells; the source then publishes lanes (`set`), `rows`, an optional
/// selection and the base. Everything else of the block is read through `Deref`.
pub struct Window<'a> {
    s: &'a mut Site,
}

impl<'a> Window<'a> {
    pub fn over(s: &'a mut Site) -> Window<'a> {
        s.rows = 0;
        s.cursor = 0;
        s.sel = gravity_abi::word(core::ptr::null());
        s.sel_n = 0;
        s.nlanes = 0;
        s.answered = 0;
        s.answer = gravity_abi::word(core::ptr::null());
        s.page = gravity_abi::word(core::ptr::null());
        s.page_flags = 0;
        s.part_facts = 0;
        Window { s }
    }

    pub fn select(&mut self, sel: &[u32]) {
        self.s.sel = gravity_abi::word(sel.as_ptr());
        self.s.sel_n = sel.len() as u32;
    }

    /// The window's part facts (`site::PART_ENTRIES_SAFE`); `over` clears them.
    pub fn set_part_facts(&mut self, f: u8) {
        self.s.part_facts = f;
    }

    /// The raw-page form: the pinned page and its flags (`site::PAGE_NONULLS` / `PAGE_FULL`) beside the offset lane.
    pub fn set_page(&mut self, page: *const u8, flags: u16) {
        self.s.page = gravity_abi::word(page);
        self.s.page_flags = flags;
    }

    /// A host-deformed window's page flags (`site::PAGE_RECHECK`); `over` clears them.
    pub fn set_page_flags(&mut self, flags: u16) {
        self.s.page_flags = flags;
    }

    pub fn set_base(&mut self, b: WindowBase) {
        self.s.pos = b.pos;
        self.s.part_id = b.part_id;
        self.s.dict_epoch = b.dict_epoch;
        self.s.granule = b.granule;
        self.s.claim_id = b.claim_id;
    }

    pub fn base(&self) -> WindowBase {
        WindowBase { pos: self.s.pos, part_id: self.s.part_id, dict_epoch: self.s.dict_epoch, granule: self.s.granule, claim_id: self.s.claim_id }
    }

    /// The answered form (addition f): `rows` and one `Answer` per site lane, no lanes.
    pub fn answer(&mut self, records: *const Answer) {
        self.s.answered = 1;
        self.s.answer = gravity_abi::word(records);
    }

    pub fn is_answered(&self) -> bool {
        self.s.answered != 0
    }
}

impl Deref for Window<'_> {
    type Target = Site;
    fn deref(&self) -> &Site {
        self.s
    }
}

impl DerefMut for Window<'_> {
    fn deref_mut(&mut self) -> &mut Site {
        self.s
    }
}

/// The row view over a source's current window, embedded by every Source
/// for the provided `next()` / `position()` (a test and tooling facility: the
/// kernels serve rows themselves through `gravity_abi::site::next_row`).
pub struct Cursor {
    /// The window's lanes, copied at `take` (grown once, at the source's first window).
    pub lanes: Vec<Lane>,
    /// The default `next`'s scratch site lanes (`Source::lanes_hint` of them).
    scratch: Vec<Lane>,
    pub nlanes: u16,
    pub rows: u32,
    pub row: u32,
    pub sel: *const u32,
    pub sel_n: u32,
    pub base: WindowBase,
    pub pos: ScanPos,
    pub answered: bool,
    offset_lane: i8,
}

impl Default for Cursor {
    fn default() -> Cursor {
        Cursor::new()
    }
}

impl Cursor {
    pub const fn new() -> Cursor {
        Cursor { lanes: Vec::new(), scratch: Vec::new(), nlanes: 0, rows: 0, row: 0, sel: core::ptr::null(), sel_n: 0, base: WindowBase { pos: ScanPos::NONE, part_id: 0, dict_epoch: 0, granule: 0, claim_id: 0 }, pos: ScanPos::NONE, answered: false, offset_lane: -1 } // alloc: Node
    }

    /// Forget the window (a seek, a release, an unbind).
    pub fn clear(&mut self) {
        self.rows = 0;
        self.row = 0;
        self.sel = core::ptr::null();
        self.sel_n = 0;
        self.nlanes = 0;
        self.offset_lane = -1;
    }

    pub fn take(&mut self, w: &Window<'_>) {
        let n = w.nlanes as usize;
        if self.lanes.len() < n {
            self.lanes.resize(n, Lane::NONE); // alloc: Node
        }
        self.lanes[..n].copy_from_slice(w.lanes());
        self.nlanes = w.nlanes;
        self.rows = w.rows;
        self.row = 0;
        self.sel = w.sel.ptr();
        self.sel_n = w.sel_n;
        self.base = w.base();
        self.answered = w.is_answered();
        self.offset_lane = self.lanes[..n].iter().position(|l| lane::id_kind(l.id) == lane::ID_OFFSET && !l.ptr.ptr().is_null()).map_or(-1, |i| i as i8);
    }

    pub const fn end(&self) -> u32 {
        if self.sel.is_null() {
            self.rows
        } else {
            self.sel_n
        }
    }

    pub const fn spent(&self) -> bool {
        self.row >= self.end()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Residency {
    pub parts: u32,
    pub bytes: u64,
}

/// `Source::kept_bytes`' unit: a buffer's capacity in bytes.
pub fn vec_bytes<T>(v: &Vec<T>) -> usize {
    v.capacity() * core::mem::size_of::<T>()
}

pub trait Source {
    fn bind(&mut self, b: &BindingRef<'_>) -> Result<()>;
    fn seek(&mut self, keys: &[Datum], keys_null: &[u8]) -> Result<()>;
    /// Fill the site's next window in place: `rows` (0 = the probe / claim is exhausted).
    fn window(&mut self, w: &mut Window<'_>) -> Result<u32>;
    fn cursor(&self) -> &Cursor;
    fn cursor_mut(&mut self) -> &mut Cursor;
    /// The site's zone predicates, filter-only lanes and answer lanes
    /// (`SiteDesc`), handed over once at Node build; a source that answers
    /// from statistics keeps them.
    fn zone(&mut self, _preds: &[ZonePred], _filter_lanes: &[u64], _answer_lanes: &[u64]) {}
    /// D3009: the site's disjunctions (`SiteDesc::zone_any`, group `i` ending at `ends[i]`), after `zone`; a bound one's
    /// value is the protocol's slot `base + i` (`base` = the site's `zone` length). A source that skips no window by
    /// them ignores them: they are never a conjunction.
    fn zone_any(&mut self, _preds: &[ZonePred], _ends: &[u16], _base: usize) {}
    /// The lanes a window of this source publishes at most: the default `next` sizes its scratch site by it.
    fn lanes_hint(&self) -> usize {
        0
    }
    /// D3014O: a non-range source a worker may call (a frozen-image btree probe, hosts index_probe.rs); a worker's every
    /// other non-range site traps (driver bind).
    fn serves_worker(&self) -> bool {
        false
    }
    /// skewsplit (D3320): this range site was just sought in a pool claim that carries a cut (`claimcut`); `drive`: it is the
    /// claim's pipeline's driving site. A source that can stop its claim at a moved end adopts the cut (`Cut::adopt`) and
    /// answers asks at its granule steps (`CutToken::step`); every other forbids further cuts, and one that cannot follow a
    /// cut already made fails the claim (no range read twice).
    fn claim_cut(&mut self, cut: &crate::claimcut::Cut, drive: bool) -> Result<()> {
        let _ = drive;
        cut.forbid();
        if cut.was_cut() {
            return Err(Error::host("gravity: a cut claim reached a range site that cannot follow the cut"));
        }
        Ok(())
    }
    /// The value behind dictionary code `code` of code lane `lane` in part
    /// `part_id`: the datum (a varlena image resident with the part) and its
    /// payload length — the leader's cross-part fold (step 4).
    fn dict_value(&mut self, _part_id: u32, _lane: u16, _code: u32) -> Result<(Datum, u32)> {
        Err(Error::host("gravity: this source has no dictionary"))
    }
    /// One row of the current window into `values` / `isnull` (the row protocol over the cursor).
    fn next(&mut self, values: &mut [Datum], isnull: &mut [u8]) -> Result<bool> {
        loop {
            let c = self.cursor_mut();
            if !c.spent() {
                c.serve(values, isnull);
                return Ok(true);
            }
            let hint = self.lanes_hint();
            assert!(hint > 0, "gravity: a source served by the row protocol declares its lanes (`Source::lanes_hint`)");
            let c = self.cursor_mut();
            if c.scratch.len() < hint {
                c.scratch.resize(hint, Lane::NONE); // alloc: Node
            }
            let lanes = c.scratch.as_mut_ptr();
            let mut site = blank_site(lanes);
            let mut w = Window::over(&mut site);
            let n = self.window(&mut w)?;
            if n > 0 && (w.is_answered() || w.lanes().iter().any(|l| l.ptr.ptr().is_null())) {
                return Err(Error::host("gravity: the row protocol over an answered window or an undecoded lane"));
            }
            self.cursor_mut().take(&w);
            if n == 0 {
                return Ok(false);
            }
        }
    }
    fn position(&self) -> ScanPos {
        self.cursor().pos
    }
    fn lane(&mut self, _k: u16, _sel: &[u32], _out: &mut Lane) -> Result<u32> {
        Ok(0)
    }
    fn fetch(&mut self, _rowids: &[u64], _lanes: &mut [Lane]) -> Result<u32> {
        Ok(0)
    }
    fn refetch(&mut self, values: &mut [Datum], isnull: &mut [u8]) -> Result<bool>;
    fn substitute(&mut self, pos: ScanPos);
    /// Gives back what `bind` took. D4602: also after a failed bind, on the host whose bind failed partway and on hosts
    /// never bound (`Node::apply`'s and a worker slot's failure paths): what a partial bind took is given back, and a host
    /// never bound is left as it is.
    fn unbind(&mut self);
    fn pins(&self) -> u64 {
        0
    }
    fn release(&mut self) {}
    /// D3196 (rearm E1): a warm worker slot's source, still bound (no unbind and no bind since its last section), at the next
    /// section's open (another member's, or the next rescan's), on the leader: the work its unbind did after every section
    /// that is not the binding's (pages its claims staged for a prune, the runs its parts published for that section's owner
    /// phase). Default: nothing.
    fn rearm(&mut self) -> Result<()> {
        Ok(())
    }
    /// D4630 (b): a worker slot's source whose bind may run at the slot's first claim, on the claiming thread (under the
    /// switch, `slots::bind_at_claim`), not on the leader at the section's open: its bind reads only the binding and its
    /// own state (no state of the leader's backend), keeps no pin past the claim, and its work goes on the claim's home.
    /// A slot binds at its claim only when every host it holds says so. Default: no.
    fn binds_at_claim(&self) -> bool {
        false
    }
    /// D3670: the leader's source of the site a rerun set's or an InitPlan's member pipeline drives, as that member runs
    /// (`Node::count_scan`): the scan PostgreSQL counts as it begins it, or rescans it, at that run (heapam `initscan`,
    /// once per begin and per `heap_rescan`), which the source's bind left to the runs. Default: nothing (a source whose
    /// bind counts its scan, or that counts none).
    fn count_scan(&mut self) {}
    /// D1507: what an UNBOUND source keeps of relation `rel` across statements (`0`: any relation) — cursors, pins, open
    /// parts, each holding its part file open — moved out for the caller to drop after its lock (`slots::drain_released`).
    /// A parked source keeps them by design (one list generation; a bind drops them on a new one), so a dropped relation's
    /// unlinked part files stayed open for as long as the slot sat parked. `None`: nothing kept (the default), or bound.
    fn release_rel(&mut self, _rel: u32) -> Option<Box<dyn core::any::Any>> {
        None
    }
    /// D4768: the relation this source reads (0: none of its own, the default): the slot depot evicts a parked set that
    /// reads a relation whose storage went (`SlotDepot::evict_reading`).
    fn rel_read(&self) -> u32 {
        0
    }
    /// C8 (D4087): the part uses an UNBOUND source keeps across statements (its kept cursors, D1507) passed to owner `to`,
    /// released at `release`: a worker set parking in the process-wide depot (`Node::release_workers`) gives them to the
    /// depot (`Cache(Parked)`), which outlives the backend that last ran the set, so no backend's session or rank keeps
    /// what no statement of its holds. Default: nothing kept. A bound source keeps its owners.
    fn kept_uses_to(&mut self, _to: memledger::Owner, _release: memledger::Release) {}
    /// D4554 (`kept_parked`): the kept uses an unbind left on its statement or portal for a park that did not take them,
    /// moved to the session (`Node::settle_kept`, at the statement's end, before its account ends). Default: none left.
    fn settle_kept(&mut self) {}
    /// RESIDUE (the slot DEPOT holder): every charge this source holds for its own kept heap (its held
    /// charge, its kept cursors') moved to `to`, released at `release`. Called on the leader thread with the set's workers
    /// idle:
    /// - at the park (`Node::release_workers`): `to` = `Owner::Cache(Holder::SlotDepot)`, `release` = `Release::Evict`;
    /// - at the take (`Node::provision` after `DEPOT.take`): `to` = the taking statement's owner and release
    ///   (`memledger::charge::context()`); a breaker that binds the source later homes it there.
    /// The idle release and the depot's trims call nothing: the set drops with its charges, on the dropping thread. Default:
    /// nothing held.
    fn transfer_held(&mut self, _to: memledger::Owner, _release: memledger::Release) {}
    fn residency(&self) -> Residency {
        Residency::default()
    }
    /// D3213: the heap this source keeps between statements, its buffers' capacity (sized at its binds, kept at their high
    /// water): a parked slot's and Node's bytes count it, so the depot's and the park's caps and trims see it. Unchanged by
    /// `release_rel` and while unbound (the park keeps a running total).
    fn kept_bytes(&self) -> usize {
        0
    }
    fn park(&mut self) {}
    fn unpark(&mut self) {}
    /// D4554: the bytes `park` would give back now: a warm worker set's buffers in the slot depot (the keep-warm switch
    /// leaves them with the set), which the depot's reclaim parks under pressure. Default: none.
    fn park_bytes(&self) -> usize {
        0
    }
    /// Road dict.entries' runs (hosts granule_source/entry_runs.rs, a count(DISTINCT) over a text column): the consumer's table open offers its record layout
    /// and radix bits (kernels host_abi v4 `entry_runs_offer`). A source that can hand a part's entries to the owner phase
    /// as resident runs of that layout does so only after an accepted offer; the default ignores it (every part's rows).
    fn entry_runs_offer(&mut self, _layout: &gravity_abi::breaker::RecordLayout, _bits: u8) {}
    /// The runs this source's parts published in place of their rows, read beside the partials by the owner phase at
    /// `pbits` partition bits (`section.rs own_with`, `entry.rs own_single_of`); `None`: none.
    fn owner_input(&self, _pbits: u8) -> Option<&dyn crate::SpillFile> {
        None
    }
    /// `owner_input` is `Some` at every partition count (whether any part published runs).
    fn has_owner_input(&self) -> bool {
        false
    }
}

/// The per-site call targets a source kind binds into its `Site` (abi-v2 §2.2): the
/// monomorphised host functions over the concrete `S` (`hostimpl::source`).
#[derive(Clone, Copy)]
pub struct SiteCalls {
    pub next: abi_site::NextFn,
    pub lane: abi_site::LaneFn,
    pub fetch: abi_site::LaneFn,
    pub dict: abi_site::DictFn,
}

impl SiteCalls {
    pub const NONE: SiteCalls = SiteCalls { next: None, lane: None, fetch: None, dict: None };

    pub fn of<S: Source + 'static>() -> SiteCalls {
        SiteCalls { next: Some(crate::hostimpl::source::next::<S>), lane: Some(crate::hostimpl::source::lane::<S>), fetch: Some(crate::hostimpl::source::fetch::<S>), dict: Some(crate::hostimpl::source::dict_value::<S>) }
    }
}

/// A driver `Error` as the status a host returns; the error itself is stored behind `env`.
pub(crate) fn status_of(e: &Error) -> i64 {
    match e {
        Error::Cancelled => Status::Cancelled.encode(),
        Error::Unsupported(k) => Status::Unsupported { kind: *k }.encode(),
        _ => Status::Error.encode(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteOp {
    Insert,
    Update,
    Delete,
}

impl WriteOp {
    pub const fn from_u32(op: u32) -> Option<WriteOp> {
        match op {
            gravity_abi::program::WRITE_INSERT => Some(WriteOp::Insert),
            gravity_abi::program::WRITE_UPDATE => Some(WriteOp::Update),
            gravity_abi::program::WRITE_DELETE => Some(WriteOp::Delete),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Done,
    Skipped,
    Recheck,
    /// ON CONFLICT DO UPDATE: the conflicting row is locked; `Writer::existing` fills the conflict site.
    Conflict,
    /// M8-WRITE W6: a MERGE's MATCHED action over a concurrently deleted target row (`WRITE_NOT_MATCHED`).
    NotMatched,
    /// W6-D1112: the row's check option parked at a SubPlan read (`WRITE_SUBPLAN`): `Writer::sub_call`, then `resume`.
    SubPlan,
}

pub trait Writer {
    fn bind(&mut self, b: &BindingRef<'_>) -> Result<()>;
    fn write(&mut self, op: WriteOp, pos: ScanPos, values: &[Datum], isnull: &[u8]) -> Result<Outcome>;
    fn recheck_pos(&self) -> ScanPos {
        ScanPos::NONE
    }
    /// After `Outcome::Conflict`: the locked conflicting row's cells, its position returned (the update's).
    fn existing(&mut self, _values: &mut [Datum], _isnull: &mut [u8]) -> Result<ScanPos> {
        Err(Error::host("gravity: a conflict outcome from a writer without ON CONFLICT DO UPDATE"))
    }
    /// After `Outcome::Done` of a write whose plan fetches the replaced version (`WritePlan.old`, RETURNING OLD, M4-RET
    /// R4a): that version's cells into the old site (the write site's successor); `false` without one.
    fn old(&mut self, _values: &mut [Datum], _isnull: &mut [u8]) -> Result<bool> {
        Ok(false)
    }
    /// D774: after `Outcome::Done` of a write whose plan hands back the written row's system columns (`WritePlan.sys`,
    /// RETURNING ctid / xmin / ..): their six cells (ctid, xmin, cmin, xmax, cmax, tableoid) into the sys site, the write
    /// site + `sys_off()`; `false` without one.
    fn sys(&mut self, _values: &mut [Datum], _isnull: &mut [u8]) -> Result<bool> {
        Ok(false)
    }
    /// D774: the sys site's offset from the write site (1 + the old site's), 0 without one.
    fn sys_off(&self) -> usize {
        0
    }
    /// After `Outcome::Done`: the row as written when it is not the kernel's cells (a BEFORE ROW trigger
    /// replaced them) — `true` rewrote `values` / `isnull` in place (RETURNING projects over them).
    fn written(&mut self, _values: &mut [Datum], _isnull: &mut [u8]) -> bool {
        false
    }
    /// D655 Row C (V4R-534, `WRITE_EPQ_ROW`): after `Outcome::Recheck`, the newest version the recheck locked: its cells
    /// into the write site, its position returned, pinned until the next such call or unbind. Without one: an error.
    fn epq_row(&mut self, _values: &mut [Datum], _isnull: &mut [u8]) -> Result<ScanPos> {
        Err(Error::host("gravity: an EvalPlanQual row from a writer that keeps no locked version"))
    }
    /// ON CONFLICT DO UPDATE (`WRITE_CONFLICT_CHECK`): after `Outcome::Conflict` and the arm's WHERE, the row-level security
    /// check over the existing row (WCO_RLS_CONFLICT_CHECK). A writer with no such policies checks nothing.
    fn conflict_check(&mut self) -> Result<()> {
        Ok(())
    }
    /// W6-D1112: after `Outcome::SubPlan`, the parked call's words (gravity_abi::program's `SUB_*` layout), alive until
    /// the resume.
    fn sub_call(&mut self) -> Result<*mut u64> {
        Err(Error::host("gravity: a SubPlan call from a writer that parks none"))
    }
    /// W6-D1112: the parked row op resumed over the same row, its SubPlan's result in the call's words.
    fn resume(&mut self, _op: WriteOp, _pos: ScanPos, _values: &[Datum], _isnull: &[u8]) -> Result<Outcome> {
        Err(Error::host("gravity: a SubPlan resume on a writer that parks none"))
    }
    /// Gives back what `bind` took. D4602: also after a failed bind, on the host whose bind failed partway and on hosts
    /// never bound (`Node::apply`'s and a worker slot's failure paths): what a partial bind took is given back, and a host
    /// never bound is left as it is.
    fn unbind(&mut self);
    fn park(&mut self) {}
    fn unpark(&mut self) {}
}

pub trait Locker {
    fn bind(&mut self, b: &BindingRef<'_>) -> Result<()>;
    fn lock(&mut self, pos: ScanPos) -> Result<Outcome>;
    fn recheck_pos(&self) -> ScanPos {
        ScanPos::NONE
    }
    /// C5b: ExecLockRows' erm->curCtid, the row the last lock left locked (its leaf relation in `rel`), for WHERE CURRENT
    /// OF; `NONE` before any.
    fn current(&self) -> ScanPos {
        ScanPos::NONE
    }
    /// RL1b: after `Outcome::Recheck`, the newest version the lock traversed to (the EPQ test tuple): its cells into
    /// the lock site, its position returned. Never locks again (D604). A locker without cells refuses.
    fn existing(&mut self, _values: &mut [Datum], _isnull: &mut [u8]) -> Result<ScanPos> {
        Err(Error::host("gravity: a lock host without the locked row's cells"))
    }
    /// REFFETCH (`LockSite::FETCH`): an in-IR recheck's REFERENCE-marked row, EvalPlanQualFetchRowMark's: the
    /// version at `pos` (its relation, its tid) under SnapshotAny, its cells into `values` / `isnull`, its position
    /// returned; nothing is locked. By-reference cells point into its pinned buffer until the next fetch or unbind.
    fn fetch(&mut self, _pos: ScanPos, _values: &mut [Datum], _isnull: &mut [u8]) -> Result<ScanPos> {
        Err(Error::host("gravity: a lock host that fetches no joined row"))
    }
    /// D604: the node runs as an EvalPlanQual recheck (`on`) until `epq(false)` or unbind. PG's EvalPlanQual re-evaluates the
    /// plan below LockRows over the rows ExecLockRows already locked and never locks again, so a lock in the recheck node
    /// answers `Done` without a `table_tuple_lock` (a second lock could traverse to a newer version and fail the recheck).
    fn epq(&mut self, _on: bool) {}
    /// Gives back what `bind` took. D4602: also after a failed bind, on the host whose bind failed partway and on hosts
    /// never bound (`Node::apply`'s and a worker slot's failure paths): what a partial bind took is given back, and a host
    /// never bound is left as it is.
    fn unbind(&mut self);
    fn park(&mut self) {}
    fn unpark(&mut self) {}
}
