//! Plain record slots. Hooks / levers: -. Budget 130 lines (files-extra.tsv, +25 % gate). Owner M1-FOLD.
//!
//! Split from plain.rs by responsibility (D267; a record slot's validity and its mark are ops/shared/slots.rs, D271: every
//! slot's validity follows abi's own add_* semantics, `Rec::fold` the one merge): the one-record region's record through
//! the `record` / `record_consumed` hosts, the group's layout with text extrema re-kinded as images, and the implicit
//! count sentinel, and a final's call of one of gravity_expr's `agg_final` hosts. The layout carries the hosted
//! aggregates' `AggState` slots (fold/hosted.rs, V4R-277). Its tests hold v4's records to abi's: two partials folded by `Rec::fold` are the record that saw every
//! row, which is abi's add_* over the rows. Registers nothing; every fold end reads it (keyed too, through plain.rs).
use gravity_abi::breaker::{HashAgg, RecordLayout, SlotKind, HASHAGG_HDR, LAYOUT_IMPLICIT_COUNT, LAYOUT_WORDS};
use gravity_abi::ctx::{hdr, partials};
use gravity_abi::program::prog;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{HostFlags, IntBinOp, IntCond, StateMask, Type, Value};
use gravity_decompose::ops::shared::agg::GroupDecl;
use mcx::{CollectIn, PgVec};

use crate::frame::env::Env;
use crate::frame::px::Px;
use crate::ops::shared::slots::{mark, slot_valid};

/// This pipeline's one record, read in IR once its region holds it open (`len` 1 and the sentinel set, as the host's first call
/// leaves it after checking the layout; SC02, main 09-24), nothing kept across windows; else through the host (`record_at`).
pub(crate) fn record(b: &mut FunctionBuilder<'_>, env: &Env, l: &RecordLayout) -> Value {
    let pb = b.load(Type::Ptr, env.ctx, hdr::PROGRAM as i32);
    let own = { let pp = b.load(Type::Ptr, pb, prog::PARTIAL as i32); b.load(Type::Ptr, pp, partials::OWN as i32) };
    let (len, rec) = (b.load_zext(Type::I32, own, core::mem::offset_of!(HashAgg, len) as i32), b.ptr_add(own, env.zero, 1, HASHAGG_HDR as i32));
    let sen = b.load(Type::I64, rec, l.sentinel_off() as i32);
    let (len1, used) = (b.icmp(IntCond::Eq, len, env.one), b.icmp(IntCond::Ne, sen, env.zero));
    let (open, l_host, l_rec) = (b.int_bin(IntBinOp::And, len1, used), b.create_block(&[]), b.create_block(&[Type::Ptr]));
    b.cond_br(open, (l_rec, &[rec]), (l_host, &[]));
    b.switch_to(l_host);
    let r = record_at(b, env, l, None);
    b.br(l_rec, &[r]);
    b.switch_to(l_rec);
    b.block_params(l_rec)[0]
}

/// The record through host `record` (None) / `record_consumed` (`Some(0)`, the drained region's; `Some(k)`: record_consumed_at, an embedded inner's at its owner's region k, D1339 row D), its layout's words in a stack slot.
pub(crate) fn record_at(b: &mut FunctionBuilder<'_>, env: &Env, l: &RecordLayout, consumed: Option<u64>) -> Value {
    let (name, addr) = match consumed { None => ("record", gravity_kernels::contract::entry2::<gravity_kernels::host_abi::Record> as *const () as usize), Some(0) => ("record_consumed", gravity_kernels::contract::entry2::<gravity_kernels::host_abi::RecordConsumed> as *const () as usize), Some(_) => ("record_consumed_at", gravity_kernels::host_abi::record_consumed_at as *const () as usize) };
    let flags = HostFlags { touches_memory: true, ..HostFlags::default() };
    let at = consumed.filter(|&k| k > 0).map(|k| b.iconst(Type::I64, k as i64));
    let h = b.declare_host_once(name, &[Type::Ptr, Type::Ptr, Type::I64, Type::I64][..3 + at.is_some() as usize], Some(Type::Ptr), addr, flags);
    let s = b.declare_stack_slot((8 * LAYOUT_WORDS) as u32, 8);
    let p = b.stack_addr(s);
    let words: [u64; LAYOUT_WORDS] = l.words();
    for (i, w) in words.iter().enumerate() {
        let c = b.const_raw(Type::I64, *w);
        b.store(p, 8 * i as i32, c);
    }
    let n = b.iconst(Type::I64, HashAgg::bytes(l, 1) as i64);
    b.call_host(h, &[env.ctx, p, n, at.unwrap_or(n)][..3 + at.is_some() as usize], Some(Type::Ptr)).unwrap()
}

