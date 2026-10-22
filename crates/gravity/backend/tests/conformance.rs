//! Two-backend conformance (milestone-2.md §2a): one IR program through TPDE and the interpreter, as
//! `Code`, run by the real driver on the fixture rig; the results are byte-identical to each other and
//! to the hand kernel at every width.
#[path = "../../parallel/tests/common/mod.rs"]
mod common;

use std::sync::Arc;

use common::*;
use gravity_abi::breaker::{Breaker, BreakerKind, HashAgg};
use gravity_abi::ctx::{hdr, lane, site_off};
use gravity_abi::site::site;
use gravity_abi::status::{unsupported, ERROR, UNSUPPORTED_SHAPE};
use gravity_abi::Ctx;
use gravity_driver::Entry;
use gravity_ir::builder::ProgramBuilder;
use gravity_ir::*;
use gravity_kernels::par_scan_count;
use gravity_kernels::util::{advance, partial, range_seek};
use gravity_parallel::FixturePool;
use gravity_program::hand::INT8;
use gravity_program::{Code, End, Layout, OutCol, Pipeline, Program, Reason, SiteDesc, WritePlan};
use mcx::MemoryContext;

const SITE: i32 = site_off(0) as i32;

unsafe extern "C" fn h_seek(ctx: u64) -> u64 {
    range_seek(ctx as *mut Ctx, 0) as u64
}

unsafe extern "C" fn h_advance(ctx: u64) -> u64 {
    advance(ctx as *mut Ctx, 0) as u64
}

unsafe extern "C" fn h_add_count(ctx: u64, n: u64) -> u64 {
    HashAgg::at(partial(ctx as *mut Ctx), &par_scan_count::LAYOUT, 1, par_scan_count::BYTES).single().add_count(0, n as i64);
    0
}

