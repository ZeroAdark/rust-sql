//! M2's index family (M2-INDEX, cards/M2.md L5 and L0-h): the graph's routing of index walks to src.index /
//! src.index_only / src.index_array / src.index_am (graph.rs `source_kind_of`), and the family's verdicts on plan fixtures
//! beyond the seed corpus (tests/m2.rs). The server runner (e2e/gravity/v4/m2_index_*.sql) judges the rows against PG.
use gravity_lowerv4_it as common;

use gravity_decompose::graph::KeyForm;
use gravity_decompose::{IndexAm, Source};
use gravity_lowerv4::face::kinds::KindId;
use gravity_lowerv4::graph::source_kind_of;
use gravity_program::{Arena, ArenaVec, SiteDesc};
use mcx::MemoryContext;

fn plan(rel: &str) -> String {
    let p = common::plans_dir().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// Every index pipeline's source kind in `text`'s graph.
fn index_kinds(text: &str) -> Vec<KindId> {
    let ctx = MemoryContext::new("m2 index routing");
    let fx = gravity_decompose::corpus::load(ctx.mcx(), text).unwrap_or_else(|e| panic!("fixture: {e}"));
    let arena = Arena::new(1 << 20);
    let g = gravity_decompose::decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).unwrap_or_else(|e| panic!("decompose: {e:?}"));
    g.pipelines.iter().filter(|p| matches!(p.source, Source::Index { .. })).map(|p| source_kind_of(&p.source, p.node_id)).collect()
}

/// L0-h: a btree walk with an `= ANY` key is src.index_array's (C80's IN list over the customer key).
#[test]
fn a_btree_walk_with_an_any_key_routes_to_index_array() {
    assert_eq!(index_kinds(&plan("oltpc-046.plan")), [KindId::SrcIndexArray]);
}

/// L0-h: a btree walk with an ORDER BY operator key is src.index_am's. No btree opclass has an ordering operator (the
/// planner never makes one), so the walk is an SP-GiST KNN walk's keys over a plain btree descriptor.
#[test]
fn a_btree_walk_with_an_order_by_key_routes_to_index_am() {
    let text = plan("regress-full/create_index_spgist-008.plan");
    let ctx = MemoryContext::new("m2 index routing");
    let fx = gravity_decompose::corpus::load(ctx.mcx(), &text).unwrap_or_else(|e| panic!("fixture: {e}"));
    let arena = Arena::new(1 << 20);
    let g = gravity_decompose::decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).unwrap_or_else(|e| panic!("decompose: {e:?}"));
    let mut n = 0;
    for p in g.pipelines.iter() {
        let Source::Index { am, scanrelid, relid, indexid, keys, backward, recheck, .. } = &p.source else { continue };
        assert!(keys.iter().any(|k| k.form == KeyForm::OrderBy), "create_index_spgist-008 has no ORDER BY key");
        let mut ks = ArenaVec::with_capacity_in(keys.len(), arena.handle());
        ks.extend(keys.iter().copied());
        let btree = Source::Index { site: SiteDesc::none_in(arena.handle()), am: *am, scanrelid: *scanrelid, relid: *relid, indexid: *indexid, keys: ks, only: false, backward: *backward, recheck, iam: IndexAm::Btree };
        assert_eq!(source_kind_of(&btree, p.node_id), KindId::SrcIndexAm);
        n += 1;
    }
    assert_eq!(n, 1);
}

/// G4 (does not apply): plain btree comparison keys stay src.index's, an index-only walk stays src.index_only's (with an
/// `= ANY` key too: C32's shape, and create_index-047's `= ANY (NULL)`).
#[test]
fn plain_and_index_only_walks_keep_their_kinds() {
    assert_eq!(index_kinds(&plan("oltpc-001.plan")), [KindId::SrcIndex]);
    assert_eq!(index_kinds(&plan("oltpc-025.plan")), [KindId::SrcIndexOnly]);
    assert_eq!(index_kinds(&plan("regress-full/create_index-047.plan")), [KindId::SrcIndexOnly]);
}

/// L0-h: an index-only walk of another AM (GiST here) is src.index_am's, which serves every non-btree walk: src.index_only
/// is the btree leaf walk (hosts `indexonly::next`).
#[test]
fn a_gist_index_only_walk_routes_to_index_am() {
    let text = plan("regress-full/create_index-011.plan");
    assert!(text.contains("{INDEXONLYSCAN") && text.contains(" am=gist "), "create_index-011 is no GiST index-only scan");
    assert_eq!(index_kinds(&text), [KindId::SrcIndexAm]);
}

/// src.index_only's site keeps a RowId lane after its attribute lanes: the host batches index-only rows into N-row windows
/// only for such a site (hosts v3_r1b.rs `batched(rowid)`; C32 / C92's windows). D2900: C32 counts rows, so its index
/// condition's text column has no lane (a btree's recheckqual is never evaluated): its site is the RowId lane alone, and
/// still asks for windows.
#[test]
fn an_index_only_site_keeps_the_rowid_lane_that_asks_for_windows() {
    let l = common::lower_fixture(&plan("oltpc-025.plan")).expect("decompose takes C32").unwrap_or_else(|e| panic!("C32 refused: {e}"));
    let site = l.descs.sites.iter().find(|s| s.lanes().iter().any(|x| matches!(x.id, gravity_program::LaneId::RowId))).expect("C32's index site");
    let lanes = site.lanes();
    assert_eq!(lanes.last().map(|x| x.id), Some(gravity_program::LaneId::RowId), "the RowId lane is the site's last lane");
    assert!(lanes[..lanes.len() - 1].iter().all(|x| matches!(x.id, gravity_program::LaneId::Attr(_))), "attribute lanes before it (the host's lane order)");
}
