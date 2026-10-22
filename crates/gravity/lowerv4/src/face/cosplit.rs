//! D3190, merge.cosplit: a merge join over two sorted inputs run wide. Exec cuts both sorted runs at the same keys once
//! both ran (exec ops/sort/cosplit.rs: splitters sampled from the outer's run, each run cut at their lower bounds on
//! the first merge keys), and each claim is one key range: the outer's emit reads its rows of the range, the inner's
//! reader its slice of the run, and the merge's machine runs per claim with its words reset at the claim's open. Equal
//! keys never straddle two ranges, so every join type is the union of the ranges' answers, a RIGHT / FULL fill each
//! range's own (W_MI and ENDOUTER per claim). An outer whose sort put its rows in runs is cut through the runs' fences:
//! a claim's outer rows are the merge of its slice of each run, laid over its owner's area (kernels sort
//! `sort_cosplit_begin`). An inner that is not one sealed resident run at exec is one claim over the whole of both: the
//! serial form's rows.
//!
//! The form's face: the sort's emit (ops/sort/lower.rs) declares it at the outer's emit, before the merge's step is
//! made; the merge's step (ops/mergejoin/lower.rs) and the inner's reader (face/slot_run.rs) find the proto (`of`) and
//! read the claim's range (`range`). The form (`declare`): the merge is the pipeline's first op past the emit's
//! projections over a full sort's run, its inner a full sort's run its pipeline spools whole (no pulled or lazy inner,
//! no union arm), its rows reach, through filters, gates and projections only, an end no row order changes
//! (effects::visible::wide_visible: a fold or a sort with no LIMIT, lock, visible call or error), and its first merge
//! clause compares two sort keys the runs order by their `[flags, word]` pair alone (no image tail, no comparator:
//! exec's cut compares those words; a type whose word is the value, two equal values one word), of one type (or two
//! integer types, whose words are the sign-extended value), in the clause's direction and NULLS placement on both
//! sides. The keys after the first that hold the same, in order, are cut on too (at most `COSPLIT_MAX_KEYS`). A helper
//! file (ROWS = NONE). Owner M11-MERGE. Budget 200 lines.
use gravity_abi::breaker::BreakerKind;
use gravity_abi::ctx::hdr;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::sort::SortForm;
use gravity_decompose::{Op, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Type, Value};
use gravity_abi::program::COSPLIT_MAX_KEYS;
use gravity_program::protos::{cosplit_arg, Proto, ProtoKind, COSPLIT_HDR_WORDS, COSPLIT_PARTS, COSPLIT_PART_WORDS, COSPLIT_WHOLE};
use types_nodes::list::NodeList;
use types_nodes::primnodes::{INNER_VAR, OUTER_VAR};
use types_nodes::Node;

use crate::face::runs::Shape;
use crate::frame::env::Env;
use crate::frame::px::Px;

/// The co-split's side in its range table.
#[derive(Clone, Copy)]
pub(crate) enum Side {
    Outer = 0,
    Inner = 1,
}

/// The form the outer's emit takes: the range table's handle word, the nominal driving site (the outer producer's) and
/// the rows the claims cover (the outer producer's estimate).
#[derive(Clone, Copy)]
pub(crate) struct At {
    pub(crate) h: u16,
    pub(crate) site: usize,
    pub(crate) rows: u64,
}

/// The integer types whose run words are their sign-extended values (a merge clause across them cuts both runs alike).
const INTS: [u32; 3] = [types_core::catalog::INT2OID, types_core::catalog::INT4OID, types_core::catalog::INT8OID];

/// The other by-value types whose run word is the value itself, two equal values one word (a float's is not: -0 and 0).
const WORDS: [u32; 6] = [types_core::catalog::OIDOID, types_core::catalog::BOOLOID, types_core::catalog::DATEOID, types_core::catalog::TIMEOID, types_core::catalog::TIMESTAMPOID, types_core::catalog::TIMESTAMPTZOID];

