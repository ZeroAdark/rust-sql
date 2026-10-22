//! Randomized phi programs under register pressure, native vs interpreter
//! (`tpde-defects`: the three shapes of `tpde-phi-assert-handoff.md` —
//! diamonds whose join carries phis fed by a value on one edge and a
//! constant or a pre-branch value on the other, loop headers with many
//! loop-carried `i64` / `i32` / `i8` / `bool` phis, host calls inside the
//! arms so the values live across them want callee-saved fixed registers,
//! nested loops inside arms and diamonds inside loops, frequency hints
//! that move the weighted allocator's nominations). Every program is
//! built through the typed builder, accepted by the full IR verifier
//! (structure, types, dominance), run through the interpreter and the
//! native code for several trip counts, and the results must agree; a
//! compile refusal (an eviction or spill inside a branch region is an
//! error in every profile) is a failure too.
//!
//! `GRAVITY_PHI_FUZZ_N` programs (default 10 000) from seed
//! `GRAVITY_PHI_FUZZ_FROM` (default 0), `GRAVITY_PHI_FUZZ_SEED=<n>`
//! replays one program and prints its text; a failure prints the seed and
//! the program, and with `GRAVITY_PHI_FUZZ_KEEP_GOING=1` the run continues
//! and lists every failing seed at the end. `GRAVITY_PHI_FUZZ_CODE_HASH=<file>`
//! also writes one `seed hash` line per program over the generated code
//! (`bl` immediates masked: the host's address differs per process), so
//! a change meant to leave the generated code alone is diffed over the
//! whole corpus.

mod common;
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::text::print_program;
use gravity_ir::*;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};

// ---------------------------------------------------------------- randomness

struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    /// xorshift64*.
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    /// Inclusive.
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi - lo + 1)
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

// ---------------------------------------------------------------- the host

/// A host that writes memory (the generated code reloads `q` after it):
/// every value live across the call must survive in a callee-saved
/// register or a slot.
extern "C" fn host_touch(q: *mut i64) {
    unsafe { *q = (*q).wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407) }
}

// ---------------------------------------------------------------- generation

#[derive(Clone, Copy)]
struct V {
    v: Value,
    ty: Type,
}

struct Ctx {
    p: Value,
    q: Value,
    touch: HostFn,
    ninst: usize,
}

const MAX_INSTS: usize = 400;

fn random_type(rng: &mut Rng) -> Type {
    match rng.below(100) {
        0..=49 => Type::I64,
        50..=64 => Type::I32,
        65..=89 => Type::I8,
        _ => Type::Bool,
    }
}

fn const_of(fb: &mut FunctionBuilder<'_>, rng: &mut Rng, ty: Type) -> Value {
    match ty {
        Type::Bool => fb.bconst(rng.chance(50)),
        _ => fb.iconst(ty, rng.range(0, 40) as i64 - 20),
    }
}

/// A live value of `ty`, or a fresh constant when none is live.
fn pick(fb: &mut FunctionBuilder<'_>, rng: &mut Rng, live: &[V], ty: Type) -> Value {
    let cands: Vec<Value> = live.iter().filter(|x| x.ty == ty).map(|x| x.v).collect();
    if cands.is_empty() || rng.chance(5) {
        const_of(fb, rng, ty)
    } else {
        cands[rng.below(cands.len() as u64) as usize]
    }
}

fn is_int(ty: Type) -> bool {
    matches!(ty, Type::I64 | Type::I32 | Type::I8)
}

fn widen(fb: &mut FunctionBuilder<'_>, v: V) -> Value {
    match v.ty {
        Type::I64 => v.v,
        _ => fb.conv(ConvOp::Sext, Type::I64, v.v),
    }
}

