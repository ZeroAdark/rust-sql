//! The tier-3 record plan (the words a group plan's params carry, `Plan`, the record layouts) and its owner phases: the bare
//! pass 1 and export of the two-pass count top (keyed.twopass, V4R-197), the granule
//! marks and the distinct family's owner rounds.

use gravity_abi::ctx::hdr;

use gravity_abi::breaker::{KeyCell, KeyKind, OwnerFns, RecordLayout, SlotKind, MAX_RECORD_KEYS, ORDER_NONE, OWNERTOP_HDR, OWNER_PARAMS};

use crate::{Touch, SITE_EXEC_CELLS};

use core::sync::atomic::{AtomicBool, AtomicU64};

pub use gravity_abi::t3::{MAX_KEYS, MAX_OUT, MAX_QUALS, MAX_SLOTS, MAX_TOPK};

use super::raw::t3::{bare_fold, bare_reduce, distinct_end_round, distinct_min, distinct_open, distinct_reduce, distinct_reduce2, export_min, export_open, export_reduce, owner_noop};
pub use super::raw::t3::{export_keys, export_records, fold_pair, gmark_lists, render_reps, slot_word, slot_word_of};

pub const TOUCH: Touch = Touch { hdr: &[hdr::RESUME], sites: &[(0, SITE_EXEC_CELLS)] };

pub const P_LIMIT: usize = 0;

pub const P_OFFSET: usize = 1;

pub const P_SHAPE: usize = 2;

pub const P_KEYS: usize = 3;

pub const P_SLOTS: usize = 4;

pub const P_OUT: usize = 5;

pub const P_OUTVAL: usize = 6;

pub const P_MISC: usize = 7;

pub const NPARAMS: usize = 8;

pub const INIT: u32 = 256;

pub const DEFAULT_BITS: u8 = 4;

pub const K_INT: u8 = 0;

pub const K_MINUTE: u8 = 1;

pub const K_CODE: u8 = 2;

/// A text key by value: the code lane's `Fp128` table lane (`fp_lane`) gives the record two key words, and a `MinCode` slot carries a `(part, code)` representative; the emit's top-k renders a
/// candidate's representative through `Site.dict` only when its comparison with the running worst reaches it (`Top::offer_lazy`) — the same over a merged table, refilled spill partitions or the
/// distinct family's accumulator; nothing else is rendered.
pub const K_TEXT: u8 = 3;

/// A text key by a derived value (e.g. a `regexp_replace` over a text column): the code lane's `DerivedFingerprint` table
/// lane (`fp_lane`, an `Fp128` table over the derived bytes, faults where the function raised) gives the
/// record its two words as `K_TEXT`, and a `MinCode` representative as `K_TEXT` (the value behind it is
/// the column's, not the derived one).
pub const K_DERIVED: u8 = 4;

/// The row's part id as a key word (the second pass's per-part minima: `MIN(text)` is a byte order
/// no slot folds across parts, so the record is per part and the owner renders and compares).
pub const K_PART: u8 = 5;

pub const S_COUNT: u8 = 0;

pub const S_SUM: u8 = 1;

pub const S_AVG: u8 = 2;

/// `avg` of an `I64` table lane (`slot_lane`) read by key 0's code (e.g. `avg(length(t))` grouped by a text `t`).
pub const S_AVG_TBL: u8 = 3;

/// `min(text)` of a code lane (`slot_lane`) as a `MinCode` slot: exact within a part (a part's codes
/// are in value order), a per-part record under `K_PART` and the join emit's byte order across parts.
pub const S_MIN_CODE: u8 = 4;

pub const ORDER_COUNT_DESC: u8 = 0;

pub const ORDER_KEYS_ASC: u8 = 1;

pub const MODE_HASH: u8 = 0;

pub const MODE_APPEND: u8 = 1;

/// Bit 2 of the mode: the program is the two-pass form (the plan words at `P1_BASE` are pass 1's).
pub const MODE_TWO_PASS: u8 = 4;

