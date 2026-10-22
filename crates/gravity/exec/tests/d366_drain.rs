//! D366: a cancel delivered while the leader drains the legs after the run (exec runner/drive.rs, LegSink::drain)
//! is seen before the next claim is sent, as PostgreSQL's executor sees one at the next tuple it sends. The drain
//! sent every buffered claim with no interrupt check: a cancel landing in it (~28 ms for m1_ordered's 400 k-row
//! stream at W16) was never seen, the statement completed, and the cancel was dropped when the backend went idle.
//! Here the client's first push is where the cancel lands: the sender raises the backend's AbortPending byte
//! (procsignal's SIGINT / SIGTERM abort class), which the drain must read before the next claim, in claim order
//! and sorted.
use gravity_abi::MorselRange;
use gravity_driver::{BorrowedBatch, Error, Flow, Result, Sink};
use gravity_exec::legs::LegSink;

/// The client: counts the rows it receives; its first push is where the cancel is delivered.
struct Client {
    rows: u64,
    pushes: u32,
}

impl Sink for Client {
    fn push(&mut self, b: BorrowedBatch<'_>) -> Result<Flow> {
        self.rows += b.rows as u64;
        self.pushes += 1;
        if self.pushes == 1 {
            init_small::globals::SetAbortPending(true);
        }
        Ok(Flow::Continue)
    }
}

/// `claims` claims of 10 rows each, buffered in the leader's leg (settled, waiting for the drain).
fn buffer(legs: &mut LegSink<'_, Client>, claims: u64) {
    for c in 0..claims {
        let lo = c * 10;
        legs.leg(0).expect("the leader's leg").claim_begin(MorselRange::new(lo, lo + 10));
        let v: Vec<u64> = (lo..lo + 10).rev().collect();
        Sink::push(legs, BorrowedBatch { natts: 1, rows: 10, values: &v, isnull: &[0; 10], region: &[] }).unwrap();
        legs.leg(0).expect("the leader's leg").claim_end(true);
    }
}

fn drain(claims: u64, keys: &[i16]) -> (Result<u64>, u64) {
    init_small::globals::SetAbortPending(false);
    let mut client = Client { rows: 0, pushes: 0 };
    let r = {
        let mut legs = LegSink::new(&mut client, 1, 0, 0, true, !keys.is_empty(), &[0], None);
        buffer(&mut legs, claims);
        legs.drain(keys)
    };
    init_small::globals::SetAbortPending(false);
    (r, client.rows)
}

#[test]
fn a_cancel_during_the_post_run_drain_is_seen_before_the_next_claim() {
    let (r, rows) = drain(8, &[]);
    assert!(
        matches!(r, Err(Error::Cancelled)),
        "a cancel delivered during the drain must end it (Error::Cancelled), got {:?}",
        r.ok()
    );
    assert_eq!(rows, 10, "only the claim sent before the cancel reaches the client");
    // sorted (a Sort root over the legs): seen before the next row
    let (r, rows) = drain(8, &[0]);
    assert!(
        matches!(r, Err(Error::Cancelled)),
        "sorted: a cancel delivered during the drain must end it, got {:?}",
        r.ok()
    );
    assert_eq!(rows, 1, "sorted: only the row sent before the cancel reaches the client");
    // the backend's next statement: a drain with no cancel sends every claim (nothing left pending by the one above)
    init_small::globals::SetAbortPending(false);
    struct Quiet(u64);
    impl Sink for Quiet {
        fn push(&mut self, b: BorrowedBatch<'_>) -> Result<Flow> {
            self.0 += b.rows as u64;
            Ok(Flow::Continue)
        }
    }
    let mut quiet = Quiet(0);
    let mut legs = LegSink::new(&mut quiet, 1, 0, 0, true, false, &[0], None);
    for c in 0..8u64 {
        legs.leg(0).expect("the leader's leg").claim_begin(MorselRange::new(c * 10, c * 10 + 10));
        let v: Vec<u64> = (c * 10..c * 10 + 10).collect();
        Sink::push(&mut legs, BorrowedBatch { natts: 1, rows: 10, values: &v, isnull: &[0; 10], region: &[] }).unwrap();
        legs.leg(0).expect("the leader's leg").claim_end(true);
    }
    assert_eq!(legs.drain(&[]).unwrap(), 80, "the next drain sends all 8 claims");
}
