//! D225: a kernel's arena scratch (`gravity_kernels::util::scratch` → `arena_grow`) past an admitted statement's G. S7b refused
//! it outright (wide group-by and top-N statements at W16 on the kernel arm: out of memory with the capacity free); the chunk is now
//! reserved from the pool at its actual size — a wait while another statement could give room back (D3239: none here, so
//! at once), then the named out-of-memory — and counted as the statement's draw.
//! One test per process: the ledger mode is read once.

use gravity_driver_rig as common;

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use gravity_driver::grant_ledger::{counts, Check};
use gravity_driver::{footprint, Entry, Grant, POOL, REGIONS};

const MIB: u64 = 1 << 20;
const NULLS: [u8; 1] = [0];

/// D3165: the memory waits that paused (`set_mem_wait`'s start: once per wait, at its first pause). A refusal "at once" is
/// one with no pause, read here, not by a wall-clock bound that follows the box's load (16.86 s at load1 81 on box 84).
static PAUSED: AtomicU64 = AtomicU64::new(0);

fn paused() {
    PAUSED.fetch_add(1, Relaxed);
}

fn resumed() {}

#[test]
fn a_leader_arena_chunk_past_g_is_reserved_at_its_size_then_refused_by_name_when_the_pool_is_full() {
    std::env::set_var("PGRUST_GRAVITY_LEDGER", "report");
    gravity_driver::mem::grant::set_mem_wait(paused, resumed);
    let program = Arc::new(p_group_share(4));
    let want = Grant::of(64 * MIB);
    let need = footprint(want, &program, 1);
    let spare = 16 * MIB;
    POOL.gate(0);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    POOL.arm(2 * need + spare);
    let res = std::rc::Rc::new(POOL.admit_need(want, &|g| footprint(g, &program, 1), Duration::from_millis(10)).unwrap());
    // the pool's own road first (D2990's per-statement part off: the whole big lane); the part is checked at the end
    res.set_pool_fraction(gravity_driver::POOL_FRACTION_WHOLE);
    let mut derive = FakeDerive::ranges(RangeTable::new(24, 10));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[8], &NULLS, &mut sink, &flag);
    let exec = entry.start(None, &program, &mut derive, &mut b, res.clone()).unwrap();
    let ctx = exec.node().ctx();
    let past = res.left() + MIB;
    let (held, l1, drawn) = (POOL.held(), counts()[Check::L1 as usize], res.drawn());
    let got = unsafe { gravity_abi::program::arena_grow(ctx, past) };
    assert_eq!(got, past as i64, "a chunk {past} past G's {} left is reserved, not refused (S7b's out-of-memory refusals)", res.left());
    assert_eq!(POOL.held(), held + past, "reserved from the pool at its actual size");
    assert_eq!(res.drawn() - drawn, past, "counted as the statement's draw");
    assert_eq!(counts()[Check::L1 as usize], l1, "a reserved draw is not a borrow");
    REGIONS.trim();
    let refusals = POOL.stats().refusals;
    let held = POOL.held();
    let pauses = PAUSED.load(Relaxed);
    let over = res.left() + POOL.capacity() - held + MIB;
    assert_eq!(unsafe { gravity_abi::program::arena_grow(ctx, over) }, 0, "past the capacity: refused");
    // D3239: no other statement is in flight, so no room can come back but this statement's own: refused at once, not after
    // the pool's 2 s wait (d3239_lone_draw_no_wait.rs: with another statement in flight it still waits)
    assert_eq!(PAUSED.load(Relaxed), pauses, "a lone statement is refused at once, with no pause");
    assert_eq!((POOL.held(), POOL.stats().refusals), (held, refusals + 1), "the refusal took nothing");
    // D2990: under a user's ceiling of half the big lane (D3147: no longer the default) the statement already holds its
    // reservation and a chunk past G, more than half of this pool: the next chunk past G is refused at once, with no wait and
    // nothing taken or counted
    let half = gravity_driver::POOL_FRACTION_WHOLE / 2;
    res.set_pool_fraction(half);
    assert!(res.reserved + res.grown() + MIB > POOL.statement_cap(half), "past the statement's part");
    assert_eq!(unsafe { gravity_abi::program::arena_grow(ctx, res.left() + MIB) }, 0, "past the statement's part: refused");
    assert_eq!(PAUSED.load(Relaxed), pauses, "refused at once, with no pause for the pool's wait");
    assert_eq!((POOL.held(), POOL.stats().refusals), (held, refusals + 1), "the part's refusal took nothing");
    entry.end(exec, false);
    drop(res);
    drop(entry);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    assert_eq!(POOL.held(), 0, "every chunk and reservation back at the statement's end");
    POOL.arm(0);
}
