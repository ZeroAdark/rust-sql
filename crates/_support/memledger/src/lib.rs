//! The memory ledger: who holds how many bytes inside Gravity's ONE memory budget B, now. It counts; it decides nothing
//! (every decision is in `mempolicy`, which reads it; the snapshot of B and every consumer is `mempolicy::snapshot`). It
//! depends on nothing but `pgsync`, `pg_clock` and libc (unix), so the allocator's door and every layer above can count into it.
//!
//! - **B** = min(B_auto, `pgrust.memory_target`, the memory governor's bar), B_auto = the memory limit less the operating
//!   system's reserve and the page-cache floor (`mempolicy::sizes::budget_of`). The free reserve (R_low / R_high) lives
//!   inside B.
//! - **Consumers**: each registers hooks (current bytes, the floor it is never reclaimed below, today's cap, and a reclaim
//!   that frees down to a byte count). The hosts and exec register here, as they install `set_idle_shed` and the part
//!   cache's hooks; the driver reads the hooks into a snapshot (`gravity_driver::mem::budget::snapshot`).
//! - **Modes** (`pgrust.memory_broker`): off = nothing computed, today's caps byte for byte; advise (default) = the budget
//!   computed and reported, nothing applied; on = the actuators in `pgrust.memory_broker_actuators` apply it:
//!   - `work_pool`: the grant pool's capacity from B (exec `arm_grant_pool`), and again every broker interval while the
//!     pool's capacity was derived (`pool_follows`);
//!   - `reclaimer` (5b, `mem::reclaim`): a background reclaimer keeps B's free bytes above the reserve's low mark, and a
//!     grant pool draw B has no free bytes for frees managed cache bytes inline first;
//!   - `caches`: the fp tables, the build pool, the zone store and the condition cache are managed while the reclaimer
//!     applies (`declare_managed`): the pool's capacity counts them at their floor, and the reclaimer frees them past it;
//!   - `part_cache` (step 7) declares the part cache the same way.
//!
//!   An unmanaged cache counts at its cap, so the pool never overbooks B with bytes no reclaimer frees.
//!
//! Registration is unconditional (fn pointers only; nothing is read under off), so a SIGHUP from off to advise needs no
//! restart. Snapshots run at the pool's arming, in the REPORT census, every broker interval (6a) and every reclaimer tick
//! (5b).
//!
//! **Accounts** (G1-A): memory is counted where it leaves the operating system, the global allocator (door 1, per
//! allocation: a thread-local add) and every direct mapping (door 2), onto the calling thread's current account: its
//! backend's while it serves a statement, a cache's while the cache allocates, UNBOUND otherwise. A home entered
//! (`Home::enter`) pauses the thread's open cache scopes until it leaves: a block is counted once, on the home's account,
//! never by an enclosing scope's holder as well.

use core::cell::Cell;
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicPtr, AtomicU32, AtomicU64, AtomicU8, Ordering};

use pgsync::Mutex;

pub mod arena;
pub mod charge;
pub mod charged_vec;
pub mod ladder_pass;
pub mod pins;
pub mod owner_end;
pub mod resident;
pub mod unattr;
pub mod value;

pub use charge::{Charge, Holder, Owner, OwnerClass, OwnerRow, Release, Residue};
pub use charged_vec::{ChargedVec, ChargedVecMut, VALUE_FLOOR};
pub use charge::{door_pool, pagepool_total, pool_claimed, pool_door1, pool_probed, pool_reading, pool_unowned, set_pool_door1, set_pool_probe, OwnerLife, PoolReading};

/// `pgrust.memory_broker`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Mode {
    Off = 0,
    Advise = 1,
    On = 2,
}

impl Mode {
    /// The GUC's enum index (guc_tables consts MEMORY_BROKER_*); anything else reads as the default, advise.
    #[inline]
    pub const fn from_index(i: i32) -> Mode {
        match i {
            0 => Mode::Off,
            2 => Mode::On,
            _ => Mode::Advise,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Advise => "advise",
            Mode::On => "on",
        }
    }
}

/// A consumer of memory inside B (plan 1.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Consumer {
    SharedBuffers = 0,
    PartCache = 1,
    FpTables = 2,
    BuildPool = 3,
    ZoneStore = 4,
    CondCache = 5,
    ProgramCaches = 6,
    WorkPool = 7,
    PerBackend = 8,
    /// The shared catalog L2 (backend/utils/cache/l2cache): the catcache / relcache bodies every backend shares.
    CatalogL2 = 9,
    /// md's process-wide relation size cache (backend/storage/smgr/md nblocks_cache): its map's nodes, made and freed in
    /// this holder's home whichever backend inserts or drops a key (a DROP DATABASE's purge frees others' inserts).
    NblocksCache = 10,
    /// pgstat's process-wide shared stats map (backend/utils/activity/pgstat shmem SHARED_STATS): its table and the
    /// per-backend entries' boxes, made and freed in this holder's home whichever session creates or drops an entry (a
    /// standby makes its backend entry inside its leader's task and drops it at its own park; a DROP TABLE / DATABASE drops
    /// entries other sessions made).
    SharedStats = 11,
}

pub const CONSUMERS: usize = 12;

impl Consumer {
    pub const ALL: [Consumer; CONSUMERS] = [
        Consumer::SharedBuffers,
        Consumer::PartCache,
        Consumer::FpTables,
        Consumer::BuildPool,
        Consumer::ZoneStore,
        Consumer::CondCache,
        Consumer::ProgramCaches,
        Consumer::WorkPool,
        Consumer::PerBackend,
        Consumer::CatalogL2,
        Consumer::NblocksCache,
        Consumer::SharedStats,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Consumer::SharedBuffers => "shared_buffers",
            Consumer::PartCache => "part_cache",
            Consumer::FpTables => "fp_tables",
            Consumer::BuildPool => "build_pool",
            Consumer::ZoneStore => "zone_store",
            Consumer::CondCache => "condition_cache",
            Consumer::ProgramCaches => "program_caches",
            Consumer::WorkPool => "work_pool",
            Consumer::PerBackend => "per_backend",
            Consumer::CatalogL2 => "catalog_l2",
            Consumer::NblocksCache => "nblocks_cache",
            Consumer::SharedStats => "shared_stats",
        }
    }

    /// A process cache kept across statements (the rows the work pool's capacity counts at their floor once managed, at
    /// their cap until then).
    #[inline]
    pub const fn is_cache(self) -> bool {
        matches!(self, Consumer::PartCache | Consumer::FpTables | Consumer::BuildPool | Consumer::ZoneStore | Consumer::CondCache)
    }

    /// Fixed per-process and per-backend memory (no actuator; reported and counted). The catalog L2 bounds itself (its
    /// entry and byte caps) and has no reclaim hook: counted at its bytes, as the program caches are.
    #[inline]
    pub const fn is_fixed(self) -> bool {
        matches!(self, Consumer::ProgramCaches | Consumer::PerBackend | Consumer::CatalogL2 | Consumer::NblocksCache | Consumer::SharedStats)
    }

    /// The reclaim class of the consumer's memory (design section 3): an evictable cache, the kept per-backend memory
    /// (WARM), the work pool's running statements, or fixed (sized at start, or with no reclaim hook yet: program code and
    /// the catalog L2 become `Cache` once one exists).
    #[inline]
    pub const fn class(self) -> Class {
        match self {
            Consumer::PartCache | Consumer::FpTables | Consumer::BuildPool | Consumer::ZoneStore | Consumer::CondCache => Class::Cache,
            Consumer::PerBackend => Class::Warm,
            Consumer::WorkPool => Class::Running,
            Consumer::SharedBuffers | Consumer::ProgramCaches | Consumer::CatalogL2 | Consumer::NblocksCache | Consumer::SharedStats => {
                Class::Fixed
            }
        }
    }

    /// The actuator bit that applies the plan to this consumer (0: none).
    #[inline]
    pub const fn actuator(self) -> u8 {
        match self {
            Consumer::SharedBuffers => actuator::SHARED_BUFFERS,
            Consumer::PartCache => actuator::PART_CACHE,
            Consumer::FpTables | Consumer::BuildPool | Consumer::ZoneStore | Consumer::CondCache => actuator::CACHES,
            Consumer::WorkPool => actuator::WORK_POOL,
            Consumer::ProgramCaches | Consumer::PerBackend | Consumer::CatalogL2 | Consumer::NblocksCache | Consumer::SharedStats => 0,
        }
    }
}

/// The reclaim class of an account's bytes (design section 3: how, and whether, the memory comes back), at account level;
/// a backend's statement bytes split further by breaker `Kind` (spillable or pinned by the breaker's map).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Class {
    /// A backend's: its running statement's bytes and what its session keeps between statements.
    Running = 0,
    /// Kept for a repeat, given back by `MADV_DONTNEED` (parked regions, the per-backend kept memory).
    Warm = 1,
    /// A process cache, evicted down to its floor.
    Cache = 2,
    /// Sized at start or held for the process's life (shared_buffers; program code and the catalog L2 until they have a
    /// reclaim hook).
    Fixed = 3,
    /// Left its owner, still resident until a background step lands (the reaper's deferred unmaps).
    Pending = 4,
    /// No owner: threads that never took an account.
    Unbound = 5,
}

pub const CLASSES: usize = 6;

impl Class {
    pub const ALL: [Class; CLASSES] = [Class::Running, Class::Warm, Class::Cache, Class::Fixed, Class::Pending, Class::Unbound];

    pub const fn name(self) -> &'static str {
        match self {
            Class::Running => "running",
            Class::Warm => "warm",
            Class::Cache => "cache",
            Class::Fixed => "fixed",
            Class::Pending => "pending",
            Class::Unbound => "unbound",
        }
    }
}

/// `pgrust.memory_broker_actuators` bits.
pub mod actuator {
    pub const RECLAIMER: u8 = 1;
    pub const CACHES: u8 = 2;
    pub const WORK_POOL: u8 = 4;
    pub const PART_CACHE: u8 = 8;
    pub const SHARED_BUFFERS: u8 = 16;
    pub const ALL: u8 = RECLAIMER | CACHES | WORK_POOL | PART_CACHE | SHARED_BUFFERS;
}

/// `pgrust.memory_broker_actuators` as bits: a comma-separated list; `all` or an empty list = every one. The second value
/// counts the names it did not know (ignored).
pub fn parse_actuators(s: &str) -> (u8, u32) {
    let mut bits = 0u8;
    let mut unknown = 0u32;
    let mut any = false;
    for name in s.split(',').map(str::trim).filter(|n| !n.is_empty()) {
        any = true;
        const NAMES: [(&str, u8); 6] = [
            ("all", actuator::ALL),
            ("reclaimer", actuator::RECLAIMER),
            ("caches", actuator::CACHES),
            ("work_pool", actuator::WORK_POOL),
            ("part_cache", actuator::PART_CACHE),
            ("shared_buffers", actuator::SHARED_BUFFERS),
        ];
        match NAMES.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)) {
            Some((_, b)) => bits |= b,
            None => unknown += 1,
        }
    }
    (if any { bits } else { actuator::ALL }, unknown)
}

/// A consumer's hooks: its bytes now, the floor it is never reclaimed below, today's cap (the off path's own limit; the
/// broker's ceiling is computed by its policy, 6a), and a reclaim that frees down to the given bytes and returns the bytes
/// freed (called by 5b's reclaimer only).
#[derive(Clone, Copy)]
pub struct Hooks {
    pub current: fn() -> u64,
    pub floor: fn() -> u64,
    pub cap: fn() -> u64,
    pub reclaim: Option<fn(u64) -> u64>,
}

/// The broker's settings (`configure`, from the GUCs and the e2e levers).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cfg {
    pub mode: Mode,
    /// `pgrust.memory_target` in bytes (0 = unset).
    pub target: u64,
    /// `pgrust.memory_page_cache_floor_mb` in bytes (0 = derived).
    pub page_cache_floor: u64,
    /// `pgrust.memory_reserve_mb` in bytes (0 = derived).
    pub reserve: u64,
    pub actuators: u8,
}

impl Cfg {
    pub const DEFAULT: Cfg = Cfg { mode: Mode::Advise, target: 0, page_cache_floor: 0, reserve: 0, actuators: actuator::ALL };
}

static HOOKS: Mutex<[Option<Hooks>; CONSUMERS]> = Mutex::new([None; CONSUMERS]);
static TARGETS: [AtomicU64; CONSUMERS] = [const { AtomicU64::new(u64::MAX) }; CONSUMERS];
static LOSSES: [AtomicU64; CONSUMERS] = [const { AtomicU64::new(u64::MAX) }; CONSUMERS];
static MODE: AtomicU8 = AtomicU8::new(Mode::Advise as u8);
static ACTUATORS: AtomicU8 = AtomicU8::new(actuator::ALL);
static TARGET: AtomicU64 = AtomicU64::new(0);
static PAGE_CACHE_FLOOR: AtomicU64 = AtomicU64::new(0);
static RESERVE: AtomicU64 = AtomicU64::new(0);
/// The consumers an actuator manages now (5b's reclaimer and step 7 declare theirs): the work pool's capacity counts a
/// managed cache at its floor, any other at its cap. Nothing declares in step 5.
static MANAGED: AtomicU64 = AtomicU64::new(0);
/// The limit when the memory watchdog has published none (exec's MemTotal read at the pool's arming).
static FALLBACK_LIMIT: AtomicU64 = AtomicU64::new(0);
/// free_B at the last snapshot (u64::MAX: none yet).
static FREE_B: AtomicU64 = AtomicU64::new(u64::MAX);
/// The grant pool's held bytes at the last snapshot (`free_now` subtracts what the pool drew since).
static HELD_AT: AtomicU64 = AtomicU64::new(0);
/// D3994 (level gate): B at the last snapshot (`note_b`; 0 before one): the part release's per-credit short read takes it
/// in place of composing B again from the limit and the knobs.
static B_AT: AtomicU64 = AtomicU64::new(0);
/// The grant pool's capacity was derived at its arming (no explicit setting, no address-space rail): under on with the
/// `work_pool` actuator the broker re-arms it from B every interval (`pool_follows`).
static POOL_DERIVED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static SNAPSHOTS: AtomicU64 = AtomicU64::new(0);
static FITS_REFUSED: AtomicU64 = AtomicU64::new(0);
static DOUBLE_REGISTER: AtomicU64 = AtomicU64::new(0);

/// The broker's settings now (SIGHUP values: the caller reads the GUCs and calls this at the pool's arming, the census and,
/// from 6a, every broker tick).
pub fn configure(cfg: Cfg) {
    MODE.store(cfg.mode as u8, Ordering::Relaxed);
    ACTUATORS.store(cfg.actuators, Ordering::Relaxed);
    TARGET.store(cfg.target, Ordering::Relaxed);
    PAGE_CACHE_FLOOR.store(cfg.page_cache_floor, Ordering::Relaxed);
    RESERVE.store(cfg.reserve, Ordering::Relaxed);
    if cfg.mode == Mode::Off {
        FREE_B.store(u64::MAX, Ordering::Relaxed);
    }
}

pub fn cfg() -> Cfg {
    Cfg {
        mode: mode(),
        target: TARGET.load(Ordering::Relaxed),
        page_cache_floor: PAGE_CACHE_FLOOR.load(Ordering::Relaxed),
        reserve: RESERVE.load(Ordering::Relaxed),
        actuators: ACTUATORS.load(Ordering::Relaxed),
    }
}

/// The actuators' bits now (`actuator`).
#[inline]
pub fn actuators() -> u8 {
    ACTUATORS.load(Ordering::Relaxed)
}

#[inline]
pub fn mode() -> Mode {
    match MODE.load(Ordering::Relaxed) {
        0 => Mode::Off,
        2 => Mode::On,
        _ => Mode::Advise,
    }
}

/// The limit to use when the memory watchdog has published none (0 = unknown).
pub fn set_fallback_limit(bytes: u64) {
    FALLBACK_LIMIT.store(bytes, Ordering::Relaxed);
}

