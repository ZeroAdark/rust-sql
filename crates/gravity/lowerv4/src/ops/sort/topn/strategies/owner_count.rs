//! Road `owner.count` (cards/M3.md L5 `runs/owner_count.rs`; M3-plan §4 I1): the count-ordered owner top with its threshold
//! gate. A top over a hash group's finals with the road's shape (props `count_cut`, V4R-113: the first key `count` DESC, a
//! constant bound n = LIMIT + OFFSET (the owner region sized from n), ties named only on word-ordered keys in key order, no HAVING but a suffix cut (D3152), no
//! raising column but checked integer arithmetic of keys) declares exec `owner.rs`'s count form in place of the generic owner (face/keyed.rs `OwnerTopMech`, the
//! L0-d seam in keyed/hashagg.rs `pipeline_desc`):
//! - each owner keeps its n best records by (count descending, key words ascending: kernels `reps::count_better`), a heap
//!   with the worst at its root;
//! - `OwnerTop.thr`, the n-th kept count, gates the candidates: the owner fold lists a record once its count reaches the
//!   gate (`Candidates::note`, `MOVING` until offered), so a partition's reduce offers only those and never scans its slots
//!   (I2: the top is kept during insertion);
//! - the emit gathers only the kept records (host_abi `FORM_COUNT_TOPK`) and sorts them by the top's own keys
//!   (runs_topk_groups `sort_words`), so the final order is PostgreSQL's; the kept set holds the SQL's top n because the
//!   owner's tie order refines every order the shape admits.
//!
//! A text key the sort names after the count ties by its representative's rendered bytes (the descriptor's `reps`, C
//! order under a bytes-ordered collation, a NULL last); one it leaves unnamed ties by its fingerprint words (`REP_NONE`:
//! no render).
//! Budget 160 lines (files.tsv, +25 % gate). Owner M3-OWNER.
use gravity_abi::breaker::{KeyKind, SlotKind, ORDER_KEY};
use gravity_decompose::ops::shared::agg::{AggDecl, FinalForm, GroupDecl, GroupForm};
use gravity_decompose::ops::shared::agg_class::Class;
use gravity_kernels::host_abi::{SK_DESC, SK_FLOAT, SK_NULLS_FIRST, SK_SLOT};
use gravity_decompose::catalog::{Facts, KeyReq};
use gravity_decompose::ops::shared::sort::{SortDesc, SortKey};
use gravity_decompose::Op;
use gravity_program::{KeyCheck, OwnerDesc, ACC_PAIRS, ACC_SLOT_BASE, MAX_REPS, REP_NONE};
use types_nodes::list::NodeList;
use types_nodes::primnodes::OUTER_VAR;
use types_nodes::Node;

use crate::table::face::{OwnerTopImpl, OwnerTopMech, TableDecl};
use crate::frame::px::Px;
use crate::admit::refusal::Reason;
use crate::ops::Table;

crate::strategies! {
    /// Count-ordered owner top, threshold gate.
    OWNER_COUNT = 64, "owner.count", CountTopk;
}

pub(crate) const ROWS: Table = Table { strategies: &[(OWNER_COUNT, &OwnerTopImpl(&COUNT))], ..crate::ops::NONE };

/// The count-ordered owner top (the road's one mechanism).
struct CountTop;

static COUNT: CountTop = CountTop;

