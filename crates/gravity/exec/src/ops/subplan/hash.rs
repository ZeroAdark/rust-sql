//! `nest.subplan`'s runtime objects (RT-2a, V4R-365; the faces agreed with M8-SUB): the hashed SubPlan's key set
//! (`ProtoKind::SubHash`, PG `buildSubPlanHash` / `ExecHashSubPlan`) and a SubPlan's per-call result object
//! (`ProtoKind::SubResult`: a by-reference EXPR result's copy, PG's `curTuple`). Both are laid once per execution in the
//! proto's one handle word and drop at its end.
//!
//! SubHash: the build's end calls `m8_subhash_insert(ctx, h, row)` per row, `row` = the key words then one null word per
//! key (nonzero = NULL); the first form takes one key column of 1, 2, 4 or 8 bytes (`arg` = ncols | width << 8). A NULL
//! key is counted (`HAS_NULL`), never stored (dropped under `abi::SUBHASH_NO_NULLS`, an unknownEqFalse set); `m8_subhash_seal` returns the header the IR probes (v3's
//! `gravity_lower::subhash::KeyTable`: keys, tags, mask, flags NONEMPTY | HAS_NULL; `slot_of` / `width_mask` for the
//! probe), the same address on every seal until a reset, also published at `abi::SUBHASH_TABLE_AT` for a probe at any
//! width (no host call). `m8_subhash_reset` empties the set (and nulls that word) for a rebuild (B3's rerun
//! path); the next seal's header is the one to probe. One filler: a build at width > 1 is an error by name; the build
//! pipeline's merge is a no-op (`merge_for`: its breaker region only orders it before the probe). The set's
//! arrays are trunk's `KeySet` (the v3 entry's), sized by its rows; PostgreSQL plans a hashed SubPlan only within
//! hash_mem.
//!
//! A LAZY set (`abi::SUBHASH_LAZY`, RT-2c, V4R-369) builds at its first probe, `m8_subhash_run`: m8_sub_lazy.rs.
//! A set over several key columns (RT-2d, V4R-415) is probed by `m8_subhash_probe`: m8_sub_multi.rs.
//!
//! SubResult: one slot per calling lane (`ctx.worker_ordinal`: each pool worker and each D1036 nested lane), touched only
//! by that lane's thread, so a parallel once-inner and a nested run never share a copy. `m8_sub_copy` copies a result into
//! the caller's slot, valid until that caller's next copy on the object (the buffer is reused, grown only for a larger
//! result), so its memory stays one result per lane however many outer rows call it. An ARRAY sublink's object (`arg` = the
//! element type) accumulates per lane through `m8_array_*` (RT-2b, V4R-428): m8_sub_array.rs. Created by M8-RUNTIME.
use core::cell::{Cell, RefCell, UnsafeCell};

use gravity_abi::status::ERROR;
use gravity_kernels::ops::shared::object::entries as abi;
use gravity_program::protos::{Proto, ProtoKind};
use types_error::PgResult;

use super::super::{Frame, Live, Prepare, Protocol};
use crate::glue;
use crate::ops::subplan::keyset::KeySet;


// the kernels' LAZY bit is the lowering's (program v4.rs; kernels names no crate past the ABI)
const _: () = assert!(abi::SUBHASH_LAZY == gravity_program::protos::SUBHASH_LAZY);

pub struct SubHash;
pub struct SubResult;
/// D3506 SETLOCK (lowerv4 rerun/epq/set_lock.rs): the set a lock's EvalPlanQual redo probes beside a SubHash set whose
/// plan locks rows, as PostgreSQL's EvalPlanQualStart gives the recheck fresh SubPlan states: built LAZY at the redo's
/// first probe by running the statement set's members again (their lock hosts live: our own locks TM_Ok, a row released
/// since locked now, one another session holds skipped), the statement set's build end redirected into it while they
/// run (`twin_run`); kept for the execution (the lock's EvalPlanQual state), never touching the statement's set.
pub struct SubHashEpq;

/// The lanes a SubResult object keeps a slot for (hir.rs's hosted lanes: pool workers below, D1036's nested ones on top).
pub(crate) const LANES: usize = 256;

/// What laying a SubPlan object reads of exec's `Prepare`: all the crate rig's `prepare_bare` has (m8_init.rs).
pub struct Bare<'a> {
    pub program: &'a gravity_program::Program,
    pub words: &'a mut [u64],
    pub nulls: &'a mut [u8],
    pub nparams: &'a mut usize,
}

