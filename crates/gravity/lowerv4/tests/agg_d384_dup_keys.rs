//! D384 (FIX-INTR, main 09-24; pgrcgen p0179, regress select_distinct#7): `SELECT count(*) FROM (SELECT DISTINCT two,
//! four, two FROM tenk1) ss` refused `Shape: a column the row does not carry (node 2)`. The planner drops the repeated
//! pathkey (grpColIdx (1 2)) while the Agg's target list still reads input column 3, the same `two`; decompose keyed only
//! grpColIdx, so the group row carried resnos 1 and 2 and the projection's outer.3 had no cell. Decompose now keys every
//! ungrouped input column the node reads that is one of its keys again (agg.rs `Shape::duplicate_keys`). The census
//! fixture is heap (NotBuilt M2 in M1): its relation is read as pgrcolumnar here, as pgrcgen's copy is.
use gravity_lowerv4_it as common;

#[test]
fn a_distinct_that_repeats_a_column_lowers() {
    let p = common::plans_dir().join("regress-full/select_distinct-002.plan");
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    let pgrc = text.replace(" am=heap ", " am=pgrcolumnar ");
    assert_ne!(pgrc, text, "select_distinct-002: no heap relation to read as pgrc");
    match common::lower_fixture(&pgrc) {
        Some(Ok(_)) => {}
        Some(Err(e)) => panic!("DISTINCT two, four, two refused: {e} (D384)"),
        None => panic!("select_distinct-002: decompose declined the fixture"),
    }
}
