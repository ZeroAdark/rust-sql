//! D656: a backend thread that ends without its session teardown (a crash or a kill: launch_backend skips the drain on
//! kill-class exits) unmaps its levels' admission arenas at the thread's end. Before, the reentry levels (`ManuallyDrop`)
//! kept every killed backend's 1 GiB arena mapping, and the memory pool's hold on it, for the life of the process: the
//! server's address space and resident size grew at each crash-restart (11.2 GB after a W0 crash-restart vs 6.6 GB before).

use gravity_exec::admit::reentry;
use gravity_exec::Backend;

fn build(_depth: usize) -> Box<Backend> {
    Box::new(Backend::new_for_test()) // alloc: Cold
}

/// Whether an address lies in a mapping of this process (`/proc/self/maps`).
fn mapped(addr: usize) -> bool {
    std::fs::read_to_string("/proc/self/maps").expect("/proc/self/maps").lines().any(|l| {
        let r = l.split_whitespace().next().unwrap_or("");
        let (a, b) = r.split_once('-').unwrap_or(("0", "0"));
        let (a, b) = (usize::from_str_radix(a, 16).unwrap_or(0), usize::from_str_radix(b, 16).unwrap_or(0));
        (a..b).contains(&addr)
    })
}

/// A level's arena address, taken on a thread that maps it (a top-level statement and one nested under it) and then ends
/// with its levels built: no `reentry::teardown`, as after a kill.
fn arenas_of_a_killed_thread() -> Vec<usize> {
    std::thread::spawn(|| {
        let r = reentry::enter(Some(build), None, false, |b, _| {
            let top = b.graph_arena().expect("the arena maps").alloc(64) as usize;
            let nested = reentry::enter(Some(build), None, false, |b, _| b.graph_arena().expect("the arena maps").alloc(64) as usize);
            vec![top, nested.expect("level 1").expect("built")]
        });
        let addrs = r.expect("level 0").expect("built");
        for a in &addrs {
            assert!(mapped(*a), "mapped while the thread runs");
        }
        addrs
    })
    .join()
    .expect("the thread")
}

/// One test (the file's only one): a parallel test's arena could be mapped at a released address and read as a leak.
#[cfg(target_os = "linux")]
#[test]
fn d656_a_thread_that_ends_without_its_teardown_unmaps_its_admission_arenas() {
    for addr in arenas_of_a_killed_thread() {
        assert!(!mapped(addr), "an admission arena outlived its thread ({addr:#x}): leaked at every crash-restart");
    }
    // a clean session end unmaps them itself, and the thread's end finds nothing left to release
    let addr = std::thread::spawn(|| {
        let a = reentry::enter(Some(build), None, false, |b, _| b.graph_arena().expect("the arena maps").alloc(64) as usize);
        let a = a.expect("level 0").expect("built");
        reentry::teardown();
        assert!(!mapped(a), "the session teardown unmaps it");
        a
    })
    .join()
    .expect("the thread");
    assert!(!mapped(addr));
}
