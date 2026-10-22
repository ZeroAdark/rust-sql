//! Observed-eviction bypass + owner phase. Hooks / levers: H13a R3-L05 R9-L12. Budget 160 lines (files.tsv, +25 %
//! gate). Owner M1-TABLES.
//!
//! The hashed keyed table: the runtime's phase-1 table (T1, abi breaker.rs) probed inline at the image's KeyHash home pair
//! (`home(cap_bits, TOP)`: a producer table holds every partition); a miss is `agg_locate` (the occupant evicted into its
//! radix partition). When the evictions say the keys outrun the table (`observe` every `ADAPT_WIN` inserts; an estimate
//! is a hint, never a gate: v1 negative result 4), T1 drains and bypasses: each record is appended to its partition in IR
//! (radix.rs `Hashed::append`; the open is table_size.rs `Hashed::open_with`) and the owner phase dedups and folds per L2-sized partition.
//!
//! Hosts: agg_open, agg_block, agg_finish (kernels host_abi), agg_locate (host_abi v4 m1).
//! Registers: road `table.bypass` (`TABLE_BYPASS`) as the hashed `TableImpl`, the resolver's last policy.
use core::mem::offset_of;

use gravity_abi::breaker::{T1, T1_BYPASS};
use gravity_expr::emit::hash::{emit_home, emit_image_eq_at, Bits};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, HostFlags, HostFn, IntBinOp, IntCond, IntUnOp, Type, Value};
use gravity_kernels::host_abi::{self as abi, AggState, AGG_BYPASS_WORD, AGG_STATE_BYTES};

use crate::table::face::{TableDecl, TableImpl, TableMech};
use crate::table::keyimg::ImageWords;
use crate::frame::env::Env;
use crate::frame::stop::Exits;

crate::strategies! {
    /// Observed-eviction bypass + owner phase.
    TABLE_BYPASS = 34, "table.bypass", Keyed;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(TABLE_BYPASS, &TableImpl(&HASHED))], ..crate::ops::NONE };

pub(crate) use crate::table::radix::open_record;

pub(crate) struct Hashed;
pub(crate) static HASHED: Hashed = Hashed;

pub(crate) const ST_T1: i32 = offset_of!(AggState, t) as i32;
pub(crate) const ST_HEADS: i32 = offset_of!(AggState, heads) as i32;
const ST_TABLE: i32 = offset_of!(AggState, table) as i32;
const ST_MASK: i32 = offset_of!(AggState, mask) as i32;
pub(crate) const ST_PBITS: i32 = offset_of!(AggState, pbits) as i32;
pub(crate) const ST_LAYOUT: i32 = offset_of!(AggState, layout) as i32;
/// Records appended since the last `agg_block` (published into `T1.buffered` there and at the finish).
pub(crate) const ST_APP: i32 = AGG_STATE_BYTES as i32;
const ST_BYPASS: i32 = AGG_BYPASS_WORD as i32;
const T1_FLAGS: i32 = offset_of!(T1, flags) as i32;
pub(crate) const T1_LAYOUT: i32 = offset_of!(T1, layout) as i32;
const T1_BUFFERED: i32 = offset_of!(T1, buffered) as i32;

pub(crate) fn host(b: &mut FunctionBuilder<'_>, name: &str, params: &[Type], addr: usize) -> HostFn {
    b.declare_host_once(name, params, Some(Type::I64), addr, HostFlags { touches_memory: true, may_error: true, ..HostFlags::default() })
}

/// The table's words: `AggState`, the append count, the bypass word.
pub(crate) fn state(b: &mut FunctionBuilder<'_>, env: &Env, t: &TableDecl) -> Value {
    let st = env.state.expect("the end's state slot is laid before its open");
    b.ptr_add(st, env.zero, 1, t.state_off as i32)
}

/// The state's bypass word read from T1's flags (the open, and after a `locate` that may switch it).
pub(crate) fn sync_bypass(b: &mut FunctionBuilder<'_>, st: Value) {
    let t1 = b.load(Type::Ptr, st, ST_T1);
    let flags = b.load_zext(Type::I8, t1, T1_FLAGS);
    let bit = b.iconst(Type::I64, T1_BYPASS as i64);
    let on = b.int_bin(IntBinOp::And, flags, bit);
    b.store(st, ST_BYPASS, on);
}

