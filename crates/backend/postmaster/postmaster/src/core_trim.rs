// D3666: a crashed server's core without the address space nobody wrote.
//
// Backends are threads, so a crash dumps the whole server. Each child thread
// reserves max_stack_depth x STACK_DEPTH_SCALE + 8 MiB of stack (launch_backend
// child_thread_stack_size: 360 MiB at max_stack_depth = 60000) and writes a few
// hundred KiB of it; mimalloc's arenas and Gravity's regions reserve far more
// than they touch too. The kernel writes a touched anonymous VMA whole, its
// untouched pages as zeros when the core goes to a pipe (a core_pattern
// handler cannot seek), so a server with 0.5 GiB resident wrote a 13 GiB core
// and the supervisor's DOOMED_GRACE_MS kill cut it short (D3661). Before the
// re-raise, the crash handler marks MADV_DONTDUMP each run of at least
// TRIM_MIN_BYTES of pages that are neither present nor swapped in every
// private, writable, non-executable anonymous VMA of at least VMA_MIN_BYTES.
// Such a page reads as zero, so the core loses nothing. Then the buffer pool
// blocks the faulting thread was touching go back into the core (bufmgr
// leaves its block array out: extent.rs, PGRUST_CORE_DUMP_BUFFERS).
//
// PGRUST_CORE_TRIM=0 turns the trim off (read once at install).

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// A VMA smaller than this is left alone (thread sigaltstacks, small heaps).
const VMA_MIN_BYTES: usize = 8 << 20;
/// A run of dead pages shorter than this stays in the core.
const TRIM_MIN_BYTES: usize = 1 << 20;
/// Kept on each side of a trimmed run: a thread still running while the
/// handler trims may write next to the pages it already touched.
const MARGIN_BYTES: usize = 64 << 10;
/// /proc/self/pagemap: bit 63 = present in RAM, bit 62 = in swap.
const PM_LIVE: u64 = (1 << 63) | (1 << 62);

const MAPS_BYTES: usize = 64 << 10;
const MAX_VMAS: usize = 8192;
const PM_ENTRIES: usize = 8192;

static ENABLED: AtomicBool = AtomicBool::new(false);
static PAGE: AtomicUsize = AtomicUsize::new(0);
static RAN: AtomicBool = AtomicBool::new(false);

/// The trim's buffers: static, because the handler runs on the thread's small
/// sigaltstack. Only the thread that wins `RAN` touches them.
struct Bufs {
    maps: [u8; MAPS_BYTES],
    vmas: [(usize, usize); MAX_VMAS],
    pm: [u64; PM_ENTRIES],
}

struct Scratch(UnsafeCell<Bufs>);

// SAFETY: one thread at most uses the buffers (`RAN` is swapped once).
unsafe impl Sync for Scratch {}

static SCRATCH: Scratch = Scratch(UnsafeCell::new(Bufs {
    maps: [0; MAPS_BYTES],
    vmas: [(0, 0); MAX_VMAS],
    pm: [0; PM_ENTRIES],
}));

/// What one crash's trim did, for the handler's log line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Trimmed {
    pub vmas: usize,
    pub runs: usize,
    pub bytes: usize,
    /// Buffer pool bytes put back into the core.
    pub kept: usize,
}

/// At install (single-threaded boot): the page size and the switch.
pub(crate) fn install() {
    // SAFETY: sysconf has no pointer arguments.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page <= 0 || !(page as usize).is_power_of_two() {
        return;
    }
    PAGE.store(page as usize, Ordering::Relaxed);
    let off = std::env::var_os("PGRUST_CORE_TRIM").is_some_and(|v| v == "0");
    ENABLED.store(!off, Ordering::Relaxed);
}

