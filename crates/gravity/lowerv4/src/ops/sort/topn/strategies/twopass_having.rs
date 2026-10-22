//! HAVING over a keyed fold, road `twopass.having` (H08 R4-L14): one final decides first, the others after. A HAVING
//! that bounds a final read whole from its slot (a count, an integer min / max, a sum over int2 / int4: `whole`) from
//! below (`>`, `>=`, `=`) or above (`<`, `<=`) by K, alone or ANDed with other quals, admits no group past the bound, so
//! the group emit keeps only the records within it (`keep`, before any final is taken) and the HAVING filter still runs
//! over those. The floor is exact only when no final can raise and no qual before it can (D453: PG finalizes every group
//! and runs its quals in order; else there is no floor), so a skipped group changes nothing. A NULL final (a group of
//! NULL inputs) fails its qual, so its slot's word, whatever it holds, skips nothing PG returns. K is a literal or a
//! parameter read from its bound word per execution, never folded (lower/src/key.rs keys a by-value literal by its type
//! only: one program serves every K); a NULL K keeps no group. A literal's word is the HAVING filter's own
//! (`Px::claim_const`, taken by the filter's compile). The same bound admits pass 2's keys when the costly aggregates
//! fold in a second pass (keyed_twopass, pass 1 folding the bounded final). PSP2-43: a count's floor K may be a numeric
//! (`count(*) > 1.5`, the count cast to numeric): the emit's keep reads its int8 floor once per execution (topn raw.rs
//! `count_floor`); the two-pass forms, whose kernels read K's word as an int, take an integer K only. Budget 160 lines
//! (files.tsv, +25 % gate). Owner M1-KEYED.
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value};
use gravity_lower::Place;
use gravity_abi::breaker::SlotKind;
use gravity_decompose::ops::shared::agg::{FinalForm, GroupForm, Register};
use gravity_decompose::ops::shared::agg_class::Class;
use gravity_decompose::Op;
use types_core::catalog::{INT2OID, INT4OID, INT8OID};
use types_nodes::Node;

use mcx::{CollectIn, PgVec};

use crate::table::face::CrossPass;
use crate::frame::cancel;
use crate::frame::env::Env;
use crate::frame::px::Px;

use crate::table::rec::GroupRec;

crate::strategies! {
    /// HAVING over a keyed fold: pass 1 counts, pass 2 folds admitted groups.
    TWOPASS_HAVING = 48, "twopass.having", HavingKeyed;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(TWOPASS_HAVING, &Having)], ..crate::ops::NONE };

/// `twopass.having`'s registry value: the floor is this file's functions (the emit and the two-pass end call them).
pub(crate) struct Having;

/// pg_opfamily `integer_ops` (btree over int2 / int4 / int8) and its strategies `<` .. `>` (stratnum.h).
const INTEGER_OPS: u32 = 1976;
/// pg_opfamily `numeric_ops` (btree) and pg_type `numeric`: the count cast to numeric (`catalog::casts::int_numeric`).
const NUMERIC_OPS: u32 = 1988;
const NUMERICOID: u32 = 1700;
const BT_LT: u8 = 1;
const BT_EQ: u8 = 3;
const BT_GT: u8 = 5;

/// The bound: the final in slot `slot` (byte `off` of the group's record, a `count` or another `whole` final) must be
/// over (`gt`: strictly) or at least K, or with `below` under (`gt`: strictly) or at most K; K the bound word `word` (an
/// int4 when `int4`; a numeric's pointer when `num`, a count's floor only, whose int8 floor the emit's keep reads).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Floor {
    pub off: i32,
    pub slot: u16,
    pub word: u16,
    pub gt: bool,
    pub int4: bool,
    pub num: bool,
    pub below: bool,
    pub count: bool,
}

/// Whether aggregate `x`'s final is its slot's word as it stands, a signed integer: a count, or an int2 / int4 / int8
/// min / max or a sum over int2 / int4 (their identity finals). Another slot's word is an image, a float's order image,
/// a scaled numeric or a state.
fn whole(rec: &GroupRec<'_, '_>, x: &crate::table::rec::Agg<'_>) -> bool {
    let j = x.d.slot as usize;
    if x.d.mods.distinct != 0 || j >= rec.layout.nslots() {
        return false;
    }
    match (rec.layout.slot_kind(j), x.d.reg) {
        (SlotKind::Count, _) => matches!(x.d.class, Class::CountStar | Class::CountAny),
        (SlotKind::MinI64, Register::MinInt) | (SlotKind::MaxI64, Register::MaxInt) | (SlotKind::SumI64, Register::SumInt) => x.d.final_ == FinalForm::Identity,
        _ => false,
    }
}

