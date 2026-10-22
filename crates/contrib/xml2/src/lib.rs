//! `contrib/xml2` (library `pgxml`) — deprecated XPath helpers over text
//! documents: xpath_string/number/bool/nodeset/list, xpath_table and
//! xml_encode_special_chars. Built without libxslt, as C's default build:
//! xslt_process raises feature_not_supported.

use core::ffi::{c_char, c_uchar};

use adt_xml::errhandler::{pg_xml_init, xml_ereport, PG_XML_STRICTNESS_LEGACY};
use adt_xml::libxml::{
    self, xml2, xmlBuffer, xmlDoc, xmlNodeHdr, xmlNodeSetHdr, xmlXPathCompExpr, xmlXPathContext,
    xmlXPathContextHdr, xmlXPathObject, xmlXPathObjectHdr, XML_NAMESPACE_DECL, XML_PARSE_NOENT,
    XPATH_NODESET, XPATH_STRING,
};
use datum::Datum;
use funcapi::{AttInMetadata, InitMaterializedSRF, MAT_SRF_USE_EXPECTED_DESC};
use types_error::{
    PgError, PgResult, ERRCODE_FEATURE_NOT_SUPPORTED, ERRCODE_INVALID_ARGUMENT_FOR_XQUERY,
    ERRCODE_INVALID_PARAMETER_VALUE, ERRCODE_SYNTAX_ERROR, NOTICE,
};
use types_fmgr::{varlena_result, FmgrInfo, FunctionCallInfoBaseData as Fcinfo, PGFunction};

const LIBRARY: &str = "pgxml";

fn arg_cstring(fcinfo: &Fcinfo, i: usize) -> PgResult<Vec<u8>> {
    // SAFETY: every pgxml entry point is STRICT; arg i is a non-null text.
    let v = unsafe { fcinfo.arg_varlena_packed(i)? };
    let mut out = mcx::std_vec_with_capacity("xml2", v.data().len() + 1)?;
    out.extend_from_slice(v.data());
    out.push(0);
    Ok(out)
}

fn text_result(fcinfo: &mut Fcinfo, s: &[u8]) -> PgResult<Datum> {
    let s = &s[..s.iter().position(|&b| b == 0).unwrap_or(s.len())];
    Ok(varlena_result(varlena::cstring_to_text(fcinfo.result_mcx(), s)?))
}

fn null_result(fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    fcinfo.isnull = true;
    Ok(Datum::null())
}

/// Owns a libxml-allocated string until drop.
struct XmlStr(*mut c_uchar);

impl XmlStr {
    fn bytes(&self) -> Vec<u8> {
        // SAFETY: a NUL-terminated libxml string (or null).
        unsafe { libxml::xmlchar_to_vec(self.0) }
    }
}

impl Drop for XmlStr {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: allocated by libxml, freed once.
            unsafe { xml2().xmlFree(self.0.cast()) }
        }
    }
}

/// The xpath_workspace: document, context and result freed on every exit.
struct Workspace {
    doctree: *mut xmlDoc,
    ctxt: *mut xmlXPathContext,
    res: *mut xmlXPathObject,
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let x = xml2();
        // SAFETY: each pointer is null or owned by this workspace.
        unsafe {
            if !self.res.is_null() {
                (x.xmlXPathFreeObject)(self.res);
            }
            if !self.ctxt.is_null() {
                (x.xmlXPathFreeContext)(self.ctxt);
            }
            if !self.doctree.is_null() {
                (x.xmlFreeDoc)(self.doctree);
            }
        }
    }
}

// Compiles and evaluates `xpath` (NUL-terminated) against a context rooted
// at the document element; syntax errors raise with the libxml detail.
unsafe fn eval_xpath(
    doctree: *mut xmlDoc,
    xpath: &[u8],
) -> PgResult<(*mut xmlXPathContext, *mut xmlXPathObject)> {
    let x = xml2();
    // SAFETY (fn body): doctree is a live parsed document.
    unsafe {
        let ctxt = (x.xmlXPathNewContext)(doctree);
        (*(ctxt as *mut xmlXPathContextHdr)).node = (x.xmlDocGetRootElement)(doctree);
        let comppath: *mut xmlXPathCompExpr =
            (x.xmlXPathCtxtCompile)(ctxt, xpath.as_ptr() as *const c_uchar);
        if comppath.is_null() {
            (x.xmlXPathFreeContext)(ctxt);
            return Err(xml_ereport("XPath Syntax Error", ERRCODE_INVALID_ARGUMENT_FOR_XQUERY).into());
        }
        let res = (x.xmlXPathCompiledEval)(comppath, ctxt);
        (x.xmlXPathFreeCompExpr)(comppath);
        Ok((ctxt, res))
    }
}

