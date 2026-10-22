//! pglpoint S3 option (a): what a point statement's hook B builds per statement, kept in the S5 memo entry hook A served
//! the statement's plan from (skip.rs `Slot::kept`, `Hit`). The entry's holder is charged for it, its cap counts it, and
//! it is dropped with the entry: when its reads change (`gravity_deps::Held`: the planner's reads and these pieces' own
//! build reads), when a setting moves, or for another role. Kept per entry:
//! - a point write's write host and walk (hosts.rs `Hosts`: their contexts, the walk's parked btree scan and the parked
//!   relation handles), and the walk's recipe (a recheck's walk is made from it);
//! - S1's probe scan, parked (`ProbeKept`), its scan keys' functions and slot, the out columns, the parked relation handles;
//! - the relation tests' verdict;
//! - an UPDATE's SET values compiled once (`SetKept`).
//! Per statement, as PostgreSQL's ExecutorStart does, nothing kept:
//! - the permission checks and the snapshot;
//! - every lock (the index's where hook A left none, the hosts' at their binds);
//! - the relations re-opened through the parked handles;
//! - the EXECUTE gates of the SET values' and the keys' functions;
//! - the keys' and the SET values' constants.
//! A run that raises drops the pieces (they may be bound); the next statement of the entry builds them again.
//! pgrust.point_keep (default off) turns it on; off, nothing is kept and a point UPDATE declines (update.rs).
use std::rc::Weak;

use execexpr::{EvalSlots, ExprState, ParamBind};
use gravity_hosts::ops::shared::rel::{park, repin};
use gravity_hosts::{NodeScratch, SiteRecipe};
use gravity_program::{Cmd, OutCol};
use mcx::{PgBox, PgVec};
use types_core::Oid;
use types_error::{PgError, PgResult};
use types_nodes::list::NodeList;
use types_nodes::primnodes::{Const, Param, ParamKind};
use types_nodes::{Node, NodeTag};
use types_portal::params::ParamExecData;
use types_rel::{NoLock, RelationData};
use types_slot::SlotData;

use super::hosts::{Hosts, NewRow};
use super::{out, probe, select};

mcx::bind!(pub(super) KeptTy => KeptIn<'k>);

/// A memo entry's pieces in their own context, charged to the memo's holder (`boxed`): the slot holds two pointers, and a
/// statement takes and returns them without copying the pieces (the hosts are kilobytes: a copy per move cost more
/// than the rebuild they save).
pub(super) struct KeptIn<'k> {
    pub kept: Kept<'k>,
}

pub(super) type KeptBox = mcx::McxOwned<KeptTy>;

/// `k` into its own context, its owner `owner` (the memo's holder): the pieces one allocation of their own size (a
/// SELECT's do not take a write's room), in a small first block, not an 8 KiB one.
pub(super) fn boxed(k: Made, owner: memledger::Owner) -> PgResult<KeptBox> {
    let ctx = mcx::MemoryContext::new_start_small("GravityPointKept");
    ctx.set_owner(owner);
    mcx::McxOwned::<KeptTy>::try_new(ctx, move |m| {
        let kept = match k {
            Made::Write(w) => Kept::Write(put_in(m, w)?),
            Made::Probe(p) => Kept::Probe(put_in(m, p)?),
        };
        Ok(KeptIn { kept })
    })
}

/// `v` moved into context `m`.
fn put_in<'k, T>(m: mcx::Mcx<'k>, v: T) -> PgResult<PgBox<'k, T>> {
    PgBox::try_new_in(v, m).map_err(|_| m.oom_boxed(core::mem::size_of::<T>()))
}

/// The table and index of a write's kept pieces of command `cmd` (`None`: none, or another kind's).
pub(super) fn write_ids(kb: &KeptBox, cmd: Cmd) -> Option<(Oid, Oid)> {
    kb.with(|k| match &k.kept {
        Kept::Write(w) if w.cmd == cmd => Some((w.relid, w.indexid)),
        _ => None,
    })
}

