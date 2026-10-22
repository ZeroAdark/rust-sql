//! Road `num.short`'s compare extension (M2-INVENTORY I47): numeric_eq / ne / gt / ge / lt / le (1718-1723) of a numeric
//! column and a constant, open-coded (face/step.rs `ExprExtMech`; the road's second row, face type `ExprExtImpl`). Budget
//! 160 (+25 %). Owner M2-REP. The constant is an operand (`sub`): its word is bound per execution, never baked (V4R-18:
//! the front key is literal-free; D407, V4R-73), and decoded once per call at its entry (`entry`) into a frame word at
//! `S`, the column's typmod scale (`read_scale`: any typmod, `NO_TYPMOD` without one). Each row's image is read by num.short at `S` (a dscale
//! `<= S` widened up) and met by one signed compare. A claimed compare is text.like's claim form (step/text_like.rs
//! `claim_calls`): `CASE WHEN Var(hot) THEN Var(value) ELSE <the compare> END`, so the compare and its literal stay in the
//! qual (the condition cache's key reads them: verdict_cond_key.rs) and whatever the decode declines on either side (a
//! constant finer than `S`: `col = 1.005` over numeric(10, 2); a special; TOAST, compressed, long form, past a word)
//! runs the compare itself, cold. A varlena column lane under the root's AND / OR / NOT, as text.eq (cmp_text.rs), and (D2996 /
//! D2997) a pgrc packed lane read at its own scale (`Rep::PackedNum`): a packed window's mantissa as is, a verbatim
//! window's image through the same decode, a kept word's order word, met by the constant's word, where the row path
//! rendered every row's image for the host's `numeric_le` (60 M rows of `l_quantity BETWEEN 1 AND 11` at W8: 210 ms,
//! 13 % in the shims and 30 % building images).

use gravity_abi::ctx::lane::KIND_BYREF;
use gravity_decompose::{catalog::typefacts, Op};
use gravity_expr::nodes::var::Var;
use gravity_expr::{Compiled, ExprNode, NodeId, Program, Source as VarSource};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value};
use gravity_program::{LaneId, NumTypmod, Rep, NUM_WORD_DIGITS};
use mcx::GrowIn;
use types_core::catalog::NUMERICOID;
use types_nodes::Node;

use crate::rep::varlena::num_konst::{strip, CMP_OPS, KONST_NONE};
use crate::face::fold::NumShortImpl;
use crate::face::step::{Claim, ExprExtImpl, ExprExtMech, ExtPlan, SubEmit, EXT_AT};
use crate::face::strategy::NUM_SHORT;
use crate::frame::{env::Env, px::{ExId, Px}, row::RowVals};
use crate::admit::refusal::Reason;
use crate::ops::Table;

pub(crate) const ROWS: Table = Table { strategies: &[(NUM_SHORT, &ExprExtImpl(&CMP))], ..crate::ops::NONE };
struct Cmp;
static CMP: Cmp = Cmp;

/// The scale a column without a typmod is read at (a finer image is cold).
const NO_TYPMOD: i16 = 6;

/// REP-03: the scale a numeric column of typmod `typmod` is read at, `None` where no word holds its values: the typmod's
/// scale by PostgreSQL's layout (gravity_program `NumTypmod::of_typmod`: an 11-bit signed scale, so `numeric(5, -2)`, whose
/// every value has dscale 0, is read at 0, not at the raw field), `NO_TYPMOD` without one; a scale past the word's digits
/// (`NUM_WORD_DIGITS`) is not claimed. num.short's decode serves any scale from 0 up (its weights and constants are the
/// scale's); whatever a row or the constant holds past the word is cold either way.
fn read_scale(typmod: i32) -> Option<i16> {
    let s = NumTypmod::of_typmod(typmod).scale().map_or(NO_TYPMOD, |s| s.max(0));
    (s as u16 <= NUM_WORD_DIGITS).then_some(s)
}

