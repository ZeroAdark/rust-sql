//! A two-pass group top's pass descriptions, the data the keyed hash end that folds its groups (agg's) carries for the
//! top's passes (topn's two-pass strategies): the export rule, pass 2's admission, which pass an end is. Their methods
//! stay with the strategies that build them (ops/sort/topn/strategies/twopass.rs).
use gravity_program::LifeId;

/// An export rule's shape (arg bits 0..47 and 63; HOSTS appends the bound word named by bits 48..62, ordinal + 1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) struct Rule {
    /// Floor: the bounded word (`count_word`: a count, or an integer final read whole) over (`gt`: strictly) or at least
    /// the bound word's K (an int4 when `int4`), with `below` under or at most K; else top n.
    pub floor: bool,
    pub gt: bool,
    pub int4: bool,
    pub below: bool,
    /// Top: the tie order on the key among the keys seen once (a nullable key's NULL flag is its image's second word).
    /// Extreme top (`ext`): the extreme's order instead (`desc`: max, the larger first; `nulls_first`: the NULL extreme).
    pub desc: bool,
    pub nulls_first: bool,
    pub nullable: bool,
    /// Extreme top (never with `floor`): the record's words after the count are the key's `max` / `min` of the order's
    /// column and, with `ext_nn`, its non-NULL rows (`count(v)`; without it every row's value is non-NULL).
    pub ext: bool,
    pub ext_nn: bool,
    /// Extreme top (gtopn GT-1): the record's word after the witness (and `count(v)`) is the key's BEST, a bound no group
    /// of it betters (its max under `desc`, else its min), where the witness is the other extreme (`min(v)` DESC,
    /// `max(v)` ASC, `avg(v)`); without it the witness is the best too (`max` DESC, `min` ASC).
    pub spread: bool,
    /// The exported image's words (1 or 2, the flags word included): what pass 2's probe compares.
    pub img_words: u8,
    /// The record's stride and its count slot, in words.
    pub rec_words: u8,
    pub count_word: u8,
    /// Top: count + offset.
    pub n: u32,
    /// D-06c count top (twopass.count; never with `floor` or `ext`): admit every record whose count ranks with the n-th
    /// best of all pass 1's records (`desc`: at least the n-th largest, else at most the n-th smallest), ties included.
    pub top_count: bool,
    /// 5c-F6 (with `top_count` only; arg bits 1, 2 and 63, the extreme top's, which a top-count rule never uses): what the
    /// lead word at `count_word` holds: `LEAD_COUNT` (a count, never NULL), else a sum's slot (`LEAD_I64` ..
    /// `LEAD_F32`), slot 0 of a record under the validity word (its last word; the sum NULL where bit 0 is clear), NULL
    /// placed by `nulls_first`. A float sum's record keeps its scaled absolute sum in the next word and its row count in
    /// the one after (topn twopass_count.rs `led`).
    pub lead: u8,
}

/// `Rule::lead`'s kinds: a count, then the sum slots a lead ranks by (abi `SlotKind::SumI64`, `SumI128`, `SumNum`,
/// `SumNumWide`, `SumF64`, `SumF32`).
pub(crate) const LEAD_COUNT: u8 = 0;
pub(crate) const LEAD_I64: u8 = 1;
pub(crate) const LEAD_I128: u8 = 2;
pub(crate) const LEAD_NUM: u8 = 3;
pub(crate) const LEAD_NUM_WIDE: u8 = 4;
pub(crate) const LEAD_F64: u8 = 5;
pub(crate) const LEAD_F32: u8 = 6;

/// The `Rule::lead` of a sum slot of `kind` (`None`: no sum a lead ranks by).
pub(crate) fn lead_of(kind: gravity_abi::breaker::SlotKind) -> Option<u8> {
    use gravity_abi::breaker::SlotKind as K;
    match kind {
        K::SumI64 => Some(LEAD_I64),
        K::SumI128 => Some(LEAD_I128),
        K::SumNum => Some(LEAD_NUM),
        K::SumNumWide => Some(LEAD_NUM_WIDE),
        K::SumF64 => Some(LEAD_F64),
        K::SumF32 => Some(LEAD_F32),
        _ => None,
    }
}

/// Pass 2's admission: the admitted set behind protocol word `word`, probed on pass 1's image of the group: group key
/// `key`'s words (the planner's pass 1 keys one of them; its NULL flag the second word when `null_word`) or, `None`, the
/// group's whole image (twopass.having: pass 1 is the same group).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Admit {
    pub word: u16,
    pub key: Option<usize>,
    pub null_word: bool,
    /// D3148 (join.domain): the end tests the every-key set (`ADMIT_ALL`) once a window and lowers a second row body
    /// without the test (`EndMech::every_guard`): a run whose build gave no set folds as a plain hash end does.
    pub every_window: bool,
}

/// D3326: the admitted set's words read once at a row loop's start (`Admit::live`), never per row: the header, the
/// prefilter's words and the shift that takes a hash's filter bits there (`emit_home_shift` of `filter_bits`, 6 to 16
/// in every laid set: kernels `admit_filter_bits`). The set behind the word is laid between pipelines, never during a
/// window.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AdmitLive {
    pub hdr: gravity_ir::Value,
    pub filter: gravity_ir::Value,
    pub shift: gravity_ir::Value,
}

/// D3326: an admitting hash end reads its set's header once a row loop (`Admit::live`) and tests a run's key once, at
/// the run's open, before any of its rows fold (`RunFold::admits_runs`). `PGRUST_GRAVITY_ADMIT_HOIST=0` (an A/B lever)
/// keeps the per-row header loads and the test at the run's close.
pub(crate) fn hoisted() -> bool {
    gravity_driver::env::num::<u8>("PGRUST_GRAVITY_ADMIT_HOIST").unwrap_or(1) != 0
}

/// A two-pass form's pass. Pass 1 never spills (its readers need the whole region; D254 grows it instead) and a split's
/// ends in its own slot (`Some`); pass 2 also consumes pass 1's slot from `LifeId`, so the region outlives pass 1.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Split {
    One(Option<u8>),
    Two(u8, LifeId),
    /// The planner pass 1 under a top rule (V4R-192): `One(None)`, its owner the rule's export.
    Top(Rule),
}
