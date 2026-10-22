//! D-STATS (seal-fusion charter §4, ruled): the BATCHED fold kernels behind
//! [`crate::builder::ColumnMeta`]'s span face. The incumbent per-value walk
//! (`observe_value_at`) pays class dispatch + key-derivation branch soup +
//! sketch feeding PER VALUE (~22 ns/value of fold shell alone; the stats
//! plane totals 59.8% of `seal_part`). These kernels pay the dispatch once
//! per SPAN and run the folds as flat passes over the already-materialized
//! datum array — the SEAL-FUSION `part_ptrs` win the charter names.
//!
//! ## The byte-neutrality law (charter conditions, carried verbatim)
//!
//! The sealed bytes are pure functions of fold RESULTS, so batching is
//! byte-invisible iff:
//! - (a) reordering is confined to order-INSENSITIVE accumulators: min/max,
//!   the i128 SUM (integer — `SumKind` has no float variant, so regrouping
//!   is exact), zero/nonnull counts, bloom bit-OR, HLL max-registers, the
//!   DistAcc multiset. The split-lane SUM below regroups integer addition
//!   only.
//! - (b) order-SENSITIVE facts keep row order: `first_key`/`last_key`, the
//!   asc/desc/ties transition chain (folded over adjacent pairs of the
//!   nonnull key sequence IN ROW ORDER, carry-in from the previous span),
//!   and the PSMA keybuf (extended in row order).
//! - (c) `meta_hash128` stays bit-identical — the same function over the
//!   same canonical bytes; batching only stages its OUTPUT in arrays.
//! - (d) the distribution finish path is untouched (`sketch.rs` — one
//!   `finish_distribution`, both producers).
//!
//! Kill switch: `PGRUST_SEAL_BATCH_FOLD=0|off` (read by the builder at
//! construction — the `populate_on` class) restores the incumbent
//! per-value path, which stays in `builder.rs` UNTOUCHED as the control
//! arm. The equivalence battery (`tests/builder_tests.rs`) pins
//! batched == per-value on adversarial corpora; the seeded-divergence
//! tooth below proves the battery can fail.
//!
//! ## Dict fold-from-codes (the #971/#983 DICT-DEDUP successor move)
//!
//! Dict codes are byte-rank ranks (code order == entry byte order — the
//! order certificate the dict build re-proves at emit), and the DICT-DEDUP
//! feed hands this builder the byte-rank entry list, so every per-value
//! fact is a pure function of the CODE through once-per-stream tables:
//! prefix keys, byte/char lengths, and the `meta_hash128` pair are computed
//! once per ENTRY (D values), and the per-row fold becomes integer table
//! lookups over the codes the seal already materialized. Bloom/HLL inserts
//! collapse to once per DISTINCT code per granule — bit-identical to the
//! per-row inserts because both accumulators are idempotent (bit-OR /
//! max-register). Counts that carry multiplicity (`nonnull`, `byte_len_sum`,
//! the keybuf, the sortedness chain) stay per-row lookups in row order.

use cost_profile::machine;

use crate::builder::{utf8_char_count, CurGranule};
use crate::format::abi::{datum_canonical_bytes, EncodeInput};
use crate::format::class::StorageClass;
use crate::hash::meta_hash128;
use crate::key::{
    bool_key, f32_key_from_datum, f64_key_from_datum, interval_cmp_key, memcmp_fixed_exact_key,
    memcmp_fixed_prefix_key, memcmp_var_prefix_key, numeric_pack_at_scale, packed_numeric_key,
    signed_word_key, timetz_utc_key, unsigned_flip_key, unsigned_small_key, KeyDerivation,
    NumericPack,
};
use crate::profile::{LenStats, MetaProfile, SumKind, ZeroKind};
use crate::sketch::DistAcc;

/// How many DistAcc slot prefetches run ahead of the scalar inserts (answers
/// do not depend on it). Timed by tests/cost_fit_distacc.rs on the machine
/// profile's reference (Neoverse V2, dev seed, 2026-10-03): with the table in
/// L1d or L2 (NDV 2^10, 2^16) every distance costs 9-11% over none (7.9 vs
/// 8.6 ns, 14.9 vs 16.5 ns per value), in DRAM (NDV 2^22) no distance differs
/// beyond noise (79-82 ns). The writer ABAB (two binaries, 8 and 0,
/// interleaved on one reference box, W0 COPY of 8 M rows x 4 int8 into
/// pgrcolumnar, six cells per arm, 2026-10-03) did not show 0 better when
/// cache-resident (NDV 2^10: 7115 vs 7090 ms median, +0.4 %) nor different
/// in DRAM (NDV 2^22: 13348 vs 13333 ms, +0.1 %): the kernel's 0.7 ns per
/// value is under 0.5 % of a COPY, so the value stays.
pub(crate) const DIST_PREFETCH_AHEAD: usize = 8;

/// SUM split-lane chunk: half one thread's L1d of u64 lane words on this
/// host's cache facts (`machine::host`; 4096 on the reference's 64 KiB), so a
/// chunk stays resident while both lanes fold it. Exact for any chunk below
/// 2^32: the low-32 unsigned lane sums at most `chunk * (2^32 - 1)` and the
/// high-32 lane at most `chunk * 2^31` in magnitude, inside u64 / i64 (the
/// chunk is held under 2^32).
fn sum_chunk() -> usize {
    machine::host().l1_span(8, 1, 2).min((1 << 32) - 1) as usize
}

