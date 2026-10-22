//! EPQ (docs/gravity/exec.md): PostgreSQL's read-committed recheck, run as a

use gravity_abi::ScanPos;
use gravity_driver::{Binding, BorrowedBatch, Entry, Exec, Recheck, Rerun, Reservation, RowBuf};
use gravity_parallel::Plan;
use gravity_hosts::HostRegistry;
use gravity_program::{Cmd, LockKind, PlanKey, Program};
use types_error::{PgError, PgResult, ERRCODE_T_R_SERIALIZATION_FAILURE};

use crate::shapes::Admitted;

mod raw;

pub fn must_serialize() -> bool {
    xact::IsolationUsesXactSnapshot()
}

pub fn serialization_failure() -> Box<PgError> {
    Box::new(PgError::error("could not serialize access due to concurrent update".to_string()).with_sqlstate(ERRCODE_T_R_SERIALIZATION_FAILURE)) // alloc: Cold
}

/// D1478: the sites a recheck leaves unsubstituted. PostgreSQL's EvalPlanQual substitutes the rows of the relations its
/// PlanRowMarks name (ExecScanFetch's relsubs_slot / relsubs_rowmark), the top-level query's alone: a SubPlan's or an
/// InitPlan's own scans run again under the statement's snapshot. Those are the lowerer's nest.subplan.rescan sites, and
/// the driving sites of the pipelines it names, of an InitPlan's and of a hashed set's build, with their producers'.
fn rescans(program: &Program) -> Vec<bool> {
    use gravity_program::protos::ProtoKind;
    let mut out = vec![false; program.sites.len()]; // alloc: Cold
    let mut todo = Vec::new(); // alloc: Cold
    for p in program.v4.iter().flat_map(|d| d.protos.iter()) {
        match p.kind {
            ProtoKind::Rescan if p.site >= 0 => out.get_mut(p.site as usize).into_iter().for_each(|o| *o = true),
            ProtoKind::Rescan | ProtoKind::Init | ProtoKind::SubHash => todo.push(p.pipeline as usize), // alloc: Cold
            _ => {}
        }
    }
    let mut seen = vec![false; program.pipelines.len()]; // alloc: Cold
    while let Some(q) = todo.pop() {
        if q >= seen.len() || core::mem::replace(&mut seen[q], true) {
            continue;
        }
        if let Some(o) = usize::try_from(program.pipelines[q].driving_site).ok().and_then(|s| out.get_mut(s)) {
            *o = true;
        }
        todo.extend(program.producers_of(q)); // alloc: Cold
    }
    out
}

/// A4-MATEND (D2371): a spooled drain's spool (the Spool proto naming its probe's breaker slot: lowerv4
/// hashjoin/drain/spool.rs) keeps its count and the drain's state in parameter words, which the recheck node would share
/// with the statement's run: its rerun probe would append to the statement's spool and its drain read it from the first
/// record. Under its own copy the recheck's probe appends past the statement's records (and past the drained row the
/// statement's drain holds there), its drain reads from there with its state reset (the fill's start word, the reader's
/// open: lowerv4 hashjoin/drain/spool.rs), and the statement's run resumes over its own count and words.
fn own_words(program: &Program) -> bool {
    use gravity_program::protos::{ProtoKind, NO_SLOT};
    program.v4.iter().flat_map(|d| d.protos.iter()).any(|p| p.kind == ProtoKind::Spool && p.slot != NO_SLOT)
}

/// mergecmd G2(c) (spool-scoped recheck): the rec.rs spools (a Spool proto naming no slot: nest.spool's Material, a
/// settled union's record) whose fill pipeline the recheck reruns (a producer `Entry::recheck`'s rerun runs: not
/// recheck_skip). Their record count and readers' words are parameter words, which the recheck node would share with the
/// statement's run: the rerun fill would append beside the records the statement already holds and its readers read
/// them all. Under the recheck's own copy of the words each gets an empty spool (ops/material/proto.rs `fresh`), as
/// EvalPlanQualStart's fresh planstate tree has fresh tuplestores; the statement resumes over its own spool and words. A
/// pulled CTE's store (a Cte proto over the same pipeline: its pull state is the statement's) and a rerun set's member
/// (its own re-fill per rescan) keep the statement's.
fn fresh_spools(program: &Program) -> Vec<&gravity_program::protos::Proto> {
    use gravity_program::protos::{ProtoKind, NO_SLOT};
    let protos = || program.v4.iter().flat_map(|d| d.protos.iter());
    let owned = |q: u16| protos().any(|x| matches!(x.kind, ProtoKind::Cte | ProtoKind::Rerun) && x.pipeline == q);
    let rerun = |q: u16| program.pipelines.get(q as usize).is_some_and(|f| f.produces().is_some() && !f.recheck_skip);
    protos().filter(|p| p.kind == ProtoKind::Spool && p.slot == NO_SLOT && rerun(p.peer) && !owned(p.peer)).collect() // alloc: Cold
}

