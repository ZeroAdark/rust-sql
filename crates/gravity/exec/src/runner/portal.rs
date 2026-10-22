//! The portal side of the runner: the per-portal slot (`ExecSlot`), Bind / Describe / Close, the portal's result descriptor
//! and the permission check (docs/gravity/exec.md §4.7). Split out of runner.rs (move-only, lane SPLIT 09-23).

use super::*;
use types_nodes::nodes_enums::CmdType;

pub struct ExecSlot {
    /// The slot's own block (its `Box` in `Backend::execs`, kept for the backend), its key words' buffers and its cached
    /// descriptor's `Rc` block on the per-backend holder (`crate::BACKEND_OWNER`), fitted as they change (`refit`). FIRST
    /// field: a door-1 charge drops before the bytes it counts are freed.
    held: memledger::Charge,
    pub(super) wire: gravity_wire::WireNode,
    gen: u32,
    pub(super) plan: usize,
    pub(super) key: PlanKey,
    /// The bound key words, sized by the statement (`key_words`); the buffers are kept from Bind to Bind.
    pub(super) kparams: Vec<u64>,
    pub(super) knulls: Vec<u8>,
    pub(super) nkparams: usize,
    pub(super) cmd: Cmd,
    pub(super) out_natts: usize,
    live: bool,
    /// C4 PortalPins: what the bound portal pins until its PortalDrop (its run, snapshot, owner, relations).
    pub(super) pins: PortalPins,
    pub(super) run_natts: usize,
    pub(super) served: u64,
    /// D307: the bound portal's run has ended (PostgreSQL's `atEnd`): a later Execute sends no row and completes with 0
    /// (a write without RETURNING: the portal cannot be run again). Cleared at Bind.
    pub(super) ended: bool,
    /// Rows the recheck nodes wrote for this execution (runner need 18).
    pub(super) rechecked: u64,
    /// Append arms the adapter has completed after the main (rung 12 need 3): the next to open is `arms[arm]`.
    pub(super) arm: usize,
    pub(super) cid: u32,
    /// H16 (a), the served Bind: the result descriptor of the plan this slot last bound, under that plan's key and program;
    /// the next Bind of the same plan hands the portal the same descriptor instead of building it again.
    desc: Option<(PlanKey, *const Program, Rc<types_tuple::TupleDescData<'static>>)>,
    /// D3547: a named portal's SELECT (its bit in `Backend::named_home` while it is at its level): sent away before another
    /// statement runs at that level (`send_away`), so a statement nested in that one may FETCH, MOVE or CLOSE it.
    pub(super) named: bool,
    /// The wire node is prepared for this run's program and formats (`step_slot`: at the run's first protocol Execute).
    pub(super) wired: bool,
}

impl ExecSlot {
    /// D3881: a portal executes from this slot now (an idle slot is kept for the next one).
    pub(crate) fn is_live(&self) -> bool {
        self.live
    }

    pub(super) fn new() -> ExecSlot {
        let o = crate::BACKEND_OWNER;
        ExecSlot {
            // the slot's block, which the caller boxes on this thread
            held: memledger::Charge::open(o, memledger::charge::release_of(o), memledger::Kind::Region, memledger::charge::Door::Heap, core::mem::size_of::<ExecSlot>() as u64),
            gen: 0,
            plan: 0,
            key: PlanKey { handle: gravity_program::Handle(0), generation: gravity_program::Generation(0) },
            // empty: each Bind sizes both to its statement's words (`size_words`)
            kparams: Vec::new(), // alloc: Cold
            knulls: Vec::new(), // alloc: Cold
            nkparams: 0,
            cmd: Cmd::Select,
            out_natts: 0,
            wire: gravity_wire::WireNode::new(),
            live: false,
            pins: PortalPins::new(),
            run_natts: 0,
            served: 0,
            ended: false,
            rechecked: 0,
            arm: 0,
            cid: 0,
            desc: None,
            named: false,
            wired: false,
        }
    }