/// Straight-line code: `count` random instructions over the live values,
/// each pushed onto `live`.
fn emit_ops(fb: &mut FunctionBuilder<'_>, rng: &mut Rng, cx: &mut Ctx, live: &mut Vec<V>, count: u64) {
    for _ in 0..count {
        if cx.ninst >= MAX_INSTS {
            return;
        }
        cx.ninst += 1;
        let ty = random_type(rng);
        let (v, ty) = match rng.below(12) {
            0..=4 if is_int(ty) => {
                let a = pick(fb, rng, live, ty);
                let b = pick(fb, rng, live, ty);
                let op = [IntBinOp::Add, IntBinOp::Sub, IntBinOp::Mul, IntBinOp::And, IntBinOp::Or, IntBinOp::Xor][rng.below(6) as usize];
                (fb.int_bin(op, a, b), ty)
            }
            5 if is_int(ty) => {
                let a = pick(fb, rng, live, ty);
                let amt = fb.iconst(ty, rng.below(ty.bytes() as u64 * 8) as i64);
                let op = [IntBinOp::Shl, IntBinOp::Ushr, IntBinOp::Sshr][rng.below(3) as usize];
                (fb.int_bin(op, a, amt), ty)
            }
            6 => {
                // A compare into a select of two live values of a random type.
                let cty = if is_int(ty) { ty } else { Type::I64 };
                let a = pick(fb, rng, live, cty);
                let b = pick(fb, rng, live, cty);
                let cond = [IntCond::Eq, IntCond::Ne, IntCond::Slt, IntCond::Ult, IntCond::Sge][rng.below(5) as usize];
                let c = fb.icmp(cond, a, b);
                if rng.chance(30) {
                    (c, Type::Bool)
                } else {
                    let sty = random_type(rng);
                    let x = pick(fb, rng, live, sty);
                    let y = pick(fb, rng, live, sty);
                    (fb.select(c, x, y), sty)
                }
            }
            7 => {
                // A conversion between two widths (or a bool).
                let from = random_type(rng);
                let a = pick(fb, rng, live, from);
                let to = random_type(rng);
                if from == to {
                    (a, to)
                } else if to == Type::Bool {
                    let z = fb.iconst(from, 0);
                    (fb.icmp(IntCond::Ne, a, z), Type::Bool)
                } else if from == Type::Bool || from.bytes() < to.bytes() {
                    let op = if rng.chance(50) { ConvOp::Sext } else { ConvOp::Zext };
                    (fb.conv(op, to, a), to)
                } else {
                    (fb.conv(ConvOp::Trunc, to, a), to)
                }
            }
            8 => {
                // A read of the input array (read-only).
                let off = 8 * rng.below(64) as i32;
                if rng.chance(50) {
                    (fb.load(Type::I64, cx.p, off), Type::I64)
                } else {
                    let ty = [Type::I8, Type::I32][rng.below(2) as usize];
                    (fb.load(ty, cx.p, off), ty)
                }
            }
            9 if rng.chance(60) => {
                // The host writes `q`; its new word is a value.
                fb.call_host(cx.touch, &[cx.q], None);
                (fb.load(Type::I64, cx.q, 0), Type::I64)
            }
            _ => {
                let a = pick(fb, rng, live, ty);
                if is_int(ty) {
                    (fb.int_un(IntUnOp::Neg, a), ty)
                } else {
                    (a, ty)
                }
            }
        };
        live.push(V { v, ty });
    }
}

/// A region: straight-line pieces, diamonds and loops in sequence. Returns
/// the values live at its end (every value it was given plus the ones the
/// region defines on every path to its end — dominance by construction).
fn region(fb: &mut FunctionBuilder<'_>, rng: &mut Rng, cx: &mut Ctx, mut live: Vec<V>, depth: u32) -> Vec<V> {
    let pieces = rng.range(1, 3);
    for _ in 0..pieces {
        if cx.ninst >= MAX_INSTS {
            break;
        }
        let kind = if depth >= 3 { 0 } else { rng.below(10) };
        match kind {
            0..=4 => {
                let n = rng.range(1, 5);
                emit_ops(fb, rng, cx, &mut live, n);
            }
            5..=7 => live = diamond(fb, rng, cx, live, depth),
            _ if depth < 2 => live = inner_loop(fb, rng, cx, live, depth),
            _ => live = diamond(fb, rng, cx, live, depth),
        }
    }
    live
}

