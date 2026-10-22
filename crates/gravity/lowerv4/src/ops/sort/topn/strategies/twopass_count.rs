//! A count top's two passes, road `twopass.count` (generalize D-06c): a group whose one reader is a top ordered first by
//! a count of the group (`count(*)` / `count(x)`, either direction, LIMIT / OFFSET or WITH TIES, any later keys; lower.rs
//! `count_lead`) and whose folds weigh more than a second read splits as twopass.having's does (twopass.rs `split_by`):
//! pass 1 counts each group, pass 2 folds only the groups whose count ranks with the top's n-th row, every record of
//! pass 1 whose count is at least the n-th best of them all (under ASC at most the n-th smallest; topn raw.rs
//! `admitted`'s `top_count` rule), ties at that count included. The top then cuts pass 2's groups as it cuts one pass's.
//!
//! Exactness. Pass 1's record keys the group by its own key image (`GroupRec::counts` / `bounded` keep the group's key
//! words; a keyed table decides a hit by its image words and an inexact image never keys one: table/bypass.rs), and
//! the owner folds whole partitions, so each record is one group holding its whole count (`count(x)`, or a filtered
//! count, folds its own slot in pass 1: a row count would only bound it). Let T be the n-th best record's count. A
//! group whose count is worse than T follows the n groups counting at least T on the order's first key, so it is none
//! of the first n rows; a WITH TIES row past n shares the n-th row's count; an OFFSET is inside n (count + offset). A
//! HAVING that drops a suffix of the order on that count (count_cut `suffix_cut`) changes nothing: the n groups ahead
//! of a group that passes it pass it too. The admitted set's export (raw.rs `key_of`) may merge two records' keys (a
//! three-word image's fingerprint): that only grows the set, after T is taken over the records. Declined: any other
//! filter before the top (a group it drops could hold one of the n best counts), anything on the group row's way to the
//! top that can raise (`can_raise`) and a fold whose row step can (`fold_quiet`): pass 2 never folds the groups below T,
//! which PostgreSQL computes. Price: the folds over the rows reaching the group against a second read of every row the
//! scan reads (twopass.rs `second_read_pays`), so a selective scan filter keeps one pass. Owner M1-KEYED.
//!
//! 5c-F6: a top led by `sum(x)` splits the same way (lower.rs `top_lead`): pass 1 folds the sum over each group's every
//! row (`GroupRec::led`), the rule ranks the records by it (raw.rs `lead_ranks`), T the n-th best. An integer or numeric
//! sum is exact, its NULL placed by the order (last under a HAVING, which a NULL sum never passes), a numeric OVERFLOW
//! admitted always (pass 2's final raises it: the lead is the one fold allowed to raise). A float sum depends on its
//! fold order, so pass 1 also folds the scaled absolute sum (`AbsSum`) and a record is admitted where any order's sum
//! could rank with T: the n records ranking at T or better under every order are ahead of it in pass 2's order too.
use gravity_decompose::ops::shared::agg::{GroupDecl, GroupForm};
use mcx::PgBox;

use crate::face::end::EndMech;
use crate::frame::px::Px;
use crate::ops::shared::twopass::Rule;
use crate::ops::sort::topn::strategies::twopass::{fold_eighths, second_read_pays, split_by, SPLIT_FLOOR_EIGHTHS};
use crate::table::face::CrossPass;
use crate::table::rec::GroupRec;

crate::strategies! {
    /// A count (or sum) top whose folds outweigh a second read: pass 1 folds the lead, pass 2 folds the groups ranking with
    /// its n-th row.
    TWOPASS_COUNT = 165, "twopass.count", CountTopk;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(TWOPASS_COUNT, &CountSplit)], ..crate::ops::NONE };

/// `twopass.count`'s registry value: `split` (keyed_hashagg `make` asks it before twopass.having's split).
pub(crate) struct CountSplit;

/// The split of graph pipeline `k`'s group `g` (`rec` its record) under its count top, or `None`: one pass, or
/// twopass.having's split. The same shapes as `having_split` decline (a hashed or sorted group of its own, no planner
/// pass, set, cascade base, rerun set or union arm, folds over the split floor that cannot raise).
pub(crate) fn split<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, g: &'g GroupDecl<'p>, rec: &GroupRec<'g, 'p>) -> Option<PgBox<'g, dyn EndMech + 'g>> {
    use crate::ops::shared::twopass::{LEAD_COUNT, LEAD_F32, LEAD_F64};
    let gx = px.gx;
    let eighths: u64 = rec.aggs.iter().map(fold_eighths).sum();
    if !CrossPass::CountTop.on() || !matches!(g.form, GroupForm::Hashed | GroupForm::Sorted) || g.two_pass.is_some() || g.set.is_some() || !crate::graph::grouped(g) || gx.cascade_base(g) {
        return None;
    }
    if eighths <= SPLIT_FLOOR_EIGHTHS || crate::rerun::sets::in_owner_set(gx, k) || px.union_of(k, gx.pipelines()[k].ops.len() + 1).is_some() {
        return None;
    }
    let lead = crate::ops::sort::topn::lower::top_lead(px, k, rec, true, true)?;
    // pass 2 folds no row of a group under T: every fold but the lead, which pass 1 folds over every row, raises nothing
    if !rec.aggs.iter().all(|a| (lead.lead != LEAD_COUNT && a.d.aggno == lead.aggno) || crate::effects::visible::topn::fold_quiet(&a.d, true)) {
        return None;
    }
    // no row estimate keeps one pass
    if !second_read_pays(px, g, eighths, false) {
        return None;
    }
    let (rec1, off) = match (lead.lead, lead.star) {
        (LEAD_COUNT, true) => GroupRec::counts(px, g).map(|r| {
            let off = r.layout.sentinel_off();
            (r, off)
        })?,
        (LEAD_COUNT, false) => GroupRec::bounded(px, g, lead.slot).map(|(r, j)| {
            let off = r.layout.slot_off(j);
            (r, off)
        })?,
        // 5c-F6: a sum lead at slot 0, a float's scaled absolute sum after it (raw.rs `float_slack`)
        (l, _) => {
            let r = GroupRec::led(px, g, lead.slot, matches!(l, LEAD_F64 | LEAD_F32))?;
            let off = r.layout.slot_off(0);
            (r, off)
        }
    };
    let node = gx.pipelines()[k].node_id;
    // a count top's rule word as before 5c-F6 (no NULL to place)
    let rule = Rule { top_count: true, lead: lead.lead, desc: lead.desc, nulls_first: lead.lead != LEAD_COUNT && lead.nulls_first, n: lead.n, ..Rule::of(&rec1, off, node) };
    fires::fire("twopass.count"); // shapecheck fire signal (registry road-fires.tsv)
    if lead.lead != LEAD_COUNT {
        fires::fire("twopass.count.sum"); // 5c-F6: the split's lead is a sum
    }
    Some(split_by(px, k, g, rec1, &rule, None))
}
