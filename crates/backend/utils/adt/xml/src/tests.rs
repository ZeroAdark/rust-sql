use crate::*;

fn decl(s: &[u8]) -> (i32, usize, Option<Vec<u8>>, Option<Vec<u8>>, i32) {
    let mut work = s.to_vec();
    work.push(0);
    let mut len = 0usize;
    let mut version = None;
    let mut encoding = None;
    let mut standalone = 0i32;
    let rc = parse_xml_decl(
        &work,
        Some(&mut len),
        Some(&mut version),
        Some(&mut encoding),
        Some(&mut standalone),
    )
    .unwrap();
    (rc, len, version, encoding, standalone)
}

#[test]
fn parse_xml_decl_full() {
    let (rc, len, version, encoding, standalone) =
        decl(b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><a/>");
    assert_eq!(rc, 0);
    assert_eq!(&decl(b"<?xml version=\"1.0\"?>x").2.unwrap(), b"1.0");
    assert_eq!(version.as_deref(), Some(b"1.0".as_slice()));
    assert_eq!(encoding.as_deref(), Some(b"UTF-8".as_slice()));
    assert_eq!(standalone, 1);
    assert_eq!(len, 55);
}

#[test]
fn parse_xml_decl_absent_and_pi() {
    let (rc, len, version, _, standalone) = decl(b"<a/>");
    assert_eq!((rc, len, standalone), (0, 0, -1));
    assert!(version.is_none());
    // <?xmlfoo ...?> is a PI, not a declaration.
    let (rc, len, ..) = decl(b"<?xmlfoo bar?><a/>");
    assert_eq!((rc, len), (0, 0));
    // PG_XMLISNAMECHAR accepts every codepoint >= 0xF8 (xmlIsBaseChar_ch's
    // last alternative), so a Greek letter also names a PI.
    let (rc, len, ..) = decl("<?xml\u{3b1}?><a/>".as_bytes());
    assert_eq!((rc, len), (0, 0));
}

// xmlStrchr stops at NUL: a version quote never closed before an embedded
// NUL (reachable through xml_recv) leaves the declaration unterminated and
// the encoding absent, instead of reading an encoding from past the NUL.
#[test]
fn parse_xml_decl_quote_search_stops_at_nul() {
    let (rc, _, version, encoding, _) =
        decl(b"<?xml version='1.0\0' encoding='bogus'?><a/>");
    assert_eq!(rc, XML_ERR_VERSION_MISSING);
    assert!(version.is_none());
    assert!(encoding.is_none());
    let (rc, _, _, encoding, _) =
        decl(b"<?xml version='1.0' encoding='bo\0gus'?><a/>");
    assert_eq!(rc, XML_ERR_MISSING_ENCODING);
    assert!(encoding.is_none());
}

#[test]
fn xpath_numbers_print_like_float8out() {
    let eval = |expr: &[u8]| {
        let mut out = Vec::new();
        assert_eq!(crate::xpath::xpath_internal(expr, b"<r/>", None, Some(&mut out)).unwrap(), 1);
        String::from_utf8(out.remove(0)).unwrap()
    };
    adt_float::set_extra_float_digits(1);
    assert_eq!(eval(b"100000000000000000000"), "1e+20");
    assert_eq!(eval(b"1000000000000000"), "1e+15");
    assert_eq!(eval(b"1 div 3"), "0.3333333333333333");
    assert_eq!(eval(b"1 div 0"), "Infinity");
    adt_float::set_extra_float_digits(0);
    assert_eq!(eval(b"1 div 3"), "0.333333333333333");
    assert_eq!(eval(b"10000000000000000"), "1e+16");
    adt_float::set_extra_float_digits(1);
}

#[test]
fn parse_xml_decl_errors() {
    assert_eq!(decl(b"<?xml?>").0, 65); // space required
    assert_eq!(decl(b"<?xml standalone=\"yes\"?>").0, 96); // version missing
    assert_eq!(decl(b"<?xml version=\"1.0\" standalone=\"maybe\"?>").0, 78);
    assert_eq!(decl(b"<?xml version=\"1.0\" encoding?>").0, 101);
    assert_eq!(decl(b"<?xml version=\"1.0\" ").0, 57);
}

#[test]
fn print_xml_decl_forms() {
    let mut buf = Vec::new();
    assert!(!print_xml_decl(&mut buf, Some(b"1.0"), 0, -1));
    assert!(buf.is_empty());
    assert!(print_xml_decl(&mut buf, Some(b"1.1"), 0, -1));
    assert_eq!(buf, b"<?xml version=\"1.1\"?>");
    buf.clear();
    assert!(print_xml_decl(&mut buf, None, 0, 0));
    assert_eq!(buf, b"<?xml version=\"1.0\" standalone=\"no\"?>");
}

#[test]
fn doctype_in_content_forms() {
    assert!(xml_doctype_in_content(b"<!DOCTYPE a><a/>\0"));
    assert!(xml_doctype_in_content(b"  <!-- c --> <?pi x?> <!DOCTYPE a><a/>\0"));
    assert!(!xml_doctype_in_content(b"<a/>\0"));
    assert!(!xml_doctype_in_content(b"text\0"));
}

#[test]
fn escape_xml_specials() {
    assert_eq!(escape_xml(b"a<b>&c\rd"), b"a&lt;b&gt;&amp;c&#x0d;d".to_vec());
}

#[test]
fn xmlcomment_validation() {
    assert_eq!(xmlcomment(b"hello").unwrap(), b"<!--hello-->".to_vec());
    assert!(xmlcomment(b"a--b").is_err());
    assert!(xmlcomment(b"tail-").is_err());
    assert_eq!(xmlcomment(b"").unwrap(), b"<!---->".to_vec());
}

#[test]
fn xmlpi_validation() {
    assert!(xmlpi("xml", None).is_err());
    assert!(xmlpi("XmL", None).is_err());
    assert_eq!(xmlpi("php", None).unwrap(), None);
    assert!(xmlpi("php", Some(b"echo ?>")).is_err());
    assert_eq!(
        xmlpi("php", Some(b"  echo 'x';")).unwrap().unwrap(),
        b"<?php echo 'x';?>".to_vec()
    );
}

#[test]
fn xmlroot_rewrites_decl() {
    let out = xmlroot(
        b"<?xml version=\"1.0\"?><a/>",
        Some(b"1.1"),
        XmlStandaloneType::XML_STANDALONE_YES,
    )
    .unwrap();
    assert_eq!(out, b"<?xml version=\"1.1\" standalone=\"yes\"?><a/>".to_vec());
    let out = xmlroot(b"<a/>", None, XmlStandaloneType::XML_STANDALONE_OMITTED).unwrap();
    assert_eq!(out, b"<a/>".to_vec());
}

#[test]
fn map_identifiers_round() {
    assert_eq!(
        map_sql_identifier_to_xml_name(b"foo_xk", true, true).unwrap(),
        b"foo_x005F_xk".to_vec()
    );
    assert_eq!(
        map_sql_identifier_to_xml_name(b"xmlfoo", true, true).unwrap(),
        b"_x0078_mlfoo".to_vec()
    );
    assert_eq!(
        map_xml_name_to_sql_identifier(b"_x0078_mlfoo").unwrap(),
        b"xmlfoo".to_vec()
    );
}

#[test]
fn errdetail_codes() {
    assert_eq!(errdetail_for_xml_code(9), "Invalid character value.");
    assert_eq!(errdetail_for_xml_code(4242), "Unrecognized libxml error code: 4242.");
}

// upstream 4c777d6dd9c9 (18.6): Fix handling of namespace nodes in xpath() (xml)
// A namespace node (XML_NAMESPACE_DECL) is an xmlNs, not an xmlNode: routing
// it through xmlCopyNode + xmlNodeDump returned the raw ` xmlns:foo="..."`
// declaration (18.3 regress: "could not copy node" on other libxml2 builds);
// C 18.6 casts it to its string value like attribute/text nodes.
#[test]
fn xpath_namespace_node_casts_to_string() {
    let mut out = Vec::new();
    let n = crate::xpath::xpath_internal(
        b"//namespace::foo",
        b"<root xmlns:foo=\"http://127.0.0.1\"/>",
        None,
        Some(&mut out),
    )
    .unwrap();
    assert_eq!(n, 1);
    assert_eq!(out, vec![b"http://127.0.0.1".to_vec()]);
}

// xml.c:2243-2262 xml_errorHandler: outside LEGACY strictness, level >=
// XML_ERR_ERROR is buffered for xml_ereport, level >= XML_ERR_WARNING is
// ereport(WARNING)ed, and everything below (libxml's informational level)
// is ereport(NOTICE)ed — never dropped.
#[test]
fn xml_error_handler_reports_below_warning_level_as_notice() {
    use core::ffi::c_void;
    use std::ffi::CString;

    use crate::errhandler::{
        set_strictness_for_test, take_pending_reports, xml_error_handler, PG_XML_STRICTNESS_ALL,
    };
    use crate::libxml::xmlErrorHdr;

    fn raise(level: i32, message: &str) {
        let msg = CString::new(message).unwrap();
        let mut err = xmlErrorHdr {
            domain: 1, // XML_FROM_PARSER
            code: 0,
            message: msg.as_ptr() as *mut _,
            level,
            file: core::ptr::null_mut(),
            line: 0,
            str1: core::ptr::null_mut(),
            str2: core::ptr::null_mut(),
            str3: core::ptr::null_mut(),
            int1: 0,
            int2: 0,
            ctxt: core::ptr::null_mut(),
            node: core::ptr::null_mut(),
        };
        // SAFETY: a fully initialised xmlError header, live for the call.
        unsafe { xml_error_handler(core::ptr::null_mut(), &mut err as *mut _ as *mut c_void) };
    }

    set_strictness_for_test(PG_XML_STRICTNESS_ALL);
    raise(1, "a warning\n"); // XML_ERR_WARNING
    raise(0, "an informational diagnostic\n"); // XML_ERR_NONE
    let reports = take_pending_reports();
    assert_eq!(
        reports,
        vec![
            (types_error::WARNING, "a warning".to_string()),
            (types_error::NOTICE, "an informational diagnostic".to_string()),
        ]
    );
    set_strictness_for_test(0);
}

// xml.c:2199-2211: XML_FROM_MEMORY (6) and XML_FROM_IO (8) errors are
// accepted under every strictness; other non-parser domains are dropped
// under WELLFORMED.
#[test]
fn xml_error_handler_accepts_memory_and_io_domains_when_wellformed() {
    use core::ffi::c_void;
    use std::ffi::CString;

    use crate::errhandler::{
        set_strictness_for_test, xml_err_detail, xml_err_occurred, xml_error_handler,
        PG_XML_STRICTNESS_WELLFORMED,
    };
    use crate::libxml::xmlErrorHdr;

    fn raise(domain: i32, message: &str) {
        let msg = CString::new(message).unwrap();
        let mut err = xmlErrorHdr {
            domain,
            code: 0,
            message: msg.as_ptr() as *mut _,
            level: 2, // XML_ERR_ERROR
            file: core::ptr::null_mut(),
            line: 0,
            str1: core::ptr::null_mut(),
            str2: core::ptr::null_mut(),
            str3: core::ptr::null_mut(),
            int1: 0,
            int2: 0,
            ctxt: core::ptr::null_mut(),
            node: core::ptr::null_mut(),
        };
        // SAFETY: a fully initialised xmlError header, live for the call.
        unsafe { xml_error_handler(core::ptr::null_mut(), &mut err as *mut _ as *mut c_void) };
    }

    for (domain, name) in [(6, "memory"), (8, "io")] {
        set_strictness_for_test(PG_XML_STRICTNESS_WELLFORMED);
        raise(domain, &format!("{name} failed\n"));
        assert!(xml_err_occurred(), "domain {domain} must be accepted");
        assert_eq!(xml_err_detail(), format!("{name} failed").as_bytes());
    }
    for domain in [13, 15] {
        set_strictness_for_test(PG_XML_STRICTNESS_WELLFORMED);
        raise(domain, "dropped\n");
        assert!(!xml_err_occurred(), "domain {domain} must be dropped");
    }
    set_strictness_for_test(0);
}

// fp-adt-xml-p1#1: xml.c:410 hands the declaration's encoding name bytes to
// pg_char_to_encoding, whose clean_encoding_name drops every non-alnum byte;
// the name is never UTF-8-validated (or lossily re-encoded) first.
#[test]
fn xml_char_to_encoding_takes_raw_name_bytes() {
    const PG_UTF8: i32 = 6;
    let mut name = b"UTF8".to_vec();
    name.extend_from_slice(&[0xff; 30]);
    assert_eq!(xmlChar_to_encoding(&name).unwrap(), PG_UTF8);
    assert_eq!(xmlChar_to_encoding(b"utf-8").unwrap(), PG_UTF8);
    let e = xmlChar_to_encoding(b"bogus").unwrap_err();
    assert_eq!(e.message(), "invalid encoding name \"bogus\"");
    assert_eq!(e.sqlstate(), ::types_error::ERRCODE_INVALID_PARAMETER_VALUE);
    let e = xmlChar_to_encoding(b"bo\xffgus").unwrap_err();
    assert_eq!(e.message_raw.as_deref(), Some(&b"invalid encoding name \"bo\xffgus\""[..]));
}

#[test]
fn xmlconcat_growth_meets_the_stringinfo_ceiling() {
    // enlargeStringInfo: needed >= MaxAllocSize - len is ERROR 54000.
    assert!(stringinfo_room(0, ::mcx::MAX_ALLOC_SIZE - 1).is_ok());
    let err = stringinfo_room(10, ::mcx::MAX_ALLOC_SIZE - 10).unwrap_err();
    assert_eq!(err.sqlstate(), ::types_error::ERRCODE_PROGRAM_LIMIT_EXCEEDED);
    assert_eq!(
        err.message(),
        "string buffer exceeds maximum allowed length (1073741823 bytes)"
    );
    assert_eq!(
        err.detail(),
        Some("Cannot enlarge string buffer containing 10 bytes by 1073741813 more bytes.")
    );
    assert_eq!(xmlconcat(&[b"<a/>", b"<b/>"]).unwrap(), b"<a/><b/>");
}

#[test]
fn escape_xml_guards_every_append_like_enlarge_string_info() {
    let max = ::mcx::MAX_ALLOC_SIZE;
    assert!(enlarge_string_info(max - 2, 1).is_ok());
    assert!(enlarge_string_info(max - 6, 5).is_ok());
    assert!(enlarge_string_info(max - 5, 5).is_err());
    let err = enlarge_string_info(max - 1, 1).unwrap_err();
    assert_eq!(err.message(), "string buffer exceeds maximum allowed length (1073741823 bytes)");
    assert_eq!(
        err.detail(),
        Some("Cannot enlarge string buffer containing 1073741822 bytes by 1 more bytes.")
    );
    assert_eq!(escape_xml(b"a&b<c>\r"), b"a&amp;b&lt;c&gt;&#x0d;");
}

#[test]
fn xml_error_detail_preserves_invalid_utf8_excerpt() {
    let error = xml_parse_ok(
        b"<a>\xff</a>",
        XmlOptionType::XMLOPTION_CONTENT,
        true,
        PG_UTF8,
        None,
    )
    .unwrap_err();
    assert_eq!(error.sqlstate(), types_error::ERRCODE_INVALID_XML_CONTENT);
    let detail = error
        .detail_raw
        .as_deref()
        .unwrap_or_else(|| error.detail().unwrap().as_bytes());
    assert!(
        detail.windows(8).any(|part| part == b"<a>\xff</a>"),
        "{detail:?}"
    );
}

#[test]
fn xmltable_document_parse_uses_strictness_all() {
    use crate::errhandler::{pg_xml_init, PG_XML_STRICTNESS_WELLFORMED};
    use crate::xmltable::XmlTableContext;

    let mut cx = XmlTableContext::new(1).unwrap();
    pg_xml_init(PG_XML_STRICTNESS_WELLFORMED);
    let e = cx.set_document(b"<nosuchprefix:tag/>").unwrap_err();
    assert_eq!(e.message(), "could not parse XML document");
    cx.destroy();
}

#[test]
fn parse_xml_decl_uses_libxml_utf8_decoder() {
    let mut len = 2;
    // SAFETY: reads at most `len` bytes of the literal.
    let c = unsafe { (crate::libxml::xml2().xmlGetUTF8Char)(b"\xC1\xA1".as_ptr(), &mut len) };
    let (rc, ..) = decl(b"<?xml\xC1\xA1?><a/>");
    if c < 0 {
        assert_eq!(rc, XML_ERR_SPACE_REQUIRED);
    } else {
        assert_eq!(rc, 0);
    }
}

// Replays query_to_xml's append sequence for 11 rows of repeat('x', 1e8)
// against the growth model alone; the numbers are C 18.6's DETAIL.
#[test]
fn stringinfo_growth_matches_c_query_to_xml_limit() {
    use crate::stringinfo::StringInfoGrowth;
    const NS: usize = " xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\"".len();
    let value_line = "  <c>".len() + 100_000_000 + "</c>\n".len();
    for (tableforest, want_len) in [(false, 1_000_000_309usize), (true, 1_000_000_840)] {
        let mut g = StringInfoGrowth::default();
        let mut len = 0usize;
        let fmt = |g: &mut StringInfoGrowth, len: &mut usize, n: usize| -> PgResult<()> {
            g.before_format(*len, n)?;
            *len += n;
            Ok(())
        };
        let s = |g: &mut StringInfoGrowth, len: &mut usize, n: usize| -> PgResult<()> {
            g.before_append(*len, n)?;
            *len += n;
            Ok(())
        };
        let err = (|| -> PgResult<()> {
            if !tableforest {
                fmt(&mut g, &mut len, "<table".len())?;
                s(&mut g, &mut len, NS)?;
                s(&mut g, &mut len, 2)?;
                g.before_char(len)?;
                len += 1;
            }
            for _ in 0..11 {
                if tableforest {
                    fmt(&mut g, &mut len, "<row".len())?;
                    s(&mut g, &mut len, NS)?;
                    s(&mut g, &mut len, 2)?;
                } else {
                    s(&mut g, &mut len, "<row>\n".len())?;
                }
                fmt(&mut g, &mut len, value_line)?;
                if tableforest {
                    fmt(&mut g, &mut len, "</row>\n".len())?;
                    g.before_char(len)?;
                    len += 1;
                } else {
                    s(&mut g, &mut len, "</row>\n\n".len())?;
                }
            }
            Ok(())
        })()
        .unwrap_err();
        assert_eq!(err.sqlstate(), ::types_error::ERRCODE_PROGRAM_LIMIT_EXCEEDED);
        assert_eq!(
            err.detail().unwrap(),
            format!("Cannot enlarge string buffer containing {want_len} bytes by 100000011 more bytes.")
        );
    }
}

#[test]
fn stringinfo_append_kinds_follow_c() {
    use crate::stringinfo::StringInfo;
    let mut b = StringInfo::with_limit(64);
    b.append_str(&"a".repeat(40)).unwrap();
    // maxlen 64: 23 bytes left, so a 22-byte formatted piece still fits.
    b.append_fmt(format_args!("{}", "b".repeat(22))).unwrap();
    assert_eq!(b.len(), 62);
    // avail 2 < 16: appendStringInfoVA asks for 32 before formatting.
    let err = b.append_fmt(format_args!("c")).unwrap_err();
    assert_eq!(
        err.detail().unwrap(),
        "Cannot enlarge string buffer containing 62 bytes by 32 more bytes."
    );
    b.append_str("c").unwrap();
    let err = b.append_str("d").unwrap_err();
    assert_eq!(
        err.detail().unwrap(),
        "Cannot enlarge string buffer containing 63 bytes by 1 more bytes."
    );
    b.append_char('\n').unwrap_err();
    assert_eq!(b.as_str(), format!("{}{}c", "a".repeat(40), "b".repeat(22)));

    // Below the cap, a formatted piece that does not fit asks for nprinted+1
    // and the buffer doubles.
    let mut b = StringInfo::with_limit(1 << 20);
    b.append_fmt(format_args!("{}", "x".repeat(1500))).unwrap();
    assert_eq!(b.len(), 1500);
    assert!(b.into_string().capacity() >= 2047);
}