/// The group's record layout (its text extrema declared as images by decompose).
pub(crate) fn layout_of(px: &Px<'_, '_>, g: &GroupDecl<'_>) -> RecordLayout {
    let mut l = g.layout.record(px.arena);
    crate::ops::agg::trans::hosted::hosted_slots(px.arena, &mut l, g);
    l
}

/// The implicit count sentinel of the record at `rec` += `n` rows (an nn record's slots are valid once a row folded),
/// marked valid under a validity word as abi's `add_count` marks it (D271: `Rec::fold` folds only a valid slot).
pub(crate) fn sentinel(b: &mut FunctionBuilder<'_>, env: &Env, l: &RecordLayout, rec: Value, n: Value) {
    if l.flags & LAYOUT_IMPLICIT_COUNT == 0 {
        return;
    }
    let o = l.sentinel_off() as i32;
    let cur = b.load(Type::I64, rec, o);
    let v = b.int_bin(IntBinOp::Add, cur, n);
    b.store(rec, o, v);
    if let Some(j) = (0..l.nslots()).find(|&j| l.has_validity() && l.slot_off(j) == l.sentinel_off() as u32) {
        let any = b.icmp(IntCond::Ne, n, env.zero);
        mark(b, env, rec, slot_valid(l, j as u16), any);
    }
}

/// A final through one of gravity_expr's `agg_final` hosts over `(region, words…)`, entered only where the state is not
/// NULL, into a per-call region the sink copies from; a failed render raises the host's error.
pub(crate) fn host_final(b: &mut FunctionBuilder<'_>, env: &Env, name: &'static str, host: usize, words: &[Value], null: Value) -> Value {
    let flags = HostFlags { may_error: true, pure: false, touches_memory: true, worker: false, state: StateMask::NONE };
    let mut params = [Type::I64; 6];
    params[0] = Type::Ptr;
    let h = b.declare_host_once(name, &params[..words.len() + 1], Some(Type::I64), host, flags);
    let (rs, buf) = (b.declare_stack_slot(16, 8), b.declare_stack_slot(128, 8));
    let (l_call, l_join) = (b.create_block(&[]), b.create_block(&[Type::I64]));
    b.cond_br(null, (l_join, &[env.zero]), (l_call, &[]));
    b.switch_to(l_call);
    let (r, p) = (b.stack_addr(rs), b.stack_addr(buf));
    let end = b.ptr_add(p, env.zero, 1, 128);
    b.store(r, 0, p);
    b.store(r, 8, end);
    let args: PgVec<'_, Value> = core::iter::once(r).chain(words.iter().copied()).collect_in(env.mcx);
    let v = b.call_host(h, &args[..], Some(Type::I64)).unwrap();
    let failed = b.icmp(IntCond::Eq, v, env.zero);
    let l_ok = b.create_block(&[]);
    let e = b.iconst(Type::I64, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64);
    b.cond_br(failed, (env.l_raise, &[e]), (l_ok, &[]));
    b.switch_to(l_ok);
    b.br(l_join, &[v]);
    b.switch_to(l_join);
    b.block_params(l_join)[0]
}