/// Register `c`'s hooks; the first registration wins (a second one is counted and ignored). True when this one took.
pub fn register(c: Consumer, hooks: Hooks) -> bool {
    let mut h = pgsync::lock(&HOOKS);
    let slot = &mut h[c as usize];
    if slot.is_some() {
        DOUBLE_REGISTER.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    *slot = Some(hooks);
    true
}

/// D4353: the bytes `c`'s reclaim can free now, where it knows them better than its current bytes less its floor (the part
/// cache: `PartRegistry::trimmable_bytes`); read by the memory ladder's R2a count only.
static TRIMMABLE: [AtomicPtr<()>; CONSUMERS] = [const { AtomicPtr::new(core::ptr::null_mut()) }; CONSUMERS];

pub fn register_trimmable(c: Consumer, f: fn() -> u64) {
    TRIMMABLE[c as usize].store(f as *mut (), Ordering::Release);
}

/// `c`'s trimmable bytes now, `None` when it registered no count.
pub fn trimmable(c: Consumer) -> Option<u64> {
    let p = TRIMMABLE[c as usize].load(Ordering::Acquire);
    if p.is_null() {
        return None;
    }
    // SAFETY: only register_trimmable stores here, always from fn() -> u64.
    let f: fn() -> u64 = unsafe { core::mem::transmute::<*mut (), fn() -> u64>(p) };
    Some(f())
}

/// D4298 (T4): a holder's reclaim that only the memory ladder runs (R2a's tail, `mempolicy::ladder::TAIL`): its bytes in
/// use (never reclaimed) and a reclaim down to the given bytes returning the bytes freed. Its `Hooks` keep `reclaim: None`,
/// so the broker, the watchdog's backstop and the snapshot read it as before.
static LADDER_FLOOR: [AtomicPtr<()>; CONSUMERS] = [const { AtomicPtr::new(core::ptr::null_mut()) }; CONSUMERS];
static LADDER_RECLAIM: [AtomicPtr<()>; CONSUMERS] = [const { AtomicPtr::new(core::ptr::null_mut()) }; CONSUMERS];

pub fn register_ladder_reclaim(c: Consumer, in_use: fn() -> u64, reclaim: fn(u64) -> u64) {
    LADDER_FLOOR[c as usize].store(in_use as *mut (), Ordering::Release);
    LADDER_RECLAIM[c as usize].store(reclaim as *mut (), Ordering::Release);
}

/// `c`'s ladder-only (bytes in use, reclaim), `None` when it registered none.
pub fn ladder_reclaim_of(c: Consumer) -> Option<(fn() -> u64, fn(u64) -> u64)> {
    let (f, r) = (LADDER_FLOOR[c as usize].load(Ordering::Acquire), LADDER_RECLAIM[c as usize].load(Ordering::Acquire));
    if f.is_null() || r.is_null() {
        return None;
    }
    // SAFETY: only register_ladder_reclaim stores here, always from fn() -> u64 and fn(u64) -> u64.
    Some(unsafe { (core::mem::transmute::<*mut (), fn() -> u64>(f), core::mem::transmute::<*mut (), fn(u64) -> u64>(r)) })
}

pub fn registered(c: Consumer) -> bool {
    pgsync::lock(&HOOKS)[c as usize].is_some()
}

/// `c`'s hooks (5b's reclaimer, tests).
pub fn hooks(c: Consumer) -> Option<Hooks> {
    pgsync::lock(&HOOKS)[c as usize]
}

/// Whether the plan is APPLIED to `c` now: the broker on, `c`'s actuator in the list, `c` registered.
pub fn enforcing(c: Consumer) -> bool {
    mode() == Mode::On && ACTUATORS.load(Ordering::Relaxed) & c.actuator() != 0 && registered(c)
}

/// An actuator declares the consumers it manages (5b, step 7): the work pool then counts them at their floor.
pub fn declare_managed(c: Consumer, on: bool) {
    let bit = 1u64 << (c as u32);
    if on {
        MANAGED.fetch_or(bit, Ordering::Relaxed);
    } else {
        MANAGED.fetch_and(!bit, Ordering::Relaxed);
    }
}

/// The managed consumers' bits now (none unless the broker is on).
#[inline]
pub fn managed_mask() -> u64 {
    if mode() == Mode::On { MANAGED.load(Ordering::Relaxed) } else { 0 }
}

/// Whether an actuator manages `c` now (`declare_managed`, under on): the reclaimer frees only managed consumers.
#[inline]
pub fn managed(c: Consumer) -> bool {
    managed_mask() & (1u64 << (c as u32)) != 0
}

/// The grant pool armed at a derived capacity (exec `arm_grant_pool`): `false` for an explicit capacity or a rail.
pub fn set_pool_derived(derived: bool) {
    POOL_DERIVED.store(derived, Ordering::Relaxed);
}

/// Whether the broker re-arms the grant pool's capacity from B (`Snapshot::work_pool_capacity`) every interval: the pool's
/// capacity was derived and the `work_pool` actuator applies.
#[inline]
pub fn pool_follows() -> bool {
    POOL_DERIVED.load(Ordering::Relaxed) && enforcing(Consumer::WorkPool)
}

/// B's free bytes now, with the grant pool holding `held`: the last snapshot's, less what the pool drew since (`u64::MAX`
/// before any snapshot). The reclaimer's tick refreshes the snapshot every `RECLAIM_TICK_MS` while it applies.
#[inline]
pub fn free_after(held: u64) -> u64 {
    let free = FREE_B.load(Ordering::Relaxed);
    if free == u64::MAX {
        return u64::MAX;
    }
    let drawn = held.saturating_sub(HELD_AT.load(Ordering::Relaxed));
    free.saturating_sub(drawn)
}

/// D3994 (level gate): the last snapshot's B (`note_b`), 0 before one.
#[inline]
pub fn b_at() -> u64 {
    B_AT.load(Ordering::Relaxed)
}

/// D3994 (level gate): the snapshot's B beside its free bytes (`note_snapshot`).
pub fn note_b(b: u64) {
    B_AT.store(b, Ordering::Relaxed);
}

/// The limit when the memory watchdog has published none (`set_fallback_limit`).
pub fn fallback_limit() -> u64 {
    FALLBACK_LIMIT.load(Ordering::Relaxed)
}

/// Every consumer's hooks (the snapshot's reader).
pub fn all_hooks() -> [Option<Hooks>; CONSUMERS] {
    *pgsync::lock(&HOOKS)
}

/// A snapshot taken with the grant pool holding `held`: its free bytes are `free_now`'s base until the next one.
pub fn note_snapshot(held: u64, free_b: u64) {
    HELD_AT.store(held, Ordering::Relaxed);
    FREE_B.store(free_b, Ordering::Relaxed);
    SNAPSHOTS.fetch_add(1, Ordering::Relaxed);
}

/// The broker's target for `c` (None: none set). Read for actuation only when `enforcing(c)`.
#[inline]
pub fn target(c: Consumer) -> Option<u64> {
    match TARGETS[c as usize].load(Ordering::Relaxed) {
        u64::MAX => None,
        t => Some(t),
    }
}

pub fn set_target(c: Consumer, bytes: u64) {
    TARGETS[c as usize].store(bytes.min(u64::MAX - 1), Ordering::Relaxed);
}

/// Broker (b): `c`'s loss at the margin as the last plan published it, ns per MiB (None: not measured, or the knob off).
#[inline]
pub fn loss(c: Consumer) -> Option<u64> {
    match LOSSES[c as usize].load(Ordering::Relaxed) {
        u64::MAX => None,
        l => Some(l),
    }
}

pub fn set_loss(c: Consumer, ns_per_mib: Option<u64>) {
    LOSSES[c as usize].store(ns_per_mib.map_or(u64::MAX, |l| l.min(u64::MAX - 1)), Ordering::Relaxed);
}

/// Whether `bytes` more fit in B's free bytes as of the last snapshot (always under off, and before any snapshot). 5b
/// refreshes the free bytes at its 100 ms tick.
#[inline]
pub fn fits(bytes: u64) -> bool {
    if mode() == Mode::Off {
        return true;
    }
    let free = FREE_B.load(Ordering::Relaxed);
    let ok = bytes <= free;
    if !ok {
        FITS_REFUSED.fetch_add(1, Ordering::Relaxed);
    }
    ok
}

/// `(snapshots, fits refused, double registrations)`.
pub fn counters() -> (u64, u64, u64) {
    (SNAPSHOTS.load(Ordering::Relaxed), FITS_REFUSED.load(Ordering::Relaxed), DOUBLE_REGISTER.load(Ordering::Relaxed))
}

/// One consumer's row of a snapshot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Row {
    pub registered: bool,
    pub current: u64,
    pub floor: u64,
    pub cap: u64,
    pub target: Option<u64>,
}

/// D3808: an account for memory no consumer's hooks count, charged by the mechanism that holds it as it comes and goes:
/// a load's in-flight memory (a COPY's parsed batches and their contexts, before the rows reach a table). Lock-free: a
/// charge or a discharge is one relaxed atomic add, safe on a per-allocation path, and allocates nothing. The snapshot
/// counts every account's bytes as fixed memory (`compose_with`): they leave B's free bytes and the pool's capacity by
/// name, with or without the governor's resident sample.
#[derive(Debug)]
pub struct Account {
    pub name: &'static str,
    bytes: AtomicU64,
    peak: AtomicU64,
}

impl Account {
    pub const fn new(name: &'static str) -> Account {
        Account { name, bytes: AtomicU64::new(0), peak: AtomicU64::new(0) }
    }

    /// `n` bytes more held.
    #[inline]
    pub fn charge(&self, n: u64) {
        let now = self.bytes.fetch_add(n, Ordering::Relaxed).saturating_add(n);
        if now > self.peak.load(Ordering::Relaxed) {
            self.peak.fetch_max(now, Ordering::Relaxed);
        }
    }

    /// `n` bytes given back (never under 0: a discharge past the charges is a caller's bug, counted as 0 held).
    #[inline]
    pub fn discharge(&self, n: u64) {
        let _ = self.bytes.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |b| Some(b.saturating_sub(n)));
    }

    /// `n` bytes held until the returned guard drops (one load's batch).
    #[inline]
    pub fn hold(&self, n: u64) -> Held<'_> {
        self.charge(n);
        Held { account: self, bytes: n }
    }

    /// The bytes held now.
    #[inline]
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// The most held at once since the process started.
    pub fn peak(&self) -> u64 {
        self.peak.load(Ordering::Relaxed)
    }
}

/// Bytes charged to an account until it drops (`Account::hold`); `resize` follows a batch that grows or shrinks.
#[derive(Debug)]
pub struct Held<'a> {
    account: &'a Account,
    bytes: u64,
}

impl Held<'_> {
    /// The guard now holds `bytes` (the difference charged or given back).
    #[inline]
    pub fn resize(&mut self, bytes: u64) {
        if bytes > self.bytes {
            self.account.charge(bytes - self.bytes);
        } else {
            self.account.discharge(self.bytes - bytes);
        }
        self.bytes = bytes;
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.account.discharge(self.bytes);
    }
}

/// D3808: a load's in-flight memory (a COPY FROM's parsed batches and the contexts that hold them, parquet or text, until
/// the rows reach the table).
pub static LOAD_IN_FLIGHT: Account = Account::new("load_in_flight");

/// D3881: the executions of nested statements a backend thread keeps for its next nested calls (exec admit `kept`: each
/// freed statement level's admissions, the contexts they live in and their parked Nodes), until a call takes one, the
/// pool gives it back (`mempolicy::keep::nested_kept_keeps`), or an abort, a plan-cache reset or the session's end drops them.
pub static NESTED_KEPT: Account = Account::new("nested_kept");

/// Every account, for the snapshot and the views.
pub static ACCOUNTS: [&Account; 2] = [&LOAD_IN_FLIGHT, &NESTED_KEPT];

/// The bytes every account holds now.
pub fn accounts_total() -> u64 {
    ACCOUNTS.iter().fold(0u64, |a, c| a.saturating_add(c.bytes()))
}

// ---- G1-A: memory counted at the doors where it leaves the operating system ------------------------------------------
//
// Two doors: (1) the global allocator wrapper (`door_heap`, every alloc / free / realloc), (2) every direct mapping
// (`charge` / `discharge`: a commit, a give-back, an unmap). Each charges the calling thread's CURRENT account: its
// backend's slot while it serves a statement (workers adopt the leader's), a process account while a cache allocates under
// `enter`, UNBOUND (slot 0) on a thread that never took one. Door 1 adds to a thread-local pending count and touches
// the account only when the pending count reaches `DOOR_FOLD_BYTES` either way (`fold`), at an `enter` / `adopt` and at a
// statement's start and end: no lock, no shared atomic and no allocation per allocation. Door 2 and the region kinds on
// the thread's own account join the same pending counts (`post`); another account takes them at once. A reader sees
// every account within `DOOR_FOLD_BYTES` per thread and counter on it (`AccountRow::fold_slack`).

/// Door 1's fold step: a thread's pending count reaches the account at this many bytes either way.
pub const DOOR_FOLD_BYTES: i64 = 256 << 10;

pub const fn arc_bytes<T>() -> u64 {
    let w = core::mem::size_of::<usize>();
    let a = if core::mem::align_of::<T>() > w { core::mem::align_of::<T>() } else { w };
    ((2 * w).next_multiple_of(a) + core::mem::size_of::<T>()).next_multiple_of(a) as u64
}

/// The account slots: process accounts first, then one per backend (`BACKEND_SLOT_BASE` + its process number): 4080 backend
/// slots, as when the base was 16 of 4096.
pub const ACCOUNT_SLOTS: usize = 4112;
/// The first backend slot (32: room for the process accounts past 12 consumers, ROUNDING at 16 then).
pub const BACKEND_SLOT_BASE: u32 = 32;
// The process accounts (UNBOUND, the consumers, PARKED .. POOL_RESERVE) sit below the first backend slot.
const _: () = assert!(5 + CONSUMERS < BACKEND_SLOT_BASE as usize);

/// An account: an index into the slots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Acct(pub u32);

impl Acct {
    /// The memory of threads that never took an account (the postmaster's helpers: broker, reclaimer, watchdog, a pool
    /// thread before its identity). Not the views' 'unattributed' row: that one is resident memory no account counts.
    pub const UNBOUND: Acct = Acct(0);
    /// D3794's parked regions between statements (`transfer` at park and at take).
    pub const PARKED: Acct = Acct(1 + CONSUMERS as u32);
    /// Pending bytes: a dropped mapping's writable pages between its owner's drop (they leave its account there) and the
    /// reaper thread's unmap (`gravity_driver::mem::reap`), still resident until then.
    pub const REAPING: Acct = Acct(2 + CONSUMERS as u32);
    /// RESIDUE: the slot depot's parked worker-slot sets' kept source heap (`Holder::SlotDepot`),
    /// between statements; reclaimable like PARKED (`Class::Warm`: the depot's trims free it).
    pub const SLOT_DEPOT: Acct = Acct(3 + CONSUMERS as u32);
    /// D4556: the allocator's rounding of the live blocks (each block's usable size less its layout's: the size class's,
    /// ~3.4 % of live bytes), door 1's when the memory policy's `charge_usable=1` arms it ([`door_round`]). One process
    /// account, so a block allocated on one thread and freed on another nets here, and every owner's account stays at its
    /// layout bytes.
    pub const ROUNDING: Acct = Acct(4 + CONSUMERS as u32);
    /// The page pool's warm reserve (`PGRUST_POOL_RESERVE=1`): its free warm superpages, posted as they come and go
    /// ([`door_pool_reserve`]), so the ladder's read and the views see them on an account, not as the allocator's
    /// retained bytes. Warm: the memory ladder's background pass gives it back (protected floor 0).
    pub const POOL_RESERVE: Acct = Acct(5 + CONSUMERS as u32);

    /// The account of a consumer's own memory (a cache allocates under `enter(Acct::of(c))`).
    #[inline]
    pub const fn of(c: Consumer) -> Acct {
        Acct(1 + c as u32)
    }

    /// A backend's account, by its process number (past the slots: UNBOUND).
    #[inline]
    pub const fn backend(procno: u32) -> Acct {
        let i = BACKEND_SLOT_BASE as u64 + procno as u64;
        if i < ACCOUNT_SLOTS as u64 {
            Acct(i as u32)
        } else {
            Acct::UNBOUND
        }
    }

    #[inline]
    pub const fn is_backend(self) -> bool {
        self.0 >= BACKEND_SLOT_BASE
    }

    /// The account's reclaim class: a backend's `Running`, PARKED and SLOT_DEPOT `Warm`, REAPING `Pending`, UNBOUND (and a slot no one
    /// uses) `Unbound`, a consumer's account its consumer's class.
    #[inline]
    pub const fn class(self) -> Class {
        match self.0 {
            i if i >= BACKEND_SLOT_BASE => Class::Running,
            i if i == Acct::PARKED.0 || i == Acct::SLOT_DEPOT.0 || i == Acct::POOL_RESERVE.0 => Class::Warm,
            i if i == Acct::REAPING.0 => Class::Pending,
            i if i == Acct::ROUNDING.0 => Class::Fixed,
            i if i >= 1 && ((i - 1) as usize) < CONSUMERS => Consumer::ALL[(i - 1) as usize].class(),
            _ => Class::Unbound,
        }
    }

    /// The account's name in the views: 'backend', 'unbound', 'parked', 'reaping' or a consumer's name.
    pub fn name(self) -> &'static str {
        match self.0 {
            0 => "unbound",
            i if i == Acct::PARKED.0 => "parked",
            i if i == Acct::REAPING.0 => "reaping",
            i if i == Acct::SLOT_DEPOT.0 => "slot_depot",
            i if i == Acct::ROUNDING.0 => "allocator_rounding",
            i if i == Acct::POOL_RESERVE.0 => "pool_reserve",
            i if i >= BACKEND_SLOT_BASE => "backend",
            i if ((i - 1) as usize) < CONSUMERS => Consumer::ALL[(i - 1) as usize].name(),
            _ => "unused",
        }
    }
}

/// The split by what the bytes are for: a Gravity region by the breaker of the pipeline that made it (allocator-backed or
/// mapped; `Region` outside every breaker's pipeline), every other mapped byte `Mapped`. Each kind keeps both doors' bytes
/// and, beside them, door 2's alone (`KindRow::mapped_bytes`). The rest of door 1 is one heap number per statement: a kind
/// tag at the allocator would cost an `enter` at every operator switch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// A Gravity region made outside every breaker's pipeline (the thread's kind outside a `with_kind`).
    Region = 0,
    HashBuild = 1,
    HashAgg = 2,
    PlainAgg = 3,
    Distinct = 4,
    Sort = 5,
    TopK = 6,
    Spool = 7,
    Spill = 8,
    /// A backend's program arena (its graph arena, kept between statements to `KEEP_BYTES`): WARM memory.
    Arena = 9,
    /// Door 2's bytes that are no Gravity region (a context's mapped block, JIT code, an io_uring ring).
    Mapped = 10,
}

pub const KINDS: usize = 11;

/// A slot's kind counters: each kind's bytes of both doors (`Kind as usize`), then each kind's door-2 bytes alone
/// (`KINDS + Kind as usize`).
const LANES: usize = 2 * KINDS;

impl Kind {
    pub const ALL: [Kind; KINDS] = [
        Kind::Region,
        Kind::HashBuild,
        Kind::HashAgg,
        Kind::PlainAgg,
        Kind::Distinct,
        Kind::Sort,
        Kind::TopK,
        Kind::Spool,
        Kind::Spill,
        Kind::Arena,
        Kind::Mapped,
    ];

    /// A Gravity region's kind (a breaker's or the default), not the program arena's nor another mapping's.
    pub const fn is_region(self) -> bool {
        (self as u8) <= Kind::Spill as u8
    }

    pub const fn name(self) -> &'static str {
        match self {
            Kind::Region => "region",
            Kind::HashBuild => "hash_build",
            Kind::HashAgg => "hash_agg",
            Kind::PlainAgg => "plain_agg",
            Kind::Distinct => "distinct",
            Kind::Sort => "sort",
            Kind::TopK => "top_k",
            Kind::Spool => "spool",
            Kind::Spill => "spill",
            Kind::Arena => "arena",
            Kind::Mapped => "mapped",
        }
    }

    #[inline]
    const fn of(i: u8) -> Kind {
        if (i as usize) < KINDS {
            Kind::ALL[i as usize]
        } else {
            Kind::Region
        }
    }
}

