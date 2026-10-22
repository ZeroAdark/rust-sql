//! CONVERT wave 2 (wave2-base commit 7): the spill host's OWN-slot arena owner (`gravity_abi::program::arena_own_owner_kind`,
//! the 0-byte `SPILL_ARENA_CHUNK` ask with `ARENA_ASK_OWN` in word 1). It answers the slot the pipeline produces into,
//! else the first slot it consumes, with that slot's breaker kind: `Inner::op_owner`, the owner the pipeline's grant-road
//! blocks charge. A count's emit (a sink-ending pipeline: it consumes the PlainAgg slot and produces none) answers the
//! PlainAgg slot's owner with Kind::PlainAgg, where the produced-else-statement ask (`arena_slot_owner_kind`, unchanged)
//! answers the statement with Kind::Region. A scan feeding the slot answers the slot both ways. A sink scan (no slot of its
//! own: it neither produces nor consumes one) answers no own owner. One test: it binds one backend.

use gravity_driver_rig as common;

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use common::*;
use gravity_abi::Ctx;
use gravity_driver::{Entry, Grant};
use gravity_program::{Code, Program};
use memledger::{Kind, Owner, OwnerClass};

const NULLS: [u8; 1] = [0];
const PROCNO: u32 = 4078; // backend slot 32 + 4078 < ACCOUNT_SLOTS (4112): a bound account
const BLOCKS: u64 = 12;
const PER_BLOCK: u64 = 10;

type Ask = Option<([u64; 2], u64)>;

/// Each kernel call's asks: (0 the count's scan, 1 its emit, 2 a sink scan; the own-slot answer, the produced-else-statement one).
static ASKS: Mutex<Vec<(u8, Ask, Ask)>> = Mutex::new(Vec::new());

/// # Safety — `ctx` is bound for the call.
unsafe fn ask(ctx: *mut Ctx, who: u8) {
    // SAFETY: the driver binds `ctx` for the call
    let (own, slot) = unsafe { (gravity_abi::program::arena_own_owner_kind(ctx), gravity_abi::program::arena_slot_owner_kind(ctx)) };
    ASKS.lock().unwrap().push((who, own, slot));
}

unsafe extern "C-unwind" fn k_scan_asks(ctx: *mut Ctx) -> i64 {
    // SAFETY: as `ask`; the program is `p_count_with`'s
    unsafe {
        ask(ctx, 0);
        k_range_count(ctx)
    }
}

unsafe extern "C-unwind" fn k_emit_asks(ctx: *mut Ctx) -> i64 {
    // SAFETY: as above
    unsafe {
        ask(ctx, 1);
        k_emit_count(ctx)
    }
}

unsafe extern "C-unwind" fn k_rows_asks(ctx: *mut Ctx) -> i64 {
    // SAFETY: as above; the program is `p_rows_with`'s
    unsafe {
        ask(ctx, 2);
        k_range_rows(ctx)
    }
}

/// `program` run to its end, serially; its sink's rows.
fn execute(entry: &mut Entry, program: &Arc<Program>, derive: &mut FakeDerive) -> Vec<Vec<u64>> {
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[1000], &NULLS, &mut sink, &flag);
    let mut exec = entry.start(None, program, derive, &mut b, lease(Grant::of(8 << 20))).expect("started");
    let r = entry.run_to_end(&mut exec, &mut b);
    entry.end(exec, r.is_ok());
    r.expect("the program runs");
    drop(b);
    sink.sorted()
}

#[test]
fn wave2_the_own_slot_ask_answers_an_emits_consumed_slot_owner_and_kind_and_none_without_an_own_slot() {
    let (stmt, count, rows) = std::thread::spawn(|| {
        memledger::bind_backend(PROCNO, 9000 + PROCNO);
        memledger::statement_begin();
        let stmt = memledger::charge::context().0;
        assert!(matches!(stmt, Owner::Statement { .. }), "the rig's backend is bound and in a statement: {stmt:?}");
        let table = RangeTable::new(BLOCKS, PER_BLOCK);
        table.record_owners.store(true, Ordering::Relaxed);
        let mut derive = FakeDerive::ranges(table.clone());
        let mut entry = Entry::new(2);
        let count_program = Arc::new(p_count_with(Code::hand(k_scan_asks), Code::hand(k_emit_asks), 4));
        let rows = execute(&mut entry, &count_program, &mut derive);
        let count = (rows, core::mem::take(&mut *table.owners.lock().unwrap()), core::mem::take(&mut *ASKS.lock().unwrap()));
        let rows_program = Arc::new(p_rows_with(Code::hand(k_rows_asks), 64));
        let n = execute(&mut entry, &rows_program, &mut derive).len();
        let rows = (n, core::mem::take(&mut *ASKS.lock().unwrap()));
        let _ = memledger::statement_end();
        drop(entry);
        memledger::unbind_backend();
        (stmt, count, rows)
    })
    .join()
    .unwrap();

    let (out, seen, asks) = count;
    let total: u64 = (0..BLOCKS * PER_BLOCK).sum();
    assert_eq!(out, vec![vec![BLOCKS * PER_BLOCK, total]], "the count, as PG's");
    let op = seen.first().and_then(|s| s.op).expect("a count's scan feeds its PlainAgg slot").0;
    assert_eq!(Owner::from_words(op).map(Owner::class), Some(OwnerClass::Op), "the slot's operator owner");
    let slot = Some((op, Kind::PlainAgg as u64));
    let scan: Vec<_> = asks.iter().filter(|a| a.0 == 0).collect();
    let emit: Vec<_> = asks.iter().filter(|a| a.0 == 1).collect();
    assert!(!scan.is_empty() && scan.iter().all(|a| a.1 == slot && a.2 == slot), "the scan: its produced slot both ways: {asks:?}");
    assert!(!emit.is_empty() && emit.iter().all(|a| a.1 == slot), "the emit (it consumes the slot, produces none): the own-slot ask answers the slot's owner and its breaker's kind: {asks:?}");
    assert!(emit.iter().all(|a| a.2 == Some((stmt.words(), Kind::Region as u64))), "the emit's produced-else-statement ask is unchanged: the statement, Kind::Region: {asks:?}");

    let (n, asks) = rows;
    assert_eq!(n as u64, BLOCKS * PER_BLOCK, "a sink scan: every row");
    assert!(!asks.is_empty() && asks.iter().all(|a| a.0 == 2 && a.1.is_none()), "a sink scan has no slot of its own: no own-slot owner: {asks:?}");
    assert!(asks.iter().all(|a| a.2 == Some((stmt.words(), Kind::Region as u64))), "a sink scan's produced-else-statement ask: the statement: {asks:?}");
}
