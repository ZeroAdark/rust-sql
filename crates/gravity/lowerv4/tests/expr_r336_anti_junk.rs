//! census rules#336 (`DELETE FROM shoelace WHERE EXISTS (SELECT * FROM shoelace_candelete WHERE sl_name =
//! shoelace.sl_name)`, regress rules.sql) refused `Shape: a column the row does not carry (node 10)`. The planner runs
//! the EXISTS as an inner join over a HashAggregate that unique-ifies the subquery (a Hash Right Anti Join for the view's
//! NOT EXISTS), and the Agg carries the ctid junk of the four relations the DELETE marks ROW_MARK_REFERENCE as ungrouped
//! columns, each an OUTER_VAR / INNER_VAR copy through the joins (the anti join's nulled side among them). decompose gave
//! such junk an `any_value` cell only when the Agg's child was the scan that reads the ctid (facts/demand/readers.rs
//! `marked_junk`), so the group row carried the key alone; it now follows the copies down to the scan (`junk_origin`).
//! The fixture is the census statement's regress plan (decompose tests/plans/regress-full/rules-060.plan).
use gravity_lowerv4_it as common;

#[test]
fn a_unique_ified_exists_over_joins_carries_its_row_marks() {
    let p = common::plans_dir().join("regress-full/rules-060.plan");
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    match common::lower_fixture(&text) {
        Some(Ok(_)) => {}
        Some(Err(e)) => panic!("rules#336 refused: {e}"),
        None => panic!("rules#336: decompose declined the fixture"),
    }
}
