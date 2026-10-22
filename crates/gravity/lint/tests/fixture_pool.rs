//! The fixture pool stays test support (D238; main 09-24): gravity_parallel's `fixture-pool` feature compiles FixturePool
//! (src/test/fixture_pool.rs, a raw-thread pool outside the spawn door) and only DEV-dependencies may enable it. A normal or
//! build dependency edge, or another crate's feature that forwards to it, would link the pool into a server build while the
//! determinism lint skips its file as test code. This walks every Cargo.toml under crates/ and fails on any such edge.
use std::fs;
use std::path::{Path, PathBuf};

use gravity_lint::{gravity_root, rel};

fn manifests(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if p.is_dir() {
            if name != "target" && !name.starts_with('.') && name != "postgres-18.6-reference" {
                manifests(&p, out);
            }
        } else if name == "Cargo.toml" {
            out.push(p);
        }
    }
}

/// `file:line: [section] line` for every mention of `fixture-pool` outside a dev-dependencies table, except the feature's
/// own definition in gravity_parallel's `[features]`.
fn normal_edges(crates: &Path) -> Vec<String> {
    let mut files = Vec::new();
    manifests(crates, &mut files);
    files.sort();
    let own = crates.join("gravity/parallel/Cargo.toml");
    let mut bad = Vec::new();
    for f in files {
        let mut section = String::new();
        for (i, line) in fs::read_to_string(&f).unwrap_or_default().lines().enumerate() {
            let t = line.trim();
            if t.starts_with('[') {
                section = t.to_string();
                continue;
            }
            if t.starts_with('#') || !t.contains("fixture-pool") {
                continue;
            }
            let dev = section == "[dev-dependencies]" || section.ends_with(".dev-dependencies]");
            let definition = f == own && section == "[features]" && t.starts_with("fixture-pool");
            if !dev && !definition {
                bad.push(format!("{}:{}: {section} {t}", rel(&f), i + 1));
            }
        }
    }
    bad
}

#[test]
fn fixture_pool_is_enabled_only_by_dev_dependencies() {
    let crates = gravity_root().parent().expect("the crates directory").to_path_buf();
    let bad = normal_edges(&crates);
    assert!(bad.is_empty(), "gravity_parallel's fixture-pool reaches a non-dev edge (it would link FixturePool into a server build):\n{}", bad.join("\n"));
}

#[test]
fn a_normal_edge_or_a_forwarding_feature_is_caught() {
    let root = std::env::temp_dir().join(format!("gravity-lint-fixture-pool-{}", std::process::id()));
    let write = |p: &str, text: &str| {
        let f = root.join(p);
        fs::create_dir_all(f.parent().unwrap()).unwrap();
        fs::write(f, text).unwrap();
    };
    write("gravity/parallel/Cargo.toml", "[package]\nname = \"gravity_parallel\"\n\n[dev-dependencies]\ngravity_parallel = { path = \".\", features = [\"fixture-pool\"] }\n\n[features]\nfixture-pool = []\n");
    write("gravity/ok/Cargo.toml", "[dev-dependencies]\ngravity_parallel = { path = \"../parallel\", features = [\"fixture-pool\"] }\n");
    write("gravity/bad/Cargo.toml", "[dependencies]\ngravity_parallel = { path = \"../parallel\", features = [\"fixture-pool\"] }\n");
    write("gravity/fwd/Cargo.toml", "[features]\nlab = [\"gravity_parallel/fixture-pool\"]\n");
    let bad = normal_edges(&root);
    let _ = fs::remove_dir_all(&root);
    assert_eq!(bad.len(), 2, "{bad:?}");
    assert!(bad[0].contains("gravity/bad/Cargo.toml:2: [dependencies]"), "{bad:?}");
    assert!(bad[1].contains("gravity/fwd/Cargo.toml:2: [features]"), "{bad:?}");
}
