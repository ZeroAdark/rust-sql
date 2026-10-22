//! pagepool P3 (`PGRUST_POOL_GRAVITY=1`): a program arena's block from the page pool keeps the arena's contract (unused
//! memory is zero, `zero` / `reset_to` re-zero, exhaustion is refused) in each of the pool's forms: a block under 2 MiB, a
//! pool span, a lazy span from `pool_lazy_min` up. Its bytes are on its owner's account as pool bytes (`Door::Pool`), all
//! gone at the drop; a lazy span's reset past `KEEP_BYTES` gives its runs dead and their charge off, and the regrowth reads
//! zero whether the purger released them or not. D4784: a warm lazy span longer than the arena that takes it has its
//! committed pages past the arena's capacity on the owner too, as the pool's PROGRAM row holds them. D4787: with
//! `PGRUST_POOL_SPAN_TRIM` that span comes trimmed to the arena's expected superpage, and the arena is charged the trimmed
//! commit.
#![cfg(target_os = "linux")]

use std::sync::{Mutex, MutexGuard, Once};

use gravity_program::arena::KEEP_BYTES;
use gravity_program::Arena;
use memledger::{account_row, Acct};
use pagepool::Tag;

const MIB: usize = 1 << 20;

/// The pool armed once with the Gravity switch, and this test alone on it.
fn serial() -> MutexGuard<'static, ()> {
    static ONCE: Once = Once::new();
    static TURN: Mutex<()> = Mutex::new(());
    ONCE.call_once(|| {
        std::env::set_var("PGRUST_POOL_GRAVITY", "1");
        // the D4784 tests take their warm span untrimmed: PGRUST_POOL_SPAN_TRIM (default ON on the branch) is off here
        // whatever order the tests run in, and `Trim` turns it on for the D4787 test's turn alone
        pagepool::hooks::span_trim_switch(false);
        assert!(pagepool::switches().gravity, "the switch, read at its first call");
        assert!(pagepool::init(&pagepool::Config::from_policy()), "the pool reserves its zones");
    });
    TURN.lock().unwrap_or_else(|e| e.into_inner())
}

fn on(procno: u32, f: impl FnOnce(Acct) + Send + 'static) {
    std::thread::spawn(move || {
        memledger::bind_backend(procno, 9000 + procno);
        f(Acct::backend(procno));
        pagepool::flush_shelf();
        memledger::unbind_backend();
    })
    .join()
    .unwrap();
}

/// Account `a`'s pool bytes, folded.
fn pool_bytes(a: Acct) -> i64 {
    memledger::fold();
    account_row(a).pagepool_bytes
}

fn program_in_use() -> usize {
    pagepool::stats().in_use[Tag::PROGRAM.0 as usize]
}

/// The warm lazy spans' pages released now and again at the drop, pass or fail: a D4784 test takes the warm span it
/// made itself, and no other test's arena takes a committed span from it.
///
/// A lazy ask of `n` 2 MiB slots (an arena of `bytes` asks `bytes + 32`) reuses a warm span of `n..=2n` slots, one with
/// committed pages first. So the D4784 tests keep their own size classes, whatever order the tests run in: 4803's arena
/// leaves a 49-slot span; 4804's longer arena asks 50 slots (50..=100: neither 49 nor 101) and its arena 25 (25..=50);
/// 4805's longer arena asks 101 slots (101..=202: neither 49 nor 50) and its arena 51 (51..=102); 4806's longer arena asks
/// 210 slots (past every other test's reach) and its arena 105 (105..=210: none of 49, 50, 101).
struct Purge;

impl Purge {
    fn now() -> Purge {
        pagepool::purge_now(usize::MAX >> 1, None);
        Purge
    }
}

impl Drop for Purge {
    fn drop(&mut self) {
        pagepool::purge_now(usize::MAX >> 1, None);
    }
}

