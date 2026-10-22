//! Gravity sections: per-worker Bindings over a shared Node, morsel claims, merge, cancel (docs/gravity/parallel.md).

pub mod barrier;
pub mod cancel;
pub mod claim;
pub mod merge;
/// Test support: FixturePool, a thread pool for crate tests and examples (never a server pool; the server's is gravity_pool).
/// Compiled only under the `fixture-pool` feature, which only dev-dependencies enable (gravity_lint's fixture_pool test).
#[cfg(feature = "fixture-pool")]
pub mod test;
pub mod section;
pub mod worker;

use std::sync::atomic::Ordering;

use gravity_driver::{Binding, Call, Derive, Entry, Error, Exec, Merge, Result, Step};
use gravity_program::cost::runtime::MAX_POOLS;

pub use barrier::Barrier;
pub use cancel::{ErrorFunnel, FirstError};
pub use claim::{BlockRange, ClaimLedger, ClaimSize, GranuleRange, MorselSource, PartitionRange, RangeCursor, Unit, startup_c0};
pub use gravity_abi::MorselRange;
pub use merge::{kway_merge, order_topk, HashAggPartial, HashBuildPartial, PlainAggPartial, RowsOrdered, RowsUnordered, SortedRun, TopKPartial, TOPK_TIES};
#[cfg(feature = "fixture-pool")]
pub use test::fixture_pool::FixturePool;
pub use section::Section;
pub use worker::Worker;

pub type Job<'a> = dyn Fn(u16, MorselRange) -> Result<()> + Sync + 'a;

pub trait Pool {
    fn run(&self, width: u16, claims: &dyn MorselSource, job: &Job<'_>) -> core::result::Result<(), FirstError>;
    /// `run` with the leader working while it waits (R1D's ordered output): `lead` on every wake, `order.wake` armed with
    /// the leader's word so a worker can wake it. A claim runs `job` only once `order` admitted it, and every admitted
    /// claim settles with `order.finish` (ok = the job's; false when it never ran). A pool with slots admits BEFORE the
    /// claim takes one (D278: a claim waiting for room in the window never holds a slot the head needs). The default
    /// admits inside the job, runs the section, then leads once.
    fn run_leading(&self, width: u16, claims: &dyn MorselSource, job: &Job<'_>, lead: &mut dyn FnMut(), order: &gravity_driver::ClaimOrder) -> core::result::Result<(), FirstError> {
        let admitted = |ordinal: u16, claim: MorselRange| {
            if !order.admit(claim) {
                return Ok(());
            }
            let r = job(ordinal, claim);
            order.finish(claim, r.is_ok());
            r
        };
        let r = self.run(width, claims, &admitted);
        lead();
        r
    }
    /// Claims run by the pool so far (every section, every leader); the runner reports the delta over a statement.
    fn claims_run(&self) -> u64 {
        0
    }
    /// Sections run so far (the runner tells the first one: the cold section).
    fn sections_run(&self) -> u64 {
        0
    }
    /// A fire-and-forget job on one pool thread (the residency prime); false when the pool has none.
    fn spawn(&self, _job: Box<dyn FnOnce() + Send>) -> bool {
        false
    }
    /// The pool's worker threads: the widest section it can run one worker per thread (0 = no bound declared).
    fn threads(&self) -> u16 {
        0
    }
    /// `(first arm, first section)` in microseconds: what the process's first serve paid.
    fn cold_micros(&self) -> (u64, u64) {
        (0, 0)
    }
}

static INSTALLED: pgsync::OnceLock<Vec<&'static (dyn Pool + Sync)>> = pgsync::OnceLock::new();

/// B2 (workersess): what a section's program needs armed on the leader while its claims are submitted (the statement's
/// catalog images for its hosted scalar sites, gravity_fn pool_catalog): built by the installed hook (exec), held until
/// the section ends. None installed (a test pool) or nothing to arm: no guard.
pub type SectionArm = fn(&gravity_program::Program) -> Option<Box<dyn core::any::Any>>;
static SECTION_ARM: pgsync::OnceLock<SectionArm> = pgsync::OnceLock::new();

/// B2: install the section arm (exec's `init_seams`, once).
pub fn install_section_arm(f: SectionArm) {
    let _ = SECTION_ARM.set(f);
}

fn section_arm(p: &gravity_program::Program) -> Option<Box<dyn core::any::Any>> {
    SECTION_ARM.get().and_then(|f| f(p))
}

/// Statements in flight per pool (`per-node`): a statement lands WHOLE on the pool with the fewest.
static INFLIGHT: [std::sync::atomic::AtomicU64; MAX_POOLS] = [const { std::sync::atomic::AtomicU64::new(0) }; MAX_POOLS];

leader_tls::leader_tls! {
    thread_local! {
        /// The leader's pool for the statement it is running (`place` .. `Placed::drop`); 0 outside one.
        static CURRENT: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
    }
}

/// One pool per NUMA node (runtime cpuset `per-node`), pool 0 first; at most `MAX_POOLS`.
pub fn install_pools(pools: Vec<&'static (dyn Pool + Sync)>) -> bool {
    !pools.is_empty() && pools.len() <= MAX_POOLS && INSTALLED.set(pools).is_ok()
}

/// The calling statement's pool: the one `place` chose on this thread, else pool 0.
pub fn installed_pool() -> Option<&'static (dyn Pool + Sync)> {
    let pools = INSTALLED.get()?;
    Some(pools[CURRENT.with(|c| c.get()).min(pools.len() - 1)])
}

pub fn pool_count() -> usize {
    INSTALLED.get().map_or(0, |p| p.len())
}

