use std::sync::Once;

use mcx::{Mcx, MemoryContext, PgVec};
use stringinfo::StringInfo;

use crate::from::{copy_from_error_context, CopyFromState, CopySrc};
use crate::fromparse::{EolType, RAW_BUF_SIZE};
use crate::to::copy_attribute_out_text;
use crate::CopyFormatOptions;

fn test_ctx() -> &'static MemoryContext {
    thread_local! {
        static CTX: &'static MemoryContext =
            Box::leak(Box::new(MemoryContext::new("copy-test")));
    }
    CTX.with(|c| *c)
}

static SETUP: Once = Once::new();

static INTERRUPT_PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn setup_fd() {
    SETUP.call_once(|| {
        guc_tables::init_seams();
        elog::init_seams();
        fd::init_seams();
        xact_seams::get_current_sub_transaction_id::set(|| 1);
        aio_seams::pgaio_closing_fd::set(|_| {});
        waitevent_seams::pgstat_report_wait_start::set(|_| {});
        waitevent_seams::pgstat_report_wait_end::set(|| {});
        postgres_seams::check_for_interrupts::set(|| {
            if INTERRUPT_PENDING.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(Box::new(
                    types_error::PgError::error("canceling statement due to user request")
                        .with_sqlstate(types_error::ERRCODE_QUERY_CANCELED),
                ));
            }
            Ok(())
        });
        mbutils::init_seams();
    });
    thread_local! {
        static FD_READY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    FD_READY.with(|f| {
        if !f.get() {
            fd::InitFileAccess();
            f.set(true);
        }
    });
}

fn out_text(s: &[u8], delim: u8) -> Vec<u8> {
    let mcx = test_ctx().mcx();
    let mut buf = StringInfo::new_in(mcx).unwrap();
    copy_attribute_out_text(&mut buf, s, delim).unwrap();
    buf.as_bytes().to_vec()
}

#[test]
fn attribute_out_text_matches_c_escapes() {
    assert_eq!(out_text(b"plain", b'\t'), b"plain");
    assert_eq!(out_text(b"a\tb", b'\t'), b"a\\tb");
    assert_eq!(out_text(b"a\nb\rc", b'\t'), b"a\\nb\\rc");
    assert_eq!(out_text(b"a\x08\x0c\x0b", b'\t'), b"a\\b\\f\\v");
    assert_eq!(out_text(b"back\\slash", b'\t'), b"back\\\\slash");
    assert_eq!(out_text(b"a|b", b'|'), b"a\\|b");
    // Non-escaped control chars pass through (C's default arm).
    assert_eq!(out_text(b"a\x01b", b'\t'), b"a\x01b");
    // Multibyte UTF-8 passes through untouched on the server-encoding arm.
    assert_eq!(out_text("héllo⽇".as_bytes(), b'\t'), "héllo⽇".as_bytes());
}

fn mk_state<'mcx>(
    mcx: Mcx<'mcx>,
    delim: u8,
    null_print: &'static str,
) -> CopyFromState<'mcx, 'static> {
    CopyFromState {
        opts: CopyFormatOptions {
            file_encoding: wchar::PG_UTF8,
            binary: false,
            csv_mode: false,
            parquet: false,
            parquet_match_by_name: false,
            parquet_coerce_epoch: false,
            freeze: false,
            delim,
            quote: b'"',
            escape: b'"',
            null_print,
            default_print: None,
            header_line: crate::CopyHeaderChoice::False,
            force_quote: None,
            force_quote_all: false,
            force_notnull: None,
            force_notnull_all: false,
            force_null: None,
            force_null_all: false,
            convert_selectively: false,
            convert_select: None,
            on_error: crate::CopyOnErrorChoice::Stop,
            log_verbosity: crate::CopyLogVerbosityChoice::Default,
            reject_limit: 0,
        },
        src: CopySrc::File { fd: -1, filename: "" },
        raw_buf: mcx::vec_from_elem_in(mcx, 0u8, RAW_BUF_SIZE + 1),
        raw_buf_index: 0,
        raw_buf_len: 0,
        raw_reached_eof: false,
        input_reached_eof: false,
        input_reached_error: false,
        input_buf: None,
        input_buf_index: 0,
        input_buf_len: 0,
        line_buf: PgVec::new_in(mcx),
        line_buf_valid: false,
        attribute_buf: PgVec::new_in(mcx),
        binary_attr_buf: stringinfo::StringInfo::new_in(mcx).unwrap(),
        raw_fields: PgVec::new_in(mcx),
        max_fields: 8,
        eol_type: EolType::Unknown,
        cur_lineno: 0,
        cur_attidx: None,
        cur_attval_off: None,
        cur_attval_stale: PgVec::new_in(mcx),
        file_encoding: wchar::PG_UTF8,
        need_transcoding: false,
        conversion_proc: 0,
        convertcx: MemoryContext::new("COPY convert"),
        attnumlist: PgVec::new_in(mcx),
        in_functions: PgVec::new_in(mcx),
        typioparams: PgVec::new_in(mcx),
        atttypmods: PgVec::new_in(mcx),
        attnames: PgVec::new_in(mcx),
        force_notnull_flags: PgVec::new_in(mcx),
        force_null_flags: PgVec::new_in(mcx),
        convert_select_flags: None,
        defexprs: PgVec::new_in(mcx),
        defmap: PgVec::new_in(mcx),
        where_clause: types_nodes::NodeList::nil(),
        relname: String::new(),
        relname_only: false,
        escontext: None,
        num_errors: 0,
        defaults: mcx::vec_from_elem_in(mcx, false, 8),
        bytes_processed: 0,
        volatile_defexprs: false,
    }
}

fn fields_of(state: &CopyFromState<'_, '_>, n: usize) -> Vec<Option<Vec<u8>>> {
    (0..n)
        .map(|i| {
            let off = state.raw_fields[i];
            if off < 0 {
                return None;
            }
            let rest = &state.attribute_buf[off as usize..];
            let end = rest.iter().position(|&b| b == 0).unwrap();
            Some(rest[..end].to_vec())
        })
        .collect()
}

fn split_line(line: &[u8], delim: u8, null_print: &'static str) -> Vec<Option<Vec<u8>>> {
    setup_fd();
    let mcx = test_ctx().mcx();
    let mut st = mk_state(mcx, delim, null_print);
    mcx::vec_append_bytes(&mut st.line_buf, line).unwrap();
    let n = st.copy_read_attributes_text().unwrap();
    fields_of(&st, n)
}

