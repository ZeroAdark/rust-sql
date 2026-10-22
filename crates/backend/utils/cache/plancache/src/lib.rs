// Divergence: RLS fields are constant-false.
#![allow(non_snake_case)]

use core::cell::RefCell;
use std::rc::Rc;

use cache_syscache::cacheinfo::{
    AMOPOPID, AUTHMEMROLEMEM, AUTHOID, DATABASEOID, FOREIGNDATAWRAPPEROID, FOREIGNSERVEROID,
    NAMESPACEOID, OPEROID, PROCOID, TYPEOID,
};
use catalog_namespace::SearchPathMatcher;
use datum::Datum;
use elog::ereport;
use mcx::{Mcx, MemoryContext, PgVec};
use types_core::xact::{InvalidTransactionId, TransactionIdIsValid};
use types_core::TransactionId;
use types_core::{CommandTag, InvalidOid, Oid};
use types_error::{PgResult, ERROR};
use types_nodes::nodes_enums::CmdType;
use types_nodes::parsenodes::{Query, RTEKind, RangeTblEntry};
use types_nodes::plannodes::PlannedStmt;
use types_nodes::rawnodes::RawStmt;
use types_portal::{
    CachedPlanHandle, ParamListHandle, PortalStrategy, QueryEnvHandle, CURSOR_OPT_CUSTOM_PLAN,
    CURSOR_OPT_GENERIC_PLAN,
};
use types_resowner::ResourceOwner;
use types_tuple::TupleDescData;

const REGCLASSOID: Oid = 2205;

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CachedPlanSourceHandle(pub u64);

pub type ReanalyzeFn = fn(
    h: CachedPlanSourceHandle,
    qmcx: Mcx<'static>,
    raw: &'static RawStmt<'static>,
    query_string: &'static str,
    param_types: &'static [Oid],
    query_env: QueryEnvHandle,
    arg: i32,
) -> PgResult<PgVec<'static, Query<'static>>>;

pub fn SetCachedPlanReanalyze(h: CachedPlanSourceHandle, f: ReanalyzeFn, arg: i32) {
    with_source(h, |src| src.reanalyze = Some((f, arg)));
}

pub type PostRewriteFn = fn(
    h: CachedPlanSourceHandle,
    qmcx: Mcx<'static>,
    query_list: &mut PgVec<'static, Query<'static>>,
    arg: i32,
) -> PgResult<()>;

/// Must be called before the source is saved: the hook is part of how the source rebuilds itself.
pub fn SetCachedPlanPostRewrite(h: CachedPlanSourceHandle, f: PostRewriteFn, arg: i32) {
    with_source(h, |src| src.post_rewrite = Some((f, arg)));
}

struct CachedPlanSource {
    handle_gen: u32,
    query_string: &'static str,
    commandTag: CommandTag,
    param_types: &'static [Oid],
    cursor_options: i32,
    fixed_result: bool,
    requires_reval: bool,
    requires_snapshot: bool,
    is_xact_exit_stmt: bool,
    // Handed out only as copies because analysis scribbles on its input.
    raw_parse_tree: Option<&'static RawStmt<'static>>,
    // Exactly one of the two trees is retained (an empty statement retains neither).
    analyzed_parse_tree: Option<&'static Query<'static>>,
    reanalyze: Option<(ReanalyzeFn, i32)>,
    post_rewrite: Option<(PostRewriteFn, i32)>,
    query_list: &'static [Query<'static>],
    relation_oids: &'static [Oid],
    inval_items: &'static [(i32, u32)],
    search_path: Option<SearchPathMatcher<'static>>,
    result_desc: Option<Rc<TupleDescData<'static>>>,
    gplan: Option<CachedPlanHandle>,
    depends_on_rls: bool,
    rewrite_role_id: Oid,
    rewrite_row_security: bool,
    is_complete: bool,
    is_saved: bool,
    is_valid: bool,
    is_oneshot: bool,
    // Dropped source whose arenas must outlive it: custom plans share
    // query-arena subnodes (C copies; we defer the free to last plan release).
    dead: bool,
    generation: i32,
    generic_cost: f64,
    total_custom_cost: f64,
    num_generic_plans: i64,
    num_custom_plans: i64,
    source_ctx: *mut MemoryContext,
    query_ctx: *mut MemoryContext,
    // An entry with barrier B is referenceable exactly by live plans of this source with generation <= B.
    pending_query_ctxs: Vec<(i32, *mut MemoryContext)>,
    // O(1) drop/release with many saved plans (C unlinks a dlist node).
    live_plans: u32,
    saved_pos: usize,
}

struct CachedPlan {
    handle_gen: u32,
    source: CachedPlanSourceHandle,
    stmt_list: &'static [PlannedStmt<'static>],
    plan_role_id: Oid,
    depends_on_role: bool,
    unnest_mode: i32,
    saved_xmin: TransactionId,
    refcount: i32,
    generation: i32,
    is_saved: bool,
    is_valid: bool,
    plan_ctx: *mut MemoryContext,
}

struct PlanCache {
    sources: Vec<Option<CachedPlanSource>>,
    source_free: Vec<u32>,
    plans: Vec<Option<CachedPlan>>,
    plan_free: Vec<u32>,
    saved_plan_list: Vec<CachedPlanSourceHandle>,
    handle_gen: u32,
    // Those releases are C's abandoned refcounts: no-ops, never staleness.
    torn_down: bool,
}

thread_local! {
    // ManuallyDrop — no dtor is registered, so late ReleaseCachedPlan calls from other TLS dtors can never hit a destroyed key.
    static CACHE: RefCell<core::mem::ManuallyDrop<PlanCache>> = const {
        RefCell::new(core::mem::ManuallyDrop::new(PlanCache {
            sources: Vec::new(),
            source_free: Vec::new(),
            plans: Vec::new(),
            plan_free: Vec::new(),
            saved_plan_list: Vec::new(),
            handle_gen: 0,
            torn_down: false,
        }))
    };
}

fn encode(idx: u32, generation: u32) -> u64 {
    (u64::from(generation) << 32) | u64::from(idx + 1)
}

fn decode(h: u64) -> (usize, u32) {
    (((h as u32) - 1) as usize, (h >> 32) as u32)
}

// Callbacks flip flags only and never call out, so one borrow per entry point is safe.
fn with_cache<R>(f: impl FnOnce(&mut PlanCache) -> R) -> R {
    CACHE.with(|c| f(&mut c.borrow_mut()))
}

fn source_mut(pc: &mut PlanCache, h: CachedPlanSourceHandle) -> &mut CachedPlanSource {
    let (idx, generation) = decode(h.0);
    match pc.sources.get_mut(idx).and_then(Option::as_mut) {
        Some(s) if s.handle_gen == generation => s,
        _ => panic!("plancache: stale CachedPlanSourceHandle {h:?} (dropped)"),
    }
}

fn with_source<R>(h: CachedPlanSourceHandle, f: impl FnOnce(&mut CachedPlanSource) -> R) -> R {
    with_cache(|pc| f(source_mut(pc, h)))
}

fn plan_mut(pc: &mut PlanCache, h: CachedPlanHandle) -> &mut CachedPlan {
    let (idx, generation) = decode(h.0);
    match pc.plans.get_mut(idx).and_then(Option::as_mut) {
        Some(p) if p.handle_gen == generation => p,
        _ => panic!("plancache: stale CachedPlanHandle {h:?} (released)"),
    }
}

fn with_plan<R>(h: CachedPlanHandle, f: impl FnOnce(&mut CachedPlan) -> R) -> R {
    with_cache(|pc| f(plan_mut(pc, h)))
}

fn leak_ctx(name: &'static str) -> *mut MemoryContext {
    Box::into_raw(Box::new(MemoryContext::new_start_small(name)))
}

// C copyObjects a finished query or plan tree into a fresh context; analysis
// here runs in the query arena, and arenas grow by doubling blocks, so a large
// arena is recopied into a context sized to its tree.
const COMPACT_MIN_BYTES: usize = 64 * 1024;

fn compact_into<T>(
    old: *mut MemoryContext,
    name: &'static str,
    copy: impl Fn(Mcx<'static>) -> PgResult<T>,
) -> PgResult<Option<(*mut MemoryContext, T)>> {
    let footprint = ctx_ref(old).stats().arena_footprint;
    if footprint < COMPACT_MIN_BYTES {
        return Ok(None);
    }
    let probe = leak_ctx(name);
    let measured = copy(ctx_mcx(probe)).map(|first| {
        let st = ctx_ref(probe).stats();
        let used = st.arena_footprint.saturating_sub(st.free_bytes);
        drop(first);
        used
    });
    reclaim_ctx(probe);
    let used = measured?;
    if used.saturating_mul(5) / 4 >= footprint {
        return Ok(None);
    }
    let exact = Box::into_raw(Box::new(MemoryContext::new_sized(name, used + used / 8 + 2048)));
    match copy(ctx_mcx(exact)) {
        Ok(v) => Ok(Some((exact, v))),
        Err(e) => {
            reclaim_ctx(exact);
            Err(e)
        }
    }
}

struct QueryArenaCopy {
    query_list: &'static [Query<'static>],
    relation_oids: &'static [Oid],
    inval_items: &'static [(i32, u32)],
    search_path: Option<SearchPathMatcher<'static>>,
}