/// `condbr c (then) (else)` into a join with `J` parameters. An arm is a
/// block (its own region) or the direct edge from the branching block
/// (the join's phi fed from the pre-branch values: `keyfold.rs`'s word
/// probe, `condbr go (ld) (join 0)`); both edges may even name the join.
fn diamond(fb: &mut FunctionBuilder<'_>, rng: &mut Rng, cx: &mut Ctx, live: Vec<V>, depth: u32) -> Vec<V> {
    let cty = [Type::I64, Type::I32, Type::I8][rng.below(3) as usize];
    let a = pick(fb, rng, &live, cty);
    let b = pick(fb, rng, &live, cty);
    let cond = [IntCond::Eq, IntCond::Ne, IntCond::Slt, IntCond::Ult, IntCond::Sgt][rng.below(5) as usize];
    let c = fb.icmp(cond, a, b);
    let nparams = rng.range(0, 6) as usize;
    let ptys: Vec<Type> = (0..nparams).map(|_| random_type(rng)).collect();
    let join = fb.create_block(&ptys);
    let here = fb.current();
    // 0: two arm blocks; 1: then direct; 2: else direct; 3: both direct.
    let shape = match rng.below(20) {
        0..=11 => 0,
        12..=15 => 1,
        16..=18 => 2,
        _ => 3,
    };
    let direct_args = |fb: &mut FunctionBuilder<'_>, rng: &mut Rng, live: &[V]| -> Vec<Value> { ptys.iter().map(|&t| pick(fb, rng, live, t)).collect() };
    let then_b = if shape == 1 || shape == 3 { None } else { Some(fb.create_block(&[])) };
    let else_b = if shape == 2 || shape == 3 { None } else { Some(fb.create_block(&[])) };
    let then_args = if then_b.is_none() { direct_args(fb, rng, &live) } else { vec![] };
    let else_args = if else_b.is_none() { direct_args(fb, rng, &live) } else { vec![] };
    fb.cond_br(c, (then_b.unwrap_or(join), &then_args), (else_b.unwrap_or(join), &else_args));
    if rng.chance(40) {
        fb.set_likely(here, rng.chance(50));
    }
    for arm in [then_b, else_b].into_iter().flatten() {
        fb.switch_to(arm);
        match rng.below(10) {
            0 => fb.mark_cold(arm),
            1 => fb.set_freq(arm, 0.01),
            2 => fb.set_freq(arm, 50.0),
            _ => {}
        }
        let arm_live = region(fb, rng, cx, live.clone(), depth + 1);
        let args: Vec<Value> = ptys.iter().map(|&t| pick(fb, rng, &arm_live, t)).collect();
        fb.br(join, &args);
    }
    fb.switch_to(join);
    let mut out = live;
    out.extend(fb.block_params(join).iter().zip(&ptys).map(|(&v, &ty)| V { v, ty }));
    out
}

/// A counted inner loop with `C` loop-carried parameters besides its
/// counter; the body is a region; the back edge feeds each parameter a
/// random value of its type live at the body's end.
fn inner_loop(fb: &mut FunctionBuilder<'_>, rng: &mut Rng, cx: &mut Ctx, live: Vec<V>, depth: u32) -> Vec<V> {
    let ncarried = rng.range(1, 8) as usize;
    let mut ptys = vec![Type::I64];
    ptys.extend((0..ncarried).map(|_| random_type(rng)));
    let header = fb.create_block(&ptys);
    let z = fb.iconst(Type::I64, 0);
    let mut inits = vec![z];
    for &t in &ptys[1..] {
        inits.push(pick(fb, rng, &live, t));
    }
    fb.br(header, &inits);
    fb.switch_to(header);
    fb.mark_loop_header(header);
    let params: Vec<V> = fb.block_params(header).iter().zip(&ptys).map(|(&v, &ty)| V { v, ty }).collect();
    let j = params[0].v;
    let mut body_live = live;
    body_live.extend(params.iter().copied());
    let body_live = region(fb, rng, cx, body_live, depth + 1);
    let one = fb.iconst(Type::I64, 1);
    let j1 = fb.int_bin(IntBinOp::Add, j, one);
    let trip = fb.iconst(Type::I64, rng.range(1, 3) as i64);
    let more = fb.icmp(IntCond::Slt, j1, trip);
    let mut args = vec![j1];
    for &t in &ptys[1..] {
        args.push(pick(fb, rng, &body_live, t));
    }
    let exit = fb.create_block(&[]);
    fb.cond_br(more, (header, &args), (exit, &[]));
    fb.switch_to(exit);
    body_live
}

