// Bug campaign batch-186-pl-plpgsql: parser witnesses against C 18.6
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

// pl_gram.y decl_stmt (493-503): the misplaced-label diagnostic fires only
// after a complete "<< any_identifier >>"; a malformed label is a plain
// syntax error at the offending token.
#[test]
fn misplaced_block_label_needs_the_full_production() {
    for (src, msg, pos) in [
        (
            &b"declare << foo >> begin end"[..],
            "block label must be placed before DECLARE, not after",
            9,
        ),
        (&b"declare << 1 >> begin end"[..], "syntax error at or near \"1\"", 12),
        (&b"declare << foo begin end"[..], "syntax error at or near \"begin\"", 16),
        (&b"declare << begin end"[..], "syntax error at or near \"begin\"", 12),
        (&b"declare <<"[..], "syntax error at end of input", 11),
    ] {
        let cx = mcx::MemoryContext::new("plpgsql b186 decl label");
        let mut comp = crate::comp::CompState::new();
        comp.ns_push_label(Some("f"), LABEL_BLOCK);
        let mut parser = parser_for!(cx, comp, src);
        let err = parser.parse_function_body().unwrap_err();
        assert_eq!(err.sqlstate, types_error::ERRCODE_SYNTAX_ERROR, "{src:?}");
        assert_eq!(err.message, msg, "{src:?}");
        assert_eq!(err.internal_position, Some(pos), "{src:?}");
    }
}

// pl_gram.y read_datatype (3007-3013): the datatype text handed to
// parse_datatype is the verbatim source span up to the terminating token,
// trailing whitespace included, and typeStringToTypeName quotes it as is.
#[test]
fn datatype_span_keeps_trailing_whitespace() {
    for (src, msg) in [
        (&b"declare x setof int ; begin end"[..], "invalid type name \"setof int \""),
        (&b"declare x setof int\n\t; begin end"[..], "invalid type name \"setof int\n\t\""),
        (&b"declare x setof int   := 1; begin end"[..], "invalid type name \"setof int   \""),
        (&b"declare x setof int; begin end"[..], "invalid type name \"setof int\""),
    ] {
        let cx = mcx::MemoryContext::new("plpgsql b186 datatype span");
        let mut comp = crate::comp::CompState::new();
        comp.ns_push_label(Some("f"), LABEL_BLOCK);
        let mut parser = parser_for!(cx, comp, src);
        let err = parser.parse_function_body().unwrap_err();
        assert_eq!(err.sqlstate, types_error::ERRCODE_SYNTAX_ERROR, "{src:?}");
        assert_eq!(err.message, msg, "{src:?}");
        assert_eq!(err.internal_position, Some(11), "{src:?}");
    }
}

// pl_gram.y:2787-2792 read_sql_construct: an unused until2/until3 is 0, the
// EOF token, so a construct cut off by end of input ends there and the caller
// reports the error ("syntax error at end of input", or EXIT's own check).
#[test]
fn sql_construct_at_end_of_input_ends_like_c() {
    for (src, msg) in [
        (&b"begin if true"[..], "syntax error at end of input"),
        (&b"begin perform 1"[..], "syntax error at end of input"),
        (&b"begin exit when true"[..], "EXIT cannot be used outside a loop, unless it has a label"),
        (&b"begin raise notice 'x', 1"[..], "missing \", or ; or USING\" at end of SQL expression"),
    ] {
        let cx = mcx::MemoryContext::new("plpgsql b234 eof construct");
        let mut comp = crate::comp::CompState::new();
        comp.ns_push_label(Some("f"), LABEL_BLOCK);
        let mut parser = parser_for!(cx, comp, src);
        let err = parser.parse_function_body().unwrap_err();
        assert_eq!(err.message, msg, "{src:?}");
    }
}