/// A claimed comparison: `m(col) cond k`, both at `scale`, `k` the constant operand `sub` decoded into frame word `word`.
#[derive(Debug, PartialEq)]
struct Site { attno: i16, scale: i16, sub: u32, cond: IntCond, word: i32 }

/// Plan node `n` (program node `id`) as a claimable comparison (`sub` unset) and its constant's plan node; the
/// constant's value is not read.
fn site_of<'p>(p: &Program<'_>, id: NodeId, n: Node<'p>) -> Option<(Site, Node<'p>)> {
    let (ExprNode::Op(f) | ExprNode::Func(f)) = &p.nodes[id.0 as usize] else { return None };
    let call = &p.sites[f.site.0 as usize];
    let o = n.as_op_expr().filter(|o| o.opfuncid == call.funcid && o.args.len() == 2 && call.args.len == 2)?;
    let &(_, cond, swapped) = CMP_OPS.iter().find(|x| x.0 == call.funcid)?;
    let (a, b) = (strip(o.args.iter().next()?), strip(o.args.iter().nth(1)?));
    let (ci, v, c, cond) = match (a.as_var(), b.as_const()) {
        (Some(v), Some(_)) => (0, v, b, cond),
        _ => (1, b.as_var()?, a.as_const().map(|_| a)?, swapped),
    };
    let k = c.as_const()?;
    let mut col = p.kids[(call.args.start + ci) as usize];
    while let ExprNode::Relabel(r) = &p.nodes[col.0 as usize] {
        col = r.arg;
    }
    let ExprNode::Var(pv) = &p.nodes[col.0 as usize] else { return None };
    if pv.src == VarSource::Inner || !(1..EXT_AT).contains(&pv.attno) || pv.typ != NUMERICOID || v.vartype != NUMERICOID || k.consttype != NUMERICOID || k.constisnull {
        return None;
    }
    let scale = read_scale(v.vartypmod)?;
    Some((Site { attno: pv.attno, scale, sub: 0, cond, word: 0 }, c))
}

/// Column `attno` of row step `at` is a lane this road does not read: the site's lane, when the steps before `at` only
/// filter, unless it is a varlena lane or a packed one at `scale` (D2996: its order word is read); unknown after a
/// projection (declined), a varlena lane when the step has no site.
fn packed(px: &mut Px<'_, '_>, (k, j): (usize, usize), attno: i16, scale: i16) -> bool {
    if k == usize::MAX {
        return false;
    }
    if !px.gx.pipelines()[k].ops[..j].iter().all(|o| matches!(o, Op::Filter(_) | Op::Gate(_))) {
        return true;
    }
    let own = |r: &Rep| matches!(r, Rep::PackedNum { facts, .. } if facts.scale().map(|x| x.max(0)) == Some(scale));
    px.site_mut(k).is_some_and(|s| s.lanes().iter().any(|l| l.id == LaneId::Attr(attno) && l.kind != KIND_BYREF && !own(&l.rep)))
}

