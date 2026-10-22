//! step/expr.rs's expression extension seams (M2-L0, M2 decision 1; face/step.rs `ExprExtMech`, `VolatileMech`): the claim
//! an extension makes at compile (after percode / text.frame decline, or before `emit_check` for what it refuses: a
//! whole-row `Var`, `= ANY` over an array that is not a constant), the plan nodes a claim takes out of the literals'
//! placement, the operands it compiles as expressions of their own, its sites at emit, and a volatile expression's held
//! words. With nothing registered each path ends in the refusal step/expr.rs made before. Budget 120 lines. Owner M1-FRAME.
use std::any::Any;
use std::rc::Rc;

use gravity_expr::{Compiled, ExprNode};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::Value;
use mcx::{CollectIn, GrowIn, Mcx, PgVec};
use types_nodes::Node;

use crate::step::expr::Outer;
use crate::face::kinds::KindId;
use crate::face::step::{Claim, ExprExtMech, ExtPlan, EXT_AT};
use crate::frame::env::Env;
use crate::frame::px::{ExId, Px};
use crate::frame::row::RowVals;

/// One expression's claim: the plan nodes it took (their literals are not placed) and its operands (`sub`), in order;
/// `sets`: `pre_claim` was left to the per-code road (C06, dict/percode.rs `sets_first`).
pub(crate) struct Claims<'g, 'p> {
    pub skip: PgVec<'g, usize>,
    pub subs: PgVec<'g, Node<'p>>,
    pub sets: bool,
}

impl<'g, 'p> Claims<'g, 'p> {
    pub(crate) fn new(mcx: Mcx<'g>) -> Self {
        Claims { skip: PgVec::new_in(mcx), subs: PgVec::new_in(mcx), sets: false }
    }
}

/// The first of `exts` that claims `c` (compiled from `node`): its `ExtPlan` as the `Ex::ext`, its nodes into `cl`.
pub(crate) fn attach<'p>(px: &mut Px<'_, 'p>, at: (usize, usize), node: Node<'p>, c: &mut Compiled<'p>, ex: ExId, cl: &mut Claims<'_, 'p>, exts: impl Iterator<Item = &'static dyn ExprExtMech>) -> Option<Rc<dyn Any>> {
    claim(px, at, node, c, ex, cl, exts).map(|x| Rc::new(x) as Rc<dyn Any>)
}

fn claim<'p>(px: &mut Px<'_, 'p>, at: (usize, usize), node: Node<'p>, c: &mut Compiled<'p>, ex: ExId, cl: &mut Claims<'_, 'p>, mut exts: impl Iterator<Item = &'static dyn ExprExtMech>) -> Option<ExtPlan> {
    let node = node.as_target_entry().map_or(node, |te| te.expr);
    exts.find_map(|mech| {
        let subs = &mut cl.subs;
        let claim = mech.attach(px, at, node, c, ex, &mut |s| {
            subs.push_in(s);
            subs.len() as u32 - 1
        });
        let Some(Claim { plan, skip }) = claim else {
            cl.subs.clear();
            return None;
        };
        cl.skip.extend_in(skip);
        Some(ExtPlan { mech, plan, subs: Default::default() })
    })
}

/// D2997: the extensions that also claim over a per-code plan's residual (what its roots left read per row): num.cmp's
/// compare alone, whose claim skips no plan node (its literal stays under the CASE), so the per-code plan's placement holds.
static RESIDUAL: [(Option<crate::face::strategy::Strategy>, crate::ops::Mech); 1] = [(Some(crate::face::strategy::NUM_SHORT), crate::rep::varlena::cmp_num::ROWS.strategies[0].1)];

