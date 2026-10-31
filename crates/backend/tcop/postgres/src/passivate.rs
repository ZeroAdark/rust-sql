//! D3.4 idle passivation (docs/design/connection-scaling.md).
//!
//! After `idle_passivate_timeout` seconds idle outside a transaction, a
//! backend drops its L1 caches and returns retained allocator memory; the
//! first query after passivation rebuilds through the shared L2 (Arc adopt,
//! µs per entry) instead of catalog scans.
//!
//! # Trigger mechanics
//!
//! The backend's idle block is the FeBeWaitSet wait inside `secure_read`
//! (be_secure), entered from `ReadCommand` with an infinite timeout. Rather
//! than threading a timeout parameter down that stack, passivation rides the
//! existing timeout engine exactly like IDLE_STATS_UPDATE_TIMEOUT: the
//! ready-for-query path arms a one-shot IDLE_PASSIVATE_TIMEOUT when the
//! session goes truly idle; the shared timer thread sets MyLatch; the wait
//! loop wakes on WL_LATCH_SET and runs ProcessClientReadInterrupt →
//! ProcessInterrupts, whose arm calls [`IdlePassivate`] once (the timer is
//! one-shot and only re-armed by the next ready-for-query) and the backend
//! goes right back to waiting.
//!
//! # What passivation drops (and what survives)
//!
//! - plancache: generic plan bodies of saved sources; the sources themselves
//!   (prepared-statement identity, query text, parse trees) survive and
//!   replan on next execution — C's behavior after any plancache inval.
//! - catcache: full ResetCatalogCaches — every unpinned tuple and CatCList
//!   freed (pinned ones survive marked dead; at idle-not-in-txn there should
//!   be none).
//! - relcache: every unpinned, un-nailed, no-subxact-state entry plus the
//!   derived side caches (rules/RLS/indexattr/statext/fkey/deform-JIT) and
//!   the L2 mirror registry. Nailed entries survive (their bulk aliases L2
//!   cores anyway since the init-file routing).
//! - typcache and the other small per-session caches: kept (census: <8KB
//!   combined; their entries are woven into fn_extra/plan state and are not
//!   safely droppable from here).
//! - mcx: per-thread retained pools (parked aset keeper blocks, Acct nodes,
//!   children-vecs) freed, then the allocator release hook (mi_collect)
//!   returns freed-but-retained segments. In-context aset freelist retention
//!   (the CacheMemoryContext high-water) is NOT released — that would need
//!   per-chunk block back-pointers on the hot dealloc path (see
//!   mcx::passivate_trim).
//! - thread stack madvise: deliberately SKIPPED for the PoC — releasing
//!   dirtied stack pages under a live thread is platform-specific
//!   (MADV_FREE vs MADV_DONTNEED semantics differ between macOS and Linux,
//!   and both race the guard-page/red-zone layout Rust's runtime assumes);
//!   the win is real (~stack high-water per idle conn) but belongs to a
//!   dedicated, carefully-gated pass.

use types_error::PgResult;

/// Effective passivation delay in seconds; 0 = off. GUC
/// `idle_passivate_timeout` (PGC_SIGHUP), with a PGRUST_IDLE_PASSIVATE_SECS
/// env override for harnesses (PoC knob precedent: PGRUST_RELCACHE_CAP).
pub(crate) fn idle_passivate_secs() -> i32 {
    static ENV: std::sync::OnceLock<Option<i32>> = std::sync::OnceLock::new();
    if let Some(v) = *ENV.get_or_init(|| {
        std::env::var("PGRUST_IDLE_PASSIVATE_SECS").ok().and_then(|v| v.trim().parse().ok())
    }) {
        return v;
    }
    guc_tables::backing::idle_passivate_timeout()
}

/// D3652 P8 (D3.1 caps, docs/design/connection-scaling.md): the L1 cap trims run here, after the statement's
/// ReadyForQuery, instead of on each catcache insertion and at each transaction end (PostgresMain sets both caches to
/// trim at idle). Same caps, same eviction and exemptions: a pinned or held entry, an open relation, and an entry
/// with in-transaction subid state stay. The relcache trim waits for the end of the transaction as before (inside a
/// transaction block it runs at the first idle point after COMMIT / ROLLBACK); the catcache trim runs at every idle
/// point, as the insert-time trim ran inside transactions too.
pub(crate) fn trim_caches_at_idle() -> PgResult<()> {
    if !xact::IsTransactionOrTransactionBlock() {
        relcache::TrimRelationCacheAtIdle()?;
    }
    catcache::TrimCatCacheAtIdle();
    Ok(())
}