/// The crash handler's call, once per process. `addrs` = the fault address and
/// the faulting thread's general registers.
///
/// Async-signal-safe: raw system calls only (open, read, pread, close,
/// madvise), static buffers, no allocation, no lock, no formatting, no panic
/// path; any error stops the trim and the core is as large as before.
pub(crate) fn at_crash(addrs: &[usize]) -> Trimmed {
    let mut t = Trimmed::default();
    if RAN.swap(true, Ordering::AcqRel) {
        return t;
    }
    let page = PAGE.load(Ordering::Relaxed);
    if page != 0 && ENABLED.load(Ordering::Relaxed) {
        // SAFETY: this thread won `RAN`, so it alone uses the buffers.
        let bufs = unsafe { &mut *SCRATCH.0.get() };
        trim(bufs, page, &mut t);
    }
    let (base, len) = bufmgr::blocks_span();
    t.kept = keep_blocks(base, len, types_core::BLCKSZ, addrs);
    t
}

fn trim(b: &mut Bufs, page: usize, t: &mut Trimmed) {
    let n = collect_vmas(b);
    // SAFETY: open(2) of a NUL-terminated literal path.
    let fd = unsafe { libc::open(c"/proc/self/pagemap".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return;
    }
    for i in 0..n {
        let (lo, hi) = b.vmas[i];
        t.vmas += 1;
        if !trim_vma(fd, lo, hi, page, &mut b.pm, t) {
            break;
        }
    }
    // SAFETY: closing the fd opened above.
    unsafe { libc::close(fd) };
}

/// The candidate VMAs from /proc/self/maps into `b.vmas`, all of them read
/// before any is trimmed (a trim splits VMAs); returns how many.
fn collect_vmas(b: &mut Bufs) -> usize {
    // SAFETY: open(2) of a NUL-terminated literal path.
    let fd = unsafe { libc::open(c"/proc/self/maps".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return 0;
    }
    let mut n = 0usize;
    let mut have = 0usize;
    loop {
        // SAFETY: read(2) into the unused tail of a static buffer.
        let r = unsafe { libc::read(fd, b.maps[have..].as_mut_ptr().cast(), MAPS_BYTES - have) };
        if r <= 0 {
            break;
        }
        have += r as usize;
        let mut start = 0usize;
        while let Some(nl) = b.maps[start..have].iter().position(|&c| c == b'\n') {
            if let Some(v) = candidate(&b.maps[start..start + nl]) {
                if n < MAX_VMAS {
                    b.vmas[n] = v;
                    n += 1;
                }
            }
            start += nl + 1;
        }
        b.maps.copy_within(start..have, 0);
        have -= start;
        if have == MAPS_BYTES {
            break; // a line longer than the buffer: stop here
        }
    }
    // SAFETY: closing the fd opened above.
    unsafe { libc::close(fd) };
    n
}

/// One /proc/self/maps line, `lo-hi perms offset dev inode [name]`: its range
/// when the VMA is private, readable, writable and not executable, anonymous
/// (inode 0; no name or a bracketed one such as [stack] / [anon:mimalloc]),
/// and at least VMA_MIN_BYTES.
fn candidate(line: &[u8]) -> Option<(usize, usize)> {
    let mut f = line.split(|&c| c == b' ').filter(|s| !s.is_empty());
    let range = f.next()?;
    let perms = f.next()?;
    let _offset = f.next()?;
    let _dev = f.next()?;
    let inode = f.next()?;
    let name = f.next();
    if perms.len() != 4 || perms[0] != b'r' || perms[1] != b'w' || perms[2] != b'-' || perms[3] != b'p' {
        return None;
    }
    if inode != b"0" || name.is_some_and(|s| s[0] != b'[') {
        return None;
    }
    let dash = range.iter().position(|&c| c == b'-')?;
    let lo = hex(&range[..dash])?;
    let hi = hex(&range[dash + 1..])?;
    (hi > lo && hi - lo >= VMA_MIN_BYTES).then_some((lo, hi))
}

fn hex(s: &[u8]) -> Option<usize> {
    if s.is_empty() || s.len() > 16 {
        return None;
    }
    let mut v = 0usize;
    for &c in s {
        let d = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            _ => return None,
        };
        v = (v << 4) | d as usize;
    }
    Some(v)
}

/// Runs of dead pages (neither present nor swapped) over a VMA's pagemap
/// entries, fed in windows.
struct RunScan {
    /// Start of the current dead run (usize::MAX = none).
    run_lo: usize,
}

impl RunScan {
    fn new() -> Self {
        RunScan { run_lo: usize::MAX }
    }

    /// The entries of the pages from `addr`; `cut(lo, hi)` for each dead run
    /// a live page ends.
    fn feed(&mut self, entries: &[u64], addr: usize, page: usize, cut: &mut impl FnMut(usize, usize)) {
        for (i, &e) in entries.iter().enumerate() {
            let a = addr + i * page;
            if e & PM_LIVE != 0 {
                if self.run_lo != usize::MAX {
                    cut(self.run_lo, a);
                    self.run_lo = usize::MAX;
                }
            } else if self.run_lo == usize::MAX {
                self.run_lo = a;
            }
        }
    }

    /// The VMA's end at `hi`: the last dead run, if any.
    fn finish(&mut self, hi: usize, cut: &mut impl FnMut(usize, usize)) {
        if self.run_lo != usize::MAX {
            cut(self.run_lo, hi);
            self.run_lo = usize::MAX;
        }
    }
}

/// The part of the dead run `[lo, hi)` to leave out of the core: the run less
/// MARGIN_BYTES at each end, when what is left is at least TRIM_MIN_BYTES.
fn cut_range(lo: usize, hi: usize, page: usize) -> Option<(usize, usize)> {
    let margin = MARGIN_BYTES.max(page);
    let a = lo.checked_add(margin)?;
    let b = hi.checked_sub(margin)?;
    (b > a && b - a >= TRIM_MIN_BYTES && a % page == 0 && b % page == 0).then_some((a, b))
}

/// One VMA's dead runs left out of the core; false on a pagemap read error
/// (the trim stops).
fn trim_vma(fd: i32, lo: usize, hi: usize, page: usize, pm: &mut [u64; PM_ENTRIES], t: &mut Trimmed) -> bool {
    let mut scan = RunScan::new();
    let mut cut = |a: usize, b: usize| {
        if let Some((x, y)) = cut_range(a, b, page) {
            // SAFETY: a page-aligned range inside a VMA of this process; the
            // advice only changes what a core dump writes.
            if unsafe { libc::madvise(x as *mut libc::c_void, y - x, libc::MADV_DONTDUMP) } == 0 {
                t.runs += 1;
                t.bytes += y - x;
            }
        }
    };
    let mut addr = lo;
    while addr < hi {
        let pages = ((hi - addr) / page).min(PM_ENTRIES);
        if pages == 0 {
            break;
        }
        let want = pages * 8;
        // SAFETY: pread(2) of `want` bytes into the static entry buffer.
        let r = unsafe { libc::pread(fd, pm.as_mut_ptr().cast(), want, ((addr / page) * 8) as libc::off_t) };
        if r != want as isize {
            return false;
        }
        scan.feed(&pm[..pages], addr, page, &mut cut);
        addr += pages * page;
    }
    scan.finish(addr, &mut cut);
    true
}

/// The buffer pool blocks the crash touched, put back into the core: for each
/// address inside the block array `[base, base + len)` or just past its end
/// (a read running off the last block, D3661), that block and its neighbours
/// (MADV_DODUMP). Returns the bytes put back.
fn keep_blocks(base: usize, len: usize, blcksz: usize, addrs: &[usize]) -> usize {
    if len == 0 {
        return 0;
    }
    let end = base + len;
    let mut kept = 0usize;
    for &a in addrs {
        if a < base || a >= end + blcksz {
            continue;
        }
        let blk = (a.min(end - 1) - base) / blcksz;
        let lo = base + blk.saturating_sub(1) * blcksz;
        let hi = (base + (blk + 2) * blcksz).min(end);
        // SAFETY: a block-aligned range inside the block array mapping; the
        // advice only changes what a core dump writes.
        if unsafe { libc::madvise(lo as *mut libc::c_void, hi - lo, libc::MADV_DODUMP) } == 0 {
            kept += hi - lo;
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: usize = 4096;

    fn runs_of(entries: &[u64], base: usize) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut scan = RunScan::new();
        let mut cut = |a: usize, b: usize| out.push((a, b));
        // Two windows, so a run spanning a window edge is one run.
        let mid = entries.len() / 2;
        scan.feed(&entries[..mid], base, P, &mut cut);
        scan.feed(&entries[mid..], base + mid * P, P, &mut cut);
        scan.finish(base + entries.len() * P, &mut cut);
        out
    }

    #[test]
    fn dead_runs_keep_present_and_swapped_pages() {
        let present = 1u64 << 63;
        let swapped = 1u64 << 62;
        let mut e = vec![0u64; 1200];
        e[0] = present;
        e[600] = swapped;
        e[611] = present;
        let base = 0x7000_0000;
        let runs = runs_of(&e, base);
        assert_eq!(runs, vec![(base + P, base + 600 * P), (base + 601 * P, base + 611 * P), (base + 612 * P, base + 1200 * P)]);
        for &(a, b) in &runs {
            for (i, &x) in e.iter().enumerate() {
                let p = base + i * P;
                if x != 0 {
                    assert!(p < a || p >= b, "a live page at {p:#x} inside the run {a:#x}..{b:#x}");
                }
            }
        }
        // The cut leaves the margins and drops a run under TRIM_MIN_BYTES.
        assert_eq!(cut_range(runs[0].0, runs[0].1, P), Some((base + P + MARGIN_BYTES, base + 600 * P - MARGIN_BYTES)));
        assert_eq!(cut_range(runs[1].0, runs[1].1, P), None);
        assert_eq!(cut_range(base, base + TRIM_MIN_BYTES + 2 * MARGIN_BYTES - P, P), None);
        assert_eq!(cut_range(base, base + TRIM_MIN_BYTES + 2 * MARGIN_BYTES, P), Some((base + MARGIN_BYTES, base + MARGIN_BYTES + TRIM_MIN_BYTES)));
    }

    #[test]
    fn maps_lines_pick_private_writable_anonymous_vmas() {
        let big = |s: &str| candidate(s.as_bytes());
        assert_eq!(big("7f0000000000-7f0001000000 rw-p 00000000 00:00 0 "), Some((0x7f00_0000_0000, 0x7f00_0100_0000)));
        assert_eq!(big("7f0000000000-7f0001000000 rw-p 00000000 00:00 0                          [anon:mimalloc]"), Some((0x7f00_0000_0000, 0x7f00_0100_0000)));
        assert_eq!(big("7f0000000000-7f0001000000 rw-p 00000000 00:00 0                          [stack]"), Some((0x7f00_0000_0000, 0x7f00_0100_0000)));
        // Too small, executable, shared, read-only, or file-backed: left alone.
        assert_eq!(big("7f0000000000-7f0000100000 rw-p 00000000 00:00 0 "), None);
        assert_eq!(big("7f0000000000-7f0001000000 rwxp 00000000 00:00 0 "), None);
        assert_eq!(big("7f0000000000-7f0001000000 r-xp 00000000 00:00 0 "), None);
        assert_eq!(big("7f0000000000-7f0001000000 rw-s 00000000 00:01 1234 /dev/zero (deleted)"), None);
        assert_eq!(big("7f0000000000-7f0001000000 r--p 00000000 00:00 0 "), None);
        assert_eq!(big("7f0000000000-7f0001000000 rw-p 00001000 103:01 5678 /usr/lib/libc.so.6"), None);
        assert_eq!(big("7f0000000000-7f0001000000 rw-p 00000000 00:00 0 /memfd:x (deleted)"), None);
        assert_eq!(big("garbage"), None);
    }

    #[test]
    fn kept_blocks_cover_the_fault_and_its_neighbours() {
        let base = 0x10_0000_0000usize;
        let b = 8192usize;
        let len = 16 * b;
        // Addresses below the base or past the block after the end keep
        // nothing (no madvise at all); an empty array keeps nothing.
        assert_eq!(keep_blocks(base, len, b, &[base - 1, base + len + b, 0]), 0);
        assert_eq!(keep_blocks(base, 0, b, &[base]), 0);
        // SAFETY: a fresh private anonymous mapping, unmapped at the end.
        let m = unsafe {
            libc::mmap(core::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0)
        };
        assert_ne!(m, libc::MAP_FAILED);
        let mb = m as usize;
        assert_eq!(keep_blocks(mb, len, b, &[mb + len]), 2 * b, "a read just past the end keeps the last two blocks");
        assert_eq!(keep_blocks(mb, len, b, &[mb + 5 * b + 17]), 3 * b, "a block in the middle keeps itself and both neighbours");
        assert_eq!(keep_blocks(mb, len, b, &[mb]), 2 * b, "the first block has no lower neighbour");
        // SAFETY: unmapping the mapping made above.
        unsafe { libc::munmap(m, len) };
    }

    /// The trim over a real mapping: the touched pages (one of them paged out:
    /// in swap where the machine has swap, else still present) stay in the
    /// core, the untouched space between them leaves it (smaps' VmFlags `dd`).
    #[test]
    fn trim_vma_leaves_out_untouched_space_only() {
        // SAFETY: sysconf has no pointer arguments.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let len = 32 << 20;
        // SAFETY: a fresh private anonymous mapping, unmapped at the end.
        let m = unsafe {
            libc::mmap(core::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0)
        };
        assert_ne!(m, libc::MAP_FAILED);
        // Small pages only, so a touch makes one page live, not a 2 MiB one.
        // SAFETY: the mapping above.
        unsafe { libc::madvise(m, len, libc::MADV_NOHUGEPAGE) };
        let lo = m as usize;
        let paged = lo + (8 << 20);
        let touched = [lo, paged, lo + (16 << 20), lo + len - page];
        for &a in &touched {
            // SAFETY: inside the mapping.
            unsafe { core::ptr::write_volatile(a as *mut u8, 1) };
        }
        // MADV_PAGEOUT (21): the page goes to swap where there is swap.
        // SAFETY: one page of the mapping above.
        unsafe { libc::madvise(paged as *mut libc::c_void, page, 21) };
        // SAFETY: open(2) of a NUL-terminated literal path.
        let fd = unsafe { libc::open(c"/proc/self/pagemap".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        assert!(fd >= 0);
        let mut e = 0u64;
        // SAFETY: an 8-byte pread into a local.
        let r = unsafe { libc::pread(fd, (&raw mut e).cast(), 8, ((paged / page) * 8) as libc::off_t) };
        assert_eq!(r, 8);
        // MADV_PAGEOUT is a hint: the page may stay present, go to swap, or
        // (no swap) stay present too. Present or swapped, it is live.
        let live = e & PM_LIVE != 0;
        eprintln!("paged-out page: present {} swapped {}", e >> 63, (e >> 62) & 1);
        let mut pm = Box::new([0u64; PM_ENTRIES]);
        let mut t = Trimmed::default();
        assert!(trim_vma(fd, lo, lo + len, page, &mut pm, &mut t));
        // SAFETY: closing the fd opened above.
        unsafe { libc::close(fd) };
        let smaps = std::fs::read_to_string("/proc/self/smaps").unwrap();
        let mut dd = Vec::new();
        let mut cur = None;
        for l in smaps.lines() {
            let head = l.split(' ').next().unwrap_or("");
            if let Some((a, b)) = head.split_once('-') {
                if let (Ok(a), Ok(b)) = (usize::from_str_radix(a, 16), usize::from_str_radix(b, 16)) {
                    cur = Some((a, b));
                    continue;
                }
            }
            if let (Some(flags), Some((a, b))) = (l.strip_prefix("VmFlags:"), cur) {
                if a >= lo && b <= lo + len && flags.split_whitespace().any(|f| f == "dd") {
                    dd.push((a, b));
                }
            }
        }
        // A page that is present or swapped is never left out.
        for &(a, b) in &dd {
            for &p in &touched {
                if p == paged && !live {
                    continue; // neither present nor swapped: it reads as zero again
                }
                assert!(p + page <= a || p >= b, "kept page {p:#x} inside the left-out range {a:#x}..{b:#x}");
            }
        }
        let out: usize = dd.iter().map(|(a, b)| b - a).sum();
        assert_eq!(out, t.bytes, "{dd:x?} {t:?}");
        // The untouched space is left out: all but the margins of each gap.
        let gaps = if live { 3 } else { 2 };
        assert_eq!(t.runs, gaps, "{dd:x?} {t:?}");
        assert!(t.bytes >= len / 2, "{t:?}");
        // SAFETY: unmapping the mapping made above.
        unsafe { libc::munmap(m, len) };
    }
}
