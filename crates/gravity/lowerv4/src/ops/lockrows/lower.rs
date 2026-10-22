//! Kind `write.lock` (cards/M4.md `write/lockrows.rs`; M4-plan.md §3.1, inventory rows I29 I30 I31): PostgreSQL's
//! nodeLockRows as a step. Owner M4-LOCK. Per row, the row's locator (H28, `Env::loc`, which the frame sets because this
//! step `wants_locator`, L0-f) is stored into the lock site's position. Then ONE host call, `lock_row(ctx, site)`
//! (kernels host_abi v4 m4.rs), reaches `Program.lock` and the `hosts::lock::LockHost` the driver derives on the leader
//! from `Descs::write` (`WritePlan::locking`: mode FOR UPDATE / NO KEY UPDATE / SHARE / KEY SHARE x wait / NOWAIT / SKIP
//! LOCKED, as `LockRecipe::from_plan`). The host's status, as ExecLockRows maps `table_tuple_lock`'s TM_Result:
//! - `WRITE_DONE`: locked; the row goes on to the steps above (the LockRows projection) and the sink.
//! - `WRITE_SKIPPED`: deleted concurrently, SKIP LOCKED, or modified by this command (TM_SelfModified): the next row.
//! - `WRITE_RECHECK`: READ COMMITTED and a newer version was locked (`tmfd.traversed`; the host's `recheck_pos`). The
//!   rows already in the out block are pushed first, so the client sees them in row order. The call then stops with
//!   `recheck(source site)`, cursor past the row. The runner runs EvalPlanQual (exec epq.rs: a second node, the source
//!   site substituted; the filters, and an index source's keys, run again) and sends the surviving row, then resumes.
//! - below zero: the host's error (NOWAIT's 55P03, a serialization failure under a transaction snapshot, a deadlock, a
//!   cancel or timeout while waiting), leaving with the rows finished before it (ENTRY §E3).
//!
//! Served (L1): a LockRows over its own locked relation's heap scan (range / index / bitmap / tid), with only filters,
//! gates, projections or subplans below the lock, only its projection and a LIMIT / OFFSET above it, and the serial
//! out-block sink as the end. There the runner's EPQ is PG's exactly: one site, substituted. Under a LIMIT / OFFSET the
//! recheck runs in the IR instead (V4R-316, L0-h: write/lock_epq.rs), so the steps above count the rechecked row; the
//! rows OFFSET skips are locked (the lock is below the Limit, as ExecLockRows is). Refused by name:
//! - any op but the projection and a LIMIT / OFFSET above a LockRows that is not over its own relation alone (D3379
//!   OPABOVE: over its own relation alone the recheck runs in the IR, so the ops above take the rechecked row); a LIMIT /
//!   OFFSET over a join (the runner's EPQ reruns its builds, D655) or over a SubPlan op (EPQ resume, L0-h); the in-IR
//!   recheck (L0-h, V4R-512) over a tid scan, whose recheck is PG's TidRecheck / TidRangeRecheck (not built in the IR:
//!   D1672);
//! - a lock over a join, a subquery or any source other than the locked relation's own heap scan (EPQ over several
//!   sites: a keyed or sorted input holds no position); several locking marks, one on a relation another pipeline
//!   scans, or a lock over a breaker (a Sort below LockRows: its row is its junk ctid, CNM-38) are ops/lockrows/marks.rs's
//!   (W5 multilock Row A);
//! - a lock beside another pipeline's product (a build; an InitPlan crate lock_init.rs does not admit, V4R-527): the
//!   leader's recheck reruns only a marked relation's producers (exec epq::recheck_leader, D610, D655) (D652);
//! - a lock pipeline whose end is not the serial out-block sink, rechecked by the runner (it sends the row to the client).
//!
//! Served (L2, V4R-248): a LockRows over a partitioned or inheritance parent. The planner marks the top parent (isParent)
//! and each unpruned child (prti = the top parent); ExecLockRows skips the parent marks and locks each row in its own
//! child's relation. decompose binds the parent mark and makes the family one lock (`LockDesc::family`; the first leaf is
//! the plan's relation), and streams the Append into the lock: one arm per leaf, each its own pipeline over the leaf's
//! scan. Each arm is admitted as L1 against its own leaf's mark and stores that leaf in the position (H-h's LockHost
//! locks there, so NOWAIT's 55P03 names the leaf); the arms share the first arm's lock site and the one plan. EPQ stops
//! with the arm's source site and D610's leader recheck reruns that arm alone: PostgreSQL's EvalPlanQual, whose other
//! children's marks sit out. The planner's single leaf (the Append elided) is the same lock over one scan. lockrows
//! FAMJOIN: a family beside a join's REFERENCE marks (`LockDesc::family_top`, FOR UPDATE OF the family) whose Append is the
//! probe side of an inner hash join streams so too (decompose append.rs `owner`): each arm is this lock over a join
//! (D655 below), the recheck rerunning the other relations' builds over their rows as read, then the arm alone. Refused by
//! name: a family beside another relation's COPY mark or a locking one (FAMILY_JOIN), EXPLAIN ANALYZE over the arms (the
//! LockRows node's loops per arm), and the L1 forms. A family's rows read back from a breaker (a sort below the lock,
//! a MergeAppend, a union kept in its Materialize form) are marks.rs's (D2604: `family_breaker`); under a LIMIT / OFFSET
//! over a Sort of the family's Append the recheck runs in the IR over the arm the row's tableoid names (FAMLIMIT).
//!
//! Served (D655, V4R-256): a lock over inner joins whose other relations are REFERENCE-marked (FOR UPDATE OF one relation):
//! the other relations' junk ctids are stored as their scan sites' positions and the recheck reruns their builds (marks.rs).
//! Those reruns pass rerun.rs's checks, as marks.rs's form does (lockrows L1-RERUN: PG's outer-first hash join builds no
//! VOLATILE side when the newest version fails its quals; refused by name RERUN_FIRST / BUILD_FIRST).
//! lockrows NEST: an INNER nested loop's inner among them (one heap scan under filters and projections, marks.rs
//! `nest_joins`): its REFERENCE row is stored in a RowOf site of the inner's scan site, and the recheck opens the inner
//! once over it (the outer, the locked relation's own scan, substituted). lockrows REFFETCH (main 10-04): under a LIMIT /
//! OFFSET over a Sort of an inner hash join whose build is the REFERENCE-marked relation's own quiet scan, the recheck runs
//! in the IR instead (`ref_limit`): the lock host refetches that row at its junk ctid (EvalPlanQualFetchRowMark), so the
//! steps above count the rechecked row; no build is kept for it.
//!
//! Served (V4R-512, D655's / D1496's lock halves): an L1 lock whose pipeline makes another's product (an Agg, as the RI
//! checks; a hash build; a CTE's store), its recheck its own relation's alone (one mark; only filters, gates, projections
//! below; any op above): the recheck runs in the IR (write/epq_ir.rs), so only the newer version reaches the product, as
//! ExecLockRows hands its EPQ row to its parent; marks.rs is not asked, lock_init.rs's InitPlan checks are (D2112).
//! producerchain E: so is an L2 family's lock whose pipeline makes another's product (the temporal RI check over a
//! partitioned referenced table, its range_agg the product), each over its own leaf (`own_leaf`). On PG's outer-first hash join a lock is a VOLATILE
//! call's side (join/build_class.rs): its build lazy (D824) or refused by name; a locking CTE outer pulls a row first.
//!
//! lockrows REFNEST (gen queue-0015): under a LIMIT / OFFSET, or over a Memoize inner, a lock over an INNER nest whose inner
//! is the REFERENCE-marked relation's own scan rechecks in the IR: that row refetched at its junk ctid, the inner's redo
//! under the nest's parameters from the newest version, then the join qual (rerun/epq/ref_nest.rs). D3554 SORTNEST: or
//! that nest below a Sort (an ORDER BY past the index's order), the same redo over the Sort's producer (`sort_form`). D3560
//! REFNEST-LEFT: or a LEFT nest, the row null-extended where the refetch, the inner's redo or the join qual finds no match.
//!
//! D3379 LOCKNEST: REFNEST's mirror, under a LIMIT / OFFSET a lock on an INNER nest's inner relation (a Memoize
//! over it or not) beside its REFERENCE-marked outer, rechecks in the IR: the inner's redo over the newest version joined
//! to the outer row the nest holds, then the join qual (ref_nest.rs `lock_form`, epq_ir.rs `epq_of_inner`). D3780
//! LOCKCHAIN: or that nest over a join of REFERENCE relations (`chain_lock_form`; with no LIMIT / OFFSET, an outer the
//! runner's recheck does not take); D3802 SORTCHAIN: or LOCKCHAIN's chain below a Sort, the redo EvalPlanQual's rerun
//! whole from the producer's refetched source row (rerun/epq/sort_chain.rs). D3779 REFNEST-FEED: REFNEST's lock whose rows feed a product only later pipelines
//! read (ref_nest.rs `form`). D3379 LOCKPAIR: the same with the outer locked too, each mark its own lock over its row's test tuple (pair.rs); PAIRPROBE:
//! or that nest in decompose's catalog form (a hash build of the inner the outer probes, ref_nest.rs `limited_probe`);
//! D3727 PAIRHASH: or PostgreSQL's own Hash Join of the two, its sides' parts raise-free (either rerun order is PG's).
//! D3777 over PostgreSQL's Merge Join (each side its relation's scan in the clauses' order or a Sort over it; its
//! EvalPlanQual runs the outer first): PAIRMERGE, both locked (ref_nest.rs `limited_merge`); LOCKMERGE, the inner locked
//! beside a REFERENCE outer, REFBUILD's redo (`ref_merge`); MERGETAIL, the outer locked under a Sort beside a REFERENCE
//! inner, REFMERGE's refetch after the Sort's producer's part (`merge_tail`).
//!
//! Served (lockrows SUBLOCK, D1018 sub-shape E): a LockRows inside a nested SubPlan's inner (`EXISTS (.. FOR UPDATE)`),
//! its position the row's junk ctid and its recheck the IR's (rerun/epq/sub_lock.rs).
//!
//! Served (D717, V4R-255): a LockRows over the planner's dummy relation (a Result under a constant false / NULL one-time
//! gate: every leaf pruned, a constant-false qual), which no row reaches: no site, no write plan, the rows passed on.
//!
//! Leader-only: decompose makes the pipeline serial (`Reason::Lock`), and a parallel-eligible one is a Bug. Budget 300.
use gravity_abi::ctx::{site_off, POS_ROW, POS_VALID};
use gravity_abi::site::site;
use gravity_abi::status::{recheck, unsupported, UNSUPPORTED_SHAPE, WRITE_RECHECK, WRITE_SKIPPED};
use std::cell::Cell;
use std::rc::Rc;

use mcx::{CollectIn, GrowIn, PgBox, PgRc, PgVec};

use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::ops::shared::modifytable::{LockDesc, Member, RowMark};
use gravity_decompose::{Am, Op, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_program::program::LockSite;
use gravity_program::{LockKind, Parallel, SiteDesc};

use crate::ops::lockrows::plan::{Census, Form, Plan, RunnerAnswer};
use crate::face::end::{EndMech, SinkKind};
use crate::face::kinds::{Face, KindId};
use crate::face::step::StepMech;
use crate::frame::env::Env;
use crate::frame::frame::{Next, StepMaker};
use crate::frame::iface::limit_bounds;
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::admit::refusal::{Reason, ShapeWhy, Variant};

use crate::rerun::epq::epq_ir::Epq;
use crate::ops::lockrows::epq::epq_of;
use crate::effects::visible::lockrows::{calm_call, quiet_ref_build};
use gravity_decompose::catalog::typefacts::{self, TypeFacts};
use gravity_decompose::join::JoinDesc;
use types_nodes::jointype::JoinType;
use types_nodes::plannodes::RowMarkType;
use types_nodes::primnodes::OUTER_VAR;

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { kinds: &[(KindId::WriteLock, &StepMaker(make))], ..crate::ops::NONE };

/// The named refusals (module doc).
/// D3506 SETLOCK (rerun/epq/set_lock.rs): a lock whose SubPlan steps probe the sets of locks in hashed SubPlans' plans,
/// beside no write it pairs with (plan.rs `Plan::write_pair`).
pub(crate) const SET_UNPAIRED: ShapeWhy = ShapeWhy("a lock probing a hashed SubPlan whose plan locks, beside no write it pairs with (D3506)");
pub(crate) const LIMIT_ABOVE: ShapeWhy = ShapeWhy("a LIMIT / OFFSET above LockRows over a join (the runner's EPQ, L0-h)");
/// V4R-316: a SubPlan op below the lock, which the in-IR recheck would run again over the newer version.
const SUBPLAN_EPQ: ShapeWhy = ShapeWhy("a LIMIT / OFFSET above LockRows over a SubPlan (the recheck's subplan, L0-h)");
pub(crate) const OP_ABOVE: ShapeWhy = ShapeWhy("an op above LockRows other than its projection (EPQ resume, L0-h)");
pub(crate) const NOT_OWN: ShapeWhy = ShapeWhy("a row lock over a join, sort, subquery or non-heap source (EPQ over several sites)");
pub(crate) const NOT_SINK: ShapeWhy = ShapeWhy("a row lock whose rows do not go to the client (EPQ sends the rechecked row there)");
/// L2: a partitioned / inheritance family's lock beside another relation's mark (a join's REFERENCE / COPY marks, D655).
pub(crate) const FAMILY_JOIN: ShapeWhy = ShapeWhy("a row lock over a partitioned or inheritance parent beside another marked relation (D655)");
/// L2: a family's rows read back from a breaker, not from each leaf's scan: the union kept in its Materialize form (an
/// op the arms cannot copy), a MergeAppend, or a sort below the lock (EPQ over a leaf the breaker holds by position).
const FAMILY_BREAKER: ShapeWhy = ShapeWhy("a row lock over a parent's leaves read back from a breaker (a Materialize union, a MergeAppend or a sort)");
/// L2: every arm counts the LockRows node's loops, so EXPLAIN ANALYZE would read one loop per leaf.
const ANALYZE_ARMS: ShapeWhy = ShapeWhy("EXPLAIN ANALYZE of a row lock over a parent's leaves (the LockRows node's loops per arm)");

/// A site number, placed at the make or (a family's arms) at emit (`Px::place_late_sites`), read while lowering.
type Late = Rc<Cell<usize>>;

pub(crate) struct Lock<'g> {
    /// The lock site (`SiteDesc::write(relid, 0)`): its position is the row `lock_row` locks. A family's arms share the
    /// first arm's (the InitPlan pair's arms each their own), numbered late (every arm's source site first: exec admit).
    site: Late,
    /// The relation this pipeline's rows are locked in: the plan's, or (L2) the arm's own leaf, which H-h opens.
    relid: u32,
    node: i32,
    /// D655: the joined relations' marks, stored as their sites' positions before the host call (marks.rs).
    marks: crate::rerun::epq::marks::Marks<'g>,
    /// V4R-316 (L0-h): a LIMIT / OFFSET above: the recheck in the IR.
    epq: Option<PgRc<'g, Epq<'g>>>,
    /// V4R-527 (M4): the lock's `Program.locks` index + 1 (`LockSite::arg`), 0 for the program's one lock.
    mark: usize,
    /// lockrows 15b (plan.rs `breaker_limit`): the row's lock position is its junk ctid column (the Sort carries it), not a locator.
    junk: Option<(i16, TypeFacts)>,
    /// D3289: the runner-rechecked lock's re-entry word over a join below it (marks.rs `again`).
    again: Option<i32>,
    /// lockrows FAMLIMIT (`family_limit`): the family's tableoid junk column names the row's leaf (the position's relation)
    /// and the arm whose in-IR recheck runs, one per leaf.
    fam: Option<Fam<'g>>,
}

