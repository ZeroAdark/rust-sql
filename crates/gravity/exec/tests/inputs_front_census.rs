//! V4R-7 (main 09-24, the front key's condition 2): the front key against decompose over the whole decompose corpus.
//! Every by-value Const of every fixture the walk keys is perturbed in the plan text (the low bit of its first byte);
//! where the front key still says "the same shape" (`main` equal, and `tl` / `zone` equal where `front::needs` says the
//! fixture reads them), decompose must agree with the original: both refuse, or both give the same content key over the
//! same front positions. `-- --nocapture` lists the Consts the walk keys by value, by (plan node, field, parent).
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use gravity_decompose::{corpus, decompose_in};
use gravity_exec::inputs::front;
use gravity_exec::inputs::front::{Class, Front, Site};
use gravity_program::{Arena, ContentKey};
use mcx::MemoryContext;
use types_nodes::{Node, NodeTag};

/// PM3a: the walk now keys statements whose lowering key asks the type cache (a record or a whole-row key, typesem): no
/// fixture carries a pg_type catalog, so the seam answers "not known" (the key refuses alike on both sides of a check).
fn seams() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| syscache_seams::lookup_pg_type_typcache_shape::set(|_| Ok(None)));
}

fn fixtures() -> Vec<PathBuf> {
    let root = manifest_dir().join("../decompose/tests/plans");
    let mut out = Vec::new();
    for d in [root.clone(), root.join("regress-full")] {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            if e.path().extension().is_some_and(|x| x == "plan") {
                out.push(e.path());
            }
        }
    }
    out.sort();
    out
}

/// Per-fixture cost (the oltp* fixtures): the front walk against decompose + key_and_nodes, in ns, best of five.
fn costs(text: &str) -> Option<(u128, u128)> {
    let ctx = MemoryContext::new("front cost");
    let fx = corpus::load(ctx.mcx(), text).ok()?;
    let (mut walk, mut dec) = (u128::MAX, u128::MAX);
    for _ in 0..5 {
        // one arena per pass and per side, each dropped with everything it holds
        let arena = Arena::new(1 << 22);
        let t = Instant::now();
        drop(front::walk(arena.handle(), fx.stmt, None)?);
        walk = walk.min(t.elapsed().as_nanos());
        let arena = Arena::new(1 << 22);
        let t = Instant::now();
        let g = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).ok()?;
        gravity_lower::inputs::key::key_and_nodes(&g, &mut |_| {});
        dec = dec.min(t.elapsed().as_nanos());
        drop(g);
    }
    Some((walk, dec))
}

/// Every non-null by-value Const's first value byte in the plan text: its offset, its length and its value.
fn const_bytes(text: &str) -> Vec<(usize, usize, u8)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = text[from..].find("{CONST ") {
        let at = from + i;
        let end = at + text[at..].find('}').unwrap();
        let c = &text[at..end];
        from = end;
        // D487: an `-- idxexprs` facts line is an index's own expression (the catalog, not the plan): a front variant holds
        // the index's relcache entry as a dependency (C3 S4d; front.rs: relcache index_expr), which an index expression's
        // change moves, so its Consts are not the plan's words. Every other facts header stays perturbable.
        let line = text[..at].rfind('\n').map_or(0, |n| n + 1);
        if text[line..at].starts_with("-- idxexprs ") {
            continue;
        }
        if !c.contains(":constbyval true :constisnull false") {
            continue;
        }
        let v = at + c.find(":constvalue ").unwrap();
        let b = v + text[v..].find("[ ").unwrap() + 2;
        let len = text[b..].find(' ').unwrap();
        out.push((b, len, text[b..b + len].parse().unwrap()));
    }
    out
}

/// The positions in the front walk of the key walk's nodes, by identity (`front::fill`'s rule); `None` when one is not there.
fn positions(f: &Front<'_>, nodes: &[Node<'_>]) -> Option<Vec<u32>> {
    nodes.iter().map(|n| f.nodes.iter().position(|m| m.ptr_eq(*n)).map(|i| i as u32)).collect()
}

struct Outcome {
    main: u128,
    tl: u128,
    zone: u128,
    needs: (bool, bool),
    trace: Vec<(u64, Class, Site)>,
    graph: Result<(ContentKey, Option<Vec<u32>>), ()>,
}

fn run(text: &str) -> Option<Outcome> {
    let ctx = MemoryContext::new("front census");
    let fx = corpus::load(ctx.mcx(), text).ok()?;
    let arena = Arena::new(1 << 22);
    let mut trace = Vec::new();
    let mut rec = |n: Node<'_>, c: Class, s: Site| trace.push((n.as_const().unwrap().constvalue.as_u64(), c, s));
    let f = front::walk(arena.handle(), fx.stmt, Some(&mut rec))?;
    let needs = front::needs(&f, fx.stmt, &fx.facts);
    let graph = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).map_err(|_| ()).map(|g| {
        let mut nodes = Vec::new();
        let k = gravity_lower::inputs::key::key_and_nodes(&g, &mut |n| nodes.push(n));
        (k, positions(&f, &nodes))
    });
    Some(Outcome { main: f.main, tl: f.tl, zone: f.zone, needs, trace, graph })
}

