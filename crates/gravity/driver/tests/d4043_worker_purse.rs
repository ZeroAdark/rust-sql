//! D4043 (P1: stock TPC-H q18 and q03 failed alone at the default settings, 53200): a worker has no statement (D187), so
//! its draw past its W took the pool's capacity alone, never its statement's reserved-but-unused bytes. A statement whose
//! own reservation filled the pool (an Estimate admission alone past it: q18 reserved 4188 MiB of a 3327 MiB pool for a
//! 64 MiB grant, nearly all of it the parallel owner pin) refused its workers' first grow past W at once (`take_wait`: no
//! other holder could give bytes back) with G's room untouched. G's room after the W carves is now the statement's purse
//! at a section's open, which its workers draw past W before the pool (`grant::Purse`). A build far past its share at W2
//! under a reservation that is the whole pool: each worker's grows past its W are served from the reservation, the pool
//! holds nothing past it, every row arrives, and every byte goes back once. The pool is process-wide: the tests take turns.

use gravity_driver_rig as common;

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use gravity_abi::MorselRange;
use gravity_driver::mem::grant::{PURSE_DRAWS, PURSE_MISSES};
use gravity_driver::{Call, Claimed, Entry, Exec, Grant, Open, Reservation, Step, POOL, REGIONS, SHARE_FLOOR};

const MIB: u64 = 1 << 20;
const NULLS: [u8; 1] = [0];
const PER_BLOCK: u64 = 64;
/// 512x the 64 KiB share floor in records (16 bytes a row): 32 MiB of rows, 16 MiB a worker, far past a worker's W.
const BLOCKS: u64 = (512 * SHARE_FLOOR as u64 / 16) / PER_BLOCK;
const CLAIM: u64 = 256;
const WIDTH: u16 = 2;
/// The pool, and the statement's reservation: all of it.
const CAP: u64 = 512 * MIB;
static LOCK: Mutex<()> = Mutex::new(());

/// The pool armed at `CAP` and a small grant admitted with all of it reserved (q18's shape: the reservation, not the grant,
/// fills the pool).
fn admit_whole_pool() -> std::rc::Rc<Reservation> {
    admit(CAP)
}

/// The pool armed at `CAP` and a small grant admitted with `need` reserved.
fn admit(need: u64) -> std::rc::Rc<Reservation> {
    // each worker's W at its prior: the other test's run of this program noted its workers' draws (DR-12's seen arena)
    gravity_driver::forget_arena_draws();
    POOL.gate(0);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    POOL.arm(CAP);
    let res = std::rc::Rc::new(POOL.admit_need_until(Grant::of(SHARE_FLOOR as u64), &|_| need, Duration::from_secs(5), &|| false).expect("admitted"));
    assert_eq!((res.reserved, POOL.held()), (need, need), "the statement holds {need} B of the pool");
    res
}

/// Every claim of the build on `open`'s two workers, each one served.
fn claims(open: &Open<'_>) {
    let mut lo = 0;
    while lo < BLOCKS {
        let o = ((lo / CLAIM) % WIDTH as u64) as u16;
        let r = open.run_claim(o, MorselRange::new(lo, (lo + CLAIM).min(BLOCKS)));
        assert!(matches!(r, Ok(Claimed::Done)), "claim at block {lo} on worker {o}: {r:?} (the pool holds {} of {}; a worker's grow past its W refused while its statement's reservation sat unused: D4043)", POOL.held(), POOL.capacity());
        lo += CLAIM;
    }
}

fn totals() -> Vec<Vec<u64>> {
    let rows = BLOCKS * PER_BLOCK;
    let sum = rows * (rows - 1) / 2;
    vec![vec![rows, sum, sum, sum]]
}

/// The pool's bytes all given back once the depots let go of what they keep: none leaked, none released twice.
fn all_back() {
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    assert_eq!(POOL.held(), 0, "the reservation, the purse and every draw from it given back once");
    POOL.arm(0);
}

