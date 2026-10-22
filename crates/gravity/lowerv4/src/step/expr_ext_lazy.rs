//! V4R-342 (D945): an expression extension's claimed sites emitted where the expression reads them (gravity_expr
//! `RowRefs::lazy`), never ahead of it, when an operand holds a VOLATILE call. step/expr_ext.rs `sites` evaluates a
//! claim's operands before its expression on every row, so a nextval under a CASE arm PG never takes (or past a raise PG
//! stops at) ran anyway. Here the placeholder's read emits the site, and gravity_expr guards the read as a host call
//! (only on the rows PG reaches it, once). The operands' calls keep the pipeline on the leader (source/heap.rs,
//! source/pgrc.rs: `hir::pool_unsafe_site` makes it `Parallel::Ineligible`). A claim whose operands hold no VOLATILE call
//! is emitted ahead, as before. `twice` (V4R-351, the per-code guard narrowed): a per-code root whose table can fault is
//! read rendered when a VOLATILE call is evaluated before it. Budget 107 lines. Owner M1-FRAME (V4R-342, v4-correctness).
use std::cell::RefCell;

use gravity_expr::emit::{LazySites, RowRefs};
use gravity_expr::nodes::SiteId;
use gravity_expr::percode::{Form, Known, Plan as PcPlan, Root};
use gravity_expr::{Compiled, ExprNode};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::Value;
use gravity_lower::Place;
use mcx::{CollectIn, PgVec};

use crate::face::step::{ExtPlan, EXT_AT};
use crate::frame::env::{AggRow, Env};
use crate::frame::px::ExId;
use crate::frame::row::RowVals;
use crate::admit::refusal::{Reason, ShapeWhy};

