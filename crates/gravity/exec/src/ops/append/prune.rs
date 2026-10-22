//! Partition pruning as PostgreSQL's own verdict over a throwaway executor state, once per execution (rung12-subplan.md fifth / seventh landings): initial at bind (`initial_valid`), exec after the InitPlans filled their words (`exec_skip`), never per row. A rerun set's exec pruning, decided again at each rerun, keeps one state over the execution instead (`ExecHold`).
use core::sync::atomic::{AtomicUsize, Ordering};

use executils::{create_executor_state, free_executor_state, EStateData};
use types_error::PgResult;
use execpartition::pruning::PartitionPruneState;
use gravity_program::cost::runtime::PRUNE_PINS_MAX as MAX_PINS;
use types_nodes::bitmapset::Bitmapset;
use types_nodes::plannodes::{PartitionPruneInfo, PlannedStmt};
use types_portal::params::ParamExecData;
use types_portal::ParamListHandle;

use crate::shapes::Admitted;

/// The subplans an Append keeps, by the plan's subplan index: every one (no steps ran), else a bit per subplan up to its
/// highest kept one. D3017: sized from the members, so an Append of any arm count prunes (a fixed four words held 256
/// subplans, and a 257-way Append's src.prune refused at run time).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArmSet {
    All,
    Of(Box<[u64]>),
}

pub const ALL: ArmSet = ArmSet::All;

pub fn member(set: &ArmSet, i: usize) -> bool {
    match set {
        ArmSet::All => true,
        ArmSet::Of(w) => w.get(i / 64).is_some_and(|w| w >> (i % 64) & 1 != 0),
    }
}

/// The subplans of `pstmt.partPruneInfos[index]` surviving initial pruning under `params` (all, without initial steps); one executor state per call (`// alloc: Bind`).
/// D2578: the set this execution's `decide` holds for the plan and `params`, when one does (no step evaluated again).
pub fn initial_valid(pstmt: &'static PlannedStmt<'static>, index: usize, params: ParamListHandle) -> PgResult<ArmSet> {
    let valid = match decided(pstmt, index, params) {
        Some(valid) => valid,
        None => over_state(pstmt, params, |es| {
            execpartition::pruning::exec_do_initial_pruning(es)?;
            Ok(match es.es_part_prune_results.get(index).and_then(|r| r.as_ref()) {
                Some(valid) => set_of(members_of(valid)),
                None => ALL,
            })
        })?,
    };
    if PINNED.load(Ordering::Relaxed) != 0 {
        check_pin(pstmt, index, &valid)?;
    }
    Ok(valid)
}

/// D2578: an execution's initial pruning, decided once at its start (`runner::held::take`, the served ExecutorStart,
/// under the statement's snapshot) as PostgreSQL's ExecDoInitialPruning decides it at InitPlan. While the run holds the
/// decision, every `initial_valid` of the same plan and parameters (src.prune at the prepare, a rerun set, the
/// admission's arms) reads its set and evaluates no step again: a step's STABLE function runs once per execution, as
/// PostgreSQL's. The newest decision of a plan and parameters answers (a nested run of the same plan holds its own).
static DECIDING: AtomicUsize = AtomicUsize::new(0);
static DECISIONS: pgsync::Mutex<Vec<Decision>> = pgsync::Mutex::new(Vec::new()); // alloc: Cold
static NEXT_DECISION: AtomicUsize = AtomicUsize::new(1);

struct Decision {
    id: usize,
    pstmt: usize,
    params: u64,
    valid: Vec<ArmSet>,
    /// D2585: ExecDoInitialPruning's own results, which an exec pruning of the run starts from (`exec_valid`): per info
    /// the surviving subplans (`None`: no initial steps), and the range-table entries left unpruned.
    results: Vec<Option<Vec<i32>>>,
    unpruned: Vec<u32>,
}

/// A run's hold on its initial pruning decision (`runner::held::Held`): dropped with the run (an error's return and an
/// unwind drop it too), which takes the decision out.
#[derive(Debug)]
pub struct Decided(usize);

impl Drop for Decided {
    fn drop(&mut self) {
        let mut ds = DECISIONS.lock().unwrap_or_else(|e| e.into_inner());
        ds.retain(|d| d.id != self.0);
        DECIDING.store(ds.len(), Ordering::Relaxed);
    }
}

