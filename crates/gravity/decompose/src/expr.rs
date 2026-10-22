#![allow(unsafe_code)]
use core::fmt::{self, Write};

use datum::varlena::VarlenaRef;
use types_nodes::list::NodeList;
use types_nodes::primnodes::{BoolExprType, NullTestType, INDEX_VAR, INNER_VAR, OUTER_VAR};
use types_nodes::{Node, NodeTag};

use crate::catalog::expr_type::type_of;

pub fn kids<'p>(node: Node<'p>, f: &mut dyn FnMut(Node<'p>)) -> bool {
    let list = |l: &NodeList<'p>, f: &mut dyn FnMut(Node<'p>)| l.iter().for_each(|n| f(n));
    let opt = |n: Option<Node<'p>>, f: &mut dyn FnMut(Node<'p>)| {
        if let Some(n) = n {
            f(n)
        }
    };
    match node.node_tag() {
        NodeTag::T_Var | NodeTag::T_Const | NodeTag::T_Param | NodeTag::T_CaseTestExpr | NodeTag::T_CoerceToDomainValue | NodeTag::T_SQLValueFunction => {}
        // MERGE .. RETURNING's merge_action(): a leaf (D789)
        NodeTag::T_MergeSupportFunc => {}
        NodeTag::T_TargetEntry => f(node.as_target_entry().unwrap().expr),
        // D3882 (pgwatch privilege_changes): an implicit-AND List (a qual list kept whole, a
        // Result's one-time filter as decompose map_result's Gate holds resconstantqual) reads as its elements, each
        // walked; before, a node no walk could see (a fail-closed `unknown`: a Gate's List hid user code)
        NodeTag::T_List => list(node.as_list().unwrap(), f),
        NodeTag::T_OpExpr => list(&node.as_op_expr().unwrap().args, f),
        NodeTag::T_DistinctExpr => list(&node.as_distinct_expr().unwrap().args, f),
        NodeTag::T_NullIfExpr => list(&node.as_null_if_expr().unwrap().args, f),
        NodeTag::T_FuncExpr => list(&node.as_func_expr().unwrap().args, f),
        NodeTag::T_BoolExpr => list(&node.as_bool_expr().unwrap().args, f),
        NodeTag::T_ScalarArrayOpExpr => list(&node.as_scalar_array_op_expr().unwrap().args, f),
        NodeTag::T_CoalesceExpr => list(&node.as_coalesce_expr().unwrap().args, f),
        NodeTag::T_MinMaxExpr => list(&node.as_min_max_expr().unwrap().args, f),
        NodeTag::T_ArrayExpr => list(&node.as_array_expr().unwrap().elements, f),
        NodeTag::T_RowExpr => list(&node.as_row_expr().unwrap().args, f),
        NodeTag::T_GroupingFunc => list(&node.as_grouping_func().unwrap().args, f),
        NodeTag::T_Aggref => {
            let a = node.as_aggref().unwrap();
            list(&a.aggdirectargs, f);
            list(&a.args, f);
            list(&a.aggorder, f);
            list(&a.aggdistinct, f);
            opt(a.aggfilter, f);
        }
        NodeTag::T_WindowFunc => {
            let w = node.as_window_func().unwrap();
            list(&w.args, f);
            opt(w.aggfilter, f);
        }
        NodeTag::T_RowCompareExpr => {
            let r = node.as_row_compare_expr().unwrap();
            list(&r.largs, f);
            list(&r.rargs, f);
        }
        NodeTag::T_RelabelType => f(node.as_relabel_type().unwrap().arg),
        NodeTag::T_CoerceViaIO => f(node.as_coerce_via_io().unwrap().arg),
        NodeTag::T_CollateExpr => f(node.as_collate_expr().unwrap().arg),
        NodeTag::T_CoerceToDomain => f(node.as_coerce_to_domain().unwrap().arg),
        NodeTag::T_ConvertRowtypeExpr => f(node.as_convert_rowtype_expr().unwrap().arg),
        NodeTag::T_FieldSelect => f(node.as_field_select().unwrap().arg),
        // the compile's order: the base row, then each new value (gravity_expr tuple.rs::compile_field_store)
        NodeTag::T_FieldStore => {
            let s = node.as_field_store().unwrap();
            f(s.arg);
            list(&s.newvals, f);
        }
        NodeTag::T_ArrayCoerceExpr => {
            let a = node.as_array_coerce_expr().unwrap();
            f(a.arg);
            opt(a.elemexpr, f);
        }
        NodeTag::T_NullTest => opt(node.as_null_test().unwrap().arg, f),
        NodeTag::T_BooleanTest => opt(node.as_boolean_test().unwrap().arg, f),
        NodeTag::T_CaseExpr => {
            let c = node.as_case_expr().unwrap();
            opt(c.arg, f);
            list(&c.args, f);
            opt(c.defresult, f);
        }
        NodeTag::T_CaseWhen => {
            let w = node.as_case_when().unwrap();
            opt(w.expr, f);
            opt(w.result, f);
        }
        NodeTag::T_SubPlan => {
            let s = node.as_sub_plan().unwrap();
            opt(s.testexpr, f);
            list(&s.args, f);
        }
        NodeTag::T_AlternativeSubPlan => list(&node.as_alternative_sub_plan().unwrap().subplans, f),
        // an open slice bound is a null member: no child (the key hashes which bounds are there)
        NodeTag::T_SubscriptingRef => {
            let r = node.as_subscripting_ref().unwrap();
            opt(r.refexpr, f);
            for n in r.refupperindexpr.iter().chain(r.reflowerindexpr.iter()) {
                opt(n, f);
            }
            opt(r.refassgnexpr, f);
        }
        NodeTag::T_JsonConstructorExpr => {
            let j = node.as_json_constructor_expr().unwrap();
            list(&j.args, f);
            opt(j.func, f);
            opt(j.coercion, f);
        }
        // the SQL/JSON query forms and XML (the expr compiler's own reads: json.rs / xml.rs)
        NodeTag::T_JsonExpr => {
            let j = node.as_json_expr().unwrap();
            opt(j.formatted_expr, f);
            opt(j.path_spec, f);
            list(&j.passing_values, f);
            opt(j.on_empty, f);
            opt(j.on_error, f);
        }
        NodeTag::T_JsonBehavior => opt(node.as_json_behavior().unwrap().expr, f),
        NodeTag::T_JsonValueExpr => {
            let j = node.as_json_value_expr().unwrap();
            opt(j.formatted_expr.or(j.raw_expr), f);
        }
        NodeTag::T_JsonIsPredicate => opt(node.as_json_is_predicate().unwrap().expr, f),
        NodeTag::T_XmlExpr => {
            let x = node.as_xml_expr().unwrap();
            list(&x.named_args, f);
            list(&x.args, f);
        }
        _ => return false,
    }
    true
}

