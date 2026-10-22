//! Prefetch hints (H04): a keyed insert or a join probe misses on a random slot per row once its table outgrows L2, so
//! the line of a row a fixed distance ahead is hinted now and its miss overlaps this row's work. The helpers here are
//! the runtime L2 gate (`above_l2`, the probe's and the row filter's), the ahead row (`row_ahead`) and the home-slot hint
//! (`home`, via `KeyHash::home`, never raw hash bits), which road `prefetch.fp` (prefetch_fp.rs) runs. Road
//! `prefetch.table` (R3-L15 R5-L07, a keyed insert's slot K rows ahead) is planned: road_off.rs `PLANNED`.
use gravity_expr::emit::hash::{emit_home, Bits};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Type, Value};

use crate::frame::env::Env;

crate::strategies! {
    /// Slot / bucket prefetch K ahead above L2 (runtime size). No mechanism yet: its readers take the straight form.
    PREFETCH_TABLE = 38, "prefetch.table", Table;
}

/// The runtime gate: a table of `bytes` above the machine's L2.
pub(crate) fn above_l2(b: &mut FunctionBuilder<'_>, bytes: Value) -> Value {
    let l2 = b.iconst(Type::I64, gravity_driver::machine::profile().l2_bytes as i64);
    b.icmp(IntCond::Ugt, bytes, l2)
}

/// Window row `j + ahead`, clamped to the window's last row; `None` outside a window.
pub(crate) fn row_ahead(b: &mut FunctionBuilder<'_>, env: &Env, j: Value, ahead: i64) -> Option<Value> {
    let win = env.win?;
    let last = b.int_bin(IntBinOp::Sub, win.end, env.one);
    let k = b.iconst(Type::I64, ahead);
    let a = b.int_bin(IntBinOp::Add, j, k);
    let over = b.icmp(IntCond::Slt, last, a);
    Some(b.select(over, last, a))
}

/// `f` under `gate` (none: always), in a block the rest of the row does not depend on.
fn gated(b: &mut FunctionBuilder<'_>, env: &mut Env, gate: Option<Value>, f: impl FnOnce(&mut FunctionBuilder<'_>, &mut Env)) {
    let Some(g) = gate else { return f(b, env) };
    let (l_hint, l_on) = (b.create_block(&[]), b.create_block(&[]));
    b.cond_br(g, (l_hint, &[]), (l_on, &[]));
    b.switch_to(l_hint);
    f(b, env);
    b.br(l_on, &[]);
    b.switch_to(l_on);
}

/// Hints the home slot of KeyHash word `h` and its neighbour (the probe's first two candidates) in `table`: 2^`cap_bits`
/// slots of `rec` bytes whose rows consumed `used` route bits. `gate`: the table's size test, or `None` to hint always.
#[allow(clippy::too_many_arguments)]
pub(crate) fn home(b: &mut FunctionBuilder<'_>, env: &mut Env, h: Value, table: Value, cap_bits: Bits, used: Bits, rec: u32, gate: Option<Value>) {
    gated(b, env, gate, |b, env| {
        let slot = emit_home(b, h, cap_bits, used);
        let p0 = slot_at(b, table, slot, rec);
        b.prefetch(p0, 0);
        let s1 = b.int_bin(IntBinOp::Add, slot, env.one);
        let cap = match cap_bits {
            Bits::Const(n) => b.iconst(Type::I64, 1i64 << n),
            Bits::Dyn(n) => b.int_bin(IntBinOp::Shl, env.one, n),
        };
        let mask = b.int_bin(IntBinOp::Sub, cap, env.one);
        let s1 = b.int_bin(IntBinOp::And, s1, mask);
        let p1 = slot_at(b, table, s1, rec);
        b.prefetch(p1, 0);
    });
}

/// Slot `slot`'s address in `table` of `rec`-byte slots.
fn slot_at(b: &mut FunctionBuilder<'_>, table: Value, slot: Value, rec: u32) -> Value {
    let r = b.iconst(Type::I64, rec as i64);
    let off = b.int_bin(IntBinOp::Mul, slot, r);
    b.ptr_add(table, off, 1, 0)
}
