use core::cmp::Ordering;
use core::mem::size_of;

use crate::hash::{build_sparse_rows, Depth, KeyHash, BUILD_IN_PLACE_LOAD, BUILD_MIN_CAP_BITS, BUILD_SPARSE_LOAD};

/// D1704: a table's typed view `&mut T` over region bytes at `p`, its alignment asserted in debug builds (a borrow through a
/// raw pointer is not checked; a driver region is `REGION_ALIGN`-aligned, and Miri places a block at exactly the ask).
/// # Safety
/// `p` points at a live `T`, aligned for it, not otherwise borrowed for `'a`.
#[inline(always)]
pub(crate) unsafe fn aligned_mut<'a, T>(p: *mut u8) -> &'a mut T {
    debug_assert!(p.cast::<T>().is_aligned(), "gravity: a {} view at {p:p} is not {}-aligned (D1704)", core::any::type_name::<T>(), core::mem::align_of::<T>());
    &mut *p.cast::<T>()
}

/// D1704: `aligned_mut`'s shared view.
/// # Safety
/// `p` points at a live `T`, aligned for it, not mutably borrowed for `'a`.
#[inline(always)]
pub(crate) unsafe fn aligned_ref<'a, T>(p: *const u8) -> &'a T {
    debug_assert!(p.cast::<T>().is_aligned(), "gravity: a {} view at {p:p} is not {}-aligned (D1704)", core::any::type_name::<T>(), core::mem::align_of::<T>());
    &*p.cast::<T>()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BreakerKind {
    PlainAgg,
    HashAgg,
    HashBuild,
    Sort,
    Materialize,
    Distinct,
    TopK,
}

/// What bounds a breaker's memory: a program constant or the execution's grant share (§2 decision 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Budget {
    Fixed(u32),
    Share,
    /// A share whose region grows IN PLACE past it: a set operation's side, as PostgreSQL's hashed SetOp keeps its table in
    /// memory past work_mem (nodeSetOp.c has no spill). The driver binds its region inside an address-space reservation of
    /// the side's rows bound (`T1::at_reserved`), armed and charged at the share; the keyed fold commits more of it when its pool
    /// fills (`SPILL_REGION_COMMIT`, `T1::grow_pool`: drawn as past-share growth) instead of spilling, and spills as today
    /// once the reservation is spent or the draw is refused. Everywhere else it is a `Share`.
    Pool,
}

impl Budget {
    /// The execution's grant share bounds it (`Share`, or `Pool`: a share that grows in place past it).
    pub const fn is_share(self) -> bool {
        matches!(self, Budget::Share | Budget::Pool)
    }
}

/// What a breaker does past its budget (§2 decision 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Spill {
    None,
    Partitioned { bits: u8 },
    Runs,
}

/// A pipeline's breaker (§2 decision 1): kind, Node slot, program-constant bytes, budget, spill posture.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Breaker {
    pub kind: BreakerKind,
    pub slot: u8,
    pub bytes: u32,
    pub budget: Budget,
    pub spill: Spill,
}

impl Breaker {
    pub const fn sized(kind: BreakerKind, bytes: u32) -> Breaker {
        Breaker { kind, slot: 0, bytes, budget: Budget::Fixed(bytes), spill: Spill::None }
    }

    pub const fn in_slot(mut self, slot: u8) -> Breaker {
        self.slot = slot;
        self
    }
}

pub const MAX_RECORD_KEYS: usize = 8;
pub const MAX_RECORD_SLOTS: usize = 8;
/// The representatives an owner renders (the `(slot, lane)` array of an owner's spec and the width of its tie mask): one
/// layout fact, read by the program's `OwnerDesc` and the kernels' `Reps`. A representative names a record slot, so a
/// record holds at most one per slot: the array is the record's slot capacity, never a cap of its own.
pub const MAX_REPS: usize = MAX_RECORD_SLOTS;
/// The words of an owner's params image of `MAX_REPS` representatives (kernels `dict::reps::pack_slot`), four a word.
pub const REP_WORDS: usize = MAX_REPS.div_ceil(4);

/// The NULL flags words a key image (and so its record's key words) carries after its image words when some key is
/// nullable: one per 64 keys (hash.rs's image, §3.1). Every reader of a key's NULL flag, IR and runtime, finds it through
/// this and [`flag_at`], so no two keys share a bit however many keys there are (CAP-28).
pub const fn flag_words(nkeys: usize) -> usize {
    nkeys.div_ceil(64)
}

/// Key `k`'s NULL flag: its flags word (0 the first, right after the image words) and its bit in that word.
pub const fn flag_at(k: usize) -> (usize, u32) {
    (k / 64, (k % 64) as u32)
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyKind {
    I32 = 0,
    I64,
    Code,
    Date,
    Timestamp,
    /// One word of a 128-bit text fingerprint (`gravity_dict::fp`): two adjacent `Fp128` words, low then high.
    Fp128,
    /// A float4 / float8 key: the datum's bits (a float4's sign-extended); the order is the type's, not the word's.
    F32,
    F64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyCell {
    I32(i32),
    I64(i64),
    Code { part: u32, code: u32 },
    Date(i32),
    Timestamp(i64),
    Fp128(u64),
    F32(u32),
    F64(u64),
}

impl KeyCell {
    pub const fn word(self) -> u64 {
        match self {
            KeyCell::I32(v) | KeyCell::Date(v) => v as i64 as u64,
            KeyCell::I64(v) | KeyCell::Timestamp(v) => v as u64,
            KeyCell::Code { part, code } => ((part as u64) << 32) | code as u64,
            KeyCell::Fp128(w) | KeyCell::F64(w) => w,
            KeyCell::F32(v) => v as i32 as i64 as u64,
        }
    }

    pub const fn code_word(part: u32, code: u32) -> u64 {
        ((part as u64) << 32) | code as u64
    }

    pub const fn from_word(kind: KeyKind, w: u64) -> KeyCell {
        match kind {
            KeyKind::I32 => KeyCell::I32(w as i32),
            KeyKind::I64 => KeyCell::I64(w as i64),
            KeyKind::Code => KeyCell::Code { part: (w >> 32) as u32, code: w as u32 },
            KeyKind::Date => KeyCell::Date(w as i32),
            KeyKind::Timestamp => KeyCell::Timestamp(w as i64),
            KeyKind::Fp128 => KeyCell::Fp128(w),
            KeyKind::F32 => KeyCell::F32(w as u32),
            KeyKind::F64 => KeyCell::F64(w),
        }
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SlotKind {
    Count = 0,
    SumI64,
    SumI128,
    MinI64,
    MaxI64,
    AvgI128,
    SumF64,
    MinCode,
    MaxCode,
    /// The i128 sum of numeric mantissas at a fixed scale (milestone-2.md §2.6 item 29; the scale is the program's).
    SumNum,
    /// `SumNum` with its row count (the `avg(numeric)` form).
    AvgNum,
    /// `min / max(text)`: the group's image as a 4-byte-header varlena in a statement-lifetime arena (`agg_image_put`), 0 for none;
    /// two images merge by their bytes then their lengths (the C / POSIX order; the lowerer refuses another collation by name).
    MinVar,
    MaxVar,
    /// `sum(float4)`: PG's float4 state, the f64 of its f32 value; merged by `float4_pl` (D244).
    SumF32,
    /// `avg(float4 / float8)`: PG's float8[3] state {N, Sx, Sxx} (float8_accum's Youngs–Cramer), three f64 words, merged
    /// by `float8_combine` (D244); valid without the validity word while N != 0.
    F64Accum,
    /// `bit_and / bit_or / bit_xor` over an integer: the state starts NULL (strict, no initcond), so the first value is
    /// copied and the rest merge by `&`, `|`, `^` (D252). `BitAnd` always rides a validity layout (decompose).
    BitAnd,
    BitOr,
    BitXor,
    /// V4R-38 (B-lite): `SumNum` as an i256 (four words, `NumWide`) for a sum whose terms can pass i128 by
    /// typmod (lowerv4 class.num_wide: a packed factor times a one-word one, up to 55 digits); merged by `num_add256`,
    /// its specials the same sentinel on the top word.
    SumNumWide,
    /// `SumNumWide` with its row count at +32 (the `avg(numeric)` form).
    AvgNumWide,
    /// M6 (V4R-277): a hosted aggregate's state (lowerv4 face/aggcall.rs, kernels host_abi/v4/m6_agg.rs): the transition
    /// value word, its NULL / noTransValue / held bytes (`AGGSTATE_HELD`), the memory in the statement's aggregate arena.
    /// Never merged by bytes: `move_state` moves the one held state into an empty slot; two held states raise.
    AggState,
    /// `min / max(interval)`: PG's 16-byte Interval image (time i64 at +0, day i32 at +8, month i32 at +12), ordered by
    /// `interval_cmp_value` (time + (month * 30 + day) * USECS_PER_DAY, as an i128). Equal values can print apart ('1 day' /
    /// '24:00:00'), so the merge keeps PG's member: `interval_smaller / larger(into, from)` keep `from` on a tie, so `src` (the
    /// later part) wins a tie (`Rec::fold_interval`).
    IntervalMin,
    IntervalMax,
    /// NEW-numeric-1 N13: an order-key extreme's companion, the slot right after its `MinI64` / `MaxI64` key slot in a
    /// `LAYOUT_KEY_TIES` layout. 0, or the address of the group's extreme over the values its rows could not key (past 16
    /// significant digits or dscale 15): a 4-byte-header numeric varlena behind an 8-byte capacity word (lowerv4
    /// class_num's own copy, overwritten in place), with `NUM_SIDE_T` in its low bit: set where the later of the record's
    /// two members equal in value is this one (`Rec::side_extreme`). Two images merge by numeric value, the later taking
    /// a tie, as numeric_smaller / numeric_larger return their second argument.
    NumSideMin,
    NumSideMax,
    /// `sum / avg(interval)`: PG's IntervalAggState (`N`, `pInfcount`, `nInfcount`, `sumX`) with, per component, the
    /// saturating sum of the finite inputs' absolute values, 64 bytes (`IntervalSum`'s words, `Rec::interval_sum`). The
    /// merge is `IntervalSum::compose`, order-free: no input order can overflow while every absolute sum stays under its
    /// component's maximum.
    IntervalSum,
}

/// NEW-numeric-1 N13: a `NumSideMin` / `NumSideMax` word's low bit: the later of the record's key member and side member
/// equal in value is the side's (images are 8-aligned, so the bit is free; `side_image` masks it).
pub const NUM_SIDE_T: u64 = 1;

/// A `NumSideMin` / `NumSideMax` word's image address (0: none).
#[inline(always)]
pub const fn side_image(w: u64) -> u64 {
    w & !NUM_SIDE_T
}

/// NEW-numeric-1 N13: a held side word `a` and a source's `s` (0: none) folded, `key_took` saying whether the paired key
/// slot took the source's member: (the word, whether the side took the source's image, whether the pair is unordered).
/// The images compare by numeric value (`numeric_image_cmp`); `NEWER` (the source holds the later rows) takes the
/// source's on a tie, as numeric_smaller / numeric_larger keep their second argument; otherwise a tie of two byte forms
/// is unordered. The tie bit composes from the two forms' takes: both from the source keep its bit, both kept keep the
/// held one, and a form taken from the source alone holds the later member (the source holds no member of the other
/// form at that value, else that form would have taken it on the tie). The image moves by address: a merge's source is
/// a partial it discards.
pub fn side_fold<const NEWER: bool>(a: u64, s: u64, key_took: bool, min: bool) -> (u64, bool, bool) {
    let (ai, si) = (side_image(a), side_image(s));
    let (side_took, unordered) = match (ai, si) {
        (_, 0) => (false, false),
        (0, _) => (true, false),
        _ => {
            // SAFETY: both are addresses of 4-byte-header numeric varlenas class_num copied, alive to the statement's end
            let c = unsafe { numeric_image_cmp(si as *const u8, ai as *const u8) };
            let take = match (NEWER, min) {
                (true, true) => c != Ordering::Greater,
                (true, false) => c != Ordering::Less,
                (false, true) => c == Ordering::Less,
                (false, false) => c == Ordering::Greater,
            };
            // SAFETY: as above
            (take, !NEWER && c == Ordering::Equal && unsafe { varlena_cmp(si as *const u8, ai as *const u8) } != Ordering::Equal)
        }
    };
    let img = if side_took { si } else { ai };
    if img == 0 {
        return (0, false, unordered);
    }
    let t = match (key_took, side_took) {
        (true, true) => s & NUM_SIDE_T,
        (true, false) => 0,
        (false, true) => NUM_SIDE_T,
        (false, false) => a & NUM_SIDE_T,
    };
    (img | t, side_took, unordered)
}

/// `interval_cmp_value` (adt_timestamp interval.rs): the i128 an interval orders by.
#[inline(always)]
pub fn interval_cmp_value(time: i64, day: i32, month: i32) -> i128 {
    time as i128 + (month as i64 * 30 + day as i64) as i128 * 86_400_000_000i128
}

/// An `IntervalSum` state whose fold ran under the statement's recheck licence (facts/order.rs `rechecked`): a state
/// that may overflow raises at once, and the plan-order re-run answers.
pub const IVSUM_LICENSED: u64 = 1;
/// An `IntervalSum` state whose unlicensed record merge overflowed as PG's `interval_agg_combine` does: its final raises
/// PG's "interval out of range".
pub const IVSUM_OVERFLOWED: u64 = 2;

/// `IntervalSum`'s state, its eight words in slot order: `time`, `day | month << 32` (PG's 16-byte Interval image, added
/// wrapping), `abs_time`, `abs_day`, `abs_month` (saturating), `n`, `pinf | ninf << 32`, `flags`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IntervalSum {
    pub time: i64,
    pub day: i32,
    pub month: i32,
    pub abs_time: u64,
    pub abs_day: u64,
    pub abs_month: u64,
    pub n: i64,
    pub pinf: u32,
    pub ninf: u32,
    pub flags: u64,
}

/// PG's `INTERVAL_NOT_FINITE`: the `-infinity` or `+infinity` image.
#[inline(always)]
pub const fn interval_not_finite(time: i64, day: i32, month: i32) -> bool {
    (time == i64::MIN && day == i32::MIN && month == i32::MIN) || (time == i64::MAX && day == i32::MAX && month == i32::MAX)
}

impl IntervalSum {
    /// Whether some input order of the folded finite values may leave a component's range or land on an infinity image:
    /// a component's absolute sum reaches its type's maximum (below it, every prefix in every order stays strictly inside).
    #[inline(always)]
    pub const fn may_overflow(&self) -> bool {
        self.abs_time >= i64::MAX as u64 || self.abs_day >= i32::MAX as u64 || self.abs_month >= i32::MAX as u64
    }

    /// One input row, as PG's `do_interval_accum`: an infinity counts; a finite value adds to the sums as
    /// `finite_interval_pl` (wrapping here; `true` when PG raises there: a component past its range, or the sum an
    /// infinity image) and to the absolute sums.
    pub fn accum(&mut self, time: i64, day: i32, month: i32) -> bool {
        if time == i64::MIN && day == i32::MIN && month == i32::MIN {
            self.ninf += 1;
            return false;
        }
        if time == i64::MAX && day == i32::MAX && month == i32::MAX {
            self.pinf += 1;
            return false;
        }
        self.n += 1;
        self.abs_time = self.abs_time.saturating_add(time.unsigned_abs());
        self.abs_day = self.abs_day.saturating_add(day.unsigned_abs() as u64);
        self.abs_month = self.abs_month.saturating_add(month.unsigned_abs() as u64);
        self.add_sum(time, day, month)
    }

    fn add_sum(&mut self, time: i64, day: i32, month: i32) -> bool {
        let (m, om) = self.month.overflowing_add(month);
        let (d, od) = self.day.overflowing_add(day);
        let (t, ot) = self.time.overflowing_add(time);
        (self.time, self.day, self.month) = (t, d, m);
        om || od || ot || interval_not_finite(t, d, m)
    }

    /// The one compose of two states, `self` the earlier part and `b` the later (abi `Rec::fold_interval_sum`, exec / hosts
    /// pair merges, lowerv4 class_interval_sum.rs's merge): PG's `interval_agg_combine` (counts add, the sums by
    /// `finite_interval_pl`: an overflow sets `IVSUM_OVERFLOWED`), the absolute sums add saturating, flags OR. Returns
    /// whether the statement raises now: a licensed state that may overflow (the plan-order re-run then answers).
    pub fn compose(&mut self, b: &IntervalSum) -> bool {
        self.n += b.n;
        self.pinf += b.pinf;
        self.ninf += b.ninf;
        self.abs_time = self.abs_time.saturating_add(b.abs_time);
        self.abs_day = self.abs_day.saturating_add(b.abs_day);
        self.abs_month = self.abs_month.saturating_add(b.abs_month);
        self.flags |= b.flags;
        if b.n > 0 && self.add_sum(b.time, b.day, b.month) {
            self.flags |= IVSUM_OVERFLOWED;
        }
        self.flags & IVSUM_LICENSED != 0 && self.may_overflow()
    }
}

/// An `AggState` slot's held byte (kernels host_abi/v4/m6_agg.rs `STATE_INIT`): 0 until the first transition initializes it.
pub const AGGSTATE_HELD: usize = 10;

impl SlotKind {
    /// Whether this kind's merge keeps one of several equal members, the later (`Rec::fold_interval`: interval_smaller /
    /// larger keep their second argument): a W0 owner then folds a partition's partials oldest first, as a hosted state's
    /// (parallel merge.rs `fold_sub`), so the member is PG's, the later in input order.
    pub const fn keeps_later_member(self) -> bool {
        matches!(self, SlotKind::IntervalMin | SlotKind::IntervalMax | SlotKind::NumSideMin | SlotKind::NumSideMax)
    }

    /// NEW-numeric-1 N13: an order-key extreme's side companion.
    pub const fn is_num_side(self) -> bool {
        matches!(self, SlotKind::NumSideMin | SlotKind::NumSideMax)
    }

    /// Whether a W0 owner folds a partition's partials of this kind oldest first (parallel merge.rs `fold_sub`, a group's
    /// partials in input order): a kind that keeps the later of equal members, and `IntervalSum`, whose unlicensed merge
    /// raises as PG's `interval_agg_combine` does, so its parts compose in PG's order (a plan-order run that spills).
    pub const fn folds_oldest_first(self) -> bool {
        self.keeps_later_member() || matches!(self, SlotKind::IntervalSum)
    }

    pub const fn bytes(self) -> u8 {
        match self {
            SlotKind::SumI128 | SlotKind::SumNum | SlotKind::AggState | SlotKind::IntervalMin | SlotKind::IntervalMax => 16,
            SlotKind::AvgI128 | SlotKind::AvgNum | SlotKind::F64Accum => 24,
            SlotKind::SumNumWide => 32,
            SlotKind::AvgNumWide => 40,
            SlotKind::IntervalSum => 64,
            _ => 8,
        }
    }
}

/// PostgreSQL's `float_overflow_error` (CHECKFLOATVAL): a float result turned infinite from finite operands. Callers raise
/// "value out of range: overflow" (`status::raise::FLOAT_OVF`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FloatOverflow;

impl FloatOverflow {
    /// Its raise mask (22003 "value out of range: overflow").
    pub const MASK: u64 = crate::status::raise::bit(crate::status::raise::FLOAT_OVF);
    /// The status a host returns for it: an `ERROR` raising `FLOAT_OVF`.
    pub const STATUS: i64 = crate::status::raised(Self::MASK);
}

/// `float8_pl` (utils/float.h): `a + b`, an infinite sum of finite operands an overflow.
pub fn float8_pl(a: f64, b: f64) -> Result<f64, FloatOverflow> {
    let r = a + b;
    if r.is_infinite() && !a.is_infinite() && !b.is_infinite() {
        return Err(FloatOverflow);
    }
    Ok(r)
}

/// `float4_pl` (utils/float.h), in float4.
pub fn float4_pl(a: f32, b: f32) -> Result<f32, FloatOverflow> {
    let r = a + b;
    if r.is_infinite() && !a.is_infinite() && !b.is_infinite() {
        return Err(FloatOverflow);
    }
    Ok(r)
}

/// `float8_combine` (utils/adt/float.c) over two {N, Sx, Sxx} states: an empty side yields the other; else N adds, Sx by
/// `float8_pl`, Sxx by the generalised Youngs–Cramer rule. PG's overflow (Sx by `float8_pl`, or an infinite Sxx from
/// finite ones) is deferred (D432): it leaves Sxx = +Inf (`F64_ACCUM_TAINT`; PG's Sxx is never infinite), as does a
/// tainted side, and the final raises it unless Sx is NaN — the rows fold in another order than PG's scan, and PG's
/// overflow depends on that order (`{NaN, 0.5, 1e300}` is NaN, `{0.5, 1e300, NaN}` raises). Never `Err` since D432.
pub fn float8_combine(a: [f64; 3], b: [f64; 3]) -> Result<[f64; 3], FloatOverflow> {
    let ([n1, sx1, sxx1], [n2, sx2, sxx2]) = (a, b);
    if n1 == 0.0 {
        return Ok(b);
    }
    if n2 == 0.0 {
        return Ok(a);
    }
    let n = n1 + n2;
    let (sx, o1) = match float8_pl(sx1, sx2) {
        Ok(v) => (v, false),
        Err(_) => (sx1 + sx2, true),
    };
    let tmp = sx1 / n1 - sx2 / n2;
    let sxx = sxx1 + sxx2 + n1 * n2 * tmp * tmp / n;
    let o2 = sxx.is_infinite() && !sxx1.is_infinite() && !sxx2.is_infinite();
    let tainted = o1 || o2 || sxx1 == F64_ACCUM_TAINT || sxx2 == F64_ACCUM_TAINT;
    Ok([n, sx, if tainted { F64_ACCUM_TAINT } else { sxx }])
}

/// An `F64Accum` state's deferred overflow: its Sxx (D432).
pub const F64_ACCUM_TAINT: f64 = f64::INFINITY;

/// `fold_run`'s status for a float merge that overflowed (every other negative is `-1 - folded`, a full table).
pub const FOLD_OVERFLOW: i64 = i64::MIN;
/// V4R-375: `fold_run_with`'s status for a hosted combine that raised (the host's error stashed: `raise::HOST`).
pub const FOLD_HOST: i64 = i64::MIN + 1;

/// V4R-375: a hosted state's combine in an owner fold: `(src, slot, into, from)`, `src` the source record, `into` / `from`
/// slot `slot`'s `AggState` bytes of the owner's record and of `src` (only read); false when it raised (error stashed).
pub type StateCombine<'a> = dyn FnMut(&Rec<'_>, usize, *mut u8, *const u8) -> bool + 'a;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AggSlot {
    Count(i64),
    SumI64(i64),
    SumI128(i128),
    MinI64(i64),
    MaxI64(i64),
    AvgI128 { sum: i128, n: i64 },
    SumF64(f64),
    MinCode { part: u32, code: u32 },
    MaxCode { part: u32, code: u32 },
    SumNum(i128),
    AvgNum { sum: i128, n: i64 },
    SumNumWide(NumWide),
    AvgNumWide { sum: NumWide, n: i64 },
    MinVar(u64),
    MaxVar(u64),
    SumF32(f64),
    F64Accum { n: f64, sx: f64, sxx: f64 },
    BitAnd(i64),
    BitOr(i64),
    BitXor(i64),
    /// A held hosted state's transition value word (a datum or an arena pointer; only its host reads it).
    AggState(u64),
    IntervalMin { time: i64, day: i32, month: i32 },
    IntervalMax { time: i64, day: i32, month: i32 },
    /// NEW-numeric-1 N13: a side companion's word (`side_image` and `NUM_SIDE_T`).
    NumSide(u64),
    IntervalSum(IntervalSum),
}

/// The headerless record (`breakers.md` "The record", partitioned-record.md rulings 14–16): `nkeys`
/// key words, the slots at 8-aligned offsets, then the validity word when the layout carries one.
/// Empty = the sentinel (the first `Count` slot, appended when the slots have none) at 0. `Copy`, hashed by content.
///
/// Two tiers, one face (decompose.md decision 82): the 8-key tier is the inline arrays (`keys`, `slots`,
/// `slot_off`, the `u8` offsets; `wide` none) — the kernels' constants, indexed by constant as before;
/// the wide tier (`wide` some, `LAYOUT_WIDE` in `flags`) is unbounded: the arrays live in `WideLayout`
/// (`u32` offsets) and the inline arrays / `u8` fields are zero. Every reader goes through the accessors
/// (`size()`, `slot_off(j)`, `slot_kind(j)`, `key_kind(k)`, `sentinel_off()`, `valid_off()`, `key_kinds()`,
/// `slot_kinds()`, `mincode(j)`); the record word layout is the same rule in both tiers.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RecordLayout {
    /// The inline tier's counts (the wide tier saturates them at 255: read `nkeys()` / `nslots()`).
    pub(crate) nkeys: u8,
    pub(crate) nslots: u8,
    pub keys: [KeyKind; MAX_RECORD_KEYS],
    pub slots: [SlotKind; MAX_RECORD_SLOTS],
    pub slot_off: [u8; MAX_RECORD_SLOTS],
    pub size: u8,
    pub flags: u8,
    pub sentinel_off: u8,
    pub valid_off: u8,
    /// Bit `j`: slot `j` is a `MinCode` (`open` sets them to `MINCODE_NONE` on a layout without the validity word).
    pub mincode_bits: u8,
    /// The key words the partition hashes over (0: every word): records sharing them meet in one owner.
    pub pkeys: u8,
    /// The wide tier's arrays and offsets (none: the 8-key tier above is the whole layout).
    /// One pointer word (`layout::Word`): `words()` carries it as the fifth u64 on every target.
    pub wide: crate::layout::Word<Option<&'static WideLayout>>,
}

const _: () = assert!(size_of::<RecordLayout>() == 40);

const fn sat_u8(n: usize) -> u8 {
    if n > u8::MAX as usize { u8::MAX } else { n as u8 }
}

/// The wide tier of a `RecordLayout` (decision 82): the arrays past the inline eight, `u32` offsets, interned for
/// the backend's life (`gravity_program::record_layout`) or a static. `slots` carries the implicit `Count` sentinel when one was appended.
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct WideLayout {
    pub keys: &'static [KeyKind],
    pub slots: &'static [SlotKind],
    pub slot_off: &'static [u32],
    pub size: u32,
    pub sentinel_off: u32,
    pub valid_off: u32,
}

impl WideLayout {
    /// The wide tier's rule (`RecordLayout::build`'s): `slots` carries the appended sentinel; `slot_off` holds `slots.len()` offsets.
    pub fn compute(keys: &'static [KeyKind], slots: &'static [SlotKind], slot_off: &'static mut [u32], validity: bool) -> WideLayout {
        let sentinel = first_count(slots).expect("record shape: the wide tier's slots carry the count sentinel");
        WideLayout::compute_at(keys, slots, slot_off, validity, sentinel)
    }

    pub fn compute_at(keys: &'static [KeyKind], slots: &'static [SlotKind], slot_off: &'static mut [u32], validity: bool, sentinel: usize) -> WideLayout {
        assert!(keys.len() <= u16::MAX as usize && slots.len() <= u16::MAX as usize, "record shape: the u16 word / slot ids are the sanity limit");
        assert_eq!(slot_off.len(), slots.len(), "record shape");
        assert!(matches!(slots[sentinel], SlotKind::Count), "record shape: the sentinel is a count slot");
        let mut off = 8 * keys.len() as u32;
        for (j, s) in slots.iter().enumerate() {
            slot_off[j] = off;
            off += s.bytes() as u32;
        }
        let sentinel = slot_off[sentinel];
        let valid_off = off;
        if validity {
            off += validity_bytes(slots.len());
        }
        WideLayout { keys, slots, slot_off: &*slot_off, size: off, sentinel_off: sentinel, valid_off }
    }
}

/// The validity region's bytes for `nslots` slots: the valid bits (bit `j` at byte `j / 8`), the `null_seen`
/// byte after them, rounded to a word — 8 bytes through 8 slots (the 8-key tier's word), 16 through 56, …
pub const fn validity_bytes(nslots: usize) -> u32 {
    (((nslots + 7) / 8 + 1 + 7) / 8 * 8) as u32
}

/// The first `Count` slot: `new` / `new_nn`'s sentinel rule.
pub const fn first_count(slots: &[SlotKind]) -> Option<usize> {
    let mut j = 0;
    while j < slots.len() {
        if matches!(slots[j], SlotKind::Count) {
            return Some(j);
        }
        j += 1;
    }
    None
}

/// Whether `keys` / `slots` (the caller's, before the sentinel) build in the 8-key tier.
pub const fn fits_inline(keys: &[KeyKind], slots: &[SlotKind], validity: bool) -> bool {
    fits_inline_at(keys, slots, validity, first_count(slots))
}

pub const fn fits_inline_at(keys: &[KeyKind], slots: &[SlotKind], validity: bool, sentinel: Option<usize>) -> bool {
    let has_count = sentinel.is_some();
    let mut off = 8 * keys.len();
    let mut j = 0;
    while j < slots.len() {
        off += slots[j].bytes() as usize;
        j += 1;
    }
    let nslots = slots.len() + if has_count { 0 } else { 1 };
    off += if has_count { 0 } else { 8 } + if validity { 8 } else { 0 };
    keys.len() <= MAX_RECORD_KEYS && nslots <= MAX_RECORD_SLOTS && off <= u8::MAX as usize
}

pub const RECORD_HDR: usize = 0;
/// `RecordLayout.flags`: a validity word follows the slots (`valid` bit per slot, `null_seen` at bit 8).
pub const LAYOUT_VALIDITY: u8 = 1;
/// `RecordLayout.flags`: the last slot is the implicit `Count` sentinel (the caller's slots had none).
pub const LAYOUT_IMPLICIT_COUNT: u8 = 2;
/// `RecordLayout.flags`: the wide tier (`wide` is some; the inline arrays and `u8` fields are zero).
pub const LAYOUT_WIDE: u8 = 4;
/// `RecordLayout.flags` (NEW-numeric-1, N11 (ii')): every `MinI64` / `MaxI64` slot holds a numeric order key (gravity_fn
/// typesem numeric `order_key`: `value << 4 | dscale`). PostgreSQL keeps the later row of equal values. A merge whose
/// source holds the later rows (`Rec::fold_newer`: compaction in chain order, the owner fold oldest first, a front or
/// a probe miss into its table) takes the source's word on a tie of `>> 4`. Any other merge (`Rec::fold`) cannot order
/// the two and raises on a tie at two words (counted in `KEY_TIES`); every merge of a breaker ends before its first
/// emitted row, so the statement's recheck licence re-runs it in plan order. The layout has the validity word and no
/// other `MinI64` / `MaxI64` or float slot a merge reads. N13: each key slot's next slot is its side companion
/// (`NumSideMin` / `NumSideMax`), the extreme over the values the rows could not key; `fold_slots` folds the pair together.
pub const LAYOUT_KEY_TIES: u8 = 8;
/// NEW-numeric-1: the merges that met a key tie (`LAYOUT_KEY_TIES`) and raised; the recheck's report names FOLD_TIE while
/// it moved (exec runner/recheck.rs `note_recheck` takes it).
pub static KEY_TIES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Bit 63 of the sentinel word: the record is moving during a resettle (`grow`) or listed as a candidate.
pub const MOVING: u64 = 1 << 63;
/// A `MinCode` slot of a layout without a validity word opens at this word (never a code word).
pub const MINCODE_NONE: u64 = u64::MAX;

/// NEW-numeric-1 N13: two numeric payloads (PostgreSQL's NumericData after the varlena header: the short or long header,
/// then base-10000 digits) by value, as numeric.c `cmp_numerics`: NaN equals NaN and sorts above +Infinity, which sorts
/// above every finite value, -Infinity below; a finite pair by `cmp_var_common`. Equal values of two dscales compare
/// equal. A payload numeric.c never writes reads as zero.
pub fn numeric_payload_cmp(a: &[u8], b: &[u8]) -> Ordering {
    let (x, y) = (NumParts::of(a), NumParts::of(b));
    if x.class != 1 || y.class != 1 {
        return x.class.cmp(&y.class);
    }
    // cmp_var_common
    match (x.n, y.n) {
        (0, 0) => return Ordering::Equal,
        (0, _) => return if y.neg { Ordering::Greater } else { Ordering::Less },
        (_, 0) => return if x.neg { Ordering::Less } else { Ordering::Greater },
        _ => {}
    }
    match (x.neg, y.neg) {
        (false, true) => Ordering::Greater,
        (true, false) => Ordering::Less,
        (false, false) => cmp_abs(&x, &y),
        (true, true) => cmp_abs(&y, &x),
    }
}

/// A numeric payload's parts: `class` 0 -Infinity, 1 finite, 2 +Infinity, 3 NaN; the sign, the base-10000 weight and the
/// `n` digits from byte `2 * first`.
struct NumParts<'a> {
    p: &'a [u8],
    class: u8,
    neg: bool,
    weight: i32,
    first: usize,
    n: usize,
}

impl<'a> NumParts<'a> {
    fn of(p: &'a [u8]) -> NumParts<'a> {
        let rd = |i: usize| p.get(2 * i..2 * i + 2).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]));
        let h = rd(0);
        let (class, neg, weight, first) = match h & 0xC000 {
            0xC000 => (match h & 0xF000 { 0xD000 => 2, 0xF000 => 0, _ => 3 }, false, 0, p.len() / 2),
            0x8000 => (1, h & 0x2000 != 0, if h & 0x40 != 0 { (h | !0x3F) as i16 as i32 } else { (h & 0x3F) as i32 }, 1),
            sign => (1, sign == 0x4000, rd(1) as i16 as i32, 2),
        };
        NumParts { p, class, neg, weight, first, n: (p.len() / 2).saturating_sub(first) }
    }

    fn digit(&self, i: usize) -> u16 {
        let k = 2 * (self.first + i);
        u16::from_le_bytes([self.p[k], self.p[k + 1]])
    }
}

/// numeric.c `cmp_abs_common`: |x| against |y|.
fn cmp_abs(x: &NumParts<'_>, y: &NumParts<'_>) -> Ordering {
    let (mut i1, mut i2, mut w1, mut w2) = (0, 0, x.weight, y.weight);
    while w1 > w2 && i1 < x.n {
        if x.digit(i1) != 0 {
            return Ordering::Greater;
        }
        (i1, w1) = (i1 + 1, w1 - 1);
    }
    while w2 > w1 && i2 < y.n {
        if y.digit(i2) != 0 {
            return Ordering::Less;
        }
        (i2, w2) = (i2 + 1, w2 - 1);
    }
    if w1 == w2 {
        while i1 < x.n && i2 < y.n {
            match x.digit(i1).cmp(&y.digit(i2)) {
                Ordering::Equal => (i1, i2) = (i1 + 1, i2 + 1),
                o => return o,
            }
        }
    }
    if (i1..x.n).any(|i| x.digit(i) != 0) {
        return Ordering::Greater;
    }
    if (i2..y.n).any(|i| y.digit(i) != 0) {
        return Ordering::Less;
    }
    Ordering::Equal
}

/// `numeric_payload_cmp` over two 4-byte-header varlenas (the side copies' form).
/// # Safety
/// Both point at such varlenas.
pub unsafe fn numeric_image_cmp(a: *const u8, b: *const u8) -> Ordering {
    let len = |p: *const u8| (u32::from_le(core::ptr::read_unaligned(p as *const u32)) >> 2) as usize - 4;
    numeric_payload_cmp(core::slice::from_raw_parts(a.add(4), len(a)), core::slice::from_raw_parts(b.add(4), len(b)))
}

/// Two 4-byte-header varlenas (`agg_image_put`'s form: the header `(len + 4) << 2` little-endian) by their payload bytes, then their lengths.
/// # Safety
/// Both point at such varlenas.
pub unsafe fn varlena_cmp(a: *const u8, b: *const u8) -> Ordering {
    let len = |p: *const u8| (u32::from_le(core::ptr::read_unaligned(p as *const u32)) >> 2) as usize - 4;
    let (la, lb) = (len(a), len(b));
    let (x, y) = (core::slice::from_raw_parts(a.add(4), la), core::slice::from_raw_parts(b.add(4), lb));
    x.cmp(y)
}

