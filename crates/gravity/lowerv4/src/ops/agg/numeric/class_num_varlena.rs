//! Kind `class.num_varlena` (`ClassImpl`: numeric folds with no decided scale; L4) (cards/M6.md, M6-changes.md §1). Owner
//! M6-FOLD.
//!
//! sum / avg / min / max of numeric whose scale decompose could not decide: PG's own transition by fmgr through
//! face/aggcall.rs (`numeric_avg_accum` into its internal NumericAggState, `numeric_smaller` / `numeric_larger` over a
//! copied extreme), the group's state in its `AggState` slot (fold/slots.rs, V4R-277) and the statement's aggregate arena,
//! the finals by fmgr (`numeric_sum`, `numeric_avg`). That is the straight form (D2); main 09-26 took fmgr's transition
//! over the cards' `m6_num_accum` host (M6-plan §6). It folds in a plain end at width 1 (the arena clamps the program);
//! a keyed end refuses the hosted slot by name until Decision 6 (combine and spill), and so does a merge of two states.
//! Its fold (`FOLD`) is every hosted class's straight form: class.stats registers it too (M6-FOLD R2).
use gravity_decompose::ops::shared::agg::{AggDecl, NONE};
use gravity_decompose::catalog::typefacts;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, Type, Value};
use gravity_kernels::ops::agg::trans::hosted::{DIRECT_BOUND, DIRECT_NULL};
use mcx::{CollectIn, GrowIn, PgVec};

use crate::ops::agg::trans::aggcall as call;
use crate::face::fold::{Acc, AccShape, ClassImpl, FoldClass};
use crate::face::kinds::KindId;
use crate::frame::env::Env;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::ops::Table;
use crate::rep::CellRef;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::ClassNumVarlena, &ClassImpl(&FOLD))], ..crate::ops::NONE };

pub(crate) struct Hosted;

/// The hosted fold: the aggregate's own transfn / finalfn by fmgr, one `AggState` per group (fold/hosted.rs).
pub(crate) static FOLD: Hosted = Hosted;

