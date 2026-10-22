//! The rig's run of one lowered program through gravity_driver, split from mod.rs (its budget). D1002 (M7-RIG): the
//! server runner finishes each width-1 producer (exec runner/producers.rs `finish_single`: the owner fold, then the merge's
//! `finish`) and runs its protocols' `after_pipeline`. `Entry::run` alone does neither, so a v4 hash build here was never
//! laid out (`BuildMerge::finish` writes the directory) and every probe over a non-empty build found nothing. `run` is
//! `Entry::run` plus that step for the merges a test can reach: the BuildSide proto's (M1-JOIN's build, `MergeImpl` under
//! `KindId::JoinInner`, its DEFER variant by arg bit 0, as BuildSideLive's `merge_for`) and D276's empty-build mask.
use gravity_driver::{Binding, Call, Entry, Exec, Merge, Step};
use gravity_lowerv4::face::{end::MergeImpl, kinds::KindId};
use gravity_program::protos::ProtoKind;

use super::on_demand::Lives;
use super::{cancel_error, Collect, Mode, Outcome};

thread_local! {
    /// D4291: the reservation the next `drive` runs under (an admitted lease over an armed pool); `None`: unadmitted, 64 MiB.
    pub static LEASE: core::cell::RefCell<Option<std::rc::Rc<gravity_driver::Reservation>>> = const { core::cell::RefCell::new(None) };
    /// D4291: after each pipeline ran to its end, before the next opens: the pipeline, the run, its cancel byte.
    pub static BETWEEN: core::cell::RefCell<Option<Box<dyn FnMut(usize, &Exec, &std::sync::atomic::AtomicU8)>>> = const { core::cell::RefCell::new(None) };
    /// The abort byte the next `drive` binds (a revoke's interrupt byte the test registered, so a kernel's poll sees its
    /// bit); `None`: a fresh byte of its own.
    pub static ABORT: core::cell::Cell<Option<&'static std::sync::atomic::AtomicU8>> = const { core::cell::Cell::new(None) };
}

pub(super) struct NoSources;

impl gravity_driver::Derive for NoSources {
    fn size_hint(&self, _: &gravity_program::SiteDesc) -> usize {
        0
    }
    fn derive_source(&mut self, _: &gravity_program::SiteDesc, _: gravity_driver::ArenaRef) -> gravity_driver::SourceBox {
        unreachable!("run_fixture runs programs without a host site")
    }
    fn writer_size_hint(&self) -> usize {
        0
    }
    fn derive_writer(&mut self, _: &gravity_program::WritePlan, _: gravity_driver::ArenaRef) -> gravity_driver::ArenaBox<dyn gravity_driver::Writer> {
        unreachable!("run_fixture runs reads")
    }
    fn locker_size_hint(&self) -> usize {
        0
    }
    fn derive_locker(&mut self, _: &gravity_program::WritePlan, _: gravity_driver::ArenaRef) -> gravity_driver::ArenaBox<dyn gravity_driver::Locker> {
        unreachable!("run_fixture runs reads")
    }
}

/// `Entry::run` with the server's width-1 producer step after each producer that declares a BuildSide proto (`finish`);
/// M8 (RT-1): each Init's members stepped past, as `run_body` steps them (`Entry::skip_deferred`; they run on demand); a
/// rerun set's members as kept slots (`Entry::skip_rerun`, RT-2e).
pub(super) fn run(entry: &mut Entry, exec: &mut Exec, b: &mut Binding<'_>, call: Call, lives: &Lives) -> gravity_driver::Result<Step> {
    loop {
        let (p, n) = (exec.next_pipeline(), exec.node().program().pipelines.len());
        if lives.iter().any(|l| l.deferred(p)) {
            if p + 1 >= n {
                return Err(gravity_driver::Error::host("v4 rig: an Init's member is the program's last pipeline"));
            }
            // M8 (RT-2e): a rerun set's member keeps its slot, opened zeroed for its owner (`Entry::skip_rerun`), as the
            // runner's producer loop steps it
            match lives.iter().find_map(|l| l.rerun_owner(p)) {
                Some(owner) => entry.skip_rerun(exec, owner)?,
                // M8 (RT-2f, nest.cte CT-2): a pulled CTE's plan keeps its store edge, opened zeroed for its readers
                None => match lives.iter().find_map(|l| l.pull_readers(p)) {
                    Some(r) => entry.skip_pulled(exec, &r)?,
                    None => entry.skip_deferred(exec),
                },
            }
            continue;
        }
        let step = entry.run_pipeline(exec, b, call)?;
        if step != Step::Done || p + 1 >= n {
            return Ok(step);
        }
        finish(entry, exec, b, p)?;
        BETWEEN.with(|h| {
            if let Some(f) = h.borrow_mut().as_mut() {
                f(p, exec, gravity_driver::Attached::abort(&*b));
            }
        });
        // D3040: a CTE filled into an owned spool is sealed after its fill pipeline (exec ops/material OwnedLive's
        // `after_pipeline`), as the server runner's producer loop does
        let owned = exec.node().program().v4.as_ref().is_some_and(|d| d.protos.iter().any(|q| q.kind == ProtoKind::Spool && q.peer as usize == p && q.arg & gravity_kernels::ops::shared::object::entries::SPOOL_OWNED != 0));
        if owned {
            for l in lives.iter() {
                if let Some(r) = l.after_pipeline(p, exec) {
                    r?;
                }
            }
        }
    }
}

