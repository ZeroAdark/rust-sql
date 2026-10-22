//! pagepool P3 (c): `present` reads the pool's resident pages, not its claimed bytes. A pool span taken Cold counts in
//! `claimed` at once and in `present` only as it is written; a lazy span's declared commit (`commit_to`, no fault) the
//! same; given back and released, neither. A ledger that reads claimed as resident over-counts (measured: U0 -1.9 GiB).
//! `present()` is the purger's PUBLISHED sweep less the bytes released since (a read is two atomic
//! loads, never a sweep); these tests start no pool thread, so `refresh_present()` stands in for the purger's cadence.

mod common;
use common::*;
use pagepool::*;

/// The tests share one pool and its published reading: one at a time.
static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// D4842: a test's body, one at a time, on a thread of its own, joined before the next test starts. A thread that used the
/// pool gives its slot back as it exits (its pthread-key destructor: `flush_shelf`, `give_slot`), and that moves a count
/// the sweep cadence reads (`stats::activity`). libtest's thread for the previous test exits after its function returns
/// and drops `ONE`; on a loaded host that exit can land inside the next test's idle window and read as a moved count
/// (present.rs's "the period passed, nothing moved: no sweep" failed so under a release build). Joined, every exit is
/// over before the next body takes the lock's turn.
fn one(body: impl FnOnce() + Send) {
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    std::thread::scope(|s| {
        if let Err(p) = s.spawn(body).join() {
            std::panic::resume_unwind(p);
        }
    });
}

/// `present::QUIET_MS`: the cadence sweeps an unmoved pool once this passed since its last sweep.
const QUIET_MS: u64 = 10_000;
/// `present::FRESH_MS`: a fresh read shares a sweep this young.
const FRESH_MS: u64 = 250;
/// `present::PRESENT_MS`: the cadence's shortest period.
const PRESENT_MS: u64 = 100;

/// D4858: the windows over pages this test wrote ("half written", "4 MiB written") read short when the host reclaims
/// them to swap (a memory-pressed precheck box; a memcg cap). Such a window, short while pswpin / pswpout moved, is
/// SKIPPED LOUDLY, never judged either way (the D4815 treatment); every other check is asserted as before.
#[test]
fn present_counts_the_pools_resident_pages_not_its_claim() {
    one(|| {
        setup();
        let h = Hint::new(Class::Task, Tag::OTHER);
        let sw = swap_counts();
        let p0 = refresh_present().expect("the pool's present bytes");
        let c0 = stats().claimed;
        let s = span(16 * MIB, h).expect("a pool span");
        assert!(!s.is_lazy());
        assert!(stats().claimed >= c0 + 16 * MIB, "taken Cold: claimed at once");
        let mut lazy = None;
        let skipped = 'checks: {
            let p1 = refresh_present().expect("present");
            assert!(p1 < p0 + MIB, "claimed, not written: not present ({p0} -> {p1})");
            touch(s.as_ptr() as usize, 8 * MIB);
            let p2 = refresh_present().expect("present");
            if p2 < p1 + 8 * MIB && swap_moved(sw) {
                break 'checks Some(format!("half written: {} of 8 MiB present ({p1} -> {p2})", p2.saturating_sub(p1)));
            }
            assert!((p1 + 8 * MIB..=p1 + 10 * MIB).contains(&p2), "half written: half present ({p1} -> {p2})");

            let mut l = span(64 * MIB, h).expect("a lazy span");
            assert!(l.is_lazy());
            let c3 = stats().claimed;
            // SAFETY: our own span
            unsafe { commit_to(&mut l, 32 * MIB) };
            assert!(stats().claimed >= c3 + 32 * MIB, "a declared commit is claimed");
            let p3 = refresh_present().expect("present");
            assert!(p3 < p2 + MIB, "declared, not written: not present ({p2} -> {p3})");
            touch(l.as_ptr() as usize, 4 * MIB);
            let a = l.as_ptr() as usize;
            lazy = Some(l);
            let p4 = refresh_present().expect("present");
            if p4 < p3 + 4 * MIB && swap_moved(sw) {
                break 'checks Some(format!("4 MiB written: {} present ({p3} -> {p4}; the span's own {})", p4.saturating_sub(p3), resident(a, 4 * MIB)));
            }
            assert!((p3 + 4 * MIB..=p3 + 6 * MIB).contains(&p4), "4 MiB written: 4 MiB present ({p3} -> {p4})");
            assert!(stats().claimed >= p4, "claimed bounds present from above");
            assert_eq!(present(), Some(p4), "the published read is the last sweep while nothing was released");
            None
        };

        // SAFETY: ours, not used again
        unsafe {
            give_span(s);
            if let Some(l) = lazy {
                give_span(l);
            }
        }
        let _ = purge_now(usize::MAX >> 1, None);
        let p5 = present().expect("present");
        let p6 = refresh_present().expect("present");
        // p0 itself reads short by what was on swap then and has come back since
        let skipped = match skipped {
            None if (p5 > p0 + 16 * MIB || p6 > p0 + MIB) && swap_moved(sw) => Some(format!("released: {p0} -> {p5} / {p6}")),
            k => k,
        };
        if skipped.is_none() {
            // pppress (the overshoot fix): before a sweep the release comes off at most the last sweep's free part
            // (these spans were live then), so the published read never rises; the next sweep shows it whole
            assert!(p5 <= p0 + 16 * MIB, "released: never above the last sweep, before any sweep ({p0} -> {p5})");
            assert!(p6 <= p0 + MIB, "released: no longer present ({p0} -> {p6})");
        }
        balanced();
        if let Some(why) = skipped {
            // straight to fd 2, not eprintln!: libtest captures eprintln! (this thread's too) and shows it only for a
            // failed test, and a SKIP passes, so the line would never reach the log
            use std::io::Write as _;
            let _ = writeln!(std::io::stderr(), "SKIP (D4858, the host swapped during the test): {why}; the checks after it were not judged");
        }
    });
}

