//! D472 (V4R-170): a top over groups ordered by an expression over the group's row (`ORDER BY min(f3) - 1`, a ratio of two
//! finals) orders by that expression's value, evaluated once per group into a record slot of its own. An M1-side helper
//! owned by M3-WORDS (keyed_hashagg, keyed_coderep, runs_topk_groups and keyed_emit_group call it).
//! - The slot: each such ORDER BY entry of the top that drains group `g` adds one slot at the end of `g`'s record
//!   (`kinds`): `MaxI64` for a signed-word result (`signed`), `SumF64` for a float8 one. Nothing folds into it, so the
//!   owners' init and merge leave it unset (not valid) until the emit writes it.
//! - The pass (`run`): the emit's fresh call, over every gathered record before any keep or sort, builds the group's row,
//!   evaluates the pipeline's HAVING filters, and for a group that passes evaluates each expression as compiled (a raise
//!   leaves as PostgreSQL's, which evaluates the Sort's key for every group that passes HAVING), storing its value and
//!   marking the slot valid where it is not NULL. A group HAVING drops is never evaluated, as PostgreSQL's.
//! - The order (`sort_word`): `agg_sort` reads the slot (`SK_SLOT`, `SK_FLOAT` for float8), the key's DESC and NULLS
//!   bits as any final's. The key is the expression's own value: ties are PostgreSQL's, no rewrite.
//! - A name result under a byte-ordered collation (O3) takes its eight image words (`name_image`) in eight slots, sorted
//!   word by word (`more_words`).
//! - A derived text group key (the key.codeif form: its THEN column's render or its ELSE constant, both live through the
//!   sort) takes one slot holding its render's datum (0: NULL) for every group, before HAVING (a render neither raises
//!   nor runs a volatile call), compared as text under the key's collation (`SK_TEXT | SK_HEAP`); a key over a function's
//!   result (its render the row's) is copied as a computed text result is (D1023).
//! - A sort key reaching the group's key through a subquery scan's bare-Var projections is the key itself (#227).
//! - A NULL-able expression (bundle 15 B): `wants_validity` gives a record without validity bits (decompose's, for its
//!   aggregates) a validity word, so the pass marks the slot's NULL and `agg_sort` reads it (`Rec::valid`).
//! - A text / varchar result computed over the group's row (D1325: `upper(k)`, `md5(t)`) is copied once per group that
//!   passes HAVING (keyed/key_copy.rs's pinned `key_copy`; the pass's cursor words, rewound at each fresh call) into one slot,
//!   ordered as a derived key's value under the sort key's collation.
//! - N6a: a by-reference result with a default btree comparator (avg(numeric), a variance, an interval, an inet; any
//!   final whose slot `sort_words` does not order) is copied as a computed text is (`key_copy`, or `key_copy_fixed` for a
//!   fixed image) and ordered by that comparator (`SK_CMP`: host_abi `copy_cmp`, its site byte past the group's keys).
//!   A by-value one (an enum, a bool) keeps its Datum word in its slot, valid where not NULL, ordered by the same
//!   comparator on the word (`SK_SLOT | SK_CMP`: host_abi `sk_slot_cmp`); a signed-word type (`signed`: a date, a time,
//!   a timestamp, money) orders by its word, its operator checked as the comparator road checks it.
//! - A volatile int / float8 expression (D1344) whose calls are random draws (effects `call_order_blind`; vol-0028, D2790: a
//!   sequence or a counting function numbers the groups in the pass's order, never PostgreSQL's), or any call over a hashed
//!   Agg's groups (effects `call_order_free`: PostgreSQL's order there is its hash table's, ORDER-FREE), under a top's drain or form
//!   G's full sort (`backs`): the drain reads its value back from the slot
//!   (`read_back`, the value PostgreSQL's Sort carries, computed once per group by its Agg's projection), and the top
//!   pipeline's projection leaves it uncomputed (`skipped`: nothing reads it there), so it runs once per group passing
//!   HAVING, as PG's, its raise at the pass (effects visible/topn.rs `can_raise`); a VOLATILE call count is not compared anyway.
//! - Refused by name (Shape): another result type (one with no comparator; a name under another collation), a volatile expression
//!   past a top's drain (D1344: form G's projection and a by-value top's evaluate it again past the pass, the value they
//!   show not the one sorted; PostgreSQL once per group), a count-ordered owner top or a planner two-pass top (both cut
//!   groups before the emit).
//! - D2862 (b): a full sort's drain re-projects every group the top pipeline projected for its raise; a raising column
//!   that is a word or a numeric is kept instead (`kept_cols`: topn lower.rs keeps it beside the record, in a side array
//!   one entry per arrival, so the record and the group's memory are unchanged), read back by the drain.
//! Budget 320 (files-override: the name order slot; #227's projections, the derived text key).
use gravity_abi::breaker::{KeyKind, RecordLayout, SlotKind, LAYOUT_IMPLICIT_COUNT};
use gravity_decompose::ops::shared::agg::fold_of;
use gravity_decompose::ops::shared::agg::{FinalForm, GroupDecl, GroupForm, NONE};
use gravity_decompose::ops::shared::agg_class::Class;
use gravity_decompose::ops::shared::sort::SortDesc;
use gravity_decompose::Op;
use gravity_fn::typesem::{int_word, text_payload};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value};
use gravity_kernels::host_abi::{SK_CMP, SK_FLOAT, SK_HEAP, SK_SLOT, SK_TEXT, SK_TEXT_COLL_SHIFT, SK_TEXT_SITE_SHIFT, SK_TEXT_SLOT_SHIFT};
use mcx::{CollectIn, Mcx, PgVec};
use types_nodes::primnodes::OUTER_VAR;
use types_nodes::Node;

use crate::table::emit_group::Emit;
use crate::table::rec::GroupRec;
use crate::frame::cancel;
use crate::cell::{Cell, Nullable};
use crate::frame::env::Env;
use crate::rep::CellRef;
use crate::frame::px::{ExId, Px};
use crate::frame::row::Carry;
use crate::graph::Gx;
use crate::effects::visible::topn::order_volatile;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::face::kinds::KindId;

/// The signed words (`signed`); float8; name (under a collation that orders by bytes: its eight-word image, O3). A
/// typesem `text_payload` key is a derived text group key's value (its render's datum, never a copy) in one slot,
/// compared as text.
const FLOAT8: u32 = 701;
const NAME: u32 = 19;
const NUMERIC: u32 = 1700;
/// D1325: a text / varchar result computed over the group's row (`upper(k)`, `md5(t)`): its value copied once per group
/// passing HAVING (keyed/key_copy.rs's `key_copy`, pinned chunks behind the pass's three cursor words) into one slot, compared as a
/// derived key's (`SK_TEXT | SK_HEAP` under the sort key's collation). Not a type oid: the tag `exprs` gives such a result.
const COPIED: u32 = 0;

