//! ST-1 (OD-2, RULED 2026-08-12): the part-grain distribution sketch —
//! MCV top-k with EXACT per-part counts + equi-depth histogram bounds over
//! canonical value bytes, accumulated at seal (stats are a seal byproduct;
//! "banks ship stats-built" is structural). HLL NDV stays in-footer (spec
//! §8.4); this module never replaces it.
//!
//! Exactness class: EXACT-AT-SEAL per part-generation. Values longer than
//! [`STATS_MCV_VALUE_MAX`] participate in COUNTS but are ineligible for
//! the value lists (`long_values` records how many observations that
//! excluded — the fold reads it and degrades honestly instead of serving
//! a histogram that silently dropped the long tail).
//!
//! DISTACC-KILL (the prof-ingest finding): the original accumulator was a
//! `BTreeMap<Vec<u8>, u64>` — every observation paid an O(log D) tree
//! descent of byte compares, and that memcmp lane alone was ~13% of ALL
//! ingest CPU (~21% with the tree insert; profile of record
//! the prof-ingest results). The accumulator is now an open-addressing
//! hash table keyed on the 128-bit `meta_hash128` the builder ALREADY
//! computes for bloom/HLL: an observation costs one masked probe, a
//! 128-bit hash compare, and ONE byte-equality confirm on the (almost
//! always hit) full-hash match — the per-value byte-rank comparisons are
//! gone. Exactness is untouched: byte equality still adjudicates identity
//! (a 128-bit collision cannot merge two values), and the byte order the
//! BTreeMap provided for free is recovered by ONE finalize-time sort over
//! the surviving distinct set (D log D once per part, not N log D during
//! observe).
//!
//! Determinism: both arms feed [`ColDistribution`] through the same
//! byte-ordered finish path — iteration order of the hash table never
//! reaches sidecar bytes (the finalize sort is total: distinct entries
//! have distinct bytes). MCV ties break by byte order, as before.
//!
//! Kill switch: `PGRUST_PGRC_DISTACC_HASH=0|off` restores the BTreeMap
//! reference arm (the A/B control and the equivalence witness; both arms
//! are pinned output-equal by the adversarial battery below).
//!
//! DICT-DEDUP (the #971 follow-up finding): on dict-elected columns the
//! WRITER already maintains the part's exact counted distinct set — the
//! dict election's builder observes every non-null value to mint the
//! byte-rank dictionary. Maintaining a second distinct structure here over
//! the same bytes double-accounts the most expensive columns (strings).
//! [`distribution_from_sorted_counts`] is the dedup seam: the seal driver
//! hands the dict build's counted, byte-rank-sorted entry set to the meta
//! builder, which serves `distribution()` through the SAME finish path
//! ([`finish_distribution`]) this accumulator uses — one implementation of
//! the MCV/histogram finish, so the two producers cannot drift. Non-dict
//! columns, demoted dict arms, and the count-free D2 inherit path keep the
//! accumulator below.

use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BinaryHeap};

use crate::format::sidecar::{
    ColDistribution, STATS_HIST_BOUNDS, STATS_MCV_K, STATS_MCV_VALUE_MAX,
};
use crate::hash::meta_hash128;
use crate::kept::{cap_bytes, charged_vec, collect_on, collect_on_n, oom, Kept, On};
use memledger::ChargedVec;
use pgrc_format::{FormatError, FormatResult};
use crate::key::KeyDerivation;
use crate::profile::MetaProfile;

/// FOLDSTATS (lane AUTOVAC unit 3): the value order of a column's canonical bytes, which its histogram bounds follow
/// (the seal's equi-depth pass here, the ANALYZE fold's merge in [`crate::fold`]). The fixed-width word families'
/// canonical bytes are little-endian words: memcmp order is not their value order, and equi-depth bounds taken in
/// byte order are not quantiles. They cluster below each 64 Ki boundary of an int8 key, and the fold planned 57520 rows
/// for `id > 300000` against 200000 (box 44). Those families order by the decoded word; every other class keeps byte
/// order (memcmp-ordered classes, text under C, and the classes whose histogram the ANALYZE consumer refuses).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistOrder {
    /// memcmp of the canonical bytes.
    Bytes,
    /// A sign-extended little-endian word (int2/4/8, date, time, timestamp[tz], money).
    Signed,
    /// A zero-extended little-endian word (oid family, pg_lsn, xid8, bool).
    Unsigned,
    /// IEEE bits, little-endian, 4 or 8 bytes, in PG float order (NaN greatest, -0 = +0).
    Float,
}

impl HistOrder {
    /// The order of `profile`'s canonical bytes.
    pub fn of(profile: &MetaProfile) -> HistOrder {
        match profile.key {
            KeyDerivation::SignedWord => HistOrder::Signed,
            KeyDerivation::UnsignedWordSmall | KeyDerivation::UnsignedWordFlip | KeyDerivation::Bool => HistOrder::Unsigned,
            KeyDerivation::Float32 | KeyDerivation::Float64 => HistOrder::Float,
            _ => HistOrder::Bytes,
        }
    }

    /// The value key of canonical `bytes`, a word whose unsigned order is the value order (`None`: byte order, or a
    /// width the order does not decode). Distinct values share a key only where PG calls them equal (±0, NaN payloads).
    fn key(self, bytes: &[u8]) -> Option<u64> {
        const SIGN: u64 = 1 << 63;
        if self == HistOrder::Bytes || bytes.is_empty() || bytes.len() > 8 {
            return None;
        }
        let mut le = [0u8; 8];
        le[..bytes.len()].copy_from_slice(bytes);
        let w = u64::from_le_bytes(le);
        let shift = 64 - 8 * bytes.len() as u32;
        Some(match self {
            HistOrder::Bytes => return None,
            HistOrder::Signed => ((((w << shift) as i64) >> shift) as u64) ^ SIGN,
            HistOrder::Unsigned => w,
            HistOrder::Float => match bytes.len() {
                4 => (crate::key::f32_order_key(f32::from_bits(w as u32)).raw() as u64) ^ SIGN,
                8 => (crate::key::f64_order_key(f64::from_bits(w)).raw() as u64) ^ SIGN,
                _ => return None,
            },
        })
    }

    /// Value order, ties (±0, NaN payloads) broken by bytes: a total order on distinct canonical values.
    pub fn cmp(self, a: &[u8], b: &[u8]) -> Ordering {
        match (self.key(a), self.key(b)) {
            (Some(x), Some(y)) => x.cmp(&y).then_with(|| a.cmp(b)),
            _ => a.cmp(b),
        }
    }

    /// Value equality (for a histogram's adjacent bounds): ±0 and NaN payloads are one value in PG.
    pub fn same(self, a: &[u8], b: &[u8]) -> bool {
        match (self.key(a), self.key(b)) {
            (Some(x), Some(y)) => x == y,
            _ => a == b,
        }
    }
}

/// The DISTACC arm switch (read at accumulator construction — the
/// `populate_on` parse: unset or anything but "0"/"off" = hash arm).
fn hash_arm_on() -> bool {
    !matches!(
        std::env::var("PGRUST_PGRC_DISTACC_HASH").as_deref(),
        Ok("0") | Ok("off")
    )
}

/// One distinct eligible value in the hash arm: the precomputed 128-bit
/// identity, the exact count, and where its canonical bytes are (the
/// confirm + finalize-sort key): copied once, on first sight, into the
/// accumulator's [`Arena`], or — pinned (K1-MEM B,
/// [`DistAcc::pin_inputs`]) — left where the caller's input holds them,
/// so a near-unique text column is not held twice.
#[derive(Debug)]
struct Entry {
    h1: u64,
    h2: u64,
    count: u64,
    /// The address of the value's `len` bytes.
    at: usize,
    len: usize,
}

impl Entry {
    fn bytes(&self) -> &[u8] {
        // SAFETY: `at..at + len` is an arena copy (chunks never move and
        // live as long as the accumulator) or a pinned input the pinner
        // keeps valid and unchanged while the accumulator is used
        // ([`DistAcc::pin_inputs`]).
        unsafe { core::slice::from_raw_parts(self.at as *const u8, self.len) }
    }
}

/// The copied values' bytes: append-only chunks, each filled only up to
/// the capacity it was allocated with, so a value's address never moves.
/// Chunks grow geometrically (a column of a few distinct values keeps a
/// small one).
#[derive(Debug, Default)]
struct Arena {
    chunks: Vec<Vec<u8>>,
}

impl Arena {
    const FIRST: usize = 1 << 10;
    const MAX: usize = 64 << 10;

    /// Copy `v` in; its address.
    fn copy(&mut self, v: &[u8]) -> Option<usize> {
        if v.is_empty() {
            return Some(core::ptr::NonNull::<u8>::dangling().as_ptr() as usize);
        }
        let room = self.chunks.last().is_some_and(|c| c.capacity() - c.len() >= v.len());
        if !room {
            let next = self.chunks.last().map_or(Self::FIRST, |c| (c.capacity() * 2).min(Self::MAX));
            self.chunks.try_reserve(1).ok()?;
            self.chunks.push(pgrc_format::try_vec(next.max(v.len()), "sketch").ok()?);
        }
        let c = self.chunks.last_mut().expect("a chunk with room");
        let at = c.len();
        c.extend_from_slice(v);
        Some(c.as_ptr() as usize + at)
    }

    /// D4539 (trunk's charge) on the line's arena: its blocks, the chunks' capacities and the chunk list's own block.
    fn heap_bytes(&self) -> u64 {
        self.chunks.iter().map(|c| c.capacity() as u64).sum::<u64>() + cap_bytes!(self.chunks)
    }

    /// The bytes [`Arena::copy`] of a `len`-byte value allocates (0: it fits the last chunk, or is empty): the next
    /// chunk, and the chunk list's next block if that grows too. An estimate for a charge made before the allocation.
    fn next_block(&self, len: usize) -> u64 {
        if len == 0 || self.chunks.last().is_some_and(|c| c.capacity() - c.len() >= len) {
            return 0;
        }
        let next = self.chunks.last().map_or(Self::FIRST, |c| (c.capacity() * 2).min(Self::MAX)).max(len);
        next as u64 + next_block(self.chunks.len(), self.chunks.capacity(), 1, std::mem::size_of::<Vec<u8>>())
    }
}

#[derive(Debug)]
enum Acc {
    /// Default: open addressing over `meta_hash128`. `slots` holds
    /// index+1 into `entries` (0 = empty); power-of-two capacity, linear
    /// probing, grown below 3/4 load. Probe compares the stored 128-bit
    /// hash first; bytes are compared exactly once, on the full-hash
    /// match that adjudicates identity.
    Hash { slots: Vec<u32>, entries: Vec<Entry>, arena: Arena },
    /// `PGRUST_PGRC_DISTACC_HASH=0|off`: the original byte-keyed tree
    /// (kept verbatim as the A/B control / equivalence witness).
    BTree(BTreeMap<Vec<u8>, u64>),
    /// PGRC-WRITER-FAST `dist`: word-class streams (every canonical image
    /// ≤ 8 bytes) — the value lives INLINE as its zero-padded LE word + len
    /// (no per-distinct allocation, a two-word identity compare, a u128-key
    /// finalize sort). Same counts, same byte order: identical output.
    /// The entries live in the open-addressed table itself (count 0 = an empty slot; `n` = the occupied ones), so a
    /// probe touches one line, not a slot and then an entry (loadstats; finalize sorts by bytes, so the table order
    /// never reaches the output).
    Small { slots: Vec<SmallEntry>, n: usize },
}

/// One distinct value of the inline word arm (a table slot; `count == 0` = empty).
#[derive(Debug, Clone, Copy, Default)]
struct SmallEntry {
    word: u64,
    count: u64,
    /// The caller's grain stamp of the last [`DistAcc::observe_word_stamped`] that reached this entry (0 = none).
    stamp: u32,
    len: u8,
    /// The low 24 bits of the value's probe hash ([`small_hash`]): a grow places the entry without hashing again.
    hash: [u8; 3],
}