/// Condition 3's witness: a published read is within a few % of a full sweep, costs well under 1 us, shows a release at
/// once and a fault at the next sweep.
#[test]
fn a_present_read_is_published_cheap_and_tracks_a_full_sweep() {
    one(|| {
        setup();
        let h = Hint::new(Class::Task, Tag::OTHER);
        // a pool span (under lazy_min 32 MiB), 16 MiB of it written
        let s = span(24 * MIB, h).expect("a pool span");
        assert!(!s.is_lazy());
        touch(s.as_ptr() as usize, 16 * MIB);
        let swept = refresh_present().expect("a sweep");
        let read = present().expect("published");
        let full = present_sweep().expect("a full sweep");
        let tol = (full / 50).max(2 * MIB);
        assert!(read.abs_diff(full) <= tol, "published {read} vs full sweep {full} (tolerance {tol})");
        assert_eq!(read, swept, "nothing released since the sweep");

        // the cost of a read: two atomic loads under a sequence count, no syscall
        let n = 1_000_000u32;
        let t0 = std::time::Instant::now();
        let mut acc = 0usize;
        for _ in 0..n {
            acc = acc.wrapping_add(core::hint::black_box(present()).unwrap_or(0));
        }
        let per = t0.elapsed().as_nanos() as f64 / n as f64;
        core::hint::black_box(acc);
        // a sweep's cost beside it, over this pool
        let t1 = std::time::Instant::now();
        let _ = present_sweep();
        let sweep_ns = t1.elapsed().as_nanos();
        eprintln!("MEASURED present() {per:.1} ns a read; one full sweep {sweep_ns} ns over {} MiB present", full / MIB);
        assert!(per < 1_000.0, "a present() read costs {per:.1} ns, the bar is < 1 us");

        // a fault after the sweep shows at the next sweep, not before
        let more = 4 * MIB;
        // inside our span (16..20 MiB of 24)
        touch(s.as_ptr() as usize + 16 * MIB, more);
        assert_eq!(present(), Some(read), "no sweep yet: the published bytes");
        let after = refresh_present().expect("a sweep");
        assert!((read + more..=read + more + 2 * MIB).contains(&after), "the next sweep shows the fault ({read} -> {after})");

        // a release of pages the sweep read free shows at once (the released-bytes count), no sweep. pppress (the
        // overshoot fix): a release comes off at most the sweep's free part, so pages freed since the sweep show at the
        // next one; given back, then swept, they are free
        // SAFETY: ours, not used again
        unsafe { give_span(s) };
        let after = refresh_present().expect("a sweep: the given span's pages read free");
        let _ = purge_now(usize::MAX >> 1, None);
        let gone = present().expect("published");
        assert!(gone + 20 * MIB <= after + 2 * MIB, "released: the published read drops at once ({after} -> {gone})");
        balanced();
    });
}

