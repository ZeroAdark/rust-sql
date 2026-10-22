//! Tier A over the timestamp lane (expr-compiler.md §2.10): `extract(unit FROM ts)` /
//! `date_part(unit, ts)` / `date_trunc(unit, ts)` as quotient forms over the microsecond word — the
//! kernels' `minute_q` shape — with PostgreSQL's `timestamp2tm` / `j2date` arithmetic, the BC year and
//! the infinities; every division by a constant is a Granlund–Montgomery multiply (the 64-bit high
//! product one `umulh`, IntBinOp::Umulh).

use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value};

use super::{from_float, Out};
use crate::nodes::func::Call;
use crate::nodes::{ExprNode, NodeId, Program};

const USECS_PER_SEC: i64 = 1_000_000;
const USECS_PER_MINUTE: i64 = 60 * USECS_PER_SEC;
const USECS_PER_HOUR: i64 = 60 * USECS_PER_MINUTE;
const USECS_PER_DAY: i64 = 24 * USECS_PER_HOUR;
const POSTGRES_EPOCH_JDATE: i64 = 2_451_545;
/// PostgreSQL's `MIN_TIMESTAMP` (4714-11-24 BC, Julian day 0): the bias under which every finite
/// timestamp is a non-negative 64-bit word whose unsigned quotient by a unit is the floor quotient.
pub const MIN_TS: i64 = -POSTGRES_EPOCH_JDATE * USECS_PER_DAY;
/// D3519: the split's bias, `LOW_DAYS` below `MIN_TS`. A timestamptz's local time (lowerv4 expr_fuse `fused`: the word
/// plus its zone offset) reaches below the range's first day in a zone west of UTC; PostgreSQL's `timestamp2tm` still
/// gives its fields (`pg_localtime`), so the split's Julian day goes down to `-LOW_DAYS` (j2date's 32044 shift keeps it
/// positive). A timestamp's own words never go below `MIN_TS`.
const LOW_DAYS: i64 = 2;
const LOW_TS: i64 = MIN_TS - LOW_DAYS * USECS_PER_DAY;
/// A whole number of weeks above `-date2j(-4714, 1, 4)` (689): `iso_start`'s `j2day` of the previous year of the range's
/// first year, kept non-negative for the unsigned remainder.
const ISO_BIAS: i64 = 7 * 128;
const TEXTOID: u32 = 25;
/// D3149: the integer word of a monotonic unit's field of an infinite source (PostgreSQL's ±Infinity there): the int4
/// range's ends, which no finite field reaches (a date's years lie within 4714 BC .. 5874897 AD, a timestamp's within
/// 294277 AD), so an int4 key image keeps them apart from every finite field and orders them as numeric's ±Infinity. A
/// caller that renders the word back as the numeric maps them to the packed sentinels (lowerv4 emit_key `KeyOut::NumInf`).
pub const WORD_NINF: i64 = i32::MIN as i64;
pub const WORD_PINF: i64 = i32::MAX as i64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unit {
    Microseconds,
    Milliseconds,
    Second,
    Minute,
    Hour,
    Day,
    Month,
    Quarter,
    Year,
    Dow,
    Isodow,
    Doy,
    Decade,
    Century,
    Millennium,
    /// The ISO 8601 week and week-numbering year (`date2isoweek` / `date2isoyear`): `extract` only.
    Week,
    Isoyear,
}

