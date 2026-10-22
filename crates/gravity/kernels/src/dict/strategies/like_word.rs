//! Road `text.memmem`'s word form (lowerv4 step/text_like.rs): a qual's LIKE pattern is its bind word, so the program is
//! the same for every pattern (V4R-18). Once per pipeline call the IR hands the word here; the pattern's class
//! (`gravity_dict::like`'s, PG's `\` escape) comes back as the class word and its literal goes into the IR's slot, which
//! the row test reads. Any other pattern, a NULL one, or an image not stored inline answers `WORD_COLD`: the rows take
//! PG's call. Owner dict.
use gravity_abi::Ctx;
use gravity_dict::like::{Class, Matcher, Mode};

use crate::contract::Host;

/// The class word's low byte (the literal's length above it). 0 is cold, so a word no call wrote reads as cold too.
pub const WORD_COLD: u64 = 0;
pub const WORD_EXACT: u64 = 1;
pub const WORD_PREFIX: u64 = 2;
pub const WORD_SUFFIX: u64 = 3;
pub const WORD_CONTAINS: u64 = 4;
pub const WORD_ANY: u64 = 5;
/// A flag beside the class: the literal is longer than the slot, which holds its window (the first `WORD_LIT_BYTES`
/// bytes, a suffix's last) and the class word the whole literal's length. A row the window rules out answers; a row
/// where the window occurs takes PG's call.
pub const WORD_WINDOW: u64 = 0x80;
/// The literal's slot: eight words (the inline literals' longest, text_like.rs); a longer literal's window.
pub const WORD_LIT_BYTES: usize = 64;

/// `pattern`'s class word with its literal written to the front of `out`: `WORD_COLD` for a pattern the row test does
/// not take (`_`, an inner `%`, a trailing escape). A literal longer than `out` writes its window (`WORD_WINDOW`).
pub fn class_word(pattern: &[u8], out: &mut [u8]) -> u64 {
    let mut scratch = [0u64; WORD_LIT_BYTES / 8];
    // an unescaped literal past the slot needs a region as long as the pattern (once per call)
    let mut long = if pattern.len() > WORD_LIT_BYTES { vec![0u64; pattern.len().div_ceil(8)] } else { Vec::new() }; // alloc: Cold
    let words: &mut [u64] = if long.is_empty() { &mut scratch } else { &mut long };
    // SAFETY: the region is `words`' own 8-aligned words, which outlive the matcher
    let mut region = unsafe { gravity_dict::Region::new(words.as_mut_ptr().cast(), words.len() * 8) };
    let Ok(m) = Matcher::new(pattern, Mode::Like, &mut region) else { return WORD_COLD };
    let cls = match m.class() {
        Class::Any => return WORD_ANY,
        Class::General | Class::Segments => return WORD_COLD,
        Class::Exact => WORD_EXACT,
        Class::Prefix => WORD_PREFIX,
        Class::Suffix => WORD_SUFFIX,
        Class::Contains => WORD_CONTAINS,
    };
    let lit = m.literal();
    let window = match (lit.len() > out.len(), cls) {
        (false, _) => lit,
        (true, WORD_SUFFIX) => &lit[lit.len() - out.len()..],
        (true, _) => &lit[..out.len()],
    };
    out[..window.len()].copy_from_slice(window);
    let flag = if window.len() < lit.len() { WORD_WINDOW } else { 0 };
    cls | flag | (lit.len() as u64) << 8
}

/// The payload of the varlena at `p` when it is stored inline (a 1-byte or plain 4-byte header).
/// # Safety — `p` is a live varlena.
unsafe fn inline<'a>(p: *const u8) -> Option<&'a [u8]> {
    match *p {
        h if h & 1 == 1 && h != 1 => Some(core::slice::from_raw_parts(p.add(1), (h >> 1) as usize - 1)),
        h if h & 3 == 0 => Some(core::slice::from_raw_parts(p.add(4), (u32::from_le_bytes(*(p as *const [u8; 4])) >> 2) as usize - 4)),
        _ => None,
    }
}

/// The word form's per-call classification (`contract::entry2::<LikeWord>`): `(pattern, out)`, the pattern's bind word
/// (0: NULL) and the literal's slot of `WORD_LIT_BYTES`; answers the class word. A panic answers `WORD_COLD`.
pub struct LikeWord;

impl Host for LikeWord {
    type Args = (u64, u64);
    /// # Safety
    /// `pattern` is 0 or a live text varlena; `out` is `WORD_LIT_BYTES` writable bytes.
    unsafe fn run(_ctx: *mut Ctx, (pattern, out): (u64, u64)) -> u64 {
        let out = core::slice::from_raw_parts_mut(out as usize as *mut u8, WORD_LIT_BYTES);
        out.fill(0);
        match pattern {
            0 => WORD_COLD,
            p => inline(p as usize as *const u8).map_or(WORD_COLD, |pat| class_word(pat, out)),
        }
    }