impl RecordLayout {
    /// The layout as the `LAYOUT_WORDS` plain words a descriptor or a parameter block carries (`from_words` reads them back).
    pub fn words(&self) -> [u64; LAYOUT_WORDS] {
        // SAFETY: `repr(C)`, 40 bytes without padding (u8 fields and u8 enums, then the wide tier's reference as a word).
        unsafe { core::mem::transmute(*self) }
    }

    /// `==`, its words compared first (D3075: a table's shape check at every open of a rescanned fold compared the fields
    /// one by one); words that differ (a wide tier's two copies of one layout) fall back to the field compare.
    #[inline]
    pub fn same(&self, other: &RecordLayout) -> bool {
        self.words() == other.words() || self == other
    }

    /// The layout `words` packed.
    ///
    /// # Safety
    /// `w` is a `words()` result (a valid key / slot kind in every byte; the wide tier's word an interned `'static` address).
    pub unsafe fn from_words(w: [u64; LAYOUT_WORDS]) -> RecordLayout {
        core::mem::transmute(w)
    }

    /// With the validity word: SQL's null semantics per slot.
    pub const fn new(keys: &[KeyKind], slots: &[SlotKind]) -> RecordLayout {
        RecordLayout::build(keys, slots, true, first_count(slots))
    }

    /// The sentinel slot named: a `Count` every opened record counts a row in (`count(*)`, a count over a NOT NULL argument);
    /// `None` appends the implicit one even when the slots hold a `Count` (a nullable count stays 0 and would read as empty).
    pub const fn with_sentinel(keys: &[KeyKind], slots: &[SlotKind], validity: bool, sentinel: Option<usize>) -> RecordLayout {
        RecordLayout::build(keys, slots, validity, sentinel)
    }

    /// One key word and no slot, no sentinel: a key set's layout (the dominance export's `BARE`).
    pub const fn bare(key: KeyKind, size: u8) -> RecordLayout {
        RecordLayout { nkeys: 1, nslots: 0, keys: [key; MAX_RECORD_KEYS], slots: [SlotKind::Count; MAX_RECORD_SLOTS], slot_off: [0; MAX_RECORD_SLOTS], size, flags: 0, sentinel_off: 0, valid_off: 0, mincode_bits: 0, pkeys: 0, wide: crate::layout::word(None) }
    }

    /// This layout over other keys: the slots, the validity and the sentinel slot kept (an implicit one is among `slot_kinds()`).
    pub fn rekeyed(&self, keys: &[KeyKind]) -> RecordLayout {
        RecordLayout::build(keys, self.slot_kinds(), self.has_validity(), Some(self.sentinel_slot()))
    }

    /// The sentinel's slot index.
    pub const fn sentinel_slot(&self) -> usize {
        let mut j = 0;
        while j < self.nslots() {
            if self.slot_off(j) == self.sentinel_off() {
                return j;
            }
            j += 1;
        }
        panic!("record shape: the sentinel is a slot")
    }

    /// NOT NULL slot lanes: no validity word (`AvgI128` valid by its `n`, `MinCode` by its word); min / max need the word.
    pub const fn new_nn(keys: &[KeyKind], slots: &[SlotKind]) -> RecordLayout {
        let mut j = 0;
        while j < slots.len() {
            assert!(!matches!(slots[j], SlotKind::MinI64 | SlotKind::MaxI64 | SlotKind::MaxCode), "a min/max slot needs the validity word");
            j += 1;
        }
        RecordLayout::build(keys, slots, false, first_count(slots))
    }

    const fn build(keys: &[KeyKind], slots: &[SlotKind], validity: bool, sentinel_slot: Option<usize>) -> RecordLayout {
        assert!(keys.len() <= MAX_RECORD_KEYS && slots.len() <= MAX_RECORD_SLOTS, "record shape");
        if let Some(j) = sentinel_slot {
            assert!(j < slots.len() && matches!(slots[j], SlotKind::Count), "record shape: the sentinel is a count slot");
        }
        let mut l = RecordLayout { nkeys: keys.len() as u8, nslots: slots.len() as u8, keys: [KeyKind::I32; MAX_RECORD_KEYS], slots: [SlotKind::Count; MAX_RECORD_SLOTS], slot_off: [0; MAX_RECORD_SLOTS], size: 0, flags: 0, sentinel_off: 0, valid_off: 0, mincode_bits: 0, pkeys: 0, wide: crate::layout::word(None) };
        let mut i = 0;
        while i < keys.len() {
            l.keys[i] = keys[i];
            i += 1;
        }
        let mut off = 8 * keys.len();
        let mut j = 0;
        let mut sentinel = usize::MAX;
        while j < slots.len() {
            l.slots[j] = slots[j];
            l.slot_off[j] = off as u8;
            if let Some(sj) = sentinel_slot {
                if sj == j {
                    sentinel = off;
                }
            }
            if matches!(slots[j], SlotKind::MinCode) {
                l.mincode_bits |= 1 << j;
            }
            off += slots[j].bytes() as usize;
            j += 1;
        }
        if sentinel == usize::MAX {
            assert!(slots.len() < MAX_RECORD_SLOTS, "record shape: no room for the count sentinel");
            l.slots[j] = SlotKind::Count;
            l.slot_off[j] = off as u8;
            l.nslots += 1;
            l.flags |= LAYOUT_IMPLICIT_COUNT;
            sentinel = off;
            off += 8;
        }
        l.sentinel_off = sentinel as u8;
        if validity {
            l.flags |= LAYOUT_VALIDITY;
            l.valid_off = off as u8;
            off += 8;
        }
        assert!(off <= u8::MAX as usize, "record size");
        l.size = off as u8;
        l
    }

    /// The wide tier over a computed `WideLayout` (the program arena's, or a static). `implicit`: the sentinel
    /// was appended (`w.slots`' last is the implicit `Count`).
    pub const fn wide(w: &'static WideLayout, validity: bool, implicit: bool) -> RecordLayout {
        let flags = LAYOUT_WIDE | if validity { LAYOUT_VALIDITY } else { 0 } | if implicit { LAYOUT_IMPLICIT_COUNT } else { 0 };
        RecordLayout { nkeys: sat_u8(w.keys.len()), nslots: sat_u8(w.slots.len()), keys: [KeyKind::I32; MAX_RECORD_KEYS], slots: [SlotKind::Count; MAX_RECORD_SLOTS], slot_off: [0; MAX_RECORD_SLOTS], size: 0, flags, sentinel_off: 0, valid_off: 0, mincode_bits: 0, pkeys: 0, wide: crate::layout::word(Some(w)) }
    }

    pub const fn is_wide(&self) -> bool {
        crate::layout::unword(self.wide).is_some()
    }

    pub const fn partitioned_by(mut self, n: u8) -> RecordLayout {
        assert!(n as usize <= self.nkeys(), "partition keys");
        self.pkeys = n;
        self
    }

    pub const fn partition_keys(&self) -> usize {
        if self.pkeys == 0 { self.nkeys() } else { self.pkeys as usize }
    }

    pub const fn size(&self) -> u32 {
        match crate::layout::unword(self.wide) {
            None => self.size as u32,
            Some(w) => w.size,
        }
    }

    pub const fn sentinel_off(&self) -> u32 {
        match crate::layout::unword(self.wide) {
            None => self.sentinel_off as u32,
            Some(w) => w.sentinel_off,
        }
    }

    /// The validity region's offset (meaningful under `has_validity()`).
    pub const fn valid_off(&self) -> u32 {
        match crate::layout::unword(self.wide) {
            None => self.valid_off as u32,
            Some(w) => w.valid_off,
        }
    }

    pub const fn nkeys(&self) -> usize {
        match crate::layout::unword(self.wide) {
            None => self.nkeys as usize,
            Some(w) => w.keys.len(),
        }
    }

    pub const fn nslots(&self) -> usize {
        match crate::layout::unword(self.wide) {
            None => self.nslots as usize,
            Some(w) => w.slots.len(),
        }
    }

    #[inline(always)]
    pub const fn slot_off(&self, j: usize) -> u32 {
        match crate::layout::unword(self.wide) {
            None => self.slot_off[j] as u32,
            Some(w) => w.slot_off[j],
        }
    }

    #[inline(always)]
    pub const fn slot_kind(&self, j: usize) -> SlotKind {
        match crate::layout::unword(self.wide) {
            None => self.slots[j],
            Some(w) => w.slots[j],
        }
    }

    #[inline(always)]
    pub const fn key_kind(&self, k: usize) -> KeyKind {
        match crate::layout::unword(self.wide) {
            None => self.keys[k],
            Some(w) => w.keys[k],
        }
    }

    /// Slot `j` is a `MinCode` (`open` sets it to `MINCODE_NONE` on a layout without the validity word).
    pub const fn mincode(&self, j: usize) -> bool {
        match crate::layout::unword(self.wide) {
            None => self.mincode_bits >> j & 1 != 0,
            Some(w) => matches!(w.slots[j], SlotKind::MinCode),
        }
    }

    pub fn key_kinds(&self) -> &[KeyKind] {
        match crate::layout::unword(self.wide) {
            None => &self.keys[..self.nkeys as usize],
            Some(w) => w.keys,
        }
    }

    pub fn slot_kinds(&self) -> &[SlotKind] {
        match crate::layout::unword(self.wide) {
            None => &self.slots[..self.nslots as usize],
            Some(w) => w.slots,
        }
    }

    pub const fn key_off(&self, k: usize) -> usize {
        8 * k
    }

    pub const fn has_validity(&self) -> bool {
        self.flags & LAYOUT_VALIDITY != 0
    }

    /// The validity byte holding slot `j`'s bit and the bit's shift.
    #[inline(always)]
    pub const fn valid_bit(&self, j: usize) -> (usize, u8) {
        (self.valid_off() as usize + j / 8, (j % 8) as u8)
    }

    /// The `null_seen` byte: after the valid bits (byte 1 of the 8-key tier's word).
    #[inline(always)]
    pub const fn null_seen_off(&self) -> usize {
        self.valid_off() as usize + (self.nslots() + 7) / 8
    }
}

/// A record's key words against a probe's, word by word (a slice `==` is a `bcmp` call per probe).
#[inline(always)]
pub fn keys_eq(record: *const u64, keys: &[u64]) -> bool {
    // SAFETY: a record holds `keys.len()` key words at `record` (the layout's `nkeys`).
    unsafe {
        match keys.len() {
            1 => *record == keys[0],
            2 => *record == keys[0] && *record.add(1) == keys[1],
            n => {
                let mut i = 0;
                while i < n {
                    if *record.add(i) != keys[i] {
                        return false;
                    }
                    i += 1;
                }
                true
            }
        }
    }
}

/// A read prefetch of the line at `p` (a hint: never faults, never dereferenced); a no-op elsewhere.
#[inline(always)]
pub fn prefetch_read(p: *const u8) {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("prfm pldl1keep, [{0}]", in(reg) p, options(nostack, preserves_flags));
    }
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::x86_64::_mm_prefetch(p as *const i8, core::arch::x86_64::_MM_HINT_T0);
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    let _ = p;
}

/// The KeyHash of the `nkeys` image words at `p` (a record's keys, or its route prefix).
/// # Safety
/// `p` points at `nkeys` readable words.
#[inline(always)]
pub unsafe fn hash_at(p: *const u8, nkeys: usize) -> KeyHash {
    KeyHash::of(core::slice::from_raw_parts(p as *const u64, nkeys))
}

/// One record in place: a pointer and its layout. The typed accessors are the
/// kernels' hot path (the layout is a kernel constant); `slot()` is the view.
#[derive(Clone, Copy)]
pub struct Rec<'a> {
    p: *mut u8,
    pub layout: &'a RecordLayout,
}

impl<'a> Rec<'a> {
    /// # Safety
    /// `p` points at `layout.size()` writable bytes, 8-aligned, zeroed or a live record of that layout.
    pub unsafe fn at(p: *mut u8, layout: &'a RecordLayout) -> Rec<'a> {
        debug_assert!(p.cast::<u64>().is_aligned(), "gravity: a record at {p:p} is not 8-aligned (D1704)");
        Rec { p, layout }
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.p
    }

    pub fn bytes(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.p, self.layout.size() as usize) }
    }

    fn u8_at(&self, off: usize) -> u8 {
        unsafe { *self.p.add(off) }
    }

    fn set_u8(&self, off: usize, v: u8) {
        unsafe { *self.p.add(off) = v }
    }

    fn u64_at(&self, off: usize) -> u64 {
        unsafe { *(self.p.add(off) as *const u64) }
    }

    fn set_u64(&self, off: usize, v: u64) {
        unsafe { *(self.p.add(off) as *mut u64) = v }
    }

    #[inline(always)]
    pub fn used(&self) -> bool {
        self.u64_at(self.layout.sentinel_off() as usize) != 0
    }

    #[inline(always)]
    fn moving(&self) -> bool {
        self.u64_at(self.layout.sentinel_off() as usize) & MOVING != 0
    }

    fn set_moving(&self) {
        let o = self.layout.sentinel_off() as usize;
        self.set_u64(o, self.u64_at(o) | MOVING);
    }

    fn settle(&self) {
        let o = self.layout.sentinel_off() as usize;
        self.set_u64(o, self.u64_at(o) & !MOVING);
    }

    #[inline(always)]
    pub fn hash(&self) -> KeyHash {
        KeyHash::of(self.keys())
    }

    /// The route: the partition keys' hash, which picks the record's partition everywhere (append, drain, owner, merge).
    #[inline(always)]
    pub fn route(&self) -> KeyHash {
        KeyHash::of(&self.keys()[..self.layout.partition_keys()])
    }

    pub fn null_seen(&self) -> bool {
        self.layout.has_validity() && self.u8_at(self.layout.null_seen_off()) != 0
    }

    pub fn set_null_seen(&self) {
        if self.layout.has_validity() {
            self.set_u8(self.layout.null_seen_off(), 1)
        }
    }

    /// The first eight slots' valid bits (the 8-key tier's byte; a wide record's later slots answer `valid(j)`).
    pub fn valid_bits(&self) -> u8 {
        if self.layout.has_validity() {
            self.u8_at(self.layout.valid_off() as usize)
        } else {
            (0..self.layout.nslots().min(8)).fold(0u8, |b, j| b | (self.valid(j) as u8) << j)
        }
    }

    #[inline(always)]
    pub fn valid(&self, j: usize) -> bool {
        if self.layout.has_validity() {
            let (o, sh) = self.layout.valid_bit(j);
            return self.u8_at(o) >> sh & 1 != 0;
        }
        match self.layout.slot_kind(j) {
            SlotKind::MinCode => self.u64_at(self.off(j)) != MINCODE_NONE,
            SlotKind::MinVar | SlotKind::MaxVar | SlotKind::NumSideMin | SlotKind::NumSideMax => self.u64_at(self.off(j)) != 0,
            SlotKind::AvgI128 | SlotKind::AvgNum => self.u64_at(self.off(j) + 16) != 0,
            SlotKind::AvgNumWide => self.u64_at(self.off(j) + 32) != 0,
            SlotKind::F64Accum => self.u64_at(self.off(j)) != 0,
            SlotKind::AggState => self.u8_at(self.off(j) + AGGSTATE_HELD) != 0,
            _ => true,
        }
    }

    #[inline(always)]
    fn mark(&self, j: usize) {
        if self.layout.has_validity() {
            let (o, sh) = self.layout.valid_bit(j);
            self.set_u8(o, self.u8_at(o) | 1 << sh)
        }
    }

    #[inline(always)]
    pub fn key_ptr(&self) -> *const u64 {
        self.p as *const u64
    }

    pub fn keys(&self) -> &[u64] {
        unsafe { core::slice::from_raw_parts(self.p as *const u64, self.layout.nkeys()) }
    }

    pub fn key(&self, k: usize) -> u64 {
        self.u64_at(self.layout.key_off(k))
    }

    /// Overwrite key word `k` (the leader's value fold leaves a rendered datum there; the hash goes stale).
    pub fn set_key(&self, k: usize, w: u64) {
        unsafe { *(self.as_ptr().add(self.layout.key_off(k)) as *mut u64) = w };
    }

    pub fn key_cell(&self, k: usize) -> KeyCell {
        KeyCell::from_word(self.layout.key_kind(k), self.key(k))
    }

    /// Occupy an empty (zeroed) record with `keys`; the row's fold follows before any other table operation.
    #[inline(always)]
    pub fn open(&self, keys: &[u64]) {
        debug_assert_eq!(keys.len(), self.layout.nkeys());
        for (k, &w) in keys.iter().enumerate() {
            self.set_u64(self.layout.key_off(k), w);
        }
        if self.layout.flags & LAYOUT_IMPLICIT_COUNT != 0 {
            self.set_u64(self.layout.sentinel_off() as usize, 1);
        }
        if !self.layout.has_validity() {
            match crate::layout::unword(self.layout.wide) {
                None => {
                    let mut m = self.layout.mincode_bits;
                    while m != 0 {
                        self.set_u64(self.off(m.trailing_zeros() as usize), MINCODE_NONE);
                        m &= m - 1;
                    }
                }
                Some(w) => {
                    for (j, s) in w.slots.iter().enumerate() {
                        if matches!(s, SlotKind::MinCode) {
                            self.set_u64(self.off(j), MINCODE_NONE);
                        }
                    }
                }
            }
        }
    }

    pub fn clear(&self) {
        unsafe { core::ptr::write_bytes(self.p, 0, self.layout.size() as usize) }
    }

    pub fn copy_from(&self, other: &Rec<'_>) {
        debug_assert_eq!(self.layout.size(), other.layout.size());
        unsafe { core::ptr::copy_nonoverlapping(other.p, self.p, self.layout.size() as usize) }
    }

    #[inline(always)]
    fn off(&self, j: usize) -> usize {
        debug_assert!(j < self.layout.nslots());
        self.layout.slot_off(j) as usize
    }

    #[inline(always)]
    pub fn count(&self, j: usize) -> i64 {
        (self.u64_at(self.off(j)) & !MOVING) as i64
    }

    #[inline(always)]
    pub fn add_count(&self, j: usize, n: i64) {
        let o = self.off(j);
        self.set_u64(o, (self.u64_at(o) as i64).wrapping_add(n) as u64);
        self.mark(j);
    }

    #[inline(always)]
    pub fn i64(&self, j: usize) -> i64 {
        self.u64_at(self.off(j)) as i64
    }

    #[inline(always)]
    pub fn add_i64(&self, j: usize, v: i64) {
        let o = self.off(j);
        self.set_u64(o, (self.u64_at(o) as i64).wrapping_add(v) as u64);
        self.mark(j);
    }

    #[inline(always)]
    pub fn i128(&self, j: usize) -> i128 {
        let o = self.off(j);
        (((self.u64_at(o + 8) as u128) << 64) | self.u64_at(o) as u128) as i128
    }

    #[inline(always)]
    fn set_i128(&self, o: usize, v: i128) {
        self.set_u64(o, v as u128 as u64);
        self.set_u64(o + 8, (v as u128 >> 64) as u64);
    }

    #[inline(always)]
    pub fn add_i128(&self, j: usize, v: i128) {
        let o = self.off(j);
        let cur = self.i128(j);
        self.set_i128(o, cur.wrapping_add(v));
        self.mark(j);
    }

    /// `add_i128` over numeric mantissa sums that may hold a sticky special (`hi == NUM_SENT_HI`, `lo` its code): the codes add as
    /// PostgreSQL's specials (`gravity_fn::agg_final::SENT_HI`; lowerv2's `add_sum` is the row fold's form).
    #[inline(always)]
    pub fn add_num(&self, j: usize, v: i128) {
        let o = self.off(j);
        self.set_i128(o, num_add(self.i128(j), v));
        self.mark(j);
    }

    /// A `SumNumWide` / `AvgNumWide` slot's i256 words.
    #[inline(always)]
    pub fn num256(&self, j: usize) -> NumWide {
        let o = self.off(j);
        [self.u64_at(o), self.u64_at(o + 8), self.u64_at(o + 16), self.u64_at(o + 24)]
    }

    /// `(sum, n)` of an `AvgNumWide` slot.
    #[inline(always)]
    pub fn avg256(&self, j: usize) -> (NumWide, i64) {
        (self.num256(j), self.u64_at(self.off(j) + 32) as i64)
    }

    /// `add_num` over a `SumNumWide` slot (`num_add256`).
    #[inline(always)]
    pub fn add_num256(&self, j: usize, v: NumWide) {
        let o = self.off(j);
        let s = num_add256(self.num256(j), v);
        (0..4).for_each(|i| self.set_u64(o + 8 * i, s[i]));
        self.mark(j);
    }

    /// `add_avg_num` over an `AvgNumWide` slot (its count at +32).
    #[inline(always)]
    pub fn add_avg_num256(&self, j: usize, v: NumWide, n: i64) {
        let o = self.off(j);
        let s = num_add256(self.num256(j), v);
        (0..4).for_each(|i| self.set_u64(o + 8 * i, s[i]));
        self.set_u64(o + 32, (self.u64_at(o + 32) as i64).wrapping_add(n) as u64);
        self.mark(j);
    }

    #[inline(always)]
    pub fn add_avg_num(&self, j: usize, v: i128, n: i64) {
        let o = self.off(j);
        self.set_i128(o, num_add(self.i128(j), v));
        self.set_u64(o + 16, (self.u64_at(o + 16) as i64).wrapping_add(n) as u64);
        self.mark(j);
    }

    /// `bit_and / bit_or / bit_xor`'s merge (D252): `v` itself into a slot not yet valid (a validity layout; PG's state
    /// starts NULL), else the slot's word combined with `v`.
    #[inline(always)]
    pub fn fold_bits(&self, j: usize, v: i64) {
        let o = self.off(j);
        let cur = self.u64_at(o) as i64;
        let w = if self.layout.has_validity() && !self.valid(j) {
            v
        } else {
            match self.layout.slot_kind(j) {
                SlotKind::BitAnd => cur & v,
                SlotKind::BitOr => cur | v,
                _ => cur ^ v,
            }
        };
        self.set_u64(o, w as u64);
        self.mark(j);
    }

    /// An `IntervalMin` / `IntervalMax` slot's (time, day, month).
    #[inline(always)]
    pub fn interval(&self, j: usize) -> (i64, i32, i32) {
        let (o, w) = (self.off(j), self.u64_at(self.off(j) + 8));
        (self.u64_at(o) as i64, w as u32 as i32, (w >> 32) as u32 as i32)
    }

    /// `interval_smaller / larger(this, v)` into an `IntervalMin` / `IntervalMax` slot: an empty slot takes `v`; else `v` wins
    /// unless this slot's value is strictly smaller (min) / larger (max), so a tie keeps `v`, PG's second argument (the later
    /// row or the later part).
    #[inline(always)]
    pub fn fold_interval(&self, j: usize, v: (i64, i32, i32), min: bool) {
        let o = self.off(j);
        if self.valid(j) {
            let (t, d, m) = self.interval(j);
            let (cur, new) = (interval_cmp_value(t, d, m), interval_cmp_value(v.0, v.1, v.2));
            if min && cur < new || !min && cur > new {
                return;
            }
        }
        self.set_u64(o, v.0 as u64);
        self.set_u64(o + 8, v.1 as u32 as u64 | (v.2 as u32 as u64) << 32);
        self.mark(j);
    }

    /// An `IntervalSum` slot's state (its eight words; an empty slot's are zero).
    #[inline(always)]
    pub fn interval_sum(&self, j: usize) -> IntervalSum {
        let o = self.off(j);
        let w = |k: usize| self.u64_at(o + 8 * k);
        let (dm, inf) = (w(1), w(6));
        IntervalSum {
            time: w(0) as i64,
            day: dm as u32 as i32,
            month: (dm >> 32) as u32 as i32,
            abs_time: w(2),
            abs_day: w(3),
            abs_month: w(4),
            n: w(5) as i64,
            pinf: inf as u32,
            ninf: (inf >> 32) as u32,
            flags: w(7),
        }
    }

    /// `s` into `IntervalSum` slot `j` (marked valid).
    #[inline(always)]
    pub fn set_interval_sum(&self, j: usize, s: &IntervalSum) {
        let o = self.off(j);
        let words = [
            s.time as u64,
            s.day as u32 as u64 | (s.month as u32 as u64) << 32,
            s.abs_time,
            s.abs_day,
            s.abs_month,
            s.n as u64,
            s.pinf as u64 | (s.ninf as u64) << 32,
            s.flags,
        ];
        for (k, w) in words.into_iter().enumerate() {
            self.set_u64(o + 8 * k, w);
        }
        self.mark(j);
    }

    /// `IntervalSum::compose(this, v)` into `IntervalSum` slot `j` (an empty slot composes from zero). `Err(FloatOverflow)`:
    /// a licensed state that may overflow, the statement's raise (its re-run in plan order answers; no text is shown), the
    /// record then garbage, as a float merge's.
    #[inline(always)]
    pub fn fold_interval_sum(&self, j: usize, v: &IntervalSum) -> Result<(), FloatOverflow> {
        let mut s = if self.valid(j) { self.interval_sum(j) } else { IntervalSum::default() };
        let raise = s.compose(v);
        self.set_interval_sum(j, &s);
        if raise {
            return Err(FloatOverflow);
        }
        Ok(())
    }

    #[inline(always)]
    pub fn min_i64(&self, j: usize, v: i64) {
        let o = self.off(j);
        if !self.valid(j) || v < self.u64_at(o) as i64 {
            self.set_u64(o, v as u64);
        }
        self.mark(j);
    }

    #[inline(always)]
    pub fn max_i64(&self, j: usize, v: i64) {
        let o = self.off(j);
        if !self.valid(j) || v > self.u64_at(o) as i64 {
            self.set_u64(o, v as u64);
        }
        self.mark(j);
    }

    /// `(sum, n)` of an `AvgI128` slot.
    #[inline(always)]
    pub fn avg(&self, j: usize) -> (i128, i64) {
        (self.i128(j), self.u64_at(self.off(j) + 16) as i64)
    }

    #[inline(always)]
    pub fn add_avg(&self, j: usize, v: i128, n: i64) {
        let o = self.off(j);
        let cur = self.i128(j);
        self.set_i128(o, cur.wrapping_add(v));
        self.set_u64(o + 16, (self.u64_at(o + 16) as i64).wrapping_add(n) as u64);
        self.mark(j);
    }

    #[inline(always)]
    pub fn f64(&self, j: usize) -> f64 {
        f64::from_bits(self.u64_at(self.off(j)))
    }

    /// `sum(float8)`'s merge: `float8_pl` onto the slot, or `v` itself into a slot not yet valid (PG's state starts NULL,
    /// so a lone -0 stays -0).
    #[inline(always)]
    #[must_use]
    pub fn add_f64(&self, j: usize, v: f64) -> Result<(), FloatOverflow> {
        let o = self.off(j);
        let s = if self.layout.has_validity() && !self.valid(j) { v } else { float8_pl(self.f64(j), v)? };
        self.set_u64(o, s.to_bits());
        self.mark(j);
        Ok(())
    }

    /// `sum(float4)`'s merge: `float4_pl` in float4 (both sides are f32 values kept as f64).
    #[inline(always)]
    #[must_use]
    pub fn add_f32(&self, j: usize, v: f64) -> Result<(), FloatOverflow> {
        let o = self.off(j);
        let s = if self.layout.has_validity() && !self.valid(j) { v as f32 } else { float4_pl(self.f64(j) as f32, v as f32)? };
        self.set_u64(o, (s as f64).to_bits());
        self.mark(j);
        Ok(())
    }

    /// An `F64Accum` slot's {N, Sx, Sxx}.
    #[inline(always)]
    pub fn f64_accum(&self, j: usize) -> [f64; 3] {
        let o = self.off(j);
        [0, 8, 16].map(|k| f64::from_bits(self.u64_at(o + k)))
    }

    /// `avg(float)`'s merge: `float8_combine` of the slot's state and `v`.
    #[inline(always)]
    #[must_use]
    pub fn combine_f64_accum(&self, j: usize, v: [f64; 3]) -> Result<(), FloatOverflow> {
        let o = self.off(j);
        let r = float8_combine(self.f64_accum(j), v)?;
        for (k, w) in r.iter().enumerate() {
            self.set_u64(o + 8 * k, w.to_bits());
        }
        self.mark(j);
        Ok(())
    }

    #[inline(always)]
    pub fn code(&self, j: usize) -> (u32, u32) {
        let w = self.u64_at(self.off(j));
        ((w >> 32) as u32, w as u32)
    }

    #[inline(always)]
    pub fn min_code(&self, j: usize, part: u32, code: u32) {
        let o = self.off(j);
        let w = KeyCell::code_word(part, code);
        if !self.layout.has_validity() {
            // opened at `MINCODE_NONE`: the plain compare is the whole rule
            if w < self.u64_at(o) {
                self.set_u64(o, w);
            }
            return;
        }
        if !self.valid(j) || w < self.u64_at(o) {
            self.set_u64(o, w);
        }
        self.mark(j);
    }

    #[inline(always)]
    pub fn max_code(&self, j: usize, part: u32, code: u32) {
        let o = self.off(j);
        let w = KeyCell::code_word(part, code);
        if !self.valid(j) || w > self.u64_at(o) {
            self.set_u64(o, w);
        }
        self.mark(j);
    }

    /// A `MinVar` / `MaxVar` slot's image address (0: none).
    #[inline(always)]
    pub fn var(&self, j: usize) -> u64 {
        self.u64_at(self.off(j))
    }

    /// `image` (a 4-byte-header varlena's address) into slot `j` when the slot holds none or `image` orders before (`min`) / after
    /// (`max`) the slot's by bytes then length — a tie keeps the slot's, as `text_smaller` / `text_larger` keep the first argument.
    #[inline(always)]
    pub fn fold_var(&self, j: usize, image: u64, min: bool) {
        if image == 0 {
            return;
        }
        let o = self.off(j);
        let cur = self.u64_at(o);
        if cur == 0 || !self.valid(j) {
            self.set_u64(o, image);
            self.mark(j);
            return;
        }
        // SAFETY: both words are addresses of 4-byte-header varlenas written by `agg_image_put`, alive to the statement's end.
        let c = unsafe { varlena_cmp(image as *const u8, cur as *const u8) };
        if c == Ordering::Less && min || c == Ordering::Greater && !min {
            self.set_u64(o, image);
        }
        self.mark(j);
    }

    /// The slot's value, `None` while invalid (no non-null input yet).
    pub fn slot(&self, j: usize) -> Option<AggSlot> {
        if !self.valid(j) {
            return None;
        }
        Some(match self.layout.slot_kind(j) {
            SlotKind::Count => AggSlot::Count(self.count(j)),
            SlotKind::SumI64 => AggSlot::SumI64(self.i64(j)),
            SlotKind::SumI128 => AggSlot::SumI128(self.i128(j)),
            SlotKind::MinI64 => AggSlot::MinI64(self.i64(j)),
            SlotKind::MaxI64 => AggSlot::MaxI64(self.i64(j)),
            SlotKind::AvgI128 => {
                let (sum, n) = self.avg(j);
                AggSlot::AvgI128 { sum, n }
            }
            SlotKind::SumF64 => AggSlot::SumF64(self.f64(j)),
            SlotKind::MinCode => {
                let (part, code) = self.code(j);
                AggSlot::MinCode { part, code }
            }
            SlotKind::MaxCode => {
                let (part, code) = self.code(j);
                AggSlot::MaxCode { part, code }
            }
            SlotKind::SumNum => AggSlot::SumNum(self.i128(j)),
            SlotKind::AvgNum => {
                let (sum, n) = self.avg(j);
                AggSlot::AvgNum { sum, n }
            }
            SlotKind::SumNumWide => AggSlot::SumNumWide(self.num256(j)),
            SlotKind::AvgNumWide => {
                let (sum, n) = self.avg256(j);
                AggSlot::AvgNumWide { sum, n }
            }
            SlotKind::MinVar => AggSlot::MinVar(self.var(j)),
            SlotKind::MaxVar => AggSlot::MaxVar(self.var(j)),
            SlotKind::SumF32 => AggSlot::SumF32(self.f64(j)),
            SlotKind::F64Accum => {
                let [n, sx, sxx] = self.f64_accum(j);
                AggSlot::F64Accum { n, sx, sxx }
            }
            SlotKind::BitAnd => AggSlot::BitAnd(self.i64(j)),
            SlotKind::BitOr => AggSlot::BitOr(self.i64(j)),
            SlotKind::BitXor => AggSlot::BitXor(self.i64(j)),
            SlotKind::AggState => AggSlot::AggState(self.u64_at(self.off(j))),
            SlotKind::IntervalMin => {
                let (time, day, month) = self.interval(j);
                AggSlot::IntervalMin { time, day, month }
            }
            SlotKind::IntervalMax => {
                let (time, day, month) = self.interval(j);
                AggSlot::IntervalMax { time, day, month }
            }
            SlotKind::NumSideMin | SlotKind::NumSideMax => AggSlot::NumSide(self.u64_at(self.off(j))),
            SlotKind::IntervalSum => AggSlot::IntervalSum(self.interval_sum(j)),
        })
    }

    /// Fold `src` into this record slot by slot under the valid flags (the
    /// one place the slot vocabulary is matched for combining); `null_seen` ORs. A float slot's merge is PG's combine
    /// function with its overflow check (D244): on `FloatOverflow` the statement raises, the record is garbage.
    #[must_use]
    pub fn fold(&self, src: &Rec<'_>) -> Result<(), FloatOverflow> {
        self.fold_slots::<false>(src, false)
    }

    /// `fold` where `src` holds rows later than this record's: an order-key extreme (`LAYOUT_KEY_TIES`) takes `src`'s
    /// word on a tie of value, as numeric_smaller / numeric_larger return their second argument, and never raises.
    #[must_use]
    pub fn fold_newer(&self, src: &Rec<'_>) -> Result<(), FloatOverflow> {
        self.fold_slots::<true>(src, false)
    }

    /// V4R-375: `fold` for a record holding hosted states (a keyed record's owner fold): each `AggState` slot `src` holds
    /// combines into this record's through `combine(src, slot, into, from)` (the arena's keep-from combine: `from`'s bytes
    /// are never written, an empty `into` takes PG's copy), every other slot as `fold`. `Err(FOLD_OVERFLOW)`: a float
    /// merge overflowed; `Err(FOLD_HOST)`: the combine raised (the host's error stashed).
    pub fn fold_with(&self, src: &Rec<'_>, combine: &mut StateCombine<'_>) -> Result<(), i64> {
        self.fold_slots::<false>(src, true).map_err(|_| FOLD_OVERFLOW)?;
        self.combine_states(src, combine)
    }

    /// V4R-375: every `AggState` slot emptied (held 0: a group's first partial copied into the owner, its states then
    /// combined in, so the owner never shares a source's state).
    pub fn clear_states(&self) {
        for j in (0..self.layout.nslots()).filter(|&j| self.layout.slot_kind(j) == SlotKind::AggState) {
            let o = self.off(j);
            self.set_u64(o, 0);
            self.set_u64(o + 8, 0);
        }
    }

    fn combine_states(&self, src: &Rec<'_>, combine: &mut StateCombine<'_>) -> Result<(), i64> {
        for j in (0..self.layout.nslots()).filter(|&j| self.layout.slot_kind(j) == SlotKind::AggState) {
            let so = src.off(j);
            if src.u8_at(so + AGGSTATE_HELD) == 0 {
                continue;
            }
            // SAFETY: slot `j` of both records, `AggState`'s 16 bytes; `src` is only read.
            let (into, from) = unsafe { (self.p.add(self.off(j)), src.p.add(so) as *const u8) };
            if !combine(src, j, into, from) {
                return Err(FOLD_HOST);
            }
            self.mark(j);
        }
        Ok(())
    }

    fn fold_slots<const NEWER: bool>(&self, src: &Rec<'_>, hosted: bool) -> Result<(), FloatOverflow> {
        debug_assert_eq!(*self.layout, *src.layout);
        if src.null_seen() {
            self.set_null_seen();
        }
        // NEW-numeric-1: one flag test per merged record; a keyed layout's extrema arms mark a tie (branch-free) and the
        // merge fails once, after its slots, unless `src` is the newer (it then wins the tie)
        let ties = self.layout.flags & LAYOUT_KEY_TIES != 0;
        let mut tied = false;
        // N13: whether the key slot just before a side companion took `src`'s member (`side_extreme` composes the tie bit)
        let mut took = false;
        let validity = self.layout.has_validity();
        let implicit = self.layout.flags & LAYOUT_IMPLICIT_COUNT != 0;
        for j in 0..self.layout.nslots() {
            let key_took = core::mem::take(&mut took);
            if self.layout.slot_kind(j) == SlotKind::AggState {
                if !hosted {
                    self.move_state(j, src, j)?;
                }
                continue;
            }
            if self.layout.slot_kind(j).is_num_side() {
                // read whatever `src` holds: its key may have taken the member while it has no side image
                tied |= self.side_extreme::<NEWER>(j, src, key_took, self.layout.slot_kind(j) == SlotKind::NumSideMin);
                continue;
            }
            if validity && !src.valid(j) {
                continue;
            }
            // perf: the common kinds merge here, one dispatch per slot (through `fold_slot`'s match behind this one: +2.5 %
            // instructions on a grouped statement)
            match self.layout.slot_kind(j) {
                SlotKind::Count if implicit && self.layout.slot_off(j) == self.layout.sentinel_off() => self.add_count(j, (src.count(j) - 1).max(0)),
                SlotKind::Count => self.add_count(j, src.count(j)),
                SlotKind::SumI64 => self.add_i64(j, src.i64(j)),
                SlotKind::SumI128 => self.add_i128(j, src.i128(j)),
                SlotKind::MinI64 if ties => {
                    let (t, d) = self.key_extreme::<NEWER>(j, src.i64(j), true);
                    (took, tied) = (t, tied | d);
                }
                SlotKind::MaxI64 if ties => {
                    let (t, d) = self.key_extreme::<NEWER>(j, src.i64(j), false);
                    (took, tied) = (t, tied | d);
                }
                SlotKind::MinI64 => self.min_i64(j, src.i64(j)),
                SlotKind::MaxI64 => self.max_i64(j, src.i64(j)),
                SlotKind::AvgI128 => {
                    let (s, n) = src.avg(j);
                    self.add_avg(j, s, n)
                }
                SlotKind::SumF64 => self.add_f64(j, src.f64(j))?,
                SlotKind::MinCode => {
                    let (p, c) = src.code(j);
                    if validity || src.u64_at(src.off(j)) != MINCODE_NONE {
                        self.min_code(j, p, c)
                    }
                }
                SlotKind::MaxCode => {
                    let (p, c) = src.code(j);
                    self.max_code(j, p, c)
                }
                _ => self.fold_slot(j, src)?,
            }
        }
        if tied {
            KEY_TIES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            return Err(FloatOverflow);
        }
        Ok(())
    }

    /// Not for an order-key extreme, a side companion or a hosted state: their merges read the slots around them.
    #[inline(always)]
    pub fn fold_slot(&self, j: usize, src: &Rec<'_>) -> Result<(), FloatOverflow> {
        let validity = self.layout.has_validity();
        let implicit = self.layout.flags & LAYOUT_IMPLICIT_COUNT != 0;
        match self.layout.slot_kind(j) {
            SlotKind::Count if implicit && self.layout.slot_off(j) == self.layout.sentinel_off() => self.add_count(j, (src.count(j) - 1).max(0)),
            SlotKind::Count => self.add_count(j, src.count(j)),
            SlotKind::SumI64 => self.add_i64(j, src.i64(j)),
            SlotKind::SumI128 => self.add_i128(j, src.i128(j)),
            SlotKind::MinI64 => self.min_i64(j, src.i64(j)),
            SlotKind::MaxI64 => self.max_i64(j, src.i64(j)),
            SlotKind::AvgI128 => {
                let (s, n) = src.avg(j);
                self.add_avg(j, s, n)
            }
            SlotKind::SumF64 => self.add_f64(j, src.f64(j))?,
            SlotKind::SumF32 => self.add_f32(j, src.f64(j))?,
            SlotKind::F64Accum => self.combine_f64_accum(j, src.f64_accum(j))?,
            SlotKind::BitAnd | SlotKind::BitOr | SlotKind::BitXor => self.fold_bits(j, src.i64(j)),
            SlotKind::MinCode => {
                let (p, c) = src.code(j);
                if validity || src.u64_at(src.off(j)) != MINCODE_NONE {
                    self.min_code(j, p, c)
                }
            }
            SlotKind::MaxCode => {
                let (p, c) = src.code(j);
                self.max_code(j, p, c)
            }
            SlotKind::SumNum => self.add_num(j, src.i128(j)),
            SlotKind::AvgNum => {
                let (s, n) = src.avg(j);
                self.add_avg_num(j, s, n)
            }
            SlotKind::SumNumWide => self.add_num256(j, src.num256(j)),
            SlotKind::AvgNumWide => {
                let (sum, n) = src.avg256(j);
                self.add_avg_num256(j, sum, n)
            }
            SlotKind::MinVar => self.fold_var(j, src.var(j), true),
            SlotKind::MaxVar => self.fold_var(j, src.var(j), false),
            SlotKind::IntervalMin => self.fold_interval(j, src.interval(j), true),
            SlotKind::IntervalMax => self.fold_interval(j, src.interval(j), false),
            SlotKind::IntervalSum => self.fold_interval_sum(j, &src.interval_sum(j))?,
            SlotKind::AggState | SlotKind::NumSideMin | SlotKind::NumSideMax => unreachable!("fold_slots' own"),
        }
        Ok(())
    }

    /// NEW-numeric-1: an order-key extreme (`LAYOUT_KEY_TIES`) folds `v` as `min_i64` / `max_i64` do: (whether the slot took
    /// `v`, whether the held key and `v` tie on value (`>> 4`) at two dscales (two words), which the merge cannot order).
    /// `NEWER`: `v` is the later member, so it takes the tie and nothing is unordered.
    #[inline(always)]
    fn key_extreme<const NEWER: bool>(&self, j: usize, v: i64, min: bool) -> (bool, bool) {
        let o = self.off(j);
        let (held, w) = (self.valid(j), self.u64_at(o) as i64);
        let take = match (NEWER, min) {
            (true, true) => v >> 4 <= w >> 4,
            (true, false) => v >> 4 >= w >> 4,
            (false, true) => v < w,
            (false, false) => v > w,
        };
        if !held || take {
            self.set_u64(o, v as u64);
        }
        self.mark(j);
        (!held || take, !NEWER & held & (w != v) & (w >> 4 == v >> 4))
    }

    /// NEW-numeric-1 N13: side companion `j` of key slot `j - 1` folds `src`'s (`side_fold`), `key_took` saying whether the
    /// key slot took `src`'s member. True where the merge cannot order the pair (not `NEWER`): two images equal in value at
    /// two byte forms, or a key member and a side member of the merged record from two records.
    fn side_extreme<const NEWER: bool>(&self, j: usize, src: &Rec<'_>, key_took: bool, min: bool) -> bool {
        let o = self.off(j);
        let a = if self.valid(j) { self.u64_at(o) } else { 0 };
        let s = if src.valid(j) { src.u64_at(src.off(j)) } else { 0 };
        let (w, side_took, unordered) = side_fold::<NEWER>(a, s, key_took, min);
        if w == 0 {
            return unordered;
        }
        self.set_u64(o, w);
        self.mark(j);
        unordered || (!NEWER && key_took != side_took && self.valid(j - 1))
    }

    /// `AggState` slot `s` of `src` into this record's slot `d`: a held state moves (its 16 bytes: the source record is a
    /// partial the merge discards) into an empty slot; a second held state has no byte merge (its combinefn is the host's),
    /// so it raises (the lowering never folds two: keyed ends refuse a hosted class, a plain record folds one partial).
    #[must_use]
    pub fn move_state(&self, d: usize, src: &Rec<'_>, s: usize) -> Result<(), FloatOverflow> {
        let (o, so) = (self.off(d), src.off(s));
        if src.u8_at(so + AGGSTATE_HELD) == 0 {
            return Ok(());
        }
        if self.u8_at(o + AGGSTATE_HELD) != 0 {
            // a Bug, never an overflow: the hosted plain merge (exec v4/m6.rs) combines two held states before this fold
            // (D806); FloatOverflow is only Rec::fold's one error status (the label's fix is debt)
            debug_assert!(false, "two held hosted aggregate states in a record fold: a Bug (D806)");
            return Err(FloatOverflow);
        }
        self.set_u64(o, src.u64_at(so));
        self.set_u64(o + 8, src.u64_at(so + 8));
        self.mark(d);
        Ok(())
    }
}

