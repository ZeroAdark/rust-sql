//! PostgreSQL's `exprType` / `exprCollation` / `exprTypmod` of a plan expression (nodes_core's port): the catalog home of
//! an expression's type, collation and typmod.
use types_core::Oid;
use types_nodes::primnodes::SubLinkType;
use types_nodes::{Node, NodeTag};

/// PostgreSQL's `exprType` (nodes_core's port) of an expression; a TargetEntry answers its expression's. `None` for a
/// node that is not an expression, and for an ARRAY sublink whose element type has no builtin array type (the promotion
/// reads the catalog-free table, D1045).
pub fn type_of(node: Node<'_>) -> Option<u32> {
    crate::walk::stack_check(); // D3748
    match node.node_tag() {
        NodeTag::T_TargetEntry => type_of(node.as_target_entry()?.expr),
        NodeTag::T_AlternativeSubPlan => type_of(node.as_alternative_sub_plan()?.subplans.iter().next()?),
        NodeTag::T_SubPlan if node.as_sub_plan()?.subLinkType == SubLinkType::ARRAY_SUBLINK => {
            crate::effects::builtin_promoted_array(node.as_sub_plan()?.firstColType)
        }
        t => is_expr(t).then(|| nodes_core::expr_type(node)),
    }
}

/// The type of an expression of the tags listed here (a column, a constant, a parameter, a function or operator call, a
/// relabel or I/O coercion, CASE, COALESCE, GREATEST / LEAST, an aggregate, a field select, a subscript); `None` for any
/// other tag, which its caller reads as an unknown type. Narrower than `type_of` on purpose: a caller that keys a
/// decision on `None` keeps its answers.
pub fn type_of_listed(e: Node<'_>) -> Option<u32> {
    Some(match e.node_tag() {
        NodeTag::T_Var => e.as_var()?.vartype,
        NodeTag::T_Const => e.as_const()?.consttype,
        NodeTag::T_Param => e.as_param()?.paramtype,
        NodeTag::T_FuncExpr => e.as_func_expr()?.funcresulttype,
        NodeTag::T_OpExpr => e.as_op_expr()?.opresulttype,
        NodeTag::T_RelabelType => e.as_relabel_type()?.resulttype,
        NodeTag::T_CoerceViaIO => e.as_coerce_via_io()?.resulttype,
        NodeTag::T_CaseExpr => e.as_case_expr()?.casetype,
        NodeTag::T_CoalesceExpr => e.as_coalesce_expr()?.coalescetype,
        NodeTag::T_MinMaxExpr => e.as_min_max_expr()?.minmaxtype,
        NodeTag::T_Aggref => e.as_aggref()?.aggtype,
        NodeTag::T_FieldSelect => e.as_field_select()?.resulttype,
        NodeTag::T_SubscriptingRef => e.as_subscripting_ref()?.refrestype,
        _ => return None,
    })
}

/// PostgreSQL's `exprCollation` (nodes_core's port) of an expression: the collation of an output column
/// (`OutCol::collation`, the created column's `attcollation` under CREATE TABLE AS); 0 for a node that is not an
/// expression. A TargetEntry answers its expression's.
pub fn collation_of(node: Node<'_>) -> u32 {
    crate::walk::stack_check(); // D3748
    match node.node_tag() {
        NodeTag::T_TargetEntry => node.as_target_entry().map(|t| collation_of(t.expr)).unwrap_or(0),
        t if is_expr(t) => nodes_core::expr_collation(node),
        _ => 0,
    }
}

/// PostgreSQL's `exprTypmod` (nodes_core's port) of an expression: the typmod of an output column (ExecTypeFromTL's
/// `atttypmod`); -1 where the node names none. A TargetEntry answers its expression's.
pub fn typmod_of(node: Node<'_>) -> i32 {
    crate::walk::stack_check(); // D3748
    match node.as_target_entry() {
        Some(t) => typmod_of(t.expr),
        None => nodes_core::expr_typmod(node),
    }
}

/// The expression tags nodes_core's `exprType` / `exprCollation` answer (every other tag is PG's unrecognized-node
/// error there).
fn is_expr(t: NodeTag) -> bool {
    use NodeTag::*;
    matches!(
        t,
        T_Var | T_Const | T_Param | T_Aggref | T_GroupingFunc | T_WindowFunc | T_MergeSupportFunc | T_SubscriptingRef
            | T_FuncExpr | T_NamedArgExpr | T_OpExpr | T_DistinctExpr | T_NullIfExpr | T_ScalarArrayOpExpr | T_BoolExpr
            | T_SubPlan | T_AlternativeSubPlan | T_FieldSelect | T_FieldStore | T_RelabelType | T_CoerceViaIO
            | T_ArrayCoerceExpr | T_ConvertRowtypeExpr | T_CollateExpr | T_CaseExpr | T_CaseTestExpr | T_ArrayExpr
            | T_RowExpr | T_RowCompareExpr | T_CoalesceExpr | T_MinMaxExpr | T_SQLValueFunction | T_XmlExpr
            | T_JsonValueExpr | T_JsonConstructorExpr | T_JsonIsPredicate | T_JsonExpr | T_JsonBehavior | T_NullTest
            | T_BooleanTest | T_CoerceToDomain | T_CoerceToDomainValue | T_SetToDefault | T_CurrentOfExpr
            | T_NextValueExpr | T_InferenceElem | T_ReturningExpr | T_PlaceHolderVar
    )
}

pub fn result_type(tree: Node<'_>) -> Oid {
    nodes_core::expr_type(tree)
}
