//! D379: a cancel delivered while one large claim is sent is seen within `SEND_ROWS` rows, as PostgreSQL's executor
//! sees one at the next tuple. The drain checked only between claims, so a cancel landing in a claim's send was seen
//! one claim late (a whole claim's rows reached the client first). Here one buffered claim holds 3 * SEND_ROWS + 5 rows
//! and the client's first push raises the backend's AbortPending byte.
use gravity_abi::MorselRange;
use gravity_driver::{BorrowedBatch, Error, Flow, Result, Sink};
use gravity_exec::legs::{LegSink, SEND_ROWS};

struct Client {
    rows: u64,
    pushes: u32,
    cancel: bool,
}

impl Sink for Client {
    fn push(&mut self, b: BorrowedBatch<'_>) -> Result<Flow> {
        self.rows += b.rows as u64;
        self.pushes += 1;
        if self.cancel && self.pushes == 1 {
            init_small::globals::SetAbortPending(true);
        }
        Ok(Flow::Continue)
    }
}

fn drain(cancel: bool) -> (Result<u64>, Client) {
    let total = 3 * SEND_ROWS + 5;
    init_small::globals::SetAbortPending(false);
    let mut client = Client { rows: 0, pushes: 0, cancel };
    let r = {
        let mut legs = LegSink::new(&mut client, 1, 0, 0, true, false, &[0], None);
        legs.leg(0).expect("the leader's leg").claim_begin(MorselRange::new(0, total));
        let v: Vec<u64> = (0..total).collect();
        let n = vec![0u8; total as usize];
        Sink::push(&mut legs, BorrowedBatch { natts: 1, rows: total as usize, values: &v, isnull: &n, region: &[] }).unwrap();
        legs.leg(0).expect("the leader's leg").claim_end(true);
        legs.drain(&[])
    };
    init_small::globals::SetAbortPending(false);
    (r, client)
}

#[test]
fn a_cancel_during_one_large_claim_is_seen_within_poll_rows() {
    let (r, client) = drain(true);
    assert!(matches!(r, Err(Error::Cancelled)), "a cancel delivered inside a claim must end the drain, got {:?}", r.ok());
    assert_eq!(client.rows, SEND_ROWS, "only the first SEND_ROWS rows of the claim reach the client");
    // no cancel: the claim's every row, in SEND_ROWS pieces
    let (r, client) = drain(false);
    assert_eq!(r.unwrap(), 3 * SEND_ROWS + 5);
    assert_eq!(client.rows, 3 * SEND_ROWS + 5);
    assert_eq!(client.pushes, 4);
}