/// N6a: a result of type `t` with a default btree comparator (its type facts', else the type cache's), ordered by that
/// comparator. Back: (comparator, copy). A by-reference value is copied once per group passing HAVING as a `COPIED`
/// text's (copy `Some(0)`: a varlena by `key_copy`; `Some(n)`: a fixed image of `n` bytes by `key_copy_fixed`) and
/// compared as a copy (`SK_CMP`, host_abi `copy_cmp`); a by-value one (copy `None`: money, an enum, a date) keeps its
/// Datum word in the slot (`SK_SLOT | SK_CMP`, host_abi `sk_slot_cmp`).
fn cmped(gx: &Gx<'_, '_>, t: u32) -> Option<(u32, Option<u16>)> {
    let tf = gravity_decompose::catalog::typefacts::facts_of(gx.facts, t).filter(|tf| tf.typlen == -1 || (tf.typlen > 0 && (!tf.byval || tf.typlen <= 8)))?;
    let cmp = match tf.cmp {
        0 => gx.catalog.type_cmp_proc(t).unwrap_or(0),
        c => c,
    };
    (cmp != 0).then_some((cmp, (!tf.byval).then_some(tf.typlen.max(0) as u16)))
}

/// Whether a result of type `t` is held as one signed word, its datum sign-extended (`narrow`) and ordered two's-complement
/// (`SK_SLOT`): typesem's signed images (int2, int4, int8, date, time, timestamp, timestamptz, money: `keysem::image_signed`;
/// a domain over one takes the comparator road).
fn signed(t: u32) -> bool {
    gravity_fn::typesem::keysem::image_signed(t)
}

/// Whether order expression type tag `t` is a copy the pass keeps (a computed text, or a by-reference `cmped` result).
fn copies(gx: &Gx<'_, '_>, t: u32) -> bool {
    t == COPIED || (!text_payload(t) && t != NAME && cmped(gx, t).is_some_and(|c| c.1.is_some()))
}

/// N6a: whether order expression type tag `t` is a by-value `cmped` result other than the slot's own signed / float8 order.
fn worded(gx: &Gx<'_, '_>, t: u32) -> bool {
    t != COPIED && !signed(t) && t != FLOAT8 && cmped(gx, t).is_some_and(|c| c.1.is_none())
}

/// The low bytes of a by-value result's word that are its Datum (`None`: all eight): an int2 / int4's, or a `worded` or
/// `signed` result's `typlen` (a date's four).
fn narrow(gx: &Gx<'_, '_>, t: u32) -> Option<Type> {
    let len = match t {
        21 => 2,
        23 => 4,
        _ if worded(gx, t) || signed(t) => gravity_decompose::catalog::typefacts::facts_of(gx.facts, t).map_or(8, |tf| tf.typlen),
        _ => 8,
    };
    match len { 1 => Some(Type::I8), 2 => Some(Type::I16), 4 => Some(Type::I32), _ => None }
}

/// The slots a result of type `t` takes: a name's eight image words, else one.
fn width(t: u32) -> usize {
    if t == NAME { 8 } else { 1 }
}

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

/// The emit's compiled pass: each expression with its first slot, its result type and its cell, and the HAVING filters.
pub(crate) struct Pass<'g> {
    /// Each expression: its compiled id, first slot, type tag, name cell, copy (`None`: not copied; `Some(0)`: a
    /// varlena; `Some(n)`: an `n`-byte fixed image) and a word's Datum bytes (`narrow`).
    exprs: PgVec<'g, (ExId, usize, u32, Option<Cell>, Option<u16>, Option<Type>)>,
    filters: PgVec<'g, ExId>,
    /// D1325: the byte offset of the three frame words (cur, end, chunk) a copied result's copies bump from.
    images: Option<i32>,
}

/// The pipeline that drains group `g` into a top (or form G's full sort): its sort and the ops before it.
fn top_of<'g, 'p>(gx: &Gx<'g, 'p>, g: &GroupDecl<'_>) -> Option<(usize, &'g SortDesc<'p>, &'g [Op<'p>])> {
    let k = gx.drainers(g).first().copied()?;
    let ops = &gx.pipelines()[k].ops;
    let j = ops.iter().position(|op| matches!(op, Op::Sort(_)))?;
    let Op::Sort(sd) = &ops[j] else { return None };
    Some((k, sd, &ops[..j]))
}

/// Whether group `g`'s records stream into a top over the groups (`runs.topk_groups`) whose sort keys are all finals:
/// then a key's representative renders only for the records the top keeps (dict/late_render.rs), never per record.
pub(crate) fn top_reads_finals(gx: &Gx<'_, '_>, g: &GroupDecl<'_>) -> bool {
    top_reads_finals_and(gx, g, |_| false)
}

/// As `top_reads_finals`, where a sort key may also be a group key's column (`OUTER_VAR`, the key's resno) that `keep`
/// passes: road key.dependent's keys that keep their image (dependent.rs `find`).
pub(crate) fn top_reads_finals_and(gx: &Gx<'_, '_>, g: &GroupDecl<'_>, keep: impl Fn(i16) -> bool) -> bool {
    let Some((k, sd, ops)) = top_of(gx, g) else { return false };
    let fin = |e: Node<'_>| e.as_aggref().is_some() || e.as_relabel_type().is_some_and(|r| r.arg.as_aggref().is_some());
    let key = |e: Node<'_>| e.as_var().is_some_and(|v| v.varno == OUTER_VAR && v.varlevelsup == 0 && g.keys.iter().any(|kd| kd.resno == v.varattno) && keep(v.varattno));
    gx.end_need(k) == KindId::RunsTopkGroups && sd.keys.iter().all(|sk| key_expr(ops, sd, sk.resno, gx.mcx).is_some_and(|e| fin(e) || key(e)))
}

/// The projections among `ops` (the ops before a sort), the nearest first.
pub(crate) fn projections<'a, 'p, 'm>(ops: &'a [Op<'p>], mcx: Mcx<'m>) -> PgVec<'m, &'a types_nodes::list::NodeList<'p>> {
    ops.iter().rev().filter_map(|op| if let Op::Project(l) = op { Some(*l) } else { None }).collect_in(mcx)
}

fn find<'p>(l: &types_nodes::list::NodeList<'p>, r: i16) -> Option<Node<'p>> {
    l.iter().filter_map(|n| n.as_target_entry()).find(|te| te.resno == r).map(|te| te.expr)
}

