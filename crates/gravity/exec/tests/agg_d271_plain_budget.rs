//! D271: a plain aggregate's breaker budget is the one-record table abi's record host lays out and `PlainAggPartial`
//! reads: the header and the record. Under `Budget::Fixed(layout.size())` a worker's partial of a record smaller than the
//! header (`[Count, SumI64]` + validity, 24 B) was `state_bytes` = 24 long, `partial_shape` saw no table and dropped it:
//! `count(*), sum(x)` answered 0 / NULL at W >= 2 (m1_fold, m1_frame). The fixture is decompose's own plan capture.
use std::path::Path;

use gravity_abi::breaker::{BreakerKind, Budget, HashAgg, HASHAGG_HDR};
use gravity_decompose::{corpus, decompose_in};
use gravity_driver::{BorrowedBatch, Flow, Merge, Partial, Sink, SinkRef, State};
use gravity_parallel::PlainAggPartial;
use gravity_program::Arena;
use mcx::MemoryContext;

struct Discard;

impl Sink for Discard {
    fn push(&mut self, _batch: BorrowedBatch<'_>) -> gravity_driver::Result<Flow> {
        Ok(Flow::Continue)
    }
}

/// A region the driver would hand out: page-backed, `n` bytes of it the breaker's (the slice a `State` / `Partial` sees).
fn page() -> Vec<u64> {
    vec![0u64; 512]
}

fn slice(v: &mut [u64], n: u32) -> &mut [u8] {
    assert!(n as usize <= v.len() * 8);
    // SAFETY: `v` is `v.len() * 8` bytes, `n` of them.
    unsafe { core::slice::from_raw_parts_mut(v.as_mut_ptr() as *mut u8, n as usize) }
}

#[test]
fn a_plain_partial_smaller_than_the_header_survives_its_budget() {
    let path = manifest_dir().join("../decompose/tests/plans/bench-040.plan");
    let ctx = MemoryContext::new("d271");
    let fx = corpus::load(ctx.mcx(), &std::fs::read_to_string(&path).unwrap()).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let arena = Arena::new(1 << 20);
    let g = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).unwrap_or_else(|r| panic!("bench-040 refused: {r:?}"));
    let b = g.breakers.iter().find(|b| b.breaker.kind == BreakerKind::PlainAgg).expect("bench-040's plain aggregate");
    let layout = b.layout.record(arena.handle());
    assert!((layout.size() as usize) < HASHAGG_HDR, "the fixture's record ({} B) is smaller than a table header", layout.size());
    let Budget::Fixed(n) = b.breaker.budget else { panic!("a plain breaker is fixed-budget: {:?}", b.breaker.budget) };
    assert_eq!(b.breaker.bytes, n, "the leader's region and a worker's are the same table");
    // slot 0 is count(*), slot 1 sum(b) (the golden's `[Count@0,SumI64@8]+valid`)
    let rows = [(3i64, 40i64), (4, 2)];
    let mut parts: Vec<Vec<u64>> = rows
        .iter()
        .map(|&(n_rows, v)| {
            let mut p = page();
            let r = unsafe { HashAgg::at(slice(&mut p, n).as_mut_ptr(), &layout, 1, n) }.single();
            r.open(&[]);
            r.add_count(0, n_rows);
            r.add_i64(1, v);
            p
        })
        .collect();
    let mut all = page();
    let reference = unsafe { HashAgg::at(slice(&mut all, n).as_mut_ptr(), &layout, 1, n) }.single();
    reference.open(&[]);
    for &(n_rows, v) in &rows {
        reference.add_count(0, n_rows);
        reference.add_i64(1, v);
    }
    let mut leader = page();
    unsafe { HashAgg::at(slice(&mut leader, n).as_mut_ptr(), &layout, 1, n) }.single().open(&[]);
    let mut sink = Discard;
    for (i, p) in parts.iter_mut().enumerate() {
        let bytes: &[u8] = slice(p, n);
        let lb = slice(&mut leader, n);
        PlainAggPartial.merge_into(&mut State::new(lb, SinkRef::of(&mut sink)), Partial { ordinal: i as u16, bytes, rows_processed: 0 }).unwrap();
    }
    let got = unsafe { HashAgg::view(slice(&mut leader, n).as_ptr()) }.record(0);
    assert_eq!((got.count(0), got.i64(1), got.valid(0), got.valid(1)), (7, 42, true, true), "both workers' partials fold into the leader");
    assert_eq!(got.bytes(), reference.bytes(), "the folded record is the record that saw every row");
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
