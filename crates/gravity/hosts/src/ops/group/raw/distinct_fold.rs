//! The distinct fold face (lower.md §15.9): the `count(DISTINCT x)` family's owner phase in ONE round over a
//! declared pair layout — the two-round owner (exec `owner.rs::distinct_fns`) re-sinks every pair by its group
//! hash and walks the owners' runs again. The **plain** form (`count(DISTINCT x)` alone: every key word is the
//! argument's) counts a partition's deduped pairs into `OwnerTop.total`, the leader sums the owners (`plain_total`);
//! the **grouped** form (the pair layout partitioned by its group words, `pkeys`: a declared property, the
//! lowerer's from the plan's group estimate, never a runtime count) counts a partition's pairs per group into an
//! accumulator table inside the region and offers the groups to the count-ordered top (t3's `[OwnerTop][n
//! records][…]`) or writes them as a run (the generic owner's directory). No flush, no resume, no second round.
//! The accumulator is either declared form — `FORM_PAIRS` (t3's `layout_of(ng, 1)`: the kept records carry their
//! text representatives RENDERED, the kernel's emit contract) or `FORM_SENTINEL` (`OwnerDesc::accumulator_of`:
//! `part << 32 | code` words, exec's emit renders) — the pair's slots behind it in `Rec::fold`'s vocabulary.
//! It sits in the family's raw half because the face is its owner entries' raw-pointer work; group/mod.rs re-exports it
//! as `ops::group::distinct_fold`, the path its readers name.
#![allow(unsafe_code)]

use gravity_abi::breaker::{prefetch_read, FloatOverflow, HashAgg, KeyKind, OwnerFns, OwnerTop, Rec, RecordLayout, SlotKind, FOLD_PF, HASHAGG_HDR, LAYOUT_IMPLICIT_COUNT, MINCODE_NONE, MOVING, ORDER_KEY, ORDER_NONE, OWNERTOP_HDR, OWNER_PARAMS, T1_MAX_PARTS};
use gravity_abi::hash::{Depth, KeyHash};
use gravity_abi::status::{unsupported, CANCELLED, UNSUPPORTED_SCRATCH};
use gravity_abi::Site;
use gravity_kernels::dict::reps::{count_better, keys_better, pack_slot, render, unpack_slot, varlena_bytes, Reps, MAX_REPS, REP_NONE, REP_WORDS};
use gravity_kernels::host_abi::slot_order;
use gravity_program::cost::owner::RUNS_FLOOR_MAX;
use gravity_program::{OwnerDesc, ACC_PAIRS, ACC_SLOT_BASE, NULL_ARG_NONE};

/// The accumulator `[pairs] ++ the pair's slots` (t3's `Plan::layout_of(ng, 1)`: the pair count at slot 0).
pub const FORM_PAIRS: u8 = 0;
/// The accumulator `[sentinel, pairs] ++ the pair's slots` (lower.md §6.12: `OwnerDesc::accumulator_of`).
pub const FORM_SENTINEL: u8 = 1;

const P_TOPK: usize = 0;
const P_FORM: usize = 1;
const P_SLOT: usize = 2;
const P_BITS: usize = 3;
const P_LAYOUT: usize = 4;
const LAYOUT_WORDS: usize = core::mem::size_of::<RecordLayout>() / 8;
/// The descriptor's accumulator (`OwnerDesc.acc`, widened by the argument-fold slots) after the pair layout; all-zero words: `accumulator()`'s.
const P_ACC: usize = P_LAYOUT + LAYOUT_WORDS;
/// The representatives' slot words past `P_SLOT` (kernels `pack_slot`'s words 1..`REP_WORDS`), after the accumulator.
const P_SLOT_MORE: usize = P_ACC + LAYOUT_WORDS;
const _: () = assert!(P_SLOT_MORE + REP_WORDS - 1 <= OWNER_PARAMS);
/// `P_FORM`'s upper bytes (M5 §4.5): the NULL-argument flag `(word + 1, bit)` at 48 (0: none), the three argument-fold slots (`slot + 1`, 0: none) at 16, 24 and 32.
const NULL_ARG_SHIFT: u32 = 48;
const ARG_FOLD_SHIFT: u32 = 16;
/// The count top's order word (`OwnerDesc::order`: an emit slot sort word over the accumulator; 0: its count slot
/// descending) in `P_TOPK`'s bits 32..48, and its order slot + 1 in `P_SLOT`'s low byte (0: the pair count).
const ORDER_WORD_SHIFT: u32 = 32;
const MAX_RUNS: usize = T1_MAX_PARTS as usize;
const DIR_OFF: usize = OWNERTOP_HDR;
const RUN_OFF: usize = DIR_OFF + 8 * MAX_RUNS;

