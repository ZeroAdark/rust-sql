//! D3.2 shared immutable L2 cache substrate (docs/design/connection-scaling.md §D3).
//!
//! Process-global, generation-keyed store of immutable cache-entry bodies,
//! shared by every backend thread. The per-thread (L1) caches are untouched on
//! their hit paths; on an L1 miss the owning cache consults this store before
//! scanning the catalogs, and on an L2 miss exactly one thread builds the
//! entry (per-key build gate — the thundering-herd fix).
//!
//! # Generations (the invalidation design, as built)
//!
//! One monotonic generation counter per invalidation domain:
//! - catcache: one per syscache id (`CAT_DOMAINS` slots), bumped by every
//!   catcache sinval message for that cache and by catalog-flush messages;
//! - relcache: `REL_STRIPES` stripes over relid, bumped per relcache sinval
//!   message (relid == InvalidOid bumps every stripe).
//!
//! Beside each domain generation, its PROGRAM generations (`prog_gen`, C3):
//! one per database bucket, bumped with the domain's but only for the
//! message's database (a shared catalog's or relation's: every bucket). A
//! Gravity program's recorded dependency holds one (gravity_deps).
//!
//! Bumps happen ONCE, on the SENDING side, strictly BEFORE the messages enter
//! the shared sinval queue (inval::eoxact send paths). Each backend keeps a
//! thread-local VIEW of every domain generation, advanced only when it
//! processes an invalidation message for that domain (or a full cache reset).
//! L2 lookups are keyed by the reader's view, so:
//! - a backend that has not yet processed a queued inval keeps reading the
//!   entries of its old generation (no forward time-travel mid-transaction;
//!   this is exactly today's L1 staleness-until-AcceptInvalidationMessages);
//! - a backend that has processed the inval reads at the new generation and
//!   misses, and the rebuilt entry can never be confused with the old one.
//!
//! Because the bump precedes queue insertion, a backend that processes the
//! message and re-syncs its view is guaranteed to see a generation at least
//! as new as the bump — an invalidation can never be "undone" by an L2 hit.
//!
//! A build that races a bump (global generation moved between the reader
//! capturing its view and finishing the catalog scan) is installed in the
//! builder's L1 but NOT published: the data read under the builder's own
//! catalog snapshot can be from either side of the concurrent commit, so it
//! must not be stamped with a generation other threads will trust.
//!
//! Superseded generations: entries stay alive while referenced (`Arc`); the
//! map retains at most the two newest generations per logical key (pruned on
//! insert), so a laggard backend usually still finds its older entry. When it
//! does not, it falls back to a private catalog read — which is exactly
//! today's (L2-less) behavior and carries today's semantics.
//!
//! # Uncommitted DDL (overlay rule)
//!
//! A session with pending, not-yet-broadcast invalidation messages (i.e. it
//! modified a catalog in the current transaction) bypasses L2 entirely — both
//! read and publish — until end of transaction (`private_build_mode`,
//! coarse-grained form of the design doc's per-key overlay: strictly safe,
//! DDL sessions simply build privately as they do today). Parallel workers
//! also bypass L2: a worker may observe its leader's uncommitted catalog
//! state, which must never be published.
//!
//! Kill switch: PGRUST_L2_CACHE=0 disables every L2 path.

use std::any::Any;
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use rustc_hash::FxBuildHasher;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use types_core::{InvalidOid, Oid};

/// syscache id space (syscache_ids.h SysCacheSize is 85; headroom for growth).
pub const CAT_DOMAINS: usize = 96;
/// relid stripes for relcache generations (power of two).
pub const REL_STRIPES: usize = 1024;

/// GUC `shared_catalog_cache` (PGC_POSTMASTER — flipping mid-life would
/// desync generation views against live L1 state; the cell is read live, but
/// the GUC engine refuses non-boot assignment) kills every L2 path when off.
/// PGRUST_L2_CACHE remains the harness override (env wins if set; cached at
/// first read — t35-law kill switch for the existing test suites/scripts).
#[inline]
pub fn enabled() -> bool {
    static ENV: std::sync::OnceLock<Option<bool>> = std::sync::OnceLock::new();
    if let Some(v) = *ENV.get_or_init(|| {
        std::env::var("PGRUST_L2_CACHE").ok().map(|v| v.trim() != "0")
    }) {
        return v;
    }
    guc_tables::backing::shared_catalog_cache()
}

// ---------------------------------------------------------------------------
// Private-build probes (uncommitted-DDL overlay + parallel workers)
// ---------------------------------------------------------------------------

// Installed by inval::init_seams (l2cache cannot depend on inval).
static PENDING_PROBE: std::sync::atomic::AtomicPtr<()> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());
// Installed by snapmgr::init_seams: catalog reads under a historic snapshot
// (logical decoding) see PAST catalog states and must never be published.
static HISTORIC_PROBE: std::sync::atomic::AtomicPtr<()> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

pub fn set_pending_invals_probe(f: fn() -> bool) {
    PENDING_PROBE.store(f as *mut (), Ordering::Release);
}

pub fn set_historic_snapshot_probe(f: fn() -> bool) {
    HISTORIC_PROBE.store(f as *mut (), Ordering::Release);
}

// Installed by inval::init_seams: inval `PendingInvalidationsTouch`.
static TOUCH_PROBE: std::sync::atomic::AtomicPtr<()> = std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

pub fn set_pending_touch_probe(f: fn(&[Oid], &[Oid], &[i32], bool) -> bool) {
    TOUCH_PROBE.store(f as *mut (), Ordering::Release);
}

/// C3 S4b: this thread's own unbroadcast invalidation messages (uncommitted DDL) touch relations `relids` (their
/// relcache entries), C3 S5d: the storage or size statistics of relations `storage`, or catalog caches `cat_ids` (inval
/// `PendingInvalidationsTouchParts`; false before inval installs it). `under`: the reads were made under the DDL, so a
/// row it created is touched too.
pub fn pending_touch(relids: &[Oid], storage: &[Oid], cat_ids: &[i32], under: bool) -> bool {
    let p = TOUCH_PROBE.load(Ordering::Acquire);
    if p.is_null() {
        return false;
    }
    // SAFETY: only set_pending_touch_probe stores here, always a fn(&[Oid], &[Oid], &[i32], bool) -> bool.
    let f: fn(&[Oid], &[Oid], &[i32], bool) -> bool = unsafe { std::mem::transmute(p) };
    f(relids, storage, cat_ids, under)
}

#[inline]
fn probe(slot: &std::sync::atomic::AtomicPtr<()>) -> bool {
    let p = slot.load(Ordering::Acquire);
    if p.is_null() {
        return false;
    }
    // SAFETY: only the set_* fns above store here, always a fn() -> bool.
    let f: fn() -> bool = unsafe { std::mem::transmute(p) };
    f()
}

/// True when this thread must build cache entries privately (no L2 read or
/// publish): it holds unbroadcast invalidation messages (uncommitted DDL), it
/// is a parallel worker (may see its leader's uncommitted catalog state), or
/// it reads catalogs under a historic snapshot (logical decoding).
pub fn private_build_mode() -> bool {
    if probe(&PENDING_PROBE) || probe(&HISTORIC_PROBE) {
        return true;
    }
    parallel_seams::is_parallel_worker::is_installed()
        && parallel_seams::is_parallel_worker::call()
}

/// D3866 (step 2): `private_build_mode` holds for one reason only, this thread's own unbroadcast invalidation messages
/// (uncommitted DDL): not a parallel worker, not under a historic snapshot. Such a thread still publishes nothing, but
/// a reader that its pending messages do not touch (inval `PendingInvalidationsTouch`) may read what other backends
/// filed under the committed catalog state.
pub fn private_by_own_ddl_only() -> bool {
    probe(&PENDING_PROBE)
        && !probe(&HISTORIC_PROBE)
        && !(parallel_seams::is_parallel_worker::is_installed() && parallel_seams::is_parallel_worker::call())
}

// ---------------------------------------------------------------------------
// Generations
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Domain {
    Cat(i32),
    Rel(Oid),
}

#[allow(clippy::declare_interior_mutable_const)]
const ZERO: AtomicU64 = AtomicU64::new(0);
static CAT_GENS: [AtomicU64; CAT_DOMAINS] = [ZERO; CAT_DOMAINS];
static REL_GENS: [AtomicU64; REL_STRIPES] = [ZERO; REL_STRIPES];

#[inline]
fn rel_stripe(relid: Oid) -> usize {
    (relid as usize) & (REL_STRIPES - 1)
}

// Cache ids arrive from WAL commit records, two-phase state and the sinval
// queue; an id outside the catcache range (C: an unchecked array index)
// lands on a spare slot no reader consults.
static SPARE_GEN: AtomicU64 = AtomicU64::new(0);

#[inline]
fn gen_slot(domain: Domain) -> &'static AtomicU64 {
    match domain {
        Domain::Cat(id) => match CAT_GENS.get(usize::try_from(id).unwrap_or(usize::MAX)) {
            Some(slot) => slot,
            None => &SPARE_GEN,
        },
        Domain::Rel(relid) => &REL_GENS[rel_stripe(relid)],
    }
}

#[inline]
pub fn current_gen(domain: Domain) -> u64 {
    gen_slot(domain).load(Ordering::Acquire)
}

thread_local! {
    static VIEW_INIT: Cell<bool> = const { Cell::new(false) };
    static CAT_VIEW: [Cell<u64>; CAT_DOMAINS] = const { [const { Cell::new(0) }; CAT_DOMAINS] };
    static REL_VIEW: [Cell<u64>; REL_STRIPES] = const { [const { Cell::new(0) }; REL_STRIPES] };
    static VIEW_MOVES: Cell<u64> = const { Cell::new(0) };
    static OWN_SEQ: Cell<u64> = const { Cell::new(1) };
}

/// C3 S4b: this thread's own-invalidation count: it moves each time the thread processes its own transaction's
/// invalidation messages (a command end that has some, a subtransaction's or the transaction's end with some), which
/// no generation records (generations move when a transaction's messages are sent at commit). What was read under this
/// thread's own uncommitted DDL holds only while the count stands (gravity_deps `Deps::current`).
#[inline]
pub fn own_seq() -> u64 {
    OWN_SEQ.with(Cell::get)
}

