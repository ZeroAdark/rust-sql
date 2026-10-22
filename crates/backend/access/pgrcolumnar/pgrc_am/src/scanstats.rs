//! SCANSTATS (COMPACTCOST Stage 0, step 1): a pgrcolumnar table's reads, as pg_stat counts and as per-table read
//! telemetry for a costed compaction policy.
//!
//! **pg_stat.** A pgrcolumnar seq scan counts as heapam's does (heapam.c initscan / heap_getnext): `seq_scan` at every
//! start and rescan of a `SO_TYPE_SEQSCAN` scan, per parallel participant (ANALYZE's scan is no seq scan; bitmap,
//! sample, TID and TID-range scans are refused on pgrcolumnar), and `seq_tup_read` per row the scan returns. `idx_*`
//! stay the index AM's. The counting never depends on the slots below: a table with no slot still counts. Each scan
//! (row executor) or statement (Gravity) lands its counts in one pgstat call per table (scans and rows together).
//! - The row executor ([`ScanTally`] in `scan::PgrcScanDescData`): a part granule is credited as the scan enters it
//!   (its rows less its delete mask's), and a granule left part-way (LIMIT, an error, a rescan) gives back the rows it
//!   did not return; a front segment's rows are heapam's own count of the segment's heap scan less the rows its
//!   tombstones skip. Nothing is added per row; the scan's starts and rows land when it is dropped (an erroring
//!   statement's teardown included). Exact: a heap twin's counts for the same statements.
//! - Gravity ([`granule_scan_start`], [`granule_scan_end`]; hosts granule_source's bind and unbind): a scan per
//!   leader bind (heap_source's rule). Binds and unbinds only add plain integers on the leader's thread, at the
//!   statement entry's position each source takes at its bind; the statement's end ([`gravity_statement_end`]: its
//!   finish, or the park of a retained serve) lands them, and pgstat's next flush lands what a statement left without
//!   reaching its end (an error; a backend's exit). Its rows are DERIVED from the statement's granule census, not counted (the derivation's limits are on
//!   `gravity_rows`): exact when the scans skip no granule, below the rows read otherwise. Whether a scan is
//!   key-filtered comes from the lane the plan recorded for the leading `sorted_by` key: no relation is opened at a bind.
//!
//! **Telemetry** ([`stats_text`], `pgrc_scan_stats(regclass)`): per table, process memory only (a restart starts
//! again), in [`SLOTS`] slots keyed by (database, relid), [`PROBE`]-way probed. A slot idle for [`IDLE_S`] is
//! reclaimed; a table that finds no slot is counted nowhere here (no telemetry: a policy reading it keeps today's
//! rules). Counters: scans, granules read, granules pruned (skipped on metadata), granules answered (from metadata, not
//! decoded), dead rows skipped, key-filtered scans (a scan with a range or equality zone predicate on the leading
//! `sorted_by` key, whether or not it pruned anything; only Gravity counts them: the row executor's zone pruning
//! (ZONEPRUNE) counts its skipped granules as pruned, and no key scan).
//! Each has a decayed rate per second: an exponentially weighted moving average with a [`HALF_LIFE_S`] half-life,
//! ticked at most once a second per slot by a fold that finds the slot's lock free (it never waits). Internal readers
//! (compaction, DML's readers, the column cache, SQE's bridge) are not counted, as heap's VACUUM is not (main 10-06).
//! Publishes ([`published`]): per kind, the table's publishes and their wall microseconds (lock waits included), with
//! the same rates; each folds at the publish itself, not through the pending counts below.
//!
//! **Batching.** A scan or statement adds its telemetry to its backend's pending counts (plain integers on the
//! backend's thread: no clock read, no shared memory). They fold into the slots ([`flush`], one clock read) on
//! pgstat's own cadence: pgstat_report_stat's flush path (`pgstat_seams::pgstat_flush_extra`) as the backend goes
//! idle outside a transaction, at most once a second (an idle backend's pending counts flush within pgstat's idle
//! interval, 10 s), and at its exit; also when it holds [`PENDING_CAP`] tables (bounding the search and the memory),
//! and when it reads [`stats_text`] (a session sees its own counts). Staleness bound: another backend's counts can lag
//! until that backend's next pgstat flush, the same as pg_stat_*'s.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use pgsync::Mutex;
use types_core::Oid;
use types_rel::Relation;

/// Slots (a power of two).
pub const SLOTS: usize = 1024;
/// Slots a table probes from its hash.
pub const PROBE: usize = 4;
/// A slot no fold has ticked for this long is reclaimed by a table that finds no slot of its own.
pub const IDLE_S: u64 = 3600;
/// The rates' half-life.
pub const HALF_LIFE_S: f64 = 600.0;
/// A slot's rates tick at most this often.
const TICK_NS: u64 = 1_000_000_000;
const NS: f64 = 1e9;

/// The counters, in this order everywhere: the [`NSCAN`] scan counters, then per publish kind (`PublishKind`'s order,
/// pgrc_write's `counters::KINDS`) its publishes and their wall microseconds.
pub const NAMES: [&str; N] = [
    "scans", "granules_read", "granules_pruned", "granules_answered", "dead_skipped", "key_scans",
    "pub_bulk", "pub_bulk_us", "pub_rotate", "pub_rotate_us", "pub_seal", "pub_seal_us",
    "pub_compile", "pub_compile_us", "pub_compact", "pub_compact_us", "pub_freeze", "pub_freeze_us",
];
pub const N: usize = NSCAN + 2 * PUB_KINDS;
/// The scan counters: what a scan or a statement adds to its backend's pending counts.
pub const NSCAN: usize = 6;
pub const PUB_KINDS: usize = 6;
const PUB: usize = NSCAN;
const SCANS: usize = 0;
const READ: usize = 1;
const PRUNED: usize = 2;
const ANSWERED: usize = 3;
const DEAD: usize = 4;
const KEY: usize = 5;