/// `fn f(p ptr, q ptr, n i64) -> i64`: `H` holders loaded from `p` in the
/// entry and folded into the accumulator at the end of every iteration
/// (live across the whole body and its host calls), a loop over `n` with
/// `M` loop-carried parameters, the body a region, the accumulator a
/// mix of every value live at the body's end.
fn generate(seed: u64) -> Program {
    let mut rng = Rng::new(seed);
    let mut pb = ProgramBuilder::new();
    let touch = pb.declare_host("touch", &[Type::Ptr], None, host_touch as extern "C" fn(*mut i64) as usize, HostFlags { touches_memory: true, ..Default::default() });
    let f = pb.declare_func("f", &[Type::Ptr, Type::Ptr, Type::I64], &[Type::I64]);
    let mut fb = pb.define(f);
    let [p, q, n]: [Value; 3] = fb.params().try_into().unwrap();
    let mut cx = Ctx { p, q, touch, ninst: 0 };
    let mut holders: Vec<V> = Vec::new();
    let nholders = rng.range(0, 24);
    for k in 0..nholders {
        let v = fb.load(Type::I64, p, 8 * k as i32);
        if rng.chance(25) {
            let ty = [Type::I8, Type::I32][rng.below(2) as usize];
            let t = fb.conv(ConvOp::Trunc, ty, v);
            holders.push(V { v: t, ty });
        } else {
            holders.push(V { v, ty: Type::I64 });
        }
    }
    let ncarried = rng.range(0, 18) as usize;
    let mut ptys = vec![Type::I64, Type::I64];
    ptys.extend((0..ncarried).map(|_| random_type(&mut rng)));
    let loop_b = fb.create_block(&ptys);
    let exit = fb.create_block(&[]);
    let z = fb.iconst(Type::I64, 0);
    let seed_c = fb.iconst(Type::I64, (seed % 1000) as i64 + 1);
    let mut inits = vec![z, seed_c];
    let mut entry_live = holders.clone();
    entry_live.push(V { v: n, ty: Type::I64 });
    for &t in &ptys[2..] {
        inits.push(pick(&mut fb, &mut rng, &entry_live, t));
    }
    fb.br(loop_b, &inits);
    fb.switch_to(loop_b);
    fb.mark_loop_header(loop_b);
    let params: Vec<V> = fb.block_params(loop_b).iter().zip(&ptys).map(|(&v, &ty)| V { v, ty }).collect();
    let i = params[0].v;
    let acc = params[1].v;
    let mut live = entry_live;
    live.extend(params.iter().copied());
    let body_live = region(&mut fb, &mut rng, &mut cx, live, 0);
    // The fold: every holder, then a sample of what the body defined.
    let k = fb.iconst(Type::I64, 0x9E37_79B9);
    let mut a = fb.int_bin(IntBinOp::Mul, acc, k);
    for h in &holders {
        let w = widen(&mut fb, *h);
        a = fb.int_bin(IntBinOp::Add, a, w);
    }
    let sample = rng.range(1, 12) as usize;
    for _ in 0..sample {
        let v = body_live[rng.below(body_live.len() as u64) as usize];
        let w = widen(&mut fb, v);
        let m = fb.int_bin(IntBinOp::Mul, a, k);
        a = fb.int_bin(IntBinOp::Xor, m, w);
    }
    let one = fb.iconst(Type::I64, 1);
    let i1 = fb.int_bin(IntBinOp::Add, i, one);
    let more = fb.icmp(IntCond::Slt, i1, n);
    let mut args = vec![i1, a];
    for &t in &ptys[2..] {
        args.push(pick(&mut fb, &mut rng, &body_live, t));
    }
    fb.cond_br(more, (loop_b, &args), (exit, &[]));
    fb.switch_to(exit);
    fb.ret(&[a]);
    pb.finish()
}

// ---------------------------------------------------------------- the oracle

/// The corpus's regalloc events, so the run proves the pressure it claims
/// (spills, phi moves, stack phis, demotions actually happened) rather
/// than agreeing on programs the allocator never strained under.
#[derive(Default, Debug)]
struct Stats {
    programs: u64,
    spills: u64,
    phi_moves: u64,
    phi_stores: u64,
    demotes: u64,
    regions: u64,
    blocks: u64,
    insts: u64,
    params: u64,
}

fn tally(p: &Program, st: &mut Stats) {
    use tpde_compiler::RegallocEvent as E;
    let ctx = mcx::MemoryContext::new("phi_fuzz");
    let c = gravity_tpde::compile_recording(ctx.mcx(), p).unwrap_or_else(|e| panic!("compile: {e:?}"));
    for e in c.regalloc() {
        match e.event {
            E::Spill => st.spills += 1,
            E::PhiMove | E::PhiReload => st.phi_moves += 1,
            E::PhiStore => st.phi_stores += 1,
            E::Demote => st.demotes += 1,
            E::RegionBegin => st.regions += 1,
            _ => {}
        }
    }
    st.programs += 1;
    st.blocks += p.funcs[0].blocks.len() as u64;
    st.insts += p.funcs[0].blocks.iter().map(|b| b.insts.len() as u64).sum::<u64>();
    st.params += p.funcs[0].blocks.iter().map(|b| b.params.len() as u64).sum::<u64>();
}