/// The bound of one qual on a `whole` final: its operator is an integer btree `<`, `<=`, `=`, `>=` or `>` of the final
/// (int2, int4 or int8) and K (int8 or int4), read from the builtin btree table (the HAVING's quals are already
/// canonical: normalize's `commute` put K on the right, `K < count(*)` reads `count(*) > K`; its `retype` made a literal
/// the count's int8); `=` bounds from below. A count is `count(*)` or `count(x)`, a FILTER or not: the emit's keep
/// reads that final's own slot, and a two-pass pass 1 folds that final (or keeps a row count never under it), so a group
/// past the bound fails the HAVING either way. K is a parameter (one word per parameter) or a by-value literal whose
/// occurrence's word the floor claims ahead of the HAVING filter's compile (one word per occurrence, exec admit's
/// template). PSP2-43: or a numeric btree `>`, `>=` or `=` of a count cast to numeric (`numeric(int8)`) and a numeric K
/// (a parameter, or a literal's word, its pointer).
fn qual_floor<'p>(px: &mut Px<'_, 'p>, rec: &GroupRec<'_, '_>, q: Node<'p>) -> Option<(Floor, Node<'p>)> {
    let o = q.as_op_expr()?;
    let (family, left, right, strategy) = gravity_decompose::effects::builtin_btree_member(o.opno)?;
    let (int4, num) = match (family, left, right) {
        (INTEGER_OPS, INT2OID | INT4OID | INT8OID, INT8OID) => (false, false),
        (INTEGER_OPS, INT2OID | INT4OID | INT8OID, INT4OID) => (true, false),
        (NUMERIC_OPS, NUMERICOID, NUMERICOID) => (false, true),
        _ => return None,
    };
    if !(BT_LT..=BT_GT).contains(&strategy) || o.args.len() != 2 {
        return None;
    }
    let (gt, below) = (matches!(strategy, BT_LT | BT_GT), strategy < BT_EQ);
    let a = match num {
        true => o.args.nth(0).as_func_expr().filter(|f| gravity_decompose::catalog::casts::int_numeric(f.funcid) && f.args.len() == 1)?.args.nth(0).as_aggref()?,
        false => o.args.nth(0).as_aggref()?,
    };
    let k = o.args.nth(1);
    if k.as_param().is_none() && k.as_const().is_none() {
        return None;
    }
    let agg = rec.aggs.iter().find(|x| x.d.aggno as i32 == a.aggno && whole(rec, x))?;
    let count = rec.layout.slot_kind(agg.d.slot as usize) == SlotKind::Count;
    // `count_floor` rounds a numeric K to a count's least kept value
    if num && (below || !count) {
        return None;
    }
    let place = match k.as_param() {
        Some(_) => {
            let (q, node) = (px.making.0, px.gx.pipelines().get(px.making.0).map_or(0, |p| p.node_id));
            crate::lifetime::bound::word(px, crate::lifetime::bound::Feed::Qual, q, k, node)
        }
        None if num => px.claim_ref_const(k)?,
        None => px.claim_const(k)?,
    };
    let Place::Word(word) = place else { return None };
    Some((Floor { off: rec.layout.slot_off(agg.d.slot as usize) as i32, slot: agg.d.slot as u16, word, gt, int4, num, below, count }, k))
}

/// The bound pipeline `k`'s leading HAVING filters require (their first bounding qual's), when `twopass.having` is on.
pub(crate) fn floor<'p>(px: &mut Px<'_, 'p>, k: usize, rec: &GroupRec<'_, '_>) -> Option<Floor> {
    floor_at(px, k, rec).map(|f| f.0)
}

/// `floor` with its qual's K (a parameter or a literal).
pub(crate) fn floor_at<'p>(px: &mut Px<'_, 'p>, k: usize, rec: &GroupRec<'_, '_>) -> Option<(Floor, Node<'p>)> {
    let f = find(px, k, rec)?;
    fires::fire("twopass.having"); // shapecheck fire signal (REGISTRY KEYED-01 / NEW-lowerv4-a2-281..284)
    Some(f)
}

