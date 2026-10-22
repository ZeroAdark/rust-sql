//! Compaction's front binding: pick one job over the newest generation and run it.

use std::time::Duration;

use mcx::Mcx;
use pgrc_am::compact::CompactOut;
use pgrc_compact::policy::{next_job, Job, JobKind, PartView, Policy, Reason};
use pgrc_compact::runs::{next_member_job, next_run_job, MemberJob, RunUnit};
use types_error::{PgError, PgResult};
use types_rel::{Relation, ShareUpdateExclusiveLock};

use crate::ring;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Compacted {
    pub jobs: u32,
    pub retired: u32,
    pub rows: u64,
    pub dropped: u64,
    pub deferred: bool,
    pub l0: u32,
}

const PAUSE_GUC: &str = "pgrust.pgrc_compact_pause_ms";

pub const FLUSH_CAP: Duration = Duration::from_secs(2);

pgsync::process_global! {
    /// Tables whose last job yielded to a waiting `pgrc_flush`. Their next job does not yield to one (it
    /// finishes, and the table leaves the list), so a flush loop cannot starve a table whose jobs outlast [`FLUSH_CAP`].
    static YIELDED: pgsync::Mutex<Vec<types_core::Oid>> = pgsync::Mutex::new(Vec::new());
    static PART_BYTES: pgsync::Mutex<std::collections::BTreeMap<(pgrc_am::catalog::RelSlot, u32, u64), u64>> = pgsync::Mutex::new(std::collections::BTreeMap::new());
    static PART_BYTES_LISTENING: pgsync::atomic::AtomicBool = pgsync::atomic::AtomicBool::new(false);
}

pub fn yielded_to_flush(relid: types_core::Oid) -> bool {
    pgsync::lock(&YIELDED).contains(&relid)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Force {
    Concat,
    Sorted,
    Dv,
    /// Each job lowers (parts + parts with a DV), so the loop ends; then each part of an older schema is rewritten
    /// alone ([`lagged_job`]), which lowers those.
    Full,
}

pub fn pick(parts: &[PartView], sorted_by: bool, force: Option<Force>) -> Option<Job> {
    pick_by(parts, sorted_by, force, base_policy())
}

fn pick_by(parts: &[PartView], sorted_by: bool, force: Option<Force>, p: Policy) -> Option<Job> {
    let fixed = Policy { max_inputs_tiny: p.max_inputs, ..p };
    let any_run = Policy { min_fanin: 1, aged_fanin: 1, max_share: 1.0, dv_rewrite: f64::INFINITY, ..fixed };
    let forced = |j: Option<Job>| j.map(|j| Job { reason: Reason::Forced, ..j });
    match force {
        None => next_job(parts, sorted_by, &p),
        Some(Force::Concat) => forced(next_job(parts, false, &any_run)),
        Some(Force::Sorted) => forced(next_job(parts, true, &any_run)),
        Some(Force::Dv) => forced(next_job(parts, sorted_by, &Policy { min_fanin: usize::MAX, aged_fanin: usize::MAX, dv_rewrite: f64::MIN_POSITIVE, ..fixed })),
        Some(Force::Full) => forced(next_job(parts, sorted_by, &Policy { min_fanin: 2, aged_fanin: 2, max_share: 1.0, dv_rewrite: f64::MIN_POSITIVE, ..fixed })),
    }
}

/// One job over the newest generation, in the caller's transaction; the caller holds the maintenance lock (so no
/// compile moves the generation; a rotation or seal may add to the head meanwhile).
pub fn run_one<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, force: Option<Force>) -> PgResult<Option<(Job, CompactOut)>> {
    run(mcx, rel, force, true, base_policy(), false)
}

/// VACUUM FULL's job ([`Force::Full`]): [`run_one`] under the caller's AccessExclusiveLock, which every other
/// transaction queues behind, so the job yields to no lock waiter (an ANALYZE or autovacuum waiting would otherwise
/// fail the command) nor to a waiting `pgrc_flush`; interrupts and the pause knob stay.
pub fn run_full<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>) -> PgResult<Option<(Job, CompactOut)>> {
    run(mcx, rel, Some(Force::Full), false, base_policy(), false)
}

fn run<'mcx>(_mcx: Mcx<'mcx>, rel: &Relation<'mcx>, force: Option<Force>, waiters: bool, policy: Policy, first: bool) -> PgResult<Option<(Job, CompactOut)>> {
    if pgrc_am::ingest::compaction_blocked(rel)? || !yield_lock(rel)? {
        return Ok(None);
    }
    let Some(gen) = ring::newest(rel)? else { return Ok(None) };
    let l = rel.rd_locator.get();
    let Some(parts) = views((l.spcOid, l.dbOid, l.relNumber as u64), &pgrc_am::dirpath::table_dir_path(l, rel.rd_backend), &gen).map(|v| without_members(&gen, v)) else { return Ok(None) };
    // any declared key counts, a jsonb path key included (`sorted_by_attnos` is the plain-column prefix,
    // empty for a path-led key); the sorted merge orders a path key by its value as the seal does
    let sorted_by = pgrc_am::reloptions::sorted_by_keys(rel).is_some_and(|k| !k.is_empty());
    let Some(job) = pick_by(&parts, sorted_by, force, policy).or_else(|| if force == Some(Force::Full) { lagged_job(&gen).or_else(|| idless_job(&gen)) } else { None }) else {
        if force.is_none() {
            let _ = crate::shadow::idle(l.dbOid, rel.rd_id, &parts, Some(sorted_by), &Policy::default(), &crate::shadow::costs());
        }
        return Ok(None);
    };
    if force.is_none() {
        crate::shadow::job(l.dbOid, rel.rd_id, &job, &parts, first);
    }
    let pool = pgrc_front_seams::seal_pool::is_installed().then(pgrc_front_seams::seal_pool::call).flatten();
    let pause = Duration::from_millis(crate::maint::setting(PAUSE_GUC).unwrap_or(0));
    let yields = waiters && !yielded_to_flush(rel.rd_id);
    let mut ins = job.inputs.clone();
    ins.sort_unstable();
    let live_in: u64 = parts.iter().filter(|v| ins.binary_search(&v.part_no).is_ok()).map(|v| v.live_bytes()).sum();
    let (cpu0, wal0, pub0) = (crate::ledger::thread_cpu_us(), crate::ledger::thread_wal_bytes(), pgrc_am::scanstats::thread_publish_us());
    let start = pg_clock::MonoStamp::now();
    let out = pgrc_am::compact::run_job(rel, &gen, &job, pool, &|first| between_granules(rel, if first { pause } else { Duration::ZERO }, waiters, yields.then_some(start)))?;
    if waiters && !yields {
        pgsync::lock(&YIELDED).retain(|&r| r != rel.rd_id);
    }
    let cost = crate::ledger::Cost {
        inputs: job.inputs.len() as u64,
        live_in,
        bytes_in: out.input_bytes,
        bytes_out: out.output_bytes,
        rows: out.rows,
        dropped: out.dropped,
        thread_cpu_us: crate::ledger::thread_cpu_us().saturating_sub(cpu0),
        wal_bytes: crate::ledger::thread_wal_bytes().wrapping_sub(wal0),
        publish_us: pgrc_am::scanstats::thread_publish_us() - pub0,
    };
    crate::ledger::record(l.dbOid, rel.rd_id, job.kind, job.reason, &cost);
    Ok(Some((job, out)))
}

