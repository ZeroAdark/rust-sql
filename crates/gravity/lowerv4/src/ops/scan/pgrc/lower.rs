//! The pgrc granule range source (KWC lowerv3 scan/range.rs, with lowerv2 source/range.rs's window facts): one window
//! loop over a relation's pgrc site. The claim's `[lo, hi)` seeks at the open and `advance` stages one granule per window
//! (the host's GranuleRangeSource: its verdicts, lane decode, deletes as the selection); a row's cells are the site's
//! attribute lanes at the row's index through the selection, each cell's value its element's address.
//!
//! The make drafts the site (`Px::set_site`): the decomposer's zone predicates are cleared and every verdict road that is
//! on is asked for each leading filter qual (`VerdictMech::declare`), so the host answers exactly the declared verdicts.
//! A leading filter op whose every qual an `AllMatch` verdict covers is named by `covers`; the frame elides it in a window
//! whose verdict says so (`WinConds::covered`). Filter-only lanes are decoded (the elision road is `lane_elide.rs`).
//!
//! The site-window reader is the core's (frame/site.rs), shared with `source/index.rs`. Registers kind `src.pgrc`. Owner
//! M1-SOURCE.
//!
//! `ctid` (the decomposer's trailing RowId lane, taken off the site: on a pgrc range site that lane names topk.fetch's
//! by-rowid site): no host lane carries it. A pgrc row's ctid is its stable ID (pgrc_format rowid IX0), which only the
//! AM's scan stamps: `make_with` sends a scan that reads ctid, or samples (D4735), down the rows road (PGRCSERVE M3).
//! `Pgrc::tid` computes a position, not that ID; it stays for a by-ID ctid lane.
use core::cell::RefCell;
use std::rc::Rc;

use gravity_abi::ctx::{lane, site_off};
use gravity_abi::site::site;
use gravity_decompose::{Am, Op, Source};
use gravity_expr::emit::LaneRef;
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::{Block, ConvOp, IntBinOp, IntCond, Type, Value};
use gravity_program::protos::{code_sparse_table_arg, code_table_arg, const_key_arg, word_zone_of, Proto, ProtoKind, VERDICT_BLOOM, VERDICT_CLAIM, VERDICT_COND, VERDICT_COUNT_ONLY, VERDICT_ELIDE, VERDICT_ELIDE_HIT, VERDICT_GUARD_SEL, VERDICT_LATE, VERDICT_PACKED, VERDICT_PSMA, VERDICT_UNREAD, VERDICT_WORD_ZONE, VERDICT_ZONE};
use gravity_program::{LaneDesc, LaneId as SiteLane, Parallel, Pipeline, Reason as Why, SiteDesc, TableDecl, ZoneOp, ZonePred};
use mcx::{CollectIn, GrowIn, PgBox, PgVec};
use types_nodes::Node;

use crate::dict::face::DictPolicy;
use crate::face::kinds::{Face, KindId};
use crate::face::source::{DecodeOrder, LaneForm, Locator, Packed, ReadForm, ReadPlan, SourceMech, VerdictImpl, VerdictKind, WinConds};
use crate::frame::env::Env;
use crate::frame::frame::{SourceMaker, Win, WordWin};
use crate::frame::px::{ExId, Px, DRAFT_SITE};
use crate::frame::row::{Carry, Mask, RowVals};
use crate::frame::site::{Reader, CTID_AT};
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::ops::Table;
use crate::ops::shared::scan::SourceExt;
use crate::ops::scan::pgrc::verdict_zone::OpenParam;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::SrcPgrc, &SourceMaker(make))], ..crate::ops::NONE };

pub(crate) fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

// ------------------------------------------------------------------------------------------ the source face's state

/// The plan node of leading qual `q`, its scan's varno and its pipeline, as the source's make recorded them.
pub(crate) fn qual_node<'p>(px: &Px<'_, 'p>, q: ExId) -> Option<(Node<'p>, i32, usize)> {
    let &(_, k, j, i) = px.ext::<SourceExt>(Face::Source)?.quals.iter().find(|e| e.0 == q)?;
    let p = &px.gx.pipelines()[k];
    let Source::Range { scanrelid, .. } = &p.source else { return None };
    let Op::Filter(n) = p.ops[j] else { return None };
    let n = match n.as_list() {
        Some(l) => l.iter().nth(i)?,
        None => n,
    };
    Some((n, *scanrelid as i32, k))
}

/// What the leading filters' quals take on the drafted site: the covered ops, the host's enables (`VERDICT_*`), the
/// condition cache's fingerprint inputs.
pub(crate) struct Declared<'g> {
    /// Bit j: op j is covered in a window whose verdict says so (the condition cache's ops when it is on, else the zone's).
    pub covers: u64,
    /// The zone's `AllMatch` covers `covers` too.
    pub zone_covers: bool,
    pub enables: u64,
    /// `(the covered words' runs, static fingerprint)` when the condition cache takes a leading op (verdict_cond.rs).
    pub cond: Option<(PgVec<'g, crate::ops::scan::pgrc::verdict_cond::Run>, [u64; 2])>,
    /// The late lanes (pgrc_late.rs; bit l: site lane l).
    pub late: u64,
    /// The packed body's candidates (pgrc_packed.rs; bit l: site lane l).
    pub packed: u64,
    /// The candidates no op after the first and no end reads: a block's survivors never fill them.
    pub first_only: u64,
    /// The plain fold takes a window its zone matches whole from the answer lanes' records (fold/answer.rs, H02 f).
    pub answered: bool,
    /// The code-range verdict of one leading qual (verdict_coderange.rs), in leading op `.0`.
    pub range: Option<(usize, crate::dict::strategies::verdict_coderange::Range)>,
    /// D289: the word-valued zone predicates (`lane op $n`, verdict_zone.rs `word_of`): `(arg, word)` each.
    pub wzone: PgVec<'g, (u64, u16)>,
    /// PS1-05: the open-time zone predicates (`lane op (SELECT ..)`, verdict_zone.rs `init_of`): `(lane, op, typlen,
    /// paramid, source)` each (D3008: a rescan parameter's too); `zargs` their src.verdicts args once make_with gave each its key
    /// cell (`word_zone_key_arg`).
    pub izone: PgVec<'g, (u16, u8, u8, i32, OpenParam)>,
    pub zargs: PgVec<'g, u64>,
    /// NEW-72: the slots that bind the site's bound static predicates (`ZonePred::bound`): `(arg, word)` each
    /// (`word_zone_at_arg`), and the IN lists' (`WORD_ZONE_LIST`, verdict_zone.rs `list_of`): `(arg, lane, word)` each.
    pub bound: PgVec<'g, (u64, u16)>,
    pub lists: PgVec<'g, (u64, u16, u16)>,
}

