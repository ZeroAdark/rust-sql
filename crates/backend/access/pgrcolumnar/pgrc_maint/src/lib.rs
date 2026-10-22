//! The front's maintenance worker: rotate, seal, compile and reclaim, each in its own transaction.

mod activate;
mod rotator;

use std::sync::Arc;
use std::time::Duration;

use init_small::globals as g;
use mcx::{Mcx, MemoryContext};
use pg_clock::MonoStamp;
use pgrc_front::{compact, compile, maint, reclaim, ring, rotate, seal};
use pgrc_write::seal::ColumnPool;
use pgsync::Mutex;
use types_core::{InvalidOid, Oid};
use types_error::{PgResult, WARNING};
use types_rel::{AccessShareLock, NoLock, Relation};
use types_storage::waiteventset::{WL_EXIT_ON_PM_DEATH, WL_LATCH_SET, WL_TIMEOUT};

const LAUNCHER: &str = "pgrcolumnar front launcher";
const WORKER: &str = "pgrcolumnar front worker";
const NAPTIME_MS: i64 = 100;
const DRAIN_WAIT: Duration = Duration::from_secs(2);
const DRAIN_NAP_MS: i64 = 10;
const LINGER: Duration = Duration::from_secs(5);
fn linger() -> Duration {
    static L: pgsync::OnceLock<Duration> = pgsync::OnceLock::new();
    *L.get_or_init(|| {
        std::env::var("PGRUST_PGRC_MAINT_LINGER_MS").ok().and_then(|v| v.trim().parse::<u64>().ok()).map_or(LINGER, Duration::from_millis)
    })
}
const LINGER_NAP_MS: i64 = 20;

fn since(stamp: MonoStamp) -> Duration {
    Duration::from_nanos(stamp.elapsed_ns())
}

const SWEEP: Duration = Duration::from_secs(10);
fn sweep() -> Duration {
    static S: pgsync::OnceLock<Duration> = pgsync::OnceLock::new();
    *S.get_or_init(|| std::env::var("PGRUST_PGRC_MAINT_SWEEP_MS").ok().and_then(|v| v.trim().parse::<u64>().ok()).map_or(SWEEP, Duration::from_millis))
}
/// A non-empty active segment older than this rotates on a sweep; a tombstone heap holding blocks this long compiles.
const AGE: Duration = Duration::from_secs(60);
const WAIT_EVENT_AUTOVACUUM_MAIN: u32 = 0x0500_0000 | 1;
/// Compile transactions per visit: a switch, the fold of the heap it retired, the reset of the heap folded.
const COMPILE_STEPS: usize = 3;

pgsync::process_global! {
    static RUNNING: Mutex<Vec<Oid>> = Mutex::new(Vec::new());
    static SEEN: Mutex<Vec<(Oid, MonoStamp)>> = Mutex::new(Vec::new());
    static SEEN_TOMB: Mutex<Vec<(Oid, MonoStamp)>> = Mutex::new(Vec::new());
    static COMPACTING: Mutex<Vec<Oid>> = Mutex::new(Vec::new());
    static LAUNCHER_PROC: pgsync::atomic::AtomicI32 = pgsync::atomic::AtomicI32::new(-1);
}

fn wake_launcher() {
    let p = LAUNCHER_PROC.load(pgsync::atomic::Ordering::Relaxed);
    if p >= 0 {
        latch::SetLatch(types_storage::latch::LatchHandle::proc(p));
    }
}

fn bgw(name: &str, main: fn(u64) -> PgResult<()>, arg: u64, restart: i32) -> bgworker::BackgroundWorker {
    bgworker::BackgroundWorker {
        bgw_name: name.to_string(),
        bgw_type: name.to_string(),
        bgw_flags: bgworker::BGWORKER_SHMEM_ACCESS | bgworker::BGWORKER_BACKEND_DATABASE_CONNECTION,
        bgw_start_time: bgworker::BgWorkerStartTime::RecoveryFinished,
        bgw_restart_time: restart,
        bgw_main: main,
        bgw_main_arg: arg,
        bgw_extra: [0; bgworker::BGW_EXTRALEN],
        bgw_notify_pid: 0,
    }
}

