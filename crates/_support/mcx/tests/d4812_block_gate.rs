//! D4812 (2), the block gate (`ladder_block_gate`): a statement's memory can rise past the watchdog's cancel line between
//! two ladder passes (a 100 ms pass saw rises of up to 390 MiB under load, and a stalled pass none at all), and the next
//! pass then finds the kernel's limit already reached. The gate acts at the allocation itself: a memory-context block of
//! at least 1 MiB that takes the gate's projection (the ladder's read at the last sample plus the context bytes taken
//! since) past the cancel line waits a bounded time for the projection to fall, then fails with 53200 when its
//! statement is the one R6 would cancel. Here no ladder pass runs at all: the sample is published once by hand, and a
//! grower takes 8 MiB blocks. The gate's other witnesses: it runs no trim, reclaim, purge or cancel of another
//! statement on the allocating thread (the ladder pass is woken for them); an innocent asker past the line waits its
//! bound and takes its block while the statement R6 would pick is the one refused, and that refusal is the one cancel in
//! flight, and it leaves no condemnation behind; a block with interrupts held off (every LWLock holder) is taken at
//! once, counted; a pending interrupt ends the wait within one step. Named cancel_* for the crate tests' serial tail (the
//! gate's wait reads the clock).
#![cfg(feature = "std")]

use std::alloc::Layout;
use std::cell::Cell;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

use mcx::ladder_stats::{self, Outcome, Site};
use mcx::{governor, ladder, MemoryContext};
use mempolicy::consts::{set, K};
use mempolicy::ladder::Rung;

const MIB: usize = 1 << 20;
const BLOCK: usize = 8 * MIB;
/// The last sample: the ladder's read, the ladder's line and the watchdog's cancel line.
const RESIDENT: usize = 100 * MIB;
const LINE: u64 = 500 * MIB as u64;
const CANCEL_LINE: u64 = 600 * MIB as u64;
/// Where the grower stops on its own: past the kernel's limit in this shape.
const GROWN_MAX: usize = 1024 * MIB;
/// The gate's bound here (`cancel_landing_ms`): short, so the test does not wait the default 2.75 s.
const BOUND_MS: u64 = 100;

/// The tests of this binary share the governor's published sample.
static ONE: Mutex<()> = Mutex::new(());

/// One test at a time, after the earlier one's cancel has landed (`cancel_landing_ms` = `BOUND_MS`): no cancel in flight.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    let g = ONE.lock().unwrap_or_else(|e| e.into_inner());
    std::thread::sleep(Duration::from_millis(3 * BOUND_MS));
    set(K::LadderBlockGate, 1);
    set(K::CancelLandingMs, BOUND_MS);
    g
}

/// No sample past any line, the gate off, R3's flag down.
fn calm() {
    set(K::LadderBlockGate, 0);
    ladder::publish(ladder::OFF, 0);
    ladder::publish_cancel_line(0);
    let fp = mcx::global_footprint::bytes();
    governor::publish(0, 0, fp);
    governor::publish_gate(0, fp);
    ladder::clear_narrow();
}

/// A sample with the gate on whose ladder read is `room` bytes under the cancel line: the next block of more than `room`
/// bytes, anywhere in the process, takes the projection past it. Published as memruntime publishes it with the bar off
/// (the shipped default): the governor's sample with no resident reading, then the gate's own (the ladder's read).
fn sample_short_of_the_line(room: usize) {
    ladder::publish(ladder::ON, LINE);
    ladder::publish_cancel_line(CANCEL_LINE);
    let fp = mcx::global_footprint::bytes();
    governor::publish(0, 0, fp);
    governor::publish_gate(CANCEL_LINE as usize - room, fp);
}

/// The block gate's episodes since `seq` (the ring's): (outcome, the rungs that ran).
fn block_episodes(seq: u64) -> Vec<(Outcome, u16)> {
    let mut v = Vec::new();
    ladder_stats::for_each_episode(&mut |e| {
        if e.seq > seq && e.site == Site::Block {
            v.push((e.outcome, e.ran));
        }
    });
    v
}

fn last_seq() -> u64 {
    let mut n = 0;
    ladder_stats::for_each_episode(&mut |e| n = n.max(e.seq));
    n
}