// At SaveCachedPlan: analysis callers are done with the arena and no plan
// references it (live_plans == 0).
fn compact_query_arena(h: CachedPlanSourceHandle) -> PgResult<()> {
    let (query_ctx, live_plans, query_list, relation_oids, inval_items) = with_source(h, |src| {
        (src.query_ctx, src.live_plans, src.query_list, src.relation_oids, src.inval_items)
    });
    if live_plans != 0 || query_ctx.is_null() {
        return Ok(());
    }
    let matcher = with_source(h, |src| src.search_path.take());
    let copied = compact_into(query_ctx, "CachedPlanQuery", |m| {
        let mut v: PgVec<'static, Query<'static>> = PgVec::new_in(m);
        v.try_reserve_exact(query_list.len()).map_err(|_| m.oom(query_list.len()))?;
        for q in query_list {
            v.push(copyfuncs::copy_query(m, q)?);
        }
        Ok(QueryArenaCopy {
            query_list: mcx::vec_borrow_in(m, v)?,
            relation_oids: mcx::slice_borrow_in(m, relation_oids)?,
            inval_items: mcx::slice_borrow_in(m, inval_items)?,
            search_path: match matcher.as_ref() {
                Some(sp) => Some(catalog_namespace::CopySearchPathMatcher(m, sp)?),
                None => None,
            },
        })
    });
    match copied {
        Ok(Some((compacted, c))) => {
            drop(matcher);
            with_source(h, |src| {
                src.query_ctx = compacted;
                src.query_list = c.query_list;
                src.relation_oids = c.relation_oids;
                src.inval_items = c.inval_items;
                src.search_path = c.search_path;
            });
            reclaim_ctx(query_ctx);
            Ok(())
        }
        Ok(None) => {
            with_source(h, |src| src.search_path = matcher);
            Ok(())
        }
        Err(e) => {
            with_source(h, |src| src.search_path = matcher);
            Err(e)
        }
    }
}

fn ctx_ref<'a>(ctx: *mut MemoryContext) -> &'a MemoryContext {
    // SAFETY: leak_ctx provenance (see ctx_mcx).
    unsafe { &*ctx }
}

fn ctx_mcx(ctx: *mut MemoryContext) -> Mcx<'static> {
    // SAFETY: ctx came from leak_ctx and is reclaimed only after its owning
    // registry slot (the only path to it) is removed.
    unsafe { (*ctx).mcx() }
}

fn reclaim_ctx(ctx: *mut MemoryContext) {
    if ctx.is_null() {
        return;
    }
    // SAFETY: leak_ctx provenance; caller removed every reference first.
    drop(unsafe { Box::from_raw(ctx) });
}

// Pending-arena drain (see pending_query_ctxs): frees every replan-displaced
// query arena no remaining live plan of `h` can reference — entry barrier B
// is safe to reclaim once every live plan of the source has generation > B.
// A stale/absent source slot returns nothing (the tombstone paths free the
// list wholesale). Caller reclaims the returned arenas outside the borrow.
fn drain_pending_arenas(
    pc: &mut PlanCache,
    h: CachedPlanSourceHandle,
) -> Vec<*mut MemoryContext> {
    let (idx, generation) = decode(h.0);
    match pc.sources.get(idx).and_then(Option::as_ref) {
        Some(src) if src.handle_gen == generation && !src.pending_query_ctxs.is_empty() => {}
        _ => return Vec::new(),
    }
    let min_gen =
        pc.plans.iter().flatten().filter(|p| p.source == h).map(|p| p.generation).min();
    let src = pc.sources[idx].as_mut().expect("checked above");
    let mut freed = Vec::new();
    src.pending_query_ctxs.retain(|&(barrier, ctx)| match min_gen {
        Some(m) if barrier >= m => true,
        _ => {
            freed.push(ctx);
            false
        }
    });
    freed
}

pub fn init_seams() {
    plancache_portal_seams::init_plan_cache::set(InitPlanCache);
    plancache_portal_seams::release_cached_plan::set(ReleaseCachedPlan);
    plancache_portal_seams::incr_cached_plan::set(IncrCachedPlan);
    plancache_portal_seams::is_source_generic_plan::set(CachedPlanIsSourceGeneric);
    plancache_portal_seams::plan_identity::set(CachedPlanIdentity);
    thread_local! {
        static PLAN_CACHE_MODE: core::cell::Cell<i32> =
            const { core::cell::Cell::new(guc_tables::consts::PLAN_CACHE_MODE_AUTO) };
    }
    {
        guc_tables::vars::plan_cache_mode.install_if_absent(guc_tables::GucVarAccessors {
            get: || PLAN_CACHE_MODE.with(core::cell::Cell::get),
            set: |v| PLAN_CACHE_MODE.with(|c| c.set(v)),
        });
    }
}

// C frees these by process death (plancache.c allocates under CacheMemoryContext); a thread-per-connection backend must free them itself or every prepared statement's CachedPlanSource/CachedPlanQuery/CachedPlan contexts leak into the postmaster's heap for the server's lifetime.
fn ReleaseAllCachedPlansAtExit(_code: i32, _arg: usize) {
    let ctxs = with_cache(|pc| {
        let mut ctxs = Vec::new();
        for slot in pc.plans.iter_mut() {
            if let Some(plan) = slot.take() {
                ctxs.push(plan.plan_ctx);
            }
        }
        for slot in pc.sources.iter_mut() {
            if let Some(src) = slot.take() {
                ctxs.extend(src.pending_query_ctxs.into_iter().map(|(_, c)| c));
                ctxs.push(src.query_ctx);
                ctxs.push(src.source_ctx);
            }
        }
        pc.plans = Vec::new();
        pc.plan_free = Vec::new();
        pc.sources = Vec::new();
        pc.source_free = Vec::new();
        pc.saved_plan_list = Vec::new();
        pc.torn_down = true;
        ctxs
    });
    for ctx in ctxs {
        reclaim_ctx(ctx);
    }
}

// A warm-claimed worker thread skips InitPlanCache after its exit teardown.
fn rearm_after_exit_teardown() {
    let was_torn_down = with_cache(|pc| core::mem::replace(&mut pc.torn_down, false));
    if was_torn_down && ipc_seams::on_proc_exit::is_installed() {
        ipc_seams::on_proc_exit::call(ReleaseAllCachedPlansAtExit, 0);
    }
}

pub fn InitPlanCache() -> PgResult<()> {
    with_cache(|pc| pc.torn_down = false);
    if ipc_seams::on_proc_exit::is_installed() {
        ipc_seams::on_proc_exit::call(ReleaseAllCachedPlansAtExit, 0);
    }
    let zero = Datum::from_oid(InvalidOid);
    inval::invalidate::CacheRegisterRelcacheCallback(PlanCacheRelCallback, zero)?;
    inval::invalidate::CacheRegisterSyscacheCallback(PROCOID, PlanCacheObjectCallback, zero)?;
    inval::invalidate::CacheRegisterSyscacheCallback(TYPEOID, PlanCacheObjectCallback, zero)?;
    inval::invalidate::CacheRegisterSyscacheCallback(NAMESPACEOID, PlanCacheSysCallback, zero)?;
    inval::invalidate::CacheRegisterSyscacheCallback(OPEROID, PlanCacheSysCallback, zero)?;
    inval::invalidate::CacheRegisterSyscacheCallback(AMOPOPID, PlanCacheSysCallback, zero)?;
    inval::invalidate::CacheRegisterSyscacheCallback(FOREIGNSERVEROID, PlanCacheSysCallback, zero)?;
    inval::invalidate::CacheRegisterSyscacheCallback(
        FOREIGNDATAWRAPPEROID,
        PlanCacheSysCallback,
        zero,
    )?;
    inval::invalidate::CacheRegisterSyscacheCallback(AUTHMEMROLEMEM, PlanCacheRoleCallback, zero)?;
    inval::invalidate::CacheRegisterSyscacheCallback(AUTHOID, PlanCacheRoleCallback, zero)?;
    inval::invalidate::CacheRegisterSyscacheCallback(DATABASEOID, PlanCacheRoleCallback, zero)?;
    Ok(())
}

pub fn CreateCachedPlan(
    raw_parse_tree: Option<&RawStmt<'_>>,
    query_string: &str,
    commandTag: CommandTag,
) -> PgResult<CachedPlanSourceHandle> {
    let flags = match raw_parse_tree {
        Some(raw) => (
            parser_analyze::stmt_requires_parse_analysis(raw),
            parser_analyze::analyze_requires_snapshot(raw),
            is_transaction_exit_stmt(raw),
        ),
        None => (false, false, false),
    };
    create_cached_plan_flags(raw_parse_tree, None, flags, query_string, commandTag)
}

pub fn CreateOneShotCachedPlan(
    raw_parse_tree: Option<&RawStmt<'_>>,
    query_string: &str,
    commandTag: CommandTag,
) -> PgResult<CachedPlanSourceHandle> {
    let h = CreateCachedPlan(raw_parse_tree, query_string, commandTag)?;
    with_source(h, |src| src.is_oneshot = true);
    Ok(h)
}

pub fn CreateCachedPlanForQuery(
    analyzed: &Query<'_>,
    query_string: &str,
    commandTag: CommandTag,
) -> PgResult<CachedPlanSourceHandle> {
    let reval = parser_analyze::query_requires_rewrite_plan(analyzed);
    create_cached_plan_flags(None, Some(analyzed), (reval, reval, false), query_string, commandTag)
}

fn create_cached_plan_flags(
    raw_parse_tree: Option<&RawStmt<'_>>,
    analyzed_parse_tree: Option<&Query<'_>>,
    (requires_reval, requires_snapshot, is_xact_exit_stmt): (bool, bool, bool),
    query_string: &str,
    commandTag: CommandTag,
) -> PgResult<CachedPlanSourceHandle> {
    rearm_after_exit_teardown();
    let source_ctx = leak_ctx("CachedPlanSource");
    let query_ctx = leak_ctx("CachedPlanQuery");
    let mcx = ctx_mcx(source_ctx);
    let qs = mcx::slice_borrow_in(mcx, query_string.as_bytes())?;
    let query_string: &'static str = core::str::from_utf8(qs).expect("query_string is UTF-8");
    // plancache.c:223: MemoryContextSetIdentifier(source_context,
    // plansource->query_string).
    ctx_ref(source_ctx).set_ident(Some(ident_prefix(query_string)));
    let bail = |e| {
        reclaim_ctx(query_ctx);
        reclaim_ctx(source_ctx);
        e
    };
    let raw_parse_tree = match raw_parse_tree {
        Some(raw) => match copy_raw_stmt_in(mcx, raw) {
            Ok(r) => Some(r),
            Err(e) => return Err(bail(e)),
        },
        None => None,
    };
    let analyzed_parse_tree = match analyzed_parse_tree {
        Some(q) => match copyfuncs::copy_query(mcx, q).and_then(|c| mcx::alloc_leak_in(mcx, c)) {
            Ok(q) => Some(&*q),
            Err(e) => return Err(bail(e)),
        },
        None => None,
    };

    Ok(with_cache(|pc| {
        pc.handle_gen = pc.handle_gen.wrapping_add(1);
        let source = CachedPlanSource {
            handle_gen: pc.handle_gen,
            query_string,
            commandTag,
            param_types: &[],
            cursor_options: 0,
            fixed_result: false,
            requires_reval,
            requires_snapshot,
            is_xact_exit_stmt,
            raw_parse_tree,
            analyzed_parse_tree,
            reanalyze: None,
            post_rewrite: None,
            query_list: &[],
            relation_oids: &[],
            inval_items: &[],
            search_path: None,
            result_desc: None,
            gplan: None,
            depends_on_rls: false,
            rewrite_role_id: InvalidOid,
            rewrite_row_security: false,
            is_complete: false,
            is_saved: false,
            is_valid: false,
            is_oneshot: false,
            dead: false,
            generation: 0,
            live_plans: 0,
            saved_pos: 0,
            generic_cost: -1.0,
            total_custom_cost: 0.0,
            num_generic_plans: 0,
            num_custom_plans: 0,
            source_ctx,
            query_ctx,
            pending_query_ctxs: Vec::new(),
        };
        let idx = match pc.source_free.pop() {
            Some(i) => {
                pc.sources[i as usize] = Some(source);
                i
            }
            None => {
                pc.sources.push(Some(source));
                (pc.sources.len() - 1) as u32
            }
        };
        CachedPlanSourceHandle(encode(idx, pc.handle_gen))
    }))
}