/// Every verdict road that is on, asked for each leading filter qual of pipeline `k` over the drafted `site`. An op is
/// covered by a verdict when each of its quals is, and every earlier op too. The site's filter lanes come in as the
/// decomposer's and leave as the elided ones (lane_elide.rs). `tids`: the site has a TID scan's key cells (`tid_bounds`).
pub(crate) fn declare_verdicts<'g>(px: &mut Px<'g, '_>, k: usize, site: &mut SiteDesc, tids: bool) -> Declared<'g> {
    let gx = px.gx;
    let mcx = px.mcx();
    let p = &gx.pipelines()[k];
    let node = p.node_id;
    let Source::Range { relid, .. } = &p.source else {
        site.filter_lanes.clear();
        let (wzone, izone, zargs, bound, lists) = (PgVec::new_in(mcx), PgVec::new_in(mcx), PgVec::new_in(mcx), PgVec::new_in(mcx), PgVec::new_in(mcx));
        return Declared { covers: 0, zone_covers: false, enables: 0, cond: None, late: 0, packed: 0, first_only: 0, answered: false, range: None, wzone, izone, zargs, bound, lists };
    };
    let filters = p.ops.iter().take_while(|op| matches!(op, Op::Filter(_))).count();
    let lead = filters.min(64);
    let (mut zone_ops, mut cond_ops, mut enables, mut cond_zones) = (0u64, 0u64, 0u64, 0usize);
    // sortmerge (D4469 road 2): a merged site's windows are cut into runs (hosts `MergedRangeSource`), so its verdicts only
    // skip granules and select rows: no answer, no condition cache (a hit is learned from a whole window), no elided, late,
    // packed, unread or count-only lane
    let merged = site.is_merged();
    // D3126: the ops whose every qual a zone verdict decides per granule: a static zone predicate, a D289 word predicate or
    // an open-time one (the condition cache yields to a STABLE open-time skip over them, below)
    let mut decided = 0u64;
    let mut shapes = PgVec::new_in(mcx);
    // D3938: what made the covered quals' verdicts (verdict_cond.rs `finish`): their recorded catalog reads, their producers
    let mut made = (gravity_deps::Deps::default(), PgVec::new_in(mcx));
    // a nest or SubPlan inner (an embedded pipeline, `Pipes::inners`) covers no op: its source is re-opened per outer row
    // and rerun/inner.rs reads no covered window, so neither the zone's AllMatch nor the condition cache covers its filters
    let embedded = k >= gx.pipelines().len();
    // The cover masks are u64 (one bit per leading filter op): past 64 leading filters the cache could cover only the first
    // 64, and in a hit window every later op then runs per row at several times its cost without the cache (100 literal
    // text quals: 181 -> 262 ms), so a wider filter keeps no cache. An EXPLAIN ANALYZE counts no node whose rows leave
    // before a leading filter (lowerv4 `instr::plan_pipeline`), so a skipped window changes no count it prints
    let cond_on = !embedded && !merged && VerdictKind::CondCache.on() && filters <= 64;
    // a verdict skips a window on qual i only while every earlier qual raises nothing: PG runs them on its rows first (D237, D454)
    let (mut range, mut quiet) = (None, true);
    // D3126: past a qual that may raise only in its STABLE comparand (one the open evaluates, `raises_in_comparand`), a
    // later qual's STABLE open-time predicate is still declared, guarded: the open adds it only when no earlier comparand
    // raised (lower.rs `izone_keys`); an open that raises nothing proves no row raises there. Any other verdict stays off.
    let mut open_quiet = true;
    // H08 / P5-04 / SRC-22: code.sparse's table form, the first leading qual answered whole by a host-built per-code table
    let mut sparse_table = None;
    // P5-03 (the code verdict): the first such table that can reject a part (`rejects_parts`), else none
    let mut verdict_table: Option<(u16, usize)> = None;
    let mut verdict_op = 0usize;
    // answer.counted (ops/agg/trans/strategies/answer_counted.rs `qual_at`): the plain fold's counted candidate, found
    // structurally: filter op j's qual i a constant-free predicate its integral lane's granule record tallies (summary.rs:
    // `x IS NOT NULL`), every aggregate a count. SRC-21: its qual takes its verdicts as any qual does (NoneMatch skips, AllMatch covers), and once
    // the fold takes the answer over the ops the zone covers (`answer_declare_site` after the loop) its zone predicate is
    // marked `tallied`: the host then answers a window that predicate leaves Mixed when every other zone predicate matches
    // it whole (the count is the record's tally, exact in any window), and decodes any other window with the true
    // verdicts, its filter run per row. The site rule names no predicate: one the summary learns needs nothing here.
    // (an EXPLAIN ANALYZE's frame counts a window the records answer as the rows its end takes: frame.rs `row_loop`)
    let counted = if merged { None } else { crate::frame::iface::answer_counted_qual(px, k, site, u64::MAX) };
    let mut counted_q = None;
    let mut wzone: PgVec<'_, (usize, u64, u16)> = PgVec::new_in(mcx);
    let mut izone: PgVec<'_, (u16, u8, u8, i32, OpenParam)> = PgVec::new_in(mcx);
    let mut lists: PgVec<'_, (usize, u64, u16, u16)> = PgVec::new_in(mcx);
    for j in 0..lead {
        let qs = crate::step::expr::quals(px, k, j);
        let nodes: PgVec<'_, Node<'_>> = match p.ops[j] {
            Op::Filter(n) => n.as_list().map_or_else(|| mcx::pg_vec![in mcx; n], |l| l.iter().collect_in(mcx)),
            _ => PgVec::new_in(mcx),
        };
        let ext = px.ext_mut::<SourceExt>(Face::Source);
        ext.quals.extend(qs.iter().enumerate().map(|(i, &q)| (q, k, j, i))); // alloc: Program
        let (mut all, mut cached, mut dec) = (true, mcx::reserved_in(qs.len(), mcx), true);
        for (i, &q) in qs.iter().enumerate() {
            if counted.is_some_and(|c| (c.0, c.1) == (j, i)) {
                counted_q = Some(q);
            }
            // NEW-72: a by-reference comparand a road binds per execution has its word before the roads declare
            if quiet {
                crate::ops::scan::pgrc::verdict_zone::offer_words(px, site, q);
            }
            let zoned = quiet && declare(px, site, VerdictKind::Zone, q, node);
            // NEW-72: an IN list's members are the execution's: its slot binds the array's word, and exec's prepare adds
            // the members' span and the members (the bloom's) after the static predicates
            if let Some((lane, ef, a)) = zoned.then(|| crate::ops::scan::pgrc::verdict_zone::list_of(px, site, q)).flatten() {
                if let Some(w) = crate::ops::scan::pgrc::verdict_zone::list_word(px, a).filter(|&w| !lists.iter().any(|x| x.3 == w)) {
                    lists.push_in((j, gravity_program::protos::word_zone_list_arg(ef.word() as u64), lane, w));
                }
            }
            // sortpath (4): an IN list of text over a jsonb path lane: its members' prefix-key span is the execution's
            if let Some((lane, ef, a)) = zoned.then(|| crate::ops::scan::pgrc::verdict_zone::prefix_list_of(px, site, q)).flatten() {
                if let Some(w) = crate::ops::scan::pgrc::verdict_zone::list_word(px, a).filter(|&w| !lists.iter().any(|x| x.3 == w)) {
                    lists.push_in((j, gravity_program::protos::word_zone_list_arg(ef.word() as u64) | gravity_program::protos::WORD_ZONE_LIST_PREFIX, lane, w));
                }
            }
            // D289: a generic plan's `lane op $n`, a zone predicate the host adds per execution (never the static site's)
            let word = if !quiet || zoned || !VerdictKind::Zone.on() { None } else { crate::ops::scan::pgrc::verdict_zone::word_of(px, site, q) };
            if let Some((arg, w)) = word.filter(|&(a, w)| !wzone.iter().any(|x| (x.1, x.2) == (a, w))) {
                wzone.push_in((j, arg, w));
            }
            // PS1-05: `lane op (SELECT ..)` over an eager-safe InitPlan, a zone predicate the host adds at each seek from the
            // value the open stores (road nest.eagersafe)
            let open_time = word.is_none() && (quiet || open_quiet) && !zoned && VerdictKind::Zone.on();
            // D2992: else `lane op e` over a STABLE comparand the open evaluates (D3126: past a guard, only that, guarded)
            let open_z = || match quiet {
                true => crate::ops::scan::pgrc::verdict_zone::init_of(px, site, q).or_else(|| crate::ops::scan::pgrc::verdict_zone::stable_of(px, site, q)),
                false => crate::ops::scan::pgrc::verdict_zone::stable_of(px, site, q).map(|mut z| {
                    if let OpenParam::Stable { guarded, .. } = &mut z.4 {
                        *guarded = true;
                    }
                    z
                }),
            };
            let entry = open_time.then(open_z).flatten();
            if let Some(z) = entry.filter(|z| !izone.contains(z)) {
                izone.push_in(z);
            }
            let bloomed = quiet && declare(px, site, VerdictKind::Bloom, q, node);
            enables |= VERDICT_BLOOM * bloomed as u64;
            // NEW-72: an IN list over a byte-equality lane: its members' probe words are the execution's (the bloom's)
            if let Some((lane, ef, a)) = bloomed.then(|| crate::ops::scan::pgrc::verdict_zone::hash_list_of(px, site, q)).flatten() {
                if let Some(w) = crate::ops::scan::pgrc::verdict_zone::list_word(px, a).filter(|&w| !lists.iter().any(|x| x.3 == w)) {
                    lists.push_in((j, gravity_program::protos::word_zone_list_arg(ef.word() as u64) | gravity_program::protos::WORD_ZONE_LIST_HASH, lane, w));
                }
            }
            enables |= VERDICT_PSMA * (quiet && declare(px, site, VerdictKind::Psma, q, node)) as u64;
            if quiet && verdict_table.is_none() && VerdictKind::CodeSparse.on() {
                if let Some(t) = crate::dict::strategies::code_sparse::table(px, site, q) {
                    if sparse_table.is_none() {
                        sparse_table = Some(t);
                    }
                    if rejects_parts(site, t) {
                        (verdict_table, verdict_op) = (Some(t), j);
                    }
                }
            }
            if quiet && VerdictKind::CodeRange.on() && range.is_none() {
                range = crate::dict::strategies::verdict_coderange::of(px, q).map(|r| (j, r));
            }
            dec &= zoned || word.is_some() || entry.is_some();
            let q_quiet = nodes.get(i).is_some_and(|&n| crate::effects::visible::pgrc::quiet_qual(gx, n));
            let guard = entry.is_some_and(|z| matches!(z.4, OpenParam::Stable { .. })) && nodes.get(i).is_some_and(|&n| crate::effects::visible::pgrc::raises_in_comparand(gx, n));
            if !q_quiet && guard && quiet {
                fires::fire("verdict.zone.guard"); // shapecheck fire signal (D3126: a later STABLE predicate guarded by this open's)
            }
            open_quiet &= q_quiet || guard;
            quiet &= q_quiet;
            all &= !embedded && zoned && declare(px, site, VerdictKind::AllMatch, q, node);
            if let Some(c) = (cond_on && quiet).then(|| crate::ops::scan::pgrc::verdict_cond_key::covered(px, k, site, q, node)).flatten() {
                cached.push_in(c);
            }
        }
        decided |= (dec as u64) << j;
        if all && zone_ops == (1u64 << j) - 1 {
            zone_ops |= 1 << j;
        }
        if cached.len() == qs.len() && cond_ops == (1u64 << j) - 1 {
            cond_ops |= 1 << j;
            for (s, d, p) in cached {
                shapes.push_in(s);
                made.0.absorb(&d);
                made.1.extend_from_slice_in(&p);
            }
            cond_zones = site.zone.len();
        }
    }
    // the plain fold's answer lanes over the ops the zone covers exactly (and the counted qual's op); it may add the
    // NotNull zone of a lane
    let answered = !merged && crate::frame::iface::answer_declare_site(px, k, site, zone_ops);
    // SRC-21: the counted qual's zone predicate is tallied once the fold takes the answer (its lane an answer lane), and
    // the tallied lane is never elided: it is the answer's (a count(*) over filter-only lanes alone counts a window's rows,
    // answer.rs `filter_only`). The condition cache decides only the windows the tally leaves (another zone predicate
    // Mixed, a delete mask): a site whose one zone predicate is the tallied one has none, so it keeps no cache
    let tallied = counted.filter(|c| answered && site.answer_lanes.contains(c.2)).and_then(|c| Some((c, crate::ops::scan::pgrc::verdict_zone::of(px, site, counted_q?)?)));
    let tallied = tallied.and_then(|(c, (z, exact))| {
        let at = site.zone.iter().position(|p| *p == z).filter(|_| exact && z.lane as usize == c.2 && z.op == c.3.zone_op() && !z.bound && !z.prefix)?;
        site.zone[at].tallied = true;
        Some(c.2)
    });
    // the host drops a row on any zone predicate (a PSMA window, a selection), so the cached words are the covered quals'
    // verdict only while every zone predicate is one of theirs: one of a later op or the answer's NotNull turns it off
    // each word-valued predicate is a zone predicate of its op for the rule above
    let wz: PgVec<'_, (u64, u16)> = wzone.iter().map(|&(_, a, w)| (a, w)).collect_in(mcx);
    // NEW-72: an IN list's per-execution predicates are its op's, as a word-valued one is
    let cond_words = wzone.iter().all(|&(j, ..)| cond_ops & (1 << j) != 0) && lists.iter().all(|&(j, ..)| cond_ops & (1 << j) != 0);
    let cw: PgVec<'_, (u64, u16)> = wz.iter().copied().chain(lists.iter().map(|&(_, a, _, w)| (a, w))).collect_in(mcx);
    // D3938: quals read under a state other sessions may not see (this thread's uncommitted DDL, a lagged view) share
    // nothing: no cache
    let stamp = made.0.committed_stamp();
    let cond = (cond_ops != 0 && site.zone.len() == cond_zones && cond_words && !(tallied.is_some() && site.zone.len() == 1)).then(|| crate::ops::scan::pgrc::verdict_cond::finish(&shapes[..], *relid, site, &cw[..], (stamp?, &made.1[..]), mcx)).flatten();
    // D2992: but a STABLE comparand's open-time skip outweighs a cache whose every covered op the zone verdicts already
    // decide per granule (word compares: the cache would spare only a Mixed granule's compares), so such a site drops the
    // cache and keeps the predicate. D3126: a D289 word predicate's op (a generic plan's `year = $1`) and an open-time
    // predicate's are decided too, not only a static zone predicate's
    let stable = izone.iter().any(|z| matches!(z.4, OpenParam::Stable { .. }));
    let cond = cond.filter(|_| !stable || cond_ops & !(zone_ops | decided) != 0);
    // PS1-05: the condition cache's key cannot hash an open-time value, and its covered words are a verdict only while
    // every zone predicate is one of theirs: a site the cache takes keeps today's form (no open-time predicate); a TID
    // scan's (`tids`: key cells of its own) too
    if cond.is_some() || tids {
        izone.clear();
    }
    // PS1-05 mechanism C: the open runs only the InitPlans whose planner charge stays under the scan's own (decompose
    // `PipelineGraph::eager_initplans`); a planner cost is an estimate, so the decision is a guard of the program (lower
    // inputs/key.rs `cost_decide`). Any other stays lazy: the filter runs it at the first row that reaches the qual
    // D3008: a rescan parameter's predicate runs nothing at the open (its producer wrote the word), so no charge decides it
    if izone.iter().any(|z| matches!(z.4, OpenParam::Init(_))) {
        let ids: PgVec<'_, i32> = izone.iter().filter(|z| matches!(z.4, OpenParam::Init(_))).map(|z| z.3).collect_in(mcx);
        let eager = gravity_lower::inputs::key::cost_decide(gx.graph, node, &ids[..]);
        let mut i = 0u32;
        izone.retain(|z| {
            if !matches!(z.4, OpenParam::Init(_)) {
                return true;
            }
            i += 1;
            eager.checked_shr(i - 1).is_some_and(|m| m & 1 != 0)
        });
    }
    let covers = if cond.is_some() { cond_ops } else { zone_ops };
    // a skipped window publishes no survivor: under the condition cache only a covered op's qual skips
    let range = range.filter(|&(j, _)| cond.is_none() || cond_ops & (1 << j) != 0);
    let zone_covers = covers != 0 && covers & !zone_ops == 0;
    if covers != 0 {
        // an end that takes a covered window whole reads which ops it covers (dict countwalk.rs, KEYED-14)
        px.ext_mut::<SourceExt>(Face::Source).covers.push((k, covers)); // alloc: Program
    }
    // decode.unread: a plain count(*)-only end over ops that are all projections or covered filters counts a
    // covered window whole (fold/answer.rs `count_covered`), reading no lane and no row of it, so on a condition-cache hit
    // the host decodes nothing (VERDICT_UNREAD). The site's lanes past its attributes may only be its offset lane (every
    // range site's, SiteDesc::range_source) and per-code tables: never the rowid lane
    let natts = site.natts();
    let every = p.ops.iter().enumerate().all(|(j, o)| match o {
        Op::Project(_) => true,
        Op::Filter(_) => j < 64 && covers >> j & 1 != 0,
        _ => false,
    });
    if !merged && covers != 0 && every && crate::frame::iface::answer_count_star_only(px, k) && site.lanes()[natts..].iter().all(|l| matches!(l.id, SiteLane::Offset | SiteLane::Table(_))) {
        px.ext_mut::<SourceExt>(Face::Source).whole.push(k); // alloc: Program
        enables |= VERDICT_UNREAD * cond.is_some() as u64;
    }
    // VERDICT_ELIDE_HIT (V4R-4): the condition cache covers an op the zone does not (a numeric or other
    // non-zone qual), so the covered ops' filter-only lanes are elided only in a window a hit decided (the frame's
    // covered one, the zone's AllMatch not among them); the host decodes them in every other window and answers none
    let hit_only = !zone_covers && cond.is_some();
    let mut elide = match (zone_covers || hit_only) && VerdictKind::Elide.on() && !merged {
        true => crate::ops::scan::pgrc::lane_elide::lanes(px, k, site, covers, hit_only),
        false => PgVec::new_in(mcx),
    };
    // sortpath: + the path lanes only covered compares read (a covered window skips them beside a document read)
    if (zone_covers || hit_only) && VerdictKind::Elide.on() && !merged {
        for l in crate::ops::scan::pgrc::lane_elide::path_lanes(px, k, site, covers) {
            if !elide.contains(&l) {
                elide.push_in(l);
            }
        }
    }
    elide.retain(|&l| Some(l) != tallied);
    // D301: an AllMatch window whose every attribute lane is filter-only (or answered) the host answers, decoding nothing
    // and publishing no lane (granule_source's answered form, stage_part's whole part); only an end that takes answered
    // windows (the fold's answer plan) may see one. Without that plan the cheapest lane stays decoded (a by-value lane
    // before a coded or by-reference one, then the narrowest), so every window is one the frame reads. A site with no
    // zone predicate has no AllMatch zone verdict, so the host answers none of its windows. Under the hit-only elision the
    // host answers none either, and a hit's window still reads that lane (only decode.unread's count reads none)
    if !answered && (!site.zone.is_empty() || hit_only) && natts > 0 && (0..natts).all(|l| elide.contains(&l)) {
        let ls = site.lanes();
        let keep = (0..natts).min_by_key(|&l| (ls[l].kind != lane::KIND_BYVAL, ls[l].width));
        elide.retain(|&l| Some(l) != keep);
    }
    // the decomposer's filter-only lanes the first op alone reads (before the site keeps only the elided ones)
    let first_only = crate::ops::scan::pgrc::lane_elide::first_only(px, k, site);
    let first_quals = crate::ops::scan::pgrc::lane_elide::lanes(px, k, site, 1, true).iter().filter(|&&l| l < 64).fold(0u64, |m, &l| m | 1 << l); // M6 R3: + qual_only
    site.filter_lanes.clear();
    for &l in elide.iter() {
        site.set_filter_only(l);
    }
    if !elide.is_empty() {
        enables |= VERDICT_ELIDE | VERDICT_ELIDE_HIT * hit_only as u64;
    }
    // D3042, count.only: a plain count(*)-only end over projections alone (the fold's `count_only` plan: answer.rs `whole`
    // counts every window `end - cursor` and reads no lane) over a site with no predicate of any kind, its attribute lanes
    // by-value (no dictionary a window names) and past them only its offset lane: the host takes a part, or a granule, as
    // one window of its live rows and reads no lane (granule_source `count_only`). A count(*) the stats answer declines
    // read its driving lane whole (a wide key column: hundreds of MB cold)
    let ls = site.lanes();
    if !merged && site.zone.is_empty() && site.zone_any.is_empty() && wz.is_empty() && izone.is_empty() && lists.is_empty() && cond.is_none() && p.ops.iter().all(|o| matches!(o, Op::Project(_)))
        && crate::frame::iface::answer_count_only(px, k)
        && ls[..natts].iter().all(|l| l.kind == lane::KIND_BYVAL) && ls[natts..].iter().all(|l| matches!(l.id, SiteLane::Offset))
    {
        fires::fire("count.only"); // shapecheck fire signal (D3042)
        enables |= VERDICT_COUNT_ONLY;
    }
    if !site.zone.is_empty() || !site.zone_any.is_empty() || !wz.is_empty() || !izone.is_empty() || !lists.is_empty() {
        enables |= VERDICT_ZONE;
    }
    // sortpath: a zone AllMatch window answers every covered qual, which the host's condition cache learns (protos)
    if zone_covers && cond.is_some() {
        enables |= gravity_program::protos::VERDICT_ZONE_COVERS;
    }
    // code.sparse (H08 / P5-04 / SRC-22: any code verdict a host-built table answers whole), at any site whatever its zone
    // predicates, when a by-value lane is gathered beside its code lane (a code-only site is slower cold: D474)
    let gathers = |c: usize| (0..site.natts()).any(|l| l != c && site.lanes()[l].kind != lane::KIND_CODES && !site.filter_lanes.contains(l));
    if let Some((t, _)) = sparse_table.filter(|&(_, c)| gathers(c)) {
        enables |= code_sparse_table_arg(t);
        fires::fire("code.sparse"); // shapecheck fire signal (REGISTRY P5-04, SRC-22): the table form declared here
    }
    // P5-03 / H06 (the code verdict): the same table answers the part and the rows (`VERDICT_CODE_TABLE`: a part whose
    // dictionary holds no passing code is skipped, a row selection drops a row whose code the table rejects) at any site, a
    // count over the code lane alone too. A skip publishes no survivor, so under the condition cache only a covered op's
    // qual takes it (as the code-range skip)
    if let Some((t, _)) = verdict_table.filter(|_| cond.is_none() || cond_ops & (1 << verdict_op) != 0) {
        enables |= code_table_arg(t);
    }
    let ops = || wz.iter().map(|&(a, _)| word_zone_of(a).1).chain(izone.iter().map(|z| z.1));
    enables |= VERDICT_BLOOM * (VerdictKind::Bloom.on() && ops().any(|o| o == ZoneOp::Eq as u8)) as u64;
    enables |= VERDICT_PSMA * (VerdictKind::Psma.on() && ops().any(|o| o != ZoneOp::Ne as u8)) as u64;
    if cond.is_some() {
        enables |= VERDICT_COND;
    }
    if ReadForm::Claim.on() {
        enables |= VERDICT_CLAIM;
    }
    let late = if LaneForm::LateDecode.on() && !merged { crate::ops::scan::pgrc::pgrc_late::lanes(px, k, site) } else { 0 };
    if late != 0 {
        enables |= VERDICT_LATE;
    }
    // an answered window loads no lane: no code range reads its codes (the host resets its packed frames per window)
    let range = range.filter(|_| !answered && !merged);
    let packed = if merged { 0 } else { crate::ops::scan::pgrc::pgrc_packed::candidates(px, k, site, late) };
    if packed != 0 {
        enables |= VERDICT_PACKED;
    }
    // FRAME lever 1 a (VERDICT_PACKED_ZONE, the host's pack before row_select) is never set (D332): a portal resuming its
    // window off a 64-row boundary ran the non-packed body over zone lanes the host never decoded, and 1 a bought nothing
    // measurable (PSMA narrows most windows). The host's gate and packed_stage stay, unreachable from this source
    let first_only = packed & first_only | late & (first_quals | crate::dict::face::path_key(node).map_or(0, |f| (f.late)(px, k, site)) | crate::ops::scan::pgrc::strategies::jsonb_nums::late(px, k, site)); // C5 S4, P1b: guard fill only
    // P1c: no late lane the program fills: a selection's window keeps the guarded columns late (VERDICT_GUARD_SEL)
    if late != 0 && late & !packed & !first_only == 0 {
        enables |= VERDICT_GUARD_SEL;
    }
    // NEW-72: each bound static predicate's slot, by its final place on the site (the host binds `zone[at]`)
    // D3009: a disjunction's bound predicates after them (`zone.len() + i`: the host binds `zone_any[i]`)
    let bound: PgVec<'_, (u64, u16)> = site.zone.iter().chain(site.zone_any.iter()).enumerate().filter_map(|(at, z)| z.bound_of().filter(|_| at < u16::MAX as usize).map(|(w, len)| (gravity_program::protos::word_zone_at_arg(z.lane, z.op as u8, len, at as u16), w))).collect_in(mcx);
    let lists = lists.into_iter().map(|(_, a, l, w)| (a, l, w)).collect_in(mcx);
    Declared { covers, zone_covers, enables, cond, late, packed, first_only, answered, range, wzone: wz, izone, zargs: PgVec::new_in(mcx), bound, lists }
}