/// D4842: the pool's counts as one array, read without allocating (with `--features global` the pool is the allocator:
/// a read that allocated would move what it reads).
fn counts() -> [u64; 16] {
    let t = stats();
    let in_use: usize = t.in_use.iter().sum();
    [
        t.claimed as u64,
        t.central as u64,
        t.shelved as u64,
        t.dead as u64,
        t.stranded as u64,
        t.waste as u64,
        in_use as u64,
        t.lazy_committed as u64,
        t.warm_misses,
        t.claims,
        t.releases,
        t.released_bytes,
        t.warm_takes,
        t.cold_takes,
        t.prefaulted,
        t.claim_held,
    ]
}

/// D4842: `f` run, with an upper bound on the cadence's age (ms since the published sweep) at the moment `f` read it: the
/// age before plus the time `f` took. A starved thread reads a window longer than it slept; the bound says by how much.
fn aged<T>(f: impl FnOnce() -> T) -> (T, u64) {
    let a0 = present_age_ms().unwrap_or(0);
    let t0 = std::time::Instant::now();
    let r = f();
    (r, a0 + t0.elapsed().as_millis() as u64 + 1)
}

/// P3 (c): the purger's sweep cadence skips a pool where no count moved (an idle server swept ~3.6 % of a core), and
/// sweeps again once one does; `present_fresh` sweeps on the caller whatever the cadence (a view's same-point read).
/// D4842: each check reads a window (since the last sweep, or the fresh read); one this thread overran (starved past
/// QUIET_MS, FRESH_MS or the period), or one a count it did not move crossed (a `--features global` harness's own
/// allocations), is SKIPPED LOUDLY, never judged either way. The rest are asserted as before.
#[test]
fn an_idle_pool_is_not_swept_and_a_fresh_read_sweeps_now() {
    one(|| {
        setup();
        let h = Hint::new(Class::Task, Tag::OTHER);
        let s = span(8 * MIB, h).expect("a pool span");
        touch(s.as_ptr() as usize, 4 * MIB);
        let mut t = None;
        let skipped = 'checks: {
            let p0 = refresh_present().expect("a sweep");
            let (n0, c0) = (present_sweeps(), counts());
            // past PRESENT_MS (the cadence's period) and FRESH_MS (a fresh read's sharing window), under QUIET_MS
            std::thread::sleep(std::time::Duration::from_millis(300));
            let (due, ub) = aged(present_due_now);
            if due && ub >= QUIET_MS {
                break 'checks Some(format!("the idle window read up to {ub} ms after its sweep, past QUIET_MS"));
            }
            if due && counts() != c0 {
                break 'checks Some("a count this test did not move moved in the idle window".to_string());
            }
            assert!(!due, "the period passed, nothing moved: no sweep (read up to {ub} ms after it)");
            assert_eq!(present_sweeps(), n0);
            // a first write to a page of a block taken earlier moves no count: a fresh read shows it, the cadence does not
            touch(s.as_ptr() as usize + 4 * MIB, 2 * MIB);
            let (due, ub) = aged(present_due_now);
            if due && ub >= QUIET_MS {
                break 'checks Some(format!("'no count moved' read up to {ub} ms after the sweep, past QUIET_MS"));
            }
            if due && counts() != c0 {
                break 'checks Some("a count this test did not move moved before 'no count moved'".to_string());
            }
            assert!(!due, "no count moved");
            assert_eq!(present(), Some(p0), "the published bytes, not swept since");
            let fresh = present_fresh().expect("a fresh read");
            assert!((p0 + 2 * MIB..=p0 + 4 * MIB).contains(&fresh), "a fresh read sweeps now ({p0} -> {fresh})");
            assert_eq!(present_sweeps(), n0 + 1);
            assert_eq!(present(), Some(fresh), "and publishes");
            // a second fresh read within FRESH_MS shares that sweep
            touch(s.as_ptr() as usize + 6 * MIB, 2 * MIB);
            let (again, ub) = aged(present_fresh);
            if again != Some(fresh) && ub >= FRESH_MS {
                break 'checks Some(format!("the second fresh read came up to {ub} ms after the first, past FRESH_MS"));
            }
            assert_eq!(again, Some(fresh), "a sweep this young is shared");
            assert_eq!(present_sweeps(), n0 + 1);
            // a take moves a count: the cadence sweeps once its period has passed since the fresh read
            t = Some(span(4 * MIB, h).expect("another pool span"));
            let (due, ub) = aged(present_due_now);
            if due && ub >= PRESENT_MS {
                break 'checks Some(format!("'within the period' read up to {ub} ms after the fresh read, past PRESENT_MS"));
            }
            assert!(!due, "within the period of the fresh read");
            // the period is PRESENT_MS, or DUTY times the last sweep's cost (`present_sweep_ns`; a sweep a loaded host
            // stalled stretches it): wait for the sweep, bounded well under QUIET_MS
            let w0 = std::time::Instant::now();
            while !present_due_now() {
                let ns = present_sweep_ns();
                assert!(w0.elapsed() < std::time::Duration::from_secs(5), "a count moved and the period passed: a sweep (none in 5 s, last sweep {ns} ns)");
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let c1 = counts();
            let (due, ub) = aged(present_due_now);
            if due && ub >= QUIET_MS {
                break 'checks Some(format!("'none again' read up to {ub} ms after the sweep, past QUIET_MS"));
            }
            if due && counts() != c1 {
                break 'checks Some("a count this test did not move moved before 'none again'".to_string());
            }
            assert!(!due, "and none again until something moves");
            None
        };
        // SAFETY: ours, not used again
        unsafe {
            give_span(s);
            if let Some(t) = t {
                give_span(t);
            }
        }
        let _ = purge_now(usize::MAX >> 1, None);
        balanced();
        if let Some(why) = skipped {
            // straight to fd 2, not eprintln!: libtest captures eprintln! (this thread's too, inherited from the
            // test's) and shows it only for a failed test, and a SKIP passes, so the line would never reach the log
            use std::io::Write as _;
            let _ = writeln!(std::io::stderr(), "SKIP (D4842, a starved or crossed window): {why}; the checks after it were not judged");
        }
    });
}

