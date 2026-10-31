//! Small lazy commits must not discard a whole warm superpage per base page.
#![cfg(target_os = "linux")]
mod common;
use common::*;
use pagepool::*;

#[test]
fn budget_reclaim_does_not_round_each_small_commit_up() {
    setup();
    let h = Hint::new(Class::Task, Tag::OTHER);
    let free = span(8 * MIB, h).unwrap();
    let free_addr = free.as_ptr() as usize;
    touch(free_addr, free.len());
    // SAFETY: our span, freed once and no longer accessed by the allocator client.
    unsafe { give_span(free) };
    assert_eq!(resident(free_addr, 8 * MIB), 8 * MIB);
    let mut live = span(64 * MIB, h).unwrap();
    assert!(live.is_lazy());
    assert_eq!(live.committed(), 0);
    set_claim_room(usize::MAX);
    set_reclaim_budget(Some(0));
    let before = guard_counts();
    let page = hooks::page_size();
    for committed in (page..=SUPER).step_by(page) {
        // SAFETY: increasing committed extent inside our live span.
        unsafe { commit_to(&mut live, committed) };
        touch(live.as_ptr() as usize + committed - page, page);
        let released = guard_counts().bytes - before.bytes;
        assert!(released <= committed as u64,
            "sub-superpage commits must not over-reclaim: committed={committed} released={released}");
        if committed < SUPER {
            assert_eq!(released, 0, "accumulate base-page growth before releasing a whole superpage");
        }
    }
    assert_eq!(guard_counts().bytes - before.bytes, SUPER as u64);
    assert_eq!(guard_counts().releases - before.releases, 1);
    // A relaxed/unknown budget discards pending fractional debt instead of charging it to a later pressure episode.
    let mut extend = |to| {
        let old = live.committed();
        // SAFETY: increasing committed extent of the span we own.
        unsafe { commit_to(&mut live, to) };
        touch(live.as_ptr() as usize + old, to - old);
    };
    extend(SUPER + page);
    set_reclaim_budget(None);
    extend(SUPER + 2 * page);
    set_reclaim_budget(Some(0));
    extend(2 * SUPER + page);
    assert_eq!(guard_counts().bytes - before.bytes, SUPER as u64, "discard old fractional debt when pressure clears");
    extend(2 * SUPER + 2 * page);
    assert_eq!(guard_counts().bytes - before.bytes, (2 * SUPER) as u64);
    assert_eq!(guard_counts().releases - before.releases, 2);
    for offset in (0..live.committed()).step_by(page) {
        // SAFETY: all checked bytes are inside the live, touched extent.
        assert_eq!(unsafe { core::ptr::read_volatile(live.as_ptr().add(offset)) }, 1);
    }
    set_reclaim_budget(None);
    // SAFETY: our live span, freed exactly once after the final read.
    unsafe { give_span(live) };
    purge_now(usize::MAX, None);
    balanced();
}
