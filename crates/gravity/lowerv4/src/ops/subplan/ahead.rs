//! A hashed SubPlan's set that waits for its first probe, as PG's `ExecHashSubPlan` builds it (`buildSubPlanHash`), where
//! that probe may never run, though building it before its reader is allowed (`SUBHASH_AHEAD`). A set built before its
//! reader pays its whole build for an outer whose probe the executor never reaches: a join qual over no candidate pair, a
//! conjunct after one that failed every row, an arm a short-circuit skips. A set built at its first probe (LAZY) pins its
//! readers to the leader (D1035), which would cost a reader that runs on workers its width; an AHEAD set has one reader
//! (the proto's `peer`), which keeps its width: exec runs the build at that reader's start when it runs on workers (as an
//! eager InitPlan's members run ahead of its one reader), and leaves it to the first probe when it runs on the leader.
use gravity_decompose::effects::Licence;
use gravity_decompose::ops::shared::subplan::{SubForm, SubPlanDesc, SubSite};
use gravity_decompose::{Op, PipelineNode};
use gravity_kernels::ops::shared::object::entries::{SUBHASH_AHEAD, SUBHASH_LAZY};
use gravity_program::protos::{members_in, pulled, Proto, ProtoKind, NONE};
use gravity_program::Pipeline;

use crate::graph::Gx;

/// The one pipeline that probes the set of `plan_id` (built by pipeline `kb`), when the set may wait for that probe at no
/// cost to the reader's width: no rerun set rebuilds it, exactly one pipeline probes it, and each of its probes may never
/// run (`may_skip`). `None` keeps the build before the reader. Construct C2: with the licence the set's build runs
/// ahead of that probe under, the eager form's (hashed.rs `eager_build` over the set's Build op in `kb`: a set plan
/// that neither acts nor may raise), which covers a build at the reader's start as it covers one at the set's open; a
/// caller asks only for a set it builds eager, so the licence is there wherever a reader is.
pub(crate) fn reader(gx: &Gx<'_, '_>, plan_id: i32, kb: usize) -> Option<(usize, Option<Licence>)> {
    if crate::lifetime::subplan::rebuilt(gx, kb) {
        return None;
    }
    let mut found = None;
    for (k, p) in gx.pipelines().iter().enumerate() {
        for (j, op) in p.ops.iter().enumerate() {
            let Op::SubPlan(d) = op else { continue };
            if d.plan_id != plan_id || !matches!(d.form, SubForm::Hashed { .. }) {
                continue;
            }
            if found.is_some_and(|q| q != k) || k <= kb || !may_skip(p, j, d) {
                return None;
            }
            found = Some(k);
        }
    }
    let found = found?;
    let build = gx.pipelines()[kb].ops.iter().find_map(|op| match op {
        Op::SubPlan(d) if d.plan_id == plan_id && matches!(d.form, SubForm::Build) => Some(d),
        _ => None,
    });
    Some((found, build.and_then(crate::ops::subplan::hashed::eager_build)))
}

/// Whether PG may never reach the probe at op `j` of `p` for a row its source yields: a join qual's (evaluated per
/// candidate pair), a conditional one (`SubPlanDesc::conditional`: behind a short-circuit), or one past an op that may drop
/// the row (a conjunct's filter, a one-time gate, a join, a limit, a run's first-row cut).
fn may_skip(p: &PipelineNode<'_>, j: usize, d: &SubPlanDesc<'_>) -> bool {
    let drops = |op: &Op<'_>| matches!(op, Op::Filter(_) | Op::Gate(_) | Op::Join(_) | Op::FirstOfRun(_));
    d.conditional || d.site == SubSite::JoinQual || p.ops[..j].iter().any(drops) || crate::frame::iface::holds_limit(&p.ops[..j])
}

/// D1035 over the pipelines that call `m8_subhash_run` (`reads`): which of them run on the leader. All of them while any
/// LAZY set is not AHEAD (a probe there may build it, and only the leader holds the runner's frame). Else only those that
/// are no AHEAD set's reader (`peer`), or that run inside another on-demand run's members (an InitPlan's, a rerun set's, a
/// LAZY set's, a pulled set's), which may run them as a section before their own start (exec's AHEAD build needs it).
pub(crate) fn pinned(pipelines: &[Pipeline], protos: &[Proto], reads: &[bool]) -> Vec<bool> {
    let lazy = |p: &Proto| p.kind == ProtoKind::SubHash && p.arg & SUBHASH_LAZY != 0;
    if protos.iter().filter(|p| lazy(p)).any(|p| p.arg & SUBHASH_AHEAD == 0 || p.peer == NONE) {
        return reads.to_vec(); // alloc: Program
    }
    let group = |p: &Proto| matches!(p.kind, ProtoKind::Init | ProtoKind::Rerun) || lazy(p) || pulled(p);
    let nested: Vec<u16> = protos.iter().filter(|p| group(p)).flat_map(|p| members_in(pipelines, protos, p.pipeline as usize).members).collect(); // alloc: Program
    let peer = |i: usize| protos.iter().any(|p| lazy(p) && p.peer as usize == i);
    reads.iter().enumerate().map(|(i, &r)| r && (!peer(i) || nested.contains(&(i as u16)))).collect() // alloc: Program
}