#[test]
fn read_attributes_text_decodes_escapes_and_nulls() {
    let f = split_line(b"a\tb", b'\t', "\\N");
    assert_eq!(f, vec![Some(b"a".to_vec()), Some(b"b".to_vec())]);

    let f = split_line(b"a\\tb\t\\N\t\\\\x\t\\101\\x41", b'\t', "\\N");
    assert_eq!(
        f,
        vec![
            Some(b"a\tb".to_vec()),
            None,
            Some(b"\\x".to_vec()),
            Some(b"AA".to_vec()),
        ]
    );

    // Escaped \N is data, not null; empty field is empty string.
    let f = split_line(b"\\\\N\t", b'\t', "\\N");
    assert_eq!(f, vec![Some(b"\\N".to_vec()), Some(b"".to_vec())]);

    // Octal/hex partials and passthrough of unknown escapes.
    let f = split_line(b"\\8\\x\\q", b'\t', "\\N");
    assert_eq!(f, vec![Some(b"8xq".to_vec())]);
}

#[test]
fn q8_f1_hex_escape_is_22021_not_slice_panic() {
    setup_fd();
    mbutils::SetDatabaseEncoding(wchar::PG_UTF8).unwrap();
    let mcx = test_ctx().mcx();
    let mut st = mk_state(mcx, b'\t', "\\N");
    mcx::vec_append_bytes(&mut st.line_buf, b"2\t\\xdd").unwrap();
    let err = st.copy_read_attributes_text().unwrap_err();
    assert_eq!(
        err.sqlstate(),
        types_error::ERRCODE_CHARACTER_NOT_IN_REPERTOIRE
    );
    assert!(err.message().contains("invalid byte sequence"));

    let mut nd = types_tuple::NameData::default();
    nd.namestrcpy("a");
    st.attnames.push(nd);
    st.cur_attidx = Some(0);
    st.cur_attval_off = Some(2);
    st.cur_lineno = 2;
    st.relname = "fzmin2".into();
    st.line_buf_valid = true;
    let wrapped = copy_from_error_context(&st, err);
    assert_eq!(
        wrapped.sqlstate(),
        types_error::ERRCODE_CHARACTER_NOT_IN_REPERTOIRE
    );
    // The field is NUL-terminated before the encoding check, as in C, so the
    // stale column pointer shows the offending byte.
    assert_eq!(wrapped.context(), Some("COPY fzmin2, line 2, column a: \"\u{fffd}\""));
}

#[test]
fn out_then_in_round_trips() {
    let nasty: &[&[u8]] = &[
        b"plain",
        b"tab\there",
        b"nl\nhere",
        b"cr\rhere",
        b"back\\slash",
        b"\x08\x0c\x0b\x01\x1f",
        "üñîçødé ⽇本".as_bytes(),
        b"\\N",
        b"N",
        b"",
    ];
    for &case in nasty {
        let encoded = out_text(case, b'\t');
        let fields = split_line(&encoded, b'\t', "\\N");
        assert_eq!(fields.len(), 1, "case {case:?}");
        assert_eq!(fields[0].as_deref(), Some(case), "case {case:?}");
    }
    // A full multi-field line with a NULL.
    let mut line = out_text(b"a\tb", b'\t');
    line.push(b'\t');
    line.extend_from_slice(b"\\N");
    line.push(b'\t');
    line.extend_from_slice(&out_text(b"c\\d", b'\t'));
    let fields = split_line(&line, b'\t', "\\N");
    assert_eq!(
        fields,
        vec![Some(b"a\tb".to_vec()), None, Some(b"c\\d".to_vec())]
    );
}

/// A COPY source file under the temp dir, removed on drop: the readers drop
/// it once the file is open (unlink after open), and a panic's unwind takes
/// it too.
struct DataFile(std::path::PathBuf);

impl Drop for DataFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The calling thread's source file for `tag` (each test runs on its own
/// thread).
fn data_path(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "copy-test-{}-{tag}-{:?}.data",
        std::process::id(),
        std::thread::current().id()
    ))
}

fn data_file(tag: &str, content: &[u8]) -> DataFile {
    let file = DataFile(data_path(tag));
    std::fs::write(&file.0, content).unwrap();
    file
}

#[test]
fn copy_sources_leave_nothing_behind() {
    assert_eq!(read_lines_from_file(b"x\n").0, vec![b"x".to_vec()]);
    assert!(!data_path("lines").exists(), "{} left behind", data_path("lines").display());
    assert_eq!(read_lines_csv(b"y\n"), vec![b"y".to_vec()]);
    assert!(!data_path("csvlines").exists(), "{} left behind", data_path("csvlines").display());
}

fn read_lines_from_file(content: &[u8]) -> (Vec<Vec<u8>>, bool) {
    setup_fd();
    let file = data_file("lines", content);

    let mcx = test_ctx().mcx();
    let mut st = mk_state(mcx, b'\t', "\\N");
    let fd = fd::AllocateFile(file.0.to_str().unwrap(), "rb").unwrap();
    drop(file);
    assert!(fd >= 0);
    st.src = CopySrc::File { fd, filename: "" };

    let mut lines = Vec::new();
    let mut saw_marker_eof = false;
    loop {
        let done = st.copy_read_line(false).unwrap();
        if done && st.line_buf.is_empty() {
            saw_marker_eof = true;
            break;
        }
        lines.push(st.line_buf.to_vec());
        if done {
            break;
        }
    }
    fd::FreeFile(fd).unwrap();
    (lines, saw_marker_eof)
}

#[test]
fn copy_read_line_splits_and_honors_end_marker() {
    let (lines, eof) = read_lines_from_file(b"one\ttwo\nthree\n");
    assert_eq!(lines, vec![b"one\ttwo".to_vec(), b"three".to_vec()]);
    assert!(eof);

    let (lines, _) = read_lines_from_file(b"a\n\\.\nignored\n");
    assert_eq!(lines, vec![b"a".to_vec()]);

    // Backslash-newline: the escaped pair is data, the line continues.
    let (lines, _) = read_lines_from_file(b"x\\\ny\n");
    assert_eq!(lines, vec![b"x\\\ny".to_vec()]);

    // A line larger than one refill unit survives buffer refills.
    let big = vec![b'z'; RAW_BUF_SIZE + 1234];
    let mut content = big.clone();
    content.push(b'\n');
    let (lines, _) = read_lines_from_file(&content);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0], big);
}