/// The face's declaration: the pair layout (`pkeys` = its group words for the grouped form), the argument's trailing
/// key words, the accumulator form, the count bound (0: every group as a run), the `(accumulator slot, code lane)` reps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DistinctFold {
    pub pair: RecordLayout,
    pub words: u8,
    pub form: u8,
    pub topk: u32,
    pub reps: Reps,
}

impl DistinctFold {
    /// Lower.md §6.12's descriptor (`OwnerDesc::distinct`) in the sentinel form: the count-ordered form's slot is an
    /// accumulator slot (`ACC_PAIRS`, a rider's count, or the slot of an order word: `distinct_fold_fns_desc` carries
    /// it), a key order (`ORDER_KEY`) is not a form of this face.
    pub fn of_desc(d: &OwnerDesc) -> DistinctFold {
        assert!(d.ctor.is_none() && d.pair != 0, "the distinct form");
        assert!(d.topk == 0 || d.order_slot < ORDER_KEY, "the count-ordered form orders by an accumulator slot");
        assert!(d.topk != 0 || d.order_slot == ORDER_NONE, "a key-ordered distinct set is not a one-round form");
        DistinctFold { pair: d.layout, words: d.pair, form: FORM_SENTINEL, topk: d.topk, reps: d.reps }
    }

    pub fn plain(&self) -> bool {
        self.pair.nkeys() == self.words as usize
    }

    pub fn group_words(&self) -> usize {
        self.pair.nkeys() - self.words as usize
    }

    pub fn accumulator(&self) -> RecordLayout {
        accumulator(&self.pair, self.words, self.form)
    }

        pub const fn pairs_slot(&self) -> usize {
        if self.form == FORM_SENTINEL { ACC_PAIRS as usize } else { 0 }
    }

        pub const fn slot_base(&self) -> usize {
        if self.form == FORM_SENTINEL { ACC_SLOT_BASE as usize } else { 1 }
    }
}

/// The accumulator: the group keys, `form`'s leading `Count` slots, the pair's slots less its implicit count.
pub fn accumulator(pair: &RecordLayout, words: u8, form: u8) -> RecordLayout {
    let nk = pair.nkeys() - words as usize;
    let implicit = pair.flags & LAYOUT_IMPLICIT_COUNT != 0;
    let ns = pair.nslots() - implicit as usize;
    let base = if form == FORM_SENTINEL { ACC_SLOT_BASE as usize } else { 1 };
    let mut slots = [SlotKind::Count; gravity_abi::breaker::MAX_RECORD_SLOTS];
    slots[base..base + ns].copy_from_slice(&pair.slot_kinds()[..ns]);
    let slots = &slots[..base + ns];
    if pair.has_validity() { RecordLayout::new(&pair.key_kinds()[..nk], slots) } else { RecordLayout::new_nn(&pair.key_kinds()[..nk], slots) }
}

/// `distinct_fold_fns` over lower.md §6.12's descriptor with its NULL-argument flag, argument-fold slots and widened accumulator carried (M5 §4.5).
pub fn distinct_fold_fns_desc(d: &OwnerDesc, spill_bits: u8) -> OwnerFns {
    let f = DistinctFold::of_desc(d);
    let mut fns = distinct_fold_fns(&f, spill_bits);
    let acc: [u64; LAYOUT_WORDS] = d.acc.words();
    fns.params[P_ACC..P_ACC + LAYOUT_WORDS].copy_from_slice(&acc);
    if d.null_arg.0 != NULL_ARG_NONE {
        fns.params[P_FORM] |= (d.null_arg.0 as u64 + 1) << NULL_ARG_SHIFT | (d.null_arg.1 as u64) << (NULL_ARG_SHIFT + 8);
        // a flagged pair may sit in an identity-shaped table: the plain form counts it out in `fold_reduce`
        fns.fold = Some(gravity_kernels::table::owner_fold::owner_fold);
    }
    // bounded("small-fixed: the descriptor's argument-fold slots, 3")
    for (i, s) in d.arg_fold.iter().enumerate().filter(|(_, s)| **s != NULL_ARG_NONE) {
        fns.params[P_FORM] |= (*s as u64 + 1) << (ARG_FOLD_SHIFT + 8 * i as u32);
    }
    if d.topk > 0 {
        fns.params[P_SLOT] |= d.order_slot as u64 + 1;
        fns.params[P_TOPK] |= (d.order as u64) << ORDER_WORD_SHIFT;
    }
    fns
}

