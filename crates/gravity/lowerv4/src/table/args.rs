//! A keyed group's aggregate arguments, split from hashagg.rs (D368): each aggregate's arguments and FILTER compiled over
//! the producer's row (`GroupRec::compile_args`). An argument that is an outer column through any relabel is that column,
//! as step.project's `bare_var` and decompose's `strip` read it (the aggregate's collation is its inputcollid, not the
//! relabel's): `min(t COLLATE "C")` over a code lane reads the lane. class.minmax_code reads its argument as the word
//! `part << 32 | code`, which only a row column is (a code lane, or the word a step kept); an expression's value is its
//! rendered datum, so an extremum decompose gave a code register over one folds as text (`code_words`: class.text, NotBuilt
//! M6 by name until it lands), never a datum read as a code word. Budget 90 lines (files-extra.tsv, +25 % gate). Owner
//! M1-KEYED.
use gravity_abi::breaker::KeyKind;
use gravity_decompose::ops::shared::agg::{GroupForm, Register};
use mcx::GrowIn;
use types_nodes::primnodes::OUTER_VAR;

use crate::cell::{Cell, Nullable};
use crate::face::kinds::KindId;
use crate::frame::px::Px;
use crate::admit::refusal::{Reason, ShapeWhy};

use crate::table::emit_group::bare;
use crate::ops::agg::hashagg::{Agg, Arg, GroupRec};

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

impl<'g, 'p> GroupRec<'g, 'p> {
    /// Each aggregate's arguments and FILTER compiled over the producer's row at its end (the folding ends; step_percode reads
    /// a function of one code column per code). `image_only`: DISTINCT arguments read as images alone (count(DISTINCT)).
    pub fn compile_args(&mut self, px: &mut Px<'g, 'p>, image_only: bool) {
        let producer = self.g.producer.0 as usize;
        let nops = px.gx.pipelines()[producer].ops.len();
        self.compile_args_at(px, image_only, (producer, nops));
    }