/// The first pass's plan words in a two-pass program's params, and the params' length then.
pub const P1_BASE: usize = 8;

pub const NPARAMS2: usize = 16;

/// The pass-1 block's export word (`P_OUTVAL`: pass 1 has no output): the rule | its threshold << 8.
pub const P_EXPORT: usize = P_OUTVAL;

/// `P_MISC` bit 57: the granule marks (`Gmark`) — a distinct pass 1 whose first key is text records,
/// per (phrase, part), the part's granules holding a surviving row (a bit per granule, `GMARK_BITS`)
/// as a phantom record `(fp, part, 1)` behind one extra key word (0 on a row record), so the owner's
/// reduce lists them and the leader cuts pass 2's claims to the admitted phrases' granules.
pub const MISC_GMARK: u64 = 1 << 57;

/// The granule bits of a mark: bit `g.min(62)` — the last bit stands for every granule from 62 on.
pub const GMARK_BITS: u32 = 63;

/// A mark listed by the owner: `[fp lo, part, bits]`; `part == u32::MAX` says the part took no marks (every granule admitted).
pub const GMARK_ENTRY: usize = 24;

pub const GMARK_ALL: u64 = u32::MAX as u64;

/// `P_MISC` bit 58 of a pass-2 plan keyed `(text key, K_PART)` with `count(*)` alone: the by-code walk (§2.2aa,
/// `Admit::open_part`) — a part's record per admitted key is its smallest code with that fingerprint (the sorted-rank
/// contract), one walk of the fingerprint table in code order; a part whose code lane carries `FLAG_DELETES` scans its rows.
pub const MISC_BYCODE: u64 = 1 << 58;

/// The group estimate's width in `P_MISC` (bits 16..56).
pub const MISC_GROUPS_MASK: u64 = (1 << 40) - 1;

/// The dominance export: every key whose count is 2 or more, plus the `LIMIT + OFFSET` smallest keys.
pub const EXPORT_DUPS: u64 = 1;

/// The dominance export's bare form (a top by `count(*)` over one int key): pass 1 over one int key with `count(*)` alone scatters
/// the bare key word — `BARE`, 8 B, no slot — by observation (a run of two or more rows appends it twice, the
/// claim's floor keys once more), the owners keep the partition's keys in an open-addressing set and list
/// every key seen again (`bare_fold`, the zero key by a counter), and `bare_reduce` exports the list: no
/// count is folded over the rows. `bare_dups` names the plan words it applies to.
pub const BARE: RecordLayout = RecordLayout::bare(KeyKind::I64, 8);

/// `HashAgg.flags` bit of a bare owner table whose dup area (`[zeros, ndups, dups…]` behind the slots) is initialised.
pub(super) const BARE_INIT: u8 = 0x40;

/// The HAVING export: every record whose count is above the threshold (`P_EXPORT >> 8`), whole.
pub const EXPORT_HAVING: u64 = 2;

/// The top export: the distinct family's round-2 tops are the admitted set (a top over a distinct count of text keys).
pub const EXPORT_TOP: u64 = 3;

pub const OUT_KEY: u8 = 0;

pub const OUT_KEY_MINUS: u8 = 1;

pub const OUT_CONST: u8 = 2;

pub const OUT_COUNT: u8 = 3;

pub const OUT_SUM: u8 = 4;

pub const OUT_AVG: u8 = 5;

pub const OUT_KEY_NUMERIC: u8 = 6;

/// The distinct family's `count(DISTINCT user)`: the accumulator's slot 0.
pub const OUT_DISTINCT: u8 = 7;

/// The join emit's `min(text)`: pass 2's `S_MIN_CODE` slot `index`, the byte minimum over the parts.
pub const OUT_MIN: u8 = 8;

/// `key + c` and `key * c` (the trimmed group keys' re-derivation; `OUT_KEY_MINUS` is `key - c`): the
/// admission proved the key's domain cannot overflow the result type, so the emit's arithmetic is exact.
pub const OUT_KEY_PLUS: u8 = 9;

pub const OUT_KEY_MUL: u8 = 10;

