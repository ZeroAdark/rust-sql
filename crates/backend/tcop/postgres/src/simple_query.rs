// exec_simple_query + the pg_parse/analyze/rewrite/plan wrapper family
// (postgres.c). Parser/rewriter/planner are seams (lanes in flight).
use ::elog::ereport;
use ::mcx::{Mcx, PgVec};
use ::types_core::CommandTag;
use ::types_dest::CommandDest;
use ::types_error::{PgResult, ERROR, LOG};
use ::types_nodes::node_tree::Node;
use ::types_nodes::nodes_enums::CmdType;
use ::types_nodes::parsenodes::Query;
use ::types_nodes::plannodes::PlannedStmt;
use ::types_nodes::rawnodes::RawStmt;
use ::types_nodes::NodeTag;
use ::types_portal::{
    CachedPlanHandle, ParamListHandle, QueryCompletion, QueryEnvHandle, CURSOR_OPT_PARALLEL_OK,
    FETCH_ALL,
};

use crate::{check_for_interrupts, loc, set_xact_started, xact_started, ResetUsage, ShowUsage};

fn log_parser_stats() -> bool {
    guc_tables::backing::log_parser_stats()
}
fn log_planner_stats() -> bool {
    guc_tables::backing::log_planner_stats()
}

fn log_statement_stats() -> bool {
    guc_tables::backing::log_statement_stats()
}

// postgres.c pg_rewrite_query (:803 parse, :870 rewritten) / pg_plan_query
// (:954): `if (Debug_print_*) elog_node_display(LOG, title, tree,
// Debug_pretty_print)`.
fn debug_print_tree<'mcx>(
    mcx: Mcx<'mcx>,
    title: &str,
    dump: PgResult<::mcx::PgString<'mcx>>,
) -> PgResult<()> {
    nodes_core::print::debug_print_tree(
        mcx,
        title,
        dump,
        guc_tables::backing::Debug_pretty_print(),
    )
}

pub fn pg_parse_query<'mcx>(
    mcx: Mcx<'mcx>,
    query_string: &str,
) -> PgResult<PgVec<'mcx, RawStmt<'mcx>>> {
    if log_parser_stats() {
        ResetUsage();
    }

    let raw_parsetree_list =
        parser_seams::raw_parser::call(mcx, query_string, parser_seams::RawParseMode::RAW_PARSE_DEFAULT)?;

    if log_parser_stats() {
        ShowUsage("PARSER STATISTICS")?;
    }

    Ok(raw_parsetree_list)
}

pub fn pg_analyze_and_rewrite_fixedparams<'a, 'mcx>(
    mcx: Mcx<'mcx>,
    parsetree: &'a RawStmt<'mcx>,
    query_string: &'a str,
    param_types: &'a [types_core::Oid],
    query_env: QueryEnvHandle,
) -> PgResult<PgVec<'mcx, Query<'mcx>>> {
    if log_parser_stats() {
        ResetUsage();
    }

    let query = analyze_seams::parse_analyze_fixedparams::call(
        mcx,
        parsetree,
        query_string,
        param_types,
        query_env,
    )?;

    if log_parser_stats() {
        ShowUsage("PARSE ANALYSIS STATISTICS")?;
    }

    pg_rewrite_query(mcx, query)
}

pub fn pg_rewrite_query<'mcx>(mcx: Mcx<'mcx>, query: Query<'mcx>) -> PgResult<PgVec<'mcx, Query<'mcx>>> {
    // postgres.c:803: debug_print_parse dumps the analyzed Query here (the
    // raw parse tree is never dumped in a stock build).
    if guc_tables::backing::Debug_print_parse() {
        debug_print_tree(mcx, "parse tree", outfuncs::queryToStringWithLocations(mcx, &query))?;
    }

    if log_parser_stats() {
        ResetUsage();
    }

    let querytree_list = if query.commandType == CmdType::CMD_UTILITY {
        let mut v = PgVec::new_in(mcx);
        v.try_reserve_exact(1).map_err(|_| mcx.oom(1))?;
        v.push(query);
        v
    } else {
        rewrite_handler_seams::query_rewrite::call(mcx, query)?
    };

    if log_parser_stats() {
        ShowUsage("REWRITER STATISTICS")?;
    }

    if guc_tables::backing::Debug_print_rewritten() {
        debug_print_tree(
            mcx,
            "rewritten parse tree",
            outfuncs::queryListToStringWithLocations(mcx, &querytree_list),
        )?;
    }

    Ok(querytree_list)
}

pub fn pg_plan_query<'mcx>(
    mcx: Mcx<'mcx>,
    querytree: &'mcx mut Query<'mcx>,
    query_string: &str,
    cursor_options: i32,
    bound_params: ParamListHandle,
) -> PgResult<Option<PlannedStmt<'mcx>>> {
    if querytree.commandType == CmdType::CMD_UTILITY {
        return Ok(None);
    }

    debug_assert!(snapmgr::ActiveSnapshotSet());

    if log_planner_stats() {
        ResetUsage();
    }

    let plan =
        planner_seams::planner::call(mcx, querytree, query_string, cursor_options, bound_params)?;

    if log_planner_stats() {
        ShowUsage("PLANNER STATISTICS")?;
    }

    if guc_tables::backing::Debug_print_plan() {
        debug_print_tree(mcx, "plan", outfuncs::plannedStmtToStringWithLocations(mcx, &plan))?;
    }

    Ok(Some(plan))
}

pub fn pg_plan_queries<'mcx>(
    mcx: Mcx<'mcx>,
    querytrees: PgVec<'mcx, Query<'mcx>>,
    query_string: &str,
    cursor_options: i32,
    bound_params: ParamListHandle,
) -> PgResult<PgVec<'mcx, PlannedStmt<'mcx>>> {
    let mut stmt_list: PgVec<'mcx, PlannedStmt<'mcx>> = PgVec::new_in(mcx);
    stmt_list
        .try_reserve_exact(querytrees.len())
        .map_err(|_| mcx.oom(querytrees.len()))?;

    // The analyzed Queries already live in arena memory; plan each in place
    // (C mutates the Query and shares the pointer) instead of moving it
    // through the call chain and copying it back into the arena at seal.
    let querytrees: &'mcx mut [Query<'mcx>] = querytrees.leak();
    for query in querytrees {
        if query.commandType == CmdType::CMD_UTILITY {
            stmt_list.push(PlannedStmt {
                commandType: CmdType::CMD_UTILITY,
                canSetTag: query.canSetTag,
                utilityStmt: query.utilityStmt,
                stmt_location: query.stmt_location,
                stmt_len: query.stmt_len,
                queryId: types_nodes::SyncCell::new(query.queryId),
                ..PlannedStmt::default()
            });
        } else {
            let stmt =
                pg_plan_query(mcx, query, query_string, cursor_options, bound_params)?
                    .expect("pg_plan_query returned None for a non-utility query");
            stmt_list.push(stmt);
        }
    }

    Ok(stmt_list)
}

