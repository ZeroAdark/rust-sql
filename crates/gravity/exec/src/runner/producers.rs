//! The producer pipelines of a pulled program (`Producers`): prepared before the open, run once after it.
//! Split out of runner.rs (move-only, lane SPLIT 09-23).

use super::*;

/// The producers of a multi-pipeline program under the pull adapter (docs/gravity/entry.md §3, chore C1): every pipeline but the last
/// — its emit — run to completion at the width the runner grants (a section on the pool at width ≥ 2, else the leader's serial arm as
/// `run_program`'s), the adapter then pulls the emit as it pulls a one-pipeline program. Prepared before the adapter opens (the granule
/// relations it resolved reach the sources through the binding), run once after the open, dropped with the adapter's close (the grant's lease).
pub struct Producers {
    /// This block, kept for the execution (`Serve.producers` until the run's release, a cursor's
    /// until CLOSE), on the execution's prepare owner (`gravity_driver::stmt_context`: the portal first):
    /// a fixed holder charged once at `prepare`, declared first so its door-1 drop moves the bytes before the free.
    held: memledger::Charge,
    plan: RunnerPlan,
    lease: GrantLease,
    salt: Option<u64>,
    pub width: u16,
}

impl Producers {
    pub fn prepare(mcx: Mcx<'static>, a: &mut Admitted, snap: &Rc<SnapshotData<'static>>, params: &[u64], nulls: &[u8]) -> PgResult<Box<Producers>> {
        super::drive::install_held_hooks();
        let n = a.program.pipelines.len();
        // D3388 (D2772 POST): an unread data-modifying CTE's post pipelines run last, after the statement's own emit, the
        // last ending in the CTE's store; the adapter pulls them after that emit (driver pull.rs `fetch` takes the turn
        // before the first), so the emit is the last pipeline before them
        let emit = a.program.pipelines.iter().rposition(|p| !p.post).unwrap_or(n - 1);
        if a.program.pipelines[emit].ends_in != End::Sink {
            return Err(refused("protocol: a program whose last pipeline is not its emit on the pull adapter"));
        }
        // D622: a nested cursor's handed grant may keep it narrower than the fetch's width (`admit_grant_narrowing`)
        // D3865 L4: one emit and on-demand InitPlans run outside the pool (`inits_only`, no grant site reserved for it)
        let w = crate::inputs::knobs::width::statement_width(&a.program);
        let (lease, width) = crate::inputs::knobs::grant::GrantLease::or_admit(&a.program, a.bitmap_site(), w, || admit_grant_narrowing(&a.program, &crate::admit::bound_all(a), w, a.pstmt))?;
        // D3823 (R3c): the portal's parts go back as its claims leave them (uses only); a soft rewind's emit keeps its sites'
        let rewinds = a.program.v4.as_deref().is_some_and(|d| d.soft_rewind);
        let release = pull_release_of(a, rewinds);
        let mut plan = RunnerPlan::prepare(mcx, a, snap, width, lease.grant(), params, nulls, None, release, Bufs::new(mcx))?;
        // D3823 gate: a portal gives its done parts back at once, whatever the cache holds. Suspended between fetches it
        // checks the cache at no claim, so a part it kept would stay in use while other statements fill the cache past its
        // budget (D3821's stress kills: a cursor open over a big table beside large joins)
        plan.resolved.set_part_release_at(Some(0));
        plan.resolved.cells_from(&a.registry); // D300: this execution's verdict cells for the sources
        let mut p = Box::new(Producers { held: memledger::Charge::NONE, plan, lease, salt: recipe_salt(a), width }); // alloc: Exec charged p.held
        if let Some((o, release)) = gravity_driver::stmt_context() {
            // the block is allocated (on this thread): its bytes move to the owner now
            p.held = memledger::Charge::open(o, release, memledger::Kind::Region, memledger::charge::Door::Heap, core::mem::size_of::<Producers>() as u64);
        }
        Ok(p)
    }

    /// D622: the lease's statement runs or sits suspended (the entry's D601 slot park): `caller_granted` counts it only running.
    pub fn set_running(&self, on: bool) {
        self.lease.set_running(on);
    }

    /// D622: the grant kept across a rewind, handed to the rerun's `prepare` (a WITH HOLD cursor's persist reruns under its
    /// own grant, never queued behind what took its slot while it sat suspended).
    pub fn into_kept(self: Box<Self>, program: &Arc<Program>) -> NestedGrants {
        let Producers { plan, lease, width, .. } = *self;
        core::mem::drop(plan);
        crate::inputs::knobs::grant::kept(program, width, lease)
    }

    /// The plan's v4 protocols' normal end (`v4::State::end`); an error's end only drops them.
    pub fn end(&self) -> PgResult<()> {
        self.plan.v4.end()
    }

