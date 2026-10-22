//! Owners and their descriptors: `OwnerDesc`, `KeyCheck`, `TopKDesc`, `TwoPassDesc`, `WordsDesc`, record layouts (split out of `program/mod.rs` by type, contract-runtime landing 3).

use super::*;

/// A named owner phase's constructor over the binding's parameter words (`frame::owner_fns`).
pub type OwnerCtor = fn(&[u64]) -> OwnerFns;

/// A constructor over the descriptor and the words (lower.md §15.10.4 need 1: a lowered program's spec is the descriptor's — `layout`, `order_slot`, `topk`, `reps` — and its partition bits the word `bits` names).
pub type OwnerCtorDesc = fn(&OwnerDesc, &[u64]) -> OwnerFns;

/// A pipeline's owner phase, declared (milestone-2.md §2.6 items 12–13 and 31; ir-probe.md §12.3): `ctor` a kernel's constructor, `None` the generic phase over `layout` (`order_slot` `ORDER_NONE` / `ORDER_KEY` with `topk` the smallest key-0 records per owner, or a `Count` slot with `topk` = n:
/// the n best per owner by count desc, keys asc — `count_topk`); `bits = (word, shift)` the bits word.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OwnerDesc {
    pub layout: RecordLayout,
    /// The distinct form's round-2 record (`accumulator_of` / `accumulator_in` over `layout` and `pair`); `layout` outside the form.
    pub acc: RecordLayout,
    pub order_slot: u8,
    pub topk: u32,
    /// D3153: the generic runs form's bound (`runs_floor`): a record whose bounded final is past it leaves the owner (exec
    /// owner.rs); 0: none.
    pub floor: i64,
    pub ctor: Option<OwnerCtor>,
    pub ctor_desc: Option<OwnerCtorDesc>,
    pub bits: Option<(u8, u8)>,
    /// The count-ordered form's text keys (item 38): `(representative slot, code lane)` per `Fp128` key in key order, `REP_NONE` past the last; a count tie orders such a key by the rendered bytes.
    pub reps: [(u8, u8); MAX_REPS],
    /// Bit `t`: the sort names no order for text key `t`, so a count tie orders it by its fingerprint words and nothing is rendered to compare (`tie_reps`; the hand frame's `TIE_FP`). 0: the rendered bytes.
    pub tie_fp: u8,
    /// The distinct form (lower.md §6.12): `layout`'s trailing DISTINCT-argument key words (0: not the form).
    pub pair: u8,
    pub groups: u32, // the planner's group estimate (the round-2 partitioning); 0 outside the distinct form
    /// The group key's attribute of the driving relation whose zone extrema are the dense domain the pipeline's `WordsDesc.dense_lo / dense_hi` receive (`dense_key`); 0: the runner's default (the site's first attribute).
    pub dense_attno: i16,
    /// The distinct form's argument (M5 §4.5), set by `null_arg` / `arg_fold`: `(key word, bit)` of the pair's NULL flag (a flagged pair folds its riders and adds nothing to the pair count; `(NULL_ARG_NONE, _)` = never NULL), and the accumulator slots (`SumI64, SumI128, AvgI128, MinI64, MaxI64`) the argument's first key word folds into once per new pair (`sum / avg / min / max(DISTINCT x)`; `NULL_ARG_NONE` past the last).
    pub null_arg: (u8, u8),
    pub arg_fold: [u8; 3],
    /// D500 (`group_flags`): the accumulator's last key is the pair's flags word (`null_arg`'s) with the argument's bit
    /// cleared, after the group words (a nullable group key beside `count(DISTINCT)`); `pair` counts the argument's words.
    pub group_flags: bool,
    /// CAP-28 (`group_flag_words`): the group-flags form's `(the pair's first flags word, the flags words the accumulator
    /// carries)`: one per 64 group keys after the group words (`breaker::flag_words`), the argument's bit cleared where it
    /// falls in one of them; `group_flags` alone is `(null_arg's word, 1)`. `(0, 0)` outside the form.
    pub flag_words: (u8, u8),
    /// The count-ordered form's key check (V4R-113, D449's class): the group keys the projection before the cut computes
    /// integer arithmetic of must keep every step in range in every group, the cut ones too; `KeyCheck::NONE`: none.
    pub check: KeyCheck,
    /// The count form's order (`ordered_by`): the top's first sort word over the record (kernels `SK_SLOT | slot`, with
    /// `SK_DESC`, `SK_NULLS_FIRST`, `SK_FLOAT`), whose cell the emit's sort reads too; 0: the count descending.
    pub order: u16,
    /// The count form's record key word where its NULL flags words start (`breaker::flag_at`): a nullable word tie key
    /// orders NULLS LAST by its flag (kernels `reps::keys_better`); `TIE_FLAGS_NONE`: the keys carry no flags read.
    pub tie_flags: u8,
    /// SRC-38 slice 2: the generic form's owners fold key 0's least / greatest datum into `OwnerTop::key_lo` / `key_hi`
    /// (one integer key; exec owner.rs `OBSERVE_KEY_BIT`), which the pipeline's `Observe` proto remembers for the next
    /// execution's dense domain (exec statement.rs `Facts`): `OBSERVE_NONE`, `OBSERVE_KEY` (a NOT NULL key, the record's
    /// one key word) or `OBSERVE_KEY_FLAGGED` (a nullable key's `(value, flags)` image: a record whose flags word has bit 0
    /// set is the NULL group, not observed). A constructor's owner, or a pair owner, never observes.
    pub observe_key: u8,
    /// D3080: the count form's word tie keys (bit `i`: key `i`, below 24) that order by their decimal text under a byte-order
    /// collation, not as signed words: an integer's text keyed by the integer (kernels `reps::int_text_less`); 0: none.
    pub tie_text: u32,
    /// The tagged distinct form (`tagged`, lowerv4 ops/group/pairs_multi.rs): the pair's first DISTINCT word is a tag (0 ..
    /// N-1, one per count(DISTINCT) argument); a new pair of tag 0 counts into `ACC_PAIRS`, one of tag t > 0 into
    /// accumulator slot `tags + t - 1`, and only tag-0 pairs merge their riders. `NULL_ARG_NONE`: not tagged.
    pub tags: u8,
}

