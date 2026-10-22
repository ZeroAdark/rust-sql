//! The generic owner phase of a program-declared `OwnerDesc` (milestone-2.md §2.6 items 12–13; partitioned-record.md §3.2, §3.4): `par_hashagg`'s owner functions over the descriptor's record
//! layout instead of the kernel's constant — an owner region is `OwnerTop`, then either the run directory (`(offset, records)` per folded partition), the run area and the table space, or a
//! top-k owner's `n` kept records, their index heap and the table space — or, for the count-ordered form (`order_slot` a `Count` slot, `topk` = n; milestone-2.md §2.6 item 31), the
//! `n` best records by (count descending, key words ascending) with `OwnerTop.thr` the count a candidate must reach (t3's `ORDER_COUNT_DESC` owner over the layout: the driver's fold lists the
//! candidates, `OwnerFns.order_slot`); the words are `[topk, order, bits, slot, layout × 4]`; the fold is the layout-dispatched `fold::owner_fold`. The region layout is `par_hashagg`'s byte for
//! byte (the count form's is t3's: `[OwnerTop][n records][table]`), so its emit reads a generic owner's result.
#![allow(unsafe_code)]

use gravity_abi::PtrWord;
use gravity_abi::breaker::{num_add, FloatOverflow, HashAgg, KeyKind, OwnerFns, OwnerTop, Rec, RecordLayout, SlotKind, DENSE_TABLE_FLOOR, HASHAGG_HDR, KEY_CHECK_KEYS, LAYOUT_IMPLICIT_COUNT, MOVING, ORDER_KEY, ORDER_NONE, OWNERTOP_HDR, OWNER_PARAMS, REDUCE_FLUSH, T1, T1_MAX_PARTS};
use gravity_abi::hash::KeyHash;
use gravity_abi::status::{raise, raised, unsupported, UNSUPPORTED_DOMAIN, UNSUPPORTED_SCRATCH};
use gravity_abi::Site;
use std::sync::atomic::{AtomicUsize, Ordering};
use gravity_kernels::ops::sort::topn::t3_dense;
use gravity_kernels::ops::sort::topn::t3_dense::{Area, AREA_PAIRS, AREA_SENTINEL, AREA_SHIFT};
use gravity_kernels::table::owner_fold::{area_fold, dense_folds, owner_fold, Fold};
use gravity_kernels::dict::reps::{count_better, keys_better, pack_slot, unpack_slot, Reps, REP_WORDS};
use gravity_kernels::host_abi::slot_order;
use gravity_kernels::table::route::owner_share;
use gravity_program::cost::owner::{dense_pays, ROUND2_T1_MAX, RUNS_FLOOR_MAX};
use gravity_program::cost::runtime::GRANT_FLOOR;
use gravity_program::{OwnerDesc, WordsDesc, RUNS_TOP_HEAP, ACC_PAIRS, ACC_SLOT_BASE, CHECK_ADD, CHECK_CAST, CHECK_KEY_NULLABLE, CHECK_NEG, CHECK_NONE, CHECK_RSUB, CHECK_SUB, NULL_ARG_NONE, OBSERVE_KEY_FLAGGED, OBSERVE_NONE, TIE_FLAGS_NONE};

const P_TOPK: usize = 0;
const P_ORDER: usize = 1;
const P_BITS: usize = 2;
const P_SLOT: usize = 3;
const P_LAYOUT: usize = 4;
const ORDER_BY_KEY: u64 = 1;
const ORDER_BY_COUNT: u64 = 2;
const LAYOUT_WORDS: usize = gravity_abi::breaker::LAYOUT_WORDS;
/// The distinct form's accumulator layout, by value after the pair layout.
const P_ACC: usize = P_LAYOUT + LAYOUT_WORDS;
/// The count form's key check at bind (`check_range`, V4R-113): the head word `keys | raise code << 8`; then the word of
/// each checked key's word, a byte each (`P_CHECK_KEYS`); then `lo`, `hi` per checked key.
const P_CHECK: usize = P_ACC + LAYOUT_WORDS;
const P_CHECK_KEYS: usize = P_CHECK + 1;
const _: () = assert!(KEY_CHECK_KEYS <= 8);
/// The NULL-argument flag `(word + 1, bit)` in `P_ORDER`'s top two bytes (0: none) and the three argument-fold slots
/// (`slot + 1`, 0: none) in `P_BITS`'s bytes 1, 2 and 3 (M5 §4.5, D1279; `OwnerDesc::null_arg` / `arg_fold`); `P_ORDER`'s top bit
/// the group-flags form (D500, `OwnerDesc::group_flags`).
const NULL_ARG_SHIFT: u32 = 48;
/// The count form's tie flags word + 1 (`OwnerDesc::tie_flags`, 0: none) in the byte the distinct form keeps its
/// NULL-argument word in: the form (`pair` 0 or not) names which.
const TIE_FLAGS_SHIFT: u32 = NULL_ARG_SHIFT;
const GROUP_FLAGS_BIT: u32 = 63;
/// SRC-38 slice 2: the generic form (`pair` 0) folds key 0's least / greatest datum word into `OwnerTop::key_lo` / `key_hi`
/// (`OwnerDesc::observe_key`, set by the lowering under an `Observe` proto for one integer key): `P_ORDER`'s bit 62, and
/// bit 61 when the key's `(value, flags)` image skips the NULL group (`OBSERVE_KEY_FLAGGED`). The distinct form's
/// NULL-argument bit field (56..62) and group-flags bit (63) hold these bits' places only when `pair` is set.
const OBSERVE_KEY_BIT: u32 = 62;
const OBSERVE_FLAGGED_BIT: u32 = 61;
/// CAP-28: the group-flags form's flags words `first | carried << 8` (`OwnerDesc::flag_words`), in the word the count
/// form's key check keeps its keys' words in (a distinct form has no key check: `OwnerDesc::key_check` is the count form's).
const P_FLAG_WORDS: usize = P_CHECK + 1;
/// The count form's order word (`OwnerDesc::order`, an emit slot sort word; 0: the count descending) in `P_CHECK`'s
/// bits 16..32, under the key check's word and raise code.
const ORDER_WORD_SHIFT: u32 = 16;
const ARG_FOLD_SHIFT: u32 = 8;
/// The representatives' slot words past `P_SLOT` (kernels `pack_slot`'s words 1..`REP_WORDS`), at the params' tail.
const P_SLOT_MORE: usize = P_CHECK + 2 + 2 * KEY_CHECK_KEYS;
const _: () = assert!(P_SLOT_MORE + REP_WORDS - 1 == OWNER_PARAMS);
/// D3153: the runs form's bound at bind (`OwnerDesc::runs_floor_of`, `floor_bound`): the bounded slot's byte offset + 1
/// (0: none), then the least and the most value a kept record holds; in the words the count form's key check keeps its
/// intervals in (a runs form has no key check, a distinct form no floor: `P_FLAG_WORDS` is before them).
/// D-06: a top's (`OwnerDesc::runs_top_of`) puts its n in bits 32..62 of the first word and its `below` in bit 63, its
/// bound each partition's own (`Best`).
const P_FLOOR: usize = P_CHECK + 3;
const _: () = assert!(P_FLOOR > P_FLAG_WORDS && P_FLOOR + 2 < P_SLOT_MORE);
/// The tagged distinct form's first tag slot + 1 (`OwnerDesc::tags`, 0: not tagged), in the key check's first interval
/// word (a distinct form has no key check, and keeps its flags words before it).
const P_TAGS: usize = P_CHECK + 2;
const _: () = assert!(P_TAGS > P_FLAG_WORDS && P_TAGS < P_FLOOR);
const MAX_RUNS: usize = T1_MAX_PARTS as usize;
const DIR_OFF: usize = OWNERTOP_HDR;
const RUN_OFF: usize = DIR_OFF + 8 * MAX_RUNS;
const DEFAULT_BITS: u8 = 4;

/// The runner's binding of a declared phase: the partition bits — the program's `spill_bits`, or
/// `spill_bits(records, record)` from the memo's count — written at `d.bits` into the words, then
/// the functions: `d.ctor` over the words, or the generic phase over `d.layout`.
pub fn bind(d: &OwnerDesc, words: &mut [u64], spill_bits: u8, records: Option<u64>) -> OwnerFns {
    bind_dense(d, words, spill_bits, records, None)
}

/// `bind` with the pipeline's declared words: the group key's dense domain the runner wrote at
/// `WordsDesc.dense_lo / dense_hi` (`lo > hi`: none) chooses the distinct form's dense round 1 — only for a
/// descriptor that names the key (`dense_attno`): the runner's default extrema (the site's first attribute) are
/// a guess this form must never refuse on.
pub fn bind_dense(d: &OwnerDesc, words: &mut [u64], spill_bits: u8, records: Option<u64>, declared: Option<WordsDesc>) -> OwnerFns {
    bind_dense_plain(d, words, spill_bits, records, declared).0
}

/// `bind_dense`, and with a dense domain the same form bound without it (D597: `HashAggPartial::with_plain`, the round-1 rerun
/// after a key outside the domain).
pub fn bind_dense_plain(d: &OwnerDesc, words: &mut [u64], spill_bits: u8, records: Option<u64>, declared: Option<WordsDesc>) -> (OwnerFns, Option<OwnerFns>) {
    bind_dense_plain_at(d, words, spill_bits, records, declared, true, None)
}

/// `bind_dense_plain` for a producer that can (`parallel`) or cannot run parallel owners (`owner_fns_at`), over the `rows` its
/// census keeps (`None`: unknown). B04 / EX-05: a domain whose area does not pay for those rows (program cost `dense_pays`'s
/// folds term) is not bound, so the owner takes the pair path as it does with no domain.
pub fn bind_dense_plain_at(d: &OwnerDesc, words: &mut [u64], spill_bits: u8, records: Option<u64>, declared: Option<WordsDesc>, parallel: bool, rows: Option<u64>) -> (OwnerFns, Option<OwnerFns>) {
    let bits = match records {
        Some(r) => gravity_kernels::table::params::spill_bits(r, d.layout.size()),
        None => spill_bits,
    };
    if let Some((w, sh)) = d.bits {
        let w = w as usize;
        words[w] = (words[w] & !(0xffu64 << sh)) | (bits as u64) << sh;
    }
    let dense = declared.filter(|_| d.dense_attno > 0).and_then(|w| (w.dense_lo != WordsDesc::NONE && w.dense_hi != WordsDesc::NONE).then_some(w)).and_then(|w| Some((*words.get(w.dense_lo as usize)? as i64, *words.get(w.dense_hi as usize)? as i64)));
    let dense = dense.filter(|&(lo, hi)| rows.is_none_or(|r| dense_pays(span_of(lo, hi), d.acc.size() as u64, r, u64::MAX)));
    match (d.ctor_desc, d.ctor) {
        (Some(ctor), _) => (ctor(d, words), None),
        (None, Some(ctor)) => (ctor(words), None),
        (None, None) => {
            let check = check_range(d, words);
            let floor = floor_bound(d, words);
            let top = d.runs_top_of().filter(|&(n, _, slot)| n >= 1 && n <= RUNS_TOP_HEAP && (slot as usize) < d.layout.nslots() && d.check.width == 0 && d.layout.slot_kind(slot as usize) == SlotKind::Count);
            let bound = |dense: Option<(i64, i64)>| {
                let mut f = owner_fns_at(d, bits, dense, parallel);
                if let Some((off, lo, hi)) = floor {
                    (f.params[P_FLOOR], f.params[P_FLOOR + 1], f.params[P_FLOOR + 2]) = (off as u64 + 1, lo as u64, hi as u64);
                }
                if let Some((n, below, slot)) = top {
                    f.params[P_FLOOR] = d.layout.slot_off(slot as usize) as u64 + 1 | (n as u64) << 32 | (below as u64) << 63;
                }
                if let Some(c) = &check {
                    f.params[P_CHECK] |= c.n as u64 | (c.code as u64) << 8;
                    for (i, &(k, lo, hi)) in c.keys[..c.n].iter().enumerate() {
                        let null = if c.nulls >> i & 1 != 0 { CHECK_KEY_NULLABLE as u64 } else { 0 };
                        f.params[P_CHECK_KEYS] |= (k as u64 | null) << (8 * i as u32);
                        (f.params[P_CHECK + 2 + 2 * i], f.params[P_CHECK + 3 + 2 * i]) = (lo as u64, hi as u64);
                    }
                }
                f
            };
            (bound(dense), dense.map(|_| bound(None)))
        }
    }
}

