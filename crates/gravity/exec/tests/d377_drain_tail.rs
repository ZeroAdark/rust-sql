//! D377: a statement_timeout (or cancel) that lands while the post-run drain sends its LAST claim (exec runner/drive.rs,
//! LegSink::drain) ends THIS statement. The drain read the abort byte before each claim (D366) and never after the
//! last: a timeout the final send drained (a client that reads slowly) left the byte, QueryCancelPending and the
//! timeout's indicator set, the statement completed, and the connection's NEXT statement failed with 57014 at its
//! first check. PostgreSQL's executor checks once more when it asks its plan for the row after the last. Here the
//! client's last push is where the interrupt lands: the sender raises the backend's AbortPending byte, which the drain
//! must read before it returns, in claim order and sorted.
use gravity_abi::MorselRange;
use gravity_driver::{BorrowedBatch, Error, Flow, Result, Sink};
use gravity_exec::legs::LegSink;

/// The client: counts the rows it receives; its `at`-th push is where the interrupt is delivered.
struct Client {
    rows: u64,
    pushes: u32,
    at: u32,
}

impl Sink for Client {
    fn push(&mut self, b: BorrowedBatch<'_>) -> Result<Flow> {
        self.rows += b.rows as u64;
        self.pushes += 1;
        if self.pushes == self.at {
            init_small::globals::SetAbortPending(true);
        }
        Ok(Flow::Continue)
    }
}

/// Eight 10-row claims buffered in the leader's leg, drained; the interrupt lands at the client's `at`-th push.
fn drain(at: u32, keys: &[i16]) -> (Result<u64>, u64) {
    init_small::globals::SetAbortPending(false);
    let mut client = Client { rows: 0, pushes: 0, at };
    let r = {
        let mut legs = LegSink::new(&mut client, 1, 0, 0, true, !keys.is_empty(), &[0], None);
        for c in 0..8u64 {
            let lo = c * 10;
            legs.leg(0).expect("the leader's leg").claim_begin(MorselRange::new(lo, lo + 10));
            let v: Vec<u64> = (lo..lo + 10).rev().collect();
            Sink::push(&mut legs, BorrowedBatch { natts: 1, rows: 10, values: &v, isnull: &[0; 10], region: &[] }).unwrap();
            legs.leg(0).expect("the leader's leg").claim_end(true);
        }
        legs.drain(keys)
    };
    // what the runner's error path does (ProcessInterrupts consumes the pending interrupt): the next statement starts clean
    init_small::globals::SetAbortPending(false);
    (r, client.rows)
}

#[test]
fn an_interrupt_during_the_drains_last_send_ends_this_statement() {
    // claim order: the eighth (last) claim's send is where the timeout lands
    let (r, rows) = drain(8, &[]);
    assert_eq!(rows, 80, "every row was sent before the timeout landed");
    assert!(
        matches!(r, Err(Error::Cancelled)),
        "a timeout landing during the last claim's send must end this statement (Error::Cancelled), got {:?}",
        r.ok()
    );
    // sorted (a Sort root over the legs): the last row's send
    let (r, rows) = drain(80, &[0]);
    assert_eq!(rows, 80, "sorted: every row was sent before the timeout landed");
    assert!(
        matches!(r, Err(Error::Cancelled)),
        "sorted: a timeout landing during the last row's send must end this statement, got {:?}",
        r.ok()
    );
    // the connection's next statement: nothing left pending, every row is sent and the drain succeeds
    let (r, rows) = drain(0, &[]);
    assert_eq!((r.ok(), rows), (Some(80), 80), "the next statement's drain completes");
    let (r, rows) = drain(0, &[0]);
    assert_eq!((r.ok(), rows), (Some(80), 80), "the next statement's sorted drain completes");
}
