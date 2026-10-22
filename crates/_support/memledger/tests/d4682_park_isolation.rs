//! D4682 (9d flip condition 6): a client backend thread that parks for the next connection folds what its exit fold
//! would (`park_thread`) before it waits, so the session that binds it next starts at its base and never carries the
//! previous session's bytes. One test in its own binary, so nothing else moves UNBOUND here.

use memledger::{account_row, bind_backend, door_heap, park_thread, unbind_backend, Acct};

#[test]
fn a_session_on_a_reused_thread_never_inherits_the_previous_sessions_bytes() {
    let estate = 200 << 10; // under the fold step, so it sits in door 1's pending until something folds it
    let shells = 24 << 10; // what A's teardown keeps after its unbind (retired root shells, freed at thread exit)
    let unbound = || account_row(Acct::UNBOUND).account_bytes;
    let before = unbound();
    std::thread::spawn(move || {
        // session A on thread T: allocates and frees its estate, unbinds, its teardown frees on UNBOUND
        bind_backend(4020, 1);
        door_heap(estate);
        unbind_backend();
        door_heap(-estate + shells);
        park_thread();
        // parked: A's leftover is on UNBOUND already, not waiting in the door for whoever binds next
        assert_eq!(unbound() - before, shells as i64, "the park folds the door before the wait");
        // session B binds the parked thread T
        bind_backend(4021, 2);
        assert_eq!(account_row(Acct::backend(4021)).account_bytes, 0, "B starts at its base");
        door_heap(4096);
        unbind_backend();
        door_heap(-4096);
        park_thread();
        assert_eq!(account_row(Acct::backend(4021)).account_bytes, 0, "B's own bytes went with its unbind");
    })
    .join()
    .unwrap();
    assert_eq!(account_row(Acct::backend(4020)).account_bytes, 0, "A's account ended at its unbind");
    assert_eq!(unbound() - before, shells as i64, "A's leftover is on UNBOUND, nowhere else");

    // a session that parks still bound (no unbind ran): its pending bytes stay on its own account, never on the next
    std::thread::spawn(move || {
        bind_backend(4022, 3);
        door_heap(8192);
        park_thread();
        assert_eq!(account_row(Acct::backend(4022)).account_bytes, 8192, "the park folds onto the still-bound account");
        bind_backend(4023, 4);
        assert_eq!(account_row(Acct::backend(4023)).account_bytes, 0, "the next session starts at its base");
        unbind_backend();
        park_thread();
    })
    .join()
    .unwrap();
    assert_eq!(account_row(Acct::backend(4022)).account_bytes, 8192, "the still-bound session's bytes stay on it");
    assert_eq!(account_row(Acct::backend(4023)).account_bytes, 0);
}
