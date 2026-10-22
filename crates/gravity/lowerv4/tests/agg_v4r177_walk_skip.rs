//! V4R-177 (D518 lever D; lowerv4 keyed/walk_skip.rs): the walked group of a grouped top-k (fieldobs-034, `GROUP BY
//! observer_id, notes LIMIT 10`) skips rows whose first key lies past the worker's bound. The program reads the bound at
//! each claim's open (host `walk_bound`) and tests each row's observer_id against it. `count(observer_id)` over a plain
//! column skips too. G4 / D449: an argument that can raise (`count(1 / observer_id)`) walks nothing (D554 `skip_safe`), so nothing skips. A
//! nullable key or the road off walks nothing, so nothing skips.
use gravity_lowerv4_it as common;

use gravity_lowerv4::face::strategy::{self, StrategySet};
use gravity_program::protos::ProtoKind;

const WALK_GROUP: u64 = 1 << 16;

const COUNT_STAR: &str = ":aggfnoid 2803 :aggtype 20 :aggcollid 0 :inputcollid 0 :aggtranstype 20 :aggargtypes <> :aggdirectargs <> :args <> :aggorder <> :aggdistinct <> :aggfilter <> :aggstar true";
const OBSERVER_ID: &str = "{VAR :varno -2 :varattno 8 :vartype 20 :vartypmod -1 :varcollid 0 :varnullingrels (b) :varlevelsup 0 :varreturningtype 0 :varnosyn 0 :varattnosyn 0 :location -1}";

fn group_top() -> String {
    std::fs::read_to_string(common::plans_dir().join("fieldobs-034.plan")).expect("fieldobs-034")
}

/// The grouped top with its `count(*)` made `count(<arg>)` (count("any"), CountAny).
fn count_of(arg: &str) -> String {
    let te = format!("({{TARGETENTRY :expr {arg} :resno 1 :resname <> :ressortgroupref 0 :resorigtbl 0 :resorigcol 0 :resjunk false}})");
    let agg = format!(":aggfnoid 2147 :aggtype 20 :aggcollid 0 :inputcollid 0 :aggtranstype 20 :aggargtypes (o 20) :aggdirectargs <> :args {te} :aggorder <> :aggdistinct <> :aggfilter <> :aggstar false");
    let text = group_top().replacen(COUNT_STAR, &agg, 1);
    assert_ne!(text, group_top(), "the count(*) is where the fixture has it");
    text
}

/// `(the group walk declared, the program reads the walk's bound)` of fixture `text`, lowered under `off`.
fn walk_and_skip(text: &str, off: &str) -> (bool, bool) {
    strategy::set_off(StrategySet::parse(off).unwrap_or_else(|e| panic!("{off}: {e}")));
    let l = match common::lower_fixture(text) {
        Some(Ok(l)) => l,
        Some(Err(e)) => panic!("lowers: {e:?}"),
        None => panic!("decompose refused"),
    };
    strategy::set_off(StrategySet::EMPTY);
    let walk = l.descs.v4.protos.iter().any(|p| p.kind == ProtoKind::Walk && p.arg & WALK_GROUP != 0);
    (walk, gravity_ir::text::print_program(&l.ir).contains("walk_bound"))
}

#[test]
fn group_walk_skips_rows_past_the_bound() {
    assert_eq!(walk_and_skip(&group_top(), ""), (true, true), "count(*): the walk and its row skip");
    assert_eq!(walk_and_skip(&count_of(OBSERVER_ID), ""), (true, true), "count(observer_id): a plain column's fold cannot raise");
}

#[test]
fn g4_a_raising_argument_folds_every_row() {
    let div = format!("{{OPEXPR :opno 687 :opfuncid 466 :opresulttype 20 :opretset false :opcollid 0 :inputcollid 0 :args ({{CONST :consttype 20 :consttypmod -1 :constcollid 0 :constlen 8 :constbyval true :constisnull false :location -1 :constvalue 8 [ 1 0 0 0 0 0 0 0 ]}} {OBSERVER_ID}) :location -1}}");
    assert_eq!(walk_and_skip(&count_of(&div), ""), (false, false), "count(1 / observer_id): D554 runs it whole, nothing skips (D449)");
    let nullable = group_top().replace("observer_id typid=20 typmod=-1 len=8 byval=t notnull=t", "observer_id typid=20 typmod=-1 len=8 byval=t notnull=f");
    assert_ne!(nullable, group_top());
    assert_eq!(walk_and_skip(&nullable, ""), (false, false), "a nullable key: no walk, no skip");
    assert_eq!(walk_and_skip(&group_top(), "topk.walk"), (false, false), "the road off: no walk, no skip");
}