/// EvalPlanQual on the newest version at `pos` (docs/gravity/exec.md): same snapshot, second node, `site` substituted, the others held fixed.
/// `positions`: the sites' positions the failing node settled (`Node::positions`), copied before it was released.
pub fn recheck(entry: &mut Entry, a: &mut Admitted, key: Option<PlanKey>, binding: &mut Binding<'_>, positions: &[ScanPos], site: usize, pos: ScanPos) -> PgResult<u64> {
    recheck_with(entry, a, key, binding, positions, site, pos, None)
}

/// `recheck` for a program of several pipelines (the entry seam's pull, D655): the producers before the stopped
/// pipeline are rerun by `plan` under the statement's admission `lease` (`Entry::recheck`).
pub fn recheck_with(entry: &mut Entry, a: &mut Admitted, key: Option<PlanKey>, binding: &mut Binding<'_>, positions: &[ScanPos], site: usize, pos: ScanPos, plan: Option<(&dyn Plan, std::rc::Rc<Reservation>)>) -> PgResult<u64> {
    let program = a.program.clone();
    recheck_on(entry, &program, &mut a.registry, a.out_natts, a.cmd, key, binding, positions, site, pos, plan)
}

/// D610: EvalPlanQual for a stop in a pipeline of a program of several (gravity_parallel's emit arm, the pull adapter's
/// producers), on the leader as the serial runner's: the rows it processed counted into the binding (D609's count);
/// `Ok(true)` resumes the pipeline past the row. The recheck node runs the stopped pipeline alone (`Entry::recheck`):
/// lowerv4 refuses a rechecking write that reads another pipeline's product (D652) unless its marks rebuild it (D655:
/// `plan` reruns the producers before the stopped pipeline).
pub fn recheck_leader(entry: &mut Entry, exec: &mut Exec, binding: &mut Binding<'_>, registry: &mut HostRegistry, key: Option<PlanKey>, site: u8, plan: Option<&dyn Plan>) -> gravity_driver::Result<bool> {
    let program = exec.node().program().clone();
    // the stop's positions first: a forced InitPlan's run below is another call on this node, whose settle lays the
    // positions again (a nest inner's RowOf mark as that call's own take: D3271, the locked row lost)
    let (pos, positions) = (exec.recheck_pos(), exec.node().positions().to_vec()); // alloc: Cold
    // D2112 phase B: the pending InitPlans the recheck plan reads run first, in this execution (`forced`)
    let cells = forced(&program, binding.parts.params);
    if !cells.is_empty() {
        entry.with_current(exec, binding, || force(&cells)).map_err(gravity_hosts::stash)?;
    }
    let cmd = if program.write.lock != LockKind::None { Cmd::Select } else { program.write.cmd };
    let plan = plan.map(|p| (p, exec.lease()));
    let rows = recheck_on(entry, &program, registry, program.out.len(), cmd, key, binding, &positions, site as usize, pos, plan).map_err(gravity_hosts::stash)?;
    binding.rows_processed += rows;
    Ok(raw::resumes(exec.node().ctx()))
}