impl<'a> Bare<'a> {
    fn of(cx: &'a mut Prepare<'_>) -> Bare<'a> {
        Bare { program: cx.program, words: &mut *cx.words, nulls: &mut *cx.nulls, nparams: &mut *cx.nparams }
    }

    /// The handle word `p` declares, checked.
    fn handle(&self, p: &Proto, what: &str) -> PgResult<usize> {
        let base = p.words.base as usize;
        if p.words.n != 1 || base >= self.words.len() || base >= self.nulls.len() {
            return Err(glue::internal(&format!("gravity: {what}'s declaration is not its object's (one handle word)"))); // alloc: Cold
        }
        Ok(base)
    }

    fn lay(&mut self, base: usize, at: u64) {
        self.words[base] = at;
        self.nulls[base] = 0;
        *self.nparams = (*self.nparams).max(base + 1);
    }
}

// ------------------------------------------------------------------------------------------------------------ SubHash

#[repr(C)]
pub(crate) struct SetObj {
    pub(crate) head: abi::Head,
    /// The sealed set's `KeyTable` header (`abi::SUBHASH_TABLE_AT`), null until the seal and after a reset: the probe's load.
    pub(crate) table: Cell<u64>,
    pub(super) width: u8,
    /// The build pipeline: its merge is `NoMerge` (the set is this object's; slot S only orders the build before the probe).
    pipeline: usize,
    pub(super) sealed: Cell<bool>,
    pub(super) set: RefCell<KeySet>,
    /// The build's road: in program order, or LAZY at the first probe (V4R-369).
    pub(crate) lazy: crate::ops::subplan::lazy::Lazy,
    /// Several key columns (RT-2d, V4R-415): the set and its seal mark (`table` stays null: the probe is `m8_subhash_probe`).
    pub(crate) multi: Option<RefCell<crate::ops::subplan::multi::Multi>>,
    pub(crate) mseal: Cell<u64>,
    /// `abi::SUBHASH_NO_NULLS`: a build row holding a NULL key is dropped (unknownEqFalse: PG keeps no `hashnulls`).
    pub(super) no_nulls: bool,
    /// D3506 SETLOCK: while a twin's build runs, the twin's head: this set's build end inserts and seals into it.
    pub(super) redirect: Cell<u64>,
    /// D3506 SETLOCK: a twin's (`SubHashEpq`) statement set, whose build members fill it (0: no twin).
    pub(super) twin_of: Cell<u64>,
}

struct SetLive {
    obj: Box<SetObj>,
    /// An AHEAD set's one reader (`abi::SUBHASH_AHEAD`, the proto's `peer`): at its start the build may run ahead (`late_words`).
    reader: Option<usize>,
}

impl Live for SetLive {
    fn answers(&self) -> u16 {
        crate::ops::ask::MERGE | crate::ops::ask::DEFERRED | crate::ops::ask::FRAME | crate::ops::ask::LATE
    }

    fn merge_for(&self, pipeline: usize) -> Option<&dyn gravity_driver::Merge> {
        (pipeline == self.obj.pipeline).then_some(&NoMerge as &dyn gravity_driver::Merge)
    }

    fn deferred(&self, pipeline: usize) -> bool {
        self.obj.lazy.deferred(pipeline)
    }

    fn frame(&self, f: Option<Frame>) {
        self.obj.lazy.frame.set(f);
    }

    /// AHEAD: the reader's start runs the build ahead when the reader runs on workers (`Lazy::ahead`).
    fn late_words(&self, pipeline: usize, _out: &mut gravity_parallel::LateWords) -> PgResult<()> {
        match self.reader {
            Some(r) if r == pipeline => self.obj.lazy.ahead(pipeline, self.obj.seal_word()),
            _ => Ok(()),
        }
    }
}

/// The build pipeline's merge: nothing to fold (its end fills the set through the handle, never its breaker region).
struct NoMerge;

impl gravity_driver::Merge for NoMerge {
    fn merge_into(&self, _leader: &mut gravity_driver::State<'_>, _partial: gravity_driver::Partial<'_>) -> gravity_driver::Result<()> {
        Ok(())
    }
}

impl Protocol for SubHash {
    fn kind(&self) -> ProtoKind {
        ProtoKind::SubHash
    }

    fn prepare(&self, p: &Proto, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        lay_set(p, &mut Bare::of(cx)).map(Some)
    }
}

/// D2914 (2a): SubHash proto `p`'s build runs only nested, at its first probe (`m8_subhash_run`): a LAZY set but an AHEAD
/// one whose reader (`lay_set`'s) can run on workers, where its start builds the set ahead (an emit or a serial reader
/// builds nothing ahead, `crate::ops::reader_runs_wide`).
pub(crate) fn builds_nested(p: &Proto, program: &gravity_program::Program) -> bool {
    p.arg & abi::SUBHASH_LAZY != 0 && !(ahead(p) && crate::ops::reader_runs_wide(program, p.peer as usize))
}