/// Passivate this backend: see the module doc. Caller (the ProcessInterrupts
/// arm) guarantees DoingCommandRead && !IsTransactionOrTransactionBlock.
#[cold]
#[inline(never)]
pub(crate) fn IdlePassivate() -> PgResult<()> {
    debug_assert!(!xact::IsTransactionOrTransactionBlock());
    let plans = plancache::ReleaseIdleGenericPlans();
    catcache::ResetCatalogCaches()?;
    let rels = relcache::PassivateRelationCache()?;
    // Wave-4 arena diet: the operator-lookup memo rebuilds from catcache on
    // next use (same rebuild-through-L2 contract as the caches above).
    parse_oper::PassivateOprCache();
    let trimmed = mcx::passivate_trim();
    // pppress card 10: what the trims freed of this session's small runs' tails, the rest a timer slice on
    let (_, more) = pagepool::idle_tails(false);
    arm_after_tails(more, 0)?;
    // Wave-4 stack discipline: after every cache drop (the drops themselves
    // dip the stack), release the dead dirty stack pages below the current
    // frame. Safety argument + platform gating: stack_mem module doc.
    let stack = crate::stack_mem::release_idle_stack();
    elog::elog(
        types_error::DEBUG1,
        format!(
            "idle passivation: dropped {rels} relcache entries, {plans} generic plans, \
             reset catcache, allocator trim{}, stack release {stack} bytes",
            if trimmed { "" } else { " (no release hook)" }
        ),
    )?;
    Ok(())
}

// True while the armed IDLE_PASSIVATE_TIMEOUT is the earlier idle-release
// stage (pgrust.memory_release_idle); false when it is passivation's own.
std::thread_local! {
    static RELEASE_STAGE: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    // pppress card 10: an idle tail cut stopped at its budget with runs left, so the timer's next fire goes on with it;
    // the ms of the passivation delay to arm once it is done (0: none). -1: no cut pending.
    static TAIL_REST: core::cell::Cell<i32> = const { core::cell::Cell::new(-1) };
}

pub(crate) fn reset_release_stage() {
    RELEASE_STAGE.with(|c| c.set(false));
    TAIL_REST.with(|c| c.set(-1));
}

/// pppress card 10: the idle timer after a tail cut. `mempolicy::keep::IDLE_TAIL_SLICE_MS` while it stopped with runs
/// left (the passivation delay's `rest` then starts once the cut is done), else `rest` (0: nothing).
fn arm_after_tails(more: bool, rest: i32) -> PgResult<()> {
    // The idle timer has fired. Share current runs once any sliced tail walk has finished using this heap.
    // This keeps allocator ownership transfers off the path between commands without an idle gap.
    if !more {
        pagepool::idle_point();
    }
    TAIL_REST.with(|c| c.set(if more { rest.max(0) } else { -1 }));
    let ms = if more { mempolicy::keep::IDLE_TAIL_SLICE_MS } else { rest };
    if ms > 0 {
        timeout_seams::enable_timeout_after::call(timeout_seams::IDLE_PASSIVATE_TIMEOUT, ms)?;
    }
    Ok(())
}

/// The idle timer for one idle period, as the memory policy times it
/// (`mempolicy::keep::idle_stage`; in a transaction block the release stage
/// only, when `idle_in_xact` finds something to give back). Returns the
/// delay to arm in ms (0 = arm nothing).
pub(crate) fn arm_idle_timer_ms(in_xact: bool) -> i32 {
    let worth = in_xact.then(|| {
        let threshold = (guc_tables::backing::pgrust_memory_release_threshold().max(0) as u64).saturating_mul(1024);
        mempolicy::keep::idle_in_xact(memledger::pool_reserved(memledger::current()), memledger::statement_peak_here(), threshold)
    });
    let release_ms = guc_tables::backing::pgrust_memory_release_idle();
    let passivate_ms = idle_passivate_secs().max(0).saturating_mul(1000);
    let (release_first, ms) = mempolicy::keep::idle_stage(release_ms, passivate_ms, worth);
    RELEASE_STAGE.with(|c| c.set(release_first));
    TAIL_REST.with(|c| c.set(-1));
    ms
}

