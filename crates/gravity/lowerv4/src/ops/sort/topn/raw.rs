//! topn's raw half (rule R6: `unsafe` lives in the family's raw.rs): the region readers the
//! two-pass protocols register (`RegionFn`), moved here unchanged from strategies/twopass.rs (`each_record`, `Single`,
//! `key_of`, `kept_keys`, `admitted`) and strategies/twopass_bare.rs (`bare_keys`), the V4R-192 tests module
//! (raw/twopass_top_tests.rs, was topn/twopass_top_tests.rs) and D-06c's count top rule (`top_count_admitted`, its tests
//! raw/twopass_count_tests.rs), widened to a sum lead by 5c-F6 (`lead_ranks`, its tests raw/twopass_sum_tests.rs).
#![allow(unsafe_code)]
use gravity_abi::breaker::{table_kind, Owned, OwnerTop, OWNERTOP_HDR, TABLE_OWNED};
use gravity_fn::arena::{Charge, Door, Kind, Owner, Release};
use gravity_kernels::ops::sort::topn::t3;

use crate::ops::shared::twopass::Rule;
use crate::ops::sort::topn::strategies::twopass_bare::P1_WORDS;

/// Every record of a finished generic owner region (`Owned`: each owner's runs behind its `OwnerTop`), `rec` bytes each.
unsafe fn each_record(region: *const u8, rec: usize, f: &mut dyn FnMut(*const u64)) -> Option<usize> {
    if table_kind(region) != TABLE_OWNED {
        return None;
    }
    let o = Owned::view(region);
    let mut n = 0;
    for j in 0..o.nowners as usize {
        let base = o.owner(j) as *const u8;
        let h = OwnerTop::view(base);
        let dir = base.add(OWNERTOP_HDR) as *const u32;
        for g in 0..h.worst as usize {
            let (off, m) = (*dir.add(2 * g) as usize, *dir.add(2 * g + 1) as usize);
            for k in 0..m {
                f(base.add(off + k * rec) as *const u64);
            }
            n += m;
        }
    }
    Some(n)
}

/// A key seen once in the top's tie order (`Ord` ascending = kept first): NULL's rank (first or last), then the value
/// (bit-inverted under DESC).
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct Single(u8, i64);

impl Single {
    fn of(w0: u64, null: bool, rule: &Rule) -> Single {
        match null {
            true => Single(if rule.nulls_first { 0 } else { 2 }, 0),
            false => Single(1, if rule.desc { !(w0 as i64) } else { w0 as i64 }),
        }
    }
}

/// Record `r`'s exported key `(w0, w1, nwords)`: its image words (a one-word image's second word 0). A three-word image
/// (`fp_flags`: `(fp0, fp1, flags)`) exports its fingerprint's two words: a NULL key's words are zero (emit_image), so it
/// shares an export only with a key whose fingerprint is `(0, 0)` (2^-128). Sharing only merges pass-1 counts toward
/// admitting both (a floor's `c > K` holds for the sum whenever it holds for either), so the admitted set is a superset of
/// the exact one, and pass 2's floor keep and HAVING re-check each group's own count: the rows PostgreSQL shows are
/// unchanged.
unsafe fn key_of(r: *const u64, rule: &Rule) -> (u64, u64, u64) {
    let n = rule.img_words as u64;
    (*r, if n >= 2 { *r.add(1) } else { 0 }, n.min(2))
}

/// The TwoPass protocol's reader (`RegionFn`): pass 1's admitted keys under the rule in `words[1]` (`words[2]`: the bound).
///
/// # Safety
/// `region` is pass 1's finished leader region (HOSTS: at least 64 bytes), of the records the rule describes.
pub(crate) unsafe fn kept_keys(region: *const u8, words: &[u64], owner: Option<Owner>, emit: &mut dyn FnMut(u64, u64, u64)) -> Option<usize> {
    admitted(region, words.get(1..)?, owner, &mut |r, rule| {
        let (a, b, n) = key_of(r, rule);
        emit(a, b, n)
    })
}

