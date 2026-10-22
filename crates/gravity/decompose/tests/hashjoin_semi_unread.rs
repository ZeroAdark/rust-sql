//! V4R-540 (W2-M7-Q20, demand/readers.rs semi_unread): a SEMI hash join's bare inner column (an equivalence-class member the
//! planner keeps for a join above) that no reader above reads rides as a typed NULL, so the lowerer's SEMI probe (the
//! outer row alone) serves it. Two captures take it: tpch-sf10-020 (TPC-H q20 on pgrcolumnar: the Hash Semi Join under
//! a Hash whose join never reads the column) and regress partition_join-029 (partitionwise: per Append member, the semi
//! under a unique-ifying HashAggregate that emits the column bare to a Nested Loop reading only its key). Each
//! does-not-apply plan is one of them edited in memory (each edit asserted to match), and keeps the inner column: the
//! lowerer's "a column the row does not carry" refusal stands. The rule reads the statement's F5 facts (a lazy build
//! anywhere keeps every column: D2147 / D2168), so each capture decomposes under the server's view of it (`Server`).
use std::fs;

use gravity_decompose::catalog::{IndexExt, LeafFact, RelExt};
use gravity_decompose::effects::{FuncProps, Parallel, Volatility};
use gravity_decompose::{corpus, decompose_in, dump, Facts};
use gravity_program::Arena;
use mcx::MemoryContext;

const TH20: &str = "plans/tpch-sf10-020.plan";
const PJ: &str = "plans/regress-full/partition_join-029.plan";
const TH20_NULL: &str = "project (1=outer.1, 2=outer.2, 3=outer.3, 4=c23:null)";
const TH20_KEPT: &str = "project (1=outer.1, 2=outer.2, 3=outer.3, 4=inner.1)";
const PJ_NULL: &str = "project (1=outer.1, 2=c23:null, 3=c23:null)";
const PJ_KEPT: &str = "project (1=outer.1, 2=inner.1, 3=inner.2)";
/// The first Append member's HashAggregate fields (partition_join-029 has one per member, in member order).
const PJ_AGG: &str = ":aggstrategy 2 :aggsplit 0 :numCols 1 :grpColIdx ( 1) :grpOperators ( 96) :grpCollations ( 0)";
/// A plpgsql user function's oid (outside the builtin catalog).
const USER_FN: u32 = 26167;

/// The server's view of a capture: the fixture's facts, but every relation the fixture names is known (`rel_ext`, the
/// server's relcache: a fixture cannot tell a temporary relation, so it answers unknown, which F5 reads as a
/// parallel-unsafe scan and the rule as a possible lazy build), and `USER_FN` has provolatile `user` (Parallel::Unsafe,
/// as the server's facts read every user function).
struct Server<'f> {
    fx: &'f corpus::FixtureFacts,
    user: Volatility,
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
        match funcid {
            USER_FN => Some(FuncProps { volatility: self.user, strict: true, leakproof: false, parallel: Parallel::Unsafe }),
            f => self.fx.func(f),
        }
    }
}

fn read(rel: &str) -> String {
    fs::read_to_string(manifest_dir().join("tests").join(rel)).unwrap()
}

/// The capture decomposed under the server's view (`USER_FN` IMMUTABLE).
fn graph(text: &str) -> String {
    graph_as(text, Some(Volatility::Immutable))
}

/// The capture decomposed under the server's view with `USER_FN` of volatility `user`, or (`None`) the fixture's own facts.
fn graph_as(text: &str, user: Option<Volatility>) -> String {
    decomposed(text, user).expect("decomposes")
}

/// `graph_as`, or the refusal's reason.
fn decomposed(text: &str, user: Option<Volatility>) -> Result<String, &'static str> {
    let ctx = MemoryContext::new("semi_unread");
    let fx = corpus::load(ctx.mcx(), text).unwrap();
    let server = user.map(|user| Server { fx: &fx.facts, user });
    let facts: &dyn Facts = match &server {
        Some(s) => s,
        None => &fx.facts,
    };
    let arena = Arena::new(1 << 22);
    let mut out = String::new();
    let g = decompose_in(arena.handle(), fx.stmt, facts, fx.mcx).map_err(|r| r.reason)?;
    dump::dump(&g, &mut out).unwrap();
    Ok(out)
}

