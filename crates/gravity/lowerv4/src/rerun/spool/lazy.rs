//! D2157 (nest.spool, material's home): the eager form's fill run at the nest's first outer row. PostgreSQL's Material
//! reads its child at its first fetch only (ExecMaterial), so under an outer empty at run time the child never runs; the
//! eager form (rerun/spool/form.rs `spool_fill`'s `(inner, false)`) ran its fill pipeline ahead, in pipeline order, the
//! whole child included. Here the fill pipeline and its producers are a D824 lazy build of the nest's pipeline, as
//! merge.join's lazy inner is (hashjoin's build_class.rs `lazy_group`): skipped in pipeline order, the Material's slot
//! open and pending, run when the nest's first outer row waits on it (probe_wait.rs `wait`'s protocol, over the nest
//! step's offset word, which the wait leaves 0: the row then opens the inner fresh). An outer that hands on no row never runs them.
//! The eager form's facts hold (invariant, raise-free, not VOLATILE), so running the fill later changes nothing a
//! statement shows but its time; a fill that may raise or call VOLATILE code (form.rs `deferred`, D1108 row 3) runs here
//! whole where PostgreSQL's first inner scan pulls it whole, or the statement refuses. The nest's pipeline already runs
//! serially (nest.loop's `StepMech::serial`); D1005 pins it to the leader, and the deferred pipelines run there at the
//! wait, at width 1 (exec's runner/producers.rs runs no section for a lazy pipeline): only a fill no worker would share
//! goes lazy, so no fill loses its parallelism; a fill over a parallel-eligible scan stays eager at
//! every width (the program is the same at each).
use gravity_abi::ctx::{hdr, FLAG_PENDING_SHIFT};
use gravity_abi::status::build_wait;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::{Op, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Type};
use mcx::{CollectIn, GrowIn, PgVec};

use crate::frame::env::Env;
use crate::frame::px::Px;
use crate::frame::row::RowVals;

/// The nest's wait on its lazy fill: the Material slot's index among the nest pipeline's consumed regions.
pub(crate) struct Lazy {
    consumed: usize,
}

/// The nest at op `j` of graph pipeline `k`, whose inner is the one reader of an eager-form plan Material: its fill
/// pipeline and their producers deferred to the nest's first outer row (`Px::lazy`), when `lazy_group` admits them
/// (every pipeline it skips past is quiet, the fill read by the nest alone, none lazy for another probe). `None`: the
/// fill runs in pipeline order, as before. Construct C2: with it, the licence of that run ahead where this file lets a
/// loud fill the wait cannot defer run in pipeline order (`Px::ahead_unchecked`'s, tagged: `fill_wait`, `drain_wait`);
/// `None` where the pipeline-order run is not this file's departure (the eager form's own, its licence held by
/// material/lower.rs `Fill.ahead`; a settled union's, rerun/union_arms.rs; a quiet fill's) or the fill waits.
pub(crate) fn declare(px: &mut Px<'_, '_>, k: usize, j: usize) -> (Option<Lazy>, Option<gravity_decompose::effects::Licence>) {
    let mut ahead = None;
    let lazy = fill_wait(px, k, j, &mut ahead);
    (lazy, ahead)
}