/// The table and index of S1's kept pieces (`None`: none, or a write's).
pub(super) fn probe_ids(kb: &KeptBox) -> Option<(Oid, Oid)> {
    kb.with(|k| match &k.kept {
        Kept::Probe(p) => Some((p.relid, p.indexid)),
        Kept::Write(_) => None,
    })
}

/// The pieces' bytes: their own context's footprint and what they hold (`Kept::bytes`: their contexts' footprints).
pub(super) fn bytes(kb: &KeptBox) -> usize {
    kb.context().mem_allocated() + kb.with(|k| k.kept.bytes())
}

/// `bytes` by piece, for REPORT's 'keep bytes' line (roots as node / exec / bind).
#[cold]
pub(super) fn parts(kb: &KeptBox) -> String {
    let own = kb.context().mem_allocated();
    kb.with(|k| match &k.kept {
        Kept::Write(w) => format!("own {own} (WriteKept {} B); {}; set {:?}", core::mem::size_of::<WriteKept>(), w.hosts.parts(), w.set.as_ref().map(|s| s.scratch.used_parts())), // alloc: Cold
        Kept::Probe(p) => format!("own {own} (ProbeKept {} B); probe {:?}", core::mem::size_of::<ProbeKept>(), p.scratch.used_parts()), // alloc: Cold
    })
}

/// What a memo entry keeps for hook B, made at the first statement S5 served from the entry, each kind in its own
/// allocation in the pieces' context (`boxed`).
pub(super) enum Kept<'k> {
    Write(PgBox<'k, WriteKept>),
    Probe(PgBox<'k, ProbeKept>),
}

/// The pieces as a statement made them, before `boxed` keeps them.
pub(super) enum Made {
    Write(WriteKept),
    Probe(ProbeKept),
}

impl Kept<'_> {
    pub(super) fn bytes(&self) -> usize {
        match self {
            Kept::Write(w) => w.hosts.bytes() + w.set.as_ref().map_or(0, SetKept::bytes),
            Kept::Probe(p) => p.scratch.used_parts().iter().sum(),
        }
    }

    /// After a run that returned: what it homed on the statement back on the memo's holder (`to`), its values freed.
    pub(super) fn settle(&mut self, to: memledger::Owner) {
        match self {
            Kept::Write(w) => w.settle(to),
            Kept::Probe(p) => {
                p.scratch.reset_exec();
                p.scratch.home(to);
            }
        }
    }
}

/// A point DELETE's or UPDATE's kept pieces: its table and index (the plan's, checked against each statement's), the
/// walk's recipe, the hosts and an UPDATE's SET values. The relation tests passed when they were built.
pub(super) struct WriteKept {
    pub cmd: Cmd,
    pub relid: Oid,
    pub indexid: Oid,
    /// The recheck walk's recipe until its first recheck moves it into the hosts' EPQ walk (`hosts::EpqRecipe::Kept`).
    pub recipe: core::cell::Cell<Option<SiteRecipe>>,
    pub hosts: Hosts,
    pub set: Option<SetKept>,
}

impl WriteKept {
    fn settle(&mut self, to: memledger::Owner) {
        self.hosts.home(to);
        if let Some(s) = self.set.as_mut() {
            s.settle(to);
        }
    }
}

/// S1's kept pieces (a point SELECT's): its table and index, the index's key properties (`probe::index_refusal`'s), the
/// parked relation handles, the out columns and the row's cells, and the scan (made at the first run, parked between
/// statements: `probe::Scan::park` / `rearm`). The relation tests passed when they were made.
pub(super) struct ProbeKept {
    pub relid: Oid,
    pub indexid: Oid,
    ops: probe::KeyOps,
    heap: Option<Weak<RelationData<'static>>>,
    index: Option<Weak<RelationData<'static>>>,
    cols: PgVec<'static, OutCol>,
    values: PgVec<'static, u64>,
    isnull: PgVec<'static, u8>,
    scan: Option<probe::Scan<'static>>,
    /// Declared last: everything above lives in its contexts (node: the build and the scan; exec: one statement's), and
    /// drops first.
    scratch: NodeScratch,
}