/// An int4 `Var` node's text as the captures spell it.
fn var(varno: i32, attno: i32, (nosyn, attnosyn): (i32, i32), loc: i32) -> String {
    format!(
        "{{VAR :varno {varno} :varattno {attno} :vartype 23 :vartypmod -1 :varcollid 0 :varnullingrels (b) :varlevelsup 0 \
         :varreturningtype 0 :varnosyn {nosyn} :varattnosyn {attnosyn} :location {loc}}}"
    )
}

fn relabel(arg: &str) -> String {
    format!("{{RELABELTYPE :arg {arg} :resulttype 23 :resulttypmod -1 :resultcollid 0 :relabelformat 2 :location -1}}")
}

/// `max(arg)` over an int4 (pg_proc 2116), as the captures spell an Aggref.
fn max_of(arg: &str) -> String {
    format!(
        "{{AGGREF :aggfnoid 2116 :aggtype 23 :aggcollid 0 :inputcollid 0 :aggtranstype 23 :aggargtypes (o 23) :aggdirectargs <> \
         :args ({{TARGETENTRY :expr {arg} :resno 1 :resname <> :ressortgroupref 0 :resorigtbl 0 :resorigcol 0 :resjunk false}}) \
         :aggorder <> :aggdistinct <> :aggfilter <> :aggstar false :aggvariadic false :aggkind n :aggpresorted false \
         :agglevelsup 0 :aggsplit 0 :aggno 0 :aggtransno 0 :location -1}}"
    )
}

/// A no-argument float8 function call (`funcid`) compared `> -1` (float8gt), a scan qual's term.
fn over_minus_one(funcid: u32) -> String {
    let call = format!("{{FUNCEXPR :funcid {funcid} :funcresulttype 701 :funcretset false :funcvariadic false :funcformat 0 :funccollid 0 :inputcollid 0 :args <> :location -1}}");
    let minus_one = "{CONST :consttype 701 :consttypmod -1 :constcollid 0 :constlen 8 :constbyval true :constisnull false :location -1 :constvalue 8 [ 0 0 0 0 0 0 240 191 ]}";
    format!("{{OPEXPR :opno 674 :opfuncid 297 :opresulttype 16 :opretset false :opcollid 0 :inputcollid 0 :args ({call} {minus_one}) :location -1}}")
}

/// A call of the user function `USER_FN` (no argument, boolean), a scan qual's term.
fn user_call() -> String {
    format!("{{FUNCEXPR :funcid {USER_FN} :funcresulttype 16 :funcretset false :funcvariadic false :funcformat 0 :funccollid 0 :inputcollid 0 :args <> :location -1}}")
}

/// `text` with its one occurrence of `old` as `new`.
fn edit(text: &str, old: &str, new: &str) -> String {
    assert_eq!(text.matches(old).count(), 1, "not one match: {old}");
    text.replacen(old, new, 1)
}

/// `text` with its first occurrence of `old` (the first Append member's copy) as `new`.
fn edit_first(text: &str, old: &str, new: &str) -> String {
    assert!(text.contains(old), "no match: {old}");
    text.replacen(old, new, 1)
}

/// TPC-H q20's semi (node 7) target entry 4, `part.p_partkey` (inner.1), and the join above's (node 2) entry 1.
fn th20_semi_entry() -> String {
    format!("{} :resno 4", var(-1, 1, (8, 1), 149))
}
fn th20_upper_entry() -> String {
    format!("{} :resno 1", var(-1, 2, (4, 2), 306))
}

#[test]
fn the_captures_null_the_unread_semi_columns() {
    let th20 = graph(&read(TH20));
    assert!(th20.contains(TH20_NULL) && !th20.contains(TH20_KEPT), "{th20}");
    // three members: each semi's projection and each HashAggregate's bare passengers over it
    let pj = graph(&read(PJ));
    assert_eq!(pj.matches(PJ_NULL).count(), 6, "{pj}");
}

