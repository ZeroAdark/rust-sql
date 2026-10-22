//! The row form of a `PerCode` root (expr-compiler.md §7.3–§7.5): the code read from its lane, then
//! a bit test, a word load, an index-view length or the code word.

use gravity_dict::table::INDEX_ENTRY_LEN;
use gravity_dict::LenField;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Type, Value};

use super::{ColsRef, EmitError, LaneRef, Lowered};
use crate::percode::{Form, Root};

/// A code lane: its attno, the lane's words, the row's code when the caller loaded it once for several
/// roots (`code_at`), the lane's ordinal on its site and the window's `part_id << 32` (the render face's
/// word `part << 32 | code`).
#[derive(Clone, Copy, Debug)]
pub struct CodeRef {
    pub attno: i16,
    pub lane: LaneRef,
    pub code: Option<Value>,
    pub ordinal: u8,
    pub part: Option<Value>,
}

/// A table bound per window, in `table_needs` order; `faults` only where the caller proved the
/// header's fault bitmap non-null (a loop clone).
#[derive(Clone, Copy, Debug)]
pub enum TableRef {
    Bits { words: Value, faults: Option<Value> },
    Words { vals: Value, faults: Option<Value> },
    /// The dictionary index's length fields in place (R4): `index` is the header's `data`.
    IdxLen { index: Value, field: LenField },
    Fp128 { vals: Value },
    /// A per-code length, the window's form resolved once into the row's linear read (`len_table` / `len_read`).
    Len { base: Value, stride: Value, sa: Value, ma: Value, mb: Value },
    LenMasked { base: Value, stride: Value, mask: Value },
}

/// The code at `row`: `u16` / `u32` zero-extended, the low 32 bits of the datum word.
pub fn code_at(b: &mut FunctionBuilder<'_>, l: &LaneRef, row: Value) -> Value {
    let cp = b.ptr_add(l.ptr, row, l.width, 0);
    match l.width {
        8 => {
            let word = b.load(Type::I64, cp, 0);
            let mask = b.iconst(Type::I64, u32::MAX as i64);
            b.int_bin(IntBinOp::And, word, mask)
        }
        4 => b.load_zext(Type::I32, cp, 0),
        _ => b.load_zext(Type::I16, cp, 0),
    }
}

/// Bit `code` of the word array `words`, as `I64` 0 / 1.
pub fn bit_of(b: &mut FunctionBuilder<'_>, words: Value, code: Value) -> Value {
    let six = b.iconst(Type::I64, 6);
    let wi = b.int_bin(IntBinOp::Ushr, code, six);
    let wp = b.ptr_add(words, wi, 8, 0);
    let word = b.load(Type::I64, wp, 0);
    let m63 = b.iconst(Type::I64, 63);
    let sh = b.int_bin(IntBinOp::And, code, m63);
    let bit = b.int_bin(IntBinOp::Ushr, word, sh);
    let one = b.iconst(Type::I64, 1);
    b.int_bin(IntBinOp::And, bit, one)
}

/// The fingerprint words `[lo, hi]` of `code` from an `Fp128` table.
pub fn fingerprint(b: &mut FunctionBuilder<'_>, t: &TableRef, code: Value) -> Option<[Value; 2]> {
    let TableRef::Fp128 { vals } = *t else { return None };
    let p = b.ptr_add(vals, code, 16, 0);
    Some([b.load(Type::I64, p, 0), b.load(Type::I64, p, 8)])
}

/// `IdxLen::get(code)`: the byte length, the char field, or their delta.
fn idx_len(b: &mut FunctionBuilder<'_>, index: Value, field: LenField, code: Value) -> Value {
    let stride = b.iconst(Type::I64, INDEX_ENTRY_LEN as i64);
    let off = b.int_bin(IntBinOp::Mul, code, stride);
    let p = b.ptr_add(index, off, 1, 0);
    match field {
        LenField::Bytes => b.load_zext_unaligned(Type::I32, p, 0),
        LenField::CharsAbsolute => b.load_zext_unaligned(Type::I32, p, 4),
        LenField::CharsDelta => {
            let bytes = b.load_zext_unaligned(Type::I32, p, 0);
            let chars = b.load_zext_unaligned(Type::I32, p, 4);
            b.int_bin(IntBinOp::Sub, bytes, chars)
        }
    }
}