impl ExprExtMech for Cmp {
    fn ops(&self) -> &'static [u32] { &[1718, 1719, 1720, 1721, 1722, 1723] }
    fn attach<'p>(&self, px: &mut Px<'_, 'p>, at: (usize, usize), node: Node<'p>, c: &mut Compiled<'p>, ex: ExId, sub: &mut dyn FnMut(Node<'p>) -> u32) -> Option<Claim> {
        let (mut stack, mut found) = (mcx::pg_vec![in px.mcx(); (c.root, node)], mcx::PgVec::new_in(px.mcx()));
        while let Some((id, n)) = stack.pop() {
            match (&c.prog.nodes[id.0 as usize], n.as_bool_expr()) {
                (ExprNode::Bool(b), Some(e)) if e.args.len() == b.args.len as usize => stack.extend_in(c.children(b.args).iter().copied().zip(e.args.iter())),
                (ExprNode::Bool(_), _) => {}
                _ => if let Some((s, k)) = site_of(&c.prog, id, n).filter(|s| found.len() < 64 && !packed(px, at, s.0.attno, s.0.scale)) {
                    let word = crate::rep::varlena::num_konst::konst_word(px, at, ex, found.len() as u8);
                    found.push_in((id.0 as usize, Site { sub: sub(k), word, ..s }));
                },
            }
        }
        // the compare keeps its literal (the CASE's cold arm): the claim skips no plan node
        crate::step::text_like::claim_calls(c, |_, i| found.iter().position(|f| f.0 == i).map(|j| (found.swap_remove(j).1, 16)))
    }

    fn emit(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, ex: ExId, site: u32, _sub: SubEmit<'_>) -> [Value; 3] {
        crate::step::text_like::site(b, env, ex, site, |b, env, s: &Site| {
            let node = env.node;
            let Some(cr) = row.col(s.attno) else { crate::refuse!(node, Reason::Bug { payload: "a num.cmp column the row does not carry" }) };
            if matches!(cr.cell.rep, Rep::PackedNum { facts, .. } if facts.scale().map(|x| x.max(0)) == Some(s.scale)) {
                return packed_compare(b, env, cr, s);
            }
            let (tf, mut cx) = (typefacts::row_of(NUMERICOID).copied().unwrap_or(typefacts::opaque(NUMERICOID, NUMERICOID, -1, false, false)), env.lx(b, node));
            let (p, null) = crate::rep::datum(&mut cx, cr, &tf);
            env.memo = cx.memo;
            let k = crate::rep::varlena::num_konst::load_konst(b, env, s.word);
            compare(b, env, (p, null), k, s)
        })
    }

    /// Each site's constant decoded once per call into its frame word (`KONST_NONE` when declined).
    fn entry(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, ex: ExId) {
        let Some(x) = ExtPlan::of(env.placed(ex)) else { return };
        let subs = x.subs.borrow().clone();
        for s in x.plan.downcast_ref::<crate::step::text_like::Plan<Site>>().into_iter().flat_map(|p| p.claims.iter()) {
            let (k, _) = crate::rep::varlena::num_konst::konst(b, env, subs[s.sub as usize], s.scale);
            crate::rep::varlena::num_konst::store_konst(b, env, s.word, k);
        }
    }
}

/// Site `s` over the column's datum `p` (`null`: its NULL flag) and the constant decoded at `s.scale` (`k`:
/// `KONST_NONE` when declined): `[hot, value, valid]`, `hot` false where the decode declined either side (the CASE's cold
/// arm, the compare itself, decides that row).
fn compare(b: &mut FunctionBuilder<'_>, env: &mut Env, (p, null): (Value, Value), k: Value, s: &Site) -> [Value; 3] {
    let (z, f, t) = (env.zero, b.bconst(false), b.bconst(true));
    let (l_out, l_read) = (b.create_block(&[Type::Bool, Type::I64, Type::Bool]), b.create_block(&[]));
    b.cond_br(null, (l_out, &[t, z, f]), (l_read, &[]));
    b.switch_to(l_read);
    let short = crate::frame::iface::need_strategy::<NumShortImpl>(&env.node, NUM_SHORT).0;
    let (m, okm, _) = short.decode(b, env, p, s.scale, true);
    let okk = { let none = b.iconst(Type::I64, KONST_NONE); b.icmp(IntCond::Ne, k, none) };
    let ok = b.int_bin(IntBinOp::And, okm, okk);
    let r = b.icmp(s.cond, m, k);
    let r = b.conv(ConvOp::Zext, Type::I64, r);
    b.br(l_out, &[ok, r, t]);
    b.switch_to(l_out);
    [0, 1, 2].map(|i| b.block_params(l_out)[i])
}