/// A fixture's own facts name no relation's persistence (`rel_ext` unknown: F5 reads each scan as parallel-unsafe), so
/// the rule takes each statement as one that may build lazily and nulls nothing: the decompose goldens stay trunk's.
#[test]
fn under_the_fixture_facts_every_column_stays() {
    let th20 = graph_as(&read(TH20), None);
    assert!(th20.contains(TH20_KEPT) && !th20.contains(TH20_NULL), "{th20}");
    let pj = graph_as(&read(PJ), None);
    // D3375: the HashAggregates' unread passengers ride as NULLs whatever the lazy reading (`a_lazy_build_anywhere..`)
    assert!(pj.matches(PJ_KEPT).count() == 3 && pj.matches(PJ_NULL).count() == 3, "{pj}");
}

#[test]
fn a_read_above_keeps_the_inner_column() {
    let t = read(TH20);
    let reads_it = var(-1, 4, (8, 1), 306); // node 2 reading the Hash's column 4 (the semi's p_partkey)
    let cases = [
        // D3375: an expression over it (a bare entry handed on to no reader is dead too: `a_bare_hand_on_no_one_reads_is_dead`)
        ("the upper join's target list", edit(&t, &th20_upper_entry(), &format!("{} :resno 1", relabel(&reads_it)))),
        ("the upper join's join filter", edit(&t, &var(-1, 3, (4, 3), 202), &var(-1, 4, (8, 1), 202))),
        // a node kind expr::kids does not list hides the read: the unread proof fails closed (FIX-2; the F5 walk does too)
        ("a kind the walker does not know", edit(&t, &th20_upper_entry(), &format!("{{RETURNINGEXPR :retlevelsup 0 :retold true :retexpr {reads_it}}} :resno 1"))),
    ];
    for (why, text) in cases {
        let g = graph(&text);
        assert!(g.contains(TH20_KEPT) && !g.contains(TH20_NULL), "{why}: {g}");
    }
}

/// D3375 (demand/readers.rs `dead_above`): the upper join hands the semi's column on bare in its target list, and no node above
/// reads it (the join's parent probes by another column): the column is dead there too, so the semi nulls it.
#[test]
fn a_bare_hand_on_no_one_reads_is_dead() {
    let t = read(TH20);
    let bare = format!("{} :resno 1", var(-1, 4, (8, 1), 306));
    let g = graph(&edit(&t, &th20_upper_entry(), &bare));
    assert!(g.contains(TH20_NULL) && !g.contains(TH20_KEPT), "{g}");
}

#[test]
fn the_semi_itself_decides_too() {
    let t = read(TH20);
    let tail = " :resname <> :ressortgroupref 0 :resorigtbl 0 :resorigcol 0 :resjunk false}) :join.plan.qual ";
    let qual = format!(
        "({{OPEXPR :opno 96 :opfuncid 65 :opresulttype 16 :opretset false :opcollid 0 :inputcollid 0 :args ({} {}) :location -1}})",
        var(-2, 1, (4, 1), 279),
        var(-1, 1, (8, 1), 149)
    );
    let cases = [
        ("its own qual reads it", edit(&t, &format!("{}{tail}<>", th20_semi_entry()), &format!("{}{tail}{qual}", th20_semi_entry()))),
        // setrefs keeps the syntactic attno: a system column's (ctid -1) or a whole row's (0) rowmark junk (FIX-1)
        ("a system column", edit(&t, &th20_semi_entry(), &format!("{} :resno 4", var(-1, 1, (8, -1), 149)))),
        ("a whole row", edit(&t, &th20_semi_entry(), &format!("{} :resno 4", var(-1, 1, (8, 0), 149)))),
    ];
    for (why, text) in cases {
        let g = graph(&text);
        assert!(g.contains(TH20_KEPT) && !g.contains(TH20_NULL), "{why}: {g}");
    }
    let g = graph(&edit(&t, &th20_semi_entry(), &format!("{} :resno 4", relabel(&var(-1, 1, (8, 1), 149)))));
    assert!(!g.contains("4=c23:null"), "an expression entry: {g}");
}

