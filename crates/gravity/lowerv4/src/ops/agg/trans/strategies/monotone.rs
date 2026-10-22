//! Road `expr.monotone` (cards/M3.md L2, M3 decision 5): the lowerer side of the monotone argument rewrite for MIN / MAX /
//! ORDER BY f(x) (face/rewrite.rs `Rewrite`). Owner M3-WORDS.
//!
//! The road applies wherever a pipeline orders by an expression or folds MIN / MAX over one (props `order_expr` /
//! `minmax_expr`). The rewrite serves only where PostgreSQL's result is unchanged: f strict, immutable and monotone in x over
//! its whole domain, by FACTS' monotone table (decompose catalog/monotone.rs: function OID, argument, direction, affine).
//! Every applicable site of the road takes the card's G4 form: f(x) is evaluated per row and the fold / order keys read its
//! value, as the M1 / M2 folds and tops already do. That is PostgreSQL's own evaluation, so registering the road serves
//! the statements it names at their other kinds' forms.
//!
//! PL-20 (the first reader of the table): an answered window (answer.rs, road `answer.stats`) folds `MIN / MAX(f(x, c))`
//! (PL-20 generalized: or a chain `f1(..fn(x)..)` of such steps, `DEPTH` at most, each step's raise of one result type)
//! of one scan column `x` and constants from x's record, by the table's reader contract: f at the record's min AND max
//! (it raises iff either does, the raising x forming tails), the result f(min) or f(max) by direction; a window no row
//! qualifies gives the identity with no call. The path holds only where nothing else per row raises (answer_site.rs: the
//! filters are zone predicates, the other aggregates read bare columns, every raising f has one result type: one message).
//!
//! Registers: road `expr.monotone` (`EXPR_MONOTONE`), the per-row form (D472 open for ORDER BY / Span).
use gravity_abi::ctx::answer;
use gravity_abi::status::raise;
use gravity_decompose::catalog::monotone::{monotone, Monotone};
use gravity_decompose::ops::shared::agg::{AggDecl, Register as R};
use gravity_expr::emit::prim::Out;
use gravity_fn::typesem::integral::int_width;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value};

use gravity_lower::Place;
use mcx::{GrowIn, Mcx, PgVec};
use types_nodes::Node;

use crate::frame::env::Env;
use crate::frame::px::Px;
use crate::ops::Table;

crate::strategies! {
    /// Monotone argument rewrite for MIN / MAX / ORDER BY f(x).
    EXPR_MONOTONE = 74, "expr.monotone", ExprOrder;
}

pub(crate) const ROWS: Table = Table { strategies: &[(EXPR_MONOTONE, &())], ..crate::ops::NONE };

/// The longest chain `f1(f2(..fn(x)..))` PL-20 reads (a composition of monotone steps is monotone: its direction the
/// product of theirs, and each raising step raises on a tail of its input, which a monotone inner maps from a tail of x,
/// so the chain raises only on tails of x: the table's reader contract holds for the chain).
pub(crate) const DEPTH: usize = 4;

/// One step of a monotone chain: f's OID, its chained argument's position, the constant arguments (that slot `None`),
/// the table's fact, f's result type.
#[derive(Clone, Copy)]
pub(crate) struct Step<'p> {
    fnoid: u32,
    pos: usize,
    consts: [Option<Node<'p>>; 2],
    nargs: usize,
    fact: Monotone,
    ty: u32,
}

/// `MIN / MAX(f1(..fn(x, c)..))`: the chain's steps innermost first (`n` of them), the composed fact, x's column in the
/// aggregate's input row, the aggregate's result type, and the one result type of the chain's raising steps (PL-20's
/// one-message rule, answer_site.rs; `None`: no step raises).
#[derive(Clone, Copy)]
pub(crate) struct Mono<'p> {
    /// The argument expression: the root its per-row compile files the constants' words under (D2958).
    expr: Node<'p>,
    steps: [Option<Step<'p>>; DEPTH],
    pub(crate) fact: Monotone,
    pub(crate) col: i16,
    pub(crate) ty: u32,
    pub(crate) raise_ty: Option<u32>,
}

