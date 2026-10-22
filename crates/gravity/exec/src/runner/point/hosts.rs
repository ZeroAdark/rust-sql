//! pglpoint S3 (option B, DESIGN-DML section 10): the Gravity program's own hosts, driven with no program. The index walk
//! is `IndexSource` over the recipe the lowerer files for the plan's IndexScan (`SiteRecipe::from_plan`): its seek, next
//! and position, and its EvalPlanQual test of a substituted version (`substitute`: the newest version under SnapshotAny,
//! each key re-tested as PostgreSQL's IndexRecheck over indexqualorig). The write is `WriteHost`: CheckValidResultRel at
//! its bind, heapam's heap_delete (S3a) or the constraint check, heap_update and index maintenance (S3b) per row and
//! ExecDelete / ExecUpdate's outcome mapping (a concurrent update's newest version locked under READ COMMITTED, 40001
//! under a transaction snapshot). Nothing here decides a tuple: every decision is a host's or an access method's, the
//! same calls the program's pipeline makes; an UPDATE's new row is update.rs's (`NewRow`: ExecGetUpdateNewTuple). The
//! hosts (`Hosts`) are made per statement, or kept in S5's memo entry across the statements of its shape (keep.rs), as
//! a parked Gravity program keeps its own.
use std::rc::Rc;

use gravity_driver::{BindingRef, Outcome, ScanPos, Source, WriteOp, Writer};
use gravity_hosts::{IndexSource, SiteRecipe, WriteHost};
use gravity_hosts::ops::modifytable::write_constraints::ModifiedCols;
use gravity_program::{Cmd, WritePlan};
use types_core::Oid;
use types_error::{PgError, PgResult};
use types_nodes::list::NodeList;
use types_rel::{NoLock, Relation};
use types_snapshot::SnapshotData;

use super::probe;

const KEYS: usize = types_core::fmgr::INDEX_MAX_KEYS as usize;

/// A point probe's seek words: clause `i`'s value in word `i` (`SiteRecipe::build` gives an operator clause the word
/// `KeyWord::Key(i)`, in `indexqual`'s order), from the Const (`probe::key_of`; S3 takes no parameter).
struct Keys {
    words: [u64; KEYS],
    nulls: [u8; KEYS],
    n: usize,
}

impl Keys {
    fn of(indexqual: &NodeList<'_>) -> PgResult<Keys> {
        let mut k = Keys { words: [0; KEYS], nulls: [0; KEYS], n: indexqual.len() };
        if k.n > KEYS {
            return Err(internal("a point probe with more keys than an index has"));
        }
        for (i, clause) in indexqual.iter().enumerate() {
            let key = probe::key_of(clause).ok_or_else(|| internal("a point probe key that is not an operator over a constant"))?;
            let (v, null) = key.value.get(&[]).ok_or_else(|| internal("a point probe key with no value"))?;
            (k.words[i], k.nulls[i]) = (if null { 0 } else { v.as_u64() }, u8::from(null));
        }
        Ok(k)
    }

    fn words(&self) -> (&[u64], &[u8]) {
        (&self.words[..self.n], &self.nulls[..self.n])
    }
}

/// The table, the index and the clauses of a point probe (the relations opened and locked by the caller), and the columns
/// the walk reads: none for a DELETE (the row's position alone), an UPDATE's old row (`NewRow::cols`).
pub(super) struct Probe<'a, 'm, 'q> {
    pub heap: &'a Relation<'m>,
    pub index: &'a Relation<'m>,
    pub indexqual: &'a NodeList<'q>,
    pub cols: &'a [i16],
}

impl Probe<'_, '_, '_> {
    /// The lowerer's walk of the plan's IndexScan: a btree recipe over `cols` (in that order), the index already locked
    /// (`NoLock`: the caller opened it at the RTE's mode, ExecInitIndexScan's).
    pub(super) fn recipe(&self) -> PgResult<SiteRecipe> {
        SiteRecipe::from_plan(self.heap, self.index, self.indexqual, &NodeList::nil(), NoLock, self.cols)
    }
}

