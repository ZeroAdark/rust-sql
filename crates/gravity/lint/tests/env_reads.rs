//! Landing 18: a Gravity crate at or above the driver reads the process environment only through `gravity_driver::env`
//! (the one reader behind the `PGRUST_GRAVITY_*` knobs), so the knobs are one grep. Scanned: every `src/` file under
//! crates/gravity. Allowed: the reader itself; `CARGO_MANIFEST_DIR` (tests reading the checkout); `#[ignore]` probe
//! tests' inputs (`M3_`); and the two crates below the driver, which cannot call it (kernels, fn).
use std::path::{Path, PathBuf};

use gravity_lint::gravity_root;

const BELOW_DRIVER: &[&str] = &["kernels/", "fn/"];

fn raw_reads(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())).map(|e| e.unwrap().path()).collect();
    entries.sort();
    for p in entries {
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        if p.is_dir() {
            if name != "target" && name != "tests" && name != "examples" && name != "benches" && !name.starts_with('.') {
                raw_reads(root, &p, out);
            }
        } else if name.ends_with(".rs") {
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
            if rel == "driver/src/env.rs" || !rel.contains("/src/") || BELOW_DRIVER.iter().any(|c| rel.starts_with(c)) {
                continue;
            }
            let text = std::fs::read_to_string(&p).unwrap_or_default();
            for (i, l) in text.lines().enumerate() {
                let reads = l.contains(concat!("std::env::", "var(")) || l.contains(concat!("std::env::", "var_os("));
                if reads && !l.trim_start().starts_with("//") && !l.contains("CARGO_MANIFEST_DIR") && !l.contains("\"M3_") {
                    out.push(format!("crates/gravity/{rel}:{}: {}", i + 1, l.trim()));
                }
            }
        }
    }
}

#[test]
fn env_knobs_are_read_through_the_driver_env_reader() {
    let root = gravity_root();
    let mut sites = Vec::new();
    raw_reads(&root, &root, &mut sites);
    assert!(sites.is_empty(), "{} raw env read(s): read through gravity_driver::env (os / set / text / num):\n{}", sites.len(), sites.join("\n"));
}

#[test]
fn the_scan_finds_a_raw_read() {
    let d = std::env::temp_dir().join(format!("gravity-lint-env-{}", std::process::id()));
    std::fs::create_dir_all(d.join("x/src")).unwrap();
    std::fs::create_dir_all(d.join("kernels/src")).unwrap();
    std::fs::write(d.join("x/src/a.rs"), concat!("fn f() -> bool { std::env::", "var_os(\"PGRUST_GRAVITY_X\").is_some() }\n// std::env::", "var(\"Y\")\n")).unwrap();
    std::fs::write(d.join("kernels/src/b.rs"), concat!("fn g() -> bool { std::env::", "var_os(\"Z\").is_some() }\n")).unwrap();
    let mut sites = Vec::new();
    raw_reads(&d, &d, &mut sites);
    let _ = std::fs::remove_dir_all(&d);
    assert_eq!(sites.len(), 1, "{sites:?}");
    assert!(sites[0].contains("x/src/a.rs:1"), "{sites:?}");
}