#[test]
fn a_workers_grow_past_its_w_draws_its_statements_unused_reservation() {
    let _l = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    memledger::bind_backend(7, 4043);
    let acct = memledger::current();
    // the claim (C8 move 3c: the views' pool_reserved_bytes is its unbacked part; the leader's own draws from G back it)
    let shown = || memledger::pool_reserved(acct);
    let program = Arc::new(p_grow_share(4));
    let mut derive = FakeDerive::ranges(RangeTable::new(BLOCKS, PER_BLOCK));
    let res = admit_whole_pool();
    // the statement's part of the pool at 1 %: a draw past the reservation would be refused there; one from the purse is
    // inside it (D3982 holds a worker's draws past G to the same part)
    res.set_pool_fraction(10_000);
    assert_eq!(shown(), CAP as i64, "admission charged the reservation on the statement's ledger account once");
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[0], &NULLS, &mut sink, &flag);
    let mut exec = entry.start(None, &program, &mut derive, &mut b, res.clone()).unwrap();
    let open = entry.open_section(&mut exec, &b, &mut derive, 0, WIDTH).unwrap();
    let w = [open.worker_room(0), open.worker_room(1)];
    assert!(res.purse_bytes() > 0 && res.left() == res.purse_bytes(), "G's room past the W carves is in the purse ({} of {} left)", res.purse_bytes(), res.left());
    // the leader's own hold past G's room comes out of the purse too; kept past the statement's end (the late drop below)
    let late = res.hold(4 * MIB).expect("the leader's hold past G's room, from the purse");
    assert_eq!(POOL.held(), CAP, "a purse draw takes nothing more from the pool");
    let draws = PURSE_DRAWS.load(Ordering::Relaxed);
    claims(&open);
    for o in 0..WIDTH {
        assert!(open.partial(o).bytes.len() as u64 > w[o as usize], "worker {o}'s partial grew past its W ({} B)", w[o as usize]);
    }
    assert!(PURSE_DRAWS.load(Ordering::Relaxed) > draws, "the grows past W were the purse's draws");
    assert_eq!(POOL.held(), CAP, "every draw past W came out of the statement's own reservation: nothing past it");
    assert_eq!((res.grown(), res.charged()), (0, CAP), "no purse draw counts past G (PastCount)");
    assert_eq!(shown(), CAP as i64, "no purse draw charges the ledger again (the workers ran on this account)");
    entry.close_section(open, &mut b, &GrowMerge, true).unwrap();
    assert_eq!(res.purse_bytes(), 0, "the close gave the purse's bytes back to G's room");
    assert_eq!(entry.run(&mut exec, &mut b, Call::batch(64)).unwrap(), Step::Done);
    entry.end(exec, true);
    assert_eq!(sink.sorted(), totals(), "every row, head and tail intact across the grows");
    assert_eq!(std::rc::Rc::strong_count(&res), 1, "the Exec dropped its lease");
    drop(res);
    assert_eq!(shown(), 0, "the reservation's charge left the account whole");
    // a purse draw dropped after its statement ended goes to the pool (the purse is closed), once
    assert!(POOL.held() >= 4 * MIB, "the late hold still holds its bytes: {}", POOL.held());
    drop(late);
    drop(entry);
    all_back();
    memledger::unbind_backend();
}