/// C3 S4b: inval processed (or, at commit, sent) this thread's own messages: `own_seq` moves, and with it the view count
/// (a holder's `current_since` mark checks again).
pub fn note_own_invals() {
    OWN_SEQ.with(|c| c.set(c.get().wrapping_add(1)));
    moved();
}

/// How many times this thread's view has moved, in any domain: a holder that found its dependencies current at one count
/// reads the same views while the count stands (C3, gravity_deps `Deps::current_since`).
#[inline]
pub fn view_moves() -> u64 {
    VIEW_MOVES.with(Cell::get)
}

#[inline]
fn moved() {
    VIEW_MOVES.with(|c| c.set(c.get().wrapping_add(1)));
}

/// C3 S6: this thread's catalog caches changed what a probe returns (catcache's invalidation, reset or flush): its view
/// moves too, so a holder of state no recording sees (gravity_deps `Opaque`) finds it moved whenever catcache's own
/// `inval_epoch` did.
#[inline]
pub fn note_local_inval() {
    moved();
}

/// Debug tracing for the D3.2 stressor (PGRUST_L2_DEBUG=1).
pub fn l2_debug_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("PGRUST_L2_DEBUG").is_ok_and(|v| v.trim() == "1"))
}

#[cold]
fn view_init_slow() {
    // First L2 use on this thread: adopt the current global generations. Any
    // sinval message queued for us before this point had its bump land first
    // (bump-before-queue), so the adopted view is never behind a message we
    // will later process — and processing it re-syncs anyway.
    for (i, g) in CAT_GENS.iter().enumerate() {
        let v = g.load(Ordering::Acquire);
        CAT_VIEW.with(|a| a[i].set(v));
    }
    for (i, g) in REL_GENS.iter().enumerate() {
        let v = g.load(Ordering::Acquire);
        REL_VIEW.with(|a| a[i].set(v));
    }
    VIEW_INIT.with(|c| c.set(true));
    moved();
    if l2_debug_enabled() {
        eprintln!(
            "L2DBG view-init thr={:?} relstripe34={} relstripe39={}",
            std::thread::current().id(),
            REL_GENS[34].load(Ordering::SeqCst),
            REL_GENS[39].load(Ordering::SeqCst),
        );
    }
}

/// Re-adopt the current global generations (e.g. right after sinval
/// registration: messages queued before registration are skipped by design,
/// so the view must cover their bumps).
pub fn readopt_views() {
    VIEW_INIT.with(|c| c.set(false));
    view_init_slow();
}

#[inline]
fn ensure_view() {
    if !VIEW_INIT.with(|c| c.get()) {
        view_init_slow();
    }
}

/// This thread's generation view for `domain` (as of its last processed
/// invalidation touching the domain).
#[inline]
pub fn view_gen(domain: Domain) -> u64 {
    ensure_view();
    match domain {
        Domain::Cat(id) => CAT_VIEW.with(|a| a.get(id as usize).map_or(0, |c| c.get())),
        Domain::Rel(relid) => REL_VIEW.with(|a| a[rel_stripe(relid)].get()),
    }
}

/// Advance this thread's view of `domain` to the current global generation.
/// Called wherever sinval processing drops L1 entries of the domain.
#[inline]
/// Returns whether the view advanced.
pub fn sync_view(domain: Domain) -> bool {
    ensure_view();
    let g = current_gen(domain);
    match domain {
        Domain::Cat(id) => CAT_VIEW.with(|a| {
            if let Some(c) = a.get(id as usize) {
                if g > c.get() {
                    c.set(g);
                    moved();
                    return true;
                }
            }
            false
        }),
        Domain::Rel(relid) => REL_VIEW.with(|a| {
            let c = &a[rel_stripe(relid)];
            if g > c.get() {
                c.set(g);
                moved();
                return true;
            }
            false
        }),
    }
}

pub fn sync_view_all_cat() -> bool {
    let mut advanced = false;
    ensure_view();
    for (i, g) in CAT_GENS.iter().enumerate() {
        let v = g.load(Ordering::Acquire);
        CAT_VIEW.with(|a| {
            if v > a[i].get() {
                a[i].set(v);
                advanced = true;
            }
        });
    }
    if advanced {
        moved();
    }
    advanced
}

pub fn sync_view_all_rel() -> bool {
    ensure_view();
    let mut advanced = false;
    for (i, g) in REL_GENS.iter().enumerate() {
        let v = g.load(Ordering::Acquire);
        REL_VIEW.with(|a| {
            if v > a[i].get() {
                a[i].set(v);
                advanced = true;
            }
        });
    }
    if advanced {
        moved();
    }
    advanced
}

pub fn sync_view_all() {
    sync_view_all_cat();
    sync_view_all_rel();
}

/// Snapshot every relcache stripe generation (one Acquire load each).
///
/// Init-file publish keying (relcache::initfile): the loader captures this
/// snapshot while holding RelCacheInitLock, so no unlink-protected DDL can
/// commit between "the file exists and is fresh" and "these are the
/// generations its content is current at" — entries decoded from the file are
/// then published at `snapshot[rel_stripe_of(relid)]`.
pub fn rel_gen_snapshot() -> Vec<u64> {
    REL_GENS.iter().map(|g| g.load(Ordering::Acquire)).collect()
}

/// The generation stripe `relid` maps to (index into [`rel_gen_snapshot`]).
#[inline]
pub fn rel_stripe_of(relid: Oid) -> usize {
    rel_stripe(relid)
}

/// Bump `domain`'s global generation (sender side, BEFORE the corresponding
/// sinval message enters the shared queue) and adopt it as this thread's own
/// view (the sender's caches already reflect the new state).
pub fn bump(domain: Domain) {
    ensure_view();
    let new = gen_slot(domain).fetch_add(1, Ordering::SeqCst) + 1;
    moved();
    match domain {
        Domain::Cat(id) => {
            CAT_VIEW.with(|a| {
                if let Some(c) = a.get(id as usize) {
                    c.set(new);
                }
            });
        }
        Domain::Rel(relid) => REL_VIEW.with(|a| a[rel_stripe(relid)].set(new)),
    }
}

pub fn bump_all_cat() {
    for id in 0..CAT_DOMAINS as i32 {
        bump(Domain::Cat(id));
    }
    // a catalog-wide message names no row: every relcache entry built from catalog rows goes with it (a support
    // catalog's row message names the relations built from it instead, D3888: inval `PreCommit_Inval`)
    bump_all_rel();
    // D3866: after the generations, as `bump_stamp` after `invalidate_key`; C3: every database's, every row's
    ROW_GENS.bump_all();
    for g in PROG_CAT_GENS.iter().flatten() {
        g.fetch_add(1, Ordering::SeqCst);
    }
    committed_moved();
}

/// C re-creates every cache with the crashed backends; this process-global
/// store survives the in-process crash cycle, so every generation advances
/// (a change committed but killed before its inval send is otherwise served
/// stale forever).
pub fn bump_all_after_crash() {
    bump_all_cat();
    bump_all_rel();
}

pub fn bump_all_rel() {
    ensure_view();
    for (i, g) in REL_GENS.iter().enumerate() {
        let new = g.fetch_add(1, Ordering::SeqCst) + 1;
        REL_VIEW.with(|a| a[i].set(new));
    }
    // C3: every database's program and storage generations, after the domains'
    PROG_REL_GENS.bump_all();
    STOR_GENS.bump_all();
    moved();
}

// ---------------------------------------------------------------------------
// Program stamps (D3866)
// ---------------------------------------------------------------------------

/// D3866: the catalog caches whose PROGRAM STAMP (`row_gen`: the generation a Gravity program's recorded dependency on
/// one of their rows holds, gravity_deps `Deps`) moves only when a committed row changes, not when a row is created or
/// removed: pg_proc (PROCOID 47) and pg_type (TYPEOID 82), `cache_syscache::cacheinfo` ids, pinned by a test there. Every
/// CREATE / DROP of a table, view, composite type or function inserts or deletes their rows; under the whole-cache
/// generation each one moved every program's stamp, and the Rails and Django suites lowered about 16k programs again after
/// DDL that none of them read (realapp speed, 2026-10-11). Both caches are keyed by OID alone: a program read the rows of
/// objects its statement names (or that those depend on, which a DROP removes first), a row created later has an OID no
/// filed program names, and a statement can no longer name a removed object, so neither changes what a program under its
/// key read. An OID is handed out again only after the OID counter wraps (`note_oid_wrap`), which the stamp mixes too.
/// Every other cache's row moves at each of its messages (C3 S5b). C3: per database, as the program generations.
pub const ROW_STAMPED_CAT_IDS: [i32; 2] = [47, 82];

/// C3 S5b: the catalog rows' program generations (`row_gen`), keyed by (cache, hash value) and database (`KeyedGens`).
static ROW_GENS: KeyedGens = KeyedGens::new();
static OID_WRAPS: AtomicU64 = AtomicU64::new(0);

/// D3866: `cache_id` is one of `ROW_STAMPED_CAT_IDS`.
#[inline]
pub fn row_stamped(cache_id: i32) -> bool {
    ROW_STAMPED_CAT_IDS.contains(&cache_id)
}

#[inline]
fn row_key(cache_id: i32, hash: u32) -> u64 {
    (u64::from(cache_id as u32) << 32) | u64::from(hash)
}

/// C3 S5b, S5e: the generations catalog row `hash` of cache `cache_id` reads in database `db` (`row_gen`'s: its slot of
/// the keyed table and its every-database stripe; tests and reports).
pub fn row_slots_of(cache_id: i32, hash: u32, db: Oid) -> (usize, usize) {
    KeyedGens::slots(row_key(cache_id, hash), db)
}