/// One account's numbers, written only at folds and door-2 calls.
#[repr(align(64))]
struct Slot {
    /// Bytes in through both doors less bytes out.
    net: AtomicI64,
    /// Of `net`, door 2's (mapped) bytes.
    mapped: AtomicI64,
    /// Of `net`, the page pool's (`charge::CtxOwner::post_pool`): owner-exact, never floored.
    pooled: AtomicI64,
    /// `net` when the running statement started.
    base: AtomicI64,
    /// The running statement's most `net - base` seen at a fold.
    peak: AtomicI64,
    /// The statement number: odd while a statement runs.
    seq: AtomicU64,
    /// C8 (D4087): the running statement's process-wide id (`charge::OWNER_IDS`), 0 between statements and on a free slot.
    stmt: AtomicU64,
    /// C8: the token of the portal the backend executes now (`charge::enter_portal`), 0 = none.
    portal: AtomicU64,
    /// C8 checker 1: the backend's portals begun and not ended (`charge::portal_begin`).
    portals: AtomicU32,
    /// The operator-table sweeps its ends ran (`charge::end_ops`; information: `charge::op_sweeps`).
    sweeps: AtomicU64,
    /// Threads on the account now.
    threads: AtomicU32,
    /// The backend's process id (backend slots), 0 = none.
    pid: AtomicU32,
    /// `mapped` when the running statement started, and its most `mapped - mbase` / heap (`net - mapped` less its start)
    /// seen since.
    mbase: AtomicI64,
    mpeak: AtomicI64,
    hpeak: AtomicI64,
    /// Gravity regions' bytes by kind (door 1's allocator regions and door 2's mapped ones) and every other mapped byte
    /// as `Mapped`, then each kind's door-2 bytes alone (`LANES`); each at the statement's start, and its most since.
    kind: [AtomicI64; LANES],
    kbase: [AtomicI64; LANES],
    kpeak: [AtomicI64; LANES],
    /// What the grant pool holds for the account's statements (their reservations, their leaders' and workers' live draws
    /// past G), and its most since the running statement started: a promise of room beside the doors' committed bytes.
    reserved: AtomicI64,
    rpeak: AtomicI64,
    /// C8 move 3c: of `reserved`, the room its draws already back: bytes a statement drew inside its room (G, a leader's or
    /// a worker's draw past it) are pages on the doors, so they count once (`pool_back`). `reserved` stays the claim
    /// (what decides on the pool's promise reads it: `pool_reserved`); the views show `reserved - backed`.
    backed: AtomicI64,
    /// A kind's bytes moved since the statement numbers last restarted (a statement with no region skips the kinds' reset).
    kdirty: AtomicBool,
    /// D3872: of `net`, the bytes the residency pass last found not resident (a region's unwritten or given-back pages;
    /// `nr_mapped` of them door 2's): the doors count what was asked for, `net - nr` what the account has resident.
    nr: AtomicI64,
    nr_mapped: AtomicI64,
    /// PINS: the account's owners' pin counts summed (`charge::OwnerCell::pins`): the backend's, for R6 (it cancels a backend).
    pins: AtomicI64,
    pin_ev: AtomicI64,
    pin_sh: AtomicI64,
}

#[allow(clippy::declare_interior_mutable_const)]
const SLOT0: Slot = Slot {
    net: AtomicI64::new(0),
    mapped: AtomicI64::new(0),
    pooled: AtomicI64::new(0),
    base: AtomicI64::new(0),
    peak: AtomicI64::new(0),
    seq: AtomicU64::new(0),
    stmt: AtomicU64::new(0),
    portal: AtomicU64::new(0),
    portals: AtomicU32::new(0),
    sweeps: AtomicU64::new(0),
    threads: AtomicU32::new(0),
    pid: AtomicU32::new(0),
    mbase: AtomicI64::new(0),
    mpeak: AtomicI64::new(0),
    hpeak: AtomicI64::new(0),
    kind: [const { AtomicI64::new(0) }; LANES],
    kbase: [const { AtomicI64::new(0) }; LANES],
    kpeak: [const { AtomicI64::new(0) }; LANES],
    reserved: AtomicI64::new(0),
    rpeak: AtomicI64::new(0),
    backed: AtomicI64::new(0),
    kdirty: AtomicBool::new(false),
    nr: AtomicI64::new(0),
    nr_mapped: AtomicI64::new(0),
    pins: AtomicI64::new(0),
    pin_ev: AtomicI64::new(0),
    pin_sh: AtomicI64::new(0),
};

static SLOTS: [Slot; ACCOUNT_SLOTS] = [SLOT0; ACCOUNT_SLOTS];

/// C8 (D4087): the id of the statement running on account `a` now (0: none); never reused, so the next backend on a slot
/// never matches its predecessor's statements (`Owner::Statement`).
pub fn running_statement(a: Acct) -> u64 {
    SLOTS.get(a.0 as usize).map_or(0, |s| s.stmt.load(Ordering::Relaxed))
}

/// The ladder's R6 landing test, by attribution: account `a`'s running statement (`running_statement`'s id; 0: none) and
/// the bytes it holds now: both doors less the keeper blocks parked on it for the next context (`keepers`: no owner holds
/// them); its threads' pending door-1 bytes not folded (`AccountRow::fold_slack`). Three relaxed loads, nothing written:
/// the caller sets the bytes against the statement's start, read once (`AccountRow::statement_net`).
pub fn statement_hold(a: Acct) -> (u64, i64) {
    SLOTS.get(a.0 as usize).map_or((0, 0), |s| (s.stmt.load(Ordering::Relaxed), s.net.load(Ordering::Relaxed).wrapping_sub(keepers(a))))
}

/// The process id of backend account `a` (0: none, or not a backend).
pub fn backend_pid(a: Acct) -> u32 {
    SLOTS.get(a.0 as usize).filter(|_| a.is_backend()).map_or(0, |s| s.pid.load(Ordering::Relaxed))
}

/// Door 2's bytes of Gravity region mappings (`Kind::is_region`) on every account, backends' or not: their writable pages.
static REGION_MAPPED: AtomicI64 = AtomicI64::new(0);

/// Door 2's bytes of Gravity region mappings on every account now (a mapping dropped and waiting for the reaper is on
/// `REAPING` instead, as `Mapped`). The views set it against the region mappings' resident pages.
pub fn region_mapped_bytes() -> i64 {
    REGION_MAPPED.load(Ordering::Relaxed)
}

/// D3872: the residency pass's not-resident bytes on every account: door 1's (allocator regions) and door 2's (mappings).
static NR_HEAP: AtomicI64 = AtomicI64::new(0);
static NR_MAPPED: AtomicI64 = AtomicI64::new(0);

/// D3872: the residency pass found `delta` more bytes of account `a` not resident (door 2's when `mapped`; a region
/// moved, resized or dropped takes its share back the same way). The doors' counts stay what was asked for; the views,
/// the snapshot and the policy read `AccountRow::resident_bytes`. No allocation, no lock.
pub fn note_not_resident(a: Acct, delta: i64, mapped: bool) {
    if delta == 0 {
        return;
    }
    let s = &SLOTS[a.0 as usize];
    s.nr.fetch_add(delta, Ordering::Relaxed);
    if mapped {
        s.nr_mapped.fetch_add(delta, Ordering::Relaxed);
        NR_MAPPED.fetch_add(delta, Ordering::Relaxed);
    } else {
        NR_HEAP.fetch_add(delta, Ordering::Relaxed);
    }
}

/// D3872: the not-resident bytes on every account now, door 1's and door 2's (`note_not_resident`).
pub fn not_resident_bytes() -> (i64, i64) {
    (NR_HEAP.load(Ordering::Relaxed), NR_MAPPED.load(Ordering::Relaxed))
}

/// A thread's door state (const: no destructor, so the allocator may reach it at any point of a thread's life).
struct Door {
    pending: Cell<i64>,
    acct: Cell<u32>,
    /// The kind a door-2 owner made now takes (`with_kind`).
    kind: Cell<u8>,
    /// Of `pending`, door 2's bytes; each kind counter's bytes not yet on the account (`LANES`; `kmask`: the counters with
    /// some).
    mapped: Cell<i64>,
    kinds: [Cell<i64>; LANES],
    kmask: Cell<u32>,
    /// Of `pending`, the page pool's bytes.
    pooled: Cell<i64>,
    /// D4321 step 1: open `arena::cache_scope`s on this thread; above 0 the global allocator takes the block from the
    /// cache arena's heap.
    cache: Cell<u32>,
    /// D4556: this thread's rounding bytes not yet on `Acct::ROUNDING` ([`door_round`]).
    round: Cell<i64>,
    /// D4556: this thread's exit fold is armed ([`bind_backend`]); until it is, the thread's rounding goes to the account
    /// at once, so a thread that never binds loses none at its exit.
    exit_armed: Cell<bool>,
    /// D4679: the backend account this thread is bound to ([`bind_backend`] .. [`unbind_backend`]; UNBOUND otherwise), which a
    /// home it enters does not change ([`bound`]).
    bound: Cell<u32>,
    /// D4709 (`home_keep`): the other account this thread holds counts for (+1; 0: none), counted on its `threads` as the
    /// current one is, and those counts; `visit`: a home visit has the current and the other swapped.
    side: Cell<u32>,
    s_pending: Cell<i64>,
    s_mapped: Cell<i64>,
    /// Of `s_pending`, the page pool's bytes (they travel with the pending count they are part of).
    s_pooled: Cell<i64>,
    s_kinds: [Cell<i64>; LANES],
    s_kmask: Cell<u32>,
    visit: Cell<bool>,
}

#[allow(clippy::declare_interior_mutable_const)]
const NO_BYTES: Cell<i64> = Cell::new(0);

thread_local! {
    static DOOR: Door = const {
        Door { pending: Cell::new(0), acct: Cell::new(0), kind: Cell::new(0), mapped: Cell::new(0), kinds: [NO_BYTES; LANES], kmask: Cell::new(0), pooled: Cell::new(0), cache: Cell::new(0), round: Cell::new(0), exit_armed: Cell::new(false), bound: Cell::new(0), side: Cell::new(0), s_pending: Cell::new(0), s_mapped: Cell::new(0), s_pooled: Cell::new(0), s_kinds: [NO_BYTES; LANES], s_kmask: Cell::new(0), visit: Cell::new(false) }
    };
}

/// D3998: a thread's pending counts reach its current account when the thread exits. `DOOR` has no destructor, so before
/// this whatever a thread moved after its last fold was lost: a session's teardown frees its estate after the unbind has
/// moved the estate's bytes to UNBOUND, so UNBOUND (and door 1's sum) kept up to `DOOR_FOLD_BYTES` per ended session.
/// The fold is a pthread key's destructor, armed at each `bind_backend` (outside the allocator: setting a key's value may
/// take memory). glibc runs the keys' destructors after every thread-local's (`__call_tls_dtors`, then
/// `__nptl_deallocate_tsd`), so it folds the thread's last frees too, those of the thread-locals armed before the bind
/// among them. The destructor only folds: thread-local cells and relaxed atomics, no allocation and no lock.
#[cfg(unix)]
mod exit_fold {
    use core::sync::atomic::{AtomicU64, Ordering};

    /// The process's key plus one (0: none yet; `u64::MAX`: pthread_key_create failed, no exit fold).
    static KEY: AtomicU64 = AtomicU64::new(0);

    extern "C" fn at_exit(_: *mut libc::c_void) {
        // B2: a session this thread unbound and no teardown drained (a thread that ends without one) ends here
        super::charge::session_drained();
        super::fold();
        super::release_side();
        // the caches' held-back hits too (value::LOCAL: no destructor, so still readable here)
        super::value::flush();
    }

    /// Arm the calling thread's exit fold (the first call makes the process's key; a racing second key is deleted): true
    /// when the thread's destructor is armed.
    pub(super) fn arm() -> bool {
        let mut k = KEY.load(Ordering::Acquire);
        if k == 0 {
            let mut key: libc::pthread_key_t = 0;
            // SAFETY: `key` is a valid out pointer; `at_exit` stays a valid destructor for the process's life.
            let made = if unsafe { libc::pthread_key_create(&mut key, Some(at_exit)) } == 0 { key as u64 + 1 } else { u64::MAX };
            k = match KEY.compare_exchange(0, made, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => made,
                Err(won) => {
                    if made != u64::MAX {
                        // SAFETY: our own key, never set on any thread.
                        unsafe { libc::pthread_key_delete(key) };
                    }
                    won
                }
            };
        }
        // SAFETY: `k - 1` is a live key; any non-null value arms its destructor for this thread.
        k != u64::MAX && unsafe { libc::pthread_setspecific((k - 1) as libc::pthread_key_t, core::ptr::NonNull::<u8>::dangling().as_ptr().cast()) } == 0
    }
}

#[cfg(not(unix))]
mod exit_fold {
    pub(super) fn arm() -> bool {
        false
    }
}

/// Door 1, every global allocator call: `delta` bytes in (+) or out (-) on the calling thread's current account. One
/// thread-local add and one compare; the account itself is touched once per `DOOR_FOLD_BYTES` of movement.
#[inline(always)]
pub fn door_heap(delta: isize) {
    DOOR.with(|d| {
        let p = d.pending.get().wrapping_add(delta as i64);
        if p.unsigned_abs() < DOOR_FOLD_BYTES as u64 {
            d.pending.set(p);
        } else {
            fold_pending(d, p);
        }
    })
}

/// The page pool's warm reserve: `delta` bytes of whole free superpages in (+) or out (-), on [`Acct::POOL_RESERVE`] as
/// pooled bytes (the pool's door claimed them: they leave `pool_unowned`, and `pagepool_total` counts them for the
/// allocator's in-use reading, so the retained term drops by what the account gains). One atomic add per superpage.
pub fn door_pool_reserve(delta: i64) {
    add_q(Acct::POOL_RESERVE, delta, 0, delta);
}

/// D4556, door 1's rounding: `delta` bytes of the allocator's rounding of a block (its usable size less its layout's) in
/// (+) or out (-), on [`Acct::ROUNDING`] whichever thread and account: a thread-local add and one compare; the account is
/// touched once per `ROUND_FOLD_BYTES` of movement on a thread whose exit fold is armed, and at each call on any other
/// thread (one that never bound has no exit fold, so nothing may wait on it).
#[inline(always)]
pub fn door_round(delta: isize) {
    DOOR.with(|d| {
        let r = d.round.get().wrapping_add(delta as i64);
        if d.exit_armed.get() && r.unsigned_abs() < ROUND_FOLD_BYTES as u64 {
            d.round.set(r);
        } else {
            fold_round(d, r);
        }
    })
}

/// D4556: this thread's rounding bytes not yet folded onto [`Acct::ROUNDING`] (a test reads its own block's rounding).
pub fn round_pending() -> i64 {
    DOOR.with(|d| d.round.get())
}

/// D4556: a thread's rounding is folded at a quarter of door 1's step: the account is one process slot.
const ROUND_FOLD_BYTES: i64 = DOOR_FOLD_BYTES / 4;

#[cold]
#[inline(never)]
fn fold_round(d: &Door, r: i64) {
    d.round.set(0);
    add(Acct::ROUNDING, r, 0);
}

#[cold]
#[inline(never)]
fn fold_pending(d: &Door, p: i64) {
    d.pending.set(p);
    fold_door(d);
}

/// D4321 step 1: this thread's open cache scopes (`arena::cache_scope`); 0 when the thread-local is gone.
#[inline(always)]
pub(crate) fn door_cache_depth() -> u32 {
    DOOR.try_with(|d| d.cache.get()).unwrap_or(0)
}

/// D4321 step 1: `delta` (+1 / -1) on this thread's open cache scopes.
pub(crate) fn door_cache_step(delta: i32) {
    let _ = DOOR.try_with(|d| d.cache.set(d.cache.get().wrapping_add_signed(delta)));
}

/// D4552 (a) backstop, the memory policy's `ledger_floor` knob (default 0, set at the server's start; it was 1, a stopgap,
/// until the floor-off legs found no fold below zero; PGRUST_MEMPOLICY=ledger_floor=1 turns it on; off until the start, so
/// tests have it off): when on, a door-1 fold never takes a
/// backend account below zero while the folding thread is the account's only one. The bytes freed past zero are bytes
/// another account was charged for (a pooled worker freeing what its engagement's leader was charged for; a bound session
/// freeing what its thread allocated on UNBOUND before the bind): they credit UNBOUND, where the charging account's
/// unbind moved them or will. A many-thread account keeps the whole free: its threads' pending counts fold in any order,
/// so a short dip below zero there is no cross-account free.
static FLOOR: AtomicU8 = AtomicU8::new(FLOOR_OFF);
const FLOOR_OFF: u8 = 1;
const FLOOR_ON: u8 = 2;
/// The floor's folds that credited UNBOUND, and their bytes.
static FLOOR_EVENTS: AtomicU64 = AtomicU64::new(0);
static FLOOR_BYTES: AtomicI64 = AtomicI64::new(0);

/// Switch the floor (the server's start, from the memory policy's `ledger_floor` knob; tests).
pub fn set_floor(on: bool) {
    FLOOR.store(if on { FLOOR_ON } else { FLOOR_OFF }, Ordering::Relaxed);
}

/// D4552 (a), the memory policy's `ledger_unbind_wait` knob (default 1; `set_unbind_wait` at the server's start): a session's unbind
/// waits for the threads still inside its account (a pool worker in its home: a resource group's completion or last
/// drop) to leave before it moves the account's bytes to UNBOUND, so their frees fold onto the session's row while it
/// still holds them, never onto the zeroed slot (the floor's case). Bounded: past `UNBIND_WAIT_CAP_NS` it moves them as
/// before and counts the timeout.
static UNBIND_WAIT: AtomicBool = AtomicBool::new(true);
/// Unbinds that found a thread inside, the longest wait (ns), and the waits that hit the cap.
static UNBIND_WAITED: AtomicU64 = AtomicU64::new(0);
static UNBIND_WAIT_MAX_NS: AtomicU64 = AtomicU64::new(0);
static UNBIND_WAIT_CAPPED: AtomicU64 = AtomicU64::new(0);
/// The longest an unbind waits for its account's other threads.
pub const UNBIND_WAIT_CAP_NS: u64 = 200_000_000;