fn pgxml_xpath(document: &[u8], xpath: &[u8]) -> PgResult<Workspace> {
    let x = xml2();
    let mut ws = Workspace {
        doctree: core::ptr::null_mut(),
        ctxt: core::ptr::null_mut(),
        res: core::ptr::null_mut(),
    };
    pg_xml_init(PG_XML_STRICTNESS_LEGACY);
    // SAFETY: libxml objects land in `ws`, which frees them on every exit.
    unsafe {
        ws.doctree = (x.xmlReadMemory)(
            document.as_ptr() as *const c_char,
            document.len() as i32,
            core::ptr::null(),
            core::ptr::null(),
            XML_PARSE_NOENT,
        );
        if !ws.doctree.is_null() {
            let (ctxt, res) = eval_xpath(ws.doctree, xpath)?;
            ws.ctxt = ctxt;
            ws.res = res;
        }
    }
    Ok(ws)
}

fn obj_type(res: *mut xmlXPathObject) -> i32 {
    // SAFETY: a live xmlXPathObject.
    unsafe { (*(res as *const xmlXPathObjectHdr)).type_ }
}

fn unsupported_result(res: *mut xmlXPathObject) -> PgResult<Vec<u8>> {
    elog::elog(NOTICE, format!("unsupported XQuery result: {}", obj_type(res)))?;
    mcx::std_vec_from_slice("xml2", b"<unsupported/>")
}

// pgxmlNodeSetToText: each node dumped (or, with `plainsep`, cast to
// string and joined), optionally wrapped in <toptag> and <septag>.
unsafe fn nodeset_to_text(
    nodeset: *mut xmlNodeSetHdr,
    toptag: &[u8],
    septag: &[u8],
    plainsep: Option<&[u8]>,
) -> Vec<u8> {
    let x = xml2();
    // SAFETY (fn body): nodeset is null or a live node set of the result;
    // tag and separator slices are NUL-terminated.
    unsafe {
        let buf: *mut xmlBuffer = (x.xmlBufferCreate)();
        let write_tag = |open: bool, tag: &[u8]| {
            if tag.first().is_some_and(|&b| b != 0) {
                (x.xmlBufferWriteChar)(buf, if open { c"<" } else { c"</" }.as_ptr());
                (x.xmlBufferWriteCHAR)(buf, tag.as_ptr());
                (x.xmlBufferWriteChar)(buf, c">".as_ptr());
            }
        };
        write_tag(true, toptag);
        if !nodeset.is_null() {
            let n = (*nodeset).node_nr as usize;
            for i in 0..n {
                let node = *(*nodeset).node_tab.add(i);
                if let Some(sep) = plainsep {
                    let s = XmlStr((x.xmlXPathCastNodeToString)(node));
                    (x.xmlBufferWriteCHAR)(buf, s.0);
                    if i + 1 < n {
                        (x.xmlBufferWriteChar)(buf, sep.as_ptr() as *const c_char);
                    }
                } else {
                    write_tag(true, septag);
                    if libxml::node_type(node) == XML_NAMESPACE_DECL {
                        let s = XmlStr((x.xmlXPathCastNodeToString)(node));
                        (x.xmlBufferWriteCHAR)(buf, s.0);
                    } else {
                        let doc = (*(node as *const xmlNodeHdr)).doc;
                        (x.xmlNodeDump)(buf, doc, node, 1, 0);
                    }
                    write_tag(false, septag);
                }
            }
        }
        write_tag(false, toptag);
        let out = libxml::xmlchar_to_vec((x.xmlBufferContent)(buf));
        (x.xmlBufferFree)(buf);
        out
    }
}

fn result_to_text(
    ws: &Workspace,
    toptag: &[u8],
    septag: &[u8],
    plainsep: Option<&[u8]>,
) -> PgResult<Option<Vec<u8>>> {
    if ws.res.is_null() {
        return Ok(None);
    }
    // SAFETY: ws.res is a live result object.
    let hdr = unsafe { &*(ws.res as *const xmlXPathObjectHdr) };
    Ok(Some(match hdr.type_ {
        // SAFETY: a node-set result's nodesetval is null or live.
        XPATH_NODESET => unsafe {
            nodeset_to_text(hdr.nodesetval as *mut xmlNodeSetHdr, toptag, septag, plainsep)
        },
        // SAFETY: a string result's stringval is a libxml string.
        XPATH_STRING => unsafe { libxml::xmlchar_to_vec(hdr.stringval) },
        _ => unsupported_result(ws.res)?,
    }))
}

