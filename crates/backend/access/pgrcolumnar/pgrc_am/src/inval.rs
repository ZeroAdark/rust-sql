//! The AM-layer cache composition: placement of the process-shared M3-F
//! [`PartRegistry`], the relid → part-key map that closes the
//! **ino-reuse hole** (pgrc_read crate doc "Identity staleness": sealed
//! parts are immutable and cache identity is `(dev, ino, len)`; the one
//! hole is inode reuse after unlink+recreate at equal length — closed HERE
//! with relcache-style invalidation, the named M3-F→M3-H seam), and the
//! per-table publish serialization mutex.
//!
//! ## Registration law (the part_cache precedent, verbatim)
//!
//! Callback registration PRECEDES first map insert: a registration failure
//! must not leave a cache that caches without invalidation. The relcache
//! callback array is per-backend TLS in `inval`, so EACH backend registers
//! its own callback before its first recorded open; the per-backend latch
//! cell lives in [`crate::session`]'s single census-pinned `thread_local!`
//! block. See [`ensure_inval_registered`].

use std::collections::BTreeMap;
use std::sync::Arc;

use pgrc_read::registry::{PartKey, PartRegistry};
use types_core::Oid;
use types_error::PgResult;

// The shared part registry's byte budget is the memory policy's (`mempolicy::cache::registry_budget`).
use mempolicy::cache::REGISTRY_BUDGET_BYTES as PART_CACHE_BUDGET_BYTES;

pgsync::process_global! {
    /// The process-shared part registry (M3-F contract; placement is M3-H's).
    static REGISTRY: pgsync::Mutex<Option<Arc<PartRegistry>>> = pgsync::Mutex::new(None);
    /// relid → the part cache keys opened for it (the ino-reuse ledger).
    /// A set (K1-RECLAIM): a scan of a 10,000-part table recorded each key with a linear `contains`.
    static RELID_PARTS: pgsync::Mutex<BTreeMap<u32, std::collections::BTreeSet<PartKey>>> =
        pgsync::Mutex::new(BTreeMap::new());
    /// relfilenumber-keyed publish mutexes (single-process serialization of
    /// generational manifest publish; crate doc "Publish serialization").
    static PUBLISH_LOCKS: pgsync::Mutex<BTreeMap<u64, Arc<pgsync::Mutex<()>>>> =
        pgsync::Mutex::new(BTreeMap::new());
    /// D4537 T18b: the bytes of RELID_PARTS and of PUBLISH_LOCKS, on the part cache (the maps are the registry's
    /// bookkeeping: they leave with the relations' parts and directories), refit after each change under the map's lock.
    static RELID_CHARGE: pgsync::Mutex<memledger::Charge> = pgsync::Mutex::new(memledger::Charge::NONE);
    static LOCKS_CHARGE: pgsync::Mutex<memledger::Charge> = pgsync::Mutex::new(memledger::Charge::NONE);
}

/// D4537 T18b (R2): `charge` fitted to `bytes` on the part cache (opened at the first bytes; a tree node's links counted
/// at two words).
pub(crate) fn refit(charge: &pgsync::Mutex<memledger::Charge>, bytes: usize) {
    const OWNER: memledger::Owner = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::PartCache));
    let mut c = pgsync::lock(charge);
    if c.owner() != OWNER {
        *c = memledger::Charge::open(OWNER, memledger::Release::Evict, memledger::Kind::Region, memledger::charge::Door::Heap, 0);
    }
    c.fit_committed(bytes as u64);
}

fn relids_bytes(map: &BTreeMap<u32, std::collections::BTreeSet<PartKey>>) -> usize {
    const NODE: usize = 2 * size_of::<usize>();
    map.len() * (size_of::<(u32, std::collections::BTreeSet<PartKey>)>() + NODE) + map.values().map(|s| s.len() * (size_of::<PartKey>() + NODE)).sum::<usize>()
}