fn lagged_job(gen: &pgrc_am::catalog::Generation) -> Option<Job> {
    let fp = gen.manifest.header.schema_fingerprint;
    let fps = pgrc_format::manifest::part_fingerprints(&gen.manifest).ok()?;
    // not a member of a sorted run with others (the run pass rewrites those)
    let k = fps.iter().enumerate().position(|(i, &f)| f != fp && !gen.pinned_member(i))?;
    Some(Job { kind: pgrc_compact::policy::JobKind::DvRewrite, reason: Reason::Forced, inputs: vec![gen.manifest.parts[k].part_no] })
}

/// A member of a sorted run that homes its IDs has no `EXT_RANGES` entry, but its rows have IDs (the run's): not
/// written before row IDs.
pub(crate) fn idless_job(gen: &pgrc_am::catalog::Generation) -> Option<Job> {
    let k = (0..gen.manifest.parts.len()).find(|&i| gen.manifest.parts[i].rows > 0 && gen.ids_of(i).is_empty() && gen.run_member(i).is_none())?;
    Some(Job { kind: pgrc_compact::policy::JobKind::DvRewrite, reason: Reason::Forced, inputs: vec![gen.manifest.parts[k].part_no] })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Renumber {
    Rewrote(CompactOut),
    Kept,
}

/// The loose parts' walk counts the runs' IDs as taken, so no part is placed on a run's IDs.
pub fn renumber_step<'mcx>(_mcx: Mcx<'mcx>, rel: &Relation<'mcx>, cursor: &mut u64) -> PgResult<Option<Renumber>> {
    let Some(gen) = ring::newest(rel)? else { return Ok(None) };
    let runs = gen.runs();
    let in_run = |i: usize| runs.iter().any(|h| h.slots.contains(&Some(i)));
    let run_ids: Vec<pgrc_format::manifest::IdRanges> = runs
        .iter()
        .map(|h| match h.run.homes() {
            true => h.run.ranges.clone(),
            false => pgrc_compact::ident::union_ranges(h.slots.iter().flatten().map(|&i| gen.ids_of(i))),
        })
        .collect();
    // a run's unit number: from the top of the u32 space down, above every part number
    let run_unit = |r: usize| u32::MAX - r as u32;
    let widths: Vec<u64> = (0..runs.len()).map(|r| pgrc_am::runmerge::run_width(&gen, r)).collect();
    let mut units: Vec<(u32, u64, &[(u64, u64)])> = gen.manifest.parts.iter().enumerate().filter(|&(i, _)| !in_run(i)).map(|(i, p)| (p.part_no, p.rows, gen.ids_of(i))).collect();
    units.extend((0..runs.len()).map(|r| (run_unit(r), widths[r], &run_ids[r][..])));
    let Some(step) = pgrc_am::rowid_alloc::renumber_next(*cursor, &units) else { return Ok(None) };
    let (part_no, base) = match step {
        pgrc_am::rowid_alloc::RenumberStep::Keep { hi, .. } => {
            *cursor = hi;
            return Ok(Some(Renumber::Kept));
        }
        pgrc_am::rowid_alloc::RenumberStep::Rewrite { part_no, base } => (part_no, base),
    };
    let pool = pgrc_front_seams::seal_pool::is_installed().then(pgrc_front_seams::seal_pool::call).flatten();
    let pause = Duration::from_millis(crate::maint::setting(PAUSE_GUC).unwrap_or(0));
    if let Some(r) = (0..runs.len()).find(|&r| run_unit(r) == part_no) {
        let out = pgrc_am::runmerge::renumber_run(rel, &gen, r, base, pool, &|first| between_granules(rel, if first { pause } else { Duration::ZERO }, false, None))?;
        *cursor = out.next;
        let _ = elog::elog(types_error::LOG, format!("pgrcolumnar: run renumber of \"{}\": {} rows ({} dropped) into {} members from ID {base}", rel.name(), out.rows, out.dropped, out.members.len())); // alloc: Cold
        return Ok(Some(Renumber::Rewrote(CompactOut { part_no: out.members.first().copied(), rows: out.rows, dropped: out.dropped, input_bytes: out.input_bytes, output_bytes: out.output_bytes })));
    }
    let job = Job { kind: pgrc_compact::policy::JobKind::DvRewrite, reason: Reason::Forced, inputs: vec![part_no] };
    let out = pgrc_am::compact::run_job_renumbered(rel, &gen, &job, base, pool, &|first| between_granules(rel, if first { pause } else { Duration::ZERO }, false, None))?;
    *cursor = base + pgrc_format::rowid::align_up(out.rows);
    Ok(Some(Renumber::Rewrote(out)))
}

pub fn run_all<'mcx>(_mcx: Mcx<'mcx>, rel: &Relation<'mcx>) -> PgResult<Option<pgrc_am::runmerge::RunOut>> {
    if pgrc_am::ingest::compaction_blocked(rel)? || !yield_lock(rel)? {
        return Ok(None);
    }
    let Some(gen) = ring::newest(rel)? else { return Ok(None) };
    let parts: Vec<u32> = gen.manifest.parts.iter().map(|p| p.part_no).collect();
    if parts.is_empty() {
        return Ok(None);
    }
    let pool = pgrc_front_seams::seal_pool::is_installed().then(pgrc_front_seams::seal_pool::call).flatten();
    let pause = Duration::from_millis(crate::maint::setting(PAUSE_GUC).unwrap_or(0));
    let live_in = live_bytes_of(rel, &gen, &parts);
    let clock = JobClock::start();
    let out = pgrc_am::runmerge::run_merge(rel, &gen, &parts, pool, &|first| between_granules(rel, if first { pause } else { Duration::ZERO }, false, None))?;
    clock.record(rel, JobKind::Run, Reason::Forced, run_cost(parts.len(), live_in, &out));
    log_run(rel, "run merge", parts.len(), &out);
    Ok(Some(out))
}