/// Every admitted record of pass 1 under the rule `words[0]` (`words[1]`: the bound), to `f` (the marks reader too).
/// CONVERSION WAVE 1 (cvsort): a marked rule's record list (24 B a record) and a key's parts are carved from a scratch
/// arena charged to `owner`, the read slot's (pass 1's hash aggregate: twopass.rs `make_pass1`), on the allocator's road
/// as the `Vec`s were; `None` (a test, no owner) leaves the bytes uncharged where the `Vec`s put them. The top's bounded
/// heap is a `ChargedHeap` on the same owner and kind.
///
/// # Safety
/// As `kept_keys`.
pub(crate) unsafe fn admitted(region: *const u8, words: &[u64], owner: Option<Owner>, f: &mut dyn FnMut(*const u64, &Rule)) -> Option<usize> {
    let rule = Rule::unpack(*words.first()?);
    let rec = rule.rec_words as usize * 8;
    if rule.ext {
        return ext_admitted(region, rec, &rule, owner, f);
    }
    if rule.top_count {
        return top_count_admitted(region, rec, &rule, owner, f);
    }
    let count = |r: *const u64| *r.add(rule.count_word as usize) as i64;
    let k = match rule.floor {
        true => words.get(1).map(|&raw| if rule.int4 { raw as u32 as i32 as i64 } else { raw as i64 })?,
        false => 0,
    };
    // top: every key seen twice, then the n first keys seen once in the tie order (a bounded heap of the n best)
    let mut heap = ChargedHeap::<(Single, *const u64)>::with_capacity(owner, Kind::HashAgg, if rule.floor { 0 } else { rule.n as usize + 1 });
    let mut n = 0;
    // one key's records `rs` (a marked key's parts), `c` rows in all
    let mut group = |c: i64, rs: &[*const u64]| {
        let keep = match (rule.floor, rule.below) {
            (true, false) => (rule.gt && c > k) || (!rule.gt && c >= k),
            (true, true) => (rule.gt && c < k) || (!rule.gt && c <= k),
            (false, _) => c >= 2,
        };
        if keep {
            rs.iter().for_each(|&r| f(r, &rule));
            n += 1;
        } else if !rule.floor && c == 1 && rule.n > 0 {
            let (w0, w1, _) = key_of(rs[0], &rule);
            heap.push((Single::of(w0, rule.nullable && w1 != 0, &rule), rs[0]));
            if heap.len() > rule.n as usize {
                heap.pop();
            }
        }
    };
    each_record(region, rec, &mut |r| group(count(r), &[r]))?;
    // `iter` walks the block in the order `into_iter` did
    for &(_, r) in heap.iter() {
        f(r, &rule);
        n += 1;
    }
    Some(n)
}

/// D-06c (twopass.count), 5c-F6 (its sum lead): every record whose lead ranks with the n-th best of ALL pass 1's
/// records, `T`, ties at `T` included; fewer than n records admit every one. A record's lead reads as the worst and the
/// best rank its group can hold (`lead_ranks`: one rank, but for a float sum, which another fold order may round
/// otherwise); `T` is the n-th best of the worst ranks, and a record is admitted where its best rank reaches `T`. One
/// sweep keeps the n best worst ranks (a bounded heap on `owner`, as the top rule's), a second admits. Each record is
/// one group with its whole lead (twopass_count.rs: pass 1 keys the group by its own image), so n groups rank at `T` or
/// better under any fold order, and a group whose best rank is under `T` is none of the top's first n rows nor a tie
/// of its n-th.
///
/// # Safety
/// As `kept_keys`.
unsafe fn top_count_admitted(region: *const u8, rec: usize, rule: &Rule, owner: Option<Owner>, f: &mut dyn FnMut(*const u64, &Rule)) -> Option<usize> {
    use crate::ops::shared::twopass::{LEAD_COUNT, LEAD_I64};
    let w = rule.count_word as usize;
    match rule.lead {
        // a count's rank, the better the larger: the count itself under DESC, its negation under ASC (a count is never
        // negative); an int8 sum's under its NULL's place, the value bit-reversed under ASC (no overflow): one word each,
        // the wide rank's cost kept off the commonest leads
        LEAD_COUNT => admit_ranked(region, rec, rule, owner, f, |r| {
            let c = *r.add(w) as i64;
            let x = if rule.desc { c } else { -c };
            (x, x)
        }),
        LEAD_I64 => admit_ranked(region, rec, rule, owner, f, |r| {
            let x = match *r.add(rule.rec_words as usize - 1) & 1 != 0 {
                false => (if rule.nulls_first { 2u8 } else { 0 }, 0),
                true if rule.desc => (1, *r.add(w) as i64),
                true => (1, !(*r.add(w) as i64)),
            };
            (x, x)
        }),
        _ => admit_ranked(region, rec, rule, owner, f, |r| lead_ranks(r, rule)),
    }
}

