//! give_pages / take_pages: a live block's dead runs round trip, and a release under pressure gives `Zeroed::Yes`
//! (the pages read zero) in runs, pool spans (a whole superpage: the remap road) and lazy spans.

mod common;
use common::*;
use core::ptr::NonNull;
use pagepool::*;

fn fill(p: usize, len: usize, v: u8) {
    // SAFETY: inside a block the test holds.
    unsafe { core::ptr::write_bytes(p as *mut u8, v, len) };
}

fn all(p: usize, len: usize, v: u8) -> bool {
    // SAFETY: as above.
    unsafe { core::slice::from_raw_parts(p as *const u8, len) }.iter().all(|&b| b == v)
}

#[test]
fn dead_pages() {
    setup();
    // card 2 (ppround): resident reads are exact, or a loud SKIP if the host swapped
    let sw = swap_counts();
    let h = Hint::new(Class::Session, Tag::OP);
    let run = 64 << 10;
    // a runs-road block
    let p = take(16 * run, run, h).unwrap().cast::<u8>();
    let a = p.as_ptr() as usize;
    fill(a, 16 * run, 0xab);
    let s0 = stats();
    // SAFETY: inside our block.
    unsafe { give_pages(p, 4 * run + 100, 8 * run) };
    assert_eq!(stats().dead - s0.dead, 7 * run, "whole runs only");
    // SAFETY: as above.
    assert_eq!(unsafe { take_pages(p, 4 * run + 100, 8 * run) }, Zeroed::No);
    assert!(all(a + 5 * run, 7 * run, 0xab), "a dead range not released keeps its bytes");
    // SAFETY: as above.
    unsafe { give_pages(p, 5 * run, 7 * run) };
    // a pool span with a whole dead superpage, and a lazy one
    let mut s = span(8 * MIB, h).unwrap();
    let sa = s.as_ptr() as usize;
    fill(sa, 8 * MIB, 0xcd);
    // SAFETY: inside our span.
    unsafe { give_pages(NonNull::new(s.as_ptr()).unwrap(), 2 * MIB, 4 * MIB) };
    let mut l = span(64 * MIB, h).unwrap();
    // SAFETY: ours.
    unsafe { commit_to(&mut l, 64 * MIB) };
    let la = l.as_ptr() as usize;
    fill(la, 64 * MIB, 0xef);
    // SAFETY: inside our span.
    unsafe { give_pages(NonNull::new(l.as_ptr()).unwrap(), 8 * MIB, 16 * MIB) };
    swap_exact!(sw, resident(sa + 2 * MIB, 4 * MIB) > 0, "the pool span's dead superpage resident before the purge");
    set_pressure(Pressure::Floor);
    purge_now(usize::MAX, None);
    assert_eq!(stats().dead, 0, "every dead run released");
    assert_eq!(resident(a + 5 * run, 7 * run), 0);
    assert_eq!(resident(sa + 2 * MIB, 4 * MIB), 0);
    assert_eq!(resident(la + 8 * MIB, 16 * MIB), 0);
    // SAFETY: the ranges given above.
    unsafe {
        assert_eq!(take_pages(p, 5 * run, 7 * run), Zeroed::Yes);
        assert_eq!(take_pages(NonNull::new(s.as_ptr()).unwrap(), 2 * MIB, 4 * MIB), Zeroed::Yes);
        assert_eq!(take_pages(NonNull::new(l.as_ptr()).unwrap(), 8 * MIB, 16 * MIB), Zeroed::Yes);
    }
    assert!(all(a + 5 * run, 7 * run, 0) && all(a, 5 * run, 0xab));
    assert!(all(sa + 2 * MIB, 4 * MIB, 0) && all(sa, 2 * MIB, 0xcd));
    assert!(all(la + 8 * MIB, 16 * MIB, 0) && all(la, 8 * MIB, 0xef));
    // a give with dead runs still in it revives them first
    // SAFETY: ours.
    unsafe {
        give_pages(p, 0, 2 * run);
        give(p, 16 * run);
        shrink_span(&mut s, 2 * MIB);
        give_span(s);
        give_span(l);
    }
    assert_eq!(stats().dead, 0);
    flush_shelf();
    balanced();
}
