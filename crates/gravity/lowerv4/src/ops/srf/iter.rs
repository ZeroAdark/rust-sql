//! The open-coded iterators (L3; a helper file, no rows of its own; cards/M5.md `srf/iter.rs`, M5-plan §3.1 (a), R8-L01,
//! Decision 4): a set function whose elements are integer arithmetic over its arguments, emitted as IR with no host and no
//! fmgr call per element. The face is pure helpers over IR values, so the step that owns the loop (function_scan.rs'
//! FROM loop, project_set.rs' lockstep) keeps the cursor in its own block parameters and one offset word `k` per row:
//! `open` (once per input row, PG's first call: its checks in PG's order, their raise word and the set's element count
//! `n`), `at(k)` (the cursor of element `k`: 0 at a fresh row, the offset word on a resume, so a resume never re-opens),
//! `value` (the element as the word a `CellForm::Word` cell reads) and `advance` (the next cursor: an add per element).
//! The caller walks `k < n` (unsigned) and leaves on a nonzero raise word with its own carry. Built: `generate_series`
//! over int4 / int8, with and without a step (ladder `series_loop` srf.rs:607, v1 `lower_generated_series`
//! lib.rs:5777). Every other kind is not an iterator form (`plan` answers `None`).
//! Owner M5-SERIES.
// Read by function_scan.rs and project_set.rs (Wave 1b); until they land, only this file's tests call it.
#![cfg_attr(not(test), allow(dead_code))]
use gravity_abi::status::raise;
use gravity_decompose::ops::shared::srf::{SrfFn, SrfKind};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Type, Value};
use mcx::{CollectIn, Mcx, PgVec};

use crate::frame::env::Env;
use crate::frame::px::Px;

/// An iterator form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IterKind {
    /// `generate_series(int4 | int8, same [, same])` (pg_proc 1066-1069): `width` 4 or 8, `step` the three-argument form.
    Series { width: u8, step: bool },
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Iter {
    pub kind: IterKind,
    pub node: i32,
}

/// A set opened on one row's arguments: `n` its element count (`I64`, unsigned, saturated at `u64::MAX`), and what the
/// cursor is made of. Its words (`words`, `Iter::from_words`) are what a caller holds across a stop inside the row, so a
/// resumed row rebuilds it and never re-opens.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Opened {
    pub n: Value,
    start: Value,
    /// The step (`I64`); `None`: the two-argument form's 1.
    step: Option<Value>,
}

impl Opened {
    pub(crate) fn words<'m>(&self, mcx: Mcx<'m>) -> PgVec<'m, Value> {
        [self.n, self.start].into_iter().chain(self.step).collect_in(mcx)
    }
}

/// Function `f`'s iterator form, `None` when it has none (a library, fmgr, PL or scalar call).
pub(crate) fn plan(_px: &mut Px<'_, '_>, f: &SrfFn<'_>, node: i32) -> Option<Iter> {
    form_of(f.kind, f.args.len()).map(|kind| Iter { kind, node })
}

fn form_of(k: SrfKind, nargs: usize) -> Option<IterKind> {
    match k {
        SrfKind::SeriesInt { width, step } if nargs == 2 + step as usize => Some(IterKind::Series { width, step }),
        _ => None,
    }
}

impl Iter {
    /// PG's first call (`generate_series_step_int4` / `_int8`) and its raise word (the gravity_expr mask, a constant 0 when
    /// the form cannot raise; the caller leaves with its own carry). The function is strict, so a NULL argument opens an
    /// empty set with no call and no check; else a zero step raises 22023; else `n` is the arithmetic count: PG ends the
    /// set when the next value overflows, which only happens past `stop`. The arguments are the call's words (`I64`,
    /// integers sign-extended) and NULL flags, in call order. `from`: the set is a FROM entry (every form here raises only
    /// at open, so it reads the same in both places).
    pub(crate) fn open(&self, b: &mut FunctionBuilder<'_>, env: &Env, args: &[(Value, Option<Value>)], _from: bool) -> (Opened, Value) {
        let IterKind::Series { width, step } = self.kind;
        if args.len() != 2 + step as usize {
            crate::refuse!(self.node, crate::admit::refusal::Reason::Bug { payload: "a series opened with another argument count" });
        }
        let null = args.iter().filter_map(|a| a.1).reduce(|x, y| b.int_bin(IntBinOp::Or, x, y));
        let (start, stop, step) = (args[0].0, args[1].0, step.then(|| args[2].0));
        let mut err = env.zero;
        let n = match step {
            None => {
                let empty = b.icmp(IntCond::Sgt, start, stop);
                let d = b.int_bin(IntBinOp::Sub, stop, start);
                let n = count(b, env, d, width);
                b.select(empty, env.zero, n)
            }
            Some(s) => {
                if b.const_of(s).is_none_or(|c| c.1 == 0) {
                    let zero = b.icmp(IntCond::Eq, s, env.zero);
                    let checked = match null {
                        Some(nl) => {
                            let t = b.bconst(true);
                            let valid = b.int_bin(IntBinOp::Xor, nl, t);
                            b.int_bin(IntBinOp::And, zero, valid)
                        }
                        None => zero,
                    };
                    let bit = b.iconst(Type::I64, raise::bit(raise::STEP_ZERO) as i64);
                    err = b.select(checked, bit, env.zero);
                }
                // a descending set walks from `start` down: the distance and the step's size as unsigned words
                let up = b.icmp(IntCond::Sgt, s, env.zero);
                let (hi, lo) = (b.select(up, stop, start), b.select(up, start, stop));
                let empty = b.icmp(IntCond::Slt, hi, lo);
                let d = b.int_bin(IntBinOp::Sub, hi, lo);
                let neg = b.int_bin(IntBinOp::Sub, env.zero, s);
                let mag = b.select(up, s, neg);
                // a zero step (raised above, or a NULL step's zero word: its set is empty below) is never a divisor
                let mag = b.int_bin(IntBinOp::Umax, mag, env.one);
                let q = b.int_bin(IntBinOp::Udiv, d, mag);
                let n = count(b, env, q, width);
                b.select(empty, env.zero, n)
            }
        };
        let n = match null {
            Some(nl) => b.select(nl, env.zero, n),
            None => n,
        };
        (Opened { n, start, step }, err)
    }