/// D2028 / D2112 phase B: PostgreSQL's EvalPlanQualStart (and EvalPlanQualBegin at each later recheck) forces every
/// still-pending InitPlan whose param the recheck plan's extParam holds (ExecSetParamPlanMulti) and copies the values,
/// never the execPlan links: an evaluated InitPlan never runs again (lowerv4 marks its members `recheck_skip`, so the
/// recheck reads this execution's Init cell), and a pending one runs once, before the recheck, in the statement's own
/// execution, so neither the recheck nor a later row runs it again. The cells (`words[base]`) and first params of the Init
/// protos lowerv4 flagged `INIT_RECHECK` (decompose `recheck_inits`); the caller reads each (`init::read_param`, a no-op
/// for one this execution ran) with the execution published (`Entry::with_current`) for the frame's nested run.
pub(crate) fn forced(program: &Program, words: &[u64]) -> Vec<(u64, i32)> {
    use gravity_kernels::ops::shared::object::entries::{init_first, INIT_RECHECK};
    use gravity_program::protos::ProtoKind;
    let flagged = program.v4.iter().flat_map(|d| d.protos.iter()).filter(|p| p.kind == ProtoKind::Init && p.arg & INIT_RECHECK != 0);
    flagged.filter_map(|p| words.get(p.words.base as usize).map(|&cell| (cell, init_first(p.arg)))).collect() // alloc: Cold
}

/// D2112 phase B: `forced`'s cells read in order (a no-op for one this execution ran), under a frame the caller publishes:
/// `recheck_leader` under the runner's (`Entry::with_current`), the entry seam under its producers' (`Producers::with_frame`).
pub(crate) fn force(cells: &[(u64, i32)]) -> PgResult<()> {
    cells.iter().try_for_each(|&(cell, id)| crate::ops::subplan::initplan::init::read_param(cell, id).map(|_| ()))
}

/// D2673: PostgreSQL's EvalPlanQualStart initializes every SubPlan's plan afresh in the recheck's EState (ExecInitSubPlan
/// arms each InitPlan inside one), so the recheck evaluates such an InitPlan again where it first reads it, while the
/// statement's own EState keeps its value. The Init protos lowerv4 flagged `INIT_EPQ_FRESH` (decompose `epq_fresh`), but those
/// a rerun set holds (its run re-arms them at every re-open, the recheck's too: D1262), armed for this recheck (initplan
/// raw.rs `epq_arm`); the caller puts each back after it (`epq_back`).
fn fresh_inits(program: &Program, words: &[u64]) -> Vec<crate::ops::subplan::initplan::raw::Armed> {
    use gravity_kernels::ops::shared::object::entries::INIT_EPQ_FRESH;
    use gravity_program::protos::ProtoKind;
    let protos = program.v4.as_ref().map_or(&[][..], |d| &d.protos[..]);
    let set = |p: &gravity_program::protos::Proto| protos.iter().any(|q| q.kind == ProtoKind::Rerun && q.pipeline == p.pipeline);
    let flagged = protos.iter().filter(|p| p.kind == ProtoKind::Init && p.arg & INIT_EPQ_FRESH != 0 && !set(p));
    flagged.filter_map(|p| words.get(p.words.base as usize).and_then(|&w| crate::ops::subplan::initplan::raw::epq_arm(w))).collect() // alloc: Cold
}

