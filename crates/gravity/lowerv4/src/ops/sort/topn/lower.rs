//! Top over group finals, kind `runs.topk_groups` (plan §3.1 the runs face; H12 `OwnerTop::Final`): `ORDER BY <finals or
//! keys> LIMIT n` over a keyed group. The group emit sorts its kept records by the sort's keys before they stream
//! (`sort_words`: each key a final's slot, ordered through its class's final image — a word, an average's exact rational, or
//! a min / max(text)'s image compared under the sort's collation — or a key's image word, or a text key's representative
//! rendered and compared under its collation; `agg_sort`). The rows
//! then reach this pipeline's top in final order, through the HAVING filter and the projection: the top (`TopEnd`) keeps
//! the first `count + offset` arrivals' records in place at the front of the emit's array and ends the pipeline when full,
//! so no later group's finals are taken. The drain (`TopDrain`, the next pipeline's source) walks that array again: each
//! record's group row, then the top pipeline's projection over it (the sort's output columns by resno). The records stay
//! live because the drain consumes the group's life too. Budget 400 lines (files.tsv, +25 % gate). Owner M1-KEYED.
use gravity_abi::breaker::{Budget, KeyKind, SlotKind, Spill};
use gravity_decompose::graph::Bound;
use gravity_decompose::ops::shared::agg_class::Class;
use gravity_decompose::ops::shared::sort::{SortDesc, SortForm};
use gravity_decompose::{Op, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, IntBinOp, IntCond, Type, Value};
use gravity_kernels::host_abi::{sk_key, sk_null, FORM_BOUND_MAX, FORM_BOUND_SHIFT, FORM_COUNT_TOPK, FORM_NULLKEY_SHIFT, FORM_RUNS, FORM_TIES, SK_CMP, SK_DESC, SK_FLOAT, SK_HEAP, SK_NULLS_FIRST, SK_SLOT, SK_TEXT, SK_UNSIGNED, SK_TEXT_COLL_SHIFT, SK_TEXT_LANE_SHIFT, SK_TEXT_SITE_SHIFT, SK_TEXT_SLOT_SHIFT};
use gravity_program::{LifeId, Pipeline, Rep};
use mcx::{CollectIn, GrowIn, PgBox, PgVec};
use types_nodes::primnodes::OUTER_VAR;
use types_nodes::Node;

use crate::cell::{Cell, Nullable, Row};
use crate::face::end::EndMech;
use crate::table::face::OwnerTopMech;
use crate::face::kinds::{Face, KindId};
use crate::face::source::SourceMech;
use crate::face::step::StepMech;
use crate::frame::env::Env;
use crate::frame::frame::{EndMaker, Next, SourceMaker, StepMaker, Win};
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::{ExId, Px, WordKey};
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::effects::visible::topn::can_raise;

use crate::table::emit_group::{Emit, W_PTRS, W_SORTED, W_STATE};
use crate::table::rec::GroupRec;

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table {
    kinds: &[(KindId::RunsTopkGroups, &StepMaker(make_step)), (KindId::RunsTopkGroups, &EndMaker(make_end)), (KindId::RunsTopkGroups, &SourceMaker(make_drain))],
    ..crate::ops::NONE
};

/// `EmitState.kept`'s byte offset (kernels table/emit.rs).
const ES_KEPT: i32 = gravity_kernels::table::EMIT_KEPT_OFF;

const NUMERIC: u32 = 1700;

fn bug(node: i32, payload: &'static str) -> ! {
    crate::refuse!(node, Reason::Bug { payload })
}

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

/// A key word's sort word names the word (a fixed key's second word one past it) and the key's NULL flag as its index + 1
/// (agg_sort reads bit k % 64 of flags word k / 64, CAP-28), each in two bytes (CAP-26: kernels `sk_key` / `sk_null`, the
/// high bytes in the collation's bits only a text word reads): 65536 key words and 65535 keys, past PostgreSQL's 1664
/// target entries, so the refusal below never meets a statement PostgreSQL accepts.
fn sort_bytes(node: i32, rec: &GroupRec<'_, '_>, i: usize) {
    let (first, n) = rec.key_words(i);
    if !gravity_kernels::host_abi::sk_fits((first + n).saturating_sub(1), i + 1) {
        shape(node, "a sort by a group key past its sort word's index (key word 65536, key 65535)");
    }
}

/// Pipeline `k`'s sort op and the projection before it (the group row's output columns by resno).
pub(crate) fn sort_of<'g, 'p>(px: &Px<'g, 'p>, k: usize) -> (&'g SortDesc<'p>, Option<&'g types_nodes::list::NodeList<'p>>) {
    let p = &px.gx.pipelines()[k];
    let Some(j) = p.ops.iter().position(|op| matches!(op, Op::Sort(_))) else { bug(p.node_id, "a top over groups without its sort") };
    let Op::Sort(sd) = &p.ops[j] else { unreachable!() };
    let project = p.ops[..j].iter().rev().find_map(|op| if let Op::Project(l) = op { Some(*l) } else { None });
    (sd, project)
}

/// The top's bound over pipeline `k`'s groups, `count + offset`, when both are constants (a `Bound::Const` is in the
/// program's key) and there are no ties.
pub(crate) fn bound(px: &Px<'_, '_>, k: usize) -> Option<i64> {
    match sort_of(px, k).0.form {
        SortForm::TopK { count: Bound::Const(c), offset: Bound::All, ties: false } if c >= 0 => Some(c),
        SortForm::TopK { count: Bound::Const(c), offset: Bound::Const(o), ties: false } if c >= 0 && o >= 0 => Some(c.saturating_add(o)),
        _ => None,
    }
}

/// D3024 (b), sorted: whether pipeline `k`'s top is WITH TIES (any bound).
pub(crate) fn with_ties(px: &Px<'_, '_>, k: usize) -> bool {
    matches!(sort_of(px, k).0.form, SortForm::TopK { ties: true, .. })
}

/// RUNS-18: a top WITH TIES's `count + offset`, both constants: the emit orders that many records, their last one's ties
/// and the record past them first (`cut`, `FORM_TIES`) and the next ones as the drain reaches their end (table/emit_group.rs
/// `Emit::more`), while the drain keeps every arrival (`top_n` is `None`) and the Limit past it extends the last row's ties.
pub(crate) fn ties_bound(px: &Px<'_, '_>, k: usize) -> Option<i64> {
    match sort_of(px, k).0.form {
        SortForm::TopK { count: Bound::Const(c), offset: Bound::All, ties: true } if c >= 0 => Some(c),
        SortForm::TopK { count: Bound::Const(c), offset: Bound::Const(o), ties: true } if c >= 0 && o >= 0 => Some(c.saturating_add(o)),
        _ => None,
    }
}

/// KEYED-08 / RUNS-19: the top's bound as the end reads it: a constant `count + offset`, or a bind-time parameter word
/// for either (a generic plan's `LIMIT $1`), read each call. `None` (every arrival kept, D2470): WITH TIES, or a bound
/// over a value with no parameter word.
#[derive(Clone, Copy)]
pub(crate) enum TopN {
    Const(i64),
    Words { count: Option<u16>, count_c: i64, offset: Option<u16>, offset_c: i64 },
}

fn top_n<'p>(px: &mut Px<'_, 'p>, k: usize) -> Option<TopN> {
    if let Some(n) = bound(px, k) {
        return Some(TopN::Const(n));
    }
    let SortForm::TopK { count, offset, ties: false } = sort_of(px, k).0.form else { return None };
    let ((count, count_c), (offset, offset_c)) = (part(px, k, count, i64::MAX)?, part(px, k, offset, 0)?);
    Some(TopN::Words { count, count_c, offset, offset_c })
}

/// One bound of `top_n`: its parameter word, or a constant (`ALL`: `all`; a negative constant keeps every arrival, the
/// Limit raises); `None` for a value with no parameter word.
fn part<'p>(px: &mut Px<'_, 'p>, k: usize, b: Bound<'p>, all: i64) -> Option<(Option<u16>, i64)> {
    let node = px.gx.pipelines().get(k).map_or(0, |p| p.node_id);
    match b {
        Bound::All => Some((None, all)),
        Bound::Const(c) => Some((None, if c < 0 { all } else { c })),
        Bound::Expr(e) => match crate::lifetime::bound::word(px, crate::lifetime::bound::Feed::TupleBound, k, e, node) {
            gravity_lower::Place::Word(w) => Some((Some(w), 0)),
            _ => None,
        },
    }
}

