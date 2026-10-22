//! D319: a wait for another thread's build takes the caller's wait step and ends when that step says so (its interrupt
//! check: a cancel, a statement_timeout), or reads the table once its builder publishes it; this thread's own build is
//! `Held`, never a wait.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use gravity_dict::{Claim, Header, Key, OnceTable, Region};

fn table() -> OnceTable {
    let cells: &'static mut [u64] = Box::leak(vec![0u64; 1024].into_boxed_slice());
    // SAFETY: a leaked 8-aligned buffer, alive for the process.
    let mut r = unsafe { Region::new(cells.as_mut_ptr() as *mut u8, cells.len() * 8) };
    let once = OnceTable::in_region(&mut r, 4).unwrap();
    once.begin_query(1);
    once
}

#[test]
fn a_wait_for_another_threads_build_ends_on_the_interrupt_check_or_reads_the_published_table() {
    let once = table();
    let k = Key { part: 3, epoch: 1, fingerprint: 0x319 };
    let Claim::Build(b) = once.claim(k) else { panic!("this thread builds") };
    assert!(matches!(once.claim(k), Claim::Held), "this thread's own build: held, never a wait");
    let cancel = AtomicBool::new(false);
    let (tx, rx) = mpsc::channel();
    std::thread::scope(|s| {
        s.spawn(|| {
            // the caller's wait step: its backoff, then its interrupt check (false ends the wait)
            let r = once.claim_while(k, &mut || {
                std::thread::yield_now();
                !cancel.load(Ordering::Acquire)
            });
            let _ = tx.send(matches!(r, Claim::Cancelled));
        });
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err(), "another thread's claim waits for the build");
        cancel.store(true, Ordering::Release);
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)), Ok(true), "the interrupt ends the wait");
    });
    drop(b);
    // a waiter the interrupt check never stops reads the table once its builder publishes it
    let k2 = Key { part: 4, epoch: 1, fingerprint: 0x319 };
    let Claim::Build(b2) = once.claim(k2) else { panic!("this thread builds") };
    std::thread::scope(|s| {
        let waiter = s.spawn(|| match once.claim_while(k2, &mut || {
            std::thread::yield_now();
            true
        }) {
            Claim::Ready(h) => h as usize,
            _ => 0,
        });
        std::thread::sleep(Duration::from_millis(50));
        b2.publish(0x1000 as *const Header);
        assert_eq!(waiter.join().unwrap(), 0x1000);
    });
}