/// C3 S5b: the program generation of catalog row `hash` of cache `cache_id` in database `db`: what a Gravity program's
/// recorded read of the row holds (gravity_deps `Dep::Cat`). A row of a cache not row-stamped moves with each message of
/// that row (`invalidate_key`, after the cache's generation): a message of another row of the cache moves only its own
/// (the whole cache's moves are its list reads', `prog_gen`). D3866: a row-stamped cache's (`ROW_STAMPED_CAT_IDS`) moves
/// only by `bump_stamp`: a committed transaction that changed a row of it that was committed before (an update, an
/// in-place update, an invalidation a command forces on an unchanged row: ALTER DOMAIN's), any batch whose origin is
/// unknown (a prepared transaction's, a replayed one's). Every row's moves with `bump_all_cat`. Two rows whose keys
/// collide in one of `KeyedGens`' two tables share that half's moves; a read moved only when both halves did
/// (`keyed_stands`, D4028: a superset, never a missed change). A reader reads it BEFORE its lag check on the cache's
/// generation (`view_gen` == `current_gen`): the sender bumps the generation first, so a reader that sees a moved row
/// sees the moved generation.
#[inline]
pub fn row_gen(cache_id: i32, hash: u32, db: Oid) -> u64 {
    ROW_GENS.gen(row_key(cache_id, hash), db)
}

/// D3866: move row `hash` of row-stamped cache `cache_id` in database `db` (the message's; InvalidOid, a shared
/// catalog's: every database's) (sender side, after its generation's bump, `invalidate_key`, and before the message is
/// queued).
pub fn bump_stamp(cache_id: i32, hash: u32, db: Oid) {
    ROW_GENS.bump(row_key(cache_id, hash), db);
}

// ---------------------------------------------------------------------------
// Program generations (C3: gravity_deps, per database)
// ---------------------------------------------------------------------------

/// C3: the database buckets of the catalog caches' program generations (`prog_gen` of a `Domain::Cat`; a relation's and
/// a row's are keyed by database too, `KeyedGens`): a database's catalog changes move its own bucket only. A program filed in one database reads that database's catalog (the program tiers key by it), so DDL
/// in another moved nothing it read; under the domains' generations alone, which are process-wide, the Django suite's
/// four test databases retired each other's programs (C3 S4b, the realapp retire rate: 3000 more lowerings than the
/// relcache-epoch scheme, whose callbacks only ever saw the backend's own database). Two databases in one bucket share
/// its moves (a superset: never a missed change).
pub const PROG_DB_BUCKETS: usize = 64;

#[allow(clippy::declare_interior_mutable_const)]
const PROG_ROW: [AtomicU64; PROG_DB_BUCKETS] = [ZERO; PROG_DB_BUCKETS];
static PROG_CAT_GENS: [[AtomicU64; PROG_DB_BUCKETS]; CAT_DOMAINS] = [PROG_ROW; CAT_DOMAINS];
/// C3: the relations' program generations, keyed by relation and database (`KeyedGens`).
static PROG_REL_GENS: KeyedGens = KeyedGens::new();
/// C3 S5 (D3889): a relation's storage generations (`stor_gen`), keyed as `PROG_REL_GENS`.
static STOR_GENS: KeyedGens = KeyedGens::new();

/// C3 S5e: the slots of each of `KeyedGens`' two tables (D4028: two u32 tables, 512 KiB of generations together).
pub const KEYED_SLOTS: usize = 1 << 16;
/// C3 S5e: the every-database stripes of `KeyedGens`.
pub const ALL_DB_STRIPES: usize = 1024;

/// C3 S5e: program generations keyed by (object, database): one table hashed over both (`KEYED_SLOTS`), so two objects,
/// or one object in two databases, share a generation only when their keys collide in it, plus one generation per
/// stripe of objects that every database reads (`ALL_DB_STRIPES`: a message of a shared catalog or relation,
/// InvalidOid, and `bump_all`). Before, a relation's was its stripe of 1024 (relid & 1023) in a database bucket, so every
/// 1024th OID the suites' DDL handed out moved the programs over the relation that shared its stripe (Django's suite
/// creates 4555 tables and 3420 indexes; catalogs that introspection reads, pg_class among them, were in the top
/// retirements).
///
/// D4028: TWO such tables of independent mixes, each a 32-bit half of what a reader holds (`gen`: the first table's
/// slot plus stripe in the high half, the second's in the low). A change of a key moves both of its halves; a key read
/// moved only when BOTH halves moved (`keyed_stands`), so a collision retires a reader only when the key collides in both
/// tables. At DDL rates one table was not enough: a Rails run sends about 10^5 row messages into 65536 slots, every
/// slot moved a few times, and the rows nearly every program reads (the builtin types and procs) retired programs at
/// each move of their slot (707 of 2385 stale retirements on the D3888 index part named a TYPEOID row, all builtin).
/// The halves are u32 counters, the same 512 KiB as the one u64 table they replace.
struct KeyedGens {
    flat: [[AtomicU32; KEYED_SLOTS]; 2],
    all_db: [[AtomicU32; ALL_DB_STRIPES]; 2],
}

#[allow(clippy::declare_interior_mutable_const)]
const ZERO32: AtomicU32 = AtomicU32::new(0);
#[allow(clippy::declare_interior_mutable_const)]
const ZERO_SLOTS: [AtomicU32; KEYED_SLOTS] = [ZERO32; KEYED_SLOTS];
#[allow(clippy::declare_interior_mutable_const)]
const ZERO_STRIPES: [AtomicU32; ALL_DB_STRIPES] = [ZERO32; ALL_DB_STRIPES];

/// D4028: the two tables' mixes (odd multipliers; the database mixed in by its own constant per table).
const KEYED_MIX: [(u64, u64); 2] = [(0x9E37_79B9_7F4A_7C15, 0xD6E8_FEB8_6659_FD93), (0xC2B2_AE3D_27D4_EB4F, 0xFF51_AFD7_ED55_8CCD)];

impl KeyedGens {
    const fn new() -> Self {
        Self { flat: [ZERO_SLOTS; 2], all_db: [ZERO_STRIPES; 2] }
    }

    /// The slot of `key` in database `db` in table `t`, and the stripe of `key` every database reads there.
    #[inline]
    fn slots_in(t: usize, key: u64, db: Oid) -> (usize, usize) {
        let (m, dm) = KEYED_MIX[t];
        let mix = |k: u64, bits: u32| (k.wrapping_mul(m) >> (64 - bits)) as usize;
        (mix(key ^ u64::from(db).wrapping_mul(dm), KEYED_SLOTS.trailing_zeros()), mix(key, ALL_DB_STRIPES.trailing_zeros()))
    }

    /// The first table's slot and stripe of `key` in `db` (tests and reports; `slots_in` for either table).
    #[inline]
    fn slots(key: u64, db: Oid) -> (usize, usize) {
        Self::slots_in(0, key, db)
    }

    /// Two keys (or one key in two databases) share no slot in either table, and two keys no every-database stripe
    /// (one key's stripe is every database's by design) (tests: reads that must move apart).
    #[cfg(test)]
    fn apart(a: u64, da: Oid, b: u64, db: Oid) -> bool {
        (0..2).all(|t| {
            let (x, y) = (Self::slots_in(t, a, da), Self::slots_in(t, b, db));
            x.0 != y.0 && (a == b || x.1 != y.1)
        })
    }

    #[inline]
    fn half(&self, t: usize, key: u64, db: Oid) -> u32 {
        let (slot, stripe) = Self::slots_in(t, key, db);
        self.flat[t][slot].load(Ordering::Acquire).wrapping_add(self.all_db[t][stripe].load(Ordering::Acquire))
    }

    #[inline]
    fn gen(&self, key: u64, db: Oid) -> u64 {
        (u64::from(self.half(0, key, db)) << 32) | u64::from(self.half(1, key, db))
    }

    /// Move `key`'s generation in database `db` (InvalidOid: every database's): both halves.
    fn bump(&self, key: u64, db: Oid) {
        for t in 0..2 {
            let (slot, stripe) = Self::slots_in(t, key, db);
            let g = if db == InvalidOid { &self.all_db[t][stripe] } else { &self.flat[t][slot] };
            g.fetch_add(1, Ordering::SeqCst);
        }
        committed_moved();
    }

    /// Move every key's generation in every database.
    fn bump_all(&self) {
        for g in self.all_db.iter().flatten() {
            g.fetch_add(1, Ordering::SeqCst);
        }
        committed_moved();
    }
}

/// D4028: a keyed generation read at `then` stands at `now` (`row_gen`, `prog_gen` of a relation, `stor_gen`): unless
/// BOTH halves moved. A change of the key moves both, so none is missed; a collision in one table moves one.
#[inline]
pub fn keyed_stands(then: u64, now: u64) -> bool {
    then == now || (then >> 32) == (now >> 32) || (then as u32) == (now as u32)
}

/// D4028: the older of two keyed generations of one key, half by half (`Deps` merge: each half only grows).
#[inline]
pub fn keyed_older(a: u64, b: u64) -> u64 {
    ((a >> 32).min(b >> 32) << 32) | u64::from((a as u32).min(b as u32))
}

#[inline]
fn db_bucket(db: Oid) -> usize {
    ((u64::from(db)).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - PROG_DB_BUCKETS.trailing_zeros())) as usize
}

fn bump_buckets(g: &[AtomicU64; PROG_DB_BUCKETS], db: Oid) {
    if db == InvalidOid {
        g.iter().for_each(|b| {
            b.fetch_add(1, Ordering::SeqCst);
        });
    } else {
        g[db_bucket(db)].fetch_add(1, Ordering::SeqCst);
    }
    committed_moved();
}


/// C3: `domain`'s program generation in database `db`: what a Gravity program's recorded dependency holds (gravity_deps
/// `Deps`). It moves with the domain's generation (`invalidate_key`, after it: a reader that sees it moved sees the domain
/// moved, and lags until it processes the message) but only for a message of `db` or of a shared catalog / relation
/// (InvalidOid: every database's), and with every `bump_all_cat` / `bump_all_rel`.
#[inline]
pub fn prog_gen(domain: Domain, db: Oid) -> u64 {
    match domain {
        Domain::Cat(id) => usize::try_from(id).ok().and_then(|i| PROG_CAT_GENS.get(i)).map_or(0, |g| g[db_bucket(db)].load(Ordering::Acquire)),
        Domain::Rel(relid) => PROG_REL_GENS.gen(u64::from(relid), db),
    }
}

/// C3: move `domain`'s program generation in database `db` (InvalidOid: every database's).
pub fn bump_prog(domain: Domain, db: Oid) {
    match domain {
        Domain::Cat(id) => {
            if let Some(g) = usize::try_from(id).ok().and_then(|i| PROG_CAT_GENS.get(i)) {
                bump_buckets(g, db);
            }
        }
        Domain::Rel(relid) => PROG_REL_GENS.bump(u64::from(relid), db),
    }
}