fn declare(px: &mut Px<'_, '_>, site: &mut SiteDesc, v: VerdictKind, q: ExId, node: i32) -> bool {
    v.on() && v.need::<VerdictImpl>(&node).0.declare(px, site, q)
}

/// The `src.verdicts` protocol of pipeline `k`'s drafted site (V4R-4; exec `Verdicts`): slot 0 the enables and the
/// condition cache's first run of bind words (`words` = `(base, n)`, `peer` its varlena mask, verdict_cond.rs `Run`),
/// slots 1 / 2 the static fingerprint, each further run in a slot-1 proto of its own (every slot-1 proto carries the
/// fingerprint's low word; the first carries the second run), slots `VERDICT_WORD_ZONE..` the word-valued zone
/// predicates (D289). No protocol when the static zone is all the site takes (the host's default).
fn declare_proto(px: &mut Px<'_, '_>, k: usize, d: &Declared<'_>) {
    if d.enables & !VERDICT_ZONE == 0 && d.wzone.is_empty() && d.zargs.is_empty() && d.bound.is_empty() && d.lists.is_empty() {
        return;
    }
    let base = Proto::new(ProtoKind::Verdicts, k as u16).site(DRAFT_SITE);
    let (runs, fp) = d.cond.as_ref().map_or((&[][..], [0, 0]), |(r, fp)| (&r[..], *fp));
    let (wb, wn, peer) = runs.first().copied().unwrap_or((0, 0, 0));
    px.declare_proto(base.slot(0).arg(d.enables).words(wb, wn).peer(peer));
    if d.cond.is_some() {
        let low = base.slot(1).arg(fp[0]);
        match runs.get(1) {
            Some(&(rb, rn, rpeer)) => px.declare_proto(low.words(rb, rn).peer(rpeer)),
            None => px.declare_proto(low),
        }
        px.declare_proto(base.slot(2).arg(fp[1]));
        for &(rb, rn, rpeer) in runs.iter().skip(2) {
            px.declare_proto(low.words(rb, rn).peer(rpeer));
        }
    }
    if d.late != 0 {
        px.declare_proto(base.slot(3).arg(d.late));
    }
    if d.packed != 0 {
        px.declare_proto(base.slot(4).arg(d.packed));
    }
    for (i, &(arg, w)) in d.wzone.iter().enumerate() {
        px.declare_proto(base.slot(VERDICT_WORD_ZONE + i as u8).arg(arg).words(w, 1));
    }
    // PS1-05: each open-time predicate's slot after them, its comparand a key cell (no words)
    for (i, &arg) in d.zargs.iter().enumerate() {
        px.declare_proto(base.slot(VERDICT_WORD_ZONE + (d.wzone.len() + i) as u8).arg(arg));
    }
    // NEW-72: the bound static predicates' slots, then the IN lists' (the lane in `peer`), while slot numbers last: a
    // bound predicate left without one leaves the host no value for it, so that site takes no zone verdict (never a guess)
    let mut at = d.wzone.len() + d.zargs.len() + VERDICT_WORD_ZONE as usize;
    for &(arg, w) in d.bound.iter().take(u8::MAX as usize - at.min(u8::MAX as usize)) {
        px.declare_proto(base.slot(at as u8).arg(arg).words(w, 1));
        at += 1;
    }
    for &(arg, lane, w) in d.lists.iter().take(u8::MAX as usize - at.min(u8::MAX as usize)) {
        px.declare_proto(base.slot(at as u8).arg(arg).words(w, 1).peer(lane));
        at += 1;
    }
}

