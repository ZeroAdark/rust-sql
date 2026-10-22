//! step's premises (v5 landing 4, moved from props/expr.rs): an expression tree's calls (tier, name, volatility), CASE,
//! `= ANY`, whole-row references, domains and json calls. Every op premise walks its expressions through `list` / `expr`.
use gravity_expr::Catalog;
use gravity_fn::Tier;
use mcx::PgVec;
use mcx::GrowIn;
use types_nodes::list::NodeList;
use types_nodes::{Node, NodeTag};

use crate::applies::Prop;
use crate::face::kinds::KindId;

use crate::props::*;

impl<'p> Walk<'_, '_, 'p> {
    pub(crate) fn list(&mut self, l: &NodeList<'p>) {
        for n in l.iter() {
            self.expr(n);
        }
    }

    /// One expression tree: calls (tier, name, volatility), CASE, `= ANY`, whole-row references, json calls.
    pub(crate) fn expr(&mut self, n: Node<'p>) {
        let mut found: PgVec<'_, Node<'p>> = PgVec::new_in(self.gx.mcx);
        visit(n, &mut |x| found.push_in(x));
        for x in found {
            if json_call(self.gx.catalog, x) {
                self.p.add(Prop::Jsonb);
            }
            match x.node_tag() {
                NodeTag::T_CaseExpr => self.p.add(Prop::Case),
                NodeTag::T_ScalarArrayOpExpr => {
                    self.add(KindId::ExprSaop);
                    if let Some(o) = x.as_scalar_array_op_expr() {
                        self.call(o.opfuncid);
                    }
                }
                NodeTag::T_Var if x.as_var().is_some_and(|v| v.varattno == 0) => self.add(KindId::ExprWholerow),
                // a SubPlan left inside an expression (a gate, a projection): step/consts.rs refuses nest.subplan (M8)
                NodeTag::T_SubPlan | NodeTag::T_AlternativeSubPlan => self.add(KindId::NestSubplan),
                NodeTag::T_CoerceToDomain => {
                    if let Some(d) = x.as_coerce_to_domain() {
                        self.domain(d.resulttype);
                    }
                }
                NodeTag::T_FuncExpr => {
                    if let Some(f) = x.as_func_expr() {
                        self.call(f.funcid);
                    }
                }
                NodeTag::T_NextValueExpr => self.add(KindId::ExprVolatile), // nextval_internal: VOLATILE, not a FuncExpr
                NodeTag::T_OpExpr | NodeTag::T_DistinctExpr | NodeTag::T_NullIfExpr => {
                    let op = x.as_op_expr().map(|o| (o.opno, o.opfuncid)).or_else(|| x.as_distinct_expr().map(|o| (o.opno, o.opfuncid))).or_else(|| x.as_null_if_expr().map(|o| (o.opno, o.opfuncid)));
                    let (opno, fid) = op.unwrap_or((0, 0));
                    self.call(if fid != 0 { fid } else { self.gx.catalog.opcode(opno).unwrap_or(0) });
                }
                _ => {}
            }
        }
    }

    /// A call's properties: a tier B / C builtin is `hosted`; its name sets like / text_eq / length / datetime / numeric;
    /// a VOLATILE function needs `expr.volatile`. A call the catalog cannot describe is taken to be all of them.
    pub(super) fn call(&mut self, fid: u32) {
        let cat = self.gx.catalog;
        match cat.func(fid) {
            Ok(f) if f.volatile() => self.add(KindId::ExprVolatile),
            Ok(_) => {}
            Err(_) => {
                self.add(KindId::ExprVolatile);
                self.p.add(Prop::Hosted);
                return;
            }
        }
        if cat.tier(fid) != Tier::A {
            self.p.add(Prop::Hosted);
        }
        let Some(name) = cat.shim(fid).map(|s| s.name) else { return };
        let props: &[(&[&str], Prop)] = &[
            (&["textlike", "namelike", "textnlike", "namenlike", "bpcharlike", "bpcharnlike", "texticlike", "texticnlike", "nameiclike", "bpchariclike"], Prop::Like),
            (&["texteq", "textne", "bpchareq", "nameeq", "nameeqtext", "texteqname"], Prop::TextEq),
            (&["textlen", "textoctetlen", "bpcharlen", "bpcharoctetlen", "byteaoctetlen", "length_in_encoding"], Prop::Length),
        ];
        for (names, t) in props {
            if names.contains(&name) {
                self.p.add(*t);
            }
        }
        if name.contains("_trunc") || name.contains("_part") || name.starts_with("extract_") {
            self.p.add(Prop::Datetime);
        }
        if name.starts_with("numeric") {
            self.p.add(Prop::Numeric);
        }
    }

