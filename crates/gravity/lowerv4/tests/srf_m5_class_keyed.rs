//! D715 / D745 (M5-FROM): the content key keys an XMLTABLE / JSON_TABLE document (lower key.rs `table_func`) and a FROM
//! set function's by-reference arguments outside a scalar call or an inlined body (key.rs `Op::Srf`, form (c)) by their
//! class, never their image (the `lit` rule), so one program serves every document / argument of a statement's shape.
//! That is sound only while the lowering never reads them: srf.tablefunc and form (c) are StoreScans over the incumbent
//! TableFuncScan / FunctionScan, which evaluates them; the IR only deforms the store. This pins it: each fixture lowers
//! with no bake read of the literal's image, and its program is filed (`no_cache` false: `lowering_reads_lists` found no
//! read of a class-keyed image). A lowering that ever read one fails here, and would file no program in the server.
#[allow(dead_code)]
use gravity_lowerv4_it as common;

/// (decompose plan fixture, the class-keyed Const's `:constvalue` bytes as the plan prints them: the whole varlena image).
const CASES: &[(&str, &str)] = &[
    // SELECT * FROM xmltable('/r/e' PASSING '<r><e>1</e></r>' COLUMNS v int PATH '.')
    ("regress-061", "76 0 0 0 60 114 62 60 101 62 49 60 47 101 62 60 47 114 62"),
    // SELECT * FROM json_table('[{"a": 1}, {"a": 2}]', '$[*]' COLUMNS (a int PATH '$.a')) jt
    (
        "regress-060",
        "0 1 0 0 2 0 0 64 24 0 0 208 24 0 0 80 1 0 0 32 1 0 0 128 11 0 0 16 97 0 0 0 32 0 0 0 0 128 1 0 1 0 0 32 1 0 0 128 11 0 0 16 97 0 0 0 32 0 0 0 0 128 2 0",
    ),
    // D745: SELECT * FROM jsonb_each('{"k": 1, "j": 2}'::jsonb) (form (c))
    ("srf-022", "176 0 0 0 2 0 0 32 1 0 0 128 1 0 0 0 10 0 0 16 8 0 0 16 106 107 0 0 32 0 0 0 0 128 2 0 32 0 0 0 0 128 1 0"),
];

#[test]
fn the_lowering_never_reads_a_class_keyed_literal() {
    for (id, bytes) in CASES {
        let text = std::fs::read_to_string(common::plans_dir().join(format!("{id}.plan"))).unwrap_or_else(|e| panic!("{id}: {e}"));
        assert_eq!(text.matches(&format!("[ {bytes} ]")).count(), 1, "{id}: the fixture holds the literal once");
        let image: Vec<u8> = bytes.split(' ').map(|b| b.parse().unwrap()).collect();
        let (l, baked) = common::lower_fixture_baked(&text, &image).unwrap_or_else(|| panic!("{id}: decomposes"));
        let l = l.unwrap_or_else(|r| panic!("{id}: lowers: {r}"));
        assert!(!baked, "{id}: the lowering read the literal's image (the content key keys it by class: D715 / D745)");
        assert!(!l.descs.no_cache, "{id}: the program is filed (no class-keyed image read)");
    }
}