/// The face's owner functions over `f` (`spill_bits`: the pipeline's, carried for the memo).
pub fn distinct_fold_fns(f: &DistinctFold, spill_bits: u8) -> OwnerFns {
    assert!(f.words != 0 && (f.words as usize) <= f.pair.nkeys(), "the argument's words");
    assert!(f.form <= FORM_SENTINEL, "the accumulator form");
    let plain = f.plain();
    assert!(plain || f.pair.partition_keys() == f.group_words(), "the grouped form is declared: the pair layout partitioned by its group words");
    assert!(!plain || f.topk == 0, "the plain form has no order");
    let mut p = [0u64; OWNER_PARAMS];
    p[P_TOPK] = f.topk as u64;
    p[P_FORM] = f.form as u64 | (f.words as u64) << 8;
    let rw = pack_slot(0, &f.reps);
    p[P_SLOT] = rw[0];
    p[P_SLOT_MORE..P_SLOT_MORE + REP_WORDS - 1].copy_from_slice(&rw[1..]);
    p[P_BITS] = spill_bits as u64;
    let words: [u64; LAYOUT_WORDS] = f.pair.words();
    p[P_LAYOUT..P_LAYOUT + LAYOUT_WORDS].copy_from_slice(&words);
    // the plain form reads `len` only: its pair table holds identities (kernel-floors.md §2.1) when the identity fold lays it
    let fold = if plain && gravity_kernels::table::owner_fold::ident_folds(&f.pair) { gravity_kernels::table::owner_fold::owner_fold_ident } else { gravity_kernels::table::owner_fold::owner_fold };
    OwnerFns { params: p, rounds: 1, order_slot: ORDER_NONE, order_slot2: ORDER_NONE, min_bytes: fold_min, open: fold_open, reduce: fold_reduce, end_round: fold_end, reduce2: fold_reduce, fold: Some(fold) }
}

/// The plain form's answer: the owners' totals summed (the leader's, over the finished owned region).
/// # Safety
/// `owned` is a leader region the owner phase finished (`TABLE_OWNED`) under this face's plain form.
pub unsafe fn plain_total(owned: *const u8) -> u64 {
    let o = gravity_abi::breaker::Owned::view(owned);
    (0..o.nowners as usize).map(|i| OwnerTop::view(o.owner(i)).total).sum()
}

struct Params {
    f: DistinctFold,
    acc: RecordLayout,
    n: usize,
    /// The NULL-argument flag `(key word, bit)` and the argument-fold slots (`distinct_fold_fns_desc`).
    null_arg: Option<(usize, u8)>,
    arg_fold: [Option<usize>; 3],
    /// The count top's accumulator slot and order word (`distinct_fold_fns_desc`; the pair count and 0 by default).
    order_slot: usize,
    order: u64,
}