/// `declare`'s wait, the licence of a loud fill's run ahead in pipeline order put in `ahead`.
fn fill_wait(px: &mut Px<'_, '_>, k: usize, j: usize, ahead: &mut Option<gravity_decompose::effects::Licence>) -> Option<Lazy> {
    let gx = px.gx;
    let ps = gx.pipelines();
    if k >= ps.len() {
        return None; // a nest inside an inner: its owner's call runs it (one level only)
    }
    let ik = ps.inner_of(k, j)?;
    let ip = &ps[ik];
    let Source::Breaker { slot } = ip.source else { return None };
    let kf = ip.desc.consumes.iter().position(|&s| s == slot).and_then(|i| ip.desc.consumes_from.get(i)).map(|l| l.0 as usize)?;
    // D3378: a kept sort run whose fill may raise (inner_once.rs `kept_run_lazy`) fills at the first outer row
    if gx.source_kind(ik) == crate::face::kinds::KindId::RunsSort && crate::rerun::inner_once::kept_run(gx, (k, j, ik)).is_none() && crate::rerun::inner_once::kept_run_lazy(gx, (k, j, ik)) {
        return kept_run_wait(px, k, kf, slot);
    }
    // D3897: a RIGHT / FULL drain's fill (INNER-SPOOL) runs at the first outer row when it may raise or calls
    if gx.source_kind(ik) == crate::face::kinds::KindId::JoinDrain && gx.spooled_inner(ik) {
        return drain_wait(px, k, ik, slot, ahead);
    }
    // rowsecurity#343 (readers.rs `copies`): the nest copies of a streamed write's arms share one fill
    let copies = crate::rerun::spool::readers::copies(gx, kf).unwrap_or_else(|| PgVec::new_in(gx.mcx));
    // errorhold (D1108 row 2): a licensed fill stays in pipeline order (deferred, a fill over a hashed aggregate
    // and a merge join tripped breaker.rs's sort-run shape check at the wait; its recheck licence repairs an early raise)
    if !matches!(crate::lifetime::material::spool_fill(gx, kf), Ok((r, false)) if r == ik || copies.contains(&ik)) || crate::rerun::spool::form::eager_licensed(gx, kf) {
        return None;
    }
    // D4569: a SELECT's union nest copies (readers.rs `union_nest`) read a calm fill run once in pipeline order ahead of
    // every arm: an arm that widens (ops/nestloop/widen.rs `spool`) reads the record from its first outer row on, so no arm
    // may defer the fill to its own first row
    if copies.len() > 1 && crate::rerun::spool::readers::union_nest(gx, k, j) && crate::lifetime::material::quiet_fill_licence(gx, kf).is_some() {
        return None;
    }
    // D3133: a fill that is a settled union's emit a rerun set refills whole (rerun/union_arms.rs `refilled_emit`, S2q) stays
    // in pipeline order: the set's run runs its feeds and then it, and the union's first feed (which empties the record)
    // writes no slot the fill reads, so the wait inside the set's run would not run it. Its arms are quiet, so a run whose
    // nest hands on no outer row shows nothing (D1107's eager refill), or may raise only under the unchecked licence
    // (union_arms.rs `refill_arms`, D3373: an error from rows PostgreSQL never reads is the tagged one)
    // D3749 (effects/visible/material.rs `pulls_loud_cte`): a fill reading a CTE whose run locks, calls or may raise goes lazy whatever else
    // holds, or the statement refuses: in pipeline order it would run that CTE ahead of the nest's outer
    let pulls = crate::effects::visible::material::pulls_loud_cte(gx, kf);
    if crate::rerun::union_arms::refilled_emit(gx, kf) {
        if pulls {
            crate::refuse!(ps[kf].node_id, crate::admit::refusal::Reason::Shape { why: crate::admit::refusal::ShapeWhy(crate::pipes::LOUD_CHILD) })
        }
        return None;
    }
    // D1108 row 3 (form.rs `deferred`): a fill that may raise or call VOLATILE code runs here at the first outer row, or
    // the statement refuses
    let loud = crate::rerun::spool::form::deferred(gx, kf);
    // D3373 (H-2): a nest in a recursive union's generation set whose Material fill runs once outside it (decompose F3's kept
    // Material over a hashed group) stays in pipeline order, as D3133's refilled emit does: the wait inside the set's run
    // does not run it. A quiet fill run before the union shows nothing PostgreSQL's first inner fetch would not
    let gen_set = |r: &gravity_decompose::graph::Rerun| r.plan_id == gravity_decompose::ops::shared::setop::RECURSIVE_SET && r.pipelines.contains(&(k as u16)) && !r.pipelines.contains(&(kf as u16));
    if !loud && !pulls && gx.graph.reruns.iter().any(gen_set) {
        return None;
    }
    if copies.first().is_some_and(|&c| c != ik) {
        return crate::rerun::spool::arm_fill::later(px, k, kf, slot, loud);
    }
    let readers: PgVec<'_, usize> = copies.iter().map(|&c| ps.inners()[c - ps.len()].0 .0).collect_in(px.mcx());
    match group(px, k, kf, slot, loud || pulls, &readers[..]) {
        // a reader that stops early (a cursor's FETCH n) would see the whole run PostgreSQL makes row by row; not the held
        // form's (form.rs `held`, D1108): its reader raises the fill's error only at the record the fill stopped at; nor the
        // first-pull form's (form.rs `first_pulled`), whose child PostgreSQL runs whole at its first fetch, nor the
        // first-sort form's (`first_sorted`, D4102), whose Sort PostgreSQL reads whole there; nor the first-run form's
        // (form.rs `first_ran`, D3803), whose per-row work is quiet
        Some(consumed) if loud => {
            if crate::rerun::spool::form::held(gx, kf).is_none() && !crate::rerun::spool::form::first_pulled(gx, kf) && !crate::rerun::spool::form::first_sorted(gx, kf) && !crate::rerun::spool::form::first_ran(gx, kf) {
                px.whole_read(k);
            }
            Some(Lazy { consumed })
        }
        Some(consumed) => Some(Lazy { consumed }),
        // `DECISIONS.md` 4 (D) b: a fill of that kind the wait cannot defer runs ahead in pipeline order, unchecked, where
        // nothing in it acts or calls user code (`Px::ahead_unchecked`, tagged; its licence in `ahead`); else D1108
        None if loud => {
            let Some(l) = px.ahead_unchecked(kf, gravity_decompose::effects::Effects::RAISES) else {
                crate::refuse!(ps[kf].node_id, crate::admit::refusal::Reason::Shape { why: crate::admit::refusal::ShapeWhy(crate::pipes::LOUD_CHILD) })
            };
            *ahead = Some(l);
            None
        }
        None if pulls => {
            crate::refuse!(ps[kf].node_id, crate::admit::refusal::Reason::Shape { why: crate::admit::refusal::ShapeWhy(crate::pipes::LOUD_CHILD) })
        }
        None => None,
    }
}

