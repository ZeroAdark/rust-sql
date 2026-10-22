//! M10's lazy argument cells (window.agg, D1232): under a VOLATILE argument or FILTER the IR stores every argument cell
//! `CELL_LAZY` and the window object evaluates it at each read, as PG's ExecEvalExpr at WinGetFuncArg* and each aggregate
//! advance, by an ExprState over the stored row's child columns. The states read the execution's extern list (D1355) and
//! an executor-parameter array (W3, window_core-0190 / -0191) the reader's open writes (`m10_win_params`): a nest's or a
//! SubPlan's PARAM_EXEC by value, as PG's ExecEvalParamExec reads es_param_exec_vals; an InitPlan's pending (PG's
//! `execPlan`), its Init cell run at the parameter's first read (ExecEvalParamExec's ExecSetParamPlan): an InitPlan no
//! read reaches never runs. Split from m10_store.rs. Created by M10-WINDOW.
use core::cell::Cell;

use datum::{Datum, NullableDatum};
use execexpr::{EvalOutcome, EvalSlots, ExprState, ParamBind, SuspendKind};
use mcx::{MemoryContext, PgBox, PinnedContext};
use types_error::PgResult;
use types_portal::params::ParamExecData;
use types_nodes::primnodes::ParamKind;
use types_nodes::Node;
use types_slot::{SlotData, TupleSlotKind};

use crate::ops::window::object::{abi, bad, Rt, WinSpec};

/// D1232: every argument and FILTER cell's ExprState over the stored row's child columns as the OUTER slot, made with
/// the object as ExecInitWindowAgg makes its argstates / filters (its EXECUTE checks before any row). Fields drop in
/// order: the context last.
pub(crate) struct Lazy {
    states: Vec<Option<PgBox<'static, ExprState<'static>>>>,
    slot: SlotData<'static>,
    /// The executor parameters' values by paramid (the states' `ParamBind::exec_vals`, address-stable for their life),
    /// each NULL until `m10_win_params` writes it.
    exec: Box<[Cell<ParamExecData>]>,
    /// The PARAM_EXEC parameters the cells read.
    reads: Vec<i32>,
    /// W3: the InitPlan parameters' Init cells (`m10_win_params`' `PARAM_INIT` records), by paramid: each pending until
    /// its first read runs the Init.
    inits: Vec<(i32, u64)>,
    /// The spec's InitPlan parameter read (`WinSpec::init_param`).
    init_param: fn(u64, i32) -> PgResult<(u64, u8)>,
    /// The by-reference parameter values' copies, kept until the next `m10_win_params`.
    keep: MemoryContext,
    cx: PinnedContext,
}