    /// The statement's admission, for the adapter's `open` (its Exec draws from it).
    pub fn reservation(&self) -> std::rc::Rc<gravity_driver::Reservation> {
        self.lease.reservation()
    }

    /// D3377 (D2701's soft rewind of an Append's arms): the emits from the execution's next pipeline on that a protocol
    /// pruned whole for this run (D2584), marked again after the rewind cleared their marks (driver `Entry::rewind_emit`):
    /// PostgreSQL's rescan of the Append with no param changed keeps its valid subplans.
    pub fn prune_emits(&self, exec: &mut Exec) {
        exec.run_empty(self.plan.v4.pruned_emits(exec.node().program(), exec.next_pipeline()));
    }

    /// D655: the plan a recheck reruns the producers by (epq.rs `recheck_with`).
    pub fn plan(&self) -> &dyn Plan {
        &self.plan
    }

    /// The statement's granule relations, for `glue::binding_parts`.
    pub fn resolved(&self) -> &(dyn std::any::Any + Send + Sync) {
        &*self.plan.resolved
    }

    /// The words the plan bound, for `glue::binding_parts` (D274): the key params, then the declared facts and the v4
    /// protocols' words past them, as `drive` binds its plan's.
    pub fn words(&self) -> (&[u64], &[u8]) {
        (&self.plan.params[..self.plan.nparams], &self.plan.nulls[..self.plan.nparams])
    }

    /// The producers of the next emit (multi-emit: an emit is every pipeline ending in the sink), from the
    /// execution's next pipeline up to that emit; the adapter pulls the emit, then calls this again before the next one.
    pub fn run(&self, entry: &mut gravity_driver::Entry, exec: &mut Exec, binding: &mut Binding<'_>, registry: &mut HostRegistry) -> PgResult<()> {
        exec.set_slot_salt(self.salt);
        let pool: &(dyn Pool + Sync) = installed_pool().unwrap_or(&NoPool);
        let _toast = heaptoast::worker::publish(self.plan.toast.clone());
        let program = exec.node().program().clone();
        // M8 (RT-1): the leader's frame while the producers run: an Init read in any of their calls runs its members
        // D1702: the slot the entry's calls publish their execution in, which the frame's nested runs re-enter through
        let current = gravity_driver::CurrentCell::new(None);
        entry.with_current_slot(&current, |entry| {
            // D1708 (a): the registry stays raw below this frame, as the frame keeps it: no producer arm holds a `&mut` of it
            // across a call a nested run re-enters from (`Leader`), whose section opens derive sources from it (`Node::provision`)
            let registry: *mut HostRegistry = registry;
            let mut leader = Leader { entry: entry as *mut _, exec: exec as *mut _, binding: binding as *mut _, registry, pool, plan: &self.plan, program: program.clone(), current: &current };
            let _frame = gravity_parallel::publish(&self.plan, &mut leader);
            while exec.state() != gravity_driver::ExecState::Done && !gravity_driver::is_emit(&program, exec.next_pipeline()) {
                let p = exec.next_pipeline();
                gravity_driver::entry::post_turn(exec, binding, p).map_err(glue::pg_error)?; // D2772 POST (driver entry/post.rs)
                debug_assert!(program.producers_of(p).all(|q| q < p), "pipeline {p} before its producers");
                // D824 (V4R-298): a lazy build runs only when its outer-first probe's first row reaches it (`build_wait`)
                if entry.skip_lazy(exec).map_err(glue::pg_error)? {
                    continue;
                }
                // M8 (RT-1): an Init's member runs only when its param is first read (`Leader::run_members`), or never
                if self.plan.deferred(p) {
                    // M8 (RT-2e): a rerun set's member: its slot kept, opened zeroed for the owner (`Entry::skip_rerun`)
                    match self.plan.rerun_owner(p) {
                        // setop_rec-0043: a CTE's plan the set re-arms, its store opened for the owner and the CTE's readers
                        Some(owner) => match self.plan.pull_readers(p) {
                            r if r.is_empty() => entry.skip_rerun(exec, owner).map_err(glue::pg_error)?,
                            r => entry.skip_rearmed(exec, owner, &r).map_err(glue::pg_error)?,
                        },
                        // M8 (RT-2f): a pulled CTE's plan: its store edge kept, opened zeroed for its readers
                        None => match self.plan.pull_readers(p) {
                            r if r.is_empty() => entry.skip_deferred(exec),
                            r => entry.skip_pulled(exec, &r).map_err(glue::pg_error)?,
                        },
                    }
                    continue;
                }
                // `registry` is this call's own argument, alive across the producer, reached on this thread only
                let r = run_producer(entry, exec, binding, registry, pool, &self.plan, &program, p, false);
                match r {
                    // as above; no other borrow of it is live here
                    Err(gravity_driver::Error::Fault { site, lane, code, part }) => Err(reevaluate_fault(exec, super::raw::registry(registry), site as usize, lane as usize, code, part)),
                    r => r,
                }
                .map_err(glue::pg_error)?;
                self.plan.after_pipeline(p, exec).map_err(glue::pg_error)?;
                if exec.next_pipeline() == p {
                    return Err(glue::internal("gravity: a producer ran without advancing past its pipeline"));
                }
            }
            // D2584: the adapter pulls the emits from here on in line: the ones pruned whole run empty, never opened
            exec.run_empty(self.plan.v4.pruned_emits(&program, exec.next_pipeline()));
            // D2772 POST: the turn before a post emit the adapter pulls next (driver entry/post.rs)
            gravity_driver::entry::post_turn(exec, binding, exec.next_pipeline()).map_err(glue::pg_error)?;
            Ok(())
        })
    }

