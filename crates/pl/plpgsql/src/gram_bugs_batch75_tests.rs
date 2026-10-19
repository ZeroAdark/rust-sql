// Bug campaign batch-75-pl-plpgsql: parser witnesses against C 18.6
// pl_gram.y / pl_scanner.c sites.
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

fn refcursor_type() -> PlType {
    let mut t = int_type();
    t.typoid = REFCURSOROID;
    t
}

fn bound_cursor(comp: &mut crate::comp::CompState, argrow: Dno) {
    let c = comp.build_variable("c", 1, refcursor_type(), true).unwrap();
    if let PlDatum::Var(v) = &mut comp.datums[c as usize] {
        v.cursor_explicit_expr = Some(PlExpr {
            query: "SELECT 1".to_string(),
            parse_mode: RawParseMode::RAW_PARSE_DEFAULT,
            ns: 0,
            expr_id: 0,
            target_param: -1,
        });
        v.cursor_explicit_argrow = argrow;
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

// pl_gram.y stmt_getdiag (1013-1057): the area restriction is checked only
// after the whole item list and ';' are parsed, so a later target error
// (check_assignable, 22005) wins over an earlier forbidden CURRENT item.
#[test]
fn get_diagnostics_area_check_runs_after_the_whole_item_list() {
    let cx = mcx::MemoryContext::new("plpgsql b75 getdiag order");
    let mut comp = crate::comp::CompState::new();
    comp.ns_push_label(Some("f"), LABEL_BLOCK);
    comp.build_variable("a", 1, int_type(), true).unwrap();
    let b = comp.build_variable("b", 1, int_type(), true).unwrap();
    if let PlDatum::Var(v) = &mut comp.datums[b as usize] {
        v.isconst = true;
    }
    let src = b"begin get current diagnostics a = message_text, b = row_count; end";
    let mut parser = parser_for!(cx, comp, src);
    let err = parser.parse_function_body().unwrap_err();
    assert_eq!(err.sqlstate, types_error::ERRCODE_ERROR_IN_ASSIGNMENT);
    assert_eq!(err.message, "variable \"b\" is declared CONSTANT");
    assert_eq!(err.internal_position, Some(49));

    // Missing ',' is a plain syntax error before any area check.
    let cx = mcx::MemoryContext::new("plpgsql b75 getdiag syntax");
    let mut comp = crate::comp::CompState::new();
    comp.ns_push_label(Some("f"), LABEL_BLOCK);
    comp.build_variable("a", 1, int_type(), true).unwrap();
    let src = b"begin get current diagnostics a = message_text a = row_count; end";
    let mut parser = parser_for!(cx, comp, src);
    let err = parser.parse_function_body().unwrap_err();
    assert_eq!(err.sqlstate, types_error::ERRCODE_SYNTAX_ERROR);
    assert_eq!(err.message, "syntax error at or near \"a\"");
    assert_eq!(err.internal_position, Some(48));

    // Control: with a well-formed list the area check still fires, at GET.
    let cx = mcx::MemoryContext::new("plpgsql b75 getdiag area");
    let mut comp = crate::comp::CompState::new();
    comp.ns_push_label(Some("f"), LABEL_BLOCK);
    comp.build_variable("a", 1, int_type(), true).unwrap();
    let src = b"begin get current diagnostics a = message_text, a = row_count; end";
    let mut parser = parser_for!(cx, comp, src);
    let err = parser.parse_function_body().unwrap_err();
    assert_eq!(err.sqlstate, types_error::ERRCODE_SYNTAX_ERROR);
    assert_eq!(
        err.message,
        "diagnostics item MESSAGE_TEXT is not allowed in GET CURRENT DIAGNOSTICS"
    );
    assert_eq!(err.internal_position, Some(7));
}

// pl_gram.y read_cursor_args (3963): named notation is recognised only when
// plpgsql_peek2 sees a raw IDENT followed by := or =>, so a compound
// reference r.x stays part of the value expression.
#[test]
fn compound_reference_is_not_a_named_cursor_argument() {
    for src in [
        &b"begin open c(r.x := 1); end"[..],
        &b"begin open c(r.x => 1); end"[..],
    ] {
        let cx = mcx::MemoryContext::new("plpgsql b75 cursor args");
        let mut comp = crate::comp::CompState::new();
        comp.ns_push_label(Some("f"), LABEL_BLOCK);
        comp.build_rec("r", 1, true);
        let arg = comp.build_variable("r.x", 1, int_type(), false).unwrap();
        let argrow = comp.build_row("c", 1, vec![arg]);
        bound_cursor(&mut comp, argrow);
        let mut parser = parser_for!(cx, comp, src);
        let block = parser.parse_function_body().unwrap();
        let PlStmt::Open { argquery: Some(q), .. } = &block.body[0] else {
            panic!("expected OPEN with args: {:?}", block.body);
        };
        assert_eq!(q.query, &core::str::from_utf8(src).unwrap()[13..21]);
    }

    // Control: a plain identifier is a named argument.
    let cx = mcx::MemoryContext::new("plpgsql b75 cursor args named");
    let mut comp = crate::comp::CompState::new();
    comp.ns_push_label(Some("f"), LABEL_BLOCK);
    let arg = comp.build_variable("x", 1, int_type(), false).unwrap();
    let argrow = comp.build_row("c", 1, vec![arg]);
    bound_cursor(&mut comp, argrow);
    let mut parser = parser_for!(cx, comp, b"begin open c(y := 1); end");
    let err = parser.parse_function_body().unwrap_err();
    assert_eq!(err.sqlstate, types_error::ERRCODE_SYNTAX_ERROR);
    assert_eq!(err.message, "cursor \"c\" has no argument named \"y\"");
    assert_eq!(err.internal_position, Some(14));
}

// pl_gram.y for_control (1378-1380): the EXECUTE arm names both accepted
// terminators.
#[test]
fn for_in_execute_missing_terminator_names_loop_or_using() {
    let cx = mcx::MemoryContext::new("plpgsql b75 for execute");
    let mut comp = crate::comp::CompState::new();
    comp.ns_push_label(Some("f"), LABEL_BLOCK);
    comp.build_rec("r", 1, true);
    let mut parser = parser_for!(cx, comp, b"begin for r in execute 'select 1'; end");
    let err = parser.parse_function_body().unwrap_err();
    assert_eq!(err.sqlstate, types_error::ERRCODE_SYNTAX_ERROR);
    assert_eq!(err.message, "missing \"LOOP or USING\" at end of SQL expression");
    assert_eq!(err.internal_position, Some(34));
}

// pl_scanner.c plpgsql_scanner_errposition (512): the byte offset becomes a
// character position under the database encoding, not always UTF-8.
#[test]
fn error_position_counts_characters_in_the_database_encoding() {
    let src = "begin /*\u{e9}\u{e9}*/ end if; end".as_bytes();
    let saved = mbutils::GetDatabaseEncoding();
    for (enc, pos) in [(wchar::PG_UTF8, 18), (wchar::PG_SQL_ASCII, 20)] {
        mbutils::SetDatabaseEncoding(enc).unwrap();
        let cx = mcx::MemoryContext::new("plpgsql b75 errposition");
        let mut comp = crate::comp::CompState::new();
        comp.ns_push_label(Some("f"), LABEL_BLOCK);
        let mut parser = parser_for!(cx, comp, src);
        let err = parser.parse_function_body().unwrap_err();
        assert_eq!(err.sqlstate, types_error::ERRCODE_SYNTAX_ERROR);
        assert_eq!(err.internal_position, Some(pos), "encoding {enc}");
    }
    mbutils::SetDatabaseEncoding(saved).unwrap();
}
