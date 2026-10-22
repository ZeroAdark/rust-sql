//! Wide fixed-scale numeric sum / avg (V4R-38, B-lite: the user 09-24). Hooks / levers: -. Budget 220 lines (files.tsv,
//! +25 % gate). Owner M3-FOLD (built by V4R38-WIDE).
//!
//! Registers: kind `class.num_wide` (`KindId::ClassNumWide`, M3): `sum` / `avg(numeric)` over a `+` / `-` / `*` tree past
//! the packed register's 37 digits whose products each take a packed tree times a one-word one (decompose's `SumNumWide` /
//! `AvgNumWide`, `wide_tree`: e.g. `p * (1 - d) * (1 + t)` over numeric(15, 2) columns, 47 digits by typmod). Its
//! terms can pass i128, so the state is the i256 slot (`SlotKind::SumNumWide` / `AvgNumWide`, abi's `NumWide`): four
//! words of two's complement at the product's fixed scale S (PG's `mul_var` rscale: the factors' dscales added), `n` at
//! +32 for avg; merged by abi's `num_add256` across workers. A term is under 10^55 < 2^183 and 2^63 of them under 2^246,
//! so a finite sum is exact for any row count (a sum past |2^254| would be D299's sticky OVERFLOW code, raised 22003 by
//! the final). NaN / ±Infinity are the specials codes on the sentinel top word, added as PG's `numeric_add` adds them.
//! A row's term is its argument's datum read as an i256 at S (`term`: exact, never rounded; a digit below S refuses by
//! name). Finals: gravity_fn's `num_sum256` / `num_avg256` (PG's numeric sum at dscale S, avg as `numeric_avg` divides).
use gravity_decompose::ops::shared::agg::{AggDecl, Register as R, NONE};
use gravity_expr::emit::CellValid;
use gravity_fn::agg_final::{SENT_HI, SP_OVERFLOW};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, HostFlags, IntBinOp, IntCond, Type, Value};
use mcx::{CollectIn, PgVec};

use crate::face::fold::{Acc, AccShape, ClassImpl, FoldClass, WideTerm};
use crate::face::kinds::KindId;
use crate::frame::env::Env;
use crate::admit::refusal::Reason;
use crate::rep::CellRef;

use crate::ops::agg::numeric::num_wide::{avg_final, codes, host_final, raise, scaled, slot_ok, sum_final, term, UNREADABLE};

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { kinds: &[(KindId::ClassNumWide, &ClassImpl(&WIDE))], ..crate::ops::NONE };

struct Wide;
static WIDE: Wide = Wide;

/// Whether `a` is an avg (its count at +32); refuses any decl that is not a wide sum / avg at one fixed scale.
fn avg(node: i32, a: &AggDecl<'_>) -> bool {
    if a.scale < 0 || a.dscale_slot != NONE {
        crate::refuse!(node, Reason::Bug { payload: "class.num_wide without one fixed scale" });
    }
    match a.reg {
        R::SumNumWide => false,
        R::AvgNumWide => true,
        _ => crate::refuse!(node, Reason::Bug { payload: "class.num_wide over another register" }),
    }
}

/// One row `[w0..w3]` (its specials code `code`: > 0 a NaN / Infinity / OVERFLOW row, the code on the sentinel's top word)
/// entered where `ok` with `n` rows into `acc`.
#[allow(clippy::too_many_arguments)]
fn fold_row(b: &mut FunctionBuilder<'_>, env: &Env, f: bool, acc: Acc<'_>, code: Value, w: [Value; 4], ok: Value, n: Value) {
    let special = b.icmp(IntCond::Sgt, code, env.zero);
    let sent = b.iconst(Type::I64, SENT_HI);
    let row = [b.select(special, code, w[0]), b.select(special, env.zero, w[1]), b.select(special, env.zero, w[2]), b.select(special, sent, w[3])];
    let row = [b.select(ok, row[0], env.zero), b.select(ok, row[1], env.zero), b.select(ok, row[2], env.zero), b.select(ok, row[3], env.zero), b.select(ok, n, env.zero)];
    let s = load(b, env, f, &acc);
    let s = combine(b, env, &s, &row);
    store(b, env, f, acc, &s);
}