    /// D824 (V4R-298): the adapter's emit stopped on `BuildWait` (`Error::BuildWait`) for lazy build `b`: it runs out of order
    /// through its width-1 producer arm, then the adapter's next fetch resumes the emit at its row.
    pub fn build_wait(&self, entry: &mut gravity_driver::Entry, exec: &mut Exec, binding: &mut Binding<'_>, registry: &mut HostRegistry, b: usize) -> PgResult<()> {
        let pool: &(dyn Pool + Sync) = installed_pool().unwrap_or(&NoPool);
        let _toast = heaptoast::worker::publish(self.plan.toast.clone());
        let program = exec.node().program().clone();
        // D1702: the slot the entry's calls publish their execution in, which the frame's nested runs re-enter through
        let current = gravity_driver::CurrentCell::new(None);
        entry.with_current_slot(&current, |entry| {
            // D1708 (a): raw below this frame (`run`)
            let registry: *mut HostRegistry = registry;
            let mut leader = Leader { entry: entry as *mut _, exec: exec as *mut _, binding: binding as *mut _, registry, pool, plan: &self.plan, program: program.clone(), current: &current };
            let _frame = gravity_parallel::publish(&self.plan, &mut leader);
            // `registry` is this call's own argument, alive across the build, reached on this thread only
            build_wait(entry, exec, binding, registry, pool, &self.plan, &program, b).map_err(glue::pg_error)
        })
    }

    /// M8 (RT-2f): the statement's end on the pull path (entry run.rs's normal close, before the v4 protocols' end): each
    /// data-modifying pulled set runs to its end through this runner's frame (PG's ExecPostprocessPlan).
    pub fn drain_pulled(&self, entry: &mut gravity_driver::Entry, exec: *mut Exec, binding: *mut Binding<'_>, registry: *mut HostRegistry) -> PgResult<()> {
        if self.plan.drain_at_end().is_empty() {
            return Ok(());
        }
        self.with_frame(entry, exec, binding, registry, |_| gravity_parallel::drain_pulled(&self.plan)).map_err(glue::pg_error)
    }

    /// M8 (RT-1): the producers' frame for the adapter's pulls of an emit (entry run.rs's fetch): published while `f` runs,
    /// so an Init read in the emit's calls runs the Init's members by this runner's producer arm. The pointers are the
    /// adapter's open execution and binding, alive across `f` (the caller's `with_open` hands them out). `f` reaches the
    /// entry only through the borrow handed to it.
    pub fn with_frame<R>(&self, entry: &mut gravity_driver::Entry, exec: *mut Exec, binding: *mut Binding<'_>, registry: *mut HostRegistry, f: impl FnOnce(&mut gravity_driver::Entry) -> R) -> R {
        let pool: &(dyn Pool + Sync) = installed_pool().unwrap_or(&NoPool);
        let program = super::raw::exec_program(exec);
        let current = gravity_driver::CurrentCell::new(None);
        entry.with_current_slot(&current, |entry| {
            let mut leader = Leader { entry: entry as *mut _, exec, binding, registry, pool, plan: &self.plan, program, current: &current };
            let _frame = gravity_parallel::publish(&self.plan, &mut leader);
            f(entry)
        })
    }
}

/// M8 (L0-k): the late words of an emit the pull adapter pulls (`Producers::emit_image`; `run_with_late` covers only the
/// runners' own arms), as an image over the binding's words the binding and the leader's context point at from the emit's
/// first call to the execution's end. The caller keeps it until the adapter closes (its binding and context go with it).
pub struct EmitImage {
    _params: Box<[u64]>,
    _nulls: Box<[u8]>,
}

