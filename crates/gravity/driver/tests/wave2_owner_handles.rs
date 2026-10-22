//! CONVERT wave 2 (R2, R3): the owner handles a source and a kernel's arena read. A scan's window answers the owner and
//! kind of the slot its pipeline PRODUCES into (`Window::op_owner`: the count's PlainAgg slot owner), on the leader and on
//! every worker; a sink pipeline's window answers none (its bytes are the statement's). Every window answers the
//! statement's owner the Node captured once at its bind (`Window::stmt_owner`, `Node::stmt_owner`), a worker's the
//! leader's. The spill host's arena owner is the produced slot's with its breaker's kind, else the statement's with
//! `Kind::Region` (`arena_slot_owner_kind`, the reply's word 0): the count's emit (it consumes the PlainAgg slot and produces
//! none) answers the statement, never the consumed slot's owner. Outside a statement (a thread that began none) the
//! statement handles answer none. One test: it binds one backend.

use gravity_driver_rig as common;

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use common::*;
use gravity_abi::{Ctx, MorselRange};
use gravity_driver::{Call, Claimed, Entry, Error, Grant, Open, Step};
use gravity_program::{Code, Program};
use memledger::{Kind, Owner, OwnerClass, Release};

const NULLS: [u8; 1] = [0];
const PROCNO: u32 = 4079;
const BLOCKS: u64 = 24;
const PER_BLOCK: u64 = 10;
const WIDTH: u16 = 3;

/// The spill host's arena owner each kernel call asked for: (0 the scan, 1 the emit; the answer's owner words and kind).
static ASKS: Mutex<Vec<(u8, Option<([u64; 2], u64)>)>> = Mutex::new(Vec::new());

unsafe extern "C-unwind" fn k_scan_asks(ctx: *mut Ctx) -> i64 {
    // SAFETY: the driver binds `ctx` for the call
    let o = unsafe { gravity_abi::program::arena_slot_owner_kind(ctx) };
    ASKS.lock().unwrap().push((0, o));
    // SAFETY: as above; the program is `p_count_with`'s
    unsafe { k_range_count(ctx) }
}

unsafe extern "C-unwind" fn k_emit_asks(ctx: *mut Ctx) -> i64 {
    // SAFETY: the driver binds `ctx` for the call
    let o = unsafe { gravity_abi::program::arena_slot_owner_kind(ctx) };
    ASKS.lock().unwrap().push((1, o));
    // SAFETY: as above
    unsafe { k_emit_count(ctx) }
}

fn take<T>(m: &Mutex<Vec<T>>) -> Vec<T> {
    core::mem::take(&mut *m.lock().unwrap())
}

fn drain(open: &Open<'_>, cursor: &AtomicU64, ordinal: u16) -> Result<(), Error> {
    loop {
        let lo = cursor.fetch_add(5, Ordering::AcqRel);
        if lo >= BLOCKS {
            return Ok(());
        }
        if let Claimed::Stopped = open.run_claim(ordinal, MorselRange::new(lo, (lo + 5).min(BLOCKS)))? {
            return Ok(());
        }
    }
}

/// One execution of `program`: serial (`run_to_end`), or its first pipeline as a section of `WIDTH` workers then the rest.
/// The sink's rows and the Node's statement owner at its start.
fn execute(entry: &mut Entry, program: &Arc<Program>, derive: &mut FakeDerive, width: u16) -> (Vec<Vec<u64>>, Option<(Owner, Release)>) {
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[1000], &NULLS, &mut sink, &flag);
    let mut exec = entry.start(None, program, derive, &mut b, lease(Grant::of(8 << 20))).expect("started");
    let node_stmt = exec.node().stmt_owner();
    if width > 1 {
        let open = entry.open_section(&mut exec, &b, derive, 0, width).expect("the section opens");
        let cursor = AtomicU64::new(0);
        for o in 0..width {
            drain(&open, &cursor, o).expect("the claims run");
        }
        entry.close_section(open, &mut b, &CountMerge, true).expect("the section closes");
        assert_eq!(entry.run(&mut exec, &mut b, Call::batch(4)).expect("the emit runs"), Step::Done);
        entry.end(exec, true);
    } else {
        let r = entry.run_to_end(&mut exec, &mut b);
        entry.end(exec, r.is_ok());
        r.expect("the program runs");
    }
    drop(b);
    (sink.sorted(), node_stmt)
}

/// The produced slot's owner words every window saw (one owner, of class Op and kind PlainAgg), and whether a worker's
/// window was among them.
fn one_slot_owner(seen: &[SeenOwners], stmt: Owner, what: &str) -> ([u64; 2], bool) {
    assert!(!seen.is_empty(), "{what}: the scan ran windows");
    let op = seen[0].op.expect("a count's scan feeds its PlainAgg slot").0;
    assert_eq!(Owner::from_words(op).map(Owner::class), Some(OwnerClass::Op), "{what}: the slot's operator owner");
    for s in seen {
        assert_eq!(s.op, Some((op, Kind::PlainAgg as u8)), "{what}: every window, the produced slot's owner and its breaker's kind: {seen:?}");
        assert_eq!(s.stmt, Some((stmt.words(), Release::Statement as u8)), "{what}: every window, the statement's owner: {seen:?}");
    }
    (op, seen.iter().any(|s| s.worker))
}