/// A `Mono` whose constants are words bound per execution (`Px::root_const`: a by-value literal is a word, never the
/// program's key, so another literal reuses the program): each step's constant slots' words and lengths.
#[derive(Clone, Copy)]
pub(crate) struct Bound {
    steps: [Option<Link>; DEPTH],
    fact: Monotone,
}

/// A bound step: the step's call with its constants' words (`(word, length)` per slot).
#[derive(Clone, Copy)]
struct Link {
    fnoid: u32,
    pos: usize,
    nargs: usize,
    fact: Monotone,
    ty: u32,
    words: [(u16, i32); 2],
}

/// One step of `e`: a tier-A f the monotone table lists in the argument the chain fills (the one argument that is not
/// a non-NULL by-value integer constant; a NULL one the planner folds to a NULL constant first; a Param is not taken),
/// and that argument.
fn step<'p>(e: Node<'p>) -> Option<(Step<'p>, Node<'p>)> {
    let (fnoid, fargs, ty) = match (e.as_op_expr(), e.as_func_expr()) {
        (Some(o), _) => (o.opfuncid, &o.args, o.opresulttype),
        (_, Some(f)) => (f.funcid, &f.args, f.funcresulttype),
        _ => return None,
    };
    let (mut x, mut consts) = (None, [None; 2]);
    if fargs.len() == 0 || fargs.len() > 2 {
        return None;
    }
    for (i, v) in fargs.iter().enumerate() {
        match v.as_const() {
            Some(c) if !c.constisnull && c.constbyval && matches!(c.constlen, 2 | 4 | 8) => consts[i] = Some(v),
            Some(_) => return None,
            None if x.is_none() => x = Some((i, v)),
            None => return None,
        }
    }
    let (pos, inner) = x?;
    let fact = monotone(fnoid, pos)?;
    let tier_a = gravity_fn::row_of(fnoid).is_some_and(|row| row.tier == gravity_fn::Tier::A);
    tier_a.then_some((Step { fnoid, pos, consts, nargs: fargs.len(), fact, ty }, inner))
}

/// `a` is an integer MIN / MAX whose one argument is a chain of up to `DEPTH` monotone steps (`step`) over a bare
/// column, every raising step of one result type.
pub(crate) fn arg<'p>(a: &AggDecl<'p>, mcx: Mcx<'_>) -> Option<Mono<'p>> {
    if !matches!(a.reg, R::MinInt | R::MaxInt) {
        return None;
    }
    let r = a.node.as_aggref().filter(|r| r.args.len() == 1)?;
    let t = r.args.iter().next()?;
    let e = t.as_target_entry().map_or(t, |t| t.expr);
    let mut outer: PgVec<'_, Step<'p>> = mcx::reserved_in(DEPTH, mcx);
    let mut cur = e;
    let col = loop {
        if let Some(c) = (!outer.is_empty()).then(|| crate::step::project::bare_var(cur)).flatten() {
            break c;
        }
        let (s, inner) = step(cur).filter(|_| outer.len() < DEPTH)?;
        outer.push_in(s);
        cur = inner;
    };
    let mut steps = [None; DEPTH];
    for (slot, s) in steps.iter_mut().zip(outer.iter().rev()) {
        *slot = Some(*s);
    }
    let increasing = outer.iter().filter(|s| !s.fact.increasing).count() % 2 == 0;
    let fact = Monotone { increasing, affine: outer.iter().all(|s| s.fact.affine), raises: outer.iter().any(|s| s.fact.raises) };
    let mut raising = outer.iter().filter(|s| s.fact.raises).map(|s| s.ty);
    let raise_ty = raising.next();
    if raising.any(|t| Some(t) != raise_ty) {
        return None;
    }
    Some(Mono { expr: e, steps, fact, col, ty: r.aggtype, raise_ty })
}

