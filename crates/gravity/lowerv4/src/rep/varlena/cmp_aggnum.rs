//! Road `num.aggcmp` (D3024): numeric_eq / ne / gt / ge / lt / le (1718-1723) of a packed numeric aggregate's final and a
//! constant or an external parameter, in either operand order, wherever a group's row is evaluated (a HAVING filter, a
//! projection's CASE): the compare reads the aggregate's raw words, never its rendered numeric. A packed sum is an i128
//! at its typmod's scale `S`, a min / max a mantissa word at `S` (`FoldClass::packed_num`; the emit's cell
//! `CellForm::PackedNum`, ops/group/stream.rs `finals_of`). The operand is decoded once per call at its entry into a frame
//! word at `S` (num_konst.rs, as num.short's compare: literal-free, bound per execution); a row then costs one signed
//! compare. Cold, through the rendered final and `cmp_numerics`: a special or past-range sum (`hi == SENT_HI`), a
//! special mantissa, an operand finer than `S` or past a word, a NULL operand's NULL, and any aggregate whose cell is not
//! packed (the eager final's form). A NULL aggregate (no row, every input NULL) is NULL. Owner gravfix3. Budget 200 lines.
use std::rc::Rc;

use gravity_decompose::ops::shared::agg::{Register, NONE};
use gravity_expr::emit::{CellForm, CellValid};
use gravity_expr::nodes::var::Var;
use gravity_expr::{Compiled, ExprNode, NodeId, Program, Source as VarSource};
use gravity_fn::agg_final::{MANT_NINF, MANT_PINF, SENT_HI};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, HostFlags, IntBinOp, IntCond, Type, Value};
use mcx::{GrowIn, PgVec};
use types_core::catalog::NUMERICOID;
use types_nodes::primnodes::ParamKind;
use types_nodes::Node;

use crate::face::step::{Claim, ExprExtImpl, ExprExtMech, ExtPlan, SubEmit, EXT_AT};
use crate::frame::{env::Env, px::{ExId, Px}, row::RowVals};
use crate::admit::refusal::Reason;
use crate::rep::varlena::num_konst::{load_konst, store_konst, strip, CMP_OPS, KONST_NONE};

crate::strategies! {
    /// A packed numeric aggregate's final compared with a constant over its raw words.
    NUM_AGGCMP = 127, "num.aggcmp", NumHeap;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(NUM_AGGCMP, &ExprExtImpl(&CMP))], ..crate::ops::NONE };
struct Cmp;
static CMP: Cmp = Cmp;

/// A claimed compare: `agg(aggno) cond k`; `asub` / `ksub` the operands compiled on their own; `k` decoded at `scale`
/// into frame word `word`.
#[derive(Debug)]
struct Site { aggno: i32, scale: i16, cond: IntCond, asub: u32, ksub: u32, word: i32 }

/// A constant or an external parameter of type numeric (an operand bound per execution).
fn operand(n: Node<'_>) -> bool {
    match (n.as_const(), n.as_param()) {
        (Some(c), _) => c.consttype == NUMERICOID,
        (_, Some(p)) => p.paramkind == ParamKind::PARAM_EXTERN && p.paramtype == NUMERICOID,
        _ => false,
    }
}

/// Plan node `n` (program node `id`) as a claimable compare over pipeline `k`'s drained group: (the site with its subs
/// unset, the aggregate's plan node, the operand's plan node).
fn site_of<'p>(px: &Px<'_, 'p>, k: usize, p: &Program<'_>, id: NodeId, n: Node<'p>) -> Option<(Site, Node<'p>, Node<'p>)> {
    let (ExprNode::Op(f) | ExprNode::Func(f)) = &p.nodes[id.0 as usize] else { return None };
    let call = &p.sites[f.site.0 as usize];
    let o = n.as_op_expr().filter(|o| o.opfuncid == call.funcid && o.args.len() == 2 && call.args.len == 2)?;
    let &(_, cond, swapped) = CMP_OPS.iter().find(|x| x.0 == call.funcid)?;
    let (a, b) = (strip(o.args.iter().next()?), strip(o.args.iter().nth(1)?));
    let (g, kn, cond) = match (a.as_aggref(), operand(b)) {
        (Some(_), true) => (a, b, cond),
        _ if operand(a) && b.as_aggref().is_some() => (b, a, swapped),
        _ => return None,
    };
    let aggno = g.as_aggref()?.aggno;
    let d = px.gx.drained_group(k)?.aggs.iter().find(|d| i32::from(d.aggno) == aggno)?;
    let packed = matches!(d.reg, Register::SumNum | Register::MinNum | Register::MaxNum) && d.dscale_slot == NONE && d.scale >= 0;
    packed.then_some((Site { aggno, scale: d.scale, cond, asub: 0, ksub: 0, word: 0 }, g, kn))
}