/// `e`, an expression over the row of `earlier[0]` (the projection before the one holding it), over the group's row: each
/// bare Var of a subquery scan's column through the projection before it (#227), stopping at the group's row (an
/// OUTER_VAR, or no earlier projection); `None` for an expression over a later projection's row (not the group's).
pub(crate) fn to_group_row<'p>(earlier: &[&types_nodes::list::NodeList<'p>], mut e: Node<'p>) -> Option<Node<'p>> {
    for l in earlier {
        match e.as_var().filter(|v| v.varlevelsup == 0)? {
            v if v.varno == OUTER_VAR => break,
            v => e = find(l, v.varattno)?,
        }
    }
    Some(e)
}

/// Sort key `resno`'s expression over the group's row: through the nearest projection when the sort reads one of its
/// columns, then `to_group_row` through the earlier ones.
pub(crate) fn key_expr<'p>(ops: &[Op<'p>], sd: &SortDesc<'p>, resno: i16, mcx: Mcx<'_>) -> Option<Node<'p>> {
    let ls = projections(ops, mcx);
    let e = find(sd.tlist, resno)?;
    let (Some(v), Some(l)) = (e.as_var().filter(|v| v.varno == OUTER_VAR), ls.first()) else { return Some(e) };
    to_group_row(&ls[1..], find(l, v.varattno)?)
}

/// Whether `e` is what runs_topk_groups' `sort_words` orders by itself: a final (or one relabeled to its own type) or a
/// group key's column that is not a derived text key.
fn plain(gx: &Gx<'_, '_>, g: &GroupDecl<'_>, e: Node<'_>) -> bool {
    let a = e.as_aggref().or_else(|| e.as_relabel_type().and_then(|r| r.arg.as_aggref().filter(|a| a.aggtype == r.resulttype)));
    a.is_some_and(|a| native(gx, g, a)) || e.as_var().is_some_and(|v| v.varno == OUTER_VAR && g.keys.iter().any(|k| k.resno == v.varattno && !derived(k)))
}

/// D1519: whether runs_topk_groups orders final `a` of `g` itself (its own slot, or a count-ordered owner top's: props
/// `count_cut`), or one of a type with no order slot; any other (a hosted state, a DISTINCT pair fold) orders here.
pub(crate) fn native(gx: &Gx<'_, '_>, g: &GroupDecl<'_>, a: &types_nodes::primnodes::Aggref<'_>) -> bool {
    let cut = || top_of(gx, g).is_some_and(|(k, sd, ops)| crate::props::count_cut(gx, &gx.pipelines()[k], g, sd, projections(ops, gx.mcx).first().copied()));
    let d = g.aggs.iter().find(|d| d.aggno as i32 == a.aggno);
    if int_word(a.aggtype) || a.aggtype == FLOAT8 || text_payload(a.aggtype) {
        return d.is_some_and(|d| d.slot != NONE) || cut();
    }
    // N6a: a final of another type is ordered by its slot only when sort_words has an arm for that slot; N13: a keyed
    // numeric extreme's key word is not its value where its side copy holds the extreme (its copy orders, SK_CMP)
    let keyed = |d: &&gravity_decompose::ops::shared::agg::AggDecl<'_>| matches!(d.reg, gravity_decompose::ops::shared::agg::Register::MinNumDyn | gravity_decompose::ops::shared::agg::Register::MaxNumDyn);
    let ordered = d.filter(|d| !keyed(d)).is_some_and(|d| d.slot != NONE && g.layout.slots.get(d.slot as usize).is_some_and(|&k| crate::ops::sort::topn::lower::final_ordered(k, d.class)));
    ordered || cut()
}

/// A derived text group key (its expression over its code column's render, an `Fp128` key that is not a bare column).
fn derived(k: &gravity_decompose::ops::shared::agg::KeyDecl<'_>) -> bool {
    (k.kind == KeyKind::Fp128 && k.expr.node().as_var().is_none()) || k.int_text() // D3080: rendered at the emit too
}

/// Group `g`'s order-slot expressions, by sort key: `(resno, expression, result type)`: signed, float8, name, text, and a
/// by-reference type with a comparator (`cmped`, N6a).
fn exprs<'g, 'p>(gx: &Gx<'g, 'p>, g: &GroupDecl<'_>) -> PgVec<'g, (i16, Node<'p>, u32)> {
    let Some((_, sd, ops)) = top_of(gx, g) else { return PgVec::new_in(gx.mcx) };
    sd.keys.iter().filter_map(|key| {
        let e = key_expr(ops, sd, key.resno, gx.mcx).filter(|&e| !plain(gx, g, e))?;
        let t = gravity_decompose::catalog::expr_type::type_of(e)?;
        let text = text_payload(t) && e.as_var().is_some_and(|v| v.varno == OUTER_VAR); // a derived key (`plain` passed it)
        if text_payload(t) && e.as_var().is_none() && e.as_const().is_none() {
            return Some((key.resno, e, COPIED)); // D1325
        }
        (signed(t) || t == FLOAT8 || (t == NAME && key.coll.bytes_order) || text || copies(gx, t) || worded(gx, t)).then_some((key.resno, e, t))
    }).collect_in(gx.mcx)
}

/// The slots group `g`'s record adds for its top's order expressions (hashagg's layout, before an implicit sentinel).
pub(crate) fn kinds<'g>(gx: &Gx<'g, '_>, g: &GroupDecl<'_>) -> PgVec<'g, SlotKind> {
    exprs(gx, g).iter().flat_map(|x| core::iter::repeat_n(if x.2 == FLOAT8 { SlotKind::SumF64 } else { SlotKind::MaxI64 }, width(x.2))).collect_in(gx.mcx)
}

