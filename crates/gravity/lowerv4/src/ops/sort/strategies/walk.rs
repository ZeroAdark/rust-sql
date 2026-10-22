//! Road `topk.walk` (cards/M3.md L7; H07 b / d): the zone / group walk. The claims of a walked pipeline come in exact
//! zone-minimum order and the walk stops at the first granule whose minimum lies past the bound the workers' partials
//! prove. frame/program.rs hands every made pipeline to `WalkImpl::declare` (face/consts.rs `walk`, V4R-148) while the
//! road applies and is on.
//! - The group walk (`Limit -> HashAgg[ZoneTopK]`, V4R-8): the pipeline that builds the group declares the run
//!   `TopKDesc::run(n).walking(attno)`: the runner orders its claims by the resident zone order (installed on the list, so
//!   the pool's runtime ranges are walk positions at any width; D504) and stops them at `HashAggPartial.limit`'s bound, the
//!   worker table's `n`-th smallest distinct first key. It declares the walk protocol over its site, the attribute and the
//!   group's slot (arg bits 16 a group walk, 18 the runner's claims): exec `v4/m3_walk.rs` cuts the owner runs after the
//!   pipeline at the `n`-th smallest distinct first key they hold, at most every proven bound, so the groups kept are
//!   exact (every granule holding one of their rows was visited and folded whole) and the LIMIT above takes only those.
//!   The keyed end skips the rows past the worker's bound (keyed/walk_skip.rs, `group`, V4R-177).
//! - It walks only what is sound as written: a constant `count + offset` with no ties and nothing but projections between
//!   the group's emit and the Limit, or a top-k Sort whose first key is the group's first key ascending (RUNS-02:
//!   `GROUP BY k1 .. ORDER BY k1 [, ..] LIMIT n`; the groups kept hold the first `n` in that order; GP-1b: `ORDER BY k1 DESC`
//!   too, the walk `descending` and every bound / cut / skip over the NOT of the key, so the same ascending tests hold); a NOT NULL first key whose word is its value (`walk_image`) that is the hint's
//!   attribute, over a pgrc relation. Anything else runs the
//!   whole group (the same rows: a LIMIT with no ORDER BY takes any `n` groups). Either walk also needs every skipped
//!   row to be one PostgreSQL could not raise on (D554, `skip_safe`): raise-free filters, projections and aggregates.
//! - The row walk (`Limit -> Sort[ZoneWalk]`, also under topk.fetch; D508): the top's pipeline declares the same
//!   runner walk (`TopKDesc::run(n).walking(attno)`, arg bit 18, no group bit). Its bound is a worker's cut run's row
//!   `n - 1` key 0 (kernels m3_cut.rs marks the run `RUN_CUT`; exec v4/m3_sort.rs `run_bound` reads it through the merge,
//!   a bound-only merge at width 1): at least the statement's `n`-th key 0, since a worker's run holds a subset of the
//!   rows; a granule is skipped only when its minimum lies strictly past it, so every row of the top and its ties is read.
//!   It walks key 0 = the hint's column ASC whose word is its value (`walk_image`: the pair's word `^ 1 << 63` reads it
//!   back, the zone minimum's order), NULLS LAST (a NULL row n - 1 arms nothing; zone minima skip
//!   NULLs) or NULLS FIRST, a constant `count + offset`, over a pgrc relation. DESC walks too (PM13,
//!   `TopKDesc::descending`): its pair word is the NOT of the ascending one, so the runner orders the granules by the NOT
//!   of their maxima and the same bound test skips a granule whose maximum lies strictly under row n - 1's key. A
//!   nullable NULLS FIRST (either direction; PL-06, `TopKDesc::nulls_first`): the runner gives every granule holding a
//!   NULL (its stats record's non-null count under its rows) the lowest walk key, so it walks first and no bound skips
//!   it; a NULL row n - 1 bounds at that key (exec merge.rs `run_bound`): only those granules are read.
//! Width: road `width.walk` (runs/walk_width.rs) sizes the walk's claims from the residency of its next granules; a row
//! walk with no filter under the top takes it too, unless a nullable key's NULLs lead (its census counts value rows: exec
//! walk.rs).
//! Owner M3-WALK. Budget 180 lines.
use gravity_abi::breaker::{Breaker, BreakerKind, ORDER_KEY, ORDER_NONE};
use gravity_decompose::ops::shared::agg::{AggDecl, GroupDecl};
use gravity_decompose::graph::Bound;
use gravity_decompose::ops::shared::sort::KeyImage;
use gravity_decompose::ops::shared::sort::{SortDesc, SortForm};
use gravity_decompose::{AttFact, Hint, Op, Source};
use gravity_program::protos::{Proto, ProtoKind};
use gravity_program::{Pipeline, TopKDesc};

