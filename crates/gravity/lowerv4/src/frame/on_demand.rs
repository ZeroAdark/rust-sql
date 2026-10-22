//! D1035 (frame/program.rs's pins, split out of it; created by M8-RUNTIME, V4R-444): a pipeline whose code
//! can run an InitPlan, a rerun set, a CTE pull or a LAZY set's build (`m8_init_run`, `m8_rerun`, `m8_rerun_pull`,
//! `m8_cte_pull`, `m8_subhash_run`) runs on the leader, whose frame holds the cells. Step 2:
//! the one reader of the program's one INIT_EAGER Init (parallel-safe and not VOLATILE: decompose's F5 hazard fold), a
//! pipeline reaching no rerun, stays eligible and is named in the Init proto's `peer`; exec runs the Init's members at that
//! reader's start when it runs on workers, as PostgreSQL's Gather evaluates its `initParam` (exec v4/m8_init.rs).
use gravity_ir::Func;
use gravity_kernels::ops::shared::object::entries as m8;
use gravity_program::protos::{Proto, ProtoKind};
use gravity_program::Pipeline;

/// Pins every reader of an on-demand run to the leader but an eager Init's one reader, whose Init proto names it (`peer`).
pub(crate) fn pin(ir: &gravity_ir::Program, fs: &[Func], pipelines: &mut [Pipeline], protos: &mut [Proto]) {
    let init_reads = on_demand_runs(ir, fs, "m8_init_run");
    let mut rerun_reads = on_demand_runs(ir, fs, "m8_rerun");
    // B2a: a LAZY hashed SubPlan's set build (`m8_subhash_run`) runs its members from the leader's frame, as a rerun set;
    // an AHEAD set's one reader keeps its width (the subplan home's `ahead::pinned`)
    let subhash_reads = on_demand_runs(ir, fs, "m8_subhash_run");
    let subhash_serial = crate::ops::subplan::ahead::pinned(pipelines, protos, &subhash_reads);
    // D2034: a streamed recursive generation's term set pull (`m8_rerun_pull`) runs the body from the leader's frame, as `m8_rerun`
    rerun_reads.iter_mut().zip(on_demand_runs(ir, fs, "m8_rerun_pull")).for_each(|(r, s)| *r |= s);
    // D2013: a nest inner union's run-time pruning (`m8_prune`) runs PostgreSQL's pruning code over the leader's frame words
    rerun_reads.iter_mut().zip(on_demand_runs(ir, fs, "m8_prune")).for_each(|(r, s)| *r |= s);
    // nest.cte (CT-2): a CTE pull's reader runs on the leader too (the pulled set is leader-serial), and is never an eager reader
    let cte_reads = on_demand_runs(ir, fs, "m8_cte_pull");
    let serial: Vec<bool> = rerun_reads.iter().zip(&cte_reads).zip(&subhash_serial).map(|((r, c), s)| *r || *c || *s).collect(); // alloc: Program
    rerun_reads.iter_mut().zip(subhash_reads).for_each(|(r, s)| *r |= s);
    let eager = eager_reader(pipelines, protos, &init_reads, &serial);
    if let Some((init, reader)) = eager {
        protos[init].peer = reader as u16;
    }
    // D2461: a group's members open nested inside the pipeline that runs it, so each member's slot is held through the
    // last pipeline that can run its kind of group: the grant's share divisor counts it where it opens
    let last = |r: &[bool]| r.iter().rposition(|&x| x).map_or(-1, |k| k as i16);
    for (p, through) in protos.iter().filter_map(|p| match p.kind {
        ProtoKind::Init => Some((p.pipeline as usize, last(&init_reads))),
        ProtoKind::SubHash if p.arg & m8::SUBHASH_LAZY != 0 => Some((p.pipeline as usize, last(&rerun_reads))),
        ProtoKind::Rerun => Some((p.pipeline as usize, last(&rerun_reads))),
        _ => None,
    }) {
        for x in gravity_program::protos::members_in(pipelines, protos, p).members {
            let d = &mut pipelines[x as usize];
            if d.produces().is_some() && through > x as i16 {
                d.live_through = d.live_through.max(through);
            }
        }
    }
    for (i, d) in pipelines.iter_mut().enumerate().filter(|(i, d)| (init_reads[*i] || serial[*i]) && d.is_eligible()) {
        if !matches!(eager, Some((_, r)) if r == i) {
            d.parallel = gravity_program::Parallel::Ineligible { site: d.driving_site, reason: gravity_program::Reason::Host };
        }
    }
}

