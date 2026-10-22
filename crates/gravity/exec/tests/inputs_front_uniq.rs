//! V4R-566 (front.rs `pin`, decompose demand/readers.rs `uniq`): the front key against decompose over the two Append-free
//! fixtures the unique-ifying Agg rewrite applies to (decompose tests/uniq: partition_join-028's arm 1, a hash join child,
//! and arm 3, a nest child over an expression index), under the server's view (every relation known: a fixture's own
//! facts read as a possible lazy build, where decompose makes no rewrite). `front_census.rs`'s rule: every by-value Const
//! perturbed (the low bit of its first byte); where the front key still says "the same shape", decompose must agree.
//! The rewrite compares the Consts of the loop's index clauses with the child's clause, so the pin keys them by value.
use std::path::Path;

use gravity_decompose::catalog::{IndexExt, LeafFact, RelExt};
use gravity_decompose::effects::FuncProps;
use gravity_decompose::{corpus, decompose_in, Facts};
use gravity_exec::inputs::front;
use gravity_program::{Arena, ContentKey};
use mcx::MemoryContext;
use types_nodes::Node;

struct Server<'f> {
    fx: &'f corpus::FixtureFacts,
}

impl Facts for Server<'_> {
    fn rel(&self, relid: u32) -> Option<gravity_decompose::RelFact> {
        self.fx.rel(relid)
    }
    fn att(&self, relid: u32, attno: i16) -> Option<gravity_decompose::AttFact> {
        self.fx.att(relid, attno)
    }
    fn att_range(&self, relid: u32, attno: i16) -> Option<(i64, i64)> {
        self.fx.att_range(relid, attno)
    }
    fn shred_election(&self, relid: u32, attno: i16, keys: &[&str]) -> Option<(u8, i32)> {
        self.fx.shred_election(relid, attno, keys)
    }
    fn index(&self, indexid: u32) -> Option<gravity_decompose::IndexFact> {
        self.fx.index(indexid)
    }
    fn index_am(&self, indexid: u32) -> gravity_decompose::IndexAm {
        self.fx.index_am(indexid)
    }
    fn indexes_of(&self, relid: u32, f: &mut dyn FnMut(gravity_decompose::IndexFact)) {
        self.fx.indexes_of(relid, f)
    }
    fn index_ext(&self, indexid: u32) -> IndexExt {
        self.fx.index_ext(indexid)
    }
    fn att_generated(&self, relid: u32, attno: i16) -> u8 {
        self.fx.att_generated(relid, attno)
    }
    fn key(&self, req: gravity_decompose::catalog::KeyReq) -> Option<gravity_decompose::catalog::KeyOrder> {
        self.fx.key(req)
    }
    fn op_image_eq(&self, opno: u32) -> Option<(u32, u32)> {
        self.fx.op_image_eq(opno)
    }
    fn rel_ext(&self, relid: u32) -> RelExt {
        RelExt { known: self.fx.rel(relid).is_some(), ..self.fx.rel_ext(relid) }
    }
    fn instead_of(&self, relid: u32) -> gravity_decompose::InsteadOf {
        self.fx.instead_of(relid)
    }
    fn trig_ext(&self, relid: u32) -> gravity_decompose::ops::modifytable::trig_facts::TrigExt {
        self.fx.trig_ext(relid)
    }
    fn partition_leaves(&self, root: u32, f: &mut dyn FnMut(LeafFact)) -> bool {
        self.fx.partition_leaves(root, f)
    }
    fn index_expr(&self, indexid: u32, col: u8) -> Option<gravity_decompose::ExprFp> {
        self.fx.index_expr(indexid, col)
    }
    fn func(&self, funcid: u32) -> Option<FuncProps> {
        self.fx.func(funcid)
    }
}

/// Every non-null by-value Const's first value byte in the plan text (`front_census.rs`'s rule, `-- idxexprs` skipped).
fn const_bytes(text: &str) -> Vec<(usize, usize, u8)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = text[from..].find("{CONST ") {
        let at = from + i;
        let end = at + text[at..].find('}').unwrap();
        let c = &text[at..end];
        from = end;
        let line = text[..at].rfind('\n').map_or(0, |n| n + 1);
        if text[line..at].starts_with("-- idxexprs ") || !c.contains(":constbyval true :constisnull false") {
            continue;
        }
        let v = at + c.find(":constvalue ").unwrap();
        let b = v + text[v..].find("[ ").unwrap() + 2;
        let len = text[b..].find(' ').unwrap();
        out.push((b, len, text[b..b + len].parse().unwrap()));
    }
    out
}

struct Outcome {
    main: u128,
    tl: u128,
    zone: u128,
    needs: (bool, bool),
    graph: Result<(ContentKey, Option<Vec<u32>>, bool), ()>,
}

fn run(text: &str) -> Option<Outcome> {
    let ctx = MemoryContext::new("front uniq");
    let fx = corpus::load(ctx.mcx(), text).ok()?;
    let server = Server { fx: &fx.facts };
    let arena = Arena::new(1 << 22);
    let f = front::walk(arena.handle(), fx.stmt, None)?;
    let needs = front::needs(&f, fx.stmt, &server);
    let graph = decompose_in(arena.handle(), fx.stmt, &server, fx.mcx).map_err(|_| ()).map(|g| {
        let mut nodes: Vec<Node<'_>> = Vec::new();
        let k = gravity_lower::inputs::key::key_and_nodes(&g, &mut |n| nodes.push(n));
        let pos = nodes.iter().map(|n| f.nodes.iter().position(|m| m.ptr_eq(*n)).map(|i| i as u32)).collect();
        let mut dump = String::new();
        gravity_decompose::dump::dump(&g, &mut dump).unwrap();
        (k, pos, dump.contains("2=c23:null, 3=c23:null"))
    });
    Some(Outcome { main: f.main, tl: f.tl, zone: f.zone, needs, graph })
}