/// D3153: the runs form's bound this execution (`OwnerDesc::runs_floor_of`): `(the slot's byte offset, the least and the
/// most value kept)`, K read from its bound word (an int4's sign-extended), a `>` K's least K + 1 and a `<` K's most K - 1
/// (saturated: a bound past the type's range keeps its end value, which the reader's HAVING still tests). A NULL K's
/// word bounds at whatever it holds: the reader's HAVING keeps no group then, so any record the bound drops is one it
/// drops. The slot is a count or an integer final read whole (an empty one's word fails the HAVING as its NULL does).
fn floor_bound(d: &OwnerDesc, words: &[u64]) -> Option<(usize, i64, i64)> {
    let (w, gt, int4, below, slot) = d.runs_floor_of()?;
    let x = *words.get(w as usize)?;
    let k = if int4 { x as i32 as i64 } else { x as i64 };
    let whole = (slot as usize) < d.layout.nslots() && d.check.width == 0 && matches!(d.layout.slot_kind(slot as usize), SlotKind::Count | SlotKind::MinI64 | SlotKind::MaxI64 | SlotKind::SumI64);
    let (lo, hi) = match (below, gt) {
        (false, true) => (k.saturating_add(1), i64::MAX),
        (false, false) => (k, i64::MAX),
        (true, true) => (i64::MIN, k.saturating_sub(1)),
        (true, false) => (i64::MIN, k),
    };
    whole.then(|| (d.layout.slot_off(slot as usize) as usize, lo, hi))
}

/// D-06: a top's running bound over one partition's records, in the order they are laid (`keeps`): the k best words
/// seen so far are a min-heap (`below`: of their complements, so the smallest words are the best) in a fixed array, and
/// a record is kept while the heap is not full or its word is at or past the heap's least (its tie kept). The least
/// only rises, so every record at or past the partition's final k-th best word is kept, in one walk; the few kept
/// before the least rose past them the emit's sort cuts.
struct Best {
    heap: [i64; RUNS_TOP_HEAP as usize],
    m: usize,
    k: usize,
    below: bool,
}

impl Best {
    fn new(k: usize, below: bool) -> Best {
        Best { heap: [0; RUNS_TOP_HEAP as usize], m: 0, k: k.clamp(1, RUNS_TOP_HEAP as usize), below }
    }

    #[inline(always)]
    fn keeps(&mut self, v: i64) -> bool {
        let key = if self.below { !v } else { v };
        let heap = &mut self.heap;
        if self.m < self.k {
            let mut c = self.m;
            heap[c] = key;
            self.m += 1;
            while c > 0 && heap[(c - 1) / 2] > heap[c] {
                heap.swap(c, (c - 1) / 2);
                c = (c - 1) / 2;
            }
            return true;
        }
        if key < heap[0] {
            return false;
        }
        if key > heap[0] {
            heap[0] = key;
            let mut c = 0;
            loop {
                let (l, r) = (2 * c + 1, 2 * c + 2);
                let mut s = c;
                if l < self.k && heap[l] < heap[s] {
                    s = l;
                }
                if r < self.k && heap[r] < heap[s] {
                    s = r;
                }
                if s == c {
                    break;
                }
                heap.swap(c, s);
                c = s;
            }
        }
        true
    }
}

/// The count form's key check bound (`OwnerDesc::check`): the raise code and, per checked key, `(key word, lo, hi)`: the
/// key values that keep every step over it in range (`lo > hi`: none, raising at the first folded record); `nulls` bit `j`:
/// key `j` can be NULL (`CHECK_KEY_NULLABLE`, its NULL flag bit `key word`), and a NULL raises nothing.
#[derive(Clone, Copy)]
struct KeyRanges {
    code: u8,
    n: usize,
    keys: [(usize, i64, i64); KEY_CHECK_KEYS],
    nulls: u8,
}

/// The count form's key check at bind (`OwnerDesc::check`, V4R-113), from the constants' words: each step's result range
/// pulled back through the steps before it in its chain (each a monotone map of the value before it, so the values that
/// keep the step in range are an interval) to an interval of the chain's key, the intervals of one key intersected. Every
/// value a step reads lies in i64 (a key, or a step's result inside the width), so each pulled interval is clipped there.
/// NEW-46: a minus reads no constant and pulls `[lo, hi]` to `[-hi, -lo]`; a narrowing cast reads none and keeps it (its
/// value must lie in the width itself). `None`: no check.
fn check_range(d: &OwnerDesc, words: &[u64]) -> Option<KeyRanges> {
    const ALL: (i128, i128) = (i64::MIN as i128, i64::MAX as i128);
    let c = d.check;
    let (min, max, code): (i128, i128, u8) = match c.width {
        0 => return None,
        2 => (i16::MIN as i128, i16::MAX as i128, raise::INT2_RANGE),
        4 => (i32::MIN as i128, i32::MAX as i128, raise::INT4_RANGE),
        _ => (i64::MIN as i128, i64::MAX as i128, raise::INT8_RANGE),
    };
    let floor = |a: i128, b: i128| a / b - (a % b != 0 && (a < 0) != (b < 0)) as i128;
    let ceil = |a: i128, b: i128| -floor(-a, b);
    // the values `v` whose `v op k` lies in `[lo, hi]`
    let pull = |op: u8, k: i128, (lo, hi): (i128, i128)| {
        let (l, h) = match op {
            CHECK_ADD => (lo - k, hi - k),
            CHECK_SUB => (lo + k, hi + k),
            CHECK_RSUB => (k - hi, k - lo),
            CHECK_NEG => (-hi, -lo),
            CHECK_CAST => (lo, hi),
            _ if k == 0 => if lo <= 0 && 0 <= hi { ALL } else { (1, 0) },
            _ if k > 0 => (ceil(lo, k), floor(hi, k)),
            _ => (ceil(hi, k), floor(lo, k)),
        };
        (l.max(ALL.0), h.min(ALL.1))
    };
    let steps = &c.steps[..c.steps.iter().position(|s| s.op == CHECK_NONE).unwrap_or(c.steps.len())];
    let mut r = KeyRanges { code, n: 0, keys: [(0, i64::MIN, i64::MAX); KEY_CHECK_KEYS], nulls: 0 };
    let mut start = 0;
    for i in 0..steps.len() {
        if steps[i].key != 0 {
            start = i;
        }
        let mut iv = (min, max);
        for s in steps[start..=i].iter().rev() {
            let k = if matches!(s.op, CHECK_NEG | CHECK_CAST) {
                0
            } else {
                let x = *words.get(s.word as usize)?;
                match s.cwidth { 2 => x as i16 as i128, 4 => x as i32 as i128, _ => x as i64 as i128 }
            };
            iv = pull(s.op, k, iv);
        }
        let key = ((steps[start].key & !CHECK_KEY_NULLABLE) as usize).checked_sub(1)?;
        let j = match r.keys[..r.n].iter().position(|e| e.0 == key) {
            Some(j) => j,
            None => {
                *r.keys.get_mut(r.n)? = (key, i64::MIN, i64::MAX);
                r.n += 1;
                r.n - 1
            }
        };
        if steps[start].key & CHECK_KEY_NULLABLE != 0 {
            r.nulls |= 1 << j;
        }
        let (lo, hi) = if iv.0 > iv.1 { (i64::MAX, i64::MIN) } else { (iv.0 as i64, iv.1 as i64) };
        let e = &mut r.keys[j];
        (e.1, e.2) = (e.1.max(lo), e.2.min(hi));
    }
    (r.n > 0).then_some(r)
}

/// The distinct form's owner functions (lower.md §6.12; t3's `owner_fns_distinct` over the descriptor): two
/// rounds — round 1 folds each partition's deduped pair records into the group accumulators of a phase-1 table
/// inside the region (`OwnerTop.round2_off`; a full pool asks the driver for a flush and resumes), the round's
/// end drains it into the round-2 buffers by the group hash, round 2 folds the accumulators across the owners
/// and keeps the count-ordered `n` best (the emit's form) or the runs. The words are `owner_fns`'s with the pair
/// words and the group estimate packed above the order.
pub fn distinct_fns(d: &OwnerDesc, spill_bits: u8) -> OwnerFns {
    distinct_fns_dense(d, spill_bits, None)
}

/// The dense round 1 takes an int group key of one word, NOT NULL on both sides (the pair's and the accumulator's
/// validity absent): `key - lo` indexes the group's accumulator. A date or timestamp key is a signed integer word too
/// (its datum: days / microseconds), ordered and compared as one, with the same lane extrema.
fn dense_form(d: &OwnerDesc) -> bool {
    d.acc.nkeys() == 1 && matches!(d.acc.key_kinds()[0], KeyKind::I32 | KeyKind::I64 | KeyKind::Date | KeyKind::Timestamp) && !d.acc.has_validity() && !d.layout.has_validity() && !float_riders(&d.layout)
}

/// A float rider (`sum` / `avg` over float4 / float8): its merge can raise PG's overflow (D244), which the dense walk has no
/// channel for; such a pair takes `fold_pair`. So does a wide numeric rider (V4R-38: the fold table's 4-bit kinds are full).
fn float_riders(pair: &RecordLayout) -> bool {
    pair.slot_kinds().iter().any(|k| matches!(k, SlotKind::SumF64 | SlotKind::SumF32 | SlotKind::F64Accum | SlotKind::SumNumWide | SlotKind::AvgNumWide | SlotKind::AggState | SlotKind::IntervalMin | SlotKind::IntervalMax | SlotKind::IntervalSum | SlotKind::NumSideMin | SlotKind::NumSideMax))
}

/// `distinct_fns` with the group key's domain `(lo, hi)` when the runner has it: the dense round 1 (`DenseArea`).
/// The domain is `t3_dense::word`'s, split over the high halves of `P_TOPK` and `P_BITS`; a domain the form does
/// not take (a text or nullable key, a span past the word's `t3_dense::SPAN_MAX`) leaves the pair path.
pub fn distinct_fns_dense(d: &OwnerDesc, spill_bits: u8, dense: Option<(i64, i64)>) -> OwnerFns {
    assert!(d.ctor.is_none() && d.ctor_desc.is_none() && d.pair != 0, "the distinct owner form");
    let acc = d.accumulator();
    let count = d.order_slot < ORDER_KEY;
    assert!(count || d.topk == 0, "the distinct form's key-ordered top-k is not an owner form");
    if count {
        assert!(d.topk > 0 && (d.order_slot as usize) < acc.nslots() && (d.order != 0 || acc.slot_kind(d.order_slot as usize) == SlotKind::Count), "the distinct form's count order: an accumulator Count slot or an order word over a slot, and a bound");
    }
    let dw = match dense {
        Some((lo, hi)) if dense_form(d) => t3_dense::word(lo, hi),
        _ => 0,
    };
    let mut p = [0u64; OWNER_PARAMS];
    p[P_TOPK] = d.topk as u64 | (dw & 0xffff_ffff) << 32;
    let order = if count { ORDER_BY_COUNT } else if d.order_slot == ORDER_KEY { ORDER_BY_KEY } else { 0 };
    p[P_ORDER] = order | (d.pair as u64) << 8 | (d.groups as u64) << 16 | null_arg_word(d);
    p[P_BITS] = if spill_bits == 0 { DEFAULT_BITS } else { spill_bits } as u64 | (dw >> 32) << 32 | arg_fold_word(d);
    put_slot(&mut p, d);
    let words: [u64; LAYOUT_WORDS] = d.layout.words();
    p[P_LAYOUT..P_ACC].copy_from_slice(&words);
    let acc_words: [u64; LAYOUT_WORDS] = d.acc.words();
    p[P_ACC..P_CHECK].copy_from_slice(&acc_words);
    if d.group_flags {
        p[P_FLAG_WORDS] = d.flag_words.0 as u64 | (d.flag_words.1 as u64) << 8;
    }
    if d.tags != NULL_ARG_NONE {
        p[P_TAGS] = d.tags as u64 + 1;
    }
    // the order word as the count form's; round 2 gates candidates by a count reaching the threshold for the count descending alone
    p[P_CHECK] = (d.order as u64) << ORDER_WORD_SHIFT;
    let order_slot2 = if count && d.order == 0 { d.order_slot } else { ORDER_NONE };
    OwnerFns { params: p, rounds: 2, order_slot: ORDER_NONE, order_slot2, min_bytes: distinct_min, open: distinct_open, reduce: distinct_reduce, end_round: distinct_end, reduce2: distinct_reduce2, fold: Some(if dw != 0 { distinct_fold } else { owner_fold }) }
}

