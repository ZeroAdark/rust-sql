//! Window / part answers from stats records. Hooks / levers: H02f R2-L01. Budget 160 lines (files.tsv, +25 % gate).
//! Owner M1-FOLD.
//!
//! Registers: road `answer.stats` (`ANSWER_STATS`): a window the source answers from its statistics (granule or
//! whole-part records, pgrc: `WinConds::answered`) folds from the answer lanes' records — `nonnull`, the exact i128 sum,
//! min, max — instead of its rows, which are never decoded. The source answers only a window every zone predicate matches
//! whole, so the fold asks for answer lanes (`declare_site`) only when each of the pipeline's filters is such a predicate
//! (or the counted form's qual, answer_counted.rs). A count(*)-only fold under no filter takes any window whole, and
//! under filters its source covers a covered window (decode.unread, source/pgrc.rs).
use gravity_abi::ctx::{answer, site_off};
use gravity_abi::site::site;
use gravity_decompose::ops::shared::agg::{AggDecl, Register as R};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, IntBinOp, IntCond, Type, Value};
use gravity_program::SiteDesc;
use mcx::{CollectIn, GrowIn, Mcx, PgVec};

use crate::face::fold::{Acc, AnswerForm, AnswerImpl, AnswerMech};
use crate::frame::env::Env;
use crate::frame::frame::Win;
use crate::frame::px::Px;
use crate::frame::row::Carry;

pub(crate) use crate::ops::agg::trans::answer_site::declare_site;
use crate::ops::agg::trans::answer_site::arg_lane;

crate::strategies! {
    /// Window / part answers from stats records.
    ANSWER_STATS = 18, "answer.stats", PgrcPlain;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(ANSWER_STATS, &AnswerImpl(&STATS))], ..crate::ops::NONE };

struct Stats;
static STATS: Stats = Stats;

/// How pipeline `k`'s end takes a window whole: per aggregate its answer lane (none for `count(*)`), the counted lane and
/// its predicate (the count from the lane's record), and whether a count(*)-only fold under no filter takes any window.
pub(crate) struct Plan<'g> {
    lanes: PgVec<'g, Option<usize>>,
    /// Per aggregate, its counted FILTER's lane and predicate (answer_counted.rs `filter_at`).
    filters: PgVec<'g, Option<(usize, crate::ops::agg::trans::strategies::answer_counted::FilterPred)>>,
    counted: Option<(usize, crate::ops::agg::trans::strategies::answer_counted::Pred)>,
    answered: bool,
    count_only: bool,
    /// decode.unread: a count(*)-only fold whose every filter its source covers counts a covered window whole.
    count_covered: bool,
}

/// Pipeline `k`'s end is a plain fold of `count(*)`s under no FILTER that takes windows whole (the carry: without it
/// `whole` never runs, D306), asked by the source for decode.unread. A `count(*)` is the register, not a spelling:
/// decompose's normalize `count_star` gives `count(x)` of a column a VALID NOT NULL constraint guards the same register,
/// so both forms here (and `filter_only`) take it as `count(*)`. D1817: a group with pinned columns (every key dropped by
/// the planner: `WHERE t = 'x' GROUP BY t`) reads them from its first row, so its windows' lanes are read.
pub(crate) fn count_star_only(px: &Px<'_, '_>, k: usize) -> bool {
    let plain = px.gx.group_at(k).filter(|g| g.form == gravity_decompose::ops::shared::agg::GroupForm::Plain && g.pinned.is_empty());
    let stars = plain.filter(|g| !g.aggs.is_empty() && g.aggs.iter().all(|a| a.reg == R::CountStar && a.filter.is_none()));
    stars.is_some_and(|g| crate::ops::agg::trans::plain::carries_group(px, k, g)) // D4703: the end's whole carry rule
}

/// The end's plan (`None`: no window is taken whole): the lanes `declare_site` asked for, read back from the drafted site.
pub(crate) fn plan<'g>(px: &mut Px<'g, '_>, k: usize, folds: &[crate::ops::agg::trans::plain::Fold<'_, '_>]) -> Option<Plan<'g>> {
    let mcx = px.mcx();
    let stars = folds.iter().all(|f| f.a.reg == R::CountStar && f.filter.is_none());
    let count_only = stars && crate::ops::agg::trans::plain::rows_kept(px, k); // D2907: the effects walk only for count(*)s
    // the source decided it (source/pgrc.rs): its host then decodes nothing on a hit, so the fold must count it whole
    let count_covered = !count_only && crate::ops::shared::scan::counts_covered(px, k);
    if count_covered && !stars {
        crate::refuse!(px.gx.pipelines()[k].node_id, crate::admit::refusal::Reason::Bug { payload: "decode.unread over a fold that is not count(*) alone" });
    }
    let arena = px.arena;
    let site = px.site_mut(k).map(|s| s.copy_in(arena));
    let filter_only = site.as_ref().is_some_and(|s| filter_only(s, stars));
    let answered = filter_only || site.as_ref().is_some_and(|s| (0..s.lanes().len()).any(|i| s.answer_lanes.contains(i)));
    let lanes = match (&site, answered) {
        _ if filter_only => mcx::pg_vec![in mcx; None; folds.len()],
        (Some(s), true) => {
            let mut lanes = mcx::reserved_in(folds.len(), mcx);
            for f in folds {
                lanes.push_in(arg_lane(px, k, f.a, s)?);
            }
            lanes
        }
        _ => PgVec::new_in(mcx),
    };
    let counted = site.as_ref().and_then(|s| crate::ops::agg::trans::strategies::answer_counted::qual(px, k, s, u64::MAX)).map(|c| (c.1, c.2)).filter(|_| answered && !filter_only);
    let mut filters = mcx::reserved_in(folds.len(), mcx);
    for f in folds {
        filters.push_in(match (&site, f.a.filter) {
            (Some(s), Some(_)) if answered && !filter_only => Some(crate::ops::agg::trans::strategies::answer_counted::filter_at(px, k, f.a, s)?),
            _ => None,
        });
    }
    (answered || count_only || count_covered).then_some(Plan { lanes, filters, counted, answered, count_only, count_covered })
}

