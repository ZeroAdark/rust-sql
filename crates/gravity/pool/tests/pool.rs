#[path = "../../hosts/tests/common/mod.rs"]
mod common;

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use common::*;
use gravity_abi::Datum;
use gravity_driver::{Binding, BindingParts, BorrowedBatch, CommandId, Entry, Error, Flow, Identity, Merge, PerTuple, Pins, Result, LegRef, Sink, SinkLeg, Snapshot};
use gravity_hosts::{HostRegistry, RangeRecipe};
use gravity_pool::{Ambient, RuntimePool};
use gravity_program::hand as programs;
use gravity_parallel::{run_program, BlockRange, ClaimSize, Job, MorselSource, Plan, PlainAggPartial, Pool};
use gravity_program::{Generation, Handle, PlanKey, Program};
use mcx::MemoryContext;
use runtime::{Runtime, RuntimeConfig, WorkerPool};
use types_rel::Relation;

static ABORT: AtomicU8 = AtomicU8::new(0);

/// The leader's wait loop calls the ambient interrupt check once per round, right before it registers its waker with the
/// section's completion and parks (`RuntimePool::run`): the count tells a job the leader has found the section unfinished and
/// is on its way to the park (D221).
static LEADER_ROUNDS: AtomicU64 = AtomicU64::new(0);

fn counted_interrupts() -> types_error::PgResult<()> {
    LEADER_ROUNDS.fetch_add(1, Ordering::AcqRel);
    Ok(())
}

/// The leader thread's `/proc` stat file (Linux), read by a pool worker to see the leader blocked.
fn leader_stat() -> Option<std::path::PathBuf> {
    let task = std::fs::read_link("/proc/thread-self").ok()?;
    Some(std::path::Path::new("/proc").join(task).join("stat"))
}

/// The thread is blocked (state S): after its interrupt round the leader's only blocking point is the park.
fn blocked(stat: &std::path::Path) -> bool {
    std::fs::read_to_string(stat).ok().and_then(|s| s.rsplit_once(')').and_then(|(_, rest)| rest.trim_start().chars().next())) == Some('S')
}

struct CountSink {
    rows: Mutex<Vec<Vec<Datum>>>,
    pushes: AtomicU64,
}

impl CountSink {
    fn take(&self, b: BorrowedBatch<'_>) -> Result<Flow> {
        self.pushes.fetch_add(1, Ordering::Relaxed);
        let mut rows = self.rows.lock().unwrap();
        for (v, _) in b.iter() {
            rows.push(v.to_vec());
        }
        Ok(Flow::Continue)
    }
}

impl Sink for CountSink {
    fn push(&mut self, b: BorrowedBatch<'_>) -> Result<Flow> {
        self.take(b)
    }
    fn leg(&self, _ordinal: u16) -> Option<LegRef<'_>> {
        Some(LegRef::of(self))
    }
}

impl SinkLeg for CountSink {
    fn push(&self, b: BorrowedBatch<'_>) -> Result<Flow> {
        self.take(b)
    }
}

struct RangePlan {
    width: u16,
    blocks: [BlockRange; 2],
}

impl Plan for RangePlan {
    fn width_of(&self, _p: usize) -> u16 {
        self.width
    }
    fn claims_for(&self, p: usize) -> &dyn MorselSource {
        &self.blocks[p]
    }
    fn merge_for(&self, _p: usize) -> &dyn Merge {
        &PlainAggPartial
    }
}

fn run(entry: &mut Entry, program: &Arc<Program>, reg: &mut HostRegistry, pool: &dyn Pool, width: u16, thr: u64, keyed: bool) -> Result<Vec<Vec<Datum>>> {
    let snap = static_mvcc_snapshot();
    let params = [thr];
    let nulls = [0u8];
    let parts = BindingParts { snapshot: Snapshot(&snap as *const _ as u64), command_id: CommandId(0), params: &params, param_nulls: &nulls, abort_flag: &ABORT, pins: Pins(0), per_tuple: PerTuple(0), identity: Identity::default(), resolved: None };
    let mut sink = CountSink { rows: Mutex::new(Vec::new()), pushes: AtomicU64::new(0) };
    let mut b = Binding::new(parts, &mut sink);
    let key = keyed.then_some(PlanKey { handle: Handle(7), generation: Generation(1) });
    let mut exec = entry.start(key, program, reg, &mut b, std::rc::Rc::new(gravity_driver::Reservation::unadmitted(gravity_driver::Grant::of(8 << 20))))?;
    let plan = RangePlan { width, blocks: [BlockRange::new(nblocks(HEAP_OID), ClaimSize::Fixed(1)), BlockRange::new(0, ClaimSize::Fixed(1))] };
    let r = run_program(entry, &mut exec, &mut b, reg, pool, &plan);
    entry.end(exec, r.is_ok());
    r?;
    Ok(sink.rows.into_inner().unwrap())
}