/// `par_scan_count` in IR: `count(*) where int4(col0) < params[0]` over the site's windows. Host calls
/// once per window (seek, advance, add_count: abi-v2 §3); the row loop is IR loads and branches.
fn count_ir() -> (gravity_ir::Program, Func) {
    let mut pb = ProgramBuilder::new();
    let seek = pb.declare_host("seek", &[Type::Ptr], Some(Type::I64), h_seek as usize, HostFlags::default());
    let adv = pb.declare_host("advance", &[Type::Ptr], Some(Type::I64), h_advance as usize, HostFlags::default());
    let add = pb.declare_host("add_count", &[Type::Ptr, Type::I64], Some(Type::I64), h_add_count as usize, HostFlags::default());
    let f = pb.declare_func("count_lt", &[Type::Ptr], &[Type::I64]);
    let mut b = pb.define(f);
    let ctx = b.params()[0];
    let l_loop = b.create_block(&[]);
    let l_window = b.create_block(&[]);
    let l_check = b.create_block(&[]);
    let l_verdict = b.create_block(&[]);
    let l_shape = b.create_block(&[]);
    let l_count = b.create_block(&[Type::I64, Type::I64]);
    let l_body = b.create_block(&[]);
    let l_gather = b.create_block(&[]);
    let l_row = b.create_block(&[Type::I64]);
    let l_bit = b.create_block(&[]);
    let l_accum = b.create_block(&[Type::I64]);
    let l_finish = b.create_block(&[Type::I64]);
    let l_skip = b.create_block(&[]);
    let l_exit = b.create_block(&[Type::I64]);

    let zero = b.iconst(Type::I64, 0);
    let one = b.iconst(Type::I64, 1);
    let r = b.call_host(seek, &[ctx], Some(Type::I64)).unwrap();
    let neg = b.icmp(IntCond::Slt, r, zero);
    let err = b.iconst(Type::I64, ERROR);
    b.cond_br(neg, (l_exit, &[err]), (l_loop, &[]));

    b.switch_to(l_loop);
    let r = b.call_host(adv, &[ctx], Some(Type::I64)).unwrap();
    let le = b.icmp(IntCond::Sle, r, zero);
    b.cond_br(le, (l_exit, &[r]), (l_window, &[]));

    b.switch_to(l_window);
    let cursor32 = b.load(Type::I32, ctx, SITE + site::CURSOR as i32);
    let cursor = b.conv(ConvOp::Zext, Type::I64, cursor32);
    let rows32 = b.load(Type::I32, ctx, SITE + site::ROWS as i32);
    let rows = b.conv(ConvOp::Zext, Type::I64, rows32);
    let sel = b.load(Type::Ptr, ctx, SITE + site::SEL as i32);
    let sel_i = b.conv(ConvOp::PtrToInt, Type::I64, sel);
    let sel_null = b.icmp(IntCond::Eq, sel_i, zero);
    let sel_n32 = b.load(Type::I32, ctx, SITE + site::SEL_N as i32);
    let sel_n = b.conv(ConvOp::Zext, Type::I64, sel_n32);
    let end = b.select(sel_null, rows, sel_n);
    let params = b.load(Type::Ptr, ctx, hdr::PARAMS as i32);
    let d = b.load(Type::I64, params, 0);
    let thr = b.conv(ConvOp::Trunc, Type::I32, d);
    let pnull = b.load(Type::Ptr, ctx, hdr::PARAMS_NULL as i32);
    let tn = b.load_zext(Type::I8, pnull, 0);
    let tn_set = b.icmp(IntCond::Ne, tn, zero);
    b.cond_br(tn_set, (l_skip, &[]), (l_check, &[]));

    b.switch_to(l_check);
    let answered = b.load_zext(Type::I8, ctx, SITE + site::ANSWERED as i32);
    let a_set = b.icmp(IntCond::Ne, answered, zero);
    let span = b.int_bin(IntBinOp::Sub, end, cursor);
    b.cond_br(a_set, (l_finish, &[span]), (l_verdict, &[]));

    b.switch_to(l_verdict);
    let lanes = b.load(Type::Ptr, ctx, SITE + site::LANES as i32);
    let flags = b.load_zext(Type::I8, lanes, lane::FLAGS as i32);
    let mask = b.iconst(Type::I64, lane::VERDICT_MASK as i64);
    let verdict = b.int_bin(IntBinOp::And, flags, mask);
    let all = b.iconst(Type::I64, lane::VERDICT_ALL_MATCH as i64);
    let all_match = b.icmp(IntCond::Eq, verdict, all);
    b.cond_br(all_match, (l_finish, &[span]), (l_shape, &[]));

    b.switch_to(l_shape);
    let width = b.load_zext(Type::I8, lanes, lane::WIDTH as i32);
    let eight = b.iconst(Type::I64, 8);
    let datum_lane = b.icmp(IntCond::Eq, width, eight);
    let unsup = b.iconst(Type::I64, unsupported(UNSUPPORTED_SHAPE));
    b.cond_br(datum_lane, (l_count, &[cursor, zero]), (l_exit, &[unsup]));

    b.switch_to(l_count);
    let (j, c) = (b.block_params(l_count)[0], b.block_params(l_count)[1]);
    let done = b.icmp(IntCond::Uge, j, end);
    b.cond_br(done, (l_finish, &[c]), (l_body, &[]));

    b.switch_to(l_body);
    b.cond_br(sel_null, (l_row, &[j]), (l_gather, &[]));

    b.switch_to(l_gather);
    let ip = b.ptr_add(sel, j, 4, 0);
    let i32v = b.load(Type::I32, ip, 0);
    let i = b.conv(ConvOp::Zext, Type::I64, i32v);
    b.br(l_row, &[i]);

    b.switch_to(l_row);
    let i = b.block_params(l_row)[0];
    let ptr = b.load(Type::Ptr, lanes, lane::PTR as i32);
    let vp = b.ptr_add(ptr, i, 8, 0);
    let v = b.load(Type::I64, vp, 0);
    let v32 = b.conv(ConvOp::Trunc, Type::I32, v);
    let lt = b.icmp(IntCond::Slt, v32, thr);
    let hit = b.select(lt, one, zero);
    let valid = b.load(Type::Ptr, lanes, lane::VALID as i32);
    let valid_i = b.conv(ConvOp::PtrToInt, Type::I64, valid);
    let valid_null = b.icmp(IntCond::Eq, valid_i, zero);
    b.cond_br(valid_null, (l_accum, &[hit]), (l_bit, &[]));

    b.switch_to(l_bit);
    let six = b.iconst(Type::I64, 6);
    let wi = b.int_bin(IntBinOp::Ushr, i, six);
    let wp = b.ptr_add(valid, wi, 8, 0);
    let word = b.load(Type::I64, wp, 0);
    let m63 = b.iconst(Type::I64, 63);
    let sh = b.int_bin(IntBinOp::And, i, m63);
    let bit = b.int_bin(IntBinOp::Ushr, word, sh);
    let bit1 = b.int_bin(IntBinOp::And, bit, one);
    let hit2 = b.int_bin(IntBinOp::And, hit, bit1);
    b.br(l_accum, &[hit2]);

    b.switch_to(l_accum);
    let h = b.block_params(l_accum)[0];
    let c2 = b.int_bin(IntBinOp::Add, c, h);
    let j2 = b.int_bin(IntBinOp::Add, j, one);
    b.br(l_count, &[j2, c2]);

    b.switch_to(l_finish);
    let hits = b.block_params(l_finish)[0];
    b.call_host(add, &[ctx, hits], Some(Type::I64));
    let ro = b.load(Type::I64, ctx, SITE + site::ROWS_OUT as i32);
    let ro2 = b.int_bin(IntBinOp::Add, ro, hits);
    b.store(ctx, SITE + site::ROWS_OUT as i32, ro2);
    b.br(l_skip, &[]);

    b.switch_to(l_skip);
    let end32 = b.conv(ConvOp::Trunc, Type::I32, end);
    b.store(ctx, SITE + site::CURSOR as i32, end32);
    b.br(l_loop, &[]);

    b.switch_to(l_exit);
    let st = b.block_params(l_exit)[0];
    b.ret(&[st]);
    drop(b);
    (pb.finish(), f)
}

