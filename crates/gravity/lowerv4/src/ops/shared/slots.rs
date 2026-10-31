//! A group record slot's validity, as abi reads it, tested and marked in IR: the folds, drains and tops of several
//! lowering families read one record's slots through it.
use gravity_abi::breaker::{RecordLayout, SlotKind, LAYOUT_IMPLICIT_COUNT, MOVING};
use gravity_expr::emit::CellValid;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Type, Value};

use crate::frame::env::Env;

mod validity;

/// Slot `slot`'s validity in a record of layout `l`, offsets from the record's base (`Acc::Slot::valid`, the drain's
/// cells): the validity bit when the layout has the word, a count's too (D271: abi's `Rec::fold`, the one merge of
/// partial records, folds only a valid slot, and its `add_count` marks it; a count's class reads it as 0 when unset);
/// else the slot's own rule as abi `Rec::valid` reads it (an average by its `n`, an image or `{N, Sx, Sxx}` by its first
/// word, a `MinCode` by `MINCODE_NONE`, which the class tests itself), a count always, and any other slot of a NOT NULL
/// layout valid once a row was folded (the count sentinel).
pub(crate) fn slot_valid(l: &RecordLayout, slot: u16) -> CellValid {
    let j = slot as usize;
    let off = l.slot_off(j);
    match l.slot_kind(j) {
        _ if l.has_validity() => {
            let (o, bit) = l.valid_bit(j);
            CellValid::Bit { off: o as u32, bit }
        }
        SlotKind::Count => CellValid::Always,
        SlotKind::AvgI128 | SlotKind::AvgNum => CellValid::NonZeroAt(off + 16),
        SlotKind::MinVar | SlotKind::MaxVar | SlotKind::F64Accum => CellValid::NonZeroAt(off),
        SlotKind::MinCode => CellValid::Always,
        _ if l.flags & LAYOUT_IMPLICIT_COUNT != 0 => CellValid::Opened(l.sentinel_off()),
        _ => CellValid::NonZeroAt(l.sentinel_off()),
    }
}

/// A slot's validity over `base` as a `Bool`.
pub(crate) fn slot_ok(b: &mut FunctionBuilder<'_>, env: &Env, base: Value, v: CellValid) -> Value {
    match v {
        CellValid::Always => b.bconst(true),
        CellValid::NonZeroAt(o) => {
            let w = b.load(Type::I64, base, o as i32);
            b.icmp(IntCond::Ne, w, env.zero)
        }
        CellValid::Bit { off, bit } => {
            let byte = validity::load(b, base, off as i32);
            let m = b.iconst(Type::I64, 1 << bit);
            let x = b.int_bin(IntBinOp::And, byte, m);
            b.icmp(IntCond::Ne, x, env.zero)
        }
        CellValid::Opened(o) => {
            let w = b.load(Type::I64, base, o as i32);
            let m = b.const_raw(Type::I64, !MOVING);
            let n = b.int_bin(IntBinOp::And, w, m);
            let two = b.iconst(Type::I64, 2);
            b.icmp(IntCond::Uge, n, two)
        }
    }
}

/// Marks the slot valid where `when` holds: a validity bit is set; every other form is implicit in the slot's words.
pub(crate) fn mark(b: &mut FunctionBuilder<'_>, env: &Env, base: Value, v: CellValid, when: Value) {
    if let CellValid::Bit { off, bit } = v {
        let byte = validity::load(b, base, off as i32);
        let m = b.iconst(Type::I64, 1 << bit);
        let m = b.select(when, m, env.zero);
        let nb = b.int_bin(IntBinOp::Or, byte, m);
        let nb = b.conv(gravity_ir::ConvOp::Trunc, Type::I8, nb);
        b.store(base, off as i32, nb);
    }
}
