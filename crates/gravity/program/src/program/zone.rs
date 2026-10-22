//! Zone predicates: `ZonePred`, `ZoneOp` (split out of `program/mod.rs` by type, contract-runtime landing 3).

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ZoneOp {
    Lt,
    Le,
    Eq,
    Ge,
    Gt,
    NotNull,
    /// `lane <> value`: `NoneMatch` when every key is the value, `AllMatch` when the value is outside the range.
    Ne,
    /// `lane IS NULL`: `AllMatch` when the granule holds no non-NULL row, `NoneMatch` when every row is non-NULL; no value.
    IsNull,
    /// `lane = value` as one member of an IN list: a lane's `InMember` predicates are one disjunction (the list), which
    /// only the bloom answers (no row matches when every member is absent); each alone decides nothing (`Mixed`).
    InMember,
    /// P5-11: `lane = c` over a lane whose equality is byte equality (text under a deterministic collation; P5-13 a
    /// memcmp-ordered fixed image: uuid, macaddr, macaddr8), which only the bloom answers: `value` is c's probe word
    /// (`order_key::bloom::bloom_pack` of its equality bytes, the writer's hash), never a word or key of the lane, so the
    /// zone map, the PSMA, the row selection and the census pass it by (`Mixed`).
    HashEq,
    /// P5-11: `HashEq` as one member of an IN list: a lane's `HashIn` predicates are one disjunction, as `InMember`.
    HashIn,
}

/// A predicate the source may answer per window from its zone statistics (footer min/max/nonnull): `lane op value`, verdict in the lane's flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ZonePred {
    pub lane: u16,
    pub op: ZoneOp,
    /// SRC-09: `value` is the constant's 8-byte prefix key (`order_key::transform::memcmp_var_prefix_key`: a text lane
    /// compared under a collation that orders by bytes), not a word of the lane. The host answers it from a granule's
    /// Coarse record by strict comparisons only (a tie proves nothing); every road that reads `value` as the lane's word
    /// (the zone map, the bloom, the PSMA, the per-row filter, the census walk) pass it by. `false` for every other predicate
    /// (`ZonePred::word`), so a constructor that does not know the flag fails closed. P5-13: also a memcmp-ordered fixed
    /// image's order key (uuid's 8-byte prefix, Coarse; macaddr's and macaddr8's whole image, Exact: answered as a word's
    /// key against an Exact record), whose lane's word is a pointer.
    pub prefix: bool,
    /// SRC-21: the plain fold's counted predicate (a constant-free one: `x IS NOT NULL` over an integral lane the site
    /// answers, lowerv4 answer_counted.rs): its passing rows are the lane's granule record's tally (`nonnull`), exact in
    /// any window. The host answers a window whose every other predicate
    /// is `AllMatch` whether this one is `AllMatch` or `Mixed` (`NoneMatch` still skips); every other reader takes it as
    /// the plain predicate. `false` for every constructor, so a predicate is tallied only where the site rule marks it.
    pub tallied: bool,
    /// NEW-72: the constant is the execution's (a literal's word, never in the program or its key): `value` holds
    /// `bound_slot(word, typlen)` until the host's bind writes the execution's value (src.verdicts `word_zone_at_arg`). A
    /// host that has no value for it takes no zone verdict in that execution. `false` for every other predicate.
    pub bound: bool,
    pub value: i64,
}

impl ZonePred {
    pub const NONE: ZonePred = ZonePred::word(0, ZoneOp::NotNull, 0);

    /// `lane op value` over the lane's word: every predicate but SRC-09's.
    pub const fn word(lane: u16, op: ZoneOp, value: i64) -> ZonePred {
        ZonePred { lane, op, prefix: false, tallied: false, bound: false, value }
    }

    /// SRC-09: `lane op c` over a text lane, `key` = c's 8-byte prefix key.
    pub const fn prefix(lane: u16, op: ZoneOp, key: i64) -> ZonePred {
        ZonePred { lane, op, prefix: true, tallied: false, bound: false, value: key }
    }

    /// NEW-72: `lane op c` whose constant `c` (of typlen `len`) is bound per execution from word `word`.
    pub const fn bound(lane: u16, op: ZoneOp, word: u16, len: u8) -> ZonePred {
        ZonePred { lane, op, prefix: false, tallied: false, bound: true, value: bound_slot(word, len) }
    }

    /// NEW-72: `lane op c` over an order key (`prefix`): a by-reference constant `c` bound per execution from word `word`,
    /// its key made by the host from the image in form `form` (`BOUND_TEXT_PREFIX`, `BOUND_FIXED_PREFIX` | its length).
    pub const fn bound_prefix(lane: u16, op: ZoneOp, word: u16, form: u8) -> ZonePred {
        ZonePred { lane, op, prefix: true, tallied: false, bound: true, value: bound_slot(word, form) }
    }

    /// NEW-72: `(word, typlen)` of a bound predicate's constant (`bound`), before the host binds its value.
    pub const fn bound_of(&self) -> Option<(u16, u8)> {
        if self.bound { Some((self.value as u16, (self.value >> 16) as u8)) } else { None }
    }

    /// P5-11: `lane = c` (`HashEq`) or an IN member (`HashIn`) over a byte-equality lane, `packed` = c's probe word.
    pub const fn hashed(lane: u16, op: ZoneOp, packed: i64) -> ZonePred {
        ZonePred { lane, op, prefix: false, tallied: false, bound: false, value: packed }
    }

    /// P5-11: a predicate only the bloom answers by its probe word (`HashEq` / `HashIn`).
    pub const fn is_hashed(&self) -> bool {
        matches!(self.op, ZoneOp::HashEq | ZoneOp::HashIn)
    }
}

/// NEW-72: a bound predicate's form past the by-value lengths 1 / 2 / 4 / 8 (`ZonePred::bound`'s `len`): how the host
/// makes the predicate's value from the execution's word, a pointer to the constant's image (a varlena's payload, or the
/// image of a fixed length): SRC-09's prefix key of a text payload under a bytes-order collation
/// (`order_key::transform::memcmp_var_prefix_key`).
pub const BOUND_TEXT_PREFIX: u8 = 0x20;
/// The probe word of a text payload (`order_key::bloom::bloom_pack`, the writer's hash): P5-11's `HashEq`.
pub const BOUND_TEXT_HASH: u8 = 0x21;
/// `| len`: the order key of a memcmp-ordered fixed image of `len` bytes (at most 16): P5-13's prefix key.
pub const BOUND_FIXED_PREFIX: u8 = 0x40;
/// `| len`: the probe word of a fixed image of `len` bytes (at most 16): P5-13's `HashEq`.
pub const BOUND_FIXED_HASH: u8 = 0x80;

/// NEW-72: a bound predicate's placeholder value: its word and typlen, so two bound predicates are equal exactly when
/// they read one word.
const fn bound_slot(word: u16, len: u8) -> i64 {
    word as i64 | (len as i64) << 16
}