/// Reusable span scratch (owned by the builder; allocation-free spans after
/// warmup).
#[derive(Debug, Default)]
pub(crate) struct BatchScratch {
    /// Densified valid datum words (word classes).
    vals: Vec<u64>,
    /// Granule-relative rows of the densified values.
    rows: Vec<u32>,
    /// Derived keys of the key-carrying subsequence (row order).
    keys: Vec<i64>,
    /// Rows aligned with `keys`.
    krows: Vec<u32>,
    /// Precomputed hash lanes of the span's canonical values (row order).
    h1: Vec<u64>,
    h2: Vec<u64>,
    /// Byte classes: canonical (addr, len) of each valid readable value —
    /// addresses point into writer-owned images (the `datum_canonical_bytes`
    /// caller contract), captured as usize so the scratch stays `Send`.
    ptrs: Vec<(usize, u32)>,
    /// Rows aligned with `ptrs`.
    prow: Vec<u32>,
    /// `same[i]`: value `i` is byte-equal to value `i - 1` (byte classes).
    same: Vec<bool>,
    /// Char counts aligned with `ptrs`, when the hash lane computed them.
    clen: Vec<u32>,
    /// Dict-codes fold: densified codes of valid rows.
    codes: Vec<u32>,
    /// Dict-codes fold: each code's count in the granule (zero between granules) + the distinct codes in first-seen
    /// order.
    cnt: Vec<u16>,
    seen_list: Vec<u32>,
}

/// Once-per-stream lookup tables for the dict fold-from-codes face, built
/// from the DICT-DEDUP feed's byte-rank entry list (index == global code).
#[derive(Debug)]
pub(crate) struct DictFoldTables {
    /// Per-code derived key (`MemcmpVarPrefix` profiles); `None` when the
    /// profile derives no keys (non-C collations).
    keys: Option<Vec<i64>>,
    blen: Vec<u32>,
    /// Per-code UTF-8 char count (BytesAndChars profiles only, else empty).
    clen: Vec<u32>,
    /// Per-code `meta_hash128` (hash-carrying profiles only, else empty).
    h1: Vec<u64>,
    h2: Vec<u64>,
    hashes: bool,
    chars: bool,
}

impl BatchScratch {
    /// The span scratch's heap bytes (capacities): what the meta builder's charge follows (`kept.rs`).
    pub(crate) fn heap_bytes(&self) -> u64 {
        use crate::kept::cap_bytes;
        cap_bytes!(self.vals)
            + cap_bytes!(self.rows)
            + cap_bytes!(self.keys)
            + cap_bytes!(self.krows)
            + cap_bytes!(self.h1)
            + cap_bytes!(self.h2)
            + cap_bytes!(self.ptrs)
            + cap_bytes!(self.prow)
            + cap_bytes!(self.same)
            + cap_bytes!(self.clen)
            + cap_bytes!(self.codes)
            + cap_bytes!(self.cnt)
            + cap_bytes!(self.seen_list)
    }
}

impl DictFoldTables {
    /// The tables' heap bytes, their box included: what the meta builder's charge follows (`kept.rs`).
    pub(crate) fn heap_bytes(&self) -> u64 {
        use crate::kept::cap_bytes;
        std::mem::size_of::<DictFoldTables>() as u64
            + self.keys.as_ref().map_or(0, |v| cap_bytes!(v))
            + cap_bytes!(self.blen)
            + cap_bytes!(self.clen)
            + cap_bytes!(self.h1)
            + cap_bytes!(self.h2)
    }
}

#[cfg(test)]
pub(crate) mod seed {
    //! The born-RED tooth: when armed, the batched hash lane SKIPS the
    //! span's last HLL update — a deliberate accumulator-coverage leak.
    //! The equivalence battery must go RED with this armed and GREEN with
    //! it disarmed; that failure is the proof the battery has teeth.
    //! THREAD-LOCAL so the armed test cannot poison concurrently running
    //! suites (builders fold on the driving thread).
    use std::cell::Cell;
    thread_local! {
        pub static SKIP_LAST_HLL: Cell<bool> = const { Cell::new(false) };
    }
    pub fn armed() -> bool {
        SKIP_LAST_HLL.with(|c| c.get())
    }
    pub fn arm(on: bool) {
        SKIP_LAST_HLL.with(|c| c.set(on));
    }
}

/// The batched span fold — the exact fold set of `observe_value_at` over
/// rows `[first, first+n)` of `input`, restructured into per-lane passes.
#[allow(clippy::too_many_arguments)]
pub(crate) fn observe_rows_batched(
    profile: &MetaProfile,
    psma_populate: bool,
    cur: &mut CurGranule,
    dist: Option<&mut DistAcc>,
    bank: Option<&mut crate::format::bank::LogicalColHash>,
    s: &mut BatchScratch,
    base: u32,
    input: &EncodeInput<'_>,
    first: u32,
    n: u32,
) {
    match profile.class {
        StorageClass::ByvalWord { .. }
        | StorageClass::F32
        | StorageClass::F64
        | StorageClass::Bool => observe_words(profile, psma_populate, cur, dist, s, base, input, first, n),
        StorageClass::Fixed { .. } | StorageClass::VarlenaVerbatim => {
            observe_bytes(profile, psma_populate, cur, dist, bank, s, base, input, first, n)
        }
    }
}

