//! nest.spool's pulled child (M8-NEST, a helper of nest/spool.rs; split from it, the LTC-NEST-DRAIN design's one child):
//! a source and its ops made again inside a site-less reader over a site of their own, read on from where they stopped
//! until one row passes the ops. What a passing row does is the reader's own (`pass`: nest/spool.rs's pull form appends it
//! to the spool's record), so each reader brings its action and its window rule over this one child. The child opens at
//! its first read (its per-open words then) and keeps its site's cursor between reads; two per-execution words after the
//! reader's `st` hold its end and its open (zeroed at bind). Registers nothing. Owner M8-NEST.
use gravity_abi::ctx::site_off;
use gravity_abi::site::site;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, IntBinOp, IntCond, Type};
use mcx::{GrowIn, PgBox, PgVec};

use crate::face::source::SourceMech;
use crate::face::step::StepMech;
use crate::frame::env::Env;
use crate::frame::open::OpenWords;
use crate::frame::px::ExId;
use crate::frame::row::{Carry, RowVals};

/// A passing row's action (`chain`'s end): the carry it returns is the row's out (non-zero: it passed).
pub(crate) type Pass<'a, 'e> = &'a mut dyn FnMut(&mut FunctionBuilder<'_>, &mut Env<'e, '_>, &RowVals) -> Carry<'e>;

/// The pull form's child: its scan and ops over its own site, and its per-open words (written once, at its first read).
pub(crate) struct Child<'g> {
    pub(crate) src: PgBox<'g, dyn SourceMech + 'g>,
    pub(crate) steps: PgVec<'g, PgBox<'g, dyn StepMech + 'g>>,
    /// D3498: a Result child's one-time gates (rerun/inner_arms_gate.rs `compile`), checked at its open: a false or NULL one
    /// ends the child with no row
    pub(crate) gates: Vec<(i32, ExId)>,
    /// `Err(site)`: its scan's site; `Ok(pos)` (D3498): a Result's one row, its position the reader's word `pos` (`fill`)
    pub(crate) at: Result<i32, usize>,
    pub(crate) words: OpenWords<'g>,
    /// D2780: read on to the end of the window the child reached at each read, every passing row `pass`ed (lifetime/material.rs
    /// `pull_reads_on`: a calm fill), instead of stopping at the first that passes.
    pub(crate) rest: bool,
}

/// The child's ops over `row`, `fin` at their end.
pub(crate) fn chain<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, steps: &[PgBox<'_, dyn StepMech + '_>], row: &RowVals, fin: Pass<'_, 'e>) -> Carry<'e> {
    crate::stack_check(); // D3832
    match steps.split_first() {
        None => fin(b, env, row),
        Some((s, rest)) => s.lower(b, env, row, &mut |b, env, r| chain(b, env, rest, r, fin)),
    }
}

impl Child<'_> {
    /// The child read on until one row passes its ops, `pass` run on that row, then `l_ready`; at the child's end its end
    /// word (`st`) set and `l_eof(0)`; a status below zero `l_eof(status)`. The child opens at its first read (its per-open
    /// words then; its open word `st + 1`), `node` the reader's.
    pub(crate) fn fill<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, st: u16, node: i32, l_eof: Block, l_ready: Block, pass: Pass<'_, 'e>) {
        fill_in(b, env, (&*self.src, &self.steps, &self.words), (self.at, &self.gates), (st, node), (l_eof, l_ready), self.rest, pass);
    }
}

/// `Child::fill` over a source, its ops and per-open words: `Err(site)` a site's cursor, `Ok(pos)` a site-less source
/// (join.drain's drain under nest/spool_drain.rs) that keeps its position in its own word `pos` (`Env::pos`), writes it
/// back itself and may send an empty window (the cursor moved past records it does not want).
pub(crate) fn fill<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, child: (&dyn SourceMech, &[PgBox<'_, dyn StepMech + '_>], &OpenWords), at: Result<i32, usize>, st: (u16, i32), ends: (Block, Block), pass: Pass<'_, 'e>) {
    fill_in(b, env, child, (at, &[]), st, ends, false, pass);
}