/// D761 / D2578: this execution's initial pruning under `params`, one ExecDoInitialPruning: the hold on its per-info
/// sets (`initial_valid` reads them while it lives) and PostgreSQL's `es_unpruned_relids`, the range-table entries whose
/// nodes ExecInitNode initializes, so a served run holds the relations PostgreSQL's executor holds (`runner::held`).
pub fn decide(pstmt: &'static PlannedStmt<'static>, params: ParamListHandle) -> PgResult<(Decided, Vec<u32>)> {
    let (results, unpruned) = over_state(pstmt, params, |es| {
        execpartition::pruning::exec_do_initial_pruning(es)?;
        let results = es.es_part_prune_results.iter().map(|r| r.as_ref().map(members_of)).collect::<Vec<_>>(); // alloc: Bind
        Ok((results, es.es_unpruned_relids.iter().map(|i| i as u32).collect::<Vec<_>>())) // alloc: Bind
    })?;
    let valid = results.iter().map(|r| r.as_ref().map_or(ALL, |v| set_of(v.iter().copied()))).collect::<Vec<_>>(); // alloc: Bind
    let id = NEXT_DECISION.fetch_add(1, Ordering::Relaxed);
    let mut ds = DECISIONS.lock().unwrap_or_else(|e| e.into_inner());
    ds.push(Decision { id, pstmt: key_of(pstmt), params: params.0, valid, results, unpruned: unpruned.clone() }); // alloc: Bind
    DECIDING.store(ds.len(), Ordering::Relaxed);
    Ok((Decided(id), unpruned))
}

/// A bitmapset's members, in order.
fn members_of(b: &types_nodes::bitmapset::Bitmapset<'_>) -> Vec<i32> {
    let mut v = Vec::new(); // alloc: Exec
    let mut i = b.next_member(-1);
    while i >= 0 {
        v.push(i); // alloc: Exec
        i = b.next_member(i);
    }
    v
}

/// D2585: the held decision's ExecDoInitialPruning results for the plan and `params` (`None`: no run holds one).
fn decided_results(pstmt: &'static PlannedStmt<'static>, params: ParamListHandle) -> Option<(Vec<Option<Vec<i32>>>, Vec<u32>)> {
    if DECIDING.load(Ordering::Relaxed) == 0 {
        return None;
    }
    let key = key_of(pstmt);
    let ds = DECISIONS.lock().unwrap_or_else(|e| e.into_inner());
    ds.iter().rev().find(|d| d.pstmt == key && d.params == params.0).map(|d| (d.results.clone(), d.unpruned.clone())) // alloc: Bind
}

/// The held decision for the plan and `params` as the prune getters read it (`initial_results`' answer, with no
/// second ExecDoInitialPruning): an EXPLAIN without ANALYZE prints the pruning its own start decided.
pub fn decided_initial(pstmt: &'static PlannedStmt<'static>, params: ParamListHandle) -> Option<Initial> {
    let (valid, unpruned) = decided_results(pstmt, params)?;
    Some(Initial { valid, unpruned: unpruned.into_iter().map(|r| r as i32).collect() }) // alloc: Cold
}

/// The held decision's set of `partPruneInfos[index]` for the plan and `params` (`None`: no run holds one).
fn decided(pstmt: &'static PlannedStmt<'static>, index: usize, params: ParamListHandle) -> Option<ArmSet> {
    if DECIDING.load(Ordering::Relaxed) == 0 {
        return None;
    }
    let key = key_of(pstmt);
    let ds = DECISIONS.lock().unwrap_or_else(|e| e.into_inner());
    ds.iter().rev().find(|d| d.pstmt == key && d.params == params.0).map(|d| d.valid.get(index).cloned().unwrap_or(ALL))
}

/// EXPLAIN ANALYZE's proof that the pruning it prints is the run's (entry.md §4): the entry pins its
/// `Initial` for the plan before the admission, every `initial_valid` over that plan (the admission's composed arms, the
/// v4 program's `src.prune`) checks its set against the pin (a difference is an internal error in the run) and counts
/// the check, and the entry takes the counts back at ExecutorFinish (`unpin`). Unpinned (anything but EXPLAIN ANALYZE
/// of a served plan with run-time pruning), a call costs one relaxed load.
static PINNED: AtomicUsize = AtomicUsize::new(0);
static PINS: pgsync::Mutex<Vec<Pin>> = pgsync::Mutex::new(Vec::new()); // alloc: Cold

struct Pin {
    pstmt: usize,
    valid: Vec<ArmSet>,
    checked: Vec<u32>,
}

/// Pins `initial` for `pstmt` (replacing an earlier pin of the same plan).
pub fn pin(pstmt: &'static PlannedStmt<'static>, initial: &Initial) {
    let valid = initial.valid.iter().map(|v| v.as_ref().map_or(ALL, |v| set_of(v.iter().copied()))).collect::<Vec<_>>(); // alloc: Cold
    pin_key(key_of(pstmt), valid);
}