#[test]
fn crlf_and_cr_line_endings() {
    let (lines, _) = read_lines_from_file(b"a\r\nb\r\n");
    assert_eq!(lines, vec![b"a".to_vec(), b"b".to_vec()]);
    let (lines, _) = read_lines_from_file(b"a\rb\r");
    assert_eq!(lines, vec![b"a".to_vec(), b"b".to_vec()]);
}

fn out_csv(s: &[u8], force_quote: bool, single_attr: bool) -> Vec<u8> {
    let mcx = test_ctx().mcx();
    let mut buf = StringInfo::new_in(mcx).unwrap();
    let mut opts = mk_state(mcx, b',', "").opts;
    opts.csv_mode = true;
    // Callers fold C's pre-conversion null_print comparison into use_quote.
    let use_quote = force_quote || s == opts.null_print.as_bytes();
    crate::to::copy_attribute_out_csv(&mut buf, s, &opts, use_quote, single_attr, None).unwrap();
    buf.as_bytes().to_vec()
}

#[test]
fn attribute_out_csv_matches_c_quoting() {
    assert_eq!(out_csv(b"plain", false, false), b"plain");
    assert_eq!(out_csv(b"a,b", false, false), b"\"a,b\"");
    assert_eq!(out_csv(b"a\"b", false, false), b"\"a\"\"b\"");
    assert_eq!(out_csv(b"a\nb", false, false), b"\"a\nb\"");
    assert_eq!(out_csv(b"a\rb", false, false), b"\"a\rb\"");
    // Empty string matches null_print "" -> forced quoting.
    assert_eq!(out_csv(b"", false, false), b"\"\"");
    assert_eq!(out_csv(b"x", true, false), b"\"x\"");
    // Lone \. is quoted only in single-attribute rows.
    assert_eq!(out_csv(b"\\.", false, true), b"\"\\.\"");
    assert_eq!(out_csv(b"\\.", false, false), b"\\.");
    // Backslashes are not special in CSV.
    assert_eq!(out_csv(b"a\\b", false, false), b"a\\b");
}

fn split_line_csv(line: &[u8], null_print: &'static str) -> Vec<Option<Vec<u8>>> {
    setup_fd();
    let mcx = test_ctx().mcx();
    let mut st = mk_state(mcx, b',', null_print);
    st.opts.csv_mode = true;
    mcx::vec_append_bytes(&mut st.line_buf, line).unwrap();
    let n = st.copy_read_attributes_csv().unwrap();
    fields_of(&st, n)
}

#[test]
fn read_attributes_csv_matches_c() {
    let f = split_line_csv(b"a,b,c", "");
    assert_eq!(
        f,
        vec![Some(b"a".to_vec()), Some(b"b".to_vec()), Some(b"c".to_vec())]
    );

    // Unquoted empty matches null_print ""; quoted empty is an empty string.
    let f = split_line_csv(b"a,,\"\"", "");
    assert_eq!(f, vec![Some(b"a".to_vec()), None, Some(b"".to_vec())]);

    // Doubled quotes de-escape; delimiters inside quotes are data.
    let f = split_line_csv(b"\"a\"\"b\",\"c,d\"", "");
    assert_eq!(f, vec![Some(b"a\"b".to_vec()), Some(b"c,d".to_vec())]);

    // Quoted section adjacent to unquoted data (C allows partial quoting).
    let f = split_line_csv(b"ab\"cd\"ef", "");
    assert_eq!(f, vec![Some(b"abcdef".to_vec())]);

    // NULL marker respected only unquoted.
    let f = split_line_csv(b"NULL,\"NULL\"", "NULL");
    assert_eq!(f, vec![None, Some(b"NULL".to_vec())]);

    let err = {
        setup_fd();
        let mcx = test_ctx().mcx();
        let mut st = mk_state(mcx, b',', "");
        st.opts.csv_mode = true;
        mcx::vec_append_bytes(&mut st.line_buf, b"\"unterminated").unwrap();
        st.copy_read_attributes_csv().unwrap_err()
    };
    assert!(err.message().contains("unterminated CSV quoted field"));
}

#[test]
fn csv_out_then_in_round_trips() {
    let nasty: &[&[u8]] = &[
        b"plain",
        b"comma,here",
        b"quote\"here",
        b"nl\nhere",
        b"cr\rhere",
        b"back\\slash",
        "üñîçødé ⽇本".as_bytes(),
        b"NULL",
        b"",
    ];
    for &case in nasty {
        let encoded = out_csv(case, false, false);
        let fields = split_line_csv(&encoded, "");
        assert_eq!(fields.len(), 1, "case {case:?}");
        assert_eq!(fields[0].as_deref(), Some(case), "case {case:?}");
    }
}

fn read_lines_csv(content: &[u8]) -> Vec<Vec<u8>> {
    setup_fd();
    let file = data_file("csvlines", content);

    let mcx = test_ctx().mcx();
    let mut st = mk_state(mcx, b',', "");
    st.opts.csv_mode = true;
    let fd = fd::AllocateFile(file.0.to_str().unwrap(), "rb").unwrap();
    drop(file);
    st.src = CopySrc::File { fd, filename: "" };

    let mut lines = Vec::new();
    loop {
        let done = st.copy_read_line(true).unwrap();
        if done && st.line_buf.is_empty() {
            break;
        }
        lines.push(st.line_buf.to_vec());
        if done {
            break;
        }
    }
    fd::FreeFile(fd).unwrap();
    lines
}

#[test]
fn csv_read_line_keeps_quoted_newlines() {
    let lines = read_lines_csv(b"a,\"x\ny\"\nb,c\n");
    assert_eq!(lines, vec![b"a,\"x\ny\"".to_vec(), b"b,c".to_vec()]);
    // \. is not an end-of-copy marker in CSV mode (PG 18 semantics).
    let lines = read_lines_csv(b"\\.\nafter\n");
    assert_eq!(lines, vec![b"\\.".to_vec(), b"after".to_vec()]);
}

// ---- ProcessCopyOptions non-String option arguments (defGetString /
// defGetInt64 arms, define.c) ----

