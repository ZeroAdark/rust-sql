//! The record a grouped DISTINCT group's emit reads, split from the group record's emit view (table/args.rs
//! `emit_view`, D511): keyed.distinct_grouped's owner phase (ops/group/pairs.rs, `OwnerDesc::distinct`) leaves accumulator
//! records (`OwnerDesc::accumulator_folded`: the group keys, `[sentinel, pairs]`, the pair's slots from `ACC_SLOT_BASE`,
//! then one slot per DISTINCT fold, D500), so each rider's slot moves by `ACC_SLOT_BASE`, each `count(DISTINCT)` reads
//! the pair count, and each DISTINCT fold (`sum / avg / min / max(DISTINCT x)`) reads its slot as the plain aggregate's
//! (`fold_of`'s register and final); the argument's words are gone, and where the image has a flags word (some key
//! nullable, D500) it stays after the group words, its argument bit cleared (the owner's group-flags form).
//! D511: a text key's representative slot moves with the riders (`rep_slot`, every reader of `KeyDecl.rep` after the
//! owner: emit_group's key cells, runs.topk_groups' text sort key). D530's order test (the emit sorts a group whose key
//! order the plan reads) reads a node's lowered output, so it is the interface's (frame/iface.rs `order_read`).
use gravity_abi::breaker::{ACC_PAIRS, ACC_SLOT_BASE};
use gravity_decompose::ops::shared::agg::fold_of;
use gravity_decompose::ops::shared::agg::{AggDecl, FinalForm, GroupDecl, GroupForm, Register, NONE};
use gravity_decompose::ops::shared::agg_class::Mods;
use gravity_program::OwnerDesc;
use mcx::{CollectIn, GrowIn, PgVec};

use crate::frame::px::Px;

use crate::ops::agg::hashagg::{Agg, GroupRec};

/// Key representative slot `slot` (`KeyDecl.rep`) in the record group `g`'s emit reads: a grouped DISTINCT group's moves
/// by `ACC_SLOT_BASE` with the riders (the owner folds the pair's slots from there).
pub(crate) fn rep_slot(g: &GroupDecl<'_>, slot: u16) -> u16 {
    if g.form == GroupForm::Distinct && crate::graph::grouped(g) { slot + u16::from(ACC_SLOT_BASE) } else { slot }
}

/// `rep_slot` as an emit sort word's slot field (kernels table/emit.rs `SK_TEXT_SLOT_SHIFT`: eight bits); a record past
/// that refuses by name.
pub(crate) fn sk_slot(g: &GroupDecl<'_>, slot: u16, node: i32) -> u64 {
    match u8::try_from(rep_slot(g, slot)) {
        Ok(s) => u64::from(s),
        Err(_) => crate::refuse!(node, crate::admit::refusal::Reason::Shape { why: crate::admit::refusal::ShapeWhy("a sort by a text key whose representative is past the record's 255th slot (the sort word's slot byte)") }),
    }
}

/// `rec` (a grouped DISTINCT group's) as its emit reads it.
pub(crate) fn view<'g, 'p>(mut rec: GroupRec<'g, 'p>, px: &mut Px<'g, 'p>) -> GroupRec<'g, 'p> {
    let g = rec.g;
    // the tagged form's accumulator (ops/group/pairs_multi.rs): a count slot per argument, its first further tag's slot
    let tagged = if crate::ops::shared::agg::tagged_multi(g) { Some(crate::ops::group::pairs_multi::view(&mut rec, px)) } else { None };
    let arg = g.keys.iter().position(|k| k.resno == 0).unwrap_or(g.keys.len());
    // the image's flags word (GroupRec::new: key_nullable over every key, the argument's included) is the owner's last key
    // wherever it exists: pairs.rs takes the group-flags form exactly then, so the view keeps it exactly then
    let flags = rec.nullable;
    let words = if flags { rec.wpk[arg] as usize } else { rec.layout.nkeys() - g.keys.iter().zip(&rec.wpk).filter(|(k, _)| k.resno != 0).map(|(_, &w)| w as usize).sum::<usize>() };
    let folds: PgVec<'_, (&AggDecl<'p>, (Register, _, FinalForm))> = g.aggs.iter().filter(|d| d.final_ == FinalForm::PairFold).map(|d| (d, fold_of(d.class, d.fnoid, false, false))).collect_in(px.mcx());
    let kinds: PgVec<'_, _> = folds.iter().filter_map(|(_, f)| f.1.abi()).collect_in(px.mcx());
    let first = match &tagged {
        Some(_) => 0, // a tagged group folds no DISTINCT (decompose multi_grouped takes count(DISTINCT) alone)
        None => {
            let (mut layout, first) = OwnerDesc::accumulator_folded(px.arena, &rec.layout, words as u8, &kinds[..], flags);
            // CAP-28: past 64 keys (a streamed group; the owner form holds one flags word) the record is the group keys'
            // image words and their own flags words (one per 64 group keys), each an I64, as the group keys' image
            // (`image_of`) writes them
            if flags && gravity_abi::breaker::flag_words(g.keys.len()) > 1 {
                layout = crate::dict::coderep::image_layout(px.arena, &layout, &crate::dict::coderep::key_kinds(px.gx, g.producer.0 as usize, &g.keys[..arg]), true, &[], layout.has_validity(), px.mcx());
            }
            (rec.layout, rec.nullable) = (layout, flags);
            if flags {
                rec.wpk[arg] = 0;
            }
            first
        }
    };
    let moved = |s: u16| if s == NONE { s } else { s + u16::from(ACC_SLOT_BASE) };
    for a in rec.aggs.iter_mut() {
        (a.d.slot, a.d.dscale_slot) = (moved(a.d.slot), moved(a.d.dscale_slot)); // D2205: a D256 sum's companion slot moves with it
    }
    // the pair count, a Count word (the tagged form's: its argument's tag's slot)
    let count_at = |d: &AggDecl<'_>| tagged.as_ref().map_or(u16::from(ACC_PAIRS), |(t, at)| t.slot(d.count_slot as usize - t.gk, *at));
    let counts = g.aggs.iter().filter(|d| d.final_ == FinalForm::PairCount).map(|d| AggDecl { slot: count_at(d), reg: Register::CountStar, final_: FinalForm::Identity, ..*d });
    let plain = folds.iter().enumerate().map(|(i, &(d, (reg, trans, final_)))| AggDecl { slot: u16::from(first) + i as u16, reg, trans, final_, mods: Mods { distinct: 0, ..d.mods }, ..*d });
    for d in counts.chain(plain) {
        let class = crate::ops::agg::trans::plain::class_of(&rec.node, &d, px.mcx());
        rec.aggs.push_in(Agg { d, class, shape: class.shape(&d), args: Vec::new(), filter: None });
    }
    rec
}
