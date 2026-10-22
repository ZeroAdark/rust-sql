//! The frame zone: Move and Fault commits; release(keep) moves whole superpages back warm and a pool take then faults 0;
//! the VMA count over commit / release cycles is recorded (1,000 by default; PAGEPOOL_BIG_TESTS=1: 10,000).

mod common;
use common::*;
use pagepool::frames::{self, Mode};
use pagepool::*;

fn vmas() -> usize {
    std::fs::read_to_string("/proc/self/maps").unwrap().lines().count()
}

#[test]
#[cfg_attr(pagepool_tsan, ignore = "TSan: its shadow memory faults too, so the minor-fault counts run high")]
fn frames() {
    setup();
    let z = frames::zone().expect("frame zone");
    assert!(z.len() >= 64 * MIB);
    let at = z.lo();
    assert_eq!(prefill(8 * SUPER, 1), 8 * SUPER);
    let s0 = stats();
    // SAFETY: frames' own PROT_NONE range.
    let c = unsafe { frames::commit(at, 8 * MIB, true, Mode::Move, Tag::FRAME) }.expect("commit");
    if c.moved == 0 {
        eprintln!("SKIP Move: the kernel refused MREMAP_DONTUNMAP; faulted {}", c.faulted);
    } else {
        assert_eq!((c.moved, c.faulted), (8 * MIB, 0));
        assert_eq!(stats().claimed, s0.claimed, "moved RAM was already claimed");
        assert_eq!(stats().moved_vmas, 4);
    }
    touch(at, 8 * MIB);
    assert_eq!(resident(at, 8 * MIB), 8 * MIB);
    assert_eq!(stats().in_use[Tag::FRAME.0 as usize], 8 * MIB);
    // SAFETY: committed above.
    let kept = unsafe { frames::release(at, 8 * MIB, true) };
    if c.moved > 0 {
        assert_eq!(kept, 8 * MIB);
        assert_eq!(stats().moved_vmas, 0);
        let f0 = minflt();
        let s = span(2 * MIB, Hint::new(Class::Task, Tag::OTHER)).unwrap();
        touch(s.as_ptr() as usize, s.len());
        assert_eq!(minflt() - f0, 0, "a superpage frames gave back is warm");
        // SAFETY: ours, not used again.
        unsafe { give_span(s) };
    }
    let f = at + 64 * MIB;
    // SAFETY: frames' own PROT_NONE range.
    let c = unsafe { frames::commit(f, 256 << 10, false, Mode::Fault, Tag::FRAME) }.unwrap();
    assert_eq!((c.moved, c.faulted), (0, 256 << 10));
    assert_eq!(resident(f, 256 << 10), 256 << 10);
    // SAFETY: committed above.
    assert_eq!(unsafe { frames::release(f, 256 << 10, false) }, 0);
    assert_eq!(resident(f, 256 << 10), 0);
    let cycles = if big() { 10_000 } else { 1_000 };
    let v0 = vmas();
    for k in 0..cycles {
        let a = at + (k % 4) * 2 * MIB;
        // SAFETY: frames' own range, released each cycle.
        unsafe {
            frames::commit(a, 2 * MIB, true, Mode::Move, Tag::FRAME).unwrap();
            frames::release(a, 2 * MIB, true);
        }
    }
    let v1 = vmas();
    eprintln!("MEASURED frame VMAs: {v0} before, {v1} after {cycles} Move commit / keep release cycles");
    assert!(v1 <= v0 + 4, "a piece goes back to its own slot: the VMA count stays put ({v0} -> {v1})");
    hooks::frame_budget(2);
    // SAFETY: frames' own PROT_NONE range.
    let c = unsafe { frames::commit(at, 8 * MIB, true, Mode::default(), Tag::FRAME) }.unwrap();
    assert!(c.moved <= 2 * SUPER && c.moved + c.faulted == 8 * MIB, "past the mapping budget Move faults: {c:?}");
    // SAFETY: committed above.
    unsafe { frames::release(at, 8 * MIB, false) };
    assert_eq!(stats().moved_vmas, 0);
    assert_eq!(stats().in_use[Tag::FRAME.0 as usize], 0);
    balanced();
}
