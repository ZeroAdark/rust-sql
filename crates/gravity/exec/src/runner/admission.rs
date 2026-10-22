//! The runner's admission side: the entry's served-plan cache (take / give back / known refusals), admitting a plan
//! (graph or shape table) and the fallthrough refusal line. Split out of runner.rs (move-only, lane SPLIT 09-23).

use super::*;

/// D622: the pool admissions a statement reaches (`admit_grant`'s callers), each its program at its width: its InitPlans at
/// width 1 (`run_inits`), then a parallel program's own at the statement width (the pull's `Producers::prepare`, the
/// driver's `run_parallel`), and under the driver (`drive`: a completion run, the simple protocol's portal) each Append
/// arm as `run_program` admits it (parallel: the statement width; else width 1 unless breakerless). The pull's arms and a
/// serial pull run unadmitted.
pub fn grant_sites(a: &Admitted, drive: bool) -> Vec<(std::sync::Arc<gravity_program::Program>, u16)> {
    let mut out = Vec::new(); // alloc: Cold
    // D3865 L4: a program of one emit and on-demand InitPlans runs outside the pool (`inits_only`): nothing to reserve
    let run = |p: &std::sync::Arc<gravity_program::Program>, bitmap: bool, out: &mut Vec<(std::sync::Arc<gravity_program::Program>, u16)>| {
        if crate::inputs::knobs::grant::inits_only(p, bitmap) {
            return;
        }
        if p.pipelines.len() > 1 || p.pipelines[0].is_eligible() {
            out.push((p.clone(), crate::inputs::knobs::width::statement_width(p)));
        } else if !crate::inputs::knobs::grant::breakerless(p) {
            out.push((p.clone(), 1));
        }
    };
    if let Some(g) = a.graph.as_ref() {
        out.extend(g.inits.iter().map(|i| (i.program.clone(), 1)));
    }
    if drive {
        run(&a.program, a.bitmap_site(), &mut out);
        for x in a.graph.as_ref().map_or(&[][..], |g| &g.arms[..]) {
            run(&x.program, (0..x.program.sites.len()).any(|s| x.registry.bitmap_morsel(s).is_some()), &mut out);
        }
    } else if a.is_parallel() && !crate::inputs::knobs::grant::inits_only(&a.program, a.bitmap_site()) {
        out.push((a.program.clone(), crate::inputs::knobs::width::statement_width(&a.program)));
    }
    out
}

/// A statement's admission starts with no narrowing recorded (D3034 (f3), `knobs::grant::clear_narrow`): one that a statement
/// before it recorded and no lease reported is forgotten. D3380: a site whose floor need is past the pool's capacity is
/// not refused here any more (D622 refused it by name to the row executor): the pool runs it alone past its big lane once
/// no other statement holds bytes (`gravity_driver::Admitted::Solo`).
pub fn grant_start() {
    crate::inputs::knobs::grant::clear_narrow();
}

/// D622: a nested statement's sites' grants, reserved together without a wait (`knobs::grant::reserve_sites`); a site whose
/// grant does not fit beside its caller's takes its floor past the pool (D622 (A), never refused).
pub fn reserve_nested(a: &Admitted, drive: bool) -> NestedGrants {
    crate::inputs::knobs::grant::reserve_sites(&grant_sites(a, drive))
}

/// A bind without a plan key (custom plan, or no plan identity seam).
pub(super) fn note_unkeyed() {
    let _ = crate::with_backend(|b| b.note_fallthrough("bind: not a generic cached plan"));
}

