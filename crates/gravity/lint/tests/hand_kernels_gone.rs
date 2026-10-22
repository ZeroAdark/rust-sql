//! KERNEL-DEL (the user's 09-29 ruling: the per-query hand kernels, kernels modules `cb` and `tpch`, are deleted): the
//! hand-kernel world stays deleted. This fails when any of it comes back:
//! - a `cb` / `tpch` kernels module: the directories or files kernels/src/{cb,tpch}, or `mod cb` / `mod tpch` in the kernels
//!   crate;
//! - the shape table (exec/src/shapes/hand.rs) or its by-identity binding (exec/src/hand_ids.rs);
//! - a `hand-shapes` or `tpch` cargo feature, or a dependency enabling one, in a Cargo.toml under crates/gravity or the
//!   server's (crates/backend/main);
//! - a `cfg` on the `hand-shapes` / `tpch` feature in a Gravity source;
//! - code naming `gravity_kernels::cb` / `gravity_kernels::tpch` (or `crate::cb` / `crate::tpch` in the kernels crate), outside
//!   comments and string literals;
//! - a per-query kernel entry or hand program: a `fn cb_*` / `fn tpch_*` in a Gravity source;
//! - a `cb_` / `tpch_` kernel in the assembly lint's corpus list (`asm_loop::KERNELS`).
//!
//! The lab kernels behind `hand-kernels` (the OL corpus, `par_*`, setops, win; catalog B3-28..B3-34) are not this lint's.
use std::fs;
use std::path::{Path, PathBuf};

use gravity_lint::{gravity_root, rel, rust_files};

const THIS: &str = "hand_kernels_gone.rs";

/// The deleted features, as a Cargo.toml names them.
const FEATURES: &[&str] = &["hand-shapes", "tpch"];

/// `line` without its string literals' contents and its `//` comment: the code a path or a `fn` can hide in.
fn code_of(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    let mut in_str = false;
    while let Some(c) = chars.next() {
        if in_str {
            match c {
                '\\' => {
                    chars.next();
                }
                '"' => {
                    in_str = false;
                    out.push('"');
                }
                _ => {}
            }
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                out.push('"');
            }
            '\'' if chars.peek() == Some(&'"') => {
                // the char literal '"'
                out.push('\'');
                out.push(chars.next().unwrap());
            }
            '/' if chars.peek() == Some(&'/') => break,
            _ => out.push(c),
        }
    }
    out
}

/// The words of `s` (identifier runs).
fn words(s: &str) -> Vec<&str> {
    s.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).filter(|w| !w.is_empty()).collect()
}

/// What a Rust source line brings back, if anything; `in_kernels` = the line is in the kernels crate.
fn rust_finding(raw: &str, in_kernels: bool) -> Option<&'static str> {
    let code = code_of(raw);
    let compact: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    for m in ["gravity_kernels::cb::", "gravity_kernels::tpch::"] {
        if compact.contains(m) {
            return Some("a path into the deleted cb / tpch kernels");
        }
    }
    if let Some(i) = compact.find("gravity_kernels::{") {
        let list = &compact[i..];
        let list = &list[..list.find('}').unwrap_or(list.len())];
        if words(list).iter().any(|w| *w == "cb" || *w == "tpch") {
            return Some("a `use` of the deleted cb / tpch kernels");
        }
    }
    if in_kernels {
        for m in ["crate::cb::", "crate::tpch::", "modcb;", "modtpch;", "modcb{", "modtpch{"] {
            if compact.contains(m) {
                return Some("a cb / tpch module in the kernels crate");
            }
        }
    }
    let w = words(&code);
    if w.windows(2).any(|p| p[0] == "fn" && (p[1].starts_with("cb_") || p[1].starts_with("tpch_"))) {
        return Some("a per-query kernel entry or hand program (`fn cb_*` / `fn tpch_*`)");
    }
    let raw_compact: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.contains("cfg(") || compact.contains("cfg_attr(") {
        for f in FEATURES {
            if raw_compact.contains(&format!("feature=\"{f}\"")) {
                return Some("a cfg on a deleted hand-kernel feature");
            }
        }
    }
    None
}

/// What a Cargo.toml line brings back: a deleted feature declared or enabled.
fn toml_finding(raw: &str) -> Option<&'static str> {
    let line = raw.split('#').next().unwrap_or("");
    let t = line.trim();
    for f in FEATURES {
        if t.starts_with(&format!("{f} =")) || t.starts_with(&format!("{f}=")) || t.starts_with(&format!("\"{f}\" =")) {
            return Some("a deleted hand-kernel feature declared");
        }
        if line.contains(&format!("\"{f}\"")) || line.contains(&format!("/{f}\"")) {
            return Some("a deleted hand-kernel feature enabled");
        }
    }
    None
}

