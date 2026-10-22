//! A group's text image is a region use (ruling 2026-09-21): an owner's `pin` from a worker thread draws its image cut of the
//! statement's reservation (`Split::images`, carved from the share) and succeeds with the pool armed at exactly the statement's
//! footprint; only past the worker's W (its cut and, since S4, its arena) is it refused — a program without images (no cut) pins
//! inside its arena alone, the regexp group-by refusal this rig would have caught. W is carved out of G when its section binds;
//! the leader has no carve (its pins charge G itself, inside the footprint).

use gravity_driver_rig as common;

use std::sync::atomic::{AtomicU64, AtomicU8};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use gravity_abi::MorselRange;
use gravity_driver::{footprint, image_cut, program_images, regions_for, regions_of, Claimed, Entry, Error, Grant, Open, Step, POOL, REGIONS, SHARE_FLOOR, worker_arena};

static LOCK: Mutex<()> = Mutex::new(());
const MIB: u64 = 1 << 20;
const NULLS: [u8; 1] = [0];
const BLOCKS: u64 = 24;
const PER_BLOCK: u64 = 10;
const WIDTH: u16 = 2;

fn armed(capacity: u64) -> std::sync::MutexGuard<'static, ()> {
    armed_under(LOCK.lock().unwrap_or_else(|p| p.into_inner()), capacity)
}