/// Pipeline `k`'s outer emit over full sort `producer`'s run of `outer` shape: the co-split form when it holds, its
/// CoSplit proto declared (the merge's step and the inner's reader find it: `of`).
pub(crate) fn declare(px: &mut Px<'_, '_>, k: usize, producer: usize, outer: &Shape) -> Option<At> {
    let gx = px.gx;
    if k >= gx.pipelines().len() || px.ident(k) != k || px.protos.iter().any(|p| p.kind == ProtoKind::CoSplit && p.pipeline as usize == k) {
        return None;
    }
    let p = &gx.pipelines()[k];
    let Source::Breaker { slot: outer_slot } = p.source else { return None };
    let m = merge_at(p)?;
    let Op::Join(JoinDesc::Merge { slot, clauses, reversals, nulls_first, .. }) = &p.ops[m] else { return None };
    let inner = gx.life_of(k, *slot).map(|l| l.0 as usize).filter(|&q| q < gx.pipelines().len())?;
    let in_rerun = |q: usize| gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(q as u16)));
    // the merge's rows reach the end through filters, gates and projections only (no step that holds state across claims)
    let after = p.ops[m + 1..].iter().all(|op| matches!(op, Op::Filter(_) | Op::Gate(_) | Op::Project(_)));
    let plain = after && !in_rerun(k) && !in_rerun(inner) && !in_rerun(producer) && !px.lazy.iter().any(|l| l.1 == k || l.0 == k) && !px.serial.contains(&k);
    let fillers = crate::ops::mergejoin::lazy_outer::fillers(gx, k, *slot);
    let quiet = fillers == [inner] && !crate::effects::visible::merge::inner_may_act(gx, inner, usize::MAX, None) && !crate::ops::mergejoin::exact_inner::pulled(px, inner);
    let sorted = |q: usize| crate::frame::iface::end_breaker(gx, q) == Some(BreakerKind::Sort) && crate::frame::iface::run_holds_rows(gx, q) && crate::ops::sort::lower::union_arm(&gx.pipelines()[q]).is_none();
    if !plain || !quiet || !sorted(producer) || !sorted(inner) || crate::effects::visible::wide_visible(gx, k).is_some() {
        return None;
    }
    let (isd, icols) = crate::ops::sort::lower::sort_of(px, inner);
    if !matches!(isd.form, SortForm::Runs) || isd.presorted > 0 {
        return None;
    }
    let (osd, _) = crate::ops::sort::lower::sort_of(px, producer);
    let ish = Shape::plan(px, gx.pipelines()[inner].node_id, &isd.keys, Some(isd.tlist), icols);
    let n = (0..clauses.len().min(COSPLIT_MAX_KEYS)).take_while(|&i| {
        let Some(op) = clauses.nth(i).as_op_expr().filter(|o| o.args.len() == 2) else { return false };
        let (dir, nf) = (reversals.get(i).copied(), nulls_first.get(i).copied());
        let o = word_key(outer, osd.tlist, op.args.nth(0), OUTER_VAR, i);
        let n = word_key(&ish, isd.tlist, op.args.nth(1), INNER_VAR, i);
        match (o, n) {
            (Some((ot, od, on)), Some((it, id, inf))) => (ot == it || INTS.contains(&ot) && INTS.contains(&it)) && od == id && on == inf && dir == Some(od) && nf == Some(on),
            _ => false,
        }
    }).count();
    let site = px.pipeline_site(producer)?;
    if n == 0 {
        return None;
    }
    let h = px.proto_words(1);
    px.declare_proto(Proto::new(ProtoKind::CoSplit, k as u16).slot(outer_slot).words(h, 1).arg(cosplit_arg(*slot, n as u8)));
    fires::fire("lower.merge.cosplit"); // shapecheck fire signal (D3190): a merge join lowered in the co-split form
    Some(At { h, site, rows: gx.pipelines()[producer].desc.predicted_rows })
}

/// The op index of pipeline `p`'s merge join when it is the first op past the emit's projections (the sort emit's own).
pub(crate) fn merge_at(p: &gravity_decompose::PipelineNode<'_>) -> Option<usize> {
    let m = p.ops.iter().position(|op| !matches!(op, Op::Project(_)))?;
    matches!(p.ops[m], Op::Join(JoinDesc::Merge { .. })).then_some(m)
}