/// The resident bytes smaps names the pool's in this process: [anon:pgrust:pool] + [anon:pgrust:pool-meta].
#[cfg(target_os = "linux")]
fn smaps_pool() -> (usize, usize) {
    let (mut pool, mut meta, mut cur) = (0, 0, 0u8);
    for line in std::fs::read_to_string("/proc/self/smaps").expect("smaps").lines() {
        let head = line.split(' ').next().unwrap_or("");
        if head.contains('-') && head.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
            cur = if line.ends_with("[anon:pgrust:pool]") { 1 } else if line.ends_with("[anon:pgrust:pool-meta]") { 2 } else { 0 };
        } else if cur != 0 && line.starts_with("Rss:") {
            let kb: usize = line.split_whitespace().nth(1).and_then(|v| v.parse().ok()).unwrap_or(0);
            if cur == 1 {
                pool += kb << 10;
            } else {
                meta += kb << 10;
            }
        }
    }
    (pool, meta)
}

/// Whether this kernel names anonymous mappings (PR_SET_VMA_ANON_NAME, CONFIG_ANON_VMA_NAME): without it smaps shows no
/// [anon:pgrust:*] row at all and the pool's naming is a no-op (os::set_name refuses harmlessly), so there is nothing to compare.
#[cfg(target_os = "linux")]
fn kernel_names_vmas() -> bool {
    const PR_SET_VMA: libc::c_int = 0x5356_4d41;
    const PR_SET_VMA_ANON_NAME: libc::c_ulong = 0;
    // SAFETY: a private one-page anonymous mapping of our own, named and unmapped here
    unsafe {
        let len = 4096;
        let p = libc::mmap(core::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0);
        if p == libc::MAP_FAILED {
            return false;
        }
        let rc = libc::prctl(PR_SET_VMA, PR_SET_VMA_ANON_NAME, p as libc::c_ulong, len as libc::c_ulong, c"pgrust:probe".as_ptr());
        libc::munmap(p, len);
        rc == 0
    }
}

