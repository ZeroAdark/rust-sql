//! Known-zero reclaim guidance differs from an unknown limit and must hold optional cold prefaults.
#![cfg(target_os = "linux")]
mod common;
use common::*;
use pagepool::*;

#[derive(Clone, Copy)]
enum Budget { Unknown, Cap, Ledger, Published, Empty }

fn check(enabled: bool, source: Budget) {
    hooks::grow_guard_switch(enabled);
    let h = Hint::new(Class::Task, Tag::OTHER);
    let live = take(9 * 64 * 1024, 64, h).unwrap();
    let p = live.cast::<u8>().as_ptr() as usize;
    touch(p, live.len());
    let free = span(8 * MIB, h).unwrap();
    let free_addr = free.as_ptr() as usize;
    touch(free_addr, free.len());
    // SAFETY: owned span, freed exactly once.
    unsafe { give_span(free) };
    assert_eq!(resident(free_addr, 8 * MIB), 8 * MIB);
    let cap = stats().claimed + 64 * MIB;
    match source {
        Budget::Unknown => set_reclaim_budget(None),
        Budget::Cap => set_cap(cap),
        Budget::Ledger => memledger::note_b(cap as u64),
        Budget::Published => set_reclaim_budget(Some(cap)),
        Budget::Empty => set_reclaim_budget(Some(0)),
    }
    set_claim_room(usize::MAX);
    let before = guard_counts();
    let big = take(64 * MIB, SUPER, Hint::new(Class::Process, Tag::REGION)).expect("soft budget must permit demand allocation");
    let after = guard_counts();
    if enabled && !matches!(source, Budget::Unknown) {
        assert!(after.bytes >= before.bytes + (8 * MIB) as u64, "known or finite budget must release free pages before growth");
        assert!(after.bytes - before.bytes <= (16 * MIB) as u64, "bounded inline reclaim");
        assert_eq!(resident(free_addr, 8 * MIB), 0);
    } else {
        assert_eq!(after.releases, before.releases, "off or unknown must not reclaim inline");
        assert_eq!(resident(free_addr, 8 * MIB), 8 * MIB);
    }
    for at in (0..live.len()).step_by(hooks::page_size()) {
        // SAFETY: inside the allocation held throughout reclamation.
        assert_eq!(unsafe { core::ptr::read_volatile((p + at) as *const u8) }, 1, "live data changed");
    }
    set_cap(0);
    memledger::note_b(0);
    set_reclaim_budget(None);
    // SAFETY: our live allocations, freed exactly once.
    unsafe { give(big.cast(), big.len()); give(live.cast(), live.len()); }
    flush_shelf();
    purge_now(usize::MAX, None);
    balanced();
}

#[test]
fn known_budget_reclaims_and_holds_prefault() {
    setup();
    assert!(guard_counts().on);
    check(true, Budget::Unknown);
    check(false, Budget::Empty);
    check(true, Budget::Cap);
    check(true, Budget::Ledger);
    check(true, Budget::Published);
    check(true, Budget::Empty);
    set_reclaim_budget(Some(0));
    let before = stats().prefaulted;
    assert_eq!(prefill(SUPER, 1), 0, "known-zero budget must hold cold prefaults");
    assert_eq!(stats().prefaulted, before);
    set_reclaim_budget(Some(stats().claimed + 32 * MIB));
    assert_eq!(prefill(SUPER, 1), SUPER, "raising the budget must permit prefaulting again");
    set_reclaim_budget(Some(stats().claimed));
    assert_eq!(prefill(SUPER, 1), 0, "used finite budget must hold cold prefaults");
    set_reclaim_budget(None);
    assert_eq!(prefill(SUPER, 1), SUPER, "unknown limit restores previous behavior");
    purge_now(usize::MAX, None);
}