// ------------------------------------------------------------------------------------------ the pgrc source

struct Pgrc<'g> {
    rd: Reader<'g>,
    /// Bit j: leading filter op j, elided in a window whose verdict covers it.
    covers: u64,
    /// The zone's `AllMatch` covers `covers`.
    zone_covers: bool,
    /// The condition cache takes the covered ops (`VERDICT_COND`): the seal adds its verdict lane.
    cond: bool,
    /// The condition cache's verdict lane (`LaneId::Verdict(0)`), placed by the seal after every table lane.
    cond_lane: Option<usize>,
    /// D3161: the current window's verdict-word fill (`next_window` loads it once; `passed` records with it per row).
    fill: core::cell::Cell<Option<crate::ops::scan::pgrc::verdict_cond::Fill>>,
    /// The site has dictionary code lanes (served as codes, `WinConds::codes`).
    codes: bool,
    claim: bool,
    /// The site's per-code tables, declared by the steps after this make and bound per window (step/percode_bind.rs).
    tabs: Rc<RefCell<crate::dict::percode::Tables>>,
    late: crate::ops::scan::pgrc::pgrc_late::Late,
    /// `DecodeOrder::Late`'s `first` (0: eager).
    first: u64,
    /// A leading qual's code-range verdict: a window it proves `NoMatch` is handed on empty.
    range: Option<crate::dict::strategies::verdict_coderange::Range>,
    /// The packed body's candidates and their frame tables per window.
    packs: crate::ops::scan::pgrc::pgrc_packed::Packs<'g>,
    /// The fold's answer lanes are declared: a window's `ANSWERED` byte is its fact.
    answered: bool,
    /// V4R-19 (A): the group key's lane and width (verdict_zone.rs `const_key`).
    ck: Option<(usize, u8)>,
    /// The plan reads `ctid`: each row's `tid` cell from its position (`Pgrc::tid`).
    ctid: bool,
    /// TABLESAMPLE (`make_sample`): the argument's and the REPEATABLE value's words, stored into key cells 2 and 3 at
    /// each open (the claim's cells 0 and 1 are range_seek's); empty for a plain scan.
    sample: PgVec<'g, u16>,
    /// The leading ctid clauses' values (`tid_bounds`): the granule range their rows can sit in, stored into key cells 2
    /// and 3 at each open; empty without one (never beside `sample`).
    tids: PgVec<'g, (u16, TidSide)>,
    /// PS1-05: the open-time zone predicates' `(paramid, InitPlan handle word, key cell, source)` (make_with), stored at
    /// each open; D3008: a rescan parameter's handle is `None` (its word is read as its producer wrote it), D2992's STABLE
    /// comparand's too (`stable_key`).
    izone: PgVec<'g, (i32, Option<u16>, u16, OpenParam)>,
}

/// Which side of a ctid clause its value bounds (`ctid = v` both; `ctid > v` / `>=` the lower; `<` / `<=` the upper;
/// `ctid = ANY (a)` both, from the least to the most of `a`'s elements).
#[derive(Clone, Copy, PartialEq, Eq)]
enum TidSide {
    Point,
    Lower,
    Upper,
    Array,
}

/// The leading filters' ctid clauses over a constant or an extern parameter (the TID clauses of a TidScan / TidRangeScan
/// the decomposer mapped onto this range scan, or a seq scan's): `(word, side)` each. The host narrows its walk to the
/// granules those rows can sit in (hosts granule_source.rs `seek`: key cells 2 and 3 of a range site with four key cells
/// and no sampler); the filters still decide every row, so the range only has to hold them (a value's whole ctid block).
/// A computed value is never read here (a word evaluated at bind could raise where PostgreSQL never evaluates it).
fn tid_bounds<'g>(px: &mut Px<'g, '_>, k: usize) -> PgVec<'g, (u16, TidSide)> {
    let gx = px.gx;
    let mcx = px.mcx();
    let p = &gx.pipelines()[k];
    let Source::Range { scanrelid, .. } = &p.source else { return PgVec::new_in(mcx) };
    let (varno, node) = (*scanrelid as i32, p.node_id);
    let is_ctid = |n: Node<'_>| n.as_var().is_some_and(|v| v.varno == varno && v.varlevelsup == 0 && v.varattno == -1);
    let plain = |n: Node<'_>| n.as_const().is_some() || n.as_param().is_some_and(|q| q.paramkind == types_nodes::primnodes::ParamKind::PARAM_EXTERN);
    let mut out = PgVec::new_in(mcx);
    for op in p.ops.iter().take_while(|op| matches!(op, Op::Filter(_))) {
        let Op::Filter(n) = op else { continue };
        for q in n.as_list().map_or_else(|| mcx::pg_vec![in mcx; *n], |l| l.iter().collect_in(mcx)) {
            // `ctid = ANY (a)` / `ctid IN (...)` (387 =, OR'd): an array literal's own word, which its compile never
            // places (step/consts.rs places its elements' words, step/expr_saop_set.rs a hashed one's set), so no claim
            // to take; an extern parameter's one word
            if let Some(a) = q.as_scalar_array_op_expr().filter(|a| a.useOr && a.opno == gravity_decompose::catalog::scan::TID_EQ && a.args.len() == 2 && is_ctid(a.args.nth(0))) {
                let value = a.args.nth(1);
                let place = match (value.as_const(), plain(value)) {
                    (Some(c), _) if !c.constisnull && !c.constbyval => {
                        let need = gravity_expr::emit::ParamNeed::Const(gravity_expr::NullableDatum::value(c.constvalue));
                        Some(px.plan.need_at(&need, c.consttype, false, value))
                    }
                    (Some(_), _) => None,
                    (None, true) => Some(crate::lifetime::bound::word(px, crate::lifetime::bound::Feed::Qual, k, value, node)),
                    (None, false) => None,
                };
                if let Some(gravity_lower::Place::Word(w)) = place {
                    out.push_in((w, TidSide::Array));
                }
                continue;
            }
            let Some(e) = q.as_op_expr().filter(|e| e.args.len() == 2) else { continue };
            let (left, right) = (e.args.nth(0), e.args.nth(1));
            let (value, flip) = match (is_ctid(left), is_ctid(right)) {
                (true, false) => (right, false),
                (false, true) => (left, true),
                _ => continue,
            };
            // 387 =, 2799 <, 2800 >, 2801 <=, 2802 >= (pg_operator.dat)
            let side = match (e.opno, flip) {
                (387, _) => TidSide::Point,
                (2799 | 2801, false) | (2800 | 2802, true) => TidSide::Upper,
                (2800 | 2802, false) | (2799 | 2801, true) => TidSide::Lower,
                _ => continue,
            };
            // a literal's word is its occurrence's, claimed ahead of the qual's compile (Px::claim_ref_const); an extern
            // parameter's is the parameter's one word
            let place = match (value.as_const(), plain(value)) {
                (Some(_), _) => px.claim_ref_const(value),
                (None, true) => Some(crate::lifetime::bound::word(px, crate::lifetime::bound::Feed::Qual, k, value, node)),
                (None, false) => None,
            };
            if let Some(gravity_lower::Place::Word(w)) = place {
                out.push_in((w, side));
            }
        }
    }
    out
}

