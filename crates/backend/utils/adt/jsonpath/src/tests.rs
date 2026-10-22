//! Round-trip tests against vectors extracted from C 18.3 regress
//! expected/jsonpath.out (in/out canonical forms + error texts).

use std::sync::Once;

use mcx::MemoryContext;
use types_error::SoftErrorContext;

use crate::path::{jsonpath_in, jsonpath_out, JSONPATH_LAX, JSONPATH_VERSION};

use crate::vectors::{ERR_VECTORS, OK_VECTORS};

fn setup() {
    let _ = mbutils::SetDatabaseEncoding(wchar::PG_UTF8);
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        mbutils::init_seams();
    });
    ::pg_locale::set_default_locale_c_for_tests();
}

fn out_text(image: &[u8]) -> String {
    let cx = MemoryContext::new("jsonpath test out");
    let v = jsonpath_out(cx.mcx(), image).expect("jsonpath_out");
    assert_eq!(v.last(), Some(&0));
    String::from_utf8(v[..v.len() - 1].to_vec()).expect("utf8 output")
}

#[test]
fn regress_ok_vectors_round_trip() {
    setup();
    for (input, expected) in OK_VECTORS {
        let cx = MemoryContext::new("jsonpath test");
        let image = jsonpath_in(cx.mcx(), input.as_bytes(), None)
            .unwrap_or_else(|e| panic!("jsonpath_in({input:?}): {}", e.message()))
            .expect("hard path returns Some");
        let out = out_text(&image);
        assert_eq!(&out, expected, "canonical form of {input:?}");

        // The canonical form re-parses to itself (regress does the same via
        // the text cast round trip).
        let cx2 = MemoryContext::new("jsonpath test 2");
        let image2 = jsonpath_in(cx2.mcx(), out.as_bytes(), None)
            .unwrap_or_else(|e| panic!("re-parse {out:?}: {}", e.message()))
            .expect("hard path returns Some");
        assert_eq!(out_text(&image2), out, "idempotent canonical form of {input:?}");
    }
}

#[test]
fn regress_err_vectors() {
    setup();
    for (input, msg, detail) in ERR_VECTORS {
        let cx = MemoryContext::new("jsonpath test err");
        let err = match jsonpath_in(cx.mcx(), input.as_bytes(), None) {
            Err(e) => e,
            Ok(v) => panic!(
                "expected error {msg:?} for {input:?}, got {:?}",
                v.map(|img| out_text(&img))
            ),
        };
        assert_eq!(err.message(), *msg, "message for {input:?}");
        if let Some(detail) = detail {
            assert_eq!(err.detail(), Some(*detail), "detail for {input:?}");
        }
    }
}

#[test]
fn soft_errors_are_recorded_not_raised() {
    setup();
    for (input, msg, _detail) in ERR_VECTORS {
        let cx = MemoryContext::new("jsonpath test soft");
        let mut esc = SoftErrorContext::new(true);
        let res = jsonpath_in(cx.mcx(), input.as_bytes(), Some(&mut esc))
            .unwrap_or_else(|e| panic!("soft parse of {input:?} raised: {}", e.message()));
        assert!(res.is_none(), "soft error for {input:?}");
        assert!(esc.error_occurred(), "escontext set for {input:?}");
        assert_eq!(esc.error().expect("saved error").message(), *msg, "{input:?}");
    }
}

// Bison expr/predicate class + method-keyword disambiguation + scanner
// edge cases from the audit (C 18.3 behavior derived from the grammar/flex
// rules; not present in regress).
static EXTRA_OK: &[(&str, &str)] = &[
    ("$.type", "$.\"type\""),
    ("$.size", "$.\"size\""),
    ("$.datetime", "$.\"datetime\""),
    ("$.decimal", "$.\"decimal\""),
    ("$.timestamp_tz", "$.\"timestamp_tz\""),
    ("(1).type()", "(1).type()"),
];

