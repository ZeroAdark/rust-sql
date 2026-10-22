//! D1919 (M8-NEST, LTC-NEST-DRAIN B-R1): a plan Material under nested nest loops is read by the innermost inner only (the
//! inner between is an owner: spool_readers.rs R1), its pulled child's site is numbered where exec admit pairs it with its
//! own source (nest/spool_site.rs `walk_site`, D1633: exec admit.rs `registry` pairs the program's source sites, in site
//! order, with the graph's sources in its walk order, a pipeline's source then its nest inners depth-first, each with the
//! first unused one of its relation and index), and the forms it does not serve refuse by name.
use gravity_lowerv4_it as common;

use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::subplan::{SubForm, SubPlanDesc};
use gravity_decompose::{corpus, Am, Op, PipelineNode, Source};

fn text(rel: &str) -> String {
    let p = [common::plans_dir(), common::milestone_dir(8).join("d1919")].map(|d| d.join(rel)).into_iter().find(|p| p.exists());
    let p = p.unwrap_or_else(|| panic!("{rel}: no plan fixture"));
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// A source's key in exec admit's pairing (its relation and index) and the columns its site reads (what tells two sites
/// over one index apart).
type Key = (u32, u32, Vec<i16>);

/// m8_d1109_readers' relations (fixtures/m8/d1919, captured over a fresh initdb) and the two indexes.
const O: u32 = 16395;
const M: u32 = 16398;
const N: u32 = 16403;
const X: u32 = 16408;
const Y: u32 = 16411;
const Z: u32 = 16414;
const W: u32 = 16418;
const X_ID: u32 = 16417;
const W_ID: u32 = 16421;
const U: u32 = 16422;

/// Exec admit's walk (admit.rs `sources`): each graph pipeline's source, then its nest inners' depth-first (a source
/// over no relation skipped), each with whether it is a heap range (whose recipe is its site's alone).
fn walk_src(rel: &str) -> Vec<(Key, bool)> {
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
    let ctx = mcx::MemoryContext::new("d1919 walk");
    let arena = gravity_program::Arena::new(1 << 20);
    let t = text(rel);
    let fx = corpus::load(ctx.mcx(), &t).unwrap_or_else(|e| panic!("{rel}: {e}"));
    let g = gravity_decompose::decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).ok().unwrap_or_else(|| panic!("{rel}: decompose refused"));
    let mut out = Vec::new();
    g.pipelines.iter().for_each(|p| sources(p, &mut out));
    out
}

fn walk(rel: &str) -> Vec<Key> {
    walk_src(rel).into_iter().map(|e| e.0).collect()
}

/// The program's source sites in site order (the order exec admit pairs them with `walk`'s, first unused by relation and
/// index).
fn sites(rel: &str) -> Vec<Key> {
    let l = common::lower_fixture(&text(rel)).expect("decomposes").unwrap_or_else(|r| panic!("{rel} refused: {r:?}"));
    l.descs.sites.iter().filter(|s| s.is_source()).map(|s| (s.relation, s.index, s.out_attnos.to_vec())).collect()
}

fn rels(keys: &[Key]) -> Vec<u32> {
    keys.iter().map(|k| k.0).collect()
}

/// The walk's relations are `want` and the program's sites are the walk's keys in its order: exec admit pairs each site
/// with its own source.
fn in_walk_order(rel: &str, want: &[u32]) {
    let w = walk(rel);
    assert_eq!(rels(&w), want, "{rel}: walk");
    assert_eq!(sites(rel), w, "{rel}: sites");
}