use crate::face::consts::{WalkImpl, WalkMech};
use crate::face::kinds::KindId;
use crate::face::strategy::WIDTH_WALK;
use crate::frame::iface::{breaker_of, limit_bounds, limit_with_ties};
use crate::frame::px::Px;
use crate::graph::Gx;

crate::strategies! {
    /// Zone / group walk: ordered claims, stop at the proven bound.
    TOPK_WALK = 66, "topk.walk", Walkable;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(TOPK_WALK, &WalkImpl(&ZONE_WALK))], ..crate::ops::NONE };

/// `ProtoKind::Walk`'s `arg`: the walked attribute in the low 16 bits; bit 16 a group walk (the owner runs cut at the
/// bound after the pipeline); bit 17 width.walk (exec `v4/m3_walk.rs`).
pub(crate) const WALK_GROUP: u64 = 1 << 16;
pub(crate) const WALK_WIDTH: u64 = 1 << 17;
/// bit 18: the runner orders the claims (`TopKDesc.walking`), the protocol only cuts (D504: the pool's runtime cuts its own
/// unit ranges, so the protocol's lazy claims never order a walk at W >= 2).
pub(crate) const WALK_RUNNER: u64 = 1 << 18;

struct ZoneWalk;
static ZONE_WALK: ZoneWalk = ZoneWalk;

impl WalkMech for ZoneWalk {
    fn declare(&self, px: &mut Px<'_, '_>, k: usize, site: Option<usize>, desc: &mut Pipeline) {
        let gx = px.gx;
        let shape = px.after_of(k);
        let p = &gx.pipelines()[shape];
        if let Some(sd) = p.ops.iter().find_map(|op| match op { Op::Sort(d) if matches!(d.form, SortForm::Walk { .. }) => Some(d), _ => None }) {
            let (Some(site), Some(Breaker { kind: BreakerKind::Sort, .. })) = (site, breaker_of(desc)) else { return };
            let at = p.ops.iter().position(|op| matches!(op, Op::Sort(_))).unwrap_or(0);
            let Some((n, attno, down, nulls)) = row_walk(gx, shape, sd).filter(|_| skip_safe(gx, shape, at, &[])) else { return };
            fires::fire("topk.walk.rows"); // shapecheck fire signal (REGISTRY RUNS-01, OIDW-53)
            let mut walk = TopKDesc::run(n).walking(attno);
            if down {
                fires::fire("topk.walk.desc"); // shapecheck fire signal (PM13: the walk goes down the key maxima)
                walk = walk.descending();
            }
            if nulls {
                fires::fire("topk.walk.nulls_first"); // shapecheck fire signal (PL-06: the granules holding a NULL walk first)
                walk = walk.nulls_first();
            }
            desc.topk = Some(walk);
            // RD-width.walk: the section's width from the census by rows (exec walk.rs), when every scanned row enters the
            // top (no filter under it: a granule's rows are the run's) and its values come first (NOT NULL, or NULLS LAST:
            // hosts counts a nullable key's non-null values per granule); a NULLS FIRST walk's NULL rows lead, unsized
            let rows = p.ops[..at].iter().all(|op| matches!(op, Op::Project(_))) && !nulls;
            let width = if rows && crate::face::policy::Width::Walk.on() && px.strategies.has(WIDTH_WALK) { crate::ops::sort::strategies::walk_width::arg(px) } else { 0 };
            px.declare_proto(Proto::new(ProtoKind::Walk, k as u16).site(site as i16).arg(attno as u16 as u64 | WALK_RUNNER | width));
            return;
        }
        let Some(g) = gx.group_at(shape) else { return };
        let Some((n, attno)) = group_walk(gx, shape, g) else { return };
        // the keyed end skips rows past the bound under this walk (`group`, V4R-177): a walked pipeline has its site and end
        let (Some(site), Some(b @ Breaker { kind: BreakerKind::HashAgg, .. })) = (site, breaker_of(desc)) else {
            crate::refuse!(g.node_id, crate::admit::refusal::Reason::Bug { payload: "a walked group's pipeline without its site or HashAgg end" })
        };
        let width = if crate::face::policy::Width::Walk.on() && px.strategies.has(WIDTH_WALK) { crate::ops::sort::strategies::walk_width::arg(px) } else { 0 };
        // GP-1b: a DESC group walk goes down the key maxima (`descending`); its bound, cut and row skip compare the NOT
        // of the key (walk_skip.rs, exec walk.rs / raw.rs, parallel merge.rs `HashAggPartial::bound`)
        let walk = TopKDesc::run(n).walking(attno);
        desc.topk = Some(if crate::ops::shared::agg::group_desc(g) { fires::fire("topk.walk.group.desc"); walk.descending() } else { walk }); // shapecheck fire signal (GP-1b)
        // D518 lever B: the cut and the LIMIT need no key order, so the owner's runs stay unsorted (decompose's ZoneTopK ORDER_KEY)
        if let Some(o) = desc.owner.as_mut().filter(|o| o.order_slot == ORDER_KEY && o.pair == 0) {
            o.order_slot = ORDER_NONE;
        }
        fires::fire("topk.walk.group"); // shapecheck fire signal (REGISTRY RUNS-02, NEW-lowerv4-a2-268, BN-136)
        px.declare_proto(Proto::new(ProtoKind::Walk, k as u16).site(site as i16).slot(b.slot).arg(attno as u16 as u64 | WALK_GROUP | WALK_RUNNER | width));
    }

