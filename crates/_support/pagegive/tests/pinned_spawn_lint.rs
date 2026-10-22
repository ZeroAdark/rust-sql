
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    let dir = std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo test sets CARGO_MANIFEST_DIR");
    Path::new(&dir).join("../../..").canonicalize().expect("repo root")
}

fn production_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    let mut ents: Vec<_> = rd.filter_map(Result::ok).map(|e| e.path()).collect();
    ents.sort();
    for p in ents {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        if p.is_dir() {
            if !matches!(name.as_str(), "tests" | "test" | "benches" | "examples" | "target") {
                production_files(&p, out);
            }
        } else if name.ends_with(".rs")
            && !matches!(name.as_str(), "tests.rs" | "test.rs" | "build.rs")
            && !name.ends_with("_test.rs")
            && !name.ends_with("_tests.rs")
        {
            out.push(p);
        }
    }
}

fn strip(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let (mut i, mut block) = (0usize, 0usize);
    while i < b.len() {
        let c = b[i];
        if block > 0 {
            if b[i..].starts_with(b"*/") {
                block -= 1;
                i += 2;
            } else if b[i..].starts_with(b"/*") {
                block += 1;
                i += 2;
            } else {
                if c == b'\n' {
                    out.push('\n');
                }
                i += 1;
            }
            continue;
        }
        if b[i..].starts_with(b"//") {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b[i..].starts_with(b"/*") {
            block = 1;
            i += 2;
            continue;
        }
        if c == b'r' && (b.get(i + 1) == Some(&b'"') || (b.get(i + 1) == Some(&b'#') && b.get(i + 2).is_some_and(|&x| x == b'"' || x == b'#')))
            && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_'))
        {
            let mut j = i + 1;
            let mut hashes = 0;
            while b.get(j) == Some(&b'#') {
                hashes += 1;
                j += 1;
            }
            if b.get(j) == Some(&b'"') {
                j += 1;
                out.push_str("\"\"");
                loop {
                    if j >= b.len() {
                        break;
                    }
                    if b[j] == b'"' && b[j + 1..].iter().take(hashes).filter(|&&x| x == b'#').count() == hashes {
                        j += 1 + hashes;
                        break;
                    }
                    if b[j] == b'\n' {
                        out.push('\n');
                    }
                    j += 1;
                }
                i = j;
                continue;
            }
        }
        if c == b'"' {
            out.push_str("\"\"");
            i += 1;
            while i < b.len() && b[i] != b'"' {
                if b[i] == b'\\' {
                    i += 1;
                }
                if i < b.len() && b[i] == b'\n' {
                    out.push('\n');
                }
                i += 1;
            }
            i += 1;
            continue;
        }
        if c == b'\'' {
            if b.get(i + 1) == Some(&b'\\') {
                if let Some(k) = b[i + 2..].iter().take(12).position(|&x| x == b'\'') {
                    out.push_str("' '");
                    i += k + 3;
                    continue;
                }
            } else if b.get(i + 2) == Some(&b'\'') {
                out.push_str("' '");
                i += 3;
                continue;
            } else if let Some(k) = std::str::from_utf8(&b[i + 1..(i + 6).min(b.len())]).ok().and_then(|s| {
                let mut it = s.char_indices();
                let (_, ch) = it.next()?;
                let (n, q) = it.next()?;
                (q == '\'' && !ch.is_ascii()).then_some(n)
            }) {
                out.push_str("' '");
                i += k + 2;
                continue;
            }
        }
        let w = match c {
            0x00..=0x7f => 1,
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            _ => 4,
        };
        out.push_str(std::str::from_utf8(&b[i..(i + w).min(b.len())]).unwrap_or(" "));
        i += w;
    }
    out
}

fn is_test_attr(line: &str) -> bool {
    let l: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    if l.contains("#[test]") || l.contains("#[cfg(test)]") {
        return true;
    }
    // #[cfg(all(..)) with a bare `test` among its arguments (a production build never has it)
    if let Some(p) = l.find("#[cfg(all(") {
        let args = &l[p + 10..];
        let bytes = args.as_bytes();
        let mut k = 0;
        while let Some(q) = args[k..].find("test") {
            let s = k + q;
            let before = if s == 0 { b'(' } else { bytes[s - 1] };
            let after = bytes.get(s + 4).copied().unwrap_or(b')');
            if matches!(before, b'(' | b',') && matches!(after, b',' | b')') {
                return true;
            }
            k = s + 4;
        }
    }
    false
}

fn production_lines(stripped: &str) -> Vec<String> {
    let mut out = Vec::new();
    let (mut skip, mut pend) = (0i64, false);
    let delta = |l: &str| l.matches('{').count() as i64 - l.matches('}').count() as i64;
    for l in stripped.split('\n') {
        if skip > 0 {
            skip += delta(l);
            out.push(String::new());
            continue;
        }
        if pend {
            if l.contains('{') {
                skip = delta(l).max(0);
                pend = false;
            } else if l.trim_end().ends_with(';') {
                pend = false;
            }
            out.push(String::new());
            continue;
        }
        if is_test_attr(l) {
            if l.contains('{') {
                skip = delta(l).max(0);
            } else {
                pend = true;
            }
            out.push(String::new());
            continue;
        }
        out.push(l.to_string());
    }
    out
}

fn unpinned(lines: &[String]) -> Vec<usize> {
    let text = lines.join("\n");
    let b = text.as_bytes();
    let mut found = Vec::new();
    for name in ["spawn(", "spawn_scoped("] {
        let mut k = 0;
        while let Some(q) = text[k..].find(name) {
            let s = k + q;
            k = s + name.len();
            if s > 0 && (b[s - 1].is_ascii_alphanumeric() || b[s - 1] == b'_') {
                continue;
            }
            if text[..s].trim_end().ends_with("fn") {
                continue; // a definition
            }
            let (mut depth, mut j) = (1i64, k);
            while j < b.len() && depth > 0 {
                match b[j] {
                    b'(' => depth += 1,
                    b')' => depth -= 1,
                    _ => {}
                }
                j += 1;
            }
            let args = &text[k..j.saturating_sub(1).max(k)];
            if args.trim().is_empty() || args.contains("pagegive::pinned(") {
                continue;
            }
            found.push(text[..s].matches('\n').count() + 1);
        }
    }
    found.sort_unstable();
    found
}

fn scan(root: &Path) -> (usize, BTreeMap<String, Vec<usize>>) {
    let mut files = Vec::new();
    production_files(&root.join("crates"), &mut files);
    let mut hits = BTreeMap::new();
    for f in &files {
        let Ok(text) = fs::read_to_string(f) else { continue };
        if !text.contains("spawn") {
            continue;
        }
        let lines = production_lines(&strip(&text));
        let u = unpinned(&lines);
        if !u.is_empty() {
            let rel = f.strip_prefix(root).unwrap_or(f).to_string_lossy().replace('\\', "/");
            hits.insert(rel, u);
        }
    }
    (files.len(), hits)
}

fn allowlist(root: &Path) -> BTreeMap<String, usize> {
    let p = root.join("crates/_support/pagegive/tests/pinned-spawn.allow");
    let text = fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    let mut m = BTreeMap::new();
    for (n, l) in text.lines().enumerate() {
        if l.trim().is_empty() || l.starts_with('#') {
            continue;
        }
        let w: Vec<&str> = l.split('\t').collect();
        assert!(w.len() == 3 && !w[2].trim().is_empty(), "pinned-spawn.allow:{}: want path TAB count TAB reason", n + 1);
        let c: usize = w[1].parse().unwrap_or_else(|_| panic!("pinned-spawn.allow:{}: bad count", n + 1));
        assert!(m.insert(w[0].to_string(), c).is_none(), "pinned-spawn.allow:{}: duplicate path", n + 1);
    }
    m
}

#[test]
fn every_production_spawn_is_pinned() {
    let root = repo_root();
    let (nfiles, hits) = scan(&root);
    assert!(nfiles > 1000, "scanned only {nfiles} files under {}", root.display());
    let allow = allowlist(&root);
    let mut bad = Vec::new();
    for (path, lines) in &hits {
        match allow.get(path) {
            Some(&n) if n == lines.len() => {}
            Some(&n) => bad.push(format!("{path}: {} unpinned spawn call(s) (lines {lines:?}), allowlist budget {n}", lines.len())),
            None => bad.push(format!("{path}: unpinned spawn call(s) at lines {lines:?}")),
        }
    }
    for (path, n) in &allow {
        if !hits.contains_key(path) {
            bad.push(format!("{path}: stale allowlist row (budget {n}, 0 unpinned spawn calls): delete it"));
        }
    }
    assert!(
        bad.is_empty(),
        "every thread pgrust starts must run its body through pagegive::pinned (CVE-2026-74674; pagegive's module doc): \
         spawn(pagegive::pinned(body)). A call that starts no server thread goes in \
         crates/_support/pagegive/tests/pinned-spawn.allow with its reason.\n{}",
        bad.join("\n")
    );
}

#[test]
fn scanner_cases() {
    let src = r##"
fn a() {
    std::thread::spawn(move || work());
    std::thread::Builder::new()
        .name("x".into())
        .spawn(pagegive::pinned(move || {
            work()
        }))
        .unwrap();
    std::thread::scope(|s| {
        s.spawn(|| work());
        s.spawn(pagegive::pinned(|| work()));
    });
    let _c = std::process::Command::new("ls").spawn();
    let _s = "thread::spawn(x)"; // spawn(y)
    /* spawn(z) */
    pub fn spawn(f: F) {}
    respawn(x);
    let _q = '(';
}
#[cfg(test)]
mod tests {
    fn t() { std::thread::spawn(|| 1); }
}
#[cfg(all(test, not(loom)))]
fn u() {
    std::thread::spawn(|| 1);
}
#[cfg(not(test))]
fn v() { std::thread::spawn(|| 1); }
#[test]
fn w() {
    std::thread::spawn(|| 1);
}
"##;
    let lines = production_lines(&strip(src));
    assert_eq!(unpinned(&lines), vec![3, 11, 30]);
}