/// `top_count_admitted` over `ranks` (a record's worst and best rank, the greater first): T the n-th best worst rank.
///
/// # Safety
/// As `kept_keys`; `ranks` reads a record of `region`.
unsafe fn admit_ranked<R: Ord + Copy>(region: *const u8, rec: usize, rule: &Rule, owner: Option<Owner>, f: &mut dyn FnMut(*const u64, &Rule), ranks: impl Fn(*const u64) -> (R, R)) -> Option<usize> {
    let n = rule.n as usize;
    let mut heap = ChargedHeap::<core::cmp::Reverse<R>>::with_capacity(owner, Kind::HashAgg, n + 1);
    each_record(region, rec, &mut |r| {
        heap.push(core::cmp::Reverse(ranks(r).0));
        if heap.len() > n {
            heap.pop();
        }
    })?;
    let t = match heap.len() < n {
        true => None,
        false => heap.peek().map(|w| w.0),
    };
    let mut admitted = 0;
    each_record(region, rec, &mut |r| {
        if t.is_none_or(|t| ranks(r).1 >= t) {
            f(r, rule);
            admitted += 1;
        }
    })?;
    Some(admitted)
}

/// A lead value on its type's ascending order (abi `num_sum_order`'s form): (class, high, low), -Infinity 0, a finite
/// value 1, +Infinity 2, NaN 3 (PostgreSQL's numeric and float orders put NaN above every value); an integer as an
/// i256 (high signed, low unsigned), a float its order-preserving bits in the high part.
type Val = (u8, i128, u128);

/// A record's place on the top's order (`Ord`: the greater ranks first): a NULL lead first (2) or last (0), else 1
/// and its `Val`, each part reversed under ASC. `RANK_FIRST` / `RANK_LAST` bound every rank (a record whose lead may
/// rank anywhere).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Rank(u8, u8, i128, u128);

const RANK_FIRST: Rank = Rank(3, 0, 0, 0);
const RANK_LAST: Rank = Rank(0, 0, i128::MIN, 0);

fn rank(v: Option<Val>, rule: &Rule) -> Rank {
    match v {
        None => Rank(if rule.nulls_first { 2 } else { 0 }, 0, 0, 0),
        Some((c, h, l)) if rule.desc => Rank(1, c, h, l),
        Some((c, h, l)) => Rank(1, 3 - c, !h, !l),
    }
}

