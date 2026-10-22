use gravity_driver::{BorrowedBatch, Error, Flow, PullBuffer, Sink};

#[test]
fn a_zero_column_row_is_buffered_as_a_row() {
    let mut buf = PullBuffer::new(0, 4);
    let flow = buf.push(BorrowedBatch { natts: 0, rows: 1, values: &[], isnull: &[], region: &[] }).unwrap();
    assert!(matches!(flow, Flow::Stop));
    assert_eq!(buf.buffered(), 1);
}

#[test]
fn a_batch_of_another_width_is_an_error_not_a_panic() {
    let mut buf = PullBuffer::new(1, 4);
    let r = buf.push(BorrowedBatch { natts: 0, rows: 1, values: &[], isnull: &[], region: &[] });
    assert!(matches!(r, Err(Error::Host(_))));
    let mut buf = PullBuffer::new(0, 4);
    let r = buf.push(BorrowedBatch { natts: 1, rows: 1, values: &[7], isnull: &[0], region: &[] });
    assert!(matches!(r, Err(Error::Host(_))));
    assert_eq!(buf.buffered(), 0);
}