/// A parameter bound's cut for the emit's sort (`FORM_BOUND`): no filter before the sort (the emit orders more records
/// past a constant bound only, `cut`), nothing that can raise; `None` for a constant bound (`cut` puts it in the form
/// word) and the rest.
pub(crate) fn late_cut(px: &mut Px<'_, '_>, k: usize, rec: &GroupRec<'_, '_>) -> Option<TopN> {
    if drops(px.gx, k) || can_raise(px, k, rec.g) {
        return None;
    }
    top_n(px, k).filter(|t| matches!(t, TopN::Words { .. }))
}

impl TopN {
    /// The emit descriptor's form word with this call's cut: `form | (n + 1) << FORM_BOUND_SHIFT`, or `form` (every
    /// record ordered) when `n` passes `FORM_BOUND_MAX`.
    pub(crate) fn store_cut(self, b: &mut FunctionBuilder<'_>, env: &Env, desc: Value, form: u64) {
        crate::frame::instr::partial(); // EXPLAIN ANALYZE: the cut emit's records are fewer than the group's
        let n = self.value(b, env);
        let (max, one) = (b.iconst(Type::I64, FORM_BOUND_MAX as i64), b.iconst(Type::I64, 1));
        let fits = b.icmp(IntCond::Sle, n, max);
        let n1 = b.int_bin(IntBinOp::Add, n, one);
        let shift = b.iconst(Type::I64, FORM_BOUND_SHIFT as i64);
        let cut = b.int_bin(IntBinOp::Shl, n1, shift);
        let cut = b.select(fits, cut, env.zero);
        let form = b.iconst(Type::I64, form as i64);
        let form = b.int_bin(IntBinOp::Or, form, cut);
        b.store(desc, gravity_kernels::host_abi::DESC_FORM as i32, form);
    }

    /// `count + offset` this call, at most `i64::MAX`: a NULL or negative count keeps every arrival, a NULL or negative
    /// offset counts 0 (a negative bound raises at the Limit, past the drain).
    pub(crate) fn value(self, b: &mut FunctionBuilder<'_>, env: &Env) -> Value {
        let (count, count_c, offset, offset_c) = match self {
            TopN::Const(n) => return b.iconst(Type::I64, n),
            TopN::Words { count, count_c, offset, offset_c } => (count, count_c, offset, offset_c),
        };
        let read = |b: &mut FunctionBuilder<'_>, w: Option<u16>, c: i64, none: Value| match w {
            None => b.iconst(Type::I64, c),
            Some(w) => {
                let nb = b.load_zext(Type::I8, env.params_null, w as i32);
                let v = b.load(Type::I64, env.params, w as i32 * 8);
                let null = b.icmp(IntCond::Ne, nb, env.zero);
                let neg = b.icmp(IntCond::Slt, v, env.zero);
                let bad = b.int_bin(IntBinOp::Or, null, neg);
                b.select(bad, none, v)
            }
        };
        let max = b.iconst(Type::I64, i64::MAX);
        let c = read(b, count, count_c, max);
        let o = read(b, offset, offset_c, env.zero);
        // both are non-negative: the sum overflows exactly when it wraps below the count
        let s = b.int_bin(IntBinOp::Add, c, o);
        let over = b.icmp(IntCond::Slt, s, c);
        b.select(over, max, s)
    }
}

/// Entry `resno` of a target list, as its expression.
fn entry<'p>(l: &types_nodes::list::NodeList<'p>, resno: i16) -> Option<Node<'p>> {
    l.iter().filter_map(|n| n.as_target_entry()).find(|te| te.resno == resno).map(|te| te.expr)
}

/// N6a: whether `sort_words` orders a final in a slot of `kind` (its match's arms; `_` refuses): a final of any other
/// slot (avg(numeric), a variance, an interval avg) orders by its value in an order slot (order_slot.rs `exprs`). D4565:
/// a var / stddev over int2 / int4 (`Stat3I128`) folds into an `AvgI128` slot and its `SumI128` companion, but its final
/// is not the slot's Sx / N, so it orders by its value as a hosted variance does (`stat_slot`).
pub(crate) fn final_ordered(kind: SlotKind, class: Class) -> bool {
    use SlotKind::*;
    if matches!(class, Class::Stat3I128(_)) {
        return false;
    }
    matches!(kind, Count | SumI64 | MinI64 | MaxI64 | AvgI128 | SumNum | SumNumWide | SumI128 | BitAnd | BitOr | BitXor | SumF64 | SumF32 | MinVar | MaxVar)
        || (kind == F64Accum && class == Class::AvgF64)
}

/// The sort words (host_abi `SK_*` over the record) of the top over pipeline `k`'s groups, and the descriptor's form word
/// (the flags word's index + 1 when a sorted key can be NULL).
pub(crate) fn sort_words<'g>(px: &Px<'g, '_>, k: usize, rec: &GroupRec<'_, '_>) -> (PgVec<'g, u64>, u64) {
    sort_words_upto(px, k, rec, usize::MAX)
}

