//! The Execute crossing and the simple-protocol run: bounded steps under `max_rows`, resume and release of a run.
//! Split out of runner.rs (move-only, lane SPLIT 09-23).

use super::*;
use types_nodes::nodes_enums::CmdType;

/// The Execute crossing. `None`: not a served portal. `Some(None)`: the kernel stopped at `max_rows` rows and the execution stays on the portal
/// (tcop sends PortalSuspended; the next Execute resumes it). `Some(Some(qc))`: complete. Snapshot: registered on the portal's resource owner for the life
/// of the execution (an abort or Close releases it with the owner), pushed active around every call. D3525 / D3526: the
/// Execute runs under the portal's resource owner, as PortalRun does, so the scan's buffer pins belong to the portal (a
/// savepoint begun after the Bind and ended before the next Execute leaves them to it); portalmem ends the run under that
/// owner too (PortalDrop, AtAbort_Portals, AtSubAbort_Portals).
pub fn execute(portal: &Portal<'static>, max_rows: i64, receiver: &mut DestReceiver<'_>) -> PgResult<Option<Option<QueryCompletion>>> {
    let r = pquery::with_portal_owner(portal, || step(portal, max_rows, receiver))?;
    if let Some(done) = &r {
        // D2085: the portal's position, kept as PortalRunSelect keeps it, so a later SQL FETCH of the portal moves on from here
        let count = if max_rows == FETCH_ALL || max_rows <= 0 { 0 } else { max_rows };
        let rows = done.as_ref().map_or(count as u64, |qc| qc.nprocessed);
        let mut p = portal.borrow_mut();
        if rows > 0 {
            p.atStart = false;
        }
        if count == 0 || rows < count as u64 {
            p.atEnd = true;
        }
        p.portalPos += rows;
    }
    Ok(r)
}

/// D2085: a SQL FETCH / MOVE forward over a portal bound here (pquery's PortalRunSelect, which keeps the portal's position):
/// the next `count` rows (0: all) of the run the Execute messages step, to the fetch's receiver; the rows sent.
pub fn fetch(portal: &Portal<'static>, count: i64, receiver: &mut DestReceiver<'_>) -> PgResult<u64> {
    let max_rows = if count <= 0 { FETCH_ALL } else { count };
    match step(portal, max_rows, receiver)? {
        Some(None) => Ok(count as u64),
        Some(Some(qc)) => Ok(qc.nprocessed),
        None => Err(glue::internal("gravity: fetch of a portal not served")),
    }
}

fn step(portal: &Portal<'static>, max_rows: i64, receiver: &mut DestReceiver<'_>) -> PgResult<Option<Option<QueryCompletion>>> {
    let token = portal.borrow().gravity;
    if token == 0 {
        return Ok(None);
    }
    if super::point::is_point(token) {
        return super::point::portal::step(portal, token, max_rows);
    }
    let (formats, params, owner): (Vec<i16>, ParamListHandle, types_resowner::ResourceOwner) = {
        let p = portal.borrow();
        (p.formats.iter().copied().collect(), p.portalParams, p.resowner) // alloc: Cold
    };
    // D3547: a named portal a statement's start sent away (`send_away`) runs from there
    if let Some((w, k)) = take_away(token) {
        return step_away(portal, w, k, &formats, params, owner, max_rows, receiver);
    }
    // D3547: the portal's own message borrows the level: its `send_away` leaves this portal at home
    let _stepping = Stepping::set(token);
    let r = crate::with_backend(|b| -> PgResult<Option<Option<QueryCompletion>>> {
        let Some(idx) = slot_of(b, token) else { return Ok(None) };
        step_slot(b, At::Home(idx), true, portal, &formats, params, owner, max_rows, receiver)
    });
    match r {
        Some(v) => v,
        None => Ok(None),
    }
}

/// D3547 (b): an Execute or FETCH of a portal sent away (`send_away`): run at the first level free now with the admission
/// lent to it, its home level when no statement holds that one; else nested, as the executor entry's serve runs away from
/// a busy level (D2355), refused by name when the statement holding the home level runs inside this program's host state
/// (`host_busy`). Away from its home the run opens under no key and its Node is torn down at its end (`release_run`).
#[allow(clippy::too_many_arguments)]
fn step_away(portal: &Portal<'static>, mut w: Away, k: usize, formats: &[i16], params: ParamListHandle, owner: types_resowner::ResourceOwner, max_rows: i64, receiver: &mut DestReceiver<'_>) -> PgResult<Option<Option<QueryCompletion>>> {
    let home = w.home;
    let hosted = host_busy(&w.a);
    let r = crate::admit::reentry::enter(Some(crate::admit::reentry::built), Some(home), hosted, |b, depth| {
        let Away { slots, a, .. } = &mut w;
        step_slot(b, At::Away(&mut slots[k].1, &mut **a), depth == home, portal, formats, params, owner, max_rows, receiver)
    });
    put_away(w);
    match r {
        Ok(Some(out)) => out,
        Ok(None) => Ok(None),
        Err(why) => Err(refused(why)),
    }
}

/// The slot a step runs and its admission (D3547): the level's own exec slot and its plan's memo entry, or a portal sent
/// away with the admission lent to it.
enum At<'w> {
    Home(usize),
    Away(&'w mut ExecSlot, &'w mut Admitted),
}

