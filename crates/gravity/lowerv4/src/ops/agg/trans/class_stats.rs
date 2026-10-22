//! Kind `class.stats` (`ClassImpl`: variance / stddev / covar / regr families; L4) (cards/M6.md, M6-changes.md §1).
//! Owner M6-FOLD.
//!
//! var_pop / var_samp / stddev_pop / stddev_samp over float, integer and numeric inputs, and the float8 regression family
//! (covar / corr / regr_*). D4565: a plain call over int2 / int4 (`ExactStat`) folds PostgreSQL's own transition state in
//! its record slots (decompose `stat_slot`, road `agg.stat_reg`, on by default: off when
//! `pgrust.gravity_road_off` names it plainly): `int2_accum` / `int4_accum`'s Int128AggState, sumX
//! and N in the `AvgI128` slot and sumX2 in the `SumI128` companion right after it, each merged by its exact add
//! (`numeric_poly_combine`); the final `numeric_poly_stddev_internal` itself (gravity_fn agg_final `int128_stat`), so
//! every row's result is PG's however a group's records were evicted and merged.
//!
//! Every other call (float's Youngs-Cramer state, whose `float8_combine` of evicted records is not PG's serial fold bit
//! for bit; int8 and numeric's NumericAggState; the regression family; a split, DISTINCT or ordered call; or the road
//! off) is the straight form (D2): PG's transition by fmgr through face/aggcall.rs into an `AggState`
//! slot, class.num_varlena's hosted fold (fold/hosted.rs), the finals by fmgr.
use gravity_abi::breaker::SlotKind;
use gravity_decompose::ops::shared::agg::{AggDecl, Register as R};
use gravity_decompose::ops::shared::agg_class::{Class, Stat, Trans};
use gravity_expr::emit::CellValid;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value};
use mcx::{CollectIn, Mcx, PgVec};

use crate::face::fold::{Acc, AccShape, AnswerForm, BlockFold, ClassImpl, FinalOrder, FoldClass};
use crate::face::kinds::KindId;
use crate::frame::env::Env;
use crate::admit::refusal::Reason;
use crate::ops::Table;
use crate::rep::CellRef;

crate::strategies! {
    /// D4565: var / stddev over int2 / int4 fold PG's transition state in record slots (decompose reads the road,
    /// on by default; named off plainly, the call folds the hosted state).
    STAT_REG = 147, "agg.stat_reg", Stats;
}

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::ClassStats, &ClassImpl(&STATS))], strategies: &[(STAT_REG, &())], ..crate::ops::NONE };

struct Stats;
static STATS: Stats = Stats;