static EXTRA_ERR: &[(&str, &str)] = &[
    ("1 && 2", "syntax error at or near \"&&\" of jsonpath input"),
    ("$ ? (@)", "syntax error at or near \")\" of jsonpath input"),
    ("$?(1)", "syntax error at or near \")\" of jsonpath input"),
    ("exists(1 == 2)", "syntax error at or near \"==\" of jsonpath input"),
    ("!(1)", "syntax error at or near \")\" of jsonpath input"),
    // yytext for a keyword token emitted by the xnq {blank}+ rule is the
    // blank run (flex), hence " " not "is".
    ("(1) is unknown", "syntax error at or near \" \" of jsonpath input"),
    ("$[1 == 1]", "syntax error at or near \"==\" of jsonpath input"),
    ("(1 == 1) + 2", "syntax error at or near \"+\" of jsonpath input"),
    ("exists($.a) + 1", "syntax error at or near \"+\" of jsonpath input"),
    ("-(1 == 1)", "syntax error at end of jsonpath input"),
    ("1 == !(true)", "syntax error at or near \"!\" of jsonpath input"),
    ("\"abc\\", "unexpected end after backslash at or near \"\\\" of jsonpath input"),
    ("\"a\\\nb\"", "unexpected end after backslash at or near \"\\\" of jsonpath input"),
    ("1 2 \"x", "syntax error at or near \"2\" of jsonpath input"),
    // expr never starts with EXISTS_P: bison rejects the keyword lookahead
    // before the operand is parsed (its numeric never reaches numeric_in).
    ("$[exists(1)]", "syntax error at end of jsonpath input"),
    ("$[exists (1)]", "syntax error at or near \" \" of jsonpath input"),
    ("$[exists(1e1000000)]", "syntax error at end of jsonpath input"),
    ("$[1 to exists(1e1000000)]", "syntax error at end of jsonpath input"),
    ("$ + exists(1e1000000)", "syntax error at end of jsonpath input"),
    ("$ ? (1 * exists(1e1000000))", "syntax error at end of jsonpath input"),
    ("$ == exists(1e1000000)", "syntax error at end of jsonpath input"),
    ("$ ? (@ == exists (1))", "syntax error at or near \" \" of jsonpath input"),
    ("-exists(1e1000000)", "syntax error at end of jsonpath input"),
    ("exists(exists(1e1000000))", "syntax error at end of jsonpath input"),
];

#[test]
fn audit_extra_vectors() {
    setup();
    for (input, expected) in EXTRA_OK {
        let cx = MemoryContext::new("jsonpath extra ok");
        let image = jsonpath_in(cx.mcx(), input.as_bytes(), None)
            .unwrap_or_else(|e| panic!("jsonpath_in({input:?}): {}", e.message()))
            .expect("hard path returns Some");
        assert_eq!(&out_text(&image), expected, "canonical form of {input:?}");
    }
    for (input, msg) in EXTRA_ERR {
        let cx = MemoryContext::new("jsonpath extra err");
        let res = jsonpath_in(cx.mcx(), input.as_bytes(), None);
        match res {
            Err(e) => assert_eq!(e.message(), *msg, "message for {input:?}"),
            Ok(v) => panic!(
                "expected error {msg:?} for {input:?}, got {:?}",
                v.map(|img| out_text(&img))
            ),
        }
    }
}

#[test]
fn header_flags() {
    setup();
    let cx = MemoryContext::new("jsonpath header");
    let lax = jsonpath_in(cx.mcx(), b"$.a", None).unwrap().unwrap();
    let hdr = u32::from_ne_bytes([lax[4], lax[5], lax[6], lax[7]]);
    assert_eq!(hdr, JSONPATH_VERSION | JSONPATH_LAX);
    let strict = jsonpath_in(cx.mcx(), b"strict $.a", None).unwrap().unwrap();
    let hdr = u32::from_ne_bytes([strict[4], strict[5], strict[6], strict[7]]);
    assert_eq!(hdr, JSONPATH_VERSION);
    // Varlena length header covers the whole image.
    let word = u32::from_ne_bytes([lax[0], lax[1], lax[2], lax[3]]);
    assert_eq!((word >> 2) as usize, lax.len());
}

