//! D2894: a hash aggregate's T1 opened over its PostgreSQL floor (the driver's `SPILL_REGION_COMMIT` answer with
//! `RESERVE_FLOOR`, its modeled share charged) is laid out `T1_RESERVED | T1_FLOOR` by either open, and a full pool commits
//! more of the floor in place before any compact, as PostgreSQL's HashAggregate fills hash_mem before it spills: at a
//! modeled share under the floor, a compact first re-read the whole pool every time it filled (a two-key group-by at W0 1.08x).
//! A set operation side's reservation (no `RESERVE_FLOOR`) keeps option c''s order: compact first, commit only when the
//! compact leaves no room. Either way every row is held and nothing spills.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use gravity_abi::breaker::{HashAgg, KeyKind, Rec, RecordLayout, SlotKind, T1, T1_FLOOR, T1_NOFOLD, T1_RESERVED};
use gravity_abi::ctx::ctx_size;
use gravity_abi::program::{partial, RESERVE_FLOOR, SPILL_ARENA_CHUNK, SPILL_REGION_COMMIT};
use gravity_abi::{Ctx, Partials, ProgramBlock};
use gravity_kernels::host_abi::{agg_finish, agg_miss, agg_open, AGG_STATE_BYTES};
use gravity_kernels::table::entries::agg_open_grown;

const SHARE: u32 = 1 << 20;
const RESERVE: u64 = 32 << 20;
const LAYOUT: RecordLayout = RecordLayout::new_nn(&[KeyKind::I64], &[SlotKind::Count]);
/// Keys far past the table's slots (a miss evicts a record into the pool), few enough that their groups compact into a
/// quarter of the share's pool; every key seen `ROWS / KEYS` times.
const KEYS: u64 = 6_000;
const ROWS: u64 = 240_000;
/// The planner's groups the open sizes the table by: its slots far under `KEYS`.
const GROUPS: u64 = 64;

/// One host's counts: the commits it granted, the other ops (a spill) it refused. Each test has its own host and counts
/// (the tests run on parallel threads; each runs its two opens in turn).
struct Counts {
    commits: AtomicU64,
    other: AtomicU64,
}

static FLOOR_COUNTS: Counts = Counts { commits: AtomicU64::new(0), other: AtomicU64::new(0) };
static SETOP_COUNTS: Counts = Counts { commits: AtomicU64::new(0), other: AtomicU64::new(0) };

/// The driver's region host: the reservation's length (with `RESERVE_FLOOR` for a floor) at ask 0; a commit's bytes granted
/// (the test owns the reservation's bytes); the compact scratch's owner ask (`SPILL_ARENA_CHUNK` with 0 bytes,
/// `OpArena::own_owner_kind`) answered "no slot owner" (its scratch uncharged), not a spill; any other op (a spill)
/// counted and refused.
fn answer(counts: &Counts, floor: bool, op: u32, arg: u64) -> i64 {
    if op == SPILL_ARENA_CHUNK {
        return 0;
    }
    if op != SPILL_REGION_COMMIT {
        counts.other.fetch_add(1, Relaxed);
        return -1;
    }
    if arg == 0 {
        return (RESERVE | if floor { RESERVE_FLOOR } else { 0 }) as i64;
    }
    counts.commits.fetch_add(1, Relaxed);
    arg as i64
}

unsafe extern "C" fn floor_host(_ctx: *mut Ctx, op: u32, arg: u64) -> i64 {
    answer(&FLOOR_COUNTS, true, op, arg)
}

unsafe extern "C" fn setop_host(_ctx: *mut Ctx, op: u32, arg: u64) -> i64 {
    answer(&SETOP_COUNTS, false, op, arg)
}

struct Bound {
    ctx: Vec<u64>,
    _program: Box<ProgramBlock>,
    _partials: Box<Partials>,
    _own: Vec<u64>,
}