/// Analysis output must be allocated with this Mcx so the plansource owns it with zero copies.
/// SaveCachedPlan may reclaim this arena: caller scratch in it must be dropped first.
pub fn SourceQueryMcx(h: CachedPlanSourceHandle) -> Mcx<'static> {
    ctx_mcx(with_source(h, |src| src.query_ctx))
}

pub fn CompleteCachedPlan(
    h: CachedPlanSourceHandle,
    query_list: PgVec<'static, Query<'static>>,
    param_types: &[Oid],
    cursor_options: i32,
    fixed_result: bool,
) -> PgResult<()> {
    let (source_ctx, query_ctx, requires_reval) = with_source(h, |src| {
        assert!(!src.is_complete, "CompleteCachedPlan: already complete");
        (src.source_ctx, src.query_ctx, src.requires_reval)
    });
    let source_mcx = ctx_mcx(source_ctx);
    let query_mcx = ctx_mcx(query_ctx);

    let query_list: &'static [Query<'static>] = mcx::vec_borrow_in(query_mcx, query_list)?;

    let (relation_oids, inval_items, search_path) = if requires_reval {
        let mut oids: PgVec<'static, Oid> = PgVec::new_in(query_mcx);
        let mut items: PgVec<'static, (i32, u32)> = PgVec::new_in(query_mcx);
        for q in query_list {
            extract_query_deps(q, &mut oids, &mut items)?;
        }
        (
            mcx::vec_borrow_in(query_mcx, oids)?,
            mcx::vec_borrow_in(query_mcx, items)?,
            Some(catalog_namespace::GetSearchPathMatcher(query_mcx)?),
        )
    } else {
        (&[] as &[Oid], &[] as &[(i32, u32)], None)
    };

    let result_desc = plan_cache_compute_result_desc(source_mcx, query_list)?;
    let param_types: &'static [Oid] = mcx::slice_borrow_in(source_mcx, param_types)?;

    let depends_on_rls = query_list.iter().any(|q| q.hasRowSecurity);
    let rewrite_role_id = miscinit::GetUserId();
    let rewrite_row_security = guc_tables::backing::row_security();
    with_source(h, |src| {
        src.query_list = query_list;
        src.depends_on_rls = depends_on_rls;
        src.rewrite_role_id = rewrite_role_id;
        src.rewrite_row_security = rewrite_row_security;
        src.relation_oids = relation_oids;
        src.inval_items = inval_items;
        src.search_path = search_path;
        src.result_desc = result_desc;
        src.param_types = param_types;
        src.cursor_options = cursor_options;
        src.fixed_result = fixed_result;
        src.is_complete = true;
        src.is_valid = true;
    });
    Ok(())
}

// plancache.c:432-440 (CompleteCachedPlan with querytree_context NULL): a
// caller that analyzed in its own scratch context copies the tree into the
// source's query context before completing it.
pub fn CopyQueryListToSource(
    h: CachedPlanSourceHandle,
    query_list: &[Query<'_>],
) -> PgResult<PgVec<'static, Query<'static>>> {
    let mcx = SourceQueryMcx(h);
    let mut v: PgVec<'static, Query<'static>> = PgVec::new_in(mcx);
    v.try_reserve_exact(query_list.len()).map_err(|_| mcx.oom(query_list.len()))?;
    for q in query_list {
        v.push(copyfuncs::copy_query(mcx, q)?);
    }
    Ok(v)
}

pub fn SaveCachedPlan(h: CachedPlanSourceHandle) -> PgResult<()> {
    ReleaseGenericPlan(h);
    compact_query_arena(h)?;
    with_cache(|pc| {
        let pos = pc.saved_plan_list.len();
        let src = source_mut(pc, h);
        assert!(src.is_complete && !src.is_saved, "SaveCachedPlan: bad order");
        src.is_saved = true;
        src.saved_pos = pos;
        pc.saved_plan_list.push(h);
    });
    Ok(())
}

pub fn DropCachedPlan(h: CachedPlanSourceHandle) {
    // Post-exit-reclaim drops (e.g. a cache teardown running after the on_proc_exit ceremony) are C's process-death abandonment: no-ops.
    if with_cache(|pc| pc.torn_down) {
        return;
    }
    if plancache_portal_seams::discard_parked_portal::is_installed() {
        plancache_portal_seams::discard_parked_portal::call(types_portal::PlanSourceHandle(h.0));
    }
    if plancache_portal_seams::engine_plan_dropped::is_installed() {
        plancache_portal_seams::engine_plan_dropped::call(types_portal::PlanSourceHandle(h.0));
    }
    with_cache(|pc| {
        let src = source_mut(pc, h);
        if src.is_saved {
            src.is_saved = false;
            let pos = src.saved_pos;
            pc.saved_plan_list.swap_remove(pos);
            if let Some(&moved) = pc.saved_plan_list.get(pos) {
                source_mut(pc, moved).saved_pos = pos;
            }
        }
    });
    ReleaseGenericPlan(h);
    let ctxs = with_cache(|pc| {
        // A surviving refcounted plan (pipelined portal) tombstones the source; ReleaseCachedPlan of the last survivor frees it.
        if source_mut(pc, h).live_plans > 0 {
            source_mut(pc, h).dead = true;
            return None;
        }
        let (idx, _) = decode(h.0);
        let src = pc.sources[idx].take().expect("checked by source_mut");
        pc.source_free.push(idx as u32);
        Some((src.source_ctx, src.query_ctx, src.pending_query_ctxs))
    });
    if let Some((source_ctx, query_ctx, pending)) = ctxs {
        for (_, ctx) in pending {
            reclaim_ctx(ctx);
        }
        reclaim_ctx(query_ctx);
        reclaim_ctx(source_ctx);
    }
}

pub fn ReleaseIdleGenericPlans() -> usize {
    let handles: Vec<CachedPlanSourceHandle> = with_cache(|pc| {
        if pc.torn_down {
            return Vec::new();
        }
        pc.saved_plan_list.clone()
    });
    let mut dropped = 0usize;
    for h in handles {
        if with_source(h, |src| src.gplan.is_some()) {
            ReleaseGenericPlan(h);
            dropped += 1;
        }
    }
    dropped
}

fn ReleaseGenericPlan(h: CachedPlanSourceHandle) {
    let gplan = with_source(h, |src| src.gplan.take());
    if let Some(plan) = gplan {
        ReleaseCachedPlan(plan);
    }
}

pub fn ReleaseCachedPlan(cplan: CachedPlanHandle) {
    let freed = with_cache(|pc| {
        // A release arriving after ReleaseAllCachedPlansAtExit reclaimed the registry is an abandoned refcount in C's process model: drop it silently.
        if pc.torn_down {
            return Vec::new();
        }
        let plan = plan_mut(pc, cplan);
        assert!(plan.refcount > 0, "ReleaseCachedPlan: refcount underflow");
        plan.refcount -= 1;
        if plan.refcount == 0 {
            let (idx, _) = decode(cplan.0);
            let plan = pc.plans[idx].take().expect("checked by plan_mut");
            pc.plan_free.push(idx as u32);
            let mut ctxs = vec![plan.plan_ctx];
            let (sidx, sgen) = decode(plan.source.0);
            let mut src_dead = false;
            let mut src_unpinned = false;
            if let Some(s) = pc.sources.get_mut(sidx).and_then(Option::as_mut) {
                if s.handle_gen == sgen {
                    s.live_plans -= 1;
                    src_dead = s.dead;
                    src_unpinned = s.live_plans == 0;
                }
            }
            if src_dead && src_unpinned {
                let src = pc.sources[sidx].take().expect("checked above");
                pc.source_free.push(sidx as u32);
                ctxs.extend(src.pending_query_ctxs.into_iter().map(|(_, c)| c));
                ctxs.push(src.query_ctx);
                ctxs.push(src.source_ctx);
            } else if !src_dead {
                ctxs.extend(drain_pending_arenas(pc, plan.source));
            }
            ctxs
        } else {
            Vec::new()
        }
    });
    for ctx in freed {
        reclaim_ctx(ctx);
    }
}

// Extra refcount on a live plan; pairs with ReleaseCachedPlan.
pub fn IncrCachedPlan(cplan: CachedPlanHandle) {
    with_cache(|pc| {
        let plan = plan_mut(pc, cplan);
        assert!(plan.refcount > 0, "IncrCachedPlan: plan already freed");
        plan.refcount += 1;
    });
}

// Caller must hold a refcount on cplan.
pub fn CachedPlanIsSourceGeneric(cplan: CachedPlanHandle) -> bool {
    with_cache(|pc| {
        let src = plan_mut(pc, cplan).source;
        let (idx, gen) = decode(src.0);
        pc.sources
            .get(idx)
            .and_then(Option::as_ref)
            .is_some_and(|s| s.handle_gen == gen && s.gplan == Some(cplan))
    })
}

pub fn CachedPlanIsValid(h: CachedPlanSourceHandle) -> bool {
    with_source(h, |src| src.is_valid)
}

pub fn CachedPlanSourceIsValid(h: CachedPlanSourceHandle) -> bool {
    with_cache(|pc| source_mut(pc, h).is_valid)
}

pub fn CachedPlanSourceRequiresReanalysis(h: CachedPlanSourceHandle) -> PgResult<bool> {
    let (requires_reval, is_valid, depends_on_rls, role, row_sec) = with_source(h, |src| {
        (
            src.requires_reval,
            src.is_valid,
            src.depends_on_rls,
            src.rewrite_role_id,
            src.rewrite_row_security,
        )
    });
    if !requires_reval {
        return Ok(false);
    }
    if !is_valid {
        return Ok(true);
    }
    if !search_path_generation_matches(h)? {
        let mut matcher = with_source(h, |src| src.search_path.take());
        let matches = match matcher.as_mut() {
            Some(m) => catalog_namespace::SearchPathMatchesCurrentEnvironment(m),
            None => panic!(
                "CachedPlanSourceRequiresReanalysis: valid revalidatable source lost its search_path"
            ),
        };
        with_source(h, |src| src.search_path = matcher);
        if !matches? {
            return Ok(true);
        }
    }
    Ok(depends_on_rls
        && (role != miscinit::GetUserId() || row_sec != guc_tables::backing::row_security()))
}

// Caller must hold a refcount on `cplan`.
pub fn CachedPlanIsSimplyValid(
    h: CachedPlanSourceHandle,
    cplan: CachedPlanHandle,
) -> PgResult<bool> {
    let active = catalog_namespace::ActiveSearchPathGeneration()?;
    let (ok, path_current) = with_cache(|pc| {
        let (src_valid, gplan, path_current) = {
            let src = source_mut(pc, h);
            let current = src.search_path.as_ref().is_some_and(|m| m.generation == active);
            (src.is_valid, src.gplan, current)
        };
        (src_valid && gplan == Some(cplan) && plan_mut(pc, cplan).is_valid, path_current)
    });
    if !ok {
        return Ok(false);
    }
    if path_current {
        return Ok(true);
    }
    // SearchPathMatchesCurrentEnvironment probes catalogs and must run outside the cache borrow.
    let mut matcher = with_source(h, |src| src.search_path.take());
    let matches = match matcher.as_mut() {
        Some(m) => catalog_namespace::SearchPathMatchesCurrentEnvironment(m),
        None => panic!("CachedPlanIsSimplyValid: valid revalidatable source lost its search_path"),
    };
    with_source(h, |src| src.search_path = matcher);
    matches
}

// SearchPathMatchesCurrentEnvironment's generation fast path, read in place:
// the matcher only needs to leave the source when it must be compared.
fn search_path_generation_matches(h: CachedPlanSourceHandle) -> PgResult<bool> {
    let active = catalog_namespace::ActiveSearchPathGeneration()?;
    Ok(with_source(h, |src| src.search_path.as_ref().is_some_and(|m| m.generation == active)))
}

thread_local! {
    static INVAL_COUNTER: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
}

pub fn PlanCacheInvalCounter() -> u64 {
    INVAL_COUNTER.with(core::cell::Cell::get)
}

fn bump_inval_counter() {
    INVAL_COUNTER.with(|c| c.set(c.get().wrapping_add(1)));
}

/// Valid while the source stays saved and valid (callers must have just revalidated via GetCachedPlan).
pub fn SourceQueryList(h: CachedPlanSourceHandle) -> &'static [Query<'static>] {
    with_source(h, |src| src.query_list)
}