/// lockrows FAMLIMIT: the tableoid junk column, the oid type's facts, and each leaf's (relid, in-IR recheck over its arm).
struct Fam<'g> {
    col: i16,
    of: TypeFacts,
    arms: PgVec<'g, (u32, PgRc<'g, Epq<'g>>)>,
}

/// L2: the lock site a family's first arm claimed, by its LockRows node (write/rowop.rs's R2 form for the write's arms; one
/// write plan per program). None for the InitPlan pair (mark > 0), whose locks are `Program.locks` entries (`Px::locks`).
#[derive(Default)]
struct LockArms {
    node: i32,
    site: Late,
}
crate::frame::px::ext_g!(LockArms);

fn shape(node: i32, why: ShapeWhy) -> ! {
    crate::refuse!(node, Reason::Shape { why })
}

fn make<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let p = &px.gx.pipelines()[k];
    let node = p.node_ops.iter().rev().find(|n| n.first_op as usize <= j).map_or(p.node_id, |n| n.node_id);
    let Op::Lock(d) = &p.ops[j] else { crate::refuse!(node, Reason::Bug { payload: "a write.lock step over another op" }) };
    if matches!(p.desc.parallel, Parallel::Eligible) {
        crate::refuse!(node, Reason::Bug { payload: "a row lock on a parallel-eligible pipeline (decompose makes a lock serial)" });
    }
    let RunnerAnswer { mark, pair, sub_claim, product, subs, twin } = match Plan(px).form(k, j, node, d) {
        Form::NoRows => return mcx::pg_box_dyn!(px.mcx(), NoRows => dyn StepMech + 'g),
        Form::SetLock(site) => return set_lock(px, node, d, (k, j), site),
        Form::SubLock(s) => return sub_lock(px, node, d, (k, j), &s),
        Form::SortSubClaim { at, col } => return sort_sub_claim(px, node, d, at, col),
        Form::Breaker { at, col, fetch, mark } => return breaker_lock(px, node, d, at, col, fetch.rows(), mark),
        Form::Tail { at, col, fetch, mark } => return tail_lock(px, node, d, (at, (k, j)), col, fetch.rows(), mark),
        Form::RefNest { ctid, fetch, mark } => return ref_nest_lock(px, node, d, (k, j), (ctid, fetch.rows()), mark),
        Form::Probe { col, mark } => return probe_lock(px, node, d, (k, j), col, mark),
        Form::LockNest { at, ctid, mark } => return lock_nest_lock(px, node, d, (k, j), at, ctid, mark),
        Form::Pair(f) => return crate::ops::lockrows::pair::lock_pair(px, node, d, (k, j), f),
        Form::RefBuild(rb, mark) => return ref_build_lock(px, node, d, (k, j), &rb, mark),
        Form::Family { arms, ctid, toid, mark } => return family_lock(px, node, d, &arms, ctid, toid, mark),
        Form::SortChain(sc) => return sort_chain_lock(px, node, d, &sc),
        Form::Drain(rd, mark) => return drain_lock(px, node, d, &rd, mark),
        Form::Marks { fam } => return crate::ops::lockrows::marks::make(px, k, j, node, d, fam),
        Form::Runner(r) => r,
    };
    let marks = match product || subs {
        true => crate::rerun::epq::marks::Marks::none(px.mcx()),
        false => crate::rerun::epq::marks::sites_in(px, k, node, &d.marks, &|rti| d.marks.iter().any(|m| m.rti == rti && m.lock != LockKind::None), true),
    };
    let (relid, arm, limit) = admit(px, k, j, d, node, product, subs);
    // lockrows L1-RERUN (main 10-03 item 3): over a join the runner's recheck reruns each producer the REFERENCE / COPY
    // marks name before the stopped pipeline (D655), as marks.rs's form does, so the same checks: PG's outer-first hash
    // join never builds when the locked relation's newest version fails its quals, and a producer it may not run must be
    // quiet (RERUN_FIRST), its later build-first rechecks too (BUILD_FIRST); not NULL_SIDE (D2048's fix serves L1's)
    if !marks.pos.is_empty() {
        // D2033 step 2: and the first probe's build the recheck keeps in the statement run's order (`first_held`)
        let mut lazy = crate::ops::lockrows::rerun::epq_lazy(px, k);
        let held = crate::ops::lockrows::rerun::first_held(px, k, &d.marks, &lazy);
        lazy.extend_in(held.iter().copied());
        // D2033 step 3: and the epq probe's own held build (`epq_held`)
        let epq_held = crate::ops::lockrows::rerun::epq_held(px, k, &d.marks, &lazy);
        lazy.extend_in(epq_held);
        crate::ops::lockrows::rerun::admit_lazy(px.gx, k, node, &d.marks, (false, &[]), &lazy);
        px.recheck_hold.extend(held); // alloc: Program
        px.recheck_hold.extend(epq_held); // alloc: Program
        px.recheck_hold_epq.extend(epq_held); // alloc: Program
    }
    // lockrows D2112 (phase A): the IR's recheck beside an InitPlan asks lock_init.rs's checks too (marks.rs `sites` asks
    // them for the runner's): PG forces a pending one at the recheck and re-arms one attached inside a CTE's plan
    if product || subs {
        crate::ops::lockrows::lock_init::admit_ir(px, k, node);
    }
    // V4R-316 (L0-h): under a LIMIT / OFFSET the recheck runs in the IR (`Epq`); over a join the runner's (D655)
    let epq = (limit || product || subs).then(|| match subs {
        true => PgRc::new_in(crate::ops::lockrows::epq::epq_of_subs(px, k, j, node, relid).twinning(twin), px.mcx()),
        false => PgRc::new_in(epq_of(px, k, j, node, relid, limit), px.mcx()),
    });
    if arm && crate::frame::instr::on() {
        shape(node, ANALYZE_ARMS);
    }
    // D3289: the runner's recheck (no IR one) over a join below the lock may stop AT the row (marks.rs `recheck_exit`)
    let again = match epq {
        None => crate::ops::lockrows::marks::again(px, k, j),
        Some(_) => None,
    };
    // one write plan per program (`Px::write`): a lock beside a data-modifying CTE is a second one (L0-d); a family's
    // later arms share the first arm's site and plan
    if px.write_plan().is_some() && mark == 0 {
        let shared = px.ext::<LockArms>(Face::RowOp).filter(|c| arm && c.node == node && px.write_plan() == Some(&d.plan));
        let Some(c) = shared else { crate::not_built!(node, Variant::Kind(KindId::NestCte)) };
        return mcx::pg_box_dyn!(px.mcx(), Lock { site: c.site.clone(), relid, node, marks, epq, mark, junk: None, fam: None, again } => dyn StepMech + 'g);
    }
    // lockrows JOBQ: the statement's own write keeps the program's write plan (its claim may come before or after this make)
    if !pair && !sub_claim {
        px.keep_write(d.plan.copy_in(px.arena));
    }
    // D655: a lock over a join numbers its site late too (the other pipelines' source sites come first); V4R-527: so does
    // one beside an InitPlan (D1710's class: exec admit's host registry is positional over the source sites)
    // V4R-316: the recheck reads the newer version from the lock site's cells (RL1b), as wide as the widest leaf
    let natts = if epq.is_some() { d.marks.iter().filter_map(|m| px.gx.facts.rel(m.relid)).map(|r| r.natts.max(0) as u16).max().unwrap_or(0) } else { 0 };
    let init = px.gx.graph.init_plans.iter().any(|i| i.pipeline as usize != k && !matches!(i.link, gravity_decompose::ops::shared::subplan::SubLink::Cte));
    // D3276 (DRAINED LOCK): so does one beside drained CTEs (made first, their source sites numbered before its own);
    // D3379 LOCKPOST: or beside a post CTE (its pipelines made last, its source sites numbered before the lock's)
    let drained = !px.gx.after_emit().is_empty();
    let site = match arm || !marks.pos.is_empty() || product || subs || init || drained {
        true => px.add_late_site(SiteDesc::write(d.plan.target_rel, natts)),
        false => Rc::new(Cell::new(px.add_site(SiteDesc::write(relid, natts)))),
    };
    if arm && mark == 0 {
        *px.ext_mut::<LockArms>(Face::RowOp) = LockArms { node, site: site.clone() };
    }
    mcx::pg_box_dyn!(px.mcx(), Lock { site, relid, node, marks, epq, mark, junk: None, fam: None, again } => dyn StepMech + 'g)
}

/// D3563 (plan.rs `breaker_limit`, 15b alone): whether lock op `j` of pipeline `p` sits past the Sort emit's projection and a
/// Result's per-row SubPlan steps (one at least), filters and projections over the sorted row, as PostgreSQL's
/// Limit -> LockRows -> Result (its target list's SubPlan) -> Sort: EvalPlanQual reruns the Result's quals and
/// projection, each SubPlan with the newest version's arguments under the statement's snapshot (epq_ir.rs
/// `epq_of_tail`).
pub(crate) fn sort_tail(p: &gravity_decompose::PipelineNode<'_>, j: usize) -> bool {
    let ops = &p.ops[..j];
    ops.iter().any(|o| matches!(o, Op::SubPlan(_))) && ops.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_) | Op::SubPlan(_))) && ops.first().is_some_and(sort_emit)
}

/// Where plan.rs `breaker_limit`'s lock sits among the statement's pipelines: alone with its Sort's producer (15b), in a CTE's store
/// (CTESORT), in a non-CTE InitPlan's pipelines (D3277 JOBQ SORT) or below the SubqueryScan the statement's own write
/// carries in its COPY mark (D3379 JOBQ HASH).
#[derive(Clone, Copy)]
pub(crate) enum Beside {
    Alone,
    Cte,
    Init,
    Copy,
}