#[test]
fn sections_over_the_runtime_pool_give_the_serial_count_and_recover_from_errors() {
    let _g = serial();
    install();
    reset_fixture();
    let ctx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("pool-test")));
    let mcx = ctx.mcx();
    let values: Vec<i32> = (0..6000).collect();
    load_rows(mcx, &values);
    let total_blocks = nblocks(HEAP_OID);
    assert!(total_blocks >= 8, "{total_blocks} blocks");
    let rt = Runtime::new(RuntimeConfig::new(4));
    let threads = WorkerPool::spawn_std(Arc::clone(&rt)).expect("pool threads");
    let pool = RuntimePool::new(Arc::clone(&rt), Ambient { interrupts: counted_interrupts, ..Ambient::HARNESS });
    let heap = Relation::open(heap_relation_data(mcx), None);
    let recipe = RangeRecipe::from_plan(&heap, &[1]).unwrap();
    drop(heap);
    let mut reg = HostRegistry::new();
    reg.push_range(recipe);
    let program = Arc::new(programs::par_scan_count_with(HEAP_OID, 6000));
    let mut entry = Entry::new(2);
    let thr = 2500u64;
    for (i, width) in [1u16, 2, 4, 4].iter().enumerate() {
        reg.rewind();
        let before = pool.claims_run();
        let rows = run(&mut entry, &program, &mut reg, &pool, *width, thr, true).unwrap();
        assert_eq!(rows, vec![vec![thr]], "width {width}");
        assert_eq!(pins_held(), 0, "width {width}: pins after the section");
        if *width >= 2 {
            let claims = pool.claims_run() - before;
            assert!((1..=total_blocks).contains(&claims), "width {width}: {claims} claims over {total_blocks} blocks (adaptive sizing)");
        } else {
            assert_eq!(pool.claims_run() - before, 0);
        }
        assert_eq!(entry.stats().built, 1, "run {i}: the node is parked and reused");
    }
    assert_eq!(pool.sections(), 3);
    FAIL_BLOCK.store(3, Ordering::Relaxed);
    reg.rewind();
    let r = run(&mut entry, &program, &mut reg, &pool, 4, thr, true);
    assert_eq!(r, Err(Error::host("pg")));
    assert_eq!(gravity_hosts::take_error().map(|e| e.message().to_string()), Some("could not read block (fixture)".to_string()));
    assert_eq!(pins_held(), 0, "pins after a failed section");
    FAIL_BLOCK.store(u64::MAX, Ordering::Relaxed);
    ABORT.store(1, Ordering::Release);
    reg.rewind();
    let before = pool.claims_run();
    assert_eq!(run(&mut entry, &program, &mut reg, &pool, 4, thr, true), Err(Error::Cancelled));
    assert!(pool.claims_run() - before <= 4, "a byte set before the section: at most one claim per worker observes it");
    ABORT.store(0, Ordering::Release);
    reg.rewind();
    assert_eq!(run(&mut entry, &program, &mut reg, &pool, 2, thr, true).unwrap(), vec![vec![thr]], "the parked node serves again after a cancel");
    assert_eq!((entry.stats().built, entry.stats().ended_failed), (1, 2));
    // D2547: no wake count here. These sections hold no claim open, so on a loaded host each can finish before the leader
    // parks (flake#25's ct pool: 0 wakes, 0 quanta) - D221's race without D221's hold. The completion wake is asserted
    // below, where every claim holds its section open until the leader is parked.
    eprintln!("sections: {} completion wakes, {} quanta", pool.wakes(), pool.quanta());

    // §1.10 increment 3 item 2: a section's completion wakes the leader — the
    // gap between the last claim's end and `run` returning is microseconds,
    // not a park quantum (1 ms) — and a byte set mid-section still cancels.
    // D221: an empty claim could finish before the leader ever parked (a gate box under load: 0 of 40 sections ended by a
    // wake), and then there is no wake to count. Each claim now holds the section open until the leader is parked: it has
    // run an interrupt round (it found the section unfinished) and is blocked (Linux; elsewhere a 200 µs hold after the round).
    let last_end: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    let round0 = AtomicU64::new(0);
    let stat = leader_stat();
    let job = |_ordinal: u16, _range: gravity_abi::MorselRange| -> Result<()> {
        let t0 = std::time::Instant::now();
        while LEADER_ROUNDS.load(Ordering::Acquire) == round0.load(Ordering::Acquire) {
            assert!(t0.elapsed() < std::time::Duration::from_secs(10), "the leader never ran an interrupt round");
            std::hint::spin_loop();
        }
        match &stat {
            Some(stat) => {
                while !blocked(stat) {
                    assert!(t0.elapsed() < std::time::Duration::from_secs(10), "the leader never parked");
                    std::hint::spin_loop();
                }
            }
            None => std::thread::sleep(std::time::Duration::from_micros(200)),
        }
        *last_end.lock().unwrap() = Some(std::time::Instant::now());
        Ok(())
    };
    let job: &Job<'_> = &job;
    let mut gaps = Vec::new();
    let (wakes0, quanta0) = (pool.wakes(), pool.quanta());
    for _ in 0..40 {
        let claims = BlockRange::new(4, ClaimSize::Fixed(1));
        round0.store(LEADER_ROUNDS.load(Ordering::Acquire), Ordering::Release);
        pool.run(4, &claims, job).unwrap();
        let end = last_end.lock().unwrap().take().expect("a claim ran");
        gaps.push(end.elapsed());
    }
    gaps.sort();
    let median = gaps[gaps.len() / 2];
    let wakes = pool.wakes() - wakes0;
    eprintln!("completion wake: median gap {median:?}, min {:?}, max {:?}, wakes {wakes}/40, quanta {}", gaps[0], gaps[gaps.len() - 1], pool.quanta() - quanta0);
    assert!(median < std::time::Duration::from_millis(1), "median gap {median:?} is not under the 1 ms park quantum: the completion did not wake the leader");
    assert!(wakes >= 20, "{wakes} of 40 sections ended by a wake");
    quiesced();
    threads.shutdown();
}