/// D2914 (2a): SubHash proto `p` is a LAZY set built ahead, its one reader named (`lay_set`'s run-ahead arm).
pub(crate) fn ahead(p: &Proto) -> bool {
    p.arg & abi::SUBHASH_LAZY != 0 && p.arg & abi::SUBHASH_AHEAD != 0 && p.peer != gravity_program::protos::NONE
}

/// SubHash proto `p`'s set, its handle laid (exec's prepare, and the crate rig's `prepare_bare`).
pub fn lay_set(p: &Proto, b: &mut Bare<'_>) -> PgResult<Box<dyn Live>> {
    let base = b.handle(p, "nest.subplan (hashed)")?;
    let lazy = match p.arg & abi::SUBHASH_LAZY {
        0 => crate::ops::subplan::lazy::Lazy::new(Box::default()),
        _ => crate::ops::subplan::lazy::Lazy::of(b.program, p.pipeline as usize)?,
    };
    // AHEAD: a LAZY set with one reader (the proto's `peer`) that may build before its first probe
    let ahead = p.arg & abi::SUBHASH_AHEAD != 0;
    if ahead && (p.arg & abi::SUBHASH_LAZY == 0 || p.peer == gravity_program::protos::NONE) {
        return Err(glue::internal("gravity: nest.subplan (hashed): an AHEAD set that is not LAZY or names no reader"));
    }
    let reader = ahead.then_some(p.peer as usize);
    let obj = new_set(p.arg & !(abi::SUBHASH_LAZY | abi::SUBHASH_AHEAD), p.pipeline as usize, lazy)?;
    b.lay(base, &obj.head as *const abi::Head as u64);
    Ok(Box::new(SetLive { obj, reader })) // alloc: Exec
}

impl Protocol for SubHashEpq {
    fn kind(&self) -> ProtoKind {
        ProtoKind::SubHashEpq
    }

    fn prepare(&self, p: &Proto, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        lay_twin(p, &mut Bare::of(cx)).map(Some)
    }
}

/// D3506 SETLOCK: SubHashEpq proto `p`'s set, beside the SubHash set of its build pipeline (laid before it: the probe
/// that declares the twin is made after the set's end), its handle laid. Its LAZY road is the statement set's members,
/// run again (`rerun_members`: each kept slot reset, the scans reopened) at its first probe; it merges nothing.
pub fn lay_twin(p: &Proto, b: &mut Bare<'_>) -> PgResult<Box<dyn Live>> {
    let base = b.handle(p, "nest.subplan.epq")?;
    let protos = b.program.v4.as_ref().map_or(&[][..], |d| &d.protos[..]);
    let main = protos.iter().find(|m| m.kind == ProtoKind::SubHash && m.pipeline == p.pipeline).map(|m| m.words.base as usize);
    let at = main.and_then(|m| b.words.get(m).copied()).filter(|&w| w != 0);
    let Some(at) = at.filter(|_| p.arg & abi::SUBHASH_LAZY != 0) else {
        return Err(glue::internal("gravity: nest.subplan.epq: no LAZY SubHash set of its build pipeline laid before it"));
    };
    let lazy = crate::ops::subplan::lazy::Lazy::of(b.program, p.pipeline as usize)?;
    lazy.run.set(crate::ops::subplan::lazy::STALE);
    let obj = new_set(p.arg & !(abi::SUBHASH_LAZY | abi::SUBHASH_AHEAD), usize::MAX, lazy)?;
    obj.twin_of.set(at);
    b.lay(base, &obj.head as *const abi::Head as u64);
    Ok(Box::new(SetLive { obj, reader: None })) // alloc: Exec
}

