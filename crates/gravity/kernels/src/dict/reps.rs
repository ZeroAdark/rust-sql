//! A text key's representative (milestone-2.md §2.6 item 38): the `(part, code)` word of its `MinCode`
//! slot rendered once per group, and the count-ordered order that renders only where a count ties.
use gravity_abi::breaker::{flag_at, flag_words, KeyKind, RecordLayout, MINCODE_NONE, MOVING};
use gravity_abi::lane::EMPTY_CODE_NONE;
use gravity_abi::site::dict_value;
use gravity_abi::Site;

pub use gravity_abi::breaker::{MAX_REPS, REP_WORDS};
pub const REP_NONE: u8 = u8::MAX;
pub type Reps = [(u8, u8); MAX_REPS];

/// # Safety — `p` points at a varlena image (a 1-byte or 4-byte header).
pub unsafe fn varlena_bytes<'a>(p: *const u8) -> &'a [u8] {
    let b0 = *p;
    if b0 & 1 == 1 {
        core::slice::from_raw_parts(p.add(1), (b0 >> 1) as usize - 1)
    } else {
        let len = u32::from_le_bytes([*p, *p.add(1), *p.add(2), *p.add(3)]) >> 2;
        core::slice::from_raw_parts(p.add(4), len as usize - 4)
    }
}

/// The literal empty string (a 4B-U varlena image of length 0, as the dictionary renders its entries): the
/// representative of a group keyed by `''` on a part whose dictionary has no empty entry (`EMPTY_CODE_NONE` as
/// the code: the guarded key, lower.md §15.25).
#[repr(C, align(4))]
struct Image([u8; 4]);
static EMPTY_VARLENA: Image = Image([4 << 2, 0, 0, 0]);

/// Code word `word` of `lane` rendered through the site's dictionary: `Ok(None)` for `MINCODE_NONE` (no code, the SQL
/// NULL); `Err(status)` when the dictionary read failed, its error stashed in the node for the caller to raise, never
/// read as a NULL (D344). # Safety — `site` is a bound site of a source with dictionaries.
pub unsafe fn render(site: *mut Site, lane: u8, word: u64) -> Result<Option<*const u8>, i64> {
    if word == MINCODE_NONE {
        return Ok(None);
    }
    if word as u32 == EMPTY_CODE_NONE {
        return Ok(Some(EMPTY_VARLENA.0.as_ptr()));
    }
    let mut v = 0u64;
    let st = dict_value(site, (word >> 32) as u32, lane as u32, word as u32, &mut v);
    if st < 0 {
        return Err(st);
    }
    Ok(Some(v as *const u8))
}

/// Whether record `a` orders before `b` in a count-ordered top: the count descending, then the keys ascending (a word signed,
/// or, for key `i` with bit `i` of `text`, by its decimal text's bytes (D3080: an integer's text keyed by the integer,
/// `int_text_less`); a text key with a representative by its rendered bytes, a NULL render last; one without by its
/// fingerprint words).
/// # Safety — `a`, `b` are live records of `layout`; `site` as `render`.
#[allow(clippy::too_many_arguments)]
pub unsafe fn count_better(layout: &RecordLayout, off_count: usize, reps: &Reps, flags: Option<usize>, text: u32, site: *mut Site, a: *const u8, b: *const u8) -> bool {
    let (ca, cb) = (*(a.add(off_count) as *const u64) & !MOVING, *(b.add(off_count) as *const u64) & !MOVING);
    if ca != cb {
        return (ca as i64) > (cb as i64);
    }
    keys_better(layout, reps, flags, text, site, a, b)
}

/// D3080: whether integer `a`'s decimal output text (int2out / int4out / int8out: an optional '-', then the digits, no
/// leading zero) orders before `b`'s bytewise (a byte-order collation's `<`, a shorter prefix first). Allocates nothing.
pub fn int_text_less(a: i64, b: i64) -> bool {
    let (mut x, mut y) = ([0u8; 20], [0u8; 20]);
    let (pa, pb) = (int_text(a, &mut x), int_text(b, &mut y));
    pa < pb
}

/// `v`'s decimal text in the tail of `buf` (at most 20 bytes: i64::MIN's), as a slice.
fn int_text(v: i64, buf: &mut [u8; 20]) -> &[u8] {
    let mut u = v.unsigned_abs();
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (u % 10) as u8;
        u /= 10;
        if u == 0 {
            break;
        }
    }
    if v < 0 {
        i -= 1;
        buf[i] = b'-';
    }
    &buf[i..]
}