#[test]
fn a_parent_outside_the_rule_keeps_the_inner_columns() {
    let t = read(PJ);
    let (at, end) = {
        let at = t.find(PJ_AGG).unwrap();
        (at, at + t[at..].find(" :chain <>").unwrap() + " :chain <>".len())
    };
    let agg_as = |tag: &str, fields: &str| format!("{}{fields}{}", t[..at].replacen("{AGG ", tag, 1), &t[end..]);
    let cases = [
        ("the HashAggregate groups by one", edit_first(&t, PJ_AGG, ":aggstrategy 2 :aggsplit 0 :numCols 2 :grpColIdx ( 1 2) :grpOperators ( 96 96) :grpCollations ( 0 0)")),
        ("the HashAggregate emits one in an expression", edit(&t, &format!("{{TARGETENTRY :expr {} :resno 2", var(-2, 2, (12, 1), -1)), &format!("{{TARGETENTRY :expr {} :resno 2", relabel(&var(-2, 2, (12, 1), -1))))),
        // an Aggref's argument is a read (expr::walk enters args, aggorder, aggdistinct and aggfilter)
        ("the HashAggregate aggregates one", edit(&t, &format!("{{TARGETENTRY :expr {} :resno 2", var(-2, 2, (12, 1), -1)), &format!("{{TARGETENTRY :expr {} :resno 2", max_of(&var(-2, 2, (12, 1), -1))))),
        ("the HashAggregate's qual reads one", edit_first(&t, ":plan.qual <> :plan.lefttree {HASHJOIN", &format!(
            ":plan.qual ({{OPEXPR :opno 521 :opfuncid 147 :opresulttype 16 :opretset false :opcollid 0 :inputcollid 0 :args ({} {{CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true :constisnull false :location -1 :constvalue 4 [ 0 0 0 0 0 0 0 0 ]}}) :location -1}}) :plan.lefttree {{HASHJOIN",
            var(-2, 3, (12, 2), -1)
        ))),
    ];
    for (why, text) in cases {
        let g = graph(&text);
        // the first member keeps both columns; the other two members still take the rule
        assert!(g.contains(PJ_KEPT), "{why}: {g}");
        assert_eq!(g.matches(PJ_NULL).count(), 4, "{why}: {g}");
    }
    // D3375 (demand/readers.rs `readers_at`): a Sort by the key alone or a Material hands the columns on bare to the Nested Loop,
    // which reads only the key: the first member's semi nulls them too
    let cases = [
        ("a Sort parent", agg_as("{SORT ", ":numCols 1 :sortColIdx ( 1) :sortOperators ( 97) :collations ( 0) :nullsFirst ( false)")),
        ("a Material parent", agg_as("{MATERIAL ", "")),
    ];
    for (why, text) in cases {
        let g = graph(&text);
        assert!(!g.contains(PJ_KEPT), "{why}: {g}");
        assert_eq!(g.matches(PJ_NULL).count(), 5, "{why}: {g}");
    }
}