impl Producers {
    /// M8 (L0-k): the next pipeline's (an emit about to start) late words, decided now inside the producers' frame (a late
    /// step may run an InitPlan's members, nested), laid over the binding's words as an image: `None` when it has none, or
    /// when the binding's words already hold them (an Append's later arm: its first arm's image is still in place).
    pub fn emit_image(&self, entry: &mut gravity_driver::Entry, exec: &mut Exec, binding: &mut Binding<'_>, registry: *mut HostRegistry) -> PgResult<Option<EmitImage>> {
        let q = exec.next_pipeline();
        let late = self.with_frame(entry, exec, binding, registry, |_| self.plan.late_words(q)).map_err(glue::pg_error)?;
        // D2584: an exec pruning the late step decided (L0-k: at the Append's first arm) prunes its emits whole from here on
        exec.run_empty(self.plan.v4.pruned_emits(exec.node().program(), q));
        let (params, nulls) = (binding.parts.params, binding.parts.param_nulls);
        if late.iter().all(|&(w, v, n)| params.get(w) == Some(&v) && nulls.get(w) == Some(&n)) {
            return Ok(None);
        }
        let (mut p, mut n): (Box<[u64]>, Box<[u8]>) = (params.into(), nulls.into()); // alloc: Exec
        for &(w, v, b) in late.iter() {
            if w < p.len() && w < n.len() {
                (p[w], n[w]) = (v, b);
            }
        }
        // the image's buffers live in the returned EmitImage, kept by the caller until the adapter has closed (raw.rs)
        super::raw::lay_image(binding, &p, &n);
        exec.node_mut().repoint_params(binding.parts.params, binding.parts.param_nulls);
        Ok(Some(EmitImage { _params: p, _nulls: n }))
    }
}

/// M8 (RT-1): the producers' frame (`Producers::run`, `build_wait`, `with_frame`): an Init read inside one of the leader's
/// calls runs the Init's members by `run_producer`'s width-1 arm and their `after_pipeline`, nested
/// (`Entry::run_nested`). Raw pointers, as gravity_parallel's run_body frame: the nested run happens inside a call the
/// runner is making, on its thread (every reader of an Init param is pinned to the leader, D1035), and `Entry::run_nested` puts back what the
/// members' runs move before the host returns.
struct Leader<'r, 'b> {
    entry: *mut gravity_driver::Entry,
    exec: *mut Exec,
    binding: *mut Binding<'b>,
    registry: *mut HostRegistry,
    pool: &'r dyn Pool,
    plan: &'r RunnerPlan,
    program: Arc<Program>,
    /// D1702: the entry's installed `Current` slot.
    current: *const gravity_driver::CurrentCell,
}

impl gravity_driver::OnDemand for Leader<'_, '_> {
    fn run_members(&mut self, members: &[u16]) -> gravity_driver::Result<()> {
        self.members(members, Nested::Run)
    }

    fn rerun_members(&mut self, members: &[u16]) -> gravity_driver::Result<()> {
        self.members(members, Nested::Rerun)
    }

    fn refill_members(&mut self, members: &[u16]) -> gravity_driver::Result<()> {
        self.members(members, Nested::Refill)
    }

    /// D1035 step 2 (V4R-444): an eager Init's reader starts: when it runs as a section (width >= 2), the members run ahead
    /// by `run_producer` at their widths (the Init's own pipeline, the last, width 1: its end folds into the cell), then true.
    /// An emit the adapter pulls runs on the leader alone: false, and its first read runs the members nested.
    fn prerun_members(&mut self, reader: usize, members: &[u16]) -> gravity_driver::Result<bool> {
        // the runner's own objects, alive while this frame is published (raw.rs `current`); pool / plan are the runner's own
        // borrows ('r), program its Arc. This thread only.
        let (entry, exec, binding) = self.at();
        // D1708 (a): the registry passed on raw, as `Producers::run` passes it: every producer arm between the runner and this
        // call holds it so (a `&mut` taken here from the frame's pointer and written by a member's section open would be a
        // write foreign to an outer arm's protected borrow)
        let (registry, pool, plan, program) = (self.registry, self.pool, self.plan, &self.program);
        if gravity_driver::is_emit(program, reader) || Section::plan(program, reader, plan.width_of(reader), false).is_none() {
            return Ok(false);
        }
        let last = members.last().map(|&m| m as usize);
        let mut run = |e: &mut gravity_driver::Entry, x: &mut Exec, bd: &mut Binding<'_>, p: usize| -> gravity_driver::Result<()> {
            // D2998: an EXISTS Init's own pipeline may run as a section too (its rows only store true and stop)
            let lazy = Some(p) == last && !(gravity_driver::exists_cell(program, p) && plan.width_of(p) >= 2);
            if !lazy && Some(p) == last {
                fires::fire("exec.exists.section");
            }
            // the runner's registry (as above), no `&mut` of it live across this producer
            run_producer(e, x, bd, registry, pool, plan, program, p, lazy)?;
            plan.after_pipeline(p, x)
        };
        entry.run_ahead(exec, binding, members, &mut run).map(|()| true)
    }

    fn pull_members(&mut self, members: &[u16], _budget: u64) -> gravity_driver::Result<gravity_driver::Pulled> {
        // the runner's own objects, alive while this frame is published (raw.rs `current`); pool / plan are the runner's own
        // borrows ('r), program its Arc. This thread only.
        let (entry, exec, binding) = self.at();
        // D1708 (a): passed on raw (`prerun_members`)
        let (registry, pool, plan, program) = (self.registry, self.pool, self.plan, &self.program);
        gravity_parallel::pulled_serial(plan, members)?;
        let mut run = |e: &mut gravity_driver::Entry, x: &mut Exec, bd: &mut Binding<'_>, p: usize| -> gravity_driver::Result<()> {
            // D3095: a producer the pull runs whole (never a lazy build, which runs at its probe's first row) runs as a section
            // over its claims taken from the start when they are big enough (D3137's rule for every nested producer); a
            // re-armed set's next pull runs it again
            let section = program.pipelines[p].lazy_for < 0 && gravity_parallel::nested_section(plan, program, p, true);
            // the runner's registry, no `&mut` of it live across this producer
            run_producer(e, x, bd, registry, pool, plan, program, p, !section)?;
            plan.after_pipeline(p, x)
        };
        gravity_parallel::pull_late(entry, exec, binding, plan, members, |e, x, b| e.pull_nested(x, b, members, &mut run))
    }

    /// setop_rec-0043: a pulled set re-armed by a rerun set's run (`members`' `Nested::Rearm`).
    fn rearm_pulled(&mut self, members: &[u16]) -> gravity_driver::Result<()> {
        self.members(members, Nested::Rearm)
    }

    /// D2605: the main plan ended, before the statement-end drain: the statement's own writers queue their AFTER STATEMENT
    /// events (driver node/ends.rs).
    fn end_main(&mut self) -> gravity_driver::Result<()> {
        let (_, exec, _) = self.at();
        exec.node_mut().end_main()
    }
}