impl SmallEntry {
    #[inline(always)]
    fn new(word: u64, len: u8, stamp: u32, h: u32) -> SmallEntry {
        let b = h.to_le_bytes();
        SmallEntry { word, count: 1, stamp, len, hash: [b[0], b[1], b[2]] }
    }
}

/// Inline-arm tables at least this many slots (24 B each: 2048 slots = 48 KiB, about a core's L1d) take the batched
/// lane's prefetch; smaller ones are L1 resident.
const SMALL_PREFETCH_MIN_SLOTS: usize = 1 << 11;

/// The inline arm's slot: placed by the image itself, so a memo probe needs no value hash (the order of the table
/// never reaches the output: finalize sorts by bytes).
#[inline(always)]
fn small_slot(word: u64, len: u8, mask: usize) -> usize {
    crate::hash::slot_index(crate::hash::mix64(word ^ (u64::from(len) << 59)), mask)
}

/// The inline arm's probe hash: `small_slot(word, len, mask) == small_hash(word, len) as usize & mask` for every
/// mask below 2^32 (the slot is the hash's low bits).
#[inline(always)]
fn small_hash(word: u64, len: u8) -> u32 {
    small_slot(word, len, u32::MAX as usize) as u32
}

/// A hint that the cache line holding `p` will be read soon. Never faults and changes no state, so nothing waits on
/// it (a `black_box` read of the slot stalls the loop on the very miss it means to hide).
#[inline(always)]
fn prefetch_line<T>(p: *const T) {
    #[cfg(all(target_arch = "aarch64", not(miri)))]
    // SAFETY: `prfm` is a hint: it reads no register but its address operand, writes nothing, and cannot fault.
    unsafe {
        core::arch::asm!("prfm pldl1keep, [{0}]", in(reg) p, options(nostack, preserves_flags, readonly));
    }
    #[cfg(not(all(target_arch = "aarch64", not(miri))))]
    let _ = p;
}

/// The finalize sort key of a ≤ 8-byte image held as a zero-padded LE
/// word: big-endian read of the padded bytes, then length — exactly the
/// byte-lexicographic order (a shorter image that ties on the padded word
/// is a proper prefix of the longer, hence smaller).
#[inline(always)]
fn small_key(word: u64, len: u8) -> u128 {
    (u128::from(u64::from_be_bytes(word.to_le_bytes())) << 8) | u128::from(len)
}

/// The accumulating half (fed per nonnull value by `builder.rs`).
#[derive(Debug)]
pub struct DistAcc {
    /// D4539: the accumulator's own blocks on the meta builder's owner ([`DistAcc::charge_to`]), fitted at every grow
    /// BEFORE the new block is allocated (the old table is still live through the rehash), so a slot table doubling to
    /// tens of MiB inside one granule is on the owner from the start. Declared first, so dropped first (`kept.rs`).
    /// `Kept::none()`: uncharged.
    kept: Kept,
    acc: Acc,
    /// PGRC-WRITER-FAST `run`: entry index + 1 the last observe landed on
    /// (0 = none / a long value / the BTree arm).
    last: usize,
    long_values: u64,
    nonnull: u64,
    /// [`DistAcc::pin_inputs`]: inputs' values are kept in place.
    in_place: bool,
    /// A slot-table allocation failed: the accumulator stops tracking and finalizes to an empty distribution.
    failed: bool,
    /// The tree arm's key blocks (their capacities), for `heap_bytes`.
    tree_bytes: u64,
}

impl Default for DistAcc {
    fn default() -> DistAcc {
        DistAcc::with_hash_arm(hash_arm_on())
    }
}

impl DistAcc {
    /// Arm-explicit constructor (tests + the env default above).
    pub fn with_hash_arm(hash: bool) -> DistAcc {
        DistAcc {
            kept: Kept::none(),
            acc: if hash {
                Acc::Hash {
                    slots: Vec::new(),
                    entries: Vec::new(),
                    arena: Arena::default(),
                }
            } else {
                Acc::BTree(BTreeMap::new())
            },
            last: 0,
            long_values: 0,
            nonnull: 0,
            in_place: false,
            failed: false,
            tree_bytes: 0,
        }
    }

    /// K1-MEM B: from now on [`DistAcc::observe_hashed_input`] keeps each
    /// new distinct value's bytes where the input holds them instead of
    /// copying them (the hash arm; the BTree control arm copies). The
    /// distinct set of a stream is then its entries only (40 B each), not
    /// a second copy of the column.
    ///
    /// # Safety
    ///
    /// Every slice later passed to [`DistAcc::observe_hashed_input`] must
    /// stay valid and unchanged for as long as this accumulator is used
    /// (observed, finalized). Dropping it needs nothing.
    pub unsafe fn pin_inputs(&mut self) {
        self.in_place = true;
    }

    /// [`DistAcc::observe_hashed`] for a value that lies in the caller's
    /// input (an `EncodeInput` datum image): kept in place when the
    /// accumulator is pinned, copied otherwise.
    ///
    /// # Safety
    ///
    /// When the accumulator is pinned, `canonical` must meet
    /// [`DistAcc::pin_inputs`]' contract.
    pub unsafe fn observe_hashed_input(&mut self, h1: u64, h2: u64, canonical: &[u8]) {
        let in_place = self.in_place;
        self.insert(h1, h2, canonical, in_place);
    }

    /// PGRC-WRITER-FAST `run`: count one more occurrence of the value the
    /// previous observe landed on (the caller proved byte equality with
    /// it). False when there is no such entry — the caller observes
    /// normally.
    #[inline]
    pub fn repeat_last(&mut self) -> bool {
        if self.last == 0 {
            return false;
        }
        let i = self.last - 1;
        match &mut self.acc {
            Acc::Small { slots, .. } => slots[i].count += 1,
            Acc::Hash { entries, .. } => entries[i].count += 1,
            Acc::BTree(_) => return false,
        }
        self.nonnull += 1;
        true
    }

    /// [`DistAcc::repeat_last`] `n` times in one step: a run of `n` values byte-equal to the last observed one adds
    /// `n` to its count (one store, not a store-to-load chain per row). False exactly when `repeat_last` is.
    #[inline]
    pub fn repeat_last_n(&mut self, n: u64) -> bool {
        if self.last == 0 {
            return false;
        }
        let i = self.last - 1;
        match &mut self.acc {
            Acc::Small { slots, .. } => slots[i].count += n,
            Acc::Hash { entries, .. } => entries[i].count += n,
            Acc::BTree(_) => return false,
        }
        self.nonnull += n;
        true
    }

    /// PGRC-WRITER-FAST `dist`: the accumulator of a word-class stream —
    /// the inline word arm when the hash arm is selected, else
    /// [`DistAcc::default`] (the BTree control arm).
    pub fn for_words() -> DistAcc {
        let mut d = DistAcc::default();
        if matches!(d.acc, Acc::Hash { .. }) {
            d.acc = Acc::Small { slots: Vec::new(), n: 0 };
        }
        d
    }

    /// Observe one ≤ 8-byte canonical image given as its zero-padded LE
    /// word `wm` (bytes past `len` zero) + length — the word lane's face
    /// (PGRC-WRITER-FAST `word`). `(h1, h2)` MUST be `meta_hash128` of
    /// the image.
    #[inline]
    pub fn observe_hashed_word(&mut self, h1: u64, h2: u64, wm: u64, len: u8) {
        if self.failed {
            return;
        }
        let Acc::Small { slots, n } = &mut self.acc else {
            let b = wm.to_le_bytes();
            self.observe_hashed(h1, h2, &b[..len as usize]);
            return;
        };
        self.nonnull += 1;
        if len as usize > STATS_MCV_VALUE_MAX {
            self.long_values += 1;
            self.last = 0;
            return;
        }
        if (*n + 1) * 4 > slots.len() * 3 && !grow_small(slots, &mut self.kept) {
            self.failed = true;
            self.last = 0;
            return;
        }
        let mask = slots.len() - 1;
        let h = small_hash(wm, len);
        let mut i = h as usize & mask;
        loop {
            let e = &mut slots[i];
            if e.count == 0 {
                *e = SmallEntry::new(wm, len, 0, h);
                *n += 1;
                self.last = i + 1;
                return;
            }
            if e.word == wm && e.len == len {
                e.count += 1;
                self.last = i + 1;
                return;
            }
            i = (i + 1) & mask;
        }
    }

    /// The word lane's stamped face: count one occurrence of the `len`-byte image `wm` (zero-padded LE word) and
    /// say whether it is the first under `stamp` (nonzero; the caller's grain: HLL and bloom want each distinct value
    /// once per granule). `None` when the inline arm is not in use or the accumulator failed: the caller treats every
    /// value as a first.
    #[inline]
    pub fn observe_word_stamped(&mut self, wm: u64, len: u8, stamp: u32) -> Option<bool> {
        debug_assert!(stamp != 0 && (1..=8).contains(&len));
        if self.failed {
            return None;
        }
        let Acc::Small { slots, n } = &mut self.acc else { return None };
        self.nonnull += 1;
        if (*n + 1) * 4 > slots.len() * 3 && !grow_small(slots, &mut self.kept) {
            self.failed = true;
            self.last = 0;
            return None;
        }
        let mask = slots.len() - 1;
        let h = small_hash(wm, len);
        let mut i = h as usize & mask;
        loop {
            let e = &mut slots[i];
            if e.count == 0 {
                *e = SmallEntry::new(wm, len, stamp, h);
                *n += 1;
                self.last = i + 1;
                return Some(true);
            }
            if e.word == wm && e.len == len {
                e.count += 1;
                self.last = i + 1;
                let first = e.stamp != stamp;
                e.stamp = stamp;
                return Some(first);
            }
            i = (i + 1) & mask;
        }
    }

    /// The batched word lane's memo over one span: for each value `img(vals[i])` exactly what
    /// [`DistAcc::observe_word_stamped`] does (a run of repeats of the last value counted as
    /// [`DistAcc::repeat_last_n`] does), calling `first` for each value that is a first sight under `stamp`, with the
    /// table, its count, `nonnull` and `last` in registers for the whole span. Call only when
    /// [`DistAcc::word_memo_ready`]. `Err(i)`: the table could not grow at value `i` (counted in `nonnull`, the
    /// accumulator now failed, `first` not called for it): the caller treats `i` and every later value as first
    /// sights, as the per-value face would.
    #[inline]
    pub fn memo_words(
        &mut self,
        vals: &[u64],
        img: impl Fn(u64) -> u64,
        len: u8,
        stamp: u32,
        prefetch: bool,
        mut first: impl FnMut(u64),
    ) -> Result<(), usize> {
        debug_assert!(stamp != 0 && (1..=8).contains(&len) && self.word_memo_ready());
        let Acc::Small { slots, n: count } = &mut self.acc else { return Ok(()) };
        let (mut cnt, mut nonnull, mut last) = (*count, self.nonnull, self.last);
        let ahead = 2 * crate::batch::DIST_PREFETCH_AHEAD;
        let n = vals.len();
        let mut prev: Option<u64> = None;
        let mut i = 0;
        let mut out = Ok(());
        while i < n {
            let wm = img(vals[i]);
            if prefetch && i + ahead < n {
                prefetch_line(&slots[small_slot(img(vals[i + ahead]), len, slots.len() - 1)]);
            }
            if prev == Some(wm) && last != 0 {
                let mut j = i + 1;
                while j < n && img(vals[j]) == wm {
                    j += 1;
                }
                slots[last - 1].count += (j - i) as u64;
                nonnull += (j - i) as u64;
                i = j;
                continue;
            }
            prev = Some(wm);
            nonnull += 1;
            if (cnt + 1) * 4 > slots.len() * 3 && !grow_small(slots, &mut self.kept) {
                self.failed = true;
                last = 0;
                out = Err(i);
                break;
            }
            let mask = slots.len() - 1;
            let h = small_hash(wm, len);
            let mut k = h as usize & mask;
            loop {
                let e = &mut slots[k];
                if e.count == 0 {
                    *e = SmallEntry::new(wm, len, stamp, h);
                    cnt += 1;
                    last = k + 1;
                    first(wm);
                    break;
                }
                if e.word == wm && e.len == len {
                    e.count += 1;
                    last = k + 1;
                    if e.stamp != stamp {
                        e.stamp = stamp;
                        first(wm);
                    }
                    break;
                }
                k = (k + 1) & mask;
            }
            i += 1;
        }
        (*count, self.nonnull, self.last) = (cnt, nonnull, last);
        out
    }

