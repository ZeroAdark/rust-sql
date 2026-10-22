//! Props: the walk over a pipeline's source and its ops (props/mod.rs; split from props.rs, main 09-24).
use gravity_abi::breaker::KeyKind;
use gravity_decompose::ops::shared::agg::GroupDecl;
use gravity_decompose::catalog::keysem::image::class_image;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::sort::SortForm;
use gravity_decompose::ops::shared::srf::SrfKind;
use gravity_decompose::{Op, PipelineNode};
use types_nodes::list::NodeList;
use types_nodes::NodeTag;

use crate::applies::Prop;
use crate::face::kinds::KindId;
use crate::ops::sort::shapes::lane_key;
use crate::rep::{key_kind, Accessor};
use crate::step::premise::json_call;

use super::code::*;
use super::props::*;

impl<'p> Walk<'_, '_, 'p> {
    /// One op; `input` is the projection before it on the pipeline (a sort's target list reads it through `OUTER_VAR`).
    pub(super) fn op(&mut self, op: &Op<'p>, p: &PipelineNode<'p>, group: Option<&GroupDecl<'p>>, input: Option<&'p NodeList<'p>>) {
        let packed = self.p.has(Prop::Packed);
        match op {
            Op::Filter(n) => {
                self.p.add(if group.is_some() { Prop::Having } else { Prop::Filter });
                self.expr(*n);
            }
            Op::Gate(n) => self.expr(*n),
            Op::Project(l) => self.list(l),
            Op::Sort(d) => {
                if matches!(d.form, SortForm::Walk { .. }) {
                    self.p.add(Prop::Walk);
                }
                // V4R-118: the sort roads' shapes (props/sort.rs), which runs/keylane.rs reads too
                if lane_key(d, group) {
                    self.p.add(Prop::LaneKey);
                }
                // V4R-126: road topk.fetch's shape (props/topk.rs), which runs/fetch.rs reads too
                // a nest inner's pipeline is outside the graph's list: decided there, filed as no guard (key.rs `decide`)
                let at = self.gx.pipelines().iter().position(|q| std::ptr::eq(q, p));
                if topk_wide(at.unwrap_or(usize::MAX), p, d, input, self.gx.mcx) {
                    self.p.add(Prop::TopkWide);
                }
                let agg_sort = at.is_some_and(|k| self.gx.end_need(k) == KindId::RunsTopkGroups || !crate::frame::iface::run_holds_rows(self.gx, k));
                for key in d.keys.iter() {
                    if key.kind == KeyKind::Fp128 {
                        self.p.add(Prop::FpSortkey);
                    }
                    let e = entry(d.tlist, key.resno).map(|e| through(e, input));
                    // an aggregate final orders through its class's final image (runs/topk_groups.rs sort_words): no Rep arm
                    // for a word / exact-avg / text final (D358), else its order kind; an expression over finals is a column.
                    // D771: only where the group's emit sorts its records (runs.topk_groups, form G); any other sort orders
                    // the final's cell through its Rep arm (runs/sort.rs), as a column
                    if let (Some(g), Some(a)) = (group.filter(|_| agg_sort), e.and_then(|e| e.as_aggref())) {
                        if let Some(k) = g.aggs.iter().find(|d| d.node.as_aggref().is_some_and(|b| b.aggno == a.aggno)).and_then(final_order) {
                            self.add(k);
                        }
                        continue;
                    }
                    let code = e.is_some_and(|e| self.code_cell(p, group, e));
                    // a code key orders through rep.code.order: a full sort by its render's prefix word and the render as its
                    // tail across parts (M6 L0-a; V4R-118: never by in-part rank), a presorted or group sort as before
                    let code_arm = KindId::RepCodeOrder;
                    // O3 (M3 decision 16): a `name` key under a collation that orders by bytes is rep.word.order's 64-byte image;
                    // any other FixedPtr key orders through its arm (fixed_order, V4R-138)
                    let name = key.is_name() && key.coll.bytes_order;
                    let order = fixed_order(self.gx.facts, e).unwrap_or_else(|| key_kind(key.kind, Some(key.image), packed, Accessor::Order));
                    self.add(if code { code_arm } else if name { KindId::RepWordOrder } else { order });
                }
                if let Some(e) = d.keys.first().and_then(|k| entry(d.tlist, k.resno)).map(|e| through(e, input)) {
                    if matches!(e.node_tag(), NodeTag::T_FuncExpr | NodeTag::T_OpExpr) {
                        self.p.add(Prop::OrderExpr);
                    }
                }
                // road owner.count's shape (count.rs `count_cut`, V4R-113): the count-ordered top the owner's cut serves
                if group.is_some_and(|g| count_cut(self.gx, p, g, d, input)) {
                    self.p.add(Prop::CountOrder);
                }
                self.list(d.tlist);
            }
            Op::Join(JoinDesc::Probe { clauses, qual, decls, .. }) => {
                for dcl in decls.iter() {
                    let image = dcl.key.image.map(class_image).or_else(|| self.image_of(dcl.key.types.0));
                    self.add(key_kind(dcl.key.kind.unwrap_or(KeyKind::Fp128), image, packed, Accessor::Hash));
                }
                self.list(clauses);
                self.list(qual);
            }
            Op::Join(j @ JoinDesc::Merge { .. }) => self.mergejoin(j),
            Op::Join(j @ JoinDesc::Nest { .. }) => self.nestloop(j),
            Op::Join(j @ JoinDesc::Memo { .. }) => self.memoize(j),
            Op::SetOp(d) => self.setop(d, input),
            Op::Window(d) => self.window(d, input),
            Op::SubPlan(s) => self.subplan(s),
            Op::Append(a) => self.append(a),
            Op::Write(w) => self.write(w),
            Op::Srf(s) => {
                // V4R-233 (M5 Decision 23): a SQL / PL body (a nested statement through SPI) by its language, in FROM and in
                // the target list; a language the catalog cannot give (the rig's user functions) by the OID
                if s.functions.iter().any(|f| crate::effects::visible::core_face::pl_body(self.gx.catalog, f.funcid)) {
                    self.add(KindId::SrfPl);
                }
                // a native SRF kind is the source's own loop; only a scalar / other function is a hosted call
                for f in s.functions.iter() {
                    if json_call(self.gx.catalog, f.node) {
                        self.p.add(Prop::Jsonb);
                    }
                    match f.kind {
                        SrfKind::Scalar | SrfKind::Other | SrfKind::Expr => self.expr(f.node),
                        _ => self.list(f.args),
                    }
                }
                self.list(s.tlist);
            }
            Op::FirstOfRun(t) => self.first_of_run(t),
            // D1071: the streamed group's keys and aggregates are its declaration's (props.rs walks `streams`)
            Op::Limit { .. } | Op::Join(_) | Op::Lock(_) | Op::GroupRun(_) => {}
        }
    }
}