#[test]
fn a_front_key_decides_what_decompose_decides() {
    seams();
    let (mut keyed, mut filed, mut consts, mut same) = (0, 0, 0, 0);
    let mut listing: BTreeMap<String, usize> = BTreeMap::new();
    let mut bad = Vec::new();
    let unvisited = Site { plan: NodeTag::T_Invalid, field: "(not walked)", parent: NodeTag::T_Invalid };
    let (mut walk_ns, mut dec_ns, mut timed) = (0u128, 0u128, 0u128);
    for path in fixtures() {
        let text = std::fs::read_to_string(&path).unwrap();
        let Some(base) = run(&text) else { continue };
        let name = path.file_stem().unwrap().to_string_lossy().to_string();
        keyed += 1;
        if let Some((w, d)) = name.starts_with("oltp").then(|| costs(&text)).flatten() {
            (walk_ns, dec_ns, timed) = (walk_ns + w, dec_ns + d, timed + 1);
        }
        filed += base.graph.as_ref().is_ok_and(|g| g.1.is_some()) as usize;
        for (at, len, byte) in const_bytes(&text) {
            let mut t = text.clone();
            t.replace_range(at..at + len, &(byte ^ 1).to_string());
            let Some(p) = run(&t) else {
                bad.push(format!("{name}: a perturbed Const leaves the walk"));
                continue;
            };
            consts += 1;
            let moved = base.trace.iter().zip(&p.trace).position(|(a, b)| a.0 != b.0);
            let (class, site) = moved.map_or((Class::Word, unvisited), |i| (base.trace[i].1, base.trace[i].2));
            if class != Class::Word {
                *listing.entry(format!("{class:?}\t{:?}.{} (in {:?})", site.plan, site.field, site.parent)).or_default() += 1;
            }
            if base.main == p.main && (!base.needs.0 || base.tl == p.tl) && (!base.needs.1 || base.zone == p.zone) {
                same += 1;
                if base.graph != p.graph {
                    bad.push(format!("{name}: {class:?} {:?}.{} (in {:?}): the front key holds, decompose differs ({:?} / {:?})", site.plan, site.field, site.parent, base.graph.as_ref().map(|g| g.0), p.graph.as_ref().map(|g| g.0)));
                }
            }
        }
    }
    eprintln!("--stats front census: {keyed} fixtures keyed ({filed} fileable), {consts} by-value Consts perturbed, {same} kept the front key");
    eprintln!("--stats front cost over {timed} oltp fixtures: walk {} ns, decompose + key_and_nodes {} ns (mean of the best of five)", walk_ns / timed.max(1), dec_ns / timed.max(1));
    for (k, n) in listing.iter() {
        eprintln!("  by value\t{k}\tx{n}");
    }
    assert!(bad.is_empty(), "{} front-key rule violations:\n{}", bad.len(), bad.join("\n"));
    assert!(keyed > 100 && consts > 500, "the corpus exercises the walk ({keyed} fixtures, {consts} Consts)");
}

/// PM3a: the census of what makes no front key. Over the corpus, every statement the walk refuses names the construct
/// (`front::refusal`), counted by it; the count of those decompose serves anyway is the slow path the arms remove, most
/// frequent first. `refusal` and `walk` agree on every fixture.
#[test]
fn every_refusal_is_named_and_counted() {
    seams();
    let mut by: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let (mut walked, mut refused) = (0, 0);
    for path in fixtures() {
        let text = std::fs::read_to_string(&path).unwrap();
        let ctx = MemoryContext::new("front refusal");
        let Ok(fx) = corpus::load(ctx.mcx(), &text) else { continue };
        let arena = Arena::new(1 << 22);
        let why = front::refusal(arena.handle(), fx.stmt);
        assert_eq!(why.is_none(), front::walk(arena.handle(), fx.stmt, None).is_some(), "{}: refusal and walk disagree", path.display());
        walked += 1;
        let Some((tag, what)) = why else { continue };
        refused += 1;
        let served = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).is_ok();
        let e = by.entry(format!("{tag:?}.{what}")).or_default();
        (e.0, e.1) = (e.0 + 1, e.1 + served as usize);
    }
    let mut rows: Vec<_> = by.into_iter().collect();
    rows.sort_by(|a, b| (b.1 .1, b.1 .0).cmp(&(a.1 .1, a.1 .0)).then(a.0.cmp(&b.0)));
    eprintln!("--stats front refusals: {refused} of {walked} fixtures make no front key");
    for (k, (n, served)) in &rows {
        eprintln!("  refused\t{k}\tx{n}\tdecompose serves {served}");
    }
    assert!(walked > 100, "the corpus exercises the walk ({walked} fixtures)");
    // PM3a: every node and field has its arm (3029 of 5464 refused before; 1 at the arms' landing)
    assert!(refused * 100 <= walked, "{refused} of {walked} corpus fixtures make no front key (at most 1%): a construct lost its arm");
}

/// A filed shape hit by a plan whose literals differ binds that plan's own nodes, and the memo holds its bound.
#[test]
fn a_hit_binds_the_new_plans_nodes_and_the_memo_is_bounded() {
    seams();
    let path = fixtures().into_iter().find(|p| p.to_string_lossy().ends_with("/oltp-001.plan")).or_else(|| fixtures().into_iter().find(|p| {
        let t = std::fs::read_to_string(p).unwrap();
        !const_bytes(&t).is_empty() && run(&t).is_some_and(|o| o.graph.is_ok_and(|g| g.1.is_some()))
    }));
    let text = std::fs::read_to_string(path.expect("a fileable fixture with a Const")).unwrap();
    let (ctx, arena) = (MemoryContext::new("front hit"), Arena::new(1 << 22));
    let fx = corpus::load(ctx.mcx(), &text).unwrap();
    let f = front::walk(arena.handle(), fx.stmt, None).unwrap();
    let g = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).unwrap();
    let mut nodes = Vec::new();
    let key = gravity_lower::inputs::key::key_and_nodes(&g, &mut |n| nodes.push(n));
    front::fill(&f, key, &nodes, front::needs(&f, fx.stmt, &fx.facts));
    // the same text read again: other nodes, the same shape
    let ctx2 = MemoryContext::new("front hit 2");
    let fx2 = corpus::load(ctx2.mcx(), &text).unwrap();
    let f2 = front::walk(arena.handle(), fx2.stmt, None).unwrap();
    let (k2, hit) = front::hit(&f2, arena.handle()).expect("the filed shape hits");
    let g2 = decompose_in(arena.handle(), fx2.stmt, &fx2.facts, fx2.mcx).unwrap();
    let mut fresh = Vec::new();
    assert_eq!(gravity_lower::inputs::key::key_and_nodes(&g2, &mut |n| fresh.push(n)), k2);
    assert!(hit.len() == fresh.len() && hit.iter().zip(&fresh).all(|(a, b)| a.ptr_eq(*b)), "the hit's nodes are the new plan's");
    let mut f3 = front::walk(arena.handle(), fx.stmt, None).unwrap();
    for i in 0..front::FRONT_ENTRIES as u128 + 10 {
        f3.main = i;
        front::fill(&f3, key, &nodes, (false, false));
        assert!(front::memo_len() <= front::FRONT_ENTRIES);
    }
}