/// D3749: the pipelines the nests of graph pipeline `q` past op `j` hold for their first outer row whatever else holds:
/// each plan Material fill one of them reads that reads a CTE whose run locks, calls or may raise (form.rs
/// `pulls_loud_cte`; `declare` defers it or refuses), with its feeders. hashjoin's lazy build at op `j` (build_class.rs
/// `lazy_group_by`) skips them as pipelines that run between it and `q`: they run after it, at a later op's first row.
pub(crate) fn waits_after<'g>(gx: &crate::graph::Gx<'g, '_>, q: usize, j: usize) -> PgVec<'g, usize> {
    let ps = gx.pipelines();
    let mut out = PgVec::new_in(gx.mcx);
    if q >= ps.len() {
        return out;
    }
    for jn in j + 1..ps[q].ops.len() {
        let Some(kf) = fill_of(gx, q, jn).filter(|&kf| crate::effects::visible::material::pulls_loud_cte(gx, kf)) else { continue };
        let src = if let Source::Breaker { slot } = ps[kf].source { Some(slot) } else { None };
        out.push_in(kf);
        out.extend_in(crate::ops::hashjoin::build_class::feed(gx, kf, ps[kf].desc.consumes.iter().copied().chain(src)));
    }
    out
}

/// The fill pipeline of the plan Material the nest at op `j` of graph pipeline `k` reads as its one reader in the eager
/// form (`spool_fill`'s `(inner, false)`), as `declare` finds it.
fn fill_of(gx: &crate::graph::Gx<'_, '_>, k: usize, j: usize) -> Option<usize> {
    let ps = gx.pipelines();
    if !matches!(ps[k].ops.get(j), Some(Op::Join(JoinDesc::Nest { .. }))) {
        return None;
    }
    let ik = ps.inner_of(k, j)?;
    let ip = &ps[ik];
    let Source::Breaker { slot } = ip.source else { return None };
    let kf = ip.desc.consumes.iter().position(|&s| s == slot).and_then(|i| ip.desc.consumes_from.get(i)).map(|l| l.0 as usize)?;
    matches!(crate::lifetime::material::spool_fill(gx, kf), Ok((r, false)) if r == ik).then_some(kf)
}