/// Exec admit's pairing (admit.rs `registry`): each site, in site order, takes the first unused walk source of its relation
/// and index, else the first. Each site pairs its own source (the same columns), but over a key whose sources are all heap
/// ranges, where any pairing reads the right rows. `distinct` keys must have one column list per source, so a crossed
/// pair shows.
fn paired(rel: &str, distinct: &[(u32, u32)]) {
    let w = walk_src(rel);
    for &(r, i) in distinct {
        let mut cols: Vec<&Vec<i16>> = w.iter().filter(|e| (e.0 .0, e.0 .1) == (r, i)).map(|e| &e.0 .2).collect();
        let n = cols.len();
        cols.dedup();
        assert!(n > 1 && cols.len() == n, "{rel}: ({r}, {i}) has no distinct column lists: {cols:?}");
    }
    let mut used = vec![false; w.len()];
    for s in sites(rel) {
        let same = |e: &(Key, bool)| (e.0 .0, e.0 .1) == (s.0, s.1);
        let i = (0..w.len()).find(|&i| !used[i] && same(&w[i])).or_else(|| (0..w.len()).find(|&i| same(&w[i]))).unwrap_or_else(|| panic!("{rel}: no source for {s:?}"));
        used[i] = true;
        assert!(w.iter().filter(|e| same(e)).all(|e| e.1) || w[i].0 == s, "{rel}: site {s:?} pairs {:?}", w[i].0);
    }
}

/// Exec admit's walk with each source's plan node (admit.rs `sources`' node id, which a pinned site names).
fn walk_nodes(rel: &str) -> Vec<(Key, bool, i32)> {
    fn sources(p: &PipelineNode<'_>, out: &mut Vec<(Key, bool, i32)>) {
        if let Some((_, r, sd)) = p.source.relation() {
            out.push(((r, sd.index, sd.out_attnos.to_vec()), matches!(p.source, Source::Range { am: Am::Heap, .. }), p.node_id));
        }
        for op in p.ops.iter() {
            if let Op::Join(JoinDesc::Nest { inner, .. }) | Op::SubPlan(SubPlanDesc { form: SubForm::Nested { inner }, .. }) = op {
                sources(inner, out);
            }
        }
    }
    let ctx = mcx::MemoryContext::new("d1919 walk");
    let arena = gravity_program::Arena::new(1 << 20);
    let t = text(rel);
    let fx = corpus::load(ctx.mcx(), &t).unwrap_or_else(|e| panic!("{rel}: {e}"));
    let g = gravity_decompose::decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).ok().unwrap_or_else(|| panic!("{rel}: decompose refused"));
    let mut out = Vec::new();
    g.pipelines.iter().for_each(|p| sources(p, &mut out));
    out
}

/// D3769: exec admit's pairing with the program's pins (V4Descs `site_pins`, admit.rs `registry`): each pinned site takes
/// its node's source of its relation and index first, then every other site, in site order, the first unused walk source
/// of its relation and index, else the first. Every site pairs a source of its own columns (but over a key whose sources
/// are all heap ranges), and each pinned node is a distinct source. The pinned nodes, in pin order.
fn paired_pinned(rel: &str) -> Vec<i32> {
    let w = walk_nodes(rel);
    let l = common::lower_fixture(&text(rel)).expect("decomposes").unwrap_or_else(|r| panic!("{rel} refused: {r:?}"));
    let sites: Vec<(usize, Key)> = l.descs.sites.iter().enumerate().filter(|(_, s)| s.is_source()).map(|(i, s)| (i, (s.relation, s.index, s.out_attnos.to_vec()))).collect();
    let (mut used, mut got) = (vec![false; w.len()], vec![None; l.descs.sites.len()]);
    let mut nodes = Vec::new();
    for &(si, node) in l.descs.v4.site_pins.iter() {
        let s = &sites.iter().find(|x| x.0 == si as usize).unwrap_or_else(|| panic!("{rel}: pinned site {si} is no source site")).1;
        let i = (0..w.len()).find(|&i| !used[i] && w[i].2 == node && (w[i].0 .0, w[i].0 .1) == (s.0, s.1)).unwrap_or_else(|| panic!("{rel}: pinned site {si}: no unused source of node {node}"));
        (used[i], got[si as usize]) = (true, Some(i));
        nodes.push(node);
    }
    for (si, s) in sites.iter() {
        let same = |e: &(Key, bool, i32)| (e.0 .0, e.0 .1) == (s.0, s.1);
        let i = got[*si].or_else(|| (0..w.len()).find(|&i| !used[i] && same(&w[i]))).or_else(|| (0..w.len()).find(|&i| same(&w[i]))).unwrap_or_else(|| panic!("{rel}: no source for {s:?}"));
        used[i] = true;
        assert!(w.iter().filter(|e| same(e)).all(|e| e.1) || w[i].0 == *s, "{rel}: site {s:?} pairs {:?}", w[i].0);
    }
    nodes
}

