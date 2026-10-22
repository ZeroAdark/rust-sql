//! D3105 (b): merge.join's inner over a Sort or a Material, past its grant (face/slot_run.rs's put road; kernels
//! ops/mergejoin/raw/window.rs). The inner's run full grows only within its owner's limit (`m11_run_full`: W, or the
//! leader's G), else it is put as runs.sort puts a run (a Material's keyless rows in their order); the reader's open
//! (`m11_run_open`) answers the sealed run as before when nothing was put, else a window over the runs merged into one (PG's
//! final merge onto one tape for a random-access sort; a keyless merge keeps run order: PG's Material tuplestore on disk). The merge's fetch stays an index (its mark and restore words unchanged): the sealed run's row `j` at
//! `base + SORTRUN_HDR + j * row_bytes`, the window's at `idx[j - j0]` while `j` is in it (no host call), `m11_window`
//! moving it else (forward fills keep the mark; a restore before it reopens the one run at the mark). A window row's cells
//! hold until the next fetch moves the window: a row's life, as runs.sort's emit's cells (`Until::Row`). The merge tells
//! the window its mark (`mark`), so a forward fill keeps the mark's rows.
use gravity_abi::breaker::SORTRUN_HDR;
use gravity_abi::status::raise;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value};
use gravity_kernels::ops::mergejoin::entries as h11;

use crate::frame::env::Env;
use crate::frame::stop::Exits;
use crate::ops::sort::lower::RunEnd;
use crate::ops::sort::strategies::resident as res;

/// The open's answer kept for the fetches: the run (sealed) or window, whether it is the window, the run's own table.
#[derive(Clone, Copy)]
pub(crate) struct Opened {
    pub(crate) run: Value,
    win: Value,
    full: Value,
}

/// `m11_run_open(ctx, k, view, full, point)` over consumed region `k` (an error leaves by `x.l_exit`): the sealed run or
/// the window. `view` the view's table (lean or the run's own, `full`); `point`: the merge reads a by-reference cell.
pub(crate) fn open(b: &mut FunctionBuilder<'_>, env: &Env, x: &Exits, k: u64, (view, full): (Value, Value), point: bool) -> Opened {
    let h = res::host(b, "m11_run_open", &[Type::Ptr, Type::I64, Type::Ptr, Type::Ptr, Type::I64], Type::Ptr, gravity_abi::host_words!(h11::m11_run_open(ctx, k, tbl, full, point)));
    let (k, pt) = (b.iconst(Type::I64, k as i64), b.iconst(Type::I64, point as i64));
    let run = b.call_host(h, &[env.ctx, k, view, full, pt], Some(Type::Ptr)).expect("m11_run_open returns the run");
    let ri = b.conv(ConvOp::PtrToInt, Type::I64, run);
    let (bad, l_ok) = (b.icmp(IntCond::Slt, ri, env.zero), b.create_block(&[]));
    b.cond_br(bad, (x.l_exit, &[ri]), (l_ok, &[]));
    b.switch_to(l_ok);
    let kind = b.load_zext(Type::I8, run, h11::W_KIND);
    let wk = b.iconst(Type::I64, h11::MJ_WINDOW_KIND as i64);
    let win = b.icmp(IntCond::Eq, kind, wk);
    Opened { run, win, full }
}

/// D3190 (merge.cosplit): `open` for a co-split claim, whose inner is rows `[lo, hi)` of the run
/// (`m11_run_slice(ctx, k, view, full, point, lo, hi)`): the opened run offset by `lo` (each of `lo` and `hi` clamped to its
/// rows), and the slice's rows. A window is only laid for a claim from the run's first row (`lo` 0), so it is not moved.
#[allow(clippy::too_many_arguments)]
pub(crate) fn open_slice(b: &mut FunctionBuilder<'_>, env: &Env, x: &Exits, k: u64, (view, full): (Value, Value), point: bool, (lo, hi): (Value, Value), row_bytes: u32) -> (Opened, Value) {
    use crate::face::cosplit::clamp;
    let h = res::host(b, "m11_run_slice", &[Type::Ptr, Type::I64, Type::Ptr, Type::Ptr, Type::I64, Type::I64, Type::I64], Type::Ptr, gravity_abi::host_words!(h11::m11_run_slice(ctx, k, tbl, full, point, lo, hi)));
    let (k, pt) = (b.iconst(Type::I64, k as i64), b.iconst(Type::I64, point as i64));
    let run = b.call_host(h, &[env.ctx, k, view, full, pt, lo, hi], Some(Type::Ptr)).expect("m11_run_slice returns the run");
    let ri = b.conv(ConvOp::PtrToInt, Type::I64, run);
    let (bad, l_ok) = (b.icmp(IntCond::Slt, ri, env.zero), b.create_block(&[]));
    b.cond_br(bad, (x.l_exit, &[ri]), (l_ok, &[]));
    b.switch_to(l_ok);
    let kind = b.load_zext(Type::I8, run, h11::W_KIND);
    let wk = b.iconst(Type::I64, h11::MJ_WINDOW_KIND as i64);
    let win = b.icmp(IntCond::Eq, kind, wk);
    let n = b.load_zext(Type::I32, run, 4); // SortRun::len, the window's rows
    let (lo, hi) = (clamp(b, lo, n), clamp(b, hi, n));
    let rb = b.iconst(Type::I64, row_bytes as i64);
    let off = b.int_bin(IntBinOp::Mul, lo, rb);
    let run = b.ptr_add(run, off, 1, 0);
    (Opened { run, win, full }, b.int_bin(IntBinOp::Sub, hi, lo))
}