/// D2147 / D2168 (pre-existing on trunk): an aggregate over a hash build the lowerer makes lazy (a side, or a pipeline
/// feeding it, calling a VOLATILE or user function) crashes the server, so a statement with such a call anywhere keeps
/// every inner column: on a SEMI's build side (form (c): partition_join's first member; form (a): q20's part scan), on
/// its outer side (q20's partsupp scan: the build of the join above, case (a)), or on a scan elsewhere. The user
/// function is known IMMUTABLE or STABLE, as the server's facts read a plpgsql function's provolatile; an IMMUTABLE
/// parallel-safe builtin (pi()) calls no user code and keeps the rule.
#[test]
fn a_lazy_build_anywhere_keeps_every_inner_column() {
    let pj = read(PJ);
    let tail = ") :scan.plan.lefttree <> :scan.plan.righttree <> :scan.plan.initPlan <> :scan.plan.extParam (b) :scan.plan.allParam (b) :scan.scanrelid 12 :cb_scan_cols (b 1 2 3)}";
    let pj_and = |term: &str| edit(&pj, &format!(":location -1}}{tail}"), &format!(":location -1}} {term}{tail}"));
    let th20 = read(TH20);
    let part_and = |term: &str| edit(&th20, ":location 182})", &format!(":location 182}} {term})"));
    let empty = |rti: u32| format!(":scan.plan.qual <> :scan.plan.lefttree <> :scan.plan.righttree <> :scan.plan.initPlan <> :scan.plan.extParam (b) :scan.plan.allParam (b) :scan.scanrelid {rti} ");
    let scan_where = |rti: u32, term: &str| edit(&th20, &empty(rti), &empty(rti).replacen(":scan.plan.qual <>", &format!(":scan.plan.qual ({term})"), 1));
    let (imm, stb) = (Some(Volatility::Immutable), Some(Volatility::Stable));
    let pj_cases = [
        ("random() on a semi's build side", pj_and(&over_minus_one(1598)), imm),
        ("an IMMUTABLE user function on a semi's build side", pj_and(&user_call()), imm),
        ("a STABLE user function on a semi's build side", pj_and(&user_call()), stb),
    ];
    // D3375: each semi keeps its inner columns; the HashAggregate over it emits them to a Nested Loop that reads only its
    // key, so its passengers ride as NULLs (demand/readers.rs `rewrite`: an aggregate over a lazy build no longer crashes, D2147)
    for (why, text, user) in pj_cases {
        let g = graph_as(&text, user);
        assert!(g.matches(PJ_KEPT).count() == 3 && g.matches(PJ_NULL).count() == 3, "{why}: {g}");
    }
    let th20_cases = [
        ("an IMMUTABLE user function on the semi's build side", part_and(&user_call()), imm),
        ("a STABLE user function on the semi's build side", part_and(&user_call()), stb),
        ("random() on the semi's outer side (the join above's build)", scan_where(4, &over_minus_one(1598)), imm),
        ("an IMMUTABLE user function on the semi's outer side", scan_where(4, &user_call()), imm),
        ("a STABLE user function on another scan", scan_where(1, &user_call()), stb),
    ];
    for (why, text, user) in th20_cases {
        let g = graph_as(&text, user);
        assert!(g.contains(TH20_KEPT) && !g.contains(TH20_NULL), "{why}: {g}");
    }
    // does not apply: an IMMUTABLE parallel-safe builtin calls no user code
    let g = graph(&pj_and(&over_minus_one(1610)));
    assert_eq!(g.matches(PJ_NULL).count(), 6, "pi() on a semi's build side: {g}");
    let g = graph(&part_and(&over_minus_one(1610)));
    assert!(g.contains(TH20_NULL) && !g.contains(TH20_KEPT), "pi() on the semi's build side: {g}");
}

/// regress-090's plan tree (`SELECT p FROM ba WHERE p = 3 OR q = 5`: a Bitmap Heap Scan over a BitmapOr of two btree
/// leaves), and in it the BitmapOr, its two leaves and the head of its node before its children.
struct Bitmap {
    tree: String,
    or: String,
    leaf1: String,
    leaf2: String,
}

/// The balanced node text starting at `at`.
fn node_at(t: &str, at: usize) -> &str {
    let mut depth = 0;
    for (i, c) in t[at..].char_indices() {
        depth += match c {
            '{' => 1,
            '}' => -1,
            _ => 0,
        };
        if depth == 0 {
            return &t[at..=at + i];
        }
    }
    panic!("unbalanced node at {at}")
}

fn bitmap() -> Bitmap {
    let t = read(BO);
    let at = t.find(":planTree ").unwrap() + ":planTree ".len();
    let tree = node_at(&t, at).to_string();
    let or = node_at(&tree, tree.find("{BITMAPOR ").unwrap()).to_string();
    let l1 = or.find("{BITMAPINDEXSCAN ").unwrap();
    let leaf1 = node_at(&or, l1).to_string();
    let leaf2 = node_at(&or, l1 + leaf1.len() + or[l1 + leaf1.len()..].find("{BITMAPINDEXSCAN ").unwrap()).to_string();
    Bitmap { tree, or, leaf1, leaf2 }
}

