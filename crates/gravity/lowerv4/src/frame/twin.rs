//! Twin arms (D3585): the arms of one union over partitions of one shape lower to functions that differ only in where
//! their site lives (the slot's ctx offsets and the site index a host takes) and which of the frame's words hold their
//! literals. One function serves them all: the first arm's code, with the site slot, the site index and the word shift
//! taken as parameters; each arm's pipeline function becomes a stub that calls it with its own. The backend compiles the
//! shared body once, not once per arm (4000 partitions: 4000 bodies before). Each arm keeps its pipeline, site, words and
//! protocols in the descriptors, so pruning, sections, rescans and EXPLAIN see the program they saw before; only the code
//! is shared.
//!
//! An arm joins only when its function equals the first arm's, instruction for instruction, except where a parameter
//! accounts for the difference exactly, so the shared body run with an arm's parameters is that arm's function:
//! - a load, store, prefetch or address off the ctx inside the first arm's site slot, moved by the arms' site distance;
//! - an `i64` constant that is each arm's own site index;
//! - a load off the frame's words (`PARAMS`) or their null flags (`PARAMS_NULL`), moved by one word shift per arm;
//! - a host call to another declaration of the same host (name, signature, address and flags equal);
//! - D3659: an integer constant of one type that is each arm's own (a write arm's target relation, the code its exit
//!   reports): a parameter of its own, each arm's value passed by its stub.
//!
//! Every arm of a group differs from the first at the same instructions, else the group ends before it. Pipelines outside
//! an `Arms` run, arms with no twin beside them, and every program with EXPLAIN ANALYZE's counters keep their IR byte for
//! byte.
use gravity_abi::ctx::{hdr, site_off};
use gravity_abi::SITE_SIZE;
use gravity_ir::{BasicBlock, Block, CloneIn, Func, Function, Instruction, IrStr, Mint, Op, Program, Terminator, Type, Value};

use mcx::{CollectIn, GrowIn, Mcx, PgVec};

use crate::admit::refusal::check_interrupts;

/// What a parameter accounts for at one instruction of the first arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Moves {
    /// The address off the ctx: the arm's site slot.
    Slot,
    /// The constant: the arm's site index.
    Site,
    /// The address off a words pointer, `scale` bytes a word: the arm's word shift.
    Word(u8),
    /// D3659: the constant of this integer type: the arm's own value (one parameter per such instruction).
    Arm(Type),
}

/// Where an arm differs from its group's first: (block, instruction, what moves), in program order.
type Diff<'m> = PgVec<'m, (u32, u32, Moves)>;

/// An arm's parameters: its pipeline (program order), its site index, its word shift and its own constants (the bits at
/// each `Moves::Arm` instruction, in program order).
#[derive(Clone, Debug)]
struct Arm<'m> {
    k: usize,
    site: i64,
    shift: i64,
    consts: PgVec<'m, u64>,
}

struct Group<'m> {
    arms: PgVec<'m, Arm<'m>>,
    diff: Diff<'m>,
}

/// Every `Arms` run's twin arms (`runs`: `[first, end)` program pipelines; `sites`: each pipeline's driving site, -1
/// without one) folded into one function per group, appended to `ir`: returns each shared function with the bounded
/// loops (cancel.rs) of the arm it was made from. A twinned arm's own function, still its pipeline's entry, is a stub with
/// no loop, its `bounded` emptied.
pub(crate) fn fold<'m>(ir: &mut Program, fs: &[Func], bounded: &mut [PgVec<'m, Block>], sites: &[i16], runs: &[(usize, usize)], node: i32, mcx: Mcx<'m>) -> PgVec<'m, (Func, PgVec<'m, Block>)> {
    let mut out = PgVec::new_in(mcx);
    let mut done = mcx::pg_vec![in mcx; false; fs.len()];
    let m = ir.mint();
    for &(lo, hi) in runs {
        for g in groups(ir, fs, sites, &done, lo, hi.min(fs.len()), node, mcx) {
            let first = &g.arms[0];
            let t = &ir.funcs[fs[first.k].index()];
            let returns = mcx::to_vec_in(&t.returns[..], mcx);
            let shared = template(t, &g.diff, first.site, &m, mcx);
            let tys: PgVec<'_, Type> = g.diff.iter().filter_map(|d| match d.2 { Moves::Arm(ty) => Some(ty), _ => None }).collect_in(mcx);
            let tw = Func(ir.funcs.len() as u32);
            ir.funcs.push(shared);
            out.push_in((tw, core::mem::replace(&mut bounded[first.k], PgVec::new_in(mcx))));
            for a in g.arms.iter() {
                let f = &mut ir.funcs[fs[a.k].index()];
                *f = stub(core::mem::replace(&mut f.name, m.str("")), &returns, tw, a, &tys, &m);
                bounded[a.k].clear();
                done[a.k] = true;
            }
        }
    }
    out
}

