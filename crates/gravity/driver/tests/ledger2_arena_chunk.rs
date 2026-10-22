//! LEDGER2 step 3: an operator arena's chunk through the spill host (`SPILL_ARENA_CHUNK`): drawn as a work block is (the
//! grant pool's region), its charge moved to the arena's owner once, at the draw, and off it again when the chunk is given
//! back (`SPILL_ARENA_GIVE`); the statement's end holds no chunk.

use gravity_driver_rig as common;

use std::sync::atomic::AtomicU8;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use gravity_abi::status::{unsupported, UNSUPPORTED_SCRATCH};
use gravity_driver::{Entry, Grant, POOL, REGIONS};
use gravity_program::Code;
use memledger::charge::{op_begin, op_end, usage};

const MIB: u64 = 1 << 20;
const NULLS: [u8; 1] = [0];

/// What the kernel saw: the chunk's address, its owner's held bytes after the draw and after the give.
static SEEN: Mutex<Vec<(usize, i64, i64)>> = Mutex::new(Vec::new());

unsafe extern "C-unwind" fn k_chunk(ctx: *mut gravity_abi::Ctx) -> i64 {
    let (parent, _) = memledger::charge::context();
    let o = op_begin(parent, memledger::Kind::Sort as u32, 0);
    // SAFETY: the driver binds `ctx` for the call
    let p = unsafe { gravity_abi::program::arena_chunk(ctx, MIB, o.words()) }.unwrap_or(core::ptr::null_mut());
    let drawn = usage(o).held;
    if !p.is_null() {
        // SAFETY: the chunk just drawn, read by nothing after
        unsafe { gravity_abi::program::arena_give(ctx, p) };
    }
    let given = usage(o).held;
    op_end(o);
    SEEN.lock().unwrap().push((p as usize, drawn, given));
    unsupported(UNSUPPORTED_SCRATCH)
}

#[test]
fn ledger2_an_arena_chunk_is_its_owners_once_and_leaves_with_its_give() {
    POOL.gate(0);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    POOL.arm(64 * MIB);
    let flag = AtomicU8::new(0);
    let sink = FakeSink::new();
    let mut sink_ref = &sink;
    let mut b = binding(&[], &NULLS, &mut sink_ref, &flag);
    let mut derive = FakeDerive::new([no_rows; 4]);
    let mut entry = Entry::new(2);
    let program = Arc::new(p_code(Code::hand(k_chunk), 1, 0));
    let a = std::rc::Rc::new(POOL.admit(Grant::of(8 * MIB), 1, 1, Duration::from_millis(10)).unwrap());
    let mut exec = entry.start(None, &program, &mut derive, &mut b, a.clone()).unwrap();
    let _ = entry.run(&mut exec, &mut b, gravity_driver::Call::batch(1));
    entry.end(exec, false);
    drop(a);
    drop(entry);
    let seen = SEEN.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "the kernel ran once: {seen:?}");
    let (p, drawn, given) = seen[0];
    assert!(p != 0, "the chunk was served");
    assert!(drawn >= MIB as i64 && drawn < (MIB + MIB / 8) as i64, "the chunk's bytes on the arena's owner, once: {drawn}");
    assert_eq!(given, 0, "and off it with the give");
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    assert_eq!(POOL.held(), 0, "every byte back at the statement's end");
    POOL.arm(0);
}