fn key_of(pstmt: &'static PlannedStmt<'static>) -> usize {
    pstmt as *const PlannedStmt<'static> as usize
}

fn pin_key(key: usize, valid: Vec<ArmSet>) {
    let mut pins = PINS.lock().unwrap_or_else(|e| e.into_inner());
    pins.retain(|p| p.pstmt != key);
    if pins.len() >= MAX_PINS {
        pins.remove(0);
    }
    let n = valid.len();
    pins.push(Pin { pstmt: key, valid, checked: vec![0; n] }); // alloc: Cold
    PINNED.store(pins.len(), Ordering::Relaxed);
}

/// The pin of `pstmt` removed: how many times the run checked each info against it (`None`: not pinned).
pub fn unpin(pstmt: &'static PlannedStmt<'static>) -> Option<Vec<u32>> {
    unpin_key(key_of(pstmt))
}

fn unpin_key(key: usize) -> Option<Vec<u32>> {
    let mut pins = PINS.lock().unwrap_or_else(|e| e.into_inner());
    let at = pins.iter().position(|p| p.pstmt == key)?;
    let pin = pins.remove(at);
    PINNED.store(pins.len(), Ordering::Relaxed);
    Some(pin.checked)
}

fn check_pin(pstmt: &'static PlannedStmt<'static>, index: usize, valid: &ArmSet) -> PgResult<()> {
    check_key(key_of(pstmt), index, valid)
}

fn check_key(key: usize, index: usize, valid: &ArmSet) -> PgResult<()> {
    let mut pins = PINS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(pin) = pins.iter_mut().find(|p| p.pstmt == key) else { return Ok(()) };
    match pin.valid.get(index) {
        Some(pinned) if pinned == valid => {
            pin.checked[index] += 1;
            Ok(())
        }
        _ => Err(crate::glue::internal(&format!("gravity: EXPLAIN ANALYZE's initial pruning of partPruneInfos[{index}] differs from the run's"))), // alloc: Cold
    }
}

/// PostgreSQL's initial pruning results for EXPLAIN ANALYZE of a served statement (entry.md §4): per `partPruneInfos`
/// entry, the surviving subplan indexes (`es_part_prune_results`; `None` = no initial steps, every subplan kept) and the
/// range-table indexes left unpruned (`es_unpruned_relids`, the plan's unprunable ones included).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Initial {
    pub valid: Vec<Option<Vec<i32>>>,
    pub unpruned: Vec<i32>,
}

/// `Initial` under `params`: `ExecDoInitialPruning` in one throwaway state, the steps and parameters `initial_valid` gives
/// the served program's `src.prune` (the entry's answer to the prune getters; `// alloc: Cold`, EXPLAIN ANALYZE only).
pub fn initial_results(pstmt: &'static PlannedStmt<'static>, params: ParamListHandle) -> PgResult<Initial> {
    over_state(pstmt, params, |es| {
        execpartition::pruning::exec_do_initial_pruning(es)?;
        let members = |b: &types_nodes::bitmapset::Bitmapset<'_>| {
            let mut v = Vec::new(); // alloc: Cold
            let mut i = b.next_member(-1);
            while i >= 0 {
                v.push(i); // alloc: Cold
                i = b.next_member(i);
            }
            v
        };
        Ok(Initial { valid: es.es_part_prune_results.iter().map(|r| r.as_ref().map(members)).collect(), unpruned: members(&es.es_unpruned_relids) }) // alloc: Cold
    })
}

/// An InitPlan's word after `run_inits`: PostgreSQL's param id, the value and null byte.
#[derive(Clone, Copy)]
pub struct ExecWord {
    pub param: i32,
    pub value: u64,
    pub null: u8,
}

/// A composed Append's exec pruning (`GraphWords.prune`): the info's index, its subplan count (main + arms), the execution's extern parameters (its initial steps run first in the same state; `runner::eval_keys` sets the handle each execution — the admission's is the first portal's, freed with it).
#[derive(Clone, Copy)]
pub struct ExecPrune {
    pub index: u32,
    pub nsubplans: u16,
    pub params: ParamListHandle,
}

/// Whether `info` carries exec pruning steps in any of its partitioned rels.
pub fn has_exec_steps(info: &PartitionPruneInfo<'_>) -> bool {
    info.prune_infos.iter().any(|l| l.as_list().is_some_and(|l| l.iter().any(|r| r.as_partitioned_rel_prune_info().is_some_and(|r| !r.exec_pruning_steps.is_nil()))))
}

