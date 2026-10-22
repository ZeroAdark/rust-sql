//! M10's host implementations (window.agg): the WindowAgg's window object behind the
//! handle word the kernels entries (host_abi/v4/m10.rs) call through. It ports nodeWindowAgg.c over the rows the
//! Materialize end stored: the stored rows (this file), the partition and frame positions (m10_frame.rs), the window
//! functions and the per-row driver (m10_funcs.rs) and the aggregates (m10_aggs.rs). The lowered IR evaluates every
//! argument and FILTER when it stores a row; a cell whose evaluation raised keeps its error, raised only when the object
//! reads that cell, where PG would evaluate the expression. Under a VOLATILE argument the IR leaves every cell to the
//! object, which evaluates it at each read (D1232); D3584: the store keeps a computed by-reference argument's wide value
//! as such a cell instead of its copy (`WinSpec::lazy_cells`). M10 derives no source of its own. Created by M10-WINDOW.
use core::cell::RefCell;
use std::rc::Rc;

use gravity_driver::{ArenaRef, SourceBox};
pub(crate) use gravity_kernels::ops::window::entries as abi;
use gravity_program::SiteDesc;
use mcx::MemoryContext;
use types_core::Oid;
use types_error::{PgError, PgResult};
use types_nodes::plannodes::WindowAgg;
use types_nodes::primnodes::WindowFunc;

use crate::ops::Recipe;


/// No recipe kind of M10's own: M10 derives nothing.
pub fn derive(_r: &Recipe, _i: usize, _desc: &SiteDesc, _a: ArenaRef) -> Option<SourceBox> {
    None
}

pub const SIZE: usize = 0;

/// The stored rows one chunk holds (a chunk never moves: a stored row's address lives until the fill's next open or the
/// execution's end): the cost home's chosen granularity (rule R11).
pub(crate) const CHUNK_ROWS: usize = gravity_program::cost::WINDOW_CHUNK_ROWS;

/// What the protocol (exec v4/m10.rs) knows of one WindowAgg: its plan, its window functions in PG's order (the qual's,
/// then the target list's, `equal` ones once: ExecInitWindowAgg's `winstate->funcs`), and the stored row's layout.
pub struct WinSpec {
    pub plan: &'static WindowAgg<'static>,
    /// E-kern: this and the spec's other lists are charged (`ChargedVec`) to the execution's prepare owner (exec's
    /// `Prepare::owner`), plan-sized, built once per prepare.
    pub funcs: memledger::ChargedVec<&'static WindowFunc<'static>>,
    /// Every stored cell's `(typlen, byval)`: the child's columns, then each function's argument cells.
    pub cols: memledger::ChargedVec<(i16, bool)>,
    /// The child's columns (the first cells).
    pub natts: usize,
    /// Per function: its first argument cell, its argument count and its FILTER's cell.
    pub args: Vec<(usize, usize, Option<usize>)>,
    /// Per function: its arguments' types (the aggregates' transition functions are bound with them).
    pub argtypes: memledger::ChargedVec<memledger::ChargedVec<Oid>>,
    /// Per function: the windowfuncs.c function it runs (its own OID; a user internal WINDOW function's prosrc's, D1233).
    pub builtins: memledger::ChargedVec<Oid>,
    /// Per function: no VOLATILE call and no SubPlan in it (initialize_peragg's moving-aggregate conditions).
    pub moving_ok: memledger::ChargedVec<bool>,
    /// The start and end offsets' `(typlen, byval)` (`calculate_frame_offsets`' copies).
    pub offsets: [(i16, bool); 2],
    /// D1232: a function's argument or FILTER calls a VOLATILE function; the IR stored every argument cell `CELL_LAZY`.
    /// D3584: or some cell may be `CELL_LAZY` (`lazy_cells`).
    pub lazy: bool,
    /// D1232 / D3584: per stored cell (the child's columns first, never lazy), whether it may be `CELL_LAZY`, the object
    /// evaluating it at each read: every argument and FILTER cell under a VOLATILE argument (the IR stores them so), else
    /// each decompose `deferred_arg` (a computed by-reference argument) of a window lowering marked so (lowerv4 window
    /// `lazy_cells`), which the store keeps as `CELL_LAZY` when its value is wide (store.rs `put`).
    pub lazy_cells: memledger::ChargedVec<bool>,
    /// D1355: this execution's extern parameters (a lazy argument's `$n`), copied at prepare; by-reference values point
    /// into the portal's (or SPI's) list memory, which outlives the execution.
    pub params: Box<[types_portal::params::ParamExternData]>, // alloc: Exec
    /// D2834: the list owner's fetch for a slot it left to evaluation time (`types_portal::params::fetch_hook`), which a
    /// lazy argument's `$n` takes where it is evaluated, as ExecEvalParamExtern does.
    pub fetch: Option<types_portal::params::ParamFetchHook>,
    /// The statement's executor-parameter count (`PlannedStmt.paramExecTypes`): a lazy argument's PARAM_EXEC array (W3).
    pub n_exec: usize,
    /// W3: an InitPlan parameter's value and NULL (nonzero) from its Init cell, the InitPlan run first if this execution
    /// has not run it (exec's L0-k read, `rerun::init_param`): a lazy cell's first read of a pending parameter.
    pub init_param: fn(u64, i32) -> PgResult<(u64, u8)>,
    /// The exact request (PLAN-v5 §2.5; W4): a pulled fill's pull (exec's, through the runner's frame): its pipeline run
    /// on until it stored the rows the store wants (true) or ended (false). `None`: an eager fill, whole before the reader.
    pub pull: Option<Box<dyn FnMut() -> PgResult<bool>>>, // alloc: Exec
    /// D4237: the pulled fill's re-arm (exec's, through the runner's frame) by the run of a rerun set holding it: its pulled
    /// set starts again, so the next pull runs the fill from its start. `None` with an eager fill.
    pub rearm: Option<Box<dyn FnMut() -> PgResult<()>>>, // alloc: Exec
    /// W10 (the budgeted pull): the pulled fill streams, so a pull may read ahead of the spool: its budget doubles from
    /// one row, pull after pull (`Rt::more`). False: every pull takes one row (the exact request).
    pub grow: bool,
    /// D3336 (win.partruns): one of a window's objects read by claims (exec lays one per fill claim ordinal): a pool
    /// thread fills it and another reads it, one at a time, its functions all `pool_safe`. Its key comparisons run there
    /// (D1368's leader-only check is a serial object's).
    pub parted: bool,
}

