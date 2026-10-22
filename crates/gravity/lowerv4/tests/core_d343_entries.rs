//! D343's Defect-Test (M1-TABLES, main 09-24): the dict.entries road fires for a count(DISTINCT code) that images at
//! Program scope. source/dict_entries.rs gated on `site.tables.is_empty()`, but pgrc.rs declares the keyed end's Fp128
//! table over the lane (keyed_emit_group `fp_lanes`) before it asks, so count(DISTINCT notes) (fieldobs-010) and aggs-041 walked every
//! row where the site should publish each part's dictionary entries. A derived argument over the same code lane
//! (aggs-039) keeps its rows.
use gravity_program::Arena;

use gravity_lowerv4_it as common;

/// Each site's `entries` as v4 lowers census fixture `name` (the arena kept alive while the descriptors are read).
fn entries_of(name: &str) -> Vec<Option<u8>> {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    let text = std::fs::read_to_string(common::plans_dir().join(format!("{name}.plan"))).unwrap_or_else(|e| panic!("{name}: {e}"));
    // lower_fixture installs the rig's seams, and the fixture must lower at all
    assert!(matches!(common::lower_fixture(&text), Some(Ok(_))), "{name} does not lower");
    let ctx = mcx::MemoryContext::new("d343 fixture");
    let fx = gravity_decompose::corpus::load(ctx.mcx(), &text).unwrap_or_else(|e| panic!("{name}: {e}"));
    let arena = Arena::new(1 << 20);
    let graph = gravity_decompose::decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).expect("decomposes");
    gravity_expr::nodes::func::acl_defer_begin();
    let mut words = gravity_lower::Sequential::new(arena.handle());
    let l = gravity_lowerv4::lower_graph(arena.handle(), rig.mcx(), &graph, &fx.facts, common::RigCatalog::get(), &mut words, gravity_lower::LowerOpts::default());
    gravity_expr::nodes::func::acl_defer_end();
    let l = l.unwrap_or_else(|r| panic!("{name} refused: {r}"));
    l.descs.sites.iter().map(|s| s.entries).collect()
}

#[test]
fn a_plain_count_distinct_of_a_code_column_reads_the_dictionary_entries() {
    for name in ["fieldobs-010", "aggs-041"] {
        assert!(entries_of(name).contains(&Some(0)), "{name}'s site publishes no entries");
    }
}

#[test]
fn a_derived_count_distinct_keeps_its_rows() {
    assert!(entries_of("aggs-039").iter().all(Option::is_none), "aggs-039's site publishes entries");
}