fn cargo_tomls(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd {
        let p = e.unwrap().path();
        if p.is_dir() {
            if p.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            cargo_tomls(&p, out);
        } else if p.file_name().is_some_and(|n| n == "Cargo.toml") {
            out.push(p);
        }
    }
}

#[test]
fn the_per_query_hand_kernels_stay_deleted() {
    let g = gravity_root();
    let crates = g.parent().unwrap();
    let mut bad: Vec<String> = Vec::new();
    for p in ["kernels/src/cb", "kernels/src/cb.rs", "kernels/src/tpch", "kernels/src/tpch.rs", "exec/src/shapes/hand.rs", "exec/src/hand_ids.rs"] {
        if g.join(p).exists() {
            bad.push(format!("crates/gravity/{p}: deleted under the user's 09-29 ruling"));
        }
    }
    let mut tomls = Vec::new();
    cargo_tomls(&g, &mut tomls);
    cargo_tomls(&crates.join("backend/main"), &mut tomls);
    assert!(tomls.iter().any(|p| p.ends_with("kernels/Cargo.toml")), "the scan reads the kernels crate's Cargo.toml");
    for p in &tomls {
        let text = fs::read_to_string(p).unwrap();
        for (i, l) in text.lines().enumerate() {
            if let Some(why) = toml_finding(l) {
                bad.push(format!("{}:{}: {why}: {}", rel(p), i + 1, l.trim()));
            }
        }
    }
    let kernels = g.join("kernels");
    let files = rust_files(&g);
    assert!(files.iter().any(|p| p.ends_with("kernels/src/lib.rs")), "the scan reads the kernels crate");
    for p in &files {
        if p.file_name().is_some_and(|n| n == THIS) {
            continue;
        }
        let text = fs::read_to_string(p).unwrap();
        for (i, l) in text.lines().enumerate() {
            if let Some(why) = rust_finding(l, p.starts_with(&kernels)) {
                bad.push(format!("{}:{}: {why}: {}", rel(p), i + 1, l.trim()));
            }
        }
    }
    for (name, path) in gravity_lint::asm_loop::KERNELS {
        if name.starts_with("cb_") || name.starts_with("tpch_") || path.contains("::cb::") || path.contains("::tpch::") {
            bad.push(format!("asm_loop::KERNELS: {name} ({path}): a deleted hand kernel in the corpus list"));
        }
    }
    assert!(bad.is_empty(), "the per-query hand kernels are deleted (the user's 09-29 ruling):\n{}", bad.join("\n"));
}

/// The detector on the forms it must flag, and on the near forms it must not (comments, string literals, the lab kernels'
/// `hand-kernels` feature, a `cb_` name that is no function).
#[test]
fn the_detector_flags_the_deleted_forms_and_nothing_near_them() {
    let q = '"';
    let flag = [
        "use gravity_kernels::cb::t3::Plan;".to_owned(),
        "    let n = gravity_kernels::tpch::q01::BYTES;".to_owned(),
        "use gravity_kernels::{cb, par_hashagg};".to_owned(),
        "pub unsafe extern \"C-unwind\" fn cb_count(ctx: *mut Ctx) -> i64 {".to_owned(),
        "pub fn tpch_scan(rel: u32) -> Program {".to_owned(),
        format!("#[cfg(feature = {q}hand-shapes{q})]"),
    ];
    for l in &flag {
        assert!(rust_finding(l, false).is_some(), "not flagged: {l}");
    }
    assert!(rust_finding("pub mod cb;", true).is_some() && rust_finding("use crate::tpch::num;", true).is_some());
    let pass = [
        "//! moved out of the deleted per-query kernel module (`cb::t3`)".to_owned(),
        "    assert_eq!(r.demangled, \"gravity_kernels::cb::frame::run\");".to_owned(),
        "use gravity_kernels::t3::{self, BARE}; // was gravity_kernels::cb::t3".to_owned(),
        format!("#[cfg(feature = {q}hand-kernels{q})]"),
        "let cb_refused = 1;".to_owned(),
        "use gravity_kernels::{par_hashagg, setops};".to_owned(),
    ];
    for l in &pass {
        assert!(rust_finding(l, true).is_none(), "flagged: {l}");
    }
    assert!(rust_finding("pub mod cb;", false).is_none(), "`mod cb` outside the kernels crate is another module");
    assert!(toml_finding(&format!("hand-shapes = [{q}gravity_program/hand{q}]")).is_some());
    assert!(toml_finding(&format!("gravity_exec = {{ path = {q}../exec{q}, features = [{q}hand-shapes{q}] }}")).is_some());
    assert!(toml_finding(&format!("tpch = [{q}dep:x{q}]")).is_some());
    assert!(toml_finding(&format!("hand-kernels = [{q}gravity_program/hand{q}]")).is_none());
    assert!(toml_finding(&format!("tpchfloor = [{q}dep:pgrc_write{q}]")).is_none());
    assert!(toml_finding("# the hand-shapes feature is gone").is_none());
}