pub struct Plan {
    pub nkeys: usize,
    pub nslots: usize,
    pub order: u8,
    pub mode: u8,
    pub filter: Option<usize>,
    pub topk: u32,
    pub key_lane: [usize; MAX_KEYS],
    pub key_kind: [u8; MAX_KEYS],
    pub slot_lane: [usize; MAX_SLOTS],
    pub slot_kind: [u8; MAX_SLOTS],
    pub nout: usize,
    pub out_kind: [u8; MAX_OUT],
    pub out_index: [usize; MAX_OUT],
    pub out_val: [i64; MAX_OUT],
    pub limit: usize,
    pub offset: usize,
    pub bits: u8,
    pub t1_16k: u8,
    pub order_slot: usize,
    pub groups: u64,
    /// Per key: its `Fp128` table lane (a text key), its first record word, its representative slot.
    pub fp_lane: [usize; MAX_KEYS],
    pub key_word: [usize; MAX_KEYS],
    pub rep_slot: [usize; MAX_KEYS],
    pub nwords: usize,
    pub ntext: usize,
    /// Every slot lane NOT NULL: the record without its validity word.
    pub nn: bool,
    /// The two-pass form: this plan is pass 2 (its rows admitted by the consumed set, over site 1)
    /// or pass 1 (`P1_BASE`, over site 0); `site` is the driving site either way.
    pub two_pass: bool,
    pub admit: bool,
    pub site: usize,
    /// The granule marks (`MISC_GMARK`): the record carries the mark word after the keys.
    pub gmark: bool,
    /// The by-code walk of pass 2 (`MISC_BYCODE`).
    pub bycode: bool,
    /// The pass-1 block's export word as read (`P_EXPORT`).
    pub export: u64,
    /// The verdict quals: a `Bits` table lane and whether a set bit rejects the row.
    pub nquals: usize,
    pub qual_lane: [usize; MAX_QUALS],
    pub qual_code: [usize; MAX_QUALS],
    pub qual_neg: [bool; MAX_QUALS],
}

/// The shape word of a plan (`P_SHAPE`).
pub const fn shape_word(nkeys: usize, nslots: usize, order: u8, mode: u8, order_slot: usize, filter_lane: Option<usize>, topk: u32, nout: usize) -> u64 {
    let f = match filter_lane {
        Some(l) => l as u64 + 1,
        None => 0,
    };
    nkeys as u64 | (nslots as u64) << 8 | (order as u64) << 16 | (mode as u64) << 24 | (order_slot as u64) << 28 | f << 32 | (topk as u64) << 40 | (nout as u64) << 56
}

/// `shape_word` with bit 27: the slot lanes are NOT NULL (the admission saw no nullable lane).
pub const fn shape_word_nn(nkeys: usize, nslots: usize, order: u8, mode: u8, order_slot: usize, filter_lane: Option<usize>, topk: u32, nout: usize, nn: bool) -> u64 {
    shape_word(nkeys, nslots, order, mode, order_slot, filter_lane, topk, nout) | (nn as u64) << 27
}

/// The `P_MISC` word: the spill bits, the phase-1 table's size in 16 KiB units (0 = the default), the plan's group estimate (40 bits).
pub const fn misc_word(bits: u8, t1_16k: u8, groups: u64) -> u64 {
    bits as u64 | (t1_16k as u64) << 8 | (if groups > MISC_GROUPS_MASK { MISC_GROUPS_MASK } else { groups }) << 16
}

/// The `P_SLOTS` word's qual bytes: per qual `(table lane + 1) | code lane << 5 | negated << 7` at bytes 4 and 5.
pub const fn quals_word(quals: &[(usize, usize, bool)]) -> u64 {
    let mut w = 0u64;
    let mut q = 0;
    while q < quals.len() && q < MAX_QUALS {
        w |= ((quals[q].0 as u64 + 1) | (quals[q].1 as u64) << 5 | (quals[q].2 as u64) << 7) << (32 + 8 * q);
        q += 1;
    }
    w
}