fn locks_bytes(map: &BTreeMap<u64, Arc<pgsync::Mutex<()>>>) -> usize {
    const NODE: usize = 2 * size_of::<usize>();
    map.len() * (size_of::<(u64, Arc<pgsync::Mutex<()>>)>() + NODE + 2 * size_of::<usize>() + size_of::<pgsync::Mutex<()>>())
}

fn refit_relids(map: &BTreeMap<u32, std::collections::BTreeSet<PartKey>>) {
    refit(&RELID_CHARGE, relids_bytes(map));
}

fn refit_locks(map: &BTreeMap<u64, Arc<pgsync::Mutex<()>>>) {
    refit(&LOCKS_CHARGE, locks_bytes(map));
}

/// Memory step 7: the registry's byte budget for a part cache budget the memory broker sets (`Some`: at least the
/// default, so its janitor never drops entries the part cache keeps, with their re-use history), else the default.
pub fn registry_budget(part_cache: Option<u64>) -> u64 {
    mempolicy::cache::registry_budget(part_cache)
}

/// The shared registry (created on first use; const-init static, no
/// OnceLock — the determinism ledger's `once` category stays empty).
///
/// Any thread may create it and reads no setting: the memory broker's census and the memory budget's part-cache hooks
/// call it on the memory watchdog's thread, which binds no GUC store (a setting read there panics). The decoded-column
/// cap the registry's parts cache under is applied by [`apply_cache_cap`], on threads that have one.
pub fn registry() -> Arc<PartRegistry> {
    registry_in(&REGISTRY)
}

fn registry_in(slot: &pgsync::Mutex<Option<Arc<PartRegistry>>>) -> Arc<PartRegistry> {
    let mut slot = pgsync::lock(slot);
    match &*slot {
        Some(r) => Arc::clone(r),
        None => {
            let r = Arc::new(PartRegistry::new(PART_CACHE_BUDGET_BYTES));
            // D3110 (b): the trim and the in-use floor reach the generations' parts a relcache invalidation took out of
            // the entries (`invalidate_relid`)
            r.set_held_parts(crate::catalog::held_parts);
            *slot = Some(Arc::clone(&r));
            r
        }
    }
}

/// C1 L5: `pgrust.pgrc_cache_size`, the decoded-column cap in MiB (`pgrc_read::mem`; default 256, 0 = no
/// caching). A restart-only setting, like shared_buffers, since the cache is process-wide: [`apply_cache_cap`]
/// applies it once per process and a reload never re-reads it.
const CACHE_SIZE_GUC: &str = "pgrust.pgrc_cache_size";

/// The setting in MiB. Only on a thread with a GUC store (the seam's lookup panics on one without).
fn cache_size_mib() -> u64 {
    #[cfg(test)]
    tests::note_setting_read();
    let seam = guc_seams::get_config_option_missing_ok::is_installed();
    let set = seam.then(|| guc_seams::get_config_option_missing_ok::call(CACHE_SIZE_GUC).ok().flatten()).flatten();
    set.and_then(|v| v.trim().parse().ok()).unwrap_or(pgrc_read::mem::DEFAULT_CAP >> 20)
}

/// Whether the setting is applied, and the lock its application holds.
struct CapState {
    applied: pgsync::atomic::AtomicBool,
    lock: pgsync::Mutex<()>,
}

pgsync::process_global! {
    /// The process's [`apply_cache_cap`] state.
    static CAP: CapState = CapState { applied: pgsync::atomic::AtomicBool::new(false), lock: pgsync::Mutex::new(()) };
}

/// C1 L5: apply `pgrust.pgrc_cache_size` to the decoded-column cap (`pgrc_read::mem::set_cap`), once per process.
/// Only from a thread with a GUC store, before anything decodes a column: the postmaster at its start
/// (`pgrc_maint::register`, after the configuration file is read and before any backend runs), and every backend's
/// [`ensure_inval_registered`] (where no postmaster started the process: single-user mode, tests).
pub fn apply_cache_cap() {
    apply_cap_once(&CAP, cache_size_mib, pgrc_read::mem::set_cap);
}