/// Record `r`'s lead (`Rule::lead`, at `count_word`) as (its worst rank, its best rank) under any fold order. A count,
/// an integer or a numeric sum is exact, one rank; a sum is NULL where the validity word (the record's last) has bit 0
/// clear; a numeric sum's OVERFLOW sentinel ranks anywhere, so it is admitted and pass 2's final raises as PostgreSQL's
/// would; a float sum spans `float_slack` around pass 1's value.
unsafe fn lead_ranks(r: *const u64, rule: &Rule) -> (Rank, Rank) {
    use crate::ops::shared::twopass::{LEAD_COUNT, LEAD_F32, LEAD_F64, LEAD_I128, LEAD_I64, LEAD_NUM, LEAD_NUM_WIDE};
    use gravity_abi::breaker::{num_sum_order, SlotKind};
    let w = r.add(rule.count_word as usize);
    let int = |v: i128| (1u8, v >> 127, v as u128);
    let one = |v: Option<Val>| {
        let x = rank(v, rule);
        (x, x)
    };
    if rule.lead == LEAD_COUNT {
        return one(Some(int(*w as i64 as i128)));
    }
    if *r.add(rule.rec_words as usize - 1) & 1 == 0 {
        return one(None);
    }
    let words = |n: usize| core::array::from_fn::<u64, 4, _>(|i| if i < n { *w.add(i) } else { 0 });
    let v = match rule.lead {
        LEAD_I64 => int(*w as i64 as i128),
        LEAD_I128 => int(((*w.add(1) as u128) << 64 | *w as u128) as i128),
        LEAD_NUM | LEAD_NUM_WIDE => {
            let (k, n) = if rule.lead == LEAD_NUM { (SlotKind::SumNum, 2) } else { (SlotKind::SumNumWide, 4) };
            match num_sum_order(k, words(n)) {
                Some(v) => v,
                None => return (RANK_LAST, RANK_FIRST),
            }
        }
        LEAD_F64 | LEAD_F32 => {
            let s = f64::from_bits(*w);
            // NaN and the infinities come from a NaN or infinite input, so every fold order ends there (a finite
            // operands' overflow raised at its row)
            if !s.is_finite() {
                return one(Some(float_val(s)));
            }
            let Some(e) = float_slack(s, f64::from_bits(*w.add(1)), *w.add(2) as i64, rule.lead == LEAD_F32) else { return (RANK_LAST, RANK_FIRST) };
            let (a, b) = (rank(Some(float_val(s - e)), rule), rank(Some(float_val(s + e)), rule));
            return (a.min(b), a.max(b));
        }
        _ => return (RANK_LAST, RANK_FIRST),
    };
    one(Some(v))
}

/// A float's `Val`: NaN above +Infinity, -0 as +0 (PostgreSQL's float order ties them), else its bits made
/// order-preserving as a signed integer.
fn float_val(x: f64) -> Val {
    match x {
        _ if x.is_nan() => (3, 0, 0),
        f64::INFINITY => (2, 0, 0),
        f64::NEG_INFINITY => (0, 0, 0),
        _ => {
            let b = (x + 0.0).to_bits() as i64;
            (1, (b ^ ((b >> 63) as u64 >> 1) as i64) as i128, 0)
        }
    }
}

/// 5c-F6: the most a float sum may move between two fold orders, around pass 1's finite sum `s`: `abs` is the group's
/// sum of `|x| 2^-128` in some order (twopass_count.rs `AbsSum`: scaled so it never overflows), `rows` its row count
/// (at least its terms m). Any order's recursive sum is within γ(m) Σ|x| of the exact one (γ(k) = k u / (1 - k u), u
/// = 2^-53, 2^-24 for float4pl's f32 adds; Higham, Accuracy and Stability of Numerical Algorithms §4.2), so two orders
/// within twice that. Σ|x| is at most 2^128 (abs / (1 - γ64(m)) + m 2^-1074): the scaled terms are non-negative, and
/// each is within half a subnormal step of |x| 2^-128. The margin covers this bound's own rounding and `s ∓ e`'s.
/// `None` (no bound: the record ranks anywhere) where m u reaches 1/2 or the bound is not finite.
fn float_slack(s: f64, abs: f64, rows: i64, f32: bool) -> Option<f64> {
    let m = rows.max(1) as f64;
    let gamma = |u: f64| (m * u < 0.5).then(|| m * u / (1.0 - m * u));
    let (g64, g) = (gamma(f64::EPSILON / 2.0)?, gamma(if f32 { f32::EPSILON as f64 / 2.0 } else { f64::EPSILON / 2.0 })?);
    let sum_abs = (abs / (1.0 - g64) + m * f64::from_bits(1)) * 2f64.powi(128);
    let e = 2.0 * g * sum_abs;
    let e = e * (1.0 + 2f64.powi(-40)) + (s.abs() + e) * 2f64.powi(-50);
    (e.is_finite() && abs >= 0.0).then_some(e)
}