/// A job holds ShareUpdateExclusiveLock (conditional, to transaction end), as autovacuum does: it keeps DML, and it
/// lets the job see a DDL (or VACUUM, CREATE INDEX) waiting behind it; the fast-path AccessShareLock the table is
/// open with has no lock-table entry to ask.
fn yield_lock(rel: &Relation<'_>) -> PgResult<bool> {
    lmgr::ConditionalLockRelation(rel, ShareUpdateExclusiveLock)
}

fn between_granules(rel: &Relation<'_>, pause: Duration, waiters: bool, flush_from: Option<pg_clock::MonoStamp>) -> PgResult<()> {
    let t0 = pg_clock::MonoStamp::now();
    loop {
        postgres_seams::check_for_interrupts::call()?;
        if waiters && lmgr::LockHasWaitersRelation(rel, ShareUpdateExclusiveLock)? {
            let e = PgError::error(format!("pgrcolumnar: compaction of \"{}\" yields to a lock waiter", rel.name()));
            return Err(Box::new(e.with_sqlstate(types_error::ERRCODE_LOCK_NOT_AVAILABLE)));
        }
        if flush_from.is_some_and(|s| Duration::from_nanos(s.elapsed_ns()) >= FLUSH_CAP) && crate::maint::flush_waiting(rel.rd_id)? {
            let mut y = pgsync::lock(&YIELDED);
            if !y.contains(&rel.rd_id) {
                y.push(rel.rd_id);
            }
            drop(y);
            let e = PgError::error(format!("pgrcolumnar: compaction of \"{}\" yields to a waiting pgrc_flush or pgrc_compact", rel.name()));
            return Err(Box::new(e.with_sqlstate(types_error::ERRCODE_LOCK_NOT_AVAILABLE)));
        }
        if Duration::from_nanos(t0.elapsed_ns()) >= pause {
            return Ok(());
        }
        waiter::sleep(Duration::from_millis(10).min(pause));
    }
}

pub fn compact<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, force: Option<Force>, continuing: bool) -> PgResult<Compacted> {
    let why = if crate::maint::backlog(rel.rd_id) > crate::maint::backlog_cap_for(rel.rd_id) / 2 {
        Some("a sealing backlog")
    } else if pgrc_am::ingest::compaction_blocked(rel)? {
        Some("a publish in progress it cannot stack on")
    } else if !yield_lock(rel)? {
        Some("a lock held")
    } else {
        None
    };
    if let Some(why) = why {
        elog::elog(types_error::DEBUG1, format!("pgrcolumnar: compaction of \"{}\" deferred: {why}", rel.name()))?;
        return Ok(Compacted { deferred: true, ..Compacted::default() });
    }
    let policy = if continuing { base_policy().continuing() } else { base_policy() };
    let Some((job, out)) = run(mcx, rel, force, true, policy, !continuing)? else {
        if force.is_none() {
            if let Some(m) = member_pass(mcx, rel, true)? {
                return Ok(Compacted { jobs: 1, retired: m.retired.len() as u32, rows: m.rows, dropped: m.dropped, deferred: false, l0: 0 });
            }
            if let Some(r) = run_pass(mcx, rel, true)? {
                return Ok(Compacted { jobs: 1, retired: r.inputs, rows: r.out.rows, dropped: r.out.dropped, deferred: false, l0: 0 });
            }
        }
        let l = rel.rd_locator.get();
        return Ok(Compacted { l0: pgrc_am::l0::newest((l.spcOid, l.dbOid, l.relNumber as u64)).unwrap_or(0), ..Compacted::default() });
    };
    elog::elog(types_error::DEBUG1, format!("pgrcolumnar: compacted {} parts of \"{}\": {} rows, {} dropped, inputs {:?}, {}", job.inputs.len(), rel.name(), out.rows, out.dropped, job.inputs, if continuing { "continuing" } else { "first" }))?;
    Ok(Compacted { jobs: 1, retired: job.inputs.len() as u32, rows: out.rows, dropped: out.dropped, deferred: false, l0: 0 })
}

pub const L0_TRIGGER: u64 = 64;
const L0_TRIGGER_GUC: &str = "pgrust.pgrc_l0_parts_trigger";

thread_local! {
    static TRIGGER: std::cell::Cell<Option<(u64, u64)>> = const { std::cell::Cell::new(None) };
}

pub fn l0_trigger() -> u64 {
    TRIGGER.with(|c| crate::maint::per_guc_change(c, || crate::maint::setting(L0_TRIGGER_GUC).unwrap_or(L0_TRIGGER)))
}

/// Not while the worker has the table in hand (its jobs in that call go on while one is due; a call queued behind
/// them would start on the few parts published meanwhile), nor while its last visit's verdict, nothing due, holds
/// ([`crate::maint::futile`]: until [`crate::maint::FUTILE_SLACK`] more L0 parts, or the sweep).
pub fn bulk_published(rel: &Relation<'_>) {
    let trigger = l0_trigger();
    let l = rel.rd_locator.get();
    let slot = (l.spcOid, l.dbOid, l.relNumber as u64);
    let parent = rel.rd_id;
    let l0_due = trigger != 0 && !crate::maint::compacting(parent) && pgrc_am::l0::newest(slot).is_some_and(|l0| u64::from(l0) >= trigger && !crate::maint::futile(parent, l0));
    // a bulk load's full parts are no L0 parts, so this queues a COPY-fed table for its run pass, even
    // while the worker has it in hand: the sweep's `due` has no relation to read `sort_runs` from, so a publish the
    // visit's last look missed would otherwise wait for the next publish (a visit queued behind it at worst finds none)
    let runs_due = run_pass_on(rel) && head_run_job(slot).is_some();
    if (l0_due || runs_due)
        && crate::maint::sealed(init_small::globals::MyDatabaseId(), parent)
        && pgrc_front_seams::wake_compaction::is_installed()
    {
        pgrc_front_seams::wake_compaction::call();
    }
}

pub fn due(slot: pgrc_am::catalog::RelSlot) -> bool {
    head_views(slot).is_some_and(|v| next_job(&v, false, &base_policy()).is_some()) || head_member_job(slot).is_some()
}