/// A negative status leaves the function (`env.x.l_exit`): a failed flush or a real allocation failure, never a size.
pub(crate) fn fail_on(b: &mut FunctionBuilder<'_>, env: &Env, x: &Exits, r: Value) {
    let (bad, l_ok) = (b.icmp(IntCond::Slt, r, env.zero), b.create_block(&[]));
    b.cond_br(bad, (x.l_exit, &[r]), (l_ok, &[]));
    b.switch_to(l_ok);
}

impl Hashed {
    /// `upsert` (V4R-201 count-at-append): with `n`, the appended record's sentinel is `1 + n` already, `counted` true there.
    #[allow(clippy::too_many_arguments)]
    fn upsert_at(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl, img: &ImageWords, h: Value, n: Option<Value>) -> (Value, Value, Value) {
        debug_assert!(img.exact, "a record decides a hit by its words: an inexact image never keys a table");
        let (ctx, zero, x, words) = (env.ctx, env.zero, env.x, &img.words.words);
        let st = state(b, env, t);
        let l_out = b.create_block(&[Type::Ptr, Type::Bool, Type::Bool]);
        let (l_app, l_probe) = (b.create_block(&[]), b.create_block(&[]));
        let bypass = b.load(Type::I64, st, ST_BYPASS);
        let on = b.icmp(IntCond::Ne, bypass, zero);
        b.cond_br(on, (l_app, &[]), (l_probe, &[]));
        b.switch_to(l_app);
        let p = self.append_n(b, env, t, st, img, h, n);
        let (fresh, counted) = (b.bconst(true), b.bconst(n.is_some()));
        b.br(l_out, &[p, fresh, counted]);

        // the table: a hit is the record; a miss opens it through the runtime's `locate` (the occupant evicted, or appended
        // when the observation switches the bypass mid-claim)
        b.switch_to(l_probe);
        let (l_hit, l_miss) = self.probe(b, env, t, st, img, h);
        b.switch_to(l_hit);
        let (p, old) = (b.block_params(l_hit)[0], b.bconst(false));
        b.br(l_out, &[p, old, old]);
        b.switch_to(l_miss);
        let slot = b.declare_stack_slot(8 * words.len() as u32, 8);
        let kp = b.stack_addr(slot);
        for (k, w) in words.iter().enumerate() {
            b.store(kp, 8 * k as i32, *w);
        }
        let locate = host(b, "agg_locate", &[Type::Ptr, Type::Ptr, Type::Ptr], gravity_abi::host_words!(gravity_kernels::table::entries::agg_locate(ctx, st, keys)));
        let r = b.call_host(locate, &[ctx, st, kp], Some(Type::I64)).expect("agg_locate returns a record or a status");
        fail_on(b, env, &x, r);
        // the observation may have switched T1 to the bypass inside `locate`: the row loop follows it from the next row
        sync_bypass(b, st);
        let p = b.conv(ConvOp::IntToPtr, Type::Ptr, r);
        let (fresh, counted) = (b.bconst(true), b.bconst(false));
        b.br(l_out, &[p, fresh, counted]);
        b.switch_to(l_out);
        (b.block_params(l_out)[0], b.block_params(l_out)[1], b.block_params(l_out)[2])
    }

