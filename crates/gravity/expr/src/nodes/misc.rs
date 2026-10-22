//! The statement-context nodes. `SQLValueFunction` is the builtin calls C `ExecEvalSQLValueFunction`
//! amounts to (`now()` and a cast / scale, `current_user()`, …: the same bodies, host sites the
//! hosted emit serves); `XmlExpr`, the JSON family, `NextValueExpr`, `CurrentOfExpr` need executor
//! state the row protocol does not carry: refused with the kind named. `ReturningExpr` compiles where
//! its image's existence is the command's (`compile_returning`).

use gravity_fn::{Datum, NullableDatum};
use types_core::Oid;
use types_error::PgResult;
use types_nodes::primnodes::{CurrentOfExpr, NextValueExpr, ReturningExpr, SQLValueFunction as SqlValueFunctionTree, SQLValueFunctionOp, XmlExpr, XmlExprOp, XmlOptionType};
use types_nodes::Node;

use crate::eval::Cx;
use crate::nodes::func::{bind_typed, FuncNode, How};
use crate::nodes::{unsupported, Builder, ExprNode, Kids, Kind, NodeId, Prog, SiteId, St};
use crate::CompileResult;

#[derive(Clone, Copy, Debug)]
pub struct SqlValueFunction {
    pub op: SQLValueFunctionOp,
    pub typ: Oid,
    pub typmod: i32,
}

#[derive(Clone, Copy, Debug)]
pub struct Xml {
    pub op: XmlExprOp,
    pub args: Kids,
    pub named_args: Kids,
    pub typ: Oid,
    /// The element / PI name and the first argument name in `Program::names`, the argument
    /// types (named, then positional) in `Program::oids`.
    pub name: Option<u32>,
    pub names: u32,
    pub types: u32,
    pub xmloption: XmlOptionType,
    pub indent: bool,
}

pub use crate::nodes::json::JsonNode as Json;

#[derive(Clone, Copy, Debug)]
pub struct NextValue {
    pub seqid: Oid,
    pub typ: Oid,
    /// The emitted call's site (`emit::store::NEXTVAL_SITE`: volatile, one call per row).
    pub site: SiteId,
}

#[derive(Clone, Copy, Debug)]
pub struct CurrentOf {
    pub cvarno: u32,
    pub cursor_param: i32,
}