/// The round-2 hot table's floor (two slots per estimated group, clamped to [floor, ROUND2_T1_MAX]). SWEPT FLAT
/// (held-out dev table `tps`: 4,000,000 rows from `generate_series`, 64 to 4096 groups and an expression key PostgreSQL
/// estimates at 200 groups that holds 65,536; same binary, floors 16 KiB, 64 KiB, 1 MiB and 4 MiB against 256 KiB, W0
/// and W8, arms interleaved): every statement within 0.98..1.02 of 256 KiB, rows identical, so the value is not tuned
/// to a benchmark. Swept on the exec owner's copy (owner.rs `distinct_open`); the hand arm t3.rs mirrors it.
const ROUND2_T1_BYTES: u64 = 256 << 10;
/// `OwnerTop.resume` after the round's end: the empty accumulator of `count(DISTINCT x)` alone was written.
const ZERO_WRITTEN: u32 = u32::MAX;

/// The dense area's header: `[groups][appended][fold entries][H_SHAPE: `area_shape`][H_BAD: the fold met a key
/// outside the domain][H_IDENT: the live table is the identity fold's]` `u32`s, then `[H_POS: the records the
/// live table's fold was given][H_FILL: per split depth, the stream position its table filled at]` `u64`s, then
/// the fold table, a `u32` slot per key of the domain (the group's 1-based accumulator; 0: unseen), the accumulators.
const DENSE_HDR: usize = 8 * (H_POS + t3_dense::Fills::WORDS);
const H_SHAPE: usize = 3;
const H_BAD: usize = 4;
const H_IDENT: usize = 5;
const H_POS: usize = 4;
const H_FILL: usize = 5;
const _: () = assert!(H_FILL == H_POS + 1 && 4 * (H_IDENT + 1) <= 8 * H_POS);
/// With a domain, the fold's marker before the table space: `[0: u32][the area's distance back: u32]` (0: none).
const MARKER: usize = 8;

const fn dense_idx_bytes(span: u32, entries: usize) -> usize {
    (DENSE_HDR + 4 * entries + 4 * span as usize + 7) & !7
}

const fn dense_bytes(span: u32, entries: usize, acc: usize) -> usize {
    dense_idx_bytes(span, entries) + span as usize * acc
}

fn null_arg_word(d: &OwnerDesc) -> u64 {
    if d.null_arg.0 == NULL_ARG_NONE { 0 } else { (d.null_arg.0 as u64 + 1) << NULL_ARG_SHIFT | (d.null_arg.1 as u64) << (NULL_ARG_SHIFT + 8) | (d.group_flags as u64) << GROUP_FLAGS_BIT }
}

fn arg_fold_word(d: &OwnerDesc) -> u64 {
    d.arg_fold.iter().enumerate().filter(|(_, s)| **s != NULL_ARG_NONE).fold(0, |w, (i, s)| w | (*s as u64 + 1) << (ARG_FOLD_SHIFT + 8 * i as u32))
}

/// The pair's NULL-argument flag is set: it adds nothing to the pair count and folds no argument.
#[inline(always)]
unsafe fn flagged(q: *const u8, null_arg: Option<(usize, u8)>) -> bool {
    match null_arg {
        Some((w, b)) => *(q as *const u64).add(w) >> b & 1 != 0,
        None => false,
    }
}

/// The argument's key word `w` folded into accumulator slot `slot` by the slot's kind (a NEW pair's contribution to `sum / avg / min / max(DISTINCT x)`).
#[inline(always)]
unsafe fn fold_arg(a: &Rec<'_>, acc: &RecordLayout, slot: usize, w: u64) {
    match acc.slot_kind(slot) {
        SlotKind::SumI64 => a.add_i64(slot, w as i64),
        SlotKind::SumI128 | SlotKind::SumNum => a.add_i128(slot, w as i64 as i128),
        SlotKind::AvgI128 | SlotKind::AvgNum => a.add_avg(slot, w as i64 as i128, 1),
        SlotKind::MinI64 => a.min_i64(slot, w as i64),
        SlotKind::MaxI64 => a.max_i64(slot, w as i64),
        _ => {}
    }
}

/// The most key words an accumulator has: a record's key count is a `u8` in both tiers (the wide tier past eight).
const ACC_KEYS_MAX: usize = u8::MAX as usize;

/// Pair `q`'s accumulator key words: its first `nk`, or in the group-flags form the group words then the pair's flags
/// words the accumulator carries (CAP-28: one per 64 group keys, `Params::flag_words`), the argument's bit cleared where
/// it falls in one of them (a NULL group key's bit kept: it never meets a 0 key's group), built in `buf`.
#[inline(always)]
unsafe fn acc_keys<'a>(p: &Params, q: *const u8, nk: usize, buf: &'a mut [u64; ACC_KEYS_MAX]) -> &'a [u64] {
    match p.null_arg.filter(|_| p.group_flags) {
        Some((w, b)) => {
            let (first, carried) = p.flag_words;
            let g = nk - carried;
            core::ptr::copy_nonoverlapping(q as *const u64, buf.as_mut_ptr(), g);
            core::ptr::copy_nonoverlapping((q as *const u64).add(first), buf.as_mut_ptr().add(g), carried);
            if w - first < carried {
                buf[g + (w - first)] &= !(1u64 << b);
            }
            &buf[..nk]
        }
        None => core::slice::from_raw_parts(q as *const u64, nk),
    }
}

/// A deduped pair `q` into its group's accumulator `a`: the pair counted and the argument folded unless flagged, the riders always;
/// in the tagged form the pair counted into its tag's slot (`tag_slot`) and the riders of tag-0 pairs alone.
#[inline(always)]
unsafe fn note_pair(p: &Params, a: &Rec<'_>, q: *const u8, ns: usize) -> Result<(), FloatOverflow> {
    let w = *(q as *const u64).add(p.acc.nkeys() - p.flag_words.1);
    if !flagged(q, p.null_arg) {
        a.add_count(tag_slot(p.tags, w), 1);
        for s in p.arg_fold.into_iter().flatten() {
            fold_arg(a, &p.acc, s, w);
        }
    }
    if p.tags.is_some() && w != 0 {
        return Ok(());
    }
    fold_pair(a, &p.layout, &Rec::at(q as *mut u8, &p.layout), ns)
}

/// The accumulator slot a new pair counts into: `ACC_PAIRS`, or in the tagged form (`tags`: the first tag slot) the slot of
/// its tag `w` (the pair's first DISTINCT word: 0 is `ACC_PAIRS`, t > 0 is `tags + t - 1`).
#[inline(always)]
fn tag_slot(tags: Option<usize>, w: u64) -> usize {
    match tags {
        Some(first) if w != 0 => first + w as usize - 1,
        _ => ACC_PAIRS as usize,
    }
}

/// The pair's riding slots (the implicit count sentinel is not folded: the accumulator counts the pairs).
fn pair_slots(pair: &RecordLayout) -> usize {
    pair.nslots() - (pair.flags & LAYOUT_IMPLICIT_COUNT != 0) as usize
}

/// The dense round's fold table (lower.md §6.12.6): one `u32` per riding slot of the pair — the kind in bits 0–3,
/// the accumulator's word in bits 4–17, the pair's word in bits 18–31 — built once per region, walked flat per
/// pair; `fold_pair`'s kinds over NOT NULL layouts (a skipped invalid slot there is the no-op fold here).
const PF_COUNT: u32 = 0;
const PF_ADD: u32 = 1;
const PF_ADD128: u32 = 2;
const PF_AVG: u32 = 3;
const PF_MIN_I64: u32 = 4;
const PF_MAX_I64: u32 = 5;
const PF_MIN_U64: u32 = 7;
const PF_MAX_U64: u32 = 8;
const PF_MIN_VAR: u32 = 9;
const PF_MAX_VAR: u32 = 10;
/// `PF_ADD128` / `PF_AVG` over a numeric sum (`num_add`: its NaN / ±Infinity sentinel).
const PF_ADDNUM: u32 = 11;
const PF_AVGNUM: u32 = 12;
/// `bit_and / bit_or / bit_xor` riders (D252): a NOT NULL layout's words combined in place.
const PF_BIT_AND: u32 = 13;
const PF_BIT_OR: u32 = 14;
const PF_BIT_XOR: u32 = 15;
const PF_WORD_BITS: u32 = 14;

fn fold_entry(acc: &RecordLayout, pair: &RecordLayout, j: usize) -> u32 {
    let kind = match pair.slot_kind(j) {
        SlotKind::Count => PF_COUNT,
        SlotKind::SumI64 => PF_ADD,
        SlotKind::SumI128 => PF_ADD128,
        SlotKind::SumNum => PF_ADDNUM,
        SlotKind::AvgI128 => PF_AVG,
        SlotKind::AvgNum => PF_AVGNUM,
        SlotKind::MinI64 => PF_MIN_I64,
        SlotKind::MaxI64 => PF_MAX_I64,
        SlotKind::SumF64 | SlotKind::SumF32 | SlotKind::F64Accum | SlotKind::SumNumWide | SlotKind::AvgNumWide | SlotKind::AggState | SlotKind::IntervalMin | SlotKind::IntervalMax | SlotKind::IntervalSum | SlotKind::NumSideMin | SlotKind::NumSideMax => {
            unreachable!("a float, wide numeric, hosted, interval or order-key side rider leaves the dense form (float_riders)")
        }
        SlotKind::BitAnd => PF_BIT_AND,
        SlotKind::BitOr => PF_BIT_OR,
        SlotKind::BitXor => PF_BIT_XOR,
        SlotKind::MinCode => PF_MIN_U64,
        SlotKind::MaxCode => PF_MAX_U64,
        SlotKind::MinVar => PF_MIN_VAR,
        SlotKind::MaxVar => PF_MAX_VAR,
    };
    let (d, s) = (acc.slot_off(j + ACC_SLOT_BASE as usize) / 8, pair.slot_off(j) / 8);
    assert!(d < 1 << PF_WORD_BITS && s < 1 << PF_WORD_BITS, "a record word past the fold table's field");
    kind | d << 4 | s << (4 + PF_WORD_BITS)
}

struct Geometry {
    r2_off: usize,
    /// The round-2 region's end (the marker or the area follows).
    r2_end: usize,
    /// The dense area's offset (`table_off` when round 1 is the pair path).
    dense_off: usize,
    table_off: usize,
}

/// The distinct region: `[OwnerTop][the results: the count form's n records, or the run directory and the runs
/// (a quarter)][the round-2 phase-1 table and its pool (half of the rest)][the table space]`; with a domain the
/// round-2 region is a quarter of the rest (round 1 only appends to it) and the dense area sits between it and
/// the table space, when the table space keeps its floor — else the pair path's split.
fn distinct_geometry(p: &Params, bytes: usize) -> Option<Geometry> {
    let res_end = if p.count.is_some() { count_table_off(p.n, p.acc.size() as usize) } else { RUN_OFF + ((bytes.saturating_sub(RUN_OFF) / 4) & !7) };
    let r2_off = (res_end + 7) & !7;
    let rest = bytes.saturating_sub(r2_off);
    if let Some((_, span)) = p.dense {
        let r2_bytes = (rest / 4) & !7;
        let dense_off = r2_off + r2_bytes;
        let table_off = dense_off + dense_bytes(span, pair_slots(&p.layout), p.acc.size() as usize) + MARKER;
        if r2_bytes >= 4096 && table_off + DENSE_TABLE_FLOOR <= bytes {
            return Some(Geometry { r2_off, r2_end: dense_off, dense_off, table_off });
        }
    }
    let marker = if p.dense.is_some() { MARKER } else { 0 };
    let r2_bytes = (rest / 2) & !7;
    let table_off = r2_off + r2_bytes;
    (r2_bytes >= 4096 + marker && table_off + HASHAGG_HDR + 2 * p.rec <= bytes).then_some(Geometry { r2_off, r2_end: table_off - marker, dense_off: table_off, table_off })
}