/// `TableRef::Len` from the window's `form` word (0 = an `I64` table's word at `code × 8`; over the index's `code × 12` entry,
/// 1 = its low `u32`, 2 = low − high, 3 = the high): the four compares and selects paid once per window, the row's read linear.
pub fn len_table(b: &mut FunctionBuilder<'_>, base: Value, form: Value) -> TableRef {
    let zero = b.iconst(Type::I64, 0);
    let two = b.iconst(Type::I64, 2);
    let three = b.iconst(Type::I64, 3);
    let eight = b.iconst(Type::I64, 8);
    let twelve = b.iconst(Type::I64, 12);
    let all = b.iconst(Type::I64, -1);
    let m32 = b.iconst(Type::I64, u32::MAX as i64);
    let s32 = b.iconst(Type::I64, 32);
    let is_words = b.icmp(IntCond::Eq, form, zero);
    let is_delta = b.icmp(IntCond::Eq, form, two);
    let is_abs = b.icmp(IntCond::Eq, form, three);
    let stride = b.select(is_words, eight, twelve);
    let sa = b.select(is_abs, s32, zero);
    let ma = b.select(is_words, all, m32);
    let mb = b.select(is_delta, m32, zero);
    TableRef::Len { base, stride, sa, ma, mb }
}

pub fn len_read(b: &mut FunctionBuilder<'_>, base: Value, stride: Value, sa: Value, ma: Value, mb: Value, code: Value) -> Value {
    let off = b.int_bin(IntBinOp::Mul, code, stride);
    let p = b.ptr_add(base, off, 1, 0);
    let v = b.load_unaligned(Type::I64, p, 0);
    let s32 = b.iconst(Type::I64, 32);
    let a = b.int_bin(IntBinOp::Ushr, v, sa);
    let a = b.int_bin(IntBinOp::And, a, ma);
    let hi = b.int_bin(IntBinOp::Ushr, v, s32);
    let bv = b.int_bin(IntBinOp::And, hi, mb);
    let d = b.int_bin(IntBinOp::Sub, a, bv);
    b.int_bin(IntBinOp::And, d, ma)
}

impl<'e, 'b, 'mcx> super::scalar::Em<'e, 'b, 'mcx> {
    pub(crate) fn percode(&mut self, r: &Root) -> Result<Lowered, EmitError> {
        let cr = *self.row.codes.iter().find(|c| c.attno == r.attno).ok_or(EmitError::NoColumn(r.attno))?;
        let ColsRef::Lanes { row, .. } = self.row.scan else { return Err(EmitError::NoColumn(r.attno)) };
        let code = match cr.code {
            Some(c) => c,
            None => code_at(self.b, &cr.lane, row),
        };
        let valid = self.lane_valid(&cr.lane, row);
        let f = self.zero;
        let (value, err) = match r.form {
            Form::Code => (code, f),
            Form::Derived => return Err(EmitError::PerCodeByRef),
            Form::Bits | Form::Words => {
                let i = r.table.expect("a tabled form has its need");
                let t = *self.row.tables.get(i as usize).ok_or(EmitError::NoTable(i))?;
                let (value, faults) = match t {
                    TableRef::Bits { words, faults } => (bit_of(self.b, words, code), faults),
                    TableRef::Words { vals, faults } => {
                        let p = self.b.ptr_add(vals, code, 8, 0);
                        (self.b.load(Type::I64, p, 0), faults)
                    }
                    TableRef::IdxLen { index, field } => (idx_len(self.b, index, field, code), None),
                    TableRef::Len { base, stride, sa, ma, mb } => (len_read(self.b, base, stride, sa, ma, mb, code), None),
                    TableRef::LenMasked { base, stride, mask } => {
                        let off = self.b.int_bin(IntBinOp::Mul, code, stride);
                        let p = self.b.ptr_add(base, off, 1, 0);
                        let v = self.b.load_unaligned(Type::I64, p, 0);
                        (self.b.int_bin(IntBinOp::And, v, mask), None)
                    }
                    TableRef::Fp128 { .. } => return Err(EmitError::NoTable(i)),
                };
                let err = match faults {
                    Some(fp) => {
                        let bit = bit_of(self.b, fp, code);
                        let raised = self.truth(bit);
                        let bad = self.and(raised, valid);
                        super::prim::raise(self.b, bad, gravity_abi::status::raise::TABLE)
                    }
                    None => f,
                };
                (value, err)
            }
        };
        // a NULL row reads the root's answer there (gravity_expr percode `nullable`)
        let (value, valid) = match r.null {
            Some(n) => {
                let k = self.b.iconst(Type::I64, n as i64);
                (self.b.select(valid, value, k), self.t)
            }
            None => (value, valid),
        };
        // PS1-15 / X5: a bound root over a NULL parameter is NULL on every row, its faulting table unread (`bound_null`)
        let (valid, err) = match self.bound_null(r) {
            Some(isnull) => {
                let ok = self.not(isnull);
                let valid = self.and(valid, ok);
                let z = self.zero;
                (valid, if matches!(self.b.const_of(err), Some((_, 0))) { err } else { self.b.select(ok, err, z) })
            }
            None => (valid, err),
        };
        Ok(Lowered { value, valid, err })
    }
}
