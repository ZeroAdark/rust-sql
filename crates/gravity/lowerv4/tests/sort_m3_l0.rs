//! M3 L0 (M3-plan §1.3, decision 1, the user 09-24): the seams M3 reaches M1 files through, each dormant at M1.
//! - L0-d, the count-ordered owner top (face/keyed.rs `OwnerTopMech`): a group whose top orders first by a count final
//!   declares road owner.count's owner descriptor and the emit gathers the kept records. With the road forced off the seam
//!   is never asked. (M1's NotBuilt(owner.count) test went when M3-OWNER built the road: tests/m3_owner.rs, V4R-113.)
use gravity_lowerv4_it as common;

use std::cell::RefCell;

use gravity_abi::breaker::SlotKind;
use gravity_decompose::ops::shared::agg::GroupDecl;
use gravity_lowerv4::table::face::{stub_owner_top, OwnerTopMech, TableDecl};
use gravity_lowerv4::face::strategy::{StrategySet, CODE_FRONT_LIGHT, OWNER_COUNT, set_off};
use gravity_lowerv4::frame::px::Px;
use gravity_program::OwnerDesc;

fn text(rel: &str) -> String {
    let p = common::plans_dir().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn verdict(t: &str) -> Result<common::LoweredIn, String> {
    common::lower_fixture(t).expect("decompose takes the fixture").map_err(|e| e.to_string())
}

thread_local! {
    /// Every call the stub saw on this test's thread: (pipeline, bound).
    static SEEN: RefCell<Vec<(usize, u32)>> = const { RefCell::new(Vec::new()) };
}

fn seen() -> Vec<(usize, u32)> {
    SEEN.with(|s| s.borrow().clone())
}

/// Stands in for M3's runs/owner_count.rs: the count-ordered descriptor over the table's record (its count, the sentinel).
struct Stub;

impl OwnerTopMech for Stub {
    fn owner<'p>(&self, _px: &mut Px<'_, 'p>, k: usize, _g: &GroupDecl<'p>, t: &TableDecl, n: u32) -> OwnerDesc {
        SEEN.with(|s| s.borrow_mut().push((k, n)));
        let count = (0..t.layout.slot_kinds().len()).find(|&j| t.layout.slot_kinds()[j] == SlotKind::Count).unwrap_or(0);
        OwnerDesc::count_topk(t.layout, count as u8, n)
    }
}

static STUB: Stub = Stub;

/// The stub on this thread and owner.count forced off (admission's refusal of the unbuilt road skipped), `f`, then both reset.
/// code.front_light (V4R-106) is forced off with it: fieldobs-033's code key under a count-ordered top applies that unbuilt road too.
fn with_stub<T>(f: impl FnOnce() -> T) -> T {
    SEEN.with(|s| s.borrow_mut().clear());
    stub_owner_top(Some(&STUB));
    let mut off = StrategySet::EMPTY;
    off.add(OWNER_COUNT);
    off.add(CODE_FRONT_LIGHT);
    set_off(off);
    let r = f();
    set_off(StrategySet::EMPTY);
    stub_owner_top(None);
    r
}

/// L0-d: a registered owner.count is asked for the count-ordered top's owner, with the top's bound, and the statement lowers
/// with its descriptor as the group pipeline's owner.
#[test]
fn a_count_ordered_top_reaches_the_owner_top_mechanism() {
    let l = with_stub(|| verdict(&text("fieldobs-033.plan")));
    let seen = seen();
    assert!(!seen.is_empty() && seen.iter().all(|&s| s == (seen[0].0, 10)), "the group pipeline's owner, bound LIMIT 10: {seen:?}");
    let l = l.unwrap_or_else(|e| panic!("fieldobs-033 under the stub: {e}"));
    let owner = l.descs.pipelines[seen[0].0].owner.expect("the group pipeline's owner phase");
    assert_eq!((owner.topk, owner.order_slot), (10, 0), "the stub's count-ordered descriptor: {owner:?}");
}

/// G4: a top ordered by a group key (`ORDER BY k`) or by an expression over finals (bench SVlen) is not count-ordered: the seam
/// never asks the mechanism.
#[test]
fn a_top_not_ordered_by_a_count_never_asks() {
    for fx in ["bench-065.plan", "bench-149.plan"] {
        let _ = with_stub(|| verdict(&text(fx)));
        assert!(seen().is_empty(), "{fx} asked the count-ordered owner");
    }
}

/// L0-c / decision 15 (D345 / D346): an ORDER BY inside an aggregate call (aggs-014: `array_agg(i4 ORDER BY i8 DESC, t)`,
/// `sum(i4 ORDER BY i8)` by k) is never folded unordered. D458 (V4R-426) serves it: the group keeps the call's arguments
/// and ORDER BY columns and its final sorts them by the ORDER BY before the transfn (host-sorted, PG's
/// process_ordered_aggregate_single / _multi); the m6 seed pins its IR (aggs-014.ir).
#[test]
fn an_ordered_aggregate_folds_host_sorted() {
    if let Err(v) = verdict(&text("aggs-014.plan")) {
        panic!("D458 serves an ordered aggregate host-sorted: {v}");
    }
}

/// M3 decision 16 (main 09-25: the lookup shape): psql `\d`'s lookup as pg_regress captured it (inherit-001: pg_class LEFT
/// JOIN pg_namespace ON n.oid = c.relnamespace) takes P catalog_nest, so nest.catalog_left applies instead of the M8 nest
/// roads (V4R-142: served as nest.catalog). DC-14 / NEST-01 (no catalog relation named): psql `\da`'s catalog nest over
/// pg_proc (create_aggregate-001: pg_namespace probed by its primary key, a quiet inner) takes it too. G4: a user-table left
/// nested loop (aggregates-037) does not. (aggregates-038, the same query over f2, decompose refuses since D858: its merge
/// clause's inner is an int4 relabelled to oid.) create_schema-001 (psql `\d`'s lookup, which a namespace
/// filter `n.nspname ~ '^(..)$'` makes inner) takes it against the server only: DC-14 / NEST-01 asks the inner's filters to be
/// quiet, and a regex is quiet once the server compiles it (semantics DC-14, Facts::regex_compiles), which a fixture
/// answers false (e2e v4/m8_dc14_catalog_lookup and regress keep the live form).
#[test]
fn psql_lookup_nest_takes_catalog_nest() {
    let props = |fx: &str| common::props_fixture(&text(fx)).unwrap_or_else(|| panic!("{fx}: decompose declined")).1;
    for fx in ["regress-full/inherit-001.plan", "regress-full/create_aggregate-001.plan"] {
        assert!(props(fx).contains("catalog_nest"), "{fx}: {:?}", props(fx));
        // V4R-142: the lookup is a catalog probe (nest.catalog, M3): it lowers, or refuses on a later need, never nest.loop
        if let Err(v) = verdict(&text(fx)) {
            assert!(v.contains("NotBuilt") && !v.contains("nest.loop"), "{fx}: {v}");
        }
    }
    for fx in ["regress-full/aggregates-037.plan"] {
        assert!(!props(fx).contains("catalog_nest"), "{fx}: {:?}", props(fx));
    }
}