impl<'b> Leader<'_, 'b> {
    /// D1702: the execution a nested run re-enters: the innermost runner call's (the entry's current, published for that
    /// call only), else the frame's own (raw.rs `current`, its contract).
    fn at<'a>(&self) -> (&'a mut gravity_driver::Entry, &'a mut Exec, &'a mut Binding<'b>) {
        super::raw::current(self.current, self.entry, self.exec, self.binding)
    }

    /// `members` run nested (`Entry::run_nested`), or rerun (`Entry::rerun_nested`, M8 RT-2e).
    fn members(&mut self, members: &[u16], how: Nested) -> gravity_driver::Result<()> {
        // the runner's own objects, alive while this frame is published (raw.rs `current`); pool / plan are the runner's own
        // borrows ('r), program its Arc. This thread only.
        let (entry, exec, binding) = self.at();
        // D1708 (a): passed on raw (`prerun_members`)
        let (registry, pool, plan, program) = (self.registry, self.pool, self.plan, &self.program);
        let mut run = |e: &mut gravity_driver::Entry, x: &mut Exec, bd: &mut Binding<'_>, p: usize| -> gravity_driver::Result<()> {
            // D3014 S2' / D3137: a nested member over a big scan runs as a section (a rerun's over its claims taken again)
            let section = !matches!(how, Nested::Rearm) && gravity_parallel::nested_section(plan, program, p, matches!(how, Nested::Rerun | Nested::Refill));
            // the runner's registry, no `&mut` of it live across this producer
            run_producer(e, x, bd, registry, pool, plan, program, p, !section)?;
            plan.after_pipeline(p, x)
        };
        match how {
            Nested::Run => entry.run_nested(exec, binding, members, &mut run),
            Nested::Rerun => {
                plan.reopen(members)?; // D2289: per-open state reset first
                entry.rerun_nested(exec, binding, members, &mut run)
            }
            Nested::Refill => {
                plan.reopen(members)?;
                entry.refill_nested(exec, binding, members, &mut run)
            }
            // setop_rec-0043: per-open state reset first (a recursive union's generation object), then the driver's pull state
            // and kept slots; nothing runs (the CTE's next pull runs the members)
            Nested::Rearm => {
                plan.reopen(members)?;
                entry.rearm_pulled(exec, members)
            }
        }
    }
}

/// What `Leader::members` does with a member set: runs it nested, runs it again (a rerun set; D4120: its kept members
/// filled again too), or re-arms it (a CTE's pulled set a rerun set's run re-arms, setop_rec-0043).
#[derive(Clone, Copy)]
enum Nested {
    Run,
    Rerun,
    Refill,
    Rearm,
}