    /// Whether the inline arm's table is large enough for [`DistAcc::prefetch_word_slot`] to pay.
    #[inline]
    pub fn word_prefetch_pays(&self) -> bool {
        matches!(&self.acc, Acc::Small { slots, .. } if slots.len() >= SMALL_PREFETCH_MIN_SLOTS)
    }

    /// The inline arm's prefetch: the slot `wm` probes first (the entry itself).
    #[inline]
    pub fn prefetch_word_slot(&self, wm: u64, len: u8) {
        if let Acc::Small { slots, .. } = &self.acc {
            if !slots.is_empty() {
                prefetch_line(&slots[small_slot(wm, len, slots.len() - 1)]);
            }
        }
    }

    /// The memo face is live: the inline word arm, not failed.
    #[inline]
    pub fn word_memo_ready(&self) -> bool {
        !self.failed && matches!(self.acc, Acc::Small { .. })
    }

    /// Observe one canonical value (the §18.1 bytes the hash lane already
    /// holds — no second canonicalization). Convenience face: hashes here.
    /// The builder's hot path uses [`DistAcc::observe_hashed`] with the
    /// `meta_hash128` it already computed for bloom/HLL.
    pub fn observe(&mut self, canonical: &[u8]) {
        let (h1, h2) = meta_hash128(canonical);
        self.observe_hashed(h1, h2, canonical);
    }

    /// Observe one canonical value whose `meta_hash128` is already in
    /// hand. `(h1, h2)` MUST be `meta_hash128(canonical)` — the builder's
    /// single hash site guarantees it; the compat face above recomputes.
    pub fn observe_hashed(&mut self, h1: u64, h2: u64, canonical: &[u8]) {
        self.insert(h1, h2, canonical, false);
    }

    /// The observation; `in_place` keeps a new value's bytes where they
    /// are (the caller vouches for them, [`DistAcc::pin_inputs`]).
    fn insert(&mut self, h1: u64, h2: u64, canonical: &[u8], in_place: bool) {
        if self.failed {
            return;
        }
        if let Acc::Small { .. } = &self.acc {
            if canonical.len() <= 8 {
                let mut t = [0u8; 8];
                t[..canonical.len()].copy_from_slice(canonical);
                self.observe_hashed_word(h1, h2, u64::from_le_bytes(t), canonical.len() as u8);
                return;
            }
            // A foreign (> 8-byte) image: leave the inline arm for the
            // general one (never on a word stream; kept total).
            self.small_to_hash();
            if self.failed {
                return;
            }
        }
        self.nonnull += 1;
        if canonical.len() > STATS_MCV_VALUE_MAX {
            self.long_values += 1;
            self.last = 0;
            return;
        }
        match &mut self.acc {
            Acc::BTree(counts) => {
                self.last = 0;
                // to_vec only on first sight of a value (the common path
                // is a hit).
                if let Some(c) = counts.get_mut(canonical) {
                    *c += 1;
                } else {
                    // D4539: the key's block and its entry on the charge before they are allocated.
                    let entry = std::mem::size_of::<(Vec<u8>, u64)>() as u64;
                    self.kept.fit(self.tree_bytes + canonical.len() as u64 + (counts.len() as u64 + 1) * entry);
                    let Ok(k) = pgrc_format::try_to_vec(canonical, "sketch") else {
                        self.kept.fit(self.tree_bytes + counts.len() as u64 * entry);
                        self.failed = true;
                        return;
                    };
                    self.tree_bytes += k.capacity() as u64;
                    counts.insert(k, 1);
                    self.kept.fit(self.tree_bytes + counts.len() as u64 * entry);
                }
            }
            Acc::Small { .. } => unreachable!("inline arm left above"),
            Acc::Hash { slots, entries, arena } => {
                // Grow below 3/4 load (also the lazy init: 0 slots grows
                // to the initial table before the first probe).
                if (entries.len() + 1) * 4 > slots.len() * 3
                    && !grow(slots, entries, &mut self.kept, cap_bytes!(entries) + arena.heap_bytes())
                {
                    self.failed = true;
                    self.last = 0;
                    return;
                }
                let mask = slots.len() - 1;
                let mut i = crate::hash::slot_index(h1, mask);
                loop {
                    let s = slots[i];
                    if s == 0 {
                        debug_assert!(entries.len() < u32::MAX as usize);
                        // D4539 (trunk) on the line's arena (K1-MEM B): a block the arena (unless the value is kept in
                        // place) or the entries grow to next is on the charge before it is allocated, and the charge is
                        // fitted to the blocks after.
                        let next = if in_place { 0 } else { arena.next_block(canonical.len()) }
                            + next_block(entries.len(), entries.capacity(), 1, std::mem::size_of::<Entry>());
                        let grows = next != 0;
                        if grows {
                            self.kept.fit(cap_bytes!(slots) + cap_bytes!(entries) + arena.heap_bytes() + next);
                        }
                        let at = if entries.try_reserve(1).is_err() {
                            None
                        } else if in_place {
                            Some(canonical.as_ptr() as usize)
                        } else {
                            arena.copy(canonical)
                        };
                        let Some(at) = at else {
                            self.kept.fit(cap_bytes!(slots) + cap_bytes!(entries) + arena.heap_bytes());
                            self.failed = true;
                            self.last = 0;
                            return;
                        };
                        entries.push(Entry {
                            h1,
                            h2,
                            count: 1,
                            at,
                            len: canonical.len(),
                        });
                        if grows {
                            self.kept.fit(cap_bytes!(slots) + cap_bytes!(entries) + arena.heap_bytes());
                        }
                        slots[i] = entries.len() as u32;
                        self.last = entries.len();
                        return;
                    }
                    let e = &mut entries[(s - 1) as usize];
                    if e.h1 == h1 && e.h2 == h2 && e.bytes() == canonical {
                        e.count += 1;
                        self.last = s as usize;
                        return;
                    }
                    i = (i + 1) & mask;
                }
            }
        }
    }

    /// D-STATS: prefetch the slot `h1` probes first — the batched hash lane
    /// runs this a few values ahead of the scalar [`DistAcc::observe_hashed`]
    /// inserts so the probe's cache line is in flight when the insert lands
    /// (a hint: no state change). Hash arm only, and only once the lazy
    /// table exists.
    #[inline]
    pub fn prefetch(&self, h1: u64) {
        if let Acc::Hash { slots, .. } = &self.acc {
            if !slots.is_empty() {
                prefetch_line(&slots[crate::hash::slot_index(h1, slots.len() - 1)]);
            }
        }
    }

    /// Stage 2 of [`DistAcc::prefetch`]: the entry the first-probed slot names.
    #[inline]
    pub fn prefetch_entry(&self, h1: u64) {
        if let Acc::Hash { slots, entries, .. } = &self.acc {
            if !slots.is_empty() {
                let s = slots[crate::hash::slot_index(h1, slots.len() - 1)];
                if s != 0 {
                    prefetch_line(&entries[(s - 1) as usize]);
                }
            }
        }
    }

    /// Stage 3: the stored bytes of that entry (the confirm's compare).
    #[inline]
    pub fn prefetch_bytes(&self, h1: u64) {
        if let Acc::Hash { slots, entries, .. } = &self.acc {
            if !slots.is_empty() {
                let s = slots[crate::hash::slot_index(h1, slots.len() - 1)];
                if s != 0 {
                    let e = &entries[(s - 1) as usize];
                    // The line's entries address their bytes directly (an arena chunk or a pinned input, K1-MEM B).
                    if e.h1 == h1 && e.len != 0 {
                        prefetch_line(e.at as *const u8);
                    }
                }
            }
        }
    }

    /// Migrate the inline arm to the general hash arm (same entries; the
    /// values are copied into the hash arm's arena).
    #[cold]
    fn small_to_hash(&mut self) {
        let Acc::Small { slots, .. } = &self.acc else { return };
        let mut arena = Arena::default();
        let es: Option<Vec<Entry>> = slots
            .iter()
            .filter(|e| e.count != 0)
            .map(|e| {
                let b = e.word.to_le_bytes();
                let v = &b[..e.len as usize];
                let (h1, h2) = meta_hash128(v);
                Some(Entry { h1, h2, count: e.count, at: arena.copy(v)?, len: v.len() })
            })
            .collect();
        let Some(es) = es else {
            self.failed = true;
            self.last = 0;
            return;
        };
        let mut slots = Vec::new();
        // The switch itself is fitted once it is done (never on a word stream: a foreign image only).
        let mut unfitted = Kept::none();
        let mut grown = true;
        while grown && (slots.is_empty() || (es.len() + 1) * 4 > slots.len() * 3) {
            grown = grow(&mut slots, &es, &mut unfitted, 0);
        }
        if !grown {
            self.failed = true;
            self.last = 0;
            return;
        }
        self.acc = Acc::Hash { slots, entries: es, arena };
        self.last = 0;
        let now = self.heap_bytes();
        self.kept.fit(now);
    }

    /// D4539: charge the accumulator's own blocks to `on` from now on (the meta builder's owner, `with_owner`): fitted
    /// to [`DistAcc::heap_bytes`] now and at every grow before the new block is allocated. `None`: uncharged.
    pub fn charge_to(&mut self, on: On) {
        self.kept = Kept::new(on);
        let now = self.heap_bytes();
        self.kept.fit(now);
    }

    /// The bytes the accumulator's own charge holds (0 uncharged).
    pub fn charged(&self) -> u64 {
        self.kept.charged()
    }

    /// The heap bytes a keeper's charge must still cover: all of [`DistAcc::heap_bytes`] when the accumulator has no
    /// charge of its own, none when it does ([`DistAcc::charge_to`]).
    pub fn unkept_bytes(&self) -> u64 {
        if self.kept.owner().is_some() { 0 } else { self.heap_bytes() }
    }

    /// The accumulator's heap bytes (capacities): the hash arm's slots, entries and value arena, the inline arm's table.
    /// What the meta builder's charge follows (`kept.rs`). The byte-keyed tree arm (`PGRUST_PGRC_DISTACC_HASH=0`, the
    /// A/B control, off by default) counts its keys' blocks and one entry each, not the tree's own nodes.
    pub fn heap_bytes(&self) -> u64 {
        match &self.acc {
            Acc::Hash { slots, entries, arena } => cap_bytes!(slots) + cap_bytes!(entries) + arena.heap_bytes(),
            Acc::Small { slots, .. } => cap_bytes!(slots),
            Acc::BTree(counts) => self.tree_bytes + counts.len() as u64 * std::mem::size_of::<(Vec<u8>, u64)>() as u64,
        }
    }

    /// A slot-table allocation failed: the sketch is incomplete and must not be written.
    pub fn failed(&self) -> bool {
        self.failed
    }