/// One byte per entry: `index | kind << shift`.
pub const fn packed(entries: &[(usize, u8)], shift: u32) -> u64 {
    let mut w = 0u64;
    let mut i = 0;
    while i < entries.len() {
        w |= ((entries[i].0 as u64) | (entries[i].1 as u64) << shift) << (8 * i);
        i += 1;
    }
    w
}

impl Plan {
    /// The plan from its `NPARAMS` words (the leader reads the same plan from the program's params).
    pub fn from_words(p: &[u64]) -> Option<Plan> {
        if p.len() < NPARAMS {
            return None;
        }
        let shape = p[P_SHAPE];
        let (nkeys, nslots, nout) = ((shape & 0xff) as usize, (shape >> 8 & 0xff) as usize, (shape >> 56) as usize);
        if nkeys == 0 || nkeys > MAX_KEYS || nslots > MAX_SLOTS || nout > MAX_OUT {
            return None;
        }
        let f = (shape >> 32 & 0xff) as usize;
        let mut pl = Plan {
            nkeys,
            nslots,
            order: (shape >> 16 & 0xff) as u8,
            mode: (shape >> 24 & 0x3) as u8,
            filter: if f == 0 { None } else { Some(f - 1) },
            topk: (shape >> 40 & 0xffff) as u32,
            key_lane: [0; MAX_KEYS],
            key_kind: [0; MAX_KEYS],
            slot_lane: [0; MAX_SLOTS],
            slot_kind: [0; MAX_SLOTS],
            nout,
            out_kind: [0; MAX_OUT],
            out_index: [0; MAX_OUT],
            out_val: [0; MAX_OUT],
            limit: p[P_LIMIT] as usize,
            offset: p[P_OFFSET] as usize,
            bits: (p[P_MISC] & 0xff) as u8,
            t1_16k: (p[P_MISC] >> 8 & 0xff) as u8,
            order_slot: (shape >> 28 & 0xf) as usize,
            groups: p[P_MISC] >> 16 & MISC_GROUPS_MASK,
            fp_lane: [0; MAX_KEYS],
            key_word: [0; MAX_KEYS],
            rep_slot: [usize::MAX; MAX_KEYS],
            nwords: 0,
            ntext: 0,
            nn: shape >> 27 & 1 != 0,
            two_pass: false,
            admit: false,
            site: 0,
            gmark: p[P_MISC] & MISC_GMARK != 0,
            bycode: p[P_MISC] & MISC_BYCODE != 0,
            export: p[P_EXPORT],
            nquals: 0,
            qual_lane: [0; MAX_QUALS],
            qual_code: [0; MAX_QUALS],
            qual_neg: [false; MAX_QUALS],
        };
        let (keys, slots, out, outval) = (p[P_KEYS], p[P_SLOTS], p[P_OUT], p[P_OUTVAL]);
        for k in 0..nkeys {
            pl.key_lane[k] = (keys >> (8 * k) & 0xf) as usize;
            pl.key_kind[k] = (keys >> (8 * k + 4) & 0xf) as u8;
            pl.fp_lane[k] = (keys >> (32 + 8 * k) & 0xff) as usize;
            pl.key_word[k] = pl.nwords;
            if is_text(pl.key_kind[k]) {
                pl.rep_slot[k] = nslots + pl.ntext;
                pl.ntext += 1;
                pl.nwords += 1;
            }
            pl.nwords += 1;
        }
        if pl.gmark {
            if pl.key_kind[0] != K_TEXT {
                return None;
            }
            pl.nwords += 1;
        }
        if pl.nwords > MAX_RECORD_KEYS || (is_text(pl.key_kind[0]) && (shape >> 40 & 0xffff) != 0) {
            return None;
        }
        for j in 0..nslots {
            pl.slot_lane[j] = (slots >> (8 * j) & 0xf) as usize;
            pl.slot_kind[j] = (slots >> (8 * j + 4) & 0xf) as u8;
        }
        for q in 0..MAX_QUALS {
            let b = (slots >> (32 + 8 * q) & 0xff) as usize;
            if b & 0x1f != 0 {
                pl.qual_lane[pl.nquals] = (b & 0x1f) - 1;
                pl.qual_code[pl.nquals] = b >> 5 & 0x3;
                pl.qual_neg[pl.nquals] = b & 0x80 != 0;
                pl.nquals += 1;
            }
        }
        for c in 0..nout {
            pl.out_index[c] = (out >> (8 * c) & 0xf) as usize;
            pl.out_kind[c] = (out >> (8 * c + 4) & 0xf) as u8;
            pl.out_val[c] = (outval >> (8 * c) & 0xff) as i64;
        }
        if (nslots > 0 && pl.slot_kind[0] != S_COUNT) || pl.limit + pl.offset > MAX_TOPK || pl.limit == 0 {
            return None;
        }
        if pl.bits == 0 {
            pl.bits = DEFAULT_BITS;
        }
        Some(pl)
    }

