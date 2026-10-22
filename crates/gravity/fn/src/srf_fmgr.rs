//! The function-manager set face (rung 13's largest census bucket, docs/gravity/ladder/rung13-setops.md):
//! a set-returning function without a library body, or a scalar function in FROM, run through PostgreSQL's
//! own set protocol — execSRF.c's ValuePerCall loop and the Materialize hand-back — once per `open`, the
//! rows kept as datum items in the slot's bump context, one row per `step` into the cell's out words.
//! A SQL or PL body runs its statements nested, at the next re-entrancy level (rung 14); a volatile one is served in a
//! target list only (V4R-295). A RECORD result bare in a target list is PG's TYPEFUNC_RECORD (V4R-319): no expected
//! row, each datum its own blessed type, a Materialize store read through a copy of the first `setDesc`. The lowering only checks (`FmgrSet::check`); an execution builds its
//! `FmgrSet`s from the plan's own expressions (`Sets`, M5 H-b), so no program holds one.
#![allow(clippy::missing_safety_doc, clippy::not_unsafe_ptr_arg_deref)]

use std::rc::Rc;
use core::ptr::NonNull;

use datum::Datum;
use fmgr::{ExprDoneCond, FmgrInfo, LocalFcinfo, ReturnSetInfo, SetFunctionReturnMode, SFRM_Materialize, SFRM_ValuePerCall};
use gravity_abi::Ctx;
use mcx::{Mcx, MemoryContext};
use types_core::Oid;
use types_error::{PgError, PgResult};
use types_nodes::plannodes::ProjectSet;
use types_nodes::Node;
use types_slot::{SlotData, TupleSlotKind};
use types_tuple::{varatt, TupleDescData};

use crate::srf::{fresh_mcx, place, slot_cx, state_of};
use crate::{fail, ErrSlot, OK};

use types_core::FUNC_MAX_ARGS;
const RECORDOID: Oid = 2249;
const LANG_INTERNAL: Oid = 12;
const LANG_C: Oid = 13;

/// Why a function is not served here; the lowerer refuses by this name.
pub enum Refuse {
    Volatile,
    Kind,
    Nested,
    Result,
    Cols,
    NoCatalog,
    Catalog(Box<PgError>),
}

impl Refuse {
    pub fn what(&self) -> &'static str {
        match self {
            Refuse::Volatile => "a volatile SQL / PL-language function in FROM (its body would re-run on a stopped row; stable and immutable bodies run nested, entry.md §8; a target-list set is served, V4R-295)",
            Refuse::Kind => "a procedure, aggregate or window function as a set",
            Refuse::Nested => "a C function that runs its own statement (ts_stat, the *_to_xml family)",
            Refuse::Result => "a target-list set whose result or argument type the call cannot resolve (a pseudo-type result, a RECORD or composite result not taken whole, an unresolved polymorphic argument, or no pg_proc row)",
            Refuse::Cols => "a target-list set with more arguments than FUNC_MAX_ARGS or a zero-column row",
            Refuse::NoCatalog => "a function-manager set without a live catalog (the rig)",
            Refuse::Catalog(_) => "a function the catalog cannot describe",
        }
    }
}

/// Where an execution's call is (`FmgrSet::phase`).
#[derive(Clone, Copy, PartialEq)]
enum Phase {
    /// No call in progress.
    Idle,
    /// Opened, not yet called.
    First,
    /// Called; the function or its store has more.
    More,
    /// A strict non-set call with a NULL argument: one NULL row.
    Null,
}