/// D301: count(*) folds (no FILTER) over a site whose every attribute lane is filter-only (zone-covered quals, D293):
/// the host answers a window every row of which passes the zones undecoded, so the fold takes it whole as its rows
/// (`whole`'s count(*) = `end - cursor`), never walking lanes it left null. The attribute lanes are the site's first
/// `natts` (an offset or verdict lane follows them, never filter-only).
fn filter_only(s: &SiteDesc, stars: bool) -> bool {
    stars && !s.zone.is_empty() && s.natts() > 0 && (0..s.natts()).all(|i| s.filter_lanes.contains(i))
}

/// `EndMech::whole` for the plain end: an answered window's carry from its records (a counted window's rows are its
/// predicate's count from the lane's record); a count(*)-only window's `end - cursor`, or a covered one's under
/// decode.unread. True iff a branch to `l_wend` was emitted.
pub(crate) fn whole(b: &mut FunctionBuilder<'_>, env: &mut Env, end: &crate::ops::agg::trans::plain::PlainEnd<'_, '_>, win: &Win, _c: &Carry, l_wend: Block) -> bool {
    let Some(p) = end.answer.as_ref() else { return false };
    let nwin = b.int_bin(IntBinOp::Sub, win.end, win.cursor);
    let mut any = false;
    if let (true, Some(a), Some(sk)) = (p.answered, win.facts.answered, env.site) {
        let (l_ans, l_no) = (b.create_block(&[]), b.create_block(&[]));
        let yes = b.icmp(IntCond::Ne, a, env.zero);
        b.cond_br(yes, (l_ans, &[]), (l_no, &[]));
        b.switch_to(l_ans);
        fires::fire("answer.stats"); // shapecheck fire signal (REGISTRY RD-answer.stats)
        let ans = b.load(Type::Ptr, env.ctx, site_off(sk) as i32 + site::ANSWER as i32);
        let rows = match p.counted {
            Some((l, pred)) => crate::ops::agg::trans::strategies::answer_counted::counted(b, ans, l, pred),
            None => nwin,
        };
        let mut words: mcx::PgVec<'_, Value> = mcx::PgVec::new_in(env.mcx);
        for ((f, lane), filt) in end.folds.iter().zip(p.lanes.iter()).zip(p.filters.iter()) {
            let mut w = mcx::pg_vec![in env.mcx; env.zero; f.words];
            match (f.a.reg, lane, *filt) {
                (R::CountStar | R::CountAny, _, Some((l, pred))) => w[0] = crate::ops::agg::trans::strategies::answer_counted::filtered(b, ans, l, pred, rows, f.a.reg == R::CountAny),
                (R::CountStar, _, _) => w[0] = rows,
                (R::CountAny, _, _) if p.counted.is_some() => w[0] = rows,
                (_, Some(l), _) => {
                    let rec = b.ptr_add(ans, env.zero, 1, (*l * answer::SIZE) as i32);
                    match f.mono.as_ref() {
                        Some(m) => crate::ops::agg::trans::strategies::monotone::answer(b, env, m, f.a.reg == R::MinInt, rec, &mut w),
                        None => {
                            let form = f.class.answer(f.a).unwrap_or(AnswerForm::Stats);
                            let mech = form.need::<AnswerImpl>(&env.node).0;
                            mech.fold(b, env, f.a, Acc::Regs(&mut w), rec);
                        }
                    }
                }
                _ => crate::refuse!(env.node, crate::admit::refusal::Reason::Bug { payload: "an answered aggregate without its lane" }),
            }
            words.extend_in(w);
        }
        let params = core::iter::once(rows).chain(words).collect_in(env.mcx);
        b.br(l_wend, &params);
        b.switch_to(l_no);
        any = true;
    }
    if p.count_covered {
        fires::fire("answer.covered"); // shapecheck fire signal (REGISTRY FOLD-04 / PL-23)
        // the source's host decodes nothing in a hit window: one the fold cannot tell covered would walk null lanes
        let Some(cv) = win.facts.covered else { crate::refuse!(env.node, crate::admit::refusal::Reason::Bug { payload: "decode.unread without a covered fact" }) };
        let (l_cnt, l_no) = (b.create_block(&[]), b.create_block(&[]));
        let c = crate::frame::frame::all(b, env, &[cv]).unwrap_or(cv);
        b.cond_br(c, (l_cnt, &[]), (l_no, &[]));
        b.switch_to(l_cnt);
        let params = mcx::pg_vec![in env.mcx; nwin; 1 + end.folds.len()];
        b.br(l_wend, &params);
        b.switch_to(l_no);
        any = true;
    }
    if p.count_only {
        fires::fire("answer.count_only"); // shapecheck fire signal (REGISTRY FOLD-04 / PL-23)
        let params = mcx::pg_vec![in env.mcx; nwin; 1 + end.folds.len()];
        b.br(l_wend, &params);
        let dead = b.create_block(&[]);
        b.switch_to(dead);
        any = true;
    }
    any
}