/// `OwnerDesc::observe_key`: no observation.
pub const OBSERVE_NONE: u8 = 0;
/// `OwnerDesc::observe_key`: key word 0, the record's one key word.
pub const OBSERVE_KEY: u8 = 1;
/// `OwnerDesc::observe_key`: key word 0 of a `(value, flags)` image, the NULL group (flags bit 0) skipped.
pub const OBSERVE_KEY_FLAGGED: u8 = 2;

/// `OwnerDesc::tie_flags` when no tie key reads a NULL flag.
pub const TIE_FLAGS_NONE: u8 = u8::MAX;

/// The group keys the projection before a count cut computes integer arithmetic of (V4R-113: `GROUP BY c, c - 1`,
/// `(a + 1) * 2`, `b * 3`, ...; NEW-46: `-c`, `c::int4 + 5`): chains of `+`, `-`, `*` with a constant, `-` and narrowing
/// int-to-int casts over one key each, every step exact and raising its result type's 22003 (smallint / integer / bigint
/// out of range) past `width` bytes; a NULL key raises nothing (the owner skips it by its NULL flag). Every folded record's keys must
/// keep each step inside the width, the records the cut drops too (PostgreSQL projects each group before its Sort). The
/// constants are words (a by-value Const is no part of the program's key): the bind reads them and pulls each step's
/// range back through its chain to an interval of its key (exec owner.rs `check_range`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct KeyCheck {
    /// Every step's result width in bytes: 2, 4 or 8 (0: no check).
    pub width: u8,
    /// The steps in evaluation order, as many as the projection's chains hold: a slice in the program's arena
    /// (`KeyCheck::in_arena`; the program's build copies it into its own, `copy_in`), empty for no check.
    pub steps: &'static [CheckStep],
}

/// One step of a key check: `op` of the value before it (the key `key` names, or the step before where `key` is 0) and
/// the constant at word `word`, sign-extended from `cwidth` bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CheckStep {
    /// The key word + 1 where the step starts a chain over that key, `| CHECK_KEY_NULLABLE` where the key can be NULL (NEW-46:
    /// its keys before it are one word each, so its NULL flag is bit `word` of the record's flags word); 0: the step reads
    /// the step before.
    pub key: u8,
    /// `CHECK_*`.
    pub op: u8,
    pub cwidth: u8,
    pub word: u16,
}

/// A key check's distinct keys: at most the record's inline key tier (`KEY_CHECK_KEYS` = `MAX_RECORD_KEYS`, eight): the
/// owner's words keep an interval per checked key and one word holds every checked key's word, a byte each (exec owner.rs
/// `P_CHECK_KEYS`). A check over more keys is no check (lowerv4 count_cut.rs `checks`: the generic owner and the emit's
/// sort). Its steps have no bound of their own (each constant's word index is a u16, the word plan's).
pub use gravity_abi::breaker::KEY_CHECK_KEYS;

pub const CHECK_NONE: u8 = 0;

/// `v + c`, `v - c`, `c - v`, `v * c` (either operand order of a sum or a product).
pub const CHECK_ADD: u8 = 1;

pub const CHECK_SUB: u8 = 2;

pub const CHECK_RSUB: u8 = 3;

pub const CHECK_MUL: u8 = 4;

/// NEW-46: `CheckStep::key`'s mark of a key that can be NULL: the owner skips a record whose NULL flag for it is set.
pub const CHECK_KEY_NULLABLE: u8 = 0x80;

/// NEW-46: `-v` (`int2um` / `int4um` / `int8um`), no constant.
pub const CHECK_NEG: u8 = 5;