pub(super) fn lookup_or_admit(b: &mut Backend, key: PlanKey, cplan: CachedPlanHandle, params: ParamListHandle) -> PgResult<Option<usize>> {
    if let Some(Verdict::Served(i)) = b.lookup(key) {
        // D2663, C3 S4c: a catalog row the admission read changed since (an invalidation read inside it too) re-admits
        if b.admitted(i as usize, key).is_some_and(|a| !Backend::serve_stale(a) && crate::admit::folded_hold(a, params)) && !crate::admit::regen::stale(b, i as usize, key)? {
            return Ok(Some(i as usize));
        }
    }
    let stmts = plancache::CachedPlanStmtList(cplan);
    let Some(pstmt) = stmts.first() else {
        note_refused(b);
        return Ok(None);
    };
    let pstmt: &'static PlannedStmt<'static> = super::raw::pinned_stmt(pstmt);
    // D3828: the Bind admits the whole reader (no counted pull): a program the counted-pull marks make whole-read
    // (`whole_read`) is routed to the entry, whose start reads the portal's kind and whose first run reads an Execute's
    // max_rows (admit/entry `start_counts`, `counted_partial`, `counted_whole_read`)
    let mut slot = None;
    admit_any(b, b.admit_mcx(), pstmt, params, true, false, WORDS, &mut slot)?;
    match slot {
        Some(a) => {
            b.cache.insert(Some(key), a.program.clone());
            // D1789: every slot's plan checked out: nothing filed, the portal routes the plan to the executor's entry, which
            // serves it unkeyed (a fallthrough line only, never the refusal text); a no-order-proc plan is never filed
            let unfiled = if a.graph.as_ref().is_some_and(|g| g.no_proc) { NO_PROC_UNFILED } else if Backend::catalog_stale(&a) { CATALOG_UNFILED } else { MEMO_FULL };
            let filed = b.remember_served(key, Box::new(a)); // alloc: Program
            if filed.is_none() {
                b.note_fallthrough(unfiled);
            }
            Ok(filed)
        }
        None => {
            b.remember_refusal(key);
            note_refused(b);
            Ok(None)
        }
    }
}

/// D1789: bind's fallthrough line when no memo slot can take its plan.
const MEMO_FULL: &str = "bind: every memo plan slot is checked out";

/// W8-EQUIVCLASS-SORTOP: bind's fallthrough line for a plan holding the no-order-proc raise (the memo never files it).
const NO_PROC_UNFILED: &str = "bind: a plan raising a missing order proc is never filed in the memo";

/// D2663, C3 S4c: bind's fallthrough line for an admission a catalog change overtook (the memo never files it).
const CATALOG_UNFILED: &str = "bind: a catalog row the admission read changed during it: the plan is not filed in the memo";

/// The memo key of `pstmt` under `cplan` (its place in the plan's list), `None` for a custom plan or none.
pub fn entry_key(cplan: CachedPlanHandle, pstmt: &PlannedStmt<'_>) -> Option<PlanKey> {
    if cplan.is_null() {
        return None;
    }
    let i = plancache::CachedPlanStmtList(cplan).iter().position(|s| core::ptr::eq(s as *const PlannedStmt<'_> as *const u8, pstmt as *const PlannedStmt<'_> as *const u8))?;
    glue::plan_key(cplan, i as u16)
}

/// D3865 L5: `entry_key`, or its identity key when `ident` (`PlanKey::identity`: `pstmt`, the desc's own plan, served over
/// its identity copy, admit/entry `identity_copy`).
pub fn entry_key_as(cplan: CachedPlanHandle, pstmt: &PlannedStmt<'_>, ident: bool) -> Option<PlanKey> {
    entry_key(cplan, pstmt).map(|k| if ident { k.identity() } else { k })
}

/// The entry refused `pstmt` at its admission: the next execution of the same generic plan asks the memo.
pub fn remember_entry_refusal(b: &mut Backend, cplan: CachedPlanHandle, pstmt: &PlannedStmt<'_>) {
    if let Some(key) = entry_key(cplan, pstmt) {
        b.remember_entry_refusal(key);
    }
}