/// The subplans surviving initial AND exec pruning: `initial_valid`'s steps, then `ExecInitPartitionExecPruning` / `ExecFindMatchingSubPlans` over `es_param_exec_vals` filled from `words`; one state per call (`// alloc: Bind`).
/// D2585: the initial results are the run's held decision when one is held (`decide`), so no initial step runs twice
/// in an execution (PostgreSQL's ExecInitPartitionExecPruning reads ExecDoInitialPruning's results); else evaluated here.
pub fn exec_valid(pstmt: &'static PlannedStmt<'static>, prune: ExecPrune, words: &[ExecWord]) -> PgResult<ArmSet> {
    over_state(pstmt, prune.params, |es| match exec_setup(es, pstmt, prune, words)? {
        Some((mut state, initially)) => exec_find(es, state.as_mut(), &initially),
        None => Ok(ALL),
    })
}

/// `exec_valid`'s state up to the first find: the initial results (D2585: the run's held decision when one is held, else
/// evaluated here), the exec parameters' array sized before the steps compile (their contexts point into it) and filled
/// from `words`, then PostgreSQL's exec pruning state and the subplans initial pruning kept; `None` without the info.
#[allow(clippy::type_complexity)]
fn exec_setup<'mcx>(es: &mut EStateData<'mcx>, pstmt: &'static PlannedStmt<'static>, prune: ExecPrune, words: &[ExecWord]) -> PgResult<Option<(Option<PartitionPruneState<'mcx>>, Bitmapset<'mcx>)>> {
    match decided_results(pstmt, prune.params) {
        Some((results, unpruned)) => {
            let mcx = es.es_query_cxt;
            for r in results {
                let set = match r {
                    Some(members) => {
                        let mut b = Bitmapset::empty();
                        members.iter().try_for_each(|&i| b.add_member(mcx, i))?;
                        Some(b)
                    }
                    None => None,
                };
                es.es_part_prune_results.push(set);
            }
            unpruned.iter().try_for_each(|&i| es.es_unpruned_relids.add_member(mcx, i as i32))?;
        }
        None => execpartition::pruning::exec_do_initial_pruning(es)?,
    }
    // EXPLAIN ANALYZE's pin: the initial set this exec pruning starts from is the run's initial pruning of the info
    // (`initial_valid`'s check; an exec pruning decided at the first arm's start, `prune_exec::late`, makes no other)
    if PINNED.load(Ordering::Relaxed) != 0 {
        let index = prune.index as usize;
        let valid = match es.es_part_prune_results.get(index).and_then(|r| r.as_ref()) {
            Some(valid) => set_of(members_of(valid)),
            None => ALL,
        };
        check_pin(pstmt, index, &valid)?;
    }
    let pstmt = es.es_plannedstmt.expect("set by over_state");
    let mcx = es.es_query_cxt;
    let n_exec = pstmt.paramExecTypes.len();
    // sized before the steps compile: their contexts point into this array
    es.es_param_exec_vals.try_reserve_exact(n_exec).map_err(|_| mcx.oom(n_exec))?;
    es.es_param_exec_vals.extend(core::iter::repeat_n(ParamExecData::EMPTY, n_exec));
    set_words(es, words);
    let Some(info) = pstmt.partPruneInfos.nth(prune.index as usize).as_partition_prune_info() else { return Ok(None) };
    Ok(Some(execpartition::pruning::exec_init_partition_exec_pruning(es, prune.nsubplans as i32, prune.index as i32, &info.relids)?))
}

/// The exec parameters `words` into the state's array, in place (the compiled steps point into it).
fn set_words(es: &mut EStateData<'_>, words: &[ExecWord]) {
    for w in words.iter() {
        if let Some(cell) = usize::try_from(w.param).ok().and_then(|i| es.es_param_exec_vals.get_mut(i)) {
            *cell = ParamExecData { value: datum::Datum::from_u64(w.value), isnull: w.null != 0, exec_plan: false };
        }
    }
}

/// `ExecFindMatchingSubPlans` over the state's current parameters, as the plan's subplan indexes (the arms' order).
fn exec_find<'mcx>(es: &mut EStateData<'mcx>, state: Option<&mut PartitionPruneState<'mcx>>, initially: &Bitmapset<'mcx>) -> PgResult<ArmSet> {
    let Some(state) = state else { return Ok(set_of(members_of(initially))) };
    let valid = execpartition::pruning::exec_find_matching_subplans(state, es, false, None)?;
    // `InitExecPartitionPruneContexts` re-sequenced the subplans initial pruning kept (the k-th kept is k): back to the plan's indexes, the arms' order
    let (mut k, mut i) = (0i32, initially.next_member(-1));
    let kept = core::iter::from_fn(move || {
        while i >= 0 {
            let (at, cur) = (k, i);
            k += 1;
            i = initially.next_member(i);
            if valid.is_member(at) {
                return Some(cur);
            }
        }
        None
    });
    Ok(set_of(kept))
}