/// D824 (V4R-298): `q` (the next pipeline) stopped on `BuildWait` for lazy build `b`: `b` runs out of order through its
/// width-1 arm (`run_producer`, never a section) and its `after_pipeline`, between `q`'s stop and its resume; a fed build's
/// producers run so before it (D1005, `Entry::build_wait`). `derive` as `run_producer`'s.
#[allow(clippy::too_many_arguments)]
fn build_wait(entry: &mut gravity_driver::Entry, exec: &mut Exec, binding: &mut Binding<'_>, derive: *mut HostRegistry, pool: &dyn Pool, plan: &dyn Plan, program: &Arc<Program>, b: usize) -> gravity_driver::Result<()> {
    stack_depth_core::check_stack_depth_or_panic();
    let mut run = |e: &mut gravity_driver::Entry, x: &mut Exec, bd: &mut Binding<'_>, p: usize| -> gravity_driver::Result<()> {
        // D2816 / D3524: a pulled set or a rerun set fed into the build runs at its reader's pulls or its owner's reruns
        if gravity_parallel::skip_in_wait(e, x, plan, p)? {
            return Ok(());
        }
        run_producer(e, x, bd, derive, pool, plan, program, p, true)?;
        plan.after_pipeline(p, x)
    };
    entry.build_wait(exec, binding, b, &mut run)
}

/// One producer pipeline of `Producers::run`: the section at width ≥ 2, else `run_program`'s width-1 arm (a claim-driven or a serial
/// source: the claims, the finish, the owner fold, the merge's finish, `end_claims`); a producer without a driving site runs its calls.
/// D1708 (a): `derive` is the runner's registry, raw: a late step (`Plan::late_words`) may run an Init's members ahead through
/// the frame (`Leader::prerun_members`), whose section opens derive sources from it (`HostRegistry::derive_source` writes
/// it); a `&mut` of it held here would be a protected borrow that write is foreign to. `derive` is the runner's own
/// registry, alive across the call and reached on this thread only; no `&mut` of it is live across it but the ones
/// derived from this pointer (raw.rs `registry`).
#[allow(clippy::too_many_arguments)]
fn run_producer(entry: &mut gravity_driver::Entry, exec: &mut Exec, binding: &mut Binding<'_>, derive: *mut HostRegistry, pool: &dyn Pool, plan: &dyn Plan, program: &Arc<Program>, p: usize, lazy: bool) -> gravity_driver::Result<()> {
    // D3918: a later arm of a run that ended at its first arm is stepped past unopened, as the parallel runner's arm. D4072:
    // a leader-only run too (`lazy`: an InitPlan's or SubPlan's member run nested at W0 or over a small scan); run, the
    // empty pair pass opened the slot its first arm sealed (a hash aggregate's phase-1 table: 'phase-1 table shape')
    if p == exec.next_pipeline() && entry.skip_ended(exec)? {
        return Ok(());
    }
    // D899: under its late key words' image, as the parallel runner's arm
    gravity_parallel::run_with_late(entry, exec, binding, plan, p, |e, x, b| run_producer_now(e, x, b, derive, pool, plan, program, p, lazy), || ())
}

