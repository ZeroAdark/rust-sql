//! Kind `nest.memo`'s rows (MC-1, M8-MEMO-CTE): the fill and the replay of one cache entry. nest/memo.rs makes the memo
//! and does the lookup; this file stores a miss's rows as the inner hands them on (`Fill`, MEMO_FILLING_CACHE; a store
//! the table cannot keep turns to MEMO_CACHE_BYPASS_MODE) and completes the entry at the inner's end (`Memo::done`), and
//! replays a hit's rows through the steps past the Memoize (`Memo::enter` / `replay`, MEMO_CACHE_FETCH_NEXT_TUPLE).
//! Owner M8-MEMO-CTE.
use gravity_decompose::catalog::typefacts;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, IntBinOp, IntCond, Type, Value};
use gravity_kernels::ops::shared::object::entries as abi;
use gravity_program::{NumForm, Rep, Until};
use mcx::{CollectIn, GrowIn};

use crate::ops::memoize::lower::{byref_len, from_word, host, image_len, raise_if_bad, row_tag, shape, to_word, Fill, Memo, Replay, BYPASS, FILLING, REPLAY};
use crate::cell::{At, Cell, CellHome, CellLife, Nullable};
use crate::face::step::StepMech;
use crate::frame::env::Env;
use crate::frame::frame::Next;
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::admit::refusal::Reason;
use crate::rep;

impl Memo<'_> {
    /// At the walk's entry: a resume past the row handed on (`skip` 1) clears the stored-row flag, a re-entry keeps it;
    /// then a hit branches to the replay block returned (`[skip, carry..]`, lowered by `replay`), else the site's walk.
    pub(crate) fn enter(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, htypes: &[Type], (skip, carry): (Value, &Carry)) -> Block {
        let c = &*self.core;
        let p = env.load_word(b, c.pend);
        let keep = b.icmp(IntCond::Eq, skip, env.zero);
        let p2 = b.select(keep, p, env.zero);
        env.store_word(b, c.pend, p2);
        let s = env.load_word(b, c.state);
        let rp = b.iconst(Type::I64, REPLAY);
        let replaying = b.icmp(IntCond::Eq, s, rp);
        let (l_r, l_site) = (b.create_block(htypes), b.create_block(&[]));
        b.cond_br(replaying, (l_r, &mcx::concat_in(&[&[skip][..], &carry.params()[..]], env.mcx)), (l_site, &[]));
        b.switch_to(l_site);
        l_r
    }

    /// The replay's first record (the position, `skip` past it) and the entry's row count.
    pub(crate) fn bounds(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, skip: Value) -> (Value, Value) {
        let c = &*self.core;
        let pos = env.load_word(b, c.pos);
        let start = b.int_bin(IntBinOp::Add, pos, skip);
        let at = env.load_word(b, c.hit);
        let p = b.conv(ConvOp::IntToPtr, Type::Ptr, at);
        (start, b.load(Type::I64, p, 0))
    }

    /// The replay's position: record `j` handed on.
    pub(crate) fn set_pos(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, j: Value) {
        env.store_word(b, self.core.pos, j);
    }

    /// Record `j` of the hit as the row at the Memoize: each stored datum as an `At::Value` cell of its column.
    pub(crate) fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        let (c, mcx) = (&*self.core, env.mcx);
        let held = c.shape.borrow();
        let Some((cells, types, cols)) = held.as_ref() else { shape(c.node, "a Memoize whose inner hands on no row (its replayed row's shape)") };
        let ncols = cells.len() as i64;
        let at = env.load_word(b, c.hit);
        let p = b.conv(ConvOp::IntToPtr, Type::Ptr, at);
        let w = b.iconst(Type::I64, ncols * 16);
        let off = b.int_bin(IntBinOp::Mul, j, w);
        let rec = b.ptr_add(p, off, 1, (abi::MEMO_HIT_HEAD * 8) as i32);
        let mut vals = mcx::reserved_in(cells.len(), mcx);
        let mut out = mcx::reserved_in(cells.len(), mcx);
        for (i, (cell, &ty)) in cells.iter().zip(types.iter()).enumerate() {
            let dw = b.load(Type::I64, rec, 16 * i as i32);
            let nw = b.load(Type::I64, rec, 16 * i as i32 + 8);
            let null = b.icmp(IntCond::Ne, nw, env.zero);
            vals.push_in((from_word(b, dw, ty), Some(null)));
            out.push_in(Cell { life: CellLife { until: Until::Row, home: CellHome::FrameSlot }, at: At::Value(i as u16), null: Nullable::Maybe, ..*cell });
        }
        RowVals { row: crate::cell::Row { cells: out }, vals, cols: mcx::to_vec_in(&cols[..], mcx), carry, next, at: j }
    }

    /// The replay loop from `l_r` (`[skip, carry..]`, types `htypes`): record `j` from the position on through `each`
    /// (the steps past the Memoize, then the owner's continuation), the interrupt checked per row, to `l_done` at the
    /// entry's end.
    pub(crate) fn replay<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, x: &Exits, (l_r, htypes, l_done): (Block, &[Type], Block), each: Replay<'_, 'e>) {
        b.switch_to(l_r);
        let p = mcx::to_vec_in(&b.block_params(l_r)[..], env.mcx);
        let (start, n) = self.bounds(b, env, p[0]);
        let (l_rh, l_row) = crate::frame::cancel::header(b, env.ctx, x.l_cancel, htypes, env.mcx);
        b.br(l_rh, &mcx::concat_in(&[&[start][..], &p[1..]], env.mcx));
        b.switch_to(l_row);
        let q = mcx::to_vec_in(&b.block_params(l_row)[..], env.mcx);
        let (j, rc) = (q[0], Carry::from_params(&q[1..], env.mcx));
        let l_body = b.create_block(&[]);
        let more = b.icmp(IntCond::Slt, j, n);
        b.cond_br(more, (l_body, &[]), (l_done, &rc.params()));
        b.switch_to(l_body);
        env.memo.clear();
        let past = b.int_bin(IntBinOp::Add, j, env.one);
        let row = self.row(b, env, j, rc, past);
        let c = each(b, env, j, &row);
        b.br(l_rh, &mcx::concat_in(&[&[past][..], &c.params()[..]], env.mcx));
    }

    /// The inner's window loop at its source's status `r` (`end`: status 0): at the end the entry filled is complete
    /// (none in bypass: the table ignores it) and the walk goes to `l_done`; any other status exits with it.
    pub(crate) fn done(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, (end, r, l_exit): (Value, Value, Block), (l_done, wc): (Block, &Carry)) {
        let l_end = b.create_block(&[]);
        b.cond_br(end, (l_end, &[]), (l_exit, &[r]));
        b.switch_to(l_end);
        self.complete(b, env);
        b.br(l_done, &wc.params());
    }

    /// MEMO_FILLING_CACHE's end (`m8_memo_done`): the entry filled is complete; only where the inner handed on its last
    /// row (its source's end, a LIMIT's, a once source's one window walked through), never at a walk the consumer cut short.
    pub(crate) fn complete(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        let c = &*self.core;
        let hv = b.load(Type::I64, env.params, c.h as i32 * 8);
        let f = host(b, "m8_memo_done", &[Type::Ptr, Type::I64], gravity_abi::host_words!(abi::m8_memo_done(ctx, h)));
        let r = b.call_host(f, &[env.ctx, hv], Some(Type::I64)).unwrap_or(env.zero);
        raise_if_bad(b, env, r);
    }
}