pub fn register() -> PgResult<()> {
    // before any backend or the memory watchdog runs
    pgrc_am::inval::apply_cache_cap();
    pgrc_front_seams::seal_pool::set(seal_pool);
    pgrc_front_seams::activate_detached::set(activate::activate_detached);
    pgrc_front_seams::rotate_detached::set(rotator::rotate_detached);
    pgrc_front_seams::wake_compaction::set(wake_launcher);
    pgrc_am::scanstats::install();
    maint::set_compactor(compact_worker_on());
    bgworker::RegisterBackgroundWorker(&bgw(LAUNCHER, launcher_main, 0, bgworker::BGW_DEFAULT_RESTART_INTERVAL))
}

fn signals() {
    procsignal::pqsignal_thread(procsignal::signums::SIGTERM, procsignal::ThreadSignalHandler::Fallible(postgres::die));
    bgworker::BackgroundWorkerUnblockSignals();
}

fn launcher_main(_: u64) -> PgResult<()> {
    signals();
    LAUNCHER_PROC.store(g::MyProcNumber(), pgsync::atomic::Ordering::Relaxed);
    let mut swept = MonoStamp::now();
    loop {
        postgres_seams::check_for_interrupts::call()?;
        let mut dbs: Vec<(Oid, bool)> = maint::due_databases().into_iter().map(|d| (d, false)).collect();
        let mut compacting: Vec<(Oid, bool)> = maint::compact_due_databases().into_iter().map(|d| (d, false)).collect();
        for (_, db, _) in pgrc_am::catalog::settle_due() {
            match dbs.iter_mut().find(|d| d.0 == db) {
                Some(d) => d.1 = true,
                None => dbs.push((db, true)),
            }
        }
        if since(swept) >= sweep() {
            swept = MonoStamp::now();
            for (_, db, _) in pgrc_am::catalog::slots() {
                match dbs.iter_mut().find(|d| d.0 == db) {
                    Some(d) => d.1 = true,
                    None => dbs.push((db, true)),
                }
            }
            let (relids, cs) = (pgrc_am::catalog::relids_by_slot(), pgrc_front::shadow::costs());
            let (mut budget, mut spent_ns) = (pgrc_front::shadow::SWEEP_TABLES, 0u64);
            for (_, db, _) in pgrc_am::catalog::slots().into_iter().filter(|s| pgrc_front::shadow::sweep_due(*s, &relids, &cs, &mut budget, &mut spent_ns)) {
                match compacting.iter_mut().find(|d| d.0 == db) {
                    Some(d) => d.1 = true,
                    None => compacting.push((db, true)),
                }
            }
            pgrc_front::ledger::sweep_done(spent_ns / 1000);
        }
        for (db, sweep) in compacting.into_iter().filter(|_| compact_worker_on()) {
            start_compactor(db, sweep);
        }
        for (db, sweep) in dbs {
            {
                let mut running = pgsync::lock(&RUNNING);
                if running.contains(&db) {
                    continue;
                }
                running.push(db);
            }
            if !matches!(bgworker::RegisterDynamicBackgroundWorker(bgw(WORKER, worker_main, db as u64 | (sweep as u64) << 32, bgworker::BGW_NEVER_RESTART)), Ok(Some(_))) {
                pgsync::lock(&RUNNING).retain(|d| *d != db);
            }
        }
        let rc = latch::WaitLatch(g::MyLatch(), WL_LATCH_SET | WL_TIMEOUT | WL_EXIT_ON_PM_DEATH, NAPTIME_MS, WAIT_EVENT_AUTOVACUUM_MAIN)?;
        if rc & WL_LATCH_SET != 0 {
            if let Some(l) = g::MyLatch() {
                latch::ResetLatch(l);
            }
        }
    }
}

struct Running(Oid);

struct Compacting(Oid);

impl Drop for Compacting {
    fn drop(&mut self) {
        pgsync::lock(&COMPACTING).retain(|d| *d != self.0);
    }
}

/// The trigger does not queue the table meanwhile, and the L0 cap's waiters wait on it.
struct InHand(Oid);

impl Drop for InHand {
    fn drop(&mut self) {
        maint::set_compacting(self.0, false);
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        pgsync::lock(&RUNNING).retain(|d| *d != self.0);
    }
}