/// Valid while the caller holds a refcount on `cplan`.
pub fn CachedPlanStmtList(cplan: CachedPlanHandle) -> &'static [PlannedStmt<'static>] {
    with_plan(cplan, |plan| {
        assert!(plan.refcount > 0, "CachedPlanStmtList: caller holds no refcount");
        plan.stmt_list
    })
}

pub fn GetCachedPlan(
    h: CachedPlanSourceHandle,
    boundParams: ParamListHandle,
    owner: Option<ResourceOwner>,
    queryEnv: QueryEnvHandle,
) -> PgResult<CachedPlanHandle> {
    let is_saved = with_source(h, |src| {
        assert!(src.is_complete, "GetCachedPlan: incomplete plansource");
        src.is_saved
    });
    if let Some(owner) = owner {
        if !is_saved {
            return Err(ereport(ERROR)
                .errmsg("cannot apply ResourceOwner to non-saved cached plan")
                .into_error()
                .into());
        }
        panic!(
            "GetCachedPlan (plancache.c): ResourceOwner-tracked plan refs ({owner:?}) \
             are the EXPLAIN EXECUTE/SPI lane"
        );
    }

    RevalidateCachedQuery(h, queryEnv, true)?;

    let mut customplan = choose_custom_plan(h, boundParams);
    let mut plan: Option<CachedPlanHandle> = None;

    if !customplan {
        if CheckCachedPlan(h)? {
            plan = with_source(h, |src| src.gplan);
            debug_assert!(plan.is_some());
        } else {
            let built = BuildCachedPlan(h, ParamListHandle::NULL, queryEnv)?;
            ReleaseGenericPlan(h);
            with_cache(|pc| {
                let cost = cached_plan_cost(plan_mut(pc, built).stmt_list, false);
                let p = plan_mut(pc, built);
                p.refcount += 1;
                p.is_saved = is_saved;
                let src = source_mut(pc, h);
                src.gplan = Some(built);
                src.generic_cost = cost;
            });
            plan = Some(built);
            // A losing generic plan is kept but not executed.
            customplan = choose_custom_plan(h, boundParams);
        }
    }

    if customplan {
        let built = BuildCachedPlan(h, boundParams, queryEnv)?;
        with_cache(|pc| {
            let cost = cached_plan_cost(plan_mut(pc, built).stmt_list, true);
            let src = source_mut(pc, h);
            src.total_custom_cost += cost;
            src.num_custom_plans += 1;
        });
        plan = Some(built);
    }

    let plan = plan.expect("GetCachedPlan: no plan chosen");
    with_cache(|pc| {
        if !customplan {
            source_mut(pc, h).num_generic_plans += 1;
        }
        let p = plan_mut(pc, plan);
        p.refcount += 1;
        if customplan && is_saved {
            p.is_saved = true;
        }
    });
    Ok(plan)
}

fn RevalidateCachedQuery(
    h: CachedPlanSourceHandle,
    queryEnv: QueryEnvHandle,
    release_generic: bool,
) -> PgResult<()> {
    let (requires_reval, is_valid, mut matcher) = with_source(h, |src| {
        let take = src.requires_reval && src.is_valid;
        (
            src.requires_reval,
            src.is_valid,
            if take { src.search_path.take() } else { None },
        )
    });
    if !requires_reval {
        debug_assert!(is_valid);
        return Ok(());
    }

    if is_valid {
        let matches = match matcher.as_mut() {
            Some(m) => catalog_namespace::SearchPathMatchesCurrentEnvironment(m),
            None => panic!("RevalidateCachedQuery: valid revalidatable source lost its search_path"),
        };
        // Leaving a still-valid entry with search_path=None corrupts the cache (the next access hits the None arm and panics).
        with_cache(|pc| source_mut(pc, h).search_path = matcher);
        if !matches? {
            with_cache(|pc| {
                let src = source_mut(pc, h);
                if let Some(gplan) = invalidate_source_entry(src) {
                    plan_mut(pc, gplan).is_valid = false;
                }
            });
        }
    }

    if with_source(h, |src| src.is_valid && src.depends_on_rls)
        && with_source(h, |src| {
            src.rewrite_role_id != miscinit::GetUserId()
                || src.rewrite_row_security != guc_tables::backing::row_security()
        })
    {
        invalidate_source(h);
    }

    if let Some(query_list) = with_source(h, |src| src.is_valid.then_some(src.query_list)) {
        AcquirePlannerLocks(query_list, true)?;
        if with_source(h, |src| src.is_valid) {
            return Ok(());
        }
        AcquirePlannerLocks(query_list, false)?;
    }

    let (reanalyze, post_rewrite, raw_parse_tree, analyzed_parse_tree) = with_source(h, |src| {
        (src.reanalyze, src.post_rewrite, src.raw_parse_tree, src.analyzed_parse_tree)
    });

    if release_generic {
        ReleaseGenericPlan(h);
    }
    // plancache.c:767 frees query_context before reanalysis: a failed reanalysis leaves no obsolete arena behind.
    let (reclaim, query_string, param_types, fixed_result, source_ctx) = with_cache(|pc| {
        let pinned = source_mut(pc, h).live_plans > 0;
        let src = source_mut(pc, h);
        src.is_valid = false;
        src.query_list = &[];
        src.relation_oids = &[];
        src.inval_items = &[];
        src.search_path = None;
        let old_qctx = core::mem::replace(&mut src.query_ctx, core::ptr::null_mut());
        let mut ctxs = Vec::new();
        if !old_qctx.is_null() {
            if pinned {
                src.pending_query_ctxs.push((src.generation, old_qctx));
            } else {
                ctxs.push(old_qctx);
            }
        }
        (ctxs, src.query_string, src.param_types, src.fixed_result, src.source_ctx)
    });
    for ctx in reclaim {
        reclaim_ctx(ctx);
    }

    let mut snapshot_set = false;
    if !snapmgr::ActiveSnapshotSet() {
        let snap: snapmgr::Snapshot = snapmgr::GetTransactionSnapshot()?;
        snapmgr::PushActiveSnapshot(&snap)?;
        snapshot_set = true;
    }
    let new_qctx = leak_ctx("CachedPlanQuery");
    let qmcx = ctx_mcx(new_qctx);
    // Analysis and the rewriter both scribble on their input, and a failed attempt must not poison the retained original.
    let analyzed = match (raw_parse_tree, analyzed_parse_tree) {
        (Some(raw), _) => copy_raw_stmt_in(qmcx, raw).and_then(|raw| match reanalyze {
            Some((f, arg)) => f(h, qmcx, raw, query_string, param_types, queryEnv, arg),
            None => reanalyze_fixedparams(qmcx, raw, query_string, param_types, queryEnv),
        }),
        (None, Some(q)) => rewrite_analyzed(qmcx, q),
        // Unreachable in practice, since an empty statement never requires revalidation.
        (None, None) => Ok(PgVec::new_in(qmcx)),
    }
    .and_then(|mut query_list| match post_rewrite {
        Some((f, arg)) => f(h, qmcx, &mut query_list, arg).map(|()| query_list),
        None => Ok(query_list),
    });
    if snapshot_set {
        if let Err(e) = snapmgr::PopActiveSnapshot() {
            drop(analyzed);
            reclaim_ctx(new_qctx);
            return Err(e);
        }
    }
    let rebuilt = analyzed.and_then(|query_list| {
        let query_list: &'static [Query<'static>] = mcx::vec_borrow_in(qmcx, query_list)?;
        let mut oids: PgVec<'static, Oid> = PgVec::new_in(qmcx);
        let mut items: PgVec<'static, (i32, u32)> = PgVec::new_in(qmcx);
        for q in query_list {
            extract_query_deps(q, &mut oids, &mut items)?;
        }
        let relation_oids = mcx::vec_borrow_in(qmcx, oids)?;
        let inval_items = mcx::vec_borrow_in(qmcx, items)?;
        let search_path = catalog_namespace::GetSearchPathMatcher(qmcx)?;
        let result_desc = plan_cache_compute_result_desc(ctx_mcx(source_ctx), query_list)?;
        Ok((query_list, relation_oids, inval_items, search_path, result_desc))
    });
    let (query_list, relation_oids, inval_items, search_path, result_desc) = match rebuilt {
        Ok(r) => r,
        Err(e) => {
            reclaim_ctx(new_qctx);
            return Err(e);
        }
    };

    let old_desc = with_source(h, |src| src.result_desc.clone());
    let desc_equal = match (&old_desc, &result_desc) {
        (None, None) => true,
        (Some(o), Some(n)) => tupdesc::equalRowTypes(o, n),
        _ => false,
    };
    if !desc_equal && fixed_result {
        // search_path's schemas PgVec lives in new_qctx: it must be dead before the arena is reclaimed or its Drop scribbles a freed slab.
        drop(search_path);
        reclaim_ctx(new_qctx);
        // routine=RevalidateCachedQuery is load-bearing: pgjdbc and postgres.js key their transparent re-prepare-after-DDL on this exact R field.
        return Err(ereport(ERROR)
            .errcode(types_error::ERRCODE_FEATURE_NOT_SUPPORTED)
            .errmsg("cached plan must not change result type")
            .into_error()
            .with_funcname("RevalidateCachedQuery")
            .into());
    }

    let depends_on_rls = query_list.iter().any(|q| q.hasRowSecurity);
    let rewrite_role_id = miscinit::GetUserId();
    let rewrite_row_security = guc_tables::backing::row_security();
    with_cache(|pc| {
        let src = source_mut(pc, h);
        src.query_ctx = new_qctx;
        src.query_list = query_list;
        src.relation_oids = relation_oids;
        src.inval_items = inval_items;
        src.search_path = Some(search_path);
        if !desc_equal {
            src.result_desc = result_desc;
        }
        src.depends_on_rls = depends_on_rls;
        src.rewrite_role_id = rewrite_role_id;
        src.rewrite_row_security = rewrite_row_security;
        src.is_valid = true;
    });
    let reclaim = with_cache(|pc| drain_pending_arenas(pc, h));
    for ctx in reclaim {
        reclaim_ctx(ctx);
    }
    Ok(())
}

