//! D4702: a pooled standby makes its per-backend shared stats entry (pgstat_bestart's pgstat_create_backend, a 2,920 B
//! box) while it serves its leader's task, inside the leader's account, and drops it at its own park (pgstat_shutdown_hook)
//! on its own slot. Both now happen in the shared stats holder's home: the leader keeps none of the boxes and the standby
//! frees none of them below its base; the holder carries them in between. Its own binary: the global allocator is door 1.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::{Arc, Barrier, Mutex};

use memledger::{account_row, bind_backend, door_heap, fold, set_home, unbind_backend, Acct, Home};

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

type Hook = fn(i32, datum::Datum) -> types_error::PgResult<()>;

/// pgstat_initialize's before_shmem_exit registration (pgstat_shutdown_hook), kept to run at a standby's park.
static HOOK: Mutex<Option<Hook>> = Mutex::new(None);

/// Standbys: their backend entries are several times the slack.
const STANDBYS: u32 = 64;
/// One backend entry (PgStat_Backend: the reset timestamp, the 3 x [3][5][8] io matrices, the WAL counters).
const ENTRY: i64 = 2920;
/// What a spawn / join and a thread's first use move between accounts: well below STANDBYS entries.
const SLACK: i64 = 64 << 10;

fn account(a: Acct) -> i64 {
    fold();
    account_row(a).account_bytes
}

/// The holder's account, found by its name (none before the holder existed).
fn holder() -> Option<Acct> {
    (1..memledger::BACKEND_SLOT_BASE).map(Acct).find(|a| a.name() == "shared_stats")
}

fn setup() {
    timestamp_seams::get_current_timestamp::set(|| 1_000_000);
    xact_seams::get_current_transaction_stop_timestamp::set(|| 1_000_000);
    xact_seams::is_transaction_or_transaction_block::set(|| false);
    backend_status_seams::pgstat_clear_backend_status_snapshot::set(|| {});
    ipc_seams::before_shmem_exit::set(|f, _| {
        *HOOK.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
        Ok(())
    });
    pgstat::init_seams();
}

#[test]
fn a_standbys_backend_entry_leaves_neither_its_leader_nor_its_slot_below_base() {
    setup();
    set_home(true);
    let leader = Acct::backend(4000);
    let (made, parked) = (Arc::new(Barrier::new(STANDBYS as usize + 1)), Arc::new(Barrier::new(STANDBYS as usize + 1)));
    let (go_make, go_park) = (Arc::new(Barrier::new(STANDBYS as usize + 1)), Arc::new(Barrier::new(STANDBYS as usize + 1)));
    // the leader holds its slot throughout
    bind_backend(4000, 9000);
    let l0 = account(leader);
    let h0 = holder().map(account);
    let standbys: Vec<_> = (0..STANDBYS)
        .map(|i| {
            let (made, parked, go_make, go_park) = (made.clone(), parked.clone(), go_make.clone(), go_park.clone());
            std::thread::spawn(move || {
                let procno = 4010 + i;
                bind_backend(procno, 9010 + procno);
                let me = Acct::backend(procno);
                miscinit::SetMyBackendType(types_core::BackendType::Backend);
                init_small::globals::SetMyProcNumber(procno as i32);
                init_small::globals::SetMyDatabaseId(0);
                let s0 = account(me);
                go_make.wait();
                {
                    // its leader's task: the standby serves it entered as the leader, and takes its stats identity there
                    let _task = Home::of(leader).enter();
                    pgstat::pgstat_initialize().unwrap();
                    pgstat::backend::pgstat_create_backend(procno as i32);
                }
                made.wait();
                go_park.wait();
                // its park, on its own slot: the shutdown hook pgstat_initialize registered
                let hook = HOOK.lock().unwrap_or_else(|e| e.into_inner()).expect("pgstat registered its shutdown hook");
                hook(0, datum::Datum::from_usize(0)).unwrap();
                let moved = account(me) - s0;
                unbind_backend();
                parked.wait();
                moved
            })
        })
        .collect();
    go_make.wait();
    made.wait();
    let l_made = account(leader) - l0;
    let held = holder().map(account);
    go_park.wait();
    parked.wait();
    let moved: Vec<i64> = standbys.into_iter().map(|t| t.join().unwrap()).collect();
    let l_parked = account(leader) - l0;
    let back = holder().map(account);
    unbind_backend();
    let below: i64 = moved.iter().filter(|m| **m < 0).sum();
    assert!(l_made.abs() < SLACK, "the leader's account kept {l_made} B of its standbys' backend entries ({STANDBYS} x {ENTRY} B)");
    assert!(below.abs() < SLACK, "the standbys' slots went {below} B below their bases at their parks ({moved:?})");
    assert!(l_parked.abs() < SLACK, "the leader's account after the parks: {l_parked} B");
    let (h0, held, back) = (h0.expect("the shared stats holder's account"), held.unwrap(), back.unwrap());
    assert!(held - h0 >= STANDBYS as i64 * ENTRY, "the holder carries the entries: {} B", held - h0);
    assert!(held - back >= STANDBYS as i64 * ENTRY, "and gives them back at the parks: {} B", held - back);
}