/// `sort_words` over the top's first `keys` sort keys only. A reader made inside the group's own producer (a planner pass
/// 2's admission, twopass.rs `planner_pass2`: before `Mechs::new` numbers the producer's site) reads the lead it checks
/// and never images a key past it: a text code key's word names the producer's site (a count top ordered past its pass-1
/// key by a dictionary-coded text key), which the emit's own `sort_words` reads once the producer is made.
pub(crate) fn sort_words_upto<'g>(px: &Px<'g, '_>, k: usize, rec: &GroupRec<'_, '_>, keys: usize) -> (PgVec<'g, u64>, u64) {
    let node = rec.node;
    let (sd, project) = sort_of(px, k);
    let ops = &px.gx.pipelines()[k].ops;
    let ops = &ops[..ops.iter().position(|op| matches!(op, Op::Sort(_))).unwrap_or(ops.len())];
    // form G (props `group_sort`, V4R-118): a full sort over the groups orders every record the same way; RUNS-18: a top
    // WITH TIES keeps every arrival (`top_n` is `None`) and orders its records `ties_bound` at a time (`cut`): the Limit
    // past the drain extends the last row's ties
    if !matches!(sd.form, SortForm::TopK { .. } | SortForm::Runs) {
        shape(node, "a zone-walked top over groups");
    }
    // past DESC_MAX_SORT words the descriptor runs on (table/emit_group.rs `desc_cmp`, wide), as `key_words`
    let mut words = mcx::reserved_in(sd.keys.len(), px.mcx());
    let mut form = 0u64;
    for key in sd.keys.iter().take(keys) {
        let mut e = entry(sd.tlist, key.resno).unwrap_or_else(|| bug(node, "a sort key outside its target list"));
        if let (Some(v), Some(l)) = (e.as_var(), project) {
            if v.varno == OUTER_VAR {
                e = entry(l, v.varattno).unwrap_or_else(|| bug(node, "a sort column outside the group's projection"));
            }
        }
        // through a subquery scan's projections back to the group's row (#227; order_slot.rs `key_expr`)
        let e = crate::ops::sort::topn::order_slot::key_expr(ops, sd, key.resno, px.mcx()).unwrap_or(e);
        // a final relabeled to its own type (`ORDER BY min(t) COLLATE x`: the collation alone) orders under `key.coll` (D358)
        let a = e.as_aggref().or_else(|| e.as_relabel_type().and_then(|r| r.arg.as_aggref().filter(|a| a.aggtype == r.resulttype))).filter(|a| crate::ops::sort::topn::order_slot::native(px.gx, rec.g, a)); // D1519
        let mut w = if let Some(a) = a {
            let Some(agg) = rec.aggs.iter().find(|x| x.d.aggno as i32 == a.aggno) else { bug(node, "a sorted aggregate the group does not fold") };
            let s = agg.d.slot as u64;
            match rec.layout.slot_kind(s as usize) {
                // D4565: a var / stddev's AvgI128 slot is not its final (`final_ordered`: it never reaches here)
                _ if matches!(agg.d.class, Class::Stat3I128(_)) => shape(node, "a top over a var / stddev final by its slot"),
                SlotKind::Count | SlotKind::SumI64 | SlotKind::MinI64 | SlotKind::MaxI64 | SlotKind::AvgI128 | SlotKind::SumNum | SlotKind::SumNumWide | SlotKind::SumI128 => SK_SLOT | s, // L0-n: numeric sums
                SlotKind::BitAnd | SlotKind::BitOr | SlotKind::BitXor => SK_SLOT | s,
                // sum(float4 / float8) and avg(float)'s finals by their float order (V4R-111); another F64Accum final (a
                // variance) is not its Sx / N
                SlotKind::SumF64 | SlotKind::SumF32 => SK_SLOT | SK_FLOAT | s,
                SlotKind::F64Accum if agg.d.class == Class::AvgF64 => SK_SLOT | SK_FLOAT | s,
                // min / max(text)'s image (`rekind`: an arena varlena, 0 over no row, D277) compared as text under the sort's
                // collation, never by code; the low byte names the slot for its NULL (`Rec::valid`) (D358)
                SlotKind::MinVar | SlotKind::MaxVar => SK_SLOT | s | SK_TEXT | SK_HEAP | s << SK_TEXT_SLOT_SHIFT | (key.coll.oid as u64) << SK_TEXT_COLL_SHIFT | crate::dict::coderep::sk_bpchar(px.gx, a.aggtype), // a bpchar final's trimmed (D1309)
                _ => shape(node, "a top over a final the group sort does not order (numeric or float)"),
            }
        } else if let Some(i) = e.as_var().filter(|v| v.varno == OUTER_VAR).and_then(|v| rec.g.keys.iter().position(|kd| kd.resno == v.varattno)) {
            // D3080: an integer-text key orders by its text (the integer's word orders otherwise): its render's order slot
            if rec.g.keys[i].int_text() { crate::ops::sort::topn::order_slot::sort_word(px.gx, rec, key.resno, key.coll.oid, node) } else {
            match rec.reps[i].filter(|_| !crate::table::emit_key::sorts_by_image(rec, i, node)) { // L0-m: a float key's image
                // a derived text key: its render in an order slot, compared as text (keyed/order_slot.rs)
                Some(r) if r.derived => crate::ops::sort::topn::order_slot::sort_word(px.gx, rec, key.resno, key.coll.oid, node),
                Some(r) if r.datum => crate::ops::sort::topn::copy_words::word(px, rec, i, crate::frame::pairs_view::sk_slot(rec.g, rec.g.keys[i].rep, node), node, Some(key.opno), key.coll.oid), // L0-l: its copy
                Some(r) => {
                    let Rep::Code { dict } = r.rep else { unreachable!() };
                    // D4669: a dependent / carried key's code is its build row's, rendered through that build's site
                    let Some(site) = px.pipeline_site(crate::table::emit_key::code_home(rec, i).0 as usize).filter(|&s| s < 256) else { bug(node, "a text sort key whose producer has no site") };
                    let slot = crate::frame::pairs_view::sk_slot(rec.g, rec.g.keys[i].rep, node);
                    SK_TEXT | slot << SK_TEXT_SLOT_SHIFT | (dict.lane as u64) << SK_TEXT_LANE_SHIFT | (site as u64) << SK_TEXT_SITE_SHIFT | (key.coll.oid as u64) << SK_TEXT_COLL_SHIFT | crate::dict::coderep::sk_bpchar(px.gx, rec.g.keys[i].tf)
                }
                None => {
                    let kd = &rec.g.keys[i];
                    // an enum orders by its comparator (enum_cmp: enumsortorder for an odd oid), never by its oid word (D1165: SK_CMP)
                    let t = gravity_decompose::catalog::expr_type::type_of(kd.expr.node()).and_then(|t| gravity_decompose::catalog::typefacts::facts_of(px.gx.facts, t));
                    let cmp = if t.is_some_and(|t| t.image.is_enum()) { crate::ops::sort::topn::copy_words::enum_word(px, rec, i, node, Some(key.opno)) } else { 0 };
                    let (word, _) = crate::ops::sort::topn::copy_words::fp128_hosted(rec, i, node); // a hosted key kept only as its Fp128: D1165
                    sort_bytes(node, rec, i);
                    let float = if matches!(kd.kind, KeyKind::F32 | KeyKind::F64) { SK_FLOAT } else { 0 }; // V4R-111
                    // the image's NULL bit is the key's index, not its word's (a text key before it takes more words, D456)
                    let null_bit = rec.flags_word().map_or(0, |f| {
                        form = ((f as u64) + 1) << FORM_NULLKEY_SHIFT;
                        sk_null(i + 1)
                    });
                    sk_key(word) | null_bit | float | cmp
                }
            }
            }
        } else {
            crate::ops::sort::topn::order_slot::sort_word(px.gx, rec, key.resno, key.coll.oid, node)
        };
        if key.desc {
            w |= SK_DESC;
        }
        if key.nulls_first {
            w |= SK_NULLS_FIRST;
        }
        let w = byte_coll(px.gx, w);
        words.extend_in(rec.sort_words(w)); // M6-TYPES: a fixed key's two words
        words.extend_in(crate::ops::sort::topn::order_slot::more_words(px.gx, rec, key.resno, w)); // a name key's image: 8 words (D472)
    }
    let kept = count_top(px, k).is_some_and(|(m, _)| m.kept());
    (words, form | if kept { FORM_COUNT_TOPK } else { FORM_RUNS } | cut(px, k, rec))
}

/// The form word's cut, `(n + 1) << FORM_BOUND_SHIFT` (0: all), when the top takes the first `n = count + offset` sorted
/// records that reach it: a constant bound. A HAVING filter before the sort keeps the bound where nothing can raise: the
/// top counts the arrivals that pass it, and the emit orders more records as the drain reaches the ordered ones' end
/// (table/emit_group.rs `Emit::more`). A group row whose way to the top can raise (D449) keeps the bound when no HAVING
/// filter runs (ITEM 0): the kernel orders the first `n` and leaves every other record after them, unordered, the drain
/// walks them all, and the top (`TopEnd::all`) projects and drops each one past its `n` arrivals, so every group's row
/// still runs, where a cut of 0 ordered every record at the owner. Under a HAVING filter it stays 0: `Emit::more` would go
/// on ordering chunks the top only drops. A top WITH TIES (RUNS-18, `ties_bound`) adds `FORM_TIES` where nothing can raise:
/// the kernel orders the first `n`, the `n`-th's ties and the record past them in one call (the Limit stops at that
/// record), and the emit's road goes on past them only where the Limit reads on.
fn cut(px: &Px<'_, '_>, k: usize, rec: &GroupRec<'_, '_>) -> u64 {
    let (raise, having) = (can_raise(px, k, rec.g), drops(px.gx, k));
    match (bound(px, k), ties_bound(px, k)) {
        (Some(n), _) if (n as u64) <= FORM_BOUND_MAX && !(raise && having) => (n as u64 + 1) << FORM_BOUND_SHIFT,
        (None, Some(n)) if (n as u64) <= FORM_BOUND_MAX && !having && !raise => (n as u64 + 1) << FORM_BOUND_SHIFT | FORM_TIES,
        _ => 0,
    }
}

/// RUNS-18: the top WITH TIES whose emit orders a lead (`cut` with `FORM_TIES`: a constant `count + offset` of at least 1,
/// nothing that can raise, no HAVING): its end stops once its arrivals reach the lead's length (the emit's `W_SORTED`), so
/// the groups past the last tie and the row after them are never pulled nor ordered.
fn ties_lead(px: &Px<'_, '_>, k: usize, g: &gravity_decompose::ops::shared::agg::GroupDecl<'_>) -> bool {
    !can_raise(px, k, g) && !drops(px.gx, k) && bound(px, k).is_none() && ties_bound(px, k).is_some_and(|n| n >= 1 && (n as u64) <= FORM_BOUND_MAX)
}

/// Whether pipeline `k` filters its groups (a HAVING) before its sort.
pub(crate) fn drops(gx: &crate::graph::Gx<'_, '_>, k: usize) -> bool {
    gx.pipelines()[k].ops.iter().take_while(|op| !matches!(op, Op::Sort(_))).any(|op| matches!(op, Op::Filter(_)))
}