impl StepMech for Fill<'_> {
    fn open_words(&self, w: &mut OpenWords) {
        let c = &*self.core;
        w.declare(OpenWord::open("nest.memo state", Base::Words, c.state, Type::I64, 0));
        w.declare(OpenWord::open("nest.memo stored row", Base::Words, c.pend, Type::I64, 0));
        w.declare(OpenWord::open("nest.memo hit", Base::Words, c.hit, Type::I64, 0));
        w.declare(OpenWord::open("nest.memo replay position", Base::Words, c.pos, Type::I64, 0));
    }

    /// MEMO_FILLING_CACHE: the row stored (unless in bypass, or re-entered after a stop), then handed on; a pgrc inner's
    /// dictionary and window cells both stored and handed on site-free (`site_free`), so a hit's replay hands on the forms
    /// a miss's row does.
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        let c = &*self.core;
        let plain = site_free(b, env, c.node, row);
        let row = plain.as_ref().unwrap_or(row);
        let ncols = row.row.cells.len();
        let mut ds = mcx::reserved_in(ncols, env.mcx);
        let mut cx = env.lx(b, c.node);
        for k in 0..ncols {
            let Some(cr) = row.cell(k) else { crate::refuse!(c.node, Reason::Bug { payload: "a memo row cell past the row" }) };
            let bl = byref_len(c.node, cr.cell);
            // the datum arm reads only the cell's Rep (step/expr.rs `tf_datum`'s row)
            let tf = typefacts::row_of(cr.cell.typid).copied().unwrap_or(typefacts::opaque(cr.cell.typid, cr.cell.typid, -1, false, false));
            let (d, null) = match cr.cell.rep {
                // a kept order word (`site_free`'s, a sort's) is stored as the word: its datum is an image valid for the row only
                Rep::PackedNum { form: NumForm::Word, .. } => (cr.val, cr.null.unwrap_or_else(|| cx.b.bconst(false))),
                _ => rep::datum(&mut cx, cr, &tf),
            };
            ds.push_in((d, null, bl));
        }
        env.memo = cx.memo;
        {
            let types: mcx::PgVec<'_, Type> = ds.iter().map(|&(d, _, _)| b.value_type(d)).collect_in(c.mcx);
            let mut s = c.shape.borrow_mut();
            match s.as_ref() {
                None => *s = Some((mcx::to_vec_in(&row.row.cells[..], c.mcx), types, mcx::to_vec_in(&row.cols[..], c.mcx))),
                Some((cells, t, _)) if cells.len() != ncols || *t != types => shape(c.node, "a Memoize whose rows differ in form between its inner's paths"),
                Some(_) => {}
            }
        }
        // store when filling and this row is not the one a stop re-entered (D1018 tail: a set function below tags its rows)
        let st = env.load_word(b, c.state);
        let fi = b.iconst(Type::I64, FILLING);
        let filling = b.icmp(IntCond::Eq, st, fi);
        let (pend, tag) = (env.load_word(b, c.pend), row_tag(b, env, c.node));
        let fresh = if let Some(t) = tag { b.icmp(IntCond::Ne, pend, t) } else { b.icmp(IntCond::Eq, pend, env.zero) };
        let go = b.int_bin(IntBinOp::And, filling, fresh);
        let (l_store, l_on) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(go, (l_store, &[]), (l_on, &[]));
        b.switch_to(l_store);
        let slot = b.declare_stack_slot(((abi::MEMO_ROW_HEAD + ncols * abi::MEMO_ROW_WORDS) * 8) as u32, 8);
        let rb = b.stack_addr(slot);
        let nc = b.iconst(Type::I64, ncols as i64);
        b.store(rb, 0, nc);
        for (i, &(d, null, bl)) in ds.iter().enumerate() {
            let len = match bl {
                Some(tl) => image_len(b, d, null, tl),
                None => env.zero,
            };
            let off = ((abi::MEMO_ROW_HEAD + i * abi::MEMO_ROW_WORDS) * 8) as i32;
            let w = to_word(b, d);
            let nw = b.conv(ConvOp::Zext, Type::I64, null);
            b.store(rb, off, w);
            b.store(rb, off + 8, nw);
            b.store(rb, off + 16, len);
        }
        let hv = b.load(Type::I64, env.params, c.h as i32 * 8);
        let f = host(b, "m8_memo_store", &[Type::Ptr, Type::I64, Type::Ptr], gravity_abi::host_words!(abi::m8_memo_store(ctx, h, row)));
        let r = b.call_host(f, &[env.ctx, hv, rb], Some(Type::I64)).unwrap_or(env.zero);
        raise_if_bad(b, env, r);
        // 0: the entry is gone, bypass until the next re-open
        let kept = b.icmp(IntCond::Ne, r, env.zero);
        let (fi2, by) = (b.iconst(Type::I64, FILLING), b.iconst(Type::I64, BYPASS));
        let s2 = b.select(kept, fi2, by);
        env.store_word(b, c.state, s2);
        b.br(l_on, &[]);
        b.switch_to(l_on);
        env.store_word(b, c.pend, tag.unwrap_or(env.one));
        let out = next(b, env, row);
        env.store_word(b, c.pend, env.zero);
        out
    }
}

