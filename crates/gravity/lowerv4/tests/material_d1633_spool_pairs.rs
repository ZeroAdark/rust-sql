//! D1633 (M8-NEST, fail closed): a plan Material's pulled child is made again inside its nest inner over a site of its
//! own. Exec admit (admit.rs `registry`) pairs the program's source sites, in site order, with the graph's sources in its
//! walk (`sources`: a pipeline's source, then its nest inners depth-first), each site the first unused source of its
//! relation and index, and an index recipe takes its quals from the source it pairs. nest/spool_site.rs `walk_site`
//! numbers the child only where it pairs with its own fill pipeline's source (a key over heap ranges only pairs in any
//! order: a heap range's recipe is its site's alone), moving held owner numbers after it where that pairs every site with
//! its own (B-R1, V4R-519), else the Material refuses by D1633's name (tests/d1919_readers.rs). Fixtures fixtures/m8/d1633:
//! m8_d1633_spool_pairs' statements captured over a fresh initdb (gravity-capture-plans.sh m8x, PostgreSQL 18.6).
use gravity_lowerv4_it as common;

use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::subplan::{SubForm, SubPlanDesc};
use gravity_decompose::{corpus, Am, Op, PipelineNode, Source};

fn text(rel: &str) -> String {
    let p = common::milestone_dir(8).join("d1633").join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// A source's key in exec admit's pairing (its relation and index) and the columns its site reads (what tells two scans
/// of one key apart).
type Key = (u32, u32, Vec<i16>);

/// Exec admit's walk: each source's key and whether it is a heap range.
fn walk(rel: &str) -> Vec<(Key, bool)> {
    fn sources(p: &PipelineNode<'_>, out: &mut Vec<(Key, bool)>) {
        if let Some((_, r, sd)) = p.source.relation() {
            out.push(((r, sd.index, sd.out_attnos.to_vec()), matches!(p.source, Source::Range { am: Am::Heap, .. })));
        }
        for op in p.ops.iter() {
            if let Op::Join(JoinDesc::Nest { inner, .. }) | Op::SubPlan(SubPlanDesc { form: SubForm::Nested { inner }, .. }) = op {
                sources(inner, out);
            }
        }
    }
    let ctx = mcx::MemoryContext::new("d1633 walk");
    let arena = gravity_program::Arena::new(1 << 20);
    let t = text(rel);
    let fx = corpus::load(ctx.mcx(), &t).unwrap_or_else(|e| panic!("{rel}: {e}"));
    let g = gravity_decompose::decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).ok().unwrap_or_else(|| panic!("{rel}: decompose refused"));
    let mut out = Vec::new();
    g.pipelines.iter().for_each(|p| sources(p, &mut out));
    out
}

/// Each program source site, in site order, with the walk entry exec admit pairs it with (the first unused of its
/// relation and index, else the first).
fn pairs(rel: &str) -> Vec<(Key, usize)> {
    let w = walk(rel);
    let l = common::lower_fixture(&text(rel)).expect("decomposes").unwrap_or_else(|r| panic!("{rel} refused: {r:?}"));
    let mut used = vec![false; w.len()];
    let sites = l.descs.sites.iter().filter(|s| s.is_source()).map(|s| (s.relation, s.index, s.out_attnos.to_vec()));
    sites
        .map(|k| {
            let first = |unused: bool| w.iter().enumerate().position(|(i, e)| (!unused || !used[i]) && (e.0 .0, e.0 .1) == (k.0, k.1));
            let i = first(true).or_else(|| first(false)).unwrap_or_else(|| panic!("{rel}: no source for site {k:?}"));
            used[i] = true;
            (k, i)
        })
        .collect()
}

/// D1633's witness (a1: an index inner over d33u held the owner's number, the child scans the owner's index d33w_id
/// with other quals) and Q2b's shape (q2b: a heap owner, an index inner over d33w_id, then the child over it): B-R1
/// (V4R-519, nest/spool_site.rs) numbers the child at the owner's held number and the owner next, so every site pairs
/// with its own scan (the full key: relation, index, columns). A scan carries only the columns its consumers read
/// (decompose facts/demand.rs `entries_read`), so both fixtures' two d33w_id scans now read (id, k) and the columns no
/// longer tell them apart: the check keeps the shared key and the pairing in walk order; the rows are e2e
/// m8_d1633_spool_pairs' (PG 18.6).
#[test]
fn a_child_after_an_index_inner_takes_the_held_number() {
    for rel in ["d1633-a1.plan", "d1633-q2b.plan"] {
        let w = walk(rel);
        let p = pairs(rel);
        for (k, i) in p.iter() {
            assert_eq!(*k, w[*i].0, "{rel}: site {k:?} pairs {:?}", w[*i].0);
        }
        let same: Vec<_> = p.iter().filter(|(k, _)| p.iter().filter(|(o, _)| (o.0, o.1) == (k.0, k.1)).count() > 1).collect();
        assert!(same.len() >= 2, "{rel}: {same:?}");
        assert!(same.windows(2).all(|x| x[0].1 < x[1].1), "{rel}: the sites of one key pair in walk order: {same:?}");
    }
}

/// Does not apply: no earlier inner (d0), the child over a copy relation with its own index (d2), an earlier inner over
/// another relation (n1). Served, and every site pairs with its own scan (the full key: relation, index, columns).
#[test]
fn a_child_pairing_its_own_scan_is_served() {
    for rel in ["d1633-d0.plan", "d1633-d2.plan", "d1633-n1.plan"] {
        let w = walk(rel);
        for (k, i) in pairs(rel) {
            assert_eq!(k, w[i].0, "{rel}: site {k:?} pairs {:?}", w[i].0);
        }
    }
}

/// Does not apply: the owner and the child heap scans of one relation with other columns (h1: the owner reads x, the
/// child does not), an index inner holding the owner's number: the owner's site pairs the child's source, and both are
/// heap ranges (a heap range's recipe is its site's alone), so the Material is served. B-R1's walk_site keeps D1633's
/// number here (one level down the next number comes first; a heap-only key never needs the shift), so the crossing
/// stands (D1633's minor 4); the fixture's control reads x2.id % 400 = 5, which returns rows (minor 2).
#[test]
fn heap_ranges_of_one_relation_pair_in_any_order() {
    let rel = "d1633-h1.plan";
    let w = walk(rel);
    let p = pairs(rel);
    let crossed: Vec<_> = p.iter().filter(|(k, i)| *k != w[*i].0).collect();
    assert!(!crossed.is_empty(), "{rel}: no crossed pair ({p:?} over {w:?})");
    assert!(crossed.iter().all(|(_, i)| w[*i].1), "{rel}: a crossed pair over a non-heap source ({crossed:?} over {w:?})");
}
