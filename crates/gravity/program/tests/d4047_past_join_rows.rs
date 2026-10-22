//! D4047: every breaker that is not a hash build reads the planner's rows at its input (`Pipeline::end_rows`) where they are
//! fewer than its driving scan's (`predicted_rows`, past a narrowing join), never more: an aggregate's held records and a
//! Sort's, a row top's, a Materialize's or a TopK's table. TPC-H q18 at SF10: lineitem's 59986052 rows at the scan,
//! 21350527 at the join under its GROUP BY (D4043). Byte for byte as before: a fanning join (end_rows past
//! predicted_rows), a serial pipeline (no predicted_rows: D3205's setting decides), a drain's named rows (D3355) and a
//! hash build. A program another statement takes is sized by that statement's rows at the join (`bind_estimates`).

use gravity_abi::breaker::{Breaker, BreakerKind, Budget, KeyKind, RecordLayout, SlotKind, Spill};
use gravity_program::cost::breaker::{of, of_at, rows_bytes};
use gravity_program::cost::{HELD_TABLES, PARTITION_BYTES};
use gravity_program::{bind_estimates, End, OwnerDesc, PipeEst, Pipeline};

const SCAN: u64 = 59_986_052;
const JOIN: u64 = 21_350_527;

fn past_join(kind: BreakerKind, spill: Spill, end_rows: u64) -> Pipeline {
    let layout = RecordLayout::new(&[KeyKind::I64], &[SlotKind::Count]);
    let b = Breaker { spill, budget: Budget::Share, ..Breaker::sized(kind, 0) };
    Pipeline { end_rows, row_width: 24, owner: Some(OwnerDesc::generic(layout, 0, 0)), ..Pipeline::eligible(0, End::Breaker(b), SCAN) }
}

#[test]
fn a_partitioned_aggregate_past_a_join_holds_the_joins_rows() {
    for kind in [BreakerKind::HashAgg, BreakerKind::Distinct] {
        let c = of(&past_join(kind, Spill::Partitioned { bits: 10 }, JOIN)).expect("a share breaker");
        let held = |rows: u64| rows.saturating_mul(c.record).min((PARTITION_BYTES << 10) * HELD_TABLES) * 5 / 4;
        assert!(held(JOIN) < held(SCAN), "{kind:?}: the bound leaves the rows to tell");
        assert_eq!((c.rows, c.held), (JOIN, held(JOIN)), "{kind:?}: the join's rows, not the scan's ({} B held)", held(SCAN));
        assert_eq!(c.table, PARTITION_BYTES << 10, "{kind:?}: the bits' table is the lowering's groups, as before");
    }
}

#[test]
fn a_sort_a_materialize_and_a_top_k_past_a_join_open_the_joins_rows() {
    for (kind, spill) in [(BreakerKind::Sort, Spill::Runs), (BreakerKind::Materialize, Spill::None), (BreakerKind::TopK, Spill::None)] {
        let c = of(&past_join(kind, spill, JOIN)).expect("a share breaker");
        assert_eq!((c.rows, c.table), (JOIN, rows_bytes(JOIN, c.record)), "{kind:?}: the scan's rows asked {} B", rows_bytes(SCAN, c.record));
    }
    let top = Pipeline { top_rows: 30_000_000, ..past_join(BreakerKind::Sort, Spill::Runs, JOIN) };
    let c = of(&top).unwrap();
    assert_eq!(c.table, rows_bytes(JOIN, c.record), "a row top under its bound reads the join's rows (the scan's past it: 30000000)");
}

#[test]
fn a_fanning_join_a_serial_pipeline_a_drain_and_a_hash_build_keep_their_rows() {
    for (kind, spill) in [(BreakerKind::HashAgg, Spill::Partitioned { bits: 10 }), (BreakerKind::Sort, Spill::Runs), (BreakerKind::Materialize, Spill::None)] {
        let fan = past_join(kind, spill, SCAN * 3);
        assert_eq!(of(&fan).unwrap().rows, SCAN, "{kind:?}: end_rows past the scan's never grow it");
        let serial = Pipeline { predicted_rows: 0, ..past_join(kind, spill, JOIN) };
        assert_eq!(of(&serial).unwrap().rows, 0, "{kind:?}: a serial pipeline, the setting none: 0 rows as before");
        assert_eq!(of_at(&serial, false, true).unwrap().rows, JOIN, "{kind:?}: the setting final | all: its end_rows as before");
        let drained = Pipeline { build_rows: 4_368, ..past_join(kind, spill, JOIN) };
        assert_eq!(of(&drained).unwrap().rows, 4_368, "{kind:?}: a drain's named rows stand first (D3355)");
    }
    let build = Pipeline { build_rows: 1_000, store_row_bytes: 24, ..past_join(BreakerKind::HashBuild, Spill::None, JOIN) };
    assert_eq!(of(&build).unwrap().rows, build.build_estimate(), "a hash build reads its Hash node's rows");
}

/// The prerequisite: `end_rows` are bound per statement as `predicted_rows` are. A program lowered for one statement and
/// taken by another (KEY-PREDICTED-ROWS road (b)) is sized by the binding statement's rows at its join where its descriptor
/// names the graph pipeline (`end_of`), never by the lowering statement's; one no graph pipeline names is sized by its
/// bound rows alone; a serial pipeline's stay the program's (D3205's setting reads them as before).
#[test]
fn a_shared_program_is_sized_by_the_binding_statements_end_rows() {
    let (kind, spill) = (BreakerKind::HashAgg, Spill::Partitioned { bits: 10 });
    let lowered = Pipeline { rows_of: 0, end_of: 0, ..past_join(kind, spill, JOIN) };
    let same = [past_join(kind, spill, JOIN)];
    assert_eq!(bind_estimates(&[lowered], |j| same.get(j as usize)), None, "the lowering statement's estimates are the program's");
    let other = [past_join(kind, spill, 5_000_000)];
    let est = bind_estimates(&[lowered], |j| other.get(j as usize)).expect("another statement's rows at the join");
    assert_eq!(est[0], PipeEst { rows: SCAN, build: 0, end: 5_000_000 });
    assert_eq!(of(&lowered.with_est(est[0])).unwrap().rows, 5_000_000, "the binding statement's rows at the join, not the lowering's {JOIN}");
    let unnamed = Pipeline { rows_of: 0, ..past_join(kind, spill, JOIN) };
    let wider = [Pipeline { predicted_rows: 40_000_000, ..past_join(kind, spill, 5_000_000) }];
    let est = bind_estimates(&[unnamed], |j| wider.get(j as usize)).expect("another statement's rows");
    assert_eq!(est[0], PipeEst { rows: 40_000_000, build: 0, end: 0 }, "end_rows no graph pipeline names are dropped");
    assert_eq!(of(&unnamed.with_est(est[0])).unwrap().rows, 40_000_000, "sized by its bound rows, as before D4047");
    let serial = Pipeline { predicted_rows: 0, rows_of: 0, end_of: 0, ..past_join(kind, spill, JOIN) };
    let other = [Pipeline { predicted_rows: 0, ..past_join(kind, spill, 5_000_000) }];
    let est = bind_estimates(&[serial], |j| other.get(j as usize)).expect("another statement's rows at the join");
    assert_eq!(of_at(&serial.with_est(est[0]), false, true).unwrap().rows, JOIN, "a serial pipeline under final | all: the program's end_rows, as before");
}
