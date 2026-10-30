//! System-backed small runs must work without a pool reservation or background purger.
#![cfg(all(feature = "global", not(target_family = "wasm"), not(miri)))]

mod common;
use pagepool::PoolAlloc;
use std::alloc::{GlobalAlloc, Layout};

static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn without_pool() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let mut config = common::cfg();
        config.va = 0;
        assert!(!pagepool::init(&config));
    });
    assert!(!pagepool::on());
}

#[test]
fn idle_system_runs_stay_local_and_retire_on_thread_exit() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    without_pool();
    for cache in [false, true] {
        let before = pagepool::small_counts();
        std::thread::spawn(move || {
            let layout = Layout::from_size_align(64, 8).unwrap();
            // SAFETY: this thread owns the allocation through its one deallocation.
            unsafe {
                let p = if cache {
                    pagepool::cache_alloc(layout.size(), layout.align(), false)
                } else {
                    PoolAlloc.alloc(layout)
                };
                assert!(!p.is_null() && !pagepool::owns(p));
                p.write_bytes(73, layout.size());
                pagepool::idle_point();
                assert!(
                    std::slice::from_raw_parts(p, layout.size())
                        .iter()
                        .all(|&b| b == 73)
                );
                PoolAlloc.dealloc(p, layout);
            }
            pagepool::idle_point();
        })
        .join()
        .unwrap();
        let after_exit = pagepool::small_counts();
        // Negative controls must also clean up their abandoned system allocation.
        pagepool::flush_remote();
        assert_eq!(after_exit.fresh - before.fresh, 1);
        assert_eq!(
            after_exit.retired - before.retired,
            1,
            "idle handoff left a system run waiting for a purger that cannot start (cache={cache})"
        );
    }
}

#[test]
fn synchronous_reclamation_flushes_remote_fallback_frees_with_pool_off() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    without_pool();
    for cache in [false, true] {
        let before = pagepool::small_counts();
        let address = std::thread::spawn(move || {
            let layout = Layout::from_size_align(64, 8).unwrap();
            // SAFETY: ownership transfers to the parent, which frees after this thread exits.
            let p = unsafe {
                if cache {
                    pagepool::cache_alloc(layout.size(), layout.align(), false)
                } else {
                    PoolAlloc.alloc(layout)
                }
            };
            assert!(!p.is_null() && !pagepool::owns(p));
            unsafe { p.write_bytes(91, layout.size()) };
            p as usize
        })
        .join()
        .unwrap();
        let layout = Layout::from_size_align(64, 8).unwrap();
        unsafe {
            let p = address as *mut u8;
            assert!(
                std::slice::from_raw_parts(p, layout.size())
                    .iter()
                    .all(|&b| b == 91)
            );
            PoolAlloc.dealloc(p, layout);
        }
        assert_eq!(pagepool::return_warm_now(0), 0);
        assert_eq!(pagepool::small_counts().retired, before.retired);
        // There are no pool warm bytes, but the remote free must still reach the system allocator.
        assert_eq!(pagepool::return_warm_now(usize::MAX), 0);
        let after_release = pagepool::small_counts();
        pagepool::flush_remote();
        assert_eq!(
            after_release.retired - before.retired,
            1,
            "synchronous reclamation skipped remote frees when only the global allocator was on (cache={cache})"
        );
    }
}