/// The granules `[g0, g1)` the rows of the tid at `ptr + at` (an ItemPointerData) can sit in: its block's positions
/// `block x 291 ..= block x 291 + 290` (the positional reading), over 8192 a granule.
fn tid_granules(b: &mut FunctionBuilder<'_>, ptr: Value, at: i32) -> (Value, Value) {
    let (bh, bl) = (b.load_zext(Type::I16, ptr, at), b.load_zext(Type::I16, ptr, at + 2));
    let sixteen = b.iconst(Type::I64, 16);
    let bh = b.int_bin(IntBinOp::Shl, bh, sixteen);
    let blk = b.int_bin(IntBinOp::Or, bh, bl);
    let per = b.iconst(Type::I64, CTID_BLOCK_ROWS);
    let first = b.int_bin(IntBinOp::Mul, blk, per);
    let rows = b.iconst(Type::I64, GRANULE_ROWS);
    let g0 = b.int_bin(IntBinOp::Udiv, first, rows);
    let last = b.iconst(Type::I64, CTID_BLOCK_ROWS - 1);
    let last = b.int_bin(IntBinOp::Add, first, last);
    let g1 = b.int_bin(IntBinOp::Udiv, last, rows);
    let one = b.iconst(Type::I64, 1);
    (g0, b.int_bin(IntBinOp::Add, g1, one))
}

/// `[lo, hi)` narrowed by `ctid = ANY (a)`, `a` the tid[] at `ptr` (not read when `isnull`): from the least to the most of
/// its elements' granules. The array PostgreSQL 18.6 builds (array_in, array_recv, construct_md_array: a 4-byte header,
/// one dimension, no NULL bitmap; ArrayType's 16 bytes, then dims and lbounds, the 6-byte elements from offset 24, each
/// at alignment 's') is walked; another form (a NULL element, a short or compressed header, more dimensions) narrows
/// nothing, and an empty array (no dimension) matches no row. The walk checks for a cancel per element.
fn tid_array(b: &mut FunctionBuilder<'_>, (ctx, l_cancel): (Value, Block), ptr: Value, isnull: Value, (lo, hi): (Value, Value)) -> (Value, Value) {
    let i64s = [Type::I64, Type::I64];
    let (hdr, walk, head, body, fin, done) =
        (b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::I64, Type::I64, Type::I64]), b.create_block(&[]), b.create_block(&i64s), b.create_block(&i64s));
    let [zero, one, three, six, all] = [0, 1, 3, 6, -1].map(|v| b.iconst(Type::I64, v));
    let no = b.bconst(false);
    let tag = b.load_zext(Type::I8, ptr, 0);
    let tag = b.int_bin(IntBinOp::And, tag, three);
    let four_byte = b.icmp(IntCond::Eq, tag, zero);
    let read = b.select(isnull, no, four_byte);
    b.cond_br(read, (hdr, &[]), (done, &[lo, hi]));
    b.switch_to(hdr);
    let (ndim, nulls) = (b.load_zext(Type::I32, ptr, 4), b.load_zext(Type::I32, ptr, 8));
    let (one_dim, no_nulls) = (b.icmp(IntCond::Eq, ndim, one), b.icmp(IntCond::Eq, nulls, zero));
    let plain = b.int_bin(IntBinOp::And, one_dim, no_nulls);
    let empty = b.icmp(IntCond::Eq, ndim, zero);
    let hi0 = b.select(empty, zero, hi);
    b.cond_br(plain, (walk, &[]), (done, &[lo, hi0]));
    b.switch_to(walk);
    let n = b.load_zext(Type::I32, ptr, 16);
    b.br(head, &[zero, all, zero]);
    (b.switch_to(head), b.mark_loop_header(head));
    let [i, least, most] = [0, 1, 2].map(|j| b.block_params(head)[j]);
    let more = b.icmp(IntCond::Ult, i, n);
    b.cond_br(more, (body, &[]), (fin, &[least, most]));
    b.switch_to(body);
    crate::frame::cancel::check(b, ctx, l_cancel);
    let off = b.int_bin(IntBinOp::Mul, i, six);
    let e = b.ptr_add(ptr, off, 1, 0);
    let (g0, g1) = tid_granules(b, e, 24);
    let lt = b.icmp(IntCond::Ult, g0, least);
    let least = b.select(lt, g0, least);
    let gt = b.icmp(IntCond::Ugt, g1, most);
    let most = b.select(gt, g1, most);
    let next = b.int_bin(IntBinOp::Add, i, one);
    b.br(head, &[next, least, most]);
    b.switch_to(fin);
    let [least, most] = [0, 1].map(|j| b.block_params(fin)[j]);
    let up = b.icmp(IntCond::Ult, lo, least);
    let lo1 = b.select(up, least, lo);
    let down = b.icmp(IntCond::Ult, most, hi);
    let hi1 = b.select(down, most, hi);
    b.br(done, &[lo1, hi1]);
    b.switch_to(done);
    (b.block_params(done)[0], b.block_params(done)[1])
}

fn make<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    let none = PgVec::new_in(px.mcx());
    make_with(px, k, none)
}

/// TABLESAMPLE over pgrc (ops/scan/sample.rs, BERNOULLI / SYSTEM): this range scan with the sampler on its host (hosts
/// granule_source.rs `begin_sample` / `sample_mask`: a row is kept when the method picks its ctid, the AM's sample rows for
/// the same seed). `keys` are the argument's and the REPEATABLE value's words, read at each seek from key cells 2 and 3.
/// The decomposer's descriptor is serial (one seek per open: nodeSamplescan.c evaluates the arguments per rescan). No
/// verdict (declare_verdicts declares none off a range source), no entries window and no constant-key window: every
/// window is decoded and masked.
pub(crate) fn make_sample<'g>(px: &mut Px<'g, '_>, k: usize, keys: PgVec<'g, u16>) -> PgBox<'g, dyn SourceMech + 'g> {
    if keys.is_empty() || keys.len() > 2 {
        crate::refuse!(px.gx.pipelines()[k].node_id, Reason::Bug { payload: "a pgrc sample without its argument word" });
    }
    make_with(px, k, keys)
}