impl Lazy {
    /// The window's evaluators when the IR left every argument cell to the object (`spec.lazy`), else `None`.
    pub(crate) fn of(spec: &WinSpec, root: &MemoryContext) -> PgResult<Option<Lazy>> {
        if !spec.lazy {
            return Ok(None);
        }
        let cx = PinnedContext::new(root.new_child("gravity window lazy arguments"));
        // every state and the slot allocated in it drop before `cx` (field order)
        let mcx = crate::ops::window::raw::lazy_mcx(&cx);
        let child = spec.plan.plan.lefttree.and_then(|c| c.as_plan()).ok_or_else(|| bad("window.agg: a WindowAgg without its child"))?;
        let desc = execscan::exec_type_from_tl(mcx, &child.targetlist)?;
        let slot = exectuples::make_tuple_table_slot(mcx, TupleSlotKind::Virtual, Some(desc));
        let mut reads: Vec<i32> = Vec::new(); // alloc: Exec
        // bounded("other: the lazy argument cells of the node's window functions, one expression walk each at bind: the query text bounds them, as PG's ExecInitWindowAgg initializes the same arguments with no poll")
        for e in (0..spec.cols.len()).filter(|&c| lazy_cell(spec, c)).filter_map(|c| expr_of(spec, c)) {
            gravity_decompose::expr::walk(e, &mut |x| {
                if let Some(p) = x.as_param().filter(|p| p.paramkind == ParamKind::PARAM_EXEC && !reads.contains(&p.paramid)) {
                    reads.push(p.paramid); // alloc: Exec
                }
                true
            });
        }
        if reads.iter().any(|&id| id < 0 || id as usize >= spec.n_exec) {
            return Err(bad("window.agg: a lazy argument's executor parameter outside the statement's"));
        }
        let exec: Box<[Cell<ParamExecData>]> = (0..spec.n_exec).map(|_| Cell::new(ParamExecData::EMPTY)).collect(); // alloc: Exec
        // a Cell is its value's layout (repr(transparent)): the states read the cells' values in place
        let exec_vals = core::ptr::NonNull::new(exec.as_ptr() as *mut ParamExecData).filter(|_| !exec.is_empty());
        let mut states = Vec::with_capacity(spec.cols.len()); // alloc: Exec
        // bounded("other: the node's stored cells (its child's columns and its window functions' arguments), one expression state each at bind: the query text bounds them, as PG's ExecInitWindowAgg initializes the same arguments with no poll")
        for c in 0..spec.cols.len() {
            // D3584: a state for each lazy cell only (an eager one may hold a SubPlan the fill evaluates)
            let st = match expr_of(spec, c).filter(|_| lazy_cell(spec, c)) {
                Some(e) => {
                    // D1355: `$n` from the execution's list, copied at prepare; W3: PARAM_EXEC from `exec`.
                    // `spec.params` outlives the states (raw.rs `spec_params`); `exec` is a boxed slice the Lazy keeps,
                    // never resized.
                    let params = ParamBind { extern_params: Some(crate::ops::window::raw::spec_params(spec)), exec_vals, n_exec: exec.len() as u32, extern_fetch: spec.fetch, ..ParamBind::NONE };
                    let Some(mut st) = execexpr::exec_init_expr(mcx, Some(e), params)? else { return Err(bad("window.agg: a lazy argument the executor did not initialize")) };
                    st.arm_result_mcx(mcx);
                    Some(st)
                }
                None => None,
            };
            states.push(st); // alloc: Exec
        }
        let keep = root.new_child_wholesale("gravity window lazy parameters");
        Ok(Some(Lazy { states, slot, exec, reads, inits: Vec::new(), init_param: spec.init_param, keep, cx })) // alloc: Exec
    }

    /// `m10_win_params`: each record of `recs` (`raw::PARAM_WORDS` words: paramid, datum, NULL, `typlen | byval << 16`)
    /// becomes its parameter's value, a by-reference one copied (the previous call's copies dropped); a `PARAM_INIT`
    /// record's parameter is pending, its datum word the Init cell its first read runs.
    pub(crate) fn set_params(&mut self, recs: &[u64]) -> PgResult<()> {
        self.keep.reset();
        self.inits.clear();
        // bounded("other: one record per executor parameter the node's lazy arguments read, set once per open: the query text bounds them")
        for r in recs.chunks_exact(crate::ops::window::raw::PARAM_WORDS) {
            let (id, w, null, (typlen, byval)) = (r[0] as i64, r[1], r[2] != 0, (r[3] as u16 as i16, r[3] >> 16 & 1 == 1));
            let Some(cell) = self.reads.iter().any(|&x| x as i64 == id).then(|| &self.exec[id as usize]) else {
                return Err(bad("m10_win_params: a parameter no lazy argument reads"));
            };
            if r[3] & abi::PARAM_INIT != 0 {
                if w == 0 {
                    return Err(bad("m10_win_params: an InitPlan parameter without its Init cell"));
                }
                self.inits.push((id as i32, w)); // alloc: Exec
                cell.set(ParamExecData { value: Datum::null(), isnull: true, exec_plan: true });
                continue;
            }
            let v = match (null, byval) {
                (true, _) => Datum::null(),
                (false, true) => Datum::from_u64(w),
                (false, false) => crate::ops::window::raw::copy_param(&self.keep, Datum::from_u64(w), typlen)?,
            };
            cell.set(ParamExecData { value: v, isnull: null, exec_plan: false });
        }
        Ok(())
    }
}