    /// The holder fitted to the slot's bytes now: its block, its key words' capacity and a cached descriptor's block.
    fn refit(&mut self) {
        let now = (core::mem::size_of::<ExecSlot>() + self.kparams.capacity() * 8 + self.knulls.capacity()) as u64
            + self.desc.as_ref().map_or(0, |_| memledger::arc_bytes::<types_tuple::TupleDescData<'static>>());
        let was = self.held.committed();
        if now > was {
            self.held.commit(now - was);
        } else if now < was {
            self.held.free(was - now);
        }
    }

    /// The key words resized to `len` zeroed words, their capacity kept (a slot's binding reuses its buffers), the holder
    /// fitted to them.
    pub(super) fn size_words(&mut self, len: usize) {
        self.kparams.clear();
        self.kparams.resize(len, 0); // alloc: Exec charged self.held
        self.knulls.clear();
        self.knulls.resize(len, 0); // alloc: Exec charged self.held
        self.refit();
    }

    pub(super) fn active(&self) -> bool {
        self.pins.run.as_ref().is_some_and(PullAdapter::is_open)
    }

    fn token(&self, idx: usize) -> u64 {
        let t = (idx as u64 + 1) | ((self.gen as u64) << 8);
        debug_assert!(!super::point::is_point(t));
        t
    }

    /// D3579: a live portal's slot running plan slot `plan`, filed under `key` (`Backend::pinned`).
    pub(crate) fn runs(&self, plan: usize, key: PlanKey) -> bool {
        self.live && self.plan == plan && self.key == key
    }

    /// D3579: see `orphan`.
    pub(crate) fn orphan(&mut self) {
        self.pins.orphan = true;
    }

    /// PortalDrop of this slot's portal (`drop_by_token`, `drop_away`): its pins ended in PostgreSQL's order
    /// (`PortalPins::end`), its run's counters zeroed; the slot is no portal's after (kept for the level's next Bind).
    fn drop_portal(&mut self, entry: &mut gravity_driver::Entry, home: bool) {
        self.pins.end(entry, home, self.key);
        (self.served, self.rechecked, self.arm) = (0, 0, 0);
        self.live = false;
        self.named = false;
    }
}

/// C4 PortalPins: what one served portal pins from its Bind to its PortalDrop, released in one place in PostgreSQL's order:
/// a run's end (`close_run`: completed, failed, closed) and the portal's (`end`: PortalCleanup's ExecutorEnd, then the
/// relations its plan held). The plan it runs is its slot's (`ExecSlot::plan` / `key`: the memo keeps it for the portal,
/// `Backend::pinned`, outside the dependency check); the wire, the key words and the descriptor are the slot's, kept from
/// Bind to Bind.
pub(super) struct PortalPins {
    /// A bounded Execute's execution, held across messages until it completes (the pull adapter owns the Exec, the Binding and the over-produced rows).
    pub(super) run: Option<PullAdapter<'static>>,
    /// The snapshot registered at Bind on the portal's resource owner, unregistered from it at the run's end.
    pub(super) snap: Option<Rc<SnapshotData<'static>>>,
    /// D3525 / D3526: the portal's resource owner, which Bind's and Execute's pins go to (not the current subtransaction's).
    pub(super) owner: types_resowner::ResourceOwner,
    /// D264: the granule relations of this slot's pull (`resolve_pull`; no producers resolve them on the extended protocol),
    /// kept past the open and each Append arm's reopen while the sources read them; replaced at the next open.
    pub(super) pulled: Option<Box<dyn std::any::Any + Send + Sync>>,
    /// D288: the pull's v4 protocols and the words they bound (`pull_words`, as the entry's serial pull), kept while the run
    /// reads them; dropped at the run's release and at an Append arm's reopen.
    pub(super) serial: Option<Box<SerialWords>>,
    /// D761: the relations the portal's plan opens at ExecutorStart (`held::take` at Bind), held until PortalDrop
    /// (PORTAL_ONE_SELECT's ExecutorEnd is PortalCleanup's); a write's are dropped at its run's end (`held_to_drop`).
    pub(super) held: held::Held,
    /// D761: the portal is PORTAL_ONE_SELECT (a SELECT without a data-modifying WITH): its relations stay open to
    /// PortalDrop. Otherwise ProcessQuery / FillPortalStore ends the executor within the Execute that runs it.
    pub(super) held_to_drop: bool,
    /// D3579: the memo dropped the plan this portal runs (`Backend::orphan`), kept for it to PortalDrop: its runs park no
    /// Node under the plan's key (another admission of the key may take a Node parked there, and this one goes at the drop).
    pub(super) orphan: bool,
    /// C8: the portal as a memory owner (`memledger::charge::portal_begin`), from its Bind to its PortalDrop: the account
    /// it began on and its owner id.
    c8: Option<(memledger::Acct, u64)>,
}

impl PortalPins {
    fn new() -> PortalPins {
        PortalPins { run: None, snap: None, owner: types_resowner::ResourceOwner::NULL, pulled: None, serial: None, held: held::Held::default(), held_to_drop: true, orphan: false, c8: None }
    }

    /// C8: the portal becomes a memory owner (its Bind), live until `end_owner`.
    fn begin_owner(&mut self) {
        self.end_owner();
        let id = memledger::charge::new_owner_id();
        memledger::charge::portal_begin(id);
        self.c8 = Some((memledger::current(), id));
    }

    /// C8: the backend executes this portal until the guard drops (an Execute or a FETCH): the part uses its run takes
    /// are the portal's, not the statement's that sent the message.
    pub(super) fn enter_owner(&self) -> Option<memledger::charge::PortalScope> {
        self.c8.map(|(_, id)| memledger::charge::enter_portal(id))
    }

    /// C8 checker 1: the portal's owner end (its PortalDrop): what it still owns is its residue (`memledger::owner_end`).
    fn end_owner(&mut self) {
        if let Some((a, id)) = self.c8.take() {
            let _ = memledger::charge::portal_end(a, id);
        }
    }

    /// The run's end (completed, failed, closed or dropped): the adapter's Node unbound and parked under the plan's key at
    /// the slot's home level only (D3547 `home` false: built at another level, torn down, `close_unparked`); an orphan's
    /// Node unparked (D3579); the pull's relations and protocols dropped; the snapshot unregistered from the portal's owner
    /// (one release for a completed, failed or dropped run, and for a Bind whose Execute never came).
    pub(super) fn close_run(&mut self, entry: &mut gravity_driver::Entry, home: bool, key: PlanKey) {
        if let Some(run) = self.run.as_mut() {
            match home {
                true => run.close(entry, true),
                false => run.close_unparked(entry, true),
            }
        }
        self.unpark_orphan(entry, key);
        self.pulled = None;
        self.serial = None;
        if let Some(snap) = self.snap.take() {
            snapmgr::UnregisterSnapshotFromOwner(&snap, self.owner);
        }
    }

    /// D3579: a run opened before the memo dropped its plan (`ExecSlot::orphan`) parked its Node under the plan's key at its
    /// close: dropped there, as no admission but this portal's may take it.
    pub(super) fn unpark_orphan(&self, entry: &mut gravity_driver::Entry, key: PlanKey) {
        if self.orphan {
            entry.park().drop_for(key.handle);
        }
    }