/// A slot being claimed or reclaimed (no fold lands in it).
const CLAIMING: u64 = u64::MAX;

/// One slot's hot lines: the owner and the totals (fetch_add, relaxed); a scan's fold touches the first line only.
#[repr(C, align(64))]
pub struct Hot {
    owner: AtomicU64,
    totals: [AtomicU64; N],
}

impl Hot {
    pub const fn zero() -> Hot {
        Hot { owner: AtomicU64::new(0), totals: [const { AtomicU64::new(0) }; N] }
    }
}

/// The rates' state: the time of the last tick, the totals it saw, the decayed sums.
#[derive(Clone, Copy)]
pub struct Ewma {
    t_ns: u64,
    seen: [u64; N],
    s: [f64; N],
}

impl Ewma {
    pub const ZERO: Ewma = Ewma { t_ns: 0, seen: [0; N], s: [0.0; N] };

    fn lambda() -> f64 {
        core::f64::consts::LN_2 / HALF_LIFE_S
    }

    /// The events since the last tick join the sums at `now` (the sums decay by `2^(-dt / half-life)` first).
    pub fn tick(&mut self, now: u64, totals: &[u64; N]) {
        let f = (-(now.saturating_sub(self.t_ns) as f64 / NS) * Self::lambda()).exp();
        for k in 0..N {
            self.s[k] = self.s[k] * f + totals[k].saturating_sub(self.seen[k]) as f64;
            self.seen[k] = totals[k];
        }
        self.t_ns = now;
    }

    /// The rates per second at `now`: `lambda x (the sums decayed to now + the events since the last tick)`.
    pub fn rates(&self, now: u64, totals: &[u64; N]) -> [f64; N] {
        let f = (-(now.saturating_sub(self.t_ns) as f64 / NS) * Self::lambda()).exp();
        core::array::from_fn(|k| Self::lambda() * (self.s[k] * f + totals[k].saturating_sub(self.seen[k]) as f64))
    }
}

pgsync::process_global! {
    /// The slots' hot lines, by [`Table::home`].
    static HOT: [Hot; SLOTS] = [const { Hot::zero() }; SLOTS];
    /// Per slot: the last tick's time, also its activity mark (a slot idle for [`IDLE_S`] is reclaimed).
    static TICK: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
    /// Per slot: the rates' state. One lock: a slot ticks at most once a second, and a fold never waits for it.
    static EWMA: Mutex<[Ewma; SLOTS]> = Mutex::new([Ewma::ZERO; SLOTS]);
}

/// A slot table: `hot.len()` slots (a power of two). The process's is [`table`]; tests build their own.
pub struct Table<'a, E> {
    hot: &'a [Hot],
    tick: &'a [AtomicU64],
    ewma: &'a Mutex<E>,
}

fn table() -> Table<'static, [Ewma; SLOTS]> {
    Table { hot: &HOT[..], tick: &TICK[..], ewma: &EWMA }
}

impl<E: AsRef<[Ewma]> + AsMut<[Ewma]>> Table<'_, E> {
    fn home(&self, key: u64) -> usize {
        // Fibonacci hashing: OIDs are allocated in strides, so the high bits of the product spread them
        (key.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) as usize & (self.hot.len() - 1)
    }

    fn probe(&self, key: u64) -> impl Iterator<Item = usize> {
        let (h, mask) = (self.home(key), self.hot.len() - 1);
        (0..PROBE.min(self.hot.len())).map(move |j| (h + j) & mask)
    }

    /// `key`'s slot, without claiming one.
    pub fn find(&self, key: u64) -> Option<usize> {
        self.probe(key).find(|&i| self.hot[i].owner.load(Relaxed) == key)
    }

    /// `key`'s slot, claiming a free one or reclaiming one idle for [`IDLE_S`] when it has none; `None` when every
    /// slot it probes belongs to an active table.
    ///
    /// Known limit: two first folds of one table racing here can each claim a probed slot (the loser sees the
    /// winner's slot still `CLAIMING`). [`Self::find`] then always answers the first in probe order, so the loser's
    /// one fold is lost and its slot idles until it is reclaimed.
    pub fn claim(&self, key: u64, now: u64) -> Option<usize> {
        if let Some(i) = self.find(key) {
            return Some(i);
        }
        for i in self.probe(key) {
            let o = self.hot[i].owner.load(Relaxed);
            let idle = o != CLAIMING && now.saturating_sub(self.tick[i].load(Relaxed)) >= IDLE_S * TICK_NS;
            if (o == 0 || idle) && self.hot[i].owner.compare_exchange(o, CLAIMING, Relaxed, Relaxed).is_ok() {
                for t in &self.hot[i].totals {
                    t.store(0, Relaxed);
                }
                pgsync::lock(self.ewma).as_mut()[i] = Ewma { t_ns: now, ..Ewma::ZERO };
                self.tick[i].store(now, Relaxed);
                self.hot[i].owner.store(key, std::sync::atomic::Ordering::Release);
                return Some(i);
            }
        }
        // another claimer may have taken a probed slot for this key meanwhile
        self.find(key)
    }

    fn totals(&self, i: usize) -> [u64; N] {
        core::array::from_fn(|k| self.hot[i].totals[k].load(Relaxed))
    }

    /// Add `add` to `key`'s slot's counters from `at` on (claimed when it has none); the rates tick when a second has
    /// passed and the lock is free. False when `key` has no slot.
    pub fn fold(&self, key: u64, at: usize, add: &[u64], now: u64) -> bool {
        let Some(i) = self.claim(key, now) else { return false };
        for (t, &x) in self.hot[i].totals[at..].iter().zip(add) {
            if x != 0 {
                t.fetch_add(x, Relaxed);
            }
        }
        if now.saturating_sub(self.tick[i].load(Relaxed)) >= TICK_NS {
            if let Ok(mut e) = self.ewma.try_lock() {
                e.as_mut()[i].tick(now, &self.totals(i));
                self.tick[i].store(now, Relaxed);
            }
        }
        true
    }

    /// `key`'s rates per second at `now`: `None` without a slot, `Some(None)` while the rates' lock is held (never
    /// waited for).
    pub fn try_rates(&self, key: u64, now: u64) -> Option<Option<[f64; N]>> {
        let i = self.find(key)?;
        let totals = self.totals(i);
        let rates = self.ewma.try_lock().ok().map(|e| e.as_ref()[i].rates(now, &totals));
        (self.hot[i].owner.load(Relaxed) == key).then_some(rates)
    }

    /// `key`'s totals and rates per second at `now`; `None` when it has no slot.
    pub fn read(&self, key: u64, now: u64) -> Option<([u64; N], [f64; N])> {
        let i = self.find(key)?;
        let totals = self.totals(i);
        let rates = pgsync::lock(self.ewma).as_ref()[i].rates(now, &totals);
        (self.hot[i].owner.load(Relaxed) == key).then_some((totals, rates))
    }
}