/// D2597: a float variance's row (class.stats: float8_accum, float8_regr_accum) whose finite input could overflow in some
/// order raises where a raise re-runs the statement in plan order (`facts::order::rechecked`), a NaN met first or not,
/// so the plan-order run answers as PG (class_float's `STAT_RISK`). A row with a NULL argument is the strict transfn's skip.
fn risky(b: &mut FunctionBuilder<'_>, env: &Env, a: &AggDecl<'_>, args: &[CellRef<'_>], vals: &[(Value, Value)]) {
    use crate::ops::agg::trans::float_pg::{arg64, big, not, raise_ovf};
    use gravity_decompose::ops::shared::agg_class::Class;
    if !matches!(a.class, Class::Stat3F64(_) | Class::Stat6F64(_)) || !gravity_decompose::facts::order::rechecked() {
        return;
    }
    let (mut ok, mut at) = (b.bconst(true), b.bconst(false));
    for (c, &(v, null)) in args.iter().zip(vals) {
        let x = arg64(b, c.cell.typid, v);
        let hi = big(b, x, crate::ops::agg::trans::class_float::STAT_RISK);
        let some = not(b, null);
        at = b.int_bin(IntBinOp::Or, at, hi);
        ok = b.int_bin(IntBinOp::And, ok, some);
    }
    let bad = b.int_bin(IntBinOp::And, at, ok);
    raise_ovf(b, env, bad);
}

/// The group's state slot and the arena's word. A decl with a slot of decompose's own is D297's wide form over a packed
/// register (its slot is no `AggState`), refused by name.
fn state(b: &mut FunctionBuilder<'_>, env: &Env, a: &AggDecl<'_>, acc: Acc<'_>) -> (Value, u16) {
    if a.slot != NONE {
        crate::refuse!(env.node, Reason::Shape { why: ShapeWhy("class.num_varlena over a packed register's slot (D297's wide form)") });
    }
    let Acc::Slot { base, off, .. } = acc else { crate::refuse!(env.node, Reason::Bug { payload: "a hosted aggregate state in registers" }) };
    let Some(word) = env.agg_arena else { crate::refuse!(env.node, Reason::Bug { payload: "a hosted fold without its end's aggregate arena" }) };
    (b.ptr_add(base, env.zero, 1, off), word)
}

impl FoldClass for Hosted {
    fn shape(&self, _a: &AggDecl<'_>) -> AccShape {
        call::SHAPE
    }

    /// A zeroed slot is a group the host has not initialized (its first call applies the initcond); registers hold none.
    fn init(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>) {
        if let Acc::Slot { .. } = acc {
            let (p, _) = state(b, env, a, acc);
            b.store(p, 0, env.zero);
            b.store(p, 8, env.zero);
        }
    }

    /// One row: the arguments' datums into the aggregate's transfn (strictness and the by-reference copy are the host's).
    fn step(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>, args: &[CellRef<'_>]) {
        let (p, word) = state(b, env, a, acc);
        let mut vals: PgVec<'_, (Value, Value)> = mcx::reserved_in(args.len(), env.mcx);
        for &c in args {
            let tf = typefacts::row_of(c.cell.typid).copied().unwrap_or(typefacts::opaque(c.cell.typid, c.cell.typid, -1, false, false));
            let mut cx = env.lx(b, env.node);
            vals.push_in(crate::rep::datum(&mut cx, c, &tf));
            env.memo = cx.memo;
        }
        risky(b, env, a, args, &vals[..]);
        call::step(b, env, word, a.aggno as i32, p, &vals[..]);
    }

    /// The carry's registers hold no hosted state (a plain end's window end merges nothing); two slots' states have no
    /// combine here yet (Decision 6).
    fn merge(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env, _a: &AggDecl<'_>, _into: Acc<'_>, from: Acc<'_>) {
        if let Acc::Slot { .. } = from {
            crate::refuse!(env.node, Reason::Shape { why: ShapeWhy("two hosted aggregate states merged (Decision 6: the combinefn form)") });
        }
    }

    /// The finalfn over the group's state (the transition value itself without one, as min / max); NULL over no row, as PG's.
    /// An end with no group row (plain, a grouping set's empty set) finalizes an ordered-set aggregate's constant direct
    /// arguments only: the host's bound ones, a NULL one given (D1520: a set's nulled key, decompose agg.rs `arm_node`).
    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>) -> (Value, Value) {
        let (p, word) = state(b, env, a, acc);
        match consts(env, a) {
            None => call::finish(b, env, word, a.aggno as i32, p),
            Some(g) => {
                let d: PgVec<'_, (Value, Value)> = g.into_iter().map(|g| (env.zero, b.iconst(Type::I64, g as i64))).collect_in(env.mcx);
                call::finish_direct(b, env, word, a.aggno as i32, p, &d[..])
            }
        }
    }

    /// D1371: the group's run sorted and folded into its state ahead of the group's finals (kernels `m6_agg_fold`).
    fn fold_ahead(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>) {
        let (p, word) = state(b, env, a, acc);
        call::fold(b, env, word, a.aggno as i32, p);
    }

    /// D1520: the finalfn given the direct arguments a keyed end evaluated over the group's row (keyed/stream.rs `finals_of`).
    fn finish_direct(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>, direct: &[(Value, Value)]) -> (Value, Value) {
        let (p, word) = state(b, env, a, acc);
        call::finish_direct(b, env, word, a.aggno as i32, p, direct)
    }
}

/// D1520: how an end with no group row gives `a`'s direct arguments (kernels `DIRECT_*`): `None` when each is a non-NULL
/// constant (the host's bound ones), else a NULL one given NULL; one that is no constant refuses by name (V4R-425's name:
/// PG evaluates it over the group's row, which only a keyed end has).
pub(crate) fn consts<'e>(env: &Env<'e, '_>, a: &AggDecl<'_>) -> Option<PgVec<'e, u8>> {
    let r = a.node.as_aggref()?;
    if r.aggdirectargs.iter().any(|n| n.as_const().is_none()) {
        crate::refuse!(env.node, Reason::Shape { why: ShapeWhy("an ordered-set aggregate whose direct argument is no constant (PG evaluates it per group)") });
    }
    let null = |n: types_nodes::Node<'_>| n.as_const().is_some_and(|c| c.constisnull);
    r.aggdirectargs.iter().any(null).then(|| r.aggdirectargs.iter().map(|n| if null(n) { DIRECT_NULL } else { DIRECT_BOUND }).collect_in(env.mcx))
}