/// D2908: the entry's admission of `pstmt` raised its refusal (`why`): kept for the plan's next execution when the
/// refusal is the plan's own, not this execution's: a plan with no parameters (an admission reads their values), and not a
/// resource refusal a later execution may not meet (the admission arena's address space). A nested statement's is not kept.
pub fn remember_off_refusal(b: &mut Backend, cplan: CachedPlanHandle, pstmt: &PlannedStmt<'_>, params: ParamListHandle, why: &str) {
    if admit::is_nested() || !params.is_null() && types_portal::params::num_params(params) > 0 || why.contains(crate::GRAPH_ARENA_REFUSAL) {
        return;
    }
    // PGRCSERVE G5: nor a plan whose roads are asked per execution (hosts am_rows.rs `Roads`)
    if gravity_hosts::ops::scan::am_rows::Roads::asks(pstmt).unwrap_or(true) {
        return;
    }
    if let Some(key) = entry_key(cplan, pstmt) {
        b.remember_off_refusal(key, why);
    }
}

/// D2908: the refusal `remember_off_refusal` kept for `pstmt`: the statement's error again, raised without lowering the
/// plan again. A nested statement admits again (its refusal is not kept).
pub fn held_refusal(b: &mut Backend, cplan: CachedPlanHandle, pstmt: &PlannedStmt<'_>) -> Option<Box<types_error::PgError>> {
    if admit::is_nested() {
        return None;
    }
    let key = entry_key(cplan, pstmt)?;
    b.off_refusal(key).map(refused)
}

/// C3a (entry/src/keyed.rs): the memo's served plan, taken out of the table for one execution on the seam road; `ident`:
/// the identity admission under the plan's identity key (D3865 L5).
pub fn take_served(b: &mut Backend, cplan: CachedPlanHandle, pstmt: &PlannedStmt<'_>, params: ParamListHandle, ident: bool) -> Option<(PlanKey, Box<Admitted>)> {
    let key = entry_key_as(cplan, pstmt, ident)?;
    let verdict = match b.lookup(key) {
        // D3881: a key this level never filed: the thread's pool of kept executions may hold one (a freed deep level's)
        None if crate::admit::kept::refill(b, key) => b.lookup(key),
        v => v,
    };
    let Some(Verdict::Served(i)) = verdict else { return None };
    let i = i as usize;
    if !b.plans.get(i).and_then(Option::as_ref).is_some_and(|s| s.key == key) {
        return None;
    }
    // D2663, C3 S4c: a catalog row the admission read changed since (or PS1-21 step 1b, a relation guard decides
    // otherwise on its pages now): dropped, and this execution admits again
    // D3005: or its LIMIT constants are another execution's parameter values
    if b.plans[i].as_ref().is_some_and(|s| Backend::serve_stale(&s.served) || !crate::admit::folded_hold(&s.served, params)) {
        b.drop_plans(Some(key.handle));
        return None;
    }
    // D3579: a live portal runs it: never lent (its Execute reads the slot); this execution admits again
    if b.pinned(i) {
        return None;
    }
    b.plans[i].take().map(|s| (key, s.served))
}

/// The execution's plan back into the memo, when the plan is still the one the key names and no verdict displaced it
/// (`pstmt`: the desc's own plan, an identity admission's too, D3865 L5).
pub fn give_back(b: &mut Backend, key: PlanKey, cplan: CachedPlanHandle, pstmt: &PlannedStmt<'_>, served: Box<Admitted>) {
    if entry_key_as(cplan, pstmt, key.is_identity()) != Some(key) {
        // D3346: a replanned statement's admission: dropped after the Nodes parked under its key (D2190)
        b.drop_admission(key, served);
        return;
    }
    b.file_back(key, served);
}

/// The permission check of the seam road, run on every execution (a memoized program included).
pub fn permitted(pstmt: &PlannedStmt<'_>) -> PgResult<()> {
    if let Some(relid) = gravity_hosts::perms::check_all(pstmt.permInfos.iter().filter_map(|n| n.as_rte_permission_info()))? {
        return Err(perm_denied(relid));
    }
    Ok(())
}

