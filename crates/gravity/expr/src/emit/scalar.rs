//! The per-node scalar form (expr-compiler.md §2.2): every value an `I64` word with a `Bool`
//! validity and an `I64` raise mask (abi `status::raise`, 0 = no error); strict operators short-circuit by `select`; a block is
//! opened only around a host call and around the operands that contain one (`guarded`), and (D3233, lowerv4 road
//! `case.select`, `CaseSelect`) around a CASE arm that computes anything, so it runs only on the rows that take it.

use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, HostFlags, IntBinOp, IntCond, StateMask, Type, Value};
use gravity_program::cost::expr::MAX_LIST;
use mcx::{CollectIn, GrowIn, PgVec};
use types_nodes::primnodes::{BoolExprType, BoolTestType, ParamKind};

use super::host::{self, CELL_ARGS, CELL_AUX, CELL_OUT};
use super::jsonb::{self, ShredType};
use super::{prim, ColsRef, EmitError, LaneRef, Lowered, Plan, RowRefs};
use crate::eval::Source;
use crate::percode::{walk_kids, Plan as PerCodePlan};
use crate::nodes::func::{Call, How};
use crate::nodes::rowcmp::{COMPARE_GE, COMPARE_GT, COMPARE_LE, COMPARE_LT};
use crate::nodes::cast::Canon;
use crate::nodes::null::NullTestNode;
use crate::nodes::op::ScalarArrayOp;
use crate::nodes::{ExprNode, Kind, NodeId, Program, SiteId};


leader_tls::leader_tls! {
    thread_local! {
        static CASE_SELECT: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    }
}

/// D3233 (lowerv4 road `case.select`, on by default): while the guard lives (a statement's lowering) a CASE arm that computes
/// anything (not a column, a constant, a parameter or a relabel of one) is evaluated in its own block only on the rows that
/// take it (PostgreSQL's ExecEvalCase jumps to the taken arm), instead of on every row and selected (`case`). The drop
/// restores the outer setting, on an unwind too.
#[must_use = "the setting ends when the guard drops"]
pub struct CaseSelect(bool);

impl CaseSelect {
    pub fn scope(on: bool) -> CaseSelect {
        CaseSelect(CASE_SELECT.with(|c| c.replace(on)))
    }

    pub fn on() -> bool {
        CASE_SELECT.with(|c| c.get())
    }
}

impl Drop for CaseSelect {
    fn drop(&mut self) {
        CASE_SELECT.with(|c| c.set(self.0));
    }
}

/// D3233: a CASE arm a select takes as cheaply as a branch: a column, a constant, a parameter, the CASE's test value or an
/// aggregate's cell, under relabels.
fn trivial(p: &Program<'_>, id: NodeId) -> bool {
    crate::stack_check();
    match &p.nodes[id.0 as usize] {
        ExprNode::Var(_) | ExprNode::Const(_) | ExprNode::Imm(_) | ExprNode::Param(_) | ExprNode::CaseTest | ExprNode::Aggref(_) => true,
        ExprNode::Relabel(r) => trivial(p, r.arg),
        _ => false,
    }
}

/// The row-valued IS [NOT] NULL nodes (`row_null_test`): their site's shim (nodes/field.rs `ROW_NULL_SITE`) reads the
/// backend's typcache, so a consumer's pool check (lowerv4 hir.rs `pool_unsafe_site`) keeps such a program on the leader,
/// though a synthetic shim site is otherwise pool-safe.
pub fn row_tests(p: &Program<'_>) -> usize {
    p.nodes.iter().filter(|n| matches!(n, ExprNode::NullTest(t) if t.row)).count()
}

/// D3542: the rowtype conversions: their site's shim (nodes/row.rs `CONVERT_SITE`) re-reads both rowtypes from the
/// backend's typcache (nodes/tuple.rs `refreshed`, D2803: DDL inside the statement), which a pool thread has not, so the
/// same pool check keeps such a program on the leader.
pub fn row_converts(p: &Program<'_>) -> usize {
    p.nodes.iter().filter(|n| matches!(n, ExprNode::ConvertRowtype(_))).count()
}

/// Whether ScalarArrayOp `n`'s array is walked by the host (`Em::array_walk`) rather than unrolled: any operand but a
/// constant of at most `MAX_LIST` elements (unrolled from its element words, `Plan::elems_of`) and a one-dimension
/// ARRAY[...] of at most `MAX_LIST` elements (unrolled in line) — a column, a parameter, a call's or a subscript's result,
/// a longer ARRAY[...], a longer constant (its whole-array word).
pub fn walked(p: &Program<'_>, n: &ScalarArrayOp) -> bool {
    match &p.nodes[p.kids[p.sites[n.site.0 as usize].args.start as usize + 1].0 as usize] {
        // P5-02: a constant list past MAX_LIST is one word too (emit `plan`), walked like any array: the body of a per-code
        // table's faulting entries (lowerv4 dict/percode.rs `sets_first`); expr.saop claims every other one first
        ExprNode::Const(c) => c.list_len().is_some_and(|k| k > MAX_LIST),
        ExprNode::Array(a) => a.multidims || a.elements.len as usize > MAX_LIST,
        _ => true,
    }
}

/// The kinds `emit` serves, checked before any IR is built (`Compiled::emit_check`); `whole` = the caller
/// supplies `RowRefs::whole` (emit/whole.rs), so a whole-row reference is served.
pub fn check(p: &Program<'_>, id: NodeId, whole: bool) -> Result<(), EmitError> {
    crate::stack_check();
    check_node(p, id, whole)?;
    for k in walk_kids(p, id) {
        check(p, k, whole)?;
    }
    Ok(())
}

/// `id`'s own kind served, its children aside (the builder islands a coverage kind this refuses: nodes/mod.rs). An
/// island root is one host call over the interpreter, its subtree no child of the emit's.
pub fn check_node(p: &Program<'_>, id: NodeId, whole: bool) -> Result<(), EmitError> {
    if p.islands.iter().any(|i| i.root == id) {
        return Ok(());
    }
    let n = &p.nodes[id.0 as usize];
    match n {
        // a system-column (< 0) reference: no lane carries it; a whole-row one (0) only with `whole`
        ExprNode::Var(v) if v.attno < 0 || (v.attno == 0 && !whole) => return Err(EmitError::NoColumn(v.attno)),
        ExprNode::Var(_) | ExprNode::Const(_) | ExprNode::Imm(_) | ExprNode::Relabel(_) | ExprNode::NullTest(_) | ExprNode::BooleanTest(_) | ExprNode::CaseTest | ExprNode::Aggref(_) | ExprNode::WindowFunc(_) | ExprNode::SubPlan(_) | ExprNode::Srf(_) => {}
        ExprNode::Param(q) if q.kind == ParamKind::PARAM_EXTERN || q.kind == ParamKind::PARAM_EXEC || crate::nodes::param::is_deferred(q) => {}
        ExprNode::Func(_) | ExprNode::Op(_) | ExprNode::Distinct(_) | ExprNode::NullIf(_) | ExprNode::Bool(_) | ExprNode::Case(_) | ExprNode::Coalesce(_) | ExprNode::CoerceViaIO(_) => {}
        // a domain's VALUE outside its CoerceToDomain is the emit's refusal, as CaseTest outside its CASE is
        ExprNode::CoerceToDomain(_) | ExprNode::CoerceToDomainValue => {}
        // the fetch (emit/subscript.rs); an assignment, FieldStore and nextval are the store's host sites (emit/store.rs)
        ExprNode::Subscripting(_) | ExprNode::FieldStore(_) | ExprNode::NextValue(_) => {}
        // the field select (emit/field.rs): a whole-row argument is the Var's refusal above
        ExprNode::FieldSelect(_) => {}
        // an image the command never has: NULL (nodes/misc.rs `Returning`)
        ExprNode::Returning(_) => {}
        // the row form (emit/row.rs): not strict, its site's arguments all by value
        ExprNode::Row(_) => {}
        // the rowtype conversion (emit/row.rs): a strict site over the composite, the map as constant words
        ExprNode::ConvertRowtype(c) if !c.wide => {}
        // an interpreter island (emit/host.rs `island`): the subtree is the interpreter's, its column Vars the site's
        // arguments; no island (a Param / whole-row / aggregate read, too many columns) is the refusal by kind
        ExprNode::Json(_) | ExprNode::Xml(_) => {
            if !p.islands.iter().any(|i| i.root == id) {
                return Err(EmitError::Unsupported(n.kind()));
            }
        }
        // the constructor (emit/arrayexpr.rs; the multidims form through MD_SITE, V4R-85; a wide one over continuation cells)
        ExprNode::Array(_) => {}
        // the list unrolled (`scalar_array`): a constant array or an ARRAY[...] of at most MAX_LIST elements; any other array
        // operand walked by the host, each element through the site's cell (`array_walk`); a constant of more than MAX_LIST
        // elements is `scalar_array`'s refusal
        ExprNode::ScalarArrayOp(_) => {}
        // the compare site (`Em::compare`): tier A in line, else a call as any FuncExpr's, by-reference arguments included
        ExprNode::MinMax(_) | ExprNode::RowCompare(_) => {}
        other => return Err(EmitError::Unsupported(other.kind())),
    }
    Ok(())
}