pub fn walk<'p>(node: Node<'p>, f: &mut dyn FnMut(Node<'p>) -> bool) -> bool {
    crate::walk::stack_check(); // D3748
    if !f(node) {
        return false;
    }
    let mut ok = true;
    let known = kids(node, &mut |k| ok &= walk(k, f));
    ok && known
}

/// Each Aggref PG's Agg runs within `node`, read or not, outermost first (an aggregate's arguments are not searched): the
/// `walk` of `node`, plus a JsonValueExpr's `raw_expr` beside the `formatted_expr` `kids` gives the expression compiler.
/// PG's prepagg walks both and never shares an aggregate with a volatile argument (`find_compatible_agg`), so FORMAT JSON
/// over one (`JSON_OBJECT('a': JSON_OBJECTAGG(..) FORMAT JSON)`) is two transitions, the raw one's never read: PG's call
/// count. decompose agg.rs `collect` and exec v4/m6.rs (agg.arena) both read the list through here.
pub fn run_aggrefs<'p>(node: Node<'p>, f: &mut dyn FnMut(Node<'p>)) {
    crate::walk::stack_check(); // D3748
    walk(node, &mut |x| {
        if x.as_aggref().is_some() {
            f(x);
            return false;
        }
        if let Some(raw) = x.as_json_value_expr().filter(|j| j.formatted_expr.is_some()).and_then(|j| j.raw_expr) {
            run_aggrefs(raw, f);
        }
        true
    });
}

