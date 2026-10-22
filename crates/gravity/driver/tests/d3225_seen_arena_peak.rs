//! D3225: DR-12 notes the widest worker arena a run drew, and the program's next run reserves it for every worker whatever its
//! grant. The note was every map since the worker's bind, so a spilled join that reloads its partitions one at a time (each
//! region given back before the next is taken) noted the sum of its reloads: a W8 self-join noted 2 GiB a worker at a 123 MiB
//! grant, its next run reserved 8.4 GiB and ran 2x slower, and a later run's 8 x 1.8 GiB passed the pool (53200). The note is
//! now what the worker held at once.

use gravity_driver_rig as common;

use std::sync::atomic::{AtomicU64, AtomicU8};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use gravity_abi::MorselRange;
use gravity_driver::mem::grant::arena_seen;
use gravity_driver::mem::spill_host::v4;
use gravity_driver::{footprint, Claimed, Entry, Error, Grant, Open, Step, POOL, REGIONS};

const MIB: u64 = 1 << 20;
const NULLS: [u8; 1] = [0];
const BLOCKS: u64 = 24;
const PER_BLOCK: u64 = 10;
const WIDTH: u16 = 2;

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

/// Through worker `ordinal`'s context from another thread, `rounds` times: `held` spill regions of `bytes` taken, then given
/// back. False when a take is refused.
fn reloads(open: &Open<'_>, ordinal: u16, bytes: usize, held: usize, rounds: usize) -> bool {
    let ctx = open.worker_ctx(ordinal) as usize;
    std::thread::scope(|s| {
        s.spawn(move || unsafe {
            let ctx = ctx as *mut gravity_abi::Ctx;
            for _ in 0..rounds {
                let mut taken = Vec::with_capacity(held); // alloc: Cold (a test)
                for _ in 0..held {
                    let p = v4::take(ctx, bytes);
                    if p.is_null() {
                        return false;
                    }
                    taken.push(p);
                }
                for p in taken {
                    v4::give(ctx, p);
                }
            }
            true
        })
        .join()
        .unwrap()
    })
}

/// One run of `program` at `want` whose worker 0 reloads `rounds` x `held` regions of `bytes`; the pool armed with room past
/// W for them (D225: a worker's draw past its W is reserved at its size).
fn run(program: &Arc<gravity_program::Program>, want: Grant, bytes: usize, held: usize, rounds: usize) {
    let need = footprint(want, program, WIDTH);
    POOL.gate(0);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    assert_eq!(POOL.held(), 0, "a previous run left bytes held");
    POOL.arm(need + (held * bytes) as u64 + 64 * MIB);
    let res = std::rc::Rc::new(POOL.admit_need(want, &|g| footprint(g, program, WIDTH), Duration::from_millis(10)).unwrap());
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table);
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    {
        let mut sink = LegSink::new();
        let mut b = binding(&[8], &NULLS, &mut sink, &flag);
        let mut exec = entry.start(None, program, &mut derive, &mut b, res.clone()).unwrap();
        let open = entry.open_section(&mut exec, &b, &mut derive, 0, WIDTH).unwrap();
        assert!(reloads(&open, 0, bytes, held, rounds), "every reload taken");
        let cursor = AtomicU64::new(0);
        for o in 0..WIDTH {
            drain(&open, &cursor, o).unwrap();
        }
        entry.close_section(open, &mut b, &GroupMerge, true).unwrap();
        assert_eq!(entry.run(&mut exec, &mut b, gravity_driver::Call::batch(4)).unwrap(), Step::Done);
        entry.end(exec, true);
    }
    drop(res);
    drop(entry);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    assert_eq!(POOL.held(), 0, "every region, chunk and W released at the statement's end");
    POOL.arm(0);
}

#[test]
fn a_reload_given_back_is_not_noted_again_and_the_next_run_reserves_what_a_worker_held_at_once() {
    let program = Arc::new(p_group_share(4));
    let want = Grant::of(64 * MIB);
    gravity_driver::forget_arena_draws();
    let prior = footprint(want, &program, WIDTH);
    // 16 reloads of 8 MiB, one held at a time: 128 MiB of maps, 8 MiB held at once
    run(&program, want, (8 * MIB) as usize, 1, 16);
    let (_, worker) = arena_seen(program.key).expect("the run noted its draws");
    assert!((8 * MIB..16 * MIB).contains(&worker), "the worker held one 8 MiB region at once, not the 128 MiB of maps: noted {worker}");
    let once = footprint(want, &program, WIDTH);
    assert!(once <= prior + WIDTH as u64 * 16 * MIB, "the next run reserves what a worker held at once: {prior} -> {once}");
    // the same run again: the note does not grow
    run(&program, want, (8 * MIB) as usize, 1, 16);
    assert_eq!(footprint(want, &program, WIDTH), once, "a run that held as much notes nothing more");
    // two held at once: the note follows what was held
    run(&program, want, (8 * MIB) as usize, 2, 4);
    let (_, worker) = arena_seen(program.key).unwrap();
    assert!((16 * MIB..24 * MIB).contains(&worker), "two 8 MiB regions held at once: noted {worker}");
    gravity_driver::forget_arena_draws();
}