/// D2996: site `s` over a packed cell `cr` read at its own scale, one signed compare against the constant's word (`k`): a
/// packed window's mantissa, a verbatim window's image through num.short's decode, a kept word's order word (the specials'
/// sentinels order as PostgreSQL's `cmp_numerics` against any finite constant). `[hot, value, valid]`: a constant or an
/// image the decode declined is not hot (the compare itself decides it, cold). A NULL cell reads nothing.
fn packed_compare(b: &mut FunctionBuilder<'_>, env: &mut Env, cr: crate::rep::CellRef<'_>, s: &Site) -> [Value; 3] {
    let (z, f, t) = (env.zero, b.bconst(false), b.bconst(true));
    let node = env.node;
    let null = cr.null.unwrap_or(f);
    let k = crate::rep::varlena::num_konst::load_konst(b, env, s.word);
    let (l_out, l_read) = (b.create_block(&[Type::Bool, Type::I64, Type::Bool]), b.create_block(&[]));
    b.cond_br(null, (l_out, &[t, z, f]), (l_read, &[]));
    b.switch_to(l_read);
    let okk = { let none = b.iconst(Type::I64, KONST_NONE); b.icmp(IntCond::Ne, k, none) };
    let l_hot = b.create_block(&[]);
    b.cond_br(okk, (l_hot, &[]), (l_out, &[f, z, t]));
    b.switch_to(l_hot);
    let tf = typefacts::row_of(NUMERICOID).copied().unwrap_or(typefacts::opaque(NUMERICOID, NUMERICOID, -1, false, false));
    // each branch's rows memo is its own (set aside): a word or image read under the NULL / constant tests does not
    // dominate the next site's reads
    let aside = env.memo.set_aside();
    let mut cx = env.lx(b, node);
    let window = crate::rep::packednum::datum::window_word(&mut cx, cr);
    let kept = match window {
        Some(_) => None,
        None => Some((crate::rep::packednum::order::ARM.image)(&mut cx, cr, &tf, typefacts::KeyColl::NONE)),
    };
    env.memo = cx.memo;
    env.memo.restore(aside);
    let l_cmp = b.create_block(&[Type::I64, Type::Bool]);
    match (window, kept) {
        // a window lane: a packed window's mantissa as is; a verbatim part's stored image decoded open-coded by num.short
        // at the column's scale (its declines are not hot), where rep.packed.order called the host per row
        (Some((verbatim, w)), _) => {
            let (l_v, l_p) = (b.create_block(&[]), b.create_block(&[]));
            b.cond_br(verbatim, (l_v, &[]), (l_p, &[]));
            b.switch_to(l_v);
            let short = crate::frame::iface::need_strategy::<NumShortImpl>(&env.node, NUM_SHORT).0;
            let (m, okm, _) = short.decode(b, env, w, s.scale, true);
            b.br(l_cmp, &[m, okm]);
            b.switch_to(l_p);
            b.br(l_cmp, &[w, t]);
        }
        (None, Some(img)) => {
            let m = img.w[0].unwrap_or_else(|| crate::refuse!(node, Reason::Bug { payload: "a packed order image without its word" }));
            b.br(l_cmp, &[m, t]);
        }
        (None, None) => crate::refuse!(node, Reason::Bug { payload: "a packed compare without its word" }),
    }
    b.switch_to(l_cmp);
    let (m, okm) = (b.block_params(l_cmp)[0], b.block_params(l_cmp)[1]);
    let r = b.icmp(s.cond, m, k);
    let r = b.conv(ConvOp::Zext, Type::I64, r);
    b.br(l_out, &[okm, r, t]);
    b.switch_to(l_out);
    [0, 1, 2].map(|i| b.block_params(l_out)[i])
}