/// The `{LABEL ...}` spans of `text` whose node may compile to an interpreter island (gravity_expr `ISLAND_TAGS`; a
/// NULLTEST only with its own `:argisrow true`): `(start, end, label)`. Braces escaped by `\` are text.
fn island_spans(text: &str) -> Vec<(usize, usize, String)> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    for tag in gravity_expr::nodes::ISLAND_TAGS {
        let label = label(tag);
        let open = format!("{{{label} ");
        let mut from = 0;
        while let Some(i) = text[from..].find(&open) {
            let start = from + i;
            from = start + 1;
            let (mut depth, mut at, mut top) = (0usize, start, String::new());
            while at < b.len() {
                match b[at] {
                    b'\\' => at += 1,
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    c if depth == 1 => top.push(c as char),
                    _ => {}
                }
                at += 1;
            }
            if tag != NodeTag::T_NullTest || top.contains(":argisrow true") {
                out.push((start, at, label.clone()));
            }
        }
    }
    out
}

fn label(tag: NodeTag) -> String {
    format!("{tag:?}").trim_start_matches("T_").to_uppercase()
}

/// A plan's keys: the content key over the decomposed graph, the front key's `main` (`None` where the walk keys no
/// statement), and, for each island-capable node the key walk visits, its label and the by-value Const values under it
/// (what `island_consts` hashes). `None` for a plan with a domain coercion: key.rs reads its constraints from the live
/// catalog (`domain_facts`), which no fixture installs.
fn both_keys(text: &str) -> Option<(ContentKey, Option<u128>, Vec<(String, Vec<u64>)>)> {
    if text.contains("{COERCETODOMAIN ") {
        return None;
    }
    let ctx = MemoryContext::new("island keys");
    let fx = corpus::load(ctx.mcx(), text).ok()?;
    let arena = Arena::new(1 << 22);
    let g = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).ok()?;
    let mut islands = Vec::new();
    let key = gravity_lower::inputs::key::key_and_nodes(&g, &mut |n| {
        if gravity_expr::nodes::island_capable(n) {
            let mut vals = Vec::new();
            gravity_decompose::expr::walk(n, &mut |k| {
                if let Some(c) = k.as_const().filter(|c| c.constbyval && !c.constisnull) {
                    vals.push(c.constvalue.as_u64());
                }
                true
            });
            islands.push((label(n.node_tag()), vals));
        }
    });
    Some((key, front::walk(arena.handle(), fx.stmt, None).map(|f| f.main), islands))
}

/// Island kinds the corpus holds no by-value Const under, each with where its constants are covered instead: a new
/// island kind is neither here nor exercised, and fails.
const COVERED_ELSEWHERE: [(NodeTag, &str); 4] = [
    (NodeTag::T_SQLValueFunction, "no argument"),
    (NodeTag::T_JsonValueExpr, "it sits inside a JsonConstructorExpr / JsonExpr / JsonIsPredicate, the island the key walk meets first (a change is counted there); the corpus's others are the raw form decompose's kids do not read"),
    (NodeTag::T_NullTest, "e2e m1_d337_rowcompare's row-valued IS [NOT] NULL at K = 3, 7, 1 (the corpus's row NullTests hold none)"),
    (NodeTag::T_JsonIsPredicate, "its argument is text / json: by-reference constants, whose image key.rs's Const arm always hashes"),
];

/// D337 (main 09-24): an interpreter island's constants are its program's own, so both keys hold them by value. Every
/// island kind (gravity_expr `ISLAND_TAGS`) is iterated over the corpus: each by-value Const inside one is perturbed
/// (the low bit of its first value byte); where the key walk visits it under an island-capable node, the content key
/// and, where the front walk keys the statement, the front key must change. A Const the key walk does not visit there
/// is some other place's (an index key's word, a JsonValueExpr's unread raw form, a table function's own expressions:
/// counted, not judged). A kind with no visited instance fails unless `COVERED_ELSEWHERE` says why.
#[test]
fn every_island_kinds_constants_are_in_both_keys() {
    seams();
    let (mut seen, mut elsewhere, mut bad): (BTreeMap<String, usize>, BTreeMap<String, usize>, Vec<String>) = Default::default();
    for path in fixtures() {
        let text = std::fs::read_to_string(&path).unwrap();
        let spans = island_spans(&text);
        if spans.is_empty() {
            continue;
        }
        let Some(base) = both_keys(&text) else { continue };
        let name = path.file_stem().unwrap().to_string_lossy().to_string();
        for (at, len, byte) in const_bytes(&text) {
            let Some(span) = spans.iter().find(|s| s.0 < at && at < s.1).map(|s| s.2.clone()) else { continue };
            let mut t = text.clone();
            t.replace_range(at..at + len, &(byte ^ 1).to_string());
            let Some(p) = both_keys(&t) else { continue };
            let Some(label) = base.2.iter().zip(&p.2).find(|(a, b)| a != b).map(|(a, _)| a.0.clone()) else {
                *elsewhere.entry(span).or_default() += 1;
                continue;
            };
            if p.0 == base.0 {
                bad.push(format!("{name}: a Const under {label}: the content key is unchanged"));
            }
            if base.1.is_some() && p.1 == base.1 {
                bad.push(format!("{name}: a Const under {label}: the front key is unchanged"));
            }
            *seen.entry(label).or_default() += 1;
        }
    }
    eprintln!("--stats island Consts perturbed and visited: {seen:?}; not visited by the key walk under an island: {elsewhere:?}");
    let missing: Vec<String> = gravity_expr::nodes::ISLAND_TAGS
        .iter()
        .filter(|t| !COVERED_ELSEWHERE.iter().any(|(c, _)| c == *t))
        .map(|t| label(*t))
        .filter(|l| !seen.contains_key(l))
        .collect();
    assert!(bad.is_empty(), "{} island Consts outside a key:\n{}", bad.len(), bad.join("\n"));
    assert!(missing.is_empty(), "island kinds the corpus never exercised over a by-value Const: {missing:?}");
}