fn key(db: Oid, relid: Oid) -> u64 {
    (db as u64) << 32 | relid as u64
}

fn now() -> u64 {
    pg_clock::mono_ns()
}

/// Tables a backend holds pending before it folds them all early: bounds the search and the memory.
pub const PENDING_CAP: usize = 64;

/// A backend's telemetry not yet in the slots: per table (its slot key, database and relid), the adds since its last
/// flush; `hint` = the last table added (a backend's statements mostly repeat their tables).
struct Pending {
    adds: Vec<(u64, [u64; NSCAN])>,
    hint: usize,
}

thread_local! {
    static PENDING: core::cell::RefCell<Pending> = const { core::cell::RefCell::new(Pending { adds: Vec::new(), hint: 0 }) };
    /// This thread's publish microseconds so far ([`thread_publish_us`]).
    static PUB_US: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
}

/// `add` joins `relid`'s pending counts in database `db` (plain adds on the backend's thread); pgstat's next flush folds them.
fn note(db: Oid, relid: Oid, add: &[u64; NSCAN]) {
    if add.iter().all(|&x| x == 0) {
        return;
    }
    let k = key(db, relid);
    let full = PENDING.with(|p| {
        let mut p = p.borrow_mut();
        let i = match p.adds.get(p.hint) {
            Some(e) if e.0 == k => p.hint,
            _ => match p.adds.iter().position(|e| e.0 == k) {
                Some(i) => i,
                None => {
                    p.adds.push((k, [0; NSCAN])); // alloc: Cold (a backend's first tables; kept across flushes)
                    p.adds.len() - 1
                }
            },
        };
        p.hint = i;
        for (a, &x) in p.adds[i].1.iter_mut().zip(add) {
            *a += x;
        }
        p.adds.len() >= PENDING_CAP
    });
    if full {
        flush();
    } else {
        pgstat::pending::pgstat_note_extra_pending();
    }
}

/// The backend's pending telemetry into the slots, with one clock read: pgstat's flush path (going idle, at most once a
/// second; at exit), a backend at [`PENDING_CAP`] tables, and [`stats_text`] (the caller's own counts first).
pub fn flush() {
    PENDING.with(|p| {
        let mut p = p.borrow_mut();
        if p.adds.is_empty() {
            return;
        }
        let t = now();
        for (k, add) in &p.adds {
            table().fold(*k, 0, add, t);
        }
        p.adds.clear();
        p.hint = 0;
    });
}

/// pgstat's flush of the backend's extra counts (between statements outside a transaction block, and at its exit): the
/// statement entries a statement left without reaching its end land first, then the pending counts fold.
fn flush_from_pgstat() {
    let left = GRAVITY.with(|g| g.borrow().len());
    if left > 0 {
        LATE_LANDS.fetch_add(left as u64, Relaxed);
    }
    gravity_statement_end();
    flush();
}

/// D4809 (F1): the Gravity statement entries pgstat's idle flush landed (not their transaction's end), since start.
static LATE_LANDS: AtomicU64 = AtomicU64::new(0);

/// D4809: [`LATE_LANDS`] now (`pgrust_memory_reconcile` reads it).
pub fn late_lands() -> u64 {
    LATE_LANDS.load(Relaxed)
}

/// Postmaster init (pgrc_maint `register`): the backends' pending telemetry rides pgstat's flush.
pub fn install() {
    pgstat::pending::pgstat_install_extra_flush(flush_from_pgstat);
}

/// A publish of `relid` in database `db` (ingest `catalog_publish`): `us` from before its publish lock to the catalog's
/// record, ending at `now`. Folded into the slot at once on whichever thread publishes (a backend, the seal and
/// compaction workers, VACUUM): the workers never reach pgstat's flush, and beside a publish's manifest write and WAL
/// the probe and two adds are noise. No clock read of its own: `now` is the reading `us` ends at.
pub fn published(db: Oid, relid: Oid, kind: pgrc_format::wal::PublishKind, us: u64, now: u64) {
    PUB_US.with(|c| c.set(c.get() + us));
    let k = kind as usize - 1;
    if k < PUB_KINDS {
        table().fold(key(db, relid), PUB + 2 * k, &[1, us], now);
    }
}

/// The publish microseconds this thread's publishes took so far (a compaction job's ledger takes the delta).
pub fn thread_publish_us() -> u64 {
    PUB_US.with(|c| c.get())
}

