//! D297's wide form with a D256 dscale word (V4R-549). Hooks / levers: -. Budget 130 lines (files-extra.tsv, +25 % gate).
//! Owner M1-FOLD. Registers nothing. A spilled sum / avg (num_carry.rs) over CASE arms of different dscales that no
//! num.window tree takes (a Sorted keyed end, an arm past the strict tree's bound, road num.window off; e.g. a sum of
//! `CASE WHEN p THEN x * (1 - y) ELSE 0 END`) folds its datum as class.num_packed (`DATUM_DS`: class_num.rs's carry form, dscale word, merge and finals), a row's
//! term its value at the scale as an exact i128 with the image's own dscale (`term_ds`; a special's 0, as PG's maxScale).
//! No answer, block, order or put: an answered window's sum carries no dscale word. `guard`: the no-widen list (D2194's form).
use gravity_decompose::ops::shared::agg::AggDecl;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::Op;
use gravity_fn::agg_final::{SP_NAN, SP_NINF, SP_PINF};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{HostFlags, IntBinOp, IntCond, Type, Value};
use mcx::{CollectIn, PgVec};

use crate::face::fold::{Acc, AccShape, FoldClass};
use crate::face::kinds::KindId;
use crate::frame::env::Env;
use crate::graph::Gx;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::rep::CellRef;
use crate::ops::agg::numeric::num_word::Term;
pub(crate) use crate::ops::agg::numeric::raw::datum::term_ds;

pub(crate) struct DatumDs;
pub(crate) static DATUM_DS: DatumDs = DatumDs;

fn num() -> &'static dyn FoldClass { crate::face::fold::class(&0, KindId::ClassNumPacked) }

impl FoldClass for DatumDs {
    fn shape(&self, a: &AggDecl<'_>) -> AccShape { num().shape(a) }
    fn init(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>) { num().init(b, env, a, acc) }
    fn merge(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, into: Acc<'_>, from: Acc<'_>) { num().merge(b, env, a, into, from) }
    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>) -> (Value, Value) { num().finish(b, env, a, acc) }
    fn datum_ds(&self) -> bool { true }

