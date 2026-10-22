//! Build -> scan key-set / range filter, armed by measurement: road `join.rtfilter` (H07c, R5-L04). Budget 160 lines.
//! Owner M1-JOIN.
//!
//! A probe whose key (or, AGA-08, the first part of a key of several: a row matches only if that part is among the
//! build's) is a column of its scan and whose image orders as the column (an integer, date or timestamp key) declares
//! `ProtoKind::RtFilter` on the probing pipeline's site: after the build finishes, the runner reads the build
//! region through `export` and arms the site's verdict with it (the host measures whether it cuts granules before it
//! skips any). The items (`RegionFn`'s emit): `(0, lo, hi)` the build's key range, then `(1, key, 0)` per record key when
//! the build holds at most the cost home's `rtfilter_set_max` records for the probe (a key set; duplicates repeat). A row the verdict keeps still probes: the
//! filter only skips granules no build key can match, so it never changes the result. A swapped pair (join.buildside)
//! filters the scan the swap made the probe (`declare_swapped`). PS1-22: a text key (`text_fp_key`) has
//! no range: `(2, fp lo word, fp hi word)` per record key of a build within the cap (`F_FP128`), the host's part skip.
use gravity_abi::breaker::KeyKind;
use gravity_decompose::join::{JoinDesc, JoinKey, JoinKeyDecl};
use gravity_decompose::Op;
use gravity_program::protos::{Proto, ProtoKind};
use mcx::{CollectIn, PgVec};
use types_nodes::Node;

use crate::effects::ops_raise_free;
use crate::graph::Gx;
use crate::props::code::scan_attno;
use crate::frame::px::{Px, DRAFT_SITE};

use crate::ops::hashjoin::build::{DBITS, DIR, FAILED, FLAGS, FORM, F_FP128, F_RANGE, F_RANGE0, HI, KIND, LO, PHASE};
use crate::ops::hashjoin::rt::{rd, Rt};
use crate::ops::hashjoin::build_side::Shape;

mod raw;
use raw::export;

/// The road's value: `declare` and `export` are the filter.
pub(crate) struct RtFilter;

crate::strategies! {
    /// Build -> scan key-set / range / bloom filter, armed by measurement.
    JOIN_RTFILTER = 52, "join.rtfilter", Join;
    /// D3245: a scan site arms each of its probes' filters (up to the host's `RT_SLOTS`), a granule skipped when any one
    /// rules it out, and a build's keys past the set cap fold into runs of consecutive keys. Off (`ARG_ONE`): the site's
    /// first filter alone, its keys a set within the cap, else its range.
    JOIN_RTFILTER_MULTI = 130, "join.rtfilter.multi", JoinChain;
    /// D4569: the filter sits on the scan of every arm of a streamed UNION ALL the probe was copied into: the arm's marker
    /// with no pruning only hands its rows on (`arms`), and each later arm's copy takes its own (`arm_copies`). On by
    /// default (not in road_off.rs `DEFAULT_OFF`); off under a plain `join.rtfilter.arms`, no union arm takes one.
    JOIN_RTFILTER_ARMS = 142, "join.rtfilter.arms", Append;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(JOIN_RTFILTER, &RtFilter), (JOIN_RTFILTER_MULTI, &RtFilter), (JOIN_RTFILTER_ARMS, &RtFilter)], ..crate::ops::NONE };

