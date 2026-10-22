// M4-INSERT (cards/M4.md L0-m): an INSERT's VALUES of bare Consts is write.literals' literal table (key.rs
// `literal_table`). The program reads the cells from the table's words on every execution, so the content key holds
// each cell's class, never its by-reference image. Two statements that differ only in literal text share a key (one
// program for every literal set). A cell's type, typmod, collation or NULL-ness, a cell that is an expression (NEW-29:
// the program's code, its own Consts hashed by image; the list's Const cells stay table cells), the column count and
// the row count each give another key. The net under it is V4R-110's: a lowering that bakes a cell's image is caught by `lowering_reads_lists`, which
// lowerv4 lib.rs ORs into `Descs::no_cache`, so a program that did depend on the bytes is never filed.
use std::path::Path;

use gravity_decompose::{corpus, decompose_in, PipelineGraph, Source};
use gravity_lower::inputs::key::{class_keyed_lists, key, key_and_nodes, literal_table, lowering_reads_lists, values_node};
use gravity_program::Arena;
use mcx::MemoryContext;
use types_nodes::Node;

/// `INSERT INTO stats_ext_tbl (col) VALUES ('secret'), ('secret'), ('very secret')`: one text column, three rows.
const FIXTURE: &str = "../decompose/tests/plans/regress-full/stats_ext-007.plan";

/// A text cell's class as the plan prints it (the fields before `:location`).
const TEXT: &str = ":consttype 25 :consttypmod -1 :constcollid 100 :constlen -1 :constbyval false :constisnull false";

/// The int4 cell the two-column form puts before each text cell.
const INT: &str = "{CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true :constisnull false :location 40 :constvalue 4 [ 1 0 0 0 0 0 0 0 ]}";

fn fixture() -> String {
    std::fs::read_to_string(manifest_dir().join(FIXTURE)).unwrap()
}

/// A non-null text cell of class `class` at plan location `loc` holding `s` (a 4-byte varlena header, as the plan
/// prints it).
fn cell(class: &str, loc: u32, s: &str) -> String {
    let len = s.len() + 4;
    let bytes: Vec<String> = ((len as u32) << 2).to_le_bytes().iter().map(u8::to_string).chain(s.bytes().map(|b| b.to_string())).collect();
    format!("{{CONST {class} :location {loc} :constvalue {len} [ {} ]}}", bytes.join(" "))
}

/// The fixture's three cells, in order.
fn cells() -> [String; 3] {
    [cell(TEXT, 40, "secret"), cell(TEXT, 52, "secret"), cell(TEXT, 64, "very secret")]
}

/// The fixture with cell `i` replaced by `with` (the whole node's text).
fn swap(text: &str, i: usize, with: &str) -> String {
    let c = &cells()[i];
    assert!(text.contains(c.as_str()), "{FIXTURE}: no longer holds cell {i} {c}");
    text.replacen(c.as_str(), with, 1)
}

/// The fixture with a second column: `INSERT INTO stats_ext_tbl (id, col) VALUES (1, ..), (1, ..), (1, ..)`.
fn two_columns(text: &str) -> String {
    let edits = [
        (":varnosyn 2 :varattnosyn 1 :location -1} :resno 2", ":varnosyn 2 :varattnosyn 2 :location -1} :resno 2"),
        ("{VAR :varno 2 :varattno 1 :vartype 25", "{VAR :varno 2 :varattno 2 :vartype 25"),
        ("{NEXTVALUEEXPR :seqid 34392 :typeId 23}", "{VAR :varno 2 :varattno 1 :vartype 23 :vartypmod -1 :varcollid 0 :varnullingrels (b) :varlevelsup 0 :varreturningtype 0 :varnosyn 2 :varattnosyn 1 :location -1}"),
        (":aliasname *VALUES* :colnames (\"column1\")", ":aliasname *VALUES* :colnames (\"column1\" \"column2\")"),
        (":insertedCols (b 9)", ":insertedCols (b 8 9)"),
    ];
    let mut t = text.to_owned();
    for (from, to) in edits {
        assert!(t.contains(from), "{FIXTURE}: no longer holds {from}");
        t = t.replacen(from, to, 1);
    }
    for c in cells() {
        t = t.replacen(&format!("({c})"), &format!("({INT} {c})"), 1);
    }
    t
}