/// runs.group / runs.unique: the sort words ordering group `rec`'s records by its own keys, each
/// in the direction decompose declared from the input's order (`KeyDecl::desc / nulls_first`, D443), and the form word. A
/// key image that does not order as its type (a fingerprint, a numeric image, an enum) refuses by name: M6's hosted order.
pub(crate) fn key_words<'g>(px: &Px<'g, '_>, rec: &GroupRec<'_, '_>) -> (PgVec<'g, u64>, u64) {
    let node = rec.node;
    // W8-SETOP-WIDEKEYS: past DESC_MAX_SORT keys the emit's descriptor runs on (keyed/emit_group.rs `desc_cmp`, wide); a
    // key's NULL flag is its index + 1 (kernels `sk_null`), in the flags words past 64 keys too (`sort_bytes`, CAP-28 / CAP-26)
    let mut form = FORM_RUNS;
    let mut words = mcx::reserved_in(rec.g.keys.len(), px.mcx());
    // M6 L0-o: a grouped DISTINCT's argument (its last key, resno 0) is gone from the emit's record (keyed/pairs_view.rs), its key copy / code at `rep_slot` (D2005)
    for (i, kd) in rec.g.keys.iter().enumerate().filter(|(_, kd)| kd.resno != 0 || rec.g.form != gravity_decompose::ops::shared::agg::GroupForm::Distinct) {
        let w = match rec.reps[i].filter(|r| !r.imaged || crate::table::key_copy::sorts_by_copy(rec, i)) { // L0-m: a float / numeric key sorts by its image (below)
            _ if kd.int_text() => shape(node, "D3080: a sorted group by an integer-text key (its word orders as the integer)"),
            Some(r) if r.derived => shape(node, "a sorted group by a derived text key"),
            Some(r) if r.datum => crate::ops::sort::topn::copy_words::word(px, rec, i, crate::frame::pairs_view::sk_slot(rec.g, kd.rep, node), node, None, kd.coll), // L0-l: its copy (runs/copy_words.rs)
            Some(r) => {
                let Rep::Code { dict } = r.rep else { unreachable!() };
                // D4669: a dependent / carried key's code is its build row's, rendered through that build's site
                let Some(site) = px.pipeline_site(crate::table::emit_key::code_home(rec, i).0 as usize).filter(|&s| s < 256) else { bug(node, "a text group key whose producer has no site") };
                SK_TEXT | (crate::frame::pairs_view::sk_slot(rec.g, kd.rep, node)) << SK_TEXT_SLOT_SHIFT | (dict.lane as u64) << SK_TEXT_LANE_SHIFT | (site as u64) << SK_TEXT_SITE_SHIFT | (kd.coll as u64) << SK_TEXT_COLL_SHIFT | crate::dict::coderep::sk_bpchar(px.gx, kd.tf)
            }
            None => {
                sort_bytes(node, rec, i);
                let (word, _) = rec.key_words(i);
                // the order arm props/group.rs names (V4R-128): admission refuses any other before this runs; an enum's word is its oid (D1165)
                let cmp = if rec.tfs[i].image.is_enum() { crate::ops::sort::topn::copy_words::enum_word(px, rec, i, node, None) } else { 0 }; // D1165: by enum_cmp
                let packed = crate::ops::sort::topn::copy_words::packed_key(px.gx, rec.g.producer.0 as usize, kd).is_some(); // M6-CLOSE (a'): its signed mantissa word
                let arm = crate::rep::key_kind(kd.kind, Some(kd.image), packed, crate::rep::Accessor::Order);
                if cmp == 0 && arm != KindId::RepWordOrder && arm != KindId::RepFixedOrder && arm != KindId::RepPackedOrder { // M6-TYPES: a uuid / interval / timetz key's two image words
                    crate::rep::hosted_sorted(node, arm);
                }
                // a float by its total order (V4R-111); an unsigned 8-byte by-value image (pg_lsn, xid8) unsigned (V4R-135, D466).
                // D2474: a by-reference one (macaddr8) keys by rep.hosted.order.fixed's word, already signed-comparable
                let float = if matches!(kd.kind, KeyKind::F32 | KeyKind::F64) { SK_FLOAT } else { 0 };
                let unsigned = if rec.tfs[i].image.is_unsigned_word() && rec.tfs[i].typlen == 8 && rec.tfs[i].byval { SK_UNSIGNED } else { 0 };
                // the image's NULL bit is the key's index, not its word's (a text key before it takes more words, D456)
                let null_bit = rec.flags_word().map_or(0, |f| {
                    form |= ((f as u64) + 1) << FORM_NULLKEY_SHIFT;
                    sk_null(i + 1)
                });
                sk_key(word) | float | unsigned | null_bit | cmp
            }
        };
        words.extend_in(rec.sort_words(byte_coll(px.gx, w | if kd.desc { SK_DESC } else { 0 } | if kd.nulls_first { SK_NULLS_FIRST } else { 0 })));
    }
    (words, form)
}

/// A text sort word's collation for `agg_sort` (host_abi `bytes_cmp`): cleared to 0 (byte order) when the collation
/// orders and compares by bytes (C, POSIX, a database default whose locale is C: facts `coll`, D247), so each compare
/// is the two images' byte order with no locale lookup (sort_core-0150 / -0151: the lookup was most of the emit's
/// sort). A kept copy ordered by its comparator (`SK_HEAP | SK_CMP`, `copy_cmp`) keeps it: the comparator reads it.
fn byte_coll(gx: &crate::graph::Gx<'_, '_>, w: u64) -> u64 {
    let coll = (w >> SK_TEXT_COLL_SHIFT) as u32;
    if w & SK_TEXT == 0 || w & (SK_HEAP | SK_CMP) == SK_HEAP | SK_CMP || coll == 0 || !gx.facts.coll(coll).bytes_order {
        return w;
    }
    w & ((1 << SK_TEXT_COLL_SHIFT) - 1)
}

/// M3 L0-d, the count-ordered owner seam: the owner top over the groups pipeline `j` drains into its top, with the top's
/// bound `n` (LIMIT + OFFSET), when road owner.count is on (face/keyed.rs `owner_top`) and the top has the road's shape
/// (props `count_cut`, V4R-113: the predicate `applies::CountTopk` reads, so the road applies exactly where it serves).
/// `None` otherwise: the emit gathers every record and sorts them (a final-ordered top, `ORDER BY avg(x)`; a count top
/// with a HAVING that is no suffix cut of its order (D3152), or a text tie key).
pub(crate) fn count_top(px: &Px<'_, '_>, j: usize) -> Option<(&'static dyn OwnerTopMech, u32)> {
    let gx = px.gx;
    let p = &gx.pipelines()[j];
    let m = crate::table::face::owner_top(&p.node_id)?;
    // D4620: a cascade's base keeps every group for its cascaded sets (and its emit gathers theirs in the runs form)
    let g = gx.drained_group(j).filter(|g| gx.end_need(j) == KindId::RunsTopkGroups && !gx.cascade_base(g))?;
    let (sd, project) = sort_of(px, j);
    let n = u32::try_from(bound(px, j)?).ok()?;
    crate::props::count_cut(gx, p, g, sd, project).then_some((m, n))
}

/// `count_top` for the group producer pipeline `k` builds: the pipeline that drains its group into a top.
pub(crate) fn count_top_of(px: &Px<'_, '_>, k: usize) -> Option<(&'static dyn OwnerTopMech, u32)> {
    let g = px.gx.group_at(k)?;
    // D3585: the drainers found once per lowering (each arm into the group's breaker walked every later pipeline)
    let j = px.gx.drainers(g).into_iter().find(|&j| j > k)?;
    count_top(px, j)
}

/// D-06: the generic owner's top floor for group end `k` over `rec` (`OwnerDesc::runs_top`): the group's one reader is
/// a top over its groups (`runs.topk_groups`) that owner.count does not take (`count_top`: a WITH TIES, a tie key the
/// count form cannot order, ...), of a constant `n` = count + offset at most `RUNS_TOP_HEAP`, whose first sort key is a
/// count of the group (never NULL), and with no HAVING and nothing that can raise before it (PostgreSQL projects every
/// group, the floor lays only some). Each owner partition then lays the records at or past its own n-th best count (exec
/// owner.rs `Best`), and the emit's sort (and a WITH TIES's lead) orders only those. A hashed one-pass group
/// (`owner_floor`'s).
pub(crate) fn top_floor(px: &Px<'_, '_>, k: usize, rec: &GroupRec<'_, '_>) -> Option<i64> {
    let (gx, g) = (px.gx, rec.g);
    if g.form != gravity_decompose::ops::shared::agg::GroupForm::Hashed || g.two_pass.is_some() || g.set.is_some() || !crate::graph::grouped(g) || gx.cascade_base(g) {
        return None;
    }
    let lead = count_lead(px, k, rec, false).filter(|l| l.n <= gravity_program::RUNS_TOP_HEAP && count_top(px, l.q).is_none())?;
    let slot = lead.slot as usize;
    (slot < rec.layout.nslots() && rec.layout.slot_kind(slot) == gravity_abi::breaker::SlotKind::Count).then(|| gravity_program::OwnerDesc::runs_top(lead.n, !lead.desc, slot as u8))
}