/// The filter of build `sh` on its probe's scan site, when the road is on and the probe key's first part is a scan column
/// that orders as its image (the build's range over it: build.rs `F_RANGE`, or `F_RANGE0` for a key of several parts).
/// `arg` = the key's column in the scan (its attno) and the key-set cap (`ARG_SET_AT`). D4569 (road join.rtfilter.arms):
/// and on each union arm's copy of that probe (`arm_copies`), each under the same guards over its own pipeline.
pub(crate) fn declare(px: &mut Px<'_, '_>, sh: &Shape, decls: &[JoinKeyDecl<'_>]) {
    let (Some(q), [d, ..]) = (sh.probe, decls) else { return };
    if !crate::face::source::on(crate::admit::refusal::Variant::Strategy(JOIN_RTFILTER)) {
        return;
    }
    if !matches!(d.key.kind, Some(KeyKind::I32 | KeyKind::I64 | KeyKind::Date | KeyKind::Timestamp)) && !(decls.len() == 1 && text_fp_key(&d.key) && text_fp_ok(px)) {
        return;
    }
    let Some(j) = px.gx.pipelines()[q].ops.iter().position(|op| matches!(op, Op::Join(JoinDesc::Probe { slot, .. }) if *slot == sh.slot)) else { return };
    declare_at(px, sh, q, j);
    if crate::face::consts::RtConst::BuildArms.on() {
        for &(c, jc) in arm_copies(px.gx, sh, q, j).iter() {
            declare_at(px, sh, c, jc);
        }
    }
}

/// D4569: the copies of probe op `j` of pipeline `q` that a streamed union made in its later arms (decompose ops/append.rs
/// `push_union` copies the parent's ops into every arm, each with the parent's plan node): every later pipeline whose probe
/// op of `sh.slot` reads this build's life and belongs to the same plan node.
fn arm_copies<'g>(gx: &Gx<'g, '_>, sh: &Shape, q: usize, j: usize) -> PgVec<'g, (usize, usize)> {
    let ps = gx.pipelines();
    let node = |k: usize, jk: usize| ps[k].node_ops.iter().rev().find(|n| n.first_op as usize <= jk).map_or(ps[k].node_id, |n| n.node_id);
    let at = node(q, j);
    let probe = |k: usize| ps[k].ops.iter().position(|op| matches!(op, Op::Join(JoinDesc::Probe { slot, .. }) if *slot == sh.slot));
    let ours = |k: usize| gx.life_of(k, sh.slot) == Some(gravity_program::LifeId(sh.build as u16));
    (q + 1..ps.len()).filter_map(|k| probe(k).filter(|&jk| ours(k) && node(k, jk) == at).map(|jk| (k, jk))).collect_in(gx.mcx)
}

/// The filter of build `sh` on pipeline `q`'s scan site at its probe op `j`, when that probe may skip granules (`arms`),
/// every probe run follows a run of the build (`same_runs`) and the key's first part reads a scan column; true when declared.
fn declare_at(px: &mut Px<'_, '_>, sh: &Shape, q: usize, j: usize) -> bool {
    let gx = px.gx;
    let p = &gx.pipelines()[q];
    let Op::Join(JoinDesc::Probe { keys, .. }) = &p.ops[j] else { return false };
    if !arms(px, q, j) || !same_runs(px.gx, sh.build, q) {
        return false;
    }
    let Some(attno) = keys.iter().next().and_then(|k| scan_attno(p.ops[..j].iter(), k)) else { return false };
    let rows = match p.source {
        gravity_decompose::graph::Source::Range { relid, .. } => px.gx.facts.rel_rows(relid),
        _ => None,
    };
    let arg = attno as u16 as u64 | (gravity_program::cost::rtfilter_set_max(rows) as u64) << ARG_SET_AT | one();
    if reruns(px.gx, sh.build) {
        fires::fire("rtfilter.rerun"); // shapecheck fire signal (D3138: a filter re-armed per rerun)
    }
    px.declare_proto(Proto::new(ProtoKind::RtFilter, q as u16).peer(sh.build as u16).slot(sh.slot).site(DRAFT_SITE).arg(arg).reading(0, export));
    crate::frame::instr::cut(q, j);
    if p.ops[..j].iter().any(arm_marker) {
        fires::fire("join.rtfilter.arms"); // shapecheck fire signal (D4569): a union arm's scan takes the filter
    }
    true
}

/// D4569: a UNION ALL arm's marker with no pruning step (drain/lower.rs `bare_append`'s arm), which only hands the arm's rows
/// on, while road join.rtfilter.arms is on.
fn arm_marker(op: &Op<'_>) -> bool {
    matches!(op, Op::Append(gravity_decompose::ops::shared::append::AppendDesc::Arm { prune: None, .. })) && crate::face::consts::RtConst::BuildArms.on()
}

/// PS1-21 / PS1-26 step 1: join.buildside's swapped pair (build_side.rs `swaps`). Program pipeline `s.build` builds the
/// probe side's rows under the probe op's keys (`decls`), and program pipeline `s.probe` scans graph pipeline `s.build`'s
/// source and probes them: the filter sits on that scan, over the graph build op's first key column, armed by the
/// swapped build's region (`peer` = `s.build`, its slot `sh.slot`). The swap's guard proved both sides' ops and both key
/// lists raise-free (D237: a skipped granule runs nothing that could raise) and the pair INNER; a rerun set takes one only when
/// it reruns both the build and the scan (D3138, `same_runs`); a build or a scan alone in a set takes none (D2856).
pub(crate) fn declare_swapped(px: &mut Px<'_, '_>, s: &crate::ops::hashjoin::face::Swap, sh: &Shape, decls: &[JoinKeyDecl<'_>]) {
    let [d, ..] = decls else { return };
    if !crate::face::source::on(crate::admit::refusal::Variant::Strategy(JOIN_RTFILTER)) {
        return;
    }
    if !matches!(d.key.kind, Some(KeyKind::I32 | KeyKind::I64 | KeyKind::Date | KeyKind::Timestamp)) && !(decls.len() == 1 && text_fp_key(&d.key) && text_fp_ok(px)) {
        return;
    }
    if !crate::ops::shared::scan::accepts_swapped(px, s.build) || reruns(px.gx, s.build) != reruns(px.gx, s.probe) || !same_runs(px.gx, s.build, s.probe) {
        return;
    }
    let bp = &px.gx.pipelines()[s.build];
    let Some((Op::Join(JoinDesc::Build { keys, .. }), pre)) = bp.ops.split_last() else { return };
    let Some(attno) = keys.iter().next().and_then(|k| scan_attno(pre.iter(), k)) else { return };
    let rows = match bp.source {
        gravity_decompose::graph::Source::Range { relid, .. } => px.gx.facts.rel_rows(relid),
        _ => None,
    };
    let arg = attno as u16 as u64 | (gravity_program::cost::rtfilter_set_max(rows) as u64) << ARG_SET_AT | one();
    if reruns(px.gx, s.build) {
        fires::fire("rtfilter.rerun"); // shapecheck fire signal (D3138: a filter re-armed per rerun)
    }
    px.declare_proto(Proto::new(ProtoKind::RtFilter, s.probe as u16).peer(s.build as u16).slot(sh.slot).site(DRAFT_SITE).arg(arg).reading(0, export));
}

/// PS1-22: a text / varchar key imaged by its bytes' fingerprint (rule A: two `Fp128` words, a pgrc dictionary's own
/// entries' image) under a collation whose equal bytes are equal values; a nondeterministic one declares nothing.
/// textkeys step 3: a text key's set filters the probe's parts by their dictionaries' Fp128 sidecars (the host's part
/// skip), which a build of canonical images (road key.canon, rep/varlena/key_canon.rs) does not meet: none under it.
fn text_fp_ok(px: &Px<'_, '_>) -> bool {
    let canon = crate::rep::varlena::key_canon::engaged(px.strategies);
    if canon {
        fires::fire("key.canon.rtfilter"); // textkeys witness signal: a text key's set filter left off under key.canon
    }
    !canon
}

pub(crate) fn text_fp_key(k: &JoinKey) -> bool {
    k.kind == Some(KeyKind::Fp128) && k.image.is_some_and(|i| i.is_plain_text()) && k.coll.bytes_eq
}

/// The proto's `arg`: the key's scan column in bits 0..16 (the runner's `attno`), the key-set cap (the cost home's
/// `rtfilter_set_max` over the probe's rows) in bits 16..48; bits 48..63 stay the reader's bound-word ordinal (none), and
/// bit 63 is `ARG_ONE`.
const ARG_SET_AT: u32 = 16;

/// D3245: `arg` bit 63, set when road join.rtfilter.multi is off: the runner arms only the site's first filter (exec
/// topn/proto.rs) and `export` sends a build's keys only within the set cap. On, the road leaves `arg` as it was.
const ARG_ONE: u64 = 1 << 63;

/// `ARG_ONE` when road join.rtfilter.multi is off.
fn one() -> u64 {
    if crate::face::source::on(crate::admit::refusal::Variant::Strategy(JOIN_RTFILTER_MULTI)) { 0 } else { ARG_ONE }
}

/// Whether pipeline `b` is in a rerun set (a nest's or SubPlan's call, a recursive union's generation runs it again).
fn reruns(gx: &Gx<'_, '_>, b: usize) -> bool {
    gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(b as u16)))
}

