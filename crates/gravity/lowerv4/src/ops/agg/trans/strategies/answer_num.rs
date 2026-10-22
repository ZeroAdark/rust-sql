//! Numeric SUM answers over packed lanes. Hooks / levers: H02f R2-L16. Budget 160 lines (files.tsv, +25 % gate). Owner
//! M1-FOLD.
//!
//! Registers: road `answer.numeric` (`ANSWER_NUMERIC`): `sum` / `avg` of a bare packed `numeric(p, s)` column
//! over an answered window (answer.rs) folds from the lane's record: pgrc's stats sum of a packed lane is the exact
//! i128 sum of its mantissas at the column's scale, which is the aggregate's (a bare column's `AggDecl::scale`), and
//! `nonnull` its rows. A part holding a special (NaN / ±Inf) has no computed sum, so the source never answers it.
use gravity_abi::ctx::answer;
use gravity_decompose::ops::shared::agg::{AggDecl, Register as R};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Type, Value};
use gravity_program::SiteDesc;

use crate::face::fold::{Acc, AnswerImpl, AnswerMech};
use crate::frame::env::Env;
use crate::frame::px::Px;

crate::strategies! {
    /// Numeric SUM answers over packed lanes.
    ANSWER_NUMERIC = 20, "answer.numeric", PgrcPlainNum;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(ANSWER_NUMERIC, &AnswerImpl(&NUMERIC))], ..crate::ops::NONE };

struct Numeric;
static NUMERIC: Numeric = Numeric;

/// `a` is `sum` / `avg` of a bare numeric column at the aggregate's scale (the stats sum's scale).
pub(crate) fn fits(a: &AggDecl<'_>) -> bool {
    let Some(r) = a.node.as_aggref().filter(|r| r.args.len() == 1 && matches!(a.reg, R::SumNum | R::AvgNum)) else { return false };
    let Some(t) = r.args.iter().next() else { return false };
    let e = t.as_target_entry().map_or(t, |t| t.expr);
    let e = e.as_relabel_type().map_or(e, |r| r.arg);
    e.as_var().is_some_and(|v| v.vartype == types_core::NUMERICOID && v.vartypmod >= 4 && ((v.vartypmod - 4) & 0xffff) as i16 == a.scale)
}

impl AnswerMech for Numeric {
    fn declare(&self, _px: &mut Px<'_, '_>, _site: &mut SiteDesc, a: &AggDecl<'_>) -> bool {
        fits(a)
    }

    /// The window's `[sum_lo, sum_hi, nonnull]` from the lane's record.
    fn fold(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _a: &AggDecl<'_>, acc: Acc<'_>, rec: Value) {
        fires::fire("answer.numeric"); // shapecheck fire signal (REGISTRY AR-45 / OIDW-67)
        let Acc::Regs(w) = acc else { crate::refuse!(env.node, crate::admit::refusal::Reason::Bug { payload: "an answer into a slot" }) };
        w[0] = b.load(Type::I64, rec, answer::SUM_LO as i32);
        w[1] = b.load(Type::I64, rec, answer::SUM_HI as i32);
        w[2] = b.load(Type::I64, rec, answer::NONNULL as i32);
    }
}