    /// pgrc format 2.1 §2.2: the EXACT part NDV when the distinct set is complete
    /// (no long-value residue — values over the eligibility bound are
    /// tallied in `long_values`, never deduped, so any residue makes the
    /// exact count unknowable). O(1).
    pub fn distinct_exact(&self) -> Option<u64> {
        if self.failed || self.long_values != 0 {
            return None;
        }
        Some(match &self.acc {
            Acc::BTree(counts) => counts.len() as u64,
            Acc::Hash { entries, .. } => entries.len() as u64,
            Acc::Small { n, .. } => *n as u64,
        })
    }

    /// Finalize into the sidecar's [`ColDistribution`]: MCV top-k
    /// (count-descending, byte-order tiebreak) + equi-depth bounds in byte
    /// order ([`Self::finalize_in`] with [`HistOrder::Bytes`]).
    pub fn finalize(&self) -> ColDistribution {
        self.finalize_in(HistOrder::Bytes)
    }

    /// [`Self::finalize`] with the histogram in `order` (the column's, [`HistOrder::of`]). Both arms converge on the
    /// same finish path ([`finish_distribution`] — shared with the DICT-DEDUP feed).
    pub fn finalize_in(&self, order: HistOrder) -> ColDistribution {
        self.finalize_in_for(order, None).unwrap_or_else(|_| missing(self.nonnull))
    }

    /// [`DistAcc::finalize`] whose working vectors (the byte order's keys, images and sorted views) are charged to `on`
    /// while they live (wave 3 A3: the meta builder's owner). The output is the caller's.
    pub fn finalize_for(&self, on: On) -> FormatResult<ColDistribution> {
        self.finalize_in_for(HistOrder::Bytes, on)
    }

    /// [`Self::finalize_in`] (the line's FOLDSTATS order) with trunk's wave 3 A3 charging: the distinct set's working
    /// vectors are charged to `on` while they live. The output is the caller's.
    pub fn finalize_in_for(&self, order: HistOrder, on: On) -> FormatResult<ColDistribution> {
        if self.failed {
            return Ok(missing(self.nonnull));
        }
        // The distinct set: the BTreeMap iterates in byte order (the
        // reference arm); the hash arm's entries are in first-seen order.
        match &self.acc {
            Acc::BTree(counts) => {
                let set = collect_on(on, counts.iter().map(|(k, &v)| (k.as_slice(), v)))?;
                finish_distribution(&set, true, self.nonnull, self.long_values, order, on)
            }
            Acc::Hash { entries, .. } => {
                let set = collect_on(on, entries.iter().map(|e| (e.bytes(), e.count)))?;
                finish_distribution(&set, false, self.nonnull, self.long_values, order, on)
            }
            Acc::Small { slots, n } => {
                // The table's occupied slots (count 0 = empty; loadstats in-table entries).
                let entries = collect_on_n(on, *n, slots.iter().copied().filter(|e| e.count != 0))?;
                let entries = &entries[..];
                // The inline arm's values as images; in byte order when the
                // histogram is byte-ordered (an LSD radix over the words when
                // every image has one length, [`small_byte_order`]; else one
                // u128-key sort, PGRC-WRITER-FAST `dist`), else first-seen (a
                // word order keys and sorts them in `equi_depth_bounds`).
                let byte_ordered = order == HistOrder::Bytes;
                let radix = if byte_ordered { small_byte_order(entries, on) } else { None };
                let imgs: ChargedVec<([u8; 8], u8, u64)> = match radix {
                    Some(ix) => collect_on(
                        on,
                        ix.iter().map(|&i| {
                            let e = &entries[i as usize];
                            (e.word.to_le_bytes(), e.len, e.count)
                        }),
                    )?,
                    None => {
                        let mut st =
                            collect_on(on, entries.iter().map(|e| (small_key(e.word, e.len), e.word.to_le_bytes(), e.len, e.count)))?;
                        if byte_ordered {
                            st.sort_unstable_by_key(|t| t.0);
                        }
                        collect_on(on, st.iter().map(|t| (t.1, t.2, t.3)))?
                    }
                };
                let set = collect_on(on, imgs.iter().map(|t| (&t.0[..t.1 as usize], t.2)))?;
                finish_distribution(&set, byte_ordered, self.nonnull, self.long_values, order, on)
            }
        }
    }
}

/// The ONE finish path: MCV top-k (count-descending, byte-order
/// tiebreak) + equi-depth histogram bounds in `order` (FOLDSTATS: the
/// column's value order) over `set` — the ELIGIBLE (len ≤
/// [`STATS_MCV_VALUE_MAX`]) distinct set with exact counts, in any order
/// (`byte_ordered`: strictly byte-ascending). Shared by [`DistAcc::finalize`] and
/// [`distribution_from_sorted_counts`] so the accumulator arm and the
/// DICT-DEDUP feed arm cannot drift: sidecar bytes are this function's
/// output on both, a pure function of the (set, counts, nonnull,
/// long_values) facts. The MCV selection runs on integers alone when
/// `set` is byte-ordered ([`mcv_top_k`]: an entry's index is its byte
/// rank), else by [`top_counts`]; both choose the same top-k.
fn finish_distribution(
    set: &[(&[u8], u64)],
    byte_ordered: bool,
    nonnull: u64,
    long_values: u64,
    order: HistOrder,
    on: On,
) -> FormatResult<ColDistribution> {
    debug_assert!(
        !byte_ordered || set.windows(2).all(|w| w[0].0 < w[1].0),
        "a byte-ordered distinct set that is not"
    );
    let eligible_total: u64 = set.iter().map(|&(_, c)| c).sum();
    Ok(ColDistribution {
        nonnull,
        ndv_eligible: set.len() as u64,
        long_values,
        mcv: if byte_ordered { mcv_top_k(set, on)? } else { top_counts(set)? },
        hist_bounds: equi_depth_bounds(set, byte_ordered, eligible_total, order, on)?,
    })
}

fn missing(nonnull: u64) -> ColDistribution {
    ColDistribution { nonnull, long_values: nonnull, ..ColDistribution::default() }
}

pub(crate) fn or_skipped<T>(r: FormatResult<T>) -> Option<T> {
    let bytes = match &r {
        Err(FormatError::OutOfMemory { bytes }) => *bytes,
        _ => 0,
    };
    r.map_err(|_| pgrc_format::note_accel_skipped(pgrc_format::Accel::DistStats, bytes)).ok()
}

/// The MCV top-k: the first [`STATS_MCV_K`] of `set` in (count desc, bytes
/// asc), a total order on distinct values, in that order. Found without
/// sorting the set: the k-th largest count first, then the byte-smallest
/// of the values tied at it (for a unique column, every value).
fn top_counts(set: &[(&[u8], u64)]) -> FormatResult<Vec<(Vec<u8>, u64)>> {
    let k = STATS_MCV_K;
    let mut top: Vec<(&[u8], u64)> = if set.len() <= k {
        pgrc_format::try_to_vec(set, "sketch")?
    } else {
        let mut counts: BinaryHeap<Reverse<u64>> = set[..k].iter().map(|&(_, c)| Reverse(c)).collect();
        for &(_, c) in &set[k..] {
            if counts.peek().is_some_and(|m| c > m.0) {
                counts.pop();
                counts.push(Reverse(c));
            }
        }
        let cut = counts.peek().map_or(0, |m| m.0);
        let mut top: Vec<(&[u8], u64)> = set.iter().filter(|e| e.1 > cut).copied().collect();
        let room = k - top.len();
        let mut tied: BinaryHeap<&[u8]> = BinaryHeap::with_capacity(STATS_MCV_K + 1);
        for &(v, c) in set {
            if c != cut {
                continue;
            }
            if tied.len() < room {
                tied.push(v);
            } else if tied.peek().is_some_and(|&m| v < m) {
                tied.pop();
                tied.push(v);
            }
        }
        top.extend(tied.into_iter().map(|v| (v, cut)));
        top
    };
    top.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let mut out = pgrc_format::try_vec(top.len(), "sketch")?;
    for (v, c) in top {
        out.push((pgrc_format::try_to_vec(v, "sketch")?, c));
    }
    Ok(out)
}

/// Equi-depth bounds over `set` in `order`: bound i is the value at
/// cumulative rank i * (total - 1) / (nbounds - 1), bound 0 the min, the
/// last bound the max. FOLDSTATS: a value repeats when it spans several
/// ranks, so every bound stands for the same share of the part's eligible
/// rows (the fold weighs parts by it). A word order sorts decoded keys (a
/// radix sort; none when they arrive ascending), and a unique set reads
/// its bounds at their ranks.
fn equi_depth_bounds(set: &[(&[u8], u64)], byte_ordered: bool, total: u64, order: HistOrder, on: On) -> FormatResult<Vec<Vec<u8>>> {
    if total == 0 {
        return Ok(Vec::new());
    }
    let nb = STATS_HIST_BOUNDS as u64;
    let mut targets: ChargedVec<u64> = charged_vec(on);
    targets.with_vec(|v| v.extend((0..nb).map(|i| (i * (total - 1)) / (nb - 1)))); // alloc: Exec charged on
    targets.with_vec(|v| v.dedup());
    // the values at the target ranks of an ascending walk
    fn walk<'a>(ordered: impl Iterator<Item = (&'a [u8], u64)>, targets: &[u64]) -> FormatResult<Vec<Vec<u8>>> {
        let mut out: Vec<Vec<u8>> = pgrc_format::try_vec(targets.len(), "sketch")?;
        let (mut cum, mut ti) = (0u64, 0usize);
        for (k, c) in ordered {
            let lo = cum;
            cum += c;
            while ti < targets.len() && targets[ti] >= lo && targets[ti] < cum {
                out.push(pgrc_format::try_to_vec(k, "sketch")?);
                ti += 1;
            }
            if ti >= targets.len() {
                break;
            }
        }
        Ok(out)
    }
    if order == HistOrder::Bytes && byte_ordered {
        return walk(set.iter().copied(), &targets);
    }
    // D4594 / D4416: the decoded keys on `on`, their block charged as it is allocated (one reserve, then fills)
    let keyed: Option<ChargedVec<(u64, u32)>> = if order == HistOrder::Bytes || u32::try_from(set.len()).is_err() {
        None
    } else {
        let mut k = charged_vec(on);
        k.try_reserve(set.len()).map_err(|_| oom::<(u64, u32)>(set.len()))?;
        let all = k.with_vec(|w| {
            for (i, &(v, _)) in set.iter().enumerate() {
                match order.key(v) {
                    Some(key) => w.push((key, i as u32)), // alloc: Exec charged on
                    None => return false,
                }
            }
            true
        });
        all.then_some(k)
    };
    let Some(mut keyed) = keyed else {
        let mut v = collect_on(on, set.iter().copied())?;
        if order == HistOrder::Bytes {
            sort_bytes_prefixed_for(&mut v, |t| t.0, on);
        } else {
            v.sort_unstable_by(|a, b| order.cmp(a.0, b.0));
        }
        return walk(v.iter().copied(), &targets);
    };
    if !keyed.as_slice().windows(2).all(|w| w[0].0 < w[1].0) {
        radix_sort_keys(&mut keyed, on);
        // equal keys (±0, NaN payloads) in byte order: HistOrder::cmp's total order
        let keyed = keyed.as_mut_slice();
        let mut i = 0;
        while i < keyed.len() {
            let j = i + keyed[i..].iter().take_while(|e| e.0 == keyed[i].0).count();
            if j - i > 1 {
                keyed[i..j].sort_unstable_by(|a, b| set[a.1 as usize].0.cmp(set[b.1 as usize].0));
            }
            i = j;
        }
    }
    let keyed = keyed.as_slice();
    if total == set.len() as u64 {
        // every count is 1: rank r is the r-th value
        let mut out = pgrc_format::try_vec(targets.len(), "sketch")?;
        for &t in targets.iter() {
            out.push(pgrc_format::try_to_vec(set[keyed[t as usize].1 as usize].0, "sketch")?);
        }
        return Ok(out);
    }
    walk(keyed.iter().map(|&(_, i)| set[i as usize]), &targets)
}

