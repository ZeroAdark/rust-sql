//! Filter-only lane elision (H02e; KEEP lowerv2 source/range.rs site_for's filter lanes and v3-r1-a4b's `zoned`): in a
//! window whose zone verdict is `AllMatch` the granule host leaves the site's filter-only lanes undecoded (null lane
//! pointers; `granule_source::lane_elision_into` under `VERDICT_ELIDE`), and the frame runs a covered window only through
//! the body that skips its covered steps (M1-FRAME 8c4893fd818). A lane is elided when the decomposer found it read by
//! the scan's quals alone (not projected: its `filter_lanes`) and no qual outside the covered ops reads it; the pgrc
//! source asks when the zone covers every covered op, so an `AllMatch` window is always a covered one, or, under
//! `VERDICT_ELIDE_HIT` (V4R-4), when the condition cache covers an op the zone does not: the host then elides only in a
//! window a hit decided. Under that bit the candidates also take the decomposer's `qual_only` side table (decision 12 b:
//! an unprojected column only non-zone quals read, e.g. a numeric compared in a WHERE), never a zone `AllMatch` window's.
//! Registers road `verdict.elide`. Owner M1-SOURCE.
use gravity_abi::breaker::BreakerKind;
use gravity_decompose::{Op, Source};
use gravity_program::{shred_kind, End, LaneId, SiteDesc};
use mcx::{CollectIn, PgVec, GrowIn};
use types_nodes::Node;

use crate::face::source::{VerdictImpl, VerdictMech};
use crate::frame::px::{ExId, Px};
use crate::ops::Table;

crate::strategies! {
    /// Filter-only lane elision.
    VERDICT_ELIDE = 13, "verdict.elide", PgrcFilter;
}

pub(crate) const ROWS: Table = Table { strategies: &[(VERDICT_ELIDE, &VerdictImpl(&Elide))], ..crate::ops::NONE };

struct Elide;

impl VerdictMech for Elide {
    /// `true` when `qual` is an exact zone predicate over a filter-only lane: an `AllMatch` window may leave it undecoded.
    fn declare(&self, px: &mut Px<'_, '_>, site: &mut SiteDesc, qual: ExId) -> bool {
        crate::ops::scan::pgrc::verdict_zone::of(px, site, qual).is_some_and(|(z, exact)| exact && site.filter_lanes.contains(z.lane as usize))
    }
}

/// The lanes of pipeline `k`'s drafted `site` an `AllMatch` window leaves undecoded when the frame skips the leading
/// filter ops in `covers` (bit j: op j): the decomposer's filter-only lanes that no filter outside `covers` reads and,
/// when the pipeline keeps its rows (`keeps_rows`) or a later join may spill its probe row, no projection reads (D2338:
/// a merge inner's run row or a spilled probe row is the scan's whole target list, filled in a covered window), and
/// under `hit_only` (VERDICT_ELIDE_HIT: only a window a condition-cache hit decided) its `qual_only` columns too. None
/// when a filter reads a whole row or a node whose reads are unknown.
pub(crate) fn lanes<'g>(px: &Px<'g, '_>, k: usize, site: &SiteDesc, covers: u64, hit_only: bool) -> PgVec<'g, usize> {
    walk(px, k, site, covers, hit_only, (keeps_rows(px.gx.pipelines()[k].end()), false))
}

/// The packed body's first-only lanes of pipeline `k`'s drafted `site` (bit l: site lane l; lower.rs `Declared`): the
/// decomposer's filter-only lanes (nothing above the scan reads them: `cb_target_cols`) that no filter past op 0 reads
/// and, when the end hands its projected rows on whole (`keeps_rows`) or a later join may spill its probe row, no
/// projection reads. KEYED-24: a group's DISTINCT breaker (its group declared) over filters, gates and projections
/// alone hands on no projected row, as an aggregate does not: its record holds its keys and aggregate slots alone
/// (unique.rs D527), all read above the scan; there the gates' reads count too. Any other op past op 0 keeps the
/// breaker a row keeper: a join (a spilled probe row, a merge inner's run row: D2338), a sort (topk.fetch's late
/// fetch), a lock or a write (EPQ's recheck), a limit, a window or a sub-plan.
pub(crate) fn first_only(px: &Px<'_, '_>, k: usize, site: &SiteDesc) -> u64 {
    let p = &px.gx.pipelines()[k];
    let plain = p.ops.iter().skip(1).all(|o| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_)));
    let group = plain && matches!(p.end(), End::Breaker(b) if b.kind == BreakerKind::Distinct) && px.gx.group_at(k).is_some();
    walk(px, k, site, 1, false, (keeps_rows(p.end()) && !group, group)).iter().filter(|&&l| l < 64).fold(0, |m, &l| m | 1 << l)
}

