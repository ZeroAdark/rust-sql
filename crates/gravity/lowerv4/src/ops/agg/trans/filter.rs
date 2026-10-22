//! Aggregate FILTER. Hooks / levers: -. Budget 250 lines (files.tsv, +25 % gate). Owner M1-FOLD.
//!
//! Registers: kind `fold.filter` (`KindId::FoldFilter`): an aggregate's `FILTER (WHERE …)` gating its transition, as
//! nodeAgg evaluates it: the predicate first, over the aggregate's input row; a NULL or false predicate skips the row, and
//! the arguments are evaluated only for a passing row (so an argument's error is raised only where PG raises it, D237).
//! Every fold end (plain, keyed) folds through `step_if`; a filtered state steps in registers or its slot alike.
use gravity_decompose::ops::shared::agg::AggDecl;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::Value;
use mcx::{CollectIn, PgVec};

use crate::face::fold::Acc;
use crate::face::kinds::KindId;
use crate::frame::env::Env;
use crate::frame::px::{ExId, Px};
use crate::frame::row::RowVals;

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { kinds: &[(KindId::FoldFilter, &FoldFilter)], ..crate::ops::NONE };

/// The kind's registration value (the ends fold through this file's helpers).
pub(crate) struct FoldFilter;

/// `a`'s FILTER compiled over the rows reaching step `j` of pipeline `k` (the aggregate's input row), if it has one.
pub(crate) fn compile<'p>(px: &mut Px<'_, 'p>, k: usize, j: usize, a: &AggDecl<'p>, node: i32) -> Option<ExId> {
    a.filter.map(|f| crate::step::expr::compile_at(px, k, j, f, None, node))
}

/// `step` over the carry words `accs` when the row passes `filter` (always without one): the predicate's NULL is false,
/// its error raises through the row; on a failing row the words pass on unchanged (joined by block parameters).
pub(crate) fn step_if(b: &mut FunctionBuilder<'_>, env: &mut Env, node: i32, row: &RowVals, filter: Option<ExId>, accs: &mut [Value], step: impl FnOnce(&mut FunctionBuilder<'_>, &mut Env, &mut [Value])) {
    let Some(ex) = filter else { return step(b, env, accs) };
    let pass = crate::step::expr::pred(b, env, node, row, ex);
    let types: PgVec<'_, _> = accs.iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
    let (l_on, l_join) = (b.create_block(&[]), b.create_block(&types[..]));
    b.cond_br(pass, (l_on, &[]), (l_join, accs));
    b.switch_to(l_on);
    let mut on = mcx::to_vec_in(&accs[..], env.mcx);
    // a datum read in the branch does not dominate the join: the row's memo is set aside and restored around it
    let memo = env.memo.set_aside();
    step(b, env, &mut on);
    env.memo.restore(memo);
    b.br(l_join, &on[..]);
    b.switch_to(l_join);
    accs.copy_from_slice(b.block_params(l_join));
}

/// D3233 (road case.select): `fold` into `acc` only where `pass` holds (num_case.rs `select`: a CASE aggregate's rows that
/// take a non-NULL arm); elsewhere a register state passes on unchanged (joined by block parameters) and a slot state is
/// left as it is. The row's memo is set aside inside, as in `step_if`.
pub(crate) fn acc_when(b: &mut FunctionBuilder<'_>, env: &mut Env, pass: Value, acc: Acc<'_>, fold: impl FnOnce(&mut FunctionBuilder<'_>, &mut Env, Acc<'_>)) {
    match acc {
        Acc::Regs(r) => {
            let types: PgVec<'_, _> = r.iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
            let (l_on, l_join) = (b.create_block(&[]), b.create_block(&types[..]));
            b.cond_br(pass, (l_on, &[]), (l_join, &r[..]));
            b.switch_to(l_on);
            let mut on = mcx::to_vec_in(&r[..], env.mcx);
            env.aside(|env| fold(b, env, Acc::Regs(&mut on[..])));
            b.br(l_join, &on[..]);
            b.switch_to(l_join);
            r.copy_from_slice(b.block_params(l_join));
        }
        Acc::Slot { base, off, valid } => {
            let (l_on, l_join) = (b.create_block(&[]), b.create_block(&[]));
            b.cond_br(pass, (l_on, &[]), (l_join, &[]));
            b.switch_to(l_on);
            env.aside(|env| fold(b, env, Acc::Slot { base, off, valid }));
            b.br(l_join, &[]);
            b.switch_to(l_join);
        }
    }
}