/// One Execute (or FETCH) of the slot `at` names, on the driver entry and admit context of level `b`, which runs it.
/// `home`: that level is the slot's own, whose park its Node comes from and goes back to; away from it the run opens under
/// no key and its Node is torn down at its end (D2190: a Node built over a lent admission is never parked at another
/// level). The run's census, when it ends, is noted at `b`.
#[allow(clippy::too_many_arguments)]
fn step_slot(b: &mut Backend, at: At<'_>, home: bool, portal: &Portal<'static>, formats: &[i16], params: ParamListHandle, owner: types_resowner::ResourceOwner, max_rows: i64, receiver: &mut DestReceiver<'_>) -> PgResult<Option<Option<QueryCompletion>>> {
    let amcx = b.admit_mcx();
    let Backend { entry, execs, plans, .. } = &mut *b;
    let (slot, a): (&mut ExecSlot, Option<&mut Admitted>) = match at {
        At::Home(idx) => {
            let slot = execs[idx].as_mut().expect("slot");
            let a = plans.get_mut(slot.plan).and_then(Option::as_mut).map(|p| p.served.as_mut());
            (&mut **slot, a)
        }
        At::Away(slot, a) => (slot, Some(a)),
    };
    // C8: what this step's run takes (its part uses) is the portal's
    let _owner = slot.pins.enter_owner();
    if slot.ended {
        // D307: a portal whose run has ended sends no row and completes with a count of 0 (PostgreSQL's portal at its
        // end: PortalRunSelect moves nowhere); a write without RETURNING ran its one time (PortalRunMulti's
        // MarkPortalDone: the next PortalRun refuses the portal)
        if !emits_rows(slot.cmd, slot.out_natts) {
            return Err(cannot_run(portal));
        }
        let tag = a.map_or(slot.cmd, |a| a.tag());
        return Ok(Some(Some(gravity_wire::completion(tag, 0))));
    }
    let a = a.expect("admitted");
    // D4131: the PL state before the open and the step, for what a dropped held error's callee leaves (below)
    let start = PlState::now();
    let remote = matches!(receiver.mydest(), CommandDest::Remote | CommandDest::RemoteExecute | CommandDest::RemoteSimple);
    let fmts = if formats.is_empty() { None } else { Some(formats) };
    if !slot.active() {
        if a.eval_at_execute {
            // the words evaluated per execution (a volatile word) run under the portal's snapshot, pushed for them as
            // PortalStart's is before the executor evaluates its expressions (D268): a write's snapshot is taken here,
            // first, and kept in the slot, so a failed evaluation releases it with the run
            if slot.pins.snap.is_none() {
                let (snap, cid) = crate::portal_snapshot::at_execute(a, owner)?;
                (slot.pins.snap, slot.cid, slot.pins.owner) = (Some(snap), cid, owner);
            }
            let snap = slot.pins.snap.clone().expect("registered");
            // the slot's key words, sized by the statement and bound in place (evaluated again at every Execute)
            slot.size_words(key_words(a));
            snapmgr::PushActiveSnapshot(&snap)?;
            let keyed = eval_keys(a, params, &mut slot.kparams, &mut slot.knulls, amcx, true);
            snapmgr::PopActiveSnapshot()?;
            if !keyed? {
                return Err(glue::internal("gravity: insert row evaluation"));
            }
        }
        let bound_cid = slot.cid;
        let bound = slot.pins.snap.take().map(|s| (s, bound_cid));
        let (snap, cid) = match bound {
            Some(held) => held,
            None => crate::portal_snapshot::at_execute(a, owner)?,
        };
        crate::phases::mark("snapshot");
        slot.pins.snap = Some(snap);
        slot.cid = cid;
        slot.pins.owner = owner;
        slot.served = 0;
        slot.rechecked = 0;
        if slot.pins.run.as_ref().is_none_or(|_| slot.run_natts != a.program.out.len()) {
            // memsig E-pub: the slot keeps its adapter from Bind to Bind (past each portal's end), so its buffer is the
            // session's, whose backend holds the slot
            let acct = memledger::current();
            let session = acct.is_backend().then_some((memledger::Owner::Session(acct), memledger::Kind::Region));
            slot.pins.run = Some(PullAdapter::new_on(a.program.out.len(), a.program.layout.out_rows as usize, session));
            slot.run_natts = a.program.out.len();
        }
        slot.wired = false;
        if a.graph.as_ref().is_some_and(|g| !g.inits.is_empty()) {
            let snap = slot.pins.snap.clone().expect("registered");
            snapmgr::PushActiveSnapshot(&snap)?;
            let nk = a.nkparams;
            let r = run_inits(amcx, entry, a, &snap, cid, &mut slot.kparams[..nk], &mut slot.knulls[..nk]).and_then(|_| crate::ops::append::prune::exec_skip(a, &slot.kparams[..nk], &slot.knulls[..nk]));
            snapmgr::PopActiveSnapshot()?;
            r?;
        }
        let s: &'static mut ExecSlot = super::raw::run_slot(&mut *slot);
        a.registry.rewind();
        // NEW-57: the Node parks under the content park key where its audit allows (`park_key`, as the simple protocol's
        // runs), so a statement Parsed again for each execution (a new plan source: a new plan key every time) takes the
        // Node the last one parked; else under the slot's plan key
        // D3579: an orphaned plan's Node parks under no plan key (another admission of the key would take it)
        // D3547: nor does a run opened away from the slot's home level (its Node is built here and torn down at its end)
        let key = if home { park_key(amcx, a).or((!s.pins.orphan).then_some(s.key)) } else { None };
        let run = s.pins.run.as_mut().expect("adapter");
        crate::ops::modifytable::after_query::begin(slot.cmd);
        let snap = s.pins.snap.clone().expect("registered");
        snapmgr::PushActiveSnapshot(&snap)?;
        // D264: a serial pull over a granule relation resolves the statement's relations itself (no producers here)
        let opened = resolve_pull(amcx, &a.registry, &snap).and_then(|mut pulled| file_pull_sets(amcx, &a.program, &a.registry, a.pstmt, crate::ops::append::proto::Initial::params_of(a), pulled.as_deref_mut()).map(|()| pulled)).and_then(|pulled| {
            s.pins.pulled = pulled;
            // D288: its v4 protocols too (a serial pgrc program's src.verdicts), their words bound as the entry's serial pull's
            s.pins.serial = pull_words(amcx, a, &snap, &s.kparams[..s.nkparams], &s.knulls[..s.nkparams])?;
            let (kp, kn) = s.pins.serial.as_deref().map_or((&s.kparams[..s.nkparams], &s.knulls[..s.nkparams]), |w| w.words());
            let parts = glue::binding_parts(s.pins.snap.as_ref().expect("registered"), cid, kp, kn, s.pins.pulled.as_deref());
            // the extended protocol's pull is not admitted (F8: its grant is the GUCs', no longer an 8 MiB default)
            run.open(entry, key, &a.program, &mut a.registry, parts, crate::inputs::knobs::grant::unadmitted_for(&a.program, 1)).map_err(glue::pg_error)
        });
        snapmgr::PopActiveSnapshot()?;
        if let Err(e) = opened {
            release_run(slot, entry, home);
            return Err(e);
        }
        crate::phases::mark("open");
    }
    // the wire is prepared at the first protocol Execute of the run: a run a FETCH opened (D2085's SQL FETCH; D3547: a
    // PL/pgSQL FETCH of a portal sent away, before its first Execute) sent its rows to that statement's receiver
    if remote && !slot.wired && emits_rows(slot.cmd, slot.out_natts) {
        slot.wire.prepare(&a.program, fmts)?;
        slot.wired = true;
    }
    let mechs = (a.program.key.0, a.program.used); // MECHCORPUS: the mechanisms line's key and record, read before the run
    let snap = slot.pins.snap.clone().expect("registered");
    snapmgr::PushActiveSnapshot(&snap)?;
    let out = match bounded_step(entry, a, slot, amcx, max_rows, remote, receiver) {
        Ok(Some(rows)) => crate::ops::modifytable::after_query::end(slot.cmd).map(|()| Some(rows)),
        o => o,
    };
    snapmgr::PopActiveSnapshot()?;
    crate::phases::mark("step");
    // D4131: an Execute that completed or suspended may have dropped an error held inside user code (evaluated ahead, on
    // a row PostgreSQL never reaches): its callee's leftover (an SPI connection) is undone, as `simple_at` undoes it
    if out.is_ok() && !start.kept() {
        start.restore();
    }
    match out {
        Ok(Some(rows)) => {
            let (cmd, tag, shape, code, via_) = (slot.cmd, a.tag(), a.shape, a.program.code_of(0).kind(), via(a));
            // the census counts the run's rows; the tag, this Execute's (D307)
            let run_rows = if cmd == Cmd::Select { slot.served } else { rows };
            let protocols = slot.pins.serial.as_deref().map_or(Ok(()), SerialWords::end);
            release_run(slot, entry, home); // the slot's snapshot leaves its owner here (one release for a completed, failed or dropped run)
            slot.ended = true;
            if !slot.pins.held_to_drop {
                slot.pins.held = held::Held::default(); // D761: a write's ProcessQuery ends its executor within the Execute
            }
            protocols?;
            check_after_teardown()?; // D376: an interrupt the teardown's spill walk left pending ends the statement
            crate::phases::mark("end");
            if crate::phases::on() {
                eprintln!("PHASES stmt ext us:{}", crate::phases::line()); // alloc: Cold
            }
            b.note_served(shape, run_rows, via_, code);
            b.note_mechs(mechs.0, mechs.1);
            // D307: PostgreSQL's executor stops at the count-th row without looking past it, so an Execute that sends
            // exactly its max_rows suspends the portal though no row is left (atEnd only when fewer came); the next
            // Execute completes it with none (the `ended` slot above)
            if cmd == Cmd::Select && max_rows > 0 && max_rows != FETCH_ALL && rows == max_rows as u64 {
                return Ok(Some(None));
            }
            Ok(Some(Some(gravity_wire::completion(tag, rows))))
        }
        Ok(None) => Ok(Some(None)),
        Err(e) => {
            release_run(slot, entry, home);
            Err(e)
        }
    }
}