/// [`apply_cache_cap`] over `state`: the first caller reads the setting (`mib`, MiB) and stores the cap (`set`,
/// bytes) under `state.lock`, then marks it applied. A caller that finds it unapplied waits on that lock, so every
/// call returns with the cap in place and no caller admits a column under the default cap while another applies the
/// setting. True: this call applied it.
fn apply_cap_once(state: &CapState, mib: impl FnOnce() -> u64, set: impl FnOnce(u64)) -> bool {
    use pgsync::atomic::Ordering;
    if state.applied.load(Ordering::Acquire) {
        return false;
    }
    let _applying = pgsync::lock(&state.lock);
    if state.applied.load(Ordering::Acquire) {
        return false;
    }
    set(mib() << 20);
    state.applied.store(true, Ordering::Release);
    true
}

/// The decoded-column cap in bytes: the setting once [`apply_cache_cap`] ran, else the default. Reads no setting.
pub fn cache_cap() -> u64 {
    pgrc_read::mem::cap()
}

/// `pgrc_cache_stats() → text` (C1 L5): the process's decoded-column counters (`pgrc_read::mem::stats`).
pub const PGRC_CACHE_STATS_FOID: Oid = 9032;

pub const CACHE_BUILTINS: &[types_fmgr::FmgrBuiltin] = &[types_fmgr::FmgrBuiltin {
    foid: PGRC_CACHE_STATS_FOID,
    name: "pgrc_cache_stats",
    nargs: 0,
    strict: false,
    retset: false,
    func: fc_pgrc_cache_stats,
}];

pub fn fc_pgrc_cache_stats(_flinfo: Option<&mut types_fmgr::FmgrInfo>, fcinfo: &mut types_fmgr::FunctionCallInfoBaseData) -> PgResult<datum::Datum> {
    let s = pgrc_read::mem::stats();
    let text = format!(
        "cap={} cached={} held={} sticky={} admitted={} evicted={} evicted_bytes={} over_cap={} futile={} slots={} image_bytes={}",
        cache_cap(), s.cached, s.held, s.sticky, s.admitted, s.evicted, s.evicted_bytes, s.over_cap, s.futile, s.slots, s.image_bytes
    );
    let mut img: mcx::PgVec<'_, u8> = mcx::PgVec::new_in(fcinfo.result_mcx());
    img.extend_from_slice(&[0u8; datum::VARHDRSZ]);
    img.extend_from_slice(text.as_bytes());
    Ok(types_fmgr::varlena_result(datum::Varlena::from_image(img)))
}

/// Record that `relid`'s scan opened the part under `key` (called with the
/// pin held; the relcache callback later invalidates exactly these).
pub fn record_part_key(relid: Oid, key: PartKey) {
    let mut map = pgsync::lock(&RELID_PARTS);
    map.entry(relid).or_default().insert(key); // alloc: Cold charged the part cache (RELID_CHARGE, refit below)
    refit_relids(&map);
}

/// The engine-registry flush hook (the sqe statement plane's standing
/// engine cache rides the SAME relcache-invalidation face as the part
/// registry and the fact cache): DDL/TRUNCATE-class events drop the
/// per-relation engine handle so the next statement re-resolves. Set once
/// at executor seam-init (process-global; a null hook is a no-op).
static ENGINE_FLUSH_HOOK: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Register the engine-registry flush face (`relid == 0` = flush all).
pub fn set_engine_flush_hook(f: fn(Oid)) {
    ENGINE_FLUSH_HOOK.store(f as usize, std::sync::atomic::Ordering::Release);
}

fn engine_flush(relid: Oid) {
    let p = ENGINE_FLUSH_HOOK.load(std::sync::atomic::Ordering::Acquire);
    if p != 0 {
        // SAFETY: only ever stored from a `fn(Oid)` in set_engine_flush_hook.
        let f: fn(Oid) = unsafe { std::mem::transmute::<usize, fn(Oid)>(p) };
        f(relid);
    }
}