/// REFFETCH (plan.rs `breaker_limit` over a join): a lock over a Sort whose producer joins its one
/// locking mark's own heap scan (filters, gates and projections before the join) to one REFERENCE-marked relation's own
/// heap scan (filters, gates and projections before its build) by an INNER hash probe, then filters and projections up to
/// the sort build; no other pipeline, mark or InitPlan, and a LIMIT / OFFSET above the lock. PostgreSQL's EvalPlanQual
/// reruns Sort -> Hash Join over the test tuple and the REFERENCE row it refetches at the row's junk ctid (SnapshotAny: the
/// version the join read), and LockRows hands the result to the Limit. Here the recheck runs in the IR (epq_ir.rs `Fetch`):
/// the lock host fetches that version, the build side's quals and projection are redone over it and the probe's clauses
/// and join qual over the pair. The build side may not raise (PG's outer-first or build-first order then cannot show).
/// D3379 REFCALM: or its calls are `calm_call` ones: the refetched row is the version the join read, so its quals and
/// projection pass again as they passed, in either order.
/// lockrows REFFETCH direct (gen queue-0015 under a hash plan): or the lock in that probe pipeline itself, no Sort between
/// (the probe side's order is the rows'), only projections and a LIMIT / OFFSET above it: the producer is the lock's own
/// pipeline, its ops before the lock redone. D3452 REFINIT: either form with no LIMIT / OFFSET beside InitPlans that lock
/// and write nothing (m8_d652_lock_initplan m13: `.. FROM w6q q JOIN w6r r .. WHERE r.c < (SELECT max(k) .. WHERE
/// d6_bump(k)) FOR SHARE OF r`): this recheck copies their values, where the runner's would run them again. D3779
/// (REFNEST-FEED's hash plan): or direct with no LIMIT / OFFSET, its rows feeding a product only the later pipelines
/// read (`above_only`), the probe INNER or LEFT. Some((producer, its ops before the sort build, the lock's junk ctid column, (build pipeline, the REFERENCE mark's junk
/// ctid column, its relation))), else none.
pub(crate) fn ref_limit(gx: &crate::graph::Gx<'_, '_>, k: usize, j: usize, d: &LockDesc<'_>) -> Option<(usize, usize, i16, (usize, i16, u32))> {
    let ps = gx.pipelines();
    let p = &ps[k];
    // lockrows REFFETCH direct (gen queue-0015 under a hash plan): the lock in the probe pipeline itself, over the locked
    // relation's own scan (no Sort: its order the probe side's), only projections and a LIMIT / OFFSET above it
    let slot = match p.source {
        Source::Breaker { slot } => Some(slot),
        _ => None,
    };
    // D3779 REFNEST-FEED's form (TypeORM's paginated lock under a hash plan: Subquery Scan -> LockRows -> Hash Left Join,
    // the lock's rows feeding the DISTINCT): or, direct, rows feeding a product only the later pipelines read
    // (`above_only`), filters, gates, projections and a LIMIT / OFFSET above the lock, then the product's sink
    let feed = match (slot, p.desc.produces()) {
        (None, Some(_)) => Census(gx).above_only(k).filter(|&a| a > 0)?,
        _ => 0,
    };
    let plain_op = |o: &Op<'_>| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_));
    let above = match slot {
        Some(_) => p.ops[..j].iter().all(sort_emit),
        None if feed > 0 => j + 2 <= p.ops.len() && p.ops[j + 1..p.ops.len() - 1].iter().all(|o| plain_op(o) || limit_op(o)) && !matches!(p.ops.last(), Some(Op::Write(_) | Op::Lock(_))),
        None => p.ops[j + 1..].iter().all(|o| matches!(o, Op::Project(_)) || limit_op(o)),
    };
    // AGA-35: beside InitPlans that lock and write nothing, their pipelines counted out (`Census::plain_inits`); the
    // producer and the build pipeline are checked outside them below, and plan.rs `Plan::form` asks `admit_ir_ref` of the producer's part
    // and `admit_ir_at` of the build part
    let own = if slot.is_some() { 3 } else { 2 };
    let inits = Census(gx).inits(k, k);
    // D3452 REFINIT: or no LIMIT / OFFSET above the lock beside such InitPlans (one or more): without them the runner's
    // recheck serves the join (L1-RERUN); beside them it would rerun first an InitPlan's producers, which PostgreSQL's
    // EvalPlanQual never runs again (RERUN_FIRST: EvalPlanQualStart copies the value), where this recheck copies it too
    let limited = p.ops[j + 1..].iter().any(limit_op) || inits.is_some_and(|n| n > 0) || feed > 0;
    if !above || !limited || d.family_top().is_some() || !inits.is_some_and(|n| Census(gx).whole(own + n + feed)) || d.marks.len() != 2 {
        return None;
    }
    let m = d.marks.iter().find(|m| m.lock != LockKind::None)?;
    let r = d.marks.iter().find(|m| m.lock == LockKind::None)?;
    if m.parent || m.ctid_col <= 0 || m.relid != d.plan.target_rel || r.parent || r.ctid_col <= 0 || r.mark != RowMarkType::ROW_MARK_REFERENCE || r.rti == m.rti {
        return None;
    }
    let heap = |s: &Source<'_>, rti: u32, relid: u32| match s {
        Source::Range { am: Am::Heap, scanrelid, relid: x, .. } | Source::Bitmap { am: Am::Heap, scanrelid, relid: x, .. } | Source::Index { am: Am::Heap, scanrelid, relid: x, only: false, .. } => u32::from(*scanrelid) == rti && *x == relid,
        _ => false,
    };
    let plain = |o: &Op<'_>| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_));
    let q = match slot {
        Some(slot) => (0..k).rev().find(|&x| ps[x].desc.produces() == Some(slot))?,
        None => k,
    };
    let pq = &ps[q];
    let jq = if slot.is_some() { pq.ops.iter().position(|o| matches!(o, Op::Sort(_))).unwrap_or(pq.ops.len()) } else { j };
    let at = pq.ops[..jq].iter().position(|o| !plain(o))?;
    // lockrows REFMERGE (lock-0009): direct, or an INNER / SEMI merge join whose inner (spooled) is the REFERENCE relation's
    // own quiet scan: PostgreSQL's EvalPlanQual reruns the merge over the test tuple and that row refetched
    let (bs, merge) = match &pq.ops[at] {
        Op::Join(JoinDesc::Probe { slot: bs, jointype: JoinType::JOIN_INNER, catalog: false, .. }) => (bs, false),
        // D3779: or, direct and feeding a product, a LEFT one (D3596's null-extended refetch, epq_ir.rs `Fetch::left`)
        Op::Join(JoinDesc::Probe { slot: bs, jointype: JoinType::JOIN_LEFT, catalog: false, .. }) if slot.is_none() && feed > 0 => (bs, false),
        Op::Join(JoinDesc::Merge { slot: bs, jointype: JoinType::JOIN_INNER | JoinType::JOIN_SEMI, .. }) if slot.is_none() => (bs, true),
        _ => return None,
    };
    let after = pq.ops[at + 1..jq].iter().all(plain) && matches!(pq.ops[..jq].last(), Some(Op::Project(_))) && (slot.is_none() || pq.ops[jq..].iter().all(|o| matches!(o, Op::Sort(_))));
    if !heap(&pq.source, m.rti, m.relid) || !after || pq.desc.consumes.len() != 1 {
        return None;
    }
    // D3379 INITCTE: the build slot's producer for this read (`life_of`: an InitPlan's or a CTE's pipeline may make the
    // same slot number in an earlier life)
    let bq = gx.life_of(q, *bs).map(|l| l.0 as usize)?;
    let pb = &ps[bq];
    let below = match (merge, pb.ops.last()) {
        (false, Some(Op::Join(JoinDesc::Build { .. }))) => &pb.ops[..pb.ops.len() - 1],
        (true, Some(Op::Project(_))) => &pb.ops[..],
        _ => return None,
    };
    let quiet = quiet_ref_build(gx, pb, below);
    let outside = !Census(gx).init_member(q) && !Census(gx).init_member(bq);
    (heap(&pb.source, r.rti, r.relid) && quiet && outside && pb.desc.consumes.is_empty()).then_some((q, jq, m.ctid_col, (bq, r.ctid_col, r.relid)))
}

/// D3569 REFCOPY (rerun/epq/ref_copy.rs: Odoo's cron acquisition): the lock at op `j` of pipeline `k` over a Sort,
/// under projections and a LIMIT / OFFSET or none, whose producer joins the locking mark's own heap scan (filters,
/// gates and projections around the join) to the COPY mark's subquery row by one INNER or LEFT nest
/// (`ref_copy::inner`); the statement's other pipelines the producers below the subquery's entry (no recheck reruns
/// them: the stored row enters there), none writing or locking, and InitPlans that lock and write nothing. The runner's
/// recheck takes a lock's COPY row in a nest inner only over VALUES (marks.rs `lock_values`), so a bound is not needed
/// here. Some((producer, its ops before the sort build, the lock's junk ctid column, the fetch: the inner, the COPY
/// mark's whole-row column, `COPY_ROW`)), else none.
pub(crate) fn ref_copy(gx: &crate::graph::Gx<'_, '_>, k: usize, j: usize, d: &LockDesc<'_>) -> Option<(usize, usize, i16, (usize, i16, u32))> {
    let ps = gx.pipelines();
    let p = &ps[k];
    let Source::Breaker { slot } = p.source else { return None };
    if !p.ops[..j].iter().all(sort_emit) || !p.ops[j + 1..].iter().all(|o| matches!(o, Op::Project(_)) || limit_op(o)) {
        return None;
    }
    let (m, c) = crate::rerun::epq::ref_copy::marks(d)?;
    let q = (0..k).rev().find(|&x| ps[x].desc.produces() == Some(slot))?;
    let pq = &ps[q];
    let jq = pq.ops.iter().position(|o| matches!(o, Op::Sort(_)))?;
    let plain = |o: &Op<'_>| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_));
    let jn = pq.ops[..jq].iter().position(|o| !plain(o))?;
    let ok = scan_rel(&pq.source) == Some((m.rti, m.relid)) && pq.ops[jn + 1..jq].iter().all(plain) && matches!(pq.ops[..jq].last(), Some(Op::Project(_))) && pq.ops[jq..].iter().all(|o| matches!(o, Op::Sort(_)));
    if !ok || Census(gx).init_member(q) {
        return None;
    }
    let (x, at, _) = crate::rerun::epq::ref_copy::inner(gx, q, jn, c)?;
    let up = crate::rerun::epq::copy_row::upstream(gx, x, at);
    let quiet = up.iter().all(|&u| u < ps.len() && !Census(gx).init_member(u) && !ps[u].ops.iter().any(|o| matches!(o, Op::Write(_) | Op::Lock(_))));
    let fed = pq.desc.consumes.iter().all(|&s| up.iter().any(|&u| ps[u].desc.produces() == Some(s)));
    let n = Census(gx).inits(k, q)?;
    (quiet && fed && Census(gx).whole(2 + n + up.len())).then_some((q, jq, m.ctid_col, (x, c.ctid_col, crate::rerun::epq::ref_copy::COPY_ROW)))
}

/// D3567 REFCHAIN (Airflow's executable task instances: `SELECT task_instance.* FROM task_instance JOIN dag_run ON ..
/// JOIN dag ON .. WHERE .. ORDER BY -task_instance.priority_weight, dag_run.execution_date, task_instance.map_index
/// LIMIT 32 FOR UPDATE OF task_instance SKIP LOCKED`): Limit -> LockRows -> Sort -> Nested Loop (Hash Join (the locked
/// relation's scan; Hash <- a REFERENCE relation's scan); a REFERENCE relation's parameterized scan). PostgreSQL's
/// EvalPlanQual reruns the Sort's subtree over the test tuple and every REFERENCE row it refetches at the row's junk
/// ctids (SnapshotAny: the versions the joins read), and LockRows hands the result to the Limit. Here REFFETCH's and
/// SORTNEST's recheck over a chain: the Sort's producer over the locking mark's own heap scan, filters, gates and
/// projections around two or more INNER joins, each a hash probe whose build is a REFERENCE mark's own quiet scan
/// (`ref_limit`'s build, `quiet_ref_build`) or a nest whose inner is one's own scan (`ref_nest::ref_side`), each
/// REFERENCE mark one join's (D3596: or LEFT joins, an ORM's select_related over a nullable relation sorted off its
/// index: a refetch finding no row or failing the clauses null-extends the row, epq_ir.rs `Fetch::left`); the lock's
/// pipeline the Sort emit's projection, projections and a LIMIT / OFFSET above;
/// the statement's other pipelines the builds' and those of InitPlans that lock and write nothing. Some((producer, its
/// ops before the sort build, the lock's junk ctid column, each join's fetch in op order: (the REFERENCE part's
/// pipeline, its mark's junk ctid column, its relation))), else none.
pub(crate) fn ref_chain<'g>(gx: &crate::graph::Gx<'g, '_>, k: usize, j: usize, d: &LockDesc<'_>) -> Option<(usize, usize, i16, PgVec<'g, (usize, i16, u32)>)> {
    let ps = gx.pipelines();
    let p = &ps[k];
    let Source::Breaker { slot } = p.source else { return None };
    if !p.ops[..j].iter().all(sort_emit) || !p.ops[j + 1..].iter().all(|o| matches!(o, Op::Project(_)) || limit_op(o)) || !p.ops[j + 1..].iter().any(limit_op) {
        return None;
    }
    if d.family().is_some() || d.family_top().is_some() || d.marks.len() < 2 {
        return None;
    }
    let mut locking = d.marks.iter().filter(|m| m.lock != LockKind::None);
    let (Some(m), None) = (locking.next(), locking.next()) else { return None };
    let refs: PgVec<'_, &gravity_decompose::ops::shared::modifytable::RowMark> = d.marks.iter().filter(|r| r.lock == LockKind::None).collect_in(gx.mcx);
    if m.parent || m.ctid_col <= 0 || m.relid != d.plan.target_rel || refs.iter().any(|r| r.parent || r.ctid_col <= 0 || r.mark != RowMarkType::ROW_MARK_REFERENCE || r.rti == m.rti) {
        return None;
    }
    let q = (0..k).rev().find(|&x| ps[x].desc.produces() == Some(slot))?;
    let pq = &ps[q];
    let n = Census(gx).inits(k, q)?;
    let jq = pq.ops.iter().position(|o| matches!(o, Op::Sort(_)))?;
    if scan_rel(&pq.source) != Some((m.rti, m.relid)) || !pq.ops[jq..].iter().all(|o| matches!(o, Op::Sort(_))) || !matches!(pq.ops[..jq].last(), Some(Op::Project(_))) || Census(gx).init_member(q) {
        return None;
    }
    let mut fetches: PgVec<'g, (usize, i16, u32)> = PgVec::new_in(gx.mcx);
    let mut used: PgVec<'_, u32> = PgVec::new_in(gx.mcx);
    let mut builds = 0;
    let mut lefts = 0;
    for (i, o) in pq.ops[..jq].iter().enumerate() {
        lefts += usize::from(matches!(o, Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_LEFT, .. } | JoinDesc::Nest { jointype: JoinType::JOIN_LEFT, .. })));
        let free = |rti: u32| refs.iter().copied().find(|r| r.rti == rti && !used.contains(&r.rti));
        let (f, r) = match o {
            Op::Filter(_) | Op::Gate(_) | Op::Project(_) => continue,
            // D3596: INNER or LEFT (an ORM's select_related over a nullable relation)
            Op::Join(JoinDesc::Probe { slot: bs, jointype: JoinType::JOIN_INNER | JoinType::JOIN_LEFT, catalog: false, .. }) => {
                let bq = gx.life_of(q, *bs).map(|l| l.0 as usize)?;
                let pb = &ps[bq];
                let Some(Op::Join(JoinDesc::Build { .. })) = pb.ops.last() else { return None };
                let (rti, _) = scan_rel(&pb.source)?;
                let r = free(rti)?;
                let outside = !Census(gx).init_member(bq);
                if scan_rel(&pb.source) != Some((r.rti, r.relid)) || !quiet_ref_build(gx, pb, &pb.ops[..pb.ops.len() - 1]) || !outside || !pb.desc.consumes.is_empty() {
                    return None;
                }
                builds += 1;
                (bq, r)
            }
            Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER | JoinType::JOIN_LEFT, inner, .. }) => {
                let (rti, _) = scan_rel(&inner.source)?;
                let r = free(rti)?;
                (crate::rerun::epq::ref_nest::ref_side(gx, q, i, r)?, r)
            }
            _ => return None,
        };
        used.push_in(r.rti);
        fetches.push_in((f, r.ctid_col, r.relid));
    }
    // REFFETCH and SORTNEST take one INNER join (D3596: one LEFT join is this form's); the statement's pipelines: the
    // lock's, the producer, the builds, the InitPlans'
    if fetches.len() < 2 && lefts == 0 || fetches.len() != refs.len() || !Census(gx).whole(2 + builds + n) || pq.desc.consumes.len() != builds {
        return None;
    }
    Some((q, jq, m.ctid_col, fetches))
}

/// A heap scan's (range, bitmap, index) (scanrelid, relid), else none.
fn scan_rel(s: &Source<'_>) -> Option<(u32, u32)> {
    match s {
        Source::Range { am: Am::Heap, scanrelid, relid, .. } | Source::Bitmap { am: Am::Heap, scanrelid, relid, .. } | Source::Index { am: Am::Heap, scanrelid, relid, only: false, .. } => Some((u32::from(*scanrelid), *relid)),
        _ => None,
    }
}

