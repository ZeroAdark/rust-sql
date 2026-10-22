//! Shared by the crate's tests: a small pool, fault counts, residency, the big-leg switch.
#![allow(dead_code)]

use pagepool::Config;

pub const MIB: usize = 1 << 20;
pub const SUPER: usize = 2 * MIB;

/// The tests' pool: 1 GiB of pool address space in 256 MiB chunks, small zones, no claimer, pressure-only release.
pub fn cfg() -> Config {
    Config {
        va: 1 << 30,
        chunk: 256 * MIB,
        lazy_va: 8 << 30,
        frame_va: 1 << 30,
        shelf_max: 4 * MIB,
        warm_zero: 0,
        warm_zero_max: 0,
        warm_cap: 0,
        decay_ms: 0,
        purge_batch: 2 * MIB,
        lazy_min: 32 * MIB,
        idle_drain_ms: 1000,
        block_min: 1024,
        release_mode: 2,
        lazy_decay_ms: 0,
        lazy_cap: 0,
        age_pass_max: 0,
        thp_partial: 0,
        host_age_ms: 0,
    }
}

pub fn init_with(f: impl FnOnce(&mut Config)) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let mut c = cfg();
        f(&mut c);
        assert!(pagepool::init(&c), "pagepool::init refused {c:?}");
    });
}

pub fn setup() {
    init_with(|_| {});
}

/// The calling thread's minor faults so far.
pub fn minflt() -> i64 {
    #[cfg(target_os = "linux")]
    let who = libc::RUSAGE_THREAD;
    #[cfg(not(target_os = "linux"))]
    let who = libc::RUSAGE_SELF;
    // SAFETY: a zeroed rusage the call fills.
    let mut r: libc::rusage = unsafe { core::mem::zeroed() };
    // SAFETY: `r` is valid for the call to write.
    assert_eq!(unsafe { libc::getrusage(who, &mut r) }, 0);
    r.ru_minflt as i64
}

/// Resident bytes of `[p, p + len)`.
pub fn resident(p: usize, len: usize) -> usize {
    let page = pagepool::hooks::page_size();
    let lo = p & !(page - 1);
    let hi = (p + len).next_multiple_of(page);
    let mut v = vec![0u8; (hi - lo) / page];
    // SAFETY: mincore over the pool's own mapped range into a vector of the right length.
    let rc = unsafe { libc::mincore(lo as *mut libc::c_void, hi - lo, v.as_mut_ptr() as *mut _) };
    assert_eq!(rc, 0, "mincore");
    v.iter().filter(|b| **b & 1 != 0).count() * page
}

/// Write one byte per page.
pub fn touch(p: usize, len: usize) {
    let page = pagepool::hooks::page_size();
    let mut a = p;
    while a < p + len {
        // SAFETY: inside a block or span the test holds.
        unsafe { core::ptr::write_volatile(a as *mut u8, 1) };
        a += page;
    }
}

/// The host's THP fault fallbacks so far (/proc/vmstat; 0 where there is none): a huge fault the kernel served with
/// small pages, as on a host whose memory is fragmented.
pub fn thp_fallbacks() -> u64 {
    let v = std::fs::read_to_string("/proc/vmstat").unwrap_or_default();
    v.lines().find_map(|l| l.strip_prefix("thp_fault_fallback ")).and_then(|n| n.trim().parse().ok()).unwrap_or(0)
}

/// Pages the host has swapped out so far (/proc/vmstat pswpout; 0 where there is none).
pub fn swapped_out() -> u64 {
    let v = std::fs::read_to_string("/proc/vmstat").unwrap_or_default();
    v.lines().find_map(|l| l.strip_prefix("pswpout ")).and_then(|n| n.trim().parse().ok()).unwrap_or(0)
}

/// The host's (pswpin, pswpout) page counts (/proc/vmstat; 0 where unreadable).
pub fn swap_counts() -> (u64, u64) {
    let v = std::fs::read_to_string("/proc/vmstat").unwrap_or_default();
    let get = |k: &str| v.lines().find_map(|l| l.strip_prefix(k)).and_then(|n| n.trim().parse().ok()).unwrap_or(0);
    (get("pswpin "), get("pswpout "))
}

/// Whether the host swapped a page in or out since `since` (`swap_counts()`).
pub fn swap_moved(since: (u64, u64)) -> bool {
    swap_counts() != since
}