/// A top's bounded std `BinaryHeap` whose block's CAPACITY is charged to `owner` on kind lane `kind` (door 1, as
/// `ChargedVec`), re-synced only when the capacity moves. The charge is declared first, so its drop moves the block's bytes
/// to this thread's account before the block's free lands there. No owner: a plain heap, uncharged as before.
struct ChargedHeap<T> {
    charge: Charge,
    on: bool,
    /// The capacity `charge` holds committed (elements).
    cap: usize,
    heap: std::collections::BinaryHeap<T>, // alloc: Exec charged self.charge.owner()
}

impl<T: Ord> ChargedHeap<T> {
    fn with_capacity(owner: Option<Owner>, kind: Kind, cap: usize) -> ChargedHeap<T> {
        let charge = owner.map_or(Charge::NONE, |o| {
            let release = match o {
                Owner::Portal { .. } => Release::Portal,
                Owner::Session(_) => Release::Session,
                Owner::Cache(_) => Release::Evict,
                _ => Release::Statement,
            };
            Charge::open(o, release, kind, Door::Heap, 0)
        });
        let mut h = ChargedHeap { charge, on: owner.is_some(), cap: 0, heap: std::collections::BinaryHeap::with_capacity(cap) }; // alloc: Exec charged owner
        h.recharge();
        h
    }

    fn push(&mut self, x: T) {
        self.heap.push(x); // alloc: Exec charged self.charge.owner()
        if self.heap.capacity() != self.cap {
            self.recharge();
        }
    }

    fn pop(&mut self) -> Option<T> {
        self.heap.pop()
    }

    fn len(&self) -> usize {
        self.heap.len()
    }

    fn peek(&self) -> Option<&T> {
        self.heap.peek()
    }

    fn iter(&self) -> impl Iterator<Item = &T> {
        self.heap.iter()
    }

    /// The charge re-synced to the block's capacity: a commit of what it grew (allocated on this thread), a free of what it
    /// shrank.
    fn recharge(&mut self) {
        let now = self.heap.capacity();
        if self.on && now != self.cap {
            let b = |c: usize| (c as u64).saturating_mul(core::mem::size_of::<T>() as u64);
            if now > self.cap {
                self.charge.commit(b(now) - b(self.cap));
            } else {
                self.charge.free(b(self.cap) - b(now));
            }
        }
        self.cap = now;
    }
}

/// A value on an extreme top's order (`Ord` ascending = ranked first): the NULL extreme first or last, then the value
/// (bit-inverted under `desc`, the max's order).
fn ext_rank(v: Option<i64>, rule: &Rule) -> (u8, i64) {
    match v {
        None => (if rule.nulls_first { 0 } else { 2 }, 0),
        Some(x) => (1, if rule.desc { !x } else { x }),
    }
}