fn copy_raw_stmt_in<'mcx>(mcx: Mcx<'mcx>, raw: &RawStmt<'_>) -> PgResult<&'mcx RawStmt<'mcx>> {
    let stmt = match raw.stmt {
        Some(s) => Some(copyfuncs::copy_object(mcx, s)?),
        None => None,
    };
    mcx::alloc_leak_in(
        mcx,
        RawStmt { stmt, stmt_location: raw.stmt_location, stmt_len: raw.stmt_len },
    )
}

/// Copies only — analysis scribbles on its input.
pub fn CachedPlanRawParseTreeCopy<'mcx>(
    mcx: Mcx<'mcx>,
    h: CachedPlanSourceHandle,
) -> PgResult<Option<&'mcx RawStmt<'mcx>>> {
    match with_source(h, |src| src.raw_parse_tree) {
        Some(raw) => Ok(Some(copy_raw_stmt_in(mcx, raw)?)),
        None => Ok(None),
    }
}

fn rewrite_analyzed(
    qmcx: Mcx<'static>,
    analyzed: &Query<'static>,
) -> PgResult<PgVec<'static, Query<'static>>> {
    // The rewriter scribbles on its input, so copy.
    let copied: Query<'static> = copyfuncs::copy_query(qmcx, analyzed)?;
    rewrite_handler_seams::acquire_rewrite_locks::call(qmcx, &copied, true, false)?;
    pg_rewrite_query(qmcx, copied)
}

fn pg_rewrite_query(
    qmcx: Mcx<'static>,
    query: Query<'static>,
) -> PgResult<PgVec<'static, Query<'static>>> {
    if guc_tables::backing::Debug_print_parse() {
        nodes_core::print::debug_print_tree(
            qmcx,
            "parse tree",
            outfuncs::queryToStringWithLocations(qmcx, &query),
            guc_tables::backing::Debug_pretty_print(),
        )?;
    }
    if guc_tables::backing::log_parser_stats() {
        postgres_seams::reset_usage::call();
    }
    let querytree_list = if query.commandType == CmdType::CMD_UTILITY {
        let mut v = PgVec::new_in(qmcx);
        v.try_reserve_exact(1).map_err(|_| qmcx.oom(1))?;
        v.push(query);
        v
    } else {
        rewrite_handler_seams::query_rewrite::call(qmcx, query)?
    };
    if guc_tables::backing::log_parser_stats() {
        postgres_seams::show_usage::call("REWRITER STATISTICS")?;
    }
    if guc_tables::backing::Debug_print_rewritten() {
        nodes_core::print::debug_print_tree(
            qmcx,
            "rewritten parse tree",
            outfuncs::queryListToStringWithLocations(qmcx, &querytree_list),
            guc_tables::backing::Debug_pretty_print(),
        )?;
    }
    Ok(querytree_list)
}

fn reanalyze_fixedparams(
    qmcx: Mcx<'static>,
    raw: &RawStmt<'static>,
    query_string: &'static str,
    param_types: &'static [Oid],
    query_env: QueryEnvHandle,
) -> PgResult<PgVec<'static, Query<'static>>> {
    if guc_tables::backing::log_parser_stats() {
        postgres_seams::reset_usage::call();
    }
    let query = analyze_seams::parse_analyze_fixedparams::call(
        qmcx,
        raw,
        query_string,
        param_types,
        query_env,
    )?;
    if guc_tables::backing::log_parser_stats() {
        postgres_seams::show_usage::call("PARSE ANALYSIS STATISTICS")?;
    }
    pg_rewrite_query(qmcx, query)
}

fn CheckCachedPlan(h: CachedPlanSourceHandle) -> PgResult<bool> {
    let user = miscinit::GetUserId();
    let Some((gplan, mut is_valid, stmt_list)) = with_cache(|pc| {
        let src = source_mut(pc, h);
        debug_assert!(src.is_valid);
        let gplan = src.gplan?;
        let plan = plan_mut(pc, gplan);
        debug_assert!(plan.refcount > 0);
        if plan.is_valid && plan.depends_on_role && plan.plan_role_id != user {
            plan.is_valid = false;
        }
        if plan.is_valid && plan.unnest_mode != guc_tables::backing::pgrust_gravity_unnest() {
            plan.is_valid = false;
        }
        Some((gplan, plan.is_valid, plan.stmt_list))
    }) else {
        return Ok(false);
    };

    if is_valid {
        AcquireExecutorLocks(stmt_list, true)?;
        is_valid = with_cache(|pc| {
            let plan = plan_mut(pc, gplan);
            if plan.is_valid
                && TransactionIdIsValid(plan.saved_xmin)
                && plan.saved_xmin != snapmgr::TransactionXmin()
            {
                plan.is_valid = false;
            }
            plan.is_valid
        });
        if is_valid {
            return Ok(true);
        }
        AcquireExecutorLocks(stmt_list, false)?;
    }

    ReleaseGenericPlan(h);
    Ok(false)
}

fn ident_prefix(query_string: &str) -> &str {
    let mut n = query_string.len().min(1024);
    while !query_string.is_char_boundary(n) {
        n -= 1;
    }
    &query_string[..n]
}