/// The deleted rows among granule rows `from..to` under the granule's delete mask (`None`: nothing deleted).
fn dead_in(mask: Option<&[u64; pgrc_scan::DV_GRANULE_WORDS]>, from: u32, to: u32) -> u64 {
    let Some(m) = mask else { return 0 };
    let (from, to) = (from as usize, (to as usize).min(m.len() * 64));
    if from >= to {
        return 0;
    }
    let mut n = 0u64;
    for (w, &word) in m.iter().enumerate().take(to.div_ceil(64)).skip(from / 64) {
        let lo = (w * 64).max(from) - w * 64;
        let hi = ((w + 1) * 64).min(to) - w * 64;
        let bits = if hi - lo == 64 { !0u64 } else { ((1u64 << (hi - lo)) - 1) << lo };
        n += (word & bits).count_ones() as u64;
    }
    n
}

/// The row executor's per-scan counts (`scan::PgrcScanDescData`): folded once, when the scan is dropped.
#[derive(Default)]
pub struct ScanTally {
    /// Scan starts (the begin and each rescan) of a `SO_TYPE_SEQSCAN` scan; 0 = not a counted scan.
    starts: u64,
    granules: u64,
    /// ZONEPRUNE: part granules skipped on their zone (never decoded, never credited).
    pruned: u64,
    dead: u64,
    /// Rows returned: credited per granule entered, less what a stop leaves unreturned.
    rows: u64,
}

impl ScanTally {
    /// A start or rescan of a scan with `flags`: a `seq_scan` when it is a `SO_TYPE_SEQSCAN` scan (heapam's initscan
    /// rule), landed with the scan's rows when it is dropped (an erroring statement's teardown included).
    pub fn start(&mut self, flags: u32) {
        if flags & tableam_vocab::SO_TYPE_SEQSCAN != 0 {
            self.starts += 1;
        }
    }

    /// The scan entered a part granule of `rows` rows under `mask`. Never inlined: it runs once per granule, and inlined
    /// (through `next_granule`) it widened the per-row `getnextslot`'s register set (+~3 instructions per row).
    #[inline(never)]
    pub fn granule(&mut self, rows: u32, mask: Option<&[u64; pgrc_scan::DV_GRANULE_WORDS]>) {
        if self.starts == 0 {
            return;
        }
        let dead = dead_in(mask, 0, rows);
        self.granules += 1;
        self.dead += dead;
        self.rows += rows as u64 - dead;
    }

    /// ZONEPRUNE: the scan skipped a part granule on its zone (a pushed qual's [min, max] fails it).
    pub fn pruned(&mut self) {
        if self.starts != 0 {
            self.pruned += 1;
        }
    }

    /// The scan stops inside a granule of `rows` rows, `cursor` of them passed: the live ones past the cursor were
    /// credited and never returned.
    pub fn stop(&mut self, rows: u32, cursor: u32, mask: Option<&[u64; pgrc_scan::DV_GRANULE_WORDS]>) {
        if self.starts == 0 || cursor >= rows {
            return;
        }
        let left = (rows - cursor) as u64 - dead_in(mask, cursor, rows);
        self.rows = self.rows.saturating_sub(left);
    }

    /// Rows a front segment returned.
    pub fn segment_rows(&mut self, n: u64) {
        if self.starts != 0 {
            self.rows += n;
        }
    }

    /// The scan's counts: its starts and rows into pg_stat in one call, its telemetry into the backend's pending counts.
    pub fn finish(&mut self, rel: &Relation<'_>) {
        let t = core::mem::take(self);
        if t.starts == 0 {
            return;
        }
        // D4809 (F4): a scan dropped in its session's teardown (after pgstat's shutdown hook), or on a thread with no
        // session, counts nothing: a count there built a PgStatState nothing retired (UNBOUND)
        if rel.pgstat_enabled.get() && pgstat::pgstat_counting_open() {
            pgstat::relation::pgstat_count_seq_scans_batched(rel.rd_id, rel.rd_rel.relisshared, t.starts, t.rows);
        }
        let mut add = [0u64; NSCAN];
        add[SCANS] = t.starts;
        add[READ] = t.granules;
        add[PRUNED] = t.pruned;
        add[DEAD] = t.dead;
        note(init_small::globals::MyDatabaseId(), rel.rd_id, &add);
    }
}

/// Gravity's counts for one table within the statement, on the leader's thread (Gravity binds and unbinds every
/// worker's source there): plain integers, added per bind and unbind, landed once by [`gravity_statement_end`].
#[derive(Clone, Copy, Default, PartialEq, Debug)]
struct GravityPending {
    db: Oid,
    relid: Oid,
    /// Leader binds (the scans) and those with a range or equality zone predicate on the leading key.
    scans: u64,
    keyed: u64,
    /// Every bind's census: decoded, skipped (pruned) and answered granules.
    census: [u64; 3],
    /// The part list's rows, granules and deleted rows, summed per leader bind (the derivation's denominators).
    list_rows: u64,
    list_granules: u64,
    list_deleted: u64,
}

thread_local! {
    static GRAVITY: core::cell::RefCell<Vec<GravityPending>> = const { core::cell::RefCell::new(Vec::new()) };
}

/// `relid`'s statement entry in this database: at `at` when it still holds it (the position a source took at its bind:
/// O(1)), else found or, with `add`, added (a source unbound after its statement's end lands in the next statement's entry).
fn gravity_at(g: &mut Vec<GravityPending>, at: u32, relid: Oid, add: bool) -> Option<usize> {
    let (at, db) = (at as usize, init_small::globals::MyDatabaseId());
    if g.get(at).is_some_and(|p| p.db == db && p.relid == relid) {
        return Some(at);
    }
    match g.iter().position(|p| p.db == db && p.relid == relid) {
        Some(i) => Some(i),
        None if !add => None,
        None => {
            g.push(GravityPending { db, relid, ..GravityPending::default() }); // alloc: Cold (a backend's first statement)
            Some(g.len() - 1)
        }
    }
}