/// `a`'s `Mono` with each constant's word placed ahead of the argument's per-row compile (plain.rs, before `args_of`),
/// which reads the same word: one word per occurrence (the argument's root and the literal), bound per execution. D2958:
/// each arm of a union makes this end over the one aggregate, so the word is the occurrence's (`Px::root_const`), never
/// a claim the first arm's compile takes and a later arm's leaves behind.
pub(crate) fn bind<'p>(px: &mut Px<'_, 'p>, a: &AggDecl<'p>) -> Option<Bound> {
    let m = arg(a, px.mcx())?;
    let mut steps = [None; DEPTH];
    for (slot, s) in steps.iter_mut().zip(m.steps.iter()) {
        let Some(s) = s else { break };
        let mut words = [(0u16, 0i32); 2];
        for (w, c) in words.iter_mut().zip(s.consts.iter()) {
            if let Some(c) = *c {
                let Place::Word(k) = px.root_const(m.expr, c)? else { return None };
                *w = (k, c.as_const()?.constlen);
            }
        }
        // the bound form keeps no plan node: the constants are its words
        *slot = Some(Link { fnoid: s.fnoid, pos: s.pos, nargs: s.nargs, fact: s.fact, ty: s.ty, words });
    }
    Some(Bound { steps, fact: m.fact })
}

/// Constant word `k` of length `len` this execution, sign-extended.
fn word(b: &mut FunctionBuilder<'_>, env: &Env, (k, len): (u16, i32)) -> Value {
    let off = 8 * k as i32;
    match len {
        2 | 4 => {
            let v = b.load(if len == 2 { Type::I16 } else { Type::I32 }, env.params, off);
            b.conv(ConvOp::Sext, Type::I64, v)
        }
        _ => b.load(Type::I64, env.params, off),
    }
}

/// An answered window's carry words `w` of `MIN` (`min`) or `MAX(f(x, c))` from x's answer record `rec`: the folded word
/// (the identity when no row qualifies) and `nonnull`.
pub(crate) fn answer(b: &mut FunctionBuilder<'_>, env: &mut Env, m: &Bound, min: bool, rec: Value, w: &mut [Value]) {
    fires::fire("answer.mono"); // shapecheck fire signal (REGISTRY PL-20)
    let nonnull = b.load(Type::I64, rec, answer::NONNULL as i32);
    let any = b.icmp(IntCond::Ne, nonnull, env.zero);
    let (lo, hi) = (b.load(Type::I64, rec, answer::MIN as i32), b.load(Type::I64, rec, answer::MAX as i32));
    let v = fold(b, env, m, min, lo, hi, any);
    let id = b.iconst(Type::I64, if min { i64::MAX } else { i64::MIN });
    w[0] = b.select(any, v, id);
    let k = w.len() - 1;
    w[k] = nonnull;
}

/// The window's `MIN` (`min`) or `MAX` word of the chain from x's record extremes `lo` / `hi` when `any` row qualifies:
/// the chain at both, raising (the function's raise block) iff any step raises at either; its value at lo or hi by the
/// chain's direction.
fn fold(b: &mut FunctionBuilder<'_>, env: &mut Env, m: &Bound, min: bool, lo: Value, hi: Value, any: Value) -> Value {
    let mut err = env.zero;
    let mut at = |b: &mut FunctionBuilder<'_>, env: &mut Env, mut x: Value| {
        for s in m.steps.iter().flatten() {
            let mut v = [env.zero; 2];
            for (i, c) in v.iter_mut().enumerate().take(s.nargs).filter(|(i, _)| *i != s.pos) {
                *c = word(b, env, s.words[i]);
            }
            v[s.pos] = x;
            let Some(o) = gravity_expr::emit::prim::emit(b, s.fnoid, &v[..s.nargs]).or_else(|| cast(b, s, x)) else {
                crate::refuse!(env.node, crate::admit::refusal::Reason::Bug { payload: "a monotone f without its tier-A form" })
            };
            err = b.int_bin(IntBinOp::Or, err, o.err);
            x = o.value;
        }
        x
    };
    let flo = at(b, env, lo);
    let fhi = at(b, env, hi);
    let err = b.select(any, err, env.zero);
    let raised = b.icmp(IntCond::Ne, err, env.zero);
    let l_ok = b.create_block(&[]);
    b.cond_br(raised, (env.l_raise, &[err]), (l_ok, &[]));
    b.switch_to(l_ok);
    if min == m.fact.increasing { flo } else { fhi }
}