    /// PortalDrop: PortalCleanup's ExecutorEnd in a live transaction (an abort's, TRANS_ABORT, only drops; the seam returns
    /// nothing, so an end's error is reported as a WARNING where PostgreSQL raises it from PortalDrop), the run's end
    /// (`close_run`), then the relations the plan held from Bind (D761).
    fn end(&mut self, entry: &mut gravity_driver::Entry, home: bool, key: PlanKey) {
        if let Err(mut e) = self.serial.as_deref().filter(|_| xact::IsTransactionState()).map_or(Ok(()), SerialWords::end) {
            e.level = types_error::WARNING;
            elog::emit_error_report_for(&e);
        }
        self.close_run(entry, home, key);
        self.held = held::Held::default();
        self.end_owner();
    }
}

pub(super) fn slot_of(b: &mut Backend, token: u64) -> Option<usize> {
    let idx = (token & 0xff) as usize;
    let i = idx.checked_sub(1)?;
    match b.execs.get(i).and_then(Option::as_ref) {
        Some(s) if s.live && s.token(i) == token => Some(i),
        _ => None,
    }
}

/// The first serve of the process hands the residency prime to the pool (a background step,
/// `gravity_hosts::prime`); without a pool nothing primes and first touch stays the way in.
pub(super) fn prime_once() {
    if let Some(pool) = installed_pool() {
        gravity_hosts::ops::scan::pgrc::prime::kick_off(&|job| pool.spawn(job));
    }
    // D1134: the relcache callback from this backend's first statement on (a DROP from a session that never ran a graph
    // admission is recorded too); a refused registration surfaces at the graph admission's `ensure_registered`
    let _ = gravity_hosts::ops::scan::pgrc::residency::ensure_release_registered();
    // D1507: every statement the seam sees starts here (execute, portal bind, unkeyed admission), outside any invalidation
    // processing, depot lock or park borrow: the relations the relcache callback recorded are released from the parked
    // worker slots now (one relaxed load when none), so a DROP's unlinked part files close at the next statement
    gravity_driver::slots::drain_released();
    // D3003: the pgrc part cache's budget, its charge to the grant pool, and a trim when over (statement boundary)
    gravity_hosts::ops::scan::pgrc::residency::arm_part_cache();
    // D3147: the idle release reaches this backend's parked Nodes (installed once; one relaxed load after)
    static SHED_ARMED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
    if !SHED_ARMED.load(core::sync::atomic::Ordering::Relaxed) && !SHED_ARMED.swap(true, core::sync::atomic::Ordering::AcqRel) {
        gravity_driver::mem::grant::set_idle_shed(idle_shed);
        gravity_driver::mem::grant::set_mem_wait(mem_wait_start, mem_wait_end);
    }
    MEM_WAIT_SERVES.with(|s| s.set(true));
}

thread_local! {
    /// W1: this thread serves statements (a backend; never a Gravity worker, which has no wait-event slot).
    static MEM_WAIT_SERVES: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

leader_tls::leader_tls! {
    thread_local! {
        /// W1: this backend's GravityMemory wait event, registered at its first memory wait (0 = not yet): per backend, not
        /// per process, since a crash cycle resets the custom wait events and a process-wide id could then name another.
        static MEM_WAIT_EVENT: core::cell::Cell<u32> = const { core::cell::Cell::new(0) };
    }
}

/// W1: a Gravity memory wait (the admission queue, the bytes wait) shows in `pg_stat_activity` as wait_event_type
/// Extension, wait_event GravityMemory, as PostgreSQL's extensions report theirs.
fn mem_wait_start() {
    if !MEM_WAIT_SERVES.with(core::cell::Cell::get) {
        return;
    }
    let mut id = MEM_WAIT_EVENT.with(core::cell::Cell::get);
    if id == 0 {
        id = waitevent::custom::WaitEventExtensionNew("GravityMemory").unwrap_or(waitevent::PG_WAIT_EXTENSION);
        MEM_WAIT_EVENT.with(|e| e.set(id));
    }
    waitevent::pgstat_report_wait_start(id);
}

fn mem_wait_end() {
    if MEM_WAIT_SERVES.with(core::cell::Cell::get) {
        waitevent::pgstat_report_wait_end();
    }
}

/// D3147: an idle backend's parked Nodes give their pool regions to the depot the idle release then trims (driver
/// `grant::idle_release`): a statement alone on the server later finds the pool those Nodes held free, not parked in a
/// session that has gone quiet (H0: 5.6 GB of 14.7 GB held idle).
fn idle_shed() {
    let _ = crate::with_existing_backend(|b| b.entry.park().shed_all());
    crate::admit::reentry::for_each_idle(|b| b.plan_scratch.drain());
    // D4550: S5's plan memo (a per-backend cache) trims at idle too: dropped whole
    super::point::idle_release();
}

/// D1922 (the portal-kind fact): a program of the statement's is served only to a reader that takes every row
/// (`Pipeline::whole_read`, lowerv4 `Px::whole_read`); the entry refuses it under a reader that may stop early. D3828: so is
/// a program the counted-pull marks hold (admission.rs `counted_refused`: D2230, D2732, D2805), admitted for a reader the
/// start cannot see count (the protocol's unnamed portal): an Execute's max_rows on it starts it again as a partial reader
/// or refuses it (admit/entry `counted_partial`, `counted_whole_read`).
pub fn whole_read(a: &Admitted) -> bool {
    let whole = |p: &Program| p.pipelines.iter().any(|x| x.whole_read);
    whole(&a.program) || a.graph.as_ref().is_some_and(|g| g.arms.iter().any(|x| whole(&x.program)) || g.inits.iter().any(|x| whole(&x.program))) || super::admission::counted_refused(a)
}

/// D2044: a program of the statement's declares a Volatile proto tagged `VOLATILE_ONCE_STORE` (lowerv4 srf/function_store.rs).
pub fn once_store(a: &Admitted) -> bool {
    use gravity_program::protos::{ProtoKind, VOLATILE_ONCE_STORE};
    let tagged = |p: &Program| p.v4.as_ref().is_some_and(|d| d.protos.iter().any(|x| x.kind == ProtoKind::Volatile && x.arg == VOLATILE_ONCE_STORE));
    tagged(&a.program) || a.graph.as_ref().is_some_and(|g| g.arms.iter().any(|x| tagged(&x.program)) || g.inits.iter().any(|x| tagged(&x.program)))
}

/// The served Bind, under the portal's resource owner as PortalStart runs (D3525 / D3526: the snapshot, the relations
/// `held::take` opens and whatever the admission pins belong to the portal, so a savepoint's end does not release them).
pub fn bind(portal: &Portal<'static>, cplan: CachedPlanHandle, params: ParamListHandle) -> PgResult<bool> {
    pquery::with_portal_owner(portal, || bind_owned(portal, cplan, params))
}

fn bind_owned(portal: &Portal<'static>, cplan: CachedPlanHandle, params: ParamListHandle) -> PgResult<bool> {
    // D3198: an executor-hook module acts on this backend's statements: the portal runs its own strategy, whose executor
    // seams the entry serves with the hooks fired (`execute::executor_hooks_active`)
    if super::execute::executor_hooks_active() {
        return Ok(false);
    }
    prime_once();
    if let Some(r) = super::point::portal::bind_point(portal, cplan, params) {
        return r;
    }
    crate::phases::reset();
    let Some(key) = glue::plan_key(cplan, 0) else {
        note_unkeyed();
        return Ok(false);
    };
    let r = crate::with_backend(|b| -> PgResult<bool> {
        // The runner binds one plannable query; a utility or a multi-statement list (PORTAL_MULTI_QUERY) is the
        // portal's own strategy, and is never memoized as refused (the portal's ProcessQuery offers each query of a
        // rule-rewritten list at the entry seam).
        let stmts = plancache::CachedPlanStmtList(cplan);
        if stmts.len() != 1 || !stmts.first().is_some_and(admit::offered) {
            b.note_fallthrough(admit::NOT_OFFERED);
            return Ok(false);
        }
        let named = !portal.borrow().name.is_empty();
        if stmts.first().is_some_and(|p| super::execute::entry_routed(p, named) || super::execute::rewriter_added(p)) {
            return Ok(false);
        }
        // D3580: WHERE CURRENT OF a named portal with row marks over a plan that prunes at start reads PG's
        // es_unpruned_relids, which the portal's own strategy keeps (the entry's `unpruned`): routing, never a refusal
        if named && stmts.first().is_some_and(|p| !p.rowMarks.is_nil() && !p.partPruneInfos.is_nil()) {
            return Ok(false);
        }
        if b.lookup(key) == Some(Verdict::Refused) {
            return Err(fall("memo: refused plan"));
        }
        let Some(slot_idx) = b.execs.iter().position(|s| s.as_ref().is_none_or(|s| !s.live)) else {
            b.note_fallthrough("no free exec slot");
            return Ok(false);
        };
        // a refusal or a full memo (D1789) notes its own fallthrough line
        let Some(plan_idx) = lookup_or_admit(b, key, cplan, params)? else { return Ok(false) };
        let stmts = plancache::CachedPlanStmtList(cplan);
        let Some(pstmt) = stmts.first() else { return Ok(false) };
        // a locking SELECT runs under the Bind-time snapshot (portal_snapshot.rs, as PortalStart's); a write here is an
        // unnamed portal's without RETURNING rows (`entry_routed` sent the rest to the portal's own strategy)
        let cmd = b.plans[plan_idx].as_ref().expect("admitted").served.cmd;
        start_checks(pstmt, cmd, b.admit_mcx())?;
        // a parallel program is the entry seam's on the extended protocol: the portal runs it through the executor, whose
        // seam pulls it after its producers (entry run.rs, `Producers`). Routing, never a refusal (no `fall`). So is a
        // store filled once (`once_store`): the entry serves its portal, whose later FETCH under SPI ends in the held error
        // on a raise after rows (D2044: entry run.rs `PL_LEFT`; a SQL FETCH of a portal bound here raises: D2085). So is a
        // program served only to a reader that takes every row (`whole_read`, D2678): an Execute's max_rows may stop the
        // portal early, and the entry refuses a named portal, or a run started under a count (PortalKind, WHOLE_READ). So is
        // an admission holding a timestamptz constant converted in the session TimeZone (B2-31: the entry starts a run under
        // max_rows, or in another zone, again with nothing converted: `zone_restart`)
        if b.plans[plan_idx].as_ref().is_some_and(|s| s.served.is_parallel() || once_store(&s.served) || whole_read(&s.served) || s.served.timezone.is_some()) {
            return Ok(false);
        }
        grant_start();
        let amcx = b.admit_mcx();
        let Backend { execs, plans, next_gen, named_home, .. } = b;
        let a: &mut Admitted = plans[plan_idx].as_mut().expect("admitted").served.as_mut();
        // the free slot's key words, sized by the statement and bound in place (its buffers reused from its last Bind)
        let slot = execs[slot_idx].get_or_insert_with(|| Box::new(ExecSlot::new())); // alloc: Cold charged ExecSlot::held
        slot.size_words(key_words(a));
        // the portal's snapshot is taken before the statement's expressions run (PortalStart), and a bind-time
        // word may run SQL (a key over a plpgsql function: SPI needs an outer snapshot), as the simple protocol's;
        // a write's own snapshot is taken when it runs (`at_execute`), and its words with it (D3516: `eval_at_execute`)
        let owner = portal.borrow().resowner;
        // D761: the relations PortalStart's ExecutorStart opens, before any expression of the statement runs
        let rels = a.graph.as_ref().map_or(Ok(held::Held::default()), |g| held::take(amcx, pstmt, &g.held, params, None))?;
        injection_point::injection_point("gravity-bind-before-snapshot")?;
        let held = crate::portal_snapshot::at_bind(a, owner)?;
        // A write takes its snapshot at Execute, after this Bind: its entry admits under it. Either is the entry's
        // (routing, never a refusal)
        if !a.roads.is_empty() {
            let hold = match held.as_ref() {
                Some(h) => {
                    snapmgr::PushActiveSnapshot(&h.0)?;
                    let hold = a.roads.hold();
                    snapmgr::PopActiveSnapshot()?;
                    hold
                }
                None => false,
            };
            if !hold {
                if let Some(h) = held {
                    snapmgr::UnregisterSnapshotFromOwner(&h.0, owner);
                }
                return Ok(false);
            }
        }
        if !a.eval_at_execute {
            let exprs = !a.key_exprs.exprs.is_empty();
            if exprs {
                match held.as_ref() {
                    Some(h) => snapmgr::PushActiveSnapshot(&h.0)?,
                    None => snapmgr::PushActiveSnapshot(&snapmgr::GetTransactionSnapshot()?)?,
                }
            }
            let keyed = eval_keys(a, params, &mut slot.kparams, &mut slot.knulls, amcx, true);
            if exprs {
                snapmgr::PopActiveSnapshot()?;
            }
            if !keyed? {
                if let Some(h) = held {
                    snapmgr::UnregisterSnapshotFromOwner(&h.0, owner);
                }
                return Err(fall("bind: key params"));
            }
        }
        *next_gen = next_gen.wrapping_add(1).max(1);
        slot.pins.owner = owner;
        slot.gen = *next_gen;
        slot.plan = plan_idx;
        slot.key = key;
        slot.pins.orphan = false;
        slot.nkparams = a.nkparams;
        slot.cmd = a.cmd;
        slot.out_natts = a.out_natts;
        slot.live = true;
        slot.pins.begin_owner();
        slot.ended = false;
        slot.cid = held.as_ref().map_or(0, |h| h.1);
        slot.pins.snap = held.map(|h| h.0);
        slot.pins.held = rels;
        slot.pins.held_to_drop = pstmt.commandType == CmdType::CMD_SELECT && !pstmt.hasModifyingCTE;
        // D3547: a named portal's SELECT outlives this message: a later statement's nested level may reach it (`send_away`)
        slot.named = slot.pins.held_to_drop && named;
        match slot.named {
            true => *named_home |= 1 << slot_idx,
            false => *named_home &= !(1 << slot_idx),
        }
        let token = slot.token(slot_idx);
        set_portal_desc(portal, token, a, amcx, key, &mut slot.desc);
        slot.refit();
        if slot.pins.held_to_drop {
            // D2085: a SELECT's portal is ready to run, as PortalStart leaves it (PORTAL_ONE_SELECT at its start), so a SQL
            // FETCH / MOVE of it runs (pquery's PortalRunSelect asks `fetch` for the rows)
            let mut p = portal.borrow_mut();
            (p.strategy, p.status, p.atStart, p.atEnd, p.portalPos) = (types_portal::PORTAL_ONE_SELECT, types_portal::PORTAL_READY, true, false, 0);
        }
        crate::phases::mark("bind");
        Ok(true)
    });
    match r {
        Some(Ok(v)) => Ok(v),
        Some(Err(e)) => Err(e),
        None => Ok(false),
    }
}

pub fn via(a: &Admitted) -> &'static str {
    if a.graph.is_some() {
        "graph"
    } else {
        "shape"
    }
}

pub fn describe(portal: &Portal<'static>) -> PgResult<bool> {
    let token = portal.borrow().gravity;
    if token == 0 {
        return Ok(false);
    }
    if super::point::is_point(token) {
        return Ok(false);
    }
    let formats: Vec<i16> = {
        let p = portal.borrow();
        p.formats.iter().copied().collect() // alloc: Cold
    };
    if let Some(r) = describe_away(token, &formats) {
        return r;
    }
    let _stepping = Stepping::set(token);
    let r = crate::with_backend(|b| -> PgResult<bool> {
        let Some(idx) = slot_of(b, token) else { return Ok(false) };
        let Backend { execs, plans, .. } = b;
        let slot = execs[idx].as_mut().expect("slot");
        let a = plans[slot.plan].as_ref().expect("admitted").served.as_ref();
        if !emits_rows(a.cmd, a.out_natts) {
            return Ok(false); // a write portal without RETURNING has no tupDesc; tcop sends NoData
        }
        let fmts = if formats.is_empty() { None } else { Some(&formats[..]) };
        slot.wire.prepare(&a.program, fmts)?;
        slot.wire.send_rowdesc()?;
        Ok(true)
    });
    r.unwrap_or(Ok(false))
}

pub fn drop_by_token(token: u64) {
    if token == 0 {
        return;
    }
    if super::point::is_point(token) {
        return super::point::portal::drop_portal(token);
    }
    if let Some((w, k)) = take_away(token) {
        return drop_away(w, k);
    }
    let _stepping = Stepping::set(token);
    let _ = crate::with_existing_backend(|b| {
        if let Some(idx) = slot_of(b, token) {
            b.named_home &= !(1 << idx);
            let Backend { entry, execs, .. } = &mut *b;
            let mut ran = None;
            if let Some(slot) = execs[idx].as_mut() {
                slot.drop_portal(entry, true);
                ran = Some((slot.plan, slot.key));
            }
            // D3579: the plan this portal ran goes when the memo dropped it while the portal lived
            if let Some((plan, key)) = ran {
                b.reap(plan, key);
            }
        }
    });
}

pub fn drop(portal: &Portal<'static>) {
    let token = portal.borrow().gravity;
    drop_by_token(token);
    portal.borrow_mut().gravity = 0;
}

// D3547 (b): a named protocol portal stays the runner's (its Bind / Execute loop, a JDBC fetch size's, keeps the runner's
// cost), yet PostgreSQL's portal is reachable from any statement: a PL/pgSQL FETCH / MOVE through a refcursor naming it,
// CLOSE, SQL FETCH inside a function. Such a statement runs nested in another (SELECT f('p')), whose level holds the
// Backend that the portal's exec slot and admission live in until it ends. So before a statement borrows a level
// (`reentry::run_at`), that level's live named portals go AWAY: the slot, with the admission it runs lent from the
// level's memo (the memo keeps the slot's lent state, D1789), into a session table any level reaches. An Execute or
// FETCH of an away portal runs at the first level free (its home when no statement holds it), as the executor entry's
// serve runs away from a busy level (D2355); away from its home its Node parks nowhere (D2190: a Node built over the
// lent admission). Its PortalDrop gives the admission back at its home level (`Backend::give_back_away`), or retires
// it there (D3346) when a function's CLOSE runs while the home level is held.
const _: () = assert!(crate::MAX_EXECS <= 32, "named_home has one bit an exec slot");
const _: () = assert!(crate::MAX_EXECS < 0xff);

/// D3547: the named portals of one plan slot sent away from their level together (`send_away`), with the admission they
/// run, lent once: two portals bound over one statement share their level's admission.
pub(super) struct Away {
    /// `slots`' block on the per-backend holder (`crate::BACKEND_OWNER`: the table is the thread's, `AWAY`), fixed at
    /// `send_away` (the group only shrinks after). FIRST field: a door-1 charge drops before the bytes it counts are freed.
    held: memledger::Charge,
    /// Each portal's token and slot.
    pub(super) slots: Vec<(u64, Box<ExecSlot>)>,
    /// The level they were bound at, whose memo lent the admission and whose park their Nodes may use.
    pub(super) home: usize,
    pub(super) key: PlanKey,
    pub(super) a: Box<Admitted>,
}

leader_tls::leader_tls! {
    thread_local! {
        /// D3547: the slots sent away. ManuallyDrop as the levels are (reentry.rs): never dropped by the thread's TLS destructor,
        /// after the contexts their runs reach; PortalDrop takes each out, the session's end any left (`drain_away`).
        /// The table's block is on the per-backend holder from its first growth (`away_push`): a pooled thread keeps it
        /// across sessions, so no session owns it.
        static AWAY: std::mem::ManuallyDrop<core::cell::RefCell<memledger::ChargedVec<Away>>> = const { std::mem::ManuallyDrop::new(core::cell::RefCell::new(memledger::ChargedVec::none())) }; // alloc: Exec charged crate::BACKEND_OWNER
        /// D3547: the token of the portal whose own message (Execute, Describe, Close) or FETCH is borrowing a level now: that
        /// level's `send_away` leaves it at home.
        static STEPPING: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
    }
}

/// D3547: `STEPPING` for one borrow, put back at its end (an error's too).
pub(super) struct Stepping(u64);

impl Stepping {
    pub(super) fn set(token: u64) -> Stepping {
        Stepping(STEPPING.replace(token))
    }
}

impl Drop for Stepping {
    fn drop(&mut self) {
        STEPPING.set(self.0);
    }
}

/// D3547: before a statement runs at level `depth` (`reentry::run_at`), the level's live named portals go away, each plan
/// slot's portals together with the admission they run. A plan's portals stay when one of them must: the portal whose own
/// message borrows the level (`STEPPING`), an orphaned one (D3579: its plan goes with its last portal, not back to a memo),
/// or an unnamed portal running the same plan; a nested FETCH of those meets the refusal it met before.
pub fn send_away(b: &mut Backend, depth: usize) {
    let mut bits = b.named_home;
    if bits == 0 {
        return;
    }
    let stepping = STEPPING.get();
    while bits != 0 {
        let i = bits.trailing_zeros() as usize;
        let (plan, key) = match b.execs.get(i).and_then(Option::as_ref) {
            Some(s) if s.live && s.named && s.token(i) != stepping => (s.plan, s.key),
            // the portal whose own message borrows the level (an Execute loop's every borrow: one compare)
            Some(s) if s.live && s.named => {
                bits &= bits - 1;
                continue;
            }
            _ => {
                b.named_home &= !(1 << i);
                bits &= bits - 1;
                continue;
            }
        };
        // the live slots running this plan slot's admission (this one among them)
        let runs = b.execs.iter().enumerate().filter(|(_, o)| o.as_ref().is_some_and(|o| o.runs(plan, key))).fold(0u32, |m, (j, _)| m | (1 << j));
        bits &= !(runs | (1 << i));
        let filed = b.plans.get(plan).and_then(Option::as_ref).is_some_and(|p| p.key == key);
        let movable = |j: usize| b.execs[j].as_ref().is_some_and(|o| o.named && !o.pins.orphan && o.token(j) != stepping);
        if !filed || (0..b.execs.len()).any(|j| runs & (1 << j) != 0 && !movable(j)) {
            continue;
        }
        let Some(lent) = b.plans[plan].take() else { continue };
        let mut slots = Vec::with_capacity(runs.count_ones() as usize); // alloc: Exec charged Away::held
        for j in (0..b.execs.len()).filter(|&j| runs & (1 << j) != 0) {
            if let Some(slot) = b.execs[j].take() {
                slots.push((slot.token(j), slot));
            }
            b.named_home &= !(1 << j);
        }
        // the group's block, allocated just above on this thread, moves to the holder
        let o = crate::BACKEND_OWNER;
        let bytes = (slots.capacity() * core::mem::size_of::<(u64, Box<ExecSlot>)>()) as u64;
        let held = memledger::Charge::open(o, memledger::charge::release_of(o), memledger::Kind::Region, memledger::charge::Door::Heap, bytes);
        away_push(Away { held, home: depth, key, a: lent.served, slots });
    }
}

/// An away group into the table, the table on the per-backend holder from its first growth (`drain_away` leaves an
/// uncharged empty one).
fn away_push(w: Away) {
    AWAY.with(|v| {
        let mut v = v.borrow_mut();
        if v.owner().is_none() && v.capacity() == 0 {
            *v = memledger::ChargedVec::new(Some(crate::BACKEND_OWNER), memledger::Kind::Region); // alloc: Exec charged crate::BACKEND_OWNER
        }
        v.with_vec(|v| v.push(w)); // alloc: Exec charged crate::BACKEND_OWNER
    })
}

/// D3547: the away group holding `token`'s portal, out of the table while that portal runs (`put_away` after), and the
/// portal's place in it.
pub(super) fn take_away(token: u64) -> Option<(Away, usize)> {
    AWAY.with(|v| {
        let mut v = v.borrow_mut();
        if v.is_empty() {
            return None;
        }
        let (g, k) = v.iter().enumerate().find_map(|(g, w)| w.slots.iter().position(|(t, _)| *t == token).map(|k| (g, k)))?;
        Some((v.with_vec(|v| v.swap_remove(g)), k))
    })
}

pub(super) fn put_away(w: Away) {
    away_push(w);
}

/// D3547: Describe of an away portal (no run: the slot's wire node only).
fn describe_away(token: u64, formats: &[i16]) -> Option<PgResult<bool>> {
    let (mut w, k) = take_away(token)?;
    let r = (|| -> PgResult<bool> {
        let a = &*w.a;
        if !emits_rows(a.cmd, a.out_natts) {
            return Ok(false); // a write portal without RETURNING has no tupDesc; tcop sends NoData
        }
        let fmts = if formats.is_empty() { None } else { Some(formats) };
        let slot = &mut w.slots[k].1;
        slot.wire.prepare(&a.program, fmts)?;
        slot.wire.send_rowdesc()?;
        Ok(true)
    })();
    put_away(w);
    Some(r)
}

/// D3547: a program of the away portal's admission holds host state a pipeline call on this thread's stack runs now (the
/// executor entry's `Serve::host_busy`, D2355): run away from its busy home, it would share that state with the running
/// statement, so it is refused by name instead (`reentry::AWAY_REFUSAL`).
pub(super) fn host_busy(a: &Admitted) -> bool {
    let running = |o: &dyn core::any::Any| gravity_driver::node::owner_running(o as *const dyn core::any::Any as *const u8);
    let busy = |p: &Program| p.owner.as_deref().is_some_and(running);
    busy(&a.program) || a.registry.owner_of().is_some_and(running) || a.graph.as_ref().is_some_and(|g| g.arms.iter().any(|x| busy(&x.program)) || g.inits.iter().any(|x| busy(&x.program)))
}

/// D3547: PortalDrop of an away portal: its run ends at the first level free now, as at home (PortalCleanup's
/// ExecutorEnd); at its home level its Node parks and its slot goes back to its place for the level's next Bind, away from
/// it (a function's CLOSE while a statement holds the home level) the Node is torn down. The group's last portal gives the
/// admission back at its home level (`give_back_away`), or retires it there (D3346: dropped at that level's next borrow,
/// after the Nodes parked under its key); the others keep it.
fn drop_away(mut w: Away, k: usize) {
    let (token, slot) = w.slots.swap_remove(k);
    let last = w.slots.is_empty();
    let (home, key) = (w.home, w.key);
    let (mut slot, mut w) = (Some(slot), Some(w));
    let _ = crate::admit::reentry::enter(Some(crate::admit::reentry::built), None, false, |b, depth| {
        let at_home = depth == home;
        slot.as_mut().expect("once").drop_portal(&mut b.entry, at_home);
        if at_home {
            let idx = ((token & 0xff) as usize).wrapping_sub(1);
            if b.execs.get(idx).is_some_and(Option::is_none) {
                b.execs[idx] = slot.take();
            }
            if last {
                b.give_back_away(key, w.take().expect("once").a);
            }
        }
    });
    match w {
        Some(w) if last => crate::admit::reentry::retire(home, key, w.a),
        Some(w) => put_away(w),
        None => {}
    }
}

/// D3547: the session's end (`reentry::teardown`, before the levels go): an away portal no PortalDrop reached is left as a
/// level's live slot is (`forget_live`), its admission with it.
/// D3881: a portal sent away from level `depth` runs that level's admission: the level stays while it lives.
pub(crate) fn away_at(depth: usize) -> bool {
    AWAY.with(|v| v.borrow().iter().any(|w| w.home == depth))
}

pub fn drain_away() {
    let left = AWAY.with(|v| core::mem::take(&mut *v.borrow_mut()));
    core::mem::forget(left);
}

/// D3598: the session's end (`reentry::teardown`): an exec slot still live missed its portal's PortalDrop (D3598: a nested
/// CLOSE that reached no slot). Its run's buffer pins, snapshot and relations belonged to the portal's resource owner,
/// which PortalDrop already released ('resource was not closed'), so dropping them again here runs ResourceOwnerForget
/// with no owner: a panic inside a destructor, then the abort that restarts the whole server. Such a slot is left
/// unreleased instead (its memory leaks, once per such miss); a slot whose portal was dropped is not live.
pub fn forget_live(b: &mut Backend) {
    for i in 0..b.execs.len() {
        if b.execs[i].as_ref().is_some_and(|s| s.live) {
            core::mem::forget(b.execs[i].take());
        }
    }
    b.named_home = 0;
}

pub(super) fn register_snapshot() -> PgResult<Rc<SnapshotData<'static>>> {
    let snap = snapmgr::GetTransactionSnapshot()?;
    Ok(snapmgr::RegisterSnapshot(Some(&snap))?.expect("a live snapshot registers"))
}

/// The portal's token and result descriptor. The descriptor is `cache`'s when it was built for this plan (`key`: the plan
/// source and its generation, so a replan builds anew) and this program; else built (`desc_of`) and kept there. It is
/// never mutated after it is built: every portal of the plan shares it, each dropping its clone at PortalDrop.
fn set_portal_desc(portal: &Portal<'static>, token: u64, a: &Admitted, mcx: Mcx<'static>, key: PlanKey, cache: &mut Option<(PlanKey, *const Program, Rc<types_tuple::TupleDescData<'static>>)>) {
    let mut p = portal.borrow_mut();
    p.gravity = token;
    if a.out_natts > 0 {
        let program = Arc::as_ptr(&a.program);
        let desc = match cache {
            Some((k, prog, d)) if *k == key && core::ptr::eq(*prog, program) => d.clone(),
            _ => {
                let d = desc_of(mcx, a);
                *cache = Some((key, program, d.clone()));
                d
            }
        };
        p.tupDesc = Some(desc);
    }
}

/// D3580: WHERE CURRENT OF a named protocol portal the runner serves (no QueryDesc: the entry's `exec_current_of` asks
/// here for a portal with a runner token). The portal's exec slot is read where it is: sent away for the statement that
/// asks (`send_away`), or at its home level. A plan with row marks answers from its run's lock host (execCurrentOf's
/// es_rowmarks arm, `current_rowmark`); any other CURRENT-OF-eligible plan from the row its run's emit last took from its
/// scan (`PullAdapter::taken`: execCurrentOf's scan node current tuple; a bounded Execute's run stops right after its last
/// row), with `current`'s checks; another plan with PG's error when it has no scan of the table. The typed refusal
/// otherwise, and for a portal whose own run asks (its level is the one running) or that no level holds.
pub fn portal_current_of(portal: &Portal<'static>, token: u64, name: &str, table_oid: types_core::Oid, table_name: &str) -> PgResult<Option<types_tuple::ItemPointerData>> {
    use crate::admit::entry::current_of as co;
    let stmts = portal.borrow().stmts;
    let Some(pstmt) = pquery::stmt_list::resolve(stmts).first() else { return Err(co::unsupported(name)) };
    let read = |s: &ExecSlot| s.pins.run.as_ref().map_or((gravity_abi::ScanPos::NONE, gravity_abi::ScanPos::NONE), |r| (r.locked(table_oid), r.taken()));
    let away = AWAY.with(|v| v.borrow().iter().find_map(|w| w.slots.iter().find(|(t, _)| *t == token).map(|(_, s)| read(s))));
    let seen = match away {
        Some(x) => Some(x),
        None if STEPPING.with(|c| c.get()) == token => None,
        None => crate::with_existing_backend(|b| slot_of(b, token).and_then(|i| b.execs[i].as_deref().map(read))).flatten(),
    };
    let Some((locked, taken)) = seen else { return Err(co::unsupported(name)) };
    if !pstmt.rowMarks.is_nil() {
        return co::current_rowmark(portal, pstmt, None, locked, name, table_oid, table_name);
    }
    if matches!(co::plan_cursor(pstmt), co::Cursor::Identity) {
        return match taken.is_valid() {
            true => co::current(portal, pstmt, Some((taken.rel, ((taken.block as u64) << 16) | taken.off as u64)), name, table_oid, table_name),
            false => co::current_running(portal, pstmt, None, name, table_oid, table_name),
        };
    }
    if co::no_scan_of(pstmt, table_oid) {
        return Err(co::not_simply_updatable(name, table_name));
    }
    Err(co::unsupported(name))
}

/// The admitted program's row descriptor under the statement's collations: the root target list's non-junk entries
/// (a set operation's derived collation, 0 where PostgreSQL derives none; a hand program's from the plan — the
/// program's own `OutCol::collation` where no plan is kept). CREATE TABLE AS defines its columns from this descriptor.
pub fn desc_of(mcx: Mcx<'static>, a: &Admitted) -> Rc<types_tuple::TupleDescData<'static>> {
    let mut desc = build_desc(mcx, &a.program);
    let Some(plan) = a.pstmt.and_then(|p| p.planTree).and_then(|n| n.as_plan()) else { return desc };
    let d = Rc::get_mut(&mut desc).expect("a fresh descriptor");
    let mut k = 0i16;
    for tle in plan.targetlist.iter() {
        let Some(t) = tle.as_target_entry() else { break };
        if t.resjunk {
            continue;
        }
        if k as i32 >= d.natts {
            break;
        }
        tupdesc::TupleDescInitEntryCollation(d, k + 1, gravity_decompose::catalog::expr_type::collation_of(t.expr));
        k += 1;
    }
    desc
}

pub fn build_desc(mcx: Mcx<'static>, program: &gravity_program::Program) -> Rc<types_tuple::TupleDescData<'static>> {
    let natts = program.out.len();
    let mut desc = tupdesc::CreateTemplateTupleDesc(mcx, natts as i32).expect("template tupdesc");
    for (i, col) in program.out.iter().enumerate() {
        let name = core::str::from_utf8(col.name_bytes()).unwrap_or("");
        let _ = tupdesc::TupleDescInitEntry(&mut desc, i as i16 + 1, Some(name), col.typid, col.typmod, 0);
        if col.collation != 0 {
            tupdesc::TupleDescInitEntryCollation(&mut desc, i as i16 + 1, col.collation);
        }
    }
    Rc::new(desc) // alloc: Exec charged ExecSlot::held (a slot's cached descriptor; else the statement's, on its thread)
}

/// `ExecCheckPermissions`' failure as PostgreSQL raises it: `aclcheck_error(ACLCHECK_NO_PRIV,
/// get_relkind_objtype(get_rel_relkind(relid)), get_rel_name(relid))` — the object noun and the relation's name, 42501.
pub(super) fn perm_denied(relid: types_core::Oid) -> Box<types_error::PgError> {
    let noun = match lsyscache::relation::get_rel_relkind(relid).unwrap_or(0) as u8 {
        b'S' => "sequence",
        b'v' => "view",
        b'm' => "materialized view",
        b'f' => "foreign table",
        b'i' | b'I' => "index",
        _ => "table",
    };
    let ctx = gravity_driver::mem::root::Root::new("GravityPermError"); // alloc: Cold
    let name = lsyscache::relation::get_rel_name(ctx.mcx(), relid).ok().flatten().map(|n| n.to_string()); // alloc: Cold
    let msg = match name {
        Some(n) => format!("permission denied for {noun} {n}"), // alloc: Cold
        None => format!("permission denied for relation with oid {relid}"), // alloc: Cold
    };
    Box::new(types_error::PgError::error(msg).with_sqlstate(types_error::ERRCODE_INSUFFICIENT_PRIVILEGE)) // alloc: Cold
}

#[cfg(test)]
mod hold_tests {
    use super::*;

    /// E-hold: an exec slot (kept in its level's `execs` for the backend) is on the per-backend holder: its block from
    /// `new`, its key words as a Bind sizes them (their capacity kept from Bind to Bind), so the charge tracks the bytes.
    #[test]
    fn e_hold_an_exec_slot_is_on_the_per_backend_holder() {
        let mut s = Box::new(ExecSlot::new()); // alloc: Cold
        let block = core::mem::size_of::<ExecSlot>() as u64;
        assert_eq!((s.held.owner(), s.held.committed()), (crate::BACKEND_OWNER, block), "its block, at new");
        s.size_words(40);
        let words = (s.kparams.capacity() * 8 + s.knulls.capacity()) as u64;
        assert!(words >= 40 * 9);
        assert_eq!(s.held.committed(), block + words, "its block and its key words' capacity");
        s.size_words(2);
        assert_eq!(s.held.committed(), block + words, "a smaller Bind keeps the buffers, and the charge with them");
    }
}