    /// `compile_args` over the row at op `nops` of pipeline `producer` (D1071: a streamed group's op, keyed/stream.rs).
    pub fn compile_args_at(&mut self, px: &mut Px<'g, 'p>, image_only: bool, (producer, nops): (usize, usize)) {
        let (node, gx) = (self.node, px.gx); // D1371: PG's transfn / finalfn order (src/regroup.rs `call_order`)
        if let Err(why) = crate::ops::group::regroup::call_order(px, self.g, (producer, nops), &|n| crate::effects::raise_free(gx, n)) { shape(node, why) }
        // G11: under a pair FILTER the argument is evaluated per row where the FILTER passes (`GroupRec::key_vals`), not per code
        let filtered = self.g.form == GroupForm::Distinct && crate::table::rec::pair_filter(self.g, node).is_some();
        let tagged = crate::ops::shared::agg::tagged_multi(self.g);
        for (i, (s, k)) in self.ksrc.iter_mut().zip(self.g.keys.iter()).enumerate().filter(|(_, (s, _))| s.is_none()) {
            let e = k.expr.node();
            let cell = Cell::of_expr(crate::step::project::out_of(px.gx, e, node), 0, 0, Nullable::Maybe);
            let filtered = filtered || (tagged && k.resno == 0 && crate::ops::shared::agg::tag_filter(self.g, i).is_some());
            *s = Some(if image_only && k.kind == KeyKind::Fp128 && !filtered {
                Arg::Key { id: crate::dict::strategies::percode_key::compile_rw(px, producer, nops, e, k.coll, node, (producer, nops)), cell }
            } else {
                Arg::Ex { id: crate::step::expr::compile_key(px, (producer, nops), e, node), cell } // P1c: its forwarded path lanes
            });
        }
        for a in self.aggs.iter_mut() {
            use crate::effects::visible::agg::order_free;
            let ordered = a.d.mods.order != 0 && !order_free(&a.d, false) && !crate::ops::shared::agg::sorted(&a.d);
            if ordered && !order_free(&a.d, true) {
                // D346 / D458: no keyed end folds in order; an order-free ORDER BY (V4R-178) folds as if it had none, a
                // host-sorted DISTINCT's (D1210) is a prefix of its DISTINCT clause, the sort its final makes
                shape(node, "runs.group D458: an ordered aggregate (gravity_expr's aggorder flag)");
            }
            let r = a.d.node.as_aggref().unwrap_or_else(|| crate::refuse!(node, Reason::Bug { payload: "an aggregate that is not an Aggref" }));
            // aggregates#251: an order-free call's ORDER BY entries that are not bare, evaluated per row and dropped (rec.rs)
            let keys = r.args.iter().filter_map(|t| t.as_target_entry()).filter(|t| ordered && t.resjunk && t.expr.as_var().is_none() && t.expr.as_const().is_none());
            self.discard.extend_in(keys.map(|t| (r.aggno, crate::step::expr::compile_at(px, producer, nops, t.expr, None, node))));
            if !ordered && crate::ops::agg::numeric::strategies::num_keyed::compile(&mut self.nums, a, px, producer, nops, node, image_only) {
                continue; // V4R-38: a num.window tree in place of the datum argument
            }
            let sort_cols = crate::ops::shared::agg::sort_cols(&a.d); // D458: the host sorts by the ORDER BY's resjunk columns too
            for te in r.args.iter().filter(|&t| sort_cols || crate::ops::agg::trans::args::read_arg(t)) {
                let e = te.as_target_entry().map_or(te, |t| t.expr);
                a.args.push(match bare(e).as_var() {
                    Some(v) if v.varno == OUTER_VAR => Arg::Col(v.varattno),
                    _ => {
                        // T7d: an argument FACTS has no output for (an XML / SQL-value form) is its type's datum for the row
                        let out = crate::step::project::out_of(px.gx, e, node);
                        Arg::Ex { id: crate::step::expr::compile_at(px, producer, nops, e, None, node), cell: Cell::of_expr(out, 0, 0, Nullable::Maybe) }
                    }
                }); // alloc: Program
            }
            code_words(node, a, px.mcx());
            a.filter = a.d.filter.map(|f| crate::step::expr::compile_at(px, producer, nops, f, None, node));
        }
        // V4R-367: the pair aggregates' one FILTER over the producer's row, as a rider's (hashagg_rec.rs `pair_filter`)
        if self.g.form == GroupForm::Distinct {
            self.pair_filter = crate::table::rec::pair_filter(self.g, node).map(|f| crate::step::expr::compile_at(px, producer, nops, f, None, node));
            // the tagged form's per-key FILTERs (ops/shared/agg.rs `tag_filter`), as the pair FILTER is
            self.key_filters.clear();
            for (i, k) in self.g.keys.iter().enumerate().filter(|_| tagged) {
                if let Some(f) = crate::ops::shared::agg::tag_filter(self.g, i).filter(|_| k.resno == 0) {
                    self.key_filters.push((i, crate::step::expr::compile_at(px, producer, nops, f, None, node)));
                }
            }
        }
    }
}

impl<'g, 'p> GroupRec<'g, 'p> {
    /// V4R-131 (keyed.distinct_grouped): the record a group's emit reads: a grouped DISTINCT group's owner accumulator
    /// (keyed/pairs_view.rs); every other group's record is its own.
    pub fn emit_view(self, px: &mut Px<'g, 'p>) -> GroupRec<'g, 'p> {
        if self.g.form != GroupForm::Distinct || !crate::graph::grouped(self.g) {
            return self;
        }
        crate::frame::pairs_view::view(self, px)
    }
}

/// Whether every argument is a code word class.minmax_code may read: a row column (a code lane, or the word a step kept),
/// never an expression's value (its rendered datum) or a DISTINCT image. Exhaustive: a new argument form decides here.
fn words(args: &[Arg]) -> bool {
    args.iter().all(|a| match a {
        Arg::Col(_) => true,
        Arg::Ex { .. } | Arg::Key { .. } => false,
    })
}