/// One Execute's worth of a served run: up to `max_rows` rows to the receiver (all of them for FETCH_ALL). `Some(rows)` when the run has
/// ended (a SELECT's rows sent by THIS Execute, PortalRun's nprocessed, D307; a write's count), `None` when it suspended.
#[allow(clippy::too_many_arguments)]
fn bounded_step(entry: &mut gravity_driver::Entry, a: &mut Admitted, slot: &mut ExecSlot, amcx: Mcx<'static>, max_rows: i64, remote: bool, receiver: &mut DestReceiver<'_>) -> PgResult<Option<u64>> {
    let cmd = slot.cmd;
    let _sent = gravity_driver::ahead_trace::statement(slot.served > 0); // construct C2 R3: an earlier Execute sent rows
    let rows_out = emits_rows(cmd, slot.out_natts);
    // a write here emits no row: a RETURNING write is the portal's own (`entry_routed`, D315)
    let want = if max_rows == FETCH_ALL || max_rows <= 0 { usize::MAX } else { max_rows as usize };
    let dctx;
    let mut dest_sink = None;
    if rows_out && !remote {
        dctx = gravity_driver::mem::root::Root::new("GravityDestSink");
        let mcx: Mcx<'static> = super::raw::sink_mcx(&dctx);
        dest_sink = Some(DestSink::new(mcx, desc_of(mcx, &a), receiver));
    }
    let wire = &mut slot.wire as *mut gravity_wire::WireNode;
    let sp: *mut ExecSlot = slot;
    let run = slot.pins.run.as_mut().expect("adapter");
    // D2890: an Execute under max_rows (a SQL FETCH n of the portal too) asks for the rows it owes only (D439's cap,
    // `set_owed_cap`): PostgreSQL computes a row in the Execute that reads it, so a SET between two Executes (TimeZone,
    // DateStyle) reaches the rows after it. An Execute that reads every row asks for a whole block, as does a write (it
    // runs to its end whatever max_rows says).
    run.set_owed_cap(rows_out && want != usize::MAX);
    let mut consume = |v: &[u64], n: &[u8]| -> Result<(), Error> {
        if !rows_out {
            return Ok(());
        }
        match dest_sink.as_mut() {
            None => super::raw::wire(wire).send_row(v, n).map_err(gravity_hosts::stash),
            Some(d) => d.send_row(v, n).map(|_| ()).map_err(gravity_hosts::stash),
        }
    };
    let mut rechecked = 0u64;
    let mut total = 0usize;
    let served = loop {
        match run.fetch(entry, want - total, &mut consume) {
            Ok(n) => {
                total += n;
                if !run.is_done() {
                    break total;
                }
                let s: &'static mut ExecSlot = super::raw::run_slot(sp);
                let (pstmt, params) = (a.pstmt, crate::ops::append::proto::Initial::params_of(a));
                let Some(arm) = a.graph.as_mut().and_then(|g| {
                    while g.arms.get(s.arm).is_some_and(|x| x.skip) {
                        s.arm += 1; // a pruned arm (initial partition pruning at bind) is stepped over
                    }
                    g.arms.get_mut(s.arm)
                }) else { break total };
                run.close(entry, true);
                s.pins.unpark_orphan(entry, s.key);
                check_after_teardown()?; // D376: a cancel or timeout landing between the Append arms ends the statement
                // D264: the arm's own granule relations (the closed run's are no longer read); D288: the main program's protocol words are
                // not the arm's (an arm binds its key params, as the entry's open_next_arm)
                s.pins.serial.as_deref().map_or(Ok(()), SerialWords::end)?;
                s.pins.serial = None;
                s.pins.pulled = resolve_pull(amcx, &arm.registry, s.pins.snap.as_ref().expect("registered"))?;
                file_pull_sets(amcx, &arm.program, &arm.registry, pstmt, params, s.pins.pulled.as_deref_mut())?;
                let parts = glue::binding_parts(s.pins.snap.as_ref().expect("registered"), s.cid, &s.kparams[..s.nkparams], &s.knulls[..s.nkparams], s.pins.pulled.as_deref());
                arm.registry.rewind();
                run.open(entry, None, &arm.program, &mut arm.registry, parts, crate::inputs::knobs::grant::unadmitted_for(&arm.program, 1)).map_err(glue::pg_error)?;
                s.arm += 1;
                if total >= want {
                    break total;
                }
            }
            Err(Error::Recheck(site)) => {
                total += run.take_partial();
                let pos = run.recheck_pos();
                let positions: Vec<gravity_abi::ScanPos> = run.positions().to_vec(); // alloc: Cold
                let binding = run.binding_mut().expect("open");
                let more = crate::rerun::epq::recheck(entry, a, Some(slot.key), binding, &positions, site as usize, pos)?;
                if cmd == Cmd::Select {
                    if run.exec().is_some_and(resumes) {
                        continue; // a multirow lock (write.rs): the rechecked row is buffered in order, the scan goes on
                    }
                    // a one-row lock (the hand OL kernels): the surviving row is the statement's last
                    let drained = run.drain(&mut consume).map_err(glue::pg_error)?;
                    slot.served += (total + drained) as u64;
                    return Ok(Some((total + drained) as u64));
                }
                // runner need 18: the write resumes past the rechecked row
                rechecked += more;
                if !run.exec().is_some_and(resumes) {
                    return Ok(Some(slot.served + rechecked + run.rows_processed()));
                }
            }
            Err(e) => return Err(glue::pg_error(e)),
        }
    };
    slot.rechecked += rechecked;
    slot.served += served as u64;
    if run.is_done() {
        let rows = if cmd == Cmd::Select { served as u64 } else { run.rows_processed() + slot.rechecked };
        return Ok(Some(rows));
    }
    Ok(None)
}

