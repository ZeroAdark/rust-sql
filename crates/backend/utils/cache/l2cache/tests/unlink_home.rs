//! A catalog L2 entry unlinked on another session's thread (a DROP DATABASE's forget, the ladder's trim) comes home to
//! the account it was published from: the L2 charge's estimate goes back there and, the L2 holding its last `Arc`, its
//! blocks are freed there too (D4552 (a)). The unlinking thread's account stays where it was. Before this the estimate
//! went back to the unlinking thread while door 1 took the real blocks off it: (blocks - estimate) below zero there, and
//! as much left on the publisher (on UNBOUND after its unbind). Its own binary: the global allocator is door 1.

use std::alloc::{GlobalAlloc, Layout, System};
use std::any::Any;
use std::sync::{mpsc, Arc, Mutex};

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

const MIB: usize = 1 << 20;
/// The entry's estimate, far below its blocks (an `approx_bytes` undercounts the same way, by less).
const EST: usize = 1024;
/// What a spawn / join and a channel move between accounts: far below the gap judged.
const SLACK: i64 = 64 << 10;
static ONE: Mutex<()> = Mutex::new(());

fn account(a: Acct) -> i64 {
    fold();
    account_row(a).account_bytes
}

/// A 1 MiB entry published under `relid` in database `db` with a 1 KiB estimate.
fn publish(relid: u32, db: u32) {
    let v: Arc<dyn Any + Send + Sync> = Arc::new(vec![7u8; MIB]);
    let gen = l2cache::current_gen(l2cache::Domain::Rel(relid));
    drop(l2cache::insert(l2cache::rel_key(relid, db), gen, v, EST, |a| a.is::<Vec<u8>>()));
}

#[test]
fn a_dropped_databases_entries_come_home_not_onto_the_dropping_session() {
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    set_home(true);
    let u0 = account(Acct::UNBOUND);
    // a session of database 71 publishes and ends: its net (blocks - estimate) moves to UNBOUND at its unbind
    std::thread::spawn(|| {
        bind_backend(3990, 9990);
        publish(1_000_300, 71);
        unbind_backend();
    })
    .join()
    .unwrap();
    // another session drops database 71
    let (dropper, unbound) = std::thread::spawn(|| {
        bind_backend(3991, 9991);
        let a = Acct::backend(3991);
        let d0 = account(a);
        l2cache::forget_database(71);
        let d = account(a) - d0;
        unbind_backend();
        (d, account(Acct::UNBOUND))
    })
    .join()
    .unwrap();
    assert!(dropper.abs() < SLACK, "the dropping session's account moved {dropper} B (blocks - estimate is {} B)", MIB - EST);
    assert!((unbound - u0).abs() < SLACK, "the publisher's bytes on UNBOUND came back to zero: {} B left", unbound - u0);
}

/// Small entries, each under its own key (its own bucket): the L2's structure is most of what a forget frees.
const SMALL: u32 = 4000;

#[test]
fn a_dropped_databases_buckets_leave_the_l2_not_the_dropping_session() {
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    set_home(true);
    let u0 = account(Acct::UNBOUND);
    // a session of database 73 publishes many small entries and ends
    std::thread::spawn(|| {
        bind_backend(3994, 9994);
        for i in 0..SMALL {
            let relid = 1_000_400 + i;
            let v: Arc<dyn Any + Send + Sync> = Arc::new([7u8; 64]);
            let gen = l2cache::current_gen(l2cache::Domain::Rel(relid));
            // the estimate is the entry's block (its Arc's header and the array): only the L2's own bytes are left
            drop(l2cache::insert(l2cache::rel_key(relid, 73), gen, v, 80, |a| a.is::<[u8; 64]>()));
        }
        unbind_backend();
    })
    .join()
    .unwrap();
    // another session drops database 73: each key's bucket goes with its entry
    let (dropper, unbound) = std::thread::spawn(|| {
        bind_backend(3995, 9995);
        let a = Acct::backend(3995);
        let d0 = account(a);
        l2cache::forget_database(73);
        let d = account(a) - d0;
        unbind_backend();
        (d, account(Acct::UNBOUND))
    })
    .join()
    .unwrap();
    assert!(dropper.abs() < SLACK, "the dropping session's account moved {dropper} B ({SMALL} buckets freed)");
    assert!((unbound - u0).abs() < SLACK, "the publisher's bytes on UNBOUND came back to zero: {} B left", unbound - u0);
}

#[test]
fn a_trimmed_entry_comes_home_to_its_publisher_while_it_holds_its_slot() {
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    set_home(true);
    let (published, trimmed) = (mpsc::channel::<()>(), mpsc::channel::<()>());
    let (go, wait_trim) = (published.0, trimmed.1);
    let publisher = std::thread::spawn(move || {
        bind_backend(3992, 9992);
        let a = Acct::backend(3992);
        let p0 = account(a);
        publish(1_000_301, 72);
        let held = account(a) - p0;
        go.send(()).unwrap();
        wait_trim.recv().unwrap();
        let back = account(a) - p0;
        unbind_backend();
        (held, back)
    });
    let trimmer = std::thread::spawn(move || {
        bind_backend(3993, 9993);
        let a = Acct::backend(3993);
        published.1.recv().unwrap();
        let t0 = account(a);
        l2cache::trim_to(0);
        let t = account(a) - t0;
        trimmed.0.send(()).unwrap();
        unbind_backend();
        t
    });
    let (held, back) = publisher.join().unwrap();
    let t = trimmer.join().unwrap();
    assert!(held > (MIB - EST) as i64 - SLACK, "the publisher holds the blocks past the estimate: {held} B");
    assert!(t.abs() < SLACK, "the trimming session's account moved {t} B");
    assert!(back.abs() < SLACK, "the publisher's account is back at its start: {back} B");
}