pub(crate) fn head_views(slot: pgrc_am::catalog::RelSlot) -> Option<Vec<PartView>> {
    let (Some((gens, head)), Some(dir)) = (pgrc_am::catalog::generations(slot), pgrc_am::catalog::dir_of(slot)) else { return None };
    gens.iter().find(|g| g.gen == head).and_then(|g| views(slot, &dir, g).map(|v| without_members(g, v)))
}

/// A run's sole member is a part like any other: a job that takes it brings the run's ranges and empties the run
/// (`pgrc_am::compact::member_ids`).
fn without_members(gen: &pgrc_am::catalog::Generation, v: Vec<PartView>) -> Vec<PartView> {
    let pinned: std::collections::BTreeSet<u32> = gen.runs().iter().filter(|h| h.run.homes() && h.run.live().count() > 1).flat_map(|h| h.run.live()).collect();
    if pinned.is_empty() {
        return v;
    }
    v.into_iter().filter(|p| !pinned.contains(&p.part_no)).collect()
}


fn base_policy() -> Policy {
    let p = Policy::default();
    match pgrc_am::runmerge::test_target_rows() {
        Some(t) => Policy { max_rows: t, ..p },
        None => p,
    }
}

fn run_pass_on(rel: &Relation<'_>) -> bool {
    pgrc_am::reloptions::sort_runs(rel) && pgrc_am::runmerge::table_sort_key(rel).is_some()
}

fn run_units(gen: &pgrc_am::catalog::Generation, views: &[PartView], p: &Policy) -> Vec<RunUnit> {
    let by_no: std::collections::BTreeMap<u32, &PartView> = views.iter().map(|v| (v.part_no, v)).collect();
    let in_run = |i: usize| gen.runs().iter().any(|h| h.slots.contains(&Some(i)));
    let mut units = Vec::new();
    for (i, rec) in gen.manifest.parts.iter().enumerate() {
        if let Some(v) = by_no.get(&rec.part_no).filter(|v| !in_run(i) && v.full(p) && v.live > 0) {
            units.push(RunUnit { level: 0, order: u64::from(rec.part_no), rows: v.live, bytes: rec.file_len, parts: vec![rec.part_no] });
        }
    }
    for h in gen.runs() {
        let parts: Vec<u32> = h.run.live().collect();
        if parts.is_empty() {
            continue;
        }
        let rows = parts.iter().filter_map(|no| by_no.get(no)).map(|v| v.live).sum();
        let bytes = h.slots.iter().flatten().map(|&i| gen.manifest.parts[i].file_len).sum();
        units.push(RunUnit { level: h.run.level, order: h.run.run_id, rows, bytes, parts });
    }
    units
}

fn run_pass_job(slot: pgrc_am::catalog::RelSlot, dir: &str, gen: &pgrc_am::catalog::Generation) -> Option<Vec<u32>> {
    let v = views(slot, dir, gen)?;
    next_run_job(&run_units(gen, &v, &base_policy()), &pgrc_am::runmerge::run_policy())
}

fn head_run_job(slot: pgrc_am::catalog::RelSlot) -> Option<Vec<u32>> {
    let (Some((gens, head)), Some(dir)) = (pgrc_am::catalog::generations(slot), pgrc_am::catalog::dir_of(slot)) else { return None };
    gens.iter().find(|g| g.gen == head).and_then(|g| run_pass_job(slot, &dir, g))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPassOut {
    pub inputs: u32,
    pub out: pgrc_am::runmerge::RunOut,
}

pub fn run_pass<'mcx>(_mcx: Mcx<'mcx>, rel: &Relation<'mcx>, waiters: bool) -> PgResult<Option<RunPassOut>> {
    if !run_pass_on(rel) || pgrc_am::ingest::compaction_blocked(rel)? || !yield_lock(rel)? {
        return Ok(None);
    }
    let Some(gen) = ring::newest(rel)? else { return Ok(None) };
    let l = rel.rd_locator.get();
    let dir = pgrc_am::dirpath::table_dir_path(l, rel.rd_backend);
    let Some(parts) = run_pass_job((l.spcOid, l.dbOid, l.relNumber as u64), &dir, &gen) else { return Ok(None) };
    let pool = pgrc_front_seams::seal_pool::is_installed().then(pgrc_front_seams::seal_pool::call).flatten();
    let pause = Duration::from_millis(crate::maint::setting(PAUSE_GUC).unwrap_or(0));
    let yields = waiters && !yielded_to_flush(rel.rd_id);
    let live_in = live_bytes_of(rel, &gen, &parts);
    let clock = JobClock::start();
    let start = pg_clock::MonoStamp::now();
    let out = pgrc_am::runmerge::run_merge(rel, &gen, &parts, pool, &|first| between_granules(rel, if first { pause } else { Duration::ZERO }, waiters, yields.then_some(start)))?;
    if waiters && !yields {
        pgsync::lock(&YIELDED).retain(|&r| r != rel.rd_id);
    }
    clock.record(rel, JobKind::Run, if waiters { Reason::Tiered } else { Reason::Forced }, run_cost(parts.len(), live_in, &out));
    log_run(rel, "run pass", parts.len(), &out);
    Ok(Some(RunPassOut { inputs: parts.len() as u32, out }))
}

struct JobClock(u64, u64, u64);

impl JobClock {
    fn start() -> JobClock {
        JobClock(crate::ledger::thread_cpu_us(), crate::ledger::thread_wal_bytes(), pgrc_am::scanstats::thread_publish_us())
    }

    fn record(&self, rel: &Relation<'_>, kind: JobKind, reason: Reason, cost: crate::ledger::Cost) {
        let cost = crate::ledger::Cost {
            thread_cpu_us: crate::ledger::thread_cpu_us().saturating_sub(self.0),
            wal_bytes: crate::ledger::thread_wal_bytes().wrapping_sub(self.1),
            publish_us: pgrc_am::scanstats::thread_publish_us() - self.2,
            ..cost
        };
        crate::ledger::record(rel.rd_locator.get().dbOid, rel.rd_id, kind, reason, &cost);
    }
}

fn run_cost(inputs: usize, live_in: u64, out: &pgrc_am::runmerge::RunOut) -> crate::ledger::Cost {
    crate::ledger::Cost { inputs: inputs as u64, live_in, bytes_in: out.input_bytes, bytes_out: out.output_bytes, rows: out.rows, dropped: out.dropped, ..crate::ledger::Cost::default() }
}