/// PostgreSQL's refusal of a portal that already ran (portalmem.c MarkPortalActive, 55000).
#[cold]
#[inline(never)]
fn cannot_run(portal: &Portal<'static>) -> Box<types_error::PgError> {
    let name = portal.borrow().name.as_str().to_string(); // alloc: Cold
    Box::new(types_error::PgError::error(format!("portal \"{name}\" cannot be run")).with_sqlstate(types_error::error::ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE)) // alloc: Cold
}

/// D3198: an executor-hook module (contrib exec_hooks: pg_stat_statements tracking at this level, auto_explain loaded
/// with a duration threshold) acts on this backend's statements now. The runner serves a statement with no QueryDesc,
/// so the hooks could not see it: `simple` and `bind` leave such a statement to the portal, whose executor seams the
/// entry serves with the hooks fired (ExecutorStart .. ExecutorEnd, as C's hooks wrap them). Routing, never a refusal.
pub(crate) fn executor_hooks_active() -> bool {
    execmain_seams::tap_executor_hooks_active::call_if_or(false, |f| f())
}

/// A portal that sends rows: a SELECT, or a write with RETURNING (its out columns).
/// D315 route (b) (M4 Decision 7): a write with RETURNING rows (PORTAL_ONE_RETURNING), or any write in a named portal, is
/// not the runner's: the portal runs its own strategy and the executor's entry seam serves the statement. FillPortalStore
/// runs it to completion into the portal's hold store, its AFTER triggers fire at ExecutorFinish before any row is sent,
/// and RunFromStore pages the rows by each Execute's max_rows; a named portal's write without rows is PORTAL_MULTI_QUERY's
/// one run. Routing, never a refusal (no `fall`: the fallback's setting does not make it an ERROR), decided on the plan
/// before any admission, which the entry seam makes for itself.
pub(super) fn entry_routed(pstmt: &PlannedStmt<'_>, named_portal: bool) -> bool {
    pstmt.commandType != CmdType::CMD_SELECT && (pstmt.hasReturning || named_portal)
}

/// D2576: a query the rewriter added in place of the statement (canSetTag false: a DO INSTEAD rule whose action is another
/// command, QueryRewrite step 3) is not the runner's even alone in its list: ChoosePortalStrategy makes it
/// PORTAL_MULTI_QUERY, whose PortalRunMulti completes with the original command's tag and zero counts ("INSERT 0 0"), not
/// the query's own. Routing, never a refusal: the portal's ProcessQuery offers it at the entry seam, which serves it.
pub(super) fn rewriter_added(pstmt: &PlannedStmt<'_>) -> bool {
    !pstmt.canSetTag
}

/// D719 (V4R-254, Decision 7's carve-out): a simple-protocol RETURNING write the runner serves, its RowDescription and rows
/// held and sent after the statement's AFTER triggers (`WireNode::hold`), where RunFromStore sends a PORTAL_ONE_RETURNING
/// portal's. At most one row, the facts home's bound on the plan under the write (`rows::max_rows`): an INSERT of one
/// VALUES row (ON CONFLICT too), an UPDATE / DELETE by a unique key; a plain table without children; no AFTER trigger but
/// the internal ones that can only raise (a user one may change the GUCs the rows' text depends on, an RI action runs
/// other tables' triggers); built-in output types (a user type's output function would run before the AFTER triggers).
/// Anything else keeps route (b).
fn held_bound(mcx: Mcx<'static>, pstmt: &'static PlannedStmt<'static>) -> PgResult<bool> {
    let Some(mt) = pstmt.planTree.and_then(|n| n.as_modify_table()) else { return Ok(false) };
    let rti = mt.resultRelations.first().unwrap_or(0) as u32;
    let Some(r) = shapes::rte(pstmt, rti) else { return Ok(false) };
    if pstmt.hasModifyingCTE || mt.resultRelations.len() != 1 || r.inh || r.relkind != types_rel::pg_class::RELKIND_RELATION {
        return Ok(false);
    }
    let builtin = mt.plan.targetlist.iter().all(|n| n.as_target_entry().is_some_and(|t| gravity_lowerv4::effects::visible::admit::builtin_type(execexpr::expr_type(t.expr))));
    let facts = gravity_hosts::facts::RelcacheFacts::new(mcx);
    let one = matches!(mt.operation, CmdType::CMD_INSERT | CmdType::CMD_UPDATE | CmdType::CMD_DELETE)
        && mt.plan.lefttree.and_then(|n| gravity_decompose::catalog::rows::max_rows(&facts, pstmt, n)).is_some_and(|m| m <= 1);
    if !(one && builtin) || !relcache_seams::relation_get_trigger_desc::is_installed() {
        return Ok(false);
    }
    // trigger.h: AFTER = neither BEFORE nor INSTEAD. fmgroids.h: the RI action functions (ri_triggers.c: CASCADE, SET NULL,
    // SET DEFAULT on delete / update); NO ACTION / RESTRICT only raise.
    const TRIGGER_TYPE_BEFORE: i16 = 1 << 1;
    const TRIGGER_TYPE_INSTEAD: i16 = 1 << 6;
    const F_RI_FKEY_CASCADE_DEL: types_core::Oid = 1646;
    const F_RI_FKEY_CASCADE_UPD: types_core::Oid = 1647;
    const F_RI_FKEY_SETNULL_DEL: types_core::Oid = 1650;
    const F_RI_FKEY_SETNULL_UPD: types_core::Oid = 1651;
    const F_RI_FKEY_SETDEFAULT_DEL: types_core::Oid = 1652;
    const F_RI_FKEY_SETDEFAULT_UPD: types_core::Oid = 1653;
    const RI_ACTIONS: [types_core::Oid; 6] =
        [F_RI_FKEY_CASCADE_DEL, F_RI_FKEY_CASCADE_UPD, F_RI_FKEY_SETNULL_DEL, F_RI_FKEY_SETNULL_UPD, F_RI_FKEY_SETDEFAULT_DEL, F_RI_FKEY_SETDEFAULT_UPD];
    Ok(relcache_seams::relation_get_trigger_desc::call(r.relid)?.is_none_or(|d| {
        d.triggers.iter().all(|t| t.tgtype & (TRIGGER_TYPE_BEFORE | TRIGGER_TYPE_INSTEAD) != 0 || (t.tgisinternal && !RI_ACTIONS.contains(&t.tgfoid)))
    }))
}