/// A pool worker carries the statement's backend state (encoding, default locale, session
/// timezone) from the leader's stamp, re-armed when the stamp changes.
#[test]
fn pool_workers_carry_the_statement_stamp() {
    let _g = serial();
    install();
    const PG_UTF8: i32 = 6;
    const PG_SQL_ASCII: i32 = 0;
    let rt = Runtime::new(RuntimeConfig::new(2));
    let _threads = WorkerPool::spawn_std(Arc::clone(&rt)).expect("pool threads");
    let pool = RuntimePool::new(Arc::clone(&rt), Ambient::HARNESS);
    let seen: Arc<Mutex<Vec<gravity_hosts::StatementArm>>> = Arc::new(Mutex::new(Vec::new()));
    let probe = |leader: gravity_hosts::StatementArm| {
        let mine = Arc::clone(&seen);
        let before = seen.lock().unwrap().len();
        assert!(pool.spawn(Box::new(move || mine.lock().unwrap().push(gravity_hosts::StatementArm::capture()))));
        let t0 = std::time::Instant::now();
        while seen.lock().unwrap().len() == before {
            assert!(t0.elapsed() < std::time::Duration::from_secs(10), "the background job never ran");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let worker = seen.lock().unwrap().last().unwrap().clone();
        assert_eq!(worker, leader);
        worker
    };
    gravity_hosts::arm_database_encoding(PG_UTF8).unwrap();
    assert_eq!(probe(gravity_hosts::StatementArm::capture()).encoding(), PG_UTF8);
    gravity_hosts::arm_database_encoding(PG_SQL_ASCII).unwrap();
    assert_eq!(probe(gravity_hosts::StatementArm::capture()).encoding(), PG_SQL_ASCII);
}

/// D278: an ordered section (R1D's `ClaimOrder`, window 2 x W) on a 16-thread pool at W below 16. Every awake pool thread
/// takes claims; when a claim was admitted only after its lease, the W slot holders parked for room while the head's
/// thread waited in `lease` for a slot, and each round ended only at ROOM_WAIT (20 ms): an ordered scan at W2 took 2.7 s for 284
/// claims (this test at the parent: W2 2.3 s, 226 of 246 claims waited out ROOM_WAIT). The pool now admits before the
/// lease, so a claim waiting for room holds no slot: a burst of claims ends in claim order, far under claims x ROOM_WAIT,
/// with (next to) no ROOM_WAIT expiry, at every W from 1 to 16; the admitted and finished claims stay within the window
/// + W (+ the head), narrowed or not; every admitted claim settles.
#[test]
fn ordered_claims_below_the_pool_width_never_wait_out_the_room_timeout() {
    let _g = serial();
    install();
    const CLAIMS: u64 = 256;
    // D2549: the orders here wait ROOM_WAIT for room (ClaimOrder::with_room_wait), not production's 20 ms: on a loaded
    // host a 20 ms room wait expired with no D278 inversion at all (1-9 expiries, x64 copies + 64 hogs). A claim must
    // leave a room wait by a release, never by the timeout; the inversion still shows as expiries and a 16-wait stall.
    const ROOM_WAIT: std::time::Duration = std::time::Duration::from_secs(2);
    let rt = Runtime::new(RuntimeConfig::new(16));
    let threads = WorkerPool::spawn_std(Arc::clone(&rt)).expect("pool threads");
    let pool = RuntimePool::new(Arc::clone(&rt), Ambient::HARNESS);
    for (width, narrow) in (1u16..=16).map(|w| (w, false)).chain([(2, true), (8, true)]) {
        let claims = BlockRange::new(CLAIMS, ClaimSize::Fixed(1));
        let order = gravity_driver::ClaimOrder::new(2 * width as usize, width as usize, claims.first_live_at(0)).with_room_wait(ROOM_WAIT);
        let ran: Mutex<Vec<u64>> = Mutex::new(Vec::new());
        // claims started and not yet released by the leader (the finished ones buffer their rows), and its peak
        let (live, peak) = (AtomicU64::new(0), AtomicU64::new(0));
        let job = |_ordinal: u16, claim: gravity_abi::MorselRange| -> Result<()> {
            peak.fetch_max(live.fetch_add(1, Ordering::AcqRel) + 1, Ordering::AcqRel);
            let t = std::time::Instant::now();
            while t.elapsed() < std::time::Duration::from_micros(20) {
                std::hint::spin_loop();
            }
            ran.lock().unwrap().push(claim.lo);
            Ok(())
        };
        // `ready` bridges each released claim under the order's lock, before it wakes a parked claim: the release counted there
        let released = |u: u64| {
            live.fetch_sub(1, Ordering::AcqRel);
            claims.first_live_at(u)
        };
        let mut emitted = Vec::new();
        let t0 = std::time::Instant::now();
        {
            let mut lead = || {
                if narrow {
                    order.narrow();
                }
                order.ready(&released, &mut emitted)
            };
            pool.run_leading(width, &claims, &job, &mut lead, &order).unwrap();
        }
        let wall = t0.elapsed();
        // the last claims can finish after the leader's last round: the emit's tail is the leader's after the section
        order.ready(&released, &mut emitted);
        let mut ran = ran.into_inner().unwrap();
        ran.sort_unstable();
        let (waits, overruns, peak) = (order.waits.load(Ordering::Relaxed), order.overruns.load(Ordering::Relaxed), peak.load(Ordering::Relaxed));
        eprintln!("D278 W{width}{}: {} claims in {wall:?}, {waits} room waits, {overruns} ROOM_WAIT expiries, peak {peak} buffered", if narrow { " narrowed" } else { "" }, ran.len());
        assert_eq!(emitted, ran, "W{width}: every claim emitted once, in claim order");
        assert!(!ran.is_empty() && ran[0] == 0, "W{width}: the claims start at the head");
        assert_eq!(order.admitted(), 0, "W{width}: every admitted claim settled");
        assert!(peak <= 3 * width as u64 + 1 + overruns, "W{width}: {peak} claims buffered past the window + W + the head");
        if !narrow {
            assert!(wall < ROOM_WAIT * (CLAIMS as u32) / 16, "W{width}: {wall:?} for {} claims: a stall bound by ROOM_WAIT", ran.len());
            assert!(overruns <= CLAIMS / 64, "W{width}: {overruns} claims waited out ROOM_WAIT for room ({waits} waited)");
        }
    }
    threads.shutdown();
}

/// D278: with the claims admitted before their lease, the ordered section's stops still hold and every admitted claim
/// settles. A LIMIT (the leader's `stop` + close) ends the claim stream. A failed claim ends the order (nothing after it
/// is emitted) and the claims parked for room leave at once instead of at ROOM_WAIT; claims admitted but refused their
/// lease (the section failed) and a claim that panics settle as failed.
#[test]
fn ordered_claims_admitted_before_the_lease_still_stop_and_settle() {
    let _g = serial();
    install();
    // D2549: room waits end by a stop or a failure, never by the timeout (see the test above).
    const ROOM_WAIT: std::time::Duration = std::time::Duration::from_secs(2);
    const CLAIMS: u64 = 256;
    // D704: the runtime's sizer does not honour ClaimSize::Fixed(1): it ramps from startup_c0 = 1, and an early claim
    // slower than ~270 us ends the ramp, so 5-7 large claims covered the 256 units and the LIMIT at 8 emitted claims
    // never fired (W1, 19% under load). t_max = t_min = 1 ns pins one-unit claims, the 256 the sub-cases are written for.
    let mut config = RuntimeConfig::new(16);
    config.sizing = runtime::SizingParams { t_max_ns: 1, t_min_ns: 1 };
    let rt = Runtime::new(config);
    let threads = WorkerPool::spawn_std(Arc::clone(&rt)).expect("pool threads");
    let pool = RuntimePool::new(Arc::clone(&rt), Ambient::HARNESS);
    let spin = |us: u64| {
        let t = std::time::Instant::now();
        while t.elapsed() < std::time::Duration::from_micros(us) {
            std::hint::spin_loop();
        }
    };
    for width in [1u16, 2, 8] {
        // LIMIT: the leader stops once 8 claims are out; unstopped, the claims take >= 256 x 200 us / W
        let claims = BlockRange::new(CLAIMS, ClaimSize::Fixed(1));
        let order = gravity_driver::ClaimOrder::new(2 * width as usize, width as usize, claims.first_live_at(0)).with_room_wait(ROOM_WAIT);
        let ran = AtomicU64::new(0);
        let job = |_ordinal: u16, _claim: gravity_abi::MorselRange| -> Result<()> {
            spin(200);
            ran.fetch_add(1, Ordering::Relaxed);
            Ok(())
        };
        let mut emitted = Vec::new();
        {
            let mut lead = || {
                order.ready(&|u| claims.first_live_at(u), &mut emitted);
                if emitted.len() >= 8 {
                    order.stop();
                    claims.close();
                }
            };
            pool.run_leading(width, &claims, &job, &mut lead, &order).unwrap();
        }
        let ran = ran.load(Ordering::Relaxed);
        assert!(emitted.len() >= 8 && emitted.windows(2).all(|w| w[0] < w[1]), "W{width}: {emitted:?}");
        assert!(ran < CLAIMS / 2, "W{width}: {ran} of {CLAIMS} claims ran after the LIMIT stopped the stream");
        assert_eq!(order.admitted(), 0, "W{width}: every admitted claim settled");
        assert!(!order.admit(gravity_abi::MorselRange::new(CLAIMS - 1, CLAIMS)), "W{width}: no claim is admitted after the stop");

        // a failed claim: the section fails, nothing after it is emitted, no claim waits out ROOM_WAIT; once it failed,
        // every claim that runs holds its slot 2 ms, so the admitted claims still waiting for one are refused their lease
        let claims = BlockRange::new(CLAIMS, ClaimSize::Fixed(1));
        let order = gravity_driver::ClaimOrder::new(2 * width as usize, width as usize, claims.first_live_at(0)).with_room_wait(ROOM_WAIT);
        let failed = std::sync::atomic::AtomicBool::new(false);
        let job = |_ordinal: u16, claim: gravity_abi::MorselRange| -> Result<()> {
            if claim.lo <= 40 && 40 < claim.hi {
                failed.store(true, Ordering::Release);
                return Err(Error::host("d278 fixture"));
            }
            spin(if failed.load(Ordering::Acquire) { 2000 } else { 20 });
            Ok(())
        };
        let mut emitted = Vec::new();
        let t0 = std::time::Instant::now();
        let r = {
            let mut lead = || order.ready(&|u| claims.first_live_at(u), &mut emitted);
            pool.run_leading(width, &claims, &job, &mut lead, &order)
        };
        let wall = t0.elapsed();
        order.ready(&|u| claims.first_live_at(u), &mut emitted);
        assert_eq!(r.map_err(|e| e.error), Err(Error::host("d278 fixture")), "W{width}");
        assert!(emitted.windows(2).all(|w| w[0] < w[1]) && emitted.iter().all(|&lo| lo <= 40), "W{width}: {emitted:?}");
        assert_eq!(order.admitted(), 0, "W{width}: every admitted claim settled (refused leases included)");
        assert_eq!(order.overruns.load(Ordering::Relaxed), 0, "W{width}: a claim parked for room waited out ROOM_WAIT after the failure");
        assert!(wall < std::time::Duration::from_millis(500), "W{width}: {wall:?}");

        // a claim that panics settles as failed on the unwind: the order ends at it
        let claims = BlockRange::new(CLAIMS, ClaimSize::Fixed(1));
        let order = gravity_driver::ClaimOrder::new(2 * width as usize, width as usize, claims.first_live_at(0)).with_room_wait(ROOM_WAIT);
        let job = |_ordinal: u16, claim: gravity_abi::MorselRange| -> Result<()> {
            assert!(!(claim.lo <= 40 && 40 < claim.hi), "d278 fixture: a claim panics");
            spin(20);
            Ok(())
        };
        let mut emitted = Vec::new();
        let r = {
            let mut lead = || order.ready(&|u| claims.first_live_at(u), &mut emitted);
            // unwind-ok: test
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| pool.run_leading(width, &claims, &job, &mut lead, &order)))
        };
        order.ready(&|u| claims.first_live_at(u), &mut emitted);
        // D2933: the claim's bug panic is C's abort(): the section completes (failed), then the leader PANICs
        let payload = r.expect_err("a claim's bug panic PANICs the leader");
        assert!(payload.is::<types_error::PanicExitThread>(), "W{width}: the leader's PANIC, not XX000");
        assert!(emitted.windows(2).all(|w| w[0] < w[1]) && emitted.iter().all(|&lo| lo <= 40), "W{width}: {emitted:?}");
        assert_eq!(order.admitted(), 0, "W{width}: the panicked claim settled");
    }
    threads.shutdown();
}