/// A `ReturningExpr`, a whole-row or a system column over the OLD (`old`) or NEW image a command never has (an
/// INSERT's OLD, a DELETE's NEW): NULL, `arg` never run (`compile_returning`, var.rs).
#[derive(Clone, Copy, Debug)]
pub struct Returning {
    pub arg: NodeId,
    pub old: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct Imm {
    pub value: i64,
    pub typ: Oid,
}

const NOW: Oid = 1299;
const TIMESTAMPTZOID: Oid = 1184;
const INT4OID: Oid = 23;

/// The calls PostgreSQL's `ExecEvalSQLValueFunction` amounts to: `now()` (the transaction start) cast to the
/// type and scaled to the typmod, or the session's role / database / schema function — each the same body.
pub fn compile_sql_value_function(b: &mut Builder<'_, '_>, s: &SqlValueFunctionTree) -> CompileResult<NodeId> {
    use SQLValueFunctionOp as Op;
    let (cast, scale) = match s.op {
        // the role ops stay the node: `current_user()`'s body reads the identity TLS directly, the node the seam
        // (`get_user_id`), and the differential rig has only the seam — the emitter serves it as an interpreter island
        // (nodes/mod.rs `covered`), which reads the role at execution, so SECURITY DEFINER's switch is seen
        Op::SVFOP_CURRENT_ROLE | Op::SVFOP_CURRENT_USER | Op::SVFOP_USER | Op::SVFOP_SESSION_USER => {
            return Ok(b.push(ExprNode::SqlValueFunction(SqlValueFunction { op: s.op, typ: s.r#type, typmod: s.typmod })));
        }
        Op::SVFOP_CURRENT_CATALOG => return call(b, 861, &[], &[], s.r#type),
        Op::SVFOP_CURRENT_SCHEMA => return call(b, 1402, &[], &[], s.r#type),
        Op::SVFOP_CURRENT_DATE => (Some(1178), None),
        Op::SVFOP_CURRENT_TIME | Op::SVFOP_CURRENT_TIME_N => (Some(1388), Some(1969)),
        Op::SVFOP_CURRENT_TIMESTAMP | Op::SVFOP_CURRENT_TIMESTAMP_N => (None, Some(1967)),
        Op::SVFOP_LOCALTIME | Op::SVFOP_LOCALTIME_N => (Some(2019), Some(1968)),
        Op::SVFOP_LOCALTIMESTAMP | Op::SVFOP_LOCALTIMESTAMP_N => (Some(2027), Some(1961)),
    };
    let mut v = call(b, NOW, &[], &[], TIMESTAMPTZOID)?;
    let mut typ = TIMESTAMPTZOID;
    if let Some(oid) = cast {
        v = call(b, oid, &[v], &[typ], s.r#type)?;
        typ = s.r#type;
    }
    match scale {
        Some(oid) if s.typmod >= 0 => {
            let m = b.push(ExprNode::Imm(Imm { value: s.typmod as i64, typ: INT4OID }));
            call(b, oid, &[v, m], &[typ, INT4OID], s.r#type)
        }
        _ => Ok(v),
    }
}

fn call(b: &mut Builder<'_, '_>, funcid: Oid, args: &[NodeId], types: &[Oid], rettype: Oid) -> CompileResult<NodeId> {
    let kids = b.kids_of(args);
    let mut c = bind_typed(b, funcid, kids, types, 0)?;
    c.rettype = rettype;
    if let How::Fmgr(ix) = c.how {
        b.state.call_facts[ix as usize].rettype = rettype;
    }
    let site = b.site(c);
    Ok(b.push(ExprNode::Func(FuncNode { site })))
}

pub fn compile_xml<'mcx>(b: &mut Builder<'_, 'mcx>, x: &XmlExpr<'mcx>) -> CompileResult<NodeId> {
    crate::nodes::xml::compile(b, x)
}

pub fn compile_json<'mcx>(b: &mut Builder<'_, 'mcx>, n: Node<'mcx>, kind: Kind) -> CompileResult<NodeId> {
    crate::nodes::json::compile(b, n, kind)
}

pub fn compile_next_value(b: &mut Builder<'_, '_>, n: &NextValueExpr) -> CompileResult<NodeId> {
    if !matches!(n.typeId, 21 | 23 | 20) {
        return unsupported(Kind::NextValueExpr);
    }
    let site = crate::emit::store::next_value_site(b, n.typeId);
    Ok(b.push(ExprNode::NextValue(NextValue { seqid: n.seqid, typ: n.typeId, site })))
}

pub fn compile_current_of<'mcx>(b: &mut Builder<'_, 'mcx>, c: &CurrentOfExpr<'mcx>) -> CompileResult<NodeId> {
    Ok(b.push(ExprNode::CurrentOf(CurrentOf { cvarno: c.cvarno, cursor_param: c.cursor_param })))
}

/// C `EEOP_RETURNINGEXPR` (PG 18: the rewriter's wrapper of a view's or a virtual generated column's OLD / NEW
/// expression): NULL where that image does not exist, else the expression. The lowerer says per command whether it
/// exists (`Returning::old_exists` / `new_exists`): where it does, the expression alone; where it never does, the
/// `Returning` node (NULL) over it, compiled so the word plan finds its literals where the tree has them. Where it depends
/// on the row (MERGE's action), `CASE WHEN <the image's flag cell> THEN <the expression> END` (`Returning::old_flag` /
/// `new_flag`: EEOP_RETURNINGEXPR's test of EEO_FLAG_OLD_IS_NULL / NEW_IS_NULL, per row); with no flag (ON CONFLICT DO
/// UPDATE), or outside a RETURNING list, unsupported.
pub fn compile_returning<'mcx>(b: &mut Builder<'_, 'mcx>, r: &ReturningExpr<'mcx>) -> CompileResult<NodeId> {
    let Some(ret) = b.returning.filter(|_| r.retlevelsup == 0) else { return unsupported(Kind::ReturningExpr) };
    match if r.retold { ret.old_exists } else { ret.new_exists } {
        Some(true) => b.compile(r.retexpr),
        Some(false) => {
            let arg = b.compile(r.retexpr)?;
            Ok(b.push(ExprNode::Returning(Returning { arg, old: r.retold })))
        }
        None => {
            let Some((src, attno)) = (if r.retold { ret.old_flag } else { ret.new_flag }) else { return unsupported(Kind::ReturningExpr) };
            let cond = b.push(ExprNode::Var(crate::nodes::var::Var { src, attno, typ: BOOLOID, desc: 0 }));
            let then = b.compile(r.retexpr)?;
            let whens = b.kids_of(&[cond, then]);
            Ok(b.push(ExprNode::Case(crate::nodes::case::Case { arg: None, whens, default: None })))
        }
    }
}

const BOOLOID: Oid = 16;

pub fn eval_sql_value_function(_p: &Prog<'_, '_>, _st: &mut St<'_>, n: &SqlValueFunction, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    use SQLValueFunctionOp as Op;
    let value = match n.op {
        Op::SVFOP_CURRENT_DATE => Datum::from_i32(adt_date::GetSQLCurrentDate()?),
        Op::SVFOP_CURRENT_TIME | Op::SVFOP_CURRENT_TIME_N => {
            let t = adt_date::GetSQLCurrentTime(n.typmod)?;
            let mut img = [0u8; 12];
            img[..8].copy_from_slice(&t.time.to_ne_bytes());
            img[8..].copy_from_slice(&t.zone.to_ne_bytes());
            into_region(cx, &img)?
        }
        Op::SVFOP_CURRENT_TIMESTAMP | Op::SVFOP_CURRENT_TIMESTAMP_N => Datum::from_i64(adt_timestamp::GetSQLCurrentTimestamp(n.typmod)),
        Op::SVFOP_LOCALTIME | Op::SVFOP_LOCALTIME_N => Datum::from_i64(adt_date::GetSQLLocalTime(n.typmod)?),
        Op::SVFOP_LOCALTIMESTAMP | Op::SVFOP_LOCALTIMESTAMP_N => Datum::from_i64(adt_timestamp::GetSQLLocalTimestamp(n.typmod)?),
        Op::SVFOP_CURRENT_ROLE | Op::SVFOP_CURRENT_USER | Op::SVFOP_USER | Op::SVFOP_SESSION_USER => {
            let roleid = if n.op == Op::SVFOP_SESSION_USER { miscinit_seams::get_session_user_id::call() } else { miscinit_seams::get_user_id::call() };
            let shape = syscache_seams::lookup_authid_session_by_oid::call(roleid)?.ok_or_else(|| invalid_role_oid(roleid))?;
            into_region(cx, &shape.rolname.data)?
        }
        Op::SVFOP_CURRENT_CATALOG => {
            let dbname = dbcommands_seams::get_database_name::call(init_small::globals::MyDatabaseId())?.expect("the current database has a pg_database row");
            let mut name = types_tuple::NameData::default();
            name.namestrcpy(&dbname);
            into_region(cx, &name.data)?
        }
        Op::SVFOP_CURRENT_SCHEMA => {
            let cx_ = gravity_fn::session::root_context("gravity_expr current_schema"); // alloc: Exec
            let path = namespace_seams::fetch_search_path::call(cx_.mcx(), false)?;
            let Some(&first) = path.first() else { return Ok(NullableDatum::null()) };
            let Some(nspname) = lsyscache::get_namespace_name(cx_.mcx(), first)? else { return Ok(NullableDatum::null()) };
            let mut name = types_tuple::NameData::default();
            name.namestrcpy(&nspname);
            drop(nspname);
            drop(path);
            into_region(cx, &name.data)?
        }
    };
    Ok(NullableDatum::value(value))
}

#[inline]
fn into_region(cx: &mut Cx<'_, '_>, img: &[u8]) -> PgResult<Datum> {
    let r = gravity_fn::region_copy(cx.region, img);
    if r.status == gravity_fn::OK {
        Ok(Datum::from_usize(r.val as usize))
    } else {
        Err(crate::views::no_space())
    }
}

#[cold]
#[inline(never)]
fn invalid_role_oid(roleid: Oid) -> Box<types_error::PgError> {
    Box::new(types_error::PgError::error(format!("invalid role OID: {roleid}")).with_sqlstate(types_error::ERRCODE_UNDEFINED_OBJECT)) // alloc: Cold
}

pub fn eval_xml(p: &Prog<'_, '_>, st: &mut St<'_>, n: &Xml, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    crate::nodes::xml::eval_xml(p, st, n, cx)
}

pub fn eval_json(p: &Prog<'_, '_>, st: &mut St<'_>, n: &Json, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    crate::nodes::json::eval_json(p, st, n, cx)
}

/// C `ExecEvalNextValueExpr`: `nextval_internal` without the permission check.
pub fn eval_next_value(_p: &Prog<'_, '_>, _st: &mut St<'_>, n: &NextValue, _cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    let v = sequence_seams::nextval_internal::call(n.seqid, false)?;
    Ok(NullableDatum::value(match n.typ {
        21 => Datum::from_i16(v as i16),
        23 => Datum::from_i32(v as i32),
        _ => Datum::from_i64(v),
    }))
}

/// C `ExecEvalCurrentOfExpr`: only a table AM's scan answers it; anywhere else is this error.
pub fn eval_current_of(_p: &Prog<'_, '_>, _st: &mut St<'_>, _n: &CurrentOf, _cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    Err(Box::new(types_error::PgError::error("WHERE CURRENT OF is not supported for this table type").with_sqlstate(types_error::ERRCODE_FEATURE_NOT_SUPPORTED))) // alloc: Cold
}

/// EEOP_RETURNINGEXPR (ExecEvalWholeRowVar / ExecEvalSysVar) under EEO_FLAG_OLD_IS_NULL / NEW_IS_NULL: NULL.
pub fn eval_returning(_p: &Prog<'_, '_>, _st: &mut St<'_>, _n: &Returning, _cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    Ok(NullableDatum::null())
}
