//! Source lints over v4's files (plan P1 / P3 / P4, the hash ruling 09-23, main's module rule 09-24):
//! - KeyHash only: no v4 file reads raw hash bits. The items of `abi/src/hash.rs` and `expr/src/emit/hash.rs` are parsed at
//!   test time; a v4 file may name only the interface (KeyHash and its methods, the route / load helpers, the IR half's
//!   `emit_*` entry points), never another item of those files, the value of a module-level 64-bit `const` they define (PHI64, the tunables;
//!   literals inside a function body, like `hash_bytes64`'s fmix, are standard mixes anyone may use), `KeyHash(..)` construction or
//!   `.0` on a hash. A mix or load swap then touches hash.rs alone.
//! - Modules: each `registry/m<k>.rs` declares its milestone's files by `#[path]`; a file of milestone k never names
//!   `registry::m<j>` for j != k (CORE files name none); every source file is declared exactly once; no per-directory
//!   `mod.rs` hub beyond the plan's two.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    manifest_dir().join("../../..")
}

fn src() -> PathBuf {
    manifest_dir().join("src")
}

/// Every `.rs` file under `dir`, as paths relative to `dir` with `/` separators, sorted.
fn rs_files(dir: &Path) -> Vec<String> {
    fn walk(d: &Path, base: &Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, base, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p.strip_prefix(base).expect("under base").to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// The text with `//` comments and string / char literal bodies blanked (so a doc line naming an item is not a use).
fn code(text: &str) -> String {
    let (mut out, mut it) = (String::with_capacity(text.len()), text.chars().peekable());
    while let Some(c) = it.next() {
        match c {
            '/' if it.peek() == Some(&'/') => {
                for d in it.by_ref() {
                    if d == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '"' => {
                out.push('"');
                while let Some(d) = it.next() {
                    match d {
                        '\\' => drop(it.next()),
                        '"' => break,
                        '\n' => out.push('\n'),
                        _ => {}
                    }
                }
                out.push('"');
            }
            _ => out.push(c),
        }
    }
    out
}

/// The identifier tokens of `text` with their line numbers.
fn words(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let mut w = String::new();
        for c in line.chars().chain([' ']) {
            if c.is_alphanumeric() || c == '_' {
                w.push(c);
            } else if !w.is_empty() {
                out.push((n + 1, std::mem::take(&mut w)));
            }
        }
    }
    out
}

/// A literal's value when it is an integer of more than 32 bits (`0x9E37_79B9_7F4A_7C15`, `11400714819323198485u64`).
fn big_literal(w: &str) -> Option<u64> {
    let w = w.trim_end_matches("u64").trim_end_matches("i64").replace('_', "");
    let v = match w.strip_prefix("0x").or_else(|| w.strip_prefix("0X")) {
        Some(h) => u64::from_str_radix(h, 16).ok()?,
        None if w.bytes().all(|b| b.is_ascii_digit()) && !w.is_empty() => w.parse().ok()?,
        None => return None,
    };
    (v > u32::MAX as u64).then_some(v)
}

/// The files the hash lint covers: lowerv4's sources and the v4 hub modules of the other crates.
fn v4_files() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = rs_files(&src()).into_iter().map(|f| (format!("crates/gravity/lowerv4/src/{f}"), std::fs::read_to_string(src().join(&f)).expect("read"))).collect();
    for dir in ["crates/gravity/exec/src/v4", "crates/gravity/hosts/src/v4", "crates/gravity/kernels/src/host_abi/v4"] {
        out.extend(rs_files(&root().join(dir)).into_iter().map(|f| (format!("{dir}/{f}"), std::fs::read_to_string(root().join(dir).join(&f)).expect("read"))));
    }
    if let Ok(t) = std::fs::read_to_string(root().join("crates/gravity/program/src/protos.rs")) {
        out.push(("crates/gravity/program/src/protos.rs".into(), t));
    }
    out
}

/// The interface (M0-CORE-B, 09-24): the runtime half's KeyHash (its methods are called on the value), Depth, the route /
/// load / fit helpers and fp128 for image producers; the IR half's key-part / image / bits types and `emit_*` entry points.
/// Anything else a v4 file takes from `gravity_abi::hash` or `gravity_expr::emit::hash` (PHI64, the bit and load constants,
/// siphash128, hash_bytes64) is raw access. `KeyHash::{to_abi, from_abi}` carry the word across the ABI: hosts / exec /
/// kernels / program only, never lowerv4/src (it uses the IR half).
const RUNTIME_API: [&str; 10] = ["KeyHash", "Depth", "table_fits", "route_and_full", "build_cap_bits", "agg_grow_at", "agg_cap_bits", "FpKey", "fp128", "self"];
const IR_API: [&str; 4] = ["KeyPart", "ImageValues", "Bits", "self"];

fn interface(name: &str, ir_half: bool) -> bool {
    if ir_half { name.starts_with("emit_") || IR_API.contains(&name) } else { RUNTIME_API.contains(&name) }
}

/// The names a file takes from the hash module `module` (`gravity_abi::hash` or `gravity_expr::emit::hash`): paths through
/// it, `use` groups from it, a glob (`*`), and `hash::x` paths once the module itself is imported.
fn hash_names(body: &str, module: &str) -> Vec<(usize, String)> {
    let (imported, prefix) = (body.contains(&format!("use {module};")), format!("{module}::"));
    let mut out = Vec::new();
    for (n, line) in body.lines().enumerate() {
        let l = line.replace(" as ", "@").replace(' ', "");
        let paths = l.match_indices(&prefix).map(|(i, _)| i + prefix.len());
        let bare = imported.then(|| l.match_indices("hash::").filter(|(i, _)| !l[..*i].ends_with("::")).map(|(i, _)| i + 6).collect::<Vec<_>>());
        for at in paths.chain(bare.into_iter().flatten()) {
            let rest = &l[at..];
            let names: Vec<String> = match rest.strip_prefix('{') {
                Some(g) => g.split('}').next().unwrap_or("").split(',').map(|x| x.split('@').next().unwrap_or("").to_owned()).collect(),
                None => vec![rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '*').collect()],
            };
            out.extend(names.into_iter().filter(|x| !x.is_empty()).map(|x| (n + 1, x)));
        }
    }
    out
}

/// The 64-bit values of the two hash files' module-level `const` items (PHI64 and any tunable beside it): a v4 file writing
/// one copies the key hash past KeyHash, and a retune of hash.rs would miss it.
fn hash_literals() -> BTreeSet<u64> {
    let mut lits = BTreeSet::new();
    for f in ["crates/gravity/abi/src/hash.rs", "crates/gravity/expr/src/emit/hash.rs"] {
        match std::fs::read_to_string(root().join(f)) {
            Ok(t) => t.lines().filter(|l| l.starts_with("const ") || l.starts_with("pub const ") || l.starts_with("pub(crate) const "))
                .flat_map(|l| words(l)).filter_map(|(_, w)| big_literal(&w)).for_each(|v| drop(lits.insert(v))),
            Err(_) => eprintln!("WARNING: {f} is not on this branch; its constants are not linted"),
        }
    }
    lits
}

/// Every raw hash access in one v4 file (`f` = its repo path).
fn raw_access(f: &str, text: &str, lits: &BTreeSet<u64>) -> Vec<String> {
    let (body, mut bad) = (code(text), Vec::new());
    for (module, ir) in [("gravity_abi::hash", false), ("gravity_expr::emit::hash", true)] {
        for (n, name) in hash_names(&body, module).into_iter().filter(|(_, name)| !interface(name, ir)) {
            bad.push(format!("{f}:{n}: takes `{name}` from {module} (only KeyHash's methods / the interface)"));
        }
    }
    for (n, w) in words(&body).into_iter().filter(|(_, w)| big_literal(w).is_some_and(|v| lits.contains(&v))) {
        bad.push(format!("{f}:{n}: the hash file's constant {w}"));
    }
    let lines: Vec<String> = body.lines().map(|l| l.replace(' ', "")).collect();
    for (n, l) in lines.iter().enumerate() {
        if f.starts_with("crates/gravity/lowerv4/src/") && (l.contains("KeyHash::to_abi") || l.contains("KeyHash::from_abi")) {
            bad.push(format!("{f}:{}: KeyHash's ABI word in lowerv4/src (the IR half carries the hash)", n + 1));
        }
        // a shift by 32..63 within six lines after an emit_key_hash / emit_route_and_full: reading the hash's halves by hand
        let after_hash = lines[n.saturating_sub(6)..n].iter().any(|p| p.contains("emit_key_hash(") || p.contains("emit_route_and_full("));
        let shift = ["Ushr", "Sshr", "Shl", ">>", "<<"].iter().any(|op| l.contains(op)) && (32..64).any(|k: u32| l.contains(&format!(",{k})")) || l.contains(&format!(">>{k}")) || l.contains(&format!("<<{k}")));
        if after_hash && shift {
            bad.push(format!("{f}:{}: a 32..63-bit shift next to a KeyHash emit (use emit_home / emit_part / emit_sub / emit_split / emit_stored)", n + 1));
        }
        let raw_field = l.match_indices(".0").any(|(i, _)| {
            let id: String = l[..i].chars().rev().take_while(|c| c.is_alphanumeric() || *c == '_').collect::<Vec<_>>().into_iter().rev().collect();
            id.to_lowercase().contains("hash") && !l[i + 2..].starts_with(|c: char| c.is_ascii_digit())
        });
        if l.contains("KeyHash(") || raw_field {
            bad.push(format!("{f}:{}: raw KeyHash bits (`KeyHash(..)` or `.0` on a hash)", n + 1));
        }
    }
    bad
}

#[test]
fn keyhash_is_the_only_hash_access() {
    let lits = hash_literals();
    let bad: Vec<String> = v4_files().iter().flat_map(|(f, text)| raw_access(f, text, &lits)).collect();
    assert!(bad.is_empty(), "{} raw hash accesses in v4 files:\n{}", bad.len(), bad.join("\n"));
}

/// The lint catches each form of raw access in a synthetic lowerv4 file, and passes the interface.
#[test]
fn the_hash_lint_catches_raw_access() {
    let lits = BTreeSet::from([0x9E37_79B9_7F4A_7C15u64]);
    let ok = "use gravity_abi::hash::{KeyHash, Depth};\nuse gravity_expr::emit::hash::{emit_key_hash, emit_home, Bits};\nlet h = emit_key_hash(b, &img);\nlet s = emit_home(b, h, Bits::Const(10), Bits::Const(0)); // PHI64 in a comment\n";
    assert_eq!(raw_access("crates/gravity/lowerv4/src/keyed/x.rs", ok, &lits), Vec::<String>::new());
    let bad = "use gravity_abi::hash::{KeyHash, PHI64};\nlet m = 0x9E37_79B9_7F4A_7C15;\nlet w = KeyHash::from_abi(x);\nlet h = emit_key_hash(b, &img);\nlet hi = b.int_bin(IntBinOp::Ushr, h, 32);\nlet r = gravity_expr::emit::hash::hash_words(b, &img);\nlet t = key_hash.0;\n";
    let found = raw_access("crates/gravity/lowerv4/src/keyed/x.rs", bad, &lits);
    assert_eq!(found.len(), 6, "{found:#?}");
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(|d| std::path::Path::new(&d).with_file_name("lowerv4")).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