/// D2862 (b): the columns of the projection before a full sort over group `g` (form G, or a top's drain) that the top
/// pipeline runs for their raise (D449: an entry `can_raise` does not read quiet) and the drain would run again:
/// `(resno, type)`, an int or float8 word or a numeric. The top (topn lower.rs `TopEnd`) keeps each value in its arrival's
/// entry of a side array (a numeric copied) and the drain reads it back (`Out::Kept`), so each runs once per group, as
/// PostgreSQL's Agg projects it once before its Sort. Not kept: a column, a constant, a final (`finals_of` builds every
/// final for the drain's row anyway), a VOLATILE or user call (its calls stay as they are), a D1344 read-back, and any
/// column under a HAVING, a bound, a planner two-pass, a grouped DISTINCT's emit view, grouping sets or past 63 columns.
pub(crate) fn kept_cols<'g>(gx: &Gx<'g, '_>, g: &GroupDecl<'_>) -> PgVec<'g, (i16, u32)> {
    let Some((k, sd, ops)) = top_of(gx, g) else { return PgVec::new_in(gx.mcx) };
    if !matches!(sd.form, gravity_decompose::ops::shared::sort::SortForm::Runs) || g.two_pass.is_some() || g.form == GroupForm::Distinct || g.set.is_some() || !drains(gx, k) || !ops.iter().all(|op| matches!(op, Op::Project(_))) {
        return PgVec::new_in(gx.mcx);
    }
    let ls = projections(ops, gx.mcx);
    let Some(near) = ls.first() else { return PgVec::new_in(gx.mcx) };
    let backs = read_back_cols(gx, g);
    let kept: PgVec<'_, (i16, u32)> = near.iter().filter_map(|n| {
        let te = n.as_target_entry()?;
        let e = te.expr;
        let bare = e.as_var().is_some() || e.as_const().is_some() || e.as_aggref().is_some() || e.as_relabel_type().is_some_and(|r| r.arg.as_aggref().is_some() || r.arg.as_var().is_some());
        if bare || backs.contains(&te.resno) || crate::effects::visible::topn::quiet_entry(gx, g, n) || crate::effects::user_or_volatile(gx, e) {
            return None;
        }
        to_group_row(&ls[1..], e)?;
        let o = gx.out_of(e)?;
        let word = (int_word(o.typid) || o.typid == FLOAT8) && matches!(o.rep, gravity_program::Rep::Word { .. });
        (word || (o.typid == NUMERIC && o.rep == gravity_program::Rep::VarlenaPtr)).then_some((te.resno, o.typid))
    }).collect_in(gx.mcx);
    if kept.len() > 63 { PgVec::new_in(gx.mcx) } else { kept }
}