/// D2856 / D3138 (colskip): whether build `b`'s filter on probe pipeline `q` holds the keys of the build run each probe run
/// follows. A build no set reruns arms once (as before D3138). A build a set reruns needs the probe in that same set, so
/// every probe run follows a run of the build: exec's RtFilter replaces (or clears) the filter after each run of the build
/// and clears it at each re-open of the set, and the source takes the cell's filter again when its generation moved
/// (hosts `rt_arm`). A recursive union's sets (`RECURSIVE_SET`, `RECURSIVE_SEED_SET`) take none: a generation re-runs the
/// term without that re-open (D2856's case stays filter-free).
fn same_runs(gx: &Gx<'_, '_>, b: usize, q: usize) -> bool {
    use gravity_decompose::ops::shared::setop::{RECURSIVE_SEED_SET, RECURSIVE_SET};
    let (b, q) = (b as u16, q as u16);
    gx.graph.reruns.iter().all(|r| !r.pipelines.contains(&b) || (r.pipelines.contains(&q) && !matches!(r.plan_id, RECURSIVE_SET | RECURSIVE_SEED_SET)))
}

/// Whether probe `j` of pipeline `q` may skip granules. D237: a skipped granule runs no op before the probe, so none of
/// them may be able to raise. D3245 (road join.rtfilter.multi): an earlier hash probe passes too when its keys and quals
/// raise on no row, its output row is the probe row's, and its build holds no error for the probe's first row (op 14,
/// `defers`) and does not run at it (`kept_road`) (effects `ops_raise_free_past_probes`). And only a probe that drops
/// its unmatched rows skips: verdict.rtconst's `accepts` takes INNER / SEMI / RIGHT / RIGHT_SEMI / RIGHT_ANTI over pgrc,
/// never a preserved probe (LEFT / FULL / ANTI). D4569: a union arm's marker passes (`arm_marker`).
fn arms(px: &Px<'_, '_>, q: usize, j: usize) -> bool {
    let gx = px.gx;
    let multi = crate::face::source::on(crate::admit::refusal::Variant::Strategy(JOIN_RTFILTER_MULTI));
    let build_holds_none = |i: usize, slot: u8| {
        gx.life_of(q, slot).is_some_and(|l| !crate::ops::hashjoin::build_class::defers(gx, l.0 as usize, q, i) && !crate::lifetime::hashjoin::kept_road(gx, q, i, l.0 as usize))
    };
    // D4569 (join.rtfilter.arms): the ops before and after a union arm's marker (`arm_marker`), the arm's own and its copy
    // of the parent's, each read as the ops of a pipeline are
    let ops = &gx.pipelines()[q].ops[..j];
    let m = ops.iter().position(arm_marker);
    let free = |from: usize, to: usize| {
        let o = &ops[from..to];
        if multi { crate::effects::ops_raise_free_past_probes(gx, o, |i, s| build_holds_none(from + i, s)) } else { ops_raise_free(gx, o) }
    };
    let before = m.map_or_else(|| free(0, j), |m| free(0, m) && free(m + 1, j));
    // D2849: PG hashes every outer row the join takes, and a key whose hash raises (effects `raising_key`) raises there,
    // in a granule no build key could match too
    let hashed = matches!(gx.pipelines()[q].ops.get(j), Some(Op::Join(JoinDesc::Probe { keys, .. })) if keys.iter().any(|n| crate::effects::raising_key(gx, n)));
    before && !hashed && crate::ops::shared::scan::accepts(px, q, j)
}