/// `tests/arena.rs`' contract on `arena`.
fn exercise(arena: &Arena) {
    let a = arena.handle();
    let m0 = a.mark();
    let p = a.alloc(24);
    assert!(pagepool::owns(p), "the pool's memory");
    let s = unsafe { core::slice::from_raw_parts_mut(p, 24) };
    assert!(s.iter().all(|&b| b == 0));
    s.fill(0xab);
    let tail = a.alloc(a.capacity() - a.used() - 8);
    let t = unsafe { core::slice::from_raw_parts(tail.add(a.capacity() - 64), 48) };
    assert!(t.iter().all(|&b| b == 0));
    assert!(a.try_alloc(16).is_none(), "exhausted past the capacity");
    let off = a.offset_of(p);
    a.zero(off + 8, 8);
    assert_eq!(&arena.bytes(off + 24)[off..off + 8], &[0xab; 8]);
    assert_eq!(&arena.bytes(off + 24)[off + 8..off + 16], &[0; 8]);
    a.reset_to(m0);
    assert_eq!(a.mark(), m0);
    let q = a.alloc(24);
    assert_eq!(q, p);
    assert!(unsafe { core::slice::from_raw_parts(q, 24) }.iter().all(|&b| b == 0), "reset_to re-zeroes");
    a.reset_to(m0);
}

#[test]
fn an_arena_under_2_mib_is_a_pool_block_charged_whole_on_its_owner() {
    let _t = serial();
    on(4801, |a| {
        let (q0, u0) = (pool_bytes(a), program_in_use());
        let arena = Arena::new(64 << 10);
        let held = pool_bytes(a) - q0;
        assert!(held >= 64 << 10, "the block, whole, on its owner: {held}");
        assert_eq!(program_in_use() - u0, held as usize, "the pool's PROGRAM row holds what the owner holds");
        assert_eq!(arena.ledger_bytes(), (held as u64, true));
        exercise(&arena);
        drop(arena);
        assert_eq!((pool_bytes(a), program_in_use()), (q0, u0), "all of it back at the drop");
    });
}

#[test]
fn an_arena_from_2_mib_under_the_lazy_min_is_a_pool_span_charged_whole() {
    let _t = serial();
    on(4802, |a| {
        let q0 = pool_bytes(a);
        let arena = Arena::new(4 * MIB);
        let held = pool_bytes(a) - q0;
        assert!(held >= 4 * MIB as i64 && held % (2 * MIB as i64) == 0, "whole superpages: {held}");
        exercise(&arena);
        drop(arena);
        assert_eq!(pool_bytes(a), q0);
    });
}

#[test]
fn a_lazy_span_arena_gives_its_runs_past_the_kept_prefix_dead_and_reads_zero_after() {
    let _t = serial();
    on(4803, |a| {
        let q0 = pool_bytes(a);
        let bytes = KEEP_BYTES + 32 * MIB;
        let arena = Arena::new(bytes);
        let h = arena.handle();
        let first = h.alloc(8);
        assert!(pagepool::owns(first));
        assert!(pool_bytes(a) - q0 <= 512 << 10, "a lazy span is charged as its bump passes");
        exercise(&arena);
        let m = h.mark();
        for purge in [false, true] {
            let n = bytes - 2 * MIB;
            let p = h.alloc(n);
            unsafe { core::ptr::write_bytes(p, 0xab, n) };
            let charged = pool_bytes(a) - q0;
            assert!(charged >= n as i64, "{charged}");
            let dead0 = pagepool::stats().dead;
            h.reset_to(m);
            let given = charged - (pool_bytes(a) - q0);
            assert!(given >= 24 * MIB as i64, "the pages past KEEP_BYTES' step left the owner: {given}");
            assert!(pagepool::stats().dead >= dead0 + 24 * MIB, "and are the pool's dead runs");
            if purge {
                assert!(pagepool::purge_now(usize::MAX >> 1, None) > 0, "the purger released them");
            }
            let q = h.alloc(n);
            assert_eq!(q, p);
            assert!(unsafe { core::slice::from_raw_parts(q, n) }.iter().all(|&b| b == 0), "the regrowth reads zero (purged: {purge})");
            assert_eq!(pool_bytes(a) - q0, charged, "charged again as the bump passed");
            h.reset_to(m);
        }
        drop(arena);
        assert_eq!(pool_bytes(a), q0, "all of it back at the drop");
    });
}