// Crash-test injection worker (env-gated caller). Signals a TARGET vpid to
// die abruptly: `quit` = SIGQUIT quickdie (WARNING + 57P02 to peers, no proc
// cleanup), `kill` = SIGKILL-equivalent (no message, no cleanup). Superuser-
// gated, mirroring pg_signal_backend's contract. The delivery is async — the
// target dies at its next interrupt point and the postmaster runs the crash-
// restart cycle; this session returns a clean empty-query response first.
fn crash_backend_injection(args: &str) -> PgResult<()> {
    if !superuser_seams::superuser::call()? {
        return Err(ereport(ERROR)
            .errmsg("crash-backend injection requires superuser")
            .into_error()
            .into());
    }

    let mut parts = args.split_whitespace();
    let pid: i32 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| {
            ereport(ERROR)
                .errmsg("crash backend: expected 'pgrust: crash backend <vpid> quit|kill'")
                .into_error()
        })?;
    let mode = parts.next().unwrap_or("quit");

    let rc = match mode {
        "quit" => procsignal::SendThreadSignal(pid, crate::SIGQUIT),
        "kill" => procsignal::SendThreadKill(pid),
        other => {
            return Err(ereport(ERROR)
                .errmsg(format!("crash backend: unknown mode {other:?} (want quit|kill)"))
                .into_error()
                .into());
        }
    };
    if rc != 0 {
        return Err(ereport(ERROR)
            .errmsg(format!("PID {pid} is not a PostgreSQL backend process"))
            .into_error()
            .into());
    }

    tcop_dest::NullCommand(elog::config::where_to_send_output())
}

// D3.0 memory-census debug command (`pgrust: memctx`, superuser-gated like
// the crash injections): dump the CURRENT backend thread's memory estate as
// NOTICE rows — the mcx context forest (what pg_backend_memory_contexts
// shows) plus the cache-entry counts and global-heap bytes the context view
// cannot see (relcache entries are Rc allocations on the global allocator;
// catcache/plancache registry counts). notes/connection-scaling-measurements.md.
fn memctx_dump() -> PgResult<()> {
    use types_error::NOTICE;
    if !superuser_seams::superuser::call()? {
        return Err(ereport(ERROR)
            .errmsg("pgrust: memctx requires superuser")
            .into_error()
            .into());
    }
    let say = |msg: String| -> PgResult<()> {
        ereport(NOTICE).errmsg(msg).finish(loc(0, "memctx_dump")).map(|_| ())
    };

    // Context forest (chunk-exact charges; block slack not included).
    fn walk(t: &::mcx::TreeStats, depth: usize, out: &mut Vec<String>, tot: &mut (usize, usize)) {
        let total = t.arena_footprint.max(t.used);
        tot.0 += total;
        tot.1 += t.used;
        out.push(format!(
            "{:indent$}{}: total={} used={} blocks={} [{}]",
            "", t.name, total, t.used, t.nblocks.max(1), t.kind,
            indent = depth * 2
        ));
        for c in &t.children {
            walk(c, depth + 1, out, tot);
        }
    }
    let mut lines = Vec::new();
    let mut tot = (0usize, 0usize);
    for root in mcxt_stats::backend_context_forest() {
        walk(&root, 0, &mut lines, &mut tot);
    }
    say(format!("memctx: context forest ({} roots): grand total={} used={}",
        lines.len(), tot.0, tot.1))?;
    for l in lines {
        say(l)?;
    }

    let rc = relcache::RelationCacheCensus();
    say(format!(
        "memctx: relcache entries={} nailed={} invalid={} stale_lineages={} side_cache_entries={} est_global_heap_bytes={}",
        rc.entries, rc.nailed, rc.invalid, rc.stale_lineages, rc.side_cache_entries,
        rc.est_heap_bytes
    ))?;
    let (mirrors, mirrors_live) = relcache::l2core::MirrorCensus();
    say(format!(
        "memctx: relcache l2 shells: tupdesc_mirrors={mirrors} live={mirrors_live}"
    ))?;

    let (ch_ntup, rows) = catcache::CatCacheCensus();
    let payload: usize = rows.iter().map(|r| r.payload_bytes).sum();
    let arena: usize = rows.iter().map(|r| r.arena_bytes).sum();
    let shared: i32 = rows.iter().map(|r| r.shared).sum();
    say(format!(
        "memctx: catcache total_tuples={ch_ntup} shared_tuples={shared} private_payload_bytes={payload} arena_bytes={arena}"
    ))?;
    let mut rows = rows;
    rows.sort_by_key(|r| core::cmp::Reverse(r.payload_bytes + r.arena_bytes));
    for r in rows.iter().take(15) {
        say(format!(
            "memctx:   cache id={} rel={} ntup={} nlist={} pinned={} shared={} payload={} arena={}",
            r.id,
            r.relname.as_deref().unwrap_or("?"),
            r.ntup, r.nlist, r.pinned, r.shared, r.payload_bytes, r.arena_bytes
        ))?;
    }

    let (nsrc, nplan, nsaved) = plancache::PlanCacheCensus();
    say(format!("memctx: plancache sources={nsrc} plans={nplan} saved={nsaved}"))?;

    // memgrowth-discriminator (suspect-A census): THIS backend thread's raw-
    // Rust slot registries (querydesc/stmt_list/queryenv/tuplestore/resowner)
    // — Vec slabs on the global allocator, invisible to the context forest
    // above. Same formatter the log-memory-contexts interrupt dump appends.
    if let Some(line) = mcxt_stats::slot_census_line() {
        say(format!("memctx: {line}"))?;
    }

    // D3.2: the process-global L2 (counted once per process, not per backend).
    let l2 = ::l2cache::stats();
    say(format!(
        "memctx: l2cache entries={} bytes={} hits={} misses={} inserts={} herd_waits={} (process-wide)",
        l2.entries, l2.bytes, l2.hits, l2.misses, l2.inserts, l2.herd_waits
    ))?;
    say(format!(
        "memctx: process-wide context blocks={} bytes",
        ::mcx::global_footprint::bytes()
    ))?;

    // Wave-4 floor census: this backend thread's stack — region size, the
    // dirtied high-water (the footprint the bootstrap dip left behind), and
    // current residency (mincore scan; stack_mem module doc).
    match crate::stack_mem::stack_census() {
        Some(s) => say(format!(
            "memctx: stack region_kb={} depth_now_kb={} resident_kb={} dirty_kb={} high_water_kb={}",
            s.region_bytes / 1024,
            s.depth_now / 1024,
            s.resident_bytes / 1024,
            s.dirty_bytes / 1024,
            s.high_water / 1024
        ))?,
        None => say("memctx: stack census unavailable on this platform".to_string())?,
    }

    // Wave-4 floor census: allocator-committed bytes (process-wide) beside
    // phys_footprint separates mimalloc retention from stacks/VM regions.
    if let Some(a) = memruntime::read_allocator_stats() {
        say(format!(
            "memctx: allocator current_rss={} current_commit={} (process-wide)",
            a.current_rss, a.current_commit
        ))?;
    }

    tcop_dest::NullCommand(elog::config::where_to_send_output())
}

