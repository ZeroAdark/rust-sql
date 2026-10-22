//! M5's host implementations (M5-plan.md §6, cards/M5.md L2). Created by M5-RIG (owner M5-HOSTS). M5 derives no source.
//! `ScanStore` (H-c FunctionScan, H-d TableFuncScan, H-e NamedTuplestoreScan): the incumbent node over a statement-lived
//! executor state, initialized at the first pull on the leader (R2, R4), read in windows (kernels host_abi v4/m5.rs)
//! through its own `scan_next` (its qual and projection are the reader's IR). By-reference values are copied into the
//! reader's region, reset at its next batch or rewind; a rewind is the node's rescan (each store reread, nothing re-called).
use std::thread::ThreadId;

use datum::Datum;
use executils::{create_executor_state, free_executor_state, EStateData, ExecutorState};
use execscan::ScanNode;
use gravity_driver::{ArenaRef, SourceBox};
pub(crate) use gravity_kernels::ops::srf::entries::{win_bytes, win_valid, StoreHead, STORE_END, STORE_FEED, STORE_MAGIC, WIN_N, WIN_VALUES};
use gravity_program::SiteDesc;
use mcx::MemoryContext;
use nodefunctionscan::{exec_end_function_scan, exec_function_scan_materialize, exec_rescan_function_scan, exec_rescan_function_scan_chg, FunctionScanState};
use nodenamedtuplestorescan::{exec_rescan_named_tuplestore_scan, NamedTuplestoreScanState};
use nodetablefuncscan::{exec_end_table_func_scan, exec_rescan_table_func_scan, exec_rescan_table_func_scan_chg, TableFuncScanState};
use types_error::{PgError, PgResult, ERRCODE_DATATYPE_MISMATCH};
use types_nodes::bitmapset::Bitmapset;
use types_nodes::plannodes::{FunctionScan, Plan, PlannedStmt, Scan};
use types_nodes::list::NodeList;
use types_nodes::parsenodes::RangeTblFunction;
use types_nodes::Node;
use types_portal::params::ParamExecData;
use types_portal::{ParamListHandle, QueryEnvHandle};
use tuplestore::Tuplestore;

use crate::ops::Recipe;
use crate::ops::srf::raw;
use crate::ops::srf::store_row::Fixed;

pub fn derive(_r: &Recipe, _i: usize, _desc: &SiteDesc, _a: ArenaRef) -> Option<SourceBox> {
    None
}

pub const SIZE: usize = 0;

/// `#[repr(C)]` with the head first: the handle is the object's address.
#[repr(C)]
pub struct ScanStore {
    head: StoreHead,
    plan: Node<'static>,
    pstmt: &'static PlannedStmt<'static>,
    params: ParamListHandle,
    query_env: QueryEnvHandle,
    leader: ThreadId,
    run: Option<Run>,
    reader: Option<Reader>,
    /// M8 store_reopen (V4R-439): the exec params the program passed (id, value, NULL), laid into the state at its start
    /// and at each pass, and the ids changed since the last rewind (its chgParam).
    pars: Vec<(i32, u64, bool)>,
    chg: Vec<i32>,
    /// A changed rewind's chgParam lives here, reset at the next one.
    chgcx: Option<Box<MemoryContext>>,
}

/// A node's rescan and end beside its access method (an ENR's store is its registrant's: the Named node ends nothing).
pub(super) trait Store<'m>: ScanNode<'m> {
    fn rescan(&mut self, es: &mut EStateData<'m>) -> PgResult<()>;
    fn end(&mut self) {}
    /// M8 store_reopen: PG's ExecReScan with `chg` its chgParam (a TableFuncScan drops its store for any change).
    fn rescan_chg(&mut self, es: &mut EStateData<'m>, _chg: &Bitmapset<'m>) -> PgResult<()> {
        self.rescan(es)
    }
    /// C2: the store whose rows are the scan's rows, read without the node (one function, no ORDINALITY).
    fn rows(&mut self, _es: &mut EStateData<'m>) -> PgResult<Option<&mut Tuplestore>> {
        Ok(None)
    }
}

macro_rules! store {
    ($t:ident, $rescan:ident $(, $end:ident)? $(; $($rows:item)*)?) => {
        impl<'m> Store<'m> for $t<'m> {
            fn rescan(&mut self, es: &mut EStateData<'m>) -> PgResult<()> {
                $rescan(self, es)
            }
            $(fn end(&mut self) { $end(self) })?
            $($($rows)*)?
        }
    };
}
store!(FunctionScanState, exec_rescan_function_scan, exec_end_function_scan; fn rows(&mut self, es: &mut EStateData<'m>) -> PgResult<Option<&mut Tuplestore>> {
    if self.is_simple() { exec_function_scan_materialize(self, es, 0).map(Some) } else { Ok(None) }
}
fn rescan_chg(&mut self, es: &mut EStateData<'m>, chg: &Bitmapset<'m>) -> PgResult<()> {
    exec_rescan_function_scan_chg(self, es, chg)
});
store!(TableFuncScanState, exec_rescan_table_func_scan, exec_end_table_func_scan; fn rescan_chg(&mut self, es: &mut EStateData<'m>, _chg: &Bitmapset<'m>) -> PgResult<()> {
    exec_rescan_table_func_scan_chg(self, es)
});
store!(NamedTuplestoreScanState, exec_rescan_named_tuplestore_scan);