// ---------------------------------------------------------------------------
// word classes
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
#[inline(never)]
fn observe_words(
    profile: &MetaProfile,
    psma_populate: bool,
    cur: &mut CurGranule,
    dist: Option<&mut DistAcc>,
    s: &mut BatchScratch,
    base: u32,
    input: &EncodeInput<'_>,
    first: u32,
    n: u32,
) {
    // Densify: valid datums (+ their granule-relative rows when the PSMA
    // keybuf will want them).
    let need_rows = psma_populate && profile.key != KeyDerivation::None;
    s.vals.clear();
    s.rows.clear();
    match input.validity {
        None => {
            s.vals
                .extend_from_slice(&input.datums[first as usize..(first + n) as usize]); // alloc: Node charged kept
            if need_rows {
                s.rows.extend(base + first..base + first + n); // alloc: Node charged kept
            }
        }
        Some(_) => {
            for r in first..first + n {
                if input.valid(r) {
                    s.vals.push(input.datums[r as usize]); // alloc: Node charged kept
                    if need_rows {
                        s.rows.push(base + r); // alloc: Node charged kept
                    }
                }
            }
        }
    }
    cur.nonnull += s.vals.len() as u32;
    if s.vals.is_empty() {
        return;
    }

    // Keys: derive per word (each word derivation is total but
    // UnsignedWordSmall's, whose out-of-image word poisons), then the
    // shared chain fold.
    if profile.key != KeyDerivation::None {
        s.keys.clear();
        match profile.key {
            KeyDerivation::SignedWord => {
                s.keys.extend(s.vals.iter().map(|&w| signed_word_key(w).raw())); // alloc: Node charged kept
            }
            KeyDerivation::UnsignedWordSmall => {
                if s.vals.iter().all(|&w| unsigned_small_key(w).is_some()) {
                    s.keys.extend(s.vals.iter().map(|&w| w as i64)); // alloc: Node charged kept
                } else {
                    // A word outside the image poisons the grain; the rest
                    // fold as the per-value path folds them.
                    cur.poisoned = true;
                    let mut j = 0;
                    for i in 0..s.vals.len() {
                        if let Some(k) = unsigned_small_key(s.vals[i]) {
                            s.keys.push(k.raw()); // alloc: Node charged kept
                            if need_rows {
                                s.rows[j] = s.rows[i];
                            }
                            j += 1;
                        }
                    }
                    if need_rows {
                        s.rows.truncate(j);
                    }
                }
            }
            KeyDerivation::UnsignedWordFlip => {
                s.keys
                    .extend(s.vals.iter().map(|&w| unsigned_flip_key(w).raw())); // alloc: Node charged kept
            }
            KeyDerivation::Bool => {
                s.keys.extend(s.vals.iter().map(|&w| bool_key(w).raw())); // alloc: Node charged kept
            }
            KeyDerivation::Float32 => {
                s.keys
                    .extend(s.vals.iter().map(|&w| f32_key_from_datum(w).raw())); // alloc: Node charged kept
            }
            KeyDerivation::Float64 => {
                s.keys
                    .extend(s.vals.iter().map(|&w| f64_key_from_datum(w).raw())); // alloc: Node charged kept
            }
            // `MetaProfile::derive` never pairs a byte derivation with a
            // word class; degrade (not panic) if a foreign profile appears.
            _ => {
                debug_assert!(false, "byte key derivation on a word class");
                cur.poisoned = true;
            }
        }
        if !s.keys.is_empty() {
            fold_key_chain(cur, &s.keys, &s.rows, need_rows);
        }
    }

    // SUM (integer regroup — exact under any association).
    match profile.sum {
        SumKind::None => {}
        SumKind::SignedWord => {
            for chunk in s.vals.chunks(sum_chunk()) {
                let (mut lo, mut hi) = (0u64, 0i64);
                for &w in chunk {
                    lo += w & 0xFFFF_FFFF;
                    hi += (w as i64) >> 32;
                }
                cur.sum += ((hi as i128) << 32) + lo as i128;
            }
        }
        SumKind::UnsignedWord => {
            for chunk in s.vals.chunks(sum_chunk()) {
                let (mut lo, mut hi) = (0u64, 0u64);
                for &w in chunk {
                    lo += w & 0xFFFF_FFFF;
                    hi += w >> 32;
                }
                cur.sum += (((hi as u128) << 32) + lo as u128) as i128;
            }
        }
        SumKind::BoolTrues => {
            cur.sum += s.vals.iter().filter(|&&w| w != 0).count() as i128;
        }
        SumKind::PackedI64 => {
            debug_assert!(false, "PackedI64 sum on a word class");
        }
    }

    // Zero counts.
    match profile.zero {
        ZeroKind::None | ZeroKind::PackedI64 => {}
        ZeroKind::Word | ZeroKind::BoolFalse => {
            cur.zero += s.vals.iter().filter(|&&w| w == 0).count() as u64;
        }
        ZeroKind::Float => {
            let is_f32 = matches!(profile.class, StorageClass::F32);
            cur.zero += s
                .vals
                .iter()
                .filter(|&&w| {
                    if is_f32 {
                        f32::from_bits(w as u32) == 0.0
                    } else {
                        f64::from_bits(w) == 0.0
                    }
                })
                .count() as u64;
        }
    }

    // Hash lane: the §18.1 canonical image of every class here is ONE
    // zero-padded word of `width` bytes (Bool: the 0/1 truth), so the hash
    // is `meta_hash128_word` (PGRC-WRITER-FAST `word`: bit-identical to
    // `meta_hash128` of the image), staged into arrays, sketches fed from
    // the arrays and the DistAcc through its word face.
    if profile.hashes_values() {
        let width: u32 = match profile.class {
            StorageClass::ByvalWord { width, .. } => width as u32,
            StorageClass::F32 => 4,
            StorageClass::F64 => 8,
            StorageClass::Bool => 1,
            _ => unreachable!("word path"),
        };
        let is_bool = matches!(profile.class, StorageClass::Bool);
        let mask = if width >= 8 { u64::MAX } else { (1u64 << (8 * width)) - 1 };
        if let Some(d) = dist {
            if d.word_memo_ready() {
                observe_words_memo(profile, cur, d, &s.vals, width, is_bool, mask);
                return;
            }
            observe_words_hashed(profile, cur, Some(d), s, width, is_bool, mask);
            return;
        }
        observe_words_hashed(profile, cur, None, s, width, is_bool, mask);
    }
}