/// Where a recheck's walk comes from (EvalPlanQualStart's scan of the relation, built at the first recheck): the plan's
/// relations (`Plan`), or the recipe a kept entry keeps (`Kept`: skip.rs `Kept`, no relation open), moved into the walk
/// at its making (the kept hosts keep the walk after it, so it is made once).
pub(super) enum EpqRecipe<'a, 'm, 'q> {
    Plan(&'a Probe<'a, 'm, 'q>),
    Kept(&'a core::cell::Cell<Option<SiteRecipe>>),
}

impl EpqRecipe<'_, '_, '_> {
    fn make(&self) -> PgResult<SiteRecipe> {
        match self {
            EpqRecipe::Plan(p) => p.recipe(),
            EpqRecipe::Kept(r) => r.take().ok_or_else(|| internal("a kept recheck walk's recipe was taken")),
        }
    }
}

/// A point write's hosts: the write host, the walk and its EvalPlanQual walk (made at the first recheck). Nothing is bound
/// between statements: made for one statement, or kept across the statements of one memo entry (skip.rs `Kept`), each
/// bind re-opening the relations through the parked handles and the locks the statement holds (a host's `bind`).
pub(super) struct Hosts {
    w: WriteHost,
    src: IndexSource,
    /// In a context of its own (`EpqWalk`): inline, every kept write entry carried its 3,360 bytes, and a point
    /// statement rechecks only when a concurrent update moved its row.
    epq: Option<EpqWalk>,
    epq_bound: bool,
    /// The holder the hosts were homed on (`home`: a kept entry's), whose the recheck walk is too when a statement makes it.
    held: Option<memledger::Owner>,
}

mcx::bind!(EpqTy => EpqIn<'k>);

/// The recheck walk boxed in its own context (`EpqWalk::make`).
struct EpqIn<'k> {
    walk: mcx::PgBox<'k, IndexSource>,
}

type EpqWalk = mcx::McxOwned<EpqTy>;

/// `walk` in a small context of its own, named `held` (a kept entry's holder) when there is one, else the making
/// statement's as any context it makes.
fn epq_walk(walk: IndexSource, held: Option<memledger::Owner>) -> PgResult<EpqWalk> {
    let ctx = mcx::MemoryContext::new_start_small("GravityPointEpq");
    if let Some(o) = held {
        ctx.set_owner(o);
    }
    mcx::McxOwned::<EpqTy>::try_new(ctx, move |m| {
        let walk = mcx::PgBox::try_new_in(walk, m).map_err(|_| m.oom_boxed(core::mem::size_of::<IndexSource>()))?;
        Ok(EpqIn { walk })
    })
}

impl Hosts {
    /// ExecInitModifyTable's write host of `cmd` (with the statement's insertedCols / updatedCols, `modified`: the host
    /// copies them at its first bind) and ExecInitIndexScan's walk (`recipe`), nothing bound.
    pub(super) fn new(heap: Oid, cmd: Cmd, modified: Option<ModifiedCols>, recipe: SiteRecipe) -> Hosts {
        let w = WriteHost::new(&WritePlan { target_rel: heap, cmd, ..WritePlan::SELECT }).with_modified(modified);
        Hosts { w, src: IndexSource::new(recipe), epq: None, epq_bound: false, held: None }
    }

    /// `new` for a memo entry to keep (keep.rs): the hosts' contexts named the entry's holder `owner` at once, their
    /// bytes moving to it (`home`), so no statement ends holding them. Not made under `enter_as(owner)`: a root that takes
    /// the creating thread's owner is that owner's in the owner tree only, its bytes on the thread's account, so the
    /// holder's account read low by them (`pgrust_memory_owners`' unattributed row went below zero).
    /// Their roots small-first (`gravity_hosts::scratch::small_roots`): parked between statements they hold little.
    pub(super) fn new_held(heap: Oid, cmd: Cmd, modified: Option<ModifiedCols>, recipe: SiteRecipe, owner: memledger::Owner) -> Hosts {
        let mut h = {
            let _small = gravity_hosts::scratch::small_roots();
            Hosts::new(heap, cmd, modified, recipe)
        };
        h.home(owner);
        h
    }