/// The walk's way into a CASE (`CASE WHEN sum(amount) >= 1000 THEN 'gold' ...`, the projection's CASE this file's header
/// names): a searched CASE's conditions, and any CASE's results and default, each with its plan node, onto the walk's
/// `stack`. A simple CASE's conditions compare its `arg` (a `CaseTestExpr`), never an aggregate, so they stay. A claimed
/// site is emitted over the row before the expression (face/ext.rs), so a compare in a WHEN PostgreSQL does not reach
/// runs anyway: it neither raises nor acts (the hot path a signed compare, the cold one `cmp_numerics` over the rendered
/// final), so no row shows it.
fn case_kids<'p>(c: &Compiled<'p>, cs: gravity_expr::nodes::case::Case, n: Node<'p>, stack: &mut PgVec<'_, (NodeId, Node<'p>)>) {
    let Some(e) = n.as_case_expr().filter(|e| e.args.len() * 2 == cs.whens.len as usize) else { return };
    let kids = c.children(cs.whens);
    if !e.args.iter().all(|w| w.as_case_when().is_some()) {
        return;
    }
    for (i, w) in e.args.iter().filter_map(|w| w.as_case_when()).enumerate() {
        if let (None, Some(x)) = (cs.arg, w.expr) {
            stack.push_in((kids[2 * i], x));
        }
        if let Some(r) = w.result {
            stack.push_in((kids[2 * i + 1], r));
        }
    }
    if let (Some(d), Some(dn)) = (cs.default, e.defresult) {
        stack.push_in((d, dn));
    }
}

impl ExprExtMech for Cmp {
    fn ops(&self) -> &'static [u32] { &[1718, 1719, 1720, 1721, 1722, 1723] }