impl Bound {
    fn new(spill: unsafe extern "C" fn(*mut Ctx, u32, u64) -> i64) -> Bound {
        let mut own = vec![0u64; RESERVE as usize / 8];
        unsafe { HashAgg::arm(own.as_mut_ptr() as *mut u8, SHARE) };
        let mut partials = Box::new(Partials::two(own.as_mut_ptr() as *mut u8, [core::ptr::null_mut(); 2]));
        let mut program = Box::new(ProgramBlock::NONE);
        program.partial = &mut *partials;
        program.spill = Some(spill);
        let mut ctx = vec![0u64; ctx_size(0) / 8];
        unsafe { (*(ctx.as_mut_ptr() as *mut Ctx)).program = &mut *program };
        Bound { ctx, _program: program, _partials: partials, _own: own }
    }
}

/// `ROWS` rows over `KEYS` keys folded through `agg_miss` into a T1 opened (`agg_open_grown` when `sized`) over a host
/// that answers a floor when `floor`; the table drained at the end. The T1's flags, the commits asked, the rows its
/// records count, its records.
fn fold(floor: bool, sized: bool) -> (u8, u64, u64, u64) {
    let (counts, spill) = if floor { (&FLOOR_COUNTS, floor_host as unsafe extern "C" fn(*mut Ctx, u32, u64) -> i64) } else { (&SETOP_COUNTS, setop_host as _) };
    counts.commits.store(0, Relaxed);
    counts.other.store(0, Relaxed);
    let mut b = Bound::new(spill);
    let ctx = b.ctx.as_mut_ptr() as *mut Ctx;
    let mut st = vec![0u64; AGG_STATE_BYTES as usize / 8 + 2];
    let sp = st.as_mut_ptr() as *mut u8;
    let mut img = [0u8; 64];
    unsafe {
        let r = if sized { agg_open_grown(ctx, sp, &LAYOUT, 0, GROUPS) } else { agg_open(ctx, sp, &LAYOUT, 0, GROUPS) };
        assert_eq!(r, 0, "floor {floor} sized {sized}: the open succeeds");
        for x in 0..ROWS {
            img.fill(0);
            let src = Rec::at(img.as_mut_ptr(), &LAYOUT);
            src.open(&[(x % KEYS).wrapping_mul(0x9E37_79B9_7F4A_7C15)]);
            src.add_count(0, 1);
            assert_eq!(agg_miss(ctx, sp, img.as_ptr()), 0, "floor {floor} sized {sized}: row {x}");
        }
        assert_eq!(agg_finish(ctx, sp), 0, "floor {floor} sized {sized}: the drain");
        let t = T1::view(partial(ctx));
        let (mut rows, mut recs) = (0u64, 0u64);
        for p in 0..t.parts() as u32 {
            for c in t.chain(p) {
                for i in 0..c.recs {
                    rows += Rec::at((c.ptr as *mut u8).add(i * LAYOUT.size() as usize), &LAYOUT).count(0) as u64;
                    recs += 1;
                }
            }
        }
        assert_eq!(counts.other.load(Relaxed), 0, "floor {floor} sized {sized}: nothing spilled");
        (t.flags, counts.commits.load(Relaxed), rows, recs)
    }
}

#[test]
fn a_floor_t1_commits_its_pool_before_it_compacts() {
    for sized in [false, true] {
        let (flags, commits, rows, recs) = fold(true, sized);
        assert_eq!(flags & (T1_RESERVED | T1_FLOOR), T1_RESERVED | T1_FLOOR, "sized {sized}: laid out in the floor, marked a floor");
        assert_eq!(flags & T1_NOFOLD, 0, "sized {sized}: the floor never ran out");
        assert!(commits > 0, "sized {sized}: the full pool committed more of the floor in place");
        assert_eq!(rows, ROWS, "sized {sized}: every row held");
        assert!(recs > ROWS / 2, "sized {sized}: the pool kept the evicted records as they came (no compact folded them: {recs} records)");
    }
}

#[test]
fn a_set_operation_side_still_compacts_before_it_commits() {
    for sized in [false, true] {
        let (flags, commits, rows, recs) = fold(false, sized);
        assert_eq!(flags & (T1_RESERVED | T1_FLOOR), T1_RESERVED, "sized {sized}: laid out in the reservation, not a floor");
        assert_eq!(commits, 0, "sized {sized}: the compact made room, nothing committed");
        assert_eq!(rows, ROWS, "sized {sized}: every row held");
        assert!(recs < ROWS / 4, "sized {sized}: the compact folded records by key ({recs} records)");
    }
}