/// The `{SUBSCRIPTINGREF ...}` spans of `text`, each with the kinds its own fields show: a constant index, a Param index
/// (the upper list's first node), a slice (a lower list), an assignment (`refassgnexpr`), nested (its container a
/// subscript). Braces escaped by `\` are text.
fn subscript_spans(text: &str) -> Vec<(usize, usize, Vec<&'static str>)> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = text[from..].find("{SUBSCRIPTINGREF ") {
        let start = from + i;
        from = start + 1;
        let (mut depth, mut at) = (0usize, start);
        while at < b.len() {
            match b[at] {
                b'\\' => at += 1,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            at += 1;
        }
        let s = &text[start..at];
        let field = |name: &str| s.find(name).map(|i| &s[i + name.len()..]).unwrap_or("");
        let mut kinds = Vec::new();
        let upper = field(":refupperindexpr ");
        if upper.starts_with("({CONST") {
            kinds.push("const index");
        }
        if upper.starts_with("({PARAM") {
            kinds.push("Param index");
        }
        if !field(":reflowerindexpr ").starts_with("<>") {
            kinds.push("slice");
        }
        if !field(":refassgnexpr ").starts_with("<>") {
            kinds.push("assignment");
        }
        if field(":refexpr ").starts_with("{SUBSCRIPTINGREF") {
            kinds.push("nested");
        }
        out.push((start, at, kinds));
    }
    out
}

/// Main 09-24 (the front walk's SubscriptingRef arm): every subscript kind the corpus holds — a constant index, a Param
/// index, a slice, an assignment, nested subscripts — is walked (its statement keys), and each by-value Const under a
/// subscript, perturbed, either moves the front key or leaves decompose's answer (the content key and the key walk's
/// positions) identical: the key-identity rule of `a_front_key_decides_what_decompose_decides`, counted per kind.
#[test]
fn every_subscript_kind_is_walked_and_its_constants_keep_the_rule() {
    seams();
    const KINDS: [&str; 5] = ["const index", "Param index", "slice", "assignment", "nested"];
    let (mut walked, mut perturbed, mut bad): (BTreeMap<&str, usize>, BTreeMap<&str, usize>, Vec<String>) = Default::default();
    for path in fixtures() {
        let text = std::fs::read_to_string(&path).unwrap();
        let spans = subscript_spans(&text);
        if spans.is_empty() {
            continue;
        }
        let Some(base) = run(&text) else { continue };
        let name = path.file_stem().unwrap().to_string_lossy().to_string();
        for k in spans.iter().flat_map(|s| s.2.iter()) {
            *walked.entry(k).or_default() += 1;
        }
        for (at, len, byte) in const_bytes(&text) {
            let under: Vec<&str> = spans.iter().filter(|s| s.0 < at && at < s.1).flat_map(|s| s.2.iter().copied()).collect();
            if !spans.iter().any(|s| s.0 < at && at < s.1) {
                continue;
            }
            let mut t = text.clone();
            t.replace_range(at..at + len, &(byte ^ 1).to_string());
            let Some(p) = run(&t) else {
                bad.push(format!("{name}: a perturbed Const under a subscript leaves the walk"));
                continue;
            };
            if base.main == p.main && (!base.needs.0 || base.tl == p.tl) && (!base.needs.1 || base.zone == p.zone) && base.graph != p.graph {
                bad.push(format!("{name}: a Const under a subscript ({under:?}): the front key holds, decompose differs ({:?} / {:?})", base.graph.as_ref().map(|g| g.0), p.graph.as_ref().map(|g| g.0)));
            }
            for k in under {
                *perturbed.entry(k).or_default() += 1;
            }
        }
    }
    eprintln!("--stats subscript kinds walked {walked:?}; by-value Consts perturbed under each {perturbed:?}");
    assert!(bad.is_empty(), "{} subscript front-key rule violations:\n{}", bad.len(), bad.join("\n"));
    let unwalked: Vec<&str> = KINDS.iter().copied().filter(|k| !walked.contains_key(k)).collect();
    assert!(unwalked.is_empty(), "subscript kinds the front walk never keyed: {unwalked:?}");
    let unperturbed: Vec<&str> = ["const index", "slice", "assignment", "nested"].into_iter().filter(|k| !perturbed.contains_key(k)).collect();
    assert!(unperturbed.is_empty(), "subscript kinds with no by-value Const perturbed: {unperturbed:?}");
}