/// W3: ExecSetParamPlan for pending InitPlan parameter `id`: its Init cell (`inits`) runs the InitPlan once for the
/// execution (or ran it already, `read`: the spec's `init_param`), then its pair is the parameter's value in `exec`; the
/// run's error (a second row: 21000; a raise inside it) is the read's.
fn fetch(exec: &[Cell<ParamExecData>], inits: &[(i32, u64)], read: fn(u64, i32) -> PgResult<(u64, u8)>, id: u32) -> PgResult<()> {
    let Some(&(_, init)) = inits.iter().find(|&&(p, _)| p as u32 == id) else { return Err(bad("window.agg: a pending executor parameter no InitPlan record names")) };
    let Some(cell) = exec.get(id as usize) else { return Err(bad("window.agg: an InitPlan parameter outside the statement's")) };
    let (w, null) = read(init, id as i32)?;
    cell.set(ParamExecData { value: if null != 0 { Datum::null() } else { Datum::from_u64(w) }, isnull: null != 0, exec_plan: false });
    Ok(())
}

/// D3584: stored cell `c` is one the IR stores `CELL_LAZY` (`WinSpec::lazy_cells`).
fn lazy_cell(spec: &WinSpec, c: usize) -> bool {
    spec.lazy_cells.get(c).is_some_and(|&l| l)
}

/// Stored cell `c`'s expression: a function's argument or its FILTER.
fn expr_of(spec: &WinSpec, c: usize) -> Option<Node<'static>> {
    spec.funcs.iter().zip(spec.args.iter()).find_map(|(f, &(base, n, filter))| match c {
        _ if (base..base + n).contains(&c) => f.args.iter().nth(c - base),
        _ if filter == Some(c) => f.aggfilter,
        _ => None,
    })
}

impl Rt {
    /// D1232: lazy cell `c` of stored row `r` evaluated now (PG's ExecEvalExpr at each WinGetFuncArg* / aggregate
    /// advance), its by-reference value in the row's context.
    #[cold]
    pub(crate) fn eval_lazy(&mut self, r: u64, c: usize) -> PgResult<NullableDatum> {
        let at = {
            let s = self.store.borrow();
            s.held(r)?;
            s.at(r)
        };
        let (natts, ncols) = (self.spec.natts, self.spec.cols.len());
        let Rt { lazy, row_cx, .. } = self;
        let Some(lz) = lazy.as_mut() else { return Err(bad("window.agg: a lazy cell in a window with no lazy argument")) };
        // the states and the slot live in `lz.cx`, dropped before it
        let mcx = crate::ops::window::raw::lazy_mcx(&lz.cx);
        exectuples::exec_clear_tuple(&mut lz.slot, mcx);
        {
            let base = lz.slot.base_mut();
            // bounded("key-columns: one cell per column of the child's row, at most MaxTupleAttributeNumber (1664) columns")
            for k in 0..natts.min(base.tts_values.len()) {
                // `r` is a stored row and `k` one of its child cells
                let (v, nb) = crate::ops::window::raw::cell_at(at, ncols, k);
                base.tts_values[k] = Datum::from_u64(v);
                base.tts_isnull[k] = nb != 0;
            }
        }
        exectuples::exec_store_virtual_tuple(&mut lz.slot);
        let Some(st) = lz.states.get_mut(c).and_then(|x| x.as_mut()) else { return Err(bad("window.agg: a lazy cell that is no argument")) };
        // the value is read within the row, before `row_cx`'s next reset; the state's own context is restored below
        crate::ops::window::raw::arm_row_result(st, row_cx.mcx());
        // W3: a pending InitPlan parameter's fetch suspends; its Init runs (once per execution), the fetch runs again
        let mut resume = None;
        // bounded("other: one pass per pending InitPlan parameter the expression reads: a fetch sets it for the execution (`fetch`), so the passes are at most the expression's executor parameters plus one, which the query text bounds")
        let v = loop {
            match execexpr::exec_eval_expr_outcome(st, &mut EvalSlots { scan: None, inner: None, outer: Some(&mut lz.slot) }, resume.take()) {
                Ok(EvalOutcome::Done(v)) => break Ok(v),
                Ok(EvalOutcome::Suspended(s)) => match s.kind {
                    SuspendKind::ParamExec(id) => match fetch(&lz.exec, &lz.inits, lz.init_param, id) {
                        Ok(()) => resume = Some(s.resume_with(NullableDatum::null())),
                        Err(e) => break Err(e),
                    },
                    SuspendKind::SubPlan(_) => break Err(bad("window.agg: a lazy argument read a SubPlan")),
                },
                Err(e) => break Err(e),
            }
        };
        st.arm_result_mcx(mcx);
        exectuples::exec_clear_tuple(&mut lz.slot, mcx);
        v
    }
}