/// D4761: whether this thread adds a statement entry it has none for: a backend's thread, whose statement end and pgstat
/// flush land and clear them. A Gravity pool thread binding a worker slot at its claim (D4630 (b)) adds none: nothing on
/// it lands or clears them, so each fresh table's entry stayed for the thread's life.
fn adds_entries() -> bool {
    memledger::bound().is_backend()
}

/// A worker's bind of a granule source over `relid` (hosts granule_source `bind_pg`): the position of the statement's
/// entry on this thread, for its unbind ([`granule_scan_end`]); `u32::MAX` without one. D4761: it never adds one.
pub fn gravity_slot(relid: Oid) -> u32 {
    GRAVITY.with(|g| gravity_at(&mut g.borrow_mut(), u32::MAX, relid, false).map_or(u32::MAX, |i| i as u32))
}

/// Gravity's leader bind of a granule source over `relid` (hosts granule_source `bind_pg`): the scan (`seq_scan`, as
/// heap_source counts its leader's bind), key-filtered when `keyed` (a range or equality zone predicate in force on the
/// lane the plan recorded for the leading `sorted_by` key), and its part list's totals. Plain adds; the position of the
/// statement's entry, for its unbind. pgstat's flush is told there is something (a statement that never reaches its end
/// still lands).
pub fn granule_scan_start(relid: Oid, keyed: bool, list_rows: u64, list_granules: u64, list_deleted: u64) -> u32 {
    let at = GRAVITY.with(|g| {
        let mut g = g.borrow_mut();
        let Some(i) = gravity_at(&mut g, u32::MAX, relid, true) else { return u32::MAX };
        let p = &mut g[i];
        p.scans += 1;
        p.keyed += keyed as u64;
        p.list_rows += list_rows;
        p.list_granules += list_granules;
        p.list_deleted += list_deleted;
        i as u32
    });
    // D4809 (F1): the backend's transaction end lands what this statement leaves unlanded (pgrc_xact_callback)
    if at != u32::MAX && adds_entries() {
        crate::session::ensure_session_hooks();
    }
    pgstat::pending::pgstat_note_extra_pending();
    at
}

/// The unbind of one of Gravity's binds over `relid` (leader or worker; hosts granule_source `unbind_pg`), `at` the
/// position its bind took: its census `[decoded, skipped, answered]` granules. Plain adds. D4761: on a thread with no
/// entry for it that adds none (`adds_entries`) the census goes, as it never landed there.
pub fn granule_scan_end(at: u32, relid: Oid, census: [u64; 3]) {
    if census != [0; 3] {
        GRAVITY.with(|g| {
            let mut g = g.borrow_mut();
            if let Some(i) = gravity_at(&mut g, at, relid, adds_entries()) {
                for (c, x) in g[i].census.iter_mut().zip(census) {
                    *c += x;
                }
            }
        });
    }
}

/// The rows read and the dead rows skipped, DERIVED from a statement's census over its part lists (the source counts
/// granules, not rows):
/// - rows read = (decoded + answered) x the lists' mean rows per granule, less the dead;
/// - dead rows skipped = the same share of the lists' deleted rows (dead rows assumed evenly spread).
///
/// Exact when the scans skip no granule. Otherwise an estimate, below the rows read: the mean granule stands for each
/// one; a decoded granule no row survives is counted skipped by the census, so its rows are not counted read; windows
/// the condition cache answers ('unread', whole-part hits) carry no census count, so their rows are not counted either.
fn gravity_rows(p: &GravityPending) -> (u64, u64) {
    let covered = p.census[0] + p.census[2];
    if covered == 0 || p.list_granules == 0 {
        return (0, 0);
    }
    let dead = share_of(p.list_deleted, covered, p.list_granules);
    (share_of(p.list_rows, covered, p.list_granules).saturating_sub(dead), dead)
}

/// `n x min(covered, total) / total` (`total` > 0): no division when every granule is covered (a scan that skips
/// nothing), a u64 division when the product fits, u128 only when it could overflow.
fn share_of(n: u64, covered: u64, total: u64) -> u64 {
    if covered >= total {
        return n;
    }
    match n.checked_mul(covered) {
        Some(x) => x / total,
        None => (n as u128 * covered as u128 / total as u128) as u64,
    }
}

/// The end of a statement Gravity served (gravity_exec `after_query::statement_end`, on the leader: its finish, and the
/// park of a retained serve, which skips the finish): each table's scans and rows into pg_stat in one call, its
/// telemetry into the backend's pending counts (no clock, no shared memory). A statement that errors before its end
/// leaves its counts here; the backend's next served statement or pgstat's next flush lands them.
pub fn gravity_statement_end() {
    GRAVITY.with(|g| {
        let mut g = g.borrow_mut();
        if g.is_empty() {
            return;
        }
        let (track, db) = (pgstat::pgstat_track_counts() && pgstat::pgstat_counting_open(), init_small::globals::MyDatabaseId());
        for p in g.iter() {
            let (rows, dead) = gravity_rows(p);
            if track && p.db == db {
                pgstat::relation::pgstat_count_seq_scans_batched(p.relid, false, p.scans, rows);
            }
            let mut add = [0u64; NSCAN];
            add[SCANS] = p.scans;
            add[READ] = p.census[0];
            add[PRUNED] = p.census[1];
            add[ANSWERED] = p.census[2];
            add[DEAD] = dead;
            add[KEY] = p.keyed;
            note(p.db, p.relid, &add);
        }
        g.clear();
    });
}

/// A table's decayed read rates per second (the compaction shadow, S4).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ReadRates {
    pub scans: f64,
    pub key_scans: f64,
    pub granules_read: f64,
    pub granules_pruned: f64,
    pub granules_answered: f64,
}

/// `relid`'s read rates in database `db`, from process memory only: `None` without a slot (no telemetry), `Some(None)`
/// while another thread ticks its rates (the caller skips it this time).
pub fn read_rates(db: Oid, relid: Oid) -> Option<Option<ReadRates>> {
    let r = table().try_rates(key(db, relid), now())?;
    Some(r.map(|r| ReadRates { scans: r[SCANS], key_scans: r[KEY], granules_read: r[READ], granules_pruned: r[PRUNED], granules_answered: r[ANSWERED] }))
}