fn defelem<'m>(
    mcx: Mcx<'m>,
    name: &'m str,
    arg: Option<types_nodes::Node<'m>>,
) -> types_nodes::Node<'m> {
    types_nodes::Node::mk(
        mcx,
        types_nodes::parsenodes::DefElem {
            defname: Some(name),
            arg,
            location: -1,
            ..Default::default()
        },
    )
    .unwrap()
}

fn opt_list<'m>(mcx: Mcx<'m>, elems: &[types_nodes::Node<'m>]) -> types_nodes::NodeList<'m> {
    let mut l = types_nodes::NodeList::nil();
    for &e in elems {
        l.lappend(mcx, e).unwrap();
    }
    l
}

// COPY t TO f WITH (delimiter 1): C's defGetString renders the Integer as
// "1" and the single-byte checks then reject it as a catchable error, not a
// panic (delimiter "1" is in C's banned \\.a-z0-9 set).
#[test]
fn integer_arg_to_string_option_is_catchable() {
    let mcx = test_ctx().mcx();
    let arg = types_nodes::Node::mk(mcx, types_nodes::Integer { ival: 1 }).unwrap();
    let opts = opt_list(mcx, &[defelem(mcx, "delimiter", Some(arg))]);
    let Err(e) = crate::ProcessCopyOptions(mcx, false, &opts, None) else {
        panic!("expected error")
    };
    assert_eq!(e.message(), "COPY delimiter cannot be \"1\"");
}

// WITH (format 5): Integer through defGetString reaches the format
// dispatch with "5".
#[test]
fn integer_arg_to_format_option() {
    let mcx = test_ctx().mcx();
    let arg = types_nodes::Node::mk(mcx, types_nodes::Integer { ival: 5 }).unwrap();
    let opts = opt_list(mcx, &[defelem(mcx, "format", Some(arg))]);
    let Err(e) = crate::ProcessCopyOptions(mcx, false, &opts, None) else {
        panic!("expected error")
    };
    assert_eq!(e.message(), "COPY format \"5\" not recognized");
}

// WITH (delimiter (a, b)): a List argument goes through NameListToString,
// yielding the dot-joined "a.b" C produces, then the length check fires.
#[test]
fn list_arg_to_string_option_matches_c() {
    let mcx = test_ctx().mcx();
    let a = types_nodes::Node::mk(mcx, types_nodes::String { sval: "a" }).unwrap();
    let b = types_nodes::Node::mk(mcx, types_nodes::String { sval: "b" }).unwrap();
    let list = types_nodes::Node::mk_list(mcx, opt_list(mcx, &[a, b])).unwrap();
    let opts = opt_list(mcx, &[defelem(mcx, "delimiter", Some(list))]);
    let Err(e) = crate::ProcessCopyOptions(mcx, false, &opts, None) else {
        panic!("expected error")
    };
    assert_eq!(e.message(), "COPY delimiter must be a single one-byte character");
}

// WITH (freeze (on)): C's defGetBoolean falls through to defGetString,
// which renders the single-element list as "on" -> true.
#[test]
fn single_element_list_boolean_quirk() {
    let mcx = test_ctx().mcx();
    let on = types_nodes::Node::mk(mcx, types_nodes::String { sval: "on" }).unwrap();
    let list = types_nodes::Node::mk_list(mcx, opt_list(mcx, &[on])).unwrap();
    let opts = opt_list(mcx, &[defelem(mcx, "freeze", Some(list))]);
    let parsed = crate::ProcessCopyOptions(mcx, true, &opts, None).unwrap();
    assert!(parsed.freeze);
}

// REJECT_LIMIT with a lexer Float: defGetInt64's int8in arm accepts values
// beyond int4 (C copy.c defGetCopyRejectLimitOption).
#[test]
fn reject_limit_float_arg_via_defgetint64() {
    let mcx = test_ctx().mcx();
    let ignore = types_nodes::Node::mk(mcx, types_nodes::String { sval: "ignore" }).unwrap();
    let big = types_nodes::Node::mk(mcx, types_nodes::Float { fval: "3000000000" }).unwrap();
    let opts = opt_list(
        mcx,
        &[
            defelem(mcx, "on_error", Some(ignore)),
            defelem(mcx, "reject_limit", Some(big)),
        ],
    );
    let parsed = crate::ProcessCopyOptions(mcx, true, &opts, None).unwrap();
    assert_eq!(parsed.reject_limit, 3_000_000_000);
}

// REJECT_LIMIT with a Boolean argument: defGetInt64's default arm raises
// C's catchable "requires a numeric value" syntax error.
#[test]
fn reject_limit_boolean_arg_is_catchable() {
    let mcx = test_ctx().mcx();
    let ignore = types_nodes::Node::mk(mcx, types_nodes::String { sval: "ignore" }).unwrap();
    let b = types_nodes::Node::mk(mcx, types_nodes::Boolean { boolval: true }).unwrap();
    let opts = opt_list(
        mcx,
        &[
            defelem(mcx, "on_error", Some(ignore)),
            defelem(mcx, "reject_limit", Some(b)),
        ],
    );
    let Err(e) = crate::ProcessCopyOptions(mcx, true, &opts, None) else {
        panic!("expected error")
    };
    assert_eq!(e.message(), "reject_limit requires a numeric value");
}

// FORMAT binary + HEADER is FEATURE_NOT_SUPPORTED; DELIMITER/NULL/DEFAULT
// in binary stay SYNTAX_ERROR.
#[test]
fn binary_header_is_feature_not_supported() {
    let mcx = test_ctx().mcx();
    let fmt = types_nodes::Node::mk(mcx, types_nodes::String { sval: "binary" }).unwrap();
    let hdr = types_nodes::Node::mk(mcx, types_nodes::Boolean { boolval: true }).unwrap();
    let opts = opt_list(
        mcx,
        &[
            defelem(mcx, "format", Some(fmt)),
            defelem(mcx, "header", Some(hdr)),
        ],
    );
    let Err(e) = crate::ProcessCopyOptions(mcx, false, &opts, None) else {
        panic!("expected error")
    };
    assert_eq!(e.message(), "cannot specify HEADER in BINARY mode");
    assert_eq!(e.sqlstate(), types_error::ERRCODE_FEATURE_NOT_SUPPORTED);
}

// ---- client-only-encoding escape walks (CopyAttributeOutText/CSV,
// encoding_embeds_ascii arm) ----

// SJIS 0x95 0x5c is a two-byte character whose trail byte is ASCII '\':
// the embedded walk must NOT escape it, while the plain walk would.
#[test]
fn text_walk_skips_ascii_trail_bytes_under_sjis() {
    let mcx = test_ctx().mcx();
    let s = b"a\x95\x5cb";
    let mut buf = StringInfo::new_in(mcx).unwrap();
    crate::to::copy_attribute_out_text_embedded(&mut buf, s, b'\t', wchar::PG_SJIS).unwrap();
    assert_eq!(buf.as_bytes(), b"a\x95\x5cb");
    // The safe-encoding walk escapes the embedded backslash byte.
    assert_eq!(out_text(s, b'\t'), b"a\x95\\\\b");
    // A real ASCII backslash after a complete SJIS character still escapes.
    let mut buf2 = StringInfo::new_in(mcx).unwrap();
    crate::to::copy_attribute_out_text_embedded(&mut buf2, b"\x95\x5c\\x", b'\t', wchar::PG_SJIS)
        .unwrap();
    assert_eq!(buf2.as_bytes(), b"\x95\x5c\\\\x");
}

// Same trail-byte blindness for the CSV quote walk: an SJIS character whose
// trail byte equals the quote must be neither doubled nor trigger quoting.
#[test]
fn csv_walk_skips_ascii_trail_bytes_under_sjis() {
    let mcx = test_ctx().mcx();
    // 0x81 0x22: SJIS two-byte character with '"' as trail byte.
    let s = b"a\x81\x22b";
    let mut opts = mk_state(mcx, b',', "").opts;
    opts.csv_mode = true;
    let mut buf = StringInfo::new_in(mcx).unwrap();
    crate::to::copy_attribute_out_csv(&mut buf, s, &opts, false, false, Some(wchar::PG_SJIS))
        .unwrap();
    assert_eq!(buf.as_bytes(), b"a\x81\x22b");
    // The safe walk sees the '"' byte and quotes + doubles it.
    let mut buf2 = StringInfo::new_in(mcx).unwrap();
    crate::to::copy_attribute_out_csv(&mut buf2, s, &opts, false, false, None).unwrap();
    assert_eq!(buf2.as_bytes(), b"\"a\x81\"\"b\"");
}

// copy-1 (audit-18.6): copyfrom.c:1881 / copyto.c:971 guard the HINT on
// save_errno == ENOENT || save_errno == EACCES.
#[test]
fn open_failure_hint_only_for_enoent_and_eacces() {
    assert!(crate::open_failure_hint_applies(libc::ENOENT));
    assert!(crate::open_failure_hint_applies(libc::EACCES));
    for e in [libc::EISDIR, libc::ENOTDIR, libc::EROFS, libc::EOPNOTSUPP, libc::EPERM, 0] {
        assert!(!crate::open_failure_hint_applies(e), "errno {e} must not carry the HINT");
    }
}

// ---- audit-18.6 remediation b016: the parquet extension's option surface
// must not leak non-C error shapes where the extension is not served
// (copy.c:574 format arm, copy.c:721 unrecognized-option arm). ----

fn defelem_at<'m>(
    mcx: Mcx<'m>,
    name: &'m str,
    sval: &'m str,
    location: i32,
) -> types_nodes::Node<'m> {
    let arg = types_nodes::Node::mk(mcx, types_nodes::String { sval }).unwrap();
    types_nodes::Node::mk(
        mcx,
        types_nodes::parsenodes::DefElem { defname: Some(name), arg: Some(arg), location, ..Default::default() },
    )
    .unwrap()
}

