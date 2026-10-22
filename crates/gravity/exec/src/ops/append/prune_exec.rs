//! L0-k (M8-plan §3.7, cards/M8.md L0-k): `src.prune`'s exec pruning when its params are in-program InitPlans'
//! (nest.initplan's Init, run at their first read). PostgreSQL evaluates them where its Append first runs
//! (`ExecAppend` -> `ExecFindMatchingSubPlans` -> `ExecEvalParamExec` -> `ExecSetParamPlan`): an Append that never runs
//! never runs them. So the decision is not the prepare's: the proto's first arm to start decides (D899's late step,
//! `Live::late_words`), reading each Init param through its cell (`init::read_param`, the Init's members run nested then
//! unless they ran earlier), then every arm binds the skip words under its own image, and a pruned arm claims nothing.
//! An InitPlan run by `run_inits` (a graph one) is read from its word as m2.rs does. Created by M8-RUNTIME.
use std::cell::Cell;

use gravity_parallel::MorselSource;
use gravity_program::protos::{Proto, ProtoKind, NONE};
use types_error::PgResult;
use types_nodes::plannodes::PlannedStmt;
use types_portal::ParamListHandle;

use crate::ops::append::proto::{NoClaims, PruneArg, SkipWords};
use super::super::{Live, Prepare};
use crate::glue;
use crate::ops::append::prune;
use crate::ops::append::prune::{ExecPrune, ExecWord, EXEC_WORDS};

/// `src.prune`'s exec pruning over at least one in-program Init param, decided at its first arm's start.
struct LatePrune {
    pstmt: &'static PlannedStmt<'static>,
    prune: ExecPrune,
    arg: PruneArg,
    /// The proto's first arm's pipeline: arm `i` is pipeline `pipeline + i`.
    pipeline: usize,
    /// A pull-form merge's reader (the proto's `peer`), whose start takes the skip words too: it reads the arms itself.
    reader: Option<usize>,
    /// The skip words' first word and count.
    base: usize,
    n: usize,
    /// The graph InitPlans' words (`run_inits` ran them at the open).
    known: memledger::ChargedVec<ExecWord>,
    /// (exec param, Init cell) of each in-program InitPlan the exec steps read.
    cells: memledger::ChargedVec<(i32, u64)>,
    decided: Cell<bool>,
    skip: SkipWords,
}

/// The prune's live if an in-program Init answers any of its exec params, else `None` (m2.rs decides at the prepare). An
/// exec param whose Init cell is not laid yet (its Init proto declared after this one) errors by name.
pub(crate) fn late(p: &Proto, a: PruneArg, pstmt: &'static PlannedStmt<'static>, params: ParamListHandle, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
    let inits = cx.program.v4.as_ref().map_or(&[][..], |d| &d.protos[..]);
    if !inits.iter().any(|x| x.kind == ProtoKind::Init) {
        return Ok(None);
    }
    let mut cells: memledger::ChargedVec<(i32, u64)> = cx.charged_vec();
    each_exec_param(pstmt, a.index, |x| {
        if cells.iter().any(|c| c.0 == x) {
            return Ok(());
        }
        let owner = inits.iter().find(|i| i.kind == ProtoKind::Init && gravity_kernels::ops::shared::object::entries::init_holds(i.arg, x));
        let Some(i) = owner else { return Ok(()) };
        let cell = cx.words.get(i.words.base as usize).copied().unwrap_or(0);
        if cell == 0 {
            return Err(glue::internal(&format!("gravity: src.prune reads exec param {x}, whose InitPlan's Init is declared after the prune (L0-k)"))); // alloc: Cold
        }
        cells.with_vec(|v| v.push((x, cell))); // alloc: Exec charged cx.owner
        Ok(())
    })?;
    if cells.is_empty() {
        return Ok(None);
    }
    let mut known: memledger::ChargedVec<ExecWord> = cx.charged_vec();
    known.with_vec(|v| v.extend(cx.admitted.graph.as_ref().map_or(&[][..], |g| &g.inits[..]).iter().filter_map(|i| Some(ExecWord { param: i.param, value: *cx.words.get(i.word)?, null: *cx.nulls.get(i.word)? })))); // alloc: Exec charged cx.owner // alloc: Exec
    if known.len() + cells.len() > EXEC_WORDS {
        return Err(glue::internal("gravity: src.prune over more InitPlans than prune::EXEC_WORDS"));
    }
    let (base, n) = (p.words.base as usize, p.words.n as usize);
    // until the decision nothing reads the words (each arm binds its own at its start)
    cx.words[base..base + n].iter_mut().for_each(|w| *w = 0);
    cx.nulls.get_mut(base..base + n).into_iter().flatten().for_each(|b| *b = 0);
    let prune = ExecPrune { index: a.index as u32, nsubplans: a.nsubplans, params };
    Ok(Some(Box::new(LatePrune { pstmt, prune, arg: a, pipeline: p.pipeline as usize, reader: (p.peer != NONE).then_some(p.peer as usize), base, n, known, cells, decided: Cell::new(false), skip: SkipWords::zeroed(n, cx.charged_vec()) }))) // alloc: Exec
}