pub struct Em<'e, 'b, 'mcx> {
    pub(super) p: &'e Program<'mcx>,
    pub(crate) b: &'e mut FunctionBuilder<'b>,
    pub(crate) row: &'e RowRefs<'e>,
    plan: &'e Plan<'e>,
    pc: PerCodePlan<'e>,
    pub(super) casetest: Option<Lowered>,
    /// The value under a domain's CHECK (`CoerceToDomainValue`).
    domainval: Option<Lowered>,
    vars: PgVec<'e, ((Source, i16), Lowered)>,
    depth: u32,
    /// The root node: a numeric `extract` there is the integer word when `RowRefs::word_root` says so.
    root: NodeId,
    /// The node that word is read at (`datetime::word_node`: the root, or under a root integer cast its argument).
    word_at: (NodeId, bool),
    pub(crate) t: Value,
    pub(crate) f: Value,
    pub(crate) zero: Value,
    /// The last datum word made from a `Bool` (`word`): the pair the truth form reads (§7.7).
    word_of: Option<(Value, Value)>,
    /// D3620: the argument words each of the program's cells holds (`host::cell_width`): its cells' stride and null bytes.
    width: usize,
    /// D3930: `has_host` per node, once (0 not asked yet, 1 no, 2 yes): each call asks it of its operands, so asked afresh a
    /// left-deep chain's walk was quadratic in its depth.
    host_memo: PgVec<'e, core::cell::Cell<u8>>,
}

impl<'e, 'b, 'mcx> Em<'e, 'b, 'mcx> {
    pub fn new(p: &'e Program<'mcx>, b: &'e mut FunctionBuilder<'b>, row: &'e RowRefs<'e>, plan: &'e Plan<'e>, pc: PerCodePlan<'e>, root: NodeId) -> Self {
        let t = b.bconst(true);
        let f = b.bconst(false);
        let zero = b.iconst(Type::I64, 0);
        let word_at = super::prim::datetime::word_node(p, root);
        let width = host::cell_width(p);
        let host_memo = mcx::vec_elem_in(row.scratch, core::cell::Cell::new(0), p.nodes.len());
        Em { p, b, row, plan, pc, casetest: None, domainval: None, vars: PgVec::new_in(row.scratch), depth: 0, root, word_at, t, f, zero, word_of: None, width, host_memo }
    }

    /// Site `site`'s cell in `cells` (`host::cell_size` of the program's width apart).
    pub(super) fn site_cell(&mut self, cells: Value, site: usize) -> Value {
        self.b.ptr_add(cells, self.zero, 1, (site * host::cell_size(self.width)) as i32)
    }

    /// The bytes from a cell to the next (`host::cell_size` of the program's width).
    pub(super) fn cell_stride(&self) -> usize {
        host::cell_size(self.width)
    }

    /// A cell's null byte `i` (after the program's width of argument words).
    fn cell_null(&self, i: usize) -> i32 {
        debug_assert!(i < self.width, "null byte {i} of a cell {} wide", self.width);
        host::cell_nulls(self.width) + i as i32
    }

    fn null(&self) -> Lowered {
        Lowered { value: self.zero, valid: self.f, err: self.zero }
    }

    pub(crate) fn and(&mut self, x: Value, y: Value) -> Value {
        self.b.int_bin(IntBinOp::And, x, y)
    }

    pub(crate) fn or(&mut self, x: Value, y: Value) -> Value {
        self.b.int_bin(IntBinOp::Or, x, y)
    }

    /// D775: the raise word of `x` then `y` in PostgreSQL's order: `x`'s where it raised, else `y`'s. PG raises at the
    /// first failing operation, so a later form's error (a narrowing cast's, a typmod check's) never replaces it; an OR
    /// of the two reported the mask's lowest code (smallint over bigint out of range).
    pub(crate) fn first(&mut self, x: Value, y: Value) -> Value {
        if matches!(self.b.const_of(x), Some((_, 0))) {
            return y;
        }
        if matches!(self.b.const_of(y), Some((_, 0))) {
            return x;
        }
        let raised = self.b.icmp(IntCond::Ne, x, self.zero);
        self.b.select(raised, x, y)
    }

    /// D775: `go` and no raise in `err` yet (`go` alone when `err` is the constant 0).
    pub(crate) fn go_fresh(&mut self, go: Value, err: Value) -> Value {
        if matches!(self.b.const_of(err), Some((_, 0))) {
            return go;
        }
        let fresh = self.b.icmp(IntCond::Eq, err, self.zero);
        self.and(go, fresh)
    }

    /// D775: node `id` after the raise word `err` of what PG evaluates before it: one that calls a host runs in its own
    /// block only where `err` is 0 (a nextval, a volatile function, a stored error never reached past PG's raise); a
    /// pure one is evaluated as before, its raise word combined by the caller with `first`.
    pub(crate) fn after(&mut self, err: Value, id: NodeId) -> Result<Lowered, EmitError> {
        if matches!(self.b.const_of(err), Some((_, 0))) || !self.has_host(id) {
            return self.node(id);
        }
        let go = self.go_fresh(self.t, err);
        self.guarded(go, id)
    }

    /// `err` where `cond` holds, else no error (a guard over a raise mask).
    fn mask_if(&mut self, cond: Value, err: Value) -> Value {
        let z = self.zero;
        if matches!(self.b.const_of(err), Some((_, 0))) {
            return z;
        }
        self.b.select(cond, err, z)
    }

    pub(crate) fn not(&mut self, x: Value) -> Value {
        let t = self.t;
        self.b.int_bin(IntBinOp::Xor, x, t)
    }

    /// PS1-15 / X5: whether bound root `r` (a `LikeBound`, or an `EqBound` whose every value is a parameter) is NULL on
    /// every row this execution: a LIKE whose pattern reads a NULL parameter (textcat and LIKE are strict), a code set
    /// whose parameters are all NULL (`x = $1`, `x <> $1`, `x NOT IN ($1, $2)`). The source gave such a table `Faults`
    /// (hosts granule_source `bound_spec`), so the row would evaluate the body to reach that NULL. The parameters' null
    /// flags are the statement's words (`RowRefs::consts_null`), one loop-invariant test. `None`: no such root, or a
    /// parameter read where the expression reaches it (a lazy InitPlan's), which keeps the fault.
    pub(super) fn bound_null(&mut self, r: &crate::percode::Root) -> Option<Value> {
        let all = match &r.table.and_then(|t| self.pc.needs.get(t as usize))?.known {
            crate::percode::Known::LikeBound { .. } => false,
            crate::percode::Known::EqBound { values, consts, .. } if values.is_empty() && consts.is_empty() => true,
            _ => return None,
        };
        let m = self.row.scratch;
        let mut nulls = PgVec::new_in(m);
        let mut stack = mcx::vec_array_in(m, [r.node]);
        while let Some(id) = stack.pop() {
            match &self.p.nodes[id.0 as usize] {
                ExprNode::Param(q) => {
                    if q.kind == ParamKind::PARAM_EXEC && self.row.lazy.is_some_and(|l| l.has_param(q.id)) {
                        return None;
                    }
                    nulls.push_in(self.row.consts_null[self.plan.index(id)? as usize]);
                }
                _ => stack.extend_in(walk_kids(self.p, id)),
            }
        }
        let (&first, rest) = nulls.split_first()?;
        Some(rest.iter().fold(first, |a, &n| if all { self.and(a, n) } else { self.or(a, n) }))
    }

    pub(crate) fn word(&mut self, v: Value) -> Value {
        let w = self.b.conv(ConvOp::Zext, Type::I64, v);
        self.word_of = Some((w, v));
        w
    }

    /// The `Bool` behind a lowered root's word when the root made it (a compare, a bool op, a
    /// null test, a per-code verdict), else `value != 0`; the caller branches on it and the word
    /// dies unused.
    pub(crate) fn truth_of(&mut self, l: &Lowered) -> Value {
        match self.word_of {
            Some((w, v)) if w == l.value => v,
            _ => self.truth(l.value),
        }
    }

