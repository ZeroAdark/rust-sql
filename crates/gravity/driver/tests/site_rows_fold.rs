//! KEYED-22: `close_section` sums each worker ctx's site rows (`rows_in` / `rows_out`) into the node ctx's, so the node
//! counts the execution's rows at any width (exec `observe`'s rows folded, EXPLAIN's rows out); `Node::apply` starts each
//! execution, a parked node's included, at none.

use gravity_driver_rig as common;

use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use common::*;
use gravity_abi::MorselRange;
use gravity_driver::{Claimed, Entry};
use gravity_program::{Generation, Handle, PlanKey};

const KEY: PlanKey = PlanKey { handle: Handle(29), generation: Generation(1) };
const NULLS: [u8; 1] = [0];

#[test]
fn a_section_folds_its_workers_site_rows_into_the_node() {
    let table = RangeTable::new(24, 10);
    let mut derive = FakeDerive::ranges(table);
    let program = Arc::new(p_first_row(4));
    let d = program.pipelines[0].driving_site as usize;
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    for round in 0..2u64 {
        let mut sink = LegSink::new();
        let mut b = binding(&[0], &NULLS, &mut sink, &flag);
        let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
        assert_eq!(exec.node().site_rows_out(d), 0, "round {round}: an execution starts at no rows (the parked node's zeroed)");
        let open = entry.open_section(&mut exec, &b, &mut derive, 0, 4).expect("the workers open clean");
        let claims: Vec<_> = std::thread::scope(|s| {
            let open = &open;
            let hs: Vec<_> = (0..4u16).map(|o| s.spawn(move || open.run_claim(o, MorselRange::new(o as u64 * 6, o as u64 * 6 + 6)))).collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(claims.iter().all(|c| *c == Ok(Claimed::Done)), "{claims:?}");
        // each worker's driving site counts its own rows out (the lowered write_back's), as its claims leave them
        for o in 0..4u16 {
            unsafe { (*gravity_abi::ctx::site_ptr(open.worker_ctx(o), d)).rows_out = 10 + o as u64 + round };
        }
        entry.close_section(open, &mut b, &NoMerge, true).unwrap();
        assert_eq!(exec.node().site_rows_out(d), 46 + 4 * round, "round {round}: the node counts every worker's rows");
        entry.end(exec, true);
    }
}