/// lockrows REFBUILD (`ref_build`): the build side's arms (pipeline, its ops before its feed or build, the locked leaf),
/// the union's emit (pipeline, its ops before the build) for a family, the probe op, the lock's junk ctid column, the
/// family's tableoid junk column, and the probe side's REFERENCE mark (its junk ctid column, its relation).
pub(crate) struct RefBuild<'g> {
    pub(crate) arms: PgVec<'g, (usize, usize, u32)>,
    pub(crate) union: Option<(usize, usize)>,
    pub(crate) jp: usize,
    pub(crate) ctid: i16,
    pub(crate) toid: Option<i16>,
    pub(crate) fetch: (i16, u32),
    /// REFBUILD sorted: the probe pipeline and its sort build op, the row's last redone op (else the lock's own pipeline and op).
    pub(crate) at: Option<(usize, usize)>,
    /// D3777 LOCKMERGE (`ref_merge`): the merge outer's Sort producer and its sort build op, when that side is sorted.
    pub(crate) so: Option<(usize, usize)>,
}

/// lockrows REFBUILD (main 10-04: lk-0108 / lk-0113, the mirror of REFFETCH): a lock in the pipeline that probes an INNER
/// hash join (filters, gates and projections around the probe; only projections and a LIMIT / OFFSET above the lock), its
/// source one REFERENCE mark's own quiet heap scan, the locked relation on the build side: one partitioned or inheritance
/// family's leaves, each its own heap scan under filters, gates and projections feeding the union whose emit builds (no
/// run-time pruning), or (under a LIMIT / OFFSET, else the runner's recheck serves it: marks.rs D776) one table's own heap
/// scan building, or (FAMJOIN) a family's one leaf the planner left (no union); no other pipeline or mark.
/// PostgreSQL's EvalPlanQual reruns the join over the test tuple (the
/// other leaves' marks inactive) and the probe side's row refetched at its junk ctid; here the recheck runs in the IR
/// (epq_ir.rs `epq_of_build`), the arm the row's tableoid names. The probe side's ops may not raise, or (D3379
/// REFCALM) call only `calm_call` functions: its refetched row is the version the join read, so they pass again, and
/// PG's outer-first or build-first order cannot show.
/// AGA-35 form 3: beside InitPlans that lock and write nothing, their pipelines counted out (`Census::plain_inits`) and
/// none of the form's own (`init_member`); EvalPlanQualStart copies an evaluated InitPlan's value, so the probe side reads
/// a counted-out InitPlan's parameter as a bound word (`init_param`), and plan.rs `Plan::form` asks `admit_ir_ref` of the probe
/// pipeline's part, `admit_ir_at` of each build arm's and `admit_ir_family` of a family's union emit. D3657: or beside
/// the InitPlan whose own row lock is the M4 pair's A (`pair_inits`; plan.rs `Plan::role` takes this lock as B).
/// D4688 REFBUILD-FEED (Delta TESTHYG's rowmap lock legs: `SELECT md5(string_agg(id::text, ',' ORDER BY id)) FROM (SELECT
/// t.id FROM t JOIN j USING (id) WHERE t.id % 5 = 0 FOR KEY SHARE OF t) l`: Aggregate -> [Sort] -> Subquery Scan ->
/// LockRows -> Hash Join, t built): or, one table, with no LIMIT / OFFSET, the lock pipeline's rows feeding a product
/// only the later pipelines read (`above_only`, REFNEST-FEED's rule: they lock and write nothing), filters, gates,
/// projections and a LIMIT / OFFSET above the lock, then the product's sink. PostgreSQL's LockRows hands its EvalPlanQual
/// row to its parent, so the rechecked row goes on through the ops above the lock into the product; the runner's recheck
/// would send it to the client (rerun/epq/marks.rs EMIT_ONLY; lockrows/marks.rs OP_ABOVE).
pub(crate) fn ref_build<'g>(gx: &crate::graph::Gx<'g, '_>, k: usize, j: usize, d: &LockDesc<'_>) -> Option<RefBuild<'g>> {
    use crate::ops::lockrows::lock_init::init_param;
    // D3777 LOCKMERGE: or the locked relation the inner of PostgreSQL's Merge Join
    if let Some(rb) = ref_merge(gx, k, j, d) {
        return Some(rb);
    }
    let ps = gx.pipelines();
    let plain = |o: &Op<'_>| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_));
    let lp = &ps[k];
    let limit = lp.ops[j + 1..].iter().any(limit_op);
    let n = Census(gx).inits(k, k)?;
    // D4688 REFBUILD-FEED: or the lock pipeline's rows feeding a product only the later pipelines read
    let feed = match lp.desc.produces() {
        Some(_) if !matches!(lp.source, Source::Breaker { .. }) => Census(gx).above_only(k).filter(|&a| a > 0)?,
        _ => 0,
    };
    let above = match feed {
        0 => lp.desc.produces().is_none() && lp.ops[j + 1..].iter().all(|o| matches!(o, Op::Project(_)) || limit_op(o)),
        _ => j + 2 <= lp.ops.len() && lp.ops[j + 1..lp.ops.len() - 1].iter().all(|o| plain(o) || limit_op(o)) && !matches!(lp.ops.last(), Some(Op::Write(_) | Op::Lock(_))),
    };
    if !above {
        return None;
    }
    // lockrows REFBUILD sorted (the queue-0015 claim's plan in the e2e rig: Limit -> LockRows -> Sort -> Hash Join, the locked
    // table built): one table under a LIMIT / OFFSET, the lock pipeline the Sort's emit, the probe pipeline its producer up to
    // the sort build (PostgreSQL's EvalPlanQual reruns the Sort over the join's one row)
    let (k, j, sorted) = match lp.source {
        Source::Breaker { slot } if limit && d.family_top().is_none() && lp.ops[..j].iter().all(sort_emit) && lp.desc.consumes.len() == 1 => {
            let q = (0..k).rev().find(|&x| ps[x].desc.produces() == Some(slot))?;
            let jq = ps[q].ops.iter().position(|o| matches!(o, Op::Sort(_)))?;
            if !ps[q].ops[jq..].iter().all(|o| matches!(o, Op::Sort(_))) {
                return None;
            }
            (q, jq, true)
        }
        _ => (k, j, false),
    };
    let p = &ps[k];
    if p.desc.consumes.len() != 1 || Census(gx).init_member(k) {
        return None;
    }
    let jp = p.ops[..j].iter().position(|o| !plain(o))?;
    let Op::Join(JoinDesc::Probe { slot: bs, jointype: JoinType::JOIN_INNER, catalog: false, .. }) = &p.ops[jp] else { return None };
    let heap = |s: &Source<'_>| match s {
        Source::Range { am: Am::Heap, scanrelid, relid, .. } | Source::Bitmap { am: Am::Heap, scanrelid, relid, .. } | Source::Index { am: Am::Heap, scanrelid, relid, only: false, .. } => Some((u32::from(*scanrelid), *relid)),
        _ => None,
    };
    // AGA-35 form 3: a counted-out InitPlan's parameter reads as a bound word (no InitPlan: `raise_free` and
    // `ops_raise_free` exactly, op_raise_free's filter, gate and projection arms and no other op)
    let bound = |id: i32| init_param(gx, id);
    // D3379 REFCALM: or a `calm_call` (REFMERGE's rule): the refetched probe row is the version the join read
    let rf = |n: types_nodes::Node<'_>| crate::effects::raise_free_by(gx, n, &bound, false) || calm_call(gx, n);
    let op_rf = |o: &Op<'_>| match o {
        Op::Filter(n) | Op::Gate(n) => rf(*n),
        Op::Project(l) => l.iter().all(rf),
        _ => false,
    };
    let quiet = |q: &gravity_decompose::PipelineNode<'_>, ops: &[Op<'_>]| {
        let recheck = match &q.source {
            Source::Index { recheck, .. } | Source::Bitmap { recheck, .. } => recheck.iter().all(rf),
            _ => true,
        };
        recheck && ops.iter().all(op_rf)
    };
    let (prti, prel) = heap(&p.source)?;
    let r = d.marks.iter().find(|m| m.rti == prti)?;
    if r.mark != RowMarkType::ROW_MARK_REFERENCE || r.lock != LockKind::None || r.parent || r.ctid_col <= 0 || r.relid != prel || !p.ops[jp + 1..j].iter().all(plain) || !quiet(p, &p.ops[..jp]) {
        return None;
    }
    let b = (0..k).rev().find(|&x| ps[x].desc.produces() == Some(*bs))?;
    let pb = &ps[b];
    let jb = pb.ops.len().checked_sub(1)?;
    if !matches!(pb.ops[jb], Op::Join(JoinDesc::Build { .. })) || Census(gx).init_member(b) {
        return None;
    }
    let others: PgVec<'_, &RowMark> = d.marks.iter().filter(|m| m.rti != prti).collect_in(gx.mcx);
    let fetch = (r.ctid_col, r.relid);
    let Some(top) = d.family_top() else {
        // one table: its one locking mark, the build scan's own, under a LIMIT / OFFSET (D4688: or feeding a product)
        let ([m], true) = (others.as_slice(), limit || feed > 0) else { return None };
        let own = heap(&pb.source) == Some((m.rti, m.relid)) && pb.ops[..jb].iter().all(plain) && pb.desc.consumes.is_empty();
        let mark = m.lock != LockKind::None && !m.parent && m.ctid_col > 0 && m.relid == d.plan.target_rel;
        let at = sorted.then_some((k, j));
        return (own && mark && Census(gx).whole(n + feed + if sorted { 3 } else { 2 })).then(|| RefBuild { arms: mcx::pg_vec![in gx.mcx; (b, jb, m.relid)], union: None, jp, ctid: m.ctid_col, toid: None, fetch, at, so: None });
    };
    // one family: the top parent's kin and the probe side's mark, nothing else (no feed: D4688's form is one table's)
    if feed > 0 || others.iter().any(|m| m.prti != top) {
        return None;
    }
    let ctid = others.iter().find(|m| m.rti == top)?.ctid_col;
    let toid = crate::ops::lockrows::marks::tableoid_col(d)?;
    let Some(Op::Append(AppendDesc::Union { slot: us, children, prune: None })) = pb.ops.first() else {
        // FAMJOIN (D655): a family the planner pruned to one leaf (its Append elided, the parent's mark kept):
        // that leaf's own heap scan builds, the one arm the row's tableoid names
        let kids: PgVec<'_, &&RowMark> = others.iter().filter(|m| m.rti != top).collect_in(gx.mcx);
        let ([m], Some((rti, relid))) = (kids.as_slice(), heap(&pb.source)) else { return None };
        let leaf = !m.parent && m.rti == rti && m.relid == relid && m.lock != LockKind::None;
        let own = pb.ops[..jb].iter().all(plain) && pb.desc.consumes.is_empty() && Census(gx).whole(n + 2);
        return (leaf && own && ctid > 0 && !sorted).then(|| RefBuild { arms: mcx::pg_vec![in gx.mcx; (b, jb, m.relid)], union: None, jp, ctid, toid: Some(toid), fetch, at: None, so: None });
    };
    if ctid <= 0 || !pb.ops[1..jb].iter().all(plain) || pb.desc.consumes.len() != 1 {
        return None;
    }
    let last = gx.life_of(b, *us).map(|l| l.0 as usize).filter(|&l| l < b)?;
    let feeds_slot = |q: usize| ps[q].ops.iter().position(|o| matches!(o, Op::Append(AppendDesc::Feed { slot, .. }) if slot == us));
    let mut feeds: PgVec<'_, usize> = (0..=last).rev().filter(|&q| feeds_slot(q).is_some()).take(*children as usize).collect_in(gx.mcx);
    feeds.reverse();
    if feeds.len() != *children as usize || feeds.last() != Some(&last) || !Census(gx).whole(feeds.len() + 2 + n) || feeds.iter().any(|&q| Census(gx).init_member(q)) {
        return None;
    }
    let mut arms: PgVec<'g, (usize, usize, u32)> = mcx::reserved_in(feeds.len(), gx.mcx);
    for q in feeds {
        let pq = &ps[q];
        let (rti, relid) = heap(&pq.source)?;
        let m = others.iter().find(|m| !m.parent && m.rti == rti && m.relid == relid && m.lock != LockKind::None)?;
        let jq = feeds_slot(q)?;
        if jq + 1 != pq.ops.len() || !pq.ops[..jq].iter().all(plain) || !pq.desc.consumes.is_empty() || arms.iter().any(|a: &(usize, usize, u32)| a.2 == m.relid) {
            return None;
        }
        arms.push_in((q, jq, m.relid));
    }
    if sorted {
        return None;
    }
    Some(RefBuild { arms, union: Some((b, jb)), jp, ctid, toid: Some(toid), fetch, at: None, so: None })
}

