use gravity_exec::admit::entry::{decode, encode, is_ours, refusal, MAX_DESCS};
use gravity_exec::admit::entry::instrument::{prune_result, root_rows, rti_unpruned};
use types_nodes::nodes_enums::CmdType;
use types_portal::QueryDescHandle;
use types_slot::{EXEC_FLAG_BACKWARD, EXEC_FLAG_EXPLAIN_GENERIC, EXEC_FLAG_EXPLAIN_ONLY, EXEC_FLAG_MARK, EXEC_FLAG_REWIND};

#[test]
fn handles_round_trip_and_never_collide_with_the_incumbents() {
    // D3671: slot 65,535 and past (a deep SQL-function recursion's nested statements) round-trip too
    for (idx, gen) in [(0u32, 1u16), (7, 3), (65534, 0xfff), (65535, 1), (65536, 0), (MAX_DESCS - 1, 0xfff)] {
        let h = encode(idx, gen);
        assert!(is_ours(h));
        assert_eq!(decode(h), (idx, gen));
        assert!(!h.is_null());
    }
    for raw in [1u64, 7, (5u64 << 32) | 1, (u32::MAX as u64) << 32 | 0xffff_ffff] {
        assert!(!is_ours(QueryDescHandle(raw)));
    }
    assert!(!is_ours(QueryDescHandle::NULL));
}

#[test]
fn the_entry_refuses_what_it_cannot_drive() {
    assert_eq!(refusal(CmdType::CMD_SELECT, 0, true, false), None);
    assert_eq!(refusal(CmdType::CMD_SELECT, EXEC_FLAG_REWIND, true, false), None);
    assert_eq!(refusal(CmdType::CMD_UTILITY, 0, true, false), Some("entry: utility statement"));
    assert_eq!(refusal(CmdType::CMD_SELECT, EXEC_FLAG_EXPLAIN_ONLY, true, false), None);
    assert_eq!(refusal(CmdType::CMD_SELECT, EXEC_FLAG_EXPLAIN_ONLY | EXEC_FLAG_EXPLAIN_GENERIC, true, false), Some("entry: explain only (a generic plan whose parameters are unbound)"));
    assert_eq!(refusal(CmdType::CMD_SELECT, EXEC_FLAG_BACKWARD, true, false), Some("entry: backward scan (scrollable cursor)"));
    assert_eq!(refusal(CmdType::CMD_SELECT, EXEC_FLAG_MARK, true, false), Some("entry: backward scan (scrollable cursor)"));
    assert_eq!(refusal(CmdType::CMD_SELECT, 0, false, false), Some("entry: no snapshot"));
    assert_eq!(refusal(CmdType::CMD_SELECT, 0, true, true), Some("entry: parallel mode"));
}

/// A served EXPLAIN ANALYZE counts its root exactly, the statement's rows: a SELECT's rows sent, a
/// ModifyTable's RETURNING rows (one per row written), none without RETURNING; a SELECT whose program writes a
/// data-modifying CTE (its count is the write's) and a run not done are not counted.
#[test]
fn the_root_counts_the_statements_rows_or_is_not_counted() {
    assert_eq!(root_rows(None, false, true, true, 10), Some(10), "a SELECT root: the rows sent");
    assert_eq!(root_rows(None, false, true, true, 0), Some(0));
    assert_eq!(root_rows(Some(true), false, false, true, 3), Some(3), "INSERT .. RETURNING: one row per row written");
    assert_eq!(root_rows(Some(false), false, false, true, 3), Some(0), "a ModifyTable without RETURNING returns no row");
    assert_eq!(root_rows(None, true, true, true, 3), None, "a data-modifying CTE's count is the write's");
    assert_eq!(root_rows(None, false, false, true, 3), None, "a write's count under a root that is no ModifyTable");
    assert_eq!(root_rows(None, false, true, false, 10), None, "not done: not counted");
    assert_eq!(root_rows(Some(false), false, false, false, 0), None);
}

/// EXPLAIN ANALYZE of run-time pruning (entry.md §4): the served desc answers PostgreSQL's initial pruning results, an
/// info's kept subplans (`None` without initial steps) and the unpruned range-table indexes; a desc that computed none
/// answers `None` for both, which EXPLAIN reads as every child kept.
#[test]
fn the_prune_getters_answer_the_initial_pruning_results() {
    let p = gravity_exec::ops::append::prune::Initial { valid: vec![Some(vec![3, 4, 5]), None, Some(vec![])], unpruned: vec![1, 2, 7] };
    assert_eq!(prune_result(Some(&p), 0), Some(vec![3, 4, 5]));
    assert_eq!(prune_result(Some(&p), 1), None, "no initial steps: every subplan kept");
    assert_eq!(prune_result(Some(&p), 2), Some(vec![]), "every subplan removed");
    assert_eq!(prune_result(Some(&p), 3), None);
    assert_eq!(prune_result(Some(&p), -1), None);
    assert_eq!(prune_result(None, 0), None);
    assert_eq!((rti_unpruned(Some(&p), 7), rti_unpruned(Some(&p), 3)), (Some(true), Some(false)));
    assert_eq!(rti_unpruned(None, 3), None);
}