/// One block of `BLOCK` bytes from `ctx`: Ok, refused by the allocator, or the error a block raised.
fn one_block(ctx: &MemoryContext) -> Result<(), Option<Box<types_error::PgError>>> {
    let block = Layout::from_size_align(BLOCK, 8).unwrap();
    match catch_unwind(AssertUnwindSafe(|| ctx.mcx().alloc_uninit_bytes(block))) {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(_)) => Err(None),
        Err(payload) => Err(Some(*payload.downcast::<Box<types_error::PgError>>().expect("the governor's payload is a PgError"))),
    }
}

/// A small chunk from `ctx` (under the gate's least block): whether it was taken with no error.
fn small(ctx: &MemoryContext) -> bool {
    let chunk = Layout::from_size_align(64, 8).unwrap();
    matches!(catch_unwind(AssertUnwindSafe(|| ctx.mcx().alloc_uninit_bytes(chunk))), Ok(Ok(_)))
}

/// Publish one sample by hand, as the watchdog's would be, and never another: the ladder on at `LINE`, the cancel line
/// at `CANCEL_LINE`, the ladder's read `RESIDENT` with the context bytes taken now. Published as memruntime publishes it
/// with the bar off: the governor's sample with no resident reading, then the gate's own.
fn sample() {
    mcx::ladder::publish(mcx::ladder::ON, LINE);
    mcx::ladder::publish_cancel_line(CANCEL_LINE);
    let fp = mcx::global_footprint::bytes();
    governor::publish(0, 0, fp);
    governor::publish_gate(RESIDENT, fp);
}

/// How a grower's blocks ended.
#[derive(Debug)]
enum End {
    /// It took every block up to its own stop.
    Max,
    /// The allocator itself refused a block.
    Refused,
    /// A block raised this error (the governor's 53200).
    Raised(Box<types_error::PgError>),
}

/// Blocks of `BLOCK` bytes from `ctx` until one fails or `max` bytes were taken: the bytes taken, and how it ended.
fn grow(ctx: &MemoryContext, max: usize) -> (usize, End) {
    let block = Layout::from_size_align(BLOCK, 8).unwrap();
    let mut grown = 0;
    while grown < max {
        match catch_unwind(AssertUnwindSafe(|| ctx.mcx().alloc_uninit_bytes(block))) {
            Ok(Ok(_)) => grown += BLOCK,
            Ok(Err(_)) => return (grown, End::Refused),
            Err(payload) => return (grown, End::Raised(*payload.downcast::<Box<types_error::PgError>>().expect("the governor's payload is a PgError"))),
        }
    }
    (grown, End::Max)
}

#[test]
fn cancel_d4812_a_grower_past_the_cancel_line_fails_at_its_block_with_no_ladder_pass() {
    let _one = serial();
    std::thread::spawn(|| {
        set(K::LadderBlockGate, 1);
        set(K::CancelLandingMs, BOUND_MS);
        governor::register(4100, 41_000);
        governor::statement_start(0, true);
        let ctx = MemoryContext::new("d4812 grower");
        sample();
        governor::arm(true);
        let (grown, end) = grow(&ctx, GROWN_MAX);
        governor::arm(false);
        drop(ctx);
        governor::statement_end();
        // the first block past the line: RESIDENT + grown + BLOCK > CANCEL_LINE
        let past = CANCEL_LINE as usize - RESIDENT;
        let End::Raised(e) = end else {
            panic!(
                "the grower took {} MiB, {} MiB past the cancel line, and ended {end:?} with no ladder pass (D4812: the pass is the only gate)",
                grown / MIB,
                (RESIDENT + grown).saturating_sub(CANCEL_LINE as usize) / MIB
            )
        };
        assert_eq!(e.sqlstate(), types_error::ERRCODE_OUT_OF_MEMORY, "{e:?}");
        assert!(grown + BLOCK > past && grown <= past, "it failed at its first block past the line: grown {} MiB", grown / MIB);
    })
    .join()
    .unwrap();
    calm();
}