/// A numeric sum's special: `hi` of the sentinel form, `lo` the code (`gravity_fn::agg_final`'s `SENT_HI` / `SP_*`).
pub const NUM_SENT_HI: i64 = i64::MIN;
const NUM_SP_NAN: i64 = 1;
/// D299: `gravity_fn::agg_final::SP_OVERFLOW` / `SUM_HI_LIMIT`: a finite sum keeps |sum| < 2^126, past it the code.
const NUM_SP_OVERFLOW: i64 = 4;
/// D299: a finite mantissa sum keeps its hi word in [-NUM_SUM_HI_LIMIT, NUM_SUM_HI_LIMIT): one term is under 2^123 (37
/// digits), so the add that leaves the range cannot have wrapped. The packed sum's format bound, read by gravity_fn
/// `agg_final::SUM_HI_LIMIT` (its re-export) and the lowered numeric sums.
pub const NUM_SUM_HI_LIMIT: i64 = 1 << 62;

/// A numeric sum slot's order as numeric_cmp's over its words (an i128 slot's two, a `SumNumWide`'s four): (class, high,
/// low) with -Infinity 0, a finite value 1 (its i256 two's complement: high signed, low unsigned), +Infinity 2, NaN 3
/// (NaN above every value); `None` for the OVERFLOW sentinel (D299), whose final raises instead. `SumI128` (sum(int8))
/// is never a sentinel. Read by the kernels' group sort (table/emit.rs) and the two-pass sum top's rule (lowerv4 topn
/// raw.rs).
pub fn num_sum_order(k: SlotKind, w: [u64; 4]) -> Option<(u8, i128, u128)> {
    let lo = (w[1] as u128) << 64 | w[0] as u128;
    let (top, hi) = match k {
        SlotKind::SumNumWide => (w[3] as i64, ((w[3] as u128) << 64 | w[2] as u128) as i128),
        SlotKind::SumNum => (w[1] as i64, (lo as i128) >> 127), // sum(numeric): its sentinel on the high word
        _ => (0, (lo as i128) >> 127),                           // sum(int8): never a sentinel
    };
    if top != NUM_SENT_HI {
        return Some((1, hi, lo));
    }
    match w[0] {
        1 => Some((3, 0, 0)),
        2 => Some((2, 0, 0)),
        3 => Some((0, 0, 0)),
        _ => None,
    }
}

/// `a + b` over two numeric mantissa sums, either a sentinel: NaN wins, opposite infinities make NaN, an infinity carries
/// over a finite; a finite sum past |2^126| is the sticky OVERFLOW code (D299: it wrapped, and could land on the sentinel).
pub fn num_add(a: i128, b: i128) -> i128 {
    let (ahi, bhi) = ((a >> 64) as i64, (b >> 64) as i64);
    let sent = |c: i64| ((NUM_SENT_HI as i128) << 64) | c as u64 as i128;
    if ahi != NUM_SENT_HI && bhi != NUM_SENT_HI {
        return a.checked_add(b).filter(|s| ((s >> 64) as i64).wrapping_add(NUM_SUM_HI_LIMIT) >= 0).unwrap_or(sent(NUM_SP_OVERFLOW));
    }
    let code = |hi: i64, v: i128| if hi == NUM_SENT_HI { v as u64 as i64 } else { 0 };
    let (x, y) = (code(ahi, a), code(bhi, b));
    let inf = |c: i64| c == 2 || c == 3;
    let c = match () {
        _ if x == NUM_SP_NAN || y == NUM_SP_NAN || (inf(x) && inf(y) && x != y) => NUM_SP_NAN,
        _ if inf(x) || y == 0 => x,
        _ if inf(y) || x == 0 => y,
        _ => x,
    };
    sent(c)
}

/// V4R-38: an i256 numeric mantissa sum, four little-endian words of two's complement (`SumNumWide`); a special is the
/// sentinel on the top word (`NUM_SENT_HI`), its code in word 0, the middle words zero.
pub type NumWide = [u64; 4];

/// V4R-38: a finite i256 sum keeps |sum| < 2^254 (its top word in [-2^62, 2^62)); a term is under 2^183 (55 digits) and
/// a sum of 2^63 of them under 2^246, so the OVERFLOW code (as `num_add`'s, raised 22003 by the final) is never reached
/// by rows; two in-range sums add without wrapping.
pub fn num_add256(a: NumWide, b: NumWide) -> NumWide {
    let (at, bt) = (a[3] as i64, b[3] as i64);
    let sent = |c: i64| [c as u64, 0, 0, NUM_SENT_HI as u64];
    if at != NUM_SENT_HI && bt != NUM_SENT_HI {
        let lo = |w: NumWide| (w[1] as u128) << 64 | w[0] as u128;
        let hi = |w: NumWide| ((w[3] as u128) << 64 | w[2] as u128) as i128;
        let (l, c) = lo(a).overflowing_add(lo(b));
        let h = hi(a).wrapping_add(hi(b)).wrapping_add(c as i128);
        let s = [l as u64, (l >> 64) as u64, h as u64, (h >> 64) as u64];
        return if (s[3] as i64).wrapping_add(NUM_SUM_HI_LIMIT) >= 0 { s } else { sent(NUM_SP_OVERFLOW) };
    }
    // a special (or OVERFLOW) wins over a finite sum; two combine as `num_add`'s codes
    match (at == NUM_SENT_HI, bt == NUM_SENT_HI) {
        (false, _) => b,
        (_, false) => a,
        _ => sent(num_add(((NUM_SENT_HI as i128) << 64) | a[0] as i128, ((NUM_SENT_HI as i128) << 64) | b[0] as i128) as u64 as i64),
    }
}

/// The record's byte face for generated code (ir-probe.md §12.2); `valid_off` `NO_VALIDITY` without the word.
/// The 8-key tier's face: a wide layout's first eight slot offsets (generated code over a wide record reads the `WideLayout` instead).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecFace {
    pub base_off: u32,
    pub slot_off: [u32; MAX_RECORD_SLOTS],
    pub nslots: u32,
    pub sentinel_off: u32,
    pub valid_off: u32,
    pub moving_bit: u64,
    pub size: u32,
}

pub const NO_VALIDITY: u32 = u32::MAX;

pub const fn rec_face(l: &RecordLayout) -> RecFace {
    let mut slot_off = [0u32; MAX_RECORD_SLOTS];
    let mut j = 0;
    while j < l.nslots() && j < MAX_RECORD_SLOTS {
        slot_off[j] = l.slot_off(j);
        j += 1;
    }
    RecFace { base_off: HASHAGG_HDR as u32, slot_off, nslots: l.nslots() as u32, sentinel_off: l.sentinel_off(), valid_off: if l.has_validity() { l.valid_off() } else { NO_VALIDITY }, moving_bit: MOVING, size: l.size() }
}

pub type RecOpenFn = Option<unsafe extern "C" fn(*mut crate::Ctx, u32, *const RecordLayout, u32, u32, *const u64) -> i64>;

/// `ProgramBlock.rec_open` (ir-probe.md §12.2): the record of `keys` in region `partial`'s table (0 own, 1 / 2 consumed), opened when new; 0 = refused.
/// # Safety
/// `ctx` is bound; `layout` and `keys` (`layout.nkeys` words) are readable; `HashAgg::at`'s contract.
pub unsafe extern "C" fn rec_open(ctx: *mut crate::Ctx, partial: u32, layout: *const RecordLayout, cap: u32, limit: u32, keys: *const u64) -> i64 {
    let region = match partial {
        0 => crate::program::partial(ctx),
        k => crate::program::consumed(ctx, k as usize - 1),
    };
    let layout = &*layout;
    let t = HashAgg::at(region, layout, cap, limit);
    let r = if layout.nkeys == 0 { Some(t.single()) } else { t.find_or_insert(core::slice::from_raw_parts(keys, layout.nkeys())) };
    match r {
        Some(r) => r.as_ptr() as i64,
        None => 0,
    }
}

/// The largest `n` of the stack-heap selects (`nth_smallest_key`, `nth_smallest_key_held`); a host passes a scratch
/// sized from the records held to `nth_smallest_key_held_in`, which takes any `n`.
pub const SELECT_MAX: usize = 1024;
pub const HASHED: u8 = 0;
/// Records appended in order at `0..len`, never probed (the leader's post-merge orderings).
pub const DENSE: u8 = 1;

/// The hash aggregate (and, at capacity 1, the plain aggregate): an open-addressed table of records that doubles in place while
/// `bytes(2·cap)` fits `limit` (the region it was laid out in, later the budget share) and refuses an insert past it (the spill's hook, §2
/// commit 4). Header 48 bytes; zeroed = empty.
#[repr(C)]
pub struct HashAgg {
    pub cap: u32,
    pub len: u32,
    pub limit: u32,
    pub mode: u8,
    /// The route bits every record of the table already consumed (a partition's and its sub-splits'): homes read the bits
    /// below them (`Depth`), so a table filled from one partition stays uniform. 0 for a table fed unrouted.
    pub route_bits: u8,
    pub flags: u8,
    /// `TABLE_AGG` (offset `TABLE_KIND_OFF`, shared with `HashBuild`): what a breaker region holds.
    pub kind: u8,
    pub layout: RecordLayout,
    /// Zero-sized: the table is written through `&self` (the partitioned merge's shared
    /// header, `len` and the records), so `&HashAgg` must not be `Freeze` — an optimiser
    /// treating it as read-only dropped phase 2's inserts in release builds.
    pub cell: core::cell::UnsafeCell<[u8; 0]>,
}

// SAFETY: the shared-reference writes are the partitioned merge's, each job inside its own slices
// and `len` through an atomic; nothing else writes through `&HashAgg`.
unsafe impl Sync for HashAgg {}

pub const HASHAGG_HDR: usize = size_of::<HashAgg>();
/// The table space a dense distinct region keeps beside its area (the partition tables and the round-2 buffers' floor):
/// exec's keyed owner lays a dense area only where this much table space is left beside it (table/owner.rs), and the
/// kernel's dense index takes an area only when it fits beside it (kernels ops/group t3_dense `Dense::of`), so both read
/// this one value. A region without it lays no dense area and takes the pair path. CHOSEN, not measured: the least table
/// a dense region keeps for the keys outside its domain.
pub const DENSE_TABLE_FLOOR: usize = 1 << 20;

/// The shortest finished breaker region a reader takes as laid out: a shorter one was never laid out (no row reached its
/// producer), so the reader (a two-pass protocol's region reader, a set operation's owned table) takes it as empty.
/// CHOSEN, not measured: the floor both readers used.
pub const MIN_REGION: usize = 64;
/// Byte 15 of every spillable table header names its kind (the spill dispatches on it).
pub const TABLE_KIND_OFF: usize = 15;
pub const TABLE_AGG: u8 = 1;
pub const TABLE_BUILD: u8 = 2;
/// A lowerv4 join build's own region layout (join/build.rs).
pub const TABLE_V4BUILD: u8 = 7;
/// `HashAgg.flags`: a `DenseFront` follows the table at `limit` (decision 7; the kernel's cache).
pub const FLAG_FRONT: u8 = 1;
/// The leader rendered this table's text words (code keys): the emit orders them by bytes.
pub const FLAG_RENDERED: u8 = 16;
pub const FLAG_SHARE_LIMIT: u8 = 32; // opened under `SHARE` (limit = the region's arm): only then does a merge open the leader under `SHARE`

/// The kind byte of a live table region (0 when the region was never laid out).
/// # Safety
/// `p` points at least `HASHAGG_HDR` readable bytes.
pub unsafe fn table_kind(p: *const u8) -> u8 {
    *p.add(TABLE_KIND_OFF)
}
/// `limit` for a share-budgeted table: the region owner (the driver, from the
/// budget share; a test) armed the header's `limit` before the kernel ran.
pub const SHARE: u32 = 0;

impl HashAgg {
    pub const fn bytes(layout: &RecordLayout, cap: u32) -> u32 {
        (HASHAGG_HDR + cap as usize * layout.size() as usize) as u32
    }

    pub const fn cap_of(layout: &RecordLayout, bytes: u32) -> u32 {
        let n = (bytes as usize).saturating_sub(HASHAGG_HDR) / layout.size() as usize;
        if n == 0 {
            0
        } else {
            1 << (usize::BITS - 1 - n.leading_zeros())
        }
    }

    /// # Safety
    /// `p` points at `limit` writable bytes, 8-aligned, zeroed or a live table of `layout`
    /// (any capacity); `bytes(layout, cap) <= limit`, `cap` a power of two. With `limit ==
    /// SHARE` the region's armed `limit` (`arm`) is the bound.
    pub unsafe fn at<'a>(p: *mut u8, layout: &RecordLayout, cap: u32, limit: u32) -> &'a mut HashAgg {
        let t = aligned_mut::<HashAgg>(p);
        let shared = limit == SHARE;
        let limit = if shared { t.limit } else { limit };
        assert!(limit != 0, "hash aggregate region without a limit");
        if t.cap == 0 {
            let cap = cap.min(HashAgg::cap_of(layout, limit)).max(1);
            assert!(cap.is_power_of_two() && HashAgg::bytes(layout, cap) <= limit, "hash aggregate shape");
            // the region beyond the header may be dirty (a share zeroed over its prefix only): the records are zeroed here
            core::ptr::write_bytes(p.add(HASHAGG_HDR), 0, cap as usize * layout.size() as usize);
            t.cap = cap;
            t.limit = limit;
            t.layout = *layout;
            t.kind = TABLE_AGG;
            if shared {
                t.flags |= FLAG_SHARE_LIMIT;
            }
        }
        // D2600: a region of another kind (a wrong region index) is reported by its scalars alone: its layout bytes are not a
        // layout (the derived Debug follows the wide tier's word, and a garbage one asked for 7.6e18 bytes, aborting the backend)
        assert!(t.kind == TABLE_AGG, "hash aggregate shape: the region holds kind {} cap {} limit {}, not a hash aggregate", t.kind, t.cap, t.limit);
        assert!(t.layout.same(layout) && t.limit == limit, "hash aggregate shape: the region holds cap {} limit {} layout {:x?}, opened for limit {} layout {:x?}", t.cap, t.limit, t.layout.words(), limit, layout.words());
        t
    }

    /// Arm a zeroed region with the bytes a table may grow into (the driver, from the budget share).
    /// # Safety
    /// `p` points at a zeroed region of at least `HASHAGG_HDR` bytes, 8-aligned.
    pub unsafe fn arm(p: *mut u8, limit: u32) {
        (*(p as *mut HashAgg)).limit = limit;
    }

    /// # Safety
    /// `p` points at a table `at` initialised.
    pub unsafe fn view<'a>(p: *const u8) -> &'a HashAgg {
        aligned_ref::<HashAgg>(p)
    }

    /// The table's records all consumed `at`'s route bits (a partition of `pbits` and `depth` sub-splits). A clear keeps
    /// them: a table emptied by a spill keeps taking the same partition's records.
    pub fn set_route_bits(&mut self, at: Depth) {
        debug_assert!(self.len == 0, "route bits set on an empty table only");
        self.route_bits = at.used() as u8;
    }

    #[inline(always)]
    fn depth(&self) -> Depth {
        // D446: `set_route_bits` stores a Depth's own bits only, so they never pass ROUTE_BITS (`Depth::stored`)
        Depth::stored(self.route_bits as u32)
    }

    /// `(capacity bits, depth)`: a probe loop hoists them and takes `h.home(bits, depth)` per record.
    #[inline(always)]
    pub fn home_bits(&self) -> (u32, Depth) {
        (self.cap.trailing_zeros(), self.depth())
    }

    /// The home slot of `h` in this table (a power-of-two capacity): the bits below the table's route bits.
    #[inline(always)]
    pub fn home(&self, h: KeyHash) -> usize {
        h.home(self.cap.trailing_zeros(), self.depth())
    }

    /// True at the fill load (7/8: the owner tables' sizing assumes it; `AGG_GROW_LOAD` is the phase-1 table's), and never
    /// past the last slot but one (D282: under 8 slots `cap / 8` is 0, and `fold_slot`'s probe of a table filled to its last
    /// slot found no empty slot to stop at).
    #[inline(always)]
    fn at_load(&self) -> bool {
        self.len >= self.cap - (self.cap / 8).max(1)
    }

    fn base(&self) -> *mut u8 {
        unsafe { (self as *const HashAgg as *mut u8).add(HASHAGG_HDR) }
    }

    #[inline(always)]
    pub fn base_ptr(&self) -> *mut u8 {
        self.base()
    }

    #[inline(always)]
    pub fn record(&self, i: usize) -> Rec<'_> {
        debug_assert!(i < self.cap as usize);
        unsafe { Rec::at(self.base().add(i * self.layout.size() as usize), &self.layout) }
    }

    pub fn records(&self) -> impl Iterator<Item = Rec<'_>> + '_ {
        (0..self.cap as usize).map(|i| self.record(i)).filter(|r| r.used())
    }

    pub fn single(&mut self) -> Rec<'_> {
        debug_assert_eq!((self.layout.nkeys, self.cap), (0, 1));
        if !self.record(0).used() {
            self.record(0).open(&[]);
            self.len = 1;
        }
        self.record(0)
    }

    #[inline(always)]
    pub fn find_or_insert(&mut self, keys: &[u64]) -> Option<Rec<'_>> {
        let i = self.find_or_insert_index(keys)?;
        Some(self.record(i))
    }

    /// The owner fold's probe (`h` precomputed): `(slot, true)` found, `(slot, false)` the first
    /// empty slot, counted (the caller copies the record whole into it, §3.1); `None` at the grow load.
    #[inline(always)]
    pub fn fold_slot(&mut self, keys: &[u64], h: KeyHash) -> Option<(usize, bool)> {
        debug_assert_eq!(self.mode, HASHED);
        let sentinel = self.layout.sentinel_off() as usize;
        let mask = self.cap as usize - 1;
        let rec = self.layout.size() as usize;
        let base = self.base();
        let mut i = self.home(h);
        loop {
            let p = unsafe { base.add(i * rec) };
            if unsafe { *(p.add(sentinel) as *const u64) } == 0 {
                if self.at_load() {
                    return None;
                }
                self.len += 1;
                return Some((i, false));
            }
            if keys_eq(p as *const u64, keys) {
                return Some((i, true));
            }
            i = (i + 1) & mask;
        }
    }

    #[inline(always)]
    pub fn home_ptr(&self, h: KeyHash) -> *const u8 {
        unsafe { self.base().add(self.home(h) * self.layout.size() as usize) }
    }

    /// The generic owner fold (`FoldFn`'s contract, `Rec::fold_newer` per slot: the source record is the newer; the
    /// home prefetched `FOLD_PF` records ahead); `FOLD_OVERFLOW` when a float merge overflowed.
    /// # Safety
    /// `recs` holds `nrecs` whole records of this table's layout, read-only during the fold.
    pub unsafe fn fold_run(&mut self, recs: *const u8, nrecs: usize, split: SubSplit, cand: &mut Candidates) -> i64 {
        self.fold_run_in(recs, nrecs, split, cand, None)
    }

    /// V4R-375: `fold_run` over records holding hosted states: a new group copies the source record with its states
    /// emptied (`clear_states`), then every source's held states combine in (`Rec::fold_with`), the first included, so
    /// the owner never shares or writes a source's state and a re-read folds the unchanged partials again. `FOLD_HOST`
    /// when a combine raised.
    /// # Safety
    /// `fold_run`'s.
    pub unsafe fn fold_run_with(&mut self, recs: *const u8, nrecs: usize, split: SubSplit, cand: &mut Candidates, combine: &mut StateCombine<'_>) -> i64 {
        self.fold_run_in(recs, nrecs, split, cand, Some(combine))
    }

    unsafe fn fold_run_in(&mut self, recs: *const u8, nrecs: usize, split: SubSplit, cand: &mut Candidates, mut combine: Option<&mut StateCombine<'_>>) -> i64 {
        let layout = self.layout;
        let (rec, nkeys, pkeys) = (layout.size() as usize, layout.nkeys(), layout.partition_keys());
        let mut ring = [KeyHash::word(0); FOLD_PF];
        for i in 0..nrecs.min(FOLD_PF) {
            let h = hash_at(recs.add(i * rec), nkeys);
            ring[i] = h;
            prefetch_read(self.home_ptr(h));
        }
        let mut folded = 0i64;
        for i in 0..nrecs {
            let h = ring[i % FOLD_PF];
            if i + FOLD_PF < nrecs {
                let h2 = hash_at(recs.add((i + FOLD_PF) * rec), nkeys);
                ring[i % FOLD_PF] = h2;
                prefetch_read(self.home_ptr(h2));
            }
            let p = recs.add(i * rec);
            if !split.keeps(if pkeys == nkeys { h } else { hash_at(p, pkeys) }) {
                continue;
            }
            let r = Rec::at(p as *mut u8, &layout);
            let Some((j, found)) = self.fold_slot(r.keys(), h) else { return -1 - folded };
            folded += 1;
            let d = self.record(j);
            match (found, combine.as_deref_mut()) {
                // NEW-numeric-1 (P): the owner reads an order-key layout's partition oldest first (parallel merge.rs
                // `fold_sub`: files, chains, then table records), so `r` is the newer
                (true, None) if d.fold_newer(&r).is_err() => return FOLD_OVERFLOW,
                (true, None) => {}
                (false, None) => d.copy_from(&r),
                (true, Some(c)) => {
                    if let Err(e) = d.fold_with(&r, c) {
                        return e;
                    }
                }
                (false, Some(c)) => {
                    d.copy_from(&r);
                    d.clear_states();
                    if let Err(e) = d.combine_states(&r, c) {
                        return e;
                    }
                }
            }
            cand.note(d.as_ptr(), j);
        }
        folded
    }

    #[inline(always)]
    pub fn find_or_insert_index(&mut self, keys: &[u64]) -> Option<usize> {
        self.find_or_insert_index_hashed(keys, KeyHash::of(keys))
    }

    /// One pass, the hash given (a caller that hashed ahead to prefetch): the probe chain ends at the first empty slot, which is where a new key goes.
    #[inline(always)]
    pub fn find_or_insert_index_hashed(&mut self, keys: &[u64], h: KeyHash) -> Option<usize> {
        debug_assert_eq!(self.mode, HASHED);
        debug_assert_eq!(h, KeyHash::of(keys));
        let sentinel = self.layout.sentinel_off() as usize;
        loop {
            let mask = self.cap as usize - 1;
            let rec = self.layout.size() as usize;
            let base = self.base();
            let mut i = self.home(h);
            let mut left = self.cap as usize;
            while left > 0 {
                // SAFETY: `i` is masked into the table; a record is `rec` bytes at `base + i·rec`.
                let p = unsafe { base.add(i * rec) };
                if unsafe { *(p.add(sentinel) as *const u64) } == 0 {
                    if self.at_load() && self.grow() {
                        break;
                    }
                    self.len += 1;
                    Self::open_new(p, &self.layout, keys);
                    return Some(i);
                }
                if keys_eq(p as *const u64, keys) {
                    return Some(i);
                }
                i = (i + 1) & mask;
                left -= 1;
            }
            // a full table: grow and probe again, else the insert is refused (the spill's hook)
            if left == 0 && !self.grow() {
                return None;
            }
        }
    }

    /// A new key's record opened out of line, once per first sight of a key: a cold stub off the probe loop's registers.
    #[cold]
    #[inline(never)]
    fn open_new(p: *mut u8, layout: &RecordLayout, keys: &[u64]) {
        unsafe { Rec::at(p, layout) }.open(keys);
    }

    pub fn find(&self, keys: &[u64]) -> Option<Rec<'_>> {
        self.find_index(keys, KeyHash::of(keys)).map(|i| self.record(i))
    }

    #[inline(always)]
    fn find_index(&self, keys: &[u64], h: KeyHash) -> Option<usize> {
        let mask = self.cap as usize - 1;
        let mut i = self.home(h);
        for _ in 0..=mask {
            let r = self.record(i);
            if !r.used() {
                return None;
            }
            if keys_eq(r.key_ptr(), keys) {
                return Some(i);
            }
            i = (i + 1) & mask;
        }
        None
    }

    /// Double in place when the doubled table fits `limit`: every record is marked moving, then settled from the low index up — a record moves to
    /// the first non-settled slot from its home (an empty one, or a moving one it swaps with), so a settled probe chain crosses settled slots only.
    pub fn grow(&mut self) -> bool {
        let cap = self.cap;
        if HashAgg::bytes(&self.layout, cap * 2) > self.limit || self.mode != HASHED {
            return false;
        }
        let rec = self.layout.size() as usize;
        unsafe { core::ptr::write_bytes(self.base().add(cap as usize * rec), 0, cap as usize * rec) };
        self.cap = cap * 2;
        for i in 0..cap as usize {
            let r = self.record(i);
            if r.used() {
                r.set_moving();
            }
        }
        self.resettle();
        true
    }

    /// Settle every moving record from the low index up (the grow's rule, above).
    fn resettle(&mut self) {
        let rec = self.layout.size() as usize;
        let mask = self.cap as usize - 1;
        for i in 0..self.cap as usize {
            loop {
                let r = self.record(i);
                if !r.moving() {
                    break;
                }
                let mut j = self.home(r.hash());
                while self.record(j).used() && !self.record(j).moving() {
                    j = (j + 1) & mask;
                }
                let dst = self.record(j);
                if j == i {
                    r.settle();
                } else if !dst.used() {
                    dst.copy_from(&r);
                    dst.settle();
                    r.clear();
                } else {
                    // D327: a swap in place, of any record width (a wide record, `LAYOUT_WIDE`, is past a fixed buffer's 256 B)
                    // SAFETY: `i != j`: two distinct records of `rec` bytes in the table.
                    unsafe { core::ptr::swap_nonoverlapping(r.p, dst.p, rec) };
                    dst.settle();
                }
            }
        }
    }

    /// Append a record at index `len` (`DENSE`: the table is an array from here on).
    pub fn append(&mut self, keys: &[u64]) -> Option<Rec<'_>> {
        debug_assert!(self.mode == DENSE || self.len == 0);
        self.mode = DENSE;
        if self.len >= self.cap && !self.grow_dense() {
            return None;
        }
        let i = self.len as usize;
        self.len += 1;
        let r = self.record(i);
        r.open(keys);
        Some(r)
    }

    fn grow_dense(&mut self) -> bool {
        if HashAgg::bytes(&self.layout, self.cap * 2) > self.limit {
            return false;
        }
        let rec = self.layout.size() as usize;
        unsafe { core::ptr::write_bytes(self.base().add(self.cap as usize * rec), 0, self.cap as usize * rec) };
        self.cap *= 2;
        true
    }

    pub fn clear(&mut self) {
        unsafe { core::ptr::write_bytes(self.base(), 0, self.cap as usize * self.layout.size() as usize) };
        self.len = 0;
        self.mode = HASHED;
    }

    /// Compact the used records to `0..len` ordered by key cell `k` (as i64), leaving a `DENSE` array (an in-place heap sort over the raw record
    /// bytes: no allocation, and no `&HashAgg` in the writers — a shared reference is `readonly` to the optimizer, so writes through pointers
    /// based on it would be hoisted across).
    pub fn sort_dense_by_key(&mut self, k: usize) {
        let rec = self.layout.size() as usize;
        let n = self.compact_dense();
        let base = self.base();
        let koff = self.layout.key_off(k);
        // SAFETY: records 0..n are live, `rec` bytes each, from `base`; every index is < n.
        unsafe {
            for start in (0..n / 2).rev() {
                sift_rec(base, rec, koff, start, n);
            }
            for end in (1..n).rev() {
                swap_rec(base, rec, 0, end);
                sift_rec(base, rec, koff, 0, end);
            }
        }
    }

    /// `sort_dense_by_key` under any strict order `less(a, b)` over record bytes.
    pub fn sort_dense_by(&mut self, less: &mut dyn FnMut(*const u8, *const u8) -> bool) {
        let rec = self.layout.size() as usize;
        let n = self.compact_dense();
        let base = self.base();
        let at = |i: usize| unsafe { base.add(i * rec) as *const u8 };
        // SAFETY: records 0..n are live, `rec` bytes each, from `base`; every index is < n.
        unsafe {
            let sift = |root: usize, end: usize, less: &mut dyn FnMut(*const u8, *const u8) -> bool| {
                let mut root = root;
                loop {
                    let mut child = 2 * root + 1;
                    if child >= end {
                        return;
                    }
                    if child + 1 < end && less(at(child), at(child + 1)) {
                        child += 1;
                    }
                    if !less(at(root), at(child)) {
                        return;
                    }
                    swap_rec(base, rec, root, child);
                    root = child;
                }
            };
            for start in (0..n / 2).rev() {
                sift(start, n, less);
            }
            for end in (1..n).rev() {
                swap_rec(base, rec, 0, end);
                sift(0, end, less);
            }
        }
    }

    /// `sort_dense_by` under the order of each record's first `g` words (as `u64`s, lexicographic): the part-collapsing
    /// owners' order. Records of 1..=16 whole words (a regexp_replace text group key's parted record is 9) sort as fixed-width values (the std in-place unstable sort: no
    /// allocation, no call per compare; V4R-168); any other width takes `sort_dense_by`'s heap sort.
    pub fn sort_dense_by_words(&mut self, g: usize) {
        let rec = self.layout.size() as usize;
        debug_assert!(8 * g <= rec, "the order's words lie in the record");
        if rec % 8 != 0 || rec > 128 || self.base() as usize % 8 != 0 {
            // SAFETY (the closure): each pointer is a live record of `rec >= 8 * g` bytes.
            let words = |p: *const u8| unsafe { core::slice::from_raw_parts(p as *const u64, g) };
            return self.sort_dense_by(&mut |a, b| words(a) < words(b));
        }
        let n = self.compact_dense();
        let base = self.base();
        fn sort<const W: usize>(base: *mut u8, n: usize, g: usize) {
            // SAFETY: records 0..n are live, `8 * W` bytes each, 8-aligned from `base`, and no other reference reaches them.
            let recs = unsafe { core::slice::from_raw_parts_mut(base as *mut [u64; W], n) };
            recs.sort_unstable_by(|a, b| a[..g].cmp(&b[..g]));
        }
        match rec / 8 {
            1 => sort::<1>(base, n, g),
            2 => sort::<2>(base, n, g),
            3 => sort::<3>(base, n, g),
            4 => sort::<4>(base, n, g),
            5 => sort::<5>(base, n, g),
            6 => sort::<6>(base, n, g),
            7 => sort::<7>(base, n, g),
            8 => sort::<8>(base, n, g),
            9 => sort::<9>(base, n, g),
            10 => sort::<10>(base, n, g),
            11 => sort::<11>(base, n, g),
            12 => sort::<12>(base, n, g),
            13 => sort::<13>(base, n, g),
            14 => sort::<14>(base, n, g),
            15 => sort::<15>(base, n, g),
            _ => sort::<16>(base, n, g),
        }
    }

    /// Runs of `same` neighbours of a sorted `DENSE` table folded into their first record (`Rec::fold`); the new `len`,
    /// or a float merge's overflow.
    pub fn coalesce_dense(&mut self, same: &mut dyn FnMut(*const u8, *const u8) -> bool) -> Result<u32, FloatOverflow> {
        debug_assert_eq!(self.mode, DENSE);
        let n = self.len as usize;
        let mut out = 0usize;
        for i in 0..n {
            if out > 0 && same(self.record(out - 1).as_ptr(), self.record(i).as_ptr()) {
                let (dst, src) = (self.record(out - 1), self.record(i));
                dst.fold(&src)?;
                src.clear();
                continue;
            }
            if i != out {
                self.record(out).copy_from(&self.record(i));
                self.record(i).clear();
            }
            out += 1;
        }
        self.len = out as u32;
        Ok(out as u32)
    }

    /// Used records moved to `0..len` in table order (`DENSE`); returns `len`.
    fn compact_dense(&mut self) -> usize {
        if self.mode == DENSE {
            // already `0..len` (a compacted or pruned table)
            return self.len as usize;
        }
        let mut n = 0usize;
        for i in 0..self.cap as usize {
            let r = self.record(i);
            if r.used() {
                if i != n {
                    self.record(n).copy_from(&r);
                    r.clear();
                }
                n += 1;
            }
        }
        debug_assert_eq!(n, self.len as usize);
        self.mode = DENSE;
        n
    }

    pub fn truncate_dense(&mut self, n: u32) {
        debug_assert_eq!(self.mode, DENSE);
        for i in n as usize..self.len as usize {
            self.record(i).clear();
        }
        self.len = self.len.min(n);
    }
}