    /// How many words `Opened::words` gives.
    pub(crate) fn nwords(&self) -> usize {
        let IterKind::Series { step, .. } = self.kind;
        2 + step as usize
    }

    /// `Opened::words` read back (a resumed row).
    pub(crate) fn from_words(&self, w: &[Value]) -> Opened {
        let IterKind::Series { step, .. } = self.kind;
        Opened { n: w[0], start: w[1], step: step.then(|| w[2]) }
    }

    /// The cursor of element `k` (`I64`, `k < n`): `start + k * step`, exact in wrapping arithmetic because the element
    /// is representable.
    pub(crate) fn at<'m>(&self, b: &mut FunctionBuilder<'_>, o: &Opened, k: Value, mcx: Mcx<'m>) -> PgVec<'m, Value> {
        let off = match o.step {
            Some(s) => b.int_bin(IntBinOp::Mul, k, s),
            None => k,
        };
        mcx::pg_vec![in mcx; b.int_bin(IntBinOp::Add, o.start, off)]
    }

    /// The element at `cur`: its word, NULL flag and raise word (a series element is never NULL and never raises).
    pub(crate) fn value(&self, b: &mut FunctionBuilder<'_>, env: &Env, _o: &Opened, cur: &[Value]) -> (Value, Value, Value) {
        (cur[0], b.bconst(false), env.zero)
    }

    /// The cursor after `cur`.
    pub(crate) fn advance<'e>(&self, b: &mut FunctionBuilder<'_>, env: &Env<'e, '_>, o: &Opened, cur: &[Value]) -> PgVec<'e, Value> {
        mcx::pg_vec![in env.mcx; b.int_bin(IntBinOp::Add, cur[0], o.step.unwrap_or(env.one))]
    }
}

