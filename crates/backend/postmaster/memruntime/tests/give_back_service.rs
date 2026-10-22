//! H3 step (1) (`give_back_service`), the broker's condition 3: the give-back service's thread (pg:memrelease) runs at
//! normal priority, the spawning thread's nice and scheduling policy. At nice 10, a saturating load starved the purger
//! past its dead-man stretch (memruntime's purge.rs), and an ordinary collect on a query thread purged its arenas
//! instead. The knob also puts the service's admission in place. Its own test binary: the knob and the thread are
//! process-wide.
#![cfg(target_os = "linux")]

/// The thread of this process named `name`, by its comm.
fn tid_of(name: &str) -> Option<i32> {
    std::fs::read_dir("/proc/self/task").ok()?.flatten().find_map(|e| {
        let comm = std::fs::read_to_string(e.path().join("comm")).ok()?;
        if comm.trim_end() != name {
            return None;
        }
        let tid = e.file_name();
        tid.to_str()?.parse().ok()
    })
}

#[test]
fn the_give_back_service_thread_runs_at_the_spawning_threads_priority() {
    assert!(!memruntime::service::enabled(), "give_back_service defaults to 0");
    mempolicy::consts::set(mempolicy::consts::K::GiveBackService, 1);
    assert!(memruntime::service::enabled());
    assert!(memruntime::release::start(), "the knob starts pg:memrelease");
    assert!(allocarena::own::service_cap() > 0, "the service's admission is in place");
    let tid = (0..500)
        .find_map(|_| {
            let t = tid_of("pg:memrelease");
            if t.is_none() {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            t
        })
        .expect("the pg:memrelease thread");
    // SAFETY: plain reads of two threads' scheduling parameters.
    let (nice, own_nice, policy) = unsafe {
        (
            libc::getpriority(libc::PRIO_PROCESS, tid as libc::id_t),
            libc::getpriority(libc::PRIO_PROCESS, libc::gettid() as libc::id_t),
            libc::sched_getscheduler(tid),
        )
    };
    assert_eq!(nice, own_nice, "the service's nice is the spawning thread's");
    assert_eq!(policy, libc::SCHED_OTHER, "the normal scheduling policy");
}