/// C3 S5 (D3889): relation `relid`'s storage generation in database `db`: what a lowering that read the relation's
/// storage or its size statistics (pg_class relfilenode, relpages, reltuples, relallvisible) holds (gravity_deps
/// `Dep::Stats`). It moves with every relcache message of the relation and with one its sender scoped to the storage
/// (`invalidate_rel_part`), which leaves the program generation of the relation's entry where it was.
#[inline]
pub fn stor_gen(relid: Oid, db: Oid) -> u64 {
    STOR_GENS.gen(u64::from(relid), db)
}

/// C3 S5 (D3890, D3889): a relcache message of relation `rel` whose sender changed only a part of the entry (inval's
/// scopes): its triggers' enabled states (ALTER TABLE .. ENABLE / DISABLE TRIGGER, whose pg_trigger rows no syscache
/// holds: no program, memo or parked Node holds one, the executor and the hosts read it where the trigger fires,
/// `TriggerEnabled` on the descriptor each bind takes, and a lowering reads the triggers' kinds only), or its storage
/// and size statistics (`storage`: TRUNCATE, a REINDEX). The L2 relcache moves as for any message (the entry is
/// rebuilt), and with `storage` the storage generation; the program generation does not.
pub fn invalidate_rel_part(rel: Oid, db: Oid, storage: bool) {
    let domain = Domain::Rel(rel);
    bump(domain);
    if storage {
        STOR_GENS.bump(u64::from(rel), db);
    }
    evict_superseded(rel_key(rel, db), current_gen(domain));
}

/// C3 S5 (D3889): a catcache message of row `hash` of cache `cache_id` whose sender changed only the relation's storage
/// columns (a pg_class row: inval's `REL_SCOPE_STORAGE`). The L2 catcache and the cache's list generation move as for
/// any message; the row's program generation (`row_gen`) does not (the storage readers hold `stor_gen`, which the
/// relation's own message of the same update moves).
pub fn invalidate_cat_storage(cache_id: i32, db: Oid, hash: u32) {
    let domain = Domain::Cat(cache_id);
    bump(domain);
    bump_prog(domain, db);
    evict_superseded(cat_key(cache_id, db, hash), current_gen(domain));
}

/// D3866: the OID counter wrapped (varsup `GetNewObjectId` restarting at FirstNormalObjectId): from here an OID a
/// removed object had can name a new one, so every row-stamped program stamp must differ from before (`oid_wraps`).
pub fn note_oid_wrap() {
    OID_WRAPS.fetch_add(1, Ordering::SeqCst);
    committed_moved();
}

/// D4579: how many times a committed generation a recorded read is judged by has moved (`row_gen`, `stor_gen`,
/// `prog_gen`, any database's, and the OID wrap), counted after each move: a holder that read the count before it last
/// judged its reads (gravity_exec's program tier sweep) finds every one of them standing while the count has not moved.
static COMMITTED_MOVES: AtomicU64 = AtomicU64::new(0);

#[inline]
fn committed_moved() {
    COMMITTED_MOVES.fetch_add(1, Ordering::Release);
}

/// D4579: the count `committed_moved` keeps ([`COMMITTED_MOVES`]).
#[inline]
pub fn committed_moves() -> u64 {
    COMMITTED_MOVES.load(Ordering::Acquire)
}

/// D3866: how many times the OID counter has wrapped in this process (`note_oid_wrap`).
#[inline]
pub fn oid_wraps() -> u64 {
    OID_WRAPS.load(Ordering::Acquire)
}

// ---------------------------------------------------------------------------
// The shared map
// ---------------------------------------------------------------------------

/// Map key below the generation: which cache, which database, which hash
/// bucket. Full logical-key equality is the caller's `matches` closure
/// (catcache compares stored keys; relcache keys are exact by relid).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct L2Key {
    pub kind: u8, // 0 = catcache, 1 = relcache
    pub id: u32,  // catcache: cache id; relcache: relid
    pub db: Oid,  // InvalidOid for shared catalogs
    pub hash: u32, // catcache hashValue; relcache: 0
}

pub const KIND_CAT: u8 = 0;
pub const KIND_REL: u8 = 1;

type L2Value = Arc<dyn Any + Send + Sync>;

/// Every catalog miss probes these maps three or four times (lookup, gate,
/// publish, gate release): Fx, not SipHash.
type FxMap<K, V> = HashMap<K, V, FxBuildHasher>;

struct Bucket {
    entries: Vec<(u64, L2Value, usize, memledger::Home)>, // (gen, entry, approx bytes, the publisher's home)
}

const SHARDS: usize = 64;

struct Shard {
    map: Mutex<FxMap<L2Key, Bucket>>,
    /// Build gates of this shard's keys.
    gates: Mutex<FxMap<(L2Key, u64), Arc<GateCell>>>,
    /// Live entry count for this shard (mutated only under `map`'s lock, so it
    /// stays exact); read to decide when the shard is over its budget.
    count: AtomicUsize,
    /// Approximate bytes of this shard's live entries, kept like `count`.
    bytes: AtomicUsize,
}

fn shards() -> &'static [Shard; SHARDS] {
    static SHARDS_CELL: std::sync::OnceLock<Box<[Shard; SHARDS]>> = std::sync::OnceLock::new();
    SHARDS_CELL.get_or_init(|| {
        let v: Vec<Shard> = (0..SHARDS)
            .map(|_| Shard {
                map: Mutex::new(FxMap::default()),
                gates: Mutex::new(FxMap::default()),
                count: AtomicUsize::new(0),
                bytes: AtomicUsize::new(0),
            })
            .collect();
        let boxed: Box<[Shard; SHARDS]> = v.try_into().ok().unwrap();
        boxed
    })
}

/// Global entry budget for the whole shared L2 map (all shards combined).
///
/// The map is process-global and outlives every session, so — unlike the
/// per-backend L1 catcache, which is capped by `catcache_size_limit` — it needs
/// its own bound. Without one, an unprivileged session probing unboundedly many
/// distinct catalog keys (e.g. negative `to_regtype`/`to_regproc` lookups over
/// `generate_series`, each publishing an immortal negative entry, or generation
/// bumps that strand whole domains of superseded entries) grows server heap
/// without limit and can OOM the entire instance (CWE-770).
///
/// Eviction is correctness-preserving: an evicted entry is exactly a laggard L2
/// miss — the next probe re-builds it from the underlying catcache/catalog, the
/// same fallback used for a pruned superseded generation. Because eviction
/// removes arbitrary live entries (including stranded superseded generations),
/// it also reclaims those independently of same-key re-insertion.
///
/// Sized to roughly mirror the catcache ceiling scaled to a shared, all-session
/// store: ~262k entries. Relcache cores run ~1.2 KB, so a many-relation
/// database fills it at ~320 MB; the byte cap (off by default) bounds that
/// instead. `0` disables a cap. `PGRUST_L2_CACHE_MAX_ENTRIES` /
/// `PGRUST_L2_CACHE_MAX_BYTES` override them (env wins if set; cached at first
/// read).
const L2_MAX_ENTRIES_DEFAULT: usize = 262_144;
const L2_MAX_BYTES_DEFAULT: usize = 0;

#[inline]
fn l2_max_entries() -> usize {
    static ENV: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    if let Some(v) = *ENV.get_or_init(|| {
        std::env::var("PGRUST_L2_CACHE_MAX_ENTRIES")
            .ok()
            .and_then(|v| v.trim().parse().ok())
    }) {
        return v;
    }
    L2_MAX_ENTRIES_DEFAULT
}

#[inline]
fn l2_max_bytes() -> usize {
    static ENV: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    if let Some(v) = *ENV.get_or_init(|| {
        std::env::var("PGRUST_L2_CACHE_MAX_BYTES")
            .ok()
            .and_then(|v| v.trim().parse().ok())
    }) {
        return v;
    }
    L2_MAX_BYTES_DEFAULT
}

/// Per-shard slice of a global cap (`0` == cap disabled). Ceil so the sum of
/// per-shard budgets is never below the configured global cap.
#[inline]
fn per_shard(cap: usize) -> usize {
    if cap == 0 {
        return 0;
    }
    cap.div_ceil(SHARDS).max(1)
}

#[inline]
fn shard_budget() -> usize {
    per_shard(l2_max_entries())
}

#[inline]
fn shard_byte_budget() -> usize {
    per_shard(l2_max_bytes())
}

#[derive(Clone, Copy)]
struct ShardBudget {
    entries: usize,
    bytes: usize,
}

impl ShardBudget {
    fn current() -> Self {
        ShardBudget { entries: shard_budget(), bytes: shard_byte_budget() }
    }

    fn exceeded(self, count: &AtomicUsize, bytes: &AtomicUsize) -> bool {
        (self.entries != 0 && count.load(Ordering::Relaxed) > self.entries)
            || (self.bytes != 0 && bytes.load(Ordering::Relaxed) > self.bytes)
    }
}

/// Evict arbitrary live entries from a shard's `map` until its `count` and
/// `bytes` are back within `budget`, keeping both exact. Returns
/// `(entries_removed, bytes_removed)` so the caller can apply the same deltas
/// to the global stats.
///
/// Called under the shard lock right after an insert overruns the budget.
/// Victims follow the map's iteration order — correctness-preserving
/// regardless of which entries are dropped (an evicted entry is just a laggard
/// miss that re-fetches from the catalog).
fn evict_shard(
    map: &mut FxMap<L2Key, Bucket>,
    count: &AtomicUsize,
    bytes: &AtomicUsize,
    budget: ShardBudget,
    unlink: fn(L2Value, usize, &memledger::Home),
) -> (usize, usize) {
    let mut n_removed = 0usize;
    let mut bytes_removed = 0usize;
    let mut empty: Vec<L2Key> = Vec::new();
    for (k, b) in map.iter_mut() {
        if !budget.exceeded(count, bytes) {
            break;
        }
        while let Some((_, e, sz, home)) = b.entries.pop() {
            n_removed += 1;
            bytes_removed += sz;
            count.fetch_sub(1, Ordering::Relaxed);
            bytes.fetch_sub(sz, Ordering::Relaxed);
            unlink(e, sz, &home);
            if !budget.exceeded(count, bytes) {
                break;
            }
        }
        if b.entries.is_empty() {
            empty.push(*k);
        }
    }
    for k in empty {
        map.remove(&k);
    }
    (n_removed, bytes_removed)
}

