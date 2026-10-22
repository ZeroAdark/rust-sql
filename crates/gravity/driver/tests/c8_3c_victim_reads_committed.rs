//! C8 move 3c (b): the projection cancel's victim pick reads what a statement has committed (policy 3.2: RUNNING counts
//! committed bytes), not its grant's untouched room. Two statements under pressure: A holds a 1 GiB reservation it never
//! drew from, B a 256 MiB committed charge. Canceling A frees nothing resident, so the pick must be B. Born red on
//! 6f81470b494 (move 3c (a)): the pick read A's whole claim (the governor's statement charge) and chose A.

use std::sync::mpsc;
use std::time::Duration;

use gravity_driver::{Grant, POOL, REGIONS};
use mcx::governor;
use mempolicy::decide::{self, Rise99, Rule};

const MIB: u64 = 1 << 20;

/// A statement on its own thread, registered on governor slot `slot`, holding what `hold` makes until told to end; it
/// reports once it holds, and at the end whether its statement was canceled.
fn statement(slot: i32, hold: impl FnOnce() -> Box<dyn std::any::Any> + Send + 'static) -> (mpsc::Receiver<()>, mpsc::Sender<()>, std::thread::JoinHandle<bool>) {
    let (ready_tx, ready) = mpsc::channel();
    let (end, end_rx) = mpsc::channel::<()>();
    let t = std::thread::spawn(move || {
        governor::register(slot, 0);
        governor::statement_start(0, true);
        let held = hold();
        ready_tx.send(()).unwrap();
        end_rx.recv().unwrap();
        let canceled = governor::take_watchdog_cancel().is_some();
        drop(held);
        governor::statement_end();
        canceled
    });
    (ready, end, t)
}

#[test]
fn the_projection_cancel_picks_the_committed_statement_not_an_untouched_reservation() {
    POOL.gate(0);
    REGIONS.trim();
    POOL.arm(4 << 30);
    let (a_ready, a_end, a) = statement(41, || Box::new(POOL.admit(Grant::of(1 << 30), 1, 1, Duration::from_millis(100)).expect("admitted")));
    a_ready.recv().unwrap();
    let (b_ready, b_end, b) = statement(42, || Box::new(governor::Charge::new("held", (256 * MIB) as usize)));
    b_ready.recv().unwrap();
    // projection, F = 0 and R + rise99 past L - margin: the backstop's Tick cancels the statement furthest past its share
    decide::set_rule(Rule::Projection);
    let fp = mcx::global_footprint::bytes();
    governor::publish(0, 0, fp);
    governor::publish_decide_resident((60 * MIB) as usize, fp);
    let rise = |b| Rise99 { bytes: b, prior: false };
    governor::publish_decide(&governor::DecidePublish { l: 64 * MIB, b: 32 * MIB, u: MIB, fast: 0, rise_fast: rise(MIB), rise_slow: rise(16 * MIB), revoke_on: false });
    let t = governor::decide_tick().expect("projection decides at Tick");
    let victim = t.victim.and_then(|v| v.backend).map(|(slot, _)| slot);
    decide::set_rule(Rule::Threshold);
    governor::publish_decide(&governor::DecidePublish::default());
    a_end.send(()).unwrap();
    b_end.send(()).unwrap();
    let (a_canceled, b_canceled) = (a.join().unwrap(), b.join().unwrap());
    POOL.arm(0);
    assert_eq!(victim, Some(42), "the committed statement is the victim, not the untouched reservation (slot 41)");
    assert!(b_canceled && !a_canceled, "B canceled {b_canceled}, A canceled {a_canceled}");
}
