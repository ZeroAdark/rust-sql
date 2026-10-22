//! The stop / resume protocol once per function (lowerv4 plan §2.1, H17): the resume dispatch, the open mark, the exits and
//! their bodies, and the resume words beside the header's one `RESUME` word, so a stopped call re-enters inside a row: an
//! expanding step's offset (H17 c) and a row op's EvalPlanQual recheck (H17 b). A resumed call skips the open, so its words
//! (the per-open words and these) keep the values the stop left.
use gravity_abi::ctx::{hdr, site_off, RESUME_NONE};
use gravity_abi::site::site;
use gravity_abi::status;
use gravity_expr::emit::raise_status;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, IntBinOp, IntCond, Type, Value};
use gravity_kernels::util::RESUME_OPEN;
use mcx::{CollectIn, PgVec};

use crate::frame::env::Env;
use crate::frame::instr;
use crate::frame::px::{ExId, Px, WordKey};

/// `l_finish(out, end)`, `l_stop_in(out, cursor, status)`, `l_full(out, cursor)`, `l_status(status)`, `l_done`,
/// `l_raise(err)`, `l_cancel`, `l_exit(status)`.
#[derive(Clone, Copy, Debug)]
pub struct Exits {
    pub l_finish: Block,
    pub l_stop_in: Block,
    pub l_full: Block,
    pub l_status: Block,
    pub l_done: Block,
    pub l_raise: Block,
    pub l_cancel: Block,
    pub l_exit: Block,
}

/// A resume word: where a stopped call re-enters inside a row. A frame word (the program's words site), kept across calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResumeWord {
    /// Pipeline `k` step `j`'s expansion offset (H32 / H17 c): the row's next output. The step declares it as a per-open word
    /// (init 0), stores the offset and stops with the cursor at the row (`RowVals::at`); on resume it continues there.
    StepOffset { k: usize, j: u64 },
    /// Pipeline `k`'s row op EvalPlanQual recheck (H17 b): the row version a stopped recheck resumes on.
    EpqRecheck { k: usize },
    /// Expression `ex`'s held word `i` (`Ex::held`): a volatile value of the row, kept across a stop inside it.
    Held { ex: u32, i: u8 },
    /// Pipeline `k`'s end state word `i` (`EndMech::state_bytes`, D2147): a lazy build's probe pipeline stops inside a claim
    /// and resumes in a new call, so its end's per-claim state (filled by the open) lives here, not in a stack slot.
    EndState { k: usize, i: u16 },
}

/// The byte offset of `w` in the program's frame words (`Env::words`).
pub fn resume_word(px: &mut Px<'_, '_>, w: ResumeWord) -> i32 {
    px.frame_word(WordKey::Resume(w)) as i32 * 8
}

/// The byte offsets of expression `ex`'s `n` held words (`Ex::held`).
pub fn held_words(px: &mut Px<'_, '_>, ex: ExId, n: u8) -> Vec<i32> {
    (0..n).map(|i| resume_word(px, ResumeWord::Held { ex: ex.0, i })).collect() // alloc: Program
}

/// A row's held values (v1's `resumed_row`): on a fresh row `eval`'s, the first `at.len()` stored in the held words `at`;
/// on the row a stopped call re-enters the words' values (the rest 0: I64 only), so a volatile call runs once per row, as
/// PG's. Re-entered: an expanding step after the lowering one has its offset set (`Env::expands`), or `own` (an expanding
/// step's offset, for its own per-row values). Where neither can be set, `eval` alone.
pub fn held<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, at: &[i32], own: Option<i32>, eval: &mut dyn FnMut(&mut FunctionBuilder<'_>, &mut Env<'e, '_>) -> PgVec<'e, Value>) -> PgVec<'e, Value> {
    let offs: PgVec<'_, i32> = env.expands.iter().filter(|e| env.step.is_some_and(|j| e.0 > j)).map(|e| e.1).chain(own).collect_in(env.mcx);
    let Some(words) = env.words.filter(|_| !offs.is_empty()) else { return eval(b, env) };
    let set = offs.iter().fold(env.zero, |a, &o| {
        let w = b.load(Type::I64, words, o);
        b.int_bin(IntBinOp::Or, a, w)
    });
    let again = b.icmp(IntCond::Ne, set, env.zero);
    let (l_old, l_new) = (b.create_block(&[]), b.create_block(&[]));
    b.cond_br(again, (l_old, &[]), (l_new, &[]));
    b.switch_to(l_new);
    let new = eval(b, env);
    at.iter().zip(new.iter()).for_each(|(&o, &v)| b.store(words, o, v));
    let types: PgVec<'_, Type> = new.iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
    let l_join = b.create_block(&types[..]);
    b.br(l_join, &new[..]);
    b.switch_to(l_old);
    let old: PgVec<'_, Value> = types.iter().enumerate().map(|(i, &t)| match at.get(i) { Some(&o) => b.load(t, words, o), None => b.iconst(t, 0) }).collect_in(env.mcx);
    b.br(l_join, &old[..]);
    b.switch_to(l_join);
    mcx::to_vec_in(&b.block_params(l_join)[..], env.mcx)
}