/// Whether record `a` orders before `b` by its keys alone, ascending, as `count_better` orders two records of one count.
/// `flags`: the record's first NULL flags word (`breaker::flag_at`), when its keys carry flags: a word key's NULL orders
/// after every value (NULLS LAST), two NULLs tie; its image word (0) is never compared. Inlined always: `count_better`
/// reads it at every count tie.
/// # Safety — as `count_better`; `flags` names the flags words of `layout`'s records.
#[inline(always)]
pub unsafe fn keys_better(layout: &RecordLayout, reps: &Reps, flags: Option<usize>, text: u32, site: *mut Site, a: *const u8, b: *const u8) -> bool {
    // the loop stops at the flags word, so the flags words must follow the image words directly and end the key words
    // (lowerv4 rec.rs `flags_word`): a slot between them would skip keys
    debug_assert!(
        flags.map_or(true, |f| f + flag_words(image_keys(layout, f)) == layout.nkeys()),
        "a count top's flags words follow its image words and end its key words"
    );
    let (mut k, mut t, mut i) = (0usize, 0usize, 0usize);
    while k < flags.unwrap_or(layout.nkeys()) {
        let (wa, wb) = (*(a as *const u64).add(k), *(b as *const u64).add(k));
        if layout.key_kind(k) == KeyKind::Fp128 {
            let (ha, hb) = (*(a as *const u64).add(k + 1), *(b as *const u64).add(k + 1));
            if (wa, ha) != (wb, hb) {
                let (slot, lane) = if t < MAX_REPS { reps[t] } else { (REP_NONE, 0) };
                if slot == REP_NONE {
                    return (wa, ha) < (wb, hb);
                }
                let off = layout.slot_off(slot as usize) as usize;
                let (ra, rb) = (render(site, lane, *(a.add(off) as *const u64)), render(site, lane, *(b.add(off) as *const u64)));
                return match (ra, rb) {
                    (Ok(Some(pa)), Ok(Some(pb))) => varlena_bytes(pa) < varlena_bytes(pb),
                    // a NULL (no representative code) orders after every text: the only NULL placement a count top claims
                    // on a rendered key is ascending's default, NULLS LAST (lowerv4 props `tie_key`)
                    (Ok(Some(_)), Ok(None)) => true,
                    (Ok(None), Ok(Some(_))) => false,
                    _ => (wa, ha) < (wb, hb),
                };
            }
            k += 2;
            t += 1;
            i += 1;
            continue;
        }
        if let Some(f) = flags {
            let (w, bit) = flag_at(i);
            let (na, nb) = (*(a as *const u64).add(f + w) >> bit & 1, *(b as *const u64).add(f + w) >> bit & 1);
            if na | nb != 0 {
                if na != nb {
                    return nb != 0;
                }
                k += 1;
                i += 1;
                continue;
            }
        }
        if wa != wb {
            if i < 32 && text >> i & 1 != 0 {
                return int_text_less(wa as i64, wb as i64);
            }
            return (wa as i64) < (wb as i64);
        }
        k += 1;
        i += 1;
    }
    false
}

/// The keys imaged in a record's first `words` key words (a two-word Fp128 one key), as `keys_better` counts them.
fn image_keys(layout: &RecordLayout, words: usize) -> usize {
    let (mut k, mut i) = (0usize, 0usize);
    while k < words {
        k += if layout.key_kind(k) == KeyKind::Fp128 { 2 } else { 1 };
        i += 1;
    }
    i
}

/// The keyed owner's slot words: the order slot (or 0) in word 0's bits 0-7; representative `t` in word `t / 4`, its
/// slot a byte from bit `8 + 8 * (t % 4)` (`REP_NONE` past the last), its lane six bits from bit `40 + 6 * (t % 4)`. The one
/// codec of every params image that carries representatives: the table owner, the distinct fold and the part-having owner,
/// each word 0 a plan word and the words past it at the params' tail.
pub fn pack_slot(slot: u8, reps: &Reps) -> [u64; REP_WORDS] {
    let mut w = [0u64; REP_WORDS];
    w[0] = slot as u64;
    for (t, &(s, l)) in reps.iter().enumerate() {
        assert!(s == REP_NONE || l < 64, "a representative's lane fits six bits");
        let j = t % 4;
        w[t / 4] |= (s as u64) << (8 + 8 * j) | ((l & 63) as u64) << (40 + 6 * j);
    }
    w
}

/// The representatives of the slot words `pack_slot` wrote.
pub fn unpack_slot(w: &[u64; REP_WORDS]) -> Reps {
    let mut reps = [(REP_NONE, 0u8); MAX_REPS];
    for (t, r) in reps.iter_mut().enumerate() {
        let (x, j) = (w[t / 4], t % 4);
        *r = ((x >> (8 + 8 * j)) as u8, (x >> (40 + 6 * j)) as u8 & 63);
    }
    reps
}

#[cfg(test)]
mod null_key_tests {
    use gravity_abi::breaker::SlotKind;

    use super::*;

    /// Two word keys and their flags word, as a count top's record of nullable int keys carries them.
    const L: RecordLayout = RecordLayout::new(&[KeyKind::I64, KeyKind::I32, KeyKind::I64], &[SlotKind::Count]);
    const NO_REPS: Reps = [(REP_NONE, 0); MAX_REPS];