/// FNV-1a over the code words of `p`'s mapping, `bl` immediates masked.
fn code_hash(p: &Program) -> u64 {
    let ctx = mcx::MemoryContext::new("phi_fuzz");
    let c = gravity_tpde::compile_recording(ctx.mcx(), p).unwrap_or_else(|e| panic!("compile: {e:?}"));
    let range = c.mapper().get_mapped_range().unwrap();
    // SAFETY: the mapping is live for `c`'s lifetime.
    let bytes = unsafe { range.bytes() };
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for w in bytes.chunks_exact(4) {
        let mut x = u32::from_le_bytes(w.try_into().unwrap());
        if x >> 26 == 0b100101 {
            x &= 0xfc00_0000;
        }
        h = (h ^ x as u64).wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn check(seed: u64) {
    let p = generate(seed);
    // The generator's own contract: a well-typed SSA program.
    if let Err(e) = gravity_ir::verify::verify(&p).and_then(|()| gravity_ir::verify::verify_dominance(&p)) {
        panic!("seed {seed}: the generated program fails verification: {} {}\n{}", e.func, e.message, print_program(&p));
    }
    let cells: Vec<i64> = (0..64).map(|k| 0x0102_0304_0506_0708u64.wrapping_mul(k as u64 + 1).rotate_left(k as u32 * 3) as i64).collect();
    for n in [0i64, 1, 2, 5] {
        let mut q_i = 7i64;
        let interp = gravity_interp::Interp::new(&p).run(Func(0), &[cells.as_ptr() as u64, &mut q_i as *mut i64 as u64, n as u64]).unwrap_or_else(|t| panic!("seed {seed} n {n}: interpreter trap {t:?}\n{}", print_program(&p)));
        let mut q_n = 7i64;
        let native = common::native(&p, Func(0), &[cells.as_ptr() as u64, &mut q_n as *mut i64 as u64, n as u64]);
        assert_eq!(native, interp, "seed {seed} n {n}: native vs interpreter");
        assert_eq!(q_n, q_i, "seed {seed} n {n}: the host's cell after the run");
    }
}

/// The seeds that failed once (backend.md §6.7): the scratch over a
/// spilled constant (16729, 1191), the anchor steal inside a loop
/// (17648), the fed register evicted before its definition (64142).
#[test]
fn known_seeds() {
    for seed in [16729u64, 1191, 17648, 64142] {
        check(seed);
    }
}

#[test]
fn random_phi_programs_agree_native_vs_interpreter() {
    if let Ok(s) = std::env::var("GRAVITY_PHI_FUZZ_SEED") {
        let seed: u64 = s.parse().unwrap();
        eprintln!("{}", print_program(&generate(seed)));
        check(seed);
        return;
    }
    let n: u64 = std::env::var("GRAVITY_PHI_FUZZ_N").ok().and_then(|s| s.parse().ok()).unwrap_or(10_000);
    let from: u64 = std::env::var("GRAVITY_PHI_FUZZ_FROM").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    let keep_going = std::env::var_os("GRAVITY_PHI_FUZZ_KEEP_GOING").is_some();
    let mut st = Stats::default();
    let mut failed: Vec<u64> = Vec::new();
    let mut hashes = std::env::var("GRAVITY_PHI_FUZZ_CODE_HASH").ok().map(|f| std::io::BufWriter::new(std::fs::File::create(f).unwrap()));
    for seed in from..from + n {
        if let Some(w) = hashes.as_mut() {
            use std::io::Write;
            writeln!(w, "{seed} {:016x}", code_hash(&generate(seed))).unwrap();
        }
        let r = catch_unwind(AssertUnwindSafe(|| check(seed)));
        if let Err(e) = r {
            eprintln!("--- phi_fuzz seed {seed} (replay: GRAVITY_PHI_FUZZ_SEED={seed}) ---\n{}", print_program(&generate(seed)));
            if !keep_going {
                resume_unwind(e);
            }
            failed.push(seed);
            continue;
        }
        tally(&generate(seed), &mut st);
        if seed % 2000 == 1999 {
            eprintln!("phi_fuzz: {} programs agree; {st:?}", seed + 1 - from);
        }
    }
    eprintln!("phi_fuzz: seeds {from}..{}: {} programs agree ({} on this target); {st:?}", from + n, st.programs, std::env::consts::ARCH);
    assert!(failed.is_empty(), "phi_fuzz: failing seeds {failed:?}");
    if n >= 1000 {
        assert!(st.spills > 0 && st.phi_moves > 0 && st.phi_stores > 0 && st.regions > 0, "the corpus never strained the allocator: {st:?}");
    }
}