#[test]
fn a_warm_lazy_span_longer_than_its_new_arena_is_charged_its_committed_pages_past_the_capacity_too() {
    let _t = serial();
    on(4804, |a| {
        let _purge = Purge::now();
        let q0 = pool_bytes(a);
        // a longer arena commits most of its span, then goes back warm with its pages (50 slots: `Purge`)
        let big = Arena::new(99 * MIB);
        let n = 90 * MIB;
        let p = big.handle().alloc(n);
        unsafe { core::ptr::write_bytes(p, 0xcd, n) };
        drop(big);
        assert_eq!(pool_bytes(a), q0, "the longer arena's charge left with it");
        // a shorter arena reuses a warm span (one of up to twice its length): its committed pages come resident and zeroed
        let u0 = program_in_use();
        let cap = 48 * MIB;
        let arena = Arena::new(cap);
        let in_use = program_in_use() - u0;
        let held = pool_bytes(a) - q0;
        assert!(in_use > cap, "the reused warm span holds more committed pages than the arena's capacity: {in_use}");
        assert_eq!(in_use - held as usize, 32, "the owner holds every committed page the pool's PROGRAM row holds, past the capacity too, but the arena's 4-word header (never charged)");
        assert_eq!(arena.ledger_bytes(), (held as u64, true));
        exercise(&arena);
        assert_eq!(pool_bytes(a) - q0, held, "a reset inside the kept prefix leaves the charge as it was");
        drop(arena);
        assert_eq!((pool_bytes(a), program_in_use()), (q0, u0), "all of it back at the drop");
    });
}

#[test]
fn a_warm_lazy_span_longer_than_its_new_arena_gives_its_runs_past_keep_off_the_charge_and_takes_them_back_on_regrowth() {
    let _t = serial();
    on(4805, |a| {
        let _purge = Purge::now();
        let q0 = pool_bytes(a);
        // a longer arena commits most of its span, then goes back warm with its pages (101 slots: `Purge`)
        let big = Arena::new(201 * MIB);
        let n = 190 * MIB;
        let p = big.handle().alloc(n);
        unsafe { core::ptr::write_bytes(p, 0xcd, n) };
        drop(big);
        assert_eq!(pool_bytes(a), q0, "the longer arena's charge left with it");
        // an arena past KEEP_BYTES reuses it: charged its capacity and the committed pages past it, from the start
        let u0 = program_in_use();
        let arena = Arena::new(101 * MIB);
        let h = arena.handle();
        let cap = h.capacity();
        let held = pool_bytes(a) - q0;
        assert!(held as usize > cap, "the reused warm span holds more committed pages than the arena's capacity: {held}");
        assert_eq!(program_in_use() - u0 - held as usize, 32, "the owner holds all the PROGRAM row holds but the 4-word header");
        // the bump past KEEP_BYTES: the charged words are at the capacity already, so the charge does not move
        let m0 = h.mark();
        let past = KEEP_BYTES + 8 * MIB;
        let p = h.alloc(past);
        unsafe { core::ptr::write_bytes(p.add(KEEP_BYTES), 0xab, 8 * MIB) };
        assert_eq!(pool_bytes(a) - q0, held, "a bump inside the capacity charges nothing more");
        // the trim: pool_reset gives the words [keep, was) dead and off the charge. was = the capacity's words; keep = the
        // first page boundary past KEEP_BYTES of the data (which starts 32 bytes into the span), in CHARGE_WORDS steps
        let (dead0, u1) = (pagepool::stats().dead, program_in_use());
        h.reset_to(m0);
        let charge = pool_bytes(a) - q0;
        let step = memledger::DOOR_FOLD_BYTES as usize;
        let keep = ((KEEP_BYTES + 32).next_multiple_of(4096) - 32).next_multiple_of(step);
        assert_eq!(charge, held - (cap - keep) as i64, "charge == held - (was - keep) * 8");
        // the PROGRAM row drops by the whole runs given dead (the pool's stats().dead): the given range's partial runs at its
        // two ends stay in in_use, so in_use - charge = 32 + (was - keep) * 8 - the given runs' bytes
        let given = pagepool::stats().dead - dead0;
        assert!(given > 0 && given <= cap - keep, "the runs given dead: {given} of {}", cap - keep);
        assert_eq!(u1 - program_in_use(), given, "the PROGRAM row reads the given pages out of in_use, as dead runs");
        assert_eq!(program_in_use() - u0 - charge as usize, 32 + (cap - keep - given), "in_use - charge after the trim");
        // the regrowth to the capacity takes the runs back: the charge is held again, and the pages past KEEP read zero
        let q = h.alloc(cap - h.used());
        assert_eq!(q, p);
        assert_eq!(pool_bytes(a) - q0, held, "charged back to held as the bump passed");
        assert_eq!(program_in_use() - u0 - held as usize, 32, "and the PROGRAM row with it");
        let back = unsafe { core::slice::from_raw_parts(q.add(KEEP_BYTES), 8 * MIB) };
        assert!(back.iter().all(|&b| b == 0), "the given pages read zero on the regrowth");
        h.reset_to(m0);
        drop(arena);
        assert_eq!((pool_bytes(a), program_in_use()), (q0, u0), "all of it back at the drop");
    });
}

