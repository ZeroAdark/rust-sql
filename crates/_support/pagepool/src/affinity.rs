//! The CPU set of the pool's helper threads. The claimer's prefault places most of the pool's memory on its own NUMA
//! node (first touch), so on a multi-node machine it must run where the runtime's workers run. Each helper applies the
//! set to itself at its next wake; a set the kernel refuses leaves its mask as it was.

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering::*};

/// CPU_SETSIZE: the set names CPUs below it.
pub const HELPER_MAX_CPUS: usize = 1024;
const WORDS: usize = HELPER_MAX_CPUS / 64;

static MASK: [AtomicU64; WORDS] = [const { AtomicU64::new(0) }; WORDS];
static STATE: AtomicU8 = AtomicU8::new(0);
const SET: u8 = 2;
static PINNED: AtomicU32 = AtomicU32::new(0);
static FAILED: AtomicU32 = AtomicU32::new(0);

/// False, nothing changed: `cpus` is empty, names a CPU at or past [`HELPER_MAX_CPUS`], or a set was made before.
pub fn set_helper_cpus(cpus: &[usize]) -> bool {
    if cpus.is_empty() || cpus.iter().any(|&c| c >= HELPER_MAX_CPUS) {
        return false;
    }
    if STATE.compare_exchange(0, 1, Acquire, Relaxed).is_err() {
        return false;
    }
    let mut words = [0u64; WORDS];
    for &c in cpus {
        words[c / 64] |= 1 << (c % 64);
    }
    for (m, w) in MASK.iter().zip(words) {
        m.store(w, Relaxed);
    }
    STATE.store(SET, Release);
    crate::claimer::unpark();
    crate::purger::wake();
    true
}

/// The helpers' pins so far: (applied, refused by the kernel).
pub fn helper_pins() -> (u32, u32) {
    (PINNED.load(Relaxed), FAILED.load(Relaxed))
}

#[inline]
pub(crate) fn follow(applied: &mut bool) {
    if *applied || STATE.load(Acquire) != SET {
        return;
    }
    *applied = true;
    pin_self();
}

#[cfg(all(target_os = "linux", not(miri)))]
fn pin_self() {
    // SAFETY: a zeroed cpu_set_t is empty, every CPU set is < CPU_SETSIZE, and tid 0 is the calling thread.
    let rc = unsafe {
        let mut set: libc::cpu_set_t = core::mem::zeroed();
        for (i, m) in MASK.iter().enumerate() {
            let mut w = m.load(Relaxed);
            while w != 0 {
                libc::CPU_SET(i * 64 + w.trailing_zeros() as usize, &mut set);
                w &= w - 1;
            }
        }
        libc::sched_setaffinity(0, core::mem::size_of::<libc::cpu_set_t>(), &set)
    };
    if rc == 0 {
        PINNED.fetch_add(1, Relaxed);
        return;
    }
    static WARNED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
    let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    FAILED.fetch_add(1, Relaxed);
    if !WARNED.swap(true, Relaxed) {
        eprintln!("pgrust: page pool helper cpu affinity refused (errno {errno}); running unpinned");
    }
}

#[cfg(not(all(target_os = "linux", not(miri))))]
fn pin_self() {}