pub fn exits(b: &mut FunctionBuilder<'_>) -> Exits {
    let x = Exits {
        l_finish: b.create_block(&[Type::I64, Type::I64]),
        l_stop_in: b.create_block(&[Type::I64, Type::I64, Type::I64]),
        l_full: b.create_block(&[Type::I64, Type::I64]),
        l_status: b.create_block(&[Type::I64]),
        l_done: b.create_block(&[]),
        l_raise: b.create_block(&[Type::I64]),
        l_cancel: b.create_block(&[]),
        l_exit: b.create_block(&[Type::I64]),
    };
    b.mark_cold(x.l_raise);
    b.mark_cold(x.l_cancel);
    x
}

/// A fresh call opens (`RESUME_NONE`); a resumed one goes straight to the window loop, which re-reads the window from the
/// cursor the stop wrote back (resume inside a window).
pub fn dispatch(b: &mut FunctionBuilder<'_>, env: &Env, l_open: Block, l_loop: Block) {
    let resume_w = b.load(Type::I64, env.ctx, hdr::RESUME as i32);
    let none = b.iconst(Type::I64, RESUME_NONE);
    let fresh = b.icmp(IntCond::Eq, resume_w, none);
    b.cond_br(fresh, (l_open, &[]), (l_loop, &[]));
}

pub fn mark_open(b: &mut FunctionBuilder<'_>, env: &Env) {
    let open = b.iconst(Type::I64, RESUME_OPEN);
    b.store(env.ctx, hdr::RESUME as i32, open);
}

/// `out` more rows emitted and the cursor after the last row, written to site `k`.
pub fn write_back(b: &mut FunctionBuilder<'_>, ctx: Value, k: usize, out: Value, cur: Value) {
    let s_off = site_off(k) as i32;
    let ro = b.load(Type::I64, ctx, s_off + site::ROWS_OUT as i32);
    let ro2 = b.int_bin(IntBinOp::Add, ro, out);
    b.store(ctx, s_off + site::ROWS_OUT as i32, ro2);
    let cur32 = b.conv(ConvOp::Trunc, Type::I32, cur);
    b.store(ctx, s_off + site::CURSOR as i32, cur32);
}

/// How a function writes its position back: a site's cursor, or the source's own (a breaker's emit).
pub enum WriteBack<'a> {
    Site(usize),
    Own(&'a mut dyn FnMut(&mut FunctionBuilder<'_>, &mut Env, Value, Value)),
}

/// The bodies: the window's end continues at `l_after`; a stop or raise writes back and returns the status; the count
/// reached writes back and continues at `l_full_then` (`done`, or D2580's flush of the steps after the one met: frame.rs
/// `pipeline_fn`); `finish` runs the end's section close before `done`; a cancel returns `CANCELLED`.
pub fn define_exits(b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits, (l_after, l_full_then): (Block, Block), mut wb: WriteBack<'_>, finish: &mut dyn FnMut(&mut FunctionBuilder<'_>, &mut Env)) {
    let (ctx, zero, stopped, done, node) = (env.ctx, env.zero, env.core.stopped, env.core.done, env.node);
    // EXPLAIN ANALYZE's rows leave wherever the position is written back: the window's end, a stop (an out block full, a
    // pull's yield) and the count reached, each the rows since the window's (or the resume's) start: `out` restarts at 0
    // on resume, so no row is counted twice (D349)
    let mut back = |b: &mut FunctionBuilder<'_>, env: &mut Env, out: Value, cur: Value| {
        instr::count_out(b, ctx, node, out);
        instr::count_end_out(b, ctx, out, env.mcx);
        instr::uncount_ahead(b, ctx, cur, env.mcx);
        match &mut wb {
            WriteBack::Site(k) => write_back(b, ctx, *k, out, cur),
            WriteBack::Own(f) => f(b, env, out, cur),
        }
    };
    // D528: an exit nothing branches to is not emitted (its block stays an empty `unreachable` `prune` empties). An exit's
    // body may enter an exit already passed over (`finish` / a write-back emits code that stops, raises or ends), so the
    // exits are visited in order until a round emits none: each is emitted once, after whatever enters it.
    let reached = |b: &FunctionBuilder<'_>, l: Block| crate::frame::frame::reached(b, l);
    let mut done_ = [false; 8];
    loop {
        let mut any = false;
        for (i, l) in [x.l_finish, x.l_stop_in, x.l_full, x.l_status, x.l_raise, x.l_cancel, x.l_done, x.l_exit].into_iter().enumerate() {
            if done_[i] || !reached(b, l) {
                continue;
            }
            (done_[i], any) = (true, true);
            b.switch_to(l);
            match i {
                0 => {
                    let (out, cur) = (b.block_params(l)[0], b.block_params(l)[1]);
                    back(b, env, out, cur);
                    b.br(l_after, &[]);
                }
                1 => {
                    let p = mcx::to_vec_in(&b.block_params(l)[..], env.mcx);
                    back(b, env, p[0], p[1]);
                    b.br(x.l_status, &[p[2]]);
                }
                2 => {
                    let (out, cur) = (b.block_params(l)[0], b.block_params(l)[1]);
                    back(b, env, out, cur);
                    b.br(l_full_then, &[]);
                }
                3 => {
                    let fl = b.block_params(l)[0];
                    let bad = b.icmp(IntCond::Slt, fl, zero);
                    let l_stop = b.create_block(&[]);
                    b.cond_br(bad, (x.l_exit, &[fl]), (l_stop, &[]));
                    b.switch_to(l_stop);
                    let st = b.call_host(stopped, &[ctx], Some(Type::I64));
                    exit_with(b, x, st);
                }
                4 => {
                    env.hir.raised(b);
                    let e = b.block_params(l)[0];
                    let st = raise_status(b, e);
                    b.br(x.l_exit, &[st]);
                }
                5 => {
                    let st = b.iconst(Type::I64, status::CANCELLED);
                    b.br(x.l_exit, &[st]);
                }
                6 => {
                    finish(b, env);
                    let st = b.call_host(done, &[ctx], Some(Type::I64));
                    exit_with(b, x, st);
                }
                _ => {
                    let st = b.block_params(l)[0];
                    b.ret(&[st]);
                }
            }
        }
        if !any {
            break;
        }
    }
}

