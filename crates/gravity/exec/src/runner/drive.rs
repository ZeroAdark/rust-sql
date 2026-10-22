//! One served execution: `drive` picks the serial (`run_once`) or parallel (`run_parallel`) run, the Append arm swap, the
//! park key / recipe salt, and the table-lane fault arm. Split out of runner.rs (move-only, lane SPLIT 09-23).

use super::*;

/// One served execution's census: the completion-tag rows, and the width and section count when it ran parallel sections (0 for a serial run).
pub struct Ran {
    pub rows: u64,
    pub width: u16,
    pub sections: u32,
    /// Claims the pool ran over the statement (its counter's delta).
    pub claims: u64,
    /// Part opens the statement performed (the registry's counter delta; 0 on a resident hit).
    pub opens: u64,
    /// Granules decoded / skipped on a `NoneMatch` verdict / answered from statistics, and parts answered whole from the part-grain record (the sources' counter deltas).
    pub granules: [u64; 5],
    /// Stats sections read from part files / bodies served from the catalog's answers (the sources' counter deltas).
    pub first_touch: [u64; 3],
    /// The process's first section: `(first arm µs, first section µs)` when this statement ran it, else zeros.
    pub cold: (u64, u64),
    /// Phase-2 sections merged / refusals by step (plan × 100 + 2a × 10 + 2b), the last 2b refusal's header.
    pub p2: (u64, u64, [u32; 4]),
    /// The spill census (§2): generation-0 partitions of slot 0, bytes across every file and every owner region mapped
    /// from a temp file, generations reached.
    pub spill: (u32, u64, u8),
    /// The part files' I/O over the statement (`granule_source::io_census` deltas) and the preads-in-flight high-water mark.
    pub io: [u64; 9],
    pub dict: [u64; 5],
    /// The dictionary entry accesses (`granule_source::entry_census` deltas; lane dictresident).
    pub dictent: [u64; 6],
    pub elided: u64,
    /// The pool the statement was placed on (`gravity_parallel::place`; 0 with one pool).
    pub node: u8,
}

#[allow(clippy::too_many_arguments)]
pub fn drive(mcx: Mcx<'static>, entry: &mut gravity_driver::Entry, scratch: &mut PlanScratch, a: &mut Admitted, key: Option<PlanKey>, snapshot: &Rc<SnapshotData<'static>>, params: &[u64], nulls: &[u8], cid: u32, cmd: Cmd, dest: &mut DestReceiver<'_>, wire: Option<&mut gravity_wire::WireNode>, formats: Option<&[i16]>) -> PgResult<Ran> {
    let remote = matches!(dest.mydest(), CommandDest::Remote | CommandDest::RemoteExecute | CommandDest::RemoteSimple);
    let _sent = gravity_driver::ahead_trace::statement(false); // construct C2 R3: one run, no row of it sent yet
    snapmgr::PushActiveSnapshot(snapshot)?;
    gravity_driver::take_unsupported(); // D1443: this run's failing pipeline only
    gravity_driver::take_ahead(None); // `DECISIONS.md` 4 (D) b: this run's run-ahead error only
    crate::ops::modifytable::after_query::begin(cmd);
    // a composed statement's InitPlans fill their exec words first (a copy of the image: the caller's is the binding's);
    // pglfast C3: a statement without InitPlans writes none, so it runs on the caller's words (no copy per statement)
    let n = params.len();
    let inits = a.graph.as_ref().is_some_and(|g| !g.inits.is_empty());
    let mut copy = inits.then(|| (params.to_vec(), nulls[..n].to_vec())); // alloc: Exec
    let (kp, kn): (&[u64], &[u8]) = match copy.as_mut() {
        Some((kp, kn)) => match run_inits(mcx, entry, a, snapshot, cid, &mut kp[..n], &mut kn[..n]) {
            Ok(()) => (&kp[..n], &kn[..n]),
            Err(e) => {
                snapmgr::PopActiveSnapshot()?;
                return Err(spilled_build_error(a, e));
            }
        },
        None => (params, &nulls[..n]),
    };
    let mut wire = wire;
    let out = crate::ops::append::prune::exec_skip(a, kp, kn).and_then(|_| {
        let (params, nulls) = (kp, kn);
        // a MergeAppend (rung 12 need 14): every program pushes into the cut's buffers; the cut drains into the receiver after the last arm
        let dctx;
        let mut inner = Receiver::Null;
        let mut byref: Vec<bool> = Vec::new(); // alloc: Exec
        let mut keys: Vec<gravity_lower::mergearms::MergeKey> = Vec::new(); // alloc: Exec
        let mut narms = 0usize;
        // the wire is the cut's inner sink or the runs' (a run under the cut never touches it)
        let mut runs_wire = None;
        if let Some(g) = a.graph.as_ref().filter(|g| !g.merge().is_empty() && emits_rows(cmd, a.out_natts)) {
            inner = if remote {
                let w = wire.as_deref_mut().expect("select without a wire node");
                w.prepare(&a.program, formats)?;
                Receiver::Wire(w.reset())
            } else {
                dctx = gravity_driver::mem::root::Root::new("GravityDestSink");
                let mcx: Mcx<'static> = super::raw::sink_mcx(&dctx);
                Receiver::Dest(DestSink::new(mcx, desc_of(mcx, &a), dest))
            };
            byref = a.program.out.iter().map(|c| !c.typbyval).collect(); // alloc: Exec
            keys = g.merge().to_vec(); // alloc: Exec
            narms = 1 + g.arms.len();
        } else {
            runs_wire = wire.as_deref_mut();
        }
        let mut cut = if narms > 0 { Some(crate::ops::append::merge::ArmMerge::new(&mut inner, a.out_natts, &byref, &keys, narms)) } else { None };
        // the arms' held rows feed no breaker: the statement's (or the executing portal's) owner,
        // the read the Node's bind makes (R3, gravity_driver::stmt_context), taken before the first arm runs
        if let Some(c) = cut.as_mut() {
            c.set_owner(gravity_driver::stmt_context().map(|(o, _)| o));
        }
        let mut ran = run_program(mcx, entry, scratch, a, key, snapshot, params, nulls, cid, cmd, remote, dest, runs_wire.as_deref_mut(), formats, cut.as_mut().map(|c| c as &mut dyn gravity_driver::Sink))?;
        // an Append's remaining arms (rung 12 need 3): each run as the main was, into the same receiver, in order
        for i in 0..a.graph.as_ref().map_or(0, |g| g.arms.len()) {
            if a.graph.as_ref().is_some_and(|g| g.arms[i].skip) {
                continue;
            }
            if let Some(c) = cut.as_mut() {
                c.arm(i + 1);
            }
            let swapped = ArmSwap::new(a, i);
            let r = run_program(mcx, entry, scratch, &mut *swapped.a, None, snapshot, params, nulls, cid, cmd, remote, dest, runs_wire.as_deref_mut(), formats, cut.as_mut().map(|c| c as &mut dyn gravity_driver::Sink));
            core::mem::drop(swapped);
            ran.rows += r?.rows;
        }
        if let Some(c) = cut.as_mut() {
            ran.rows = c.drain().map_err(glue::pg_error)?;
        }
        crate::ops::modifytable::after_query::end(cmd)?;
        Ok(ran)
    });
    // D4474: the wire node keeps at most FRAME_KEEP of its framing buffer past the run (printtup's shutdown frees its row buffer)
    if let Some(w) = wire.as_deref_mut() {
        w.end_run();
    }
    snapmgr::PopActiveSnapshot()?;
    out.map_err(|e| spilled_build_error(a, e))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn run_program(mcx: Mcx<'static>, entry: &mut gravity_driver::Entry, scratch: &mut PlanScratch, a: &mut Admitted, key: Option<PlanKey>, snapshot: &Rc<SnapshotData<'static>>, params: &[u64], nulls: &[u8], cid: u32, cmd: Cmd, remote: bool, dest: &mut DestReceiver<'_>, wire: Option<&mut gravity_wire::WireNode>, formats: Option<&[i16]>, outer: Option<&mut dyn gravity_driver::Sink>) -> PgResult<Ran> {
    if a.is_parallel() {
        install_held_hooks();
        run_parallel(mcx, entry, scratch, a, key, snapshot, params, nulls, cid, cmd, remote, dest, wire, formats)
    } else { run_once(mcx, entry, a, key, snapshot, params, nulls, cid, cmd, remote, dest, wire, formats, outer).map(|rows| Ran { rows, width: 0, sections: 0, claims: 0, opens: 0, granules: [0; 5], first_touch: [0; 3], cold: (0, 0), spill: (0, 0, 0), p2: (0, 0, [0; 4]), io: [0; 9], dict: [0; 5], dictent: [0; 6], elided: 0, node: 0 }) }
}

/// Op 14: the driver's held-error hooks over gravity_hosts' stash, once per process (a parallel program's runs: the only
/// ones with a producer whose merge can defer its error).
pub(crate) fn install_held_hooks() {
    fn take() -> Option<Box<dyn std::any::Any + Send + Sync>> {
        gravity_hosts::take_any_error().map(|e| e as Box<dyn std::any::Any + Send + Sync>)
    }
    fn restash(p: &(dyn std::any::Any + Send + Sync)) {
        if let Some(e) = p.downcast_ref::<types_error::PgError>() {
            let _ = gravity_hosts::stash(Box::new(e.clone())); // alloc: Cold
        }
    }
    // D2657: an interrupt check_for_interrupts raised inside a host call (57014 and the rest of class 57, or FATAL and
    // above) is never held; the stash is taken to look and put back for the raise
    fn interrupt() -> bool {
        let Some(e) = gravity_hosts::take_any_error() else { return false };
        let hit = types_error::errcode_to_category(e.sqlstate()) == types_error::ERRCODE_OPERATOR_INTERVENTION || e.level() >= types_error::FATAL;
        let _ = gravity_hosts::stash(e);
        hit
    }
    if gravity_driver::HELD_HOOKS.get().is_none() {
        let _ = gravity_driver::HELD_HOOKS.set(gravity_driver::HeldHooks { take, restash });
    }
    if gravity_driver::HELD_INTERRUPT.get().is_none() {
        let _ = gravity_driver::HELD_INTERRUPT.set(interrupt);
    }
}

/// An Append arm swapped into the admission's program and registry for one run (`run_once` / `run_parallel` see the arm as
/// the program, its own park key and salt); the drop swaps the main back, error or not: the cached admission never stays on an arm.
struct ArmSwap<'a> {
    a: &'a mut Admitted,
    i: usize,
}

