//! nest.spool's record (M8-NEST S1; a helper of nest/spool.rs, reused by nest.cte and nest.memo): one spooled row as its
//! columns' datum words then one null byte per column (8-aligned), appended at `base + n * rec` of RT-1s's spool object
//! (exec v4/m8_spool.rs, kernels host_abi/v4/m8.rs `SpoolCell`). A by-reference datum is copied into the object's arena
//! (`m8_spool_copy`, never moved in the execution) by `spool_keep`, so a record never points into a page the scan moved
//! past; an expanded or indirect value is flattened first (PostgreSQL's tuplestore flattens it into the minimal tuple). The
//! record is read back as datum cells, by index, `base` read again at every read (a grow may move the region). The count
//! of records and the other per-execution words are parameter words no expression owns (zeroed at bind, `Px::proto_words`),
//! as the object itself is laid at every execution's prepare. Registers nothing. Owner M8-NEST.
use gravity_abi::status::raise;
use gravity_decompose::catalog::typefacts::{facts_of, TypeFacts};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_kernels::ops::shared::object::entries as m8;
use gravity_program::protos::{Proto, ProtoKind};
use gravity_program::{NumTypmod, Rep, Until};
use mcx::{CollectIn, PgVec};
use types_nodes::list::NodeList;

use crate::cell::{At, Cell, CellHome, CellLife, Nullable, Row};
use crate::frame::env::Env;
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::rep::CellRef;
use crate::admit::refusal::{Reason, ShapeWhy};

/// A spool's record: its columns' type facts, its bytes, the spool cell's handle word and the count word.
pub(crate) struct Rec<'g> {
    tfs: PgVec<'g, TypeFacts>,
    bytes: i64,
    h: u16,
    n: u16,
    node: i32,
    /// D3040 (`m8::SPOOL_OWNED`): an owned spool, each owner appending to its own slot's spool, read once sealed.
    owned: bool,
    /// D4569 (road drain.spool.wide): an owned spool's reader's own spool (its handle word; its count word after it stays
    /// 0), where a spooled drain puts its drained row at the sealed count (`store` unbumped) and `read` reads record `count`
    /// from (drain/spool.rs `DrainReader`): the merged cell takes no record (exec ops/material/raw.rs `op_merged`).
    put: Option<u16>,
}

impl<'g> Rec<'g> {
    /// The record of the rows `cols` (a projection's target list, in resno order) makes, its spool declared for pipeline `k`
    /// (`ProtoKind::Spool`, its words the handle, the count after it and `extra` more zeroed-at-bind words after the count
    /// (their first returned), `arg` the record's bytes, `peer` the Material's fill pipeline `fill`).
    pub(crate) fn declare(px: &mut Px<'g, '_>, k: usize, fill: usize, node: i32, cols: &NodeList<'_>, extra: u16) -> (Rec<'g>, u16) {
        let tfs = types(px, node, cols);
        Rec::declare_tfs(px, k, fill, node, tfs, extra)
    }

    /// `declare` over columns the caller typed (nest/spool_drain.rs: a joined row's columns); one spool per fill (make-once).
    pub(crate) fn declare_tfs(px: &mut Px<'_, '_>, k: usize, fill: usize, node: i32, tfs: PgVec<'g, TypeFacts>, extra: u16) -> (Rec<'g>, u16) {
        if px.protos.iter().any(|p| p.kind == ProtoKind::Spool && p.peer as usize == fill) {
            crate::refuse!(node, Reason::Bug { payload: "a second spool declared for one fill pipeline" })
        }
        let bytes = record_bytes(tfs.len());
        let h = px.proto_words(2 + extra);
        // the proto names every word (the handle, the count, the extras), so a recheck that reruns the
        // fill can lay its own empty spool over all of them (exec rerun/epq.rs `fresh_spools`)
        px.declare_proto(Proto::new(ProtoKind::Spool, k as u16).peer(fill as u16).words(h, 2 + extra).arg(bytes as u64));
        (Rec { tfs, bytes, h, n: h + 1, node, owned: false, put: None }, h + 2)
    }

