//! Every syscall the pool makes (unix). wasm uses fallback.rs.

use core::ffi::{c_int, c_void, CStr};
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering::Relaxed};

#[cfg(target_os = "linux")]
const MADV_POPULATE_WRITE: c_int = 23;
#[cfg(target_os = "linux")]
const MREMAP_DONTUNMAP: c_int = 4;
#[cfg(target_os = "linux")]
const NORESERVE: c_int = libc::MAP_NORESERVE;
#[cfg(not(target_os = "linux"))]
const NORESERVE: c_int = 0;

pub(crate) fn phys_bytes() -> usize {
    // SAFETY: plain sysconf reads.
    unsafe { (libc::sysconf(libc::_SC_PHYS_PAGES).max(0) as usize).saturating_mul(libc::sysconf(libc::_SC_PAGESIZE).max(0) as usize) }
}

pub(crate) fn env_is_one(name: &CStr) -> bool {
    env_str(name) == Some("1")
}

/// A switch the pagepool branch turns ON by default (the user, 2026-10-17 ~22:4xZ: "lock it in"): on unless the
/// environment says `0`.
pub(crate) fn env_on_by_default(name: &CStr) -> bool {
    env_str(name) != Some("0")
}

pub(crate) fn env_str(name: &CStr) -> Option<&'static str> {
    // SAFETY: getenv with a NUL-terminated name; the value is read before anything can change it.
    unsafe {
        let v = libc::getenv(name.as_ptr());
        if v.is_null() { None } else { CStr::from_ptr(v).to_str().ok() }
    }
}

static PAGE: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn page_size() -> usize {
    let p = PAGE.load(Relaxed);
    if p != 0 {
        return p;
    }
    // SAFETY: a plain sysconf read.
    let p = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).ok().filter(|p| p.is_power_of_two()).unwrap_or(4096);
    PAGE.store(p, Relaxed);
    p
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Thp {
    Always,
    Madvise,
    Never,
}

static THP: AtomicU8 = AtomicU8::new(0);

pub(crate) fn thp() -> Thp {
    let mut v = THP.load(Relaxed);
    if v == 0 {
        let mut buf = [0u8; 64];
        let n = read_small(c"/sys/kernel/mm/transparent_hugepage/enabled", &mut buf);
        let s = &buf[..n];
        v = if contains(s, b"[always]") {
            1
        } else if contains(s, b"[madvise]") {
            2
        } else {
            3
        };
        THP.store(v, Relaxed);
    }
    match v {
        1 => Thp::Always,
        2 => Thp::Madvise,
        _ => Thp::Never,
    }
}

static NODES: AtomicU8 = AtomicU8::new(0);

/// NUMA nodes online ("0", "0-1", "0,2-3"). P1 serves every thread from node 0's lists.
pub(crate) fn nodes() -> u8 {
    let v = NODES.load(Relaxed);
    if v != 0 {
        return v;
    }
    let mut buf = [0u8; 64];
    let n = read_small(c"/sys/devices/system/node/online", &mut buf);
    let mut count = 0u32;
    for part in buf[..n].split(|&b| b == b',') {
        let part = part.trim_ascii();
        let num = |s: &[u8]| s.iter().try_fold(0u32, |a, &d| d.is_ascii_digit().then(|| a * 10 + (d - b'0') as u32));
        let mut it = part.splitn(2, |&b| b == b'-');
        match (it.next().and_then(num), it.next().map(num)) {
            (Some(_), None) => count += 1,
            (Some(a), Some(Some(b))) if b >= a => count += b - a + 1,
            _ => {}
        }
    }
    let v = count.clamp(1, 64) as u8;
    NODES.store(v, Relaxed);
    v
}

#[cfg(target_os = "linux")]
static SWAP: AtomicU8 = AtomicU8::new(0);

/// pagepool P3 (F1): whether /proc/swaps lists a swap device (read once). With one, a page out of RAM may hold data on
/// the device, so a range with no page resident is not known to read zero. Unreadable reads as on.
#[cfg(target_os = "linux")]
pub(crate) fn swap_on() -> bool {
    let v = SWAP.load(Relaxed);
    if v != 0 {
        return v == 2;
    }
    let mut buf = [0u8; 256];
    let n = read_small(c"/proc/swaps", &mut buf);
    let s = &buf[..n];
    // the header line alone: no device; anything past it is one
    let on = n == 0 || s.iter().position(|&b| b == b'\n').is_none_or(|nl| s.len() > nl + 1);
    SWAP.store(if on { 2 } else { 1 }, Relaxed);
    on
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// Open a host fact file read-only (negative: refused).
pub(crate) fn open_ro(path: &CStr) -> c_int {
    // SAFETY: open on a NUL-terminated path.
    unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) }
}

