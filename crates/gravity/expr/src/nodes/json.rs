//! The SQL/JSON family (the tail lane's file, expr-compiler.md §4.7): `JsonExpr` (JSON_EXISTS /
//! JSON_QUERY / JSON_VALUE: the path over the document with the PASSING variables, the ON EMPTY /
//! ON ERROR behaviors, the RETURNING coercion through the input function or `json_populate_type`),
//! `JsonConstructorExpr` (JSON_OBJECT / JSON_ARRAY / JSON_SCALAR / JSON_PARSE over the adt
//! builders), `JsonIsPredicate` (IS JSON) and `JsonValueExpr` (its formatted expression). The C
//! step program's jumps are the branches here; the jsonb routines that build in an `Mcx` run on
//! the array scratch and their images are copied into the region.

use mcx::GrowIn;
use adt_jsonpath_exec::{JbV, JsonPathQueryResult, JsonPathValueResult, JsonPathVariable, JsonWrapper};
use fmgr::ErrorSaveNode;
use gravity_fn::{Datum, FmgrCall, NullableDatum, Region};
use mcx::Mcx;
use types_core::Oid;
use types_error::{PgError, PgResult};
use types_nodes::primnodes::{JsonBehaviorType as JBT, JsonConstructorType as JC, JsonExprOp, JsonFormatType, JsonValueType as JT, JsonWrapper as NodeWrapper};
use types_nodes::{Node, NodeTag};

use crate::eval::{eval, Cx};
use crate::nodes::array::with_scratch;
use crate::nodes::cast;
use crate::nodes::{Builder, ExprNode, Kids, Kind, NodeId, Prog, St};
use crate::views::{copy_into, datum_of, image_view, no_space};
use crate::CompileResult;

const INT4OID: Oid = 23;
const TEXTOID: Oid = 25;
const JSONOID: Oid = 114;
const JSONBOID: Oid = 3802;

#[derive(Clone, Copy, Debug)]
pub struct Behavior {
    pub btype: JBT,
    /// The behavior's expression when it is evaluated (not the NULL-constant shortcut).
    pub expr: Option<NodeId>,
    pub coerce: bool,
    pub needs_finish: bool,
}

