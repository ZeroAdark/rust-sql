use super::{ProcGlobal, ProcStructLock, spin_acquire, INVALID_PROC_NUMBER};

// Worker-pool claims of a bgworker PGPROC not yet popped by InitProcess;
// guarded by ProcStructLock. C has no equivalent: every worker is a fresh
// fork whose InitProcess pops the freelist directly.
static BGW_RESERVED: core::sync::atomic::AtomicI32 = core::sync::atomic::AtomicI32::new(0);

std::thread_local! {
    static BGW_HOLDS_RESERVATION: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

pub fn reserve_bgworker_proc() -> bool {
    use core::sync::atomic::Ordering::Relaxed;
    let hdr = ProcGlobal();
    spin_acquire(&ProcStructLock);
    let reserved = BGW_RESERVED.load(Relaxed);
    let mut nfree = 0;
    // SAFETY: [PSL] bgworkerFreeProcs traversed under ProcStructLock.
    let mut cur = unsafe { hdr.bgworkerFreeProcs.get() }.head;
    while cur != INVALID_PROC_NUMBER && nfree <= reserved {
        nfree += 1;
        // SAFETY: [PSL] freelist links traversed under ProcStructLock.
        cur = unsafe { hdr.allProcs[cur as usize].links.get() }.next;
    }
    let ok = nfree > reserved;
    if ok {
        BGW_RESERVED.store(reserved + 1, Relaxed);
    }
    ProcStructLock.unlock();
    ok
}

pub fn unreserve_bgworker_proc() {
    spin_acquire(&ProcStructLock);
    let r = BGW_RESERVED.load(core::sync::atomic::Ordering::Relaxed);
    BGW_RESERVED.store((r - 1).max(0), core::sync::atomic::Ordering::Relaxed);
    ProcStructLock.unlock();
}

pub fn adopt_bgworker_reservation() {
    BGW_HOLDS_RESERVATION.with(|h| h.set(true));
}

pub fn release_bgworker_reservation() {
    if BGW_HOLDS_RESERVATION.with(|h| h.replace(false)) {
        unreserve_bgworker_proc();
    }
}