fn BuildCachedPlan(
    h: CachedPlanSourceHandle,
    boundParams: ParamListHandle,
    queryEnv: QueryEnvHandle,
) -> PgResult<CachedPlanHandle> {
    // Invalidated since GetCachedPlan's revalidation: re-analyze without releasing the generic plan the caller may hold.
    if !with_source(h, |src| src.is_valid) {
        RevalidateCachedQuery(h, queryEnv, false)?;
    }
    let (query_list, query_string, cursor_options, requires_snapshot) = with_source(h, |src| {
        (src.query_list, src.query_string, src.cursor_options, src.requires_snapshot)
    });

    let plan_ctx = leak_ctx("CachedPlan");
    ctx_ref(plan_ctx).set_ident(Some(ident_prefix(query_string)));
    let result = build_stmt_list(
        ctx_mcx(plan_ctx),
        query_list,
        query_string,
        cursor_options,
        requires_snapshot,
        boundParams,
    );
    let stmt_list = match result {
        Ok(list) => list,
        Err(e) => {
            reclaim_ctx(plan_ctx);
            return Err(e);
        }
    };
    let copy_stmts = |m: Mcx<'static>| -> PgResult<&'static [PlannedStmt<'static>]> {
        let mut v: PgVec<'static, PlannedStmt<'static>> = PgVec::new_in(m);
        v.try_reserve_exact(stmt_list.len()).map_err(|_| m.oom(stmt_list.len()))?;
        for s in stmt_list {
            v.push(copyfuncs::copy_planned_stmt(m, s)?);
        }
        mcx::vec_borrow_in(m, v)
    };
    let (plan_ctx, stmt_list) = match compact_into(plan_ctx, "CachedPlan", copy_stmts) {
        Ok(Some((compacted, list))) => {
            ctx_ref(compacted).set_ident(Some(ident_prefix(query_string)));
            reclaim_ctx(plan_ctx);
            (compacted, list)
        }
        Ok(None) => (plan_ctx, stmt_list),
        Err(e) => {
            reclaim_ctx(plan_ctx);
            return Err(e);
        }
    };

    let mut depends_on_role = with_source(h, |src| src.depends_on_rls);
    let mut is_transient = false;
    for stmt in stmt_list {
        if stmt.commandType == CmdType::CMD_UTILITY {
            continue;
        }
        is_transient |= stmt.transientPlan;
        depends_on_role |= stmt.dependsOnRole;
    }
    let saved_xmin = if is_transient {
        snapmgr::TransactionXmin()
    } else {
        InvalidTransactionId
    };
    let plan_role_id = miscinit::GetUserId();
    let unnest_mode = guc_tables::backing::pgrust_gravity_unnest();

    Ok(with_cache(|pc| {
        pc.handle_gen = pc.handle_gen.wrapping_add(1);
        let src = source_mut(pc, h);
        src.generation += 1;
        src.live_plans += 1;
        let generation = src.generation;
        let plan = CachedPlan {
            handle_gen: pc.handle_gen,
            source: h,
            stmt_list,
            plan_role_id,
            depends_on_role,
            unnest_mode,
            saved_xmin,
            refcount: 0,
            generation,
            is_saved: false,
            is_valid: true,
            plan_ctx,
        };
        let idx = match pc.plan_free.pop() {
            Some(i) => {
                pc.plans[i as usize] = Some(plan);
                i
            }
            None => {
                pc.plans.push(Some(plan));
                (pc.plans.len() - 1) as u32
            }
        };
        CachedPlanHandle(encode(idx, pc.handle_gen))
    }))
}

fn build_stmt_list(
    mcx: Mcx<'static>,
    query_list: &[Query<'static>],
    query_string: &'static str,
    cursor_options: i32,
    requires_snapshot: bool,
    boundParams: ParamListHandle,
) -> PgResult<&'static [PlannedStmt<'static>]> {
    let mut snapshot_set = false;
    if !snapmgr::ActiveSnapshotSet() && requires_snapshot {
        let snap: snapmgr::Snapshot = snapmgr::GetTransactionSnapshot()?;
        snapmgr::PushActiveSnapshot(&snap)?;
        snapshot_set = true;
    }

    let plan_one = |pmcx: Mcx<'static>, q: &Query<'static>| -> PgResult<PlannedStmt<'static>> {
        let input = mcx::leak_in(mcx::alloc_in(pmcx, copy_query_in(pmcx, q)?)?);
        if guc_tables::backing::log_planner_stats() {
            postgres_seams::reset_usage::call();
        }
        let plan =
            planner_seams::planner::call(pmcx, input, query_string, cursor_options, boundParams)?;
        if guc_tables::backing::log_planner_stats() {
            postgres_seams::show_usage::call("PLANNER STATISTICS")?;
        }
        if guc_tables::backing::Debug_print_plan() {
            nodes_core::print::debug_print_tree(
                pmcx,
                "plan",
                outfuncs::plannedStmtToStringWithLocations(pmcx, &plan),
                guc_tables::backing::Debug_pretty_print(),
            )?;
        }
        Ok(plan)
    };
    let plan = |mcx: Mcx<'static>| -> PgResult<PgVec<'static, PlannedStmt<'static>>> {
        let mut stmts: PgVec<'static, PlannedStmt<'static>> = PgVec::new_in(mcx);
        stmts
            .try_reserve_exact(query_list.len())
            .map_err(|_| mcx.oom(query_list.len()))?;
        for q in query_list {
            if q.commandType == CmdType::CMD_UTILITY {
                stmts.push(PlannedStmt {
                    commandType: CmdType::CMD_UTILITY,
                    canSetTag: q.canSetTag,
                    utilityStmt: q.utilityStmt,
                    stmt_location: q.stmt_location,
                    stmt_len: q.stmt_len,
                    queryId: ::types_nodes::SyncCell::new(q.queryId),
                    ..PlannedStmt::default()
                });
            } else {
                let scratch = Box::into_raw(Box::new(MemoryContext::new("CachedPlan planning")));
                let planned = plan_one(ctx_mcx(scratch), q).and_then(|p| copyfuncs::copy_planned_stmt(mcx, &p));
                reclaim_ctx(scratch);
                stmts.push(planned?);
            }
        }
        Ok(stmts)
    };
    let result = plan(mcx);

    if snapshot_set {
        snapmgr::PopActiveSnapshot()?;
    }

    mcx::vec_borrow_in(mcx, result?)
}

// The planner writes plan-arena pointers into its input tree; a subtree shared with the cached query_list dangles once that plan's arena is reclaimed, so the copy must share no storage with the source.
fn copy_query_in(mcx: Mcx<'static>, q: &Query<'static>) -> PgResult<Query<'static>> {
    copyfuncs::copy_query(mcx, q)
}

fn choose_custom_plan(h: CachedPlanSourceHandle, boundParams: ParamListHandle) -> bool {
    if with_source(h, |src| src.is_oneshot) {
        return true;
    }
    if boundParams.is_null() {
        return false;
    }
    with_source(h, |src| {
        if !src.requires_reval {
            return false;
        }
        let mode = guc_tables::vars::plan_cache_mode.read();
        if mode == guc_tables::consts::PLAN_CACHE_MODE_FORCE_GENERIC_PLAN {
            return false;
        }
        if mode == guc_tables::consts::PLAN_CACHE_MODE_FORCE_CUSTOM_PLAN {
            return true;
        }
        if src.cursor_options & CURSOR_OPT_GENERIC_PLAN != 0 {
            return false;
        }
        if src.cursor_options & CURSOR_OPT_CUSTOM_PLAN != 0 {
            return true;
        }
        if src.num_custom_plans < 5 {
            return true;
        }
        let avg_custom_cost = src.total_custom_cost / src.num_custom_plans as f64;
        if src.generic_cost < avg_custom_cost {
            return false;
        }
        true
    })
}