/// One function of an execution (`Sets`): its call frame, the result row's descriptor and the call in progress, stepped a
/// row per `step` as `ExecMakeFunctionResultSet` steps it (value-per-call, or the materialized store read a row at a
/// time). The row is one column: a scalar, or a composite handed through as one datum.
pub struct FmgrSet {
    slot: SlotData<'static>,
    /// The row the query expects (`funcResultDesc`); `None`: a RECORD result in a target list (`TYPEFUNC_RECORD`,
    /// init_sexpr's "this will work if function doesn't need an expectedDesc"), whose read slot takes the store's
    /// `setDesc` at its first materialize (`ExecPrepareTuplestoreResult`).
    desc: Option<Rc<TupleDescData<'static>>>,
    /// The RECORD read slot has its descriptor (created once per set, as `fcache->funcResultSlot`).
    record_slot: bool,
    flinfo: FmgrInfo,
    fc: Box<LocalFcinfo<FUNC_MAX_ARGS>>,
    rsinfo: Box<ReturnSetInfo>,
    store: Option<tuplestore::Tuplestore>,
    rowcx: Box<MemoryContext>,
    cx: Box<MemoryContext>,
    _fn_expr: Box<Node<'static>>,
    /// Each argument's (typlen, typbyval): a by-reference one is copied at open, as the call spans the steps.
    argl: Vec<(i16, bool)>,
    retset: bool,
    returns_tuple: bool,
    phase: Phase,
}

impl FmgrSet {
    /// The lowering's check (M5 H-b): `new`'s refusals with nothing built, `node` of any lifetime.
    pub fn check(funcid: Oid, node: Node<'_>, nargs: usize, whole_row: bool, target_list: bool) -> Result<(), Refuse> {
        let cx = MemoryContext::new_bump("gravity_fn fmgr check");
        Self::shape(funcid, node, nargs, whole_row, target_list, cx.mcx())?;
        Ok(())
    }