/// `derive` as `run_producer`'s.
#[allow(clippy::too_many_arguments)]
fn run_producer_now(entry: &mut gravity_driver::Entry, exec: &mut Exec, binding: &mut Binding<'_>, derive: *mut HostRegistry, pool: &dyn Pool, plan: &dyn Plan, program: &Arc<Program>, p: usize, lazy: bool) -> gravity_driver::Result<()> {
    use gravity_driver::{Call, MorselRange, Step};
    let pipe = program.pipelines[p];
    // D276: a producer the build-first join's empty build masked runs as if no claim reached it (as run_program's arm);
    // so does an exec-pruned arm (D2105, `Plan::skips_whole`)
    let empty = exec.runs_empty(p) || plan.skips_whole(p);
    if let Some(section) = if empty || lazy { None } else { Section::plan(program, p, plan.width_of(p), false) } {
        // op 14: a failed pipeline whose merge defers its error (JOIN's outer-first build), or a feeder only it reads, is held and passed
        // the borrow ends with the section, whose claims make no nested run (an Init a section reads was run ahead of it,
        // D1035 step 2)
        return match run_section(entry, exec, binding, super::raw::registry(derive), &section, plan.claims_for(p), pool, plan.merge_for(p)) {
            Err(e) => entry.defer_failed(exec, binding, plan.merge_for(p), p, e).or_else(|e| entry.defer_fed(exec, binding, &|q| plan.merge_for(q), p, e)),
            ok => ok,
        };
    }
    let merge = plan.merge_for(p);
    if pipe.driving_site < 0 || pipe.produces().is_none() {
        let out_rows = program.layout.out_rows;
        let mut call = Call::batch(out_rows);
        let ran = loop {
            match entry.run_pipeline(exec, binding, call) {
                Ok(Step::Done) => break Ok(()),
                Ok(Step::Suspended { .. }) => call = Call::resume(out_rows),
                // the recheck's borrow ends with it
                Ok(Step::Recheck { site }) if crate::rerun::epq::recheck_leader(entry, exec, binding, super::raw::registry(derive), None, site, None)? => call = Call::resume(out_rows), // D610
                Ok(Step::Recheck { site }) => return Err(gravity_driver::Error::Recheck(site)),
                // D824: a site-less producer stopped for its lazy build: the build runs, the call resumes at its row
                Ok(Step::BuildWait { pipeline }) => match build_wait(entry, exec, binding, derive, pool, plan, program, pipeline) {
                    Ok(()) => call = Call::resume(out_rows),
                    Err(e) => break Err(e),
                },
                Err(e) => break Err(e),
            }
        };
        let Some(slot) = pipe.produces() else { return ran };
        // D763 (op 14): a site-less producer's error (a VALUES, set or table-function build, an aggregate's table) is held as
        // the claim arm below holds a relation build's: its merge defers it, or the deferring build it feeds takes it
        if let Err(e) = ran {
            return entry.defer_failed(exec, binding, merge, p, e).or_else(|e| entry.defer_fed(exec, binding, &|q| plan.merge_for(q), p, e));
        }
        // M7 L3b: a site-less producer (the drain pipe under a fold) owns and finishes its one partial as the width-1 arm below does
        return finish_single(entry, exec, binding, merge, p, slot);
    }
    let ran: gravity_driver::Result<()> = 'run: {
        if empty {
            // no claim: the finish, the owner fold and the seal below, as over a relation pruned whole
        } else if pipe.is_eligible() {
            let claims = plan.claims_for(p);
            claims.reset();
            let mut first = pipe.first_call;
            while let Some(claim) = gravity_parallel::next_claim(claims, &mut first) {
                if claims.beyond_bound(claim.lo) {
                    claims.close();
                    break;
                }
                match entry.run_claim(exec, binding, claim) {
                    Ok(Step::Suspended { .. }) => {
                        claims.close();
                        break;
                    }
                    // D824: a lazy build's probe runs leader-only over one claim (lowerv4 refuses an eligible one)
                    Ok(Step::BuildWait { .. }) => {
                        claims.close();
                        break 'run Err(gravity_driver::Error::BadReturn(gravity_abi::status::build_wait(0)));
                    }
                    Ok(_) => {}
                    Err(e) => {
                        claims.close();
                        break 'run Err(e);
                    }
                }
                if let Some(b) = merge.bound(entry.leader_partial(exec)) {
                    claims.observe_bound(b);
                }
            }
        } else {
            // D824: the claim stopped for its lazy build: the build runs, the same claim resumes at the row
            let mut step = entry.run_claim(exec, binding, MorselRange::ALL);
            while let Ok(Step::BuildWait { pipeline }) = step {
                if let Err(e) = build_wait(entry, exec, binding, derive, pool, plan, program, pipeline) {
                    break 'run Err(e);
                }
                step = entry.resume_claim(exec, binding, MorselRange::ALL);
            }
            if let Err(e) = step {
                break 'run Err(e);
            }
        }
        if pipe.finish {
            if let Err(e) = entry.run_claim(exec, binding, MorselRange::SECTION_END) {
                break 'run Err(e);
            }
        }
        Ok(())
    };
    // op 14: the run's error held and the pipeline passed when its merge defers it, else the statement's error
    if let Err(e) = ran {
        return entry.defer_failed(exec, binding, merge, p, e).or_else(|e| entry.defer_fed(exec, binding, &|q| plan.merge_for(q), p, e));
    }
    // op 14, fed (V4R-89): a feeder's merge raising in its finish (an aggregate's combine) is held as its run's error
    if let Some(slot) = pipe.produces() {
        if let Err(e) = finish_single(entry, exec, binding, merge, p, slot) {
            return entry.defer_fed(exec, binding, &|q| plan.merge_for(q), p, e);
        }
    }
    entry.end_claims(exec)
}