/// Drop every recorded key for `relid` from the shared registry.
/// `relid == 0` (InvalidOid) = a full-cache invalidation event.
pub fn invalidate_relid(relid: Oid) {
    engine_flush(relid);
    // the columnar catalog's relid map (hygiene: its entries live with their directories)
    crate::catalog::invalidate_relid(relid);
    let reg = registry();
    if relid == 0 {
        let mut map = pgsync::lock(&RELID_PARTS);
        map.clear();
        refit_relids(&map);
        drop(map);
        reg.clear();
        return;
    }
    let keys = {
        let mut map = pgsync::lock(&RELID_PARTS);
        let keys = map.remove(&relid);
        refit_relids(&map);
        keys
    };
    if let Some(keys) = keys {
        let _ = elog::elog(types_error::DEBUG1, format!("pgrc relcache invalidation: relid {relid}: {} part registry keys dropped", keys.len())); // alloc: Cold
        for key in keys {
            reg.invalidate(key);
        }
    }
}

fn relcache_callback(_arg: datum::Datum, relid: Oid) {
    crate::reloptions::forget_lead(relid);
    invalidate_relid(relid);
}

/// Register this backend's relcache callback (BEFORE the first recorded
/// open — the part_cache register-before-install law). The latch lives in
/// the session TLS block (census-pinned single block). First the
/// decoded-column cap ([`apply_cache_cap`]: once per process; one relaxed
/// load after).
pub fn ensure_inval_registered() -> PgResult<()> {
    apply_cache_cap();
    crate::session::ensure_session_hooks();
    crate::session::with_inval_latch(|c| {
        if !c.get() {
            inval::invalidate::CacheRegisterRelcacheCallback(
                relcache_callback,
                datum::Datum::null(),
            )?;
            c.set(true);
        }
        Ok(())
    })
}

/// The per-table publish lock: held across finish+publish, a catalog load, and a sections completion.
pub fn publish_lock(relfilenumber: u64) -> Arc<pgsync::Mutex<()>> {
    let mut map = pgsync::lock(&PUBLISH_LOCKS);
    let l = Arc::clone(map.entry(relfilenumber).or_insert_with(|| Arc::new(pgsync::Mutex::new(())))); // alloc: Cold charged the part cache (LOCKS_CHARGE, refit below)
    refit_locks(&map);
    l
}

/// D4581: table directory `dir` went for good (`dirpath::remove_table_dir`: DROP, a rewrite's old locator, an aborted
/// creation): its publish mutex goes when nothing holds it (no holder, no waiter: the next [`publish_lock`] makes an
/// equal one).
pub fn forget_dir(dir: &str) {
    let parse = |p: &str| p.rsplit('/').next()?.strip_prefix("pgrc_")?.parse::<u64>().ok();
    let p = dir.trim_end_matches('/');
    if let Some(n) = parse(p) {
        forget_publish_lock(n);
    }
}

/// D4581: the publish mutexes of the relfilenumbers `numbers` (the catalog entries a database's removal dropped) go the
/// same way.
pub fn forget_dirs_under(_db_path: &str, numbers: impl Iterator<Item = u64>) {
    for n in numbers {
        forget_publish_lock(n);
    }
}

fn forget_publish_lock(n: u64) {
    let unheld = {
        let mut m = pgsync::lock(&PUBLISH_LOCKS);
        match m.get(&n) {
            Some(l) if Arc::strong_count(l) == 1 => m.remove(&n),
            _ => None,
        }
        .inspect(|_| refit_locks(&m))
    };
    drop(unheld);
}

/// D4581: the part keys `gone` names leave the relid map (their parts left the registry: a removed relation's, at
/// `catalog::remove_dir` / `remove_under`), and a relid with none left goes. The keys are the files' identities, not
/// relids, which two databases share: a relcache message of the other database's relation still reaches its own parts.
pub fn forget_part_keys(gone: &dyn Fn(&PartKey) -> bool) {
    let mut map = pgsync::lock(&RELID_PARTS);
    // bounded("the relids scanned this lifetime, once per removal")
    map.retain(|_, keys| {
        keys.retain(|k| !gone(k));
        !keys.is_empty()
    });
    refit_relids(&map);
}

