//! Part<<32|code representatives in records. Hooks / levers: H10 R3-L10. Budget 160 lines (files.tsv, +25 % gate).
//! Owner M1-TABLES.
//!
//! A text key rides its record as `part << 32 | code` (a MinCode slot the grouping forms write at the record's open: M1-KEYED's
//! `rep_words`), the record keyed by the value's image (`image_layout`, the record every keyed table hands its end), and
//! merged by `Rec::fold`'s MinCode arm at the owner; the render happens once per surviving group (late_render.rs).
//!
//! Registers: road `code.carry` (`CODE_CARRY`).
use gravity_abi::breaker::{KeyKind, RecordLayout, SlotKind};
use gravity_decompose::ops::shared::agg::KeyDecl;
use gravity_decompose::catalog::typefacts::facts_of;
use gravity_kernels::host_abi::{SK_BPCHAR, SK_HEAP, SK_TEXT, SK_TEXT_COLL_SHIFT, SK_TEXT_SLOT_SHIFT};
use gravity_program::{ArenaRef, Rep};
use mcx::{CollectIn, GrowIn, Mcx, PgVec};

use crate::graph::Gx;

use crate::table::emit_group::RepSrc;

crate::strategies! {
    /// Part<<32|code representatives in records.
    CODE_CARRY = 44, "code.carry", CodeKeyed;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(CODE_CARRY, &())], ..crate::ops::NONE };

/// Each key's record kind and words (`image_layout`'s `keys`) in pipeline `producer`'s tables, for a group or DISTINCT key
/// set whose images decide equality: the key image face's widths (face/keyimg.rs `record_words`: decompose's, or two Fp128
/// words under a nondeterministic collation, D823, and for a narrow numeric over a varlena cell, M6-REPS batch B).
pub(crate) fn key_kinds<'g>(gx: &Gx<'g, '_>, producer: usize, keys: &[KeyDecl<'_>]) -> PgVec<'g, (KeyKind, u8)> {
    keys.iter().map(|k| crate::table::keyimg::record_words(gx, producer, k)).collect_in(gx.mcx)
}