/// Under load the ledger's unattributed row sat ~+190 MiB over pool-off because the sweep missed pool memory smaps names the pool's: a
/// lazy span's huge-page tail past its commit, and the metadata zone. The sweep now reads what smaps reads.
#[cfg(target_os = "linux")]
#[test]
fn a_sweep_reads_what_smaps_names_the_pools() {
    if !kernel_names_vmas() {
        eprintln!("SKIP: this kernel does not name anonymous mappings (PR_SET_VMA_ANON_NAME); smaps has no pool rows to compare");
        return;
    }
    one(|| {
        setup();
        let h = Hint::new(Class::Task, Tag::OTHER);
        // a lazy span whose commit ends mid-superpage, written to its end (a huge-page fault may fill the superpage)
        let mut l = span(64 * MIB, h).expect("a lazy span");
        assert!(l.is_lazy());
        // SAFETY: our own span
        unsafe { commit_to(&mut l, 3 * MIB) };
        touch(l.as_ptr() as usize, 3 * MIB);
        let s = span(8 * MIB, h).expect("a pool span");
        touch(s.as_ptr() as usize, 5 * MIB);
        let tail = resident(l.as_ptr() as usize, 4 * MIB);
        let swept = present_sweep().expect("a sweep");
        let (pool, meta) = smaps_pool();
        eprintln!("MEASURED sweep {swept} smaps pool {pool} + meta {meta}; lazy span resident {tail} of its 3 MiB commit's 4 MiB");
        assert!(swept.abs_diff(pool + meta) <= 256 << 10, "the sweep {swept} vs smaps {} (pool {pool} + meta {meta})", pool + meta);
        let _ = refresh_present();
        let m = meta_present().expect("the metadata part");
        let (_, meta_now) = smaps_pool();
        assert!(m > 0 && m.abs_diff(meta_now) <= 256 << 10, "meta_present {m} vs smaps meta {meta_now} (before the sweep {meta})");
        let (lp, llen) = (l.as_ptr() as usize, l.len());
        pagepool::hooks::clear();
        // SAFETY: ours, not used again
        unsafe {
            give_span(s);
            give_span(l);
        }
        let _ = purge_now(usize::MAX >> 1, None);
        // the warm lazy span's release takes its huge-page tail too, inside the span, through give_back only (CVE-2026-74674)
        assert_eq!(resident(lp, 4 * MIB), 0, "the lazy span's tail past its 3 MiB commit is released");
        let (mut tail, mut bad, mut outside) = (false, Vec::new(), Vec::new());
        let complete = pagepool::hooks::each(|op, at, len, arg| match op {
            pagepool::hooks::Op::Madvise if arg == 4 && at.next_multiple_of(SUPER) + SUPER <= at + len => bad.push((at, len)),
            pagepool::hooks::Op::GiveBack if at >= lp && at < lp + llen => {
                tail |= at == lp && len == 4 * MIB;
                if at + len > lp + llen {
                    outside.push((at, len));
                }
            }
            _ => {}
        });
        assert!(complete, "the syscall log overflowed");
        assert!(bad.is_empty(), "MADV_DONTNEED over a whole 2 MiB range: {bad:x?}");
        assert!(outside.is_empty(), "a release past the span's end: {outside:x?}");
        assert!(tail, "the lazy span released [base, base + 4 MiB): its commit rounded up to the superpage");
        balanced();
    });
}