/// The node, then the executor state whose query context holds its data (`Drop` ends them in that order).
struct Run {
    node: Option<Box<dyn Store<'static>>>,
    /// D975: whether a Var of the node's own qual or projection no longer matches the scan row (`stale`); None until
    /// the first row
    stale: Option<bool>,
    state: Option<ExecutorState>,
    _cx: gravity_driver::mem::root::Root,
    /// D1874: the node was initialized over a copy of the FunctionScan with no qual and no target list (`bare`)
    bare: bool,
}

struct Reader {
    max: u32,
    natts: u32,
    window: Vec<u64>,
    region: Box<MemoryContext>,
}

impl ScanStore {
    /// `None` unless `plan` is a FunctionScan, a TableFuncScan or a NamedTuplestoreScan. Nothing runs until the first batch.
    pub fn new(pstmt: &'static PlannedStmt<'static>, plan: Node<'static>, params: ParamListHandle, query_env: QueryEnvHandle) -> Option<Box<ScanStore>> {
        (plan.as_function_scan().is_some() || plan.as_table_func_scan().is_some() || plan.as_named_tuplestore_scan().is_some()).then(|| {
            let head = StoreHead { magic: STORE_MAGIC, batch: raw::batch_of, rewind: raw::rewind_of, param: raw::param_of };
            Box::new(ScanStore { head, plan, pstmt, params, query_env, leader: std::thread::current().id(), run: None, reader: None, pars: Vec::new(), chg: Vec::new(), chgcx: None }) // alloc: Exec
        })
    }

    /// The handle the IR passes to `store_batch` / `store_rewind` (`SETFN_W_NODE`).
    pub fn handle(&mut self) -> u64 {
        &mut self.head as *mut StoreHead as u64
    }

    pub(super) fn batch(&mut self, ptr: u32, max: u32, natts: u32, need: u64) -> PgResult<u64> {
        self.check(ptr)?;
        cancelpoll::interrupts::check()?;
        let reader = match &mut self.reader {
            Some(r) if (r.max, r.natts) == (max, natts) => r,
            Some(_) => return Err(bug("gravity: a store reader changed its window's shape")),
            None if max == 0 || max % 64 != 0 => return Err(bug("gravity: a store window of rows not a multiple of 64")),
            None => {
                let region = Box::new(MemoryContext::new_bump("gravity srf store batch")); // alloc: Exec
                self.reader.insert(Reader { max, natts, window: vec![0u64; win_bytes(natts as usize, max as usize).div_ceil(8)], region }) // alloc: Exec
            }
        };
        reader.region.reset();
        let (win, rmcx) = (reader.window.as_mut_ptr() as *mut u8, reader.region.mcx());
        if self.run.is_none() {
            self.run = Some(Run::start(self.pstmt, self.plan, self.params, self.query_env, &self.pars)?);
        }
        let n = self.run.as_mut().expect("started").pull(self.plan, win, rmcx, max as usize, natts as usize, need)?;
        raw::put_count(win, n);
        Ok(if n == 0 { STORE_END } else { win as u64 })
    }

    /// Before the first batch there is nothing to rewind (PG's rescan of a node never run). M8 store_reopen: with params
    /// changed since the last rewind, the node's rescan under them as its chgParam (a call reading one re-runs, PG's
    /// ExecReScanFunctionScan); none changed, its stores are reread.
    pub(super) fn rewind(&mut self, ptr: u32) -> PgResult<()> {
        self.check(ptr)?;
        if let Some(r) = &mut self.reader {
            r.region.reset();
        }
        let chg = core::mem::take(&mut self.chg);
        let Some(run) = self.run.as_mut() else { return Ok(()) };
        if chg.is_empty() {
            return run.with(|node, es| node.rescan(es));
        }
        let cx = self.chgcx.get_or_insert_with(|| Box::new(MemoryContext::new_bump("GravitySrfScan chgParam"))); // alloc: Exec
        cx.reset();
        let mut set = Bitmapset::empty();
        chg.iter().try_for_each(|&id| set.add_member(cx.mcx(), id))?;
        run.with(|node, es| node.rescan_chg(es, raw::chg_param(&set)))
    }

