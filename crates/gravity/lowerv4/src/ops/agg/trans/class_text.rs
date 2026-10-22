//! Kind `class.text` (`ClassImpl`: min / max over text / bytea; L4) (cards/M6.md, M6-changes.md §1). Owner M6-FOLD.
//!
//! min / max of text, varchar, bpchar-as-text and bytea whose argument is no code lane (a code lane's extremum is
//! class.minmax_code's): the straight form (D2) is PG's own transition by fmgr through face/aggcall.rs with
//! class.num_varlena's hosted fold: `text_smaller` / `text_larger` (varlena.c:2674, 2686: `varstr_cmp` under the
//! aggregate's input collation) or `byteasmaller` / `bytealarger`, the extreme copied into the statement's aggregate
//! arena (a TOASTed input is detoasted by the call, as PG's), no final. The group's state is its `AggState` slot
//! (fold/hosted.rs). A keyed end refuses the hosted slot by name (D798). Under a collation whose equality is not the
//! bytes' (nondeterministic), equal extremes differ in bytes: `text_smaller` / `text_larger` return their second argument
//! on a tie, so PG keeps the equal member its scan meets last. That form is served (main 10-04 00:14Z, ip-0017..0020;
//! the equal-member ruling, extended 09-29 to min / max of every type, collation-equal text included: MAIN-NOTES 11:54Z):
//! the result is always an input member's text (the rigs judge a min / max column against PG's equal set), and a plain
//! fold meets the rows in PG's order anyway (the hosted fold runs width 1, and `graph::walk_free` is false for such an
//! extremum, so a plain end never walks pages backwards); beside a DISTINCT a rider may keep another equal member.
//! The prefix-word forms (v2 partmin / pairmin, cards/M6.md L4) are a perf row, measured.
use gravity_decompose::ops::shared::agg::AggDecl;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::Value;

use crate::ops::agg::numeric::class_num_varlena::FOLD;
use crate::face::fold::{Acc, AccShape, ClassImpl, FoldClass};
use crate::face::kinds::KindId;
use crate::frame::env::Env;
use crate::ops::Table;
use crate::rep::CellRef;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::ClassText, &ClassImpl(&TEXT))], ..crate::ops::NONE };

/// The hosted fold (any collation: PG's own `text_smaller` / `text_larger` / `byteasmaller` / `bytealarger`).
struct Text;

static TEXT: Text = Text;

impl FoldClass for Text {
    fn shape(&self, a: &AggDecl<'_>) -> AccShape {
        FOLD.shape(a)
    }

    fn init(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>) {
        FOLD.init(b, env, a, acc)
    }

    fn step(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>, args: &[CellRef<'_>]) {
        FOLD.step(b, env, a, acc, args)
    }

    fn merge(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, into: Acc<'_>, from: Acc<'_>) {
        FOLD.merge(b, env, a, into, from)
    }

    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>) -> (Value, Value) {
        FOLD.finish(b, env, a, acc)
    }
}
