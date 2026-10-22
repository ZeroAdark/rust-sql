//! D4395: a memory decision counts a statement as holding its charge or what its ledger row measures, whichever is more.
//! Each test runs a statement that asked for nothing and holds 4 GiB on its ledger account (posted, not allocated), and
//! checks that one decision treats it as 4 GiB: the statement limit, decide(Grow)'s spill-self, the projection's fair-share
//! victim, the ladder's R4a asker and R4b's revoke order.
#![cfg(feature = "std")]

use std::sync::atomic::AtomicU8;
use std::sync::{mpsc, Arc, Barrier, Mutex, MutexGuard};

use mcx::{governor, ladder, revoke};
use mempolicy::decide::{self, Rise99, Rule};

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

static ONE: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    ONE.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone, Copy)]
struct Stmt {
    procno: u32,
    limit: usize,
    asked: usize,
    measured: u64,
    spillable: i64,
}

impl Stmt {
    fn new(procno: u32, asked: u64, measured: u64) -> Stmt {
        Stmt { procno, limit: 0, asked: asked as usize, measured, spillable: 0 }
    }
}

/// The statement started on this thread's backend: `asked` charged, `measured` heap on its account, `spillable` noted.
fn begin(s: Stmt) -> governor::Charge {
    let pid = 9000 + s.procno as i32;
    governor::register(s.procno as i32, pid);
    revoke::register_byte(Box::leak(Box::new(AtomicU8::new(0))));
    memledger::bind_backend(s.procno, pid as u32);
    governor::statement_start(s.limit, true);
    memledger::statement_begin();
    let charge = governor::Charge::new("asked", s.asked);
    memledger::door_heap(s.measured as isize);
    memledger::fold();
    revoke::note_spillable(s.spillable);
    charge
}

/// Whether it was canceled, and the bytes revoke asked it for.
fn end(s: Stmt, charge: governor::Charge) -> (bool, u64) {
    let canceled = governor::take_watchdog_cancel().is_some();
    let asked = governor::current().map_or(0, |l| l.revoke().asked().0);
    memledger::door_heap(-(s.measured as isize));
    memledger::fold();
    drop(charge);
    let _ = memledger::statement_end();
    governor::statement_end();
    memledger::unbind_backend();
    (canceled, asked)
}

fn within<T: Send + 'static>(s: Stmt, f: impl FnOnce() -> T + Send + 'static) -> (T, (bool, u64)) {
    std::thread::spawn(move || {
        let c = begin(s);
        let t = f();
        (t, end(s, c))
    })
    .join()
    .unwrap()
}

/// `s` running until `done`; it reports once it holds.
fn running(s: Stmt, ready: mpsc::Sender<()>, done: Arc<Barrier>) -> std::thread::JoinHandle<(bool, u64)> {
    std::thread::spawn(move || {
        let c = begin(s);
        ready.send(()).unwrap();
        done.wait();
        end(s, c)
    })
}

fn rise(bytes: u64) -> Rise99 {
    Rise99 { bytes, prior: false }
}

fn publish(l: u64, b: u64, rise_slow: u64) {
    governor::publish_decide(&governor::DecidePublish { l, b, u: MIB, fast: 0, rise_fast: rise(MIB), rise_slow: rise(rise_slow), revoke_on: false });
}

fn unpublish() {
    decide::set_rule(Rule::Threshold);
    governor::publish_decide(&governor::DecidePublish::default());
    governor::publish_decide_resident(0, mcx::global_footprint::bytes());
}

#[test]
fn the_statement_limit_counts_measured_bytes() {
    let _one = serial();
    let fits = |measured| within(Stmt { limit: GIB as usize, ..Stmt::new(61, 0, measured) }, || governor::Charge::new("probe", 0).fits(1)).0;
    assert!(fits(0), "nothing held: the 1 GiB limit has room");
    assert!(!fits(4 * GIB), "a 1 GiB limit refuses a statement holding 4 GiB it never asked for");
}

#[test]
fn decide_grow_spills_a_statement_past_its_share_by_measured_bytes() {
    let _one = serial();
    decide::set_rule(Rule::Projection);
    let fp = mcx::global_footprint::bytes();
    governor::publish(0, 0, fp);
    // B 2 GiB, R 1 GiB: F 1 GiB short of F* 1.5 GiB, L far (no cancel); one holder's share is B
    governor::publish_decide_resident(GIB as usize, fp);
    publish(64 * GIB, 2 * GIB, 3 * GIB / 2);
    // a 1 GiB charge is past the credit (at most tranche_max): decide(Grow) runs at the next fits
    let grow = |measured| {
        within(Stmt::new(62, 0, measured), || {
            let c = governor::Charge::new("grows", GIB as usize);
            (c.fits(1), governor::spills_itself())
        })
        .0
    };
    let (within_share, past_share) = (grow(0), grow(4 * GIB));
    unpublish();
    assert_eq!(within_share, (true, false), "1 GiB within its 2 GiB share: no spill-self");
    assert_eq!(past_share, (false, true), "4 GiB measured is past its 2 GiB share: it spills itself");
}