/// The twin groups of run `[lo, hi)`, consecutive arms each.
fn groups<'m>(ir: &Program, fs: &[Func], sites: &[i16], done: &[bool], lo: usize, hi: usize, node: i32, mcx: Mcx<'m>) -> PgVec<'m, Group<'m>> {
    let mut out = PgVec::new_in(mcx);
    let mut i = lo;
    while i < hi {
        let t = &ir.funcs[fs[i].index()];
        if done[i] || sites[i] < 0 || t.params != [Type::Ptr] || t.returns.len() > 1 {
            i += 1;
            continue;
        }
        let words = words_of(t, mcx);
        let mut g = Group { arms: mcx::pg_vec![in mcx; Arm { k: i, site: sites[i] as i64, shift: 0, consts: PgVec::new_in(mcx) }], diff: PgVec::new_in(mcx) };
        let mut j = i + 1;
        while j < hi && !done[j] && sites[j] >= 0 {
            check_interrupts(node);
            let Some((d, shift, consts)) = diff(ir, t, &ir.funcs[fs[j].index()], sites[i] as i64, sites[j] as i64, &words, mcx) else { break };
            if g.arms.len() > 1 && d != g.diff {
                break;
            }
            g.diff = d;
            g.arms.push_in(Arm { k: j, site: sites[j] as i64, shift, consts });
            j += 1;
        }
        if g.arms.len() > 1 {
            // the first arm's own constants, at the instructions where the others differ from it
            let own = |&(b, i, m): &(u32, u32, Moves)| match (m, &t.blocks[b as usize].insts[i as usize].op) {
                (Moves::Arm(_), Op::Const { bits, .. }) => Some(*bits),
                _ => None,
            };
            g.arms[0].consts = g.diff.iter().filter_map(own).collect_in(mcx);
            out.push_in(g);
        }
        i = j;
    }
    out
}

/// The words pointers `t` loads off its ctx: (value, bytes a word): `PARAMS` 8, `PARAMS_NULL` 1.
fn words_of<'m>(t: &Function, mcx: Mcx<'m>) -> PgVec<'m, (Value, u8)> {
    let Some(&ctx) = t.blocks.get(t.entry.index()).and_then(|b| b.params.first()) else { return PgVec::new_in(mcx) };
    let scale = |o: i32| [(hdr::PARAMS, 8u8), (hdr::PARAMS_NULL, 1u8)].into_iter().find(|w| w.0 as i32 == o).map(|w| w.1);
    let all = t.blocks.iter().flat_map(|b| b.insts.iter());
    all.filter_map(|i| match (i.result, &i.op) {
        (Some(r), Op::Load { ty: Type::Ptr, ptr, offset, .. }) if *ptr == ctx => scale(*offset).map(|s| (r, s)),
        _ => None,
    })
    .collect_in(mcx)
}