    /// The record: a word per int key, two `Fp128` words per text key; the plain slots, then a
    /// `MinCode` representative per text key (`rep_slot`).
    pub fn layout(&self) -> RecordLayout {
        self.layout_of(self.nkeys, 0)
    }

    /// The layout of the first `nkeys` keys with `extra` leading `Count` slots before the plain
    /// slots (the distinct family's accumulator: the group keys, the pair count, the pair slots,
    /// the group text keys' representatives).
    pub fn layout_of(&self, nkeys: usize, extra: usize) -> RecordLayout {
        let mut keys = [KeyKind::I64; MAX_RECORD_KEYS];
        let mut w = 0;
        for k in 0..nkeys {
            keys[w] = match self.key_kind[k] {
                K_CODE => KeyKind::Code,
                K_TEXT | K_DERIVED => KeyKind::Fp128,
                _ => KeyKind::I64,
            };
            if is_text(self.key_kind[k]) {
                keys[w + 1] = KeyKind::Fp128;
                w += 1;
            }
            w += 1;
        }
        if self.gmark && nkeys == self.nkeys {
            w += 1;
        }
        let mut slots = [SlotKind::Count; MAX_SLOTS + 1 + MAX_KEYS];
        for j in 0..self.nslots {
            slots[extra + j] = match self.slot_kind[j] {
                S_SUM => SlotKind::SumI64,
                S_AVG | S_AVG_TBL => SlotKind::AvgI128,
                S_MIN_CODE => SlotKind::MinCode,
                _ => SlotKind::Count,
            };
        }
        let mut n = extra + self.nslots;
        for k in 0..nkeys {
            if is_text(self.key_kind[k]) {
                slots[n] = SlotKind::MinCode;
                n += 1;
            }
        }
        if self.nn { RecordLayout::new_nn(&keys[..w], &slots[..n]) } else { RecordLayout::new(&keys[..w], &slots[..n]) }
    }

    pub fn has_code_key(&self) -> bool {
        self.key_kind[..self.nkeys].contains(&K_CODE)
    }

    pub fn has_text_key(&self) -> bool {
        self.ntext > 0
    }

    pub fn has_part_key(&self) -> bool {
        self.key_kind[..self.nkeys].contains(&K_PART)
    }
}

/// A pass-1 block of the dominance export in the bare form: one key word, `count(*)` alone, no verdict qual (a code filter allowed).
/// It names the plan lowerv4's bare pass 1 emits (twopass_bare `P1_WORDS`), not a narrowing of it: that pass 1 keys the
/// TwoPass hint's one column (keyed_twopass treats more as a bug), `K_INT` is its one image word whatever the key's type
/// (the lowerer takes any type whose words order as it does, KeySem `word_order`), and the one slot is the count, the
/// form's definition (no costly aggregate in pass 1: any other keeps the hash aggregate).
pub fn bare_dups(pl: &Plan) -> bool {
    pl.export & 0xff == EXPORT_DUPS && pl.nkeys == 1 && pl.key_kind[0] == K_INT && pl.nslots == 1 && pl.nquals == 0
}