impl Bitmap {
    /// A BitmapOr (`and`: a BitmapAnd) over `a` and `b`.
    fn of(&self, and: bool, a: &str, b: &str) -> String {
        let head = &self.or[..self.or.find(":bitmapplans (").unwrap() + ":bitmapplans (".len()];
        let head = if and { head.replacen("{BITMAPOR ", "{BITMAPAND ", 1).replacen(" :isshared false", "", 1) } else { head.to_string() };
        format!("{head}{a} {b})}}")
    }
    /// TH20 with the plan tree, its BitmapOr as `bitmap`, as one more subplans entry.
    fn th20(&self, bitmap: &str) -> String {
        th20_with(&[&self.tree.replacen(&self.or, bitmap, 1)])
    }
}

/// TH20 with `trees` as more subplans entries (`lazy_build` reads every plan tree of the statement; decompose maps only the
/// ones the statement reads).
fn th20_with(trees: &[&str]) -> String {
    edit(&read(TH20), ":subplans <>", &format!(":subplans ({})", trees.join(" ")))
}

/// union-043's plan tree (`select intersect select`: a SetOp over two Results).
fn setop_tree() -> String {
    let t = read("plans/regress-full/union-043.plan");
    let at = t.find(":planTree ").unwrap() + ":planTree ".len();
    let tree = node_at(&t, at).to_string();
    assert!(tree.starts_with("{SETOP "), "{tree}");
    tree
}

const BO: &str = "plans/regress-090.plan";
/// The key of leaf 1 (`p = 3`) and of leaf 2 (`q = 5`), each first in its leaf's indexqual.
const KEY1: &str = "{CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true :constisnull false :location 27 :constvalue 4 [ 3 0 0 0 0 0 0 0 ]}";
const KEY2: &str = "{CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true :constisnull false :location 36 :constvalue 4 [ 5 0 0 0 0 0 0 0 ]}";
const EXTERN: &str = "{PARAM :paramkind 0 :paramid 1 :paramtype 23 :paramtypmod -1 :paramcollid 0 :location -1}";
const EXEC: &str = "{PARAM :paramkind 1 :paramid 0 :paramtype 23 :paramtypmod -1 :paramcollid 0 :location -1}";
const NULL_KEY: &str = "{CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true :constisnull true :location -1 :constvalue <>}";

