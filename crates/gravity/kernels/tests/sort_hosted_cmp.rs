//! M6-REPS HO (rep.hosted.order): a var run's `RowCmp` orders a hosted key (`KFLAG_HOSTED`) by the btree support function
//! the table's hosted section names by its resolution id, called through fmgr on the two var copies (varlenas behind a
//! 4-byte header) under the key's collation; equal copies tie without a call (under no collation the call still runs,
//! D2043); a comparator's ERROR is stashed and
//! `RowCmp::failed` reports it.
use core::cmp::Ordering;
use gravity_kernels::host_abi::{RowCmp, SortSpec, KFLAG_EQTIE, KFLAG_HOSTED, KFLAG_WORDEXACT, VAR_TBL, VAR_WIDTH, WORD_WIDTH};

/// `byteacmp` and `bttextcmp` (pg_proc.dat).
const BYTEACMP: u32 = 1954;
const BTTEXTCMP: u32 = 360;
/// One `[flags, word]` pair, then the key's var slot.
const SLOT: usize = 16;
const ROW: usize = 24;
const CAP: usize = ROW + 64;

/// A run row: the key a value (flags 1) with the hosted image word 0, its copy after the row as a 4-byte-header varlena.
struct Row(Box<[u8; CAP]>);

impl Row {
    fn new(v: &[u8]) -> Row {
        let mut r = Row(Box::new([0; CAP]));
        r.0[..8].copy_from_slice(&1u64.to_ne_bytes());
        let size = (v.len() + 4) as u32;
        r.0[SLOT..SLOT + 4].copy_from_slice(&size.to_ne_bytes());
        r.0[SLOT + 4..SLOT + 8].copy_from_slice(&((ROW - SLOT) as u32).to_ne_bytes());
        r.0[ROW..ROW + 4].copy_from_slice(&(size << 2).to_ne_bytes());
        r.0[ROW + 4..ROW + 4 + v.len()].copy_from_slice(v);
        r
    }
}

/// The emit table: no images, one var column; key 0 its tail (the `SortSpec`), hosted by `proc` under `coll`. The hosted
/// section holds `proc`'s resolution id, as lowering writes it (`gravity_fn::resolved`: a builtin resolves with no catalog).
fn table(proc: u32, coll: u32, desc: bool) -> Vec<u32> {
    let proc = gravity_fn::resolved::resolve(proc).expect("a builtin resolves").expect("not refused");
    let spec = SortSpec::pack(&[(Some((SLOT as u32, VAR_WIDTH)), desc)]).unwrap().0;
    vec![ROW as u32, 2, 1 | VAR_TBL, 0, spec as u32, (spec >> 32) as u32, 1, 0, 0, SLOT as u32, coll, KFLAG_HOSTED, proc]
}

fn cmp(t: &[u32], a: &Row, b: &Row) -> Ordering {
    unsafe { RowCmp::of(t.as_ptr()).cmp(a.0.as_ptr(), b.0.as_ptr()) }
}

#[test]
fn a_hosted_key_orders_by_its_comparator() {
    let (a, b, ab) = (Row::new(b"a"), Row::new(b"b"), Row::new(b"ab"));
    let t = table(BYTEACMP, 0, false);
    assert_eq!(cmp(&t, &a, &b), Ordering::Less);
    assert_eq!(cmp(&t, &ab, &b), Ordering::Less, "byteacmp: 'ab' < 'b'");
    assert_eq!(cmp(&t, &a, &ab), Ordering::Less, "a prefix first");
    assert_eq!(cmp(&t, &b, &Row::new(b"b")), Ordering::Equal, "equal copies tie");
    assert_eq!(cmp(&table(BYTEACMP, 0, true), &a, &b), Ordering::Greater, "DESC reverses the comparator");
    assert!(!RowCmp::failed());
}