// COPY t TO STDOUT WITH (format parquet): C has no such format — 22023
// 'COPY format "parquet" not recognized' with the cursor on the option.
#[test]
fn parquet_format_on_copy_to_is_not_recognized_like_c() {
    setup_fd();
    let mcx = test_ctx().mcx();
    let src = "COPY t TO STDOUT WITH (format parquet)";
    let opts = opt_list(mcx, &[defelem_at(mcx, "format", "parquet", 23)]);
    let Err(e) = crate::ProcessCopyOptions(mcx, false, &opts, Some(src)) else {
        panic!("expected error")
    };
    assert_eq!(e.message(), "COPY format \"parquet\" not recognized");
    assert_eq!(e.sqlstate(), types_error::ERRCODE_INVALID_PARAMETER_VALUE);
    assert_eq!(e.cursor_position(), Some(24));
}

// COPY (SELECT 1) TO STDOUT WITH (match_by position): outside a parquet load
// the companion option is, as in C, an unrecognized option — 42601 with the
// cursor on it, not a 0A000 "requires parquet format".
#[test]
fn match_by_without_parquet_is_unrecognized_option_like_c() {
    setup_fd();
    let mcx = test_ctx().mcx();
    let src = "COPY (SELECT 1) TO STDOUT WITH (match_by position)";
    let opts = opt_list(mcx, &[defelem_at(mcx, "match_by", "position", 32)]);
    let Err(e) = crate::ProcessCopyOptions(mcx, false, &opts, Some(src)) else {
        panic!("expected error")
    };
    assert_eq!(e.message(), "option \"match_by\" not recognized");
    assert_eq!(e.sqlstate(), types_error::ERRCODE_SYNTAX_ERROR);
    assert_eq!(e.cursor_position(), Some(33));
}

// COPY t FROM STDIN WITH (format csv, coerce_epoch true): same on the FROM
// side under a non-parquet format, at the option's own position.
#[test]
fn coerce_epoch_without_parquet_is_unrecognized_option_like_c() {
    setup_fd();
    let mcx = test_ctx().mcx();
    let src = "COPY t FROM STDIN WITH (format csv, coerce_epoch true)";
    let opts = opt_list(
        mcx,
        &[defelem_at(mcx, "format", "csv", 24), defelem_at(mcx, "coerce_epoch", "true", 36)],
    );
    let Err(e) = crate::ProcessCopyOptions(mcx, true, &opts, Some(src)) else {
        panic!("expected error")
    };
    assert_eq!(e.message(), "option \"coerce_epoch\" not recognized");
    assert_eq!(e.sqlstate(), types_error::ERRCODE_SYNTAX_ERROR);
    assert_eq!(e.cursor_position(), Some(37));
}

