//! LEDGER2 step 5: a breaker slot's ledger owner. The leader begins one operator owner per breaker slot at the slot's open
//! (its breaker's kind, its share as budget, under the statement or the portal it executes), and the slot's region is
//! charged to it there, so the owner tree shows the breaker holding its region under the statement. It ends at the slot's
//! free or the statement's end, its bytes gone with the region: no operator residue at the statement's end. One test: it
//! arms the process-wide pool.

use gravity_driver_rig as common;

use std::sync::atomic::AtomicU8;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use gravity_driver::{footprint, BorrowedBatch, Entry, Flow, Grant, Result, Sink, POOL, REGIONS};
use memledger::charge::{for_each_owner, usage, OwnerView, Usage};
use memledger::owner_end::{totals, Source};
use memledger::{Acct, Owner, OwnerClass};

const NULLS: [u8; 1] = [0];
const PROCNO: u32 = 4082;

/// The owners of account `a` named `name`: (cell, parent cell, label, held).
fn owners(a: Acct, name: &str) -> Vec<(u32, u32, u32, i64)> {
    let mut v = Vec::new();
    for_each_owner(|o: &OwnerView| {
        if o.acct == a && o.name == name {
            v.push((o.cell, o.parent, o.label, o.held()));
        }
    });
    v
}

/// A sink that reads the account's operator owners and its statement's numbers as the rows arrive (the count's emit
/// reads its live slot).
struct Watch {
    acct: Acct,
    stmt: Owner,
    rows: Vec<Vec<u64>>,
    ops: Vec<(u32, u32, u32, i64)>,
    usage: Option<Usage>,
}

impl Sink for Watch {
    fn push(&mut self, batch: BorrowedBatch<'_>) -> Result<Flow> {
        for (v, _) in batch.iter() {
            self.rows.push(v.to_vec());
        }
        self.ops.extend(owners(self.acct, "operator"));
        self.usage = Some(usage(self.stmt));
        Ok(Flow::Continue)
    }
}

#[test]
fn ledger2_a_breakers_region_is_held_by_its_slot_owner_under_the_statement_and_leaves_with_it() {
    POOL.gate(0);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    let program = Arc::new(p_count(4));
    let grant = Grant::of(1 << 20);
    POOL.arm(footprint(grant, &program, 1) + (64 << 20));
    let (rows, ops, seen, after, residues) = std::thread::spawn(move || {
        memledger::bind_backend(PROCNO, 9000 + PROCNO);
        memledger::statement_begin();
        let a = Acct::backend(PROCNO);
        let stmt = memledger::charge::context().0;
        let res = std::rc::Rc::new(POOL.admit_need(grant, &|g| footprint(g, &program, 1), Duration::from_millis(100)).expect("admitted"));
        let table = RangeTable::new(24, 10);
        let mut derive = FakeDerive::ranges(table);
        let mut entry = Entry::new(2);
        let flag = AtomicU8::new(0);
        let mut sink = Watch { acct: a, stmt, rows: Vec::new(), ops: Vec::new(), usage: None };
        let mut b = binding(&[100], &NULLS, &mut sink, &flag);
        let mut exec = entry.start(None, &program, &mut derive, &mut b, res.clone()).expect("started");
        let r = entry.run_to_end(&mut exec, &mut b);
        entry.end(exec, r.is_ok());
        r.expect("the count runs");
        drop(b);
        drop(res);
        let after = owners(a, "operator");
        let e0 = totals(Source::Owner, OwnerClass::Op).events;
        let _ = memledger::statement_end();
        let residues = totals(Source::Owner, OwnerClass::Op).events - e0;
        memledger::unbind_backend();
        (sink.rows, sink.ops, sink.usage, after, residues)
    })
    .join()
    .unwrap();
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    POOL.arm(0);
    assert_eq!(rows, vec![vec![100, (0..100).sum::<u64>()]], "the count, as PG's");
    let agg: Vec<_> = ops.iter().filter(|o| o.2 & 0xff == memledger::Kind::PlainAgg as u32).collect();
    assert!(!agg.is_empty(), "the count's breaker has an operator owner while its emit reads it: {ops:?}");
    assert!(agg.iter().all(|o| o.3 > 0), "holding its region: {agg:?}");
    let u = seen.expect("the statement read as the rows arrived");
    let held: i64 = ops.iter().map(|o| o.3).sum();
    assert!(u.subtree - u.held >= held, "under the statement: its subtree counts them ({u:?}, operators {held})");
    assert!(after.is_empty(), "the slot owners ended with the statement's regions settled: {after:?}");
    assert_eq!(residues, 0, "no operator residue at the statement's end");
}