    /// The language and kind checks and the result row resolved as `ExecInitFunctionScan` does (`get_expr_result_type`):
    /// (the row's descriptor, whether the result is a tuple). A composite result is served only `whole_row`, as one datum
    /// (a record-yielding set bare in a target list); a scalar is its own row.
    fn shape<'c>(funcid: Oid, node: Node<'_>, nargs: usize, whole_row: bool, target_list: bool, cx: Mcx<'c>) -> Result<(Option<TupleDescData<'c>>, bool), Refuse> {
        if !syscache_seams::lookup_pg_proc_shape::is_installed() || !syscache_seams::pg_proc_proname::is_installed() {
            return Err(Refuse::NoCatalog);
        }
        let shape = syscache_seams::lookup_pg_proc_shape::call(funcid).map_err(Refuse::Catalog)?.ok_or(Refuse::Result)?;
        // a SQL / PL-language body is a nested statement at the next re-entrancy level (entry.md §8, rung 14 C2); a
        // volatile one is served in a target list only (V4R-295, D787): its cells are per execution, so a stop resumes
        // the read of the set already opened and a call happens only where ExecProjectSRF calls (the next input row)
        if !target_list && shape.prolang != LANG_INTERNAL && shape.prolang != LANG_C && shape.provolatile == b'v' as i8 {
            return Err(Refuse::Volatile);
        }
        if shape.prokind != b'f' as i8 {
            return Err(Refuse::Kind);
        }
        if crate::typesem::srf::nested_by_oid(funcid) {
            return Err(Refuse::Nested);
        }
        if let Some(name) = syscache_seams::pg_proc_proname::call(funcid).map_err(Refuse::Catalog)? {
            if crate::typesem::srf::nested_by_name(name.name_str()) {
                return Err(Refuse::Nested);
            }
        }
        if nargs > FUNC_MAX_ARGS {
            return Err(Refuse::Cols);
        }
        let resolved = funcapi::get_expr_result_type(cx, Some(node)).map_err(Refuse::Catalog)?;
        let (desc, returns_tuple) = match resolved.class {
            funcapi::TypeFuncClass::Scalar => {
                let mut d = tupdesc::CreateTemplateTupleDesc(cx, 1).map_err(Refuse::Catalog)?;
                tupdesc::TupleDescInitEntry(&mut d, 1, None, resolved.result_type_id, -1, 0).map_err(Refuse::Catalog)?;
                (Some(d), false)
            }
            funcapi::TypeFuncClass::Composite | funcapi::TypeFuncClass::CompositeDomain => (Some(resolved.result_tuple_desc.ok_or(Refuse::Result)?), true),
            // init_sexpr's TYPEFUNC_RECORD: no descriptor; each datum carries its own type (a value-per-call result
            // whole, a materialized store read under its setDesc). Bare in a target list only: FROM needs a column list.
            funcapi::TypeFuncClass::Record if whole_row && target_list => (None, true),
            _ => return Err(Refuse::Result),
        };
        if returns_tuple && !whole_row {
            return Err(Refuse::Result);
        }
        if let Some(desc) = &desc {
            if desc.natts == 0 {
                return Err(Refuse::Cols);
            }
            if returns_tuple && desc.tdtypeid == RECORDOID && desc.tdtypmod < 0 && !typcache_seams::assign_record_type_typmod::is_installed() {
                return Err(Refuse::NoCatalog);
            }
        }
        Ok((desc, returns_tuple))
    }

    /// The face for `node` (a `FuncExpr` of the statement's plan, alive as long as this execution): `shape`'s row, the
    /// frame bound.
    pub fn new(funcid: Oid, node: Node<'static>, coll: Oid, nargs: usize, retset: bool, whole_row: bool, target_list: bool) -> Result<Box<FmgrSet>, Refuse> {
        let cx = Box::new(MemoryContext::new_bump("gravity_fn fmgr set")); // alloc: Exec
        let (mut desc, returns_tuple) = Self::shape(funcid, node, nargs, whole_row, target_list, cx.mcx())?;
        // `BlessTupleDesc`: an anonymous record's descriptor registered in the typcache so the datum's typmod resolves
        if let Some(desc) = desc.as_mut().filter(|d| returns_tuple && d.tdtypeid == RECORDOID && d.tdtypmod < 0) {
            typcache_seams::assign_record_type_typmod::call(desc).map_err(Refuse::Catalog)?;
        }
        let mut flinfo = fmgr_core::fmgr_info(funcid).map_err(Refuse::Catalog)?;
        let fn_expr = Box::new(node); // alloc: Exec
        // SAFETY: the box is a field of the set, freed after the frame (drop order); the plan outlives the execution.
        flinfo.fn_expr = Some(unsafe { types_core::fmgr::FnExprErased::from_node_ref(&*fn_expr) });
        let mut argl = Vec::with_capacity(nargs);
        for i in 0..nargs {
            let t = funcapi::get_fn_expr_argtype(Some(&flinfo), i);
            argl.push(if t == types_core::InvalidOid { return Err(Refuse::Result) } else { lsyscache::get_typlenbyval(t).map_err(Refuse::Catalog)? });
        }
        let rowcx = Box::new(MemoryContext::new_bump("gravity_fn fmgr row")); // alloc: Exec
        let mut fc = Box::new(LocalFcinfo::<FUNC_MAX_ARGS>::new(coll)); // alloc: Exec
        fc.rearm(coll);
        fc.nargs = nargs as i16;
        // a target-list set's modes (`ExecMakeFunctionResultSet`: no SFRM_Materialize_Preferred)
        let rsinfo = Box::new(ReturnSetInfo::new(SFRM_ValuePerCall | SFRM_Materialize)); // alloc: Exec
        // SAFETY: the descriptor and the slot live in `cx`, a box that never moves and drops after them.
        let desc: Option<Rc<TupleDescData<'static>>> = desc.map(|d| Rc::new(unsafe { core::mem::transmute::<TupleDescData<'_>, TupleDescData<'static>>(d) }));
        let mcx: Mcx<'static> = unsafe { core::mem::transmute::<Mcx<'_>, Mcx<'static>>(cx.mcx()) };
        let slot = exectuples::make_tuple_table_slot(mcx, TupleSlotKind::MinimalTuple, desc.clone());
        let phase = Phase::Idle;
        Ok(Box::new(FmgrSet { slot, desc, record_slot: false, flinfo, fc, rsinfo, store: None, rowcx, cx, _fn_expr: fn_expr, argl, retset, returns_tuple, phase })) // alloc: Exec
    }

    /// This open's arguments (a by-reference one copied into `mcx`, the slot's context); a set the reader stopped inside
    /// ends first. EXECUTE is admission's, checked at every bind as `init_sexpr` checks it at executor start (exec
    /// tests/m5_srf_acl.rs), not per open.
    unsafe fn start(&mut self, mcx: Mcx<'static>, args: *const i64, nulls: *const u8) -> PgResult<()> {
        self.shutdown()?;
        let mut any_null = false;
        for (i, &(len, byval)) in self.argl.iter().enumerate() {
            if nulls.add(i).read() != 0 {
                any_null = true;
                self.fc.set_arg_null(i);
            } else {
                let d = args.add(i).read();
                self.fc.set_arg(i, Datum::from_i64(if byval { d } else { copy_datum(mcx, d as usize as *const u8, len, false)? as i64 }));
            }
        }
        self.phase = match (any_null && self.flinfo.fn_strict, self.retset) {
            (false, _) => Phase::First,
            (true, true) => Phase::Idle,
            (true, false) => Phase::Null,
        };
        Ok(())
    }

    /// The call in progress ended as `ShutdownSetExpr` ends a set the reader stopped inside: the function's shutdown
    /// hook (a suspended SQL body), its cross-call state and its store: at a re-open and at the normal end (`Sets::end`); an
    /// error's end drops the set (the abort path's teardown).
    fn shutdown(&mut self) -> PgResult<()> {
        let live = self.phase == Phase::More;
        self.phase = Phase::Idle;
        if let Some(store) = self.store.take() {
            store.end();
        }
        if !live {
            return Ok(());
        }
        let hook = self.rsinfo.srf_shutdown.take();
        let r = hook.map_or(Ok(()), |f| f(&mut self.flinfo));
        self.flinfo.fn_extra = None;
        r
    }

    /// The set's next row into `out` (`ExecMakeFunctionResultSet`, one call per row): false at its end.
    unsafe fn next(&mut self, mcx: Mcx<'static>, out: *mut i64) -> PgResult<bool> {
        match self.phase {
            Phase::Idle => return Ok(false),
            Phase::Null => {
                self.phase = Phase::Idle;
                return self.put(mcx, out, Datum::null(), true);
            }
            Phase::First | Phase::More if self.store.is_some() => return self.read(mcx, out),
            Phase::First | Phase::More => {}
        }
        let first = self.phase == Phase::First;
        self.rowcx.reset();
        self.fc.isnull = false;
        self.rsinfo.returnMode = SetFunctionReturnMode::ValuePerCall;
        self.rsinfo.isDone = ExprDoneCond::ExprSingleResult;
        self.rsinfo.expectedDesc = self.desc.as_ref().map(|d| NonNull::from(&**d).cast::<core::ffi::c_void>());
        self.fc.resultinfo = self.rsinfo.as_fmnode_ptr();
        self.fc.set_result_mcx(self.rowcx.mcx());
        // pgstat_init_function_usage's `pgstat_track_functions <= fn_stats` early out
        let stats = self.flinfo.fn_stats < fmgr::TRACK_FUNC_ALL && pgstat::function::pgstat_track_functions() > self.flinfo.fn_stats as i32;
        let usage = if stats { Some(pgstat::function::pgstat_init_function_usage(self.flinfo.fn_oid)?) } else { None };
        let result = self.flinfo.invoke(&mut *self.fc);
        if let Some(usage) = &usage {
            pgstat::function::pgstat_end_function_usage(usage, self.rsinfo.isDone != ExprDoneCond::ExprMultipleResult);
        }
        let result = match result {
            Ok(d) => d,
            Err(e) => {
                // the abort path: no shutdown hook, the cross-call state dropped
                (self.phase, self.rsinfo.srf_shutdown, self.flinfo.fn_extra) = (Phase::Idle, None, None);
                return Err(e);
            }
        };
        self.phase = Phase::More;
        match (self.rsinfo.returnMode, self.rsinfo.isDone) {
            (SetFunctionReturnMode::ValuePerCall, ExprDoneCond::ExprEndResult) => {
                self.phase = Phase::Idle;
                if first && !self.retset {
                    return self.put(mcx, out, Datum::null(), true);
                }
                Ok(false)
            }
            (SetFunctionReturnMode::ValuePerCall, done) => {
                if done != ExprDoneCond::ExprMultipleResult {
                    self.phase = Phase::Idle;
                } else if !self.retset {
                    return Err(protocol("value-per-call"));
                }
                self.put(mcx, out, result, self.fc.isnull)
            }
            (SetFunctionReturnMode::Materialize, done) => {
                if !first || done != ExprDoneCond::ExprSingleResult || !self.retset {
                    return Err(protocol("materialize"));
                }
                let Some(set) = self.rsinfo.setResult.take() else {
                    self.phase = Phase::Idle;
                    return Ok(false);
                };
                let mut store = *set.downcast::<tuplestore::Tuplestore>().expect("rsinfo.setResult downcasts to Tuplestore");
                let src = self.rsinfo.setDesc.take();
                match &self.desc {
                    // SAFETY (both arms): setDesc is live for the call that armed it
                    Some(desc) => {
                        if let Some(src) = src {
                            tupledesc_match(desc, src.cast::<TupleDescData<'_>>().as_ref())?;
                        }
                    }
                    // ExecPrepareTuplestoreResult: a RECORD set's read slot is made once, from the first setDesc
                    None if !self.record_slot => {
                        let Some(src) = src else { return Err(setof_record_not_accepted()) };
                        let smcx: Mcx<'static> = core::mem::transmute::<Mcx<'_>, Mcx<'static>>(self.cx.mcx());
                        let d = Rc::new(tupdesc::CreateTupleDescCopy(smcx, src.cast::<TupleDescData<'_>>().as_ref())?);
                        exectuples::exec_set_slot_descriptor(&mut self.slot, smcx, d);
                        self.record_slot = true;
                    }
                    None => {}
                }
                store.rescan()?;
                self.store = Some(store);
                self.read(mcx, out)
            }
        }
    }

    /// The materialized store's next row.
    unsafe fn read(&mut self, mcx: Mcx<'static>, out: *mut i64) -> PgResult<bool> {
        let smcx: Mcx<'static> = core::mem::transmute::<Mcx<'_>, Mcx<'static>>(self.cx.mcx());
        let store = self.store.as_mut().expect("reading the store");
        if !store.gettupleslot(true, false, &mut self.slot, smcx)? {
            exectuples::exec_clear_tuple(&mut self.slot, smcx);
            if let Some(store) = self.store.take() {
                store.end();
            }
            self.phase = Phase::Idle;
            return Ok(false);
        }
        if self.returns_tuple {
            // `ExecFetchSlotHeapTupleDatum`: the stored row re-formed as a datum of the blessed descriptor
            self.rowcx.reset();
            let d = exectuples::exec_fetch_slot_heap_tuple_datum(&mut self.slot, smcx, self.rowcx.mcx())?;
            return self.put(mcx, out, d, false);
        }
        let mut isnull = false;
        let d = exectuples::slot_getattr(&mut self.slot, 1, &mut isnull);
        self.put(mcx, out, d, isnull)
    }

    /// The row's one column into `out[0]` / `out[1]`: a by-reference value copied into `mcx`, the slot's context (valid
    /// until the slot's next open); a composite handed through whole. Always true (a row).
    unsafe fn put(&self, mcx: Mcx<'static>, out: *mut i64, d: Datum, isnull: bool) -> PgResult<bool> {
        let v = match (isnull, self.returns_tuple) {
            (true, _) => 0,
            (false, true) => copy_datum(mcx, d.as_usize() as *const u8, -1, true)? as i64,
            (false, false) => {
                let a = &self.desc.as_ref().expect("a scalar set has its row").attrs[0];
                if a.attbyval { d.as_i64() } else { copy_datum(mcx, d.as_usize() as *const u8, a.attlen, true)? as i64 }
            }
        };
        out.write(v);
        out.add(1).write(isnull as i64);
        Ok(true)
    }
}

/// M5 H-b: one execution's fmgr sets of a ProjectSet node (exec v4/m5.rs `SetFn`, the handle in `SETFN_W_NODE`), each
/// built at its first open from the node's own target-list entry, so no program holds an execution's expression.
pub struct Sets {
    node: &'static ProjectSet<'static>,
    sets: Vec<Option<Box<FmgrSet>>>,
    leader: std::thread::ThreadId,
}

impl Sets {
    /// `None` unless `node` is a ProjectSet.
    pub fn new(node: Node<'static>) -> Option<Box<Sets>> {
        let node = node.as_project_set()?;
        let sets = (0..node.plan.targetlist.len()).map(|_| None).collect();
        Some(Box::new(Sets { node, sets, leader: std::thread::current().id() })) // alloc: Exec
    }

    /// Target-list entry `resno`'s set; a composite result is one datum (`ExecMakeFunctionResultSet`).
    pub fn get(&mut self, resno: usize) -> PgResult<&mut FmgrSet> {
        if std::thread::current().id() != self.leader {
            return Err(internal("gravity: an fmgr set read off the leader"));
        }
        let node = self.node;
        let set = self.sets.get_mut(resno.wrapping_sub(1)).ok_or_else(|| internal("gravity: no ProjectSet entry for an fmgr set"))?;
        if set.is_none() {
            let te = node.plan.targetlist.iter().filter_map(|n| n.as_target_entry()).find(|t| t.resno as usize == resno);
            let e = te.ok_or_else(|| internal("gravity: no ProjectSet entry for an fmgr set"))?.expr;
            let (funcid, coll, nargs, retset) = match (e.as_func_expr(), e.as_op_expr()) {
                (Some(f), _) => (f.funcid, f.inputcollid, f.args.len(), f.funcretset),
                (_, Some(o)) => (o.opfuncid, o.inputcollid, o.args.len(), o.opretset),
                _ => return Err(internal("gravity: an fmgr set over no function call")),
            };
            // the lowering refused these by name; a refusal now is a catalog changed under the program
            *set = Some(FmgrSet::new(funcid, e, coll, nargs, retset, true, true).map_err(|r| internal(r.what()))?);
        }
        Ok(set.as_deref_mut().expect("built above"))
    }

    /// The execution's normal end (exec v4/m5.rs `Live::end`): each set the reader stopped inside ends as ExecutorEnd's
    /// `ShutdownSetExpr` ends it; an error's end only drops them.
    pub fn end(&mut self) -> PgResult<()> {
        if std::thread::current().id() != self.leader {
            return Err(internal("gravity: an fmgr set ended off the leader"));
        }
        self.sets.iter_mut().flatten().try_for_each(|s| s.shutdown())
    }
}

#[cold]
#[inline(never)]
fn internal(msg: &str) -> Box<PgError> {
    Box::new(PgError::error(msg)) // alloc: Cold
}

/// A by-reference datum's bytes copied into `mcx` (a varlena whole, an external one detoasted when `flat`; a cstring
/// with its NUL; a fixed-length one by `attlen`), 8-aligned.
unsafe fn copy_datum(mcx: Mcx<'static>, p: *const u8, attlen: i16, flat: bool) -> PgResult<usize> {
    if attlen == -1 && flat && varatt::varatt_is_1b_e(p) {
        let raw = core::slice::from_raw_parts(p, varatt::varsize_any(p));
        return Ok(detoast_seams::detoast_attr::call(mcx, raw)?.leak().as_ptr() as usize);
    }
    let n = match attlen {
        -1 => varatt::varsize_any(p),
        -2 => core::ffi::CStr::from_ptr(p.cast()).to_bytes_with_nul().len(),
        l => l as usize,
    };
    let d = mcx.alloc_bytes(n)?.as_ptr();
    core::ptr::copy_nonoverlapping(p, d, n);
    Ok(d as usize)
}

/// execSRF.c `tupledesc_match`: the store's rows (`setDesc`) against the row the query expects.
fn tupledesc_match(dst: &TupleDescData<'_>, src: &TupleDescData<'_>) -> PgResult<()> {
    let mismatch = |detail: String| Box::new(PgError::error("function return row and query-specified return row do not match").with_sqlstate(types_error::ERRCODE_DATATYPE_MISMATCH).with_detail(detail)); // alloc: Cold
    if dst.natts != src.natts {
        let noun = if src.natts == 1 { "attribute" } else { "attributes" };
        return Err(mismatch(format!("Returned row contains {} {noun}, but query expects {}.", src.natts, dst.natts))); // alloc: Cold
    }
    for (i, (d, s)) in dst.attrs.iter().zip(src.attrs.iter()).enumerate().take(dst.natts as usize) {
        if coerce::IsBinaryCoercible(s.atttypid, d.atttypid)? {
            continue;
        }
        if !d.attisdropped {
            let (st, dt) = (format_type::format_type_be(s.atttypid)?, format_type::format_type_be(d.atttypid)?);
            return Err(mismatch(format!("Returned type {st} at ordinal position {}, but query expects {dt}.", i + 1))); // alloc: Cold
        }
        if d.attlen != s.attlen || d.attalign != s.attalign {
            return Err(mismatch(format!("Physical storage mismatch on dropped attribute at ordinal position {}.", i + 1))); // alloc: Cold
        }
    }
    Ok(())
}

/// execSRF.c ExecPrepareTuplestoreResult: a RECORD set whose function gave no setDesc.
#[cold]
#[inline(never)]
fn setof_record_not_accepted() -> Box<PgError> {
    Box::new(PgError::error("function returning setof record called in context that cannot accept type record").with_sqlstate(types_error::ERRCODE_FEATURE_NOT_SUPPORTED)) // alloc: Cold
}

#[cold]
#[inline(never)]
fn protocol(mode: &str) -> Box<PgError> {
    Box::new(PgError::error(format!("table-function protocol for {mode} mode was not followed")).with_sqlstate(types_error::ERRCODE_E_R_I_E_SRF_PROTOCOL_VIOLATED)) // alloc: Cold
}

gravity_abi::host! {
    /// The cell's `open`: `aux` is the `FmgrSet`, whose call steps from here (`FmgrSet::start`).
    #[unwind]
    pub unsafe fn open(e: *mut ErrSlot, _ctx: *mut Ctx, state: *mut u8, args: *const i64, nulls: *const u8, _nargs: i64, aux: i64) -> i64 {
        let set = aux as usize as *mut FmgrSet;
        match (*set).start(fresh_mcx(state), args, nulls) {
            Ok(()) => {
                place(state, set);
                OK
            }
            Err(err) => fail(e, err).status,
        }
    }
}

gravity_abi::host! {
    /// The next row into the out words (`out[0]` / `out[1]`; a by-reference value points at its copy in the slot's context,
    /// valid until the slot's next open, M5 H-a): 1, 0 when exhausted.
    #[unwind]
    pub unsafe fn step(e: *mut ErrSlot, _ctx: *mut Ctx, state: *mut u8, out: *mut i64) -> i64 {
        let set = *state_of::<*mut FmgrSet>(state);
        match (*set).next(slot_cx(state, false), out) {
            Ok(row) => row as i64,
            Err(err) => fail(e, err).status,
        }
    }
}