fn finish_text(fcinfo: &mut Fcinfo, out: Option<Vec<u8>>) -> PgResult<Datum> {
    match out {
        Some(s) => text_result(fcinfo, &s),
        None => null_result(fcinfo),
    }
}

/// xpath_string wraps the path as `string(<path>)`.
pub fn string_xpath(path: &[u8]) -> PgResult<Vec<u8>> {
    let mut xpath = mcx::std_vec_with_capacity("xml2", path.len() + 9)?;
    xpath.extend_from_slice(b"string(");
    xpath.extend_from_slice(path);
    xpath.extend_from_slice(b")\0");
    Ok(xpath)
}

fn fc_xml_encode_special_chars(_f: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let ts = arg_cstring(fcinfo, 0)?;
    // SAFETY: ts is NUL-terminated; the result is a libxml string.
    let tt = XmlStr(unsafe { (xml2().xmlEncodeSpecialChars)(core::ptr::null(), ts.as_ptr()) });
    text_result(fcinfo, &tt.bytes())
}

fn fc_xpath_nodeset(_f: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let document = arg_cstring(fcinfo, 0)?;
    let xpath = arg_cstring(fcinfo, 1)?;
    let toptag = arg_cstring(fcinfo, 2)?;
    let septag = arg_cstring(fcinfo, 3)?;
    let ws = pgxml_xpath(&document[..document.len() - 1], &xpath)?;
    let out = result_to_text(&ws, &toptag, &septag, None)?;
    drop(ws);
    finish_text(fcinfo, out)
}

fn fc_xpath_list(_f: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let document = arg_cstring(fcinfo, 0)?;
    let xpath = arg_cstring(fcinfo, 1)?;
    let plainsep = arg_cstring(fcinfo, 2)?;
    let ws = pgxml_xpath(&document[..document.len() - 1], &xpath)?;
    let out = result_to_text(&ws, b"\0", b"\0", Some(&plainsep))?;
    drop(ws);
    finish_text(fcinfo, out)
}

fn fc_xpath_string(_f: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let document = arg_cstring(fcinfo, 0)?;
    let path = arg_cstring(fcinfo, 1)?;
    let xpath = string_xpath(&path[..path.len() - 1])?;
    let ws = pgxml_xpath(&document[..document.len() - 1], &xpath)?;
    let out = result_to_text(&ws, b"\0", b"\0", None)?;
    drop(ws);
    finish_text(fcinfo, out)
}

fn fc_xpath_number(_f: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let document = arg_cstring(fcinfo, 0)?;
    let xpath = arg_cstring(fcinfo, 1)?;
    let ws = pgxml_xpath(&document[..document.len() - 1], &xpath)?;
    if ws.res.is_null() {
        return null_result(fcinfo);
    }
    // SAFETY: ws.res is a live result object.
    let f_res = unsafe { (xml2().xmlXPathCastToNumber)(ws.res) } as f32;
    drop(ws);
    if f_res.is_nan() {
        return null_result(fcinfo);
    }
    Ok(Datum::from_f32(f_res))
}

fn fc_xpath_bool(_f: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let document = arg_cstring(fcinfo, 0)?;
    let xpath = arg_cstring(fcinfo, 1)?;
    let ws = pgxml_xpath(&document[..document.len() - 1], &xpath)?;
    if ws.res.is_null() {
        return Ok(Datum::from_bool(false));
    }
    // SAFETY: ws.res is a live result object.
    let b_res = unsafe { (xml2().xmlXPathCastToBoolean)(ws.res) };
    Ok(Datum::from_bool(b_res != 0))
}

/// xpath_table splits its path set on `|`, keeping at most `max` paths.
pub fn split_xpaths(xpathset: &[u8], max: usize) -> Vec<&[u8]> {
    let mut paths = Vec::new();
    let mut rest = xpathset;
    while paths.len() < max {
        match rest.iter().position(|&b| b == b'|') {
            Some(p) => {
                paths.push(&rest[..p]);
                rest = &rest[p + 1..];
            }
            None => {
                paths.push(rest);
                break;
            }
        }
    }
    paths
}

struct DocGuard(*mut xmlDoc);