#[test]
fn wave2_a_scan_window_answers_its_produced_slots_owner_and_the_statement_and_a_sinks_emit_arena_the_statement() {
    let (serial, wide, sink_in, sink_out) = std::thread::spawn(|| {
        memledger::bind_backend(PROCNO, 9000 + PROCNO);
        memledger::statement_begin();
        let stmt = memledger::charge::context().0;
        let table = RangeTable::new(BLOCKS, PER_BLOCK);
        table.record_owners.store(true, Ordering::Relaxed);
        let mut derive = FakeDerive::ranges(table.clone());
        let mut entry = Entry::new(2);
        let count = Arc::new(p_count_with(Code::hand(k_scan_asks), Code::hand(k_emit_asks), 4));
        let (rows, node_stmt) = execute(&mut entry, &count, &mut derive, 1);
        let serial = (rows, node_stmt, take(&table.owners), take(&ASKS));
        let (rows, node_stmt) = execute(&mut entry, &count, &mut derive, WIDTH);
        let wide = (rows, node_stmt, take(&table.owners), take(&ASKS));
        let rows_program = Arc::new(p_rows(64));
        let (rows, node_stmt) = execute(&mut entry, &rows_program, &mut derive, 1);
        let sink_in = (rows.len(), node_stmt, take(&table.owners));
        let _ = memledger::statement_end();
        let (rows, node_stmt) = execute(&mut entry, &rows_program, &mut derive, 1);
        let sink_out = (rows.len(), node_stmt, take(&table.owners));
        drop(entry);
        memledger::unbind_backend();
        ((stmt, serial), wide, sink_in, sink_out)
    })
    .join()
    .unwrap();
    let (stmt, (rows, node_stmt, seen, asks)) = serial;
    let total: u64 = (0..BLOCKS * PER_BLOCK).sum();
    assert_eq!(rows, vec![vec![BLOCKS * PER_BLOCK, total]], "serial: the count, as PG's");
    assert_eq!(node_stmt, Some((stmt, Release::Statement)), "serial: the Node took the statement's owner at its bind");
    let (op, workers) = one_slot_owner(&seen, stmt, "serial");
    assert!(!workers, "serial: the leader's windows only");
    let scan: Vec<_> = asks.iter().filter(|a| a.0 == 0).map(|a| a.1).collect();
    let emit: Vec<_> = asks.iter().filter(|a| a.0 == 1).map(|a| a.1).collect();
    assert!(!scan.is_empty() && scan.iter().all(|&a| a == Some((op, Kind::PlainAgg as u64))), "serial: the scan's arena owner is its produced slot's, its kind: {asks:?}");
    assert!(!emit.is_empty() && emit.iter().all(|&a| a == Some((stmt.words(), Kind::Region as u64))), "serial: the emit's (it consumes the slot, produces none) is the statement's, Kind::Region, not the slot's: {asks:?}");

    let (rows, node_stmt, seen, asks) = wide;
    assert_eq!(rows, vec![vec![BLOCKS * PER_BLOCK, total]], "W{WIDTH}: the count, as PG's");
    assert_eq!(node_stmt, Some((stmt, Release::Statement)), "W{WIDTH}: the Node's statement owner");
    let (op, workers) = one_slot_owner(&seen, stmt, "workers");
    assert!(workers, "W{WIDTH}: the workers' windows answered: {seen:?}");
    assert!(asks.iter().any(|a| a.0 == 0) && asks.iter().filter(|a| a.0 == 0).all(|a| a.1 == Some((op, Kind::PlainAgg as u64))), "W{WIDTH}: each worker's scan arena owner is the produced slot's: {asks:?}");
    assert!(asks.iter().any(|a| a.0 == 1) && asks.iter().filter(|a| a.0 == 1).all(|a| a.1 == Some((stmt.words(), Kind::Region as u64))), "W{WIDTH}: the emit's is the statement's: {asks:?}");

    let (n, node_stmt, seen) = sink_in;
    assert_eq!(n as u64, BLOCKS * PER_BLOCK, "a sink scan: every row");
    assert_eq!(node_stmt, Some((stmt, Release::Statement)), "a sink scan's Node: the statement's owner");
    assert!(!seen.is_empty() && seen.iter().all(|s| s.op.is_none() && s.stmt == Some((stmt.words(), Release::Statement as u8))), "a sink pipeline produces no slot: no slot owner, the statement's: {seen:?}");

    let (n, node_stmt, seen) = sink_out;
    assert_eq!(n as u64, BLOCKS * PER_BLOCK, "outside a statement: every row");
    assert_eq!(node_stmt, None, "outside a statement the Node captured no statement owner");
    assert!(!seen.is_empty() && seen.iter().all(|s| s.op.is_none() && s.stmt.is_none()), "outside a statement: no owner at all: {seen:?}");
}