pub use crate::ops::window::raw::rearm_at;

/// D3336: the types a parted window's aggregate may take, return or keep its state in: by-value or numeric words whose
/// functions read no catalog (no array, record, domain or collatable type: those read type rows or a collation).
const PLAIN_TYPES: &[Oid] = &[21, 23, 20, 1700, 700, 701, 1082, 16, 26];
/// `internal` (a numeric or int128 accumulator's state in the aggregate's context).
const INTERNAL: Oid = 2281;
/// D3336: the transition functions of sum over int4 / int8 / numeric (and avg over numeric) a parted window may run on a
/// pool thread beyond the catalog-free builtins (`pool_hosted`): numeric builtins.rs `int4_sum` (two words), and
/// `int8_avg_accum` / `numeric_avg_accum`, whose state lives in the call's aggregate context (`agg_state_arg`); none reads a
/// catalog, a GUC or session state.
const POOL_TRANS: &[Oid] = &[1841, 2746, 2858];
/// D3336: the final functions of sum / avg over numeric and int8 a parted window may run on a pool thread (numeric
/// aggregates.rs `numeric_sum`, `numeric_avg`, `numeric_poly_sum`, `numeric_poly_avg`: the state and the call's result
/// context alone).
const POOL_FINALS: &[Oid] = &[3178, 1837, 3388, 3389];