/// A pass-1 block's exported record size: the bare word, else the plan's record.
pub(super) fn export_rec(pl: &Plan) -> usize {
    if bare_dups(pl) { BARE.size() as usize } else { pl.layout().size() as usize }
}

/// A key with two fingerprint words and a `MinCode` representative (`K_TEXT`, `K_DERIVED`).
#[inline(always)]
pub const fn is_text(kind: u8) -> bool {
    kind == K_TEXT || kind == K_DERIVED
}

/// A many-group pair set (the group estimate `table::route::group_routed` routes by the group, STEP-19; a text group key) is partitioned by its group words, so a group's pairs reach one owner partition
/// and its count is final when that partition's reduce ends (§3.5's re-sink carries one record per group, not one per pair); the few-group sets keep the pair hash (a heavy group
/// spread over every owner, the lab's rule).
pub(super) fn grouped(pl: &Plan, lp: Loop) -> bool {
    // a gmark plan never groups (`Loop::of` sends it to the generic loops first): its `[fp, part, mark]` records stay on the full hash
    !pl.gmark && matches!(lp, Loop::TextInt | Loop::IntTextInt) && crate::table::route::group_routed(pl.groups, crate::table::params::spill_bits(pl.groups, pl.layout().size()), 0, true)
}

/// The row-loop class of a plan (the int-headed count shapes, the distinct family's pair sets, the int-pair slot
/// shapes), else the generic `Rows`; `grouped` reads it to pick the grouped round-2 table.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Loop {
    Rows,
    Int,
    IntInt,
    IntText,
    IntMinuteText,
    IntTextF,
    IntIntFSlots,
    Text,
    TextInt,
    IntTextInt,
    IntIntSlots,
}

impl Loop {
    pub(super) fn of(pl: &Plan) -> Loop {
        let keys = &pl.key_kind[..pl.nkeys];
        if pl.nquals > 0 {
            return Loop::Rows;
        }
        if pl.nslots <= 1 {
            match (keys, pl.filter) {
                ([K_INT], None) => return Loop::Int,
                ([K_INT, K_INT], None) => return Loop::IntInt,
                ([K_INT, K_TEXT], None) => return Loop::IntText,
                ([K_INT, K_TEXT], Some(f)) if f == pl.key_lane[1] => return Loop::IntTextF,
                ([K_INT, K_MINUTE, K_TEXT], None) => return Loop::IntMinuteText,
                _ => {}
            }
        }
        if pl.gmark || pl.two_pass {
            return Loop::Rows;
        }
        // the distinct family's pair sets: no slot, the filter (if any) on the text key
        let text_filter = |kt: usize| pl.filter.is_none_or(|f| f == pl.key_lane[kt]);
        if pl.nslots == 0 && pl.topk == 0 {
            match keys {
                [K_TEXT] if text_filter(0) => return Loop::Text,
                [K_TEXT, K_INT] if text_filter(0) => return Loop::TextInt,
                [K_INT, K_TEXT, K_INT] if text_filter(1) => return Loop::IntTextInt,
                _ => {}
            }
        }
        if (2..=3).contains(&pl.nslots) && keys == [K_INT, K_INT] && pl.slot_kind[..pl.nslots].iter().skip(1).all(|&k| k == S_SUM || k == S_AVG) {
            return if pl.filter.is_none() { Loop::IntIntSlots } else { Loop::IntIntFSlots };
        }
        Loop::Rows
    }
}

/// The phase-1 census (`PGRUST_GRAVITY_PHASES`): nanoseconds per claim in the source's window
pub static CENSUS_ON: AtomicBool = AtomicBool::new(false);