/// `pgrc_scan_stats`' text for `relid` in this database (the caller's pending counts flushed first): `slot=own|none`, then each counter (`name=total`) and each
/// rate per second (`rate.name=x`), space-separated (zeros without a slot).
pub fn stats_text(relid: Oid) -> String {
    // the caller's own pending counts first (a session sees its own reads at once)
    flush();
    let (slot, totals, rates) = match table().read(key(init_small::globals::MyDatabaseId(), relid), now()) {
        Some((t, r)) => ("own", t, r),
        None => ("none", [0; N], [0.0; N]),
    };
    let mut out = format!("slot={slot}");
    for (name, t) in NAMES.iter().zip(totals) {
        out.push_str(&format!(" {name}={t}"));
    }
    for (name, r) in NAMES.iter().zip(rates) {
        out.push_str(&format!(" rate.{name}={r:.6}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000_000;

    #[test]
    fn a_steady_rate_reads_as_itself_and_halves_in_a_half_life() {
        let mut e = Ewma { t_ns: 0, ..Ewma::ZERO };
        let mut totals = [0u64; N];
        // 10 scans a second for an hour, ticked once a second
        for t in 1..=3600u64 {
            totals[SCANS] += 10;
            e.tick(t * S, &totals);
        }
        let r = e.rates(3600 * S, &totals)[SCANS];
        assert!((r - 10.0).abs() < 0.2, "steady rate {r}");
        // then nothing for a half-life
        let r2 = e.rates((3600 + 600) * S, &totals)[SCANS];
        assert!((r2 - r / 2.0).abs() < 0.01, "after a half-life {r2} vs {r}");
        // the events since the last tick count at read time
        totals[SCANS] += 600;
        let r3 = e.rates((3600 + 600) * S, &totals)[SCANS];
        assert!((r3 - r2 - 600.0 * Ewma::lambda()).abs() < 1e-9);
    }

    #[test]
    fn a_burst_decays_from_lambda_times_its_size() {
        let mut e = Ewma::ZERO;
        let mut totals = [0u64; N];
        totals[SCANS] = 100;
        e.tick(5 * S, &totals);
        let r = e.rates(5 * S, &totals)[SCANS];
        assert!((r - 100.0 * Ewma::lambda()).abs() < 1e-12);
        assert!((e.rates(1205 * S, &totals)[SCANS] - r / 4.0).abs() < 1e-9);
    }

    struct Local {
        hot: Vec<Hot>,
        tick: Vec<AtomicU64>,
        ewma: Mutex<Vec<Ewma>>,
    }

    fn local(n: usize) -> Local {
        Local { hot: (0..n).map(|_| Hot::zero()).collect(), tick: (0..n).map(|_| AtomicU64::new(0)).collect(), ewma: Mutex::new(vec![Ewma::ZERO; n]) }
    }

    impl Local {
        fn t(&self) -> Table<'_, Vec<Ewma>> {
            Table { hot: &self.hot, tick: &self.tick, ewma: &self.ewma }
        }
    }

    fn same_home(t: &Table<'_, Vec<Ewma>>, n: usize) -> Vec<u64> {
        let h = t.home(key(5, 16384));
        (16384u32..).map(|r| key(5, r)).filter(|&k| t.home(k) == h).take(n).collect()
    }

    #[test]
    fn a_table_finds_its_slot_and_counts() {
        let l = local(64);
        let t = l.t();
        let k = key(5, 16400);
        assert!(t.read(k, S).is_none());
        assert!(t.fold(k, 0, &[1, 20, 0, 0, 2000, 0], S));
        assert!(t.fold(k, 0, &[1, 20, 0, 0, 2000, 0], 2 * S));
        let (totals, rates) = t.read(k, 2 * S).unwrap();
        assert_eq!(totals[..NSCAN], [2, 40, 0, 0, 4000, 0]);
        assert_eq!(totals[NSCAN..], [0; N - NSCAN], "no publish");
        assert!(rates[SCANS] > 0.0);
        assert_eq!(t.find(key(6, 16400)), None, "another database's table of the same relid is another key");
    }

    #[test]
    fn rates_are_read_without_waiting() {
        let l = local(64);
        let t = l.t();
        let k = key(5, 16500);
        assert_eq!(t.try_rates(k, S), None, "no slot");
        assert!(t.fold(k, 0, &[3, 0, 0, 0, 0, 1], S));
        assert!(t.try_rates(k, 2 * S).unwrap().unwrap()[SCANS] > 0.0);
        let held = pgsync::lock(&l.ewma);
        assert_eq!(t.try_rates(k, 2 * S), Some(None), "the rates' lock held: skipped, not waited for");
        drop(held);
    }

    #[test]
    fn a_full_probe_window_leaves_a_table_without_a_slot_until_one_idles() {
        let l = local(64);
        let t = l.t();
        let ks = same_home(&t, PROBE + 1);
        for &k in &ks[..PROBE] {
            assert!(t.fold(k, 0, &[1, 0, 0, 0, 0, 0], S));
        }
        assert!(!t.fold(ks[PROBE], 0, &[1, 0, 0, 0, 0, 0], 2 * S), "every probed slot is an active table's");
        assert!(t.read(ks[PROBE], 2 * S).is_none());
        // the owners stay active (a tick a second) except the first, idle past IDLE_S
        let later = (IDLE_S + 10) * S;
        for &k in &ks[1..PROBE] {
            assert!(t.fold(k, 0, &[1, 0, 0, 0, 0, 0], later - S));
        }
        assert!(t.fold(ks[PROBE], 0, &[1, 0, 0, 0, 0, 0], later), "an idle slot is reclaimed");
        assert_eq!(t.read(ks[PROBE], later).unwrap().0[SCANS], 1, "a reclaimed slot starts from zero");
        assert!(t.read(ks[0], later).is_none(), "the idle table lost its slot");
    }

    #[test]
    fn gravity_rows_are_the_covered_share_of_the_lists() {
        let p = GravityPending { census: [6, 3, 0], list_rows: 120_000, list_granules: 12, list_deleted: 1200, ..GravityPending::default() };
        // half the granules covered: half the rows, less half the dead
        assert_eq!(gravity_rows(&p), (60_000 - 600, 600));
        let all = GravityPending { census: [8, 0, 4], ..p };
        assert_eq!(gravity_rows(&all), (120_000 - 1200, 1200), "every granule covered: the lists' live rows exactly");
        assert_eq!(gravity_rows(&GravityPending { census: [0, 12, 0], ..p }), (0, 0), "all pruned: nothing read");
        assert_eq!(gravity_rows(&GravityPending::default()), (0, 0));
    }

    fn pending_of(relid: Oid) -> Option<[u64; NSCAN]> {
        let k = key(init_small::globals::MyDatabaseId(), relid);
        PENDING.with(|p| p.borrow().adds.iter().find(|e| e.0 == k).map(|e| e.1))
    }

    #[test]
    fn gravity_binds_add_plainly_land_at_the_statement_end_and_share_at_the_flush() {
        let relid: Oid = 0x7e57_5ca0;
        let db = init_small::globals::MyDatabaseId();
        let at = granule_scan_start(relid, true, 1000, 10, 0);
        assert_eq!(gravity_slot(relid), at, "a worker's bind takes the leader's entry");
        for c in [[3, 4, 0], [2, 0, 1], [0, 0, 0]] {
            granule_scan_end(at, relid, c);
        }
        granule_scan_end(at + 7, relid, [0, 0, 1]); // a stale position finds the entry
        let p = GRAVITY.with(|g| g.borrow().iter().find(|p| p.relid == relid).copied()).unwrap();
        assert_eq!((p.scans, p.keyed, p.census), (1, 1, [5, 4, 2]));
        gravity_statement_end();
        assert!(GRAVITY.with(|g| g.borrow().iter().all(|p| p.relid != relid)), "drained");
        assert_eq!(pending_of(relid), Some([1, 5, 4, 2, 0, 1]), "the backend's pending counts");
        assert!(table().find(key(db, relid)).is_none(), "nothing shared before the flush");
        flush();
        assert_eq!(pending_of(relid), None);
        assert_eq!(table().read(key(db, relid), now()).unwrap().0[..NSCAN], [1, 5, 4, 2, 0, 1]);
        gravity_statement_end();
        flush();
        assert_eq!(table().read(key(db, relid), now()).unwrap().0[..NSCAN], [1, 5, 4, 2, 0, 1], "nothing pending adds nothing");
    }

    #[test]
    fn a_pool_threads_worker_binds_keep_no_entry_and_a_backends_late_unbind_does() {
        // D4761: a Gravity pool thread (no backend bound) binds and unbinds worker slots over fresh tables at their claims
        std::thread::spawn(|| {
            for relid in 0x7e57_5d00..0x7e57_5d10 {
                let at = gravity_slot(relid);
                assert_eq!(at, u32::MAX, "no entry to take");
                granule_scan_end(at, relid, [3, 1, 0]);
            }
            assert_eq!(GRAVITY.with(|g| g.borrow().len()), 0, "nothing kept on the pool thread");
        })
        .join()
        .unwrap();
        // a backend's source unbound after its statement's end: its census waits for the next statement's end
        let _serial = crate::session::tests::bind_serial();
        std::thread::spawn(|| {
            memledger::bind_backend(3_957, 93_957);
            let relid: Oid = 0x7e57_5d20;
            granule_scan_end(u32::MAX, relid, [2, 0, 0]);
            let p = GRAVITY.with(|g| g.borrow().iter().find(|p| p.relid == relid).copied());
            assert_eq!(p.map(|p| p.census), Some([2, 0, 0]), "kept on the backend's thread");
            gravity_statement_end();
            assert_eq!(GRAVITY.with(|g| g.borrow().len()), 0, "landed");
            memledger::unbind_backend();
        })
        .join()
        .unwrap();
    }

    #[test]
    fn a_statement_that_never_reaches_its_end_lands_at_pgstats_flush() {
        let relid: Oid = 0x7e57_5cb0;
        let db = init_small::globals::MyDatabaseId();
        let at = granule_scan_start(relid, false, 800, 8, 0);
        granule_scan_end(at, relid, [8, 0, 0]);
        flush();
        assert!(table().find(key(db, relid)).is_none(), "a fold alone leaves the statement's entries");
        flush_from_pgstat();
        assert!(GRAVITY.with(|g| g.borrow().iter().all(|p| p.relid != relid)), "landed");
        assert_eq!(table().read(key(db, relid), now()).unwrap().0[..NSCAN], [1, 8, 0, 0, 0, 0]);
    }

    #[test]
    fn a_tables_counts_stay_in_the_database_they_were_read_in() {
        let (db, other, relid): (Oid, Oid, Oid) = (init_small::globals::MyDatabaseId(), 0x7e57_d000, 0x7e57_5cc0);
        granule_scan_start(relid, false, 800, 8, 0);
        init_small::globals::SetMyDatabaseId(other);
        let at = granule_scan_start(relid, false, 800, 8, 0);
        granule_scan_end(at, relid, [8, 0, 0]);
        gravity_statement_end();
        flush();
        init_small::globals::SetMyDatabaseId(db);
        assert_eq!(table().read(key(other, relid), now()).unwrap().0[..NSCAN], [1, 8, 0, 0, 0, 0]);
        assert_eq!(table().read(key(db, relid), now()).unwrap().0[..NSCAN], [1, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn the_share_is_the_same_on_every_branch() {
        let reference = |n: u64, c: u64, t: u64| (n as u128 * c.min(t) as u128 / t as u128) as u64;
        // covered >= total: no division
        for (n, c, t) in [(120_000, 12, 12), (120_000, 20, 12), (0, 5, 5), (u64::MAX, 9, 3)] {
            assert_eq!(share_of(n, c, t), reference(n, c, t), "all covered {n} {c} {t}");
        }
        // the product fits u64
        for (n, c, t) in [(120_000u64, 6u64, 12u64), (1_000_003, 1, 123), (7, 3, 11), ((1 << 40) + 17, 1000, 1001)] {
            assert!(n.checked_mul(c).is_some());
            assert_eq!(share_of(n, c, t), reference(n, c, t), "u64 {n} {c} {t}");
        }
        // the product would overflow u64: u128
        for (n, c, t) in [(u64::MAX / 3, 5, 7), (u64::MAX, u32::MAX as u64, u32::MAX as u64 + 1), (1 << 62, 4, 9)] {
            assert!(n.checked_mul(c).is_none());
            assert_eq!(share_of(n, c, t), reference(n, c, t), "u128 {n} {c} {t}");
        }
    }

    #[test]
    fn a_backend_folds_early_at_the_pending_cap() {
        let db = init_small::globals::MyDatabaseId();
        let base: Oid = 0x7e57_6000;
        for r in 0..(PENDING_CAP as Oid - 1) {
            note(db, base + r, &[1, 2, 0, 0, 0, 0]);
            note(db, base + r, &[1, 2, 0, 0, 0, 0]);
        }
        assert_eq!(pending_of(base), Some([2, 4, 0, 0, 0, 0]));
        assert!(table().find(key(db, base)).is_none());
        note(db, base + PENDING_CAP as Oid - 1, &[1, 0, 0, 0, 0, 0]);
        assert_eq!(pending_of(base), None, "the cap's table folded every pending table");
        assert_eq!(table().read(key(db, base), now()).unwrap().0[..NSCAN], [2, 4, 0, 0, 0, 0]);
        note(db, base + 1, &[0; NSCAN]);
        assert_eq!(pending_of(base + 1), None, "zeros add no entry");
    }

    #[test]
    fn dead_rows_count_by_range_under_the_mask() {
        let mut m = [0u64; pgrc_scan::DV_GRANULE_WORDS];
        for r in (9..8192).step_by(10) {
            m[r / 64] |= 1 << (r % 64);
        }
        assert_eq!(dead_in(Some(&m), 0, 8192), 819);
        assert_eq!(dead_in(Some(&m), 0, 10), 1);
        assert_eq!(dead_in(Some(&m), 10, 8192), 818);
        assert_eq!(dead_in(Some(&m), 63, 65), 0);
        assert_eq!(dead_in(Some(&m), 69, 70), 1);
        assert_eq!(dead_in(None, 0, 8192), 0);
        assert_eq!(dead_in(Some(&m), 100, 100), 0);
    }

    #[test]
    fn a_granule_left_part_way_gives_back_the_rows_it_never_returned() {
        let mut m = [0u64; pgrc_scan::DV_GRANULE_WORDS];
        for r in (9..8192).step_by(10) {
            m[r / 64] |= 1 << (r % 64);
        }
        let mut t = ScanTally { starts: 1, ..ScanTally::default() };
        t.granule(8192, Some(&m));
        t.granule(8192, None);
        assert_eq!((t.granules, t.dead, t.rows), (2, 819, 8192 - 819 + 8192));
        // stopped after 5 rows of the second granule: 5 returned
        t.stop(8192, 5, None);
        assert_eq!(t.rows, 8192 - 819 + 5);
        // a scan that is no seq scan counts nothing
        let mut a = ScanTally::default();
        a.granule(8192, None);
        a.segment_rows(3);
        assert_eq!((a.granules, a.rows), (0, 0));
    }

    #[test]
    fn the_publish_counters_follow_the_process_counters_kinds() {
        for (i, (k, name)) in pgrc_write::publish::counters::KINDS.iter().enumerate() {
            assert_eq!(*k as usize, i + 1, "{name}");
            assert_eq!((NAMES[PUB + 2 * i], NAMES[PUB + 2 * i + 1]), (format!("pub_{name}").as_str(), format!("pub_{name}_us").as_str()));
        }
        assert_eq!(pgrc_write::publish::counters::KINDS.len(), PUB_KINDS);
    }

    #[test]
    fn a_publish_folds_at_once_by_kind_with_its_microseconds() {
        use pgrc_format::wal::PublishKind;
        let (db, relid): (Oid, Oid) = (init_small::globals::MyDatabaseId(), 0x7e57_7000);
        let us0 = thread_publish_us();
        published(db, relid, PublishKind::Bulk, 120, now());
        published(db, relid, PublishKind::Bulk, 80, now());
        published(db, relid, PublishKind::Compact, 5000, now());
        assert_eq!(thread_publish_us() - us0, 5200);
        let (t, r) = table().read(key(db, relid), now()).expect("folded without a flush");
        assert_eq!(t[..NSCAN], [0; NSCAN], "no scan");
        let at = |name: &str| NAMES.iter().position(|n| *n == name).unwrap();
        let got: Vec<u64> = ["pub_bulk", "pub_bulk_us", "pub_compact", "pub_compact_us", "pub_seal", "pub_freeze"].iter().map(|n| t[at(n)]).collect();
        assert_eq!(got, [2, 200, 1, 5000, 0, 0]);
        assert!(r[at("pub_bulk")] > 0.0 && r[at("pub_seal")] <= 0.0);
        assert!(PENDING.with(|p| p.borrow().adds.iter().all(|e| e.0 != key(db, relid))), "publishes never join the pending scan counts");
    }
}