/// `PGRUST_POOL_SPAN_TRIM` on for one test, inside its `serial` turn, and off again at its end, pass or fail.
struct Trim;

impl Trim {
    fn on() -> Trim {
        pagepool::hooks::span_trim_switch(true);
        Trim
    }
}

impl Drop for Trim {
    fn drop(&mut self) {
        pagepool::hooks::span_trim_switch(false);
    }
}

#[test]
fn a_trimmed_warm_lazy_span_charges_its_new_arena_the_trimmed_commit() {
    let _t = serial();
    on(4806, |a| {
        let _purge = Purge::now();
        let q0 = pool_bytes(a);
        // a longer arena commits much of its span, then goes back warm with its pages (210 slots: `Purge`)
        let big = Arena::new(419 * MIB);
        let n = 100 * MIB;
        let p = big.handle().alloc(n);
        unsafe { core::ptr::write_bytes(p, 0xcd, n) };
        drop(big);
        assert_eq!(pool_bytes(a), q0, "the longer arena's charge left with it");
        // D4787: a shorter arena takes it again with the trim on, so the span comes cut to the arena's trim target: its
        // hint's expect_kib (2 MiB), in whole 2 MiB superpages
        let _trim = Trim::on();
        let target = 2 * MIB;
        let (u0, r0) = (program_in_use(), pagepool::span_reuse());
        let arena = Arena::new(208 * MIB);
        let r1 = pagepool::span_reuse();
        assert_eq!(r1.trims - r0.trims, 1, "the warm span came back trimmed");
        let cut = (r1.trimmed - r0.trimmed) as usize;
        assert!(cut >= n - target, "its commit past the target went: {cut}");
        let held = pool_bytes(a) - q0;
        assert_eq!(held, (target - 32) as i64, "held after a trimmed take == trim_target - 32 (the 4-word header)");
        assert_eq!(program_in_use() - u0, target, "the PROGRAM row holds the trimmed commit, not the commit the span had");
        assert_eq!(arena.ledger_bytes(), (held as u64, true));
        exercise(&arena);
        drop(arena);
        assert_eq!((pool_bytes(a), program_in_use()), (q0, u0), "all of it back at the drop");
    });
}