/// Where `a` (an arm on site `sa`) differs from `t` (its group's first, on site `st`), `a`'s word shift and its own
/// constants (`Moves::Arm`); `None` when a difference is not one a parameter accounts for exactly (the module doc).
fn diff<'m>(ir: &Program, t: &Function, a: &Function, st: i64, sa: i64, words: &[(Value, u8)], mcx: Mcx<'m>) -> Option<(Diff<'m>, i64, PgVec<'m, u64>)> {
    let same = t.params == a.params && t.returns == a.returns && t.values == a.values && t.stack_slots == a.stack_slots && t.entry == a.entry && t.blocks.len() == a.blocks.len();
    // block frequencies and branch likelihoods are layout hints only (gravity_ir `Hints`): the first arm's serve the group
    let (ht, ha) = (&t.hints, &a.hints);
    if !same || ht.cold_blocks != ha.cold_blocks || ht.loop_headers != ha.loop_headers || ht.remat != ha.remat || ht.nowrap != ha.nowrap {
        return None;
    }
    let ctx = *t.blocks.get(t.entry.index())?.params.first()?;
    let slot = site_off(st as usize) as i64..(site_off(st as usize) + SITE_SIZE) as i64;
    let delta = (sa - st) * SITE_SIZE as i64;
    let (mut out, mut shift, mut consts) = (PgVec::new_in(mcx), None, PgVec::new_in(mcx));
    for (b, (bt, ba)) in t.blocks.iter().zip(a.blocks.iter()).enumerate() {
        if bt.params != ba.params || bt.term != ba.term || bt.insts.len() != ba.insts.len() {
            return None;
        }
        for (i, (it, ia)) in bt.insts.iter().zip(ba.insts.iter()).enumerate() {
            if it == ia {
                continue;
            }
            if it.result != ia.result {
                return None;
            }
            let m = match (&it.op, &ia.op) {
                (Op::Const { ty: Type::I64, bits: x }, Op::Const { ty: Type::I64, bits: y }) if *x as i64 == st && *y as i64 == sa => Moves::Site,
                (Op::Const { ty: x, .. }, Op::Const { ty: y, bits }) if x == y && matches!(x, Type::Bool | Type::I8 | Type::I16 | Type::I32 | Type::I64) => {
                    consts.push_in(*bits);
                    Moves::Arm(*x)
                }
                (Op::CallHost { callee: x, args: p }, Op::CallHost { callee: y, args: q }) if p == q && ir.host_fn(*x) == ir.host_fn(*y) => continue,
                (x, y) => {
                    let ((pt, ot), (pa, oa)) = (addr(x)?, addr(y)?);
                    if pt != pa || !same_but_offset(x, y) {
                        return None;
                    }
                    if pt == ctx {
                        if oa - ot != delta || !slot.contains(&ot) {
                            return None;
                        }
                        Moves::Slot
                    } else {
                        let &(_, scale) = words.iter().find(|w| w.0 == pt)?;
                        let d = oa - ot;
                        if d % scale as i64 != 0 || *shift.get_or_insert(d / scale as i64) != d / scale as i64 {
                            return None;
                        }
                        Moves::Word(scale)
                    }
                }
            };
            out.push_in((b as u32, i as u32, m));
        }
    }
    Some((out, shift.unwrap_or(0), consts))
}

/// The address an instruction reads, writes or forms: (pointer, offset).
fn addr(op: &Op) -> Option<(Value, i64)> {
    match *op {
        Op::Load { ptr, offset, .. } | Op::LoadZext { ptr, offset, .. } | Op::Store { ptr, offset, .. } | Op::VecLoad { ptr, offset, .. } | Op::VecStore { ptr, offset, .. } | Op::Prefetch { ptr, offset } => Some((ptr, offset as i64)),
        Op::PtrAdd { base, offset, .. } => Some((base, offset as i64)),
        _ => None,
    }
}

fn set_addr(op: &mut Op, to: Value, off: i64) {
    match op {
        Op::Load { ptr, offset, .. } | Op::LoadZext { ptr, offset, .. } | Op::Store { ptr, offset, .. } | Op::VecLoad { ptr, offset, .. } | Op::VecStore { ptr, offset, .. } | Op::Prefetch { ptr, offset } | Op::PtrAdd { base: ptr, offset, .. } => {
            (*ptr, *offset) = (to, off as i32);
        }
        _ => unreachable!("an address the twin pass found"),
    }
}

