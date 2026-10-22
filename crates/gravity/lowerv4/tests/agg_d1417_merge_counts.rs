//! D1417 (main): regress partition_aggregate#32, `SELECT count(*) FROM pagg_tab GROUP BY c ORDER BY c LIMIT 1`, a
//! MergeAppend under a LIMIT over partitionwise GroupAggregates. The arms' count(*) raises on no input and each fold
//! pipeline scans its partition raising on no row, so props/merge.rs `arms_admit` admits the breaker form; before D1417
//! the arm's Aggref refused by D449's name.
use gravity_lowerv4_it as common;

#[test]
fn a_count_arm_under_the_limit_lowers() {
    let p = common::plans_dir().join("regress-full/partition_aggregate-010.plan");
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    assert!(text.contains("SELECT count(*) FROM pagg_tab GROUP BY c ORDER BY c LIMIT 1"), "the fixture's statement moved");
    match common::lower_fixture(&text).expect("regress-full-partition_aggregate-010 decomposes") {
        Ok(_) => {}
        Err(e) => panic!("refused: {e:?}"),
    }
}