impl AnswerMech for Stats {
    fn declare(&self, px: &mut Px<'_, '_>, _site: &mut SiteDesc, a: &AggDecl<'_>) -> bool {
        form_of(0, a, px.mcx()) == Some(AnswerForm::Stats)
    }

    /// The window's carry words from lane record `rec`: a count's `nonnull`, a sum's exact i128 (its low word for an
    /// int8 sum), min / max (the identity over an all-NULL window), each with `nonnull` rows (PL-20's `f(x, c)`: the
    /// end's `whole`, strategies/monotone.rs).
    fn fold(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>, rec: Value) {
        let Acc::Regs(w) = acc else { crate::refuse!(env.node, crate::admit::refusal::Reason::Bug { payload: "an answer into a slot" }) };
        let nonnull = b.load(Type::I64, rec, answer::NONNULL as i32);
        let any = b.icmp(IntCond::Ne, nonnull, env.zero);
        let k = w.len() - 1;
        w[k] = nonnull;
        match a.reg {
            R::CountAny => {}
            R::MinInt | R::MaxInt | R::MinImage | R::MaxImage => {
                let (off, id) = if matches!(a.reg, R::MinInt | R::MinImage) { (answer::MIN, i64::MAX) } else { (answer::MAX, i64::MIN) };
                let mut m = b.load(Type::I64, rec, off as i32);
                if matches!(a.reg, R::MinImage | R::MaxImage) {
                    // NEW-47: an unsigned 4-byte lane's record holds the word as the row path reads it (hosts granule_source
                    // `answer`, answer_site.rs); the register holds its flipped image
                    let flip = b.iconst(Type::I64, i64::MIN);
                    m = b.int_bin(IntBinOp::Xor, m, flip);
                }
                let id = b.iconst(Type::I64, id);
                w[0] = b.select(any, m, id);
            }
            _ => {
                w[0] = b.load(Type::I64, rec, answer::SUM_LO as i32);
                if k == 2 {
                    w[1] = b.load(Type::I64, rec, answer::SUM_HI as i32);
                }
            }
        }
    }
}

/// The answer form `a`'s class folds an answered window by, if any.
pub(crate) fn form_of(node: i32, a: &AggDecl<'_>, mcx: Mcx<'_>) -> Option<AnswerForm> {
    crate::ops::agg::trans::class_of::class_in(&node, a, true, mcx).answer(a)
}

#[cfg(test)]
mod tests {
    use gravity_program::{Arena, SiteDesc, ZoneOp, ZonePred};

    /// D301: count(*) takes an answered window whole only over a range site (its attribute lanes, then the offset lane)
    /// whose every attribute lane is filter-only under a zone.
    #[test]
    fn a_count_star_over_filter_only_lanes_takes_answered_windows_whole() {
        let arena = Arena::new(1 << 16);
        let site = |attnos: &[i16], filter: &[usize], nzone: usize| {
            let mut s = SiteDesc::range_source(arena.handle(), 16384, attnos);
            filter.iter().for_each(|&l| s.set_filter_only(l));
            (0..nzone).for_each(|_| s.add_zone(ZonePred::word(0, ZoneOp::NotNull, 0)));
            s
        };
        assert!(super::filter_only(&site(&[1], &[0], 1), true), "the site's only attribute lane filter-only (D293), the offset lane beside it");
        assert!(super::filter_only(&site(&[1, 3], &[0, 1], 2), true), "every attribute lane filter-only");
        assert!(!super::filter_only(&site(&[1, 3], &[0], 1), true), "an attribute lane the kernel reads");
        assert!(!super::filter_only(&site(&[1], &[0], 0), true), "no zone: the host answers no window");
        assert!(!super::filter_only(&SiteDesc::none_in(arena.handle()), true), "no attribute lane");
        assert!(!super::filter_only(&site(&[1], &[0], 1), false), "an aggregate other than count(*), or a FILTER");
    }
}