impl OwnerTopMech for CountTop {
    /// The count form over the table's record: the top's count slot (decompose's slot, which the image layout keeps), n, and
    /// each text key the sort names after the count as its representative (slot, dictionary lane): a count tie renders it.
    fn owner<'p>(&self, px: &mut Px<'_, 'p>, k: usize, g: &GroupDecl<'p>, t: &TableDecl, n: u32) -> OwnerDesc {
        let Some((j, d, input)) = top(px, k, g) else { bug(g.node_id) };
        // D563: a grouped count(DISTINCT) group's count form is over the accumulator (keyed/pairs.rs takes it from here):
        // the pair count's slot ACC_PAIRS, a rider's and a text key's representative ACC_SLOT_BASE on (keyed/pairs_view.rs)
        let acc = g.form == GroupForm::Distinct && crate::graph::grouped(g);
        let Some(x) = count_slot(d, input, g) else { bug(g.node_id) };
        let Some((slot, kind)) = top_slot(g, x).filter(|&(s, _)| acc || (s as usize) < t.layout.nslots()) else { bug(g.node_id) };
        // the count descending keeps its gated form (order word 0); any other final orders by the emit's own word
        let Some(order) = order_word(px.gx.facts, Some(kind), slot, x, &d.keys[0]) else { bug(g.node_id) };
        let mut reps = [(REP_NONE, 0u8); MAX_REPS];
        let mut null_tie = false;
        let mut text = 0u32; // D3080: the int-text tie keys, by key index
        for (i, (sk, key)) in d.keys[1..].iter().zip(g.keys.iter()).enumerate() {
            match crate::props::tie_key(px.gx, g.producer.0 as usize, &g.keys[..i], key, sk, !acc) {
                Some(Some(rep)) => reps[g.keys[..i].iter().filter(|b| b.kind == KeyKind::Fp128 && b.words == 2).count()] = (rep.0 + if acc { ACC_SLOT_BASE } else { 0 }, rep.1),
                Some(None) => {
                    null_tie |= crate::table::emit_group::key_nullable(px.gx, g.producer.0 as usize, key);
                    if key.int_text() {
                        if acc || i >= 24 {
                            bug(g.node_id);
                        }
                        text |= 1 << i;
                    }
                }
                None => bug(g.node_id),
            }
        }
        // a projection's integer arithmetic of keys the owner checks in every group, the constants' words claimed (props `key_check`)
        let Some((check, check_nulls)) = crate::props::key_check(px, j, g, d) else { bug(g.node_id) };
        let desc = OwnerDesc::count_topk_text(t.layout, slot, n, reps);
        let desc = if order == 0 { desc } else { desc.ordered_by(order) };
        let desc = if text == 0 { desc } else { desc.tie_text(text) };
        // a nullable word tie key orders by its NULL flag, and (NEW-46) a nullable checked key's NULL is skipped by it: the
        // record's flags words follow its image words (rec.rs `flags_word`)
        let desc = if null_tie || check_nulls { desc.tie_flags(flags_first(px, g)) } else { desc };
        fires::fire("owner.count"); // shapecheck fire signal (REGISTRY RUNS-17 / STEP-17 / STEP-18 / BG-06 / OIDW-39 / OIDW-40)
        if order != 0 {
            fires::fire("owner.order"); // shapecheck fire signal: a top kept by another final's slot order
        }
        if check == KeyCheck::NONE { desc } else { desc.key_check(check) }
    }
}

/// The record's first NULL flags word: past every key's image words (rec.rs `GroupRec::flags_word`).
fn flags_first(px: &Px<'_, '_>, g: &GroupDecl<'_>) -> u8 {
    let words: usize = crate::dict::coderep::key_kinds(px.gx, g.producer.0 as usize, &g.keys).iter().map(|k| k.1 as usize).sum();
    u8::try_from(words).unwrap_or_else(|_| bug(g.node_id))
}

/// A shape `count_cut` admitted that the owner cannot take after all: a lowering defect, never a statement's refusal.
fn bug(node: i32) -> ! {
    crate::refuse!(node, Reason::Bug { payload: "a count-ordered top outside road owner.count's shape (props count_cut)" })
}

/// The top over group `g` (producer `k`): the sort on the pipeline that drains `g` (runs_topk_groups `count_top_of`) and
/// the projection before it.
fn top<'g, 'p>(px: &Px<'g, 'p>, k: usize, g: &GroupDecl<'_>) -> Option<(usize, &'g SortDesc<'p>, Option<&'p NodeList<'p>>)> {
    let gx = px.gx;
    let j = gx.drainers(g).into_iter().find(|&j| j > k)?;
    let ops = &gx.pipelines()[j].ops;
    let s = ops.iter().position(|op| matches!(op, Op::Sort(_)))?;
    let Op::Sort(d) = &ops[s] else { return None };
    Some((j, d, ops[..s].iter().rev().find_map(|op| if let Op::Project(l) = op { Some(*l) } else { None })))
}

