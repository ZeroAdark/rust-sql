//! D4685 (CVE-2026-74674): no raw `madvise(MADV_DONTNEED)` in the tree outside the sites that can never cover a whole
//! 2 MiB-aligned range. Everything else gives pages back through `pagegive` (a whole 2 MiB-aligned range is mapped over
//! afresh, the rest split).

use std::path::{Path, PathBuf};

/// Paths under crates/, each with why its MADV_DONTNEED can never cover a whole 2 MiB-aligned range.
const ALLOWED: &[(&str, &str)] = &[
    ("_support/pagegive/src/lib.rs", "the helper: split pieces, or ranges holding no whole 2 MiB-aligned range"),
    ("_support/allocarena/src/lib.rs", "split pieces that never hold a whole 2 MiB-aligned range (its decommit remaps a whole range)"),
    ("_support/allocarena/src/own.rs", "runs of at most 512 KiB"),
];

fn code_of(line: &str) -> String {
    let mut out = String::new();
    let mut chars = line.chars().peekable();
    let mut in_str = false;
    while let Some(c) = chars.next() {
        if in_str {
            if c == '\\' {
                chars.next();
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        if c == '"' {
            in_str = true;
            continue;
        }
        if c == '/' && chars.peek() == Some(&'/') {
            break;
        }
        out.push(c);
    }
    out
}

fn violations(text: &str) -> Vec<usize> {
    let mut v = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let code = code_of(line);
        let compact: String = code.chars().filter(|c| !c.is_whitespace()).collect();
        if code.contains("MADV_DONTNEED") || (compact.contains("madvise(") && (compact.ends_with(",4)") || compact.contains(",4)}"))) {
            v.push(i + 1);
        }
    }
    v
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name();
        if p.is_dir() {
            if name != "target" && !name.to_string_lossy().starts_with('.') {
                walk(&p, out);
            }
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

#[test]
fn the_scanner_sees_a_seeded_violation() {
    assert_eq!(violations("unsafe { libc::madvise(p, n, libc::MADV_DONTNEED) };"), vec![1]);
    assert_eq!(violations("let x = 1;\n    madvise(lo as *mut c_void, hi - lo, 4)\n"), vec![2]);
    assert!(violations("// MADV_DONTNEED in a comment\nlet s = \"MADV_DONTNEED\";\n/// doc MADV_DONTNEED").is_empty());
}

#[test]
fn no_raw_madv_dontneed_outside_the_allowed_sites() {
    // The checkout under test, read at run time (D793-b): cargo sets CARGO_MANIFEST_DIR for every test process.
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR for the test process");
    let crates = Path::new(&manifest).join("../..").canonicalize().unwrap();
    let mut files = Vec::new();
    walk(&crates, &mut files);
    assert!(files.len() > 1000, "the walk found the tree: {} files under {}", files.len(), crates.display());
    let mut bad = Vec::new();
    for f in files {
        let rel = f.strip_prefix(&crates).unwrap().to_string_lossy().replace('\\', "/");
        if ALLOWED.iter().any(|(p, _)| *p == rel) || rel == "_support/pagegive/tests/no_raw_dontneed.rs" {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&f) else { continue };
        for line in violations(&text) {
            bad.push(format!("crates/{rel}:{line}"));
        }
    }
    assert!(
        bad.is_empty(),
        "raw MADV_DONTNEED (CVE-2026-74674: a whole 2 MiB-aligned range must be mapped over, never DONTNEED'd); give pages \
         back through pagegive::give_back instead:\n  {}",
        bad.join("\n  ")
    );
}
