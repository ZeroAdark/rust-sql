//! C3: gravity_deps records on the lowering's own thread, so every catalog read
//! made for a lowering must happen there. Decompose and lowering hand no work to another thread (a later need propagates
//! the recording, and this lint names it first). The code generator, which the tier-0 compile pool runs on another thread,
//! runs inside `gravity_deps::catalog_free` wherever it is called: a catalog read there is counted, and check mode raises it.
use std::fs;
use std::path::PathBuf;

use gravity_lint::*;

const LOWERING_CRATES: &[&str] = &["decompose", "lower", "lowerv4", "fn", "expr"];
const HANDOFFS: &[&str] = &["thread::spawn", "thread::scope", "thread::Builder", "rayon::", "gravity_pool::"];
const CODEGEN: &[&str] = &["gravity_tpde::compile(", "gravity_tpde::compile_owned(", "gravity_tpde::compile_tagged(", "gravity_wasm::compile_entries("];

/// The lines of `text` outside `#[cfg(test)]` items (brace-matched from the attribute's item).
fn production_lines(text: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let (mut skipping, mut depth, mut opened) = (false, 0i64, false);
    for (i, line) in text.lines().enumerate() {
        if !skipping && line.trim_start().starts_with("#[cfg(test)]") {
            (skipping, depth, opened) = (true, 0, false);
        }
        if skipping {
            for c in line.chars() {
                match c {
                    '{' => (depth, opened) = (depth + 1, true),
                    '}' => depth -= 1,
                    _ => {}
                }
            }
            if opened && depth <= 0 || !opened && line.trim_end().ends_with(';') && !line.contains("#[cfg(test)]") {
                skipping = false;
            }
            continue;
        }
        let code = line.split("//").next().unwrap_or("");
        out.push((i + 1, code));
    }
    out
}

#[test]
fn lowering_hands_no_work_to_another_thread() {
    let mut failures = Vec::new();
    for name in LOWERING_CRATES {
        for path in rust_files(&gravity_root().join(name).join("src")) {
            let text = fs::read_to_string(&path).unwrap();
            for (n, code) in production_lines(&text) {
                if let Some(h) = HANDOFFS.iter().find(|h| code.contains(*h)) {
                    failures.push(format!("{}:{n}: `{h}` (propagate gravity_deps' recording to that thread, or fail closed)", rel(&path)));
                }
            }
        }
    }
    assert!(failures.is_empty(), "a lowering crate hands work to another thread:\n{}", failures.join("\n"));
}

#[test]
fn production_lines_skip_test_items() {
    let text = "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn b() { std::thread::spawn(|| {}); }\n}\nfn c() { std::thread::scope(|_| {}); }\n";
    let lines: Vec<usize> = production_lines(text).into_iter().map(|e| e.0).collect();
    assert_eq!(lines, vec![1, 6]);
}

/// The production lines of every `src/` file of the gravity crates but this one.
fn gravity_sources() -> Vec<(PathBuf, String)> {
    rust_files(&gravity_root())
        .into_iter()
        .filter(|p| {
            let r = rel(p);
            r.contains("/src/") && !r.starts_with("crates/gravity/lint/")
        })
        .map(|p| {
            let text = fs::read_to_string(&p).unwrap();
            (p, text)
        })
        .collect()
}

/// The text of the function holding line `n` (1-based), from its `fn` line.
fn enclosing_fn(lines: &[(usize, &str)], n: usize) -> String {
    let at = lines.iter().position(|e| e.0 == n).unwrap();
    let from = (0..=at).rev().find(|&i| lines[i].1.contains("fn ")).unwrap_or(0);
    lines[from..=at].iter().map(|e| e.1).collect::<Vec<_>>().join("\n")
}

#[test]
fn the_code_generator_runs_catalog_free() {
    let mut failures = Vec::new();
    let mut calls = 0;
    for (path, text) in gravity_sources() {
        let lines = production_lines(&text);
        for &(n, code) in &lines {
            if !CODEGEN.iter().any(|c| code.contains(c)) {
                continue;
            }
            calls += 1;
            let via_cache = rel(&path) == "crates/gravity/backend/src/lib.rs" && enclosing_fn(&lines, n).contains("cache::get_or_compile_");
            if !code.contains("gravity_deps::catalog_free(") && !via_cache {
                failures.push(format!("{}:{n}: a code generator call outside `gravity_deps::catalog_free` and the code cache's compile", rel(&path)));
            }
        }
    }
    let cache = gravity_root().join("backend/src/cache.rs");
    let text = fs::read_to_string(&cache).unwrap();
    let invokes: Vec<(usize, &str)> = production_lines(&text)
        .into_iter()
        .filter(|(_, code)| code.match_indices("compile(").any(|(i, _)| i == 0 || !code.as_bytes()[i - 1].is_ascii_alphanumeric() && code.as_bytes()[i - 1] != b'_'))
        .collect();
    if invokes.len() != 1 || !invokes[0].1.contains("gravity_deps::catalog_free(|| compile(") {
        failures.push(format!("{}: the code cache calls its compile outside one `gravity_deps::catalog_free`: {invokes:?}", rel(&cache)));
    }
    assert!(calls >= 4, "the scan found the code generator's callers ({calls})");
    assert!(failures.is_empty(), "the code generator can read the catalog off the lowering's recording:\n{}", failures.join("\n"));
}
