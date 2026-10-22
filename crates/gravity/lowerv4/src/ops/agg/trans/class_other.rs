//! Kind `class.other` (`ClassImpl`: interval / money / opaque / tid folds; L4) (cards/M6.md, M6-changes.md §1). Owner
//! M6-FOLD. The straight form (D2): PG's own transition and final by fmgr through face/aggcall.rs with class.num_varlena's
//! hosted fold: sum / avg(interval) (`interval_avg_accum` into its `IntervalAggState`, infinities counted, overflow raising
//! as PG's), sum(money) (`cash_pl`, "money out of range" raised as PG's), min / max(tid) (`tidsmaller` / `tidlarger`),
//! min / max of an opaque type by its own smaller / larger (enum, inet, pg_lsn, ...), bit_and / bit_or / bit_xor(varbit)
//! (a length mismatch raises). decompose gives each a `Var` register (fold_of), so the group's state is its `AggState`
//! slot (fold/hosted.rs). A keyed end refuses the hosted slot by name (D798). The register forms are a perf row.
//!
//! The interval kinds (no kind of their own: the dispatch here): min / max(interval)'s `MinInterval` /
//! `MaxInterval` registers (abi `IntervalMin` / `IntervalMax` slots, never `Var`, so never hosted: fold/hosted.rs
//! `hosted_kind`) fold in class_interval.rs, and sum / avg(interval)'s `SumInterval` / `AvgInterval` over an abi
//! `IntervalSum` slot in class_interval_sum.rs (a call decompose kept on PG's fold, `Trans::Interval16`, stays hosted);
//! every other register keeps the hosted fold. Each method dispatches on the
//! aggregate's register; the two that take none (`deferred_code`, `datum_ds`) are false for both.
use gravity_abi::breaker::SlotKind;
use gravity_decompose::ops::shared::agg::{AggDecl, Register};
use gravity_decompose::ops::shared::agg_class::Trans;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::Value;

use crate::face::fold::{Acc, AccShape, AnswerForm, BlockFold, ClassImpl, FinalOrder, FoldClass, WideTerm};
use crate::face::kinds::KindId;
use crate::frame::env::Env;
use crate::ops::Table;
use crate::rep::CellRef;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::ClassOther, &ClassImpl(&OTHER))], ..crate::ops::NONE };

struct Other;
static OTHER: Other = Other;

/// The class folding `a`'s register.
fn pick(a: &AggDecl<'_>) -> &'static dyn FoldClass {
    match a.reg {
        Register::MinInterval | Register::MaxInterval => &crate::ops::agg::trans::class_interval::INTERVAL,
        Register::SumInterval | Register::AvgInterval if matches!(a.trans, Trans::Abi(SlotKind::IntervalSum)) => &crate::ops::agg::trans::class_interval_sum::INTERVAL_SUM,
        _ => &crate::ops::agg::numeric::class_num_varlena::FOLD,
    }
}

impl FoldClass for Other {
    fn shape(&self, a: &AggDecl<'_>) -> AccShape {
        pick(a).shape(a)
    }
    fn init(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>) {
        pick(a).init(b, env, a, acc)
    }
    fn step(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>, args: &[CellRef<'_>]) {
        pick(a).step(b, env, a, acc, args)
    }
    fn merge(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, into: Acc<'_>, from: Acc<'_>) {
        pick(a).merge(b, env, a, into, from)
    }
    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>) -> (Value, Value) {
        pick(a).finish(b, env, a, acc)
    }
    fn finish_direct(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>, direct: &[(Value, Value)]) -> (Value, Value) {
        pick(a).finish_direct(b, env, a, acc, direct)
    }
    fn order(&self, a: &AggDecl<'_>) -> Option<FinalOrder> {
        pick(a).order(a)
    }
    fn block(&self, a: &AggDecl<'_>) -> Option<BlockFold> {
        pick(a).block(a)
    }
    fn answer(&self, a: &AggDecl<'_>) -> Option<AnswerForm> {
        pick(a).answer(a)
    }
    fn fold_wide(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>, t: &WideTerm) {
        pick(a).fold_wide(b, env, a, acc, t)
    }
    fn puts(&self, a: &AggDecl<'_>) -> bool {
        pick(a).puts(a)
    }
    fn put(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, into: Acc<'_>, from: &[Value]) {
        pick(a).put(b, env, a, into, from)
    }
    fn record_only(&self, a: &AggDecl<'_>) -> bool {
        pick(a).record_only(a)
    }
}