/// The extreme rule (`Rule::ext`, one record a key: `[image][count(*)][extreme][count(v)]`). A key's witness is its
/// extreme, or NULL when no row has a value: one of its groups attains it (the group of the extreme's row, or every
/// group). Its best is the best value any of its groups can have: NULL under NULLS FIRST when some row's value is NULL
/// (that row's group may hold no other), else the witness, since a group's extreme never passes its key's. The n-th
/// best witness `m` is the value of n distinct groups or better, so the top's n-th group ranks at `m` or before, and
/// every group of the top lies in a key whose best ranks at `m` or before: those keys are admitted, ties included (a
/// superset; pass 2 folds them exactly and the top orders them by all its keys). Fewer than n keys: every key.
unsafe fn ext_admitted(region: *const u8, rec: usize, rule: &Rule, owner: Option<Owner>, f: &mut dyn FnMut(*const u64, &Rule)) -> Option<usize> {
    let cw = rule.count_word as usize;
    let read = |r: *const u64| {
        let (c, e) = (*r.add(cw) as i64, *r.add(cw + 1) as i64);
        let nn = if rule.ext_nn { *r.add(cw + 2) as i64 } else { c };
        let witness = (nn > 0).then_some(e);
        // gtopn GT-1: a spread key's best is its own word after the witness (and count(v)), its column's other extreme
        let top = if rule.spread { (nn > 0).then(|| *r.add(cw + 2 + rule.ext_nn as usize) as i64) } else { witness };
        let best = if rule.nulls_first && nn < c { None } else { top };
        (ext_rank(witness, rule), ext_rank(best, rule))
    };
    let mut heap = ChargedHeap::<(u8, i64)>::with_capacity(owner, Kind::HashAgg, rule.n as usize + 1);
    each_record(region, rec, &mut |r| {
        heap.push(read(r).0);
        if heap.len() > rule.n as usize {
            heap.pop();
        }
    })?;
    let m = if heap.len() < rule.n as usize { None } else { heap.peek().copied() };
    let mut n = 0;
    each_record(region, rec, &mut |r| {
        if m.is_none_or(|m| read(r).1 <= m) {
            f(r, rule);
            n += 1;
        }
    })?;
    Some(n)
}

/// The TwoPass protocol's reader over the bare pass 1's finished region: each listed key as a one-word image `(k, 0, 1)`
/// (repeats included: the admitted set dedups); under a nullable rule (`words[1]`) the image is `(k, NULL flag, 2)`, every
/// listed key's flag 0, and the NULL key `(0, 1, 2)` once (its image words are zero, emit_image).
///
/// # Safety
/// `region` is the bare pass 1's finished leader region (HOSTS: at least 64 bytes).
pub(crate) unsafe fn bare_keys(region: *const u8, words: &[u64], _owner: Option<Owner>, emit: &mut dyn FnMut(u64, u64, u64)) -> Option<usize> {
    let nullable = Rule::unpack(*words.get(1)?).nullable;
    let n = t3::export_keys(region, &P1_WORDS, &mut |k| emit(k, 0, 1 + nullable as u64))?;
    if nullable {
        emit(0, 1, 2);
    }
    Some(n + nullable as usize)
}

gravity_abi::host! {
    /// D2862 (b): `bytes` of room bumped from a top's copy cursor `images` (its three words: cur, end, chunk), as
    /// table/raw.rs `key_copy_fixed` bumps a copy, nothing copied: the side array of a full sort's kept columns, taken at
    /// a fresh call's first arrival (topn lower.rs `TopEnd::keep`). Below zero: no room could be pinned.
    /// # Safety — `images` the cursor's three live words.
    pub(crate) unsafe fn kept_room(ctx: u64, images: u64, bytes: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let ctx = ctx as *mut gravity_abi::ctx::Ctx;
        let images = images as *mut u64;
        let need = (bytes as usize).next_multiple_of(8);
        // D3312: the arena's one bump (a cursor key_copy may bump too: one chunk format)
        let d = gravity_abi::breaker::img_bump(images, need, |b| gravity_abi::program::pin(ctx, b));
        if d.is_null() { gravity_kernels::host_abi::pin_refused(need); return -1; } // D1392 (b): named, 53200
        d as i64
    }
}

