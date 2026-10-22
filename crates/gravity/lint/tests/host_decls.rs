//! The IR-to-host call types (wasmjit entry c): a host the IR calls (`declare_host` / `declare_host_once`) is reached by an
//! indirect call typed from its IR declaration, which on wasm32 must name the Rust function's own wasm type exactly or the
//! call traps (`call_indirect`'s type check). The host ABI is 64-bit words (gravity_abi::host): an IR `Ptr` or `I64` is a
//! `u64` / `i64` / `Datum` in the Rust signature, never a typed pointer, reference or `usize` (pointer width: an `i32` on
//! wasm32); an `I32` / `I16` / `I8` / `Bool` is an integer of 32 bits or fewer (an `i32`, `call_typed`'s narrow mask);
//! no result is no result. Scanned: every `src/` file under crates/gravity. A site is paired when its params and result
//! are literal `Type`s and its address is a named function (`f as usize`, `f as *const () as usize`, `f::<H> as ...`)
//! whose `extern "C"` / `host!` definitions under crates/gravity agree on one shape; the rest (params or address built at
//! run time, a name defined twice with two shapes) are listed in `UNPAIRED`, so a new one is a decision.
//!
//! 64-bit-only declarations (D3129): a declaration whose Rust callee differs from its IR types only by carrying a typed
//! pointer or a two-word struct where the IR has a 64-bit word is the same call under a 64-bit ABI (one register each, a
//! struct's first word in the first return register) and traps on wasm32; it is accepted only inside a function compiled
//! for 64-bit targets alone (`#[cfg(target_pointer_width = "64")]`). A `<shim>.addr()` address is a gravity_fn shim's
//! typed entry (`fn/src/shims/catalog.rs`: `(*mut ErrSlot, ..) -> Ret`), the expression compiler's direct road: every
//! such declaration must be 64-bit only.
//!
//! gravity_abi's blocks: the IR has one host call (`Op::CallHost`) and never calls a block's fn-pointer field (`PushFn`,
//! `RecOpenFn`, `NextFn`, ..); Rust calls those through their own typed pointers, which `ir_calls_no_block_fn_field` keeps
//! true.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use gravity_lint::gravity_root;

/// The sites the scan cannot pair, `path:fn-or-text`: dynamic params / address, or a name with two host shapes. Checked both
/// ways (a listed site that pairs now, or is gone, fails too).
const UNPAIRED: &[&str] = &[
    "backend/src/cache.rs:host_addr",
    "expr/src/emit/agg.rs:host",
    "ir/src/text.rs:addr",
    "lowerv4/src/dict/minmax_code.rs:0",
    "lowerv4/src/dict/strategies/codefront_light.rs:addr",
    "lowerv4/src/dict/strategies/text_frame.rs:like_frame",
    "lowerv4/src/end/sink.rs:f",
    "lowerv4/src/frame/site.rs:entry1",
    "lowerv4/src/ops/agg/numeric/num_wide.rs:host",
    "lowerv4/src/ops/agg/trans/class_interval_sum.rs:host",
    "lowerv4/src/ops/agg/trans/code_copy.rs:host.1",
    "lowerv4/src/ops/agg/trans/slots.rs:addr",
    "lowerv4/src/ops/agg/trans/slots.rs:host",
    "lowerv4/src/ops/group/distinct_total.rs:addr",
    "lowerv4/src/ops/hashjoin/build.rs:f",
    "lowerv4/src/ops/hashjoin/probe_wait.rs:f",
    "lowerv4/src/ops/hashjoin/spill/build.rs:f",
    "lowerv4/src/ops/hashjoin/spill/pair.rs:f",
    "lowerv4/src/ops/lockrows/lower.rs:lock_row",
    "lowerv4/src/ops/lockrows/marks.rs:lock_row",
    "lowerv4/src/ops/lockrows/pair.rs:lock_row",
    "lowerv4/src/ops/memoize/lower.rs:addr",
    "lowerv4/src/ops/modifytable/onconflict.rs:write_row",
    "lowerv4/src/ops/modifytable/rowop.rs:write_row",
    "lowerv4/src/ops/setop/recursive.rs:addr",
    "lowerv4/src/ops/sort/strategies/resident.rs:addr",
    "lowerv4/src/ops/sort/topn/strategies/twopass_bare.rs:addr",
    "lowerv4/src/ops/srf/cell.rs:addr",
    "lowerv4/src/ops/subplan/hashed.rs:addr",
    "lowerv4/src/ops/subplan/initplan/lower.rs:addr",
    "lowerv4/src/ops/subplan/result.rs:addr",
    "lowerv4/src/ops/window/lower.rs:addr",
    "lowerv4/src/rep/code/datum.rs:0",
    "lowerv4/src/rep/code/hash.rs:0",
    "lowerv4/src/rep/hosted/hash.rs:host",
    "lowerv4/src/rep/keylane.rs:host",
    "lowerv4/src/rep/varlena/num_arith.rs:keep",
    "lowerv4/src/rep/varlena/num_image.rs:keep",
    "lowerv4/src/rep/word/hash.rs:host",
    "lowerv4/src/rerun/epq/epq_ir.rs:lock_row",
    "lowerv4/src/rerun/sets.rs:addr",
    "lowerv4/src/rerun/store_read.rs:addr",
    "lowerv4/src/table/bypass.rs:addr",
    "lowerv4/src/table/emit_group.rs:f",
    "lowerv4/src/table/keyimg.rs:0",
    "lowerv4/src/table/size.rs:addr",
];