    /// D3040: this record's spool made OWNED (`m8::SPOOL_OWNED` on its proto): a parallel fill, each owner (its ctx's
    /// worker ordinal) appending to its own slot, sealed after the fill by exec; readers read the sealed records. D4628
    /// (road `spool.seal`, seal.rs): sealed off the leader's copy.
    pub(crate) fn owned(mut self, px: &mut Px<'_, '_>, fill: usize) -> Rec<'g> {
        match px.protos.iter_mut().find(|p| p.kind == ProtoKind::Spool && p.peer as usize == fill) {
            Some(p) => p.arg |= m8::SPOOL_OWNED | super::seal::bits(),
            None => crate::refuse!(self.node, Reason::Bug { payload: "an owned spool with no Spool proto" }),
        }
        self.owned = true;
        self
    }

    /// D4569 (road drain.spool.wide): this owned spool's reader given its put spool (`put`): a plain spool of the same
    /// record, declared for pipeline `d` (its one filler: the reader, serial), that holds the one record past the sealed
    /// count the reader puts there.
    pub(crate) fn with_put(mut self, px: &mut Px<'_, '_>, d: usize) -> Rec<'g> {
        if !self.owned {
            crate::refuse!(self.node, Reason::Bug { payload: "a put spool beside a plain spool (its count's record is its own)" })
        }
        // the reader made again (a second making of pipeline `d`) finds its own put spool: two words, this record's bytes
        if let Some(p) = px.protos.iter().find(|p| p.kind == ProtoKind::Spool && p.peer as usize == d) {
            if p.words.n != 2 || p.arg != self.bytes as u64 {
                crate::refuse!(self.node, Reason::Bug { payload: "a second spool declared for one fill pipeline" })
            }
            self.put = Some(p.words.base);
            return self;
        }
        let h = px.proto_words(2);
        px.declare_proto(Proto::new(ProtoKind::Spool, d as u16).peer(d as u16).words(h, 2).arg(self.bytes as u64));
        self.put = Some(h);
        self
    }

    /// The record fill pipeline `fill`'s end declared (`declare`), for its reader.
    pub(crate) fn find(px: &Px<'g, '_>, fill: usize, node: i32, cols: &NodeList<'_>) -> Rec<'g> {
        let tfs = types(px, node, cols);
        Rec::find_tfs(px, fill, node, tfs)
    }

    /// `find` over columns the caller typed (as its fill's `declare_tfs`).
    pub(crate) fn find_tfs(px: &Px<'_, '_>, fill: usize, node: i32, tfs: PgVec<'g, TypeFacts>) -> Rec<'g> {
        let Some(p) = px.protos.iter().find(|p| p.kind == ProtoKind::Spool && p.peer as usize == fill) else {
            crate::refuse!(node, Reason::Bug { payload: "a spool reader made before its fill's end declared the spool" })
        };
        let h = p.words.base;
        let owned = p.arg & m8::SPOOL_OWNED != 0;
        Rec { bytes: record_bytes(tfs.len()), tfs, h, n: h + 1, node, owned, put: None }
    }

    /// setop.recursive's two tables (worklist setop P6): the record a spool declared at handle word `h` holds (its count
    /// word after it), found by the word, never by a peer: the recursive union swaps two spools' words each generation.
    pub(crate) fn by_words(px: &Px<'g, '_>, h: u16, node: i32, cols: &NodeList<'_>) -> Rec<'g> {
        let tfs = types(px, node, cols);
        Rec { bytes: record_bytes(tfs.len()), tfs, h, n: h + 1, node, owned: false, put: None }
    }

    /// D3054: whether the spool is owned (`owned`): its count is the sealed one, read after the fill.
    pub(crate) fn is_owned(&self) -> bool {
        self.owned
    }

    /// The handle and count words (the IR swaps a recursive union's two tables by them).
    pub(crate) fn words(&self) -> (u16, u16) {
        (self.h, self.n)
    }

    /// Whether `o` lays its records as this one does (each cell's length and by-value form).
    pub(crate) fn same(&self, o: &Rec<'_>) -> bool {
        self.tfs.len() == o.tfs.len() && self.tfs.iter().zip(o.tfs.iter()).all(|(a, b)| (a.typlen, a.byval) == (b.typlen, b.byval))
    }

    /// The record emptied: its count back to 0 (D1107's refill; the arena's copies stay until the execution ends).
    pub(crate) fn empty(&self, b: &mut FunctionBuilder<'_>, env: &Env) {
        if self.owned {
            crate::refuse!(self.node, Reason::Bug { payload: "an owned spool emptied (it is filled once, in parallel)" })
        }
        b.store(env.params, self.n as i32 * 8, env.zero);
    }

    /// The count of records appended in this execution.
    pub(crate) fn count(&self, b: &mut FunctionBuilder<'_>, env: &Env) -> Value {
        if self.owned {
            // D3040: the sealed count (`OwnedSpools::total`), read after the fill
            let (_, cell) = self.cell(b, env);
            return b.load(Type::I64, cell, m8::OWNED_TOTAL as i32);
        }
        b.load(Type::I64, env.params, self.n as i32 * 8)
    }

    /// D3040: this owner's slot of an owned spool: its spool cell's handle (as an integer and a pointer) and the slot's
    /// address (its count at `OWNED_SLOT_N`), by the ctx's worker ordinal.
    fn slot(&self, b: &mut FunctionBuilder<'_>, env: &Env) -> (Value, Value, Value) {
        let (_, obj) = self.cell(b, env);
        let ord = b.load_zext(Type::I16, env.ctx, gravity_abi::ctx::hdr::WORKER_ORDINAL as i32);
        let sz = b.iconst(Type::I64, m8::OWNED_SLOT_BYTES as i64);
        let off = b.int_bin(IntBinOp::Mul, ord, sz);
        let first = b.iconst(Type::I64, m8::OWNED_SLOTS as i64);
        let off = b.int_bin(IntBinOp::Add, off, first);
        let ov = b.conv(ConvOp::PtrToInt, Type::I64, obj);
        let at = b.int_bin(IntBinOp::Add, ov, off);
        let slot = b.conv(ConvOp::IntToPtr, Type::Ptr, at);
        let hv = b.load(Type::I64, slot, m8::OWNED_SLOT_CELL as i32);
        (hv, b.conv(ConvOp::IntToPtr, Type::Ptr, hv), slot)
    }

    fn cell(&self, b: &mut FunctionBuilder<'_>, env: &Env) -> (Value, Value) {
        self.cell_at(b, env, self.h)
    }

    /// The spool cell whose handle is word `h` (this record's, or D4569's put spool's).
    fn cell_at(&self, b: &mut FunctionBuilder<'_>, env: &Env, h: u16) -> (Value, Value) {
        let hv = b.load(Type::I64, env.params, h as i32 * 8);
        (hv, b.conv(ConvOp::IntToPtr, Type::Ptr, hv))
    }

    fn at(&self, b: &mut FunctionBuilder<'_>, env: &Env, j: Value) -> Value {
        let (_, cell) = self.cell(b, env);
        let base = b.load(Type::I64, cell, m8::SPOOL_BASE as i32);
        let rb = b.iconst(Type::I64, self.bytes);
        let off = b.int_bin(IntBinOp::Mul, j, rb);
        let at = b.int_bin(IntBinOp::Add, base, off);
        b.conv(ConvOp::IntToPtr, Type::Ptr, at)
    }

    /// `row`'s columns appended as record `n`, the count bumped: every datum first (a by-reference one copied), then a grow
    /// when the region is full, then the stores. A failed copy or grow raises the host's stashed error.
    pub(crate) fn append(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals) {
        let cells: PgVec<'_, Option<CellRef<'_>>> = (0..self.tfs.len()).map(|c| row.col(c as i16 + 1)).collect_in(env.mcx);
        self.store(b, env, &cells[..], true);
    }

    /// `cells` stored as record `n` (the count); `bump`: the count bumped (`append`), else the record is `n`'s until the
    /// next store (nest/spool_drain.rs's put at the count).
    pub(crate) fn store(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, cells: &[Option<CellRef<'_>>], bump: bool) {
        let mut cx = env.lx(b, self.node);
        let vals: PgVec<'_, (Value, Value)> = self.tfs.iter().enumerate().map(|(c, tf)| {
            let Some(cr) = cells.get(c).copied().flatten() else { shape(self.node, "a spooled column past the child's row") };
            crate::rep::datum(&mut cx, cr, tf)
        }).collect_in(env.mcx);
        env.memo = cx.memo;
        // D4569: an owned spool's put (unbumped) goes to its reader's put spool, record 0 (its count word stays 0)
        let put = match (self.owned, bump, self.put) {
            (true, false, Some(p)) => Some(p),
            _ => None,
        };
        let (hv, cell, slot) = match self.owned && put.is_none() {
            true => {
                if !bump {
                    crate::refuse!(self.node, Reason::Bug { payload: "an owned spool's record stored without its count" })
                }
                let (hv, cell, slot) = self.slot(b, env);
                (hv, cell, Some(slot))
            }
            false => {
                let (hv, cell) = self.cell_at(b, env, put.unwrap_or(self.h));
                (hv, cell, None)
            }
        };
        let words: PgVec<'_, Value> = self.tfs.iter().zip(vals.iter()).map(|(tf, &(v, null))| {
            let word = b.select(null, env.zero, v);
            if tf.byval {
                return word;
            }
            let (l_copy, l_join) = (b.create_block(&[]), b.create_block(&[Type::I64]));
            b.cond_br(null, (l_join, &[env.zero]), (l_copy, &[]));
            b.switch_to(l_copy);
            let f = b.declare_host_once("spool_keep", &[Type::Ptr, Type::I64, Type::I64, Type::I64], Some(Type::I64), super::raw::spool_keep as *const () as usize, HostFlags { may_error: true, touches_memory: true, ..HostFlags::default() });
            let len = b.iconst(Type::I64, tf.typlen as i64);
            let at = b.call_host(f, &[env.ctx, hv, word, len], Some(Type::I64)).expect("spool_keep returns the copy");
            raise_below_zero(b, env, at);
            b.br(l_join, &[at]);
            b.switch_to(l_join);
            b.block_params(l_join)[0]
        }).collect_in(env.mcx);
        let n = match (slot, put) {
            (Some(s), _) => b.load(Type::I64, s, m8::OWNED_SLOT_N as i32),
            (None, Some(p)) => b.load(Type::I64, env.params, (p + 1) as i32 * 8),
            (None, None) => self.count(b, env),
        };
        let cap = b.load(Type::I64, cell, m8::SPOOL_CAP as i32);
        let (l_grow, l_store) = (b.create_block(&[]), b.create_block(&[]));
        let full = b.icmp(IntCond::Uge, n, cap);
        b.cond_br(full, (l_grow, &[]), (l_store, &[]));
        b.switch_to(l_grow);
        let f = b.declare_host_once("m8_spool_grow", &[Type::Ptr, Type::I64], Some(Type::I64), m8::m8_spool_grow as *const () as usize, HostFlags { may_error: true, touches_memory: true, ..HostFlags::default() });
        let r = b.call_host(f, &[env.ctx, hv], Some(Type::I64)).expect("m8_spool_grow returns the capacity");
        raise_below_zero(b, env, r);
        b.br(l_store, &[]);
        b.switch_to(l_store);
        let rec = match slot.or(put.map(|_| cell)) {
            Some(_) => {
                let base = b.load(Type::I64, cell, m8::SPOOL_BASE as i32);
                let rb = b.iconst(Type::I64, self.bytes);
                let off = b.int_bin(IntBinOp::Mul, n, rb);
                let at = b.int_bin(IntBinOp::Add, base, off);
                b.conv(ConvOp::IntToPtr, Type::Ptr, at)
            }
            None => self.at(b, env, n),
        };
        let nc = self.tfs.len() as i32;
        for (c, (w, &(_, null))) in words.iter().zip(vals.iter()).enumerate() {
            b.store(rec, 8 * c as i32, *w);
            let nb = b.conv(ConvOp::Zext, Type::I8, null);
            b.store(rec, 8 * nc + c as i32, nb);
        }
        if bump {
            let n1 = b.int_bin(IntBinOp::Add, n, env.one);
            match slot {
                Some(s) => b.store(s, m8::OWNED_SLOT_N as i32, n1),
                None => b.store(env.params, self.n as i32 * 8, n1),
            }
        }
    }

    /// Record `j` as the row's datum cells (a cell lives for the row; its by-reference datum for the execution).
    pub(crate) fn read<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        let rec = match self.put {
            // D4569: record `count` (the sealed total) is the put spool's record 0, every earlier one the merged cell's
            Some(p) => {
                let n = self.count(b, env);
                let (_, cell) = self.cell(b, env);
                let base = b.load(Type::I64, cell, m8::SPOOL_BASE as i32);
                let rb = b.iconst(Type::I64, self.bytes);
                let off = b.int_bin(IntBinOp::Mul, j, rb);
                let own = b.int_bin(IntBinOp::Add, base, off);
                let (_, pc) = self.cell_at(b, env, p);
                let first = b.load(Type::I64, pc, m8::SPOOL_BASE as i32);
                let held = b.icmp(IntCond::Slt, j, n);
                let at = b.select(held, own, first);
                b.conv(ConvOp::IntToPtr, Type::Ptr, at)
            }
            None => self.at(b, env, j),
        };
        let (nc, mcx) = (self.tfs.len() as i32, env.mcx);
        let vals = (0..nc).map(|c| {
            let v = b.load(Type::I64, rec, 8 * c);
            let nb = b.load_zext(Type::I8, rec, 8 * nc + c);
            (v, Some(b.icmp(IntCond::Ne, nb, env.zero)))
        }).collect_in(mcx);
        let cells = self.tfs.iter().enumerate().map(|(c, tf)| {
            let life = CellLife { until: Until::Row, home: CellHome::FrameSlot };
            Cell { rep: Rep::of_storage(tf.typlen, tf.byval), typid: tf.typid, num: NumTypmod::NONE, life, at: At::Slot(c as u16), null: Nullable::Maybe }
        }).collect_in(mcx);
        RowVals { row: Row { cells }, cols: (0..nc as u16).map(Some).collect_in(mcx), vals, carry, next, at: j }
    }
}