impl<'a> ArmSwap<'a> {
    fn new(a: &'a mut Admitted, i: usize) -> ArmSwap<'a> {
        Self::swap(a, i);
        ArmSwap { a, i }
    }

    fn swap(a: &mut Admitted, i: usize) {
        let Admitted { program, registry, graph, .. } = a;
        let arm = &mut graph.as_mut().expect("arms").arms[i];
        core::mem::swap(program, &mut arm.program);
        core::mem::swap(registry, &mut arm.registry);
    }
}

impl Drop for ArmSwap<'_> {
    fn drop(&mut self) {
        Self::swap(self.a, self.i);
    }
}

#[allow(clippy::too_many_arguments)]
fn run_parallel(mcx: Mcx<'static>, entry: &mut gravity_driver::Entry, scratch: &mut PlanScratch, a: &mut Admitted, key: Option<PlanKey>, snap: &Rc<SnapshotData<'static>>, params: &[u64], nulls: &[u8], cid: u32, cmd: Cmd, remote: bool, dest: &mut DestReceiver<'_>, wire: Option<&mut gravity_wire::WireNode>, formats: Option<&[i16]>) -> PgResult<Ran> {
    let width = crate::inputs::knobs::width::statement_width(&a.program);
    let key = key.or_else(|| park_key(mcx, a));
    let opens_before = granule_source::registry_opens();
    let granules_before = granule_source::granule_census();
    // pglserve (D4601): the census only the served line (PGRUST_GRAVITY_REPORT) and the PHASES line print is read when either
    // is armed (`PGRUST_GRAVITY_SERVE_LEAN=0`: always, as before); EXPLAIN ANALYZE's record reads opens, granules, claims and
    // the spill bytes, kept above and below
    let census = crate::phases::armed() || !gravity_driver::env::serve_lean();
    let touch_before = if census { granule_source::first_touch_census() } else { [0; 3] };
    let io_before = if census { granule_source::io_census() } else { [0; 8] };
    let dict_before = if census { granule_source::dict_census() } else { [0; 5] };
    let dictent_before = if census { granule_source::entry_census() } else { [0; 6] };
    let elided_before = if census { granule_source::lane_elided_census() } else { 0 };
    if census {
        granule_source::take_io_depth();
    }
    let _placed = gravity_parallel::place();
    // D3865 L4: one emit and on-demand InitPlans run outside the pool (`inits_only`, no grant site reserved for it)
    let (lease, _) = crate::inputs::knobs::grant::GrantLease::or_admit(&a.program, a.bitmap_site(), width, || Ok((admit_grant(&a.program, &crate::admit::bound_all(a), width, a.pstmt)?, width)))?;
    let grant = lease.grant();
    // D4542: the plan's vectors go back to the scratch at every exit, an error's or a cancel's too (plan.rs `Planned`)
    let mut planned = Planned::new(RunnerPlan::prepare(mcx, a, snap, width, grant, params, nulls, key, part_release(&a.program, cmd, false), scratch.take())?, scratch);
    let plan = planned.plan();
    plan.resolved.cells_from(&a.registry); // D300: this execution's verdict cells for the sources (a parked Node's are its builder's)
    crate::phases::mark("prepare");
    let parts = glue::binding_parts(snap, cid, &plan.params[..plan.nparams], &plan.nulls[..plan.nparams], Some(&*plan.resolved));
    a.registry.rewind();
    let dctx;
    let mut inner = if remote {
        let w = wire.expect("select without a wire node");
        w.prepare(&a.program, formats)?;
        Receiver::Wire(w.reset())
    } else {
        dctx = gravity_driver::mem::root::Root::new("GravityDestSink");
        let mcx: Mcx<'static> = super::raw::sink_mcx(&dctx);
        let desc = desc_of(mcx, &a);
        Receiver::Dest(DestSink::new(mcx, desc, dest))
    };
    let natts = a.program.out.len();
    // KEY-PREDICTED-ROWS road (b): this statement's estimates (exec admit `bound_all`), not the ones the program was lowered with
    let predicted = crate::admit::bound_all(&a).iter().filter(|p| p.ends_in == End::Sink).map(|p| p.predicted_rows).max().unwrap_or(0);
    // rows a parallel pipeline emits (a Sink pipeline over a source, or the join's phases) must be ordered by the drain
    let hold = a.nsort > 0 && a.program.pipelines.iter().any(|p| p.driving_site >= 0 && p.is_eligible() && (p.ends_in == End::Sink || matches!(p.ends_in, End::Breaker(Breaker { kind: BreakerKind::Materialize, .. }))));
    // per column its typlen when by reference, 0 by value: a leg copies an image pushed outside its block's region (D470)
    let typlen: Vec<i16> = a.program.out.iter().map(|c| if c.typbyval { 0 } else { c.typlen }).collect(); // alloc: Cold
    // the rows the legs hold feed no breaker, and the claim cursors were cut before any slot
    // owner: both the statement's, or the executing portal's (R3: the read the Node's bind makes, taken before the legs
    // are built; None outside a statement: uncharged)
    let stmt_owner = gravity_driver::stmt_context().map(|(o, _)| o);
    let mut legs = LegSink::new(&mut inner, natts, width as usize, (predicted as usize).min(1 << 20), hold, a.nsort > 0, &typlen, stmt_owner);
    let mut binding = Binding::new(parts, &mut legs);
    crate::phases::mark("sink");
    let stats0 = entry.stats();
    let mut exec = entry.start_at(key, &a.program, &mut a.registry, &mut binding, lease.reservation(), gravity_driver::Widths::Planned(&plan.widths)).map_err(glue::pg_error)?;
    crate::phases::mark("start");
    plan.charge_claims(stmt_owner);
    exec.set_slot_salt(recipe_salt(a));
    let acc0 = if census { exec.node().accounting() } else { Default::default() };
    let pool: &(dyn Pool + Sync) = installed_pool().unwrap_or(&NoPool);
    let claims_before = pool.claims_run();
    let sections_before = if census { pool.sections_run() } else { 0 };
    // the PHASES line's counters (pglserve D4601: read only under PGRUST_GRAVITY_PHASES, or with the switch off)
    let phased = crate::phases::on() || !gravity_driver::env::serve_lean();
    let load = |c: &core::sync::atomic::AtomicU64| if phased { c.load(core::sync::atomic::Ordering::Relaxed) } else { 0 };
    let census0: [u64; 4] = core::array::from_fn(|i| load(&gravity_driver::SPILL_CENSUS[i]));
    let t3c0: [u64; 4] = core::array::from_fn(|i| load(&gravity_kernels::ops::sort::topn::t3::CENSUS[i]));
    let t3p0: [u64; 4] = core::array::from_fn(|i| load(&gravity_kernels::ops::sort::topn::t3::CENSUS2[i]));
    let cv0: [u64; 2] = core::array::from_fn(|i| load(&gravity_kernels::ops::sort::topn::t3::CONVERT[i]));
    let end0: [u64; 9] = core::array::from_fn(|i| load(&gravity_driver::END_US[i]));
    let files0: [u64; 3] = core::array::from_fn(|i| load(&gravity_driver::END_FILES[i]));
    let ceil0 = load(&gravity_driver::mem::grow::AT_CEILING);
    let tbl0 = if phased { (granule_source::tables_built(), granule_source::tables_shared(), gravity_hosts::dict::text_hosts::derived_entries()) } else { (0, 0, 0) };
    gravity_kernels::ops::sort::topn::t3::CENSUS_ON.store(crate::phases::on(), core::sync::atomic::Ordering::Relaxed);
    let toast = heaptoast::worker::publish(plan.toast.clone());
    // D610: a write or lock pipeline's recheck is the leader's EvalPlanQual (epq::recheck_leader), then the pipeline resumes;
    // D655: the plan reruns the producers it reads on the recheck node
    let r = gravity_parallel::run_program_rechecking(entry, &mut exec, &mut binding, &mut a.registry, pool, &*plan, &mut |e, x, b, r, site| crate::rerun::epq::recheck_leader(e, x, b, r, key, site, Some(&*plan)));
    core::mem::drop(toast);
    crate::phases::mark("run");
    // ===== kernels track (pass 4): a deferred table-lane fault is re-evaluated serially (expr.md §2.2 item 4) =====
    let r = match r {
        Err(gravity_driver::Error::Fault { site, lane, code, part }) => Err(reevaluate_fault(&mut exec, &a.registry, site as usize, lane as usize, code, part)),
        r => r,
    };
    // ===== end =====
    // the bytes: the spill files', and the owners' regions mapped from temp files (a refused owner draw's spill exit)
    let spill = (exec.node().spill_parts_of(0), exec.node().spilled_bytes() + exec.node().owner_mapped_bytes(), exec.node().spill_generations());
    let acc = if census { exec.node().accounting() } else { Default::default() };
    let p2 = (acc.merged - acc0.merged, (acc.p2_refused_at[0] - acc0.p2_refused_at[0]) * 100 + (acc.p2_refused_at[1] - acc0.p2_refused_at[1]) * 10 + (acc.p2_refused_at[2] - acc0.p2_refused_at[2]));
    let p2 = (p2.0, p2.1, if acc.p2_refused_at[2] != acc0.p2_refused_at[2] { acc.p2_fail } else { [0; 4] });
    // D374: the run's error first (a cancel's or a timeout's own ERROR, from the interrupt still pending), then the end
    let r = r.map_err(glue::pg_error).and_then(|v| plan.v4.end().map(|()| v));
    let ended = end_entry(entry, exec, r.is_ok());
    crate::phases::mark("end");
    r?;
    ended?;
    // D609: a write's count is the rows it wrote (the driver's add_rows, as run_once), not the rows its sink emitted
    let written = binding.rows_processed;
    legs.drain(a.sort()).map_err(glue::pg_error)?;
    crate::phases::mark("drain");
    if crate::phases::on() {
        let c: [u64; 4] = core::array::from_fn(|i| gravity_driver::SPILL_CENSUS[i].load(core::sync::atomic::Ordering::Relaxed) - census0[i]);
        let st = entry.stats();
        let k: [u64; 4] = core::array::from_fn(|i| gravity_kernels::ops::sort::topn::t3::CENSUS[i].load(core::sync::atomic::Ordering::Relaxed) - t3c0[i]);
        let k2: [u64; 4] = core::array::from_fn(|i| gravity_kernels::ops::sort::topn::t3::CENSUS2[i].load(core::sync::atomic::Ordering::Relaxed) - t3p0[i]);
        let cv: [u64; 2] = core::array::from_fn(|i| gravity_kernels::ops::sort::topn::t3::CONVERT[i].load(core::sync::atomic::Ordering::Relaxed) - cv0[i]);
        // D3203: the end phase's parts (gravity_driver::END_US: leader slots / leader chunks + spills / workers' regions /
        // remove / reset to the spill table / the spill table / the rest of the reset / worker slots + release log / park)
        let ep: [u64; 9] = core::array::from_fn(|i| gravity_driver::END_US[i].load(core::sync::atomic::Ordering::Relaxed) - end0[i]);
        // D3203: the end's spill files: taken to delete across the pool / their MiB / the ones the leader deleted itself
        let ef: [u64; 3] = core::array::from_fn(|i| gravity_driver::END_FILES[i].load(core::sync::atomic::Ordering::Relaxed) - files0[i]);
        let w = width.max(1) as f64 * 1e6;
        let tbl = plan.resolved.tables().held();
        let (ag, at) = plan.admitted.get();
        let ex = plan.exported.get();
        let hx = [0u64; 2]; // the ladder's h_exact census (ladder-final)
        let rel = plan.resolved.part_release_counts();
        eprintln!("PHASES stmt width={width} us:{} build={} apply={} provision={} zero={} bind={} scatter={:.2}ms/{} refill={:.2}ms/{} t3/thr: read={:.2} fold={:.2} drain={:.2} claims={} pass2/thr: read={:.2} fold={:.2} parts={}/{} convert/thr={:.2}ms/{} tables={}/{}/{} held={}/{}MiB pass2_granules={ag}/{at} exported={ex} exact={}/{} released={} reopened={} private={} cache={}/{}MiB cache_held={}MiB endparts={}/{}/{}/{}/{}/{}/{}/{}/{} endfiles={}/{}MiB/{} at_ceil={} plan_order={}", crate::phases::line(), st.build_us - stats0.build_us, st.apply_us - stats0.apply_us, st.provision_us - stats0.provision_us, st.zero_us - stats0.zero_us, st.bind_us - stats0.bind_us - (st.zero_us - stats0.zero_us), c[0] as f64 / 1e6, c[1], c[2] as f64 / 1e6, c[3], k[0] as f64 / w, k[1] as f64 / w, k[2] as f64 / w, k[3], k2[0] as f64 / w, k2[1] as f64 / w, k2[2], k2[3], cv[0] as f64 / w, cv[1], granule_source::tables_built() - tbl0.0, granule_source::tables_shared() - tbl0.1, gravity_hosts::dict::text_hosts::derived_entries() - tbl0.2, tbl.0, tbl.1 >> 20, hx[0], hx[1], rel.0, rel.1, plan.resolved.part_release_private(), granule_source::part_cache_mib().0, granule_source::part_cache_mib().1, granule_source::part_cache_mib().2, ep[0], ep[1], ep[2], ep[3], ep[4], ep[5], ep[6], ep[7], ep[8], ef[0], ef[1] >> 20, ef[2], gravity_driver::mem::grow::AT_CEILING.load(core::sync::atomic::Ordering::Relaxed) - ceil0, u8::from(super::in_plan_order())); // alloc: Cold
    }
    let cold = if census && sections_before == 0 && pool.sections_run() > 0 { pool.cold_micros() } else { (0, 0) };
    let used = plan.widths.iter().copied().max().unwrap_or(0);
    let ran = Ran { rows: if cmd.writes() { written } else { legs.through() }, width: if used >= 2 { used } else { 0 }, sections: plan.sections.get(), claims: pool.claims_run() - claims_before, opens: granule_source::registry_opens() - opens_before, granules: { let g = granule_source::granule_census(); [g[0] - granules_before[0], g[1] - granules_before[1], g[2] - granules_before[2], g[3] - granules_before[3], g[4] - granules_before[4]] }, first_touch: if !census { [0; 3] } else { let t = granule_source::first_touch_census(); [t[0] - touch_before[0], t[1] - touch_before[1], t[2] - touch_before[2]] }, cold, spill, p2, io: if !census { [0; 9] } else { let i = granule_source::io_census(); [i[0] - io_before[0], i[1] - io_before[1], i[2] - io_before[2], i[3] - io_before[3], i[4] - io_before[4], i[5] - io_before[5], i[6] - io_before[6], i[7] - io_before[7], granule_source::take_io_depth()] }, dict: if !census { [0; 5] } else { let d = granule_source::dict_census(); [d[0] - dict_before[0], d[1] - dict_before[1], d[2] - dict_before[2], d[3] - dict_before[3], d[4] - dict_before[4]] }, dictent: if !census { [0; 6] } else { let d = granule_source::entry_census(); [d[0] - dictent_before[0], d[1] - dictent_before[1], d[2] - dictent_before[2], d[3] - dictent_before[3], d[4] - dictent_before[4], d[5] - dictent_before[5]] }, elided: if census { granule_source::lane_elided_census() - elided_before } else { 0 }, node: _placed.idx as u8 };
    Ok(ran)
}