/// First slot of order expression `i` of `xs`: the expressions' slots are the last ones, or those before an implicit sentinel,
/// or (D1212) before the DISTINCT folds a grouped DISTINCT group's emit view appends (keyed/pairs_view.rs `view`), which
/// the order pass would otherwise overwrite with the expression's value and the sort read in its place.
fn slot(rec: &GroupRec<'_, '_>, xs: &[(i16, Node<'_>, u32)], i: usize) -> usize {
    let implicit = rec.layout.flags & LAYOUT_IMPLICIT_COUNT != 0;
    let total: usize = xs.iter().map(|x| width(x.2)).sum();
    rec.layout.nslots() - implicit as usize - view_folds(rec) - total + xs[..i].iter().map(|x| width(x.2)).sum::<usize>()
}

/// D1212: the DISTINCT fold slots after the order slots in `rec`: a grouped DISTINCT group's emit view (`GroupRec::emit_view`)
/// appends one per pair fold with a slot kind (`accumulator_folded`'s folds), the tagged form's one count slot per argument
/// past the first (ops/group/pairs_multi.rs `Tagged::acc`), any other record none.
fn view_folds(rec: &GroupRec<'_, '_>) -> usize {
    let g = rec.g;
    if g.form != GroupForm::Distinct || !crate::graph::grouped(g) {
        return 0;
    }
    if crate::ops::shared::agg::tagged_multi(g) {
        return crate::ops::shared::agg::tag_slots(g);
    }
    g.aggs.iter().filter(|d| d.final_ == FinalForm::PairFold && fold_of(d.class, d.fnoid, false, false).1.abi().is_some()).count()
}

/// Whether `e` over group `g`'s row is never NULL: a constant, a count, a key of a group with no NULL-able key, an audited
/// function or operator of such (D605: decompose catalog `never_null.rs`; a strict one alone may be NULL for non-NULL arguments).
fn never_null(gx: &Gx<'_, '_>, g: &GroupDecl<'_>, l: &RecordLayout, nullable: bool, e: Node<'_>) -> bool {
    crate::stack_check();
    let strict = |f: u32| gravity_decompose::catalog::never_null::audited(f) && gx.facts.func(f).is_some_and(|p| p.strict);
    let all = |xs: &types_nodes::list::NodeList<'_>| xs.iter().all(|a| never_null(gx, g, l, nullable, a));
    if let Some(a) = e.as_aggref() {
        // over a record without validity bits a final's NULL is its slot kind's own test (abi `Rec::valid`, D443): a kind
        // with none (a count, a word sum / min / max) is never NULL
        let Some(d) = g.aggs.iter().find(|d| d.aggno as i32 == a.aggno) else { return false };
        let own = d.slot == NONE || matches!(l.slot_kind(d.slot as usize), SlotKind::MinCode | SlotKind::MinVar | SlotKind::MaxVar | SlotKind::AvgI128 | SlotKind::AvgNum | SlotKind::AvgNumWide | SlotKind::F64Accum);
        return matches!(d.class, Class::CountStar | Class::CountAny) || !own;
    }
    match (e.as_op_expr(), e.as_func_expr(), e.as_relabel_type(), e.as_const(), e.as_var()) {
        (Some(o), ..) => strict(o.opfuncid) && all(&o.args),
        (_, Some(f), ..) => strict(f.funcid) && all(&f.args),
        (_, _, Some(r), ..) => never_null(gx, g, l, nullable, r.arg),
        (_, _, _, Some(c), _) => !c.constisnull,
        (.., Some(v)) => v.varno == OUTER_VAR && !nullable,
        _ => false,
    }
}

/// Bundle 15 B: whether group `g`'s record takes a validity word it would not have for its aggregates (decompose's `record`,
/// its slots as `GroupRec::new` rekinds them): a top ordered by a NULL-able int / float8 expression over the groups, whose
/// slot's NULL the order pass marks (`run`'s `put_valid`) and `agg_sort` reads (`sort_cell`: `Rec::valid`).
pub(crate) fn wants_validity(gx: &Gx<'_, '_>, g: &GroupDecl<'_>, record: &RecordLayout, nullable: bool) -> bool {
    !record.has_validity() && exprs(gx, g).iter().any(|x| !(text_payload(x.2) || copies(gx, x.2)) && !never_null(gx, g, record, nullable, x.1))
}

/// The sort word of the top's key `resno` that `sort_words` does not order itself (a Shape refusal when it has no slot).
/// `coll` is the sort key's own collation: two keys may name one column under two collations (D2199:
/// `ORDER BY x COLLATE ci DESC, x`), so it is the key's, never the first key's over `resno`.
pub(crate) fn sort_word(gx: &Gx<'_, '_>, rec: &GroupRec<'_, '_>, resno: i16, coll: u32, node: i32) -> u64 {
    let xs = exprs(gx, rec.g);
    let Some(i) = xs.iter().position(|x| x.0 == resno) else { shape(node, "a top ordered by an expression over the groups (not an int or float8)") };
    if rec.g.two_pass.is_some() {
        shape(node, "a planner two-pass top ordered by an expression (pass 1 cuts groups before the emit)");
    }
    // the final runs again at the emit: a read-write final's second call returns its memo (hosted.rs D1520) unless a
    // DISTINCT fold gives it a fresh state. A hosted aggregate that acts (any support function VOLATILE or unknown) is
    // refused too: a rewound SCROLL cursor rescans the top, which folds again where PostgreSQL's Sort replays its rows
    // (e2e m6_d1406_cursor_rewind_replay, ordered by the aggregate)
    let agg = xs[i].1.as_aggref().and_then(|a| rec.g.aggs.iter().find(|d| d.aggno as i32 == a.aggno));
    if agg.filter(|d| matches!(d.class, Class::User | Class::OrderedSet(_) | Class::Hypothetical(_))).is_some_and(|d| d.mods.distinct != 0 || gravity_decompose::effects::of_agg(gx.facts, d.fnoid).acts) {
        shape(node, "a top ordered by a DISTINCT or acting hosted aggregate (its final runs again at the emit, its fold on a rescan)");
    }
    if order_volatile(gx, xs[i].1) && !backs(gx, rec.g).iter().any(|b| b.0 == i) {
        // D1344: the pass evaluates it once per group; past a top's drain (`backs`) the drain reads that value back, any
        // other emit evaluates it again and shows that value, not the one sorted
        shape(node, "a top ordered by a volatile expression over the groups (evaluated again past the order pass)");
    }
    let s = slot(rec, &xs[..], i) as u64;
    let key = top_of(gx, rec.g).and_then(|(_, sd, _)| sd.keys.iter().find(|k| k.resno == resno));
    let coll = coll as u64; // the key's own collation (D2199)
    if text_payload(xs[i].2) || xs[i].2 == COPIED {
        // the render's datum (0: NULL), compared as text under the key's collation (host_abi `text_image`'s SK_HEAP arm)
        return SK_TEXT | SK_HEAP | s | s << SK_TEXT_SLOT_SHIFT | coll << SK_TEXT_COLL_SHIFT;
    }
    // N6a: a `cmped` result ordered by its type's comparator (copy_words.rs `cmp_word`'s checks); the site byte names the
    // expression past the group's keys (`expr_cmp`, read by copy_words.rs `procs`). A signed word beyond the ints orders by
    // its word under its type's own comparator's order only: the operator check below, no call
    let own = (xs[i].2 != NAME && !int_word(xs[i].2) && xs[i].2 != FLOAT8).then(|| cmped(gx, xs[i].2)).flatten();
    let cmp = own.filter(|_| !signed(xs[i].2));
    let site = rec.g.keys.len() + i;
    if let (Some((c, _)), Some(op)) = (own, key.map(|k| k.opno)) {
        match gx.catalog.ordering_cmp_proc(op) {
            Ok(p) if p == c => {}
            Ok(0) | Err(_) if gravity_decompose::catalog::opdir::builtin_dir(op).is_some() => {}
            _ => shape(node, "a top ordered by an expression over the groups under an operator class other than its type's default"),
        }
    }
    if let Some((cmp, _)) = cmp {
        if site > 0xff {
            shape(node, "a top ordered by an expression over the groups past the 256th sort site");
        }
        if let Err(why) = gravity_fn::typesem::keysem::rep::sort_cmp_site(gx.catalog.func(cmp).ok().and_then(|f| f.prolang), cmp, xs[i].2) {
            shape(node, why);
        }
    }
    if let Some((_, Some(_))) = cmp {
        return SK_TEXT | SK_HEAP | SK_CMP | s | s << SK_TEXT_SLOT_SHIFT | (site as u64) << SK_TEXT_SITE_SHIFT | coll << SK_TEXT_COLL_SHIFT;
    }
    if !rec.layout.has_validity() && !never_null(gx, rec.g, &rec.layout, rec.nullable, xs[i].1) {
        crate::refuse!(node, Reason::Bug { payload: "a top ordered by a NULL-able expression over a record without validity (wants_validity adds it)" });
    }
    if cmp.is_some() {
        return SK_SLOT | SK_CMP | s | (site as u64) << SK_TEXT_SITE_SHIFT; // the word, by its comparator (`sk_slot_cmp`)
    }
    SK_SLOT | s | if xs[i].2 == FLOAT8 { SK_FLOAT } else { 0 }
}

/// D1344 (a VOLATILE call count is not compared): group `g`'s volatile int / float8 order
/// expressions a top's drain reads back from their slots (topn lower.rs `make_drain`): `(index among the order
/// expressions, the column of the projection before the sort its key reads)`. Only a `runs.topk_groups` top or form G's
/// full sort (`drains`; no planner two-pass), and a key reading that projection's column (the drain's outputs are its entries).
fn backs<'g>(gx: &Gx<'g, '_>, g: &GroupDecl<'_>) -> PgVec<'g, (usize, i16)> {
    let Some((k, sd, ops)) = top_of(gx, g) else { return PgVec::new_in(gx.mcx) };
    if !drains(gx, k) || g.two_pass.is_some() {
        return PgVec::new_in(gx.mcx);
    }
    let Some(near) = projections(ops, gx.mcx).first().copied() else { return PgVec::new_in(gx.mcx) };
    let xs = exprs(gx, g);
    // vol-0028 / D2790: a top's drain, like form G's full sort, reads back only a key whose calls' group order shows in no
    // result (a random draw): its pass meets the groups in Gravity's table order, never PostgreSQL's hashed Agg's, so a
    // nextval key or a counting user function numbers them otherwise (D2790: `ORDER BY nextval('tqs') LIMIT 3` over seven
    // groups kept groups 6 3 0 where PostgreSQL keeps 1 5 4); any other volatile key refuses by D1344's name.
    // ORDER-FREE (effects `call_order_free`): over the groups of a hashed Agg PostgreSQL's
    // order of those calls is its hash table's, so the pass makes the same calls (one per group passing HAVING, the same
    // arguments) in its own order and reads each value back: any call there (`call_order_free`), a counting user function
    // and a sequence draw too. A sorted Agg's group order is fixed by its plan: only the random draws, as before.
    let hashed = g.form == GroupForm::Hashed && g.set.is_none();
    let blind = |e: Node<'_>| crate::effects::visible::call_order_blind(gx, e) || hashed && crate::effects::visible::call_order_free(gx, e);
    xs.iter().enumerate().filter(|(_, x)| (int_word(x.2) || x.2 == FLOAT8) && order_volatile(gx, x.1) && blind(x.1)).filter_map(|(i, x)| {
        let v = find(sd.tlist, x.0)?.as_var().filter(|v| v.varno == OUTER_VAR && v.varlevelsup == 0)?;
        find(near, v.varattno).map(|_| (i, v.varattno))
    }).collect_in(gx.mcx)
}