/// D767: a bpchar key over a code lane (`reps[i]`: rep_src's representative column) whose dictionary can hold values
/// `bpchareq` calls equal as distinct entries: a column of no declared length (typmod -1: 'a' and 'a  ' are two entries of
/// one part), or a bpchar key derived from the lane (a cast, key.codeif's ELSE literal). Its code, in-part rank and the
/// Fp128 of its padded render each tell trim-equal values apart, so the key refuses by name until the bpchar Fp128 table
/// (hashbpchar's trimmed image; M6 D767 (2)). A char(n) lane pads every entry to n: trim-equal entries are one entry.
/// D1353: a bpchar key over a lane of another type (`v::bpchar` over a varchar / text column, the relabel decompose strips,
/// D1309) keys its lane's plain Fp128 table (pgrc.rs declares the trimmed one for a bpchar column only), so 'a' and
/// 'a ' were two groups (a semi-join's unique-ification counted a row twice): it refuses by name too.
pub(crate) fn coarse_codes(gx: &Gx<'_, '_>, producer: usize, node: i32, keys: &[KeyDecl<'_>], reps: &[Option<RepSrc>]) {
    let Some((_, relid, _)) = gx.pipelines()[producer].source.relation() else { return };
    for (k, r) in keys.iter().zip(reps.iter()) {
        let Some(r) = r.as_ref().filter(|r| matches!(r.rep, Rep::Code { .. })) else { continue };
        let att = gx.facts.att(relid, r.attno);
        if sk_bpchar(gx, k.tf) != 0 && att.is_some_and(|a| sk_bpchar(gx, a.typid) == 0) {
            crate::admit::refusal::refuse_native(node, relid, crate::admit::refusal::ShapeWhy(BPCHAR_KEY_OVER_OTHER_TYPE));
        }
        if sk_bpchar(gx, k.tf) != 0 && (r.derived || att.is_none_or(|a| a.typmod < 0)) {
            crate::admit::refusal::refuse_native(node, relid, crate::admit::refusal::ShapeWhy(BPCHAR_OVER_CODE_LANE));
        }
    }
}

/// D767: a bpchar text key's sort word bit (`SK_BPCHAR`: agg_sort trims both renders first, as bpcharcmp), else 0.
pub(crate) fn sk_bpchar(gx: &Gx<'_, '_>, t: u32) -> u64 {
    sk_bpchar_of(facts_of(gx.facts, t).as_ref())
}

/// `sk_bpchar` over a type's facts (D1309: every SK_TEXT word, a Code render's or a kept copy's, takes it).
pub(crate) fn sk_bpchar_of(tf: Option<&gravity_decompose::catalog::typefacts::TypeFacts>) -> u64 {
    if tf.is_some_and(|f| f.image.is_bpchar()) { SK_BPCHAR } else { 0 }
}

/// A kept key copy's sort word (L0-l): its slot's datum compared as text under `coll`, `bpchar` (keyed_coderep
/// `sk_bpchar`: SK_BPCHAR for a bpchar key, else 0) trimming both renders first as bpcharcmp (D1309: without it a bpchar
/// copy compared with its trailing blanks, 'a ' after 'a' || chr(1)).
pub(crate) fn copy_word(slot: u64, coll: u32, bpchar: u64) -> u64 {
    SK_TEXT | SK_HEAP | slot << SK_TEXT_SLOT_SHIFT | (coll as u64) << SK_TEXT_COLL_SHIFT | bpchar
}

/// The refusals of a bpchar key or extreme that no code form serves exactly: over a lane of another type (D1353), and
/// over a lane whose renders are not bpchar's trimmed, typmod-padded values (D767; D3976: a key derived from the lane,
/// `COALESCE(c_tier, 'none')` through a set operation, met it as an internal-invariant refusal). Both are shape gaps
/// until the trimmed bpchar key image (M6 D767 (2)), so PostgreSQL's executor answers; the texts name their bugs.
pub(crate) const BPCHAR_KEY_OVER_OTHER_TYPE: &str = "D1353: a bpchar key over a code lane of another type (a relabel to bpchar: the lane's Fp128 is not hashbpchar's trimmed image)";
pub(crate) const BPCHAR_OVER_CODE_LANE: &str = "D767: a bpchar key or extreme over a dictionary code lane (the code, its rank and its padded render are not bpchar's equality or order)";

/// The owner record of a keyed table over the key image: `record` (decompose's) rekeyed to the image's words. `keys[i]` is
/// key i's decompose kind and its image words (`ImageWords` in key order: 1 or 2); the flags words (one per 64 keys,
/// `breaker::flag_words`) follow when `nullable` (keyimg.rs: `emit_image` appends them when any key is nullable, CAP-28).
/// The slots, the validity word and the sentinel slot are `record`'s. Every key word is a partition word, so a record's
/// route (`Rec::route`, `KeyHash::of` its key words) is the image's `emit_key_hash` word: the IR and the runtime home and
/// route a key alike (D250).
///
/// A word's kind names how the owner reads it back: a one-word image of a text key is a code of one dictionary
/// (`part << 32 | code`, `KeyKind::Code`); a two-word image keeps the pair's kind (`Fp128` low then high, or a fixed
/// type's `I64` pair); a one-word integer image is the unified word (`I64`); each flags word is an `I64`.
pub(crate) fn image_layout(arena: ArenaRef, record: &RecordLayout, keys: &[(KeyKind, u8)], nullable: bool, extra: &[SlotKind], validity: bool, mcx: Mcx<'_>) -> RecordLayout {
    let mut kinds: PgVec<'_, KeyKind> = mcx::reserved_in(2 * keys.len() + gravity_abi::breaker::flag_words(keys.len()), mcx);
    for &(kind, words) in keys {
        debug_assert!(words == 1 || words == 2, "a key image word count is 1 or 2 (keyimg.rs)");
        let k = match (kind, words) {
            (KeyKind::Fp128, 1) => KeyKind::Code,
            (KeyKind::I32, 1) => KeyKind::I64,
            (k, _) => k,
        };
        kinds.extend_in(core::iter::repeat_n(k, words as usize));
    }
    if nullable {
        kinds.extend_in(core::iter::repeat_n(KeyKind::I64, gravity_abi::breaker::flag_words(keys.len())));
    }
    let sentinel = (record.flags & gravity_abi::breaker::LAYOUT_IMPLICIT_COUNT == 0).then(|| record.sentinel_slot());
    let slots = record.slot_kinds();
    // An implicit sentinel is the last of `slot_kinds()`: `record_layout_at` appends it again from `None`.
    let slots = if sentinel.is_none() { &slots[..slots.len() - 1] } else { slots };
    // D472 (keyed/order_slot.rs): a top's order-expression slots after the aggregates' (before an implicit sentinel)
    let slots: PgVec<'_, SlotKind> = slots.iter().chain(extra).copied().collect_in(mcx);
    // bundle 15 B: `validity` is decompose's (`record.has_validity()`) or keyed/order_slot.rs `wants_validity`'s
    gravity_program::record_layout_at(arena, &kinds, &slots, validity, sentinel)
}

#[cfg(test)]
mod tests {
    use gravity_abi::breaker::{KeyKind, RecordLayout, SlotKind, LAYOUT_IMPLICIT_COUNT};

    use super::image_layout;

    #[test]
    fn a_text_key_as_one_code_word_and_a_nullable_int_key_keep_the_slots_and_the_sentinel() {
        // GROUP BY text_col, int_col: decompose's record is [Fp128, Fp128, I32] with count(*) as its sentinel.
        let rec = RecordLayout::with_sentinel(&[KeyKind::Fp128, KeyKind::Fp128, KeyKind::I32], &[SlotKind::SumI64, SlotKind::Count], true, Some(1));
        let ctx = mcx::MemoryContext::new("test");
        let img = image_layout(gravity_program::ArenaRef::EMPTY, &rec, &[(KeyKind::Fp128, 1), (KeyKind::I32, 1)], true, &[], rec.has_validity(), ctx.mcx());
        assert_eq!(img.key_kinds(), &[KeyKind::Code, KeyKind::I64, KeyKind::I64]);
        assert_eq!(img.slot_kinds(), rec.slot_kinds());
        assert_eq!((img.sentinel_slot(), img.has_validity(), img.partition_keys()), (1, true, 3));
        assert_eq!(img.size(), rec.size());
    }

    /// D1168: set operations (setop/setop.rs `flags_index`) read a keyed record's NULL flags word at the sum of its keys'
    /// record words, `flags_word`'s index; this layout is the recording setop's test pins that index to.
    #[test]
    fn a_nullable_mixed_width_image_puts_its_flags_word_after_every_image_word() {
        let rec = RecordLayout::with_sentinel(&[KeyKind::Fp128, KeyKind::Fp128, KeyKind::I64, KeyKind::I64, KeyKind::I64, KeyKind::Fp128, KeyKind::Fp128], &[SlotKind::Count], true, Some(0));
        let words = [(KeyKind::Fp128, 2), (KeyKind::I64, 1), (KeyKind::I64, 2), (KeyKind::Fp128, 2)];
        let ctx = mcx::MemoryContext::new("test");
        let img = image_layout(gravity_program::ArenaRef::EMPTY, &rec, &words, true, &[], rec.has_validity(), ctx.mcx());
        let recorded = [KeyKind::Fp128, KeyKind::Fp128, KeyKind::I64, KeyKind::I64, KeyKind::I64, KeyKind::Fp128, KeyKind::Fp128, KeyKind::I64];
        assert_eq!(img.key_kinds(), &recorded);
        let flags_word: usize = words.iter().map(|&(_, w)| w as usize).sum(); // hashagg_rec.rs `flags_word`: the wpk sum
        assert_eq!((flags_word, img.key_off(flags_word)), (7, 56));
    }

    /// CAP-28: past 64 nullable keys the record carries one flags word per 64 keys after its image words (the image's
    /// `emit_image`), here 130 keys of one word: three `I64` flags words, the slots and sentinel kept.
    #[test]
    fn a_nullable_image_past_64_keys_takes_one_flags_word_per_64_keys() {
        let arena = gravity_program::Arena::new(1 << 16);
        let rec = RecordLayout::with_sentinel(&[KeyKind::I64], &[SlotKind::Count], true, Some(0));
        let ctx = mcx::MemoryContext::new("test");
        let img = image_layout(arena.handle(), &rec, &[(KeyKind::I64, 1); 130], true, &[], rec.has_validity(), ctx.mcx());
        assert_eq!(img.nkeys(), 133, "130 image words, then three flags words");
        assert!(img.key_kinds()[130..].iter().all(|&k| k == KeyKind::I64));
        assert_eq!((img.slot_kinds(), img.sentinel_slot()), (rec.slot_kinds(), 0));
    }

    #[test]
    fn an_implicit_sentinel_is_not_appended_twice() {
        let rec = RecordLayout::with_sentinel(&[KeyKind::I64], &[SlotKind::MaxI64], true, None);
        assert!(rec.flags & LAYOUT_IMPLICIT_COUNT != 0);
        let ctx = mcx::MemoryContext::new("test");
        let img = image_layout(gravity_program::ArenaRef::EMPTY, &rec, &[(KeyKind::Fp128, 2)], false, &[], rec.has_validity(), ctx.mcx());
        assert_eq!(img.key_kinds(), &[KeyKind::Fp128, KeyKind::Fp128]);
        assert_eq!(img.slot_kinds(), rec.slot_kinds());
        assert!(img.flags & LAYOUT_IMPLICIT_COUNT != 0);
    }

    #[test]
    fn an_order_slot_validity_word_keeps_the_slot_indexes() {
        // bundle 15 B: GROUP BY k ORDER BY sum(v) - max(v) LIMIT n over NOT NULL v: decompose's record has no validity; the
        // order expression's MaxI64 slot is NULL-able (keyed/order_slot.rs `wants_validity`), so the image layout takes a word
        let rec = RecordLayout::with_sentinel(&[KeyKind::I64], &[SlotKind::SumI64, SlotKind::MaxI64], false, None);
        let ctx = mcx::MemoryContext::new("test");
        let at = |v: bool| image_layout(gravity_program::ArenaRef::EMPTY, &rec, &[(KeyKind::I64, 1)], false, &[SlotKind::MaxI64], v, ctx.mcx());
        let (plain, valid) = (at(false), at(true));
        assert!(!plain.has_validity() && valid.has_validity());
        assert_eq!(valid.slot_kinds(), plain.slot_kinds(), "the same slots, in the same order");
        assert_eq!((valid.nslots(), valid.sentinel_slot()), (plain.nslots(), plain.sentinel_slot()));
        let bits: Vec<_> = (0..valid.nslots()).map(|j| valid.valid_bit(j)).collect();
        let mut dedup = bits.clone();
        dedup.dedup();
        assert_eq!(dedup.len(), bits.len(), "one bit per slot");
        assert!(valid.size() > plain.size(), "the word adds to the record");
    }
}