type Emit = fn(&mut FunctionBuilder<'_>, &mut Env, i32, &RowVals, ExId) -> [Value; 3];

/// M8 B1: expression `id`'s InitPlan parameters `(id, handle word)` (params.rs `init_of`: its tagged places).
pub(crate) fn inits<'e>(env: &Env<'e, '_>, id: ExId) -> PgVec<'e, (i32, u16)> {
    env.placed(id).places.iter().filter_map(|p| if let Place::Exec(x) = *p { crate::rerun::params::init_of(x) } else { None }).collect_in(env.mcx)
}

/// Whether `id`'s claimed sites are emitted at their read: an operand of its claim holds a VOLATILE call or an InitPlan
/// parameter.
pub(crate) fn on(env: &Env, id: ExId) -> bool {
    ExtPlan::of(env.placed(id)).is_some_and(|x| x.subs.borrow().iter().any(|&s| crate::effects::visible::expr::volatile(env, s)))
}

/// V4R-351 (main 06:16Z): whether per-code root `r` would run a VOLATILE call a second time. Its table can fault (a LIKE
/// verdict or a program body; lengths and code forms never do), and step/expr.rs `emit_row`'s TABLE path re-evaluates the
/// whole expression where it does; a call evaluated after the root sits behind its raise (gravity_expr `after`), one
/// evaluated before it has run already.
pub(crate) fn twice(c: &Compiled<'_>, plan: &PcPlan, r: &Root) -> bool {
    let faults = matches!(r.form, Form::Bits | Form::Words) && r.table.is_some_and(|t| matches!(plan.needs[t as usize].known, Known::Like { .. } | Known::LikeBound { .. } | Known::Body | Known::EqBound { .. } | Known::RangeBound { .. }));
    faults && first_volatile(c).is_some_and(|v| v < r.node.0 as usize)
}

/// The first node calling a VOLATILE function, in evaluation order (kids before parents, arguments in order); a VOLATILE
/// site no node here names (an island's, a row comparison's) counts from the first node.
fn first_volatile(c: &Compiled<'_>) -> Option<usize> {
    let p = &c.prog;
    let vol = |s: SiteId| p.sites.get(s.0 as usize).is_some_and(|x| x.volatile && !gravity_expr::emit::prim::is_prim(p, x, false));
    let site = |n: &ExprNode| match n {
        ExprNode::Func(f) | ExprNode::Op(f) | ExprNode::Distinct(f) | ExprNode::NullIf(f) => Some(f.site),
        ExprNode::ScalarArrayOp(s) => Some(s.site),
        _ => None,
    };
    let named = p.nodes.iter().filter_map(site).filter(|&s| vol(s)).count();
    let first = p.nodes.iter().position(|n| matches!(n, ExprNode::NextValue(_)) || site(n).is_some_and(vol));
    if named < (0..p.sites.len() as u32).filter(|&i| vol(SiteId(i))).count() { Some(0) } else { first }
}

/// `RowRefs::lazy` of `id` over `row`: its extension's site `attno - EXT_AT`, with `env` lent (the aggregate row back in
/// it, the column memo fresh: what the site reads in its block never serves a read outside it).
struct Lazy<'r, 'g, 'e, 'p> {
    env: RefCell<&'r mut Env<'e, 'p>>,
    at: (i32, &'g RowVals<'g>, ExId),
    aggs: &'g Option<AggRow<'e>>,
    emit: Emit,
    /// The claimed sites are emitted here (`on`).
    sites: bool,
    /// M8 B1: the InitPlan parameters, loaded where their Param is read (nest/initplan.rs `InitLoad`).
    inits: PgVec<'e, (i32, u16)>,
    /// M8 B2: the SubPlans the expression reads, as their step lent them (`Env::subplans`).
    subs: PgVec<'e, (i32, i32, i32, usize)>,
    /// D2997: the columns only a claim's fallback arm reads, `(inner, attno)` (step/expr.rs `fallback_cols`).
    cols: PgVec<'e, (bool, i16)>,
}

/// M8 B2: the SubPlans expression `id` reads, as a step of this row wrote them (`Env::subplans`); one no step of the row
/// evaluated refuses (gravity_expr would read an aggregate cell in its place).
fn subs<'e>(env: &Env<'e, '_>, node: i32, id: ExId) -> PgVec<'e, (i32, i32, i32, usize)> {
    let p = &env.placed(id).c.prog;
    let held = |pid: i32| env.subplans.iter().find(|s| s.0 == pid).copied().unwrap_or_else(|| crate::refuse!(node, Reason::Shape { why: ShapeWhy("a SubPlan read where no step of its row evaluated it") }));
    p.nodes.iter().filter_map(|n| if let ExprNode::SubPlan(q) = n { Some(held(q.plan_id)) } else { None }).collect_in(env.mcx)
}

impl LazySites for Lazy<'_, '_, '_, '_> {
    fn has(&self, attno: i16) -> bool {
        self.sites && crate::face::ext::ext_site(attno)
    }

    fn has_col(&self, src: gravity_expr::Source, attno: i16) -> bool {
        self.cols.contains(&(src == gravity_expr::Source::Inner, attno))
    }

    fn emit_col(&self, b: &mut FunctionBuilder<'_>, src: gravity_expr::Source, attno: i16) -> [Value; 3] {
        let (node, row, _) = self.at;
        let env = &mut **self.env.borrow_mut();
        crate::step::expr::lazy_col(b, env, node, row, src == gravity_expr::Source::Inner, attno)
    }

    fn has_param(&self, id: i32) -> bool {
        self.inits.iter().any(|x| x.0 == id)
    }

    fn emit_param(&self, b: &mut FunctionBuilder<'_>, id: i32) -> [Value; 3] {
        let node = self.at.0;
        let Some(&(_, h)) = self.inits.iter().find(|x| x.0 == id) else { crate::refuse!(node, Reason::Bug { payload: "a lazy parameter the hook does not hold" }) };
        let env = &mut **self.env.borrow_mut();
        (crate::frame::iface::init_load(&node).0)(b, env, node, h, id)
    }

    fn has_subplan(&self, plan_id: i32) -> bool {
        self.subs.iter().any(|x| x.0 == plan_id)
    }

    fn emit_subplan(&self, b: &mut FunctionBuilder<'_>, plan_id: i32) -> [Value; 3] {
        let node = self.at.0;
        let Some(&(_, vo, fo, lent)) = self.subs.iter().find(|x| x.0 == plan_id) else { crate::refuse!(node, Reason::Bug { payload: "a SubPlan the hook does not hold" }) };
        let env = &mut **self.env.borrow_mut();
        if lent != 0 {
            let memo = env.memo.set_aside(); // B2c: evaluated here; its block's reads serve no read outside it
            (crate::frame::iface::sub_eval(&node).0)(b, env, lent, self.at.1);
            env.memo.restore(memo);
        }
        let (v, flags) = (env.load_word(b, vo), env.load_word(b, fo));
        let null = b.iconst(gravity_ir::Type::I64, crate::rerun::params::NULL);
        let n = b.int_bin(gravity_ir::IntBinOp::And, flags, null);
        let valid = b.icmp(gravity_ir::IntCond::Eq, n, env.zero);
        [v, valid, env.zero]
    }

    fn emit_subplan_held(&self, b: &mut FunctionBuilder<'_>, plan_id: i32) -> [Value; 3] {
        use crate::rerun::params::{HELD, NULL};
        let node = self.at.0;
        let Some(&(_, vo, fo, lent)) = self.subs.iter().find(|x| x.0 == plan_id) else { crate::refuse!(node, Reason::Bug { payload: "a SubPlan the hook does not hold" }) };
        if lent == 0 || !(crate::frame::iface::sub_holds(&node).0)(lent) {
            return self.emit_subplan(b, plan_id);
        }
        let env = &mut **self.env.borrow_mut();
        let memo = env.memo.set_aside();
        env.hold_rows = true;
        (crate::frame::iface::sub_eval(&node).0)(b, env, lent, self.at.1);
        env.hold_rows = false;
        env.memo.restore(memo);
        // a held second row: a NULL whose word is the mark the interpreter raises at (gravity_expr nodes/srf.rs `eval_subplan`)
        let (v, flags) = (env.load_word(b, vo), env.load_word(b, fo));
        let off = b.iconst(gravity_ir::Type::I64, NULL | HELD);
        let n = b.int_bin(gravity_ir::IntBinOp::And, flags, off);
        let valid = b.icmp(gravity_ir::IntCond::Eq, n, env.zero);
        let h = b.iconst(gravity_ir::Type::I64, HELD);
        let h = b.int_bin(gravity_ir::IntBinOp::And, flags, h);
        let held = b.icmp(gravity_ir::IntCond::Ne, h, env.zero);
        let mark = b.iconst(gravity_ir::Type::I64, gravity_expr::nodes::srf::HELD_ROWS as i64);
        let v = b.select(held, mark, v);
        [v, valid, env.zero]
    }

    fn emit(&self, b: &mut FunctionBuilder<'_>, attno: i16) -> [Value; 3] {
        let (node, row, id) = self.at;
        let env = &mut **self.env.borrow_mut();
        let Some(x) = ExtPlan::of(env.placed(id)) else { crate::refuse!(node, Reason::Bug { payload: "a lazy site without its claim" }) };
        let subs = mcx::to_vec_in(&x.subs.borrow()[..], env.mcx);
        let (memo, aggs) = (env.memo.set_aside(), std::mem::replace(&mut env.aggs, self.aggs.clone()));
        let f = self.emit;
        let v = x.mech.emit(b, env, row, id, (attno - EXT_AT) as u32, &mut |b: &mut FunctionBuilder<'_>, env: &mut Env, i: u32| f(b, env, node, row, subs[i as usize]));
        env.memo.restore(memo);
        env.aggs = aggs;
        v
    }
}

/// `f` over `refs`, `id`'s claimed sites emitted at their read when `on`; `env` handed back. A placeholder an interpreter
/// island reads is laid into its row before the island runs, ahead of the island's own guards: refused by name (an
/// InitPlan parameter only under a conditional of the island).
#[allow(clippy::type_complexity)]
pub(crate) fn run<'r, 'g, 'e, 'p, T>(b: &mut FunctionBuilder<'_>, env: &'r mut Env<'e, 'p>, at: (i32, &'g RowVals, ExId), aggs: &'g Option<AggRow<'e>>, refs: RowRefs<'_>, emit: Emit, cols: PgVec<'e, (bool, i16)>, f: impl FnOnce(&mut FunctionBuilder<'_>, &RowRefs<'_>) -> T) -> (T, &'r mut Env<'e, 'p>) {
    let (sites, inits, subs) = (on(env, at.2), inits(env, at.2), subs(env, at.0, at.2));
    if !sites && inits.is_empty() && subs.is_empty() && cols.is_empty() {
        return (f(b, &refs), env);
    }
    let p = &env.placed(at.2).c.prog;
    let held = |i: &gravity_expr::nodes::Island| p.nodes[i.first.0 as usize..=i.root.0 as usize].iter().any(|n| matches!(n, ExprNode::Var(v) if sites && crate::face::ext::ext_site(v.attno)));
    if p.islands.iter().any(held) {
        crate::refuse!(at.0, Reason::Shape { why: ShapeWhy("a claimed operand holding a VOLATILE call inside an interpreter island") });
    }
    // an island's InitPlan parameter is an argument of its call, loaded where the island is reached: where PG reaches it
    // too unless the island evaluates part of its subtree conditionally (CASE, COALESCE, AND / OR, a row comparison's
    // early stop, JSON_VALUE / JSON_QUERY's DEFAULT ON ERROR / EMPTY that is no constant), which keeps the refusal. A
    // SubPlan is an argument the same way (json-0033, gravity_expr nodes/mod.rs `island`): read before the island runs,
    // so the same refusal, unless it holds (D3374, `SubHolds`: lent, no effect, no raise but its second row, which the
    // island raises only where it reads the SubPlan): then running it on a row PG skips is not visible.
    let konst = |b: &gravity_expr::nodes::json::Behavior| b.expr.map_or(true, |e| matches!(p.nodes[e.0 as usize], ExprNode::Const(_)));
    let cond_holding = |i: &gravity_expr::nodes::Island, held: &dyn Fn(&ExprNode) -> bool| {
        let range = &p.nodes[i.first.0 as usize..=i.root.0 as usize];
        let cond = |n: &ExprNode| match n {
            ExprNode::Case(_) | ExprNode::Coalesce(_) | ExprNode::Bool(_) | ExprNode::RowCompare(_) => true,
            ExprNode::Json(gravity_expr::nodes::json::JsonNode::Expr(e)) => !konst(&e.on_error) || e.on_empty.as_ref().is_some_and(|b| !konst(b)),
            _ => false,
        };
        range.iter().any(held) && range.iter().any(cond)
    };
    if p.islands.iter().any(|i| cond_holding(i, &|n| matches!(n, ExprNode::Param(q) if inits.iter().any(|x| x.0 == q.id)))) {
        crate::refuse!(at.0, Reason::Shape { why: ShapeWhy("an InitPlan parameter under a conditional inside an interpreter island") });
    }
    let holds = |q: &gravity_expr::nodes::srf::SubPlan| subs.iter().find(|x| x.0 == q.plan_id).is_some_and(|x| x.3 != 0 && (crate::frame::iface::sub_holds(&at.0).0)(x.3));
    if p.islands.iter().any(|i| cond_holding(i, &|n| matches!(n, ExprNode::SubPlan(q) if !holds(q)))) {
        crate::refuse!(at.0, Reason::Shape { why: ShapeWhy("a SubPlan inside an interpreter island that evaluates part of its subtree conditionally") });
    }
    // a per-code expression may be emitted twice per row (step/expr.rs `emit3`'s rendered retry): a lazy SubPlan once
    if subs.iter().any(|s| s.3 != 0) && crate::step::expr::plan_of(env, at.2).is_some() {
        crate::refuse!(at.0, Reason::Shape { why: ShapeWhy("a lazy SubPlan in a per-code expression") });
    }
    let lazy = Lazy { env: RefCell::new(env), at, aggs, emit, sites, inits, subs, cols };
    let out = f(b, &RowRefs { lazy: Some(&lazy), ..refs });
    (out, lazy.env.into_inner())
}