// GL-MEMWATCH-1: the deliberate context hog behind the developer GUC
// pgrust.memory_watchdog_test_hog. Allocates (and touches — RSS must grow)
// `mb` one-MB slabs into a session-lifetime "WatchdogTestHog" context and
// leaks them there: ACCOUNTED growth, the incident shape. Freed only at
// session teardown (the session_root Roots phase). Never reachable at the
// default 0.
#[cold]
#[inline(never)]
fn watchdog_test_hog(mb: usize) {
    use std::cell::Cell;
    thread_local! {
        static HOG_CTX: Cell<Option<&'static ::mcx::MemoryContext>> = const { Cell::new(None) };
    }
    let ctx = HOG_CTX.with(|c| match c.get() {
        Some(ctx) => ctx,
        None => {
            let ctx: &'static ::mcx::MemoryContext = ::mcx::session_root("WatchdogTestHog");
            c.set(Some(ctx));
            // D4682: the teardown retires this root; clear the slot first so a reused thread starts empty
            ::mcx::register_session_cleanup(Box::new(|| {
                let _ = HOG_CTX.try_with(|c| c.set(None));
            }));
            ctx
        }
    });
    for _ in 0..mb {
        let v: PgVec<'_, u8> = ::mcx::vec_from_elem_in(ctx.mcx(), 0xA5u8, 1 << 20);
        let _ = v.leak();
    }
}

const HEAP_HOG_CHUNK: usize = 1 << 20;

// A negative pgrust.memory_watchdog_test_hog: grow that many MB outside every
// memory context (plain heap, which the governor cannot refuse), one MB at a
// time with interrupt checks, freed when the statement ends or fails — the
// shape only the watchdog's victim cancel stops.
#[cold]
#[inline(never)]
fn watchdog_test_heap_hog(mb: usize) -> PgResult<()> {
    let mut held: Vec<Vec<u8>> = Vec::new();
    for _ in 0..mb {
        held.push(vec![0xA5u8; HEAP_HOG_CHUNK]);
        check_for_interrupts()?;
    }
    Ok(())
}

// `pgrust: admission stats` — one NOTICE with the D1 gate and D6 queue
// counters, then an empty-query response. Read-only, always available.
fn admission_stats_command() -> PgResult<()> {
    let (active, waiters, total_waits, total_wait_us) = crate::admission::stats();
    let (queued, total_queued, served, timeouts, hangups) = lmgr_proc::connqueue::stats();
    ereport(types_error::NOTICE)
        .errmsg(format!(
            "admission stats: max_active_queries active={active} waiters={waiters} \
             total_waits={total_waits} total_wait_ms={} | connection_queue queued={queued} \
             total_queued={total_queued} served={served} timeouts={timeouts} hangups={hangups}",
            total_wait_us / 1000,
        ))
        .finish(loc(0, "admission_stats_command"))?;
    tcop_dest::NullCommand(elog::config::where_to_send_output())
}

// D4563: the 'pgrust: ' debug commands (crash-test injection, admission stats, the memory census), out of line and cold:
// exec_simple_query carried all of them inlined (memctx_dump alone 1.3 KB) though no ordinary statement runs them. Ok(true)
// = a command ran and its result is the statement's; Ok(false) = not a command, the statement goes on as before.
#[cold]
#[inline(never)]
fn pgrust_debug_command(query_string: &str) -> PgResult<bool> {
    // Crash-restart test injection: PANIC-class fault on demand, env-gated so
    // the surface is inert in production (notes/crash-restart-design.md).
    if query_string == "pgrust: inject panic"
        && std::env::var_os("PGRUST_CRASH_TEST").is_some()
    {
        if !superuser_seams::superuser::call()? {
            return Err(ereport(ERROR)
                .errmsg("crash-backend injection requires superuser")
                .into_error()
                .into());
        }
        ereport(types_error::PANIC)
            .errmsg("crash-restart test injection")
            .finish(loc(0, "exec_simple_query"))?;
    }

    // Crash-test injection: make a TARGET backend die as if externally
    // SIGQUIT/SIGKILL'd. TAP crash tests kill by OS pid; thread-model vpids
    // aren't OS-signalable, so the recovery rig routes pg_ctl kill here
    // (notes/crash-tap-kill-design.md). Env-gated: inert in production.
    if query_string.starts_with("pgrust: crash ") && std::env::var_os("PGRUST_CRASH_TEST").is_some() {
        if let Some(args) = query_string.strip_prefix("pgrust: crash backend ") {
            return crash_backend_injection(args).map(|()| true);
        }
        if query_string == "pgrust: crash park insert" {
            if !superuser_seams::superuser::call()? {
                return Err(ereport(ERROR)
                    .errmsg("crash-park injection requires superuser")
                    .into_error()
                    .into());
            }
            init_small::globals::set_crash_test_park_pid(init_small::globals::MyProcPid());
            return tcop_dest::NullCommand(elog::config::where_to_send_output()).map(|()| true);
        }
    }

    // Admission-control observability (connection-scaling D1+D6): read-only
    // counter dump over the pgrust: debug command channel. Deliberately
    // ahead of the admission gate below — the diagnostic must not queue.
    if query_string == "pgrust: admission stats" {
        return admission_stats_command().map(|()| true);
    }
    // D3.0 memory-census probe: dump this backend's memory estate (contexts
    // + cache entry counts) as NOTICEs. Superuser-gated, read-only.
    // (psql sends "stmt;" — accept an optional trailing semicolon.)
    if query_string.trim().trim_end_matches(';').trim_end() == "pgrust: memctx" {
        return memctx_dump().map(|()| true);
    }
    Ok(false)
}