// Control: the extension itself still parses on the FROM side, in either
// option order.
#[test]
fn parquet_from_with_companion_options_still_parses() {
    setup_fd();
    let mcx = test_ctx().mcx();
    for order in [["format", "match_by"], ["match_by", "format"]] {
        let elems: Vec<_> = order
            .iter()
            .map(|n| match *n {
                "format" => defelem_at(mcx, "format", "parquet", 0),
                _ => defelem_at(mcx, "match_by", "name", 0),
            })
            .collect();
        let opts = opt_list(mcx, &elems);
        let parsed = crate::ProcessCopyOptions(mcx, true, &opts, None).unwrap();
        assert!(parsed.parquet && parsed.parquet_match_by_name, "order {order:?}");
    }
}

// copyfrom.c:1886 / copyto.c:976: an fstat failure on the opened COPY file
// is 'could not stat file "%s": %m' under errcode_for_file_access (58030 for
// EIO), never a silent "not a directory" fallback.
#[test]
fn could_not_stat_file_is_c_shaped() {
    setup_fd();
    let e = crate::could_not_stat_file("/x/y", &std::io::Error::from_raw_os_error(libc::EIO));
    assert_eq!(e.message(), "could not stat file \"/x/y\": Input/output error");
    assert_eq!(e.sqlstate(), types_error::ERRCODE_IO_ERROR);
}

// copy.c:574 for the PROGRAM / STDIN sources of a parquet load: C's format
// error at the format option, raised in option order before any later
// option's parquet-specific refusal.
#[test]
fn parquet_format_refusal_points_at_the_format_option() {
    setup_fd();
    let mcx = test_ctx().mcx();
    let src = "COPY t FROM PROGRAM 'echo 1' WITH (format parquet, delimiter ',')";
    let opts = opt_list(
        mcx,
        &[defelem_at(mcx, "format", "parquet", 35), defelem_at(mcx, "delimiter", ",", 51)],
    );
    let Err(e) = crate::process_copy_options(mcx, true, false, &opts, Some(src)) else {
        panic!("parquet accepted for a non-file source");
    };
    assert_eq!(e.message(), "COPY format \"parquet\" not recognized");
    assert_eq!(e.sqlstate(), types_error::ERRCODE_INVALID_PARAMETER_VALUE);
    assert_eq!(e.cursor_position(), Some(36));
    let Err(e) = crate::process_copy_options(mcx, true, true, &opts, Some(src)) else {
        panic!("delimiter accepted with parquet");
    };
    assert_eq!(e.message(), "cannot specify DELIMITER with parquet format");
}

#[test]
fn binary_empty_field_does_not_read_source() {
    let mcx = test_ctx().mcx();
    for eof in [false, true] {
        let mut st = mk_state(mcx, b'\t', "");
        st.opts.binary = true;
        st.raw_reached_eof = eof;
        st.src = CopySrc::Callback { cb: Box::new(|_, _| panic!("unexpected read")) };
        st.binary_attr_buf.append_bytes(b"previous field").unwrap();
        st.binary_attr_buf.cursor = 3;
        st.read_binary_attr_data(0).unwrap();
        assert!(st.binary_attr_buf.as_bytes().is_empty());
        assert_eq!(st.binary_attr_buf.cursor, 0);
    }
}

#[test]
fn binary_oversize_field_is_rejected_before_reading() {
    let mcx = test_ctx().mcx();
    for size in [0x3fff_ffff, 0x4000_0000, i32::MAX as usize] {
        let mut st = mk_state(mcx, b'\t', "");
        st.opts.binary = true;
        st.src = CopySrc::Callback { cb: Box::new(|_, _| panic!("unexpected read")) };
        let error = st.read_binary_attr_data(size).unwrap_err();
        assert_eq!(error.sqlstate(), types_error::ERRCODE_PROGRAM_LIMIT_EXCEEDED);
    }
}

#[test]
fn binary_field_handles_fragmentation_and_truncation() {
    setup_fd();
    let mcx = test_ctx().mcx();
    for (size, payload) in [(1, b"x".as_slice()), (3, b"abc".as_slice()), (4, b"abc".as_slice())] {
        let mut st = mk_state(mcx, b'\t', "");
        st.opts.binary = true;
        let mut offset = 0;
        st.src = CopySrc::Callback { cb: Box::new(move |dst, _| {
            if offset == payload.len() { return Ok(0); }
            dst[0] = payload[offset];
            offset += 1;
            Ok(1)
        }) };
        let result = st.read_binary_attr_data(size);
        if size == payload.len() {
            result.unwrap();
            assert_eq!(st.binary_attr_buf.as_bytes(), payload);
            st.read_binary_attr_data(0).unwrap();
        } else {
            assert_eq!(result.unwrap_err().sqlstate(), types_error::ERRCODE_BAD_COPY_FILE_FORMAT);
        }
    }
}

// Lane dop (2026-09-15): the parquet driver's DOP and decode budget derive from the machine.
#[test]
fn parquet_dop_is_the_pool_width_never_clamped_at_32() {
    use crate::parallel::dop_for;
    assert_eq!(dop_for(None, 16), 16);
    assert_eq!(dop_for(None, 96), 96, "the v1 min(…, 32) clamp is gone");
    assert_eq!(dop_for(None, 192), 192);
    assert_eq!(dop_for(Some(128), 192), 128, "the knob is honoured above 32");
    assert_eq!(dop_for(Some(0), 16), 1);
    assert_eq!(dop_for(Some(10_000), 192), runtime::MAX_EXTERNAL_LANES as u64);
}

#[test]
fn parquet_budget_is_legacy_on_16_cores_and_a_sixteenth_of_headroom_above() {
    use crate::parallel::{parquet_budget_for, LEGACY_PARQUET_BUDGET_BYTES};
    let gb = |n: u64| n << 30;
    // the 16-core class keeps the memory-safe 2048 MB whatever the headroom
    assert_eq!(parquet_budget_for(16, Some(gb(27))), LEGACY_PARQUET_BUDGET_BYTES);
    assert_eq!(parquet_budget_for(8, Some(gb(352))), LEGACY_PARQUET_BUDGET_BYTES);
    // a wider pool takes a sixteenth of the headroom (the metal: 22 GB, window ≈ 38 on hits)
    assert_eq!(parquet_budget_for(192, Some(gb(352))), gb(22));
    assert_eq!(parquet_budget_for(96, Some(gb(352))), gb(22));
    // never below the legacy value, and legacy when the headroom is unknown
    assert_eq!(parquet_budget_for(192, Some(gb(16))), LEGACY_PARQUET_BUDGET_BYTES);
    assert_eq!(parquet_budget_for(192, None), LEGACY_PARQUET_BUDGET_BYTES);
}