pub static CENSUS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// Pass 2's share of the census: read and fold nanoseconds, the parts walked by code, the parts scanned by row.
pub static CENSUS2: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// The dense front's converts: nanoseconds and records (the touched codes) — pass 1's residue after the walk.
pub static CONVERT: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// The first pass's owner phase: `[OwnerTop][the list: whole records][table space]`, the list a
pub fn owner_fns_pass1(params: &[u64]) -> OwnerFns {
    let rule = Plan::from_words(&params[..NPARAMS]).map_or(0, |pl| pl.export & 0xff);
    if rule == EXPORT_TOP {
        return owner_fns_distinct(params);
    }
    let mut p = [0u64; OWNER_PARAMS];
    p[..NPARAMS].copy_from_slice(&params[..NPARAMS]);
    if Plan::from_words(&params[..NPARAMS]).is_some_and(|pl| bare_dups(&pl)) {
        return OwnerFns { params: p, rounds: 1, order_slot: ORDER_NONE, order_slot2: ORDER_NONE, min_bytes: export_min, open: export_open, reduce: bare_reduce, end_round: owner_noop, reduce2: bare_reduce, fold: Some(bare_fold) };
    }
    let dups = rule == EXPORT_DUPS;
    let order_slot = if dups { 0 } else { ORDER_NONE };
    OwnerFns { params: p, rounds: 1, order_slot, order_slot2: order_slot, min_bytes: export_min, open: export_open, reduce: export_reduce, end_round: owner_noop, reduce2: export_reduce, fold: Some(crate::table::owner_fold::owner_fold) }
}

/// The list's offset, and its bytes for a region of `bytes`.
pub(super) fn export_list(bytes: usize) -> (usize, usize) {
    ((OWNERTOP_HDR + 7) & !7, (bytes / 4) & !7)
}

/// The distinct family's owner phase (§3.5): round 1 dedupes a partition's pairs into the
pub fn owner_fns_distinct(params: &[u64]) -> OwnerFns {
    let mut p = [0u64; OWNER_PARAMS];
    p[..NPARAMS].copy_from_slice(&params[..NPARAMS]);
    let pl = Plan::from_words(&params[..NPARAMS]);
    let order_slot2 = match &pl {
        Some(pl) if pl.order == ORDER_COUNT_DESC => pl.order_slot as u8,
        _ => ORDER_NONE,
    };
    OwnerFns { params: p, rounds: 2, order_slot: ORDER_NONE, order_slot2, min_bytes: distinct_min, open: distinct_open, reduce: distinct_reduce, end_round: distinct_end_round, reduce2: distinct_reduce2, fold: Some(crate::table::owner_fold::owner_fold_dense) }
}

/// The distinct owner's region: `[OwnerTop][results][mark list][round-2 region][table space]` — the
pub(super) fn distinct_geometry(pl: &Plan, bytes: usize) -> (usize, usize, usize) {
    let list_off = distinct_list_off(pl);
    let rest = bytes.saturating_sub(list_off);
    let list_bytes = if pl.gmark { (rest / 4) & !7 } else { 0 };
    let round2_off = list_off + list_bytes;
    let r2_bytes = ((rest - list_bytes) / 2) & !7;
    (list_off, round2_off, round2_off + r2_bytes)
}

pub(super) fn distinct_list_off(pl: &Plan) -> usize {
    let ng = pl.nkeys - 1;
    let n = if ng == 0 { 0 } else { pl.limit + pl.offset };
    (OWNERTOP_HDR + n * pl.layout_of(ng, 1).size() as usize + 7) & !7
}

/// The pair → accumulator fold of a NOT NULL plan as word offsets: per entry the accumulator's word,
pub(crate) struct PairFold {
    pub(super) n: usize,
    pub(super) dst: [usize; 2 * MAX_SLOTS + MAX_KEYS],
    pub(super) src: [usize; 2 * MAX_SLOTS + MAX_KEYS],
    pub(super) kind: [u8; 2 * MAX_SLOTS + MAX_KEYS],
}

pub(super) const PF_ONE: u8 = 0;

pub(super) const PF_COUNT: u8 = 1;

pub(super) const PF_SUM: u8 = 2;

pub(super) const PF_AVG: u8 = 3;

const PF_MIN: u8 = 4;