/// D3336 (win.partruns): whether every function WindowAgg `plan`'s objects call per row may run on a pool thread, so its
/// window may be read by claims: its partition and order equality functions catalog-free builtins
/// (`gravity_fn::session::pool_hosted`), no frame offset (an in_range function), each window function windowfuncs.c's
/// own (its rows and the stored cells alone) or an aggregate whose transition, inverse and final functions (the moving
/// set when initialize_peragg would take it, aggs.rs `Agg::new`'s reading) are catalog-free (`pool_hosted`,
/// `POOL_TRANS`, `POOL_FINALS`), over `PLAIN_TYPES` (its state `internal` too). Read once at prepare, on the leader; false
/// sends the window's claims to one object (its serial form).
pub fn pool_safe(plan: &WindowAgg<'_>, funcs: &[&WindowFunc<'_>], argtypes: &[memledger::ChargedVec<Oid>], moving_ok: &[bool]) -> PgResult<bool> {
    use types_nodes::rawnodes::{FRAMEOPTION_END_OFFSET, FRAMEOPTION_START_OFFSET, FRAMEOPTION_START_UNBOUNDED_PRECEDING};
    if plan.frameOptions & (FRAMEOPTION_START_OFFSET | FRAMEOPTION_END_OFFSET) != 0 {
        return Ok(false);
    }
    let pool = gravity_fn::session::pool_hosted;
    let keys_ok = |n: i32, ops: &[Oid]| -> PgResult<bool> {
        let mut ok = true;
        // bounded("key-columns: one opcode read per partition or order key at prepare, at most MaxTupleAttributeNumber (1664) keys")
        for &op in ops.iter().take(n.max(0) as usize) {
            ok &= pool(lsyscache::get_opcode(op)?);
        }
        Ok(ok)
    };
    if !keys_ok(plan.partNumCols, plan.partOperators)? || !keys_ok(plan.ordNumCols, plan.ordOperators)? {
        return Ok(false);
    }
    let trans_ok = |f: Oid| f == 0 || POOL_TRANS.contains(&f) || pool(f);
    let final_ok = |f: Oid| f == 0 || POOL_FINALS.contains(&f) || pool(f);
    let state = |t: Oid| t == INTERNAL || PLAIN_TYPES.contains(&t);
    let read_only = crate::ops::window::aggs::AGGMODIFY_READ_ONLY;
    let mut ok = true;
    // bounded("other: the WindowAgg's window functions, one catalog read each at prepare: the query text bounds them, as ExecInitWindowAgg's walk")
    for (i, w) in funcs.iter().enumerate() {
        ok &= argtypes.get(i).is_some_and(|ts| ts.iter().all(|t| PLAIN_TYPES.contains(t)));
        if !w.winagg {
            ok &= (3100..=3114).contains(&w.winfnoid);
            continue;
        }
        let Some(a) = syscache_seams::lookup_pg_aggregate_shape::call(w.winfnoid)? else { return Ok(false) };
        // initialize_peragg's choice of the moving set (aggs.rs `Agg::new`)
        let moving = a.aggminvtransfn != 0
            && (a.aggmfinalmodify == read_only && a.aggfinalmodify != read_only || plan.frameOptions & FRAMEOPTION_START_UNBOUNDED_PRECEDING == 0 && moving_ok.get(i).copied().unwrap_or(true));
        let (trans, inv, fin, st) = match moving {
            true => (a.aggmtransfn, a.aggminvtransfn, a.aggmfinalfn, a.aggmtranstype),
            false => (a.aggtransfn, 0, a.aggfinalfn, a.aggtranstype),
        };
        ok &= PLAIN_TYPES.contains(&w.wintype) && state(st) && trans_ok(trans) && trans_ok(inv) && final_ok(fin);
    }
    Ok(ok)
}

/// PG's WindowAggStatus.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Status {
    Run,
    PassThrough,
    PassThroughStrict,
    Done,
}

/// The object: the cell the IR reads first (`h` points at it), the runtime behind it.
#[repr(C)]
pub struct WinObj {
    cell: abi::WinCell,
    /// The stored rows (the fill's side), apart from the walk so a pulled fill can write them inside the object's call.
    pub(super) store: Rc<RefCell<crate::ops::window::store::Store>>,
    pub(super) rt: RefCell<Rt>,
}

/// The nodeWindowAgg.c state over the stored rows.
pub(crate) struct Rt {
    pub(crate) spec: WinSpec,
    /// The stored rows (`WinObj::store`'s): borrowed only for a read, never across a pull of the fill.
    pub(crate) store: Rc<RefCell<crate::ops::window::store::Store>>,
    /// The fill ran to its end (an eager fill's always has, before the reader).
    pub(crate) ended: bool,
    /// The rows the next pull of the fill takes: one, doubling after each pull when the spec grows (W10), at most
    /// `gravity_program::cost::WINDOW_PULL_MAX_ROWS` (D3345).
    pub(crate) budget: u64,
    /// W10: the error a budgeted pull raised past the rows the spool wanted, kept for the next pull (`Rt::more`).
    pub(crate) held: Option<Box<PgError>>, // alloc: Cold
    /// The result cells: a datum word per function, then a validity byte each.
    pub(crate) res: Box<[u64]>, // alloc: Exec
    pub(crate) f: crate::ops::window::frame::Frame,
    pub(crate) fns: crate::ops::window::funcs::Funcs,
    pub(crate) aggs: crate::ops::window::aggs::Aggs,
    pub(crate) status: Status,
    /// The row last answered, its answer and (once asked) its run condition's.
    pub(crate) last: Option<(u64, i64, Option<i64>)>,
    /// A row's function results and calls' results (reset at each row).
    pub(crate) row_cx: MemoryContext,
    /// D1232: the argument cells' evaluators of a window with a VOLATILE argument.
    pub(crate) lazy: Option<crate::ops::window::lazy::Lazy>,
    /// D3951: the reader's last open said a later open walks the same stored rows again (`abi::OPEN_REWALK`): the walk
    /// frees no passed partition (D3345's release waits for the fill's next open).
    pub(crate) rewalks: bool,
    /// D1419: the object's memory root, every context above its child (the per-row / per-call / per-aggregate ones reset
    /// wholesale, PG's AllocSetReset), made by the mem home (rule C14); last, so it drops after them.
    _root: gravity_driver::mem::root::Root,
}