    /// A domain's CHECK constraints are compiled into the program (gravity_expr `compile_coerce_to_domain`), so a VOLATILE
    /// call in one is the step's `expr.volatile` (effects/visible/expr.rs `volatile_site`). A domain the catalog cannot describe adds
    /// nothing: the census's fixture catalog describes no domain (every CoerceToDomain would read VOLATILE, 46 regress
    /// statements the lowerer serves), and lowering compiles the checks from the real catalog and refuses a VOLATILE one.
    /// Calls, operators, IS DISTINCT FROM, NULLIF and `= ANY` are read; a CoerceViaIO's I/O procs are not walked, so the
    /// census under-places that corner (CORE 09-24).
    fn domain(&mut self, typid: u32) {
        let cat = self.gx.catalog;
        let Ok(facts) = cat.domain(typid) else { return };
        let mut volatile = false;
        for con in facts.constraints.iter() {
            if let gravity_expr::catalog::DomainConstraint::Check { expr, .. } = con {
                gravity_decompose::expr::walk(*expr, &mut |y| {
                    let op = |opno: u32, fid: u32| if fid != 0 { fid } else { cat.opcode(opno).unwrap_or(0) };
                    let fid = match y.node_tag() {
                        NodeTag::T_FuncExpr => y.as_func_expr().map_or(0, |f| f.funcid),
                        NodeTag::T_OpExpr => y.as_op_expr().map_or(0, |o| op(o.opno, o.opfuncid)),
                        NodeTag::T_DistinctExpr => y.as_distinct_expr().map_or(0, |o| op(o.opno, o.opfuncid)),
                        NodeTag::T_NullIfExpr => y.as_null_if_expr().map_or(0, |o| op(o.opno, o.opfuncid)),
                        NodeTag::T_ScalarArrayOpExpr => y.as_scalar_array_op_expr().map_or(0, |o| op(o.opno, o.opfuncid)),
                        _ => 0,
                    };
                    volatile |= fid != 0 && cat.func(fid).map_or(true, |f| f.volatile());
                    true
                });
            }
        }
        if volatile {
            self.add(KindId::ExprVolatile);
        }
    }

    /// Whether a call in `n` has a shim name containing one of `parts` (`int4mod`, `int8div` …).
    pub(crate) fn named(&self, n: Node<'p>, parts: &[&str]) -> bool {
        let mut hit = false;
        let cat = self.gx.catalog;
        gravity_decompose::expr::walk(n, &mut |x| {
            let fid = match x.node_tag() {
                NodeTag::T_OpExpr => x.as_op_expr().map_or(0, |o| if o.opfuncid != 0 { o.opfuncid } else { cat.opcode(o.opno).unwrap_or(0) }),
                NodeTag::T_FuncExpr => x.as_func_expr().map_or(0, |f| f.funcid),
                _ => 0,
            };
            hit |= fid != 0 && cat.shim(fid).is_some_and(|s| parts.iter().any(|p| s.name.contains(p)));
            true
        });
        hit
    }
}

/// aggtrans overfit OIDW-49: a json or jsonb value, a domain over one included (PostgreSQL's json_categorize_type reads
/// the base type first); a type the catalog cannot resolve counts as itself.
fn json_category(cat: &dyn Catalog<'_>, t: u32) -> bool {
    let mut typmod = -1;
    matches!(cat.base_type(t, &mut typmod).unwrap_or(t), types_core::JSONOID | types_core::JSONBOID)
}

/// Every node of an expression tree, pre-order: `expr::kids` plus a list (VALUES rows, gate quals) and a sort clause
/// (`aggorder` / `aggdistinct`), which carry no expression of their own. A node `kids` does not know is visited alone.
pub(crate) fn visit<'p>(n: Node<'p>, f: &mut dyn FnMut(Node<'p>)) {
    crate::stack_check();
    match n.node_tag() {
        NodeTag::T_List => n.as_list().into_iter().flat_map(|l| l.iter()).for_each(|x| visit(x, f)),
        NodeTag::T_SortGroupClause => {}
        _ => {
            f(n);
            gravity_decompose::expr::kids(n, &mut |k| visit(k, f));
        }
    }
}

/// A call (function, operator, aggregate, window function, subscript, SQL/JSON form) that takes or returns json / jsonb.
/// A polymorphic call (a polymorphic / `any` argument: `FuncFacts.node_arg`; COALESCE, GREATEST / LEAST) counts only
/// when it returns json: `count(j)` does not, `coalesce(j, '{}')` does. A pass-through `Var` or a typed NULL is no call.
pub(crate) fn json_call(cat: &dyn Catalog<'_>, x: Node<'_>) -> bool {
    use gravity_decompose::expr::kids;
    use gravity_decompose::catalog::expr_type::type_of;
    let json = |n: Node<'_>| type_of(n).is_some_and(|t| json_category(cat, t));
    let fid = match x.node_tag() {
        NodeTag::T_FuncExpr => x.as_func_expr().map(|f| f.funcid),
        NodeTag::T_OpExpr => x.as_op_expr().map(|o| o.opfuncid),
        NodeTag::T_DistinctExpr => x.as_distinct_expr().map(|o| o.opfuncid),
        NodeTag::T_NullIfExpr => x.as_null_if_expr().map(|o| o.opfuncid),
        NodeTag::T_ScalarArrayOpExpr => x.as_scalar_array_op_expr().map(|o| o.opfuncid),
        NodeTag::T_Aggref => x.as_aggref().map(|a| a.aggfnoid),
        NodeTag::T_WindowFunc => x.as_window_func().map(|w| w.winfnoid),
        NodeTag::T_SubscriptingRef => Some(0),
        NodeTag::T_CoalesceExpr | NodeTag::T_MinMaxExpr => return json(x),
        NodeTag::T_JsonExpr | NodeTag::T_JsonConstructorExpr | NodeTag::T_JsonIsPredicate => return true,
        _ => return false,
    };
    let polymorphic = fid.is_some_and(|f| f != 0 && cat.func(f).is_ok_and(|f| f.node_arg));
    let mut arg = false;
    kids(x, &mut |k| arg |= json(k));
    json(x) || (arg && !polymorphic)
}