/// The dense round 1's area (lower.md §6.12.4): the group's accumulator at `key - lo`, opened on first sight,
/// every deduped pair counted into `ACC_PAIRS` and its slots folded through the fold table — no hash, no probe,
/// no flush inside a partition; the round's end appends the accumulators to the round-2 buffers.
struct DenseArea {
    base: *mut u8,
    lo: i64,
    span: u32,
    /// The fold table and its count, the accumulators' offset and size, the pair-count word.
    entries: *const u32,
    n: usize,
    idx: usize,
    acc_size: usize,
    pairs_word: usize,
    acc: RecordLayout,
    null_arg: Option<(usize, u8)>,
    arg_fold: [Option<usize>; 3],
    /// The tagged form's first tag slot (`Params::tags`).
    tags: Option<usize>,
}

impl DenseArea {
    /// The area of an opened region: the marker before the table names its distance back through round 1 (0 = the
    /// pair path: a region the area did not fit, or round 2). Never `OwnerTop.resume`: the pair path's reduce resumes a
    /// flushed table by it (V4R-194: a region with a domain but no area read its resume index as an area).
    unsafe fn at(p: &Params, owner: *mut u8, h: &OwnerTop) -> Option<DenseArea> {
        let (lo, span) = p.dense?;
        let back = *(owner.add(h.table_off as usize - 4) as *const u32) as usize;
        (back != 0).then(|| DenseArea::new(owner.add(h.table_off as usize - back), lo, span, p))
    }

    unsafe fn new(base: *mut u8, lo: i64, span: u32, p: &Params) -> DenseArea {
        let n = pair_slots(&p.layout);
        DenseArea { base, lo, span, entries: base.add(DENSE_HDR) as *const u32, n, idx: dense_idx_bytes(span, n), acc_size: p.acc.size() as usize, pairs_word: p.acc.slot_off(ACC_PAIRS as usize) as usize / 8, acc: p.acc, null_arg: p.null_arg, arg_fold: p.arg_fold, tags: p.tags }
    }

    /// The header, the fold table and the index written once per region (a NULL or folded argument leaves the count to `note`: no shaped fold).
    unsafe fn open(&self, pair: &RecordLayout) {
        core::ptr::write_bytes(self.base, 0, self.idx);
        *self.hdr().add(2) = self.n as u32;
        *self.hdr().add(H_SHAPE) = if self.null_arg.is_some() || self.arg_fold[0].is_some() || self.tags.is_some() { 0 } else { area_shape(&self.acc, pair) };
        let e = self.entries as *mut u32;
        for j in 0..self.n {
            *e.add(j) = fold_entry(&self.acc, pair, j);
        }
    }

    #[inline(always)]
    unsafe fn hdr(&self) -> *mut u32 {
        self.base as *mut u32
    }

    #[inline(always)]
    unsafe fn rec(&self, j: u32) -> *mut u8 {
        self.base.add(self.idx + j as usize * self.acc_size)
    }

    /// A deduped pair record `q` into its group's accumulator; false when the key is outside the domain.
    #[inline(always)]
    unsafe fn note(&self, q: *const u8) -> bool {
        let ri = (*(q as *const i64)).wrapping_sub(self.lo) as u64;
        if ri >= self.span as u64 {
            return false;
        }
        let slot = (self.base.add(self.idx - 4 * self.span as usize) as *mut u32).add(ri as usize);
        let mut j = *slot;
        if j == 0 {
            j = *self.hdr() + 1;
            *self.hdr() = j;
            *slot = j;
            let d = self.rec(j - 1);
            core::ptr::write_bytes(d, 0, self.acc_size);
            let a = Rec::at(d, &self.acc);
            a.open(core::slice::from_raw_parts(q as *const u64, 1));
            a.add_count(0, 1);
        }
        let dst = self.rec(j - 1) as *mut u64;
        let w = *(q as *const u64).add(self.acc.nkeys());
        if !flagged(q, self.null_arg) {
            match self.tags {
                Some(_) => *dst.add(self.acc.slot_off(tag_slot(self.tags, w)) as usize / 8) += 1,
                None => *dst.add(self.pairs_word) += 1,
            }
            let a = Rec::at(dst as *mut u8, &self.acc);
            for s in self.arg_fold.into_iter().flatten() {
                fold_arg(&a, &self.acc, s, w);
            }
        }
        if self.tags.is_some() && w != 0 {
            return true;
        }
        let src = q as *const u64;
        for e in 0..self.n {
            let w = *self.entries.add(e);
            let (d, s) = (dst.add((w >> 4 & ((1 << PF_WORD_BITS) - 1)) as usize), src.add((w >> (4 + PF_WORD_BITS)) as usize));
            match w & 15 {
                PF_COUNT => *d = (*d).wrapping_add(*s & !MOVING),
                PF_ADD => *d = (*d).wrapping_add(*s),
                PF_ADD128 | PF_AVG | PF_ADDNUM | PF_AVGNUM => {
                    let (x, y) = (((*d as u128) | (*d.add(1) as u128) << 64) as i128, ((*s as u128) | (*s.add(1) as u128) << 64) as i128);
                    let v = (if w & 15 >= PF_ADDNUM { num_add(x, y) } else { x.wrapping_add(y) }) as u128;
                    *d = v as u64;
                    *d.add(1) = (v >> 64) as u64;
                    if w & 15 == PF_AVG || w & 15 == PF_AVGNUM {
                        *d.add(2) = (*d.add(2)).wrapping_add(*s.add(2));
                    }
                }
                PF_MIN_I64 => {
                    if (*s as i64) < *d as i64 {
                        *d = *s;
                    }
                }
                PF_MAX_I64 => {
                    if (*s as i64) > *d as i64 {
                        *d = *s;
                    }
                }
                PF_MIN_U64 => {
                    if *s < *d {
                        *d = *s;
                    }
                }
                PF_BIT_AND => *d &= *s,
                PF_BIT_OR => *d |= *s,
                PF_BIT_XOR => *d ^= *s,
                // a text image (`MinVar` / `MaxVar`): the addresses of two arena varlenas, ordered by their bytes
                PF_MIN_VAR | PF_MAX_VAR => {
                    if *s != 0 {
                        let take = *d == 0 || {
                            let c = gravity_abi::breaker::varlena_cmp(*s as *const u8, *d as *const u8);
                            if w & 15 == PF_MIN_VAR { c == core::cmp::Ordering::Less } else { c == core::cmp::Ordering::Greater }
                        };
                        if take {
                            *d = *s;
                        }
                    }
                }
                _ => {
                    if *s > *d {
                        *d = *s;
                    }
                }
            }
        }
        true
    }

    /// Every accumulator into `r2`'s buffer of its group-hash partition; `REDUCE_FLUSH` (resumed) when the pool is full.
    unsafe fn append(&self, r2: *mut T1) -> i64 {
        let n = *self.hdr();
        let pbits = (*r2).pbits;
        let mut ap = T1::appender(r2);
        let mut i = *self.hdr().add(1);
        while i < n {
            let src = self.rec(i);
            let part = (KeyHash::of(core::slice::from_raw_parts(src as *const u64, 1))).part(pbits as u32);
            if !ap.append(part, src) {
                ap.finish();
                *self.hdr().add(1) = i;
                return REDUCE_FLUSH;
            }
            i += 1;
        }
        ap.finish();
        *self.hdr().add(1) = n;
        0
    }
}

/// 0 leaves the count to the reduce.
fn area_shape(acc: &RecordLayout, pair: &RecordLayout) -> u32 {
    if !dense_folds(pair) {
        return 0;
    }
    let fixed = acc.flags & LAYOUT_IMPLICIT_COUNT == 0 && acc.wide.ptr().is_none() && pair.wide.ptr().is_none() && acc.slot_off(0) as usize == 8 * AREA_SENTINEL && acc.slot_off(ACC_PAIRS as usize) as usize == 8 * AREA_PAIRS;
    let riding = (0..pair_slots(pair)).all(|j| acc.slot_kind(j + ACC_SLOT_BASE as usize) == pair.slot_kind(j) && acc.slot_off(j + ACC_SLOT_BASE as usize) as usize == pair.slot_off(j) as usize + AREA_SHIFT);
    (fixed && riding) as u32
}

/// `OwnerFns.fold` of the dense form, the count inside the fold (lower.md §6.12.8): behind a live marker a shaped
/// pair table is the identity fold's (`owner_fold::area_fold`), counted into the area as it is folded; anything else is
/// `owner_fold`. The driver re-reads a partition's stream from its start under a finer split when a table fills,
/// so the header keeps the stream position (`H_POS`, zeroed at every table's end) and the position each depth's
/// table filled at (`H_FILL`): the records before the furthest fill of the coarser depths were counted already
/// and the run's leading ones up to it are quiet.
/// # Safety
/// `FoldFn`'s contract over a table laid at the region's `table_off`.
unsafe fn distinct_fold(params: *const u64, t: *mut HashAgg, recs: *const u8, nrecs: usize, split: gravity_abi::breaker::SubSplit, cand: *mut gravity_abi::breaker::Candidates) -> i64 {
    let back = *(t as *const u32).sub(1) as usize;
    if back != 0 {
        let base = (t as *mut u8).sub(back);
        let hdr = base as *mut u32;
        let sh = *hdr.add(H_SHAPE);
        let dom = t3_dense::domain((*params.add(P_TOPK) >> 32) | (*params.add(P_BITS) >> 32) << 32);
        if let (true, Some((lo, span)), Some(f)) = (sh != 0, dom, Fold::dense(&(*t).layout)) {
            let idx = dense_idx_bytes(span, *hdr.add(2) as usize);
            let acc = (*(params.add(P_ACC) as *const RecordLayout)).size() as usize;
            // the records noted at the insert before the coarser depths' fills (this depth's on `merge.rs`'s same-depth re-read) stay quiet
            let fills = (base as *mut u64).add(H_POS);
            let mut fill = 0u64;
            *hdr.add(H_IDENT) = 1;
            let area = Area::over(hdr, hdr.add(H_BAD), base.add(idx - 4 * span as usize) as *mut u32, base.add(idx), acc, lo, span, t3_dense::Fills::quiet(fills, split), &mut fill);
            let v = area_fold(t, recs, nrecs, split, &area, &f, pair_slots(&(*t).layout));
            t3_dense::Fills::advance(fills, split, nrecs, v, fill);
            return v;
        }
    }
    owner_fold(params, t, recs, nrecs, split, cand)
}

fn distinct_min(params: *const u64, owners: u32) -> u64 {
    // SAFETY: `OWNER_PARAMS` words.
    let p = unsafe { params_of(params) };
    let one = distinct_floor(&p, p.groups as u64);
    if owners <= 1 {
        return one;
    }
    // KEYED-28: round 2 re-sinks every group by its hash, so one of the owners keeps its share of the planned groups (the
    // maximum-load excess included). A partition past its owner's region is redone on one owner (parallel lib.rs
    // `overflow2`, D533): a low estimate costs time, never the statement.
    distinct_floor(&p, owner_share(p.groups as u64, owners)).min(one)
}

/// The least region of an owner whose runs hold `groups` groups' accumulators (`dense_floor`: and the dense area).
fn distinct_floor(p: &Params, groups: u64) -> u64 {
    let acc = p.acc.size() as u64;
    // D563: the count form keeps n records, but its region still folds every partition's pairs and round-2 accumulators:
    // floored like the runs form's (own_all takes a two-round shape's least fitting floor; a count-kept floor alone left
    // a W0 table of a few KiB, fold_sub re-reading the spill at every split level)
    let runs = RUN_OFF as u64 + 4 * (groups.max(64) * acc).min(RUNS_FLOOR_MAX);
    let res = if p.count.is_some() { runs.max(count_table_off(p.n, acc as usize) as u64) } else { runs };
    dense_floor(p, res + 2 * 4096 + (HASHAGG_HDR + 64 * p.rec) as u64)
}

