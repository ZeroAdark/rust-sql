//! A parked, still-live thread must not pin one mostly empty current run per size class.
#![cfg(all(feature = "global", not(target_family = "wasm"), not(miri)))]

mod common;
use pagepool::{PoolAlloc, Tag};
use std::alloc::{GlobalAlloc, Layout};
use std::sync::mpsc;
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn synchronous_pressure_reclaims_runs_from_exited_idle_threads() {
    let _serial = SERIAL.lock().unwrap();
    common::setup();
    let before = pagepool::stats().in_use[Tag::SMALL.0 as usize];
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(64));
    let mut threads = Vec::new();
    for _ in 0..64 {
        let barrier = barrier.clone();
        threads.push(std::thread::spawn(move || {
            let mut held = Vec::new();
            for size in [64, 256, 1024] {
                let layout = Layout::from_size_align(size, 8).unwrap();
                // SAFETY: each block is owned by this thread until its one deallocation below.
                let p = unsafe { PoolAlloc.alloc(layout) };
                assert!(!p.is_null() && pagepool::owns(p));
                unsafe { p.write_bytes(42, size) };
                held.push((p, layout));
            }
            // Establish the peak population before any thread hands off its empty runs.
            barrier.wait();
            for (p, layout) in held {
                unsafe { PoolAlloc.dealloc(p, layout) };
            }
            pagepool::idle_point();
        }));
    }
    for thread in threads {
        thread.join().unwrap();
    }
    let retained = pagepool::stats().in_use[Tag::SMALL.0 as usize] - before;
    assert_eq!(retained, 64 * 3 * (64 << 10));
    assert_eq!(pagepool::return_warm_now(0), 0);
    assert_eq!(
        pagepool::stats().in_use[Tag::SMALL.0 as usize] - before,
        retained
    );
    let released = pagepool::return_warm_now(usize::MAX);
    let remaining = pagepool::stats().in_use[Tag::SMALL.0 as usize] - before;
    // Clean up even on the negative control, before asserting the synchronous pressure contract.
    pagepool::flush_remote();
    assert_eq!(
        remaining, 0,
        "synchronous pressure left idle runs waiting for a background sweep"
    );
    assert!(released >= retained);
}

#[test]
fn idle_cycles_reuse_formatted_empty_runs_until_reclamation() {
    let _serial = SERIAL.lock().unwrap();
    common::setup();
    let capacity_before = pagepool::stats().in_use[Tag::SMALL.0 as usize];
    let before = pagepool::small_counts();
    for _ in 0..200 {
        for size in [64, 256, 1024] {
            let layout = Layout::from_size_align(size, 8).unwrap();
            // SAFETY: allocate, initialize, and free one valid block, with no outstanding references.
            unsafe {
                let p = PoolAlloc.alloc(layout);
                assert!(!p.is_null() && pagepool::owns(p));
                p.write_bytes(42, size);
                PoolAlloc.dealloc(p, layout);
            }
        }
        pagepool::idle_point();
    }
    let fresh = pagepool::small_counts().fresh - before.fresh;
    // An explicit reclamation still returns empty shared runs, despite keeping them formatted between commands.
    pagepool::flush_remote();
    assert_eq!(
        pagepool::stats().in_use[Tag::SMALL.0 as usize],
        capacity_before
    );
    assert!(
        (1..=3).contains(&fresh),
        "200 idle cycles rebuilt {fresh} runs for three size classes"
    );
}

#[test]
fn idle_threads_share_current_runs_and_resume_after_remote_frees() {
    let _serial = SERIAL.lock().unwrap();
    common::setup();
    let before = pagepool::stats().in_use[Tag::SMALL.0 as usize];
    let live_before = pagepool::global_live();
    let mut threads = Vec::new();
    let mut wakes = Vec::new();
    let mut allocations = Vec::new();
    for id in 0..64u8 {
        let (send, receive) = mpsc::channel();
        let (wake, sleep) = mpsc::channel();
        wakes.push(wake);
        threads.push(std::thread::spawn(move || {
            let mut held = Vec::new();
            for size in [64, 256, 1024] {
                let layout = Layout::from_size_align(size, 8).unwrap();
                // SAFETY: a valid nonzero layout. The parent frees the 64-byte block; this thread retains ownership of the others.
                let p = unsafe { PoolAlloc.alloc(layout) };
                assert!(!p.is_null() && pagepool::owns(p));
                unsafe { p.write_bytes(id, size) };
                held.push((p as usize, layout, id));
            }
            pagepool::idle_point();
            send.send(held.clone()).unwrap();
            sleep.recv().unwrap();
            // Other threads may now be adopting these runs: an old owner must use the remote-free path correctly.
            for (address, layout, _) in held {
                if layout.size() != 64 {
                    unsafe { PoolAlloc.dealloc(address as *mut u8, layout) };
                }
            }
            // The original owner stays alive and later allocates again, after another thread freed its blocks.
            for n in 0..256 {
                let layout = Layout::from_size_align([64, 256, 1024][n % 3], 8).unwrap();
                let p = unsafe { PoolAlloc.alloc(layout) };
                assert!(!p.is_null());
                unsafe {
                    p.write_bytes(id, layout.size());
                    assert!(std::slice::from_raw_parts(p, layout.size())
                        .iter()
                        .all(|&b| b == id));
                    PoolAlloc.dealloc(p, layout);
                }
            }
            pagepool::idle_point();
        }));
        // Deliberately establish idle runs before the next thread starts, so reuse is deterministic.
        allocations.extend(receive.recv().unwrap());
    }
    let capacity = pagepool::stats().in_use[Tag::SMALL.0 as usize] - before;
    eprintln!("idle small-run capacity: {capacity} bytes");
    for (address, layout, id) in allocations {
        // SAFETY: the owners stay parked during this read; free only the blocks they transferred to the parent.
        unsafe {
            let p = address as *mut u8;
            assert!(std::slice::from_raw_parts(p, layout.size())
                .iter()
                .all(|&b| b == id));
            if layout.size() == 64 {
                PoolAlloc.dealloc(p, layout);
            }
        }
    }
    for wake in wakes {
        wake.send(()).unwrap();
    }
    for thread in threads {
        thread.join().unwrap();
    }
    pagepool::flush_remote();
    assert_eq!(pagepool::stats().in_use[Tag::SMALL.0 as usize], before);
    assert_eq!(pagepool::global_live(), live_before);
    // Three classes need four runs for these live blocks; allow two spare runs, not 64 private sets.
    assert!(
        capacity <= 6 * (64 << 10),
        "idle threads retained {capacity} bytes of small-run capacity"
    );
}