/// `fill`, and with `rest` (D2780, a site's cursor only) the read goes on past the first passing row to the end of its
/// window: each row's cursor stored as it is read, every passing row `pass`ed, `l_ready` when one passed, else the next
/// window.
#[allow(clippy::too_many_arguments)]
fn fill_in<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, child: (&dyn SourceMech, &[PgBox<'_, dyn StepMech + '_>], &OpenWords), (at, gates): (Result<i32, usize>, &[(i32, ExId)]), (st, node): (u16, i32), (l_eof, l_ready): (Block, Block), rest: bool, pass: Pass<'_, 'e>) {
    let (src, steps, words) = child;
    let rest = rest && at.is_err();
    let saved = (env.site, env.win, env.pos);
    match at {
        Err(site) => env.site = Some(site),
        Ok(pos) => (env.site, env.pos) = (None, Some(pos)),
    }
    let memo = env.memo.set_aside();
    let (l_open, l_read) = (b.create_block(&[]), b.create_block(&[]));
    let opened = b.load(Type::I64, env.params, (st + 1) as i32 * 8);
    let fresh = b.icmp(IntCond::Eq, opened, env.zero);
    b.cond_br(fresh, (l_open, &[]), (l_read, &[]));
    b.switch_to(l_open);
    words.write_all(b, node, env.ctx, env.state, env.words);
    let l_shut = crate::rerun::inner_arms_gate::check(b, env, gates);
    let r = src.open(b, env);
    b.store(env.params, (st + 1) as i32 * 8, env.one);
    let (bad, l_opened) = (b.icmp(IntCond::Slt, r, env.zero), b.create_block(&[]));
    b.cond_br(bad, (l_eof, &[r]), (l_opened, &[]));
    b.switch_to(l_opened);
    b.br(l_read, &[]);
    // D3498: a false or NULL gate: the child opened and ended with no row (ExecResult's rs_done)
    if let Some(l) = l_shut {
        b.switch_to(l);
        b.store(env.params, (st + 1) as i32 * 8, env.one);
        b.store(env.params, st as i32 * 8, env.one);
        b.br(l_eof, &[env.zero]);
    }
    b.switch_to(l_read);
    let l_end = b.create_block(&[Type::I64]);
    // a filtered row reads the next: the loop's back edge checks for cancel
    let (l_loop, l_body) = crate::frame::cancel::header(b, env.ctx, env.l_cancel, &[], env.mcx);
    b.br(l_loop, &[]);
    b.switch_to(l_body);
    env.memo.clear();
    let win = src.next_window(b, env, l_end);
    env.win = Some(win);
    if at.is_ok() {
        let (l_empty, l_row) = (b.create_block(&[]), b.create_block(&[]));
        let empty = b.icmp(IntCond::Sge, win.cursor, win.end);
        b.cond_br(empty, (l_empty, &[]), (l_row, &[]));
        b.switch_to(l_empty);
        src.write_back(b, env, win.end, env.zero);
        b.br(l_loop, &[]);
        b.switch_to(l_row);
    }
    src.fill_late(b, env, &win, None);
    if rest {
        // D2780: every row of the window read here, the site's cursor past each as it is read
        let (l_rows, l_row, l_done) = (b.create_block(&[Type::I64, Type::I64]), b.create_block(&[]), b.create_block(&[Type::I64]));
        b.br(l_rows, &[win.cursor, env.zero]);
        // bounded by the window, inside the read loop's checked back edge (rerun/inner.rs's row loop does the same)
        b.mark_loop_header(l_rows);
        env.bounded.push_in(l_rows);
        b.switch_to(l_rows);
        let (c, got) = (b.block_params(l_rows)[0], b.block_params(l_rows)[1]);
        let more = b.icmp(IntCond::Slt, c, win.end);
        b.cond_br(more, (l_row, &[]), (l_done, &[got]));
        b.switch_to(l_row);
        env.memo.clear();
        let row = src.row(b, env, &win, c, Carry::new(env.zero, env.mcx), env.zero);
        let c1 = b.int_bin(IntBinOp::Add, c, env.one);
        if let Err(site) = at {
            let c32 = b.conv(ConvOp::Trunc, Type::I32, c1);
            b.store(env.ctx, site_off(site) as i32 + site::CURSOR as i32, c32);
        }
        let out = chain(b, env, steps, &row, pass);
        let found = b.icmp(IntCond::Ne, out.out, env.zero);
        let got1 = b.select(found, env.one, got);
        b.br(l_rows, &[c1, got1]);
        b.switch_to(l_done);
        let got = b.block_params(l_done)[0];
        let any = b.icmp(IntCond::Ne, got, env.zero);
        b.cond_br(any, (l_ready, &[]), (l_loop, &[]));
    } else {
        fill_one(b, env, (src, steps), at, &win, (l_loop, l_ready), pass);
    }
    b.switch_to(l_end);
    let r = b.block_params(l_end)[0];
    let (bad, l_ended) = (b.icmp(IntCond::Slt, r, env.zero), b.create_block(&[]));
    b.cond_br(bad, (l_eof, &[r]), (l_ended, &[]));
    b.switch_to(l_ended);
    b.store(env.params, st as i32 * 8, env.one);
    // D4204: a step that ends a state at its input's end (a lock's EvalPlanQual: ExecLockRows' EvalPlanQualEnd)
    steps.iter().filter(|s| s.ends()).for_each(|s| s.ended(b, env));
    b.br(l_eof, &[env.zero]);
    env.memo.restore(memo);
    (env.site, env.win, env.pos) = saved;
}

/// `fill`'s one row: the window's first, consumed at once (the site's cursor, or a site-less source's own position, past
/// it), its ops run; `l_ready` when it passed, else `l_loop` (the next read).
fn fill_one<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, (src, steps): (&dyn SourceMech, &[PgBox<'_, dyn StepMech + '_>]), at: Result<i32, usize>, win: &crate::frame::frame::Win, (l_loop, l_ready): (Block, Block), pass: Pass<'_, 'e>) {
    let row = src.row(b, env, win, win.cursor, Carry::new(env.zero, env.mcx), env.zero);
    let cur1 = b.int_bin(IntBinOp::Add, win.cursor, env.one);
    match at {
        Err(site) => {
            let c32 = b.conv(ConvOp::Trunc, Type::I32, cur1);
            b.store(env.ctx, site_off(site) as i32 + site::CURSOR as i32, c32);
        }
        Ok(_) => src.write_back(b, env, cur1, env.zero),
    }
    // D4204: a step that positions its site at the row (a pulled LOCK-FILL child's lock) reads its locator, as the frame's
    let saved = env.loc;
    env.loc = if steps.iter().any(|s| s.wants_locator()) { src.locator(b, env, &row) } else { None };
    let out = chain(b, env, steps, &row, pass);
    env.loc = saved;
    let found = b.icmp(IntCond::Ne, out.out, env.zero);
    b.cond_br(found, (l_ready, &[]), (l_loop, &[]));
}