/// Switch the unbind's wait (the server's start, from the memory policy's `ledger_unbind_wait` knob; tests).
pub fn set_unbind_wait(on: bool) {
    UNBIND_WAIT.store(on, Ordering::Relaxed);
}

/// Unbinds that waited for a thread inside their account, the longest wait (ns), and the waits that hit the cap.
pub fn unbind_waits() -> (u64, u64, u64) {
    (UNBIND_WAITED.load(Ordering::Relaxed), UNBIND_WAIT_MAX_NS.load(Ordering::Relaxed), UNBIND_WAIT_CAPPED.load(Ordering::Relaxed))
}

/// The unbind's wait: until no thread is inside slot `s` (this one left it first), at most `UNBIND_WAIT_CAP_NS`.
fn wait_entered(s: &Slot) {
    // with the entrant's re-check (`Home::enter`): this thread's pid store, then its read of the count; the entrant's
    // count, then its read of the pid: one of the two sees the other's write
    core::sync::atomic::fence(Ordering::SeqCst);
    // Acquire, with the leaving thread's Release (`switch`): its last fold is on `net` before the unbind moves it
    if s.threads.load(Ordering::Acquire) == 0 {
        return;
    }
    let t0 = pg_clock::mono_ns();
    let waited = || pg_clock::mono_ns().saturating_sub(t0);
    let mut spins = 0u32;
    // bounded("other: one account's entered threads leaving; at most UNBIND_WAIT_CAP_NS, then the unbind goes on")
    let capped = loop {
        if s.threads.load(Ordering::Acquire) == 0 {
            break false;
        }
        if waited() >= UNBIND_WAIT_CAP_NS {
            break true;
        }
        spins += 1;
        if spins < 64 {
            core::hint::spin_loop();
        } else if spins < 128 {
            pgsync::thread::yield_now();
        } else {
            pgsync::thread::sleep(core::time::Duration::from_micros(50));
        }
    };
    UNBIND_WAITED.fetch_add(1, Ordering::Relaxed);
    UNBIND_WAIT_MAX_NS.fetch_max(waited(), Ordering::Relaxed);
    if capped {
        UNBIND_WAIT_CAPPED.fetch_add(1, Ordering::Relaxed);
    }
}

/// The floor's folds that credited UNBOUND and their bytes, since the start.
pub fn floored() -> (u64, i64) {
    (FLOOR_EVENTS.load(Ordering::Relaxed), FLOOR_BYTES.load(Ordering::Relaxed))
}

/// Of a fold of `p` bytes (`m` of them door 2's, `q` the page pool's) onto backend account `a`, what `a` takes under the
/// floor: all of it unless it would end below zero with this thread its only one; then door 1's bytes past zero credit
/// UNBOUND.
#[cold]
#[inline(never)]
fn floor_at_zero(a: Acct, p: i64, m: i64, q: i64) -> i64 {
    let s = &SLOTS[a.0 as usize];
    if s.threads.load(Ordering::Relaxed) > 1 {
        return p;
    }
    let after = s.net.load(Ordering::Relaxed).wrapping_add(p);
    if after >= 0 {
        return p;
    }
    // never more than the fold's door-1 free (door 2's and the pool's bytes are their owners')
    let rest = after.max(p.wrapping_sub(m).wrapping_sub(q));
    if rest >= 0 {
        return p;
    }
    add(Acct::UNBOUND, rest, 0);
    FLOOR_EVENTS.fetch_add(1, Ordering::Relaxed);
    FLOOR_BYTES.fetch_sub(rest, Ordering::Relaxed);
    p.wrapping_sub(rest)
}

/// Every pending count of the thread onto its current account.
fn fold_door(d: &Door) {
    let a = Acct(d.acct.get());
    let (p, m, q) = (d.pending.replace(0), d.mapped.replace(0), d.pooled.replace(0));
    // D4552 (a) backstop: a free past what the account holds is no byte of its own
    let p = if p.wrapping_sub(q) < m && a.is_backend() && FLOOR.load(Ordering::Relaxed) == FLOOR_ON { floor_at_zero(a, p, m, q) } else { p };
    add_q(a, p, m, q);
    add(Acct::ROUNDING, d.round.replace(0), 0);
    let mut mask = d.kmask.replace(0);
    while mask != 0 {
        let i = mask.trailing_zeros() as usize;
        mask &= mask - 1;
        add_lane(a, i, d.kinds[i].replace(0));
    }
}

/// `p` bytes (`m` of them door 2's) and `kb` bytes of kind `k` onto `a` (with `kb`, the `m` door-2 bytes are of kind `k`
/// too): the calling thread's own account takes them into its pending counts (on the account at the next fold, as door
/// 1's are), any other account at once.
#[inline]
fn post(a: Acct, p: i64, m: i64, k: Kind, kb: i64) {
    post_q(a, p, m, 0, k, kb)
}

/// `post` with `q` of the `p` bytes the page pool's.
#[inline]
fn post_q(a: Acct, p: i64, m: i64, q: i64, k: Kind, kb: i64) {
    if m != 0 && k.is_region() {
        REGION_MAPPED.fetch_add(m, Ordering::Relaxed);
    }
    let own = DOOR.with(|d| {
        if d.acct.get() != a.0 {
            return false;
        }
        let (np, nm, nq) = (d.pending.get().wrapping_add(p), d.mapped.get().wrapping_add(m), d.pooled.get().wrapping_add(q));
        d.pending.set(np);
        d.mapped.set(nm);
        d.pooled.set(nq);
        let mut over = np.unsigned_abs() >= DOOR_FOLD_BYTES as u64 || nm.unsigned_abs() >= DOOR_FOLD_BYTES as u64 || nq.unsigned_abs() >= DOOR_FOLD_BYTES as u64;
        if kb != 0 && a.is_backend() {
            over |= pend_lane(d, k as usize, kb);
            if m != 0 {
                over |= pend_lane(d, KINDS + k as usize, m);
            }
        }
        if over {
            fold_door(d);
        }
        true
    });
    if !own {
        add_q(a, p, m, q);
        add_kind(a, k, kb, m);
    }
}

/// `b` more on the thread's pending kind counter `i`; whether it reached the fold step.
#[inline]
fn pend_lane(d: &Door, i: usize, b: i64) -> bool {
    let v = d.kinds[i].get().wrapping_add(b);
    d.kinds[i].set(v);
    d.kmask.set(d.kmask.get() | 1 << i);
    v.unsigned_abs() >= DOOR_FOLD_BYTES as u64
}

/// `p` bytes (`mapped` of them through door 2) onto `a`, its statement peaks kept (whole, mapped, heap). No allocation, no
/// lock.
#[inline]
fn add(a: Acct, p: i64, mapped: i64) {
    add_q(a, p, mapped, 0)
}

/// `add` with `pooled` of the `p` bytes the page pool's.
#[inline]
fn add_q(a: Acct, p: i64, mapped: i64, pooled: i64) {
    if p == 0 && mapped == 0 && pooled == 0 {
        return;
    }
    let s = &SLOTS[a.0 as usize];
    if pooled != 0 {
        s.pooled.fetch_add(pooled, Ordering::Relaxed);
    }
    let net = s.net.fetch_add(p, Ordering::Relaxed).wrapping_add(p);
    // only a backend's statement reads the peaks, and only a rise moves them (a park, a free, a give-back skip them)
    if !a.is_backend() || (p <= 0 && mapped <= 0) {
        if mapped != 0 {
            s.mapped.fetch_add(mapped, Ordering::Relaxed);
        }
        return;
    }
    let m = if mapped != 0 { s.mapped.fetch_add(mapped, Ordering::Relaxed).wrapping_add(mapped) } else { s.mapped.load(Ordering::Relaxed) };
    let (base, mbase) = (s.base.load(Ordering::Relaxed), s.mbase.load(Ordering::Relaxed));
    peak(&s.peak, net.wrapping_sub(base));
    // `unattr`: the in-server unattributed check at the statement's rises (off: one relaxed load)
    if unattr::on() {
        unattr::rise(a, net.wrapping_sub(base), p.wrapping_sub(mapped).wrapping_sub(pooled));
    }
    if mapped != 0 {
        peak(&s.mpeak, m.wrapping_sub(mbase));
    }
    if p != mapped {
        peak(&s.hpeak, net.wrapping_sub(m).wrapping_sub(base.wrapping_sub(mbase)));
    }
}

#[inline]
fn peak(at: &AtomicI64, cur: i64) {
    if cur > at.load(Ordering::Relaxed) {
        at.fetch_max(cur, Ordering::Relaxed);
    }
}

/// Of `a`'s bytes, `b` more of kind `k`, `m` of them door 2's (only a backend's statement reads kinds: other accounts keep
/// none).
#[inline]
fn add_kind(a: Acct, k: Kind, b: i64, m: i64) {
    if b == 0 {
        return;
    }
    add_lane(a, k as usize, b);
    add_lane(a, KINDS + k as usize, m);
}

/// `b` more on `a`'s kind counter `i` (a backend's only), its peak kept.
#[inline]
fn add_lane(a: Acct, i: usize, b: i64) {
    if b == 0 || !a.is_backend() {
        return;
    }
    let s = &SLOTS[a.0 as usize];
    let now = s.kind[i].fetch_add(b, Ordering::Relaxed).wrapping_add(b);
    if b > 0 {
        peak(&s.kpeak[i], now.wrapping_sub(s.kbase[i].load(Ordering::Relaxed)));
    }
    if !s.kdirty.load(Ordering::Relaxed) {
        s.kdirty.store(true, Ordering::Relaxed);
    }
}

/// The calling thread's pending counts onto its current account now.
#[inline]
pub fn fold() {
    DOOR.with(|d| {
        if d.pending.get() != 0 || d.mapped.get() != 0 || d.pooled.get() != 0 || d.kmask.get() != 0 || d.round.get() != 0 {
            fold_door(d);
        }
    })
}

/// Door 2: `bytes` of a direct mapping committed (made writable, mapped) on the current account.
#[cfg(test)]
#[inline]
pub(crate) fn charge(bytes: u64) {
    charge_to(current(), bytes);
}

/// Door 2: `bytes` of a direct mapping given back (made inaccessible, advised away, unmapped) from the current account.
#[cfg(test)]
#[inline]
pub(crate) fn discharge(bytes: u64) {
    discharge_from(current(), bytes);
}

/// Door 2 onto a named account: memory whose owner keeps its account (a mapping charged where it was made, whichever
/// thread later grows or drops it), of kind `Mapped`.
#[cfg(test)]
#[inline]
pub(crate) fn charge_to(a: Acct, bytes: u64) {
    charge_kind(a, Kind::Mapped, bytes);
}

/// Door 2 from a named account (`charge_to`'s pair).
#[cfg(test)]
#[inline]
pub(crate) fn discharge_from(a: Acct, bytes: u64) {
    discharge_kind(a, Kind::Mapped, bytes);
}

/// `charge_to` for bytes of kind `k`.
#[cfg(test)]
#[inline]
pub(crate) fn charge_kind(a: Acct, k: Kind, bytes: u64) {
    let b = bytes.min(i64::MAX as u64) as i64;
    post(a, b, b, k, b);
}

/// `discharge_from` for bytes of kind `k`.
#[cfg(test)]
#[inline]
pub(crate) fn discharge_kind(a: Acct, k: Kind, bytes: u64) {
    let b = bytes.min(i64::MAX as u64) as i64;
    post(a, -b, -b, k, -b);
}

/// What the grant pool holds for account `a`'s statements moved by `delta` bytes (a reservation made or dropped, a leader's
/// or a worker's draw past G or its give-back): a promise of room, not memory; the views show it beside the doors' bytes
/// (`pool_reserved_bytes`).
#[inline]
pub(crate) fn pool_charge(a: Acct, delta: i64) {
    let s = &SLOTS[a.0 as usize];
    peak(&s.rpeak, s.reserved.fetch_add(delta, Ordering::Relaxed).wrapping_add(delta));
}

/// C8 move 3c: of what the grant pool holds for account `a`'s statements, `delta` more bytes backed by pages its draws
/// committed (a draw inside the room), or (negative) unbacked again (the draw given back, settled or moved): the claim
/// (`pool_reserved`) is unchanged, the views' unbacked room (`AccountRow::pool_reserved_bytes`) moves by `-delta`.
#[inline]
pub fn pool_back(a: Acct, delta: i64) {
    SLOTS[a.0 as usize].backed.fetch_add(delta, Ordering::Relaxed);
}

/// What the grant pool holds for account `a`'s statements now (`pool_charge`'s sum: the claim, backed or not), one load:
/// an idle session's suspended portals hold it.
#[inline]
pub fn pool_reserved(a: Acct) -> i64 {
    SLOTS[a.0 as usize].reserved.load(Ordering::Relaxed)
}

/// The calling thread's account's statement peak so far (as `account_row`'s `statement_peak_bytes`, without the fold): 0
/// outside a statement.
#[inline]
pub fn statement_peak_here() -> u64 {
    let s = &SLOTS[current().0 as usize];
    if s.seq.load(Ordering::Relaxed) & 1 == 0 {
        return 0;
    }
    let cur = s.net.load(Ordering::Relaxed).saturating_sub(s.base.load(Ordering::Relaxed)).max(0) as u64;
    (s.peak.load(Ordering::Relaxed).max(0) as u64).max(cur)
}

/// The kind the regions the calling thread makes take now (`Region` outside a `with_kind`).
#[inline]
pub fn kind() -> Kind {
    DOOR.with(|d| Kind::of(d.kind.get()))
}

/// The regions the calling thread makes until the guard drops are of kind `k` (a breaker's region bind, a call of its
/// pipeline).
#[must_use]
pub fn with_kind(k: Kind) -> KindScope {
    KindScope { back: DOOR.with(|d| d.kind.replace(k as u8)) }
}

/// The kind before a `with_kind`, restored when it drops.
#[derive(Debug)]
pub struct KindScope {
    back: u8,
}

impl Drop for KindScope {
    fn drop(&mut self) {
        DOOR.with(|d| d.kind.set(self.back));
    }
}

/// D3872: the pages inside live allocator blocks given back to the kernel (MADV_DONTNEED) and not in use again, on every
/// account (`Charge::give` / `commit` of a door-1 charge, C8 area move 3a): the allocator still counts them in live
/// blocks, the accounts do not.
static GIVEN: AtomicI64 = AtomicI64::new(0);

/// D3872: `GIVEN` now.
pub fn given_back_bytes() -> i64 {
    GIVEN.load(Ordering::Relaxed)
}

/// `bytes` moved from one account to another (a region parked, or taken by another statement): door-2 bytes when
/// `mapped`, else bytes door 1 counted (an allocator's region).
#[cfg(test)]
pub(crate) fn transfer(from: Acct, to: Acct, bytes: u64, mapped: bool) {
    if from == to {
        return;
    }
    let b = bytes.min(i64::MAX as u64) as i64;
    if mapped {
        transfer_kind(from, to, Kind::Mapped, bytes, true);
    } else {
        post(from, -b, 0, Kind::Region, 0);
        post(to, b, 0, Kind::Region, 0);
    }
}

/// `transfer` of a Gravity region's bytes of kind `k`: the kind's bytes move with them, door 1's or door 2's.
#[cfg(test)]
pub(crate) fn transfer_kind(from: Acct, to: Acct, k: Kind, bytes: u64, mapped: bool) {
    if from == to {
        return;
    }
    let b = bytes.min(i64::MAX as u64) as i64;
    let m = if mapped { b } else { 0 };
    post(from, -b, -m, k, -b);
    post(to, b, m, k, b);
}

/// An allocator-backed Gravity region's bytes of kind `k` on account `a` moved by `delta` (door 1 counts the bytes
/// themselves; this keeps the per-kind split of the regions whole).
#[cfg(test)]
#[inline]
pub(crate) fn count_kind(a: Acct, k: Kind, delta: i64) {
    post(a, 0, 0, k, delta);
}

/// The calling thread's current account.
#[inline]
pub fn current() -> Acct {
    DOOR.with(|d| Acct(d.acct.get()))
}

#[allow(clippy::declare_interior_mutable_const)]
const NO_KEEPERS: AtomicI64 = AtomicI64::new(0);

/// The keeper blocks mcx parks for the next context (its context free lists, aset.c's context_freelists), by the account
/// each parked on: door 1 counted the block on that account and no owner holds it, so the owners view and the unattributed
/// check take these bytes off the account's unattributed ones (`keepers`).
static KEEPERS: [AtomicI64; ACCOUNT_SLOTS] = [NO_KEEPERS; ACCOUNT_SLOTS];

/// mcx parked (`delta` above 0) or let go of (below 0) keeper bytes that parked on account `a`.
#[inline]
pub fn keepers_moved(a: Acct, delta: i64) {
    KEEPERS[(a.0 as usize).min(ACCOUNT_SLOTS - 1)].fetch_add(delta, Ordering::Relaxed);
}

/// Account `a`'s parked keeper bytes now.
pub fn keepers(a: Acct) -> i64 {
    KEEPERS[(a.0 as usize).min(ACCOUNT_SLOTS - 1)].load(Ordering::Relaxed).max(0)
}

/// Every account's parked keeper bytes now.
pub fn keepers_total() -> i64 {
    KEEPERS.iter().map(|k| k.load(Ordering::Relaxed).max(0)).fold(0i64, i64::wrapping_add)
}

/// D4679: of the keepers, the context growth blocks mcx's growth shelf parks (a dropped context's blocks after its first),
/// bytes and blocks by the account each parked on. Counted in the keepers too (`growth_moved`): the same holder class.
static GROWTH: [AtomicI64; ACCOUNT_SLOTS] = [NO_KEEPERS; ACCOUNT_SLOTS];
static GROWTH_BLOCKS: [AtomicI64; ACCOUNT_SLOTS] = [NO_KEEPERS; ACCOUNT_SLOTS];

