//! D3998: a thread's door-1 bytes not yet on an account when it exits reach its current account. Before, a session's
//! teardown, which frees its estate after the unbind moved the estate's bytes to UNBOUND, left them on UNBOUND for good:
//! up to `DOOR_FOLD_BYTES` per ended session. One test in its own binary, so nothing else moves UNBOUND here.

use std::cell::Cell;

use memledger::{account_row, bind_backend, door_heap, unbind_backend, Acct};

/// A thread-local that frees its bytes when the thread exits (its destructor armed before the bind).
struct LateFree(Cell<isize>);

impl Drop for LateFree {
    fn drop(&mut self) {
        door_heap(-self.0.get());
    }
}

thread_local! {
    static LATE: LateFree = const { LateFree(Cell::new(0)) };
}

#[test]
fn an_exiting_thread_folds_what_it_moved_since_its_last_fold() {
    let estate = 200 << 10; // under the fold step, as a small session's is
    let unbound = || account_row(Acct::UNBOUND).account_bytes;
    let before = unbound();
    std::thread::spawn(move || {
        bind_backend(4010, 1);
        door_heap(estate); // the session's estate, allocated while it is bound
        unbind_backend(); // the slot's bytes move to UNBOUND
        door_heap(-estate); // the session's teardown frees it on UNBOUND; the thread then exits
    })
    .join()
    .unwrap();
    assert_eq!(unbound() - before, 0, "a session's teardown after its unbind must not stay on UNBOUND");
    // a thread-local whose destructor was armed before the bind: its free at the thread's exit reaches UNBOUND too
    std::thread::spawn(move || {
        LATE.with(|l| l.0.set(64 << 10));
        bind_backend(4012, 3);
        door_heap(estate + (64 << 10));
        unbind_backend();
        door_heap(-estate);
    })
    .join()
    .unwrap();
    assert_eq!(unbound() - before, 0, "a free in a thread-local's destructor must reach UNBOUND too");
    // a thread that ends without an unbind (a crash-class exit) leaves its pending bytes on its own account
    std::thread::spawn(move || {
        bind_backend(4011, 2);
        door_heap(4096);
    })
    .join()
    .unwrap();
    assert_eq!(account_row(Acct::backend(4011)).account_bytes, 4096);
    assert_eq!(unbound() - before, 0);
}