/// The park key of an unkeyed statement, serial or parallel (the simple protocol admits a fresh plan per statement): the program's content and the
/// salt of what the content does not carry, so the Node with its bound sources serves the next statement of the same shape. C3 S4c: the Node
/// serves only the program it was built over (driver `Entry::acquire`), which a change to anything the program read retires (its recorded
/// Deps); a relation's relcache entry rebuilt under it releases the Node's state on it (D1507, `slots::note_released`).
///
/// A writer parks too (H16 (b), C07), after the field audit of what its Node keeps of the admitting statement (hosts `WriteHost`, registry
/// `derive_writer`): the recipe, the index maintenance recipes and the pruned root come from the WritePlan, which the content key hashes; the heap,
/// slots, triggers, constraints, generated columns, partition check, ON CONFLICT state and check states are bound per bind from the relcache (a
/// change retires the program) or live in the per-bind context; `modified` (the statement's insertedCols / updatedCols, pointers into its plan) is copied
/// into the host's node context at its first bind, and its words are salted here; `checks` (RLS / WITH CHECK OPTION quals relocated into THIS
/// plan) is never carried across statements: a statement with checks parks nothing, and its key being `None`, it never takes a Node built
/// without them. A MERGE and a routed or pruned-partition write (RouteHost, the root's statement triggers) are not audited and never park, nor
/// does a program with a writer beside its write (a writing CTE's: D3474): that host keeps this statement's check quals and MERGE subcommands
/// (beside.rs `set`, hosts `derive_writer_beside`), which `plain_write` / `has_checks` (the top ModifyTable's lists) never see. A
/// locker (`LockHost`) keeps only its WritePlan recipe. The DELETE is M1's one served writer (C07 C51 C95; m1_bound SW1-SW10 are its pairs);
/// an INSERT or UPDATE joins this arm with its own pairs when its row op serves (M4): the audit above already covers both.
/// PARK-W (M4-OLTP-HOT) parks an INSERT or UPDATE under the guard set `plain_write` (plan) + `plain_target` (relcache)
/// below; AGB-14 widened it to what the audit covers, as the DELETE's: a target with triggers (RI ones included), a
/// stored generated column, a partition written directly and ON CONFLICT now park too (`WriteHost::bind_pg` binds the
/// trigger host, and `unbind_pg` drops it with the generated, partition-check and arbiter states, per execution; the
/// conflict action and arbiters are the WritePlan's, in the content key). Anything else keeps building its Node per statement.
pub fn park_key(mcx: Mcx<'static>, a: &Admitted) -> Option<PlanKey> {
    park_key_if(a, |rel| plain_target(mcx, rel))
}

/// `park_key` with an INSERT's or UPDATE's target guard (`plain_target`) asked of `target` (D3881: the pool of kept
/// executions keeps no writer's Node, `|_| false`).
pub(crate) fn park_key_if(a: &Admitted, target: impl FnOnce(u32) -> bool) -> Option<PlanKey> {
    let writer = parks(a.cmd, a.program.write.route.is_some(), a.pstmt)?;
    if !a.program.writes.is_empty() {
        return None;
    }
    if matches!(a.cmd, Cmd::Insert | Cmd::Update) && !target(a.program.write.target_rel) {
        return None;
    }
    let salt = a.memo.salt(a)?;
    // D3253: any admission (another statement) may take a content key's Node, so it must hold nothing of this one. Every
    // closure over this admission's parameter slots is counted in `filters` and sits in a recipe whose salt is None.
    if a.key_exprs.filters > 0 {
        fires::fire("park.content_refused_filters");
        return None;
    }
    let salt = if writer { modified_salt(salt, a.pstmt.and_then(crate::admit::modified_cols)) } else { salt };
    let c = a.program.key.0;
    Some(PlanKey { handle: Handle(((c as u64) ^ ((c >> 64) as u64) ^ salt) | PlanKey::CONTENT), generation: Generation(0) })
}

/// Whether a statement's Node may park, and whether it writes: a SELECT, an unrouted DELETE whose ModifyTable has no
/// `withCheckOptionLists` (the plan's checks, bound per statement: view CHECK OPTION, RLS, MERGE's RLS delete check), or an
/// unrouted INSERT / UPDATE whose plan passes `plain_write` (its target then passes `plain_target` in `park_key`).
fn parks(cmd: Cmd, routed: bool, pstmt: Option<&PlannedStmt<'_>>) -> Option<bool> {
    match cmd {
        Cmd::Select => Some(false),
        Cmd::Delete if !routed && !has_checks(pstmt) => Some(true),
        Cmd::Insert | Cmd::Update if !routed && plain_write(pstmt) => Some(true),
        _ => None,
    }
}

/// PARK-W's plan guards for an INSERT / UPDATE: one result relation, no partition root (`rootRelation`: a routed or
/// pruned-partition write, RouteHost / the root's statement triggers, not audited), no check quals (`withCheckOptionLists`:
/// view CHECK OPTION, RLS WITH CHECK, relocated into THIS plan), and a plan (a statement without one never parks). ON
/// CONFLICT parks (AGB-14): its action and arbiters are the WritePlan's (content key), its SET / WHERE the program's, its
/// arbiter index state and lock mode per bind. RETURNING parks (oltpfix), as a DELETE .. RETURNING always has: its list
/// is the program's (content key), and what the host keeps for it (the deleted or replaced version, `WritePlan.old`'s
/// slot, the system columns) is the row op's, cleared at every unbind (`WriteHost::clear_deleted`); a RETURNING write
/// built its Node at every statement (C84 / C52 / C30 / C21 simple: 10-30 us over the row executor). The extended
/// protocol's RETURNING portal (its hold store) runs on the entry path, which parks nothing of this.
fn plain_write(pstmt: Option<&PlannedStmt<'_>>) -> bool {
    let Some(mt) = pstmt.and_then(|p| p.planTree).and_then(|n| n.as_modify_table()) else { return false };
    mt.resultRelations.len() == 1 && mt.rootRelation == 0 && mt.withCheckOptionLists.is_nil()
}