/// The top over group end `k`'s groups that orders them first by a count of the group (D-06's `top_floor`, D-06c's
/// twopass.count): pipeline `q`, the group's one reader, a top over its groups (`runs.topk_groups`) of a constant `n` =
/// count + offset (or a WITH TIES count), 1 ..= 2^23 - 1, whose first sort key is `count(*)` / `count(x)` of the group
/// (never NULL, no DISTINCT; `slot`, `star`: a plain `count(*)`) under int8's own `<` / `>`, in direction `desc`; with
/// nothing that can raise before it (PostgreSQL projects every group) and no filter before its sort but, with `cuts`,
/// HAVING quals that drop a suffix of that order (count_cut `suffix_cut`). Neither pipeline in a rerun set. 5c-F6
/// (`top_lead`): or a sum of the group, `lead` its slot's `Rule::lead` (`LEAD_COUNT` for a count).
pub(crate) struct CountLead {
    pub q: usize,
    pub n: u32,
    pub desc: bool,
    pub slot: u16,
    pub star: bool,
    pub lead: u8,
    /// A NULL lead's place: the sort key's NULLS FIRST, never under a HAVING on the lead (a NULL sum passes none).
    pub nulls_first: bool,
    pub aggno: i16,
}

pub(crate) fn count_lead(px: &Px<'_, '_>, k: usize, rec: &GroupRec<'_, '_>, cuts: bool) -> Option<CountLead> {
    top_lead(px, k, rec, cuts, false)
}

/// 5c-F6: `count_lead`, with `sums` widened to a top whose first sort key is `sum(x)` of the group in an ABI sum slot
/// (`lead_of`: int2 / int4 / int8, a numeric at a fixed scale, float4 / float8; no DISTINCT, no dscale companion) under
/// its result type's own order (the sort operator's btree family is the type's, in the key's direction), its NULLs
/// where the key puts them; with `cuts`, HAVING quals that drop a suffix of that order (count_cut `suffix_cut_on`). The
/// lead's own final is no bar to the cut where it can raise (a numeric sum's OVERFLOW, D299): pass 1 folds it over
/// every row and the rule admits such a record (topn raw.rs `lead_ranks`), so pass 2 takes its final as PostgreSQL
/// would.
pub(crate) fn top_lead(px: &Px<'_, '_>, k: usize, rec: &GroupRec<'_, '_>, cuts: bool, sums: bool) -> Option<CountLead> {
    use crate::ops::shared::twopass::{lead_of, LEAD_COUNT};
    let (gx, g) = (px.gx, rec.g);
    let rerun = |q: usize| gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(q as u16)));
    let &[q] = &gx.drainers(g)[..] else { return None };
    if gx.end_need(q) != KindId::RunsTopkGroups || rerun(q) || rerun(k) {
        return None;
    }
    let n = bound(px, q).or_else(|| ties_bound(px, q)).filter(|&n| (1..=0x7f_ffff).contains(&n))? as u32;
    let (sd, project) = sort_of(px, q);
    let p = &gx.pipelines()[q];
    let mut having = false;
    for op in p.ops.iter().take_while(|op| !matches!(op, Op::Sort(_))) {
        if let Op::Filter(f) = op {
            if !cuts || !crate::ops::sort::topn::count_cut::suffix_cut_on(p, g, sd, *f, sums) {
                return None;
            }
            having = true;
        }
    }
    let first = sd.keys.first()?;
    let e = crate::props::code::entry(sd.tlist, first.resno)?;
    let e = match (e.as_var().filter(|v| v.varno == types_nodes::primnodes::OUTER_VAR && v.varlevelsup == 0), project) {
        (Some(v), Some(l)) => crate::props::code::entry(l, v.varattno)?,
        _ => e,
    };
    let a = e.as_aggref()?;
    let d = rec.aggs.iter().find(|x| x.d.aggno as i32 == a.aggno && x.d.mods.distinct == 0)?;
    let counted = matches!(d.d.class, Class::CountStar | Class::CountAny);
    let lead = match counted {
        true => LEAD_COUNT,
        false if sums && matches!(d.d.class, Class::SumI64 | Class::SumI128 | Class::SumNum | Class::SumF32 | Class::SumF64) && d.d.dscale_slot == gravity_decompose::ops::shared::agg::NONE && (d.d.slot as usize) < rec.layout.nslots() => lead_of(rec.layout.slot_kind(d.d.slot as usize))?,
        false => return None,
    };
    // a count ordered by int8's own `<` / `>` (its default btree family), a sum by its result type's: another family's
    // order is not the word's
    let typ = if counted { types_core::catalog::INT8OID } else { a.aggtype };
    let own = gx.facts.key(gravity_decompose::catalog::KeyReq::sort(first.opno)).is_some_and(|o| o.opcintype == typ && o.dir() == Some(first.desc));
    // PostgreSQL projects every group: nothing on the way to the top may raise, the lead's own final aside (above)
    let raise = crate::effects::visible::topn::can_raise_but(px, q, g, (lead != LEAD_COUNT).then_some(d.d.aggno));
    let star = d.d.class == Class::CountStar && d.d.filter.is_none();
    (own && !raise).then_some(CountLead { q, n, desc: first.desc, slot: d.d.slot, star, lead, nulls_first: first.nulls_first && !having, aggno: d.d.aggno })
}

/// The sort op: the rows already arrive in its order (the emit sorted them), so it hands each on.
struct SortStep;

impl StepMech for SortStep {
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        next(b, env, row)
    }
}

fn make_step<'g, 'p>(px: &mut Px<'g, 'p>, _k: usize, _j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    mcx::pg_box_dyn!(px.mcx(), SortStep => dyn StepMech + 'g)
}

/// The top: the first `n` arrivals' records, in place at the front of the emit's array; `None`: form G's full sort
/// (runs/sort.rs, V4R-118), every arrival kept.
struct TopEnd<'g> {
    n: Option<TopN>,
    /// D449: an entry before the sort can raise, so every group's row runs the pipeline (PostgreSQL projects each group
    /// before its Sort): past `n` arrivals the top drops the row instead of ending the pipeline.
    all: bool,
    naggs: usize,
    node: i32,
    /// Frame words: the emit's array and state, and the arrivals so far.
    w_ptrs: i32,
    w_state: i32,
    w_m: i32,
    /// RUNS-18 (`ties_lead`): the emit's ordered count (`W_SORTED`), past which the top WITH TIES pulls no arrival.
    w_lead: Option<i32>,
    /// D2862 (b): the columns the top keeps for the drain (order_slot.rs `kept_cols`), beside the records.
    kept: Option<KeptEnd<'g>>,
    /// D3024 (b), sorted: the run table's handle word of a sorted wide emit (table/emit_runs.rs): each claim's call files
    /// the run it kept there.
    runs: Option<u16>,
    /// D3336 (road emit.parmerge): the run table whose runs the section end merges in parallel (`emit_runs::part`): a top
    /// that keeps every record over a sorted wide emit.
    par: Option<crate::table::emit_runs::SortedRuns<'g>>,
}

/// A top's kept columns (`kept_cols`' order: each one's resno and whether it is a numeric, copied) and where they go: a
/// side array of one `stride`-byte entry per arrival (word `i` column `i`'s value, the last word their validity bits:
/// bit `i` set when column `i` is not NULL), taken from the copy cursor (frame words `cursor`: cur, end, chunk) at a
/// fresh call's first arrival, its address in frame word `side`, which the drain reads (`side_word`). The arrivals come
/// in the emit's order and the top keeps them in place, so arrival `m` is the drain's record `m`.
struct KeptEnd<'g> {
    cols: PgVec<'g, (i16, bool)>,
    stride: i64,
    cursor: i32,
    side: i32,
}

/// Emit frame words 12-14 of a top: the kept columns' copy cursor (the side array, then each numeric's copy).
const W_KEPT_CURSOR: [u8; 3] = [12, 13, 14];
/// Emit frame word 15 of a top: its side array of kept columns (`KeptEnd`).
const W_KEPT_SIDE: u8 = 15;

/// D2862 (b): the frame word holding the side array of the top in pipeline `top` (its drain's read of `KeptEnd::side`).
fn side_word(px: &mut Px<'_, '_>, top: usize) -> i32 {
    crate::table::emit_group::word(px, top, W_KEPT_SIDE)
}

fn kept_end<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, g: &'g gravity_decompose::ops::shared::agg::GroupDecl<'p>, node: i32) -> Option<KeptEnd<'g>> {
    let cols = crate::ops::sort::topn::order_slot::kept_cols(px.gx, g);
    if cols.is_empty() {
        return None;
    }
    // the drain keys the side word by the top's graph pipeline: a top made under another pipeline's identity (a remade
    // pipeline's words are keyed by that one) would file it where the drain does not read
    if px.ident(k) != k {
        bug(node, "a top keeping columns for its drain made under another pipeline's identity");
    }
    let w = W_KEPT_CURSOR.map(|i| crate::table::emit_group::word(px, k, i));
    if w[1] != w[0] + 8 || w[2] != w[0] + 16 {
        bug(node, "a top's kept copy cursor words are not adjacent");
    }
    let side = side_word(px, k);
    fires::fire("order_slot.kept");
    Some(KeptEnd { stride: 8 * (cols.len() as i64 + 1), cols: cols.iter().map(|&(r, t)| (r, t == NUMERIC)).collect_in(px.mcx()), cursor: w[0], side })
}

