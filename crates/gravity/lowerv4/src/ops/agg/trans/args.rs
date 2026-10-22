//! Aggregate classes and arguments. Hooks / levers: H20. Budget 120 lines (files-extra.tsv, +25 % gate). Owner M1-FOLD.
//!
//! Split from plain.rs by responsibility (D267): join.mult's weighted test, an aggregate's argument list over the row
//! reaching the end (a column, or an expression compiled there) and its cells, and the strict transition's row test (the
//! class kind folding an aggregate moved on to class_of.rs). Registers nothing; every fold end reads it (keyed too,
//! through plain.rs).
use gravity_decompose::ops::shared::agg::AggDecl;
use gravity_decompose::ops::shared::agg_class::Class;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, Type, Value};
use mcx::{CollectIn, GrowIn, PgVec};
use types_nodes::primnodes::OUTER_VAR;

use crate::cell::{Cell, Nullable};
use crate::face::kinds::KindId;
use crate::frame::env::Env;
use crate::frame::px::{ExId, Px};
use crate::frame::row::RowVals;
use crate::admit::refusal::Reason;
use crate::effects::visible::agg;
use crate::rep::CellRef;

use crate::props::class_kind;

/// join.mult (H20): `a` folds `m` copies of a row exactly (counts, integer sums widened, numeric, extrema); a float sum's
/// product is not `m` rounded adds, and DISTINCT / ordered inputs are not copies.
pub(crate) fn weighted(a: &AggDecl<'_>) -> bool {
    matches!(class_kind(a), KindId::ClassWord | KindId::ClassNumPacked | KindId::ClassMinmaxCode) && a.mods.distinct == 0 && a.mods.order == 0
}

/// A `Bool` as the row count 0 / 1.
pub(crate) fn n_of(b: &mut FunctionBuilder<'_>, _env: &Env, ok: Value) -> Value {
    b.conv(gravity_ir::ConvOp::Zext, Type::I64, ok)
}