/// An empty set of the key layout `arg` (ncols | width << 8; the first form: one column of 1, 2, 4 or 8 bytes).
pub(crate) fn new_set(arg: u64, pipeline: usize, lazy: crate::ops::subplan::lazy::Lazy) -> PgResult<Box<SetObj>> {
    let (no_nulls, arg) = (arg & abi::SUBHASH_NO_NULLS != 0, arg & !abi::SUBHASH_NO_NULLS);
    let (ncols, width) = (arg & 0xff, (arg >> 8) & 0xff);
    let multi = crate::ops::subplan::multi::Multi::of(arg).map(RefCell::new);
    if multi.is_none() && (ncols != 1 || ![1, 2, 4, 8].contains(&width) || arg >> 16 != 0) {
        return Err(glue::internal(&format!("gravity: nest.subplan's hashed key layout {arg:#x} (one column of 1, 2, 4 or 8 bytes, 2 to 8 columns, or 1 to 8 with byte keys, or 9 to 20 in the wide form)"))); // alloc: Cold
    }
    let width = if multi.is_some() { 8 } else { width as u8 };
    let head = abi::Head { magic: abi::SUBHASH_MAGIC, op: super::raw::set_op };
    let set = RefCell::new(KeySet::new(width));
    Ok(Box::new(SetObj { head, table: Cell::new(0), width, pipeline, sealed: Cell::new(false), set, lazy, multi, mseal: Cell::new(0), no_nulls, redirect: Cell::new(0), twin_of: Cell::new(0) })) // alloc: Exec
}

impl SetObj {
    /// A rerun set's re-open (exec subplan/raw.rs `rearm_sets`) of a LAZY set: the build re-armed (`Lazy::rearm`) and the set
    /// emptied, so the next probe finds no header and builds it again over the new parameters.
    pub(crate) fn rearm(&self) -> PgResult<()> {
        self.lazy.rearm()?;
        let Ok(mut set) = self.set.try_borrow_mut() else { return Err(glue::internal("gravity: m8_rerun: a LAZY hashed set re-armed inside its own call")) };
        self.table.set(0);
        self.mseal.set(0);
        if let Some(m) = &self.multi {
            m.borrow_mut().clear();
        }
        *set = KeySet::new(self.width);
        self.sealed.set(false);
        Ok(())
    }
}


// ---------------------------------------------------------------------------------------------------------- SubResult

#[repr(C)]
pub(crate) struct ResultObj {
    pub(crate) head: abi::Head,
    /// One copy buffer per calling lane (`ctx.worker_ordinal`), each touched only by that lane's thread.
    pub(super) slots: Box<[UnsafeCell<Vec<u64>>]>,
    pub(crate) arrays: crate::ops::subplan::array::Arrays,
}

struct ResultLive {
    _obj: Box<ResultObj>,
}

impl Live for ResultLive {
    fn answers(&self) -> u16 {
        crate::ops::ask::AFTER
    }

    fn after_pipeline(&self, _pipeline: usize, exec: &mut gravity_driver::Exec) -> Option<gravity_driver::Result<()>> {
        self._obj.arrays.charge(exec)
    }
}

impl Protocol for SubResult {
    fn kind(&self) -> ProtoKind {
        ProtoKind::SubResult
    }

    fn prepare(&self, p: &Proto, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        lay_result(p, &mut Bare::of(cx)).map(Some)
    }
}

/// SubResult proto `p`'s object, its handle laid (exec's prepare, and the crate rig's `prepare_bare`).
pub fn lay_result(p: &Proto, b: &mut Bare<'_>) -> PgResult<Box<dyn Live>> {
    let base = b.handle(p, "nest.subplan.result")?;
    let obj = new_result(crate::ops::subplan::array::Arrays::of(p.arg)?);
    b.lay(base, &obj.head as *const abi::Head as u64);
    Ok(Box::new(ResultLive { _obj: obj })) // alloc: Exec
}

pub(crate) fn new_result(arrays: crate::ops::subplan::array::Arrays) -> Box<ResultObj> {
    let slots = (0..LANES).map(|_| UnsafeCell::new(Vec::new())).collect(); // alloc: Exec
    Box::new(ResultObj { head: abi::Head { magic: abi::ARRAY_MAGIC, op: super::raw::result_op }, slots, arrays }) // alloc: Exec
}