/// The datum word of a kept int or float8 value (an int sign-extended, a float8 by its bits).
fn datum_word(b: &mut FunctionBuilder<'_>, v: Value) -> Value {
    match b.value_type(v) {
        Type::I64 => v,
        Type::F64 => b.conv(ConvOp::Bitcast, Type::I64, v),
        Type::Ptr => b.conv(ConvOp::PtrToInt, Type::I64, v),
        _ => b.conv(ConvOp::Sext, Type::I64, v),
    }
}

fn make_end<'g, 'p>(px: &mut Px<'g, 'p>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    let node = px.gx.pipelines()[k].node_id;
    let Some(g) = px.gx.drained_group(k) else { bug(node, "a top over groups in a pipeline that drains no group") };
    let full = matches!(sort_of(px, k).0.form, SortForm::Runs);
    // D2470: a bound that is not a constant (a Param LIMIT / OFFSET, a generic plan) keeps every arrival like form G's full
    // sort (`cut` is 0 then); the Limit op past the drain reads the bound at run time
    let n = if full { None } else { top_n(px, k) };
    let w_ptrs = crate::table::emit_group::word(px, k, W_PTRS);
    let w_state = crate::table::emit_group::word(px, k, W_STATE);
    let w_m = px.frame_word(WordKey::Open { face: Face::Runs, k, j: u64::MAX, i: crate::frame::px::runs_word::GROUPS_ARRIVALS }) as i32 * 8; // D482
    let w_lead = (n.is_none() && !full && ties_lead(px, k, g)).then(|| crate::table::emit_group::word(px, k, W_SORTED));
    let kept = if n.is_none() && w_lead.is_none() { kept_end(px, k, g, node) } else { None };
    let runs = crate::table::emit_runs::of(px, k).map(|r| r.word);
    if runs.is_some() {
        if w_lead.is_some() || kept.is_some() {
            bug(node, "a sorted wide emit's top with a ties lead or kept columns (emit_group.rs `sorted_wide` admits neither)");
        }
        crate::table::emit_runs::bound(px, k, n);
    }
    let par = crate::table::emit_runs::of(px, k).filter(|_| crate::table::emit_runs::parallel(px, k));
    if par.is_some() {
        fires::fire("emit.parmerge"); // shapecheck fire signal (D3336): a sorted wide emit's runs merged in parallel at its section end
    }
    mcx::pg_box_dyn!(px.mcx(), TopEnd { n, all: can_raise(px, k, g), naggs: g.aggs.len(), node, w_ptrs, w_state, w_m, w_lead, kept, runs, par } => dyn EndMech + 'g)
}

/// A host call's answer below zero raised as its stashed error (a failed pin or copy).
fn raise_below_zero(b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, w: Value) {
    let failed = b.icmp(IntCond::Slt, w, env.zero);
    let e = b.iconst(Type::I64, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64);
    let err = b.select(failed, e, env.zero);
    crate::step::expr::raise_if(b, env, row, err);
}

impl<'g> TopEnd<'g> {
    /// D2862 (b): arrival `m`'s kept columns into its side array entry (a numeric's copy), then their validity word. The
    /// fresh call's first arrival takes the array, one entry per record the emit walks (`EmitState.kept`).
    fn keep(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, m: Value, kp: &KeptEnd<'g>) {
        let Some(words) = env.words else { bug(self.node, "a top's kept columns without the words site") };
        let cur = b.ptr_add(words, env.zero, 1, kp.cursor);
        let stride = b.iconst(Type::I64, kp.stride);
        let (l_room, l_keep) = (b.create_block(&[]), b.create_block(&[]));
        let first = b.icmp(IntCond::Eq, m, env.zero);
        b.cond_br(first, (l_room, &[]), (l_keep, &[]));
        b.switch_to(l_room);
        let st = env.load_word(b, self.w_state);
        let st = b.conv(ConvOp::IntToPtr, Type::Ptr, st);
        let n = b.load(Type::I64, st, ES_KEPT);
        let bytes = b.int_bin(IntBinOp::Mul, n, stride);
        let flags = gravity_ir::HostFlags { may_error: true, touches_memory: true, ..gravity_ir::HostFlags::default() };
        let h = b.declare_host_once("kept_room", &[Type::Ptr, Type::Ptr, Type::I64], Some(Type::I64), crate::ops::sort::topn::raw::kept_room as *const () as usize, flags);
        let p = b.call_host(h, &[env.ctx, cur, bytes], Some(Type::I64)).expect("returns the room");
        raise_below_zero(b, env, row, p);
        env.store_word(b, kp.side, p);
        b.br(l_keep, &[]);
        b.switch_to(l_keep);
        let side = env.load_word(b, kp.side);
        let side = b.conv(ConvOp::IntToPtr, Type::Ptr, side);
        let off = b.int_bin(IntBinOp::Mul, m, stride);
        let at = b.ptr_add(side, off, 1, 0);
        let t = b.bconst(true);
        let mut bits = env.zero;
        for (i, &(resno, num)) in kp.cols.iter().enumerate() {
            let Some(c) = row.col(resno) else { bug(self.node, "a kept column the top's row does not carry") };
            let (v, valid) = (c.val, c.null.map_or(t, |n| b.int_bin(IntBinOp::Xor, n, t)));
            let w = match (num, c.cell.rep) {
                (true, Rep::VarlenaPtr) => {
                    let w = crate::ops::sort::topn::order_slot::copy_word(b, env, (v, valid), cur, 0);
                    raise_below_zero(b, env, row, w);
                    w
                }
                (false, Rep::Word { .. }) => datum_word(b, v),
                _ => bug(self.node, "a kept column whose cell is not its kept form"),
            };
            b.store(at, 8 * i as i32, w);
            let bit = b.iconst(Type::I64, 1i64 << i);
            let bit = b.select(valid, bit, env.zero);
            bits = b.int_bin(IntBinOp::Or, bits, bit);
        }
        b.store(at, 8 * kp.cols.len() as i32, bits);
    }
}

impl<'g> EndMech for TopEnd<'g> {
    fn open_words(&self, w: &mut OpenWords) {
        w.declare(OpenWord::open("top arrivals", Base::Words, self.w_m, Type::I64, 0));
    }

    /// A fresh call's side array and kept copies start over in the cursor's chunk (order_slot.rs `rewind`). D3336: the
    /// section end's call merges its share of the runs first (`emit_runs::part`; it gathers and files nothing).
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        if let Some(r) = &self.par {
            crate::table::emit_runs::part(b, env, r);
        }
        if let Some(kp) = &self.kept {
            crate::ops::sort::topn::order_slot::rewind(b, env, kp.cursor);
        }
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, x: &Exits) -> Carry<'e> {
        let rec = crate::table::emit_group::record_of(b, env, self.naggs);
        let m = env.load_word(b, self.w_m);
        let ptrs = env.load_word(b, self.w_ptrs);
        let ptrs = b.conv(ConvOp::IntToPtr, Type::Ptr, ptrs);
        // the arrival writes at or before the emit's cursor: every earlier record was read already
        let at = b.ptr_add(ptrs, m, 8, 0);
        if let (true, Some(n)) = (self.all, self.n) {
            let (one, n) = (b.iconst(Type::I64, 1), n.value(b, env));
            // D449: past `n` arrivals the row was projected (it got here) and is dropped; the pipeline runs on
            let l_keep = b.create_block(&[]);
            let l_next = b.create_block(&[Type::I64]);
            let kept = b.icmp(IntCond::Slt, m, n);
            b.cond_br(kept, (l_keep, &[]), (l_next, &[row.carry.out]));
            b.switch_to(l_keep);
            b.store(at, 0, rec);
            let m1 = b.int_bin(IntBinOp::Add, m, one);
            env.store_word(b, self.w_m, m1);
            let out = b.int_bin(IntBinOp::Add, row.carry.out, one);
            b.br(l_next, &[out]);
            b.switch_to(l_next);
            return Carry::new(b.block_params(l_next)[0], env.mcx);
        }
        b.store(at, 0, rec);
        if let Some(kp) = &self.kept {
            self.keep(b, env, row, m, kp);
        }
        let one = b.iconst(Type::I64, 1);
        let m1 = b.int_bin(IntBinOp::Add, m, one);
        env.store_word(b, self.w_m, m1);
        if let Some(n) = self.n {
            // D3266 (L1): EXPLAIN ANALYZE ends at `n` arrivals as the plain run does, where PostgreSQL's Sort reads every
            // group the aggregate emits: the frame's nodes print not counted (L2), as a cut emit's (`Emit::desc`)
            crate::frame::instr::partial();
            let n = n.value(b, env);
            let full = b.icmp(IntCond::Sge, m1, n);
            let l_more = b.create_block(&[]);
            b.cond_br(full, (x.l_done, &[]), (l_more, &[]));
            b.switch_to(l_more);
        } else if let Some(w) = self.w_lead {
            // RUNS-18: the lead holds the first `n`, their last one's ties and the row the Limit stops at
            crate::frame::instr::partial();
            let lead = env.load_word(b, w);
            let full = b.icmp(IntCond::Sge, m1, lead);
            let l_more = b.create_block(&[]);
            b.cond_br(full, (x.l_done, &[]), (l_more, &[]));
            b.switch_to(l_more);
        }
        let out = b.int_bin(IntBinOp::Add, row.carry.out, one);
        Carry::new(out, env.mcx)
    }

    /// DC-04: the top's records stay in place at the front of the emit's array (the group's slot, which the drain reads),
    /// so its own slot holds none: it asks no bytes and spills nothing, where decompose's declaration (`topk_budget`)
    /// sized `count + offset` run rows it never writes into the leader's footprint, or the share's runs. The pipeline is a
    /// section only under a sorted wide emit (table/emit_runs.rs), whose claims' runs go to the run table: the slot holds
    /// none there either.
    fn pipeline_desc(&self, _px: &Px<'_, '_>, _k: usize, mut d: Pipeline) -> Pipeline {
        if let Some(br) = crate::frame::iface::breaker_of_mut(&mut d) {
            (br.bytes, br.budget, br.spill) = (0, Budget::Fixed(0), Spill::None);
            // its answer past the grant, as the decider reads this declaration: a program constant of no bytes
            debug_assert_eq!(gravity_program::spill::declared(br), gravity_program::spill::Holds::Bounded(gravity_program::spill::Proof::Constant(0)));
        }
        // D3336: a sorted wide emit on its nominal site takes section ends for the parallel merge (`par`); one the leader
        // keeps (site-less, one call) takes none and its reader merges its one run as before
        if self.par.is_some() && d.driving_site >= 0 {
            d.finish = true;
        }
        d
    }

    /// The emit's kept count cut to the arrivals (at most `n`): the drain walks exactly the top.
    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _x: &Exits) {
        let state = env.load_word(b, self.w_state);
        let st = b.conv(ConvOp::IntToPtr, Type::Ptr, state);
        let m = env.load_word(b, self.w_m);
        let kept = match self.n {
            Some(n) => {
                let n = n.value(b, env);
                let over = b.icmp(IntCond::Sgt, m, n);
                b.select(over, n, m)
            }
            None => m,
        };
        match self.runs {
            None => b.store(st, ES_KEPT, kept),
            // D3024 (b), sorted: the emit's pipeline has a driving site, so a step that closes the call at its open (a zero
            // LIMIT count, D2152) ends a claim's call before its source opens: no array was gathered, no run is filed
            Some(w) => {
                let (l_run, l_end) = (b.create_block(&[]), b.create_block(&[]));
                let none = b.icmp(IntCond::Eq, state, env.zero);
                b.cond_br(none, (l_end, &[]), (l_run, &[]));
                b.switch_to(l_run);
                b.store(st, ES_KEPT, kept);
                crate::table::emit_runs::file(b, env, w, st, kept);
                b.br(l_end, &[]);
                b.switch_to(l_end);
            }
        }
        let _ = self.node;
    }
}

