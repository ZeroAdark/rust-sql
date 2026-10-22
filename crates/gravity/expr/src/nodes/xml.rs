//! `XmlExpr` (the tail lane's file, expr-compiler.md §4.5): C `ExecEvalXmlExpr` over the
//! `adt_xml` value routines (the incumbent's `xmlops`, whose SQL-value-to-XML mapping is reused
//! through `execexpr`); the named and positional arguments evaluate into region cells, the
//! result is an xml image in the region.

use mcx::GrowIn;
use gravity_fn::{Datum, NullableDatum, Region};
use mcx::Mcx;
use types_core::Oid;
use types_error::PgResult;
use types_nodes::primnodes::{XmlExpr as XmlExprTree, XmlExprOp, XmlOptionType};

use crate::eval::{eval, Cx};
use crate::nodes::array::with_scratch;
use crate::nodes::misc::Xml;
use crate::nodes::{Builder, ExprNode, NodeId, Prog, St};
use crate::views::{no_space, payload_view};
use crate::CompileResult;

pub fn compile<'mcx>(b: &mut Builder<'_, 'mcx>, x: &XmlExprTree<'mcx>) -> CompileResult<NodeId> {
    let names = b.prog.names.len() as u32;
    for n in x.arg_names.iter() {
        let s = n.as_string().expect("an XML argument name is a String node").sval;
        // SAFETY: the node arena outlives the program (the harness's and the planner's contract).
        b.name(s);
    }
    let name = x.name.map(|s| b.name(s));
    let types = b.prog.oids.len() as u32;
    for a in x.named_args.iter().chain(x.args.iter()) {
        b.prog.oids.push_in(nodes_core::expr_type(a));
    }
    let named_args = b.kids(&x.named_args)?;
    let args = b.kids(&x.args)?;
    Ok(b.push(ExprNode::Xml(Xml { op: x.op, args, named_args, typ: x.r#type, name, names, types, xmloption: x.xmloption, indent: x.indent })))
}

fn xml_opt(v: XmlOptionType) -> adt_xml::XmlOptionType {
    match v {
        XmlOptionType::XMLOPTION_DOCUMENT => adt_xml::XmlOptionType::XMLOPTION_DOCUMENT,
        XmlOptionType::XMLOPTION_CONTENT => adt_xml::XmlOptionType::XMLOPTION_CONTENT,
    }
}

fn cells<'a>(region: &mut Region, n: usize) -> PgResult<&'a mut [NullableDatum]> {
    let p = region.reserve(n.max(1) * 16).ok_or_else(no_space)?;
    // SAFETY: `n` cells reserved (at least one); each is written before it is read.
    Ok(unsafe { core::slice::from_raw_parts_mut(p.cast(), n) })
}

pub fn eval_xml(p: &Prog<'_, '_>, st: &mut St<'_>, n: &Xml, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    let named = cells(cx.region, n.named_args.len as usize)?;
    for (i, c) in named.iter_mut().enumerate() {
        *c = eval(p, st, p.kids[n.named_args.start as usize + i], cx)?;
    }
    let args = cells(cx.region, n.args.len as usize)?;
    for (i, c) in args.iter_mut().enumerate() {
        *c = eval(p, st, p.kids[n.args.start as usize + i], cx)?;
    }
    let region: &mut Region = cx.region;
    with_scratch(|mcx| {
        let out = match n.op {
            XmlExprOp::IS_XMLCONCAT => {
                let mut vals: mcx::PgVec<&[u8]> = mcx::vec_with_capacity_in(mcx, args.len())?;
                for a in args.iter() {
                    if !a.isnull {
                        vals.push(payload_view(a.value, -1, region)?.bytes());
                    }
                }
                if vals.is_empty() {
                    return Ok(NullableDatum::null());
                }
                adt_xml::xmlconcat(&vals)?
            }
            XmlExprOp::IS_XMLFOREST => {
                let mut buf = Vec::new(); // alloc: Cold
                let mut any = false;
                for (i, a) in named.iter().enumerate() {
                    if a.isnull {
                        continue;
                    }
                    let argname = p.names[n.names as usize + i];
                    let mapped = map(mcx, a.value, p.oids[n.types as usize + i], true)?;
                    buf.extend_from_slice(b"<");
                    buf.extend_from_slice(argname.as_bytes());
                    buf.extend_from_slice(b">");
                    buf.extend_from_slice(&mapped);
                    buf.extend_from_slice(b"</");
                    buf.extend_from_slice(argname.as_bytes());
                    buf.extend_from_slice(b">");
                    any = true;
                }
                if !any {
                    return Ok(NullableDatum::null());
                }
                buf
            }
            XmlExprOp::IS_XMLELEMENT => {
                let mut named_strs: Vec<(String, Option<Vec<u8>>)> = Vec::with_capacity(named.len()); // alloc: Cold
                for (i, a) in named.iter().enumerate() {
                    let argname = p.names[n.names as usize + i].to_string(); // alloc: Cold
                    let v = if a.isnull { None } else { Some(map(mcx, a.value, p.oids[n.types as usize + i], false)?) };
                    named_strs.push((argname, v));
                }
                let mut content: Vec<Vec<u8>> = Vec::with_capacity(args.len()); // alloc: Cold
                for (i, a) in args.iter().enumerate() {
                    if !a.isnull {
                        content.push(map(mcx, a.value, p.oids[n.types as usize + named.len() + i], true)?);
                    }
                }
                adt_xml::xmlelement(p.names[n.name.expect("XMLELEMENT carries a name") as usize], &named_strs, &content)?
            }
            XmlExprOp::IS_XMLPARSE => {
                if args[0].isnull || args[1].isnull {
                    return Ok(NullableDatum::null());
                }
                let data = payload_view(args[0].value, -1, region)?.bytes();
                adt_xml::xmlparse(data, xml_opt(n.xmloption), args[1].value.as_bool())?
            }
            XmlExprOp::IS_XMLPI => {
                let name = p.names[n.name.expect("XMLPI carries a name") as usize];
                if args.is_empty() {
                    let _ = adt_xml::xmlpi(name, None)?;
                    let mut buf = Vec::with_capacity(name.len() + 4); // alloc: Cold
                    buf.extend_from_slice(b"<?");
                    buf.extend_from_slice(name.as_bytes());
                    buf.extend_from_slice(b"?>");
                    buf
                } else {
                    let arg = if args[0].isnull { None } else { Some(payload_view(args[0].value, -1, region)?.bytes()) };
                    match adt_xml::xmlpi(name, arg)? {
                        Some(out) => out,
                        None => return Ok(NullableDatum::null()),
                    }
                }
            }
            XmlExprOp::IS_XMLROOT => {
                if args[0].isnull {
                    return Ok(NullableDatum::null());
                }
                let data = payload_view(args[0].value, -1, region)?.bytes();
                let version = if args[1].isnull { None } else { Some(payload_view(args[1].value, -1, region)?.bytes()) };
                let standalone = match args[2].value.as_i32() {
                    0 => adt_xml::XmlStandaloneType::XML_STANDALONE_YES,
                    1 => adt_xml::XmlStandaloneType::XML_STANDALONE_NO,
                    2 => adt_xml::XmlStandaloneType::XML_STANDALONE_NO_VALUE,
                    _ => adt_xml::XmlStandaloneType::XML_STANDALONE_OMITTED,
                };
                adt_xml::xmlroot(data, version, standalone)?
            }
            XmlExprOp::IS_XMLSERIALIZE => {
                if args[0].isnull {
                    return Ok(NullableDatum::null());
                }
                let data = payload_view(args[0].value, -1, region)?.bytes();
                adt_xml::xmltotext_with_options(data, xml_opt(n.xmloption), n.indent)?
            }
            XmlExprOp::IS_DOCUMENT => {
                if args[0].isnull {
                    return Ok(NullableDatum::null());
                }
                let data = payload_view(args[0].value, -1, region)?.bytes();
                return Ok(NullableDatum::value(Datum::from_bool(adt_xml::xml_is_document(data)?)));
            }
        };
        let r = gravity_fn::region_payload(region, &out);
        if r.status != gravity_fn::OK {
            return Err(no_space());
        }
        Ok(NullableDatum::value(Datum::from_usize(r.val as usize)))
    })
}

/// C `map_sql_value_to_xml_value` (the incumbent's, over the scratch context).
fn map(mcx: Mcx<'_>, value: Datum, typ: Oid, xml_escape_strings: bool) -> PgResult<Vec<u8>> {
    let slot: execexpr::ResMcx = Some(core::ptr::NonNull::from(mcx_of(mcx)));
    execexpr::map_sql_value_to_xml_value(value, typ, xml_escape_strings, &slot)
}

fn mcx_of<'a>(mcx: Mcx<'a>) -> &'a mcx::MemoryContext {
    mcx.context()
}