/// Place the calling statement (fewest in flight, ties → the lowest index) until the `Placed` drops.
pub fn place() -> Placed {
    let n = pool_count();
    let mut idx = 0;
    if n > 1 {
        let mut best = u64::MAX;
        for (k, c) in INFLIGHT.iter().enumerate().take(n) {
            let v = c.load(std::sync::atomic::Ordering::Relaxed);
            if v < best {
                best = v;
                idx = k;
            }
        }
        INFLIGHT[idx].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    let prev = CURRENT.with(|c| c.replace(idx));
    Placed { idx, counted: n > 1, prev }
}

/// A statement's placement: `idx` its pool (the NUMA node's index), `prev` the enclosing statement's, restored at the drop.
pub struct Placed {
    pub idx: usize,
    counted: bool,
    prev: usize,
}

impl Drop for Placed {
    fn drop(&mut self) {
        if self.counted {
            INFLIGHT[self.idx].fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
        CURRENT.with(|c| c.set(self.prev));
    }
}

pub fn placed() -> usize {
    CURRENT.with(|c| c.get())
}

pub fn inflight() -> Vec<u64> {
    (0..pool_count()).map(|k| INFLIGHT[k].load(std::sync::atomic::Ordering::Relaxed)).collect() // alloc: Cold
}


/// `PGRUST_GRAVITY_PHASES=1`: one stderr line per section with the wall clock of its phases (diagnostic; read once).
fn phases_on() -> bool {
    static ON: pgsync::OnceLock<bool> = pgsync::OnceLock::new();
    *ON.get_or_init(|| gravity_driver::env::set("PGRUST_GRAVITY_PHASES"))
}

/// The owner phase's census under the same switch: nanoseconds per partition in the table's
/// zeroing, the fold and the kernel's reduce, summed over the owners; the partitions folded; the
/// records the owners read (the phase-1 output: runs, not rows, under the collapse).
pub static OWN_CENSUS: [std::sync::atomic::AtomicU64; 5] = [const { std::sync::atomic::AtomicU64::new(0) }; 5];

pub fn census_on() -> bool {
    phases_on()
}

pub static OWN_OVERFLOW: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// `PGRUST_GRAVITY_OWNER_GROUPS_CAP=n`: every owner partition's first table sized for at most `n` groups (0 / unset: the estimate) — the fill → refold path forced (the e2e's witness; read once).
pub fn owner_groups_cap() -> u64 {
    static CAP: pgsync::OnceLock<u64> = pgsync::OnceLock::new();
    *CAP.get_or_init(|| gravity_driver::env::num("PGRUST_GRAVITY_OWNER_GROUPS_CAP").unwrap_or(0))
}

/// The owner partition tables that filled at the group estimate and were refolded at the records' size (the rig's witness).
pub static TABLE_RETRY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn census_take(c: &[std::sync::atomic::AtomicU64; 5]) -> [u64; 5] {
    core::array::from_fn(|i| c[i].swap(0, Ordering::Relaxed))
}

/// The held records at or below which the leader folds the owner phase itself instead of waking the pool.
/// MEASURED-HELDOUT (NEW-constants-1846, held-out group corpus, 87 statements, W8): 256..4096 within 3% (128.4..132.5 s, one take
/// beside other box work, noise level); the shipped 1024 stays (sweep file shapecheck/sweep-NEW-constants-1846.tsv).
pub const LEADER_OWNS_RECORDS: u64 = 1024;

/// EX-13: the leader folds the owner phase itself while the held records' bytes fit half its core's L1d (the machine
/// profile's `l1d_bytes`): a fold that never leaves the L1 costs less than a pool round (wake, lease, return). On the
/// reference core (64 KiB) that is `LEADER_OWNS_RECORDS` records of the common 32-byte record, as measured; a wider
/// record takes fewer, a larger L1 more.
pub fn leader_owns_bytes() -> u64 {
    gravity_driver::machine::profile().l1d_bytes / 2
}

/// D2877: the empty claim, a range no source holds a unit of (a heap or pgrc range site clamps it to none).
pub const FIRST_CALL: MorselRange = MorselRange::new(0, 0);

/// D2877: the W1 producer arm's next claim: `claims`' next, or, when none has reached the pipeline yet and its kernel makes
/// a check PostgreSQL makes at its node's first call (`first`: `Pipeline::first_call`), `FIRST_CALL` once, so the check
/// runs (PG's Result evaluates its one-time qual before it pulls, whatever its outer holds) and the source reads nothing.
pub fn next_claim(claims: &dyn MorselSource, first: &mut bool) -> Option<MorselRange> {
    let c = claims.claim().or_else(|| first.then_some(FIRST_CALL));
    *first = false;
    c
}

/// One claim of a section (the zone walk's bound first): the job every section runs, ordered or not.
fn run_one(open: &gravity_driver::Open<'_>, claims: &dyn MorselSource, merge: &dyn Merge, abort: &std::sync::atomic::AtomicU8, ordinal: u16, claim: MorselRange) -> Result<()> {
    // the zone walk: a claim past the proven bound is not run and ends the hand-out
    if claims.beyond_bound(claim.lo) {
        claims.close();
        return Ok(());
    }
    let worker = Worker::new(open, ordinal, abort);
    if worker.run_claim(claim)? == gravity_driver::Claimed::Stopped {
        claims.close();
    }
    if let Some(b) = merge.bound(open.partial(ordinal)) {
        claims.observe_bound(b);
    }
    Ok(())
}

pub fn run_section(entry: &mut Entry, exec: &mut Exec, binding: &mut Binding<'_>, derive: &mut dyn Derive, section: &Section, claims: &dyn MorselSource, pool: &dyn Pool, merge: &dyn Merge) -> Result<()> {
    let t_open = std::time::Instant::now();
    let lease = exec.lease();
    // D393b: an arm that continues its union's run skips the owner phase (close_section scatters or merges its partials)
    let cont = exec.node().arm_continues(section.pipeline);
    let first_call = exec.node().program().pipelines[section.pipeline].first_call;
    // D4620: a cascaded grouping set's owners regrow to their even share first (`Pipeline::cascade`)
    let cascade = exec.node().program().pipelines[section.pipeline].cascade;
    // B2: held for the section: each claim's carry (captured at submit) clones the program's catalog arm
    let _catalog = section_arm(exec.node().program());
    let mut open = entry.open_section(exec, binding, derive, section.pipeline, section.width)?;
    let t_open = t_open.elapsed();
    let abort = binding.parts.abort_flag;
    // R1D: a pipeline ending in the emit whose sink orders its legs streams them claim by claim while the section runs
    let mut sink = binding.sink;
    let order = (open.ends_in_sink() && sink.ordered()).then(|| gravity_driver::ClaimOrder::for_section(section.width as usize, claims.first_live_at(0)));
    // an ordered claim reaches the job admitted and settles after it: the pool admits before the claim's lease (D278)
    let reached = std::sync::atomic::AtomicBool::new(false);
    let job = |ordinal: u16, claim: MorselRange| -> Result<()> {
        reached.store(true, Ordering::Relaxed);
        run_one(&open, claims, merge, abort, ordinal, claim)
    };
    if phases_on() {
        census_take(&OWN_CENSUS);
    }
    let t0 = std::time::Instant::now();
    let mut led: Option<Error> = None;
    let mut ran = match &order {
        None => pool.run(section.width, claims, &job),
        Some(o) => {
            let mut los = Vec::new(); // alloc: Exec
            // the window's bytes held in G (grown by doubling as the legs fill; G's refusal narrows the window to the head)
            let mut held = (gravity_driver::Held::NONE, 0u64);
            let mut lead = || {
                if led.is_some() {
                    return;
                }
                let want = sink.buffered();
                if want > held.1 {
                    let to = want.max(held.1 * 2);
                    match lease.hold(to) {
                        Some(h) => held = (h, to),
                        None => o.narrow(),
                    }
                }
                los.clear();
                o.ready(&|u| claims.first_live_at(u), &mut los);
                if los.is_empty() {
                    return;
                }
                match sink.drain_ready(&los) {
                    Ok(gravity_driver::Flow::Continue) => {}
                    Ok(gravity_driver::Flow::Stop) => {
                        o.stop();
                        claims.close();
                    }
                    Err(e) => {
                        o.stop();
                        claims.close();
                        led = Some(e);
                    }
                }
            };
            pool.run_leading(section.width, claims, &job, &mut lead, o)
        }
    };
    if let Some(e) = led {
        // the emit's own error comes before any claim's after it: the rows before it were delivered in claim order. A
        // cancel the emit saw (D379) is the statement's interrupt, not a row's error: a claim that failed meanwhile keeps
        // its error, which may be the interrupt's own (a host call's interrupt check consumed it, and with it the
        // statement_timeout indicator a bare cancel reads back as "user request")
        if !(matches!(e, Error::Cancelled) && ran.is_err()) {
            ran = Err(FirstError { claim_lo: 0, ordinal: 0, error: e });
        }
    }
    // D2877: no claim reached a section whose kernel makes a check PG makes at its node's first call (`Pipeline::first_call`):
    // `FIRST_CALL` on one ordinal (no row: an ordered emit's leg keeps none, so the order is untouched)
    if ran.is_ok() && first_call && !reached.load(Ordering::Relaxed) {
        ran = pool.run(section.width, &PartitionRange::new(1, 1), &|ordinal: u16, _: MorselRange| run_one(&open, claims, merge, abort, ordinal, FIRST_CALL));
    }
    let cancelled = gravity_abi::ctx::abort_set(abort);
    if ran.is_ok() && !cancelled && section.finish {
        // the section end (item 20): the kernel once more per ordinal that claimed, `SECTION_END`
        // as its claim, one unit per ordinal on whatever thread takes it
        let mut claimed = [false; gravity_driver::SLOTS];
        let mut ps: [gravity_driver::Partial<'_>; gravity_driver::SLOTS] = [const { gravity_driver::Partial { ordinal: 0, bytes: &[], rows_processed: 0 } }; gravity_driver::SLOTS];
        let n = open.partials(&mut ps);
        for p in ps.iter().take(n) {
            claimed[p.ordinal as usize] = true;
        }
        let units = PartitionRange::new(section.width as u64, 1);
        let job_f = |_ordinal: u16, range: MorselRange| -> Result<()> {
            for o in range.lo..range.hi.min(section.width as u64) {
                if claimed[o as usize] {
                    open.run_claim(o as u16, MorselRange::SECTION_END)?;
                }
            }
            Ok(())
        };
        // D3259: the merge's seal answer over every partial, read by each ordinal's section end alike
        open.file_seal_over(Some(merge));
        ran = pool.run(section.width, &units, &job_f);
        open.file_seal_over(None);
    }
    let t_sec = t0.elapsed();
    let (mut t_2a, mut t_2b) = (std::time::Duration::ZERO, std::time::Duration::ZERO);
    let cancelled = cancelled || gravity_abi::ctx::abort_set(abort);
    // phase 2 (decision 5): above the gate the workers fold disjoint partition ranges of every
    // partial into the partitioned leader table; a refusal (a slice too small) falls back to the serial fold over a re-zeroed region
    // the owner phase reads the spilled epochs too (§3.7): no seal, no refill at the emit
    // A leader that started small commits what the owners' counted records take before they are cut from it
    // C10 2b (D3582): a spilled one-round group past the statement's room, read by a refilling emit, is batched: the partials
    // go to the slot's files and the emit folds one batch of partitions at a time (driver mem/batch.rs); no owner phase here
    let batched = ran.is_ok() && !cancelled && !cont && match open.own_batched(merge) {
        Ok(b) => b,
        Err(e) => {
            ran = Err(FirstError { claim_lo: 0, ordinal: 0, error: e });
            false
        }
    };
    if ran.is_ok() && !cancelled && !cont && !batched && section.width >= 2 {
        open.owners_in_place(merge);
    }
    let owned = if ran.is_ok() && !cancelled && !batched { open.owned_partitions(merge).map(|b| open.spill_bits().unwrap_or(b)) } else { None };
    // a few records (W workers x a handful of groups): the leader folds them, not two or three pool rounds (wake, lease, return, their stalls)
    let few = || {
        !open.spilled() && !open.has_entry_runs() && {
            let mut ps: [gravity_driver::Partial<'_>; gravity_driver::SLOTS] = [const { gravity_driver::Partial { ordinal: 0, bytes: &[], rows_processed: 0 } }; gravity_driver::SLOTS];
            let n = open.partials(&mut ps);
            merge::held_bytes(&ps[..n]) <= leader_owns_bytes()
        }
    };
    let leads = owned.is_some() && few();
    // KN-02 (B): the grant's owner cut missed the merge's floor over more records than the leader folds alone; the owners
    // sized from the real records, pinned past G within the pool's room as it is now, take the owner phase before one owner on
    // this thread does (`own_past_grant`; `None` = as before)
    // D3004: the owners are the grant's cut (an estimate's size), not regions sized from the real records
    let cut_owners = owned.is_some();
    let owned = match owned {
        None if ran.is_ok() && !cancelled && !cont && !batched && section.width >= 2 && !few() => match open.own_past_grant(merge) {
            Ok(o) => o.inspect(|&pbits| {
                if census_on() {
                    eprintln!("OWN pipeline={} width={} owners=past_grant owner_bytes={} pbits={}", section.pipeline, section.width, open.owner_bytes(), pbits);
                }
            }),
            // D4280: the owners' pin stopped for the statement's cancel
            Err(e) => {
                ran = Err(FirstError { claim_lo: 0, ordinal: 0, error: e });
                None
            }
        },
        o => o,
    };
    if cont {
        // D393b: an arm that continues its run folds only: no owner phase, no partition merge (close_section scatters its partials)
    } else if batched {
        open.set_merged();
    } else if let (Some(pbits), true) = (owned, leads) {
        let tb = std::time::Instant::now();
        if census_on() {
            eprintln!("OWN pipeline={} width={} own_single=leader pbits={}", section.pipeline, section.width, pbits);
        }
        match open.own_single(merge, pbits) {
            Ok(()) => open.set_merged(),
            Err(e) => ran = Err(FirstError { claim_lo: 0, ordinal: 0, error: e }),
        }
        t_2b = tb.elapsed();
    } else if let Some(pbits) = owned {
        // D3004: a cut under the real records is regrown before the owners fold, an overflow after (`own_pooled`)
        let tb = std::time::Instant::now();
        let regrow = match (cut_owners && section.width >= 2, cascade) {
            (false, _) => 0,
            (true, true) => 1,
            (true, false) => 2,
        };
        if let Err(e) = own_pooled(&open, pool, merge, pbits, regrow) {
            ran = Err(e);
        }
        t_2b = tb.elapsed();
    } else if ran.is_ok() && !cancelled && (open.spill_bits().is_some() || open.partition_bits(merge).is_some()) {
        // the owner regions could not hold the shape's results: one owner over the whole leader region, on this thread
        let tb = std::time::Instant::now();
        if census_on() {
            eprintln!("OWN pipeline={} width={} own_single=floor owner_bytes={}", section.pipeline, section.width, open.owner_bytes());
        }
        match open.own_single(merge, open.spill_bits().or(open.partition_bits(merge)).unwrap_or(0)) {
            Ok(()) => open.set_merged(),
            Err(e) => ran = Err(FirstError { claim_lo: 0, ordinal: 0, error: e }),
        }
        t_2b = tb.elapsed();
    } else if ran.is_ok() && !cancelled && !open.spilled() {
        match open.plan_partitions(merge) {
            Ok(Some(plan)) => {
                // phase 2a: every partial prepared on a pool thread (a build partition sorted by partition)
                let prepared = if merge.prepares_partials() {
                    let n = open.partials_count() as u64;
                    let units = PartitionRange::new(n, 1);
                    let job_a = |_ordinal: u16, range: MorselRange| -> Result<()> {
                        for i in range.lo..range.hi {
                            open.prepare_partial(merge, plan, i as usize)?;
                        }
                        Ok(())
                    };
                    let ta = std::time::Instant::now();
                    let r = pool.run(section.width.min(n.max(1) as u16), &units, &job_a);
                    t_2a = ta.elapsed();
                    r
                } else {
                    Ok(())
                };
                let tb = std::time::Instant::now();
                let cursor = PartitionRange::for_width(1u64 << plan.pbits, section.width);
                let job2 = |_ordinal: u16, range: MorselRange| -> Result<()> { open.merge_partitions(merge, plan, range.lo as u32, range.hi as u32) };
                match prepared {
                    Err(FirstError { error: Error::Unsupported(_), .. }) => open.reset_leader(1),
                    Err(e) => ran = Err(e),
                    Ok(()) => match pool.run(section.width, &cursor, &job2) {
                        Ok(()) => open.set_merged(),
                        Err(FirstError { error: Error::Unsupported(_), .. }) => open.reset_leader(2),
                        Err(e) => ran = Err(e),
                    },
                }
                t_2b = tb.elapsed();
            }
            Ok(None) => {}
            Err(Error::Unsupported(_)) => open.reset_leader(0),
            Err(e) => ran = Err(FirstError { claim_lo: 0, ordinal: 0, error: e }),
        }
    }
    let ok = ran.is_ok() && !cancelled;
    let tc = std::time::Instant::now();
    let closed = entry.close_section(open, binding, merge, ok);
    if phases_on() {
        let w = section.width.max(1) as f64 * 1e6;
        let o = census_take(&OWN_CENSUS);
        let runs = gravity_driver::RUN_CENSUS[0].swap(0, Ordering::Relaxed);
        eprintln!("PHASES pipeline={} width={} open={:.2}ms section={:.2}ms 2a={:.2}ms 2b={:.2}ms close={:.2}ms total={:.2}ms own/thr: zero={:.2} fold={:.2} reduce={:.2} parts={} recs={} runs={} rounds={} retries={}", section.pipeline, section.width, t_open.as_secs_f64() * 1e3, t_sec.as_secs_f64() * 1e3, t_2a.as_secs_f64() * 1e3, t_2b.as_secs_f64() * 1e3, tc.elapsed().as_secs_f64() * 1e3, t0.elapsed().as_secs_f64() * 1e3, o[0] as f64 / w, o[1] as f64 / w, o[2] as f64 / w, o[3], o[4], runs, if owned.is_some() { merge.owned_rounds() } else { 0 }, TABLE_RETRY.swap(0, Ordering::Relaxed));
    }
    match ran {
        Err(first) => Err(first.error),
        Ok(()) if cancelled => Err(Error::Cancelled),
        Ok(()) => closed,
    }
}

/// The owner phase (partitioned-record.md §3.2) at `pbits` over `open`'s partials and its slot's files: the pool's threads
/// claim partitions one at a time and fold every partial's partition into their own region; no leader fold, no plan.
/// D3004: a one-round owner region the grant's cut sized from an estimate under the result overflows; the owner phase
/// runs once more over regions sized from the partials' real records (`own_past_grant`, KN-02 (B)) before one owner on
/// this thread redoes it all (a generic plan's `col = $1` estimate: 8.7 s of one owner's fold at W8). `regrow` 2: the
/// regrown owners take twice the real records' even share, then four times (a skewed group's partition), then one owner;
/// a cut under the real records (`owners_short`) is regrown before the first owner phase. 0: an overflow goes to one owner.
/// 1 (D4620, `Pipeline::cascade`): the even share first, then twice and four times it.
fn own_pooled(open: &gravity_driver::Open<'_>, pool: &dyn Pool, merge: &dyn Merge, mut pbits: u8, mut regrow: usize) -> core::result::Result<(), FirstError> {
    let (pipeline, width) = (open.pipeline(), open.width());
    let first = |error: Error| FirstError { claim_lo: 0, ordinal: 0, error };
    if regrow != 0 && open.owners_short(merge, pbits) && open.own_past_grant_at(merge, regrow).map_err(first)?.inspect(|&p| pbits = p).is_some() {
        regrow *= 2;
        if census_on() {
            eprintln!("OWN pipeline={pipeline} width={width} owners=regrown_first owner_bytes={} pbits={pbits}", open.owner_bytes());
        }
    }
    loop {
        let opened: [std::sync::atomic::AtomicBool; gravity_driver::SLOTS] = core::array::from_fn(|_| std::sync::atomic::AtomicBool::new(false));
        let units = PartitionRange::new(1u64 << pbits, 1);
        let job_o = |ordinal: u16, range: MorselRange| -> Result<()> { open.own_partitions(merge, ordinal, pbits, range.lo as u32, range.hi as u32, &opened[ordinal as usize]) };
        let mut r = pool.run(width, &units, &job_o);
        if r.is_ok() && merge.owned_rounds() == 1 {
            // every owner's round closed on a pool thread (a one-round shape orders its run here)
            let owners = PartitionRange::new(width as u64, 1);
            let job_e = |_ordinal: u16, range: MorselRange| -> Result<()> {
                for o in range.lo..range.hi.min(width as u64) {
                    let region = open.owner_region(o as u16);
                    if !opened[o as usize].swap(true, Ordering::AcqRel) {
                        merge.open_owner(region, o as u16)?;
                    }
                    merge.end_round(region, &mut || Err(Error::Unsupported(gravity_abi::status::UNSUPPORTED_SCRATCH)))?;
                }
                Ok(())
            };
            r = pool.run(width, &owners, &job_e);
        }
        if r.is_ok() && merge.owned_rounds() == 2 {
            // the second round (§3.5): the owners' round-2 buffers re-folded by the group hash
            r = match open.end_round(merge, &opened[..width as usize]) {
                Ok(pbits2) => {
                    let units2 = PartitionRange::new(1u64 << pbits2, 1);
                    let job2 = |ordinal: u16, range: MorselRange| -> Result<()> { open.own_partitions2(merge, ordinal, pbits2, range.lo as u32, range.hi as u32) };
                    pool.run(width, &units2, &job2)
                }
                Err(e) => Err(first(e)),
            };
        }
        // D3004: an overflowed one-round owner phase regrows its owners from the real records (`own_past_grant_at`)
        let regrown = matches!(r, Err(FirstError { error: Error::Unsupported(_), .. })) && merge.owned_rounds() == 1 && regrow != 0 && regrow <= 4 && open.own_past_grant_at(merge, regrow).map_err(first)?.inspect(|&p| pbits = p).is_some();
        return match r {
            Ok(()) => open.finish_owners(merge, &opened[..width as usize]).map(|()| open.set_merged()).map_err(first),
            Err(FirstError { error: Error::Unsupported(_), .. }) if regrown => {
                // D3004: regions sized from the real records (`own_past_grant_at` pinned them), the owner phase again
                regrow *= 2;
                OWN_OVERFLOW.fetch_add(1, Ordering::Relaxed);
                if census_on() {
                    eprintln!("OWN pipeline={pipeline} width={width} owners=regrown owner_bytes={} pbits={pbits}", open.owner_bytes());
                }
                continue;
            }
            Err(FirstError { error: Error::Unsupported(_), .. }) if merge.owned_rounds() == 1 => {
                // an owner region overflowed (the estimate under the result): a one-round shape wrote only its owner regions, `own_single` redoes it
                OWN_OVERFLOW.fetch_add(1, Ordering::Relaxed);
                if census_on() {
                    eprintln!("OWN pipeline={pipeline} width={width} own_single=overflow pbits={pbits}");
                }
                open.own_single(merge, pbits).map(|()| open.set_merged()).map_err(first)
            }
            Err(FirstError { error: Error::Unsupported(_), .. }) if merge.owned_rounds() == 2 => {
                // D533: a two-round owner's partition past its route bits (merge.rs fold_sub SCRATCH) or an owner region it
                // overflowed: both rounds redone by one owner on this thread, in a region own_all grows up to the wall; the
                // owners only read the partials and files, and their round-2 files restart empty
                OWN_OVERFLOW.fetch_add(1, Ordering::Relaxed);
                if census_on() {
                    eprintln!("OWN pipeline={pipeline} width={width} own_single=overflow2 pbits={pbits}");
                }
                open.reset_round2();
                open.own_single(merge, pbits).map(|()| open.set_merged()).map_err(first)
            }
            Err(e) => Err(e),
        };
    }
}

/// D899: a pipeline's late key words, as (word, value, null byte), in the block the plan fills (`Plan::late_words`). memsig
/// E-pub: the caller makes it on the owner it holds (the execution's prepare owner, exec `ops::State::charged_vec`; none in
/// a rig, `LateWords::none()`: uncharged), and every protocol appends to it (`Live::late_words`); dropped when the
/// pipeline's run under its image ends (`with_late`).
pub type LateWords = memledger::ChargedVec<(usize, u64, u8)>;

/// M8 (RT-2f): the pulled member sets the statement's end drains (`Plan::drain_at_end`), each borrowed from the protocol
/// that names it, in drain order. memsig E-pub: one block on the execution's prepare owner (exec `ops::State::charged_vec`;
/// uncharged when nothing drains, `DrainSets::none()`).
pub type DrainSets<'a> = memledger::ChargedVec<&'a [u16]>;

pub trait Plan {
    fn width_of(&self, pipeline: usize) -> u16;
    /// D3014 S2', D3137: whether a nested run's eligible member may run as a section (`nested_section`); off unless the plan
    /// says so.
    fn rerun_sections(&self) -> bool {
        false
    }
    fn claims_for(&self, pipeline: usize) -> &dyn MorselSource;
    fn merge_for(&self, pipeline: usize) -> &dyn Merge;
    /// After `pipeline` completed (as a section or serially), before its consumer starts.
    fn after_pipeline(&self, _pipeline: usize, _exec: &mut Exec) -> Result<()> {
        Ok(())
    }
    /// D899: `pipeline`'s late key words evaluated now, as (word, value, null byte): PostgreSQL's runtime keys, evaluated
    /// at their scan's first pull (`run_with_late` calls it as the pipeline starts, never for one the runner skips).
    fn late_words(&self, _pipeline: usize) -> Result<LateWords> {
        Ok(LateWords::none())
    }
    /// D2736: a pulled set's last pipeline's late key words for its next pull (`pull_late`): evaluated at its first pull and
    /// the same at every later one, as PostgreSQL evaluates a scan's runtime keys once at its first fetch, until a re-open
    /// of the pipeline (`reopen`: PostgreSQL's rescan evaluates them again).
    fn pulled_late(&self, pipeline: usize) -> Result<LateWords> {
        self.late_words(pipeline)
    }
    /// D2289: the rerun set `members` re-opens (PG's ExecReScan of the set's nodes), before its members run again: a
    /// protocol holding per-open state of one of them resets it.
    fn reopen(&self, _members: &[u16]) -> Result<()> {
        Ok(())
    }
    /// M8 (RT-1): `pipeline` runs only on demand (an Init's member): the runner steps past it in the producer order.
    fn deferred(&self, _pipeline: usize) -> bool {
        false
    }
    /// M8 (RT-2e): a deferred `pipeline` that is a rerun set's member: the set's owner (the pipeline calling `m8_rerun`).
    fn rerun_owner(&self, _pipeline: usize) -> Option<usize> {
        None
    }
    /// M8 (RT-2f): a deferred `pipeline` that is a pulled set's last member (a CTE's plan): its readers (every CteScan
    /// pipeline of the CTE, each pulling before it reads); empty: none.
    fn pull_readers(&self, _pipeline: usize) -> &[usize] {
        &[]
    }
    /// M8 (RT-2f): the pulled member sets that run to their end at the statement's end whether or not a reader pulled them
    /// all (a data-modifying CTE, PG's ExecPostprocessPlan): `drain_pulled` pulls each to `Done`.
    fn drain_at_end(&self) -> DrainSets<'_> {
        DrainSets::none()
    }
    /// M8 (RT-1): publish `f` (the runner's frame, `None`: none) to the plan's protocols; the one published before.
    fn frame(&self, _f: Option<gravity_driver::Frame>) -> Option<gravity_driver::Frame> {
        None
    }
    /// D2105: `pipeline` is an arm this run's exec pruning removed (a rerun set's Append arm): it runs as a relation
    /// pruned whole, as `runs_empty`'s producer does: no late word (its runtime keys, the InitPlans they read) and no
    /// claim. PostgreSQL's ExecReScanAppend never opens a pruned arm, so it never evaluates either.
    fn skips_whole(&self, _pipeline: usize) -> bool {
        false
    }
    /// EXPLAIN ANALYZE: the counter cells a recorded run ends with (`cells`, the program's `instr` order), with the cells
    /// the program never adds to that the plan's protocols fill (a Memoize's cache counters: exec `State::fill_cells`).
    fn fill_cells(&self, _cells: &mut [u64]) {}
}

/// M8 (RT-1): a frame published to a plan until this drops (`publish`); the frame before it back then, on every exit of
/// the scope, an unwind included, so frames stack as the runs that publish them nest.
pub struct Published<'g> {
    plan: &'g dyn Plan,
    prev: Option<gravity_driver::Frame>,
    _frame: core::marker::PhantomData<&'g mut ()>,
}

impl Drop for Published<'_> {
    fn drop(&mut self) {
        self.plan.frame(self.prev);
    }
}

/// M8 (RT-1): publish `f` to `plan`'s protocols for as long as the guard lives: a host of a pipeline the runner calls
/// meanwhile (`m8_init_run`) runs other pipelines through it, nested (`Entry::run_nested`).
pub fn publish<'g>(plan: &'g dyn Plan, f: &'g mut (dyn gravity_driver::OnDemand + 'g)) -> Published<'g> {
    // SAFETY: the lifetime is erased only for the plan's cell: the guard borrows `f` for 'g and takes it out of the plan
    // (the frame before it back) when it drops, so nothing reads the pointer after `f` is gone.
    let frame = unsafe { core::mem::transmute::<core::ptr::NonNull<dyn gravity_driver::OnDemand + 'g>, gravity_driver::Frame>(core::ptr::NonNull::from(f)) };
    Published { plan, prev: plan.frame(Some(frame)), _frame: core::marker::PhantomData }
}