// Parallel text COPY (textseg.rs): the leader's cut points and preset line
// numbers must make each chunk's serial reader produce exactly the rows,
// line numbers and first error of one serial reader over the whole input.
#[derive(Debug, PartialEq)]
enum RawRow {
    Row(u64, Vec<Option<Vec<u8>>>),
    Err(u64, String),
}

fn raw_rows(st: &mut CopyFromState<'_, '_>, csv: bool, header: bool) -> Vec<RawRow> {
    let mut out = Vec::new();
    loop {
        if st.cur_lineno == 0 && header {
            st.cur_lineno += 1;
            match st.copy_read_line(csv) {
                Err(e) => {
                    out.push(RawRow::Err(st.cur_lineno, e.message().to_string()));
                    return out;
                }
                Ok(true) => return out,
                Ok(false) => {}
            }
        }
        st.cur_lineno += 1;
        let done = match st.copy_read_line(csv) {
            Ok(d) => d,
            Err(e) => {
                out.push(RawRow::Err(st.cur_lineno, e.message().to_string()));
                return out;
            }
        };
        if done && st.line_buf.is_empty() {
            return out;
        }
        let n = if csv { st.copy_read_attributes_csv() } else { st.copy_read_attributes_text() };
        match n {
            Ok(n) => out.push(RawRow::Row(st.cur_lineno, fields_of(st, n))),
            Err(e) => {
                out.push(RawRow::Err(st.cur_lineno, e.message().to_string()));
                return out;
            }
        }
    }
}

fn seg_state<'mcx>(mcx: Mcx<'mcx>, csv: bool, escape: u8, header: bool) -> CopyFromState<'mcx, 'static> {
    let mut st = mk_state(mcx, if csv { b',' } else { b'\t' }, if csv { "" } else { "\\N" });
    st.opts.csv_mode = csv;
    st.opts.escape = escape;
    if header {
        st.opts.header_line = crate::CopyHeaderChoice::True;
    }
    st
}

fn seg_vs_serial(input: &[u8], csv: bool, escape: u8, header: bool) {
    use crate::textseg::{ChunkCursor, SegOpts, Segmentator};
    use std::sync::Arc;
    setup_fd();
    mbutils::SetDatabaseEncoding(wchar::PG_UTF8).unwrap();
    let mcx = test_ctx().mcx();
    let mut st = seg_state(mcx, csv, escape, header);
    let owned = input.to_vec();
    let mut pos = 0usize;
    st.src = CopySrc::Callback {
        cb: Box::new(move |dst, _| {
            let n = (owned.len() - pos).min(dst.len()).min(5);
            dst[..n].copy_from_slice(&owned[pos..pos + n]);
            pos += n;
            Ok(n)
        }),
    };
    let serial = raw_rows(&mut st, csv, header);
    for (grain, grains) in [(1u64, 1u64), (2, 1), (2, 3)] {
        for bs in [1usize, 2, 3, 7, input.len().max(1)] {
            let o = SegOpts {
                csv,
                quote: b'"',
                escape,
                header,
                grain,
                grains,
                target_bytes: u64::MAX,
                max_line_bytes: 1 << 30,
            };
            let mut seg = Segmentator::new(o);
            let mut descs = Vec::new();
            for b in input.chunks(bs) {
                seg.feed(&Arc::new(crate::textseg::seg_block(b)), &mut descs).unwrap();
                if seg.input_complete() {
                    break;
                }
            }
            seg.finish(&mut descs);
            let mut par = Vec::new();
            let n = descs.len();
            for (i, d) in descs.into_iter().enumerate() {
                assert_eq!(d.last, i + 1 == n);
                if !d.last {
                    assert_eq!(d.rows % grain, 0, "non-final chunk {i} is not whole grains");
                }
                let mut cst = seg_state(mcx, csv, escape, header);
                cst.src = CopySrc::Chunk { cur: ChunkCursor::new(d.segs, None), parked: None };
                cst.cur_lineno = d.lineno_before;
                cst.eol_type = d.eol_at_start;
                let rows = raw_rows(&mut cst, csv, header);
                let errored = rows.iter().any(|r| matches!(r, RawRow::Err(..)));
                if !errored {
                    assert_eq!(rows.len() as u64, d.rows, "chunk {i} row count (grain {grain} bs {bs})");
                }
                par.extend(rows);
                if errored {
                    break;
                }
            }
            assert_eq!(
                par, serial,
                "input {:?} csv {csv} grain {grain}x{grains} block {bs}",
                String::from_utf8_lossy(input)
            );
        }
    }
}

#[test]
fn segmentation_matches_the_serial_reader_text() {
    let cases: &[&[u8]] = &[
        b"a\tb\nc\td\ne\tf\ng\th\ni\tj\n",
        b"a\nb\nc\nd\ne",
        b"x\\\ny\nz\n\\\\\nw\\\rv\nlast\n",
        b"a\nb\n\\.\nignored\nmore\n",
        b"a\nb\nc\n\\.",
        b"a\nb\nc\n\\.x\nd\n",
        b"a\nb\nab\\.\nc\n",
        b"a\r\nb\r\nc\r\nd\r\ne\r\n",
        b"a\rb\rc\rd\r",
        b"a\r\nb\nc\r\nd\n",
        b"a\nb\r\nc\n",
        b"a\rb\nc\r",
        b"\n\n\na\n\n",
        b"",
        b"only",
        b"a\nb\nc\r",
    ];
    for c in cases {
        seg_vs_serial(c, false, b'"', false);
        seg_vs_serial(c, false, b'"', true);
    }
}