fn make_with<'g>(px: &mut Px<'g, '_>, k: usize, sample: PgVec<'g, u16>) -> PgBox<'g, dyn SourceMech + 'g> {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let node = p.node_id;
    let (site, relid) = match &p.source {
        Source::Range { site, am: Am::Pgrcolumnar, relid, .. } if sample.is_empty() => (site, relid),
        Source::Sample { site, am: Am::Pgrcolumnar, relid, .. } if !sample.is_empty() => (site, relid),
        _ => crate::refuse!(node, Reason::Bug { payload: "a pgrc entry over another source" }),
    };
    // the decomposer's trailing RowId lane is the plan's `ctid` read: computed from the row's position, never a lane (taken
    // off before the site is driven, as frame/site.rs index_site does)
    let ctid = site.lanes().last().is_some_and(|l| l.id == SiteLane::RowId);
    // pgrc's ctid is the row's stable ID, not `Pgrc::tid`'s position: a TABLESAMPLE (D4735: the AM samples the blocks of
    // its IDs) and a ctid read take the rows road (PGRCSERVE M3)
    if !sample.is_empty() {
        crate::admit::refusal::refuse_native(node, *relid, crate::admit::refusal::ShapeWhy("TABLESAMPLE over a pgrc relation (its ctid is the row's ID, which the rows road samples)"));
    }
    if ctid {
        crate::admit::refusal::refuse_native(node, *relid, crate::admit::refusal::ShapeWhy("a pgrc scan reading ctid (its ctid is the row's ID, which the rows road reads)"));
    }
    let bare = ctid.then(|| {
        let mut d = site.copy_in(px.arena);
        d.lanes.pop();
        d
    });
    let site = bare.as_ref().unwrap_or(site);
    let mut s = crate::ops::scan::pgrc::pgrc_late::fetch_narrow(px, k, driven(px, site, *relid, node));
    s.clear_zone();
    // L0-g (M6 D2): the path lanes the leading quals read, declared before they compile (a merged site reads documents)
    if !s.is_merged() {
        crate::face::source::site_draft(px, k, &mut s);
    }
    let tids = if sample.is_empty() { tid_bounds(px, k) } else { PgVec::new_in(px.mcx()) };
    // a TID scan's key cells: its granule range, then a leading `ctid = ANY (a)`'s array word (tid_keys)
    let extra = if tids.is_empty() { sample.len() } else if tids.iter().any(|t| t.1 == TidSide::Array) { 3 } else { 2 };
    if let gravity_program::SiteKind::RangeSource { nkeys, .. } = &mut s.kind {
        *nkeys = (*nkeys).max((gravity_program::RANGE_KEYS + extra) as u16);
    }
    // the leading quals compile over the drafted site, so their per-code tables are declared on it (step/percode.rs)
    px.set_site(k, s);
    for j in 0..p.ops.iter().take_while(|op| matches!(op, Op::Filter(_))).count() {
        crate::step::expr::quals(px, k, j);
    }
    // the Fp128 tables of the code lanes a keyed end hashes at Program scope (KEYED's rule), before the row cells' Reps
    // (a bpchar lane's is the trimmed renders' table, hashbpchar's image: D767 (2), so it meets a heap bpchar key's image)
    // join.payload_fp: and those of the code lanes a hash build of this scan stores the Fp128 of in its records, for a group
    // past its probe (frame::iface `build_fp_lanes`)
    let fpl = crate::table::emit_group::fp_lanes(gx, k);
    let built = crate::frame::iface::build_fp_lanes(gx, k);
    for l in fpl.iter().copied().chain(built.cols().iter().map(|c| c.0 as u8).filter(|l| !fpl.contains(l))) {
        let bpchar = match gx.pipelines()[k].source.relation().and_then(|(_, _, site)| site.lanes.get(l as usize)).map(|d| d.id) {
            Some(SiteLane::Attr(a)) => gx.facts.att(*relid, a).is_some_and(|f| crate::dict::coderep::sk_bpchar(gx, f.typid) != 0),
            _ => false,
        };
        // textkeys step 3 (road key.canon): the lane's canonical image table, so its codes image as a datum of their bytes does
        let decl = match crate::rep::varlena::key_canon::engaged(px.strategies) {
            true => {
                fires::fire("key.canon"); // textkeys witness signal: a code lane's canonical image table
                TableDecl::fingerprint_canon(l, bpchar)
            }
            false if bpchar => TableDecl::fingerprint_bpchar(l),
            false => TableDecl::fingerprint(l),
        };
        crate::dict::percode::declare_table(px, k, decl, crate::dict::percode::Class::Fp);
    }
    let arena = px.arena;
    let Some(mut s) = px.site_mut(k).map(|d| d.copy_in(arena)) else { crate::refuse!(node, Reason::Bug { payload: "a drafted pgrc site gone" }) };
    let mut d = declare_verdicts(px, k, &mut s, !tids.is_empty());
    // PS1-05 (road nest.eagersafe): each open-time zone predicate's key cell after every other cell (X5's bound cells
    // included), counted in `zkeys` apart from the site's own, and the InitPlan handle word its open reads it through
    let mut izone = PgVec::new_in(px.mcx());
    // (D3008: a rescan parameter's needs no InitPlan handle)
    let init_load = crate::ops::kind_as::<crate::rerun::params::InitLoad>(crate::face::kinds::KindId::NestInitplan).is_some();
    d.izone.retain(|z| !matches!(z.4, OpenParam::Init(_)) || init_load);
    if !d.izone.is_empty() {
        if let gravity_program::SiteKind::RangeSource { nkeys, zkeys, .. } = &mut s.kind {
            for &(lane, op, len, id, from) in d.izone.iter() {
                let cell = *nkeys;
                *nkeys += 1;
                *zkeys += 1;
                let h = matches!(from, OpenParam::Init(_)).then(|| crate::rerun::params::init_handle(px, id));
                if matches!(from, OpenParam::Stable { .. }) {
                    fires::fire("verdict.zone.stable"); // shapecheck fire signal (D2992)
                }
                izone.push_in((id, h, cell, from));
                d.zargs.push_in(gravity_program::protos::word_zone_key_arg(lane, op, len, cell));
            }
        }
    }
    let ck = if sample.is_empty() && !s.is_merged() { crate::ops::scan::pgrc::verdict_zone::const_key(px, k, &s) } else { None };
    d.enables |= ck.map_or(0, |(l, _)| const_key_arg(l as u16));
    if DictPolicy::Open.on() {
        crate::ops::scan::pgrc::dict_open::open(px, k, &mut s);
    }
    if DictPolicy::Entries.on() && sample.is_empty() && !s.is_merged() {
        crate::ops::scan::pgrc::dict_entries::entries(px, k, &mut s);
    }
    let rd = Reader::new(px, &s, *relid, node, None, ctid.then_some(CTID_AT));
    let codes = s.has_codes();
    let first = crate::ops::scan::pgrc::pgrc_late::first(&s, d.late | d.packed);
    let packs = crate::ops::scan::pgrc::pgrc_packed::Packs::new(&s, d.packed, px.mcx());
    px.set_site(k, s);
    let tabs = crate::dict::percode::tables(px, k);
    declare_proto(px, k, &d);
    let claim = d.enables & VERDICT_CLAIM != 0;
    // the fill set: the late lanes (a path-only jsonb column's the host fills, M6 D2) and the packed candidates (read undecoded only by the packed body's first step)
    let late = crate::ops::scan::pgrc::pgrc_late::Late::new((d.late | d.packed) & !(d.first_only & d.late & !d.packed), (d.late | d.packed) & !d.first_only);
    mcx::pg_box_dyn!(px.mcx(), Pgrc { rd, covers: d.covers, zone_covers: d.zone_covers, cond: d.cond.is_some(), cond_lane: None, fill: core::cell::Cell::new(None), codes, claim, tabs, late, first, packs, answered: d.answered, range: d.range.map(|r| r.1), ck, ctid, sample, tids, izone } => dyn SourceMech + 'g)
}

/// The decomposer's site; a scan reading no column is driven by a row-count lane: any live column, since a granule's rows
/// are its part's (a NULL is a row), the cheapest decode first (by-value NOT NULL, by-value, fixed length, varlena; D320),
/// the narrowest within a kind (D3042: a count(*) the stats answer declines read an 8-byte first column whole where a
/// 2-byte column reads a quarter of it; a count-only window reads none: granule_source `count_only`).
fn driven(px: &Px<'_, '_>, site: &SiteDesc, relid: u32, node: i32) -> SiteDesc {
    if site.lanes().iter().any(|l| matches!(l.id, SiteLane::Attr(_))) {
        return site.copy_in(px.arena);
    }
    let natts = px.gx.facts.rel(relid).map_or(0, |r| r.natts);
    let rank = |f: gravity_decompose::AttFact| if f.byval { !f.notnull as u8 } else if f.typlen > 0 { 2 } else { 3 };
    let width = |f: gravity_decompose::AttFact| if f.typlen > 0 { f.typlen as i32 } else { i32::MAX };
    let a = (1..=natts).filter_map(|a| px.gx.facts.att(relid, a).map(|f| (rank(f), width(f), a))).min().map(|(_, _, a)| a);
    let Some(a) = a else { shape(node, "a pgrc scan reading no column over a relation with no live column") };
    SiteDesc::range_source(px.arena, relid, &[a])
}

/// pgrc_format geom `GRANULE_ROWS` (a granule's row positions) and rowid `CTID_BLOCK_ROWS` (PostgreSQL's
/// `MaxHeapTuplesPerPage`): the positional ctid numbering (pgrc_am scan.rs `store_row`), which `make_with` refuses to read
/// while a pgrc ctid is the row's stable ID.
const GRANULE_ROWS: i64 = 8192;

/// D3224 A2: the most rows a window of this source holds (one granule; a fetch window's validity words cover as many):
/// a code front record's sums are bounded by it (`num_kinds` sends a larger window to the generic step).
pub(crate) const WINDOW_ROWS: i64 = gravity_program::cost::PGRC_GRANULE_ROWS as i64;
const CTID_BLOCK_ROWS: i64 = 291;

impl<'g> Pgrc<'g> {
    /// TABLESAMPLE's words into key cells `RANGE_KEYS ..` (frame/site.rs `store_keys`' statement and executor-parameter
    /// arms; make_sample admits no other), before range_seek writes the claim into cells 0 and 1.
    fn sample_keys(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        use crate::frame::site::EXEC_KEY;
        let s = site_off(self.rd.k(env)) as i32;
        let keys = b.load(Type::Ptr, env.ctx, s + site::KEYS as i32);
        let keys_null = b.load(Type::Ptr, env.ctx, s + site::KEYS_NULL as i32);
        for (n, &w) in self.sample.iter().enumerate() {
            let (v, isnull) = if w & EXEC_KEY == 0 {
                (b.load(Type::I64, env.params, w as i32 * 8), b.load(Type::I8, env.params_null, w as i32))
            } else {
                let (v, null) = crate::rerun::params::load(b, env, env.node, (w & !EXEC_KEY) as i32);
                (v, b.conv(ConvOp::Zext, Type::I8, null))
            };
            let at = (gravity_program::RANGE_KEYS + n) as i32;
            b.store(keys, at * 8, v);
            b.store(keys_null, at, isnull);
        }
    }

    /// The leading ctid clauses' granule range into key cells 2 and 3 (`tid_bounds`): each value's ctid block `blk`
    /// spans positions `blk x 291 ..= blk x 291 + 290`, so granules `blk x 291 / 8192 ..= (blk x 291 + 290) / 8192`; a
    /// point bounds both ends, a lower bound the first, an upper bound the last. A NULL value matches no row: an empty range.
    fn tid_keys(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        let s = site_off(self.rd.k(env)) as i32;
        let keys = b.load(Type::Ptr, env.ctx, s + site::KEYS as i32);
        let keys_null = b.load(Type::Ptr, env.ctx, s + site::KEYS_NULL as i32);
        let (mut lo, mut hi) = (b.iconst(Type::I64, 0), b.iconst(Type::I64, -1));
        let zero = b.iconst(Type::I64, 0);
        let safe = b.conv(ConvOp::PtrToInt, Type::I64, env.ctx);
        // the first `ctid = ANY (a)`'s word and NULL flag go to key cell 4: the host decodes only its elements' granules
        if let Some(&(w, _)) = self.tids.iter().find(|t| t.1 == TidSide::Array) {
            let at = gravity_program::RANGE_KEYS as i32 + 2;
            let word = b.load(Type::I64, env.params, w as i32 * 8);
            let null = b.load(Type::I8, env.params_null, w as i32);
            b.store(keys, at * 8, word);
            b.store(keys_null, at, null);
        }
        for &(w, side) in self.tids.iter() {
            let word = b.load(Type::I64, env.params, w as i32 * 8);
            let null = b.load(Type::I8, env.params_null, w as i32);
            let z8 = b.iconst(Type::I8, 0);
            let isnull = b.icmp(IntCond::Ne, null, z8);
            // a NULL word's pointer is never read: the context stands in, its result dropped below
            let at = b.select(isnull, safe, word);
            let ptr = b.conv(ConvOp::IntToPtr, Type::Ptr, at);
            if side == TidSide::Array {
                (lo, hi) = tid_array(b, (env.ctx, env.l_cancel), ptr, isnull, (lo, hi));
                hi = b.select(isnull, zero, hi);
                continue;
            }
            let (g0, g1) = tid_granules(b, ptr, 0);
            if side != TidSide::Upper {
                let up = b.icmp(IntCond::Ult, lo, g0);
                lo = b.select(up, g0, lo);
            }
            if side != TidSide::Lower {
                let down = b.icmp(IntCond::Ult, g1, hi);
                hi = b.select(down, g1, hi);
            }
            hi = b.select(isnull, zero, hi);
        }
        let at = gravity_program::RANGE_KEYS as i32;
        let no = b.iconst(Type::I8, 0);
        b.store(keys, at * 8, lo);
        b.store(keys, (at + 1) * 8, hi);
        b.store(keys_null, at, no);
        b.store(keys_null, at + 1, no);
    }

    /// PS1-05 (road nest.eagersafe): each open-time zone predicate's eager-safe InitPlan value into its key cell, through
    /// nest.initplan's `InitLoad` (run here if this context has not: frame/site.rs `init_key`, an index site's InitPlan
    /// key's read), before the seek reads it; a NULL result leaves the cell NULL (the host adds no predicate).
    /// D3126: a guarded STABLE predicate's cell is NULL too when an earlier STABLE comparand raised at this open.
    fn izone_keys(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        let mut raised = b.bconst(false);
        for &(id, h, cell, from) in self.izone.iter() {
            let (v, isnull) = match (h, from) {
                (Some(h), _) => crate::frame::site::init_key(b, env, id, h),
                (None, OpenParam::Stable { q, node, cross, op, guarded, .. }) => {
                    let (v, none, r) = stable_key(b, env, q, node, cross, op);
                    let none = if guarded { b.int_bin(IntBinOp::Or, none, raised) } else { none };
                    raised = b.int_bin(IntBinOp::Or, raised, r);
                    (v, b.conv(ConvOp::Zext, Type::I8, none))
                }
                (None, _) => open_param(b, env, id),
            };
            let s = site_off(self.rd.k(env)) as i32;
            let keys = b.load(Type::Ptr, env.ctx, s + site::KEYS as i32);
            let keys_null = b.load(Type::Ptr, env.ctx, s + site::KEYS_NULL as i32);
            b.store(keys, cell as i32 * 8, v);
            b.store(keys_null, cell as i32, isnull);
        }
    }

    /// Row `i`'s ctid `(block, offset)` (I32, I16): position `r = POS_BLOCK x 8192 + i` (the window is one granule, `i`
    /// granule-relative), `(r / 291, r % 291 + 1)`: a position, not the row's ID.
    fn tid(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, i: Value) -> (Value, Value) {
        let s = site_off(self.rd.k(env)) as i32;
        let g = b.load_zext(Type::I32, env.ctx, s + site::POS_BLOCK as i32);
        let rows = b.iconst(Type::I64, GRANULE_ROWS);
        let base = b.int_bin(IntBinOp::Mul, g, rows);
        let r = b.int_bin(IntBinOp::Add, base, i);
        let per = b.iconst(Type::I64, CTID_BLOCK_ROWS);
        let block = b.int_bin(IntBinOp::Udiv, r, per);
        let rem = b.int_bin(IntBinOp::Urem, r, per);
        let one = b.iconst(Type::I64, 1);
        let off = b.int_bin(IntBinOp::Add, rem, one);
        (b.conv(ConvOp::Trunc, Type::I32, block), b.conv(ConvOp::Trunc, Type::I16, off))
    }

    /// The window's cursor, or its end where the code range proves the window `NoMatch` (read without a selection and with
    /// its code lane decoded only): the frame walks no row and writes the window back consumed, so the host moves on.
    fn range_cursor(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, r: crate::dict::strategies::verdict_coderange::Range, cursor: Value, end: Value) -> Value {
        let w = self.rd.vals();
        let lanes = self.rd.cols.iter().zip(w.lanes.iter());
        let Some(ptr) = lanes.filter(|(c, _)| c.attno == r.attno && c.desc.kind == lane::KIND_CODES).map(|(_, l)| l.0).next() else { return cursor };
        let (l_scan, l_done) = (b.create_block(&[]), b.create_block(&[Type::I64]));
        // an undecoded code lane (decode.unread's hit, an elided lane) has no codes to scan
        let pi = b.conv(ConvOp::PtrToInt, Type::I64, ptr);
        let z = b.iconst(Type::I64, 0);
        let has = b.icmp(IntCond::Ne, pi, z);
        let go = b.int_bin(IntBinOp::And, w.sel_null, has);
        b.cond_br(go, (l_scan, &[]), (l_done, &[cursor]));
        b.switch_to(l_scan);
        let none = crate::dict::strategies::verdict_coderange::none(b, env, r, ptr, cursor, end);
        let c = b.select(none, end, cursor);
        b.br(l_done, &[c]);
        b.switch_to(l_done);
        b.block_params(l_done)[0]
    }
}

impl<'g> SourceMech for Pgrc<'g> {
    fn declare(&mut self, pb: &mut ProgramBuilder) {
        self.rd.declare(pb);
        self.late.declare(pb);
    }

    /// The decomposer's descriptor, run on the leader in block order when a pool thread cannot run one of its hosted
    /// calls, or when it counts a LIMIT / OFFSET: at the sink (v2 compose.rs, M1-plan §7.3) or under a breaker (D281).
    fn pipeline_desc(&self, px: &Px<'_, '_>, k: usize) -> Pipeline {
        let p = &px.gx.pipelines()[k];
        let d = p.desc;
        let sink = crate::frame::iface::ends_in_sink(px.gx, k);
        let limit = crate::frame::iface::holds_limit(&p.ops);
        if sink && limit {
            // a sink keeps no owner, words, finish or admit; consumes_from is dropped too (a probe-with-LIMIT would need it)
            return Pipeline { consumes: d.consumes, ..Pipeline::serial(d.driving_site, gravity_program::End::Sink, Why::Sink) };
        }
        if limit {
            // D281: step.limit's counters are per-open words, and an eligible pipeline opens per claim (the width-1 arm
            // too) and per worker, so each would keep `count` rows past its own `offset`; PostgreSQL's Limit sits above the
            // Gather. One open over the whole relation in claim order, PostgreSQL's serial row choice too (a sum over the
            // LIMIT reads it); the breaker's descriptor is otherwise the decomposer's.
            return Pipeline { parallel: Parallel::Ineligible { site: d.driving_site, reason: Why::Sink }, ..d };
        }
        match crate::hir::hosted_serial(px, k) {
            Some(reason) => Pipeline { parallel: Parallel::Ineligible { site: d.driving_site, reason }, ..d },
            None => d,
        }
    }

    /// The condition cache's verdict lane goes last: the steps' tables must follow the attribute and offset lanes
    /// directly (`SiteDesc::declare_table`), and they are declared after this source's make.
    fn seal(&mut self, _px: &mut Px<'_, '_>, _k: usize, site: &mut SiteDesc) {
        if self.cond {
            self.cond_lane = Some(site.lanes().len());
            site.add_lane(LaneDesc { id: SiteLane::Verdict(0), ..LaneDesc::rowid() }, false);
        }
    }

    fn decode_order(&self) -> DecodeOrder {
        if self.late.mask != 0 { DecodeOrder::Late { first: self.first } } else { DecodeOrder::Eager }
    }

    /// A row body's fill (`at` none) skips an empty window (a code-range `NoMatch`). A whole block's survivors (`at`'s
    /// base 64 rows or more before the end; a tail's fewer) fill no candidate the first step alone reads: it tested them
    /// packed, and nothing after reads them.
    fn fill_late(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, win: &Win, at: Option<(Value, Mask)>) {
        let s = self.rd.k(env);
        if let (Some((base, _)), true) = (at, self.late.block != self.late.mask) {
            let (l_blk, l_tail, l_end) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
            let sixty_four = b.iconst(Type::I64, 64);
            let e = b.int_bin(IntBinOp::Add, base, sixty_four);
            let whole = b.icmp(IntCond::Sle, e, win.end);
            b.cond_br(whole, (l_blk, &[]), (l_tail, &[]));
            b.switch_to(l_blk);
            self.late.fill(b, env, s, at, self.late.block);
            b.br(l_end, &[]);
            b.switch_to(l_tail);
            self.late.fill(b, env, s, at, self.late.mask);
            b.br(l_end, &[]);
            b.switch_to(l_end);
            return;
        }
        if at.is_some() || self.late.mask == 0 {
            return self.late.fill(b, env, s, at, self.late.mask);
        }
        let (l_fill, l_end) = (b.create_block(&[]), b.create_block(&[]));
        let rows = b.icmp(IntCond::Ult, win.cursor, win.end);
        b.cond_br(rows, (l_fill, &[]), (l_end, &[]));
        b.switch_to(l_fill);
        self.late.fill(b, env, s, at, self.late.mask);
        b.br(l_end, &[]);
        b.switch_to(l_end);
    }

    /// D3194: the late lanes of columns `attnos` for block `at`'s rows (a filtered probe's keys, frame.rs `block_loop`); a
    /// column the site does not carry is not readable.
    fn fill_late_cols(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _win: &Win, at: (Value, Mask), attnos: &[i16]) -> bool {
        let lane = |a: i16| self.rd.cols.iter().find(|c| c.attno == a).map(|c| if c.k < 64 { 1u64 << c.k } else { 0 });
        let Some(bits) = attnos.iter().try_fold(0u64, |m, &a| lane(a).map(|l| m | l)) else { return false };
        let s = self.rd.k(env);
        self.late.fill(b, env, s, Some(at), bits & self.late.mask);
        true
    }

    fn read_plan(&self) -> ReadPlan {
        if self.claim { ReadPlan::PLAIN.with(ReadForm::Claim) } else { ReadPlan::PLAIN }
    }

    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        if !self.sample.is_empty() {
            self.sample_keys(b, env);
        }
        if !self.tids.is_empty() {
            self.tid_keys(b, env);
        }
        if !self.izone.is_empty() {
            self.izone_keys(b, env);
        }
        // NEW-72 entry C: the bound per-code tables' values, which the seek's first fetch may read (`bound_keys`)
        let sk = self.rd.k(env);
        crate::dict::percode_bind::bound_keys(b, env, sk, &self.tabs);
        self.rd.open(b, env)
    }

    /// The window and its facts: no nulls in the nullable lanes, no selection, code lanes as codes (their `CodeRef`s in
    /// `env.codes`), and covered when the condition cache hit or the zone verdict is `AllMatch` over every covered op.
    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        let (cursor, end) = self.rd.window(b, env, l_eof);
        let sk = self.rd.k(env);
        let l_exit = env.x.l_exit;
        crate::dict::percode_bind::bind(b, env, sk, &self.tabs, l_exit);
        let w = self.rd.vals();
        let cursor = self.range.map_or(cursor, |r| self.range_cursor(b, env, r, cursor, end));
        env.codes.clear();
        // the render face's `part << 32 | code` word
        let part = self.codes.then(|| {
            let p = b.load_zext(Type::I32, env.ctx, site_off(sk) as i32 + site::PART_ID as i32);
            let sh = b.iconst(Type::I64, 32);
            b.int_bin(IntBinOp::Shl, p, sh)
        });
        for (c, &(ptr, valid, vnull)) in self.rd.cols.iter().zip(w.lanes.iter()).filter(|(c, _)| c.desc.kind == lane::KIND_CODES) {
            let lr = LaneRef { ptr, valid, valid_null: Some(vnull), width: 4, kind: lane::KIND_CODES, all_valid: c.notnull };
            env.codes.push_in(gravity_expr::emit::CodeRef { attno: c.attno, lane: lr, code: None, ordinal: c.k as u8, part });
        }
        let zone = self.zone_covers.then(|| crate::ops::scan::pgrc::verdict_allmatch::covered(b, w.lane_arr, self.rd.cols[0].k));
        let cond = self.cond_lane.map(|k| {
            let (hit, fill) = crate::ops::scan::pgrc::verdict_cond::window(b, w.lane_arr, k);
            self.fill.set(Some(fill));
            hit
        });
        let covered = match (zone, cond) {
            (Some(z), Some(c)) => Some(b.int_bin(IntBinOp::Or, z, c)),
            (z, c) => z.or(c),
        };
        let codes = self.codes.then(|| b.bconst(true));
        let ptr_of = |l: usize| self.rd.cols.iter().zip(w.lanes.iter()).find(|(c, _)| c.k == l).map_or(env.zero, |(_, v)| v.0);
        let ptrs: PgVec<'_, (usize, Value)> = self.rd.cols.iter().map(|c| (c.k, ptr_of(c.k))).collect_in(env.mcx);
        let lane_ptr = |l: usize| ptrs.iter().find(|p| p.0 == l).map_or(ptrs[0].1, |p| p.1);
        let packed = self.packs.window(b, env, cursor, &lane_ptr, &|b, env, k| self.late.call(b, env, sk, k));
        let answered = self.answered.then(|| b.load_zext(Type::I8, env.ctx, site_off(sk) as i32 + site::ANSWERED as i32));
        // the fast body of a window whose per-code tables bound clean reads no fault table (percode_len.rs `table_ref`)
        let clean = (!self.tabs.borrow().lanes.is_empty()).then(|| crate::dict::percode_bind::clean(b, env, &self.tabs));
        let const_key = self.ck.map(|(l, width)| crate::ops::scan::pgrc::verdict_zone::const_key_window(b, env, w.lane_arr, l, width));
        let facts = WinConds { nonulls: self.rd.nonulls(b), nosel: Some(w.sel_null), codes, covered, packed, answered, clean, const_key, ..WinConds::default() };
        Win { cursor, end, facts }
    }

    /// A1 (D3224): each packed numeric lane's kind byte tested once per window (a verbatim part's lane holds image
    /// pointers): the rows' folds read the Bool instead of loading the byte per row per aggregate.
    fn num_kinds(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Vec<(usize, u16, Value)> {
        let num = |c: &&crate::frame::site::Col| matches!(c.desc.rep, gravity_program::Rep::PackedNum { .. } | gravity_program::Rep::PackedNumDyn);
        if !self.rd.cols.iter().any(|c| num(&c)) {
            return Vec::new();
        }
        fires::fire("num.kinds.window"); // fire signal (D3224 A1: the kind tests made once per window)
        let (sk, w) = (self.rd.k(env), self.rd.vals());
        let byref = b.iconst(Type::I64, lane::KIND_BYREF as i64);
        // D3224 A2: a window past WINDOW_ROWS (none is) takes the generic step, so a lean fold may bound its sums by it
        let big = env.win.as_ref().map(|w| (w.cursor, w.end)).map(|(c, e)| {
            let (rows, cap) = (b.int_bin(IntBinOp::Sub, e, c), b.iconst(Type::I64, WINDOW_ROWS));
            b.icmp(IntCond::Sgt, rows, cap)
        });
        self.rd.cols.iter().filter(num).map(|c| {
            let kind = b.load_zext(Type::I8, w.lane_arr, (c.k * lane::SIZE + lane::KIND) as i32);
            let verbatim = b.icmp(IntCond::Eq, kind, byref);
            (sk, c.k as u16, big.map_or(verbatim, |g| b.int_bin(IntBinOp::Or, verbatim, g)))
        }).collect() // alloc: Program
    }

    fn packed(&self, b: &mut FunctionBuilder<'_>, _env: &mut Env, _win: &Win, attno: i16, base: Value) -> Option<Packed> {
        self.packs.packed(b, attno, base)
    }

    fn word_win<'e>(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win) -> Option<WordWin<'e>> {
        self.rd.word_win(env.mcx)
    }

    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        let mut row = self.rd.row(b, env, j, carry, next);
        if self.ctid {
            let i = self.rd.row_index(b, env, j);
            let (block, off) = self.tid(b, env, i);
            let img = crate::ops::scan::heap::tid_image(b, block, off);
            crate::ops::scan::heap::push_ctid(&mut row, img);
        }
        row
    }

    /// H28 (road topk.fetch, V4R-169): the row's `RowId` (pgrc_format rowid: part 32 | granule 19 | row 13 bits), its
    /// `part << 32 | granule << 13` and the granule's row.
    fn locator(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals) -> Option<Locator> {
        let s = site_off(self.rd.k(env)) as i32;
        let (part, granule) = (b.load_zext(Type::I32, env.ctx, s + site::PART_ID as i32), b.load_zext(Type::I32, env.ctx, s + site::GRANULE as i32));
        let (s32, s13) = (b.iconst(Type::I64, 32), b.iconst(Type::I64, 13));
        let (hi, g) = (b.int_bin(IntBinOp::Shl, part, s32), b.int_bin(IntBinOp::Shl, granule, s13));
        Some(Locator { block: b.int_bin(IntBinOp::Or, hi, g), offset: self.rd.row_index(b, env, row.at), pinned: None, rel: None })
    }

    fn covers(&self) -> u64 {
        self.covers
    }

    /// A condition-cache miss: the row's bit in the window's verdict words.
    fn passed(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _win: &Win, row: &RowVals) {
        if let Some(fill) = self.fill.get().filter(|_| self.cond_lane.is_some()) {
            let i = self.rd.row_index(b, env, row.at);
            crate::ops::scan::pgrc::verdict_cond::record(b, fill, i);
        }
    }

    fn fills(&self) -> bool {
        self.cond_lane.is_some()
    }

    fn fills_blocks(&self) -> bool {
        self.cond_lane.is_some()
    }

    fn passed_block(&self, b: &mut FunctionBuilder<'_>, _env: &mut Env, _win: &Win, base: Value, m: Mask) {
        if let Some(fill) = self.fill.get().filter(|_| self.cond_lane.is_some()) {
            crate::ops::scan::pgrc::verdict_cond::record_block(b, fill, base, m.0);
        }
    }
}