#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use super::*;
    use crate::frame::stop::tests::run1;
    use super::super::raw::cmp_num::num_cmp_cold;

    /// The 4-byte-header varlena of `v / 10^d` (`long`: the long form, which num.short declines): base-10000 groups
    /// aligned to the point, zero groups stripped (numeric.c `make_result`).
    fn img(v: i128, d: u32, long: bool) -> Vec<u8> {
        let (neg, d4) = (v < 0, d.div_ceil(4) * 4);
        let (mut x, mut g, mut low) = (v.unsigned_abs() * 10u128.pow(d4 - d), Vec::new(), -(d4 as i32 / 4)); // alloc: Program
        while x > 0 {
            (g.push((x % 10_000) as u16), x /= 10_000); // alloc: Program
        }
        while g.first() == Some(&0) {
            (g.remove(0), low += 1);
        }
        g.reverse();
        let (neg, w) = if g.is_empty() { (false, 0) } else { (neg, low + g.len() as i32 - 1) };
        let mut p = match long {
            true => [((if neg { 0x4000 } else { 0 }) | d as u16).to_le_bytes(), (w as i16).to_le_bytes()].concat(),
            false => (0x8000 | if neg { 0x2000u16 } else { 0 } | ((d as u16) << 7) | (w as u16 & 0x7F)).to_le_bytes().to_vec(), // alloc: Program
        };
        g.iter().for_each(|x| p.extend_from_slice(&x.to_le_bytes()));
        special(&p)
    }

    fn special(p: &[u8]) -> Vec<u8> {
        [(((p.len() + 4) as u32) << 2).to_le_bytes().as_slice(), p].concat()
    }

    /// `(value, valid)` of `s` over the datum `d` (`None`: NULL) and the constant `k`, through the interpreter: the hot
    /// site's answer, else (not hot) the CASE's cold arm, `cmp_numerics(k, col)` as the compare itself decides it.
    fn eval(s: &Site, d: Option<&[u8]>, k: &[u8]) -> (i64, bool) {
        let p = d.map_or(0, |d| d.as_ptr() as i64);
        let one = |i: usize| {
            run1(&[], &mut |b, env| {
                let (pv, null, kp, t) = (b.iconst(Type::I64, p), b.bconst(d.is_none()), b.iconst(Type::I64, k.as_ptr() as i64), b.bconst(true));
                let (kv, _) = crate::rep::varlena::num_konst::konst_at(b, env, kp, t, s.scale);
                let out = compare(b, env, (pv, null), kv, s);
                if i == 1 { out[1] } else { b.conv(ConvOp::Zext, Type::I64, out[i]) }
            })
        };
        if one(0) != 0 {
            return (one(1), one(2) != 0);
        }
        // SAFETY: `k` and `d` are live numeric varlenas the caller owns (a NULL column is always hot).
        let r = unsafe { num_cmp_cold(k.as_ptr() as u64, p as u64) };
        let v = match s.cond { IntCond::Eq => 0 == r, IntCond::Ne => 0 != r, IntCond::Sgt => 0 > r, IntCond::Sge => 0 >= r, IntCond::Slt => 0 < r, _ => 0 <= r };
        (v as i64, true)
    }

    /// Every operator over column values at dscale 2 (short and long forms: the decode and the cold host) and a
    /// finer column value (dscale 4, cold), against constants at dscales 0, 2 (short and long) and 3 (cold: `col =
    /// 1.005` over numeric(10, 2) is never true); the exact order of the rationals decides.
    #[test]
    fn exact_at_the_wider_scale() {
        let cols: Vec<(i128, u32)> = [-12_345, -100, -1, 0, 1, 100, 101, 12_345, 99_999_999].iter().map(|&v| (v, 2)).chain([(10_050, 4), (10_051, 4)]).collect(); // alloc: Program
        for (kv, kd, klong) in [(1, 0, false), (100, 2, false), (100, 2, true), (1005, 3, false), (-1, 0, false), (0, 0, false), (123_450, 3, false)] {
            let k = img(kv, kd, klong);
            for &(f, cond, _) in CMP_OPS.iter() {
                let s = Site { attno: 1, scale: 2, sub: 0, cond, word: 0 };
                for &(v, d) in &cols {
                    let (x, y) = (v * 10i128.pow(6 - d), kv * 10i128.pow(6 - kd));
                    let want = match f { 1718 => x == y, 1719 => x != y, 1720 => x > y, 1721 => x >= y, 1722 => x < y, _ => x <= y };
                    for long in [false, true] {
                        assert_eq!(eval(&s, Some(&img(v, d, long)), &k), (want as i64, true), "{v}e-{d} op {f} {kv}e-{kd} long {long}");
                    }
                }
                assert!(!eval(&s, None, &k).1, "NULL is NULL");
            }
        }
    }

    /// REP-03: the read scale is the typmod's by PostgreSQL's layout, a negative scale read at 0 (its values' dscale).
    #[test]
    fn read_scale_of_any_typmod() {
        let tm = |p: i32, s: i32| ((p << 16) | (s & 0x7ff)) + 4;
        assert_eq!(read_scale(-1), Some(NO_TYPMOD), "no typmod");
        assert_eq!(read_scale(tm(15, 2)), Some(2));
        assert_eq!(read_scale(tm(10, 0)), Some(0));
        assert_eq!(read_scale(tm(5, -2)), Some(0), "numeric(5, -2): every value has dscale 0");
        assert_eq!(read_scale(tm(1000, -1000)), Some(0));
        assert_eq!(read_scale(tm(20, 18)), Some(18));
        assert_eq!(read_scale(tm(30, 19)), None, "past the word's digits");
        assert_eq!(read_scale(tm(1000, 1000)), None);
    }

    /// A negative-scale column (`numeric(7, -2)`: values rounded to hundreds, dscale 0) read at 0 against constants at
    /// dscales 0 and 1 (cold), short and long forms: the exact order decides.
    #[test]
    fn negative_typmod_scale_reads_at_zero() {
        let cols: [i128; 6] = [-1_234_500, -100, 0, 100, 12_300, 99_999_00];
        for (kv, kd) in [(12_300, 0), (12_345, 0), (123_005, 1), (-100, 0), (0, 0), (5, 1)] {
            let k = img(kv, kd, false);
            for &(f, cond, _) in CMP_OPS.iter() {
                let s = Site { attno: 1, scale: read_scale(((7 << 16) | (-2i32 & 0x7ff)) + 4).unwrap(), sub: 0, cond, word: 0 };
                for &v in &cols {
                    let (x, y) = (v * 10, kv * 10i128.pow(1 - kd));
                    let want = match f { 1718 => x == y, 1719 => x != y, 1720 => x > y, 1721 => x >= y, 1722 => x < y, _ => x <= y };
                    for long in [false, true] {
                        assert_eq!(eval(&s, Some(&img(v, 0, long)), &k), (want as i64, true), "{v} op {f} {kv}e-{kd} long {long}");
                    }
                }
            }
        }
    }

    /// NaN, Infinity and -Infinity on either side order as cmp_numerics: -Infinity < finite < Infinity < NaN, NaN = NaN.
    #[test]
    fn specials_order() {
        let (nan, pinf, ninf) = (special(&0xC000u16.to_le_bytes()), special(&0xD000u16.to_le_bytes()), special(&0xF000u16.to_le_bytes()));
        let vals: [(&[u8], i64); 4] = [(&ninf, 0), (&img(5, 2, false), 1), (&pinf, 2), (&nan, 3)];
        for &(k, ki) in &vals {
            for &(f, cond, _) in CMP_OPS.iter() {
                let s = Site { attno: 1, scale: 2, sub: 0, cond, word: 0 };
                for &(d, rank) in &vals {
                    let (x, y) = (rank, ki);
                    let want = match f { 1718 => x == y, 1719 => x != y, 1720 => x > y, 1721 => x >= y, 1722 => x < y, _ => x <= y };
                    assert_eq!(eval(&s, Some(d), k).0, want as i64, "rank {x} op {f} rank {y}");
                }
            }
        }
    }

    /// The cold host over a 1-byte header and the long form.
    #[test]
    fn cold_host() {
        let p = img(150, 2, false)[4..].to_vec(); // alloc: Program
        let short = [&[((p.len() as u8 + 1) << 1) | 1][..], &p].concat();
        let at = |v: &[u8]| v.as_ptr() as u64;
        // SAFETY: every argument is a live numeric varlena the test owns for the block.
        unsafe {
            assert_eq!(num_cmp_cold(at(&short), at(&img(150, 2, false))), 0);
            assert_eq!(num_cmp_cold(at(&img(151, 2, false)), at(&short)), 1);
            assert_eq!(num_cmp_cold(at(&img(-2999, 3, false)), at(&img(-3, 0, true))), 1);
        }
    }
}