/// D899: pipeline `p`'s run (`f`) under its own parameter image: the binding's words with `p`'s late key words bound now
/// (`Plan::late_words`), so a pipeline PostgreSQL never starts never evaluates them (a lazy build under an empty outer runs
/// only from `build_wait`; a producer an empty build masks, `runs_empty`, binds none). An evaluation error is the
/// pipeline's own: held as a failed run's when its merge defers it (op 14), else the statement's (`held` is the arm's
/// result then). A pipeline without late words runs as before, on the binding's image.
pub fn run_with_late<R>(entry: &mut Entry, exec: &mut Exec, binding: &mut Binding<'_>, plan: &dyn Plan, p: usize, f: impl FnOnce(&mut Entry, &mut Exec, &mut Binding<'_>) -> Result<R>, held: impl FnOnce() -> R) -> Result<R> {
    // G1-B: the regions the pipeline's run draws (its calls, its claims, its owner phase, a pool claim's through the session
    // carry) count under its breaker's kind in the memory ledger's per-kind split
    let _kind = gravity_driver::pipeline_kind_scope(exec.node().program(), p);
    let pruned = plan.skips_whole(p);
    // D3670: a member's run counts the scan its driving site begins or rescans (its bind did not); PG's
    // ExecReScanAppend never reaches an arm its exec pruning removed, so a pruned arm counts none
    if !pruned {
        exec.node_mut().count_scan(p);
    }
    // D2105: an exec-pruned arm binds no late word either (`Plan::skips_whole`)
    if exec.runs_empty(p) || pruned {
        return f(entry, exec, binding);
    }
    // D1702: a late step's host may run an Init's members ahead (`prerun_members`): over this call's execution
    let late = match entry.with_current(exec, binding, || plan.late_words(p)) {
        Ok(l) => l,
        Err(e) => {
            // op 14: the key's error is the pipeline's own, before any claim opened its slot: open it (as a claim-less
            // producer's, D292) so a deferring merge holds the error for the probe's first row
            if exec.node().program().pipelines[p].produces().is_some() {
                entry.open_unclaimed_of(exec, p)?;
            }
            entry.defer_failed(exec, binding, plan.merge_for(p), p, e).or_else(|e| entry.defer_fed(exec, binding, &|q| plan.merge_for(q), p, e))?;
            return Ok(held());
        }
    };
    with_late(exec, binding, late, |x, b| f(entry, x, b))
}