fn read_each(path: &CStr, buf: &mut [u8], mut f: impl FnMut(&[u8])) {
    let fd = open_ro(path);
    // SAFETY: read / close on our own descriptor into our own buffer.
    unsafe {
        if fd < 0 {
            return;
        }
        loop {
            let n = libc::read(fd, buf.as_mut_ptr().cast(), buf.len());
            if n <= 0 {
                break;
            }
            f(&buf[..n as usize]);
        }
        libc::close(fd);
    }
}

fn read_small(path: &CStr, buf: &mut [u8]) -> usize {
    let mut tmp = [0u8; 64];
    let mut n = 0;
    read_each(path, &mut tmp, |c| {
        let k = c.len().min(buf.len() - n);
        buf[n..n + k].copy_from_slice(&c[..k]);
        n += k;
    });
    n
}

/// How many more mappings the process may make before vm.max_map_count (None: unknown, e.g. macOS).
pub(crate) fn map_headroom() -> Option<usize> {
    let mut buf = [0u8; 32];
    let n = read_small(c"/proc/sys/vm/max_map_count", &mut buf);
    let max = buf[..n].trim_ascii().iter().try_fold(0usize, |a, &d| d.is_ascii_digit().then(|| a * 10 + (d - b'0') as usize))?;
    if n == 0 {
        return None;
    }
    let mut lines = 0;
    let mut page = [0u8; 4096];
    read_each(c"/proc/self/maps", &mut page, |c| lines += c.iter().filter(|&&b| b == b'\n').count());
    Some(max.saturating_sub(lines))
}

/// Reserve `len` bytes of address space aligned to `align`: PROT_NONE, NORESERVE. None when the kernel refuses.
/// # Safety
/// None beyond mmap's; the range is the caller's for the process's life.
pub(crate) unsafe fn reserve(len: usize, align: usize) -> Option<usize> {
    let total = len.checked_add(align)?;
    // SAFETY: a fresh private anonymous mapping.
    let raw = unsafe { libc::mmap(core::ptr::null_mut(), total, libc::PROT_NONE, libc::MAP_PRIVATE | libc::MAP_ANON | NORESERVE, -1, 0) };
    log::note(log::Op::Mmap, raw as usize, total, libc::PROT_NONE);
    if raw == libc::MAP_FAILED {
        return None;
    }
    let raw = raw as usize;
    let at = raw.next_multiple_of(align);
    // SAFETY: the head and tail of our own fresh mapping, outside [at, at + len).
    unsafe {
        if at > raw {
            libc::munmap(raw as *mut c_void, at - raw);
        }
        let end = at + len;
        if raw + total > end {
            libc::munmap(end as *mut c_void, raw + total - end);
        }
    }
    Some(at)
}

/// Open reserved address space read-write (NORESERVE), advised huge or not, and named.
/// # Safety
/// `[at, at + len)` is the pool's own reserved range, page-aligned, holding nothing live.
pub(crate) unsafe fn open(at: usize, len: usize, huge: bool, name: &CStr) -> bool {
    // SAFETY: the caller's contract: MAP_FIXED replaces only our own range.
    let p = unsafe {
        libc::mmap(at as *mut c_void, len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_FIXED | NORESERVE, -1, 0)
    };
    log::note(log::Op::Mmap, at, len, libc::PROT_READ | libc::PROT_WRITE);
    if p as usize != at {
        return false;
    }
    advise_huge(at, len, huge && thp() != Thp::Never);
    set_name(at, len, name);
    true
}

#[cfg(target_os = "linux")]
fn advise_huge(at: usize, len: usize, huge: bool) {
    let advice = if huge { libc::MADV_HUGEPAGE } else { libc::MADV_NOHUGEPAGE };
    // SAFETY: advice only, over our own mapping; a refusal leaves base pages.
    unsafe { libc::madvise(at as *mut c_void, len, advice) };
    log::note(log::Op::Madvise, at, len, advice);
}