// D4563: log_statement's 'statement: ...' line, out of line and cold (exec_simple_query's caller tests check_log_statement).
#[cold]
#[inline(never)]
fn log_statement_line(parsetree_list: &[RawStmt<'_>], query_string: &str) -> PgResult<()> {
    let mut rep = ereport(LOG)
        .errmsg(format!("statement: {query_string}"))
        .errhidestmt(true);
    if let Some(detail) = errdetail_execute(parsetree_list)? {
        rep = rep.errdetail(detail);
    }
    rep.finish(loc(1069, "exec_simple_query"))
}

// D4563: log_duration's line (check_log_duration's 1 = duration only, 2 = with the statement), out of line and cold.
#[cold]
#[inline(never)]
fn log_duration_line(kind: i32, msec_str: &str, parsetree_list: &[RawStmt<'_>], query_string: &str) -> PgResult<()> {
    if kind == 1 {
        return ereport(LOG)
            .errmsg(format!("duration: {msec_str} ms"))
            .errhidestmt(true)
            .finish(loc(1362, "exec_simple_query"));
    }
    let mut rep = ereport(LOG)
        .errmsg(format!("duration: {msec_str} ms  statement: {query_string}"))
        .errhidestmt(true);
    if let Some(detail) = errdetail_execute(parsetree_list)? {
        rep = rep.errdetail(detail);
    }
    rep.finish(loc(1367, "exec_simple_query"))
}

/// Re-plan and retry (v0.4, FALLBACK-OPTIONS.md option (d), approved 2026-10-19): the planner settings each retry plans
/// under, in order: `(defaults, settings)`, `defaults` first resetting every plan-method switch the statement's session
/// may have turned off (`REPLAN_DEFAULTS`). Each one steers the planner away from one node family (or two) whose shapes
/// Gravity's lowering may refuse (a Material or Memoize over a rescanned inner, a join method's drain or nest form, a
/// parallel plan); the settings are planner costs only, so the plan's answer is the statement's whichever is chosen.
const REPLAN_SETTINGS: &[(bool, &[(&str, &str)])] = &[
    (false, &[("enable_material", "off")]),
    (false, &[("enable_memoize", "off")]),
    (false, &[("enable_hashjoin", "off")]),
    (false, &[("enable_mergejoin", "off")]),
    (false, &[("enable_nestloop", "off")]),
    (false, &[("enable_hashagg", "off")]),
    (false, &[("max_parallel_workers_per_gather", "0")]),
    (false, &[("enable_material", "off"), ("enable_memoize", "off")]),
    (false, &[("enable_indexscan", "off"), ("enable_indexonlyscan", "off"), ("enable_bitmapscan", "off")]),
    (true, &[]),
    (true, &[("enable_hashagg", "off")]),
    (true, &[("enable_material", "off"), ("enable_memoize", "off")]),
    (true, &[("enable_nestloop", "off")]),
];

/// The plan-method switches a `defaults` retry turns back on (a session's `SET enable_hashjoin = off` included).
const REPLAN_DEFAULTS: &[&str] = &[
    "enable_seqscan",
    "enable_indexscan",
    "enable_indexonlyscan",
    "enable_bitmapscan",
    "enable_tidscan",
    "enable_sort",
    "enable_incremental_sort",
    "enable_hashagg",
    "enable_material",
    "enable_memoize",
    "enable_nestloop",
    "enable_mergejoin",
    "enable_hashjoin",
];

/// The first OID of a user-made object (FirstNormalObjectId): a function, operator or domain at or past it is user code.
const FIRST_NORMAL_OBJECT_ID: u32 = 16384;
/// count(*) and count(any): the aggregates whose answer is the same whatever order their rows arrive in.
const F_COUNT_STAR: u32 = 2803;
const F_COUNT_ANY: u32 = 2147;

/// Re-plan and retry: a statement whose answer may depend on the plan that runs it, so another plan could answer what
/// PostgreSQL's plan would not, in the statement or any subquery:
/// - user code: a function, a domain, or an operator that is user-made or belongs to a user-made operator family (the
///   family's support functions are what a Sort, a hash or merge join, a group or an index walk calls), or a relation
///   with an index of a user-made operator class (a plan may walk it): their notices, side effects and errors follow the
///   calls the plan makes;
/// - an aggregate other than count (array_agg, string_agg, float sums and the like read their rows' order), a window
///   function, an ARRAY(subquery) or a DISTINCT ON whose subquery has no ORDER BY, or a LIMIT / OFFSET with no ORDER BY
///   (which rows it keeps, and in which order, is the plan's).
struct PlanDependent;

/// Operator `opno` is user-made, or a member of a user-made operator family.
fn user_operator(opno: u32) -> PgResult<bool> {
    let mut user = opno >= FIRST_NORMAL_OBJECT_ID;
    lsyscache::with_amop_members(opno, |m| user |= m.amopfamily >= FIRST_NORMAL_OBJECT_ID)?;
    Ok(user)
}

/// Relation `relid` has an index with a user-made operator class on a key column.
fn user_index(relid: u32) -> PgResult<bool> {
    let scratch = ::mcx::MemoryContext::new("replan index check");
    for index in relcache::RelationGetIndexList(scratch.mcx(), relid)?.iter() {
        let Some(ix) = syscache_seams::lookup_pg_index_ls_shape::call(*index)? else { continue };
        for col in 0..ix.indnkeyatts.max(0) as i32 {
            if syscache_seams::pg_index_indclass_element::call(*index, col)?.is_some_and(|c| c >= FIRST_NORMAL_OBJECT_ID) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

impl<'mcx> nodes_core::NodeWalker<'mcx> for PlanDependent {
    fn visit(&mut self, node: Node<'mcx>) -> PgResult<bool> {
        if nodes_core::check_functions_in_node(node, &mut |f| Ok(f >= FIRST_NORMAL_OBJECT_ID))? {
            return Ok(true);
        }
        let dependent = match node.node_tag() {
            NodeTag::T_Aggref => !matches!(node.as_aggref().map(|a| a.aggfnoid), Some(F_COUNT_STAR | F_COUNT_ANY)),
            NodeTag::T_WindowFunc => true,
            NodeTag::T_CoerceToDomain => node.as_coerce_to_domain().is_some_and(|c| c.resulttype >= FIRST_NORMAL_OBJECT_ID),
            NodeTag::T_OpExpr => user_operator(node.as_op_expr().expect("T_OpExpr").opno)?,
            NodeTag::T_DistinctExpr => user_operator(node.as_distinct_expr().expect("T_DistinctExpr").opno)?,
            NodeTag::T_NullIfExpr => user_operator(node.as_null_if_expr().expect("T_NullIfExpr").opno)?,
            NodeTag::T_ScalarArrayOpExpr => user_operator(node.as_scalar_array_op_expr().expect("T_ScalarArrayOpExpr").opno)?,
            NodeTag::T_RowCompareExpr => {
                let r = node.as_row_compare_expr().expect("T_RowCompareExpr");
                r.opfamilies.iter().any(|f| f >= FIRST_NORMAL_OBJECT_ID) || r.opnos.iter().try_fold(false, |u, o| Ok::<_, Box<types_error::PgError>>(u || user_operator(o)?))?
            }
            NodeTag::T_SubLink => node.as_sub_link().is_some_and(|s| {
                s.subLinkType == types_nodes::primnodes::SubLinkType::ARRAY_SUBLINK
                    && s.subselect.as_query().is_some_and(|q| q.sortClause.is_nil())
            }),
            _ => false,
        };
        if dependent {
            return Ok(true);
        }
        match node.node_tag() {
            NodeTag::T_Query => self.visit_query_ref(node.as_query().expect("T_Query")),
            _ => nodes_core::expression_tree_walker(node, self),
        }
    }

    fn visit_query_ref(&mut self, q: &'mcx Query<'mcx>) -> PgResult<bool> {
        let cut = q.limitCount.is_some() || q.limitOffset.is_some();
        if q.hasWindowFuncs || ((cut || q.hasDistinctOn) && q.sortClause.is_nil()) {
            return Ok(true);
        }
        for l in [&q.sortClause, &q.groupClause, &q.distinctClause] {
            for c in l.iter().filter_map(|n| n.as_sort_group_clause()) {
                if user_operator(c.sortop)? || user_operator(c.eqop)? {
                    return Ok(true);
                }
            }
        }
        for rte in q.rtable.iter().filter_map(|n| n.as_range_tbl_entry()) {
            if rte.rtekind == types_nodes::parsenodes::RTEKind::RTE_RELATION && user_index(rte.relid)? {
                return Ok(true);
            }
        }
        nodes_core::query_tree_walker(q, self, 0)
    }
}

/// Re-plan and retry: query `q` sends at most one row whatever plan runs it: a plain aggregate (no GROUP BY, grouping
/// sets, set-returning target or set operation), so no plan choice orders its rows.
fn one_row(q: &Query<'_>) -> bool {
    q.hasAggs && q.groupClause.is_nil() && q.groupingSets.is_nil() && !q.hasTargetSRFs && q.setOperations.is_none()
}

/// Re-plan and retry: plan `p` is one the road may plan again (a plain SELECT: no row lock, no data-modifying WITH), with
/// Gravity's seams installed.
fn replan_eligible(p: &PlannedStmt<'_>) -> bool {
    p.commandType == CmdType::CMD_SELECT
        && !p.hasModifyingCTE
        && p.rowMarks.is_nil()
        && gravity_entry_seams::replan_arm::is_installed()
        && gravity_entry_seams::replan_end::is_installed()
        && gravity_entry_seams::simple_refused_at_admission::is_installed()
}

/// Re-plan and retry: statement `idx` of `query_string`, whose plan Gravity refused at its admission (nothing
/// ran, nothing reached the client: `first` is that refusal), is planned again under each of `REPLAN_SETTINGS` in turn
/// and the first plan Gravity admits runs. Only a read-only statement whose answer does not depend on its plan takes
/// this road: a plain SELECT (no row lock, no data-modifying WITH) that sends at most one row (`one_row`), calls no
/// VOLATILE function and holds nothing `PlanDependent` names; any other, a statement no setting gets admitted, or one whose other plan raises at run time,
/// ends in `first`. The statement is parsed and analyzed again from its text (parse
/// analysis scribbles on its raw tree, and the planner on its Query), under the statement's own snapshot rules; the
/// settings hold only while it plans (a GUC nest level, rolled back before the run).
fn replan_refused<'mcx>(
    mcx: Mcx<'mcx>,
    query_string: &str,
    idx: usize,
    receiver: &mut tcop_dest::DestReceiver<'_>,
    first: Box<types_error::PgError>,
) -> PgResult<Option<QueryCompletion>> {
    for &(defaults, settings) in REPLAN_SETTINGS {
        let plan = match replan_under(mcx, query_string, idx, defaults, settings) {
            Ok(Some(plan)) => plan,
            // a statement the road does not take (its text no longer one read-only statement), or a planning error the first
            // plan did not meet: the first refusal is the statement's
            Ok(None) | Err(_) => break,
        };
        gravity_entry_seams::replan_arm::call();
        match gravity_entry_seams::simple::call(&plan, receiver) {
            Err(_) if gravity_entry_seams::simple_refused_at_admission::call() => continue,
            // not offered to Gravity after all (the portal's road would run the first plan), or another plan's run raised: an
            // error PostgreSQL's plan may not meet (or may meet elsewhere), so the first refusal is the statement's
            Ok(None) | Err(_) => break,
            r => {
                // another plan was served: the first plan's refusal line goes
                gravity_entry_seams::replan_end::call(true);
                if gravity_entry_seams::note_replanned::is_installed() {
                    let mut named: Vec<String> = settings.iter().map(|(n, v)| format!("{n}={v}")).collect(); // alloc: Cold
                    if defaults {
                        named.insert(0, "plan methods at their defaults".to_string()); // alloc: Cold
                    }
                    gravity_entry_seams::note_replanned::call(&named.join(" "));
                }
                return r;
            }
        }
    }
    gravity_entry_seams::replan_end::call(false);
    Err(first)
}

/// Re-plan and retry: statement `idx` of `query_string` parsed, analyzed, rewritten and planned under `settings` (after
/// `REPLAN_DEFAULTS` back on when `defaults`); `None`
/// when it is no longer one read-only SELECT that calls no VOLATILE function.
fn replan_under<'mcx>(
    mcx: Mcx<'mcx>,
    query_string: &str,
    idx: usize,
    defaults: bool,
    settings: &[(&str, &str)],
) -> PgResult<Option<PlannedStmt<'mcx>>> {
    let raw = pg_parse_query(mcx, query_string)?;
    let Some(parsetree) = raw.get(idx) else { return Ok(None) };
    let snapshot_set = if analyze_seams::analyze_requires_snapshot::call(parsetree) {
        let snap = snapmgr::GetTransactionSnapshot()?;
        snapmgr::PushActiveSnapshot(&snap)?;
        true
    } else {
        false
    };
    let nest = guc::NewGUCNestLevel();
    let planned = (|| -> PgResult<Option<PlannedStmt<'mcx>>> {
        let on = REPLAN_DEFAULTS.iter().filter(|_| defaults).map(|&n| (n, "on"));
        for (name, value) in on.chain(settings.iter().copied()) {
            guc::set_config_option(
                name,
                Some(value),
                types_guc::PGC_USERSET,
                types_guc::PGC_S_SESSION,
                guc::GUC_ACTION_SAVE,
                true,
                ERROR,
                false,
            )?;
        }
        let queries = pg_analyze_and_rewrite_fixedparams(mcx, parsetree, query_string, &[], QueryEnvHandle::NULL)?;
        if queries.len() != 1 {
            return Ok(None);
        }
        let query = queries.into_iter().next().expect("one query");
        if query.commandType != CmdType::CMD_SELECT || query.utilityStmt.is_some() || query.hasModifyingCTE || !query.rowMarks.is_nil() {
            return Ok(None);
        }
        // the order of the rows sent is the plan's (a hash join and a merge join emit one result in two orders; an ORDER
        // BY's ties keep their input order, and equal keys may differ in bytes: numeric 1.0 and 1.00): only a statement
        // that sends at most one row, a plain aggregate's (`PlanDependent` leaves count only), sends PostgreSQL's bytes
        // whatever plan runs it
        if !one_row(&query) {
            return Ok(None);
        }
        let node = Node::mk(mcx, query)?;
        if clauses::contain_volatile_functions(node)? || nodes_core::NodeWalker::visit(&mut PlanDependent, node)? {
            return Ok(None);
        }
        // SAFETY: a fresh node; this call holds its only access.
        let query: Query<'mcx> = unsafe { node.with_mut::<Query, _>(core::mem::take) }.expect("the analyzed query");
        let mut one = PgVec::new_in(mcx);
        one.try_reserve_exact(1).map_err(|_| mcx.oom(1))?;
        one.push(query);
        let plans = pg_plan_queries(mcx, one, query_string, CURSOR_OPT_PARALLEL_OK, ParamListHandle::NULL)?;
        Ok(plans.into_iter().next())
    })();
    guc::AtEOXact_GUC(false, nest);
    if snapshot_set {
        snapmgr::PopActiveSnapshot()?;
    }
    planned
}

// D4563: a statement Gravity failed leaves C's FAILED unnamed portal (its query defined) to the transaction's cleanup: an
// Execute of "" in the aborted block finds it and raises 25P02, never 34000 or an empty query. Out of line and cold.
#[cold]
#[inline(never)]
fn leave_failed_unnamed_portal(query_string: &str, command_tag: CommandTag) {
    if let Ok(failed) = portalmem::CreatePortal("", true, true) {
        failed.borrow_mut().visible = false;
        let _ = portalmem::PortalDefineQuery(&failed, None, query_string, command_tag, types_portal::StmtListHandle::NULL, CachedPlanHandle::NULL)
            .and_then(|()| portalmem::MarkPortalFailed(&failed));
    }
}

pub fn exec_simple_query<'mcx>(mcx: Mcx<'mcx>, query_string: &'mcx str) -> PgResult<()> {
    // D4481: the 'pgrust: ' debug commands below sit behind one prefix test, so an ordinary query no longer pays their
    // checks (a trim pass over its whole text among them). Every command's text starts with the prefix (after the
    // leading whitespace the memctx check trims), so the gate changes no answer.
    if query_string.trim_start().starts_with("pgrust: ") && pgrust_debug_command(query_string)? {
        return Ok(());
    }

    // GL-MEMWATCH-1 test instrumentation (developer GUC, default 0 = one
    // TLS load + branch): each simple query leaks N MB into a named
    // session-lifetime context — the memory watchdog e2e's deliberate hog.
    let hog = guc_tables::backing::pgrust_memory_watchdog_test_hog();
    if hog > 0 {
        watchdog_test_hog(hog as usize);
    }

    // C: `debug_query_string = query_string;` (exec_simple_query top); the
    // scope's drop is the tail `debug_query_string = NULL` plus the
    // sigsetjmp reset on the error path. current_query() reads it.
    let _debug_query = elog::debug_query_string_scope(query_string);

    let dest = elog::config::where_to_send_output();
    let save_log_statement_stats = log_statement_stats();
    let mut was_logged = false;

    backend_status_seams::pgstat_report_activity::call(
        backend_status_seams::BackendState::STATE_RUNNING,
        Some(query_string),
    );
    if hog < 0 {
        watchdog_test_heap_hog(hog.unsigned_abs() as usize)?;
    }

    // D1 max_active_queries gate (docs/design/connection-scaling.md §D1):
    // acquired for the whole message scope, AFTER activity reporting (so a
    // waiter shows active with its query and the MaxActiveQueries wait
    // event in pg_stat_activity) but BEFORE the transaction starts, any
    // snapshot is taken, or any lock acquired; the guard's drop releases
    // on every exit path (Ok, Err unwind, panic).
    let _admission = crate::admission::acquire_for_statement(|| {
        crate::admission::finishes_prepared_query(mcx, query_string)
    })?;

    if save_log_statement_stats {
        ResetUsage();
    }

    start_xact_command()?;
    crate::stmt_trace::probe("q.xact");

    drop_unnamed_stmt();


    let parsetree_list = pg_parse_query(mcx, query_string)?;
    crate::stmt_trace::probe("q.parse");

    if check_log_statement(&parsetree_list)? {
        log_statement_line(&parsetree_list, query_string)?;
        was_logged = true;
    }

    let use_implicit_block = parsetree_list.len() > 1;

    let n = parsetree_list.len();
    for (idx, parsetree) in parsetree_list.iter().enumerate() {
        let is_last = idx + 1 == n;
        let mut snapshot_set = false;

        backend_status_seams::pgstat_report_query_id::call(0, true);
        backend_status_seams::pgstat_report_plan_id::call(0, true);

        let stmt = parsetree.stmt.expect("RawStmt has a stmt");

        let command_tag: CommandTag = utility_seams::create_command_tag::call(stmt);
        let (cmdtagname, _cmdtaglen) = cmdtag::GetCommandTagNameAndLen(command_tag);

        ps_status_seams::set_ps_display::call(cmdtagname);

        tcop_dest::BeginCommand(command_tag, dest);

        if xact::IsAbortedTransactionBlockState() && !IsTransactionExitStmt(Some(stmt)) {
            return Err(crate::extended_query::aborted_xact_error("exec_simple_query"));
        }

        start_xact_command()?;

        if use_implicit_block {
            xact::BeginImplicitTransactionBlock();
        }

        check_for_interrupts()?;

        if analyze_seams::analyze_requires_snapshot::call(parsetree) {
            let snap = snapmgr::GetTransactionSnapshot()?;
            snapmgr::PushActiveSnapshot(&snap)?;
            snapshot_set = true;
        }

        // All but the last parsetree analyze+plan in a child context freed
        // after the statement (C: per_parsetree_context), so an N-statement
        // message retains O(len), not N x per-statement work. On the error
        // path the failed portal still points into it, so it is parked in
        // MessageContext's reset, which runs after error recovery has dropped
        // the portals.
        let per_parsetree_context =
            (!is_last).then(|| mcx.context().new_child_wholesale("per-parsetree message context"));
        let smcx: Mcx<'_> = per_parsetree_context.as_ref().map_or(mcx, |c| c.mcx());
        let result = (|| -> PgResult<()> {
            let querytree_list = pg_analyze_and_rewrite_fixedparams(
                smcx,
                parsetree,
                query_string,
                &[],
                QueryEnvHandle::NULL,
            )?;
            crate::stmt_trace::probe("q.rewrite");

            // pgrust.point_path: a statement whose plan is built without the planner (a SELECT without FROM)
            let skipped = if gravity_entry_seams::plan_skip::is_installed() {
                gravity_entry_seams::plan_skip::call(smcx, querytree_list, dest)?
            } else {
                Err(querytree_list)
            };
            let plantree_list = match skipped {
                Ok(plans) => plans,
                Err(querytree_list) => pg_plan_queries(smcx, querytree_list, query_string, CURSOR_OPT_PARALLEL_OK, ParamListHandle::NULL)?,
            };
            crate::stmt_trace::probe("q.plan");

            if snapshot_set {
                snapmgr::PopActiveSnapshot()?;
            }

            check_for_interrupts()?;

            // Gravity: a plain-text statement to the wire runs here with no
            // portal, each statement of a multi-statement string too (its
            // implicit transaction block's per-statement end is the portal
            // path's); else the incumbent portal path. D3134: postgres
            // --single's (DestDebug, the browser's postgres.wasm) too, so a
            // statement there is the server's uncounted simple-query run, not a
            // portal pull Gravity must assume may count its rows.
            if plantree_list.len() == 1 && matches!(dest, CommandDest::Remote | CommandDest::Debug) {
                let mut greceiver = tcop_dest::CreateDestReceiver(dest);
                // D3550: the statement holds the unnamed portal's place while
                // Gravity runs it (an unnamed portal left by the extended
                // protocol dropped first, as CreatePortal("", true, true) does)
                let held = portalmem::HoldUnnamedPortal()?;
                let replan = replan_eligible(&plantree_list[0]);
                if replan {
                    gravity_entry_seams::replan_arm::call();
                }
                let served = match gravity_entry_seams::simple::call(&plantree_list[0], &mut greceiver) {
                    Err(e) if replan && gravity_entry_seams::simple_refused_at_admission::call() => {
                        replan_refused(smcx, query_string, idx, &mut greceiver, e)
                    }
                    r => {
                        if replan {
                            gravity_entry_seams::replan_end::call(false);
                        }
                        r
                    }
                };
                drop(held);
                if served.is_err() {
                    leave_failed_unnamed_portal(query_string, command_tag);
                }
                if let Some(qc) = served? {
                    greceiver.destroy();
                    end_parsetree(stmt, is_last, use_implicit_block)?;
                    tcop_dest::EndCommand(&qc, dest, false)?;
                    return Ok(());
                }
            }

            let portal = portalmem::CreatePortal("", true, true)?;
            portal.borrow_mut().visible = false;

            // SAFETY: `plantree_list` is arena-backed by `mcx` and neither moves
            // nor drops before `stmt_list::free(stmts)` / the next reset_all().
            let stmts = unsafe { pquery::stmt_list::register(&plantree_list) };
            // SAFETY: `query_string` is the MessageContext message, which
            // outlives this unnamed portal: the portal is dropped below on
            // success, or by error_recovery's AbortCurrentTransaction on
            // failure, and MessageContext is reset only at the top of the
            // next main-loop iteration. C shares the pointer the same way
            // (`portal->sourceText = sourceText`).
            unsafe {
                portalmem::PortalDefineQuerySharedText(
                    &portal,
                    query_string,
                    command_tag,
                    stmts,
                    CachedPlanHandle::NULL,
                )
            };

            pquery::PortalStart(&portal, ParamListHandle::NULL, 0, None)?;
            crate::stmt_trace::probe("q.portalstart");

            /* Output format: text unless FETCH from a binary cursor. */
            let format: i16 = match stmt.node_tag() {
                NodeTag::T_FetchStmt => {
                    let fstmt = stmt.as_fetch_stmt().expect("T_FetchStmt");
                    let binary = !fstmt.ismove
                        && portalmem::GetPortalByName(fstmt.portalname).is_some_and(|fportal| {
                            fportal.borrow().cursorOptions & types_portal::CURSOR_OPT_BINARY != 0
                        });
                    i16::from(binary)
                }
                _ => 0, /* TEXT is default */
            };
            pquery::PortalSetResultFormat(&portal, &[format])?;

            let mut receiver = tcop_dest::CreateDestReceiver(dest);
            if dest == CommandDest::Remote {
                tcop_dest::SetRemoteDestReceiverParams(&mut receiver, portal.clone());
            }

            // GL-STMTTASK-1 (kill knob PGRUST_STMT_TASK, default OFF): arm the
            // statement-as-task executor hook for exactly THIS statement's
            // top-level portal run — the protocol-level half of the admission
            // envelope (simple protocol, single statement, wire dest, a raw
            // SELECT, normal non-subtransaction session state). The executor
            // hook owns the plan-shape gates and consumes the arm; the guard
            // disarms on every exit path so the arm can never leak past the
            // statement. Knob-OFF cost: one memoized bool read per statement.
            let _stmt_task_arm = postgres_seams::stmt_task_arm::arm_statement(
                n == 1
                    && dest == CommandDest::Remote
                    && stmt.node_tag() == NodeTag::T_SelectStmt
                    && !xact::IsSubTransaction(),
            );

            let mut qc = QueryCompletion::default();
            let _ = pquery::PortalRun(
                &portal,
                FETCH_ALL,
                true, /* always top level */
                &mut receiver,
                None, /* altdest aliases dest, as in C */
                Some(&mut qc),
            )?;

            crate::stmt_trace::probe("q.run");
            receiver.destroy();

            portalmem::PortalDrop(&portal, false)?;
            pquery::stmt_list::free(stmts);

            end_parsetree(stmt, is_last, use_implicit_block)?;

            tcop_dest::EndCommand(&qc, dest, false)?;
            Ok(())
        })();
        match (result, per_parsetree_context) {
            (Ok(()), ctx) => drop(ctx),
            (Err(e), Some(ctx)) => {
                mcx.context().register_reset_callback(move || drop(ctx));
                return Err(e);
            }
            (Err(e), None) => return Err(e),
        }
    }

    finish_xact_command()?;
    crate::stmt_trace::probe("q.commit");

    if parsetree_list.is_empty() {
        tcop_dest::NullCommand(dest)?;
    }

    match check_log_duration(was_logged) {
        (kind @ (1 | 2), msec_str) => log_duration_line(kind, &msec_str, &parsetree_list, query_string)?,
        _ => {}
    }

    if save_log_statement_stats {
        ShowUsage("QUERY STATISTICS")?;
    }

    // C: `debug_query_string = NULL;` here, after the duration line. A statement that finished leaves no retired text
    // behind; the `?` paths above still drop the guard, which keeps the text for the error's STATEMENT line (D4481).
    _debug_query.finish();

    Ok(())
}