/// runner/producers.rs `finish_single` and BuildSideLive's `after_pipeline` for producer `p`, when it is a v4 build.
pub(super) fn finish(entry: &mut Entry, exec: &mut Exec, b: &mut Binding<'_>, p: usize) -> gravity_driver::Result<()> {
    let program = exec.node().program().clone();
    let (Some(v4), Some(slot)) = (program.v4.as_ref(), program.pipelines[p].produces()) else { return Ok(()) };
    let Some(proto) = v4.protos.iter().find(|q| q.kind == ProtoKind::BuildSide && q.pipeline as usize == p) else { return Ok(()) };
    let m = gravity_lowerv4::ops::kind_as::<MergeImpl>(KindId::JoinInner).expect("join.buildside without the build's merge").0;
    let merge: &dyn Merge = if proto.arg & 1 != 0 { m.with_defer(true).unwrap_or(m) } else { m };
    entry.open_unclaimed_of(exec, p)?;
    if exec.node().arm_continues(p) {
        return Ok(());
    }
    if let Some(pbits) = entry.owner_bits_of(exec, p, merge) {
        entry.own_single_of(exec, p, merge, pbits)?;
    }
    match merge.finish(&mut exec.node_mut().leader_state(slot, b.sink)) {
        Ok(()) => {}
        Err(gravity_driver::Error::Unsupported(_)) if program.pipelines[p].breaker().is_some_and(|k| matches!(k.spill, gravity_abi::breaker::Spill::Partitioned { .. })) => {
            entry.spill_leader_partial_of(exec, p)?;
        }
        Err(e) => return Err(e),
    }
    // D276: an empty build masks the probe's outer-side producers (arg bits 8..47); a spilled build is never read as empty
    let mask = (proto.arg >> 8) & ((1 << 40) - 1);
    if let (Some(read), true) = (proto.read[0], mask != 0 && exec.node().spill_parts_of(slot) == 0) {
        let owner = exec.node().slot_owner(slot);
        let r = exec.node_mut().slot_region(slot);
        let mut rows = None;
        if r.len() >= 64 {
            // SAFETY: the build's finished region of at least 64 bytes (m1_keyed.rs MIN_REGION); the reader checks its header.
            let _ = unsafe { read(r.as_ptr(), &[proto.arg], owner, &mut |_, n, _| rows = Some(n)) };
        }
        if rows == Some(0) {
            exec.run_empty(mask);
        }
    }
    Ok(())
}

pub(super) fn drive(program: &std::sync::Arc<gravity_program::Program>, params: &[u64], nulls: &[u8], mode: Mode, lives: &Lives) -> Result<Outcome, String> {
    use gravity_driver::{BindingParts, CommandId, Grant, Identity, PerTuple, Pins, Reservation, Snapshot};
    let own = std::sync::atomic::AtomicU8::new(0);
    let flag: &std::sync::atomic::AtomicU8 = ABORT.with(|a| a.get()).unwrap_or(&own);
    let mut sink = Collect { out: &program.out, rows: Vec::new(), err: None, pushes: 0, cancel: match mode { Mode::Cancel(n) => Some((n, flag)), _ => None } };
    if mode == Mode::Cancel(0) {
        flag.store(1, std::sync::atomic::Ordering::Release);
    }
    let parts = BindingParts { snapshot: Snapshot(0), command_id: CommandId(0), params, param_nulls: nulls, abort_flag: flag, pins: Pins(0), per_tuple: PerTuple(0), identity: Identity { proc_number: 0, resource_owner: 0, latch: 0 }, resolved: None };
    let mut b = Binding::new(parts, &mut sink);
    let (mut entry, cap) = (Entry::new(1), if let Mode::Stop(n) = mode { n.max(1) } else { program.layout.out_rows as u32 });
    let lease = LEASE.with(|l| l.borrow().clone()).unwrap_or_else(|| std::rc::Rc::new(Reservation::unadmitted(Grant::of(64 << 20))));
    let r = match entry.start(None, program, &mut NoSources, &mut b, lease) {
        Err(e) => Err(e),
        Ok(mut exec) => {
            let r = super::on_demand::run(&mut entry, &mut exec, &mut b, cap, program, lives)?;
            entry.end(exec, r.is_ok());
            r
        }
    };
    drop(b);
    // a host's stashed error behind a raise, taken once (none is left for the next fixture): a cancel inside a nested run
    // is PG's 57014; any other stays the bare raise below, which the server-only rows pin (no live arena or store here)
    let stashed = matches!(r, Err(gravity_driver::Error::Raised(_))).then(gravity_fn::take_error).flatten();
    match (r, sink.err) {
        (_, Some(e)) => Err(e),
        (Ok(()), None) => Ok(Outcome::Rows(sink.rows)),
        (Err(gravity_driver::Error::Cancelled), None) => Ok(cancel_error()),
        // a cancel inside a nested run (a rerun set's members): the host stashed PG's 57014 and raised; any other stashed
        // error still fails the fixture below
        (Err(gravity_driver::Error::Raised(_)), None) if stashed.as_ref().is_some_and(|e| e.sqlstate() == types_error::ERRCODE_QUERY_CANCELED) => Ok(cancel_error()),
        // M5 L0-e: a region only the server's section grants (a breaker's scratch): the server runner's statement
        (Err(gravity_driver::Error::Unsupported(k)), None) => Err(format!("host site(s): the driver asks the server's section (Unsupported({k})), the server runner's statement")),
        (Err(e), None) => Ok(Outcome::Error { sqlstate: "XX000".into(), message: format!("{e:?}") }),
    }
}