/// The per-execution checks standard_ExecutorStart makes before any node starts, in its order: the read-only guard
/// (ExecCheckXactReadOnly), then the relation permissions (InitPlan's ExecCheckPermissions). Every start makes them
/// here, once (admit landing 8, one prologue): the entry's (`admit::entry` prologue), the simple protocol's run and the
/// extended protocol's bind; each then checks the program's EXECUTE gates where ExecInitNode would (`check_gates`).
pub fn start_checks(pstmt: &PlannedStmt<'_>, cmd: Cmd, mcx: Mcx<'_>) -> PgResult<()> {
    admit::prevent_write_if_read_only(pstmt, cmd, mcx)?;
    permitted(pstmt)
}

/// Admissions this backend has run (tests: a memoized refusal adds none).
pub fn admissions() -> u64 {
    crate::with_existing_backend(|b| b.admissions).unwrap_or(0)
}

/// The entry's admission (docs/gravity/entry.md §2): the admission order over a plan without a plan
/// key (the crossing is bind's: the statement's own words, `key_words`, held in the desc's boxed `Serve`); the per-execution checks are the entry prologue's (admit/entry `prologue`).
/// `explain`: an EXPLAIN without ANALYZE, which runs nothing, so no pull of it counts its rows (`counted_refused`). D3828:
/// nor does a reader the start cannot see count (admit/entry `start_counts`: the protocol's unnamed portal is a whole reader
/// until an Execute's max_rows reaches its first run).
pub fn admit_unkeyed(b: &mut Backend, pstmt: &'static PlannedStmt<'static>, params: ParamListHandle, completion: bool, explain: bool) -> PgResult<Option<Admitted>> {
    prime_once();
    if !admit::offered(pstmt) {
        b.note_fallthrough(admit::NOT_OFFERED);
        return Ok(None);
    }
    let counted = !completion && !explain && crate::admit::entry::start_counts(pstmt);
    let mut slot = None;
    admit_any(b, b.admit_mcx(), pstmt, params, !completion, counted, WORDS, &mut slot)?;
    Ok(slot)
}

leader_tls::leader_tls! {
    thread_local! {
        static REFUSAL: core::cell::RefCell<String> = const { core::cell::RefCell::new(String::new()) };
    }
}

/// The last admission's refusal for the fallthrough line: the graph's typed reason.
fn refusal_line(why: &str) {
    REFUSAL.with(|r| {
        let mut r = r.borrow_mut();
        r.clear();
        r.push_str(why);
    });
}

pub(super) fn note_refused(b: &mut Backend) {
    REFUSAL.with(|r| b.note_fallthrough(&r.borrow()));
}

/// The last admission's refusal line (the entry's refusal of an admission that returned none).
pub fn last_refusal_line() -> String {
    REFUSAL.with(|r| r.borrow().to_string()) // alloc: Cold
}

/// The refusal as the statement's error: 0A000, the typed reason verbatim.
pub fn refused(why: &str) -> Box<types_error::PgError> {
    Box::new(types_error::PgError::error(format!("gravity: refused ({why})")).with_sqlstate(types_error::ERRCODE_FEATURE_NOT_SUPPORTED)) // alloc: Cold
}

/// The pgrc read counts its granule sources added (`pgrc_am::scanstats`) land here; nothing pending, nothing done.
pub fn pgrc_statement_end() {
    gravity_hosts::ops::scan::pgrc::granule_source::scanstats_statement_end();
}

/// D1509: the reason of a run-time `refused` error (a refusal the run met after its admission), `None` for any other error.
pub fn refusal_of(e: &types_error::PgError) -> Option<&str> {
    let why = e.message().strip_prefix("gravity: refused (")?.strip_suffix(')')?;
    (e.sqlstate() == types_error::ERRCODE_FEATURE_NOT_SUPPORTED).then_some(why)
}

/// D1509: a run's error as the statement sees it: the spilled build's wall named (`spilled_build_error`), on every road.
pub fn run_error(a: &Admitted, e: Box<types_error::PgError>) -> Box<types_error::PgError> {
    spilled_build_error(a, e)
}

/// A refusal after the admission: the statement's error.
pub(super) fn fall(why: &str) -> Box<types_error::PgError> {
    refused(why)
}

/// The refusal under `pgrust.gravity_admit = shape`: no graph admission runs, and no shape table is built.
const OFF: &str = "admit: the graph admission is off (pgrust.gravity_admit = shape)";