/// Row `j`'s address: the sealed run's by index, or the window's (`idx[j - j0]`, `m11_window` when `j` is outside it; its
/// error leaves by the row's exit).
pub(crate) fn fetch(b: &mut FunctionBuilder<'_>, env: &Env, o: Opened, row_bytes: u32, j: Value) -> Value {
    let (l_res, l_win, l_hit, l_miss, l_done) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::I64]));
    b.cond_br(o.win, (l_win, &[]), (l_res, &[]));
    b.switch_to(l_res);
    let rb = b.iconst(Type::I64, row_bytes as i64);
    let off = b.int_bin(IntBinOp::Mul, j, rb);
    let p = b.ptr_add(o.run, off, 1, SORTRUN_HDR as i32);
    let pi = b.conv(ConvOp::PtrToInt, Type::I64, p);
    b.br(l_done, &[pi]);
    b.switch_to(l_win);
    let (j0, cnt) = (b.load(Type::I64, o.run, h11::W_J0), b.load(Type::I64, o.run, h11::W_CNT));
    let d = b.int_bin(IntBinOp::Sub, j, j0);
    let hit = b.icmp(IntCond::Ult, d, cnt);
    b.cond_br(hit, (l_hit, &[]), (l_miss, &[]));
    b.switch_to(l_hit);
    let ix = b.load(Type::I64, o.run, h11::W_IDX);
    let ix = b.conv(ConvOp::IntToPtr, Type::Ptr, ix);
    let at = b.ptr_add(ix, d, 8, 0);
    let row = b.load(Type::I64, at, 0);
    b.br(l_done, &[row]);
    b.mark_cold(l_miss);
    b.switch_to(l_miss);
    let h = res::host(b, "m11_window", &[Type::Ptr, Type::Ptr, Type::I64, Type::Ptr], Type::I64, gravity_abi::host_words!(h11::m11_window(ctx, w, j, full)));
    let r = b.call_host(h, &[env.ctx, o.run, j, o.full], Some(Type::I64)).expect("m11_window returns the row");
    let (bad, l_ok) = (b.icmp(IntCond::Slt, r, env.zero), b.create_block(&[]));
    b.cond_br(bad, (env.x.l_exit, &[r]), (l_ok, &[]));
    b.switch_to(l_ok);
    b.br(l_done, &[r]);
    b.switch_to(l_done);
    let p = b.block_params(l_done)[0];
    b.conv(ConvOp::IntToPtr, Type::Ptr, p)
}

/// The merge marked row `j` (its mark word set): the window keeps the mark's rows on its forward fills.
pub(crate) fn mark(b: &mut FunctionBuilder<'_>, o: Opened, j: Value) {
    let (l_set, l_on) = (b.create_block(&[]), b.create_block(&[]));
    b.cond_br(o.win, (l_set, &[]), (l_on, &[]));
    b.switch_to(l_set);
    b.store(o.run, h11::W_MARK, j);
    b.br(l_on, &[]);
    b.switch_to(l_on);
}

/// The inner run full (`RunEnd::append`'s `on_full`): grown within the owner's limit (`m11_run_full`: the new base into
/// the run word `w_run`), else put (`RunEnd::put`, runs.sort's: a status above zero D1330's next run); a failed allocation
/// raises (HOST), a status below zero leaves.
pub(crate) fn full(b: &mut FunctionBuilder<'_>, env: &Env, end: &RunEnd, w_run: i32, tbl: Value) {
    let run = end.run(b, env);
    let h = res::host(b, "m11_run_full", &[Type::Ptr, Type::Ptr, Type::Ptr], Type::I64, gravity_abi::host_words!(h11::m11_run_full(ctx, run, tbl)));
    let r = b.call_host(h, &[env.ctx, run, tbl], Some(Type::I64)).expect("m11_run_full returns the base");
    let (l_grown, l_other, l_put, l_bad, l_oom, l_on) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
    let grown = b.icmp(IntCond::Sgt, r, env.one);
    b.cond_br(grown, (l_grown, &[]), (l_other, &[]));
    b.switch_to(l_grown);
    env.store_word(b, w_run, r);
    b.br(l_on, &[]);
    b.switch_to(l_other);
    let put = b.icmp(IntCond::Eq, r, env.one);
    b.cond_br(put, (l_put, &[]), (l_bad, &[]));
    b.switch_to(l_put);
    end.put(b, env, false);
    b.br(l_on, &[]);
    b.switch_to(l_bad);
    let oom = b.icmp(IntCond::Eq, r, env.zero);
    b.cond_br(oom, (l_oom, &[]), (env.x.l_exit, &[r]));
    b.switch_to(l_oom);
    let e = b.iconst(Type::I64, raise::bit(raise::HOST) as i64);
    b.br(env.l_raise, &[e]);
    b.switch_to(l_on);
}