/// The graph of `text` as the executor decomposes it (admit.rs: `decompose_in`), handed to `f`.
fn with_graph<T>(text: &str, f: impl FnOnce(&PipelineGraph<'_>) -> T) -> T {
    let ctx = MemoryContext::new("literal table key");
    let fx = corpus::load(ctx.mcx(), text).unwrap_or_else(|e| panic!("fixture: {e}"));
    let arena = Arena::new(1 << 20);
    let g = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).unwrap_or_else(|r| panic!("decompose: {r}"));
    f(&g)
}

/// The content key of `text`, and whether its VALUES pipeline is a literal table.
fn key_of(text: &str) -> (u128, bool) {
    with_graph(text, |g| (key(g).0, g.pipelines.iter().any(|p| literal_table(p))))
}

/// The VALUES pipeline's cells, in order.
fn value_cells<'p>(g: &PipelineGraph<'p>) -> Vec<Node<'p>> {
    let lists = g.pipelines.iter().find_map(|p| if let Source::Values { lists, .. } = &p.source { Some(*lists) } else { None }).expect("a VALUES pipeline");
    lists.iter().flat_map(|l| l.as_list().expect("a VALUES row").iter()).collect()
}

#[test]
fn a_literal_tables_key_holds_its_cells_classes_not_their_text() {
    let text = fixture();
    let (base, table) = key_of(&text);
    assert!(table, "{FIXTURE}: the VALUES is no literal table");
    // the same shape: one program for every literal set (same length, shorter, longer)
    for (i, s) in [(0, "secreu"), (2, "x"), (2, "a literal longer than the fixture's by far, to move the header")] {
        assert_eq!(key_of(&swap(&text, i, &cell(TEXT, [40, 52, 64][i], s))), (base, true), "cell {i} as {s:?}: another key");
    }
    // what the program does depend on: the class of each cell, a cell that is an expression, the column and row counts
    let varchar = ":consttype 1043 :consttypmod -1 :constcollid 100 :constlen -1 :constbyval false :constisnull false";
    let typmod = ":consttype 25 :consttypmod 20 :constcollid 100 :constlen -1 :constbyval false :constisnull false";
    let c_coll = ":consttype 25 :consttypmod -1 :constcollid 950 :constlen -1 :constbyval false :constisnull false";
    let null = "{CONST :consttype 25 :consttypmod -1 :constcollid 100 :constlen -1 :constbyval false :constisnull true :location 64 :constvalue <>}";
    let relabel = |c: &str| format!("{{RELABELTYPE :arg {c} :resulttype 25 :resulttypmod -1 :resultcollid 100 :relabelformat 2 :location -1}}");
    let third = &cells()[2];
    let differ = [
        ("type (varchar)", swap(&text, 2, &cell(varchar, 64, "very secret"))),
        ("typmod", swap(&text, 2, &cell(typmod, 64, "very secret"))),
        ("collation (C)", swap(&text, 2, &cell(c_coll, 64, "very secret"))),
        ("NULL", swap(&text, 2, null)),
        ("an expression cell", swap(&text, 2, &relabel(third))),
        ("two rows", text.replacen(&format!(" ({third})"), "", 1)),
        ("two columns", two_columns(&text)),
    ];
    let mut seen = vec![("the fixture", base)];
    for (what, t) in &differ {
        let (k, _) = key_of(t);
        if let Some((other, _)) = seen.iter().find(|s| s.1 == k) {
            panic!("{what}: the same key as {other}");
        }
        seen.push((*what, k));
    }
    // NEW-29: an expression cell is the program's code (its own Const keyed by image); the list's Const cells stay table
    // cells, keyed by class
    let expr = swap(&text, 2, &relabel(third));
    let (k, table) = key_of(&expr);
    assert!(table, "a VALUES with an expression cell is no literal table");
    assert_eq!(key_of(&swap(&expr, 0, &cell(TEXT, 40, "secreu"))).0, k, "a table cell's text is in the key");
    assert_ne!(key_of(&swap(&expr, 2, &cell(TEXT, 64, "very secreu"))).0, k, "the expression's own Const left the key");
    // two columns: one key for every literal set of that shape too
    let two = two_columns(&text);
    let (k2, table) = key_of(&two);
    assert!(table, "the two-column form is no literal table");
    assert_eq!(key_of(&two.replacen(third.as_str(), &cell(TEXT, 64, "x"), 1)), (k2, true), "two columns: another key for another text");
}