::mcx::bind!(HoldTy => Hold<'mcx>);

/// `ExecHold`'s state, in its own executor state's context.
struct Hold<'mcx> {
    es: EStateData<'mcx>,
    /// The range table's relations are open (closed at the drop).
    opened: bool,
    /// `None` until the setup ran, then the pruning state (`None` inside: no exec steps) and the subplans initially kept.
    #[allow(clippy::type_complexity)]
    pruning: Option<(Option<PartitionPruneState<'mcx>>, Bitmapset<'mcx>)>,
    /// The info is not in the plan: every arm runs.
    missing: bool,
}

/// One exec pruning decided again and again over one execution (a rerun set's, PostgreSQL's ExecReScanAppend ->
/// ExecFindMatchingSubPlans at each rescan): the state `exec_valid` builds once per call is built once here and each
/// call only sets the parameters and finds the subplans, as PostgreSQL's PartitionPruneState built at ExecInitAppend
/// serves every rescan. The state's context is a bump arena (each find's sets stay in it), so the hold says `spent`
/// after `HOLD_CALLS` finds and its owner builds a fresh one: memory stays bounded over any number of reruns.
pub struct ExecHold {
    owned: ::mcx::McxOwned<HoldTy>,
    calls: u32,
    _root: gravity_driver::mem::root::Root,
}

/// Finds per hold before it is rebuilt (bounds the bump arena; one rebuild costs what one `exec_valid` call did).
/// CHOSEN, not measured: a find's sets are a few hundred bytes, so a hold stays under about a megabyte.
const HOLD_CALLS: u32 = 4096;

impl ExecHold {
    pub fn new(pstmt: &'static PlannedStmt<'static>, prune: ExecPrune, words: &[ExecWord]) -> PgResult<ExecHold> {
        let root = gravity_driver::mem::root::Root::new("GravityPruneHold");
        let owned = ::mcx::McxOwned::<HoldTy>::try_new(root.mcx().context().new_child_bump("ExecutorState"), |mcx| Ok(Hold { es: EStateData::new_in(mcx), opened: false, pruning: None, missing: false }))?;
        let mut hold = ExecHold { owned, calls: 0, _root: root };
        hold.owned.with_mut(|h| -> PgResult<()> {
            let plan: &PlannedStmt<'_> = super::raw::state_plan(pstmt);
            if !prune.params.is_null() {
                h.es.es_param_list_info = Some(super::raw::state_params(prune.params));
                h.es.es_param_fetch = types_portal::params::fetch_hook(prune.params);
            }
            let unpruned = plan.unprunableRelids.clone_in(h.es.es_query_cxt)?;
            h.es.exec_init_range_table(&plan.rtable, &plan.permInfos, unpruned)?;
            h.opened = true;
            h.es.es_plannedstmt = Some(plan);
            match exec_setup(&mut h.es, pstmt, prune, words)? {
                Some(p) => h.pruning = Some(p),
                None => h.missing = true,
            }
            Ok(())
        })?;
        Ok(hold)
    }

    /// The subplans surviving under `words` (every exec parameter of the steps).
    pub fn valid(&mut self, words: &[ExecWord]) -> PgResult<ArmSet> {
        self.calls += 1;
        self.owned.with_mut(|h| {
            if h.missing {
                return Ok(ALL);
            }
            set_words(&mut h.es, words);
            let Some((state, initially)) = h.pruning.as_mut() else { return Ok(ALL) };
            exec_find(&mut h.es, state.as_mut(), initially)
        })
    }

    /// Whether the owner should build a fresh hold (`HOLD_CALLS`).
    pub fn spent(&self) -> bool {
        self.calls >= HOLD_CALLS
    }
}

impl Drop for ExecHold {
    fn drop(&mut self) {
        self.owned.with_mut(|h| {
            h.pruning = None;
            if h.opened {
                let _ = h.es.exec_close_range_table_relations();
            }
            h.es.teardown();
        });
    }
}

/// The InitPlan words one `exec_skip` carries (admission refuses more by name: a missing word would read as a non-null zero).
pub const EXEC_WORDS: usize = 64;