/// memoize, gen M2 rows dict-0214 / dict-0219: a pgrc inner's dictionary (`Rep::Code`) and window (`PackedNum` `Window`)
/// cells read through the inner's site, which a cache entry outlives (the next outer row's scan reads other windows): each
/// is made site-free here, in the fill under the inner's site, as nest.loop's `site_free` makes it for the owner's
/// continuation: a code its rendered varlena (the dictionary's resident image, copied into the entry like any varlena), a
/// window element its order word (the packed arm's retain, a by-value word). `None` when the row holds no such cell.
fn site_free<'r>(b: &mut FunctionBuilder<'_>, env: &mut Env, node: i32, r: &RowVals<'r>) -> Option<RowVals<'r>> {
    let sited = |c: &Cell| matches!(c.rep, Rep::Code { .. } | Rep::PackedNum { form: NumForm::Window, .. } | Rep::PackedNumDyn);
    if !r.row.cells.iter().any(sited) {
        return None;
    }
    let (mut cells, mut vals) = (r.row.cells.clone(), r.vals.clone());
    {
        let mut cx = env.lx(b, node);
        for (c, v) in cells.iter_mut().zip(vals.iter_mut()) {
            if !sited(c) {
                continue;
            }
            let tf = crate::step::expr::tf_datum(c.typid);
            let cr = rep::CellRef { cell: &*c, val: v.0, null: v.1 };
            let (cell, val) = if matches!(c.rep, Rep::Code { .. }) {
                let (d, null) = rep::datum(&mut cx, cr, &tf);
                let life = CellLife { until: Until::Row, home: CellHome::FrameSlot };
                (Cell { rep: Rep::VarlenaPtr, life, at: rep::word::datum::off_lane(c.at), null: Nullable::Maybe, ..*c }, (d, Some(null)))
            } else {
                // the arm itself, not `rep::retain`: a window cell's life already reaches the row, so that keeps the lane cell
                let k = (rep::packednum::datum::ARM.retain)(&mut cx, cr, &tf, Until::Row, rep::RetainHome::Frame);
                (k.cell, (k.val, k.null))
            };
            *c = cell;
            *v = val;
        }
        env.memo = cx.memo;
    }
    Some(RowVals { row: crate::cell::Row { cells }, vals, cols: r.cols.clone(), carry: r.carry.copy_in(*r.vals.allocator()), next: r.next, at: r.at })
}