/// The state `[w0, w1, w2, w3, n]`: registers as they are; a slot's words (an invalid slot the identity, zeros).
fn load(b: &mut FunctionBuilder<'_>, env: &Env, avg: bool, acc: &Acc<'_>) -> [Value; 5] {
    let Acc::Slot { base, off, valid } = *acc else {
        let Acc::Regs(w) = acc else { unreachable!() };
        return [w[0], w[1], w[2], w[3], w[4]];
    };
    let ok = slot_ok(b, env, base, valid);
    let mut w = [env.zero; 5];
    for (i, x) in w.iter_mut().enumerate().take(4) {
        let v = b.load(Type::I64, base, off + 8 * i as i32);
        *x = b.select(ok, v, env.zero);
    }
    let n = if avg { b.load(Type::I64, base, off + 32) } else { b.conv(ConvOp::Zext, Type::I64, ok) };
    w[4] = b.select(ok, n, env.zero);
    w
}

fn store(b: &mut FunctionBuilder<'_>, env: &Env, avg: bool, acc: Acc<'_>, w: &[Value; 5]) {
    match acc {
        Acc::Regs(r) => r.copy_from_slice(w),
        Acc::Slot { base, off, valid } => {
            for (i, &v) in w[..4].iter().enumerate() {
                b.store(base, off + 8 * i as i32, v);
            }
            if avg {
                b.store(base, off + 32, w[4]);
            }
            if let CellValid::Bit { off, bit } = valid {
                let any = b.icmp(IntCond::Ne, w[4], env.zero);
                let byte = b.load_zext(Type::I8, base, off as i32);
                let m = b.iconst(Type::I64, 1 << bit);
                let m = b.select(any, m, env.zero);
                let nb = b.int_bin(IntBinOp::Or, byte, m);
                let nb = b.conv(ConvOp::Trunc, Type::I8, nb);
                b.store(base, off as i32, nb);
            }
        }
    }
}

/// `a + c` over two i256 sums either of which may be a sentinel (abi's `num_add256`): the words added with their carries,
/// past |2^254| the OVERFLOW code; a sentinel wins over a finite sum, two combine by `codes` (cold).
fn add256(b: &mut FunctionBuilder<'_>, env: &Env, a: &[Value], c: &[Value]) -> [Value; 4] {
    let sent = b.iconst(Type::I64, SENT_HI);
    let m = b.int_bin(IntBinOp::Smin, a[3], c[3]);
    let slow = b.icmp(IntCond::Eq, m, sent);
    let (l_slow, l_fast, l_join) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::I64; 4]));
    b.mark_cold(l_slow);
    b.cond_br(slow, (l_slow, &[]), (l_fast, &[]));
    b.switch_to(l_fast);
    let mut s = [env.zero; 4];
    let mut k = env.zero;
    for i in 0..4 {
        let x = b.int_bin(IntBinOp::Add, a[i], c[i]);
        let y = b.int_bin(IntBinOp::Add, x, k);
        if i < 3 {
            let (c1, c2) = (b.icmp(IntCond::Ult, x, a[i]), b.icmp(IntCond::Ult, y, x));
            let cy = b.int_bin(IntBinOp::Or, c1, c2);
            k = b.conv(ConvOp::Zext, Type::I64, cy);
        }
        s[i] = y;
    }
    let (lim, span) = (b.iconst(Type::I64, 1 << 62), b.iconst(Type::I64, i64::MIN));
    let h = b.int_bin(IntBinOp::Add, s[3], lim);
    let out = b.icmp(IntCond::Uge, h, span);
    let ovf = b.iconst(Type::I64, SP_OVERFLOW);
    let (s0, s1, s2, s3) = (b.select(out, ovf, s[0]), b.select(out, env.zero, s[1]), b.select(out, env.zero, s[2]), b.select(out, sent, s[3]));
    b.br(l_join, &[s0, s1, s2, s3]);
    b.switch_to(l_slow);
    let (a_s, c_s) = (b.icmp(IntCond::Eq, a[3], sent), b.icmp(IntCond::Eq, c[3], sent));
    let h = b.declare_host_once("class_num_wide::codes", &[Type::I64, Type::I64], Some(Type::I64), codes as *const () as usize, HostFlags { pure: true, ..HostFlags::default() });
    let both = b.call_host(h, &[a[0], c[0]], Some(Type::I64)).expect("returns the code");
    let two = b.int_bin(IntBinOp::And, a_s, c_s);
    let mut w = [env.zero; 4];
    for (i, x) in w.iter_mut().enumerate() {
        let one = b.select(a_s, a[i], c[i]);
        let pair = if i == 0 { both } else if i == 3 { sent } else { env.zero };
        *x = b.select(two, pair, one);
    }
    b.br(l_join, &w);
    b.switch_to(l_join);
    let p = b.block_params(l_join);
    [p[0], p[1], p[2], p[3]]
}