impl Unit {
    /// PostgreSQL's monotonic units: an infinite timestamp's field is ±Infinity (the others': NULL).
    pub fn monotonic(self) -> bool {
        matches!(self, Unit::Year | Unit::Decade | Unit::Century | Unit::Millennium | Unit::Isoyear)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// `extract_timestamp`: numeric; served as the integer word at the root (`RowRefs::word_root`), under an integer cast
    /// and in a compare with an integral constant (emit/fuse.rs).
    Extract,
    /// `timestamp_part`: float8.
    Part,
    /// `timestamp_trunc`: timestamp.
    Trunc,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DtForm {
    pub kind: Kind,
    pub unit: Unit,
    /// B2-31: the argument is a date (`extract_date`): its word is the day number (i32, `DATEVAL_NOBEGIN` / `NOEND` the
    /// infinities), read as its Julian day; the fields are PostgreSQL's date.c `extract_date`'s, the same formulas over
    /// the same `j2date` as a timestamp's day part, over the date's wider day range.
    pub date: bool,
}

fn kind_of(name: &str) -> Option<(Kind, bool)> {
    Some(match name {
        "extract_timestamp" => (Kind::Extract, false),
        "extract_date" => (Kind::Extract, true),
        "timestamp_part" => (Kind::Part, false),
        "timestamp_trunc" => (Kind::Trunc, false),
        _ => return None,
    })
}

/// The units date.c `extract_date` serves with an integer this form computes (a time-of-day unit raises there: PostgreSQL's
/// "unit not supported for type date" stays the call's).
fn date_unit(u: Unit) -> bool {
    matches!(u, Unit::Day | Unit::Month | Unit::Quarter | Unit::Year | Unit::Decade | Unit::Century | Unit::Millennium | Unit::Dow | Unit::Isodow | Unit::Doy | Unit::Week | Unit::Isoyear)
}

/// The unit PostgreSQL's `DecodeUnits` reads (the constant lowercased); the fractional fields serve
/// `date_trunc` only, the calendar-only fields the integral forms only.
fn unit_of(kind: Kind, s: &[u8]) -> Option<Unit> {
    let mut low = [0u8; 16];
    if s.len() > low.len() {
        return None;
    }
    for (d, c) in low.iter_mut().zip(s) {
        *d = c.to_ascii_lowercase();
    }
    let integral = kind != Kind::Trunc;
    Some(match &low[..s.len()] {
        b"microseconds" => Unit::Microseconds,
        b"milliseconds" if !integral => Unit::Milliseconds,
        b"second" if !integral => Unit::Second,
        b"minute" => Unit::Minute,
        b"hour" => Unit::Hour,
        b"day" => Unit::Day,
        b"month" => Unit::Month,
        b"quarter" => Unit::Quarter,
        b"year" => Unit::Year,
        b"dow" if integral => Unit::Dow,
        b"isodow" if integral => Unit::Isodow,
        b"doy" if integral => Unit::Doy,
        b"decade" if integral => Unit::Decade,
        b"century" if integral => Unit::Century,
        b"millennium" if integral => Unit::Millennium,
        b"week" if kind == Kind::Extract => Unit::Week,
        b"isoyear" if kind == Kind::Extract => Unit::Isoyear,
        _ => return None,
    })
}

/// The form of a datetime site: a non-null text constant unit the forms serve; an `Extract`
/// (numeric) only at the root the caller reads as the integer word (`RowRefs::word_root`).
pub fn form_of(p: &Program<'_>, call: &Call, name: &str, word_root: bool) -> Option<DtForm> {
    let (kind, date) = kind_of(name)?;
    if call.nargs != 2 || (kind == Kind::Extract && !word_root) {
        return None;
    }
    let ExprNode::Const(c) = &p.nodes[p.kids[call.args.start as usize].0 as usize] else { return None };
    if c.is_null() || c.typ != TEXTOID {
        return None;
    }
    let unit = crate::percode::with_const_bytes(c, &mut |b| unit_of(kind, b))?;
    (!date || date_unit(unit)).then_some(DtForm { kind, unit, date })
}

/// B2-31: the node a word root's integer word is read at (`RowRefs::word_root`): the root itself, or the one argument of
/// a root `int4(numeric)` / `int8(numeric)` cast (`extract(hour FROM ts)::int`), whose value is that word: the caller
/// takes such a root only over an integral, non-monotonic field, whose every value fits int4, so the cast neither rounds
/// nor raises. `true`: the root is that cast.
pub fn word_node(p: &Program<'_>, root: NodeId) -> (NodeId, bool) {
    if let ExprNode::Func(n) = &p.nodes[root.0 as usize] {
        let call = &p.sites[n.site.0 as usize];
        if call.nargs == 1 && matches!(gravity_fn::row_of(call.funcid).map(|r| r.name), Some("numeric_int4" | "numeric_int8")) {
            return (p.kids[call.args.start as usize], true);
        }
    }
    (root, false)
}

/// `floor(x / d)` for `0 ≤ x < 2^bits` as `(x · m) >> s` in one 64-bit product (Granlund–Montgomery,
/// the round-up form): `m = ceil(2^s / d)` with `m · d − 2^s ≤ 2^(s − bits)` and `m ≤ 2^(64 − bits)`.
fn magic(d: u64, bits: u32) -> Option<(u64, u32)> {
    (bits..64).find_map(|s| {
        let m = (1u128 << s).div_ceil(d as u128);
        let e = m * d as u128 - (1u128 << s);
        (e <= 1u128 << (s - bits) && m <= 1u128 << (64 - bits)).then_some((m as u64, s))
    })
}

/// `floor(x / d)` for `0 ≤ x < 2^bits` as `umulh(x, m) >> (s − 64)`: the same bound on the error with
/// `m < 2^64` and `s ≥ 64`.
fn magic_hi(d: u64, bits: u32) -> Option<(u64, u32)> {
    (64..128).find_map(|s| {
        let m = (1u128 << s).div_ceil(d as u128);
        let e = m * d as u128 - (1u128 << s);
        (m < 1u128 << 64 && e <= 1u128 << (s - bits)).then_some((m as u64, s))
    })
}

struct Dt<'a, 'b> {
    b: &'a mut FunctionBuilder<'b>,
    /// B2-31: Julian days and years of a date's range (jd < 2^31, years < 2^23) rather than a timestamp's (2^27, 2^19).
    wide: bool,
}

impl Dt<'_, '_> {
    /// The bit bound of a quotient operand that grows with the Julian day or the year: a timestamp's `b`, a date's 5 more.
    fn jb(&self, b: u32) -> u32 {
        if self.wide { b + 5 } else { b }
    }

    fn k(&mut self, v: i64) -> Value {
        self.b.iconst(Type::I64, v)
    }

    fn bin(&mut self, op: IntBinOp, x: Value, y: Value) -> Value {
        self.b.int_bin(op, x, y)
    }

    fn add(&mut self, x: Value, y: Value) -> Value {
        self.bin(IntBinOp::Add, x, y)
    }

    fn sub(&mut self, x: Value, y: Value) -> Value {
        self.bin(IntBinOp::Sub, x, y)
    }

    fn addk(&mut self, x: Value, k: i64) -> Value {
        let k = self.k(k);
        self.add(x, k)
    }

    fn mulk(&mut self, x: Value, k: i64) -> Value {
        let k = self.k(k);
        self.bin(IntBinOp::Mul, x, k)
    }

    fn ushr(&mut self, x: Value, s: u32) -> Value {
        if s == 0 {
            return x;
        }
        let s = self.k(s as i64);
        self.bin(IntBinOp::Ushr, x, s)
    }

    /// The high 64 bits of `x · m` (`m` a constant): one `umulh`.
    fn umulh(&mut self, x: Value, m: u64) -> Value {
        let m = self.k(m as i64);
        self.bin(IntBinOp::Umulh, x, m)
    }

    /// `floor(x / d)` for `0 ≤ x < 2^bits` (`d` a positive constant): one 64-bit product.
    fn udiv(&mut self, x: Value, d: u64, bits: u32) -> Value {
        if d.is_power_of_two() {
            return self.ushr(x, d.trailing_zeros());
        }
        // past one product's reach (a date's day range): the high product over the operand's own bound
        let Some((m, s)) = magic(d, bits) else {
            let (m, s) = magic_hi(d, bits).unwrap_or_else(|| panic!("no high magic for {d} over {bits} bits"));
            let hi = self.umulh(x, m);
            return self.ushr(hi, s - 64);
        };
        let p = self.mulk(x, m as i64);
        self.ushr(p, s)
    }

    /// `floor(x / d)` for any 64-bit word `x` (`d` a positive constant): the power-of-two part
    /// shifted out, the odd part by the high product.
    fn udiv64(&mut self, x: Value, d: u64) -> Value {
        let k = d.trailing_zeros();
        let x = self.ushr(x, k);
        let d = d >> k;
        if d == 1 {
            return x;
        }
        let (m, s) = magic_hi(d, 64 - k).unwrap_or_else(|| panic!("no high magic for {d} over {} bits", 64 - k));
        let hi = self.umulh(x, m);
        self.ushr(hi, s - 64)
    }

    fn urem(&mut self, x: Value, d: u64, bits: u32) -> Value {
        let q = self.udiv(x, d, bits);
        let qd = self.mulk(q, d as i64);
        self.sub(x, qd)
    }

    /// The finite timestamp as days since Julian day 0 (at least `-LOW_DAYS`) and its microseconds into the day
    /// (non-negative): PostgreSQL's `timestamp2tm` split (`LOW_TS` is a whole number of days).
    fn split_day(&mut self, ts: Value) -> (Value, Value) {
        let x = self.addk(ts, -LOW_TS);
        let q = self.udiv64(x, USECS_PER_DAY as u64);
        let qd = self.mulk(q, USECS_PER_DAY);
        (self.addk(q, -LOW_DAYS), self.sub(x, qd))
    }

    /// `ts mod d` in `0 ≤ r < d` for a unit `d` dividing `LOW_TS` (the floor remainder of the signed word).
    fn rem_euclid(&mut self, ts: Value, d: i64) -> Value {
        debug_assert_eq!(LOW_TS % d, 0);
        let x = self.addk(ts, -LOW_TS);
        let q = self.udiv64(x, d as u64);
        let qd = self.mulk(q, d);
        self.sub(x, qd)
    }

    /// `(year, month, day)` of the Julian day `jd` (`-LOW_DAYS ≤ jd < 2^27`, a date's `2^31` under `wide`), PostgreSQL's `j2date`; the year is
    /// astronomical (0 = 1 BC).
    fn j2date(&mut self, jd: Value) -> (Value, Value, Value) {
        let julian = self.addk(jd, 32044);
        let quad = self.udiv(julian, 146_097, self.jb(28));
        let qd = self.mulk(quad, 146_097);
        let rest = self.sub(julian, qd);
        let rest4 = self.mulk(rest, 4);
        let extra = self.addk(rest4, 3);
        let ex = self.udiv(extra, 146_097, 20);
        let q3 = self.mulk(quad, 3);
        let j2 = self.addk(julian, 60);
        let j2 = self.add(j2, q3);
        let j2 = self.add(j2, ex);
        let quad2 = self.udiv(j2, 1461, self.jb(28));
        let q2d = self.mulk(quad2, 1461);
        let j3 = self.sub(j2, q2d);
        let j34 = self.mulk(j3, 4);
        let y = self.udiv(j34, 1461, 13);
        let a = self.addk(j3, 305);
        let a = self.urem(a, 365, 11);
        let c = self.addk(j3, 306);
        let c = self.urem(c, 366, 11);
        let zero = self.k(0);
        let y0 = self.b.icmp(IntCond::Eq, y, zero);
        let j4 = self.b.select(y0, c, a);
        let j4 = self.addk(j4, 123);
        let q24 = self.mulk(quad2, 4);
        let y2 = self.add(y, q24);
        let year = self.addk(y2, -4800);
        let m2141 = self.mulk(j4, 2141);
        let quad3 = self.ushr(m2141, 16);
        let d7834 = self.mulk(quad3, 7834);
        let d7834 = self.ushr(d7834, 8);
        let day = self.sub(j4, d7834);
        let m10 = self.addk(quad3, 10);
        let m10 = self.urem(m10, 12, 5);
        let month = self.addk(m10, 1);
        (year, month, day)
    }

    /// The Julian day of `(year, month, 1)`, PostgreSQL's `date2j` (`year ≥ −4713`, `1 ≤ month ≤ 12`).
    fn date2j_first(&mut self, year: Value, month: Value) -> Value {
        let two = self.k(2);
        let late = self.b.icmp(IntCond::Sgt, month, two);
        let m1 = self.addk(month, 1);
        let m13 = self.addk(month, 13);
        let m = self.b.select(late, m1, m13);
        let y1 = self.addk(year, 4800);
        let y2 = self.addk(year, 4799);
        let y = self.b.select(late, y1, y2);
        let century = self.udiv(y, 100, self.jb(20));
        let y365 = self.mulk(y, 365);
        let j = self.addk(y365, -32167);
        let y4 = self.ushr(y, 2);
        let j = self.add(j, y4);
        let j = self.sub(j, century);
        let c4 = self.ushr(century, 2);
        let j = self.add(j, c4);
        let mm = self.mulk(m, 7834);
        let mm = self.ushr(mm, 8);
        let j = self.add(j, mm);
        self.addk(j, 1)
    }

    /// PostgreSQL's rounding of the astronomical year into its decade / century / millennium (the BC branches).
    fn era(&mut self, year: Value, unit: Unit) -> Value {
        let d = match unit {
            Unit::Decade => 10,
            Unit::Century => 100,
            _ => 1000,
        };
        let zero = self.k(0);
        let (pos_bias, neg_bias, cond) = match unit {
            Unit::Decade => (0, 9, self.b.icmp(IntCond::Sge, year, zero)),
            Unit::Century => (99, 100, self.b.icmp(IntCond::Sgt, year, zero)),
            _ => (999, 1000, self.b.icmp(IntCond::Sgt, year, zero)),
        };
        let p = self.addk(year, pos_bias);
        let p = self.udiv(p, d, self.jb(20));
        let nb = self.k(neg_bias);
        let n = self.sub(nb, year);
        let n = self.udiv(n, d, self.jb(20));
        let n = self.sub(zero, n);
        self.b.select(cond, p, n)
    }

    /// D3149: a monotonic unit's word: `value` where the source `x` is finite, else `WORD_NINF` where `x` is `nobegin` (the
    /// type's -infinity) and `WORD_PINF` otherwise (its +infinity).
    fn inf_word(&mut self, finite: Value, x: Value, nobegin: i64, value: Value) -> Value {
        let lo = self.k(nobegin);
        let neg = self.b.icmp(IntCond::Eq, x, lo);
        let (ninf, pinf) = (self.k(WORD_NINF), self.k(WORD_PINF));
        let inf = self.b.select(neg, ninf, pinf);
        self.b.select(finite, value, inf)
    }

    fn finite(&mut self, ts: Value) -> Value {
        let lo = self.k(i64::MIN);
        let hi = self.k(i64::MAX);
        let a = self.b.icmp(IntCond::Ne, ts, lo);
        let c = self.b.icmp(IntCond::Ne, ts, hi);
        self.b.int_bin(IntBinOp::And, a, c)
    }

    /// The integer field of a finite `ts` (`part_units_common`).
    fn field(&mut self, ts: Value, unit: Unit) -> Value {
        match unit {
            Unit::Microseconds => self.rem_euclid(ts, USECS_PER_MINUTE),
            Unit::Minute => {
                let r = self.rem_euclid(ts, USECS_PER_HOUR);
                let x = self.ushr(r, 8);
                self.udiv(x, (USECS_PER_MINUTE >> 8) as u64, 24)
            }
            Unit::Hour => {
                let (_, r) = self.split_day(ts);
                let x = self.ushr(r, 10);
                self.udiv(x, (USECS_PER_HOUR >> 10) as u64, 27)
            }
            Unit::Milliseconds | Unit::Second => unreachable!("a fractional field"),
            unit => {
                let (jd, _) = self.split_day(ts);
                self.day_field(jd, unit)
            }
        }
    }

    /// The calendar field of Julian day `jd` (`unit` neither a time-of-day nor a fractional one).
    fn day_field(&mut self, jd: Value, unit: Unit) -> Value {
        if matches!(unit, Unit::Dow | Unit::Isodow) {
            let j1 = self.addk(jd, 1 + 7 * LOW_DAYS);
            let dow = self.urem(j1, 7, self.jb(28));
            if unit == Unit::Dow {
                return dow;
            }
            let zero = self.k(0);
            let seven = self.k(7);
            let sunday = self.b.icmp(IntCond::Eq, dow, zero);
            return self.b.select(sunday, seven, dow);
        }
        let (year, month, day) = self.j2date(jd);
        match unit {
            Unit::Day => day,
            Unit::Month => month,
            Unit::Year => {
                // there is no year 0: 1 BC (astronomical 0) is -1
                let zero = self.k(0);
                let ad = self.b.icmp(IntCond::Sgt, year, zero);
                let bc = self.addk(year, -1);
                self.b.select(ad, year, bc)
            }
            Unit::Quarter => {
                let m = self.addk(month, -1);
                let q = self.udiv(m, 3, 4);
                self.addk(q, 1)
            }
            Unit::Doy => {
                let one = self.k(1);
                let j11 = self.date2j_first(year, one);
                let d = self.sub(jd, j11);
                self.addk(d, 1)
            }
            Unit::Week | Unit::Isoyear => self.iso_field(jd, year, unit),
            _ => self.era(year, unit),
        }
    }

    /// `date2isoweek`'s `day4 - day0` of astronomical `year`: `day4 = date2j(year, 1, 4)`, `day0 = j2day(day4 - 1)` (the
    /// floor remainder, through `ISO_BIAS`: `day4` of the range's first years lies below Julian day 0), the Monday of
    /// the year's ISO week 1.
    fn iso_start(&mut self, year: Value) -> Value {
        let y = self.addk(year, 4799);
        let century = self.udiv(y, 100, self.jb(20));
        let y365 = self.mulk(y, 365);
        let j = self.addk(y365, -32167 + (7834 * 14 >> 8) + 4);
        let y4 = self.ushr(y, 2);
        let j = self.add(j, y4);
        let j = self.sub(j, century);
        let c4 = self.ushr(century, 2);
        let day4 = self.add(j, c4);
        let biased = self.addk(day4, ISO_BIAS);
        let day0 = self.urem(biased, 7, self.jb(28));
        self.sub(day4, day0)
    }

    /// `date2isoweek` (`Unit::Week`) or `date2isoyear` (`Unit::Isoyear`, with `extract`'s BC adjustment) of Julian day `jd`
    /// in astronomical `year`: a day before `iso_start(year)` is in the previous year's last week, one on or after
    /// `iso_start(year + 1)` in the next year's week 1.
    fn iso_field(&mut self, jd: Value, year: Value, unit: Unit) -> Value {
        let s0 = self.iso_start(year);
        let yp = self.addk(year, -1);
        let yn = self.addk(year, 1);
        let sn = self.iso_start(yn);
        let before = self.b.icmp(IntCond::Slt, jd, s0);
        let after = self.b.icmp(IntCond::Sge, jd, sn);
        if unit == Unit::Isoyear {
            let y = self.b.select(before, yp, year);
            let y = self.b.select(after, yn, y);
            let zero = self.k(0);
            let ad = self.b.icmp(IntCond::Sgt, y, zero);
            let bc = self.addk(y, -1);
            return self.b.select(ad, y, bc);
        }
        let sp = self.iso_start(yp);
        let start = self.b.select(before, sp, s0);
        let d = self.sub(jd, start);
        let w = self.udiv(d, 7, 9);
        let w = self.addk(w, 1);
        let one = self.k(1);
        self.b.select(after, one, w)
    }

    /// `ts` floored to the unit (finite `ts`).
    fn trunc(&mut self, ts: Value, unit: Unit) -> Value {
        let d = match unit {
            Unit::Microseconds => return ts,
            Unit::Milliseconds => 1000,
            Unit::Second => USECS_PER_SEC,
            Unit::Minute => USECS_PER_MINUTE,
            Unit::Hour => USECS_PER_HOUR,
            Unit::Day => USECS_PER_DAY,
            _ => {
                let (jd, _) = self.split_day(ts);
                let (year, month, _) = self.j2date(jd);
                let m = match unit {
                    Unit::Month => month,
                    Unit::Quarter => {
                        let m = self.addk(month, -1);
                        let q = self.udiv(m, 3, 4);
                        let q3 = self.mulk(q, 3);
                        self.addk(q3, 1)
                    }
                    _ => self.k(1),
                };
                let j = self.date2j_first(year, m);
                let days = self.addk(j, -POSTGRES_EPOCH_JDATE);
                return self.mulk(days, USECS_PER_DAY);
            }
        };
        let r = self.rem_euclid(ts, d);
        self.sub(ts, r)
    }
}

/// The site's value and, when the form decides it, its validity; only a calendar-unit `date_trunc`
/// of the range's first days raises (PostgreSQL's "timestamp out of range"). An infinite
/// timestamp truncates to itself; its `date_part` is NULL (an oscillating unit) or ±Infinity (a
/// monotonic one); its `extract` word is NULL (an oscillating unit: the key form is invalid there) or,
/// D3149, `WORD_NINF` / `WORD_PINF` (a monotonic one: ±Infinity, valid).
pub fn emit(b: &mut FunctionBuilder<'_>, form: DtForm, ts: Value) -> (Out, Option<Value>) {
    if form.date {
        return emit_date(b, form, ts);
    }
    let mut dt = Dt { b, wide: false };
    let finite = dt.finite(ts);
    let err = super::no_raise(dt.b);
    match form.kind {
        Kind::Trunc => {
            let t = dt.trunc(ts, form.unit);
            let value = dt.b.select(finite, t, ts);
            if !matches!(form.unit, Unit::Month | Unit::Quarter | Unit::Year) {
                return (Out { value, err }, None);
            }
            // a calendar unit's floor of the range's first days leaves the range: `tm2timestamp` raises
            let min = dt.k(MIN_TS);
            let low = dt.b.icmp(IntCond::Slt, t, min);
            let bad = dt.b.int_bin(IntBinOp::And, finite, low);
            let err = super::raise(dt.b, bad, gravity_abi::status::raise::TS_RANGE);
            (Out { value, err }, None)
        }
        Kind::Extract => {
            let value = dt.field(ts, form.unit);
            if !form.unit.monotonic() {
                return (Out { value, err }, Some(finite));
            }
            // D3149: a monotonic unit's field of an infinite timestamp is ±Infinity, the word's ends
            let value = dt.inf_word(finite, ts, i64::MIN, value);
            (Out { value, err }, None)
        }
        Kind::Part => {
            let n = dt.field(ts, form.unit);
            let f = dt.b.conv(ConvOp::SintToFloat, Type::F64, n);
            let value = from_float(dt.b, f, 8);
            if !form.unit.monotonic() {
                return (Out { value, err }, Some(finite));
            }
            let lo = dt.k(i64::MIN);
            let nobegin = dt.b.icmp(IntCond::Eq, ts, lo);
            let ninf = dt.k(f64::NEG_INFINITY.to_bits() as i64);
            let pinf = dt.k(f64::INFINITY.to_bits() as i64);
            let inf = dt.b.select(nobegin, ninf, pinf);
            let value = dt.b.select(finite, value, inf);
            (Out { value, err }, None)
        }
    }
}

/// Whether timestamp word `ts` is finite (neither `DT_NOBEGIN`, `i64::MIN`, nor `DT_NOEND`, `i64::MAX`).
pub fn finite(b: &mut FunctionBuilder<'_>, ts: Value) -> Value {
    Dt { b, wide: false }.finite(ts)
}

/// `form`'s source word `x` finite, and `x` its type's -infinity: a date's `DATEVAL_NOBEGIN` / `NOEND` (the i32 ends of
/// its low word), a timestamp's `DT_NOBEGIN` / `DT_NOEND`.
pub fn source_inf(b: &mut FunctionBuilder<'_>, form: DtForm, x: Value) -> (Value, Value) {
    let (x, lo, hi) = if form.date {
        let d = b.conv(ConvOp::Trunc, Type::I32, x);
        (b.conv(ConvOp::Sext, Type::I64, d), i32::MIN as i64, i32::MAX as i64)
    } else {
        (x, i64::MIN, i64::MAX)
    };
    let (l, h) = (b.iconst(Type::I64, lo), b.iconst(Type::I64, hi));
    let a = b.icmp(IntCond::Ne, x, l);
    let c = b.icmp(IntCond::Ne, x, h);
    let nobegin = b.icmp(IntCond::Eq, x, l);
    (b.int_bin(IntBinOp::And, a, c), nobegin)
}

/// B2-31: `extract(unit FROM d)` of a date word `d` (date.c `extract_date`): the integer field of its Julian day, valid
/// where `d` is finite (an infinite date's field of an oscillating unit is NULL); D3149: a monotonic unit's field of an
/// infinite date is ±Infinity, the word `WORD_NINF` / `WORD_PINF`, valid.
fn emit_date(b: &mut FunctionBuilder<'_>, form: DtForm, word: Value) -> (Out, Option<Value>) {
    debug_assert!(form.kind == Kind::Extract && date_unit(form.unit));
    let d = b.conv(ConvOp::Trunc, Type::I32, word);
    let d = b.conv(ConvOp::Sext, Type::I64, d);
    let mut dt = Dt { b, wide: true };
    let (lo, hi) = (dt.k(i32::MIN as i64), dt.k(i32::MAX as i64));
    let a = dt.b.icmp(IntCond::Ne, d, lo);
    let c = dt.b.icmp(IntCond::Ne, d, hi);
    let finite = dt.b.int_bin(IntBinOp::And, a, c);
    // an infinite date's Julian day is never read (its field is masked); clamp it so every quotient stays in range
    let zero = dt.k(-POSTGRES_EPOCH_JDATE);
    let x = dt.b.select(finite, d, zero);
    let jd = dt.addk(x, POSTGRES_EPOCH_JDATE);
    let value = dt.day_field(jd, form.unit);
    let err = super::no_raise(dt.b);
    if !form.unit.monotonic() {
        return (Out { value, err }, Some(finite));
    }
    // D3149: `DATEVAL_NOBEGIN` (i32::MIN) is -Infinity, `DATEVAL_NOEND` +Infinity
    let value = dt.inf_word(finite, d, i32::MIN as i64, value);
    (Out { value, err }, None)
}

/// M3-T1KEY: the unit of a `date_trunc(unit, ts)` call on the surrogate-key road — `timestamp_trunc` by function id
/// (the session time zone enters `timestamptz_trunc`: not this road) over a calendar unit whose cheapest injective,
/// monotone image is an integer word: the Julian day (`day`), `year · 12 + month − 1` (`month`), `year · 4 + quarter − 1`
/// (`quarter`), the year (`year`); the sub-day units truncate in one product already, the eras round through BC.
pub fn surrogate_unit(funcid: u32, unit: &[u8]) -> Option<Unit> {
    if gravity_fn::row_of(funcid)?.name != "timestamp_trunc" {
        return None;
    }
    unit_of(Kind::Trunc, unit).filter(|u| matches!(u, Unit::Day | Unit::Month | Unit::Quarter | Unit::Year))
}

/// The unit of an `extract(unit FROM ts)` call on the computed-key road — `extract_timestamp` by function id (the session
/// time zone enters `extract_timestamptz`: not this road) over an integral unit of `Kind::Extract`'s table: the field is the key's
/// integer word (`emit`), rendered back as the numeric at the emit.
/// B2-31: the unit `extract` serves as an integer over a timestamp's fields (its unit table), for a caller that maps
/// another source onto the timestamp form (lowerv4 expr.fusion: a timestamptz under a fixed session offset).
pub fn extract_timestamp_unit(unit: &[u8]) -> Option<Unit> {
    unit_of(Kind::Extract, unit)
}

pub fn extract_unit(funcid: u32, unit: &[u8]) -> Option<Unit> {
    let (Kind::Extract, date) = kind_of(gravity_fn::row_of(funcid)?.name)? else { return None };
    unit_of(Kind::Extract, unit).filter(|&u| !date || date_unit(u))
}

impl Dt<'_, '_> {
    fn surrogate_finite(&mut self, unit: Unit, ts: Value) -> Value {
        let (jd, _) = self.split_day(ts);
        if unit == Unit::Day {
            return jd;
        }
        let (year, month, _) = self.j2date(jd);
        match unit {
            Unit::Year => year,
            Unit::Quarter => {
                let m = self.addk(month, -1);
                let q = self.udiv(m, 3, 4);
                let y4 = self.mulk(year, 4);
                self.add(y4, q)
            }
            _ => {
                let y12 = self.mulk(year, 12);
                let m = self.addk(month, -1);
                self.add(y12, m)
            }
        }
    }
}

/// The surrogate word of the timestamp `ts` under `unit` (`surrogate_unit`): an infinite timestamp is its own word
/// (no finite image reaches ±2^63), so the word orders as the truncated value does.
pub fn surrogate(b: &mut FunctionBuilder<'_>, unit: Unit, ts: Value) -> Value {
    let mut dt = Dt { b, wide: false };
    let finite = dt.finite(ts);
    let w = dt.surrogate_finite(unit, ts);
    dt.b.select(finite, w, ts)
}

/// `date_trunc(unit, ts)` from its surrogate word: `date2j` of the period's first day back to microseconds, and the
/// raise mask (`TS_RANGE` when the period's first day is before `MIN_TIMESTAMP`: PostgreSQL's `tm2timestamp`).
pub fn surrogate_render(b: &mut FunctionBuilder<'_>, unit: Unit, word: Value) -> (Value, Value) {
    let mut dt = Dt { b, wide: false };
    let finite = dt.finite(word);
    let jd = match unit {
        Unit::Day => word,
        Unit::Year => {
            let one = dt.k(1);
            dt.date2j_first(word, one)
        }
        Unit::Quarter => {
            let (k3, k2) = (dt.k(3), dt.k(2));
            let q = dt.b.int_bin(IntBinOp::And, word, k3);
            let y = dt.b.int_bin(IntBinOp::Sshr, word, k2);
            let m3 = dt.mulk(q, 3);
            let m = dt.addk(m3, 1);
            dt.date2j_first(y, m)
        }
        _ => {
            // `word = y · 12 + (m − 1)` with `y ≥ −4713`: the floor split through a bias keeps both quotients unsigned
            let biased = dt.addk(word, 4800 * 12);
            let y = dt.udiv(biased, 12, 24);
            let y12 = dt.mulk(y, 12);
            let m0 = dt.sub(biased, y12);
            let m = dt.addk(m0, 1);
            let year = dt.addk(y, -4800);
            dt.date2j_first(year, m)
        }
    };
    let days = dt.addk(jd, -POSTGRES_EPOCH_JDATE);
    let t = dt.mulk(days, USECS_PER_DAY);
    let min = dt.k(MIN_TS);
    let low = dt.b.icmp(IntCond::Slt, t, min);
    let bad = dt.b.int_bin(IntBinOp::And, finite, low);
    let err = super::raise(dt.b, bad, gravity_abi::status::raise::TS_RANGE);
    (dt.b.select(finite, t, word), err)
}

/// `surrogate` at compile time (the packed front's bounds from the column's estimate): PostgreSQL's `j2date`.
pub fn surrogate_of(unit: Unit, ts: i64) -> i64 {
    if ts == i64::MIN || ts == i64::MAX {
        return ts;
    }
    let jd = ts.wrapping_sub(MIN_TS) as u64 / USECS_PER_DAY as u64;
    if unit == Unit::Day {
        return jd as i64;
    }
    let mut julian = jd + 32044;
    let quad = julian / 146_097;
    let extra = (julian - quad * 146_097) * 4 + 3;
    julian += 60 + quad * 3 + extra / 146_097;
    let quad = julian / 1461;
    julian -= quad * 1461;
    let y = julian * 4 / 1461;
    julian = if y != 0 { (julian + 305) % 365 } else { (julian + 306) % 366 } + 123;
    let year = (y + quad * 4) as i64 - 4800;
    let month = ((julian * 2141 / 65536 + 10) % 12 + 1) as i64;
    match unit {
        Unit::Year => year,
        Unit::Quarter => year * 4 + (month - 1) / 3,
        _ => year * 12 + month - 1,
    }
}

#[cfg(test)]
mod tests {
    use super::{magic, magic_hi, MIN_TS, USECS_PER_DAY, USECS_PER_HOUR, USECS_PER_MINUTE, USECS_PER_SEC};

    /// Every `(d, bits)` pair the single-product route uses has a magic, exact over a dense sample.
    #[test]
    fn magics_exist_and_divide() {
        for (d, bits) in [(234_375u64, 24u32), (3_515_625, 27), (146_097, 28), (146_097, 20), (1461, 28), (1461, 13), (365, 11), (366, 11), (12, 5), (7, 28), (100, 20), (3, 4), (10, 20), (1000, 20), (7, 9)] {
            let (m, s) = magic(d, bits).unwrap_or_else(|| panic!("no magic for {d} / {bits}"));
            let top = 1u64 << bits;
            let step = (top / 4_000_000).max(1);
            for x in (0..top).step_by(step as usize).chain(top.saturating_sub(1000)..top).chain(0..top.min(1000)) {
                assert_eq!(x.wrapping_mul(m) >> s, x / d, "{d} / {bits} at {x}");
            }
        }
    }

    fn umulh(x: u64, m: u64) -> u64 {
        ((x as u128 * m as u128) >> 64) as u64
    }

    /// B2-31: a date's wide operands (Julian days < 2^33 after the bias, years < 2^24): where one product's magic is out
    /// of reach `udiv` takes the high product over the operand's bound; it exists and divides the range exactly.
    #[test]
    fn wide_divisors_divide() {
        for (d, bits) in [(146_097u64, 33u32), (1461, 33), (146_097, 20), (1461, 13), (365, 11), (366, 11), (7, 33), (100, 24), (100, 25), (10, 24), (1000, 24), (4, 24)] {
            let top = 1u64 << bits;
            let step = (top / 4_000_000).max(1);
            let xs = || (0..top).step_by(step as usize).chain(top.saturating_sub(1000)..top).chain(0..top.min(1000));
            match magic(d, bits) {
                Some((m, s)) => xs().for_each(|x| assert_eq!(x.wrapping_mul(m) >> s, x / d, "{d} / {bits} at {x}")),
                None => {
                    let (m, s) = magic_hi(d, bits).unwrap_or_else(|| panic!("no high magic for {d} / {bits}"));
                    xs().for_each(|x| assert_eq!(umulh(x, m) >> (s - 64), x / d, "{d} / {bits} at {x}"));
                }
            }
        }
    }

    /// The units' high-product magics exist and divide every word of the biased range exactly (a
    /// dense sample, the ends, and the multiples of `d` on both sides).
    #[test]
    fn high_magics_divide_the_biased_range() {
        assert_eq!(MIN_TS, -211_813_488_000_000_000);
        for d in [1000u64, USECS_PER_SEC as u64, USECS_PER_MINUTE as u64, USECS_PER_HOUR as u64, USECS_PER_DAY as u64] {
            assert_eq!(MIN_TS % d as i64, 0);
            let k = d.trailing_zeros();
            let dd = d >> k;
            let (m, s) = magic_hi(dd, 64 - k).unwrap_or_else(|| panic!("no high magic for {dd} / {}", 64 - k));
            let q = |x: u64| umulh(x >> k, m) >> (s - 64);
            let mut x = 0u64;
            let step = u64::MAX / 1_000_003;
            loop {
                assert_eq!(q(x), x / d, "{d} at {x}");
                let (nx, o) = x.overflowing_add(step);
                if o {
                    break;
                }
                x = nx;
            }
            for x in (u64::MAX - 5000)..=u64::MAX {
                assert_eq!(q(x), x / d, "{d} at {x}");
            }
            for j in [1u64, 2, 1000, 1 << 20, (1u64 << 63) / d, u64::MAX / d] {
                for x in (j * d).saturating_sub(2)..=(j * d).saturating_add(2) {
                    assert_eq!(q(x), x / d, "{d} at {x}");
                }
            }
        }
    }
}