fn worker_main(arg: u64) -> PgResult<()> {
    let (db, sweep) = (arg as Oid, arg >> 32 != 0);
    let _running = Running(db);
    signals();
    bgworker::BackgroundWorkerInitializeConnectionByOid(db, InvalidOid, 0)?;
    // CREATE / DROP / ALTER DATABASE SIGTERM this worker like an autovacuum worker (CountOtherDBBackends) instead of
    // waiting out its visit: every step is its own transaction, so a termination aborts at most one unpublished step.
    procarray::ProcSetStatusFlagDbMaintWorker()?;
    let mut relids = maint::take_due(db);
    let due = relids.len();
    if sweep {
        xact::StartTransactionCommand()?;
        for (spc, d, relfilenumber) in pgrc_am::catalog::slots() {
            let relid = if d == db { relfilenumbermap::RelidByRelfilenumber(spc, relfilenumber as Oid)? } else { InvalidOid };
            if relid != InvalidOid && !relids.contains(&relid) {
                relids.push(relid);
            }
        }
        xact::CommitTransactionCommand()?;
    }
    let mut due = due;
    loop {
        for (i, relid) in relids.into_iter().enumerate() {
            if let Err(e) = maintain(relid, i < due) {
                let _ = elog::elog(WARNING, format!("pgrcolumnar: front maintenance of relation {relid}: {}", e.message()));
            }
        }
        // a busy table is due again within a second; a worker that exited would leave the next rotation to the
        // launcher's nap plus a worker start (~0.8 s a cycle), which a wide insert load outruns.
        let idle = MonoStamp::now();
        relids = maint::take_due(db);
        while relids.is_empty() && since(idle) < linger() {
            nap(LINGER_NAP_MS)?;
            relids = maint::take_due(db);
        }
        due = relids.len();
        if relids.is_empty() {
            return Ok(());
        }
    }
}

fn seal_pool() -> Option<Arc<dyn ColumnPool>> {
    static POOL: pgsync::OnceLock<Arc<dyn ColumnPool>> = pgsync::OnceLock::new();
    match POOL.get() {
        Some(p) => Some(Arc::clone(p)),
        None => pgrc_ingest_par::RtColumnPool::global().map(|p| Arc::clone(POOL.get_or_init(|| Arc::new(p.with_lanes(pgrc_front::maint::seal_threads()))))),
    }
}

fn nap(ms: i64) -> PgResult<()> {
    latch_wait(ms)?;
    postgres_seams::check_for_interrupts::call()
}

/// The interrupt check after it is the caller's ([`nap`], [`worker_loop`]).
fn latch_wait(ms: i64) -> PgResult<()> {
    let rc = latch::WaitLatch(g::MyLatch(), WL_LATCH_SET | WL_TIMEOUT | WL_EXIT_ON_PM_DEATH, ms, WAIT_EVENT_AUTOVACUUM_MAIN)?;
    if rc & WL_LATCH_SET != 0 {
        if let Some(l) = g::MyLatch() {
            latch::ResetLatch(l);
        }
    }
    Ok(())
}

