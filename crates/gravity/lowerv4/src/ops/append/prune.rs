//! Partition pruning of a v4 program (cards/M2.md L6 `src.prune`; the ladder's subplan.rs arm pruning and decompose
//! append.rs `prune_of` as the forms). The planner's `PartitionPruneInfo` stays PostgreSQL's: the runtime protocol
//! (exec/src/v4/m2.rs `Prune`) runs `initial_valid` at the bind over the execution's parameters, or `exec_valid` after the
//! InitPlans, into one skip bit per arm; each arm's marker (append.rs) reads its bit at the open and passes no row when it
//! is set, and a pruned arm's pipeline claims nothing. EXPLAIN ANALYZE's "Subplans Removed" is PostgreSQL's: the entry
//! answers the prune getters from PostgreSQL's initial pruning over the execution's parameters and pins that set, and this
//! protocol's `initial_valid` checks its own set against the pin (V4R-94). The protocol's arg (exec m2.rs `PruneArg`): bits 0..15 the `partPruneInfos` index, 16..31 the
//! subplan count, 32..47 the first arm's subplan, 48..62 the arm count, 63 exec steps; its words `ceil(count / 64)`, bit
//! `i` = arm `first + i`. Arms that are contiguous program pipelines share one protocol (a run): arm `i` extends the run
//! arm `i - 1` opened when it is the next pipeline, else it opens a run with words for every arm left. Exec pruning over
//! a rerun's parameters (a nest's, decided per outer row) is decided at every rerun by nest.rerun's runtime (M8 L0-h,
//! exec v4/m8_rerun.rs) when the arms are the set's members; otherwise it refuses by that name. Registers kind
//! `src.prune` (admission; append.rs is its reader). Owner M2-SOURCES.
use gravity_decompose::ops::shared::append::PruneDesc;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{HostFlags, Type, Value};
use gravity_kernels::ops::append::raw::NestPrune;
use gravity_program::protos::{Proto, ProtoKind, Words};

use crate::face::kinds::KindId;
use crate::frame::env::Env;
use crate::frame::px::Px;
use crate::graph::Gx;
use crate::admit::refusal::Reason;
use crate::ops::Table;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::SrcPrune, &crate::face::source::PruneImpl(declare))], ..crate::ops::NONE };

/// The protocol's arg (exec m2.rs `PruneArg::arg`; also nest.prune's, rerun/inner_arms.rs `nest_prune`).
pub(crate) const fn arg(index: u32, nsubplans: u16, first: u16, count: u16, exec: bool) -> u64 {
    (index as u16) as u64 | (nsubplans as u64) << 16 | (first as u64) << 32 | ((count & 0x7fff) as u64) << 48 | (exec as u64) << 63
}

const fn first_of(arg: u64) -> u16 {
    (arg >> 32) as u16
}

const fn count_of(arg: u64) -> u16 {
    ((arg >> 48) & 0x7fff) as u16
}

/// Every exec pruning parameter of `p` is an InitPlan's result (decided once per execution); a rerun's is not.
fn exec_over_init_plans(px: &Px<'_, '_>, p: &PruneDesc<'_>) -> bool {
    let inits = &px.gx.graph.init_plans;
    p.exec_params_all(|x| inits.iter().any(|i| i.param == x))
}

/// M8 L0-h: arm pipeline `k` is a member of a rerun set and every exec pruning parameter of `p` is an InitPlan's or set by
/// a producer of the graph (the set's owner writes it before each rerun): exec/src/v4/m8_rerun.rs decides the skip words
/// at every rerun, as PG's ExecReScanAppend re-runs ExecFindMatchingSubPlans.
pub(crate) fn exec_per_rerun(gx: &Gx<'_, '_>, k: usize, p: &PruneDesc<'_>) -> bool {
    if !gx.graph.reruns.iter().any(|r| r.pipelines.iter().any(|&q| q as usize == k)) {
        return false;
    }
    p.exec_params_all(|x| crate::rerun::params::producer(gx, x).is_some())
}

/// D2105 / D2106: arm pipeline `k` of an Append pruned by `p` is skipped whole when its rerun set's pruning removes it: an
/// exec pruning the set decides at every rerun (`exec_per_rerun`), or quiet initial steps only, which the set decides once
/// per execution (`initial_in_set`). Exec's set answers the arm's skip bit as `Live::pruned_whole`, so the runner never
/// opens the arm's walk nor evaluates its keys (D2105's root fix), as PostgreSQL never opens a pruned arm.
pub(crate) fn skipped_whole(gx: &Gx<'_, '_>, k: usize, p: &PruneDesc<'_>) -> bool {
    if p.exec {
        exec_per_rerun(gx, k, p)
    } else {
        initial_in_set(gx, k) && crate::rerun::union_arms::quiet_prune(gx, p)
    }
}

/// D2106: arm pipeline `k` is a member of a rerun set, whose live decides an initial pruning of its arms (no exec steps)
/// once, at the execution's prepare (exec rerun/mod.rs `prunes_of`), where PG's ExecInitAppend does at the executor start.
fn initial_in_set(gx: &Gx<'_, '_>, k: usize) -> bool {
    gx.graph.reruns.iter().any(|r| r.pipelines.iter().any(|&q| q as usize == k))
}

