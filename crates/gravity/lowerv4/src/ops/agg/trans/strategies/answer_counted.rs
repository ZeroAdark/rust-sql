//! Counts answered from a lane's granule record. Hooks / levers: H02f R2-L02. Budget 160 lines (files.tsv, +25 % gate).
//! Owner M1-FOLD.
//!
//! Registers: road `answer.counted` (`ANSWER_COUNTED`): `count(*)` (or `count(x)`) under a filter one of whose quals is a
//! predicate whose passing rows the column's granule record counts with no constant in it (pgrc summary.rs `tally`, read
//! through frame/iface), answered from that record: `x IS NOT NULL` as `nonnull`, over a column of an integral type. A
//! comparison with a constant is never answered here: `x = c` / `x <> c` take the zone verdict every constant takes, so
//! `x <> 0` costs what `x <> 5` costs. SRC-21: the qual is the site's zone predicate marked `tallied` (the source's site
//! rule), so the host answers a window that predicate leaves Mixed when every other zone predicate matches it whole,
//! skips one it matches no row of, and decodes the rest with the filter run per row. A qual list (the qual beside others
//! in one filter) and other filter ops take the form when the site's zone covers them whole. The filter's step is skipped
//! for an answered window (the end takes it whole before its rows). A count battery's FILTERs (`filter_at`) take the
//! constant-free tallies too: `x IS NULL`, `x IS NOT NULL`, and a bool column's `b` / `NOT b` from its record's false
//! tally. Every other aggregate of the fold turns the form off (answer.rs `declare_site`).
use gravity_abi::ctx::answer;
use gravity_decompose::ops::shared::agg::{AggDecl, Register as R};
use gravity_decompose::graph::Op;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, Type, Value};
use gravity_program::{LaneId, Rep, SiteDesc};
use mcx::{CollectIn, PgVec};

use crate::face::fold::{Acc, AnswerImpl, AnswerMech};
use crate::frame::env::Env;
use crate::frame::px::Px;

crate::strategies! {
    /// Counts answered from a lane's granule record. Premise `PgrcPlain` (not `PgrcPlainFilter`): a
    /// count battery's FILTERs are no pipeline filter op, and `qual_at` / `filter_at` decide each fold themselves.
    ANSWER_COUNTED = 19, "answer.counted", PgrcPlain;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(ANSWER_COUNTED, &AnswerImpl(&COUNTED))], ..crate::ops::NONE };

struct Counted;
static COUNTED: Counted = Counted;

/// The counted qual's predicate over its column `x`: which of a window's rows pass it, read from `x`'s granule record
/// (pgrc summary.rs `Tally`).
pub(crate) use crate::frame::iface::SummaryTally as Pred;

/// The counted qual of pipeline `k`'s plain fold: `(j, x's site lane, the predicate)` of [`qual_at`].
pub(crate) fn qual(px: &Px<'_, '_>, k: usize, site: &SiteDesc, zoned: u64) -> Option<(usize, usize, Pred)> {
    qual_at(px, k, site, zoned).map(|(j, _, l, pr)| (j, l, pr))
}

/// The counted qual of pipeline `k`'s plain fold: filter op `j`'s qual `i` is the first counted predicate (summary.rs
/// `tally`) of the pipeline's filters, over an integer column `x` the site holds, and every aggregate is `count(*)` or
/// `count(x)` (the same rows: the predicate passes only non-null `x`) with no FILTER; the pipeline's other ops are
/// filters and projections. Op `j` may hold other quals only where the source's zone covers it whole (`zoned` bit `j`:
/// every qual of it a zone predicate the site declares, the counted one tallied); a lone qual takes the form whether or
/// not it is zoned. `(j, i, x's site lane, the predicate)`.
pub(crate) fn qual_at(px: &Px<'_, '_>, k: usize, site: &SiteDesc, zoned: u64) -> Option<(usize, usize, usize, Pred)> {
    if !(px.strategies.has(ANSWER_COUNTED) && crate::face::source::on(crate::admit::refusal::Variant::Strategy(ANSWER_COUNTED))) {
        return None;
    }
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let g = gx.group_at(k)?;
    if !g.aggs.iter().all(|a| a.filter.is_none() && matches!(a.reg, R::CountStar | R::CountAny)) {
        return None;
    }
    let mut found = None;
    for (j, op) in p.ops.iter().enumerate() {
        match op {
            Op::Project(_) => {}
            Op::Filter(_) if found.is_some() => {}
            Op::Filter(q) => {
                let qs: PgVec<'_, types_nodes::Node<'_>> = q.as_list().map_or_else(|| mcx::pg_vec![in px.mcx(); *q], |l| l.iter().collect_in(px.mcx()));
                if let Some((i, (attno, pr))) = qs.iter().enumerate().find_map(|(i, &q)| Some((i, crate::frame::iface::summary_tally(q)?))) {
                    if qs.len() > 1 && !(j < 64 && zoned >> j & 1 == 1) {
                        return None;
                    }
                    found = Some((j, i, attno, pr));
                }
            }
            _ => return None,
        }
    }
    let (j, i, attno, pr) = found?;
    let lane = site.lane_index(LaneId::Attr(attno))?;
    let ok = g.aggs.iter().all(|a| a.reg == R::CountStar || arg_is(px, k, a, attno));
    ok.then_some((j, i, lane, pr))
}