/// `v` ascending by key, stable (equal keys keep their order): LSD radix on
/// the key's bytes, skipping a byte every key shares.
fn radix_sort_keys(v: &mut ChargedVec<(u64, u32)>, on: On) {
    let n = v.len();
    if n < 1024 {
        v.as_mut_slice().sort_by_key(|e| e.0);
        return;
    }
    let mut hist = [[0usize; 256]; 8];
    for &(k, _) in v.as_slice() {
        for (d, h) in hist.iter_mut().enumerate() {
            h[((k >> (8 * d)) & 0xFF) as usize] += 1;
        }
    }
    // D4594 / D4416: the scatter buffer on `on` (a failed reserve leaves the keys in their order: the stable sort below)
    let mut buf: ChargedVec<(u64, u32)> = charged_vec(on);
    let bytes = n * std::mem::size_of::<(u64, u32)>();
    if buf.resize(n, (0, 0)).map_err(|_| pgrc_format::note_accel_skipped(pgrc_format::Accel::SortOrder, bytes)).is_err() {
        v.as_mut_slice().sort_by_key(|e| e.0);
        return;
    }
    for (d, h) in hist.iter().enumerate() {
        if h.iter().any(|&c| c == n) {
            continue;
        }
        let mut at = [0usize; 256];
        let mut sum = 0;
        for (slot, &c) in at.iter_mut().zip(h.iter()) {
            *slot = sum;
            sum += c;
        }
        let out = buf.as_mut_slice();
        for &e in v.as_slice() {
            let b = ((e.0 >> (8 * d)) & 0xFF) as usize;
            out[at[b]] = e;
            at[b] += 1;
        }
        std::mem::swap(v, &mut buf);
    }
}

/// MCV top-k of a byte-ascending distinct set: the K first entries under
/// (count desc, bytes asc), in that order. `ordered` is strictly
/// byte-ascending, so an entry's INDEX is its byte rank and the selection
/// runs on integers alone (pcopy stats 1b): the K-th largest count by an
/// integer select, every entry above it, then the lowest-index entries equal
/// to it — the exact set (and order) the byte-comparing selection chose,
/// without the byte compare it paid on every count tie, i.e. on every
/// comparison of a near-unique column.
fn mcv_top_k(ordered: &[(&[u8], u64)], on: On) -> FormatResult<Vec<(Vec<u8>, u64)>> {
    let k = STATS_MCV_K.min(ordered.len());
    if k == 0 {
        return Ok(Vec::new());
    }
    let kth = if ordered.len() > STATS_MCV_K {
        let mut counts = collect_on(on, ordered.iter().map(|t| t.1))?;
        *counts.select_nth_unstable_by(STATS_MCV_K - 1, |a, b| b.cmp(a)).1
    } else {
        0 // every count is >= 1: all entries are above
    };
    let above = ordered.iter().filter(|t| t.1 > kth).count();
    let mut equal_left = k - above;
    let mut pick: Vec<(u64, u32)> = pgrc_format::try_vec(k, "sketch")?;
    for (i, t) in ordered.iter().enumerate() {
        let take = if t.1 > kth {
            true
        } else if t.1 == kth && equal_left > 0 {
            equal_left -= 1;
            true
        } else {
            false
        };
        if take {
            pick.push((t.1, i as u32));
        }
    }
    pick.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut out = pgrc_format::try_vec(pick.len(), "sketch")?;
    for &(c, i) in &pick {
        out.push((pgrc_format::try_to_vec(ordered[i as usize].0, "sketch")?, c));
    }
    Ok(out)
}

/// The inline word arm's byte order when every entry has the same image
/// length (a word stream always does): the images are the zero-padded LE
/// words, so byte order is the order of their big-endian reads, sorted
/// here by an LSD radix over the bytes that vary (pcopy stats 1b; a
/// comparison sort over a u128 key before). `None` = mixed lengths (the
/// caller keeps the comparison sort).
fn small_byte_order(entries: &[SmallEntry], on: On) -> Option<ChargedVec<u32>> {
    let first = entries.first()?;
    if entries.iter().any(|e| e.len != first.len) {
        return None;
    }
    let mut keyed = collect_on(None, entries.iter().enumerate().map(|(i, e)| (u64::from_be_bytes(e.word.to_le_bytes()), i as u32))).ok()?;
    radix_by_key(&mut keyed, None);
    collect_on(on, keyed.iter().map(|t| t.1)).ok()
}

/// Sort `(key, payload)` pairs with DISTINCT keys ascending by key: an LSD radix over the key bytes that vary (every
/// byte's histogram in one pass; a byte with one bucket takes no pass), a comparison sort under 1024 pairs or when
/// the radix buffer cannot be had. Distinct keys make the order unique, so every path gives the same output.
/// The radix buffer is charged to `on` while it lives (it swaps with `keyed`, charge and block together).
fn radix_by_key<T: Copy + Default>(keyed: &mut ChargedVec<(u64, T)>, on: On) {
    if keyed.len() < 1024 {
        keyed.sort_unstable_by_key(|t| t.0);
        return;
    }
    let mut hist = [[0u32; 256]; 8];
    for t in keyed.iter() {
        for (b, h) in hist.iter_mut().enumerate() {
            h[((t.0 >> (8 * b)) & 0xff) as usize] += 1;
        }
    }
    let mut tmp: ChargedVec<(u64, T)> = charged_vec(on);
    let filled = tmp.with_vec(|t| match pgrc_format::try_filled::<(u64, T)>(keyed.len(), (0, T::default()), "sketch") { // alloc: Exec charged on
        Ok(b) => {
            *t = b;
            true
        }
        Err(_) => false,
    });
    if !filled {
        pgrc_format::note_accel_skipped(pgrc_format::Accel::SortOrder, keyed.len() * std::mem::size_of::<(u64, T)>());
        keyed.sort_unstable_by_key(|t| t.0);
        return;
    }
    for byte in 0..8u32 {
        let shift = 8 * byte;
        let h = &hist[byte as usize];
        if h.iter().any(|&c| c as usize == keyed.len()) {
            continue;
        }
        let mut count = [0usize; 256];
        let mut at = 0usize;
        for (c, &n) in count.iter_mut().zip(h.iter()) {
            *c = at;
            at += n as usize;
        }
        for &t in keyed.iter() {
            let d = ((t.0 >> shift) & 0xff) as usize;
            tmp[count[d]] = t;
            count[d] += 1;
        }
        std::mem::swap(keyed, &mut tmp);
    }
}

/// DICT-DEDUP: [`ColDistribution`] from an externally maintained counted
/// distinct set — the dict election's byte-rank-sorted entry list with
/// exact per-part counts (one structure, two consumers; module doc).
///
/// Contract (the dict build guarantees both by construction — its strict
/// byte-rank sort is contractual and mechanically re-proven at emit):
/// `entries` is strictly byte-ascending (distinct), and each count is the
/// value's exact non-null occurrence count over the part, so Σ counts IS
/// the part's nonnull row count (the dict observes every non-null row —
/// oversize values demote the dict arm before this face can be reached).
///
/// The [`STATS_MCV_VALUE_MAX`] eligibility gate the accumulator applies at
/// OBSERVE grain is applied here at ENTRY grain — the same partition of
/// the same multiset (eligibility is a pure function of value length), so
/// the output equals [`DistAcc::finalize`] over the expanded value
/// sequence byte-for-byte (pinned by the equivalence battery below).
pub fn distribution_from_sorted_counts(entries: &[(Vec<u8>, u64)]) -> ColDistribution {
    distribution_from_sorted_counts_in(entries, HistOrder::Bytes)
}

/// [`distribution_from_sorted_counts`] with the histogram in `order` (the column's, [`HistOrder::of`]).
pub fn distribution_from_sorted_counts_in(entries: &[(Vec<u8>, u64)], order: HistOrder) -> ColDistribution {
    distribution_from_sorted_counts_in_for(entries, order, None).unwrap_or_else(|_| missing(entries.iter().map(|(_, c)| c).sum()))
}

/// [`distribution_from_sorted_counts`] whose working vectors are charged to `on` while they live (wave 3 A3).
pub fn distribution_from_sorted_counts_for(entries: &[(Vec<u8>, u64)], on: On) -> FormatResult<ColDistribution> {
    distribution_from_sorted_counts_in_for(entries, HistOrder::Bytes, on)
}

/// The line's histogram order (FOLDSTATS) and trunk's charging (wave 3 A3) in one face: the meta builder's feed arm.
pub fn distribution_from_sorted_counts_in_for(entries: &[(Vec<u8>, u64)], order: HistOrder, on: On) -> FormatResult<ColDistribution> {
    let nonnull: u64 = entries.iter().map(|(_, c)| c).sum();
    let mut long_values = 0u64;
    let mut ordered: ChargedVec<(&[u8], u64)> = charged_vec(on);
    let exact = ordered.with_vec(|o| o.try_reserve_exact(entries.len())); // alloc: Exec charged on
    exact.map_err(|_| oom::<(&[u8], u64)>(entries.len()))?;
    ordered.with_vec(|ordered| {
        for (val, count) in entries {
            if val.len() > STATS_MCV_VALUE_MAX {
                long_values += count;
            } else {
                ordered.push((val.as_slice(), *count)); // alloc: Exec charged on
            }
        }
    });
    finish_distribution(&ordered, true, nonnull, long_values, order, on)
}

/// Sort by byte-lexicographic order, most significant 16 bytes first (PGRC-WRITER-FAST `sort`; a one-key sort with
/// a full-compare tie break before loadstats, which rescanned a long shared prefix (URLs, titles) on every tie).
/// Each round sorts a run by the 16 bytes at its depth, zero padded, plus the bytes left there capped at 17: equal
/// padded chunks with 16 or fewer bytes left mean the shorter value is a prefix of the longer, so the shorter sorts
/// first, and only a run whose values all hold more than 16 bytes past the chunk goes one chunk deeper. That is the
/// same total order as a plain byte sort; the runs are an explicit stack (no recursion on a long shared prefix).
pub fn sort_bytes_prefixed<T>(v: &mut [T], bytes: impl Fn(&T) -> &[u8]) {
    sort_bytes_prefixed_for(v, bytes, None)
}

/// [`sort_bytes_prefixed`] whose working vectors (the keys, the run stack, the permutation) are charged to `on` while
/// they live (wave 3 A3: a caller's owner).
pub fn sort_bytes_prefixed_for<T>(v: &mut [T], bytes: impl Fn(&T) -> &[u8], on: On) {
    let key_at = |b: &[u8], off: usize| -> (u64, u64, u8) {
        let rest = b.get(off..).unwrap_or(&[]);
        let mut t = [0u8; 16];
        let n = rest.len().min(16);
        t[..n].copy_from_slice(&rest[..n]);
        let (hi, lo) = t.split_at(8);
        (
            u64::from_be_bytes(hi.try_into().expect("8")),
            u64::from_be_bytes(lo.try_into().expect("8")),
            rest.len().min(17) as u8,
        )
    };
    let Ok(mut keyed) = collect_on(on, v.iter().enumerate().map(|(i, t)| (key_at(bytes(t), 0), i))) else {
        return plain_byte_sort(v, bytes);
    };
    let mut runs: ChargedVec<(usize, usize, usize)> = charged_vec(on);
    let first = (0, keyed.len(), 0);
    runs.with_vec(|r| r.push(first)); // alloc: Exec charged on
    while let Some((lo, hi, off)) = runs.with_vec(|r| r.pop()) {
        let run = &mut keyed[lo..hi];
        if off > 0 {
            for e in run.iter_mut() {
                e.0 = key_at(bytes(&v[e.1]), off);
            }
        }
        run.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let mut i = 0;
        while i < run.len() {
            let mut j = i + 1;
            while j < run.len() && run[j].0 == run[i].0 {
                j += 1;
            }
            if j - i > 1 && run[i].0 .2 == 17 {
                runs.with_vec(|r| r.push((lo + i, lo + j, off + 16))); // alloc: Exec charged on
            }
            i = j;
        }
    }
    // Apply the permutation (cycle walk over the index map).
    let Ok(mut perm) = collect_on(on, keyed.iter().map(|t| t.1)) else {
        return plain_byte_sort(v, bytes);
    };
    drop(keyed);
    for i in 0..perm.len() {
        let mut cur = i;
        while perm[cur] != i {
            let next = perm[cur];
            v.swap(cur, next);
            perm[cur] = cur;
            cur = next;
        }
        perm[cur] = cur;
    }
}