    /// `declare`'s group walk on pipeline `k`, which the keyed end reads before `declare` runs (V4R-177).
    fn group(&self, px: &Px<'_, '_>, k: usize, g: &GroupDecl<'_>) -> Option<(u32, i16)> {
        let shape = px.after_of(k);
        px.gx.group_at(shape).filter(|x| core::ptr::eq(*x, g)).and_then(|g| group_walk(px.gx, shape, g))
    }
}

/// The group walk `(count + offset, attno)` of group `g`, which pipeline `shape` builds into its HashAgg slot, when the
/// group walks as written (the module doc's conditions). `None`: the group runs whole.
fn group_walk(gx: &Gx<'_, '_>, shape: usize, g: &GroupDecl<'_>) -> Option<(u32, i16)> {
    let attno = g.hints.iter().find_map(|h| match h { Some(Hint::ZoneTopK { attno, nulls_first: false, .. }) => Some(*attno), _ => None })?;
    let p = &gx.pipelines()[shape];
    let hashagg = matches!(breaker_of(&p.desc), Some(Breaker { kind: BreakerKind::HashAgg, slot, .. }) if slot == g.slot);
    walk_n(gx, g, crate::ops::shared::agg::group_desc(g)).filter(|_| hashagg && walkable(gx, shape, g, attno) && skip_safe(gx, shape, p.ops.len(), &g.aggs)).map(|n| (n, attno))
}

/// D554: a walk leaves unread the granules past its bound, whose rows PostgreSQL's HashAgg / Sort still evaluates. It
/// walks only when nothing it skips can raise or act (D449's rule, props merge.rs `ops_can_raise`): the ops before `upto`
/// are raise-free filters and projections, and each aggregate folds a skipped row as PG does none (the effects home's
/// `walk_folds`). Otherwise the plan runs whole.
fn skip_safe(gx: &Gx<'_, '_>, shape: usize, upto: usize, aggs: &[AggDecl<'_>]) -> bool {
    let ops = &gx.pipelines()[shape].ops[..upto];
    ops.iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_))) && !crate::props::ops_can_raise(gx, ops) && aggs.iter().all(|a| crate::effects::visible::agg::walk_folds(gx, a))
}

/// The row walk's `(count + offset, attno, desc, nulls first over a nullable key)` when the top walks as written (the
/// module doc's conditions); `None`: whole.
fn row_walk(gx: &Gx<'_, '_>, shape: usize, sd: &SortDesc<'_>) -> Option<(u32, i16, bool, bool)> {
    let SortForm::Walk { attno, desc, nulls_first, count: Bound::Const(c), offset, .. } = sd.form else { return None };
    let key = sd.keys.first()?;
    let n = limit_n(c, &offset)?;
    let src = &gx.pipelines()[shape].source;
    let (_, relid, _) = src.relation()?;
    let att = gx.facts.att(relid, attno)?;
    let int = walk_image(&att, key.image);
    (int && key.desc == desc && key.nulls_first == nulls_first && matches!(src, Source::Range { .. }) && gx.source_kind(shape) == KindId::SrcPgrc).then_some((n, attno, desc, nulls_first && !att.notnull))
}

