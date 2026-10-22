//! V4R-566 (W2-NOCOL-UNIQ-PASSENGER-R2, demand/readers.rs `uniq`): a unique-ifying HashAggregate's passengers that the Nested
//! Loop above reads only through its index parameters, inside one key expression that the Agg's child equates with the
//! key (a hash clause, or a nest child's index clause), ride as NULLs, and the loop binds one parameter to the key.
//! regress partition_join-028 (partitionwise, three arms) and two of its arms hand-lifted Append-free (tests/uniq/
//! arm1.plan: a hash join child; arm3.plan: a nest child over an expression index), each under the server's view
//! (`Server`: every relation known; a fixture answers unknown, which F5 reads as a possible lazy build: no rewrite). Each
//! does-not-apply plan is one of them edited in memory (each edit asserted to match) and keeps the plan's parameters.
use std::fs;

use gravity_decompose::catalog::{IndexExt, LeafFact, RelExt};
use gravity_decompose::effects::FuncProps;
use gravity_decompose::{corpus, decompose_in, dump, Facts};
use gravity_program::Arena;
use mcx::MemoryContext;

const ARM1: &str = "uniq/arm1.plan";
const ARM3: &str = "uniq/arm3.plan";
const PJ: &str = "plans/regress-full/partition_join-028.plan";
const NULLED: &str = "project (1=outer.1, 2=c23:null, 3=c23:null)";
const KEPT: &str = "project (1=outer.1, 2=outer.2, 3=outer.3)";

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

fn read(rel: &str) -> String {
    fs::read_to_string(manifest_dir().join("tests").join(rel)).unwrap()
}

/// The plan decomposed under the server's view (`server`) or the fixture's own facts.
fn graph(text: &str, server: bool) -> String {
    let ctx = MemoryContext::new("uniq_passenger");
    let fx = corpus::load(ctx.mcx(), text).unwrap();
    let s = Server { fx: &fx.facts };
    let facts: &dyn Facts = if server { &s } else { &fx.facts };
    let arena = Arena::new(1 << 22);
    let mut out = String::new();
    let g = decompose_in(arena.handle(), fx.stmt, facts, fx.mcx).expect("decomposes");
    dump::dump(&g, &mut out).unwrap();
    out
}

/// `text` with the first `from` after the first `after` as `to`.
fn edit(text: &str, after: &str, from: &str, to: &str) -> String {
    let a = text.find(after).unwrap_or_else(|| panic!("no {after}"));
    let f = a + text[a..].find(from).unwrap_or_else(|| panic!("no {from} after {after}"));
    format!("{}{}{}", &text[..f], to, &text[f + from.len()..])
}

fn rewritten(g: &str, p0: &str, leaf: &str) {
    assert!(g.contains(NULLED), "{g}");
    assert!(g.contains(&format!("params=(${p0}=outer.1)")), "{g}");
    assert!(g.contains(&format!("key att=1 op=96 value=${p0}")) && g.contains(&format!("recheck (op96({leaf}.1, ${p0}))")), "{g}");
    assert!(g.contains(&format!("param ${p0} owner=")), "{g}");
}

fn kept(g: &str, p0: &str, p1: &str) {
    assert!(g.contains(&format!("{KEPT}\n  nest inner=")) && !g.contains(NULLED), "{g}");
    assert!(g.contains(&format!("params=(${p0}=outer.2, ${p1}=outer.3)")), "{g}");
}

#[test]
fn each_arm_binds_its_key_and_its_passengers_ride_as_nulls() {
    rewritten(&graph(&read(ARM1), true), "exec0", "v5");
    rewritten(&graph(&read(ARM3), true), "exec5", "v7");
    let pj = graph(&read(PJ), true);
    rewritten(&pj, "exec0", "v5");
    rewritten(&pj, "exec2", "v6");
    rewritten(&pj, "exec5", "v7");
    assert!(!pj.contains("$exec1") && !pj.contains("$exec3") && !pj.contains("$exec6"), "{pj}");
    // the child nest's own parameter (arm 3) is the plan's
    assert!(pj.contains("params=($exec4=outer.1)"), "{pj}");
}

#[test]
fn under_a_possible_lazy_build_the_plan_stands() {
    kept(&graph(&read(ARM1), false), "exec0", "exec1");
    kept(&graph(&read(PJ), false), "exec5", "exec6");
}

#[test]
fn does_not_apply() {
    let arm1 = read(ARM1);
    // C's side is another expression than the loop's (/ 3 against / 2)
    kept(&graph(&edit(&arm1, ":hashclauses (", "[ 2 0 0 0 0 0 0 0 ]", "[ 3 0 0 0 0 0 0 0 ]"), true), "exec0", "exec1");
    // the leaf's two reads differ (F1)
    kept(&graph(&edit(&arm1, ":indexqualorig (", "[ 2 0 0 0 0 0 0 0 ]", "[ 3 0 0 0 0 0 0 0 ]"), true), "exec0", "exec1");
    // no P parameter of the key expression's type (F2)
    kept(&graph(&edit(&arm1, ":paramExecTypes (o 23 23", "(o 23 23", "(o 20 20"), true), "exec0", "exec1");
    // the group's equality is not a same-type IMAGE_EQ (int48eq)
    kept(&graph(&edit(&arm1, ":grpOperators ( 96)", "96", "15"), true), "exec0", "exec1");
    // a SEMI child emits no row per member
    kept(&graph(&edit(&arm1, "{HASHJOIN ", ":join.jointype 0", ":join.jointype 4"), true), "exec0", "exec1");
    // the loop's own join clause reads a passenger (`own`): inner.1 = outer.2 for inner.1 = outer.1
    kept(&graph(&edit(&arm1, ":join.joinqual (", "{VAR :varno -2 :varattno 1 ", "{VAR :varno -2 :varattno 2 "), true), "exec0", "exec1");
    // the leaf's target list reads a P parameter (`quiet`): $exec0 for its column b
    let b = "{VAR :varno 5 :varattno 2 :vartype 23 :vartypmod -1 :varcollid 0 :varnullingrels (b) :varlevelsup 0 :varreturningtype 0 :varnosyn 5 :varattnosyn 2 :location -1}";
    let p0 = "{PARAM :paramkind 1 :paramid 0 :paramtype 23 :paramtypmod -1 :paramcollid 0 :location -1}";
    kept(&graph(&edit(&arm1, ":join.plan.righttree {INDEXSCAN", &format!("{b} :resno 2"), &format!("{p0} :resno 2")), true), "exec0", "exec1");
    let arm3 = read(ARM3);
    // the child's index clause is another expression than the loop's
    kept(&graph(&edit(&arm3, ":scan.scanrelid 13", "[ 2 0 0 0 0 0 0 0 ]", "[ 3 0 0 0 0 0 0 0 ]"), true), "exec5", "exec6");
}

fn manifest_dir() -> std::path::PathBuf {
    // D793-b: the checkout under test, read at run time (cargo sets it for every test process), never the compile-time path
    std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)")
}