/// D3378: the wait of the nest at graph pipeline `k` over the kept sort run in `slot` that fill `kf` makes (inner_once.rs
/// `kept_run_lazy`): the fill and its producers deferred to the first outer row and read whole there (`group`, as a loud
/// Material fill is); a group the wait cannot defer refuses by name.
fn kept_run_wait(px: &mut Px<'_, '_>, k: usize, kf: usize, slot: u8) -> Option<Lazy> {
    let ps = px.gx.pipelines();
    match group(px, k, kf, slot, true, &[]) {
        Some(consumed) => {
            px.whole_read(k);
            Some(Lazy { consumed })
        }
        None => crate::refuse!(ps[kf].node_id, crate::admit::refusal::Reason::Shape { why: crate::admit::refusal::ShapeWhy(KEPT_RUN_LOUD) }),
    }
}

/// D3378: the Shape of a kept sort run whose fill may raise and whose fill group the nest's first-row wait cannot defer.
pub(crate) const KEPT_RUN_LOUD: &str = "a nest inner reading a sort run kept across re-opens whose fill may raise, filled ahead of the owner where PostgreSQL sorts at the nest's first rescan (D3378)";

/// D3897: the wait of the nest at graph pipeline `k` whose inner `ik` is a RIGHT / FULL drain (INNER-SPOOL, drain/lower.rs
/// `inner_spooled`): the drain emits the build in `slot` and reads the spool its probe fills (the Drain op's `matches`).
/// PostgreSQL runs the inner Hash Right / Full Join at the nest's first rescan, its Hash built there and kept across the
/// rescans whose parameters it does not read (ExecReScanHashJoin), and never under an outer that hands on no row. In
/// pipeline order the fill ran ahead of the owner: with no rerun set the probe and what feeds it, the build among them;
/// with the nest's set re-running the probe at every re-open (D3724), the build kept outside it (D2592) and its feeders.
/// The probe and what feeds it passed INNER-SPOOL's own N1 reading (drain/spool_admit.rs: D393's quiet or D1902's side
/// through the probe), which leaves out the build it probes; a build that PostgreSQL 18.6 may see raise or that calls
/// (build_class.rs `side_by`'s PG-quiet reading of each pipeline writing its slot in this life, with its feeders and
/// its hash keys) makes the fill run whole at the first outer row instead (`lazy_group_drain` / `lazy_group_by`, the
/// wait over its slot); one the wait cannot defer runs ahead unchecked (`Px::ahead_unchecked`, tagged; its licence put
/// in `ahead`), else refuses by name.
fn drain_wait(px: &mut Px<'_, '_>, k: usize, ik: usize, slot: u8, ahead: &mut Option<gravity_decompose::effects::Licence>) -> Option<Lazy> {
    use crate::ops::hashjoin::build_class::{feed, lazy_group_by, lazy_group_drain, side_by, Side};
    let gx = px.gx;
    let ps = gx.pipelines();
    let m = ps[ik].ops.iter().find_map(|o| if let Op::Join(JoinDesc::Drain { matches, .. }) = o { Some(*matches) } else { None })?;
    let graph = |s: u8| gx.life_of(ik, s).map(|l| l.0 as usize).filter(|&q| q < ps.len());
    let (kp, kb) = (graph(m)?, graph(slot)?);
    let reads = |c: usize, x: usize| ps[c].desc.consumes.iter().any(|&s| gx.life_of(c, s).is_some_and(|l| l.0 as usize == x));
    let (fill, wait) = match crate::rerun::sets::owner_set_at(gx, ik) {
        None => (kp, m),
        Some(r) => {
            let member = |x: usize| gx.graph.reruns[r].pipelines.contains(&(x as u16));
            if !member(kp) || crate::rerun::sets::in_owner_set(gx, kb) || !(0..ps.len()).all(|c| c == k || member(c) || !reads(c, kb)) {
                return None;
            }
            (kb, slot)
        }
    };
    // every pipeline writing the build's slot in this life (a build that is itself a RIGHT / FULL join's output has
    // two: that join's probe and its drain), back to the slot's previous reader
    let writes = |q: usize| ps[q].desc.produces() == Some(slot);
    let from = (0..kb).rev().find(|&q| !writes(q) && reads_slot(ps, q, slot)).map_or(0, |q| q + 1);
    if (from..=kb).filter(|&q| writes(q)).all(|q| side_by(gx, q, usize::MAX, false) == Side::Safe) {
        return None;
    }
    let src = if let Source::Breaker { slot } = ps[k].source { Some(slot) } else { None };
    let first = ps[k].desc.consumes.iter().copied().filter(|&s| s != slot && s != m).chain(src);
    let before = feed(gx, k, first);
    // as D3378's: a raise a pattern compiled at bind makes would come under an empty outer too (`raises_at_rows`)
    let fsrc = if let Source::Breaker { slot } = ps[fill].source { Some(slot) } else { None };
    let at_rows = feed(gx, fill, ps[fill].desc.consumes.iter().copied().chain(fsrc)).into_iter().chain([fill]).all(|x| crate::effects::raises_at_rows_joined(gx, x));
    let readers: PgVec<'_, usize> = (0..ps.len()).filter(|&c| c == k || reads(c, fill)).collect_in(px.mcx());
    let group = px.consumed(k, wait).filter(|_| at_rows).and_then(|consumed| {
        let g = if fill == kp { lazy_group_drain(px, fill, k, consumed, wait, &before) } else { lazy_group_by(px, fill, k, &readers[..], consumed, wait, &before) };
        g.map(|g| (consumed, g))
    });
    match group {
        Some((consumed, group)) => {
            group.into_iter().for_each(|x| px.lazy.push((x, k))); // alloc: Program
            Some(Lazy { consumed })
        }
        None => {
            let Some(l) = px.ahead_unchecked(fill, gravity_decompose::effects::Effects::RAISES) else {
                crate::refuse!(ps[fill].node_id, crate::admit::refusal::Reason::Shape { why: crate::admit::refusal::ShapeWhy(DRAIN_LOUD) })
            };
            *ahead = Some(l);
            None
        }
    }
}

