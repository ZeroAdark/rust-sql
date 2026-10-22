//! Road `num.short`'s projection arithmetic (M2-INVENTORY I48): a projected numeric_mul / add / sub (1726 / 1724 / 1725) of
//! numeric(p, s) columns and a constant, open-coded (face/step.rs `ExprExtMech`; num.short's third row). Budget 190
//! (V4R-73). Owner M2-REP. The columns are read by num.short at their scales, the constant once per call at the column's
//! (rep/varlena/num_konst.rs: never baked, D407), met in one i64 word at the result's scale (`*`: s1 + s2, its dscale the
//! column's plus the constant's own; `+` / `-`: the larger) and written as make_result's image (num_image.rs) into a
//! per-site slot the out row copies. Claimed where the typmods and the constant's bound keep the word and its padded digits
//! in range; whatever the decode declines (NaN, TOAST, long form, a constant finer or past its bound) takes the shim, cold.
//! Only a projection step's root.
use std::rc::Rc;

use gravity_decompose::{catalog::typefacts, Op};
use gravity_decompose::catalog::numeric::{F_NUMERIC_ADD, F_NUMERIC_MUL, F_NUMERIC_SUB};
use gravity_expr::{Compiled, ExprNode, NodeId, Program, Source as VarSource};
use gravity_expr::nodes::var::Var;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, HostFlags, IntBinOp, IntCond, Type, Value};
use types_core::catalog::NUMERICOID;
use types_nodes::Node;

use crate::rep::varlena::num_image::{image, num_arith_cold};
use crate::rep::varlena::num_konst as kn;
use crate::rep::varlena::num_konst::{bounded, KDatum};
use crate::face::fold::NumShortImpl;
use crate::face::step::{Claim, ExprExtImpl, ExprExtMech, ExtPlan, SubEmit, EXT_AT};
use crate::face::strategy::NUM_SHORT;
use crate::frame::{env::Env, px::{ExId, Px}, row::RowVals};
use crate::admit::refusal::Reason;
use crate::ops::Table;

pub(crate) const ROWS: Table = Table { strategies: &[(NUM_SHORT, &ExprExtImpl(&ARITH))], ..crate::ops::NONE };
struct Arith; static ARITH: Arith = Arith;

/// An operand: a column read at its typmod scale, or the constant operand `sub` read at `scale`, below `10^digits`.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Arg { Col { attno: i16, scale: i16, digits: u32 }, Const { sub: u32, scale: i16, digits: u32 } }

/// A claimed `a op b` (op: 1726 `*`, 1724 `+`, 1725 `-`) at the result's `scale`; a constant's frame words `(its
/// mantissa, `*`'s dscale)`, written at each call's entry.
#[derive(Debug, PartialEq)]
struct Site { a: Arg, b: Arg, op: u32, scale: i16, words: (i32, i32) }

fn scale_of(a: Arg) -> i16 { match a { Arg::Col { scale, .. } | Arg::Const { scale, .. } => scale } }
fn digits_of(a: Arg) -> u32 { match a { Arg::Col { digits, .. } | Arg::Const { digits, .. } => digits } }

/// Program node `id` (plan node `n`) as an operand.
fn arg_of(p: &Program<'_>, id: NodeId, n: Node<'_>) -> Option<Arg> {
    crate::stack_check();
    let n = n.as_relabel_type().map_or(n, |r| r.arg);
    match &p.nodes[id.0 as usize] {
        ExprNode::Relabel(r) => arg_of(p, r.arg, n),
        ExprNode::Var(v) if v.src != VarSource::Inner && (1..EXT_AT).contains(&v.attno) && v.typ == NUMERICOID => {
            let t = u32::try_from(n.as_var()?.vartypmod.checked_sub(4)?).ok()?;
            Some(Arg::Col { attno: v.attno, scale: (t & 0xffff) as i16, digits: t >> 16 })
        }
        // the value is not read: the scale and bound are the site's (`site_of`)
        ExprNode::Const(k) if !k.is_null() && !k.byval && n.as_const()?.consttype == NUMERICOID => Some(Arg::Const { sub: 0, scale: 0, digits: 0 }),
        _ => None,
    }
}