#[test]
fn the_fair_share_victim_is_picked_by_measured_bytes() {
    let _one = serial();
    let (tx, ready) = mpsc::channel();
    let done = Arc::new(Barrier::new(3));
    let unasked = running(Stmt::new(63, 0, 4 * GIB), tx.clone(), done.clone());
    let asked = running(Stmt::new(64, 256 * MIB, 0), tx, done.clone());
    ready.recv().unwrap();
    ready.recv().unwrap();
    // projection, F = 0 and R + rise99 past L - margin: the Tick cancels the statement furthest past its share
    decide::set_rule(Rule::Projection);
    let fp = mcx::global_footprint::bytes();
    governor::publish(0, 0, fp);
    governor::publish_decide_resident((60 * MIB) as usize, fp);
    publish(64 * MIB, 32 * MIB, 16 * MIB);
    let t = governor::decide_tick().expect("projection decides at Tick");
    unpublish();
    done.wait();
    let ((unasked_canceled, _), (asked_canceled, _)) = (unasked.join().unwrap(), asked.join().unwrap());
    assert_eq!(t.victim.and_then(|v| v.backend).map(|(slot, _)| slot), Some(63), "the statement holding 4 GiB, not the 256 MiB charge");
    assert!(unasked_canceled && !asked_canceled, "4 GiB canceled {unasked_canceled}, 256 MiB canceled {asked_canceled}");
}

fn read_past_line() -> Option<u64> {
    Some(8 * GIB + 64 * MIB)
}

static ACTS: ladder::Acts = ladder::Acts {
    read: read_past_line,
    reclaimable: || 0,
    reclaim: |_| 0,
    cancel: |_| false,
    cancel_in_flight: || false,
    protected: || 0,
    evict_protected: |_| 0,
};

#[test]
fn the_ladder_asker_spills_its_own_by_measured_bytes() {
    let _one = serial();
    ladder::install(&ACTS);
    let climb = |measured| {
        within(Stmt { spillable: (64 * MIB) as i64, ..Stmt::new(65, 0, measured) }, || {
            // B 2 GiB over this one holder: its fair share is under 2 GiB
            publish(64 * GIB, 2 * GIB, MIB);
            ladder::publish(ladder::ON, 8 * GIB);
            let c = ladder::climb(MIB);
            ladder::publish(ladder::OFF, 0);
            ladder::clear_narrow();
            c.ran & (1 << mempolicy::ladder::Rung::OwnSpill as u8) != 0
        })
    };
    let ((none_held, (_, none_asked)), (held, (_, held_asked))) = (climb(0), climb(4 * GIB));
    unpublish();
    assert!(!none_held && none_asked == 0, "holding nothing it does not spill its own (R4a ran {none_held}, asked {none_asked})");
    assert!(held && held_asked != 0, "4 GiB measured past its fair share: R4a asks it to spill its own (ran {held}, asked {held_asked})");
}

#[test]
fn revoke_asks_the_statement_furthest_past_its_share_by_measured_bytes() {
    let _one = serial();
    revoke::set_enabled(true);
    let (tx, ready) = mpsc::channel();
    let done = Arc::new(Barrier::new(3));
    let spillable = (64 * MIB) as i64;
    let unasked = running(Stmt { spillable, ..Stmt::new(66, 0, 4 * GIB) }, tx.clone(), done.clone());
    let asked = running(Stmt { spillable, ..Stmt::new(67, GIB, 0) }, tx, done.clone());
    ready.recv().unwrap();
    ready.recv().unwrap();
    // one MiB short: one ask, to the first in revoke's order
    let bytes = revoke::request_revoke(MIB, 512 * MIB, None);
    revoke::set_enabled(false);
    done.wait();
    let ((_, unasked_asked), (_, asked_asked)) = (unasked.join().unwrap(), asked.join().unwrap());
    assert_eq!(bytes, MIB);
    assert!(unasked_asked == MIB && asked_asked == 0, "the 4 GiB statement is asked first (asked {unasked_asked}), not the 1 GiB charge ({asked_asked})");
}