/// D2992: a STABLE comparand's key cell at the open (verdict_zone.rs `stable_of`): subtree `node` of qual `q`'s program
/// over the qual's words (as frame/hoist.rs `emit_late` computes an operand), converted into the lane's units by kernels
/// `zone_cross_bound` for a cross-unit kind `cross`: `(value, none, raised)`. None (no predicate) when the value is NULL,
/// raised (the row filter raises it where PostgreSQL does, never on an empty input), not emitted here, or when no bound
/// holds; `raised` (D3126) when the comparand raised or was not emitted here.
fn stable_key(b: &mut FunctionBuilder<'_>, env: &mut Env, q: ExId, node: gravity_expr::NodeId, cross: u8, op: u8) -> (Value, Value, Value) {
    use gravity_expr::emit::{ColsRef, RowRefs};
    let (words, nulls) = crate::frame::hoist::words(b, env, env.node, q);
    let x = env.placed(q);
    let refs = RowRefs::scan(ColsRef::Lanes { row: env.zero, lanes: &[] }, &words, &nulls, env.mcx);
    let refs = env.hir.with_ex(b, x, refs);
    let t = b.bconst(true);
    let Ok(l) = x.c.emit_at(b, &refs, node) else {
        return (env.zero, t, t);
    };
    let raised = b.icmp(IntCond::Ne, l.err, env.zero);
    let null = b.int_bin(IntBinOp::Xor, l.valid, t);
    let mut none = b.int_bin(IntBinOp::Or, null, raised);
    let mut v = l.value;
    if cross != 0 {
        use gravity_kernels::ops::scan::entries::{zone_cross_bound, CROSS_NONE};
        let h = b.declare_host_once("zone_cross_bound", &[Type::I64, Type::I64, Type::I64], Some(Type::I64), zone_cross_bound as *const () as usize, gravity_ir::HostFlags::default());
        let (kind, op) = (b.iconst(Type::I64, cross as i64), b.iconst(Type::I64, op as i64));
        v = b.call_host(h, &[kind, op, l.value], Some(Type::I64)).expect("zone_cross_bound returns a word");
        let unbound = b.iconst(Type::I64, CROSS_NONE as i64);
        let unbound = b.icmp(IntCond::Eq, v, unbound);
        none = b.int_bin(IntBinOp::Or, none, unbound);
    }
    (v, none, raised)
}