    /// The hosts' contexts' footprints (a kept entry's bytes in the memo; the hosts themselves are in the entry's own
    /// context).
    pub(super) fn bytes(&self) -> usize {
        let sum = |p: [usize; 3]| p.iter().sum::<usize>();
        let [w, oc] = self.w.scratch_parts();
        sum(w) + sum(oc) + self.w.maint_bytes() + sum(self.src.scratch_parts()) + self.epq.as_ref().map_or(0, |e| e.context().mem_allocated() + e.with(|x| sum(x.walk.scratch_parts())))
    }

    /// `bytes` by piece, for REPORT's 'keep bytes' line: each scratch's roots (node / exec / bind) and the inline sizes.
    #[cold]
    pub(super) fn parts(&self) -> String {
        let epq = self.epq.as_ref().map(|e| (e.context().mem_allocated(), e.with(|x| x.walk.scratch_parts())));
        let [w, oc] = self.w.scratch_parts();
        format!("write {w:?} + bind {oc:?} + maint {}, walk {:?}, epq {epq:?}, Hosts {} B", self.w.maint_bytes(), self.src.scratch_parts(), core::mem::size_of::<Hosts>()) // alloc: Cold
    }

    /// The write host's and the walks' contexts on owner `to` (a kept entry's holder: at their making, and after each
    /// statement whose bind homed any of them on it).
    pub(super) fn home(&mut self, to: memledger::Owner) {
        let release = memledger::charge::release_of(to);
        self.w.home(to);
        self.src.transfer_held(to, release);
        if let Some(e) = self.epq.as_mut() {
            e.context().set_owner(to);
            e.with_mut(|x| x.walk.transfer_held(to, release));
        }
        self.held = Some(to);
    }

    /// ExecEndPlan's order: the recheck walk, the walk, then the write host (ExecEndModifyTable closes the result relation).
    fn unbind(&mut self) {
        if core::mem::take(&mut self.epq_bound) {
            if let Some(e) = self.epq.as_mut() {
                e.with_mut(|x| x.walk.unbind());
            }
        }
        self.src.unbind();
        self.w.unbind();
    }
}

/// A point DELETE's run, ExecutorStart to ExecutorFinish under the statement's snapshot (`snap`, pushed by the caller):
/// the output command id (es_output_cid), the AFTER trigger query bracket (`after_query`: the program's own), the write
/// host's bind (ExecInitModifyTable's CheckValidResultRel, before its subplan starts), the keys' EXECUTE gates
/// (ExecInitIndexScan's), the walk's bind; then per row ExecModifyTable's order: the walk's next (ExecScanFetch's interrupt
/// check first), the row deleted at its position (`delete_at`). Returns the rows deleted (es_processed).
pub(super) fn delete(h: &mut Hosts, epq: &EpqRecipe<'_, '_, '_>, indexqual: &NodeList<'_>, snap: &Rc<SnapshotData<'static>>) -> PgResult<u64> {
    let cid = xact::GetCurrentCommandId(true)?;
    crate::ops::modifytable::after_query::begin(Cmd::Delete);
    let parts = crate::glue::binding_parts(snap, cid, &[], &[], None);
    let b = parts.as_ref();
    h.w.bind(&b).map_err(crate::glue::pg_error)?;
    probe::key_gates(indexqual)?;
    let keys = Keys::of(indexqual)?;
    h.src.bind(&b).map_err(crate::glue::pg_error)?;
    let (kw, kn) = keys.words();
    h.src.seek(kw, kn).map_err(crate::glue::pg_error)?;
    let mut rows = 0u64;
    loop {
        cancelpoll::interrupts::check()?;
        if !h.src.next(&mut [], &mut []).map_err(crate::glue::pg_error)? {
            break;
        }
        let at = h.src.position();
        if delete_at(h, epq, &b, &keys, at)? {
            rows += 1;
        }
    }
    h.unbind();
    crate::ops::modifytable::after_query::end(Cmd::Delete)?;
    Ok(rows)
}