    fn before(a: [i64; 3], b: [i64; 3], flags: Option<usize>) -> bool {
        let rec = |k: [i64; 3]| { let mut r = [0u64; 8]; r[..3].copy_from_slice(&k.map(|w| w as u64)); r };
        let (ra, rb) = (rec(a), rec(b));
        // SAFETY: both records hold the layout's words; no key is a fingerprint, so `site` is never read
        unsafe { keys_better(&L, &NO_REPS, flags, 0, core::ptr::null_mut(), ra.as_ptr() as *const u8, rb.as_ptr() as *const u8) }
    }

    #[test]
    fn an_int_text_key_orders_by_its_decimal_text() {
        let mut v = [i64::MIN, -100, -20, -1, 0, 1, 9, 10, 99, 100, 1000, i64::MAX, 512345678901234567];
        v.sort_by(|&a, &b| if int_text_less(a, b) { core::cmp::Ordering::Less } else if int_text_less(b, a) { core::cmp::Ordering::Greater } else { core::cmp::Ordering::Equal });
        let mut t: Vec<String> = v.iter().map(|x| x.to_string()).collect(); // alloc: Cold
        let want = { let mut w = t.clone(); w.sort(); w }; // alloc: Cold
        assert_eq!(t, want);
        t.dedup();
        assert_eq!(t.len(), v.len());
        let rec = |k: [i64; 3]| { let mut r = [0u64; 8]; r[..3].copy_from_slice(&k.map(|w| w as u64)); r };
        let (ra, rb) = (rec([10, 0, 0]), rec([9, 0, 0]));
        // SAFETY: both records hold the layout's words; no key is a fingerprint
        unsafe {
            assert!(keys_better(&L, &NO_REPS, None, 0b1, core::ptr::null_mut(), ra.as_ptr() as *const u8, rb.as_ptr() as *const u8)); // '10' < '9'
            assert!(!keys_better(&L, &NO_REPS, None, 0, core::ptr::null_mut(), ra.as_ptr() as *const u8, rb.as_ptr() as *const u8));
        }
    }

    #[test]
    fn a_null_word_key_orders_after_every_value() {
        let f = Some(2);
        assert!(!before([0, 5, 0b01], [0, 5, 0], f) && before([0, 5, 0], [0, 5, 0b01], f)); // NULL against 0, its image word
        assert!(!before([0, 0, 0b01], [i64::MIN, 0, 0], f) && before([i64::MIN, 0, 0], [0, 0, 0b01], f));
        assert!(!before([0, 0, 0b01], [i64::MAX, 0, 0], f));
        assert!(before([1, 9, 0], [1, 0, 0b10], f) && !before([1, 0, 0b10], [1, 9, 0], f)); // the second key's flag
    }

    #[test]
    fn two_nulls_tie_and_the_next_key_decides() {
        let f = Some(2);
        assert!(before([0, 3, 0b01], [0, 7, 0b01], f) && !before([0, 7, 0b01], [0, 3, 0b01], f));
        assert!(!before([0, 3, 0b11], [0, 3, 0b11], f));
    }

    #[test]
    fn not_null_keys_order_as_before() {
        for f in [Some(2), None] {
            assert!(before([-1, 0, 0], [1, 0, 0], f) && !before([1, 0, 0], [-1, 0, 0], f));
            assert!(before([4, -8, 0], [4, 2, 0], f) && !before([4, 2, 0], [4, 2, 0], f));
        }
        // without flags the flags word is one more key word, as every caller without a nullable tie key reads it
        assert!(before([0, 5, 0], [0, 5, 0b01], None) && !before([0, 5, 0b01], [0, 5, 0], None));
    }

    /// dict 2026-10-06T01:10:15Z: a flags index that leaves a key word between the image and the flags words is caught.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "flags words follow")]
    fn a_flags_word_short_of_the_last_image_word_is_caught() {
        before([0, 0, 0], [0, 0, 0], Some(1));
    }
}

#[cfg(test)]
mod slot_word_tests {
    use super::*;

    #[test]
    fn slot_words_round_trip_a_representative_per_record_slot() {
        let mut reps: Reps = [(REP_NONE, 0); MAX_REPS];
        reps[0] = (3, 7);
        reps[1] = (9, 63);
        let w = pack_slot(5, &reps);
        assert_eq!(w[0] as u8, 5);
        assert_eq!(unpack_slot(&w), reps);
        assert_eq!(unpack_slot(&pack_slot(0, &reps)), reps);
        assert_eq!(unpack_slot(&pack_slot(0, &[(REP_NONE, 0); MAX_REPS])), [(REP_NONE, 0); MAX_REPS]);
        // one per record slot: every slot a representative, past the first word's four
        let all: Reps = core::array::from_fn(|t| (t as u8, (63 - t) as u8));
        assert_eq!(MAX_REPS, gravity_abi::breaker::MAX_RECORD_SLOTS);
        assert_eq!(unpack_slot(&pack_slot(2, &all)), all);
        let four: Reps = core::array::from_fn(|t| if t < 4 { all[t] } else { (REP_NONE, 0) });
        assert_eq!(pack_slot(2, &all)[0], pack_slot(2, &four)[0], "word 0 is the first four's, as the one-word codec wrote it");
    }
}