/// D3008: a rescan parameter's key cell at the open: its word as the producer wrote it (`rerun::params::load`), NULL when
/// the flags say NULL or not set in this context (no predicate then: the host's `KeyZone::pred` reads a NULL cell as none).
fn open_param(b: &mut FunctionBuilder<'_>, env: &mut Env, id: i32) -> (Value, Value) {
    let (v, null) = crate::rerun::params::load(b, env, env.node, id);
    let set = crate::rerun::params::is_set(b, env, env.node, id);
    let t = b.bconst(true);
    let unset = b.int_bin(IntBinOp::Xor, set, t);
    let none = b.int_bin(IntBinOp::Or, null, unset);
    (v, b.conv(ConvOp::Zext, Type::I8, none))
}

/// P5-03: whether code table `t` can be the code verdict's: one that rejects a part whose dictionary lacks a passing code.
/// A set, an interval or a pattern rejects any part without a match. A negated table (`<>`, NOT IN, NOT LIKE) passes every
/// code but those it names, so it rejects only a part or granule every row of which holds one of them, which for `<> c`
/// the zone records decide (a code zone of `c` alone). It is no code verdict's: its part check opens each part's
/// dictionary at the part's pin, ahead of the claim run, and a cold scan under it waits longer with no part or granule
/// more skipped (D4946).
fn rejects_parts(site: &SiteDesc, (t, _): (u16, usize)) -> bool {
    site.tables.get(t as usize).is_some_and(|d| d.flags & gravity_program::table_fn::FLAG_NEGATED == 0)
}