/// D3153: the generic owner's bound for group end `k` over `rec` (keyed_hashagg `end_of`: a one-pass group, no split,
/// pass 2, front, top or hosted state), packed for `OwnerDesc::floor` (`OwnerDesc::runs_floor`): the bound its one
/// reader's leading HAVING filters require (`floor`'s rules; an integer K), so exec owner.rs lays only the records within
/// it in its runs and the reader's keep and filter, still the exact gate, walk the groups that can pass (without it every
/// group is laid, gathered and tested). A group more than one pipeline reads, or a rerun set's, keeps all.
/// Only a hashed group's records are the HAVING's groups: a DISTINCT's records are its argument's (keyed.distinct_plain's
/// pair form under a plain aggregate, a grouped DISTINCT's (key, argument) pairs), whose counts are parts of one row's.
pub(crate) fn owner_floor<'p>(px: &mut Px<'_, 'p>, k: usize, rec: &GroupRec<'_, '_>) -> Option<i64> {
    let g = rec.g;
    if g.form != GroupForm::Hashed || g.two_pass.is_some() || g.set.is_some() || !crate::graph::grouped(g) {
        return None;
    }
    let gx = px.gx;
    let reads = gx.drainers(rec.g);
    let rerun = |q: usize| gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(q as u16)));
    let &[q] = &reads[..] else { return None };
    if rerun(q) || rerun(k) {
        return None;
    }
    let f = find(px, q, rec).map(|f| f.0).filter(|f| !f.num)?;
    Some(gravity_program::OwnerDesc::runs_floor(f.word, f.gt, f.int4, f.below, u8::try_from(f.slot).ok()?))
}

fn find<'p>(px: &mut Px<'_, 'p>, k: usize, rec: &GroupRec<'_, '_>) -> Option<(Floor, Node<'p>)> {
    if !CrossPass::Admitted.on() {
        return None;
    }
    // D453: PG takes every group's finals before its HAVING, and runs the HAVING's quals in order: a final that can
    // raise, or a qual before the count floor that can, keeps every group
    if !rec.aggs.iter().all(|a| crate::effects::visible::topn::fold_quiet(&a.d, false)) {
        return None;
    }
    let gx = px.gx;
    for op in gx.pipelines()[k].ops.iter() {
        let Op::Filter(n) = op else { break };
        let quals: PgVec<'_, Node<'p>> = match n.as_list() {
            Some(l) => l.iter().collect_in(px.mcx()),
            None => mcx::pg_vec![in px.mcx(); *n],
        };
        for q in quals {
            if let Some(f) = qual_floor(px, rec, q) {
                return Some(f);
            }
            if !crate::effects::raise_free(gx, q) {
                return None;
            }
        }
    }
    None
}

/// PSP2-43: a numeric K's int8 floor this execution (`count_floor` over the pointer in its bound word), read once before
/// the keep's walk. A NULL K's word is no pointer: the call reads 0 (no floor) and `passes` keeps no count.
fn numeric_bound(b: &mut FunctionBuilder<'_>, env: &Env, f: &Floor) -> Value {
    let nb = b.load_zext(Type::I8, env.params_null, f.word as i32);
    let set = b.icmp(IntCond::Eq, nb, env.zero);
    let p = b.load(Type::I64, env.params, 8 * f.word as i32);
    let p = b.select(set, p, env.zero);
    let gt = b.iconst(Type::I64, f.gt as i64);
    let flags = gravity_ir::HostFlags { may_error: true, ..gravity_ir::HostFlags::default() };
    let h = b.declare_host_once("count_floor", &[Type::I64, Type::I64], Some(Type::I64), crate::ops::sort::topn::raw::count_floor as *const () as usize, flags);
    b.call_host(h, &[p, gt], Some(Type::I64)).expect("returns the floor")
}