/// NEW-46: a narrowing int-to-int cast of `v` to `width` bytes (`int84`, `int82`, `i4toi2`), no constant: `v` itself must
/// lie in the width.
pub const CHECK_CAST: u8 = 6;

impl CheckStep {
    pub const NONE: CheckStep = CheckStep { key: 0, op: CHECK_NONE, cwidth: 0, word: 0 };
}

impl KeyCheck {
    pub const NONE: KeyCheck = KeyCheck { width: 0, steps: &[] };

    /// A check at `width` of `steps`, the slice laid in `arena` (the program's lowering arena, which outlives every copy of
    /// the descriptor, as `Pipeline::consuming_in`'s slices do).
    pub fn in_arena(arena: ArenaRef, width: u8, steps: &[CheckStep]) -> KeyCheck {
        KeyCheck { width, steps: steps_in(arena, steps) }
    }

    /// This check with its steps copied into `arena` (`Program::build`: the program's own arena).
    pub fn copy_in(self, arena: ArenaRef) -> KeyCheck {
        KeyCheck { steps: steps_in(arena, self.steps), ..self }
    }
}

/// `steps` as a slice of `arena`.
fn steps_in(arena: ArenaRef, steps: &[CheckStep]) -> &'static [CheckStep] {
    if steps.is_empty() {
        return &[];
    }
    let mut v: ArenaVec<CheckStep> = ArenaVec::with_capacity_in(steps.len(), arena);
    v.extend_from_slice(steps);
    // SAFETY: the arena outlives every copy of the descriptor (the program's).
    unsafe { &*(v.leak() as *const [CheckStep]) }
}

pub use gravity_abi::breaker::MAX_REPS;
const _: () = assert!(MAX_REPS <= u8::BITS as usize, "tie_fp holds a bit per representative");

pub const REP_NONE: u8 = u8::MAX;

pub const NULL_ARG_NONE: u8 = u8::MAX;

/// `OwnerDesc::runs_floor`'s flag bit: the runs form is gated by a count floor (a flag, not a bound).
const RUNS_GATED: i64 = 1 << 32;
/// `OwnerDesc::runs_top`'s flag bit: the runs form's bound is each partition's n-th best count (a top's), not a K.
const RUNS_TOP: i64 = 1 << 33;
/// The longest top `runs_top` bounds: exec owner.rs selects each partition's n-th best count in an array this long.
pub const RUNS_TOP_HEAP: u32 = 256;

impl OwnerDesc {
    pub const fn generic(layout: RecordLayout, order_slot: u8, topk: u32) -> OwnerDesc {
        OwnerDesc { layout, acc: layout, order_slot, topk, floor: 0, ctor: None, ctor_desc: None, bits: None, reps: [(REP_NONE, 0); MAX_REPS], pair: 0, groups: 0, dense_attno: 0, tie_fp: 0, null_arg: (NULL_ARG_NONE, 0), arg_fold: [NULL_ARG_NONE; 3], group_flags: false, flag_words: (0, 0), check: KeyCheck::NONE, order: 0, tie_flags: TIE_FLAGS_NONE, observe_key: OBSERVE_NONE, tie_text: 0, tags: NULL_ARG_NONE }
    }

    /// The count-ordered form (`GROUP BY … ORDER BY count DESC[, keys] LIMIT n`'s owner).
    pub const fn count_topk(layout: RecordLayout, slot: u8, n: u32) -> OwnerDesc {
        assert!(slot < ORDER_KEY && n > 0);
        OwnerDesc { layout, acc: layout, order_slot: slot, topk: n, floor: 0, ctor: None, ctor_desc: None, bits: None, reps: [(REP_NONE, 0); MAX_REPS], pair: 0, groups: 0, dense_attno: 0, tie_fp: 0, null_arg: (NULL_ARG_NONE, 0), arg_fold: [NULL_ARG_NONE; 3], group_flags: false, flag_words: (0, 0), check: KeyCheck::NONE, order: 0, tie_flags: TIE_FLAGS_NONE, observe_key: OBSERVE_NONE, tie_text: 0, tags: NULL_ARG_NONE }
    }

    pub const fn count_topk_text(layout: RecordLayout, slot: u8, n: u32, reps: [(u8, u8); MAX_REPS]) -> OwnerDesc {
        assert!(slot < ORDER_KEY && n > 0);
        OwnerDesc { layout, acc: layout, order_slot: slot, topk: n, floor: 0, ctor: None, ctor_desc: None, bits: None, reps, pair: 0, groups: 0, dense_attno: 0, tie_fp: 0, null_arg: (NULL_ARG_NONE, 0), arg_fold: [NULL_ARG_NONE; 3], group_flags: false, flag_words: (0, 0), check: KeyCheck::NONE, order: 0, tie_flags: TIE_FLAGS_NONE, observe_key: OBSERVE_NONE, tie_text: 0, tags: NULL_ARG_NONE }
    }