/// The root `id` (plan node `n`) as a claimable operation (a constant's `sub` unset) and its constant's plan node: one
/// column and at most one constant, read at the column's scale with the widest bound that keeps the word and its padded
/// image digits in range.
fn site_of<'p>(p: &Program<'_>, id: NodeId, n: Node<'p>) -> Option<(Site, Option<Node<'p>>)> {
    let (ExprNode::Op(f) | ExprNode::Func(f)) = &p.nodes[id.0 as usize] else { return None };
    let call = &p.sites[f.site.0 as usize];
    let o = n.as_op_expr().filter(|o| o.opfuncid == call.funcid && matches!(call.funcid, F_NUMERIC_ADD | F_NUMERIC_SUB | F_NUMERIC_MUL) && o.args.len() == 2 && call.args.len == 2)?;
    let kid = |i: u32| p.kids[(call.args.start + i) as usize];
    let (na, nb) = (o.args.iter().next()?, o.args.iter().nth(1)?);
    let (a, b) = (arg_of(p, kid(0), na)?, arg_of(p, kid(1), nb)?);
    let k = match (a, b) {
        (Arg::Col { .. }, Arg::Col { .. }) => return fits(call.funcid, a, b).map(|s| (s, None)),
        (Arg::Const { .. }, Arg::Col { .. }) => na,
        (Arg::Col { .. }, Arg::Const { .. }) => nb,
        _ => return None,
    };
    bound(call.funcid, a, b).map(|s| (s, Some(k.as_relabel_type().map_or(k, |r| r.arg))))
}

/// `a op b`, one a constant: read at the column's scale under the widest bound that fits.
fn bound(op: u32, a: Arg, b: Arg) -> Option<Site> {
    let col = if matches!(a, Arg::Col { .. }) { a } else { b };
    (1..=18).rev().find_map(|digits| {
        let at = |x: Arg| if matches!(x, Arg::Const { .. }) { Arg::Const { sub: 0, scale: scale_of(col), digits } } else { x };
        fits(op, at(a), at(b))
    })
}

/// `a op b` when the result's word and padded digits fit.
fn fits(op: u32, a: Arg, b: Arg) -> Option<Site> {
    let (sa, sb) = (scale_of(a), scale_of(b));
    let (scale, digits) = match op {
        F_NUMERIC_MUL => (sa + sb, digits_of(a) + digits_of(b)),
        _ => { let s = sa.max(sb); (s, (digits_of(a) + (s - sa) as u32).max(digits_of(b) + (s - sb) as u32) + 1) }
    };
    let pad = ((4 - scale % 4) % 4) as u32;
    ((0..=18).contains(&scale) && digits <= 18 && digits + pad <= 19).then_some(Site { a, b, op, scale, words: (0, 0) })
}