/// Pipeline `k`'s co-split table word, when its merge takes the form (`declare`).
pub(crate) fn of(px: &Px<'_, '_>, k: usize) -> Option<u16> {
    let k = px.ident(k);
    px.protos.iter().find(|p| p.kind == ProtoKind::CoSplit && p.pipeline as usize == k).map(|p| p.words.base)
}

/// Run key `i` of `sh` when merge clause argument `arg` (a `varno` column, through a RelabelType) is it and it orders by
/// its `[flags, word]` pair alone, its word the value (`INTS`, `WORDS`): its type, direction and NULLS placement.
fn word_key(sh: &Shape, tlist: &NodeList<'_>, arg: Node<'_>, varno: i32, i: usize) -> Option<(u32, bool, bool)> {
    let v = arg.as_relabel_type().map_or(arg, |r| r.arg).as_var().filter(|v| v.varno == varno && v.varlevelsup == 0 && v.varattno > 0)?;
    // the run's column: the Sort's target list entry of the clause's column, an OUTER_VAR of the run's row
    let te = tlist.iter().filter_map(|n| n.as_target_entry()).find(|te| te.resno == v.varattno)?;
    let col = te.expr.as_var().filter(|x| x.varno == OUTER_VAR && x.varattno > 0)?.varattno as usize - 1;
    let key = sh.keys.get(i).filter(|x| x.col as usize == col && x.tail == 0 && x.proc == 0 && !x.user)?;
    let c = sh.cols.get(col).filter(|c| c.tf.byval && (INTS.contains(&c.tf.typid) || WORDS.contains(&c.tf.typid)))?;
    Some((c.tf.typid, key.desc, key.nulls_first))
}

/// The claim's `[lo, hi)` row positions of `side` (`I64`, each at most `COSPLIT_WHOLE`; a reader clamps them to its rows).
/// A claim `[a, b)` of the table's parts (`a < b <= parts`: the pool's sizer may hand out several at once) reads part a's
/// start to part b - 1's end of each run (consecutive parts are consecutive key ranges: one range of each run); the
/// section's end claim and a first call with no claim read nothing (`[MAX, MAX)`); any other claim (`ALL`) the whole run.
pub(crate) fn range(b: &mut FunctionBuilder<'_>, env: &Env, h: u16, side: Side) -> (Value, Value) {
    let lo = b.load(Type::I64, env.ctx, hdr::CLAIM_LO as i32);
    let hi = b.load(Type::I64, env.ctx, hdr::CLAIM_HI as i32);
    let tbl = b.load(Type::Ptr, env.params, h as i32 * 8);
    let parts = b.load(Type::I64, tbl, 8 * COSPLIT_PARTS as i32);
    let some = b.icmp(IntCond::Ult, lo, hi);
    let fits = b.icmp(IntCond::Ule, hi, parts);
    let parted = b.int_bin(IntBinOp::And, some, fits);
    // the entries read at part 0 when the claim is no parts' (the table always holds part 0), so the address stays inside it
    let last = b.int_bin(IntBinOp::Sub, hi, env.one);
    let (first, last) = (b.select(parted, lo, env.zero), b.select(parted, last, env.zero));
    let stride = b.iconst(Type::I64, 8 * COSPLIT_PART_WORDS as i64);
    let at = |b: &mut FunctionBuilder<'_>, p: Value| {
        let off = b.int_bin(IntBinOp::Mul, p, stride);
        b.ptr_add(tbl, off, 1, 0)
    };
    let (at_first, at_last) = (at(b, first), at(b, last));
    let word = 8 * COSPLIT_HDR_WORDS as i32 + 16 * side as i32;
    let e_lo = b.load(Type::I64, at_first, word);
    let e_hi = b.load(Type::I64, at_last, word + 8);
    let whole = b.iconst(Type::I64, COSPLIT_WHOLE as i64);
    let none_lo = b.select(some, env.zero, whole);
    (b.select(parted, e_lo, none_lo), b.select(parted, e_hi, whole))
}

/// `v` at most `n` (both `I64`, `n` not negative and `v` at most `COSPLIT_WHOLE`).
pub(crate) fn clamp(b: &mut FunctionBuilder<'_>, v: Value, n: Value) -> Value {
    let past = b.icmp(IntCond::Sgt, v, n);
    b.select(past, n, v)
}
