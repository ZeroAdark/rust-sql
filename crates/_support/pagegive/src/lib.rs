
#[cfg(target_os = "linux")]
const PMD: usize = 2 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mapping {
    Allocator { huge: bool },
    Accounted { huge: bool },
    /// A MAP_NORESERVE mapping of the caller's own (not the allocator's: no name is set; the caller re-applies its own
    /// name and advice, which a fresh mapping does not keep).
    Reserved { huge: bool },
    /// The page pool's own memory (crate pagepool): NORESERVE, named `pgrust:pool`.
    Pool { huge: bool },
}

/// The global allocator (mimalloc) names every mapping it makes `mimalloc` (`PR_SET_VMA_ANON_NAME`); the fresh mapping
/// over a range of its takes the same name, so /proc/<pid>/smaps still reads the range as the allocator's (the memory
/// ledger's RETAINED row counts the allocator's resident bytes by that name) and the mapping merges back into its
/// neighbours. A kernel without anonymous VMA names refuses the name; nothing else changes.
#[cfg(target_os = "linux")]
fn name_allocator(lo: usize, len: usize) {
    // <linux/prctl.h>
    const PR_SET_VMA: libc::c_int = 0x5356_4d41;
    const PR_SET_VMA_ANON_NAME: libc::c_ulong = 0;
    // SAFETY: prctl reads the NUL-terminated name (the kernel copies it) and the range, which is one anonymous mapping
    // just made; a refusal changes nothing.
    let _ = unsafe { libc::prctl(PR_SET_VMA, PR_SET_VMA_ANON_NAME, lo as libc::c_ulong, len as libc::c_ulong, c"mimalloc".as_ptr()) };
}

/// The page pool's mappings are named `pgrust:pool`, so /proc/<pid>/smaps reads them apart from the allocator's.
#[cfg(target_os = "linux")]
fn name_pool(lo: usize, len: usize) {
    const PR_SET_VMA: libc::c_int = 0x5356_4d41;
    const PR_SET_VMA_ANON_NAME: libc::c_ulong = 0;
    // SAFETY: as `name_allocator`.
    let _ = unsafe { libc::prctl(PR_SET_VMA, PR_SET_VMA_ANON_NAME, lo as libc::c_ulong, len as libc::c_ulong, c"pgrust:pool".as_ptr()) };
}

#[cfg(target_os = "linux")]
fn holds_whole_pmd(lo: usize, len: usize) -> bool {
    let p = lo.next_multiple_of(PMD);
    p.checked_add(PMD).is_some_and(|e| e <= lo.saturating_add(len))
}

/// # Safety
/// As `madvise(MADV_DONTNEED)` over the range: page-aligned, the caller's own, its bytes dead.
#[cfg(target_os = "linux")]
pub unsafe fn dontneed_split(lo: usize, len: usize) -> core::ffi::c_int {
    let end = lo + len;
    let mut cur = lo;
    let mut rc = 0;
    while cur < end {
        let p = cur.next_multiple_of(PMD);
        let stop = if p <= end && end - p >= PMD { p + PMD / 2 } else { end };
        // SAFETY: a page-aligned sub-range of the caller's range (`PMD / 2` keeps page alignment).
        rc |= unsafe { libc::madvise(cur as *mut libc::c_void, stop - cur, libc::MADV_DONTNEED) };
        cur = stop;
    }
    rc
}