/// D858: the argument of `node` when it is a RelabelType that leaves the word as it is. A relabel between a zero-extended
/// 4-byte type (oid, xid, cid, the reg* types: `ObjectIdGetDatum`) and int4 (`Int32GetDatum` sign-extends) changes the
/// word's convention: the lowered expression re-forms it (gravity_expr `Canon`), so no form may read the argument's word in
/// its place — a key, an order, a forwarded column. The type list is gravity_expr nodes/cast.rs `zext4`'s.
pub fn through_relabel<'p>(node: Node<'p>) -> Option<Node<'p>> {
    let r = node.as_relabel_type()?;
    let same = match type_of(r.arg) {
        Some(from) => !reforms_word(from, r.resulttype),
        None => !zext4(r.resulttype) && r.resulttype != types_core::catalog::INT4OID,
    };
    same.then_some(r.arg)
}

use crate::catalog::casts::zext4;

/// D3630: `through_relabel` with each side judged by its base type (`typefacts::facts_of`: a domain carries its base's
/// row). A relabel from a domain over oid to int4 re-forms the word as one from oid does (gravity_expr nodes/cast.rs
/// `relabel_canon` reads the base types too), so no key may read the domain column's word in its place.
pub fn through_relabel_in<'p>(facts: &dyn crate::catalog::Facts, node: Node<'p>) -> Option<Node<'p>> {
    let a = through_relabel(node)?;
    let base = |t: u32| crate::catalog::typefacts::facts_of(facts, t).map_or(t, |f| f.base);
    let to = node.as_relabel_type()?.resulttype;
    match type_of(a) {
        Some(from) if reforms_word(base(from), base(to)) => None,
        _ => Some(a),
    }
}

/// D3630: a relabel on `e`'s relabel chain re-forms the word only once its domains are read as their bases
/// (`through_relabel` would strip it, `through_relabel_in` keeps it).
pub fn domain_reforms(facts: &dyn crate::catalog::Facts, mut e: Node<'_>) -> bool {
    while let Some(a) = through_relabel(e) {
        if through_relabel_in(facts, e).is_none() {
            return true;
        }
        e = a;
    }
    false
}

/// D858: whether a word of type `from` read as type `to` changes convention (one zero-extended, the other not): a binary
/// coercion between them re-forms the word, so `from`'s order is not `to`'s (int4's signed order against oid's unsigned one).
pub fn reforms_word(from: u32, to: u32) -> bool {
    zext4(from) != zext4(to)
}

pub fn has_subplan(node: Node<'_>) -> bool {
    let mut found = false;
    walk(node, &mut |n| {
        found |= matches!(n.node_tag(), NodeTag::T_SubPlan | NodeTag::T_AlternativeSubPlan);
        !found
    });
    found
}

/// A varlena constant's payload: a 1-byte or 4-byte uncompressed header (`plpgsql`'s short `bpchar` constants panicked
/// `VarlenaRef::data`); `None` for a null, a by-value type, a fixed length, a toast pointer or a compressed image.
/// The bytes decide what the caller builds: a bake (`bake::read`).
pub fn const_bytes<'p>(c: &types_nodes::primnodes::Const) -> Option<&'p [u8]> {
    if c.constisnull || c.constbyval || c.constlen != -1 {
        return None;
    }
    varlena_payload(crate::bake::read(c).as_usize() as *const u8)
}

/// P5-13: a non-NULL fixed-length by-reference constant's image (`len` bytes, its type's `typlen`), logged in the key as
/// `const_bytes`' payload is; `None` for a NULL, a by-value or a varlena constant, or another length.
pub fn const_fixed_bytes<'p>(c: &types_nodes::primnodes::Const, len: u16) -> Option<&'p [u8]> {
    crate::raw::const_fixed_image(c, len)
}