impl ProbeKept {
    /// S1's opens (the table with the parser's lock, the index as ExecInitIndexScan, `select::index_lockmode`) and tests
    /// (`select::relation_refusal`: `Err` with the reason, nothing kept), then the out columns; the relations parked (the
    /// runs re-open them), the context charged to `owner`.
    pub(super) fn make(s: &select::Shape<'_>, owner: memledger::Owner) -> PgResult<Result<ProbeKept, &'static str>> {
        let mut scratch = {
            let _small = gravity_hosts::scratch::small_roots();
            NodeScratch::new("GravityPointProbe")
        };
        scratch.home(owner);
        let m = scratch.mcx();
        let heap = table::table_open(m, s.relid, NoLock)?;
        let index = match indexam::index_open(m, s.scan.indexid, select::index_lockmode(s)) {
            Ok(i) => i,
            Err(e) => {
                table::table_close(heap, NoLock)?;
                return Err(e);
            }
        };
        let mut ops: probe::KeyOps = [(0, 0); types_core::fmgr::INDEX_MAX_KEYS as usize];
        let why = select::relation_refusal(&heap, &index, s, &[], &mut ops);
        let index = park(Some(index))?;
        let heap = park(Some(heap))?;
        if let Some(why) = why? {
            return Ok(Err(why));
        }
        let tlist = &s.scan.scan.plan.targetlist;
        let n = tlist.len();
        let mut cols: PgVec<'static, OutCol> = PgVec::new_in(m);
        cols.try_reserve_exact(n).map_err(|_| m.oom(n * core::mem::size_of::<OutCol>()))?;
        for t in tlist.iter() {
            cols.push(out::out_col(t.as_target_entry().ok_or_else(|| internal("an out entry that is no target entry"))?)?);
        }
        let mut values: PgVec<'static, u64> = PgVec::new_in(m);
        let mut isnull: PgVec<'static, u8> = PgVec::new_in(m);
        values.try_reserve_exact(n).map_err(|_| m.oom(n * 8))?;
        isnull.try_reserve_exact(n).map_err(|_| m.oom(n))?;
        Ok(Ok(ProbeKept { relid: s.relid, indexid: s.scan.indexid, ops, heap, index, cols, values, isnull, scan: None, scratch }))
    }

    /// S1's run over the kept pieces (`select::probe_rows`' order, inside ExecutorStart's checks and the snapshot): the
    /// keys' EXECUTE gates, the RowDescription, the relations re-opened through the parked handles, the scan made (the
    /// first run) or re-armed with this statement's keys, a DataRow per row, the scan parked and the relations parked again.
    /// Returns the rows sent.
    pub(super) fn run(&mut self, s: &select::Shape<'_>, mcx: mcx::Mcx<'_>, wire: &mut gravity_wire::WireNode) -> PgResult<u64> {
        probe::key_gates(&s.scan.indexqual)?;
        wire.prepare_cols(&self.cols, None)?;
        wire.reset();
        wire.send_rowdesc()?;
        let em = self.scratch.exec_mcx();
        let heap = match repin(&self.heap) {
            Some(h) => h,
            None => table::table_open(em, self.relid, NoLock)?,
        };
        let index = match repin(&self.index) {
            Some(i) => Ok(i),
            None => indexam::index_open(em, self.indexid, NoLock),
        };
        let r = index.and_then(|index| {
            let r = self.walk(s, mcx, wire, &heap, &index);
            park(Some(index)).map(|p| self.index = p).and(r)
        });
        let parked = park(Some(heap)).map(|p| self.heap = p);
        let rows = r?;
        parked?;
        Ok(rows)
    }

    fn walk(&mut self, s: &select::Shape<'_>, mcx: mcx::Mcx<'_>, wire: &mut gravity_wire::WireNode, heap: &types_rel::Relation<'static>, index: &types_rel::Relation<'static>) -> PgResult<u64> {
        let (m, em) = (self.scratch.mcx(), self.scratch.exec_mcx());
        let indexqual = &s.scan.indexqual;
        let snap = snapmgr::GetActiveSnapshot();
        let scan = match self.scan.as_mut() {
            Some(scan) => {
                scan.rearm(heap, index, indexqual, &[], snap)?;
                scan
            }
            None => self.scan.insert(probe::Scan::begin(m, heap, index, indexqual, &[], &self.ops, snap)?),
        };
        let tlist = &s.scan.scan.plan.targetlist;
        let mut rows = 0u64;
        let r = scan.rescan().and_then(|()| loop {
            if !scan.next(em)? {
                return Ok(());
            }
            select::project(tlist, &mut scan.slot, &mut self.values, &mut self.isnull);
            wire.send_row_in(&self.values, &self.isnull, mcx)?;
            rows += 1;
        });
        let parked = scan.park(em);
        r.and(parked).map(|()| rows)
    }
}

/// The kept compile takes a SET value made of these nodes alone: execexpr initializes each over its arguments, whatever
/// they are, so a constant read through a parameter is evaluated as the constant is. Anything else (an array or row
/// constructor, a subscript, a hashed IN list, a JSON or XML node) compiles per statement, as without the memo.
fn kept_node(tag: NodeTag) -> bool {
    matches!(
        tag,
        NodeTag::T_Var
            | NodeTag::T_Const
            | NodeTag::T_OpExpr
            | NodeTag::T_FuncExpr
            | NodeTag::T_DistinctExpr
            | NodeTag::T_NullIfExpr
            | NodeTag::T_RelabelType
            | NodeTag::T_CoerceViaIO
            | NodeTag::T_BoolExpr
            | NodeTag::T_NullTest
            | NodeTag::T_BooleanTest
            | NodeTag::T_CaseExpr
            | NodeTag::T_CaseWhen
            | NodeTag::T_CaseTestExpr
            | NodeTag::T_CoalesceExpr
            | NodeTag::T_MinMaxExpr
    )
}

/// Every node of `n` is one `kept_node` takes, and no constant is an untyped literal (`unknown_literal`).
pub(super) fn keeps(n: Node<'_>) -> bool {
    stack_depth_core::check_stack_depth_or_panic();
    let mut ok = kept_node(n.node_tag()) && !n.as_const().is_some_and(unknown_literal);
    let known = gravity_decompose::expr::kids(n, &mut |c| ok = ok && keeps(c));
    ok && known
}

/// D4824: an unknown-typed constant (an untyped literal the parser left as it was: an argument of a VARIADIC "any" call,
/// `json_build_object('x', 1)`) is the one argument `get_fn_expr_arg_stable` reports stable, and the callee reads it as
/// text only then (extract_variadic_args' convert_unknown). As the kept compile's PARAM_EXEC parameter (`paramize`) it is
/// no stable argument, so the call would raise 'could not determine data type for argument 1' where PostgreSQL returns
/// the value. A SET list with one is not kept: the UPDATE is Gravity's, as without the memo.
fn unknown_literal(c: &Const) -> bool {
    c.consttype == types_core::catalog::UNKNOWNOID
}

/// The constants of `n`, in one fixed order (`kids`' pre-order): the parameter numbering of the kept compile and the
/// order a statement's constants are written in.
fn each_const<'p>(n: Node<'p>, f: &mut dyn FnMut(&'p Const)) {
    stack_depth_core::check_stack_depth_or_panic();
    if let Some(c) = n.as_const() {
        f(c);
        return;
    }
    gravity_decompose::expr::kids(n, &mut |c| each_const(c, &mut *f));
}

/// `n` (a copy in the kept context) with each constant replaced by a PARAM_EXEC parameter of its type, typmod and
/// collation, numbered by its position in `at` (the constants' addresses in `each_const` order). A PARAM_EXEC parameter
/// is no stable argument to a function (get_fn_expr_arg_stable): no function caches on its value as on a constant's.
fn paramize<'m>(m: mcx::Mcx<'m>, n: Node<'m>, at: &[usize]) -> PgResult<Option<Node<'m>>> {
    stack_depth_core::check_stack_depth()?;
    if let Some(c) = n.as_const() {
        let me = c as *const Const as usize;
        let id = at.iter().position(|&a| a == me).ok_or_else(|| internal("a SET constant outside the kept walk"))?;
        let p = Param { paramkind: ParamKind::PARAM_EXEC, paramid: id as i32, paramtype: c.consttype, paramtypmod: c.consttypmod, paramcollid: c.constcollid, location: c.location };
        return Ok(Some(Node::mk(m, p)?));
    }
    nodes_core::expression_tree_mutator(m, n, &mut |c| paramize(m, c, at))
}

/// An UPDATE's SET values compiled once for the entry, as ExecInitUpdateProjection over the scan's target list, each
/// constant a PARAM_EXEC parameter (`params`, address-stable: the steps read it by pointer) the statement's constant is
/// written into (`bind`); the functions whose EXECUTE privilege the compile checked (`gates`, compile order with repeats:
/// execexpr's recording window, re-run at each statement as a parked executor's reuse re-runs them); the new row's cells
/// and the table's slot (ExecInitUpdateProjection's virtual slot of the table's). The values a statement computes live in
/// the scratch's exec context, reset after the statement.
pub(super) struct SetKept {
    cols: PgVec<'static, i16>,
    targets: PgVec<'static, i16>,
    gates: PgVec<'static, Oid>,
    params: PgVec<'static, ParamExecData>,
    states: PgVec<'static, PgBox<'static, ExprState<'static>>>,
    old: PgVec<'static, u64>,
    old_null: PgVec<'static, u8>,
    row: PgVec<'static, u64>,
    row_null: PgVec<'static, u8>,
    slot: SlotData<'static>,
    /// Declared last: everything above lives in its contexts, and drops first.
    scratch: NodeScratch,
}

impl SetKept {
    /// The kept compile of the plan's SET values (`tlist`'s first `targets.len()` entries, each one `keeps` takes) over
    /// table `relid` (`cols`: the old row's columns), its context charged to `owner`.
    pub(super) fn build(relid: Oid, cols: &[i16], targets: &[i16], tlist: &NodeList<'_>, owner: memledger::Owner) -> PgResult<SetKept> {
        let mut scratch = {
            let _small = gravity_hosts::scratch::small_roots();
            NodeScratch::new("GravityPointSet")
        };
        scratch.home(owner);
        let m = scratch.mcx();
        let heap = table::table_open(m, relid, NoLock)?;
        let natts = heap.rd_att.attrs.len();
        let slot = tableam::table_slot_create(m, &heap);
        table::table_close(heap, NoLock)?;
        let slot = slot?;
        let mut exprs: PgVec<'_, Node<'_>> = PgVec::new_in(m);
        let mut at: PgVec<'_, usize> = PgVec::new_in(m);
        exprs.try_reserve_exact(targets.len()).map_err(|_| m.oom(targets.len() * 8))?;
        for t in tlist.iter().take(targets.len()) {
            let te = t.as_target_entry().ok_or_else(|| internal("a SET entry that is no target entry"))?;
            let copy = copyfuncs::copy_object(m, te.expr)?;
            let mut n = 0usize;
            each_const(copy, &mut |_| n += 1);
            at.try_reserve(n).map_err(|_| m.oom(n * 8))?;
            each_const(copy, &mut |c| at.push(c as *const Const as usize));
            exprs.push(paramize(m, copy, &at)?.unwrap_or(copy));
        }
        let mut params: PgVec<'static, ParamExecData> = PgVec::new_in(m);
        params.try_reserve_exact(at.len()).map_err(|_| m.oom(at.len() * core::mem::size_of::<ParamExecData>()))?;
        for _ in 0..at.len() {
            params.push(ParamExecData::EMPTY);
        }
        let bind = ParamBind { exec_vals: core::ptr::NonNull::new(params.as_mut_ptr()), n_exec: params.len() as u32, ..ParamBind::NONE };
        let mut states: PgVec<'static, PgBox<'static, ExprState<'static>>> = PgVec::new_in(m);
        states.try_reserve_exact(exprs.len()).map_err(|_| m.oom(exprs.len() * 8))?;
        let window = execexpr::execute_acl_window();
        for &e in exprs.iter() {
            nodes_core::fix_opfuncids(e)?;
            states.push(execexpr::exec_init_expr(m, Some(e), bind)?.ok_or_else(|| internal("a SET value with no expression"))?);
        }
        let checked = window.finish();
        let gates = mcx::slice_in(m, &checked)?;
        let cells = |n: usize, v: u64| -> PgResult<PgVec<'static, u64>> {
            let mut x: PgVec<'static, u64> = PgVec::new_in(m);
            x.try_reserve_exact(n).map_err(|_| m.oom(n * 8))?;
            x.resize(n, v);
            Ok(x)
        };
        let flags = |n: usize| -> PgResult<PgVec<'static, u8>> {
            let mut x: PgVec<'static, u8> = PgVec::new_in(m);
            x.try_reserve_exact(n).map_err(|_| m.oom(n))?;
            x.resize(n, 1);
            Ok(x)
        };
        Ok(SetKept {
            cols: mcx::slice_in(m, cols)?,
            targets: mcx::slice_in(m, targets)?,
            gates,
            params,
            states,
            old: cells(cols.len(), 0)?,
            old_null: flags(cols.len())?,
            row: cells(natts, 0)?,
            row_null: flags(natts)?,
            slot,
            scratch,
        })
    }

    fn bytes(&self) -> usize {
        self.scratch.used_parts().iter().sum()
    }

    /// The statement's constants into the parameters, in the kept compile's order (S5 served the plan: its SET values are
    /// the entry's but for their constants, each of the same type and nullness). False when they do not line up.
    pub(super) fn bind(&mut self, tlist: &NodeList<'_>) -> bool {
        let mut k = 0usize;
        let params = &mut self.params;
        for t in tlist.iter().take(self.targets.len()) {
            let Some(te) = t.as_target_entry() else {
                return false;
            };
            each_const(te.expr, &mut |c| {
                if let Some(p) = params.get_mut(k) {
                    *p = ParamExecData { value: c.constvalue, isnull: c.constisnull, exec_plan: false };
                }
                k += 1;
            });
        }
        k == self.params.len()
    }

    /// After the statement: the slot cleared, the values it computed freed, the contexts on the memo's holder.
    fn settle(&mut self, to: memledger::Owner) {
        exectuples::exec_clear_tuple(&mut self.slot, self.scratch.mcx());
        for p in self.params.iter_mut() {
            *p = ParamExecData::EMPTY;
        }
        self.scratch.reset_exec();
        self.scratch.home(to);
    }
}