/// `lanes`' walk: `keeps` counts every projection; `gates` counts the gates' reads too.
fn walk<'g>(px: &Px<'g, '_>, k: usize, site: &SiteDesc, covers: u64, hit_only: bool, (keeps, gates): (bool, bool)) -> PgVec<'g, usize> {
    let p = &px.gx.pipelines()[k];
    let mcx = px.mcx();
    let Source::Range { scanrelid, relid, .. } = &p.source else { return PgVec::new_in(mcx) };
    let n = site.lanes().len();
    let mut read = mcx::pg_vec![in mcx; false; n];
    for (j, op) in p.ops.iter().enumerate() {
        let quals: PgVec<'_, Node<'_>> = match op {
            Op::Filter(q) if !(j < 64 && covers & (1 << j) != 0) => q.as_list().map_or_else(|| mcx::pg_vec![in mcx; *q], |l| l.iter().collect_in(mcx)),
            Op::Gate(q) if gates => q.as_list().map_or_else(|| mcx::pg_vec![in mcx; *q], |l| l.iter().collect_in(mcx)),
            Op::Project(cols) if keeps || p.ops[j + 1..].iter().any(|o| matches!(o, Op::Join(_))) => {
                cols.iter().collect_in(mcx)
            }
            _ => continue,
        };
        for q in quals {
            let mut whole = false;
            let known = gravity_decompose::expr::walk(q, &mut |x| {
                if let Some(v) = x.as_var().filter(|v| v.varno == *scanrelid as i32 && v.varlevelsup == 0) {
                    whole |= v.varattno == 0;
                    if let Some(l) = site.lanes().iter().position(|l| l.id == LaneId::Attr(v.varattno)) {
                        read[l] = true;
                    }
                }
                true
            });
            if !known || whole {
                return PgVec::new_in(mcx);
            }
        }
    }
    let qual_only = |l: usize| {
        hit_only
            && match site.lanes()[l].id {
                LaneId::Attr(a) => px.gx.graph.qual_only.iter().any(|e| e.node_id == p.node_id && e.relid == *relid && e.attno == a),
                _ => false,
            }
    };
    let elided = |l: usize| (site.filter_lanes.contains(l) || qual_only(l)) && !read[l];
    // M6 R3: a path's code lane (`shred_kind::TEXT_CODES`) only compares over its column read goes with its elided column
    let col = |l: usize| site.lanes().iter().position(|c| site.out_attnos.get(l).is_some_and(|&a| c.id == LaneId::Attr(a)));
    let coded = |l: usize| site.shred_of(l).is_some_and(|d| d.kind == shred_kind::TEXT_CODES) && col(l).is_some_and(elided);
    (0..n).filter(|&l| elided(l) || coded(l)).collect_in(mcx)
}