impl PairFold {
    pub(crate) fn of(pl: &Plan, acc: &RecordLayout, pair: &RecordLayout, ng: usize) -> PairFold {
        let mut f = PairFold { n: 0, dst: [0; 2 * MAX_SLOTS + MAX_KEYS], src: [0; 2 * MAX_SLOTS + MAX_KEYS], kind: [0; 2 * MAX_SLOTS + MAX_KEYS] };
        let mut push = |d: usize, s: usize, k: u8| {
            f.dst[f.n] = acc.slot_off[d] as usize / 8;
            f.src[f.n] = pair.slot_off[s] as usize / 8;
            f.kind[f.n] = k;
            f.n += 1;
        };
        push(0, 0, PF_ONE);
        for j in 0..pl.nslots {
            push(j + 1, j, match pl.slot_kind[j] {
                S_SUM => PF_SUM,
                S_AVG | S_AVG_TBL => PF_AVG,
                S_MIN_CODE => PF_MIN,
                _ => PF_COUNT,
            });
        }
        for k in 0..ng {
            if is_text(pl.key_kind[k]) {
                push(1 + pl.rep_slot[k], pl.rep_slot[k], PF_MIN);
            }
        }
        f
    }
}

/// The record layout of a shape, for the program's spill bits (`par_hashagg::spill_bits`).
pub fn layout_of(nkeys: usize, code_keys: u32, nslots: usize, slot_kinds: &[u8]) -> RecordLayout {
    let mut kinds = [K_INT; MAX_KEYS];
    for (k, kind) in kinds.iter_mut().enumerate().take(nkeys) {
        *kind = if code_keys >> k & 1 != 0 { K_CODE } else { K_INT };
    }
    layout_of_kinds(&kinds[..nkeys], nslots, slot_kinds)
}

/// The record layout of a shape by key kinds (`K_INT` / `K_MINUTE` / `K_CODE` / `K_TEXT`); `nn`: no validity word.
pub fn layout_of_kinds(kinds: &[u8], nslots: usize, slot_kinds: &[u8]) -> RecordLayout {
    layout_of_kinds_nn(kinds, nslots, slot_kinds, false)
}

pub fn layout_of_kinds_nn(kinds: &[u8], nslots: usize, slot_kinds: &[u8], nn: bool) -> RecordLayout {
    layout_of_kinds_gmark(kinds, nslots, slot_kinds, nn, false)
}

/// `layout_of_kinds_nn` with the granule-mark word (`MISC_GMARK`).
pub fn layout_of_kinds_gmark(kinds: &[u8], nslots: usize, slot_kinds: &[u8], nn: bool, gmark: bool) -> RecordLayout {
    let nkeys = kinds.len();
    let mut pl = Plan { nkeys, nslots, order: 0, mode: 0, filter: None, topk: 0, key_lane: [0; MAX_KEYS], key_kind: [0; MAX_KEYS], slot_lane: [0; MAX_SLOTS], slot_kind: [0; MAX_SLOTS], nout: 0, out_kind: [0; MAX_OUT], out_index: [0; MAX_OUT], out_val: [0; MAX_OUT], limit: 1, offset: 0, bits: 0, t1_16k: 0, order_slot: 0, groups: 0, fp_lane: [0; MAX_KEYS], key_word: [0; MAX_KEYS], rep_slot: [usize::MAX; MAX_KEYS], nwords: 0, ntext: 0, nn, two_pass: false, admit: false, site: 0, gmark, bycode: false, export: 0, nquals: 0, qual_lane: [0; MAX_QUALS], qual_code: [0; MAX_QUALS], qual_neg: [false; MAX_QUALS] };
    pl.key_kind[..nkeys].copy_from_slice(kinds);
    for k in 0..nkeys {
        pl.key_word[k] = pl.nwords;
        if is_text(kinds[k]) {
            pl.rep_slot[k] = nslots + pl.ntext;
            pl.ntext += 1;
            pl.nwords += 1;
        }
        pl.nwords += 1;
    }
    pl.nwords += gmark as usize;
    pl.slot_kind[..nslots].copy_from_slice(&slot_kinds[..nslots]);
    pl.layout()
}

#[allow(dead_code)]
const _: fn(KeyCell) = |_| {};