/// mcx's growth shelf parked (`bytes` above 0) or let go of (below 0) `blocks` blocks that parked on account `a`: moves the
/// account's keepers by the same bytes.
#[inline]
pub fn growth_moved(a: Acct, bytes: i64, blocks: i64) {
    let i = (a.0 as usize).min(ACCOUNT_SLOTS - 1);
    KEEPERS[i].fetch_add(bytes, Ordering::Relaxed);
    GROWTH[i].fetch_add(bytes, Ordering::Relaxed);
    GROWTH_BLOCKS[i].fetch_add(blocks, Ordering::Relaxed);
}

/// Account `a`'s parked growth (bytes, blocks) now, of its keepers.
pub fn growth(a: Acct) -> (i64, i64) {
    let i = (a.0 as usize).min(ACCOUNT_SLOTS - 1);
    (GROWTH[i].load(Ordering::Relaxed).max(0), GROWTH_BLOCKS[i].load(Ordering::Relaxed).max(0))
}

/// Every account's parked growth (bytes, blocks) now.
pub fn growth_total() -> (i64, i64) {
    let sum = |t: &[AtomicI64; ACCOUNT_SLOTS]| t.iter().map(|k| k.load(Ordering::Relaxed).max(0)).fold(0i64, i64::wrapping_add);
    (sum(&GROWTH), sum(&GROWTH_BLOCKS))
}

/// Switch the calling thread to `a` (its pending bytes folded onto the account it leaves); returns the account it left.
fn switch(a: Acct) -> Acct {
    DOOR.with(|d| {
        fold_door(d);
        let old = Acct(d.acct.replace(a.0));
        // UNBOUND counts no threads: every thread starts there uncounted
        if old != a {
            if old != Acct::UNBOUND {
                // Release: the fold above lands before the count drops (an unbind waiting for it reads `net` after)
                SLOTS[old.0 as usize].threads.fetch_sub(1, Ordering::Release);
            }
            if a != Acct::UNBOUND {
                SLOTS[a.0 as usize].threads.fetch_add(1, Ordering::Relaxed);
            }
        }
        old
    })
}

/// The calling thread charges `a` until the guard drops (a cache's insert; a worker serving a leader's statement), then
/// the account it had.
#[cfg(test)]
#[must_use]
pub(crate) fn enter(a: Acct) -> Entered {
    Entered { back: switch(a), paused: 0, visit: false }
}

/// The account before an `enter`, restored when it drops.
#[derive(Debug)]
pub struct Entered {
    pub(crate) back: Acct,
    /// D4552 (a): a home's (`Home::enter`): the cache scopes this thread had open, paused while the home is entered (a
    /// home's blocks are its own, on its account: an enclosing scope's holder counting them too would take them off the
    /// scope opener's account a second time), open again as it leaves.
    pub(crate) paused: u32,
    /// D4709: a home visit (`visit_enter`).
    pub(crate) visit: bool,
}

impl Drop for Entered {
    fn drop(&mut self) {
        if self.visit {
            visit_leave(self.back);
        } else {
            switch(self.back);
            release_side();
        }
        if self.paused != 0 {
            door_cache_step(self.paused as i32);
        }
        // a worker's served statement ends here: its held-back cache hits in (value::FOLD)
        if !self.visit {
            value::flush();
        }
    }
}

/// D4709: the memory policy's `home_keep` knob (default 0). On, a home visit swaps the thread's counts for the account it
/// leaves with those it keeps for the home, both accounts counting the thread, so its enter and leave move no shared line;
/// the kept counts fold at the fold step, the next plain leave, the unbind and the thread's exit.
static HOME_KEEP: AtomicBool = AtomicBool::new(false);

pub fn set_home_keep(on: bool) {
    HOME_KEEP.store(on, Ordering::Relaxed);
}

/// D4781: the memory policy's `fold_on_park` knob (default 0). On, a thread about to park (`park_fold`: a wait that blocks
/// in the kernel, a latch wait, a pool worker's idle park, a pooled bgworker standby's wait for a task, the memory watchdog's park) folds its pending counts first, so an account read while its
/// threads wait is exact, not within `DOOR_FOLD_BYTES` per thread (UNBOUND counts no threads: its `fold_slack` reads 0
/// though its threads' pending counts lag it as any account's do).
static FOLD_ON_PARK: AtomicBool = AtomicBool::new(false);

pub fn set_fold_on_park(on: bool) {
    FOLD_ON_PARK.store(on, Ordering::Relaxed);
}

/// D4781: the calling thread is about to park. With `fold_on_park` on, its pending counts onto its current account and its
/// kept counts (a home visit's, `home_keep`) onto the home's, the thread still counted there; with nothing pending, a few
/// thread-local loads. Off, one relaxed load. No allocation, no lock.
#[inline]
pub fn park_fold() {
    if !FOLD_ON_PARK.load(Ordering::Relaxed) {
        return;
    }
    DOOR.with(|d| {
        // pagepool P2: its pool bytes too (`pooled` / `s_pooled`), as `fold` reads them
        if d.pending.get() != 0 || d.mapped.get() != 0 || d.pooled.get() != 0 || d.kmask.get() != 0 || d.round.get() != 0 {
            fold_door(d);
        }
        // the swap is its own inverse: the kept counts fold onto their account and the thread's stay as they are
        if d.side.get() != 0 && (d.s_pending.get() != 0 || d.s_mapped.get() != 0 || d.s_pooled.get() != 0 || d.s_kmask.get() != 0) {
            swap_side(d);
            fold_door(d);
            swap_side(d);
        }
    })
}

/// The calling thread's exit fold armed (outside the allocator: setting a key's value may take memory): true when it is.
pub fn arm_thread_exit_fold() -> bool {
    if DOOR.with(|d| d.exit_armed.get()) {
        return true;
    }
    let armed = exit_fold::arm();
    DOOR.with(|d| d.exit_armed.set(d.exit_armed.get() || armed));
    armed
}

fn swap_side(d: &Door) {
    let a = d.acct.get();
    d.acct.set(d.side.get() - 1);
    d.side.set(a + 1);
    d.pending.swap(&d.s_pending);
    d.mapped.swap(&d.s_mapped);
    d.pooled.swap(&d.s_pooled);
    if d.kmask.get() | d.s_kmask.get() != 0 {
        for (k, s) in d.kinds.iter().zip(d.s_kinds.iter()) {
            k.swap(s);
        }
    }
    d.kmask.swap(&d.s_kmask);
}

/// A visit into home `h` (no backend): the account left, or `None` for the plain switch (a visit already running, a
/// third account's kept counts, no exit fold).
fn visit_enter(h: Acct) -> Option<Acct> {
    DOOR.with(|d| {
        let a = Acct(d.acct.get());
        let side = d.side.get();
        if d.visit.get() || a == h || (side != 0 && side != h.0 + 1) || !d.exit_armed.get() {
            return None;
        }
        if side == 0 {
            d.side.set(h.0 + 1);
            if h != Acct::UNBOUND {
                SLOTS[h.0 as usize].threads.fetch_add(1, Ordering::Relaxed);
            }
        }
        swap_side(d);
        d.visit.set(true);
        Some(a)
    })
}

fn visit_leave(back: Acct) {
    let swapped = DOOR.with(|d| {
        d.visit.set(false);
        let ok = d.side.get() == back.0 + 1;
        if ok {
            swap_side(d);
        }
        ok
    });
    if !swapped {
        release_side();
        switch(back);
    }
}

/// D4709: the kept counts onto their account and the thread off its count (a plain leave, the unbind, the exit).
pub(crate) fn release_side() {
    DOOR.with(|d| {
        if d.side.get() == 0 || d.visit.get() {
            return;
        }
        swap_side(d);
        fold_door(d);
        let h = Acct(d.acct.get());
        swap_side(d);
        d.side.set(0);
        if h != Acct::UNBOUND {
            // Release: the fold above lands before the count drops, as `switch`'s
            SLOTS[h.0 as usize].threads.fetch_sub(1, Ordering::Release);
        }
    })
}

/// D4552 (a): the switch of `Home`, the memory policy's `ledger_home` knob (default 1, set at the server's start;
/// PGRUST_MEMPOLICY=ledger_home=0 turns it off). Off until then, so a process that never starts a server (tests) has it off.
static HOME: AtomicU8 = AtomicU8::new(FLOOR_OFF);

/// Switch the homes (the server's start, from the memory policy's `ledger_home` knob; tests).
pub fn set_home(on: bool) {
    HOME.store(if on { FLOOR_ON } else { FLOOR_OFF }, Ordering::Relaxed);
}

/// D4552 (a): where a shared object's bytes come home when it is freed: the account of the thread that made it. Door 1
/// takes a free off the FREEING thread's account, so an object a leader allocates and a pooled worker drops last (a
/// query's resource group and its task sets, the leader's session carry inside them, held in a worker's cache past the
/// statement) left the worker's own row below zero and the leader's bytes on UNBOUND at its unbind. Made with the object
/// (`here`), entered around its drop (`HomeExit`): the backend account while the same session (pid) holds its slot,
/// UNBOUND once it ended (its unbind moved the bytes there), any other account as it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Home {
    acct: Acct,
    pid: u32,
}

impl Home {
    /// The calling thread's current account, with its session's pid.
    pub fn here() -> Home {
        let acct = current();
        Home { acct, pid: backend_pid(acct) }
    }

    /// A holder's home (a cache's account, which no session ends): what the holder keeps is allocated and freed there,
    /// whichever thread changes it (`enter`).
    pub fn of(acct: Acct) -> Home {
        Home { acct, pid: backend_pid(acct) }
    }

    /// The account the home's bytes come back to now.
    pub fn acct_now(&self) -> Acct {
        if self.acct.is_backend() && backend_pid(self.acct) != self.pid {
            Acct::UNBOUND
        } else {
            self.acct
        }
    }

    /// The calling thread charges the home until the guard drops (None with the switch off).
    #[must_use]
    pub fn enter(&self) -> Option<Entered> {
        (HOME.load(Ordering::Relaxed) == FLOOR_ON).then(|| {
            if HOME_KEEP.load(Ordering::Relaxed) && !self.acct.is_backend() && arm_thread_exit_fold() {
                if let Some(back) = visit_enter(self.acct) {
                    let paused = door_cache_depth();
                    if paused != 0 {
                        door_cache_step(-(paused as i32));
                    }
                    return Entered { back, paused, visit: true };
                }
            }
            let back = switch(self.acct_now());
            // with the unbind's wait (`wait_entered`): counted in first, then the pid read again; a session that ended
            // meanwhile moved its bytes to UNBOUND, where this home's frees go too
            if self.acct.is_backend() && UNBIND_WAIT.load(Ordering::Relaxed) {
                core::sync::atomic::fence(Ordering::SeqCst);
                if backend_pid(self.acct) != self.pid && current() == self.acct {
                    let was = switch(Acct::UNBOUND);
                    debug_assert_eq!(was, self.acct);
                }
            }
            let paused = door_cache_depth();
            if paused != 0 {
                door_cache_step(-(paused as i32));
            }
            Entered { back, paused, visit: false }
        })
    }
}

/// D4552 (a): the LAST field of a struct that frees inside its `Home`: the struct's `Drop` arms it (`arm`) before its
/// fields drop, in declaration order, and this one, dropped last, leaves the home. A struct that lives in its own heap
/// block (`for_block`: an `Arc`'s, `arc_bytes`, freed right after its fields drop, on the same thread) takes that block's bytes out of
/// the home with it, onto the thread whose free of the block lands next, as a heap `Charge`'s drop does.
#[derive(Debug, Default)]
pub struct HomeExit {
    entered: Option<Entered>,
    block: u64,
}

impl HomeExit {
    pub const fn new() -> HomeExit {
        HomeExit { entered: None, block: 0 }
    }

    /// `new` for a struct whose own heap block of `bytes` (`arc_bytes`) is freed right after its fields drop.
    pub const fn for_block(bytes: u64) -> HomeExit {
        HomeExit { entered: None, block: bytes }
    }

    /// Enter `home` until this field drops.
    pub fn arm(&mut self, home: &Home) {
        self.entered = home.enter();
    }
}

impl Drop for HomeExit {
    fn drop(&mut self) {
        let Some(entered) = self.entered.take() else { return };
        let home = current();
        drop(entered);
        let to = current();
        if self.block != 0 && to != home {
            let b = self.block as i64;
            post(home, -b, 0, Kind::Region, 0);
            post(to, b, 0, Kind::Region, 0);
        }
    }
}

/// The calling thread becomes backend `procno` (pid `pid`) for the rest of its life (its start; a pooled thread's bind).
pub fn bind_backend(procno: u32, pid: u32) {
    // B2: a session this thread unbound before and no teardown drained ends before the next one binds
    charge::session_drained();
    let a = Acct::backend(procno);
    if a.is_backend() {
        SLOTS[a.0 as usize].pid.store(pid, Ordering::Relaxed);
    }
    let armed = exit_fold::arm();
    DOOR.with(|d| {
        d.exit_armed.set(d.exit_armed.get() || armed);
        d.bound.set(a.0);
    });
    switch(a);
    charge::begin_session(a);
}

/// D4679: the backend account the calling thread is bound to (its session's, from `bind_backend` to `unbind_backend`; a
/// home it enters leaves it as it is), UNBOUND when none.
#[inline]
pub fn bound() -> Acct {
    DOOR.with(|d| Acct(d.bound.get()))
}

/// D4679: run by `unbind_backend` first, on the unbinding thread while it still charges the session's account (mcx's growth
/// shelf frees its blocks there, so the account goes back to its base before the unbind moves what is left to UNBOUND).
static UNBIND_HOOK: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// D4679: sets `unbind_backend`'s first step (the last call wins; mcx's growth shelf, installed at its first park).
pub fn set_unbind_hook(f: fn()) {
    UNBIND_HOOK.store(f as *mut (), Ordering::Release);
}

/// The calling thread leaves its backend account (its exit; an unbind): what the account still holds moves to
/// UNBOUND, so the next backend on the slot starts at zero.
pub fn unbind_backend() {
    let hook = UNBIND_HOOK.load(Ordering::Acquire);
    if !hook.is_null() {
        // SAFETY: only set_unbind_hook stores here, always from fn().
        let f: fn() = unsafe { core::mem::transmute::<*mut (), fn()>(hook) };
        f();
    }
    DOOR.with(|d| d.bound.set(Acct::UNBOUND.0));
    release_side();
    let a = switch(Acct::UNBOUND);
    if a.is_backend() {
        let s = &SLOTS[a.0 as usize];
        // B2: the session's own end check waits for its teardown (`charge::session_drained`)
        charge::exit_session(a);
        s.stmt.store(0, Ordering::Relaxed);
        s.portal.store(0, Ordering::Relaxed);
        s.pid.store(0, Ordering::Relaxed);
        // D4552 (a): a thread still inside the account (a pool worker in the session's home) folds its frees here first
        if UNBIND_WAIT.load(Ordering::Relaxed) {
            wait_entered(s);
        }
        let left = s.net.swap(0, Ordering::Relaxed);
        let mapped = s.mapped.swap(0, Ordering::Relaxed);
        let pooled = s.pooled.swap(0, Ordering::Relaxed);
        for x in [&s.base, &s.peak, &s.mbase, &s.mpeak, &s.hpeak, &s.reserved, &s.rpeak, &s.backed] {
            x.store(0, Ordering::Relaxed);
        }
        s.seq.store(0, Ordering::Relaxed);
        add_q(Acct::UNBOUND, left, mapped, pooled);
        // C8 checker 1's legacy read (D4084): what the unbind moved to UNBOUND
        owner_end::unbound(left);
        for i in 0..LANES {
            for x in [&s.kind[i], &s.kbase[i], &s.kpeak[i]] {
                x.store(0, Ordering::Relaxed);
            }
        }
        s.kdirty.store(false, Ordering::Relaxed);
    }
}

/// D4682 (9d flip condition 6): a client backend thread parks for the next connection instead of exiting. What its exit
/// fold would do at the thread's end runs now, before it waits: the session's drain check, door 1's pending bytes, kind
/// lanes and rounding onto the current account (UNBOUND after the session's unbind), the caches' held-back hits; then the
/// door's account goes back to UNBOUND. So nothing of the ended session folds onto the session that binds the thread
/// next: that one starts at its base.
pub fn park_thread() {
    charge::session_drained();
    fold();
    value::flush();
    switch(Acct::UNBOUND);
}

/// C8 move 6 (D2953; main 2026-10-12T06:14:08Z + 07:14:39Z): backend slot `procno`'s session died by a kill-class exit
/// (quickdie, a crash signal, a panic: no ProcKill, so no `unbind_backend`) in a server that reinitializes in its own
/// process (postgres --no-supervisor, wasm, pgrust_sim; the supervised default ends the process at the crash cycle). At the
/// crash cycle's quiesce point (every child gone: lmgr_proc's `ProcGlobalResetAfterCrash`, on the postmaster's thread) the
/// ledger alone unbinds it: its owner cells end as 'crashed' residues (`owner_end`), its net moves to UNBOUND as an unbind's
/// does and its pool reservations are dropped, so the slot's next session starts at zero and the dead one is a visible row.
/// No Drop glue runs and no memory is freed (D2953's memory side is step 2's). Its threads' count stays (a thread that
/// entered the account and outlived the cycle would leave it later). False: the slot was not bound.
pub fn unbind_dead(procno: u32) -> bool {
    let a = Acct::backend(procno);
    if !a.is_backend() {
        return false;
    }
    let s = &SLOTS[a.0 as usize];
    if s.pid.load(Ordering::Relaxed) == 0 && !charge::session_bound(a) {
        return false;
    }
    let (_, noted) = charge::end_session_as(a, owner_end::Source::Crashed);
    s.stmt.store(0, Ordering::Relaxed);
    s.portal.store(0, Ordering::Relaxed);
    let left = s.net.swap(0, Ordering::Relaxed);
    let mapped = s.mapped.swap(0, Ordering::Relaxed);
    let pooled = s.pooled.swap(0, Ordering::Relaxed);
    let reserved = s.reserved.load(Ordering::Relaxed);
    for x in [&s.base, &s.peak, &s.mbase, &s.mpeak, &s.hpeak, &s.reserved, &s.rpeak] {
        x.store(0, Ordering::Relaxed);
    }
    s.seq.store(0, Ordering::Relaxed);
    add_q(Acct::UNBOUND, left, mapped, pooled);
    owner_end::crashed(a, left, reserved, noted);
    s.pid.store(0, Ordering::Relaxed);
    for i in 0..LANES {
        for x in [&s.kind[i], &s.kbase[i], &s.kpeak[i]] {
            x.store(0, Ordering::Relaxed);
        }
    }
    s.kdirty.store(false, Ordering::Relaxed);
    true
}