std::thread_local! {
    /// This thread is the gate's asker (an allocating statement's thread).
    static ASKER: Cell<bool> = const { Cell::new(false) };
    /// This thread holds interrupts off (as HoldInterrupts, every LWLock acquire, would).
    static HELD: Cell<bool> = const { Cell::new(false) };
}

/// The live rungs' work an asker's thread did, by actuator (`ACTUATORS`' order).
static ON_ASKER: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];
const ACTUATORS: [&str; 6] = ["read (a cgroup read)", "reclaim (R2a)", "cancel (R6)", "evict_protected (R2b)", "alloc_return (R1)", "trim_parked (R2p)"];
/// The ladder pass's wakes (memledger's futex wake hook).
static WAKES: AtomicU64 = AtomicU64::new(0);
/// An interrupt pending (as InterruptPending, raised by another thread's cancel), for the gate's interrupt probe.
static PENDING: AtomicBool = AtomicBool::new(false);

fn on_asker(i: usize) {
    if ASKER.with(Cell::get) {
        ON_ASKER[i].fetch_add(1, SeqCst);
    }
}

fn fake_read() -> Option<u64> {
    on_asker(0);
    Some(CANCEL_LINE + 64 * MIB as u64)
}

/// Actuators with bytes to give at every rung, so a climb that ran any of them here would be seen.
static ACTS: ladder::Acts = ladder::Acts {
    read: fake_read,
    reclaimable: || 1 << 30,
    reclaim: |n| {
        on_asker(1);
        n
    },
    cancel: |_| {
        on_asker(2);
        false
    },
    cancel_in_flight: || false,
    protected: || 1 << 30,
    evict_protected: |n| {
        on_asker(3);
        n
    },
};

fn fakes() {
    ladder::install(&ACTS);
    mcx::set_cache_give_back(|| on_asker(4));
    ladder::set_parked_trim(
        || 1 << 30,
        |n, _| {
            on_asker(5);
            n
        },
    );
    memledger::arena::set_wake_hook(|| {
        WAKES.fetch_add(1, SeqCst);
    });
    governor::set_sleep_probe(|| !HELD.with(Cell::get));
    governor::set_interrupt_probe(|| PENDING.load(SeqCst));
}

#[test]
fn cancel_d4812_the_block_gate_runs_no_trim_reclaim_purge_or_cancel_on_the_allocating_thread() {
    let _one = serial();
    fakes();
    // as the ladder thread does at its sample: an earlier wake is cleared, so the next one wakes it again
    let _ = memledger::arena::take_ladder_due();
    let (seq, wakes) = (last_seq(), WAKES.load(SeqCst));
    let before: Vec<u64> = ON_ASKER.iter().map(|c| c.load(SeqCst)).collect();
    let end = std::thread::spawn(|| {
        ASKER.with(|c| c.set(true));
        governor::register(4001, 40_010);
        governor::statement_start(0, true);
        let ctx = MemoryContext::new("d4812 asker");
        sample();
        governor::arm(true);
        let (_, end) = grow(&ctx, GROWN_MAX);
        governor::arm(false);
        drop(ctx);
        governor::statement_end();
        ASKER.with(|c| c.set(false));
        end
    })
    .join()
    .unwrap();
    calm();
    let did: Vec<String> = ON_ASKER.iter().zip(&before).zip(ACTUATORS).filter(|((c, b), _)| c.load(SeqCst) != **b).map(|((c, b), name)| format!("{name} x{}", c.load(SeqCst) - b)).collect();
    assert!(did.is_empty(), "the block gate ran on the allocating thread: {}", did.join(", "));
    assert!(matches!(end, End::Raised(_)), "the asker held the most past the line and was refused: {end:?}");
    assert!(WAKES.load(SeqCst) > wakes, "the ladder pass was woken for R1, R2p and R2a");
    let eps = block_episodes(seq);
    let posted = [Rung::AllocReturn, Rung::TrimParked, Rung::EvictElastic, Rung::EvictProtected];
    assert!(!eps.is_empty() && eps.iter().all(|(_, ran)| posted.iter().all(|r| ran & (1 << *r as u8) == 0)), "no posted rung ran inline: {eps:?}");
    // its refusal is R6's run for its own statement (the pass's cancel actuator never ran here, above)
    let r6 = 1 << Rung::Cancel as u8;
    assert_eq!(eps.last().map(|e| (e.0, e.1 & r6)), Some((Outcome::Refused, r6)), "{eps:?}");
}