/// The admission order (exec.md "Admission from the graph"): under `pgrust.gravity_admit=graph`
/// the plan's graph (its typed refusal kept), else a refusal. `extended`: the program runs on the pull adapter;
/// `counted`: a pull of it may count its rows (the refusals of `counted_refused`). D4454: the admission is written once
/// into the caller's slot `out` (admit `graph`), `Some` exactly when this answers `Ok(true)`; a refusal or an error leaves
/// it empty.
#[allow(clippy::too_many_arguments)]
pub(super) fn admit_any(b: &mut Backend, mcx: Mcx<'static>, pstmt: &'static PlannedStmt<'static>, params: ParamListHandle, extended: bool, counted: bool, words: usize, out: &mut Option<Admitted>) -> PgResult<bool> {
    // H16 (d): a code-cache miss in this admission is tier 0 only under the rows the statement reads (knobs/tier0.rs; computed
    // at the first miss, never on a hit)
    let rows = || crate::inputs::knobs::tier0::rows_read(pstmt, mcx);
    match admit::redelivering(|| gravity_backend::with_row_estimate(&rows, || admit_either(b, mcx, pstmt, params, extended, counted, words, &mut *out))) {
        Ok(true) => {}
        r => {
            *out = None;
            return r;
        }
    }
    let a = out;
    // a write's modified columns are the statement's, not the cached program's (`INSERT (c1)` and `INSERT (c1, c2)
    // … NULL` share a plan shape): ExecConstraints' DETAIL for a user without table SELECT reads them
    if let Some(a) = a.as_mut().filter(|_| pstmt.resultRelations.len() > 1) {
        a.registry.set_modified_by_rel(crate::ops::modifytable::beside::modified_cols_by_rel(pstmt));
    }
    if let Some(a) = a.as_mut().filter(|a| a.cmd != Cmd::Select) {
        // D3623: a writing CTE's write as the program's own (a SELECT over writing CTEs) reads its own ModifyTable's
        let own = crate::ops::modifytable::beside::program_write_cols(&a.program, pstmt);
        a.registry.set_modified_cols(own.or_else(|| admit::modified_cols(pstmt)));
        if a.cmd == Cmd::Merge {
            a.registry.set_merge_ops(crate::ops::modifytable::statement::merge_ops(pstmt));
        }
    }
    Ok(true)
}

/// D2230 (setop.recursive, worklist setop P6) / D2212: a pull that may count its rows (`counted`) may stop before the union's
/// end, where the lowering runs another CTE's plan the union is read through whole before its first row (decompose
/// `IterDesc::through`); the simple protocol's run is whole. A Shape by its text, as lowerv4's walls. A union a qual or a
/// join only drops rows of (`IterDesc::thin`) is served: its pull asks for the rows each run owes (`thin_pull`).
const COUNTED_PULL: &str = "Shape: a recursive union read through another CTE's plan no breaker reads whole, under a pull that may count its rows (a cursor FETCH n, an SPI cursor or a PL/pgSQL FOR loop, a SQL function's lazy row, a bounded Execute): the lowering runs that plan's seed whole before its first row, which a non-terminating union never ends, where PG returns the first n rows at once (D2230)";

/// lowerv4 ops/setop/recursive.rs names such a union (`IterDesc::through`) in the Generation protocol's `peer`.
fn counted_pull(a: &Admitted) -> bool {
    use gravity_program::protos::{ProtoKind, NONE};
    let thin = |p: &gravity_program::Program| p.v4.as_ref().is_some_and(|d| d.protos.iter().any(|x| x.kind == ProtoKind::Generation && x.peer != NONE));
    thin(&a.program) || a.graph.as_ref().is_some_and(|g| g.arms.iter().any(|x| thin(&x.program)) || g.inits.iter().any(|x| thin(&x.program)))
}