fn count_program(code: Code, rows: u64) -> Program {
    let st = gravity_program::Arena::new(1 << 12);
    let sites = [SiteDesc::range_source(st.handle(), T_A, &[1])];
    let out = [OutCol::new("count", INT8, -1, 8, true)];
    let pipes = [
        (Pipeline::eligible(0, End::Breaker(Breaker::sized(BreakerKind::PlainAgg, par_scan_count::BYTES)), rows), code),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(par_scan_count::par_scan_count_emit)),
    ];
    Program::with_pipelines(Layout { nsites: 1, out_natts: 1, out_rows: 1, scratch_bytes: 0, exec_bytes: 0 }, &sites, &out, WritePlan::SELECT, pipes)
}

fn run_all(program: Arc<Program>, table: &Arc<FixtureTable>, derive: &mut FixtureDerive, pool: &FixturePool, thr: u64) -> Vec<(u16, u64, Vec<Vec<u64>>)> {
    let mut entry = Entry::new(2);
    let mut out = Vec::new();
    for w in WIDTHS {
        for claim in [1u64, 7, 64] {
            table.fresh();
            let mut sink = LegSink::new(true);
            let rows = run_once(&mut entry, &program, derive, pool, w, claim, &[thr], &NO_ABORT, &mut sink).unwrap();
            assert_eq!(rows, 1, "width {w} claim {claim}");
            assert!(table.ledger.consumed_once(), "width {w} claim {claim}");
            out.push((w, claim, sink.sorted()));
        }
    }
    out
}