/// Pipeline `q` reads `slot`: a consumed region or its breaker source.
fn reads_slot(ps: &crate::pipes::Pipes<'_>, q: usize, slot: u8) -> bool {
    ps[q].desc.consumes.contains(&slot) || matches!(ps[q].source, Source::Breaker { slot: s } if s == slot)
}

/// D3897: the Shape of a nest inner's spooled RIGHT / FULL drain whose fill may raise or calls and whose fill group the
/// nest's first-row wait cannot defer, where the unchecked licence is denied.
pub(crate) const DRAIN_LOUD: &str = "a nest inner reading a RIGHT / FULL hash join's spool whose probe or build may raise or calls, filled ahead of the owner where PostgreSQL runs the join only at the nest's first rescan (D3897)";

/// `declare`'s group for fill `kf` of the Material in `slot` read by the nest of pipeline `k`, pushed onto `Px::lazy`: the
/// slot's index among `k`'s consumed regions. `None`: the fill runs in pipeline order. `loud`: a `deferred` fill, which no
/// eager form may run, goes lazy over parallel-eligible pipelines too (run at width 1 on the leader, as D824's builds are).
fn group(px: &mut Px<'_, '_>, k: usize, kf: usize, slot: u8, loud: bool, readers: &[usize]) -> Option<usize> {
    let gx = px.gx;
    let ps = gx.pipelines();
    let consumed = px.consumed(k, slot)?;
    // the outer's feeders (its breaker source, the other slots the nest's call reads) PG runs before the Material's
    // first fetch: not in the group
    let src = if let Source::Breaker { slot } = ps[k].source { Some(slot) } else { None };
    let first = ps[k].desc.consumes.iter().copied().filter(|&s| s != slot).chain(src);
    let before = crate::ops::hashjoin::build_class::feed(gx, k, first);
    // rowsecurity#343: every arm's nest copy reads the fill (`readers`, the first arm's `k` among them)
    let one = [k];
    let group = crate::ops::hashjoin::build_class::lazy_group_by(px, kf, k, if readers.is_empty() { &one } else { readers }, consumed, slot, &before)?;
    // a deferred pipeline runs on the leader at the wait, so only fills no worker would share
    // go lazy: every pipeline of the group serial already (not parallel-eligible, or driving a nest, whose step is serial)
    let serial = |x: usize| !ps[x].desc.is_eligible() || ps[x].ops.iter().any(|o| matches!(o, Op::Join(JoinDesc::Nest { .. })));
    if !loud && !group.iter().all(|&x| serial(x)) {
        return None;
    }
    // a union's arms writing one slot (a settled union filling the Material) go lazy too: the driver's wait runs each
    // earlier arm, whose renumbered slot no pipeline reads, with the next deferred pipeline (entry.rs `feed_mate`, D2652)
    group.into_iter().for_each(|x| px.lazy.push((x, k))); // alloc: Program
    Some(consumed)
}

