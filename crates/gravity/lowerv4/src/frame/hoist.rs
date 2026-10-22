//! Road `hoist` (H35, R9-L06): loop-invariant words computed once at their scope, not per row (TPDE has no LICM). An
//! expression's constants are its placed words (step/consts.rs `place`: loaded from the bind image) and its folded literals
//! (immediates), as `load` emits them. A step lists its expressions' scopes (`StepMech::hoist`) and the frame emits them at
//! each window's start (`Window`). A step reads them through `words`: the hoisted words when the road put them there, else the same words
//! emitted in place (the road off, or not applying). Only loads of the bind image and immediates are hoisted: they are the
//! call's invariants (so no step needs a `Chain` scope), and neither can raise, so no hoisted value is computed where PG
//! would not compute it. Budget 160 lines.
use gravity_expr::emit::{ColsRef, ParamNeed, RowRefs};
use gravity_expr::NodeId;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntCond, Type, Value};
use gravity_lower::Place;
use mcx::{CollectIn, GrowIn, PgVec};

use crate::face::step::{Hoist, HoistAt, HoistImpl, HoistMech, Hoisted};
use crate::frame::env::Env;
use crate::frame::px::ExId;
use crate::admit::refusal::Reason;

crate::strategies! {
    /// Loop-invariant words hoisted (TPDE has no LICM).
    LOOP_HOIST = 7, "hoist", Loop;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(LOOP_HOIST, &HoistImpl(&HOIST))], ..crate::ops::NONE };

struct Words;
static HOIST: Words = Words;

impl HoistMech for Words {
    fn emit(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, at: HoistAt, h: &Hoist) {
        for &(a, e) in h.words.iter() {
            if a != at || env.hoisted(e).is_some() || env.placed(e).places.is_empty() {
                continue;
            }
            // M8 L0-b: an executor parameter changes between re-opens and calls; it is read where the row reads it. D3033:
            // but an expression with late operands reads only nest parameters (`late_roots`), whose producer writes them
            // before the source (re-)opens, so they hold still across a window
            let late = at == HoistAt::Window && env.placed(e).late.is_some();
            if !late && env.placed(e).places.iter().any(|p| matches!(p, Place::Exec(_))) {
                continue;
            }
            let (words, nulls) = load(b, env, env.node, e);
            let late = if late { emit_late(b, env, e, &words, &nulls) } else { PgVec::new_in(env.mcx) };
            env.hoisted.push_in(Hoisted { ex: e, words, nulls, late });
        }
    }
}

/// D3033 (gravfix2's diagnosis of D3018): expression `e`'s operands over literals and parameters alone, which the row would
/// compute per row (a `DATE '1992-01-01' + m.lo` bound: `date_pli` per row, ~10 ns x the inner's rows per rescan), at its
/// hoist scope over its words `(words, nulls)`. Each keeps its raise word: the row reads it where it reaches the operand
/// (`RowRefs::late`), so PG's raise (an overflowing bound) surfaces at the first row that evaluates it, never on an empty
/// input or past a guard. An operand the emitter refuses here is left to the row.
fn emit_late<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, e: ExId, words: &[Value], nulls: &[Value]) -> PgVec<'e, (NodeId, [Value; 3])> {
    let mcx = env.mcx;
    let x = env.placed(e);
    let refs = RowRefs::scan(ColsRef::Lanes { row: env.zero, lanes: &[] }, words, nulls, env.mcx);
    let refs = env.hir.with_ex(b, x, refs);
    let late: PgVec<'e, _> = x.late.iter().flat_map(|(_, roots)| roots.iter()).filter_map(|&n| x.c.emit_at(b, &refs, n).ok().map(|l| (n, [l.value, l.valid, l.err]))).collect_in(mcx);
    if late.iter().any(|&(n, _)| leaf_images(&x.c.prog, n)) {
        fires::fire("hoist.late.leaf_images"); // shapecheck fire signal (D3108)
    }
    late
}

/// D3108: late root `n` (or the call a relabel wraps) is a by-value shim over leaf images (`host::leaf_images`), not a
/// direct one.
fn leaf_images(p: &gravity_expr::Program<'_>, n: NodeId) -> bool {
    crate::stack_check();
    use gravity_expr::ExprNode;
    match &p.nodes[n.0 as usize] {
        ExprNode::Relabel(r) => leaf_images(p, r.arg),
        ExprNode::Func(f) | ExprNode::Op(f) => {
            let c = &p.sites[f.site.0 as usize];
            gravity_expr::emit::host::leaf_images(p, c) && gravity_expr::emit::host::direct(p, c).is_none()
        }
        _ => false,
    }
}