fn maintain(relid: Oid, due: bool) -> PgResult<()> {
    step(relid, &mut |mcx, rel| {
        let aged = aged(&SEEN, relid, ring::front_bearing(mcx, rel)?);
        let min = if due { maint::rotate_min(relid) } else if aged { 1 } else { return Ok(()) };
        if rotate::rotate(mcx, rel, min)? {
            pgsync::lock(&SEEN).retain(|s| s.0 != relid);
        }
        Ok(())
    }, Locks::Rotate)?;
    let start = MonoStamp::now();
    loop {
        let mut s = seal::Seals::default();
        // one seal per transaction, so a user publisher waits at most for one seal's publish-to-commit tail
        loop {
            s = seal::Seals::default();
            step(relid, &mut |mcx, rel| {
                s = seal::seal_some_nowait(mcx, rel, 1)?;
                Ok(())
            }, Locks::Front)?;
            if s.sealed == 0 {
                break;
            }
            // the active segment filled while this seal ran; rotate it now, not after compile and reclaim
            let _ = step(relid, &mut |mcx, rel| rotate::rotate(mcx, rel, maint::rotate_min(relid)).map(drop), Locks::Rotate);
        }
        if s.over_budget > 0 {
            maint::queue(g::MyDatabaseId(), relid);
            nap(LINGER_NAP_MS)?;
        }
        if s.skipped == 0 || since(start) >= DRAIN_WAIT {
            break;
        }
        nap(DRAIN_NAP_MS)?;
        // The active segment keeps filling while a drain waits; a full ring only fails this try.
        let _ = step(relid, &mut |mcx, rel| rotate::rotate(mcx, rel, maint::rotate_min(relid)).map(drop), Locks::Rotate);
    }
    // A compile that switched, folded or reset is followed by another after its commit, up to COMPILE_STEPS;
    // no tombstone is deleted, so no tombstone-heap vacuum follows.
    let mut progressed = false;
    step(relid, &mut |mcx, rel| {
        let force = aged(&SEEN_TOMB, relid, compile::blocks(mcx, rel)? > 0);
        if force {
            pgsync::lock(&SEEN_TOMB).retain(|s| s.0 != relid);
        }
        progressed = compile::compile(mcx, rel, force)?.progressed();
        Ok(())
    }, Locks::Both)?;
    for _ in 1..COMPILE_STEPS {
        if !std::mem::take(&mut progressed) {
            break;
        }
        step(relid, &mut |mcx, rel| {
            progressed = compile::compile(mcx, rel, false)?.progressed();
            Ok(())
        }, Locks::Both)?;
    }
    step(relid, &mut |mcx, rel| reclaim::reclaim(mcx, rel).map(drop), Locks::Both)?;
    step(relid, &mut |_, rel| reclaim::settle(rel).map(drop), Locks::Both)
}

fn aged(seen: &Mutex<Vec<(Oid, MonoStamp)>>, relid: Oid, bearing: bool) -> bool {
    let mut seen = pgsync::lock(seen);
    if !bearing {
        seen.retain(|s| s.0 != relid);
        return false;
    }
    match seen.iter().find(|s| s.0 == relid) {
        Some(s) => since(s.1) >= AGE,
        None => {
            seen.push((relid, MonoStamp::now()));
            false
        }
    }
}