impl NewRow for SetKept {
    /// ExecInitIndexScan's projection, as a parked executor's reuse: the compile's EXECUTE gates (and execute hooks) again,
    /// under the current role, in compile order.
    fn init(&mut self) -> PgResult<()> {
        execexpr::recheck_execute_acls(self.scratch.exec_mcx(), &self.gates)
    }

    fn old(&mut self) -> (&mut [u64], &mut [u8]) {
        (&mut self.old[..], &mut self.old_null[..])
    }

    /// update.rs `SetRow::form`'s: the old row's cells (a dropped column NULL), each SET value computed over them.
    fn form(&mut self) -> PgResult<()> {
        self.row.fill(0);
        self.row_null.fill(1);
        for (k, &a) in self.cols.iter().enumerate() {
            let i = a as usize - 1;
            self.row[i] = self.old[k];
            self.row_null[i] = self.old_null[k];
        }
        let em = self.scratch.exec_mcx();
        exectuples::exec_clear_tuple(&mut self.slot, em);
        {
            let base = self.slot.base_mut();
            for (i, (&v, &null)) in self.row.iter().zip(self.row_null.iter()).enumerate() {
                base.tts_values[i] = datum::Datum::from_u64(v);
                base.tts_isnull[i] = null != 0;
            }
        }
        exectuples::exec_store_virtual_tuple(&mut self.slot);
        let mut slots = EvalSlots { scan: Some(&mut self.slot), inner: None, outer: None };
        for (st, &a) in self.states.iter_mut().zip(self.targets.iter()) {
            st.arm_result_mcx(em);
            let d = execexpr::exec_eval_expr(st, &mut slots)?;
            let i = a as usize - 1;
            self.row[i] = d.value.as_u64();
            self.row_null[i] = u8::from(d.isnull);
        }
        Ok(())
    }