unsafe fn key_rec(base: *mut u8, rec: usize, koff: usize, i: usize) -> i64 {
    *(base.add(i * rec + koff) as *const i64)
}

/// Records `a` and `b` (`a != b`) of `rec` bytes at `base` swapped in place (D327): any width, a wide record's
/// (`LAYOUT_WIDE`, past 256 B) as well — no fixed buffer (the old `[u8; 256]` took `rec` bytes past its end), no allocation.
unsafe fn swap_rec(base: *mut u8, rec: usize, a: usize, b: usize) {
    core::ptr::swap_nonoverlapping(base.add(a * rec), base.add(b * rec), rec);
}

unsafe fn sift_rec(base: *mut u8, rec: usize, koff: usize, mut root: usize, end: usize) {
    loop {
        let mut child = 2 * root + 1;
        if child >= end {
            return;
        }
        if child + 1 < end && key_rec(base, rec, koff, child) < key_rec(base, rec, koff, child + 1) {
            child += 1;
        }
        if key_rec(base, rec, koff, root) >= key_rec(base, rec, koff, child) {
            return;
        }
        swap_rec(base, rec, root, child);
        root = child;
    }
}

pub const TABLE_T1: u8 = 5;
pub const T1_HDR: usize = size_of::<T1>();
/// The phase-1 table's default bytes: 512 KiB; a plan may carry another.
/// MEASURED-HELDOUT (D06, held-out group corpus, 87 statements, W8, arms 128 KiB..2 MiB): flat within 2.6% from x/4 to 2x,
/// best at 512 KiB; at 2 MiB the benchmark set runs 2.1x slower, so the size never grows by a
/// fixed step (sweep file shapecheck/sweep-D06.tsv).
pub const T1_DEFAULT_BYTES: u32 = 512 << 10;

/// D06: the phase-1 table's default bytes on this host: `cost_profile::hash::t1_table_bytes` over the one host reading
/// (`cost_profile::machine::host`, a quarter of the L2 share as a power of two) that the planner's Gravity paths read
/// too; `T1_DEFAULT_BYTES` on the reference machine and wherever the L2 is not detected.
#[inline]
pub fn t1_default_bytes() -> u32 {
    cost_profile::hash::t1_table_bytes(cost_profile::machine::host()) as u32
}
/// The most a keyed fold's phase-1 table opens at for a bound of `groups` groups of `rec`-byte records on this host:
/// `cost_profile::hash::t1_fit_bytes`, the table that holds them when it is past the default and within 512 KiB, else the
/// default. A bound of exactly the default's slots is the lowering's stand-in for none (lowerv4 table/size.rs `groups`).
#[inline]
pub fn t1_open_bytes(groups: u64, rec: u64) -> u32 {
    let d = t1_default_bytes() as u64;
    if rec == 0 || groups == d / rec {
        return d as u32;
    }
    cost_profile::hash::t1_fit_bytes(groups, rec, cost_profile::machine::host()) as u32
}
/// The most any keyed fold's phase-1 table opens at on this host (`t1_open_bytes` over every bound).
#[inline]
pub fn t1_open_max_bytes() -> u32 {
    t1_default_bytes().max(cost_profile::hash::T1_FIT_MAX_BYTES as u32)
}
/// The least slots a keyed fold's phase-1 table opens with (kernels table/fold_entries.rs `t1_bytes`: the planner's groups x
/// `ADAPT_FACTOR` to a power of two, never below this).
pub const T1_MIN_SLOTS: u64 = 1024;
/// D2988: the least slots a grown phase-1 table opens with (kernels table/entries.rs `agg_open_grown`, `grown_first_bytes`).
/// It reserves the default's slots and doubles in place up to them (`T1::grow`), and every open lays its slots zeroed, so
/// a rerun fold of a row or two per rescan opened at `T1_MIN_SLOTS` zeroed 1,024 slots on each rescan. A full home pair
/// under the grow's fill (`can_grow`: the keys the table opened) evicts as at any size; `observe`'s window keeps
/// `T1_MIN_SLOTS` (`adapt_win`).
pub const T1_GROWN_MIN_SLOTS: u64 = 64;
/// A phase-1 table's key-image arena (`T1::img_words`, kernels `agg_image_put`; the min / max copies and the group key
/// copies bump the same arena): its first chunk (a text key's image per new group, so a chunk holds thousands of them)
/// and the largest chunk it doubles to.
pub const T1_IMAGE_CHUNK: usize = 64 << 10;
pub const T1_IMAGE_CHUNK_MAX: usize = 32 << 20;
/// D3312: a phase-1 table's image dedupe word, the last bytes before its pool: 0, an odd count `2n + 1` of the evictions
/// from a table under a quarter full since its last flush, or the address of its dedupe cache once armed (`T1::img_cache`).
pub const T1_DEDUPE_WORD: usize = 8;
/// D3312: the dedupe cache's entries, a fixed size whatever the table: `[image address, image bytes]` pairs in sets of two,
/// 32 KiB pinned once per table that arms it. A table under a quarter full that evicts holds few keys whose home pairs
/// collide, and those few re-open over and over: the cache needs to hold them, not every group.
pub const T1_DEDUPE_ENTRIES: usize = 2048;
/// D3312: the sparse evictions within one flush's fill that arm the dedupe: an eighth of the default table's slots. A table
/// whose keys only outrun its slots evicts once it is full and counts none; a refill after a flush counts a few dozen.
pub const T1_DEDUPE_ARM: u64 = 512;
/// D3312: a phase-1 table's side words before its pool: its copy-slot word (`T1::img_slots`), the image bytes its last
/// reclaim kept (`T1::img_kept`), then the dedupe word (`T1_DEDUPE_WORD`, the last).
pub const T1_SIDE_BYTES: usize = 16 + T1_DEDUPE_WORD;
/// D3312: the copy-slot word's mark that the lowerer named every slot of the table's records holding a key copy's address
/// (lowerv4 `TableDecl::copies`, kernels `agg_copies`); without it nothing is reclaimed. Slot `j < IMG_SLOTS_MAX` has two
/// bits at `2 j`: `IMG_SLOT_PLAIN` (0 or an image's address) or `IMG_SLOT_TAGGED` (a jsonb path key's word: an image's
/// address tagged `IMG_TAG_COPY`, else a code).
pub const IMG_SLOTS_KNOWN: u64 = 1 << 63;
pub const IMG_SLOT_PLAIN: u64 = 1;
pub const IMG_SLOT_TAGGED: u64 = 2;
pub const IMG_SLOTS_MAX: usize = 31;
/// D3312: a jsonb path key's kept text in its MinCode slot (lowerv4 ops/agg/emit_group_nocode.rs): `IMG_TAG_COPY | copy`.
/// Codes sit below it, copies at or over it and below `MINCODE_NONE`.
pub const IMG_TAG_COPY: u64 = 1 << 62;

/// D3312: an image arena chunk (`img_bump`): a header of four words `[next, size, fill, first]` (the chain's next chunk, 0
/// at its end; the chunk's bytes; where its images end once the cursor left it; the chain's first chunk), a start map
/// (bit `g`: an image starts at byte `8 g` of the chunk), then the images. The chain is the arena in bump order: a
/// reclaim (kernels table/img_reclaim.rs) slides the live images down it, and the bump takes its kept chunks before it
/// pins another, so an arena whose images die stops growing instead of growing with every copy.
pub const IMG_HDR: usize = 32;
const IMG_NEXT: usize = 0;
const IMG_SIZE: usize = 1;
const IMG_FILL: usize = 2;
const IMG_FIRST: usize = 3;

/// D3312: the start map's bytes in a chunk of `size` bytes (a bit per 8 bytes, whole words).
pub const fn img_map_bytes(size: usize) -> usize {
    size.div_ceil(64).next_multiple_of(8)
}

/// D3312: a chunk's first image byte, from its base.
pub const fn img_data_off(size: usize) -> usize {
    IMG_HDR + img_map_bytes(size)
}

/// D3312: the bytes of the chunk an arena pins after one of `prev` bytes (0: none) for an image of `need` bytes: 64 KiB
/// doubling to 32 MiB, or the image's own bytes with the header and the map when it is larger.
pub const fn img_chunk_bytes(prev: usize, need: usize) -> usize {
    let s = if 2 * prev < T1_IMAGE_CHUNK { T1_IMAGE_CHUNK } else if 2 * prev > T1_IMAGE_CHUNK_MAX { T1_IMAGE_CHUNK_MAX } else { 2 * prev };
    if s - img_data_off(s) >= need {
        return s;
    }
    // the map of a chunk at most twice the image's bytes past the header covers this one
    need + IMG_HDR + img_map_bytes(2 * (need + IMG_HDR) + 128)
}

/// D3312: word `i` of the chunk at `base`'s header.
/// # Safety
/// `base` is a chunk `img_bump` laid.
#[inline(always)]
pub unsafe fn img_hdr(base: usize, i: usize) -> *mut u64 {
    (base as *mut u64).add(i)
}

/// D3312: the chunk at `base`: its next in the chain (0: the last), its bytes, the chain's first chunk.
/// # Safety
/// As `img_hdr`.
#[inline]
pub unsafe fn img_chunk(base: usize) -> (usize, usize, usize) {
    (*img_hdr(base, IMG_NEXT) as usize, *img_hdr(base, IMG_SIZE) as usize, *img_hdr(base, IMG_FIRST) as usize)
}

/// D3312: where the images of the chunk at `base` end once the cursor left it (`img_set_fill`); the open chunk's end is
/// its cursor.
/// # Safety
/// As `img_hdr`.
#[inline]
pub unsafe fn img_fill(base: usize) -> usize {
    *img_hdr(base, IMG_FILL) as usize
}

/// D3312: as `img_fill`, set.
/// # Safety
/// As `img_hdr`.
#[inline]
pub unsafe fn img_set_fill(base: usize, at: usize) {
    *img_hdr(base, IMG_FILL) = at as u64;
}

/// D3312: the open chunk of image cursor `w` (`[cur, end, chunk]`), 0 when it has none.
/// # Safety
/// `w` is three live words of a cursor `img_bump` bumps.
#[inline(always)]
pub unsafe fn img_open_chunk(w: *const u64) -> usize {
    if *w == 0 { 0 } else { (*w.add(1) - *w.add(2)) as usize }
}

/// D3312: an image starts at `at` in the chunk at `base`.
/// # Safety
/// As `img_hdr`; `at` is 8-aligned within the chunk.
#[inline(always)]
pub unsafe fn img_mark(base: usize, at: usize) {
    let g = (at - base) / 8;
    *((base + IMG_HDR) as *mut u64).add(g / 64) |= 1 << (g % 64);
}

/// D3312: the chunk at `base`'s start map cleared (a reclaim marks the moved images again).
/// # Safety
/// As `img_hdr`.
pub unsafe fn img_unmark_all(base: usize) {
    let size = *img_hdr(base, IMG_SIZE) as usize;
    core::ptr::write_bytes((base + IMG_HDR) as *mut u8, 0, img_map_bytes(size));
}

/// D3312: the start of the image that holds byte `at` of the chunk at `base` (the last marked start at or before it), and
/// its end (the next marked start, else `fill`); `None` when no start precedes `at` in the chunk.
/// # Safety
/// As `img_hdr`; `at` lies in the chunk's images, below `fill`.
pub unsafe fn img_extent(base: usize, at: usize, fill: usize) -> Option<(usize, usize)> {
    let map = (base + IMG_HDR) as *const u64;
    let g = (at - base) / 8;
    let (mut i, mut bits) = (g / 64, *map.add(g / 64) & (u64::MAX >> (63 - g % 64)));
    while bits == 0 {
        if i == 0 {
            return None;
        }
        i -= 1;
        bits = *map.add(i);
    }
    let start = 64 * i + 63 - bits.leading_zeros() as usize;
    if 8 * start < img_data_off(*img_hdr(base, IMG_SIZE) as usize) {
        return None;
    }
    let last = (fill - base) / 8;
    let mut s = start + 1;
    while s < last {
        let word = *map.add(s / 64) >> (s % 64);
        if word != 0 {
            return Some((base + 8 * start, base + 8 * (s + word.trailing_zeros() as usize).min(last)));
        }
        s = (s / 64 + 1) * 64;
    }
    Some((base + 8 * start, base + 8 * last))
}

/// D3312: `size` bytes at `p` laid as an arena's first chunk (as `img_bump` lays a pin): its cursor words `[cur, end, chunk]`.
/// # Safety
/// `p` holds `size` writable, 8-aligned bytes, `size` past `img_data_off(size)`.
pub unsafe fn img_lay(p: *mut u8, size: usize) -> [u64; 3] {
    let base = p as usize;
    for (i, v) in [(IMG_NEXT, 0), (IMG_SIZE, size), (IMG_FILL, base + img_data_off(size)), (IMG_FIRST, base)] {
        *img_hdr(base, i) = v as u64;
    }
    img_unmark_all(base);
    [(base + img_data_off(size)) as u64, (base + size) as u64, size as u64]
}

/// D3312: `need` bytes (a multiple of 8) bumped from image cursor `w` (`[cur, end, chunk]`, resolved: `T1::img_resolve`),
/// their start marked; past the open chunk, the chain's next kept chunk that holds them (`img_next_chunk`), else a chunk
/// from `pin`. Null when `pin` refused. Every copy host bumps through it (kernels `agg_image_put`, the per-code front's
/// copies; lowerv4 `key_copy`, `key_copy_fixed`, code_copy's and topn's copies), so a chain is one format.
/// # Safety
/// `w` is three live words of an image cursor that only this function bumps; `pin` returns null or that many writable,
/// 8-aligned bytes that live as long as the cursor's images.
#[inline]
pub unsafe fn img_bump(w: *mut u64, need: usize, pin: impl FnOnce(u64) -> *mut u8) -> *mut u8 {
    debug_assert!(need % 8 == 0, "an image's bytes are whole words");
    if *w == 0 || *w.add(1) - *w < need as u64 {
        if !img_next_chunk(w, need, pin) {
            return core::ptr::null_mut();
        }
    }
    let d = *w as usize;
    *w += need as u64;
    img_mark((*w.add(1) - *w.add(2)) as usize, d);
    d as *mut u8
}

/// D3312: `img_bump`'s next chunk: the open chunk's end noted, then the chain's next chunk whose images fit `need` (a
/// smaller kept one left empty), else one from `pin` linked at the chain's end. False when `pin` refused.
#[cold]
unsafe fn img_next_chunk(w: *mut u64, need: usize, pin: impl FnOnce(u64) -> *mut u8) -> bool {
    let (mut last, mut next) = (0usize, 0usize);
    let open = img_open_chunk(w);
    if open != 0 {
        img_set_fill(open, *w as usize);
        (last, next) = (open, *img_hdr(open, IMG_NEXT) as usize);
        while next != 0 {
            let size = *img_hdr(next, IMG_SIZE) as usize;
            if size - img_data_off(size) >= need {
                break;
            }
            img_set_fill(next, next + img_data_off(size));
            (last, next) = (next, *img_hdr(next, IMG_NEXT) as usize);
        }
    }
    if next == 0 {
        let size = img_chunk_bytes(if last == 0 { 0 } else { *img_hdr(last, IMG_SIZE) as usize }, need);
        let p = pin(size as u64);
        if p.is_null() {
            return false;
        }
        next = p as usize;
        img_lay(p, size);
        if last != 0 {
            *img_hdr(next, IMG_FIRST) = *img_hdr(last, IMG_FIRST);
            *img_hdr(last, IMG_NEXT) = next as u64;
        }
    }
    let size = *img_hdr(next, IMG_SIZE) as usize;
    debug_assert!(size - img_data_off(size) >= need, "a chunk holds the image it was taken for");
    (*w, *w.add(1), *w.add(2)) = ((next + img_data_off(size)) as u64, (next + size) as u64, size as u64);
    true
}
/// The light code front (kernels dict/strategies front_light.rs): its first arrays' capacity in codes. CHOSEN, not measured
/// (the text front's rung value, carried unchanged). Its largest part dictionary and its walk choice live in the cost home.
pub const CODE_FRONT_MIN_CODES: u64 = 1024;
pub use cost_profile::front::{code_key_pays, light_claim_takes, light_dense, light_keeps, light_takes, light_ways, light_ways_max_codes, LIGHT_DENSE, LIGHT_KEYS_MAX_GROUPS, LIGHT_MAX_CODES, LIGHT_WAYS, LIGHT_WAYS_MAX_CODES};
/// A block's bytes: at most `BLOCK_MAX` plus the stagger (a block's bytes left ride its partition's head word).
/// MEASURED-HELDOUT (D07, held-out group corpus, 87 statements, W8): 8 KiB 131.4 s, 16 KiB 130.7 s, 32 KiB 130.2 s, flat
/// and best at 32 KiB; larger blocks do not fit `BLOCK_MAX` (sweep file shapecheck/sweep-D07.tsv).
pub const BLOCK_BYTES: u32 = 32 << 10;
pub const BLOCK_STAGGER: usize = 64;
/// V4R-236: every chain's last block is partly empty; with fewer full blocks a chain than this the partial blocks eat the
/// pool (a table flushes with most chains a block short), so the blocks are halved to keep that waste to a small block.
/// MEASURED-HELDOUT (CR2-02, held-out group corpus, 87 statements, W8): 1..24 within 2.5% (127.6..130.8 s), noise level
/// for one take; the shipped 6 stays (sweep file shapecheck/sweep-NEW-constants-258.tsv).
pub const BLOCK_FULL_CHAIN: usize = 6;
/// V4R-236: the smallest halved block: below it the per-block cost (the block switches, the extra spill units) outweighs
/// the partial-block waste halving saves, and the block stays as before.
/// MEASURED-HELDOUT (CR2-02, same corpus): 2 KiB..32 KiB within 0.8% (130.1..131.1 s); the shipped 8 KiB stays
/// (sweep file shapecheck/sweep-NEW-constants-259.tsv).
pub const BLOCK_HALVED_MIN: usize = 8 << 10;
/// D08: the most partitions a phase-1 table routes into: a format fact, not a tuning knob. It sizes the owner's run
/// directory (`OWNER_RUN_OFF`), the build partial's counting sort (`MAX_BUILD_PARTS`) and the split depth's route bits
/// (`Depth`: pbits <= 11). The fan-out itself is picked from the bytes (`driver::sink::partition_plan`: twice the
/// statement's group bytes over the cache slice); past this cap a partition holds more than a slice, its table grows
/// into the grant, and past the grant `mem::spill` re-splits it while the hash has route bits left (DR-17).
pub const T1_MAX_PARTS: u32 = 2048;
/// DR-6: the reference slice one phase-1 partition's groups are sized to: a quarter of the reference machine's 2 MiB L2
/// (cost_profile `Machine::REFERENCE`). MEASURED-HELDOUT as program's `cost::PARTITION_BYTES` (DR-2: flat over 128 KiB ..
/// 2 MiB). The lowering's default bits read it, so a program is the same on every machine.
pub const PARTITION_SLICE: u64 = 512 << 10;
/// DR-6: a breaker's partition bits when the planner gave no rows.
pub const PARTITION_DEFAULT_BITS: u8 = 4;
/// DR-6: the fewest partition bits a sized breaker takes.
pub const PARTITION_MIN_BITS: u8 = 3;
/// DR-6: the fewest partition bits a phase-1 table opens with at width 8 or more, so the owner phase has a
/// unit for each worker to take.
pub const PARTITION_WIDE_MIN_BITS: u8 = 6;
/// DR-6: the width from which `opened_bits` floors a phase-1 table at `PARTITION_WIDE_MIN_BITS`.
pub const PARTITION_WIDE_WIDTH: u16 = 8;

/// DR-6, the one partition rule (decision 3; every copy reads it: program `cost::spill_bits`, kernels `table::params`,
/// exec table/owner.rs): `rows` records of `record` bytes over partitions of `slice` bytes each, a power of two, from
/// `PARTITION_MIN_BITS` to T1_MAX_PARTS' bits; `PARTITION_DEFAULT_BITS` with no rows. Saturating, so no estimate overflows.
pub const fn partition_bits(rows: u64, record: u64, slice: u64) -> u8 {
    if rows == 0 {
        return PARTITION_DEFAULT_BITS;
    }
    let slice = if slice == 0 { 1 } else { slice };
    let parts = rows.saturating_mul(record).div_ceil(slice);
    let max = T1_MAX_PARTS.trailing_zeros() as u8;
    let bits = match parts.checked_next_power_of_two() {
        Some(p) => p.trailing_zeros() as u8,
        None => max,
    };
    if bits < PARTITION_MIN_BITS {
        PARTITION_MIN_BITS
    } else if bits > max {
        max
    } else {
        bits
    }
}

/// DR-6: the bits a phase-1 table opens with: an arm run's `ctx::ARM_RUN_PBITS` (every arm partitions alike, D393b),
/// else `bits` floored at `PARTITION_WIDE_MIN_BITS` at width 8 or more. Every open reads it.
pub const fn opened_bits(bits: u8, width: u16, arm_run: bool) -> u8 {
    if arm_run {
        crate::ctx::ARM_RUN_PBITS
    } else if width >= PARTITION_WIDE_WIDTH && bits < PARTITION_WIDE_MIN_BITS {
        PARTITION_WIDE_MIN_BITS
    } else {
        bits
    }
}

/// DR-6: reference-slice bits `bits` (`partition_bits` over `PARTITION_SLICE`) re-cut to a machine slice of `slice` bytes:
/// each halving of the slice doubles the partitions, each doubling halves them, within `partition_bits`' clamp. The
/// identity on the reference machine and for a slice of 0 (none read).
pub const fn machine_bits(bits: u8, slice: u64) -> u8 {
    if slice == 0 || slice == PARTITION_SLICE {
        return bits;
    }
    let (r, s) = (PARTITION_SLICE.ilog2() as i32, slice.ilog2() as i32);
    let (b, max) = (bits as i32 + r - s, T1_MAX_PARTS.trailing_zeros() as i32);
    if b < PARTITION_MIN_BITS as i32 {
        PARTITION_MIN_BITS
    } else if b > max {
        max as u8
    } else {
        b as u8
    }
}

/// DR-6: the most partition bits, at most `bits` and not under `PARTITION_MIN_BITS`, whose block pool gives every chain
/// four halved blocks (`2^bits x BLOCK_BYTES`) beside a `t1`-byte table inside a `share`-byte region (`T1::at_sized`
/// shrinks blocks toward one record under that, and a chain of one-record blocks flushes per record); `bits` when no
/// share is known (0). `opened_bits`' width floor stays above it.
pub const fn granted_bits(bits: u8, share: u64, t1: u64) -> u8 {
    if share == 0 {
        return bits;
    }
    let room = share.saturating_sub(t1);
    let mut b = bits;
    while b > PARTITION_MIN_BITS && (BLOCK_BYTES as u64) << b > room {
        b -= 1;
    }
    b
}

/// B3-17: the expected groups a partition's table is sized for, over `fit_bits`' partitions (a load factor of a half).
pub const PARTITION_FIT_FACTOR: u64 = 2;

/// B3-17, the floor under `granted_bits` below `PARTITION_WIDE_WIDTH`: the fewest partition bits, from `PARTITION_MIN_BITS` to
/// `bits`, at which one partition's table of `groups / 2^k` groups (at least one) times `PARTITION_FIT_FACTOR`, a power of two
/// of `rec`-byte records, fits a `share`-byte region; `bits` with no estimate (`groups` 0) or no share known. Fewer partitions
/// than this outgrow the share where few workers fold (1.37-1.46x on 4M groups at a quarter grant at W0, W2 and W4).
pub const fn fit_bits(bits: u8, groups: u64, rec: u64, share: u64) -> u8 {
    if groups == 0 || rec == 0 || share == 0 || bits <= PARTITION_MIN_BITS {
        return bits;
    }
    let mut k = PARTITION_MIN_BITS;
    while k < bits {
        let per = groups >> k;
        let slots = if per == 0 { 1 } else { per }.saturating_mul(PARTITION_FIT_FACTOR);
        match slots.checked_next_power_of_two() {
            Some(n) if n.saturating_mul(rec) <= share => break,
            _ => k += 1,
        }
    }
    k
}

/// DR-6 (B3-16): the machine's phase-1 slice the kernels' fold opens re-cut their bits to (`machine_bits`), set once
/// per process by the driver (`sink::partition_bytes`' quarter, the slice the runner's bind reads); `PARTITION_SLICE` until
/// set, so a process with no driver (a kernel test) opens as the reference machine does.
static MACHINE_SLICE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub fn machine_slice() -> u64 {
    match MACHINE_SLICE.load(core::sync::atomic::Ordering::Relaxed) {
        0 => PARTITION_SLICE,
        s => s,
    }
}

/// D03: the machine the abi tables size by: an L2 share of four slices (`machine_slice` is a quarter of the driver's
/// `partition_bytes`, its L2 / SMT), the reference's otherwise.
pub fn detected_machine() -> cost_profile::machine::Machine {
    cost_profile::machine::Machine::detected(None, 4 * machine_slice(), 1, cost_profile::machine::Machine::REFERENCE.prefetch)
}

/// Sets `machine_slice`; 0 restores the reference's.
pub fn set_machine_slice(slice: u64) {
    MACHINE_SLICE.store(slice, core::sync::atomic::Ordering::Relaxed);
}
const NO_BLOCK: u32 = u32::MAX;
/// `T1.flags`: append-only — every record goes to its partition buffer, the table is bypassed.
pub const T1_BYPASS: u8 = 1;
/// `T1.flags`: held — a derived key's code front drains the table after every part's convert (the ladder's per-claim table).
pub const T1_HOLD: u8 = 2;
pub const T1_EST: u8 = 4; // `T1.flags`: the estimate's bypass, unconfirmed (kernels `EST_CONFIRM`)
/// `T1.flags`: D2587, a `compact` freed too little: the table's groups do not fit its region, so a full pool spills.
pub const T1_NOFOLD: u8 = 8;
/// `T1.flags`: the setop posture (option c'): the table's region is an address-space reservation past `limit` (a
/// `Budget::Pool` side, `T1::at_reserved`); its chain words cover the reservation's blocks, so `grow_pool` commits more of
/// the pool IN PLACE (nothing laid moves: the lowered appends hold head addresses across `agg_block`).
pub const T1_RESERVED: u8 = 16;
/// `T1.flags`: D2894, a `T1_RESERVED` table whose reservation is its breaker's PostgreSQL floor (hash_mem, the driver's
/// `RESERVE_FLOOR` answer): a full pool commits more of it in place before any `compact` (kernels `folded_in_memory`),
/// as PostgreSQL's HashAggregate fills hash_mem before it spills; a compact only once the floor is spent or refused.
pub const T1_FLOOR: u8 = 32;
/// `T1.flags`: a `T1_NOFOLD` table that started small (pass 1's bare table in a region laid at its reservation): never
/// compacted, but a full pool still commits more of its reservation in place before it spills (kernels `folded_in_memory`).
pub const T1_GROWS: u8 = 64;
/// `T1.flags`: laid out at its region's reservation (`T1::at_laid`, the region started small): a reopen keeps its partitions.
pub const T1_LAID: u8 = 128;

/// The phase-1 table (`partitioned-record.md` §3.1): `cap` fixed slots probed at the home and the
/// next, replace-on-collision (the home's occupant evicted whole into its partition buffer), and
/// `1 << pbits` buffers as chains of `block_bytes` blocks from a pool behind the table. Header 104
/// bytes, the first four words `HashAgg`'s (`cap`, `len`, `limit`, the kind byte); zeroed prefix + armed = empty.
#[repr(C)]
pub struct T1 {
    pub cap: u32,
    pub len: u32,
    pub limit: u32,
    pub pbits: u8,
    pub flags: u8,
    /// log2 of the slots the layout reserves (`at_sized`: the table grows in place up to them, `grow`); 0: fixed at `cap`.
    pub max_bits: u8,
    pub kind: u8,
    pub layout: RecordLayout,
    pub block_bytes: u32,
    pub nblocks: u32,
    pub next_block: u32,
    pub table_off: u32,
    pub pool_off: u32,
    /// The partition of every slot's occupant (`u32[cap]` behind the table): the eviction and the drain never rehash a record.
    pub hash_off: u32,
    /// The opened-slot list's entries (`u32[open_cap(max_cap)]` behind the hashes; `OPENED_OVER`: full, the drain walks).
    pub opened: u32,
    pub block_recs: u32,
    pub buffered: u64,
    pub inserts: u64,
    pub evictions: u64,
    pub flushes: u64,
    pub win_at: u64,
    pub win_ev: u64,
    /// D1392: the table's image arena (a kept varlena key's copy, a text extreme's, `agg_image_put`): the open chunk's
    /// cursor, its end and its size (`[cur, end, chunk]`), all in Program.pin's statement-lived chunks. The table lives for
    /// its worker's section (its own share, `partial(ctx)`), so do they: zeroed where the table is laid out (`at_sized`,
    /// `cap == 0`: a section's first claim, a rescan's re-armed region), kept by `grow`, `clear` and `reset_pool`. In a cell
    /// (Tree Borrows: a live `&T1` never freezes these bytes); read and written only through `T1::img_words`.
    pub img: core::cell::UnsafeCell<[u64; 3]>,
    pub cell: core::cell::UnsafeCell<[u8; 0]>,
}

const _: () = assert!(size_of::<T1>() == 160);
// OF-TG2-1: the planner reads the phase-1 layout through cost_profile (`t1_fold_records`): its sizes are these.
const _: () = assert!(T1_HDR as u64 == cost_profile::hash::T1_HDR_BYTES && T1_DEFAULT_BYTES as u64 == cost_profile::hash::T1_TABLE_BYTES);
pub const OPENED_OVER: u32 = u32::MAX;
/// D1392 (a): the third image word of a state that names its table's words by reference (`T1::img_resolve`).
pub const IMG_BY_REF: u64 = u64::MAX;

/// The opened-slot list's capacity: an eighth of the slots (a fuller table's walk is within 8× its `len`).
pub const fn open_cap(cap: usize) -> usize {
    if cap / 8 < 2 { 2 } else { cap / 8 }
}
/// The append switch's window of inserts and the from-row-0 factor over the slots.
/// MEASURED-HELDOUT (D09, held-out group corpus, 87 statements, W8): 32K..256K inserts within 0.8% (127.7..128.7 s);
/// 16K is 8% slower (139.0 s), the floor below; the shipped 64K stays (sweep file shapecheck/sweep-NEW-constants-266.tsv).
pub const ADAPT_WIN: u64 = 1 << 16;
pub const ADAPT_FACTOR: u64 = 8;
/// D09: the append switch's window, in turns of the table: a window of `ADAPT_TURNS` x its slots, so the share of the
/// window the table spends filling (no eviction is possible before it is full) is the same at every size. The reference
/// table (`T1_DEFAULT_BYTES` of 32-byte slots, 16,384) takes `ADAPT_WIN`; the held-out sweep above is flat from 2 to 16
/// turns and 8 % slower at one.
pub const ADAPT_TURNS: u64 = 4;
const _: () = assert!(ADAPT_TURNS * (T1_DEFAULT_BYTES as u64 / 32) == ADAPT_WIN, "the reference table keeps ADAPT_WIN");