/// The keys of the domain `[lo, hi]` (0 when empty).
fn span_of(lo: i64, hi: i64) -> u64 {
    (hi as i128 - lo as i128 + 1).clamp(0, u64::MAX as i128) as u64
}

/// With a domain whose area fits the least grant admission hands out (program cost `dense_pays` against cost runtime
/// `GRANT_FLOOR`, EX-05: the cost reading where a fixed 64 MiB bound stood), the least region past
/// `floor` that `distinct_geometry` lays the area in (D586): own_all sizes a two-round owner at its least fitting floor, so a
/// floor without the area left W0's one owner on the pair path (W0: area=false over a 131071-key domain, the W8 owners'
/// larger regions laid it).
fn dense_floor(p: &Params, floor: u64) -> u64 {
    let Some((_, span)) = p.dense.filter(|&(_, span)| dense_pays(span as u64, p.acc.size() as u64, u64::MAX, GRANT_FLOOR)) else { return floor };
    let laid = |b: u64| distinct_geometry(p, b as usize).is_some_and(|g| g.dense_off != g.table_off);
    let mut b = floor.max((dense_bytes(span, pair_slots(&p.layout), p.acc.size() as usize) + MARKER + DENSE_TABLE_FLOOR) as u64);
    for _ in 0..64 {
        if laid(b) {
            return b.next_multiple_of(8);
        }
        b += (b / 8).max(64 << 10);
    }
    floor
}

unsafe fn distinct_open(params: *const u64, owner: *mut u8, bytes: usize) -> i64 {
    let p = params_of(params);
    let Some(Geometry { r2_off, r2_end, dense_off, table_off }) = distinct_geometry(&p, bytes) else { return unsupported(UNSUPPORTED_SCRATCH) };
    let acc = p.acc;
    let groups = p.groups.max(1) as u64;
    let pbits2 = gravity_kernels::table::params::spill_bits(groups, acc.size()).max(3);
    let r2 = owner.add(r2_off);
    core::ptr::write_bytes(r2, 0, 64);
    let r2_bytes = (r2_end - r2_off) as u32;
    // the hot table: two slots per estimated group, at most a quarter of its region; the dense round only appends
    let want = if dense_off != table_off { 4096 } else { (2 * groups * acc.size() as u64).clamp(ROUND2_T1_BYTES, ROUND2_T1_MAX).next_power_of_two() as u32 };
    T1::at(r2, &acc, pbits2, want.min(r2_bytes / 4), r2_bytes);
    let area = p.dense.filter(|_| dense_off != table_off);
    if let Some((lo, span)) = area {
        DenseArea::new(owner.add(dense_off), lo, span, &p).open(&p.layout);
    }
    if p.dense.is_some() {
        *(owner.add(table_off - MARKER) as *mut u32) = 0;
        *(owner.add(table_off - 4) as *mut u32) = if area.is_some() { (table_off - dense_off) as u32 } else { 0 };
    }
    *(owner as *mut OwnerTop) = OwnerTop { len: 0, n: p.n as u32, table_off: table_off as u32, worst: 0, round2_off: r2_off as u32, resume: 0, total: 0, groups: 0, records: 0, thr: 0, key_lo: gravity_abi::breaker::KEY_LO_NONE, key_hi: gravity_abi::breaker::KEY_HI_NONE, cell: core::cell::UnsafeCell::new([]) };
    table_off as i64
}

/// The pair's slots into its group's accumulator (`Rec::fold`'s vocabulary, the slots offset by `ACC_SLOT_BASE`).
unsafe fn fold_pair(a: &Rec<'_>, pair: &RecordLayout, s: &Rec<'_>, ns: usize) -> Result<(), FloatOverflow> {
    if s.null_seen() {
        a.set_null_seen();
    }
    for j in 0..ns {
        if !s.valid(j) {
            continue;
        }
        let d = j + ACC_SLOT_BASE as usize;
        match pair.slot_kind(j) {
            SlotKind::Count => a.add_count(d, s.count(j)),
            SlotKind::SumI64 => a.add_i64(d, s.i64(j)),
            SlotKind::SumI128 => a.add_i128(d, s.i128(j)),
            SlotKind::SumNum => a.add_num(d, s.i128(j)),
            SlotKind::MinI64 => a.min_i64(d, s.i64(j)),
            SlotKind::MaxI64 => a.max_i64(d, s.i64(j)),
            SlotKind::AvgI128 => {
                let (sum, n) = s.avg(j);
                a.add_avg(d, sum, n)
            }
            SlotKind::AvgNum => {
                let (sum, n) = s.avg(j);
                a.add_avg_num(d, sum, n)
            }
            SlotKind::SumNumWide => a.add_num256(d, s.num256(j)),
            SlotKind::AvgNumWide => {
                let (sum, n) = s.avg256(j);
                a.add_avg_num256(d, sum, n)
            }
            SlotKind::SumF64 => a.add_f64(d, s.f64(j))?,
            SlotKind::SumF32 => a.add_f32(d, s.f64(j))?,
            SlotKind::F64Accum => a.combine_f64_accum(d, s.f64_accum(j))?,
            SlotKind::BitAnd | SlotKind::BitOr | SlotKind::BitXor => a.fold_bits(d, s.i64(j)),
            SlotKind::MinCode => {
                let (part, code) = s.code(j);
                a.min_code(d, part, code)
            }
            SlotKind::MaxCode => {
                let (part, code) = s.code(j);
                a.max_code(d, part, code)
            }
            SlotKind::MinVar => a.fold_var(d, s.var(j), true),
            SlotKind::MaxVar => a.fold_var(d, s.var(j), false),
            SlotKind::IntervalMin => a.fold_interval(d, s.interval(j), true),
            SlotKind::IntervalMax => a.fold_interval(d, s.interval(j), false),
            SlotKind::IntervalSum => a.fold_interval_sum(d, &s.interval_sum(j))?,
            SlotKind::AggState => a.move_state(d, s, j)?,
            // N13: an order-key layout (LAYOUT_KEY_TIES) keeps no DISTINCT sibling (decompose settle_keyed: a pair fold is hosted)
            SlotKind::NumSideMin | SlotKind::NumSideMax => unreachable!("an order-key side slot in a DISTINCT pair's riders"),
        }
    }
    Ok(())
}

/// Round 1: every deduped pair of the partition probed into the round-2 table by its group words and folded
/// (a fresh accumulator takes the owner-record sentinel); a full pool returns `REDUCE_FLUSH` and resumes.
unsafe fn distinct_reduce(params: *const u64, owner: *mut u8, table: *const HashAgg, _site: *mut Site, _cand: *const u32, _ncand: i64) -> i64 {
    let p = params_of(params);
    let h = &mut *(owner as *mut OwnerTop);
    let t = &*table;
    let dense = DenseArea::at(&p, owner, h);
    if h.resume == 0 || dense.is_some() {
        h.records += t.len as u64;
    }
    let acc = p.acc;
    let nk = acc.nkeys();
    let pair = t.layout;
    let ns = pair.nslots() - (pair.flags & LAYOUT_IMPLICIT_COUNT != 0) as usize;
    let r2 = owner.add(h.round2_off as usize) as *mut T1;
    let (rec, sent, base) = (pair.size() as usize, pair.sentinel_off() as usize, t.base_ptr());
    if let Some(d) = dense {
        // D597: a key outside the domain (a stale one) ends round 1: the driver reruns it through the plain form
        if *d.hdr().add(H_BAD) != 0 {
            return unsupported(UNSUPPORTED_DOMAIN);
        }
        // the identity fold counted this table (it holds identities: nothing to walk); the next table's stream starts at 0
        if core::mem::replace(&mut *d.hdr().add(H_IDENT), 0) != 0 {
            t3_dense::Fills::reset((d.base as *mut u64).add(H_POS));
            return 0;
        }
        // the dense round: the partition's deduped pairs indexed into their groups' accumulators, nothing probed
        for i in 0..t.cap as usize {
            let q = base.add(i * rec);
            if *(q.add(sent) as *const u64) != 0 && !d.note(q) {
                return unsupported(UNSUPPORTED_DOMAIN);
            }
        }
        return 0;
    }
    let mut buf = [0u64; ACC_KEYS_MAX];
    for i in h.resume as usize..t.cap as usize {
        let q = base.add(i * rec);
        if *(q.add(sent) as *const u64) == 0 {
            continue;
        }
        let keys = acc_keys(&p, q, nk, &mut buf);
        let hash = KeyHash::of(keys);
        let d = match T1::probe(r2, keys, hash) {
            Some(d) => d,
            None => {
                let home = (*r2).home(hash);
                if !T1::evict(r2, home) {
                    h.resume = i as u32;
                    return REDUCE_FLUSH;
                }
                T1::open_at_hashed(r2, home, keys, hash)
            }
        };
        let a = Rec::at(d, &acc);
        if a.count(0) == 0 {
            a.add_count(0, 1);
        }
        if note_pair(&p, &a, q, ns).is_err() {
            return FloatOverflow::STATUS;
        }
    }
    h.resume = 0;
    0
}

/// After the owner's last round-1 partition: `count(DISTINCT x)` alone over no pairs writes its empty accumulator
/// (one row on an empty input), then the round-2 table drains into the buffers by the group hash.
unsafe fn distinct_end(params: *const u64, owner: *mut u8) -> i64 {
    let p = params_of(params);
    let h = &mut *(owner as *mut OwnerTop);
    if h.round2_off == 0 {
        return 0;
    }
    let r2 = owner.add(h.round2_off as usize) as *mut T1;
    let dense = DenseArea::at(&p, owner, h);
    if let Some(d) = dense {
        // the dense round's end: the accumulators to the round-2 buffers
        let rc = d.append(r2);
        if rc != 0 {
            return rc;
        }
        // round 2's tables are the generic fold's
        *(owner.add(h.table_off as usize - 4) as *mut u32) = 0;
    }
    if p.acc.nkeys() == 0 && h.records == 0 && h.resume != ZERO_WRITTEN {
        let hash = KeyHash::of(&[]);
        let d = match T1::probe(r2, &[], hash) {
            Some(d) => d,
            None => {
                let home = (*r2).home(hash);
                if !T1::evict(r2, home) {
                    return REDUCE_FLUSH;
                }
                T1::open_at_hashed(r2, home, &[], hash)
            }
        };
        Rec::at(d, &p.acc).add_count(0, 1);
        h.resume = ZERO_WRITTEN;
    }
    if !T1::drain(r2) {
        return REDUCE_FLUSH;
    }
    0
}

/// Round 2: a partition's accumulators folded across the owners — the count-ordered `n` best, or a run (sorted
/// by the first key under `ORDER_KEY`) in the runs area before the round-2 buffers.
unsafe fn distinct_reduce2(params: *const u64, owner: *mut u8, table: *const HashAgg, site: *mut Site, cand: *const u32, ncand: i64) -> i64 {
    let p = params_of(params);
    let h = &mut *(owner as *mut OwnerTop);
    let t = &mut *(table as *mut HashAgg);
    let acc = p.acc;
    let rec = acc.size() as usize;
    h.groups += t.len as u64;
    if let Some(slot) = p.count {
        let mut top = CountTop { base: owner.add(OWNERTOP_HDR), rec, n: p.n, len: h.len as usize, worst: h.worst as usize, off_count: acc.slot_off(slot) as usize, layout: acc, reps: p.reps, flags: p.tie_flags, text: p.tie_text, site, order: p.order };
        top.offer_folded(t, cand, ncand);
        h.len = top.len as u32;
        h.worst = top.worst as u32;
        h.thr = top.threshold();
        return 0;
    }
    if p.ordered {
        t.sort_dense_by_key(0);
    }
    let n = t.len as usize;
    if n == 0 {
        return 0;
    }
    let runs = h.worst as usize;
    let off = RUN_OFF + h.total as usize;
    if runs >= MAX_RUNS || off + n * rec > h.round2_off as usize {
        return unsupported(UNSUPPORTED_SCRATCH);
    }
    let mut m = 0usize;
    for i in 0..t.cap as usize {
        let r = t.record(i);
        if r.used() {
            core::ptr::copy_nonoverlapping(r.as_ptr(), owner.add(off + m * rec), rec);
            m += 1;
        }
    }
    let dir = owner.add(DIR_OFF) as *mut u32;
    *dir.add(2 * runs) = off as u32;
    *dir.add(2 * runs + 1) = m as u32;
    h.worst = runs as u32 + 1;
    h.total += (m * rec) as u64;
    h.len += m as u32;
    0
}