/// `armed` under a lock the caller already holds (DR-12: a footprint reads the arena draws other tests' runs noted, so
/// it is taken under the lock that orders those runs).
fn armed_under(g: std::sync::MutexGuard<'static, ()>, capacity: u64) -> std::sync::MutexGuard<'static, ()> {
    POOL.gate(0);
    gravity_driver::forget_arena_draws();
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    assert_eq!(POOL.held(), 0, "a previous test left bytes held");
    POOL.arm(capacity);
    g
}

fn drain(open: &Open<'_>, cursor: &AtomicU64, ordinal: u16) -> Result<u64, Error> {
    let mut n = 0;
    loop {
        let lo = cursor.fetch_add(5, std::sync::atomic::Ordering::AcqRel);
        if lo >= BLOCKS {
            return Ok(n);
        }
        match open.run_claim(ordinal, MorselRange::new(lo, (lo + 5).min(BLOCKS)))? {
            Claimed::Done => n += 1,
            Claimed::Stopped => return Ok(n),
        }
    }
}

/// `bytes` pinned through worker 0's context from another thread: the pointer, null = refused.
fn pin_from_worker(open: &Open<'_>, bytes: u64) -> bool {
    let ctx = open.worker_ctx(0) as usize;
    std::thread::scope(|s| s.spawn(move || unsafe { !gravity_abi::program::pin(ctx as *mut gravity_abi::Ctx, bytes).is_null() }).join().unwrap())
}

/// The statement over `program` at `want`, the pool armed at exactly its footprint; `body` runs with the section open and
/// that footprint. DR-12: a footprint read before the lock and `forget_arena_draws` counts the arena draws an earlier test's
/// run of the same program noted (a worker that pinned its whole cut: 8 MiB more at --test-threads 1), so a body compares
/// the pool's holds with the footprint taken here, never with one of its own.
fn statement(program: Arc<gravity_program::Program>, want: Grant, _images: bool, body: impl FnOnce(&Open<'_>, u64)) {
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table);
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    gravity_driver::forget_arena_draws();
    let need = footprint(want, &program, WIDTH);
    let _g = armed_under(g, need);
    let a = std::rc::Rc::new(POOL.admit_need(want, &|g| footprint(g, &program, WIDTH), Duration::from_millis(10)).unwrap());
    assert_eq!(a.reserved, need, "the whole reservation fits the capacity");
    assert_eq!(POOL.held(), need);
    {
        let mut sink = LegSink::new();
        let mut b = binding(&[8], &NULLS, &mut sink, &flag);
        let mut exec = entry.start(None, &program, &mut derive, &mut b, a.clone()).unwrap();
        let open = entry.open_section(&mut exec, &b, &mut derive, 0, WIDTH).unwrap();
        assert_eq!(POOL.held(), need, "the regions came out of the reservation, nothing more held");
        body(&open, need);
        let cursor = AtomicU64::new(0);
        for o in 0..WIDTH {
            drain(&open, &cursor, o).unwrap();
        }
        entry.close_section(open, &mut b, &GroupMerge, true).unwrap();
        assert_eq!(entry.run(&mut exec, &mut b, gravity_driver::Call::batch(4)).unwrap(), Step::Done);
        entry.end(exec, true);
    }
    assert_eq!(std::rc::Rc::strong_count(&a), 1, "the Exec gave its lease back");
    drop(a);
    drop(entry);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    assert_eq!(POOL.held(), 0, "every region, chunk and W released at the statement's end");
    POOL.arm(0);
}

#[test]
fn the_image_cut_is_carved_from_the_share_and_the_reservation_is_unchanged() {
    let want = Grant::of(64 * MIB);
    let plain = want.split(1, WIDTH);
    let cut = want.split_for(1, WIDTH, true);
    assert_eq!(cut.images.bytes, image_cut(plain.worker.bytes));
    assert_eq!(cut.images.bytes, 4 * MIB as u32, "an eighth of the 32 MiB worker share");
    assert_eq!(cut.breaker.bytes + cut.images.bytes, plain.breaker.bytes);
    assert_eq!(cut.worker.bytes + cut.images.bytes, plain.worker.bytes);
    assert_eq!(want.split_for(1, WIDTH, false), plain, "no images: no cut");
    assert_eq!(regions_of(want, 1, WIDTH), plain.breaker.bytes as u64 + plain.worker.bytes as u64 * WIDTH as u64);
    assert_eq!(regions_of(want, 2, WIDTH), 2 * (32 * MIB) + 16 * MIB * WIDTH as u64, "two live breakers: two leader regions of their share");
    assert_eq!(regions_of(Grant::of(100 * MIB), 3, 1), 3 * 34_955_264 + 34_952_528, "a share page-rounded, never its power-of-two class (D157); a multiple of 8 (D335)");
    assert_eq!(image_cut(4 * MIB as u32), MIB as u32, "at least a MiB");
    assert_eq!(image_cut(SHARE_FLOOR), 0, "a floor share spares nothing");
    assert_eq!(image_cut(SHARE_FLOOR + 28), 24, "what the floor leaves, word-aligned");
    assert!(program_images(&p_group_images(4)));
    assert!(!program_images(&p_group_share(4)));
}

#[test]
fn a_worker_pins_its_images_inside_the_grant_and_is_refused_only_past_it() {
    pins_inside_the_cut();
}

/// D226: the refusal count is the process's; under load the other test's refusal landed first (left 2, right 1).
#[test]
fn a_refusal_before_is_not_counted_as_this_statements() {
    first_pin_without_a_cut();
    pins_inside_the_cut();
}

fn pins_inside_the_cut() {
    let want = Grant::of(64 * MIB);
    let cut = want.split_for(1, WIDTH, true).images.bytes as u64;
    assert_eq!(regions_for(want, 1, WIDTH, true), regions_of(want, 1, WIDTH), "the leader's cut comes out of its own share: its region is the cut share (D157)");
    let program = Arc::new(p_group_images(4));
    // a worker's W is its image cut and its arena (S4), carved out of the statement's G
    let cut = cut + worker_arena(want, WIDTH);
    statement(program, want, true, |open, need| {
        assert_eq!(open.worker_room(0), cut, "the worker's cut, carved out of the statement's G");
        assert_eq!(open.worker_room(1), cut);
        // the chunks of an owner's images: inside the cut every pin lands, the pool holds nothing more
        let chunk = 512 * 1024;
        let mut pinned = 0;
        while pinned + chunk <= cut {
            assert!(pin_from_worker(open, chunk), "{pinned} pinned of {cut}: inside the cut");
            pinned += chunk;
            assert_eq!(POOL.held(), need, "an image inside the cut is the reservation's");
        }
        assert_eq!(open.worker_room(0), cut - pinned);
        assert_eq!(open.worker_room(1), cut, "the other worker's W is its own");
        // past W, the pool exactly full: the chunk's reservation waits, then is refused (the runner names it), nothing leaked
        let refusals = POOL.stats().refusals;
        assert!(!pin_from_worker(open, chunk), "past the worker's W with the capacity full");
        assert_eq!(POOL.held(), need);
        assert_eq!(POOL.stats().refusals - refusals, 1, "this pin's refusal, not the process's count (D226)");
    });
}

#[test]
fn without_the_cut_a_worker_pins_inside_its_arena_only() {
    first_pin_without_a_cut();
}

fn first_pin_without_a_cut() {
    let want = Grant::of(64 * MIB);
    let program = Arc::new(p_group_share(4));
    statement(program, want, false, |open, need| {
        assert_eq!(open.worker_room(0), 4 * MIB, "no images declared: no cut, the worker's arena alone (S4: 64 MiB / 16, capped at 4 MiB)");
        for _ in 0..8 {
            assert!(pin_from_worker(open, 512 * 1024), "inside the arena");
        }
        assert!(!pin_from_worker(open, 512 * 1024), "no cut: an image chunk past the worker's W is refused");
        assert_eq!(POOL.held(), need);
    });
}

/// A pin is bounded by the owner's cut, never by the grow pool's chunk count (none since D642; `MAX_CHUNKS` = 64 per owner then): a regexp group-by's images at W8
/// were refused at the 16 MiB chunk with the reservation untouched — the leader's 60 `arena_grow` chunks and its pins shared the cap.
#[test]
fn pins_are_bounded_by_the_cut_not_the_chunk_count() {
    let want = Grant::of(128 * MIB);
    let cut = want.split_for(1, WIDTH, true).images.bytes as u64;
    assert_eq!(cut, 8 * MIB);
    let program = Arc::new(p_group_images(4));
    statement(program, want, true, |open, need| {
        let chunk = 64 * 1024;
        for i in 0..100 {
            assert!(pin_from_worker(open, chunk), "pin {i}: 100 chunks of 64 KB (6.25 MiB) inside an 8 MiB cut, past MAX_CHUNKS");
        }
        assert_eq!(open.worker_room(0), cut + worker_arena(want, WIDTH) - 100 * chunk);
        assert_eq!(POOL.held(), need);
    });
}