#[test]
fn send_recv_round_trip() {
    setup();
    let cx = MemoryContext::new("jsonpath sendrecv");
    let mcx = cx.mcx();
    for (input, expected) in OK_VECTORS.iter().take(40) {
        let image = jsonpath_in(mcx, input.as_bytes(), None).unwrap().unwrap();
        let sent = crate::path::jsonpath_send(mcx, &image).unwrap();
        let bytes = sent.data();
        assert_eq!(bytes[0], 1, "version byte for {input:?}");
        assert_eq!(
            core::str::from_utf8(&bytes[1..]).unwrap(),
            *expected,
            "send payload for {input:?}"
        );
        let mut msg = stringinfo::StringInfo::from_vec(mcx::slice_in(mcx, bytes).unwrap()).unwrap();
        let recv = crate::path::jsonpath_recv(mcx, &mut msg).unwrap();
        assert_eq!(out_text(&recv), *expected, "recv round trip for {input:?}");
    }
}

// Bison's heap stacks bound nesting at YYMAXDEPTH states; the parser keeps
// no native recursion. Cutoffs and yytext measured on C 18.6 at 2MB.
const CUTOFFS: &[(&str, &str, &str, &str, &str, usize, &str)] = &[
    ("", "(", "1", ")", "", 9995, ")"),
    ("", "(", "$", ")", "", 9995, ")"),
    ("", "-", "1", "", "", 9996, "1"),
    ("", "+", "1", "", "", 9996, "1"),
    ("", "-(", "1", ")", "", 4997, ")"),
    ("$ ? (", "!(", "@ == 1", ")", ")", 4995, "1"),
    ("$", "[$", "", "]", "", 4997, "]"),
    ("", "1 + (", "1", ")", "", 3331, ")"),
    ("$ ? (", "(", "@ == 1", ")", ")", 9991, "1"),
];

fn nested(prefix: &str, open: &str, core: &str, close: &str, suffix: &str, n: usize) -> Vec<u8> {
    let mut s = Vec::with_capacity(prefix.len() + n * (open.len() + close.len()) + 64);
    s.extend_from_slice(prefix.as_bytes());
    for _ in 0..n {
        s.extend_from_slice(open.as_bytes());
    }
    s.extend_from_slice(core.as_bytes());
    for _ in 0..n {
        s.extend_from_slice(close.as_bytes());
    }
    s.extend_from_slice(suffix.as_bytes());
    s
}

fn on_small_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(256 << 10)
        .spawn(move || {
            setup();
            f()
        })
        .expect("spawn")
        .join()
        .expect("parse must not overflow the native stack")
}

#[test]
fn parser_stack_overflows_exactly_at_bison_yymaxdepth() {
    for &(prefix, open, core, close, suffix, last_ok, near) in CUTOFFS {
        let ok = nested(prefix, open, core, close, suffix, last_ok);
        let bad = nested(prefix, open, core, close, suffix, last_ok + 1);
        let (parsed, err) = on_small_stack(move || {
            let cx = MemoryContext::new("jsonpath depth");
            let parsed = crate::gram::parsejsonpath(cx.mcx(), &ok, None)
                .map(|r| r.is_some())
                .map_err(|e| e.message().to_string());
            let err = match crate::gram::parsejsonpath(cx.mcx(), &bad, None) {
                Err(e) => {
                    let loc = e.location().expect("jsonpath_yyerror location");
                    assert_eq!(loc.filename.as_deref(), Some("jsonpath_scan.l"));
                    assert_eq!(loc.funcname.as_deref(), Some("jsonpath_yyerror"));
                    (e.sqlstate(), e.message().to_string())
                }
                Ok(_) => panic!("{open:?} x{}: parsed past YYMAXDEPTH", last_ok + 1),
            };
            (parsed, err)
        });
        assert_eq!(parsed, Ok(true), "{open:?} x{last_ok} parses");
        assert_eq!(err.0, types_error::ERRCODE_SYNTAX_ERROR, "{open:?}");
        assert_eq!(
            err.1,
            format!("memory exhausted at or near \"{near}\" of jsonpath input"),
            "{open:?} x{}",
            last_ok + 1
        );
    }
}