    pub const fn named(layout: RecordLayout, ctor: OwnerCtor, bits: Option<(u8, u8)>) -> OwnerDesc {
        OwnerDesc { layout, acc: layout, order_slot: ORDER_NONE, topk: 0, floor: 0, ctor: Some(ctor), ctor_desc: None, bits, reps: [(REP_NONE, 0); MAX_REPS], pair: 0, groups: 0, dense_attno: 0, tie_fp: 0, null_arg: (NULL_ARG_NONE, 0), arg_fold: [NULL_ARG_NONE; 3], group_flags: false, flag_words: (0, 0), check: KeyCheck::NONE, order: 0, tie_flags: TIE_FLAGS_NONE, observe_key: OBSERVE_NONE, tie_text: 0, tags: NULL_ARG_NONE }
    }

    /// A declared constructor (`OwnerCtorDesc`): the count-ordered form's `order_slot` / `topk` / `reps` on the descriptor, `bits` the word the runner writes the partition bits into.
    pub const fn declared(layout: RecordLayout, ctor: OwnerCtorDesc, order_slot: u8, topk: u32, reps: [(u8, u8); MAX_REPS], bits: Option<(u8, u8)>) -> OwnerDesc {
        OwnerDesc { layout, acc: layout, order_slot, topk, floor: 0, ctor: None, ctor_desc: Some(ctor), bits, reps, pair: 0, groups: 0, dense_attno: 0, tie_fp: 0, null_arg: (NULL_ARG_NONE, 0), arg_fold: [NULL_ARG_NONE; 3], group_flags: false, flag_words: (0, 0), check: KeyCheck::NONE, order: 0, tie_flags: TIE_FLAGS_NONE, observe_key: OBSERVE_NONE, tie_text: 0, tags: NULL_ARG_NONE }
    }

    /// The distinct form: `order_slot` / `topk` as `count_topk_text` over the accumulator (`ORDER_NONE` / 0: every group). The distinct form over `layout` (the pair record: the group keys then `pair` DISTINCT-argument key words)
    /// and its round-2 record `acc` (`accumulator_of` for the 8-key tier, `accumulator_in` for the wide).
    pub const fn distinct(layout: RecordLayout, acc: RecordLayout, pair: u8, groups: u32, order_slot: u8, topk: u32, reps: [(u8, u8); MAX_REPS]) -> OwnerDesc {
        assert!(pair != 0 && pair as usize <= layout.nkeys() && (order_slot == ORDER_NONE) == (topk == 0));
        assert!(acc.nkeys() == layout.nkeys() - pair as usize, "the accumulator's keys are the group keys");
        OwnerDesc { layout, acc, order_slot, topk, floor: 0, ctor: None, ctor_desc: None, bits: None, reps, pair, groups, dense_attno: 0, tie_fp: 0, null_arg: (NULL_ARG_NONE, 0), arg_fold: [NULL_ARG_NONE; 3], group_flags: false, flag_words: (0, 0), check: KeyCheck::NONE, order: 0, tie_flags: TIE_FLAGS_NONE, observe_key: OBSERVE_NONE, tie_text: 0, tags: NULL_ARG_NONE }
    }

    /// D3153: `floor` for the generic runs form (lowerv4 twopass_having `owner_floor`): a record whose slot `slot` (a
    /// count, or an integer final read whole) is not over (`gt`: strictly) or at least K, or with `below` not under or at
    /// most K, leaves the owner, K the bound word `word` (an int4 when `int4`), read per execution.
    pub const fn runs_floor(word: u16, gt: bool, int4: bool, below: bool, slot: u8) -> i64 {
        RUNS_GATED | word as i64 | (gt as i64) << 16 | (int4 as i64) << 17 | (below as i64) << 18 | (slot as i64) << 24
    }

    /// `runs_floor`'s `(word, gt, int4, below, slot)`, when this descriptor is the generic runs form (no constructor, pair,
    /// top or count order) and carries one.
    pub const fn runs_floor_of(&self) -> Option<(u16, bool, bool, bool, u8)> {
        let f = self.floor;
        let runs = self.ctor.is_none() && self.ctor_desc.is_none() && self.pair == 0 && self.topk == 0 && self.order_slot >= ORDER_KEY;
        if !runs || f & RUNS_GATED == 0 {
            return None;
        }
        Some((f as u16, f >> 16 & 1 != 0, f >> 17 & 1 != 0, f >> 18 & 1 != 0, (f >> 24) as u8))
    }

    /// D-06: `floor` for the generic runs form under a top of `n` (LIMIT + OFFSET, WITH TIES or not) ordered first by the
    /// count in slot `slot`, descending (with `below`: ascending) (lowerv4 topn lower.rs `top_floor`): each partition
    /// lays the records whose count is at least its n-th largest (at most its n-th smallest; exec owner.rs `Best`, which
    /// may lay a few more). A partition holds a key's every row (its count is final), so a record past that bound has n
    /// others before it in the top's order whatever its later keys, and every record the top takes or ties with is laid.
    pub const fn runs_top(n: u32, below: bool, slot: u8) -> i64 {
        assert!(n >= 1 && n <= RUNS_TOP_HEAP);
        RUNS_TOP | (below as i64) << 18 | (slot as i64) << 24 | (n as i64) << 40
    }