/// Whether `x` and `y` are one instruction but for their offsets.
fn same_but_offset(x: &Op, y: &Op) -> bool {
    let (Some((p, _)), Some((_, o))) = (addr(y), addr(x)) else { return false };
    let mut y = y.copy_flat().expect("an address op is flat");
    set_addr(&mut y, p, o);
    *x == y
}

/// The group's shared function: the first arm's (`t`, on site `st`) with the site slot (`Ptr`), site index (`I64`), word
/// shift (`I64`) and then each arm-own constant (`Moves::Arm`, in program order) as parameters after the ctx, at every
/// instruction `diff` names.
fn template(t: &Function, diff: &Diff<'_>, st: i64, m: &Mint, mcx: Mcx<'_>) -> Function {
    let mut f = t.clone_in(m);
    f.name = m.str(&mcx::pg_format!(mcx, "{}_twins", t.name));
    let param = |f: &mut Function, ty: Type| {
        let v = Value(f.values.len() as u32);
        f.values.push(ty);
        f.params.push(ty);
        let e = f.entry.index();
        f.blocks[e].params = m.appended(&f.blocks[e].params, v);
        v
    };
    let (slot, site, shift) = (param(&mut f, Type::Ptr), param(&mut f, Type::I64), param(&mut f, Type::I64));
    let base = site_off(st as usize) as i64;
    let (mut consts, mut moved): (PgVec<'_, Value>, PgVec<'_, (Value, Value, u8)>) = (PgVec::new_in(mcx), PgVec::new_in(mcx));
    let mut own: PgVec<'_, (Value, Value)> = PgVec::new_in(mcx);
    for &(b, i, m) in diff.iter() {
        let (p, o) = addr(&f.blocks[b as usize].insts[i as usize].op).unwrap_or((Value(0), 0));
        let to = match m {
            Moves::Site => {
                consts.extend_in(f.blocks[b as usize].insts[i as usize].result);
                continue;
            }
            Moves::Arm(ty) => {
                let v = param(&mut f, ty);
                own.extend_in(f.blocks[b as usize].insts[i as usize].result.map(|r| (r, v)));
                continue;
            }
            Moves::Slot => (slot, o - base),
            Moves::Word(scale) => match moved.iter().find(|w| w.0 == p) {
                Some(w) => (w.1, o),
                None => {
                    let v = Value(f.values.len() as u32);
                    f.values.push(Type::Ptr);
                    moved.push_in((p, v, scale));
                    (v, o)
                }
            },
        };
        set_addr(&mut f.blocks[b as usize].insts[i as usize].op, to.0, to.1);
    }
    // each words pointer's shifted copy right after the pointer's load (the positions above are all rewritten first)
    for &(p, v, scale) in moved.iter() {
        let at = f.blocks.iter().enumerate().find_map(|(b, bb)| bb.insts.iter().position(|i| i.result == Some(p)).map(|i| (b, i)));
        let (b, i) = at.expect("a words pointer the twin pass found");
        f.blocks[b].insts.insert(i + 1, Instruction { result: Some(v), op: Op::PtrAdd { base: p, index: shift, scale, offset: 0 } });
    }
    // the site constants' and the arm-own constants' uses read their parameters (the constants stay, unused)
    if !consts.is_empty() || !own.is_empty() {
        let mut swap = |v: &mut Value| {
            if consts.contains(v) {
                *v = site;
            } else if let Some(&(_, p)) = own.iter().find(|c| c.0 == *v) {
                *v = p;
            }
        };
        for bb in f.blocks.iter_mut() {
            bb.insts.iter_mut().for_each(|i| operands_mut(&mut i.op, &mut swap));
            terminator_mut(&mut bb.term, &mut swap);
        }
    }
    f
}