    fn row(&self) -> (&[u64], &[u8]) {
        (&self.row[..], &self.row_null[..])
    }
}

#[cold]
pub(super) fn internal(msg: &str) -> Box<PgError> {
    Box::new(PgError::error(format!("point path: {msg}")).with_sqlstate(types_error::ERRCODE_INTERNAL_ERROR)) // alloc: Cold
}

#[cfg(test)]
mod tests {
    use super::*;
    use types_nodes::primnodes::{CoercionForm, FuncExpr, Var};

    const INT4: Oid = 23;
    const TEXT: Oid = 25;
    const JSON: Oid = 114;
    /// json_build_object(VARIADIC "any")
    const JSON_BUILD_OBJECT: Oid = 3200;

    fn konst(mcx: mcx::Mcx<'_>, typ: Oid) -> Node<'_> {
        let len = if typ == INT4 { 4 } else if typ == types_core::catalog::UNKNOWNOID { -2 } else { -1 };
        Node::mk(mcx, Const { consttype: typ, consttypmod: -1, constcollid: 0, constlen: len, constvalue: datum::Datum::from_u64(0), constisnull: true, constbyval: typ == INT4, location: -1 }).unwrap()
    }

    fn call<'m>(mcx: mcx::Mcx<'m>, a: Node<'m>, b: Node<'m>) -> Node<'m> {
        let f = FuncExpr { funcid: JSON_BUILD_OBJECT, funcresulttype: JSON, funcretset: false, funcvariadic: false, funcformat: CoercionForm::COERCE_EXPLICIT_CALL, funccollid: 0, inputcollid: 0, args: NodeList::make2(mcx, a, b).unwrap(), location: -1 };
        Node::mk(mcx, f).unwrap()
    }

    /// D4824: `SET payload = json_build_object('x', 1)` keeps 'x' an unknown-typed constant, which the kept compile's
    /// parameter would turn into an error; the SET list is not kept. A typed literal and a column are.
    #[test]
    fn an_unknown_typed_literal_is_not_kept() {
        let ctx = gravity_driver::mem::root::context("PointKeepTest", true);
        let mcx = ctx.mcx();
        let unknown = call(mcx, konst(mcx, types_core::catalog::UNKNOWNOID), konst(mcx, INT4));
        assert!(!keeps(unknown), "an untyped literal under a VARIADIC \"any\" call");
        assert!(!keeps(konst(mcx, types_core::catalog::UNKNOWNOID)), "an untyped literal alone");
        let typed = call(mcx, konst(mcx, TEXT), konst(mcx, INT4));
        assert!(keeps(typed), "typed literals");
        let col = Node::mk(mcx, Var { varno: 1, varattno: 2, vartype: INT4, vartypmod: -1, ..Var::default() }).unwrap();
        assert!(keeps(call(mcx, konst(mcx, TEXT), col)), "a typed literal and a column");
    }
}