/// What a statement thread of the next test saw: its block, the time it took, and its small chunk after it.
type Seen = (Result<(), Option<Box<types_error::PgError>>>, Duration, bool);

#[test]
fn cancel_d4812_an_innocent_asker_takes_its_block_and_the_statement_r6_picks_is_the_one_refused() {
    let _one = serial();
    fakes();
    set(K::LadderBlockGate, 0);
    let seq = last_seq();
    // the hog holds 256 MiB, the innocent asker 32 MiB, both grown with the gate off; each holds its bytes until both
    // reported (bytes freed meanwhile would end the other's wait under the line)
    let statement = |procno: i32, held: usize, ready: mpsc::Sender<()>, go: mpsc::Receiver<()>, done: mpsc::Sender<Seen>, release: mpsc::Receiver<()>| {
        std::thread::spawn(move || {
            governor::register(procno, procno * 10);
            governor::statement_start(0, true);
            let ctx = MemoryContext::new("d4812 statement");
            governor::arm(true);
            let (grown, end) = grow(&ctx, held);
            assert!(grown == held && matches!(end, End::Max), "{procno}: grew {} MiB: {end:?}", grown / MIB);
            ready.send(()).unwrap();
            go.recv().unwrap();
            let small_first = small(&ctx);
            let t = Instant::now();
            let block = one_block(&ctx);
            let took = t.elapsed();
            // the hog, refused: while it unwinds, R6's pick cancels no second statement (its cancel is the one in flight)
            let second = if block.is_err() { governor::try_cancel_largest_victim(1, 0) } else { Ok(None) };
            done.send((block, took, small_first && matches!(second, Ok(None)))).unwrap();
            release.recv().unwrap();
            governor::arm(false);
            drop(ctx);
            governor::statement_end();
        })
    };
    let (ready_tx, ready) = mpsc::channel();
    let (hog_go, hog_rx) = mpsc::channel();
    let (asker_go, asker_rx) = mpsc::channel();
    let (hog_tx, hog_done) = mpsc::channel();
    let (asker_tx, asker_done) = mpsc::channel();
    let (hog_release, hog_held) = mpsc::channel();
    let (asker_release, asker_held) = mpsc::channel();
    let hog = statement(4002, 256 * MIB, ready_tx.clone(), hog_rx, hog_tx, hog_held);
    let asker = statement(4003, 32 * MIB, ready_tx, asker_rx, asker_tx, asker_held);
    ready.recv().unwrap();
    ready.recv().unwrap();
    set(K::LadderBlockGate, 1);
    sample_short_of_the_line(4 * MIB);
    asker_go.send(()).unwrap();
    let (asker_block, asker_took, asker_small) = asker_done.recv().unwrap();
    hog_go.send(()).unwrap();
    let (hog_block, _, hog_ok) = hog_done.recv().unwrap();
    hog_release.send(()).unwrap();
    asker_release.send(()).unwrap();
    hog.join().unwrap();
    asker.join().unwrap();
    calm();
    assert!(asker_block.is_ok(), "the innocent asker was refused: {asker_block:?}");
    assert!(asker_small, "the innocent asker's chunk before its block");
    assert!(asker_took >= Duration::from_millis(BOUND_MS * 9 / 10), "it waited its bound first: {asker_took:?}");
    assert!(hog_ok, "the hog's chunk before its block was taken (the asker canceled no one), and no second cancel while it unwinds");
    match hog_block {
        Err(Some(e)) => assert_eq!(e.sqlstate(), types_error::ERRCODE_OUT_OF_MEMORY, "{e:?}"),
        other => panic!("the hog's block past the line: {other:?}"),
    }
    let outcomes: Vec<Outcome> = block_episodes(seq).iter().map(|e| e.0).collect();
    assert_eq!(outcomes, [Outcome::Overran, Outcome::Refused], "the asker overran its bound, the hog was refused");
}