/// D09: the append switch's window for a table of `cap` slots (`ADAPT_TURNS` turns, at least one `T1_MIN_SLOTS` table's).
#[inline(always)]
pub const fn adapt_win(cap: u64) -> u64 {
    let c = if cap < T1_MIN_SLOTS { T1_MIN_SLOTS } else { cap };
    ADAPT_TURNS * c
}

// SAFETY: the shared-reference readers are phase 2's (a partial read by the merge); nothing writes through `&T1`. The image
// words (`img`, an UnsafeCell) are written only by the table's own worker (laid at partial(ctx), its share), via
// `T1::img_words` (D1392 (a)).
unsafe impl Sync for T1 {}

/// A loop of bypass appends over one table (`T1::appender`); `finish` writes the counters back.
pub struct Appender {
    t: *mut T1,
    thresh: u64,
    delta: u64,
    rec: u32,
    block_bytes: u32,
    nblocks: u32,
    next_block: u32,
    first: *mut u32,
    heads: *mut u64,
    next: *mut u32,
    pool: *mut u8,
    appended: u64,
}

impl Appender {
    /// The next record's slot in partition `part`'s buffer (the caller writes every byte of it); `None` when the pool is exhausted (finish, flush, re-open). # Safety — the table is untouched since `appender`.
    #[inline(always)]
    pub unsafe fn reserve(&mut self, part: u32) -> Option<*mut u8> {
        let hp = self.heads.add(part as usize);
        let mut h = *hp;
        if h < self.thresh {
            (h, self.next_block) = take_block(self.first, self.next, self.next_block, self.nblocks, self.block_bytes, self.pool, part, h)?;
        }
        *hp = h.wrapping_add(self.delta);
        self.appended += 1;
        Some((h & HEAD_POS) as *mut u8)
    }

    /// `reserve` without the block path: `None` when `part`'s block is full or absent (`ensure` links one, outside a loop that must not call). # Safety — as `reserve`.
    #[inline(always)]
    pub unsafe fn reserve_fast(&mut self, part: u32) -> Option<*mut u8> {
        let hp = self.heads.add(part as usize);
        let h = *hp;
        if h < self.thresh {
            return None;
        }
        *hp = h.wrapping_add(self.delta);
        self.appended += 1;
        Some((h & HEAD_POS) as *mut u8)
    }

    /// A block behind `part`'s head when it has no room for a record; false when the pool is exhausted. # Safety — as `reserve`.
    pub unsafe fn ensure(&mut self, part: u32) -> bool {
        let hp = self.heads.add(part as usize);
        let h = *hp;
        if h >= self.thresh {
            return true;
        }
        match take_block(self.first, self.next, self.next_block, self.nblocks, self.block_bytes, self.pool, part, h) {
            Some((nh, nb)) => {
                *hp = nh;
                self.next_block = nb;
                true
            }
            None => false,
        }
    }

    /// `src`'s record into partition `part`'s buffer; false when the pool is exhausted (safety: as `reserve`).
    #[inline(always)]
    pub unsafe fn append(&mut self, part: u32, src: *const u8) -> bool {
        match self.reserve(part) {
            Some(dst) => {
                copy_record(src, dst, self.rec as usize);
                true
            }
            None => false,
        }
    }

    /// The counters back into the header (safety: as `append`).
    #[inline(always)]
    pub unsafe fn finish(&mut self) {
        (*self.t).next_block = self.next_block;
        (*self.t).buffered += self.appended;
        self.appended = 0;
    }
}

/// A partition's write head: bytes left in its block above `HEAD_LEFT`, the next record's address below; 0 = no block.
pub const HEAD_LEFT: u32 = 48;
pub const HEAD_POS: u64 = (1 << HEAD_LEFT) - 1;
/// D2473: the most bytes one block may hold: the bytes left sit above `HEAD_LEFT` and the head word must stay positive as an
/// `i64` (a reader takes `head as i64 <= 0` for no block), so 15 bits; a 64 KiB block packed a negative head.
pub const BLOCK_MAX: usize = (1 << (63 - HEAD_LEFT)) - 1;
const _: () = assert!(BLOCK_BYTES as usize - BLOCK_STAGGER <= BLOCK_MAX, "a full block's bytes left fit the head word");

/// A record copied by its width in whole words (a runtime-length copy is a `memcpy` call per record). # Safety — `n` bytes each, disjoint.
#[inline(always)]
pub unsafe fn copy_record(src: *const u8, dst: *mut u8, n: usize) {
    macro_rules! words {
        ($k:literal) => {
            core::ptr::write_unaligned(dst as *mut [u64; $k], core::ptr::read_unaligned(src as *const [u64; $k]))
        };
    }
    match n {
        8 => words!(1),
        16 => words!(2),
        24 => words!(3),
        32 => words!(4),
        40 => words!(5),
        48 => words!(6),
        56 => words!(7),
        64 => words!(8),
        _ => core::ptr::copy_nonoverlapping(src, dst, n),
    }
}

/// A fresh block linked after `part`'s current one (head `h`, full) or opened as its first: the new head and the next free block (by value: an appender whose
/// field address never escapes stays in the loop's registers), `None` when the pool is exhausted.
#[cold]
#[inline(never)]
unsafe fn take_block(first: *mut u32, next: *mut u32, next_block: u32, nblocks: u32, block_bytes: u32, pool: *mut u8, part: u32, h: u64) -> Option<(u64, u32)> {
    let nb = next_block;
    if nb >= nblocks {
        return None;
    }
    *next.add(nb as usize) = NO_BLOCK;
    let f = first.add(part as usize);
    if *f == NO_BLOCK {
        *f = nb;
    } else {
        let b = ((h & HEAD_POS) as usize - 1 - pool as usize) / block_bytes as usize;
        *next.add(b) = nb;
    }
    Some(((block_bytes as u64) << HEAD_LEFT | pool.add(nb as usize * block_bytes as usize) as u64, nb + 1))
}

/// One block of a partition buffer: its first record and how many it holds.
#[derive(Clone, Copy, Debug)]
pub struct Block {
    pub ptr: *const u8,
    pub recs: usize,
}

impl T1 {
    /// # Safety
    /// `p` points at `limit` writable bytes, 8-aligned, its header prefix zeroed and (with `limit ==
    /// SHARE`) armed, or a live table laid out here.
    pub unsafe fn at<'a>(p: *mut u8, layout: &RecordLayout, pbits: u8, t1_bytes: u32, limit: u32) -> &'a mut T1 {
        T1::at_sized(p, layout, pbits, t1_bytes, t1_bytes, limit)
    }

    /// `at` with room to grow (table.sized, R3-L02): the layout reserves `max_bytes` of slots (clipped to half of what the
    /// header leaves, as `at` clips), the table opens at `first_bytes` of them (a power of two, at most the reservation) and
    /// only those are zeroed; `grow` doubles it in place, zeroing each new half as it comes.
    /// # Safety
    /// As `at`.
    pub unsafe fn at_sized<'a>(p: *mut u8, layout: &RecordLayout, pbits: u8, max_bytes: u32, first_bytes: u32, limit: u32) -> &'a mut T1 {
        T1::at_reserved(p, layout, pbits, max_bytes, first_bytes, limit, 0)
    }

    /// `at_sized` in a region that is an address-space reservation of `reserve` bytes past its `limit` (the setop posture,
    /// option c': a `Budget::Pool` side, the driver's `SPILL_REGION_COMMIT` answer): the share's layout (table, pool blocks,
    /// `block_bytes`) behind chain words sized for the reservation's blocks (`T1_RESERVED`), so `grow_pool` commits the rest
    /// in place. `reserve` at or under `limit` (or too little past it for a block) lays the table as `at_sized` does.
    /// # Safety
    /// As `at`; `reserve` bytes at `p` are writable when it is past `limit`.
    pub unsafe fn at_reserved<'a>(p: *mut u8, layout: &RecordLayout, pbits: u8, max_bytes: u32, first_bytes: u32, limit: u32, reserve: u64) -> &'a mut T1 {
        T1::at_laid(p, layout, pbits, max_bytes, first_bytes, limit, reserve, false)
    }

    /// `at_reserved` in a region that started small (`laid`: the driver's `RESERVE_LAYOUT` answer): its `limit` is the
    /// committed start, so the pool's blocks are sized from the reservation, as the share would have sized them, and the
    /// pool grows into it in place (`grow_pool`); a start too small for one such block past the table lays out as
    /// `at_reserved` does.
    /// # Safety
    /// As `at_reserved`.
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn at_laid<'a>(p: *mut u8, layout: &RecordLayout, pbits: u8, max_bytes: u32, first_bytes: u32, limit: u32, reserve: u64, laid: bool) -> &'a mut T1 {
        let t = aligned_mut::<T1>(p);
        let limit = if limit == SHARE { t.limit } else { limit };
        assert!(limit != 0, "phase-1 table region without a limit");
        if t.cap == 0 {
            let reserve = (reserve.min(u32::MAX as u64) as usize).max(limit as usize);
            let parts = 1usize << pbits;
            assert!(pbits as u32 <= T1_MAX_PARTS.trailing_zeros(), "phase-1 partitions");
            let rec = layout.size() as usize;
            let meta = T1_HDR + 12 * parts;
            let avail = (limit as usize).checked_sub(meta).unwrap_or(0);
            let floor2 = |bytes: usize| 1usize << (usize::BITS - 1 - (bytes / rec).max(2).leading_zeros());
            let cap = floor2((max_bytes as usize).min(avail / 2));
            let first = floor2(first_bytes as usize).min(cap);
            // the slots' partition words, the opened-slot list, then the side words (`T1_SIDE_BYTES`: the copy-slot word, the
            // kept image bytes, the image dedupe word) before the pool
            let side = 4 * cap + 4 * open_cap(cap) + T1_SIDE_BYTES;
            if cap * rec + side + 2 * rec > avail {
                crate::limits::exceeded(format_args!("a share too small for the phase-1 table: {avail} bytes for {cap} records of {rec} bytes"));
            }
            let pool_avail = avail - cap * rec - side;
            // four blocks per chain at least (two flushed a two-key text group-by at P = 2,048; an eighth of the pool cost a one-key group-by's loop 15 ms); eight
            // (V4R-236) where the pool cannot give each chain BLOCK_FULL_CHAIN full blocks and the halved block keeps BLOCK_HALVED_MIN
            let full = BLOCK_BYTES as usize - BLOCK_STAGGER;
            let block_of = |pool: usize| {
                let per = if pool < BLOCK_FULL_CHAIN * parts * full && pool / (8 * parts).max(1) >= BLOCK_HALVED_MIN { 8 } else { 4 };
                (full.min(pool / (per * parts).max(1)) / rec * rec).max(rec)
            };
            let mut block_bytes = block_of(pool_avail);
            // a region that started small: its blocks are the reservation's (the start would cut them to a record or two), when
            // a few of them fit the start past the table and the chain words the reservation's blocks take
            if laid && reserve > limit as usize {
                let lay = block_of((reserve - meta).saturating_sub(cap * rec + side));
                let words = 4 * (reserve / (lay + 4)) + 8;
                if lay > block_bytes && pool_avail >= words + 4 * (lay + 4) {
                    block_bytes = lay;
                }
            }
            debug_assert!(block_bytes <= BLOCK_MAX, "D203: one record per block at least (a small share); fewer blocks than chains flush when spent");
            let mut nblocks = pool_avail / (block_bytes + 4);
            // the table's 8-alignment after the chain words may take the 4 bytes the last block's word was budgeted (an odd count over an odd share: a text min() group-by at W = 8)
            if nblocks * (block_bytes + 4) + ((meta + 4 * nblocks).wrapping_neg() & 7) > pool_avail {
                nblocks -= 1;
            }
            debug_assert!(nblocks >= 1, "the table takes at most half of what the header leaves");
            // option c': chain words for the reservation's blocks (`reserved_blocks`), the share's blocks behind them; the
            // share keeps what fits it after the longer chain-word array (none: laid as an unreserved table)
            let (mut nmax, mut flags) = (nblocks, 0);
            if reserve > limit as usize {
                let fixed = cap * rec + side;
                // the count the chain words' length gives back (`reserved_blocks`: the table's 8-alignment may add a word)
                let words = |n: usize| (((meta + 4 * n + 7) & !7) - meta) / 4;
                let fits = |n: usize| meta + 4 * words(n) + 8 + fixed + words(n) * block_bytes <= reserve;
                let mut n = (reserve - meta - fixed - 8) / (block_bytes + 4);
                while n > nblocks && !fits(n) {
                    n -= 1;
                }
                // the count must fit the reservation itself: the loop may stop at `nblocks` unfit, and the chain words'
                // 8-alignment (`words`) may then lift it past `nblocks` (a reservation within a block of the limit: a
                // laid region parked near its reservation's end; the layout's assert below caught it)
                let fit = n > nblocks && fits(n);
                let n = words(n);
                let start = ((meta + 4 * n + 7) & !7) + fixed;
                let n0 = (limit as usize).saturating_sub(start) / block_bytes;
                if fit && n > nblocks && n0 >= 1 && p as usize + reserve <= HEAD_POS as usize {
                    (nmax, nblocks, flags) = (n, n0.min(nblocks), T1_RESERVED);
                }
            }
            if laid {
                flags |= T1_LAID;
            }
            let end = if flags & T1_RESERVED != 0 { reserve } else { limit as usize };
            assert!(p as usize + end <= HEAD_POS as usize, "the pool's addresses fit the head word");
            let table_off = (meta + 4 * nmax + 7) & !7;
            let hash_off = table_off + cap * rec;
            let pool_off = hash_off + side;
            assert!(pool_off + nblocks * block_bytes <= limit as usize, "phase-1 layout");
            assert!(pool_off + nmax * block_bytes <= end, "phase-1 reserved layout");
            let max_bits = if first < cap { cap.trailing_zeros() as u8 } else { 0 };
            *t = T1 { cap: first as u32, len: 0, limit, pbits, flags, max_bits, kind: TABLE_T1, layout: *layout, block_bytes: block_bytes as u32, nblocks: nblocks as u32, next_block: 0, table_off: table_off as u32, pool_off: pool_off as u32, hash_off: hash_off as u32, opened: 0, block_recs: (block_bytes / rec) as u32, buffered: 0, inserts: 0, evictions: 0, flushes: 0, win_at: 0, win_ev: 0, img: core::cell::UnsafeCell::new([0; 3]), cell: core::cell::UnsafeCell::new([]) };
            debug_assert!(flags == 0 || t.reserved_blocks() as usize >= nmax, "the chain words cover the reservation");
            // the region beyond the header may be dirty: the heads, the chain words and the table are written here; the pool is written before read
            core::ptr::write_bytes(p.add(T1_HDR), 0, 8 * parts);
            core::ptr::write_bytes(p.add(T1_HDR + 8 * parts), 0xff, 4 * parts + 4 * nblocks);
            core::ptr::write_bytes(p.add(table_off), 0, first * rec);
            // D3312: no copy slots named, no bytes kept, no dedupe cache and no sparse evictions counted, as the image words
            // start empty
            core::ptr::write_bytes(p.add(pool_off - T1_SIDE_BYTES), 0, T1_SIDE_BYTES);
        }
        assert!(t.kind == TABLE_T1 && t.layout.same(layout) && t.pbits == pbits, "phase-1 table shape");
        t
    }

    /// D1392: the image arena's three words (`img`: the `(cur, end, chunk)` triple the image
    /// copies bump) as a raw pointer from the table's base, never through a reference.
    /// # Safety
    /// `this` is a live table, laid out by `T1::at` (kind `TABLE_T1`: no other table kind carries the words).
    #[inline(always)] // a call per image copy otherwise: fast-profile has no LTO across crates
    pub unsafe fn img_words(this: *mut T1) -> *mut u64 {
        debug_assert_eq!(core::ptr::addr_of!((*this).kind).read(), TABLE_T1, "D1392: image words on a table that is not a T1");
        (*core::ptr::addr_of_mut!((*this).img)).get() as *mut u64
    }

    /// D1392 (a): the image words a copy host bumps. A claim-lived `AggState`'s three words name the table's by reference
    /// (`[T1::img_words, the table, IMG_BY_REF]`, set by `agg_open`), so the lowered programs keep addressing the state's words
    /// (no extra load per row); any other cursor (a streamed group's own, a code copy's) is its own words. A chunk size
    /// is at most 32 MiB, never `IMG_BY_REF`.
    /// # Safety
    /// `w` is null or three live words.
    #[inline(always)] // a call per image copy otherwise: fast-profile has no LTO across crates
    pub unsafe fn img_resolve(w: *mut u64) -> *mut u64 {
        if !w.is_null() && w.add(2).read() == IMG_BY_REF { w.read() as usize as *mut u64 } else { w }
    }

    /// D3312: the table a claim's state words `w` name by reference (`[T1::img_words, the table, IMG_BY_REF]`, kernels
    /// `agg_open`), null for any other cursor (a streamed group's own words, a pass's) or a reference without its table:
    /// only a table's copies dedupe.
    /// # Safety
    /// `w` is null or three live words.
    #[inline(always)]
    pub unsafe fn img_table(w: *mut u64) -> *mut T1 {
        if !w.is_null() && w.add(2).read() == IMG_BY_REF { w.add(1).read() as usize as *mut T1 } else { core::ptr::null_mut() }
    }

    /// D3312: the dedupe word (`T1_DEDUPE_WORD`), laid zero with the table.
    #[inline(always)]
    unsafe fn dedupe_word(this: *mut T1) -> *mut u64 {
        (this as *mut u8).add((*this).pool_off as usize - T1_DEDUPE_WORD) as *mut u64
    }

    /// D3312: the copy-slot word (`IMG_SLOTS_KNOWN`; kernels `agg_copies`), laid zero with the table.
    /// # Safety
    /// `this` is a live table laid out by `at_reserved`.
    #[inline(always)]
    pub unsafe fn img_slots(this: *mut T1) -> *mut u64 {
        (this as *mut u8).add((*this).pool_off as usize - T1_SIDE_BYTES) as *mut u64
    }

    /// D3312: the image bytes the table's last reclaim kept (kernels table/img_reclaim.rs), laid zero with the table.
    /// # Safety
    /// As `img_slots`.
    #[inline(always)]
    pub unsafe fn img_kept(this: *mut T1) -> *mut u64 {
        (this as *mut u8).add((*this).pool_off as usize - T1_SIDE_BYTES + 8) as *mut u64
    }

    /// D3312: an armed dedupe cache emptied (a reclaim moved the images it names; the count of a cache not armed stays).
    /// # Safety
    /// As `img_slots`.
    pub unsafe fn dedupe_clear(this: *mut T1) {
        let v = *T1::dedupe_word(this);
        if v != 0 && v & 1 == 0 {
            core::ptr::write_bytes(v as usize as *mut u8, 0, 2 * 8 * T1_DEDUPE_ENTRIES);
        }
    }

    /// D3312: the table's image dedupe cache (`T1_DEDUPE_ENTRIES` pairs), null while it is not armed. A table under a
    /// quarter full whose home pairs two other keys hold evicts on every turn of an input that cycles those keys (a
    /// spilled join's rows of one hot key, PostgreSQL's batches), and each re-open copied its key's image again into the
    /// arena, pinned to the statement's end: 6.75 M copies, 64 MiB past a 1 MB work_mem for 300 groups. Once its sparse
    /// evictions since its last flush reach `T1_DEDUPE_ARM`, the cache is pinned (`pin(bytes)`, the arena's own pins, so it
    /// lives as long as the images it names) and a copy of bytes already in the arena takes that image instead. A refused
    /// pin leaves the copies as they were and counts again.
    /// # Safety
    /// `this` is a live table laid out by `at_reserved`; `pin` returns null or that many writable bytes.
    pub unsafe fn img_cache(this: *mut T1, pin: impl FnOnce(u64) -> *mut u8) -> *mut u64 {
        let w = T1::dedupe_word(this);
        let v = *w;
        if v != 0 && v & 1 == 0 {
            return v as usize as *mut u64;
        }
        if v >> 1 < T1_DEDUPE_ARM {
            return core::ptr::null_mut();
        }
        let p = pin((2 * 8 * T1_DEDUPE_ENTRIES) as u64);
        if p.is_null() {
            *w = 0;
            return core::ptr::null_mut();
        }
        core::ptr::write_bytes(p, 0, 2 * 8 * T1_DEDUPE_ENTRIES);
        *w = p as u64;
        p as *mut u64
    }

    /// D3312: a flush starts the sparse evictions' count again (an armed cache stays: its images stay pinned).
    /// # Safety
    /// As `img_cache`.
    #[inline(always)]
    pub unsafe fn dedupe_flushed(this: *mut T1) {
        let w = T1::dedupe_word(this);
        if *w & 1 != 0 {
            *w = 0;
        }
    }

    /// D3312: the hash an image is cached by: `head` (a varlena's header word, 0 for a fixed image) and its bytes.
    #[inline]
    pub fn img_hash(head: u32, bytes: &[u8]) -> u64 {
        let mut h = crate::hash::PHI64 ^ ((head as u64) << 32 | bytes.len() as u64);
        let mut c = bytes.chunks_exact(8);
        for w in &mut c {
            h = (h ^ u64::from_le_bytes([w[0], w[1], w[2], w[3], w[4], w[5], w[6], w[7]])).wrapping_mul(crate::hash::PHI64);
            h ^= h >> 29;
        }
        let mut t = [0u8; 8];
        t[..c.remainder().len()].copy_from_slice(c.remainder());
        h = (h ^ u64::from_le_bytes(t)).wrapping_mul(crate::hash::PHI64);
        h ^ h >> 32
    }

    /// D3312: the cached image of `len` bytes for which `same` holds, in hash `h`'s set; else the entry the copy is to
    /// fill (`img_note`): the set's empty one, else the one `h`'s top bit picks.
    /// # Safety
    /// `cache` is an armed cache (`img_cache`); `same` reads at most `len` bytes of the address it is given.
    #[inline]
    pub unsafe fn img_find(cache: *mut u64, h: u64, len: usize, same: impl Fn(*const u8) -> bool) -> Result<*mut u8, usize> {
        let set = (h as usize & (T1_DEDUPE_ENTRIES / 2 - 1)) * 2;
        for e in [set, set + 1] {
            let (a, n) = (*cache.add(2 * e), *cache.add(2 * e + 1));
            if a != 0 && n == len as u64 && same(a as usize as *const u8) {
                return Ok(a as usize as *mut u8);
            }
        }
        Err(if *cache.add(2 * set) == 0 { set } else if *cache.add(2 * set + 2) == 0 { set + 1 } else { set + (h >> 63) as usize })
    }

    /// D3312: entry `e` (`img_find`'s miss) names the image of `len` bytes copied at `at`.
    /// # Safety
    /// As `img_find`; `at` holds `len` bytes pinned as long as the cache.
    #[inline]
    pub unsafe fn img_note(cache: *mut u64, e: usize, at: *const u8, len: usize) {
        (*cache.add(2 * e), *cache.add(2 * e + 1)) = (at as usize as u64, len as u64);
    }

    /// # Safety
    /// `p` points at a table `at` initialised.
    pub unsafe fn view<'a>(p: *const u8) -> &'a T1 {
        aligned_ref::<T1>(p)
    }

    /// The phase-1 table a region holds, when the region is a table header or longer and its header names a T1.
    pub fn of(region: &[u8]) -> Option<&T1> {
        // SAFETY: `region` holds a table header; a T1 header names its shape, laid by `at`.
        (region.len() >= HASHAGG_HDR && unsafe { table_kind(region.as_ptr()) } == TABLE_T1).then(|| unsafe { T1::view(region.as_ptr()) })
    }

    pub const fn parts(&self) -> usize {
        1 << self.pbits
    }

    fn base(&self) -> *mut u8 {
        self as *const T1 as *mut u8
    }

    fn heads(&self) -> *mut u64 {
        unsafe { self.base().add(T1_HDR) as *mut u64 }
    }

    fn first(&self) -> *mut u32 {
        unsafe { self.heads().add(self.parts()) as *mut u32 }
    }

    fn next(&self) -> *mut u32 {
        unsafe { self.first().add(self.parts()) }
    }

    #[inline(always)]
    pub fn slot_ptr(&self, i: usize) -> *mut u8 {
        unsafe { self.base().add(self.table_off as usize + i * self.layout.size() as usize) }
    }

    /// Per slot: its occupant's partition (`u32[max_cap]`), then the opened-slot list.
    #[inline(always)]
    fn parts_of(&self) -> *mut u32 {
        unsafe { self.base().add(self.hash_off as usize) as *mut u32 }
    }

    #[inline(always)]
    fn opened_list(&self) -> *mut u32 {
        unsafe { self.parts_of().add(self.max_cap()) }
    }

    /// Option c': the pool blocks a `T1_RESERVED` table's chain words cover (`nblocks` grows in place up to them,
    /// `grow_pool`); an unreserved table's `nblocks`.
    pub const fn reserved_blocks(&self) -> u32 {
        if self.flags & T1_RESERVED == 0 {
            return self.nblocks;
        }
        let meta = T1_HDR + 12 * self.parts();
        ((self.table_off as usize - meta) / 4) as u32
    }

    /// Option c': the bytes `grow_pool` would commit next: the pool doubled (at most to `reserved_blocks`), 0 when the
    /// table is not `T1_RESERVED` or its reservation is spent.
    pub const fn pool_growth(&self) -> u64 {
        let room = self.reserved_blocks().saturating_sub(self.nblocks);
        let more = if self.nblocks < room { self.nblocks } else { room };
        more as u64 * self.block_bytes as u64
    }

    /// Option c': `bytes` more of a `T1_RESERVED` table's pool committed IN PLACE (`pool_growth`'s, which the driver drew:
    /// `SPILL_REGION_COMMIT`): `nblocks` and `limit` grow, nothing laid moves (the table, the heads' block addresses, the
    /// chain words: a block's word is written when it is taken, `take_block`). False (nothing changed) past the reservation.
    /// # Safety
    /// `this` is a live table, no appender open; the bytes behind its committed pool are the reservation's.
    pub unsafe fn grow_pool(this: *mut T1, bytes: u64) -> bool {
        let bb = (*this).block_bytes as u64;
        let more = bytes / bb;
        if (*this).flags & T1_RESERVED == 0 || more == 0 || (*this).nblocks as u64 + more > (*this).reserved_blocks() as u64 {
            return false;
        }
        (*this).nblocks += more as u32;
        (*this).limit = (*this).limit.saturating_add((more * bb) as u32);
        true
    }

    /// C5 (revoke's give-back): `grow_pool`'s reverse, `bytes` of a `T1_RESERVED` table's pool given up, its records all
    /// written (`reset_pool`: nothing buffered, no block taken, the table drained) and no appender open: `nblocks` and
    /// `limit` shrink by the blocks they hold, and the driver gives their pages back. False (nothing changed) when the table
    /// holds anything, the bytes are not whole blocks of the pool's growth, or no block would be left.
    /// # Safety
    /// `this` is a live table, no appender open.
    pub unsafe fn shrink_pool(this: *mut T1, bytes: u64) -> bool {
        let bb = (*this).block_bytes as u64;
        if (*this).flags & T1_RESERVED == 0 || bb == 0 || bytes == 0 || bytes % bb != 0 || (*this).buffered != 0 || (*this).next_block != 0 || (*this).len != 0 {
            return false;
        }
        let less = bytes / bb;
        if less >= (*this).nblocks as u64 || bytes > (*this).limit as u64 {
            return false;
        }
        (*this).nblocks -= less as u32;
        (*this).limit -= bytes as u32;
        true
    }

    /// The slots the layout reserves: `cap` grows in place up to them (`grow`).
    #[inline(always)]
    pub const fn max_cap(&self) -> usize {
        if self.max_bits == 0 { self.cap as usize } else { 1 << self.max_bits }
    }

    /// Room to double, and past the sizing's fill (a group per `ADAPT_FACTOR` slots, `t1_bytes`) by the keys the table
    /// opened (`inserts`, an evicted key's reopen counted again): the caller grows on a full home pair instead of
    /// evicting (`grow`). D4942: by the keys it holds (`len`), three keys sharing a home pair kept one of them out, so
    /// a table opened at exactly its groups' fill never reached it; each row of the pair's keys evicted another, and
    /// once the evictions passed 95 % of the inserts `observe` switched the claim to the bypass (32 groups in 256
    /// slots: every row appended and compacted, up to 2.4x the presized table's time). A recurring collision now
    /// doubles the table once the opens reach the fill; a collision before them still evicts.
    #[inline(always)]
    pub const fn can_grow(&self) -> bool {
        (self.cap as usize) < self.max_cap() && ADAPT_FACTOR * self.inserts >= self.cap as u64
    }

    /// The route of `keys` (full hash `full`): `full` unless the layout is `partitioned_by` fewer keys.
    #[inline(always)]
    pub fn route(&self, keys: &[u64], full: KeyHash) -> KeyHash {
        let pk = self.layout.partition_keys();
        if pk == keys.len() { full } else { KeyHash::of(&keys[..pk]) }
    }

    /// The partition of `keys` (full hash `full`).
    #[inline(always)]
    pub fn part_of(&self, keys: &[u64], full: KeyHash) -> u32 {
        self.route(keys, full).part(self.pbits as u32)
    }

    #[inline(always)]
    pub fn prefetch_home(&self, h: KeyHash) {
        let mask = self.cap as usize - 1;
        let home = self.home(h);
        prefetch_read(self.slot_ptr(home));
        prefetch_read(self.slot_ptr((home + 1) & mask));
    }

    #[inline(always)]
    pub fn prefetch_append(&self, part: u32) {
        let h = unsafe { *self.heads().add(part as usize) };
        prefetch_read((h & HEAD_POS) as *const u8);
    }

    #[inline(always)]
    fn block_ptr(&self, b: u32) -> *mut u8 {
        unsafe { self.base().add(self.pool_off as usize + b as usize * self.block_bytes as usize) }
    }