/// # Safety
/// As `madvise(MADV_DONTNEED)` over the range: page-aligned, inside one mapping of the caller's own described by
/// `mapping`, its bytes dead, and nothing else touching it during the call. Not for a live thread's stack
/// ([`give_back_stack`]).
#[cfg(target_os = "linux")]
pub unsafe fn give_back(lo: usize, len: usize, mapping: Mapping) -> core::ffi::c_int {
    if len == 0 {
        return 0;
    }
    if !holds_whole_pmd(lo, len) {
        // SAFETY: the caller's contract; no whole 2 MiB-aligned range, so no table can be reclaimed
        return unsafe { libc::madvise(lo as *mut libc::c_void, len, libc::MADV_DONTNEED) };
    }
    let (extra, huge) = match mapping {
        Mapping::Allocator { huge } => (libc::MAP_NORESERVE, huge),
        Mapping::Accounted { huge } => (0, huge),
        Mapping::Reserved { huge } => (libc::MAP_NORESERVE, huge),
        Mapping::Pool { huge } => (libc::MAP_NORESERVE, huge),
    };
    let flags = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED | extra;
    // SAFETY: MAP_FIXED replaces only the caller's own range (its contract), with the protection it had (read-write)
    let p = unsafe { libc::mmap(lo as *mut libc::c_void, len, libc::PROT_READ | libc::PROT_WRITE, flags, -1, 0) };
    if p as usize != lo {
        // SAFETY: the caller's contract
        return unsafe { dontneed_split(lo, len) };
    }
    if let Mapping::Allocator { .. } = mapping {
        name_allocator(lo, len);
    }
    if let Mapping::Pool { .. } = mapping {
        name_pool(lo, len);
    }
    if huge {
        // SAFETY: advice only; the bytes are unchanged. A refusal leaves base pages, which is correct.
        unsafe { libc::madvise(lo as *mut libc::c_void, len, libc::MADV_HUGEPAGE) };
    }
    0
}

/// # Safety
/// As `madvise(MADV_DONTNEED)` over the range: page-aligned, inside the calling thread's stack below its live frames.
#[cfg(target_os = "linux")]
pub unsafe fn give_back_stack(lo: usize, len: usize) -> core::ffi::c_int {
    // SAFETY: the caller's contract
    unsafe { dontneed_split(lo, len) }
}

/// D4689: pins that failed in this process (a thread whose exit then gives its whole stack reservation back), for a diag view.
static PIN_FAILURES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static PIN_WARNED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// D4689: the number of `pinned` threads whose stack pin failed.
pub fn pin_failures() -> u64 {
    PIN_FAILURES.load(core::sync::atomic::Ordering::Relaxed)
}

/// D4689: counts a failed pin and warns once per process, naming `errno` and the RLIMIT_MEMLOCK soft / hard limits
/// (`None`: unreadable). Never refuses the spawn. Returns whether this call printed the warning.
pub fn note_pin_failure(errno: i32, memlock: Option<(u64, u64)>) -> bool {
    use core::sync::atomic::Ordering::Relaxed;
    PIN_FAILURES.fetch_add(1, Relaxed);
    if PIN_WARNED.swap(true, Relaxed) {
        return false;
    }
    let lim = |v: u64| if v == u64::MAX { "unlimited".to_string() } else { v.to_string() };
    let limits = memlock.map_or("unreadable".to_string(), |(c, m)| format!("soft {} hard {}", lim(c), lim(m)));
    eprintln!("WARNING:  pagegive: the thread stack pin failed (errno {errno}); RLIMIT_MEMLOCK {limits}: this thread's exit may give its whole stack reservation back to the kernel");
    true
}

#[cfg(target_os = "linux")]
fn memlock_limits() -> Option<(u64, u64)> {
    let mut rl = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: a read of this process's limit into a local.
    (unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut rl) } == 0).then_some((rl.rlim_cur as u64, rl.rlim_max as u64))
}