/// D899: `f` under the binding's words with `late` (`Plan::late_words`) bound, then the binding's own image again:
/// `run_with_late`'s image, and a recheck's rerun producer's (exec, D655: PostgreSQL's EvalPlanQual rescans the index,
/// its runtime keys, and rechecks the refetched row against them).
pub fn with_late<R>(exec: &mut Exec, binding: &mut Binding<'_>, late: LateWords, f: impl FnOnce(&mut Exec, &mut Binding<'_>) -> Result<R>) -> Result<R> {
    if late.is_empty() {
        return f(exec, binding);
    }
    let owner = gravity_driver::stmt_context().map(|(o, _)| o);
    let (mut params, mut nulls): (Box<[u64]>, Box<[u8]>) = (binding.parts.params.into(), binding.parts.param_nulls.into()); // alloc: Exec
    let mut late_at: memledger::ChargedVec<usize> = memledger::ChargedVec::new(owner, memledger::Kind::Region); // alloc: Exec charged owner
    late_at.with_vec(|v| { v.reserve_exact(late.len()); v.extend(late.iter().map(|l| l.0)) }); // alloc: Exec charged owner
    for &(w, v, n) in late.iter() {
        if w < params.len() && w < nulls.len() {
            (params[w], nulls[w]) = (v, n);
        }
    }
    drop(late);
    let saved = (binding.parts.params, binding.parts.param_nulls);
    let mut made: (memledger::ChargedVec<u64>, memledger::ChargedVec<u8>) = (memledger::ChargedVec::new(owner, memledger::Kind::Region), memledger::ChargedVec::new(owner, memledger::Kind::Region)); // alloc: Exec charged owner
    made.0.with_vec(|v| { v.reserve_exact(params.len()); v.extend_from_slice(&params) }); // alloc: Exec charged owner
    made.1.with_vec(|v| { v.reserve_exact(nulls.len()); v.extend_from_slice(&nulls) }); // alloc: Exec charged owner
    // SAFETY: the image lives on this frame until the binding and the leader's context point back at `saved` below, after
    // `f` returns, Ok or Err (every section `f` launched has joined by then; a section's workers copied the pointers at its
    // launch, which happens after the image was complete). `saved` is the image found on entry (the binding's own, or an
    // enclosing `with_late`'s: the images stack). Nothing writes the image after this point: it is created whole, before
    // any reader, and dropped after the last.
    unsafe {
        binding.parts.params = &*(&*params as *const [u64]);
        binding.parts.param_nulls = &*(&*nulls as *const [u8]);
    }
    exec.node_mut().repoint_params(binding.parts.params, binding.parts.param_nulls);
    let r = f(exec, binding);
    (binding.parts.params, binding.parts.param_nulls) = saved;
    exec.node_mut().repoint_params(saved.0, saved.1);
    // D2736: a word the run wrote into the image (a protocol's state word: a settled union's spool, its fill's length) is
    // the execution's, as it is where no late word made an image: it goes back to the words the image was copied from. The
    // late words stay the run's (the binding's own keep their bind values)
    for i in 0..params.len().min(nulls.len()).min(saved.0.len()).min(saved.1.len()) {
        if (params[i] != made.0[i] || nulls[i] != made.1[i]) && !late_at.contains(&i) {
            // SAFETY: the binding's words are the execution's parameter image, which the program's hosts write in place
            // through the context's pointer (a state word); this frame alone writes them now, between two runs
            unsafe {
                *(saved.0.as_ptr() as *mut u64).add(i) = params[i];
                *(saved.1.as_ptr() as *mut u8).add(i) = nulls[i];
            }
        }
    }
    drop((params, nulls));
    r
}

/// D2736: a pulled set's pull (`Entry::pull_nested`, `f`) under its last pipeline's late key words (`Plan::pulled_late`):
/// the driver runs that pipeline itself, outside `run_with_late`, so its scan read none of the key words it binds late (a
/// pulled WindowAgg fill over an index scan keyed by an expression returned no rows). A masked or exec-pruned last
/// pipeline binds none (as `run_with_late`).
pub fn pull_late<R>(entry: &mut Entry, exec: &mut Exec, binding: &mut Binding<'_>, plan: &dyn Plan, members: &[u16], f: impl FnOnce(&mut Entry, &mut Exec, &mut Binding<'_>) -> Result<R>) -> Result<R> {
    let Some(q) = members.last().map(|&m| m as usize).filter(|&q| !exec.runs_empty(q) && !plan.skips_whole(q)) else { return f(entry, exec, binding) };
    let late = entry.with_current(exec, binding, || plan.pulled_late(q))?;
    with_late(exec, binding, late, |x, b| f(entry, x, b))
}