fn plain_byte_sort<T>(v: &mut [T], bytes: impl Fn(&T) -> &[u8]) {
    pgrc_format::note_accel_skipped(pgrc_format::Accel::SortOrder, v.len() * std::mem::size_of::<((u64, u64, u8), usize)>());
    v.sort_unstable_by(|a, b| bytes(a).cmp(bytes(b)));
}

/// Grow (or lazily create) the inline arm's slot table (rehash by the
/// stored image).
///
/// D4539: the inline arm's heap IS its table, so `kept` is fitted to the old table plus the new one before the new one
/// is allocated (both live through the rehash), and to the new one after.
fn grow_small(slots: &mut Vec<SmallEntry>, kept: &mut Kept) -> bool {
    let ncap = if slots.is_empty() { 16 } else { slots.len() * 2 };
    let mask = ncap - 1;
    kept.fit(cap_bytes!(slots) + (ncap as u64).saturating_mul(std::mem::size_of::<SmallEntry>() as u64));
    let Ok(mut ns) = pgrc_format::try_filled(ncap, SmallEntry::default(), "sketch") else {
        kept.fit(cap_bytes!(slots));
        return false;
    };
    // The stored 24 hash bits place the entry while the mask fits in them (2^24 slots), else hash again.
    let stored = mask <= 0xff_ffff;
    for e in slots.iter().filter(|e| e.count != 0) {
        let mut i = if stored {
            u32::from_le_bytes([e.hash[0], e.hash[1], e.hash[2], 0]) as usize & mask
        } else {
            small_slot(e.word, e.len, mask)
        };
        while ns[i].count != 0 {
            i = (i + 1) & mask;
        }
        ns[i] = *e;
    }
    *slots = ns;
    kept.fit(cap_bytes!(slots));
    true
}

/// Grow (or lazily create) the slot table and rehash by the STORED h1 —
/// no byte access on the rehash path.
///
/// D4539: `kept` is fitted to the old table plus the new one plus `rest` (the arm's other blocks) before the new one is
/// allocated, and to the new one plus `rest` after.
fn grow(slots: &mut Vec<u32>, entries: &[Entry], kept: &mut Kept, rest: u64) -> bool {
    let ncap = if slots.is_empty() { 16 } else { slots.len() * 2 };
    let mask = ncap - 1;
    kept.fit(cap_bytes!(slots) + (ncap as u64).saturating_mul(std::mem::size_of::<u32>() as u64) + rest);
    let Ok(mut ns) = pgrc_format::try_filled(ncap, 0u32, "sketch") else {
        kept.fit(cap_bytes!(slots) + rest);
        return false;
    };
    for (idx, e) in entries.iter().enumerate() {
        let mut i = crate::hash::slot_index(e.h1, mask);
        while ns[i] != 0 {
            i = (i + 1) & mask;
        }
        ns[i] = idx as u32 + 1;
    }
    *slots = ns;
    kept.fit(cap_bytes!(slots) + rest);
    true
}