#[cfg(test)]
#[allow(unsafe_code)] // the conformance tests read abi's record bytes
mod conform {
    //! D271: v4's plain records are abi-conformant. Per slot class v4 emits, two partials stepped by v4's own class code (in
    //! the slot, and in the carry folded into the slot at the window's end) and folded by abi's `Rec::fold`, the one merge,
    //! equal the partial that saw every row and abi's `add_*` over the same rows: the record's bytes, validity word included.
    use super::*;
    use crate::cell::{Cell, Nullable};
    use crate::face::fold::Acc;
    use crate::rep::CellRef;
    use gravity_decompose::ops::shared::agg::{AggDecl, Register};
    use gravity_decompose::ops::shared::agg_class::{Class, Ext};
    use crate::ops::agg::trans::class_of::class_of;
    use gravity_abi::breaker::Rec;
    use gravity_decompose::ops::shared::agg::fold_of;
    use gravity_decompose::ops::shared::agg::NONE;
    use gravity_decompose::ops::shared::sort::NO_SCALE;
    use gravity_decompose::ops::shared::agg_class::{Bit, Mods, Split};
    use gravity_decompose::catalog::typefacts::KeyColl;
    use gravity_ir::builder::ProgramBuilder;
    use gravity_program::{ExprLife, ExprOut, Rep};
    use mcx::{Mcx, MemoryContext};
    use types_nodes::primnodes::Aggref;
    use types_nodes::Node;

    /// An aggregate under test: class, function, result type, argument type and Rep, numeric scale (`NO_SCALE` off numeric).
    #[derive(Clone, Copy)]
    struct G {
        class: Class,
        fnoid: u32,
        aggtype: u32,
        typid: u32,
        rep: Rep,
        scale: i16,
        /// V4R-38: the wide register and its i256 slot (class.num_wide).
        wide: bool,
    }

    const W8: Rep = Rep::Word { bytes: 8 };
    const NULL: (u64, bool) = (0, true);

    const fn int(class: Class, fnoid: u32, typid: u32) -> G {
        G { class, fnoid, aggtype: 20, typid, rep: W8, scale: NO_SCALE, wide: false }
    }

    const fn float(class: Class, fnoid: u32) -> G {
        G { class, fnoid, aggtype: 701, typid: 701, rep: W8, scale: NO_SCALE, wide: false }
    }

    const fn num(class: Class, fnoid: u32) -> G {
        G { class, fnoid, aggtype: 1700, typid: 1700, rep: Rep::VarlenaPtr, scale: 2, wide: false }
    }

    const fn wide(class: Class, fnoid: u32) -> G {
        G { wide: true, ..num(class, fnoid) }
    }

    /// V4R-38: a wide decl's register and i256 slot (decompose's `wide_tree` rewrite of `fold_of`'s packed ones).
    fn wide_of(class: Class) -> (Register, SlotKind) {
        if class == Class::AvgNum { (Register::AvgNumWide, SlotKind::AvgNumWide) } else { (Register::SumNumWide, SlotKind::SumNumWide) }
    }