#[test]
fn segmentation_matches_the_serial_reader_csv() {
    let cases: &[&[u8]] = &[
        b"a,1\n\"b\nb\",2\nc,3\nd,4\ne,5\n\"f\n\nf\",6\ng,7\nh,8\ni,9\n",
        b"\"x\ny\",1\na,2\nb,3\nc,4\n",
        b"\"x\ry\",1\r\na,2\r\n\"q\r\nq\",3\r\nb,4\r\n",
        b"\"a\"\"b\",1\n\"\"\"\",2\n\"\",3\n,4\n",
        b"a,1\n\"unterminated\n,2\nb,3\n",
        b"a,1\nb,2\r\nc,3\n",
        b"a\rb\r\"c\rd\"\re\r",
        b"h1,h2\n1,2\n3,4\n5,6\n",
        b"a,\"b\\\"\nc\",1\nd,2\ne,3\n",
        b"\\.\n",
        b"a\n\\.\nb\n",
        b"",
        b"x,y",
    ];
    for c in cases {
        seg_vs_serial(c, true, b'"', false);
        seg_vs_serial(c, true, b'"', true);
        seg_vs_serial(c, true, b'\\', false);
    }
}

// After a soft error C's cur_attname/cur_attval stay set (copyfromparse.c
// NextCopyFrom soft-error return), so a line-level error on the NEXT line
// reports the previous column, with whatever attribute_buf holds at the old
// offset: the new line's bytes, or the old ones once a shorter line was parsed.
#[test]
fn stale_column_context_survives_the_next_line() {
    setup_fd();
    mbutils::SetDatabaseEncoding(wchar::PG_UTF8).unwrap();
    let mcx = test_ctx().mcx();
    let mut st = mk_state(mcx, b'\t', "\\N");
    for n in ["a", "b"] {
        let mut nd = types_tuple::NameData::default();
        nd.namestrcpy(n);
        st.attnames.push(nd);
    }
    st.relname = "t3".into();
    st.line_buf_valid = true;

    mcx::vec_append_bytes(&mut st.line_buf, b"1\tabc").unwrap();
    st.copy_read_attributes_text().unwrap();
    st.cur_lineno = 1;
    st.cur_attidx = Some(1);
    st.cur_attval_off = Some(st.raw_fields[1]);

    st.line_buf.clear();
    mcx::vec_append_bytes(&mut st.line_buf, b"2\t3\t4").unwrap();
    st.copy_read_attributes_text().unwrap();
    st.cur_lineno = 2;
    let e = copy_from_error_context(&st, crate::fromparse::extra_data());
    assert_eq!(e.context(), Some("COPY t3, line 2, column b: \"3\""));

    st.line_buf.clear();
    mcx::vec_append_bytes(&mut st.line_buf, b"2").unwrap();
    st.copy_read_attributes_text().unwrap();
    st.cur_lineno = 3;
    let e = copy_from_error_context(&st, crate::fromparse::extra_data());
    assert_eq!(e.context(), Some("COPY t3, line 3, column b: \"3\""));

    st.cur_attidx = None;
    st.cur_attval_off = None;
    let e = copy_from_error_context(&st, crate::fromparse::extra_data());
    assert_eq!(e.context(), Some("COPY t3, line 3: \"2\""));
}

// fp-contrib-file_fdw-file_fdw#5: file_fdw.c:789 checks for interrupts with
// CopyFromErrorCallback still installed, so a cancel there carries the COPY
// relation and line.
#[test]
fn interrupt_between_soft_errors_carries_the_copy_context() {
    setup_fd();
    let mcx = test_ctx().mcx();
    let mut st = mk_state(mcx, b'\t', "\\N");
    st.relname = "t5".into();
    st.cur_lineno = 4;
    assert!(crate::from::check_for_interrupts_in_copy(&st).is_ok());
    INTERRUPT_PENDING.store(true, std::sync::atomic::Ordering::SeqCst);
    let e = crate::from::check_for_interrupts_in_copy(&st).unwrap_err();
    INTERRUPT_PENDING.store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(e.sqlstate(), types_error::ERRCODE_QUERY_CANCELED);
    assert_eq!(e.message(), "canceling statement due to user request");
    assert_eq!(e.context(), Some("COPY t5, line 4"));
}

// copyfrom.c:258-262 + 466-470: a foreign batch flush (here the one after the
// input ended, cursor past the last line) reports only the relation (wio P225).
#[test]
fn foreign_batch_flush_context_names_only_the_relation() {
    setup_fd();
    let mcx = test_ctx().mcx();
    let mut st = mk_state(mcx, b'\t', "\\N");
    st.relname = "rem2".into();
    st.line_buf_valid = true;
    st.cur_lineno = 2;
    let e = copy_from_error_context(&st, crate::fromparse::extra_data());
    assert_eq!(e.context(), Some("COPY rem2, line 2: \"\""));
    st.relname_only = true;
    let e = copy_from_error_context(&st, crate::fromparse::extra_data());
    assert_eq!(e.context(), Some("COPY rem2"));
}

// enlargeStringInfo (stringinfo.c) for COPY FROM's line and attribute
// buffers: 54000 with C's DETAIL at MaxAllocSize, and growth past a 512 MB
// buffer clamps to MaxAllocSize instead of requesting 1 GB.
#[test]
fn copy_buffers_grow_like_stringinfo() {
    let e = crate::fromparse::stringinfo_check(1_073_676_288, 65_536).unwrap_err();
    assert_eq!(e.sqlstate(), types_error::ERRCODE_PROGRAM_LIMIT_EXCEEDED);
    assert_eq!(e.message(), "string buffer exceeds maximum allowed length (1073741823 bytes)");
    assert_eq!(
        e.detail(),
        Some("Cannot enlarge string buffer containing 1073676288 bytes by 65536 more bytes.")
    );
    assert!(crate::fromparse::stringinfo_check(0, mcx::MAX_ALLOC_SIZE - 1).is_ok());
    assert!(crate::fromparse::stringinfo_check(1, mcx::MAX_ALLOC_SIZE - 1).is_err());
    assert_eq!(mcx::grow_target(536_870_912, 536_870_913), mcx::MAX_ALLOC_SIZE);
    let mcx = test_ctx().mcx();
    let mut v: PgVec<'_, u8> = PgVec::new_in(mcx);
    crate::fromparse::stringinfo_append(&mut v, b"abc").unwrap();
    crate::fromparse::stringinfo_reserve(&mut v, 100).unwrap();
    assert!(v.capacity() >= 100);
    assert!(crate::fromparse::stringinfo_reserve(&mut v, mcx::MAX_ALLOC_SIZE + 1).is_err());
}