#[cfg(target_os = "linux")]
pub fn pin_thread_stack() -> bool {
    // SAFETY: the standard self-probe of the calling thread's attributes; `attr` is destroyed after the reads, and the
    // locked page lies inside the calling thread's own stack.
    unsafe {
        let mut attr: libc::pthread_attr_t = core::mem::zeroed();
        if libc::pthread_getattr_np(libc::pthread_self(), &mut attr) != 0 {
            return false;
        }
        let (mut lo, mut size, mut guard) = (core::ptr::null_mut::<libc::c_void>(), 0usize, 0usize);
        let ok = libc::pthread_attr_getstack(&attr, &mut lo, &mut size) == 0;
        let _ = libc::pthread_attr_getguardsize(&attr, &mut guard);
        libc::pthread_attr_destroy(&mut attr);
        let page = usize::try_from(libc::sysconf(libc::_SC_PAGESIZE)).ok().filter(|p| p.is_power_of_two()).unwrap_or(4096);
        if !ok || lo.is_null() || size <= guard + 2 * page {
            return false;
        }
        let at = (lo as usize + guard).next_multiple_of(page);
        libc::mlock(at as *const libc::c_void, page) == 0
    }
}

/// `pin_thread_stack` that reports a failure (D4689): the errno of the failing call is read at once.
#[cfg(target_os = "linux")]
fn pin_or_note() {
    if !pin_thread_stack() {
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        note_pin_failure(errno, memlock_limits());
    }
}

#[cfg(not(target_os = "linux"))]
fn pin_or_note() {}

#[cfg(not(target_os = "linux"))]
pub fn pin_thread_stack() -> bool {
    false
}

pub fn pinned<F, T>(f: F) -> impl FnOnce() -> T
where
    F: FnOnce() -> T,
{
    move || {
        pin_or_note();
        f()
    }
}

/// The bytes of `[lo, hi)` resident now (mincore(2)): its whole pages, read `buf.len()` pages a call (`buf` is the
/// caller's scratch, one byte a page). `None` when the page size is unknown, `buf` is empty, or part of the range is not
/// mapped (a mapping gone since its owner listed it).
#[cfg(target_os = "linux")]
pub fn resident_bytes(lo: usize, hi: usize, buf: &mut [u8]) -> Option<usize> {
    // SAFETY: sysconf has no preconditions.
    let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).ok().filter(|p| p.is_power_of_two())?;
    if buf.is_empty() {
        return None;
    }
    let (mut at, end) = (lo.checked_next_multiple_of(page)?, hi & !(page - 1));
    let mut pages = 0usize;
    while at < end {
        let len = (end - at).min(buf.len() * page);
        // SAFETY: `[at, at + len)` is page-aligned; mincore writes one byte per page of it into `buf`, which holds
        // `len / page` of them. A page of it not mapped fails the call (ENOMEM) and nothing is read.
        if unsafe { libc::mincore(at as *mut libc::c_void, len, buf.as_mut_ptr()) } != 0 {
            return None;
        }
        pages += buf[..len / page].iter().filter(|b| **b & 1 != 0).count();
        at += len;
    }
    Some(pages * page)
}