/// Whether the final `n` passes the bound: over / at least K, or under / at most K `below`, this execution (`params` its
/// bound words; `nk` a numeric K's floor, `numeric_bound`); a NULL K passes none.
fn passes(b: &mut FunctionBuilder<'_>, env: &Env, n: Value, f: &Floor, nk: Option<Value>) -> Value {
    let k = match (nk, f.int4) {
        (Some(k), _) => k,
        (None, true) => {
            let k = b.load(Type::I32, env.params, 8 * f.word as i32);
            b.conv(ConvOp::Sext, Type::I64, k)
        }
        (None, false) => b.load(Type::I64, env.params, 8 * f.word as i32),
    };
    let cond = match (f.below, f.gt) {
        (false, true) => IntCond::Sgt,
        (false, false) => IntCond::Sge,
        (true, true) => IntCond::Slt,
        (true, false) => IntCond::Sle,
    };
    let ok = b.icmp(cond, n, k);
    let nb = b.load_zext(Type::I8, env.params_null, f.word as i32);
    let set = b.icmp(IntCond::Eq, nb, env.zero);
    b.int_bin(IntBinOp::And, ok, set)
}

/// The records of `ptrs[0 .. len)` whose final passes the bound, compacted in order to the front: their number.
pub(crate) fn keep(b: &mut FunctionBuilder<'_>, env: &mut Env, ptrs: Value, len: Value, f: &Floor) -> Value {
    let nk = f.num.then(|| numeric_bound(b, env, f));
    let (h, body) = cancel::header(b, env.ctx, env.l_cancel, &[Type::I64, Type::I64], env.mcx);
    let l_done = b.create_block(&[Type::I64]);
    b.br(h, &[env.zero, env.zero]);
    b.switch_to(body);
    let (i, kept) = (b.block_params(body)[0], b.block_params(body)[1]);
    let more = b.icmp(IntCond::Slt, i, len);
    let l_step = b.create_block(&[]);
    b.cond_br(more, (l_step, &[]), (l_done, &[kept]));
    b.switch_to(l_step);
    let at = b.ptr_add(ptrs, i, 8, 0);
    let rec = b.load(Type::Ptr, at, 0);
    let n = b.load(Type::I64, rec, f.off);
    let ok = passes(b, env, n, f, nk);
    let to = b.ptr_add(ptrs, kept, 8, 0);
    b.store(to, 0, rec);
    let inc = b.conv(ConvOp::Zext, Type::I64, ok);
    let kept2 = b.int_bin(IntBinOp::Add, kept, inc);
    let one = b.iconst(Type::I64, 1);
    let i2 = b.int_bin(IntBinOp::Add, i, one);
    b.br(h, &[i2, kept2]);
    b.switch_to(l_done);
    b.block_params(l_done)[0]
}

#[cfg(test)]
mod tests {
    use datum::Datum;
    use gravity_decompose::{NoFacts, PipelineGraph, PipelineNode, Source};
    use gravity_lower::{Place, Sequential};
    use gravity_program::{Arena, End, Pipeline};
    use mcx::MemoryContext;
    use types_nodes::Node;

    use crate::frame::px::Px;
    use crate::graph::Gx;
    use crate::admit::refusal::Reason;

    /// A literal K (int8 100000): the split's floor and the emit's keep share one claimed word, which the HAVING
    /// filter's compile takes once (no second word); a claim no compile took refuses the program.
    #[test]
    fn a_literal_k_is_one_word_claimed_by_the_floor_and_taken_by_the_filter() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let cx = MemoryContext::new("twopass_having");
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let mut g = PipelineGraph::new(a);
        g.pipelines.push(PipelineNode::new(a, 7, Source::Const, Pipeline::serial(-1, End::Sink, gravity_program::Reason::NoDrivingSite)));
        let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
        let mut plan = Sequential::new(a);
        let mut px = Px::new(a, &gx, &mut plan);
        let k: Node<'_> = Node::mk_const(cx.mcx(), 20, -1, 0, 8, Datum::from_i64(100_000), false, true).expect("a const node");
        let w = px.claim_const(k);
        assert!(matches!(w, Some(Place::Word(_))), "{w:?}");
        assert_eq!(px.claim_const(k), w, "the emit's keep reads the split's claim");
        let n = px.plan.nwords();
        assert_eq!(px.take_claim(k), w, "the filter's compile takes the claimed word");
        assert_eq!((px.take_claim(k), px.plan.nwords()), (None, n), "taken once, no second word");
        let _ = px.claim_const(k);
        let r = crate::catch(0, || drop(px.take_exs()));
        assert_eq!(r.map_err(|r| r.reason), Err(Reason::Bug { payload: "an unconsumed literal claim" }));
    }
}