impl Drop for DocGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: a document parsed for this row, freed once.
            unsafe { (xml2().xmlFreeDoc)(self.0) }
        }
    }
}

struct CtxtGuard(*mut xmlXPathContext, *mut xmlXPathObject);

impl Drop for CtxtGuard {
    fn drop(&mut self) {
        let x = xml2();
        // SAFETY: context and result of one path evaluation, freed once.
        unsafe {
            if !self.1.is_null() {
                (x.xmlXPathFreeObject)(self.1);
            }
            (x.xmlXPathFreeContext)(self.0);
        }
    }
}

fn put_row(
    mcx: mcx::Mcx<'_>,
    attinmeta: &mut AttInMetadata,
    srf: &mut funcapi::MaterializedSRF<'_>,
    values: &[Option<Vec<u8>>],
) -> PgResult<()> {
    let mut refs: Vec<Option<&[u8]>> = mcx::std_vec_with_capacity("xml2", values.len())?;
    refs.extend(values.iter().map(|v| v.as_deref()));
    let (d, n) = attinmeta.build(mcx, &refs)?;
    srf.putvalues(&d, &n)
}

fn fc_xpath_table(flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let text_arg = |fcinfo: &Fcinfo, i| -> PgResult<String> {
        let v = arg_cstring(fcinfo, i)?;
        Ok(String::from_utf8_lossy(&v[..v.len() - 1]).into_owned())
    };
    let pkeyfield = text_arg(fcinfo, 0)?;
    let xmlfield = text_arg(fcinfo, 1)?;
    let relname = text_arg(fcinfo, 2)?;
    let xpathset = arg_cstring(fcinfo, 3)?;
    let condition = text_arg(fcinfo, 4)?;

    // SAFETY: the arming (per-query) context outlives this call.
    let mcx = unsafe { fcinfo.result_mcx_detached() };
    let flinfo = flinfo.expect("xpath_table: resolved FmgrInfo required");
    let mut srf = InitMaterializedSRF(mcx, flinfo, fcinfo, MAT_SRF_USE_EXPECTED_DESC)?;
    let natts = srf.tupdesc.natts as usize;
    if natts < 1 {
        return Err(Box::new(
            PgError::error("xpath_table must have at least one output column")
                .with_sqlstate(ERRCODE_SYNTAX_ERROR),
        ));
    }
    let mut attinmeta = AttInMetadata::new(&srf.tupdesc)?;

    let split = split_xpaths(&xpathset[..xpathset.len() - 1], natts - 1);
    let mut xpaths: Vec<Vec<u8>> = mcx::std_vec_with_capacity("xml2", split.len())?;
    for p in split {
        let mut path = mcx::std_vec_with_capacity("xml2", p.len() + 1)?;
        path.extend_from_slice(p);
        path.push(0);
        xpaths.push(path);
    }

    let query = format!("SELECT {pkeyfield}, {xmlfield} FROM {relname} WHERE {condition}");
    spi::SPI_connect()?;
    if spi::SPI_exec(&query, 0)? != spi::SPI_OK_SELECT {
        return Err(Box::new(PgError::error(format!(
            "xpath_table: SPI execution failed for query {query}"
        ))));
    }
    let proc = spi::SPI_processed() as usize;
    let h = spi::SPI_tuptable().expect("SELECT leaves a tuptable");
    let rows = spi::tuptable_with(h, |t| -> PgResult<Vec<(Option<&[u8]>, Option<&[u8]>)>> {
        if t.tupdesc.natts != 2 {
            return Err(Box::new(
                PgError::error(
                    "expression returning multiple columns is not valid in parameter list",
                )
                .with_sqlstate(ERRCODE_INVALID_PARAMETER_VALUE)
                .with_detail(format!(
                    "Expected two columns in SPI result, got {}.",
                    t.tupdesc.natts
                )),
            ));
        }
        let mut rows = mcx::std_vec_with_capacity("xml2", proc)?;
        for i in 0..proc {
            let pkey = spi::SPI_getvalue(mcx, &t.vals[i], &t.tupdesc, 1)?;
            let xmldoc = spi::SPI_getvalue(mcx, &t.vals[i], &t.tupdesc, 2)?;
            rows.push((pkey, xmldoc));
        }
        Ok(rows)
    })?;

    pg_xml_init(PG_XML_STRICTNESS_LEGACY);
    let x = xml2();
    for (pkey, xmldoc) in rows {
        let mut values: Vec<Option<Vec<u8>>> = mcx::std_vec_from_elem("xml2", None, natts)?;
        values[0] = pkey.map(|p| mcx::std_vec_from_slice("xml2", p)).transpose()?;
        let doctree = DocGuard(match &xmldoc {
            // SAFETY: parses the row's document bytes.
            Some(doc) => unsafe {
                (x.xmlReadMemory)(
                    doc.as_ptr() as *const c_char,
                    doc.len() as i32,
                    core::ptr::null(),
                    core::ptr::null(),
                    XML_PARSE_NOENT,
                )
            },
            None => core::ptr::null_mut(),
        });
        if doctree.0.is_null() {
            put_row(mcx, &mut attinmeta, &mut srf, &values)?;
            continue;
        }
        let mut rownr = 0usize;
        loop {
            let mut had_values = false;
            for (j, xpath) in xpaths.iter().enumerate() {
                // SAFETY: doctree is live for this row.
                let (ctxt, res) = unsafe { eval_xpath(doctree.0, xpath)? };
                let eval = CtxtGuard(ctxt, res);
                if eval.1.is_null() {
                    continue;
                }
                // SAFETY: a live result object.
                let hdr = unsafe { &*(eval.1 as *const xmlXPathObjectHdr) };
                values[j + 1] = match hdr.type_ {
                    XPATH_NODESET => {
                        let ns = hdr.nodesetval as *mut xmlNodeSetHdr;
                        // SAFETY: nodesetval is null or live with node_nr entries.
                        unsafe {
                            if !ns.is_null() && rownr < (*ns).node_nr as usize {
                                had_values = true;
                                let node = *(*ns).node_tab.add(rownr);
                                Some(XmlStr((x.xmlXPathCastNodeToString)(node)).bytes())
                            } else {
                                None
                            }
                        }
                    }
                    // SAFETY: a string result's stringval is a libxml string.
                    XPATH_STRING => Some(unsafe { libxml::xmlchar_to_vec(hdr.stringval) }),
                    _ => Some(unsupported_result(eval.1)?),
                };
            }
            if had_values {
                put_row(mcx, &mut attinmeta, &mut srf, &values)?;
            }
            rownr += 1;
            if !had_values {
                break;
            }
        }
    }

    spi::SPI_finish()?;
    Ok(srf.finish(fcinfo))
}

