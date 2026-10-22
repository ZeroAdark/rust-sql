//! W3-D1486-NONFAM row 1 (V4R-553, M8-NEST): a settled union whose arms are hash joins of quiet scans (a partitionwise
//! join's children; nest/union_join_arms.rs) passes D1486's arms rule. Fixtures fixtures/m8/d1486, captured from the
//! partition_join regress state on trunk e08236da3d0 (census r47 partition_join #44 = -027, #45 = -028, #235 = -103,
//! #241 = -109). The crate's rig catalog has no hash support face, so GUARD-G1's `support` answers USER_FN for the
//! joined arms here (the server resolves int4 / text hashing as built in: e2e v4/m8_d1486_joinarms serves them). An arm
//! whose hash clause calls a built-in function that is not leakproof (it can raise) takes no rule of D1486; by
//! `DECISIONS.md` 4 (D) b its union fills ahead of PostgreSQL's pulls unchecked (nothing in it acts or calls user code),
//! so it is served.
use gravity_lowerv4_it as common;

use gravity_lowerv4::effects::USER_FN;

/// partition_join-027's hash clauses: the first is the top join's filter `(t1.b + coalesce(ss.t2b, 0)) = 0`, then one per arm.
const INT4EQ: &str = ":opno 96 :opfuncid 65";

fn text(name: &str) -> String {
    let p = common::milestone_dir(8).join("d1486").join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn verdict(text: &str) -> String {
    match common::lower_fixture(text).expect("decomposes") {
        Ok(_) => "served".to_owned(),
        Err(r) => r.what.to_owned(),
    }
}

#[test]
fn the_census_joined_arms_pass_d1486_to_the_guard() {
    let user_fn = format!("Shape: {}", USER_FN.0);
    for f in ["partition_join-027.plan", "partition_join-028.plan", "partition_join-103.plan"] {
        assert_eq!(verdict(&text(f)), user_fn, "{f}: past D1486's arms rule, at GUARD-G1's hash support");
    }
}

#[test]
fn an_arm_whose_hash_clause_can_raise_reads_ahead() {
    let t = text("partition_join-027.plan");
    assert_eq!(t.matches(INT4EQ).count(), 4, "the fixture's int4 = clauses");
    for arm in 1..=3 {
        // the arm's clause through int4pl (177): not leakproof, it can raise
        let at = t.match_indices(INT4EQ).nth(arm).map(|(i, _)| i).expect("the arm's clause");
        let loud = format!("{}:opno 96 :opfuncid 177{}", &t[..at], &t[at + INT4EQ.len()..]);
        assert_eq!(verdict(&loud), "served", "arm {arm} loud: its union fills ahead unchecked");
    }
}

#[test]
fn a_quiet_union_under_an_outer_first_hash_build_lowers() {
    // #241: the union is the build of an outer-first hash join (join/build_class.rs reads every arm's side: all quiet scans)
    assert_eq!(verdict(&text("partition_join-109.plan")), "served");
}