/// A statement starts on the calling thread's account: its committed bytes count from here. While one runs already (the
/// extended protocol's Parse, Bind and Execute before their Sync) it changes nothing: they are one statement.
pub fn statement_begin() {
    let a = current();
    let s = &SLOTS[a.0 as usize];
    // only the account's own thread moves its statement number (here, `statement_end`, the unbind): a plain store
    let seq = s.seq.load(Ordering::Relaxed);
    if seq & 1 == 1 {
        return;
    }
    fold();
    restart(s);
    if unattr::on() && a.is_backend() {
        unattr::begin(a);
    }
    let id = charge::OWNER_IDS.fetch_add(1, Ordering::Relaxed);
    s.stmt.store(id, Ordering::Relaxed);
    charge::begin_statement(a, id);
    s.seq.store(seq.wrapping_add(1), Ordering::Relaxed);
}

/// The statement numbers start from the account's now: bases set, peaks cleared.
fn restart(s: &Slot) {
    s.base.store(s.net.load(Ordering::Relaxed), Ordering::Relaxed);
    s.mbase.store(s.mapped.load(Ordering::Relaxed), Ordering::Relaxed);
    for x in [&s.peak, &s.mpeak, &s.hpeak] {
        x.store(0, Ordering::Relaxed);
    }
    s.rpeak.store(s.reserved.load(Ordering::Relaxed), Ordering::Relaxed);
    if s.kdirty.load(Ordering::Relaxed) && s.kdirty.swap(false, Ordering::Relaxed) {
        for i in 0..LANES {
            s.kbase[i].store(s.kind[i].load(Ordering::Relaxed), Ordering::Relaxed);
            s.kpeak[i].store(0, Ordering::Relaxed);
        }
    }
}

/// A statement's numbers at its end, for the statement-end hook (`mempolicy::trace` records them while
/// `PGRUST_MEMPOLICY=trace=1`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatementEnd {
    pub pid: u32,
    pub seq: u64,
    pub peak: u64,
    pub end: u64,
    pub heap_peak: i64,
    pub mapped_peak: i64,
    pub mapped: i64,
    pub pool_reserved_peak: i64,
    /// Each kind's peak (`Kind::ALL` order), both doors', then door 2's alone.
    pub kinds: [i64; KINDS],
    pub mapped_kinds: [i64; KINDS],
}

/// The statement-end hook: called at each statement's end on the ending thread while one is installed.
pub type StatementEndHook = fn(&StatementEnd);

static STATEMENT_END_HOOK: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Installs (or, `None`, removes) the statement-end hook.
pub fn set_statement_end_hook(hook: Option<StatementEndHook>) {
    STATEMENT_END_HOOK.store(hook.map_or(core::ptr::null_mut(), |h| h as *mut ()), Ordering::Release);
}

#[inline]
fn statement_end_hook() -> Option<StatementEndHook> {
    let p = STATEMENT_END_HOOK.load(Ordering::Acquire);
    // SAFETY: `STATEMENT_END_HOOK` only ever holds null or a `StatementEndHook` stored by `set_statement_end_hook`.
    (!p.is_null()).then(|| unsafe { core::mem::transmute::<*mut (), StatementEndHook>(p) })
}

/// The statement on the calling thread's account ends: (its committed bytes at the end, its peak). Nothing runs: (0, 0).
/// The statement-end hook, while one is installed, gets its numbers (`StatementEnd`).
pub fn statement_end() -> (u64, u64) {
    fold();
    value::flush();
    let a = current();
    let s = &SLOTS[a.0 as usize];
    let seq = s.seq.load(Ordering::Relaxed);
    if seq & 1 == 0 {
        return (0, 0);
    }
    let net = s.net.load(Ordering::Relaxed);
    let end = net.saturating_sub(s.base.load(Ordering::Relaxed)).max(0) as u64;
    let peak = (s.peak.load(Ordering::Relaxed).max(0) as u64).max(end);
    if let Some(hook) = statement_end_hook() {
        let (mut kinds, mut mapped_kinds) = ([0i64; KINDS], [0i64; KINDS]);
        for i in 0..KINDS {
            kinds[i] = s.kpeak[i].load(Ordering::Relaxed);
            mapped_kinds[i] = s.kpeak[KINDS + i].load(Ordering::Relaxed);
        }
        hook(&StatementEnd {
            pid: s.pid.load(Ordering::Relaxed),
            seq,
            peak,
            end,
            heap_peak: s.hpeak.load(Ordering::Relaxed).max(0),
            mapped_peak: s.mpeak.load(Ordering::Relaxed).max(0),
            mapped: s.mapped.load(Ordering::Relaxed),
            pool_reserved_peak: s.rpeak.load(Ordering::Relaxed),
            kinds,
            mapped_kinds,
        });
    }
    // C8: what the statement still owns goes to its session (the owner-end residue)
    charge::end_statement(a);
    s.stmt.store(0, Ordering::Relaxed);
    // the numbers restart at the next statement's start; between statements the views show none
    s.seq.store(seq.wrapping_add(1), Ordering::Relaxed);
    (end, peak)
}

/// One account as the views show it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccountRow {
    pub acct: Acct,
    /// Backend rows: the process id (0 = none).
    pub pid: u32,
    /// The statement number (0 = none yet); `running` while it is odd.
    pub statement_seq: u64,
    pub running: bool,
    /// The running statement's committed bytes (`net - base`, at least 0; 0 when none runs).
    pub statement_bytes: u64,
    /// D4035: `net - base` signed (below 0 when another account's bytes were freed here during the statement).
    pub statement_net: i64,
    pub statement_peak_bytes: u64,
    /// Everything on the account (a backend's session memory included); may read below 0 for a while when another
    /// account's bytes are freed here.
    pub account_bytes: i64,
    pub mapped_bytes: i64,
    /// Of `account_bytes`, the page pool's (owner-exact, as door 2's).
    pub pagepool_bytes: i64,
    /// The running statement's most heap (door 1) and mapped (door 2) bytes since its start (0 when none runs).
    pub statement_heap_peak_bytes: u64,
    pub statement_mapped_peak_bytes: u64,
    /// C8 move 3c: what the grant pool holds for the account's statements (their reservations + their leaders' and workers'
    /// live draws past G: a promise of room, not memory) less what their draws already back (`pool_backed_bytes`: pages on
    /// the doors), so `account_bytes` + this counts a drawn byte once; and the claim's most (backed or not) since the running
    /// statement started (the claim itself between statements).
    pub pool_reserved_bytes: i64,
    pub pool_reserved_peak_bytes: i64,
    /// C8 move 3c: of the claim, the bytes its draws back (`pool_back`): `pool_reserved_bytes + pool_backed_bytes` is the
    /// claim.
    pub pool_backed_bytes: i64,
    pub threads: u32,
    /// How far these numbers may lag: `DOOR_FOLD_BYTES` per thread on the account.
    pub fold_slack: u64,
    /// D3872: of `account_bytes`, the bytes the residency pass last found not resident, and of `mapped_bytes` door 2's.
    pub not_resident_bytes: i64,
    pub not_resident_mapped_bytes: i64,
    /// PINS: the account's owners' (statements', portals', session's) cache-entry pins summed: the bytes their scans
    /// reference, of those the evicted ones, and their share of the evicted ones. Counts beside `account_bytes`.
    pub pins_bytes: i64,
    pub pins_evicted_bytes: i64,
    pub pins_share_bytes: i64,
}

impl AccountRow {
    /// D3872: what the account has resident as of the last residency pass: its bytes less the not-resident ones.
    pub fn resident_bytes(&self) -> i64 {
        self.account_bytes.wrapping_sub(self.not_resident_bytes)
    }

    /// D3872: door 2's resident bytes (`mapped_bytes` less door 2's not-resident ones).
    pub fn resident_mapped_bytes(&self) -> i64 {
        self.mapped_bytes.wrapping_sub(self.not_resident_mapped_bytes)
    }
}

/// Account `a` now.
/// Account `a`'s bytes now (both doors; its threads' pending door-1 bytes not folded): `unattr`'s read.
#[inline]
pub(crate) fn account_net(a: Acct) -> i64 {
    SLOTS[a.0 as usize].net.load(Ordering::Relaxed)
}

/// PINS: account `a`'s pin counts moved by `d` (pins, evicted, share).
pub(crate) fn pin_slot(a: Acct, d: (i64, i64, i64)) {
    let s = &SLOTS[a.0 as usize];
    s.pins.fetch_add(d.0, Ordering::Relaxed);
    s.pin_ev.fetch_add(d.1, Ordering::Relaxed);
    s.pin_sh.fetch_add(d.2, Ordering::Relaxed);
}

pub fn account_row(a: Acct) -> AccountRow {
    let s = &SLOTS[a.0 as usize];
    let net = s.net.load(Ordering::Relaxed);
    let seq = s.seq.load(Ordering::Relaxed);
    let running = seq & 1 == 1;
    let delta = if running { net.saturating_sub(s.base.load(Ordering::Relaxed)) } else { 0 };
    let cur = delta.max(0) as u64;
    let threads = s.threads.load(Ordering::Relaxed);
    AccountRow {
        acct: a,
        pid: s.pid.load(Ordering::Relaxed),
        statement_seq: seq,
        running,
        statement_bytes: cur,
        statement_net: delta,
        statement_peak_bytes: if running { (s.peak.load(Ordering::Relaxed).max(0) as u64).max(cur) } else { 0 },
        account_bytes: net,
        mapped_bytes: s.mapped.load(Ordering::Relaxed),
        pagepool_bytes: s.pooled.load(Ordering::Relaxed),
        statement_heap_peak_bytes: if running { s.hpeak.load(Ordering::Relaxed).max(0) as u64 } else { 0 },
        statement_mapped_peak_bytes: if running { s.mpeak.load(Ordering::Relaxed).max(0) as u64 } else { 0 },
        pool_reserved_bytes: s.reserved.load(Ordering::Relaxed).wrapping_sub(s.backed.load(Ordering::Relaxed)),
        pool_reserved_peak_bytes: if running { s.rpeak.load(Ordering::Relaxed) } else { s.reserved.load(Ordering::Relaxed) },
        pool_backed_bytes: s.backed.load(Ordering::Relaxed),
        threads,
        fold_slack: threads as u64 * DOOR_FOLD_BYTES as u64,
        not_resident_bytes: s.nr.load(Ordering::Relaxed),
        not_resident_mapped_bytes: s.nr_mapped.load(Ordering::Relaxed),
        pins_bytes: s.pins.load(Ordering::Relaxed),
        pins_evicted_bytes: s.pin_ev.load(Ordering::Relaxed),
        pins_share_bytes: s.pin_sh.load(Ordering::Relaxed),
    }
}

/// Every account with bytes, a thread, a backend or pool bytes reserved (the view's rows; UNBOUND always), in slot order.
pub fn for_each_row(mut f: impl FnMut(&AccountRow)) {
    // the reader's own statement whole (a view read inside it)
    fold();
    for i in 0..ACCOUNT_SLOTS as u32 {
        let r = account_row(Acct(i));
        // 'unbound' and the pending 'reaping' always (a reader tells "nothing pending" from "no row")
        if r.account_bytes != 0 || r.threads != 0 || r.pid != 0 || r.pool_reserved_bytes.wrapping_add(r.pool_backed_bytes) != 0 || i == 0 || i == Acct::REAPING.0 {
            f(&r);
        }
    }
}

/// Every account's resident bytes (`AccountRow::resident_bytes`) summed by its reclaim class (`Class as usize`): one pass
/// over the slots, for the views and the snapshot, never on a door path. D3808's `LOAD_IN_FLIGHT` is not added: its bytes
/// are on the loading backend's account already (door 1).
pub fn class_bytes() -> [i64; CLASSES] {
    fold();
    class_sums((0..ACCOUNT_SLOTS as u32).map(|i| account_row(Acct(i))))
}

/// `rows`' resident bytes summed by their accounts' classes.
pub fn class_sums(rows: impl Iterator<Item = AccountRow>) -> [i64; CLASSES] {
    let mut out = [0i64; CLASSES];
    for r in rows {
        out[r.acct.class() as usize] = out[r.acct.class() as usize].wrapping_add(r.resident_bytes());
    }
    out
}

/// `for_each_row` into `out`; the count written.
pub fn account_rows(out: &mut [AccountRow]) -> usize {
    let mut n = 0;
    for_each_row(|r| {
        if n < out.len() {
            out[n] = *r;
            n += 1;
        }
    });
    n
}

/// One kind of a running statement's bytes, as `pgrust_memory_account_kinds()` shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KindRow {
    pub acct: Acct,
    pub pid: u32,
    pub statement_seq: u64,
    pub kind: Kind,
    /// The kind's bytes now less at the statement's start (may read below 0: a region parked from the account), and their
    /// most since: Gravity regions of both doors, every other mapped byte under `Mapped`.
    pub bytes: i64,
    pub peak_bytes: u64,
    /// Of them, door 2's (mapped) alone, the same way.
    pub mapped_bytes: i64,
    pub mapped_peak_bytes: u64,
}

/// Every running statement's kinds with bytes or a peak, in slot then kind order.
pub fn for_each_kind_row(mut f: impl FnMut(&KindRow)) {
    fold();
    for i in BACKEND_SLOT_BASE..ACCOUNT_SLOTS as u32 {
        let s = &SLOTS[i as usize];
        let seq = s.seq.load(Ordering::Relaxed);
        if seq & 1 == 0 {
            continue;
        }
        let lane = |j: usize| (s.kind[j].load(Ordering::Relaxed).wrapping_sub(s.kbase[j].load(Ordering::Relaxed)), s.kpeak[j].load(Ordering::Relaxed).max(0) as u64);
        for k in Kind::ALL {
            let ((bytes, peak_bytes), (mapped_bytes, mapped_peak_bytes)) = (lane(k as usize), lane(KINDS + k as usize));
            if bytes != 0 || peak_bytes > 0 || mapped_bytes != 0 || mapped_peak_bytes > 0 {
                f(&KindRow { acct: Acct(i), pid: s.pid.load(Ordering::Relaxed), statement_seq: seq, kind: k, bytes, peak_bytes, mapped_bytes, mapped_peak_bytes });
            }
        }
    }
}

/// `for_each_kind_row` into `out`; the count written.
pub fn kind_rows(out: &mut [KindRow]) -> usize {
    let mut n = 0;
    for_each_kind_row(|r| {
        if n < out.len() {
            out[n] = *r;
            n += 1;
        }
    });
    n
}

/// Account `a`'s bytes of kind `k` now, both doors' (tests, the census).
pub fn kind_bytes(a: Acct, k: Kind) -> i64 {
    SLOTS[a.0 as usize].kind[k as usize].load(Ordering::Relaxed)
}

/// Of `kind_bytes`, door 2's.
pub fn kind_mapped_bytes(a: Acct, k: Kind) -> i64 {
    SLOTS[a.0 as usize].kind[KINDS + k as usize].load(Ordering::Relaxed)
}

/// The sum of every running statement's committed bytes (the ledger's RUNNING).
pub fn running_bytes() -> u64 {
    (BACKEND_SLOT_BASE..ACCOUNT_SLOTS as u32).fold(0u64, |t, i| t.saturating_add(account_row(Acct(i)).statement_bytes))
}

/// The sum over every account (resident less this = what no door counted).
pub fn doors_total() -> i64 {
    SLOTS.iter().fold(0i64, |t, s| t.wrapping_add(s.net.load(Ordering::Relaxed)))
}

/// D4356: the lib tests share one process's UNBOUND account. A test that binds a backend may move its books (an unbind
/// with bytes left, a thread's exit fold after its unbind), so each holds this shared for its run; the one test that reads
/// UNBOUND's delta (owner_end's crash-cycle unbind) holds it alone across its two reads, so no other test's bytes reach
/// UNBOUND between them. The check itself is as strict as before.
#[cfg(test)]
static UNBOUND_TURN: std::sync::RwLock<()> = std::sync::RwLock::new(());

/// D4356: a test that binds a backend holds UNBOUND's turn shared for its run (`UNBOUND_TURN`).
#[cfg(test)]
pub(crate) fn unbound_shared() -> std::sync::RwLockReadGuard<'static, ()> {
    UNBOUND_TURN.read().unwrap_or_else(|e| e.into_inner())
}

/// D4356: the test that reads UNBOUND's delta holds its turn alone between the two reads (`UNBOUND_TURN`).
#[cfg(test)]
pub(crate) fn unbound_alone() -> std::sync::RwLockWriteGuard<'static, ()> {
    UNBOUND_TURN.write().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    /// D3808: a guard gives its account's bytes back when it drops; an account never goes under 0.
    #[test]
    fn a_guard_gives_its_bytes_back() {
        let a = Account::new("test");
        {
            let mut h = a.hold(10 * MIB);
            assert_eq!(a.bytes(), 10 * MIB);
            h.resize(30 * MIB);
            h.resize(20 * MIB);
            assert_eq!((a.bytes(), a.peak()), (20 * MIB, 30 * MIB));
        }
        assert_eq!(a.bytes(), 0);
        a.discharge(MIB);
        assert_eq!(a.bytes(), 0, "never under 0");
        assert_eq!(accounts_total(), LOAD_IN_FLIGHT.bytes() + NESTED_KEPT.bytes());
    }

    #[test]
    fn actuators_parse_all_by_default_and_count_unknown_names() {
        assert_eq!(parse_actuators("all"), (actuator::ALL, 0));
        assert_eq!(parse_actuators(""), (actuator::ALL, 0));
        assert_eq!(parse_actuators(" work_pool , Part_Cache"), (actuator::WORK_POOL | actuator::PART_CACHE, 0));
        assert_eq!(parse_actuators("work_pool,ghosts"), (actuator::WORK_POOL, 1));
        assert_eq!(parse_actuators("ghosts"), (0, 1), "a list of unknown names applies nothing");
    }

    #[test]
    fn modes_read_from_the_guc_index() {
        assert_eq!(Mode::from_index(0), Mode::Off);
        assert_eq!(Mode::from_index(1), Mode::Advise);
        assert_eq!(Mode::from_index(2), Mode::On);
        assert_eq!(Mode::from_index(7), Mode::Advise);
        assert_eq!(Consumer::WorkPool.actuator(), actuator::WORK_POOL);
        assert!(Consumer::ALL.iter().filter(|c| c.is_cache()).count() == 5);
        assert!(Consumer::ALL.iter().filter(|c| c.is_fixed()).count() == 5, "the program caches, per-backend, the catalog L2, md's size cache and pgstat's shared stats");
    }
}