fn refusal(rel: &str) -> String {
    match common::lower_fixture(&text(rel)).expect("decomposes") {
        Ok(_) => panic!("{rel}: lowered"),
        Err(r) => r.what.to_owned(),
    }
}

/// join-204 (join#617): a Material of int8_tbl (16403) under a LEFT nest's inner over sj (25707) under the top nest's
/// VALUES outer, the pull form two levels down: the child's site is numbered first, as exec admit walks the fill pipeline
/// ahead of the inner between.
#[test]
fn a_two_level_pull_child_takes_the_walk_order() {
    in_walk_order("regress-full/join-204.plan", &[16403, 25707]);
}

/// join-091 (join#333 / #334): the OUTER nest has a nest parameter ($exec0 = i41.f1, read by the inner nest's qual, not
/// the Material's child): its outer int4_tbl scan held its site number before the inner between was made; the 2-level
/// child takes that number and the outer scan the next. The 1-level Material p1's child is numbered next (the number one
/// level down gives today) after the outer scan's held number: both over int4_tbl, whose every source is a heap range,
/// so exec admit's crossed pair reads the right rows (each recipe is its site's alone).
#[test]
fn an_outer_nest_with_parameters_serves_and_its_heap_ranges_pair_freely() {
    let rel = "regress-full/join-091.plan";
    assert_eq!(rels(&walk(rel)), [16400, 16400, 16400, 16403]);
    assert!(walk_src(rel).iter().all(|e| e.1), "{rel}: every source a heap range");
    assert_eq!(rels(&sites(rel)), [16400, 16400, 16403, 16400]);
    paired(rel, &[]);
}

/// join-144 (join#438): two Materials, one read two levels down (under the LEFT nest over int4_tbl) and one read one
/// level down; the top pipeline's source is a Result (no site).
#[test]
fn two_materials_one_nested() {
    in_walk_order("regress-full/join-144.plan", &[16403, 16400, 16400]);
}

/// m8_d1109_readers' served forms, one relation per level: the pull form two levels down under an outer nest with a nest
/// parameter (p2) and three levels down (p3), the eager form (a Material over a join, itself over a 1-level Material of
/// d19z) two and three levels down (e2, e3), a VOLATILE filter in the pulled child (v2).
#[test]
fn the_pull_and_eager_children_take_the_walk_order_at_two_and_three_levels() {
    in_walk_order("d1919-p2.plan", &[X, O, M]);
    in_walk_order("d1919-p3.plan", &[Y, O, M, N]);
    in_walk_order("d1919-e2.plan", &[Z, X, O, M]);
    in_walk_order("d1919-e3.plan", &[Z, Y, O, M, N]);
    in_walk_order("d1919-v2.plan", &[X, O, M]);
}