fn step(relid: Oid, f: &mut dyn FnMut(Mcx<'_>, &Relation<'_>) -> PgResult<()>, locks: Locks) -> PgResult<()> {
    let visited = match visit(relid, f, locks) {
        Err(e) if pgrc_am::ingest::is_deferral(&e) => Visit::Busy,
        v => v?,
    };
    if visited == Visit::Busy {
        maint::queue(g::MyDatabaseId(), relid);
        nap(LINGER_NAP_MS)?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Visit {
    Ran,
    Busy,
    Gone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Locks {
    Front,
    /// A rotation: the rotate lock alone (beside a seal in progress).
    Rotate,
    Maint,
    Both,
}

fn visit(relid: Oid, f: &mut dyn FnMut(Mcx<'_>, &Relation<'_>) -> PgResult<()>, locks: Locks) -> PgResult<Visit> {
    xact::StartTransactionCommand()?;
    let ctx = MemoryContext::new("pgrc front worker");
    let r = (|| {
        let Some(rel) = table::try_table_open(ctx.mcx(), relid, AccessShareLock)? else { return Ok(Visit::Gone) };
        let out = match tableam_vocab::is_pgrcolumnar_am_oid(rel.rd_rel.relam) {
            false => Ok(Visit::Gone),
            true if !match locks {
                Locks::Front => maint::try_front_lock(relid)?,
                Locks::Rotate => maint::try_rotate_lock(relid)?,
                Locks::Maint => maint::try_maint_lock(relid)?,
                Locks::Both => maint::try_lock(relid)?,
            } => Ok(Visit::Busy),
            true => f(ctx.mcx(), &rel).map(|()| Visit::Ran),
        };
        rel.close(NoLock)?;
        out
    })();
    match r {
        Ok(v) => xact::CommitTransactionCommand().map(|()| v),
        Err(e) => {
            xact::AbortCurrentTransaction()?;
            Err(e)
        }
    }
}

const COMPACTOR: &str = "pgrcolumnar compaction worker";
const COMPACT_BUDGET: Duration = Duration::from_secs(30);
const DEFER_NAP_MS: i64 = 250;
const VISIT_POINT: &str = "pgrc-compact-visit";

fn compact_worker_on() -> bool {
    static ON: pgsync::OnceLock<bool> = pgsync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("PGRUST_PGRC_COMPACT_WORKER").map_or(true, |v| v.trim() != "off"))
}

fn start_compactor(db: Oid, sweep: bool) {
    {
        let mut running = pgsync::lock(&COMPACTING);
        if running.contains(&db) {
            return;
        }
        running.push(db);
    }
    if !matches!(bgworker::RegisterDynamicBackgroundWorker(bgw(COMPACTOR, compact_main, db as u64 | (sweep as u64) << 32, bgworker::BGW_NEVER_RESTART)), Ok(Some(_))) {
        pgsync::lock(&COMPACTING).retain(|d| *d != db);
    }
}

fn compact_main(arg: u64) -> PgResult<()> {
    let (db, sweep) = (arg as Oid, arg >> 32 != 0);
    let _running = Compacting(db);
    signals();
    bgworker::BackgroundWorkerInitializeConnectionByOid(db, InvalidOid, 0)?;
    procarray::ProcSetStatusFlagDbMaintWorker()?;
    let mut relids = maint::take_compact_due(db);
    if sweep {
        xact::StartTransactionCommand()?;
        for slot in pgrc_am::catalog::slots().into_iter().filter(|s| s.1 == db && compact::due(*s)) {
            let relid = relfilenumbermap::RelidByRelfilenumber(slot.0, slot.2 as Oid)?;
            if relid != InvalidOid && !relids.contains(&relid) {
                relids.push(relid);
            }
        }
        xact::CommitTransactionCommand()?;
    }
    worker_loop(db, relids, &mut |relid| compact_table(db, relid), &mut WallClock(MonoStamp::now()))
}

trait Clock {
    fn reset(&mut self);
    fn lingering(&self) -> bool;
    /// The latch wait alone (the interrupt check after it is [`worker_loop`]'s).
    fn wait(&mut self, ms: i64) -> PgResult<()>;
}

struct WallClock(MonoStamp);

impl Clock for WallClock {
    fn reset(&mut self) {
        self.0 = MonoStamp::now();
    }

    fn lingering(&self) -> bool {
        since(self.0) < linger()
    }

    fn wait(&mut self, ms: i64) -> PgResult<()> {
        latch_wait(ms)
    }
}

fn worker_loop(db: Oid, mut relids: Vec<Oid>, table: &mut dyn FnMut(Oid) -> PgResult<Option<bool>>, clock: &mut dyn Clock) -> PgResult<()> {
    loop {
        let mut deferred = Vec::new();
        let pass = relids.clone();
        for relid in relids {
            match table(relid) {
                Ok(Some(true)) => clock.reset(),
                Ok(Some(false)) => {}
                Ok(None) => deferred.push(relid),
                Err(e) => {
                    let _ = elog::elog(WARNING, format!("pgrcolumnar: compaction of relation {relid}: {}", e.message()));
                }
            }
        }
        pgrc_front::shadow::pass_end(db, &pass);
        relids = maint::take_compact_due(db);
        while relids.is_empty() && clock.lingering() {
            clock.wait(if deferred.is_empty() { LINGER_NAP_MS } else { DEFER_NAP_MS })?;
            // SIGTERM / SIGHUP are served here; postmaster death exits in the wait (WL_EXIT_ON_PM_DEATH)
            postgres_seams::check_for_interrupts::call()?;
            relids = maint::take_compact_due(db);
            if relids.is_empty() {
                relids = std::mem::take(&mut deferred);
            }
        }
        if relids.is_empty() {
            return Ok(());
        }
    }
}

/// `Some(ran)`, or `None` when deferred before any job ran.
fn compact_table(db: Oid, relid: Oid) -> PgResult<Option<bool>> {
    maint::set_compacting(relid, true);
    let _hand = InHand(relid);
    injection_point::injection_point(VISIT_POINT)?;
    let start = MonoStamp::now();
    let mut l0 = 0;
    let mut job = |continuing: bool| -> PgResult<(Visit, compact::Compacted)> {
        let mut done = compact::Compacted::default();
        // a job that yielded to a waiting pgrc_flush is retried as a busy table (its next job does not yield)
        let visited = match visit(relid, &mut |mcx, rel| compact::compact(mcx, rel, None, continuing).map(|c| done = c), Locks::Maint) {
            Err(e) if e.sqlstate() == types_error::ERRCODE_LOCK_NOT_AVAILABLE && compact::yielded_to_flush(relid) => Visit::Busy,
            Err(e) if pgrc_am::ingest::is_deferral(&e) => Visit::Busy,
            v => v?,
        };
        l0 = done.l0;
        Ok((visited, done))
    };
    let jobs = match run_jobs(db, relid, &mut || since(start) >= COMPACT_BUDGET, &mut job)? {
        JobsEnd::Deferred => return Ok(None),
        JobsEnd::Gone(ran) => return Ok(Some(ran)),
        JobsEnd::Ran(jobs) => jobs,
    };
    // nothing was due, so no job lowers the table's L0 count until it grows; a job that ran clears the mark
    maint::note_futile(relid, (jobs == 0).then_some(l0));
    if jobs > 0 {
        visit(relid, &mut |mcx, rel| reclaim::reclaim(mcx, rel).map(drop), Locks::Both)?;
    }
    Ok(Some(jobs > 0))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobsEnd {
    Deferred,
    Gone(bool),
    Ran(u32),
}

/// Spent, or deferred after jobs ran, the table is queued again ([`maint::sealed`]). `job`'s argument: a job ran
/// before it in this call.
fn run_jobs(db: Oid, relid: Oid, spent: &mut dyn FnMut() -> bool, job: &mut dyn FnMut(bool) -> PgResult<(Visit, compact::Compacted)>) -> PgResult<JobsEnd> {
    let mut jobs = 0;
    loop {
        if spent() {
            maint::sealed(db, relid);
            break;
        }
        let (visited, done) = job(jobs > 0)?;
        match visited {
            Visit::Gone => return Ok(JobsEnd::Gone(jobs > 0)),
            Visit::Busy if jobs == 0 => return Ok(JobsEnd::Deferred),
            Visit::Ran if done.deferred && jobs == 0 => return Ok(JobsEnd::Deferred),
            Visit::Ran if done.jobs > 0 => jobs += done.jobs,
            // jobs ran and the next was deferred: nothing else would mark the table due in a COPY-only load (a COPY
            // publishes its parts without a seal), and it would wait for the next sweep
            _ if visited == Visit::Busy || done.deferred => {
                maint::sealed(db, relid);
                let _ = elog::elog(types_error::DEBUG1, format!("pgrcolumnar: compaction of relation {relid} queued again after {jobs} jobs: {}", if visited == Visit::Busy { "busy" } else { "deferred" }));
                break;
            }
            _ => break,
        }
    }
    Ok(JobsEnd::Ran(jobs))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ran(jobs: u32, deferred: bool) -> PgResult<(Visit, compact::Compacted)> {
        Ok((Visit::Ran, compact::Compacted { jobs, deferred, ..Default::default() }))
    }

    fn busy() -> PgResult<(Visit, compact::Compacted)> {
        Ok((Visit::Busy, compact::Compacted::default()))
    }

    struct FakeClock {
        lingers: u32,
        left: u32,
        waits: Vec<i64>,
    }

    impl Clock for FakeClock {
        fn reset(&mut self) {
            self.left = self.lingers;
        }

        fn lingering(&self) -> bool {
            self.left > 0
        }

        fn wait(&mut self, ms: i64) -> PgResult<()> {
            self.waits.push(ms);
            assert!(self.waits.len() <= 64, "the worker polled past the cap: {:?}", self.waits);
            self.left -= 1;
            Ok(())
        }
    }

    fn no_interrupts() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            if !postgres_seams::check_for_interrupts::is_installed() {
                postgres_seams::check_for_interrupts::set(|| Ok(()));
            }
        });
    }

    fn worker(db: Oid, relid: Oid, turns: Vec<Vec<PgResult<(Visit, compact::Compacted)>>>) -> (usize, Vec<i64>) {
        no_interrupts();
        let scripted = turns.len();
        let mut turns = turns.into_iter();
        let mut taken = 0;
        let mut clock = FakeClock { lingers: 4, left: 4, waits: Vec::new() };
        worker_loop(db, vec![relid], &mut |r| {
            assert_eq!(r, relid);
            taken += 1;
            let mut visits = turns.next().unwrap_or_else(|| panic!("turn {taken} past the script's {scripted}")).into_iter();
            Ok(match run_jobs(db, r, &mut || false, &mut |_| visits.next().expect("a visit past the turn's script"))? {
                JobsEnd::Deferred => None,
                JobsEnd::Gone(ran) => Some(ran),
                JobsEnd::Ran(jobs) => Some(jobs > 0),
            })
        }, &mut clock)
        .unwrap();
        (taken, clock.waits)
    }

    /// Each test its own `db`: the queue is process-wide.
    fn drive(db: Oid, relid: Oid, budget: usize, script: Vec<PgResult<(Visit, compact::Compacted)>>) -> (JobsEnd, bool) {
        drive_seen(db, relid, budget, script).0
    }

    fn drive_seen(db: Oid, relid: Oid, budget: usize, script: Vec<PgResult<(Visit, compact::Compacted)>>) -> ((JobsEnd, bool), Vec<bool>) {
        let mut jobs = script.into_iter();
        let n = std::cell::Cell::new(0);
        let mut seen = Vec::new();
        let end = run_jobs(db, relid, &mut || n.get() >= budget, &mut |continuing| {
            n.set(n.get() + 1);
            seen.push(continuing);
            jobs.next().expect("no job past the script")
        })
        .unwrap();
        ((end, maint::take_compact_due(db).contains(&relid)), seen)
    }

    #[test]
    fn only_jobs_after_a_job_in_the_call_continue() {
        assert_eq!(drive_seen(91_007, 1, usize::MAX, vec![ran(1, false), ran(1, false), ran(0, false)]).1, vec![false, true, true]);
        assert_eq!(drive_seen(91_007, 2, usize::MAX, vec![busy()]).1, vec![false]);
        assert_eq!(drive_seen(91_007, 3, usize::MAX, vec![ran(0, false)]).1, vec![false]);
    }

    #[test]
    fn a_job_deferred_after_jobs_ran_queues_the_table_again() {
        assert_eq!(drive(91_001, 16_384, usize::MAX, vec![ran(1, false), busy()]), (JobsEnd::Ran(1), true));
    }

    #[test]
    fn a_job_compact_skipped_after_jobs_ran_queues_the_table_again() {
        assert_eq!(drive(91_002, 16_385, usize::MAX, vec![ran(2, false), ran(0, true)]), (JobsEnd::Ran(2), true));
    }

    #[test]
    fn the_other_ends_of_a_compaction_call_stay() {
        assert_eq!(drive(91_003, 1, usize::MAX, vec![busy()]), (JobsEnd::Deferred, false));
        assert_eq!(drive(91_003, 2, usize::MAX, vec![ran(0, true)]), (JobsEnd::Deferred, false));
        assert_eq!(drive(91_003, 3, usize::MAX, vec![ran(1, false), ran(0, false)]), (JobsEnd::Ran(1), false), "nothing more due");
        assert_eq!(drive(91_003, 4, 2, vec![ran(1, false), ran(1, false)]), (JobsEnd::Ran(2), true), "the budget spent");
        assert_eq!(drive(91_003, 5, usize::MAX, vec![ran(1, false), Ok((Visit::Gone, compact::Compacted::default()))]), (JobsEnd::Gone(true), false));
        assert_eq!(drive(91_003, 6, usize::MAX, vec![ran(1, false), ran(2, true), ran(0, false)]), (JobsEnd::Ran(3), false), "a call that ran jobs and was then deferred: the next");
    }

    #[test]
    fn a_deferral_after_jobs_ran_gets_the_table_its_next_turn() {
        assert_eq!(worker(91_004, 16_386, vec![vec![ran(1, false), busy()], vec![ran(0, false)]]).0, 2);
    }

    #[test]
    fn a_compact_skip_after_jobs_ran_gets_the_table_its_next_turn() {
        assert_eq!(worker(91_005, 16_387, vec![vec![ran(2, false), ran(0, true)], vec![ran(0, false)]]).0, 2);
    }

    #[test]
    fn a_deferral_before_any_job_is_the_workers_retry() {
        let (turns, waits) = worker(91_006, 16_388, vec![vec![busy()], vec![ran(0, false)]]);
        assert_eq!(turns, 2);
        assert_eq!(waits.first(), Some(&DEFER_NAP_MS));
    }
}
