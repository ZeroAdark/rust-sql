//! D793-b: no test or tool under crates/gravity or crates/_support reads the checkout through `env!("CARGO_MANIFEST_DIR")`,
//! the path baked in at compile time. A test binary reused from another checkout's build then reads THAT checkout's files
//! (D793: kernel_boundary scanned the wrong tree and passed on M6-HOSTS's H2, 09-26). Read `CARGO_MANIFEST_DIR` at run time
//! instead (`std::env::var_os`, as cargo sets it for every test process: the checkout under test), the way gravity_lint's
//! `lint_dir` and the `manifest_dir()` helpers do. `include_str!` / `include_bytes!` are compile inputs cargo tracks: not
//! this class. The one allowed site is gravity_lint's `lint_dir`, which passes the build's path to `root_from` only to name
//! it in a message.
use std::path::{Path, PathBuf};

use gravity_lint::gravity_root;

/// `relative path:line` of every `.rs` line under `dir` that names the compile-time manifest dir, minus the allowed site.
fn baked_sites(crates: &Path, dir: &Path, out: &mut Vec<String>) {
    let needle = concat!("env!(\"CARGO_", "MANIFEST_DIR\")");
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())).map(|e| e.unwrap().path()).collect();
    entries.sort();
    for p in entries {
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        if p.is_dir() {
            if name != "target" && !name.starts_with('.') {
                baked_sites(crates, &p, out);
            }
        } else if name.ends_with(".rs") && name != "build.rs" {
            let rel = p.strip_prefix(crates).unwrap().to_string_lossy().replace('\\', "/");
            let text = std::fs::read_to_string(&p).unwrap_or_default();
            for (i, l) in text.lines().enumerate() {
                if l.contains(needle) && !l.trim_start().starts_with("//") && !(rel == "gravity/lint/src/lib.rs" && l.contains("root_from(")) {
                    out.push(format!("crates/{rel}:{}: {}", i + 1, l.trim()));
                }
            }
        }
    }
}

#[test]
fn no_test_reads_the_checkout_through_the_compile_time_manifest_dir() {
    let crates = gravity_root().parent().unwrap().to_path_buf();
    let mut sites = Vec::new();
    for sub in ["gravity", "_support"] {
        baked_sites(&crates, &crates.join(sub), &mut sites);
    }
    assert!(
        sites.is_empty(),
        "{} site(s) read the checkout through the compile-time CARGO_MANIFEST_DIR (D793-b): read it at run time, \
         std::env::var_os(\"CARGO_MANIFEST_DIR\") (a manifest_dir() helper):\n{}",
        sites.len(),
        sites.join("\n")
    );
}

#[test]
fn the_scan_finds_a_baked_site() {
    let d = std::env::temp_dir().join(format!("gravity-lint-d793b-{}", std::process::id()));
    let sub = d.join("gravity/x/tests");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("t.rs"), concat!("// a comment naming env!(\"CARGO_", "MANIFEST_DIR\") reads nothing\nfn f() { let _ = std::path::Path::new(env!(\"CARGO_", "MANIFEST_DIR\")); }\n")).unwrap();
    std::fs::write(sub.join("build.rs"), concat!("fn main() { let _ = env!(\"CARGO_", "MANIFEST_DIR\"); }\n")).unwrap();
    let mut sites = Vec::new();
    baked_sites(&d, &d.join("gravity"), &mut sites);
    let _ = std::fs::remove_dir_all(&d);
    assert_eq!(sites.len(), 1, "{sites:?}");
    assert!(sites[0].starts_with("crates/gravity/x/tests/t.rs:2: "), "{sites:?}");
}