/// D1035: per pipeline function `fs[i]`, whether it or a function it calls calls an on-demand run's host, the host
/// declared as `run` (the program declares each host once by name, `declare_host_once`). By name, never by address
/// (D2916: a function has no address of its own: merged with an identical one, or one copy per codegen unit).
fn on_demand_runs(ir: &gravity_ir::Program, fs: &[Func], run: &str) -> Vec<bool> {
    let hosts: Vec<bool> = ir.host_fns.iter().map(|h| h.name == run).collect(); // alloc: Program
    // D3886: a program that declares no such host calls none, and its functions are not walked (`pin` asks six runs; the
    // walks over every instruction were ~3% of a lowering whose program runs none of them)
    if !hosts.contains(&true) {
        return vec![false; fs.len()]; // alloc: Program
    }
    // 0 unseen, 1 on the walk (a call cycle adds nothing), 2 no, 3 yes
    fn walk(ir: &gravity_ir::Program, hosts: &[bool], seen: &mut [u8], f: usize) -> bool {
        crate::stack_check();
        if seen[f] != 0 {
            return seen[f] == 3;
        }
        seen[f] = 1;
        let calls = ir.funcs[f].blocks.iter().flat_map(|b| b.insts.iter()).any(|i| match i.op {
            gravity_ir::Op::CallHost { callee, .. } => hosts[callee.index()],
            gravity_ir::Op::Call { callee, .. } => walk(ir, hosts, seen, callee.index()),
            _ => false,
        });
        seen[f] = if calls { 3 } else { 2 };
        calls
    }
    let mut seen = vec![0u8; ir.funcs.len()]; // alloc: Program
    fs.iter().map(|f| walk(ir, &hosts, &mut seen, f.index())).collect() // alloc: Program
}

/// D1035 step 2 (V4R-444): `(the Init proto, its reader)` when the program declares exactly one Init, flagged INIT_EAGER
/// (parallel-safe, not VOLATILE), and exactly one pipeline can read it (`init_reads`), a pipeline that runs no rerun set
/// nor pulls a CTE (`serial`); else `None`, and every reader stays on the leader (D1035). with#32: never an Init whose run
/// reads a recursive union's store (`reads_union`): PG runs it at the first evaluation reading its param, and decompose
/// (ops/setop.rs `readers`) admits the union read through it on that ground (a union read whole may never end).
fn eager_reader(pipelines: &[Pipeline], protos: &[Proto], init_reads: &[bool], serial: &[bool]) -> Option<(usize, usize)> {
    let mut inits = protos.iter().enumerate().filter(|(_, p)| p.kind == ProtoKind::Init);
    let (init, p) = inits.next()?;
    let mut readers = init_reads.iter().enumerate().filter(|(_, r)| **r).map(|(i, _)| i);
    let reader = readers.next()?;
    let one = inits.next().is_none() && readers.next().is_none();
    // D1262: an Init a rerun set re-arms runs again per re-open, never ahead
    let rerun = protos.iter().any(|q| q.kind == ProtoKind::Rerun && q.pipeline == p.pipeline);
    let union = reads_union(pipelines, protos, p.pipeline as usize);
    (one && p.arg & m8::INIT_EAGER != 0 && !serial[reader] && !rerun && !union && reader <= u16::MAX as usize - 1).then_some((init, reader))
}

/// with#32: whether a member of on-demand run `p` (`members_in`) reads a slot whose nearest earlier producer is a recursive
/// union's driver (its Generation proto, ops/setop/recursive.rs): the union's store, a CTE's, read through a CteScan.
fn reads_union(pipelines: &[Pipeline], protos: &[Proto], p: usize) -> bool {
    let driver = |q: usize| protos.iter().any(|g| g.kind == ProtoKind::Generation && g.pipeline as usize == q);
    let reads = |x: usize| pipelines[x].consumes.iter().any(|&s| (0..x).rev().find(|&q| pipelines[q].produces() == Some(s)).is_some_and(driver));
    gravity_program::protos::members_in(pipelines, protos, p).members.iter().any(|&x| reads(x as usize))
}