/// The first argument's datum and whether the row enters the transition: a strict transition skips a row with any NULL
/// argument; `count(*)` takes every row. Where only NULL-ness is read (`count(x)`, any later argument), a code is not
/// rendered (`null_only`).
pub(crate) fn args_ok(b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, args: &[CellRef<'_>]) -> (Value, Value) {
    let mut ok = b.bconst(true);
    let mut first = env.zero;
    let t = b.bconst(true);
    for (i, &c) in args.iter().enumerate() {
        let (d, null) = match null_only(b, c).filter(|_| i > 0 || a.class == Class::CountAny) {
            Some(null) => (env.zero, null),
            None => {
                let mut cx = env.lx(b, env.node);
                let v = crate::rep::datum(&mut cx, c, &crate::step::expr::tf_datum(c.cell.typid));
                env.memo = cx.memo;
                v
            }
        };
        let v = b.int_bin(IntBinOp::Xor, null, t);
        ok = b.int_bin(IntBinOp::And, ok, v);
        if i == 0 {
            first = d;
        }
    }
    (first, ok)
}

/// Whether cell `c` is NULL, as `args_ok` reads a count's argument (a code cell's without its render).
pub(crate) fn null_of(b: &mut FunctionBuilder<'_>, env: &mut Env, c: CellRef<'_>) -> Value {
    if let Some(null) = null_only(b, c) {
        return null;
    }
    let mut cx = env.lx(b, env.node);
    let (_, null) = crate::rep::datum(&mut cx, c, &crate::step::expr::tf_datum(c.cell.typid));
    env.memo = cx.memo;
    null
}

/// A code cell's NULL-ness without its render (a count over a dictionary-coded text column rendered every row): its
/// render is NULL only on a NULL row or, off its lane, a record's none word (`MINCODE_NONE`); any other cell `None`
/// (its datum is read).
fn null_only(b: &mut FunctionBuilder<'_>, c: CellRef<'_>) -> Option<Value> {
    let gravity_program::Rep::Code { .. } = c.cell.rep else { return None };
    let null = c.null.unwrap_or_else(|| b.bconst(false));
    if matches!(c.cell.at, crate::cell::At::Lane(_)) {
        return Some(null);
    }
    let none = b.iconst(Type::I64, gravity_abi::breaker::MINCODE_NONE as i64);
    let is_none = b.icmp(gravity_ir::IntCond::Eq, c.val, none);
    Some(b.int_bin(IntBinOp::Or, null, is_none))
}

/// An aggregate argument: a column of the row reaching the end, or an expression over it.
#[derive(Clone, Copy)]
pub(crate) enum Arg {
    Col(i16),
    Ex(ExId, gravity_program::ExprOut),
}

/// `a`'s ORDER BY entries as the plain fold routes them: none when the ORDER BY is order-free (V4R-178).
pub(crate) fn ordered(a: &AggDecl<'_>) -> u8 {
    if agg::order_free(a, false) { 0 } else { a.mods.order }
}

/// aggregates#251, the plain fold's half: call `a`'s ORDER BY entries that are not bare columns or constants, compiled over
/// the rows reaching step `j` of pipeline `k`, where only `order_free`'s `evaluated` frees the call: the fold steps it
/// unordered and evaluates each per row past its FILTER, dropping the value, as the keyed end does (table/rec.rs `discard`).
pub(crate) fn discards<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, j: usize, a: &AggDecl<'p>, node: i32) -> PgVec<'g, ExId> {
    let mcx = px.mcx();
    let entries = a.node.as_aggref().filter(|_| freed(a)).into_iter().flat_map(|r| r.args.iter()).filter_map(|t| t.as_target_entry());
    entries.filter(|t| discarded(t)).map(|t| crate::step::expr::compile_at(px, k, j, t.expr, None, node)).collect_in(mcx)
}

/// D4703: whether `discards` gives `a` an entry, so the plain end walks every row (plain.rs `every_row`), unasked to compile.
pub(crate) fn discarding(a: &AggDecl<'_>) -> bool {
    freed(a) && a.node.as_aggref().is_some_and(|r| r.args.iter().filter_map(|t| t.as_target_entry()).any(|t| discarded(t)))
}

fn freed(a: &AggDecl<'_>) -> bool {
    a.mods.order != 0 && !agg::order_free(a, false) && agg::order_free(a, true) && !crate::ops::shared::agg::sorted(a)
}

fn discarded(t: &types_nodes::primnodes::TargetEntry<'_>) -> bool {
    t.resjunk && t.expr.as_var().is_none() && t.expr.as_const().is_none()
}

/// Whether aggregate argument `t` is one the transition reads: an ORDER BY entry of its own (resjunk) is not (#509: a NULL
/// sort key never skips the row).
pub(crate) fn read_arg(t: types_nodes::Node<'_>) -> bool {
    t.as_target_entry().is_none_or(|t| !t.resjunk)
}

/// Expression `n` compiled as `Expr::compile_scan` does, every aggregate call a cell of the lowerer's own fold (the
/// Builder's `folded`: only `Agg::cell_gate`), with the aggorder gate lifted when every aggregate call in `n` with an
/// ORDER BY is order-free (its cell is the unordered fold's: plain.rs `ordered`, group_rec; a keyed end's with its
/// entries evaluated, `order_free`'s `evaluated`, the plain fold's too: `discards`) or host-sorted (D458,
/// src/regroup.rs `sorted_ref`), and a RETURNING list's
/// OLD / NEW Vars read `returning`'s sources (M4-RET R4a, V4R-286). memsig E-prog: the program's lists are built in `m`
/// (`Program::empty_in`): the lowering's lists root for a program it may host (`Px::lists_mcx`), else its planning context.
pub(crate) fn compile_scan<'p>(m: mcx::Mcx<'p>, n: types_nodes::Node<'p>, catalog: &dyn gravity_expr::Catalog<'p>, outer: Option<(i32, &types_nodes::list::NodeList<'p>)>, tableoid: Option<(i32, gravity_expr::TableOid)>, returning: Option<gravity_expr::nodes::Returning>) -> gravity_expr::CompileResult<gravity_expr::Compiled<'p>> {
    let (mut any, mut free) = (false, true);
    gravity_decompose::expr::walk(n, &mut |x| {
        if let Some(r) = x.as_aggref().filter(|r| !r.aggorder.is_nil()) {
            (any, free) = (true, free && (agg::order_free_ref(r, true) || crate::ops::agg::trans::args::sorted_ref(r)));
        }
        true
    });
    let (mut prog, mut state) = (gravity_expr::Program::empty_in(m), gravity_expr::State::empty());
    let root = gravity_expr::nodes::Builder { prog: &mut prog, state: &mut state, catalog, outer, tableoid, aggorder: any && free, returning, folded: true }.compile(n)?;
    let result_type = gravity_decompose::catalog::expr_type::result_type(n);
    Ok(gravity_expr::Compiled { prog, state, root, params: types_portal::params::ParamBind::NONE, callback_arg: core::ptr::null_mut(), result_type })
}

/// `a`'s argument list over pipeline `k`'s row (the rows reaching step `j`).
pub(crate) fn args_of<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, j: usize, a: &AggDecl<'p>, node: i32) -> PgVec<'g, Arg> {
    let Some(r) = a.node.as_aggref() else { crate::refuse!(node, Reason::Bug { payload: "an aggregate that is not an Aggref" }) };
    let gx = px.gx;
    let mut out = PgVec::new_in(px.mcx());
    let sort_cols = crate::ops::shared::agg::sort_cols(a); // D458: the host sorts by the ORDER BY's resjunk columns too
    for t in r.args.iter().filter(|&t| sort_cols || read_arg(t)) {
        let e = t.as_target_entry().map_or(t, |t| t.expr);
        out.push_in(match crate::step::project::bare_var(e).filter(|_| e.as_var().is_none_or(|v| v.varno == OUTER_VAR)) {
            Some(c) => Arg::Col(c),
            None => Arg::Ex(crate::step::expr::compile_at(px, k, j, e, None, node), crate::step::project::out_of(gx, e, node)),
        });
    }
    out
}

/// Each argument's cell over `row` (an expression emitted here), then `f` over them.
pub(crate) fn with_args<T>(b: &mut FunctionBuilder<'_>, env: &mut Env, node: i32, row: &RowVals, args: &[Arg], f: impl FnOnce(&mut FunctionBuilder<'_>, &mut Env, &[CellRef<'_>]) -> T) -> T {
    let t = b.bconst(true);
    let mut own: PgVec<'_, (Cell, Value, Option<Value>)> = mcx::reserved_in(args.len(), env.mcx);
    for a in args {
        own.push_in(match *a {
            Arg::Col(c) => match row.col(c) {
                Some(r) => (*r.cell, r.val, r.null),
                None => crate::refuse!(node, Reason::Bug { payload: "an aggregate argument the row does not carry" }),
            },
            Arg::Ex(id, out) => {
                let (v, ok) = crate::step::expr::emit(b, env, node, row, id);
                let null = b.int_bin(IntBinOp::Xor, ok, t);
                (Cell::of_expr(out, 0, 0, Nullable::Maybe), v, Some(null))
            }
        });
    }
    let refs: PgVec<'_, CellRef<'_>> = own.iter().map(|(c, v, n)| CellRef { cell: c, val: *v, null: *n }).collect_in(env.mcx);
    f(b, env, &refs[..])
}

/// D458: whether call `r` folds host-sorted in its ORDER BY's order (decompose's `OrderedFold`: an order-sensitive class,
/// no DISTINCT; `sorted` judges the decl, the ends refuse by name what it does not take): fold/args.rs lifts the aggorder gate.
pub fn sorted_ref(r: &types_nodes::primnodes::Aggref<'_>) -> bool {
    let c = gravity_decompose::ops::agg::trans::table::row(r.aggfnoid).map_or(Class::User, |w| w.class); // decompose agg.rs `cell`: no row = User
    r.aggdistinct.is_nil() && c.order_sensitive() && !matches!(c, Class::OrderedSet(_) | Class::Hypothetical(_))
}