#[cfg(not(target_os = "linux"))]
pub fn resident_bytes(_lo: usize, _hi: usize, _buf: &mut [u8]) -> Option<usize> {
    None
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn whole_pmd_test() {
        assert!(!holds_whole_pmd(PMD + 4096, PMD));
        assert!(holds_whole_pmd(PMD, PMD));
        assert!(holds_whole_pmd(PMD - 4096, PMD + 4096));
        assert!(!holds_whole_pmd(PMD - 4096, PMD));
        assert!(!holds_whole_pmd(0, 0));
    }

    fn map(len: usize) -> usize {
        // SAFETY: a fresh private anonymous mapping
        let p = unsafe {
            libc::mmap(core::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0)
        };
        assert_ne!(p, libc::MAP_FAILED);
        p as usize
    }

    #[test]
    fn give_back_reads_zero_and_keeps_the_mapping() {
        let len = 8 * PMD;
        let base = map(len);
        for mapping in [Mapping::Allocator { huge: true }, Mapping::Accounted { huge: false }] {
            // SAFETY: the test's own mapping
            unsafe { core::ptr::write_bytes(base as *mut u8, 0xAB, len) };
            let (lo, n) = (base + 4096, len - 2 * 4096);
            // SAFETY: page-aligned, inside the test's own mapping, bytes dead
            assert_eq!(unsafe { give_back(lo, n, mapping) }, 0);
            // SAFETY: the mapping is still there, read-write
            let s = unsafe { core::slice::from_raw_parts_mut(base as *mut u8, len) };
            assert!(s[4096..len - 4096].iter().all(|&b| b == 0));
            assert_eq!((s[4095], s[len - 4096]), (0xAB, 0xAB));
            s[PMD] = 7; // writable again
            assert_eq!(s[PMD], 7);
        }
        // SAFETY: as above
        unsafe { core::ptr::write_bytes(base as *mut u8, 0xCD, len) };
        // SAFETY: as above
        assert_eq!(unsafe { give_back(base + 4096, 8192, Mapping::Allocator { huge: false }) }, 0);
        // SAFETY: as above
        let s = unsafe { core::slice::from_raw_parts(base as *const u8, 4 * 4096) };
        assert!(s[4096..3 * 4096].iter().all(|&b| b == 0) && s[0] == 0xCD && s[3 * 4096] == 0xCD);
        // SAFETY: the test's own mapping
        unsafe { libc::munmap(base as *mut libc::c_void, len) };
    }

    #[test]
    fn pool_give_back_names_the_range() {
        let len = 4 * PMD;
        let base = map(len);
        // SAFETY: the test's own mapping
        unsafe { core::ptr::write_bytes(base as *mut u8, 0x3C, len) };
        let lo = base.next_multiple_of(PMD);
        // SAFETY: page-aligned, inside the test's own mapping, bytes dead
        assert_eq!(unsafe { give_back(lo, PMD, Mapping::Pool { huge: true }) }, 0);
        // SAFETY: the mapping is still there, read-write
        assert!(unsafe { core::slice::from_raw_parts(lo as *const u8, PMD) }.iter().all(|&b| b == 0));
        let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
        let named = maps.lines().any(|l| l.starts_with(&format!("{lo:x}-")) && l.ends_with("[anon:pgrust:pool]"));
        // a kernel without anonymous VMA names (before 5.17) refuses the name and keeps the mapping
        let names = maps.contains("[anon:");
        assert!(named || !names, "no [anon:pgrust:pool] mapping at {lo:x}");
        // SAFETY: the test's own mapping
        unsafe { libc::munmap(base as *mut libc::c_void, len) };
    }

    #[test]
    fn stack_give_back_reads_zero() {
        let len = 4 * PMD;
        let base = map(len);
        // SAFETY: the test's own mapping
        unsafe { core::ptr::write_bytes(base as *mut u8, 0x5A, len) };
        // SAFETY: as above
        assert_eq!(unsafe { give_back_stack(base, len) }, 0);
        // SAFETY: as above
        assert!(unsafe { core::slice::from_raw_parts(base as *const u8, len) }.iter().all(|&b| b == 0));
        // SAFETY: as above
        unsafe { libc::munmap(base as *mut libc::c_void, len) };
    }

    #[test]
    fn pin_locks_a_page_of_a_spawned_thread() {
        let pinned = std::thread::Builder::new().stack_size(8 << 20).spawn(pin_thread_stack).unwrap().join().unwrap();
        // an RLIMIT_MEMLOCK under a page refuses the lock; anything else locks one page
        let mut rl = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        // SAFETY: a read of this process's limit
        unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut rl) };
        // SAFETY: a pure read
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
        assert_eq!(pinned, rl.rlim_cur >= page);
    }

    #[test]
    fn a_failed_pin_is_counted_and_warns_once() {
        let before = pin_failures();
        let first = note_pin_failure(12, Some((0, 0)));
        let second = note_pin_failure(12, None);
        assert_eq!(pin_failures(), before + 2, "every failure counts");
        assert!(!second, "the warning prints once per process");
        let _ = first;
    }

    /// RLIMIT_MEMLOCK 0 in a child process: `pinned` fails the lock, still runs its closure, and counts the failure.
    #[test]
    fn pinned_with_no_memlock_counts_and_still_runs() {
        if std::env::var_os("PAGEGIVE_CHILD").is_some() {
            let rl = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
            // SAFETY: lowers this child process's own limit.
            assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &rl) }, 0);
            let n = std::thread::Builder::new().stack_size(8 << 20).spawn(pinned(|| 7)).unwrap().join().unwrap();
            assert_eq!(n, 7);
            assert_eq!(pin_failures(), 1);
            return;
        }
        let me = std::env::current_exe().unwrap();
        let out = std::process::Command::new(me)
            .args(["tests::pinned_with_no_memlock_counts_and_still_runs", "--exact", "--nocapture", "--test-threads", "1"])
            .env("PAGEGIVE_CHILD", "1")
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "child failed: {err}");
        assert!(err.contains("WARNING:  pagegive: the thread stack pin failed (errno ") && err.contains("soft 0 hard 0"), "{err}");
    }

    #[inline(never)]
    fn dig(depth: usize) -> u8 {
        let mut buf = [0u8; 64 << 10];
        std::hint::black_box(&mut buf)[depth % 1024] = depth as u8;
        let below = if depth == 0 { 0 } else { dig(depth - 1) };
        below.wrapping_add(std::hint::black_box(&buf)[depth % 1024])
    }

    #[test]
    fn pinned_thread_exit_leaves_its_deep_stack_alone() {
        let mut rl = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        // SAFETY: a read of this process's limit
        unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut rl) };
        // SAFETY: a pure read
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let (tx, rx) = std::sync::mpsc::channel();
        let h = std::thread::Builder::new()
            .stack_size(32 << 20)
            .spawn(pinned(move || {
                // SAFETY: the calling thread's own attributes, read and destroyed here
                let (lo, size) = unsafe {
                    let mut attr: libc::pthread_attr_t = core::mem::zeroed();
                    assert_eq!(libc::pthread_getattr_np(libc::pthread_self(), &mut attr), 0);
                    let (mut lo, mut size) = (core::ptr::null_mut::<libc::c_void>(), 0usize);
                    assert_eq!(libc::pthread_attr_getstack(&attr, &mut lo, &mut size), 0);
                    libc::pthread_attr_destroy(&mut attr);
                    (lo as usize, size)
                };
                let here = &lo as *const usize as usize;
                std::hint::black_box(dig(160)); // ~10 MiB of 64 KiB frames below `here`
                // SAFETY: gettid has no preconditions
                let tid = unsafe { libc::syscall(libc::SYS_gettid) } as libc::pid_t;
                tx.send((lo, size, here, tid)).unwrap();
            }))
            .unwrap();
        let (lo, size, here, tid) = rx.recv().unwrap();
        assert!(lo < here && here <= lo + size);
        let t0 = std::time::Instant::now();
        // SAFETY: signal 0 probes for the task's existence
        while unsafe { libc::syscall(libc::SYS_tgkill, libc::getpid(), tid, 0) } == 0 {
            assert!(t0.elapsed() < std::time::Duration::from_secs(30), "thread {tid} did not exit");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        if rl.rlim_cur < page as u64 {
            h.join().unwrap(); // no lock possible: nothing to check
            return;
        }
        let hi = (here - (1 << 20)) & !(page - 1);
        let from = hi - (8 << 20);
        assert!(from > lo + 2 * page);
        let mut v = vec![0u8; (hi - from) / page];
        // SAFETY: a residency query over a range of the thread's stack, still mapped until the join
        assert_eq!(unsafe { libc::mincore(from as *mut libc::c_void, hi - from, v.as_mut_ptr()) }, 0);
        let gone = v.iter().filter(|&&b| b & 1 == 0).count();
        h.join().unwrap();
        assert_eq!(gone, 0, "{gone} of {} deep stack pages were given back at thread exit", v.len());
    }
}