#[inline]
fn shard_of(key: &L2Key) -> &'static Shard {
    // Cheap mix; the map inside re-hashes properly.
    let h = (key.id as usize)
        .wrapping_mul(0x9E37_79B9)
        .wrapping_add(key.hash as usize)
        .wrapping_add((key.kind as usize) << 5)
        .wrapping_add(key.db as usize);
    &shards()[(h >> 7) & (SHARDS - 1)]
}

// Stats (memory gate: the L2's total size counted once).
static ENTRIES: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);
static INSERTS: AtomicU64 = AtomicU64::new(0);
static HERD_WAITS: AtomicU64 = AtomicU64::new(0);

/// C2: the catalog L2's account in the memory ledger. An entry's approximate bytes (`BYTES`'s count) move onto it at the
/// publish, from the publishing thread's account where its build allocated them, and back to the unlinking thread's at
/// an unlink (a prune, an eviction, an invalidation, a clear). No single drop point (a backend's L1 or a reader may still
/// hold the entry's `Arc`), as the zone store's. C8 move 7: they move through the L2's one charge (`HELD`).
#[cfg(test)]
const LEDGER: memledger::Acct = memledger::Acct::of(memledger::Consumer::CatalogL2);

/// C8: the published entries' bytes on the catalog L2's owner (opened at the first publish).
static HELD: Mutex<Option<memledger::Charge>> = Mutex::new(None);

/// `bytes` of an entry published: on the census and the L2's ledger account.
#[inline]
fn count_in(bytes: usize) {
    BYTES.fetch_add(bytes, Ordering::Relaxed);
    let owner = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::CatalogL2));
    let mut held = HELD.lock().unwrap_or_else(|e| e.into_inner());
    held.get_or_insert_with(|| memledger::Charge::open(owner, memledger::Release::Evict, memledger::Kind::Region, memledger::charge::Door::Heap, 0))
        .commit(bytes as u64);
}

/// `bytes` of entries unlinked: off the census and back to the calling thread's account (`unlink`: the entry's home).
#[inline]
fn count_out(bytes: usize) {
    BYTES.fetch_sub(bytes, Ordering::Relaxed);
    if let Some(held) = HELD.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        held.free(bytes as u64);
    }
}

/// An entry unlinked, inside the home it was published from (D4552 (a)): its `bytes` leave the L2's charge for that
/// account, and when the L2 held its last `Arc` its blocks are freed there too. Never the unlinking thread's (a DROP
/// DATABASE's forget, an eviction, the ladder's trim): the estimate went back to it while door 1 took the real blocks
/// off it, so each entry left (blocks - estimate) below zero there and as much on its publisher.
fn unlink(entry: L2Value, bytes: usize, home: &memledger::Home) {
    let _home = home.enter();
    count_out(bytes);
    drop(entry);
}

/// The L2's own structure (its shards' tables and each key's bucket of entries) in the L2's account, made and freed there
/// whichever thread publishes or unlinks: a bucket made by one session's publish was freed by another's unlink (a DROP
/// DATABASE's forget frees every bucket of the database), below that thread's base once `unlink` stopped handing it the
/// estimate. Entered around every change of a shard's map; an entry's own bytes still move in its home (`unlink`, the
/// publish's `count_in`), entered inside this one.
#[inline]
fn structure() -> Option<memledger::Entered> {
    memledger::Home::of(memledger::Acct::of(memledger::Consumer::CatalogL2)).enter()
}

pub struct L2Stats {
    pub entries: usize,
    pub bytes: usize,
    pub hits: u64,
    pub misses: u64,
    pub inserts: u64,
    pub herd_waits: u64,
}

pub fn stats() -> L2Stats {
    // the calling thread's held-back hits in (memledger::value folds a thread's hits every FOLD)
    memledger::value::flush();
    L2Stats {
        entries: ENTRIES.load(Ordering::Relaxed),
        bytes: BYTES.load(Ordering::Relaxed),
        hits: memledger::value::stats(memledger::Consumer::CatalogL2).hits,
        misses: memledger::value::stats(memledger::Consumer::CatalogL2).misses,
        inserts: INSERTS.load(Ordering::Relaxed),
        herd_waits: HERD_WAITS.load(Ordering::Relaxed),
    }
}

/// Look up `key` at generation `gen`. `matches` performs full logical-key
/// comparison (hash collisions share a bucket).
pub fn lookup(
    key: L2Key,
    gen: u64,
    mut matches: impl FnMut(&(dyn Any + Send + Sync)) -> bool,
) -> Option<L2Value> {
    memledger::value::lookup(memledger::Consumer::CatalogL2, value_key(&key, gen), 0);
    let shard = shard_of(&key);
    let map = shard.map.lock().unwrap();
    let hit = map.get(&key).and_then(|b| {
        b.entries
            .iter()
            .find(|(g, e, _, _)| *g == gen && matches(e.as_ref()))
            .map(|(_, e, _, _)| Arc::clone(e))
    });
    drop(map);
    if hit.is_some() {
        memledger::value::hit(memledger::Consumer::CatalogL2);
    } else {
        // the lookups' counters and a miss's price, timed to this key's publish (`insert`)
        memledger::value::miss(memledger::Consumer::CatalogL2, value_key(&key, gen));
    }
    hit
}

/// The word a miss's timer is keyed by (`memledger::value`): the key at its generation.
#[inline]
fn value_key(key: &L2Key, gen: u64) -> u64 {
    ((u64::from(key.id) << 32) | u64::from(key.hash)) ^ ((u64::from(key.db) << 8) | u64::from(key.kind)).rotate_left(17) ^ gen.rotate_left(41)
}

/// Publish an entry at (key, gen). `matches` identifies entries with the same
/// LOGICAL key (across generations) for pruning: after the insert, only the
/// two newest generations of that logical key are kept reachable — anything
/// older is unlinked (still alive while referenced; a laggard reader that
/// misses simply falls back to a private build, today's exact semantics).
///
/// If an entry with the same (gen, logical key) is already present (a benign
/// publish race between two builders), the existing entry wins and is
/// returned so racers converge on one allocation.
pub fn insert(
    key: L2Key,
    gen: u64,
    entry: L2Value,
    bytes: usize,
    mut matches: impl FnMut(&(dyn Any + Send + Sync)) -> bool,
) -> L2Value {
    let shard = shard_of(&key);
    let home = memledger::Home::here();
    let _l2 = structure();
    let mut map = shard.map.lock().unwrap();
    let bucket = map.entry(key).or_insert_with(|| Bucket { entries: Vec::new() });

    if let Some((_, existing, _, _)) = bucket
        .entries
        .iter()
        .find(|(g, e, _, _)| *g == gen && matches(e.as_ref()))
    {
        return Arc::clone(existing);
    }

    bucket.entries.push((gen, Arc::clone(&entry), bytes, home));
    ENTRIES.fetch_add(1, Ordering::Relaxed);
    {
        // the estimate moves off the publishing thread's account, where its build allocated the entry
        let _home = home.enter();
        count_in(bytes);
    }
    memledger::value::fill(memledger::Consumer::CatalogL2, value_key(&key, gen), bytes as u64);
    INSERTS.fetch_add(1, Ordering::Relaxed);
    shard.count.fetch_add(1, Ordering::Relaxed);
    shard.bytes.fetch_add(bytes, Ordering::Relaxed);

    // Prune this logical key down to its two newest generations (a bucket of
    // two entries or fewer holds nothing to prune).
    if bucket.entries.len() > 2 {
        let mut newest = 0u64;
        let mut second = 0u64;
        for (g, e, _, _) in bucket.entries.iter() {
            if matches(e.as_ref()) {
                if *g > newest {
                    second = newest;
                    newest = *g;
                } else if *g > second && *g < newest {
                    second = *g;
                }
            }
        }
        for (_, e, sz, home) in bucket.entries.extract_if(.., |(g, e, _, _)| matches(e.as_ref()) && *g != newest && *g != second) {
            ENTRIES.fetch_sub(1, Ordering::Relaxed);
            shard.count.fetch_sub(1, Ordering::Relaxed);
            shard.bytes.fetch_sub(sz, Ordering::Relaxed);
            unlink(e, sz, &home);
        }
    }

    // Enforce the global entry budget. Same-key pruning above only reclaims
    // superseded generations of THIS logical key; without a hard cap, entries
    // whose keys are never re-inserted (attacker-driven negative lookups,
    // generation-stranded entries of other domains) accumulate forever. Evict
    // arbitrary entries from this shard until it is back within its share of
    // the budget — a miss simply re-fetches from the underlying cache/catalog.
    let budget = ShardBudget::current();
    if budget.exceeded(&shard.count, &shard.bytes) {
        let (n, _) = evict_shard(&mut map, &shard.count, &shard.bytes, budget, unlink);
        ENTRIES.fetch_sub(n, Ordering::Relaxed);
    }
    entry
}

/// Sender-side form of [`bump`] for an invalidation that names one key (a
/// catcache message's hash, a relcache message's relid): bump the domain, then
/// unlink every superseded generation stored under that key. Pruning on
/// insert only reaches keys that are looked up again; the key of a dropped
/// object never is, so without this its entries stay until budget eviction.
pub fn invalidate_key(domain: Domain, key: L2Key) {
    bump(domain);
    // C3: the program generation of the message's database, after the domain's (`prog_gen`); a relation's storage
    // generation too (`stor_gen`: a whole entry's message changes it as well)
    bump_prog(domain, key.db);
    match domain {
        Domain::Rel(rel) => STOR_GENS.bump(u64::from(rel), key.db),
        // C3 S5b: the message's row (a row-stamped cache's moves by `bump_stamp` only)
        Domain::Cat(id) if !row_stamped(id) => ROW_GENS.bump(row_key(id, key.hash), key.db),
        Domain::Cat(_) => {}
    }
    evict_superseded(key, current_gen(domain));
}