/// D2816: a lazy build's wait (`Entry::build_wait`) runs the producers fed into the build out of order, in program order.
/// A pulled set's last member among them (`Plan::pull_readers`: a merge join's pulled inner, which feeds the build through
/// the merge) is stepped past as the producer order steps it, its store opened zeroed for its readers (`skip_pulled`), so
/// its reader's first pull opens and calls it (`Entry::pull_nested`). Run whole here, it stayed opened but not parked, and
/// that pull called it on under its reader's regions (PostgreSQL fetches a merge's inner only as the merge reads it, also
/// inside a Hash or Sort it builds at its parent's first outer row). `true`: stepped past.
/// D3524: so is a rerun set's member whose owner is fed into the build too (not open yet: it runs later in this wait):
/// stepped past as the producer order steps it (`skip_rerun`; `skip_rearmed` for a CTE's plan the set re-arms), its slot
/// kept and opened zeroed for the owner, whose `m8_rerun` runs it at each outer row. Run whole here, it stayed open over a
/// slot no rerun keeps, so the owner's rerun neither reset nor reopened it and called it over the owner's own region (a
/// Sort member read the owner's hashed group table as its run: breaker.rs 'sort run shape'). A member whose owner is open
/// is a member lazy build inside its set's run (D1700): it runs here.
pub fn skip_in_wait(entry: &mut Entry, exec: &mut Exec, plan: &dyn Plan, p: usize) -> Result<bool> {
    if !plan.deferred(p) {
        return Ok(false);
    }
    let owner = plan.rerun_owner(p);
    if owner.is_some_and(|o| exec.node().is_opened(o)) {
        return Ok(false);
    }
    let readers = plan.pull_readers(p);
    match owner {
        Some(o) if readers.is_empty() => entry.skip_rerun(exec, o)?,
        Some(o) => entry.skip_rearmed(exec, o, &readers)?,
        None if readers.is_empty() => return Ok(false),
        None => entry.skip_pulled(exec, &readers)?,
    }
    Ok(true)
}

pub fn run_program(entry: &mut Entry, exec: &mut Exec, binding: &mut Binding<'_>, derive: &mut dyn Derive, pool: &dyn Pool, plan: &dyn Plan) -> Result<u64> {
    run_body(entry, exec, binding, derive, pool, plan, &mut |_, _, _, _, site| Err(Error::Recheck(site)))
}

/// D610: a recheck hook's view of the derive `run_program` was given (the runner's own registry, whose rewind the
/// recheck node needs, or the trait object of `run_program`'s callers).
pub trait AsDerive {
    fn as_derive(&mut self) -> &mut dyn Derive;
}

impl<T: Derive> AsDerive for T {
    fn as_derive(&mut self) -> &mut dyn Derive {
        self
    }
}

impl<'a> AsDerive for dyn Derive + 'a {
    fn as_derive(&mut self) -> &mut dyn Derive {
        self
    }
}

/// D610: the leader's EvalPlanQual for a pipeline it runs (the emit arm: a write or lock is never a worker's). A stop on a
/// recheck calls `recheck` with the pipeline's exec, which rechecks the row on a second node and counts what it
/// processed into the binding; `Ok(true)` resumes the pipeline past the row (its resume word), `Ok(false)` ends the
/// statement there (a stop the kernel cannot resume: the serial runner's end, legal only on the last pipeline).
pub fn run_program_rechecking<D: Derive>(entry: &mut Entry, exec: &mut Exec, binding: &mut Binding<'_>, derive: &mut D, pool: &dyn Pool, plan: &dyn Plan, recheck: &mut dyn FnMut(&mut Entry, &mut Exec, &mut Binding<'_>, &mut D, u8) -> Result<bool>) -> Result<u64> {
    run_body(entry, exec, binding, derive, pool, plan, recheck)
}

#[allow(clippy::too_many_arguments)]
fn run_body<D: AsDerive + ?Sized>(entry: &mut Entry, exec: &mut Exec, binding: &mut Binding<'_>, derive: &mut D, pool: &dyn Pool, plan: &dyn Plan, recheck: &mut dyn FnMut(&mut Entry, &mut Exec, &mut Binding<'_>, &mut D, u8) -> Result<bool>) -> Result<u64> {
    // D1702: the slot the entry's calls publish their execution in; a nested run re-enters through it (`Leader::at`)
    let current = gravity_driver::CurrentCell::new(None);
    entry.with_current_slot(&current, |entry| run_slotted(entry, exec, binding, derive, pool, plan, recheck, &current))
}

