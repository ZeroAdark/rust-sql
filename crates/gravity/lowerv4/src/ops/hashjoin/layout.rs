//! Where a build record's payload descriptor and NULL bits sit, for any payload width (landing 19).
//!
//! Up to 32 payload columns the layout is the fixed one of build.rs: the descriptor is `[u16; 32]` at `DESC` inside the
//! header, records start at `HDR`, and column c's NULL bit is bit 32 + c of the link word. Past 32 columns the
//! descriptor runs on past the header (records start at `base(pay)`, a 64-byte boundary) and the NULL bits of columns
//! 32.. take extra words after the payload words. Both are pure functions of the payload width, so the JIT (from the
//! Shape) and the runtime (from the header's `PAY`) agree without a stored offset. Only the plain form holds a wide
//! payload: the compact and counted forms pack their own bits into the link word.
use crate::ops::hashjoin::build::{DESC, HDR};

/// The widest payload a build holds: PostgreSQL's own target-list bound (MaxTupleAttributeNumber, htup_details.h), so a
/// payload PostgreSQL can form is never refused for width (CAP-05). The width is the header's `PAY` byte plus `PAY_HI`.
pub(crate) const MAX_PAY: usize = gravity_abi::limits::MAX_TUPLE_ATTRIBUTE_NUMBER;
/// The payload width's high byte (inside the 64 header bytes the driver zeroes; `pay_of` / `set_pay`).
pub(crate) const PAY_HI: usize = 55;
/// The words of a per-payload-column bit set (build_side.rs `Rel`).
pub(crate) const REL_WORDS: usize = MAX_PAY.div_ceil(64);

pub(crate) use crate::ops::hashjoin::raw::{pay_of, set_pay};
/// The payload columns whose NULL bits fit in the link word's high half.
pub(crate) const LINK_NULLS: usize = 32;

/// The byte offset of the first record of a build with `pay` payload columns.
pub(crate) const fn base(pay: usize) -> usize {
    if pay <= LINK_NULLS {
        HDR
    } else {
        (HDR + 2 * (pay - LINK_NULLS)).next_multiple_of(64)
    }
}

/// The byte offset in the region of payload column `c`'s descriptor slot.
pub(crate) const fn desc_at(c: usize) -> usize {
    if c < LINK_NULLS {
        DESC + 2 * c
    } else {
        HDR + 2 * (c - LINK_NULLS)
    }
}

/// The words a record carries past its payload for the NULL bits of columns 32.. .
pub(crate) const fn null_words(pay: usize) -> usize {
    pay.saturating_sub(LINK_NULLS).div_ceil(64)
}

/// Payload column `c`'s NULL bit in a record of `words` image words and `pay` payload columns: (byte offset, bit).
pub(crate) const fn null_at(words: usize, pay: usize, c: usize) -> (usize, u32) {
    if c < LINK_NULLS {
        (8 * words, (32 + c) as u32)
    } else {
        (8 * (words + 1 + pay + (c - LINK_NULLS) / 64), ((c - LINK_NULLS) % 64) as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrow_payload_keeps_the_fixed_layout() {
        for pay in 0..=32 {
            assert_eq!(base(pay), HDR);
            assert_eq!(null_words(pay), 0);
        }
        assert_eq!(desc_at(31), DESC + 62);
        assert!(desc_at(31) + 2 <= HDR, "the 32 narrow slots stay inside the header");
        assert_eq!(null_at(2, 5, 4), (16, 36), "link word of a two-word image, bit 32 + 4");
    }

    #[test]
    fn wide_payload_runs_past_the_header() {
        assert_eq!(base(33), HDR + 64);
        assert_eq!(base(64), HDR + 64);
        assert_eq!(base(65), HDR + 128);
        assert_eq!(base(MAX_PAY) % 64, 0);
        for pay in 33..=MAX_PAY {
            assert!(desc_at(pay - 1) + 2 <= base(pay), "every descriptor slot sits before the first record");
        }
        assert_eq!(desc_at(32), HDR);
        assert_eq!((null_words(33), null_words(96), null_words(97)), (1, 1, 2));
        // one image word, 40 payload columns: link at 8, payload words 16..336, the extra NULL word at 336
        assert_eq!(null_at(1, 40, 32), (8 * (1 + 1 + 40), 0));
        assert_eq!(null_at(1, 40, 39), (8 * (1 + 1 + 40), 7));
        assert_eq!(null_at(1, 200, 96), (8 * (1 + 1 + 200 + 1), 0));
        assert_eq!(null_at(1, MAX_PAY, MAX_PAY - 1), (8 * (1 + 1 + MAX_PAY + (MAX_PAY - 1 - LINK_NULLS) / 64), ((MAX_PAY - 1 - LINK_NULLS) % 64) as u32));
    }
}