fn evict_superseded(key: L2Key, gen: u64) {
    let shard = shard_of(&key);
    let _l2 = structure();
    let mut map = shard.map.lock().unwrap();
    let Some(bucket) = map.get_mut(&key) else { return };
    for (_, e, sz, home) in bucket.entries.extract_if(.., |(g, _, _, _)| *g < gen) {
        ENTRIES.fetch_sub(1, Ordering::Relaxed);
        shard.count.fetch_sub(1, Ordering::Relaxed);
        shard.bytes.fetch_sub(sz, Ordering::Relaxed);
        unlink(e, sz, &home);
    }
    if bucket.entries.is_empty() {
        map.remove(&key);
    }
}

// D4579: installed by Gravity's executor: the process-wide program tier lets go the programs lowered in a dropped
// database (`forget_database`; no relcache message names a dropped database's relations).
static FORGET_DB_HOOK: std::sync::atomic::AtomicPtr<()> = std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

pub fn set_forget_database_hook(f: fn(Oid)) {
    FORGET_DB_HOOK.store(f as *mut (), Ordering::Release);
}

/// Drop every entry keyed to database `db` (DROP DATABASE). Nothing can look
/// them up again, and without this they stay until budget eviction reaches
/// them (C frees a database's cache entries with the backends that held them).
/// D4579: then the forget-database hook's owner drops what it keeps for `db`.
pub fn forget_database(db: Oid) {
    if db == InvalidOid {
        return;
    }
    forget_database_entries(db);
    let p = FORGET_DB_HOOK.load(Ordering::Acquire);
    if !p.is_null() {
        // SAFETY: only set_forget_database_hook stores here, always a fn(Oid).
        let f: fn(Oid) = unsafe { std::mem::transmute(p) };
        f(db);
    }
}

fn forget_database_entries(db: Oid) {
    let _l2 = structure();
    for s in shards().iter() {
        let mut map = s.map.lock().unwrap();
        for (_, b) in map.extract_if(|k, _| k.db == db) {
            for (_, e, sz, home) in b.entries {
                ENTRIES.fetch_sub(1, Ordering::Relaxed);
                s.count.fetch_sub(1, Ordering::Relaxed);
                s.bytes.fetch_sub(sz, Ordering::Relaxed);
                unlink(e, sz, &home);
            }
        }
    }
}

/// The L2 key a catcache invalidation message names.
pub fn cat_key(cache_id: i32, db: Oid, hash: u32) -> L2Key {
    L2Key { kind: KIND_CAT, id: cache_id as u32, db, hash }
}

/// The L2 key a relcache invalidation message names.
pub fn rel_key(relid: Oid, db: Oid) -> L2Key {
    L2Key { kind: KIND_REL, id: relid, db, hash: 0 }
}

/// D4298 (T4, the memory ladder's R2a tail): unlink live entries until the L2's bytes are at most `target`, shard by shard
/// (`trim_shard`: `evict_shard`'s road, so an unlinked entry is exactly a laggard miss re-built from the catalog). A shard
/// whose lock is held is skipped (never waited on: the ladder runs this from any thread). The bytes unlinked; the memory
/// returns as the last `Arc` of each entry drops (an L1 may still hold one).
pub fn trim_to(target: usize) -> usize {
    let mut freed = 0usize;
    let _l2 = structure();
    for s in shards().iter() {
        let over = BYTES.load(Ordering::Relaxed).saturating_sub(target);
        if over == 0 {
            break;
        }
        let Ok(mut map) = s.map.try_lock() else { continue };
        let (n, b) = trim_shard(&mut map, &s.count, &s.bytes, over, unlink);
        drop(map);
        ENTRIES.fetch_sub(n, Ordering::Relaxed);
        freed += b;
    }
    freed
}

/// `trim_to`'s step on one shard: unlink up to `over` of its bytes (all of them when it holds no more). `(entries, bytes)`.
fn trim_shard(
    map: &mut FxMap<L2Key, Bucket>,
    count: &AtomicUsize,
    bytes: &AtomicUsize,
    over: usize,
    unlink: fn(L2Value, usize, &memledger::Home),
) -> (usize, usize) {
    let keep = bytes.load(Ordering::Relaxed).saturating_sub(over);
    // a byte budget of 0 means no cap: keep 1 byte at most instead (every entry holds at least one)
    evict_shard(map, count, bytes, ShardBudget { entries: 0, bytes: keep.max(1) }, unlink)
}