/// The child and the top scan over one index (d19x_id) with different keys: exec admit takes an Index source's recipe
/// (its index quals) from the graph source it pairs, so the child's site (no date column) must come first, as the fill
/// pipeline's source does in the walk, and the top scan's (id, k, x) next. Two levels down (w2) and one (w1, d19w). A
/// scan carries only the columns its consumers read (decompose facts/demand.rs `entries_read`), so w1's two scans both
/// read (id, k): only the walk order tells them apart there.
#[test]
fn one_index_two_sites_pair_by_the_walk() {
    in_walk_order("d1919-w2.plan", &[X, X, M]);
    assert_eq!(walk("d1919-w2.plan")[..2], [(X, X_ID, vec![1, 2]), (X, X_ID, vec![1, 2, 3])]);
    in_walk_order("d1919-w1.plan", &[W, W]);
    assert_eq!(walk("d1919-w1.plan"), [(W, W_ID, vec![1, 2]), (W, W_ID, vec![1, 2])]);
}

/// join-278 (join#782 / #783): the Material's child is an index scan keyed by the OUTER nest's parameter ($exec0 =
/// v.x), and m8_d1109_readers' r7 a filter of the child reading the outer's d19o.k. PostgreSQL refills the store per
/// changed parameter (ExecReScanMaterial): the outer nest's rerun set re-runs the raise-free, IMMUTABLE fill, emptied
/// first, at each re-open (D1107's eager refill; before, both refused by D1107's name).
#[test]
fn a_child_reading_the_outer_nests_parameter_takes_the_eager_refill() {
    for rel in ["regress-full/join-278.plan", "d1919-r7.plan"] {
        if let Err(r) = common::lower_fixture(&text(rel)).expect("decomposes") {
            panic!("{rel}: {}", r.what);
        }
    }
}

/// D1109 (the SubPlan link): a Material a nested SubPlan's inner reads passes the reader walk (the inner re-opens per call
/// as a nest's per outer row), so neither join-167 nor foreign_key-051 refuses by D1109's name; join-167's child, no one
/// scan of filters and projections (D1108), takes D1107's eager refill: the SubPlan's one rerun set re-runs it whole.
#[test]
fn a_subplan_link_passes_the_reader_walk() {
    for rel in ["regress-full/join-167.plan", "regress-full/foreign_key-051.plan"] {
        match common::lower_fixture(&text(rel)).expect("decomposes") {
            Ok(_) => eprintln!("{rel}: lowered"),
            Err(r) => {
                eprintln!("{rel}: {}", r.what);
                assert!(!r.what.contains("(D1109)"), "{rel}: {}", r.what);
            }
        }
    }
    if let Err(r) = common::lower_fixture(&text("regress-full/join-167.plan")).expect("decomposes") {
        panic!("regress-full/join-167.plan: {}", r.what);
    }
}

/// D1633: a sibling nest inner (a parameterized index scan) held the owner's site number before the Material's child was
/// numbered, and the child scans the sibling's index (q2) or the owner's (a1; q3 beside a Material two levels down; b2
/// with that Material's child over the owner's index too): the child takes the owner's held number and the owner the next,
/// so each index site pairs its own source (the owner's scan reads every column, PostgreSQL's physical target list, the
/// children theirs only). h2: join-091's shape over heap ranges only.
#[test]
fn a_child_after_a_sibling_inner_pairs_its_own_source() {
    assert_eq!(rels(&walk("d1919-q2.plan")), [W, O, W]);
    paired("d1919-q2.plan", &[(W, W_ID)]);
    assert_eq!(rels(&walk("d1919-a1.plan")), [W, W, U]);
    paired("d1919-a1.plan", &[(W, W_ID)]);
    assert_eq!(rels(&walk("d1919-q3.plan")), [Y, W, W, M]);
    paired("d1919-q3.plan", &[(W, W_ID)]);
    assert_eq!(rels(&walk("d1919-b2.plan")), [W, W, W, M]);
    paired("d1919-b2.plan", &[(W, W_ID)]);
    assert!(walk_src("d1919-h2.plan").iter().all(|e| e.1));
    paired("d1919-h2.plan", &[]);
}