/// D3777 LOCKMERGE (REFBUILD's form over PostgreSQL's Merge Join; realapp-django's m2m `get()` FOR UPDATE OF the link
/// table, `SELECT sm.id, mms.id FROM sm JOIN mms ON sm.id = mms.sm_id .. LIMIT 21 FOR UPDATE OF mms`: Limit -> LockRows ->
/// Merge Join (Index Scan sm_pkey, Index Scan mms_sm), or either side a Sort over its scan): the locked relation the merge
/// inner, the REFERENCE-marked one the outer, each side its relation's own heap scan under filters and projections
/// (ref_nest.rs `merge_side`), the outer this pipeline's scan or a Sort's producer below its emit. PostgreSQL's
/// EvalPlanQual reruns the Merge Join over the test tuple and the outer's row refetched at its junk ctid (the version the
/// join read), outer first; here REFBUILD's redo (epq_ir.rs `epq_of_build`): the inner's part over the newest version,
/// then the outer's part over the refetched row, then the merge clauses and join qual. The outer's part is quiet
/// (`quiet_ref_build`: raise-free, or REFCALM's calm calls), so it passes again as it passed and the order cannot show.
/// Lock op `j` of pipeline `k`, no InitPlan, family or product, filters, gates and projections around the merge up to the
/// lock, projections and at least one LIMIT / OFFSET above, two marks, and no SubPlan in what the recheck evaluates.
fn ref_merge<'g>(gx: &crate::graph::Gx<'g, '_>, k: usize, j: usize, d: &LockDesc<'_>) -> Option<RefBuild<'g>> {
    use crate::rerun::epq::ref_nest::{merge_side, part_reads_subplan};
    let ps = gx.pipelines();
    let p = &ps[k];
    let plain = |o: &Op<'_>| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_));
    if !gx.graph.init_plans.is_empty() || d.family().is_some() || d.family_top().is_some() || p.desc.produces().is_some() || d.marks.len() != 2 {
        return None;
    }
    if !p.ops[j + 1..].iter().all(|o| matches!(o, Op::Project(_)) || limit_op(o)) || !p.ops[j + 1..].iter().any(limit_op) {
        return None;
    }
    let jp = p.ops[..j].iter().position(|o| !plain(o))?;
    let Op::Join(JoinDesc::Merge { slot, jointype: JoinType::JOIN_INNER, clauses, qual, .. }) = &p.ops[jp] else { return None };
    if !p.ops[jp + 1..j].iter().all(plain) {
        return None;
    }
    let m = d.marks.iter().find(|m| m.lock != LockKind::None)?;
    let r = d.marks.iter().find(|m| m.lock == LockKind::None)?;
    if m.parent || m.ctid_col <= 0 || m.relid != d.plan.target_rel || r.parent || r.ctid_col <= 0 || r.mark != RowMarkType::ROW_MARK_REFERENCE || r.rti == m.rti {
        return None;
    }
    let x = gx.life_of(k, *slot).map(|l| l.0 as usize).filter(|&x| x < k)?;
    let xe = merge_side(&ps[x])?;
    let so = match p.source {
        Source::Breaker { slot: os } => {
            let q = gx.life_of(k, os).map(|l| l.0 as usize).filter(|&q| q < k && q != x)?;
            let qe = merge_side(&ps[q]).filter(|&e| e < ps[q].ops.len())?;
            Some((q, qe))
        }
        _ => None,
    };
    let sorted = usize::from(so.is_some());
    let (oq, oe) = so.unwrap_or((k, jp));
    if p.desc.consumes.len() != 1 + sorted || !p.desc.consumes.contains(slot) || !Census(gx).whole(2 + sorted) || scan_rel(&ps[oq].source) != Some((r.rti, r.relid)) || scan_rel(&ps[x].source) != Some((m.rti, m.relid)) {
        return None;
    }
    let quiet = quiet_ref_build(gx, &ps[oq], &ps[oq].ops[..oe]) && quiet_ref_build(gx, p, &p.ops[..jp]);
    let reads = part_reads_subplan(&ps[oq], oe) || part_reads_subplan(p, j) || part_reads_subplan(&ps[x], xe) || clauses.iter().chain(qual.iter()).any(gravity_decompose::expr::has_subplan);
    (quiet && !reads).then(|| RefBuild { arms: mcx::pg_vec![in gx.mcx; (x, xe, m.relid)], union: None, jp, ctid: m.ctid_col, toid: None, fetch: (r.ctid_col, r.relid), at: None, so })
}

/// D3777 MERGETAIL (REFMERGE with the locked outer under a Sort; realapp-django's m2m `get()` FOR UPDATE OF the outer
/// relation, `.. JOIN mms ON sm.id = mms.sm_id WHERE mms.mm_id = 1 AND sm.field = 2 LIMIT 21 FOR UPDATE OF sm`: Limit ->
/// LockRows -> Merge Join (Sort -> Seq Scan sm, Sort -> Bitmap Heap Scan mms)): the lock in the merge pipeline over a
/// Sort's emit, the Sort's producer the locking mark's own heap scan under filters and projections, the merge inner the
/// REFERENCE relation's own quiet heap scan, spooled or sorted (ref_nest.rs `merge_side`, `quiet_ref_build`).
/// PostgreSQL's EvalPlanQual reruns the Merge Join over the test tuple (Sort -> scan) and the REFERENCE row refetched at its
/// junk ctid, outer first; here the Sort's producer's part over the newest version, then the merge pipeline's ops with
/// REFMERGE's fetch (epq_ir.rs `epq_of_tail` over `redo_of`'s merge arm). Lock op `j` of pipeline `k`, no InitPlan, family
/// or product, the emit's projections before the merge, filters, gates and projections after it up to the lock,
/// projections and at least one LIMIT / OFFSET above, two marks, and no SubPlan in what the recheck evaluates.
/// Some((the producer, its sort build op, the lock's junk ctid column, (the inner's pipeline, the REFERENCE mark's junk
/// ctid column, its relation))).
pub(crate) fn merge_tail(gx: &crate::graph::Gx<'_, '_>, k: usize, j: usize, d: &LockDesc<'_>) -> Option<(usize, usize, i16, (usize, i16, u32))> {
    use crate::rerun::epq::ref_nest::{merge_side, part_reads_subplan};
    let ps = gx.pipelines();
    let p = &ps[k];
    let Source::Breaker { slot: os } = p.source else { return None };
    let plain = |o: &Op<'_>| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_));
    if !gx.graph.init_plans.is_empty() || d.family().is_some() || d.family_top().is_some() || p.desc.produces().is_some() || d.marks.len() != 2 {
        return None;
    }
    if !p.ops[j + 1..].iter().all(|o| matches!(o, Op::Project(_)) || limit_op(o)) || !p.ops[j + 1..].iter().any(limit_op) {
        return None;
    }
    let jm = p.ops[..j].iter().position(|o| !matches!(o, Op::Project(_)))?;
    let Op::Join(JoinDesc::Merge { slot, jointype: JoinType::JOIN_INNER, clauses, qual, .. }) = &p.ops[jm] else { return None };
    if !p.ops[jm + 1..j].iter().all(plain) {
        return None;
    }
    let m = d.marks.iter().find(|m| m.lock != LockKind::None)?;
    let r = d.marks.iter().find(|m| m.lock == LockKind::None)?;
    if m.parent || m.ctid_col <= 0 || m.relid != d.plan.target_rel || r.parent || r.ctid_col <= 0 || r.mark != RowMarkType::ROW_MARK_REFERENCE || r.rti == m.rti {
        return None;
    }
    let q = gx.life_of(k, os).map(|l| l.0 as usize).filter(|&q| q < k)?;
    let qe = merge_side(&ps[q]).filter(|&e| e < ps[q].ops.len())?;
    let x = gx.life_of(k, *slot).map(|l| l.0 as usize).filter(|&x| x < k && x != q)?;
    let xe = merge_side(&ps[x])?;
    if p.desc.consumes.len() != 2 || !Census(gx).whole(3) || scan_rel(&ps[q].source) != Some((m.rti, m.relid)) || scan_rel(&ps[x].source) != Some((r.rti, r.relid)) || !quiet_ref_build(gx, &ps[x], &ps[x].ops[..xe]) {
        return None;
    }
    let reads = part_reads_subplan(&ps[q], qe) || part_reads_subplan(p, j) || part_reads_subplan(&ps[x], xe) || clauses.iter().chain(qual.iter()).any(gravity_decompose::expr::has_subplan);
    (!reads).then_some((q, qe, m.ctid_col, (x, r.ctid_col, r.relid)))
}

/// lockrows REFBUILD (`ref_build`): the lock step, each arm's in-IR recheck (a family's chosen by the row's tableoid), its
/// site a write site as wide as the widest leaf, numbered late (the scan sites and each recheck's fetch site first).
/// D3657: `mark`, its `Program.locks` entry (`Plan::pair_entry`), whose host also fetches the probe side's row.
fn ref_build_lock<'g>(px: &mut Px<'g, '_>, node: i32, d: &LockDesc<'_>, (k, j): (usize, usize), rb: &RefBuild<'_>, mark: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let Some(tf) = typefacts::facts_of(px.gx.facts, crate::cell::TIDOID) else { crate::refuse!(node, Reason::Bug { payload: "the tid type the catalog does not know" }) };
    let (k, j) = rb.at.unwrap_or((k, j));
    let cx = px.mcx();
    let epqs: PgVec<'_, (u32, PgRc<'_, Epq<'_>>)> = rb.arms.iter().map(|&(q, jq, relid)| (relid, PgRc::new_in(crate::ops::lockrows::epq::epq_of_build(px, (q, jq), rb.union, (k, rb.jp, j), rb.fetch, node, relid, rb.so).fetching_by(mark), cx))).collect_in(cx);
    if mark == 0 {
        px.claim_write(node, &d.plan);
    }
    let natts = rb.arms.iter().filter_map(|a| px.gx.facts.rel(a.2)).map(|r| r.natts.max(0) as u16).max().unwrap_or(0);
    let site = px.add_late_site(SiteDesc::write(d.plan.target_rel, natts));
    let marks = crate::rerun::epq::marks::Marks::none(px.mcx());
    let junk = Some((rb.ctid, tf));
    match rb.toid {
        Some(col) => {
            let Some(of) = typefacts::facts_of(px.gx.facts, types_core::catalog::OIDOID) else { crate::refuse!(node, Reason::Bug { payload: "the oid type the catalog does not know" }) };
            mcx::pg_box_dyn!(px.mcx(), Lock { site, relid: d.plan.target_rel, node, marks, epq: None, mark, junk, fam: Some(Fam { col, of, arms: epqs }), again: None } => dyn StepMech + 'g)
        }
        None => {
            let epq = epqs.into_iter().next().map(|e| e.1);
            mcx::pg_box_dyn!(px.mcx(), Lock { site, relid: d.plan.target_rel, node, marks, epq, mark, junk, fam: None, again: None } => dyn StepMech + 'g)
        }
    }
}

/// lockrows 15b: whether op `o` below the lock is the Sort emit's projection, each column the producer's same column.
pub(crate) fn sort_emit(o: &Op<'_>) -> bool {
    match o {
        Op::Project(l) => l.iter().all(|n| n.as_target_entry().is_some_and(|te| te.expr.as_var().is_some_and(|v| v.varno == OUTER_VAR && v.varlevelsup == 0 && i32::from(v.varattno) == i32::from(te.resno)))),
        _ => false,
    }
}

/// FAMLIMIT (plan.rs `breaker_limit` over a family): a lock over a Sort of one partitioned / inheritance family's Append,
/// each arm its leaf's own heap scan under filters, gates and projections (no other pipeline), with a LIMIT /
/// OFFSET above it. PostgreSQL's EvalPlanQual reruns Sort -> Append over the test tuple of the leaf the row's tableoid
/// names (the other children's marks are inactive: no tuple), and LockRows hands that row to the Limit. Here the recheck
/// runs in the IR over that arm's recheck quals, filters and projections (epq_ir.rs over its pipeline), chosen by the row's
/// tableoid junk column, and the row goes on through the steps above, counted; the lock position is the row's junk ctid in
/// that leaf. lockrows FAMMERGE (gen part-0048): or over a MergeAppend of those leaves, each arm feeding the merge's runs:
/// PostgreSQL's EvalPlanQual reruns the merge over that leaf's test tuple, the other leaves' scans empty (their marks
/// inactive), so the same redo. Some((each arm's (producer, its ops before the sort build, leaf relid)), junk ctid
/// column, tableoid column)).
/// lockrows FAMCLAIM (`paired`): the lock pipeline feeds the statement's other pipelines (its rows the IN subquery's),
/// `write_pair` then admits the write that carries them. D3502 FAMCTE: or it is a CTE's store (pg-boss v10's `WITH next
/// AS (.. ORDER BY .. LIMIT n FOR UPDATE SKIP LOCKED) UPDATE job j .. FROM next ..` over its partitioned job table),
/// every InitPlan a CTE; `write_pair` then admits the write that reads the store whole (CTESORT's form for a family).
/// AGA-35 form 4: FAMLIMIT (D3287 FAMMERGE-INIT: FAMMERGE too) beside InitPlans that lock and write nothing, their
/// pipelines counted out (`Census::plain_inits`; the paired form's write keeps it None) and no arm one of them
/// (`init_member`); EvalPlanQualStart copies an evaluated InitPlan's value, and plan.rs `Plan::form` asks `admit_ir_family` of each
/// arm's part.
pub(crate) fn family_limit<'g>(gx: &crate::graph::Gx<'g, '_>, k: usize, j: usize, d: &LockDesc<'_>, paired: bool) -> Option<(PgVec<'g, (usize, usize, u32)>, i16, i16)> {
    let ps = gx.pipelines();
    let p = &ps[k];
    let Source::Breaker { slot } = p.source else { return None };
    let top = d.family()?;
    // lockrows FAMMERGE (gen part-0048): or over a MergeAppend of the leaves (no run-time pruning), its projection passing
    // the merged rows through
    let merge = |o: &Op<'_>| matches!(o, Op::Append(AppendDesc::Merge { prune: None, .. }));
    if !p.ops[..j].iter().all(|o| sort_emit(o) || merge(o)) || !p.ops[j + 1..].iter().any(limit_op) {
        return None;
    }
    // D3287 FAMMERGE-INIT: beside an InitPlan too (AGA-35 form 4 held FAMMERGE back there only for its program's fixed
    // cost against the row executor's on the short pop; with the row executor gone that refusal is the user's error)
    // D3379 INITSINK: or beside the InitPlan whose own row lock is the M4 pair's A (`Census::pair_inits`)
    // D3502 FAMCTE (pg-boss v10's claim): or, `paired`, the lock pipeline a CTE's store beside CTEs only (CTESORT's
    // `Beside::Cte`): the statement's write reads the store, so no InitPlan here only reads (`plain_inits` none), and
    // `write_pair` admits that write when it reads the store whole (its `in_cte`)
    let n = match Census(gx).inits(k, k) {
        Some(n) => n,
        None if paired && gx.cte_write(k) && Census(gx).ctes_only() => 0,
        None => return None,
    };
    let ctid = d.marks.iter().find(|m| m.rti == top)?.ctid_col;
    let toid = crate::ops::lockrows::marks::tableoid_col(d)?;
    // the union's arms: the run of pipelines making the Sort's slot that ends at its producer for this read (`life_of`; an
    // InitPlan's or a CTE's pipeline may make the same slot number in an earlier life, D3379 INITCTE)
    let last = gx.life_of(k, slot).map(|l| l.0 as usize)?;
    let first = (0..=last).rev().take_while(|&x| ps[x].desc.produces() == Some(slot)).last()?;
    let qs: PgVec<'_, usize> = (first..=last).collect_in(gx.mcx);
    let alone = match paired {
        false => Census(gx).whole(qs.len() + 1 + n),
        true => p.desc.produces().is_some() && qs.iter().all(|&q| ps[q].desc.consumes.is_empty()),
    };
    if ctid <= 0 || qs.is_empty() || !alone {
        return None;
    }
    let arm = |o: &Op<'_>| matches!(o, Op::Append(AppendDesc::Arm { .. } | AppendDesc::Feed { .. }));
    let mut arms: PgVec<'g, (usize, usize, u32)> = mcx::reserved_in(qs.len(), gx.mcx);
    for q in qs {
        let pq = &ps[q];
        let (scanrelid, relid) = match &pq.source {
            Source::Range { am: Am::Heap, scanrelid, relid, .. } | Source::Bitmap { am: Am::Heap, scanrelid, relid, .. } | Source::Index { am: Am::Heap, scanrelid, relid, only: false, .. } => (u32::from(*scanrelid), *relid),
            _ => return None,
        };
        let m = d.marks.iter().find(|m| !m.parent && m.rti == scanrelid && m.relid == relid && m.lock != LockKind::None)?;
        let jq = pq.ops.iter().position(|o| matches!(o, Op::Sort(_))).unwrap_or(pq.ops.len());
        let below = pq.ops[..jq].iter().all(|o| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_)) || arm(o)) && matches!(pq.ops[..jq].iter().rev().find(|o| !arm(o)), Some(Op::Project(_)));
        let after = pq.ops[jq..].iter().all(|o| matches!(o, Op::Sort(_)));
        if !below || !after || !pq.desc.consumes.is_empty() || arms.iter().any(|a: &(usize, usize, u32)| a.2 == m.relid) || Census(gx).init_member(q) {
            return None;
        }
        arms.push_in((q, jq, m.relid));
    }
    Some((arms, ctid, toid))
}