/// D2732 (cursor form): a counted pull of a recursive union whose seed or once-run term input may raise or call a user or
/// VOLATILE function: PG returns the seed rows it read as it reads them, the lowering's first pull runs the seed whole.
const LOUD_PULL: &str = "Shape: a recursive union whose seed or once-run term input can raise or calls a user or VOLATILE function, read on the statement's root path under a pull that may count its rows (a cursor FETCH n or MOVE, an SPI cursor or a PL/pgSQL FOR loop, a SQL function's lazy row, a bounded Execute): PG reads the seed row by row and returns the rows it read, the lowering runs the seed whole at the first pull (D2732)";

/// lowerv4 ops/setop/recursive.rs marks such a union with the kernels' `GEN_ARG_LOUD` bit in the Generation protocol's `arg`.
fn seed_pull(a: &Admitted) -> bool {
    use gravity_kernels::ops::setop::entries::GEN_ARG_LOUD;
    use gravity_program::protos::ProtoKind;
    let loud = |p: &gravity_program::Program| p.v4.as_ref().is_some_and(|d| d.protos.iter().any(|x| x.kind == ProtoKind::Generation && x.arg & GEN_ARG_LOUD != 0));
    loud(&a.program) || a.graph.as_ref().is_some_and(|g| g.arms.iter().any(|x| loud(&x.program)) || g.inits.iter().any(|x| loud(&x.program)))
}

/// D2805: a counted pull of a sorted set operation on the statement's root path whose left input may raise: PG hands on each
/// left group's rows as it reads them (nodeSetOp.c setop_retrieve_sorted), the lowering reads the left whole at the first pull.
const LOUD_SET_PULL: &str = "Shape: a sorted set operation whose left input can raise, read on the statement's root path under a pull that may count its rows (a cursor FETCH n or MOVE, an SPI cursor or a PL/pgSQL FOR loop, a SQL function's lazy row, a bounded Execute): PG reads the left row by row and returns the rows it read, the lowering reads the left whole at the first pull (D2805)";

/// lowerv4 ops/setop/lower.rs marks such a set operation with the kernels' `SETOP_ARG_LOUD` bit (ops/setop/proto.rs `counted_pull_mark`).
fn set_pull(a: &Admitted) -> bool {
    let loud = |p: &gravity_program::Program| p.v4.as_ref().is_some_and(|d| d.protos.iter().any(crate::ops::setop::proto::counted_pull_mark));
    loud(&a.program) || a.graph.as_ref().is_some_and(|g| g.arms.iter().any(|x| loud(&x.program)) || g.inits.iter().any(|x| loud(&x.program)))
}

/// D2230: `a` holds a recursive union a qual, a join, a nested loop's inner, a SubPlan or an InitPlan reads with no breaker
/// over it (lowerv4 ops/setop/recursive.rs sets the kernels' `GEN_ARG_THIN` bit in its Generation protocol's `arg`): a
/// pull of a block may never fill over a non-terminating union where PostgreSQL returns the rows the run counts, so every
/// pull of it asks for the rows its run owes only (driver `PullAdapter::set_owed_cap`; admit/entry run.rs `bind_runs`;
/// a bounded Execute's own cap, runner/execute.rs), never a block ahead.
pub(crate) fn thin_pull(a: &Admitted) -> bool {
    use gravity_kernels::ops::setop::entries::GEN_ARG_THIN;
    use gravity_program::protos::ProtoKind;
    let thin = |p: &gravity_program::Program| p.v4.as_ref().is_some_and(|d| d.protos.iter().any(|x| x.kind == ProtoKind::Generation && x.arg & GEN_ARG_THIN != 0));
    thin(&a.program) || a.graph.as_ref().is_some_and(|g| g.arms.iter().any(|x| thin(&x.program)) || g.inits.iter().any(|x| thin(&x.program)))
}

/// Whether a pull that may count its rows refuses `a` (D2230, D2732, D2805): an EXPLAIN without ANALYZE is admitted past
/// these refusals for its verdict and never filed for an execution whose pull may count (admit/entry `executor_start`).
pub(crate) fn counted_refused(a: &Admitted) -> bool {
    counted_pull(a) || seed_pull(a) || set_pull(a)
}