/// A one-argument increasing row the tier-A forms do not name is an integer cast (int48, int84): one that never raises
/// widens (x passes on); one that raises narrows to the result's width, out of range raising that width's error.
fn cast(b: &mut FunctionBuilder<'_>, m: &Link, x: Value) -> Option<Out> {
    if m.nargs != 1 || !m.fact.increasing {
        return None;
    }
    let zero = b.iconst(Type::I64, 0);
    if !m.fact.raises {
        return Some(Out { value: x, err: zero });
    }
    let (ty, code) = match int_width(m.ty)? {
        2 => (Type::I16, raise::INT2_RANGE),
        4 => (Type::I32, raise::INT4_RANGE),
        _ => return None,
    };
    let t = b.conv(ConvOp::Trunc, ty, x);
    let back = b.conv(ConvOp::Sext, Type::I64, t);
    let bad = b.icmp(IntCond::Ne, back, x);
    let code = b.iconst(Type::I64, raise::bit(code) as i64);
    Some(Out { value: x, err: b.select(bad, code, zero) })
}

#[cfg(test)]
mod tests {
    use datum::Datum;
    use gravity_decompose::{NoFacts, PipelineGraph, PipelineNode, Source};
    use gravity_lower::{Place, Sequential};
    use gravity_program::{Arena, End, Pipeline};
    use mcx::MemoryContext;
    use types_nodes::Node;

    use crate::frame::px::Px;
    use crate::graph::Gx;

    /// D2958: every arm of a union makes the plain end over the one `MIN(1 - id)`, so `bind` and the argument's compile run
    /// once per arm. The constant's word is its occurrence's (the argument's root, the literal): the compile reads it, a
    /// later arm's bind and compile read it again, no second word is placed and no claim is left for `take_exs` to refuse
    /// ("an unconsumed literal claim": the first arm's compile took the claim, the second arm's found its word filed).
    #[test]
    fn each_arm_reads_the_occurrence_word_and_leaves_no_claim() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let cx = MemoryContext::new("monotone");
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let mut g = PipelineGraph::new(a);
        g.pipelines.push(PipelineNode::new(a, 7, Source::Const, Pipeline::serial(-1, End::Sink, gravity_program::Reason::NoDrivingSite)));
        let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
        let mut plan = Sequential::new(a);
        let mut px = Px::new(a, &gx, &mut plan);
        let root: Node<'_> = Node::mk_const(cx.mcx(), 23, -1, 0, 4, Datum::from_i64(7), false, true).expect("a root node");
        let one: Node<'_> = Node::mk_const(cx.mcx(), 23, -1, 0, 4, Datum::from_i64(1), false, true).expect("a const node");
        let compiled = |_px: &mut Px<'_, '_>| -> Place { panic!("the compile placed a second word") };
        let w = px.root_const(root, one);
        assert!(matches!(w, Some(Place::Word(_))), "{w:?}");
        let n = px.plan.nwords();
        for arm in 0..3 {
            if arm > 0 {
                assert_eq!(px.root_const(root, one), w, "arm {arm}'s bind reads the occurrence's word");
            }
            assert_eq!(Some(px.lit_word(root, one, u32::MAX, compiled)), w, "arm {arm}'s compile reads the same word");
        }
        assert_eq!(px.plan.nwords(), n, "one word for the occurrence");
        assert!(crate::catch(0, || drop(px.take_exs())).is_ok(), "no claim left behind");
    }
}