fn live_bytes_of(rel: &Relation<'_>, gen: &pgrc_am::catalog::Generation, parts: &[u32]) -> u64 {
    let l = rel.rd_locator.get();
    views((l.spcOid, l.dbOid, l.relNumber as u64), &pgrc_am::dirpath::table_dir_path(l, rel.rd_backend), gen).map_or(0, |v| v.iter().filter(|x| parts.contains(&x.part_no)).map(|x| x.live_bytes()).sum())
}


fn pinned_views(gen: &pgrc_am::catalog::Generation, views: &[PartView]) -> Vec<PartView> {
    let pinned: Vec<u32> = (0..gen.manifest.parts.len()).filter(|&i| gen.pinned_member(i)).map(|i| gen.manifest.parts[i].part_no).collect();
    views.iter().filter(|v| pinned.contains(&v.part_no)).copied().collect()
}

fn member_job(slot: pgrc_am::catalog::RelSlot, dir: &str, gen: &pgrc_am::catalog::Generation, p: &Policy) -> Option<MemberJob> {
    if gen.runs().is_empty() {
        return None;
    }
    let v = views(slot, dir, gen)?;
    next_member_job(&pinned_views(gen, &v), p)
}

fn head_member_job(slot: pgrc_am::catalog::RelSlot) -> Option<MemberJob> {
    let (gens, head) = pgrc_am::catalog::generations(slot)?;
    let g = gens.iter().find(|g| g.gen == head)?;
    if g.runs().is_empty() {
        return None;
    }
    member_job(slot, &pgrc_am::catalog::dir_of(slot)?, g, &base_policy())
}

pub fn member_pass<'mcx>(_mcx: Mcx<'mcx>, rel: &Relation<'mcx>, waiters: bool) -> PgResult<Option<pgrc_am::runmerge::MemberOut>> {
    member_pass_by(rel, waiters, false)
}

pub fn member_pass_forced<'mcx>(_mcx: Mcx<'mcx>, rel: &Relation<'mcx>) -> PgResult<Option<pgrc_am::runmerge::MemberOut>> {
    member_pass_by(rel, false, true)
}

fn member_pass_by(rel: &Relation<'_>, waiters: bool, forced: bool) -> PgResult<Option<pgrc_am::runmerge::MemberOut>> {
    let Some(gen) = ring::newest(rel)? else { return Ok(None) };
    if gen.runs().is_empty() {
        return Ok(None);
    }
    if pgrc_am::ingest::compaction_blocked(rel)? || !yield_lock(rel)? {
        return Ok(None);
    }
    let l = rel.rd_locator.get();
    let dir = pgrc_am::dirpath::table_dir_path(l, rel.rd_backend);
    let p = match forced {
        true => Policy { dv_rewrite: f64::MIN_POSITIVE, ..base_policy() },
        false => base_policy(),
    };
    let Some(job) = member_job((l.spcOid, l.dbOid, l.relNumber as u64), &dir, &gen, &p) else { return Ok(None) };
    let clock = JobClock::start();
    let (out, reason, live_in) = match job {
        MemberJob::Retire(parts) => (pgrc_am::runmerge::retire_members(rel, &gen, &parts)?, Reason::Retire, 0),
        MemberJob::Swap(no) => {
            let live_in = live_bytes_of(rel, &gen, &[no]);
            let pool = pgrc_front_seams::seal_pool::is_installed().then(pgrc_front_seams::seal_pool::call).flatten();
            let pause = Duration::from_millis(crate::maint::setting(PAUSE_GUC).unwrap_or(0));
            let yields = waiters && !yielded_to_flush(rel.rd_id);
            let start = pg_clock::MonoStamp::now();
            let out = pgrc_am::runmerge::rewrite_member(rel, &gen, no, pool, &|first| between_granules(rel, if first { pause } else { Duration::ZERO }, waiters, yields.then_some(start)))?;
            if waiters && !yields {
                pgsync::lock(&YIELDED).retain(|&r| r != rel.rd_id);
            }
            (out, if forced { Reason::Forced } else { Reason::DvFloor }, live_in)
        }
    };
    clock.record(rel, JobKind::DvRewrite, reason, member_cost(live_in, &out));
    log_member(rel, &out);
    Ok(Some(out))
}

fn member_cost(live_in: u64, out: &pgrc_am::runmerge::MemberOut) -> crate::ledger::Cost {
    crate::ledger::Cost { inputs: out.retired.len() as u64, live_in, bytes_in: out.input_bytes, bytes_out: out.output_bytes, rows: out.rows, dropped: out.dropped, ..crate::ledger::Cost::default() }
}

fn log_member(rel: &Relation<'_>, out: &pgrc_am::runmerge::MemberOut) {
    let what = match out.to {
        Some(to) => format!("member swap of \"{}\": part {} into part {to}", rel.name(), out.retired.first().copied().unwrap_or(0)),
        None => format!("member retire of \"{}\": parts {:?}", rel.name(), out.retired),
    };
    let _ = elog::elog(types_error::LOG, format!("pgrcolumnar: {what}: {} rows ({} dropped)", out.rows, out.dropped)); // alloc: Cold
}

fn log_run(rel: &Relation<'_>, what: &str, inputs: usize, out: &pgrc_am::runmerge::RunOut) {
    let rev = match out.stats {
        Some(s) => format!("reverse {} {} {} bytes ({} frames, {} groups; spilled {} bytes)", out.rev.name(), out.rank.name(), out.rev_bytes, s.frames, s.groups, s.spilled),
        None => "no reverse".to_string(),
    };
    let _ = elog::elog(
        types_error::LOG,
        format!(
            "pgrcolumnar: {what} of \"{}\": {} rows ({} dropped) from {inputs} inputs into {} members, run {} level {}, {rev}",
            rel.name(),
            out.rows,
            out.dropped,
            out.members.len(),
            out.run_id,
            out.level
        ),
    ); // alloc: Cold
}

fn run_columns(rel: &Relation<'_>, gen: Option<&pgrc_am::catalog::Generation>, due: Option<Vec<u32>>, member: Option<MemberJob>) -> PgResult<String> {
    let runs: Vec<&pgrc_am::catalog::RunHome> = gen.map(|g| g.runs().iter().filter(|h| h.run.homes()).collect()).unwrap_or_default();
    let mut levels: std::collections::BTreeMap<u8, u32> = std::collections::BTreeMap::new();
    for h in &runs {
        *levels.entry(h.run.level).or_default() += 1;
    }
    let mut level_strs: Vec<String> = ::mcx::stdvec::try_vec(levels.len(), "pgrc compaction stats")?;
    level_strs.extend(levels.iter().map(|(l, n)| format!("{l}:{n}")));
    let levels = level_strs.join(",");
    let due = if run_pass_on(rel) { due.map_or(0, |d| d.len()) } else { 0 };
    let self_runs = gen.map_or(0, |g| g.runs().iter().filter(|h| !h.run.homes()).count());
    let member = match member {
        Some(MemberJob::Retire(v)) => v.len(),
        Some(MemberJob::Swap(_)) => 1,
        None => 0,
    };
    Ok(format!("runs={} run_members={} run_levels={levels} run_due={due} self_runs={self_runs} member_due={member}", runs.len(), runs.iter().map(|h| h.run.live().count()).sum::<usize>()))
}