gravity_abi::host! {
    /// PSP2-43: the int8 floor a HAVING's numeric K puts on a group's count (strategies/twopass_having.rs
    /// `numeric_bound`, once per execution): with `gt`, K rounded down (`count > K` holds exactly when `count > floor(K)`),
    /// else K rounded up (`count >= K`, and `count = K`, need `count >= ceil(K)`: typesem `Mantissa::rounded`), clamped
    /// to the int8 range. `datum` 0 (a NULL K), NaN, an infinity, a K wider than an i128 mantissa or a payload this does
    /// not read: i64::MIN, no floor (the HAVING decides every group). A floor never skips a group whose count passes the
    /// HAVING's compare.
    /// # Safety — `datum` is 0 or a live numeric varlena.
    pub(crate) unsafe fn count_floor(datum: u64, gt: u64) -> i64 {
        if datum == 0 {
            return i64::MIN;
        }
        let m = gravity_fn::with_payload(datum as usize as *const u8, gravity_fn::typesem::numeric::mantissa);
        let k = m.ok().flatten().and_then(|m| m.rounded(gt != 0));
        k.map_or(i64::MIN, |k| i64::try_from(k).unwrap_or(if k < 0 { i64::MIN } else { i64::MAX }))
    }
}

/// V4R-192: admitted over the export equals admitted over every pass-1 record (tests only: no rows).
#[cfg(test)]
pub(crate) mod twopass_top_tests;

/// D-06c: the count top's rule admits every record ranking with the n-th best count (tests only: no rows).
#[cfg(test)]
pub(crate) mod twopass_count_tests;

/// 5c-F6: the sum top's rule admits every record ranking with the n-th best sum, under any fold order for a float
/// (tests only: no rows).
#[cfg(test)]
pub(crate) mod twopass_sum_tests;

/// The extreme rule admits every key holding a group of some valid top (tests only: no rows).
#[cfg(test)]
mod ext_tests {
    use std::collections::{BTreeMap, BTreeSet};

    use gravity_abi::breaker::{KeyKind, Owned, OwnerTop, RecordLayout, SlotKind, OWNED_HDR, OWNERTOP_HDR, OWNER_RUN_OFF, TABLE_OWNED};

    use super::admitted;
    use crate::ops::shared::twopass::Rule;

    type Row = (i64, i64, Option<i64>);

    /// The keys `admitted` lists for rows `(k1, k2, v)` under the extreme rule, and the keys of the groups every valid
    /// top of `n` may hold (a group whose value ranks at or before the n-th group's).
    fn run(rows: &[Row], n: u32, max: bool, nulls_first: bool, nn_slot: bool) -> (BTreeSet<i64>, BTreeSet<i64>, usize) {
        let better = |a: i64, b: i64| if max { a.max(b) } else { a.min(b) };
        let fold = |m: &mut BTreeMap<(i64, i64), (i64, Option<i64>, i64)>, k: (i64, i64), v: Option<i64>| {
            let e = m.entry(k).or_insert((0, None, 0));
            e.0 += 1;
            if let Some(x) = v {
                e.2 += 1;
                e.1 = Some(e.1.map_or(x, |y| better(x, y)));
            }
        };
        let (mut per, mut groups) = (BTreeMap::new(), BTreeMap::new());
        for &(k1, k2, v) in rows {
            fold(&mut per, (k1, 0), v);
            fold(&mut groups, (k1, k2), v);
        }
        let slots: &[SlotKind] = if nn_slot { &[SlotKind::Count, SlotKind::MaxI64, SlotKind::Count] } else { &[SlotKind::Count, SlotKind::MaxI64] };
        let l = RecordLayout::with_sentinel(&[KeyKind::I64], slots, true, Some(0));
        let rec = l.size() as usize;
        let obytes = OWNER_RUN_OFF + (per.len() + 1) * rec;
        let mut region = vec![0u64; (OWNED_HDR + obytes).div_ceil(8)];
        let rule = Rule { ext: true, ext_nn: nn_slot, desc: max, nulls_first, img_words: 1, rec_words: (rec / 8) as u8, count_word: (l.sentinel_off() / 8) as u8, n, ..Rule::default() };
        let mut got = BTreeSet::new();
        unsafe {
            let o = region.as_mut_ptr() as *mut Owned;
            *o = Owned { nowners: 1, owner_bytes: obytes as u32, limit: 0, pad0: [0; 3], kind: TABLE_OWNED, base: 0, pad1: [0; 5], cell: core::cell::UnsafeCell::new([]) };
            let base = Owned::view(region.as_ptr() as *const u8).owner(0);
            for (i, (&(k1, _), &(c, e, nn))) in per.iter().enumerate() {
                let d = base.add(OWNER_RUN_OFF + i * rec);
                *(d as *mut i64) = k1;
                *(d.add(l.slot_off(0) as usize) as *mut i64) = c;
                *(d.add(l.slot_off(1) as usize) as *mut i64) = e.unwrap_or(0x5a5a);
                if nn_slot {
                    *(d.add(l.slot_off(2) as usize) as *mut i64) = nn;
                }
            }
            (*(base as *mut OwnerTop)).worst = 1;
            let dir = base.add(OWNERTOP_HDR) as *mut u32;
            (*dir, *dir.add(1)) = (OWNER_RUN_OFF as u32, per.len() as u32);
            let words = [rule.pack(None)];
            admitted(region.as_ptr() as *const u8, &words, None, &mut |r, _| assert!(got.insert(*r as i64), "a key admitted once")).expect("an owned region");
        }
        let rank = |v: Option<i64>| super::ext_rank(v, &rule);
        let mut ranks: Vec<_> = groups.values().map(|g| rank(g.1)).collect();
        ranks.sort_unstable();
        let need = match ranks.get(n.max(1) as usize - 1) {
            Some(&nth) => groups.iter().filter(|(_, g)| rank(g.1) <= nth).map(|((k1, _), _)| *k1).collect(),
            None => per.keys().map(|k| k.0).collect(),
        };
        (got, need, per.len())
    }