#[cfg(not(target_os = "linux"))]
fn advise_huge(_at: usize, _len: usize, _huge: bool) {}

#[cfg(target_os = "linux")]
fn set_name(at: usize, len: usize, name: &CStr) {
    const PR_SET_VMA: c_int = 0x5356_4d41;
    const PR_SET_VMA_ANON_NAME: libc::c_ulong = 0;
    // SAFETY: names our own mapping; the kernel copies the name; a kernel without names refuses harmlessly.
    unsafe { libc::prctl(PR_SET_VMA, PR_SET_VMA_ANON_NAME, at as libc::c_ulong, len as libc::c_ulong, name.as_ptr()) };
}

#[cfg(not(target_os = "linux"))]
fn set_name(_at: usize, _len: usize, _name: &CStr) {}

/// Map `[at, at + len)` PROT_NONE afresh (its pages go back to the OS).
/// # Safety
/// As `open`.
pub(crate) unsafe fn guard(at: usize, len: usize) -> bool {
    // SAFETY: the caller's contract.
    let p = unsafe { libc::mmap(at as *mut c_void, len, libc::PROT_NONE, libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_FIXED | NORESERVE, -1, 0) };
    log::note(log::Op::Mmap, at, len, libc::PROT_NONE);
    p as usize == at
}

/// Fault `[at, at + len)` in now: MADV_POPULATE_WRITE, or one plain store per page on a kernel without it.
/// # Safety
/// The range is open read-write and no other thread writes it during the call.
pub(crate) unsafe fn populate(at: usize, len: usize) -> bool {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: advice over our own open mapping.
        let rc = unsafe { libc::madvise(at as *mut c_void, len, MADV_POPULATE_WRITE) };
        log::note(log::Op::Madvise, at, len, MADV_POPULATE_WRITE);
        if rc == 0 {
            return true;
        }
        if errno() != libc::EINVAL {
            return false;
        }
    }
    let page = page_size();
    let mut p = at;
    while p < at + len {
        // SAFETY: inside the caller's open range, which only its owner writes; the byte is stored back unchanged with a
        // plain store (an atomic first touch faults twice on arm64).
        unsafe { core::ptr::write_volatile(p as *mut u8, core::ptr::read_volatile(p as *const u8)) };
        p += page;
    }
    true
}

#[cfg(target_os = "linux")]
fn errno() -> c_int {
    // SAFETY: the calling thread's errno.
    unsafe { *libc::__errno_location() }
}

/// Give `[at, at + len)`'s pages back: a whole 2 MiB-aligned range is remapped, never MADV_DONTNEED'd. 0 = done.
/// # Safety
/// The range is the pool's own open range, page-aligned, its bytes dead, and nothing else touches it during the call.
pub(crate) unsafe fn release(at: usize, len: usize) -> c_int {
    // SAFETY: the caller's contract.
    unsafe { release_with(at, len, true) }
}

/// `release`, the D4760 split only with `split` (pppress card 10: false where no page of the range is PMD-mapped, so
/// there is no huge folio to split).
/// # Safety
/// As `release`.
pub(crate) unsafe fn release_with(at: usize, len: usize, split: bool) -> c_int {
    log::note(log::Op::GiveBack, at, len, 0);
    #[cfg(target_os = "linux")]
    {
        // D4760: a part of a superpage in use, its folio split first (`pool_thp_partial` 2, and 3: its callers release
        // only what is cold or pressed)
        if split && crate::thp_partial() >= crate::THP_SPLIT {
            split_partial(at, len);
        }
        // SAFETY: the caller's contract is give_back's.
        unsafe { pagegive::give_back(at, len, pagegive::Mapping::Pool { huge: true }) }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = split;
        // SAFETY: the caller's contract: MAP_FIXED replaces only our own dead range.
        let p = unsafe {
            libc::mmap(at as *mut c_void, len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_FIXED, -1, 0)
        };
        log::note(log::Op::Mmap, at, len, libc::PROT_READ | libc::PROT_WRITE);
        if p as usize == at { 0 } else { -1 }
    }
}