/// `$cond` exactly, or a loud named SKIP (the calling test function returns) when the host swapped a page in or out since
/// `$sw` (`swap_counts()`): a page on swap is not resident, so resident reads, a pass's budget (resident bytes given back)
/// and fault counts move with it. lazy_cap's form (its own copy), for the tests that count resident bytes or faults.
#[macro_export]
macro_rules! swap_exact {
    ($sw:expr, $cond:expr, $($msg:tt)+) => {
        if !$cond {
            if $crate::common::swap_moved($sw) {
                // straight to fd 2, not eprintln!: libtest captures eprintln! and shows it only for a failed test, and a
                // SKIP passes, so the line would never reach the log
                use std::io::Write as _;
                let _ = writeln!(
                    std::io::stderr(),
                    "SKIP {}: the host swapped during the test (pswpin / pswpout moved): `{}` not exact here: {}",
                    module_path!(),
                    stringify!($cond),
                    format!($($msg)+)
                );
                return;
            }
            panic!("`{}` failed: {}", stringify!($cond), format!($($msg)+));
        }
    };
}

/// A count `got` of `want` written bytes: whole, or short by no more than the pages the host swapped out since `since`
/// (`swapped_out()` before they were written). A page on swap is not resident (PAGEMAP_SCAN, /proc/self/pagemap and
/// the memcg charge agree), so on a host with swap under memory pressure a count of resident bytes can come up short
/// by it; said loudly when it does (a shortfall swap does not explain fails).
pub fn whole_but_swap(got: usize, want: usize, since: u64) -> bool {
    if got == want {
        return true;
    }
    let out = swapped_out().saturating_sub(since) as usize * pagepool::hooks::page_size();
    let ok = got < want && want - got <= out;
    eprintln!(
        "{got} of {want} bytes counted, the host swapped out {out} bytes meanwhile: {}",
        if ok { "short by swap, allowed" } else { "NOT explained by swap" }
    );
    ok
}

/// Faults a warm path may still see from the host's own housekeeping (a khugepaged collapse or a compaction move of
/// a small page, on a host whose THP fell back); a cold path faults at least once per 2 MiB in every test that uses it.
pub const HOST_SLACK: i64 = 8;

pub fn big() -> bool {
    std::env::var("PAGEPOOL_BIG_TESTS").is_ok_and(|v| v == "1")
}

pub fn balanced() {
    if let Err(e) = pagepool::hooks::check() {
        panic!("{e}: {:?}", pagepool::stats());
    }
}

pub fn cpu_list(s: &str) -> Vec<usize> {
    let mut out = Vec::new();
    for item in s.trim().split(',').filter(|i| !i.is_empty()) {
        match item.split_once('-') {
            Some((a, b)) => out.extend(a.parse::<usize>().unwrap()..=b.parse::<usize>().unwrap()),
            None => out.push(item.parse::<usize>().unwrap()),
        }
    }
    out
}

#[cfg(target_os = "linux")]
pub fn current_cpus() -> Vec<usize> {
    let s = std::fs::read_to_string("/proc/thread-self/status").expect("/proc/thread-self/status");
    cpu_list(s.lines().find_map(|l| l.strip_prefix("Cpus_allowed_list:")).expect("Cpus_allowed_list"))
}

#[cfg(target_os = "linux")]
pub fn threads_cpus(comm: &str) -> Vec<(String, Vec<usize>)> {
    let mut out = Vec::new();
    for e in std::fs::read_dir("/proc/self/task").expect("/proc/self/task").flatten() {
        let p = e.path();
        if std::fs::read_to_string(p.join("comm")).is_ok_and(|c| c.trim() == comm) {
            if let Ok(s) = std::fs::read_to_string(p.join("status")) {
                if let Some(l) = s.lines().find_map(|l| l.strip_prefix("Cpus_allowed_list:")) {
                    out.push((e.file_name().to_string_lossy().into_owned(), cpu_list(l)));
                }
            }
        }
    }
    out
}

#[cfg(target_os = "linux")]
pub fn wait_helpers_on(comms: &[&str], want: &[usize]) {
    let t0 = std::time::Instant::now();
    loop {
        let seen: Vec<_> = comms.iter().map(|c| (*c, threads_cpus(c))).collect();
        if seen.iter().all(|(_, t)| !t.is_empty() && t.iter().all(|(_, cpus)| cpus == want)) {
            return;
        }
        assert!(t0.elapsed() < std::time::Duration::from_secs(10), "the helpers are not on {want:?}: {seen:?}");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

pub fn wait_pins(want: (u32, u32)) {
    let t0 = std::time::Instant::now();
    while pagepool::helper_pins() != want && t0.elapsed() < std::time::Duration::from_secs(10) {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(pagepool::helper_pins(), want, "the helpers' pins (applied, refused)");
}