/// D368: an extremum decompose gave a code register over arguments that are not code words folds as text: its register the
/// text one (so every `class_kind` reader agrees: the step, the merge, the image cursor), its class class.text's.
fn code_words(node: i32, a: &mut Agg<'_>, mcx: mcx::Mcx<'_>) {
    if crate::props::class_kind(&a.d) != KindId::ClassMinmaxCode || words(&a.args) {
        return;
    }
    a.d.reg = if a.d.reg == Register::MinCode { Register::MinVar } else { Register::MaxVar };
    a.class = crate::ops::agg::trans::plain::class_of(&node, &a.d, mcx);
    a.shape = a.class.shape(&a.d);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::At;
    use crate::frame::px::ExId;
    use gravity_decompose::ops::shared::agg::fold_of;
    use gravity_decompose::ops::shared::agg::{AggDecl, NONE};
    use gravity_decompose::ops::shared::sort::NO_SCALE;
    use gravity_decompose::ops::shared::agg_class::{Class, Ext, Mods, Split};
    use gravity_decompose::catalog::typefacts::KeyColl;
    use gravity_program::{DictRef, ExprLife, ExprOut, Rep};
    use mcx::MemoryContext;
    use types_nodes::primnodes::Aggref;
    use types_nodes::Node;

    /// D368: min / max(text) that decompose gave a code register (its argument a code lane through a relabel) keeps
    /// class.minmax_code over a row column; over an expression (its cell a Code by its output, its value the rendered
    /// datum: the XX000 "dictionary lookup outside the plan") or a DISTINCT image it folds as text, its register the text
    /// one, class.text (built at M6, the hosted fold).
    #[test]
    fn a_code_extremum_over_an_expression_folds_as_text() {
        let ctx = MemoryContext::new("d368");
        let coll = KeyColl { oid: 950, bytes_eq: true, bytes_order: true };
        let mods = Mods { distinct: 0, order: 0, filter: false, direct: 0, split: Split::Simple, star: false };
        let cell = Cell::of_expr(ExprOut { rep: Rep::Code { dict: DictRef::lane(0) }, typid: 25, life: ExprLife::Row }, 0, 0, Nullable::Maybe);
        assert!(matches!(cell.at, At::Value(_)), "an expression's cell is off its lane: `word` would read its value as the word");
        for (class, fnoid, var) in [(Class::Min(Ext::Text), 2145, Register::MinVar), (Class::Max(Ext::Text), 2129, Register::MaxVar)] {
            let (reg, trans, final_) = fold_of(class, fnoid, true, true);
            let node = Node::mk(ctx.mcx(), Aggref { aggfnoid: fnoid, aggtype: 25, ..Default::default() }).unwrap();
            let d = AggDecl { aggno: 0, fnoid, reg, slot: 1, count_slot: NONE, final_, node, scale: NO_SCALE, filter: None, class, trans, mods, coll, dscale_slot: NONE, dscales: &[] };
            let agg = |args: Vec<Arg>| {
                let c = crate::ops::agg::trans::plain::class_of(&0i32, &d, ctx.mcx());
                Agg { d, class: c, shape: c.shape(&d), args, filter: None }
            };
            let mut col = agg(vec![Arg::Col(2)]);
            code_words(3, &mut col, ctx.mcx());
            assert_eq!((crate::props::class_kind(&col.d), col.d.reg), (KindId::ClassMinmaxCode, reg), "a row column stays a code");
            for arg in [Arg::Ex { id: ExId(0), cell }, Arg::Key { id: ExId(1), cell }] {
                let mut ex = agg(vec![arg]);
                code_words(3, &mut ex, ctx.mcx());
                assert_eq!((crate::props::class_kind(&ex.d), ex.d.reg), (KindId::ClassText, var), "the text register");
            }
        }
    }
}
