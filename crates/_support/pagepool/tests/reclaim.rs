//! Reuse before release: a 16 MiB pool whose every superpage holds free class-Task blocks in the central lists still
//! serves a Session take on each road (block, runs, pool span) by homing them, with no OS release; a pool full of live
//! blocks refuses and asks the purger for a return pass (low_warm).

mod common;
use common::*;
use core::ptr::NonNull;
use pagepool::*;

const RUN: usize = 64 << 10;

fn fill(h: Hint, bytes: usize) -> Vec<usize> {
    let mut v = Vec::new();
    while let Some(p) = take(bytes, 16, h) {
        v.push(p.cast::<u8>().as_ptr() as usize);
    }
    v
}

fn give_all(v: Vec<usize>, bytes: usize) {
    for a in v {
        // SAFETY: taken by the test, not used again.
        unsafe { give(NonNull::new(a as *mut u8).unwrap(), bytes) };
    }
}

#[test]
fn reclaim() {
    init_with(|c| {
        c.va = 16 * MIB;
        c.chunk = 8 * MIB;
        c.lazy_va = 0;
        c.frame_va = 0;
        c.shelf_max = 16 * MIB;
    });
    let task = Hint::new(Class::Task, Tag::OP);
    let t = fill(task, 64 << 10);
    let s0 = stats();
    assert!(s0.claimed >= 14 * MIB && s0.warm_zero + s0.warm_dirty == 0, "Task blocks hold the pool: {s0:?}");
    std::thread::spawn(move || give_all(t, 64 << 10)).join().unwrap();
    let s1 = stats();
    assert!(s1.central >= 12 * MIB, "the freed Task blocks sit in the central lists: {s1:?}");

    let s = Hint::new(Class::Session, Tag::MCX);
    let b = take(8 << 10, 16, s).expect("a Session block once Task's are free");
    let r = take(4 * RUN, RUN, s).expect("Session runs");
    let sp = span(2 * SUPER, s).expect("a Session pool span");
    let s2 = stats();
    assert_eq!(s2.releases, 0, "nothing went to the OS: {s2:?}");
    assert!(s2.central < s1.central, "the Task lists went home: {s2:?}");
    // SAFETY: ours, not used again.
    unsafe {
        give(b.cast(), 8 << 10);
        give(r.cast(), 4 * RUN);
        give_span(sp);
    }
    flush_shelf();
    balanced();

    assert!(start());
    let live = fill(s, 64 << 10);
    assert!(take(64 << 10, 16, Hint::new(Class::Process, Tag::REGION)).is_none(), "a pool of live blocks refuses");
    assert!(low_warm(), "a refusal asks the shelves home");
    let t0 = std::time::Instant::now();
    while hooks::return_pending() && t0.elapsed() < std::time::Duration::from_secs(5) {
        std::thread::yield_now();
    }
    assert!(!hooks::return_pending(), "the purger ran the return pass");
    give_all(live, 64 << 10);
    let p = take(64 << 10, 16, Hint::new(Class::Process, Tag::REGION)).expect("a Process block once Session's are given");
    // SAFETY: ours, not used again.
    unsafe { give(p.cast(), 64 << 10) };
    flush_shelf();
    assert_eq!(stats().releases, 0, "the return pass homes, it never releases");
    stop();
    balanced();
}