/// Op 14, fed (V4R-89): whether build `b`'s feeders (every pipeline its rows come through, `join_build_class::feed`) can have
/// their errors held for the outer-first probe as the build's own are (the driver's `Entry::defer_fed`): they are the
/// pipelines just before `b`, read by nothing but one another and `b`, and none calls a user function (its effects would
/// run where PG's never do). RISK (multi-row, parallel): see `defer_fed`: the error held is the first participant's.
pub(crate) fn held_feeders(gx: &Gx<'_, '_>, b: usize) -> bool {
    use crate::ops::hashjoin::build_class::{feed, own_side, Side};
    let p = &gx.pipelines()[b];
    let src = if let gravity_decompose::graph::Source::Breaker { slot } = p.source { Some(slot) } else { None };
    let f = feed(gx, b, p.desc.consumes.iter().copied().chain(src));
    let reads = |c: usize, x: usize| gx.pipelines()[c].desc.consumes.iter().any(|&s| gx.life_of(c, s).is_some_and(|l| l.0 as usize == x));
    let only = |x: usize| (0..gx.pipelines().len()).all(|c| c == b || f.contains(&c) || !reads(c, x));
    !f.is_empty() && f.iter().all(|&x| x < b && x + f.len() >= b && own_side(gx, x) != Side::UserFn && only(x))
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use gravity_decompose::graph::Source;
    use gravity_decompose::{Am, NoFacts, PipelineGraph, PipelineNode};
    use gravity_program::{Arena, ArenaVec, End, Pipeline, Reason, SiteDesc};
    use types_nodes::jointype::JoinType;
    use types_nodes::list::NodeList;

    use super::*;

    /// main 09-24: the filter arms over a pgrc probe that drops its unmatched rows and never over a preserved one (a
    /// skipped granule would lose a LEFT / FULL / ANTI probe's null-extended rows); it does not wait on M7 to refuse them.
    #[test]
    fn the_filter_arms_only_over_a_probe_that_drops_its_unmatched_rows() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        use JoinType::*;
        let cases = [(JOIN_INNER, true), (JOIN_SEMI, true), (JOIN_RIGHT, true), (JOIN_RIGHT_SEMI, true), (JOIN_RIGHT_ANTI, true), (JOIN_LEFT, false), (JOIN_FULL, false), (JOIN_ANTI, false)];
        for (jt, want) in cases {
            for (am, pgrc) in [(Am::Pgrcolumnar, true), (Am::Heap, false)] {
                let arena = Arena::new(1 << 16);
                let a = arena.handle();
                let nil: &NodeList<'_> = Box::leak(Box::new(NodeList::nil()));
                let src = Source::Range { site: SiteDesc::none_in(a), am, scanrelid: 1, relid: 16384 };
                let mut p = PipelineNode::new(a, 1, src, Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite));
                p.push(Op::Join(JoinDesc::Probe { slot: 0, jointype: jt, clauses: nil, keys: nil, qual: nil, unique: false, marks: false, decls: ArenaVec::new_in(a), inner_node: 2, outer_first: false, catalog: false }));
                let mut g = PipelineGraph::new(a);
                g.pipelines.push(p);
                let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
                let mut plan = gravity_lower::Sequential::new(a);
                let px = Px::new(a, &gx, &mut plan);
                assert_eq!(arms(&px, 0, 0), want && pgrc, "{jt:?} over {am:?}");
            }
        }
    }

    /// PS1-21 / PS1-26 step 1: a swapped pair's filter sits on the scan of the graph build pipeline (the one the swap
    /// made the probe) when that scan is pgrc; a heap build side takes none.
    #[test]
    fn a_swapped_pair_filters_the_build_sides_pgrc_scan_only() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        for (am, want) in [(Am::Pgrcolumnar, true), (Am::Heap, false)] {
            let arena = Arena::new(1 << 16);
            let a = arena.handle();
            let src = Source::Range { site: SiteDesc::none_in(a), am, scanrelid: 1, relid: 16384 };
            let p = PipelineNode::new(a, 1, src, Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite));
            let mut g = PipelineGraph::new(a);
            g.pipelines.push(p);
            let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
            let mut plan = gravity_lower::Sequential::new(a);
            let px = Px::new(a, &gx, &mut plan);
            assert_eq!(crate::ops::shared::scan::accepts_swapped(&px, 0), want, "{am:?}");
            assert!(!crate::ops::shared::scan::accepts_swapped(&px, 1), "no such pipeline");
        }
    }

    /// AGA-08: a build keyed by several parts exports its first part's range and, under the cap, each record's first word;
    /// a build with neither range flag exports nothing.
    #[test]
    fn a_key_of_several_parts_exports_its_first_parts_range_and_set() {
        use crate::ops::hashjoin::build::{HDR, N, REC, WORDS};
        use crate::ops::hashjoin::rt::wr;
        let mut buf = vec![0u64; 512];
        let d = buf.as_mut_ptr() as *mut u8;
        let run = |flags: u8| {
            let mut got = Vec::new();
            // SAFETY: `buf` (4 KiB) holds the header and three 24-byte records (two key words and the link) from HDR.
            let n = unsafe {
                (wr::<u8>(d, 15, KIND), wr::<u8>(d, PHASE, 2), wr::<u8>(d, FLAGS, flags), wr::<u8>(d, WORDS, 2), wr::<u32>(d, REC, 24));
                (wr::<u64>(d, N, 3), wr::<i64>(d, LO, 5), wr::<i64>(d, HI, 40));
                for (i, (a, b)) in [(40i64, 7i64), (5, -3), (17, 1 << 40)].into_iter().enumerate() {
                    (wr::<i64>(d, HDR + 24 * i, a), wr::<i64>(d, HDR + 24 * i + 8, b));
                }
                export(d, &[], None, &mut |k, a, b| got.push((k, a as i64, b as i64)))
            };
            (n, got)
        };
        assert_eq!(run(F_RANGE0), (Some(4), vec![(0, 5, 40), (1, 40, 0), (1, 5, 0), (1, 17, 0)]));
        assert_eq!(run(0), (None, vec![]));
    }

    /// D2856: a build a rerun set runs again (scan-0235: a recursive union's term builds over the work table every
    /// generation) takes no filter; a build outside every set (run once, its keys fixed) keeps it.
    #[test]
    fn a_build_a_rerun_set_runs_again_takes_no_filter() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        use gravity_decompose::graph::Rerun;
        use gravity_decompose::ops::shared::setop::RECURSIVE_SET;
        for (members, plan_id) in [(&[1u16, 2][..], RECURSIVE_SET), (&[1u16, 2][..], 0), (&[2u16][..], RECURSIVE_SET)] {
            let arena = Arena::new(1 << 16);
            let a = arena.handle();
            let mut g = PipelineGraph::new(a);
            for id in 0..3 {
                let src = Source::Range { site: SiteDesc::none_in(a), am: Am::Pgrcolumnar, scanrelid: 1, relid: 16384 };
                g.pipelines.push(PipelineNode::new(a, id, src, Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite)));
            }
            let mut pipelines = ArenaVec::new_in(a);
            pipelines.extend_from_slice(members);
            g.reruns.push(Rerun { node_id: 9, plan_id, pipelines, slots: [0u64; 4] });
            let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
            assert_eq!(reruns(&gx, 1), members.contains(&1), "build 1, set {members:?} plan {plan_id}");
            assert!(!reruns(&gx, 0), "build 0 is in no set");
        }
    }

    /// D3138 (colskip): a nest's or SubPlan's set that reruns both the build and its probe takes the filter (re-armed per
    /// run by exec); a build the set reruns without the probe, or any recursive union's set over the build, takes none;
    /// a build outside every set takes it whatever the probe's sets (it runs once, as before).
    #[test]
    fn a_set_rerunning_both_the_build_and_its_probe_takes_the_filter() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        use gravity_decompose::graph::Rerun;
        use gravity_decompose::ops::shared::setop::{RECURSIVE_SEED_SET, RECURSIVE_SET};
        let cases: [(&[u16], i32, usize, usize, bool); 8] = [
            (&[1, 2], 0, 1, 2, true),
            (&[1, 2], 3, 1, 2, true),
            (&[1, 2], RECURSIVE_SET, 1, 2, false),
            (&[1, 2], RECURSIVE_SEED_SET, 1, 2, false),
            (&[1], 0, 1, 2, false),
            (&[1], 3, 1, 2, false),
            (&[2], 0, 1, 2, true),
            (&[2], RECURSIVE_SET, 0, 2, true),
        ];
        for (members, plan_id, b, q, want) in cases {
            let arena = Arena::new(1 << 16);
            let a = arena.handle();
            let mut g = PipelineGraph::new(a);
            for id in 0..3 {
                let src = Source::Range { site: SiteDesc::none_in(a), am: Am::Pgrcolumnar, scanrelid: 1, relid: 16384 };
                g.pipelines.push(PipelineNode::new(a, id, src, Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite)));
            }
            let mut pipelines = ArenaVec::new_in(a);
            pipelines.extend_from_slice(members);
            g.reruns.push(Rerun { node_id: 9, plan_id, pipelines, slots: [0u64; 4] });
            let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
            assert_eq!(same_runs(&gx, b, q), want, "build {b} probe {q}, set {members:?} plan {plan_id}");
        }
    }
}