/// DC-14: `const_bytes` without the bake log, for a decision both keys hold as a class, never as the image's bytes: effects
/// `pattern_operand` (a constant-pattern match's class, `effects::pattern_good`: exec front.rs puts it, the content key
/// hashes the image) and D3788's `pattern_list` (a constant pattern list's class, `effects::pattern_list_good`: exec
/// front.rs and lower key.rs put it, the list's values staying words). Any other caller keys the literal through
/// `const_bytes`.
pub fn const_bytes_keyed<'p>(c: &types_nodes::primnodes::Const) -> Option<&'p [u8]> {
    if c.constisnull || c.constbyval || c.constlen != -1 {
        return None;
    }
    varlena_payload(c.constvalue.as_usize() as *const u8)
}

/// NEW-72 entry C: whether a non-null varlena constant is `''`, without the bake log: a decision both keys hold (exec
/// front.rs for every varlena literal; lower key.rs keys `''` by its image and skips it in `bound_text_refs`), never the image's bytes: the
/// per-code road's `''` code set (gravity_expr percode) and the length identity of `= ''` / `<> ''` (lowerv4 octet_lane.rs).
/// A compressed or external image is never empty. `None` for a NULL, a by-value or a fixed-length constant.
pub fn const_text_empty(c: &types_nodes::primnodes::Const) -> Option<bool> {
    if c.constisnull || c.constbyval || c.constlen != -1 {
        return None;
    }
    Some(varlena_payload(c.constvalue.as_usize() as *const u8).is_some_and(|b| b.is_empty()))
}

/// A varlena image's payload at `p` (a non-null by-reference constant's value): see `const_bytes`.
pub fn varlena_payload<'p>(p: *const u8) -> Option<&'p [u8]> {
    let b0 = unsafe { *p };
    if b0 & 0x01 == 0x01 {
        if b0 == 0x01 {
            return None;
        }
        let len = ((b0 >> 1) & 0x7F) as usize;
        Some(unsafe { core::slice::from_raw_parts(p.add(1), len - 1) })
    } else if b0 & 0x03 == 0 {
        Some(unsafe { VarlenaRef::from_ptr(p) }.data())
    } else {
        None
    }
}

