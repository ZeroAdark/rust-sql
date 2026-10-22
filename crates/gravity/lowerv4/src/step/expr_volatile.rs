//! Kind `expr.volatile` (cards/M2.md L2): a VOLATILE call (or a non-builtin call whose volatility is volatile) is
//! evaluated exactly once per row PG evaluates it on, in PG's order: never ahead (D237: gravity_expr guards every
//! operand PG would not evaluate; a filter's quals stop at the first false; a volatile call has no word form, so a
//! 64-row block mask never evaluates it ahead of the row), never hoisted or memoised (frame/hoist.rs hoists only bind
//! words and immediates; a per-code body and the condition cache take no volatile site; join/hoist.rs only IMMUTABLE
//! calls), never in a verdict (verdict_cond_key.rs: IMMUTABLE only), and never twice on one row: `plan` holds the
//! expression's result (`Held`, two frame words: step/expr.rs emit3 stores it at the first evaluation and a row a stop
//! re-enters through a later expanding step, a join's match chain, reloads it, frame/stop.rs `held`). A sink stops past
//! its row (D357) and a LIMIT after its count, so neither re-evaluates a row; a probe's quals run once per match, as
//! PG's joinqual. join.mult (a probe row folded once as its match count's copies) stays off over a volatile call past the
//! probe or in the group it folds into, and in a pipeline that folds into no group (join/multiplicity.rs `past_volatile`).
//! One M1 shape that assumes no volatile call refuses by name: a statement where twopass.having may scan twice
//! (keyed/twopass.rs `having_split`). A build side's calls
//! keep PG's outer-first order (join/probe_inner.rs, L0-f); PG never hashes or merges on a volatile join clause. Owner
//! M2-FRAME.
use gravity_decompose::ops::shared::agg::{GroupForm, Register};
use gravity_expr::Compiled;
use gravity_program::protos::{Proto, ProtoKind};

use crate::face::kinds::KindId;
use crate::face::step::{Held, VolatileImpl, VolatileMech};
use crate::frame::px::{ExId, Px};
use crate::graph::Gx;
use crate::admit::refusal::{Reason, ShapeWhy};

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { kinds: &[(KindId::ExprVolatile, &VolatileImpl(&VOL))], ..crate::ops::NONE };

struct Vol;
static VOL: Vol = Vol;

/// A group twopass.having may split (keyed/twopass.rs `having_split`: a hashed or sorted group, no two-pass or set
/// declaration, a min / max over codes): its producer's scan runs twice.
fn twopass_candidate(gx: &Gx<'_, '_>) -> bool {
    (0..gx.pipelines().len()).filter_map(|q| gx.group_at(q)).any(|g| {
        matches!(g.form, GroupForm::Hashed | GroupForm::Sorted) && g.two_pass.is_none() && g.set.is_none() && g.aggs.iter().any(|a| matches!(a.reg, Register::MinCode | Register::MaxCode))
    })
}

/// The plan node of step `j` of pipeline `k` (the pipeline's node outside a step).
fn node_at(gx: &Gx<'_, '_>, k: usize, j: usize) -> i32 {
    match gx.pipelines().get(k) {
        Some(p) => p.node_ops.iter().rev().find(|n| n.first_op as usize <= j).map_or(p.node_id, |n| n.node_id),
        None => gx.root_node(),
    }
}

fn refuse(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

impl VolatileMech for Vol {
    /// The expression's result is held (step/expr.rs emit3, frame/stop.rs `held`): a row a stop re-enters reads it.
    fn plan(&self, px: &mut Px<'_, '_>, (k, j): (usize, usize), _c: &Compiled<'_>, _ex: ExId) -> Held {
        let gx = px.gx;
        let node = node_at(gx, k, j);
        // join.mult (join/multiplicity.rs) hands a probe row on once with its match count only to a group's fold, and
        // `past_volatile` keeps it off when a call past the probe or in that group is VOLATILE, or no group takes the
        // rows (a sort, a hash build, a sink): every call past a probe runs once per match. M8 L0-v: a nest's or a
        // Memoize's matches are handed on one row at a time.
        if twopass_candidate(gx) {
            refuse(node, "a volatile call where twopass.having may scan twice");
        }
        // D439: a pull (a cursor's FETCH, Execute's max_rows) asks each call for the rows it still owes, never a full block
        px.declare_proto(Proto::new(ProtoKind::Volatile, k as u16));
        Held { words: 2 }
    }
}
