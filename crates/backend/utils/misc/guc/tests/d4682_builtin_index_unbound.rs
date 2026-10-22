//! D4682 step 2 (a), 9d's ledger condition: the process's shared index of the built-in GUC names is process-lifetime
//! memory, so it is built on UNBOUND (by construction, inside its holder) and never on the session that happens to
//! publish it. Session 1's account ends with none of its bytes, UNBOUND rises by them once and not again for sessions
//! 2..N, and a registry's clone of it drops with no free (the holder keeps the last reference). Its own binary: the
//! global allocator here is door 1 (every allocation and free counted on the calling thread's account, `door_heap`), as
//! the server's allocator is, and the shared index is process state.

use std::alloc::{GlobalAlloc, Layout, System};

use memledger::{account_row, bind_backend, door_heap, fold, set_home, unbind_backend, Acct};

struct Door1;

// SAFETY: forwards to the system allocator; `door_heap` is a const thread-local add (no allocation, no destructor).
unsafe impl GlobalAlloc for Door1 {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        // SAFETY: the caller's layout, forwarded.
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            door_heap(l.size() as isize);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: `p` came from `alloc` with `l`.
        unsafe { System.dealloc(p, l) };
        door_heap(-(l.size() as isize));
    }
}

#[global_allocator]
static A: Door1 = Door1;

/// Far below the index (some hundred names and their table, tens of KB): what else a session's registry build leaves
/// behind may differ this much between two sessions.
const SLACK: i64 = 1024;

fn setup_seams() {
    guc_tables::init_seams();
    elog::init_seams();
    guc::init_seams();
    xact_seams::is_in_parallel_mode::set(|| false);
    scalar_seams::parse_bool::set(|value| match value {
        "true" | "on" | "yes" | "1" => Some(true),
        "false" | "off" | "no" | "0" => Some(false),
        _ => None,
    });
    aclchk_seams::pg_parameter_aclcheck_set::set(|_, _| Ok(true));
    mbutils_seams::get_database_encoding::set(|| 6);
    timestamp_seams::get_current_timestamp::set(|| 0);
}

fn account(a: Acct) -> i64 {
    fold();
    account_row(a).account_bytes
}

/// One session on its own thread and backend slot: builds its registry, then drops it (its teardown) while still
/// bound. Returns (what UNBOUND rose by during the build, what the session's account kept after the drop).
fn session(procno: u32) -> (i64, i64) {
    std::thread::spawn(move || {
        bind_backend(procno, 9100 + procno);
        let base = account(Acct::backend(procno));
        let unbound = account(Acct::UNBOUND);
        guc::store::initialize_guc_options().unwrap();
        let rose = account(Acct::UNBOUND) - unbound;
        guc::store::with_store_mut(|r| drop(std::mem::replace(r, guc::registry::GucRegistry::new()))).unwrap();
        let kept = account(Acct::backend(procno)) - base;
        unbind_backend();
        (rose, kept)
    })
    .join()
    .unwrap()
}

#[test]
fn the_shared_builtin_index_is_built_once_on_unbound_and_never_on_a_session() {
    setup_seams();
    set_home(true);
    // sharing off first: whatever the first registry build in the process sets up for good lands on the first session,
    // and the second is the plain session the shared ones are compared with
    let _ = session(4039);
    let (rose, warm) = session(4040);
    assert!(rose.abs() <= SLACK, "sharing off: nothing is built on UNBOUND: rose {rose}");

    guc::store::set_share_builtin_index(true);
    let (rose1, kept1) = session(4041);
    eprintln!("d4682 witness: a plain session kept {warm} B; session 1 put {rose1} B on UNBOUND and kept {kept1} B");
    assert!(rose1 > 8 << 10, "session 1 publishes the index on UNBOUND: rose {rose1}");
    assert!((kept1 - warm).abs() <= SLACK, "session 1 keeps none of the index: kept {kept1}, a plain session {warm}");
    for procno in 4042..4045 {
        let (rose, kept) = session(procno);
        eprintln!("d4682 witness: session {procno} put {rose} B on UNBOUND and kept {kept} B");
        assert!(rose.abs() <= SLACK, "session {procno} adopts the index, nothing more on UNBOUND: rose {rose}");
        // the registry's clone dropped with it; had it been the last reference, the index's free would show here as
        // the index's bytes below the plain session
        assert!((kept - warm).abs() <= SLACK, "session {procno}: kept {kept}, a plain session {warm}");
    }
    guc::store::set_share_builtin_index(false);
}