pub(super) fn emits_rows(cmd: Cmd, out_natts: usize) -> bool {
    cmd == Cmd::Select || out_natts > 0
}

/// A kernel that leaves with a recheck and its resume word set continues past that row on the next
/// call; one that leaves without it (the hand OL kernels: every call seeks afresh) is a one-row statement.
pub(super) fn resumes(exec: &Exec) -> bool {
    super::raw::resume(exec.node().ctx()) != gravity_abi::ctx::RESUME_NONE
}

/// D376: CHECK_FOR_INTERRUPTS after a run's teardown, wherever the caller can return an error. The teardown drops the
/// spill sets, whose temp-directory walk (fd walkdir, the one check in PostgreSQL's fd.c) runs with interrupts held
/// (FileSet::drop): a cancel, statement_timeout or terminate that lands there stays pending and is raised here, as
/// PostgreSQL raises it from the walk; unchecked, the statement ran on (the next Append arm) or completed.
pub fn check_after_teardown() -> PgResult<()> {
    // D743: a statement_timeout whose deadline passed in the run fires here even if the timer thread has not run yet.
    if timeout_seams::fire_due::is_installed() {
        timeout_seams::fire_due::call();
    }
    cancelpoll::interrupts::check()
}

/// End a slot's execution (`PortalPins::close_run`) and zero its run's counters. D3547: `home` false: the run ends at
/// another level than its slot's own, its Node torn down.
pub(super) fn release_run(slot: &mut ExecSlot, entry: &mut gravity_driver::Entry, home: bool) {
    slot.pins.close_run(entry, home, slot.key);
    slot.served = 0;
    slot.rechecked = 0;
    slot.arm = 0;
}

/// The statement's context, the wire node and the context's root. D842: the statement's context is a wholesale child
/// (C's AllocSetReset: every chunk goes, live or not), as PostgreSQL resets the context a statement's ExprStates were
/// made in: an ExprState's result cell (`resnd`, execexpr) lives until its context resets, never freed by the state's drop,
/// and a key expression's state is admitted into this context (`bind_words`), so an exact root's leak check fired at the
/// statement's end. First field first: the child drops before its root. Field 0 is the box's own block on the per-backend
/// holder (`crate::BACKEND_OWNER`: `KEPT` is the thread's, kept across statements), FIRST so it drops before the block is
/// freed.
type Kept = Box<(memledger::Charge, MemoryContext, gravity_wire::WireNode, gravity_driver::mem::root::Root)>;

leader_tls::leader_tls! {
    thread_local! {
        /// H16 (a), what the simple protocol keeps across statements (1.2 + 1.0 µs a statement on C01 built anew): the admission's
        /// memory context, reset at the statement's end instead of built and dropped, and the wire node, whose prepared formats
        /// serve the next statement of the same program (`WireNode::prepare` keys on the program's content key and the formats;
        /// the content key hashes the output names). Taken by a statement and put back at its end; a nested one builds its own.
        static KEPT: core::cell::RefCell<Option<Kept>> = const { core::cell::RefCell::new(None) };
        static KEPT_CLEANUP_ARMED: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    }
}

/// D3517: `KEPT` goes at session teardown (State phase), while the arenas its wire node's output caches point into
/// still live; its TLS destructor ran after the session's roots were freed (range_out's fn_extra over a composite
/// subtype dropped its type-cache Rc's into a dead arena: SIGSEGV at backend exit). Re-armed for a reused thread.
fn drop_kept_at_teardown() {
    let _ = KEPT_CLEANUP_ARMED.try_with(|c| c.set(false));
    let kept = KEPT.try_with(|k| k.borrow_mut().take()).ok().flatten();
    core::mem::drop(kept);
    super::point::drop_state_at_teardown();
}

/// D3517: the runner's kept state (KEPT, and the point path's run state) drops at the session's teardown, before the
/// teardown frees the contexts it points into: one registration per session, by whichever keeps state first.
pub(super) fn arm_teardown() {
    if KEPT_CLEANUP_ARMED.try_with(|c| !c.replace(true)).unwrap_or(false) {
        ::mcx::register_session_cleanup(Box::new(drop_kept_at_teardown)); // alloc: Cold
    }
}

/// A statement's kept pieces, back to `KEPT` at drop with the context reset (every object admitted into it dropped before).
struct KeptGuard(Option<Kept>);

fn kept_new() -> Kept {
    let root = gravity_driver::mem::root::Root::new("GravitySimpleRoot");
    let o = crate::BACKEND_OWNER;
    // the box's block, allocated just below on this thread
    let held = memledger::Charge::open(o, memledger::charge::release_of(o), memledger::Kind::Region, memledger::charge::Door::Heap, core::mem::size_of::<(memledger::Charge, MemoryContext, gravity_wire::WireNode, gravity_driver::mem::root::Root)>() as u64);
    Box::new((held, root.new_child_wholesale("GravitySimple"), gravity_wire::WireNode::new(), root)) // alloc: Cold charged crate::BACKEND_OWNER
}

impl KeptGuard {
    fn take() -> KeptGuard {
        let kept = KEPT.with(|k| k.borrow_mut().take());
        KeptGuard(Some(kept.unwrap_or_else(kept_new)))
    }
}

impl Drop for KeptGuard {
    fn drop(&mut self) {
        if let Some(mut k) = self.0.take() {
            k.1.reset();
            let _ = KEPT.try_with(|slot| *slot.borrow_mut() = Some(k));
            arm_teardown();
        }
    }
}

pub fn simple(pstmt: &PlannedStmt<'_>, dest: &mut DestReceiver<'_>) -> PgResult<Option<QueryCompletion>> {
    ADMISSION_REFUSED.with(|c| c.set(false));
    let armed = REPLAN_ARMED.with(|c| c.replace(false));
    let r = simple_at(pstmt, dest, None, armed);
    DEFER_REFUSAL_LINE.with(|c| c.set(false));
    r
}

leader_tls::leader_tls! {
    thread_local! {
        /// Re-plan and retry (v0.4 FALLBACK-OPTIONS (d)): the last `simple` call refused its statement at the admission,
        /// before anything ran or reached the client (no RowDescription, no row, no snapshot taken).
        static ADMISSION_REFUSED: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
        /// Re-plan and retry: tcop may plan the next `simple` statement again if its admission refuses (`replan_arm`).
        static REPLAN_ARMED: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
        /// Re-plan and retry: an armed statement's admission is running; its refusal line waits (`refusal_report`).
        static DEFER_REFUSAL_LINE: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
        /// Re-plan and retry: the first plan's refusal line, held until tcop knows whether another plan was served
        /// (`replan_end`).
        static PENDING_REFUSAL_LINE: core::cell::RefCell<Option<String>> = const { core::cell::RefCell::new(None) };
    }
}