/// Each column's type facts, `cols` in resno order.
fn types<'g>(px: &Px<'g, '_>, node: i32, cols: &NodeList<'_>) -> PgVec<'g, TypeFacts> {
    cols.iter().enumerate().map(|(i, n)| {
        let te = n.as_target_entry().filter(|te| te.resno as usize == i + 1).unwrap_or_else(|| shape(node, "a spooled row out of resno order"));
        let typ = gravity_decompose::catalog::expr_type::type_of(te.expr).unwrap_or_else(|| gravity_decompose::catalog::expr_type::result_type(te.expr));
        facts_of(px.gx.facts, typ).unwrap_or_else(|| shape(node, "a spooled column whose type the catalog does not know"))
    }).collect_in(px.mcx())
}

/// A record's bytes: a datum word per column, then a null byte per column, 8-aligned (8 at least).
fn record_bytes(nc: usize) -> i64 {
    let nc = nc as i64;
    (8 * nc + (nc + 7) / 8 * 8).max(8)
}

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

/// A host status below zero leaves by the raise exit with the host's stashed error.
fn raise_below_zero(b: &mut FunctionBuilder<'_>, env: &Env, st: Value) {
    let bad = b.icmp(IntCond::Slt, st, env.zero);
    let (l_bad, l_ok) = (b.create_block(&[]), b.create_block(&[]));
    b.mark_cold(l_bad);
    b.cond_br(bad, (l_bad, &[]), (l_ok, &[]));
    b.switch_to(l_bad);
    let e = b.iconst(Type::I64, raise::bit(raise::HOST) as i64);
    b.br(env.l_raise, &[e]);
    b.switch_to(l_ok);
}