/// ExecDelete of the row at `pos`: the write host's heap_delete and outcome. On `Recheck` (READ COMMITTED, the row updated
/// since the snapshot: the host locked its newest version, table_tuple_lock FIND_LAST_VERSION) the EvalPlanQual test of
/// that version (`recheck`): a pass deletes it at its position (`goto ldelete`), a miss deletes nothing. True when a row
/// was deleted.
fn delete_at(h: &mut Hosts, epq: &EpqRecipe<'_, '_, '_>, b: &BindingRef<'_>, keys: &Keys, mut pos: ScanPos) -> PgResult<bool> {
    loop {
        match h.w.write(WriteOp::Delete, pos, &[], &[]).map_err(crate::glue::pg_error)? {
            Outcome::Done => return Ok(true),
            Outcome::Skipped => return Ok(false),
            Outcome::Recheck => {
                let at = h.w.recheck_pos();
                match recheck(h, epq, b, keys, at, &mut [], &mut [])? {
                    Some(at) => pos = at,
                    None => return Ok(false),
                }
            }
            _ => return Err(internal("a point DELETE's write host returned an outcome only a MERGE, an ON CONFLICT or a check option returns")),
        }
    }
}

/// EvalPlanQual over the walk (the program's recheck, exec rerun/epq.rs `recheck_on`: a transaction snapshot raises
/// first): the version at `at` substituted for the scan's row and tested by the walk's keys, the walk's columns into
/// `values` / `isnull`. `Some(position)` when it passes. The recheck walk is made at the statement's first recheck (or
/// kept from an earlier statement's) and bound once per statement.
fn recheck(h: &mut Hosts, epq: &EpqRecipe<'_, '_, '_>, b: &BindingRef<'_>, keys: &Keys, at: ScanPos, values: &mut [u64], isnull: &mut [u8]) -> PgResult<Option<ScanPos>> {
    if crate::rerun::epq::must_serialize() {
        return Err(crate::rerun::epq::serialization_failure());
    }
    if h.epq.is_none() {
        let mut e = IndexSource::new(epq.make()?);
        // a kept entry's recheck walk, made by the statement of its first recheck, is the entry's holder's as the rest
        if let Some(o) = h.held {
            e.transfer_held(o, memledger::charge::release_of(o));
        }
        h.epq = Some(epq_walk(e, h.held)?);
    }
    let bound = &mut h.epq_bound;
    h.epq.as_mut().expect("made").with_mut(|x| -> PgResult<Option<ScanPos>> {
        let s = &mut *x.walk;
        if !*bound {
            s.bind(b).map_err(crate::glue::pg_error)?;
            *bound = true;
        }
        s.substitute(at);
        let (kw, kn) = keys.words();
        s.seek(kw, kn).map_err(crate::glue::pg_error)?;
        // EvalPlanQualNext's fetch is ExecScanFetch's too: its CHECK_FOR_INTERRUPTS before the test tuple is handed out
        cancelpoll::interrupts::check()?;
        Ok(if s.next(values, isnull).map_err(crate::glue::pg_error)? { Some(s.position()) } else { None })
    })
}

/// An UPDATE's new row (keep.rs `SetKept`), as ExecGetUpdateNewTuple makes it from the scanned version and the SET values.
pub(super) trait NewRow {
    /// ExecInitIndexScan's projection: the SET expressions initialized (an EXECUTE privilege error here), after the write
    /// host's bind and before the keys' gates, PostgreSQL's order.
    fn init(&mut self) -> PgResult<()>;
    /// The cells the walk fills with a version's columns, in `Probe::cols`' order.
    fn old(&mut self) -> (&mut [u64], &mut [u8]);
    /// The new row over the cells `old` holds: the SET values computed over that version, its other columns kept.
    fn form(&mut self) -> PgResult<()>;
    /// The new row, one cell per column of the table.
    fn row(&self) -> (&[u64], &[u8]);
}