/// lockrows FAMLIMIT (`family_limit`): the lock step, each arm's in-IR recheck, its site a write site as wide as the widest
/// leaf (the newest version's cells, in the row's leaf's layout), numbered late (the arms' scan sites first).
/// lockrows FAMCLAIM: `mark` 1, the lock is `Program.locks`' entry beside the statement's write, which keeps the write plan.
fn family_lock<'g>(px: &mut Px<'g, '_>, node: i32, d: &LockDesc<'_>, arms: &[(usize, usize, u32)], ctid: i16, col: i16, mark: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let Some(tf) = typefacts::facts_of(px.gx.facts, crate::cell::TIDOID) else { crate::refuse!(node, Reason::Bug { payload: "the tid type the catalog does not know" }) };
    let Some(of) = typefacts::facts_of(px.gx.facts, types_core::catalog::OIDOID) else { crate::refuse!(node, Reason::Bug { payload: "the oid type the catalog does not know" }) };
    let cx = px.mcx();
    let epqs = arms.iter().map(|&(q, jq, relid)| (relid, PgRc::new_in(crate::ops::lockrows::epq::epq_of_breaker(px, q, jq, node, relid), cx))).collect_in(cx);
    if mark == 0 {
        px.claim_write(node, &d.plan);
    }
    let natts = arms.iter().filter_map(|a| px.gx.facts.rel(a.2)).map(|r| r.natts.max(0) as u16).max().unwrap_or(0);
    let site = px.add_late_site(SiteDesc::write(d.plan.target_rel, natts));
    let marks = crate::rerun::epq::marks::Marks::none(px.mcx());
    let fam = Some(Fam { col, of, arms: epqs });
    mcx::pg_box_dyn!(px.mcx(), Lock { site, relid: d.plan.target_rel, node, marks, epq: None, mark, junk: Some((ctid, tf)), fam, again: None } => dyn StepMech + 'g)
}

/// lockrows 15b (plan.rs `breaker_limit`): the lock step, its site a write site wide enough for the newest version's cells (the
/// in-IR recheck reads them), numbered late (the producer's scan site first). lockrows REFFETCH (`ref_limit`): `fetch`, the
/// join's REFERENCE-marked build, whose row the recheck refetches into its own late site.
/// lockrows CTESORT: `mark` 1, the lock is `Program.locks`' entry beside the statement's write, which keeps the write plan.
fn breaker_lock<'g>(px: &mut Px<'g, '_>, node: i32, d: &LockDesc<'_>, (q, jq): (usize, usize), col: i16, fetch: &[(usize, i16, u32)], mark: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let relid = d.plan.target_rel;
    let Some(tf) = typefacts::facts_of(px.gx.facts, crate::cell::TIDOID) else { crate::refuse!(node, Reason::Bug { payload: "the tid type the catalog does not know" }) };
    // D3506 SETLOCK: a producer probing the sets of locks in hashed SubPlans' plans (plan.rs `breaker_limit`'s CTE form): its
    // probes evaluated again in the redo against sets of its own, as the LockRows' fresh EvalPlanQual estate builds
    // them
    let sets = fetch.is_empty() && crate::rerun::epq::set_lock::probes_sets(px.gx, q, jq);
    let epq = match fetch.is_empty() {
        false => PgRc::new_in(crate::ops::lockrows::epq::epq_of_ref(px, q, jq, node, relid, fetch).fetching_by(mark), px.mcx()),
        true if sets => {
            let own = crate::rerun::epq::set_lock::redo_probes(px, q, jq, node);
            PgRc::new_in(crate::ops::lockrows::epq::epq_of_subs(px, q, jq, node, relid).twinning(true).owning(own), px.mcx())
        }
        true => PgRc::new_in(crate::ops::lockrows::epq::epq_of_breaker(px, q, jq, node, relid), px.mcx()),
    };
    if mark == 0 {
        px.claim_write(node, &d.plan);
    }
    let natts = px.gx.facts.rel(relid).map_or(0, |r| r.natts.max(0) as u16);
    let site = px.add_late_site(SiteDesc::write(relid, natts));
    let marks = crate::rerun::epq::marks::Marks::none(px.mcx());
    mcx::pg_box_dyn!(px.mcx(), Lock { site, relid, node, marks, epq: Some(epq), mark, junk: Some((col, tf)), fam: None, again: None } => dyn StepMech + 'g)
}

/// D3565 REFDRAIN (`ref_drain`): `breaker_lock`'s lock over the Sort of a Hash Right Anti Join's drain, the locked relation
/// the join's build: its recheck redoes the build's part over the newest version, then the drain's ops (lockrows epq.rs
/// `epq_of_drain`).
fn drain_lock<'g>(px: &mut Px<'g, '_>, node: i32, d: &LockDesc<'_>, rd: &crate::ops::lockrows::plan::RefDrain, mark: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let relid = d.plan.target_rel;
    let Some(tf) = typefacts::facts_of(px.gx.facts, crate::cell::TIDOID) else { crate::refuse!(node, Reason::Bug { payload: "the tid type the catalog does not know" }) };
    let epq = PgRc::new_in(crate::ops::lockrows::epq::epq_of_drain(px, rd.build, (rd.q, rd.jd, rd.jq), node, relid), px.mcx());
    if mark == 0 {
        px.claim_write(node, &d.plan);
    }
    let natts = px.gx.facts.rel(relid).map_or(0, |r| r.natts.max(0) as u16);
    let site = px.add_late_site(SiteDesc::write(relid, natts));
    let marks = crate::rerun::epq::marks::Marks::none(px.mcx());
    mcx::pg_box_dyn!(px.mcx(), Lock { site, relid, node, marks, epq: Some(epq), mark, junk: Some((rd.col, tf)), fam: None, again: None } => dyn StepMech + 'g)
}