    /// `runs_top`'s `(n, below, slot)`, when this descriptor is the generic runs form and carries one.
    pub const fn runs_top_of(&self) -> Option<(u32, bool, u8)> {
        let f = self.floor;
        let runs = self.ctor.is_none() && self.ctor_desc.is_none() && self.pair == 0 && self.topk == 0 && self.order_slot >= ORDER_KEY;
        if !runs || f & RUNS_TOP == 0 {
            return None;
        }
        Some(((f >> 40) as u32, f >> 18 & 1 != 0, (f >> 24) as u8))
    }

    /// The text keys the sort leaves unordered (`tie_fp`'s bits), declared by the lowering rung.
    pub const fn tie_fp(mut self, mask: u8) -> OwnerDesc {
        assert!((mask as u16) >> MAX_REPS == 0, "a tie bit past the text keys");
        self.tie_fp = mask;
        self
    }

    /// The representatives a count tie renders: `reps` without the `tie_fp` keys (`REP_NONE`: the words order it).
    pub const fn tie_reps(&self) -> [(u8, u8); MAX_REPS] {
        let mut r = self.reps;
        let mut t = 0;
        while t < MAX_REPS {
            if self.tie_fp >> t & 1 != 0 {
                r[t] = (REP_NONE, 0);
            }
            t += 1;
        }
        r
    }

    pub const fn null_arg(mut self, word: u8, bit: u8) -> OwnerDesc {
        assert!(self.pair != 0 && (word as usize) < self.layout.nkeys() && bit < 64, "the NULL flag is a key word of the distinct form's pair");
        self.null_arg = (word, bit);
        self
    }

    pub const fn arg_fold(mut self, slot: u8) -> OwnerDesc {
        assert!(self.pair != 0 && (slot as usize) < self.acc.nslots(), "the fold slot is an accumulator slot of the distinct form");
        let i = if self.arg_fold[0] == NULL_ARG_NONE { 0 } else if self.arg_fold[1] == NULL_ARG_NONE { 1 } else { 2 };
        assert!(self.arg_fold[i] == NULL_ARG_NONE, "at most three argument folds");
        self.arg_fold[i] = slot;
        self
    }

    /// D500: the group-flags form (`OwnerDesc::group_flags`'s field); `null_arg` names the flags word, the pair's last.
    pub const fn group_flags(mut self) -> OwnerDesc {
        assert!(self.null_arg.0 != NULL_ARG_NONE && self.null_arg.0 as usize + 1 == self.layout.nkeys(), "the group flags are the pair's last word");
        self.group_flags = true;
        self.flag_words = (self.null_arg.0, 1);
        self
    }

    /// CAP-28: the group-flags form over flags words `first..` of the pair (the argument's, `null_arg`'s, the last), the
    /// accumulator carrying `carried` of them after the group words (the group keys' own, one per 64: the argument's word
    /// is dropped when no group key's flag shares it).
    pub const fn group_flag_words(self, first: u8, carried: u8) -> OwnerDesc {
        let mut d = self.group_flags();
        let nk = d.layout.nkeys();
        assert!(first <= d.null_arg.0 && carried >= 1 && (first as usize + carried as usize) <= nk && (carried as usize) < d.acc.nkeys() + 1 && d.null_arg.0 as usize - (first as usize) < carried as usize + 1, "the carried flags words are the pair's, from its first");
        d.flag_words = (first, carried);
        d
    }

    /// The tagged distinct form (`OwnerDesc::tags`): tag t > 0 counts into slot `first + t - 1` of the accumulator.
    pub const fn tagged(mut self, first: u8) -> OwnerDesc {
        assert!(self.pair > 1 && self.topk == 0 && self.arg_fold[0] == NULL_ARG_NONE && (first as usize) < self.acc.nslots() && first >= ACC_SLOT_BASE, "a tagged distinct form: a tag word and an image, no top, no argument fold, its tag slots in the accumulator");
        self.tags = first;
        self
    }