    #[inline(always)]
    pub fn record(&self, i: usize) -> Rec<'_> {
        debug_assert!(i < self.cap as usize);
        unsafe { Rec::at(self.slot_ptr(i), &self.layout) }
    }

    /// The table's live records (not the buffers').
    pub fn records(&self) -> impl Iterator<Item = Rec<'_>> + '_ {
        (0..self.cap as usize).map(|i| self.record(i)).filter(|r| r.used())
    }

    /// The home slot of `h`: a producer table holds every partition, so its homes consumed no route bit (`Depth::TOP`).
    #[inline(always)]
    pub const fn home(&self, h: KeyHash) -> usize {
        h.home(self.cap.trailing_zeros(), Depth::TOP)
    }

    /// Found at the home or the next slot, or opened in the first empty one; `None`: both hold
    /// other keys (the caller evicts the home's occupant, `evict`, then `open_at`).
    /// # Safety
    /// `this` is a live table (a raw pointer: the flush host rewrites the pool between calls).
    #[inline(always)]
    pub unsafe fn probe(this: *mut T1, keys: &[u64], h: KeyHash) -> Option<*mut u8> {
        let sentinel = (*this).layout.sentinel_off() as usize;
        let mask = (*this).cap as usize - 1;
        let home = (*this).home(h);
        let p0 = (*this).slot_ptr(home);
        if *(p0.add(sentinel) as *const u64) == 0 {
            (*this).len += 1;
            (*this).inserts += 1;
            *(*this).parts_of().add(home) = (*this).part_of(keys, h);
            T1::note_open(this, home);
            Rec::at(p0, &(*this).layout).open(keys);
            return Some(p0);
        }
        if keys_eq(p0 as *const u64, keys) {
            return Some(p0);
        }
        let p1 = (*this).slot_ptr((home + 1) & mask);
        if *(p1.add(sentinel) as *const u64) == 0 {
            (*this).len += 1;
            (*this).inserts += 1;
            *(*this).parts_of().add((home + 1) & mask) = (*this).part_of(keys, h);
            T1::note_open(this, (home + 1) & mask);
            Rec::at(p1, &(*this).layout).open(keys);
            return Some(p1);
        }
        if keys_eq(p1 as *const u64, keys) {
            return Some(p1);
        }
        None
    }

    /// Slot `i` opened: listed for the drain (an emptied slot is skipped there), `OPENED_OVER` once the list is full.
    #[inline(always)]
    unsafe fn note_open(this: *mut T1, i: usize) {
        let n = (*this).opened as usize;
        if n < open_cap((*this).cap as usize) {
            *(*this).opened_list().add(n) = i as u32;
            (*this).opened = n as u32 + 1;
        } else {
            (*this).opened = OPENED_OVER;
        }
    }

    /// Doubled in place (table.sized, R3-L02; `can_grow` first): the new half zeroed, the only zeroing past the open, and
    /// every live record moved into its home pair under the new `cap`. Homes are the hash's top bits (`KeyHash::home`), so
    /// old home `o` becomes `2o` or `2o + 1`: a descending walk moves each record to its new home, else the slot after, never
    /// over a record still to move. Two records of one old pair that both take `2o + 1` can find `2o + 2` taken: the later is
    /// evicted to its partition, as a probe's collision evicts. A dry run counts those first; false (nothing changed) when
    /// the pool's free blocks may not hold them, and the caller evicts as before. The opened list is rebuilt.
    /// # Safety
    /// `this` is a live table.
    pub unsafe fn grow(this: *mut T1) -> bool {
        let (old, rec) = ((*this).cap as usize, (*this).layout.size() as usize);
        if old >= (*this).max_cap() {
            return false;
        }
        // the dry run's occupancy of the 2 x old slots: a bitmap in `parts_of`'s unused upper half (4 x old bytes)
        let bits = (*this).parts_of().add(old) as *mut u64;
        core::ptr::write_bytes(bits, 0, (2 * old).div_ceil(64));
        for i in (0..old).filter(|&i| (*this).record(i).used()) {
            *bits.add(i / 64) |= 1 << (i % 64);
        }
        // an eviction's partition takes at most a block per `block_recs` of them, plus its first
        let evict = T1::rehome(this, old, Some(bits));
        if evict.div_ceil((*this).block_recs as usize) + evict.min((*this).parts()) > ((*this).nblocks - (*this).next_block) as usize {
            return false;
        }
        core::ptr::write_bytes((*this).slot_ptr(old), 0, old * rec);
        (*this).cap = 2 * old as u32;
        let moved = T1::rehome(this, old, None);
        debug_assert_eq!(moved, evict, "the dry run counts the grow's evictions");
        (*this).opened = 0;
        for i in 0..2 * old {
            if (*this).opened != OPENED_OVER && (*this).record(i).used() {
                T1::note_open(this, i);
            }
        }
        true
    }

    /// The grow's walk over the old slots, descending, under the doubled `cap`: a record stays when its slot is in its new
    /// pair, else moves to the pair's first free slot, else is evicted; the evictions. `dry`: only the occupancy bitmap
    /// changes (the table is untouched, `cap` still the old one).
    unsafe fn rehome(this: *mut T1, old: usize, dry: Option<*mut u64>) -> usize {
        let (mask, nk, rec, bits) = (2 * old - 1, (*this).layout.nkeys(), (*this).layout.size() as usize, (2 * old).trailing_zeros());
        let sentinel = (*this).layout.sentinel_off() as usize;
        let occ = |i: usize| match dry {
            Some(b) => *b.add(i / 64) >> (i % 64) & 1 != 0,
            None => *((*this).slot_ptr(i).add(sentinel) as *const u64) != 0,
        };
        let mark = |i: usize, on: bool| {
            if let Some(b) = dry {
                *b.add(i / 64) = if on { *b.add(i / 64) | 1 << (i % 64) } else { *b.add(i / 64) & !(1 << (i % 64)) };
            }
        };
        let mut evicted = 0;
        for s in (0..old).rev().filter(|&s| occ(s)) {
            let p = (*this).slot_ptr(s);
            let nh = hash_at(p, nk).home(bits, Depth::TOP);
            if s == nh || s == (nh + 1) & mask {
                continue;
            }
            mark(s, false);
            match [nh, (nh + 1) & mask].into_iter().find(|&j| !occ(j)) {
                Some(j) => {
                    mark(j, true);
                    if dry.is_none() {
                        copy_record(p, (*this).slot_ptr(j), rec);
                        *(*this).parts_of().add(j) = *(*this).parts_of().add(s);
                        core::ptr::write_bytes(p, 0, rec);
                    }
                }
                None => {
                    evicted += 1;
                    if dry.is_none() {
                        let ok = T1::evict(this, s);
                        debug_assert!(ok, "the dry run checked the pool");
                    }
                }
            }
        }
        evicted
    }

    /// Evict the record at `home` into its partition buffer and clear the slot; false when the pool
    /// has no block for it (flush first, then call again).
    /// # Safety
    /// `this` is a live table; slot `home` holds a record.
    #[inline(always)]
    pub unsafe fn evict(this: *mut T1, home: usize) -> bool {
        let p = (*this).slot_ptr(home);
        let part = *(*this).parts_of().add(home);
        if !T1::append_bytes(this, part, p) {
            return false;
        }
        core::ptr::write_bytes(p, 0, (*this).layout.size() as usize);
        // D3312: an eviction from a table under a quarter full (a home pair two other keys hold) counts toward the image
        // dedupe (`img_cache`); a fuller table's evictions are its keys outrunning its slots and count nothing
        if 4 * (*this).len < (*this).cap {
            let w = T1::dedupe_word(this);
            if *w & 1 != 0 || *w == 0 {
                *w = (*w | 1) + 2;
            }
        }
        (*this).len -= 1;
        (*this).evictions += 1;
        true
    }

    /// Open `keys` at the emptied slot `home`.
    /// # Safety
    /// `this` is a live table; slot `home` is empty.
    #[inline(always)]
    pub unsafe fn open_at(this: *mut T1, home: usize, keys: &[u64]) -> *mut u8 {
        T1::open_at_hashed(this, home, keys, KeyHash::of(keys))
    }

    /// `open_at` with the record's full hash from the caller's probe (no second hash of the keys).
    /// # Safety
    /// `this` is a live table; slot `home` is empty; `h == KeyHash::of(keys)`.
    #[inline(always)]
    pub unsafe fn open_at_hashed(this: *mut T1, home: usize, keys: &[u64], h: KeyHash) -> *mut u8 {
        let p = (*this).slot_ptr(home);
        *(*this).parts_of().add(home) = (*this).part_of(keys, h);
        T1::note_open(this, home);
        Rec::at(p, &(*this).layout).open(keys);
        (*this).len += 1;
        (*this).inserts += 1;
        p
    }

    /// The append path's state hoisted out of the header for a loop of appends (`Appender`).
    /// # Safety — `this` is a live table; the appender is finished before any other table call.
    #[inline(always)]
    pub unsafe fn appender(this: *mut T1) -> Appender {
        let rec = (*this).layout.size() as u64;
        Appender { t: this, thresh: rec << HEAD_LEFT, delta: rec.wrapping_sub(rec << HEAD_LEFT), rec: rec as u32, block_bytes: (*this).block_bytes, nblocks: (*this).nblocks, next_block: (*this).next_block, first: (*this).first(), heads: (*this).heads(), next: (*this).next(), pool: (*this).base().add((*this).pool_off as usize), appended: 0 }
    }

    /// `src`'s record bytes appended to buffer `part`; false when the pool is exhausted.
    /// # Safety
    /// `this` is a live table; `src` points at `layout.size()` bytes.
    #[inline(always)]
    pub unsafe fn append_bytes(this: *mut T1, part: u32, src: *const u8) -> bool {
        let Some(dst) = T1::reserve(this, part) else { return false };
        copy_record(src, dst, (*this).layout.size() as usize);
        true
    }

    /// A fresh record of `keys` opened in partition `part`'s buffer (the bypass); `None` when the pool is exhausted.
    /// # Safety
    /// `this` is a live table.
    #[inline(always)]
    pub unsafe fn append_new(this: *mut T1, part: u32, keys: &[u64]) -> Option<*mut u8> {
        let dst = T1::reserve(this, part)?;
        core::ptr::write_bytes(dst, 0, (*this).layout.size() as usize);
        Rec::at(dst, &(*this).layout).open(keys);
        Some(dst)
    }

    /// # Safety
    /// `this` is a live table.
    #[inline(always)]
    pub unsafe fn reserve_for(this: *mut T1, part: u32) -> Option<*mut u8> {
        T1::reserve(this, part)
    }

    #[inline(always)]
    unsafe fn reserve(this: *mut T1, part: u32) -> Option<*mut u8> {
        let rec = (*this).layout.size() as u64;
        let hp = (*this).heads().add(part as usize);
        let mut h = *hp;
        if h < rec << HEAD_LEFT {
            (h, (*this).next_block) = take_block((*this).first(), (*this).next(), (*this).next_block, (*this).nblocks, (*this).block_bytes, (*this).base().add((*this).pool_off as usize), part, h)?;
        }
        *hp = h.wrapping_add(rec).wrapping_sub(rec << HEAD_LEFT);
        (*this).buffered += 1;
        Some((h & HEAD_POS) as *mut u8)
    }

    /// Every table record appended to its buffer and cleared; false when the pool ran out (flush, call again).
    /// # Safety
    /// `this` is a live table.
    pub unsafe fn drain(this: *mut T1) -> bool {
        if (*this).len == 0 {
            (*this).opened = 0;
            return true;
        }
        let (cap, rec) = ((*this).cap as usize, (*this).layout.size() as usize);
        let mut listed = ((*this).opened != OPENED_OVER).then(|| core::slice::from_raw_parts_mut((*this).opened_list(), (*this).opened as usize));
        let n = match listed.as_mut() {
            Some(l) => {
                l.sort_unstable();
                l.len()
            }
            None => cap,
        };
        for k in 0..n {
            let i = match &listed {
                Some(l) => l[k] as usize,
                None => k,
            };
            let p = (*this).slot_ptr(i);
            if *(p.add((*this).layout.sentinel_off() as usize) as *const u64) == 0 {
                continue;
            }
            let part = *(*this).parts_of().add(i);
            if !T1::append_bytes(this, part, p) {
                return false;
            }
            core::ptr::write_bytes(p, 0, rec);
            (*this).len -= 1;
        }
        debug_assert_eq!((*this).len, 0);
        (*this).opened = 0;
        true
    }

    /// True once the evictions over the last `adapt_win(cap)` inserts passed 95 % (§3.1).
    #[inline(always)]
    pub fn observe(&mut self) -> bool {
        let win = adapt_win(self.cap as u64);
        if self.inserts - self.win_at < win {
            return false;
        }
        let hot = (self.evictions - self.win_ev) * 20 > win * 19;
        self.win_at = self.inserts;
        self.win_ev = self.evictions;
        hot
    }

    /// After a flush: the pool emptied, the table kept.
    pub fn reset_pool(&mut self) {
        unsafe {
            core::ptr::write_bytes(self.heads() as *mut u8, 0, 8 * self.parts());
            core::ptr::write_bytes(self.first() as *mut u8, 0xff, 4 * self.parts());
        }
        self.next_block = 0;
        self.buffered = 0;
        self.flushes += 1;
    }

    /// D2609: the records `compact` folds before it may judge the fold too poor to go on. One chain is no sample: at 64
    /// partitions a chain held 9 records of 9 keys while the pool held 5,132 of 1,000, and its stop spilled (53400 under
    /// `temp_file_limit`) a W8 fold whose groups PostgreSQL holds in hash_mem.
    pub const COMPACT_SAMPLE: u64 = 1024;

    /// D2587: the most records one partition buffer holds (`compact`'s index is sized from it).
    pub fn longest_chain(&self) -> usize {
        (0..self.parts() as u32).map(|p| self.chain(p).map(|b| b.recs).sum::<usize>()).max().unwrap_or(0)
    }

    /// D2587 (CAND-M25): the partition buffers folded by key in place, as PostgreSQL's HashAggregate keeps one state per
    /// group until its groups pass hash_mem. A key's records all ride one partition, so each chain keeps one record per
    /// key at its front, a later one folded in by `fold(into, from)` (false: kept as its own, as a buffer may hold a key
    /// twice and the merge folds them); its emptied tail blocks are dropped and the kept blocks packed down to the pool's
    /// start in pool order (a block's new index is its rank, never above its own), so the freed blocks are taken again.
    /// `index`: scratch, a power of two at least twice `longest_chain`; `map`: scratch, `nblocks` long. The records held.
    /// # Safety
    /// `this` is a live table with no appender open; `fold` writes `into` only.
    pub unsafe fn compact(this: *mut T1, index: &mut [u32], map: &mut [u32], fold: &mut dyn FnMut(*mut u8, *const u8) -> bool) -> u64 {
        let (rec, nk, br, parts) = ((*this).layout.size() as usize, (*this).layout.nkeys(), (*this).block_recs as usize, (*this).parts());
        let (first, next, heads, bb) = ((*this).first(), (*this).next(), (*this).heads(), (*this).block_bytes as usize);
        let (mask, ibits, depth) = (index.len() - 1, index.len().trailing_zeros(), Depth::part((*this).pbits as u32).unwrap_or(Depth::TOP));
        debug_assert!(index.len().is_power_of_two() && index.len() >= 2 * (*this).longest_chain() && map.len() >= (*this).nblocks as usize);
        debug_assert!(((*this).nblocks as usize).saturating_mul(br) < u32::MAX as usize, "a record's place fits an index word");
        let pool = (*this).base().add((*this).pool_off as usize) as usize;
        let at = |w: usize| (*this).block_ptr((w / br) as u32).add(w % br * rec);
        map.fill(NO_BLOCK);
        let (mut held, mut sampled, mut s_read, mut s_held) = (0u64, false, 0u64, 0u64);
        for part in 0..parts {
            let f = *first.add(part);
            if f == NO_BLOCK {
                continue;
            }
            if sampled {
                // the sample folded too little: the rest kept as they are (their blocks still packed below)
                for b in (*this).chain(part as u32) {
                    held += b.recs as u64;
                    map[(b.ptr as usize - pool) / bb] = 0;
                }
                continue;
            }
            index.fill(0);
            let (mut rb, mut wb, mut wi, mut read, start) = (f, f, 0usize, 0usize, held);
            while rb != NO_BLOCK {
                let nb = *next.add(rb as usize);
                let recs = if nb == NO_BLOCK { ((*heads.add(part) & HEAD_POS) as usize - (*this).block_ptr(rb) as usize) / rec } else { br };
                read += recs;
                for ri in 0..recs {
                    let p = (*this).block_ptr(rb).add(ri * rec);
                    let keys = core::slice::from_raw_parts(p as *const u64, nk);
                    let mut i = KeyHash::of(keys).home(ibits, depth);
                    let mut folded = false;
                    while index[i] != 0 {
                        let q = at(index[i] as usize - 1);
                        if keys_eq(q as *const u64, keys) {
                            folded = fold(q, p);
                            break;
                        }
                        i = (i + 1) & mask;
                    }
                    if folded {
                        continue;
                    }
                    if wi == br {
                        (wb, wi) = (*next.add(wb as usize), 0);
                    }
                    let dst = (*this).block_ptr(wb).add(wi * rec);
                    if dst != p {
                        copy_record(p, dst, rec);
                    }
                    if index[i] == 0 {
                        index[i] = (wb as usize * br + wi + 1) as u32;
                    }
                    (wi, held) = (wi + 1, held + 1);
                }
                map[rb as usize] = 0;
                rb = nb;
            }
            if wi == 0 {
                // a chain whose blocks held no record (a block taken, none written yet): emptied
                let mut b = f;
                while b != NO_BLOCK {
                    map[b as usize] = NO_BLOCK;
                    b = *next.add(b as usize);
                }
                (*first.add(part), *heads.add(part)) = (NO_BLOCK, 0);
                continue;
            }
            let mut drop = *next.add(wb as usize);
            while drop != NO_BLOCK {
                map[drop as usize] = NO_BLOCK;
                drop = *next.add(drop as usize);
            }
            *next.add(wb as usize) = NO_BLOCK;
            *heads.add(part) = ((bb - wi * rec) as u64) << HEAD_LEFT | ((*this).block_ptr(wb) as usize + wi * rec) as u64;
            // a key's partition is its hash's top bits, so the chains sample the keys alike: once the chains folded so far
            // read `COMPACT_SAMPLE` records, a fold keeping three quarters of them frees too little to stop a spill, and the
            // walk over the other chains is not paid
            (s_read, s_held) = (s_read + read as u64, s_held + (held - start));
            sampled = s_read >= T1::COMPACT_SAMPLE && 4 * s_held > 3 * s_read;
        }
        ((*this).next_block, (*this).buffered) = (T1::pack_blocks(this, map), held);
        held
    }

    /// spillopt #5 (hybrid T1): after its host wrote the chains of the partitions `out` names (bit `part % 64` of word
    /// `part / 64`), those chains dropped and the other chains' blocks moved below the kept count (`fill_blocks`), so the
    /// kernel's appends go on into the freed blocks while the kept partitions stay in memory for the owner phase. Counted
    /// as a flush. The records still held (`buffered`).
    /// # Safety
    /// `this` is a live table with no appender open; `out` covers `parts()` bits; `map`: scratch, `nblocks` long. Its
    /// slot table may still hold records (a drain the full pool stopped, `scatter_t1_destaging`): they live in the
    /// slots, not in the blocks, and the drain goes on into the freed blocks.
    pub unsafe fn destage(this: *mut T1, out: &[u64], map: &mut [u32]) -> u64 {
        let (parts, first, heads, bb) = ((*this).parts(), (*this).first(), (*this).heads(), (*this).block_bytes as usize);
        debug_assert!(out.len() * 64 >= parts && map.len() >= (*this).nblocks as usize);
        let pool = (*this).base().add((*this).pool_off as usize) as usize;
        map.fill(NO_BLOCK);
        let mut held = 0u64;
        for part in 0..parts {
            if *first.add(part) == NO_BLOCK {
                continue;
            }
            if out[part / 64] >> (part % 64) & 1 != 0 {
                (*first.add(part), *heads.add(part)) = (NO_BLOCK, 0);
                continue;
            }
            for b in (*this).chain(part as u32) {
                held += b.recs as u64;
                map[(b.ptr as usize - pool) / bb] = 0;
            }
        }
        ((*this).next_block, (*this).buffered) = (T1::fill_blocks(this, map), held);
        (*this).flushes += 1;
        held
    }

    /// `destage`'s end: the kept blocks (`map` 0; `NO_BLOCK`: freed) below the kept count stay where they are and the ones
    /// at or past it move into the freed blocks below it, so only those are copied (`pack_blocks` keeps pool order and
    /// moves every kept block past the first freed one: half the pool per flush); the chains' links, firsts and heads
    /// re-pointed. The blocks used.
    /// # Safety
    /// `this` is a live table whose chains and heads name only blocks `map` keeps.
    unsafe fn fill_blocks(this: *mut T1, map: &mut [u32]) -> u32 {
        let (parts, first, next, heads, bb) = ((*this).parts(), (*this).first(), (*this).next(), (*this).heads(), (*this).block_bytes as usize);
        let pool = (*this).base().add((*this).pool_off as usize) as usize;
        let n = (*this).next_block as usize;
        let kept = map[..n].iter().filter(|&&m| m != NO_BLOCK).count();
        // the stayers keep their index; the freed blocks below the kept count stay NO_BLOCK, the movers' places in order
        for m in map[..kept].iter_mut().enumerate().filter(|(_, m)| **m != NO_BLOCK) {
            *m.1 = m.0 as u32;
        }
        let mut hole = 0usize;
        for b in kept..n {
            if map[b] == NO_BLOCK {
                continue;
            }
            while map[hole] != NO_BLOCK {
                hole += 1;
            }
            debug_assert!(hole < kept, "a block past the kept count has a freed block below it");
            map[b] = hole as u32;
            core::ptr::copy_nonoverlapping((*this).block_ptr(b as u32), (*this).block_ptr(hole as u32), bb);
            hole += 1;
        }
        // each kept block's link at its place: a stayer's read and written in place, a mover's read at its old index (never
        // written: places are below the kept count) and written at a freed block's (never read: freed blocks are skipped)
        for b in 0..n {
            let nb = map[b];
            if nb == NO_BLOCK {
                continue;
            }
            let link = *next.add(b);
            *next.add(nb as usize) = if link == NO_BLOCK { NO_BLOCK } else { map[link as usize] };
        }
        for part in 0..parts {
            let f = *first.add(part);
            if f == NO_BLOCK {
                continue;
            }
            *first.add(part) = map[f as usize];
            let h = *heads.add(part);
            let last = ((h & HEAD_POS) as usize - 1 - pool) / bb;
            *heads.add(part) = h - ((last - map[last] as usize) * bb) as u64;
        }
        kept as u32
    }

    /// WC-02: the group walk's held records whose key cell `k` lies past `bound` dropped from the partition buffers of a
    /// drained table (`len` 0: a claim's open), the kept ones packed to the front of their chains in append order, the emptied tail
    /// blocks freed and the kept blocks packed down (`pack_blocks`, as `compact` does). `bound` is the walk's
    /// `nth_smallest_key_held`: at least `n` distinct groups at or under it stay held, and the bound only falls within a
    /// section, so a dropped group is never among the `n` smallest (walk_skip already drops its later rows). Without it
    /// the first claim's records, nearly all past the bound after it, stayed held, and the bound's scan at every claim
    /// grew with them. `map`: scratch, `nblocks` long. The records held.
    /// # Safety
    /// `this` is a live drained table (`len` 0) with no appender open and no record appended since its last count.
    pub unsafe fn prune_held_above(this: *mut T1, k: usize, bound: i64, map: &mut [u32]) -> u64 {
        T1::prune_held_above_by(this, k, bound, map, 0)
    }

    /// GP-1b: `prune_held_above` with each key cell XOR `flip` compared to `bound` (a descending walk's NOT of the key).
    /// # Safety
    /// As `prune_held_above`.
    pub unsafe fn prune_held_above_by(this: *mut T1, k: usize, bound: i64, map: &mut [u32], flip: i64) -> u64 {
        debug_assert_eq!((*this).len, 0, "the prune reads the buffers of a drained table");
        let (rec, br, parts) = ((*this).layout.size() as usize, (*this).block_recs as usize, (*this).parts());
        let (first, next, heads, bb) = ((*this).first(), (*this).next(), (*this).heads(), (*this).block_bytes as usize);
        debug_assert!(map.len() >= (*this).nblocks as usize);
        map.fill(NO_BLOCK);
        let mut held = 0u64;
        for part in 0..parts {
            let f = *first.add(part);
            if f == NO_BLOCK {
                continue;
            }
            let (mut rb, mut wb, mut wi) = (f, f, 0usize);
            while rb != NO_BLOCK {
                let nb = *next.add(rb as usize);
                let recs = if nb == NO_BLOCK { ((*heads.add(part) & HEAD_POS) as usize - (*this).block_ptr(rb) as usize) / rec } else { br };
                for ri in 0..recs {
                    let p = (*this).block_ptr(rb).add(ri * rec);
                    if Rec::at(p, &(*this).layout).key(k) as i64 ^ flip > bound {
                        continue;
                    }
                    if wi == br {
                        (wb, wi) = (*next.add(wb as usize), 0);
                    }
                    let dst = (*this).block_ptr(wb).add(wi * rec);
                    if dst != p {
                        copy_record(p, dst, rec);
                    }
                    (wi, held) = (wi + 1, held + 1);
                }
                map[rb as usize] = 0;
                rb = nb;
            }
            if wi == 0 {
                // every record of the chain dropped: emptied
                let mut b = f;
                while b != NO_BLOCK {
                    map[b as usize] = NO_BLOCK;
                    b = *next.add(b as usize);
                }
                (*first.add(part), *heads.add(part)) = (NO_BLOCK, 0);
                continue;
            }
            let mut drop = *next.add(wb as usize);
            while drop != NO_BLOCK {
                map[drop as usize] = NO_BLOCK;
                drop = *next.add(drop as usize);
            }
            *next.add(wb as usize) = NO_BLOCK;
            *heads.add(part) = ((bb - wi * rec) as u64) << HEAD_LEFT | ((*this).block_ptr(wb) as usize + wi * rec) as u64;
        }
        ((*this).next_block, (*this).buffered) = (T1::pack_blocks(this, map), held);
        held
    }

    /// `compact`'s and `prune_held_above`'s end: the blocks `map` keeps (0; `NO_BLOCK`: freed) renumbered by rank in pool
    /// order (a block's new index is never above its own), moved down, and the chains' links, firsts and heads re-pointed.
    /// The blocks used.
    /// # Safety
    /// `this` is a live table whose chains and heads name only blocks `map` keeps.
    unsafe fn pack_blocks(this: *mut T1, map: &mut [u32]) -> u32 {
        let (parts, first, next, heads, bb) = ((*this).parts(), (*this).first(), (*this).next(), (*this).heads(), (*this).block_bytes as usize);
        let pool = (*this).base().add((*this).pool_off as usize) as usize;
        let mut used = 0u32;
        for m in map.iter_mut().take((*this).next_block as usize).filter(|m| **m != NO_BLOCK) {
            (*m, used) = (used, used + 1);
        }
        for b in 0..(*this).next_block as usize {
            let nb = map[b];
            if nb == NO_BLOCK {
                continue;
            }
            let link = *next.add(b);
            *next.add(nb as usize) = if link == NO_BLOCK { NO_BLOCK } else { map[link as usize] };
            if nb as usize != b {
                core::ptr::copy_nonoverlapping((*this).block_ptr(b as u32), (*this).block_ptr(nb), bb);
            }
        }
        for part in 0..parts {
            let f = *first.add(part);
            if f == NO_BLOCK {
                continue;
            }
            *first.add(part) = map[f as usize];
            let h = *heads.add(part);
            let last = ((h & HEAD_POS) as usize - 1 - pool) / bb;
            *heads.add(part) = h - ((last - map[last] as usize) * bb) as u64;
        }
        used
    }

    /// Empty: the table zeroed, the pool reset.
    pub fn clear(&mut self) {
        unsafe { core::ptr::write_bytes(self.slot_ptr(0), 0, self.cap as usize * self.layout.size() as usize) };
        self.len = 0;
        self.opened = 0;
        self.reset_pool();
        self.flushes -= 1;
    }

    /// The blocks of partition `part`'s buffer in append order.
    pub fn chain(&self, part: u32) -> Chain<'_> {
        Chain { t: self, part, block: unsafe { *self.first().add(part as usize) } }
    }

    /// D2988: every partition whose buffer chain holds a record, as bit `part` of `mask` (parallel merge.rs `held_parts`, an
    /// owner phase's partitions to fold): the chains' first-block words compared 64 at a time without a branch, a chain
    /// walked only where one starts (a rescanned fold's owner phase walked every partition's chain per rescan).
    pub fn held_chains(&self, mask: &mut [u64]) {
        // SAFETY: the table lays one first-block word per partition (`first`, `parts()` of them).
        let first = unsafe { core::slice::from_raw_parts(self.first() as *const u32, self.parts()) };
        for (w, words) in first.chunks(64).enumerate() {
            let mut started = words.iter().enumerate().fold(0u64, |m, (i, &b)| m | u64::from(b != NO_BLOCK) << i);
            while started != 0 {
                let part = 64 * w + started.trailing_zeros() as usize;
                started &= started - 1;
                if self.chain(part as u32).any(|b| b.recs != 0) {
                    mask[part / 64] |= 1 << (part % 64);
                }
            }
        }
    }

    /// Records held: the table's and the buffers'.
    pub fn held(&self) -> u64 {
        self.len as u64 + self.buffered
    }

    /// The `n`-th smallest key cell `k` over the table's records (the zone walk's bound).
    pub fn nth_smallest_key(&self, k: usize, n: usize) -> Option<i64> {
        nth_smallest(n, self.len as usize, self.records().map(|r| r.key(k) as i64))
    }

    /// `nth_smallest_key_held` for any `n`: the held keys copied into the caller's scratch (at least `held()` long,
    /// so a host sizes it from the records the table already holds), sorted, and the `n`-th distinct one taken.
    /// `None` when the scratch is short or fewer than `n` distinct keys are held.
    pub fn nth_smallest_key_held_in(&self, k: usize, n: usize, scratch: &mut [i64]) -> Option<i64> {
        self.nth_smallest_key_held_in_by(k, n, scratch, 0)
    }

    /// GP-1b: `nth_smallest_key_held_in` over each key cell XOR `flip` (`!0`: the NOT of the key, a descending walk's
    /// order read as ascending; 0: the key).
    pub fn nth_smallest_key_held_in_by(&self, k: usize, n: usize, scratch: &mut [i64], flip: i64) -> Option<i64> {
        let held = usize::try_from(self.held()).ok()?;
        if n == 0 || held < n || scratch.len() < held {
            return None;
        }
        let rec = self.layout.size() as usize;
        let table = (self.len != 0).then(|| self.records()).into_iter().flatten().map(move |r| r.key(k) as i64 ^ flip);
        // SAFETY: a block holds `recs` whole records of the layout.
        let buffered = (0..self.parts() as u32).flat_map(|p| self.chain(p)).flat_map(move |b| (0..b.recs).map(move |i| unsafe { Rec::at(b.ptr.add(i * rec) as *mut u8, &self.layout) }.key(k) as i64 ^ flip));
        let mut m = 0usize;
        for (slot, key) in scratch[..held].iter_mut().zip(table.chain(buffered)) {
            *slot = key;
            m += 1;
        }
        let keys = &mut scratch[..m];
        keys.sort_unstable();
        let mut distinct = 0usize;
        for (i, &key) in keys.iter().enumerate() {
            if i == 0 || key != keys[i - 1] {
                distinct += 1;
                if distinct == n {
                    return Some(key);
                }
            }
        }
        None
    }

    /// WC-02: the group walk's bound for `n` is worth a `prune_held_above` after it: the records held outnumber
    /// `PRUNE_HELD_FACTOR` x `n` and `PRUNE_HELD_MIN` (so the prune's one pass is paid back by the bound's scans it saves at
    /// the next claims), the table is drained (its records are all in the buffers, as at a claim's open) and it is no code
    /// front's held table (`T1_HOLD`).
    pub fn prune_due(&self, n: usize) -> bool {
        self.len == 0 && self.flags & T1_HOLD == 0 && self.held() > (T1::PRUNE_HELD_FACTOR * n as u64).max(T1::PRUNE_HELD_MIN)
    }

    /// `prune_due`'s ratio of records held to the walk's `n`, and its floor.
    pub const PRUNE_HELD_FACTOR: u64 = 4;
    pub const PRUNE_HELD_MIN: u64 = 1024;

    /// The `n`-th smallest DISTINCT key cell `k` over the records held (the table's and the
    /// buffers', where a group drained at a claim's end may recur as a fresh record): the zone
    /// walk's bound after `drain_below` — at least the `n`-th smallest group's key, so sound.
    /// WC-02: the heap's members are also in a probe set (`KeySet`), so a candidate's distinct check is one probe, not a
    /// scan of the heap (that scan made the walk's per-claim bound cost grow with `n`: 92 % of a 192-group walk).
    pub fn nth_smallest_key_held(&self, k: usize, n: usize) -> Option<i64> {
        self.nth_smallest_key_held_by(k, n, 0)
    }

    /// GP-1b: `nth_smallest_key_held` over each key cell XOR `flip` (`!0`: a descending walk's NOT of the key).
    pub fn nth_smallest_key_held_by(&self, k: usize, n: usize, flip: i64) -> Option<i64> {
        if n == 0 || n > SELECT_MAX {
            return None;
        }
        let rec = self.layout.size() as usize;
        let table = (self.len != 0).then(|| self.records()).into_iter().flatten().map(move |r| r.key(k) as i64 ^ flip);
        // SAFETY: a block holds `recs` whole records of the layout.
        let buffered = (0..self.parts() as u32).flat_map(|p| self.chain(p)).flat_map(move |b| (0..b.recs).map(move |i| unsafe { Rec::at(b.ptr.add(i * rec) as *mut u8, &self.layout) }.key(k) as i64 ^ flip));
        let mut heap = [i64::MAX; SELECT_MAX];
        let mut hl = 0usize;
        let mut set = KeySet::new(n);
        for key in table.chain(buffered) {
            if (hl == n && key >= heap[0]) || set.has(key) {
                continue;
            }
            if hl < n {
                set.put(key);
                heap[hl] = key;
                hl += 1;
                let mut i = hl - 1;
                while i > 0 && heap[(i - 1) / 2] < heap[i] {
                    heap.swap(i, (i - 1) / 2);
                    i = (i - 1) / 2;
                }
            } else {
                set.take(heap[0]);
                set.put(key);
                heap[0] = key;
                let mut i = 0;
                loop {
                    let (l, r) = (2 * i + 1, 2 * i + 2);
                    let mut m = i;
                    if l < n && heap[l] > heap[m] {
                        m = l;
                    }
                    if r < n && heap[r] > heap[m] {
                        m = r;
                    }
                    if m == i {
                        break;
                    }
                    heap.swap(i, m);
                    i = m;
                }
            }
        }
        (hl == n).then_some(heap[0])
    }

    /// The zone top-k's compaction at a claim's end: the table's records whose key cell `k` is at
    /// most `bound` appended to their buffers, the rest dropped (never among the `n` smallest), the
    /// table cleared; false when the pool ran out (flush, call again).
    /// # Safety
    /// `this` is a live table.
    pub unsafe fn drain_below(this: *mut T1, k: usize, bound: i64) -> bool {
        if (*this).len == 0 {
            return true;
        }
        let (cap, rec) = ((*this).cap as usize, (*this).layout.size() as usize);
        for i in 0..cap {
            let p = (*this).slot_ptr(i);
            if *(p.add((*this).layout.sentinel_off() as usize) as *const u64) == 0 {
                continue;
            }
            if Rec::at(p, &(*this).layout).key(k) as i64 <= bound {
                let part = *(*this).parts_of().add(i);
                if !T1::append_bytes(this, part, p) {
                    return false;
                }
            }
            core::ptr::write_bytes(p, 0, rec);
            (*this).len -= 1;
        }
        debug_assert_eq!((*this).len, 0);
        true
    }

    /// The zone walk's cut between claims (lowerv2 `tail/walk.rs`): every table record whose key cell `k` is past `bound` dropped (never among the `n` smallest), the survivors kept merging, the buffers untouched; a dropped slot is a hole the two-slot probe re-opens, a group split by it merges at the owner.
    /// # Safety
    /// `this` is a live table.
    pub unsafe fn prune_above(this: *mut T1, k: usize, bound: i64) {
        if (*this).len == 0 || bound == i64::MAX {
            return;
        }
        let (cap, rec) = ((*this).cap as usize, (*this).layout.size() as usize);
        for i in 0..cap {
            let p = (*this).slot_ptr(i);
            if *(p.add((*this).layout.sentinel_off() as usize) as *const u64) == 0 {
                continue;
            }
            if Rec::at(p, &(*this).layout).key(k) as i64 > bound {
                core::ptr::write_bytes(p, 0, rec);
                (*this).len -= 1;
            }
        }
    }
}

pub struct Chain<'a> {
    t: &'a T1,
    part: u32,
    block: u32,
}

impl Iterator for Chain<'_> {
    type Item = Block;

    fn next(&mut self) -> Option<Block> {
        if self.block == NO_BLOCK {
            return None;
        }
        let b = self.block;
        let t = self.t;
        // SAFETY: `b` is a block of this partition's chain; the last block is the head's.
        let recs = unsafe {
            let next = *t.next().add(b as usize);
            self.block = next;
            if next == NO_BLOCK {
                let pos = (*t.heads().add(self.part as usize) & HEAD_POS) as usize;
                (pos - t.block_ptr(b) as usize) / t.layout.size() as usize
            } else {
                t.block_recs as usize
            }
        };
        Some(Block { ptr: t.block_ptr(b), recs })
    }
}

/// `T1::nth_smallest_key_held`'s distinct check (WC-02): the bounded heap's keys in an open-addressed set, linear probing
/// over a power of two of at least `2 n` slots (at most half full), a removal shifting its probe run back; one probe per
/// candidate where the heap scan cost `n`. A slot's key is written before its used bit is set and read only under it, so
/// the keys are never zeroed (a 16 KB memset per call cost more than the scan it saves at small `n`).
struct KeySet {
    keys: [core::mem::MaybeUninit<i64>; 2 * SELECT_MAX],
    used: [u64; 2 * SELECT_MAX / 64],
    mask: usize,
    shift: u32,
}

impl KeySet {
    fn new(n: usize) -> KeySet {
        let cap = (2 * n).next_power_of_two().clamp(64, 2 * SELECT_MAX);
        KeySet { keys: [const { core::mem::MaybeUninit::uninit() }; 2 * SELECT_MAX], used: [0; 2 * SELECT_MAX / 64], mask: cap - 1, shift: 64 - cap.trailing_zeros() }
    }
    /// splitmix64's finalizer, then the top bits: a key cell's low bits may all be zero (a multiply alone then clusters
    /// the homes and the probe runs grow long).
    fn home(&self, key: i64) -> usize {
        let mut z = key as u64;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        ((z ^ (z >> 31)) >> self.shift) as usize
    }
    fn full(&self, i: usize) -> bool {
        (self.used[i / 64] >> (i % 64)) & 1 != 0
    }
    /// The key in a used slot.
    fn at(&self, i: usize) -> i64 {
        debug_assert!(self.full(i));
        // SAFETY: a used slot's key was written (`put`, or `take`'s shift) before its bit was set.
        unsafe { self.keys[i].assume_init() }
    }
    fn slot(&self, key: i64) -> Option<usize> {
        let mut i = self.home(key);
        while self.full(i) {
            if self.at(i) == key {
                return Some(i);
            }
            i = (i + 1) & self.mask;
        }
        None
    }
    fn has(&self, key: i64) -> bool {
        self.slot(key).is_some()
    }
    /// `key` in (the caller checked it is not).
    fn put(&mut self, key: i64) {
        let mut i = self.home(key);
        while self.full(i) {
            i = (i + 1) & self.mask;
        }
        self.keys[i] = core::mem::MaybeUninit::new(key);
        self.used[i / 64] |= 1 << (i % 64);
    }
    /// `key` out, the run behind it shifted back so every key stays reachable from its home.
    fn take(&mut self, key: i64) {
        let Some(mut i) = self.slot(key) else { return };
        let mut j = i;
        loop {
            j = (j + 1) & self.mask;
            if !self.full(j) {
                break;
            }
            let h = self.home(self.at(j));
            // the key at j stays when its home lies cyclically in (i, j]
            let stays = if i <= j { i < h && h <= j } else { i < h || h <= j };
            if !stays {
                self.keys[i] = core::mem::MaybeUninit::new(self.at(j));
                i = j;
            }
        }
        self.used[i / 64] &= !(1 << (i % 64));
    }
}

/// The `n`-th smallest of `len` keys (a bounded heap, `n ≤ SELECT_MAX`).
fn nth_smallest(n: usize, len: usize, keys: impl Iterator<Item = i64>) -> Option<i64> {
    if n == 0 || n > SELECT_MAX || len < n {
        return None;
    }
    let mut heap = [i64::MIN; SELECT_MAX];
    let mut hl = 0usize;
    for key in keys {
        if hl < n {
            heap[hl] = key;
            hl += 1;
            let mut i = hl - 1;
            while i > 0 && heap[(i - 1) / 2] < heap[i] {
                heap.swap(i, (i - 1) / 2);
                i = (i - 1) / 2;
            }
        } else if key < heap[0] {
            heap[0] = key;
            let mut i = 0;
            loop {
                let (l, r) = (2 * i + 1, 2 * i + 2);
                let mut m = i;
                if l < n && heap[l] > heap[m] {
                    m = l;
                }
                if r < n && heap[r] > heap[m] {
                    m = r;
                }
                if m == i {
                    break;
                }
                heap.swap(i, m);
                i = m;
            }
        }
    }
    (hl == n).then_some(heap[0])
}

/// The leader region after the owner phase (§3.2, §3.4): `nowners` regions of `owner_bytes` behind
/// this 64-byte header, each its owner's result (`OwnerTop`) then its partition table space.
#[repr(C)]
pub struct Owned {
    pub nowners: u32,
    pub owner_bytes: u32,
    pub limit: u32,
    pub pad0: [u8; 3],
    pub kind: u8,
    /// The owner regions' base (0: behind this header); the one owner of a small share folds in the leader's arena.
    pub base: u64,
    pub pad1: [u64; 5],
    /// D1458: zero-sized; the owner regions behind the header are written through `owner(&self)`'s pointer while the leader's
    /// `&Owned` view is live, so `&Owned` must not be `Freeze` (Tree Borrows: a `&`-derived pointer of a `Freeze` type is
    /// read-only; LLVM: `readonly noalias`, D1358's dropped store). No size or offset change.
    pub cell: core::cell::UnsafeCell<[u8; 0]>,
}