/// vol-0028 (D1344's form G): pipeline `k` keeps its group's records for the topn drain (`make_drain`, which re-projects
/// each from its record): a `runs.topk_groups` top, or form G's full sort, whose run holds the group's records, not rows
/// (frame::iface `run_holds_rows`; `SELECT g .. GROUP BY g ORDER BY random() + g * 10`), its end and drain the top's
/// (ops/sort/lower.rs hands both to `runs.topk_groups`).
fn drains(gx: &Gx<'_, '_>, k: usize) -> bool {
    gx.end_need(k) == KindId::RunsTopkGroups || (gx.drained_group(k).is_some() && !crate::frame::iface::run_holds_rows(gx, k))
}

/// `backs`' projection columns: the entries the order pass already evaluated for every group passing HAVING.
pub(crate) fn read_back_cols<'g>(gx: &Gx<'g, '_>, g: &GroupDecl<'_>) -> PgVec<'g, i16> {
    backs(gx, g).into_iter().map(|b| b.1).collect_in(gx.mcx)
}

/// `read_back_cols` for projection op `j` of pipeline `k` (step/project.rs): when `k` is a `runs.topk_groups` top over its
/// group and `j` the projection before its sort, the columns that projection leaves uncomputed; else none.
pub(crate) fn skipped<'g>(gx: &Gx<'g, '_>, k: usize, j: usize) -> PgVec<'g, i16> {
    let Some(g) = gx.drained_group(k).filter(|_| drains(gx, k)) else { return PgVec::new_in(gx.mcx) };
    let ops = &gx.pipelines()[k].ops;
    let s = ops.iter().position(|op| matches!(op, Op::Sort(_))).unwrap_or(ops.len());
    match ops[..s].iter().rposition(|op| matches!(op, Op::Project(_))) {
        Some(near) if near == j && top_of(gx, g).is_some_and(|t| t.0 == k) => read_back_cols(gx, g),
        _ => PgVec::new_in(gx.mcx),
    }
}

/// `backs` over record `rec`: `(projection column, first slot, type)` for the drain's read.
pub(crate) fn read_back<'g>(gx: &Gx<'g, '_>, rec: &GroupRec<'_, '_>) -> PgVec<'g, (i16, usize, u32)> {
    let xs = exprs(gx, rec.g);
    backs(gx, rec.g).into_iter().map(|(i, col)| (col, slot(rec, &xs, i), xs[i].2)).collect_in(gx.mcx)
}

/// N6a: the comparator of order expression `i` of `rec`'s top (a `cmped` result's; 0 for any other).
pub(crate) fn expr_cmp(gx: &Gx<'_, '_>, rec: &GroupRec<'_, '_>, i: usize) -> u32 {
    exprs(gx, rec.g).get(i).filter(|x| copies(gx, x.2) || worded(gx, x.2)).and_then(|x| cmped(gx, x.2)).map_or(0, |c| c.0)
}

/// A name key's seven later sort words (`w`, its first, with the key's DESC / NULLS bits): its image's next slots, each
/// compared as the first (DESC reverses each word, so the whole image); none for any other key.
pub(crate) fn more_words<'g>(gx: &Gx<'g, '_>, rec: &GroupRec<'_, '_>, resno: i16, w: u64) -> PgVec<'g, u64> {
    let xs = exprs(gx, rec.g);
    let Some(i) = xs.iter().position(|x| x.0 == resno && x.2 == NAME) else { return PgVec::new_in(gx.mcx) };
    let s = slot(rec, &xs[..], i) as u64;
    (1..8).map(|j| (w & !0xff) | (s + j)).collect_in(gx.mcx)
}

/// The emit's pass over pipeline `k`'s group, compiled: `None` when its top orders by no expression.
pub(crate) fn pass<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, rec: &GroupRec<'_, 'p>) -> Option<Pass<'g>> {
    let (gx, node) = (px.gx, rec.node);
    let xs = exprs(gx, rec.g);
    if xs.is_empty() {
        return None;
    }
    // a derived text key renders its code column (a dictionary image or the ELSE constant: live through the sort); a key
    // over a function's result (its render the row's, D1023) is copied as a computed text result (D1325)
    let key = |v: i16| rec.g.keys.iter().position(|kd| kd.resno == v).and_then(|i| rec.reps[i]);
    let xs: PgVec<'_, _> = xs.into_iter().map(|(r, e, t)| {
        let live = e.as_var().and_then(|v| key(v.varattno)).is_some_and(|r| r.guard);
        (r, e, if text_payload(t) && !live { COPIED } else { t })
    }).collect_in(gx.mcx);
    // D3080: an integer-text key the owner already orders (props `tie_key`'s int-text arm) is copied for the kept groups only
    let int_text = |e: Node<'_>| e.as_var().filter(|v| v.varno == OUTER_VAR).and_then(|v| rec.g.keys.iter().find(|kd| kd.resno == v.varattno)).is_some_and(|kd| kd.int_text());
    if crate::ops::sort::topn::lower::count_top(px, k).is_some() && xs.iter().any(|x| !text_payload(x.2) && !(x.2 == COPIED && int_text(x.1))) {
        shape(node, "a count-ordered owner top with an order expression (its groups are cut before the emit)");
    }
    let ops = &gx.pipelines()[k].ops;
    let s = if xs.iter().all(|x| text_payload(x.2)) { 0 } else { ops.iter().position(|op| matches!(op, Op::Sort(_))).unwrap_or(ops.len()) };
    let filters = ops[..s].iter().filter_map(|op| if let Op::Filter(q) = op { Some(*q) } else { None }).map(|q| crate::step::expr::compile(px, q, None, node)).collect_in(gx.mcx);
    let exprs = xs.iter().enumerate().map(|(i, x)| {
        let cell = (x.2 == NAME).then(|| gx.out_of(x.1).map(|o| Cell::of_expr(o, 0, 0, Nullable::Maybe))).flatten();
        let copy = if x.2 == COPIED { Some(0) } else { copies(gx, x.2).then(|| cmped(gx, x.2).and_then(|c| c.1).unwrap_or(0)) };
        (crate::step::expr::compile(px, x.1, None, node), slot(rec, &xs, i), x.2, cell, copy, narrow(gx, x.2))
    }).collect_in(gx.mcx);
    let images = xs.iter().any(|x| copies(gx, x.2)).then(|| {
        let w = [4, 5, 6].map(|i| crate::table::emit_group::word(px, k, i));
        if w[1] != w[0] + 8 || w[2] != w[0] + 16 {
            crate::refuse!(node, Reason::Bug { payload: "an order pass's copy cursor words are not adjacent" });
        }
        w[0]
    });
    for x in &xs {
        fires::fire(match x.2 { FLOAT8 => "order_slot.float8", NAME => "order_slot.name", COPIED => "order_slot.copied", t if text_payload(t) => "order_slot.text", _ => "order_slot.int" });
    }
    Some(Pass { exprs, filters, images })
}