    /// The count-ordered form's key check (`KeyCheck`).
    pub const fn key_check(mut self, check: KeyCheck) -> OwnerDesc {
        assert!(self.pair == 0 && self.order_slot < ORDER_KEY && matches!(check.width, 2 | 4 | 8), "a key check is the count form's, at an integer width");
        let (mut keys, mut nkeys, mut i, mut done) = ([0u8; KEY_CHECK_KEYS], 0, 0, false);
        while i < check.steps.len() {
            let s = check.steps[i];
            // the key word + 1 without its nullable mark (a mark on a step that starts no chain is no step's)
            let key = s.key & !CHECK_KEY_NULLABLE;
            if s.op == CHECK_NONE {
                done = true;
            } else {
                assert!(!done && s.op <= CHECK_CAST && matches!(s.cwidth, 2 | 4 | 8) && s.cwidth <= check.width && (key as usize) <= self.layout.nkeys() && (i > 0 || key > 0) && (key > 0 || s.key == 0), "a step reads a key word or the step before");
                let mut j = 0;
                while j < nkeys && keys[j] != key {
                    j += 1;
                }
                if key > 0 && j == nkeys {
                    assert!(nkeys < KEY_CHECK_KEYS, "a key check's distinct keys fit the owner's words");
                    keys[nkeys] = key;
                    nkeys += 1;
                }
            }
            i += 1;
        }
        assert!(nkeys > 0, "a key check has a step");
        self.check = check;
        self
    }

    /// The count form ordered by another slot word (`order`): any final the emit orders as a word, either direction; in the
    /// distinct form a slot of the accumulator.
    pub const fn ordered_by(mut self, word: u16) -> OwnerDesc {
        assert!(self.order_slot < ORDER_KEY && word > 0xff && word as u8 == self.order_slot, "an ordered count form names its own slot");
        self.order = word;
        self
    }

    /// The count form whose tie keys read their NULL flags from key word `first` on (a nullable word tie key, NULLS LAST).
    /// D3080: the count form's int-text tie keys (`tie_text`'s bits).
    pub const fn tie_text(mut self, mask: u32) -> OwnerDesc {
        assert!(self.pair == 0 && self.order_slot < ORDER_KEY && mask >> 24 == 0, "an int-text tie is the count form's, below key 24");
        self.tie_text = mask;
        self
    }

    pub const fn tie_flags(mut self, first: u8) -> OwnerDesc {
        assert!(self.pair == 0 && first != TIE_FLAGS_NONE && (first as usize) < self.layout.nkeys(), "the count form's flags are key words of its record");
        self.tie_flags = first;
        self
    }

    /// The distinct form's group key attribute (1-based) for the dense domain (exec `owner.rs::distinct_fns_dense`).
    pub const fn dense_key(mut self, attno: i16) -> OwnerDesc {
        assert!(self.pair != 0 && attno > 0, "the dense key is the distinct form's group key attribute");
        self.dense_attno = attno;
        self
    }

    /// The round-2 record: the group key words, `[Count, Count]` (the sentinel; the pairs), the pair's slots — the 8-key tier's (`accumulator_in` builds either tier).
    pub fn accumulator_of(pair: &RecordLayout, words: u8) -> RecordLayout {
        let nk = pair.nkeys() - words as usize;
        let implicit = pair.flags & LAYOUT_IMPLICIT_COUNT != 0;
        let ns = pair.nslots() - implicit as usize;
        let mut slots = [SlotKind::Count; MAX_RECORD_SLOTS];
        slots[ACC_SLOT_BASE as usize..ACC_SLOT_BASE as usize + ns].copy_from_slice(&pair.slot_kinds()[..ns]);
        let slots = &slots[..ACC_SLOT_BASE as usize + ns];
        if pair.has_validity() { RecordLayout::new(&pair.key_kinds()[..nk], slots) } else { RecordLayout::new_nn(&pair.key_kinds()[..nk], slots) }
    }

    /// `accumulator_of` for any tier: the wide tier's arrays in `arena`.
    pub fn accumulator_in(arena: ArenaRef, pair: &RecordLayout, words: u8) -> RecordLayout {
        let nk = pair.nkeys() - words as usize;
        let implicit = pair.flags & LAYOUT_IMPLICIT_COUNT != 0;
        let ns = pair.nslots() - implicit as usize;
        let mut slots: ArenaVec<SlotKind> = ArenaVec::with_capacity_in(ACC_SLOT_BASE as usize + ns, arena);
        slots.extend_from_slice(&[SlotKind::Count; ACC_SLOT_BASE as usize]);
        slots.extend_from_slice(&pair.slot_kinds()[..ns]);
        record_layout(arena, &pair.key_kinds()[..nk], &slots, pair.has_validity())
    }

    /// D500 (keyed.distinct_grouped): `accumulator_in`'s record, then one slot per DISTINCT fold (`sum / avg / min /
    /// max(DISTINCT x)`, the kinds `arg_fold` folds), validity on where a fold is (an extremum starts unset; a group of
    /// NULL arguments alone folds nothing, its fold NULL); with the first fold's slot.
    /// `flags` (`group_flags`): `words` counts the argument's words alone, the last key the pair's flags word.
    pub fn accumulator_folded(arena: ArenaRef, pair: &RecordLayout, words: u8, folds: &[SlotKind], flags: bool) -> (RecordLayout, u8) {
        let acc = Self::accumulator_in(arena, pair, words);
        if folds.is_empty() && !flags {
            return (acc, acc.nslots() as u8);
        }
        let mut keys: ArenaVec<KeyKind> = ArenaVec::with_capacity_in(acc.nkeys(), arena);
        keys.extend_from_slice(&acc.key_kinds()[..acc.nkeys() - flags as usize]);
        keys.extend(flags.then(|| pair.key_kinds()[pair.nkeys() - 1]));
        let mut slots: ArenaVec<SlotKind> = ArenaVec::with_capacity_in(acc.nslots() + folds.len(), arena);
        slots.extend_from_slice(acc.slot_kinds());
        slots.extend_from_slice(folds);
        (record_layout(arena, &keys, &slots, acc.has_validity() || !folds.is_empty()), acc.nslots() as u8)
    }

