// Wave-2 audit witnesses against C 18.6 pl_gram.y / pl_scanner.c sites.
use super::*;

fn int_type() -> PlType {
    PlType {
        typoid: 23, // INT4OID
        ttype: TypeKind::Scalar,
        typlen: 4,
        typbyval: true,
        typtype: b'b' as i8,
        collation: types_core::InvalidOid,
        typisarray: false,
        atttypmod: -1,
        typinput: types_core::InvalidOid,
        typioparam: types_core::InvalidOid,
        rec_ident: None,
    }
}

macro_rules! parser_for {
    ($cx:ident, $comp:ident, $src:expr) => {{
        let buf = mcx::slice_borrow_in($cx.mcx(), $src).unwrap();
        Parser {
            sc: PlScanner::new($cx.mcx(), buf),
            comp: &mut $comp,
            check_syntax: false,
            fn_rettype: 2278, // VOIDOID
            fn_retset: false,
            fn_prokind: b'f' as i8,
            fn_input_collation: types_core::InvalidOid,
            fn_is_trigger: false,
            out_param_varno: -1,
            scratch: $cx.mcx(),
            last_endtoken_loc: -1,
        }
    }};
}

fn parse_err(mut comp: crate::comp::CompState, src: &[u8]) -> (Box<PgError>, i32) {
    let cx = mcx::MemoryContext::new("plpgsql w2 parse");
    let mut parser = parser_for!(cx, comp, src);
    let err = parser.parse_function_body().unwrap_err();
    (err, parser.sc.latest_lineno())
}

// cursor_variable (pl_gram.y:2282-2297): a TG_* promise datum is not
// DTYPE_VAR, so OPEN/FETCH/CLOSE on it is "not a simple variable".
#[test]
fn promise_variable_is_not_a_cursor_variable() {
    for src in [&b"begin open tg_name for select 1; end"[..], &b"begin close tg_name; end"[..]] {
        let mut comp = crate::comp::CompState::new();
        comp.ns_push_label(Some("f"), LABEL_BLOCK);
        let mut ty = int_type();
        ty.typoid = REFCURSOROID;
        let dno = comp.build_variable("tg_name", 0, ty, true).unwrap();
        if let PlDatum::Var(v) = &mut comp.datums[dno as usize] {
            v.promise = PROMISE_TG_NAME;
        }
        let (err, _) = parse_err(comp, src);
        assert_eq!(err.sqlstate, types_error::ERRCODE_DATATYPE_MISMATCH);
        assert_eq!(err.message, "cursor variable must be a simple variable");
    }
}

// scan.l {xnstart} under the PL-reserved keyword list returns IDENT "n":
// N'..' names the one-character identifier, not the keyword NCHAR.
#[test]
fn national_prefix_is_the_identifier_n() {
    let mut comp = crate::comp::CompState::new();
    comp.ns_push_label(Some("f"), LABEL_BLOCK);
    let (err, _) = parse_err(comp, b"begin raise N'x'; end");
    assert_eq!(err.message, "unrecognized exception condition \"n\"");

    let mut comp = crate::comp::CompState::new();
    comp.ns_push_label(Some("f"), LABEL_BLOCK);
    let (err, _) = parse_err(comp, b"begin select 1 into N'x'; end");
    assert_eq!(err.message, "\"n\" is not a known variable");
}

// read_into_scalar_list / make_scalar_list1 (pl_gram.y:3663-3747): the
// row's field names are the targets as written (NameOfDatum).
#[test]
fn scalar_target_rows_keep_names_as_written() {
    let cx = mcx::MemoryContext::new("plpgsql w2 fieldnames");
    let mut comp = crate::comp::CompState::new();
    comp.ns_push_label(Some("blk"), LABEL_BLOCK);
    let x = comp.build_variable("x", 1, int_type(), true).unwrap();
    comp.ns_additem(NsType::Var, x, "y");
    let first_row = comp.datums.len();
    let mut parser = parser_for!(cx, comp, b"begin select 1 into y; select 2 into blk.x; end");
    parser.parse_function_body().unwrap();
    let names: Vec<Vec<String>> = comp.datums[first_row..]
        .iter()
        .filter_map(|d| match d {
            PlDatum::Row(r) => Some(r.fieldnames.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(names, vec![vec!["y".to_string()], vec!["blk.x".to_string()]]);
}

// plpgsql_location_to_lineno moves the scanner's latest line, which the
// compile error context reports: each statement computes its line where
// its C grammar action does.
#[test]
fn statement_lines_are_computed_where_the_grammar_actions_run() {
    let with_x = || {
        let mut comp = crate::comp::CompState::new();
        comp.ns_push_label(Some("f"), LABEL_BLOCK);
        comp.build_variable("x", 1, int_type(), true).unwrap();
        comp
    };
    // stmt_open: the line follows cursor_variable.
    let (err, line) =
        parse_err(with_x(), b"begin\n x := 1;\n\n open nosuch for select 1;\nend");
    assert_eq!(err.message, "\"nosuch\" is not a known variable");
    assert_eq!(line, 2);
    // stmt_loop: the line is computed before check_labels.
    let (err, line) =
        parse_err(with_x(), b"begin\n loop\n  exit;\n end loop wrong;\n return;\nend");
    assert_eq!(err.message, "end label \"wrong\" specified for unlabeled block");
    assert_eq!(line, 2);
    // stmt_exit: the line is computed before the label lookup.
    let (err, line) = parse_err(with_x(), b"begin\n x := 1;\n exit\n  nolabel;\nend");
    assert!(err.message.starts_with("there is no label \"nolabel\""), "{}", err.message);
    assert_eq!(line, 3);
    // stmt_case/case_when: WHEN lines follow their branch, the CASE line
    // follows END CASE ;.
    let (_, line) = parse_err(
        with_x(),
        b"begin\n case\n  when x = 1 then\n   x := 1;\n  when x = 2 then\n   x := 2;\n end case;\n nosuch := 1;\nend",
    );
    assert_eq!(line, 2);
    // stmt_commit: the line follows the chain clause.
    let (_, line) =
        parse_err(with_x(), b"begin\n x := 1;\n commit and\n  garbage;\nend");
    assert_eq!(line, 2);
}

#[test]
fn dotted_word_pairs_resolve_quotes_and_skip_comments() {
    let got = crate::scanner::PlScanner::dotted_word_pairs(
        r#"x := "r".a + R/**/.b + "Rec".c + 'r.d' + lbl.r.e -- r.f"#,
    )
    .unwrap();
    let want: Vec<(String, String)> =
        [("r", "a"), ("r", "b"), ("Rec", "c"), ("lbl", "r"), ("r", "e")]
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect();
    assert_eq!(got, want);
}