/// A counted FILTER's predicate (frame/iface `SummaryFilterTally`).
pub(crate) use crate::frame::iface::SummaryFilterTally as FilterPred;

/// A count battery's member (`count(*) FILTER (WHERE is_bot), ...` with no WHERE): aggregate `a`
/// of pipeline `k`'s plain fold as a counted FILTER, `count(*)` or `count(x)` of the FILTER's own column `x`, whose FILTER
/// is a predicate `x`'s granule record tallies (pgrc summary.rs `filter_tally`) over a lane of `site`: `(x's lane, the
/// predicate)`. An answered window's count is then its record's tally (`filtered`), the same rows the FILTER passes.
pub(crate) fn filter_at(px: &Px<'_, '_>, k: usize, a: &AggDecl<'_>, site: &SiteDesc) -> Option<(usize, FilterPred)> {
    let f = a.filter?;
    if !matches!(a.reg, R::CountStar | R::CountAny) || !(px.strategies.has(ANSWER_COUNTED) && crate::face::source::on(crate::admit::refusal::Variant::Strategy(ANSWER_COUNTED))) {
        return None;
    }
    let col = |n: types_nodes::Node<'_>| {
        let v = n.as_var()?;
        Some((crate::ops::agg::trans::block_fold::scan_attno(px, k, crate::step::project::bare_var(n)?)?, v.vartype))
    };
    let (attno, pred) = crate::frame::iface::summary_filter_tally(f, &col)?;
    if a.reg == R::CountAny && !arg_is(px, k, a, attno) {
        return None;
    }
    // `IS NULL` of any type, over a by-value word lane (ints, dates, timestamps, bool, floats): the host answers a window
    // only from an exact record (granule_source.rs `part_answered`), never a text or code lane's
    let l = site.lane_index(LaneId::Attr(attno))?;
    (pred != FilterPred::IsNull || matches!(site.lanes()[l].rep, Rep::Word { .. })).then_some((l, pred))
}

/// A counted FILTER's count over an answered window of `rows` rows from lane `lane`'s record (`filter_at`): `x IS NOT
/// NULL`'s `nonnull`, a bool lane's `b` / `NOT b` from its false tally, `x IS NULL`'s `rows - nonnull` (0 for `count(x)`:
/// `any`).
pub(crate) fn filtered(b: &mut FunctionBuilder<'_>, ans: Value, lane: usize, pred: FilterPred, rows: Value, any: bool) -> Value {
    if pred == FilterPred::IsNull && any {
        return b.iconst(Type::I64, 0);
    }
    fires::fire("answer.counted"); // shapecheck fire signal (as `counted`)
    let at = (lane * answer::SIZE) as i32;
    let nonnull = |b: &mut FunctionBuilder<'_>| b.load(Type::I64, ans, at + answer::NONNULL as i32);
    // a bool lane's record tallies its false rows (hosts granule_source `answer`)
    let falses = |b: &mut FunctionBuilder<'_>| b.load(Type::I64, ans, at + answer::FALSE_COUNT as i32);
    match pred {
        FilterPred::NotNull => nonnull(b),
        FilterPred::IsNull => {
            let n = nonnull(b);
            b.int_bin(IntBinOp::Sub, rows, n)
        }
        FilterPred::True => {
            let (n, f) = (nonnull(b), falses(b));
            b.int_bin(IntBinOp::Sub, n, f)
        }
        FilterPred::False => falses(b),
    }
}

/// `a`'s one argument is scan column `attno`.
fn arg_is(px: &Px<'_, '_>, k: usize, a: &AggDecl<'_>, attno: i16) -> bool {
    let Some(r) = a.node.as_aggref() else { return false };
    let Some(t) = r.args.iter().next().filter(|_| r.args.len() == 1) else { return false };
    crate::step::project::bare_var(t.as_target_entry().map_or(t, |t| t.expr)).and_then(|c| crate::ops::agg::trans::block_fold::scan_attno(px, k, c)) == Some(attno)
}

/// The rows of an answered window that pass `pred` over lane `lane`, from the lane's record.
pub(crate) fn counted(b: &mut FunctionBuilder<'_>, ans: Value, lane: usize, pred: Pred) -> Value {
    fires::fire("answer.counted"); // shapecheck fire signal (REGISTRY FOLD-01 / PL-21 / OIDW-5 / AR-43 / AR-44)
    let at = (lane * answer::SIZE) as i32;
    match pred {
        Pred::NotNull => b.load(Type::I64, ans, at + answer::NONNULL as i32),
    }
}

impl AnswerMech for Counted {
    fn declare(&self, _px: &mut Px<'_, '_>, _site: &mut SiteDesc, a: &AggDecl<'_>) -> bool {
        matches!(a.reg, R::CountStar | R::CountAny) && a.filter.is_none()
    }

    /// A count's carry word from the counted lane's record: `nonnull`.
    fn fold(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _a: &AggDecl<'_>, acc: Acc<'_>, rec: Value) {
        let Acc::Regs(w) = acc else { crate::refuse!(env.node, crate::admit::refusal::Reason::Bug { payload: "an answer into a slot" }) };
        w[0] = counted(b, rec, 0, Pred::NotNull);
    }
}