fn end_parsetree(stmt: Node<'_>, is_last: bool, use_implicit_block: bool) -> PgResult<()> {
    if is_last {
        if use_implicit_block {
            xact::EndImplicitTransactionBlock();
        }
        finish_xact_command()
    } else if stmt.node_tag() == NodeTag::T_TransactionStmt {
        finish_xact_command()
    } else {
        debug_assert!((xact::MyXactFlags() & types_core::xact::XACT_FLAGS_NEEDIMMEDIATECOMMIT) == 0);
        xact::CommandCounterIncrement()?;
        disable_statement_timeout()
    }
}

// errdetail_execute (postgres.c:2487): "prepare: <query>" for the first
// EXECUTE in the list whose prepared statement exists (throw_error = false).
fn errdetail_execute(raw_parsetree_list: &[RawStmt<'_>]) -> PgResult<Option<String>> {
    for parsetree in raw_parsetree_list {
        let Some(stmt) = parsetree.stmt.and_then(|s| s.as_execute_stmt()) else {
            continue;
        };
        if let Some(psrc) = prepare_seams::fetch_prepared_statement_plansource::call(
            stmt.name.unwrap_or(""),
            false,
        )? {
            return Ok(Some(format!("prepare: {}", plancache::CachedPlanQueryString(psrc))));
        }
    }
    Ok(None)
}

pub fn start_xact_command() -> PgResult<()> {
    init_small::globals::ForgetInterruptRaised();
    init_small::globals::SetRedeliverInterrupts(0);
    if !xact_started() {
        xact::StartTransactionCommand()?;
        set_xact_started(true);
    } else if (xact::MyXactFlags() & types_core::xact::XACT_FLAGS_PIPELINING) != 0 {
        xact::BeginImplicitTransactionBlock();
    }

    enable_statement_timeout()?;

    // C postgres.c: start the dead-client check when configured. One-shot;
    // ProcessInterrupts re-arms it after every live check, so a parked
    // parallel leader keeps polling for the disconnect that would otherwise
    // have no cancel vector at all (GL-DISCONNECT-WEDGE-1).
    let interval = crate::client_connection_check_interval_ms();
    if interval > 0
        && init_small::globals::IsUnderPostmaster()
        && !timeout_seams::get_timeout_active::call(timeout_seams::CLIENT_CONNECTION_CHECK_TIMEOUT)
    {
        timeout_seams::enable_timeout_after::call(
            timeout_seams::CLIENT_CONNECTION_CHECK_TIMEOUT,
            interval,
        )?;
    }

    Ok(())
}

pub fn finish_xact_command() -> PgResult<()> {
    disable_statement_timeout()?;

    if xact_started() {
        xact::CommitTransactionCommand()?;
        set_xact_started(false);
    }

    Ok(())
}

pub(crate) fn IsTransactionExitStmt(parsetree: Option<Node<'_>>) -> bool {
    use types_nodes::parsenodes::TransactionStmtKind::*;
    match parsetree.and_then(|node| node.as_transaction_stmt()) {
        Some(stmt) => matches!(
            stmt.kind,
            TRANS_STMT_COMMIT | TRANS_STMT_PREPARE | TRANS_STMT_ROLLBACK | TRANS_STMT_ROLLBACK_TO
        ),
        None => false,
    }
}


use crate::extended_query::drop_unnamed_stmt;

// check_log_statement (postgres.c): GetCommandLogLevel ereports out of the
// EXPLAIN option probe and the EXECUTE look-through, so the probe is fallible.
fn check_log_statement(stmt_list: &PgVec<'_, RawStmt<'_>>) -> PgResult<bool> {
    use guc_tables::consts::{LOGSTMT_ALL, LOGSTMT_NONE};
    let log_statement = guc_tables::backing::log_statement();

    if log_statement == LOGSTMT_NONE {
        return Ok(false);
    }
    if log_statement == LOGSTMT_ALL {
        return Ok(true);
    }

    for raw in stmt_list.iter() {
        let Some(stmt) = raw.stmt else { continue };
        if utility_seams::get_command_log_level::call(stmt)? <= log_statement {
            return Ok(true);
        }
    }

    Ok(false)
}

// check_log_duration (postgres.c:2427), full C shape: log_min_duration_sample
// / log_statement_sample_rate decide an in_sample draw at the same decision
// point as C, and xact_is_sampled (drawn once per transaction from
// log_transaction_sample_rate at StartTransaction) forces logging for sampled
// transactions. (GL-GUCBATCH-1: the sampling refinement was a silent PARTIAL —
// SET accepted, never honored.)
pub(crate) fn check_log_duration(was_logged: bool) -> (i32, String) {
    let gucs = LogDurationGucs {
        log_duration: guc_tables::backing::log_duration(),
        log_min: guc_tables::backing::log_min_duration_statement(),
        log_min_sample: guc_tables::backing::log_min_duration_sample(),
        sample_rate: guc_tables::backing::log_statement_sample_rate(),
        xact_is_sampled: xact::xact_is_sampled(),
    };
    let diff_us =
        crate::get_current_timestamp() - xact::GetCurrentStatementStartTimestamp();
    check_log_duration_impl(was_logged, diff_us, &gucs, || {
        pg_prng::global_prng(pg_prng::PgPrng::next_f64)
    })
}

pub(crate) struct LogDurationGucs {
    pub(crate) log_duration: bool,
    pub(crate) log_min: i32,
    pub(crate) log_min_sample: i32,
    pub(crate) sample_rate: f64,
    pub(crate) xact_is_sampled: bool,
}

// The C decision body, GUC/clock/PRNG-free for the deterministic units.
pub(crate) fn check_log_duration_impl(
    was_logged: bool,
    diff_us: i64,
    g: &LogDurationGucs,
    draw: impl FnOnce() -> f64,
) -> (i32, String) {
    if !g.log_duration && g.log_min_sample < 0 && g.log_min < 0 && !g.xact_is_sampled {
        return (0, String::new());
    }

    // TimestampDifference (timestamp.c:1697): a nonpositive difference is 0/0.
    let diff_us = diff_us.max(0);
    let secs = diff_us / 1_000_000;
    let usecs = (diff_us % 1_000_000) as i64;
    let msecs = usecs / 1000;

    let exceeded_duration = g.log_min == 0
        || (g.log_min > 0 && (secs > i64::from(g.log_min) / 1000
            || secs * 1000 + msecs >= i64::from(g.log_min)));

    let exceeded_sample_duration = g.log_min_sample == 0
        || (g.log_min_sample > 0
            && (secs > i64::from(g.log_min_sample) / 1000
                || secs * 1000 + msecs >= i64::from(g.log_min_sample)));

    // C: no PRNG draw at rate 0 (never log) or rate 1 (always log).
    let mut in_sample = false;
    if exceeded_sample_duration {
        in_sample = g.sample_rate != 0.0 && (g.sample_rate == 1.0 || draw() <= g.sample_rate);
    }

    if exceeded_duration || in_sample || g.log_duration || g.xact_is_sampled {
        let msec_str = format!("{}.{:03}", secs * 1000 + msecs, usecs % 1000);
        if (exceeded_duration || in_sample || g.xact_is_sampled) && !was_logged {
            return (2, msec_str);
        }
        return (1, msec_str);
    }

    (0, String::new())
}

fn enable_statement_timeout() -> PgResult<()> {
    debug_assert!(xact_started());
    arm_statement_timeout()
}

pub(crate) fn arm_statement_timeout() -> PgResult<()> {
    let statement_timeout = lmgr_proc::globals::StatementTimeout();
    let transaction_timeout = lmgr_proc::globals::TransactionTimeout();
    // An earlier-or-equal active TRANSACTION_TIMEOUT subsumes the statement
    // timer. Admission can wait before a transaction starts: a configured
    // but unarmed transaction timer cannot bound that wait.
    if statement_timeout > 0
        && (statement_timeout < transaction_timeout || transaction_timeout == 0
            || !timeout_seams::get_timeout_active::call(timeout_seams::TRANSACTION_TIMEOUT))
    {
        if !timeout_seams::get_timeout_active::call(timeout_seams::STATEMENT_TIMEOUT) {
            timeout_seams::enable_timeout_after::call(
                timeout_seams::STATEMENT_TIMEOUT,
                statement_timeout,
            )?;
        }
    } else if timeout_seams::get_timeout_active::call(timeout_seams::STATEMENT_TIMEOUT) {
        timeout_seams::disable_timeout::call(timeout_seams::STATEMENT_TIMEOUT, false)?;
    }
    Ok(())
}

pub(crate) fn disable_statement_timeout() -> PgResult<()> {
    if timeout_seams::get_timeout_active::call(timeout_seams::STATEMENT_TIMEOUT) {
        timeout_seams::disable_timeout::call(timeout_seams::STATEMENT_TIMEOUT, false)?;
    }
    Ok(())
}