/// D2997: num.cmp's claims over `c` after the per-code road rewrote it (step/expr.rs `compile_at`), for its `Plan::ext`.
/// A per-code expression reads its scan columns from the lanes, so each claimed placeholder is an `OUTER` column, read from
/// the spliced site values as the rest of the row's datums are.
pub(crate) fn residual<'p>(px: &mut Px<'_, 'p>, at: (usize, usize), node: Node<'p>, c: &mut Compiled<'p>, ex: ExId, cl: &mut Claims<'_, 'p>) -> Option<ExtPlan> {
    let x = claim(px, at, node, c, ex, cl, crate::face::ext::exts_in(&RESIDUAL))?;
    for n in c.prog.nodes.iter_mut() {
        if let ExprNode::Var(v) = n {
            if (EXT_AT..crate::frame::site::SHRED_AT).contains(&v.attno) {
                v.src = gravity_expr::Source::Outer;
            }
        }
    }
    Some(x)
}

/// The kind `c` needs an extension for before `emit_check`: a whole-row `Var` (first, as before), then a `= ANY` / `ALL`
/// whose array gravity_expr would walk (emit/scalar.rs `walked`; a one-dimension ARRAY[...] of at most `MAX_LIST`
/// elements it unrolls in line, PS4-08 / PS4-16), a constant PG's plan hashed (not read: V4R-102) or a constant of more
/// than `MAX_LIST` elements.
pub(crate) fn pre_kind(c: &Compiled<'_>, node: Node<'_>, mcx: Mcx<'_>) -> Option<KindId> {
    let p = &c.prog;
    let hashed = hashed_in(node, mcx);
    let array = |s: &gravity_expr::nodes::op::ScalarArrayOp| p.kids[p.sites[s.site.0 as usize].args.start as usize + 1].0 as usize;
    let whole = p.nodes.iter().any(|n| matches!(n, ExprNode::Var(v) if v.attno == 0));
    // a constant past MAX_LIST too (V4R-SAOP): its elements would take a word each before a later claim could skip them;
    // its count is its class, keyed, not a bake (V4R-110)
    let long = |s: &gravity_expr::nodes::op::ScalarArrayOp| match &p.nodes[array(s)] {
        ExprNode::Const(k) => k.list_count(s.elem).is_some_and(|n| n > gravity_program::cost::expr::MAX_LIST),
        _ => gravity_expr::emit::scalar::walked(p, s),
    };
    let mut at = 0;
    let saop = p.nodes.iter().any(|n| match n {
        ExprNode::ScalarArrayOp(s) => {
            at += 1;
            (matches!(p.nodes[array(s)], ExprNode::Const(_)) && hashed.get(at - 1).copied().unwrap_or(false)) || long(s)
        }
        _ => false,
    });
    [(whole, KindId::ExprWholerow), (saop, KindId::ExprSaop)].into_iter().find(|w| w.0).map(|w| w.1)
}

/// Whether PG's plan hashed each ScalarArrayOpExpr of `node` (`hashfuncid`), in walk order: the program's (step/expr_saop.rs
/// pairs them the same way; a nested one declines there).
fn hashed_in<'g>(node: Node<'_>, mcx: Mcx<'g>) -> PgVec<'g, bool> {
    let mut out = PgVec::new_in(mcx);
    gravity_decompose::expr::walk(node, &mut |x| {
        if let Some(s) = x.as_scalar_array_op_expr() {
            out.push_in(s.hashfuncid != 0);
        }
        true
    });
    out
}

/// `pre_kind`'s claim: `NotBuilt` by the kind's name until its extension is registered.
pub(crate) fn pre_claim<'p>(px: &mut Px<'_, 'p>, at: (usize, usize), node: Node<'p>, c: &mut Compiled<'p>, ex: ExId, cl: &mut Claims<'_, 'p>, id: i32) -> Option<Rc<dyn Any>> {
    let k = pre_kind(c, node, px.mcx())?;
    // a long constant text `= ANY` expr.saop declines is road text.eq's (V4R-67): tried here, before a per-code sweep takes it
    let roads = (k == KindId::ExprSaop).then(crate::face::step::expr_exts).into_iter().flatten();
    attach(px, at, node, c, ex, cl, std::iter::once(crate::frame::iface::ext_impl(&id, k).0).chain(roads))
}