#[allow(clippy::too_many_arguments)]
fn recheck_on(entry: &mut Entry, program: &std::sync::Arc<Program>, registry: &mut HostRegistry, natts: usize, cmd: Cmd, key: Option<PlanKey>, binding: &mut Binding<'_>, positions: &[ScanPos], site: usize, pos: ScanPos, plan: Option<(&dyn Plan, std::rc::Rc<Reservation>)>) -> PgResult<u64> {
    if must_serialize() {
        return Err(serialization_failure());
    }
    let mut values = vec![0u64; natts]; // alloc: Cold
    let mut isnull = vec![0u8; natts]; // alloc: Cold
    // D470: the row's by-reference images, owned here (the recheck node's region is its own again after the recheck)
    let mut region: Vec<u8> = Vec::new(); // alloc: Cold
    let byref: Vec<bool> = program.out.iter().map(|c| !c.typbyval).collect(); // alloc: Cold
    // D2744: a MERGE with RETURNING takes a second surviving row, ExecMerge's pending NOT MATCHED row after a BY SOURCE
    // action's (a MATCHED row whose recheck no longer joins runs both: ExecMergeMatched's switch, mt_merge_pending_not_matched)
    let mut pending = (cmd == Cmd::Merge && natts > 0).then(gravity_driver::Pending::default);
    registry.rewind();
    // D655: the producers the stopped pipeline reads, rerun on the recheck node (runner `rerun_producer`)
    let mut run;
    let rerun = match plan {
        Some((plan, lease)) => {
            run = move |e: &mut Entry, x: &mut Exec, b: &mut Binding<'_>| crate::runner::rerun_producer(e, x, b, plan);
            Some(Rerun { lease, run: &mut run, positions })
        }
        None => None,
    };
    let rescan = rescans(program);
    // A4-MATEND (D2371): a program with a spooled drain rechecks under its own copy of the parameter words, as
    // EvalPlanQualStart copies the parent's es_param_exec_vals (`own_words`)
    // mergecmd G2(c): and a recheck that reruns a spool's fill, each such spool empty in the copy (`fresh_spools`)
    let fresh = fresh_spools(program);
    // mergecmd tw-core-197 (D1933 MEMO): and every memo (a Cache proto) an empty one in the copy, as EvalPlanQualStart's
    // fresh Memoize has an empty cache (ops/memoize/proto.rs `fresh`)
    let memos: Vec<&gravity_program::protos::Proto> = program.v4.iter().flat_map(|d| d.protos.iter()).filter(|p| p.kind == gravity_program::protos::ProtoKind::Cache).collect(); // alloc: Cold
    let copied = own_words(program) || !fresh.is_empty() || !memos.is_empty();
    let (mut words, mut nulls) = if copied { (binding.parts.params.to_vec(), binding.parts.param_nulls.to_vec()) } else { (Vec::new(), Vec::new()) }; // alloc: Cold
    // the recheck's own spools, alive until it returns (the copy covers every word they name)
    let need = fresh.iter().chain(memos.iter()).map(|p| p.words.base as usize + p.words.n as usize).max().unwrap_or(0);
    if words.len() < need {
        words.resize(need, 0); // alloc: Cold
        nulls.resize(need, 0); // alloc: Cold
    }
    let _spools = fresh.iter().map(|p| crate::ops::material::proto::fresh(p, &mut words, &mut nulls)).collect::<PgResult<Vec<_>>>()?; // alloc: Cold
    let _memos = memos.iter().map(|p| crate::ops::memoize::proto::fresh(p, &mut words, &mut nulls)).collect::<PgResult<Vec<_>>>()?; // alloc: Cold
    let own = Binding { parts: gravity_driver::BindingParts { params: &words, param_nulls: &nulls, ..binding.parts }, sink: binding.sink, rows_processed: 0 };
    let held: &Binding<'_> = if copied { &own } else { binding };
    // D2673: the InitPlans EvalPlanQualStart arms afresh (below the write or lock) run again where this recheck first reads
    // them, and the statement's run keeps its own values (`fresh_inits`)
    let armed = fresh_inits(program, binding.parts.params);
    let r = entry.recheck(
        key,
        program,
        registry,
        held,
        &mut |i, s| {
            if i == site {
                s.substitute(pos);
            } else if positions.get(i).is_some_and(|p| p.is_valid()) && !rescan.get(i).copied().unwrap_or(false) {
                s.substitute(positions[i]);
            }
        },
        RowBuf { values: &mut values[..natts], isnull: &mut isnull[..natts], region: &mut region, byref: &byref },
        pending.as_mut(),
        rerun,
        // mergecmd 9 (COPYROW): the stopped node's COPY rows for a recheck without producers (D732)
        positions,
    );
    armed.into_iter().for_each(crate::ops::subplan::initplan::raw::epq_back);
    match r.map_err(crate::glue::pg_error)? {
        (Recheck::Skip, _) => Ok(0),
        (Recheck::Pass, rows) => {
            // the surviving row out: a lock's, or a RETURNING write's (its out columns)
            if natts > 0 {
                binding.sink.push(BorrowedBatch { natts, rows: 1, values: &values[..natts], isnull: &isnull[..natts], region: &region }).map_err(crate::glue::pg_error)?;
            }
            // D2744: the pending NOT MATCHED action's row after it, as ExecModifyTable's next call returns it
            if let Some(p) = pending.as_ref().filter(|p| p.got && p.values.len() >= natts) {
                binding.sink.push(BorrowedBatch { natts, rows: 1, values: &p.values[..natts], isnull: &p.isnull[..natts], region: &p.region }).map_err(crate::glue::pg_error)?;
            }
            Ok(if cmd == Cmd::Select { rows.max(1) } else { rows })
        }
    }
}