#[test]
fn cancel_d4812_a_block_past_the_line_with_interrupts_held_off_is_taken_at_once_and_counted() {
    let _one = serial();
    fakes();
    let (waits, skipped) = (ladder_stats::site(Site::Block).waits, governor::block_gate_counts().skipped);
    let (block, took) = std::thread::spawn(|| {
        governor::register(4004, 40_040);
        governor::statement_start(0, true);
        let ctx = MemoryContext::new("d4812 held");
        governor::arm(true);
        sample_short_of_the_line(4 * MIB);
        HELD.with(|c| c.set(true));
        let t = Instant::now();
        let block = one_block(&ctx);
        let took = t.elapsed();
        HELD.with(|c| c.set(false));
        governor::arm(false);
        drop(ctx);
        governor::statement_end();
        (block, took)
    })
    .join()
    .unwrap();
    calm();
    assert!(block.is_ok(), "{block:?}");
    assert!(took < Duration::from_millis(BOUND_MS / 2), "taken at once, never waited: {took:?}");
    assert_eq!(governor::block_gate_counts().skipped, skipped + 1, "counted");
    assert_eq!(ladder_stats::site(Site::Block).waits, waits, "no wait");
}

#[test]
fn cancel_d4812_a_pending_interrupt_ends_the_gates_wait_within_one_step() {
    let _one = serial();
    fakes();
    let seq = last_seq();
    // the only statement past the line: R6's pick, so only the interrupt ends its wait before the bound
    let grower = std::thread::spawn(|| {
        governor::register(4005, 40_050);
        governor::statement_start(0, true);
        let ctx = MemoryContext::new("d4812 interrupted");
        governor::arm(true);
        let (grown, _) = grow(&ctx, 64 * MIB);
        sample_short_of_the_line(4 * MIB);
        let block = one_block(&ctx);
        let ended = Instant::now();
        governor::arm(false);
        drop(ctx);
        governor::statement_end();
        (grown, block, ended)
    });
    std::thread::sleep(Duration::from_millis(BOUND_MS / 3));
    let raised = Instant::now();
    PENDING.store(true, SeqCst);
    let (grown, block, ended) = grower.join().unwrap();
    PENDING.store(false, SeqCst);
    calm();
    assert_eq!(grown, 64 * MIB);
    assert!(block.is_ok(), "the block is taken, the interrupt answered at the caller's next check: {block:?}");
    let lag = ended.saturating_duration_since(raised);
    assert!(lag < Duration::from_millis(40), "the wait ended {lag:?} after the interrupt (one 5 ms step, plus the scheduler)");
    let outcomes: Vec<Outcome> = block_episodes(seq).iter().map(|e| e.0).collect();
    assert_eq!(outcomes, [Outcome::Interrupted]);
}

#[test]
fn cancel_d4812_a_self_cancel_leaves_no_condemnation_and_the_next_statement_runs_clean() {
    let _one = serial();
    fakes();
    let (first, condemned, second, after) = std::thread::spawn(|| {
        governor::register(4006, 40_060);
        governor::statement_start(0, true);
        let ctx = MemoryContext::new("d4812 refused");
        governor::arm(true);
        let _ = grow(&ctx, 64 * MIB);
        sample_short_of_the_line(4 * MIB);
        let first = one_block(&ctx);
        governor::arm(false);
        drop(ctx);
        governor::statement_end();
        // what ProcessInterrupts would consume at the next CHECK_FOR_INTERRUPTS
        let condemned = governor::take_condemned();
        calm();
        governor::statement_start(0, true);
        let ctx = MemoryContext::new("d4812 next");
        governor::arm(true);
        let (_, second) = grow(&ctx, 64 * MIB);
        let after = small(&ctx);
        governor::arm(false);
        drop(ctx);
        governor::statement_end();
        (first, condemned, second, after)
    })
    .join()
    .unwrap();
    calm();
    match first {
        Err(Some(e)) => assert_eq!(e.sqlstate(), types_error::ERRCODE_OUT_OF_MEMORY, "{e:?}"),
        other => panic!("the statement R6 picks is refused at its block: {other:?}"),
    }
    assert_eq!(condemned, 0, "no condemnation left for a second 53200");
    assert!(matches!(second, End::Max) && after, "the next statement runs clean: {second:?}");
}