/// A twinned arm's entry, `name` kept: the shared function `tw` called with the arm's site slot, site index, word shift
/// and its own constants (`tys`: their types, in the shared function's parameter order).
fn stub(name: IrStr, returns: &[Type], tw: Func, a: &Arm<'_>, tys: &[Type], m: &Mint) -> Function {
    let (ctx, zero, slot, site, shift) = (Value(0), Value(1), Value(2), Value(3), Value(4));
    let konst = |v: Value, ty: Type, bits: u64| Instruction { result: Some(v), op: Op::Const { ty, bits } };
    let mut insts = m.collect([
        konst(zero, Type::I64, 0),
        Instruction { result: Some(slot), op: Op::PtrAdd { base: ctx, index: zero, scale: 1, offset: site_off(a.site as usize) as i32 } },
        konst(site, Type::I64, a.site as u64),
        konst(shift, Type::I64, a.shift as u64),
    ]);
    let mut values = m.vec_from(&[Type::Ptr, Type::I64, Type::Ptr, Type::I64, Type::I64]);
    let mut args = m.vec_from(&[ctx, slot, site, shift]);
    for (&ty, &bits) in tys.iter().zip(a.consts.iter()) {
        let v = Value(values.len() as u32);
        values.push(ty);
        insts.push(konst(v, ty, bits));
        args.push(v);
    }
    let r = Value(values.len() as u32);
    insts.push(Instruction { result: returns.first().map(|_| r), op: Op::Call { callee: tw, args: gravity_ir::IrSlice::from_vec(args) } });
    values.extend(returns.first());
    let term = Terminator::Return(m.slice_collect(returns.first().map(|_| r)));
    let entry = BasicBlock { params: m.slice_from(&[ctx]), insts, term };
    let (params, returns) = (m.vec_from(&[Type::Ptr]), m.vec_from(returns));
    Function { name, params, returns, values, stack_slots: m.vec(), blocks: m.collect([entry]), entry: Block(0), hints: m.hints() }
}

/// Every value `op` reads, in place.
fn operands_mut(op: &mut Op, f: &mut dyn FnMut(&mut Value)) {
    match op {
        Op::Const { .. } | Op::StackAddr { .. } => {}
        Op::IntBin { a, b, .. } | Op::IntOvf { a, b, .. } | Op::Icmp { a, b, .. } | Op::FloatBin { a, b, .. } | Op::Fcmp { a, b, .. } | Op::VecBin { a, b, .. } | Op::VecCmp { a, b, .. } | Op::VecNarrow { a, b } => {
            f(a);
            f(b);
        }
        Op::IntUn { a, .. } | Op::FloatUn { a, .. } | Op::Conv { a, .. } | Op::VecShift { a, .. } | Op::VecReduceAdd { a } | Op::VecMovmask { a } | Op::VecSplat { a, .. } | Op::VecExtract { a, .. } | Op::VecWiden { a, .. } => f(a),
        Op::Select { cond, a, b } | Op::VecSelect { mask: cond, a, b } => {
            f(cond);
            f(a);
            f(b);
        }
        Op::Load { ptr, .. } | Op::LoadZext { ptr, .. } | Op::VecLoad { ptr, .. } | Op::Prefetch { ptr, .. } => f(ptr),
        Op::Store { ptr, value, .. } | Op::VecStore { ptr, value, .. } => {
            f(ptr);
            f(value);
        }
        Op::PtrAdd { base, index, .. } => {
            f(base);
            f(index);
        }
        Op::Crc32c { acc, data } => {
            f(acc);
            f(data);
        }
        Op::AddHi { alo, blo, ahi, bhi } => {
            f(alo);
            f(blo);
            f(ahi);
            f(bhi);
        }
        Op::Call { args, .. } | Op::CallHost { args, .. } => args.iter_mut().for_each(f),
        Op::VecInsert { a, value, .. } => {
            f(a);
            f(value);
        }
    }
}