#[cfg(test)]
mod door_tests {
    use super::*;

    /// Each test runs on its own thread with its own backend slot, so the tests share no account.
    fn on(procno: u32, f: impl FnOnce(Acct) + Send + 'static) {
        std::thread::spawn(move || {
            bind_backend(procno, 7000 + procno);
            f(Acct::backend(procno));
            unbind_backend();
        })
        .join()
        .unwrap();
    }

    #[test]
    fn door_one_reaches_the_account_only_at_the_fold_step() {
        let _unbound = crate::unbound_shared(); // D4356: this test binds a backend, so it may move UNBOUND's books
        on(4000, |a| {
            door_heap(1000);
            door_heap(-200);
            assert_eq!(account_row(a).account_bytes, 0, "under the fold step the bytes stay on the thread");
            door_heap(DOOR_FOLD_BYTES as isize);
            assert_eq!(account_row(a).account_bytes, 800 + DOOR_FOLD_BYTES);
            door_heap(-(DOOR_FOLD_BYTES as isize) - 800);
            door_heap(-(DOOR_FOLD_BYTES as isize));
            assert_eq!(account_row(a).account_bytes, -DOOR_FOLD_BYTES, "a fold either way");
            door_heap(DOOR_FOLD_BYTES as isize);
            fold();
            assert_eq!(account_row(a).account_bytes, 0);
            assert_eq!(account_row(a).mapped_bytes, 0, "door 1 is never mapped");
        });
    }

    /// The tests that switch the homes take turns (the switch is the process's).
    static HOME_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// D4552 (a): a Home made on one thread brings a later free on another thread back to the making account while its
    /// session (pid) holds the slot, to UNBOUND after; off, the free stays on the freeing thread's account.
    #[test]
    fn a_home_brings_a_free_back_to_the_account_that_made_it() {
        let _homes = HOME_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let _unbound = crate::unbound_alone(); // reads UNBOUND's delta
        set_home(true);
        let a = Acct::backend(4065);
        assert!(a.is_backend());
        // the maker: its session (pid 9065) stays bound on the slot after its thread is gone
        let home = std::thread::spawn(|| {
            bind_backend(4065, 9065);
            Home::here()
        })
        .join()
        .unwrap();
        assert_eq!(home.acct_now(), a);
        on(4066, move |b| {
            let before = account_row(a).account_bytes;
            {
                let _in = home.enter();
                assert_eq!(current(), a, "the maker's session still holds its slot");
                door_heap(-50_000);
            }
            assert_eq!(account_row(a).account_bytes - before, -50_000, "the free came home");
            assert_eq!(account_row(b).account_bytes, 0, "not on the freeing thread's row");
        });
        // another session (pid 9165) takes the slot: the home is UNBOUND
        std::thread::spawn(move || {
            bind_backend(4065, 9165);
            assert_eq!(home.acct_now(), Acct::UNBOUND);
            unbind_backend();
        })
        .join()
        .unwrap();
        set_home(false);
        assert!(home.enter().is_none(), "off: no switch");
    }