fn cached_plan_cost(stmt_list: &[PlannedStmt<'_>], include_planner: bool) -> f64 {
    let mut result = 0.0;
    for stmt in stmt_list {
        if stmt.commandType == CmdType::CMD_UTILITY {
            continue;
        }
        if let Some(tree) = stmt.planTree {
            result += tree.as_plan().expect("planTree is a plan node").total_cost;
        }
        if include_planner {
            let nrelations = stmt.rtable.len() as f64;
            result = planner_overhead_cost(
                result,
                guc_tables::vars::cpu_operator_cost.read(),
                nrelations,
            );
        }
    }
    result
}

fn AcquirePlannerLocks(query_list: &[Query<'static>], acquire: bool) -> PgResult<()> {
    for query in query_list {
        if query.commandType == CmdType::CMD_UTILITY {
            let contained = query
                .utilityStmt
                .and_then(|s| utility_seams::utility_contains_query::call(s));
            if let Some(q) = contained {
                ScanQueryForLocks(q.as_query().expect("contained Query"), acquire)?;
            }
            continue;
        }
        ScanQueryForLocks(query, acquire)?;
    }
    Ok(())
}

fn AcquireExecutorLocks(stmt_list: &[PlannedStmt<'static>], acquire: bool) -> PgResult<()> {
    for stmt in stmt_list {
        if stmt.commandType == CmdType::CMD_UTILITY {
            // Unplanned Query inside EXPLAIN/DECLARE still needs its locks.
            let contained = stmt
                .utilityStmt
                .and_then(|s| utility_seams::utility_contains_query::call(s));
            if let Some(q) = contained {
                ScanQueryForLocks(q.as_query().expect("contained Query"), acquire)?;
            }
            continue;
        }
        for rte_node in stmt.rtable.iter() {
            let rte = rte_node.as_range_tbl_entry().expect("rtable holds RangeTblEntry");
            let lockable = rte.rtekind == RTEKind::RTE_RELATION
                || (rte.rtekind == RTEKind::RTE_SUBQUERY && rte.relid != InvalidOid);
            if !lockable {
                continue;
            }
            lock_relation(rte, acquire)?;
        }
    }
    Ok(())
}

fn lock_relation(rte: &RangeTblEntry<'_>, acquire: bool) -> PgResult<()> {
    if acquire {
        lmgr::LockRelationOid(rte.relid, rte.rellockmode)
    } else {
        lmgr::UnlockRelationOid(rte.relid, rte.rellockmode)
    }
}

fn ScanQueryForLocks(query: &Query<'static>, acquire: bool) -> PgResult<()> {
    debug_assert!(query.commandType != CmdType::CMD_UTILITY);
    for rte_node in query.rtable.iter() {
        let rte = rte_node.as_range_tbl_entry().expect("rtable holds RangeTblEntry");
        match rte.rtekind {
            RTEKind::RTE_RELATION => lock_relation(rte, acquire)?,
            RTEKind::RTE_SUBQUERY => {
                if rte.relid != InvalidOid {
                    lock_relation(rte, acquire)?;
                }
                ScanQueryForLocks(rte.subquery.expect("RTE_SUBQUERY has subquery"), acquire)?;
            }
            _ => {}
        }
    }
    for cte_node in &query.cteList {
        let cte = cte_node.as_common_table_expr().expect("cteList cell");
        let ctequery = cte.ctequery.and_then(|n| n.as_query()).expect("analyzed CTE query");
        ScanQueryForLocks(ctequery, acquire)?;
    }
    if query.hasSubLinks {
        walk_sublink_queries(query, &mut |sub| ScanQueryForLocks(sub, acquire))?;
    }
    Ok(())
}

fn walk_sublink_queries<F>(query: &Query<'static>, f: &mut F) -> PgResult<()>
where
    F: FnMut(&Query<'static>) -> PgResult<()>,
{
    walk_query_expr_nodes(query, &mut |node| {
        if let Some(sl) = node.as_sub_link() {
            f(sl.subselect.as_query().expect("analyzed sublink sub-select"))?;
        }
        Ok(())
    })
}

fn walk_query_expr_nodes<F>(query: &Query<'static>, f: &mut F) -> PgResult<()>
where
    F: FnMut(types_nodes::Node<'static>) -> PgResult<()>,
{
    struct W<'a, F> {
        f: &'a mut F,
    }
    impl<F> nodes_core::NodeWalker<'static> for W<'_, F>
    where
        F: FnMut(types_nodes::Node<'static>) -> PgResult<()>,
    {
        fn visit(&mut self, node: types_nodes::Node<'static>) -> PgResult<bool> {
            (self.f)(node)?;
            nodes_core::expression_tree_walker(node, self)
        }
    }
    let mut w = W { f };
    nodes_core::query_tree_walker(query, &mut w, nodes_core::QTW_IGNORE_RC_SUBQUERIES)?;
    Ok(())
}

// Built-ins below it are immutable, untracked.
const FIRST_UNPINNED_OBJECT_ID: Oid = 12000;

fn syscache_oid_hash(cache_id: i32, oid: Oid) -> PgResult<u32> {
    cache_syscache::GetSysCacheHashValue(
        cache_id,
        cache_syscache::SysCacheKey::Value(Datum::from_oid(oid)),
        cache_syscache::SysCacheKey::UNUSED,
        cache_syscache::SysCacheKey::UNUSED,
        cache_syscache::SysCacheKey::UNUSED,
    )
}

fn push_proc_dep(items: &mut PgVec<'static, (i32, u32)>, funcid: Oid) -> PgResult<()> {
    if funcid < FIRST_UNPINNED_OBJECT_ID {
        return Ok(());
    }
    let hash = syscache_oid_hash(PROCOID, funcid)?;
    items
        .try_reserve(1)
        .map_err(|_| Box::new(items.allocator().oom(core::mem::size_of::<(i32, u32)>())))?;
    items.push((PROCOID, hash));
    Ok(())
}

// OIDOID is accepted because oideq-style folding coerces regclass Consts to it.
fn record_one_dep(
    node: types_nodes::Node<'static>,
    out: &mut PgVec<'static, Oid>,
    items: &mut PgVec<'static, (i32, u32)>,
) -> PgResult<()> {
    if let Some(c) = node.as_const() {
        if (c.consttype == REGCLASSOID || c.consttype == types_core::OIDOID) && !c.constisnull {
            out.try_reserve(1).map_err(|_| mcx_oom(out))?;
            out.push(c.constvalue.as_u32());
        }
    } else if let Some(sl) = node.as_sub_link() {
        extract_query_deps(
            sl.subselect.as_query().expect("analyzed sublink sub-select"),
            out,
            items,
        )?;
    } else if let Some(f) = node.as_func_expr() {
        push_proc_dep(items, f.funcid)?;
    } else if let Some(o) = node.as_op_expr() {
        push_proc_dep(items, o.opfuncid)?;
    } else if let Some(o) = node.as_distinct_expr() {
        push_proc_dep(items, o.opfuncid)?;
    } else if let Some(o) = node.as_null_if_expr() {
        push_proc_dep(items, o.opfuncid)?;
    } else if let Some(sa) = node.as_scalar_array_op_expr() {
        push_proc_dep(items, sa.opfuncid)?;
    } else if let Some(a) = node.as_aggref() {
        push_proc_dep(items, a.aggfnoid)?;
    } else if let Some(w) = node.as_window_func() {
        push_proc_dep(items, w.winfnoid)?;
    }
    Ok(())
}

fn record_expr_tree_deps(
    node: types_nodes::Node<'static>,
    out: &mut PgVec<'static, Oid>,
    items: &mut PgVec<'static, (i32, u32)>,
) -> PgResult<()> {
    struct W<'a> {
        out: &'a mut PgVec<'static, Oid>,
        items: &'a mut PgVec<'static, (i32, u32)>,
    }
    impl nodes_core::NodeWalker<'static> for W<'_> {
        fn visit(&mut self, node: types_nodes::Node<'static>) -> PgResult<bool> {
            record_one_dep(node, self.out, self.items)?;
            nodes_core::expression_tree_walker(node, self)
        }
    }
    let mut w = W { out, items };
    nodes_core::NodeWalker::visit(&mut w, node)?;
    Ok(())
}

fn extract_query_deps(
    query: &Query<'static>,
    out: &mut PgVec<'static, Oid>,
    items: &mut PgVec<'static, (i32, u32)>,
) -> PgResult<()> {
    // The outer utility Query has an empty rtable, so its dependencies live in the utility statement.
    if query.commandType == CmdType::CMD_UTILITY {
        let ustmt = query.utilityStmt.expect("CMD_UTILITY query has utilityStmt");
        if let Some(callstmt) = ustmt.as_call_stmt() {
            if let Some(fe) = callstmt.funcexpr {
                push_proc_dep(items, fe.funcid)?;
                for arg in fe.args.iter() {
                    record_expr_tree_deps(arg, out, items)?;
                }
            }
            for outarg in callstmt.outargs.iter() {
                record_expr_tree_deps(outarg, out, items)?;
            }
            return Ok(());
        }
        if let Some(contained) = utility_seams::utility_contains_query::call(ustmt) {
            extract_query_deps(
                contained.as_query().expect("contained analyzed Query"),
                out,
                items,
            )?;
        }
        return Ok(());
    }
    for cte_node in &query.cteList {
        let cte = cte_node.as_common_table_expr().expect("cteList cell");
        let ctequery = cte.ctequery.and_then(|n| n.as_query()).expect("analyzed CTE query");
        extract_query_deps(ctequery, out, items)?;
    }
    walk_query_expr_nodes(query, &mut |node| record_one_dep(node, out, items))?;
    for rte_node in query.rtable.iter() {
        let rte = rte_node.as_range_tbl_entry().expect("rtable holds RangeTblEntry");
        match rte.rtekind {
            RTEKind::RTE_RELATION => {
                out.try_reserve(1).map_err(|_| mcx_oom(out))?;
                out.push(rte.relid);
            }
            RTEKind::RTE_SUBQUERY => {
                if rte.relid != InvalidOid {
                    out.try_reserve(1).map_err(|_| mcx_oom(out))?;
                    out.push(rte.relid);
                }
                extract_query_deps(
                    rte.subquery.expect("RTE_SUBQUERY has subquery"),
                    out,
                    items,
                )?;
            }
            // A transition-table RTE carries the base relation's OID so ALTERing that table invalidates dependent plans.
            RTEKind::RTE_NAMEDTUPLESTORE => {
                if rte.relid != InvalidOid {
                    out.try_reserve(1).map_err(|_| mcx_oom(out))?;
                    out.push(rte.relid);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn mcx_oom(v: &PgVec<'static, Oid>) -> Box<types_error::PgError> {
    Box::new(v.allocator().oom(core::mem::size_of::<Oid>()))
}

fn choose_portal_strategy_queries(query_list: &[Query<'static>]) -> PortalStrategy {
    use PortalStrategy::*;
    if query_list.len() == 1 {
        let query = &query_list[0];
        if query.canSetTag {
            if query.commandType == CmdType::CMD_SELECT {
                if query.hasModifyingCTE {
                    return PORTAL_ONE_MOD_WITH;
                }
                return PORTAL_ONE_SELECT;
            }
            if query.commandType == CmdType::CMD_UTILITY {
                let u = query.utilityStmt.expect("CMD_UTILITY query has utilityStmt");
                if utility_seams::utility_returns_tuples::call(u) {
                    return PORTAL_UTIL_SELECT;
                }
                return PORTAL_MULTI_QUERY;
            }
        }
    }

    let mut n_set_tag = 0i32;
    for query in query_list {
        if query.canSetTag {
            n_set_tag += 1;
            if n_set_tag > 1 {
                return PORTAL_MULTI_QUERY;
            }
            if query.commandType == CmdType::CMD_UTILITY || query.returningList.is_nil() {
                return PORTAL_MULTI_QUERY;
            }
        }
    }
    if n_set_tag == 1 {
        return PORTAL_ONE_RETURNING;
    }
    PORTAL_MULTI_QUERY
}

fn plan_cache_compute_result_desc(
    mcx: Mcx<'static>,
    query_list: &[Query<'static>],
) -> PgResult<Option<Rc<TupleDescData<'static>>>> {
    use PortalStrategy::*;
    match choose_portal_strategy_queries(query_list) {
        PORTAL_ONE_SELECT | PORTAL_ONE_MOD_WITH => Ok(Some(execscan::exec_clean_type_from_tl(
            mcx,
            &query_list[0].targetList,
        )?)),
        PORTAL_ONE_RETURNING => {
            let query = query_list
                .iter()
                .find(|q| q.canSetTag)
                .expect("ONE_RETURNING has a canSetTag query");
            Ok(Some(execscan::exec_clean_type_from_tl(mcx, &query.returningList)?))
        }
        PORTAL_UTIL_SELECT => {
            let u = query_list[0].utilityStmt.expect("CMD_UTILITY query has utilityStmt");
            utility_seams::utility_tuple_descriptor::call(u)
        }
        PORTAL_MULTI_QUERY => Ok(None),
    }
}

fn is_transaction_exit_stmt(raw: &RawStmt<'_>) -> bool {
    use types_nodes::parsenodes::TransactionStmtKind::*;
    match raw.stmt.and_then(|node| node.as_transaction_stmt()) {
        Some(stmt) => matches!(
            stmt.kind,
            TRANS_STMT_COMMIT | TRANS_STMT_PREPARE | TRANS_STMT_ROLLBACK | TRANS_STMT_ROLLBACK_TO
        ),
        None => false,
    }
}

pub fn CachedPlanGetTargetList<'mcx>(
    mcx: Mcx<'mcx>,
    h: CachedPlanSourceHandle,
    queryEnv: QueryEnvHandle,
) -> PgResult<PgVec<'mcx, pquery_seams::TargetEntrySummary>> {
    let mut out: PgVec<'mcx, pquery_seams::TargetEntrySummary> = PgVec::new_in(mcx);
    if with_source(h, |src| src.result_desc.is_none()) {
        return Ok(out);
    }

    RevalidateCachedQuery(h, queryEnv, true)?;

    let query_list = with_source(h, |src| src.query_list);
    let primary = query_list
        .iter()
        .find(|q| q.canSetTag)
        .expect("QueryListGetPrimaryStmt: fixed-result source has a canSetTag query");
    if primary.commandType == CmdType::CMD_UTILITY {
        return pquery_seams::fetch_utility_statement_target_list::call(mcx, primary.utilityStmt);
    }
    let tlist = if primary.commandType == CmdType::CMD_SELECT {
        &primary.targetList
    } else if !primary.returningList.is_nil() {
        &primary.returningList
    } else {
        return Ok(out);
    };
    out.try_reserve(tlist.len()).map_err(|_| mcx.oom(tlist.len()))?;
    for node in tlist.iter() {
        let tle = node.as_target_entry().expect("targetlist entry is a TargetEntry");
        out.push(pquery_seams::TargetEntrySummary {
            resjunk: tle.resjunk,
            resorigtbl: tle.resorigtbl,
            resorigcol: tle.resorigcol,
        });
    }
    Ok(out)
}

pub fn CachedPlanParamTypes(h: CachedPlanSourceHandle) -> &'static [Oid] {
    with_source(h, |src| src.param_types)
}

/// Valid while the caller holds the source handle.
pub fn CachedPlanQueryList(h: CachedPlanSourceHandle) -> &'static [Query<'static>] {
    with_source(h, |src| src.query_list)
}

pub fn CachedPlanRequiresSnapshot(h: CachedPlanSourceHandle) -> bool {
    with_source(h, |src| src.requires_snapshot)
}

pub fn CachedPlanIsTransactionExitStmt(h: CachedPlanSourceHandle) -> bool {
    with_source(h, |src| src.is_xact_exit_stmt)
}

pub fn CachedPlanCounts(h: CachedPlanSourceHandle) -> (i64, i64) {
    with_source(h, |src| (src.num_generic_plans, src.num_custom_plans))
}

pub fn CachedPlanResultDesc(h: CachedPlanSourceHandle) -> Option<Rc<TupleDescData<'static>>> {
    with_source(h, |src| src.result_desc.clone())
}

pub fn CachedPlanCommandTag(h: CachedPlanSourceHandle) -> CommandTag {
    with_source(h, |src| src.commandTag)
}

pub fn CachedPlanQueryString(h: CachedPlanSourceHandle) -> &'static str {
    with_source(h, |src| src.query_string)
}

pub fn CachedPlanRawParseTree(h: CachedPlanSourceHandle) -> Option<&'static RawStmt<'static>> {
    with_source(h, |src| src.raw_parse_tree)
}

pub fn CachedPlanNumParams(h: CachedPlanSourceHandle) -> usize {
    with_source(h, |src| src.param_types.len())
}

pub fn CachedPlanFixedResult(h: CachedPlanSourceHandle) -> bool {
    with_source(h, |src| src.fixed_result)
}

#[derive(Clone, Copy)]
pub struct SourceExecInfo {
    pub fixed_result: bool,
    pub num_params: usize,
    pub query_string: &'static str,
    pub commandTag: CommandTag,
}

pub fn CachedPlanSourceExecInfo(h: CachedPlanSourceHandle) -> SourceExecInfo {
    with_source(h, |src| SourceExecInfo {
        fixed_result: src.fixed_result,
        num_params: src.param_types.len(),
        query_string: src.query_string,
        commandTag: src.commandTag,
    })
}

pub fn CachedPlanGeneration(cplan: CachedPlanHandle) -> i32 {
    with_plan(cplan, |plan| plan.generation)
}

/// Caller must hold a refcount on `cplan`.
pub fn CachedPlanIdentity(cplan: CachedPlanHandle) -> (u64, i64, bool) {
    with_cache(|pc| {
        let (source, generation) = {
            let plan = plan_mut(pc, cplan);
            (plan.source, plan.generation as i64)
        };
        let (idx, gen) = decode(source.0);
        let generic = pc
            .sources
            .get(idx)
            .and_then(Option::as_ref)
            .is_some_and(|s| s.handle_gen == gen && s.gplan == Some(cplan));
        (source.0, generation, generic)
    })
}

fn invalidate_source_entry(src: &mut CachedPlanSource) -> Option<CachedPlanHandle> {
    src.is_valid = false;
    src.gplan
}

fn invalidate_source(h: CachedPlanSourceHandle) {
    with_cache(|pc| {
        if let Some(gplan) = invalidate_source_entry(source_mut(pc, h)) {
            plan_mut(pc, gplan).is_valid = false;
        }
    });
}

pub fn PlanCacheRelCallback(_arg: Datum, relid: Oid) {
    bump_inval_counter();
    with_cache(|pc| {
        for i in 0..pc.saved_plan_list.len() {
            let h = pc.saved_plan_list[i];
            let (hit, gplan) = {
                let src = source_mut(pc, h);
                if !src.is_valid || !src.requires_reval {
                    continue;
                }
                let hit = if relid == InvalidOid {
                    !src.relation_oids.is_empty()
                } else {
                    src.relation_oids.contains(&relid)
                };
                if hit {
                    src.is_valid = false;
                }
                (hit, src.gplan)
            };
            let Some(gplan) = gplan else { continue };
            let plan = plan_mut(pc, gplan);
            if hit {
                plan.is_valid = false;
                continue;
            }
            // The generic plan can have more dependencies than the querytree.
            if plan.is_valid {
                let plan_hit = plan.stmt_list.iter().any(|stmt| {
                    stmt.commandType != CmdType::CMD_UTILITY
                        && if relid == InvalidOid {
                            !stmt.relationOids.is_nil()
                        } else {
                            stmt.relationOids.as_slice().contains(&relid)
                        }
                });
                if plan_hit {
                    plan.is_valid = false;
                }
            }
        }
    });
}

pub fn PlanCacheObjectCallback(_arg: Datum, cacheid: i32, hashvalue: u32) {
    bump_inval_counter();
    with_cache(|pc| {
        for i in 0..pc.saved_plan_list.len() {
            let h = pc.saved_plan_list[i];
            let gplan = {
                let src = source_mut(pc, h);
                if !src.is_valid || !src.requires_reval {
                    continue;
                }
                if src.inval_items.iter().any(|&(cid, hv)| {
                    cid == cacheid && (hashvalue == 0 || hv == hashvalue)
                }) {
                    if let Some(gplan) = invalidate_source_entry(src) {
                        plan_mut(pc, gplan).is_valid = false;
                    }
                    continue;
                }
                src.gplan
            };
            let Some(gplan) = gplan else { continue };
            let plan = plan_mut(pc, gplan);
            if plan.is_valid && stmt_list_matches_inval(plan.stmt_list, cacheid, hashvalue) {
                plan.is_valid = false;
            }
        }
    });
}

// hashvalue == 0 matches every entry of the cache.
fn stmt_list_matches_inval(stmt_list: &[PlannedStmt<'_>], cacheid: i32, hashvalue: u32) -> bool {
    stmt_list.iter().any(|stmt| {
        stmt.commandType != CmdType::CMD_UTILITY
            && stmt.invalItems.iter().any(|node| {
                let item =
                    node.as_plan_inval_item().expect("invalItems holds PlanInvalItem");
                item.cacheId == cacheid && (hashvalue == 0 || item.hashValue == hashvalue)
            })
    })
}

// Shared-catalog invals reach every backend, so only the role-dependent plans drop.
pub fn PlanCacheRoleCallback(_arg: Datum, cacheid: i32, hashvalue: u32) {
    if cacheid == DATABASEOID && hashvalue != adt_acl::cached_db_hash() && hashvalue != 0 {
        return;
    }
    with_cache(|pc| {
        for i in 0..pc.saved_plan_list.len() {
            let h = pc.saved_plan_list[i];
            let (rls, gplan) = {
                let src = source_mut(pc, h);
                if !src.is_valid || !src.requires_reval {
                    continue;
                }
                if src.depends_on_rls {
                    (true, invalidate_source_entry(src))
                } else {
                    (false, src.gplan)
                }
            };
            let Some(gplan) = gplan else { continue };
            let plan = plan_mut(pc, gplan);
            if rls || plan.depends_on_role {
                plan.is_valid = false;
            }
        }
    });
}

pub fn PlanCacheSysCallback(_arg: Datum, _cacheid: i32, _hashvalue: u32) {
    ResetPlanCache();
}

pub fn ResetPlanCache() {
    bump_inval_counter();
    with_cache(|pc| {
        for i in 0..pc.saved_plan_list.len() {
            let h = pc.saved_plan_list[i];
            let src = source_mut(pc, h);
            // Never invalidate what a new parse/plan cycle can't change — particularly ROLLBACK.
            if !src.is_valid || !src.requires_reval {
                continue;
            }
            if let Some(gplan) = invalidate_source_entry(src) {
                plan_mut(pc, gplan).is_valid = false;
            }
        }
    });
    if plancache_portal_seams::engine_plan_cache_reset::is_installed() {
        plancache_portal_seams::engine_plan_cache_reset::call();
    }
}

pub fn PlanCacheCensus() -> (usize, usize, usize) {
    with_cache(|pc| {
        (
            pc.sources.iter().flatten().count(),
            pc.plans.iter().flatten().count(),
            pc.saved_plan_list.len(),
        )
    })
}

#[inline]
pub(crate) fn planner_overhead_cost(result: f64, cpu_operator_cost: f64, nrelations: f64) -> f64 {
    fp_contract::fmadd(1000.0 * cpu_operator_cost, nrelations + 1.0, result)
}


#[cfg(test)]
mod d400_fma_site_tests {
    use super::*;

    fn pick(fused: u64, unfused: u64) -> u64 {
        if cfg!(target_arch = "aarch64") { fused } else { unfused }
    }

    #[test]
    fn d400_planner_overhead_cost_fuses() {
        let got = planner_overhead_cost(3001.19, 0.010867, 17.0);
        assert_eq!(got.to_bits(), pick(0x40A8F9978D4FDF3B, 0x40A8F9978D4FDF3C));
    }
}