/// The generic owner functions of `d` (`spill_bits`: the pipeline's `Spill::Partitioned` bits, 0 = default); the
/// distinct form's (`d.pair`) are `distinct_fns`.
pub fn owner_fns(d: &OwnerDesc, spill_bits: u8) -> OwnerFns {
    owner_fns_dense(d, spill_bits, None)
}

/// `owner_fns` with the group key's dense domain for the two-round distinct form (`distinct_fns_dense`).
pub fn owner_fns_dense(d: &OwnerDesc, spill_bits: u8, dense: Option<(i64, i64)>) -> OwnerFns {
    owner_fns_at(d, spill_bits, dense, true)
}

/// `owner_fns_dense` for a producer that can (`parallel`) or cannot run parallel owners. STEP-19: a serial producer's pair
/// set partitioned by its group words (lowerv4 count_cut.rs `pair_partition`: always, on one owner) takes the two-round owner,
/// whose round 1 then sees each group in one partition and round 2 re-sinks one accumulator per group; the one-round fold
/// stays the parallel producers' (measured on the dev table tpd, topn STATUS "STEP-19 ABAB2": the two-round owner over the
/// group route is never slower than the pair route at W0 or W8, the one-round fold up to 1.47x slower than it at W0).
fn owner_fns_at(d: &OwnerDesc, spill_bits: u8, dense: Option<(i64, i64)>, parallel: bool) -> OwnerFns {
    assert!(d.ctor.is_none() && d.ctor_desc.is_none(), "the generic owner phase serves a descriptor without a constructor");
    if d.pair != 0 {
        // one round where the form is declared (lower.md §15.9): the plain set without riding slots, the pair set partitioned by its group words
        let gw = d.layout.nkeys() - d.pair as usize;
        let declared = if gw == 0 { d.acc.nslots() == ACC_SLOT_BASE as usize } else { d.layout.pkeys as usize == gw && parallel };
        if declared && !d.group_flags && d.tags == NULL_ARG_NONE && d.order_slot != ORDER_KEY && (d.topk == 0 || d.order_slot == ACC_PAIRS) {
            return gravity_hosts::ops::group::distinct_fold::distinct_fold_fns_desc(d, spill_bits);
        }
        return distinct_fns_dense(d, spill_bits, dense);
    }
    let count = d.order_slot < ORDER_KEY;
    if count {
        assert!(d.topk > 0 && (d.order_slot as usize) < d.layout.nslots() && (d.order != 0 || d.layout.slot_kind(d.order_slot as usize) == SlotKind::Count), "the count-ordered owner form: a Count slot or an order word over a slot, and a bound");
    }
    let mut p = [0u64; OWNER_PARAMS];
    p[P_TOPK] = d.topk as u64;
    let tie = if d.tie_flags == TIE_FLAGS_NONE { 0 } else { (d.tie_flags as u64 + 1) << TIE_FLAGS_SHIFT };
    p[P_ORDER] = (if count { ORDER_BY_COUNT } else if d.order_slot == ORDER_KEY { ORDER_BY_KEY } else { 0 }) | (d.groups as u64) << 16 | tie;
    let flagged = d.observe_key == OBSERVE_KEY_FLAGGED;
    if d.observe_key != OBSERVE_NONE && observable_key(&d.layout, flagged).is_some() {
        p[P_ORDER] |= 1 << OBSERVE_KEY_BIT | (flagged as u64) << OBSERVE_FLAGGED_BIT;
    }
    // D3080: the count form's int-text tie keys in the bytes the distinct form keeps its argument folds in (`pair` names which)
    p[P_BITS] = if spill_bits == 0 { DEFAULT_BITS } else { spill_bits } as u64 | if count { (d.tie_text as u64 & 0xff_ffff) << ARG_FOLD_SHIFT } else { 0 };
    put_slot(&mut p, d);
    let words: [u64; LAYOUT_WORDS] = d.layout.words();
    p[P_LAYOUT..P_ACC].copy_from_slice(&words);
    let acc_words: [u64; LAYOUT_WORDS] = d.acc.words();
    p[P_ACC..P_CHECK].copy_from_slice(&acc_words);
    p[P_CHECK] = (d.order as u64) << ORDER_WORD_SHIFT;
    // the fold lists candidates by a count reaching the threshold: the count descending alone; any other order offers every
    // record of a folded partition (`ORDER_NONE`), its final settled only there
    let order_slot = if count && d.order == 0 { d.order_slot } else { ORDER_NONE };
    let reduce = if p[P_ORDER] >> OBSERVE_KEY_BIT & 1 != 0 { owner_reduce_observe } else { owner_reduce };
    OwnerFns { params: p, rounds: 1, order_slot, order_slot2: order_slot, min_bytes: owner_min, open: owner_open, reduce, end_round: owner_end, reduce2: reduce, fold: Some(gravity_kernels::table::owner_fold::owner_fold) }
}

struct Params {
    n: usize,
    ordered: bool,
    /// The count slot of the count-ordered form.
    count: Option<usize>,
    bits: u8,
    rec: usize,
    layout: RecordLayout,
    /// The count-ordered form's text keys (item 32): `(representative slot, code lane)` per key.
    reps: Reps,
    /// The distinct form: the pair words (0 otherwise), the group estimate and the accumulator layout (`layout` otherwise).
    pair: u8,
    groups: u32,
    acc: RecordLayout,
    /// The dense round 1's domain `(lo, span)` of the group key (`t3_dense::domain`); `None`: the pair path.
    dense: Option<(i64, u32)>,
    /// The NULL-argument flag `(key word, bit)` and the argument-fold slots (`OwnerDesc::null_arg` / `arg_fold`).
    null_arg: Option<(usize, u8)>,
    arg_fold: [Option<usize>; 3],
    /// The tagged distinct form's first tag slot (`OwnerDesc::tags`, `P_TAGS`).
    tags: Option<usize>,
    group_flags: bool,
    /// The group-flags form's `(the pair's first flags word, the flags words the accumulator carries)`; `(0, 0)` outside it.
    flag_words: (usize, usize),
    /// The count form's first NULL flags word its nullable tie keys read (`OwnerDesc::tie_flags`); `None`: no flag read.
    tie_flags: Option<usize>,
    /// D3080: the count form's int-text tie keys (`OwnerDesc::tie_text`).
    tie_text: u32,
    /// The count form's key check: the raise code and `(key word, lo, hi)` per checked key.
    check: Option<KeyRanges>,
    /// The count form's order word (`OwnerDesc::order`; 0: the count descending).
    order: u64,
    /// D3153: the runs form's bound (`P_FLOOR`): `(the slot's byte offset, the least and the most value kept)`.
    floor: Option<(usize, i64, i64)>,
    /// D-06: a top's bound instead (`P_FLOOR`'s high bits): `(n, below)`, the least / most value each partition's own.
    top: Option<(usize, bool)>,
}

/// `d`'s order slot and representatives into the owner's params (`pack_slot`'s first word at `P_SLOT`, the rest at
/// `P_SLOT_MORE`).
fn put_slot(p: &mut [u64; OWNER_PARAMS], d: &OwnerDesc) {
    let w = pack_slot(d.order_slot, &d.tie_reps());
    p[P_SLOT] = w[0];
    p[P_SLOT_MORE..P_SLOT_MORE + REP_WORDS - 1].copy_from_slice(&w[1..]);
}

/// The representatives `put_slot` wrote.
fn slot_reps(w: &[u64]) -> Reps {
    let mut x = [0u64; REP_WORDS];
    x[0] = w[P_SLOT];
    x[1..].copy_from_slice(&w[P_SLOT_MORE..P_SLOT_MORE + REP_WORDS - 1]);
    unpack_slot(&x)
}

unsafe fn params_of(p: *const u64) -> Params {
    let w = core::slice::from_raw_parts(p, OWNER_PARAMS);
    let mut words = [0u64; LAYOUT_WORDS];
    words.copy_from_slice(&w[P_LAYOUT..P_ACC]);
    // SAFETY: the words `owner_fns` packed.
    let layout: RecordLayout = RecordLayout::from_words(words);
    words.copy_from_slice(&w[P_ACC..P_CHECK]);
    let acc: RecordLayout = RecordLayout::from_words(words);
    let order = w[P_ORDER] & 0xff;
    let (pair, groups) = ((w[P_ORDER] >> 8) as u8, (w[P_ORDER] >> 16) as u32);
    let count = (order == ORDER_BY_COUNT).then_some(w[P_SLOT] as u8 as usize);
    let dense = t3_dense::domain((w[P_TOPK] >> 32) | (w[P_BITS] >> 32) << 32);
    let na = (w[P_ORDER] >> NULL_ARG_SHIFT) as u8;
    let null_arg = (na != 0 && pair != 0).then(|| (na as usize - 1, (w[P_ORDER] >> (NULL_ARG_SHIFT + 8)) as u8 & 63));
    let tie = (w[P_ORDER] >> TIE_FLAGS_SHIFT) as u8;
    let tie_flags = (tie != 0 && pair == 0 && order == ORDER_BY_COUNT).then(|| tie as usize - 1);
    let fold = |i: u32| { let s = (w[P_BITS] >> (ARG_FOLD_SHIFT + 8 * i)) as u8; (s != 0 && pair != 0).then(|| s as usize - 1) };
    let tie_text = if pair == 0 && order == ORDER_BY_COUNT { (w[P_BITS] >> ARG_FOLD_SHIFT) as u32 & 0xff_ffff } else { 0 };
    let nk = (w[P_CHECK] as u8 as usize).min(KEY_CHECK_KEYS);
    let check = (nk != 0).then(|| {
        let mut c = KeyRanges { code: (w[P_CHECK] >> 8) as u8, n: nk, keys: [(0, 0, 0); KEY_CHECK_KEYS], nulls: 0 };
        for (i, e) in c.keys[..nk].iter_mut().enumerate() {
            let b = (w[P_CHECK_KEYS] >> (8 * i as u32)) as u8;
            *e = ((b & !CHECK_KEY_NULLABLE) as usize, w[P_CHECK + 2 + 2 * i] as i64, w[P_CHECK + 3 + 2 * i] as i64);
            c.nulls |= u8::from(b & CHECK_KEY_NULLABLE != 0) << i;
        }
        c
    });
    let group_flags = pair != 0 && w[P_ORDER] >> GROUP_FLAGS_BIT != 0;
    let flag_words = if group_flags { (w[P_FLAG_WORDS] as u8 as usize, (w[P_FLAG_WORDS] >> 8) as u8 as usize) } else { (0, 0) };
    let tags = (pair != 0 && w[P_TAGS] != 0).then(|| w[P_TAGS] as usize - 1);
    let order_word = w[P_CHECK] >> ORDER_WORD_SHIFT & 0xffff;
    let floor = (pair == 0 && order != ORDER_BY_COUNT && w[P_TOPK] as u32 == 0 && w[P_FLOOR] != 0).then(|| ((w[P_FLOOR] as u32 - 1) as usize, w[P_FLOOR + 1] as i64, w[P_FLOOR + 2] as i64));
    let top = floor.and_then(|_| (w[P_FLOOR] >> 32 & 0x7fff_ffff != 0).then(|| ((w[P_FLOOR] >> 32 & 0x7fff_ffff) as usize, w[P_FLOOR] >> 63 != 0)));
    Params { n: w[P_TOPK] as u32 as usize, ordered: order == ORDER_BY_KEY, count, bits: w[P_BITS] as u8, rec: layout.size() as usize, layout, reps: slot_reps(w), pair, groups, acc, dense, null_arg, arg_fold: [fold(0), fold(1), fold(2)], tags, group_flags, flag_words, tie_flags, tie_text, check, order: order_word, floor, top }
}

const fn count_table_off(n: usize, rec: usize) -> usize {
    (OWNERTOP_HDR + n * rec + 7) & !7
}

