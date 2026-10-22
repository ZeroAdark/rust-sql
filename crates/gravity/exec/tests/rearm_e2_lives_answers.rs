//! rearm E2: `State` asks a live only the questions its `Live::answers` declares (ops/mod.rs `ask`), so each rescan's
//! per-member questions walk only the lives that answer them. An `impl Live` that declares `answers` names exactly the
//! `Live` methods it writes past their defaults: a method added without its bit would never be asked (wrong), a bit without
//! its method costs a call for nothing. An impl without `answers` takes the default, every question (correct, only not
//! narrowed): it is listed, never failed, so a live another change adds lands without this test first (rearm#1873 was
//! ejected when a new live landed beside it).
use std::path::{Path, PathBuf};

const BITS: [(&str, &str); 12] = [
    ("claims_for", "CLAIMS"),
    ("merge_for", "MERGE"),
    ("after_pipeline", "AFTER"),
    ("end", "END"),
    ("deferred", "DEFERRED"),
    ("frame", "FRAME"),
    ("rerun_owner", "RERUN_OWNER"),
    ("pull_readers", "PULL_READERS"),
    ("drain_at_end", "DRAIN"),
    ("late_words", "LATE"),
    ("pruned_whole", "PRUNED"),
    ("reopen", "REOPEN"),
];

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).expect("the crate's src") {
        let p = e.expect("a dir entry").path();
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

#[test]
fn every_declared_live_answers_exactly_the_questions_it_writes() {
    let mut files = Vec::new();
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR for a test process (D793-b: the checkout under test, read at run time)");
    rs_files(&Path::new(&manifest).join("src"), &mut files);
    let (mut impls, mut bad, mut undeclared) = (0, Vec::new(), Vec::new());
    for f in files {
        let text = std::fs::read_to_string(&f).expect("a source file");
        let lines: Vec<&str> = text.lines().collect();
        let mut i = 0;
        while i < lines.len() {
            let l = lines[i];
            if !(l.starts_with("impl") && (l.contains(" Live for ") || l.contains("::Live for "))) {
                i += 1;
                continue;
            }
            let (mut depth, mut j, mut fns, mut decl) = (0i32, i, Vec::new(), None);
            loop {
                let t = lines[j];
                if depth == 1 {
                    if let Some(name) = t.trim_start().strip_prefix("fn ").and_then(|r| r.split('(').next()) {
                        if name == "answers" {
                            decl = lines.get(j + 1).map(|d| d.trim().to_string());
                        }
                        fns.push(name.to_string());
                    }
                }
                depth += t.matches('{').count() as i32 - t.matches('}').count() as i32;
                if depth <= 0 {
                    break;
                }
                j += 1;
            }
            impls += 1;
            let mut want: Vec<&str> = BITS.iter().filter(|(m, _)| fns.iter().any(|f| f == m)).map(|(_, b)| *b).collect();
            let mut got: Vec<String> = decl.as_deref().map_or_else(Vec::new, |d| d.split('|').map(|x| x.trim().trim_start_matches("crate::ops::ask::").to_string()).filter(|x| x != "NONE").collect());
            want.sort_unstable();
            got.sort_unstable();
            match decl {
                None => undeclared.push(format!("{}:{}: {} (asked every question; it writes {:?})", f.display(), i + 1, l.trim(), want)),
                Some(_) if want != got => bad.push(format!("{}:{}: {} declares {:?}, implements {:?}", f.display(), i + 1, l.trim(), decl, want)),
                Some(_) => {}
            }
            i = j + 1;
        }
    }
    if !undeclared.is_empty() {
        eprintln!("lives with no `answers` (the default: asked every question):\n{}", undeclared.join("\n"));
    }
    assert!(impls >= 32, "found {impls} impls of Live (the scan missed some)");
    assert!(bad.is_empty(), "lives whose `answers` differs from their methods:\n{}", bad.join("\n"));
}
