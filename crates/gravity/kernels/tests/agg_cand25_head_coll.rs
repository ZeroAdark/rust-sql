//! CAND-25: `agg_sort`'s text-first road orders records by each first text image's head (its first eight bytes) before
//! the full comparator, and only where the word's collation orders by bytes as a property: collation 0, which the
//! lowering (topn lower.rs `byte_coll`) leaves only for C, POSIX or a C database's default. Under any other collation
//! the word keeps its oid and takes no head, so every compare is the collation's from the first one. The collation seam
//! here orders ASCII letters as ICU's root collation (und-x-icu) does, case-blind first and a lowercase letter before its
//! uppercase, an order no byte prefix gives: the records come out in the seam's order, and the seam is called.
use core::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use gravity_abi::breaker::{KeyKind, RecordLayout, SlotKind, LAYOUT_WORDS};
use gravity_abi::{Ctx, Site};
use gravity_kernels::host_abi::{agg_sort, EmitState, DESC_BYTES, DESC_FORM, DESC_LAYOUT, DESC_NSORT, DESC_SORT, FORM_BOUND_SHIFT, SK_DESC, SK_HEAP, SK_TEXT, SK_TEXT_COLL_SHIFT};

/// Not 0: a collation the lowering leaves on the word (ICU, libc, a non-C database default).
const COLL: u64 = 12345;

/// The dictionary: code i is `KEYS[i]` (4-byte-header varlena images, the bytes after the header).
const KEYS: [&str; 8] = ["B", "a", "ab", "A", "b", "Ab", "aB", "BA"];
static ENTRIES: [[u8; 8]; 8] = {
    let mut e = [[0u8; 8]; 8];
    let mut i = 0;
    while i < 8 {
        let k = KEYS[i].as_bytes();
        e[i][0] = ((4 + k.len()) << 2) as u8;
        let mut j = 0;
        while j < k.len() {
            e[i][4 + j] = k[j];
            j += 1;
        }
        i += 1;
    }
    e
};

/// The seam's calls: the collation compares the sort made.
static CALLS: AtomicUsize = AtomicUsize::new(0);

/// und-x-icu's order over ASCII letters: case-blind, then a lowercase letter before its uppercase, then the length.
fn icu_like(a: &[u8], b: &[u8]) -> i32 {
    let primary = |s: &[u8]| s.iter().map(|c| c.to_ascii_lowercase()).collect::<Vec<_>>();
    let tertiary = |s: &[u8]| s.iter().map(|c| c.is_ascii_uppercase()).collect::<Vec<_>>();
    primary(a).cmp(&primary(b)).then_with(|| tertiary(a).cmp(&tertiary(b))) as i32
}

fn arm_seam() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        pg_locale_seams::varstr_cmp_locale::set(|_coll, a, b| {
            CALLS.fetch_add(1, Relaxed);
            Ok(icu_like(a, b))
        })
    });
}

unsafe extern "C" fn dict(_s: *mut Site, _part: u32, _lane: u32, code: u32, out: *mut u64) -> i64 {
    *out = ENTRIES[code as usize].as_ptr() as u64;
    0
}

/// Key word 0: the group's own word (the last tie-break); slot 0: the text key's representative code.
const LAYOUT: RecordLayout = RecordLayout::new_nn(&[KeyKind::I64], &[SlotKind::MinCode]);

/// The groups `KEYS` sorted by their text key under collation `coll` (DESC when `desc`), the first `bound` of them in
/// order when `bound` is some (a top's cut, `FORM_BOUND_SHIFT`), as their keys.
fn sorted(coll: u64, desc: bool, bound: Option<usize>) -> Vec<&'static str> {
    let words = LAYOUT.size() as usize / 8;
    let mut recs: Vec<Vec<u64>> = (0..KEYS.len() as u64)
        .map(|code| {
            let mut r = vec![0u64; words];
            r[0] = 100 - code; // the key words alone would order the codes backwards
            r[LAYOUT.slot_off[0] as usize / 8] = 3 << 32 | code;
            r
        })
        .collect();
    let mut ptrs: Vec<*const u8> = recs.iter_mut().map(|r| r.as_ptr() as *const u8).collect();
    let mut st = EmitState { ptrs: ptrs.as_mut_ptr(), len: KEYS.len() as u64, kept: KEYS.len() as u64 };
    let mut desc_words = vec![0u64; DESC_BYTES as usize / 8];
    let lw: [u64; LAYOUT_WORDS] = LAYOUT.words();
    desc_words[DESC_LAYOUT / 8..DESC_LAYOUT / 8 + LAYOUT_WORDS].copy_from_slice(&lw);
    desc_words[DESC_FORM / 8] = bound.map_or(0, |n| (n as u64 + 1) << FORM_BOUND_SHIFT);
    desc_words[DESC_NSORT / 8] = 1;
    desc_words[DESC_SORT / 8] = SK_TEXT | coll << SK_TEXT_COLL_SHIFT | if desc { SK_DESC } else { 0 };
    assert_eq!(desc_words[DESC_SORT / 8] & SK_HEAP, 0, "a dictionary-coded key: the road takes its images in code order");
    // SAFETY: a site and a ctx are plain words; zero is their closed state.
    let mut site: Site = unsafe { core::mem::zeroed() };
    site.dict = Some(dict);
    let mut ctx: Ctx = unsafe { core::mem::zeroed() };
    ctx.sites = &mut site;
    // SAFETY: `st` holds live records of `LAYOUT`, `desc_words` a descriptor of it; site 0 renders through `dict`.
    let r = unsafe { agg_sort(&mut ctx as *mut _ as u64, (&mut st as *mut EmitState as *mut u8) as u64, (desc_words.as_ptr() as *const u8) as u64) };
    assert_eq!(r, 0);
    let n = bound.unwrap_or(KEYS.len());
    ptrs[..n]
        .iter()
        .map(|&p| {
            // SAFETY: every pointer is one of `recs`.
            let r = unsafe { core::slice::from_raw_parts(p as *const u64, words) };
            KEYS[r[LAYOUT.slot_off[0] as usize / 8] as u32 as usize]
        })
        .collect()
}

#[test]
fn a_collation_left_on_the_word_orders_every_compare_and_takes_no_head() {
    arm_seam();
    let icu = ["a", "A", "ab", "aB", "Ab", "b", "B", "BA"];
    let before = CALLS.load(Relaxed);
    assert_eq!(sorted(COLL, false, None), icu, "the collation's order, not the bytes' (a head would give A Ab B BA a aB ab b)");
    assert!(CALLS.load(Relaxed) > before, "the collation's compare ordered the records");
    let mut rev = icu;
    rev.reverse();
    assert_eq!(sorted(COLL, true, None), rev, "DESC");
    assert_eq!(sorted(COLL, false, Some(3)), icu[..3], "a top's cut: the bounded select under the same comparator");
    assert_eq!(sorted(COLL, true, Some(2)), rev[..2], "DESC cut");
}

#[test]
fn collation_zero_orders_by_bytes_with_heads() {
    arm_seam();
    let bytes = ["A", "Ab", "B", "BA", "a", "aB", "ab", "b"];
    assert_eq!(sorted(0, false, None), bytes, "a byte-ordered collation (C / POSIX / a C default, cleared to 0): bytes, then length");
    let mut rev = bytes;
    rev.reverse();
    assert_eq!(sorted(0, true, Some(3)), rev[..3], "DESC cut");
}