/// Re-plan and retry: tcop may plan the next `simple` statement again, so a refusal of its admission does not log the
/// statement's `gravity: graph refused` verdict line yet (`refusal_report`, `replan_end`).
pub fn replan_arm() {
    REPLAN_ARMED.with(|c| c.set(true));
}

/// Re-plan and retry: the statement's road ends. `served` false: no plan was served, and the first plan's refusal line
/// (held by `refusal_report`) is logged as the statement's verdict; true: another plan ran, and the line is dropped.
pub fn replan_end(served: bool) {
    let held = PENDING_REFUSAL_LINE.with(|p| p.borrow_mut().take());
    if let Some(line) = held.filter(|_| !served) {
        let _ = crate::with_existing_backend(|b| b.report_line(&line));
    }
}

/// The admission's `gravity: graph refused (..)` report line, or, inside an armed statement's admission (re-plan and
/// retry), held: the first plan's line waits for `replan_end`, a later plan's is logged as a `gravity: replan refused`
/// line, which no reader takes for the statement's verdict.
pub(crate) fn refusal_report(b: &crate::Backend, line: String) {
    if !DEFER_REFUSAL_LINE.with(|c| c.get()) {
        return b.report_line(&line);
    }
    let first = PENDING_REFUSAL_LINE.with(|p| {
        let mut p = p.borrow_mut();
        let first = p.is_none();
        if first {
            *p = Some(line.clone()); // alloc: Cold
        }
        first
    });
    if !first {
        b.report_line(&line.replacen("gravity: graph refused", "gravity: replan refused", 1));
    }
}

/// Re-plan and retry: whether the last `simple` call's error is an admission refusal (`ADMISSION_REFUSED`), read once.
/// tcop's exec_simple_query plans the statement again under other planner settings and offers each plan in turn
/// (simple_query.rs `replan_refused`); a refusal at run time, or any other error, is the statement's.
pub fn simple_refused_at_admission() -> bool {
    ADMISSION_REFUSED.with(|c| c.replace(false))
}

/// Re-plan and retry: the report line of a statement served by a plan made after its first plan was refused.
pub fn note_replanned(settings: &str) {
    let _ = crate::with_existing_backend(|b| {
        if b.reporting() {
            b.report_line(&format!("gravity: replanned ({settings}) after an admission refusal")); // alloc: Cold
        }
    });
}

/// `simple`; `Some(snap)`: the plan-order run of a statement whose licensed run failed before any row left it (v5
/// errorhold, `runner::recheck`), under that run's snapshot (registered for it): PostgreSQL ran the statement once,
/// under one snapshot.
/// pglfast C7: the most key words `simple_at` keeps on its frame (256 + 32 bytes); a statement with more takes the heap.
const KEY_WORDS_ON_FRAME: usize = 32;