/// D4760 (`pool_thp_partial` 2): `[at, at + len)` is about to be given back; where it covers a superpage only in part,
/// a MADV_COLD over the part's first page splits that superpage's THP folio (the kernel splits a large folio a cold or
/// pageout range covers in part; 7.0, measured: 32 of 32), so the give-back after frees and uncharges its pages instead
/// of leaving a 2 MiB folio charged until a deferred split. The page is about to go anyway: deactivating it costs
/// nothing. No call for whole superpages, and none where the call is refused (a kernel before 5.4: the give-back as
/// before).
#[cfg(target_os = "linux")]
pub(crate) fn split_partial(at: usize, len: usize) {
    const MADV_COLD: c_int = 20;
    const SUPER: usize = crate::superpage::SUPER;
    let end = at + len;
    if len == 0 || (at % SUPER == 0 && end % SUPER == 0) {
        return;
    }
    let t0 = pg_clock::mono_ns();
    let page = page_size();
    let mut n = 0u64;
    let head = at % SUPER != 0;
    if head {
        // SAFETY: advice only, over one page of the caller's own range, whose bytes are dead (`release`'s contract).
        unsafe { libc::madvise(at as *mut c_void, page, MADV_COLD) };
        log::note(log::Op::Madvise, at, page, MADV_COLD);
        n += 1;
    }
    // the last superpage the range covers in part, unless the head's call split it already
    let tail = (end / SUPER * SUPER).max(at);
    if end % SUPER != 0 && !(head && tail / SUPER == at / SUPER) {
        // SAFETY: as above: `tail` lies inside the range (at <= tail < end).
        unsafe { libc::madvise(tail as *mut c_void, page, MADV_COLD) };
        log::note(log::Op::Madvise, tail, page, MADV_COLD);
        n += 1;
    }
    crate::stats::THP_SPLITS.fetch_add(n, Relaxed);
    crate::stats::THP_SPLIT_NS.fetch_add(pg_clock::mono_ns().saturating_sub(t0), Relaxed);
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn split_partial(_at: usize, _len: usize) {}

/// pagepool P3 (F1 / F2): /proc/self/pagemap, opened once by the age passes on a swap host (-2: not yet; -1: refused).
#[cfg(target_os = "linux")]
static PAGEMAP: core::sync::atomic::AtomicI32 = core::sync::atomic::AtomicI32::new(-2);

/// pagepool P3 (F1 / F2): the resident bytes of `[at, at + len)` (page-aligned) and whether any of its pages is on a swap
/// device, from /proc/self/pagemap (bit 63 present, bit 62 swapped; readable without privilege, which only zeroes the
/// frame numbers): one page-table walk, as mincore's, which reads a swapped page as not resident like one never faulted.
/// `None` when the file is unreadable. The age passes' read with swap on; the purger's (a test's seam) only.
#[cfg(target_os = "linux")]
pub(crate) fn mapped_of(at: usize, len: usize) -> Option<(usize, bool)> {
    let mut fd = PAGEMAP.load(Relaxed);
    if fd == -2 {
        let f = open_ro(c"/proc/self/pagemap").max(-1);
        // a racing opener's descriptor stays open (one, once)
        fd = PAGEMAP.compare_exchange(-2, f, Relaxed, Relaxed).unwrap_or_else(|cur| cur);
    }
    if fd < 0 {
        return None;
    }
    let page = page_size();
    let mut buf = [0u64; 512];
    let (mut a, end) = (at, at + len);
    let (mut resident, mut swapped) = (0usize, false);
    while a < end {
        let n = ((end - a) / page).clamp(1, buf.len());
        let want = n * core::mem::size_of::<u64>();
        // SAFETY: pread of `want` bytes into our own buffer of `n` entries.
        let got = unsafe { libc::pread(fd, buf.as_mut_ptr().cast(), want, ((a / page) * 8) as libc::off_t) };
        if got != want as isize {
            return None;
        }
        for &e in &buf[..n] {
            resident += ((e >> 63) & 1) as usize * page;
            swapped |= (e >> 62) & 1 != 0;
        }
        a += n * page;
    }
    Some((resident, swapped))
}

/// The age passes' read of a range: its (resident, zero-page) bytes (`None`: unknown) and whether a page of it may be on
/// a swap device. With swap on, pagemap's (it tells a swapped page from one never faulted; a zero-page PTE reads
/// resident); else `present::residency_of`'s.
fn residency(at: usize, len: usize) -> (Option<(usize, usize)>, bool) {
    #[cfg(target_os = "linux")]
    {
        if swap_on() {
            return match mapped_of(at, len) {
                Some((r, s)) => (Some((r, 0)), s),
                None => (None, true),
            };
        }
    }
    (crate::present::residency_of(at, len), false)
}

/// pagepool P3 (F1 / F2): the age and cap passes' give-back of a dead range `[at, at + len)`, by what is resident in it.
/// With pages resident (or residency unknown) the range is released as `release` does, a remap of a whole 2 MiB piece
/// (mmap_lock write). With none, nothing is freed, but the range is still cleared by MADV_DONTNEED (`clear_nonresident`:
/// mmap_lock read, no page freed, no TLB flush, no mapping split): D4869, non-residency alone does not prove it reads zero
/// (a swap device enabled after `swap_on`'s one read can hold its pages), and its runs' zero bits follow this call. Returns the resident bytes released (the
/// pass budget's cost; the whole range when unknown), or None when the call failed (the range is as it was). Card 11: a
/// range whose only present PTEs map the zero page is still released (the remap frees their page table, so the range
/// can fault huge again) at a cost of 0.
/// # Safety
/// As `release`.
pub(crate) unsafe fn age_release(at: usize, len: usize) -> Option<usize> {
    // SAFETY: the caller's contract.
    unsafe { age_release_with(at, len, true) }
}

/// `age_release`, the D4760 split only with `split` (`release_with`).
/// # Safety
/// As `release`.
pub(crate) unsafe fn age_release_with(at: usize, len: usize, split: bool) -> Option<usize> {
    let (r, swapped) = residency(at, len);
    // with swap on, `swapped` told a never-faulted range from one with a page on the device; D4869 clears both alike
    let _ = swapped;
    if r == Some((0, 0)) {
        #[cfg(target_os = "linux")]
        {
            // SAFETY: the caller's contract: page-aligned, our own, its bytes dead.
            if unsafe { clear_nonresident(at, len) } != 0 {
                return None;
            }
        }
        return Some(0);
    }
    // SAFETY: the caller's contract.
    if unsafe { release_with(at, len, split) } != 0 {
        return None;
    }
    Some(r.map_or(len, |(resident, _)| resident))
}

/// D4869: a dead range with no page resident cleared by MADV_DONTNEED (mmap_lock read, no page freed, no TLB flush, no
/// mapping split; whole 2 MiB pieces split as `pagegive::dontneed_split` does) rather than trusted to read zero: `swap_on`
/// reads /proc/swaps once, so a swap device enabled after it can hold such a range's pages, and an age pass or `clean_base`
/// that skipped the call would then mark them zero over the old bytes. The caller sets zero state only when this returns 0.
/// # Safety
/// Page-aligned, the pool's own, its bytes dead.
#[cfg(target_os = "linux")]
pub(crate) unsafe fn clear_nonresident(at: usize, len: usize) -> c_int {
    // madvise's DONTNEED advice (4) as the syscall log records it, the value tests/cve.rs reads as ADVICE_DONTNEED; the
    // calls themselves are pagegive's (dontneed_split, the CVE-2026-74674 lint's allowed site), noted one per call by its own
    // rule (a whole 2 MiB piece in two), so the log holds the syscalls made, not the one range asked
    const ADVICE_DONTNEED: i32 = 4;
    const PMD: usize = 2 << 20;
    let (end, mut cur) = (at + len, at);
    while cur < end {
        let p = cur.next_multiple_of(PMD);
        let stop = if p <= end && end - p >= PMD { p + PMD / 2 } else { end };
        log::note(log::Op::Madvise, cur, stop - cur, ADVICE_DONTNEED);
        cur = stop;
    }
    // SAFETY: the caller's contract is dontneed_split's: page-aligned, our own, its bytes dead.
    unsafe { pagegive::dontneed_split(at, len) }
}

/// C.23: `release` for the base-page sub-zone, huge pages advised off again after (the fresh mapping lost the advice).
pub(crate) unsafe fn release_base(at: usize, len: usize) -> c_int {
    #[cfg(target_os = "linux")]
    {
        log::note(log::Op::GiveBack, at, len, 0);
        // SAFETY: the caller's contract is give_back's.
        let rc = unsafe { pagegive::give_back(at, len, pagegive::Mapping::Pool { huge: false }) };
        if rc == 0 {
            advise_huge(at, len, false);
        }
        rc
    }
    #[cfg(not(target_os = "linux"))]
    // SAFETY: the caller's contract.
    unsafe {
        release(at, len)
    }
}

/// Move `[from, from + len)`'s pages to `to` (mremap). With `keep_from` the source stays mapped, empty.
/// # Safety
/// Both ranges are the pool's own, page-aligned; the source is one mapping and nothing touches either during the call.
pub(crate) unsafe fn move_pages(from: usize, to: usize, len: usize, keep_from: bool) -> bool {
    #[cfg(target_os = "linux")]
    {
        let flags = libc::MREMAP_MAYMOVE | libc::MREMAP_FIXED | if keep_from { MREMAP_DONTUNMAP } else { 0 };
        // SAFETY: the caller's contract; MREMAP_FIXED replaces only our own target range.
        let p = unsafe { libc::mremap(from as *mut c_void, len, len, flags, to as *mut c_void) };
        log::note(log::Op::Mremap, from, len, flags);
        p as usize == to
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (from, to, len, keep_from);
        false
    }
}

/// mprotect `[at, at + len)` read-write or PROT_NONE.
/// # Safety
/// The range is the pool's own, page-aligned; nothing live is read there while it is PROT_NONE.
pub(crate) unsafe fn set_prot(at: usize, len: usize, rw: bool) -> bool {
    let prot = if rw { libc::PROT_READ | libc::PROT_WRITE } else { libc::PROT_NONE };
    log::note(log::Op::Mprotect, at, len, prot);
    // SAFETY: the caller's contract.
    unsafe { libc::mprotect(at as *mut c_void, len, prot) == 0 }
}

/// The syscall log: a fixed ring of the last calls (no heap; cold paths only), read by the tests.
pub mod log {
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    #[repr(u8)]
    pub enum Op {
        Mmap = 1,
        Madvise = 2,
        Mremap = 3,
        Mprotect = 4,
        GiveBack = 5,
    }

    mod ring {
        use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering::*};

        pub(super) const N: usize = 4096;
        pub(super) struct Entry {
            pub(super) head: AtomicU64,
            pub(super) at: AtomicUsize,
            pub(super) len: AtomicUsize,
        }
        pub(super) static RING: [Entry; N] = [const { Entry { head: AtomicU64::new(0), at: AtomicUsize::new(0), len: AtomicUsize::new(0) } }; N];
        pub(super) static NEXT: AtomicUsize = AtomicUsize::new(0);
        pub(super) static FROM: AtomicUsize = AtomicUsize::new(0);

        pub(super) fn note(op: u8, at: usize, len: usize, arg: i32) {
            let i = NEXT.fetch_add(1, Relaxed);
            let e = &RING[i % N];
            e.head.store(0, Release);
            e.at.store(at, Relaxed);
            e.len.store(len, Relaxed);
            e.head.store((((i as u64 + 1) & 0xff_ffff) << 40) | ((op as u64) << 32) | arg as u32 as u64, Release);
        }
    }

    #[inline]
    pub(crate) fn note(op: Op, at: usize, len: usize, arg: i32) {
        ring::note(op as u8, at, len, arg);
    }

    pub fn clear() {
        use core::sync::atomic::Ordering::Relaxed;
        ring::FROM.store(ring::NEXT.load(Relaxed), Relaxed);
    }

    /// Each call since the last `clear` still in the ring: (op, at, len, flags / advice / prot). False: some were lost.
    pub fn each(mut f: impl FnMut(Op, usize, usize, i32)) -> bool {
        use core::sync::atomic::Ordering::*;
        let (from, to) = (ring::FROM.load(Relaxed), ring::NEXT.load(Acquire));
        let lost = to - from > ring::N;
        for i in from.max(to.saturating_sub(ring::N))..to {
            let e = &ring::RING[i % ring::N];
            let h = e.head.load(Acquire);
            if h >> 40 != (i as u64 + 1) & 0xff_ffff {
                continue;
            }
            let op = match (h >> 32) as u8 & 0xff {
                1 => Op::Mmap,
                2 => Op::Madvise,
                3 => Op::Mremap,
                4 => Op::Mprotect,
                _ => Op::GiveBack,
            };
            f(op, e.at.load(Relaxed), e.len.load(Relaxed), h as u32 as i32);
        }
        !lost
    }
}