/// The order kind a top over groups needs for an aggregate final as its sort key (main 09-24): none for the finals
/// topk_groups.rs orders at M1 (a count, an i64 sum, an exact integer avg, an integer / text min / max: D358); float,
/// money, bool and bit finals are word orders (M3); numeric finals packed (M6); interval fixed; bytea / bit strings
/// varlena; collected, user, ordered-set and opaque finals hosted (M6).
fn final_order(a: &gravity_decompose::ops::shared::agg::AggDecl<'_>) -> Option<KindId> {
    use gravity_decompose::ops::shared::agg_class::{Class, Ext};
    Some(match a.class {
        Class::CountStar | Class::CountAny | Class::CountPair | Class::SumI64 | Class::AvgI128 => return None,
        Class::Min(Ext::Int | Ext::U64 | Ext::Text) | Class::Max(Ext::Int | Ext::U64 | Ext::Text) => return None,
        Class::SumF32 | Class::SumF64 | Class::AvgF64 | Class::Stat3F64(_) | Class::Stat6F64(_) | Class::SumMoney => KindId::RepWordOrder,
        Class::BoolAnd | Class::BoolOr | Class::BitI64(_) | Class::Min(Ext::F64 | Ext::Tid) | Class::Max(Ext::F64 | Ext::Tid) => KindId::RepWordOrder,
        Class::SumI128 | Class::SumNum | Class::AvgNum | Class::Stat3I128(_) | Class::Stat3Num(_) => KindId::RepPackedOrder,
        Class::Min(Ext::Num) | Class::Max(Ext::Num) => KindId::RepPackedOrder,
        Class::SumInterval | Class::AvgInterval => KindId::RepFixedOrder,
        Class::Min(Ext::Bytes) | Class::Max(Ext::Bytes) | Class::BitVar(_) => KindId::RepVarlenaOrder,
        Class::Collect(_) | Class::User | Class::OrderedSet(_) | Class::Hypothetical(_) => KindId::RepHostedOrder,
        Class::Min(Ext::Opaque) | Class::Max(Ext::Opaque) => KindId::RepHostedOrder,
    })
}

/// A sort key over a by-reference fixed-width type (macaddr, macaddr8, name, point; not tid, whose ctid lane is a word:
/// LaneDesc::rowid): its cell is `FixedPtr`, so it orders through `rep::arm_kind` (rep.fixed.order for a native fixed
/// image, else rep.hosted.order.fixed, M6: no var copy for the comparator) where `key_kind` reads a word-sized key image
/// as a word (M3-MERGE's live regress run: macaddr-007 / macaddr8-020). An enum key likewise: a word whose order is
/// pg_enum.enumsortorder (V4R-84), rep.hosted.order.fixed through the same arm_kind (regress enum#63-67). An
/// inet / cidr key, a bytes image that does not order by its bytes (`rep::bytes_order`), likewise: rep.hosted.order, as
/// rep/varlena/order.rs refuses it (D387's class: props names what the lowering needs; regress inet-012).
fn fixed_order(facts: &dyn gravity_decompose::catalog::Facts, e: Option<types_nodes::Node<'_>>) -> Option<KindId> {
    let tf = gravity_decompose::catalog::typefacts::facts_of(facts, gravity_decompose::catalog::expr_type::type_of(e?)?).filter(|tf| tf.typid != TID)?;
    let rep = gravity_program::Rep::of_storage(tf.typlen, tf.byval);
    let net = matches!(rep, gravity_program::Rep::VarlenaPtr) && tf.image.is_bytes() && !crate::rep::bytes_order(&tf);
    let arm = matches!(rep, gravity_program::Rep::FixedPtr { .. }) || tf.image.is_enum() || net;
    arm.then(|| crate::rep::arm_kind(rep, Accessor::Order, &tf))
}
