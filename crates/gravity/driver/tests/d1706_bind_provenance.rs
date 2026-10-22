//! D1706 (found by D1702's Miri run): the pointers a node keeps to its own `Inner` and to the statement's binding lost their
//! access under Tree Borrows before their use. (a) The bind handed out reborrows of the `ArenaBox` (the program block's and
//! each site's `env`, which every host reaches the node through, and the ctx's `program` / `out`): siblings of every later
//! deref of `inner` in the Node's own methods, so a Node write after the bind (`open_pipeline` points `home` and the
//! partials at the pipeline's regions before its first call) disabled them, and the call's settle (`Inner::settle_call`
//! over `env`, D1744) read through a disabled pointer. (b) `Node::apply` kept the binding's pointer from `Entry::start`'s
//! borrow, which the caller's next `&mut` borrow of the binding (`Entry::run`) froze, and the settle's `add_rows` wrote
//! through it. Plain `cargo test` passes either way; the defect shows under
//! `MIRIFLAGS=-Zmiri-tree-borrows cargo +nightly miri test -p gravity_driver --test d1706_bind_provenance`.
use gravity_driver_rig as common;

use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use common::*;
use gravity_abi::status::{DONE, ERROR};
use gravity_abi::Ctx;
use gravity_driver::{Call, Entry, Step};
use gravity_program::{Code, End, Generation, Handle, Layout, OutCol, Pipeline, PlanKey, Program, Reason, WritePlan};

const KEY: PlanKey = PlanKey { handle: Handle(1706), generation: Generation(1) };
const NULLS: [u8; 1] = [0];

/// One row, 7, then done: the host's send reaches the node through the ctx's `out` and the program block's `env`.
unsafe extern "C-unwind" fn k_one(ctx: *mut Ctx) -> i64 {
    let o = (*ctx).out;
    *(*o).values = 7;
    *(*o).isnull = 0;
    (*o).count = 1;
    if gravity_abi::program::push(ctx) < 0 {
        return ERROR;
    }
    DONE
}

#[test]
fn a_pipelines_first_call_settles_through_the_env_the_bind_handed_out() {
    let layout = Layout { nsites: 0, out_natts: 1, out_rows: 8, scratch_bytes: 0, exec_bytes: 0 };
    let pipes = [(Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(k_one))];
    let program = Arc::new(Program::with_pipelines(layout, &[], &[OutCol::new("c0", 20, -1, 8, true)], WritePlan::SELECT, pipes));
    let mut entry = Entry::new(2);
    let mut derive = FakeDerive::new([no_rows, no_rows, no_rows, no_rows]);
    let sink = FakeSink::new();
    let flag = AtomicU8::new(0);
    let mut sink_ref = &sink;
    let mut b = binding(&[0], &NULLS, &mut sink_ref, &flag);
    for _ in 0..2 {
        let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(1 << 20))).unwrap();
        let r = entry.run(&mut exec, &mut b, Call::batch(8));
        entry.end(exec, true);
        assert_eq!(r, Ok(Step::Done), "the pipeline runs to its end");
    }
    assert_eq!((sink.rows(), sink.last()[0]), (2, 7), "one row per statement, the parked node's second bind too");
}