pub const TABLE_OWNED: u8 = 6;
/// C10 2b (D3582): `Owned::pad1[0]` of a batched slot's leader region (driver mem/batch.rs): its one owner holds one batch
/// of the slot's partitions at a time, and the emit that consumes it folds the next one in its place
/// (`program::SPILL_OWNED_NEXT`); no other reader takes it (`counts`, `totals` and `key_range` answer `None`, the kernels'
/// claim gather refuses it). The mark lives in the header's pad words: no size or offset moves.
pub const OWNED_BATCHED: u64 = u64::from_le_bytes(*b"batched\0");
/// C10 2b (D3582): the emit state a batched slot's refill files at its owner's table space (kernels `EmitState`: the
/// pointer array's address, its length and the kept count), the pointer array right behind it.
pub const OWNED_EMIT_BYTES: usize = 24;
const _: () = assert!(core::mem::offset_of!(Owned, kind) == 15 && core::mem::offset_of!(Owned, base) == 16 && core::mem::offset_of!(Owned, pad1) == 24);
/// D15: the most owners a leader region carries = the driver's partial-slot capacity (`driver::SLOTS`, a fixed array
/// bound, not a machine fact). A statement's owner count is its width, which `exec::inputs::knobs::room::clamp_width`
/// sizes from the PG parallel knobs bounded by the installed pool's worker threads and then by this capacity.
pub const MAX_OWNERS: usize = 256;
pub const OWNED_HDR: usize = size_of::<Owned>();
const _: () = assert!(OWNED_HDR == 64);

/// D3024 (b): the slices a group emit over owner runs claims (`ClaimKind::OwnerRuns`, kernels table/emit.rs
/// `agg_gather_claim`): exec's runner cuts this fixed count into its claims (one claim of every slice below width 2), and
/// the kernel maps a claim's slices to runs at the call (`Owned::slice_runs`).
pub const OWNER_RUN_SLICES: u64 = 64;

impl Owned {
    /// # Safety
    /// `p` points at a leader region the owner phase finished.
    pub unsafe fn view<'a>(p: *const u8) -> &'a Owned {
        aligned_ref::<Owned>(p)
    }

    pub fn owner(&self, i: usize) -> *mut u8 {
        let base = if self.base == 0 { unsafe { (self as *const Owned as *mut u8).add(OWNED_HDR) } } else { self.base as *mut u8 };
        unsafe { base.add(i * self.owner_bytes as usize) }
    }

    /// C10 2b (D3582): a batched slot's header (`OWNED_BATCHED`).
    pub fn batched(&self) -> bool {
        self.pad1[0] == OWNED_BATCHED
    }
}

/// `OwnerTop.groups` of an owner whose reduce pruned groups unseen (parthaving's floor prune, V4R-173): no distinct count.
pub const GROUPS_UNCOUNTED: u64 = u64::MAX;

/// An owner's bounded run at the front of its region: `len` records (its text keys rendered), the table space at `table_off`.
#[repr(C)]
pub struct OwnerTop {
    pub len: u32,
    pub n: u32,
    pub table_off: u32,
    pub worst: u32,
    /// The second round's buffers (a phase-1 table inside the owner's region); 0: one round.
    pub round2_off: u32,
    /// The table slot a `reduce` resumes at after `REDUCE_FLUSH` (0 between partitions). D3111: a one-round form keeping
    /// `n > 0` records whose table offset moves with its region may name here, from its open on, the end of the state its
    /// reduces keep across partitions (hosts distinct_fold: its results, before its accumulator space), at an offset its
    /// region's size does not move: the driver's one owner (section.rs `own_all`) then carries the header and the bytes
    /// before it into a grown region and goes on at the partition that overflowed.
    pub resume: u32,
    /// `count(DISTINCT x)` alone: the owner's sum; the phase-1 records and the groups it folded (the memo's counts;
    /// `GROUPS_UNCOUNTED`: a reduce that did not see every group, and the memo keeps what it had).
    pub total: u64,
    pub groups: u64,
    pub records: u64,
    /// The count a folded record must reach to be a candidate for the run (0 until `n` are kept).
    pub thr: i64,
    /// SRC-38 slice 2 (the observed key range): the least and greatest datum word of key 0 over the groups this owner's
    /// reduce saw, when its params ask (exec owner.rs `P_OBSERVE_KEY`, set only under an `Observe` proto for one integer
    /// key); `KEY_LO_NONE` / `KEY_HI_NONE` (lo > hi) otherwise. Every other form writes the empty range at its open and
    /// never reads it.
    pub key_lo: i64,
    pub key_hi: i64,
    /// D1458: zero-sized; the run's records behind the header are written through `records(&self)`'s pointer while an
    /// `&OwnerTop` view is live, so `&OwnerTop` must not be `Freeze` (as `Owned`'s marker). No size or offset change.
    pub cell: core::cell::UnsafeCell<[u8; 0]>,
}

/// SRC-38 slice 2: 72 bytes (56 before `key_lo` / `key_hi`); every form's records, run directory and table follow at
/// `OWNERTOP_HDR` / `OWNER_RUN_OFF`, so they move 16 bytes, nothing else.
pub const OWNERTOP_HDR: usize = size_of::<OwnerTop>();
const _: () = assert!(OWNERTOP_HDR == 72);
/// `OwnerTop::key_lo` / `key_hi` of an owner that observed no key (an empty range: lo > hi).
pub const KEY_LO_NONE: i64 = i64::MAX;
pub const KEY_HI_NONE: i64 = i64::MIN;
/// The generic owner's runs form: the run directory (`(off, m)` per folded partition) at `OWNERTOP_HDR`, the records from `OWNER_RUN_OFF`.
pub const OWNER_MAX_RUNS: usize = T1_MAX_PARTS as usize;
pub const OWNER_RUN_OFF: usize = OWNERTOP_HDR + 8 * OWNER_MAX_RUNS;
/// The owner fold's order slot when it keeps no candidate list (the reduce scans the table).
pub const ORDER_NONE: u8 = 255;
/// The distinct form's accumulator record: slot 0 the sentinel count, `ACC_PAIRS` the pair count, the pair's slot `j` at `ACC_SLOT_BASE + j`.
pub const ACC_PAIRS: u8 = 1;
pub const ACC_SLOT_BASE: u8 = 2;
pub const ORDER_KEY: u8 = 254;

impl OwnerTop {
    /// # Safety
    /// `p` points at an owner region opened by the shape's `OwnerFns::open`.
    pub unsafe fn view<'a>(p: *const u8) -> &'a OwnerTop {
        aligned_ref::<OwnerTop>(p)
    }

    pub fn records(&self) -> *mut u8 {
        unsafe { (self as *const OwnerTop as *mut u8).add(OWNERTOP_HDR) }
    }
}

impl Owned {
    /// `(records, groups)` summed over the owners of `region` when it is a finished `Owned` leader region (at least an
    /// `Owned` header long, its kind byte `TABLE_OWNED`); `None` otherwise, and for a batched one (C10 2b: its owner holds
    /// one batch). An owner whose reduce pruned groups unseen (`GROUPS_UNCOUNTED`) has no count, so `groups` is then `None`.
    pub fn counts(region: &[u8]) -> Option<(u64, Option<u64>)> {
        // SAFETY: a leader region at least an `Owned` header long; its kind byte names the layout before it is viewed.
        if region.len() >= OWNED_HDR && unsafe { table_kind(region.as_ptr()) } == TABLE_OWNED && !unsafe { Owned::view(region.as_ptr()) }.batched() {
            let o = unsafe { Owned::view(region.as_ptr()) };
            Some((0..o.nowners as usize).map(|i| unsafe { OwnerTop::view(o.owner(i)) }).fold((0u64, Some(0u64)), |(r, g), t| (r + t.records, g.filter(|_| t.groups != GROUPS_UNCOUNTED).map(|g| g + t.groups))))
        } else {
            None
        }
    }

    /// D3024 (b): the owner runs `[a, b)` (the flat index: owner 0's runs, then owner 1's, ..) that slices `[lo, hi)` of
    /// `OWNER_RUN_SLICES` take over a region holding `runs` runs: the slices cut the runs evenly whatever their count, so a
    /// claim never depends on a count filed when the producer ran. `MorselRange::ALL` takes every run, `SECTION_END` none.
    pub const fn slice_runs(lo: u64, hi: u64, runs: u64) -> (u64, u64) {
        let s = OWNER_RUN_SLICES;
        let (lo, hi) = (if lo < s { lo } else { s }, if hi < s { hi } else { s });
        (lo * runs / s, hi * runs / s)
    }

    /// `counts` for the plain distinct form (`count(DISTINCT x)` alone): `(records, the owners' totals)`, the totals being
    /// its deduped pairs, the groups its phase-1 pair table holds.
    pub fn totals(region: &[u8]) -> Option<(u64, Option<u64>)> {
        // SAFETY: as `counts`.
        if region.len() >= OWNED_HDR && unsafe { table_kind(region.as_ptr()) } == TABLE_OWNED && !unsafe { Owned::view(region.as_ptr()) }.batched() {
            let o = unsafe { Owned::view(region.as_ptr()) };
            Some((0..o.nowners as usize).map(|i| unsafe { OwnerTop::view(o.owner(i)) }).fold((0, Some(0)), |(r, g), t| (r + t.records, g.map(|g: u64| g + t.total))))
        } else {
            None
        }
    }

    /// SRC-38 slice 2: the observed range `(lo, hi)` of key 0 over the owners of a finished `Owned` leader `region`
    /// (`OwnerTop::key_lo` / `key_hi`, owners that saw no key skipped); `None` when no owner observed one, or the region
    /// is no `Owned` leader.
    pub fn key_range(region: &[u8]) -> Option<(i64, i64)> {
        // SAFETY: as `counts`.
        if region.len() < OWNED_HDR || unsafe { table_kind(region.as_ptr()) } != TABLE_OWNED || unsafe { Owned::view(region.as_ptr()) }.batched() {
            return None;
        }
        let o = unsafe { Owned::view(region.as_ptr()) };
        let (lo, hi) = (0..o.nowners as usize).map(|i| unsafe { OwnerTop::view(o.owner(i)) }).fold((KEY_LO_NONE, KEY_HI_NONE), |(lo, hi), t| (lo.min(t.key_lo), hi.max(t.key_hi)));
        (lo <= hi).then_some((lo, hi))
    }
}

/// The owner phase's kernel half (the fold is the driver's): `open` lays the owner's
/// result at the front of its region and returns its bytes (< 0: a status); `reduce` offers a
/// folded partition's groups to it: the `ncand` table indexes at `cand` whose count slot
/// `order_slot` reached `OwnerTop.thr` during the fold (`MOVING` set until offered), or the
/// whole table when `ncand < 0` (an overflowed list, or `ORDER_NONE`). A `reduce` or `end_round`
/// returning `REDUCE_FLUSH` has filled the owner's round-2 buffers: the driver writes them as an
/// epoch of the owner's round-2 file (§3.7), resets the pool and calls again (`OwnerTop.resume`).
#[derive(Clone, Copy)]
pub struct OwnerFns {
    pub params: [u64; OWNER_PARAMS],
    /// One owned round, or two (§3.5: the distinct family re-sinks its group records by the group hash).
    pub rounds: u8,
    pub order_slot: u8,
    pub order_slot2: u8,
    /// The least region ONE owner opens when `owners` owners share the partitions (1: one owner folds them all, the
    /// driver's `own_all`). Only a two-round form's floor reads it: its second round re-sinks the groups by their hash, so
    /// a planned-groups part is one owner's share (KEYED-28); every other form's floor is the same for any count.
    pub min_bytes: fn(params: *const u64, owners: u32) -> u64,
    pub open: unsafe fn(params: *const u64, owner: *mut u8, bytes: usize) -> i64,
    pub reduce: ReduceFn,
    /// After the owner's last round-1 partition: its round-2 buffers closed for the second round.
    pub end_round: unsafe fn(params: *const u64, owner: *mut u8) -> i64,
    pub reduce2: ReduceFn,
    /// The owner fold (breakers.md "the owner fold per layout"); `None`: the generic `HashAgg::fold_run`.
    pub fold: Option<FoldFn>,
}

pub type ReduceFn = unsafe fn(params: *const u64, owner: *mut u8, table: *const HashAgg, site: *mut crate::Site, cand: *const u32, ncand: i64) -> i64;

/// `nrecs` whole records at `recs` folded into `table` in order under the split's filter, each noted in
/// `cand`: the records folded, or `-1 - folded` when the table refused one (7/8 load; the driver splits), or
/// `FOLD_OVERFLOW` when a float merge overflowed (D244: the statement raises).
pub type FoldFn = unsafe fn(params: *const u64, table: *mut HashAgg, recs: *const u8, nrecs: usize, split: SubSplit, cand: *mut Candidates) -> i64;

/// The owner fold's second generation: sub-partition `sub` of the `3 · depth` hash bits above `pbits` (`depth` 0: every record);
/// `reread` 1: the same stream read again at this depth after its table filled (the records before that fill are noted already).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubSplit {
    pub pbits: u8,
    pub depth: u8,
    pub reread: u8,
    /// D446: up to (ROUTE_BITS - pbits) bits of path (past 32 at depth 11), so a word.
    pub sub: u64,
}

impl SubSplit {
    pub const NONE: SubSplit = SubSplit { pbits: 0, depth: 0, reread: 0, sub: 0 };

    /// The sub-partition path of a record whose route is `route`: the `3 · depth` route bits after `pbits`, the first split's
    /// the most significant (a split at depth d fans a path `sub` out to `sub << 3 | s`).
    #[inline(always)]
    pub const fn sub_of(route: KeyHash, pbits: u8, depth: u8) -> u64 {
        route.sub(pbits as u32, depth as u32)
    }

    #[inline(always)]
    pub const fn keeps(&self, route: KeyHash) -> bool {
        self.depth == 0 || SubSplit::sub_of(route, self.pbits, self.depth) == self.sub
    }

    /// The route bits a table fed under this split consumed (its homes read below them).
    #[inline(always)]
    pub const fn table_depth(&self) -> Depth {
        // D446: unreachable past ROUTE_BITS. The one deepening fold (parallel merge.rs fold_sub) splits only while
        // `Depth::new(pbits, depth + 1)` exists, every other split is depth 0, and pbits <= 11 (T1_MAX_PARTS): so the
        // depth is always a Depth. `Depth::stored` asserts it in a debug build and never forgets the route (no TOP).
        Depth::stored(self.pbits as u32 + crate::hash::SPLIT_BITS * self.depth as u32)
    }
}

/// The owner fold's candidate list (`OwnerFns.order_slot`): the table indexes of the records whose count
/// reached the owner's threshold, listed once (`MOVING` until offered); overflowed = the whole table (`ncand` < 0).
#[repr(C)]
pub struct Candidates {
    pub ptr: *mut u32,
    pub len: usize,
    pub cap: usize,
    pub off: usize,
    pub thr: i64,
    pub overflow: bool,
}

impl Candidates {
    pub const fn none() -> Candidates {
        Candidates { ptr: core::ptr::null_mut(), len: 0, cap: 0, off: 0, thr: 0, overflow: true }
    }

    /// # Safety
    /// `ptr` has room for `cap` indexes, 4-aligned.
    pub unsafe fn at(ptr: *mut u8, cap: usize, off: usize, thr: i64) -> Candidates {
        debug_assert!(ptr.cast::<u32>().is_aligned(), "gravity: candidates at {ptr:p} are not 4-aligned (D1704)");
        Candidates { ptr: ptr as *mut u32, len: 0, cap, off, thr, overflow: false }
    }

    /// # Safety
    /// `p` is a live record (table index `j`) of the layout `off` indexes.
    #[inline(always)]
    pub unsafe fn note(&mut self, p: *mut u8, j: usize) {
        if self.overflow {
            return;
        }
        let w = p.add(self.off) as *mut u64;
        let c = *w;
        if c & MOVING != 0 || ((c & !MOVING) as i64) < self.thr {
            return;
        }
        if self.len == self.cap {
            self.overflow = true;
            return;
        }
        *w = c | MOVING;
        *self.ptr.add(self.len) = j as u32;
        self.len += 1;
    }

    pub fn ncand(&self) -> i64 {
        if self.overflow { -1 } else { self.len as i64 }
    }
}

/// The probe's prefetch distance in records: the reference machine's prefetch ring (cost_profile `Machine::REFERENCE`,
/// the driver's `REFERENCE_PREFETCH`), the ring array's length. A detected machine folds with a ring of at most this depth
/// (the driver's `MachineProfile::prefetch`, clamped to it), so the array never grows past the reference.
pub const FOLD_PF: usize = cost_profile::machine::Machine::REFERENCE.prefetch;

/// The owner phase's parameter words: four plan words, then the generic phase's `RecordLayout` and the
/// distinct form's accumulator layout by value (`LAYOUT_WORDS` each), then the count form's key check (a head word, a word
/// of the checked keys' words and one `(lo, hi)` interval per checked key: `OwnerDesc::check`, V4R-113; exec's
/// `owner.rs`), then the representatives' words past the first (`REP_WORDS`; the first is a plan word).
pub const LAYOUT_WORDS: usize = size_of::<RecordLayout>() / 8;
pub const OWNER_PARAMS: usize = 4 + 2 * LAYOUT_WORDS + 2 + 2 * KEY_CHECK_KEYS + (REP_WORDS - 1);
/// The group keys one count form's key check bounds (`OwnerDesc::check`): the owner's words keep an interval per key, one
/// for each key a record's inline tier holds, so a check over every key of such a record fits (a wide record's check
/// past them declines: the emit sorts every group).
pub const KEY_CHECK_KEYS: usize = MAX_RECORD_KEYS;
pub const REDUCE_FLUSH: i64 = 1;

pub const TABLE_SORT: u8 = 3;

/// The sort breaker's run buffer (§2 decision 12, `breakers.md`): rows of `row_bytes` dense at
/// `0..len` over a share, sorted in place by their first two words and put as a run when full and
/// at the claim's end; M4's by-reference copies grow down from the region's end to `var_top`, a put of
/// such rows compacts them into `var_bytes` of self-contained records; both words sit past the rows at `cap × row_bytes`.
/// The kind byte at 15; zeroed + armed = empty.
#[repr(C)]
pub struct SortRun {
    pub cap: u32,
    pub len: u32,
    pub row_bytes: u32,
    pub pad: [u8; 3],
    pub kind: u8,
}

pub const SORTRUN_HDR: usize = size_of::<SortRun>();
pub const SORTRUN_VAR_WORDS: u32 = 8;
/// `pad[1]` of a run opened by `at_rows`: its trailer is `SORTRUN_SIZED_WORDS` (the var words, the region's bytes, a spare).
pub const SORTRUN_SIZED: u8 = 0x52;
pub const SORTRUN_SIZED_WORDS: u32 = SORTRUN_VAR_WORDS + 8;

/// D17: the least rows a sized sort run opens with, as bytes: one 4 KiB page (the OS's allocation unit; 64 rows of the
/// common 64-byte spool row, the old fixed floor), so a wide row's run starts no larger than a narrow one's.
pub const SORTRUN_FLOOR_BYTES: u32 = 4096;

impl SortRun {
    /// The buffer of a share-armed region (word 2 = the share) at its first touch, read back afterwards.
    /// # Safety
    /// `p` points at a share-armed region or a live buffer of `row_bytes`.
    pub unsafe fn at_share<'a>(p: *mut u8, row_bytes: u32) -> &'a mut SortRun {
        let r = aligned_mut::<SortRun>(p);
        if r.cap == 0 {
            let armed = r.row_bytes;
            if armed as usize <= SORTRUN_HDR + SORTRUN_VAR_WORDS as usize + row_bytes as usize {
                crate::limits::exceeded(format_args!("a share too small for one sort row: {armed} bytes for a {row_bytes}-byte row"));
            }
            r.cap = (armed - SORTRUN_HDR as u32 - SORTRUN_VAR_WORDS) / row_bytes;
            r.row_bytes = row_bytes;
            r.kind = TABLE_SORT;
            r.clear();
        }
        assert!(r.row_bytes == row_bytes && r.kind == TABLE_SORT, "sort run shape");
        r
    }

    /// NEW-43: a region a merge was laid over (its runs read and dropped) armed again as an empty run of `bytes`
    /// (`region_bytes` before the merge: the room the run had), as `at_share` arms a fresh share.
    /// # Safety
    /// `p` points at a region of at least `bytes` bytes that no live run or merge uses.
    pub unsafe fn rearm<'a>(p: *mut u8, bytes: u32, row_bytes: u32) -> &'a mut SortRun {
        let r = aligned_mut::<SortRun>(p);
        (r.cap, r.len, r.row_bytes, r.pad, r.kind) = (0, 0, bytes, [0; 3], 0);
        Self::at_share(p, row_bytes)
    }

    /// The run opened for `rows` (2× the estimate, at least a page of rows: `SORTRUN_FLOOR_BYTES`) inside the armed share: the region's bytes are kept past the var words (`pad[1] = SORTRUN_SIZED`) so `grow` doubles within it; read back afterwards.
    /// # Safety
    /// `p` points at a share-armed region or a live buffer of `row_bytes`.
    pub unsafe fn at_rows<'a>(p: *mut u8, row_bytes: u32, rows: u32) -> &'a mut SortRun {
        let r = aligned_mut::<SortRun>(p);
        if r.cap == 0 {
            let armed = r.row_bytes;
            if armed as usize <= SORTRUN_HDR + SORTRUN_SIZED_WORDS as usize + row_bytes as usize {
                crate::limits::exceeded(format_args!("a share too small for one spool row: {armed} bytes for a {row_bytes}-byte row"));
            }
            let most = (armed - SORTRUN_HDR as u32 - SORTRUN_SIZED_WORDS) / row_bytes;
            // D17: the estimate's rows with one doubling of headroom (the step `grow` takes), at least one page of rows
            r.cap = rows.saturating_mul(2).max((SORTRUN_FLOOR_BYTES / row_bytes).max(1)).min(most);
            r.row_bytes = row_bytes;
            r.kind = TABLE_SORT;
            r.pad[1] = SORTRUN_SIZED;
            r.clear();
            *r.var_words_mut().add(2) = armed;
        }
        assert!(r.row_bytes == row_bytes && r.kind == TABLE_SORT, "sort run shape");
        r
    }

    /// The bytes of the region an `at_rows` run was opened in; 0 for a share-wide run (its extent is the region).
    pub fn region(&self) -> u32 {
        if self.pad[1] == SORTRUN_SIZED { unsafe { *self.var_words().add(2) } } else { 0 }
    }

    /// The capacity doubled within the region, the trailer moved past the new rows (the rows stay): false when the region is full or a by-reference copy is held (it would move) — the tail puts the run.
    pub fn grow(&mut self) -> bool {
        let region = self.region();
        if region == 0 || self.var_top() != SORTRUN_HDR as u32 + self.cap * self.row_bytes {
            return false;
        }
        let most = (region - SORTRUN_HDR as u32 - SORTRUN_SIZED_WORDS) / self.row_bytes;
        let cap = self.cap.saturating_mul(2).min(most);
        if cap <= self.cap {
            return false;
        }
        let var_bytes = self.var_bytes();
        self.cap = cap;
        unsafe {
            let w = self.var_words_mut();
            *w = SORTRUN_HDR as u32 + cap * self.row_bytes;
            *w.add(1) = var_bytes;
            *w.add(2) = region;
        }
        true
    }

    /// The rows' base (`row(0)`): the reader's pointer walk from the run's header.
    pub const fn rows_ptr(&self) -> *const u8 {
        unsafe { (self as *const SortRun as *const u8).add(SORTRUN_HDR) }
    }

    /// The usable bytes of a region: a live buffer's extent (an `at_rows` run's whole region), else the share it was armed with.
    /// # Safety
    /// `p` points at a share-armed region or a live buffer.
    pub unsafe fn region_bytes(p: *const u8, row_bytes: u32) -> usize {
        let r = aligned_ref::<SortRun>(p);
        if r.cap == 0 {
            r.row_bytes as usize
        } else if r.region() != 0 {
            r.region() as usize
        } else {
            SORTRUN_HDR + r.cap as usize * row_bytes as usize + SORTRUN_VAR_WORDS as usize
        }
    }

    fn var_words(&self) -> *const u32 {
        unsafe { self.base().add(self.cap as usize * self.row_bytes as usize) as *const u32 }
    }

    /// The var words for a writer, from `&mut self` (D1358): `reserve_var`'s closure stored var_top through `var_words`'s
    /// pointer, a write through a shared borrow of the run, and LLVM dropped the store once the closure was out of line
    /// (instcombine under `-Zub-checks`): var_top never moved, every by-reference copy took the same slot.
    fn var_words_mut(&mut self) -> *mut u32 {
        let at = SORTRUN_HDR + self.cap as usize * self.row_bytes as usize;
        unsafe { (self as *mut SortRun as *mut u8).add(at) as *mut u32 }
    }

    pub fn var_top(&self) -> u32 {
        unsafe { *self.var_words() }
    }

    pub fn var_bytes(&self) -> u32 {
        unsafe { *self.var_words().add(1) }
    }

    pub fn set_var_bytes(&mut self, bytes: u32) {
        unsafe { *self.var_words_mut().add(1) = bytes }
    }

    fn base(&self) -> *const u8 {
        unsafe { (self as *const SortRun as *const u8).add(SORTRUN_HDR) }
    }

    /// The rows for a writer, from `&mut self` (D1358, as `var_words_mut`).
    fn base_mut(&mut self) -> *mut u8 {
        unsafe { (self as *mut SortRun as *mut u8).add(SORTRUN_HDR) }
    }

    /// The next row's bytes (the caller fills them); the buffer must have room.
    pub fn push(&mut self) -> *mut u8 {
        debug_assert!(self.len < self.cap);
        let p = unsafe { self.base_mut().add(self.len as usize * self.row_bytes as usize) };
        self.len += 1;
        p
    }

    pub fn row(&self, i: usize) -> *const u8 {
        unsafe { self.base().add(i * self.row_bytes as usize) }
    }

    /// The run emptied (its rows put, or a fresh share): no rows, so no seal either. D2471: a slot more than one pipeline writes
    /// (a Hash Right Join's probe, then its drain) kept the first writer's seal past the drain's put, and the reader took
    /// the empty region for the whole sorted input.
    pub fn clear(&mut self) {
        self.len = 0;
        self.pad[0] = 0;
        unsafe {
            let w = self.var_words_mut();
            *w = SORTRUN_HDR as u32 + self.cap * self.row_bytes;
            *w.add(1) = 0;
        }
    }

    pub fn dense_bytes(&self) -> usize {
        match self.var_bytes() {
            0 => self.len as usize * self.row_bytes as usize,
            n => n as usize,
        }
    }

    /// Reserve `bytes` (8-rounded) of the variable region above the next row's slot: its offset from the region base, `None` without room.
    pub fn reserve_var(&mut self, bytes: u32) -> Option<u32> {
        let floor = SORTRUN_HDR as u32 + (self.len + 1) * self.row_bytes;
        let top = self.var_top().checked_sub(bytes)?;
        if top < floor {
            return None;
        }
        unsafe { *self.var_words_mut() = top };
        Some(top)
    }

    /// Order the rows by their first two words (an in-place heap sort over the raw rows).
    pub fn sort(&mut self) {
        let (rb, n) = (self.row_bytes as usize, self.len as usize);
        let base = self.base_mut();
        // SAFETY: rows 0..n are live, `rb` bytes each, from `base`.
        unsafe {
            let key = |i: usize| -> (u64, u64) { (*(base.add(i * rb) as *const u64), *(base.add(i * rb + 8) as *const u64)) };
            // D327: rows of any width swap in place (a fixed 256 B buffer asserted wider sort rows away)
            let swap = |a: usize, b: usize| core::ptr::swap_nonoverlapping(base.add(a * rb), base.add(b * rb), rb);
            let sift = |mut root: usize, end: usize| loop {
                let mut child = 2 * root + 1;
                if child >= end {
                    return;
                }
                if child + 1 < end && key(child) < key(child + 1) {
                    child += 1;
                }
                if key(root) >= key(child) {
                    return;
                }
                swap(root, child);
                root = child;
            };
            for start in (0..n / 2).rev() {
                sift(start, n);
            }
            for end in (1..n).rev() {
                swap(0, end);
                sift(0, end);
            }
        }
    }
}

/// The dense key front (§2 decision 7, `breakers.md`): record indexes (+ 1; 0 = unknown) keyed by
/// `key - lo` over a domain of at most `u32::MAX` values; a cache the kernel clears when records move.
#[repr(C)]
pub struct DenseFront {
    pub lo: i64,
    pub n: u32,
    pub pad: u32,
}

/// `FLAG_ACC` = cells follow the front.
pub const FLAG_ACC: u8 = 2;

/// A dense-domain value's pending `sum(v)`, `count(*)` and non-null `v` count (zero = nothing pending).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AccCell {
    pub sum: i64,
    pub cnt: i64,
    pub nn: i64,
}

pub const ACC_CELL: usize = size_of::<AccCell>();

impl DenseFront {
    pub const fn bytes(n: u32) -> u32 {
        (size_of::<DenseFront>() + n as usize * 4) as u32
    }

    pub const fn bytes_with_acc(n: u32) -> u32 {
        ((DenseFront::bytes(n) + 7) & !7) + n * ACC_CELL as u32
    }

    /// The `AccCell`s behind the slots (8-aligned; present under the table's `FLAG_ACC`).
    #[inline(always)]
    pub fn cells(&self) -> *mut AccCell {
        unsafe { (self as *const DenseFront as *mut u8).add(((DenseFront::bytes(self.n) + 7) & !7) as usize) as *mut AccCell }
    }

    /// The domain's size: `None` when it is empty or past the front's `u32` count (the structural bound;
    /// whether the cells pay is `acc_fits`, a cost fact).
    pub const fn domain(lo: i64, hi: i64) -> Option<u32> {
        if hi < lo {
            return None;
        }
        let n = (hi as i128 - lo as i128 + 1) as u128;
        if n > u32::MAX as u128 {
            None
        } else {
            Some(n as u32)
        }
    }

    /// Whether `n` accumulator cells pay over a cache partition of `cache_bytes` (the machine's L2 share):
    /// every row adds into a cell at random and the claim-end fold scans them all, so the cells keep to
    /// half the partition (the other half is the phase-1 table's hot slots); past it the group is hashed.
    pub const fn acc_fits(n: u32, cache_bytes: u64) -> bool {
        n as u64 * ACC_CELL as u64 <= cache_bytes / 2
    }

    /// # Safety
    /// `p` points at `bytes(n)` writable bytes, 8-aligned, zeroed or a live front of that shape.
    pub unsafe fn at<'a>(p: *mut u8, lo: i64, n: u32) -> &'a mut DenseFront {
        let f = aligned_mut::<DenseFront>(p);
        if f.n == 0 {
            f.lo = lo;
            f.n = n;
        }
        debug_assert!(f.lo == lo && f.n == n, "dense front shape");
        f
    }

    #[inline(always)]
    fn slots(&self) -> *const u32 {
        unsafe { (self as *const DenseFront as *const u8).add(size_of::<DenseFront>()) as *const u32 }
    }

    /// D1358b: the slots' writers derive their pointer from the front's own `&mut` borrow, never from `&self` (Tree Borrows:
    /// a `&self`-derived pointer is read-only).
    #[inline(always)]
    fn slots_mut(&mut self) -> *mut u32 {
        unsafe { (self as *mut DenseFront as *mut u8).add(size_of::<DenseFront>()) as *mut u32 }
    }

    #[inline(always)]
    pub fn slot(&self, key: i64) -> Option<usize> {
        let d = key.wrapping_sub(self.lo) as u64;
        (d < self.n as u64).then_some(d as usize)
    }

    #[inline(always)]
    pub fn get(&self, d: usize) -> Option<usize> {
        let v = unsafe { *self.slots().add(d) };
        if v == 0 {
            None
        } else {
            Some(v as usize - 1)
        }
    }

    #[inline(always)]
    pub fn set(&mut self, d: usize, index: usize) {
        unsafe { *self.slots_mut().add(d) = index as u32 + 1 };
    }

    pub fn clear(&mut self) {
        let n = self.n as usize;
        unsafe { core::ptr::write_bytes(self.slots_mut(), 0, n) };
    }
}

/// `key × φ`: a partition reads its TOP bits (`key_hash`; a spill's split the next down), a home
/// its LOW bits — disjoint, so a table refilled from one partition still uses every home.
const fn mix(key: i32) -> u32 {
    (key as u32).wrapping_mul(0x9E37_79B9)
}