fn part_bytes(slot: pgrc_am::catalog::RelSlot, dir: &str, gen: &pgrc_am::catalog::Generation) -> Option<Vec<u64>> {
    use pgrc_write::facts::{cut_bytes, part_columns, part_heap_stats};
    let key = |r: &pgrc_format::manifest::PartRecord| (slot, r.part_no, r.file_len);
    let mut out: Vec<Option<u64>> = {
        let m = pgsync::lock(&PART_BYTES);
        gen.manifest.parts.iter().map(|r| m.get(&key(r)).copied()).collect()
    };
    let missing: Vec<usize> = (0..out.len()).filter(|&i| out[i].is_none()).collect();
    if missing.is_empty() {
        return out.into_iter().collect();
    }
    let cols = part_columns(&gen.parts[missing[0]].part).ok()?;
    let heap: Vec<usize> = (0..cols.len()).filter(|&k| cols[k].heap()).collect();
    let attnos: Vec<u32> = heap.iter().map(|&k| cols[k].attno).collect();
    let fp = gen.manifest.header.schema_fingerprint;
    let plane = if missing.len() > 1 && !attnos.is_empty() { pgrc_am::analyze::plane_records_here(dir, &gen.manifest, fp, &attnos) } else { None };
    for &i in &missing {
        let rec = &gen.manifest.parts[i];
        let from_plane = plane.as_ref().and_then(|pl| {
            let mut c = cols.clone();
            for (j, &k) in heap.iter().enumerate() {
                c[k].stats = Some(pl[j][i]?).filter(|r| r.flags & pgrc_format::meta::STATSF_COMPUTED != 0);
            }
            Some(c)
        });
        let c = match from_plane {
            Some(c) => c,
            None => part_heap_stats(&gen.parts[i].part).ok()?,
        };
        out[i] = Some(cut_bytes(rec.rows, &c));
    }
    let mut nos: Vec<u32> = gen.manifest.parts.iter().map(|r| r.part_no).collect();
    nos.sort_unstable();
    if !PART_BYTES_LISTENING.load(pgsync::atomic::Ordering::Acquire) && !PART_BYTES_LISTENING.swap(true, pgsync::atomic::Ordering::AcqRel) {
        pgrc_am::catalog::on_slot_event(part_bytes_event);
    }
    let mut m = pgsync::lock(&PART_BYTES);
    m.retain(|(s, no, _), _| *s != slot || nos.binary_search(no).is_ok());
    m.extend(missing.iter().filter_map(|&i| Some((key(&gen.manifest.parts[i]), out[i]?))));
    out.into_iter().collect()
}

/// D4742: a slot's sizes go when the catalog removes it (DROP, TRUNCATE, a rewrite, DROP DATABASE) or makes it anew.
fn part_bytes_event(ev: &pgrc_am::catalog::SlotEvent<'_>) {
    if let pgrc_am::catalog::SlotEvent::Removed(slot, _) | pgrc_am::catalog::SlotEvent::Created(slot, _) = *ev {
        pgsync::lock(&PART_BYTES).retain(|(s, _, _), _| *s != slot);
    }
}

fn views(slot: pgrc_am::catalog::RelSlot, dir: &str, gen: &pgrc_am::catalog::Generation) -> Option<Vec<PartView>> {
    let bytes = part_bytes(slot, dir, gen)?;
    let now = pg_clock::wall_ns();
    let age = |no: u32| {
        let mut info = vfs::FileInfo::zeroed();
        let path = std::ffi::CString::new(format!("{dir}/{}", pgrc_format::dirlayout::part_file_name(no)));
        if !path.is_ok_and(|p| vfs::stat(&p, &mut info) == 0) {
            return 0;
        }
        u64::try_from(now - info.mtime_sec.saturating_mul(1_000_000_000).saturating_add(info.mtime_nsec)).map_or(0, |d| d / 1_000_000_000)
    };
    Some(gen.manifest.parts.iter().zip(&gen.parts).zip(bytes).map(|((rec, p), bytes)| PartView { age_s: age(rec.part_no), bytes, ..PartView::from_record(rec, p.deletes.as_ref().map_or(0, |d| d.deleted_rows)) }).collect())
}

fn target_rows(views: &[PartView], p: &Policy) -> u64 {
    let (rows, bytes) = views.iter().fold((0u128, 0u128), |(r, b), v| (r + u128::from(v.rows), b + u128::from(v.bytes)));
    if rows == 0 { 0 } else if bytes == 0 { p.max_rows } else { u64::try_from(rows * u128::from(p.max_bytes) / bytes).unwrap_or(u64::MAX).min(p.max_rows) }
}