/// The word lane with the DistAcc's inline arm keyed by the image itself: each value costs one probe, and only a
/// value's first sight in the granule (the entry's stamp) is hashed for HLL and bloom; both are idempotent, so the
/// registers and bits equal [`observe_words_hashed`]'s per-value feed, and the counts are the same counts.
#[inline(never)]
fn observe_words_memo(
    profile: &MetaProfile,
    cur: &mut CurGranule,
    d: &mut DistAcc,
    vals: &[u64],
    width: u32,
    is_bool: bool,
    mask: u64,
) {
    #[cfg(test)]
    let skip_hll = seed::armed();
    #[cfg(not(test))]
    let skip_hll = false;
    let stamp = cur.granule + 1;
    let bloom = cur.bloom.is_some() || cur.bloom_defer;
    let len = width as u8;
    let img = |w: u64| if is_bool { (w != 0) as u64 } else { w & mask };
    let prefetch = d.word_prefetch_pays();
    // HLL and bloom take each distinct value once per granule: the values the memo reports as first sights.
    let mut feed = |wm: u64| {
        let (h1, h2) = crate::hash::meta_hash128_word(wm, width);
        if profile.ndv && !skip_hll {
            cur.hll.observe_hash(h1);
        }
        if bloom {
            cur.bloom_stage(h1, h2);
        }
    };
    if let Err(i) = d.memo_words(vals, img, len, stamp, prefetch, &mut feed) {
        // The table could not grow at value `i`: the accumulator failed there, and from then on every value is a
        // first sight (the per-value face answers `None` for each).
        for &w in &vals[i..] {
            feed(img(w));
        }
    }
}