#[inline(always)]
const fn home_of(key: i32, mask: usize) -> usize {
    mix(key) as usize & mask
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HashBuildEntry {
    pub used: u32,
    pub key: i32,
    pub payload: i64,
}

#[repr(C)]
pub struct HashBuild {
    pub cap: u32,
    pub len: u32,
    pub table: u32,
    /// `BUILD_TABLE` in `1 << pbits` slices (decision 5); an entry is probed inside its key's slice only.
    pub pbits: u8,
    /// A partition sorted by `partition_of(key, sorted − 1)` with its directory behind the entries (0: unsorted).
    pub sorted: u8,
    pub pad: u8,
    pub kind: u8,
    /// The share a table opened by `at_share_sized` may grow into (0: a fixed region).
    pub region: u32,
    pub pad2: u32,
}

pub const BUILD_PARTITION: u32 = 0;
/// The `used` tag of a build entry whose payload is NULL.
pub const USED_NULL: u32 = 2;
pub const BUILD_TABLE: u32 = 1;
pub const HASHBUILD_HDR: usize = size_of::<HashBuild>();
const BUILD_MOVING: u32 = 0x8000_0000;
pub const HASHBUILD_ENTRY: usize = size_of::<HashBuildEntry>();
const _: () = assert!(HASHBUILD_ENTRY as u64 == cost_profile::hash::HASHBUILD_ENTRY_BYTES);
/// The most partitions a build partial is sorted into: the phase-1 route bound (`partition_plan` caps every record width there).
pub const MAX_BUILD_PARTS: usize = T1_MAX_PARTS as usize;

impl HashBuild {
    pub const fn bytes(cap: u32) -> u32 {
        (HASHBUILD_HDR + cap as usize * HASHBUILD_ENTRY) as u32
    }

    /// The capacity a breaker of `bytes` was laid out for (the inverse of `bytes`).
    pub const fn cap_of(bytes: u32) -> u32 {
        ((bytes as usize).saturating_sub(HASHBUILD_HDR) / HASHBUILD_ENTRY) as u32
    }

    pub const fn table_cap_of(bytes: u32) -> u32 {
        let n = HashBuild::cap_of(bytes);
        if n == 0 {
            0
        } else {
            1 << (u32::BITS - 1 - n.leading_zeros())
        }
    }

    /// # Safety
    /// `p` points at `bytes(cap)` writable bytes, 8-aligned, zeroed or a live build of capacity `cap`.
    pub unsafe fn at<'a>(p: *mut u8, cap: u32, table: u32) -> &'a mut HashBuild {
        let b = aligned_mut::<HashBuild>(p);
        if b.cap == 0 {
            // a hashed table probes for empty slots: its extent is zeroed here (a partition appends, `len` gates its reads)
            if table == BUILD_TABLE {
                core::ptr::write_bytes((b as *mut HashBuild as *mut u8).add(HASHBUILD_HDR), 0, cap as usize * HASHBUILD_ENTRY);
            }
            b.cap = cap;
            b.table = table;
            b.kind = TABLE_BUILD;
        }
        assert!(b.cap == cap && b.table == table && (table == BUILD_PARTITION || cap.is_power_of_two()), "hash build shape");
        b
    }

    /// A `BUILD_TABLE` of `1 << pbits` slices over a zeroed region of `bytes`.
    /// # Safety
    /// `p` points at `bytes` zeroed writable bytes, 8-aligned.
    pub unsafe fn at_partitioned<'a>(p: *mut u8, bytes: u32, pbits: u8) -> &'a mut HashBuild {
        HashBuild::at_partitioned_sized(p, bytes, pbits, u32::MAX)
    }

    /// `at_partitioned` opened at `cap` entries (a power of two; the region's largest when larger).
    /// # Safety
    /// As `at_partitioned`.
    pub unsafe fn at_partitioned_sized<'a>(p: *mut u8, bytes: u32, pbits: u8, cap: u32) -> &'a mut HashBuild {
        let b = aligned_mut::<HashBuild>(p);
        assert!(b.cap == 0, "a partitioned build is laid out over a fresh region");
        debug_assert!(HashBuild::table_cap_of(bytes) >= 1 << pbits, "D209: a region of {bytes} B under one entry per partition ({pbits} bits); the caller refuses first");
        let cap = cap.min(HashBuild::table_cap_of(bytes)).max(1 << pbits);
        assert!(cap.is_power_of_two() && cap >= 1 << pbits, "partition slices need at least one entry each");
        // D1358: through the build's own borrow, never `p` beside it (a write past the header through `p` while `b` lives
        // disables `b`'s claim on the entries its caller then reads)
        core::ptr::write_bytes((b as *mut HashBuild as *mut u8).add(HASHBUILD_HDR), 0, cap as usize * HASHBUILD_ENTRY);
        b.cap = cap;
        b.table = BUILD_TABLE;
        b.pbits = pbits;
        b.kind = TABLE_BUILD;
        b
    }

    /// A live build read back (a partial that describes itself).
    /// # Safety
    /// `p` points at a build `at` initialised.
    pub unsafe fn view<'a>(p: *const u8) -> &'a HashBuild {
        aligned_ref::<HashBuild>(p)
    }

    /// The share word 2 of a zeroed region was armed with (`HashAgg::arm`), 0 once laid out.
    /// # Safety
    /// `p` points at least 16 readable bytes, 8-aligned.
    pub unsafe fn armed(p: *const u8) -> u32 {
        let b = aligned_ref::<HashBuild>(p);
        if b.cap == 0 {
            b.table
        } else {
            0
        }
    }

    /// The build of a share-armed region, laid out over the whole share at the first touch.
    /// # Safety
    /// `p` points at a share-armed region (`armed(p) != 0`) or a live build laid out by this call.
    pub unsafe fn at_share<'a>(p: *mut u8, table: u32) -> &'a mut HashBuild {
        HashBuild::at_share_sized(p, table, u32::MAX)
    }

    /// A table opened at `cap` inside `region` bytes at `p` (its grow ceiling), read back afterwards.
    /// # Safety
    /// `p` points at `region` writable bytes, 8-aligned, holding a fresh (header-zeroed) or live build.
    pub unsafe fn at_sized<'a>(p: *mut u8, region: u32, table: u32, cap: u32) -> &'a mut HashBuild {
        let b = aligned_mut::<HashBuild>(p);
        if b.cap != 0 {
            assert!(b.table == table, "hash build shape");
            return b;
        }
        let cap = if table == BUILD_TABLE { HashBuild::table_cap_of(region).min(cap.max(1024)) } else { HashBuild::cap_of(region) };
        if cap == 0 {
            crate::limits::exceeded(format_args!("a region too small for one build entry: {region} bytes"));
        }
        let b = HashBuild::at(p, cap, table);
        b.region = region;
        b
    }

    /// `at_share` with a hashed table opened at `cap` (a power of two; the region's largest when
    /// larger) and grown from there (`grow`), so a small build does not zero a whole share.
    /// # Safety
    /// As `at_share`.
    pub unsafe fn at_share_sized<'a>(p: *mut u8, table: u32, cap: u32) -> &'a mut HashBuild {
        let armed = HashBuild::armed(p);
        if armed != 0 {
            (*(p as *mut HashBuild)).table = 0;
            let cap = if table == BUILD_TABLE { HashBuild::table_cap_of(armed).min(cap.max(1024)) } else { HashBuild::cap_of(armed) };
            if cap == 0 {
                crate::limits::exceeded(format_args!("a share too small for one build entry: {armed} bytes"));
            }
            let b = HashBuild::at(p, cap, table);
            b.sorted = 0;
            b.region = armed;
            return b;
        }
        let b = aligned_mut::<HashBuild>(p);
        assert!(b.cap != 0 && b.table == table, "hash build shape");
        b
    }

    /// The partition hash: `mix` bit-reversed (`& mask` reads its top bits, a split's `>> shift` the next).
    pub const fn key_hash(key: i32) -> u32 {
        mix(key).reverse_bits()
    }

    pub const fn partition_of(key: i32, pbits: u8) -> u32 {
        HashBuild::key_hash(key) & ((1u32 << pbits) - 1)
    }

    #[inline(always)]
    fn slice_of(&self, key: i32) -> (usize, usize) {
        let sub = (self.cap as usize) >> self.pbits;
        (HashBuild::partition_of(key, self.pbits) as usize * sub, sub - 1)
    }

    #[inline(always)]
    fn home(&self, key: i32, mask: usize) -> usize {
        home_of(key, mask)
    }

    pub fn entry(&self, i: usize) -> &HashBuildEntry {
        debug_assert!(i < self.cap as usize);
        unsafe { &*(self as *const HashBuild as *const u8).add(HASHBUILD_HDR).cast::<HashBuildEntry>().add(i) }
    }

    pub fn entry_mut(&mut self, i: usize) -> &mut HashBuildEntry {
        debug_assert!(i < self.cap as usize);
        unsafe { &mut *(self as *mut HashBuild as *mut u8).add(HASHBUILD_HDR).cast::<HashBuildEntry>().add(i) }
    }

    fn entry_ptr(&self, i: usize) -> *const HashBuildEntry {
        unsafe { (self as *const HashBuild as *const u8).add(HASHBUILD_HDR).cast::<HashBuildEntry>().add(i) }
    }

    /// Entry `i` for a writer, from `&mut self` (D1358: a write through `entry_ptr`'s pointer, a shared borrow of the build,
    /// is undefined behaviour the optimizer may act on).
    fn entry_ptr_mut(&mut self, i: usize) -> *mut HashBuildEntry {
        unsafe { (self as *mut HashBuild as *mut u8).add(HASHBUILD_HDR).cast::<HashBuildEntry>().add(i) }
    }

    /// The used entries: a partition's `0..len` (its tail may be dirty), a table's used slots.
    pub fn entries(&self) -> impl Iterator<Item = &HashBuildEntry> + '_ {
        let n = if self.table == BUILD_PARTITION { self.len as usize } else { self.cap as usize };
        (0..n).map(|i| self.entry(i)).filter(|e| e.used != 0)
    }

    /// The largest hashed capacity this table may grow to inside its region (its own when fixed).
    pub fn max_cap(&self) -> u32 {
        if self.region == 0 {
            self.cap
        } else {
            HashBuild::table_cap_of(self.region)
        }
    }

    /// Double a hashed table in place while the doubled table fits its region: the new half zeroed,
    /// every entry marked moving and settled by the cycle-leader placement (`hash_in_place`'s).
    pub fn grow(&mut self) -> bool {
        if self.table != BUILD_TABLE || self.cap * 2 > self.max_cap() {
            return false;
        }
        let old = self.cap as usize;
        unsafe { core::ptr::write_bytes(self.entry_ptr_mut(old) as *mut u8, 0, old * HASHBUILD_ENTRY) };
        for i in 0..old {
            let e = self.entry_mut(i);
            if e.used != 0 {
                e.used |= BUILD_MOVING;
            }
        }
        self.cap = (old * 2) as u32;
        self.settle_moving(old);
        true
    }

    /// Settle every `BUILD_MOVING` entry among the first `n` slots into the (current-capacity) table.
    fn settle_moving(&mut self, n: usize) {
        let mask = self.cap as usize - 1;
        let pbits = self.pbits;
        let place = |b: &mut HashBuild, mut e: HashBuildEntry| {
            e.used &= !BUILD_MOVING;
            loop {
                let (base, smask) = if pbits == 0 { (0, mask) } else { b.slice_of(e.key) };
                let mut j = base + home_of(e.key, smask);
                loop {
                    let slot = *b.entry(j);
                    if slot.used == 0 {
                        *b.entry_mut(j) = e;
                        return;
                    }
                    if slot.used & BUILD_MOVING != 0 {
                        *b.entry_mut(j) = e;
                        e = HashBuildEntry { used: slot.used & !BUILD_MOVING, ..slot };
                        break;
                    }
                    j = base + ((j - base + 1) & smask);
                }
            }
        };
        for i in 0..n {
            let e = *self.entry(i);
            if e.used & BUILD_MOVING == 0 {
                continue;
            }
            *self.entry_mut(i) = Default::default();
            place(self, e);
        }
    }

    pub fn dense_bytes(&self) -> &[u8] {
        debug_assert_eq!(self.table, BUILD_PARTITION);
        unsafe { core::slice::from_raw_parts(self.entry_ptr(0) as *const u8, self.len as usize * HASHBUILD_ENTRY) }
    }

    pub fn insert(&mut self, key: i32, payload: i64) -> bool {
        self.insert_tagged(key, payload, 1)
    }

    /// `used` tags the entry: 1 a payload, `USED_NULL` a NULL payload (the
    /// probe emits it as NULL); any nonzero value keeps the slot occupied.
    pub fn insert_tagged(&mut self, key: i32, payload: i64, used: u32) -> bool {
        debug_assert!(used != 0, "a used tag");
        if self.table == BUILD_PARTITION {
            if self.len >= self.cap {
                return false;
            }
            let i = self.len as usize;
            self.len += 1;
            *self.entry_mut(i) = HashBuildEntry { used, key, payload };
            return true;
        }
        // SAFETY: `self` is exclusively borrowed; the raw pointer carries its provenance.
        match unsafe { HashBuild::insert_at(self as *mut HashBuild, key, payload, used) } {
            Some(()) => {
                self.len += 1;
                true
            }
            None => false,
        }
    }

    /// Insert into the key's slice, `len` added atomically; `None` when the slice is full.
    /// # Safety
    /// `this` is a live `BUILD_TABLE` and the slice is this thread's (a raw pointer, never `&self`).
    pub unsafe fn insert_partitioned(this: *mut HashBuild, key: i32, payload: i64, used: u32) -> Option<()> {
        HashBuild::insert_at(this, key, payload, used)?;
        HashBuild::add_len(this, 1);
        Some(())
    }

    /// # Safety
    /// As `insert_partitioned` (`len` untouched: a job adds its count once, `add_len`).
    #[inline(always)]
    pub unsafe fn insert_partitioned_uncounted(this: *mut HashBuild, key: i32, payload: i64, used: u32) -> Option<()> {
        HashBuild::insert_at(this, key, payload, used)
    }

    /// # Safety
    /// `this` is a live build shared by the partition jobs (`len += n` atomically).
    pub unsafe fn add_len(this: *mut HashBuild, n: u32) {
        (*(core::ptr::addr_of_mut!((*this).len) as *const core::sync::atomic::AtomicU32)).fetch_add(n, core::sync::atomic::Ordering::Relaxed);
    }

    unsafe fn insert_at(this: *mut HashBuild, key: i32, payload: i64, used: u32) -> Option<()> {
        debug_assert_eq!((*this).table, BUILD_TABLE);
        let (base, mask) = (*this).slice_of(key);
        let mut i = base + (*this).home(key, mask);
        let entries = (this as *mut u8).add(HASHBUILD_HDR).cast::<HashBuildEntry>();
        for _ in 0..=mask {
            let e = entries.add(i);
            if (*e).used == 0 {
                *e = HashBuildEntry { used, key, payload };
                return Some(());
            }
            i = base + ((i - base + 1) & mask);
        }
        None
    }

    pub fn clear(&mut self) {
        let cap = self.cap as usize;
        unsafe { core::ptr::write_bytes((self as *mut HashBuild as *mut u8).add(HASHBUILD_HDR), 0, cap * HASHBUILD_ENTRY) };
        self.len = 0;
        self.sorted = 0;
    }

    /// A partition counting-sorted in place by `partition_of`, the `P + 1` starts behind the entries; false = no room.
    pub fn sort_by_partition(&mut self, pbits: u8) -> bool {
        debug_assert_eq!(self.table, BUILD_PARTITION);
        let parts = 1usize << pbits;
        if parts > MAX_BUILD_PARTS {
            crate::limits::exceeded(format_args!("{parts} build partitions: at most {MAX_BUILD_PARTS}"));
        }
        let n = self.len as usize;
        let dir_entries = (4 * (parts + 1)).div_ceil(HASHBUILD_ENTRY);
        if n + dir_entries > self.cap as usize {
            return false;
        }
        let entries = self.entry_ptr_mut(0);
        let part_of = |i: usize| unsafe { HashBuild::partition_of((*entries.add(i)).key, pbits) as usize };
        let mut counts = [0u32; MAX_BUILD_PARTS];
        for i in 0..n {
            counts[part_of(i)] += 1;
        }
        let mut starts = [0u32; MAX_BUILD_PARTS + 1];
        for p in 0..parts {
            starts[p + 1] = starts[p] + counts[p];
        }
        let mut next = starts;
        for p in 0..parts {
            while next[p] < starts[p + 1] {
                let i = next[p] as usize;
                let part = part_of(i);
                if part == p {
                    next[p] += 1;
                    continue;
                }
                let j = next[part] as usize;
                next[part] += 1;
                unsafe { core::ptr::swap(entries.add(i), entries.add(j)) };
            }
        }
        let dir = self.entry_ptr_mut(n) as *mut u32;
        for (p, s) in starts.iter().enumerate().take(parts + 1) {
            unsafe { *dir.add(p) = *s };
        }
        self.sorted = pbits + 1;
        true
    }

    #[inline(always)]
    pub fn partition_range(&self, part: u32, pbits: u8) -> Option<(usize, usize)> {
        if self.sorted != pbits + 1 {
            return None;
        }
        let dir = self.entry_ptr(self.len as usize) as *const u32;
        unsafe { Some((*dir.add(part as usize) as usize, *dir.add(part as usize + 1) as usize)) }
    }

    /// `at_partitioned_sized` without zeroing the slices (`zero_slice_at`, one per job).
    /// # Safety
    /// `p` points at `bytes` writable bytes, 8-aligned, the header zeroed.
    pub unsafe fn at_partitioned_lazy<'a>(p: *mut u8, bytes: u32, pbits: u8, cap: u32) -> &'a mut HashBuild {
        let b = aligned_mut::<HashBuild>(p);
        assert!(b.cap == 0, "a partitioned build is laid out over a fresh region");
        debug_assert!(HashBuild::table_cap_of(bytes) >= 1 << pbits, "D209: a region of {bytes} B under one entry per partition ({pbits} bits); the caller refuses first");
        let cap = cap.min(HashBuild::table_cap_of(bytes)).max(1 << pbits);
        assert!(cap.is_power_of_two() && cap >= 1 << pbits, "partition slices need at least one entry each");
        b.cap = cap;
        b.table = BUILD_TABLE;
        b.pbits = pbits;
        b.kind = TABLE_BUILD;
        b
    }

    /// # Safety
    /// `this` is a live partitioned `BUILD_TABLE`; slice `part` is this thread's alone.
    pub unsafe fn zero_slice_at(this: *mut HashBuild, part: u32) {
        let sub = ((*this).cap >> (*this).pbits) as usize;
        core::ptr::write_bytes((*this).entry_ptr(part as usize * sub) as *mut u8, 0, sub * HASHBUILD_ENTRY);
    }

    /// A table's capacity for `len` entries (at least 2^`BUILD_MIN_CAP_BITS`, capped by the caller's region): the hash
    /// loads' `BUILD_IN_PLACE_LOAD`, or `BUILD_SPARSE_LOAD` up to `build_sparse_rows` (D155, D03: from the L2 share).
    pub fn table_cap_for(len: u64) -> u32 {
        let l = if len <= build_sparse_rows() { BUILD_SPARSE_LOAD } else { BUILD_IN_PLACE_LOAD };
        let want = (len * l.den).div_ceil(l.num);
        want.next_power_of_two().max(1 << BUILD_MIN_CAP_BITS) as u32
    }

    /// Whether `len` entries pass `BUILD_IN_PLACE_LOAD` of a `cap`-slot table.
    fn past_in_place_load(len: u64, cap: u64) -> bool {
        len * BUILD_IN_PLACE_LOAD.den > cap * BUILD_IN_PLACE_LOAD.num
    }

    /// Turn a `BUILD_PARTITION` (dense entries at `0..len`) into a `BUILD_TABLE` over the same region
    /// in place, sized for its rows: every entry moved to its chain by a cycle-leader placement (an
    /// unsettled entry in the way is taken in hand). False past 7/8 load (the caller spills instead).
    pub fn hash_in_place(&mut self) -> bool {
        if self.table == BUILD_TABLE {
            return true;
        }
        let region = HashBuild::bytes(self.cap);
        let most = HashBuild::table_cap_of(region);
        let len = self.len as usize;
        // like `merge_into`'s table (`table_cap_for`), grown on demand: a region-wide table zero-faults the share (12 rows zeroed 1 GB per W0 statement)
        let cap = (HashBuild::table_cap_for(len as u64) as u64).min(most as u64) as usize;
        if HashBuild::past_in_place_load(len as u64, cap as u64) {
            return false;
        }
        self.region = region;
        for i in 0..len {
            self.entry_mut(i).used |= BUILD_MOVING;
        }
        // the dirty tail inside the new table is zeroed; entries past the capacity are taken in hand first
        if len < cap {
            unsafe { core::ptr::write_bytes(self.entry_ptr_mut(len) as *mut u8, 0, (cap - len) * HASHBUILD_ENTRY) };
        }
        self.table = BUILD_TABLE;
        self.pbits = 0;
        self.cap = cap as u32;
        let mask = cap - 1;
        for i in cap..len {
            let mut e = unsafe { *self.entry_ptr(i) };
            unsafe { *self.entry_ptr_mut(i) = Default::default() };
            e.used &= !BUILD_MOVING;
            // an entry in hand finds the first slot from its home that is empty or unsettled
            'place: loop {
                let mut j = home_of(e.key, mask);
                loop {
                    let slot = *self.entry(j);
                    if slot.used == 0 {
                        *self.entry_mut(j) = e;
                        break 'place;
                    }
                    if slot.used & BUILD_MOVING != 0 {
                        *self.entry_mut(j) = e;
                        e = HashBuildEntry { used: slot.used & !BUILD_MOVING, ..slot };
                        continue 'place;
                    }
                    j = (j + 1) & mask;
                }
            }
        }
        self.settle_moving(len.min(cap));
        true
    }

    pub fn probe(&self, key: i32) -> Probe<'_> {
        debug_assert_eq!(self.table, BUILD_TABLE, "probe on a partition");
        let (base, mask) = self.slice_of(key);
        Probe { b: self, base, mask, i: self.home(key, mask), key, left: mask + 1 }
    }

    /// Continue a probe chain at in-slice position `at` (the `cont` cell, decision 9).
    pub fn probe_from(&self, key: i32, at: usize) -> Probe<'_> {
        let (base, mask) = self.slice_of(key);
        Probe { b: self, base, mask, i: at & mask, key, left: mask + 1 }
    }
}

pub struct Probe<'a> {
    b: &'a HashBuild,
    base: usize,
    mask: usize,
    i: usize,
    key: i32,
    left: usize,
}

impl Probe<'_> {
    pub const fn position(&self) -> usize {
        self.i
    }
}

impl<'a> Iterator for Probe<'a> {
    type Item = &'a HashBuildEntry;

    fn next(&mut self) -> Option<&'a HashBuildEntry> {
        while self.left > 0 {
            let e = self.b.entry(self.base + self.i);
            self.left -= 1;
            self.i = (self.i + 1) & self.mask;
            if e.used == 0 {
                self.left = 0;
                return None;
            }
            if e.key == self.key {
                return Some(e);
            }
        }
        None
    }
}

/// A bounded run of the `n` smallest rows by the key in column 0 (sign- extended int keys, ascending; a NULL rides the image: the pair under `TOPK_PAIRS`, a folded word otherwise): the zone-walk breaker
/// (§1.10 increment 3 step 5). `cap` rows (`cap × natts` datums) then nulls (`cap × natts` bytes) follow the header; the run is kept sorted by
/// insertion. `TOPK_TIES` keeps every row whose key equals the n-th key (`cap > n`): the leader then orders ties by a secondary key it alone can
/// read (a dictionary code's bytes, `merge::order_topk`) and cuts to `n`.
#[repr(C)]
pub struct TopK {
    pub n: u32,
    pub natts: u32,
    pub len: u32,
    pub cap: u32,
    pub flags: u32,
    pub pad: u32,
}

pub const TOPK_TIES: u32 = 1;
pub const TOPK_SECOND: u32 = 2; // the column after the key is a second key's image: without ties the row is admitted and placed by (key, second)
pub const TOPK_PAIRS: u32 = 4; // the key is the pair `[flags, word]` (the null image: flags as i64, then the word unsigned)
pub const TOPK_SECOND_PAIR: u32 = 8; // the second key is a pair too

impl TopK {
    pub const fn bytes(n: u32, natts: u32) -> u32 {
        TopK::bytes_cap(n, natts)
    }

    /// The bytes of a run of `cap` rows.
    pub const fn bytes_cap(cap: u32, natts: u32) -> u32 {
        (size_of::<TopK>() as u32) + cap * natts * 9
    }

    /// # Safety
    /// `p` points at `bytes(n, natts)` writable bytes, 8-aligned, zeroed or a live run of that shape.
    pub unsafe fn at<'a>(p: *mut u8, n: u32, natts: u32) -> &'a mut TopK {
        TopK::at_cap(p, n, n, natts, 0)
    }

    /// A run of `cap ≥ n` rows with `flags` (`TOPK_TIES`).
    /// # Safety
    /// `p` points at `bytes_cap(cap, natts)` writable bytes, 8-aligned, zeroed or a live run of that shape.
    pub unsafe fn at_cap<'a>(p: *mut u8, n: u32, cap: u32, natts: u32, flags: u32) -> &'a mut TopK {
        let t = aligned_mut::<TopK>(p);
        if t.n == 0 {
            assert!(n > 0, "a top-k run keeps at least one row");
            if cap < n {
                crate::limits::exceeded(format_args!("top-k run capacity: {cap} rows for a LIMIT of {n}"));
            }
            t.n = n;
            t.natts = natts;
            t.cap = cap;
            t.flags = flags;
        }
        assert!(t.n == n && t.natts == natts && t.cap == cap && t.flags == flags, "top-k run shape");
        t
    }

    /// A live run read back (a partial describes itself).
    /// # Safety
    /// `p` points at a run `at` initialised.
    pub unsafe fn view<'a>(p: *const u8) -> &'a TopK {
        aligned_ref::<TopK>(p)
    }

    pub const fn ties(&self) -> bool {
        self.flags & TOPK_TIES != 0
    }

    fn values(&self) -> *const u64 {
        unsafe { (self as *const TopK as *const u8).add(size_of::<TopK>()) as *const u64 }
    }

    fn nulls(&self) -> *const u8 {
        unsafe { (self.values() as *const u8).add(self.cap as usize * self.natts as usize * 8) }
    }

    /// The cells for a writer, from `&mut self` (D1358: a write through `values`' pointer, a shared borrow of the run, is
    /// undefined behaviour the optimizer may act on).
    fn cells_mut(&mut self) -> (*mut u64, *mut u8) {
        let n = self.cap as usize * self.natts as usize * 8;
        let v = unsafe { (self as *mut TopK as *mut u8).add(size_of::<TopK>()) };
        (v as *mut u64, unsafe { v.add(n) })
    }

    pub fn row(&self, i: usize) -> (&[u64], &[u8]) {
        let w = self.natts as usize;
        unsafe { (core::slice::from_raw_parts(self.values().add(i * w), w), core::slice::from_raw_parts(self.nulls().add(i * w), w)) }
    }

    pub fn key_of(&self, i: usize) -> i64 {
        self.row(i).0[0] as i64
    }

    /// The key words of a row: `1 + TOPK_PAIRS`.
    pub const fn key_words(&self) -> usize {
        1 + (self.flags & TOPK_PAIRS != 0) as usize
    }

    /// The key of `values` as its order tuple: `(word as i64, 0)`, or the pair `(flags as i64, word)` under `TOPK_PAIRS`.
    pub fn key_tuple(&self, values: &[u64]) -> (i64, u64) {
        (values[0] as i64, if self.flags & TOPK_PAIRS != 0 { values[1] } else { 0 })
    }

    /// The second key of `values` (`TOPK_SECOND`): `(0, word)` — the word unsigned — or the pair `(flags as i64, word)` under `TOPK_SECOND_PAIR`.
    pub fn second_tuple(&self, values: &[u64]) -> (i64, u64) {
        let at = self.key_words();
        if self.flags & TOPK_SECOND_PAIR != 0 {
            (values[at] as i64, values[at + 1])
        } else {
            (0, values[at])
        }
    }

    fn key_at(&self, i: usize) -> (i64, u64) {
        self.key_tuple(self.row(i).0)
    }

    /// The key the run is bounded by once it holds `n` rows (the n-th smallest): a row with a key
    /// past it never enters; without ties a row at it never enters either.
    pub fn bound(&self) -> Option<i64> {
        (self.len >= self.n && self.n > 0).then(|| self.key_of(self.n as usize - 1))
    }

    /// Insert a row (its key `values[0]` as i64) into the sorted run, dropping the largest when full; false when it did not enter.
    pub fn insert(&mut self, values: &[u64], nulls: &[u8]) -> bool {
        self.insert_ties(values, nulls) == Some(true)
    }

    /// `insert` keeping ties at the bound when `TOPK_TIES`: a row whose key equals the n-th key
    /// enters after its equals; a full run makes room by dropping its last row, which with ties is
    /// legal only when that row lies past the bound the insertion leaves (`None` otherwise: the ties alone fill the capacity and the kernel refuses the statement).
    pub fn insert_ties(&mut self, values: &[u64], nulls: &[u8]) -> Option<bool> {
        let w = self.natts as usize;
        let key = self.key_tuple(values);
        let (len, n) = (self.len as usize, self.n as usize);
        let ties = self.ties();
        let by_pair = !ties && self.flags & TOPK_SECOND != 0;
        let second = if by_pair { self.second_tuple(values) } else { (0, 0) };
        let below = |run: &TopK, i: usize| if by_pair { (key, second) < (run.key_at(i), run.second_tuple(run.row(i).0)) } else { key < run.key_at(i) };
        if len >= n && n > 0 {
            let b = self.key_at(n - 1);
            if by_pair {
                if !below(self, n - 1) {
                    return Some(false);
                }
            } else if key > b || (!ties && key == b) {
                return Some(false);
            }
        }
        let mut i = len;
        while i > 0 && below(self, i - 1) {
            i -= 1;
        }
        let full = len == self.cap as usize;
        if full && ties {
            let new_bound = if i + 1 > n { self.key_at(n - 1) } else if i + 1 == n { key } else { self.key_at(n - 2) };
            if self.key_at(len - 1) <= new_bound {
                return None;
            }
        }
        let last = if full { len - 1 } else { len };
        debug_assert!(i <= last, "a full run drops a row past the new bound, so the slot lies below it");
        unsafe {
            let (v, nl) = self.cells_mut();
            if last > i {
                core::ptr::copy(v.add(i * w), v.add((i + 1) * w), (last - i) * w);
                core::ptr::copy(nl.add(i * w), nl.add((i + 1) * w), (last - i) * w);
            }
            core::ptr::copy_nonoverlapping(values.as_ptr(), v.add(i * w), w);
            core::ptr::copy_nonoverlapping(nulls.as_ptr(), nl.add(i * w), w);
        }
        if !full {
            self.len += 1;
        }
        Some(true)
    }

    /// `insert` under the kernel's own order: `less` (the full comparator over every key: a text
    /// key's bytes on an image tie) places the row, the run keeps its `n` best and no ties — a row
    /// not below the n-th row of a full run does not enter; true when it entered.
    pub fn insert_by(&mut self, values: &[u64], nulls: &[u8], mut less: impl FnMut((&[u64], &[u8]), (&[u64], &[u8])) -> bool) -> bool {
        let w = self.natts as usize;
        let (len, n) = (self.len as usize, self.n as usize);
        let full = len >= n;
        if full && !less((values, nulls), self.row(n - 1)) {
            return false;
        }
        let last = if full { n - 1 } else { len };
        let mut i = last;
        while i > 0 && less((values, nulls), self.row(i - 1)) {
            i -= 1;
        }
        unsafe {
            let (v, nl) = self.cells_mut();
            if last > i {
                core::ptr::copy(v.add(i * w), v.add((i + 1) * w), (last - i) * w);
                core::ptr::copy(nl.add(i * w), nl.add((i + 1) * w), (last - i) * w);
            }
            core::ptr::copy_nonoverlapping(values.as_ptr(), v.add(i * w), w);
            core::ptr::copy_nonoverlapping(nulls.as_ptr(), nl.add(i * w), w);
        }
        if !full {
            self.len += 1;
        }
        true
    }

    /// Keep the first `n` rows.
    pub fn truncate(&mut self, n: u32) {
        self.len = self.len.min(n);
    }

    /// Order the rows by `less` (an insertion sort in place).
    pub fn sort_by(&mut self, mut less: impl FnMut((&[u64], &[u8]), (&[u64], &[u8])) -> bool) {
        let w = self.natts as usize;
        let len = self.len as usize;
        for i in 1..len {
            let mut j = i;
            while j > 0 && less(self.row(j), self.row(j - 1)) {
                unsafe {
                    let (v, nl) = self.cells_mut();
                    core::ptr::swap_nonoverlapping(v.add(j * w), v.add((j - 1) * w), w);
                    core::ptr::swap_nonoverlapping(nl.add(j * w), nl.add((j - 1) * w), w);
                }
                j -= 1;
            }
        }
    }

    /// Row `i`'s cells, writable (the leader's code → text render).
    pub fn row_mut(&mut self, i: usize) -> (&mut [u64], &mut [u8]) {
        let w = self.natts as usize;
        let (v, nl) = self.cells_mut();
        unsafe { (core::slice::from_raw_parts_mut(v.add(i * w), w), core::slice::from_raw_parts_mut(nl.add(i * w), w)) }
    }
}

#[cfg(test)]
mod topk_pairs_tests {
    use super::*;

    #[test]
    fn the_pair_orders_flags_then_the_unsigned_word() {
        let mut reg = [0u64; 64];
        let t = unsafe { TopK::at_cap(reg.as_mut_ptr().cast(), 3, 3, 3, TOPK_PAIRS) };
        let ins = |t: &mut TopK, f: u64, w: u64, tag: u64| t.insert(&[f, w, tag], &[0, 0, 0]);
        assert!(ins(t, 2, 0, 1), "a NULL last");
        assert!(ins(t, 1, u64::MAX, 2), "the largest value, before the NULL");
        assert!(ins(t, 1, 7, 3));
        assert_eq!(t.bound(), Some(2), "the first word of row n − 1");
        assert!(!ins(t, 2, 0, 4), "at the bound: does not enter");
        assert!(ins(t, 1, 1 << 63, 5), "below the largest value: the NULL leaves");
        assert!(!ins(t, 1, u64::MAX, 6), "equal to row n − 1: does not enter");
        assert!(ins(t, 0, 0, 7), "a NULL first");
        let tags: [u64; 3] = core::array::from_fn(|i| t.row(i).0[2]);
        assert_eq!(tags, [7, 3, 5]);
    }

    #[test]
    fn the_second_key_as_a_pair_and_the_ties_on_the_pair() {
        let mut reg = [0u64; 64];
        let t = unsafe { TopK::at_cap(reg.as_mut_ptr().cast(), 2, 2, 5, TOPK_SECOND | TOPK_PAIRS | TOPK_SECOND_PAIR) };
        assert!(t.insert(&[1, 5, 1, 9, 1], &[0; 5]));
        assert!(t.insert(&[1, 5, 1, 3, 2], &[0; 5]), "the same key, a smaller second: placed before");
        assert!(t.insert(&[1, 5, 0, 0, 3], &[0; 5]), "a NULL-first second: enters first, the last leaves");
        let tags: [u64; 2] = core::array::from_fn(|i| t.row(i).0[4]);
        assert_eq!(tags, [3, 2]);
        assert!(!t.insert(&[1, 5, 1, 3, 4], &[0; 5]), "equal to row n − 1 on both pairs");
        let mut reg2 = [0u64; 64];
        let tt = unsafe { TopK::at_cap(reg2.as_mut_ptr().cast(), 2, 6, 3, TOPK_TIES | TOPK_PAIRS) };
        assert_eq!(tt.insert_ties(&[1, 4, 1], &[0; 3]), Some(true));
        assert_eq!(tt.insert_ties(&[2, 0, 2], &[0; 3]), Some(true));
        assert_eq!(tt.insert_ties(&[2, 0, 3], &[0; 3]), Some(true), "a tie on the pair (a NULL equals a NULL) stays");
        assert_eq!(tt.insert_ties(&[1, 5, 4], &[0; 3]), Some(true), "below the bound: the NULLs move past n");
        assert_eq!(tt.insert_ties(&[2, 0, 5], &[0; 3]), Some(false), "past the bound (1, 5): does not enter");
        assert_eq!(tt.len, 4);
        let tags: [u64; 4] = core::array::from_fn(|i| tt.row(i).0[2]);
        assert_eq!(tags, [1, 4, 2, 3]);
    }
}

#[cfg(test)]
mod num_add_tests {
    use super::*;

    fn sent(code: i64) -> i128 {
        ((NUM_SENT_HI as i128) << 64) | code as u64 as i128
    }

    #[test]
    fn a_special_stays_exact_across_a_merge_with_finite_sums() {
        let (nan, pinf, ninf) = (sent(1), sent(2), sent(3));
        assert_eq!(num_add(5, -7), -2);
        for s in [nan, pinf, ninf] {
            assert_eq!(num_add(s, 12_345), s);
            assert_eq!(num_add(-(1 << 70), s), s);
            assert_eq!(num_add(s, s), s);
        }
        assert_eq!(num_add(pinf, ninf), nan);
        assert_eq!(num_add(nan, pinf), nan);
        assert_eq!(num_add(ninf, nan), nan);
    }

    fn w(v: i128) -> NumWide {
        [v as u64, (v >> 64) as u64, (v >> 127) as u64, (v >> 127) as u64]
    }

    /// V4R-38: `num_add256` adds exactly across all four words (carries, signs); past |2^254| a finite sum is the sticky
    /// OVERFLOW code (never a wrap, never a value on the sentinel word); a special wins over a finite sum as `num_add`'s.
    #[test]
    fn num_add256_is_exact_and_sticky() {
        assert_eq!(num_add256(w(5), w(-7)), w(-2));
        assert_eq!(num_add256(w(i128::MAX), w(1)), [0, 1 << 63, 0, 0], "a carry into the third word");
        assert_eq!(num_add256(num_add256(w(i128::MAX), w(1)), w(-1)), w(i128::MAX));
        assert_eq!(num_add256(w(i128::MIN), w(-1)), [u64::MAX, (1 << 63) - 1, u64::MAX, u64::MAX]);
        let top = [0, 0, 0, (1 << 62) - 1];
        let ovf = [NUM_SP_OVERFLOW as u64, 0, 0, NUM_SENT_HI as u64];
        assert_eq!(num_add256(top, [0, 0, 0, 0]), top);
        assert_eq!(num_add256(top, [0, 0, 0, 1]), ovf, "2^254 is past the range");
        assert_eq!(num_add256([0, 0, 0, (-(1i64 << 62)) as u64], [u64::MAX, u64::MAX, u64::MAX, u64::MAX]), ovf);
        assert_eq!(num_add256(ovf, w(12)), ovf);
        let sent = |c: i64| [c as u64, 0, 0, NUM_SENT_HI as u64];
        assert_eq!(num_add256(w(-3), sent(2)), sent(2));
        assert_eq!(num_add256(sent(2), sent(3)), sent(1));
        assert_eq!(num_add256(ovf, sent(1)), sent(1));
    }
}

