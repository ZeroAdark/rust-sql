//! D3553 (B): a LEFT / FULL probe lowers the steps after it once. PG's HJ_FILL_OUTER hands an outer row on joined to each
//! record that passes the join qual and, when none did, once with the inner columns NULL. join.inner lowered the rest of
//! the pipeline once for the hits (walk.rs `hand_on`) and again for the miss (probe_fill.rs `miss`), so n stacked LEFT
//! joins held 2^n copies of it (a 9-lookup LEFT JOIN: 25 s of compile). Here both enter one block, `l_cont`, whose
//! parameters are the payload words, their NULL flags and a miss tag: a hit passes the record's words, the miss passes
//! every word NULL. D688's resume word is kept: a hit stores its match count and the miss its mark before the block, as
//! before; the block goes back to the walk after a hit and past the fill after the miss. The early misses (probe_fill.rs
//! `entry`, D1904's empty kept table, D1908's NULL key or EMPTY build) enter the walk with no record, so the walk's header
//! dominates the block (the loop stays reducible); the walk's invariants (the image words, `exact`, D688's later word)
//! become its parameters. join.mult's LEFT probe shares its rest the same way, its multiplicity a parameter; the
//! single-row body never fills (`single_guard`). D3818: in a statement that writes or locks rows too: the block keeps the
//! image and payload row in reach of the EvalPlanQual redo (`Env::redo_probes`, `Env::redo_nests`), the miss tag its
//! run-time NULL-extended flag (`Filled::At`: the miss's payload NULL, its image words never read).
//! Only in a pipeline stacking `cost::compile::FILL_SHARE_MIN_PROBES` NULL-filling joins or more (`Gx::fills_share`: LEFT
//! nests count too, D3776): below it the copies are few and the shared block's parameters cost more per row than the
//! copies do.
//! Owner M7-PROBE (D3553, rxcontrib). Budget 200 lines.
use gravity_expr::emit::hash::{emit_image_eq_at, ImageValues};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, IntBinOp, IntCond, Type, Value};
use mcx::{CollectIn, GrowIn, Mcx, PgVec};

use crate::cell::{Cell, Nullable};
use crate::frame::env::Env;
use crate::frame::frame::Next;
use crate::frame::row::{Carry, RowVals};
use crate::ops::hashjoin::build::LOW;
use crate::ops::hashjoin::probe::{later_set, rec_at, Hv, Probe};
use crate::ops::hashjoin::probe_fill::FILLED;
use crate::ops::hashjoin::walk::Rec;
use crate::table::keyimg::ImageWords;

fn n_of(n: usize, v: Value) -> impl Iterator<Item = Value> {
    std::iter::repeat(v).take(n)
}