/// RUNS-03: a walked key's word is its value, so a zone minimum (the column's values) bounds it: its order image is
/// KeySem's (`keysem::word_orders_as_i64` over `order_image`; no type list here) and the key holds that image (`KeyImage::Raw`), a signed one at
/// any width (sign-extended) or an unsigned one narrower than a word (zero-extended: bool, "char", oid), each ordered as an
/// i64 (rep/word/order.rs `int_order`; exec merge.rs `run_bound` reads a pair's `word ^ 1 << 63` back to it). An unsigned
/// 8-byte image (xid8, pg_lsn) is its word flipped, which no zone minimum is; a domain has no image of its own
/// (`order_image` answers its base's type only): neither walks. The planner hints only its zone types
/// (gravity_paths.rs `zone_word_type`), all of them answered here.
fn walk_image(att: &AttFact, image: KeyImage) -> bool {
    gravity_fn::typesem::keysem::word_orders_as_i64(att.typid, att.byval, att.typlen) && image.is_raw()
}

/// `count + offset` of constant bounds, both positive (an offset may be absent).
fn limit_n(c: i64, offset: &Bound<'_>) -> Option<u32> {
    let o = match offset {
        Bound::All => 0,
        Bound::Const(o) if *o >= 0 => *o,
        _ => return None,
    };
    u32::try_from(c.checked_add(o)?).ok().filter(|&n| n > 0 && c > 0)
}

/// The group's walk bound `count + offset`: the Limit over the group's emit, or (RUNS-02) a top-k Sort over it whose first
/// key is the group's first key ascending (`GROUP BY k1 .. ORDER BY k1 [, ..] LIMIT n`: the groups the cut keeps hold the
/// first `n` in that order, each exact, and the Sort orders them), reached through projections only, with constant bounds
/// and no ties; `None` otherwise (the group runs whole).
fn walk_n(gx: &Gx<'_, '_>, g: &GroupDecl<'_>, desc: bool) -> Option<u32> {
    let emit = gx.drainers(g).first().copied()?;
    let ops = &gx.pipelines()[emit].ops;
    let j = ops.iter().position(|op| !matches!(op, Op::Project(_)))?;
    if let Op::Sort(sd) = &ops[j] {
        let SortForm::TopK { count: Bound::Const(c), offset, ties: false } = &sd.form else { return None };
        // the key's own word order (the cut's): ascending under the type's default btree family
        let key = sd.keys.first().filter(|k| k.desc == desc && !k.own_family && k.no_proc.is_none() && k.image.is_raw())?;
        let first = crate::ops::sort::topn::order_slot::key_expr(&ops[..j], sd, key.resno, gx.mcx)?.as_var().filter(|v| v.varno == types_nodes::primnodes::OUTER_VAR && v.varlevelsup == 0)?;
        return (g.keys.first().is_some_and(|k| k.resno == first.varattno)).then(|| limit_n(*c, offset)).flatten();
    }
    let op = &ops[j];
    if desc {
        return None; // a descending walk is the planner's under a DESC Sort only
    }
    let (Bound::Const(c), offset) = limit_bounds(op)? else { return None };
    if limit_with_ties(op) {
        return None;
    }
    limit_n(*c, offset)
}

/// The first key is the hint's column of the pgrc relation that pipeline `shape` alone scans into the group, NOT NULL,
/// with an integer image (decompose checked the hint names the first key's attribute).
fn walkable(gx: &Gx<'_, '_>, shape: usize, g: &GroupDecl<'_>, attno: i16) -> bool {
    let Some(key) = g.keys.first() else { return false };
    let src = &gx.pipelines()[shape].source;
    let Some((_, relid, _)) = src.relation() else { return false };
    let Some(att) = gx.facts.att(relid, attno) else { return false };
    let (int, notnull) = (walk_image(&att, key.image), att.notnull);
    // the group's producers by its life, never by slot number: decompose reuses a freed number, so another InitPlan's HashAgg or
    // the parent's PlainAgg may hold it (D2914); an Append's arms all fold into the group and refuse
    let alone = g.producer.0 as usize == shape && (0..gx.pipelines().len()).filter(|&q| gx.group_at(q).is_some_and(|x| core::ptr::eq(x, g))).count() == 1;
    int && key.attno == attno && notnull && alone && matches!(src, Source::Range { .. }) && gx.source_kind(shape) == KindId::SrcPgrc
}
