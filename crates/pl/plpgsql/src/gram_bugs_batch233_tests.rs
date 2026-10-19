// Bug campaign batch-233-mixed-lowtail: parser witnesses against C 18.6
// pl_gram.y sites.
use super::*;

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

// pl_gram.y comp_option (385-410): a '#' directive with no production is
// the grammar's plain syntax error at the offending token.
#[test]
fn unknown_compiler_directive_is_a_syntax_error() {
    for (src, msg, pos) in [
        (&b"#foo begin end"[..], "syntax error at or near \"foo\"", 2),
        (&b"#\"foo\" begin end"[..], "syntax error at or near \"\"foo\"\"", 2),
        (&b"# 12 begin end"[..], "syntax error at or near \"12\"", 3),
        (&b"#option foo begin end"[..], "syntax error at or near \"foo\"", 9),
    ] {
        let cx = mcx::MemoryContext::new("plpgsql b233 comp option");
        let mut comp = crate::comp::CompState::new();
        comp.ns_push_label(Some("f"), LABEL_BLOCK);
        let mut parser = parser_for!(cx, comp, src);
        let err = parser.parse_function_body().unwrap_err();
        assert_eq!(err.sqlstate, types_error::ERRCODE_SYNTAX_ERROR, "{src:?}");
        assert_eq!(err.message, msg, "{src:?}");
        assert_eq!(err.internal_position, Some(pos), "{src:?}");
    }
}

// pl_gram.y read_sql_construct (2792-2796): end of input (token 0) matches
// an absent until2/until3 like a terminator, so the SQL text is accepted
// and the grammar's next token reports the EOF; only ';' (or EOF with every
// terminator set) reaches the 'missing "X"' diagnostic.
#[test]
fn end_of_input_inside_sql_construct_reports_the_eof() {
    for (src, msg, pos) in [
        (&b"begin for r in execute 'select 1'"[..], "syntax error at end of input", 34),
        (&b"begin for r in execute 'select 1' end"[..], "syntax error at end of input", 38),
        (&b"begin for r in execute 'select 1' using 1"[..], "syntax error at end of input", 42),
        (&b"begin for r in select 1"[..], "syntax error at end of input", 24),
        (&b"begin x := 1"[..], "syntax error at end of input", 13),
        (&b"begin x := (1"[..], "mismatched parentheses at end of input", 14),
        (&b"begin if true"[..], "syntax error at end of input", 14),
        (&b"begin for r in execute 'select 1' loop"[..], "syntax error at end of input", 39),
        (
            &b"begin for r in execute 'select 1'; end loop; end"[..],
            "missing \"LOOP or USING\" at end of SQL expression",
            34,
        ),
    ] {
        let cx = mcx::MemoryContext::new("plpgsql b233 eof");
        let mut comp = crate::comp::CompState::new();
        comp.ns_push_label(Some("f"), LABEL_BLOCK);
        comp.build_rec("r", 1, true);
        comp.build_variable("x", 1, int_type(), true).unwrap();
        let mut parser = parser_for!(cx, comp, src);
        let err = parser.parse_function_body().unwrap_err();
        assert_eq!(err.sqlstate, types_error::ERRCODE_SYNTAX_ERROR, "{src:?}");
        assert_eq!(err.message, msg, "{src:?}");
        assert_eq!(err.internal_position, Some(pos), "{src:?}");
    }
}