/// D1702: `run_body` with `current` the entry's installed slot.
#[allow(clippy::too_many_arguments)]
fn run_slotted<D: AsDerive + ?Sized>(entry: &mut Entry, exec: &mut Exec, binding: &mut Binding<'_>, derive: &mut D, pool: &dyn Pool, plan: &dyn Plan, recheck: &mut dyn FnMut(&mut Entry, &mut Exec, &mut Binding<'_>, &mut D, u8) -> Result<bool>, current: &gravity_driver::CurrentCell) -> Result<u64> {
    let program = exec.node_mut().program().clone();
    let recorded = entry.recording(&program);
    let legs = binding.sink.leg(0).is_some();
    let mut barrier = Barrier::new(&program);
    // D1708 (a): the derive and the recheck hook stay raw below this frame, as the frame keeps them: no arm holds a `&mut` of
    // either across a call a nested run re-enters from (`Leader`), whose section opens write the derive (`Node::provision`)
    let (derive, recheck): (*mut D, *mut _) = (derive, recheck);
    // M8 (RT-1): the leader's frame while it runs the program: an Init read in any of its calls runs the Init's members
    let mut leader = Leader { entry: entry as *mut Entry, exec: exec as *mut Exec, binding: binding as *mut Binding<'_>, derive, pool, plan, recheck, legs, current };
    let _frame = publish(plan, &mut leader);
    while let Some(p) = barrier.next() {
        // D2772 POST: the turn before an unread data-modifying CTE's pipelines, the program's last (driver entry/post.rs)
        gravity_driver::entry::post_turn(exec, binding, p)?;
        // D824 (V4R-298): a lazy build runs only when its outer-first probe's first row reaches it (`Arm::build_wait`)
        if entry.skip_lazy(exec)? {
            barrier.done(p);
            continue;
        }
        // M8 (RT-1): an Init's member runs only when its param is first read (`Leader::run_members`), or never
        if plan.deferred(p) {
            // M8 (RT-2e): a rerun set's member: its slot kept, opened zeroed for the owner (`Entry::skip_rerun`)
            match plan.rerun_owner(p) {
                // setop_rec-0043: a CTE's plan the set re-arms, its store opened for the owner and the CTE's readers
                Some(owner) => match plan.pull_readers(p) {
                    r if r.is_empty() => entry.skip_rerun(exec, owner)?,
                    r => entry.skip_rearmed(exec, owner, &r)?,
                },
                // M8 (RT-2f): a pulled CTE's plan: its store edge kept, opened zeroed for its readers
                None => match plan.pull_readers(p) {
                    r if r.is_empty() => entry.skip_deferred(exec),
                    r => entry.skip_pulled(exec, &r)?,
                },
            }
            barrier.done(p);
            continue;
        }
        // D3586: the clock only where a record or the phase lines read it (three reads a pipeline, every arm of an Append)
        let timed = recorded || phases_on();
        let t_pipe = timed.then(std::time::Instant::now);
        // EXPLAIN ANALYZE's record (driver entry/record.rs): the claims, the clock and the buffer counters before the arm,
        // read only for a recorded program
        let claims0 = recorded.then(|| pool.claims_run());
        let mark = if recorded { entry.record_mark() } else { None };
        // SAFETY: `derive` and `recheck` are this frame's own arguments, alive across the arm, reached on this thread only
        let arm = match unsafe { run_arm(entry, exec, binding, derive, pool, plan, recheck, (p, legs, false)) }? {
            Arm::Ran(a) => a,
            Arm::Return(rows) => {
                if recorded {
                    record_cells(entry, exec, plan);
                }
                return Ok(rows);
            }
        };
        let t_arm = t_pipe.map_or(std::time::Duration::ZERO, |t| t.elapsed());
        if let Some(c0) = claims0 {
            let width = if arm == "section" { plan.width_of(p) } else { 0 };
            // serial census M0: the lowered verdict beside the arm, so a serial or emit row names why it ran on the leader
            let (why, site) = gravity_driver::entry::record::verdict(program.pipelines[p].parallel);
            let (at, b0) = mark.unwrap_or((0, [0; 10]));
            let buf = entry.record_bufs_since(&b0);
            entry.record_push(gravity_driver::entry::record::PipeRun { pipeline: p as u16, arm, width, ns: t_arm.as_nanos() as u64, claims: pool.claims_run() - c0, why, site, at, buf });
        }
        let t_after = timed.then(std::time::Instant::now);
        plan.after_pipeline(p, exec)?;
        if let Some(t_after) = t_after.filter(|_| phases_on()) {
            eprintln!("PHASES pipeline={} arm={} arm_ms={:.2} after={:.2}ms", p, arm, t_arm.as_secs_f64() * 1e3, t_after.elapsed().as_secs_f64() * 1e3);
        }
        barrier.done(p);
        // op 14, fed (V4R-89): the feeders and the build `Entry::defer_fed` passed unrun
        for q in p + 1..exec.next_pipeline().min(program.pipelines.len()) {
            barrier.next();
            barrier.done(q);
        }
    }
    // M8 (RT-2f): the data-modifying pulled sets run to their end after the emit (PG's ExecPostprocessPlan)
    drain_pulled(plan)?;
    if recorded {
        record_cells(entry, exec, plan);
    }
    Ok(binding.rows_processed)
}

/// EXPLAIN ANALYZE's record of the counter cells the run ended with, the plan's own (`Plan::fill_cells`) written in.
fn record_cells(entry: &mut Entry, exec: &Exec, plan: &dyn Plan) {
    let owner = gravity_driver::stmt_context().map(|(o, _)| o);
    let src = exec.node().instr_cells();
    let mut cells: memledger::ChargedVec<u64> = memledger::ChargedVec::new(owner, memledger::Kind::Region); // alloc: Exec charged owner
    cells.with_vec(|v| { v.reserve_exact(src.len()); v.extend_from_slice(src) }); // alloc: Exec charged owner
    plan.fill_cells(&mut cells);
    entry.record_cells(&cells);
}

/// M8 (RT-1): `run_body`'s frame (`publish`): an Init read inside one of the leader's calls runs the Init's members through
/// `Entry::run_nested`, each by its width-1 arm and its `after_pipeline`, as a lazy build's pipelines run (`build_wait`).
/// The fields are `run_body`'s own arguments as raw pointers: the nested run happens inside a call `run_body` is making
/// (the kernel's host call), on the leader's thread (every reader of an Init param is pinned to the leader, D1035), and every field of the
/// execution the members' runs move and the call reads after it (the next pipeline, the state, the context's attachment,
/// flags and cap) is put back before the host returns (`Entry::run_nested`).
struct Leader<'r, 'b, D: AsDerive + ?Sized> {
    entry: *mut Entry,
    exec: *mut Exec,
    binding: *mut Binding<'b>,
    derive: *mut D,
    pool: &'r dyn Pool,
    plan: &'r dyn Plan,
    recheck: *mut (dyn FnMut(&mut Entry, &mut Exec, &mut Binding<'_>, &mut D, u8) -> Result<bool> + 'r),
    legs: bool,
    /// D1702: the entry's installed `Current` slot (`run_body`'s).
    current: *const gravity_driver::CurrentCell,
}

impl<D: AsDerive + ?Sized> gravity_driver::OnDemand for Leader<'_, '_, D> {
    fn run_members(&mut self, members: &[u16]) -> Result<()> {
        self.members(members, false, false)
    }

    fn rerun_members(&mut self, members: &[u16]) -> Result<()> {
        self.members(members, true, false)
    }

    fn refill_members(&mut self, members: &[u16]) -> Result<()> {
        self.members(members, true, true)
    }

    /// D1035 step 2 (V4R-444): an eager Init's reader starts: when it runs as a section (width >= 2), the members run ahead
    /// by `run_arm` at their widths (the Init's own pipeline, the last, width 1: its end folds into the cell), then true.
    fn prerun_members(&mut self, reader: usize, members: &[u16]) -> Result<bool> {
        // SAFETY: as `Leader`: `run_body` owns the entry, execution, binding, derive and recheck for its whole run and
        // unpublishes this frame (`_frame`) before any of them ends; `at`'s current is the innermost call's own, live across
        // that call, inside which this nested run runs; pool / plan / legs are `run_body`'s own borrows ('r). This thread only.
        let (e, x, b) = unsafe { self.at() };
        let (entry, exec, binding) = unsafe { (&mut *e, &mut *x, &mut *b) };
        // D1708 (a): the derive and the hook passed on raw, as `run_body` passes them: every arm between `run_body` and this
        // call holds them so (a `&mut` taken here from the frame's pointer and written by a member's section open would be a
        // write foreign to an outer arm's protected borrow)
        let (derive, recheck, pool, plan, legs) = (self.derive, self.recheck, self.pool, self.plan, self.legs);
        let program = exec.node().program().clone();
        if Section::plan(&program, reader, plan.width_of(reader), legs).is_none() {
            return Ok(false);
        }
        let last = members.last().map(|&m| m as usize);
        let mut run = |e: &mut Entry, x: &mut Exec, bd: &mut Binding<'_>, p: usize| -> Result<()> {
            // D2998: an EXISTS Init's own pipeline may run as a section too (its rows only store true and stop)
            let lazy = Some(p) == last && !(gravity_driver::exists_cell(&program, p) && plan.width_of(p) >= 2);
            if !lazy && Some(p) == last {
                fires::fire("exec.exists.section");
            }
            // SAFETY: `run_body`'s derive and hook (as above), no `&mut` of either live across this arm
            match unsafe { run_arm(e, x, bd, derive, pool, plan, recheck, (p, legs, lazy)) }? {
                Arm::Ran(_) => plan.after_pipeline(p, x),
                Arm::Return(_) => Err(Error::BadReturn(gravity_abi::status::ERROR)),
            }
        };
        entry.run_ahead(exec, binding, members, &mut run).map(|()| true)
    }

    fn pull_members(&mut self, members: &[u16], _budget: u64) -> Result<gravity_driver::Pulled> {
        // SAFETY: as `Leader`: `run_body` owns the entry, execution, binding, derive and recheck for its whole run and
        // unpublishes this frame (`_frame`) before any of them ends; `at`'s current is the innermost call's own, live across
        // that call, inside which this nested run runs; pool / plan / legs are `run_body`'s own borrows ('r). This thread only.
        let (e, x, b) = unsafe { self.at() };
        let (entry, exec, binding) = unsafe { (&mut *e, &mut *x, &mut *b) };
        // D1708 (a): passed on raw (`prerun_members`)
        let (derive, recheck, pool, plan, legs) = (self.derive, self.recheck, self.pool, self.plan, self.legs);
        pulled_serial(plan, members)?;
        let program = exec.node_mut().program().clone();
        let mut run = |e: &mut Entry, x: &mut Exec, bd: &mut Binding<'_>, p: usize| -> Result<()> {
            // D3095: a producer the pull runs whole (never a lazy build, which runs at its probe's first row) runs as a section
            // over its claims taken from the start when they are big enough (`nested_section`, D3137's rule for every nested
            // producer; the serial statement's pull, exec runner/producers.rs, takes the same rule); the pulled pipeline is
            // bound at width 0 (`pulled_serial`), so it stays on the leader
            let lazy = program.pipelines[p].lazy_for >= 0 || !nested_section(plan, &program, p, true);
            // SAFETY: `run_body`'s derive and hook, no `&mut` of either live across this arm
            match unsafe { run_arm(e, x, bd, derive, pool, plan, recheck, (p, legs, lazy)) }? {
                Arm::Ran(_) => plan.after_pipeline(p, x),
                Arm::Return(_) => Err(Error::BadReturn(gravity_abi::status::ERROR)),
            }
        };
        pull_late(entry, exec, binding, plan, members, |e, x, b| e.pull_nested(x, b, members, &mut run))
    }

    fn end_main(&mut self) -> Result<()> {
        // SAFETY: as `pull_members`
        let (_, x, _) = unsafe { self.at() };
        unsafe { &mut *x }.node_mut().end_main()
    }

    /// setop_rec-0043: a pulled set re-armed by a rerun set's run: the protocols' per-open state of its members reset first
    /// (D2289's `reopen`: a recursive union's generation object), then the driver's pull state and kept slots
    /// (`Entry::rearm_pulled`).
    fn rearm_pulled(&mut self, members: &[u16]) -> Result<()> {
        // SAFETY: as `pull_members`: `run_body` owns the entry and execution for its whole run; this thread only.
        let (e, x, _) = unsafe { self.at() };
        let (entry, exec) = unsafe { (&mut *e, &mut *x) };
        self.plan.reopen(members)?;
        entry.rearm_pulled(exec, members)
    }
}

/// M8 (RT-2f): a pulled set runs leader-serial (a parked member is never a pool section stopped
/// mid-run with workers holding claims): the pulled pipeline (the members' last) is planned serial; one at a width past 1 is
/// a Bug by name. D3095: its producers run whole before its first window, so one may keep the statement's width and run as a
/// section at the pull (`nested_section`): no claim is held across a stop.
pub fn pulled_serial(plan: &dyn Plan, members: &[u16]) -> Result<()> {
    match members.last().filter(|&&m| plan.width_of(m as usize) > 1) {
        Some(&m) => Err(Error::Refused(format!("gravity: Bug: pulled member {m} planned at width {} (a pulled set runs leader-serial, M8 RT-2f)", plan.width_of(m as usize)).into())), // alloc: Cold
        None => Ok(()),
    }
}

/// M8 (RT-2f): PG's ExecPostprocessPlan: each pulled set the plan names (`Plan::drain_at_end`) runs
/// to its end through the frame published to the plan, after the main plan's emit finished and before the statement's
/// end-of-command work; a set a reader already pulled to its end runs nothing again (`Pulled::Done` is sticky).
pub fn drain_pulled(plan: &dyn Plan) -> Result<()> {
    let sets = plan.drain_at_end();
    if sets.is_empty() {
        return Ok(());
    }
    let f = plan.frame(None);
    plan.frame(f);
    let Some(mut f) = f else { return Err(Error::Refused("gravity: Bug: a statement-end drain with no frame published (M8 RT-2f)".into())) };
    // D2605: the statement's own ModifyTable ended with the main plan: its AFTER STATEMENT events queue before the drained
    // CTEs' (driver node/ends.rs)
    // SAFETY: as below
    unsafe { f.as_mut() }.end_main()?;
    for set in sets.iter() {
        // SAFETY: the frame published to the plan (`publish`), alive while its guard is; this thread's only.
        while unsafe { f.as_mut() }.pull_members(set, u64::MAX)? == gravity_driver::Pulled::Stopped {}
    }
    Ok(())
}

impl<'b, D: AsDerive + ?Sized> Leader<'_, 'b, D> {
    /// D1702: the execution a nested run re-enters, raw: the innermost runner call's (the entry's current, which
    /// `Entry::run_pipeline` / `claim_at` / `with_current` publish for that call only), else `run_body`'s own (a
    /// statement-end drain: no call on the stack). The caller derives its references from these, as it did from the frame's.
    /// # Safety
    /// `self.current` is `run_body`'s installed slot: `with_current_slot` takes it off and the cell drops after `_frame` unpublishes this
    /// frame, so the cell outlives every call through the frame.
    unsafe fn at(&self) -> (*mut Entry, *mut Exec, *mut Binding<'b>) {
        gravity_driver::current_or(self.current, self.entry, self.exec, self.binding)
    }

    /// `members` run nested (`Entry::run_nested`), or rerun (`Entry::rerun_nested`, M8 RT-2e; D4120 `refill`: their kept
    /// members filled again, `Entry::refill_nested`).
    fn members(&mut self, members: &[u16], rerun: bool, refill: bool) -> Result<()> {
        // SAFETY: as `Leader`: `run_body` owns the entry, execution, binding, derive and recheck for its whole run and
        // unpublishes this frame (`_frame`) before any of them ends; `at`'s current is the innermost call's own, live across
        // that call, inside which this nested run runs; pool / plan / legs are `run_body`'s own borrows ('r). This thread only.
        let (e, x, b) = unsafe { self.at() };
        let (entry, exec, binding) = unsafe { (&mut *e, &mut *x, &mut *b) };
        // D1708 (a): passed on raw (`prerun_members`)
        let (derive, recheck, pool, plan, legs) = (self.derive, self.recheck, self.pool, self.plan, self.legs);
        let program = exec.node_mut().program().clone();
        let mut run = |e: &mut Entry, x: &mut Exec, bd: &mut Binding<'_>, p: usize| -> Result<()> {
            // D3014 S2' / D3137: a nested member over a big scan runs as a section (`nested_section`)
            let lazy = !nested_section(plan, &program, p, rerun);
            // SAFETY: `run_body`'s derive and hook, no `&mut` of either live across this arm
            match unsafe { run_arm(e, x, bd, derive, pool, plan, recheck, (p, legs, lazy)) }? {
                Arm::Ran(_) => plan.after_pipeline(p, x),
                Arm::Return(_) => Err(Error::BadReturn(gravity_abi::status::ERROR)),
            }
        };
        match (rerun, refill) {
            (false, _) => entry.run_nested(exec, binding, members, &mut run),
            (true, false) => {
                plan.reopen(members)?; // D2289: per-open state reset first
                entry.rerun_nested(exec, binding, members, &mut run)
            }
            (true, true) => {
                plan.reopen(members)?;
                entry.refill_nested(exec, binding, members, &mut run)
            }
        }
    }
}