impl WinObj {
    /// The object over `spec`: every comparison and in_range function bound, every aggregate's catalog read and its
    /// EXECUTE checks made (ExecInitWindowAgg's order: the functions, then the frame's), an error by PG's text else.
    pub fn new(spec: WinSpec) -> PgResult<Box<WinObj>> {
        let ncols = spec.cols.len();
        if spec.args.len() != spec.funcs.len() || spec.argtypes.len() != spec.funcs.len() || spec.moving_ok.len() != spec.funcs.len() || spec.builtins.len() != spec.funcs.len() || spec.natts > ncols {
            return Err(bad("window.agg: a window object whose layout disagrees with its functions"));
        }
        let nf = spec.funcs.len();
        let root = gravity_driver::mem::root::Root::new("gravity window");
        let rcx = root.mcx().context();
        let fns = crate::ops::window::funcs::Funcs::new(&spec)?;
        let aggs = crate::ops::window::aggs::Aggs::new(&spec, rcx)?;
        let f = crate::ops::window::frame::Frame::new(&spec, rcx)?;
        let lazy = crate::ops::window::lazy::Lazy::of(&spec, rcx)?;
        let mut res = vec![0u64; nf + nf.div_ceil(8)].into_boxed_slice(); // alloc: Exec
        let cell = abi::WinCell { head: abi::Head { magic: abi::WIN_MAGIC, op: crate::ops::window::raw::op }, res: res.as_mut_ptr() as u64, nfuncs: nf as u64, ncols: ncols as u64 };
        // E-kern: the store's own copies of the cell layout, charged as the spec's are (its prepare owner)
        let mut cols = memledger::ChargedVec::<(i16, bool)>::new(spec.cols.owner(), memledger::Kind::Region);
        cols.with_vec(|v| v.extend_from_slice(&spec.cols)); // alloc: Exec charged spec.cols.owner()
        let mut defer = memledger::ChargedVec::<bool>::new(spec.lazy_cells.owner(), memledger::Kind::Region);
        defer.with_vec(|v| v.extend_from_slice(&spec.lazy_cells)); // alloc: Exec charged spec.lazy_cells.owner()
        let store = Rc::new(RefCell::new(crate::ops::window::store::Store::new(cols, defer, rcx.new_child_wholesale("gravity window rows")))); // alloc: Exec
        let ended = spec.pull.is_none();
        let rt = Rt {
            spec,
            store: store.clone(),
            ended,
            budget: 1,
            rewalks: false,
            held: None,
            res,
            f,
            fns,
            aggs,
            status: Status::Run,
            last: None,
            row_cx: rcx.new_child_wholesale("gravity window row"),
            lazy,
            _root: root,
        };
        Ok(Box::new(WinObj { cell, store, rt: RefCell::new(rt) })) // alloc: Exec
    }

    /// The handle word the program reads: the cell's address.
    pub fn handle(&self) -> u64 {
        &self.cell as *const abi::WinCell as u64
    }

    /// D4237 (PG's ExecReScanWindowAgg: the spool emptied, spool_tuples reading the child again from its start): the
    /// object re-armed by the run of the rerun set holding its pulled fill, ahead of the reader's next open. The stored
    /// rows go, the walk restarts, the fill is not ended and holds no error, the budget is one row again (W10's doubling
    /// starts over), and the fill's pulled set starts again (`WinSpec::rearm`). Inside the object's own call, or for an
    /// eager fill, a Bug by name.
    pub(crate) fn rearm(&self) -> PgResult<()> {
        let Ok(mut rt) = self.rt.try_borrow_mut() else {
            return Err(bad("window.agg: a window re-armed inside its own call (D4237)"));
        };
        if rt.spec.pull.is_none() {
            return Err(bad("window.agg: an eager window's fill re-armed (D4237)"));
        }
        rt.open(true);
        rt.ended = false;
        rt.budget = 1;
        match rt.spec.rearm.as_mut() {
            Some(rearm) => rearm(),
            None => Err(bad("window.agg: a pulled fill with no re-arm (D4237)")),
        }
    }
}

impl Drop for WinObj {
    fn drop(&mut self) {
        self.cell.head.magic = 0;
    }
}

#[cold]
#[inline(never)]
pub(crate) fn bad(what: &str) -> Box<PgError> {
    Box::new(PgError::error(format!("gravity: {what}")).with_sqlstate(types_error::ERRCODE_INTERNAL_ERROR)) // alloc: Cold
}