    /// The argument's datum as one term with its dscale (NULL enters nothing), folded as class_num.rs folds a tree's.
    fn step(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>, args: &[CellRef<'_>]) {
        let [c] = args else { crate::refuse!(env.node, Reason::Bug { payload: "class.num_packed's datum over other than one argument" }) };
        let tf = gravity_decompose::catalog::typefacts::row_of(1700).copied().expect("numeric's type facts");
        let mut cx = env.lx(b, env.node);
        let (ptr, null) = crate::rep::datum(&mut cx, *c, &tf);
        env.memo = cx.memo;
        let (l_call, l_out) = (b.create_block(&[]), b.create_block(&[Type::I64; 4]));
        b.cond_br(null, (l_out, &[env.zero; 4]), (l_call, &[]));
        b.switch_to(l_call);
        let h = b.declare_host_once("num_datum::term_ds", &[Type::I64, Type::I64, Type::Ptr], Some(Type::I64), term_ds as *const () as usize, HostFlags { may_error: true, touches_memory: true, ..HostFlags::default() });
        let out = b.declare_stack_slot(24, 8);
        let (out, sc) = (b.stack_addr(out), b.iconst(Type::I64, a.scale as i64));
        let code = b.call_host(h, &[ptr, sc, out], Some(Type::I64)).expect("returns the term's code");
        let v: PgVec<'_, Value> = core::iter::once(code).chain((0..3).map(|i| b.load(Type::I64, out, 8 * i))).collect_in(env.mcx);
        (b.br(l_out, &v[..]), b.switch_to(l_out));
        let v = mcx::to_vec_in(&b.block_params(l_out)[..], env.mcx);
        // a negative code: term_ds stashed its error, raised here
        let (bad, l_bad, l_ok) = (b.icmp(IntCond::Slt, v[0], env.zero), b.create_block(&[]), b.create_block(&[]));
        (b.mark_cold(l_bad), b.cond_br(bad, (l_bad, &[]), (l_ok, &[])), b.switch_to(l_bad));
        let e = b.iconst(Type::I64, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64);
        (b.br(env.l_raise, &[e]), b.switch_to(l_ok));
        let yes = b.bconst(true);
        let ok = b.int_bin(IntBinOp::Xor, null, yes);
        crate::ops::agg::numeric::class_num::fold(b, env, a, acc, &Term { lo: v[1], hi: Some(v[2]), sp: Some(v[0]), ok, ds: Some(v[3]) }, None);
    }
}

/// The no-widen list over DATUM_DS, one form left: a statement folding DATUM_DS that also holds a merge join, or
/// an outer-first hash probe whose build calls a user or VOLATILE function (or whose build is unknown), is refused by the
/// form's name, statement-wide (the graph pipelines and every embedded inner), fail closed. It stays while D2325 (an
/// aggregate over a hash join whose probe holds a hashed NOT IN SubPlan crashes once this refusal is lifted) is open. The
/// keyed end (keyed/hashagg_rec.rs) and the plain end (fold/plain.rs) call `guard` on the class they chose. The forms
/// named for D2178 / D2179 (a CTE or a write outside the top pipeline), D2180 (a bitmap key under a BitmapOr that is no
/// Const), D2197 (a set operation) and D2213 (a SubPlan beside a pgrcolumnar scan) left the list with their fixes: those
/// statements fold DATUM_DS like any other (e2e v4/m8_d297_wide_ds, W0 + W8).
pub(crate) const NW_D2194: &str = "V4R-549 no-widen: a merge join, or a hash join on the outer-first path whose build calls a user or VOLATILE function (D2194 / D2147 / D2201)";

/// `class` is DATUM_DS (by type, D720: zero-sized classes share addresses) and the statement holds the listed form: refused.
pub(crate) fn guard(gx: &Gx<'_, '_>, node: i32, class: &'static dyn FoldClass) {
    if class.datum_ds() {
        if let Some(why) = no_widen(gx) {
            crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) });
        }
    }
}

/// The listed form, if the statement holds it: any merge join, or an outer-first hash probe whose build
/// (join/build_class.rs `side`, its feeders' too) calls a user or VOLATILE function, or whose build is unknown (D2194;
/// kept for D2325).
fn no_widen(gx: &Gx<'_, '_>) -> Option<&'static str> {
    use crate::ops::hashjoin::build_class::{side, Side};
    let ps = gx.pipelines();
    let all = || (0..ps.end()).filter_map(|k| ps.get(k).map(|p| (k, p)));
    let ops = |f: &dyn Fn(usize, &Op<'_>) -> bool| all().any(|(k, p)| p.ops.iter().any(|o| f(k, o)));
    let lazy = |k: usize, s: u8| gx.life_of(k, s).is_none_or(|l| l.0 as usize >= ps.len() || side(gx, l.0 as usize) == Side::UserFn);
    ops(&|k, o| matches!(o, Op::Join(JoinDesc::Merge { .. })) || matches!(*o, Op::Join(JoinDesc::Probe { slot, outer_first: true, .. }) if lazy(k, slot))).then_some(NW_D2194)
}

const PAST: &str = "gravity: a numeric term past 128 bits";
const UNSCALED: &str = "gravity: a numeric value the mantissa fold cannot read (an unscaled image)";