    pub const fn accumulator(&self) -> RecordLayout {
        self.acc
    }
}

pub(crate) struct WideNode {
    w: WideLayout,
    next: *const WideNode,
}

pub(crate) struct WideHead(*const WideNode);

// SAFETY: the nodes are leaked, immutable once linked, and the head is only read or written under `WIDE`'s lock.
unsafe impl Send for WideHead {}

pub(crate) static WIDE: pgsync::Mutex<WideHead> = pgsync::Mutex::new(WideHead(core::ptr::null()));

/// The wide tier's `WideLayout` interned per process, leaked once per shape: the code carries the pointer, so it hits the code cache in every session (D069).
pub(crate) fn intern_wide(w: WideLayout) -> &'static WideLayout {
    let mut head = WIDE.lock().unwrap_or_else(|e| e.into_inner());
    let mut p = head.0;
    while !p.is_null() {
        // SAFETY: every node is a leaked `Box`, written once before it is linked under the lock.
        let n = unsafe { &*p };
        if n.w == w {
            return &n.w;
        }
        p = n.next;
    }
    let keys: &'static [KeyKind] = Box::leak(w.keys.to_vec().into_boxed_slice()); // alloc: Cold
    let slots: &'static [SlotKind] = Box::leak(w.slots.to_vec().into_boxed_slice()); // alloc: Cold
    let slot_off: &'static [u32] = Box::leak(w.slot_off.to_vec().into_boxed_slice()); // alloc: Cold
    let w = WideLayout { keys, slots, slot_off, ..w };
    let n: &'static WideNode = Box::leak(Box::new(WideNode { w, next: head.0 })); // alloc: Cold
    head.0 = n;
    &n.w
}

/// A `RecordLayout` of any shape (decompose.md decision 82): the 8-key tier when `keys` / `slots` fit its arrays, else the wide tier — computed in `arena`, the `WideLayout` interned.
pub fn record_layout(arena: ArenaRef, keys: &[KeyKind], slots: &[SlotKind], validity: bool) -> RecordLayout {
    record_layout_at(arena, keys, slots, validity, gravity_abi::breaker::first_count(slots))
}

/// `record_layout` with the sentinel slot named (`RecordLayout::with_sentinel`'s rule; `None` appends the implicit one).
pub fn record_layout_at(arena: ArenaRef, keys: &[KeyKind], slots: &[SlotKind], validity: bool, sentinel: Option<usize>) -> RecordLayout {
    if gravity_abi::breaker::fits_inline_at(keys, slots, validity, sentinel) {
        return RecordLayout::with_sentinel(keys, slots, validity, sentinel);
    }
    let implicit = sentinel.is_none();
    let mut ks: ArenaVec<KeyKind> = ArenaVec::with_capacity_in(keys.len(), arena);
    ks.extend_from_slice(keys);
    let mut ss: ArenaVec<SlotKind> = ArenaVec::with_capacity_in(slots.len() + implicit as usize, arena);
    ss.extend_from_slice(slots);
    if implicit {
        ss.push(SlotKind::Count);
    }
    let n = ss.len();
    let mut offs: ArenaVec<u32> = ArenaVec::with_capacity_in(n, arena);
    offs.resize(n, 0);
    // SAFETY: the arena lives through this call; `intern_wide` copies the slices before the layout is returned.
    let (ks, ss, offs): (&'static [KeyKind], &'static [SlotKind], &'static mut [u32]) = unsafe { (&*(ks.leak() as *const [KeyKind]), &*(ss.leak() as *const [SlotKind]), &mut *(offs.leak() as *mut [u32])) };
    let w = intern_wide(WideLayout::compute_at(ks, ss, offs, validity, sentinel.unwrap_or(slots.len())));
    RecordLayout::wide(w, validity, implicit)
}

/// A finished pass-1 region's exported key words to the closure (`t3::export_keys`; `None`: not the export form).
pub type ExportKeys = unsafe fn(*const u8, &[u64], &mut dyn FnMut(u64)) -> Option<usize>;

/// A finished pass-1 region's granule marks `(key, part, bits)` to the closure (`t3::gmark_lists`; `None`: every granule admitted).
pub type GranuleMarks = unsafe fn(*const u8, &[u64], &mut dyn FnMut(u64, u64, u64)) -> Option<usize>;

/// The two-pass protocol on the admitting pipeline (milestone-2.md §2.6 item 15; ir-probe.md §19): `pass1` the exporting pipeline, its owner phase bound over the `words` parameter words from `base` (its `OwnerDesc.bits` word relative to it);
/// then the runner lays the keys `export` lists as the admitted set and cuts this pipeline's claims to `marks`' granules.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TwoPassDesc {
    pub pass1: u16,
    pub base: u8,
    pub words: u8,
    pub export: ExportKeys,
    pub marks: Option<GranuleMarks>,
}

