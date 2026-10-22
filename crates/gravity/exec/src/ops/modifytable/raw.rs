//! modifytable's raw half in exec (rule R6: the family's unsafe code lives here): write.literals' stream form (CAP-29).
//! A VALUES list whose rows holding an expression pass the per-row code's capacity (`cost::VALUES_CODED_ROWS_MAX`)
//! streams those rows as PostgreSQL's ValuesNext does: at a coded row's turn the program calls `eval` (through protos
//! `LiteralsStream`, the table's `LIT_STREAM` word), which initializes the row's expressions in a per-row context reset
//! at the next such call, evaluates each cell that is not a Const in column order (`$n` from the execution's list, as
//! the window's lazy cells read it) and writes it into the table's row, whose Const cells prepare already filled. The
//! first failing cell's error is the row's, raised in the row's turn; a row no read reaches is never evaluated. D3173:
//! the program reads the table's cells as Consts, alive for the execution (an out batch, a sort run hold them by
//! reference past the next row), so a by-reference value is copied out of the row context into the stream's kept one.
#![allow(unsafe_code)]
use execexpr::EvalSlots;
use gravity_program::protos::LiteralsStream;
use mcx::{Mcx, MemoryContext};
use types_error::PgResult;
use types_nodes::{Node, NodeList};
use datum::Datum;
use types_portal::params::{ParamBind, ParamExecData, ParamExternData, ParamFetchHook};

use gravity_decompose::catalog::expr_type;

use crate::glue;

/// One execution's stream: the plan's rows, the table's arrays it writes, the execution's `$n` and the row context.
pub(crate) struct Stream {
    /// The hook the program calls; `cx` is this stream's address (boxed, never moved while the execution runs).
    hook: LiteralsStream,
    rows: &'static [Node<'static>],
    cols: usize,
    cells: *mut u64,
    nulls: *mut u8,
    params: memledger::ChargedVec<ParamExternData>,
    /// D2834: the list owner's fetch for a slot it left to evaluation time (taken where the cell is evaluated).
    fetch: Option<ParamFetchHook>,
    row: MemoryContext,
    /// Each column's `(typlen, typbyval)`, and the execution-lived context a by-reference value is copied into.
    lens: Box<[(i16, bool)]>,
    kept: MemoryContext,
    /// D3374 (result_cap29): the executor parameters the streamed cells read (lowerv4 `values::exec_params`, ascending
    /// paramid), their slots the program writes before each call (two words each: the datum, then 1 for NULL; protos
    /// `LiteralsStream::params`) and the `es_param_exec_vals` array the evaluation reads them from.
    xids: memledger::ChargedVec<u32>,
    slots: Box<[u64]>,
    exec: Box<[ParamExecData]>,
}