    pub(crate) fn truth(&mut self, word: Value) -> Value {
        let z = self.zero;
        self.b.icmp(IntCond::Ne, word, z)
    }

    fn select3(&mut self, c: Value, a: Lowered, d: Lowered) -> Lowered {
        let value = self.b.select(c, a.value, d.value);
        let valid = self.b.select(c, a.valid, d.valid);
        let err = self.or(a.err, d.err);
        Lowered { value, valid, err }
    }

    fn has_host(&self, id: NodeId) -> bool {
        crate::stack_check();
        let memo = self.host_memo.get(id.0 as usize);
        match memo.map(core::cell::Cell::get) {
            Some(1) => return false,
            Some(2) => return true,
            _ => {}
        }
        let r = self.host_at(id);
        if let Some(m) = memo {
            m.set(if r { 2 } else { 1 });
        }
        r
    }

    /// `has_host` of node `id` (its operands' through `has_host`, each once).
    fn host_at(&self, id: NodeId) -> bool {
        if self.row.late.iter().any(|l| l.0 == id) {
            return false; // D3033: computed ahead, a read here
        }
        if self.p.islands.iter().any(|i| i.root == id) {
            return true;
        }
        let site = |s: SiteId| !prim::is_prim(self.p, &self.p.sites[s.0 as usize], false);
        let here = match &self.p.nodes[id.0 as usize] {
            ExprNode::Func(n) | ExprNode::Op(n) | ExprNode::Distinct(n) | ExprNode::NullIf(n) => site(n.site),
            ExprNode::ScalarArrayOp(n) => site(n.site) || walked(self.p, n),
            ExprNode::NullTest(t) => t.row,
            ExprNode::MinMax(m) => site(m.cmp),
            ExprNode::RowCompare(r) => (0..r.largs.len as u32).any(|i| site(SiteId(r.sites + i))),
            ExprNode::Subscripting(_) | ExprNode::FieldSelect(_) | ExprNode::Row(_) | ExprNode::ConvertRowtype(_) | ExprNode::Array(_) | ExprNode::FieldStore(_) | ExprNode::NextValue(_) => true,
            ExprNode::Json(_) | ExprNode::Xml(_) => true,
            ExprNode::CoerceViaIO(c) => self.shred_lane(c.arg).is_none(),
            ExprNode::Aggref(a) => self.row.aggs.get(a.aggno as usize).is_some_and(|c| c.is_host()),
            ExprNode::WindowFunc(w) => self.row.aggs.get(w.cell as usize).is_some_and(|c| c.is_host()),
            // M8 B2c: a lazy SubPlan's hook runs the subplan where the expression reaches it (a host call: a guard branches)
            ExprNode::SubPlan(s) => self.row.aggs.get(s.cell as usize).is_some_and(|c| c.is_host()) || self.row.lazy.is_some_and(|l| l.has_subplan(s.plan_id)),
            ExprNode::Srf(s) => self.row.aggs.get(s.cell as usize).is_some_and(|c| c.is_host()),
            ExprNode::Var(v) => self.row.lazy.is_some_and(|l| (v.src == Source::Scan && l.has(v.attno)) || l.has_col(v.src, v.attno)),
            // D2834: a deferred extern parameter is fetched where it is evaluated (`deferred`, a host call: a guard branches)
            ExprNode::Param(q) => (q.kind == ParamKind::PARAM_EXEC && self.row.lazy.is_some_and(|l| l.has_param(q.id))) || crate::nodes::param::is_deferred(q),
            _ => false,
        };
        here || walk_kids(self.p, id).into_iter().any(|k| self.has_host(k))
    }

    /// `id` evaluated only where `guard` holds: in its own block when it calls a host, else in
    /// line with its flag masked by the guard.
    pub(super) fn guarded(&mut self, guard: Value, id: NodeId) -> Result<Lowered, EmitError> {
        if !self.has_host(id) {
            let l = self.node(id)?;
            let err = self.mask_if(guard, l.err);
            let valid = self.and(l.valid, guard);
            return Ok(Lowered { value: l.value, valid, err });
        }
        self.branch(guard, id)
    }

    /// D3233 (`CaseSelect`): a CASE arm that computes anything in its own block where `guard` holds, as `guarded` runs one
    /// that calls a host; a trivial arm is read and masked as before.
    fn arm(&mut self, guard: Value, id: NodeId) -> Result<Lowered, EmitError> {
        if CaseSelect::on() && !trivial(self.p, id) && !matches!(self.b.const_of(guard), Some(_)) {
            return self.branch(guard, id);
        }
        self.guarded(guard, id)
    }

    /// Node `id` evaluated in its own block only where `guard` holds: NULL and no raise elsewhere.
    fn branch(&mut self, guard: Value, id: NodeId) -> Result<Lowered, EmitError> {
        let l_do = self.b.create_block(&[]);
        let l_join = self.b.create_block(&[Type::I64, Type::Bool, Type::I64]);
        let (z, f) = (self.zero, self.f);
        self.b.cond_br(guard, (l_do, &[]), (l_join, &[z, f, z]));
        self.b.switch_to(l_do);
        self.depth += 1;
        let l = self.node(id)?;
        self.depth -= 1;
        self.b.br(l_join, &[l.value, l.valid, l.err]);
        self.b.switch_to(l_join);
        let ps = self.b.block_params(l_join);
        Ok(Lowered { value: ps[0], valid: ps[1], err: ps[2] })
    }