    /// A deterministic spread: `keys` leading keys, each with a few groups; `null_every` > 0 leaves every such row's value NULL.
    fn rows(keys: i64, null_every: i64) -> Vec<Row> {
        let mut x = 0x9e37_79b9u64;
        (0..keys * 3)
            .map(|i| {
                x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
                let k1 = (x >> 20) as i64 % keys;
                let v = ((x >> 40) % 1000) as i64 - 500;
                (k1, i % 4, (null_every == 0 || i % null_every != 0).then_some(v))
            })
            .collect()
    }

    #[test]
    fn the_extreme_rule_admits_every_key_of_a_valid_top() {
        for (data, nn_slot) in [(rows(300, 0), false), (rows(300, 0), true), (rows(300, 7), true), (rows(40, 2), true)] {
            for (max, nulls_first) in [(true, true), (true, false), (false, false), (false, true)] {
                for n in [0u32, 1, 5, 37, 400] {
                    let (got, need, keys) = run(&data, n, max, nulls_first, nn_slot);
                    assert!(need.is_subset(&got), "n {n} max {max} nulls_first {nulls_first} nn {nn_slot}: missed {:?}", need.difference(&got).collect::<Vec<_>>());
                    if n == 5 && keys >= 250 && !(nulls_first && nn_slot) {
                        assert!(got.len() < keys / 4, "n 5 prunes most keys: {} of {keys}", got.len());
                    }
                }
            }
        }
    }

    #[test]
    fn an_all_null_key_ranks_as_null() {
        // key 1: values NULL only; key 2: 5; key 3: 9 and a NULL row
        let data: Vec<Row> = vec![(1, 0, None), (1, 1, None), (2, 0, Some(5)), (3, 0, Some(9)), (3, 1, None)];
        let (got, _, _) = run(&data, 1, true, false, true);
        assert_eq!(got, BTreeSet::from([3]), "max DESC NULLS LAST: 9 first");
        let (got, _, _) = run(&data, 1, true, true, true);
        assert_eq!(got, BTreeSet::from([1, 3]), "max DESC NULLS FIRST: key 1's groups are NULL, key 3 may hold one");
        let (got, _, _) = run(&data, 1, false, false, true);
        assert_eq!(got, BTreeSet::from([2]), "min ASC NULLS LAST: 5 first");
    }
}
