//! D319: the thread that holds a key's build never waits for itself: a second claim of the key before the build ends
//! returns at once (a program's table stays building until its LANE_DONE, and a second table lane of the same key claimed
//! it meanwhile: the statement spun forever, deaf to cancel, statement_timeout and SIGTERM). The waits for another
//! thread's build: tests/d319_cancel.rs.
use std::sync::mpsc;
use std::time::Duration;

use gravity_dict::{Claim, Key, OnceTable, Region};

fn table() -> OnceTable {
    let cells: &'static mut [u64] = Box::leak(vec![0u64; 1024].into_boxed_slice());
    // SAFETY: a leaked 8-aligned buffer, alive for the process.
    let mut r = unsafe { Region::new(cells.as_mut_ptr() as *mut u8, cells.len() * 8) };
    let once = OnceTable::in_region(&mut r, 4).unwrap();
    once.begin_query(1);
    once
}

#[test]
fn a_second_claim_of_a_key_this_thread_builds_returns_at_once() {
    let once = table();
    let k = Key { part: 3, epoch: 1, fingerprint: 0xd319 };
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let Claim::Build(b) = once.claim(k) else { panic!("the first claimer builds") };
        // the build is pending (a program's table until its LANE_DONE) when a second lane of the same key claims it
        let second = once.claim(k);
        let _ = tx.send(matches!(second, Claim::Build(_) | Claim::Ready(_) | Claim::Full));
        drop(b);
    });
    // before D319 the second claim waited for its own build forever
    assert_eq!(rx.recv_timeout(Duration::from_secs(20)), Ok(false), "the second claim returns at once, and with no table");
}
