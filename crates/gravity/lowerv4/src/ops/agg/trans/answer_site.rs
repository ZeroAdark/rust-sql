//! The answer lanes a plain fold asks its source for. Hooks / levers: H02f R2-L01. Budget 100 lines (files-extra.tsv,
//! +25 % gate). Owner M1-FOLD.
//!
//! Split from answer.rs by responsibility (D267): the source's call after its verdicts (`declare_site`), which declares
//! the answer lanes (`nonnull`, the exact i128 sum, min, max) on its drafted site only when every aggregate answers
//! through an answer road that is on and every filter is a zone predicate or the counted qual; a zone-free NotNull zone
//! only on a NOT NULL column's lane or the one lane every aggregate reads. answer.rs folds the answers; registers nothing.
use gravity_decompose::ops::shared::agg::{AggDecl, Register as R};
use gravity_decompose::graph::{Op, Source};
use gravity_program::{LaneDesc, LaneId, SiteDesc, ZoneOp, ZonePred};
use mcx::GrowIn;

use crate::face::fold::AnswerForm;
use crate::face::strategy::{ANSWER_COUNTED, ANSWER_NUMERIC, ANSWER_STATS};
use crate::frame::px::Px;

use crate::ops::agg::trans::answer::form_of;
use crate::ops::agg::trans::strategies::monotone;

