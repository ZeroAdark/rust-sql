// D3681: a top-level UPDATE / DELETE over twin result relations reads them as one relation set (ops/append.rs `relsets`,
// modifytable/mod.rs `set_write`) only while the admission groups relation sets (`admit::grouping_relsets`, exec's
// `pgrust.gravity_relset`). Over the corpus, every pair of heap relations of equal captured attributes a twin
// (`FixtureFacts::twins`): grouping off, no write reads a relation set and a write over several result relations keeps one
// arm per target; grouping on, a write that reads a set reads every target through it, in the targets' order, from one
// pipeline, where grouping off gave it one arm per target.
use std::fs;
use std::path::PathBuf;

use gravity_decompose::graph::{Op, PipelineGraph, Source};
use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::{admit, corpus, decompose_in};
use gravity_program::{Arena, Cmd};
use mcx::MemoryContext;

/// The checkout under test, read at run time (D793-b: never the compile-time path).
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}

fn plans() -> Vec<PathBuf> {
    let root = manifest_dir().join("tests/plans");
    let mut v: Vec<PathBuf> = [root.clone(), root.join("regress-full")].iter().flat_map(|d| fs::read_dir(d).unwrap().map(|e| e.unwrap().path())).filter(|p| p.extension().is_some_and(|e| e == "plan")).collect(); // alloc: Cold
    v.sort();
    v
}

/// Each write op of `g` as (its command, its targets' relids, the relation set its pipeline's source reads (empty: none),
/// whether the pipeline is an Append's arm).
fn writes(g: &PipelineGraph<'_>) -> Vec<(Cmd, Vec<u32>, Vec<u32>, bool)> {
    let mut out = Vec::new(); // alloc: Cold
    for p in g.pipelines.iter() {
        let Some(w) = p.ops.iter().find_map(|o| if let Op::Write(w) = o { Some(w) } else { None }) else { continue };
        let set = match &p.source {
            Source::Range { site, .. } => site.set.iter().copied().collect(), // alloc: Cold
            _ => Vec::new(), // alloc: Cold
        };
        let arm = p.ops.iter().any(|o| matches!(o, Op::Append(AppendDesc::Arm { .. })));
        out.push((w.cmd, w.targets.iter().map(|t| t.relid).collect(), set, arm)); // alloc: Cold
    }
    out
}

#[test]
fn relation_set_writes_only_while_grouping() {
    let arena = Arena::new(1 << 24);
    let (mut sets, mut several) = (0usize, 0usize);
    for path in plans() {
        let text = fs::read_to_string(&path).unwrap();
        let ctx = MemoryContext::new("fixture");
        let mut fx = corpus::load(ctx.mcx(), &text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        fx.facts.twins = true;
        let h = arena.handle();
        let m = h.mark();
        let off = admit::grouping_relsets(false, || decompose_in(h, fx.stmt, &fx.facts, fx.mcx).map(|g| writes(&g)));
        h.reset_to(m);
        let on = admit::grouping_relsets(true, || decompose_in(h, fx.stmt, &fx.facts, fx.mcx).map(|g| writes(&g)));
        h.reset_to(m);
        let (Ok(off), Ok(on)) = (off, on) else { continue };
        for (cmd, targets, set, _) in off.iter() {
            assert!(set.is_empty(), "{}: a {cmd:?} reads a relation set with the grouping off", path.display());
            several += (targets.len() > 1) as usize;
        }
        for (cmd, targets, set, arm) in on.iter().filter(|w| !w.2.is_empty()) {
            assert!(matches!(cmd, Cmd::Update | Cmd::Delete), "{}: a {cmd:?} over a relation set", path.display());
            assert_eq!(set, targets, "{}: a write's relation set is not its targets in order", path.display());
            assert!(!arm, "{}: a relation-set write is an Append's arm", path.display());
            let arms = off.iter().filter(|w| w.0 == *cmd && w.1 == *targets && w.3).count();
            assert_eq!(arms, targets.len(), "{}: grouping off, the write over the set's {} targets has {arms} arms", path.display(), targets.len());
            assert_eq!(on.iter().filter(|w| w.0 == *cmd && w.1 == *targets).count(), 1, "{}: a relation-set write beside other pipelines writing its targets", path.display());
            sets += 1;
        }
    }
    assert!(several > 0, "the corpus holds no write over several result relations");
    assert!(sets > 0, "no corpus write over twin result relations read them as a relation set");
}