/// The planner's estimates in a plan text: every plan node's `startup_cost` / `total_cost` / `plan_rows` (the plans of
/// `PlannedStmt.subplans`, the SubPlans' and the initPlans', included), a Hash's `rows_total` and an RTE's `enrtuples`:
/// the field, the value's offset and length, the value, and whether it sits in `subplans`.
fn estimates(text: &str) -> Vec<(&'static str, usize, usize, f64, bool)> {
    let sub = text.find(":subplans (").map(|at| {
        let mut depth = 0i32;
        let end = text[at..].char_indices().find_map(|(i, c)| {
            depth += (c == '(') as i32 - (c == ')') as i32;
            (c == ')' && depth == 0).then_some(at + i)
        });
        (at, end.unwrap_or(text.len()))
    });
    let mut out = Vec::new();
    for f in ["startup_cost", "total_cost", "plan_rows", "rows_total", "enrtuples"] {
        // the field by its last name: a node prints its Plan's under its own path (`:scan.plan.plan_rows`, `:join.plan.…`)
        let pat = format!("{f} ");
        let mut from = 0;
        while let Some(i) = text[from..].find(&pat) {
            let (at, b) = (from + i, from + i + pat.len());
            let len = text[b..].find([' ', '}', ')']).unwrap_or(text.len() - b);
            from = b + len;
            if !matches!(text[..at].as_bytes().last(), Some(b'.' | b':')) {
                continue;
            }
            if let Ok(v) = text[b..b + len].parse::<f64>() {
                out.push((f, b, len, v, sub.is_some_and(|(s, e)| s < b && b < e)));
            }
        }
    }
    out
}

/// The front walk alone: (main, tl, zone), or `None` when the walk makes no key.
fn front_keys(text: &str) -> Option<(u128, u128, u128)> {
    let ctx = MemoryContext::new("front census");
    let fx = corpus::load(ctx.mcx(), text).ok()?;
    let arena = Arena::new(1 << 22);
    let f = front::walk(arena.handle(), fx.stmt, None)?;
    Some((f.main, f.tl, f.zone))
}

/// D359 (main 09-24, fix (a)): the front key carries what decompose derives from the planner's estimates, never the raw
/// estimates. Every estimate of every fixture (the decompose corpus and regress-full, plus the `.plan` fixtures of the
/// directory `FRONT_CENSUS_EXTRA` names: the e2e recapture) is perturbed twice, to 0 and upward; each perturbation moves
/// the front key or leaves decompose's answer (the content key and the key walk's positions) identical. A fixture the
/// walk does not key (`front::refusal` names why) takes today's path whatever its estimates:
/// its perturbations are counted, not run. The Defect-Test half: an estimate nothing reads keeps the front key (at the
/// parent the raw bits were hashed, and no perturbation kept it).
#[test]
fn every_estimate_moves_the_front_key_or_leaves_decompose_identical() {
    seams();
    let mut paths = fixtures();
    let extra = std::env::var("FRONT_CENSUS_EXTRA").ok().filter(|d| !d.is_empty());
    if let Some(d) = &extra {
        let mut more: Vec<PathBuf> = std::fs::read_dir(d).unwrap().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "plan")).collect();
        more.sort();
        eprintln!("--stats FRONT_CENSUS_EXTRA {d}: {} fixtures", more.len());
        paths.extend(more);
    }
    // (field, in subplans) -> [perturbed, the key moved, the key kept (decompose run)]
    let mut stats: BTreeMap<(&str, bool), [usize; 3]> = BTreeMap::new();
    let (mut keyed, mut unkeyed, mut unkeyed_est, mut unkeyed_sub) = (0, 0, 0, 0);
    let mut bad = Vec::new();
    // the fixtures on the harness's threads (par_map), folded in fixture order: the same counts, stats and messages; a
    // panic in one fixture's work names the fixture
    let per = par_map(&paths, |p| {
        std::panic::catch_unwind(|| estimate_census(p)).unwrap_or_else(|e| {
            let what = e.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| e.downcast_ref::<String>().cloned());
            panic!("{}: {}", p.display(), what.unwrap_or_else(|| "a panic".into()))
        })
    });
    for one in per {
        match one {
            Err((est, sub)) => {
                unkeyed += 1;
                unkeyed_est += est;
                unkeyed_sub += sub;
            }
            Ok((st, b)) => {
                keyed += 1;
                for (k, [n, moved, kept]) in st {
                    let e = stats.entry(k).or_default();
                    e[0] += n;
                    e[1] += moved;
                    e[2] += kept;
                }
                bad.extend(b);
            }
        }
    }
    eprintln!("--stats estimate census: {keyed} fixtures keyed; {unkeyed} not keyed ({unkeyed_est} estimates, {unkeyed_sub} of them in subplans: today's path)");
    for ((f, sub), [n, moved, kept]) in stats.iter() {
        eprintln!("  {f}{}\tperturbed {n}\tkey moved {moved}\tkey kept (decompose identical) {kept}", if *sub { " (in subplans)" } else { "" });
    }
    assert!(bad.is_empty(), "{} estimate front-key rule violations:\n{}", bad.len(), bad.join("\n"));
    assert!(keyed > 100, "the corpus exercises the walk ({keyed} fixtures)");
    let never_kept: Vec<&str> = ["startup_cost", "total_cost", "plan_rows"].into_iter().filter(|f| stats.get(&(*f, false)).is_none_or(|s| s[2] == 0)).collect();
    assert!(never_kept.is_empty(), "estimates the front key still hashes raw (no perturbation kept the key): {never_kept:?}");
}