const fn topk_table_off(n: usize, rec: usize) -> usize {
    (OWNERTOP_HDR + n * rec + 4 * n + 7) & !7
}

fn owner_min(params: *const u64, _owners: u32) -> u64 {
    // SAFETY: `OWNER_PARAMS` words.
    let p = unsafe { shape_of(params) };
    let table = HASHAGG_HDR + 64 * p.rec;
    if p.count {
        return (count_table_off(p.n, p.rec) + table) as u64;
    }
    if p.n > 0 {
        return (topk_table_off(p.n, p.rec) + table) as u64;
    }
    RUN_OFF as u64 + runs_floor(&p).max(2 * table as u64)
}

/// What the region's size and laying read of the params (`owner_min`, `owner_open`): the kept count, the record's bytes,
/// the form, the groups and the spill bits, as `params_of` decodes them (D3075: not the whole decode, twice per owner phase
/// of a rescanned fold).
struct Shape {
    n: usize,
    rec: usize,
    count: bool,
    groups: u32,
    bits: u8,
}

/// # Safety
/// `p` points at `OWNER_PARAMS` words `owner_fns` packed.
unsafe fn shape_of(p: *const u64) -> Shape {
    let w = core::slice::from_raw_parts(p, OWNER_PARAMS);
    let mut words = [0u64; LAYOUT_WORDS];
    words.copy_from_slice(&w[P_LAYOUT..P_ACC]);
    let layout: RecordLayout = RecordLayout::from_words(words);
    Shape { n: w[P_TOPK] as u32 as usize, rec: layout.size() as usize, count: w[P_ORDER] & 0xff == ORDER_BY_COUNT, groups: (w[P_ORDER] >> 16) as u32, bits: w[P_BITS] as u8 }
}

/// The runs form's result over every owner: `OwnerDesc.groups` records, else `bits` partitions of 512 KiB (`spill_bits`); a page up.
fn runs_floor(p: &Shape) -> u64 {
    let bytes = if p.groups > 0 { p.groups as u64 * p.rec as u64 } else { (1u64 << p.bits) * (512 << 10) };
    (bytes + 4095) & !4095
}

unsafe fn owner_open(params: *const u64, owner: *mut u8, bytes: usize) -> i64 {
    let p = shape_of(params);
    let table_off = if p.count {
        count_table_off(p.n, p.rec)
    } else if p.n > 0 {
        topk_table_off(p.n, p.rec)
    } else {
        // the runs form (D485): the fold's table at the region's end, half of it up to `OWNER_TABLE_CAP`; the runs fill the rest
        // from the front. The fold lays one partition's table at a time (its re-reads and splits at the same place), so a region
        // of the records' runs needs one partition's table, not a half of the records' bytes. A partition past the table
        // splits on its next route bits (parallel merge.rs fold_sub); past the route's bits the driver folds again in twice the
        // region (section.rs own_all / own_split). A region under twice the cap lays out as before (the half).
        let half = bytes.saturating_sub(RUN_OFF) / 2;
        bytes - (half.min(OWNER_TABLE_CAP.load(Ordering::Relaxed)) & !7)
    };
    if table_off + HASHAGG_HDR + 2 * p.rec > bytes {
        return unsupported(UNSUPPORTED_SCRATCH);
    }
    *(owner as *mut OwnerTop) = OwnerTop { len: 0, n: p.n as u32, table_off: table_off as u32, worst: 0, round2_off: 0, resume: 0, total: 0, groups: 0, records: 0, thr: 0, key_lo: gravity_abi::breaker::KEY_LO_NONE, key_hi: gravity_abi::breaker::KEY_HI_NONE, cell: core::cell::UnsafeCell::new([]) };
    table_off as i64
}

/// The count form's key check over a folded partition (`OwnerDesc::check`): every group, the ones the cut drops too, as
/// PostgreSQL projects each before its Sort. A checked key outside its `[lo, hi]` is the projection's overflow: its raise code.
/// NEW-46: a key `c.nulls` marks is skipped where its NULL flag is set (`flags`, the record's first NULL flags word,
/// `OwnerDesc::tie_flags`; its bit the key word, the keys before it one word each): a strict step over NULL raises nothing.
/// Perf (D4456): the scans below read the table's memory with every offset hoisted and no call inside, so their code does
/// not depend on what else shares the codegen unit (an unrelated edit to this crate made this loop 2 % of a statement slower).
#[inline(never)]
unsafe fn key_check(t: &HashAgg, layout: &RecordLayout, c: &KeyRanges, flags: Option<usize>) -> Result<(), u8> {
    let keys = &c.keys[..c.n];
    let mut ks = [(0usize, 0i64, 0i64, false, None::<(usize, u32)>); KEY_CHECK_KEYS];
    for (j, (e, &(k, lo, hi))) in ks.iter_mut().zip(keys).enumerate() {
        let null = flags.filter(|_| c.nulls >> j & 1 != 0).map(|f| {
            let (fw, bit) = gravity_abi::breaker::flag_at(k);
            (t.layout.key_off(f + fw), bit)
        });
        *e = (t.layout.key_off(k), lo, hi, layout.key_kind(k) == KeyKind::I32, null);
    }
    let scan = Scan { base: t.base_ptr(), cap: t.cap as usize, size: t.layout.size() as usize, used: t.layout.sentinel_off() as usize };
    let ok = match &ks[..keys.len()] {
        &[(off, lo, hi, true, None)] => scan.one_key::<true>(off, lo, hi),
        &[(off, lo, hi, false, None)] => scan.one_key::<false>(off, lo, hi),
        rest => scan.keys(rest),
    };
    if ok { Ok(()) } else { Err(c.code) }
}

struct Scan {
    base: *const u8,
    cap: usize,
    size: usize,
    used: usize,
}

impl Scan {
    #[inline(always)]
    unsafe fn word(&self, i: usize, off: usize) -> u64 {
        *(self.base.add(i * self.size + off) as *const u64)
    }

    #[inline(always)]
    unsafe fn one_key<const NARROW: bool>(&self, off: usize, lo: i64, hi: i64) -> bool {
        for i in 0..self.cap {
            if self.word(i, self.used) == 0 {
                continue;
            }
            let w = self.word(i, off);
            let v = if NARROW { w as i32 as i64 } else { w as i64 };
            if v < lo || v > hi {
                return false;
            }
        }
        true
    }

    #[inline(always)]
    unsafe fn keys(&self, ks: &[(usize, i64, i64, bool, Option<(usize, u32)>)]) -> bool {
        for i in 0..self.cap {
            if self.word(i, self.used) == 0 {
                continue;
            }
            for &(off, lo, hi, narrow, null) in ks {
                if null.is_some_and(|(fo, bit)| self.word(i, fo) >> bit & 1 != 0) {
                    continue;
                }
                let w = self.word(i, off);
                let v = if narrow { w as i32 as i64 } else { w as i64 };
                if v < lo || v > hi {
                    return false;
                }
            }
        }
        true
    }
}

/// SRC-38 slice 2: whether `layout`'s records carry one integer key word whose range the generic owner observes
/// (`OBSERVE_KEY_BIT`): the record's one key word, or (`flagged`) a `(value, flags)` image's value; and whether its word
/// is read sign-extended from 32 bits (an int4 or date kind; lowerv4 images an int4 as the unified `I64` word): `Some(narrow)`.
fn observable_key(layout: &RecordLayout, flagged: bool) -> Option<bool> {
    let words = if flagged { 2 } else { 1 };
    (layout.nkeys() == words && (!flagged || layout.key_kind(1) == KeyKind::I64)).then(|| layout.key_kind(0)).and_then(|k| match k {
        KeyKind::I32 | KeyKind::Date => Some(true),
        KeyKind::I64 | KeyKind::Timestamp => Some(false),
        _ => None,
    })
}

/// SRC-38 slice 2: key 0's running least and greatest datum over every group a partition folded, before any form prunes
/// it, into `h.key_lo` / `h.key_hi` (exec statement.rs `observe` remembers them, `Owned::key_range`, for the next execution's
/// DenseLo / DenseHi); the NULL group of a `flagged` image (its value word zeroed, emit_image) takes the slot after the
/// domain, so it is not observed. Only `owner_reduce_observe` carries it: an owner that observes nothing runs a reduce with
/// no trace of it (an in-loop fold slowed the W0 scan of statements with no owner at all by 6-10 %, a separate pass over a
/// 1M-group table cost +5-12 %).
struct KeySeen {
    lo: i64,
    hi: i64,
    key: usize,
    flags: Option<usize>,
    narrow: bool,
}

impl KeySeen {
    fn new(word: u64, h: &OwnerTop, layout: &RecordLayout) -> Option<KeySeen> {
        let flagged = word >> OBSERVE_FLAGGED_BIT & 1 != 0;
        let narrow = observable_key(layout, flagged)?;
        Some(KeySeen { lo: h.key_lo, hi: h.key_hi, key: layout.key_off(0) as usize, flags: flagged.then(|| layout.key_off(1) as usize), narrow })
    }

    /// # Safety
    /// `rec` is a used record of the reduce's table, at least `layout.size()` bytes.
    #[inline(always)]
    unsafe fn see(&mut self, rec: *const u8) {
        if let Some(f) = self.flags {
            if core::ptr::read_unaligned(rec.add(f) as *const u64) & 1 != 0 {
                return;
            }
        }
        let w = core::ptr::read_unaligned(rec.add(self.key) as *const u64);
        let v = if self.narrow { w as i32 as i64 } else { w as i64 };
        (self.lo, self.hi) = (self.lo.min(v), self.hi.max(v));
    }

    fn store(&self, h: &mut OwnerTop) {
        (h.key_lo, h.key_hi) = (self.lo, self.hi);
    }
}

unsafe fn owner_reduce(params: *const u64, owner: *mut u8, table: *const HashAgg, site: *mut Site, cand: *const u32, ncand: i64) -> i64 {
    owner_reduce_at::<false>(params, owner, table, site, cand, ncand)
}

#[cold]
#[inline(never)]
unsafe fn owner_reduce_observe(params: *const u64, owner: *mut u8, table: *const HashAgg, site: *mut Site, cand: *const u32, ncand: i64) -> i64 {
    owner_reduce_at::<true>(params, owner, table, site, cand, ncand)
}

