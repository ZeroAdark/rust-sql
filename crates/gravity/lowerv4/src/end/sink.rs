//! The out-block sink, kind `end.sink` (KEEP lowerv3 scan/sink.rs): the loop itself stores the row's out columns into the
//! out block (values, null bytes, `count` bumped). A by-reference column's image is copied into the block's region first,
//! and the block is pushed early when the images do not fit. `push` runs only at a full block, and the receiver's stop is
//! honoured after it. Every stop of the sink leaves with its row stored and the cursor past it (`env.x.l_stop_in`, where a
//! LIMIT above counts the row, D357). A parallel sink pipeline stores the same way into its worker's own out block; the
//! claim-order merge is sink.ordered's (sink_ordered.rs). Budget 250 lines.
use gravity_abi::ctx::{hdr, out};
use gravity_abi::status::raise;
use gravity_expr::byref::{self, ByRef};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, HostFlags, IntBinOp, IntCond, Type, Value};
use mcx::{GrowIn, PgBox, PgVec};

use crate::face::end::{EndMech, SinkKind};
use crate::face::kinds::KindId;
use crate::frame::env::Env;
use crate::frame::frame::EndMaker;
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::rep;

use crate::end::output as end_output;
use crate::end::output::Col;

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { kinds: &[(KindId::EndSink, &EndMaker(make))], ..crate::ops::NONE };

pub(crate) struct Sink<'g> {
    cols: PgVec<'g, Col>,
    node: i32,
    form: SinkKind,
}

fn make<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    let node = px.gx.pipelines()[k].node_id;
    let cols = end_output::declare(px, k, crate::ops::lockrows::plan::out_rows(px.gx)); // D4205: one row a push under row locks
    let form = crate::end::strategies::sink_ordered::form(px, k);
    mcx::pg_box_dyn!(px.mcx(), Sink { cols, node, form } => dyn EndMech + 'g)
}

/// The loops a helper emitted are bounded by their data (an image's length), not by a window: the cancel verifier
/// accepts them.
fn bounded_since(b: &FunctionBuilder<'_>, env: &mut Env, n0: usize) {
    env.bounded.extend_in((n0..b.num_blocks()).map(|i| Block(i as u32)));
}

impl Sink<'_> {
    /// Each out column's datum and validity, through the `datum` accessor.
    fn datums<'g>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'g, '_>, row: &RowVals) -> PgVec<'g, (Value, Value)> {
        let t = b.bconst(true);
        let mcx = env.mcx;
        let mut cx = env.lx(b, self.node);
        let mut v = mcx::reserved_in(self.cols.len(), mcx);
        for c in self.cols.iter() {
            let Some(cr) = row.cell(c.entry) else { crate::refuse!(self.node, crate::admit::refusal::Reason::Bug { payload: "an out column past the row" }) };
            let (d, null) = rep::datum(&mut cx, cr, &c.tf);
            let valid = cx.b.int_bin(IntBinOp::Xor, null, t);
            v.push_in((d, valid));
        }
        env.memo = cx.memo;
        v
    }

    /// The by-reference columns' region bytes: a row that does not fit behind the block's rows pushes the block first, and
    /// a row wider than the whole region (D645) pushes the rows before it, then has the driver swap the region for one that
    /// holds it (`out_grow`, drawn from the grant; its OutOfMemory is the host raise). The row is then stored into the block
    /// that push emptied, also when the receiver stops: the stop (returned, 0 for none) leaves after the row (D357: leaving
    /// before it re-read the row, twice under a probe's walk, whose next is its probe row). A failed push leaves at the row,
    /// uncounted.
    fn place(&self, b: &mut FunctionBuilder<'_>, env: &Env, o: Value, refs: &[ByRef], row: &RowVals, x: &Exits) -> (Vec<byref::Size>, Value) {
        let (per, total) = byref::sizes(b, refs);
        let over = byref::oversize(b, o, total);
        let (l_fit, l_over) = (b.create_block(&[]), b.create_block(&[]));
        let l_store = b.create_block(&[Type::I64]);
        b.cond_br(over, (l_over, &[]), (l_fit, &[]));
        b.switch_to(l_over);
        self.grow(b, env, o, total, row, x, l_store);
        b.switch_to(l_fit);
        let fits = byref::fits(b, o, total);
        let (l_push, l_fail) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(fits, (l_store, &[env.zero]), (l_push, &[]));
        b.switch_to(l_push);
        let fl = b.call_host(env.core.push, &[env.ctx], Some(Type::I64)).unwrap_or(env.zero);
        let bad = b.icmp(IntCond::Slt, fl, env.zero);
        b.cond_br(bad, (l_fail, &[]), (l_store, &[fl]));
        b.switch_to(l_fail);
        b.br(x.l_stop_in, &[row.carry.out, row.at, fl]);
        b.switch_to(l_store);
        (per, b.block_params(l_store)[0])
    }

    /// D645, the cold arm of a row wider than the whole region: the block's rows (their images in this region) pushed when
    /// there are any (a failed push leaves at the row, uncounted, as `place`'s); then the region regrown to hold `total`
    /// (`gravity_abi::program::out_grow`), its failure the host raise; on to `l_store` with the push's stop (0 when nothing
    /// was pushed).
    fn grow(&self, b: &mut FunctionBuilder<'_>, env: &Env, o: Value, total: Value, row: &RowVals, x: &Exits, l_store: Block) {
        let count = b.load(Type::I64, o, out::COUNT as i32);
        let held = b.icmp(IntCond::Ne, count, env.zero);
        let (l_flush, l_fail, l_grow) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::I64]));
        b.cond_br(held, (l_flush, &[]), (l_grow, &[env.zero]));
        b.switch_to(l_flush);
        let fl = b.call_host(env.core.push, &[env.ctx], Some(Type::I64)).unwrap_or(env.zero);
        let bad = b.icmp(IntCond::Slt, fl, env.zero);
        b.cond_br(bad, (l_fail, &[]), (l_grow, &[fl]));
        b.switch_to(l_fail);
        b.br(x.l_stop_in, &[row.carry.out, row.at, fl]);
        b.switch_to(l_grow);
        let fl = b.block_params(l_grow)[0];
        let flags = HostFlags { may_error: true, touches_memory: true, ..HostFlags::default() };
        let f = gravity_abi::host_words!(gravity_abi::program::out_grow(ctx, bytes));
        let h = b.declare_host_once("out_grow", &[Type::Ptr, Type::I64], Some(Type::I64), f, flags);
        let cap = b.call_host(h, &[env.ctx, total], Some(Type::I64)).expect("out_grow returns the new capacity");
        let failed = b.icmp(IntCond::Eq, cap, env.zero);
        let e = b.iconst(Type::I64, raise::bit(raise::HOST) as i64);
        b.cond_br(failed, (env.l_raise, &[e]), (l_store, &[fl]));
    }
}

