//! pppress card 11: the pool's present bytes leave out PTEs that map the shared zero page (PAGEMAP_SCAN, Linux 6.7): a
//! read fault of a never-written page and a THP split's all-zero subpages count 0, written pages count whole; mincore
//! counts both. While pool splits run, a sweep equals mincore's sweep less its zero pages, and (as root, which reads
//! frame numbers) /proc/self/pagemap's present pages that are not the zero page, over every pgrust:pool mapping. One
//! test binary: the pool and its mode are process-wide.
#![cfg(target_os = "linux")]

mod common;
use common::*;
use core::ptr::NonNull;
use pagepool::*;
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;

fn page() -> usize {
    pagepool::hooks::page_size()
}

/// The kernel's (major, minor).
fn kernel() -> (u32, u32) {
    let r = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let mut it = r.trim().split(|c: char| !c.is_ascii_digit()).filter_map(|x| x.parse().ok());
    (it.next().unwrap_or(0), it.next().unwrap_or(0))
}

fn thp_host() -> bool {
    std::fs::read_to_string("/sys/kernel/mm/transparent_hugepage/enabled").is_ok_and(|s| !s.contains("[never]"))
}

/// An anonymous private mapping of `len` bytes, its start aligned to `align`.
fn map(len: usize, align: usize, huge: bool) -> usize {
    // SAFETY: a fresh private anonymous mapping, ours until the process ends.
    let p = unsafe { libc::mmap(core::ptr::null_mut(), len + align, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0) };
    assert_ne!(p, libc::MAP_FAILED);
    let a = (p as usize).next_multiple_of(align);
    // SAFETY: advice over our own mapping.
    unsafe { libc::madvise(a as *mut libc::c_void, len, if huge { libc::MADV_HUGEPAGE } else { libc::MADV_NOHUGEPAGE }) };
    a
}

fn write(p: usize) {
    // SAFETY: a page of a mapping the test owns.
    unsafe { core::ptr::write_volatile(p as *mut u8, 7) };
}

fn read(p: usize) -> u8 {
    // SAFETY: as `write`.
    unsafe { core::ptr::read_volatile(p as *const u8) }
}

/// The shared zero page's frame number (0 without CAP_SYS_ADMIN, which pagemap needs to show frame numbers).
fn zero_pfn() -> u64 {
    let p = map(page(), page(), false);
    assert_eq!(read(p), 0);
    pagemap(p, 1)[0] & ((1 << 55) - 1)
}

fn pagemap(at: usize, pages: usize) -> Vec<u64> {
    let mut f = std::fs::File::open("/proc/self/pagemap").expect("pagemap");
    f.seek(SeekFrom::Start((at / page() * 8) as u64)).unwrap();
    let mut b = vec![0u8; pages * 8];
    f.read_exact(&mut b).unwrap();
    b.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect()
}

/// Root's count over every readable pgrust:pool mapping (the pool's zones and its metadata): (present pages not the
/// zero page, zero-page pages), bytes.
fn root_count(zp: u64) -> (usize, usize) {
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
    let (mut res, mut zero) = (0, 0);
    for l in maps.lines().filter(|l| l.contains("pgrust:pool")) {
        let mut f = l.split_whitespace();
        let (range, perms) = (f.next().unwrap(), f.next().unwrap());
        if !perms.starts_with('r') {
            continue;
        }
        let (a, b) = range.split_once('-').unwrap();
        let (a, b) = (usize::from_str_radix(a, 16).unwrap(), usize::from_str_radix(b, 16).unwrap());
        for e in pagemap(a, (b - a) / page()) {
            if e >> 63 != 0 {
                if e & ((1 << 55) - 1) == zp { zero += page() } else { res += page() }
            }
        }
    }
    (res, zero)
}