/// The PARAM_EXEC ids the prune info's exec steps read (every partitioned rel's `execparamids`), in order, repeats included:
/// the caller dedups against what it has kept.
fn each_exec_param(pstmt: &'static PlannedStmt<'static>, index: u16, mut f: impl FnMut(i32) -> PgResult<()>) -> PgResult<()> {
    let Some(info) = pstmt.partPruneInfos.nth(index as usize).as_partition_prune_info() else { return Ok(()) };
    for l in info.prune_infos.iter().filter_map(|l| l.as_list()) {
        for r in l.iter().filter_map(|r| r.as_partitioned_rel_prune_info()) {
            let mut x = r.execparamids.next_member(-1);
            while x >= 0 {
                f(x)?;
                x = r.execparamids.next_member(x);
            }
        }
    }
    Ok(())
}

impl LatePrune {
    /// Arm `pipeline`'s index in the proto, if it is one of its arms.
    fn arm(&self, pipeline: usize) -> Option<usize> {
        pipeline.checked_sub(self.pipeline).filter(|&i| i < self.arg.count as usize)
    }

    /// The decision: every Init param read (run now, nested, if this execution has not run it), then the exec steps.
    fn decide(&self) -> PgResult<()> {
        let mut words = [ExecWord { param: -1, value: 0, null: 1 }; EXEC_WORDS];
        words[..self.known.len()].copy_from_slice(&self.known);
        for (k, &(param, cell)) in self.cells.iter().enumerate() {
            let (value, null) = crate::ops::subplan::initplan::init::read_param(cell, param)?;
            words[self.known.len() + k] = ExecWord { param, value, null };
        }
        let valid = prune::exec_valid(self.pstmt, self.prune, &words[..self.known.len() + self.cells.len()])?;
        fires::fire("append.exec_prune"); // shapecheck NEW-kernels-exec-121: the late (in-program InitPlan) exec-time arm skips (L0-k)
        self.skip.decide(&valid, self.arg);
        self.decided.set(true);
        if crate::inputs::knobs::seam::report_on() {
            let pruned: Vec<usize> = (0..self.arg.count as usize).filter(|&i| self.skip.skipped(i)).map(|i| self.pipeline + i).collect(); // alloc: Cold
            let _ = elog::ereport(types_error::LOG).errmsg(format!("gravity: src.prune pruned {} of {} arms (pipelines {pruned:?}) at the first arm's start", pruned.len(), self.arg.count)).errhidestmt(true).errhidecontext(true).finish(types_error::ErrorLocation::new("gravity", 0, "prune")); // alloc: Cold
        }
        Ok(())
    }
}

impl Live for LatePrune {
    fn answers(&self) -> u16 {
        crate::ops::ask::CLAIMS | crate::ops::ask::LATE | crate::ops::ask::PRUNED
    }

    fn claims_for(&self, pipeline: usize) -> Option<&dyn MorselSource> {
        self.pruned_whole(pipeline).then_some(&NoClaims as &dyn MorselSource)
    }

    /// D2584: an arm the decision pruned runs whole empty from then on (a serial source, which takes no claims, never walks
    /// it), as PostgreSQL's ExecAppend never runs an arm ExecFindMatchingSubPlans left out.
    fn pruned_whole(&self, pipeline: usize) -> bool {
        self.arm(pipeline).is_some_and(|i| self.decided.get() && self.skip.skipped(i))
    }

    fn late_words(&self, pipeline: usize, out: &mut gravity_parallel::LateWords) -> PgResult<()> {
        if self.arm(pipeline).is_none() && self.reader != Some(pipeline) {
            return Ok(());
        }
        if !self.decided.get() {
            self.decide()?;
        }
        out.with_vec(|v| (0..self.n).for_each(|k| v.push((self.base + k, self.skip.word(k), 0)))); // alloc: Exec charged out
        Ok(())
    }
}