/// D3014 S2' (Form I, inner widening), D3137: whether member `p` of a nested run (a rerun's, `rerun`: this rescan; an
/// Init's first run) runs as a section. Its pipeline is eligible (pool-safe code: a VOLATILE or non-'s' call keeps it
/// serial), drives a site and folds into no Init cell (`folds_init_cell`), its width is 2 or more, and its claims (a
/// rerun's taken again from the start, `MorselSource::rescan`) hold a granule's rows per worker; else it runs on the
/// leader. `PGRUST_GRAVITY_RERUN_SECTIONS=0` keeps every nested member serial (`Plan::rerun_sections`).
pub fn nested_section(plan: &dyn Plan, program: &gravity_program::Program, p: usize, rerun: bool) -> bool {
    let width = plan.width_of(p);
    if !plan.rerun_sections() || width < 2 || !program.pipelines[p].is_eligible() || program.pipelines[p].driving_site < 0 || folds_init_cell(program, p) {
        return false;
    }
    let claims = plan.claims_for(p);
    if rerun {
        claims.rescan();
    }
    // a section per rescan pays its open and close every outer row: only an inner of at least a granule's rows per worker
    // (8,192: a pgrc granule; a heap block holds ~128) widens, so a many-row outer over a small inner stays serial
    let per_worker = match claims.unit() {
        Unit::Granule => 1,
        Unit::Block => RERUN_BLOCKS_PER_WORKER,
        Unit::Partition => return false,
    };
    let section = claims.total() >= width as u64 * per_worker;
    if section {
        fires::fire(if rerun { "exec.rerun.section" } else { "exec.nested.section" });
        if gravity_driver::exists_cell(program, p) {
            fires::fire("exec.exists.section");
        }
    }
    section
}

/// D3137: pipeline `p` is an Init's own (its rows fold into the Init's cell on the leader, as `prerun_members` runs it),
/// but an EXISTS Init's (D2998, `INIT_EXISTS`: a row stores true and stops the claims, the runtime set false first).
fn folds_init_cell(program: &gravity_program::Program, p: usize) -> bool {
    program.v4.as_deref().is_some_and(|d| d.protos.iter().any(|q| q.kind == gravity_program::protos::ProtoKind::Init && q.pipeline as usize == p && q.arg & gravity_abi::limits::INIT_EXISTS == 0))
}

/// D3014 S2': the heap blocks per worker a rerun's section needs (64 blocks ~ a granule's 8,192 rows at ~128 per block).
const RERUN_BLOCKS_PER_WORKER: u64 = 64;

/// D3140: `p`, an arm run's last arm, ran on the leader (a join's drain) after the run's sections scattered their partials
/// to the slot's files: the pool's owners fold the run's records (`own_pooled` over the files and the leader's table) at
/// the run's widest section width, not one owner on this thread, once they pass what the leader folds alone
/// (`leader_owns_bytes`). `false`: not taken, or the owners' regions found no room (`own_single_of` runs).
#[allow(clippy::too_many_arguments)]
fn own_run_end(entry: &mut Entry, exec: &mut Exec, binding: &Binding<'_>, derive: &mut dyn Derive, pool: &dyn Pool, plan: &dyn Plan, (p, legs): (usize, bool), merge: &dyn Merge) -> Result<bool> {
    let Some(run) = exec.node().arm_run(p) else { return Ok(false) };
    let program = exec.node().program().clone();
    let width = run.filter(|&q| q != p).filter_map(|q| Section::plan(&program, q, plan.width_of(q), legs)).map(|s| s.width).max().unwrap_or(0);
    if width < 2 || entry.owner_held_of(exec, p) <= leader_owns_bytes() || gravity_abi::ctx::abort_set(binding.parts.abort_flag) {
        return Ok(false);
    }
    if phases_on() {
        census_take(&OWN_CENSUS);
    }
    entry.open_unclaimed_of(exec, p)?;
    let open = entry.open_owners(exec, binding, derive, p, width)?;
    let r = match open.own_past_grant(merge) {
        Ok(Some(pbits)) => {
            if census_on() {
                eprintln!("OWN pipeline={p} width={width} owners=run_end owner_bytes={} pbits={pbits}", open.owner_bytes());
            }
            own_pooled(&open, pool, merge, pbits, 2).map(|()| true).map_err(|f| f.error)
        }
        Ok(None) => Ok(false),
        Err(e) => Err(e),
    };
    entry.close_owners(open);
    if phases_on() {
        let (w, o) = (width as f64 * 1e6, census_take(&OWN_CENSUS));
        eprintln!("PHASES pipeline={p} width={width} run_end owners={} own/thr: zero={:.2} fold={:.2} reduce={:.2} parts={} recs={}", r.as_ref().is_ok_and(|&b| b), o[0] as f64 / w, o[1] as f64 / w, o[2] as f64 / w, o[3], o[4]);
    }
    r
}

/// What one pipeline's arm did: its name (PHASES), or the statement's end (a recheck that cannot resume, on the last pipeline).
enum Arm {
    Ran(&'static str),
    Return(u64),
}

/// D824 (V4R-298): `q` (the next pipeline) stopped on `BuildWait` for lazy build `b`: `b` runs out of order through its own
/// width-1 arm (`run_arm`, a section never: `(b, legs, true)`) and its `after_pipeline`, between `q`'s stop and resume; a fed
/// build's producers run so before it (D1005, `Entry::build_wait`).
/// # Safety
/// As `run_arm`.
#[allow(clippy::too_many_arguments)]
unsafe fn build_wait<D: AsDerive + ?Sized>(entry: &mut Entry, exec: &mut Exec, binding: &mut Binding<'_>, derive: *mut D, pool: &dyn Pool, plan: &dyn Plan, recheck: *mut (dyn FnMut(&mut Entry, &mut Exec, &mut Binding<'_>, &mut D, u8) -> Result<bool> + '_), (b, legs): (usize, bool)) -> Result<()> {
    stack_depth_core::check_stack_depth_or_panic();
    let mut run = |e: &mut Entry, x: &mut Exec, bd: &mut Binding<'_>, p: usize| -> Result<()> {
        // D2816 / D3524: a pulled set or a rerun set fed into the build runs at its reader's pulls or its owner's reruns
        if skip_in_wait(e, x, plan, p)? {
            return Ok(());
        }
        // SAFETY: this call's contract
        match unsafe { run_arm(e, x, bd, derive, pool, plan, recheck, (p, legs, true)) }? {
            Arm::Ran(_) => plan.after_pipeline(p, x),
            Arm::Return(_) => Err(Error::BadReturn(gravity_abi::status::build_wait(0))),
        }
    };
    entry.build_wait(exec, binding, b, &mut run)
}

/// Pipeline `p`'s arm (`run_arm_now`) under its late key words' image (D899, `run_with_late`).
/// D1708 (a): `derive` and `recheck` are `run_body`'s, raw: a late step (`Plan::late_words`) may run an Init's members ahead
/// through the frame (`Leader::prerun_members`), whose section opens write the derive; a `&mut` of it held here would be
/// a protected borrow that write is foreign to.
/// # Safety
/// `derive` and `recheck` are `run_body`'s own, alive across the call and reached on this thread only; no `&mut` of either
/// is live across it but the ones derived from these pointers.
#[allow(clippy::too_many_arguments)]
unsafe fn run_arm<D: AsDerive + ?Sized>(entry: &mut Entry, exec: &mut Exec, binding: &mut Binding<'_>, derive: *mut D, pool: &dyn Pool, plan: &dyn Plan, recheck: *mut (dyn FnMut(&mut Entry, &mut Exec, &mut Binding<'_>, &mut D, u8) -> Result<bool> + '_), (p, legs, lazy): (usize, bool, bool)) -> Result<Arm> {
    // D3918: a later arm of a run that ended at its first arm (an empty pair pass) is stepped past unopened; D4072: on a
    // leader-only run too (`lazy`: a nested member at W0 or over a small scan), whose run reopened the sealed slot
    if p == exec.next_pipeline() && entry.skip_ended(exec)? {
        return Ok(Arm::Ran("ended"));
    }
    // SAFETY: this call's contract
    run_with_late(entry, exec, binding, plan, p, |e, x, b| unsafe { run_arm_now(e, x, b, derive, pool, plan, recheck, (p, legs, lazy)) }, || Arm::Ran("held"))
}