    /// M8 store_reopen: exec param `id` as its owner wrote it (`flags`: lowerv4 params.rs `SET` 1, `NULL` 2), into the
    /// state now when it runs, else at its start; `changed` joins the next rewind's chgParam.
    pub(super) fn param(&mut self, id: u32, value: u64, flags: u64, changed: bool) -> PgResult<()> {
        self.check(0)?;
        let n = self.pstmt.paramExecTypes.len();
        // D1874: a call's SubPlan value, the exec param `bare` put in its place (past the plan's own, one per subplan)
        let id = match id.checked_sub(STORE_FEED) {
            Some(p) => (1..=self.pstmt.subplans.len()).contains(&(p as usize)).then(|| (n + p as usize - 1) as i32),
            None => i32::try_from(id).ok().filter(|&i| (i as usize) < n),
        };
        let Some(id) = id.filter(|_| flags & 1 != 0) else { return Err(bug("gravity: a store param unset or past the plan's exec params")) };
        let isnull = flags & 2 != 0;
        match self.pars.iter_mut().find(|p| p.0 == id) {
            Some(p) => (p.1, p.2) = (value, isnull),
            None => self.pars.push((id, value, isnull)), // alloc: Exec
        }
        if changed && !self.chg.contains(&id) {
            self.chg.push(id); // alloc: Exec
        }
        self.run.as_mut().map_or(Ok(()), |run| {
            run.with(|_, es| {
                // D1874: a fed id past the state's array (its node started with no SubPlan in a call) is a lowering bug
                if id as usize >= es.es_param_exec_vals.len() {
                    return Err(bug("gravity: a store param past its state's exec params"));
                }
                lay(es, id, value, isnull);
                Ok(())
            })
        })
    }

    fn check(&self, ptr: u32) -> PgResult<()> {
        if std::thread::current().id() != self.leader {
            return Err(bug("gravity: a scan store read off the leader"));
        }
        (ptr == 0).then_some(()).ok_or_else(|| bug("gravity: a scan store has one reader (read pointer 0)"))
    }
}

impl Drop for ScanStore {
    fn drop(&mut self) {
        self.head.magic = 0;
    }
}