/// The emit's sort (`sort_words`) with its pass.
pub(crate) fn sorted<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, rec: &GroupRec<'_, 'p>) -> (PgVec<'g, u64>, u64, Option<Pass<'g>>) {
    let (words, form) = crate::ops::sort::topn::lower::sort_words(px, k, rec);
    (words, form, pass(px, k, rec))
}

/// The pass over the gathered records `ptrs[0 .. len)` (the emit's pointer word filed, as `group_row` reads it), when the
/// emit's sort has one: each group passing HAVING has its expressions' values in their slots, valid where not NULL.
pub(crate) fn run(b: &mut FunctionBuilder<'_>, env: &mut Env, emit: &Emit<'_, '_>, ptrs: Value, len: Value, sort: &Option<(PgVec<'_, u64>, u64, Option<Pass<'_>>)>) {
    let Some((_, _, Some(p))) = sort else { return };
    let pi = b.conv(ConvOp::PtrToInt, Type::I64, ptrs);
    env.store_word(b, emit.words[crate::table::emit_group::W_PTRS as usize], pi); // group_row's read (filed again after)
    let (node, layout) = (emit.rec.node, &emit.rec.layout);
    let memo = env.memo.set_aside(); // the loop's values do not reach the code after it
    // D3024 (b), sorted: a sorted wide emit's claims keep their copies to the statement's end (the drain's merge compares
    // them past every claim), so a claim's fresh call does not take its thread's earlier claims' chunk again
    let cursor = p.images.map(|off| if emit.sorted_wide() { cursor_at(b, env, off) } else { rewind(b, env, off) });
    let (h, body) = cancel::header(b, env.ctx, env.l_cancel, &[Type::I64], env.mcx);
    let (l_done, l_next) = (b.create_block(&[]), b.create_block(&[]));
    b.br(h, &[env.zero]);
    b.switch_to(body);
    let i = b.block_params(body)[0];
    let (more, l_step) = (b.icmp(IntCond::Slt, i, len), b.create_block(&[]));
    b.cond_br(more, (l_step, &[]), (l_done, &[]));
    b.switch_to(l_step);
    // D3080: a group's renders (its key cells, a copied expression's text) are dead once its slots are filled: every text
    // is copied into the images (`copied`) unless a live derived key's datum is stored as is, so the region's top goes
    // back after each group (PostgreSQL's per-tuple reset), not one render per group to the region's cap
    let mark = p.exprs.iter().all(|x| !text_payload(x.2)).then(|| env.hir.mark(b)).flatten();
    let row = emit.group_row(b, env, i, Carry::new(env.zero, env.mcx), env.zero);
    let at = b.ptr_add(ptrs, i, 8, 0);
    let rec = b.load(Type::Ptr, at, 0);
    // a derived text key's render for every group (the sort reads each record's; no raise, no HAVING): its datum, 0 NULL
    for &(ex, s, ..) in p.exprs.iter().filter(|x| text_payload(x.2)) {
        let (v, valid) = crate::step::expr::emit(b, env, node, &row, ex);
        let v = if b.value_type(v) == Type::Ptr { b.conv(ConvOp::PtrToInt, Type::I64, v) } else { v };
        let v = b.select(valid, v, env.zero);
        b.store(rec, layout.slot_off(s) as i32, v);
        put_valid(b, env, rec, crate::ops::shared::slots::slot_valid(layout, s as u16), valid); // D923: text_image reads the bit
    }
    for &q in &p.filters {
        let ok = crate::step::expr::pred(b, env, node, &row, q);
        let l_ok = b.create_block(&[]);
        b.cond_br(ok, (l_ok, &[]), (l_next, &[]));
        b.switch_to(l_ok);
    }
    for (ex, s, _, cell, copy, narrow) in p.exprs.iter().filter(|x| !text_payload(x.2)) {
        let (ex, s) = (*ex, *s);
        let (v, valid) = crate::step::expr::emit(b, env, node, &row, ex);
        if let (Some(len), Some(cur)) = (*copy, cursor) {
            copied(b, env, (v, valid), cur, rec, layout, s, len);
            continue;
        }
        if let Some(cell) = cell {
            name(b, env, node, layout, cell, (v, valid), rec, s);
            continue;
        }
        // a narrow result's low bytes decide (its word's upper bits are not the datum's): sign-extended to the i64 read; a
        // float4 / bool by-value result is its Datum's bits
        let v = match b.value_type(v) {
            Type::Bool => b.select(v, env.one, env.zero),
            Type::F32 => b.conv(ConvOp::Bitcast, Type::I32, v),
            Type::Ptr => b.conv(ConvOp::PtrToInt, Type::I64, v),
            _ => v,
        };
        let v = match *narrow {
            Some(ty) if b.value_type(v) != ty => {
                let x = b.conv(ConvOp::Trunc, ty, v);
                b.conv(ConvOp::Sext, Type::I64, x)
            }
            Some(_) => b.conv(ConvOp::Sext, Type::I64, v),
            None if b.value_type(v) != Type::I64 => b.conv(ConvOp::Sext, Type::I64, v),
            None => v,
        };
        b.store(rec, layout.slot_off(s) as i32, v);
        put_valid(b, env, rec, crate::ops::shared::slots::slot_valid(layout, s as u16), valid);
    }
    b.br(l_next, &[]);
    b.switch_to(l_next);
    if let Some(m) = mark {
        env.hir.give_back(b, m);
    }
    let one = b.iconst(Type::I64, 1);
    let i2 = b.int_bin(IntBinOp::Add, i, one);
    b.br(h, &[i2]);
    b.switch_to(l_done);
    env.memo.restore(memo);
}