    pub fn node(&mut self, id: NodeId) -> Result<Lowered, EmitError> {
        crate::stack_check();
        // D3033: an operand the caller computed once at its hoist scope (`RowRefs::late`), its held raise read here
        if let Some(&(_, [value, valid, err])) = self.row.late.iter().find(|l| l.0 == id) {
            return Ok(Lowered { value, valid, err });
        }
        if let Some(r) = self.pc.root_at(id).copied() {
            return self.percode(&r);
        }
        if self.p.islands.iter().any(|i| i.root == id) {
            return self.island(id);
        }
        match &self.p.nodes[id.0 as usize] {
            ExprNode::Var(v) => self.var(v.src, v.attno),
            ExprNode::Param(q) if q.kind == ParamKind::PARAM_EXEC && self.row.lazy.is_some_and(|l| l.has_param(q.id)) => {
                let [value, valid, err] = self.row.lazy.map_or([self.zero, self.f, self.zero], |l| l.emit_param(self.b, q.id)); // M8 B1: here, where PG evaluates it
                Ok(Lowered { value, valid, err })
            }
            ExprNode::Param(q) if crate::nodes::param::is_deferred(q) => self.deferred(id, q.id, q.typ),
            ExprNode::Const(_) | ExprNode::Param(_) => {
                let i = self.plan.index(id).ok_or(EmitError::Unsupported(Kind::Param))? as usize;
                let value = self.row.consts[i];
                let valid = self.not(self.row.consts_null[i]);
                Ok(Lowered { value, valid, err: self.zero })
            }
            ExprNode::Relabel(r) => {
                let l = self.node(r.arg)?;
                let ext = match r.canon {
                    Canon::Keep => return Ok(l),
                    Canon::Sext32 => ConvOp::Sext,
                    Canon::Zext32 => ConvOp::Zext,
                };
                let t = self.b.conv(ConvOp::Trunc, Type::I32, l.value);
                Ok(Lowered { value: self.b.conv(ext, Type::I64, t), ..l })
            }
            ExprNode::Imm(i) => Ok(Lowered { value: self.b.iconst(Type::I64, i.value), valid: self.t, err: self.zero }),
            ExprNode::Returning(_) => Ok(self.null()),
            ExprNode::Func(n) | ExprNode::Op(n) => {
                if let Some(l) = self.shred(id, ShredType::Text) {
                    return Ok(l);
                }
                if let Some(l) = super::fuse::fused(self, n.site)? {
                    return Ok(l);
                }
                // B2-31: a word root's integer cast is its argument's word (`datetime::word_node`)
                if self.row.word_root && id == self.root && self.word_at.1 {
                    return self.node(self.word_at.0);
                }
                let args = self.args(n.site)?;
                self.call(n.site, &args, self.row.word_root && id == self.word_at.0)
            }
            ExprNode::CoerceViaIO(c) => {
                if let Some(l) = self.shred(c.arg, ShredType::Fixed(c.ioparam)) {
                    return Ok(l);
                }
                let a = self.node(c.arg)?;
                let text = if c.direct { a } else { self.call(c.out, &[a], false)? };
                let ioparam = self.b.iconst(Type::I64, c.ioparam as i64);
                let typmod = self.b.iconst(Type::I64, -1);
                let (t, z) = (self.t, self.zero);
                let n = self.p.sites[c.inp.0 as usize].nargs as usize;
                self.call(c.inp, &[text, Lowered { value: ioparam, valid: t, err: z }, Lowered { value: typmod, valid: t, err: z }][..n], false)
            }
            ExprNode::Distinct(n) => {
                let a = self.args(n.site)?;
                let r = self.call(n.site, &a, false)?;
                let (na, nb) = (self.not(a[0].valid), self.not(a[1].valid));
                let both = self.and(na, nb);
                let one = self.b.int_bin(IntBinOp::Xor, na, nb);
                let rt = self.truth(r.value);
                let ne = self.not(rt);
                let v0 = self.b.select(both, self.f, ne);
                let v = self.b.select(one, self.t, v0);
                Ok(Lowered { value: self.word(v), valid: self.t, err: r.err })
            }
            ExprNode::NullIf(n) => {
                let a = self.args(n.site)?;
                let r = self.call(n.site, &a, false)?;
                let rt = self.truth(r.value);
                let eq = self.and(r.valid, rt);
                let neq = self.not(eq);
                let valid = self.and(a[0].valid, neq);
                Ok(Lowered { value: a[0].value, valid, err: r.err })
            }
            ExprNode::ScalarArrayOp(n) => self.scalar_array(n),
            ExprNode::Bool(n) => {
                let ks = walk_kids(self.p, id).slice().unwrap_or(&[]);
                match n.op {
                    BoolExprType::NOT_EXPR => {
                        let a = self.node(ks[0])?;
                        let t = self.truth(a.value);
                        let nt = self.not(t);
                        Ok(Lowered { value: self.word(nt), valid: a.valid, err: a.err })
                    }
                    BoolExprType::AND_EXPR => self.chain(ks, false),
                    BoolExprType::OR_EXPR => self.chain(ks, true),
                }
            }
            ExprNode::NullTest(n) if n.row => self.row_null_test(n),
            ExprNode::NullTest(n) => {
                // a dictionary-coded column has no value word in the row loop, but its validity is the code lane's bitmap
                let a = match &self.p.nodes[n.arg.0 as usize] {
                    ExprNode::Var(v) if v.src == Source::Scan && v.attno > 0 && self.row.codes.iter().any(|c| c.attno == v.attno) => self.code_valid(v.attno)?,
                    _ => self.node(n.arg)?,
                };
                let v = if n.is_null { self.not(a.valid) } else { a.valid };
                Ok(Lowered { value: self.word(v), valid: self.t, err: a.err })
            }
            ExprNode::BooleanTest(n) => {
                let a = self.node(n.arg)?;
                let tv = self.truth(a.value);
                let (null, t) = (self.not(a.valid), self.and(a.valid, tv));
                let ntv = self.not(tv);
                let fl = self.and(a.valid, ntv);
                let v = match n.test {
                    BoolTestType::IS_TRUE => t,
                    BoolTestType::IS_NOT_TRUE => self.not(t),
                    BoolTestType::IS_FALSE => fl,
                    BoolTestType::IS_NOT_FALSE => self.not(fl),
                    BoolTestType::IS_UNKNOWN => null,
                    BoolTestType::IS_NOT_UNKNOWN => a.valid,
                };
                Ok(Lowered { value: self.word(v), valid: self.t, err: a.err })
            }
            ExprNode::Case(c) => {
                let saved = self.casetest;
                let mut test_err = self.zero;
                if let Some(a) = c.arg {
                    let v = self.node(a)?;
                    (self.casetest, test_err) = (Some(v), v.err);
                }
                let r = self.case(id, test_err);
                self.casetest = saved;
                r
            }
            ExprNode::CaseTest => self.casetest.ok_or(EmitError::Unsupported(Kind::CaseTestExpr)),
            ExprNode::CoerceToDomain(d) => {
                let d = *d;
                let a = self.node(d.arg)?;
                let mut err = a.err;
                if d.notnull {
                    let null = self.not(a.valid);
                    err = self.domain_raise(d.site, d.typ, -1, null, err)?;
                }
                let saved = self.domainval;
                self.domainval = Some(a);
                for (i, k) in walk_kids(self.p, id).skip(1).enumerate() {
                    let c = self.after(err, k)?; // D775: a check calling a host never runs past a raise
                    err = self.first(err, c.err);
                    let tv = self.truth(c.value);
                    let ntv = self.not(tv);
                    let fails = self.and(c.valid, ntv);
                    err = self.domain_raise(d.site, d.typ, i as i64, fails, err)?;
                }
                self.domainval = saved;
                Ok(Lowered { value: a.value, valid: a.valid, err })
            }
            ExprNode::CoerceToDomainValue => self.domainval.ok_or(EmitError::Unsupported(Kind::CoerceToDomainValue)),
            ExprNode::Subscripting(n) => self.subscript(n),
            ExprNode::FieldSelect(n) => self.field_select(n),
            ExprNode::FieldStore(n) => self.field_store(n),
            ExprNode::NextValue(n) => self.next_value(n),
            ExprNode::Row(n) => self.row_expr(n),
            ExprNode::ConvertRowtype(n) => self.convert_rowtype(n),
            ExprNode::Json(_) | ExprNode::Xml(_) => self.island(id),
            ExprNode::Array(n) => self.array_expr(n),
            ExprNode::Coalesce(_) => {
                let ks = walk_kids(self.p, id);
                let mut alive = self.t;
                let mut arms = mcx::reserved_in(ks.count_kids(), self.row.scratch);
                for k in ks {
                    let a = self.guarded(alive, k)?;
                    let nv = self.not(a.valid);
                    alive = self.and(alive, nv);
                    alive = self.go_fresh(alive, a.err); // D775: no arm past a raise (PG stops there)
                    arms.push_in(a);
                }
                let mut acc = self.null();
                for a in arms.into_iter().rev() {
                    acc = self.select3(a.valid, a, acc);
                }
                Ok(acc)
            }
            ExprNode::MinMax(m) => {
                let ks = walk_kids(self.p, id);
                let cmp = &self.p.sites[m.cmp.0 as usize];
                let mut best = self.null();
                // D775: the running raise in PG's order (each argument, then its compare); `best.err` is it
                let mut err = self.zero;
                for k in ks {
                    let mut a = self.after(err, k)?;
                    err = self.first(err, a.err);
                    a.err = err;
                    let c = self.compare(m.cmp, cmp, a, best)?;
                    err = self.first(err, c.err);
                    a.err = err;
                    let z = self.zero;
                    let better = if m.least { self.b.icmp(IntCond::Slt, c.value, z) } else { self.b.icmp(IntCond::Sgt, c.value, z) };
                    let nb = self.not(best.valid);
                    let take0 = self.or(nb, better);
                    let take = self.and(a.valid, take0);
                    best = self.select3(take, a, best);
                }
                Ok(best)
            }
            ExprNode::RowCompare(r) => self.row_compare(r.cmptype, r.largs, r.rargs, r.sites),
            ExprNode::Aggref(a) => self.aggref(a),
            ExprNode::WindowFunc(w) => self.aggref(&crate::nodes::agg::Aggref { aggno: w.cell, typ: w.typ }),
            ExprNode::SubPlan(s) if self.row.lazy.is_some_and(|l| l.has_subplan(s.plan_id)) => {
                // D3374: a SubPlan inside an island's range is only ever that island's argument (nodes/mod.rs `island`)
                let island = self.p.islands.iter().any(|i| i.first.0 <= id.0 && id.0 <= i.root.0);
                let [value, valid, err] = self.row.lazy.map_or([self.zero, self.f, self.zero], |l| if island { l.emit_subplan_held(self.b, s.plan_id) } else { l.emit_subplan(self.b, s.plan_id) });
                Ok(Lowered { value, valid, err })
            }
            ExprNode::SubPlan(s) => self.aggref(&crate::nodes::agg::Aggref { aggno: s.cell, typ: s.typ }),
            ExprNode::Srf(s) => self.aggref(&crate::nodes::agg::Aggref { aggno: s.cell, typ: s.typ }),
            other => Err(EmitError::Unsupported(other.kind())),
        }
    }

    /// A column read once per row: the load is shared by every `Var` of it emitted in the entry
    /// block (a load inside a guarded block does not dominate the rest and is not shared).
    fn shred_lane(&self, id: NodeId) -> Option<&jsonb::ShredRef> {
        let (attno, path, text) = jsonb::path_of(self.p, id)?;
        if !text {
            return None;
        }
        self.row.shreds.iter().find(|s| s.attno == attno && s.path == path)
    }

