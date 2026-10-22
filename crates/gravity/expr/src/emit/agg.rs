//! The aggregate cells in the IR form (expr-compiler.md §9): an `Aggref` reads its cell from the
//! owning node's record (`RowRefs::rec` + `RowRefs::aggs[aggno]`: the breaker record's slots), the
//! finals over the raw slot words, `agg_final`'s numeric finals through a host once per record.

use gravity_fn::{agg_final, ErrSlot, Region};
use gravity_ir::{ConvOp, FloatBinOp, HostFlags, IntBinOp, IntCond, StateMask, Type, Value};

use super::scalar::Em;
use super::{EmitError, Lowered};
use crate::nodes::agg::Aggref;

const MOVING: i64 = 1 << 63;

/// How the datum word comes out of the slot at `off`: the `i64` word; a count word (`MOVING`
/// cleared); an `i128` sum as int8 (the low word, `err` when the high word is not its sign) or as
/// numeric (`int8_sum` over (lo, hi, the count at `count_off`)); an avg slot (`i128` sum, count
/// at `off + 16`) through `int8_avg`; `avg(float8)` as the `f64` bits over the count, in line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellForm {
    Word,
    CountWord,
    I128AsI64,
    I128Numeric { count_off: u32 },
    AvgI128,
    AvgF64 { count_off: u32 },
    /// An f64 sum rendered as float4 (`sum(float4)`).
    F32Sum,
    /// A min / max slot holding an f64's order-preserving i64 image, rendered back (an involution; NaN was mapped to
    /// i64::MAX); `f32`: the argument was float4, the datum is the demoted value's 32 bits.
    F64Ordered { f32: bool },
    /// A `numeric(p, s)` mantissa at `scale`: one i64 word (min / max), an i128 sum (`sum`), an i128 sum with its
    /// count at `off + 16` (`avg`); each rendered through a host (`gravity_fn::agg_final`'s `num_*`).
    NumWord { scale: u16 },
    NumSum { scale: u16 },
    NumAvg { scale: u16 },
    /// A `MinCode` slot holding `part << 32 | code` of code lane `lane` of site `site`, rendered through the dictionary at the emit
    /// (`min(text)` riding a pair collapse: the owner kept the code) through `RowRefs::ctx`, which refuses the cell when `None`; `MINCODE_NONE` is NULL.
    Code { site: u8, lane: u8 },
    /// D3024: a packed numeric aggregate's raw words, its final rendered lazily at each read (never for a group no
    /// expression reads it in, nor where a fused compare reads the words: lowerv4 ops/agg/numeric/agg_cmp.rs). `sum`: an
    /// i128 sum (`lo` at `off`, `hi` at `off + 8`, PostgreSQL's sum at `scale`, a special when `hi == SENT_HI`) rendered
    /// through the host at `render` over `(region, lo, hi, scale, scale)`; else a min / max mantissa word at `off` (its
    /// specials the mantissa sentinels) rendered through `num_word` over `(region, m, scale)`.
    PackedNum { sum: bool, scale: u16, render: usize },
}

gravity_abi::host! {
    /// `part << 32 | code` of lane `lane` of site `site` rendered through the dictionary: the resident image's address, 0 for
    /// `MINCODE_NONE` or a code the part does not hold. # Safety — `ctx` is the program's ctx with site `site` bound.
    pub unsafe fn render_code(ctx: u64, site: u64, lane: u64, word: u64) -> u64 {
        if word == gravity_abi::breaker::MINCODE_NONE {
            return 0;
        }
        let s = gravity_abi::ctx::site_ptr(ctx as *mut gravity_abi::Ctx, site as usize);
        let mut v = 0u64;
        if gravity_abi::site::dict_value(s, (word >> 32) as u32, lane as u32, word as u32, &mut v) < 0 {
            return 0;
        }
        v
    }
}