/// One fixture of `every_estimate_moves_the_front_key_or_leaves_decompose_identical`: `Err((estimates, of them in
/// subplans))` when the walk does not key it, else its (field, in subplans) -> [perturbed, moved, kept] and its violations.
#[allow(clippy::type_complexity)]
fn estimate_census(path: &Path) -> Result<(BTreeMap<(&'static str, bool), [usize; 3]>, Vec<String>), (usize, usize)> {
    let mut stats: BTreeMap<(&'static str, bool), [usize; 3]> = BTreeMap::new();
    let mut bad = Vec::new();
    {
        let text = std::fs::read_to_string(path).unwrap();
        let est = estimates(&text);
        let Some(base) = run(&text) else {
            return Err((est.len(), est.iter().filter(|e| e.4).count()));
        };
        let name = path.file_stem().unwrap().to_string_lossy().to_string();
        for (field, at, len, v, sub) in est {
            for nv in [0.0, v * 3.0 + 1000.0] {
                if nv == v {
                    continue;
                }
                let mut t = text.clone();
                t.replace_range(at..at + len, &format!("{nv:.2}"));
                let e = stats.entry((field, sub)).or_default();
                e[0] += 1;
                let Some(k) = front_keys(&t) else {
                    bad.push(format!("{name}: {field} {v} -> {nv}: the perturbed plan leaves the walk"));
                    continue;
                };
                if k.0 != base.main || (base.needs.0 && k.1 != base.tl) || (base.needs.1 && k.2 != base.zone) {
                    e[1] += 1;
                    continue;
                }
                e[2] += 1;
                let p = run(&t).expect("the walk keyed it");
                if p.graph != base.graph {
                    // the plan node the estimate belongs to: the nearest node name before it in the plan text
                    let node = text[..at].rfind('{').map_or("?", |i| text[i + 1..].split([' ', '\n']).next().unwrap_or("?"));
                    bad.push(format!("{name}: {node} {field} {v} -> {nv}{}: the front key holds, decompose differs ({:?} / {:?})", if sub { " (in subplans)" } else { "" }, base.graph.as_ref().map(|g| g.0), p.graph.as_ref().map(|g| g.0)));
                }
            }
        }
    }
    Ok((stats, bad))
}

/// buildprof BD-15: `f` over `items` on the threads the test harness gives this binary (RUST_TEST_THREADS, which the
/// ct pool sets to the tokens it holds for it; else the CPUs), the results in `items`' order. A panic in `f` fails the
/// test as it would on the test's own thread.
fn par_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let threads = std::env::var("RUST_TEST_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
        .clamp(1, items.len().max(1));
    let next = AtomicUsize::new(0);
    let mut out: Vec<Option<R>> = items.iter().map(|_| None).collect();
    std::thread::scope(|s| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| {
                    let mut mine = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= items.len() {
                            break mine;
                        }
                        mine.push((i, f(&items[i])));
                    }
                })
            })
            .collect();
        for w in workers {
            match w.join() {
                Ok(mine) => mine.into_iter().for_each(|(i, r)| out[i] = Some(r)),
                Err(e) => std::panic::resume_unwind(e),
            }
        }
    });
    out.into_iter().map(|r| r.expect("par_map: every item mapped")).collect()
}

/// Decompose's answer keyed: the content key, the key walk's positions in `f`, the key walk's nodes.
fn keyed<'p>(g: &gravity_decompose::PipelineGraph<'p>, f: &Front<'_>) -> (ContentKey, Option<Vec<u32>>, Vec<Node<'p>>) {
    let mut nodes = Vec::new();
    let k = gravity_lower::inputs::key::key_and_nodes(g, &mut |n| nodes.push(n));
    (k, positions(f, &nodes), nodes)
}

/// A varlena image's payload in place (1-byte or 4-byte uncompressed header), for an in-place perturbation.
///
/// # Safety
/// `p` points at a live varlena image.
unsafe fn payload(p: *mut u8) -> Option<(*mut u8, usize)> {
    let b0 = *p;
    if b0 & 0x01 == 0x01 {
        let len = ((b0 >> 1) & 0x7F) as usize;
        (b0 != 0x01 && len > 1).then(|| (p.add(1), len - 1))
    } else if b0 & 0x03 == 0 {
        let len = (u32::from_le_bytes([*p, *p.add(1), *p.add(2), *p.add(3)]) >> 2) as usize;
        (len > 4).then(|| (p.add(4), len - 4))
    } else {
        None
    }
}