    /// A `->` / `->>` chain the row references carry as a shredded lane of `typ` (§2.7).
    fn shred(&mut self, id: NodeId, typ: ShredType) -> Option<Lowered> {
        let (attno, path, text) = jsonb::path_of(self.p, id)?;
        if !text {
            return None;
        }
        let s = jsonb::find(self.row.shreds, attno, &path, typ)?;
        let ColsRef::Lanes { row, .. } = self.row.scan else { return None };
        let lane = s.lane;
        Some(self.lane_cell(&lane, row))
    }

    fn var(&mut self, src: Source, attno: i16) -> Result<Lowered, EmitError> {
        if self.depth == 0 {
            if let Some((_, l)) = self.vars.iter().find(|(k, _)| *k == (src, attno)) {
                return Ok(*l);
            }
        }
        let l = self.var_load(src, attno)?;
        if self.depth == 0 {
            self.vars.push_in(((src, attno), l));
        }
        Ok(l)
    }

    fn var_load(&mut self, src: Source, attno: i16) -> Result<Lowered, EmitError> {
        if attno == 0 {
            return self.whole_row(src);
        }
        if src == Source::Scan && self.row.codes.iter().any(|c| c.attno == attno) {
            return Err(EmitError::CodesLane(attno));
        }
        if let Some(l) = self.row.lazy.filter(|l| src == Source::Scan && l.has(attno)) {
            let [value, valid, err] = l.emit(self.b, attno); // V4R-342: here, where PG evaluates it
            return Ok(Lowered { value, valid, err });
        }
        if let Some(l) = self.row.lazy.filter(|l| l.has_col(src, attno)) {
            let [value, valid, err] = l.emit_col(self.b, src, attno); // D2997: a claim's fallback-only column, built here
            return Ok(Lowered { value, valid, err });
        }
        let cols = match src {
            Source::Scan => self.row.scan,
            Source::Inner => self.row.inner,
            Source::Outer => self.row.outer,
        };
        let i = attno as usize - 1;
        match cols {
            ColsRef::None => Err(EmitError::NoColumn(attno)),
            ColsRef::Slot { values, nulls } => {
                let value = self.b.load(Type::I64, values, (i * 8) as i32);
                let n = self.b.load_zext(Type::I8, nulls, i as i32);
                let valid = self.b.icmp(IntCond::Eq, n, self.zero);
                Ok(Lowered { value, valid, err: self.zero })
            }
            ColsRef::Values { values, valids } => {
                let value = *values.get(i).ok_or(EmitError::NoColumn(attno))?;
                let valid = *valids.get(i).ok_or(EmitError::NoColumn(attno))?;
                let raised = (src == Source::Scan).then(|| self.row.col_raises.iter().find(|r| r.0 == attno)).flatten();
                Ok(Lowered { value, valid, err: raised.map_or(self.zero, |r| r.1) })
            }
            ColsRef::Lanes { row, lanes } => {
                let l = lanes.get(i).copied().flatten().ok_or(EmitError::NoColumn(attno))?;
                if l.kind == gravity_abi::ctx::lane::KIND_CODES {
                    return Err(EmitError::CodesLane(attno));
                }
                Ok(self.lane_cell(&l, row))
            }
            ColsRef::Record { base, offsets } => {
                let off = *offsets.get(i).ok_or(EmitError::NoColumn(attno))?;
                let value = self.b.load(Type::I64, base, off as i32);
                Ok(Lowered { value, valid: self.t, err: self.zero })
            }
            ColsRef::RecordNulls { base, offsets, null_off, bits } => {
                let off = *offsets.get(i).ok_or(EmitError::NoColumn(attno))?;
                let value = self.b.load(Type::I64, base, off as i32);
                let bit = *bits.get(i).ok_or(EmitError::NoColumn(attno))?;
                let valid = if bit == super::NULL_BIT_NONE {
                    self.t
                } else {
                    let w = self.b.load(Type::I64, base, null_off as i32);
                    let sh = self.b.iconst(Type::I64, bit as i64);
                    let s = self.b.int_bin(IntBinOp::Ushr, w, sh);
                    let one = self.b.iconst(Type::I64, 1);
                    let b1 = self.b.int_bin(IntBinOp::And, s, one);
                    self.b.icmp(IntCond::Eq, b1, self.zero)
                };
                Ok(Lowered { value, valid, err: self.zero })
            }
        }
    }

    /// The validity of a code lane's cell (its code word is not a datum: only a NULL test reads it).
    fn code_valid(&mut self, attno: i16) -> Result<Lowered, EmitError> {
        let cr = *self.row.codes.iter().find(|c| c.attno == attno).ok_or(EmitError::NoColumn(attno))?;
        let ColsRef::Lanes { row, .. } = self.row.scan else { return Err(EmitError::CodesLane(attno)) };
        let l = self.lane_cell(&cr.lane, row);
        Ok(Lowered { value: self.zero, valid: l.valid, err: self.zero })
    }

    fn lane_cell(&mut self, l: &LaneRef, row: Value) -> Lowered {
        let p = self.b.ptr_add(l.ptr, row, l.width, 0);
        let value = match l.width {
            8 => self.b.load(Type::I64, p, 0),
            4 => {
                let v = self.b.load(Type::I32, p, 0);
                self.b.conv(ConvOp::Sext, Type::I64, v)
            }
            2 => {
                let v = self.b.load(Type::I16, p, 0);
                self.b.conv(ConvOp::Sext, Type::I64, v)
            }
            _ => self.b.load_zext(Type::I8, p, 0),
        };
        let valid = self.lane_valid(l, row);
        Lowered { value, valid, err: self.zero }
    }

    /// The lane's validity bit at `row` (`true` for an all-valid lane; branch-free otherwise).
    pub(crate) fn lane_valid(&mut self, l: &LaneRef, row: Value) -> Value {
        if l.all_valid {
            return self.t;
        }
        let vnull = match l.valid_null {
            Some(v) => v,
            None => {
                let vi = self.b.conv(ConvOp::PtrToInt, Type::I64, l.valid);
                self.b.icmp(IntCond::Eq, vi, self.zero)
            }
        };
        let six = self.b.iconst(Type::I64, 6);
        let wi = self.b.int_bin(IntBinOp::Ushr, row, six);
        let vp = self.b.ptr_add(l.valid, wi, 8, 0);
        // D2431: a lane with no bitmap reads its (discarded) word at the lane pointer rounded down to 8, so the load stays
        // 8-aligned (a 4- or 2-byte lane's element address is not) and in the same mapped word
        let pi = self.b.conv(ConvOp::PtrToInt, Type::I64, l.ptr);
        let m8 = self.b.iconst(Type::I64, !7);
        let pa = self.b.int_bin(IntBinOp::And, pi, m8);
        let aligned = self.b.conv(ConvOp::IntToPtr, Type::Ptr, pa);
        let safe = self.b.select(vnull, aligned, vp);
        let w = self.b.load(Type::I64, safe, 0);
        let m63 = self.b.iconst(Type::I64, 63);
        let sh = self.b.int_bin(IntBinOp::And, row, m63);
        let bit0 = self.b.int_bin(IntBinOp::Ushr, w, sh);
        let one = self.b.iconst(Type::I64, 1);
        let bit = self.b.int_bin(IntBinOp::And, bit0, one);
        let set = self.b.icmp(IntCond::Ne, bit, self.zero);
        self.b.select(vnull, self.t, set)
    }