fn simple_at(pstmt: &PlannedStmt<'_>, dest: &mut DestReceiver<'_>, mut kept_snap: Option<Rc<SnapshotData<'static>>>, armed: bool) -> PgResult<Option<QueryCompletion>> {
    stack_depth_core::check_stack_depth()?;
    // D3198: an executor-hook module acts on this backend's statements: the statement takes the portal's way, whose
    // executor seams fire the hooks for a statement the entry serves (admit::entry)
    if kept_snap.is_none() && executor_hooks_active() {
        return Ok(None);
    }
    // D616: PostgreSQL's exec_simple_query creates the statement's unnamed portal (CreatePortal("", true, true)) before it
    // runs anything, which drops an unnamed portal the extended protocol left in the transaction; a served statement never
    // reaches that line (tcop simple_query.rs calls this seam first), so the old portal lived on and its next Execute sent
    // rows where PostgreSQL raises 34000. Dropped here, for every statement this seam sees (tcop's CreatePortal drops it
    // anyway for one that falls through). The unnamed prepared statement is dropped before the seam (drop_unnamed_stmt).
    if let Some(unnamed) = portalmem::GetPortalByName(Some("")) {
        portalmem::PortalDrop(&unnamed, false)?;
    }
    prime_once();
    if let Some(r) = kept_snap.is_none().then(|| super::point::serve(pstmt, dest)).flatten() {
        return r;
    }
    crate::phases::reset();
    let mut kept = KeptGuard::take();
    let (_, ctx, wire, _) = &mut **kept.0.as_mut().expect("kept");
    wire.drop_held(); // D719: a failed held statement's messages are never sent
    let mcx: Mcx<'static> = super::raw::kept_mcx(ctx.mcx());
    let pstmt_s: &'static PlannedStmt<'static> = super::raw::current_stmt(pstmt);
    // v5 errorhold: the failed licensed run's snapshot, registered once more for the plan-order run, and its error
    let mut again: Option<Rc<SnapshotData<'static>>> = None;
    let mut first: Option<Box<types_error::PgError>> = None;
    let r = crate::with_backend(|b| -> PgResult<Option<QueryCompletion>> {
        if !admit::offered(pstmt_s) {
            b.note_fallthrough(admit::NOT_OFFERED);
            return Ok(None);
        }
        if rewriter_added(pstmt_s) {
            return Ok(None);
        }
        let remote = matches!(dest.mydest(), CommandDest::Remote | CommandDest::RemoteExecute | CommandDest::RemoteSimple);
        // D3134: postgres --single's receiver (tcop routes DestDebug here too), started before the run and shut down after
        // it as ExecutorRun does for a statement that sends tuples; a licensed SELECT holds its header (debugtup's
        // `hold_header`) as the wire holds its RowDescription, so the plan-order run may print its own
        let debug = matches!(dest.mydest(), CommandDest::Debug);
        // D719 (V4R-254): a RETURNING write with a one-row bound stays here, its wire held to the statement's end
        let held = entry_routed(pstmt_s, false) && remote && admit::redelivering(|| held_bound(mcx, pstmt_s))?;
        if entry_routed(pstmt_s, false) && !held {
            return Ok(None);
        }
        // C3 S4c: nothing files this admission (no plan key): it records no reads (`admit::unfiled`)
        // D4454: the admission is written once into this frame's slot and read there (never moved through the layers)
        let mut slot = None;
        DEFER_REFUSAL_LINE.with(|c| c.set(armed && kept_snap.is_none()));
        let admitted = crate::admit::unfiled(|| admit_any(b, mcx, pstmt_s, ParamListHandle::NULL, false, false, WORDS, &mut slot));
        DEFER_REFUSAL_LINE.with(|c| c.set(false));
        if admitted.as_ref().is_err_and(|e| refusal_of(e).is_some()) && kept_snap.is_none() {
            ADMISSION_REFUSED.with(|c| c.set(true));
        }
        admitted?;
        let Some(a) = slot.as_mut() else {
            ADMISSION_REFUSED.with(|c| c.set(kept_snap.is_none()));
            return Err(refused(&last_refusal_line()));
        };
        crate::phases::mark("admit");
        // v5 errorhold: a licensed SELECT into another receiver goes to the portal path, which re-runs into it (`admit::entry`)
        if !remote && !debug && rechecks(a, pstmt_s) {
            return Ok(None);
        }
        grant_start();
        start_checks(pstmt_s, a.cmd, b.admit_mcx())?;
        crate::phases::mark("perms");
        // the portal's snapshot is taken before the statement's expressions run (PortalStart), and a bind-time
        // word may run SQL (`INSERT … VALUES (f())` with an ANALYZE in `f`: SPI needs an outer snapshot)
        let snap = match kept_snap.take() {
            Some(s) => s,
            None => {
                injection_point::injection_point("gravity-simple-before-snapshot")?;
                register_snapshot()?
            }
        };
        // A front row committed before this snapshot is one it sees: a changed answer is the portal's path, whose entry
        // admits under the snapshot (routing, never a refusal: no `fall`)
        if !a.roads.is_empty() {
            snapmgr::PushActiveSnapshot(&snap)?;
            let hold = a.roads.hold();
            snapmgr::PopActiveSnapshot()?;
            if !hold {
                snapmgr::UnregisterSnapshot(Some(&snap));
                return Ok(None);
            }
        }
        // D761: the relations the statement's ExecutorStart opens, held until the run is over (before its key expressions);
        // D2578: its initial pruning decided there once, under the snapshot (ExecDoInitialPruning's, after PortalStart's)
        let _held = match a.graph.as_ref().map_or(Ok(held::Held::default()), |g| held::take(mcx, pstmt_s, &g.held, ParamListHandle::NULL, Some(&snap))) {
            Ok(h) => h,
            Err(e) => {
                snapmgr::UnregisterSnapshot(Some(&snap));
                return Err(e);
            }
        };
        let nw = key_words(a);
        // pglfast C7: the key words on this frame when they fit (an OLTP statement's few), on the heap past that: no
        // allocation per statement
        let (mut stack_p, mut stack_n) = ([0u64; KEY_WORDS_ON_FRAME], [0u8; KEY_WORDS_ON_FRAME]);
        let mut heap = (nw > KEY_WORDS_ON_FRAME).then(|| {
            let o = gravity_driver::stmt_context().map(|(o, _)| o);
            let mut p = memledger::ChargedVec::<u64>::new(o, memledger::Kind::Region);
            let mut n = memledger::ChargedVec::<u8>::new(o, memledger::Kind::Region);
            p.with_vec(|v| { v.reserve_exact(nw); v.resize(nw, 0); }); // alloc: Exec charged o
            n.with_vec(|v| { v.reserve_exact(nw); v.resize(nw, 0); }); // alloc: Exec charged o
            (p, n)
        });
        let (kparams, knulls): (&mut [u64], &mut [u8]) = match heap.as_mut() {
            Some((p, n)) => (&mut p[..], &mut n[..]),
            None => (&mut stack_p[..nw], &mut stack_n[..nw]),
        };
        let exprs = !a.key_exprs.exprs.is_empty();
        if exprs {
            snapmgr::PushActiveSnapshot(&snap)?;
        }
        let keyed = eval_keys(a, ParamListHandle::NULL, kparams, knulls, mcx, true);
        if exprs {
            snapmgr::PopActiveSnapshot()?;
        }
        if !keyed? {
            snapmgr::UnregisterSnapshot(Some(&snap));
            return Err(fall("simple: key params"));
        }
        let cid = output_cid(a)?;
        crate::phases::mark("snapshot");
        let cmd = a.cmd;
        let nk = a.nkparams;
        // Simple protocol: the server sends RowDescription before the rows (PostgreSQL's DrPrinttup startup does); extended Describe is separate.
        // v5 errorhold: a licensed SELECT keeps its RowDescription until its first row, so its plan-order run may replace it
        let rechecks = (remote || debug) && rechecks(a, pstmt_s);
        if emits_rows(cmd, a.out_natts) && remote {
            wire.prepare(&a.program, None)?;
            if held {
                wire.hold();
            }
            if rechecks {
                wire.defer_rowdesc();
            }
            wire.send_rowdesc()?;
        }
        crate::phases::mark("rowdesc");
        // D2573: the PL state the run starts with, read where an error may go on: an error that left any behind never
        // re-runs in plan order, it is the statement's
        let start = PlState::now();
        let pl = rechecks.then_some(start);
        // D3134: standard_ExecutorRun's sendTuples (a SELECT, or a write with RETURNING) starts the receiver before any node runs
        let send = debug && (pstmt_s.commandType == CmdType::CMD_SELECT || pstmt_s.hasReturning);
        if send {
            if let (true, DestReceiver::DebugTup(dr)) = (rechecks, &mut *dest) {
                dr.hold_header();
            }
            dest.startup(pstmt_s.commandType as i32, &desc_of(mcx, a))?;
        }
        // serial census M0: under PGRUST_GRAVITY_REPORT the run's pipelines are recorded (driver entry/record.rs, closed
        // right after the run, an error's included) for the served line's `pipes=` / `lead_ms=`
        let token = b.reporting().then(|| b.entry.record_open(&a.program));
        // C5: the revoke counters around the run, for its report line (process-wide: one session's statement in a test)
        let revoked0 = b.reporting().then(gravity_driver::mem::grow::revoke_counts);
        let loads0 = b.reporting().then(gravity_hosts::ops::scan::pgrc::residency::load_counts);
        let reads0 = b.reporting().then(gravity_driver::mem::spill::read_counts);
        let uncapped0 = b.reporting().then(gravity_driver::mem::grow::uncapped_at);
        let named0 = b.reporting().then(gravity_driver::mem::grow::named_at);
        let res = drive(mcx, &mut b.entry, &mut b.plan_scratch, a, None, &snap, &kparams[..nk], &knulls[..nk], cid, cmd, dest, Some(&mut *wire), None);
        let runs = token.map(|t| b.entry.record_close(t));
        // C10: one line when the run's spilling breakers grew or pinned past their grant on the uncapped road (an erroring
        // run's too): `unnamed` ones took a road no lowering named (a bug key), `named` ones a SPILL-DEBT row's, by row
        // (NR-1: `named=<row>:<n>,..`, 0 for none). Written after the statement's own line (`note_served_at`; an error's, a
        // recheck's), whose `fires=` it must not take
        let uncapped = uncapped0.map(gravity_driver::mem::grow::uncapped_since).filter(|u| u[0] + u[1] > 0).map(|[unnamed, _, slot, past]| {
            let named = named0.map_or_else(|| "0".to_string(), gravity_driver::mem::grow::named_since); // alloc: Cold
            format!("gravity: uncapped {} unnamed={unnamed} named={named} last_slot={slot} last_past={past}", a.shape) // alloc: Cold
        });
        // D4131: a run that completes may have dropped an error held inside user code (a PL/pgSQL function evaluated
        // ahead of PostgreSQL's demand, on a row PostgreSQL never reaches): what that callee left (its SPI connection) is
        // undone, as the portal's fetch undoes it (admit/entry/run.rs, D2126), so it neither meets a later caller nor warns
        // at COMMIT ("transaction left non-empty SPI stack")
        if res.is_ok() && !start.kept() {
            start.restore();
        }
        let kept = res.is_err() && pl.is_some_and(PlState::kept);
        let unsent = match &*dest {
            DestReceiver::DebugTup(dr) if debug => dr.header_held(),
            _ => wire.unsent(),
        };
        let recheck = kept && rechecks && unsent && res.as_ref().is_err_and(|e| reruns(e));
        if recheck {
            again = snapmgr::RegisterSnapshot(Some(&snap))?;
        }
        snapmgr::UnregisterSnapshot(Some(&snap));
        crate::phases::mark("drive");
        // D719: the held RowDescription and rows go after the AFTER triggers (drive's after_query::end), none on an error;
        // an error follows its RowDescription
        let res = match res {
            Err(e) if recheck && again.is_some() => {
                wire.drop_held();
                if let DestReceiver::DebugTup(dr) = &mut *dest {
                    dr.drop_header(); // D3134: the plan-order run's startup prints it
                }
                let ties = super::take_key_ties();
                if b.reporting() {
                    b.report_line(&super::recheck_line(ties, e.message()));
                }
                if let Some(u) = &uncapped {
                    b.report_line(u);
                }
                first = Some(e);
                return Ok(None);
            }
            Err(e) if debug => {
                if let DestReceiver::DebugTup(dr) = &mut *dest {
                    dr.release_header(); // D3134: PostgreSQL printed it at the startup, before the error
                }
                Err(e)
            }
            Err(e) if rechecks => wire.flush_rowdesc().and(Err(e)),
            r => r,
        };
        let ran = res.map_err(|mut e| {
            wire.drop_held();
            if let Some(why) = tag_refusal_of(&e) {
                b.note_removed(why); // D4206: the named line of a run-ahead error's refusal (runner `note_tag_refused`)
            }
            crate::glue::note_debt_refused(&mut e); // an out of memory a SPILL-DEBT row names: its server-only line
            if let Some(u) = &uncapped {
                b.report_line(u);
            }
            e
        })?;
        wire.release()?;
        if send {
            dest.shutdown()?;
        }
        let pipes = runs.map_or_else(String::new, |(r, ns, _)| crate::admit::entry::instrument::served_pipes(&r, ns));
        b.note_served_at(a.shape, ran.rows, ran.width, ran.sections, ran.claims, ran.opens, ran.granules, ran.first_touch, ran.cold, ran.p2, ran.io, ran.dict, ran.dictent, ran.elided, ran.node, via(&a), a.program.code_of(0).kind(), &pipes);
        b.note_program(&a.program);
        b.note_mechs(a.program.key.0, a.program.used);
        if let Some(u) = &uncapped {
            b.report_line(u);
        }
        if ran.spill.1 > 0 && b.reporting() {
            b.report_line(&format!("gravity: spilled {} parts={} bytes={} generations={}", a.shape, ran.spill.0, ran.spill.1, ran.spill.2)); // alloc: Cold
        }
        if let Some(r0) = revoked0 {
            let r1 = gravity_driver::mem::grow::revoke_counts();
            if r1 != r0 {
                b.report_line(&format!("gravity: revoked {} cuts={} refused={} given={}", a.shape, r1[0] - r0[0], r1[1] - r0[1], r1[2] - r0[2])); // alloc: Cold
            }
        }
        // the part cache's LOAD GATE over the statement (process counters: concurrent statements' loads count too)
        if let Some(l0) = loads0 {
            let l1 = gravity_hosts::ops::scan::pgrc::residency::load_counts();
            if l1 != l0 {
                b.report_line(&format!("gravity: load gate {} asks={} trimmed={} waits={} wait_ms={} over={}", a.shape, l1[0] - l0[0], l1[1] - l0[1], l1[2] - l0[2], l1[3] - l0[3], l1[4] - l0[4])); // alloc: Cold
            }
        }
        // D4215 / D4408: the spilled bytes read back over the statement, by path (process counters, as the load gate's)
        if let Some(r0) = reads0 {
            let r1 = gravity_driver::mem::spill::read_counts();
            if r1 != r0 {
                b.report_line(&format!("gravity: spill read {} part={} run={}", a.shape, r1[0] - r0[0], r1[1] - r0[1])); // alloc: Cold
            }
        }
        Ok(Some(gravity_wire::completion(a.tag(), ran.rows)))
    });
    if let Some(s) = kept_snap.take() {
        snapmgr::UnregisterSnapshot(Some(&s)); // the plan-order run fell through before its snapshot
    }
    let r = match r {
        Some(v) => v,
        None => Ok(None),
    };
    match again {
        // v5 errorhold: the statement once more in plan order (every licence denied); its rows or its error are the statement's
        Some(snap) => {
            core::mem::drop(kept);
            let r = plan_order(|| simple_at(pstmt, dest, Some(snap), false));
            match first.filter(|e| surfaces_over_refusal(e)) {
                Some(first) => plan_order_or_first(r, first),
                None => r,
            }
        }
        None => r,
    }
}