/// A value's class at the host boundary: `W` a 64-bit word, `N` an integer of 32 bits or fewer, `F` a float, `X` anything
/// pointer-width or not a scalar (never a host parameter's type).
fn rust_kind(t: &str) -> char {
    let t = t.trim().trim_start_matches("mut ").trim();
    match t.rsplit("::").next().unwrap_or(t) {
        "u64" | "i64" | "Datum" => 'W',
        "u32" | "i32" | "u16" | "i16" | "u8" | "i8" | "bool" => 'N',
        "f64" | "f32" => 'F',
        _ => 'X',
    }
}

fn ir_kind(t: &str) -> char {
    match t.trim().trim_start_matches("Type::") {
        "Ptr" | "I64" => 'W',
        "I32" | "I16" | "I8" | "Bool" => 'N',
        "F64" | "F32" => 'F',
        _ => '?',
    }
}

/// The text with `//` comments blanked to spaces (offsets kept, so line numbers stay true).
fn strip_comments(s: &str) -> String {
    s.lines().map(|l| match l.find("//") { Some(i) => format!("{}{}", &l[..i], " ".repeat(l.len() - i)), None => l.to_string() }).collect::<Vec<_>>().join("\n")
}

/// The index just past the bracket that closes the one at `open` (`(`/`[`/`{`/`<`), counting that kind only.
fn close(b: &[u8], open: usize) -> usize {
    let (o, c) = match b[open] { b'(' => (b'(', b')'), b'[' => (b'[', b']'), b'{' => (b'{', b'}'), _ => (b'<', b'>') };
    let (mut d, mut i) = (0i32, open);
    while i < b.len() {
        if b[i] == o {
            d += 1;
        } else if b[i] == c && !(c == b'>' && i > 0 && b[i - 1] == b'-') {
            d -= 1;
            if d == 0 {
                return i + 1;
            }
        }
        i += 1;
    }
    b.len()
}