impl Run {
    /// The node's `ExecInit*` over a fresh executor state carrying this execution's parameters and query environment.
    fn start(pstmt: &'static PlannedStmt<'static>, plan: Node<'static>, params: ParamListHandle, query_env: QueryEnvHandle, pars: &[(i32, u64, bool)]) -> PgResult<Run> {
        let cx = gravity_driver::mem::root::Root::new("GravitySrfScan");
        let mut run = Run { node: None, stale: None, state: Some(create_executor_state(cx.mcx().context())?), _cx: cx, bare: false };
        let mut is_bare = false;
        let node = run.state.as_mut().expect("live state").with_mut(|es| -> PgResult<Box<dyn Store<'static>>> {
            raw::bind(es, pstmt, params, query_env);
            // M8 store_reopen: the plan's exec params sized before ExecInit binds their array (as InitPlan's
            // ExecutorStart), the passed ones laid
            // D1874: a call holding a SubPlan reads it as an exec param past the plan's own, one per subplan (`bare`)
            let fed = plan.as_function_scan().is_some_and(|f| f.functions.iter().any(|r| r.as_range_tbl_function().and_then(|r| r.funcexpr).is_some_and(sub)));
            let n = pstmt.paramExecTypes.len();
            let m = n + if fed { pstmt.subplans.len() } else { 0 };
            es.es_param_exec_vals.extend(core::iter::repeat_n(ParamExecData::EMPTY, m));
            es.es_param_subplans.extend(core::iter::repeat_n(None, m));
            pars.iter().for_each(|&(id, v, isnull)| lay(es, id, v, isnull));
            let mcx = es.es_query_cxt;
            let bare = match plan.as_function_scan() {
                Some(f) => bare(mcx, f, n as i32)?,
                None => None,
            };
            is_bare = bare.is_some();
            raw::init(mcx, es, plan, bare)
        });
        run.node = Some(node?);
        run.bare = is_bare;
        Ok(run)
    }

    /// Runs `f` over the node and the executor state it lives in.
    fn with<T>(&mut self, f: impl for<'a> FnOnce(&mut dyn Store<'a>, &mut EStateData<'a>) -> PgResult<T>) -> PgResult<T> {
        let node = &mut **self.node.as_mut().expect("live node");
        self.state.as_mut().expect("live state").with_mut(|es| {
            f(raw::narrow(node), es)
        })
    }

    /// Up to `max` rows into the window; the rows pulled.
    fn pull(&mut self, plan: Node<'static>, win: *mut u8, rmcx: mcx::Mcx<'_>, max: usize, natts: usize, need: u64) -> PgResult<usize> {
        let mut stale_of = self.stale;
        let bare = self.bare;
        let n = self.with(|node, es| {
            let valid = raw::clear_valid(win, natts, max);
            let (mut n, ix, qcx) = (0, node.ss_mut().ss_ScanTupleSlot, es.es_query_cxt);
            let rows = node.rows(es)?.map(|s| s as *mut Tuplestore);
            let fixed = rows.and_then(|_| es.slot_mut(ix).base().tts_tupleDescriptor.as_deref().and_then(|d| Fixed::of(d, natts, need)));
            while n < max {
                cancelpoll::interrupts::check()?;
                if !match rows { Some(s) => raw::store_of(s).gettupleslot(true, false, es.slot_mut(ix), qcx)?, None => node.scan_next(es)? } {
                    break;
                }
                // D975: a stale Var raises where PG raises it (CheckVarSlotCompatibility at its expression's first run): each
                // row goes through the node's own qual and projection, which raise PG's error or reject the row
                // D1217: the descriptor is read once, at the first row (no Rc clone per row)
                if stale_of.is_none() {
                    stale_of = Some(es.slot_mut(ix).base().tts_tupleDescriptor.as_deref().is_some_and(|d| stale(plan, d)));
                }
                if stale_of == Some(true) {
                    if bare {
                        // D1874: the node holds no qual or projection to raise it (PG: CheckVarSlotCompatibility)
                        return Err(Box::new(PgError::error("gravity: a function's row type no longer matches the scan's columns").with_sqlstate(ERRCODE_DATATYPE_MISMATCH))); // alloc: Cold
                    }
                    if execscan::lane_scan_accept(node.ss_mut(), es, ix)?.is_some() {
                        return Err(bug("gravity: a scan row whose Vars the descriptor refused passed its projection"));
                    }
                    continue;
                }
                let slot = es.slot_mut(ix);
                if fixed.as_ref().is_some_and(|f| f.deform(slot, win, valid, max, n, need)) {
                    n += 1;
                    continue;
                }
                let Some(desc) = slot.base().tts_tupleDescriptor.clone().filter(|d| natts <= d.natts as usize) else {
                    return Err(bug("gravity: a store window wider than the scan's row"));
                };
                exectuples::slot_getsomeattrs(slot, natts as i32);
                let base = slot.base();
                // bounded("key-columns: one cell per column of the scan's row; PG polls no deeper (a datum copy is one unit there too), at most MaxTupleAttributeNumber (1664) columns")
                for c in (0..natts).filter(|&c| (c >= 64 || need >> c & 1 != 0) && !base.tts_isnull[c]) {
                    let (a, d) = (&desc.attrs[c], base.tts_values[c]);
                    let v = if a.attbyval { d.as_usize() as u64 } else { raw::copy(rmcx, d.as_usize() as *const u8, a.attlen)? };
                    raw::put(win, valid, max, c, n, v);
                }
                n += 1;
            }
            Ok(n)
        });
        self.stale = stale_of;
        n
    }
}

/// Whether expression `n` holds a SubPlan (or an AlternativeSubPlan).
fn sub(n: Node<'_>) -> bool {
    let mut s = false;
    gravity_decompose::expr::walk(n, &mut |x| {
        s |= x.as_sub_plan().is_some() || x.as_alternative_sub_plan().is_some();
        !s
    });
    s
}