    fn fail(_a: (u64, u64)) -> u64 {
        WORD_COLD
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::entry2;

    fn word(p: &str) -> (u64, String) {
        let mut out = [0u8; WORD_LIT_BYTES];
        let w = class_word(p.as_bytes(), &mut out);
        (w & 0xff, String::from_utf8(out[..(w >> 8) as usize].to_vec()).unwrap())
    }

    /// PG's LIKE classes with `\` unescaped into the literal; `_`, an inner `%` and a trailing escape are cold.
    #[test]
    fn patterns_classify_into_the_class_word() {
        let lit = |c: u64, s: &str| (c, s.to_string()); // alloc: Cold
        assert_eq!(word("abc"), lit(WORD_EXACT, "abc"));
        assert_eq!(word(""), lit(WORD_EXACT, ""));
        assert_eq!(word("PROMO%"), lit(WORD_PREFIX, "PROMO"));
        assert_eq!(word("%%x"), lit(WORD_SUFFIX, "x"));
        assert_eq!(word("%ab%%"), lit(WORD_CONTAINS, "ab"));
        assert_eq!(word("%"), lit(WORD_ANY, ""));
        assert_eq!(word("%%"), lit(WORD_ANY, ""));
        assert_eq!(word(r"50\%%"), lit(WORD_PREFIX, "50%"));
        assert_eq!(word(r"a\_b"), lit(WORD_EXACT, "a_b"));
        assert_eq!(word(r"a\\"), lit(WORD_EXACT, r"a\"));
        assert_eq!(word("%日本%"), lit(WORD_CONTAINS, "日本"));
        assert_eq!(word(&"x".repeat(64)), lit(WORD_EXACT, &"x".repeat(64)));
        for general in ["a_c", "a%c", "%a%b%", r"ab\", "_"] {
            assert_eq!(word(general).0, WORD_COLD, "{general}");
        }
    }

    /// B2-23 / CAP-24: a literal past the slot is its class with `WORD_WINDOW`, the whole literal's length above the low
    /// byte and its window in the slot: a prefix's, an exact's and an infix's first `WORD_LIT_BYTES` bytes, a suffix's
    /// last; one of exactly `WORD_LIT_BYTES` takes no flag, an escaped one unescapes past the 64-byte scratch.
    #[test]
    fn a_literal_past_the_slot_writes_its_window() {
        let window = |p: &str| {
            let mut out = [0u8; WORD_LIT_BYTES];
            let w = class_word(p.as_bytes(), &mut out);
            (w & 0x7f, w & WORD_WINDOW, w >> 8, String::from_utf8(out.to_vec()).unwrap())
        };
        let lit = format!("{}{}", "h".repeat(WORD_LIT_BYTES), "t".repeat(7)); // alloc: Cold
        let (head, tail) = ("h".repeat(WORD_LIT_BYTES), format!("{}{}", "h".repeat(WORD_LIT_BYTES - 7), "t".repeat(7))); // alloc: Cold
        let n = lit.len() as u64;
        assert_eq!(window(&format!("{lit}%")), (WORD_PREFIX, WORD_WINDOW, n, head.clone()), "prefix"); // alloc: Cold
        assert_eq!(window(&format!("%{lit}")), (WORD_SUFFIX, WORD_WINDOW, n, tail), "suffix: the last bytes"); // alloc: Cold
        assert_eq!(window(&format!("%{lit}%")), (WORD_CONTAINS, WORD_WINDOW, n, head.clone()), "infix"); // alloc: Cold
        assert_eq!(window(&lit), (WORD_EXACT, WORD_WINDOW, n, head.clone()), "exact");
        assert_eq!(window(&head), (WORD_EXACT, 0, WORD_LIT_BYTES as u64, head.clone()), "exactly the slot: no window");
        let one_more = "h".repeat(WORD_LIT_BYTES + 1); // alloc: Cold
        assert_eq!(window(&format!("{one_more}%")), (WORD_PREFIX, WORD_WINDOW, WORD_LIT_BYTES as u64 + 1, head.clone()), "the slot + 1"); // alloc: Cold
        let escaped = format!("{}\\%x%", "h".repeat(WORD_LIT_BYTES)); // alloc: Cold
        assert_eq!(window(&escaped), (WORD_PREFIX, WORD_WINDOW, WORD_LIT_BYTES as u64 + 2, head), "escaped past the scratch");
    }

    /// Through the entry: both header forms, a NULL pattern, and a compressed image (cold).
    #[test]
    fn the_entry_reads_the_bind_word() {
        let short = [&[((5u8 + 1) << 1) | 1][..], &b"%ab%x"[..]].concat();
        let long = [&((10u32 << 2).to_le_bytes())[..], &b"PROMO%"[..]].concat();
        let compressed = [&((40u32 << 2) | 2).to_le_bytes()[..], &[0; 36]].concat();
        let mut out = [0u8; WORD_LIT_BYTES];
        // SAFETY: `img` is a live varlena the test owns, `out` the slot
        let at = |img: &[u8], out: &mut [u8]| unsafe { entry2::<LikeWord>(0, img.as_ptr() as u64, out.as_mut_ptr() as u64) };
        assert_eq!(at(&short, &mut out), WORD_COLD, "an inner %");
        assert_eq!(at(&long, &mut out), WORD_PREFIX | 5 << 8);
        assert_eq!(&out[..5], b"PROMO");
        assert_eq!(at(&compressed, &mut out), WORD_COLD);
        // SAFETY: a NULL pattern reads nothing; `out` is the slot
        assert_eq!(unsafe { entry2::<LikeWord>(0, 0, out.as_mut_ptr() as u64) }, WORD_COLD);
    }
}