/// Pipeline `p`'s arm: a section at its width, else the width-1 arm of a producer, else the emit / site-less arm. `lazy`: a
/// lazy build run out of order (D824): its width-1 arm, never a section.
/// # Safety
/// As `run_arm`.
#[allow(clippy::too_many_arguments)]
unsafe fn run_arm_now<D: AsDerive + ?Sized>(entry: &mut Entry, exec: &mut Exec, binding: &mut Binding<'_>, derive: *mut D, pool: &dyn Pool, plan: &dyn Plan, recheck: *mut (dyn FnMut(&mut Entry, &mut Exec, &mut Binding<'_>, &mut D, u8) -> Result<bool> + '_), (p, legs, lazy): (usize, bool, bool)) -> Result<Arm> {
    let program = exec.node_mut().program().clone();
    let out_rows = program.layout.out_rows;
    let pipe = program.pipelines[p];
    let mut arm = "section";
    // D276: a producer the build-first join's empty build masked runs as if no claim reached it (the width-1 arm); so
    // does an exec-pruned arm (D2105, `Plan::skips_whole`)
    let empty = exec.runs_empty(p) || plan.skips_whole(p);
    match if empty || lazy { None } else { Section::plan(&program, p, plan.width_of(p), legs) } {
        // op 14: a failed pipeline whose merge defers its error (JOIN's outer-first build) is held and passed (`defer_failed`)
        Some(section) => {
            // SAFETY: this call's contract; the borrow ends with the section, whose claims make no nested run (an Init a
            // section reads was run ahead of it, D1035 step 2)
            if let Err(e) = run_section(entry, exec, binding, unsafe { (*derive).as_derive() }, &section, plan.claims_for(p), pool, plan.merge_for(p)) {
                entry.defer_failed(exec, binding, plan.merge_for(p), p, e).or_else(|e| entry.defer_fed(exec, binding, &|q| plan.merge_for(q), p, e))?;
            }
        }
        // the width-1 arm of a producer: an eligible one runs claim by claim on the leader and
        // consults the merge's bound after each (decision 13: the ordered walk stops early serially too);
        // a serial source (an index scan, a sorted pgrc scan — decompose.md decision 81) has no claims
        // and runs its whole relation in one call, then the same owner fold and finish as any producer
        None if pipe.driving_site >= 0 && pipe.produces().is_some() => {
            arm = if empty { "empty" } else { "serial" };
            let merge = plan.merge_for(p);
            let ran: Result<()> = 'run: {
                if empty {
                    // no claim: the finish, the owner fold and the seal below, as over a relation pruned whole
                } else if pipe.is_eligible() {
                    let claims = plan.claims_for(p);
                    claims.reset();
                    let mut first = pipe.first_call;
                    while let Some(claim) = next_claim(claims, &mut first) {
                        if claims.beyond_bound(claim.lo) {
                            claims.close();
                            break;
                        }
                        match entry.run_claim(exec, binding, claim) {
                            Ok(Step::Suspended { .. }) => {
                                claims.close();
                                break;
                            }
                            // D824: a lazy build's probe runs leader-only over one claim (lowerv4 refuses an eligible one)
                            Ok(Step::BuildWait { .. }) => {
                                claims.close();
                                break 'run Err(Error::BadReturn(gravity_abi::status::build_wait(0)));
                            }
                            Ok(_) => {}
                            Err(e) => {
                                claims.close();
                                break 'run Err(e);
                            }
                        }
                        if let Some(b) = merge.bound(entry.leader_partial(exec)) {
                            claims.observe_bound(b);
                        }
                    }
                } else {
                    // D824: the claim stopped for its lazy build: the build runs, the same claim resumes at the row
                    let mut step = entry.run_claim(exec, binding, MorselRange::ALL);
                    while let Ok(Step::BuildWait { pipeline }) = step {
                        // SAFETY: this call's contract
                        if let Err(e) = unsafe { build_wait(entry, exec, binding, derive, pool, plan, recheck, (pipeline, legs)) } {
                            break 'run Err(e);
                        }
                        step = entry.resume_claim(exec, binding, MorselRange::ALL);
                    }
                    if let Err(e) = step {
                        break 'run Err(e);
                    }
                }
                if pipe.finish {
                    if let Err(e) = entry.run_claim(exec, binding, MorselRange::SECTION_END) {
                        break 'run Err(e);
                    }
                }
                Ok(())
            };
            // op 14: the run's error held and the pipeline passed when its merge defers it, else the statement's error
            if let Err(e) = ran {
                entry.defer_failed(exec, binding, merge, p, e).or_else(|e| entry.defer_fed(exec, binding, &|q| plan.merge_for(q), p, e))?;
            } else {
                // a record table: the owner fold on this thread (§3.2); else the merge's finish before
                // the seal: the leader's own partial becomes the consumer's table, a refusal (it does not
                // fit that table) scatters the partial whole and the seal finds the region empty
                // D393b: an arm that continues its union's run folds only (end_claims scatters a spilling end's records)
                let cont = exec.node().arm_continues(p);
                let fin: Result<()> = 'fin: {
                    let Some(slot) = pipe.produces().filter(|_| !cont) else { break 'fin Ok(()) };
                    // D292: a slot no claim reached is opened before the merge touches it
                    entry.open_unclaimed_of(exec, p)?;
                    if let Some(pbits) = entry.owner_bits_of(exec, exec.next_pipeline(), merge) {
                        if let Err(e) = entry.own_single(exec, merge, pbits) {
                            break 'fin Err(e);
                        }
                    }
                    let node = exec.node_mut();
                    match merge.finish(&mut node.leader_state(slot, binding.sink)) {
                        Ok(()) => Ok(()),
                        Err(Error::Unsupported(_)) if pipe.breaker().is_some_and(|b| matches!(b.spill, gravity_abi::breaker::Spill::Partitioned { .. })) => entry.spill_leader_partial(exec).map(|_| ()),
                        Err(e) => Err(e),
                    }
                };
                // op 14, fed (V4R-89): a feeder's fold or finish raising (an aggregate's combine) is held like its run's error
                match fin {
                    Ok(()) => entry.end_claims(exec)?,
                    Err(e) => entry.defer_fed(exec, binding, &|q| plan.merge_for(q), p, e)?,
                }
            }
        }
        None => {
            arm = "emit";
            // D2584: an emit its pruning removed (`Plan::skips_whole`: an Append arm streaming into the sink) runs empty,
            // never opened (`Exec::run_empty`), as the producer arm above skips a pruned producer
            if p < 64 && pipe.driving_site >= 0 && plan.skips_whole(p) && gravity_driver::is_emit(&program, p) {
                exec.run_empty(1 << p);
                arm = "empty";
            }
            let mut call = Call::batch(out_rows);
            let ran = loop {
                match entry.run_pipeline(exec, binding, call) {
                    Ok(Step::Done) => break Ok(()),
                    Ok(Step::Suspended { .. }) => call = Call::resume(out_rows),
                    Ok(Step::Recheck { site }) => {
                        // SAFETY: this call's contract; the hook's borrows end with its call
                        if !unsafe { (*recheck)(entry, exec, binding, &mut *derive, site) }? {
                            return if p + 1 >= program.pipelines.len() { Ok(Arm::Return(binding.rows_processed)) } else { Err(Error::Recheck(site)) };
                        }
                        call = Call::resume(out_rows);
                    }
                    // D824: the emit (or a site-less producer) stopped for its lazy build: the build runs, the call resumes
                    // SAFETY: this call's contract
                    Ok(Step::BuildWait { pipeline }) => match unsafe { build_wait(entry, exec, binding, derive, pool, plan, recheck, (pipeline, legs)) } {
                        Ok(()) => call = Call::resume(out_rows),
                        Err(e) => break Err(e),
                    },
                    Err(e) => break Err(e),
                }
            };
            // D763 (op 14): a site-less producer's error (a VALUES, set or table-function build, an aggregate's table) is
            // held as the serial arm holds a relation build's (`defer_failed`, else the deferring build it feeds)
            let held = match (ran, pipe.produces()) {
                (Err(e), Some(_)) => entry.defer_failed(exec, binding, plan.merge_for(p), p, e).or_else(|e| entry.defer_fed(exec, binding, &|q| plan.merge_for(q), p, e)).map(|()| true)?,
                (Err(e), None) => return Err(e),
                (Ok(()), _) => false,
            };
            if let Some(slot) = pipe.produces().filter(|_| !held && !exec.node().arm_continues(p)) { // finishes like the serial arm; a keyed site-less producer owns by index first (D109); an arm that continues its run folds only (D393b)
                let merge = plan.merge_for(p);
                if let Some(pbits) = entry.owner_bits_of(exec, p, merge) {
                    // SAFETY: this call's contract; the borrow ends with the owners (no nested run)
                    if !own_run_end(entry, exec, binding, unsafe { (*derive).as_derive() }, pool, plan, (p, legs), merge)? {
                        entry.own_single_of(exec, p, merge, pbits)?;
                    }
                }
                merge.finish(&mut exec.node_mut().leader_state(slot, binding.sink))?;
            }
        }
    }
    Ok(Arm::Ran(arm))
}

#[cfg(test)]
mod m8_frame_tests {
    use super::*;
    use core::cell::Cell;

    /// A plan that only keeps the frame published to it.
    struct Kept(Cell<Option<gravity_driver::Frame>>);

    impl Plan for Kept {
        fn width_of(&self, _p: usize) -> u16 {
            0
        }
        fn claims_for(&self, _p: usize) -> &dyn MorselSource {
            unreachable!("no claims in a frame test")
        }
        fn merge_for(&self, _p: usize) -> &dyn Merge {
            unreachable!("no merge in a frame test")
        }
        fn frame(&self, f: Option<gravity_driver::Frame>) -> Option<gravity_driver::Frame> {
            self.0.replace(f)
        }
    }

    /// A frame that counts the members it was asked to run.
    struct Count(usize);

    impl gravity_driver::OnDemand for Count {
        fn run_members(&mut self, members: &[u16]) -> Result<()> {
            self.0 += members.len();
            Ok(())
        }
    }

    fn at(f: Option<gravity_driver::Frame>) -> usize {
        f.map_or(0, |f| f.as_ptr() as *mut u8 as usize)
    }

    #[test]
    fn frames_stack_and_each_guard_puts_the_one_before_back_on_every_exit_an_unwind_too() {
        let plan = Kept(Cell::new(None));
        let (mut a, mut b) = (Count(0), Count(0));
        {
            let _a = publish(&plan, &mut a);
            let fa = at(plan.0.get());
            assert_ne!(fa, 0, "a is published");
            {
                let _b = publish(&plan, &mut b);
                assert!(at(plan.0.get()) != fa && at(plan.0.get()) != 0, "b is published over a");
            }
            assert_eq!(at(plan.0.get()), fa, "b's guard put a back");
            let r = std::panic::catch_unwind(core::panic::AssertUnwindSafe(|| {
                let mut c = Count(0);
                let _c = publish(&plan, &mut c);
                panic!("a panic inside a nested frame");
            }));
            assert!(r.is_err());
            assert_eq!(at(plan.0.get()), fa, "the unwind put a back");
            // SAFETY: the published frame is `a`, alive and not otherwise borrowed while its guard lives.
            unsafe { plan.0.get().expect("a").as_mut().run_members(&[1, 2, 3]).expect("a runs") };
        }
        assert!(plan.0.get().is_none(), "a's guard put none back");
        assert_eq!((a.0, b.0), (3, 0));
    }
}