/// D1874: a copy of FunctionScan `f` in `mcx` (the state's query context, which outlives the node) with no qual and no
/// target list when either or a call holds a SubPlan, else None. The qual and the projection are the reader's IR (the
/// pipeline's filters and projection after the store), so the node needs neither, and this executor state holds no
/// SubPlan state to compile one with (PG's 'SubPlan found with no parent plan'). A call's SubPlan is the program's: each
/// is replaced by the exec param `base + plan_id - 1` (decompose `fed_params`; `base` the plan's own exec params), whose
/// value the program evaluates on the step's row and passes before every rewind (`param`, `STORE_FEED`), and that id joins
/// the call's funcparams, so a rewind that passes it recomputes the call (PG's ExecReScanFunctionScan on a changed param).
fn bare<'m>(mcx: mcx::Mcx<'m>, f: &FunctionScan<'m>, base: i32) -> PgResult<Option<&'m FunctionScan<'m>>> {
    let p = &f.scan.plan;
    let calls = f.functions.iter().any(|r| r.as_range_tbl_function().and_then(|r| r.funcexpr).is_some_and(sub));
    if !calls && !p.qual.iter().chain(p.targetlist.iter()).any(sub) {
        return Ok(None);
    }
    let plan = Plan { plan_node_id: p.plan_node_id, parallel_safe: p.parallel_safe, ..Default::default() };
    let functions = if calls { fed(mcx, &f.functions, base)? } else { f.functions.clone_in(mcx)? };
    mcx::alloc_leak_in(mcx, FunctionScan { scan: Scan { plan, scanrelid: f.scan.scanrelid }, functions, funcordinality: f.funcordinality }).map(Some)
}

/// D1874: FROM functions `l` with each call's SubPlans replaced by their exec params (`bare`), each id in its funcparams.
fn fed<'m>(mcx: mcx::Mcx<'m>, l: &NodeList<'m>, base: i32) -> PgResult<NodeList<'m>> {
    let mut out = NodeList::with_capacity(mcx, l.len())?;
    // bounded("other: the FROM functions of one FunctionScan, copied once at bind: the query text bounds them, as PG's ExecInitFunctionScan walks the same list with no poll")
    for n in l.iter() {
        let mut ids = Vec::new(); // alloc: Exec
        let r = n.as_range_tbl_function();
        let e = match r.and_then(|r| r.funcexpr) {
            Some(e) => gravity_decompose::params::fed_params(mcx, e, base, &mut ids)?,
            None => None,
        };
        let (Some(r), Some(e)) = (r, e) else {
            out.lappend(mcx, n)?;
            continue;
        };
        let mut funcparams = r.funcparams.clone_in(mcx)?;
        ids.iter().try_for_each(|&id| funcparams.add_member(mcx, id))?;
        let r = RangeTblFunction {
            funcexpr: Some(e),
            funccolcount: r.funccolcount,
            funccolnames: r.funccolnames.clone_in(mcx)?,
            funccoltypes: r.funccoltypes.clone_in(mcx)?,
            funccoltypmods: r.funccoltypmods.clone_in(mcx)?,
            funccolcollations: r.funccolcollations.clone_in(mcx)?,
            funcparams,
        };
        out.lappend(mcx, Node::mk(mcx, r)?)?;
    }
    Ok(out)
}

/// D975: whether a Var of scan node `plan`'s own qual or projection no longer matches the scan row's descriptor `d`: an
/// attribute past its end, dropped, or of another type (a view or a prepared plan over a function whose row type changed
/// since). PG's CheckVarSlotCompatibility raises it at that expression's first run; the program would read the wrong type.
fn stale(plan: Node<'_>, d: &types_tuple::TupleDescData<'_>) -> bool {
    let scan = plan.as_function_scan().map(|f| &f.scan).or_else(|| plan.as_table_func_scan().map(|t| &t.scan));
    let Some(scan) = scan.or_else(|| plan.as_named_tuplestore_scan().map(|n| &n.scan)) else { return false };
    let mut bad = false;
    // bounded("other: the scan node's own qual and target list entries, walked once at the first row: the query text bounds them, as PG's ExecInitExpr walks the same expressions with no poll")
    for n in scan.plan.qual.iter().chain(scan.plan.targetlist.iter()) {
        gravity_decompose::expr::walk(n, &mut |x| {
            if let Some(v) = x.as_var().filter(|v| v.varno == scan.scanrelid as i32 && v.varlevelsup == 0 && v.varattno > 0) {
                let a = d.attrs.get(v.varattno as usize - 1).filter(|_| (v.varattno as i32) <= d.natts);
                bad |= a.is_none_or(|a| a.attisdropped || a.atttypid != v.vartype);
            }
            !bad
        });
    }
    bad
}

impl Drop for Run {
    fn drop(&mut self) {
        if let Some(mut node) = self.node.take() {
            node.end();
        }
        if let Some(state) = self.state.take() {
            free_executor_state(state);
        }
    }
}

/// Exec param `id` (inside the plan's array) set in the store's own state.
fn lay(es: &mut EStateData<'_>, id: i32, value: u64, isnull: bool) {
    es.es_param_exec_vals[id as usize] = ParamExecData { value: Datum::from_usize(value as usize), isnull, exec_plan: false };
}

#[cold]
#[inline(never)]
fn bug(msg: &str) -> Box<PgError> {
    Box::new(PgError::error(msg)) // alloc: Cold
}