/// The word and validity (`Bool`) the pass left in order slot `s` of record `at` of `rec` (`read_back`'s drain read): its
/// validity bit, or valid for a record without one (`wants_validity` gives a NULL-able expression its bit).
pub(crate) fn slot_read(b: &mut FunctionBuilder<'_>, env: &Env, rec: &GroupRec<'_, '_>, at: Value, s: usize) -> (Value, Value) {
    let l = &rec.layout;
    let v = b.load(Type::I64, at, l.slot_off(s) as i32);
    if !l.has_validity() {
        return (v, b.bconst(true));
    }
    let (o, bit) = l.valid_bit(s);
    let byte = b.load_zext(Type::I8, at, o as i32);
    let m = b.iconst(Type::I64, 1i64 << bit);
    let x = b.int_bin(IntBinOp::And, byte, m);
    (v, b.icmp(IntCond::Ne, x, env.zero))
}

/// D1325: the copy cursor at frame words `off ..` rewound to its chunk's start (a rescan's fresh call reuses the chunk).
pub(crate) fn rewind(b: &mut FunctionBuilder<'_>, env: &Env, off: i32) -> Value {
    let cur = cursor_at(b, env, off);
    let (c, e, n) = (b.load(Type::I64, cur, 0), b.load(Type::I64, cur, 8), b.load(Type::I64, cur, 16));
    let (start, none) = (b.int_bin(IntBinOp::Sub, e, n), b.icmp(IntCond::Eq, c, env.zero));
    let c = b.select(none, env.zero, start);
    b.store(cur, 0, c);
    cur
}

/// The copy cursor at frame words `off ..` as it stands.
fn cursor_at(b: &mut FunctionBuilder<'_>, env: &Env, off: i32) -> Value {
    let Some(words) = env.words else { crate::refuse!(env.node, Reason::Bug { payload: "an order pass's copy cursor without the words site" }) };
    b.ptr_add(words, env.zero, 1, off)
}

/// D1325: a computed result's copy (0: NULL) into slot `s` of `rec`, valid where not NULL; a failed copy raises. `len`:
/// 0 for a varlena (`key_copy`), else a fixed image's bytes (`key_copy_fixed`, N6a).
#[allow(clippy::too_many_arguments)]
fn copied(b: &mut FunctionBuilder<'_>, env: &mut Env, (v, valid): (Value, Value), cur: Value, rec: Value, layout: &RecordLayout, s: usize, len: u16) {
    let w = copy_word(b, env, (v, valid), cur, len);
    let (failed, l_ok) = (b.icmp(IntCond::Slt, w, env.zero), b.create_block(&[]));
    let e = b.iconst(Type::I64, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64);
    b.cond_br(failed, (env.l_raise, &[e]), (l_ok, &[]));
    b.switch_to(l_ok);
    b.store(rec, layout.slot_off(s) as i32, w);
    put_valid(b, env, rec, crate::ops::shared::slots::slot_valid(layout, s as u16), valid);
}

/// D1325: the copy's word (0: NULL) of a computed result bumped from the cursor `cur`: below zero when the copy failed,
/// its error stashed. `len` as `copied`'s.
pub(crate) fn copy_word(b: &mut FunctionBuilder<'_>, env: &mut Env, (v, valid): (Value, Value), cur: Value, len: u16) -> Value {
    let v = if b.value_type(v) == Type::Ptr { b.conv(ConvOp::PtrToInt, Type::I64, v) } else { v };
    let d = b.select(valid, v, env.zero);
    let flags = gravity_ir::HostFlags { may_error: true, touches_memory: true, ..gravity_ir::HostFlags::default() };
    if len == 0 {
        let h = b.declare_host_once("key_copy", &[Type::Ptr, Type::Ptr, Type::I64], Some(Type::I64), crate::table::key_copy::key_copy as *const () as usize, flags);
        b.call_host(h, &[env.ctx, cur, d], Some(Type::I64)).expect("returns the copy")
    } else {
        let h = b.declare_host_once("key_copy_fixed", &[Type::Ptr, Type::Ptr, Type::I64, Type::I64], Some(Type::I64), crate::table::key_copy::key_copy_fixed as *const () as usize, flags);
        let n = b.iconst(Type::I64, len as i64);
        b.call_host(h, &[env.ctx, cur, d, n], Some(Type::I64)).expect("returns the copy")
    }
}

/// D1212: order slot bit `v` of `rec` set to `valid`, cleared for a NULL: a grouped DISTINCT group's emit view (keyed/pairs_view.rs)
/// has validity only for its folds, so the owner leaves every other slot's bit set (`fold_plain::mark` only sets).
fn put_valid(b: &mut FunctionBuilder<'_>, env: &mut Env, rec: Value, v: gravity_expr::emit::CellValid, valid: Value) {
    if let gravity_expr::emit::CellValid::Bit { off, bit } = v {
        let byte = b.load_zext(Type::I8, rec, off as i32);
        let (keep, one) = (b.iconst(Type::I64, !(1i64 << bit)), b.iconst(Type::I64, 1 << bit));
        let (kept, m) = (b.int_bin(IntBinOp::And, byte, keep), b.select(valid, one, env.zero));
        let nb = b.int_bin(IntBinOp::Or, kept, m);
        let nb = b.conv(ConvOp::Trunc, Type::I8, nb);
        b.store(rec, off as i32, nb);
    }
}

/// A name result's eight image words (rep.word.order's `name_image` through `rep::name_order`, C order) into slots `s ..`, all valid or none.
#[allow(clippy::too_many_arguments)]
fn name(b: &mut FunctionBuilder<'_>, env: &mut Env, node: i32, layout: &RecordLayout, cell: &Cell, (v, valid): (Value, Value), rec: Value, s: usize) {
    let Some(tf) = gravity_decompose::catalog::typefacts::row_of(NAME) else { shape(node, "a name order key without its type facts") };
    let t = b.bconst(true);
    let null = b.int_bin(IntBinOp::Xor, valid, t);
    let coll = gravity_decompose::catalog::typefacts::KeyColl { oid: 950, bytes_eq: true, bytes_order: true };
    let words = {
        let mut cx = env.lx(b, node);
        let w = crate::rep::name_order(&mut cx, CellRef { cell, val: v, null: Some(null) }, tf, coll);
        env.memo = cx.memo;
        w
    };
    let Some(words) = words else { shape(node, "a name order key without rep.word.order's name image") };
    for (j, w) in words.into_iter().enumerate() {
        b.store(rec, layout.slot_off(s + j) as i32, w);
        put_valid(b, env, rec, crate::ops::shared::slots::slot_valid(layout, (s + j) as u16), valid);
    }
}