/// PARK-W's target guards, read from the relcache (the statement holds the target's lock): a plain table (not a view, a
/// partitioned table or a foreign table) without row-level security. Triggers (`rd_hastriggers`: row, statement, RI,
/// transition tables), a stored generated column and a partition's own constraint are bound per execution (AGB-14).
/// A relation that cannot be opened parks nothing.
fn plain_target(mcx: Mcx<'static>, relid: u32) -> bool {
    let Ok(rel) = table::table_open(mcx, relid as types_core::Oid, types_storage::lock::NoLock) else { return false };
    let ok = plain_target_rel(&rel);
    let _ = rel.close(types_storage::lock::NoLock);
    ok
}

fn plain_target_rel(rel: &types_rel::Relation<'_>) -> bool {
    rel.rd_rel.relkind == types_rel::RELKIND_RELATION && !rel.rd_rel.relrowsecurity
}

fn has_checks(pstmt: Option<&PlannedStmt<'_>>) -> bool {
    pstmt.and_then(|p| p.planTree).and_then(|n| n.as_modify_table()).is_some_and(|mt| !mt.withCheckOptionLists.is_nil())
}

/// The park key's salt (H16 (b) B1, FRAME's design): what the content key does not carry of the statement's
/// sources, so a parked Node serves only a statement whose recipes it holds. An all-granule statement keeps the slot depot's
/// `recipe_salt`; otherwise the database and each entry's identity in registry order: a heap source's relation and width,
/// a granule source's `recipe_salt` mix, an index source's `SiteRecipe::park_salt` (its keys, lanes, walk and search-array
/// images; `None` for a recipe with a filter, a closure over the admitting plan). Any other entry parks nothing. C3 S4c: an
/// index, ALTER, TRUNCATE, ANALYZE, ACL or pg_proc change to what the program read retires the program, and with it the Node
/// (driver `Entry::acquire` serves a Node only to the program it was built over).
/// A one-index bitmap entry (`HostRegistry::bitmap_site`) mixes its recipe's salt behind a mark (an index entry over the
/// same recipe derives another source): its recheck qual is compiled per execution in the execution's context, and its
/// source drops it at the unbind and takes the executing statement's at each bind (hosts `Resolved::recheck`, filed by
/// `cells_from`). A BitmapAnd / BitmapOr tree's or a parameter rung's bitmap parks nothing (M2-HEAP).
fn park_salt(a: &Admitted) -> Option<u64> {
    // the depot-fetch switch is the slot depot's alone: a fetch site still parks no Node
    if let Some(s) = depot_salt(&a.registry, init_small::globals::MyDatabaseId() as u64, false) {
        return Some(s);
    }
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let mut mix = |w: u64| {
        for b in w.to_le_bytes() {
            h = (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    mix(init_small::globals::MyDatabaseId() as u64);
    for i in 0..a.registry.len() {
        if let Some(r) = a.registry.range_recipe(i) {
            mix(r.heap as u64);
            mix(r.natts as u64);
        } else if let Some(r) = a.registry.granule_recipe(i) {
            mix_granule(&mut mix, r);
        } else if let Some(r) = a.registry.bitmap_site(i) {
            mix(u64::MAX);
            mix(r.park_salt()?);
        } else {
            mix(a.registry.recipe(i)?.park_salt()?);
        }
    }
    Some(h)
}

/// A writer's modified columns into its park salt (`park_key`): two statements of one program may write other columns (`INSERT (c1)` and
/// `INSERT (c1, c2) … NULL` share a plan shape), and the parked host keeps the columns of the statement that built it.
fn modified_salt(mut h: u64, cols: Option<gravity_hosts::ops::modifytable::write_constraints::ModifiedCols>) -> u64 {
    let mut mix = |w: u64| {
        for b in w.to_le_bytes() {
            h = (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    match cols {
        None => mix(u64::MAX),
        Some((ins, upd)) => {
            for s in [ins, upd] {
                mix(s.nwords() as u64);
                s.as_words().iter().for_each(|&w| mix(w));
            }
        }
    }
    h
}

/// The slot depot's salt (§4.5): the database and every granule recipe's identity (relation,
/// lanes, widths, kinds, table specs) — what the program's content key does not carry and a
/// parked worker slot would otherwise serve across. `None` unless every site is a granule
/// source: a heap or index source holds backend-bound state and its slots stay with the Node.
/// Under the depot-fetch switch (`gravity_driver::slots::depot_fetch`) a by-rowid fetch site counts as
/// a granule source does, its recipe's identity mixed in (`depot_salt`).
pub(super) fn recipe_salt(a: &Admitted) -> Option<u64> {
    depot_salt(&a.registry, init_small::globals::MyDatabaseId() as u64, gravity_driver::slots::depot_fetch())
}

/// `recipe_salt` over a registry and a database: `fetch` lets a fetch site's recipe in (D4709: a `SELECT *` top-N's
/// late materialization held its whole statement out of the depot, so every run built each worker slot and bound its
/// buffers again on the leader). A fetch source keeps nothing bound to the backend between statements: its unbind moves
/// its kept cursors' uses past the statement, and the depot's hooks (`kept_uses_to`, `transfer_held`, `release_rel`)
/// cover its kept parts and buffers as a granule source's.
fn depot_salt(reg: &gravity_hosts::ops::HostRegistry, db: u64, fetch: bool) -> Option<u64> {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let mut mix = |w: u64| {
        for b in w.to_le_bytes() {
            h = (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    mix(db);
    for i in 0..reg.len() {
        if let Some(r) = reg.granule_recipe(i) {
            mix_granule(&mut mix, r);
        } else if let Some(r) = reg.fetch_recipe(i).filter(|_| fetch) {
            mix_fetch(&mut mix, r);
        } else {
            return None;
        }
    }
    Some(h)
}

/// One fetch recipe's identity into a salt, behind a mark (a fetch site never matches a granule entry of the same
/// relation): relation, then each column's attno and kind.
fn mix_fetch(mix: &mut impl FnMut(u64), r: &gravity_hosts::ops::scan::pgrc::fetch_site::FetchRecipe) {
    mix(u64::MAX - 1);
    mix(r.rel as u64);
    mix(r.natts as u64);
    for k in 0..r.natts {
        mix(r.attnos[k] as u16 as u64 | (r.kinds[k] as u64) << 16);
    }
}

/// One granule recipe's identity into a salt: relation, lanes, widths, kinds, table specs.
fn mix_granule(mix: &mut impl FnMut(u64), r: &gravity_hosts::ops::scan::pgrc::granule_source::GranuleRecipe) {
    mix(r.rel as u64);
    mix(r.natts as u64);
    for k in 0..r.natts {
        mix(r.attnos[k] as u64 | (r.widths[k] as u64) << 16 | (r.kinds[k] as u64) << 24);
    }
    for t in r.tables.iter() {
        mix(t.fingerprint());
    }
}

/// D3865: what an admission's program and recipes alone decide, worked out at its first execution and kept with it (a
/// memo-served or shelved admission runs again at every call): its park salt (`park_key`) and a portal's part release
/// (`pull_release`), and each Init's members by its pipeline (ops initplan `lay`). Each is a function of the admission,
/// never of the execution, so nothing but the admission keys it.
#[derive(Default)]
pub(crate) struct AdmitMemo {
    salt: Cell<Option<Option<u64>>>,
    release: core::cell::RefCell<Option<PartRelease>>,
    members: core::cell::RefCell<Vec<Option<Box<[u16]>>>>,
}

impl AdmitMemo {
    /// The park salt (`park_salt`), worked out at the first ask.
    fn salt(&self, a: &Admitted) -> Option<u64> {
        if let Some(s) = self.salt.get() {
            return s;
        }
        let s = park_salt(a);
        self.salt.set(Some(s));
        s
    }

    /// Pipeline `p`'s members as `f` works them out (an error is never kept: the next execution meets it again).
    pub(crate) fn members(&self, npipelines: usize, p: usize, f: impl FnOnce() -> PgResult<Box<[u16]>>) -> PgResult<Box<[u16]>> {
        if let Some(m) = self.members.borrow().get(p).and_then(|m| m.as_ref()) {
            return Ok(m.clone()); // alloc: Exec
        }
        let m = f()?;
        let mut all = self.members.borrow_mut();
        if all.len() < npipelines {
            all.resize(npipelines, None); // alloc: Program
        }
        if let Some(slot) = all.get_mut(p) {
            *slot = Some(m.clone()); // alloc: Exec
        }
        Ok(m)
    }
}

/// `pull_release` of the admission's program, kept in its memo.
pub(super) fn pull_release_of(a: &Admitted, rewinds: bool) -> PartRelease {
    if let Some(r) = a.memo.release.borrow().as_ref() {
        return r.clone(); // alloc: Exec
    }
    let r = pull_release(&a.program, a.cmd, rewinds);
    *a.memo.release.borrow_mut() = Some(r.clone()); // alloc: Exec
    r
}

/// What a statement's granule relations give back while it runs (`part_release`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PartRelease {
    /// Every part the statement opened stays in use to its end.
    Off,
    /// D3268: each part's use, once every claim left it; its dictionaries stay in the list's cells.
    Uses,
    /// D3823: as `Uses`, and past the part cache's level its dictionaries are its own and go with each part: its breaker
    /// keeps code words, no dictionary image (`mempolicy::release::dicts_go_with_part`).
    UsesDicts,
    /// D2985: each part's use and, past the part cache's budget, its dictionaries (the statement's own cells).
    Dicts,
    /// D3823 (R3b): a program over several sites: per listed relation (oid, readers: the sites over it, dicts), each part's
    /// use once every reader's claims left it, as `Uses` does, and with `dicts` its dictionaries past the part cache's
    /// level, as `UsesDicts` does (every site over it feeds a breaker that keeps code words); the relations not listed keep
    /// theirs to the end.
    Rels(Box<[(u32, u32, bool)]>),
}

impl PartRelease {
    pub(super) fn apply(self, resolved: &mut granule_source::Resolved) {
        resolved.set_part_release(self == PartRelease::Dicts);
        resolved.set_part_uses_release(matches!(self, PartRelease::Uses | PartRelease::UsesDicts));
        resolved.set_part_own_dicts(self == PartRelease::UsesDicts);
        if let PartRelease::Rels(rels) = self {
            resolved.set_part_own_dicts_rels(rels.iter().filter(|r| r.2).map(|r| r.0).collect()); // alloc: Exec
            resolved.set_part_uses_rels(rels.iter().map(|r| (r.0, r.1)).collect()); // alloc: Exec
        }
    }
}

/// How the statement's parts go back while it runs. A SELECT whose pipeline 0 drives the program's one site, a granule
/// source, with no outer sink, row locks or writes:
/// - D2985 `Dicts`: its only pipeline streams into the statement's own receiver. The receiver takes each row before the
///   source's next window (a worker's leg copies every by-reference image at push), so no datum outlives the claim that
///   read its part: its dictionaries are its own past the part cache's budget and released part by part.
/// - D3268 / D3823 `Uses`: pipeline 0 ends in a breaker (not a two-pass one), the runtime never runs it again (no rerun
///   set holds it) and every later pipeline reads breaker slots, no site. The scan never reads a part again once every
///   claim left it, so the part's use is released; its dictionaries stay in the list's cells. D3003 holds for every breaker
///   kind: a by-reference datum a breaker keeps past its window is copied at the retain (the source's arena, packed
///   numerics and in-place lanes live a window: an aggregate's state, a Sort's or a Materialize's runs, a hash build's
///   window images), or it is a dictionary's (a HashAgg / Distinct code key, a TopK's code words and rendered pointers, a
///   hash build's Code cells), owned by the list's cell through the statement, which a render after the release reads
///   without the part (`dict_value_pg`). Before, an aggregate held every part it read to its end: 22 GB of part cache past
///   a 3.6 GB budget, out of memory on a 30 GB box (D3268); a Sort, TopK, hash build, Distinct or Materialize over
///   one site still did (D3823: 2-6 GB of in-use parts at memtest's stress kills, D3821).
/// - D3823 (R3b) `Rels`: a program over several sites (a join's build and probe, a scan beside an InitPlan's): each relation
///   whose every site is read once by claim (`read_once`) releases its parts' uses once all of its sites' claims left each
///   part (the credit counts granules times its readers); a relation with a site read again (a nest's inner, a SubPlan's,
///   a CTE re-run, a fetch site's row-id reads, an undriven seek per outer row) keeps every part to the end. The breakers
///   keep D3003 as for one site (`Uses`).
pub(super) fn part_release(program: &Program, cmd: Cmd, outer: bool) -> PartRelease {
    let Some(p0) = program.pipelines.first() else { return PartRelease::Off };
    if program.sites.len() > 1 {
        return if outer || !matches!(cmd, Cmd::Select) || !program.locks.is_empty() || !program.writes.is_empty() { PartRelease::Off } else { rels_read_once(program) };
    }
    let scan = !outer
        && matches!(cmd, Cmd::Select)
        && program.sites.len() == 1
        && program.locks.is_empty()
        && program.writes.is_empty()
        && p0.driving_site == 0
        && p0.is_eligible()
        && matches!(p0.claims, gravity_program::ClaimKind::Source);
    if !scan {
        return PartRelease::Off;
    }
    if program.pipelines.len() == 1 && matches!(p0.ends_in, gravity_program::End::Sink) {
        return PartRelease::Dicts;
    }
    let breaker = matches!(p0.ends_in, End::Breaker(_));
    // a rerun set's member runs again at its owner's re-open (a nest's inner, a SubPlan's call): its scan reads its parts again
    let reruns = program.v4.as_deref().is_some_and(|v| v.lifetimes.member(0).is_some());
    if breaker && !reruns && p0.admit.is_none() && program.pipelines.len() > 1 && program.pipelines[1..].iter().all(|p| p.driving_site < 0) {
        return if mempolicy::release::dicts_go_with_part(keeps_dict_images(&p0.ends_in)) { PartRelease::UsesDicts } else { PartRelease::Uses };
    }
    PartRelease::Off
}

/// D3003: the breaker keeps images of its scan's dictionaries by reference past the window that read them: a TopK's
/// rendered rows, a hash build's code cells. The others keep code words (an aggregate's or a DISTINCT's code keys, a
/// min / max's carried code, rendered into its record's own copy) or copy at the retain (a Sort's or a Materialize's runs).
fn keeps_dict_images(end: &End) -> bool {
    matches!(end, End::Breaker(b) if matches!(b.kind, BreakerKind::TopK | BreakerKind::HashBuild))
}

/// D3823: the pipeline that drives site `s` ends in a breaker that keeps no image of the site's dictionaries
/// (`keeps_dict_images`); a stream into the sink or the emit keeps rows by reference.
fn codes_only(program: &Program, s: usize) -> bool {
    program.pipelines.iter().find(|p| usize::try_from(p.driving_site).ok() == Some(s)).is_some_and(|p| matches!(p.ends_in, End::Breaker(_)) && mempolicy::release::dicts_go_with_part(keeps_dict_images(&p.ends_in)))
}

/// D3823 (R3b): site `s` is read once by claim: exactly one pipeline drives it, claiming Source (an eligible one), with no
/// two-pass admit (pass 2 reads the parts again), and no rerun set runs that pipeline again. A site no pipeline drives (a
/// nest's inner sought per outer row, a fetch site's row-id reads) is not.
fn read_once(program: &Program, s: usize) -> bool {
    let reruns = |i: usize| program.v4.as_deref().is_some_and(|v| u16::try_from(i).is_ok_and(|i| v.lifetimes.member(i).is_some()));
    let mut drivers = program.pipelines.iter().enumerate().filter(|(_, p)| usize::try_from(p.driving_site).ok() == Some(s));
    let Some((i, p)) = drivers.next() else { return false };
    drivers.next().is_none() && p.is_eligible() && matches!(p.claims, gravity_program::ClaimKind::Source) && p.admit.is_none() && !reruns(i)
}

/// D3823 (R3b): the relations of a program over several sites whose every site is read once (`read_once`), each with its
/// readers (the sites over it) and whether its dictionaries go with its parts (D3823: the pipeline driving each of its
/// sites ends in a breaker that keeps code words, `mempolicy::release::dicts_go_with_part`); `Off` when there is none.
fn rels_read_once(program: &Program) -> PartRelease {
    let mut rels: Vec<(u32, u32, bool)> = Vec::new(); // alloc: Exec
    let mut out: Vec<u32> = Vec::new(); // alloc: Exec
    for (s, d) in program.sites.iter().enumerate() {
        if d.relation == 0 || out.contains(&d.relation) {
            continue;
        }
        if !read_once(program, s) {
            out.push(d.relation);
            rels.retain(|r| r.0 != d.relation);
            continue;
        }
        let dicts = codes_only(program, s);
        match rels.iter_mut().find(|r| r.0 == d.relation) {
            Some(r) => {
                r.1 += 1;
                r.2 &= dicts;
            }
            None => rels.push((d.relation, 1, dicts)),
        }
    }
    if rels.is_empty() { PartRelease::Off } else { PartRelease::Rels(rels.into_boxed_slice()) }
}

/// D3823 (R3c): a portal's release (`Producers::prepare`: the pull adapter's producers and the emit it pulls; the outer
/// sink of a portal is the portal itself): `part_release`'s per-site decision, uses only. A one-site stream's `Dicts`
/// becomes `Uses`: the pull buffer keeps the rows a fetch did not take past the claim that read them, their dictionary
/// images by reference (driver `pull::relocate`), so a dictionary stays the list cell's to the portal's close. A FETCH-paced
/// scan gives each part back once its claims are done; a cursor abandoned after a few fetches holds only the parts its
/// claims are in, not every part its producers read. A program a soft rewind runs again (`rewinds`: D2701 reruns the emit
/// over the producers' kept slots) keeps the parts of the sites its emit drives: the rerun reads them again.
pub(super) fn pull_release(program: &Program, cmd: Cmd, rewinds: bool) -> PartRelease {
    let emit = program.pipelines.iter().rposition(|p| !p.post);
    let emit_drives = |s: usize| emit.and_then(|e| program.pipelines.get(e)).is_some_and(|p| usize::try_from(p.driving_site).ok() == Some(s));
    match part_release(program, cmd, false) {
        PartRelease::Dicts if rewinds && emit_drives(0) => PartRelease::Off,
        PartRelease::Dicts => PartRelease::Uses,
        PartRelease::Rels(rels) if rewinds => {
            let reread = |rel: u32| program.sites.iter().enumerate().any(|(s, d)| d.relation == rel && emit_drives(s));
            let kept: Box<[(u32, u32, bool)]> = rels.iter().copied().filter(|r| !reread(r.0)).collect(); // alloc: Exec
            if kept.is_empty() { PartRelease::Off } else { PartRelease::Rels(kept) }
        }
        r => r,
    }
}

/// The statement's granule relations for a serial run (`run_once`): every granule recipe's part list under the snapshot, the tables capped by the grant.
pub(super) fn resolve_serial(mcx: Mcx<'static>, registry: &HostRegistry, snap: &Rc<SnapshotData<'static>>, grant: Grant, part_release: PartRelease) -> PgResult<granule_source::Resolved> {
    let mut resolved = granule_source::Resolved::new(); // alloc: Exec
    resolved.set_table_cap(grant.bytes);
    part_release.apply(&mut resolved);
    resolved.cells_from(registry); // D300: this execution's verdict cells for the sources
    install_undriven(mcx, registry, snap, &mut resolved)?;
    Ok(resolved)
}

/// D1322: every granule recipe's part list under the snapshot that `resolved` does not hold yet (a serial run's all of
/// them; RunnerPlan::prepare's the sites no pipeline drives, a nest.loop side's scan: bound without one, its source
/// raised "without a part list"). The count installed (0 when every site is already resolved: nothing opened).
pub(super) fn install_undriven(mcx: Mcx<'static>, registry: &HostRegistry, snap: &Rc<SnapshotData<'static>>, resolved: &mut granule_source::Resolved) -> PgResult<u32> {
    let mut n = 0;
    for site in 0..registry.len() {
        let Some(r) = registry.granule_recipe(site) else { continue };
        if resolved.list(r.rel).is_some() {
            continue;
        }
        let rel = table::table_open(mcx, r.rel, types_storage::lock::NoLock)?;
        let list = granule_source::resolve(&rel, Some(snap));
        rel.close(types_storage::lock::NoLock)?;
        resolved.install(r.rel, list?);
        n += 1;
    }
    Ok(n)
}

/// D264: the granule relations of a pull that no producers resolved (the entry's serial arm, the extended protocol's open
/// and each Append arm's reopen), under the pull's grant: `None` for a program without a granule site or a bitmap
/// recheck, so a heap or index statement pays nothing; (D3663) a pruned relation set's program resolves too, its
/// survivors filed by `file_pull_sets`; a bitmap entry's recheck qual is filed for its source's bind (`park_salt`).
/// The caller keeps the box while the adapter runs: a granule source reads it until its next bind.
pub fn resolve_pull(mcx: Mcx<'static>, registry: &HostRegistry, snap: &Rc<SnapshotData<'static>>) -> PgResult<Option<Box<dyn std::any::Any + Send + Sync>>> {
    if (0..registry.len()).all(|site| registry.granule_recipe(site).is_none() && registry.bitmap_recheck(site).is_none() && !registry.range_recipe(site).is_some_and(|r| r.set_pruned)) {
        return Ok(None);
    }
    Ok(Some(Box::new(resolve_serial(mcx, registry, snap, granted_memory(1), PartRelease::Off)?))) // alloc: Exec
}

/// D3663: files each relation set `initial` prunes, its surviving members alone (hosts `RelSet::resolve_kept`), or the
/// site as pruned when none survives: no other member is opened, sized, read or counted, as PostgreSQL never initializes
/// a subplan its initial pruning removed.
pub(crate) fn file_pruned_sets(mcx: Mcx<'_>, initial: &crate::ops::append::proto::Initial, registry: &HostRegistry, resolved: &mut granule_source::Resolved) -> PgResult<()> {
    for (s, valid) in initial.set_sites() {
        let Some(r) = registry.range_recipe(s).filter(|r| r.set.is_some()) else { continue };
        let set = heap_source::RelSet::resolve_kept(mcx, r, &|i| crate::ops::append::prune::member(valid, i))?;
        match set.is_empty() {
            true => resolved.prune_site(s),
            false => resolved.file_rel_set(s, Arc::new(set)), // alloc: Exec
        }
    }
    Ok(())
}

/// D3663: a pull's relation sets that `program`'s Appends prune at the executor start (a statement's or an arm's, under
/// plan `pstmt` and parameters `params`), their survivors filed in the pull's `Resolved` (`resolve_pull` makes one for
/// such a program) for the sources' bind.
pub fn file_pull_sets(mcx: Mcx<'_>, program: &Program, registry: &HostRegistry, pstmt: Option<&'static PlannedStmt<'static>>, params: ParamListHandle, pulled: Option<&mut (dyn std::any::Any + Send + Sync)>) -> PgResult<()> {
    if !program.sites.iter().any(|d| d.set_prune.is_some()) {
        return Ok(());
    }
    if let Some(r) = pulled.and_then(|p| p.downcast_mut::<granule_source::Resolved>()) {
        file_pruned_sets(mcx, &crate::ops::append::proto::Initial::decide_in(program, pstmt, params)?, registry, r)?;
    }
    Ok(())
}

/// D280: a serial program's v4 protocols, prepared as a parallel program's RunnerPlan prepares them (src.verdicts' cells,
/// Facts), with the words they leave for the binding: the key params, then what the protocols wrote. The protocols'
/// state lives with it, so the caller keeps it while the program runs. `None` for a program that declares none (a heap
/// or index statement pays nothing). A protocol that acts after a producer has none to act after here: the lowering
/// refuses it (Reason::Bug), and this is the wall behind that.
pub struct SerialWords {
    /// The bytes kept here for the execution (the words, their nulls and this block), on the
    /// execution's prepare owner (`Prepare::owner`: the portal first, so a cursor's `PortalPins.serial` keeps them on its
    /// portal until CLOSE): charged once at `serial_words` (a fixed holder), declared first so its
    /// door-1 drop moves the bytes to the freeing thread before the frees. `Charge::NONE` outside a statement (a rig).
    held: memledger::Charge,
    v4: crate::ops::State,
    words: Vec<u64>,
    nulls: Vec<u8>,
    n: usize,
}

impl SerialWords {
    pub fn words(&self) -> (&[u64], &[u8]) {
        (&self.words[..self.n], &self.nulls[..self.n])
    }

    /// The protocols' normal end (`v4::State::end`); an error's end only drops them.
    pub fn end(&self) -> PgResult<()> {
        self.v4.end()
    }
}

/// `serial_words` for a pull that no producers prepared (the entry's serial arm), under the pull's grant (`resolve_pull`'s).
pub fn pull_words(mcx: Mcx<'static>, a: &Admitted, snap: &Rc<SnapshotData<'static>>, params: &[u64], nulls: &[u8]) -> PgResult<Option<Box<SerialWords>>> {
    // an inert program's pull reads no grant (granted_memory reads the grant knobs' snapshot and two GUCs every Execute)
    if a.program.v4.as_ref().is_none_or(|d| serial_inert(&d.protos)) {
        return Ok(None);
    }
    serial_words(mcx, a, snap, granted_memory(1), params, nulls, None)
}

/// A serial pull's protocols that leave it nothing to bind: none, or only `width` / `sink.ordered`. Neither writes a word or
/// keeps live state, and the widths `width` fills size a parallel run's sections, never a serial pull, so a program declaring
/// only those (C01's index point read) binds its key params with no allocation per execution.
fn serial_inert(protos: &[gravity_program::protos::Proto]) -> bool {
    use gravity_program::protos::ProtoKind as K;
    protos.iter().all(|p| matches!(p.kind, K::Width | K::OrderedSink | K::SortMerge))
}

/// `initial`: the initial pruning `run_once` decided before its sources bind (`Initial`), which `src.prune` takes.
pub(super) fn serial_words(mcx: Mcx<'static>, a: &Admitted, snap: &Rc<SnapshotData<'static>>, grant: Grant, params: &[u64], nulls: &[u8], initial: Option<&crate::ops::append::proto::Initial>) -> PgResult<Option<Box<SerialWords>>> {
    use gravity_program::protos::ProtoKind as K;
    let program = a.program.clone();
    let Some(d) = program.v4.as_ref().filter(|d| !serial_inert(&d.protos)) else { return Ok(None) };
    if let Some(p) = d.protos.iter().find(|p| matches!(p.kind, K::TwoPass | K::RtFilter | K::BuildSide | K::Observe | K::TopGroups)) {
        return Err(glue::internal(&format!("gravity: a serial program declares {}, a protocol that acts after a producer (D280)", p.kind.name()))); // alloc: Cold
    }
    let nk = params.len();
    let nw = run_words(&program, nk);
    let owner = gravity_driver::stmt_context().map(|(o, _)| (o, memledger::Kind::Region));
    let (mut words, mut wn) = (vec![0u64; nw], vec![0u8; nw]); // alloc: Exec charged sw.held
    words[..nk].copy_from_slice(params);
    wn[..nk].copy_from_slice(&nulls[..nk]);
    let (mut widths, mut n) = (vec![0u16; program.pipelines.len()], nk); // alloc: Exec
    let v4 = crate::ops::State::prepare(&mut crate::ops::Prepare { mcx, program: &program, admitted: a, snap, width: 0, grant, widths: &mut widths, words: &mut words, nulls: &mut wn, nparams: &mut n, initial, owner })?;
    let mut sw = Box::new(SerialWords { held: memledger::Charge::NONE, v4, words, nulls: wn, n }); // alloc: Exec charged sw.held
    if let Some((o, k)) = owner {
        // the blocks are allocated (on this thread): their bytes move to the owner now
        let bytes = core::mem::size_of::<SerialWords>() + sw.words.capacity() * core::mem::size_of::<u64>() + sw.nulls.capacity();
        sw.held = memledger::Charge::open(o, memledger::charge::release_of(o), k, memledger::charge::Door::Heap, bytes as u64);
    }
    Ok(Some(sw))
}

#[allow(clippy::too_many_arguments)]
fn run_once(mcx: Mcx<'static>, entry: &mut gravity_driver::Entry, a: &mut Admitted, key: Option<PlanKey>, snap: &Rc<SnapshotData<'static>>, params: &[u64], nulls: &[u8], cid: u32, cmd: Cmd, remote: bool, dest: &mut DestReceiver<'_>, wire: Option<&mut gravity_wire::WireNode>, formats: Option<&[i16]>, outer: Option<&mut dyn gravity_driver::Sink>) -> PgResult<u64> {
    // the simple protocol's serial statements park their Node too (§4.5), under the same recipe condition
    let key = key.or_else(|| park_key(mcx, a));
    let _placed = gravity_parallel::place();
    // a breakerless program has nothing to reserve: outside the pool, as the extended pull (H16 (a); strict: a region draw is XX000)
    let lease = if crate::inputs::knobs::grant::breakerless(&a.program) { crate::inputs::knobs::grant::GrantLease::strict(1) } else { admit_grant(&a.program, &crate::admit::bound_all(a), 1, a.pstmt)? };
    let mut resolved = resolve_serial(mcx, &a.registry, snap, lease.grant(), part_release(&a.program, cmd, outer.is_some()))?;
    // D3586: the arms the initial pruning removes, before the sources bind (`Initial`): their relations are not opened
    let initial = crate::ops::append::proto::Initial::decide(&a.program, a)?;
    initial.pruned_sites(&a.program).into_iter().for_each(|s| resolved.prune_site(s));
    // D3663: the relation sets the initial pruning keeps members of, resolved over those members alone
    file_pruned_sets(mcx, &initial, &a.registry, &mut resolved)?;
    // D280: the protocols' words (src.verdicts engage on a serial pgrc program), bound as a parallel program's plan words
    let serial = serial_words(mcx, a, snap, lease.grant(), params, nulls, Some(&initial))?;
    let (params, nulls) = serial.as_deref().map_or((params, nulls), |s| s.words());
    let parts = glue::binding_parts(snap, cid, params, nulls, Some(&resolved));
    a.registry.rewind();
    let dctx;
    let mut sink = if !emits_rows(cmd, a.out_natts) {
        Receiver::Null
    } else if let Some(o) = outer {
        Receiver::Outer(o)
    } else if remote {
        let w = wire.expect("select without a wire node");
        w.prepare(&a.program, formats)?;
        Receiver::Wire(w.reset())
    } else {
        dctx = gravity_driver::mem::root::Root::new("GravityDestSink");
        let mcx: Mcx<'static> = super::raw::sink_mcx(&dctx);
        let desc = desc_of(mcx, &a);
        Receiver::Dest(DestSink::new(mcx, desc, dest))
    };
    let mut binding = Binding::new(parts, &mut sink);
    crate::phases::mark("sink");
    let mut exec = entry.start_at(key, &a.program, &mut a.registry, &mut binding, lease.reservation(), gravity_driver::Widths::Serial).map_err(glue::pg_error)?;
    crate::phases::mark("start");
    // Runner need 18: a recheck is one row of the statement — EvalPlanQual on that row, its outcome
    // counted, then the same pipeline resumed past it (the kernel's resume word: no second seek).
    let cap = a.program.layout.out_rows;
    let mut call = gravity_driver::Call::batch(cap);
    let mut rechecked = 0u64;
    let r: PgResult<()> = loop {
        match entry.run(&mut exec, &mut binding, call) {
            Ok(gravity_driver::Step::Done) => break Ok(()),
            Ok(gravity_driver::Step::Suspended { .. }) => call = gravity_driver::Call::resume(cap),
            Ok(gravity_driver::Step::Recheck { site }) => {
                let pos = exec.recheck_pos();
                let positions: Vec<gravity_abi::ScanPos> = exec.node().positions().to_vec(); // alloc: Cold
                match crate::rerun::epq::recheck(entry, a, key, &mut binding, &positions, site as usize, pos) {
                    Ok(more) => rechecked += more,
                    Err(e) => break Err(e),
                }
                if !resumes(&exec) {
                    break Ok(());
                }
                call = gravity_driver::Call::resume(cap);
            }
            // D824: a one-pipeline program has no lazy build (lowerv4 declares one only beside its probe's pipeline)
            Ok(gravity_driver::Step::BuildWait { pipeline }) => break Err(glue::pg_error(gravity_driver::Error::BuildWait(pipeline))),
            Err(e) => break Err(glue::pg_error(e)),
        }
    };
    crate::phases::mark("run");
    let r = r.and_then(|()| serial.as_deref().map_or(Ok(()), SerialWords::end));
    let ended = end_entry(entry, exec, r.is_ok());
    crate::phases::mark("end");
    if crate::phases::on() {
        let rel = resolved.part_release_counts();
        eprintln!("PHASES stmt width=0 us:{} released={} reopened={} private={} cache={}/{}MiB cache_held={}MiB plan_order={}", crate::phases::line(), rel.0, rel.1, resolved.part_release_private(), granule_source::part_cache_mib().0, granule_source::part_cache_mib().1, granule_source::part_cache_mib().2, u8::from(super::in_plan_order())); // alloc: Cold
    }
    r?;
    ended.map(|()| binding.rows_processed + rechecked)
}

/// D374: an entry ends with interrupts held off, as PostgreSQL's cleanup runs (AbortTransaction, resource owner release).
/// Its cleanup drops the spill sets, whose temp-directory walk (fd walkdir) calls CHECK_FOR_INTERRUPTS inside a Drop that
/// cannot return an error: a pending cancel or timeout was raised there and dropped, so the statement reported glue's
/// generic "user request" (a statement_timeout, a pg_terminate_backend's FATAL). Held, it stays pending for the next check,
/// which is the one this makes once the entry has ended (D376): a cancel, timeout or terminate that landed during the
/// cleanup ends the statement here, as PostgreSQL's walkdir check would raise it. The caller returns the run's own error
/// first; this one only when the run succeeded.
pub(super) fn end_entry(entry: &mut gravity_driver::Entry, exec: gravity_driver::Exec, ok: bool) -> PgResult<()> {
    {
        let _held = init_small::globals::hold_interrupts();
        entry.end(exec, ok);
    }
    check_after_teardown()
}

// ===== kernels track (pass 4): the table-lane fault arm =====
/// A gather selected a faulting table entry: render it and run the per-entry function on it alone (expr.md §2.2).
pub(super) fn reevaluate_fault(exec: &mut Exec, registry: &gravity_hosts::HostRegistry, site: usize, lane: usize, code: u32, part: u32) -> gravity_driver::Error {
    let Some(recipe) = registry.granule_recipe(site) else { return gravity_driver::Error::Fault { site: site as u8, lane: lane as u8, code, part } };
    let Some(spec) = lane.checked_sub(recipe.natts + 1).and_then(|i| recipe.tables.get(i)) else { return gravity_driver::Error::Fault { site: site as u8, lane: lane as u8, code, part } };
    let Some(src) = exec.node_mut().source_mut(site) else { return gravity_driver::Error::Fault { site: site as u8, lane: lane as u8, code, part } };
    let (datum, len) = match src.dict_value(part, spec.over as u16, code) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let bytes = super::raw::varlena_payload(datum, len);
    match gravity_hosts::dict::text_hosts::reevaluate(spec, bytes) {
        Err(e) => gravity_hosts::stash(e),
        Ok(()) => gravity_driver::Error::Fault { site: site as u8, lane: lane as u8, code, part },
    }
}

// ===== end =====

#[cfg(test)]
mod part_release_tests {
    use super::*;
    use gravity_abi::breaker::{Budget, Spill};
    use gravity_program::{Arena, Code, Layout, Pipeline, Reason, SiteDesc, WritePlan};

    fn program(sites: usize, pipes: Vec<Pipeline>) -> Program {
        let st = Arena::new(1 << 14);
        let sites: Vec<SiteDesc> = (0..sites).map(|i| SiteDesc::range_source(st.handle(), 7 + i as u32, &[1])).collect(); // alloc: Cold
        let layout = Layout { nsites: sites.len() as u16, out_natts: 0, out_rows: 1, scratch_bytes: 0, exec_bytes: 0 };
        Program::with_pipelines(layout, &sites, &[], WritePlan::SELECT, pipes.into_iter().map(|p| (p, Code::hand(crate::ops::ctescan::raw::test_noop))))
    }

    fn breaker(kind: BreakerKind) -> End {
        End::Breaker(Breaker { kind, slot: 0, bytes: 64, budget: Budget::Share, spill: Spill::None })
    }

    fn emit() -> Pipeline {
        Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite)
    }

    /// D3268 / D3823: a breaker over one granule site (COUNT(*) WHERE url LIKE .., SUM(octet_length(url)) GROUP BY
    /// k, an ORDER BY .. LIMIT's TopK, a Sort, a hash build, a DISTINCT, a Materialize) releases its parts' uses as
    /// its claims leave them; the streaming SELECT keeps D2985's release; a second site, a later pipeline that drives a
    /// site, a pipeline a rerun set runs again, an outer sink or a write releases nothing.
    #[test]
    fn a_breaker_over_one_granule_site_releases_its_parts_uses() {
        use gravity_program::lifetime::{Lifetimes, RunLife, RunMember, Runner, NO_PROBE};
        use gravity_program::protos::{Proto, ProtoKind, V4Descs};
        let sel = Cmd::Select;
        let agg = |k| program(1, vec![Pipeline::eligible(0, breaker(k), 1000), emit()]); // alloc: Cold
        for k in [BreakerKind::PlainAgg, BreakerKind::HashAgg, BreakerKind::Sort, BreakerKind::Distinct, BreakerKind::Materialize] {
            assert_eq!(part_release(&agg(k), sel, false), PartRelease::UsesDicts, "{k:?}: code words or copies, its dictionaries go with the part");
        }
        for k in [BreakerKind::TopK, BreakerKind::HashBuild] {
            assert_eq!(part_release(&agg(k), sel, false), PartRelease::Uses, "{k:?}: keeps dictionary images, which stay in the list's cells");
        }
        assert_eq!(part_release(&program(1, vec![Pipeline::eligible(0, End::Sink, 1000)]), sel, false), PartRelease::Dicts); // alloc: Cold
        assert_eq!(part_release(&agg(BreakerKind::HashAgg), sel, true), PartRelease::Off, "an outer sink");
        assert_eq!(part_release(&agg(BreakerKind::HashAgg), Cmd::Update, false), PartRelease::Off, "a write");
        let two_sites = program(2, vec![Pipeline::eligible(0, breaker(BreakerKind::HashAgg), 1000), emit()]); // alloc: Cold
        assert_eq!(part_release(&two_sites, sel, false), PartRelease::Rels(Box::new([(7, 1, true)])), "a second site no pipeline drives keeps its relation's parts"); // alloc: Cold
        let rescan = program(1, vec![Pipeline::eligible(0, breaker(BreakerKind::HashAgg), 1000), Pipeline::eligible(0, End::Sink, 1000)]); // alloc: Cold
        assert_eq!(part_release(&rescan, sel, false), PartRelease::Off, "a later pipeline drives the site again");
        let member = RunMember { pipeline: 0, runner: Runner::Set(0), life: RunLife::EveryRun, lazy_probe: NO_PROBE };
        let mut d = V4Descs::new(vec![Proto::new(ProtoKind::Rerun, 0)]); // alloc: Cold
        d.lifetimes = Lifetimes::new(vec![member]).expect("one member"); // alloc: Cold
        let rerun = agg(BreakerKind::Sort).with_v4(d);
        assert_eq!(part_release(&rerun, sel, false), PartRelease::Off, "a rerun set runs the scan's pipeline again");
    }

    /// D3823 (R3c): a portal releases as `part_release` decides, uses only: a one-site stream's `Dicts` becomes `Uses` (its
    /// pull buffer keeps dictionary images between fetches), a breaker's `Uses` and a join's `Rels` stay; under a soft
    /// rewind the sites the emit drives keep their parts (the rerun reads them again), the producers' still release.
    #[test]
    fn a_portal_releases_uses_only_and_a_soft_rewind_keeps_what_its_emit_reads() {
        let sel = Cmd::Select;
        let stream = program(1, vec![Pipeline::eligible(0, End::Sink, 1000)]); // alloc: Cold
        assert_eq!(part_release(&stream, sel, false), PartRelease::Dicts, "the push run's stream");
        assert_eq!(pull_release(&stream, sel, false), PartRelease::Uses, "a portal's stream keeps its dictionaries");
        assert_eq!(pull_release(&stream, sel, true), PartRelease::Off, "a soft rewind reruns the emit over the site");
        assert_eq!(pull_release(&stream, Cmd::Update, false), PartRelease::Off, "a write");
        let sort = program(1, vec![Pipeline::eligible(0, breaker(BreakerKind::Sort), 1000), emit()]); // alloc: Cold
        assert_eq!(pull_release(&sort, sel, false), PartRelease::UsesDicts, "a Sort's input");
        assert_eq!(pull_release(&sort, sel, true), PartRelease::UsesDicts, "a soft rewind reruns the emit over the Sort's slots, no site");
        let join = || program(2, vec![Pipeline::eligible(0, breaker(BreakerKind::HashBuild), 1000), Pipeline::eligible(1, End::Sink, 1000)]); // alloc: Cold
        assert_eq!(pull_release(&join(), sel, false), PartRelease::Rels(Box::new([(7, 1, false), (8, 1, false)])), "a join's build and probe"); // alloc: Cold
        assert_eq!(pull_release(&join(), sel, true), PartRelease::Rels(Box::new([(7, 1, false)])), "the probe the emit drives keeps its relation's parts"); // alloc: Cold
        let probe_only = program(2, vec![Pipeline::eligible(1, End::Sink, 1000)]); // alloc: Cold
        assert_eq!(pull_release(&probe_only, sel, true), PartRelease::Off, "nothing left to release");
    }

    /// D3823 (R3b): a program over several sites releases per relation: a hash join's build and probe (each site driven once)
    /// both release; a self-join's two sites over one relation release it with two readers; a site no pipeline drives (a
    /// nest's inner, a fetch site) or one a rerun set reads again keeps its relation's parts, the others still release; an
    /// outer sink or a write releases nothing.
    #[test]
    fn a_program_over_several_sites_releases_each_relation_read_once() {
        use gravity_program::lifetime::{Lifetimes, RunLife, RunMember, Runner, NO_PROBE};
        use gravity_program::protos::{Proto, ProtoKind, V4Descs};
        let sel = Cmd::Select;
        let join = || program(2, vec![Pipeline::eligible(0, breaker(BreakerKind::HashBuild), 1000), Pipeline::eligible(1, End::Sink, 1000)]); // alloc: Cold
        assert_eq!(part_release(&join(), sel, false), PartRelease::Rels(Box::new([(7, 1, false), (8, 1, false)])), "a hash join's build and probe: its code cells and the streamed probe keep their dictionaries"); // alloc: Cold
        assert_eq!(part_release(&join(), sel, true), PartRelease::Off, "an outer sink");
        assert_eq!(part_release(&join(), Cmd::Update, false), PartRelease::Off, "a write");
        let st = gravity_program::Arena::new(1 << 14);
        let sites: Vec<SiteDesc> = (0..2).map(|_| SiteDesc::range_source(st.handle(), 7, &[1])).collect(); // alloc: Cold
        let layout = Layout { nsites: 2, out_natts: 0, out_rows: 1, scratch_bytes: 0, exec_bytes: 0 };
        let pipes = [Pipeline::eligible(0, breaker(BreakerKind::HashBuild), 1000), Pipeline::eligible(1, End::Sink, 1000)];
        let selfjoin = Program::with_pipelines(layout, &sites, &[], WritePlan::SELECT, pipes.into_iter().map(|p| (p, Code::hand(crate::ops::ctescan::raw::test_noop))));
        assert_eq!(part_release(&selfjoin, sel, false), PartRelease::Rels(Box::new([(7, 2, false)])), "a self-join: one relation, two readers"); // alloc: Cold
        let inner = program(3, vec![Pipeline::eligible(0, breaker(BreakerKind::HashBuild), 1000), Pipeline::eligible(1, End::Sink, 1000)]); // alloc: Cold
        assert_eq!(part_release(&inner, sel, false), PartRelease::Rels(Box::new([(7, 1, false), (8, 1, false)])), "a site no pipeline drives (relation 9) keeps its parts"); // alloc: Cold
        let member = RunMember { pipeline: 1, runner: Runner::Set(0), life: RunLife::EveryRun, lazy_probe: NO_PROBE };
        let mut d = V4Descs::new(vec![Proto::new(ProtoKind::Rerun, 0)]); // alloc: Cold
        d.lifetimes = Lifetimes::new(vec![member]).expect("one member"); // alloc: Cold
        assert_eq!(part_release(&join().with_v4(d), sel, false), PartRelease::Rels(Box::new([(7, 1, false)])), "the probe a rerun set runs again keeps its relation's parts"); // alloc: Cold
        // D3823: a relation whose every site feeds a breaker that keeps code words gives its dictionaries back with its parts
        let aggs = program(2, vec![Pipeline::eligible(0, breaker(BreakerKind::HashAgg), 1000), Pipeline::eligible(1, breaker(BreakerKind::Sort), 1000), emit()]); // alloc: Cold
        assert_eq!(part_release(&aggs, sel, false), PartRelease::Rels(Box::new([(7, 1, true), (8, 1, true)])), "two breakers that keep code words"); // alloc: Cold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D4709 (the depot-fetch switch): a statement whose sources are granule sources and a by-rowid fetch site has no
    /// slot-depot salt with the switch off (every run builds its worker slots) and one with it on. The fetch recipe's
    /// columns and kinds are in that salt, and an all-granule statement's salt is the same either way.
    #[test]
    fn a_fetch_site_parks_its_statements_slots_only_under_the_switch() {
        use gravity_hosts::ops::scan::pgrc::fetch_site::{FetchRecipe, KIND_INT, KIND_TEXT};
        use gravity_hosts::ops::scan::pgrc::granule_source::GranuleRecipe;
        const REL: u32 = 16_401;
        let parts = std::sync::Arc::new(pgrc_read::PartRegistry::new(64 << 20));
        let reg = |fetch: Option<(&[i16], &[u8])>| {
            let mut r = gravity_hosts::ops::HostRegistry::new();
            r.push_granule(GranuleRecipe::for_test(REL, &[1, 5], parts.clone()));
            if let Some((attnos, kinds)) = fetch {
                r.push_fetch(FetchRecipe::for_test(REL, attnos, kinds, parts.clone()));
            }
            r
        };
        let granule_only = reg(None);
        let with_fetch = reg(Some((&[1, 2, 3], &[KIND_INT, KIND_TEXT, KIND_INT])));
        assert_eq!(depot_salt(&with_fetch, 5, false), None, "switch off: a fetch site keeps the statement out of the depot");
        let on = depot_salt(&with_fetch, 5, true).expect("switch on: the statement has a depot salt");
        assert_eq!(depot_salt(&with_fetch, 5, true), Some(on), "the same recipes, the same salt");
        assert_ne!(depot_salt(&granule_only, 5, false), None, "an all-granule statement parks");
        assert_eq!(depot_salt(&granule_only, 5, false), depot_salt(&granule_only, 5, true), "the switch leaves an all-granule salt alone");
        assert_ne!(depot_salt(&granule_only, 5, true), Some(on), "the fetch site is in the salt");
        for (what, other) in [
            ("another fetched column", reg(Some((&[1, 2, 4], &[KIND_INT, KIND_TEXT, KIND_INT])))),
            ("another kind", reg(Some((&[1, 2, 3], &[KIND_INT, KIND_INT, KIND_INT])))),
            ("one column fewer", reg(Some((&[1, 2], &[KIND_INT, KIND_TEXT])))),
        ] {
            assert_ne!(depot_salt(&other, 5, true), Some(on), "{what}: another salt");
        }
        assert_ne!(depot_salt(&with_fetch, 6, true), Some(on), "another database: another salt");
    }

    /// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
    /// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
    fn manifest_dir() -> &'static std::path::Path {
        static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
    }

    fn plan<'m>(mcx: Mcx<'m>, name: &str) -> &'m PlannedStmt<'m> {
        let path = manifest_dir().join("../decompose/tests/plans/regress-full").join(name);
        gravity_decompose::corpus::load(mcx, &std::fs::read_to_string(&path).unwrap()).unwrap_or_else(|e| panic!("{}: {e}", path.display())).stmt
    }

    /// PostgreSQL gives a DELETE no check of its own (the rewriter adds a view's CHECK OPTION to INSERT and UPDATE only; RLS
    /// DELETE policies are quals): `DELETE FROM v2` through two CHECK OPTION views parks, the same view's UPDATE and a MERGE
    /// DELETE under RLS carry checks and never park.
    #[test]
    fn a_write_with_a_check_option_never_parks() {
        let ctx = MemoryContext::new("park_key");
        let delete = plan(ctx.mcx(), "updatable_views-112.plan");
        let update = plan(ctx.mcx(), "updatable_views-111.plan");
        let merge = plan(ctx.mcx(), "rowsecurity-077.plan");
        assert!(!has_checks(Some(delete)) && has_checks(Some(update)) && has_checks(Some(merge)));
        assert_eq!(parks(Cmd::Delete, false, Some(delete)), Some(true));
        assert_eq!(parks(Cmd::Delete, false, Some(update)), None);
        assert_eq!(parks(Cmd::Delete, false, Some(merge)), None);
        assert_eq!(parks(Cmd::Delete, true, Some(delete)), None);
        assert_eq!(parks(Cmd::Delete, false, None), Some(true));
        assert_eq!(parks(Cmd::Select, false, None), Some(false));
        assert_eq!(parks(Cmd::Update, false, Some(update)), None);
        // PARK-W: an INSERT / UPDATE with check quals, or without a plan, never parks
        assert_eq!(parks(Cmd::Insert, false, Some(update)), None);
        assert_eq!(parks(Cmd::Update, false, None), None);
        assert_eq!(parks(Cmd::Insert, true, Some(delete)), None);
    }

    /// PARK-W's plan guards on real plans: a plain UPDATE (arrays-010), INSERT VALUES (rules-039), AGB-14, ON CONFLICT
    /// DO UPDATE (insert_conflict-005) and DO NOTHING (insert_conflict-006) and, oltpfix, RETURNING (arrays-023) and ON
    /// CONFLICT DO UPDATE .. RETURNING (arrays-045) park; an UPDATE through a partitioned root (alter_table-022:
    /// rootRelation) never does.
    #[test]
    fn only_a_plain_insert_or_update_parks() {
        let ctx = MemoryContext::new("park_key");
        let upd = plan(ctx.mcx(), "arrays-010.plan");
        let ins = plan(ctx.mcx(), "rules-039.plan");
        let upsert = plan(ctx.mcx(), "insert_conflict-005.plan");
        let nothing = plan(ctx.mcx(), "insert_conflict-006.plan");
        let ret = plan(ctx.mcx(), "arrays-023.plan");
        let conflict = plan(ctx.mcx(), "arrays-045.plan");
        let rooted = plan(ctx.mcx(), "alter_table-022.plan");
        assert!(plain_write(Some(upd)) && plain_write(Some(ins)) && plain_write(Some(upsert)) && plain_write(Some(nothing)));
        assert!(plain_write(Some(ret)) && plain_write(Some(conflict)));
        assert!(!plain_write(Some(rooted)) && !plain_write(None));
        assert_eq!(parks(Cmd::Update, false, Some(upd)), Some(true));
        assert_eq!(parks(Cmd::Insert, false, Some(ins)), Some(true));
        assert_eq!(parks(Cmd::Insert, false, Some(upsert)), Some(true));
        assert_eq!(parks(Cmd::Insert, false, Some(nothing)), Some(true));
        assert_eq!(parks(Cmd::Update, false, Some(ret)), Some(true));
        assert_eq!(parks(Cmd::Insert, false, Some(conflict)), Some(true));
        assert_eq!(parks(Cmd::Update, false, Some(rooted)), None);
        assert_eq!(parks(Cmd::Update, true, Some(upd)), None);
    }

    /// A Width-only or Width + sink.ordered program gets no SerialWords (5d9aff09793 paid ~1.5k instructions per Execute on
    /// C01 for one); a protocol that writes words (src.verdicts, facts) still prepares.
    #[test]
    fn a_width_or_ordered_only_program_binds_no_serial_words() {
        use gravity_program::protos::{Proto, ProtoKind as K};
        assert!(serial_inert(&[]));
        assert!(serial_inert(&[Proto::new(K::Width, 0)]));
        assert!(serial_inert(&[Proto::new(K::Width, 0), Proto::new(K::OrderedSink, 0), Proto::new(K::Width, 1)]));
        assert!(!serial_inert(&[Proto::new(K::Width, 0), Proto::new(K::Verdicts, 0)]));
        assert!(!serial_inert(&[Proto::new(K::Facts, 0)]));
        assert!(serial_inert(&[Proto::new(K::Width, 0), Proto::new(K::SortMerge, 0)]), "runs.sort acts in a section's merge alone");
    }
}