/// D4454: a refused admission leaves `out` empty (its admission dropped where the by-value one was); an error leaves it to
/// `admit_any`, which empties it.
#[allow(clippy::too_many_arguments)]
fn admit_either(b: &mut Backend, mcx: Mcx<'static>, pstmt: &'static PlannedStmt<'static>, params: ParamListHandle, extended: bool, counted: bool, words: usize, out: &mut Option<Admitted>) -> PgResult<bool> {
    // a Foreign Scan, a Custom Scan or a foreign result relation: the statement's 0A000, never lowered nor handed off.
    // D4600 (`lean`): under the graph admission the check runs where a front hit does not answer it (admit `admit_in`:
    // on every road but a front hit), and again ahead of any refusal or error the admission ends in, so such a statement
    // raises its 0A000 first, as here, and counts no admission
    let graph_mode = admit::mode() == admit::Mode::Graph;
    let late = graph_mode && admit::lean::on();
    if !late {
        admit::removed::check(pstmt)?;
    }
    b.admissions += 1;
    let graph = if graph_mode {
        let admit_graph = || {
            let r = admit::graph(b, mcx, pstmt, params, &mut *out);
            if !matches!(r, Ok(Ok(()))) {
                *out = None;
                if late {
                    if let Err(e) = admit::removed::check(pstmt) {
                        b.admissions -= 1;
                        return Err(e);
                    }
                }
            }
            r
        };
        let (admitted, again) = or_plan_order(!extended && !counted, admit_graph)?;
        if again {
            relowered_line(b, &admitted);
        }
        let refused = match admitted.map(|()| out.as_ref().expect("graph wrote the admission")) {
            Ok(a) if a.nkparams > words => Some(admit::Refused { stage: "words", what: format!("{} words over the crossing's {words}", a.nkparams) }), // alloc: Cold
            Ok(a) if a.graph.as_ref().is_some_and(|g| !g.merge().is_empty()) && (extended || a.is_parallel() || a.graph.as_ref().is_some_and(|g| g.arms.iter().any(|x| x.program.pipelines.len() > 1 || x.program.pipelines[0].is_eligible()))) => Some(admit::Refused { stage: "protocol", what: "a MergeAppend cut on the pull adapter or over a parallel arm (the cut runs serial arms into one buffer)".into() }), // alloc: Cold
            Ok(a) if extended && a.graph.as_ref().is_some_and(|g| g.arms.iter().any(|x| x.program.pipelines.len() > 1 || x.program.pipelines[0].is_eligible())) => Some(admit::Refused { stage: "protocol", what: "an Append arm that is a multi-pipeline or eligible program on the extended protocol (the pull adapter is serial over one pipeline)".into() }), // alloc: Cold
            Ok(a) if counted && counted_pull(&a) => Some(admit::Refused { stage: "protocol", what: COUNTED_PULL.into() }), // alloc: Cold
            Ok(a) if counted && seed_pull(&a) => Some(admit::Refused { stage: "protocol", what: LOUD_PULL.into() }), // alloc: Cold
            Ok(a) if counted && set_pull(&a) => Some(admit::Refused { stage: "protocol", what: LOUD_SET_PULL.into() }), // alloc: Cold
            Ok(a) => match crate::admit::regen::check_build(a, mcx)? {
                Some(r) => Some(r),
                None => return Ok(true),
            },
            Err(r) => Some(r),
        };
        *out = None;
        refused
    } else {
        None
    };
    // no hand shape table: a statement the graph does not admit is refused
    let why = graph.map_or_else(|| OFF.to_string(), |r| r.to_string()); // alloc: Cold
    if why.contains(gravity_decompose::join::USER_HASH_FNS) {
        init_raise(pstmt, params)?;
    }
    refusal_line(&why);
    // a routed write through a foreign leaf: the same 0A000
    if let Some(e) = admit::removed::leaf_refusal(&why) {
        return Err(e);
    }
    // D2926: a counted start's partial admission refused: the whole reader starts instead (entry `run_counted`)
    if !crate::admit::entry::counted_start(pstmt) {
        admit::hand_off(&why)?;
    }
    Ok(false)
}