impl Probe<'_> {
    /// The row the rest runs on: `row`'s cells, then each kept column's record cell valued `words` / `nulls` (walk.rs
    /// `joined`, probe_fill.rs `miss`), and the INNER_VAR map (`Env::inner`).
    fn shared_row<'r, 'm>(&self, row: &RowVals<'r>, carry: &[Value], words: &[Value], nulls: &[Value], mcx: Mcx<'m>) -> (RowVals<'r>, PgVec<'m, Option<u16>>) {
        let m = *row.vals.allocator();
        let mut r = RowVals { row: crate::cell::Row { cells: mcx::to_vec_in(&row.row.cells[..], m) }, vals: mcx::to_vec_in(&row.vals[..], m), cols: mcx::to_vec_in(&row.cols[..], m), carry: Carry::from_params(carry, m), next: row.at, at: row.at };
        let base = r.row.cells.len();
        for (i, c) in self.sh.kept.borrow().iter().enumerate() {
            r.row.cells.push_in(Cell::of_record(c.rep, c.typid, c.num, crate::ops::hashjoin::build_side::home_of(c, self.life), i as u16, Nullable::Maybe));
            r.vals.push_in((words[i], Some(nulls[i])));
        }
        let width = self.sh.payload.iter().copied().max().unwrap_or(0).max(0) as usize;
        let mut map = mcx::pg_vec![in mcx; None; width];
        for (i, &a) in self.sh.payload.iter().enumerate() {
            map[a as usize - 1] = Some((base + i) as u16);
        }
        (r, map)
    }

    /// The fill (probe_fill.rs `miss`'s): unless the offset word says the row went on (a hit, a resumed row) and no later
    /// expanding step stopped inside it (D688), the word takes its mark and the row enters `l_cont` as `args` names
    /// (`true`: the NULL flags' and the tag's value); `l_after` past it.
    fn fill_to<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, ce: &Carry, (l_cont, l_after): (Block, Block), args: &dyn Fn(Value, &Carry) -> PgVec<'e, Value>) {
        let l_fill = b.create_block(&[]);
        let off = env.load_word(b, self.offset);
        let fresh = b.icmp(IntCond::Eq, off, env.zero);
        let fresh = later_set(b, env).map_or(fresh, |x| b.int_bin(IntBinOp::Or, fresh, x));
        b.cond_br(fresh, (l_fill, &[]), (l_after, &ce.params()));
        b.switch_to(l_fill);
        let mark = if self.fill.is_some_and(|f| f.tag) { b.iconst(Type::I64, FILLED) } else { env.one };
        env.store_word(b, self.offset, mark);
        let t = b.bconst(true);
        b.br(l_cont, &args(t, ce)[..]);
    }

    /// D3818: the shared block's record in reach of a write's or lock's EvalPlanQual redo, as walk.rs `hand_on` and
    /// probe_fill.rs `miss` keep it (none for a swapped probe, whose record is the outer side): the image the hit matched
    /// (`words`, the walk's; the miss's are never read) and the payload row of `ks` (the block's words and NULL flags),
    /// NULL-extended when `tag` is set (`Filled::At`). Whether it pushed.
    fn held(&self, env: &mut Env, words: &ImageValues, ks: &[Value], tag: Value) -> bool {
        if self.sh.swapped {
            return false;
        }
        let (nk, mcx) = (self.sh.kept.borrow().len(), env.mcx);
        let cells: PgVec<'_, Cell> = self.sh.kept.borrow().iter().enumerate().map(|(i, c)| Cell::of_record(c.rep, c.typid, c.num, crate::ops::hashjoin::build_side::home_of(c, self.life), i as u16, Nullable::Maybe)).collect_in(mcx);
        let vals: PgVec<'_, (Value, Option<Value>)> = (0..nk).map(|i| (ks[i], Some(ks[nk + i]))).collect_in(mcx);
        let width = self.sh.payload.iter().copied().max().unwrap_or(0).max(0) as usize;
        let mut cols = mcx::pg_vec![in mcx; None; width];
        for (i, &a) in self.sh.payload.iter().enumerate() {
            cols[a as usize - 1] = Some(i as u16);
        }
        env.redo_probes.push_in((self.node, mcx::to_vec_in(&words.words[..], mcx)));
        env.redo_nests.push_in((self.node, (cells, vals, cols, crate::frame::env::Filled::At(tag))));
        true
    }

    /// The rest, lowered once in `l_cont` on the row `shared_row` makes of its parameters `ks` (the payload words, their
    /// NULL flags, then `skip` values the caller reads, then the carry), the memo the outer rows only. Its carry.
    fn rest<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>, ks: &[Value], skip: usize) -> Carry<'e> {
        let nk = self.sh.kept.borrow().len();
        let (r, map) = self.shared_row(row, &ks[2 * nk + skip..], &ks[..nk], &ks[nk..2 * nk], env.mcx);
        let memo = env.memo.fork();
        let saved = env.inner.replace(map);
        let c = next(b, env, &r);
        env.inner = saved;
        env.memo.restore(memo);
        c
    }

    /// probe.rs `lower`'s walk for a sharing probe, from the row's image on: the chain, the fill and the one rest.
    /// `kept`: the memo set aside before the keys (it comes back at the walk, which the early misses enter too).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn lower_shared<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, hv: &Hv, img: &ImageWords, row: &RowVals, next: Next<'_, 'e>, (l_join, l_miss): (Block, Block), kept: Option<crate::lx::SetAside<'e>>) -> Carry<'e> {
        let mcx = env.mcx;
        let ctypes: PgVec<'_, Type> = row.carry.params().iter().map(|&v| b.value_type(v)).collect_in(mcx);
        let (p0, exact0) = self.first(b, env, hv, img, true);
        let back0 = later_set(b, env).map(|x| b.conv(ConvOp::Zext, Type::I64, x));
        let nw = img.words.words.len();
        let ni = nw + 1 + back0.is_some() as usize;
        let mut wtypes = mcx::pg_vec![in mcx; Type::I64; 2 + nw];
        wtypes.push_in(Type::Bool);
        wtypes.extend_in(back0.map(|_| Type::I64));
        wtypes.extend_from_slice_in(&ctypes[..]);
        // the chain: (record address or 0, matches so far, the image words, exact, D688's later word, carry)
        let l_walk = b.create_block(&wtypes[..]);
        let start: PgVec<'_, Value> = [p0, env.zero].into_iter().chain(img.words.words.iter().copied()).chain([exact0]).chain(back0).chain(row.carry.params()).collect_in(mcx);
        b.br(l_walk, &start[..]);
        if let Some(m) = kept {
            env.memo.restore(m);
        }
        b.switch_to(l_miss);
        let f = b.bconst(false);
        let early: PgVec<'_, Value> = n_of(2 + nw, env.zero).chain([f]).chain(back0.map(|_| env.zero)).chain(b.block_params(l_miss).iter().copied()).collect_in(mcx);
        b.br(l_walk, &early[..]);
        b.switch_to(l_walk);
        b.mark_loop_header(l_walk);
        env.bounded.push_in(l_walk);
        let ps = mcx::to_vec_in(b.block_params(l_walk), mcx);
        let (p, m, inv, carry) = (ps[0], ps[1], &ps[2..2 + ni], Carry::from_params(&ps[2 + ni..], env.mcx));
        let words = ImageValues { words: inv[..nw].to_vec() }; // alloc: Program
        let (exact, back) = (inv[nw], back0.map(|_| inv[nw + 1]));
        let on = |p: Value, m: Value, c: &Carry| { [p, m].into_iter().chain(inv.iter().copied()).chain(c.params()).collect_in(mcx) };
        let (l_rec, l_end) = (b.create_block(&[]), b.create_block(&ctypes));
        let none = b.icmp(IntCond::Eq, p, env.zero);
        b.cond_br(none, (l_end, &carry.params()), (l_rec, &[]));
        b.switch_to(l_rec);
        let rec = b.conv(ConvOp::IntToPtr, Type::Ptr, p);
        let link = b.load(Type::I64, rec, self.sh.link_off());
        let low = b.iconst(Type::I64, LOW as i64);
        let n1 = b.int_bin(IntBinOp::And, link, low);
        let np = rec_at(b, env, hv, n1, self.sh.rec.get());
        let nz = b.icmp(IntCond::Eq, n1, env.zero);
        let next_p = b.select(nz, env.zero, np);
        let eq = emit_image_eq_at(b, &words, rec, 0);
        let eq = b.int_bin(IntBinOp::Or, eq, exact);
        let m1 = b.int_bin(IntBinOp::Add, m, env.one);
        let (l_hit, l_emit) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(eq, (l_hit, &[]), (l_walk, &on(next_p, m, &carry)));
        b.switch_to(l_hit);
        let off = env.load_word(b, self.offset);
        let off = back.map_or(off, |x| b.int_bin(IntBinOp::Sub, off, x));
        let resumed = b.icmp(IntCond::Slt, m, off);
        b.cond_br(resumed, (l_walk, &on(next_p, m1, &carry)), (l_emit, &[]));
        b.switch_to(l_emit);
        // rec-0013 (probe.rs `lower`): each match checks for interrupts
        crate::frame::cancel::check(b, env.ctx, env.l_cancel);
        // the rest's parameters: the payload words, their NULL flags, the miss tag, the next record, the matches, the carry
        let nk = self.sh.kept.borrow().len();
        let mut ktypes = mcx::pg_vec![in mcx; Type::I64; nk];
        ktypes.extend_in(std::iter::repeat(Type::Bool).take(nk + 1));
        ktypes.extend_in([Type::I64, Type::I64]);
        ktypes.extend_from_slice_in(&ctypes[..]);
        let l_cont = b.create_block(&ktypes[..]);
        let (base, f) = (row.row.cells.len(), b.bconst(false));
        let mut jump = |b: &mut FunctionBuilder<'_>, _: &mut Env<'e, '_>, r: &RowVals| -> Carry<'e> {
            let pay = &r.vals[base..base + nk];
            let args: PgVec<'_, Value> = pay.iter().map(|v| v.0).chain(pay.iter().map(|v| v.1.unwrap_or(f))).chain([f, next_p, m1]).chain(r.carry.params()).collect_in(mcx);
            b.br(l_cont, &args[..]);
            r.carry.copy_in(mcx)
        };
        let hit = env.memo.fork();
        self.hand_on(b, env, hv, row, (Rec::At(rec, link), &carry), Some(m1), (l_walk, &on(next_p, m1, &carry)), &mut jump);
        env.memo.restore(hit);
        b.switch_to(l_end);
        let ce = Carry::from_params(b.block_params(l_end), env.mcx);
        env.redo_probes.pop();
        let l_after = b.create_block(&ctypes);
        let zero = env.zero;
        self.fill_to(b, env, &ce, (l_cont, l_after), &|t, c| n_of(nk, zero).chain(n_of(nk + 1, t)).chain([zero, zero]).chain(c.params()).collect_in(mcx));
        b.switch_to(l_cont);
        let ks = mcx::to_vec_in(b.block_params(l_cont), mcx);
        let (tag, np, mk) = (ks[2 * nk], ks[2 * nk + 1], ks[2 * nk + 2]);
        let held = self.held(env, &words, &ks[..], tag);
        let c = self.rest(b, env, row, next, &ks[..], 3);
        if held {
            env.redo_nests.pop();
            env.redo_probes.pop();
        }
        // a unique build's hit ends the walk (PG's single_match): the walk's next record is none
        let np = if self.unique { env.zero } else { np };
        b.cond_br(tag, (l_after, &c.params()), (l_walk, &on(np, mk, &c)));
        b.switch_to(l_after);
        let ca = Carry::from_params(b.block_params(l_after), env.mcx);
        env.store_word(b, self.offset, env.zero);
        b.br(l_join, &ca.params());
        b.switch_to(l_join);
        Carry::from_params(b.block_params(l_join), env.mcx)
    }

    /// walk.rs `lower_mult`'s LEFT probe sharing its rest: the hit enters it with the key's multiplicity, the miss with the
    /// outer one (1 when none). Its payload is empty (multiplicity.rs `candidate`), so the rows differ in nothing else.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn lower_mult_shared<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, (w, ctypes): (Value, &[Type]), row: &RowVals, next: Next<'_, 'e>, (l_join, l_miss): (Block, Block), kept: Option<crate::lx::SetAside<'e>>) -> Carry<'e> {
        let mcx = env.mcx;
        let none = b.icmp(IntCond::Eq, w, env.zero);
        let mut ktypes = mcx::pg_vec![in mcx; Type::I64, Type::Bool];
        ktypes.extend_from_slice_in(ctypes);
        let (l_cont, l_go, l_after) = (b.create_block(&ktypes[..]), b.create_block(&[]), b.create_block(ctypes));
        b.cond_br(none, (l_miss, &row.carry.params()), (l_go, &[]));
        b.switch_to(l_go);
        let m = env.mult.map_or(w, |m0| b.int_bin(IntBinOp::Mul, m0, w));
        let f = b.bconst(false);
        let args: PgVec<'_, Value> = [m, f].into_iter().chain(row.carry.params()).collect_in(mcx);
        b.br(l_cont, &args[..]);
        if let Some(m) = kept {
            env.memo.restore(m);
        }
        b.switch_to(l_miss);
        let ce = Carry::from_params(b.block_params(l_miss), env.mcx);
        let m0 = env.mult.unwrap_or(env.one);
        self.fill_to(b, env, &ce, (l_cont, l_after), &|t, c| [m0, t].into_iter().chain(c.params()).collect_in(mcx));
        b.switch_to(l_cont);
        let ks = mcx::to_vec_in(b.block_params(l_cont), mcx);
        let saved = env.mult.replace(ks[0]);
        let c = self.rest(b, env, row, next, &ks[..], 2);
        env.mult = saved;
        b.cond_br(ks[1], (l_after, &c.params()), (l_join, &c.params()));
        b.switch_to(l_after);
        let ca = Carry::from_params(b.block_params(l_after), env.mcx);
        env.store_word(b, self.offset, env.zero);
        b.br(l_join, &ca.params());
        b.switch_to(l_join);
        Carry::from_params(b.block_params(l_join), env.mcx)
    }
}