/// The RETURNING coercion through `json_populate_type` (`cache` indexes `State::json`).
#[derive(Clone, Copy, Debug)]
pub struct Coercion {
    pub typ: Oid,
    pub typmod: i32,
    pub len: i16,
    pub byval: bool,
    pub omit_quotes: bool,
    pub exists_coerce: bool,
    pub exists_cast_to_int: bool,
    pub exists_check_domain: bool,
    pub cache: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct JsonExprNode {
    pub op: JsonExprOp,
    pub formatted: NodeId,
    pub path: NodeId,
    pub passing: Kids,
    /// The first of `passing.len` entries in `Program::json_vars`.
    pub vars: u32,
    pub wrapper: JsonWrapper,
    pub returning: Oid,
    pub throw_error: bool,
    pub on_error: Behavior,
    pub on_empty: Option<Behavior>,
    /// The input function of the RETURNING type (`State::calls`) for `use_io_coercion`.
    pub io: Option<u32>,
    pub json: Option<Coercion>,
    pub column_name: Option<u32>,
    pub ioparam: Oid,
    pub ret_typmod: i32,
    pub ret_len: i16,
    pub ret_byval: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct Constructor {
    pub ctor: JC,
    /// The constructor's own function or its first argument, when the value is theirs.
    pub func: Option<NodeId>,
    pub args: Kids,
    /// The first of `args.len` entries in `Program::oids`.
    pub types: u32,
    pub is_jsonb: bool,
    pub absent_on_null: bool,
    pub unique: bool,
    /// `JSON_SCALAR`'s categorized type (`State::json`).
    pub cat: u32,
    pub coercion: Option<NodeId>,
}

#[derive(Clone, Copy, Debug)]
pub struct IsJson {
    pub arg: NodeId,
    pub exprtype: Oid,
    pub item_type: JT,
    pub unique_keys: bool,
}

#[derive(Clone, Copy, Debug)]
pub enum JsonNode {
    Expr(JsonExprNode),
    Func(NodeId),
    Constructor(Constructor),
    IsJson(IsJson),
    Value(NodeId),
}

/// A PASSING variable: the name from the tree, the argument's type.
#[derive(Clone, Copy, Debug)]
pub struct JsonVar {
    pub name: &'static str,
    pub typid: Oid,
    pub typmod: i32,
}

/// The per-node mutable state: `json_populate_type`'s cache on its own context, a scalar's category.
pub struct JsonSlot {
    /// Declared before `cx`: the cache is freed before its context.
    pub cache: Option<adt_jsonb::populate::ColumnIoData<'static>>,
    pub cx: Box<mcx::MemoryContext>,
    pub cat_jsonb: Option<adt_jsonb::tojsonb::ValCategory>,
    pub cat_json: Option<adt_json::tojson::TypeCat>,
}

impl JsonSlot {
    fn new() -> JsonSlot {
        JsonSlot { cache: None, cx: Box::new(gravity_fn::session::root_context("gravity_expr json cache")), cat_jsonb: None, cat_json: None } // alloc: Program
    }
}

pub fn compile<'mcx>(b: &mut Builder<'_, 'mcx>, n: Node<'mcx>, kind: Kind) -> CompileResult<NodeId> {
    let node = match kind {
        Kind::JsonValueExpr => {
            let j = n.as_json_value_expr().unwrap();
            let e = j.formatted_expr.or(j.raw_expr).ok_or(crate::CompileError::Unsupported(kind))?;
            JsonNode::Value(b.compile(e)?)
        }
        Kind::JsonIsPredicate => {
            let p = n.as_json_is_predicate().unwrap();
            let e = p.expr.ok_or(crate::CompileError::Unsupported(kind))?;
            JsonNode::IsJson(IsJson { arg: b.compile(e)?, exprtype: nodes_core::expr_type(e), item_type: p.item_type, unique_keys: p.unique_keys })
        }
        Kind::JsonConstructorExpr => compile_constructor(b, n.as_json_constructor_expr().unwrap())?,
        _ => compile_expr(b, n.as_json_expr().unwrap())?,
    };
    Ok(b.push(ExprNode::Json(node)))
}

fn compile_constructor<'mcx>(b: &mut Builder<'_, 'mcx>, c: &types_nodes::primnodes::JsonConstructorExpr<'mcx>) -> CompileResult<JsonNode> {
    let coercion = match c.coercion {
        Some(e) => Some(b.compile(e)?),
        None => None,
    };
    let func = if let Some(f) = c.func {
        Some(b.compile(f)?)
    } else if (c.r#type == JC::JSCTOR_JSON_PARSE && !c.unique) || c.r#type == JC::JSCTOR_JSON_SERIALIZE {
        Some(b.compile(c.args.nth(0))?)
    } else {
        None
    };
    if let Some(func) = func {
        return Ok(JsonNode::Constructor(Constructor { ctor: c.r#type, func: Some(func), args: Kids::NONE, types: 0, is_jsonb: false, absent_on_null: false, unique: false, cat: u32::MAX, coercion }));
    }
    let types = b.prog.oids.len() as u32;
    for a in c.args.iter() {
        b.prog.oids.push_in(nodes_core::expr_type(a));
    }
    let args = b.kids(&c.args)?;
    let is_jsonb = c.returning.and_then(|r| r.format).is_some_and(|f| f.format_type == JsonFormatType::JS_FORMAT_JSONB);
    let mut slot = JsonSlot::new();
    if c.r#type == JC::JSCTOR_JSON_SCALAR {
        let typid = b.prog.oids[types as usize];
        if is_jsonb {
            slot.cat_jsonb = Some(adt_jsonb::tojsonb::json_categorize_type(typid)?);
        } else {
            slot.cat_json = Some(adt_json::tojson::json_categorize_type(typid)?);
        }
    }
    let cat = b.state.json.len() as u32;
    b.state.json.push(slot);
    Ok(JsonNode::Constructor(Constructor { ctor: c.r#type, func: None, args, types, is_jsonb, absent_on_null: c.absent_on_null, unique: c.unique, cat, coercion }))
}

fn compile_expr<'mcx>(b: &mut Builder<'_, 'mcx>, j: &types_nodes::primnodes::JsonExpr<'mcx>) -> CompileResult<JsonNode> {
    if j.op == JsonExprOp::JSON_TABLE_OP {
        return Ok(JsonNode::Func(b.compile(j.formatted_expr.ok_or(crate::CompileError::Unsupported(Kind::JsonExpr))?)?));
    }
    let returning = j.returning.ok_or(crate::CompileError::Unsupported(Kind::JsonExpr))?;
    let on_error = j.on_error.and_then(|n| n.as_json_behavior()).ok_or(crate::CompileError::Unsupported(Kind::JsonExpr))?;
    let on_empty = j.on_empty.map(|n| n.as_json_behavior().expect("a JsonBehavior node"));
    let returning_domain = b.catalog.type_facts(returning.typid)?.is_domain();
    let formatted = b.compile(j.formatted_expr.ok_or(crate::CompileError::Unsupported(Kind::JsonExpr))?)?;
    let path = b.compile(j.path_spec.ok_or(crate::CompileError::Unsupported(Kind::JsonExpr))?)?;
    let vars = b.prog.json_vars.len() as u32;
    for (value, name) in j.passing_values.iter().zip(j.passing_names.iter()) {
        let name = b.own_str(name.as_string().expect("a passing name is a String node").sval);
        b.prog.json_vars.push_in(JsonVar { name, typid: nodes_core::expr_type(value), typmod: nodes_core::expr_typmod(value) });
    }
    let passing = b.kids(&j.passing_values)?;
    let throw_error = on_error.btype == JBT::JSON_BEHAVIOR_ERROR;
    let mut behavior = |b: &mut Builder<'_, 'mcx>, bh: &types_nodes::primnodes::JsonBehavior<'mcx>| -> CompileResult<Behavior> {
        let shortcut = bh.expr.and_then(|e| e.as_const()).is_some_and(|c| c.constisnull) && !returning_domain;
        let expr = match bh.expr {
            Some(e) if bh.btype != JBT::JSON_BEHAVIOR_ERROR && !shortcut => Some(b.compile(e)?),
            _ => None,
        };
        let needs_finish = bh.coerce || bh.expr.is_some_and(|e| matches!(e.node_tag(), NodeTag::T_CoerceViaIO | NodeTag::T_CoerceToDomain));
        Ok(Behavior { btype: bh.btype, expr, coerce: bh.coerce, needs_finish })
    };
    let on_error = behavior(b, on_error)?;
    let on_empty = match on_empty {
        Some(e) => Some(behavior(b, e)?),
        None => None,
    };
    let json = if j.use_json_coercion { Some(coercion(b, returning.typid, returning.typmod, j.omit_quotes, j.op == JsonExprOp::JSON_EXISTS_OP)?) } else { None };
    let mut ioparam = 0;
    let io = if j.use_io_coercion && json.is_none() {
        let io = b.catalog.io(returning.typid)?;
        ioparam = io.ioparam;
        Some(b.fmgr(FmgrCall::bind(io.input, 3, 0)?, &[], true))
    } else {
        None
    };
    let rf = b.catalog.type_facts(returning.typid)?;
    let column_name = j.column_name.map(|s| b.name(s));
    Ok(JsonNode::Expr(JsonExprNode { op: j.op, formatted, path, passing, vars, wrapper: wrapper_of(j.wrapper), returning: returning.typid, throw_error, on_error, on_empty, io, json, column_name, ioparam, ret_typmod: returning.typmod, ret_len: rf.len, ret_byval: rf.byval }))
}

fn wrapper_of(w: NodeWrapper) -> JsonWrapper {
    match w {
        NodeWrapper::JSW_UNSPEC => JsonWrapper::Unspec,
        NodeWrapper::JSW_NONE => JsonWrapper::None,
        NodeWrapper::JSW_CONDITIONAL => JsonWrapper::Conditional,
        NodeWrapper::JSW_UNCONDITIONAL => JsonWrapper::Unconditional,
    }
}

fn coercion(b: &mut Builder<'_, '_>, typ: Oid, typmod: i32, omit_quotes: bool, exists_coerce: bool) -> CompileResult<Coercion> {
    let mut tm = -1;
    let exists_cast_to_int = exists_coerce && b.catalog.base_type(typ, &mut tm)? == INT4OID;
    let exists_check_domain = exists_coerce && typcache::DomainHasConstraints(typ)?;
    let cache = b.state.json.len() as u32;
    b.state.json.push(JsonSlot::new());
    let tf = b.catalog.type_facts(typ)?;
    Ok(Coercion { typ, typmod, len: tf.len, byval: tf.byval, omit_quotes, exists_coerce, exists_cast_to_int, exists_check_domain, cache })
}

pub fn eval_json(p: &Prog<'_, '_>, st: &mut St<'_>, n: &JsonNode, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    match n {
        JsonNode::Func(id) | JsonNode::Value(id) => eval(p, st, *id, cx),
        JsonNode::IsJson(n) => eval_is_json(p, st, n, cx),
        JsonNode::Constructor(n) => eval_constructor(p, st, n, cx),
        JsonNode::Expr(n) => eval_expr(p, st, n, cx),
    }
}

/// The path evaluation's outcome before the behaviors: the value, and the flags C keeps in its
/// state (`error`, `empty`); the io-coercion input string lives in the scratch for the call.
struct Path {
    value: NullableDatum,
    error: bool,
    empty: bool,
}

fn eval_expr(p: &Prog<'_, '_>, st: &mut St<'_>, n: &JsonExprNode, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    let doc = eval(p, st, n.formatted, cx)?;
    if doc.isnull {
        return Ok(doc);
    }
    let path = eval(p, st, n.path, cx)?;
    if path.isnull {
        return Ok(path);
    }
    let nvars = n.passing.len as usize;
    let vars = cx.region.reserve(nvars * core::mem::size_of::<JsonPathVariable<'_>>()).ok_or_else(no_space)?;
    // SAFETY: `nvars` variables' worth of 8-aligned bytes reserved; every entry is written below.
    let vars: &mut [JsonPathVariable<'_>] = unsafe { core::slice::from_raw_parts_mut(vars.cast(), nvars) };
    for i in 0..nvars {
        let v = eval(p, st, p.kids[n.passing.start as usize + i], cx)?;
        let jv = &p.json_vars[n.vars as usize + i];
        vars[i] = JsonPathVariable { name: jv.name.as_bytes(), typid: jv.typid, typmod: jv.typmod, value: v.value, isnull: v.isnull };
    }
    let doc_img = image_view(doc.value, -1, cx.region)?;
    let path_img = image_view(path.value, -1, cx.region)?;
    let mut esc = ErrorSaveNode::new(false);
    let soft = !n.throw_error;
    let column_name = n.column_name.map(|i| p.names[i as usize]);
    let region: &mut Region = cx.region;
    let mut out = with_scratch(|mcx| path_eval(st, n, mcx, &doc_img.bytes()[4..], path_img.bytes(), vars, soft, column_name, &mut esc, region))?;
    if out.empty {
        out.value = NullableDatum::null();
        if let Some(e) = n.on_empty {
            if e.btype != JBT::JSON_BEHAVIOR_ERROR {
                return behavior(p, st, n, &e, false, cx);
            }
        } else if n.on_error.btype != JBT::JSON_BEHAVIOR_ERROR {
            return behavior(p, st, n, &n.on_error, true, cx);
        }
        return Err(no_json_item(column_name));
    }
    if out.error {
        return behavior(p, st, n, &n.on_error, true, cx);
    }
    if let Some(c) = n.json {
        let v = coerce(st, &c, out.value, if soft { Some(&mut esc) } else { None }, cx.region)?;
        if soft && esc.ctx.error_occurred() {
            return behavior(p, st, n, &n.on_error, true, cx);
        }
        return Ok(v);
    }
    Ok(out.value)
}

/// The ON EMPTY / ON ERROR arm: the behavior's expression (coerced, checked) or NULL.
fn behavior(p: &Prog<'_, '_>, st: &mut St<'_>, n: &JsonExprNode, b: &Behavior, is_error: bool, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    let Some(expr) = b.expr else { return Ok(NullableDatum::null()) };
    let clause = if is_error { "ON ERROR" } else { "ON EMPTY" };
    let soft = !n.throw_error;
    let mut v = if b.needs_finish && soft { behavior_root(p, st, expr, cx, clause, b.btype)? } else { eval(p, st, expr, cx)? };
    if b.coerce {
        let c = n.json.expect("a coerced behavior has the RETURNING coercion");
        let mut esc = ErrorSaveNode::new(true);
        v = coerce(st, &c, v, if soft { Some(&mut esc) } else { None }, cx.region)?;
        if esc.ctx.error_occurred() {
            return Err(behavior_coercion_error(clause, b.btype, esc.ctx.take_error()));
        }
    }
    Ok(v)
}

/// C evaluates the behavior's expression under the JSON soft-error context: a `CoerceToDomain` / `CoerceViaIO`
/// root's own failure (the domain's checks, the input function) is saved and rethrown wrapped by COERCION_FINISH
/// (execExpr.c); the argument beneath, and the output function, raise as themselves.
fn behavior_root(p: &Prog<'_, '_>, st: &mut St<'_>, expr: NodeId, cx: &mut Cx<'_, '_>, clause: &str, btype: JBT) -> PgResult<NullableDatum> {
    match &p.nodes[expr.0 as usize] {
        ExprNode::CoerceToDomain(d) => {
            let v = eval(p, st, d.arg, cx)?;
            cast::domain_checks(p, st, d, v, cx).map(|()| v).map_err(|e| behavior_coercion_error(clause, btype, Some(*e)))
        }
        ExprNode::CoerceViaIO(c) => {
            let v = eval(p, st, c.arg, cx)?;
            let text = cast::via_io_out(p, st, c, v, cx)?;
            cast::via_io_in(p, st, c, text, cx).map_err(|e| behavior_coercion_error(clause, btype, Some(*e)))
        }
        _ => eval(p, st, expr, cx),
    }
}

#[allow(clippy::too_many_arguments)]
fn path_eval(st: &mut St<'_>, n: &JsonExprNode, mcx: Mcx<'_>, doc: &[u8], path: &[u8], vars: &[JsonPathVariable<'_>], soft: bool, column_name: Option<&str>, esc: &mut ErrorSaveNode, region: &mut Region) -> PgResult<Path> {
    let mut out = Path { value: NullableDatum::null(), error: false, empty: false };
    let mut val_string: Option<mcx::PgVec<'_, u8>> = None;
    match n.op {
        JsonExprOp::JSON_EXISTS_OP => match adt_jsonpath_exec::json_path_exists(mcx, doc, path, soft, vars)? {
            None => out.error = true,
            Some(e) => out.value = NullableDatum::value(Datum::from_bool(e)),
        },
        JsonExprOp::JSON_QUERY_OP => match adt_jsonpath_exec::json_path_query(mcx, doc, path, n.wrapper, soft, vars, column_name)? {
            JsonPathQueryResult::Image(img) => out.value = NullableDatum::value(copy_into(region, datum_of(img.as_ptr()), -1)?),
            JsonPathQueryResult::Empty => out.empty = true,
            JsonPathQueryResult::Error => out.error = true,
        },
        JsonExprOp::JSON_VALUE_OP => match adt_jsonpath_exec::json_path_value(mcx, doc, path, soft, vars, column_name)? {
            JsonPathValueResult::Empty => out.empty = true,
            JsonPathValueResult::Error => out.error = true,
            JsonPathValueResult::Null => {}
            JsonPathValueResult::Value(jbv) => {
                if n.returning == JSONOID || n.returning == JSONBOID {
                    let img = adt_jsonpath_exec::jbv_to_jsonb_image(mcx, &jbv)?;
                    val_string = Some(adt_jsonb::io::jsonb_out(mcx, &img[4..])?);
                    out.value = NullableDatum::value(Datum::null());
                } else if n.json.is_some() {
                    let img = adt_jsonpath_exec::jbv_to_jsonb_image(mcx, &jbv)?;
                    out.value = NullableDatum::value(copy_into(region, datum_of(img.as_ptr()), -1)?);
                } else {
                    let s = item_cstring(mcx, &jbv)?;
                    if n.io.is_some() {
                        out.value = NullableDatum::value(Datum::null());
                    } else {
                        let r = gravity_fn::region_payload(region, &s[..s.len() - 1]);
                        if r.status != gravity_fn::OK {
                            return Err(no_space());
                        }
                        out.value = NullableDatum::value(Datum::from_usize(r.val as usize));
                    }
                    val_string = Some(s);
                }
            }
        },
        JsonExprOp::JSON_TABLE_OP => unreachable!("JSON_TABLE compiles to its document expression"),
    }
    if let (false, Some(io)) = (out.value.isnull, n.io) {
        let s = val_string.as_ref().expect("an io coercion has its value string");
        let args = [Datum::from_usize(s.as_ptr() as usize), Datum::from_u32(0), Datum::from_i32(n.ret_typmod)];
        let ctx = soft.then(|| core::ptr::NonNull::from(&mut *esc).cast());
        let v = st.calls[io as usize].call_site_in_context(&[args[0], Datum::from_u32(n.ioparam), args[2]], &[false; 3], ctx)?;
        if esc.ctx.error_occurred() {
            out.error = true;
            out.value = NullableDatum::null();
        } else {
            out.value = if v.isnull { v } else { NullableDatum::value(keep(v.value, n.ret_len, n.ret_byval, region)?) };
        }
    }
    Ok(out)
}

/// A by-reference result copied into the region (a by-value word as is).
fn keep(d: Datum, len: i16, byval: bool, region: &mut Region) -> PgResult<Datum> {
    if byval {
        return Ok(d);
    }
    copy_into(region, d, len)
}

/// C `ExecEvalJsonCoercion`: `json_populate_type` (or the EXISTS forms) over the jsonb result.
fn coerce(st: &mut St<'_>, c: &Coercion, v: NullableDatum, esc: Option<&mut ErrorSaveNode>, region: &mut Region) -> PgResult<NullableDatum> {
    let mut esc = esc;
    let mut cur = v;
    with_scratch(|mcx| {
        if c.exists_coerce {
            if c.exists_cast_to_int {
                if c.exists_check_domain {
                    typcache_seams::domain_check_input::call(cur.value, cur.isnull, c.typ, esc.as_deref_mut().map(|n| &mut n.ctx))?;
                    if esc.as_ref().is_some_and(|n| n.ctx.error_occurred()) {
                        return Ok(NullableDatum::null());
                    }
                }
                return Ok(NullableDatum { value: Datum::from_i32(cur.value.as_bool() as i32), isnull: cur.isnull });
            }
            let img = adt_jsonb::io::jsonb_in(mcx, if cur.value.as_bool() { b"true" } else { b"false" }, None)?.expect("a hard error returns Err");
            cur.value = datum_of(img.as_ptr());
            if c.typ == JSONBOID {
                return Ok(NullableDatum { value: copy_into(region, cur.value, -1)?, isnull: cur.isnull });
            }
        }
        let slot = &mut st.json[c.cache as usize];
        let cache_mcx: Mcx<'static> = unsafe { core::mem::transmute::<Mcx<'_>, Mcx<'static>>(slot.cx.mcx()) };
        let mut isnull = cur.isnull;
        // SAFETY: a non-null value is a live jsonb image (the path result or the `jsonb_in` above).
        let value = unsafe { adt_jsonb::populate::json_populate_type(cur.value, JSONBOID, c.typ, c.typmod, &mut slot.cache, cache_mcx, mcx, &mut isnull, c.omit_quotes, esc.as_deref_mut())? };
        if isnull {
            return Ok(NullableDatum::null());
        }
        Ok(NullableDatum::value(keep(value, c.len, c.byval, region)?))
    })
}

fn eval_is_json(p: &Prog<'_, '_>, st: &mut St<'_>, n: &IsJson, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    let v = eval(p, st, n.arg, cx)?;
    if v.isnull {
        return Ok(v);
    }
    let region: &mut Region = cx.region;
    let res = with_scratch(|mcx| {
        if n.exprtype == TEXTOID || n.exprtype == JSONOID {
            let text = image_view(v.value, -1, region)?;
            let json = &text.bytes()[4..];
            let mut res = if n.item_type == JT::JS_TYPE_ANY {
                true
            } else {
                use adt_json::jsonapi::JsonToken;
                match adt_jsonb::builtins::json_get_first_token(json)? {
                    Some(JsonToken::ObjectStart) => n.item_type == JT::JS_TYPE_OBJECT,
                    Some(JsonToken::ArrayStart) => n.item_type == JT::JS_TYPE_ARRAY,
                    Some(JsonToken::String | JsonToken::Number | JsonToken::True | JsonToken::False | JsonToken::Null) => n.item_type == JT::JS_TYPE_SCALAR,
                    _ => false,
                }
            };
            if res && (n.unique_keys || n.exprtype == TEXTOID) {
                let copy: &[u8] = mcx::slice_in(mcx, json)?.leak();
                res = adt_json::funcs::json_validate(mcx, copy, n.unique_keys, false)?;
            }
            Ok(res)
        } else if n.exprtype == JSONBOID {
            if n.item_type == JT::JS_TYPE_ANY {
                return Ok(true);
            }
            let c = &image_view(v.value, -1, region)?.bytes()[4..];
            Ok(match n.item_type {
                JT::JS_TYPE_OBJECT => adt_jsonb::container::container_is_object(c),
                JT::JS_TYPE_ARRAY => adt_jsonb::container::container_is_array(c) && !adt_jsonb::container::container_is_scalar(c),
                JT::JS_TYPE_SCALAR => adt_jsonb::container::container_is_array(c) && adt_jsonb::container::container_is_scalar(c),
                JT::JS_TYPE_ANY => true,
            })
        } else {
            Ok(false)
        }
    })?;
    Ok(NullableDatum::value(Datum::from_bool(res)))
}

fn eval_constructor(p: &Prog<'_, '_>, st: &mut St<'_>, n: &Constructor, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    let value = if let Some(f) = n.func {
        eval(p, st, f, cx)?
    } else {
        let count = n.args.len as usize;
        let cells = cx.region.reserve(count.max(1) * 16).ok_or_else(no_space)?;
        // SAFETY: `count` cells reserved (at least one), each written before it is read.
        let cells: &mut [NullableDatum] = unsafe { core::slice::from_raw_parts_mut(cells.cast(), count) };
        for (i, cell) in cells.iter_mut().enumerate() {
            *cell = eval(p, st, p.kids[n.args.start as usize + i], cx)?;
        }
        let types = &p.oids[n.types as usize..n.types as usize + count];
        let region: &mut Region = cx.region;
        with_scratch(|mcx| build(st, n, mcx, cells, types, region))?
    };
    let Some(co) = n.coercion else { return Ok(value) };
    let saved = cx.casetest;
    cx.casetest = value;
    let r = eval(p, st, co, cx);
    cx.casetest = saved;
    r
}

fn build(st: &mut St<'_>, n: &Constructor, mcx: Mcx<'_>, cells: &[NullableDatum], types: &[Oid], region: &mut Region) -> PgResult<NullableDatum> {
    let mut values = mcx::vec_with_capacity_in(mcx, cells.len())?;
    let mut nulls = mcx::vec_with_capacity_in(mcx, cells.len())?;
    for c in cells {
        values.push(c.value);
        nulls.push(c.isnull);
    }
    let img: &[u8] = match n.ctor {
        JC::JSCTOR_JSON_ARRAY => {
            if n.is_jsonb {
                adt_jsonb::tojsonb::jsonb_build_array_worker(mcx, &values, &nulls, types, n.absent_on_null)?.leak()
            } else {
                adt_json::tojson::json_build_array_worker(mcx, &values, &nulls, types, n.absent_on_null)?.into_image().leak()
            }
        }
        JC::JSCTOR_JSON_OBJECT => {
            if n.is_jsonb {
                adt_jsonb::tojsonb::jsonb_build_object_worker(mcx, &values, &nulls, types, n.absent_on_null, n.unique)?.leak()
            } else {
                adt_json::tojson::json_build_object_worker(mcx, &values, &nulls, types, n.absent_on_null, n.unique)?.into_image().leak()
            }
        }
        JC::JSCTOR_JSON_SCALAR => {
            if nulls[0] {
                return Ok(NullableDatum::null());
            }
            let slot = &mut st.json[n.cat as usize];
            if n.is_jsonb {
                adt_jsonb::tojsonb::datum_to_jsonb_cat(mcx, values[0], slot.cat_jsonb.as_mut().expect("categorized at compile"))?.leak()
            } else {
                adt_json::tojson::datum_to_json_cat(mcx, values[0], slot.cat_json.as_mut().expect("categorized at compile"))?.into_image().leak()
            }
        }
        JC::JSCTOR_JSON_PARSE => {
            if nulls[0] {
                return Ok(NullableDatum::null());
            }
            let text = image_view(values[0], -1, region)?;
            let js: &[u8] = mcx::slice_in(mcx, &text.bytes()[4..])?.leak();
            if n.is_jsonb {
                adt_jsonb::io::jsonb_from_cstring(mcx, js, true, None)?.expect("a hard error returns Err").leak()
            } else {
                adt_json::funcs::json_validate(mcx, js, true, true)?;
                return Ok(NullableDatum::value(copy_into(region, values[0], -1)?));
            }
        }
        JC::JSCTOR_JSON_OBJECTAGG | JC::JSCTOR_JSON_ARRAYAGG | JC::JSCTOR_JSON_SERIALIZE => unreachable!("an aggregate or serialize constructor compiles to its function"),
    };
    Ok(NullableDatum::value(copy_into(region, datum_of(img.as_ptr()), -1)?))
}

/// C `ExecGetJsonValueItemString` minus the null leg: the item's text with its NUL, in `mcx`.
fn item_cstring<'m>(mcx: Mcx<'m>, item: &JbV<'_>) -> PgResult<mcx::PgVec<'m, u8>> {
    use adt_formatting::ParsedDatetime;
    let cstr = |bytes: &[u8]| -> PgResult<mcx::PgVec<'m, u8>> {
        let mut v = mcx::vec_with_capacity_in(mcx, bytes.len() + 1)?;
        mcx::vec_append_bytes(&mut v, bytes)?;
        v.push(0);
        Ok(v)
    };
    match item {
        JbV::Null => unreachable!("a null item is the caller's Null variant"),
        JbV::String(s) => cstr(s),
        JbV::Numeric(img) => {
            let mut buf = Vec::new(); // alloc: Cold
            adt_numeric::numeric_out_into(adt_numeric::Num::from_payload(&img[4..]), &mut buf);
            cstr(&buf)
        }
        JbV::Bool(b) => cstr(if *b { b"t" } else { b"f" }),
        JbV::Datetime { value, .. } => {
            let mut buf = [0u8; adt_datetime::consts::MAXDATELEN + 1];
            let n = match value {
                ParsedDatetime::Date(d) => adt_date::date_out(*d, &mut buf),
                ParsedDatetime::Time(t) => adt_date::time_out(*t, &mut buf),
                ParsedDatetime::TimeTz(t) => adt_date::timetz_out(t, &mut buf),
                ParsedDatetime::Timestamp(ts) => adt_timestamp::timestamp_out(*ts, &mut buf)?,
                ParsedDatetime::TimestampTz(ts) => adt_timestamp::timestamptz_out(*ts, &mut buf)?,
            };
            cstr(&buf[..n])
        }
        JbV::Binary(_) => {
            let img = adt_jsonpath_exec::jbv_to_jsonb_image(mcx, item)?;
            adt_jsonb::io::jsonb_out(mcx, &img[4..])
        }
    }
}

#[cold]
#[inline(never)]
fn no_json_item(column_name: Option<&str>) -> Box<PgError> {
    let msg = match column_name {
        Some(col) => format!("no SQL/JSON item found for specified path of column \"{col}\""), // alloc: Cold
        None => "no SQL/JSON item found for specified path".to_string(), // alloc: Cold
    };
    Box::new(PgError::error(msg).with_sqlstate(types_error::ERRCODE_NO_SQL_JSON_ITEM)) // alloc: Cold
}

#[cold]
#[inline(never)]
fn behavior_coercion_error(clause: &str, btype: JBT, saved: Option<PgError>) -> Box<PgError> {
    const NAMES: [&str; 9] = ["NULL", "ERROR", "EMPTY", "TRUE", "FALSE", "UNKNOWN", "EMPTY ARRAY", "EMPTY OBJECT", "DEFAULT"];
    let mut e = PgError::error(format!("could not coerce {clause} expression ({}) to the RETURNING type", NAMES[btype as usize])).with_sqlstate(types_error::ERRCODE_DATATYPE_MISMATCH); // alloc: Cold
    if let Some(saved) = saved {
        e = e.with_detail(saved.message().to_string()); // alloc: Cold
    }
    Box::new(e) // alloc: Cold
}