/// D4581 test witness: the part keys recorded for `relid`; whether a publish mutex is kept for `relfilenumber`.
pub fn relid_part_keys(relid: Oid) -> usize {
    pgsync::lock(&RELID_PARTS).get(&relid).map_or(0, std::collections::BTreeSet::len)
}
pub fn publish_lock_kept(relfilenumber: u64) -> bool {
    pgsync::lock(&PUBLISH_LOCKS).contains_key(&relfilenumber)
}

#[cfg(test)]
mod tests {
    /// D4537 T18b: the relid -> part keys map and the publish mutexes are the part cache's bytes, refit after each change
    /// (read under the maps' locks, where the refit runs, so parallel tests cannot move them between the two reads).
    #[test]
    fn the_relid_map_and_the_publish_mutexes_are_on_the_part_cache() {
        const OWNER: memledger::Owner = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::PartCache));
        let relid: Oid = 4_282_000_007;
        let rel = 9_930_000_000 + u64::from(std::process::id());
        record_part_key(relid, (u64::MAX - 7, 1, 1));
        let held = publish_lock(rel);
        {
            let map = pgsync::lock(&RELID_PARTS);
            let c = pgsync::lock(&RELID_CHARGE);
            assert_eq!((c.owner(), c.committed()), (OWNER, relids_bytes(&map) as u64));
            assert!(c.committed() > 0);
        }
        {
            let map = pgsync::lock(&PUBLISH_LOCKS);
            let c = pgsync::lock(&LOCKS_CHARGE);
            assert_eq!((c.owner(), c.committed()), (OWNER, locks_bytes(&map) as u64));
            assert!(c.committed() > 0);
        }
        drop(held);
        forget_dir(&format!("t/pgrc_{rel}"));
        invalidate_relid(relid);
        let map = pgsync::lock(&PUBLISH_LOCKS);
        assert!(!map.contains_key(&rel));
        assert_eq!(pgsync::lock(&LOCKS_CHARGE).committed(), locks_bytes(&map) as u64, "the forgotten mutex's bytes went");
    }

    use super::*;
    use pgsync::atomic::{AtomicBool, Ordering};
    use std::sync::atomic::{AtomicU64, AtomicUsize};
    use std::thread::ThreadId;

    /// The threads standing for one with no GUC store (the memory watchdog's): a setting read on one panics, as the
    /// seam's lookup does there (`GetConfigOption`'s "GUC store not initialized").
    static STORELESS: pgsync::Mutex<Vec<ThreadId>> = pgsync::Mutex::new(Vec::new());

    pub(super) fn note_setting_read() {
        let me = std::thread::current().id();
        if pgsync::lock(&STORELESS).contains(&me) {
            panic!("GUC store not initialized");
        }
    }

    /// `f` on a new thread with no GUC store; Err: it panicked.
    fn storeless<R: Send>(f: impl FnOnce() -> R + Send) -> std::thread::Result<R> {
        std::thread::scope(|s| {
            s.spawn(|| {
                pgsync::lock(&STORELESS).push(std::thread::current().id());
                f()
            })
            .join()
        })
    }

    fn unapplied() -> CapState {
        CapState { applied: AtomicBool::new(false), lock: pgsync::Mutex::new(()) }
    }

    /// The memory broker's census, the budget's part-cache floor and the idle release's trim run on threads with no
    /// GUC store; the first of them may create the registry. None reads a setting.
    #[test]
    fn a_thread_with_no_guc_store_creates_the_registry_and_reads_no_setting() {
        static SLOT: pgsync::Mutex<Option<Arc<PartRegistry>>> = pgsync::Mutex::new(None);
        let same = storeless(|| {
            let r = registry_in(&SLOT);
            let _ = (r.in_use_bytes(), r.reuse_census(1), r.removed(), r.trim_contents(u64::MAX));
            let p = registry();
            let _ = (p.in_use_bytes(), p.reuse_census(1), p.removed(), cache_cap());
            Arc::ptr_eq(&r, &registry_in(&SLOT))
        });
        assert_eq!(same.ok(), Some(true), "the registry is created on a thread with no GUC store, reading no setting");
        // the control: the setting's own read does panic on such a thread, and leaves the cap unapplied
        let state = unapplied();
        assert!(storeless(|| apply_cap_once(&state, cache_size_mib, |_| {})).is_err(), "a setting read panics there");
        assert!(!state.applied.load(Ordering::Acquire));
    }

    /// The broker's thread creates the registry first; then a backend whose pgrust.pgrc_cache_size is not the default
    /// registers. The cap is the setting, not the default, and it is never read again (a reload, another session's
    /// own value: the setting is a placeholder, so a session may hold its own; the first applying thread's value wins,
    /// and under a postmaster that thread is the postmaster, whose value is the configuration file's).
    #[test]
    fn the_cap_is_the_setting_after_a_storeless_creation_and_is_read_once() {
        static SLOT: pgsync::Mutex<Option<Arc<PartRegistry>>> = pgsync::Mutex::new(None);
        let state = unapplied();
        let cap = AtomicU64::new(pgrc_read::mem::DEFAULT_CAP);
        let set = |b: u64| cap.store(b, std::sync::atomic::Ordering::Relaxed);
        assert!(storeless(|| drop(registry_in(&SLOT))).is_ok());
        assert!(!state.applied.load(Ordering::Acquire), "creating the registry applies nothing");
        assert_eq!(cap.load(std::sync::atomic::Ordering::Relaxed), pgrc_read::mem::DEFAULT_CAP);
        assert!(apply_cap_once(&state, || 64, set), "the backend's registration applies the setting");
        assert_eq!(cap.load(std::sync::atomic::Ordering::Relaxed), 64 << 20);
        assert!(!apply_cap_once(&state, || 128, set), "never read again");
        assert_eq!(cap.load(std::sync::atomic::Ordering::Relaxed), 64 << 20);
    }

    /// Concurrent first calls: threads with no GUC store create the registry while backends apply the setting, whose
    /// read is slow. The setting is read and stored once, and every backend returns with the setting's cap in place:
    /// the cap its admission's room check (`pgrc_read::mem::make_room`) reads next is never the default.
    #[test]
    fn concurrent_first_calls_apply_the_setting_once_and_before_any_admission() {
        static SLOT: pgsync::Mutex<Option<Arc<PartRegistry>>> = pgsync::Mutex::new(None);
        let state = unapplied();
        let cap = AtomicU64::new(pgrc_read::mem::DEFAULT_CAP);
        let (reads, stores) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let seen = pgsync::Mutex::new(Vec::new());
        let panicked = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for i in 0..16 {
                if i % 2 == 0 {
                    s.spawn(|| {
                        if storeless(|| drop(registry_in(&SLOT))).is_err() {
                            panicked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        }
                    });
                } else {
                    s.spawn(|| {
                        let read = || {
                            reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            std::thread::sleep(std::time::Duration::from_millis(20));
                            64
                        };
                        let set = |b: u64| {
                            stores.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            cap.store(b, std::sync::atomic::Ordering::Relaxed);
                        };
                        apply_cap_once(&state, read, set);
                        pgsync::lock(&seen).push(cap.load(std::sync::atomic::Ordering::Relaxed));
                    });
                }
            }
        });
        let seen = pgsync::lock(&seen).clone();
        assert_eq!(panicked.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!((reads.into_inner(), stores.into_inner()), (1, 1), "the setting is read and stored once");
        assert_eq!(seen.len(), 8);
        assert!(seen.iter().all(|&c| c == 64 << 20), "a backend returned before the cap was in place: {seen:?}");
    }
}