/// A licence never unserves a statement (D3816: a pinned builtin read as non-acting let a sorted group take the ahead
/// fold's licence; its runs.group emit then refused by D_RERUN_PULLED where the plan-order form, a streamed group, was
/// served). An admission whose graph took a licence (`effects::grants` moved across it) and whose lowering refused by Shape
/// is admitted again in plan order (every licence denied: each family's PostgreSQL form, as the errorhold re-run admits
/// it), and that admission is the statement's: served, or refused under the first refusal's reason. `whole`: the read is
/// taken to its end (`plan_order`), else the bounded plan-order run (`effects::deny_all_bounded`: a read a count may stop).
/// Answers whether it lowered again (`Backend::relowered`). Every other admission reads the counter twice, nothing more;
/// the plan-order program is never filed under its content key (`admit_in`), so each admission of such a statement lowers
/// twice.
pub fn or_plan_order<T>(whole: bool, mut f: impl FnMut() -> PgResult<Result<T, admit::Refused>>) -> PgResult<(Result<T, admit::Refused>, bool)> {
    let (g0, _) = (gravity_decompose::effects::grants(), gravity_decompose::effects::take_acted());
    match f()? {
        Err(why) if relowers(&why, gravity_decompose::effects::grants() != g0, gravity_decompose::effects::take_acted()) => {
            let again = if whole { plan_order(&mut f) } else { gravity_decompose::effects::deny_all_bounded(&mut f) }?;
            Ok((again.map_err(|_| why), true))
        }
        r => Ok((r, false)),
    }
}

/// `or_plan_order`'s test: a refusal of the lowering by Shape (lowerv4 `Reason::Shape`, its seam text `<module>: Shape: …`),
/// outside the plan-order run, of a graph that took a licence (`granted`) in a statement that does not act (`acted`:
/// `effects::take_acted`; a statement that acts has no plan-order form, D3540).
pub fn relowers(why: &admit::Refused, granted: bool, acted: bool) -> bool {
    granted && !acted && !in_plan_order() && why.stage == "lower" && why.what.split_once(": ").is_some_and(|(_, w)| w.starts_with("Shape: "))
}

/// The admissions `or_plan_order` lowered again in plan order on this backend (how often a licensed form loses to its
/// lowering).
pub fn relowered() -> u64 {
    crate::with_existing_backend(|b| b.relowered).unwrap_or(0)
}

#[cold]
fn relowered_line<T>(b: &mut Backend, admitted: &Result<T, admit::Refused>) {
    b.relowered += 1;
    if b.reporting() {
        let line = match admitted {
            Ok(_) => "gravity: licence re-lowered in plan order: served".to_string(), // alloc: Cold
            Err(why) => format!("gravity: licence re-lowered in plan order: refused ({why})"), // alloc: Cold
        };
        b.report_line(&line);
    }
}

/// D2036: a hash join the graph refused for a hash function its facts do not know (decompose join.rs `USER_HASH_FNS`).
/// Where its operator has none (the family's FUNCTION 1 dropped, which PostgreSQL's plancache does not follow), PostgreSQL's
/// ExecInitHashJoin raises XX000 "could not find hash function for hash operator" at every execution, EXPLAIN's included,
/// before any row: the statement raises it here, in ExecutorStart's order (initial pruning, then the inits' lookups over
/// the arms it keeps: `start_gates::init_lookups`), never refused nor handed off. `Ok` when every lookup finds its proc
/// (a VOLATILE hash function keeps the refusal). The refusal path only: an admitted program does no lookup.
#[cold]
fn init_raise(pstmt: &'static PlannedStmt<'static>, params: ParamListHandle) -> PgResult<()> {
    let prune = if pstmt.partPruneInfos.is_nil() { None } else { Some(crate::ops::append::prune::initial_results(pstmt, params)?) };
    super::start_gates::init_lookups(pstmt, prune.as_ref())
}