/// V4R-18 (main 09-24, lever (f)): every by-reference Const the walk keys as a word (`Front::byref`) leaves the shape
/// to its variants. Each such varlena literal of the corpus is perturbed in place (its first payload byte, an `= ANY`
/// list's last one: its count is the shape, V4R-110; the same length):
/// - the main key holds (the image is not the shape's); a variant filed with every image baked (`fill`) misses, and one
///   filed with the images decompose logged (`fill_lowered`, nothing lowered) misses exactly when this one was logged;
/// - decompose's answer on the perturbed plan, keyed after the byte is restored, equals the original unless decompose
///   logged the literal's image as read (`gravity_decompose::bake`): a read that decides the graph is logged. The
///   lowering's reads are the shadow check's (exec admit `front_check` lowers again on a variant hit).
#[test]
fn every_byref_word_keeps_the_shape_and_decompose_logs_what_it_reads() {
    seams();
    let (mut fixtures_n, mut words, mut held, mut logged_moved, mut logged_held) = (0, 0, 0, 0, 0);
    let mut bad = Vec::new();
    for path in fixtures() {
        let text = std::fs::read_to_string(&path).unwrap();
        let name = path.file_stem().unwrap().to_string_lossy().to_string();
        let ctx = MemoryContext::new("front census byref");
        let Ok(fx) = corpus::load(ctx.mcx(), &text) else { continue };
        let arena = Arena::new(1 << 22);
        let Some(f) = front::walk(arena.handle(), fx.stmt, None) else { continue };
        if f.byref.is_empty() {
            continue;
        }
        let Ok(g) = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx) else { continue };
        let (key, pos, nodes) = keyed(&g, &f);
        let logged = f.plan_reads();
        fixtures_n += 1;
        let needs = front::needs(&f, fx.stmt, &fx.facts);
        // V4R-110: an `= ANY` list's count is the shape (the front key puts its `list_class`), so a list's word is
        // perturbed past its dimensions header (ndim, dataoffset, elemtype, then dims and lower bounds): its last byte
        let lists: Vec<usize> = f.nodes.iter().filter_map(|n| n.as_scalar_array_op_expr()).filter_map(|o| o.args.nth(1).as_const().map(|k| k.constvalue.as_usize())).collect(); // alloc: Cold
        for &p in f.byref.iter() {
            let k = f.nodes[p as usize].as_const().unwrap();
            let img = k.constvalue.as_usize();
            // SAFETY: a non-null by-reference varlena constant of the loaded plan, in the test's own arena.
            let Some((at, len)) = (k.constlen == -1).then(|| unsafe { payload(img as *mut u8) }).flatten() else { continue };
            let at = if !lists.contains(&img) {
                at
            } else {
                // SAFETY: the payload holds at least ndim (its first 4 bytes are checked by the length).
                let ndim = if len >= 4 { unsafe { at.cast::<i32>().read_unaligned() }.max(0) as usize } else { 0 };
                if len <= 12 + 8 * ndim {
                    continue; // an empty list: no element byte
                }
                unsafe { at.add(len - 1) }
            };
            words += 1;
            let read = logged.binary_search(&img).is_ok();
            // the variants, each filed from the original image (a shape decompose keys through a node the walk does
            // not name files none): every image baked, then the ones decompose logged
            let fileable = pos.is_some();
            front::clear();
            front::fill(&f, key, &nodes, needs);
            unsafe { *at ^= 1 };
            let a2 = Arena::new(1 << 22);
            let f2 = front::walk(a2.handle(), fx.stmt, None).expect("the perturbed plan keys");
            if f2.main != f.main || f2.tl != f.tl || f2.zone != f.zone {
                bad.push(format!("{name}: a by-reference word's image moved the shape"));
            }
            if front::hit(&f2, a2.handle()).is_some() {
                bad.push(format!("{name}: a variant with every image baked hit another image"));
            }
            unsafe { *at ^= 1 };
            front::clear();
            front::fill_lowered(&f, key, &nodes, needs, 0);
            unsafe { *at ^= 1 };
            if fileable && front::hit(&f2, a2.handle()).is_some() == read {
                bad.push(format!("{name}: a variant baking what decompose logged (this word: {read}) answered another image wrongly"));
            }
            // decompose on the perturbed image, keyed after the restore: the same answer unless it logged the read
            let g2 = decompose_in(a2.handle(), fx.stmt, &fx.facts, fx.mcx);
            unsafe { *at ^= 1 };
            let same = g2.as_ref().is_ok_and(|g2| {
                let (k2, pos2, _) = keyed(g2, &f2);
                k2 == key && pos2 == pos
            });
            match (same, read) {
                (true, false) => held += 1,
                (true, true) => logged_held += 1,
                (false, true) => logged_moved += 1,
                (false, false) => bad.push(format!("{name}: decompose's answer moved with a by-reference word's image it did not log as read (position {p})")),
            }
        }
    }
    eprintln!("--stats by-reference words: {fixtures_n} fixtures, {words} varlena words perturbed; decompose's answer held {held} (unlogged), held {logged_held} (logged), moved {logged_moved} (logged)");
    assert!(bad.is_empty(), "{} by-reference word violations:\n{}", bad.len(), bad.join("\n"));
    assert!(words > 100, "the corpus exercises by-reference words ({words})");
}

/// A plan's text with what the front key leaves out by design removed (V4R-18's plan-shape guarantee): every Const's
/// value (a word, or a by-reference image the variants carry), the `location`s, `queryId` / `stmt_location` /
/// `stmt_len`, the planner's raw estimates (D359) and a range table entry's names (`alias` / `eref`: EXPLAIN's). What remains is the plan's shape: every node, its kind and
/// parameters, types, collations, operators, indexes and join order.
fn shape_text(text: &str) -> String {
    let plan = text.lines().find(|l| l.starts_with('{')).unwrap_or("");
    let skip_one = [":location", ":queryId", ":stmt_location", ":stmt_len", ":rows_total", ":enrtuples"];
    let est = ["startup_cost", "total_cost", "plan_rows"];
    let mut out = String::with_capacity(plan.len());
    let mut it = plan.split_whitespace();
    while let Some(t) = it.next() {
        if skip_one.contains(&t) || est.iter().any(|e| t.starts_with(':') && t.ends_with(e)) {
            it.next();
        } else if t == ":alias" || t == ":eref" {
            // a range table entry's names (setrefs keeps them for EXPLAIN; nothing executes them)
            let mut depth = 0i32;
            for u in it.by_ref() {
                depth += u.matches('{').count() as i32 - u.matches('}').count() as i32;
                if depth <= 0 {
                    break;
                }
            }
        } else if t == ":constvalue" {
            if it.next() != Some("<>") && it.next() == Some("[") {
                for u in it.by_ref() {
                    if u == "]" || u.starts_with(']') {
                        break;
                    }
                }
            }
        } else {
            out.push_str(t);
            out.push(' ');
        }
    }
    out
}

/// A statement's text with its literals replaced by `?` (the simple protocol's repeats of one statement).
fn sql_template(sql: &str) -> String {
    let (mut out, mut chars) = (String::new(), sql.chars().peekable());
    while let Some(c) = chars.next() {
        if c == '\'' {
            while let Some(d) = chars.next() {
                if d == '\'' && chars.peek() != Some(&'\'') {
                    break;
                } else if d == '\'' {
                    chars.next();
                }
            }
            out.push('?');
        } else if c.is_ascii_digit() && !out.ends_with(|p: char| p.is_ascii_alphanumeric() || p == '_') {
            while chars.peek().is_some_and(|d| d.is_ascii_digit() || *d == '.') {
                chars.next();
            }
            out.push('?');
        } else {
            out.push(c);
        }
    }
    out
}