/// Test/debug: drop every entry (does not touch generations or views).
pub fn clear_all() {
    let _l2 = structure();
    for s in shards().iter() {
        let mut map = s.map.lock().unwrap();
        for (_, b) in map.drain() {
            for (_, e, sz, home) in b.entries {
                ENTRIES.fetch_sub(1, Ordering::Relaxed);
                s.count.fetch_sub(1, Ordering::Relaxed);
                s.bytes.fetch_sub(sz, Ordering::Relaxed);
                unlink(e, sz, &home);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Per-key build gates (thundering-herd fix)
// ---------------------------------------------------------------------------

struct GateCell {
    owner: std::thread::ThreadId,
    state: Mutex<bool>, // done
    cv: Condvar,
}

#[inline]
fn gates_of(key: &L2Key) -> &'static Mutex<FxMap<(L2Key, u64), Arc<GateCell>>> {
    &shard_of(key).gates
}

pub enum GateOutcome {
    /// This thread builds; publish, then drop the guard (wakes waiters).
    Owner(GateGuard),
    /// The gate owner finished; re-check L2.
    Waited,
    /// The bounded wait expired without the owner finishing (possible
    /// undetected deadlock): fall back to a private build, do not retry.
    TimedOut,
    /// Re-entered while owning this key's gate (recursive build): build
    /// privately, do not wait.
    Recursive,
}

pub struct GateGuard {
    key: (L2Key, u64),
}

impl Drop for GateGuard {
    fn drop(&mut self) {
        let cell = gates_of(&self.key.0).lock().unwrap().remove(&self.key);
        // Waiters clone the cell under the gates lock, so once it is unlinked
        // a count of one means nobody waits: skip the futex wake syscall.
        if let Some(cell) = cell.filter(|c| Arc::strong_count(c) > 1) {
            *cell.state.lock().unwrap() = true;
            cell.cv.notify_all();
        }
    }
}

/// Serialize builders of (key, gen). The wait is bounded (the gate is
/// invisible to the deadlock detector, and the owner may block on a
/// heavyweight lock a waiter holds): on timeout the waiter proceeds with a
/// private build — correct, merely duplicated work.
pub fn acquire_gate(key: L2Key, gen: u64) -> GateOutcome {
    let me = std::thread::current().id();
    let cell = {
        let mut g = gates_of(&key).lock().unwrap();
        match g.get(&(key, gen)) {
            Some(cell) => {
                if cell.owner == me {
                    return GateOutcome::Recursive;
                }
                Arc::clone(cell)
            }
            None => {
                let cell = Arc::new(GateCell {
                    owner: me,
                    state: Mutex::new(false),
                    cv: Condvar::new(),
                });
                g.insert((key, gen), cell);
                return GateOutcome::Owner(GateGuard { key: (key, gen) });
            }
        }
    };
    HERD_WAITS.fetch_add(1, Ordering::Relaxed);
    let mut done = cell.state.lock().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !*done {
        let now = std::time::Instant::now();
        if now >= deadline {
            break;
        }
        let (guard, _timeout) = cell.cv.wait_timeout(done, deadline - now).unwrap();
        done = guard;
    }
    // Distinguish 'owner finished' from 'genuine timeout': the latter must not
    // be retried forever (the gate is invisible to the deadlock detector), so
    // callers fall back to a private build.
    if *done {
        GateOutcome::Waited
    } else {
        GateOutcome::TimedOut
    }
}

#[cfg(test)]
mod tests {
    /// D4579: every committed generation move a recorded read is judged by counts (`committed_moves`): a row's stamp, a
    /// relation's storage, a catalog cache's and a relation's program generation, in one database or in every one.
    #[test]
    fn committed_moves_counts_every_committed_generation_move() {
        let db: Oid = 0xD457;
        let r: Oid = 0xFFFF_D457;
        let c = committed_moves();
        bump_stamp(47, 0xD457_9001, db);
        assert!(committed_moves() > c, "a row's stamp");
        let c = committed_moves();
        invalidate_rel_part(r, db, true);
        assert!(committed_moves() > c, "a relation's storage");
        let c = committed_moves();
        bump_prog(Domain::Cat(47), db);
        assert!(committed_moves() > c, "a catalog cache's program generation");
        let c = committed_moves();
        bump_prog(Domain::Rel(r), InvalidOid);
        assert!(committed_moves() > c, "a relation's program generation, every database's");
        let c = committed_moves();
        let _ = (stor_gen(r, db), prog_gen(Domain::Rel(r), db), row_gen(47, 0xD457_9001, db));
        assert!(committed_moves() >= c, "a read moves nothing (another test's move may)");
    }

    #[test]
    fn out_of_range_cache_id_is_inert() {
        // ids arrive from WAL/2PC/sinval: 96..=127 decode but exceed CAT_DOMAINS.
        for id in [96, 100, 127] {
            super::bump(super::Domain::Cat(id));
            super::sync_view(super::Domain::Cat(id));
            let _ = super::current_gen(super::Domain::Cat(id));
        }
    }

    use super::*;

    /// The tests that move or compare process-wide generations run one at a time: `bump_all_cat` moves every cache's and
    /// every relation stripe, and the others assert what did not move.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[derive(Debug)]
    struct E(u32);

    fn is_e(v: u32) -> impl FnMut(&(dyn Any + Send + Sync)) -> bool {
        move |a| a.downcast_ref::<E>().is_some_and(|e| e.0 == v)
    }

    fn key(id: u32) -> L2Key {
        L2Key { kind: KIND_CAT, id, db: 5, hash: id }
    }

    #[test]
    fn lookup_is_generation_exact() {
        let k = key(1001);
        insert(k, 3, Arc::new(E(1)), 8, is_e(1));
        assert!(lookup(k, 3, is_e(1)).is_some());
        assert!(lookup(k, 2, is_e(1)).is_none(), "older view must not see newer gen");
        assert!(lookup(k, 4, is_e(1)).is_none(), "newer view must not see older gen");
    }

    #[test]
    fn lookups_count_at_the_value_door() {
        // the counters are process-wide and the tests run in parallel: each delta at least this test's own
        let v = || memledger::value::stats(memledger::Consumer::CatalogL2);
        let k = key(1007);
        memledger::value::flush();
        let before = v();
        assert!(lookup(k, 3, is_e(4)).is_none());
        insert(k, 3, Arc::new(E(4)), 24, is_e(4));
        assert!(lookup(k, 3, is_e(4)).is_some());
        let after = stats();
        assert!(after.misses > before.misses && after.hits > before.hits, "a miss, then a hit");
        assert!(v().miss_bytes >= before.miss_bytes + 24, "the publish's bytes");
    }

    #[test]
    fn insert_keeps_two_newest_generations() {
        let k = key(1002);
        insert(k, 1, Arc::new(E(7)), 8, is_e(7));
        insert(k, 2, Arc::new(E(7)), 8, is_e(7));
        insert(k, 3, Arc::new(E(7)), 8, is_e(7));
        assert!(lookup(k, 1, is_e(7)).is_none(), "gen 1 pruned");
        assert!(lookup(k, 2, is_e(7)).is_some());
        assert!(lookup(k, 3, is_e(7)).is_some());
    }

    #[test]
    fn invalidate_key_unlinks_the_named_keys_superseded_entries() {
        let _serial = serial();
        let dom = Domain::Rel(1_000_123);
        let k = rel_key(1_000_123, 5);
        let other = rel_key(1_000_124, 5);
        let g = current_gen(dom);
        insert(k, g, Arc::new(E(1)), 8, is_e(1));
        insert(other, current_gen(Domain::Rel(1_000_124)), Arc::new(E(2)), 8, is_e(2));
        let bucket_len = |key: L2Key| {
            shard_of(&key).map.lock().unwrap().get(&key).map_or(0, |b| b.entries.len())
        };
        assert_eq!(bucket_len(k), 1);
        invalidate_key(dom, k);
        assert!(current_gen(dom) > g);
        assert_eq!(bucket_len(k), 0, "a dropped relation's entry must not outlive its invalidation");
        assert_eq!(bucket_len(other), 1, "keys the message does not name are untouched");
    }

    /// C2: a published entry's bytes move from the publishing thread's memory account onto the L2's, and back to the
    /// unlinking thread's at its unlink (an invalidation here). A gigabyte's estimate, so the other tests' 8-byte entries
    /// and this thread's own allocations stay inside the tolerance.
    #[test]
    fn a_published_entry_is_on_the_catalog_l2_account_until_unlinked() {
        const N: i64 = 1 << 30;
        let me = memledger::Acct::backend(4084);
        let _in = memledger::charge::enter_as(memledger::Owner::Session(me));
        let mine = || {
            memledger::fold();
            memledger::account_row(me).account_bytes
        };
        let l2 = || memledger::account_row(LEDGER).account_bytes;
        let near = |x: i64, want: i64| (x - want).abs() < (1 << 20);
        let (m0, l0) = (mine(), l2());
        let dom = Domain::Rel(1_000_200);
        let k = rel_key(1_000_200, 5);
        insert(k, current_gen(dom), Arc::new(E(3)), N as usize, is_e(3));
        assert!(near(mine() - m0, -N) && near(l2() - l0, N), "published: {} / {}", mine() - m0, l2() - l0);
        invalidate_key(dom, k);
        assert!(near(mine() - m0, 0) && near(l2() - l0, 0), "unlinked: {} / {}", mine() - m0, l2() - l0);
    }

    #[test]
    fn duplicate_publish_converges() {
        let k = key(1003);
        let a = insert(k, 1, Arc::new(E(9)), 8, is_e(9));
        let b = insert(k, 1, Arc::new(E(9)), 8, is_e(9));
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn hash_collisions_are_disambiguated_by_matcher() {
        let k = key(1004);
        insert(k, 1, Arc::new(E(1)), 8, is_e(1));
        insert(k, 1, Arc::new(E(2)), 8, is_e(2));
        assert!(lookup(k, 1, is_e(1)).is_some());
        assert!(lookup(k, 1, is_e(2)).is_some());
        assert!(lookup(k, 1, is_e(3)).is_none());
    }

    #[test]
    fn view_lags_until_sync() {
        let _serial = serial();
        let d = Domain::Cat(90); // slot untouched by other tests
        let v0 = view_gen(d);
        let g = gen_slot(d).fetch_add(1, Ordering::SeqCst) + 1; // foreign bump
        assert_eq!(view_gen(d), v0, "view must not advance before sync");
        sync_view(d);
        assert_eq!(view_gen(d), g);
    }

    #[test]
    fn bump_advances_own_view() {
        let _serial = serial();
        let d = Domain::Rel(0xF000_0001);
        bump(d);
        assert_eq!(view_gen(d), current_gen(d));
    }

    /// C3: a database's catalog change moves its own program generations and stamps, a shared catalog's or relation's
    /// (InvalidOid) every database's; the domain's generation moves either way.
    #[test]
    fn program_generations_are_per_database() {
        let _serial = serial();
        let a: Oid = 16_384;
        let r: Oid = 0x7FFF_0123;
        let b: Oid = (a + 1..a + 1000)
            .find(|&b| db_bucket(b) != db_bucket(a) && KeyedGens::apart(u64::from(r), b, u64::from(r), a))
            .unwrap();
        let d = Domain::Rel(r);
        let (g, ga, gb) = (current_gen(d), prog_gen(d, a), prog_gen(d, b));
        invalidate_key(d, rel_key(r, a));
        assert!(current_gen(d) > g, "the domain moves for every database");
        assert!(prog_gen(d, a) > ga);
        assert_eq!(prog_gen(d, b), gb, "another database's change moves nothing it read");
        invalidate_key(d, rel_key(r, InvalidOid));
        assert!(prog_gen(d, b) > gb, "a shared relation's: every database's");
        let (sa, sb) = (row_gen(47, 0x5A5A_0001, a), row_gen(47, 0x5A5A_0001, b));
        bump_stamp(47, 0x5A5A_0001, b);
        assert!(row_gen(47, 0x5A5A_0001, b) > sb);
        assert!(row_gen(47, 0x5A5A_0001, a) >= sa, "another database's row (only bump_all_cat moves it here)");
        assert_eq!(prog_gen(Domain::Cat(-1), a), 0);
    }

    /// D4028: a key that collides with another in one table only stands across the other's move (one half moved),
    /// and its own move moves both halves; the older of two reads is taken half by half.
    #[test]
    fn a_collision_in_one_table_does_not_move_a_keyed_read() {
        let _serial = serial();
        let db: Oid = 16_384;
        let a = row_key(60, 0x0D40_2800);
        let (s0, s1) = (KeyedGens::slots_in(0, a, db).0, KeyedGens::slots_in(1, a, db).0);
        let b = (1..u32::MAX)
            .map(|h| row_key(60, h))
            .find(|&b| b != a && KeyedGens::slots_in(0, b, db).0 == s0 && KeyedGens::slots_in(1, b, db).0 != s1)
            .unwrap();
        let (ga, gb) = (ROW_GENS.gen(a, db), ROW_GENS.gen(b, db));
        ROW_GENS.bump(b, db);
        let (ga2, gb2) = (ROW_GENS.gen(a, db), ROW_GENS.gen(b, db));
        assert_ne!(ga2, ga, "the shared slot moved a's first half");
        assert!(keyed_stands(ga, ga2), "a moved in one table only: it stands");
        assert!(!keyed_stands(gb, gb2), "b's own move moves both halves");
        ROW_GENS.bump(a, db);
        assert!(!keyed_stands(ga, ROW_GENS.gen(a, db)), "a's own move");
        assert_eq!(keyed_older((5 << 32) | 9, (7 << 32) | 3), (5 << 32) | 3);
    }

    /// C3 S5e: two relations in one stripe of the domains' generations (OIDs 1024 apart) keep their own program
    /// generations; a shared relation's message (InvalidOid) moves every database's.
    #[test]
    fn relations_keep_their_own_program_generations() {
        let _serial = serial();
        let (db, other): (Oid, Oid) = (16_384, 16_385);
        let r: Oid = 0x7FFF_1001;
        let s = (1..64u32)
            .map(|i| r + i * 1024)
            .find(|&s| KeyedGens::apart(u64::from(s), db, u64::from(r), db))
            .unwrap();
        assert_eq!(rel_stripe(r), rel_stripe(s));
        let (gr, gs, go) = (prog_gen(Domain::Rel(r), db), prog_gen(Domain::Rel(s), db), prog_gen(Domain::Rel(r), other));
        invalidate_key(Domain::Rel(s), rel_key(s, db));
        assert_eq!(prog_gen(Domain::Rel(r), db), gr, "another relation's message");
        assert!(prog_gen(Domain::Rel(s), db) > gs);
        invalidate_key(Domain::Rel(r), rel_key(r, InvalidOid));
        assert!(prog_gen(Domain::Rel(r), db) > gr && prog_gen(Domain::Rel(r), other) > go, "every database's");
    }

    /// C3 S5 (D3890, D3889): a relcache message scoped to a part no program holds (the triggers' enabled states) moves
    /// the domain, not the program generation of the entry; a storage-scoped one the storage generation too; a whole
    /// entry's message moves all three.
    #[test]
    fn a_trigger_scoped_message_leaves_the_program_generation() {
        let _serial = serial();
        let (r, db): (Oid, Oid) = (0x7FFF_0457, 16_384);
        let d = Domain::Rel(r);
        let (g, p, st) = (current_gen(d), prog_gen(d, db), stor_gen(r, db));
        invalidate_rel_part(r, db, false);
        assert!(current_gen(d) > g, "the L2 relcache moves");
        assert_eq!((prog_gen(d, db), stor_gen(r, db)), (p, st), "what a lowering read did not change");
        let g = current_gen(d);
        invalidate_rel_part(r, db, true);
        assert!(current_gen(d) > g && stor_gen(r, db) > st, "D3889: a storage-scoped message moves the storage generation");
        assert_eq!(prog_gen(d, db), p, "and not the entry's program generation");
        let st = stor_gen(r, db);
        invalidate_key(d, rel_key(r, db));
        assert!(prog_gen(d, db) > p && stor_gen(r, db) > st);
    }

    /// D3866, C3 S5b: a row's program generation moves with its own messages only (another row's of the cache leaves it,
    /// the cache's list generation moves for both); a row-stamped cache's row moves only by `bump_stamp`, never by an
    /// invalidation of its generation alone; `bump_all_cat` moves every row. The one test here that calls `bump_all_cat`
    /// (every cache's generations and rows: the others' equalities would race it).
    #[test]
    fn a_row_generation_moves_with_its_own_rows_messages() {
        let _serial = serial();
        let db: Oid = 16_384;
        let h1 = 0x00C0_FFEE;
        let (s1, d1) = row_slots_of(40, h1, db);
        let h2 = (h1 + 1..).find(|&h| row_slots_of(40, h, db).0 != s1 && row_slots_of(40, h, db).1 != d1).unwrap();
        let (r1, r2, l) = (row_gen(40, h1, db), row_gen(40, h2, db), prog_gen(Domain::Cat(40), db));
        invalidate_key(Domain::Cat(40), cat_key(40, db, h1));
        assert!(row_gen(40, h1, db) > r1);
        assert_eq!(row_gen(40, h2, db), r2, "another row of the cache did not move");
        assert!(prog_gen(Domain::Cat(40), db) > l, "the cache's lists move");
        invalidate_key(Domain::Cat(40), cat_key(40, InvalidOid, h2));
        assert!(row_gen(40, h2, db) > r2, "a shared catalog's row: every database's");
        assert!(row_stamped(47) && row_stamped(82) && !row_stamped(40));
        let s0 = row_gen(82, 7, 1);
        invalidate_key(Domain::Cat(82), cat_key(82, 1, 7));
        assert_eq!(row_gen(82, 7, 1), s0, "the generation moved, the row stamp did not");
        bump_stamp(82, 7, 1);
        assert!(row_gen(82, 7, 1) > s0);
        // C3 S5 (D3889): a storage-scoped pg_class row message leaves the row's generation, and moves the cache's lists
        let (rg, l) = (row_gen(40, h1, db), prog_gen(Domain::Cat(40), db));
        invalidate_cat_storage(40, db, h1);
        assert_eq!(row_gen(40, h1, db), rg);
        assert!(prog_gen(Domain::Cat(40), db) > l);
        let (s2, r3) = (row_gen(47, 9, 1), row_gen(40, h1, db));
        bump_all_cat();
        assert!(row_gen(47, 9, 1) > s2 && row_gen(40, h1, db) > r3);
        let w = oid_wraps();
        note_oid_wrap();
        assert!(oid_wraps() > w);
    }

    #[test]
    fn gate_owner_then_waiter() {
        let k = key(1005);
        let GateOutcome::Owner(guard) = acquire_gate(k, 1) else {
            panic!("first acquire must own");
        };
        // Same thread, same key: recursive.
        assert!(matches!(acquire_gate(k, 1), GateOutcome::Recursive));
        let t = {
            let k = k;
            std::thread::spawn(move || matches!(acquire_gate(k, 1), GateOutcome::Waited))
        };
        std::thread::sleep(Duration::from_millis(50));
        drop(guard); // wakes the waiter
        assert!(t.join().unwrap());
        // Gate is gone; next acquire owns again.
        assert!(matches!(acquire_gate(k, 1), GateOutcome::Owner(_)));
    }

    /// A local shard's unlink: its entries were never on the global census or the L2's charge.
    fn drop_only(_: L2Value, _: usize, _: &memledger::Home) {}

    /// D4298: the ladder's trim of one shard unlinks up to the bytes asked, all of them when asked for more, with the
    /// count and bytes kept exact (a local shard: the global L2 is shared by the tests running beside).
    #[test]
    fn trim_shard_unlinks_up_to_the_bytes_asked() {
        let mut map: FxMap<L2Key, Bucket> = FxMap::default();
        let (count, bytes) = (AtomicUsize::new(0), AtomicUsize::new(0));
        for i in 0..100u32 {
            let k = L2Key { kind: KIND_CAT, id: 9, db: 1, hash: i };
            map.entry(k).or_insert_with(|| Bucket { entries: Vec::new() }).entries.push((1, Arc::new(E(i)) as L2Value, 100, memledger::Home::here()));
            count.fetch_add(1, Ordering::Relaxed);
            bytes.fetch_add(100, Ordering::Relaxed);
        }
        assert_eq!(trim_shard(&mut map, &count, &bytes, 2_550, drop_only), (26, 2_600), "whole entries until 2 550 bytes are gone");
        assert_eq!((count.load(Ordering::Relaxed), bytes.load(Ordering::Relaxed)), (74, 7_400));
        assert_eq!(trim_shard(&mut map, &count, &bytes, 1 << 20, drop_only), (74, 7_400), "asked past its bytes: all of them");
        assert_eq!((count.load(Ordering::Relaxed), map.values().map(|b| b.entries.len()).sum::<usize>()), (0, 0));
        assert_eq!(trim_shard(&mut map, &count, &bytes, 100, drop_only), (0, 0), "an empty shard: nothing");
    }

    #[test]
    fn evict_shard_bounds_a_shard_to_its_budget() {
        // Simulate an attacker publishing many distinct-key negative entries
        // into one shard. Eviction must bring it back to budget and report the
        // exact deltas, with `count` kept consistent.
        let mut map: FxMap<L2Key, Bucket> = FxMap::default();
        let count = AtomicUsize::new(0);
        let bytes = AtomicUsize::new(0);
        const N: usize = 500;
        const BYTES_EACH: usize = 300;
        for i in 0..N as u32 {
            // Distinct logical keys -> distinct buckets (no same-key pruning),
            // exactly the unbounded-growth vector.
            let k = L2Key { kind: KIND_CAT, id: 7, db: 1, hash: i };
            map.entry(k)
                .or_insert_with(|| Bucket { entries: Vec::new() })
                .entries
                .push((1, Arc::new(E(i)) as L2Value, BYTES_EACH, memledger::Home::here()));
            count.fetch_add(1, Ordering::Relaxed);
            bytes.fetch_add(BYTES_EACH, Ordering::Relaxed);
        }
        assert_eq!(count.load(Ordering::Relaxed), N);

        let budget = 64;
        let caps = ShardBudget { entries: budget, bytes: 0 };
        let (n_removed, bytes_removed) = evict_shard(&mut map, &count, &bytes, caps, drop_only);
        assert_eq!(n_removed, N - budget, "must evict down to exactly budget");
        assert_eq!(bytes_removed, (N - budget) * BYTES_EACH);
        assert_eq!(count.load(Ordering::Relaxed), budget);
        let live: usize = map.values().map(|b| b.entries.len()).sum();
        assert_eq!(live, budget, "live entries match the tracked count");

        // Already within budget: a no-op.
        let (n2, b2) = evict_shard(&mut map, &count, &bytes, caps, drop_only);
        assert_eq!((n2, b2), (0, 0));
        assert_eq!(count.load(Ordering::Relaxed), budget);
        assert_eq!(bytes.load(Ordering::Relaxed), budget * BYTES_EACH);

        // The byte cap binds even when the entry count is within budget.
        let byte_caps = ShardBudget { entries: budget, bytes: 10 * BYTES_EACH };
        let (n3, b3) = evict_shard(&mut map, &count, &bytes, byte_caps, drop_only);
        assert_eq!(n3, budget - 10);
        assert_eq!(b3, (budget - 10) * BYTES_EACH);
        assert_eq!(bytes.load(Ordering::Relaxed), 10 * BYTES_EACH);
        let live: usize = map.values().map(|b| b.entries.len()).sum();
        assert_eq!(live, 10);
    }

    #[test]
    fn forget_database_drops_only_that_databases_entries() {
        let dropped = L2Key { kind: KIND_REL, id: 9001, db: 31_337, hash: 0 };
        let kept = L2Key { kind: KIND_REL, id: 9001, db: 31_338, hash: 0 };
        let shared = L2Key { kind: KIND_CAT, id: 9, db: InvalidOid, hash: 9001 };
        insert(dropped, 1, Arc::new(E(1)), 100, is_e(1));
        insert(kept, 1, Arc::new(E(2)), 100, is_e(2));
        insert(shared, 1, Arc::new(E(3)), 100, is_e(3));
        let before = shard_of(&dropped).bytes.load(Ordering::Relaxed);
        forget_database(31_337);
        assert!(lookup(dropped, 1, is_e(1)).is_none());
        assert!(lookup(kept, 1, is_e(2)).is_some());
        assert!(lookup(shared, 1, is_e(3)).is_some());
        assert!(shard_of(&dropped).bytes.load(Ordering::Relaxed) <= before - 100);
    }

    #[test]
    fn shard_budget_covers_the_configured_cap() {
        // Per-shard budgets must sum to at least the global cap so the cap is
        // never under-enforced, and 0 (disabled) must propagate.
        let b = shard_budget();
        if l2_max_entries() == 0 {
            assert_eq!(b, 0);
        } else {
            assert!(b * SHARDS >= l2_max_entries());
            assert!(b >= 1);
        }
    }

    #[test]
    fn gate_wait_times_out() {
        let k = key(1006);
        let GateOutcome::Owner(_guard) = acquire_gate(k, 2) else {
            panic!("first acquire must own");
        };
        let t = std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let out = acquire_gate(k, 2);
            (matches!(out, GateOutcome::TimedOut), started.elapsed())
        });
        let (waited, dur) = t.join().unwrap();
        assert!(waited);
        assert!(dur >= Duration::from_secs(2));
    }

    // D3888: a catcache bump on an opclass-family syscache moves no relation: the sender names the indexes and
    // partitioned tables built from the family's rows in relcache messages of their own (inval `PreCommit_Inval`).
    #[test]
    fn opclass_catcache_bump_moves_no_rel_stripe() {
        let _serial = serial();
        for id in [3, 4, 5, 13, 14, 41, 42] {
            let before = rel_gen_snapshot();
            bump(Domain::Cat(id));
            assert_eq!(before, rel_gen_snapshot(), "cache {id}");
        }
        let before = rel_gen_snapshot();
        bump_all_cat();
        assert!(before.iter().zip(&rel_gen_snapshot()).all(|(b, a)| a > b), "a catalog-wide message moves them all");
    }
}