fn combine(b: &mut FunctionBuilder<'_>, env: &Env, a: &[Value; 5], c: &[Value; 5]) -> [Value; 5] {
    let s = add256(b, env, &a[..4], &c[..4]);
    [s[0], s[1], s[2], s[3], b.int_bin(IntBinOp::Add, a[4], c[4])]
}

impl FoldClass for Wide {
    fn class_kind(&self) -> Option<KindId> {
        Some(KindId::ClassNumWide)
    }

    fn shape(&self, a: &AggDecl<'_>) -> AccShape {
        AccShape { words: 5, bytes: if avg(0, a) { 40 } else { 32 }, align: 8 }
    }

    fn init(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>) {
        store(b, env, avg(env.node, a), acc, &[env.zero; 5]);
    }

    /// The argument's datum as a one-row state (`term`, w copies under join.mult), added into `acc`; NULL adds nothing.
    fn step(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>, args: &[CellRef<'_>]) {
        let f = avg(env.node, a);
        let [c] = args else { crate::refuse!(env.node, Reason::Bug { payload: "class.num_wide over other than one argument" }) };
        let tf = gravity_decompose::catalog::typefacts::row_of(gravity_decompose::catalog::NUMERICOID).copied().expect("numeric's type facts");
        let mut cx = env.lx(b, env.node);
        let (ptr, null) = crate::rep::datum(&mut cx, *c, &tf);
        env.memo = cx.memo;
        let w = env.mult.unwrap_or(env.one);
        let (l_call, l_out) = (b.create_block(&[]), b.create_block(&[Type::I64; 5]));
        b.cond_br(null, (l_out, &[env.zero; 5]), (l_call, &[]));
        b.switch_to(l_call);
        let h = b.declare_host_once("class_num_wide::term", &[Type::I64, Type::I64, Type::I64, Type::Ptr], Some(Type::I64), term as *const () as usize, HostFlags { touches_memory: true, ..HostFlags::default() });
        let out = b.declare_stack_slot(32, 8);
        let out = b.stack_addr(out);
        let sc = b.iconst(Type::I64, a.scale as i64);
        let code = b.call_host(h, &[ptr, sc, w, out], Some(Type::I64)).expect("returns the term's code");
        let v: PgVec<'_, Value> = core::iter::once(code).chain((0..4).map(|i| b.load(Type::I64, out, 8 * i))).collect_in(env.mcx);
        b.br(l_out, &v[..]);
        b.switch_to(l_out);
        let v = mcx::to_vec_in(&b.block_params(l_out)[..], env.mcx);
        let bad = b.iconst(Type::I64, UNREADABLE);
        let bad = b.icmp(IntCond::Eq, v[0], bad);
        raise(b, env, bad);
        let yes = b.bconst(true);
        let ok = b.int_bin(IntBinOp::Xor, null, yes);
        fold_row(b, env, f, acc, v[0], [v[1], v[2], v[3], v[4]], ok, w);
    }

    /// A num.window product's term (the plain and keyed ends' tree fold, num_window `wide_term`); join.mult's `w` copies
    /// of it multiplied exactly (`scaled`, past |2^254| the OVERFLOW code).
    fn fold_wide(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>, t: &WideTerm) {
        let f = avg(env.node, a);
        let (mut w, mut code) = (t.w, t.sp.unwrap_or(env.zero));
        let n = match env.mult {
            Some(m) => {
                let words = b.declare_stack_slot(32, 8);
                let p = b.stack_addr(words);
                for (i, &x) in w.iter().enumerate() {
                    b.store(p, 8 * i as i32, x);
                }
                let h = b.declare_host_once("class_num_wide::scaled", &[Type::Ptr, Type::I64], Some(Type::I64), scaled as *const () as usize, HostFlags { touches_memory: true, ..HostFlags::default() });
                let past = b.call_host(h, &[p, m], Some(Type::I64)).expect("returns the overflow flag");
                w = [0, 1, 2, 3].map(|i| b.load(Type::I64, p, 8 * i));
                let (past, ovf) = (b.icmp(IntCond::Ne, past, env.zero), b.iconst(Type::I64, SP_OVERFLOW));
                let fin = b.icmp(IntCond::Eq, code, env.zero);
                let lost = b.int_bin(IntBinOp::And, past, fin);
                code = b.select(lost, ovf, code);
                m
            }
            None => env.one,
        };
        fold_row(b, env, f, acc, code, w, t.ok, n);
    }

    fn merge(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, into: Acc<'_>, from: Acc<'_>) {
        let f = avg(env.node, a);
        let (x, y) = (load(b, env, f, &into), load(b, env, f, &from));
        let s = combine(b, env, &x, &y);
        store(b, env, f, into, &s);
    }

    /// gravity_fn's `num_sum256` / `num_avg256` over the words (a stack copy), NULL for no row.
    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>) -> (Value, Value) {
        let f = avg(env.node, a);
        let w = load(b, env, f, &acc);
        let none = b.icmp(IntCond::Eq, w[4], env.zero);
        let words = b.declare_stack_slot(32, 8);
        let p = b.stack_addr(words);
        for (i, &v) in w[..4].iter().enumerate() {
            b.store(p, 8 * i as i32, v);
        }
        let sc = b.iconst(Type::I64, a.scale as i64);
        let v = match f {
            false => host_final(b, env, "class_num_wide::sum_final", sum_final as *const () as usize, &[p, sc], none),
            true => host_final(b, env, "class_num_wide::avg_final", avg_final as *const () as usize, &[p, w[4], sc], none),
        };
        (v, none)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_decompose::ops::shared::agg_class::{Class, Mods, Split};

    /// G4: only decompose's wide registers are class.num_wide (props.rs's admission rule, the fold's class_of.rs twin); a
    /// packed sum stays class.num_packed and an unscaled (arbitrary-scale) one class.num_varlena.
    #[test]
    fn only_a_wide_register_is_class_num_wide() {
        use gravity_decompose::ops::shared::agg::fold_of;
        let ctx = mcx::MemoryContext::new("g4");
        let node = types_nodes::Node::mk(ctx.mcx(), types_nodes::primnodes::Aggref { aggfnoid: 2114, aggtype: 1700, ..Default::default() }).unwrap();
        let mods = Mods { distinct: 0, order: 0, filter: false, direct: 0, split: Split::Simple, star: false };
        let coll = gravity_decompose::catalog::typefacts::KeyColl { oid: 0, bytes_eq: true, bytes_order: true };
        let decl = |class: Class, scaled: bool, reg: Option<R>| {
            let (r, trans, final_) = fold_of(class, 2114, false, scaled);
            AggDecl { aggno: 0, fnoid: 2114, reg: reg.unwrap_or(r), slot: 0, count_slot: NONE, final_, node, scale: if scaled { 6 } else { -1 }, filter: None, class, trans, mods, coll, dscale_slot: NONE, dscales: &[] }
        };
        for (a, k) in [
            (decl(Class::SumNum, true, Some(R::SumNumWide)), KindId::ClassNumWide),
            (decl(Class::AvgNum, true, Some(R::AvgNumWide)), KindId::ClassNumWide),
            (decl(Class::SumNum, true, None), KindId::ClassNumPacked),
            (decl(Class::AvgNum, false, None), KindId::ClassNumVarlena),
        ] {
            assert_eq!(crate::props::class_kind(&a), k, "{:?}", a.reg);
        }
        assert!(avg(0, &decl(Class::AvgNum, true, Some(R::AvgNumWide))));
        assert_eq!(WIDE.shape(&decl(Class::AvgNum, true, Some(R::AvgNumWide))).bytes, 40);
        assert_eq!(WIDE.shape(&decl(Class::SumNum, true, Some(R::SumNumWide))).bytes, 32);
    }
}