/// When the cell is SQL NULL: never; the `i64` at an offset is zero (a count); a bit of the validity byte;
/// the implicit count sentinel at an offset is below two (a record opened once per region plus the rows
/// folded into it: no row folded leaves every open's one).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellValid {
    Always,
    NonZeroAt(u32),
    Bit { off: u32, bit: u8 },
    Opened(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AggCell {
    pub form: CellForm,
    pub off: u32,
    pub valid: CellValid,
}

impl AggCell {
    pub const fn count(off: u32) -> AggCell {
        AggCell { form: CellForm::CountWord, off, valid: CellValid::Always }
    }
    pub const fn word(off: u32, valid: CellValid) -> AggCell {
        AggCell { form: CellForm::Word, off, valid }
    }
    pub const fn sum_i128_as_i64(off: u32, count_off: u32) -> AggCell {
        AggCell { form: CellForm::I128AsI64, off, valid: CellValid::NonZeroAt(count_off) }
    }
    pub const fn sum_i128_numeric(off: u32, count_off: u32) -> AggCell {
        AggCell { form: CellForm::I128Numeric { count_off }, off, valid: CellValid::NonZeroAt(count_off) }
    }
    pub const fn avg_i128(off: u32) -> AggCell {
        AggCell { form: CellForm::AvgI128, off, valid: CellValid::NonZeroAt(off + 16) }
    }
    pub const fn avg_f64(off: u32, count_off: u32) -> AggCell {
        AggCell { form: CellForm::AvgF64 { count_off }, off, valid: CellValid::NonZeroAt(count_off) }
    }

    pub fn is_host(&self) -> bool {
        matches!(self.form, CellForm::I128Numeric { .. } | CellForm::AvgI128 | CellForm::NumWord { .. } | CellForm::NumSum { .. } | CellForm::NumAvg { .. } | CellForm::PackedNum { .. })
    }
}

gravity_abi::host! {
    /// `int8_avg` over (lo, hi, n): the numeric image's address in `region`, 0 when it raised (the
    /// caller's `err`; the message is the replay's). # Safety — `region` is a live `Region`.
    pub unsafe fn int8_avg(region: u64, lo: u64, hi: u64, n: u64) -> u64 {
        let mut e = ErrSlot::NONE;
        settle(agg_final::int8_avg(&mut e, region as *mut Region, lo as i64, hi as i64, n as i64), &mut e)
    }

    /// `int8_sum` over (lo, hi, n), as `int8_avg`. # Safety — as `int8_avg`.
    pub unsafe fn int8_sum(region: u64, lo: u64, hi: u64, n: u64) -> u64 {
        let mut e = ErrSlot::NONE;
        settle(agg_final::int8_sum(&mut e, region as *mut Region, lo as i64, hi as i64, n as i64), &mut e)
    }

    /// `num_sum` over (lo, hi) at `scale`, as `int8_avg`. # Safety — as `int8_avg`.
    pub unsafe fn num_sum(region: u64, lo: u64, hi: u64, scale: u64) -> u64 {
        let mut e = ErrSlot::NONE;
        settle(agg_final::num_sum(&mut e, region as *mut Region, lo as i64, hi as i64, scale as i64), &mut e)
    }

    /// `num_avg` over (lo, hi, n) at `scale`. # Safety — as `int8_avg`.
    pub unsafe fn num_avg(region: u64, lo: u64, hi: u64, n: u64, scale: u64) -> u64 {
        let mut e = ErrSlot::NONE;
        settle(agg_final::num_avg(&mut e, region as *mut Region, lo as i64, hi as i64, n as i64, scale as i64), &mut e)
    }

    /// `num_word` over one mantissa at `scale`. # Safety — as `int8_avg`.
    pub unsafe fn num_word(region: u64, m: u64, scale: u64) -> u64 {
        let mut e = ErrSlot::NONE;
        settle(agg_final::num_word(&mut e, region as *mut Region, m as i64, scale as i64), &mut e)
    }

    /// D4565: `var_pop` over an int2 / int4 Int128AggState (sumX, sumX2, N): agg_final `int128_stat`. # Safety — as
    /// `int8_avg`.
    pub unsafe fn int_var_pop(region: u64, sx_lo: u64, sx_hi: u64, sq_lo: u64, sq_hi: u64, n: u64) -> u64 {
        int_stat(region, [sx_lo, sx_hi, sq_lo, sq_hi, n], false, false)
    }

    /// D4565: `var_samp` / `variance`, as `int_var_pop`. # Safety — as `int8_avg`.
    pub unsafe fn int_var_samp(region: u64, sx_lo: u64, sx_hi: u64, sq_lo: u64, sq_hi: u64, n: u64) -> u64 {
        int_stat(region, [sx_lo, sx_hi, sq_lo, sq_hi, n], false, true)
    }

    /// D4565: `stddev_pop`, as `int_var_pop`. # Safety — as `int8_avg`.
    pub unsafe fn int_stddev_pop(region: u64, sx_lo: u64, sx_hi: u64, sq_lo: u64, sq_hi: u64, n: u64) -> u64 {
        int_stat(region, [sx_lo, sx_hi, sq_lo, sq_hi, n], true, false)
    }

    /// D4565: `stddev_samp` / `stddev`, as `int_var_pop`. # Safety — as `int8_avg`.
    pub unsafe fn int_stddev_samp(region: u64, sx_lo: u64, sx_hi: u64, sq_lo: u64, sq_hi: u64, n: u64) -> u64 {
        int_stat(region, [sx_lo, sx_hi, sq_lo, sq_hi, n], true, true)
    }
}

/// D4565: agg_final `int128_stat` over the state words `w` (`[sx_lo, sx_hi, sq_lo, sq_hi, n]`) into `region`.
fn int_stat(region: u64, w: [u64; 5], stddev: bool, sample: bool) -> u64 {
    let mut e = ErrSlot::NONE;
    settle(agg_final::int128_stat(&mut e, region as *mut Region, w[0] as i64, w[1] as i64, w[2] as i64, w[3] as i64, w[4] as i64, stddev, sample), &mut e)
}

fn settle(r: gravity_fn::Ret, e: &mut ErrSlot) -> u64 {
    if r.status == gravity_fn::OK {
        return r.val as u64;
    }
    drop(e.take());
    0
}

impl Em<'_, '_, '_> {
    pub(crate) fn aggref(&mut self, n: &Aggref) -> Result<Lowered, EmitError> {
        let (Some(cell), Some(rec)) = (self.row.aggs.get(n.aggno as usize).copied(), self.row.rec) else { return Err(EmitError::NoAggCell(n.aggno as u32)) };
        let valid = self.cell_valid(rec, cell.valid);
        let off = cell.off as i32;
        match cell.form {
            CellForm::Word => {
                let value = self.b.load(Type::I64, rec, off);
                Ok(Lowered { value, valid, err: self.zero })
            }
            CellForm::CountWord => {
                let w = self.b.load(Type::I64, rec, off);
                let mask = self.b.iconst(Type::I64, !MOVING);
                let value = self.b.int_bin(IntBinOp::And, w, mask);
                Ok(Lowered { value, valid, err: self.zero })
            }
            CellForm::I128AsI64 => {
                let lo = self.b.load(Type::I64, rec, off);
                let hi = self.b.load(Type::I64, rec, off + 8);
                let s63 = self.b.iconst(Type::I64, 63);
                let sign = self.b.int_bin(IntBinOp::Sshr, lo, s63);
                let ovf = self.b.icmp(IntCond::Ne, hi, sign);
                let bad = self.and(valid, ovf);
                let err = super::prim::raise(self.b, bad, gravity_abi::status::raise::INT8_RANGE);
                Ok(Lowered { value: lo, valid, err })
            }
            CellForm::AvgF64 { count_off } => {
                let bits = self.b.load(Type::I64, rec, off);
                let n = self.b.load(Type::I64, rec, count_off as i32);
                let sum = self.b.conv(ConvOp::Bitcast, Type::F64, bits);
                let nf = self.b.conv(ConvOp::SintToFloat, Type::F64, n);
                let q = self.b.float_bin(FloatBinOp::Div, sum, nf);
                let value = self.b.conv(ConvOp::Bitcast, Type::I64, q);
                Ok(Lowered { value, valid, err: self.zero })
            }
            CellForm::F32Sum => {
                let bits = self.b.load(Type::I64, rec, off);
                let sum = self.b.conv(ConvOp::Bitcast, Type::F64, bits);
                let f = self.b.conv(ConvOp::Fdemote, Type::F32, sum);
                let b32 = self.b.conv(ConvOp::Bitcast, Type::I32, f);
                let value = self.b.conv(ConvOp::Zext, Type::I64, b32);
                Ok(Lowered { value, valid, err: self.zero })
            }
            CellForm::F64Ordered { f32 } => {
                let img = self.b.load(Type::I64, rec, off);
                let s63 = self.b.iconst(Type::I64, 63);
                let imax = self.b.iconst(Type::I64, i64::MAX);
                let sign = self.b.int_bin(IntBinOp::Sshr, img, s63);
                let mask = self.b.int_bin(IntBinOp::And, sign, imax);
                let bits = self.b.int_bin(IntBinOp::Xor, img, mask);
                let value = if f32 {
                    let f = self.b.conv(ConvOp::Bitcast, Type::F64, bits);
                    let n = self.b.conv(ConvOp::Fdemote, Type::F32, f);
                    let b32 = self.b.conv(ConvOp::Bitcast, Type::I32, n);
                    self.b.conv(ConvOp::Zext, Type::I64, b32)
                } else {
                    bits
                };
                Ok(Lowered { value, valid, err: self.zero })
            }
            CellForm::I128Numeric { count_off } => {
                let n = self.b.load(Type::I64, rec, count_off as i32);
                let (lo, hi) = self.i128_words(rec, off);
                self.final_host("gravity_expr::int8_sum", gravity_abi::host_words!(int8_sum(region, lo, hi, n)), &[lo, hi, n], valid)
            }
            CellForm::AvgI128 => {
                let n = self.b.load(Type::I64, rec, off + 16);
                let (lo, hi) = self.i128_words(rec, off);
                self.final_host("gravity_expr::int8_avg", gravity_abi::host_words!(int8_avg(region, lo, hi, n)), &[lo, hi, n], valid)
            }
            CellForm::NumSum { scale } => {
                let (lo, hi) = self.i128_words(rec, off);
                let sc = self.b.iconst(Type::I64, scale as i64);
                self.final_host("gravity_expr::num_sum", gravity_abi::host_words!(num_sum(region, lo, hi, scale)), &[lo, hi, sc], valid)
            }
            CellForm::NumAvg { scale } => {
                let n = self.b.load(Type::I64, rec, off + 16);
                let (lo, hi) = self.i128_words(rec, off);
                let sc = self.b.iconst(Type::I64, scale as i64);
                self.final_host("gravity_expr::num_avg", gravity_abi::host_words!(num_avg(region, lo, hi, n, scale)), &[lo, hi, n, sc], valid)
            }
            CellForm::NumWord { scale } => {
                let m = self.b.load(Type::I64, rec, off);
                let sc = self.b.iconst(Type::I64, scale as i64);
                self.final_host("gravity_expr::num_word", gravity_abi::host_words!(num_word(region, m, scale)), &[m, sc], valid)
            }
            CellForm::PackedNum { sum, scale, render } => {
                let sc = self.b.iconst(Type::I64, scale as i64);
                if sum {
                    let (lo, hi) = self.i128_words(rec, off);
                    self.local_final("class_num::num_sum_ds", render, &[lo, hi, sc, sc], valid)
                } else {
                    let m = self.b.load(Type::I64, rec, off);
                    self.local_final("gravity_expr::num_word", gravity_abi::host_words!(num_word(region, m, scale)), &[m, sc], valid)
                }
            }
            CellForm::Code { site, lane } => {
                let Some(ctx) = self.row.ctx else { return Err(EmitError::NoCells) };
                let flags = HostFlags { may_error: false, pure: false, touches_memory: true, worker: false, state: StateMask::NONE };
                let h = self.b.declare_host_once("gravity_expr::render_code", &[Type::Ptr, Type::I64, Type::I64, Type::I64], Some(Type::I64), render_code as *const () as usize, flags);
                let word = self.b.load(Type::I64, rec, off);
                let s = self.b.iconst(Type::I64, site as i64);
                let l = self.b.iconst(Type::I64, lane as i64);
                let value = self.b.call_host(h, &[ctx, s, l, word], Some(Type::I64)).unwrap();
                let has = self.b.icmp(IntCond::Ne, value, self.zero);
                let valid = self.and(valid, has);
                Ok(Lowered { value, valid, err: self.zero })
            }
        }
    }

    fn cell_valid(&mut self, rec: Value, v: CellValid) -> Value {
        match v {
            CellValid::Always => self.t,
            CellValid::NonZeroAt(off) => {
                let n = self.b.load(Type::I64, rec, off as i32);
                self.b.icmp(IntCond::Ne, n, self.zero)
            }
            CellValid::Bit { off, bit } => {
                let byte = self.b.load_zext(Type::I8, rec, off as i32);
                let m = self.b.iconst(Type::I64, 1 << bit);
                let b = self.b.int_bin(IntBinOp::And, byte, m);
                self.b.icmp(IntCond::Ne, b, self.zero)
            }
            CellValid::Opened(off) => {
                let w = self.b.load(Type::I64, rec, off as i32);
                let mask = self.b.iconst(Type::I64, !MOVING);
                let n = self.b.int_bin(IntBinOp::And, w, mask);
                let two = self.b.iconst(Type::I64, 2);
                self.b.icmp(IntCond::Uge, n, two)
            }
        }
    }

    fn i128_words(&mut self, rec: Value, off: i32) -> (Value, Value) {
        (self.b.load(Type::I64, rec, off), self.b.load(Type::I64, rec, off + 8))
    }

    /// D3024: `final_host` into a region of its own on the frame's stack (a 128-byte buffer: a numeric of an i128 sum's
    /// digits fits), as lowerv4's eager finals render (ops/agg/trans/slots.rs `host_final`): a lazily read final needs no
    /// row region.
    fn local_final(&mut self, name: &'static str, host: usize, words: &[Value], valid: Value) -> Result<Lowered, EmitError> {
        let flags = HostFlags { may_error: true, pure: false, touches_memory: true, worker: false, state: StateMask::NONE };
        let mut params = [Type::I64; 6];
        params[0] = Type::Ptr;
        let h = self.b.declare_host_once(name, &params[..words.len() + 1], Some(Type::I64), host, flags);
        let l_call = self.b.create_block(&[]);
        let l_join = self.b.create_block(&[Type::I64, Type::I64]);
        let z = self.zero;
        self.b.cond_br(valid, (l_call, &[]), (l_join, &[z, z]));
        self.b.switch_to(l_call);
        let (rs, buf) = (self.b.declare_stack_slot(16, 8), self.b.declare_stack_slot(128, 8));
        let (r, p) = (self.b.stack_addr(rs), self.b.stack_addr(buf));
        let end = self.b.ptr_add(p, z, 1, 128);
        self.b.store(r, 0, p);
        self.b.store(r, 8, end);
        let mut args = [r; 6];
        args[1..words.len() + 1].copy_from_slice(words);
        let v = self.b.call_host(h, &args[..words.len() + 1], Some(Type::I64)).unwrap();
        let failed = self.b.icmp(IntCond::Eq, v, z);
        let e = super::prim::raise(self.b, failed, gravity_abi::status::raise::HOST);
        self.b.br(l_join, &[v, e]);
        self.b.switch_to(l_join);
        let ps = self.b.block_params(l_join);
        Ok(Lowered { value: ps[0], valid, err: ps[1] })
    }

    /// A numeric final through `host` over `(region, words…)`, entered only where the cell is valid.
    fn final_host(&mut self, name: &'static str, host: usize, words: &[Value], valid: Value) -> Result<Lowered, EmitError> {
        let Some(region) = self.row.region else { return Err(EmitError::NoCells) };
        let flags = HostFlags { may_error: true, pure: false, touches_memory: true, worker: false, state: StateMask::NONE };
        let mut params = [Type::I64; 6];
        params[0] = Type::Ptr;
        let h = self.b.declare_host_once(name, &params[..words.len() + 1], Some(Type::I64), host, flags);
        let l_call = self.b.create_block(&[]);
        let l_join = self.b.create_block(&[Type::I64, Type::I64]);
        let z = self.zero;
        self.b.cond_br(valid, (l_call, &[]), (l_join, &[z, z]));
        self.b.switch_to(l_call);
        let mut args = [region; 6];
        args[1..words.len() + 1].copy_from_slice(words);
        let p = self.b.call_host(h, &args[..words.len() + 1], Some(Type::I64)).unwrap();
        let failed = self.b.icmp(IntCond::Eq, p, z);
        let e = super::prim::raise(self.b, failed, gravity_abi::status::raise::HOST);
        self.b.br(l_join, &[p, e]);
        self.b.switch_to(l_join);
        let ps = self.b.block_params(l_join);
        Ok(Lowered { value: ps[0], valid, err: ps[1] })
    }
}