    /// The home pair's probe: `l_hit(record)` or `l_miss`.
    fn probe(&self, b: &mut FunctionBuilder<'_>, env: &Env, t: &TableDecl, st: Value, img: &ImageWords, h: Value) -> (gravity_ir::Block, gravity_ir::Block) {
        let (rec, so) = (t.layout.size(), t.layout.sentinel_off() as i32);
        let (l_hit, l_miss) = (b.create_block(&[Type::Ptr]), b.create_block(&[]));
        let table = b.load(Type::Ptr, st, ST_TABLE);
        let mask = b.load(Type::I64, st, ST_MASK);
        let cap_bits = b.int_un(IntUnOp::Popcnt, mask);
        let home = emit_home(b, h, Bits::Dyn(cap_bits), Bits::Const(0));
        let next = b.int_bin(IntBinOp::Add, home, env.one);
        let next = b.int_bin(IntBinOp::And, next, mask);
        let l_second = b.create_block(&[]);
        for (i, (at, l_else)) in [(home, l_second), (next, l_miss)].into_iter().enumerate() {
            if i == 1 {
                b.switch_to(l_second);
            }
            let size = b.iconst(Type::I64, rec as i64);
            let off = b.int_bin(IntBinOp::Mul, at, size);
            let p = b.ptr_add(table, off, 1, 0);
            let c = b.load(Type::I64, p, so);
            let (empty, l_key) = (b.icmp(IntCond::Eq, c, env.zero), b.create_block(&[]));
            b.cond_br(empty, (l_miss, &[]), (l_key, &[]));
            b.switch_to(l_key);
            let eq = emit_image_eq_at(b, &img.words, p, 0);
            b.cond_br(eq, (l_hit, &[p]), (l_else, &[]));
        }
        (l_hit, l_miss)
    }
}

impl TableMech for Hashed {
    fn hashed(&self) -> bool {
        true
    }

    fn upsert(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl, img: &ImageWords, h: Value) -> (Value, Value) {
        let (r, f, _) = self.upsert_at(b, env, t, img, h, None);
        (r, f)
    }

    fn upsert_n(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl, img: &ImageWords, h: Value, n: Option<Value>) -> (Value, Value, Value) {
        self.upsert_at(b, env, t, img, h, n.filter(|_| t.layout.flags & gravity_abi::breaker::LAYOUT_IMPLICIT_COUNT != 0))
    }

    fn find(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl, img: &ImageWords, h: Value) -> Value {
        let st = state(b, env, t);
        let l_out = b.create_block(&[Type::Ptr]);
        let (l_hit, l_miss) = self.probe(b, env, t, st, img, h);
        b.switch_to(l_hit);
        let p = b.block_params(l_hit)[0];
        b.br(l_out, &[p]);
        b.switch_to(l_miss);
        let null = b.conv(ConvOp::IntToPtr, Type::Ptr, env.zero);
        b.br(l_out, &[null]);
        b.switch_to(l_out);
        b.block_params(l_out)[0]
    }

    fn state_bytes(&self, _t: &TableDecl) -> u32 {
        AGG_STATE_BYTES + 16
    }

    /// The claim's open at the size bound (`keyed_table_size::groups`).
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl) {
        let groups = crate::table::size::groups(b, env, t);
        self.open_with(b, env, t, groups);
    }

    /// The producer's end: the appended count published, the table drained into its partitions (`agg_finish`).
    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl, x: &Exits) {
        let st = state(b, env, t);
        let t1 = b.load(Type::Ptr, st, ST_T1);
        let (app, buffered) = (b.load(Type::I64, st, ST_APP), b.load(Type::I64, t1, T1_BUFFERED));
        let sum = b.int_bin(IntBinOp::Add, buffered, app);
        b.store(t1, T1_BUFFERED, sum);
        b.store(st, ST_APP, env.zero);
        let fin = host(b, "agg_finish", &[Type::Ptr, Type::Ptr], gravity_abi::host_words!(abi::agg_finish(ctx, st)));
        let r = b.call_host(fin, &[env.ctx, st], Some(Type::I64)).expect("agg_finish returns a status");
        fail_on(b, env, x, r);
    }
}

#[cfg(test)]
mod tests {
    use crate::ops::agg::testkit::{fold, want, Case};
    use super::HASHED;

    #[test]
    fn a_few_keys_fold_in_the_table_and_many_keys_bypass_into_their_partitions() {
        for (n, key, bypass) in [(10_000, (|i| i % 37) as fn(i64) -> i64, false), (300_000, |i| i % 150_000, true)] {
            let (got, bypassed) = fold(Case { n, win: 4096, key, table: &HASHED, run: None, runs: None, code: false, null: 0, dense: None });
            assert_eq!(got, want(n, key, 0), "{n} rows");
            assert_eq!(bypassed, bypass, "{n} rows: the bypass");
        }
    }
}