/// D3033: placed expression `e`'s late operands (`Compiled::late_roots`): its subtrees over literals, bind parameters and
/// nest parameters alone whose calls are in line or by-value direct shims of functions pg_proc marks IMMUTABLE or STABLE
/// (`effects::of_call`, fail closed), so the value is the same on every row between two writes of the parameters. None
/// when the expression reads any other executor parameter (an InitPlan's runs at its first read, M8 B1; a SubPlan's
/// changes per row of its walk), holds a VOLATILE call, a per-code plan or held words. Construct C2: the operands run
/// ahead of PostgreSQL's per-row evaluation with their error held until a row reaches them, so they hold
/// `effects::held_error`'s licence: every call passed `call_ok` (no VOLATILE call, none `of_call` reads as acting) and
/// `Compiled::late_roots` admits built-in shims and primitives only (no user code), so the operands may raise and do
/// nothing else. `None` with no late operand.
pub(crate) fn late_roots(gx: &crate::graph::Gx<'_, '_>, e: &crate::hir::Ex<'_>) -> Option<(gravity_decompose::effects::Licence, Vec<NodeId>)> {
    use gravity_expr::ExprNode;
    use types_nodes::primnodes::ParamKind;
    if e.volatile || e.ext.is_some() || !e.held.is_empty() {
        return None;
    }
    let nest = |id: i32| matches!(crate::rerun::params::producer(gx, id), Some(crate::face::kinds::KindId::NestParam));
    if e.c.prog.nodes.iter().any(|n| matches!(n, ExprNode::Param(q) if q.kind == ParamKind::PARAM_EXEC && !nest(q.id))) {
        return None;
    }
    let param_ok = |q: &gravity_expr::nodes::param::Param| q.kind == ParamKind::PARAM_EXTERN || q.kind == ParamKind::PARAM_EXEC && nest(q.id);
    let call_ok = |c: &gravity_expr::nodes::func::Call| c.funcid != 0 && !c.volatile && !gravity_decompose::effects::of_call(gx.facts, c.funcid).acts;
    let roots = e.c.late_roots(&param_ok, &call_ok, gx.mcx);
    if roots.is_empty() {
        return None;
    }
    Some((gravity_decompose::effects::held_error(gravity_decompose::effects::Effects::RAISES, false, None).ok()?, roots))
}

/// Expression `id`'s constants `(words, nulls)` in need order (`load`): the hoisted ones when road `hoist` computed them,
/// else emitted here.
pub(crate) fn words<'e>(b: &mut FunctionBuilder<'_>, env: &Env<'e, '_>, node: i32, id: ExId) -> (PgVec<'e, Value>, PgVec<'e, Value>) {
    match env.hoisted(id) {
        Some(h) => (mcx::to_vec_in(&h.words[..], env.mcx), mcx::to_vec_in(&h.nulls[..], env.mcx)),
        None => load(b, env, node, id),
    }
}

/// Expression `id`'s constants as cells: a placed word loaded from the bind image with its null byte, a folded literal as
/// an immediate. `(values, nulls)` in need order.
fn load<'e>(b: &mut FunctionBuilder<'_>, env: &Env<'e, '_>, node: i32, id: ExId) -> (PgVec<'e, Value>, PgVec<'e, Value>) {
    let e = env.placed(id);
    let mut v = mcx::reserved_in(e.places.len(), env.mcx);
    let mut n = mcx::reserved_in(e.places.len(), env.mcx);
    for (p, need) in e.places.iter().zip(e.needs.iter()) {
        match (*p, need) {
            (Place::Word(w), _) => {
                v.push_in(b.load(Type::I64, env.params, w as i32 * 8));
                let nb = b.load_zext(Type::I8, env.params_null, w as i32);
                n.push_in(b.icmp(IntCond::Ne, nb, env.zero));
            }
            // D1035: an InitPlan's value is read by its Param through `InitLoad` alone (its m8_init_run pins the reader to the
            // leader): its word here is a NULL no read takes, never the frame pair, which a thread holds only once it ran it
            (Place::Exec(id), _) if crate::rerun::params::init_of(id).is_some() => {
                v.push_in(env.zero);
                n.push_in(b.bconst(true));
            }
            // M8 L0-b: its producer wrote it (a nest before re-opening its inner, a SubPlan before its call)
            (Place::Exec(id), _) => {
                let (x, null) = crate::rerun::params::load(b, env, node, id);
                v.push_in(x);
                n.push_in(null);
            }
            (_, ParamNeed::Exec { .. }) => crate::refuse!(node, Reason::Bug { payload: "an executor parameter placed outside its words" }),
            (_, ParamNeed::Const(d) | ParamNeed::Elem(d)) => {
                // folded into the code: only a by-value literal's word (D684: step/consts.rs refuses a by-reference literal
                // or element past the word budget, whose address the plan would free before the program)
                if !d.isnull {
                    gravity_decompose::bake::note(d.value.as_usize());
                }
                v.push_in(b.iconst(Type::I64, d.value.as_i64()));
                n.push_in(b.bconst(d.isnull));
            }
            _ => crate::refuse!(node, Reason::Bug { payload: "a folded need that is not a literal" }),
        }
    }
    (v, n)
}