/// One output column of the drain: a group key's cell as it is, or the projection's expression over the group row.
enum Out {
    Key { col: i16 },
    Ex { id: ExId, cell: Cell },
    /// errorhold (D1344): a volatile order expression's value from its order slot `s` (order_slot.rs `read_back`), the
    /// value the sort ordered by, as PG's Sort carries the Agg's one value; typed `typid` (an int or a float8 word).
    Slot { s: usize, typid: u32 },
    /// D2862 (b): kept column `i` of the top's side array (`KeptEnd`), typed `typid` (an int or float8 word, a numeric).
    Kept { i: u8, typid: u32 },
}

/// The drain: the top's records again, each as the top pipeline's projected row.
struct TopDrain<'g, 'p> {
    emit: Emit<'g, 'p>,
    outs: PgVec<'g, (i16, Out)>,
    node: i32,
    /// What the drain's pipeline consumes, in its declared order: the group's slot and life, the top's, then each other
    /// slot the pipeline consumes (a following step's: a hash probe's build, a merge's inner), as `Px::consumed` reads it.
    consumes: PgVec<'g, (u8, LifeId)>,
    /// D2862 (b): the top's side array (`KeptEnd`) when it keeps columns: its frame word, the entry's bytes and columns.
    side: Option<(i32, i64, usize)>,
    /// D3336 (A1a, face/partfill.rs): the drain runs as a parted window's fill claims over K-cut parts: the window's claims
    /// site, the drain's nominal driving site.
    cut: Option<usize>,
}