#[test]
fn the_count_kernel_through_tpde_and_the_interpreter_matches_the_hand_kernel_at_every_width() {
    let (ir, f) = count_ir();
    gravity_ir::verify::verify(&ir).unwrap_or_else(|e| panic!("{e:?}"));
    gravity_ir::verify::verify_dominance(&ir).unwrap_or_else(|e| panic!("{e:?}"));
    let table = FixtureTable::new(T_A, 200, 50, gen_a);
    let mut derive = FixtureDerive::new(&[&table]);
    let pool = FixturePool::new(8);
    let thr = 400u64;
    let expect = table.rows().filter(|r| r[0] < thr as i64).count() as u64;

    let mapped = gravity_backend::mapped(&ir, f).unwrap_or_else(|e| panic!("{e:?}"));
    assert!(!mapped.is_hand() && !mapped.is_interp() && mapped.bytes() > 0, "{mapped:?}");
    let interp = gravity_backend::interp(ir.clone(), f).unwrap();
    assert!(interp.is_interp(), "{interp:?}");

    let hand = run_all(Arc::new(count_program(Code::hand(par_scan_count::par_scan_count), 10_000)), &table, &mut derive, &pool, thr);
    let tpde = run_all(Arc::new(count_program(mapped, 10_000)), &table, &mut derive, &pool, thr);
    let interp = run_all(Arc::new(count_program(interp, 10_000)), &table, &mut derive, &pool, thr);
    for (w, claim, rows) in &hand {
        assert_eq!(rows, &vec![vec![expect]], "hand width {w} claim {claim}");
    }
    assert_eq!(tpde, hand, "tpde vs hand");
    assert_eq!(interp, hand, "interp vs hand");
    assert!(pool.claims_run() > 0);
}

/// Arithmetic, overflow predicates, division, shifts, bit counts, float round trips and a loop with
/// parallel-copy edges: native and interpreted results are bit-identical over a grid of inputs.
fn mix_ir() -> (gravity_ir::Program, Func) {
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("mix", &[Type::I64, Type::I64], &[Type::I64]);
    let mut b = pb.define(f);
    let (a, x) = (b.params()[0], b.params()[1]);
    let l_loop = b.create_block(&[Type::I64, Type::I64]);
    let l_step = b.create_block(&[]);
    let l_exit = b.create_block(&[Type::I64]);
    let zero = b.iconst(Type::I64, 0);
    let one = b.iconst(Type::I64, 1);
    let aa = b.int_un(IntUnOp::Neg, a);
    let neg = b.icmp(IntCond::Slt, a, zero);
    let abs_a = b.select(neg, aa, a);
    let xx = b.int_un(IntUnOp::Neg, x);
    let negx = b.icmp(IntCond::Slt, x, zero);
    let abs_x = b.select(negx, xx, x);
    b.br(l_loop, &[abs_a, abs_x]);

    b.switch_to(l_loop);
    let (p, q) = (b.block_params(l_loop)[0], b.block_params(l_loop)[1]);
    let done = b.icmp(IntCond::Eq, q, zero);
    b.cond_br(done, (l_exit, &[p]), (l_step, &[]));

    b.switch_to(l_step);
    let rem = b.int_bin(IntBinOp::Srem, p, q);
    b.br(l_loop, &[q, rem]);

    b.switch_to(l_exit);
    let g = b.block_params(l_exit)[0];
    let xo = b.int_bin(IntBinOp::Or, x, one);
    let quo = b.int_bin(IntBinOp::Sdiv, a, xo);
    let rm = b.int_bin(IntBinOp::Srem, a, xo);
    let ovf = b.int_ovf(IntOvfOp::SaddOvf, a, x);
    let pick = b.select(ovf, quo, rm);
    let three = b.iconst(Type::I64, 3);
    let five = b.iconst(Type::I64, 5);
    let sh1 = b.int_bin(IntBinOp::Shl, a, three);
    let sh2 = b.int_bin(IntBinOp::Ushr, x, five);
    let sh3 = b.int_bin(IntBinOp::Sshr, x, three);
    let y = b.int_bin(IntBinOp::Xor, sh1, sh2);
    let y2 = b.int_bin(IntBinOp::Xor, y, sh3);
    let m = b.int_bin(IntBinOp::Mul, pick, y2);
    let pc = b.int_un(IntUnOp::Popcnt, a);
    let cz = b.int_un(IntUnOp::Clz, xo);
    let tz = b.int_un(IntUnOp::Ctz, xo);
    let fa = b.conv(ConvOp::SintToFloat, Type::F64, a);
    let fx = b.conv(ConvOp::SintToFloat, Type::F64, x);
    let fs = b.float_bin(FloatBinOp::Add, fa, fx);
    let half = b.f64const(0.5);
    let fh = b.float_bin(FloatBinOp::Mul, fs, half);
    let ff = b.float_un(FloatUnOp::Floor, fh);
    let fi = b.conv(ConvOp::FloatToSint, Type::I64, ff);
    let flt = b.fcmp(FloatCond::Lt, fa, fx);
    let fsel = b.select(flt, one, zero);
    let s1 = b.int_bin(IntBinOp::Add, m, fi);
    let s2 = b.int_bin(IntBinOp::Add, s1, pc);
    let s3 = b.int_bin(IntBinOp::Add, s2, cz);
    let s4 = b.int_bin(IntBinOp::Add, s3, tz);
    let s5 = b.int_bin(IntBinOp::Add, s4, g);
    let s6 = b.int_bin(IntBinOp::Add, s5, fsel);
    let n32 = b.conv(ConvOp::Trunc, Type::I32, s6);
    let back = b.conv(ConvOp::Sext, Type::I64, n32);
    let s7 = b.int_bin(IntBinOp::Add, s6, back);
    b.ret(&[s7]);
    drop(b);
    (pb.finish(), f)
}

