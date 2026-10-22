// The content key over an index scan's keys (key.rs `keys`): two graphs that differ only in one field of one IndexKey get
// different keys. `col` above all: every expression column's table attno is 0, so keys on two expression columns of one
// index differ only in `col` (e1 index keys, main 09-24).
use std::path::Path;

use gravity_decompose::graph::KeyForm;
use gravity_decompose::{corpus, decompose_in, IndexKey, PipelineGraph, Source};
use gravity_lower::inputs::key::key_and_nodes;
use gravity_program::Arena;
use mcx::MemoryContext;

fn key(g: &PipelineGraph<'_>) -> u128 {
    key_and_nodes(g, &mut |_| {}).0
}

fn first_key<'a, 'p>(g: &'a mut PipelineGraph<'p>) -> Option<&'a mut IndexKey<'p>> {
    g.pipelines.iter_mut().find_map(|p| match &mut p.source {
        Source::Index { keys, .. } => keys.iter_mut().next(),
        _ => None,
    })
}

#[test]
fn every_field_of_an_index_key_is_in_the_key() {
    let dir = manifest_dir().join("../decompose/tests/plans/regress-full");
    let mut names: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).filter(|p| p.extension().is_some_and(|x| x == "plan")).collect();
    names.sort();
    let arena = Arena::new(1 << 22);
    for path in names {
        let text = std::fs::read_to_string(&path).unwrap();
        if !text.contains("{INDEXSCAN") || !text.contains(":indexqual ({") {
            continue;
        }
        let ctx = MemoryContext::new("fixture");
        let Ok(fx) = corpus::load(ctx.mcx(), &text) else { continue };
        let Ok(mut g) = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx) else { continue };
        if first_key(&mut g).is_none() {
            continue;
        }
        let mut seen = vec![key(&g)];
        let edits: [&dyn Fn(&mut IndexKey<'_>); 4] = [
            &|k| k.col += 1,
            &|k| k.attno += 1,
            &|k| k.opno += 1,
            &|k| k.form = if k.form == KeyForm::Row { KeyForm::Op } else { KeyForm::Row },
        ];
        for (i, e) in edits.iter().enumerate() {
            e(first_key(&mut g).unwrap());
            let k = key(&g);
            assert!(!seen.contains(&k), "{}: edit {i} left the key unchanged", path.display());
            seen.push(k);
        }
        return;
    }
    panic!("no regress-full fixture decomposes to an index scan with a key");
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
