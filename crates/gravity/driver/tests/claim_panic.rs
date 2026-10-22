//! D1408 (the robustness half of D1407's live abort): a claim whose host panics with a page pinned. The pool catches the
//! claim's unwind on its thread and fails the section; the claim's pins must be released there, on the thread that took
//! them, before the leader's detach: released by the leader, bufmgr's private refcount answered "buffer is not pinned", and
//! a Drop's second panic in that unwind aborted the process. A pin a claim still holds at the section's close fails it by name.

use gravity_driver_rig as common;

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;

use common::*;
use gravity_abi::status::{DONE, ERROR};
use gravity_abi::{Ctx, MorselRange};
use gravity_driver::{Claimed, Entry};
use gravity_program::{Arena, Code, End, Generation, Handle, Layout, OutCol, Pipeline, PlanKey, Program, SiteDesc, WritePlan};

const KEY: PlanKey = PlanKey { handle: Handle(28), generation: Generation(1) };
const NULLS: [u8; 1] = [0];
const WIDTH: u16 = 4;
const BLOCKS: u64 = 24;

/// When set, `p_panic_row`'s claim panics after its first row, the page still pinned (DONE at the first row keeps it, D227):
/// parked by the host as a JIT'd frame's host call parks it and resumed at `Code::call` (D1407's array_agg transfn panic).
static PANIC_ROW: AtomicBool = AtomicBool::new(true);

fn p_panic_row() -> Program {
    let st = Arena::new(1 << 14);
    let a = st.handle();
    let sites = [SiteDesc::range_source(a, 2001, &[1, 2])];
    let cols = [OutCol::new("c0", 20, -1, 8, true), OutCol::new("c1", 20, -1, 8, true)];
    let layout = Layout { nsites: 1, out_natts: 2, out_rows: 4, scratch_bytes: 0, exec_bytes: 0 };
    Program::with_pipelines(layout, &sites, &cols, WritePlan::SELECT, [(Pipeline::eligible(0, End::Sink, 1000), Code::hand(k_panic_row))])
}

/// # Safety — `ctx` is bound to `p_panic_row`.
unsafe extern "C-unwind" fn k_panic_row(ctx: *mut Ctx) -> i64 {
    let r = k_range_first_row(ctx);
    if r == DONE && PANIC_ROW.load(Ordering::Relaxed) {
        gravity_abi::status::unwind::park(Box::new("a host's panic in a claim (D1408)"));
        return ERROR;
    }
    r
}

#[test]
fn a_w4_claim_that_panics_with_a_pin_releases_it_on_its_own_thread_and_the_node_serves_again_1000_times() {
    let table = RangeTable::new(BLOCKS, 10);
    let mut derive = FakeDerive::ranges(table.clone());
    let program = Arc::new(p_panic_row());
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    for i in 0..1000 {
        for panics in [true, false] {
            PANIC_ROW.store(panics, Ordering::Relaxed);
            let mut sink = LegSink::new();
            let mut b = binding(&[0], &NULLS, &mut sink, &flag);
            let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
            let open = entry.open_section(&mut exec, &b, &mut derive, 0, WIDTH).unwrap();
            // each claim on its own thread, caught there as the pool's run_morsel catches it
            let claims: Vec<_> = std::thread::scope(|s| {
                let open = &open;
                let hs: Vec<_> = (0..WIDTH).map(|o| s.spawn(move || catch_unwind(AssertUnwindSafe(|| open.run_claim(o, MorselRange::new(o as u64 * 6, o as u64 * 6 + 6)))).map_err(|_| ()))).collect();
                hs.into_iter().map(|h| h.join().unwrap()).collect()
            });
            if panics {
                assert!(claims.iter().all(|c| c.is_err()), "iteration {i}: every claim panicked: {claims:?}");
                assert_eq!(table.pins.load(Ordering::Relaxed), 0, "iteration {i}: the unwind released every claim's pin before the leader saw the failure");
                assert_eq!(entry.close_section(open, &mut b, &NoMerge, false), Ok(()), "iteration {i}: a failed section closes without a pin left");
            } else {
                assert!(claims.iter().all(|c| *c == Ok(Ok(Claimed::Done))), "iteration {i}: the next execution serves: {claims:?}");
                entry.close_section(open, &mut b, &NoMerge, true).unwrap();
                assert_eq!(sink.rows.lock().unwrap().len(), WIDTH as usize, "iteration {i}: one row per worker");
            }
            entry.end(exec, !panics);
            assert_eq!(table.pins.load(Ordering::Relaxed), 0, "iteration {i}: no pin leaked");
            assert_eq!(table.foreign_unpins.load(Ordering::Relaxed), 0, "iteration {i}: every pin released on the thread that took it");
        }
    }
    PANIC_ROW.store(true, Ordering::Relaxed);
}

#[test]
fn a_pin_a_claim_leaves_behind_fails_the_section_by_name() {
    let table = RangeTable::new(BLOCKS, 10);
    let mut derive = FakeDerive::ranges(table.clone());
    let program = Arc::new(p_first_row(4));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    table.leak.store(true, Ordering::Relaxed);
    let mut sink = LegSink::new();
    let mut b = binding(&[0], &NULLS, &mut sink, &flag);
    let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    let open = entry.open_section(&mut exec, &b, &mut derive, 0, 2).unwrap();
    assert!(open.run_claim(0, MorselRange::new(0, 6)).is_err(), "the claim's settle sees its pin");
    let r = entry.close_section(open, &mut b, &NoMerge, false);
    assert!(matches!(&r, Err(e) if format!("{e:?}").contains("D1408")), "the section fails by name: {r:?}");
    entry.end(exec, false);
    table.leak.store(false, Ordering::Relaxed);
}