pub fn print(node: Node<'_>, out: &mut dyn Write) -> fmt::Result {
    crate::walk::stack_check(); // D3748
    match node.node_tag() {
        NodeTag::T_Var => {
            let v = node.as_var().unwrap();
            match v.varno {
                INNER_VAR => write!(out, "inner.{}", v.varattno),
                OUTER_VAR => write!(out, "outer.{}", v.varattno),
                INDEX_VAR => write!(out, "index.{}", v.varattno),
                n => write!(out, "v{n}.{}", v.varattno),
            }
        }
        NodeTag::T_Const => {
            let c = node.as_const().unwrap();
            write!(out, "c{}:", c.consttype)?;
            if c.constisnull {
                write!(out, "null")
            } else if c.constbyval {
                match c.constlen {
                    1 => write!(out, "{}", c.constvalue.as_usize() as u8),
                    2 => write!(out, "{}", c.constvalue.as_i16()),
                    4 => write!(out, "{}", c.constvalue.as_i32()),
                    _ => write!(out, "{}", c.constvalue.as_i64()),
                }
            } else if c.constlen == -1 {
                let Some(data) = const_bytes(c) else { return write!(out, "<varlena>") };
                write!(out, "'")?;
                for &b in data {
                    if b.is_ascii_graphic() || b == b' ' {
                        out.write_char(b as char)?;
                    } else {
                        write!(out, "\\x{b:02x}")?;
                    }
                }
                write!(out, "'")
            } else {
                write!(out, "#{}", c.constlen)
            }
        }
        NodeTag::T_Param => {
            let p = node.as_param().unwrap();
            write!(out, "${}{}", if p.paramkind == types_nodes::primnodes::ParamKind::PARAM_EXTERN { "" } else { "exec" }, p.paramid)
        }
        NodeTag::T_TargetEntry => {
            let t = node.as_target_entry().unwrap();
            write!(out, "{}{}=", t.resno, if t.resjunk { "j" } else { "" })?;
            print(t.expr, out)
        }
        NodeTag::T_OpExpr => call(out, "op", node.as_op_expr().unwrap().opno, &node.as_op_expr().unwrap().args),
        NodeTag::T_DistinctExpr => call(out, "distinct", node.as_distinct_expr().unwrap().opno, &node.as_distinct_expr().unwrap().args),
        NodeTag::T_NullIfExpr => call(out, "nullif", node.as_null_if_expr().unwrap().opno, &node.as_null_if_expr().unwrap().args),
        NodeTag::T_FuncExpr => call(out, "fn", node.as_func_expr().unwrap().funcid, &node.as_func_expr().unwrap().args),
        NodeTag::T_ScalarArrayOpExpr => {
            let s = node.as_scalar_array_op_expr().unwrap();
            call(out, if s.useOr { "any" } else { "all" }, s.opno, &s.args)
        }
        NodeTag::T_BoolExpr => {
            let b = node.as_bool_expr().unwrap();
            let name = match b.boolop {
                BoolExprType::AND_EXPR => "and",
                BoolExprType::OR_EXPR => "or",
                BoolExprType::NOT_EXPR => "not",
            };
            write!(out, "{name}")?;
            args(out, &b.args)
        }
        NodeTag::T_Aggref => {
            let a = node.as_aggref().unwrap();
            write!(out, "agg{}", a.aggfnoid)?;
            if a.aggstar {
                write!(out, "*")?;
            }
            if !a.aggdistinct.is_nil() {
                write!(out, "!distinct")?;
            }
            args(out, &a.args)?;
            if let Some(f) = a.aggfilter {
                write!(out, " filter ")?;
                print(f, out)?;
            }
            Ok(())
        }
        NodeTag::T_WindowFunc => call(out, "win", node.as_window_func().unwrap().winfnoid, &node.as_window_func().unwrap().args),
        NodeTag::T_RelabelType => {
            let r = node.as_relabel_type().unwrap();
            write!(out, "relabel{}(", r.resulttype)?;
            print(r.arg, out)?;
            write!(out, ")")
        }
        NodeTag::T_CoerceViaIO => {
            let r = node.as_coerce_via_io().unwrap();
            write!(out, "io{}(", r.resulttype)?;
            print(r.arg, out)?;
            write!(out, ")")
        }
        NodeTag::T_NullTest => {
            let n = node.as_null_test().unwrap();
            write!(out, "{}(", if n.nulltesttype == NullTestType::IS_NULL { "isnull" } else { "notnull" })?;
            if let Some(a) = n.arg {
                print(a, out)?;
            }
            write!(out, ")")
        }
        NodeTag::T_CoalesceExpr => {
            write!(out, "coalesce")?;
            args(out, &node.as_coalesce_expr().unwrap().args)
        }
        NodeTag::T_MinMaxExpr => {
            write!(out, "minmax")?;
            args(out, &node.as_min_max_expr().unwrap().args)
        }
        NodeTag::T_CaseExpr => {
            let c = node.as_case_expr().unwrap();
            write!(out, "case")?;
            if let Some(a) = c.arg {
                write!(out, "[")?;
                print(a, out)?;
                write!(out, "]")?;
            }
            args(out, &c.args)?;
            if let Some(d) = c.defresult {
                write!(out, " else ")?;
                print(d, out)?;
            }
            Ok(())
        }
        NodeTag::T_CaseWhen => {
            let w = node.as_case_when().unwrap();
            write!(out, "when ")?;
            if let Some(e) = w.expr {
                print(e, out)?;
            }
            write!(out, " then ")?;
            if let Some(r) = w.result {
                print(r, out)?;
            }
            Ok(())
        }
        NodeTag::T_ArrayExpr => {
            write!(out, "array")?;
            args(out, &node.as_array_expr().unwrap().elements)
        }
        NodeTag::T_SubPlan => {
            let s = node.as_sub_plan().unwrap();
            write!(out, "subplan{}", s.plan_id)?;
            args(out, &s.args)
        }
        tag => write!(out, "{tag:?}"),
    }
}

fn call(out: &mut dyn Write, name: &str, oid: u32, a: &NodeList<'_>) -> fmt::Result {
    write!(out, "{name}{oid}")?;
    args(out, a)
}

pub fn args(out: &mut dyn Write, a: &NodeList<'_>) -> fmt::Result {
    write!(out, "(")?;
    for (i, n) in a.iter().enumerate() {
        if i > 0 {
            write!(out, ", ")?;
        }
        print(n, out)?;
    }
    write!(out, ")")
}