/// Every value a terminator reads (its edges' arguments too), in place.
fn terminator_mut(t: &mut Terminator, f: &mut dyn FnMut(&mut Value)) {
    match t {
        Terminator::Br(e) => e.args.iter_mut().for_each(f),
        Terminator::CondBr { cond, then_edge, else_edge } => {
            f(cond);
            then_edge.args.iter_mut().chain(else_edge.args.iter_mut()).for_each(f);
        }
        Terminator::Switch { value, cases, default } => {
            f(value);
            cases.iter_mut().flat_map(|c| c.edge.args.iter_mut()).chain(default.args.iter_mut()).for_each(f);
        }
        Terminator::Return(vs) => vs.iter_mut().for_each(f),
        Terminator::Unreachable => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_ir::builder::ProgramBuilder;
    use gravity_ir::{HostFlags, IntBinOp};

    extern "C" fn plus(a: i64, b: i64) -> i64 {
        a.wrapping_mul(1000).wrapping_add(b)
    }

    /// `n` arm functions like a heap arm's: arm `i` on site `sites[i]`, its literal at word `2 + 3 * i` (and its null
    /// flag), a site-slot word read and written, the site index passed to a host declared per arm; `odd` makes arm 1
    /// subtract its constant 7 (an operation no parameter accounts for); `own` gives each arm its own constants, as a
    /// write arm bakes its target relation (D3659): `7 + i`, and an i32 `16387 + 3 * i` stored at one ctx offset.
    fn arms(sites: &[i64], odd: bool, own: bool) -> (Program, Vec<Func>) {
        let mut pb = ProgramBuilder::new();
        let fs: Vec<Func> = (0..sites.len()).map(|i| pb.declare_func(&format!("p{i}"), &[Type::Ptr], &[Type::I64])).collect(); // alloc: Program
        for (i, &s) in sites.iter().enumerate() {
            let h = pb.declare_host("plus", &[Type::I64, Type::I64], Some(Type::I64), plus as *const () as usize, HostFlags::default());
            let mut b = pb.define(fs[i]);
            let ctx = b.params()[0];
            let words = b.load(Type::Ptr, ctx, hdr::PARAMS as i32);
            let nulls = b.load(Type::Ptr, ctx, hdr::PARAMS_NULL as i32);
            let w = 2 + 3 * i as i32;
            let lit = b.load(Type::I64, words, 8 * w);
            let null = b.load_zext(Type::I8, nulls, w);
            let at = site_off(s as usize) as i32 + 16;
            let cur = b.load(Type::I64, ctx, at);
            let seven = b.iconst(Type::I64, if own { 7 + i as i64 } else { 7 });
            let next = b.int_bin(IntBinOp::Add, cur, lit);
            let next = b.int_bin(if odd && i == 1 { IntBinOp::Sub } else { IntBinOp::Add }, next, seven);
            b.store(ctx, at, next);
            if own {
                let rel = b.iconst(Type::I32, 16387 + 3 * i as i64);
                b.store(ctx, site_off(3) as i32 + 40, rel);
            }
            let site = b.iconst(Type::I64, s);
            let r = b.call_host(h, &[site, null], Some(Type::I64)).unwrap();
            let r = b.int_bin(IntBinOp::Add, r, next);
            b.ret(&[r]);
        }
        (pb.finish(), fs)
    }

    /// Each entry run on a fresh ctx (three words a site past site 3, words and null flags set per word): its result and
    /// the ctx after.
    fn run(ir: &Program, fs: &[Func]) -> Vec<(u64, Vec<u64>)> {
        let words: Vec<u64> = (0..32).map(|w| 100 + w).collect(); // alloc: Program
        let nulls: Vec<u8> = (0..32).map(|w| (w % 2) as u8).collect(); // alloc: Program
        fs.iter()
            .map(|&f| {
                let mut ctx = vec![0u64; site_off(4) / 8]; // alloc: Program
                ctx[hdr::PARAMS / 8] = words.as_ptr() as u64;
                ctx[hdr::PARAMS_NULL / 8] = nulls.as_ptr() as u64;
                (0..4).for_each(|s| ctx[(site_off(s) + 16) / 8] = 10 * s as u64);
                let r = gravity_interp::Interp::new(ir).run(f, &[ctx.as_mut_ptr() as u64]).expect("runs");
                ctx[hdr::PARAMS / 8] = 0;
                ctx[hdr::PARAMS_NULL / 8] = 0;
                (r[0], ctx)
            })
            .collect()
    }

    #[test]
    fn twin_arms_share_one_function_and_compute_what_each_did() {
        let sites = [0i64, 1, 3];
        let (mut ir, fs) = arms(&sites, false, false);
        let before = run(&ir, &fs);
        let rig = mcx::MemoryContext::new("test");
        let mut bounded: [PgVec<'_, Block>; 3] = core::array::from_fn(|_| PgVec::new_in(rig.mcx()));
        let twins = fold(&mut ir, &fs, &mut bounded, &[0, 1, 3], &[(0, 3)], 0, rig.mcx());
        assert_eq!(twins.len(), 1, "one group");
        assert_eq!(ir.funcs.len(), 4, "the shared function added");
        assert!(fs.iter().all(|f| ir.func(*f).blocks.len() == 1), "every arm a stub");
        assert!(gravity_ir::verify::verify(&ir).is_ok());
        assert_eq!(run(&ir, &fs), before);
    }

    #[test]
    fn a_difference_no_parameter_accounts_for_keeps_the_arms_apart() {
        let (mut ir, fs) = arms(&[0, 1, 2], true, false);
        let (was, before) = (ir.clone(), run(&ir, &fs));
        let m = mcx::MemoryContext::new("test");
        let twins = fold(&mut ir, &fs, &mut [PgVec::new_in(m.mcx()), PgVec::new_in(m.mcx()), PgVec::new_in(m.mcx())], &[0, 1, 2], &[(0, 3)], 0, m.mcx());
        assert!(twins.is_empty(), "arm 1 differs in an operation, arm 2 is alone");
        assert_eq!(ir, was);
        assert_eq!(run(&ir, &fs), before);
    }

    #[test]
    fn functions_outside_a_run_keep_their_ir() {
        let (mut ir, fs) = arms(&[0, 1, 2], false, false);
        let was = ir.clone();
        let m = mcx::MemoryContext::new("test");
        let twins = fold(&mut ir, &fs, &mut [PgVec::new_in(m.mcx()), PgVec::new_in(m.mcx()), PgVec::new_in(m.mcx())], &[0, 1, 2], &[(0, 2)], 0, m.mcx());
        assert_eq!(twins.len(), 1);
        assert_eq!(ir.funcs[fs[2].index()], was.funcs[fs[2].index()], "p2 is outside the run");
        assert!(fold(&mut was.clone(), &fs, &mut [PgVec::new_in(m.mcx()), PgVec::new_in(m.mcx()), PgVec::new_in(m.mcx())], &[0, 1, 2], &[], 0, m.mcx()).is_empty());
    }

    /// D3659: arms that differ only in their own constants (a write arm's target relation) share one function too, each
    /// stub passing its values, and each arm computes what it did.
    #[test]
    fn arm_own_constants_are_parameters_of_the_shared_function() {
        let sites = [0i64, 1, 3];
        let (mut ir, fs) = arms(&sites, false, true);
        let before = run(&ir, &fs);
        let m = mcx::MemoryContext::new("test");
        let twins = fold(&mut ir, &fs, &mut [PgVec::new_in(m.mcx()), PgVec::new_in(m.mcx()), PgVec::new_in(m.mcx())], &sites.map(|s| s as i16), &[(0, 3)], 0, m.mcx());
        assert_eq!(twins.len(), 1, "one group");
        let shared = &ir.funcs[twins[0].0.index()];
        assert_eq!(shared.params, [Type::Ptr, Type::Ptr, Type::I64, Type::I64, Type::I64, Type::I32], "slot, site, shift, then the two own constants");
        assert!(fs.iter().all(|f| ir.func(*f).blocks.len() == 1), "every arm a stub");
        assert!(gravity_ir::verify::verify(&ir).is_ok());
        assert_eq!(run(&ir, &fs), before);
    }
}