/// D655: one producer of a recheck (epq.rs), rerun whole on the recheck node, every site of which is substituted: the
/// width-1 arm of `run_producer` over ONE claim (a substituted source's first window is its one row, and the scan ends
/// there), the finish, the owner fold and the merge's finish, `end_claims`; a producer without a driving site runs its
/// calls. No section, no deferred error: a recheck's error is the statement's, but a `recheck_hold` build's, which op 14
/// holds for its probe's first row (D2033 step 2).
pub(crate) fn rerun_producer(entry: &mut gravity_driver::Entry, exec: &mut Exec, binding: &mut Binding<'_>, plan: &dyn Plan) -> gravity_driver::Result<()> {
    // writesubplan (lowerv4 marks.rs, a MERGE action's rerun set): a rerun set's member is stepped past as `Producers::run`
    // steps it (its slot kept, opened zeroed for the owner, which reruns it at each call over this node), never run whole
    if let Some(owner) = plan.rerun_owner(exec.next_pipeline()).filter(|_| plan.deferred(exec.next_pipeline())) {
        // setop_rec-0043: a CTE's plan the set re-arms, its store opened for the owner and the CTE's readers
        return match plan.pull_readers(exec.next_pipeline()) {
            r if r.is_empty() => entry.skip_rerun(exec, owner),
            r => entry.skip_rearmed(exec, owner, &r),
        };
    }
    // ACTION-CTE (lowerv4 marks.rs `act_ctes`): a pulled CTE's plan is stepped past as `Producers::run` steps it
    // (its store edge opened zeroed for its readers, `Entry::skip_pulled`), never run whole: the recheck's readers read and
    // pull on the statement's one store through the CTE's object, as EvalPlanQual's CteScan reads the parent's tuplestore
    let p = exec.next_pipeline();
    let cte = exec.node().program().v4.iter().flat_map(|d| d.protos.iter()).any(|x| x.kind == gravity_program::protos::ProtoKind::Cte && usize::from(x.pipeline) == p);
    if cte && plan.deferred(p) {
        let readers = plan.pull_readers(p);
        if !readers.is_empty() {
            return entry.skip_pulled(exec, &readers);
        }
    }
    // D3918: a later arm of a run that ended at its first arm is stepped past unopened, as `run_producer` steps it
    if entry.skip_ended(exec)? {
        return Ok(());
    }
    // D899: under its late key words, bound now (the substituted index rechecks its keys, as PostgreSQL's IndexRecheck)
    let late = plan.late_words(exec.next_pipeline())?;
    gravity_parallel::with_late(exec, binding, late, |x, b| rerun_producer_now(entry, x, b, plan))
}

fn rerun_producer_now(entry: &mut gravity_driver::Entry, exec: &mut Exec, binding: &mut Binding<'_>, plan: &dyn Plan) -> gravity_driver::Result<()> {
    use gravity_driver::{Call, MorselRange, Step};
    let p = exec.next_pipeline();
    let pipe = exec.node().program().pipelines[p];
    let merge = plan.merge_for(p);
    if pipe.driving_site < 0 {
        let out_rows = exec.node().program().layout.out_rows;
        let mut call = Call::batch(out_rows);
        loop {
            match entry.run_pipeline(exec, binding, call)? {
                Step::Done => break,
                Step::Suspended { .. } => call = Call::resume(out_rows),
                Step::Recheck { site } => return Err(gravity_driver::Error::Recheck(site)),
                Step::BuildWait { .. } => return Err(gravity_driver::Error::BadReturn(gravity_abi::status::build_wait(0))),
            }
        }
        return pipe.produces().map_or(Ok(()), |slot| finish_single(entry, exec, binding, merge, p, slot));
    }
    let ran = entry.run_claim(exec, binding, MorselRange::ALL).and_then(|_| match pipe.finish {
        true => entry.run_claim(exec, binding, MorselRange::SECTION_END).map(|_| ()),
        false => Ok(()),
    });
    // D2033 step 2: a build the recheck keeps in the statement run's order (`recheck_hold`, lowerv4 lockrows rerun.rs
    // `first_held`) holds its error for its probe's first row, as op 14 does in the statement's run
    if let Err(e) = ran {
        // D2033 step 3: but the epq probe's own (`recheck_hold_epq`) once PG's join builds first (`rerun_seen`)
        return match pipe.recheck_hold && !(pipe.recheck_hold_epq && entry.rerun_seen()) {
            true => entry.defer_failed(exec, binding, merge, p, e),
            false => Err(e),
        };
    }
    if let Some(slot) = pipe.produces() {
        finish_single(entry, exec, binding, merge, p, slot)?;
    }
    entry.end_claims(exec)
}

/// The one partial a width-1 producer leaves: owned when the merge partitions (`own_single`), then the merge's finish (a partitioned
/// breaker whose finish has no room spills the partial instead).
fn finish_single(entry: &mut gravity_driver::Entry, exec: &mut Exec, binding: &mut Binding<'_>, merge: &dyn Merge, p: usize, slot: u8) -> gravity_driver::Result<()> {
    let pipe = exec.node().program().pipelines[p];
    // D292: a slot no claim reached is opened before the merge touches it
    entry.open_unclaimed_of(exec, p)?;
    // D393b (V4R-57): an arm that continues its union's run folds only (end_claims scatters a spilling end's records)
    if exec.node().arm_continues(p) {
        return Ok(());
    }
    if let Some(pbits) = entry.owner_bits_of(exec, p, merge) {
        entry.own_single_of(exec, p, merge, pbits)?;
    }
    let node = exec.node_mut();
    match merge.finish(&mut node.leader_state(slot, binding.sink)) {
        Ok(()) => Ok(()),
        Err(gravity_driver::Error::Unsupported(_)) if pipe.breaker().is_some_and(|b| matches!(b.spill, gravity_abi::breaker::Spill::Partitioned { .. })) => entry.spill_leader_partial_of(exec, p).map(|_| ()),
        Err(e) => Err(e),
    }
}