    fn attach<'p>(&self, px: &mut Px<'_, 'p>, at: (usize, usize), node: Node<'p>, c: &mut Compiled<'p>, ex: ExId, sub: &mut dyn FnMut(Node<'p>) -> u32) -> Option<Claim> {
        if at.0 == usize::MAX || px.gx.drained_group(at.0).is_none() {
            return None;
        }
        let (mut stack, mut sites, mut skip) = (mcx::pg_vec![in px.mcx(); (c.root, node)], Vec::new(), Vec::new()); // alloc: Program
        while let Some((id, n)) = stack.pop() {
            match (&c.prog.nodes[id.0 as usize], n.as_bool_expr()) {
                (ExprNode::Bool(b), Some(e)) if e.args.len() == b.args.len as usize => stack.extend_in(c.children(b.args).iter().copied().zip(e.args.iter())),
                (ExprNode::Bool(_), _) => {}
                (ExprNode::Case(cs), _) => case_kids(c, *cs, n, &mut stack),
                _ => if let Some((s, g, kn)) = site_of(px, at.0, &c.prog, id, n).filter(|_| sites.len() < 64) {
                    c.prog.nodes[id.0 as usize] = ExprNode::Var(Var { src: VarSource::Scan, attno: EXT_AT + sites.len() as i16, typ: 16, desc: 0 });
                    let word = crate::rep::varlena::num_konst::konst_word(px, at, ex, sites.len() as u8);
                    let (asub, ksub) = (sub(g), sub(kn));
                    (sites.push(Site { asub, ksub, word, ..s }), skip.push(n.as_raw().as_ptr() as usize)); // alloc: Program
                },
            }
        }
        if !sites.is_empty() {
            fires::fire("num.aggcmp"); // shapecheck fire signal (D3024)
        }
        (!sites.is_empty()).then(|| Claim { plan: Rc::new(sites), skip })
    }

    fn emit(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _row: &RowVals, ex: ExId, site: u32, sub: SubEmit<'_>) -> [Value; 3] {
        let node = env.node;
        let sites = ExtPlan::of(env.placed(ex)).and_then(|x| x.plan.downcast_ref::<Vec<Site>>());
        let Some(s) = sites.and_then(|v| v.get(site as usize)) else { crate::refuse!(node, Reason::Bug { payload: "a num.aggcmp site without its plan" }) };
        let cell = env.aggs.as_ref().and_then(|a| Some((a.rec, *a.cells.get(s.aggno as usize)?)));
        let k = load_konst(b, env, s.word);
        let (z, f, t) = (env.zero, b.bconst(false), b.bconst(true));
        let (l_out, l_cold) = (b.create_block(&[Type::I64, Type::Bool, Type::I64]), b.create_block(&[]));
        b.mark_cold(l_cold);
        match cell {
            Some((area, c @ gravity_expr::emit::AggCell { form: CellForm::PackedNum { sum, scale, .. }, valid: CellValid::NonZeroAt(v), .. })) if scale as i16 == s.scale => {
                let ok = b.load(Type::I64, area, v as i32);
                let ok = b.icmp(IntCond::Ne, ok, z);
                let l_read = b.create_block(&[]);
                b.cond_br(ok, (l_read, &[]), (l_out, &[z, f, z]));
                b.switch_to(l_read);
                let off = c.off as i32;
                let none = b.iconst(Type::I64, KONST_NONE);
                let kok = b.icmp(IntCond::Ne, k, none);
                let (hot, r) = if sum {
                    let (lo, hi) = (b.load(Type::I64, area, off), b.load(Type::I64, area, off + 8));
                    let sent = b.iconst(Type::I64, SENT_HI);
                    let fin = b.icmp(IntCond::Ne, hi, sent);
                    (b.int_bin(IntBinOp::And, fin, kok), i128_cmp(b, s.cond, (lo, hi), k))
                } else {
                    let m = b.load(Type::I64, area, off);
                    let (lo, hi) = (b.iconst(Type::I64, MANT_NINF), b.iconst(Type::I64, MANT_PINF));
                    let (above, below) = (b.icmp(IntCond::Sgt, m, lo), b.icmp(IntCond::Slt, m, hi));
                    let fin = b.int_bin(IntBinOp::And, above, below);
                    (b.int_bin(IntBinOp::And, fin, kok), b.icmp(s.cond, m, k))
                };
                let r = b.conv(ConvOp::Zext, Type::I64, r);
                let l_hot = b.create_block(&[]);
                b.cond_br(hot, (l_hot, &[]), (l_cold, &[]));
                b.switch_to(l_hot);
                b.br(l_out, &[r, t, z]);
            }
            _ => b.br(l_cold, &[]),
        }
        // cold: `cmp_numerics(agg, k)` over the rendered final and the operand's datum (-1 / 0 / 1), so `agg cond k` is
        // `r cond 0`; a NULL either side is NULL
        b.switch_to(l_cold);
        let [av, aok, ae] = sub(b, env, s.asub);
        let [kv, kok, ke] = sub(b, env, s.ksub);
        let both = b.int_bin(IntBinOp::And, aok, kok);
        let (l_call, l_null) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(both, (l_call, &[]), (l_null, &[]));
        b.switch_to(l_null);
        let e = b.int_bin(IntBinOp::Or, ae, ke);
        b.br(l_out, &[z, f, e]);
        b.switch_to(l_call);
        let h = b.declare_host_once("num_cmp_cold", &[Type::I64; 2], Some(Type::I64), num_cmp_cold as *const () as usize, HostFlags { touches_memory: true, may_error: true, ..HostFlags::default() });
        let r = b.call_host(h, &[av, kv], Some(Type::I64)).expect("returns");
        let (min, raise) = (b.iconst(Type::I64, i64::MIN), b.iconst(Type::I64, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64));
        let bad = b.icmp(IntCond::Eq, r, min);
        let e = b.select(bad, raise, z);
        let e = b.int_bin(IntBinOp::Or, e, ae);
        let e = b.int_bin(IntBinOp::Or, e, ke);
        let v = b.icmp(s.cond, r, z);
        let v = b.conv(ConvOp::Zext, Type::I64, v);
        b.br(l_out, &[v, t, e]);
        b.switch_to(l_out);
        [0, 1, 2].map(|i| b.block_params(l_out)[i])
    }

    /// Each site's operand decoded once per call at its aggregate's scale into its frame word (`KONST_NONE` when declined:
    /// finer than the scale, past a word, special, NULL, or an operand with no bind word).
    fn entry(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, ex: ExId) {
        let Some(x) = ExtPlan::of(env.placed(ex)) else { return };
        let subs = mcx::to_vec_in(&x.subs.borrow()[..], env.mcx);
        for s in x.plan.downcast_ref::<Vec<Site>>().into_iter().flatten() {
            let id = subs[s.ksub as usize];
            let k = match env.placed(id).places.first() {
                Some(&gravity_lower::Place::Word(_)) => crate::rep::varlena::num_konst::konst(b, env, id, s.scale).0,
                _ => b.iconst(Type::I64, KONST_NONE),
            };
            store_konst(b, env, s.word, k);
        }
    }
}

/// The i128 `(lo, hi)` against the sign-extended word `k` under `cond`.
fn i128_cmp(b: &mut FunctionBuilder<'_>, cond: IntCond, (lo, hi): (Value, Value), k: Value) -> Value {
    let s63 = b.iconst(Type::I64, 63);
    let kh = b.int_bin(IntBinOp::Sshr, k, s63);
    let (hi_eq, lo_eq) = (b.icmp(IntCond::Eq, hi, kh), b.icmp(IntCond::Eq, lo, k));
    let eq = b.int_bin(IntBinOp::And, hi_eq, lo_eq);
    let (hc, lc) = match cond {
        IntCond::Eq => return eq,
        IntCond::Ne => {
            let t = b.bconst(true);
            return b.int_bin(IntBinOp::Xor, eq, t);
        }
        IntCond::Sgt => (IntCond::Sgt, IntCond::Ugt),
        IntCond::Sge => (IntCond::Sgt, IntCond::Uge),
        IntCond::Slt => (IntCond::Slt, IntCond::Ult),
        _ => (IntCond::Slt, IntCond::Ule),
    };
    let (h, l) = (b.icmp(hc, hi, kh), b.icmp(lc, lo, k));
    let l = b.int_bin(IntBinOp::And, hi_eq, l);
    b.int_bin(IntBinOp::Or, h, l)
}

use super::raw::cmp_num::num_cmp_cold;

