//! The in-server unattributed check (memledger `unattr`). While on, a backend's statement is checked as it
//! rises (every 64 MiB, at least) and at its end: the account's bytes less what its owners hold, against max(16 MiB, 2 %).
//! A report comes once per statement, and only when the end or a rise passed its bound. Off, nothing is kept or reported.
//! One test in its own binary: the switch is process-wide. No counting allocator: the account moves only by `door_heap`
//! (door 1, as the server's allocator calls it) and the charges.
use memledger::charge::{context, Door};
use memledger::{bind_backend, door_heap, statement_begin, statement_end, unattr, unbind_backend, Charge, Kind};

const MIB: isize = 1 << 20;

#[test]
fn the_check_reports_unattributed_bytes_past_the_bound_at_the_end_and_at_a_rise() {
    std::thread::spawn(|| {
        bind_backend(3991, 1);
        // off: nothing kept, nothing reported
        statement_begin();
        door_heap(32 * MIB);
        statement_end();
        assert_eq!(unattr::end_report(), None, "off");
        door_heap(-32 * MIB);
        unattr::set(true);

        // 32 MiB no owner holds, left at the end: over at the end (the bound's floor, 16 MiB); under 64 MiB no rise check
        statement_begin();
        door_heap(32 * MIB);
        statement_end();
        let r = unattr::end_report().expect("over at the end");
        let (u, b) = r.end.expect("measured");
        assert!(u >= 32 << 20 && b == unattr::FLOOR, "{r:?}");
        assert_eq!((r.peak, r.checks, r.skips), (None, 1, 0), "{r:?}");
        assert_eq!(unattr::end_report(), None, "one report a statement");
        door_heap(-32 * MIB);

        // the same bytes charged to the statement (door 1: the account's bytes, now an owner's): within the bound
        statement_begin();
        door_heap(32 * MIB);
        let (o, release) = context();
        let c = Charge::open(o, release, Kind::Region, Door::Heap, 32 << 20);
        statement_end();
        assert_eq!(unattr::end_report(), None, "charged: within the bound");
        drop(c);
        door_heap(-32 * MIB);

        // 300 MiB no owner holds, taken in three 100 MiB rises and freed before the end. A rise check does not judge the
        // fold that triggered it (its owner's commit comes right after), and counts the excess sustained over two
        // consecutive checks: the third rise reports the second's 100 MiB unattributed
        statement_begin();
        for _ in 0..3 {
            door_heap(100 * MIB);
        }
        door_heap(-300 * MIB);
        statement_end();
        let r = unattr::end_report().expect("over at the rises");
        let (pu, pb, level) = r.peak.expect("a sustained rise past its bound");
        assert_eq!((pu, pb, level), (100 << 20, unattr::FLOOR, 300 << 20), "{r:?}");
        assert!(r.end.is_some_and(|(u, b)| u <= b), "the end within the bound: {r:?}");
        assert_eq!((r.checks, r.skips), (4, 0), "three rises and the end: {r:?}");

        // one uncharged 100 MiB rise, then freed: never sustained, no report
        statement_begin();
        door_heap(100 * MIB);
        door_heap(-100 * MIB);
        statement_end();
        assert_eq!(unattr::end_report(), None, "a transient rise is not sustained");

        // a large statement's 1.5 GiB in two blocks, each charged right after its allocation (as ChargedVec does): its
        // rises are checked inside the allocation-to-commit window, and none is over
        statement_begin();
        door_heap(1024 * MIB);
        let (o, release) = context();
        let c = Charge::open(o, release, Kind::Region, Door::Heap, 1 << 30);
        door_heap(512 * MIB);
        let mut c2 = Charge::open(o, release, Kind::Region, Door::Heap, 0);
        c2.commit(512 << 20);
        statement_end();
        assert_eq!(unattr::end_report(), None, "charged: no rise over");
        drop((c, c2));
        door_heap(-1536 * MIB);

        unattr::set(false);
        unbind_backend();
    })
    .join()
    .unwrap();
}