#[test]
fn parser_stack_overflow_is_soft_errorable() {
    let input = nested("$ ? (", "(", "@ > 1", ")", ")", 100_000);
    let (none, msg) = on_small_stack(move || {
        let cx = MemoryContext::new("jsonpath depth soft");
        let mut esc = SoftErrorContext::new(true);
        let res = jsonpath_in(cx.mcx(), &input, Some(&mut esc))
            .unwrap_or_else(|e| panic!("soft overflow raised hard: {}", e.message()));
        (res.is_none(), esc.error().map(|e| (e.sqlstate(), e.message().to_string())))
    });
    assert!(none);
    assert_eq!(
        msg,
        Some((
            types_error::ERRCODE_SYNTAX_ERROR,
            "memory exhausted at or near \"(\" of jsonpath input".to_string()
        ))
    );
}

#[test]
fn deep_unary_chain_round_trips() {
    let cx = MemoryContext::new("jsonpath deep");
    let input = nested("", "-(", "$", ")", "", 50);
    let image = jsonpath_in(cx.mcx(), &input, None).unwrap().unwrap();
    assert_eq!(out_text(&image), format!("({}-${}", "-(".repeat(49), ")".repeat(50)));
}


// jsonpath.c checks for interrupts per parsed item (makeItemType), per
// flattened item and per printed item; a pending cancel surfaces as 57014
// from input conversion and from output rendering.
#[test]
fn parse_flatten_and_print_poll_for_interrupts() {
    use std::sync::Mutex;
    use std::thread::ThreadId;
    use types_error::{PgError, ERRCODE_QUERY_CANCELED};

    // The mock only cancels threads that armed themselves, so parallel
    // tests in this binary never observe another test's cancel.
    static ARMED: Mutex<Vec<ThreadId>> = Mutex::new(Vec::new());
    static INSTALL: Once = Once::new();
    fn process_interrupts() -> types_error::PgResult<()> {
        let me = std::thread::current().id();
        let mut armed = ARMED.lock().unwrap();
        if let Some(i) = armed.iter().position(|t| *t == me) {
            armed.swap_remove(i);
            init_small::globals::SetInterruptPending(false);
            return Err(PgError::error("canceling statement due to user request")
                .with_sqlstate(ERRCODE_QUERY_CANCELED)
                .into());
        }
        Ok(())
    }
    fn arm() {
        ARMED.lock().unwrap().push(std::thread::current().id());
        init_small::globals::SetInterruptPending(true);
    }

    setup();
    INSTALL.call_once(|| {
        if !postgres_seams::check_for_interrupts::is_installed() {
            postgres_seams::check_for_interrupts::set(process_interrupts);
        }
    });
    let cx = MemoryContext::new("jsonpath cfi");
    let image = jsonpath_in(cx.mcx(), b"$.a.b", None).unwrap().unwrap();

    arm();
    let err = jsonpath_in(cx.mcx(), b"$.a.b", None).unwrap_err();
    assert_eq!(err.sqlstate(), ERRCODE_QUERY_CANCELED);

    arm();
    let err = jsonpath_out(cx.mcx(), &image).unwrap_err();
    assert_eq!(err.sqlstate(), ERRCODE_QUERY_CANCELED);
    assert!(ARMED.lock().unwrap().is_empty());
}

// jsonpath.c:189 reserves 4 * len through enlargeStringInfo: the estimate is
// what is bounded (>= MaxAllocSize is 54000, a negative int product XX000),
// never the estimate plus the header.
#[test]
fn flatten_reserve_bounds_match_enlarge_string_info() {
    assert_eq!(crate::path::flatten_reserve(10).unwrap(), 1024);
    assert_eq!(crate::path::flatten_reserve(1000).unwrap(), 4096);
    assert_eq!(crate::path::flatten_reserve(268_435_454).unwrap(), 1_073_741_823);
    assert_eq!(crate::path::flatten_reserve(268_435_455).unwrap(), 1_073_741_823);
    let err = crate::path::flatten_reserve(268_435_456).unwrap_err();
    assert_eq!(
        err.message(),
        "string buffer exceeds maximum allowed length (1073741823 bytes)"
    );
    assert_eq!(err.sqlstate(), types_error::ERRCODE_PROGRAM_LIMIT_EXCEEDED);
    assert_eq!(
        err.detail(),
        Some("Cannot enlarge string buffer containing 0 bytes by 1073741824 more bytes.")
    );
    let err = crate::path::flatten_reserve(536_870_912).unwrap_err();
    assert_eq!(err.message(), "invalid string enlargement request size: -2147483648");
}