#[inline(always)]
unsafe fn owner_reduce_at<const OBS: bool>(params: *const u64, owner: *mut u8, table: *const HashAgg, site: *mut Site, cand: *const u32, ncand: i64) -> i64 {
    let p = params_of(params);
    let h = &mut *(owner as *mut OwnerTop);
    let t = &mut *(table as *mut HashAgg);
    h.groups += t.len as u64;
    h.records += t.len as u64;
    let mut obs = if OBS { KeySeen::new(*params.add(P_ORDER), h, &p.layout) } else { None };
    if let Some(slot) = p.count {
        if let Some(o) = obs.as_mut() {
            for r in t.records() {
                o.see(r.as_ptr());
            }
            o.store(h);
        }
        if let Some(c) = &p.check {
            if let Err(code) = key_check(t, &p.layout, c, p.tie_flags) {
                return raised(raise::bit(code));
            }
        }
        let mut top = CountTop { base: owner.add(OWNERTOP_HDR), rec: p.rec, n: p.n, len: h.len as usize, worst: h.worst as usize, off_count: p.layout.slot_off(slot) as usize, layout: p.layout, reps: p.reps, flags: p.tie_flags, text: p.tie_text, site, order: p.order };
        top.offer_folded(t, cand, ncand);
        h.len = top.len as u32;
        h.worst = top.worst as u32;
        h.thr = top.threshold();
        return 0;
    }
    if p.n > 0 {
        let mut top = Top { base: owner.add(OWNERTOP_HDR), heap: owner.add(OWNERTOP_HDR + p.n * p.rec) as *mut u32, n: p.n, len: h.len as usize, rec: p.rec };
        let mut seen = 0usize;
        for i in 0..t.cap as usize {
            if seen == t.len as usize {
                break;
            }
            let r = t.record(i);
            if r.used() {
                if let Some(o) = obs.as_mut() {
                    o.see(r.as_ptr());
                }
                top.offer(r.as_ptr());
                seen += 1;
            }
        }
        if let Some(o) = &obs {
            o.store(h);
        }
        h.len = top.len as u32;
        return 0;
    }
    if p.ordered {
        t.sort_dense_by_key(0);
    }
    let n = t.len as usize;
    if n == 0 {
        return 0;
    }
    let runs = h.worst as usize;
    let off = RUN_OFF + h.total as usize;
    if runs >= MAX_RUNS || off + n * p.rec > h.table_off as usize {
        return unsupported(UNSUPPORTED_SCRATCH);
    }
    let mut m = 0usize;
    match p.floor {
        // D3153: only the records within the bound are laid (every group still counted and observed)
        Some((at, lo, hi)) => {
            // D-06: a top keeps by its running bound (`Best`), every record of a partition no larger than its n
            let (lo, hi) = if p.top.is_some() { (i64::MIN, i64::MAX) } else { (lo, hi) };
            let mut best = p.top.filter(|&(k, _)| n > k).map(|(k, below)| Best::new(k, below));
            let mut seen = 0usize;
            for i in 0..t.cap as usize {
                if seen == n {
                    break;
                }
                let r = t.record(i);
                if r.used() {
                    seen += 1;
                    if let Some(o) = obs.as_mut() {
                        o.see(r.as_ptr());
                    }
                    let v = core::ptr::read_unaligned(r.as_ptr().add(at) as *const i64);
                    if best.as_mut().map_or(v >= lo && v <= hi, |b| b.keeps(v)) {
                        core::ptr::copy_nonoverlapping(r.as_ptr(), owner.add(off + m * p.rec), p.rec);
                        m += 1;
                    }
                }
            }
        }
        None => {
            for i in 0..t.cap as usize {
                if m == n {
                    break;
                }
                let r = t.record(i);
                if r.used() {
                    if let Some(o) = obs.as_mut() {
                        o.see(r.as_ptr());
                    }
                    core::ptr::copy_nonoverlapping(r.as_ptr(), owner.add(off + m * p.rec), p.rec);
                    m += 1;
                }
            }
        }
    }
    if let Some(o) = &obs {
        o.store(h);
    }
    if m == 0 {
        return 0;
    }
    let dir = owner.add(DIR_OFF) as *mut u32;
    *dir.add(2 * runs) = off as u32;
    *dir.add(2 * runs + 1) = m as u32;
    h.worst = runs as u32 + 1;
    h.total += (m * p.rec) as u64;
    h.len += m as u32;
    0
}

/// D485: the runs form's table space at most (half the region below it); a test lowers it so a partition's table fills and splits.
pub static OWNER_TABLE_CAP: AtomicUsize = AtomicUsize::new(64 << 20);

unsafe fn owner_end(params: *const u64, owner: *mut u8) -> i64 {
    let p = params_of(params);
    if p.n == 0 || p.count.is_some() {
        return 0;
    }
    let h = &*(owner as *const OwnerTop);
    Top { base: owner.add(OWNERTOP_HDR), heap: owner.add(OWNERTOP_HDR + p.n * p.rec) as *mut u32, n: p.n, len: h.len as usize, rec: p.rec }.sort();
    0
}

#[inline(always)]
unsafe fn key_at(p: *const u8) -> i64 {
    *(p as *const i64)
}

/// The `n` smallest key-0 records so far: kept copies over `base`, an index heap whose root is the largest kept key.
struct Top {
    base: *mut u8,
    heap: *mut u32,
    n: usize,
    len: usize,
    rec: usize,
}

impl Top {
    unsafe fn at(&self, i: usize) -> *mut u8 {
        self.base.add(i * self.rec)
    }

    unsafe fn key(&self, hi: usize) -> i64 {
        key_at(self.at(*self.heap.add(hi) as usize))
    }

    unsafe fn offer(&mut self, r: *const u8) {
        let slot = if self.len < self.n {
            self.len
        } else {
            if key_at(r) >= self.key(0) {
                return;
            }
            *self.heap as usize
        };
        core::ptr::copy_nonoverlapping(r, self.at(slot), self.rec);
        if slot == self.len {
            *self.heap.add(self.len) = slot as u32;
            self.len += 1;
            self.sift_up(self.len - 1);
        } else {
            self.sift_down(0, self.len);
        }
    }

    unsafe fn swap(&mut self, i: usize, j: usize) {
        core::ptr::swap(self.heap.add(i), self.heap.add(j));
    }

    unsafe fn sift_up(&mut self, mut i: usize) {
        while i > 0 {
            let parent = (i - 1) / 2;
            if self.key(i) <= self.key(parent) {
                break;
            }
            self.swap(i, parent);
            i = parent;
        }
    }

    unsafe fn sift_down(&mut self, mut i: usize, end: usize) {
        loop {
            let (l, r) = (2 * i + 1, 2 * i + 2);
            let mut m = i;
            if l < end && self.key(l) > self.key(m) {
                m = l;
            }
            if r < end && self.key(r) > self.key(m) {
                m = r;
            }
            if m == i {
                return;
            }
            self.swap(i, m);
            i = m;
        }
    }

    unsafe fn sort(&mut self) {
        for end in (1..self.len).rev() {
            self.swap(0, end);
            self.sift_down(0, end);
        }
    }
}

/// The count-ordered form's `n` best records so far (t3's `Top` over the layout): kept copies over
/// `base`, `worst` the index a candidate must beat; the order is `reps::count_better`'s — the count
/// descending, then the keys ascending (an int word as signed, a text key by its rendered bytes, or by its words where the descriptor's `tie_fp` names it: `tie_reps`).
struct CountTop {
    base: *mut u8,
    rec: usize,
    n: usize,
    len: usize,
    worst: usize,
    off_count: usize,
    layout: RecordLayout,
    reps: Reps,
    /// The record's first NULL flags word a nullable tie key reads (`Params::tie_flags`).
    flags: Option<usize>,
    /// D3080: the int-text tie keys (`Params::tie_text`).
    text: u32,
    site: *mut Site,
    /// The order word (`OwnerDesc::order`): 0 the count descending, gated by `thr`; else the emit's slot sort word.
    order: u64,
}

impl CountTop {
    unsafe fn at(&self, i: usize) -> *const u8 {
        self.base.add(i * self.rec)
    }

    unsafe fn count_of(&self, r: *const u8) -> i64 {
        (*(r.add(self.off_count) as *const u64) & !MOVING) as i64
    }

    /// Inlined always, the order word's compare out of line: the count descending's compare is the one `count_better`
    /// call in every heap step.
    #[inline(always)]
    unsafe fn better(&self, a: *const u8, b: *const u8) -> bool {
        if self.order == 0 {
            return count_better(&self.layout, self.off_count, &self.reps, self.flags, self.text, self.site, a, b);
        }
        self.word_better(a, b)
    }

    /// The emit's own order of the final (`slot_order`, agg_sort's), then the keys ascending on a tie: it refines the top's
    /// order as `count_better` refines the count's.
    #[inline(never)]
    unsafe fn word_better(&self, a: *const u8, b: *const u8) -> bool {
        let (ra, rb) = (Rec::at(a as *mut u8, &self.layout), Rec::at(b as *mut u8, &self.layout));
        match slot_order(&ra, &rb, &self.layout, self.order) {
            core::cmp::Ordering::Less => true,
            core::cmp::Ordering::Greater => false,
            core::cmp::Ordering::Equal => keys_better(&self.layout, &self.reps, self.flags, self.text, self.site, a, b),
        }
    }

    /// The kept records as a heap with the worst at the root (`worst` stays 0): `log n` compares per kept
    /// candidate, where a linear scan rendered every kept text key per candidate (`n` past a thousand).
    unsafe fn sift_down(&mut self, mut i: usize) {
        loop {
            let (l, r) = (2 * i + 1, 2 * i + 2);
            let mut m = i;
            if l < self.len && self.better(self.at(m), self.at(l)) {
                m = l;
            }
            if r < self.len && self.better(self.at(m), self.at(r)) {
                m = r;
            }
            if m == i {
                return;
            }
            core::ptr::swap_nonoverlapping(self.base.add(i * self.rec), self.base.add(m * self.rec), self.rec);
            i = m;
        }
    }

    unsafe fn heapify(&mut self) {
        for i in (0..self.len / 2).rev() {
            self.sift_down(i);
        }
        self.worst = 0;
    }

    unsafe fn threshold(&self) -> i64 {
        if self.len < self.n || self.order != 0 {
            return 0;
        }
        self.count_of(self.at(self.worst))
    }

    unsafe fn offer(&mut self, r: *const u8) {
        if self.len < self.n {
            core::ptr::copy_nonoverlapping(r, self.base.add(self.len * self.rec), self.rec);
            self.len += 1;
            if self.len == self.n {
                self.heapify();
            }
            return;
        }
        if self.better(r, self.at(self.worst)) {
            core::ptr::copy_nonoverlapping(r, self.base.add(self.worst * self.rec), self.rec);
            self.sift_down(self.worst);
        }
    }

    /// A folded partition's records: the driver's candidates (their `MOVING` mark cleared), or
    /// every used record of the table when there is no list (`ncand < 0`).
    unsafe fn offer_folded(&mut self, t: &HashAgg, cand: *const u32, ncand: i64) {
        let base = t.base_ptr();
        let mut thr = self.threshold();
        debug_assert!(self.order == 0 || ncand < 0, "an order word's top lists no candidates");
        if ncand >= 0 {
            for k in 0..ncand as usize {
                let p = base.add(*cand.add(k) as usize * self.rec);
                let w = p.add(self.off_count) as *mut u64;
                *w &= !MOVING;
                if ((*w) as i64) < thr {
                    continue;
                }
                self.offer(p);
                thr = self.threshold();
            }
            return;
        }
        let off_used = t.layout.sentinel_off() as usize;
        for i in 0..t.cap as usize {
            let p = base.add(i * self.rec);
            if *(p.add(off_used) as *const u64) == 0 {
                continue;
            }
            if self.order != 0 {
                self.offer(p);
                continue;
            }
            let w = p.add(self.off_count) as *mut u64;
            *w &= !MOVING;
            if ((*w) as i64) < thr {
                continue;
            }
            self.offer(p);
            thr = self.threshold();
        }
    }
}

#[cfg(test)]
mod tie_flags_tests {
    use gravity_abi::breaker::{KeyKind, RecordLayout, SlotKind};
    use gravity_program::{OwnerDesc, ACC_PAIRS, MAX_REPS, REP_NONE};

    use super::{owner_fns, params_of};

    /// A nullable int key and its flags word.
    const L: RecordLayout = RecordLayout::new(&[KeyKind::I32, KeyKind::I64], &[SlotKind::Count]);

    #[test]
    fn the_count_form_carries_its_tie_flags_word_and_no_null_argument() {
        let d = OwnerDesc::count_topk(L, 0, 10);
        let (plain, flagged) = (owner_fns(&d, 0).params, owner_fns(&d.tie_flags(1), 0).params);
        // SAFETY: owner_fns packed both images
        let (p, q) = unsafe { (params_of(plain.as_ptr()), params_of(flagged.as_ptr())) };
        assert_eq!((p.tie_flags, p.null_arg, p.group_flags), (None, None, false));
        assert_eq!((q.tie_flags, q.null_arg, q.group_flags, q.pair, q.groups, q.n), (Some(1), None, false, 0, 0, 10));
    }

    #[test]
    fn the_distinct_count_form_carries_its_null_argument_and_no_tie_flags() {
        let pair = RecordLayout::new_nn(&[KeyKind::I64, KeyKind::I64, KeyKind::I64], &[SlotKind::Count]);
        let acc = RecordLayout::new(&[KeyKind::I64, KeyKind::I64], &[SlotKind::Count, SlotKind::Count, SlotKind::Count, SlotKind::SumI64]);
        let d = OwnerDesc::distinct(pair, acc, 1, 16, ACC_PAIRS, 10, [(REP_NONE, 0); MAX_REPS]).null_arg(2, 1).arg_fold(3).group_flags();
        let w = owner_fns(&d, 3).params;
        // SAFETY: owner_fns packed the image
        let p = unsafe { params_of(w.as_ptr()) };
        assert_eq!((p.null_arg, p.group_flags, p.tie_flags, p.pair), (Some((2, 1)), true, None, 1));
    }
}