/// `s` split at its top-level commas (outside `()`, `[]`, `{}`, `<>`).
fn split_top(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut d) = (Vec::new(), String::new(), 0i32);
    let b = s.as_bytes();
    for (i, ch) in s.char_indices() {
        match ch {
            '(' | '[' | '{' | '<' => d += 1,
            ')' | ']' | '}' => d -= 1,
            '>' if !(i > 0 && b[i - 1] == b'-') => d -= 1,
            ',' if d == 0 => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(ch);
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out.into_iter().map(|x| x.trim().to_string()).collect()
}

/// One host definition's shape: parameter kinds and result kind (`None` for no result).
type Shape = (Vec<char>, Option<char>);

/// Every `extern "C"` function and every function inside a `host! { .. }` block in `text`, by name.
fn host_defs(text: &str, out: &mut BTreeMap<String, Vec<Shape>>) {
    let b = text.as_bytes();
    let mut spans = Vec::new();
    let mut at = 0;
    while let Some(k) = text[at..].find("host!") {
        let s = at + k;
        let rest = text[s + 5..].trim_start();
        if rest.starts_with('{') {
            let open = text.len() - rest.len();
            spans.push((s, close(b, open)));
        }
        at = s + 5;
    }
    let mut at = 0;
    while let Some(k) = text[at..].find("fn ") {
        let s = at + k;
        at = s + 3;
        if s > 0 && (b[s - 1].is_ascii_alphanumeric() || b[s - 1] == b'_') {
            continue;
        }
        let line_start = text[..s].rfind('\n').map_or(0, |i| i + 1);
        let before = &text[line_start..s];
        let in_host = spans.iter().any(|&(a, z)| a <= s && s < z);
        if !in_host && !before.contains("extern \"C") {
            continue;
        }
        let name: String = text[s + 3..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
        let mut i = s + 3 + name.len();
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if name.is_empty() || i >= b.len() {
            continue;
        }
        if b[i] == b'<' {
            i = close(b, i);
        }
        if i >= b.len() || b[i] != b'(' {
            continue;
        }
        let pe = close(b, i);
        let params: Vec<char> = split_top(&text[i + 1..pe - 1]).iter().filter_map(|p| p.split_once(':').map(|(_, t)| rust_kind(t))).collect();
        let tail = &text[pe..];
        let end = tail.find(['{', ';']).unwrap_or(tail.len());
        let sig = tail[..end].split(" where ").next().unwrap().trim();
        let ret = sig.strip_prefix("->").map(|r| r.trim()).filter(|r| *r != "()").map(rust_kind);
        out.entry(name).or_default().push((params, ret));
    }
}

/// A declaration site: `(path:line, address text, IR params, IR result)`; params `None` when not a literal `&[Type::..]`.
struct Site {
    at: String,
    key: String,
    /// The declaring call's byte offset in its file (`native_only`).
    off: usize,
    /// The address argument's text, whitespace collapsed.
    raw: String,
    addr: Option<String>,
    params: Option<Vec<char>>,
    ret: Option<Option<char>>,
}

/// The address-text prefix of a `host_words!` site (`decl_sites`): its host is wrapped, so its shape is all words.
const WORDS: &str = "words:";

/// A literal `&[Type::..]` (or `&[Type::X; n]`) as kinds; `None` when not literal.
fn ir_params(a: &str) -> Option<Vec<char>> {
    a.strip_prefix("&[").and_then(|p| p.strip_suffix(']')).and_then(|p| {
        let p = p.trim();
        if let Some((t, n)) = p.split_once(';') {
            let n: usize = n.trim().parse().ok()?;
            let k = ir_kind(t);
            return (k != '?').then(|| vec![k; n]);
        }
        let ks: Vec<char> = split_top(p).iter().map(|t| ir_kind(t)).collect();
        (!ks.contains(&'?')).then_some(ks)
    })
}

/// A literal `None` / `Some(Type::..)` as a kind.
fn ir_ret(a: &str) -> Option<Option<char>> {
    match a.trim() {
        "None" => Some(None),
        r => r.strip_prefix("Some(").and_then(|r| r.strip_suffix(')')).map(ir_kind).filter(|k| *k != '?').map(Some),
    }
}

fn decl_sites(rel: &str, text: &str, out: &mut Vec<Site>) {
    let b = text.as_bytes();
    let mut at = 0;
    while let Some(k) = text[at..].find("declare_host") {
        let s = at + k;
        at = s + 12;
        let mut i = s + 12;
        if text[i..].starts_with("_once") {
            i += 5;
        }
        if i >= b.len() || b[i] != b'(' || (s > 0 && b[s - 1] != b'.' && b[s - 1] != b':') {
            continue;
        }
        let args = split_top(&text[i + 1..close(b, i) - 1]);
        if args.len() < 4 {
            continue;
        }
        let line = text[..s].matches('\n').count() + 1;
        let addr_text = args[3].split_whitespace().collect::<Vec<_>>().join(" ");
        if let Some(w) = addr_text.strip_prefix("gravity_abi::host_words!(") {
            // the word-typed wrapper (`gravity_abi::host_words!`): every parameter and the answer a 64-bit word
            let name = w.split('(').next().unwrap_or("").rsplit("::").next().unwrap_or("").to_string();
            out.push(Site { at: format!("{rel}:{line}"), key: format!("{rel}:{name}"), off: s, raw: addr_text.clone(), addr: Some(format!("{WORDS}{name}")), params: ir_params(&args[1]), ret: ir_ret(&args[2]) });
            continue;
        }
        let addr = addr_text.strip_suffix(" as usize").map(|a| a.strip_suffix(" as *const ()").unwrap_or(a)).and_then(|a| {
            let path = a.split("::<").next().unwrap();
            let ok = !path.is_empty() && path.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':') && !path.chars().next().unwrap().is_ascii_digit();
            ok.then(|| path.rsplit("::").next().unwrap().to_string())
        });
        let key = addr.clone().unwrap_or_else(|| addr_text.split_whitespace().next().unwrap_or("").to_string());
        let params = ir_params(&args[1]);
        let ret = ir_ret(&args[2]);
        out.push(Site { at: format!("{rel}:{line}"), key: format!("{rel}:{key}"), off: s, raw: addr_text, addr, params, ret });
    }
}

/// The typed entries' shapes of gravity_fn's `Family::ByVal` shims (the direct road's callees), from the catalog's
/// `Addr(transmute::<extern "C-unwind" fn(..) -> Ret, RawFn>(..))`.
fn byval_entries(files: &[(String, String)]) -> Vec<Shape> {
    let mut out = Vec::new();
    for (rel, text) in files {
        if !rel.ends_with("fn/src/shims/catalog.rs") {
            continue;
        }
        for row in text.lines().filter(|l| l.contains("family: Family::ByVal,")) {
            let Some(k) = row.find("transmute::<extern \"C") else { continue };
            let t = &row[k..];
            let Some(o) = t.find("fn(") else { continue };
            let b = t.as_bytes();
            let pe = close(b, o + 2);
            let params = split_top(&t[o + 3..pe - 1]).iter().map(|p| rust_kind(p)).collect();
            let ret = t[pe..].split(", RawFn>").next().unwrap_or("").trim().strip_prefix("->").map(|r| r.trim()).filter(|r| *r != "()").map(rust_kind);
            out.push((params, ret));
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Whether the innermost function whose body holds byte `off` of `text` is compiled for 64-bit targets only: a
/// `#[cfg(target_pointer_width = "64")]` among the attributes right above its `fn` line (doc comments are blank here).
fn native_only(text: &str, off: usize) -> bool {
    let b = text.as_bytes();
    let mut best: Option<(usize, usize)> = None;
    let mut at = 0;
    while let Some(k) = text[at..].find("fn ") {
        let s = at + k;
        at = s + 3;
        if s > off || (s > 0 && (b[s - 1].is_ascii_alphanumeric() || b[s - 1] == b'_')) {
            continue;
        }
        let Some(open) = text[s..].find(['{', ';']).map(|i| s + i) else { continue };
        if b[open] != b'{' {
            continue;
        }
        let end = close(b, open);
        if open < off && off < end && best.is_none_or(|(o, _)| s > o) {
            best = Some((s, end));
        }
    }
    let Some((s, _)) = best else { return false };
    let line_start = text[..s].rfind('\n').map_or(0, |i| i + 1);
    for line in text[..line_start].lines().rev() {
        let l = line.trim();
        if l.is_empty() {
            continue;
        }
        if !l.starts_with("#[") {
            return false;
        }
        if l.replace(' ', "") == "#[cfg(target_pointer_width=\"64\")]" {
            return true;
        }
    }
    false
}

/// IR shape `ir` and Rust shape `rust` differ only where the Rust callee has a typed pointer / struct (`X`) and the IR a
/// 64-bit word (`W`): the same call under a 64-bit ABI.
fn wide_only(ir: &Shape, rust: &Shape) -> bool {
    let same = |a: char, b: char| a == b || (a == 'W' && b == 'X');
    ir.0.len() == rust.0.len() && ir.0.iter().zip(&rust.0).all(|(&a, &b)| same(a, b)) && match (ir.1, rust.1) {
        (Some(a), Some(b)) => same(a, b),
        (a, b) => a == b,
    }
}

fn sources(root: &Path) -> Vec<(String, String)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
        entries.sort();
        for p in entries {
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            if p.is_dir() {
                if !["target", "tests", "examples", "benches"].contains(&name.as_str()) && !name.starts_with('.') {
                    walk(root, &p, out);
                }
            } else if name.ends_with(".rs") {
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
                if rel.contains("/src/") || rel.starts_with("src/") {
                    out.push((rel, strip_comments(&std::fs::read_to_string(&p).unwrap_or_default())));
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out
}

/// -> (mismatches, unpaired site keys, paired site count).
fn audit(root: &Path) -> (Vec<String>, Vec<String>, usize) {
    let files = sources(root);
    let mut defs = BTreeMap::new();
    let mut sites = Vec::new();
    for (rel, text) in &files {
        host_defs(text, &mut defs);
        decl_sites(rel, text, &mut sites);
    }
    let byval = byval_entries(&files);
    let text_of = |at: &str| files.iter().find(|(rel, _)| at.starts_with(&format!("{rel}:"))).map(|(_, t)| t.as_str()).unwrap_or("");
    let (mut bad, mut unpaired, mut paired) = (Vec::new(), Vec::new(), 0);
    let show = |s: &Shape| format!("({}) -> {}", s.0.iter().collect::<String>(), s.1.map_or("()".into(), |c| c.to_string()));
    for s in &sites {
        if s.raw.ends_with(".addr()") {
            // a gravity_fn shim's typed entry (the direct road): `(*mut ErrSlot, i64..) -> Ret`, never the IR's words
            paired += 1;
            let typed = byval.iter().filter(|e| e.0.contains(&'X') || e.1 == Some('X')).count();
            if typed > 0 && !native_only(text_of(&s.at), s.off) {
                bad.push(format!("crates/gravity/{}: {}: a gravity_fn shim's typed entry ({typed} Family::ByVal shapes, e.g. Rust {}) declared for the IR outside a fn under #[cfg(target_pointer_width = \"64\")]: wasm32 traps (D3129)", s.at, s.raw, show(&byval[0])));
            }
            continue;
        }
        let (Some(name), Some(params), Some(ret)) = (&s.addr, &s.params, s.ret) else {
            unpaired.push(s.key.clone());
            continue;
        };
        if let Some(host) = name.strip_prefix(WORDS) {
            // a wrapped host: the wrapper takes and answers words whatever the host's own types
            paired += 1;
            if params.iter().any(|&k| k != 'W') || ret != Some('W') {
                bad.push(format!("crates/gravity/{}: {host}: IR {} through host_words! (its wrapper is all 64-bit words: Ptr / I64 only, one result)", s.at, show(&(params.clone(), ret))));
            }
            continue;
        }
        let mut shapes: Vec<&Shape> = defs.get(name).map(|v| v.iter().collect()).unwrap_or_default();
        shapes.dedup();
        shapes.sort();
        shapes.dedup();
        if shapes.len() != 1 {
            unpaired.push(s.key.clone());
            continue;
        }
        paired += 1;
        let want: Shape = (params.clone(), ret);
        if *shapes[0] != want && !(wide_only(&want, shapes[0]) && native_only(text_of(&s.at), s.off)) {
            bad.push(format!("crates/gravity/{}: {name}: IR {} vs Rust {}", s.at, show(&want), show(shapes[0])));
        }
    }
    unpaired.sort();
    unpaired.dedup();
    (bad, unpaired, paired)
}

#[test]
fn every_paired_host_takes_its_ir_declaration_as_words() {
    let (bad, unpaired, paired) = audit(&gravity_root());
    assert!(paired >= 150, "the scan paired only {paired} declaration sites (about 190 when written): the parser lost them");
    assert!(bad.is_empty(), "{} host declaration(s) whose Rust signature is not the IR's (W = u64/i64/Datum for Ptr and I64, N = an integer of 32 bits or fewer, X = a typed pointer / usize: take the word and cast inside):\n{}", bad.len(), bad.join("\n"));
    let listed: Vec<String> = UNPAIRED.iter().map(|s| s.to_string()).collect();
    let new: Vec<&String> = unpaired.iter().filter(|u| !listed.contains(u)).collect();
    let gone: Vec<&String> = listed.iter().filter(|u| !unpaired.contains(u)).collect();
    assert!(new.is_empty() && gone.is_empty(), "UNPAIRED is stale. New unpaired sites (make them pairable, or list them): {new:?}; listed but now paired or gone (drop them): {gone:?}");
}

#[test]
fn ir_calls_no_block_fn_field() {
    // The IR's calls are `Op::Call` (an IR function) and `Op::CallHost` (a declared host, audited above): no op calls through
    // a loaded pointer, so a gravity_abi block's fn field (`PushFn`, `RecOpenFn`, `NextFn`, ..) is called only by Rust,
    // through its own typed pointer, which wasm32 types exactly. A new call op is an indirect host call this audit misses.
    let ir = std::fs::read_to_string(gravity_root().join("ir/src/lib.rs")).unwrap();
    let at = ir.find("pub enum Op {").expect("gravity_ir's Op enum");
    let body = &ir[at..close(ir.as_bytes(), at + ir[at..].find('{').unwrap())];
    let calls: Vec<&str> = body.lines().map(str::trim).filter(|l| l.starts_with("Call")).map(|l| l.split([' ', '{', '(']).next().unwrap()).collect();
    assert_eq!(calls, ["Call", "CallHost"], "gravity_ir's call ops changed: audit the new one's host types (host_decls.rs)");
}

#[test]
fn the_audit_finds_a_typed_pointer_host() {
    let d = std::env::temp_dir().join(format!("gravity-lint-hostdecl-{}", std::process::id()));
    std::fs::create_dir_all(d.join("x/src")).unwrap();
    std::fs::write(
        d.join("x/src/a.rs"),
        concat!(
            "gravity_abi::host! {\n    pub unsafe fn good(ctx: u64, n: u64) -> i64 { 0 }\n    pub unsafe fn bad(ctx: *mut Ctx, n: u64) -> i64 { 0 }\n}\n",
            "pub unsafe extern \"C\" fn narrow(a: u64, b: u32) {}\n",
            "fn f(pb: &mut B) {\n    pb.declare_host(\"good\", &[Type::Ptr, Type::I64], Some(Type::I64), good as usize, f);\n",
            "    pb.declare_host(\"bad\", &[Type::Ptr, Type::I64], Some(Type::I64), bad as *const () as usize, f);\n",
            "    pb.declare_host(\"narrow\", &[Type::I64, Type::I32], None, narrow as usize, f);\n",
            "    pb.declare_host_once(\"narrow2\", &[Type::I64; 2], None, narrow as usize, f);\n",
            "    pb.declare_host(\"dyn\", &params, None, addr, f);\n}\n",
        ),
    )
    .unwrap();
    let (bad, unpaired, paired) = audit(&d);
    let _ = std::fs::remove_dir_all(&d);
    assert_eq!((bad.len(), paired), (2, 4), "{bad:?}");
    assert!(bad[0].contains("x/src/a.rs:8: bad: IR (WW) -> W vs Rust (XW) -> W"), "{bad:?}");
    assert!(bad[1].contains("x/src/a.rs:10: narrow: IR (WW) -> () vs Rust (WN) -> ()"), "{bad:?}");
    assert_eq!(unpaired, vec!["x/src/a.rs:addr".to_string()]);
}

#[test]
fn the_audit_holds_typed_entries_to_64_bit_only_functions() {
    // D3129: the expression compiler's direct road declared a shim's typed entry, `(*mut ErrSlot, i64..) -> Ret`, as IR
    // words on every target; wasm32 trapped at the indirect call (104 e2e files). A deliberately mismatched host: the same
    // declaration outside a 64-bit-only fn, and a typed-pointer host declared as words outside one, are both found.
    let d = std::env::temp_dir().join(format!("gravity-lint-hostwide-{}", std::process::id()));
    std::fs::create_dir_all(d.join("fn/src/shims")).unwrap();
    std::fs::create_dir_all(d.join("x/src")).unwrap();
    std::fs::write(
        d.join("fn/src/shims/catalog.rs"),
        concat!(
            "pub static SHIMS: &[Shim] = &[\n",
            "    Shim { oid: 56, name: \"boollt\", family: Family::ByVal, nargs: 2, addr: Addr(unsafe { core::mem::transmute::<extern \"C-unwind\" fn(*mut ErrSlot, i64, i64) -> Ret, RawFn>(byval::boollt) }), call: |e, r, coll, ty, a| byval::boollt(e, a[0].v(), a[1].v()) },\n",
            "    Shim { oid: 31, name: \"byteaout\", family: Family::ByRefRef, nargs: 1, addr: Addr(unsafe { core::mem::transmute::<extern \"C-unwind\" fn(*mut ErrSlot, *mut Region, View) -> Ret, RawFn>(byref_ref::byteaout) }), call: |e, r, coll, ty, a| byref_ref::byteaout(e, r, a[0].r()) },\n",
            "];\n",
        ),
    )
    .unwrap();
    std::fs::write(
        d.join("x/src/a.rs"),
        concat!(
            "pub unsafe extern \"C\" fn typed(ctx: *mut Ctx, n: u64) -> i64 { 0 }\n",
            "impl E {\n    fn open_road(&mut self, shim: &'static Shim) {\n",
            "        let h = self.b.declare_host_once(&name, &params, Some(Type::I64), shim.addr(), flags);\n    }\n",
            "    /// the direct road\n    #[cfg(target_pointer_width = \"64\")]\n    fn gated_road(&mut self, shim: &'static Shim) {\n",
            "        let h = self.b.declare_host_once(&name, &params, Some(Type::I64), shim.addr(), flags);\n    }\n}\n",
            "#[cfg(target_pointer_width = \"64\")]\nfn wide(pb: &mut B) {\n",
            "    pb.declare_host(\"typed\", &[Type::Ptr, Type::I64], Some(Type::I64), typed as usize, f);\n",
            "    pb.declare_host(\"typed\", &[Type::Ptr, Type::I32], Some(Type::I64), typed as usize, f);\n}\n",
            "fn plain(pb: &mut B) {\n",
            "    pb.declare_host(\"typed\", &[Type::Ptr, Type::I64], Some(Type::I64), typed as usize, f);\n}\n",
        ),
    )
    .unwrap();
    let (bad, unpaired, paired) = audit(&d);
    let _ = std::fs::remove_dir_all(&d);
    assert_eq!((bad.len(), paired, unpaired.len()), (3, 5, 0), "{bad:?} {unpaired:?}");
    assert!(bad[0].starts_with("crates/gravity/x/src/a.rs:4: shim.addr(): a gravity_fn shim's typed entry (1 Family::ByVal shapes, e.g. Rust (XWW) -> X)"), "{bad:?}");
    assert!(bad[1].contains("x/src/a.rs:15: typed: IR (WN) -> W vs Rust (XW) -> W"), "{bad:?}");
    assert!(bad[2].contains("x/src/a.rs:18: typed: IR (WW) -> W vs Rust (XW) -> W"), "{bad:?}");
}

/// Every place a host whose Rust signature takes or answers a typed pointer / `usize` (`X`) has its address taken for the
/// IR, whatever the declaring call (`declare_host`, a lowerer's local `host(b, ..)`, a `("name", addr)` table): a line
/// that names the host (a string literal) and takes `f as usize`, or any `f as *const () as usize`. On wasm32 such an address
/// must be `gravity_abi::host_words!(f(..))` (lane wasmjit stage 2: `sort_open`'s `*mut u8` answer trapped the
/// interpreter's indirect call). A name with a word-typed definition too is skipped (two hosts, one name).
fn unwrapped_typed(root: &Path) -> Vec<String> {
    let files = sources(root);
    let mut defs = BTreeMap::new();
    for (_, text) in &files {
        host_defs(text, &mut defs);
    }
    let typed: Vec<&String> = defs.iter().filter(|(_, v)| v.iter().all(|s| s.0.contains(&'X') || s.1 == Some('X'))).map(|(n, _)| n).collect();
    let mut out = Vec::new();
    for (rel, text) in &files {
        if rel == "abi/src/host.rs" {
            continue; // host_words!'s own wrapper (`words as *const () as usize`)
        }
        let text = strip_comments(text);
        for (n, line) in text.lines().enumerate() {
            // every pattern below ends in "as usize": a line without it cannot match (the scan was lines x names x
            // format! allocations, ~125 s per check-test run at opt-level 1; buildprof 2026-10-09)
            if !line.contains("as usize") {
                continue;
            }
            let named = line.contains('"');
            for name in &typed {
                // `f as *const () as usize` is a function's address wherever it stands (`let addr = ..` a line above its
                // declaration); a bare `f as usize` only on a line that names a host
                let pats = if named { vec![format!("{name} as *const () as usize"), format!("{name} as usize")] } else { vec![format!("{name} as *const () as usize")] };
                for pat in pats {
                    let mut at = 0;
                    while let Some(k) = line[at..].find(&pat) {
                        let s = at + k;
                        at = s + pat.len();
                        let prev = line[..s].chars().next_back();
                        if prev.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '!') {
                            continue;
                        }
                        out.push(format!("crates/gravity/{rel}:{}: {name}", n + 1));
                    }
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

#[test]
fn every_typed_pointer_host_is_declared_through_host_words() {
    let bad = unwrapped_typed(&gravity_root());
    assert!(bad.is_empty(), "{} typed-pointer host address(es) taken for the IR without gravity_abi::host_words! (wasm32 calls them with words):\n{}", bad.len(), bad.join("\n"));
}

#[test]
fn the_wide_audit_finds_an_unwrapped_typed_host() {
    let d = std::env::temp_dir().join(format!("gravity-lint-hostwords-{}", std::process::id()));
    std::fs::create_dir_all(d.join("x/src")).unwrap();
    std::fs::write(
        d.join("x/src/a.rs"),
        concat!(
            "gravity_abi::host! {\n    pub unsafe fn open(ctx: *mut Ctx, n: u64) -> *mut u8 { 0 }\n    pub unsafe fn word(ctx: u64) -> i64 { 0 }\n}\n",
            "fn f(b: &mut B) {\n    host(b, \"open\", &[Type::Ptr, Type::I64], Type::Ptr, h::open as *const () as usize);\n",
            "    let t = (\"open\", open as usize);\n",
            "    host(b, \"open\", &[Type::Ptr, Type::I64], Type::Ptr, gravity_abi::host_words!(h::open(ctx, n)));\n",
            "    host(b, \"word\", &[Type::Ptr], Type::I64, word as usize);\n    let n = s.open as usize;\n",
            "    let addr = h::open as *const () as usize;\n    let k = open as usize;\n}\n",
        ),
    )
    .unwrap();
    let bad = unwrapped_typed(&d);
    let _ = std::fs::remove_dir_all(&d);
    assert_eq!(bad, vec!["crates/gravity/x/src/a.rs:11: open".to_string(), "crates/gravity/x/src/a.rs:6: open".to_string(), "crates/gravity/x/src/a.rs:7: open".to_string()]);
}