fn exit_with(b: &mut FunctionBuilder<'_>, x: &Exits, st: Option<Value>) {
    match st {
        Some(st) => b.br(x.l_exit, &[st]),
        None => b.unreachable(),
    }
}

/// The row loop's raise block `(err, out, cursor)`: the cursor written back, the raise status returned. A faulting row is
/// never counted: `out` is the rows finished before it (the ENTRY §E3 contract).
pub fn raise_row(b: &mut FunctionBuilder<'_>, env: &Env, x: &Exits) -> Block {
    let l = b.create_block(&[Type::I64, Type::I64, Type::I64]);
    b.mark_cold(l);
    let cur = b.current();
    b.switch_to(l);
    env.hir.raised(b);
    let p = mcx::to_vec_in(&b.block_params(l)[..], env.mcx);
    let st = raise_status(b, p[0]);
    b.br(x.l_stop_in, &[p[1], p[2], st]);
    b.switch_to(cur);
    l
}

#[cfg(test)]
pub(crate) mod tests {
    use gravity_abi::ctx::{ctx_size, hdr};
    use gravity_interp::Interp;
    use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
    use gravity_ir::{Type, Value};

    use crate::frame::env::{self, Env};
    use crate::hir::{Ex, Ir};

    /// `f`'s `I64` result, emitted over a fresh function's `Env` (its exits unreachable) and run once by the interpreter
    /// (its bind words all 0).
    pub(crate) fn run1(exs: &[Ex<'_>], f: &mut dyn FnMut(&mut FunctionBuilder<'_>, &mut Env) -> Value) -> i64 {
        run_with(exs, &[], f)
    }

    /// `run1` with bind words `words` (none NULL).
    pub(crate) fn run_with(exs: &[Ex<'_>], words: &[u64], f: &mut dyn FnMut(&mut FunctionBuilder<'_>, &mut Env) -> Value) -> i64 {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let mut pb = ProgramBuilder::new();
        let core = env::declare(&mut pb);
        let func = pb.declare_func("t", &[Type::Ptr], &[Type::I64]);
        {
            let mut b = pb.define(func);
            let x = super::exits(&mut b);
            let mut env = Env::new(&mut b, core, Ir::none(), exs, x, rig.mcx());
            let v = f(&mut b, &mut env);
            b.ret(&[v]);
            for l in [x.l_finish, x.l_stop_in, x.l_full, x.l_status, x.l_done, x.l_raise, x.l_cancel, x.l_exit] {
                b.switch_to(l);
                b.unreachable();
            }
        }
        let p = pb.finish();
        let (mut ctx, mut params, mut nulls) = (vec![0u64; ctx_size(1) / 8], vec![0u64; 64], vec![0u64; 8]); // alloc: Program
        params[..words.len()].copy_from_slice(words);
        (ctx[hdr::PARAMS / 8], ctx[hdr::PARAMS_NULL / 8]) = (params.as_mut_ptr() as u64, nulls.as_mut_ptr() as u64);
        Interp::new(&p).run(func, &[ctx.as_mut_ptr() as u64]).expect("runs")[0] as i64
    }
}