/// Arm `i` of `n` (pipeline `k`) of the Append pruned by `p`: its skip bit as `(word, bit)`.
pub(crate) fn declare(px: &mut Px<'_, '_>, k: usize, i: u16, n: u16, p: &PruneDesc<'_>, node: i32) -> (u16, u32) {
    if p.exec && !exec_over_init_plans(px, p) && !exec_per_rerun(px.gx, k, p) {
        crate::refuse!(node, Reason::Shape { why: crate::admit::refusal::ShapeWhy("an Append's run-time prune over executor parameters neither InitPlans' nor set by a rerun set's producer (a nested SubPlan's inner: D1262)") });
    }
    // D2106: the set decides an initial pruning at the prepare, outside the statement's snapshot, so one whose steps call a
    // user or VOLATILE function or may raise (union_arms.rs `quiet_prune`) refuses: a call would run SQL with no snapshot or
    // a different number of times, a raise would come in another order than at PostgreSQL's executor start
    if !p.exec && initial_in_set(px.gx, k) && !crate::rerun::union_arms::quiet_prune(px.gx, p) {
        crate::refuse!(node, Reason::Shape { why: crate::admit::refusal::ShapeWhy("an initial pruning of a rerun set's Append arms that calls a user or VOLATILE function or may raise (the set decides it at the prepare, outside the statement's snapshot: D2106)") });
    }
    if n > 0x7fff || p.index > u16::MAX as u32 {
        crate::refuse!(node, Reason::Bug { payload: "an Append with more arms or prune infos than the prune protocol's arg holds" });
    }
    let at = px.ident(k) as u16;
    // the run arm i - 1 opened or extended, when this arm is its next pipeline
    let run = px.protos.iter_mut().rev().find(|q| {
        q.kind == ProtoKind::Prune && q.arg as u16 == p.index as u16 && (q.arg >> 16) as u16 == n && first_of(q.arg) + count_of(q.arg) == i && q.pipeline + count_of(q.arg) == at
    });
    if let Some(q) = run {
        let (first, count) = (first_of(q.arg), count_of(q.arg) + 1);
        q.arg = arg(p.index, n, first, count, p.exec);
        q.words.n = count.div_ceil(64);
        let at = i - first;
        return (q.words.base + at / 64, (at % 64) as u32);
    }
    // a new run: words for every arm from here on (a later arm extends into them)
    let base = px.proto_words((n - i).div_ceil(64));
    let mut q = Proto::new(ProtoKind::Prune, k as u16);
    q.arg = arg(p.index, n, i, 1, p.exec);
    q.words = Words { base, n: 1 };
    px.declare_proto(q);
    (base, 0)
}

/// D2013's nest.prune road for a MergeAppend's pull-form reader `k` over `n` arms that no rerun restarts: a SubPlan's or a
/// nest's inner, which its owner re-opens at every outer row, while the arms' rerun set lays their skip words only at the
/// start of a pipeline (the owner's, once per execution). PostgreSQL's ExecReScanMergeAppend clears the valid set when the
/// parameters change and its next call finds it again (ExecFindMatchingSubPlans); here `m8_prune` (exec ops/append/prune.rs
/// `NestPrune`, PostgreSQL's own pruning code) decides the arms at every re-open (`reopen_mask`), over the parameters a
/// SubPlan or nest wrote before it. Only a quiet pruning (union_arms.rs `quiet_prune`): PostgreSQL calls it at the merge's
/// first pull after the rescan, this at the re-open, and a quiet call shows no difference. The handle word, else `None`: the
/// skip words as before (every arm read at a re-open: PostgreSQL's rows, more of them read).
pub(crate) fn reopen(px: &mut Px<'_, '_>, k: usize, d: &PruneDesc<'_>, n: usize) -> Option<u16> {
    let gx = px.gx;
    let ps = gx.pipelines();
    let inners = ps.inners();
    let &((owner, _), _) = k.checked_sub(ps.len()).and_then(|i| inners.get(i))?;
    let fits = n <= 63 && d.index != gravity_decompose::facts::hoist::HOISTED && d.index <= u16::MAX as u32;
    // a quiet pruning's exec parameters are ones a nest or a per-row SubPlan binds (graph.params, union_arms.rs `quiet_in`):
    // the reader's owner or an outer one writes them before the re-open, where m8_prune reads them from the frame words
    if !(d.exec && fits && crate::rerun::union_arms::quiet_prune(gx, d)) {
        return None;
    }
    let mut root = owner;
    while root >= ps.len() {
        root = inners[root - ps.len()].0 .0;
    }
    let base = px.proto_words(1);
    let mut q = Proto::new(ProtoKind::NestPrune, root as u16);
    q.arg = arg(d.index, n as u16, 0, n as u16, true);
    q.words = Words { base, n: 1 };
    px.declare_proto(q);
    Some(base)
}

/// `reopen`'s decision at a re-open: `m8_prune(ctx, h)`, the pruned arms' mask (bit `i`: arm `i`), else a status below zero
/// with the host's error stashed (merge_pull.rs then opens no arm and leaves with it).
pub(crate) fn reopen_mask(b: &mut FunctionBuilder<'_>, env: &Env, h: u16) -> Value {
    let hv = b.load(Type::I64, env.params, h as i32 * 8);
    let f = b.declare_host_once("m8_prune", &[Type::Ptr, Type::I64], Some(Type::I64), gravity_kernels::contract::entry1::<NestPrune> as *const () as usize, HostFlags { touches_memory: true, may_error: true, ..HostFlags::default() });
    b.call_host(f, &[env.ctx, hv], Some(Type::I64)).unwrap_or(env.zero)
}