#[test]
fn a_comparator_error_is_stashed_and_reported() {
    // bttextcmp without a collation raises (PostgreSQL: could not determine which collation to use)
    let t = table(BTTEXTCMP, 0, false);
    assert_eq!(cmp(&t, &Row::new(b"x"), &Row::new(b"y")), Ordering::Equal, "a failed compare ties");
    assert!(RowCmp::failed(), "the error is reported");
    assert!(!RowCmp::failed(), "once");
    let e = gravity_fn::take_error().expect("the comparator's own error is stashed");
    assert!(e.message().contains("collation"), "PostgreSQL's message: {}", e.message());
    // D2043: under no collation equal copies still call the comparator (PostgreSQL's record_cmp -> varstr_cmp raises
    // at the first compare, equal or not); under a collation equal copies tie without a call
    assert_eq!(cmp(&t, &Row::new(b"x"), &Row::new(b"x")), Ordering::Equal);
    assert!(RowCmp::failed(), "equal copies under no collation raise too");
    assert!(gravity_fn::take_error().expect("stashed").message().contains("collation"));
    assert_eq!(cmp(&table(BTTEXTCMP, C_COLLATION, false), &Row::new(b"x"), &Row::new(b"x")), Ordering::Equal);
    assert!(!RowCmp::failed(), "equal copies under a collation never call the comparator");
}

/// `C` (pg_collation.dat).
const C_COLLATION: u32 = 950;

/// `btint4cmp`, `enum_cmp` (pg_proc.dat).
const BTINT4CMP: u32 = 351;
const ENUM_CMP: u32 = 3514;

/// V4R-386 (rep.hosted.order.fixed): a by-value key's run row, `[flags, word 0]` then its datum word at 16 (`WORD_WIDTH`).
fn word_row(d: u64) -> [u64; 3] {
    [1, 0, d]
}

/// A var table of no var column (nvar 0, the record word at 24): key 0's tail the datum word, hosted by `proc`.
fn word_table(proc: u32, desc: bool) -> Vec<u32> {
    let proc = gravity_fn::resolved::resolve(proc).expect("a builtin resolves").expect("not refused");
    let spec = SortSpec::pack(&[(Some((16, WORD_WIDTH)), desc)]).unwrap().0;
    vec![32, 2, 1 | VAR_TBL, 0, spec as u32, (spec >> 32) as u32, 0, 24, 0, KFLAG_HOSTED, proc]
}

fn word_cmp(t: &[u32], a: u64, b: u64) -> Ordering {
    let (a, b) = (word_row(a), word_row(b));
    unsafe { RowCmp::of(t.as_ptr()).cmp(a.as_ptr() as *const u8, b.as_ptr() as *const u8) }
}

#[test]
fn a_by_value_hosted_key_orders_by_its_comparator_over_the_datum_word() {
    let t = word_table(BTINT4CMP, false);
    let m1 = -1i32 as u32 as u64; // unsigned, the word bytes order it last; btint4cmp first
    assert_eq!(word_cmp(&t, m1, 1), Ordering::Less, "the comparator, not the word's bytes");
    assert_eq!(word_cmp(&t, 7, 7), Ordering::Equal, "equal words tie");
    assert_eq!(word_cmp(&word_table(BTINT4CMP, true), m1, 1), Ordering::Greater, "DESC reverses the comparator");
    // enum_cmp's even-oid path (no type cache): the oids' order
    let e = word_table(ENUM_CMP, false);
    assert_eq!((word_cmp(&e, 16500, 16502), word_cmp(&e, 16502, 16500), word_cmp(&e, 16502, 16502)), (Ordering::Less, Ordering::Greater, Ordering::Equal));
    assert!(!RowCmp::failed());
}

