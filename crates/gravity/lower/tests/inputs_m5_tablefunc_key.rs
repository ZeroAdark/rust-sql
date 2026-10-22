// M5 L0-f (1) (M5-plan §1.3, §9 Wave 1a row 1a-3): the content key hashes an XMLTABLE / JSON_TABLE source by its whole
// face (key.rs `table_func`). It used to hash `Source::TableFunc` by (scanrelid, cols, ordinality) only, so two table
// functions of one rti and width shared a cached program: the second statement would run the first one's row path
// (WRONG once srf.tablefunc serves). A path is a by-reference literal, and the Const rule keys its image; the document
// and the PASSING values are keyed by class (D715).
use std::path::Path;

use gravity_decompose::{corpus, decompose_in, Source};
use gravity_lower::inputs::key::key;
use gravity_program::{Arena, ContentKey};
use mcx::MemoryContext;

/// `SELECT * FROM xmltable('/r/e' PASSING '<r><e>1</e></r>' COLUMNS v int PATH '.')`.
const XML: &str = "../decompose/tests/plans/regress-061.plan";
/// `SELECT * FROM json_table('[{"a": 1}, {"a": 2}]', '$[*]' COLUMNS (a int PATH '$.a')) jt`.
const JSON: &str = "../decompose/tests/plans/regress-060.plan";

/// The row path '/r/e' as the plan prints it (a 4-byte varlena header, then the bytes).
const ROW_PATH: &str = ":constvalue 8 [ 32 0 0 0 47 114 47 101 ]";
/// The column path '.'.
const COL_PATH: &str = ":constvalue 5 [ 20 0 0 0 46 ]";

fn fixture(rel: &str) -> String {
    std::fs::read_to_string(manifest_dir().join(rel)).unwrap()
}

fn edit(text: &str, from: &str, to: &str) -> String {
    assert_eq!(text.matches(from).count(), 1, "the fixture holds {from} once");
    text.replacen(from, to, 1)
}

/// `text`'s content key, as the executor decomposes it; it must have a TableFunc source.
fn key_of(text: &str) -> ContentKey {
    let ctx = MemoryContext::new("tablefunc key");
    let fx = corpus::load(ctx.mcx(), text).unwrap_or_else(|e| panic!("fixture: {e}"));
    let arena = Arena::new(1 << 20);
    let g = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).unwrap_or_else(|r| panic!("decompose: {r}"));
    assert!(g.pipelines.iter().any(|p| matches!(p.source, Source::TableFunc { .. })), "a TableFunc source");
    key(&g)
}

#[test]
fn two_xmltables_of_one_rti_and_width_key_apart() {
    let base = fixture(XML);
    let k = key_of(&base);
    // '/r/e' -> '/r/x': the row path
    assert_ne!(key_of(&edit(&base, ROW_PATH, ":constvalue 8 [ 32 0 0 0 47 114 47 120 ]")), k, "another row path");
    // '.' -> '*': a column's path
    assert_ne!(key_of(&edit(&base, COL_PATH, ":constvalue 5 [ 20 0 0 0 42 ]")), k, "another column path");
}

#[test]
fn a_json_table_column_path_keys_apart() {
    let base = fixture(JSON);
    // the column path '$.a' -> '$.b' (the jsonpath image's member name)
    let b = edit(&base, "1 0 0 0 97 0 ]", "1 0 0 0 98 0 ]");
    assert_ne!(key_of(&b), key_of(&base), "another column path");
}

/// Does not apply (G4): the same statement keys the same, a location change included.
#[test]
fn the_same_xmltable_keys_the_same() {
    let base = fixture(XML);
    assert_eq!(key_of(&base), key_of(&base));
    assert_eq!(key_of(&edit(&base, ":location 75 :constvalue 5", ":location 76 :constvalue 5")), key_of(&base), "a location is not the key");
}

/// D715: the document is the incumbent TableFuncScan's to evaluate (the IR only deforms its store), so it is keyed by its
/// class, never its image: one program serves every document of the statement's shape, and no execution hashes the
/// document (a 100k-row JSON_TABLE paid ~10 ms a statement in the key). The key logs it with the class-keyed lists, so a
/// lowering that read it would file no program (`lowering_reads_lists`).
#[test]
fn another_document_keys_the_same() {
    // XMLTABLE: '<r><e>1</e></r>' -> '<r><e>2</e></r>'
    let xml = fixture(XML);
    let doc = "60 114 62 60 101 62 49 60 47 101 62 60 47 114 62 ]";
    assert_eq!(key_of(&edit(&xml, doc, "60 114 62 60 101 62 50 60 47 101 62 60 47 114 62 ]")), key_of(&xml), "another XML document");
    // JSON_TABLE: '[{"a": 1}, {"a": 2}]' -> '[{"a": 1}, {"a": 3}]' (the jsonb image's last numeric digit)
    let json = fixture(JSON);
    assert_eq!(key_of(&edit(&json, "0 0 0 0 128 2 0 ]", "0 0 0 0 128 3 0 ]")), key_of(&json), "another JSON document");
    // the row path still keys apart (the document's class rule reaches only the document and the PASSING values)
    assert_ne!(key_of(&edit(&xml, ROW_PATH, ":constvalue 8 [ 32 0 0 0 47 114 47 120 ]")), key_of(&xml), "another row path");
}

/// D715: the class-keyed document is logged with the lists (`class_keyed_lists`), the safety net's input.
#[test]
fn the_document_is_class_keyed() {
    for rel in [XML, JSON] {
        let ctx = MemoryContext::new("tablefunc key");
        let fx = corpus::load(ctx.mcx(), &fixture(rel)).unwrap_or_else(|e| panic!("fixture: {e}"));
        let arena = Arena::new(1 << 20);
        let g = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).unwrap_or_else(|r| panic!("decompose: {r}"));
        assert!(!gravity_lower::inputs::key::class_keyed_lists(&g).is_empty(), "{rel}: the document is class-keyed");
    }
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