fn make_drain<'g, 'p>(px: &mut Px<'g, 'p>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    let gx = px.gx;
    let mcx = px.mcx();
    let p = &gx.pipelines()[k];
    let node = p.node_id;
    let Source::Breaker { slot } = p.source else { bug(node, "a top's drain over a source that is not its breaker") };
    let Some(top) = gx.life_of(k, slot) else { bug(node, "a top's drain with no producer") };
    let Some(g) = gx.drained_group(top.0 as usize) else { bug(node, "a top's drain whose top drains no group") };
    let tp = &gx.pipelines()[top.0 as usize];
    let ls = crate::ops::sort::topn::order_slot::projections(&tp.ops[..tp.ops.iter().position(|op| matches!(op, Op::Sort(_))).unwrap_or(tp.ops.len())], mcx);
    let Some(project) = ls.first() else { shape(node, "a top over groups without a projection") };
    let pnode = tp.node_id;
    let mut outs = mcx::reserved_in(project.len(), mcx);
    let backs = crate::ops::sort::topn::order_slot::read_back_cols(gx, g);
    let kc = crate::ops::sort::topn::order_slot::kept_cols(gx, g);
    for te in project.iter().filter_map(|n| n.as_target_entry()) {
        if backs.contains(&te.resno) {
            outs.push_in((te.resno, Out::Slot { s: usize::MAX, typid: 0 })); // its slot once the emit's record is laid out
            continue;
        }
        if let Some(i) = kc.iter().position(|c| c.0 == te.resno) {
            outs.push_in((te.resno, Out::Kept { i: i as u8, typid: kc[i].1 }));
            continue;
        }
        // through a subquery scan's projections back to the group's row (#227)
        let Some(e) = crate::ops::sort::topn::order_slot::to_group_row(&ls[1..], te.expr) else { shape(node, "a projected expression over a subquery scan's row") };
        let key = e.as_var().filter(|v| v.varno == OUTER_VAR && g.keys.iter().any(|kd| kd.resno == v.varattno));
        outs.push_in((te.resno, match key {
            Some(v) => Out::Key { col: v.varattno },
            None => {
                let Some(o) = gx.out_of(te.expr) else { shape(node, "a projected column without an output rep") };
                Out::Ex { id: crate::step::expr::compile(px, e, None, pnode), cell: Cell::of_expr(o, 0, 0, Nullable::Maybe) }
            }
        }));
    }
    // D3024 (b), sorted: over a sorted wide emit the drain merges its claims' runs (table/emit_runs.rs)
    let emit = Emit::new(px, k, g, true).merging(crate::table::emit_runs::of(px, top.0 as usize));
    let back = crate::ops::sort::topn::order_slot::read_back(gx, &emit.rec);
    for (r, o) in outs.iter_mut() {
        if let Out::Slot { s, typid } = o {
            let Some(&(_, slot, t)) = back.iter().find(|x| x.0 == *r) else { bug(node, "a read-back order column without its slot") };
            (*s, *typid) = (slot, t);
        }
    }
    let side = (!kc.is_empty()).then(|| (side_word(px, top.0 as usize), 8 * (kc.len() as i64 + 1), kc.len()));
    let group = (g.slot, emit.rec.home);
    // D4620: a cascade base's records sit in its own region and in each cascaded set's (ops/agg/cascade.rs): the drain
    // reads the merged runs' records of every one, so it holds each cascaded set's life as it holds the group's
    let cascades: PgVec<'_, (u8, LifeId)> = gx.graph.cascades.iter().filter(|c| c.base == g.producer).map(|c| (c.slot, c.producer)).collect_in(mcx);
    // D295: the drain fuses with the group's life (and the top's) only while no pipeline in (that life, k] produces the
    // slot again: decompose frees the group's slot at the top, so a later breaker (the drain's own, an outer fold) may take
    // it, which validate_dag refuses either way (a pipeline consuming the slot it produces; a life read past its next
    // producer). No M1 form keeps those rows by value: the row top (runs.topk_rows, M3) does, its drain handed the
    // pipeline here (M3 L0-g; NotBuilt by its name until M3 registers it).
    let again = |(s, l): (u8, LifeId)| (l.0 as usize + 1..=k).any(|m| gx.pipelines()[m].desc.produces() == Some(s));
    if again(group) || again((slot, top)) || cascades.iter().any(|&c| again(c)) {
        if !back.is_empty() {
            // the row top keeps the top pipeline's projected rows by value: a volatile order expression evaluated again
            shape(node, "a top ordered by a volatile expression over the groups (evaluated again past the order pass)");
        }
        crate::mechs_used::declined(crate::admit::refusal::Variant::Kind(KindId::RunsTopkGroups)); // MECHCORPUS: the node is another kind's form
        return (crate::frame::iface::kind_impl::<SourceMaker>(&node, KindId::RunsTopkRows).0)(px, k);
    }
    // D2342 / D2345: the group's life first (the resumed gather reads consumed region 0), then the top's, then every other
    // slot the pipeline consumes; the list is declared so a following step (a hash probe, a merge's inner) finds its region
    let d = &p.desc;
    let others = d.consumes.iter().enumerate().filter(|&(_, &s)| s != group.0 && s != slot && !cascades.iter().any(|c| c.0 == s));
    let others = others.filter_map(|(j, &s)| Some((s, d.consumes_from.get(j).copied().or_else(|| gx.life_of(k, s))?)));
    let consumes: PgVec<'_, (u8, LifeId)> = [group, (slot, top)].into_iter().chain(cascades.iter().copied()).chain(others).collect_in(mcx);
    px.set_consumes(consumes.iter().map(|&(s, _)| s).collect()); // alloc: Program
    // D3336 (A1a, face/partfill.rs): a plain drain into a parted window over K-cut parts runs as the window's fill claims
    let cut = if side.is_none() { crate::face::partfill::cut_form(px, k, &emit) } else { None };
    let emit = emit.cutting(cut.map(|(t, o, _)| (t, o)));
    let cut = cut.map(|c| c.2);
    mcx::pg_box_dyn!(mcx, TopDrain { emit, outs, node, consumes, side, cut } => dyn SourceMech + 'g)
}

impl<'g, 'p> SourceMech for TopDrain<'g, 'p> {
    fn rereads_top(&self) -> bool { true }
    /// The declared consumes (`make_drain`), in order.
    fn pipeline_desc(&self, px: &Px<'_, '_>, k: usize) -> Pipeline {
        let mut d = px.gx.pipelines()[k].desc;
        (d.consumes, d.consumes_from) = (&[], &[]);
        let d = self.consumes.iter().fold(d, |d, &(s, l)| d.consuming_from_in(px.arena, s, l));
        // D3336 (A1a): a K-cut fill's claims are the window's objects (exec answers them, `WINDOW_CUT`), on its claims site;
        // a call a pool thread cannot run keeps it on the leader, its one claim reading every record, dealt
        match self.cut {
            Some(site) => {
                let parallel = match crate::hir::hosted_serial(px, k) {
                    Some(reason) => gravity_program::Parallel::Ineligible { site: site as i16, reason },
                    None => gravity_program::Parallel::Eligible,
                };
                Pipeline { parallel, driving_site: site as i16, ..d }
            }
            None => d,
        }
    }

    fn nominal_site(&self, _px: &Px<'_, '_>) -> Option<usize> {
        self.cut
    }

    fn open_words(&self, w: &mut OpenWords) {
        self.emit.open_words(w);
    }

    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        self.emit.open(b, env)
    }

    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        self.emit.next_window(b, env, l_eof)
    }

    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        let g = self.emit.group_row(b, env, j, carry, next);
        let ncols = self.outs.iter().map(|(r, _)| r.max(&0).unsigned_abs() as usize).max().unwrap_or(0);
        let mut row = RowVals { row: Row { cells: mcx::reserved_in(self.outs.len(), env.mcx) }, vals: mcx::reserved_in(self.outs.len(), env.mcx), cols: mcx::pg_vec![in env.mcx; None; ncols], carry: g.carry.copy_in(env.mcx), next, at: j };
        let t = b.bconst(true);
        // D2862 (b): record `j`'s side array entry and its validity bits, as the top wrote them (`TopEnd::keep`)
        let kept = self.side.map(|(w, stride, n)| {
            let side = env.load_word(b, w);
            let side = b.conv(ConvOp::IntToPtr, Type::Ptr, side);
            let stride = b.iconst(Type::I64, stride);
            let off = b.int_bin(IntBinOp::Mul, j, stride);
            let at = b.ptr_add(side, off, 1, 0);
            (at, b.load(Type::I64, at, 8 * n as i32))
        });
        for (resno, o) in &self.outs {
            let (cell, v, n) = match o {
                Out::Key { col } => {
                    let c = g.col(*col).unwrap_or_else(|| bug(self.node, "a group key the group row does not carry"));
                    (*c.cell, c.val, c.null)
                }
                Out::Ex { id, cell } => {
                    let (v, valid) = crate::step::expr::emit(b, env, self.node, &g, *id);
                    (*cell, v, Some(b.int_bin(IntBinOp::Xor, valid, t)))
                }
                Out::Slot { s, typid } => {
                    // record `j` of the array the emit left (`Emit::record`), its slot word and validity as the pass wrote them
                    let ptrs = env.load_word(b, self.emit.words[W_PTRS as usize]);
                    let ptrs = b.conv(ConvOp::IntToPtr, Type::Ptr, ptrs);
                    let at = b.ptr_add(ptrs, j, 8, 0);
                    let rec = b.load(Type::Ptr, at, 0);
                    let (v, ok) = crate::ops::sort::topn::order_slot::slot_read(b, env, &self.emit.rec, rec, *s);
                    let cell = Cell::of_record(Rep::Word { bytes: 8 }, *typid, gravity_program::NumTypmod::NONE, self.emit.rec.home, *s as u16, Nullable::Maybe);
                    (cell, v, Some(b.int_bin(IntBinOp::Xor, ok, t)))
                }
                Out::Kept { i, typid } => {
                    // its word (a numeric's copy, living as long as the statement's pins) and its bit; read as the record's
                    let Some((at, bits)) = kept else { bug(self.node, "a kept column without the top's side array") };
                    let v = b.load(Type::I64, at, 8 * *i as i32);
                    let m = b.iconst(Type::I64, 1i64 << i);
                    let x = b.int_bin(IntBinOp::And, bits, m);
                    let null = b.icmp(IntCond::Eq, x, env.zero);
                    let rep = if *typid == NUMERIC { Rep::VarlenaPtr } else { Rep::Word { bytes: 8 } };
                    (Cell::of_record(rep, *typid, gravity_program::NumTypmod::NONE, self.emit.rec.home, *i as u16, Nullable::Maybe), v, Some(null))
                }
            };
            if *resno > 0 {
                row.cols[*resno as usize - 1] = Some(row.row.cells.len() as u16);
            }
            row.row.cells.push_in(cell);
            row.vals.push_in((v, n));
        }
        row
    }

    fn write_back(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, cur: Value, out: Value) {
        self.emit.write_back(b, env, cur, out);
    }
}
#[cfg(test)]
mod d1309_tests {
    use gravity_decompose::catalog::typefacts::row_of;
    use gravity_kernels::host_abi::SK_BPCHAR;

    /// D1309: a bpchar key's kept copy sorts trimmed (SK_BPCHAR, as the Code arm's word); a text / varchar / bytea copy
    /// does not trim.
    #[test]
    fn a_bpchar_key_copy_sorts_trimmed() {
        let word = |t: u32| crate::dict::coderep::copy_word(3, 100, crate::dict::coderep::sk_bpchar_of(row_of(t)));
        assert_ne!(word(1042) & SK_BPCHAR, 0, "a bpchar copy's word trims (bpcharcmp)");
        for t in [25, 1043, 17] {
            assert_eq!(word(t) & SK_BPCHAR, 0, "type {t}'s copy compares every byte");
        }
    }
}