/// The class folding `a`: a register form where decompose gave it its slot, else the hosted fold.
fn pick(a: &AggDecl<'_>) -> &'static dyn FoldClass {
    match (a.reg, a.trans) {
        (R::ExactStat, Trans::Abi(SlotKind::AvgI128)) => &EXACT,
        _ => &crate::ops::agg::numeric::class_num_varlena::FOLD,
    }
}

impl FoldClass for Stats {
    fn shape(&self, a: &AggDecl<'_>) -> AccShape {
        pick(a).shape(a)
    }
    fn init(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>) {
        pick(a).init(b, env, a, acc)
    }
    fn step(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>, args: &[CellRef<'_>]) {
        pick(a).step(b, env, a, acc, args)
    }
    fn merge(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, into: Acc<'_>, from: Acc<'_>) {
        pick(a).merge(b, env, a, into, from)
    }
    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>) -> (Value, Value) {
        pick(a).finish(b, env, a, acc)
    }
    fn finish_direct(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>, direct: &[(Value, Value)]) -> (Value, Value) {
        pick(a).finish_direct(b, env, a, acc, direct)
    }
    fn fold_ahead(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>) {
        pick(a).fold_ahead(b, env, a, acc)
    }
    fn order(&self, a: &AggDecl<'_>) -> Option<FinalOrder> {
        pick(a).order(a)
    }
    fn block(&self, a: &AggDecl<'_>) -> Option<BlockFold> {
        pick(a).block(a)
    }
    fn answer(&self, a: &AggDecl<'_>) -> Option<AnswerForm> {
        pick(a).answer(a)
    }
}

/// D4565: int2 / int4's Int128AggState (numeric.c `do_int128_accum` with sumX2) in registers `[sx_lo, sx_hi, sq_lo,
/// sq_hi, n]`, in a record as the `AvgI128` slot `[sx_lo, sx_hi, n]` and its `SumI128` companion `[sq_lo, sq_hi]` (the
/// next slot: 24 bytes on, the next validity bit), both marked once a row folded.
struct Exact;
static EXACT: Exact = Exact;

/// The companion's word offset from the slot's and its validity (the next bit), as class_num's dscale companion.
fn companion(off: i32, valid: CellValid) -> (i32, CellValid) {
    let v = match valid {
        CellValid::Bit { off, bit: 7 } => CellValid::Bit { off: off + 1, bit: 0 },
        CellValid::Bit { off, bit } => CellValid::Bit { off, bit: bit + 1 },
        v => v,
    };
    (off + 24, v)
}

/// The state's words: registers as they are; a record's (an unmarked slot the identity, zero).
fn load<'e>(b: &mut FunctionBuilder<'_>, env: &Env<'e, '_>, acc: &Acc<'_>) -> PgVec<'e, Value> {
    let (base, off, valid) = match acc {
        Acc::Regs(r) => return mcx::to_vec_in(&r[..], env.mcx),
        Acc::Slot { base, off, valid } => (*base, *off, *valid),
    };
    let ok = crate::ops::shared::slots::slot_ok(b, env, base, valid);
    let q = companion(off, valid).0;
    [off, off + 8, q, q + 8, off + 16]
        .into_iter()
        .map(|o| {
            let w = b.load(Type::I64, base, o);
            b.select(ok, w, env.zero)
        })
        .collect_in(env.mcx)
}

fn store(b: &mut FunctionBuilder<'_>, env: &Env, acc: Acc<'_>, w: &[Value]) {
    match acc {
        Acc::Regs(r) => r.copy_from_slice(w),
        Acc::Slot { base, off, valid } => {
            let (q, qv) = companion(off, valid);
            for (o, &v) in [off, off + 8, q, q + 8, off + 16].into_iter().zip(w) {
                b.store(base, o, v);
            }
            let any = b.icmp(IntCond::Ne, w[4], env.zero);
            crate::ops::shared::slots::mark(b, env, base, valid, any);
            crate::ops::shared::slots::mark(b, env, base, qv, any);
        }
    }
}

/// Two states added (`numeric_poly_combine`: each sum exact in 128 bits, the counts).
fn combine<'e>(b: &mut FunctionBuilder<'_>, mcx: Mcx<'e>, x: &[Value], y: &[Value]) -> PgVec<'e, Value> {
    use crate::ops::agg::trans::class_word::add128;
    let (lo, hi) = add128(b, x[0], x[1], y[0], y[1]);
    let (qlo, qhi) = add128(b, x[2], x[3], y[2], y[3]);
    let n = b.int_bin(IntBinOp::Add, x[4], y[4]);
    mcx::pg_vec![in mcx; lo, hi, qlo, qhi, n]
}

/// One row's state (`do_int128_accum` of int2 / int4 `x` into the empty state), the identity where `ok` fails (a NULL
/// argument): x sign-extended to 128 bits, x * x (an int4's square is under 2^62: one non-negative word), N.
fn row(b: &mut FunctionBuilder<'_>, x: Value, ok: Value, zero: Value) -> [Value; 5] {
    let x = b.select(ok, x, zero);
    let s63 = b.iconst(Type::I64, 63);
    let hi = b.int_bin(IntBinOp::Sshr, x, s63);
    let sq = b.int_bin(IntBinOp::Mul, x, x);
    let n = b.conv(ConvOp::Zext, Type::I64, ok);
    [x, hi, sq, zero, n]
}

/// `[stddev?, sample?]` of the call's class (the catalog finals `numeric_poly_{var,stddev}_{pop,samp}`).
fn form(env: &Env, a: &AggDecl<'_>) -> (bool, bool) {
    match a.class {
        Class::Stat3I128(Stat::VarPop) => (false, false),
        Class::Stat3I128(Stat::VarSamp) => (false, true),
        Class::Stat3I128(Stat::StddevPop) => (true, false),
        Class::Stat3I128(Stat::StddevSamp) => (true, true),
        _ => crate::refuse!(env.node, Reason::Bug { payload: "the exact stat register over another class" }),
    }
}

impl FoldClass for Exact {
    fn shape(&self, _a: &AggDecl<'_>) -> AccShape {
        AccShape { words: 5, bytes: 40, align: 8 }
    }

    fn init(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _a: &AggDecl<'_>, acc: Acc<'_>) {
        let w = mcx::pg_vec![in env.mcx; env.zero; 5];
        store(b, env, acc, &w[..]);
    }

    /// One row (`row`) added to the state; a NULL argument is the strict transition's skip.
    fn step(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>, args: &[CellRef<'_>]) {
        if args.len() != 1 {
            crate::refuse!(env.node, Reason::Bug { payload: "the exact stat register over other than one argument" });
        }
        let (x, ok) = crate::ops::agg::trans::plain::args_ok(b, env, a, args);
        let r = row(b, x, ok, env.zero);
        let cur = load(b, env, &acc);
        let new = combine(b, env.mcx, &cur[..], &r);
        store(b, env, acc, &new[..]);
    }

    fn merge(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _a: &AggDecl<'_>, into: Acc<'_>, from: Acc<'_>) {
        let (x, y) = (load(b, env, &into), load(b, env, &from));
        let new = combine(b, env.mcx, &x[..], &y[..]);
        store(b, env, into, &new[..]);
    }

    /// `numeric_poly_stddev_internal` over the state (gravity_fn agg_final `int128_stat`): NULL with no row, or one row
    /// for a sample form, as PG's (the host is entered only past that).
    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>) -> (Value, Value) {
        let (root, samp) = form(env, a);
        let w = load(b, env, &acc);
        let one = b.iconst(Type::I64, 1);
        let null = b.icmp(if samp { IntCond::Sle } else { IntCond::Eq }, w[4], if samp { one } else { env.zero });
        let (name, host) = match (root, samp) {
            (false, false) => ("gravity_expr::int_var_pop", gravity_abi::host_words!(gravity_expr::emit::agg::int_var_pop(region, sx_lo, sx_hi, sq_lo, sq_hi, n))),
            (false, true) => ("gravity_expr::int_var_samp", gravity_abi::host_words!(gravity_expr::emit::agg::int_var_samp(region, sx_lo, sx_hi, sq_lo, sq_hi, n))),
            (true, false) => ("gravity_expr::int_stddev_pop", gravity_abi::host_words!(gravity_expr::emit::agg::int_stddev_pop(region, sx_lo, sx_hi, sq_lo, sq_hi, n))),
            (true, true) => ("gravity_expr::int_stddev_samp", gravity_abi::host_words!(gravity_expr::emit::agg::int_stddev_samp(region, sx_lo, sx_hi, sq_lo, sq_hi, n))),
        };
        (crate::ops::agg::trans::plain::host_final(b, env, name, host, &w[..], null), null)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `f` over word params, its words returned, in the interpreter.
    fn run(args: &[u64], out: usize, f: impl FnOnce(&mut FunctionBuilder<'_>, &[Value]) -> Vec<Value>) -> Vec<u64> {
        let mut pb = gravity_ir::builder::ProgramBuilder::new();
        let fid = pb.declare_func("t", &vec![Type::I64; args.len()], &vec![Type::I64; out]);
        let mut b = pb.define(fid);
        let p = b.params().to_vec();
        let r = f(&mut b, &p);
        b.ret(&r);
        let prog = pb.finish();
        gravity_interp::Interp::new(&prog).run(fid, args).expect("runs")
    }

    /// `rows` folded from the empty state, one row at a time (`None`: a NULL argument), as the IR's words.
    fn fold(rows: &[Option<i64>]) -> [u64; 5] {
        let cx = mcx::MemoryContext::new("d4565");
        let mut s = [0u64; 5];
        for x in rows {
            let args = [s[0], s[1], s[2], s[3], s[4], x.unwrap_or(0) as u64, x.is_some() as u64];
            let w = run(&args, 5, |b, p| {
                let zero = b.iconst(Type::I64, 0);
                let ok = b.icmp(IntCond::Ne, p[6], zero);
                let r = row(b, p[5], ok, zero);
                combine(b, cx.mcx(), &p[..5], &r).to_vec()
            });
            s.copy_from_slice(&w);
        }
        s
    }

    fn merged(x: [u64; 5], y: [u64; 5]) -> [u64; 5] {
        let cx = mcx::MemoryContext::new("d4565");
        let w = run(&[x, y].concat(), 5, |b, p| combine(b, cx.mcx(), &p[..5], &p[5..]).to_vec());
        w.try_into().unwrap()
    }

    /// numeric.c's Int128AggState after `do_int128_accum` over the non-NULL rows, as `[sx_lo, sx_hi, sq_lo, sq_hi, n]`.
    fn pg(rows: &[Option<i64>]) -> [u64; 5] {
        let mut st = adt_numeric::Int128AggState::new(true);
        rows.iter().flatten().for_each(|&x| adt_numeric::aggregates::do_int128_accum(&mut st, x as i128));
        let w = |v: i128| [v as u64, (v >> 64) as u64];
        let (sx, sq) = (w(st.sum_x), w(st.sum_x2));
        [sx[0], sx[1], sq[0], sq[1], st.n as u64]
    }

    /// D4565: the register fold is int4_accum's Int128AggState (sumX and sumX2 exact in 128 bits, N), a NULL row skipped,
    /// and two partial states merged are the state that saw every row (`numeric_poly_combine`), at the int2 / int4
    /// extremes too.
    #[test]
    fn exact_fold_is_int128_agg_state() {
        let (max, min) = (i32::MAX as i64, i32::MIN as i64);
        let sets: [Vec<Option<i64>>; 6] = [
            vec![Some(1), Some(2), None, Some(-3), Some(7)],
            vec![None, None],
            vec![Some(5)],
            vec![Some(max); 40],
            [vec![Some(min); 33], vec![Some(max); 7], vec![None]].concat(),
            vec![Some(i16::MIN as i64), Some(i16::MAX as i64), Some(-1), Some(0)],
        ];
        for rows in &sets {
            assert_eq!(fold(rows), pg(rows), "{rows:?}");
            let (a, b) = rows.split_at(rows.len() / 2);
            assert_eq!(merged(fold(a), fold(b)), pg(rows), "{rows:?} in two parts");
        }
        assert!(pg(&sets[4])[1] != 0 && pg(&sets[4])[3] != 0, "the extremes carry into the high words");
    }

    /// The final: numeric_poly_stddev_internal over the state, NULL with no row (or one, sampled), as PG's catalog finals.
    #[test]
    fn exact_final_is_numeric_poly_stddev() {
        let rows = [Some(1), Some(2), Some(4), None, Some(-8)];
        let s = pg(&rows);
        let st = adt_numeric::Int128AggState { calc_sum_x2: true, n: 4, sum_x: -1, sum_x2: 85 };
        for (stddev, sample) in [(false, false), (false, true), (true, false), (true, true)] {
            let want = adt_numeric::aggregates::numeric_poly_stddev_internal(Some(&st), !stddev, sample).unwrap().unwrap();
            let mut buf = [0u8; 256];
            let mut r = gravity_fn::Region::over(&mut buf);
            let mut e = gravity_fn::ErrSlot::NONE;
            let got = gravity_fn::agg_final::int128_stat(&mut e, &mut r, s[0] as i64, s[1] as i64, s[2] as i64, s[3] as i64, s[4] as i64, stddev, sample);
            assert_eq!(got.status, gravity_fn::OK, "{stddev} {sample}");
            let at = got.val as usize - buf.as_ptr() as usize;
            assert_eq!(&buf[at..at + want.as_bytes().len()], want.as_bytes(), "{stddev} {sample}");
        }
    }
}