unsafe fn params_of(p: *const u64) -> Params {
    let w = core::slice::from_raw_parts(p, OWNER_PARAMS);
    let mut words = [0u64; LAYOUT_WORDS];
    words.copy_from_slice(&w[P_LAYOUT..P_LAYOUT + LAYOUT_WORDS]);
    // SAFETY: the words `distinct_fold_fns` packed.
    let pair: RecordLayout = RecordLayout::from_words(words);
    let f = DistinctFold { pair, words: (w[P_FORM] >> 8) as u8, form: w[P_FORM] as u8, topk: w[P_TOPK] as u32, reps: unpack_slot(&core::array::from_fn(|i| if i == 0 { w[P_SLOT] } else { w[P_SLOT_MORE + i - 1] })) };
    words.copy_from_slice(&w[P_ACC..P_ACC + LAYOUT_WORDS]);
    let declared: RecordLayout = RecordLayout::from_words(words);
    let acc = if f.plain() { pair } else if declared.nslots() != 0 { declared } else { f.accumulator() };
    let na = (w[P_FORM] >> NULL_ARG_SHIFT) as u8;
    let null_arg = (na != 0).then(|| (na as usize - 1, (w[P_FORM] >> (NULL_ARG_SHIFT + 8)) as u8));
    let fold = |i: u32| { let s = (w[P_FORM] >> (ARG_FOLD_SHIFT + 8 * i)) as u8; (s != 0).then(|| s as usize - 1) };
    let order_slot = match w[P_SLOT] as u8 { 0 => f.pairs_slot(), s => s as usize - 1 };
    Params { f, acc, n: w[P_TOPK] as u32 as usize, null_arg, arg_fold: [fold(0), fold(1), fold(2)], order_slot, order: w[P_TOPK] >> ORDER_WORD_SHIFT & 0xffff }
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

/// The results' end: the count form's `n` accumulators, the run form's directory and runs area (a quarter of the
/// rest), the plain form's nothing.
fn results_end(p: &Params, bytes: usize) -> usize {
    if p.f.plain() {
        OWNERTOP_HDR
    } else if p.n > 0 {
        (OWNERTOP_HDR + p.n * p.acc.size() as usize + 7) & !7
    } else {
        RUN_OFF + ((bytes.saturating_sub(RUN_OFF) / 4) & !7)
    }
}

/// The grouped region behind the results: `(acc_off, table_off)` — the accumulator space and the pair table space
/// share the rest so that the accumulator table always holds the pair table's capacity (a group needs at most one
/// record per pair, and the driver's table never exceeds its space's capacity).
fn split(p: &Params, res_end: usize, bytes: usize) -> Option<(usize, usize)> {
    let rest = bytes.checked_sub(res_end)?;
    let (pr, ar) = (p.f.pair.size() as usize, p.acc.size() as usize);
    let mut pair_bytes = (rest * pr / (pr + ar)) & !7;
    let cap_p = HashAgg::cap_of(&p.f.pair, pair_bytes as u32);
    let need = HashAgg::bytes(&p.acc, cap_p) as usize;
    if need > rest - pair_bytes {
        pair_bytes = (rest - need) & !7;
    }
    let floor = HASHAGG_HDR + 64 * pr;
    let acc_floor = HASHAGG_HDR + 64 * ar;
    (pair_bytes >= floor && rest - pair_bytes >= acc_floor).then_some((res_end, res_end + (rest - pair_bytes)))
}

fn fold_min(params: *const u64, _owners: u32) -> u64 {
    // SAFETY: `OWNER_PARAMS` words.
    let p = unsafe { params_of(params) };
    let pr = p.f.pair.size() as u64;
    if p.f.plain() {
        return (OWNERTOP_HDR + HASHAGG_HDR) as u64 + 64 * pr;
    }
    let ar = p.acc.size() as u64;
    let res = if p.n > 0 { (OWNERTOP_HDR + p.n * ar as usize + 7) as u64 & !7 } else { RUN_OFF as u64 + 4 * (64 * ar).min(RUNS_FLOOR_MAX) };
    res + 2 * (HASHAGG_HDR as u64 + 64 * pr.max(ar)) + 64
}

unsafe fn fold_open(params: *const u64, owner: *mut u8, bytes: usize) -> i64 {
    let p = params_of(params);
    let res_end = results_end(&p, bytes);
    let (acc_off, table_off) = if p.f.plain() {
        if res_end + HASHAGG_HDR + 2 * p.f.pair.size() as usize > bytes {
            return unsupported(UNSUPPORTED_SCRATCH);
        }
        (0, res_end)
    } else {
        let Some(s) = split(&p, res_end, bytes) else { return unsupported(UNSUPPORTED_SCRATCH) };
        s
    };
    // `resume` (the two-round owner's flush slot; this face never flushes) holds the accumulator space's offset: the end
    // of the results the reduces keep (`OwnerTop::resume`, D3111: a grown region takes them and goes on, not from partition 0)
    *(owner as *mut OwnerTop) = OwnerTop { len: 0, n: p.n as u32, table_off: table_off as u32, worst: 0, round2_off: 0, resume: acc_off as u32, total: 0, groups: 0, records: 0, thr: 0, key_lo: gravity_abi::breaker::KEY_LO_NONE, key_hi: gravity_abi::breaker::KEY_HI_NONE, cell: core::cell::UnsafeCell::new([]) };
    table_off as i64
}

unsafe fn fold_slots(a: &Rec<'_>, pair: &RecordLayout, s: &Rec<'_>, ns: usize, base: usize) -> Result<(), FloatOverflow> {
    if s.null_seen() {
        a.set_null_seen();
    }
    // bounded("small-fixed: one slot of a record, at most MAX_RECORD_SLOTS (8)")
    for j in 0..ns {
        if !s.valid(j) {
            continue;
        }
        let d = base + j;
        match pair.slots[j] {
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

/// The next used pair slot at or after `*j` (`None` past the table); `*j` advances past it. C9: a slot is one load, so
/// `step` ticks in batches by the slot's index (every `BATCH` slots of the table, whatever the calls); `Err` on a stop.
#[inline(always)]
unsafe fn next_used(step: &mut cancelpoll::Step, tbase: *mut u8, rec: usize, sent: usize, cap: usize, j: &mut usize) -> Result<Option<*const u8>, cancelpoll::Canceled> {
    while *j < cap {
        step.batch(*j)?;
        let q = tbase.add(*j * rec);
        *j += 1;
        if *(q.add(sent) as *const u64) != 0 {
            return Ok(Some(q as *const u8));
        }
    }
    Ok(None)
}

/// One partition, one round: the plain form counts the deduped pairs; the grouped form folds them per group into
/// the accumulator table laid fresh over the accumulator space (the pair table's capacity, never fuller than its
/// pairs), then offers the groups to the count-ordered top or writes them as a run. C9: under the byte the owner phase
/// lends (driver section.rs `lend_cancel`), the pair table's slots, the accumulator's and the kept top's poll a step:
/// `CANCELLED` on a stop, which the merge raises as the statement's cancel (parallel merge.rs `reduce_error`).
unsafe fn fold_reduce(params: *const u64, owner: *mut u8, table: *const HashAgg, site: *mut Site, _cand: *const u32, _ncand: i64) -> i64 {
    let mut step = cancelpoll::step("distinct_fold fold_reduce: a partition's pairs folded");
    let p = params_of(params);
    let h = &mut *(owner as *mut OwnerTop);
    let t = &*table;
    h.records += t.len as u64;
    if p.f.plain() {
        h.total += t.len as u64;
        if p.null_arg.is_some() {
            // the NULL pair (its words 0, the flag set) is at most one record per partition: counted out
            let (rec, sent, tbase) = (t.layout.size() as usize, t.layout.sentinel_off as usize, t.base_ptr());
            let mut j = 0usize;
            loop {
                let Ok(next) = next_used(&mut step, tbase, rec, sent, t.cap as usize, &mut j) else { return CANCELLED };
                let Some(q) = next else { break };
                if flagged(q, p.null_arg) {
                    h.total -= 1;
                    break;
                }
            }
        }
        return 0;
    }
    if t.len == 0 {
        return 0;
    }
    let acc = p.acc;
    let acc_bytes = (h.table_off - h.resume) as usize;
    let cap = t.cap.min(HashAgg::cap_of(&acc, acc_bytes as u32));
    if cap < 2 || (cap - cap / 8) < t.len {
        return unsupported(UNSUPPORTED_SCRATCH);
    }
    // the accumulator opens at the partition's expected group count (the owner's groups-per-pair ratio so far, the
    // pair count for its first partition) and doubles in place (`HashAgg::grow`) up to the space's capacity: a table
    // of the pair capacity was zeroed and walked at random per partition (x86-opt.md §2b: a pair space far larger than the
    // groups it folds paid its zeroing and random walk per partition)
    let est = if h.records > t.len as u64 && h.groups > 0 { (t.len as u64 * h.groups / (h.records - t.len as u64)).max(1) } else { t.len as u64 };
    // the T1's route bits are a Depth's own (`set_route_bits`): never past ROUTE_BITS (`Depth::stored`)
    let at = Depth::stored(t.route_bits as u32);
    let cap0 = (est.saturating_mul(2).next_power_of_two().max(64) as u32).min(cap);
    let ap = owner.add(h.resume as usize);
    core::ptr::write_bytes(ap, 0, HASHAGG_HDR);
    let a = HashAgg::at(ap, &acc, cap0, acc_bytes as u32);
    // the pairs share the partition's (and split's) route bits: they are routed by the group words the accumulator keys on
    a.set_route_bits(at);
    let pair = t.layout;
    let nk = p.f.group_words();
    let ns = pair.nslots() - (pair.flags & LAYOUT_IMPLICIT_COUNT != 0) as usize;
    let (pairs_slot, base, sentinel) = (p.f.pairs_slot(), p.f.slot_base(), p.f.form == FORM_SENTINEL);
    let (rec, sent, tbase) = (pair.size() as usize, pair.sentinel_off as usize, t.base_ptr());
    // the used pair slots hashed `pf` ahead of the probe, their accumulator homes prefetched (`kernels::fold::run_with`'s
    // ring): the pair table is streamed, the accumulator is the random walk
    let pf = gravity_driver::machine::profile().prefetch.clamp(1, FOLD_PF);
    let arec = acc.size() as usize;
    let mut ring = [(core::ptr::null::<u8>(), KeyHash::word(0)); FOLD_PF];
    let (mut j, mut w, mut r) = (0usize, 0usize, 0usize);
    while w < pf {
        let Ok(next) = next_used(&mut step, tbase, rec, sent, t.cap as usize, &mut j) else { return CANCELLED };
        let Some(q) = next else { break };
        let hash = KeyHash::of(core::slice::from_raw_parts(q as *const u64, nk));
        prefetch_read(a.home_ptr(hash));
        ring[w] = (q, hash);
        w += 1;
    }
    while r < w {
        let (q, hash) = ring[r % FOLD_PF];
        r += 1;
        // a used slot's pair is folded once below: the slots' batches poll the fold too (at most `BATCH` pairs a tick)
        let Ok(next) = next_used(&mut step, tbase, rec, sent, t.cap as usize, &mut j) else { return CANCELLED };
        if let Some(q2) = next {
            let h2 = KeyHash::of(core::slice::from_raw_parts(q2 as *const u64, nk));
            prefetch_read(a.home_ptr(h2));
            ring[w % FOLD_PF] = (q2, h2);
            w += 1;
        }
        let keys = core::slice::from_raw_parts(q as *const u64, nk);
        let Some(i) = a.find_or_insert_index_hashed(keys, hash) else { return unsupported(UNSUPPORTED_SCRATCH) };
        let g = a.record(i);
        if sentinel && g.count(0) == 0 {
            g.add_count(0, 1);
        }
        if !flagged(q, p.null_arg) {
            g.add_count(pairs_slot, 1);
            let w = *(q as *const u64).add(nk);
            // bounded("small-fixed: the argument-fold slots, 3")
            for s in p.arg_fold.into_iter().flatten() {
                fold_arg(&g, &acc, s, w);
            }
        }
        if fold_slots(&g, &pair, &Rec::at(q as *mut u8, &pair), ns, base).is_err() {
            return FloatOverflow::STATUS;
        }
    }
    h.groups += a.len as u64;
    if p.n > 0 {
        let rendered = p.f.form == FORM_PAIRS && p.f.reps.iter().any(|r| r.0 != REP_NONE);
        let mut top = CountTop { base: owner.add(OWNERTOP_HDR), rec: arec, n: p.n, len: h.len as usize, worst: h.worst as usize, off_count: acc.slot_off[p.order_slot] as usize, layout: acc, reps: p.f.reps, site, rendered, order: p.order };
        if top.offer_all(&mut step, a).is_err() {
            return CANCELLED;
        }
        h.len = top.len as u32;
        h.worst = top.worst as u32;
        h.thr = top.threshold();
        return 0;
    }
    let runs = h.worst as usize;
    let off = RUN_OFF + h.total as usize;
    let m = a.len as usize;
    if runs >= MAX_RUNS || off + m * arec > h.resume as usize {
        return unsupported(UNSUPPORTED_SCRATCH);
    }
    let mut k = 0usize;
    for i in 0..a.cap as usize {
        // a slot is a test and at most one record's copy: a batch
        if step.batch(i).is_err() {
            return CANCELLED;
        }
        let r = a.record(i);
        if r.used() {
            core::ptr::copy_nonoverlapping(r.as_ptr(), owner.add(off + k * arec), arec);
            k += 1;
        }
    }
    let dir = owner.add(DIR_OFF) as *mut u32;
    *dir.add(2 * runs) = off as u32;
    *dir.add(2 * runs + 1) = k as u32;
    h.worst = runs as u32 + 1;
    h.total += (k * arec) as u64;
    h.len += k as u32;
    0
}

unsafe fn fold_end(_params: *const u64, _owner: *mut u8) -> i64 {
    0
}

/// The count-ordered form's `n` best accumulators so far (exec `owner.rs`'s over the accumulator): kept copies over
/// `base`, `worst` the index a candidate must beat; the order is `reps::count_better`'s, or under an order word the
/// emit's own order of that slot (`slot_order`) then the keys — under `rendered` the kept copies' representatives are
/// datums (t3's `offer_lazy`) and a candidate renders only at a tie that reaches its key.
struct CountTop {
    base: *mut u8,
    rec: usize,
    n: usize,
    len: usize,
    worst: usize,
    off_count: usize,
    layout: RecordLayout,
    reps: Reps,
    site: *mut Site,
    rendered: bool,
    /// The order word (0: the count at `off_count` descending).
    order: u64,
}

impl CountTop {
    unsafe fn at(&self, i: usize) -> *const u8 {
        self.base.add(i * self.rec)
    }

    unsafe fn count_of(&self, r: *const u8) -> i64 {
        (*(r.add(self.off_count) as *const u64) & !MOVING) as i64
    }

    /// `a` before `b`; `a_raw`: a candidate whose representatives are still words. Inlined always, the order word's
    /// compare out of line: the count descending's compare is the one `count_better` call in every heap step.
    #[inline(always)]
    unsafe fn better(&self, a: *const u8, a_raw: bool, b: *const u8) -> bool {
        if self.order != 0 {
            return self.word_better(a, a_raw, b);
        }
        if !self.rendered {
            return count_better(&self.layout, self.off_count, &self.reps, None, 0, self.site, a, b);
        }
        let (ca, cb) = (self.count_of(a), self.count_of(b));
        if ca != cb {
            return ca > cb;
        }
        self.keys_before(a, a_raw, b)
    }

    /// The emit's own order of the order word's slot (`slot_order`), then the keys.
    #[inline(never)]
    unsafe fn word_better(&self, a: *const u8, a_raw: bool, b: *const u8) -> bool {
        let (ra, rb) = (Rec::at(a as *mut u8, &self.layout), Rec::at(b as *mut u8, &self.layout));
        match slot_order(&ra, &rb, &self.layout, self.order) {
            core::cmp::Ordering::Less => true,
            core::cmp::Ordering::Greater => false,
            core::cmp::Ordering::Equal => self.keys_before(a, a_raw, b),
        }
    }

    /// `a` before `b` by the keys alone, ascending, as `better` orders two records of one count. Inlined always: the
    /// count descending reads it at every count tie.
    #[inline(always)]
    unsafe fn keys_before(&self, a: *const u8, a_raw: bool, b: *const u8) -> bool {
        if !self.rendered {
            return keys_better(&self.layout, &self.reps, None, 0, self.site, a, b);
        }
        let (mut k, mut t) = (0usize, 0usize);
        // bounded("small-fixed: one key word of a record, at most MAX_RECORD_KEYS (8)")
        while k < self.layout.nkeys() {
            let (wa, wb) = (*(a as *const u64).add(k), *(b as *const u64).add(k));
            if self.layout.key_kind(k) == KeyKind::Fp128 {
                let (ha, hb) = (*(a as *const u64).add(k + 1), *(b as *const u64).add(k + 1));
                if (wa, ha) != (wb, hb) {
                    let (slot, lane) = if t < MAX_REPS { self.reps[t] } else { (REP_NONE, 0) };
                    if slot == REP_NONE {
                        return (wa, ha) < (wb, hb);
                    }
                    let off = self.layout.slot_off(slot as usize) as usize;
                    let (da, db) = (*(a.add(off) as *const u64), *(b.add(off) as *const u64));
                    let pa = if a_raw { render(self.site, lane, da).ok().flatten() } else { (da != MINCODE_NONE).then_some(da as *const u8) };
                    let pb = (db != MINCODE_NONE).then_some(db as *const u8);
                    return match (pa, pb) {
                        (Some(pa), Some(pb)) => varlena_bytes(pa) < varlena_bytes(pb),
                        _ => (wa, ha) < (wb, hb),
                    };
                }
                k += 2;
                t += 1;
                continue;
            }
            if wa != wb {
                return (wa as i64) < (wb as i64);
            }
            k += 1;
        }
        false
    }

    /// C9: a scan of the kept copies ticks once (a top of a few copies is rescanned per record it takes) and in batches
    /// past `BATCH` copies (a compare is a few loads, or a text compare at a tie).
    unsafe fn find_worst(&mut self, step: &mut cancelpoll::Step) -> Result<(), cancelpoll::Canceled> {
        step.tick()?;
        let mut w = 0;
        for i in 1..self.len {
            step.batch(i)?;
            if self.better(self.at(w), false, self.at(i)) {
                w = i;
            }
        }
        self.worst = w;
        Ok(())
    }

    /// The kept copy at `i` takes its representatives' datums (`MINCODE_NONE` stays: a NULL key).
    unsafe fn render_kept(&self, i: usize) {
        if !self.rendered {
            return;
        }
        let r = self.base.add(i * self.rec);
        // bounded("small-fixed: one representative slot of a record, at most MAX_REPS (8)")
        for &(slot, lane) in self.reps.iter().filter(|r| r.0 != REP_NONE) {
            let w = r.add(self.layout.slot_off(slot as usize) as usize) as *mut u64;
            *w = render(self.site, lane, *w).ok().flatten().map_or(MINCODE_NONE, |p| p as u64);
        }
    }

    unsafe fn threshold(&self) -> i64 {
        if self.len < self.n || self.order != 0 {
            return 0;
        }
        self.count_of(self.at(self.worst))
    }

    unsafe fn offer(&mut self, step: &mut cancelpoll::Step, r: *const u8) -> Result<(), cancelpoll::Canceled> {
        if self.len < self.n {
            core::ptr::copy_nonoverlapping(r, self.base.add(self.len * self.rec), self.rec);
            self.render_kept(self.len);
            self.len += 1;
            if self.len == self.n {
                self.find_worst(step)?;
            }
            return Ok(());
        }
        if self.better(r, true, self.at(self.worst)) {
            core::ptr::copy_nonoverlapping(r, self.base.add(self.worst * self.rec), self.rec);
            self.render_kept(self.worst);
            self.find_worst(step)?;
        }
        Ok(())
    }

    /// C9: the accumulator's slots in batches (a slot under the threshold is a test); an offer that takes a copy ticks in
    /// `find_worst`.
    unsafe fn offer_all(&mut self, step: &mut cancelpoll::Step, t: &HashAgg) -> Result<(), cancelpoll::Canceled> {
        let base = t.base_ptr();
        let off_used = t.layout.sentinel_off as usize;
        let mut thr = self.threshold();
        for i in 0..t.cap as usize {
            step.batch(i)?;
            let p = base.add(i * self.rec);
            if *(p.add(off_used) as *const u64) == 0 {
                continue;
            }
            if self.order != 0 {
                self.offer(step, p)?;
                continue;
            }
            let w = p.add(self.off_count) as *mut u64;
            *w &= !MOVING;
            if ((*w) as i64) < thr {
                continue;
            }
            self.offer(step, p)?;
            thr = self.threshold();
        }
        Ok(())
    }
}

pub fn partition_hash(layout: &RecordLayout, keys: &[u64]) -> KeyHash {
    KeyHash::of(&keys[..layout.partition_keys()])
}