fn fc_xslt_process(_f: Option<&mut FmgrInfo>, _fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    Err(Box::new(
        PgError::error("xslt_process() is not available without libxslt")
            .with_sqlstate(ERRCODE_FEATURE_NOT_SUPPORTED),
    ))
}

fn lookup(function: &str) -> Option<PGFunction> {
    Some(match function {
        "xml_encode_special_chars" => fc_xml_encode_special_chars,
        "xpath_nodeset" => fc_xpath_nodeset,
        "xpath_list" => fc_xpath_list,
        "xpath_string" => fc_xpath_string,
        "xpath_number" => fc_xpath_number,
        "xpath_bool" => fc_xpath_bool,
        "xpath_table" => fc_xpath_table,
        "xslt_process" => fc_xslt_process,
        _ => return None,
    })
}

pub fn init_seams() {
    dfmgr::register_builtin_library(dfmgr::BuiltinLibraryEntry {
        name: LIBRARY,
        lookup,
        pg_init: None,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xpath_set_splits_on_pipe_up_to_column_count() {
        assert_eq!(
            split_xpaths(b"/a/b|/a/c|/a/d", 3),
            vec![&b"/a/b"[..], b"/a/c", b"/a/d"]
        );
        assert_eq!(split_xpaths(b"/a/b|/a/c|/a/d", 2), vec![&b"/a/b"[..], b"/a/c"]);
        assert_eq!(split_xpaths(b"/a/b", 3), vec![&b"/a/b"[..]]);
        assert!(split_xpaths(b"/a/b", 0).is_empty());
    }

    #[test]
    fn string_xpath_wraps_path() {
        assert_eq!(string_xpath(b"/a/text()").unwrap(), b"string(/a/text())\0".to_vec());
    }

    #[test]
    fn library_exports_every_c_symbol() {
        for f in [
            "xml_encode_special_chars",
            "xpath_nodeset",
            "xpath_list",
            "xpath_string",
            "xpath_number",
            "xpath_bool",
            "xpath_table",
            "xslt_process",
        ] {
            assert!(lookup(f).is_some(), "{f}");
        }
        assert!(lookup("xml_valid").is_none());
    }
}
