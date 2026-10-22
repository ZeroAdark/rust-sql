//! CVE-2026-74674: nothing the pool does is a MADV_DONTNEED over a whole 2 MiB-aligned range. Every release goes through
//! `pagegive::give_back` (whole PMDs remapped, smaller pieces split by pagegive's own rule); the pool zone stays mapped.

mod common;
use common::*;
use core::ptr::NonNull;
use pagepool::hooks::{self, Op};
use pagepool::*;

const ADVICE_DONTNEED: i32 = 4;

fn holds_whole_pmd(lo: usize, len: usize) -> bool {
    let p = lo.next_multiple_of(SUPER);
    p.checked_add(SUPER).is_some_and(|e| e <= lo + len)
}

#[test]
fn no_whole_pmd_dontneed() {
    setup();
    hooks::clear();
    let h = Hint::new(Class::Cache, Tag::PART);
    let blocks: Vec<_> = (0..64).map(|_| take(64 << 10, 16, h).unwrap()).collect();
    for p in &blocks {
        touch(p.cast::<u8>().as_ptr() as usize, 64 << 10);
    }
    for p in blocks {
        // SAFETY: ours.
        unsafe { give(p.cast(), 64 << 10) };
    }
    flush_shelf();
    let d = take(24 * 64 << 10, 64 << 10, h).unwrap().cast::<u8>();
    let s = span(16 * MIB, h).unwrap();
    touch(s.as_ptr() as usize, s.len());
    let sp = s.as_ptr() as usize;
    // SAFETY: ours, not used again.
    unsafe { give_span(s) };
    touch(d.as_ptr() as usize, 24 * 64 << 10);
    // SAFETY: inside our own block.
    unsafe { give_pages(d, 64 << 10, 5 * 64 << 10) };
    let mut l = span(64 * MIB, h).unwrap();
    // SAFETY: ours.
    unsafe { commit_to(&mut l, 64 * MIB) };
    touch(l.as_ptr() as usize, l.len());
    let lp = l.as_ptr() as usize;
    // SAFETY: ours, not used again.
    unsafe { give_span(l) };
    let got = purge_now(usize::MAX, None);
    assert!(got >= 16 * MIB + 64 * MIB, "released {got}");
    let (mut gives, mut whole, mut bad) = (0, 0, Vec::new());
    let complete = hooks::each(|op, at, len, arg| match op {
        Op::Madvise if arg == ADVICE_DONTNEED && holds_whole_pmd(at, len) => bad.push((at, len)),
        Op::GiveBack => {
            gives += 1;
            whole += holds_whole_pmd(at, len) as usize;
        }
        _ => {}
    });
    assert!(complete, "the syscall log overflowed");
    assert!(bad.is_empty(), "MADV_DONTNEED over a whole 2 MiB range: {bad:x?}");
    assert!(gives >= 3 && whole >= 2, "{gives} releases, {whole} holding a whole PMD");
    assert_eq!(resident(sp, 16 * MIB), 0, "the released span's pages went back");
    assert_eq!(resident(lp, 64 * MIB), 0, "the warm lazy span's pages went back");
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
    let mapped = maps.lines().any(|line| {
        let mut it = line.split(|c| c == '-' || c == ' ');
        let lo = usize::from_str_radix(it.next().unwrap(), 16).unwrap();
        let hi = usize::from_str_radix(it.next().unwrap(), 16).unwrap();
        lo <= sp && sp + 16 * MIB <= hi && line.contains(" rw")
    });
    assert!(mapped, "the pool zone stays mapped read-write over a released range");
    // SAFETY: as above.
    assert_eq!(unsafe { take_pages(d, 64 << 10, 5 * 64 << 10) }, Zeroed::Yes);
    // SAFETY: ours, not used again.
    unsafe { give(NonNull::new(d.as_ptr()).unwrap(), 24 * 64 << 10) };
    flush_shelf();
    balanced();
}