pub fn stats(rel: &Relation<'_>) -> PgResult<String> {
    let l = rel.rd_locator.get();
    let slot = (l.spcOid, l.dbOid, l.relNumber as u64);
    pgrc_am::catalog::drain_deferred_now()?;
    let held = pgrc_am::catalog::generations(slot).map_or(0, |(g, _)| g.len());
    let deferred = pgrc_am::catalog::deferred_count(slot);
    let pubs = format!("{} {}", pgrc_write::publish::counters::line(), pgrc_am::holds::line());
    let rot = crate::maint::rotate_blocks(rel.rd_id);
    let (paced_ms, bp_waits) = crate::maint::pace_stats(rel.rd_id);
    let (ar, sr) = (crate::maint::active_rows(rel.rd_id), crate::maint::sealing_rows(rel.rd_id));
    let rot = format!("{rot} paced_ms={paced_ms} bp_waits={bp_waits} bp_ms={} bp_checks={} l0_waits={l0_waits} active_rows={ar} sealing_rows={sr} target_rows={} rows_cap={} rotating={} stall_rot={}", crate::maint::bp_ms(rel.rd_id), crate::maint::bp_checks(rel.rd_id), crate::maint::rows_target(rel.rd_id), crate::maint::rows_cap(rel.rd_id), u8::from(crate::maint::rotating(rel.rd_id)), crate::maint::stall_rotations(rel.rd_id), l0_waits = crate::maint::l0_waits(rel.rd_id));
    let l0c = pgrc_am::catalog::generations(slot).and_then(|(g, h)| g.into_iter().find(|x| x.gen == h)).and_then(|g| pgrc_am::l0::of(slot, &g)).map_or_else(|| "-".to_string(), |n| n.to_string());
    let led = crate::ledger::text(l.dbOid, rel.rd_id);
    let Some(gen) = ring::newest(rel)? else { return Ok(format!("gen=0 parts=0 l0=0 l1=0 full=0 l0c={l0c} target=0 due=0 rows=0 live=0 deleted=0 gens={held} deferred={deferred} files=0 nos= rotate={rot} {pubs} {led} {}", run_columns(rel, None, None, None)?)) };
    let p = base_policy();
    let dir = pgrc_am::dirpath::table_dir_path(l, rel.rd_backend);
    let (views, due) = match views(slot, &dir, &gen) {
        Some(v) => {
            let due = next_job(&v, false, &p).map_or(0, |j| j.inputs.len());
            (v, due)
        }
        None => (gen.manifest.parts.iter().zip(&gen.parts).map(|(rec, x)| PartView { bytes: 0, ..PartView::from_record(rec, x.deletes.as_ref().map_or(0, |d| d.deleted_rows)) }).collect(), 0),
    };
    let l1 = views.iter().filter(|v| v.compacted).count();
    let full = views.iter().filter(|v| v.full(&p)).count();
    let l0 = views.iter().filter(|v| !v.compacted && !v.full(&p)).count();
    let target = target_rows(&views, &p);
    let (rows, live) = (views.iter().map(|v| v.rows).sum::<u64>(), views.iter().map(|v| v.live).sum::<u64>());
    let runs = run_columns(rel, Some(&*gen), run_pass_on(rel).then(|| run_pass_job(slot, &dir, &gen)).flatten(), member_job(slot, &dir, &gen, &p))?;
    let files = std::ffi::CString::new(dir).map_or(0, |d| vfs::read_dir(&d).map_or(0, |d| d.filter(|n| n.as_ref().is_ok_and(|n| n.as_str().is_some_and(|n| matches!(pgrc_format::dirlayout::classify(n), Some(pgrc_format::dirlayout::FileClass::Part(_)))))).count()));
    let mut nos: Vec<String> = ::mcx::stdvec::try_vec(views.len(), "pgrc compaction stats")?;
    nos.extend(views.iter().map(|v| v.part_no.to_string()));
    let nos = nos.join(",");
    Ok(format!("gen={} parts={} l0={l0} l1={l1} full={full} l0c={l0c} target={target} due={due} rows={rows} live={live} deleted={} gens={held} deferred={deferred} files={files} nos={nos} rotate={rot} {pubs} {led} {runs}", gen.gen, views.len(), rows - live))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgrc_compact::policy::JobKind;

    fn part(part_no: u32, rows: u64, live: u64) -> PartView {
        PartView { part_no, rows, live, bytes: rows * 40, compacted: false, sorted: false, age_s: 0 }
    }

    #[test]
    fn a_removed_or_new_slot_takes_its_part_bytes() {
        let (a, b) = ((1663, 4_000_042, 7), (1663, 4_000_043, 7));
        pgsync::lock(&PART_BYTES).extend([((a, 0, 10), 1), ((a, 1, 10), 1), ((b, 0, 10), 1)]);
        part_bytes_event(&pgrc_am::catalog::SlotEvent::Removed(a, "/nowhere"));
        assert!(pgsync::lock(&PART_BYTES).keys().all(|k| k.0 != a));
        assert!(pgsync::lock(&PART_BYTES).contains_key(&(b, 0, 10)));
        part_bytes_event(&pgrc_am::catalog::SlotEvent::Created(b, "/nowhere"));
        assert!(pgsync::lock(&PART_BYTES).keys().all(|k| k.0 != b));
    }

    #[test]
    fn forced_jobs_take_any_run_or_any_deleted_part() {
        let parts = [part(0, 1000, 1000), part(1, 1000, 990), part(2, 1 << 20, 1 << 20)];
        let inputs = |j: Option<Job>| j.map(|j| (j.kind, j.inputs));
        assert_eq!(inputs(pick(&parts, false, None)), None, "a run of two is under the fan-in");
        assert_eq!(inputs(pick(&parts, false, Some(Force::Concat))), Some((JobKind::Concat, vec![0, 1])));
        assert_eq!(inputs(pick(&parts, false, Some(Force::Sorted))), Some((JobKind::SortedMerge, vec![0, 1])));
        assert_eq!(inputs(pick(&parts, false, Some(Force::Dv))), Some((JobKind::DvRewrite, vec![1])));
        assert_eq!(inputs(pick(&[part(0, 1 << 20, 1 << 20)], false, Some(Force::Dv))), None, "nothing deleted");
        assert_eq!(inputs(pick(&parts, false, Some(Force::Full))), Some((JobKind::Concat, vec![0, 1])), "a run of two merges");
        assert_eq!(inputs(pick(&parts, true, Some(Force::Full))), Some((JobKind::SortedMerge, vec![0, 1])));
        assert_eq!(inputs(pick(&[part(0, 1000, 1000)], false, Some(Force::Full))), None, "one small part alone stays");
        assert_eq!(inputs(pick(&[part(0, 1 << 20, (1 << 20) - 1)], false, Some(Force::Full))), Some((JobKind::DvRewrite, vec![0])), "one deleted row");
        assert_eq!(inputs(pick(&[part(0, 5000, 4500), part(1, 2, 2)], false, Some(Force::Full))), Some((JobKind::Concat, vec![0, 1])), "a tiny neighbour joins");
    }

    #[test]
    fn tiny_parts_by_job_and_turn() {
        let tiny: Vec<PartView> = (0..300).map(|i| part(i, 10, 10)).collect();
        let n = |j: Option<Job>| j.map(|j| j.inputs.len());
        assert_eq!(n(pick(&tiny, false, None)), Some(256));
        for f in [Force::Concat, Force::Sorted, Force::Full] {
            assert_eq!(n(pick(&tiny, false, Some(f))), Some(16), "{f:?}");
        }
        let cont = Policy::default().continuing();
        assert_eq!(n(pick_by(&tiny[..31], false, None, cont)), None, "31 tiny parts wait");
        assert_eq!(n(pick_by(&tiny[..32], false, None, cont)), Some(32));
        assert_eq!(n(pick_by(&tiny[..31], false, None, Policy::default())), Some(31), "a call's first job: the fan-in");
        let seals: Vec<PartView> = (0..5).map(|i| part(i, 60_000, 60_000)).collect();
        assert_eq!(n(pick_by(&seals, false, None, cont)), Some(5), "not tiny: the fan-in");
    }

    #[test]
    fn the_full_loop_ends_with_every_deleted_row_gone() {
        let p = Policy::default();
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = |m: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % m.max(1)
        };
        for round in 0..3000 {
            let n = next(40) as usize;
            let mut parts: Vec<PartView> = (0..n)
                .map(|i| {
                    let rows = match next(3) {
                        0 => p.max_rows,
                        1 => 1 + next(p.max_rows / 3),
                        _ => 1 + next(5000),
                    };
                    let live = match next(3) {
                        0 => next(rows + 1),
                        _ => rows,
                    };
                    PartView { part_no: i as u32, rows, live, bytes: rows * 40, compacted: false, sorted: false, age_s: 0 }
                })
                .collect();
            let live0: u64 = parts.iter().map(|v| v.live).sum();
            let (mut jobs, mut written, mut next_no) = (0, 0u64, n as u32);
            while let Some(job) = pick(&parts, round % 2 == 0, Some(Force::Full)) {
                jobs += 1;
                assert!(jobs <= 2 * n + 1, "round {round}: no end after {jobs} jobs over {n} parts");
                let live: u64 = parts.iter().filter(|v| job.inputs.contains(&v.part_no)).map(|v| v.live).sum();
                parts.retain(|v| !job.inputs.contains(&v.part_no));
                written += live;
                if live > 0 {
                    parts.push(PartView { part_no: next_no, rows: live, live, bytes: live * 40, compacted: true, sorted: false, age_s: 0 });
                    next_no += 1;
                }
            }
            assert!(parts.iter().all(|v| v.live == v.rows && v.live > 0), "round {round}: {parts:?}");
            assert_eq!(parts.iter().map(|v| v.live).sum::<u64>(), live0);
            assert!(written <= 8 * live0 + 1, "round {round}: {written} rows written for {live0} live");
        }
    }

    #[test]
    fn the_target_at_a_tables_width() {
        use pgrc_format::class::{CLASS_BYVAL, CLASS_FIXED, CLASS_VARLENA};
        use pgrc_write::facts::{cut_bytes, ColSize};
        let p = Policy::default();
        let col = |attno, class, fixed_len, byte_len_sum, nonnull: u64| {
            let mut r = pgrc_format::meta::StatsRecord::absent();
            (r.byte_len_sum, r.nonnull) = (byte_len_sum, nonnull as u32);
            ColSize { attno, class, fixed_len, stats: (class != CLASS_BYVAL).then_some(r) }
        };
        let hits = |rows: u64| (1..=105).map(|a| if a <= 77 { col(a, CLASS_BYVAL, 0, 0, rows) } else { col(a, CLASS_VARLENA, 0, 11 * rows, rows) }).collect::<Vec<_>>();
        assert_eq!(cut_bytes(1000, &hits(1000)), 840_000 + 16 * 8 * 105 + 28 * (11_000 + 7_500));
        assert!(cut_bytes(188_416, &hits(188_416)) < p.max_bytes && cut_bytes(196_608, &hits(196_608)) >= p.max_bytes);
        let view = |rows| PartView { part_no: 0, rows, live: rows, bytes: cut_bytes(rows, &hits(rows)), compacted: false, sorted: false, age_s: 0 };
        assert!(view(196_608).full(&p) && !view(188_416).full(&p));
        let t = target_rows(&[view(196_608), view(1000)], &p);
        assert!((195_000..196_500).contains(&t), "{t}");
        assert_eq!(cut_bytes(64, &[col(1, CLASS_VARLENA, 0, 0, 0), col(2, CLASS_FIXED, 12, 0, 64)]), 2 * (64 * 8 + 8) + 64 * 16);
        assert_eq!(target_rows(&[PartView { bytes: cut_bytes(1000, &[col(1, CLASS_BYVAL, 0, 0, 1000)]), ..view(1000) }], &p), p.max_rows);
        assert_eq!(target_rows(&[], &p), 0);
    }

    #[test]
    fn sortruns_teeth_vacuum_full_rewrites_no_run_member_as_idless() {
        use pgrc_format::manifest::{with_next_row_id, with_ranges, Manifest, ManifestHeader, PartRecord};
        use pgrc_format::sortrun::{with_runs, RankForm, RunRevForm, SortRun};
        let rec = |part_no| PartRecord { rows: 5, file_len: 64, footer_off: 0, dv_gen: 0, dv_len: 0, part_no, flags: 0, granule_count: 1, band_count: 1, dv_crc: 0, granule_rows: 0 };
        let header = ManifestHeader { gen: 6, prev_gen: 5, publisher_fxid: 1, relfilenumber: 9, schema_fingerprint: 1, magic: 0, format_version: 0, spc: 1663, db: 5, part_count: 3, next_part_no: 9, flags: 0, reserved: 0 };
        let run = SortRun { run_id: 6, level: 1, rev: RunRevForm::FrameIx, rank: RankForm::Dense, rows: 10, rev_len: 4096, rev_crc: 0, members: vec![3, 4], ranges: vec![(291, 582)] };
        let ext = with_runs(&with_ranges(&with_next_row_id(&[], 291 * 10).unwrap(), &[(2, vec![(582, 873)])]).unwrap(), &[run]).unwrap();
        let gen = pgrc_am::catalog::Generation::detached(Manifest::new(header, vec![rec(2), rec(3), rec(4)]).with_ext(ext.clone())).unwrap();
        assert_eq!(idless_job(&gen).map(|j| j.inputs), None, "members 3 and 4 have the run's IDs");
        let with_old = pgrc_am::catalog::Generation::detached(Manifest::new(ManifestHeader { part_count: 4, ..header }, vec![rec(1), rec(2), rec(3), rec(4)]).with_ext(ext)).unwrap();
        assert_eq!(idless_job(&with_old).map(|j| j.inputs), Some(vec![1]), "a part written before row IDs");
        let views: Vec<PartView> = [1, 2, 3, 4].map(|n| PartView { part_no: n, ..part(n, 5, 5) }).to_vec();
        assert_eq!(without_members(&with_old, views).iter().map(|v| v.part_no).collect::<Vec<_>>(), [1, 2], "no L0 job takes a member of a run with others");
    }
}