/// The arm skips recomputed after `run_inits` (the entry's one call per execution, after every `run_inits`): the arms the exec steps rule out do not run; the main (subplan 0) always runs.
pub fn exec_skip(a: &mut Admitted, kparams: &[u64], knulls: &[u8]) -> PgResult<()> {
    let Some(pstmt) = a.pstmt else { return Ok(()) };
    let Some(g) = a.graph.as_mut() else { return Ok(()) };
    // `nsubplans` 0: a v4 program's parameter handle alone (V4R-37 b; admit.rs `v4_prune_handle`): its src.prune protocol prunes
    let Some(prune) = g.prune.filter(|p| p.nsubplans > 0) else { return Ok(()) };
    let mut words = [ExecWord { param: -1, value: 0, null: 1 }; EXEC_WORDS];
    let mut n = 0usize;
    for i in g.inits.iter() {
        if n < words.len() && i.word < kparams.len() {
            words[n] = ExecWord { param: i.param, value: kparams[i.word], null: knulls.get(i.word).copied().unwrap_or(0) };
            n += 1;
        }
    }
    let valid = exec_valid(pstmt, prune, &words[..n])?;
    fires::fire("append.exec_prune"); // shapecheck NEW-kernels-exec-121: the exec-time arm skips are decided
    for (i, arm) in g.arms.iter_mut().enumerate() {
        arm.skip = !member(&valid, i + 1);
    }
    Ok(())
}

/// The set of the subplan indexes `members` (negative ones ignored), its words up to the highest.
fn set_of(members: impl IntoIterator<Item = i32>) -> ArmSet {
    let mut w: Vec<u64> = Vec::new(); // alloc: Exec
    for i in members.into_iter().filter(|&i| i >= 0).map(|i| i as usize) {
        if w.len() <= i / 64 {
            w.resize(i / 64 + 1, 0); // alloc: Exec
        }
        w[i / 64] |= 1 << (i % 64);
    }
    ArmSet::Of(w.into_boxed_slice())
}

