// The content key over a write's D1 faces (key.rs `write_faces`): two graphs that differ only in one of
// route / targets / maps / generated / checks-bearing members / old_new / tableoid get different keys.
use std::path::Path;

use gravity_decompose::{corpus, decompose_in, Op, PipelineGraph};
use gravity_decompose::ops::shared::modifytable::{Member, Target, WriteDesc};
use gravity_lower::inputs::key::key_and_nodes;
use gravity_program::Arena;
use mcx::MemoryContext;

fn key(g: &PipelineGraph<'_>) -> u128 {
    key_and_nodes(g, &mut |_| {}).0
}

fn write<'a, 'p>(g: &'a mut PipelineGraph<'p>) -> &'a mut WriteDesc<'p> {
    g.pipelines.iter_mut().flat_map(|p| p.ops.iter_mut()).find_map(|o| if let Op::Write(d) = o { Some(d) } else { None }).unwrap()
}

/// The write's target list is shared by its arms (D2483), so an edit to one target replaces the list with an edited copy.
fn second_target<'p>(d: &mut WriteDesc<'p>, edit: impl FnOnce(&mut Target<'p>)) {
    let mut targets = d.targets.to_vec();
    edit(&mut targets[1]);
    d.targets = Vec::leak(targets);
}

#[test]
fn every_d1_face_of_a_write_is_in_the_key() {
    let path = manifest_dir().join("../decompose/tests/plans/regress-full/alter_table-022.plan");
    let text = std::fs::read_to_string(&path).unwrap();
    let ctx = MemoryContext::new("fixture");
    let fx = corpus::load(ctx.mcx(), &text).unwrap();
    let arena = Arena::new(1 << 20);
    let mut g = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).unwrap();
    assert_eq!(write(&mut g).targets.len(), 2);
    let mut seen = vec![key(&g)];
    let edits: [&dyn Fn(&mut WriteDesc<'_>); 9] = [
        &|d| d.route.as_mut().unwrap().cross_partition = true,
        &|d| d.route.as_mut().unwrap().root_natts += 1,
        &|d| second_target(d, |t| t.to_root[0] = 2),
        &|d| second_target(d, |t| t.generated.push((1, b's'))),
        &|d| second_target(d, |t| t.relid += 1),
        &|d| d.members = d.members.with(Member::CheckRls),
        &|d| d.tableoid_col += 1,
        &|d| d.old_new.old = true,
        &|d| d.route = None,
    ];
    for (i, e) in edits.iter().enumerate() {
        e(write(&mut g));
        let k = key(&g);
        assert!(!seen.contains(&k), "edit {i} left the key unchanged");
        seen.push(k);
    }
}

/// cachekey: a write's expression-free target and leaf facts are one sub-key per (targets, leaves) slice pair of the walk
/// (key.rs `target_facts`, its arms share the lists): equal copies of the lists key as the shared lists do.
#[test]
fn equal_copies_of_a_writes_lists_key_as_the_shared_ones() {
    let path = manifest_dir().join("../decompose/tests/plans/regress-full/alter_table-022.plan");
    let text = std::fs::read_to_string(&path).unwrap();
    let ctx = MemoryContext::new("fixture");
    let fx = corpus::load(ctx.mcx(), &text).unwrap();
    let arena = Arena::new(1 << 20);
    let mut g = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).unwrap();
    let k = key(&g);
    let d = write(&mut g);
    d.targets = Vec::leak(d.targets.to_vec());
    d.leaves = Vec::leak(d.leaves.to_vec());
    assert_eq!(key(&g), k);
    second_target(write(&mut g), |t| t.relid += 1);
    assert_ne!(key(&g), k);
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
