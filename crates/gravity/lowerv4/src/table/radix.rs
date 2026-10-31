//! Radix partition at every width incl. W0. Hooks / levers: H13c R5-L05. Budget 160 lines (files.tsv, +25 % gate).
//! Owner M1-TABLES.
//!
//! A keyed table's records route into `2^bits` radix partitions by the top bits of their route hash (`KeyHash::part`), at
//! every width, W0 included, so the owner phase folds each partition in an L2-sized table. Past the runtime's floor (16
//! partitions, 64 from W8: `agg_open`), the bits follow the bound: enough partitions that the bound's records fill one
//! phase-1 table each, at most `T1_MAX_PARTS`, so a big fold never re-reads a partition to split it.
//!
//! A record lands in its partition here (`Hashed::append`): the partition of its route hash, the partition's head word
//! against the record threshold, the head stepped by the record size, a full or absent block through `agg_block`.
//!
//! Registers: road `table.radix` (`TABLE_RADIX`).
use gravity_abi::breaker::{RecordLayout, HEAD_LEFT, HEAD_POS, LAYOUT_IMPLICIT_COUNT, MINCODE_NONE, t1_default_bytes, T1_MAX_PARTS};
use gravity_expr::emit::hash::{emit_part, emit_route_and_full, Bits};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, IntCond, IntUnOp, Type, Value};
use gravity_kernels::host_abi as abi;

use crate::table::bypass::{host, Hashed, ST_APP, ST_HEADS, ST_PBITS};
use crate::table::face::TableDecl;
use crate::table::keyimg::ImageWords;
use crate::frame::env::Env;

crate::strategies! {
    /// Radix partition at every width incl. W0.
    TABLE_RADIX = 36, "table.radix", Table;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(TABLE_RADIX, &())], ..crate::ops::NONE };

/// `t`'s partition bits: `bits_for` its bound (`table_size::groups`), 0 = the runtime's floor.
pub(crate) fn bits(b: &mut FunctionBuilder<'_>, env: &Env, t: &TableDecl) -> Value {
    let groups = crate::table::size::groups(b, env, t);
    bits_for(b, groups, t.layout.size())
}

/// `ceil(log2(groups * rec / T1_DEFAULT_BYTES))` clamped to `[0, log2 T1_MAX_PARTS]`: 0 for no bound or one table's worth.
pub(crate) fn bits_for(b: &mut FunctionBuilder<'_>, groups: Value, rec: u32) -> Value {
    if rec == 0 {
        return b.iconst(Type::I64, 0);
    }
    // Estimates can reach u64::MAX. Cap before multiplying so an overflow
    // cannot turn a large table into an unpartitioned one. The cap already
    // fills the maximum number of partitions; larger inputs have the same result.
    let cap = (u64::from(t1_default_bytes()) * u64::from(T1_MAX_PARTS)).div_ceil(u64::from(rec));
    let cap = b.iconst(Type::I64, cap as i64);
    let over = b.icmp(IntCond::Ugt, groups, cap);
    let groups = b.select(over, cap, groups);
    let one = b.iconst(Type::I64, 1);
    let r = b.iconst(Type::I64, rec as i64);
    let bytes = b.int_bin(IntBinOp::Mul, groups, r);
    let round = b.iconst(Type::I64, t1_default_bytes() as i64 - 1);
    let up = b.int_bin(IntBinOp::Add, bytes, round);
    let shift = b.iconst(Type::I64, t1_default_bytes().trailing_zeros() as i64);
    let tables = b.int_bin(IntBinOp::Ushr, up, shift);
    let many = b.icmp(IntCond::Ugt, tables, one);
    let less = b.int_bin(IntBinOp::Sub, tables, one);
    let lz = b.int_un(IntUnOp::Clz, less);
    let sixty_four = b.iconst(Type::I64, 64);
    let log = b.int_bin(IntBinOp::Sub, sixty_four, lz);
    let max = b.iconst(Type::I64, T1_MAX_PARTS.trailing_zeros() as i64);
    let over = b.icmp(IntCond::Ugt, log, max);
    let log = b.select(over, max, log);
    let zero = b.iconst(Type::I64, 0);
    b.select(many, log, zero)
}

/// A reserved record opened as `Rec::open` opens one: zeroed, the image's words as keys, the implicit sentinel at 1 (the
/// record form, 1 + rows: the end adds its rows), a MinCode slot at `MINCODE_NONE` on a layout without the validity word.
pub(crate) fn open_record(b: &mut FunctionBuilder<'_>, p: Value, l: &RecordLayout, words: &[Value]) {
    open_record_n(b, p, l, words, None)
}

/// `open_record` with the implicit sentinel at `1 + n` when `n` is given (V4R-201 count-at-append): the record's words
/// written once each, keys first, the sentinel never zeroed then rewritten.
pub(crate) fn open_record_n(b: &mut FunctionBuilder<'_>, p: Value, l: &RecordLayout, words: &[Value], n: Option<Value>) {
    let zero = b.iconst(Type::I64, 0);
    let implicit = l.flags & LAYOUT_IMPLICIT_COUNT != 0;
    for (k, w) in words.iter().enumerate() {
        b.store(p, 8 * k as i32, *w);
    }
    for off in (8 * words.len() as u32..l.size()).step_by(8).filter(|&o| !(implicit && o == l.sentinel_off())) {
        b.store(p, off as i32, zero);
    }
    if implicit {
        let one = b.iconst(Type::I64, 1);
        let c = match n {
            Some(n) => b.int_bin(IntBinOp::Add, n, one),
            None => one,
        };
        b.store(p, l.sentinel_off() as i32, c);
    }
    if !l.has_validity() {
        let none = b.const_raw(Type::I64, MINCODE_NONE);
        for j in (0..l.nslots()).filter(|&j| l.mincode(j)) {
            b.store(p, l.slot_off(j) as i32, none);
        }
    }
}