impl ExprExtMech for Arith {
    fn ops(&self) -> &'static [u32] { &[F_NUMERIC_ADD, F_NUMERIC_SUB, F_NUMERIC_MUL] }
    fn attach<'p>(&self, px: &mut Px<'_, 'p>, (k, j): (usize, usize), node: Node<'p>, c: &mut Compiled<'p>, ex: ExId, sub: &mut dyn FnMut(Node<'p>) -> u32) -> Option<Claim> {
        let project = k != usize::MAX && matches!(px.gx.pipelines()[k].ops.get(j), Some(Op::Project(_)));
        let n = node.as_target_entry().map_or(node, |t| t.expr);
        let (mut s, kn) = site_of(&c.prog, c.root, n).filter(|_| project)?;
        if let Some(kn) = kn {
            let (w, konst_word) = (sub(kn), |px: &mut Px<'_, 'p>, i| kn::konst_word(px, (k, j), ex, i));
            s.words = (konst_word(px, 0), konst_word(px, 1));
            for a in [&mut s.a, &mut s.b] {
                if let Arg::Const { sub, .. } = a { *sub = w }
            }
        }
        c.prog.nodes[c.root.0 as usize] = ExprNode::Var(Var { src: VarSource::Scan, attno: EXT_AT, typ: NUMERICOID, desc: 0 });
        Some(Claim { plan: Rc::new(s), skip: vec![n.as_raw().as_ptr() as usize] }) // alloc: Program
    }

    fn emit(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, ex: ExId, _site: u32, sub: SubEmit<'_>) -> [Value; 3] {
        let node = env.node;
        let Some(s) = ExtPlan::of(env.placed(ex)).and_then(|x| x.plan.downcast_ref::<Site>()) else { crate::refuse!(node, Reason::Bug { payload: "a num.arith site without its plan" }) };
        let (tf, f) = (typefacts::row_of(NUMERICOID).copied().unwrap_or(typefacts::opaque(NUMERICOID, NUMERICOID, -1, false, false)), b.bconst(false));
        let mut datum = |b: &mut FunctionBuilder<'_>, env: &mut Env, a: Arg| match a {
            Arg::Col { attno, .. } => {
                let Some(cr) = row.col(attno) else { crate::refuse!(node, Reason::Bug { payload: "a num.arith column the row does not carry" }) };
                let mut cx = env.lx(b, node);
                let (p, null) = crate::rep::datum(&mut cx, cr, &tf);
                (env.memo = cx.memo, (p, null)).1
            }
            Arg::Const { .. } => (env.zero, f),
        };
        let ((pa, na), (pb, nb)) = (datum(b, env, s.a), datum(b, env, s.b));
        let kc = konsts(s).map(|_| (kn::load_konst(b, env, s.words.0), if s.op == F_NUMERIC_MUL { env.load_word(b, s.words.1) } else { env.zero }));
        let null = b.int_bin(IntBinOp::Or, na, nb);
        let (l_out, l_read) = (b.create_block(&[Type::I64, Type::Bool, Type::I64]), b.create_block(&[]));
        b.cond_br(null, (l_out, &[env.zero, f, env.zero]), (l_read, &[]));
        b.switch_to(l_read);
        let slot = { let sl = b.declare_stack_slot(64, 8); b.stack_addr(sl) };
        let w = konsts(s).map_or(0, |a| if let Arg::Const { sub, .. } = a { sub } else { 0 });
        let [v, ok, e] = arith(b, env, s, (pa, pb), kc, &mut |b, env| { let [v, ok, _] = sub(b, env, w); (kn::or_nan(b, v, ok), ok) }, slot);
        (b.br(l_out, &[v, ok, e]), b.switch_to(l_out));
        [0, 1, 2].map(|i| b.block_params(l_out)[i])
    }

    /// The constant decoded once per call at the column's scale, bounded by `digits` (else declined), and `*`'s dscale.
    fn entry(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, ex: ExId) {
        let Some(x) = ExtPlan::of(env.placed(ex)) else { return };
        let (Some(s), Some(&id)) = (x.plan.downcast_ref::<Site>(), x.subs.borrow().first()) else { return };
        let Some(Arg::Const { scale, digits, .. }) = konsts(s) else { return };
        let (k, ds) = kn::konst(b, env, id, scale);
        let k = bounded(b, k, digits);
        kn::store_konst(b, env, s.words.0, k);
        let ds = { let sc = b.iconst(Type::I64, scale as i64); b.int_bin(IntBinOp::Add, ds, sc) };
        env.store_word(b, s.words.1, ds);
    }
}

/// Site `s`'s constant operand.
fn konsts(s: &Site) -> Option<Arg> {
    [s.a, s.b].into_iter().find(|a| matches!(a, Arg::Const { .. }))
}