/// Two levels down, the shift over two held owners (review r2's probe A): the top an index-only scan of d19w_id (s2: id
/// only), the inner between (d19m, a heap scan) held for its d19w_id sibling (id, k, x), and the Material child over
/// d19w_id (id, k): the child takes the first held number, the top the inner's, the inner a fresh one. b7 (probe B7): the
/// top an index-only scan, a range inner keyed by it and the child, all over d19w_id. Every site pairs its own scan.
#[test]
fn a_two_level_shift_over_held_owners_pairs_every_site() {
    assert_eq!(rels(&walk("d1919-s2.plan")), [W, W, M, W]);
    paired("d1919-s2.plan", &[(W, W_ID)]);
    assert_eq!(rels(&walk("d1919-b7.plan")), [W, W, W]);
    paired("d1919-b7.plan", &[(W, W_ID)]);
}

/// D1633's refusal ("does not apply" to the forms above): a sibling inner over the owner's own index (a3: the shift
/// would put the owner after it, the next number would put the child after the owner), and two levels down the inner
/// between (a d19w_id range) holding its number for its d19w_id sibling (b8, probe B8: the shift would move the inner past
/// its sibling, the next number pairs the child with the inner's scan). An owner holds a number of the child's key in
/// both, so no pin serves them.
#[test]
fn a_child_no_number_pairs_refuses_d1633() {
    for rel in ["d1919-a3.plan", "d1919-b8.plan"] {
        let r = refusal(rel);
        assert!(r.contains("(D1633)"), "{rel}: {r}");
    }
}

/// D3769: an owner without a site whose index inner over d19w_id (u, keyed by the outer's max) is numbered before the
/// child over the same index (x2, a constant range): no number pairs the child by the walk order and no owner holds one of
/// that key, so the inner's site and the child's are pinned to their own sources (two distinct d19w_id scans), which
/// exec admit pairs first.
#[test]
fn a_child_after_an_inner_of_its_key_is_pinned_to_its_own_source() {
    let rel = "d1919-g1.plan";
    let nodes = paired_pinned(rel);
    assert_eq!(nodes.len(), 2, "{rel}: pins {nodes:?}");
    assert_ne!(nodes[0], nodes[1], "{rel}: pins {nodes:?}");
    let w = walk_nodes(rel);
    assert!(nodes.iter().all(|n| w.iter().any(|e| e.2 == *n && (e.0 .0, e.0 .1) == (W, W_ID))), "{rel}: pins {nodes:?} over {w:?}");
}

/// D1637 in-row (B-R1): where B-R1 serves, a child whose index key is an expression over no executor parameter (a
/// STABLE call here) refuses by name, as its key would bind once at bind where PostgreSQL evaluates it at the child's
/// first scan only: two levels down (k4, review r2's k4) and one level down at the shift (k6: a1's shape). Does not apply
/// (served above): a constant key at the same places (p2, a1). One level down at D1633's number (review r2's k2) the form
/// is trunk's own: D1637 filed, DEFERRED to the bug phase, not pinned here.
#[test]
fn an_expression_key_where_b_r1_serves_refuses_d1637() {
    for rel in ["d1919-k4.plan", "d1919-k6.plan"] {
        let r = refusal(rel);
        assert!(r.contains("(D1637)"), "{rel}: {r}");
    }
}

/// D1630 in-row (B-R1): where B-R1 serves, an inner of the graph whose walk key is an expression over no executor
/// parameter refuses by D1630's name, as exec admit binds it once at bind where PostgreSQL evaluates it at that inner's
/// scan only (never under an empty outer): m1, b7's shape with the inner between's constant bound a STABLE call. Does not
/// apply (served above): b7 itself, the inner between keyed by the outer nest's parameter and a constant. One level down
/// the class is trunk's own (D1630's row), not pinned here.
#[test]
fn an_inner_walk_bound_at_bind_where_b_r1_serves_refuses_d1630() {
    let r = refusal("d1919-m1.plan");
    assert!(r.contains("D1630)"), "d1919-m1.plan: {r}");
}