/// D2566: a failed claim leaves its ordinal's partial state as its kernel left it (a var sort run compacted for a put that
/// failed is neither put nor cleared); no later claim of the section may run on that ordinal. The W slot holders here fail
/// together, as workers do when their spills cross the session's temp_file_limit at once, so their `fail` calls contend.
/// Before, the slot went back before the section was marked failed, and a claim waiting for a slot leased it in between:
/// e2e corr_d2350_d2351_worker_carry at W8 under load re-compacted a compacted run (SIGSEGV in RunShape::compact).
#[test]
fn no_claim_runs_on_the_ordinal_of_a_failed_claim() {
    let _g = serial();
    install();
    const SECTIONS: usize = 400;
    let mut config = RuntimeConfig::new(16);
    config.sizing = runtime::SizingParams { t_max_ns: 1, t_min_ns: 1 };
    let rt = Runtime::new(config);
    let threads = WorkerPool::spawn_std(Arc::clone(&rt)).expect("pool threads");
    let pool = RuntimePool::new(Arc::clone(&rt), Ambient::HARNESS);
    let mut after = 0;
    for s in 0..SECTIONS {
        let width = [2u16, 4, 8][s % 3];
        let claims = BlockRange::new(256, ClaimSize::Fixed(1));
        let poisoned: Vec<std::sync::atomic::AtomicBool> = (0..width).map(|_| std::sync::atomic::AtomicBool::new(false)).collect();
        let ran_after = AtomicU64::new(0);
        let job = |ordinal: u16, _claim: gravity_abi::MorselRange| -> Result<()> {
            if poisoned[ordinal as usize].load(Ordering::Acquire) {
                ran_after.fetch_add(1, Ordering::Relaxed);
            }
            let t = std::time::Instant::now();
            while t.elapsed() < std::time::Duration::from_micros(2) {
                std::hint::spin_loop();
            }
            poisoned[ordinal as usize].store(true, Ordering::Release);
            Err(gravity_hosts::stash(Box::new(types_error::PgError::error("d2566 fixture: a claim fails"))))
        };
        assert!(pool.run(width, &claims, &job).is_err(), "section {s}");
        after += ran_after.load(Ordering::Relaxed);
    }
    threads.shutdown();
    assert_eq!(after, 0, "{after} claims ran on an ordinal whose claim had failed ({SECTIONS} sections)");
}