#[test]
fn a_lowering_that_bakes_a_cell_files_no_program() {
    with_graph(&fixture(), |g| {
        let cells = value_cells(g);
        assert_eq!(cells.len(), 3);
        // the three cells' images are logged with the class-keyed lists
        let listed = class_keyed_lists(g);
        for c in &cells {
            let at = c.as_const().unwrap().constvalue.as_usize();
            assert!(listed.contains(&at), "a cell's image is not logged as keyed by its class");
        }
        // a lowering that reads a cell (the plan-tree bake, gravity_decompose; the IR's, gravity_expr) is its literals':
        // lowerv4 lib.rs sets `Descs::no_cache` from this flag, so it is run once and never filed
        let third = cells[2].as_const().unwrap();
        assert!(lowering_reads_lists(g, || {
            let _ = gravity_decompose::bake::read(third);
        })
        .1, "a plan-tree bake of a cell files a program");
        assert!(lowering_reads_lists(g, || gravity_expr::bake::note(third.constvalue.as_usize())).1, "an IR bake of a cell files a program");
        // one that reads none is filed: the one program every literal set of the shape shares
        assert!(!lowering_reads_lists(g, || ()).1, "a lowering that bakes nothing is marked no_cache");
    });
}

/// D4448 (D3882's double walk): the key walk visits each VALUES row and each of its cells once. Since D3882 opened a
/// list in `expr::kids`, key.rs walked a row's members in its list arm and again through `kids`, so every cell of a
/// literal table was hashed, logged and placed twice (a 25k-row INSERT keyed 150k cells, then sorted them).
#[test]
fn the_key_walk_visits_each_values_cell_once() {
    for text in [fixture(), two_columns(&fixture())] {
        with_graph(&text, |g| {
            let cells = value_cells(g);
            let mut walked: Vec<Node<'_>> = Vec::new();
            let _ = key_and_nodes(g, &mut |n| walked.push(n));
            for (i, c) in cells.iter().enumerate() {
                assert_eq!(walked.iter().filter(|n| n.ptr_eq(*c)).count(), 1, "cell {i}: not walked once");
            }
            let lists = g.pipelines.iter().find_map(|p| if let Source::Values { lists, .. } = &p.source { Some(*lists) } else { None }).unwrap();
            for (r, row) in lists.iter().enumerate() {
                assert_eq!(walked.iter().filter(|n| n.ptr_eq(row)).count(), 1, "row {r}: not walked once");
            }
            let images: Vec<usize> = cells.iter().filter_map(|c| c.as_const().filter(|k| !k.constbyval && !k.constisnull)).map(|k| k.constvalue.as_usize()).collect();
            let listed = class_keyed_lists(g);
            assert_eq!(listed.iter().filter(|a| images.contains(a)).count(), images.len(), "each by-reference cell logged once");
        });
    }
}

/// D2918: write.literals names its scan by the plan node id (`values_node`, `Proto::arg`), and other protos and exec
/// lookups name other plan nodes, so one graph content whose plan numbers its nodes apart (groupingsets.sql's chained
/// aggregates over gstest1: the sorted plan's VALUES scan is node 3, the hashed plan's node 2) keys apart, never one program
/// naming the other plan's nodes: the VALUES scan renumbered, and the ModifyTable above it renumbered.
#[test]
fn a_literal_tables_key_holds_its_scans_plan_node_id() {
    let text = fixture();
    let (from, to) = (":scan.plan.plan_node_id 1 :scan.plan.targetlist", ":scan.plan.plan_node_id 2 :scan.plan.targetlist");
    assert_eq!(text.matches(from).count(), 1, "{FIXTURE}: no longer holds its VALUES scan as node 1");
    let renumbered = text.replacen(from, to, 1);
    let ids = |t: &str| with_graph(t, |g| g.pipelines.iter().filter(|p| literal_table(p)).map(|p| values_node(p)).collect::<Vec<i32>>());
    assert_eq!((ids(&text), ids(&renumbered)), (vec![1], vec![2]), "values_node is not the scan's plan node id");
    let ((k, t), (k2, t2)) = (key_of(&text), key_of(&renumbered));
    assert!(t && t2, "the VALUES is no literal table");
    assert_ne!(k, k2, "two numberings of one VALUES scan share a key (D2918)");
    let top = (":plan.plan_node_id 0 :plan.targetlist", ":plan.plan_node_id 5 :plan.targetlist");
    assert_eq!(text.matches(top.0).count(), 1, "{FIXTURE}: no longer holds its ModifyTable as node 0");
    let (k3, _) = key_of(&text.replacen(top.0, top.1, 1));
    assert!(k3 != k && k3 != k2, "a renumbered ModifyTable shares a key (D2918: every plan node id a pipeline runs is keyed)");
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