#[test]
fn the_pin_keys_what_the_rewrite_compares() {
    let (mut kept, mut moved) = (0, 0);
    let mut bad = Vec::new();
    for name in ["arm1", "arm3"] {
        let path = manifest_dir().join("../decompose/tests/uniq").join(format!("{name}.plan"));
        let text = std::fs::read_to_string(&path).unwrap();
        let base = run(&text).expect("the fixture keys");
        assert!(base.graph.as_ref().is_ok_and(|g| g.2), "{name}: the rewrite applies under the server's view");
        for (at, len, byte) in const_bytes(&text) {
            let mut t = text.clone();
            t.replace_range(at..at + len, &(byte ^ 1).to_string());
            let p = run(&t).expect("a perturbed Const keeps the walk");
            if base.main == p.main && (!base.needs.0 || base.tl == p.tl) && (!base.needs.1 || base.zone == p.zone) {
                kept += 1;
                if base.graph != p.graph {
                    bad.push(format!("{name}: the Const at {at} keeps the front key, decompose differs"));
                }
            } else {
                moved += 1;
            }
        }
    }
    eprintln!("--stats front uniq: {kept} perturbations kept the front key, {moved} moved it");
    assert!(bad.is_empty(), "{} front-key rule violations:\n{}", bad.len(), bad.join("\n"));
    assert!(moved > 0, "the pinned Consts key by value");
}

/// memoize lane (main 10-03 item 2): the rewritten loop's key walk visits the nodes the rewrite synthesized (the loop's one
/// NestLoopParam over an OUTER_VAR Var bound to the Agg's key, the PARAM_EXEC Param standing for the key expression in the
/// inner leaf's clauses, any clause the swap copied, and the Agg's passengers nulled: a NULL Const under its TargetEntry),
/// which no front walk holds, so `front::file` refused the shape
/// ("a key-walk node outside the plan") and every execution decomposed and keyed again. Now they file as D644 SYNTH
/// positions (no word or check reads one): the first execution files, the second is a front hit binding its own plan's
/// nodes, with a stand-in at exactly the synthesized positions.
#[test]
fn the_rewritten_loop_files_and_the_next_execution_hits() {
    for name in ["arm1", "arm3"] {
        let text = std::fs::read_to_string(manifest_dir().join("../decompose/tests/uniq").join(format!("{name}.plan"))).unwrap();
        let (ctx, arena) = (MemoryContext::new("front uniq file"), Arena::new(1 << 22));
        let fx = corpus::load(ctx.mcx(), &text).unwrap();
        let server = Server { fx: &fx.facts };
        let f = front::walk(arena.handle(), fx.stmt, None).unwrap_or_else(|| panic!("{name}: the front walk keys the plan"));
        let g = decompose_in(arena.handle(), fx.stmt, &server, fx.mcx).unwrap_or_else(|_| panic!("{name}: decomposes"));
        let mut dump = String::new();
        gravity_decompose::dump::dump(&g, &mut dump).unwrap();
        assert!(dump.contains("2=c23:null, 3=c23:null"), "{name}: the rewrite applies under the server's view");
        let mut nodes: Vec<Node<'_>> = Vec::new();
        let key = gravity_lower::inputs::key::key_and_nodes(&g, &mut |n| nodes.push(n));
        let outside: Vec<u32> = (0..nodes.len() as u32).filter(|&i| !f.nodes.iter().any(|m| m.ptr_eq(nodes[i as usize]))).collect();
        let tags: Vec<types_nodes::NodeTag> = outside.iter().map(|&i| nodes[i as usize].node_tag()).collect();
        eprintln!("--stats {name}: {} key-walk nodes, outside the plan {tags:?}", nodes.len());
        assert!(tags.contains(&types_nodes::NodeTag::T_NestLoopParam) && tags.contains(&types_nodes::NodeTag::T_Param), "{name}: the key walk visits the synthesized parameter nodes");
        front::clear();
        assert_eq!(front::fill_reading(&f, key, &nodes, front::needs(&f, fx.stmt, &server), &[]), Ok(()), "{name}: filed");
        // the second execution: the same text read again (other nodes, the same shape)
        let ctx2 = MemoryContext::new("front uniq file 2");
        let fx2 = corpus::load(ctx2.mcx(), &text).unwrap();
        let server2 = Server { fx: &fx2.facts };
        let f2 = front::walk(arena.handle(), fx2.stmt, None).unwrap();
        let (k2, hit) = front::hit(&f2, arena.handle()).unwrap_or_else(|| panic!("{name}: a front hit on the 2nd execution"));
        assert_eq!(k2, key, "{name}");
        assert_eq!(front::filed_synth(&f2), outside, "{name}: the stand-ins sit at exactly the synthesized positions");
        let g2 = decompose_in(arena.handle(), fx2.stmt, &server2, fx2.mcx).unwrap();
        let mut fresh = Vec::new();
        assert_eq!(gravity_lower::inputs::key::key_and_nodes(&g2, &mut |n| fresh.push(n)), key, "{name}");
        assert_eq!(hit.len(), fresh.len(), "{name}");
        for (i, (a, b)) in hit.iter().zip(&fresh).enumerate() {
            if outside.contains(&(i as u32)) {
                assert_eq!(b.node_tag(), nodes[i].node_tag(), "{name}: the fresh decompose synthesizes the same kind at {i}");
            } else {
                assert!(a.ptr_eq(*b), "{name}: the hit's node {i} is the new plan's");
            }
        }
    }
}

fn manifest_dir() -> &'static Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test"))
}