/// V4R-18's plan-shape guarantee (the user 09-24, decision 11: "we just have to make sure the plan shape is the same"):
/// two plans share a front key only when their shapes are identical. Over the whole corpus, every pair of fixtures with
/// one main key has one `shape_text`; so a literal that makes the planner pick another plan (an index scan for a rare
/// value, a sequential scan for a common one, another join order, a constant-folded branch) makes another key. The
/// fixtures that are one statement with other literals (one `sql_template`) and another plan are counted: each pair
/// holds two keys. The converse (one shape, another by-reference image: one key) is
/// `every_byref_word_keeps_the_shape_and_decompose_logs_what_it_reads`.
#[test]
fn one_front_key_means_one_plan_shape() {
    seams();
    let mut by_key: BTreeMap<u128, (String, String)> = BTreeMap::new();
    let mut by_template: BTreeMap<String, Vec<(u128, String, String)>> = BTreeMap::new();
    let (mut keyed, mut bad) = (0usize, Vec::new());
    for path in fixtures() {
        let text = std::fs::read_to_string(&path).unwrap();
        let name = path.file_stem().unwrap().to_string_lossy().to_string();
        let Some((main, _, _)) = front_keys(&text) else { continue };
        keyed += 1;
        let shape = shape_text(&text);
        let sql = text.lines().find_map(|l| l.strip_prefix("-- sql: ")).unwrap_or("").to_string();
        match by_key.get(&main) {
            Some((other, s)) if *s != shape => bad.push(format!("{name} and {other}: one front key, two plan shapes")),
            Some(_) => {}
            None => {
                by_key.insert(main, (name.clone(), shape.clone()));
            }
        }
        by_template.entry(sql_template(&sql)).or_default().push((main, shape, name));
    }
    let (mut replans, mut same_shape) = (0usize, 0usize);
    for v in by_template.values() {
        for (i, a) in v.iter().enumerate() {
            for b in &v[i + 1..] {
                if a.1 != b.1 {
                    replans += 1;
                    if a.0 == b.0 {
                        bad.push(format!("{} and {}: one statement, another plan for another literal, one front key", a.2, b.2));
                    }
                } else {
                    same_shape += 1;
                }
            }
        }
    }
    eprintln!("--stats plan shapes: {keyed} fixtures keyed, {} front keys; {replans} same-statement pairs planned otherwise (each two keys), {same_shape} same-statement pairs of one shape", by_key.len());
    assert!(bad.is_empty(), "{} front keys over two plan shapes:\n{}", bad.len(), bad.join("\n"));
    assert!(keyed > 100, "the corpus exercises the walk ({keyed} fixtures)");
}

/// The same guarantee on one plan edited by hand the ways a replan moves it: another index of the relation, another
/// scan direction, another width: each another front key; the estimates alone (D359): one key.
#[test]
fn a_literal_that_replans_makes_another_front_key() {
    seams();
    let path = fixtures().into_iter().find(|p| p.to_string_lossy().ends_with("/oltp-001.plan")).expect("oltp-001.plan");
    let text = std::fs::read_to_string(path).unwrap();
    let base = front_keys(&text).expect("oltp-001 keys").0;
    // another index of the same relation (customer's second unique index, 16517): the planner's other choice
    let other_index = text.replacen(":indexid 16410", ":indexid 16517", 1);
    assert_ne!(other_index, text);
    assert_ne!(front_keys(&other_index).expect("keys").0, base, "another index, one front key");
    // another scan direction (a backward scan: ORDER BY ... DESC)
    let backward = text.replacen(":indexorderdir 1", ":indexorderdir -1", 1);
    if backward != text {
        assert_ne!(front_keys(&backward).expect("keys").0, base, "another scan direction, one front key");
    }
    // another width (another projection) and another plan node id
    let width = text.replacen(":scan.plan.plan_width 26", ":scan.plan.plan_width 27", 1);
    assert_ne!(front_keys(&width).expect("keys").0, base, "another width, one front key");
    // the estimates alone (D359): one key
    let rows = text.replacen(":scan.plan.plan_rows 1 ", ":scan.plan.plan_rows 2 ", 1);
    assert_ne!(rows, text);
    assert_eq!(front_keys(&rows).expect("keys").0, base, "an estimate moved the front key");
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}

/// D3 (front walk): the walk asks normalize `op_classes` once per plan OpExpr in place of the six class fns; each class
/// it answers is that fn's own answer, over every OpExpr the walk keys in the corpus and regress-full.
#[test]
fn op_classes_are_each_class_fns_answer() {
    use gravity_decompose::normalize::{self, OpClasses};
    seams();
    let none = OpClasses { retype: None, detext: None, identity: None, implied: None, twin: None, coalesce: None };
    let (mut ops, mut classed) = (0usize, 0usize);
    for p in fixtures() {
        let text = std::fs::read_to_string(&p).unwrap();
        let ctx = MemoryContext::new("op classes");
        let Ok(fx) = corpus::load(ctx.mcx(), &text) else { continue };
        let arena = Arena::new(1 << 22);
        let Some(f) = front::walk(arena.handle(), fx.stmt, None) else { continue };
        for &n in f.nodes.iter().filter(|n| n.as_op_expr().is_some()) {
            let got = normalize::op_classes(n);
            let each = OpClasses {
                retype: normalize::retype_class(n),
                detext: normalize::detext_class(n),
                identity: normalize::identity_class(n),
                implied: normalize::implied_class_of(n),
                twin: normalize::twin_class(n),
                coalesce: normalize::coalesce_filter_class(n),
            };
            assert_eq!(got, each, "{}", p.display());
            ops += 1;
            classed += usize::from(got != none);
        }
    }
    eprintln!("op classes: {ops} OpExprs, {classed} with a class");
    assert!(ops > 1000 && classed > 0, "{ops} OpExprs, {classed} with a class: the corpus reaches the classes");
}