/// A point UPDATE's run, as `delete`'s (ExecutorStart to ExecutorFinish under `snap`): the output command id, the AFTER
/// trigger query bracket, the write host's bind (its first copies the statement's insertedCols / updatedCols: ExecConstraints'
/// DETAIL and the indexUnchanged hints read them), the SET expressions' init (`NewRow::init`), the keys' gates, the walk
/// over the old row's columns; then per row ExecModifyTable's order: the walk's next (its interrupt check first), the new
/// row formed and written at the row's position (`update_at`). Returns the rows updated (es_processed).
pub(super) fn update(h: &mut Hosts, epq: &EpqRecipe<'_, '_, '_>, indexqual: &NodeList<'_>, new: &mut dyn NewRow, snap: &Rc<SnapshotData<'static>>) -> PgResult<u64> {
    let cid = xact::GetCurrentCommandId(true)?;
    crate::ops::modifytable::after_query::begin(Cmd::Update);
    let parts = crate::glue::binding_parts(snap, cid, &[], &[], None);
    let b = parts.as_ref();
    h.w.bind(&b).map_err(crate::glue::pg_error)?;
    new.init()?;
    probe::key_gates(indexqual)?;
    let keys = Keys::of(indexqual)?;
    h.src.bind(&b).map_err(crate::glue::pg_error)?;
    let (kw, kn) = keys.words();
    h.src.seek(kw, kn).map_err(crate::glue::pg_error)?;
    let mut rows = 0u64;
    loop {
        cancelpoll::interrupts::check()?;
        let (v, n) = new.old();
        if !h.src.next(v, n).map_err(crate::glue::pg_error)? {
            break;
        }
        let at = h.src.position();
        if update_at(h, epq, &b, &keys, new, at)? {
            rows += 1;
        }
    }
    h.unbind();
    crate::ops::modifytable::after_query::end(Cmd::Update)?;
    Ok(rows)
}

/// ExecUpdate of the row at `pos`: the new row formed over the version the walk read, then the write host's constraint
/// check, heap_update, index maintenance and outcome. On `Recheck` (READ COMMITTED, the row updated since the snapshot:
/// the host locked its newest version) the EvalPlanQual test of that version (`recheck`, its columns into `new.old()`): a
/// pass forms the new row again over it, SET values recomputed (EvalPlanQual's projection), and writes at its position
/// (`goto lreplace`); a miss updates nothing. True when a row was updated.
fn update_at(h: &mut Hosts, epq: &EpqRecipe<'_, '_, '_>, b: &BindingRef<'_>, keys: &Keys, new: &mut dyn NewRow, mut pos: ScanPos) -> PgResult<bool> {
    new.form()?;
    loop {
        let (v, n) = new.row();
        match h.w.write(WriteOp::Update, pos, v, n).map_err(crate::glue::pg_error)? {
            Outcome::Done => return Ok(true),
            Outcome::Skipped => return Ok(false),
            Outcome::Recheck => {
                let at = h.w.recheck_pos();
                let (v, n) = new.old();
                match recheck(h, epq, b, keys, at, v, n)? {
                    Some(at) => {
                        pos = at;
                        new.form()?;
                    }
                    None => return Ok(false),
                }
            }
            _ => return Err(internal("a point UPDATE's write host returned an outcome only a MERGE, an ON CONFLICT or a check option returns")),
        }
    }
}

#[cold]
fn internal(msg: &str) -> Box<PgError> {
    Box::new(PgError::error(format!("point path: {msg}")).with_sqlstate(types_error::ERRCODE_INTERNAL_ERROR)) // alloc: Cold
}