/// The implicit count's bump (`+= n` at `off`), skipped when the append wrote the record with its count (`counted`,
/// V4R-201): no reload of a word just stored.
pub(crate) fn bump_unless(b: &mut FunctionBuilder<'_>, rec: Value, off: i32, n: Value, counted: Value) {
    let (l_bump, l_go) = (b.create_block(&[]), b.create_block(&[]));
    b.cond_br(counted, (l_go, &[]), (l_bump, &[]));
    b.switch_to(l_bump);
    let c = b.load(Type::I64, rec, off);
    let c = b.int_bin(IntBinOp::Add, c, n);
    b.store(rec, off, c);
    b.br(l_go, &[]);
    b.switch_to(l_go);
}

impl Hashed {
    /// The bypass arm: `img`'s record appended at its partition's head (`reserve_fast`; a full or absent block through
    /// `agg_block`), opened as `Rec::open` opens one. The route is the image's partition words (`Rec::route`, D250).
    pub(crate) fn append(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl, st: Value, img: &ImageWords, h: Value) -> Value {
        self.append_n(b, env, t, st, img, h, None)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn append_n(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl, st: Value, img: &ImageWords, h: Value, n: Option<Value>) -> Value {
        let (ctx, zero, x) = (env.ctx, env.zero, env.x);
        let (rec, words) = (t.layout.size() as i64, &img.words.words);
        let pk = t.layout.partition_keys();
        let route = if pk >= words.len() { h } else { emit_route_and_full(b, &img.words, pk).0 };
        let pbits = b.load(Type::I64, st, ST_PBITS);
        let part = emit_part(b, route, Bits::Dyn(pbits));
        let heads = b.load(Type::Ptr, st, ST_HEADS);
        let hp = b.ptr_add(heads, part, 8, 0);
        let head = b.load(Type::I64, hp, 0);
        let thresh = b.const_raw(Type::I64, (rec as u64) << HEAD_LEFT);
        let (l_blk, l_st) = (b.create_block(&[]), b.create_block(&[Type::I64]));
        let full = b.icmp(IntCond::Ult, head, thresh);
        b.cond_br(full, (l_blk, &[]), (l_st, &[head]));
        b.switch_to(l_blk);
        let app = b.load(Type::I64, st, ST_APP);
        let block = host(b, "agg_block", &[Type::Ptr, Type::Ptr, Type::I64, Type::I64], gravity_abi::host_words!(abi::agg_block(ctx, st, part, appended)));
        let r = b.call_host(block, &[ctx, st, part, app], Some(Type::I64)).expect("agg_block returns a head or a status");
        b.store(st, ST_APP, zero);
        let bad = b.icmp(IntCond::Sle, r, zero);
        b.cond_br(bad, (x.l_exit, &[r]), (l_st, &[r]));
        b.switch_to(l_st);
        let head = b.block_params(l_st)[0];
        let size = b.iconst(Type::I64, rec);
        let stepped = b.int_bin(IntBinOp::Add, head, size);
        let next = b.int_bin(IntBinOp::Sub, stepped, thresh);
        b.store(hp, 0, next);
        let posm = b.const_raw(Type::I64, HEAD_POS);
        let pos = b.int_bin(IntBinOp::And, head, posm);
        let p = b.conv(ConvOp::IntToPtr, Type::Ptr, pos);
        open_record_n(b, p, &t.layout, words, n);
        let app = b.load(Type::I64, st, ST_APP);
        let app1 = b.int_bin(IntBinOp::Add, app, env.one);
        b.store(st, ST_APP, app1);
        p
    }
}

#[cfg(test)]
mod tests {
    use gravity_interp::Interp;
    use gravity_ir::builder::ProgramBuilder;
    use gravity_ir::Type;

    fn bits(groups: u64, rec: u32) -> u64 {
        let mut pb = ProgramBuilder::new();
        let f = pb.declare_func("bits", &[Type::I64], &[Type::I64]);
        let mut b = pb.define(f);
        let g = b.params()[0];
        let v = super::bits_for(&mut b, g, rec);
        b.ret(&[v]);
        let prog = pb.finish();
        Interp::new(&prog).run(f, &[groups]).expect("runs")[0]
    }

    #[test]
    fn the_bound_sets_one_table_of_records_per_partition() {
        let bytes = u64::from(gravity_abi::breaker::t1_default_bytes());
        let max = u64::from(gravity_abi::breaker::T1_MAX_PARTS.trailing_zeros());
        // The table budget follows the detected L2 share, not a fixed 512 KiB.
        assert_eq!([bits(0, 32), bits(1, 32), bits(bytes / 32, 32)], [0, 0, 0]);
        assert_eq!([bits(bytes / 32 + 1, 32), bits(4 * bytes / 32, 32)], [1, 2]);
        for rec in [1, 24, 32, 64, u32::MAX] {
            for groups in [0, 1, bytes / 32, 1 << 20, u64::MAX / 32 + 1, u64::MAX] {
                let tables = (u128::from(groups) * u128::from(rec)).div_ceil(u128::from(bytes));
                let expected = if tables <= 1 { 0 } else { u64::from(128 - (tables - 1).leading_zeros()).min(max) };
                assert_eq!(bits(groups, rec), expected, "groups={groups}, record bytes={rec}");
            }
        }
        assert_eq!(bits(u64::MAX, 0), 0);
    }
}