#[test]
fn pure_arithmetic_and_branches_agree_between_tpde_and_the_interpreter() {
    let (ir, f) = mix_ir();
    gravity_ir::verify::verify(&ir).unwrap_or_else(|e| panic!("{e:?}"));
    gravity_ir::verify::verify_dominance(&ir).unwrap_or_else(|e| panic!("{e:?}"));
    let ctx = MemoryContext::new("gravity-backend-conformance");
    let compiled = gravity_tpde::compile(ctx.mcx(), &ir).unwrap_or_else(|e| panic!("{e:?}"));
    let native: unsafe extern "C" fn(i64, i64) -> i64 = unsafe { core::mem::transmute(compiled.entry(f)) };
    let interp = gravity_interp::Interp::new(&ir);
    let grid: [i64; 12] = [-7, -3, -1, 0, 1, 2, 3, 10, 1000, 48, i32::MAX as i64, -(1 << 40)];
    let mut n = 0;
    for &a in &grid {
        for &x in &grid {
            let want = interp.run(f, &[a as u64, x as u64]).unwrap()[0] as i64;
            let got = unsafe { native(a, x) };
            assert_eq!(got, want, "mix({a}, {x})");
            n += 1;
        }
    }
    assert_eq!(n, 144);
}

#[test]
fn the_entry_contract_is_checked_and_interpreter_code_has_an_identity_entry() {
    let (mix, f) = mix_ir();
    assert!(gravity_backend::check_entry(&mix, f).is_err());
    assert!(gravity_backend::mapped(&mix, f).is_err());
    assert!(gravity_backend::interp(mix, f).is_err());
    let (ir, f) = count_ir();
    let code = gravity_backend::interp(ir, f).unwrap();
    assert!(code.is_interp() && !code.is_hand() && code.bytes() == 0);
    assert_ne!(code.entry() as usize, par_scan_count::par_scan_count as usize);
    assert_eq!(format!("{code:?}").split(',').next(), Some("Code(interp"));
}