/// The former no-widen forms, each served now that its root fix landed: a BitmapOr whose subtree has a key other than a
/// non-NULL Const (D2180's form); a set operation (D2197's); a merge join (D2194's); a CTE, a CteScan or a CTE InitPlan
/// no scan reads (D2178 / D2179's, D3201). A statement the rule does not serve (the fixture's facts) keeps today's answers.
#[test]
fn a_former_no_widen_form_serves() {
    let bm = bitmap();
    let (l1, l2) = (bm.leaf1.as_str(), bm.leaf2.as_str());
    let key1 = |k: &str| edit_first(l1, KEY1, k);
    let null_test = edit_first(l1, &l1[l1.find(":indexqual (").unwrap()..l1.find(" :indexqualorig").unwrap()], &format!(":indexqual ({{NULLTEST :arg {} :nulltesttype 0 :argisrow false :location -1}})", node_at(l1, l1.find("{VAR :varno -3 ").unwrap())));
    let served = [
        ("Const keys", bm.or.clone()),
        ("a parameter key under a BitmapAnd alone", bm.of(true, &key1(EXTERN), l2)),
        ("a null test leaf", bm.of(false, &null_test, l2)),
        ("an EXTERN parameter key (a generic plan's $1)", bm.of(false, &key1(EXTERN), l2)),
        ("an executor parameter key", bm.of(false, l1, &edit_first(l2, KEY2, EXEC))),
        ("a NULL Const key", bm.of(false, &key1(NULL_KEY), l2)),
        ("the key under a BitmapAnd under the BitmapOr", bm.of(false, &bm.of(true, &key1(EXTERN), l2), l2)),
        ("the BitmapOr under a BitmapAnd", bm.of(true, &bm.of(false, &key1(EXTERN), l2), l2)),
    ];
    for (why, bitmap) in served {
        let g = graph(&bm.th20(&bitmap));
        assert!(g.contains(TH20_NULL) && !g.contains(TH20_KEPT), "{why}: {g}");
    }
    let g = graph_as(&bm.th20(&bm.of(false, &key1(EXTERN), l2)), None);
    assert!(g.contains(TH20_KEPT) && !g.contains(TH20_NULL), "the rule does not apply: {g}");
    let setop = setop_tree();
    let merge = node_of("plans/regress-full/aggregates-034.plan", "{MERGEJOIN ");
    let bitmap_run = bm.tree.replacen(&bm.or, &bm.of(false, &key1(EXTERN), l2), 1);
    let serves = |why: &str, trees: &[&str]| {
        let g = graph(&th20_with(trees));
        assert!(g.contains(TH20_NULL) && !g.contains(TH20_KEPT), "{why}: {g}");
    };
    serves("a SetOp", &[&setop]);
    serves("a SetOp beside a BitmapOr over a run-time key", &[&setop, &bitmap_run]);
    let g = graph_as(&th20_with(&[&setop]), None);
    assert!(g.contains(TH20_KEPT) && !g.contains(TH20_NULL), "a SetOp where the rule does not apply: {g}");
    let cte_scan = node_of("plans/regress-full/conversion-005.plan", "{CTESCAN ");
    let cte_init = node_of("plans/regress-full/rowtypes-037.plan", ":planTree {RESULT ");
    assert!(cte_init.contains(":subLinkType 7") && !cte_init.contains("{CTESCAN "), "a Result whose InitPlan is a CTE no scan reads");
    serves("a CteScan", &[&cte_scan]);
    serves("a CTE InitPlan alone (an unread or data-modifying CTE)", &[&cte_init]);
    serves("a CteScan beside a merge join and a SetOp", &[&merge, &setop, &cte_scan]);
    serves("a merge join", &[&merge]);
    serves("a merge join beside a SetOp", &[&merge, &setop]);
    let g = graph_as(&th20_with(&[&merge, &cte_scan, &cte_init]), None);
    assert!(g.contains(TH20_KEPT) && !g.contains(TH20_NULL), "a CTE or a merge join where the rule does not apply: {g}");
}

/// The first node of fixture `path` whose text opens with `open` (from its `{`), balanced.
fn node_of(path: &str, open: &str) -> String {
    let t = read(path);
    let at = t.find(open).unwrap_or_else(|| panic!("{path}: no {open}")) + open.find('{').unwrap();
    node_at(&t, at).to_string()
}

/// The reader search looks each parent up in a map of the plan built once per statement (`parents`). SEMI_UNREAD_SCALE
/// names a partitionwise capture (e.g. a partition_join-029 of 256+ members); the probe prints decompose's time over it
/// (the capture loaded once).
#[test]
#[ignore = "timing probe: SEMI_UNREAD_SCALE=<plan> cargo test --test semi_unread -- --ignored --nocapture"]
fn decompose_time_of_a_wide_partitionwise_plan() {
    let Some(path) = std::env::var_os("SEMI_UNREAD_SCALE") else { return };
    let text = fs::read_to_string(path).unwrap();
    let ctx = MemoryContext::new("semi_unread_scale");
    let fx = corpus::load(ctx.mcx(), &text).unwrap();
    let facts = Server { fx: &fx.facts, user: Volatility::Immutable };
    let (mut ms, mut what) = (Vec::new(), String::new());
    for _ in 0..5 {
        let arena = Arena::new(1 << 28);
        let t0 = std::time::Instant::now();
        let g = decompose_in(arena.handle(), fx.stmt, &facts, fx.mcx);
        ms.push(t0.elapsed().as_secs_f64() * 1e3);
        what = match g {
            Ok(g) => {
                let mut out = String::new();
                dump::dump(&g, &mut out).unwrap();
                format!("{} nulled projections", out.matches(PJ_NULL).count())
            }
            Err(r) => format!("refused: {}", r.reason),
        };
    }
    ms.sort_by(f64::total_cmp);
    println!("semi_unread scale: {what}; decompose ms min {:.2} median {:.2}", ms[0], ms[2]);
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