/// The word lane's hash pass: hashes staged per value (a run repeat reuses its predecessor's), then HLL / bloom and
/// the DistAcc's word face from the arrays.
#[allow(clippy::too_many_arguments)]
#[inline(never)]
fn observe_words_hashed(
    profile: &MetaProfile,
    cur: &mut CurGranule,
    dist: Option<&mut DistAcc>,
    s: &mut BatchScratch,
    width: u32,
    is_bool: bool,
    mask: u64,
) {
    {
        s.h1.clear();
        s.h2.clear();
        // PGRC-WRITER-FAST `run`: a value equal to its predecessor reuses
        // its hashes.
        let (mut pw, mut ph1, mut ph2) = (0u64, 0u64, 0u64);
        let mut have = false;
        for &w in &s.vals {
            let wm = if is_bool { (w != 0) as u64 } else { w & mask };
            if have && wm == pw {
                s.h1.push(ph1); // alloc: Node charged kept
                s.h2.push(ph2); // alloc: Node charged kept
                continue;
            }
            let (h1, h2) = crate::hash::meta_hash128_word(wm, width);
            (pw, ph1, ph2, have) = (wm, h1, h2, true);
            s.h1.push(h1); // alloc: Node charged kept
            s.h2.push(h2); // alloc: Node charged kept
        }
        feed_hll_bloom(profile, cur, &s.h1, &s.h2);
        if let Some(d) = dist {
            // `run`: a repeat counts on the entry its predecessor landed on.
            let mut prev: Option<u64> = None;
            for i in 0..s.vals.len() {
                let w = s.vals[i];
                let wm = if is_bool { (w != 0) as u64 } else { w & mask };
                if prev == Some(wm) && d.repeat_last() {
                    continue;
                }
                prev = Some(wm);
                if i + 2 * DIST_PREFETCH_AHEAD < s.h1.len() {
                    d.prefetch(s.h1[i + 2 * DIST_PREFETCH_AHEAD]);
                }
                if i + DIST_PREFETCH_AHEAD < s.h1.len() {
                    d.prefetch_entry(s.h1[i + DIST_PREFETCH_AHEAD]);
                }
                d.observe_hashed_word(s.h1[i], s.h2[i], wm, width as u8);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// byte classes (Fixed / VarlenaVerbatim)
// ---------------------------------------------------------------------------

/// `meta_hash128(b)`, the O-10 bank digests over `bank` seeds when asked (`meta_hash128_and_digests`), and
/// `utf8_char_count(b)` in ONE pass over the words: a continuation byte is one with bits 7..6 = 10, counted per word
/// in the shadow of the hash chains' latency (zero tail padding counts none). Pinned against the separate functions
/// by `tests::builder_tests::fused_hash_chars_equal_the_separate_passes`.
#[inline]
pub(crate) fn hash_and_chars(bytes: &[u8], bank: Option<(u64, u64)>) -> (u64, u64, u64, u64, u32) {
    use crate::hash::{fold64, mix64};
    const LANE1: u64 = 0x3168_6d32_6372_6770;
    const LANE2: u64 = 0x3268_6d32_6372_6770;
    const HI: u64 = 0x8080_8080_8080_8080;
    let cont = |w: u64| (w & HI & !((w << 1) & HI)).count_ones();
    let len = bytes.len() as u64;
    let (mut a, mut b) = (fold64(LANE1, len), fold64(LANE2, len));
    let (bk, with_bank) = match bank {
        Some(s) => (s, true),
        None => ((0, 0), false),
    };
    let (mut c, mut d) = (fold64(bk.0, len), fold64(bk.1, len));
    let mut n_cont = 0u32;
    let mut chunks = bytes.chunks_exact(8);
    for ch in &mut chunks {
        let w = u64::from_le_bytes(ch.try_into().expect("len 8"));
        a = fold64(a, w);
        b = fold64(b, w);
        if with_bank {
            c = fold64(c, w);
            d = fold64(d, w);
        }
        n_cont += cont(w);
    }
    let rem = chunks.remainder();
    if !rem.is_empty() {
        let mut t = [0u8; 8];
        t[..rem.len()].copy_from_slice(rem);
        let w = u64::from_le_bytes(t);
        a = fold64(a, w);
        b = fold64(b, w);
        if with_bank {
            c = fold64(c, w);
            d = fold64(d, w);
        }
        n_cont += cont(w);
    }
    (mix64(a), mix64(b), c, d, (bytes.len() as u32).wrapping_sub(n_cont))
}

/// Rehydrate one captured canonical span. SAFETY: every `(addr, len)` in
/// `BatchScratch::ptrs` was minted THIS span from a live
/// `datum_canonical_bytes` slice (the pointer arms return views into the
/// writer-owned image, untouched by the 8-byte scratch), and those images
/// outlive the seal — the same caller contract the incumbent walk and
/// `verify_roundtrip` ride. The unconstrained lifetime never escapes this
/// module: callers consume the slice within the span passes.
fn slice_of<'x>(p: (usize, u32)) -> &'x [u8] {
    unsafe { core::slice::from_raw_parts(p.0 as *const u8, p.1 as usize) }
}

#[allow(clippy::too_many_arguments)]
#[inline(never)]
fn observe_bytes(
    profile: &MetaProfile,
    psma_populate: bool,
    cur: &mut CurGranule,
    dist: Option<&mut DistAcc>,
    mut bank: Option<&mut crate::format::bank::LogicalColHash>,
    s: &mut BatchScratch,
    base: u32,
    input: &EncodeInput<'_>,
    first: u32,
    n: u32,
) {
    // Densify + canonicalize ONCE: capture (addr, len) of each valid
    // value's canonical bytes. The pointer arms of `datum_canonical_bytes`
    // never touch the 8-byte scratch — the returned slice aims into the
    // writer-owned image, which outlives the span (the same caller
    // contract the incumbent walk and `verify_roundtrip` ride).
    s.ptrs.clear();
    s.prow.clear();
    let mut tmp = [0u8; 8];
    for r in first..first + n {
        if !input.valid(r) {
            continue;
        }
        cur.nonnull += 1;
        match unsafe { datum_canonical_bytes(profile.class, input.datums[r as usize], &mut tmp) } {
            Ok(b) => {
                s.ptrs.push((b.as_ptr() as usize, b.len().min(u32::MAX as usize) as u32)); // alloc: Node charged kept
                s.prow.push(base + r); // alloc: Node charged kept
            }
            Err(_) => {
                // Unreadable image: degrade, never guess (incumbent arm's
                // exact behavior — the value is counted nonnull, poisons
                // the grain, and participates in nothing else).
                debug_assert!(false, "unreadable datum image at meta build");
                cur.poisoned = true;
            }
        }
    }
    if s.ptrs.is_empty() {
        return;
    }

    // Keys (+ the PackedNumeric sum/zero coupling — pack ONCE per value).
    match profile.key {
        KeyDerivation::None => {}
        KeyDerivation::MemcmpFixed { len } => {
            s.keys.clear();
            s.krows.clear();
            for (i, &p) in s.ptrs.iter().enumerate() {
                let b = slice_of(p);
                if b.len() != len as usize {
                    cur.poisoned = true;
                    continue;
                }
                let raw = if len <= 8 {
                    memcmp_fixed_exact_key(b).raw()
                } else {
                    memcmp_fixed_prefix_key(b).raw()
                };
                s.keys.push(raw); // alloc: Node charged kept
                s.krows.push(s.prow[i]); // alloc: Node charged kept
            }
            fold_key_chain(cur, &s.keys, &s.krows, psma_populate);
        }
        KeyDerivation::MemcmpVarPrefix => {
            s.keys.clear();
            for &p in s.ptrs.iter() {
                s.keys.push(memcmp_var_prefix_key(slice_of(p)).raw()); // alloc: Node charged kept
            }
            fold_key_chain(cur, &s.keys, &s.prow, psma_populate);
        }
        KeyDerivation::IntervalCmpSat => {
            s.keys.clear();
            s.krows.clear();
            for (i, &p) in s.ptrs.iter().enumerate() {
                match interval_cmp_key(slice_of(p)) {
                    Some(k) => {
                        s.keys.push(k.raw()); // alloc: Node charged kept
                        s.krows.push(s.prow[i]); // alloc: Node charged kept
                    }
                    None => cur.poisoned = true,
                }
            }
            fold_key_chain(cur, &s.keys, &s.krows, psma_populate);
        }
        KeyDerivation::TimetzUtc => {
            s.keys.clear();
            s.krows.clear();
            for (i, &p) in s.ptrs.iter().enumerate() {
                match timetz_utc_key(slice_of(p)) {
                    Some(k) => {
                        s.keys.push(k.raw()); // alloc: Node charged kept
                        s.krows.push(s.prow[i]); // alloc: Node charged kept
                    }
                    None => cur.poisoned = true,
                }
            }
            fold_key_chain(cur, &s.keys, &s.krows, psma_populate);
        }
        KeyDerivation::PackedNumeric { scale } => {
            // Key + SUM + zero ride ONE pack per value; a pack failure
            // poisons the grain and contributes to none of the three (the
            // incumbent arm's validity coupling, exactly).
            s.keys.clear();
            s.krows.clear();
            for (i, &p) in s.ptrs.iter().enumerate() {
                match numeric_pack_at_scale(slice_of(p), scale) {
                    NumericPack::Packed(v) => {
                        s.keys.push(packed_numeric_key(v).raw()); // alloc: Node charged kept
                        s.krows.push(s.prow[i]); // alloc: Node charged kept
                        cur.sum += v as i128;
                        if v == 0 {
                            cur.zero += 1;
                        }
                    }
                    _ => cur.poisoned = true,
                }
            }
            fold_key_chain(cur, &s.keys, &s.krows, psma_populate);
        }
        // `MetaProfile::derive` never pairs a word derivation with a byte
        // class; degrade (not panic) if a foreign profile appears.
        _ => {
            debug_assert!(false, "word key derivation on a byte class");
            cur.poisoned = true;
        }
    }

    // PGRC-WRITER-FAST `run`: a value byte-equal to its predecessor reuses its char count, its hashes, its bank
    // digests and its DistAcc entry; the compare runs once per value.
    let chars = profile.len_stats == LenStats::BytesAndChars;
    s.same.clear();
    if chars || profile.hashes_values() {
        s.same.push(false); // alloc: Node charged kept
        for w in s.ptrs.windows(2) {
            let (a, b) = (w[0], w[1]);
            s.same.push(a.1 == b.1 && slice_of(a) == slice_of(b)); // alloc: Node charged kept
        }
    }

    // Hash lane; with char stats, the char counts ride the same pass (`hash_and_chars`).
    let fused = chars && profile.hashes_values();
    s.clen.clear();
    if profile.hashes_values() {
        s.h1.clear();
        s.h2.clear();
        let same = |i: usize| -> bool { s.same[i] };
        // `bank`: the O-10 logical digests ride the same pass (four chains,
        // one load per word) when the seal asked for them.
        let seeds = bank.is_some().then_some(crate::format::bank::BANK_LANE_SEEDS);
        let (mut plo, mut phi, mut pcl) = (0u64, 0u64, 0u32);
        for i in 0..s.ptrs.len() {
            if same(i) {
                let (a, b) = (s.h1[i - 1], s.h2[i - 1]);
                s.h1.push(a); // alloc: Node charged kept
                s.h2.push(b); // alloc: Node charged kept
                if let Some(bk) = bank.as_deref_mut() {
                    bk.add(plo, phi);
                }
                if fused {
                    s.clen.push(pcl); // alloc: Node charged kept
                }
                continue;
            }
            let v = slice_of(s.ptrs[i]);
            let (h1, h2) = if fused {
                let (h1, h2, lo, hi, cl) = hash_and_chars(v, seeds);
                s.clen.push(cl); // alloc: Node charged kept
                pcl = cl;
                if let Some(bk) = bank.as_deref_mut() {
                    (plo, phi) = (lo, hi);
                    bk.add(lo, hi);
                }
                (h1, h2)
            } else {
                match bank.as_deref_mut() {
                    Some(bk) => {
                        let (h1, h2, lo, hi) =
                            crate::hash::meta_hash128_and_digests(v, crate::format::bank::BANK_LANE_SEEDS);
                        (plo, phi) = (lo, hi);
                        bk.add(lo, hi);
                        (h1, h2)
                    }
                    None => meta_hash128(v),
                }
            };
            s.h1.push(h1); // alloc: Node charged kept
            s.h2.push(h2); // alloc: Node charged kept
        }
        feed_hll_bloom(profile, cur, &s.h1, &s.h2);
        if let Some(d) = dist {
            for i in 0..s.ptrs.len() {
                if same(i) && d.repeat_last() {
                    continue;
                }
                if i + 3 * DIST_PREFETCH_AHEAD < s.h1.len() {
                    d.prefetch(s.h1[i + 3 * DIST_PREFETCH_AHEAD]);
                }
                if i + 2 * DIST_PREFETCH_AHEAD < s.h1.len() {
                    d.prefetch_entry(s.h1[i + 2 * DIST_PREFETCH_AHEAD]);
                }
                if i + DIST_PREFETCH_AHEAD < s.h1.len() {
                    d.prefetch_bytes(s.h1[i + DIST_PREFETCH_AHEAD]);
                }
                // SAFETY: the value's canonical bytes lie in the input's
                // writer-owned image (the densify above); a pinned builder's
                // driver keeps it valid while the accumulator is used.
                unsafe { d.observe_hashed_input(s.h1[i], s.h2[i], slice_of(s.ptrs[i])) };
            }
        }
    }

    // Length stats (varlena payload units; BOTH units for text — #80).
    if profile.len_stats != LenStats::None {
        let mut clen = 0u32;
        for (i, &p) in s.ptrs.iter().enumerate() {
            let blen = p.1;
            cur.blen_min = cur.blen_min.min(blen);
            cur.blen_max = cur.blen_max.max(blen);
            cur.blen_sum += blen as u64;
            if chars {
                if fused {
                    clen = s.clen[i];
                } else if !s.same[i] {
                    clen = utf8_char_count(slice_of(p));
                }
                cur.clen_min = cur.clen_min.min(clen);
                cur.clen_max = cur.clen_max.max(clen);
                if clen != blen {
                    cur.ascii = false;
                }
            }
        }
        cur.have_len = true;
    }
}

// ---------------------------------------------------------------------------
// shared folds
// ---------------------------------------------------------------------------

/// The order-sensitive key chain over one span's derived-key subsequence
/// (row order; `keys[i]` at granule-relative row `rows[i]`): first/last
/// key, the asc/desc/ties transition fold (carry-in from `cur.last_key`),
/// min/max, and the PSMA keybuf extension. Exactly `observe_value_at`'s
/// per-key updates, chained.
fn fold_key_chain(cur: &mut CurGranule, keys: &[i64], rows: &[u32], stage_psma: bool) {
    if keys.is_empty() {
        return;
    }
    if stage_psma {
        cur.keybuf.extend(rows, keys);
    }
    let (mut asc, mut desc, mut ties) = (cur.asc, cur.desc, cur.ties);
    if cur.have_key {
        let (prev, k0) = (cur.last_key, keys[0]);
        asc &= k0 >= prev;
        desc &= k0 <= prev;
        ties |= k0 == prev;
    } else {
        cur.first_key = keys[0];
        cur.have_key = true;
    }
    // Adjacent-pair transition fold: branchless, auto-vectorizable.
    for i in 1..keys.len() {
        let (a, b) = (keys[i - 1], keys[i]);
        asc &= b >= a;
        desc &= b <= a;
        ties |= b == a;
    }
    let (mut kmin, mut kmax) = (cur.kmin, cur.kmax);
    for &k in keys {
        kmin = kmin.min(k);
        kmax = kmax.max(k);
    }
    cur.asc = asc;
    cur.desc = desc;
    cur.ties = ties;
    cur.kmin = kmin;
    cur.kmax = kmax;
    cur.last_key = keys[keys.len() - 1];
}

/// Feed HLL + the staged bloom block from precomputed hash arrays. Both
/// accumulators are idempotent + commutative, so array order is free; the
/// arrays are row order anyway.
fn feed_hll_bloom(profile: &MetaProfile, cur: &mut CurGranule, h1: &[u64], h2: &[u64]) {
    if profile.ndv {
        #[cfg(test)]
        let skip_last = seed::armed();
        #[cfg(not(test))]
        let skip_last = false;
        let upto = if skip_last { h1.len().saturating_sub(1) } else { h1.len() };
        // PGRC-WRITER-FAST `run`: HLL is max-register idempotent — a
        // repeat of the previous hash sets nothing new.
        let mut prev: Option<u64> = None;
        for &h in &h1[..upto] {
            if prev != Some(h) {
                cur.hll.observe_hash(h);
                prev = Some(h);
            }
        }
    }
    if cur.bloom.is_some() || cur.bloom_defer {
        // `run`: bloom bit-OR is idempotent.
        let mut prev: Option<(u64, u64)> = None;
        for i in 0..h1.len() {
            if prev != Some((h1[i], h2[i])) {
                cur.bloom_stage(h1[i], h2[i]);
                prev = Some((h1[i], h2[i]));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// dict fold-from-codes
// ---------------------------------------------------------------------------

/// Build the once-per-stream code tables from the DICT-DEDUP feed's
/// byte-rank entry list (index == global code; strictly byte-ascending —
/// the dict build's contractual order certificate).
pub(crate) fn build_dict_tables(
    profile: &MetaProfile,
    entries: &[(Vec<u8>, u64)],
) -> DictFoldTables {
    let keys = match profile.key {
        KeyDerivation::MemcmpVarPrefix => Some(
            entries
                .iter()
                .map(|(b, _)| memcmp_var_prefix_key(b).raw())
                .collect(), // alloc: Node charged kept
        ),
        _ => None,
    };
    let chars = profile.len_stats == LenStats::BytesAndChars;
    let hashes = profile.hashes_values();
    let (mut h1, mut h2, mut clen) = (Vec::new(), Vec::new(), Vec::new());
    if hashes {
        h1.reserve(entries.len()); // alloc: Node charged kept
        h2.reserve(entries.len()); // alloc: Node charged kept
        if chars {
            // fallible: a refused reservation only leaves the pushes to grow it
            let _ = clen.try_reserve_exact(entries.len()); // alloc: Node charged kept
        }
        for (b, _) in entries {
            let (a, c) = if chars {
                let (a, c, _, _, n) = hash_and_chars(b, None);
                clen.push(n); // alloc: Node charged kept
                (a, c)
            } else {
                meta_hash128(b)
            };
            h1.push(a); // alloc: Node charged kept
            h2.push(c); // alloc: Node charged kept
        }
    } else if chars {
        clen = entries.iter().map(|(b, _)| utf8_char_count(b)).collect(); // alloc: Node charged kept
    }
    DictFoldTables {
        keys,
        blen: entries
            .iter()
            .map(|(b, _)| b.len().min(u32::MAX as usize) as u32)
            .collect(), // alloc: Node charged kept
        clen,
        h1,
        h2,
        hashes,
        chars,
    }
}

/// Fold one dict-stream granule FROM ITS CODES (`input.datums` = global
/// codes, row-dense, null slots carrying 0 — skipped via validity). Every
/// fact matches the hydrated-value walk over the same granule:
/// - keys/lengths are table lookups per row IN ROW ORDER (order-sensitive
///   facts + multiplicity sums preserved);
/// - bloom/HLL insert once per DISTINCT code (idempotent accumulators —
///   identical bits/registers to per-row inserts);
/// - the DistAcc is NOT fed here: the codes face is gated on the feed
///   being staged, which disarmed the accumulator (DICT-DEDUP).
pub(crate) fn observe_codes_granule(
    profile: &MetaProfile,
    psma_populate: bool,
    cur: &mut CurGranule,
    t: &DictFoldTables,
    s: &mut BatchScratch,
    base: u32,
    input: &EncodeInput<'_>,
) {
    // Densify valid (row, code); rows only for the PSMA key staging.
    let need_rows = psma_populate && t.keys.is_some();
    s.codes.clear();
    s.rows.clear();
    match input.validity {
        None => {
            s.codes
                .extend(input.datums[..input.rows as usize].iter().map(|&c| c as u32)); // alloc: Node charged kept
            if need_rows {
                s.rows.extend(base..base + input.rows); // alloc: Node charged kept
            }
        }
        Some(_) => {
            for r in 0..input.rows {
                if input.valid(r) {
                    s.codes.push(input.datums[r as usize] as u32); // alloc: Node charged kept
                    if need_rows {
                        s.rows.push(base + r); // alloc: Node charged kept
                    }
                }
            }
        }
    }
    debug_assert!(s.codes.iter().all(|&c| (c as usize) < t.blen.len()), "code out of dict range");
    cur.nonnull += s.codes.len() as u32;
    if s.codes.is_empty() {
        return;
    }

    // Keys through the code table (row order — the chain fold is shared
    // with the value walks).
    if let Some(kt) = &t.keys {
        s.keys.clear();
        for &c in &s.codes {
            s.keys.push(kt[c as usize]); // alloc: Node charged kept
        }
        fold_key_chain(cur, &s.keys, &s.rows, psma_populate);
    }

    // Each distinct code once, with its count in the granule (runs counted without touching the table): length
    // sums carry the count, min / max and the ASCII witness are idempotent, and bloom / HLL inserts are idempotent.
    let lens = profile.len_stats != LenStats::None;
    if !(lens || t.hashes) {
        return;
    }
    if s.cnt.len() < t.blen.len() {
        s.cnt.resize(t.blen.len(), 0); // alloc: Node charged kept
    }
    s.seen_list.clear();
    let (mut run_c, mut run_n) = (s.codes[0], 0u16);
    for &c in &s.codes {
        if c == run_c {
            run_n += 1;
            continue;
        }
        let e = &mut s.cnt[run_c as usize];
        if *e == 0 {
            s.seen_list.push(run_c); // alloc: Node charged kept
        }
        *e += run_n;
        (run_c, run_n) = (c, 1);
    }
    let e = &mut s.cnt[run_c as usize];
    if *e == 0 {
        s.seen_list.push(run_c); // alloc: Node charged kept
    }
    *e += run_n;
    #[cfg(test)]
    let skip_last = seed::armed();
    #[cfg(not(test))]
    let skip_last = false;
    let last = s.seen_list.len() - 1;
    for (i, &c) in s.seen_list.iter().enumerate() {
        let c = c as usize;
        let n = std::mem::take(&mut s.cnt[c]);
        if lens {
            let blen = t.blen[c];
            cur.blen_min = cur.blen_min.min(blen);
            cur.blen_max = cur.blen_max.max(blen);
            cur.blen_sum += u64::from(blen) * u64::from(n);
            if t.chars {
                let clen = t.clen[c];
                cur.clen_min = cur.clen_min.min(clen);
                cur.clen_max = cur.clen_max.max(clen);
                if clen != blen {
                    cur.ascii = false;
                }
            }
        }
        if t.hashes {
            if profile.ndv && !(skip_last && i == last) {
                cur.hll.observe_hash(t.h1[c]);
            }
            cur.bloom_stage(t.h1[c], t.h2[c]);
        }
    }
    if lens {
        cur.have_len = true;
    }
}