/// A throwaway executor state over the plan's range table, `params` its extern parameters, freed after `f` (`// alloc: Bind`).
fn over_state<R>(pstmt: &'static PlannedStmt<'static>, params: ParamListHandle, f: impl FnOnce(&mut EStateData<'_>) -> PgResult<R>) -> PgResult<R> {
    let ctx = gravity_driver::mem::root::Root::new("GravityPrune"); // alloc: Bind
    let mut state = create_executor_state(ctx.mcx().context())?;
    let out = state.with_mut(|es| -> PgResult<R> {
        let pstmt: &PlannedStmt<'_> = super::raw::state_plan(pstmt);
        if !params.is_null() {
            es.es_param_list_info = Some(super::raw::state_params(params));
            // D2834: a PL list's slot left to evaluation time is its owner's fetch's, as execMain's start takes it
            es.es_param_fetch = types_portal::params::fetch_hook(params);
        }
        let unpruned = pstmt.unprunableRelids.clone_in(es.es_query_cxt)?;
        es.exec_init_range_table(&pstmt.rtable, &pstmt.permInfos, unpruned)?;
        es.es_plannedstmt = Some(pstmt);
        let r = f(es);
        es.exec_close_range_table_relations()?;
        r
    });
    free_executor_state(state);
    out
}

/// D2013 (`nest.prune`): a nest inner union's run-time pruning, PostgreSQL's ExecReScanAppend (the valid set cleared by a
/// rescan whose chgParam meets the prune state's execparamids: a nest flags every parameter it sets at every outer row)
/// and choose_next_subplan_locally (ExecFindMatchingSubPlans at the union's first read after it). The lowering (lowerv4
/// rerun/inner_arms.rs `source`) calls `m8_prune` at that first read; this per-execution object (raw.rs) reads the nest's
/// parameters from the caller's frame words and runs PostgreSQL's own pruning code (`exec_valid`; on the leader over the
/// state it keeps across re-opens, `ExecHold`), so the partition key's support functions run as PostgreSQL runs them (a
/// raise is its raise, a NOTICE its NOTICE) and a pruned arm never opens. Also a pull-form MergeAppend reader's (lowerv4
/// ops/append/prune.rs `reopen`), decided at each re-open.
pub(crate) struct NestPrune;

/// The protocol's per-execution state, which the object's last word names.
pub(crate) struct NestPruneRt {
    pstmt: &'static PlannedStmt<'static>,
    prune: ExecPrune,
    arg: crate::ops::append::proto::PruneArg,
    /// The PARAM_EXEC ids the info's exec steps read (every partitioned rel's `execparamids`).
    ids: memledger::ChargedVec<i32>,
    /// The program's words site (its keys: the frame words).
    words_site: usize,
    /// The thread that prepared the execution (its leader) and the pruning state kept for its calls across re-opens, as
    /// PostgreSQL's PartitionPruneState serves every rescan (`ExecHold`). Another thread's call (a parallel nest's worker)
    /// builds its state per call (`exec_valid`), as every call did before.
    leader: std::thread::ThreadId,
    hold: pgsync::Mutex<Option<super::raw::LeaderHold>>,
    /// D3612 (lowerv4 rerun/inner_arms.rs `pulled`: a Material's pulled union pruned over InitPlans that may raise or act):
    /// the exec params InitPlans answer, read at the decision instead of the frame: a graph InitPlan's value (`run_inits`
    /// ran it at the open) and an in-program one's Init cell (`init::read_param` runs its members nested at that read, as
    /// PostgreSQL's ExecFindMatchingSubPlans runs the InitPlan at the Append's first call).
    known: memledger::ChargedVec<ExecWord>,
    cells: memledger::ChargedVec<(i32, u64)>,
}

impl NestPruneRt {
    pub(crate) fn ids(&self) -> &[i32] {
        &self.ids
    }

    /// D3612: exec param `id`'s word when an InitPlan answers it (`known`, else its Init cell read now), `None` when the
    /// frame holds it (a nest's or SubPlan's parameter).
    pub(crate) fn init_word(&self, id: i32) -> PgResult<Option<ExecWord>> {
        if let Some(w) = self.known.iter().find(|w| w.param == id) {
            return Ok(Some(*w));
        }
        match self.cells.iter().find(|c| c.0 == id) {
            Some(&(_, cell)) => crate::rerun::init_param(cell, id).map(|(value, null)| Some(ExecWord { param: id, value, null })),
            None => Ok(None),
        }
    }

    pub(crate) fn words_site(&self) -> usize {
        self.words_site
    }
}

struct NestPruneLive {
    _cell: Box<[u64]>,
    _rt: Box<NestPruneRt>,
}

impl crate::ops::Live for NestPruneLive {
    fn answers(&self) -> u16 {
        crate::ops::ask::NONE
    }
}

impl crate::ops::Protocol for NestPrune {
    fn kind(&self) -> gravity_program::protos::ProtoKind {
        gravity_program::protos::ProtoKind::NestPrune
    }

    fn prepare(&self, p: &gravity_program::protos::Proto, cx: &mut crate::ops::Prepare<'_>) -> PgResult<Option<Box<dyn crate::ops::Live>>> {
        let a = crate::ops::append::proto::PruneArg::of(p.arg);
        let Some(pstmt) = cx.admitted.pstmt else { return Err(crate::glue::internal("gravity: nest.prune without its plan")) };
        let base = p.words.base as usize;
        if p.words.n != 1 || base >= cx.words.len() || base >= cx.nulls.len() || !a.exec || a.first != 0 || a.count > 63 {
            return Err(crate::glue::internal("gravity: nest.prune's declaration is not one handle word over at most 63 arms with exec steps (D2013)"));
        }
        let params = cx.admitted.graph.as_ref().and_then(|g| g.prune).map_or(ParamListHandle::NULL, |e| e.params);
        let ids = nest_param_ids(pstmt, a.index, cx.owner);
        if ids.len() > EXEC_WORDS {
            return Err(crate::glue::internal("gravity: nest.prune over more params than prune::EXEC_WORDS"));
        }
        let words_site = cx.program.sites.iter().position(|s| matches!(s.kind, gravity_program::SiteKind::Words { .. })).unwrap_or(usize::MAX);
        let (leader, hold) = (std::thread::current().id(), pgsync::Mutex::new(None));
        // D3612: the params InitPlans answer (a graph one's value, an in-program one's Init cell, laid by its earlier proto)
        let mut known = cx.charged_vec::<ExecWord>();
        known.with_vec(|v| v.extend(cx.admitted.graph.as_ref().map_or(&[][..], |g| &g.inits[..]).iter().filter(|i| ids.contains(&i.param)).filter_map(|i| Some(ExecWord { param: i.param, value: *cx.words.get(i.word)?, null: *cx.nulls.get(i.word)? })))); // alloc: Exec charged cx.owner
        let protos = cx.program.v4.as_ref().map_or(&[][..], |d| &d.protos[..]);
        let mut cells: memledger::ChargedVec<(i32, u64)> = cx.charged_vec();
        for &x in ids.iter().filter(|&&x| !known.iter().any(|w| w.param == x)) {
            let Some(i) = protos.iter().find(|i| i.kind == gravity_program::protos::ProtoKind::Init && gravity_kernels::ops::shared::object::entries::init_holds(i.arg, x)) else { continue };
            let cell = cx.words.get(i.words.base as usize).copied().unwrap_or(0);
            if cell == 0 {
                return Err(crate::glue::internal(&format!("gravity: nest.prune reads exec param {x}, whose InitPlan's Init is declared after the prune (D3612)"))); // alloc: Cold
            }
            cells.with_vec(|v| v.push((x, cell))); // alloc: Exec charged cx.owner
        }
        let rt = Box::new(NestPruneRt { pstmt, prune: ExecPrune { index: a.index as u32, nsubplans: a.nsubplans, params }, arg: a, ids, words_site, leader, hold, known, cells }); // alloc: Exec
        let mut cell = vec![0u64; super::raw::WORDS].into_boxed_slice(); // alloc: Exec
        cx.words[base] = super::raw::lay(&mut cell, &rt);
        cx.nulls[base] = 0;
        *cx.nparams = (*cx.nparams).max(base + 1);
        Ok(Some(Box::new(NestPruneLive { _cell: cell, _rt: rt }))) // alloc: Exec
    }
}

/// The PARAM_EXEC ids `partPruneInfos[index]`'s exec steps read, in first-seen order.
fn nest_param_ids(pstmt: &'static PlannedStmt<'static>, index: u16, owner: Option<(memledger::Owner, memledger::Kind)>) -> memledger::ChargedVec<i32> {
    let (o, k) = owner.map_or((None, memledger::Kind::Region), |(o, k)| (Some(o), k));
    let mut out = memledger::ChargedVec::new(o, k); // alloc: Exec charged o
    let Some(info) = pstmt.partPruneInfos.nth(index as usize).as_partition_prune_info() else { return out };
    for l in info.prune_infos.iter().filter_map(|l| l.as_list()) {
        for r in l.iter().filter_map(|r| r.as_partitioned_rel_prune_info()) {
            let mut x = r.execparamids.next_member(-1);
            while x >= 0 {
                if !out.contains(&x) {
                    out.with_vec(|v| v.push(x)); // alloc: Exec charged o
                }
                x = r.execparamids.next_member(x);
            }
        }
    }
    out
}

/// `m8_prune`'s decision: the pruned arms' mask (bit `i`: arm `i` pruned), PostgreSQL's ExecFindMatchingSubPlans over the
/// parameters the nest wrote (`words`, raw.rs read them from the frame).
pub(crate) fn nest_decide(rt: &NestPruneRt, words: &[ExecWord]) -> PgResult<i64> {
    let held = if std::thread::current().id() == rt.leader { rt.hold.try_lock().ok() } else { None };
    let valid = match held {
        Some(mut hold) => {
            if hold.as_ref().is_none_or(|h| h.0.spent()) {
                *hold = None;
                *hold = Some(super::raw::LeaderHold(ExecHold::new(rt.pstmt, rt.prune, words)?));
            }
            match hold.as_mut() {
                Some(h) => h.0.valid(words)?,
                None => return Err(crate::glue::internal("gravity: m8_prune's pruning state not built (D2013)")),
            }
        }
        None => exec_valid(rt.pstmt, rt.prune, words)?,
    };
    let mut skip = [0u64; 4];
    crate::ops::append::proto::skip_words(&valid, rt.arg, &mut skip[..1]);
    if skip[0] >> 63 != 0 {
        return Err(crate::glue::internal("gravity: m8_prune's mask past 63 arms (D2013)"));
    }
    Ok(skip[0] as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// EXPLAIN ANALYZE's proof (main 09-24): a run's set equal to the pin is counted per info, a different one is an
    /// error, an unpinned plan is never checked, and `unpin` hands the counts back once.
    #[test]
    fn a_pinned_plan_counts_equal_sets_and_refuses_a_different_one() {
        let key = 0x5eed_0001usize;
        let kept = set_of([3, 4]);
        pin_key(key, vec![kept.clone(), ALL]); // alloc: Cold
        assert!(PINNED.load(Ordering::Relaxed) >= 1);
        check_key(key, 0, &kept).expect("the pinned set");
        check_key(key, 0, &kept).expect("a second arm run of the same info");
        check_key(key, 1, &ALL).expect("no initial steps: every subplan");
        assert!(check_key(key, 0, &set_of([3])).is_err(), "a subplan the pin keeps was pruned by the run");
        assert!(check_key(key, 1, &kept).is_err());
        assert!(check_key(key, 2, &ALL).is_err(), "an info the pin does not have");
        check_key(key + 8, 0, &set_of([])).expect("another plan is not pinned");
        assert_eq!(unpin_key(key), Some(vec![2, 1])); // alloc: Cold
        assert_eq!(unpin_key(key), None);
        check_key(key, 0, &set_of([])).expect("unpinned");
    }
}