/// D3802 SORTCHAIN (rerun/epq/sort_chain.rs): `breaker_lock`'s lock over the Sort, its site made first: the recheck
/// reads the locked inner's newest version from it (epq_ir.rs `Fetch::held`), its other rows refetched.
fn sort_chain_lock<'g>(px: &mut Px<'g, '_>, node: i32, d: &LockDesc<'_>, sc: &crate::rerun::epq::sort_chain::SortChain) -> PgBox<'g, dyn StepMech + 'g> {
    let relid = d.plan.target_rel;
    let Some(tf) = typefacts::facts_of(px.gx.facts, crate::cell::TIDOID) else { crate::refuse!(node, Reason::Bug { payload: "the tid type the catalog does not know" }) };
    let natts = px.gx.facts.rel(relid).map_or(0, |r| r.natts.max(0) as u16);
    let site = px.add_late_site(SiteDesc::write(relid, natts));
    let epq = PgRc::new_in(crate::ops::lockrows::epq::epq_of_sort_chain(px, (sc.q, sc.jq), node, relid, sc.from, &sc.fetch, &site), px.mcx());
    px.claim_write(node, &d.plan);
    let marks = crate::rerun::epq::marks::Marks::none(px.mcx());
    mcx::pg_box_dyn!(px.mcx(), Lock { site, relid, node, marks, epq: Some(epq), mark: 0, junk: Some((sc.ctid, tf)), fam: None, again: None } => dyn StepMech + 'g)
}

/// D3607 COPYPROBE: `breaker_lock` whose recheck redoes lock pipeline `k`'s ops before the lock `j`, its hash probes of
/// COPY-marked rows matched again over the records they handed on (lockrows epq.rs `epq_of_copy_probe`).
fn probe_lock<'g>(px: &mut Px<'g, '_>, node: i32, d: &LockDesc<'_>, (k, j): (usize, usize), col: i16, mark: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let relid = d.plan.target_rel;
    let Some(tf) = typefacts::facts_of(px.gx.facts, crate::cell::TIDOID) else { crate::refuse!(node, Reason::Bug { payload: "the tid type the catalog does not know" }) };
    let epq = PgRc::new_in(crate::ops::lockrows::epq::epq_of_copy_probe(px, k, j, node, relid), px.mcx());
    if mark == 0 {
        px.claim_write(node, &d.plan);
    }
    let natts = px.gx.facts.rel(relid).map_or(0, |r| r.natts.max(0) as u16);
    let site = px.add_late_site(SiteDesc::write(relid, natts));
    let marks = crate::rerun::epq::marks::Marks::none(px.mcx());
    mcx::pg_box_dyn!(px.mcx(), Lock { site, relid, node, marks, epq: Some(epq), mark, junk: Some((col, tf)), fam: None, again: None } => dyn StepMech + 'g)
}

/// D3563 (`sort_tail`): `breaker_lock`'s 15b lock whose recheck redoes the Sort's producer `q` up to its sort build,
/// then lock pipeline `k`'s ops before the lock (the Result's SubPlan steps and projections) over that row.
/// D3777 MERGETAIL (`merge_tail`): `fetch`, the merge's REFERENCE inner, its row refetched in the recheck.
fn tail_lock<'g>(px: &mut Px<'g, '_>, node: i32, d: &LockDesc<'_>, ((q, jq), (k, j)): ((usize, usize), (usize, usize)), col: i16, fetch: &[(usize, i16, u32)], mark: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let relid = d.plan.target_rel;
    let Some(tf) = typefacts::facts_of(px.gx.facts, crate::cell::TIDOID) else { crate::refuse!(node, Reason::Bug { payload: "the tid type the catalog does not know" }) };
    let epq = PgRc::new_in(crate::ops::lockrows::epq::epq_of_tail(px, (q, jq), (k, j), node, relid, fetch).fetching_by(mark), px.mcx());
    if mark == 0 {
        px.claim_write(node, &d.plan);
    }
    let natts = px.gx.facts.rel(relid).map_or(0, |r| r.natts.max(0) as u16);
    let site = px.add_late_site(SiteDesc::write(relid, natts));
    let marks = crate::rerun::epq::marks::Marks::none(px.mcx());
    mcx::pg_box_dyn!(px.mcx(), Lock { site, relid, node, marks, epq: Some(epq), mark, junk: Some((col, tf)), fam: None, again: None } => dyn StepMech + 'g)
}

/// REFNEST's lock (rerun/epq/ref_nest.rs `form`): the lock at op `j` of pipeline `k`, its position the row's junk ctid
/// `ctid` (as 15b's), its recheck the IR's over the nest (epq_ir.rs `Fetch` under the nest's parameters; D3564
/// `chain_form`: one `fetch` per nest); the program's one lock. Its site a write site as wide as the relation (the
/// newest version's cells), numbered late (the scan sites first).
fn ref_nest_lock<'g>(px: &mut Px<'g, '_>, node: i32, d: &LockDesc<'_>, (k, j): (usize, usize), (ctid, fetch): (i16, &[(usize, i16, u32)]), mark: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let relid = d.plan.target_rel;
    let Some(tf) = typefacts::facts_of(px.gx.facts, crate::cell::TIDOID) else { crate::refuse!(node, Reason::Bug { payload: "the tid type the catalog does not know" }) };
    let epq = PgRc::new_in(crate::ops::lockrows::epq::epq_of_nest(px, k, j, node, relid, fetch).fetching_by(mark), px.mcx());
    if mark == 0 {
        px.claim_write(node, &d.plan);
    }
    let natts = px.gx.facts.rel(relid).map_or(0, |r| r.natts.max(0) as u16);
    let site = px.add_late_site(SiteDesc::write(relid, natts));
    let marks = crate::rerun::epq::marks::Marks::none(px.mcx());
    mcx::pg_box_dyn!(px.mcx(), Lock { site, relid, node, marks, epq: Some(epq), mark, junk: Some((ctid, tf)), fam: None, again: None } => dyn StepMech + 'g)
}

/// D3379 LOCKNEST (rerun/epq/ref_nest.rs `lock_form`): the lock at op `j` of pipeline `k` on the inner `ik` of its INNER
/// nest op `jn`, its position the locked mark's junk ctid column `ctid`, its recheck the IR's (`epq_of_lock_nest`); the
/// program's one lock (D3601 COPYNEST: or `Program.locks` entry `mark`), its site a write site as wide as the relation
/// (the newest version's cells), numbered late.
fn lock_nest_lock<'g>(px: &mut Px<'g, '_>, node: i32, d: &LockDesc<'_>, (k, j): (usize, usize), (jn, ik): (usize, usize), ctid: i16, mark: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let relid = d.plan.target_rel;
    let Some(tf) = typefacts::facts_of(px.gx.facts, crate::cell::TIDOID) else { crate::refuse!(node, Reason::Bug { payload: "the tid type the catalog does not know" }) };
    let epq = PgRc::new_in(crate::ops::lockrows::epq::epq_of_lock_nest(px, (k, j), (jn, ik), node, relid), px.mcx());
    // D3601 (6) COPYNEST: a claim beside the writes is a Program.locks entry (`mark`), the writes keep the write plan
    if mark == 0 {
        px.claim_write(node, &d.plan);
    }
    let natts = px.gx.facts.rel(relid).map_or(0, |r| r.natts.max(0) as u16);
    let site = px.add_late_site(SiteDesc::write(relid, natts));
    let marks = crate::rerun::epq::marks::Marks::none(px.mcx());
    mcx::pg_box_dyn!(px.mcx(), Lock { site, relid, node, marks, epq: Some(epq), mark, junk: Some((ctid, tf)), fam: None, again: None } => dyn StepMech + 'g)
}

/// D3506 SETLOCK (rerun/epq/set_lock.rs `form`): the lock at op `j` of hashed SubPlan set build `k`, rechecked in the
/// IR over its own scan as a product-form lock (its newest version goes on into the set), a `Program.locks` entry of
/// its own after the claims' (`set_lock::mark`; plan.rs `Plan::write_pair` lays the claims' and appends these). Its site
/// a write site as wide as the relation (the newest version's cells), numbered late.
fn set_lock<'g>(px: &mut Px<'g, '_>, node: i32, d: &LockDesc<'_>, (k, j): (usize, usize), site: LockSite) -> PgBox<'g, dyn StepMech + 'g> {
    let Some(mark) = Census(px.gx).set_entry(k, j, limit_op) else { crate::refuse!(node, Reason::Bug { payload: "a SETLOCK lock with no mark" }) };
    let (relid, _, _) = admit(px, k, j, d, node, true, false);
    crate::ops::lockrows::lock_init::admit_ir(px, k, node);
    let epq = PgRc::new_in(epq_of(px, k, j, node, relid, false), px.mcx());
    Plan(px).lay_at(mark, site);
    let natts = px.gx.facts.rel(relid).map_or(0, |r| r.natts.max(0) as u16);
    let at = px.add_late_site(SiteDesc::write(relid, natts));
    let marks = crate::rerun::epq::marks::Marks::none(px.mcx());
    mcx::pg_box_dyn!(px.mcx(), Lock { site: at, relid, node, marks, epq: Some(epq), mark, junk: None, fam: None, again: None } => dyn StepMech + 'g)
}

/// lockrows SUBLOCK (rerun/epq/sub_lock.rs `form`): the lock at op `j` of SubPlan inner `k`, its position the row's junk ctid, its
/// recheck the IR's over the inner's source and ops below it; beside the statement's write a `Program.locks` entry of its
/// own (mark 1, lock_init.rs JOBQ's form), else the program's one lock. Its site a write site as wide as the relation (the
/// newest version's cells), numbered late (the scan sites first).
fn sub_lock<'g>(px: &mut Px<'g, '_>, node: i32, d: &LockDesc<'_>, (k, j): (usize, usize), s: &crate::rerun::epq::sub_lock::SubLock) -> PgBox<'g, dyn StepMech + 'g> {
    // D3379 SUBLOCK-LEADER: the SubPlan's pipeline runs on the leader, the lock host's (decompose leaves a plain SELECT's
    // pipeline parallel-eligible; PostgreSQL plans no parallel node in a query whose subquery holds row marks,
    // max_parallel_hazard_walker)
    if matches!(px.gx.pipelines()[s.outer].desc.parallel, Parallel::Eligible) && !px.serial.contains(&s.outer) {
        px.serial.push(s.outer); // alloc: Program
    }
    // AGA-35 form 6: beside the outer query's InitPlans (plan.rs `Census::sub_inits`, lock_init.rs `admit_sub`): beside the write each one
    // read before any row reaches it
    crate::ops::lockrows::lock_init::admit_sub(px, node, s.outer, s.write);
    let relid = d.plan.target_rel;
    let Some(tf) = typefacts::facts_of(px.gx.facts, crate::cell::TIDOID) else { crate::refuse!(node, Reason::Bug { payload: "the tid type the catalog does not know" }) };
    let epq = PgRc::new_in(crate::ops::lockrows::epq::epq_of_sub(px, k, j, node, relid), px.mcx());
    // D3506 EXISTS: beside a claim of the outer pipeline instead (rerun/epq/sub_lock.rs `claim_above`): the claim's
    // entry 1
    let mark = match (s.write, s.claim) {
        (_, Some(c)) => {
            Plan(px).lay(vec![c, s.site]); // alloc: Program
            2
        }
        // D3660: beside the InitPlan whose own row lock is the M4 pair's A: entry 2, plan.rs `Plan::role` laying both
        (false, None) if s.init => {
            let m = Plan(px).role(k, node);
            if m != 2 {
                shape(node, crate::ops::lockrows::lock_init::LOCK_INIT_FORM);
            }
            m
        }
        (true, None) => {
            Plan(px).lay(vec![s.site]); // alloc: Program
            1
        }
        // D3667: or beside INITCTE's INSERTs, which keep the program's plan (made before or after this lock)
        (false, None) if s.cte => {
            Plan(px).lay(vec![s.site]); // alloc: Program
            1
        }
        (false, None) => {
            px.claim_write(node, &d.plan);
            0
        }
    };
    let natts = px.gx.facts.rel(relid).map_or(0, |r| r.natts.max(0) as u16);
    let site = px.add_late_site(SiteDesc::write(relid, natts));
    let marks = crate::rerun::epq::marks::Marks::none(px.mcx());
    mcx::pg_box_dyn!(px.mcx(), Lock { site, relid, node, marks, epq: Some(epq), mark, junk: Some((s.ctid, tf)), fam: None, again: None } => dyn StepMech + 'g)
}

/// D3506 EXISTS SORT (rerun/epq/sub_lock.rs `sort_claim`): `breaker_lock` for the claim, `Program.locks`' entry 1 (the
/// inner lock's 2, `sub_lock`), whose recheck redoes the Sort producer `q`'s part before its sort build `jq` with its
/// SubPlan steps made again for it (`redo_subs`), each EXISTS locking again as in PostgreSQL's EvalPlanQual.
fn sort_sub_claim<'g>(px: &mut Px<'g, '_>, node: i32, d: &LockDesc<'_>, (q, jq): (usize, usize), col: i16) -> PgBox<'g, dyn StepMech + 'g> {
    let relid = d.plan.target_rel;
    let Some(tf) = typefacts::facts_of(px.gx.facts, crate::cell::TIDOID) else { crate::refuse!(node, Reason::Bug { payload: "the tid type the catalog does not know" }) };
    let own = crate::rerun::epq::sub_lock::redo_subs(px, q, jq);
    let epq = PgRc::new_in(crate::ops::lockrows::epq::epq_of_subs(px, q, jq, node, relid), px.mcx());
    let natts = px.gx.facts.rel(relid).map_or(0, |r| r.natts.max(0) as u16);
    let site = px.add_late_site(SiteDesc::write(relid, natts));
    let marks = crate::rerun::epq::marks::Marks::none(px.mcx());
    let lock: PgBox<'g, dyn StepMech + 'g> = mcx::pg_box_dyn!(px.mcx(), Lock { site, relid, node, marks, epq: Some(epq), mark: 1, junk: Some((col, tf)), fam: None, again: None } => dyn StepMech + 'g);
    mcx::pg_box_dyn!(px.mcx(), crate::rerun::epq::sub_lock::SortClaim { lock, own } => dyn StepMech + 'g)
}

/// Every refusal by name of the module doc; the relation the rows are locked in when the lock serves, and whether the
/// pipeline is one arm of a family's Append (L2).
/// `product` (V4R-512): the rechecked row goes on through every step above into the product, as any row: no op above refuses.
/// `subs` (D652-S1's lock half): the recheck is the IR's over the SubPlan steps, so a LIMIT / OFFSET above counts its row.
fn admit(px: &Px<'_, '_>, k: usize, j: usize, d: &LockDesc<'_>, node: i32, product: bool, subs: bool) -> (u32, bool, bool) {
    let p = &px.gx.pipelines()[k];
    if d.plan.lock == LockKind::None {
        crate::refuse!(node, Reason::Bug { payload: "a LockRows whose plan takes no lock" });
    }
    // a LockRows desc carries ParentMark (L2) and no other member: another is a bug
    if d.members.iter().any(|m| m != Member::ParentMark) {
        crate::refuse!(node, Reason::Bug { payload: "a LockRows carrying a member write.lock does not bind" });
    }
    // lockrows REFFAM: the locked family may sit beside other parents' whole REFERENCE families (marks.rs `lock_family`),
    // and a lock with no family beside them only (L1): rerun/epq/marks.rs `sites_in` gates each REFERENCE child
    let family = crate::ops::lockrows::marks::lock_family(d);
    let top = d.marks.iter().find(|m| m.parent && m.prti == m.rti).map(|m| m.rti);
    let refs_only = crate::ops::lockrows::marks::refs_beside(d);
    if family.is_none() && !refs_only && top.is_some_and(|t| d.marks.iter().any(|m| m.prti != t)) {
        shape(node, FAMILY_JOIN);
    }
    let (am, scanrelid, src, only) = match &p.source {
        Source::Range { am, scanrelid, relid, .. } | Source::Bitmap { am, scanrelid, relid, .. } | Source::Tid { am, scanrelid, relid, .. } => (*am, u32::from(*scanrelid), *relid, false),
        Source::Index { am, scanrelid, relid, only, .. } => (*am, u32::from(*scanrelid), *relid, *only),
        _ if family.is_some() => shape(node, FAMILY_BREAKER),
        _ => shape(node, NOT_OWN),
    };
    let mark = match family {
        // L1: the one locking mark, the plan's relation
        None => {
            let mut locking = d.marks.iter().filter(|m| m.lock != LockKind::None);
            let (Some(mark), None) = (locking.next(), locking.next()) else { crate::refuse!(node, Reason::Bug { payload: "a LockRows without exactly one locking mark" }) };
            if mark.relid != d.plan.target_rel {
                crate::refuse!(node, Reason::Bug { payload: "a LockRows whose locking mark is not its plan's relation" });
            }
            mark
        }
        // L2: the family child this pipeline scans (ExecLockRows' ExecRowMark whose relid is the row's tableoid); FAMJOIN: a
        // pipeline over the joined relation's own scan (the family's leaf on the build side) is no family arm
        Some(_) => d.marks.iter().find(|m| !m.parent && m.rti == scanrelid && m.lock != LockKind::None).unwrap_or_else(|| shape(node, if d.family().is_some() { NOT_OWN } else { FAMILY_JOIN })),
    };
    let arm = |o: &Op<'_>| family.is_some() && matches!(o, Op::Append(AppendDesc::Arm { .. }));
    // D3287 (b): or a LEFT hash probe / LEFT nest whose inner is a REFERENCE relation (FOR UPDATE never reaches a nullable
    // side): a row the join null-extended holds that mark's ctid NULL, which gives the recheck no inner row (marks.rs
    // `sites_in` / `nest_scan`: null_empty, D2048), so the recheck null-extends again, as EvalPlanQualFetchRowMark's no-row does
    let left = |o: &Op<'_>| crate::rerun::epq::marks::left_joins(o) || super::marks::spool_left_nest(px.gx, k, o);
    // D3287 (b): or an INNER nest whose inner is a VALUES list's scan: its COPY row is stored per row and the recheck's
    // re-open of the inner yields it alone (rerun/epq/marks.rs `lock_values_nest`); or one whose inner is a constant
    // Result (no mark; the recheck re-opens it: `nest_const`)
    // D3291: or an INNER nest whose inner holds one Memoize over a REFERENCE relation's scan (`nest_memo_joins`)
    // D3656: or an INNER nest whose inner reads the quiet fill of a COPY-marked VALUES list (`lock_values_spool`)
    let join = |o: &Op<'_>| crate::rerun::epq::marks::joins(o) || crate::rerun::epq::marks::nest_joins(o) || crate::rerun::epq::marks::nest_memo_joins(o) || left(o) || crate::rerun::epq::marks::lock_values_nest(px.gx, k, o, &d.marks) || crate::rerun::epq::marks::lock_values_spool(px.gx, k, o, &d.marks) || crate::rerun::epq::marks::nest_const(o);
    let below = p.ops[..j].iter().all(|o| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_) | Op::SubPlan(_)) || arm(o) || join(o));
    if am != Am::Heap || only || src != mark.relid || scanrelid != mark.rti || !below {
        shape(node, NOT_OWN);
    }
    let mut limit = false;
    for o in p.ops[j + 1..].iter() {
        match o {
            Op::Project(_) => {}
            o if limit_op(o) && p.ops[..j].iter().any(join) => shape(node, LIMIT_ABOVE),
            o if limit_op(o) && !subs && p.ops[..j].iter().any(|o| matches!(o, Op::SubPlan(_))) => shape(node, SUBPLAN_EPQ),
            o if limit_op(o) => limit = true,
            _ if product => {}
            _ => shape(node, OP_ABOVE),
        }
    }
    (mark.relid, p.ops[..j].iter().any(arm), limit)
}

/// Whether op `o` above the lock is a LIMIT / OFFSET (its presence only: the bound is the limit family's, C30).
pub(crate) fn limit_op(o: &Op<'_>) -> bool {
    limit_bounds(o).is_some()
}

/// V4R-512: the lock's recheck is its own relation's alone: one locking mark and no other (no family, no REFERENCE / COPY
/// mark), and only filters, gates and projections below it (no join, no SubPlan op: nothing another pipeline makes).
pub(crate) fn one_site(p: &gravity_decompose::PipelineNode<'_>, j: usize, d: &LockDesc<'_>) -> bool {
    let below = p.ops[..j].iter().all(|o| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_)));
    d.family().is_none() && d.marks.iter().all(|m| m.lock != LockKind::None && !m.parent) && below
}

/// D3506 SETLOCK: `one_site` beside SubPlan steps below the lock, each a hashed probe of a set whose plan locks
/// (rerun/epq/set_lock.rs `probes_sets`, which the caller asks first).
pub(crate) fn set_site(p: &gravity_decompose::PipelineNode<'_>, j: usize, d: &LockDesc<'_>) -> bool {
    let below = p.ops[..j].iter().all(|o| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_) | Op::SubPlan(_)));
    d.family().is_none() && d.marks.iter().all(|m| m.lock != LockKind::None && !m.parent) && below
}