    /// D4552 (a): `arc_bytes` is the block `Arc::new` allocates (its two counts, then the value, `repr(C)`).
    #[test]
    fn an_arc_block_is_its_counts_then_its_value() {
        fn laid<T>() -> u64 {
            let (l, _) = std::alloc::Layout::new::<[usize; 2]>().extend(std::alloc::Layout::new::<T>()).unwrap();
            l.pad_to_align().size() as u64
        }
        assert_eq!(arc_bytes::<u8>(), laid::<u8>());
        assert_eq!(arc_bytes::<[u64; 3]>(), laid::<[u64; 3]>());
        assert_eq!(arc_bytes::<u128>(), laid::<u128>());
        #[repr(align(64))]
        struct Padded(#[allow(dead_code)] u8);
        assert_eq!(arc_bytes::<Padded>(), laid::<Padded>());
        assert_eq!(arc_bytes::<Home>(), laid::<Home>());
    }

    /// D4552 (a): a struct in its own heap block (an `Arc`'s) that frees inside its `Home` takes the block's bytes out of
    /// the home with its last field (`HomeExit::for_block`), onto the thread whose free of the block comes next: that
    /// thread nets zero and the maker's account gives the block back. With `HomeExit::new` the block's free stays on the
    /// freeing thread's row, below zero (the floor's case).
    #[test]
    fn a_homed_blocks_bytes_leave_its_home_with_its_last_field() {
        let _homes = HOME_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let _unbound = crate::unbound_alone(); // reads UNBOUND's delta
        set_home(true);
        struct Homed {
            home: Home,
            exit: HomeExit,
        }
        impl Drop for Homed {
            fn drop(&mut self) {
                let home = self.home;
                self.exit.arm(&home);
            }
        }
        const BLOCK: u64 = arc_bytes::<Homed>();
        let a = Acct::backend(4068);
        for (exit, freer_nets) in [(HomeExit::for_block(BLOCK), 0), (HomeExit::new(), -(BLOCK as i64))] {
            // the maker allocates the block (door 1, as `Arc::new` would) and stays bound after its thread is gone
            let homed = std::thread::spawn(move || {
                bind_backend(4068, 9068);
                door_heap(BLOCK as isize);
                let h = Homed { home: Home::here(), exit };
                fold();
                h
            })
            .join()
            .unwrap();
            let made = account_row(a).account_bytes;
            on(4069, move |b| {
                drop(homed);
                door_heap(-(BLOCK as isize)); // the block's free, on the thread that dropped the last reference
                fold();
                assert_eq!(account_row(b).account_bytes, freer_nets, "the freeing thread's row");
                assert_eq!(account_row(a).account_bytes - made, -(BLOCK as i64) - freer_nets, "the maker's");
            });
            std::thread::spawn(|| {
                bind_backend(4068, 9168);
                unbind_backend();
            })
            .join()
            .unwrap();
        }
        set_home(false);
    }

    /// D4552 (a): a pool worker inside a session's home (its group's last drop) frees `N` bytes of the session's while
    /// the session unbinds. The worker's free returns `N` floored bytes when the unbind moves the row to UNBOUND first.
    fn late_worker_free(n: isize) -> i64 {
        let (_, b0) = floored();
        let (in_tx, in_rx) = std::sync::mpsc::channel::<()>();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            bind_backend(4070, 9070);
            door_heap(n); // the session's block
            fold();
            let home = Home::here();
            let worker = std::thread::spawn(move || {
                let e = home.enter();
                in_tx.send(()).unwrap();
                go_rx.recv().unwrap();
                door_heap(-n); // its free, on the worker inside the session's home
                drop(e);
            });
            in_rx.recv().unwrap();
            // the worker frees 20 ms into the unbind
            let release = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(20));
                go_tx.send(()).unwrap();
            });
            unbind_backend();
            release.join().unwrap();
            worker.join().unwrap();
        })
        .join()
        .unwrap();
        floored().1 - b0
    }

    /// D4552 (a): at the defaults a session's unbind waits for a pool worker still inside the session's home, so the
    /// worker's free folds onto the session's row before the row moves to UNBOUND: nothing floored.
    #[test]
    fn an_unbind_waits_for_a_worker_inside_the_sessions_home() {
        let _homes = HOME_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let _unbound = crate::unbound_alone(); // reads the floor's counts
        set_home(true);
        set_floor(true);
        let floored_bytes = late_worker_free(64 << 10);
        set_floor(false);
        set_home(false);
        assert_eq!(floored_bytes, 0, "the worker's free folded onto the session's row before the unbind moved it");
    }

    /// D4552 (a): the wait counted (`unbind_waits`), and with `ledger_unbind_wait` 0 the same late free floored.
    #[test]
    fn the_unbind_wait_is_counted_and_off_floors_the_late_free() {
        let _homes = HOME_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let _unbound = crate::unbound_alone();
        set_home(true);
        set_floor(true);
        let (w0, _, _) = unbind_waits();
        assert_eq!(late_worker_free(64 << 10), 0);
        let (w1, longest, _) = unbind_waits();
        assert!(w1 > w0 && longest >= 10_000_000, "the unbind waited for the worker: {:?}", unbind_waits());
        set_unbind_wait(false);
        let off = late_worker_free(64 << 10);
        set_unbind_wait(true);
        set_floor(false);
        set_home(false);
        assert_eq!(off, 64 << 10, "without the wait the free lands on the zeroed slot");
    }

    /// D4552 (a): a home entered inside a cache scope pauses the scope (its blocks are the home's, on the home's account,
    /// not the scope holder's as well) and opens it again as it leaves; an account switch that is no home leaves it open.
    #[test]
    fn a_home_entered_inside_a_cache_scope_pauses_the_scope_until_it_leaves() {
        let _homes = HOME_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        set_home(true);
        std::thread::spawn(|| {
            let cache = Owner::Cache(Holder::Of(Consumer::PartCache));
            let scope = crate::arena::cache_scope_for(cache, Kind::Region);
            assert_eq!(door_cache_depth(), 1);
            {
                let _home = Home::of(Acct::of(Consumer::PartCache)).enter();
                assert_eq!(door_cache_depth(), 0, "the scope is paused inside the home");
                let inner = crate::arena::cache_scope_for(cache, Kind::Region);
                assert_eq!(door_cache_depth(), 1, "a scope opened inside the home counts there");
                drop(inner);
            }
            assert_eq!(door_cache_depth(), 1, "open again once the home is left");
            {
                let _worker = enter(Acct::of(Consumer::PartCache));
                assert_eq!(door_cache_depth(), 1, "an account switch that is no home leaves it open");
            }
            drop(scope);
            assert_eq!(door_cache_depth(), 0);
        })
        .join()
        .unwrap();
        set_home(false);
    }

    fn threads_of(a: Acct) -> u32 {
        SLOTS[a.0 as usize].threads.load(Ordering::Relaxed)
    }

    /// D4709: under `home_keep` a thousand home visits move neither account nor a thread count (the visitor keeps both
    /// accounts' counts, counted on both); a plain leave folds the home's, the account's fold its own, each exact.
    #[test]
    fn home_visits_under_home_keep_move_no_shared_counter_and_fold_exact() {
        let _homes = HOME_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let _unbound = crate::unbound_shared();
        set_home(true);
        set_home_keep(true);
        let h = Acct::of(Consumer::NblocksCache);
        on(4075, move |b| {
            fold();
            let (b0, h0, t0) = (account_row(b).account_bytes, account_row(h).account_bytes, threads_of(h));
            for _ in 0..1000 {
                {
                    let _in = Home::of(h).enter();
                    assert_eq!(current(), h);
                    door_heap(100);
                }
                assert_eq!(current(), b);
                door_heap(-30);
            }
            assert_eq!(account_row(b).account_bytes, b0, "the visits folded none of the account's counts");
            assert_eq!(account_row(h).account_bytes, h0, "nor the home's");
            assert_eq!(threads_of(h), t0 + 1, "the visitor stays counted on the home while it keeps its counts");
            assert_eq!(threads_of(b), 1);
            drop(enter(Acct::UNBOUND));
            assert_eq!(account_row(h).account_bytes - h0, 100_000, "a plain leave folds the home's kept counts");
            assert_eq!(threads_of(h), t0);
            fold();
            assert_eq!(account_row(b).account_bytes - b0, -30_000);
        });
        set_home_keep(false);
        set_home(false);
    }

    /// D4781: with `fold_on_park` on a park folds the thread's pending counts onto its account and its kept home counts
    /// onto the home's (the thread stays counted there); off it moves nothing. A thread that never bound folds onto UNBOUND.
    #[test]
    fn park_fold_makes_the_accounts_exact_when_on() {
        let _homes = HOME_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let _unbound = crate::unbound_alone();
        set_home(true);
        set_home_keep(true);
        let h = Acct::of(Consumer::NblocksCache);
        on(4077, move |b| {
            fold();
            let (b0, h0, t0) = (account_row(b).account_bytes, account_row(h).account_bytes, threads_of(h));
            {
                let _in = Home::of(h).enter();
                door_heap(700);
            }
            door_heap(300);
            set_fold_on_park(false);
            park_fold();
            assert_eq!((account_row(b).account_bytes - b0, account_row(h).account_bytes - h0), (0, 0), "off: a park folds nothing");
            set_fold_on_park(true);
            park_fold();
            assert_eq!(account_row(b).account_bytes - b0, 300, "on: the account's pending bytes, exact");
            assert_eq!(account_row(h).account_bytes - h0, 700, "and the home's kept bytes");
            assert_eq!(threads_of(h), t0 + 1, "the thread stays counted on the home");
            DOOR.with(|d| assert_eq!((d.pending.get(), d.s_pending.get(), d.side.get()), (0, 0, h.0 + 1)));
            park_fold();
            assert_eq!((account_row(b).account_bytes - b0, account_row(h).account_bytes - h0), (300, 700), "nothing pending: nothing moves");
            {
                let _in = Home::of(h).enter();
                door_heap(-700);
            }
            door_heap(-300);
            park_fold();
            assert_eq!((account_row(b).account_bytes, account_row(h).account_bytes), (b0, h0));
        });
        let u0 = account_row(Acct::UNBOUND).account_bytes;
        std::thread::spawn(move || {
            door_heap(500);
            park_fold();
            assert_eq!(account_row(Acct::UNBOUND).account_bytes - u0, 500, "an unbound thread's pending bytes, exact");
            door_heap(-500);
            park_fold();
        })
        .join()
        .unwrap();
        assert_eq!(account_row(Acct::UNBOUND).account_bytes, u0);
        set_fold_on_park(false);
        set_home_keep(false);
        set_home(false);
    }

    /// pagepool (the branch's pool bytes on D4781's park): a thread whose pending count nets 0 while its pool bytes do not
    /// (a pool post and a door-1 free of the same size), on its account and kept for a home, still folds at a park with
    /// `fold_on_park` on, so both accounts' pool bytes read exact.
    #[test]
    fn park_fold_folds_pool_only_pending_counts() {
        let _homes = HOME_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let _unbound = crate::unbound_alone();
        set_home(true);
        set_home_keep(true);
        let h = Acct::of(Consumer::NblocksCache);
        on(4078, move |b| {
            fold();
            let (q0, hq0) = (account_row(b).pagepool_bytes, account_row(h).pagepool_bytes);
            let mut o = charge::CtxOwner::NONE;
            {
                let _in = Home::of(h).enter();
                o.post_pool(8192);
                door_heap(-8192);
            }
            o.post_pool(4096);
            door_heap(-4096);
            DOOR.with(|d| assert_eq!((d.pending.get(), d.pooled.get(), d.s_pending.get(), d.s_pooled.get()), (0, 4096, 0, 8192), "pool-only pending counts: the thread's and the home's kept ones"));
            set_fold_on_park(true);
            park_fold();
            assert_eq!(account_row(b).pagepool_bytes - q0, 4096, "the account's pool bytes, folded");
            assert_eq!(account_row(h).pagepool_bytes - hq0, 8192, "the home's kept pool bytes, folded");
            DOOR.with(|d| assert_eq!((d.pooled.get(), d.s_pooled.get()), (0, 0)));
            {
                let _in = Home::of(h).enter();
                o.post_pool(-8192);
                door_heap(8192);
            }
            o.post_pool(-4096);
            door_heap(4096);
            park_fold();
            assert_eq!((account_row(b).pagepool_bytes, account_row(h).pagepool_bytes), (q0, hq0));
        });
        set_fold_on_park(false);
        set_home_keep(false);
        set_home(false);
    }

    /// D4709: workers serving one leader's account each visit a home M times beside their own posts; once they leave the
    /// account (a claim's end) both accounts hold the exact sums and no thread count is left, with `home_keep` on and off.
    #[test]
    fn workers_visiting_a_home_leave_both_accounts_exact() {
        let _homes = HOME_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let _unbound = crate::unbound_shared();
        set_home(true);
        let h = Acct::of(Consumer::SharedStats);
        for keep in [false, true] {
            set_home_keep(keep);
            on(4076, move |l| {
                fold();
                let (l0, h0, t0) = (account_row(l).account_bytes, account_row(h).account_bytes, threads_of(h));
                std::thread::scope(|sc| {
                    for _ in 0..8 {
                        sc.spawn(move || {
                            let _serving = charge::enter_as(Owner::Session(l));
                            for _ in 0..2000 {
                                {
                                    let _in = Home::of(h).enter();
                                    door_heap(64);
                                }
                                door_heap(16);
                            }
                        });
                    }
                });
                assert_eq!(account_row(h).account_bytes - h0, 8 * 2000 * 64, "keep {keep}: the home's bytes, exact");
                assert_eq!(account_row(l).account_bytes - l0, 8 * 2000 * 16, "keep {keep}: the account's bytes, exact");
                assert_eq!((threads_of(h), threads_of(l)), (t0, 1), "keep {keep}: no worker left counted");
                door_heap(-(8 * 2000 * 16));
                fold();
            });
        }
        set_home_keep(false);
        set_home(false);
    }

    /// D4552 (a) backstop: under the floor a one-thread backend account's fold stops at zero, and the bytes freed past it
    /// (charged to another account) credit UNBOUND, counted; door 2's bytes never move.
    #[test]
    fn the_floor_stops_a_one_thread_account_at_zero() {
        let _unbound = crate::unbound_alone(); // reads UNBOUND's delta
        on(4067, |a| {
            assert!(a.is_backend(), "a backend slot (procno < ACCOUNT_SLOTS - BACKEND_SLOT_BASE)");
            door_heap(100_000);
            fold();
            let u0 = account_row(Acct::UNBOUND).account_bytes;
            let (e0, b0) = floored();
            assert_eq!(floor_at_zero(a, -300_000, 0, 0), -100_000, "down to zero");
            add(a, -100_000, 0);
            assert_eq!(account_row(a).account_bytes, 0);
            assert_eq!(account_row(Acct::UNBOUND).account_bytes - u0, -200_000, "the rest credits UNBOUND");
            assert_eq!(floored(), (e0 + 1, b0 + 200_000));
            assert_eq!(floor_at_zero(a, -1_000, -1_000, 0), -1_000, "door 2's bytes are their owners'");
            assert_eq!(floor_at_zero(a, -1_000, 0, -1_000), -1_000, "and the pool's");
            assert_eq!(floor_at_zero(a, 0, 0, 0), 0);
            assert_eq!(floored(), (e0 + 1, b0 + 200_000));
        });
    }

    #[test]
    fn a_statement_counts_from_its_start_and_keeps_its_peak() {
        let _unbound = crate::unbound_shared(); // D4356: this test binds a backend, so it may move UNBOUND's books
        on(4001, |a| {
            door_heap(5000);
            statement_begin();
            let r = account_row(a);
            assert!(r.running && r.statement_bytes == 0 && r.account_bytes == 5000, "{r:?}");
            charge(3 << 20);
            door_heap(1 << 20);
            fold();
            discharge(3 << 20);
            let r = account_row(a);
            assert_eq!((r.statement_bytes, r.statement_peak_bytes, r.mapped_bytes), (1 << 20, 4 << 20, 0));
            let (end, peak) = statement_end();
            assert_eq!((end, peak), (1 << 20, 4 << 20));
            let r = account_row(a);
            assert!(!r.running && r.statement_bytes == 0 && r.account_bytes == 5000 + (1 << 20), "the session keeps it: {r:?}");
            assert_eq!(r.statement_seq, 2);
        });
    }

    #[test]
    fn enter_charges_the_entered_account_and_gives_the_thread_back() {
        let _unbound = crate::unbound_shared(); // D4356: this test binds a backend, so it may move UNBOUND's books
        on(4002, |a| {
            let cache = Acct::of(Consumer::FpTables);
            let before = account_row(cache).account_bytes;
            door_heap(100);
            {
                let _g = enter(cache);
                assert_eq!(account_row(a).account_bytes, 100, "the pending bytes fold onto the account left");
                assert_eq!(current(), cache);
                door_heap(4096);
                charge(1 << 20);
            }
            assert_eq!(current(), a);
            assert_eq!(account_row(cache).account_bytes - before, 4096 + (1 << 20));
            let _g = enter(cache);
            door_heap(-4096);
            discharge(1 << 20);
        });
    }

    #[test]
    fn a_worker_on_the_leaders_account_nets_with_it() {
        let _unbound = crate::unbound_shared(); // D4356: this test binds a backend, so it may move UNBOUND's books
        on(4003, |a| {
            statement_begin();
            door_heap(DOOR_FOLD_BYTES as isize);
            std::thread::spawn(move || {
                let _g = enter(a);
                assert_eq!(account_row(a).threads, 2);
                door_heap(-(DOOR_FOLD_BYTES as isize));
                door_heap(1 << 20);
            })
            .join()
            .unwrap();
            let r = account_row(a);
            assert_eq!((r.account_bytes, r.threads), (1 << 20, 1), "the worker's frees net the leader's allocations");
            statement_end();
        });
    }

    #[test]
    fn transfer_moves_mapped_bytes_and_unbind_leaves_a_clean_slot() {
        let _unbound = crate::unbound_shared(); // D4356: this test binds a backend, so it may move UNBOUND's books
        let a = Acct::backend(4004);
        std::thread::spawn(move || {
            bind_backend(4004, 1);
            charge(8 << 20);
            transfer(a, Acct::PARKED, 2 << 20, true);
            assert_eq!(account_row(a).mapped_bytes, 6 << 20);
            transfer(Acct::PARKED, a, 2 << 20, true);
            door_heap(4096);
            fold();
            transfer(a, Acct::PARKED, 4096, false);
            assert_eq!(account_row(a).account_bytes, (8 << 20) + 4096, "the own account's side waits for a fold");
            fold();
            assert_eq!((account_row(a).account_bytes, account_row(a).mapped_bytes), (8 << 20, 8 << 20), "heap bytes move as heap");
            transfer(Acct::PARKED, a, 4096, false);
            charge_to(Acct::PARKED, 1 << 20);
            discharge_from(Acct::PARKED, 1 << 20);
            door_heap(-4096);
            discharge(8 << 20);
            door_heap(123);
            unbind_backend();
        })
        .join()
        .unwrap();
        let r = account_row(a);
        assert_eq!((r.account_bytes, r.mapped_bytes, r.threads, r.pid), (0, 0, 0, 0));
    }

    /// C2: every account has its reclaim class (design section 3); a consumer's account its consumer's, the caches' the
    /// `is_cache` ones; the slots between REAPING and the first backend, which nothing uses, read unbound.
    #[test]
    fn every_account_has_its_reclaim_class() {
        assert_eq!([Acct::backend(3).class(), Acct::PARKED.class(), Acct::REAPING.class(), Acct::UNBOUND.class()], [Class::Running, Class::Warm, Class::Pending, Class::Unbound]);
        for c in Consumer::ALL {
            assert_eq!(Acct::of(c).class(), c.class(), "{}", c.name());
            assert_eq!(c.is_cache(), c.class() == Class::Cache, "{}", c.name());
        }
        assert_eq!((Acct::of(Consumer::CatalogL2).name(), Acct::of(Consumer::CatalogL2).class()), ("catalog_l2", Class::Fixed));
        assert_eq!((Acct::of(Consumer::NblocksCache).name(), Acct::of(Consumer::NblocksCache).class()), ("nblocks_cache", Class::Fixed));
        assert_eq!((Acct::of(Consumer::SharedStats).name(), Acct::of(Consumer::SharedStats).class()), ("shared_stats", Class::Fixed));
        assert_eq!((Acct::SLOT_DEPOT.class(), Acct::SLOT_DEPOT.name()), (Class::Warm, "slot_depot"));
        assert_eq!((Acct::ROUNDING.class(), Acct::ROUNDING.name()), (Class::Fixed, "allocator_rounding"));
        assert_eq!((Acct::POOL_RESERVE.class(), Acct::POOL_RESERVE.name()), (Class::Warm, "pool_reserve"));
        // the slots between the process accounts and the first backend read unbound
        assert!((Acct::POOL_RESERVE.0 + 1..BACKEND_SLOT_BASE).all(|i| Acct(i).class() == Class::Unbound));
        let names: std::collections::HashSet<&str> = Class::ALL.iter().map(|c| c.name()).collect();
        assert_eq!(names.len(), CLASSES);
    }

    /// D4556: door 1's rounding lands on the process's rounding account, whichever thread moves it and whatever account
    /// the thread is on: a block's rounding taken on one thread and given back on another nets there, and the threads'
    /// own accounts never see it.
    /// The tests that read the process-wide rounding row take turns.
    static ROUNDING_ROW: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn door_round_nets_on_the_rounding_account_across_threads() {
        let _turn = ROUNDING_ROW.lock().unwrap_or_else(|e| e.into_inner());
        let row = || account_row(Acct::ROUNDING).account_bytes;
        let base = row();
        let a = std::thread::spawn(|| {
            bind_backend(4072, 94072);
            fold();
            let before = account_row(Acct::backend(4072)).account_bytes;
            door_round(1_000);
            assert_eq!(round_pending(), 1_000, "under the fold step it waits on the thread");
            fold();
            let after = account_row(Acct::backend(4072)).account_bytes;
            unbind_backend();
            after - before
        })
        .join()
        .unwrap();
        assert_eq!(a, 0, "the thread's own account never takes the rounding");
        assert_eq!(row() - base, 1_000);
        std::thread::spawn(|| {
            door_round(-1_000);
            fold();
        })
        .join()
        .unwrap();
        assert_eq!(row() - base, 0, "given back on another thread, it nets on the rounding account");
        std::thread::spawn(|| {
            bind_backend(4072, 94072);
            door_round((ROUND_FOLD_BYTES + 5) as isize);
            unbind_backend();
        })
        .join()
        .unwrap();
        assert_eq!(row() - base, ROUND_FOLD_BYTES + 5, "past the fold step it folds at once");
        std::thread::spawn(|| door_round(-(ROUND_FOLD_BYTES + 5) as isize)).join().unwrap();
        assert_eq!(row() - base, 0);
    }

    /// D4556: a thread that never binds has no exit fold, so its rounding reaches the rounding account at each call and
    /// none is lost when it exits without a fold.
    #[test]
    fn a_never_bound_threads_rounding_survives_its_exit() {
        let _turn = ROUNDING_ROW.lock().unwrap_or_else(|e| e.into_inner());
        let row = || account_row(Acct::ROUNDING).account_bytes;
        let base = row();
        let pending = std::thread::spawn(|| {
            door_round(700);
            door_round(-200);
            round_pending()
        })
        .join()
        .unwrap();
        assert_eq!(pending, 0, "nothing waits on a thread with no exit fold");
        assert_eq!(row() - base, 500, "the exited thread's rounding is on the account");
        std::thread::spawn(|| door_round(-500)).join().unwrap();
        assert_eq!(row() - base, 0);
    }

    /// C2: the class sums add each row's resident bytes (its bytes less the residency pass's not-resident ones) to its
    /// account's class.
    #[test]
    fn class_sums_add_resident_bytes_by_class() {
        let row = |a: Acct, bytes: i64, nr: i64| AccountRow { account_bytes: bytes, not_resident_bytes: nr, ..account_row(a) };
        let rows = [row(Acct::backend(7), 100, 10), row(Acct::backend(8), 5, 0), row(Acct::of(Consumer::PartCache), 40, 0), row(Acct::REAPING, 3, 0), row(Acct::PARKED, 9, 4), row(Acct::of(Consumer::CatalogL2), 2, 0)];
        let s = class_sums(rows.into_iter());
        assert_eq!(s, [95, 5, 40, 2, 3, 0]);
    }

    #[test]
    fn parked_keepers_count_on_the_account_they_parked_on() {
        // a backend slot no other test here uses (mcx, which parks keepers, is not in this crate's tests)
        let (a, b) = (Acct::backend(4000), Acct::backend(4001));
        let total = keepers_total();
        keepers_moved(a, 8 << 10);
        keepers_moved(a, 8 << 10);
        keepers_moved(b, 1 << 10);
        assert_eq!((keepers(a), keepers(b)), (16 << 10, 1 << 10));
        assert_eq!(keepers_total() - total, 17 << 10);
        keepers_moved(a, -(8 << 10));
        keepers_moved(b, -(1 << 10));
        assert_eq!((keepers(a), keepers(b)), (8 << 10, 0), "a take counts off the account the keeper parked on");
        keepers_moved(a, -(8 << 10));
        assert_eq!(keepers_total(), total);

        // D4679: mcx's growth shelf's blocks are of the keepers (one test: the keeper total is process-wide)
        let g = growth_total();
        growth_moved(a, 16 << 10, 1);
        growth_moved(a, 8 << 10, 1);
        growth_moved(b, 32 << 10, 1);
        keepers_moved(a, 8 << 10);
        assert_eq!((growth(a), growth(b)), ((24 << 10, 2), (32 << 10, 1)), "bytes and blocks, by the account each parked on");
        assert_eq!(keepers(a), 32 << 10, "growth is of the keepers, beside a keeper list's");
        assert_eq!(keepers_total() - total, 64 << 10);
        assert_eq!((growth_total().0 - g.0, growth_total().1 - g.1), (56 << 10, 3));
        growth_moved(a, -(16 << 10), -1);
        growth_moved(b, -(32 << 10), -1);
        assert_eq!((growth(a), growth(b), keepers(a)), ((8 << 10, 1), (0, 0), 16 << 10), "a take counts off the account it parked on");
        growth_moved(a, -(8 << 10), -1);
        keepers_moved(a, -(8 << 10));
        assert_eq!((keepers_total(), growth_total()), (total, g));
    }

    #[test]
    fn slots_past_the_table_are_unbound_and_names_read_right() {
        assert_eq!(Acct::backend(ACCOUNT_SLOTS as u32), Acct::UNBOUND);
        assert_eq!(Acct::backend(0).0, BACKEND_SLOT_BASE);
        assert_eq!(Acct::of(Consumer::PartCache).name(), "part_cache");
        assert_eq!(Acct::PARKED.name(), "parked");
        assert_eq!(Acct::backend(3).name(), "backend");
        assert_eq!(Acct::UNBOUND.name(), "unbound");
        assert_eq!((Kind::Region.name(), Kind::Mapped.name(), Kind::of(KINDS as u8)), ("region", "mapped", Kind::Region));
        assert!(Acct::PARKED.0 < BACKEND_SLOT_BASE);
        assert_eq!(Acct::REAPING.name(), "reaping");
        assert!(Acct::PARKED.0 < Acct::REAPING.0 && Acct::REAPING.0 < Acct::SLOT_DEPOT.0 && Acct::SLOT_DEPOT.0 < Acct::ROUNDING.0 && Acct::ROUNDING.0 < BACKEND_SLOT_BASE);
        assert!(Acct::ROUNDING.0 < Acct::POOL_RESERVE.0 && Acct::POOL_RESERVE.0 < BACKEND_SLOT_BASE);
        let mut shown = (false, false);
        for_each_row(|r| shown = (shown.0 || r.acct == Acct::UNBOUND, shown.1 || r.acct == Acct::REAPING));
        assert_eq!(shown, (true, true), "'unbound' and 'reaping' are rows even at zero");
    }

    /// Door 2 and the region kinds on the thread's own account wait in its pending counts as door 1's do (one fold step
    /// either way); another account takes them at once, and only backends keep kinds.
    #[test]
    fn the_own_account_batches_door_two_and_the_kinds() {
        let _unbound = crate::unbound_shared(); // D4356: this test binds a backend, so it may move UNBOUND's books
        on(4006, |a| {
            statement_begin();
            charge_kind(a, Kind::Sort, 64 << 10);
            count_kind(a, Kind::HashAgg, 32 << 10);
            assert_eq!((account_row(a).mapped_bytes, kind_bytes(a, Kind::Sort), kind_bytes(a, Kind::HashAgg)), (0, 0, 0), "under the step: on the thread");
            let other = Acct::of(Consumer::CondCache);
            let before = account_row(other).account_bytes;
            transfer_kind(a, other, Kind::Sort, 16 << 10, true);
            assert_eq!(account_row(other).account_bytes - before, 16 << 10, "the other side lands at once");
            assert_eq!(kind_bytes(other, Kind::Sort), 0, "no kinds off a backend");
            fold();
            let r = account_row(a);
            assert_eq!((r.mapped_bytes, kind_bytes(a, Kind::Sort), kind_bytes(a, Kind::HashAgg)), (48 << 10, 48 << 10, 32 << 10));
            assert_eq!((kind_mapped_bytes(a, Kind::Sort), kind_mapped_bytes(a, Kind::HashAgg)), (48 << 10, 0), "an allocator region is no door-2 byte");
            assert_eq!(r.statement_mapped_peak_bytes, 48 << 10, "the peak at the fold");
            charge_kind(a, Kind::Sort, DOOR_FOLD_BYTES as u64);
            assert_eq!(kind_bytes(a, Kind::Sort), (48 << 10) + DOOR_FOLD_BYTES, "a step's worth folds at once");
            transfer_kind(other, a, Kind::Sort, 16 << 10, true);
            discharge_kind(a, Kind::Sort, (64 << 10) + DOOR_FOLD_BYTES as u64);
            count_kind(a, Kind::HashAgg, -(32 << 10));
            statement_end();
            assert_eq!((account_row(a).mapped_bytes, kind_bytes(a, Kind::Sort), kind_bytes(a, Kind::HashAgg)), (0, 0, 0));
        });
    }

    #[test]
    fn kinds_split_the_regions_and_the_pool_charge_keeps_its_peak() {
        let _unbound = crate::unbound_shared(); // D4356: this test binds a backend, so it may move UNBOUND's books
        on(4005, |a| {
            statement_begin();
            let k = with_kind(Kind::HashAgg);
            assert_eq!(kind(), Kind::HashAgg);
            charge_kind(a, kind(), 4 << 20);
            count_kind(a, Kind::Sort, 1 << 20);
            charge(1 << 20);
            transfer_kind(a, Acct::PARKED, Kind::HashAgg, 4 << 20, true);
            pool_charge(a, 64 << 20);
            pool_charge(a, -(64 << 20));
            let mut rows = Vec::new();
            for_each_kind_row(|r| {
                if r.acct == a {
                    rows.push((r.kind, r.bytes, r.peak_bytes, r.mapped_bytes, r.mapped_peak_bytes));
                }
            });
            assert_eq!(
                rows,
                vec![(Kind::HashAgg, 0, 4 << 20, 0, 4 << 20), (Kind::Sort, 1 << 20, 1 << 20, 0, 0), (Kind::Mapped, 1 << 20, 1 << 20, 1 << 20, 1 << 20)]
            );
            let r = account_row(a);
            assert_eq!((r.pool_reserved_bytes, r.pool_reserved_peak_bytes, r.statement_mapped_peak_bytes), (0, 64 << 20, 5 << 20));
            transfer_kind(Acct::PARKED, a, Kind::HashAgg, 4 << 20, true);
            discharge_kind(a, Kind::HashAgg, 4 << 20);
            count_kind(a, Kind::Sort, -(1 << 20));
            discharge(1 << 20);
            drop(k);
            assert_eq!(kind(), Kind::Region);
            statement_end();
            assert_eq!(kind_bytes(a, Kind::HashAgg), 0);
        });
        assert_eq!(account_row(Acct::backend(4005)).statement_seq, 0, "an unbound slot starts over");
    }
}
