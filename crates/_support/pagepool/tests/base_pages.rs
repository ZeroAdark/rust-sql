//! C.23: `base_pages` under `PoolAlloc` (own binary), in the D4135 test's form: no syscall on the taking thread.
#![cfg(all(feature = "global", target_os = "linux"))]

mod common;
use common::{balanced, resident, swapped_out, whole_but_swap, MIB};
use pagepool::hooks::{self, Op};
use pagepool::PoolAlloc;
use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::time::{Duration, Instant};

#[global_allocator]
static A: PoolAlloc = PoolAlloc;

fn vm_flags(p: usize) -> Vec<String> {
    let maps = std::fs::read_to_string("/proc/self/smaps").unwrap();
    let mut inside = false;
    for l in maps.lines() {
        let range = l.split_once(' ').and_then(|(r, _)| r.split_once('-'));
        if let Some((Ok(lo), Ok(hi))) = range.map(|(lo, hi)| (usize::from_str_radix(lo, 16), usize::from_str_radix(hi, 16))) {
            inside = lo <= p && p < hi;
        } else if let Some(f) = l.strip_prefix("VmFlags:").filter(|_| inside) {
            return f.split_whitespace().map(str::to_string).collect();
        }
    }
    panic!("no mapping holds {p:#x}")
}

fn take(l: Layout) -> usize {
    // SAFETY: a non-zero layout.
    let p = pagepool::base_pages(l.size(), || unsafe { alloc_zeroed(l) }).expect("PoolAlloc serves this binary");
    assert!(!p.is_null());
    p as usize
}

fn all_zero(p: usize, len: usize) -> bool {
    // SAFETY: inside a block the test holds.
    unsafe { std::slice::from_raw_parts(p as *const u8, len) }.iter().all(|&b| b == 0)
}

#[test]
fn base_pages() {
    assert!(pagepool::base_pages(MIB, || ()).is_none(), "under a superpage: the caller's own road");
    let l = Layout::from_size_align(8 * MIB, 8).unwrap();
    let mut first = 0;
    for round in 0..6 {
        hooks::clear();
        let p = take(l);
        let mut calls = 0;
        hooks::each(|op, _, _, _| calls += matches!(op, Op::Mmap | Op::Madvise | Op::Mremap | Op::GiveBack) as usize);
        assert_eq!(resident(p, l.size()), 0, "round {round}: nothing resident at take");
        if round == 0 {
            first = p;
        } else {
            assert_eq!(p, first, "round {round}: the same slots, clean again");
            assert_eq!(calls, 0, "round {round}: no syscall on the taking thread");
        }
        let flags = vm_flags(p);
        assert!(flags.iter().any(|f| f == "nh") && !flags.iter().any(|f| f == "hg"), "round {round}: huge pages advised off: {flags:?}");
        // SAFETY: inside the block.
        unsafe { (p as *mut u8).add(3 * MIB).write_bytes(7, 64 << 10) };
        let r = resident(p, l.size());
        assert!((64 << 10..=128 << 10).contains(&r), "round {round}: one 64 KiB fault committed {r} bytes");
        assert!(all_zero(p, 3 * MIB) && all_zero(p + 3 * MIB + (64 << 10), 5 * MIB - (64 << 10)), "round {round}: the rest reads zero");
        let sw = swapped_out();
        // SAFETY: the block, then its free.
        unsafe {
            (p as *mut u8).write_bytes(0xa5, l.size());
            dealloc(p as *mut u8, l);
        }
        assert!(whole_but_swap(hooks::clean_base(), l.size(), sw), "round {round}: the given span went back whole");
    }
    // a cache scope's zeroed block (Rescue's road for it) is a base span's too, tagged CACHE
    let c = pagepool::base_pages(l.size(), || pagepool::cache_alloc(l.size(), 8, true)).unwrap() as usize;
    assert!(c != 0 && pagepool::is_cache(c as *const u8, l.size(), 8), "a cache block");
    assert_eq!(resident(c, l.size()), 0, "the cache block: nothing resident at take");
    assert!(vm_flags(c).iter().any(|f| f == "nh"), "the cache block: huge pages advised off");
    let sw = swapped_out();
    // SAFETY: the block, then its free.
    unsafe {
        (c as *mut u8).write_bytes(0xa5, l.size());
        dealloc(c as *mut u8, l);
    }
    assert!(whole_but_swap(hooks::clean_base(), l.size(), sw), "the cache block went back whole");
    assert_eq!(resident(c, l.size()), 0, "the cache block: nothing resident after its give-back");
    // a given span with nothing resident goes back with no remap
    let p = take(l);
    // SAFETY: the block, untouched, then its free.
    unsafe { dealloc(p as *mut u8, l) };
    hooks::clear();
    let got = hooks::clean_base();
    let (mut remaps, mut cleared) = (0, 0);
    hooks::each(|op, _, len, arg| {
        remaps += matches!(op, Op::Mmap | Op::GiveBack) as usize;
        cleared += if matches!(op, Op::Madvise) && arg == 4 { len } else { 0 };
    });
    assert!(hooks::swap_on() || (got, remaps) == (0, 0), "nothing resident: {got} bytes, {remaps} remaps");
    // D4869: nothing resident is still cleared by MADV_DONTNEED (no remap), never trusted to read zero
    assert!(!cfg!(target_os = "linux") || cleared > 0, "nothing resident: cleared by MADV_DONTNEED ({cleared} B)");
    assert_eq!(hooks::base_dirty(), 0, "the span is clean again");
    balanced();

    // the purger's own give-back, then four threads at once over it
    assert!(pagepool::start(), "the pool's threads run");
    let p = take(l);
    // SAFETY: the block, then its free.
    unsafe {
        (p as *mut u8).write_bytes(0xa5, l.size());
        dealloc(p as *mut u8, l);
    }
    let t0 = Instant::now();
    while resident(p, l.size()) > 0 && t0.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(resident(p, l.size()), 0, "the purger gave the given span back");
    let threads: Vec<_> = (0..4)
        .map(|t| {
            std::thread::spawn(move || {
                for i in 0..40 {
                    let l = Layout::from_size_align((2 + (t * 7 + i * 3) % 19) * MIB + i * 4096, 8).unwrap();
                    let p = take(l);
                    assert!(all_zero(p, l.size()), "thread {t} take {i}: zero");
                    // SAFETY: the block, then its free.
                    unsafe {
                        (p as *mut u8).write_bytes(0xa5, l.size());
                        dealloc(p as *mut u8, l);
                    }
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let t0 = Instant::now();
    while hooks::base_dirty() > 0 && t0.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(hooks::base_dirty(), 0, "the purger cleaned every given span");
    balanced();
}