impl EndMech for Sink<'_> {
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, x: &Exits) -> Carry<'e> {
        let ds = self.datums(b, env, row);
        let o = b.load(Type::Ptr, env.ctx, hdr::OUT as i32);
        let safe = b.conv(ConvOp::PtrToInt, Type::I64, o);
        let mut refs = PgVec::new_in(env.mcx);
        for (c, &(d, valid)) in self.cols.iter().zip(ds.iter()) {
            if let Some(typlen) = c.byref {
                let value = b.select(valid, d, safe);
                refs.push_in(ByRef { value, valid, typlen });
            }
        }
        let (per, stop) = match refs.is_empty() {
            true => (Vec::new(), None),
            false => {
                let (per, stop) = self.place(b, env, o, &refs[..], row, x);
                (per, Some(stop))
            }
        };
        let count = b.load(Type::I64, o, out::COUNT as i32);
        let vals = b.load(Type::Ptr, o, out::VALUES as i32);
        let nulls = b.load(Type::Ptr, o, out::ISNULL as i32);
        let ncols = b.iconst(Type::I64, self.cols.len() as i64);
        let cell = b.int_bin(IntBinOp::Mul, count, ncols);
        let dv = b.ptr_add(vals, cell, 8, 0);
        let dn = b.ptr_add(nulls, cell, 1, 0);
        let t = b.bconst(true);
        let mut r = 0;
        for (k, (c, &(d, valid))) in self.cols.iter().zip(ds.iter()).enumerate() {
            if c.byref.is_some() {
                let n0 = b.num_blocks();
                byref::store(b, o, dv, dn, k as i32, refs[r], per[r]);
                bounded_since(b, env, n0);
                r += 1;
                continue;
            }
            b.store(dv, (k * 8) as i32, d);
            let nv = b.int_bin(IntBinOp::Xor, valid, t);
            let byte = b.conv(ConvOp::Zext, Type::I8, nv);
            b.store(dn, k as i32, byte);
        }
        let count2 = b.int_bin(IntBinOp::Add, count, env.one);
        b.store(o, out::COUNT as i32, count2);
        let out2 = b.int_bin(IntBinOp::Add, row.carry.out, env.one);
        if let Some(st) = stop {
            // the early push's stop, after the row (the block that push emptied holds it alone, below any cap: not full)
            let l_go = b.create_block(&[]);
            let stopped = b.icmp(IntCond::Ne, st, env.zero);
            b.cond_br(stopped, (env.x.l_stop_in, &[out2, row.next, st]), (l_go, &[]));
            b.switch_to(l_go);
        }
        let cap = b.load(Type::I64, o, out::CAP as i32);
        let full = b.icmp(IntCond::Sge, count2, cap);
        let (l_push, l_on) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(full, (l_push, &[]), (l_on, &[]));
        b.switch_to(l_push);
        let fl = b.call_host(env.core.push, &[env.ctx], Some(Type::I64)).unwrap_or(env.zero);
        let cont = b.icmp(IntCond::Eq, fl, env.zero);
        let l_cont = if env.hir.hosted() { b.create_block(&[]) } else { l_on };
        b.cond_br(cont, (l_cont, &[]), (env.x.l_stop_in, &[out2, row.next, fl]));
        if l_cont != l_on {
            b.switch_to(l_cont);
            env.hir.rewind(b);
            b.br(l_on, &[]);
        }
        b.switch_to(l_on);
        Carry { out: out2, accs: mcx::to_vec_in(&row.carry.accs[..], env.mcx) }
    }

    fn sink(&self) -> SinkKind {
        self.form
    }
}