/// The final the top orders by first, read through the projection before the sort.
fn count_slot<'a, 'p>(d: &SortDesc<'_>, input: Option<&NodeList<'_>>, g: &'a GroupDecl<'p>) -> Option<&'a AggDecl<'p>> {
    let mut e = entry(d.tlist, d.keys.first()?.resno)?;
    if let (Some(v), Some(l)) = (e.as_var().filter(|v| v.varno == OUTER_VAR && v.varlevelsup == 0), input) {
        e = entry(l, v.varattno)?;
    }
    let a = e.as_aggref()?;
    g.aggs.iter().find(|x| x.node.as_aggref().is_some_and(|b| b.aggno == a.aggno))
}

/// The slot the count form keeps final `x` of group `g` by, a byte, and its kind: the group record's, or in a grouped
/// DISTINCT's accumulator (D563, keyed/pairs_view.rs) the pair count's ACC_PAIRS or a rider's from ACC_SLOT_BASE; `None`
/// for a DISTINCT fold's slot (PairFold) or a slot past a byte.
pub(crate) fn top_slot(g: &GroupDecl<'_>, x: &AggDecl<'_>) -> Option<(u8, SlotKind)> {
    let acc = g.form == GroupForm::Distinct && crate::graph::grouped(g);
    match x.final_ {
        FinalForm::PairCount if acc => Some((ACC_PAIRS, SlotKind::Count)),
        FinalForm::PairCount | FinalForm::PairFold => None,
        _ => Some((u8::try_from(x.slot).ok()?.checked_add(if acc { ACC_SLOT_BASE } else { 0 })?, *g.layout.slots.get(x.slot as usize)?)),
    }
}

/// The owner's order word for a top whose first key `sk` is final `x` in record slot `slot` of `kind`: `Some(0)` for `count(*)` /
/// `count(x)` descending in a `Count` slot (the gated form); the slot's sort word (kernels `SK_SLOT`, with `SK_DESC`,
/// `SK_NULLS_FIRST`, `SK_FLOAT`) for a final the emit orders by its slot alone (topn lower.rs `sort_words`' plain slot
/// arms: a count, an int sum, min or max, a numeric sum, an average, a float sum or average) under its result type's own
/// order (KeySem: the sort operator orders the final's type by the type's default btree family); `None` for any other
/// final (a text or bit final, another family's order).
pub(crate) fn order_word(facts: &dyn Facts, kind: Option<SlotKind>, slot: u8, x: &AggDecl<'_>, sk: &SortKey) -> Option<u16> {
    let kind = kind?;
    // D4565: a var / stddev over int2 / int4 folds into an AvgI128 slot whose Sx / N is not its final
    if matches!(x.class, Class::Stat3I128(_)) {
        return None;
    }
    if kind == SlotKind::Count && sk.desc && matches!(x.class, Class::CountStar | Class::CountAny) {
        return Some(0);
    }
    let typ = x.node.as_aggref()?.aggtype;
    let own = facts.key(KeyReq::sort(sk.opno)).is_some_and(|o| o.opcintype == typ && o.dir() == Some(sk.desc));
    let float = match kind {
        SlotKind::Count | SlotKind::SumI64 | SlotKind::MinI64 | SlotKind::MaxI64 | SlotKind::SumI128 | SlotKind::SumNum | SlotKind::SumNumWide | SlotKind::AvgI128 => false,
        SlotKind::SumF64 | SlotKind::SumF32 => true,
        SlotKind::F64Accum if x.class == Class::AvgF64 => true,
        _ => return None,
    };
    let slot = Some(slot).filter(|&s| s < ORDER_KEY)?;
    let flag = |on: bool, f: u64| if on { f } else { 0 };
    let w = SK_SLOT | slot as u64 | flag(float, SK_FLOAT) | flag(sk.desc, SK_DESC) | flag(sk.nulls_first, SK_NULLS_FIRST);
    own.then_some(u16::try_from(w).ok()?)
}

/// The expression of target entry `resno` in `l`.
fn entry<'p>(l: &NodeList<'p>, resno: i16) -> Option<Node<'p>> {
    l.iter().filter_map(|n| n.as_target_entry()).find(|t| t.resno == resno).map(|t| t.expr)
}