/// The source's answer lanes for pipeline `k`'s plain fold (called by the source after its verdicts, on its drafted site):
/// `zoned` bit `j` = filter op `j` is exactly the site's zone predicates. Every aggregate must answer through an answer
/// road that is on, over `count(*)` or a column the site holds; every filter zoned or the counted qual. Returns whether
/// the lanes were declared (the site is untouched otherwise).
pub(crate) fn declare_site(px: &mut Px<'_, '_>, k: usize, site: &mut SiteDesc, zoned: u64) -> bool {
    let gx = px.gx;
    let mcx = px.mcx();
    let p = &gx.pipelines()[k];
    let Some(g) = gx.group_at(k).filter(|g| g.form == gravity_decompose::ops::shared::agg::GroupForm::Plain && crate::ops::agg::trans::kept::count(g) == 0) else { return false };
    let counted = crate::ops::agg::trans::strategies::answer_counted::qual(px, k, site, zoned);
    let filters_ok = p.ops.iter().enumerate().all(|(j, o)| match o {
        Op::Project(_) => true,
        // a filter past the 64th is never zoned: `zoned >> j` would wrap to bit j % 64 (D2521)
        Op::Filter(_) => (j < 64 && zoned >> j & 1 == 1) || counted.is_some_and(|c| c.0 == j),
        _ => false,
    });
    // an answered window decodes no lane: only an end that takes it whole may see one (not under road_off frame.carry, nor
    // beside a call whose ORDER BY entries run per row: D4703)
    if !filters_ok || !px.strategies.has(ANSWER_STATS) || !crate::ops::agg::trans::plain::carries(px) {
        return false;
    }
    // PL-20 (the monotone table's contract (b)): the raising f's an answered window folds at its extremes share one
    // result type, so the window raises PostgreSQL's one message whichever row raised first
    let mut raising = g.aggs.iter().filter_map(|a| monotone::arg(a, mcx)).filter_map(|m| m.raise_ty);
    let first = raising.next();
    if raising.any(|t| Some(t) != first) {
        return false;
    }
    let mut lanes = mcx::reserved_in(g.aggs.len(), mcx);
    // a count's FILTER the FILTER column's record tallies (answer_counted.rs `filter_at`), never beside the
    // counted WHERE qual (the window's count would be the two predicates' joint count, which no record holds)
    let mut filtered = mcx::reserved_in(g.aggs.len(), mcx);
    for a in g.aggs.iter() {
        let Some(form) = form_of(p.node_id, a, mcx) else { return false };
        let road = match form {
            AnswerForm::Stats => ANSWER_STATS,
            AnswerForm::Counted => ANSWER_COUNTED,
            AnswerForm::Numeric => ANSWER_NUMERIC,
        };
        let filt = match a.filter {
            Some(_) if counted.is_none() => match crate::ops::agg::trans::strategies::answer_counted::filter_at(px, k, a, site) {
                Some(f) => Some(f.0),
                None => return false,
            },
            Some(_) => return false,
            None => None,
        };
        filtered.push_in(filt);
        if !(px.strategies.has(road) && crate::face::source::on(crate::admit::refusal::Variant::Strategy(road))) || (form == AnswerForm::Numeric && !crate::ops::agg::trans::strategies::answer_num::fits(a)) {
            return false;
        }
        match arg_lane(px, k, a, site) {
            // NEW-47: an unsigned word's min / max answers only over a 4-byte lane (hosts granule_source `answer`)
            Some(Some(l)) if matches!(a.reg, R::MinImage | R::MaxImage) && site.lanes()[l].width != 4 => return false,
            Some(l) => lanes.push_in(l),
            None => return false,
        }
    }
    if !crate::ops::agg::trans::plain::carries_group(px, k, g) {
        return false;
    }
    // the host answers only a window its zone predicates match whole; with none, `IS NOT NULL` over a NOT NULL column's
    // lane, which matches every window, or over the one lane every aggregate reads (a NULL row adds nothing); over
    // another nullable lane it would skip an all-NULL window count(*) and the others still read. count(*) alone answers it
    let nn = |l: &LaneDesc| matches!((l.id, &p.source), (LaneId::Attr(a), Source::Range { relid, .. }) if gx.facts.att(*relid, a).is_some_and(|f| f.notnull));
    // (count(*) under the counted qual reads only the rows its lane holds)
    let read = |x: &Option<usize>| x.or(counted.map(|c| c.1));
    let one = lanes.first().and_then(read).filter(|&l| lanes.iter().all(|x| read(x) == Some(l)) && counted.is_none_or(|c| c.1 == l));
    let ls = site.lanes();
    // PL-20 (rule 7): a monotone form takes no nullable lane's NotNull zone, which answers only a granule holding no NULL;
    // a window the host decodes then pays its forms' answered path laid out before the row loops (p-many 1.05-1.09x)
    let one = one.filter(|_| !g.aggs.iter().any(|a| monotone::arg(a, mcx).is_some() || a.filter.is_some()));
    let zone = match site.zone.len() {
        0 => match lanes.iter().flatten().copied().find(|&l| nn(&ls[l])).or_else(|| ls.iter().position(nn)).or(one) {
            Some(l) => Some(l),
            None => return false,
        },
        _ => None,
    };
    for l in lanes.iter().flatten().copied().chain(filtered.iter().flatten().copied()).chain(counted.map(|c| c.1)).chain(zone) {
        site.set_answered(l);
    }
    if let Some(l) = zone {
        site.add_zone(ZonePred::word(l as u16, ZoneOp::NotNull, 0));
    }
    true
}

/// `a`'s answer lane on `site`: `Some(None)` for `count(*)`, `Some(Some(lane))` for a bare scan column or PL-20's
/// `MIN / MAX(f(x, c))` of one (strategies/monotone.rs), `None` otherwise.
pub(crate) fn arg_lane(px: &Px<'_, '_>, k: usize, a: &AggDecl<'_>, site: &SiteDesc) -> Option<Option<usize>> {
    let r = a.node.as_aggref()?;
    match r.args.len() {
        0 => Some(None),
        1 => {
            let t = r.args.iter().next()?;
            let c = crate::step::project::bare_var(t.as_target_entry().map_or(t, |t| t.expr)).or_else(|| monotone::arg(a, px.mcx()).map(|m| m.col))?;
            let attno = crate::ops::agg::trans::block_fold::scan_attno(px, k, c)?;
            site.lane_index(LaneId::Attr(attno)).map(Some)
        }
        _ => None,
    }
}