/// `q + 1` elements for a quotient `q`; at width 8 `q` may be `u64::MAX` (`generate_series(-2^63, 2^63 - 1)`), which
/// saturates. At width 4 the distance fits in 33 bits.
fn count(b: &mut FunctionBuilder<'_>, env: &Env, q: Value, width: u8) -> Value {
    let n = b.int_bin(IntBinOp::Add, q, env.one);
    if width < 8 {
        return n;
    }
    let wrapped = b.icmp(IntCond::Eq, n, env.zero);
    b.select(wrapped, q, n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::stop::tests::run1;

    const RAISED: i64 = raise::bit(raise::STEP_ZERO) as i64;

    /// The set of `generate_series(args)` walked by the helpers as a caller's loop does (cursor in block parameters,
    /// `k < n` unsigned), folded as `acc * 31 + element`, and `n`: `(fold, n)`; a raise returns its raise word and 0.
    /// `None` arguments are NULL.
    fn walk(width: u8, args: &[Option<i64>]) -> (i64, i64) {
        let it = Iter { kind: form_of(SrfKind::SeriesInt { width, step: args.len() == 3 }, args.len()).expect("an iterator form"), node: 0 };
        let mut n_seen = 0;
        let fold = |want_n: bool| {
            run1(&[], &mut |b, env| {
                let (l_raised, l_go) = (b.create_block(&[]), b.create_block(&[]));
                let a: Vec<(Value, Option<Value>)> = args.iter().map(|v| (b.iconst(Type::I64, v.unwrap_or(0)), Some(b.bconst(v.is_none())))).collect(); // alloc: Program
                let (o, err) = it.open(b, env, &a, false);
                let raised = b.icmp(IntCond::Ne, err, env.zero);
                b.cond_br(raised, (l_raised, &[]), (l_go, &[]));
                b.switch_to(l_go);
                let o = it.from_words(&o.words(env.mcx));
                let l = b.create_block(&[Type::I64, Type::I64, Type::I64]);
                let (l_body, l_out) = (b.create_block(&[]), b.create_block(&[Type::I64]));
                let c0 = it.at(b, &o, env.zero, env.mcx)[0];
                b.br(l, &[env.zero, c0, env.zero]);
                b.switch_to(l);
                b.mark_loop_header(l);
                let (k, cur, acc) = (b.block_params(l)[0], b.block_params(l)[1], b.block_params(l)[2]);
                let more = b.icmp(IntCond::Ult, k, o.n);
                b.cond_br(more, (l_body, &[]), (l_out, &[acc]));
                b.switch_to(l_body);
                let (v, _, _) = it.value(b, env, &o, &[cur]);
                let m = b.iconst(Type::I64, 31);
                let acc = b.int_bin(IntBinOp::Mul, acc, m);
                let acc = b.int_bin(IntBinOp::Add, acc, v);
                let (k1, next) = (b.int_bin(IntBinOp::Add, k, env.one), it.advance(b, env, &o, &[cur])[0]);
                b.br(l, &[k1, next, acc]);
                b.switch_to(l_out);
                let out = if want_n { o.n } else { b.block_params(l_out)[0] };
                b.ret(&[out]);
                b.switch_to(l_raised);
                err
            })
        };
        let f = fold(false);
        if f != RAISED {
            n_seen = fold(true);
        }
        (f, n_seen)
    }

    /// PG's `generate_series_step_int4` / `_int8` loop over the same words, folded the same way.
    fn pg(width: u8, args: &[Option<i64>]) -> (i64, i64) {
        if args.iter().any(|a| a.is_none()) {
            return (0, 0);
        }
        let (mut cur, stop, mut step) = (args[0].unwrap(), args[1].unwrap(), args.get(2).map_or(1, |s| s.unwrap()));
        if step == 0 {
            return (RAISED, 0);
        }
        let (mut acc, mut n) = (0i64, 0i64);
        while (step > 0 && cur <= stop) || (step < 0 && cur >= stop) {
            (acc, n) = (acc.wrapping_mul(31).wrapping_add(cur), n + 1);
            let next = if width == 4 { (cur as i32).checked_add(step as i32).map(i64::from) } else { cur.checked_add(step) };
            match next {
                Some(v) => cur = v,
                None => step = 0,
            }
        }
        (acc, n)
    }

    #[test]
    fn series_equal_pg_at_the_bounds() {
        let (i4, i8) = (i32::MAX as i64, i64::MAX);
        let cases: &[(u8, &[Option<i64>])] = &[
            (4, &[Some(1), Some(3)]),
            (4, &[Some(3), Some(1)]),
            (4, &[Some(5), Some(5)]),
            (4, &[Some(1), Some(10), Some(3)]),
            (4, &[Some(10), Some(1), Some(-3)]),
            (4, &[Some(1), Some(10), Some(-1)]),
            (4, &[Some(i4 - 2), Some(i4)]),
            (4, &[Some(i4 - 5), Some(i4), Some(4)]),
            (4, &[Some(-i4 - 1 + 3), Some(-i4 - 1), Some(-2)]),
            (4, &[Some(-i4 - 1), Some(i4), Some(i4)]),
            (4, &[Some(i4), Some(-i4 - 1), Some(-i4 - 1)]),
            (4, &[Some(1), Some(3), Some(0)]),
            (4, &[None, Some(3)]),
            (4, &[Some(1), Some(3), None]),
            (4, &[Some(1), None, Some(0)]),
            (8, &[Some(i8 - 3), Some(i8)]),
            (8, &[Some(i8 - 7), Some(i8), Some(3)]),
            (8, &[Some(-i8 - 1), Some(i8), Some(i8)]),
            (8, &[Some(0), Some(-i8 - 1), Some(-i8 - 1)]),
            (8, &[Some(i8), Some(-i8 - 1), Some(-i8 - 1)]),
            (8, &[Some(-5), Some(5), Some(2)]),
            (8, &[Some(1), Some(2), Some(0)]),
        ];
        for &(w, a) in cases {
            assert_eq!(walk(w, a), pg(w, a), "generate_series width {w} {a:?}");
        }
    }

    /// `generate_series(-2^63, 2^63 - 1)` has 2^64 elements: the count saturates instead of reading as an empty set.
    #[test]
    fn a_full_int8_range_saturates() {
        let it = Iter { kind: IterKind::Series { width: 8, step: false }, node: 0 };
        let n = run1(&[], &mut |b, env| {
            let a = [(b.iconst(Type::I64, i64::MIN), None), (b.iconst(Type::I64, i64::MAX), None)];
            it.open(b, env, &a, true).0.n
        });
        assert_eq!(n as u64, u64::MAX);
    }

    /// Does not apply (G4): a numeric, a timestamp or any library / fmgr set is not an iterator form, nor a series whose
    /// call has another argument count.
    #[test]
    fn other_sets_are_not_iterator_forms() {
        for k in [SrfKind::SeriesNumeric { step: false }, SrfKind::SeriesTime { tz: true }, SrfKind::Unnest, SrfKind::Other, SrfKind::Scalar, SrfKind::Expr] {
            assert_eq!(form_of(k, 2), None, "{k:?}");
        }
        assert_eq!(form_of(SrfKind::SeriesInt { width: 4, step: false }, 3), None);
    }
}