impl Lazy {
    /// A wait on the Material slot at `consumed` among the nest pipeline's consumed regions (arm_fill.rs).
    pub(crate) fn new(consumed: usize) -> Self {
        Lazy { consumed }
    }
}

/// The offset word's tag while the fill runs (probe_wait.rs's `WAIT`): the row re-enters the nest step, which clears it.
const WAIT: i64 = 1 << 62;

impl Lazy {
    /// At the nest step's entry, before its parameters and its fresh test: while the fill is pending the outer row stops
    /// the call (`BUILD_WAIT`, the offset word tagged) and re-enters here once it ran, the tag cleared (probe_wait.rs
    /// `wait`'s protocol). The stop leaves through the frame's plain stop exit, not the window's carry exit: the fill is
    /// pending only until the first outer row of the execution reaches the nest, so no row has passed the nest and the
    /// end's carried accumulators still hold what its state held (the resumed call reloads them from the state, which
    /// D2147 keeps in frame words). Leaving the carry exit unreached keeps the row loop's registers as without the wait
    /// (through it, the exit's accumulator parameters cost the inner loop spills: x1.75 per read of a 90k-row spool).
    pub(crate) fn wait(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, offset: i32) {
        let fl = b.load_zext(Type::I32, env.ctx, hdr::FLAGS as i32);
        let bit = b.iconst(Type::I64, 1i64 << (FLAG_PENDING_SHIFT as usize + self.consumed));
        let m = b.int_bin(IntBinOp::And, fl, bit);
        let pending = b.icmp(IntCond::Ne, m, env.zero);
        let (l_wait, l_go) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(pending, (l_wait, &[]), (l_go, &[]));
        b.switch_to(l_wait);
        b.mark_cold(l_wait);
        let tag = b.iconst(Type::I64, WAIT);
        env.store_word(b, offset, tag);
        let st = b.iconst(Type::I64, build_wait(self.consumed as u8));
        b.br(env.x.l_stop_in, &[row.carry.out, row.at, st]);
        b.switch_to(l_go);
        let off = env.load_word(b, offset);
        let tag = b.iconst(Type::I64, WAIT);
        let tagged = b.icmp(IntCond::Eq, off, tag);
        let off = b.select(tagged, env.zero, off);
        env.store_word(b, offset, off);
    }
}