/// The bytes of the block a buffer of `len` elements of `size` bytes in `cap` grows to when `more` are added: 0 when
/// they fit, else the standard library's amortized next capacity (double, at least what is needed, at least 8 bytes /
/// 4 elements). An estimate for a charge made before the allocation: the fit after it uses the real capacity.
fn next_block(len: usize, cap: usize, more: usize, size: usize) -> u64 {
    if cap - len >= more {
        return 0;
    }
    let min = if size == 1 { 8 } else { 4 };
    let ncap = (cap.saturating_mul(2)).max(len + more).max(min);
    (ncap as u64).saturating_mul(size as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stored probe hash places an entry where `small_slot` would, at every table size a grow uses it for.
    #[test]
    fn small_hash_low_bits_are_the_slot() {
        let mut x = 0x0bad_5eed_u64;
        for _ in 0..20_000 {
            x = crate::hash::mix64(x);
            let len = 1 + (x % 8) as u8;
            let h = small_hash(x, len);
            let e = SmallEntry::new(x, len, 1, h);
            let stored = u32::from_le_bytes([e.hash[0], e.hash[1], e.hash[2], 0]) as usize;
            for bits in [4u32, 10, 17, 24] {
                let mask = (1usize << bits) - 1;
                assert_eq!(stored & mask, small_slot(x, len, mask));
            }
            assert_eq!(h as usize & ((1usize << 31) - 1), small_slot(x, len, (1usize << 31) - 1));
        }
        assert_eq!(std::mem::size_of::<SmallEntry>(), 24);
    }

    /// PGRC-WRITER-FAST: the inline word arm, the prefixed sort and the MCV
    /// selection produce the hash arm's exact distribution.
    #[test]
    fn small_arm_and_sorts_equal_the_hash_arm() {
        let mut x: u64 = 0x1357_9bdf_2468_ace0;
        for width in [1usize, 2, 4, 8] {
            let mut a = DistAcc::with_hash_arm(true);
            let mut b = DistAcc::with_hash_arm(true);
            b.acc = Acc::Small { slots: Vec::new(), n: 0 };
            for i in 0..20_000u64 {
                x = crate::hash::mix64(x);
                // skewed: many repeats of a few values + a long tail
                let w = if i % 3 == 0 { x % 7 } else { x };
                let img = w.to_le_bytes();
                let v = &img[..width];
                let (h1, h2) = meta_hash128(v);
                a.observe_hashed(h1, h2, v);
                let mask = if width == 8 { u64::MAX } else { (1u64 << (8 * width)) - 1 };
                if i % 2 == 0 {
                    b.observe_hashed_word(h1, h2, w & mask, width as u8);
                } else {
                    b.observe_hashed(h1, h2, v);
                }
            }
            assert_eq!(a.finalize(), b.finalize(), "width {width}");
        }
        // A foreign > 8-byte image moves the inline arm to the hash arm
        // with the same entries.
        let mut a = DistAcc::with_hash_arm(true);
        let mut b = DistAcc::for_words();
        for v in [&b"ab"[..], b"ab", b"abcdefgh", b"a", b"abcdefghij", b"ab"] {
            let (h1, h2) = meta_hash128(v);
            a.observe_hashed(h1, h2, v);
            b.observe_hashed(h1, h2, v);
        }
        assert_eq!(a.finalize(), b.finalize(), "small -> hash migration");
        // prefixed sort == plain sort on mixed lengths with shared prefixes
        let mut v: Vec<Vec<u8>> = Vec::new();
        for i in 0..3000u64 {
            x = crate::hash::mix64(x);
            let len = (x % 20) as usize;
            let mut b: Vec<u8> = (0..len).map(|k| ((x >> (k % 8 * 8)) % 3) as u8).collect();
            if i % 5 == 0 {
                b.push(0);
            }
            v.push(b);
        }
        v.sort();
        v.dedup();
        let mut w = v.clone();
        w.reverse();
        let mut r: Vec<(&[u8], u64)> = w.iter().map(|b| (b.as_slice(), 1)).collect();
        sort_bytes_prefixed(&mut r, |t| t.0);
        let want: Vec<&[u8]> = v.iter().map(|b| b.as_slice()).collect();
        let got: Vec<&[u8]> = r.iter().map(|t| t.0).collect();
        assert_eq!(want, got);
    }

    /// PGRC-WRITER-FAST `bank`: the four-chain pass the batched hash lane
    /// runs is `meta_hash128` plus the O-10 row digests, lane for lane.
    #[test]
    fn four_chain_pass_equals_meta_hash_and_bank_digests() {
        let mut x: u64 = 0x2468_ace0_1357_9bdf;
        for len in 0..90usize {
            x = crate::hash::mix64(x);
            let b: Vec<u8> = (0..len).map(|i| (x >> (i % 7 * 8)) as u8 ^ i as u8).collect();
            let (h1, h2, lo, hi) =
                crate::hash::meta_hash128_and_digests(&b, crate::format::bank::BANK_LANE_SEEDS);
            assert_eq!((h1, h2), meta_hash128(&b));
            let mut a = crate::format::bank::LogicalColHash::new();
            a.observe(&b);
            let mut c = crate::format::bank::LogicalColHash::new();
            c.add(lo, hi);
            assert_eq!(a.digest(), c.digest());
        }
    }

    /// pcopy stats 1b: the integer MCV selection, the word arm's radix
    /// order and the 16-byte prefix sort reproduce the byte-comparing
    /// references exactly — count ties at and across the K boundary, all
    /// counts 1 (near-unique), fewer than K entries, every word width with
    /// constant and varying bytes, and byte strings sharing 0..40-byte
    /// prefixes.
    #[test]
    fn finalize_without_byte_compares_equals_the_references() {
        fn mcv_reference(ordered: &[(&[u8], u64)]) -> Vec<(Vec<u8>, u64)> {
            let mut by_count = ordered.to_vec();
            by_count.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
            by_count.iter().take(STATS_MCV_K).map(|&(k, v)| (k.to_vec(), v)).collect()
        }
        let mut x: u64 = 0x1b1b_0000_5eed_0001;
        for case in 0..400u64 {
            x = crate::hash::mix64(x);
            let n = match case % 5 {
                0 => (x % 40) as usize,
                1 => STATS_MCV_K,
                2 => STATS_MCV_K + 1,
                _ => 100 + (x % 3000) as usize,
            };
            let spread = match case % 4 {
                0 => 1,  // every count 1
                1 => 3,  // many ties
                _ => 1 + (x >> 20) % 50,
            };
            let mut vals: Vec<Vec<u8>> = (0..n as u64)
                .map(|i| {
                    let w = crate::hash::mix64(x ^ i);
                    (0..1 + (w % 12) as usize).map(|j| (w >> (j % 8 * 8)) as u8 % 7).collect()
                })
                .collect();
            vals.sort();
            vals.dedup();
            let ordered: Vec<(&[u8], u64)> = vals
                .iter()
                .enumerate()
                .map(|(i, v)| (v.as_slice(), 1 + crate::hash::mix64(x ^ (i as u64) << 7) % spread))
                .collect();
            assert_eq!(mcv_top_k(&ordered, None).unwrap(), mcv_reference(&ordered), "case {case} n {n} spread {spread}");
        }
        // The word arm's radix order == the u128-key sort, widths 1..8,
        // above and below the radix cut-in.
        for width in [1u8, 2, 4, 8] {
            for n in [5usize, 1023, 1024, 20_000] {
                let mask = if width == 8 { u64::MAX } else { (1u64 << (8 * width)) - 1 };
                let mut seen = std::collections::HashSet::new();
                let mut entries = Vec::new();
                while entries.len() < n.min(if width == 1 { 256 } else { usize::MAX }) {
                    x = crate::hash::mix64(x);
                    // some bytes constant: a narrow value range in places
                    let w = if x & 1 == 0 { x & mask & 0x00ff_00ff_00ff_00ff } else { x & mask };
                    if seen.insert(w) {
                        entries.push(SmallEntry { word: w, count: 1 + x % 3, stamp: 0, len: width, hash: [0; 3] });
                    }
                }
                let mut want: Vec<(u128, u32)> =
                    entries.iter().enumerate().map(|(i, e)| (small_key(e.word, e.len), i as u32)).collect();
                want.sort_unstable_by_key(|t| t.0);
                let want: Vec<u32> = want.into_iter().map(|t| t.1).collect();
                assert_eq!(small_byte_order(&entries, None).map(|v| v.to_vec()), Some(want), "width {width} n {}", entries.len());
            }
        }
        let mixed = [
            SmallEntry { word: 1, count: 1, stamp: 0, len: 1, hash: [0; 3] },
            SmallEntry { word: 1, count: 1, stamp: 0, len: 2, hash: [0; 3] },
        ];
        assert!(small_byte_order(&mixed, None).is_none(), "mixed lengths keep the comparison sort");
        // 16-byte prefix sort == plain sort with long shared prefixes.
        let mut v: Vec<Vec<u8>> = Vec::new();
        for i in 0..4000u64 {
            x = crate::hash::mix64(x);
            let len = (x % 41) as usize;
            let mut b: Vec<u8> = (0..len).map(|k| if k < 18 { b'a' + (x % 2) as u8 } else { ((x >> (k % 8 * 8)) % 3) as u8 }).collect();
            if i % 7 == 0 {
                b.push(0);
            }
            v.push(b);
        }
        v.sort();
        v.dedup();
        let mut w = v.clone();
        w.reverse();
        sort_bytes_prefixed(&mut w, |t| t.as_slice());
        assert_eq!(v, w);
    }

    /// The chunked sort equals a plain byte sort on long shared prefixes (several chunks deep), values that are
    /// prefixes of others at and around chunk edges, trailing zero bytes, and shuffled input.
    #[test]
    fn chunked_sort_equals_the_plain_byte_sort() {
        let mut x = 0x5eed_u64;
        let mut v: Vec<Vec<u8>> = Vec::new();
        let stem: Vec<u8> = b"http://example.com/some/long/shared/path/segment/".to_vec();
        for i in 0..6000u64 {
            x = crate::hash::mix64(x);
            let cut = [0usize, 15, 16, 17, 31, 32, 33, stem.len()][(x % 8) as usize].min(stem.len());
            let mut b = stem[..cut].to_vec();
            let tail = (x >> 8) % 40;
            for k in 0..tail {
                b.push(((x >> (k % 7 * 8)) % 4) as u8 + if i % 3 == 0 { 0 } else { b'0' });
            }
            if i % 11 == 0 {
                b.push(0);
            }
            v.push(b);
        }
        v.sort();
        v.dedup();
        let mut w = v.clone();
        for i in (1..w.len()).rev() {
            x = crate::hash::mix64(x);
            w.swap(i, (x % (i as u64 + 1)) as usize);
        }
        sort_bytes_prefixed(&mut w, |t| t.as_slice());
        assert_eq!(v, w);
    }

    #[test]
    fn exact_counts_topk_and_bounds() {
        let mut d = DistAcc::default();
        // 100 x "a", 10 x "b", 1 x each of c00..c49.
        for _ in 0..100 {
            d.observe(b"a");
        }
        for _ in 0..10 {
            d.observe(b"b");
        }
        for i in 0..50 {
            d.observe(format!("c{i:02}").as_bytes());
        }
        let out = d.finalize();
        assert_eq!(out.nonnull, 160);
        assert_eq!(out.ndv_eligible, 52);
        assert_eq!(out.long_values, 0);
        assert_eq!(out.mcv[0], (b"a".to_vec(), 100));
        assert_eq!(out.mcv[1], (b"b".to_vec(), 10));
        // singles tiebreak by byte order
        assert_eq!(out.mcv[2].0, b"c00".to_vec());
        assert_eq!(out.mcv.len(), STATS_MCV_K.min(52));
        // bounds: byte-ordered, min first, max last
        assert_eq!(out.hist_bounds.first().unwrap(), &b"a".to_vec());
        assert_eq!(out.hist_bounds.last().unwrap(), &b"c49".to_vec());
        let mut sorted = out.hist_bounds.clone();
        sorted.sort();
        assert_eq!(sorted, out.hist_bounds, "bounds are byte-ordered");
    }

    #[test]
    fn long_values_counted_but_ineligible() {
        let mut d = DistAcc::default();
        let long = vec![7u8; STATS_MCV_VALUE_MAX + 1];
        d.observe(&long);
        d.observe(b"x");
        let out = d.finalize();
        assert_eq!(out.nonnull, 2);
        assert_eq!(out.long_values, 1);
        assert_eq!(out.ndv_eligible, 1);
        assert!(out.mcv.iter().all(|(v, _)| v.len() <= STATS_MCV_VALUE_MAX));
    }

    #[test]
    fn deterministic_across_observation_order() {
        let vals: Vec<Vec<u8>> = (0..200u32).map(|i| i.to_le_bytes().to_vec()).collect();
        let mut a = DistAcc::default();
        for v in &vals {
            a.observe(v);
        }
        let mut b = DistAcc::default();
        for v in vals.iter().rev() {
            b.observe(v);
        }
        assert_eq!(a.finalize(), b.finalize());
    }

    // ---- DISTACC-KILL equivalence battery (born-RED charter) ------------
    //
    // The hash arm must be OUTPUT-IDENTICAL to the BTreeMap reference arm
    // on every corpus — ColDistribution is sealed sidecar bytes (nonnull,
    // ndv_eligible, long_values, MCV values+counts+order, hist bounds),
    // so any divergence here is a byte-law break, not a tuning delta.

    /// xorshift64* — deterministic corpus generator (no external entropy).
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545F4914F6CDD1D)
        }
    }

    fn assert_arms_equal(corpus: &[Vec<u8>], what: &str) {
        let mut h = DistAcc::with_hash_arm(true);
        let mut r = DistAcc::with_hash_arm(false);
        for v in corpus {
            h.observe(v);
            r.observe(v);
        }
        let (fh, fr) = (h.finalize(), r.finalize());
        assert_eq!(fh, fr, "hash arm diverged from BTreeMap reference: {what}");
        // And observation order must not matter on the hash arm either.
        let mut h2 = DistAcc::with_hash_arm(true);
        for v in corpus.iter().rev() {
            h2.observe(v);
        }
        assert_eq!(h2.finalize(), fr, "hash arm order-sensitive: {what}");
    }

    #[test]
    fn arms_equal_per_width_families() {
        // The canonical-byte families the builder actually feeds: 1-byte
        // (bool), 4/8-byte LE words, fixed-16, and variable text.
        let mut rng = Rng(0x5EED_1);
        let bools: Vec<Vec<u8>> = (0..1000).map(|_| vec![(rng.next() & 1) as u8]).collect();
        assert_arms_equal(&bools, "bool family");

        let mut rng = Rng(0x5EED_2);
        let w4: Vec<Vec<u8>> = (0..20_000)
            .map(|_| ((rng.next() % 3000) as u32).to_le_bytes().to_vec())
            .collect();
        assert_arms_equal(&w4, "4-byte word family (duplicate-heavy)");

        let mut rng = Rng(0x5EED_3);
        let w8: Vec<Vec<u8>> = (0..20_000).map(|_| rng.next().to_le_bytes().to_vec()).collect();
        assert_arms_equal(&w8, "8-byte word family (all-distinct)");

        let mut rng = Rng(0x5EED_4);
        let f16: Vec<Vec<u8>> = (0..5000)
            .map(|_| {
                let mut v = vec![0u8; 16];
                v[..8].copy_from_slice(&(rng.next() % 500).to_le_bytes());
                v
            })
            .collect();
        assert_arms_equal(&f16, "fixed-16 family");

        let mut rng = Rng(0x5EED_5);
        let text: Vec<Vec<u8>> = (0..10_000)
            .map(|_| format!("url/{}/page", rng.next() % 4000).into_bytes())
            .collect();
        assert_arms_equal(&text, "text family (zipf-ish)");
    }

    #[test]
    fn arms_equal_adversarial_probe_chains() {
        // Values filtered to share low h1 bits: they pile into the same
        // initial buckets, forcing long linear-probe chains and rehash
        // relocations — the collision paths the reference arm never has.
        let mut corpus = Vec::new();
        let mut i: u64 = 0;
        while corpus.len() < 3000 {
            let v = i.to_le_bytes().to_vec();
            let (h1, _) = meta_hash128(&v);
            if h1 & 0x3F == 0 {
                corpus.push(v);
            }
            i += 1;
        }
        // Duplicate a slice of them heavily so hit-confirms also traverse
        // the chains.
        let dups: Vec<Vec<u8>> = corpus.iter().take(50).cloned().collect();
        for _ in 0..40 {
            corpus.extend(dups.iter().cloned());
        }
        assert_arms_equal(&corpus, "same-bucket probe chains + dup hits");
    }

    #[test]
    fn arms_equal_edges_ties_and_long_values() {
        // Empty value, the exact eligibility boundary, long values, and
        // an all-ties MCV field (every count equal — tiebreak is pure
        // byte order, the exact sealed-order hazard).
        let mut corpus: Vec<Vec<u8>> = Vec::new();
        corpus.push(Vec::new()); // b""
        corpus.push(vec![0u8]);
        corpus.push(vec![0u8; STATS_MCV_VALUE_MAX]); // last eligible len
        corpus.push(vec![0u8; STATS_MCV_VALUE_MAX + 1]); // first long len
        corpus.push(vec![255u8; STATS_MCV_VALUE_MAX + 7]);
        for i in 0..100u8 {
            for _ in 0..3 {
                corpus.push(vec![i, i ^ 0x55]); // 100 values, count 3 each
            }
        }
        assert_arms_equal(&corpus, "edges + all-ties MCV");
    }

    #[test]
    fn arms_equal_growth_and_rehash() {
        // Enough distinct values to force many doublings from the lazy
        // 16-slot start; counts must survive every rehash.
        let corpus: Vec<Vec<u8>> = (0..100_000u64)
            .map(|i| i.to_le_bytes().to_vec())
            .collect();
        assert_arms_equal(&corpus, "growth ladder to 100k distinct");
    }

    // ---- DICT-DEDUP feed equivalence battery (born-RED charter) ----------
    //
    // `distribution_from_sorted_counts` over a counted distinct set must be
    // OUTPUT-IDENTICAL to the accumulator (BOTH #971 arms) over the
    // expanded value sequence — the feed becomes sealed sidecar bytes on
    // dict-elected columns, so any divergence is a byte-law break.

    /// Counted set (byte-sorted) from a corpus + the corpus expanded in a
    /// deterministic non-sorted interleave, then all three producers.
    fn assert_feed_equals_acc(corpus: &[Vec<u8>], what: &str) {
        let mut counts: BTreeMap<Vec<u8>, u64> = BTreeMap::new();
        for v in corpus {
            *counts.entry(v.clone()).or_insert(0) += 1;
        }
        let entries: Vec<(Vec<u8>, u64)> = counts.into_iter().collect();
        let fed = distribution_from_sorted_counts(&entries);
        let mut h = DistAcc::with_hash_arm(true);
        let mut r = DistAcc::with_hash_arm(false);
        for v in corpus {
            h.observe(v);
            r.observe(v);
        }
        assert_eq!(fed, h.finalize(), "feed diverged from hash acc: {what}");
        assert_eq!(fed, r.finalize(), "feed diverged from BTree acc: {what}");
    }

    #[test]
    fn feed_equals_accumulator_families() {
        let mut rng = Rng(0xD1C7_1);
        let text: Vec<Vec<u8>> = (0..10_000)
            .map(|_| format!("url/{}/page", rng.next() % 4000).into_bytes())
            .collect();
        assert_feed_equals_acc(&text, "text family (zipf-ish)");

        let mut rng = Rng(0xD1C7_2);
        let dup4: Vec<Vec<u8>> = (0..20_000)
            .map(|_| ((rng.next() % 300) as u32).to_le_bytes().to_vec())
            .collect();
        assert_feed_equals_acc(&dup4, "duplicate-heavy 4-byte family");

        let mut rng = Rng(0xD1C7_3);
        let uniq: Vec<Vec<u8>> = (0..5_000).map(|_| rng.next().to_le_bytes().to_vec()).collect();
        assert_feed_equals_acc(&uniq, "all-distinct family");
    }

    #[test]
    fn feed_equals_accumulator_edges_long_values_and_ties() {
        // Empty value, the exact eligibility boundary, LONG dict entries
        // (a dict entry may exceed STATS_MCV_VALUE_MAX — the entry-grain
        // gate must reproduce the observe-grain gate's counts exactly),
        // and an all-ties MCV field.
        let mut corpus: Vec<Vec<u8>> = Vec::new();
        corpus.push(Vec::new());
        corpus.push(vec![0u8; STATS_MCV_VALUE_MAX]); // last eligible len
        for _ in 0..7 {
            corpus.push(vec![9u8; STATS_MCV_VALUE_MAX + 1]); // long, count 7
        }
        for _ in 0..3 {
            corpus.push(vec![255u8; STATS_MCV_VALUE_MAX + 40]); // long, count 3
        }
        for i in 0..100u8 {
            for _ in 0..3 {
                corpus.push(vec![i, i ^ 0x55]); // all-ties MCV
            }
        }
        assert_feed_equals_acc(&corpus, "edges + long entries + ties");
    }

    #[test]
    fn feed_empty_set_matches_empty_accumulator() {
        assert_eq!(
            distribution_from_sorted_counts(&[]),
            DistAcc::default().finalize(),
            "zero-entry feed must equal a zero-observation accumulator"
        );
    }

    #[test]
    fn observe_hashed_matches_observe() {
        // The builder feeds the precomputed hash; the compat face hashes
        // internally. Same bytes -> same accumulator state.
        let mut rng = Rng(0xFEED);
        let corpus: Vec<Vec<u8>> = (0..5000)
            .map(|_| (rng.next() % 1000).to_le_bytes().to_vec())
            .collect();
        let mut a = DistAcc::with_hash_arm(true);
        let mut b = DistAcc::with_hash_arm(true);
        for v in &corpus {
            a.observe(v);
            let (h1, h2) = meta_hash128(v);
            b.observe_hashed(h1, h2, v);
        }
        assert_eq!(a.finalize(), b.finalize());
    }

    #[test]
    fn a_pinned_accumulator_keyed_into_its_input_finalizes_as_the_copying_ones() {
        // K1-MEM B: values kept in place (pinned) and values copied into
        // the arena (unpinned, and across many chunks) give the BTree
        // reference arm's distribution, with empty, long and repeated
        // values in the mix.
        let mut rng = Rng(0xB0B);
        let corpus: Vec<Vec<u8>> = (0..20_000)
            .map(|i| match i % 11 {
                0 => Vec::new(),
                1 => vec![b'L'; STATS_MCV_VALUE_MAX + 1 + (rng.next() % 5) as usize],
                2 => format!("r{}", rng.next() % 40).into_bytes(),
                _ => format!("{:016x}{:016x}{:016x}", rng.next(), rng.next(), rng.next() % 3_000).into_bytes(),
            })
            .collect();
        let mut pinned = DistAcc::with_hash_arm(true);
        // SAFETY: `corpus` outlives every use of `pinned` below.
        unsafe { pinned.pin_inputs() };
        let mut copied = DistAcc::with_hash_arm(true);
        let mut reference = DistAcc::with_hash_arm(false);
        for v in &corpus {
            let (h1, h2) = meta_hash128(v);
            // SAFETY: as above.
            unsafe { pinned.observe_hashed_input(h1, h2, v) };
            unsafe { copied.observe_hashed_input(h1, h2, v) };
            reference.observe_hashed(h1, h2, v);
        }
        let want = reference.finalize();
        assert_eq!(pinned.finalize(), want);
        assert_eq!(copied.finalize(), want);
        assert_eq!(pinned.distinct_exact(), reference.distinct_exact());
        let Acc::Hash { entries, arena, .. } = &pinned.acc else { panic!("hash arm") };
        assert!(arena.chunks.is_empty(), "nothing copied");
        let inputs: std::collections::HashSet<usize> = corpus.iter().map(|v| v.as_ptr() as usize).collect();
        assert!(entries.iter().all(|e| e.len == 0 || inputs.contains(&e.at)), "keyed into the input");
        let Acc::Hash { arena, .. } = &copied.acc else { panic!("hash arm") };
        assert!(arena.chunks.len() > 3, "several arena chunks");
    }

    /// FOLDSTATS: value order written out independently of `HistOrder::key` (sign / zero extension, IEEE with PG's
    /// float rules), ties by bytes.
    fn value_cmp(order: HistOrder, a: &[u8], b: &[u8]) -> Ordering {
        let word = |v: &[u8], signed: bool| {
            let fill = if signed && v.last().is_some_and(|&x| x & 0x80 != 0) { 0xFF } else { 0 };
            let mut le = [fill; 8];
            le[..v.len()].copy_from_slice(v);
            le
        };
        let fl = |v: &[u8]| match v.len() {
            4 => Some(f64::from(f32::from_le_bytes(v.try_into().expect("4")))),
            8 => Some(f64::from_le_bytes(v.try_into().expect("8"))),
            _ => None,
        };
        let decodable = |v: &[u8]| !v.is_empty() && v.len() <= 8;
        let by_value = match order {
            HistOrder::Bytes => None,
            _ if !decodable(a) || !decodable(b) => None,
            HistOrder::Signed => Some(i64::from_le_bytes(word(a, true)).cmp(&i64::from_le_bytes(word(b, true)))),
            HistOrder::Unsigned => Some(u64::from_le_bytes(word(a, false)).cmp(&u64::from_le_bytes(word(b, false)))),
            HistOrder::Float => match (fl(a), fl(b)) {
                (Some(x), Some(y)) => Some(match (x.is_nan(), y.is_nan()) {
                    (true, true) => Ordering::Equal,
                    (true, false) => Ordering::Greater,
                    (false, true) => Ordering::Less,
                    (false, false) => x.partial_cmp(&y).expect("not NaN"),
                }),
                _ => None,
            },
        };
        by_value.unwrap_or(Ordering::Equal).then_with(|| a.cmp(b))
    }

    /// The full-sort reference: MCVs the first k of the whole set sorted (count desc, bytes asc); bound i the value
    /// holding cumulative rank i * (total - 1) / (nbounds - 1) of the whole set sorted in value order.
    fn full_sort_reference(set: &[(&[u8], u64)], order: HistOrder) -> (Vec<(Vec<u8>, u64)>, Vec<Vec<u8>>) {
        let mut by_count = set.to_vec();
        by_count.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        let mcv = by_count.iter().take(STATS_MCV_K).map(|&(v, c)| (v.to_vec(), c)).collect();
        let mut ordered = set.to_vec();
        ordered.sort_by(|a, b| value_cmp(order, a.0, b.0));
        let total: u64 = set.iter().map(|e| e.1).sum();
        let mut bounds = Vec::new();
        if total > 0 {
            let cum: Vec<u64> = ordered.iter().scan(0u64, |s, e| { *s += e.1; Some(*s) }).collect();
            let nb = STATS_HIST_BOUNDS as u64;
            let mut targets: Vec<u64> = (0..nb).map(|i| i * (total - 1) / (nb - 1)).collect();
            targets.dedup();
            for t in targets {
                bounds.push(ordered[cum.partition_point(|&c| c <= t)].0.to_vec());
            }
        }
        (mcv, bounds)
    }

    /// FOLDSTATS perf: the finish (top-k counts, radix-sorted keys, bounds read at their ranks) equals the full-sort
    /// reference byte for byte, for every order and width, unique and repeated counts, heavy hitters, ±0 / NaN
    /// payload ties, a width the order does not decode, sets on both sides of the radix cut and of the MCV size,
    /// arriving shuffled, ascending and descending; and both accumulator arms finalize to it.
    #[test]
    fn finish_is_the_full_sort_reference() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for order in [HistOrder::Bytes, HistOrder::Signed, HistOrder::Unsigned, HistOrder::Float] {
            for width in [1usize, 2, 4, 8, 12] {
                for n in [20usize, 700, 5000] {
                    let mut vals: Vec<Vec<u8>> = (0..n).map(|_| rng.next().to_le_bytes()[..width.min(8)].to_vec()).collect();
                    if width == 12 {
                        for v in &mut vals {
                            v.extend_from_slice(&[7, 7, 7, 7]);
                        }
                    }
                    if width == 8 {
                        vals.extend([0.0f64, -0.0, f64::NAN, -f64::NAN, f64::INFINITY, f64::NEG_INFINITY].map(|f| f.to_le_bytes().to_vec()));
                        vals.push(f64::from_bits(0x7FF8_0000_0000_0001).to_le_bytes().to_vec());
                    }
                    if width == 4 {
                        vals.extend([0.0f32, -0.0, f32::NAN, -f32::NAN, f32::INFINITY].map(|f| f.to_le_bytes().to_vec()));
                        vals.push(f32::from_bits(0x7FC0_0001).to_le_bytes().to_vec());
                    }
                    vals.sort();
                    vals.dedup();
                    let mut asc = vals.clone();
                    asc.sort_by(|a, b| value_cmp(order, a, b));
                    let mut desc = asc.clone();
                    desc.reverse();
                    let mut shuffled = vals.clone();
                    for i in (1..shuffled.len()).rev() {
                        shuffled.swap(i, (rng.next() % (i as u64 + 1)) as usize);
                    }
                    for (shape, arrival) in [("byte-ordered", &vals), ("ascending", &asc), ("descending", &desc), ("shuffled", &shuffled)] {
                        for counted in [false, true] {
                            // counted: 1..=4 with every 97th value heavy
                            let set: Vec<(&[u8], u64)> = arrival
                                .iter()
                                .enumerate()
                                .map(|(i, v)| (v.as_slice(), if !counted { 1 } else if i % 97 == 0 { 400 + i as u64 } else { 1 + rng.next() % 4 }))
                                .collect();
                            let nonnull: u64 = set.iter().map(|e| e.1).sum();
                            let (mcv, bounds) = full_sort_reference(&set, order);
                            let what = format!("{order:?} width {width} n {n} {shape} counted {counted}");
                            let got = finish_distribution(&set, shape == "byte-ordered", nonnull, 0, order, None).unwrap();
                            assert_eq!(got.mcv, mcv, "MCVs: {what}");
                            assert_eq!(got.hist_bounds, bounds, "bounds: {what}");
                            assert_eq!(got.ndv_eligible, set.len() as u64, "{what}");
                        }
                    }
                }
            }
            // both arms, a corpus with repeats and ties at the MCV cut
            let corpus: Vec<Vec<u8>> = (0..5000).map(|_| (rng.next() % 700).to_le_bytes().to_vec()).collect();
            let mut h = DistAcc::with_hash_arm(true);
            let mut r = DistAcc::with_hash_arm(false);
            let mut counts: BTreeMap<Vec<u8>, u64> = BTreeMap::new();
            for v in &corpus {
                h.observe(v);
                r.observe(v);
                *counts.entry(v.clone()).or_default() += 1;
            }
            let set: Vec<(&[u8], u64)> = counts.iter().map(|(k, &c)| (k.as_slice(), c)).collect();
            let (mcv, bounds) = full_sort_reference(&set, order);
            for d in [h.finalize_in(order), r.finalize_in(order)] {
                assert_eq!(d.mcv, mcv, "arm MCVs in {order:?}");
                assert_eq!(d.hist_bounds, bounds, "arm bounds in {order:?}");
            }
        }
    }
}