/// The top-k run declared on its producing pipeline (lower.md §15.12; the ladder's rung 6 face "the walk's claim order declared on the program"): `n` the run's bound — the merge's `TopKPartial.n`, whatever columns the run carries (a
/// lowered run's columns are its own: the order key's image first, then the emitted cells) —; `walk_attno > 0` the zone walk: the driving site's claims are its granules in that attribute's key-minimum order, one granule per first claim,
/// the section stopped at the bound the workers' partials prove (ascending, NULLs last: the run's own order; `walk_desc` the key-maximum order, descending: a DESC run's key word is the NOT of the ascending one, so the walk orders by the NOT of each maximum and the bound compares as ascending; `walk_nulls_first` a NULLS FIRST run over a nullable key: every granule holding a NULL walks first and is never past a bound); `codes` / `secondary` the code columns the leader renders and the tie key it orders by (`merge::order_topk`), `code_lanes` their
/// site lanes. A program that declares none is the shape table's (`shapes::Walk`), unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TopKDesc {
    pub n: u32,
    pub walk_attno: i16,
    pub codes: u32,
    pub secondary: Option<u16>,
    /// The driving site's lane of each code column, a byte per set bit of `codes` in ascending column order (at most eight code columns): the leader resolves `(part, lane, code)`, and a declared run's column is not its lane.
    pub code_lanes: u64,
    pub walk_desc: bool,
    pub walk_nulls_first: bool,
}

impl TopKDesc {
    /// A bounded run over the source's own claim order.
    pub const fn run(n: u32) -> TopKDesc {
        TopKDesc { n, walk_attno: 0, codes: 0, secondary: None, code_lanes: 0, walk_desc: false, walk_nulls_first: false }
    }

    /// The zone walk of a NULLS FIRST run over a nullable key: a granule holding a NULL walks first (its NULL rows lead
    /// the run), and a cut run's NULL row n - 1 bounds the walk to those granules.
    pub const fn nulls_first(mut self) -> TopKDesc {
        assert!(self.walk_attno > 0, "a NULLS FIRST walk is a walk");
        self.walk_nulls_first = true;
        self
    }

    /// The zone walk in descending key order (`walking`'s attribute, maxima first).
    pub const fn descending(mut self) -> TopKDesc {
        assert!(self.walk_attno > 0, "a descending walk is a walk");
        self.walk_desc = true;
        self
    }

    /// The zone walk over `attno` (1-based, an attribute of the driving site's relation).
    pub const fn walking(mut self, attno: i16) -> TopKDesc {
        assert!(attno > 0, "the walk's key is an attribute of the driving relation");
        self.walk_attno = attno;
        self
    }

    /// The run's code columns (a bit per column), their site lanes (a byte per code column, ascending) and the tie key among them.
    pub const fn codes(mut self, codes: u32, code_lanes: u64, secondary: Option<u16>) -> TopKDesc {
        assert!(codes.count_ones() <= 8, "a declared run names at most eight code columns");
        assert!(codes & 1 == 0, "column 0 is the run's order key");
        self.codes = codes;
        self.code_lanes = code_lanes;
        self.secondary = secondary;
        self
    }
}

/// The parallel kernels' parameter words declared (milestone-2.md §2.6 item 18; ir-probe.md §20.3): each field the slot receiving the runner's fact — the zone walk's group bound, the group key's dense domain `(lo, hi)`, the pipeline's
/// partition bits, the key-order flag — or `NONE`; `nparams` the words' extent. The join's build takes the bits alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WordsDesc {
    pub topk: u16,
    pub dense_lo: u16,
    pub dense_hi: u16,
    pub bits: u16,
    pub order: u16,
    pub nparams: u16,
}

impl WordsDesc {
    pub const NONE: u16 = u16::MAX;

    pub const fn bits_only(bits: u16, nparams: u16) -> WordsDesc {
        WordsDesc { topk: WordsDesc::NONE, dense_lo: WordsDesc::NONE, dense_hi: WordsDesc::NONE, bits, order: WordsDesc::NONE, nparams }
    }

    /// The slots in the runner's fact order: top-k, dense lo, dense hi, bits, order.
    pub const fn slots(&self) -> [u16; 5] {
        [self.topk, self.dense_lo, self.dense_hi, self.bits, self.order]
    }
}