/// v5 errorhold: a plan-order run that met a refusal (its admission's, a grant's or a run-time one) gives the failed
/// licensed run's resource error back as the statement's (`surfaces_over_refusal`: an out-of-memory is never hidden
/// behind a refusal).
fn plan_order_or_first(r: PgResult<Option<QueryCompletion>>, mut first: Box<types_error::PgError>) -> PgResult<Option<QueryCompletion>> {
    match r {
        Err(e) if refusal_of(&e).is_some() => {
            crate::glue::note_debt_refused(&mut first); // the statement's now: an out of memory a SPILL-DEBT row names, its line
            Err(first)
        }
        r => r,
    }
}

#[cfg(test)]
mod kept_tests {
    use super::*;

    /// D842: the statement's context resets wholesale: a chunk still live at the reset (an ExprState's `resnd`) goes with it.
    #[test]
    fn the_kept_statement_context_resets_wholesale() {
        let mut k = kept_new();
        core::mem::forget(mcx::vec_with_capacity_in::<u64>(k.1.mcx(), 2).unwrap());
        assert_eq!(k.1.used(), 16);
        k.1.reset();
        assert_eq!(k.1.used(), 0);
    }

    /// D842: the kept root keeps the exact leak check: an owner leaked into it outside the wholesale child is still caught
    /// at its reset (debug builds; the check is a debug_assert).
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "context \"GravitySimpleRoot\" reset with 16 bytes still charged")]
    fn a_leak_in_the_kept_root_is_still_caught() {
        let mut k = kept_new();
        core::mem::forget(mcx::vec_with_capacity_in::<u64>(k.3.mcx(), 2).unwrap());
        k.3.reset();
    }
}