#[cold]
#[inline(never)]
pub(crate) fn fail(what: &str) -> i64 {
    gravity_fn::stash_error(glue::internal(&format!("gravity: {what}"))); // alloc: Cold
    ERROR
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use super::*;
    use gravity_abi::Ctx;
    use gravity_lower::ops::subplan::keyset::{lookup, verdict, KeyTable, HAS_NULL, NONEMPTY};

    fn message() -> String {
        gravity_fn::take_error().map(|e| e.message().to_owned()).unwrap_or_default()
    }

    fn h(head: &abi::Head) -> u64 {
        head as *const abi::Head as u64
    }

    unsafe fn insert(o: &SetObj, key: u64, null: bool) -> i64 {
        let row = [key, null as u64];
        abi::m8_subhash_insert(core::ptr::null_mut(), h(&o.head), row.as_ptr())
    }

    unsafe fn seal(o: &SetObj) -> *const KeyTable {
        let t = abi::m8_subhash_seal(core::ptr::null_mut(), h(&o.head));
        assert!(t > 0, "{}", message());
        t as *const KeyTable
    }

    fn ctx(ordinal: u16, width: u16) -> Ctx {
        // SAFETY: a Ctx of null pointers and zero words is a valid value; the ops read only the ordinal and the width.
        let mut c: Ctx = unsafe { core::mem::zeroed() };
        (c.worker_ordinal, c.width) = (ordinal, width);
        c
    }

    #[test]
    fn the_set_answers_pgs_hashed_verdicts_null_keys_counted_never_stored() {
        let o = new_set(1 | 4 << 8, 0, crate::ops::subplan::lazy::Lazy::new(Box::default())).expect("a set");
        unsafe {
            let empty = seal(&o);
            assert_eq!((*empty).flags & NONEMPTY, 0, "an empty set: every probe answers false");
            assert_eq!(lookup(empty, 5, 4), (false, false, false));
            assert_eq!(abi::m8_subhash_reset(core::ptr::null_mut(), h(&o.head)), 0);
            for k in 0..1000u64 {
                assert_eq!(insert(&o, k * 3, false), 0);
                assert_eq!(insert(&o, k * 3 | 0xdead_0000_0000_0000, false), 0, "past the width: the same key");
            }
            let t = seal(&o);
            assert_eq!(seal(&o), t, "a second seal is the same header");
            let loaded = *(h(&o.head) as *const u8).add(abi::SUBHASH_TABLE_AT as usize).cast::<u64>();
            assert_eq!(loaded, t as u64, "the probe's load at SUBHASH_TABLE_AT is the sealed header");
            assert_eq!(((*t).flags & NONEMPTY, (*t).flags & HAS_NULL), (NONEMPTY, 0));
            assert_eq!(lookup(t, 9, 4), (true, true, false));
            assert_eq!(lookup(t, 10, 4), (true, false, false));
            // a NULL key: counted; a miss is then unknown (x = ANY (.., NULL) is NULL), a hit stays true
            assert_eq!(abi::m8_subhash_reset(core::ptr::null_mut(), h(&o.head)), 0);
            assert_eq!(o.table.get(), 0, "a reset set has no header to probe until its next seal");
            assert_eq!(insert(&o, 9, false), 0);
            assert_eq!(insert(&o, 0, true), 0);
            let t = seal(&o);
            let (ne, found, null) = lookup(t, 10, 4);
            assert_eq!(verdict(ne, found, null, true), (0, false));
            let (ne, found, null) = lookup(t, 9, 4);
            assert_eq!(verdict(ne, found, null, true), (1, true));
            assert_eq!(lookup(t, 0, 4), (true, false, true), "the NULL key is not stored as 0");
            // an all-NULL build: nonempty, nothing found, HAS_NULL
            assert_eq!(abi::m8_subhash_reset(core::ptr::null_mut(), h(&o.head)), 0);
            assert_eq!(insert(&o, 0, true), 0);
            let t = seal(&o);
            assert_eq!(lookup(t, 0, 4), (true, false, true));
        }
        // unknownEqFalse (no hashnulls): a NULL key is dropped, so an all-NULL build is the empty set
        let o = new_set(1 | 4 << 8 | abi::SUBHASH_NO_NULLS, 0, crate::ops::subplan::lazy::Lazy::new(Box::default())).expect("a set");
        unsafe {
            assert_eq!(insert(&o, 0, true), 0);
            assert_eq!((*seal(&o)).flags & (NONEMPTY | HAS_NULL), 0, "only NULL keys: empty");
            assert_eq!(abi::m8_subhash_reset(core::ptr::null_mut(), h(&o.head)), 0);
            assert_eq!(insert(&o, 0, true), 0);
            assert_eq!(insert(&o, 9, false), 0);
            let t = seal(&o);
            assert_eq!(((*t).flags & NONEMPTY, (*t).flags & HAS_NULL), (NONEMPTY, 0));
            assert_eq!(lookup(t, 9, 4), (true, true, false));
        }
    }

    #[test]
    fn a_sealed_set_takes_no_row_until_a_reset_and_a_second_filler_errors_by_name() {
        let o = new_set(1 | 8 << 8, 0, crate::ops::subplan::lazy::Lazy::new(Box::default())).expect("a set");
        unsafe {
            seal(&o);
            assert_eq!(insert(&o, 1, false), ERROR);
            assert_eq!(message(), "gravity: m8_subhash_insert: a row into a sealed set (a rebuild resets it first)");
            let mut c = ctx(3, 8);
            let row = [1u64, 0];
            assert_eq!(abi::m8_subhash_insert(&mut c, h(&o.head), row.as_ptr()), ERROR);
            assert!(message().contains("filled by more than one worker"));
        }
        for arg in [0, 2 | 4 << 8, 1 | 3 << 8, 1 | 4 << 8 | 1 << 16] {
            let e = new_set(arg, 0, crate::ops::subplan::lazy::Lazy::new(Box::default())).err().expect("refused");
            assert!(e.message().starts_with("gravity: nest.subplan's hashed key layout"), "{}", e.message());
        }
    }

    /// V4R-369's frame: records each member run and plays the build's end (`keys` inserted, then the seal unless `seal` is
    /// off), or answers `then`; `probe` probes the set inside its own build.
    struct Build {
        set: u64,
        keys: Vec<u64>,
        seal: bool,
        probe: bool,
        then: Option<gravity_driver::Error>,
        runs: Vec<Vec<u16>>,
        inner: i64,
        /// AHEAD: the reader runs on workers, so its start runs the members (`prerun_members`); else it runs nothing.
        wide: bool,
    }

    impl gravity_driver::OnDemand for Build {
        fn run_members(&mut self, members: &[u16]) -> gravity_driver::Result<()> {
            self.runs.push(members.to_vec()); // alloc: Cold
            if let Some(e) = self.then.take() {
                return Err(e);
            }
            // SAFETY: `set` is the live set's head; the calls are the build end's (the leader).
            unsafe {
                if self.probe {
                    self.inner = abi::m8_subhash_run(core::ptr::null_mut(), self.set);
                }
                for &k in &self.keys {
                    let row = [k, 0];
                    assert_eq!(abi::m8_subhash_insert(core::ptr::null_mut(), self.set, row.as_ptr()), 0);
                }
                if self.seal {
                    assert!(abi::m8_subhash_seal(core::ptr::null_mut(), self.set) > 0);
                }
            }
            Ok(())
        }

        fn prerun_members(&mut self, _reader: usize, members: &[u16]) -> gravity_driver::Result<bool> {
            if !self.wide {
                return Ok(false);
            }
            self.run_members(members).map(|()| true)
        }
    }

    fn lazy_set(members: &[u16]) -> SetLive {
        SetLive { obj: new_set(1 | 8 << 8, members[members.len() - 1] as usize, crate::ops::subplan::lazy::Lazy::new(members.into())).expect("a set"), reader: None }
    }

    fn build(l: &SetLive, keys: &[u64]) -> Build {
        Build { set: h(&l.obj.head), keys: keys.to_vec(), seal: true, probe: false, then: None, runs: Vec::new(), inner: 0, wide: false } // alloc: Cold
    }

    fn frame(b: &mut Build) -> Frame {
        let p: *mut (dyn gravity_driver::OnDemand + '_) = b;
        // SAFETY: each test keeps `b` alive past every use of the frame.
        core::ptr::NonNull::new(unsafe { core::mem::transmute::<*mut (dyn gravity_driver::OnDemand + '_), *mut (dyn gravity_driver::OnDemand + 'static)>(p) }).unwrap()
    }

    #[test]
    fn a_lazy_set_builds_at_its_first_probe_once_and_never_unprobed() {
        let l = lazy_set(&[2, 3, 5]);
        assert!(l.deferred(2) && l.deferred(5) && !l.deferred(4) && !l.deferred(6), "the members are stepped past");
        let mut b = build(&l, &[7, 9, 11]);
        l.frame(Some(frame(&mut b)));
        let ctx: *mut Ctx = core::ptr::null_mut();
        let set = h(&l.obj.head);
        unsafe {
            assert_eq!(*(set as *const u8).add(abi::SUBHASH_TABLE_AT as usize).cast::<u64>(), 0, "unbuilt: the probe calls the run");
            let t = abi::m8_subhash_run(ctx, set);
            assert!(t > 0, "{}", message());
            assert_eq!(*(set as *const u8).add(abi::SUBHASH_TABLE_AT as usize).cast::<u64>(), t as u64, "then the probe loads it");
            assert_eq!(lookup(t as *const KeyTable, 9, 8), (true, true, false));
            assert_eq!(lookup(t as *const KeyTable, 8, 8), (true, false, false));
            assert_eq!(abi::m8_subhash_run(ctx, set), t, "built: the same header, no second run");
            l.frame(None);
            assert_eq!(abi::m8_subhash_run(ctx, set), t, "a built set needs no frame");
        }
        assert_eq!(b.runs, [[2u16, 3, 5]]);
        // a LAZY set never probed never runs its build (PG: an empty outer never reaches buildSubPlanHash)
        let never = lazy_set(&[1]);
        let mut nb = build(&never, &[1]);
        never.frame(Some(frame(&mut nb)));
        never.frame(None);
        assert!(nb.runs.is_empty() && never.obj.table.get() == 0);
        // a set built in program order is not LAZY: nothing is deferred, and its run before the seal is an error by name
        let eager = SetLive { obj: new_set(1 | 8 << 8, 0, crate::ops::subplan::lazy::Lazy::new(Box::default())).expect("a set"), reader: None };
        assert!(!eager.deferred(0));
        assert_eq!(unsafe { abi::m8_subhash_run(ctx, h(&eager.obj.head)) }, ERROR);
        assert_eq!(message(), "gravity: m8_subhash_run: a set built in program order (not LAZY) probed before its seal");
    }

    #[test]
    fn an_ahead_set_builds_at_its_readers_start_on_workers_and_at_its_first_probe_on_the_leader() {
        let ctx: *mut Ctx = core::ptr::null_mut();
        // the reader runs on workers: its start builds the set, and the probe loads the header and runs nothing
        let mut w = lazy_set(&[2, 3]);
        w.reader = Some(4);
        let mut b = build(&w, &[7, 9]);
        b.wide = true;
        w.frame(Some(frame(&mut b)));
        w.late_words(3, &mut gravity_parallel::LateWords::none()).expect("another pipeline's start");
        assert!(b.runs.is_empty() && w.obj.table.get() == 0, "only the reader's start runs ahead");
        w.late_words(4, &mut gravity_parallel::LateWords::none()).expect("the reader's start");
        assert_eq!(b.runs, [[2u16, 3]]);
        let t = unsafe { abi::m8_subhash_run(ctx, h(&w.obj.head)) };
        assert!(t > 0 && t as u64 == w.obj.table.get(), "{}", message());
        // SAFETY: the table the run just sealed, alive while `w` holds it
        assert_eq!(unsafe { lookup(t as *const KeyTable, 9, 8) }, (true, true, false));
        w.late_words(4, &mut gravity_parallel::LateWords::none()).expect("a second start of the reader");
        assert_eq!(b.runs.len(), 1, "built ahead once: neither the probe nor a second start runs it again");
        w.frame(None);
        // the reader runs on the leader alone: its start runs nothing, and its first probe builds the set
        let mut l = lazy_set(&[1]);
        l.reader = Some(2);
        let mut lb = build(&l, &[5]);
        l.frame(Some(frame(&mut lb)));
        l.late_words(2, &mut gravity_parallel::LateWords::none()).expect("the reader's start");
        assert!(lb.runs.is_empty() && l.obj.table.get() == 0, "a leader reader leaves the build to its first probe");
        assert!(unsafe { abi::m8_subhash_run(ctx, h(&l.obj.head)) } > 0, "{}", message());
        assert_eq!(lb.runs.len(), 1);
        l.frame(None);
        // a run ahead that fails is the reader start's error, and the build never runs again
        let mut f = lazy_set(&[1]);
        f.reader = Some(2);
        let mut fb = build(&f, &[5]);
        (fb.wide, fb.then) = (true, Some(gravity_driver::Error::Host("the build's error".into())));
        f.frame(Some(frame(&mut fb)));
        assert_eq!(f.late_words(2, &mut gravity_parallel::LateWords::none()).expect_err("the build's error").message(), "gravity: the build's error");
        assert_eq!(unsafe { abi::m8_subhash_run(ctx, h(&f.obj.head)) }, ERROR);
        assert_eq!(message(), "gravity: m8_subhash_run: the set's build failed earlier in this execution");
        assert_eq!(fb.runs.len(), 1);
        f.frame(None);
    }

    #[test]
    fn a_lazy_builds_error_raises_at_the_probe_and_the_build_never_runs_again() {
        let l = lazy_set(&[0, 1]);
        let mut b = build(&l, &[1]);
        b.then = Some(gravity_driver::Error::Host("the build's error".into()));
        l.frame(Some(frame(&mut b)));
        let ctx: *mut Ctx = core::ptr::null_mut();
        assert_eq!(unsafe { abi::m8_subhash_run(ctx, h(&l.obj.head)) }, ERROR);
        assert_eq!(message(), "gravity: the build's error", "the build's error raises at the probe");
        assert_eq!(unsafe { abi::m8_subhash_run(ctx, h(&l.obj.head)) }, ERROR);
        assert_eq!(message(), "gravity: m8_subhash_run: the set's build failed earlier in this execution");
        assert_eq!(b.runs.len(), 1);
        l.frame(None);
    }

    #[test]
    fn a_lazy_set_without_a_frame_unsealed_reprobed_inside_its_build_or_reset_after_it_errors_by_name() {
        let ctx: *mut Ctx = core::ptr::null_mut();
        let l = lazy_set(&[4]);
        assert_eq!(unsafe { abi::m8_subhash_run(ctx, h(&l.obj.head)) }, ERROR);
        assert!(message().starts_with("gravity: m8_subhash_run: no runner frame"));
        // a build whose end never sealed
        let u = lazy_set(&[4]);
        let mut ub = build(&u, &[1]);
        ub.seal = false;
        u.frame(Some(frame(&mut ub)));
        assert_eq!(unsafe { abi::m8_subhash_run(ctx, h(&u.obj.head)) }, ERROR);
        assert_eq!(message(), "gravity: m8_subhash_run: the LAZY set's build ran without sealing it");
        u.frame(None);
        // a probe of the set inside its own build
        let r = lazy_set(&[4]);
        let mut rb = build(&r, &[1]);
        rb.probe = true;
        r.frame(Some(frame(&mut rb)));
        assert!(unsafe { abi::m8_subhash_run(ctx, h(&r.obj.head)) } > 0, "the outer probe's build completes");
        assert_eq!(rb.inner, ERROR);
        // (the inner probe's stashed error was taken by nothing: the outer call returned a header)
        let _ = message();
        // a reset after the build that no rerun set re-armed (the build's own open never resets a LAZY set)
        assert_eq!(unsafe { abi::m8_subhash_reset(ctx, h(&r.obj.head)) }, ERROR);
        assert_eq!(message(), "gravity: m8_subhash_reset: a LAZY set's rebuild its rerun set did not re-arm");
        r.frame(None);
    }

    #[test]
    fn a_copy_is_valid_until_the_same_lanes_next_copy_and_lanes_never_share() {
        let o = new_result(crate::ops::subplan::array::Arrays::with(None));
        let (mut c0, mut c7) = (ctx(0, 8), ctx(7, 8));
        let (a, b) = (b"first by-reference result".as_slice(), b"another".as_slice());
        unsafe {
            let p0 = abi::m8_sub_copy(&mut c0 as *mut _ as u64, h(&o.head), a.as_ptr() as u64, a.len() as u64);
            let p7 = abi::m8_sub_copy(&mut c7 as *mut _ as u64, h(&o.head), b.as_ptr() as u64, b.len() as u64);
            assert!(p0 > 0 && p7 > 0 && p0 != p7, "{}", message());
            assert_eq!(std::slice::from_raw_parts(p0 as *const u8, a.len()), a, "lane 7's copy never touches lane 0's");
            // the same lane again: its buffer reused for a result no larger (PG frees curTuple at the next call)
            let q0 = abi::m8_sub_copy(&mut c0 as *mut _ as u64, h(&o.head), b.as_ptr() as u64, b.len() as u64);
            assert_eq!((q0, std::slice::from_raw_parts(q0 as *const u8, b.len())), (p0, b));
            // a copy of the lane's own previous copy is moved, not read after a free
            let r0 = abi::m8_sub_copy(&mut c0 as *mut _ as u64, h(&o.head), (q0 as *const u8) as u64, b.len() as u64);
            assert_eq!(std::slice::from_raw_parts(r0 as *const u8, b.len()), b);
            let big = vec![5u8; 4096]; // alloc: Cold
            let s0 = abi::m8_sub_copy(&mut c0 as *mut _ as u64, h(&o.head), big.as_ptr() as u64, 4096);
            assert_eq!(std::slice::from_raw_parts(s0 as *const u8, 4096), &big[..]);
        }
    }

    #[test]
    fn a_million_calls_keep_one_result_per_lane() {
        let o = new_result(crate::ops::subplan::array::Arrays::with(None));
        let mut c = ctx(2, 1);
        let v: Vec<u8> = (0..200u8).collect(); // alloc: Cold
        let first = unsafe { abi::m8_sub_copy(&mut c as *mut _ as u64, h(&o.head), v.as_ptr() as u64, 200) };
        for i in 0..1_000_000u64 {
            let n = 1 + i % 200;
            let p = unsafe { abi::m8_sub_copy(&mut c as *mut _ as u64, h(&o.head), v.as_ptr() as u64, n) };
            assert_eq!(p, first, "call {i}: the lane's buffer is reused, never grown past the largest result");
        }
        // SAFETY: the slot is this thread's.
        assert_eq!(unsafe { (*o.slots[2].get()).capacity() }, 25);
    }

}