/// sortpath: the path lanes of pipeline `k`'s drafted `site` (any `shred_kind`) a covered filter op in `covers` (bit j:
/// op j) compares and nothing else reads: no uncovered filter, gate, projection, group key or aggregate names a path of
/// that column the lane's path extends or that extends it (a group key over the projection's entry counts by its path
/// alone). In a covered window (a zone `AllMatch`, or a condition-cache hit under `VERDICT_ELIDE_HIT`) the frame skips
/// the compare, so the lane is left undecoded beside a document the rows past the filters still read (the host's late
/// guard then reads no elided path lane: `guard_fill`). Empty when an op past the filters is anything else or a read is
/// unknown. Speed only: an elided lane is one no step of a covered window reads.
pub(crate) fn path_lanes<'g>(px: &Px<'g, '_>, k: usize, site: &SiteDesc, covers: u64) -> PgVec<'g, usize> {
    use crate::ops::scan::pgrc::strategies::jsonb_lanes::{contained, field_in, key_test};
    let p = &px.gx.pipelines()[k];
    let mcx = px.mcx();
    let Source::Range { scanrelid, .. } = &p.source else { return PgVec::new_in(mcx) };
    let scan = *scanrelid as i32;
    if site.shreds.is_empty() {
        return PgVec::new_in(mcx);
    }
    // (column or None for a projection entry's path, path) of every field an uncovered read names; and the covered ones
    let (mut other, mut mine): (PgVec<'_, (Option<i16>, PgVec<'g, u8>)>, PgVec<'_, (i16, PgVec<'g, u8>)>) = (PgVec::new_in(mcx), PgVec::new_in(mcx));
    let mut known = true;
    let mut see = |n: Node<'_>, out: &mut PgVec<'_, (Option<i16>, PgVec<'g, u8>)>| {
        known &= reads(n, &mut |x| {
            if let Some((a, path)) = field_in(x, scan, mcx) {
                out.push_in((Some(a), path));
            } else if let Some((_, path)) = field_in(x, types_nodes::primnodes::OUTER_VAR, mcx) {
                out.push_in((None, path));
            }
            // a template's leaves and a key test's paths read path lanes too (jsonb_codes `contained`, jsonops (3))
            for (a, path) in contained(x, scan, mcx).into_iter().flatten().chain(key_test(x, scan, mcx).into_iter().flat_map(|(_, v)| v)) {
                out.push_in((Some(a), path));
            }
        });
    };
    for (j, op) in p.ops.iter().enumerate() {
        match op {
            Op::Filter(q) if j < 64 && covers & (1 << j) != 0 => {
                let mut v = PgVec::new_in(mcx);
                see(*q, &mut v);
                mine.extend_in(v.into_iter().filter_map(|(a, path)| a.map(|a| (a, path))));
            }
            Op::Filter(q) | Op::Gate(q) => see(*q, &mut other),
            Op::Project(cols) => cols.iter().for_each(|c| see(c, &mut other)),
            _ => return PgVec::new_in(mcx),
        }
    }
    if let Some(g) = px.gx.group_at(k) {
        g.keys.iter().for_each(|x| see(x.expr.node(), &mut other));
        g.aggs.iter().for_each(|a| {
            see(a.node, &mut other);
            a.filter.into_iter().for_each(|f| see(f, &mut other));
        });
    }
    if !known {
        return PgVec::new_in(mcx);
    }
    let related = |a: &[u8], b: &[u8]| {
        let (s, l) = if a.len() <= b.len() { (a, b) } else { (b, a) };
        l.starts_with(s) && (l.len() == s.len() || l[s.len()] == 0)
    };
    site.shreds
        .iter()
        .filter(|d| {
            let (l, path) = (d.lane as usize, site.table_arg_at(d.path));
            let Some(&a) = site.out_attnos.get(l).filter(|_| site.entries.is_none()) else { return false };
            mine.iter().any(|(c, q)| *c == a && &q[..] == path) && !other.iter().any(|(c, q)| c.is_none_or(|c| c == a) && related(q, path))
        })
        .map(|d| d.lane as usize)
        .collect_in(mcx)
}

/// `path_lanes`' walk: `f` on `n` and each node below it; false when a node's reads are unknown. An aggregate's DISTINCT
/// or ORDER BY clause (a SortGroupClause) names an argument and reads nothing (jsonb_keys `agg_reads`, D3023).
fn reads<'p>(n: Node<'p>, f: &mut dyn FnMut(Node<'p>)) -> bool {
    crate::stack_check();
    if n.node_tag() == types_nodes::NodeTag::T_SortGroupClause {
        return true;
    }
    f(n);
    let mut ok = true;
    let known = gravity_decompose::expr::kids(n, &mut |x| ok &= reads(x, f));
    ok && known
}

/// Whether a pipeline ending in `end` hands its projected rows on whole: a sink emits them, a Materialize / Sort / hash
/// build / Distinct / top-k stores them, so every column a projection reads is live. An aggregate consumes only its
/// arguments, which the decomposer already counts as projected (a scan under an ungrouped aggregate keeps its physical
/// target list, whose other columns are dead).
fn keeps_rows(end: End) -> bool {
    match end {
        End::Sink => true,
        End::Breaker(b) => !matches!(b.kind, BreakerKind::PlainAgg | BreakerKind::HashAgg),
    }
}