    fn args(&mut self, site: SiteId) -> Result<PgVec<'e, Lowered>, EmitError> {
        let p = self.p;
        let k = p.sites[site.0 as usize].args;
        let ids = &p.kids[k.start as usize..k.start as usize + k.len as usize];
        // the last operand with a host call (`has_host` is memoized per node)
        let last_host = ids.iter().rposition(|&id| self.has_host(id));
        let mut out = mcx::reserved_in(ids.len(), self.row.scratch);
        let mut err = self.zero;
        // D775: in order, an operand with a host call only where no earlier one raised (the running word kept only while
        // a later operand calls one: pure arithmetic pays nothing here, `call` combines the words)
        for (i, &id) in ids.iter().enumerate() {
            let a = if self.has_host(id) { self.after(err, id)? } else { self.node(id)? };
            if last_host.is_some_and(|l| l > i) {
                err = self.first(err, a.err);
            }
            out.push_in(a);
        }
        Ok(out)
    }

    /// The domain's raise where `flag` holds and no earlier raise of the row is in `err`: the site called with the
    /// type and the check's index (its cell then carries the built error, as a tier B call's does); the mask out.
    fn domain_raise(&mut self, site: SiteId, typ: u32, ix: i64, flag: Value, err: Value) -> Result<Value, EmitError> {
        let raised = self.b.icmp(IntCond::Ne, err, self.zero);
        let fresh = self.not(raised);
        let go = self.and(flag, fresh);
        let l_do = self.b.create_block(&[]);
        let l_join = self.b.create_block(&[Type::I64]);
        self.b.cond_br(go, (l_do, &[]), (l_join, &[err]));
        self.b.switch_to(l_do);
        let t = self.b.iconst(Type::I64, typ as i64);
        let i = self.b.iconst(Type::I64, ix);
        // D2019: the domain's baked names from the site's cell (`CELL_AUX`): the raise may run on a pool thread
        let Some(cells) = self.row.cells else { return Err(EmitError::NoCells) };
        let cell = self.site_cell(cells, site.0 as usize);
        let names = self.b.load(Type::I64, cell, super::host::CELL_AUX);
        let args = [Lowered { value: t, valid: self.t, err: self.zero }, Lowered { value: i, valid: self.t, err: self.zero }, Lowered { value: names, valid: self.t, err: self.zero }];
        let r = self.call(site, &args, false)?;
        let e = self.or(err, r.err);
        self.b.br(l_join, &[e]);
        self.b.switch_to(l_join);
        Ok(self.b.block_params(l_join)[0])
    }

    /// The site over evaluated arguments: tier A in line, tier B / C through the cell.
    pub(super) fn call(&mut self, site: SiteId, args: &[Lowered], word_root: bool) -> Result<Lowered, EmitError> {
        let call: &Call = &self.p.sites[site.0 as usize];
        let mut all_valid = self.t;
        let mut err = self.zero;
        for a in args {
            all_valid = self.and(all_valid, a.valid);
            err = self.first(err, a.err);
        }
        if prim::is_prim(self.p, call, word_root) {
            let vals: PgVec<'_, Value> = args.iter().map(|a| a.value).collect_in(self.row.scratch);
            let (o, nonnull) = prim::emit_ext(self.b, self.p, call, &vals, word_root).expect("a tier-A primitive");
            let e = self.mask_if(all_valid, o.err);
            let err = self.first(err, e);
            let valid = match nonnull {
                Some(n) => self.and(all_valid, n),
                None => all_valid,
            };
            return Ok(Lowered { value: o.value, valid, err });
        }
        let (Some(cells), Some(region)) = (self.row.cells, self.row.region) else { return Err(EmitError::NoCells) };
        // D3129: the direct road is 64-bit only (`direct`); on a 32-bit target (wasm32) every site takes the cell road below
        #[cfg(target_pointer_width = "64")]
        if let Some(shim) = host::direct(self.p, call) {
            return Ok(self.direct(site, shim, args, all_valid, err, cells));
        }
        let cell = self.site_cell(cells, site.0 as usize);
        debug_assert!(args.len() <= self.width, "site {} stores {} words in a cell {} wide", site.0, args.len(), self.width);
        let flags = HostFlags { may_error: true, pure: false, touches_memory: true, worker: false, state: StateMask::NONE };
        // a shim site is strict unless its call says otherwise AND every argument is by value (a view of a null
        // datum is never formed): a RowExpr site's arguments are words, its null mask one of them
        let strict = call.strict || matches!(call.how, How::Shim { convs, .. } if self.p.convs[convs as usize..convs as usize + call.nargs as usize].iter().any(|c| c.kind != gravity_fn::ArgKind::V));
        let l_call = self.b.create_block(&[]);
        let l_join = self.b.create_block(&[Type::I64, Type::Bool, Type::I64]);
        let (z, f) = (self.zero, self.f);
        // D775: never called past an operand's raise (PG never reaches the call; its error or its side effect would win)
        let go = self.go_fresh(if strict { all_valid } else { self.t }, err);
        if strict || go != self.t {
            self.b.cond_br(go, (l_call, &[]), (l_join, &[z, f, z]));
        } else {
            self.b.br(l_call, &[]);
        }
        self.b.switch_to(l_call);
        for (i, a) in args.iter().enumerate() {
            self.b.store(cell, CELL_ARGS + (i * 8) as i32, a.value);
            if !strict {
                let nv = self.not(a.valid);
                let byte = self.b.conv(ConvOp::Zext, Type::I8, nv);
                let at = self.cell_null(i);
                self.b.store(cell, at, byte);
            }
        }
        let status = match call.how {
            How::Shim { .. } => {
                let h = self.b.declare_host_once("gravity_expr::shim", &[Type::Ptr, Type::Ptr], Some(Type::I64), host::shim as *const () as usize, flags);
                self.b.call_host(h, &[cell, region], Some(Type::I64)).unwrap()
            }
            How::Fmgr(_) => {
                let h = self.b.declare_host_once("gravity_expr::fmgr", &[Type::Ptr], Some(Type::I64), host::fmgr as *const () as usize, flags);
                self.b.call_host(h, &[cell], Some(Type::I64)).unwrap()
            }
            How::Island { .. } => {
                let h = self.b.declare_host_once("gravity_expr::island", &[Type::Ptr, Type::Ptr], Some(Type::I64), host::island as *const () as usize, flags);
                self.b.call_host(h, &[cell, region], Some(Type::I64)).unwrap()
            }
        };
        let valid = self.b.icmp(IntCond::Eq, status, self.zero);
        let failed = self.b.icmp(IntCond::Slt, status, self.zero);
        let e = super::prim::raise(self.b, failed, gravity_abi::status::raise::HOST);
        let value = self.b.load(Type::I64, cell, CELL_OUT);
        self.b.br(l_join, &[value, valid, e]);
        self.b.switch_to(l_join);
        let ps = self.b.block_params(l_join);
        let (value, valid, e) = (ps[0], ps[1], ps[2]);
        let err = self.or(err, e);
        Ok(Lowered { value, valid, err })
    }

    /// D2834: deferred extern parameter `pid` (nodes/param.rs `DEFERRED`) where the expression evaluates it, never ahead:
    /// the host `param_deferred` fetches it from the execution's list (its word, `ParamNeed::Deferred`) as
    /// ExecEvalParamExtern does, so a slot PL/pgSQL left without a value raises its error, and one whose type changed raises
    /// the mismatch, only on the rows (and arms) that reach it. Its error is stashed (`HOST`).
    fn deferred(&mut self, id: NodeId, pid: i32, typ: types_core::Oid) -> Result<Lowered, EmitError> {
        let i = self.plan.index(id).ok_or(EmitError::Unsupported(Kind::Param))? as usize;
        let list = self.row.consts[i];
        let tmp = self.b.declare_stack_slot(8, 8);
        let out = self.b.stack_addr(tmp);
        self.b.store(out, 0, self.zero);
        let (pid, typ) = (self.b.iconst(Type::I64, i64::from(pid)), self.b.iconst(Type::I64, i64::from(typ)));
        let flags = HostFlags { may_error: true, pure: false, touches_memory: true, worker: false, state: StateMask::NONE };
        let h = self.b.declare_host_once("gravity_expr::param_deferred", &[Type::I64, Type::I64, Type::I64, Type::Ptr], Some(Type::I64), host::param_deferred as *const () as usize, flags);
        let status = self.b.call_host(h, &[list, pid, typ, out], Some(Type::I64)).unwrap();
        let value = self.b.load(Type::I64, out, 0);
        let valid = self.b.icmp(IntCond::Eq, status, self.zero);
        let failed = self.b.icmp(IntCond::Slt, status, self.zero);
        let err = super::prim::raise(self.b, failed, gravity_abi::status::raise::HOST);
        Ok(Lowered { value, valid, err })
    }

    /// The direct road (`host::direct`): the shim's typed entry over the words in registers behind the strict null
    /// short-circuit, the value back in the return register, the error the site's slot after the call.
    /// D3129: 64-bit targets only. The typed entry is `(*mut ErrSlot, i64..) -> Ret` and the IR declares it as
    /// `(Ptr, I64..) -> I64`, which a 64-bit ABI makes the same call (the slot pointer one 64-bit word, `Ret::val` the first
    /// return register). On wasm32 the pointer is an `i32` and the 16-byte `Ret` comes back through a hidden result pointer,
    /// so the indirect call traps (`call_indirect`'s type check); there the site takes the cell road, which reaches the shim
    /// from Rust (`Shim::call`). gravity_lint's host_decls test holds a shim-entry declaration to this cfg.
    #[cfg(target_pointer_width = "64")]
    fn direct(&mut self, site: SiteId, shim: &'static gravity_fn::Shim, args: &[Lowered], all_valid: Value, err: Value, cells: Value) -> Lowered {
        let cell_slot = self.b.ptr_add(cells, self.zero, 1, (site.0 as usize * self.cell_stride()) as i32 + host::CELL_ERR);
        let tmp = self.b.declare_stack_slot(8, 8);
        let l_call = self.b.create_block(&[]);
        let l_fail = self.b.create_block(&[]);
        let l_join = self.b.create_block(&[Type::I64, Type::Bool, Type::I64]);
        let (z, f, t) = (self.zero, self.f, self.t);
        let go = self.go_fresh(all_valid, err); // D775: as `call`'s cell road
        self.b.cond_br(go, (l_call, &[]), (l_join, &[z, f, z]));
        self.b.switch_to(l_call);
        let slot = self.b.stack_addr(tmp);
        self.b.store(slot, 0, z);
        let m = self.row.scratch;
        let mut params = mcx::vec_array_in(m, [Type::Ptr]);
        let mut vals = mcx::vec_array_in(m, [slot]);
        for a in args {
            params.push_in(Type::I64);
            vals.push_in(a.value);
        }
        let flags = HostFlags { may_error: true, pure: false, touches_memory: true, worker: false, state: StateMask::NONE };
        let name = format!("gravity_fn::byval::{}", shim.name); // alloc: Program
        let h = self.b.declare_host_once(&name, &params, Some(Type::I64), shim.addr(), flags);
        let value = self.b.call_host(h, &vals, Some(Type::I64)).unwrap();
        let ep = self.b.load(Type::I64, slot, 0);
        let failed = self.b.icmp(IntCond::Ne, ep, z);
        let raised = self.b.iconst(Type::I64, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64);
        self.b.cond_br(failed, (l_fail, &[]), (l_join, &[value, t, z]));
        self.b.switch_to(l_fail);
        let put = self.b.declare_host_once("gravity_expr::slot_put", &[Type::Ptr, Type::I64], None, host::slot_put as *const () as usize, flags);
        self.b.call_host(put, &[cell_slot, ep], None);
        self.b.br(l_join, &[z, f, raised]);
        self.b.switch_to(l_join);
        let ps = self.b.block_params(l_join);
        let (value, valid, e) = (ps[0], ps[1], ps[2]);
        let err = self.or(err, e);
        Lowered { value, valid, err }
    }

    /// An interpreter island: the root's `Program::islands` entry, its column arguments lowered as any site's, the
    /// host `gravity_expr::island`. A `CaseTest` / domain VALUE under the island while the emitter is inside a CASE
    /// / a domain coercion could be the outer form's (the island starts with none): refused by name.
    fn island(&mut self, id: NodeId) -> Result<Lowered, EmitError> {
        let kind = self.p.nodes[id.0 as usize].kind();
        let isl = *self.p.islands.iter().find(|i| i.root == id).ok_or(EmitError::Unsupported(kind))?;
        if self.casetest.is_some() || self.domainval.is_some() {
            let range = &self.p.nodes[isl.first.0 as usize..=id.0 as usize];
            if range.iter().any(|n| matches!(n, ExprNode::CaseTest)) {
                return Err(EmitError::Unsupported(Kind::CaseTestExpr));
            }
            if range.iter().any(|n| matches!(n, ExprNode::CoerceToDomainValue)) {
                return Err(EmitError::Unsupported(Kind::CoerceToDomainValue));
            }
        }
        let args = self.args(isl.site)?;
        self.call(isl.site, &args, false)
    }

    /// AND (`or_chain == false`) / OR of `ks` in order, three-valued, each operand evaluated where
    /// no earlier one decided.
    fn chain(&mut self, ks: &[NodeId], or_chain: bool) -> Result<Lowered, EmitError> {
        let mut alive = self.t;
        let mut decided = self.f;
        let mut any_null = self.f;
        let mut err = self.zero;
        for &k in ks {
            let go = self.go_fresh(alive, err); // D775: no operand past a raise
            let a = self.guarded(go, k)?;
            let tv = self.truth(a.value);
            let hit = if or_chain { self.and(a.valid, tv) } else {
                let ntv = self.not(tv);
                self.and(a.valid, ntv)
            };
            let nv = self.not(a.valid);
            let nl = self.and(alive, nv);
            any_null = self.or(any_null, nl);
            decided = self.or(decided, hit);
            err = self.first(err, a.err);
            let nh = self.not(hit);
            alive = self.and(alive, nh);
        }
        let value = if or_chain { decided } else { self.not(decided) };
        let nn = self.not(any_null);
        let valid = self.or(decided, nn);
        Ok(Lowered { value: self.word(value), valid, err })
    }

    /// D947 (GV-D5): PG evaluates a simple CASE's test before any arm (ExecInitExprRec, T_CaseExpr): its raise is the
    /// CASE's and no arm runs, whether an arm reads the test (the parser's shape) or not (a hand-built plan's).
    fn case(&mut self, id: NodeId, test_err: Value) -> Result<Lowered, EmitError> {
        let ExprNode::Case(c) = &self.p.nodes[id.0 as usize] else { unreachable!() };
        let whens = &self.p.kids[c.whens.start as usize..c.whens.start as usize + c.whens.len as usize];
        let mut alive = self.go_fresh(self.t, test_err);
        let mut arms = mcx::reserved_in(whens.len() / 2, self.row.scratch);
        for pair in whens.chunks_exact(2) {
            let cond = self.guarded(alive, pair[0])?;
            let tv = self.truth(cond.value);
            let hit0 = self.and(cond.valid, tv);
            let taken = self.and(alive, hit0);
            // D775: a WHEN that raised takes no arm and ends the CASE (PG raises there); its raise is carried by the arm
            // selected where it holds (`taken` is false there, so the default's select carries it)
            let taken = self.go_fresh(taken, cond.err);
            let res = self.arm(taken, pair[1])?;
            let nt = self.not(taken);
            alive = self.and(alive, nt);
            alive = self.go_fresh(alive, cond.err);
            let arm = (taken, Lowered { err: self.first(cond.err, res.err), ..res });
            arms.push_in(arm);
        }
        let mut acc = match c.default {
            Some(d) => self.arm(alive, d)?,
            None => self.null(),
        };
        for (taken, res) in arms.into_iter().rev() {
            acc = self.select3(taken, res, acc);
        }
        Ok(Lowered { err: self.first(test_err, acc.err), ..acc })
    }

    fn scalar_array(&mut self, n: &ScalarArrayOp) -> Result<Lowered, EmitError> {
        let (site, use_or) = (n.site, n.use_or);
        let call = &self.p.sites[site.0 as usize];
        let hosted = !prim::is_prim(self.p, call, false);
        let (k0, k1) = (self.p.kids[call.args.start as usize], self.p.kids[call.args.start as usize + 1]);
        let scalar = self.node(k0)?;
        if walked(self.p, n) {
            return self.array_walk(n, scalar, k1);
        }
        let mut err = scalar.err;
        let mut elems = PgVec::new_in(self.row.scratch);
        let arr_null = match self.plan.elems_of(k1) {
            Some((start, count, arr_null)) => {
                if count as usize > MAX_LIST {
                    return Err(EmitError::LongList);
                }
                for ci in start as usize..start as usize + count as usize {
                    let ev = self.not(self.row.consts_null[ci]);
                    elems.push_in(Lowered { value: self.row.consts[ci], valid: ev, err: self.zero });
                }
                arr_null
            }
            None => {
                // an ARRAY[...] operand (C ExecEvalArrayExpr): every element in order, after the scalar, before any
                // compare; the array itself is never NULL (`walked` takes any other operand)
                let ExprNode::Array(a) = &self.p.nodes[k1.0 as usize] else { return Err(EmitError::LongList) };
                let p = self.p;
                for &id in &p.kids[a.elements.start as usize..(a.elements.start + a.elements.len) as usize] {
                    let l = self.after(err, id)?;
                    err = self.first(err, l.err);
                    elems.push_in(Lowered { err: self.zero, ..l });
                }
                false
            }
        };
        if arr_null {
            // D1025 (GV-D6): PG evaluates the scalar before the NULL array's test, so its raise stands
            return Ok(Lowered { err, ..self.null() });
        }
        if elems.is_empty() {
            let v = if use_or { self.f } else { self.t };
            return Ok(Lowered { value: self.word(v), valid: self.t, err });
        }
        let mut alive = self.t;
        let mut decided = self.f;
        let mut any_null = self.f;
        for e in elems {
            // GV-D1: PG stops at the first decided element and at a raise; a hosted compare past either never runs (its
            // error would stay in the site's cell for the row's raise to report)
            let r = if hosted {
                let go = self.go_fresh(alive, err);
                self.call_when(go, site, &[scalar, e])?
            } else {
                self.call(site, &[scalar, e], false)?
            };
            let tv = self.truth(r.value);
            let hit0 = if use_or { self.and(r.valid, tv) } else {
                let ntv = self.not(tv);
                self.and(r.valid, ntv)
            };
            let hit = self.and(alive, hit0);
            let nv = self.not(r.valid);
            let nl = self.and(alive, nv);
            any_null = self.or(any_null, nl);
            decided = self.or(decided, hit);
            let re = self.mask_if(alive, r.err);
            err = self.first(err, re); // D775: PG's first raise
            let nh = self.not(hit);
            alive = self.and(alive, nh);
        }
        let value = if use_or { decided } else { self.not(decided) };
        let nn = self.not(any_null);
        let valid0 = self.or(decided, nn);
        let valid = if call.strict { self.and(valid0, scalar.valid) } else { valid0 };
        Ok(Lowered { value: self.word(value), valid, err })
    }

    /// A ScalarArrayOp whose array is walked (`walked`): the array evaluated after the scalar and never past its raise
    /// (D946: one that calls a host runs in its own block), then where neither raised one `host::shim` call over the site's
    /// cell holding the walk word (`host::saop_walk`), which walks the array and compares each element through that cell,
    /// PG's `ExecEvalScalarArrayOp` (its NULL, empty-array and strict rules; decided at the first true (ANY) / false (ALL)
    /// answer; a raise ends it, the error in the site's cell as any hosted call's).
    fn array_walk(&mut self, n: &ScalarArrayOp, scalar: Lowered, arr_id: NodeId) -> Result<Lowered, EmitError> {
        let (Some(cells), Some(region)) = (self.row.cells, self.row.region) else { return Err(EmitError::NoCells) };
        let arr = self.after(scalar.err, arr_id)?;
        let err = self.first(scalar.err, arr.err);
        let cell = self.site_cell(cells, n.site.0 as usize);
        let l_call = self.b.create_block(&[]);
        let l_join = self.b.create_block(&[Type::I64, Type::Bool, Type::I64]);
        let (z, f) = (self.zero, self.f);
        // D775: PG evaluates both operands first; a raise leaves before any compare
        let go = self.go_fresh(self.t, err);
        if go != self.t {
            self.b.cond_br(go, (l_call, &[]), (l_join, &[z, f, z]));
        } else {
            self.b.br(l_call, &[]);
        }
        self.b.switch_to(l_call);
        // the operands and their NULLs in the site's cell, the walk word in its `aux`: `host::shim` takes the call as a walk
        self.b.store(cell, CELL_ARGS, scalar.value);
        self.b.store(cell, CELL_ARGS + 8, arr.value);
        for (i, v) in [scalar.valid, arr.valid].into_iter().enumerate() {
            let nv = self.not(v);
            let byte = self.b.conv(ConvOp::Zext, Type::I8, nv);
            let at = self.cell_null(i);
            self.b.store(cell, at, byte);
        }
        let word = self.b.iconst(Type::I64, host::walk_word(n.elem, n.use_or));
        self.b.store(cell, CELL_AUX, word);
        let hf = HostFlags { may_error: true, pure: false, touches_memory: true, worker: false, state: StateMask::NONE };
        let h = self.b.declare_host_once("gravity_expr::shim", &[Type::Ptr, Type::Ptr], Some(Type::I64), host::shim as *const () as usize, hf);
        let status = self.b.call_host(h, &[cell, region], Some(Type::I64)).unwrap();
        let valid = self.b.icmp(IntCond::Eq, status, self.zero);
        let failed = self.b.icmp(IntCond::Slt, status, self.zero);
        let e = super::prim::raise(self.b, failed, gravity_abi::status::raise::HOST);
        let value = self.b.load(Type::I64, cell, CELL_OUT);
        self.b.br(l_join, &[value, valid, e]);
        self.b.switch_to(l_join);
        let ps = self.b.block_params(l_join);
        let (value, valid, e) = (ps[0], ps[1], ps[2]);
        let err = self.first(err, e);
        Ok(Lowered { value, valid, err })
    }

    /// A row-valued IS [NOT] NULL (C `ExecEvalRowNull`): a NULL composite is NULL (IS NULL true, IS NOT NULL false); a valid
    /// one is IS NULL when every field not dropped is NULL, IS NOT NULL when none is, through the node's strict site
    /// (nodes/field.rs `ROW_NULL_SITE`, the interpreter's walk), never called past the argument's raise. Before, only an
    /// island served it, and a subtree an island cannot carry (a SubPlan's composite, a whole-row `Var`) was refused.
    fn row_null_test(&mut self, n: &NullTestNode) -> Result<Lowered, EmitError> {
        let site = n.site.ok_or(EmitError::Unsupported(Kind::NullTest))?;
        let a = self.node(n.arg)?;
        let k = self.b.iconst(Type::I64, n.is_null as i64);
        let r = self.call(site, &[a, Lowered { value: k, valid: self.t, err: self.zero }], false)?;
        let value = self.b.select(a.valid, r.value, k);
        Ok(Lowered { value, valid: self.t, err: r.err })
    }

    /// The compare site over two values: tier A in line, else the site's call (shim or fmgr, by-reference arguments as
    /// datum words); the sign in `value`, the call's raise in `err`.
    fn compare(&mut self, site: SiteId, call: &Call, l: Lowered, r: Lowered) -> Result<Lowered, EmitError> {
        if let Some(o) = prim::emit(self.b, call.funcid, &[l.value, r.value]) {
            return Ok(Lowered { value: o.value, valid: self.t, err: self.zero });
        }
        self.call(site, &[l, r], false)
    }

    /// A RowCompare operand evaluated only where `go` holds (the first pair's always).
    fn pair_arg(&mut self, go: Value, id: NodeId) -> Result<Lowered, EmitError> {
        if go == self.t {
            return self.node(id);
        }
        self.guarded(go, id)
    }

    /// `compare` whose hosted road runs only where `go` holds (a tier-A compare is in line and cannot raise).
    fn compare_when(&mut self, go: Value, site: SiteId, call: &Call, l: Lowered, r: Lowered) -> Result<Lowered, EmitError> {
        if let Some(o) = prim::emit(self.b, call.funcid, &[l.value, r.value]) {
            return Ok(Lowered { value: o.value, valid: self.t, err: self.zero });
        }
        self.call_when(go, site, &[l, r])
    }

    fn row_compare(&mut self, cmptype: i32, largs: crate::nodes::Kids, rargs: crate::nodes::Kids, sites: u32) -> Result<Lowered, EmitError> {
        let mut alive = self.t;
        let mut null_seen = self.f;
        let mut cmp = self.zero;
        let mut err = self.zero;
        for i in 0..largs.len as usize {
            // GV-D1: PG evaluates a pair only while every earlier pair was equal and not NULL and nothing raised: a pair
            // past the one that decided never runs a host call, nor leaves an error in a site's cell
            let go = self.go_fresh(alive, err);
            let l = self.pair_arg(go, self.p.kids[largs.start as usize + i])?;
            let go_r = self.go_fresh(go, l.err);
            let r = self.pair_arg(go_r, self.p.kids[rargs.start as usize + i])?;
            let call = &self.p.sites[sites as usize + i];
            let go_c = self.go_fresh(go_r, r.err);
            let c = self.compare_when(go_c, SiteId(sites + i as u32), call, l, r)?;
            let both = self.and(l.valid, r.valid);
            let nb = self.not(both);
            let nul = self.and(alive, nb);
            null_seen = self.or(null_seen, nul);
            let nz = self.b.icmp(IntCond::Ne, c.value, self.zero);
            let dec0 = self.and(both, nz);
            let dec = self.and(alive, dec0);
            cmp = self.b.select(dec, c.value, cmp);
            let le0 = self.first(l.err, r.err);
            let le = self.first(le0, c.err);
            err = self.first(err, le);
            let stop = self.or(nul, dec);
            let ns = self.not(stop);
            alive = self.and(alive, ns);
        }
        let z = self.zero;
        let v = match cmptype {
            COMPARE_LT => self.b.icmp(IntCond::Slt, cmp, z),
            COMPARE_LE => self.b.icmp(IntCond::Sle, cmp, z),
            COMPARE_GE => self.b.icmp(IntCond::Sge, cmp, z),
            COMPARE_GT => self.b.icmp(IntCond::Sgt, cmp, z),
            _ => return Err(EmitError::Unsupported(Kind::RowCompareExpr)),
        };
        let valid = self.not(null_seen);
        Ok(Lowered { value: self.word(v), valid, err })
    }
}