/// producerchain E (V4R-512 over L2): a family's lock (`LockDesc::family`: the parent marks and their children, every one
/// with the plan's strength and wait policy, no other relation's mark) in a pipeline over one of its leaves' own scans, the
/// planner's single leaf or an Append arm, with only filters, gates, projections and the arm's marker below it. Its recheck
/// is that leaf's alone, as V4R-512's is its relation's: ExecLockRows skips the parent marks and gives the other children's
/// scans no test tuple (their marks inactive for the row), so EvalPlanQual reruns only the leaf the row came from.
pub(crate) fn own_leaf(p: &gravity_decompose::PipelineNode<'_>, j: usize, d: &LockDesc<'_>) -> bool {
    let below = p.ops[..j].iter().all(|o| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_) | Op::Append(AppendDesc::Arm { .. })));
    let scan = match &p.source {
        Source::Range { scanrelid, .. } | Source::Index { scanrelid, .. } | Source::Bitmap { scanrelid, .. } => u32::from(*scanrelid),
        _ => return false,
    };
    d.family().is_some() && below && d.marks.iter().any(|m| !m.parent && m.rti == scan && m.lock != LockKind::None)
}

/// D652-S1's lock half (lockrows; modifytable's write/rowop_epq.rs `subs_in_ir`): a lock over its own relation alone (one
/// locking mark, no other) whose only other pipelines are the rerun sets of its per-row SubPlan steps below it (a correlated
/// SubPlan over an aggregate inner, re-run at every call). The runner's recheck would rerun those producers whole, their
/// scans held at the stopped node's positions (D652); in the IR the redo evaluates each SubPlan step again over the newest
/// version, in its place among the filters and projections, as PostgreSQL's EvalPlanQual re-runs the SubPlan with the new
/// row's arguments under the statement's snapshot. AGA-35 form 7: beside InitPlans that read only, their pipelines counted
/// out (`Census::subs_inits`: none attached inside a SubPlan step's plan or an inner, where EvalPlanQualStart re-arms it
/// and the recheck runs it again, D2112); `admit_ir` then asks each of them to be attached at the lock or above (INIT_BELOW)
/// and read by a filter below the lock, an index scan's keys or a TID scan's quals at an unconditional position
/// (INIT_READ), since a SubPlan step below the lock may read any of them. False keeps marks.rs's form and its refusals.
pub(crate) fn subs_in_ir(gx: &crate::graph::Gx<'_, '_>, k: usize, j: usize, d: &LockDesc<'_>) -> bool {
    let p = &gx.pipelines()[k];
    if d.family().is_some() || !d.marks.iter().all(|m| m.lock != LockKind::None && !m.parent) {
        return false;
    }
    for o in p.ops[..j].iter() {
        match o {
            Op::Filter(_) | Op::Gate(_) | Op::Project(_) | Op::SubPlan(_) => {}
            _ => return false,
        }
    }
    let member = |q: usize| gx.graph.reruns.iter().any(|r| p.ops[..j].iter().any(|o| matches!(o, Op::SubPlan(s) if s.plan_id == r.plan_id)) && r.pipelines.iter().any(|&m| usize::from(m) == q));
    // D3563: or SubPlan steps whose plans are embedded inners (no product: Django's annotate(Subquery(..)) on a claim,
    // `SELECT t.id, (SELECT q.name FROM app_queue q WHERE q.id = t.queue_id) .. LIMIT 4 FOR UPDATE SKIP LOCKED`) under
    // a LIMIT / OFFSET, where the runner's recheck would send the rechecked row past it (SUBPLAN_EPQ)
    // D3707: or the claim beside an inner lock (rerun/epq/sub_lock.rs `sub_claim`) with no LIMIT, which the runner's
    // recheck cannot take (SUBLOCK_RUNNER: its node never locks again)
    if !p.ops[..j].iter().any(|o| matches!(o, Op::SubPlan(_))) || !gx.reads_product(k) && !p.ops[j + 1..].iter().any(limit_op) && !crate::rerun::epq::sub_lock::sub_claim(gx, k, j) {
        return false;
    }
    let Some(init) = Census(gx).subs_inits(k, &member) else { return false };
    (0..gx.pipelines().len()).all(|q| q == k || member(q) || init.contains(&q))
}

/// V4R-255 (D717): the lock over the planner's dummy relation, which no row reaches.
struct NoRows;

impl StepMech for NoRows {
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        next(b, env, row)
    }
}

impl Lock<'_> {
    /// The lock site's 12-byte position (`ScanPos`): flags | off << 16, the relation, the block (write/rowop.rs's form).
    fn store_pos(&self, b: &mut FunctionBuilder<'_>, env: &Env, block: Value, offset: Value) {
        self.store_pos_in(b, env, block, offset, None);
    }

    /// `store_pos` in relation `rel` (an I32: lockrows FAMLIMIT's row's leaf), else the lock's own.
    fn store_pos_in(&self, b: &mut FunctionBuilder<'_>, env: &Env, block: Value, offset: Value, rel: Option<Value>) {
        let rel = rel.unwrap_or_else(|| b.iconst(Type::I32, self.relid as i32 as i64));
        store_site_pos(b, env, self.site.get(), (block, offset), rel);
    }
}

/// Site `site`'s 12-byte position (`ScanPos`): flags | off << 16, relation `rel` (an I32), the block (write/rowop.rs's form).
pub(crate) fn store_site_pos(b: &mut FunctionBuilder<'_>, env: &Env, site: usize, (block, offset): (Value, Value), rel: Value) {
    let s = site_off(site) as i32;
    let off = b.conv(ConvOp::Trunc, Type::I32, offset);
    let sixteen = b.iconst(Type::I32, 16);
    let hi = b.int_bin(IntBinOp::Shl, off, sixteen);
    let flags = b.iconst(Type::I32, (POS_VALID | POS_ROW) as i64);
    let w0 = b.int_bin(IntBinOp::Or, hi, flags);
    b.store(env.ctx, s + site::POS_FLAGS as i32, w0);
    b.store(env.ctx, s + site::POS_REL as i32, rel);
    let blk = b.conv(ConvOp::Trunc, Type::I32, block);
    b.store(env.ctx, s + site::POS_BLOCK as i32, blk);
}

/// The row's junk ctid column `col` (an ItemPointerData image: bi_hi, bi_lo, ip_posid) as (block, offset): the position
/// of a lock whose recheck is the IR's.
pub(crate) fn junk_tid(b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, (col, tf): (i16, &TypeFacts), node: i32) -> (Value, Value) {
    let Some(cr) = row.col(col) else { crate::refuse!(node, Reason::Bug { payload: "a lock's ctid column past the row" }) };
    let mut cx = env.lx(b, node);
    let (d, _null) = crate::rep::datum(&mut cx, cr, tf);
    env.memo = cx.memo;
    let p = b.conv(ConvOp::IntToPtr, Type::Ptr, d);
    let (hi, lo, off) = (b.load_zext(Type::I16, p, 0), b.load_zext(Type::I16, p, 2), b.load_zext(Type::I16, p, 4));
    let sixteen = b.iconst(Type::I64, 16);
    let hi = b.int_bin(IntBinOp::Shl, hi, sixteen);
    (b.int_bin(IntBinOp::Or, hi, lo), off)
}

impl<'g> StepMech for Lock<'g> {
    /// The runner's recheck sends the rechecked row to the client (module doc): the end is the serial out-block sink.
    fn end_is(&mut self, end: &dyn EndMech) {
        if self.epq.is_none() && self.fam.is_none() && end.sink() != SinkKind::Serial {
            shape(self.node, NOT_SINK);
        }
    }

    fn wants_locator(&self) -> bool {
        self.junk.is_none()
    }

    fn reentry(&self) -> Option<i32> {
        self.again
    }

    /// D3289: the re-entry word starts clear at every open, as an expanding step's offset does.
    fn open_words(&self, w: &mut OpenWords) {
        if let Some(o) = self.again {
            w.declare(OpenWord::open("write.lock re-entry", Base::Words, o, Type::I64, 0));
        }
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        // D3289: the row is here: a re-entry it waited for is over
        if let Some(w) = self.again {
            env.store_word(b, w, env.zero);
        }
        // lockrows 15b: the row's junk ctid (an ItemPointerData image: bi_hi, bi_lo, ip_posid) is its position; the recheck
        // is the IR's, so no source site is read
        // lockrows FAMLIMIT: the row's leaf, its tableoid junk column (an oid datum), the position's relation
        let mut leaf = None;
        let src = match &self.junk {
            Some((col, tf)) => {
                let (block, off) = junk_tid(b, env, row, (*col, tf), self.node);
                if let Some(f) = &self.fam {
                    let Some(tr) = row.col(f.col) else { crate::refuse!(self.node, Reason::Bug { payload: "a lock's tableoid column past the row" }) };
                    let mut cx = env.lx(b, self.node);
                    leaf = Some(crate::rep::datum(&mut cx, tr, &f.of).0);
                    env.memo = cx.memo;
                }
                match leaf {
                    Some(o) => {
                        let rel = b.conv(ConvOp::Trunc, Type::I32, o);
                        self.store_pos_in(b, env, block, off, Some(rel));
                    }
                    None => self.store_pos(b, env, block, off),
                }
                0
            }
            None => {
                let Some(loc) = env.loc else { crate::refuse!(self.node, Reason::Bug { payload: "a row lock over a source without a locator" }) };
                let Some(src) = env.site else { crate::refuse!(self.node, Reason::Bug { payload: "a row lock whose source has a locator but no site" }) };
                self.store_pos(b, env, loc.block, loc.offset);
                src
            }
        };
        if self.junk.is_some() && self.epq.is_none() && self.fam.is_none() {
            crate::refuse!(self.node, Reason::Bug { payload: "a junk-ctid row lock without its in-IR recheck" });
        }
        crate::rerun::epq::marks::store(b, env, row, self.node, &self.marks);
        let mem = HostFlags { touches_memory: true, ..HostFlags::default() };
        let lock_row = gravity_kernels::ops::lockrows::entries::lock_row as *const () as usize;
        let h = b.declare_host_once("lock_row", &[Type::Ptr, Type::I64], Some(Type::I64), lock_row, mem);
        let arg = self.mark.checked_sub(1).map_or(self.site.get() as u64, |i| LockSite::arg(self.site.get(), i));
        let site_c = b.iconst(Type::I64, arg as i64);
        let st = b.call_host(h, &[env.ctx, site_c], Some(Type::I64)).unwrap_or(env.zero);
        let types: PgVec<'_, Type> = row.carry.params().iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
        let l_join = b.create_block(&types[..]);
        // below zero: the host's error; above RECHECK: no lock answers it (CONFLICT is ON CONFLICT's)
        let (l_bad, l_ok) = (b.create_block(&[]), b.create_block(&[]));
        b.mark_cold(l_bad);
        let rc = b.iconst(Type::I64, WRITE_RECHECK);
        let odd = b.icmp(IntCond::Sgt, st, rc);
        let neg = b.icmp(IntCond::Slt, st, env.zero);
        let bad = b.int_bin(IntBinOp::Or, neg, odd);
        b.cond_br(bad, (l_bad, &[]), (l_ok, &[]));
        b.switch_to(l_bad);
        let uns = b.iconst(Type::I64, unsupported(UNSUPPORTED_SHAPE));
        let err = b.select(neg, st, uns);
        b.br(env.x.l_stop_in, &[row.carry.out, row.at, err]);
        // SKIPPED: the next row, nothing carried
        b.switch_to(l_ok);
        let sk = b.iconst(Type::I64, WRITE_SKIPPED);
        let is_sk = b.icmp(IntCond::Eq, st, sk);
        let l_nsk = b.create_block(&[]);
        b.cond_br(is_sk, (l_join, &row.carry.params()), (l_nsk, &[]));
        b.switch_to(l_nsk);
        let is_rc = b.icmp(IntCond::Eq, st, rc);
        let (l_rc, l_done) = (b.create_block(&[]), b.create_block(&[]));
        b.mark_cold(l_rc);
        b.cond_br(is_rc, (l_rc, &[]), (l_done, &[]));
        b.switch_to(l_rc);
        match (&self.epq, &self.fam, leaf) {
            (Some(e), _, _) => {
                let c = e.lower(b, env, row, self.site.get(), self.node, &mut *next);
                b.br(l_join, &c.params());
            }
            // lockrows FAMLIMIT: the recheck of the arm whose leaf the row's tableoid names; no arm's is the host's bug
            (None, Some(f), Some(o)) => {
                for (relid, e) in f.arms.iter() {
                    let want = b.iconst(Type::I64, i64::from(*relid));
                    let hit = b.icmp(IntCond::Eq, o, want);
                    let (l_arm, l_next) = (b.create_block(&[]), b.create_block(&[]));
                    b.cond_br(hit, (l_arm, &[]), (l_next, &[]));
                    b.switch_to(l_arm);
                    let c = e.lower(b, env, row, self.site.get(), self.node, &mut *next);
                    b.br(l_join, &c.params());
                    b.switch_to(l_next);
                }
                let uns = b.iconst(Type::I64, unsupported(UNSUPPORTED_SHAPE));
                b.br(env.x.l_stop_in, &[row.carry.out, row.at, uns]);
            }
            _ => crate::ops::lockrows::marks::recheck_exit(b, env, row, self.again, &|b| b.iconst(Type::I64, recheck(src))),
        }
        // DONE: the locked row goes on
        b.switch_to(l_done);
        let c = next(b, env, row);
        b.br(l_join, &c.params());
        b.switch_to(l_join);
        Carry::from_params(b.block_params(l_join), env.mcx)
    }

    /// D3274: a lock inside a SubPlan's inner whose recheck arms InitPlans in its own EvalPlanQual state (rerun/epq/sub_lock.rs
    /// `armed`) ends that state at its inner's input end, as ExecLockRows calls EvalPlanQualEnd there.
    fn ends(&self) -> bool {
        self.epq.as_ref().is_some_and(|e| e.ends())
    }

    fn ended(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        if let Some(e) = &self.epq {
            e.epq_end(b, env);
        }
    }
}