/// Site `s` over the column datums (`kc`: the constant's entry words, `(mantissa or KONST_NONE, *'s dscale)`; `kd`: its
/// datum, in the cold block): `[datum, valid, err]`, the image written at `slot` (64 bytes, 8-aligned).
fn arith(b: &mut FunctionBuilder<'_>, env: &mut Env, s: &Site, (pa, pb): (Value, Value), kc: Option<(Value, Value)>, kd: KDatum<'_>, slot: Value) -> [Value; 3] {
    let c = |b: &mut FunctionBuilder<'_>, v: i64| b.iconst(Type::I64, v);
    let short = crate::frame::iface::need_strategy::<NumShortImpl>(&env.node, NUM_SHORT).0;
    let mut word = |b: &mut FunctionBuilder<'_>, env: &mut Env, a: Arg, p: Value| match (a, kc) {
        (Arg::Col { scale, .. }, _) => { let (m, ok, _) = short.decode(b, env, p, scale, false); (m, ok) }
        (Arg::Const { .. }, Some((k, _))) => { let none = c(b, kn::KONST_NONE); (k, b.icmp(IntCond::Ne, k, none)) }
        (Arg::Const { .. }, None) => crate::refuse!(env.node, Reason::Bug { payload: "a num.arith constant without its entry word" }),
    };
    let ((ma, oka), (mb, okb)) = (word(b, env, s.a, pa), word(b, env, s.b, pb));
    let (ok, t) = (b.int_bin(IntBinOp::And, oka, okb), b.bconst(true));
    // `*` by a constant: dscale = the column's + the constant's own (`+` / `-`: the column's, the constant no finer)
    let ds = kc.filter(|_| s.op == F_NUMERIC_MUL).map(|x| x.1);
    let (l_hot, l_cold, l_out) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::I64, Type::Bool, Type::I64]));
    b.mark_cold(l_cold);
    b.cond_br(ok, (l_hot, &[]), (l_cold, &[]));
    b.switch_to(l_hot);
    let up = |b: &mut FunctionBuilder<'_>, m: Value, a: Arg| if scale_of(a) == s.scale { m } else { let k = c(b, 10i64.pow((s.scale - scale_of(a)) as u32)); b.int_bin(IntBinOp::Mul, m, k) };
    let m = match s.op {
        F_NUMERIC_MUL => b.int_bin(IntBinOp::Mul, ma, mb),
        op => { let (x, y) = (up(b, ma, s.a), up(b, mb, s.b)); b.int_bin(if op == F_NUMERIC_ADD { IntBinOp::Add } else { IntBinOp::Sub }, x, y) }
    };
    let img = image(b, env, m, s.scale, ds, slot);
    b.br(l_out, &[img, t, env.zero]);
    b.switch_to(l_cold);
    let (k, valid) = if konsts(s).is_some() { kd(b, env) } else { (pa, t) };
    let (pa, pb) = (if matches!(s.a, Arg::Const { .. }) { k } else { pa }, if matches!(s.b, Arg::Const { .. }) { k } else { pb });
    let h = b.declare_host_once("num_arith_cold", &[Type::I64; 4], Some(Type::I64), num_arith_cold as *const () as usize, HostFlags { touches_memory: true, may_error: true, ..HostFlags::default() });
    let r = { let (sl, op) = (b.conv(ConvOp::PtrToInt, Type::I64, slot), c(b, s.op as i64)); b.call_host(h, &[sl, pa, pb, op], Some(Type::I64)).expect("returns") };
    let (bad, raise) = (b.icmp(IntCond::Eq, r, env.zero), c(b, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64));
    let e = b.select(bad, raise, env.zero);
    (b.br(l_out, &[r, valid, e]), b.switch_to(l_out));
    [0, 1, 2].map(|i| b.block_params(l_out)[i])
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use super::*;
    use crate::frame::stop::tests::run1;

    /// PostgreSQL's canonical image of `v × 10^-scale` (fixed_scale_unpack through agg_final::num_sum), 8-aligned.
    fn img(v: i64, scale: i16) -> Vec<u64> {
        let mut buf = vec![0u64; 8]; // alloc: Program
        // SAFETY: `buf` is 64 bytes, live and unaliased while `r` lives.
        let mut r = gravity_fn::Region::over(unsafe { core::slice::from_raw_parts_mut(buf.as_mut_ptr().cast::<u8>(), 64) });
        let ret = gravity_fn::agg_final::num_sum(core::ptr::null_mut(), &mut r, v, v >> 63, scale as i64);
        assert_eq!(ret.status, gravity_fn::OK);
        assert_eq!(ret.val as usize, buf.as_ptr() as usize);
        buf
    }

    fn payload(p: i64) -> Vec<u8> {
        // SAFETY: `p` is a live numeric image the program returned.
        unsafe { gravity_fn::with_payload(p as usize as *const u8, |x| x.to_vec()) }.expect("payload") // alloc: Program
    }

    /// `*`, `+` and `-` of numeric(p, s) columns at mixed scales, signs, zero, results with leading and trailing zero
    /// groups: the image's payload is the shim's (numeric_mul / add / sub) byte for byte; a NaN operand takes the cold
    /// host and is NaN.
    #[test]
    fn images_are_the_shims() {
        let vals: &[(i64, i16)] = &[(0, 2), (1, 2), (-1, 2), (12_345, 2), (-99_999_999, 2), (100_000, 2), (123, 3), (-5, 3), (999_999, 3), (7, 0), (-10_000, 0), (1_000_000, 4)];
        let mut n = 0;
        for &(va, sa) in vals {
            for &(vb, sb) in vals {
                for op in [F_NUMERIC_MUL, F_NUMERIC_ADD, F_NUMERIC_SUB] {
                    let (a, b) = (Arg::Col { attno: 1, scale: sa, digits: 8 }, Arg::Col { attno: 2, scale: sb, digits: 8 });
                    let scale = if op == F_NUMERIC_MUL { sa + sb } else { sa.max(sb) };
                    let s = Site { a, b, op, scale, words: (0, 0) };
                    let (ia, ib) = (img(va, sa), img(vb, sb));
                    let mut want = vec![0u64; 8]; // alloc: Program
                    // SAFETY: `ia` / `ib` are live numeric images, `want` 64 writable bytes.
                    let w = unsafe { num_arith_cold(want.as_mut_ptr() as u64, ia.as_ptr() as u64, ib.as_ptr() as u64, op as u64) };
                    let (got, at) = eval_copy(&s, &ia, &ib);
                    assert_eq!((got, at), (payload(w), 8), "{va}e-{sa} op {op} {vb}e-{sb} (8: the open-coded image)");
                    n += 1;
                }
            }
        }
        assert!(n > 400);
        let nan = vec![(8u64 << 2) | (0xC000u64 << 32)]; // alloc: Program
        let s = Site { a: Arg::Col { attno: 1, scale: 2, digits: 8 }, b: Arg::Col { attno: 2, scale: 2, digits: 8 }, op: F_NUMERIC_MUL, scale: 4, words: (0, 0) };
        assert_eq!(eval_copy(&s, &nan, &img(5, 2)), (vec![0x00, 0xC0], 0), "NaN: the cold host, into the slot's start");
    }

    /// A constant operand read at run time at the column's scale (D407): either side, dscales up to the column's (hot;
    /// `*`'s dscale the column's plus the constant's) and finer (cold): the shim's payload byte for byte.
    #[test]
    fn constants_are_read_at_run_time() {
        let (cols, ks): (&[(i64, i16)], &[(i64, i16)]) = (&[(0, 2), (12_345, 2), (-99_999_999, 2), (7, 0), (123, 3)], &[(0, 0), (1, 0), (15, 1), (-25, 2), (1005, 3), (999, 0)]);
        let (mut hot, mut n) = (0, 0);
        for &(va, sa) in cols {
            for &(vk, sk) in ks {
                for op in [F_NUMERIC_MUL, F_NUMERIC_ADD, F_NUMERIC_SUB] {
                    for first in [false, true] {
                        let (c, k) = (Arg::Col { attno: 1, scale: sa, digits: 8 }, Arg::Const { sub: 0, scale: 0, digits: 0 });
                        let (a, b, ia, ib) = if first { (k, c, img(vk, sk), img(va, sa)) } else { (c, k, img(va, sa), img(vk, sk)) };
                        let s = bound(op, a, b).expect("a numeric(8, s) column and a constant fit");
                        let mut want = vec![0u64; 8]; // alloc: Program
                        // SAFETY: `ia` / `ib` are live numeric images, `want` 64 writable bytes.
                        let w = unsafe { num_arith_cold(want.as_mut_ptr() as u64, ia.as_ptr() as u64, ib.as_ptr() as u64, op as u64) };
                        let (got, at) = eval_copy(&s, &ia, &ib);
                        assert_eq!(got, payload(w), "{va}e-{sa} op {op} {vk}e-{sk} (constant first {first})");
                        assert!(at == 8 || sk > sa, "{vk}e-{sk} no finer than the column is open-coded");
                        (hot, n) = (hot + (at == 8) as usize, n + 1);
                    }
                }
            }
        }
        assert!(hot > n / 2, "{hot} of {n} open-coded");
    }

    /// The site over two images through the interpreter: the result's payload (copied out by a host inside the program,
    /// the slot dies with the frame) and its offset in the slot (8: the open-coded image, 0: the cold host's).
    fn eval_copy(s: &Site, a: &[u64], b: &[u64]) -> (Vec<u8>, i64) {
        unsafe extern "C" fn keep(p: u64, slot: u64, out: u64) -> i64 {
            // SAFETY: `out` is the caller's `&mut Vec<u8>`, unaliased for the call; `p` a live image.
            unsafe { *(out as *mut Vec<u8>) = payload(p as i64) };
            p as i64 - slot as i64
        }
        let mut out: Vec<u8> = Vec::new(); // alloc: Program
        let outp = &mut out as *mut Vec<u8> as i64;
        let at = run1(&[], &mut |bl, env| {
            let slot = { let sl = bl.declare_stack_slot(64, 8); bl.stack_addr(sl) };
            let (pa, pb, t) = (bl.iconst(Type::I64, a.as_ptr() as i64), bl.iconst(Type::I64, b.as_ptr() as i64), bl.bconst(true));
            // the constant as `entry` decodes it (without the frame word)
            let kc = konsts(s).map(|k| {
                let Arg::Const { scale, digits, .. } = k else { unreachable!() };
                let (kv, ds) = super::kn::konst_at(bl, env, if matches!(s.a, Arg::Const { .. }) { pa } else { pb }, t, scale);
                let sc = bl.iconst(Type::I64, scale as i64);
                (bounded(bl, kv, digits), bl.int_bin(IntBinOp::Add, ds, sc))
            });
            let kp = if matches!(s.a, Arg::Const { .. }) { pa } else { pb };
            let [v, _, _] = arith(bl, env, s, (pa, pb), kc, &mut |_, _| (kp, t), slot);
            let h = bl.declare_host_once("keep", &[Type::I64; 3], Some(Type::I64), keep as *const () as usize, HostFlags { touches_memory: true, ..HostFlags::default() });
            let (sl, o) = (bl.conv(ConvOp::PtrToInt, Type::I64, slot), bl.iconst(Type::I64, outp));
            bl.call_host(h, &[v, sl, o], Some(Type::I64)).expect("returns")
        });
        (out, at)
    }
}