/// The ProcessInterrupts arm of the idle timer (DoingCommandRead). The
/// release stage hands freed memory back and re-arms the timer for the rest
/// of the passivation delay; the passivation stage passivates, outside a
/// transaction only. Statements without an idle gap never pay the release.
pub(crate) fn idle_timer_fired(in_xact: bool) -> PgResult<()> {
    // pppress card 10: the next slice of an idle tail cut that stopped at its budget
    let rest = TAIL_REST.with(|c| c.replace(-1));
    if rest >= 0 {
        let (_, more) = pagepool::idle_tails(true);
        return arm_after_tails(more, rest);
    }
    if RELEASE_STAGE.with(|c| c.replace(false)) {
        // D3003 (c): process caches parked between statements first (Gravity's depots when no statement runs)
        mcx::run_idle_release();
        let released = mcx::release_retained();
        // pppress card 10 (PGRUST_POOL_IDLE_TAIL): this session's small runs' free tails back to the OS
        let (_, more) = pagepool::idle_tails(false);
        let release_ms = guc_tables::backing::pgrust_memory_release_idle();
        let passivate_ms = idle_passivate_secs().max(0).saturating_mul(1000);
        let rest = mempolicy::keep::idle_after_release(release_ms, passivate_ms, in_xact);
        arm_after_tails(more, rest)?;
        return elog::elog(
            types_error::DEBUG1,
            format!("idle memory release: allocator release {}", if released { "done" } else { "unavailable" }),
        );
    }
    if !in_xact && idle_passivate_secs() > 0 {
        IdlePassivate()?;
    }
    Ok(())
}

/// pgrust.memory_release_threshold: once the client has its ReadyForQuery, a
/// statement that freed more context bytes than the threshold after its peak
/// hands that memory back to the OS. C gets this from free() returning large
/// chunks and from process exit; the process-wide allocator here keeps freed
/// segments until its next purge, which an idle server never reaches.
/// Ported from pgrust main 5100cad95d0 / 9e77de1e93b.
pub(crate) fn release_after_statement() -> PgResult<()> {
    // pagepool P2: the purger asked the shelves home (a relaxed load; never set while the pool is off)
    if pagepool::low_warm() {
        pagepool::flush_shelf();
    }
    let freed = mcx::global_footprint::thread_freed_since_peak_rebase();
    let threshold = (guc_tables::backing::pgrust_memory_release_threshold().max(0) as usize).saturating_mul(1024);
    if !mempolicy::keep::end_release(freed, threshold) {
        return Ok(());
    }
    release_after_statement_slow(freed)
}

#[cold]
#[inline(never)]
fn release_after_statement_slow(freed: usize) -> PgResult<()> {
    let released = mcx::release_retained();
    // D4878: the pool's free memory (the release above only homes this thread's runs, warm) goes back to the OS under
    // ladder pressure: a canceled statement's bytes leave the read at its let-go, before the ladder's next sample
    if pagepool::on() {
        match mempolicy::keep::pool_end(mcx::ladder::pressed_now(), mcx::ladder::past_cancel_line()) {
            mempolicy::keep::PoolEnd::Now => {
                pagepool::note_end_give(true, pagepool::return_warm_now(usize::MAX));
            }
            mempolicy::keep::PoolEnd::Ask => {
                let _ = pagepool::return_warm();
                pagepool::note_end_give(false, 0);
            }
            // P329: unpressed, the bytes this statement freed past the threshold leave the read now, as C's free() of
            // its large chunks does (a rolled-back deferred-trigger queue); the rest of the pool's free memory stays
            // warm for the next take
            mempolicy::keep::PoolEnd::Keep => {
                let _ = pagepool::return_warm_now(freed);
            }
        }
    }
    elog::elog(
        types_error::DEBUG1,
        format!(
            "statement memory release: freed context bytes {} kB, allocator release {}",
            freed / 1024,
            if released { "done" } else { "unavailable" }
        ),
    )
}