/// aggtrans 0387 (`keysem::call::order_seen`): under no collation equal copies call their comparator until it ran clean on the value,
/// then tie without a call; a value one comparator remembered never ties under another (that one still raises), and a
/// call that raised is never remembered (each equal pair raises again, as each of PostgreSQL's compares would).
#[test]
fn an_equal_copy_under_no_collation_ties_once_its_comparator_ran_clean() {
    let (x, x2) = (Row::new(b"x"), Row::new(b"x"));
    let (tb, tt) = (table(BYTEACMP, 0, false), table(BTTEXTCMP, 0, false));
    let (bytea, text) = unsafe { (RowCmp::of(tb.as_ptr()), RowCmp::of(tt.as_ptr())) };
    let eq = |rc: &RowCmp| unsafe { rc.cmp(x.0.as_ptr(), x2.0.as_ptr()) };
    assert_eq!(eq(&bytea), Ordering::Equal);
    assert!(!RowCmp::failed(), "byteacmp ran clean on 'x'");
    assert_eq!(eq(&bytea), Ordering::Equal, "'x' remembered: ties");
    assert!(!RowCmp::failed());
    for i in 0..2 {
        assert_eq!(eq(&text), Ordering::Equal);
        assert!(RowCmp::failed(), "bttextcmp under no collation raises on equal copies (pair {i}), whatever byteacmp saw");
        assert!(gravity_fn::take_error().expect("stashed").message().contains("collation"));
    }
}

/// `table` with more key flags beside `KFLAG_HOSTED`.
fn table_flags(proc: u32, coll: u32, desc: bool, flags: u32) -> Vec<u32> {
    let mut t = table(proc, coll, desc);
    t[11] |= flags;
    t
}

/// `Row::new` with key 0's word `w`.
fn row_word(v: &[u8], w: u64) -> Row {
    let mut r = Row::new(v);
    r.0[8..16].copy_from_slice(&w.to_ne_bytes());
    r
}

/// `KFLAG_EQTIE` (the lowering's word: the key's comparator cannot raise on equal copies) ties equal copies under no
/// collation with no call; `KFLAG_WORDEXACT` (a word that marks itself whole by its low bit, a bit string's) ties an equal
/// whole word with no payload read and no call (its low bit clear; set under DESC), and an equal word that is not whole
/// goes to the comparator. bttextcmp under no collation raises on any call, so a call shows as `failed`.
#[test]
fn eqtie_and_whole_words_tie_with_no_call() {
    let t = table_flags(BTTEXTCMP, 0, false, KFLAG_EQTIE);
    assert_eq!(cmp(&t, &Row::new(b"x"), &Row::new(b"x")), Ordering::Equal);
    assert!(!RowCmp::failed(), "equal copies tie with no call under KFLAG_EQTIE");
    assert_eq!(cmp(&t, &Row::new(b"x"), &Row::new(b"y")), Ordering::Equal, "unequal copies still call (and fail)");
    assert!(RowCmp::failed());
    let _ = gravity_fn::take_error();
    let asc = table_flags(BTTEXTCMP, 0, false, KFLAG_WORDEXACT);
    assert_eq!(cmp(&asc, &row_word(b"x", 6), &row_word(b"y", 6)), Ordering::Equal, "an equal whole word is an equal value");
    assert!(!RowCmp::failed(), "no call on a whole word");
    assert_eq!(cmp(&asc, &row_word(b"x", 5), &row_word(b"y", 5)), Ordering::Equal);
    assert!(RowCmp::failed(), "an equal word that is not whole calls the comparator");
    let _ = gravity_fn::take_error();
    assert_eq!(cmp(&asc, &row_word(b"x", 4), &row_word(b"y", 6)), Ordering::Less, "unequal words order as the words");
    let desc = table_flags(BTTEXTCMP, 0, true, KFLAG_WORDEXACT);
    assert_eq!(cmp(&desc, &row_word(b"x", !6), &row_word(b"y", !6)), Ordering::Equal, "DESC: the word is the image's !");
    assert!(!RowCmp::failed());
    assert_eq!(cmp(&desc, &row_word(b"x", !5), &row_word(b"y", !5)), Ordering::Equal);
    assert!(RowCmp::failed());
    let _ = gravity_fn::take_error();
}