impl Stream {
    /// The stream over `lists` (the ValuesScan's) writing into `cells` / `nulls` (the table's arrays, kept beside it by the
    /// protocol's Live and never resized).
    pub(crate) fn new(lists: &NodeList<'static>, cols: usize, cells: &mut [u64], nulls: &mut [u8], params: memledger::ChargedVec<ParamExternData>, fetch: Option<ParamFetchHook>, mcx: Mcx<'static>, owner: Option<(memledger::Owner, memledger::Kind)>) -> PgResult<Box<Stream>> {
        if cells.len() != lists.len() * cols || nulls.len() != cells.len() {
            return Err(glue::internal("gravity: write.literals' stream over arrays that are not its table's"));
        }
        // SAFETY: the admitted plan outlives the execution, and the stream lives in the execution's Live (as the table's
        // Const cells point into the same plan for as long).
        let rows: &'static [Node<'static>] = unsafe { &*(lists.as_slice() as *const [Node<'static>]) };
        // D2943: reset wholesale per row (ValuesNext's ReScanExprContext) with the last row's values still in it
        let row = mcx.context().new_child_wholesale("gravity VALUES stream row");
        let kept = mcx.context().new_child_wholesale("gravity VALUES stream values");
        // a VALUES column has one type (the parser coerced every row's cell to it): the first row's cells name them
        let first = rows.first().and_then(|r| r.as_list()).filter(|l| l.len() == cols).ok_or_else(|| glue::internal("gravity: a write.literals stream row is not the table's width"))?;
        let lens = first.iter().map(|e| gravity_fn::typesem::storage::len_byval(expr_type::type_of(e).unwrap_or_else(|| expr_type::result_type(e)))).collect::<PgResult<Box<[_]>>>()?; // alloc: Exec
        let (o, k) = owner.map_or((None, memledger::Kind::Region), |(o, k)| (Some(o), k));
        let mut xids = memledger::ChargedVec::<u32>::new(o, k);
        xids.with_vec(|v| v.extend(gravity_lowerv4::ops::shared::values::exec_params(lists).iter().filter_map(|n| n.as_param()).map(|p| p.paramid as u32))); // alloc: Exec charged o
        let slots = vec![0u64; 2 * xids.len()].into_boxed_slice(); // alloc: Exec
        let exec = vec![ParamExecData::EMPTY; xids.last().map_or(0, |&id| id as usize + 1)].into_boxed_slice(); // alloc: Exec
        let mut s = Box::new(Stream { hook: LiteralsStream { eval, cx: 0, params: 0 }, rows, cols, cells: cells.as_mut_ptr(), nulls: nulls.as_mut_ptr(), params, fetch, row, lens, kept, xids, slots, exec }); // alloc: Exec
        s.hook.cx = &*s as *const Stream as u64;
        // the slots are boxed: their address stays put while the execution runs
        s.hook.params = if s.slots.is_empty() { 0 } else { s.slots.as_ptr() as u64 };
        Ok(s)
    }

    /// The `LIT_STREAM` word: the hook's address.
    pub(crate) fn hook(&self) -> u64 {
        &self.hook as *const LiteralsStream as u64
    }

    /// Row `j`'s cells that are not a Const, evaluated now into the table (ValuesNext: ReScanExprContext, the row's
    /// ExecInitExprList in the per-tuple context, ExecEvalExpr per cell in column order).
    fn row(&mut self, j: usize) -> PgResult<()> {
        let Some(list) = self.rows.get(j).and_then(|r| r.as_list()) else {
            return Err(glue::internal("gravity: write.literals' stream past its list"));
        };
        if list.len() != self.cols {
            return Err(glue::internal("gravity: a write.literals stream row is not the table's width"));
        }
        self.row.reset();
        let (cells, nulls, base) = (self.cells, self.nulls, j * self.cols);
        let mcx = self.row.mcx();
        // D3374: the executor parameters' values the program wrote for this row's turn (its outer row's, its InitPlan's)
        for (i, &id) in self.xids.iter().enumerate() {
            self.exec[id as usize] = ParamExecData { value: Datum::from_u64(self.slots[2 * i]), isnull: self.slots[2 * i + 1] != 0, exec_plan: false };
        }
        let (n_exec, exec_vals) = (self.exec.len() as u32, core::ptr::NonNull::new(self.exec.as_mut_ptr()).filter(|_| !self.exec.is_empty()));
        let ext: &[ParamExternData] = &self.params;
        for (c, e) in list.iter().enumerate().filter(|(_, e)| e.as_const().is_none()) {
            let params = ParamBind { extern_params: Some(ext), extern_fetch: self.fetch, exec_vals, n_exec, ..ParamBind::NONE };
            let Some(mut st) = execexpr::exec_init_expr(mcx, Some(e), params)? else {
                return Err(glue::internal("gravity: a write.literals stream cell the executor did not initialize"));
            };
            st.arm_result_mcx(mcx);
            let v = execexpr::exec_eval_expr(&mut st, &mut EvalSlots { scan: None, inner: None, outer: None })?;
            let (len, byval) = self.lens[c];
            let value = match v.isnull || byval {
                true => v.value,
                // SAFETY: a non-NULL by-reference result of the row's evaluation, live in the row context until its reset.
                false => unsafe { execexpr::agg_datum_copy(self.kept.mcx(), v.value, len)? },
            };
            // SAFETY: `base + c` is inside the table (checked width, `j` a row of the list); the arrays outlive the stream
            // (the Live's fields drop the stream first) and the program reads row `j` only after this call returns.
            unsafe {
                *cells.add(base + c) = value.as_u64();
                let nb = nulls.add(base + c);
                *nb = (*nb & !1) | u8::from(v.isnull);
            }
        }
        Ok(())
    }
}

/// `LiteralsStream::eval`: row `j` of the stream at `cx` (`Stream::row`); 1 its cells written, 0 an error (stashed for the
/// raise). Called only through the VALUES source's host entry (lowerv4 literals/raw.rs `stream_row`), under its boundary.
/// # Safety
/// `cx` is the boxed Stream the table's Live keeps for the execution (its hook's own `cx`), called from the thread that runs
/// the execution (the entry is not a worker's).
unsafe fn eval(cx: u64, j: u64) -> i64 {
    // SAFETY: the caller's contract above.
    let s = unsafe { &mut *(cx as usize as *mut Stream) };
    match s.row(j as usize) {
        Ok(()) => 1,
        Err(e) => {
            gravity_fn::stash_error(e);
            0
        }
    }
}
