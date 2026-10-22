//! dict's premises (v5 landing 4, moved from props/code.rs): whether an expression keys as a code over a relation
//! source's code lane (a code cell). The props walk calls `code_cell` / `code_expr`.
use gravity_decompose::ops::shared::agg::GroupDecl;
use gravity_decompose::PipelineNode;
use gravity_program::{LaneId, Rep};

use types_nodes::list::NodeList;
use types_nodes::primnodes::OUTER_VAR;
use types_nodes::{Node, NodeTag};

use crate::props::code::{entry, last_project, projections_under};
use crate::props::Walk;

impl<'p> Walk<'_, '_, 'p> {
    /// Whether `e` is a code cell of pipeline `p`: it reads exactly one lane of `p`'s relation source, that lane is a code
    /// lane, and no call in it is VOLATILE (evaluable once per code, so it hashes and orders as a code); or, on a pipeline
    /// draining `group`, it is the group's output column of a key that is one.
    pub(crate) fn code_cell(&self, p: &PipelineNode<'p>, group: Option<&GroupDecl<'p>>, e: Node<'p>) -> bool {
        let Some(g) = group else { return self.code_expr(p, e, None) };
        let Some(v) = e.as_var().filter(|v| v.varno == OUTER_VAR) else { return false };
        let producer = self.gx.pipelines().get(g.producer.0 as usize);
        g.keys.iter().find(|k| k.resno != 0 && k.resno == v.varattno).is_some_and(|k| producer.is_some_and(|q| self.code_expr(q, k.expr.node(), last_project(q))))
    }

    /// Whether `e` keys as a code over `p`'s relation source (per code evaluable): it reads exactly one code lane, only
    /// as a value; a word lane only inside a CASE condition (`key.codeif`: `CASE WHEN w = 0 THEN code ELSE '' END`); no
    /// other lane, no VOLATILE call, no aggregate, window function, SubPlan or Param. `url || id::text` is not.
    /// `outer` is `p`'s last projection when `e` is a group key over it (a key decompose left over `OUTER_VAR`s, e.g.
    /// `regexp_replace(outer.2, ..)`): an `OUTER_VAR` reads that projection's entry, and an `OUTER_VAR` inside an entry
    /// reads the projection before it (`projections_under`: a join's output over its probe side's columns).
    pub(crate) fn code_expr(&self, p: &PipelineNode<'p>, e: Node<'p>, outer: Option<&'p NodeList<'p>>) -> bool {
        let Some((rti, _, site)) = p.source.relation() else { return false };
        let mcx = self.gx.mcx;
        let projs = outer.map_or_else(|| mcx::PgVec::new_in(mcx), |l| projections_under(p, l, mcx));
        let level = projs.len();
        let mut w = CodeWalk { facts: self.gx.facts, site, rti: rti as i32, projs, level, code: None, ok: true };
        w.walk(e, false);
        w.ok && w.code.is_some()
    }
}

/// `Walk::code_expr`'s walk: `cond` is set inside a CASE condition (a WHEN test, a simple CASE's argument); an
/// `OUTER_VAR` reads the entry of `projs[level - 1]` (none left at level 0: it fails), walked at `level - 1`.
struct CodeWalk<'c, 'p> {
    facts: &'c dyn gravity_decompose::Facts,
    site: &'c gravity_program::SiteDesc,
    rti: i32,
    projs: mcx::PgVec<'c, &'p NodeList<'p>>,
    level: usize,
    code: Option<i16>,
    ok: bool,
}

impl<'p> CodeWalk<'_, 'p> {
    pub(super) fn walk(&mut self, n: Node<'p>, cond: bool) {
        crate::stack_check();
        match n.node_tag() {
            NodeTag::T_List => n.as_list().into_iter().flat_map(|l| l.iter()).for_each(|x| self.walk(x, cond)),
            NodeTag::T_SortGroupClause => {}
            NodeTag::T_CaseExpr => {
                let c = n.as_case_expr().expect("a CaseExpr");
                if let Some(a) = c.arg {
                    self.walk(a, true);
                }
                c.args.iter().for_each(|w| self.walk(w, cond));
                if let Some(d) = c.defresult {
                    self.walk(d, cond);
                }
            }
            NodeTag::T_CaseWhen => {
                let w = n.as_case_when().expect("a CaseWhen");
                if let Some(t) = w.expr {
                    self.walk(t, true);
                }
                if let Some(r) = w.result {
                    self.walk(r, cond);
                }
            }
            NodeTag::T_Var => {
                let v = n.as_var().expect("a Var");
                if v.varno == OUTER_VAR && v.varlevelsup == 0 && self.level > 0 {
                    match entry(self.projs[self.level - 1], v.varattno) {
                        Some(x) => {
                            self.level -= 1;
                            self.walk(x, cond);
                            self.level += 1;
                        }
                        None => self.ok = false,
                    }
                    return;
                }
                self.ok &= v.varno == self.rti && v.varlevelsup == 0;
                match self.site.lanes.iter().find(|l| l.id == LaneId::Attr(v.varattno)).map(|l| l.rep) {
                    Some(Rep::Code { .. }) if !cond && self.code.is_none_or(|a| a == v.varattno) => self.code = Some(v.varattno),
                    Some(Rep::Word { .. }) if cond => {}
                    _ => self.ok = false,
                }
            }
            NodeTag::T_Aggref | NodeTag::T_WindowFunc | NodeTag::T_SubPlan | NodeTag::T_Param => self.ok = false,
            _ => {
                if let Some(f) = n.as_func_expr() {
                    self.ok &= !crate::effects::visible::dict::call_acts(self.facts, f.funcid);
                }
                gravity_decompose::expr::kids(n, &mut |k| self.walk(k, cond));
            }
        }
    }
}