/// One statement, two sections open at once (two Execs on its reservation): the first one's close leaves the purse to the
/// second one's live workers (a count of the open sections; only the last close drains it).
#[test]
fn a_sections_close_leaves_the_purse_to_another_open_section() {
    let _l = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let program = Arc::new(p_grow_share(4));
    let (mut d1, mut d2) = (FakeDerive::ranges(RangeTable::new(BLOCKS, PER_BLOCK)), FakeDerive::ranges(RangeTable::new(BLOCKS, PER_BLOCK)));
    let res = admit_whole_pool();
    let (mut e1, mut e2) = (Entry::new(2), Entry::new(2));
    let (f1, f2) = (AtomicU8::new(0), AtomicU8::new(0));
    let (mut s1, mut s2) = (LegSink::new(), LegSink::new());
    let mut b1 = binding(&[0], &NULLS, &mut s1, &f1);
    let mut b2 = binding(&[0], &NULLS, &mut s2, &f2);
    let mut x1: Exec = e1.start(None, &program, &mut d1, &mut b1, res.clone()).unwrap();
    let mut x2: Exec = e2.start(None, &program, &mut d2, &mut b2, res.clone()).unwrap();
    let o1 = e1.open_section(&mut x1, &b1, &mut d1, 0, WIDTH).unwrap();
    claims(&o1);
    // the second section binds while the first is open: its W carves come out of the purse
    let o2 = e2.open_section(&mut x2, &b2, &mut d2, 0, WIDTH).unwrap();
    assert!(o2.worker_room(0) > 0 && o2.worker_room(1) > 0, "the second section's workers have their W");
    e1.close_section(o1, &mut b1, &GrowMerge, true).unwrap();
    assert!(res.purse_bytes() > 0, "the first close left the purse to the second section's workers");
    // the regions the first close parked go back to the pool (else the second section's workers reuse them and draw nothing
    // fresh): that leaves the pool room, which a draw past W must not take while the purse holds the reservation's bytes
    REGIONS.trim();
    let (held, draws) = (POOL.held(), PURSE_DRAWS.load(Ordering::Relaxed));
    assert!(held < CAP, "the first close and the trim gave carved bytes back: {held} held of {CAP}");
    claims(&o2);
    assert!(PURSE_DRAWS.load(Ordering::Relaxed) > draws, "the second section's grows past W were the purse's draws");
    assert_eq!(POOL.held(), held, "the second section's draws came out of the statement's own reservation, not the pool's room");
    e2.close_section(o2, &mut b2, &GrowMerge, true).unwrap();
    assert_eq!(res.purse_bytes(), 0, "the last close gave the purse's bytes back to G's room");
    assert_eq!(e1.run(&mut x1, &mut b1, Call::batch(64)).unwrap(), Step::Done);
    assert_eq!(e2.run(&mut x2, &mut b2, Call::batch(64)).unwrap(), Step::Done);
    e1.end(x1, true);
    e2.end(x2, true);
    assert_eq!((s1.sorted(), s2.sorted()), (totals(), totals()), "both sections' rows");
    drop(res);
    drop((e1, e2));
    all_back();
}

/// gravfix3's review of (a): a leader's road reads `left` (G's room and the purse's bytes) a step before its take, and the
/// section's workers draw the purse on their own threads, so it can be empty at the take. That draw goes on past G, the road
/// G's room alone gave it before the purse (from the pool's room, counted as grown), never a refusal of a draw the pool had
/// room for. The witness runs the workers' draw between the road and the take.
#[test]
fn a_leaders_draw_whose_purse_the_workers_drew_after_its_road_was_read_goes_past_g() {
    let _l = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let program = Arc::new(p_grow_share(4));
    let mut derive = FakeDerive::ranges(RangeTable::new(BLOCKS, PER_BLOCK));
    // half the pool reserved: the road past G has the pool's room
    let res = admit(CAP / 2);
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[0], &NULLS, &mut sink, &flag);
    let mut exec = entry.start(None, &program, &mut derive, &mut b, res.clone()).unwrap();
    let open = entry.open_section(&mut exec, &b, &mut derive, 0, WIDTH).unwrap();
    let purse = res.purse_bytes();
    assert!(purse >= 4 * MIB && res.left() == purse, "G's room is all in the purse ({purse} of {} left)", res.left());
    let (refusals, misses, held) = (POOL.stats().refusals, PURSE_MISSES.load(Ordering::Relaxed), POOL.held());
    // the workers' draws: the whole purse, after the leader's road read it
    let mut theirs = None;
    let draw = gravity_driver::mem::region::leader_road_race_witness(4 * MIB as usize, &res, || theirs = res.hold(res.purse_bytes()));
    assert!(theirs.is_some() && res.purse_bytes() == 0, "the purse was drawn empty between the road and the take");
    let draw = draw.expect("the leader's draw goes past G when its purse was drawn before the take, as it did before the purse (gravfix3's leader-road race)");
    assert_eq!(PURSE_MISSES.load(Ordering::Relaxed), misses + 1, "the take missed the purse once");
    assert_eq!(POOL.stats().refusals, refusals, "no refusal counted: the draw was served past G");
    assert_eq!((res.grown(), POOL.held()), (4 * MIB, held + 4 * MIB), "served from the pool's room past the reservation, counted as grown");
    drop(draw);
    drop(theirs);
    assert_eq!((res.purse_bytes(), POOL.held()), (purse, held), "both draws given back: the purse's to it, the pool's to the pool");
    claims(&open);
    entry.close_section(open, &mut b, &GrowMerge, true).unwrap();
    assert_eq!(entry.run(&mut exec, &mut b, Call::batch(64)).unwrap(), Step::Done);
    entry.end(exec, true);
    assert_eq!(sink.sorted(), totals(), "every row");
    drop(res);
    drop(entry);
    all_back();
}