/// numeric.c's payload (the short or long header, then base-10000 digits) at `scale`: (code, value, dscale).
pub(crate) fn read(x: &[u8], scale: i64) -> Result<(i64, i128, u64), &'static str> {
    let rd = |i: usize| u16::from_le_bytes([x[2 * i], x[2 * i + 1]]);
    let h = rd(0);
    let (neg, weight, d, first) = match h & 0xC000 {
        0xC000 => return Ok((match h & 0xF000 { 0xD000 => SP_PINF, 0xF000 => SP_NINF, _ => SP_NAN }, 0, 0)),
        0x8000 => (h & 0x2000 != 0, if h & 0x0040 != 0 { (h | !0x3F) as i16 } else { (h & 0x3F) as i16 }, (h & 0x1F80) >> 7, 1),
        s => (s == 0x4000, rd(1) as i16, h & 0x3FFF, 2),
    };
    let n = x.len() / 2;
    if d as i64 > scale { return Err(UNSCALED) }
    let v = (first..n).try_fold(0i128, |v, i| v.checked_mul(10_000)?.checked_add(rd(i) as i128)).ok_or(PAST)?;
    let e = 4 * (weight as i64 - (n - first) as i64 + 1) + scale;
    let v = match e {
        _ if v == 0 => 0,
        0.. => 10i128.checked_pow(e as u32).and_then(|p| v.checked_mul(p)).ok_or(PAST)?,
        _ => match 10i128.checked_pow(-e as u32) {
            Some(p) if v % p == 0 => v / p,
            _ => return Err(UNSCALED),
        },
    };
    Ok((0, if neg { -v } else { v }, d as u64))
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use super::*;
    use gravity_abi::breaker::SlotKind;
    use gravity_decompose::ops::shared::agg::fold_of;
    use gravity_decompose::ops::shared::agg::{Register, NONE};
    use gravity_decompose::ops::shared::agg_class::{Class, Mods, Split, Trans};

    /// numeric.c's long image (4-byte varlena header, sign / dscale word, weight, base-10000 digits).
    fn long(neg: bool, dscale: u16, weight: i16, digits: &[u16]) -> Vec<u8> {
        let mut v = (((4 + 4 + 2 * digits.len()) as u32) << 2).to_le_bytes().to_vec();
        v.extend((if neg { 0x4000 } else { 0 } | dscale).to_le_bytes());
        v.extend(weight.to_le_bytes());
        digits.iter().for_each(|d| v.extend(d.to_le_bytes()));
        v
    }

    /// numeric.c's short image behind a 1-byte varlena header (an odd-aligned payload).
    fn short(neg: bool, dscale: u16, weight: i16, digits: &[u16]) -> Vec<u8> {
        let mut v = vec![0u8, (((1 + 2 + 2 * digits.len()) as u8) << 1) | 1];
        let h = 0x8000 | if neg { 0x2000 } else { 0 } | dscale << 7 | (weight as u16 & 0x7F);
        v.extend(h.to_le_bytes());
        digits.iter().for_each(|d| v.extend(d.to_le_bytes()));
        v
    }

    /// `term_ds` over `img[at..]`: (code, value, dscale).
    fn run(img: &[u8], at: usize, scale: i64) -> (i64, i128, u64) {
        let mut out = [7u64; 3];
        // SAFETY: `img[at..]` is a numeric image; `out` three words.
        let code = unsafe { term_ds(img[at..].as_ptr() as u64, scale, out.as_mut_ptr() as u64) };
        (code, ((out[1] as u128) << 64 | out[0] as u128) as i128, out[2])
    }

    /// A datum's term: exact at the scale with the image's own dscale (a smaller one widened), signed, both headers, zero;
    /// the specials their codes at dscale 0; a dscale past the scale and a value past i128 raise by name (-1, stashed).
    #[test]
    fn a_datum_reads_as_an_exact_i128_term_with_its_dscale() {
        // 12345.6789 at scale 4: digits 1 2345 6789 at weight 1
        assert_eq!(run(&long(false, 4, 1, &[1, 2345, 6789]), 0, 4), (0, 123_456_789, 4));
        assert_eq!(run(&short(false, 4, 1, &[1, 2345, 6789]), 1, 4), (0, 123_456_789, 4));
        assert_eq!(run(&short(true, 2, 0, &[7, 2500]), 1, 4), (0, -72_500, 2), "7.25 widened to scale 4, dscale 2 kept");
        assert_eq!(run(&short(false, 0, 1, &[1]), 1, 4), (0, 100_000_000, 0), "10000: a stripped trailing group");
        assert_eq!(run(&short(false, 0, 0, &[]), 1, 4), (0, 0, 0), "0 (ELSE 0) at dscale 0");
        assert_eq!(run(&short(false, 4, 0, &[]), 1, 4), (0, 0, 4), "0.0000 keeps dscale 4");
        assert_eq!(run(&short(true, 4, -1, &[5]), 1, 4), (0, -5, 4), "-0.0005: weight -1");
        assert_eq!(run(&short(false, 1, -1, &[5000]), 1, 1), (0, 5, 1), "0.5 at scale 1: the group's zeros divided off");
        // 37 digits at scale 4: 10^33 - 10^-4 (9 then nine 9999 groups at weight 8)
        let big = long(false, 4, 8, &[9, 9999, 9999, 9999, 9999, 9999, 9999, 9999, 9999, 9999]);
        assert_eq!(run(&big, 0, 4), (0, 10i128.pow(37) - 1, 4));
        assert_eq!(run(&long(true, 4, 8, &[9, 9999, 9999, 9999, 9999, 9999, 9999, 9999, 9999, 9999]), 0, 4), (0, 1 - 10i128.pow(37), 4));
        for (h, code) in [(0xC000u16, SP_NAN), (0xD000, SP_PINF), (0xF000, SP_NINF)] {
            let mut s = (8u32 << 2).to_le_bytes().to_vec();
            s.extend(h.to_le_bytes());
            s.extend([0u8; 2]);
            assert_eq!(run(&s, 0, 4), (code, 0, 0), "a special: its code, dscale 0");
        }
        assert_eq!(run(&long(false, 6, 0, &[1, 2345, 6700]), 0, 4).0, -1, "dscale 6 past scale 4");
        assert_eq!(gravity_fn::take_error().map(|e| e.message().to_owned()).as_deref(), Some(UNSCALED));
        assert_eq!(run(&long(false, 0, 10, &[1]), 0, 4).0, -1, "10^40 x 10^4 is past i128");
        assert_eq!(gravity_fn::take_error().map(|e| e.message().to_owned()).as_deref(), Some(PAST));
    }

    fn decl(ctx: &mcx::MemoryContext, trans: Option<SlotKind>, companion: bool) -> AggDecl<'_> {
        let (reg, t, final_) = fold_of(Class::SumNum, 2114, false, true);
        let node = types_nodes::Node::mk(ctx.mcx(), types_nodes::primnodes::Aggref { aggfnoid: 2114, aggtype: 1700, ..Default::default() }).unwrap();
        let mods = Mods { distinct: 0, order: 0, filter: false, direct: 0, split: Split::Simple, star: false };
        let coll = gravity_decompose::catalog::typefacts::KeyColl { oid: 0, bytes_eq: true, bytes_order: true };
        let (trans, dscale_slot) = (trans.map_or(t, Trans::Abi), if companion { 2 } else { NONE });
        let dscales: &[u8] = &[4, 0];
        AggDecl { aggno: 0, fnoid: 2114, reg, slot: 1, count_slot: NONE, final_, node, scale: 4, filter: None, class: Class::SumNum, trans, mods, coll, dscale_slot, dscales }
    }

    struct Other;
    impl FoldClass for Other {
        fn shape(&self, _a: &AggDecl<'_>) -> AccShape { AccShape { words: 1, bytes: 8, align: 8 } }
        fn init(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _a: &AggDecl<'_>, _acc: Acc<'_>) {}
        fn step(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _a: &AggDecl<'_>, _acc: Acc<'_>, _args: &[CellRef<'_>]) {}
        fn merge(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _a: &AggDecl<'_>, _into: Acc<'_>, _from: Acc<'_>) {}
        fn finish(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env, _a: &AggDecl<'_>, _acc: Acc<'_>) -> (Value, Value) { (env.zero, env.zero) }
    }
    static VARLENA: Other = Other;

    /// num_carry.rs `datum` (props/code.rs fold_kind's twin; tests/props_needs.rs checks that one on a CASE-sum plan): a
    /// spilled sum with a dscale word is DATUM_DS (class.num_packed's state, 5 words, the i256 slot); does not apply: a
    /// spilled one without it keeps class.num_wide's datum, an unspilled one (DISTINCT / ORDER BY inside the call:
    /// decompose spills neither) the varlena class it is handed. DATUM_DS answers no window (the answer's sum has no word).
    #[test]
    fn only_a_spilled_sum_with_a_dscale_word_folds_its_datum_here() {
        let ctx = mcx::MemoryContext::new("v4r549");
        let other = |c: &'static dyn FoldClass, a: &AggDecl<'_>| c.shape(a) == Other.shape(a); // by type, not address (D720)
        let ds = decl(&ctx, Some(SlotKind::SumNumWide), true);
        let c = crate::ops::agg::numeric::num_carry::datum(&0, &ds, &VARLENA);
        assert!(c.datum_ds(), "spilled + companion: DATUM_DS");
        assert_eq!((c.shape(&ds).words, c.shape(&ds).bytes), (5, 32), "class.num_packed's [lo, hi, carry, dscale, n] over the i256 slot");
        assert!(c.answer(&ds).is_none() && c.block(&ds).is_none() && c.order(&ds).is_none() && !c.puts(&ds));
        let wide = decl(&ctx, Some(SlotKind::SumNumWide), false);
        let c = crate::ops::agg::numeric::num_carry::datum(&0, &wide, &VARLENA);
        assert!(!c.datum_ds() && !other(c, &wide), "spilled, no companion: class.num_wide's datum");
        for companion in [true, false] {
            let un = decl(&ctx, None, companion);
            assert_eq!(un.reg, Register::SumNum);
            let c = crate::ops::agg::numeric::num_carry::datum(&0, &un, &VARLENA);
            assert!(!c.datum_ds() && other(c, &un), "unspilled: varlena");
        }
    }

    /// Does not apply (D720's form): no registered class is DATUM_DS, nor any zero-sized one placed at DATUM_DS's address as a
    /// link may place it (the 04:41Z T0 link put class.text's TEXT there; the address compare refused its min / max).
    #[test]
    fn a_zero_sized_class_at_datum_ds_s_address_is_not_datum_ds() {
        assert!(DATUM_DS.datum_ds());
        let at = core::ptr::addr_of!(DATUM_DS).addr();
        let rows = crate::face::kinds::KINDS.iter().flat_map(|d| crate::ops::kind_rows(d.id).iter().map(move |m| (d.id, *m)));
        let (mut moved, mut text) = (0, false);
        for (k, c) in rows.filter_map(|(k, m)| m.downcast_ref::<crate::face::fold::ClassImpl>().map(|c| (k, c.0))) {
            assert!(!c.datum_ds(), "{k:?}: a registered class is not DATUM_DS");
            if core::mem::size_of_val(c) != 0 || at % core::mem::align_of_val(c) != 0 {
                continue;
            }
            // SAFETY: a zero-sized value is valid at any non-null address aligned for it (no byte is read)
            let c: &dyn FoldClass = unsafe { &*(c as *const dyn FoldClass).with_addr(at) };
            assert!(!c.datum_ds(), "{k:?}: a zero-sized class at DATUM_DS's address is not DATUM_DS");
            (moved, text) = (moved + 1, text || k == KindId::ClassText);
        }
        assert!(moved > 0 && text, "class.text's TEXT is zero-sized and was placed at DATUM_DS ({moved} placed)");
    }
}