#[test]
fn present_leaves_out_zero_pages() {
    init_with(|c| c.thp_partial = 2);
    let pg = page();
    let scan_kernel = kernel() >= (6, 7);
    // a written page the host swaps out under pressure is not resident to any reader but mincore's swap cache
    let sw0 = swapped_out();

    // a read fault of a page never written maps the zero page; 10 written, 20 read, 34 untouched
    let p = map(64 * pg, pg, false);
    (0..10).for_each(|i| write(p + i * pg));
    (10..30).for_each(|i| assert_eq!(read(p + i * pg), 0));
    let (r, z, scanned) = residency_seam(p, 64 * pg, false).expect("a reading");
    assert_eq!(scanned, scan_kernel, "PAGEMAP_SCAN read it, unprivileged, from 6.7 on");
    if scanned {
        assert!(z == 20 * pg && whole_but_swap(r, 10 * pg, sw0), "written pages resident, zero-page reads apart: {r} {z}");
    } else {
        assert!(z == 0 && whole_but_swap(r, 30 * pg, sw0), "mincore: zero-page reads counted resident: {r}");
    }
    let (m, mz, ms) = residency_seam(p, 64 * pg, true).expect("a mincore reading");
    assert!(mz == 0 && !ms && whole_but_swap(m, 30 * pg, sw0), "mincore as before: {m}");
    // D4877: a give-back's read (bytes in RAM) is the default reader's where nothing is swapped or in migration
    let (g, gz) = in_ram_seam(p, 64 * pg).expect("a give-back reading");
    assert!(gz == z && whole_but_swap(g, if scanned { 10 } else { 30 } * pg, sw0), "the give-back read: {g} {gz}");

    // a THP split remaps the all-zero subpages to the zero page (6.12 on)
    if thp_host() {
        let h = map(SUPER, SUPER, true);
        write(h);
        let (r0, ..) = residency_seam(h, SUPER, false).unwrap();
        if r0 == SUPER {
            // SAFETY: advice over our own mapping (one page: the kernel splits the folio it covers in part).
            unsafe { libc::madvise(h as *mut libc::c_void, pg, 20) };
            let (r, z, scanned) = residency_seam(h, SUPER, false).unwrap();
            assert!(whole_but_swap(r + z, SUPER, sw0), "the split frees nothing it was not asked to: {r} + {z}");
            // a split the kernel refused (the folio busy, a host under load) remaps nothing
            if scanned && z > 0 {
                assert!(z == SUPER - pg && whole_but_swap(r, pg, sw0), "the split's zero subpages read 0: {r} {z}");
            }
            println!("split: resident {r} zero {z} (kernel {:?})", kernel());
        } else {
            println!("no huge page faulted ({r0} resident): split leg skipped");
        }
    }

    // the pool: blocks a splitter takes, writes one page of every 64 KiB of, and gives back half of (mode 2 splits the
    // superpage first), while sweeps read it
    let zp = zero_pfn();
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = stop.clone();
    let splitter = std::thread::spawn(move || {
        let hint = Hint::new(Class::Task, Tag::OTHER);
        let mut held: Vec<NonNull<u8>> = Vec::new();
        let mut n = 0u64;
        while !s2.load(Relaxed) {
            let b = take(SUPER, 64, hint).expect("a superpage block").cast::<u8>();
            let a = b.as_ptr() as usize;
            (0..SUPER / 2).step_by(64 << 10).for_each(|o| write(a + o));
            // SAFETY: the block's second half: ours, dead.
            assert_eq!(unsafe { give_back_any(a + SUPER / 2, SUPER / 2, true) }, 0);
            held.push(b);
            n += 1;
            if held.len() == 48 {
                for b in held.drain(..) {
                    // SAFETY: ours, freed once.
                    unsafe { give(b, SUPER) };
                }
                flush_shelf();
                // back to the OS, so the next takes fault fresh huge pages to split
                purge_now(usize::MAX, None);
            }
            // bursts of 4, then a gap a bracketed read fits in (root's pagemap read takes tens of ms)
            if n % 4 == 0 {
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        }
        n
    });
    let (mut ok, mut tries) = (0, 0);
    let first = thp_partial_counts().3;
    let mut seen = first;
    while ok < 3 {
        tries += 1;
        assert!(tries < 400, "no stable bracket in {tries} tries ({ok} taken)");
        std::thread::sleep(std::time::Duration::from_millis(70));
        // each read after a burst of splits since the last one
        if thp_partial_counts().3 == seen {
            continue;
        }
        let (a1, z1, s1, ns) = present_sweep_by(false).unwrap();
        let (m, mz, ms, mns) = present_sweep_by(true).unwrap();
        let root = (zp != 0).then(|| root_count(zp));
        let (a2, z2, ..) = present_sweep_by(false).unwrap();
        if (a1, z1) != (a2, z2) {
            continue;
        }
        assert!(s1 == scan_kernel && !ms && mz == 0, "readers: scan {s1} mincore {ms}");
        // mincore also counts a page in the swap cache, which the scan reads as not present
        assert!(whole_but_swap(a1 + z1, m, sw0), "mincore = resident + zero-page bytes: {m} vs {a1} + {z1}");
        if let Some((r, z)) = root {
            assert_eq!((a1, z1), (r, z), "root's pagemap: present and not the zero page, to the byte");
        }
        println!("read {ok}: present {a1} zero {z1} mincore {m} root {root:?} sweep {ns} ns (mincore {mns} ns)");
        seen = thp_partial_counts().3;
        ok += 1;
    }
    stop.store(true, Relaxed);
    let taken = splitter.join().unwrap();
    println!("{ok} reads in {tries} tries, {taken} blocks, {} split asks, root witness {}", seen - first, zp != 0);
}
