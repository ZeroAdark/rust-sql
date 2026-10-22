//! `gravity_tpde/tests/phi_fuzz.rs`'s random phi programs (the generator
//! below is that file's, copied verbatim so the seeds name the same
//! programs), wasm under wasmtime vs the interpreter, bit-identical
//! results and the host's cell after the run. `GRAVITY_PHI_FUZZ_N`
//! programs (default 1 000 here; the TPDE arm's 10 000 and the 100 k
//! runs are the env var) from `GRAVITY_PHI_FUZZ_FROM`;
//! `GRAVITY_PHI_FUZZ_SEED=<n>` replays one program.

mod common;
use common::*;
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

const CELLS: u32 = DATA;
const Q: u32 = DATA + 1024;

fn touch_guest() -> Vec<(&'static str, HostImpl)> {
    vec![(
        "touch",
        Box::new(|m: &mut [u8], a: &[u64]| {
            let q = a[0] as usize;
            let v = i64::from_le_bytes(m[q..q + 8].try_into().unwrap());
            m[q..q + 8].copy_from_slice(&v.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407).to_le_bytes());
            0
        }),
    )]
}

#[derive(Default, Debug)]
struct Stats {
    programs: u64,
    blocks: u64,
    insts: u64,
    params: u64,
    bytes: u64,
    compile_ns: u128,
}

fn check(rig: &mut Rig, seed: u64, st: &mut Stats) {
    let p = generate(seed);
    if let Err(e) = gravity_ir::verify::verify(&p).and_then(|()| gravity_ir::verify::verify_dominance(&p)) {
        panic!("seed {seed}: the generated program fails verification: {} {}\n{}", e.func, e.message, print_program(&p));
    }
    let bound = rig.bind(&p, touch_guest());
    let t0 = std::time::Instant::now();
    let bytes = gravity_wasm::compile(&bound, Func(0)).unwrap_or_else(|e| panic!("seed {seed}: refused: {e}\n{}", print_program(&p)));
    st.compile_ns += t0.elapsed().as_nanos();
    st.bytes += bytes.len() as u64;
    let (inst, _) = rig.instantiate(&bound, Func(0));
    let cells: Vec<i64> = (0..64).map(|k| 0x0102_0304_0506_0708u64.wrapping_mul(k as u64 + 1).rotate_left(k as u32 * 3) as i64).collect();
    rig.write(CELLS, &cells.iter().flat_map(|c| c.to_le_bytes()).collect::<Vec<u8>>());
    for n in [0i64, 1, 2, 5] {
        let mut q_i = 7i64;
        let interp = gravity_interp::Interp::new(&p).run(Func(0), &[cells.as_ptr() as u64, &mut q_i as *mut i64 as u64, n as u64]).unwrap_or_else(|t| panic!("seed {seed} n {n}: interpreter trap {t:?}\n{}", print_program(&p)));
        rig.write_i64(Q, 7);
        let wasm = rig.call(&inst, &bound, Func(0), &[CELLS as u64, Q as u64, n as u64]).unwrap_or_else(|e| panic!("seed {seed} n {n}: wasm trap {e}\n{}", print_program(&p)));
        assert_eq!(wasm, interp, "seed {seed} n {n}: wasm vs interpreter\n{}", print_program(&p));
        assert_eq!(rig.read_i64(Q), q_i, "seed {seed} n {n}: the host's cell after the run");
    }
    st.programs += 1;
    st.blocks += p.funcs[0].blocks.len() as u64;
    st.insts += p.funcs[0].blocks.iter().map(|b| b.insts.len() as u64).sum::<u64>();
    st.params += p.funcs[0].blocks.iter().map(|b| b.params.len() as u64).sum::<u64>();
}

/// The TPDE arm's once-failing seeds (backend.md §6.7), kept as the fixed set.
#[test]
fn known_seeds() {
    let mut rig = Rig::new();
    let mut st = Stats::default();
    for seed in [16729u64, 1191, 17648, 64142] {
        check(&mut rig, seed, &mut st);
    }
}

#[test]
fn random_phi_programs_agree_wasm_vs_interpreter() {
    let mut rig = Rig::new();
    let mut st = Stats::default();
    if let Ok(s) = std::env::var("GRAVITY_PHI_FUZZ_SEED") {
        let seed: u64 = s.parse().unwrap();
        eprintln!("{}", print_program(&generate(seed)));
        check(&mut rig, seed, &mut st);
        return;
    }
    let n: u64 = std::env::var("GRAVITY_PHI_FUZZ_N").ok().and_then(|s| s.parse().ok()).unwrap_or(1_000);
    let from: u64 = std::env::var("GRAVITY_PHI_FUZZ_FROM").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    let keep_going = std::env::var_os("GRAVITY_PHI_FUZZ_KEEP_GOING").is_some();
    let mut failed: Vec<u64> = Vec::new();
    let t0 = std::time::Instant::now();
    for seed in from..from + n {
        if seed % 5_000 == 0 && seed > from {
            // A fresh store: the table and the compiled modules are per store.
            rig = Rig::new();
        }
        let r = catch_unwind(AssertUnwindSafe(|| check(&mut rig, seed, &mut st)));
        if let Err(e) = r {
            eprintln!("--- phi_fuzz seed {seed} (replay: GRAVITY_PHI_FUZZ_SEED={seed}) ---\n{}", print_program(&generate(seed)));
            if !keep_going {
                resume_unwind(e);
            }
            failed.push(seed);
            continue;
        }
        if seed % 2000 == 1999 {
            eprintln!("phi_fuzz: {} programs agree; {st:?}", seed + 1 - from);
        }
    }
    eprintln!(
        "phi_fuzz(wasm): seeds {from}..{}: {} programs agree in {:.1} s; {st:?}; compile median-ish {} us/program, {} bytes/program",
        from + n,
        st.programs,
        t0.elapsed().as_secs_f64(),
        st.compile_ns / 1000 / st.programs.max(1) as u128,
        st.bytes / st.programs.max(1)
    );
    assert!(failed.is_empty(), "phi_fuzz: failing seeds {failed:?}");
}