    fn decl<'p>(m: Mcx<'p>, g: G, slot: u16, dscale_slot: u16) -> AggDecl<'p> {
        let (mut reg, mut trans, final_) = fold_of(g.class, g.fnoid, false, g.scale != NO_SCALE);
        if g.wide {
            let (r, k) = wide_of(g.class);
            (reg, trans) = (r, gravity_decompose::ops::shared::agg_class::Trans::Abi(k));
        }
        let star = g.class == Class::CountStar;
        let node = Node::mk(m, Aggref { aggfnoid: g.fnoid, aggtype: g.aggtype, aggstar: star, ..Default::default() }).unwrap();
        let mods = Mods { distinct: 0, order: 0, filter: false, direct: 0, split: Split::Simple, star };
        let coll = KeyColl { oid: 0, bytes_eq: true, bytes_order: true };
        AggDecl { aggno: 0, fnoid: g.fnoid, reg, slot, count_slot: NONE, final_, node, scale: g.scale, filter: None, class: g.class, trans, mods, coll, dscale_slot, dscales: &[] }
    }

    /// v4's code over `rows` (the argument's word, NULL) into `rec`: each aggregate's class step in its slot, or (`carry`)
    /// in registers merged into its slot after the last row as frame.carry's window end does; the implicit sentinel after.
    fn fold_rows(l: &RecordLayout, aggs: &[AggDecl<'_>], cells: &[Cell], carry: bool, rec: Rec<'_>, rows: &[(u64, bool)]) {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let mut pb = ProgramBuilder::new();
        let core = crate::frame::env::declare(&mut pb);
        let f = pb.declare_func("t", &[Type::Ptr, Type::Ptr, Type::Ptr], &[Type::I64]);
        let mut b = pb.define(f);
        let (base, rp) = (b.params()[1], b.params()[2]);
        let x = crate::frame::stop::exits(&mut b);
        let mut env = Env::new(&mut b, core, crate::hir::Ir::none(), &[], x, rig.mcx());
        let classes: Vec<_> = aggs.iter().map(|a| class_of(&0i32, a, rig.mcx())).collect();
        let slot = |a: &AggDecl<'_>| Acc::Slot { base, off: l.slot_off(a.slot as usize) as i32, valid: slot_valid(l, a.slot) };
        let mut accs: Vec<Vec<Value>> = Vec::new();
        for (a, c) in aggs.iter().zip(&classes) {
            let mut w = vec![env.zero; c.shape(a).words as usize];
            if carry {
                c.init(&mut b, &mut env, a, Acc::Regs(&mut w));
            }
            accs.push(w);
        }
        for i in 0..rows.len() {
            let v = b.load(Type::I64, rp, 16 * i as i32);
            let nw = b.load(Type::I64, rp, 16 * i as i32 + 8);
            let null = b.icmp(IntCond::Ne, nw, env.zero);
            for (k, a) in aggs.iter().enumerate() {
                let acc = if carry { Acc::Regs(&mut accs[k]) } else { slot(a) };
                let args: Vec<CellRef<'_>> = if a.mods.star { vec![] } else { vec![CellRef { cell: &cells[k], val: v, null: Some(null) }] };
                classes[k].step(&mut b, &mut env, a, acc, &args);
            }
            if !carry {
                let one = env.one;
                sentinel(&mut b, &env, l, base, one);
            }
        }
        if carry {
            for (k, a) in aggs.iter().enumerate() {
                classes[k].merge(&mut b, &mut env, a, slot(a), Acc::Regs(&mut accs[k]));
            }
            let n = b.iconst(Type::I64, rows.len() as i64);
            sentinel(&mut b, &env, l, base, n);
        }
        let ok = env.zero;
        b.ret(&[ok]);
        for (blk, st) in [(x.l_finish, -1), (x.l_stop_in, -1), (x.l_full, -1), (x.l_status, -1), (x.l_done, -1), (x.l_raise, -2), (x.l_cancel, -1), (x.l_exit, -1)] {
            b.switch_to(blk);
            let s = b.iconst(Type::I64, st);
            b.ret(&[s]);
        }
        let prog = pb.finish();
        let mut ctx = vec![0u64; gravity_abi::ctx::ctx_size(1) / 8];
        let words: Vec<u64> = rows.iter().flat_map(|&(v, n)| [v, n as u64]).collect();
        let got = gravity_interp::Interp::new(&prog).run(f, &[ctx.as_mut_ptr() as u64, rec.as_ptr() as u64, words.as_ptr() as u64]).expect("runs");
        assert_eq!(got[0] as i64, 0, "the fold ran to its end (-2: it raised)");
    }

    /// abi's own transition of slot `j` over one row's word (the reference the class code must match).
    fn abi_add(l: &RecordLayout, r: &Rec<'_>, a: &AggDecl<'_>, v: u64) {
        let j = a.slot as usize;
        let i = v as i64;
        match l.slot_kind(j) {
            SlotKind::Count => r.add_count(j, 1),
            SlotKind::SumI64 => r.add_i64(j, i),
            SlotKind::SumI128 => r.add_i128(j, i as i128),
            SlotKind::AvgI128 => r.add_avg(j, i as i128, 1),
            SlotKind::MinI64 | SlotKind::MaxI64 => {
                let w = match a.reg {
                    Register::MinF64 | Register::MaxF64 => {
                        let m = i64::MAX;
                        if i & m > 0x7FF0_0000_0000_0000 { m } else { i ^ ((i >> 63) & m) }
                    }
                    Register::SumNum | Register::AvgNum => unreachable!(),
                    _ => i,
                };
                if l.slot_kind(j) == SlotKind::MinI64 { r.min_i64(j, w) } else { r.max_i64(j, w) }
            }
            SlotKind::BitAnd | SlotKind::BitOr | SlotKind::BitXor => r.fold_bits(j, i),
            SlotKind::SumF64 => r.add_f64(j, f64::from_bits(v)).unwrap(),
            SlotKind::F64Accum => r.combine_f64_accum(j, [1.0, f64::from_bits(v), 0.0]).unwrap(),
            SlotKind::SumNum | SlotKind::AvgNum => {
                let (m, d) = numeric(v);
                let m = m * 10i128.pow((a.scale as u32).saturating_sub(d as u32));
                if l.slot_kind(j) == SlotKind::SumNum { r.add_num(j, m) } else { r.add_avg_num(j, m, 1) }
                if a.dscale_slot != NONE {
                    r.max_i64(a.dscale_slot as usize, d);
                }
            }
            SlotKind::SumNumWide | SlotKind::AvgNumWide => {
                let (m, d) = numeric(v);
                let m = m * 10i128.pow((a.scale as u32).saturating_sub(d as u32));
                let w = [m as u64, (m >> 64) as u64, (m >> 127) as u64, (m >> 127) as u64];
                if l.slot_kind(j) == SlotKind::SumNumWide { r.add_num256(j, w) } else { r.add_avg_num256(j, w, 1) }
            }
            k => panic!("no reference for {k:?}"),
        }
    }

    /// A short numeric image's value as (mantissa at its dscale, dscale): the test's images are positive, weight 0.
    fn numeric(p: u64) -> (i128, i64) {
        // SAFETY: `p` is one of the test's images: a 4-byte varlena header, numeric.c's short header, base-10000 digits.
        let (len, h) = unsafe { ((*(p as *const u32) >> 2) as usize, *((p + 4) as *const u16)) };
        let d = ((h & 0x1F80) >> 7) as i64;
        let mut v = 0i128;
        for i in 0..(len - 6) / 2 {
            let g = unsafe { *((p + 6 + 2 * i as u64) as *const u16) } as i128;
            let e = d - 4 * i as i64;
            v += if e >= 0 { g * 10i128.pow(e as u32) } else { g / 10i128.pow((-e) as u32) };
        }
        (v, d)
    }

    /// numeric.c's short image of a positive value of weight 0 at dscale `d` (the integer digit, then the fraction's).
    fn img(d: u16, digits: &[u16]) -> Vec<u8> {
        let mut v = (((4 + 2 + 2 * digits.len()) as u32) << 2).to_le_bytes().to_vec();
        v.extend((0x8000u16 | d << 7).to_le_bytes());
        digits.iter().for_each(|g| v.extend(g.to_le_bytes()));
        v
    }

    /// The record of `aggs` (a D256 dscale companion after each when `ds`), the count(*) slot its sentinel as decompose
    /// declares it, else the implicit count; each aggregate's slot.
    fn layout(aggs: &[G], validity: bool, ds: bool) -> (RecordLayout, Vec<u16>) {
        let mut kinds = Vec::new();
        let mut slots = Vec::new();
        for g in aggs {
            let (_, trans, _) = fold_of(g.class, g.fnoid, false, g.scale != NO_SCALE);
            slots.push(kinds.len() as u16);
            kinds.push(if g.wide { wide_of(g.class).1 } else { trans.abi().expect("an abi slot") });
            if ds {
                kinds.push(SlotKind::MaxI64);
            }
        }
        let star = aggs.iter().position(|g| g.class == Class::CountStar).map(|i| slots[i] as usize);
        (RecordLayout::with_sentinel(&[], &kinds, validity, star), slots)
    }

    fn table(l: &RecordLayout) -> Vec<u64> {
        let mut v = vec![0u64; HashAgg::bytes(l, 1).div_ceil(8) as usize];
        // SAFETY: the region is `HashAgg::bytes(l, 1)` long.
        unsafe { HashAgg::at(v.as_mut_ptr() as *mut u8, l, 1, HashAgg::bytes(l, 1)) }.single();
        v
    }

    fn rec<'a>(v: &'a mut [u64]) -> Rec<'a> {
        // SAFETY: `table` laid a one-record table there.
        unsafe { HashAgg::view(v.as_ptr() as *const u8) }.record(0)
    }

    /// Every split of `rows` into two partials, in the slot and in the carry: `Rec::fold` of the two (into the first, and
    /// into an empty leader) is the record that saw every row, which is abi's `add_*` over the rows.
    fn conforms(gs: &[G], validity: bool, rows: &[(u64, bool)], ds: bool) {
        let ctx = MemoryContext::new("conform");
        let (l, slots) = layout(gs, validity, ds);
        let aggs: Vec<AggDecl<'_>> = gs.iter().zip(&slots).map(|(&g, &s)| decl(ctx.mcx(), g, s, if ds { s + 1 } else { NONE })).collect();
        let cells: Vec<Cell> = gs.iter().map(|g| Cell::of_expr(ExprOut { rep: g.rep, typid: g.typid, life: ExprLife::Row }, 0, 0, Nullable::Maybe)).collect();
        let sentinel = (0..l.nslots()).find(|&j| l.flags & LAYOUT_IMPLICIT_COUNT != 0 && l.slot_off(j) == l.sentinel_off() as u32);
        let mut want = table(&l);
        for &(v, null) in rows {
            let r = rec(&mut want);
            for a in aggs.iter().filter(|a| a.mods.star || !null) {
                abi_add(&l, &r, a, v);
            }
            if let Some(j) = sentinel {
                r.add_count(j, 1);
            }
        }
        let name = |carry: bool, k: usize| format!("{:?} validity={validity} carry={carry} split={k}", gs.iter().map(|g| g.class).collect::<Vec<_>>());
        for carry in [false, true] {
            let mut all = table(&l);
            fold_rows(&l, &aggs, &cells, carry, rec(&mut all), rows);
            assert_eq!(rec(&mut all).bytes(), rec(&mut want).bytes(), "{}: v4's record is abi's", name(carry, rows.len()));
            for k in 0..=rows.len() {
                let (mut p, mut q, mut leader) = (table(&l), table(&l), table(&l));
                fold_rows(&l, &aggs, &cells, carry, rec(&mut p), &rows[..k]);
                fold_rows(&l, &aggs, &cells, carry, rec(&mut q), &rows[k..]);
                rec(&mut leader).fold(&rec(&mut p)).unwrap();
                rec(&mut leader).fold(&rec(&mut q)).unwrap();
                rec(&mut p).fold(&rec(&mut q)).unwrap();
                assert_eq!(rec(&mut p).bytes(), rec(&mut want).bytes(), "{}: partial + partial", name(carry, k));
                assert_eq!(rec(&mut leader).bytes(), rec(&mut want).bytes(), "{}: an empty leader + both", name(carry, k));
            }
        }
    }

    fn ints(xs: &[Option<i64>]) -> Vec<(u64, bool)> {
        xs.iter().map(|x| x.map_or(NULL, |v| (v as u64, false))).collect()
    }

    #[test]
    fn word_records_are_abi_records() {
        let rows = ints(&[Some(5), None, Some(-3), Some(12), None, Some(7), Some(0)]);
        let bools = ints(&[Some(1), None, Some(0), Some(1)]);
        let singles = [
            (int(Class::CountStar, 2803, 20), &rows),
            (int(Class::CountAny, 2147, 20), &rows),
            (int(Class::SumI64, 2108, 20), &rows),
            (int(Class::SumI128, 2107, 20), &rows),
            (int(Class::AvgI128, 2101, 20), &rows),
            (int(Class::AvgI128, 2100, 20), &rows),
            (int(Class::Min(Ext::Int), 2131, 20), &rows),
            (int(Class::Max(Ext::Int), 2115, 20), &rows),
            (int(Class::BitI64(Bit::And), 2240, 20), &rows),
            (int(Class::BitI64(Bit::Or), 2241, 20), &rows),
            (int(Class::BitI64(Bit::Xor), 8452, 20), &rows),
            (int(Class::BoolAnd, 2517, 16), &bools),
            (int(Class::BoolOr, 2518, 16), &bools),
        ];
        for (g, rows) in singles {
            conforms(&[g], true, rows, false);
            // a NOT NULL layout (no validity word): a count over any argument, or a sum / avg over a NOT NULL one
            let counted = matches!(g.class, Class::CountStar | Class::CountAny);
            if counted || matches!(g.class, Class::SumI64 | Class::SumI128 | Class::AvgI128) {
                let nn: Vec<(u64, bool)> = rows.iter().copied().filter(|&(_, null)| counted || !null).collect();
                conforms(&[g], false, &nn, false);
            }
        }
        // one record of several (the D271 statement's count / count(x) / sum / avg / min / max / bit_or)
        let many = [int(Class::CountStar, 2803, 20), int(Class::CountAny, 2147, 20), int(Class::SumI128, 2107, 20), int(Class::AvgI128, 2100, 20), int(Class::Min(Ext::Int), 2131, 20), int(Class::Max(Ext::Int), 2115, 20), int(Class::BitI64(Bit::Or), 2241, 20)];
        conforms(&many, true, &rows, false);
        conforms(&many[2..], true, &rows, false);
    }

    #[test]
    fn float_records_are_abi_records() {
        let rows: Vec<(u64, bool)> = [Some(1.0f64), None, Some(2.0), Some(3.0), Some(4.0)].iter().map(|x| x.map_or(NULL, |v| (v.to_bits(), false))).collect();
        for g in [float(Class::SumF64, 2111), float(Class::AvgF64, 2105), float(Class::Min(Ext::F64), 2136), float(Class::Max(Ext::F64), 2120)] {
            conforms(&[g], true, &rows, false);
        }
        conforms(&[float(Class::SumF64, 2111), float(Class::AvgF64, 2105)], false, &rows[2..], false);
    }

    #[test]
    fn numeric_records_are_abi_records() {
        let ims = [img(2, &[1, 5000]), img(2, &[2, 2500]), img(2, &[10, 500])];
        let rows = vec![(ims[0].as_ptr() as u64, false), NULL, (ims[1].as_ptr() as u64, false), (ims[2].as_ptr() as u64, false)];
        for g in [num(Class::SumNum, 2114), num(Class::AvgNum, 2103)] {
            conforms(&[g], true, &rows, false);
        }
        // D256: arms of dscales 1 and 2 summed at scale 2, the dscale companion after the sum
        let mixed = [img(1, &[1, 5000]), img(2, &[2, 2500]), img(0, &[7])];
        let rows = vec![(mixed[0].as_ptr() as u64, false), NULL, (mixed[1].as_ptr() as u64, false), (mixed[2].as_ptr() as u64, false)];
        for g in [num(Class::SumNum, 2114), num(Class::AvgNum, 2103)] {
            conforms(&[g], true, &rows, true);
        }
    }

    /// V4R-38: class.num_wide's i256 records are abi's (`Rec::fold`'s `num_add256`, the worker merge), with a validity word
    /// and without (an avg's count at +32), beside a packed sum.
    #[test]
    fn wide_numeric_records_are_abi_records() {
        let ims = [img(2, &[1, 5000]), img(2, &[2, 2500]), img(2, &[10, 500])];
        let rows = vec![(ims[0].as_ptr() as u64, false), NULL, (ims[1].as_ptr() as u64, false), (ims[2].as_ptr() as u64, false)];
        for g in [wide(Class::SumNum, 2114), wide(Class::AvgNum, 2103)] {
            conforms(&[g], true, &rows, false);
            conforms(&[g], false, &[rows[0], rows[2], rows[3]], false);
        }
        conforms(&[wide(Class::SumNum, 2114), num(Class::SumNum, 2114), wide(Class::AvgNum, 2103)], true, &rows, false);
    }
}