/// A volatile call's held words (`Ex::held`): `NotBuilt(expr.volatile)` until its mechanism is registered.
pub(crate) fn held(px: &mut Px<'_, '_>, at: (usize, usize), c: &Compiled<'_>, ex: ExId, id: i32) -> Vec<i32> {
    let h = crate::frame::iface::volatile_impl(&id).0.plan(px, at, c, ex);
    if h.words == 0 {
        return Vec::new(); // alloc: Program
    }
    crate::frame::stop::held_words(px, ex, h.words.max(2))
}

/// The claimed operands, compiled after their expression `ex` over the same row, into its `ExtPlan::subs`; a literal keeps
/// its word under `root` (the expression's), never a second one under its operand (D390: exec admit.rs's template).
pub(crate) fn compile_subs<'p>(px: &mut Px<'_, 'p>, ex: ExId, subs: PgVec<'_, Node<'p>>, outer: Outer<'_, 'p>, id: i32, root: Node<'p>, rw: (usize, usize)) {
    let ids: Vec<ExId> = subs.into_iter().map(|n| crate::step::expr::compile_under_rw(px, n, outer, id, root, rw)).collect(); // alloc: Program
    if let Some(x) = ExtPlan::of(px.placed(ex)) {
        *x.subs.borrow_mut() = ids;
    }
}

/// Expression `id`'s claimed sites over the row, `(placeholder attno, [value, valid, err])`: an extension's `sub(i)` is its
/// operand `i` through `emit` (step/expr.rs `value3`). None when they are emitted at their read (V4R-342, expr_ext_lazy.rs).
pub(crate) fn sites<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, node: i32, row: &RowVals, id: ExId, emit: fn(&mut FunctionBuilder<'_>, &mut Env, i32, &RowVals, ExId) -> [Value; 3]) -> PgVec<'e, (i16, [Value; 3])> {
    let mcx = env.mcx;
    let e = env.placed(id);
    let Some(x) = ExtPlan::of(e).filter(|_| !crate::step::expr_ext_lazy::on(env, id)) else { return PgVec::new_in(mcx) };
    let subs = mcx::to_vec_in(&x.subs.borrow()[..], mcx);
    let mut sub = |b: &mut FunctionBuilder<'_>, env: &mut Env, i: u32| emit(b, env, node, row, subs[i as usize]);
    // D3875: a shred lane's column (`SHRED_AT + n`, a char_length lane under a claimed CASE) is the row's, not a site
    let at = e.c.prog.nodes.iter().filter_map(|n| if let ExprNode::Var(v) = n { crate::face::ext::ext_site(v.attno).then_some(v.attno) } else { None });
    at.map(|a| (a, x.mech.emit(b, env, row, id, (a - EXT_AT) as u32, &mut sub))).collect_in(mcx)
}

/// The row's columns `(values, valids)` with each site's value at its placeholder attno.
pub(crate) fn splice(sv: &mut PgVec<'_, Value>, sn: &mut PgVec<'_, Value>, sites: &[(i16, [Value; 3])], zero: Value) {
    for &(a, [v, ok, _]) in sites {
        let n = sv.len().max(a as usize);
        (sv.resize_in(n, zero), sn.resize_in(n, ok));
        (sv[a as usize - 1], sn[a as usize - 1]) = (v, ok);
    }
}

/// Each site's raise word by its placeholder attno (`RowRefs::col_raises`), a constant 0 left out. GV-D1: the placeholder's
/// read carries it, so it takes the site's place in PG's order and a branch PG never takes masks it (an or'ed word raised
/// ahead of an earlier error and under a CASE arm never reached).
pub(crate) fn raises<'g>(b: &FunctionBuilder<'_>, sites: &[(i16, [Value; 3])], mcx: Mcx<'g>) -> PgVec<'g, (i16, Value)> {
    sites.iter().filter(|s| !b.const_of(s.1[2]).is_some_and(|(_, c)| c == 0)).map(|s| (s.0, s.1[2])).collect_in(mcx)
}
