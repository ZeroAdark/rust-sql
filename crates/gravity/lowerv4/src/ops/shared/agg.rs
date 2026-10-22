//! Decisions on the decomposer's aggregate declarations that several lowering families read (the group ends and
//! the aggregate transitions).
use gravity_decompose::ops::shared::agg::{AggDecl, FinalForm, GroupDecl, GroupForm, Register};
use gravity_decompose::ops::shared::agg_class::Class;
use types_nodes::Node;

/// Whether `d` folds host-sorted: a DISTINCT aggregate decompose left without a pair slot (`FinalForm::Distinct`: a second
/// DISTINCT argument, a fold the pair set does not take) whose every input is a DISTINCT column (one: PG's datum sort; more,
/// D1283: its tuple sort), not an ordered-set one; D1323: a user-defined one too, its transfn and finalfn PG's own by fmgr
/// (class.user's hosted fold). Its ORDER BY, a prefix of the DISTINCT's clause, is that sort's.
/// D458: or an order-sensitive aggregate with an ORDER BY of its own (`FinalForm::OrderedFold`, no DISTINCT): the group
/// keeps its arguments and the ORDER BY's resjunk columns (`sort_cols`), its final sorts them by the ORDER BY (PG 18.6's
/// process_ordered_aggregate_single / _multi) and folds every row in that order (a user-defined one: D1371).
pub fn sorted(d: &AggDecl<'_>) -> bool {
    let all = d.node.as_aggref().is_some_and(|r| r.args.iter().count() >= 1 && r.aggdistinct.iter().count() == r.args.iter().count());
    let set = matches!(d.class, Class::OrderedSet(_) | Class::Hypothetical(_));
    let ordered = d.final_ == FinalForm::OrderedFold && d.mods.distinct == 0 && d.mods.order > 0 && d.node.as_aggref().is_some_and(|r| r.args.iter().count() >= 1);
    (d.final_ == FinalForm::Distinct && all || ordered) && !set
}

/// D458: whether host-sorted `d` keeps its ORDER BY's resjunk arguments beside its inputs (the host's sort columns; the
/// transfn reads the inputs alone). A DISTINCT's ORDER BY is among its inputs, so it keeps none.
pub fn sort_cols(d: &AggDecl<'_>) -> bool {
    sorted(d) && d.final_ == FinalForm::OrderedFold
}

/// NEW-numeric-1: whether `d` is an order-key numeric extreme (`MinNumDyn` / `MaxNumDyn`, class.num_packed's keyed form:
/// ops/agg/numeric/num_side.rs). N13: its record slot points at a side copy the group's ends keep and allocate (the
/// image cursor), which a register carry cannot hold.
pub fn order_keyed(d: &AggDecl<'_>) -> bool {
    matches!(d.reg, Register::MinNumDyn | Register::MaxNumDyn)
}

/// The least `Pipeline::end_rate` (the planner's rows reaching a pipeline's end per iteration of its innermost loop,
/// `END_RATE_ONE` = 1 row) at which a keyed end takes its run carry: dropped only where the innermost loop iterates more
/// than once per row reaching the end ...
pub(crate) const KEYED_RATE: u64 = gravity_program::END_RATE_ONE;
/// ... and at which a plain fold carries its accumulators: over inner, semi and anti hash probes at 0.09 to 1 row reaching
/// the end a probe and over nested loops, the carry pays from about a quarter of a row an iteration on.
pub(crate) const FOLD_RATE: u64 = gravity_program::END_RATE_ONE / 4;

fn at_least(v: u64, a: &[u64; 4]) -> u64 {
    (v >= a[0]) as u64
}

/// Whether an end's carried registers ride pipeline `k` (a keyed end's run carry at `least` = `KEYED_RATE`, a plain fold's
/// `frame.carry` accumulators at `FOLD_RATE`). They ride every iteration of the pipeline's loops as TPDE stack slots,
/// stored and reloaded on each loop edge, while only the rows reaching the end use them: where the plan's rows reaching the
/// end per innermost-loop iteration (`end_rate`, decompose ops/agg) fall under `least` (a nested loop's walk over unmatched
/// candidates, an anti or a selective probe), the end keeps its state in memory instead. A decision on an estimate: a
/// guard of the program (lower inputs/key.rs `decide_end_rate`). A pipeline with no loop past its source's
/// (`END_RATE_NONE`) carries, and files no guard.
pub(crate) fn carry_rides(px: &crate::frame::px::Px<'_, '_>, k: usize, least: u64) -> bool {
    let rate = px.gx.pipelines().get(k).map_or(gravity_program::END_RATE_NONE, |p| p.desc.end_rate);
    rate == gravity_program::END_RATE_NONE || gravity_lower::inputs::key::decide_end_rate(k, rate, at_least, [least, 0, 0, 0]) == 1
}

/// GP-1b: group `g`'s ZoneTopK hint walks down (a DESC first sort key over a NOT NULL key); the sort walk and the agg
/// row skip read it here.
pub(crate) fn group_desc(g: &gravity_decompose::ops::shared::agg::GroupDecl<'_>) -> bool {
    g.hints.iter().any(|h| matches!(h, Some(gravity_decompose::Hint::ZoneTopK { desc: true, .. })))
}

/// Whether group `g` is decompose's tagged multi-argument count(DISTINCT) form (decompose ops/agg/mod.rs `multi_grouped`;
/// ops/group/pairs_multi.rs serves it): a grouped DISTINCT whose two or more trailing resno-0 keys are the arguments, every
/// DISTINCT call a `PairCount` naming one of them.
pub(crate) fn tagged_multi(g: &GroupDecl<'_>) -> bool {
    let Some(gk) = g.keys.iter().position(|k| k.resno == 0) else { return false };
    let args = gk..g.keys.len();
    g.form == GroupForm::Distinct && gk > 0 && args.len() > 1 && g.keys[gk..].iter().all(|k| k.resno == 0) && g.aggs.iter().filter(|d| d.mods.distinct > 0).all(|d| d.final_ == FinalForm::PairCount && args.contains(&(d.count_slot as usize)))
}

/// Tagged group `g`'s FILTER on resno-0 key `key`: its calls' (decompose `multi_grouped` keys a filtered call apart, so
/// the calls on one key share its FILTER). A row the FILTER does not pass reads the key as NULL (table/rec.rs
/// `key_vals`): tag 0's record flags it, another tag's is not upserted.
pub(crate) fn tag_filter<'p>(g: &GroupDecl<'p>, key: usize) -> Option<Node<'p>> {
    g.aggs.iter().find(|d| d.final_ == FinalForm::PairCount && d.count_slot as usize == key).and_then(|d| d.filter)
}

/// The count slots tagged group `g`'s accumulator appends past its riders (and a top's order slots): one per argument past
/// the first (ops/group/pairs_multi.rs `Tagged::acc`).
pub(crate) fn tag_slots(g: &GroupDecl<'_>) -> usize {
    g.keys.iter().filter(|k| k.resno == 0).count() - 1
}
