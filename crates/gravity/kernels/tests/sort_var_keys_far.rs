//! W8-SETOP-WIDEKEYS: a var run's `RowCmp` compares the tails of keys past `ROWCMP_KEYS` (16), reading each one's
//! `SortSpec` entry, collation, `KFLAG_BPCHAR` and `KFLAG_HOSTED` from the table's var section (and a hosted key's
//! comparator from its hosted section); before, keys past 16 compared by their `[flags, word]` pairs alone, so rows tying on
//! a key's prefix word compared equal (stats_import#137-140: 30 keys, texts and arrays past 16). A table with no var
//! section still orders keys past 16 by their pairs.
use core::cmp::Ordering;
use gravity_kernels::host_abi::{RowCmp, SortSpec, KFLAG_BPCHAR, KFLAG_HOSTED, KFLAG_SPEC_SHIFT, ROWCMP_KEYS, VAR_TBL, VAR_WIDTH};

const NKEY: usize = 20;
/// Keys 17, 18 and 19 have var tails in these slots after the `NKEY` pairs.
const SLOT: [usize; 3] = [16 * NKEY, 16 * NKEY + 8, 16 * NKEY + 16];
const ROW: usize = 16 * NKEY + 24;
const CAP: usize = ROW + 3 * 64;
/// `byteacmp` (pg_proc.dat).
const BYTEACMP: u32 = 1954;

/// A run row: every key a value (flags 1) with word 0 but keys 17-19's prefix words (the first 8 bytes big-endian,
/// zero-padded) and their copies after the row, each behind a 4-byte varlena header.
struct Row(Box<[u8; CAP]>);

impl Row {
    fn new(t: [&[u8]; 3]) -> Row {
        let mut r = Row(Box::new([0; CAP]));
        for k in 0..NKEY {
            r.0[16 * k..16 * k + 8].copy_from_slice(&1u64.to_ne_bytes());
        }
        let mut at = ROW;
        for (i, (slot, v)) in SLOT.into_iter().zip(t).enumerate() {
            let mut w = [0u8; 8];
            w[..v.len().min(8)].copy_from_slice(&v[..v.len().min(8)]);
            let k = 17 + i;
            r.0[16 * k + 8..16 * k + 16].copy_from_slice(&u64::from_be_bytes(w).to_ne_bytes());
            let size = (v.len() + 4) as u32;
            r.0[slot..slot + 4].copy_from_slice(&size.to_ne_bytes());
            r.0[slot + 4..slot + 8].copy_from_slice(&((at - slot) as u32).to_ne_bytes());
            r.0[at..at + 4].copy_from_slice(&(size << 2).to_ne_bytes());
            r.0[at + 4..at + 4 + v.len()].copy_from_slice(v);
            at += 64;
        }
        r
    }
}

/// The emit table: three var columns; keys 17-19 VAR_WIDTH tails in their kflags (key 18 DESC / bpchar by the flags, key 19
/// hosted by `byteacmp` when `hosted`, then the hosted section).
fn table(desc18: bool, bpchar18: bool, hosted: bool) -> Vec<u32> {
    let entry = |slot: usize, desc: bool| (SortSpec::pack(&[(Some((slot as u32, VAR_WIDTH)), desc)]).unwrap().0 as u32) << KFLAG_SPEC_SHIFT;
    let mut t = vec![ROW as u32, 2 * NKEY as u32, NKEY as u32 | VAR_TBL, 0, 0, 0, 3, 0, 17, SLOT[0] as u32, 18, SLOT[1] as u32, 19, SLOT[2] as u32];
    for k in 0..NKEY {
        let kf = match k {
            17 => entry(SLOT[0], false),
            18 => entry(SLOT[1], desc18) | if bpchar18 { KFLAG_BPCHAR } else { 0 },
            19 => entry(SLOT[2], false) | if hosted { KFLAG_HOSTED } else { 0 },
            _ => 0,
        };
        t.extend([0, kf]);
    }
    if hosted {
        let proc = gravity_fn::resolved::resolve(BYTEACMP).expect("a builtin resolves").expect("not refused");
        t.extend((0..NKEY).map(|k| if k == 19 { proc } else { 0 }));
    }
    t
}

fn cmp(t: &[u32], a: &Row, b: &Row) -> Ordering {
    unsafe { RowCmp::of(t.as_ptr()).cmp(a.0.as_ptr(), b.0.as_ptr()) }
}

#[test]
fn key_seventeen_orders_by_its_tail_past_the_prefix_word() {
    assert!(NKEY > ROWCMP_KEYS + 3);
    let t = table(false, false, false);
    let (jam, fool) = (Row::new([b"strawberry jam", b"x", b"x"]), Row::new([b"strawberry fool", b"x", b"x"]));
    assert_eq!(cmp(&t, &fool, &jam), Ordering::Less, "key 17's tail decides a tie on every pair");
    assert_eq!(cmp(&t, &jam, &fool), Ordering::Greater);
    assert_eq!(cmp(&t, &jam, &Row::new([b"strawberry jam", b"x", b"x"])), Ordering::Equal);
    let mut rows = [Row::new([b"strawberry jam", b"b", b"x"]), Row::new([b"strawberry fool", b"b", b"x"]), Row::new([b"strawberry jam", b"a", b"x"])];
    rows.sort_by(|a, b| cmp(&t, a, b));
    let got: Vec<(&[u8], &[u8])> = rows.iter().map(|r| (&r.0[ROW + 4..ROW + 19], &r.0[ROW + 68..ROW + 69])).collect();
    assert_eq!(got, [(&b"strawberry fool"[..], &b"b"[..]), (&b"strawberry jam\0"[..], &b"a"[..]), (&b"strawberry jam\0"[..], &b"b"[..])]);
}

#[test]
fn key_eighteen_takes_its_own_desc_and_bpchar_flags() {
    let (a, b) = (Row::new([b"same", b"abcdefgh1", b"x"]), Row::new([b"same", b"abcdefgh2", b"x"]));
    assert_eq!(cmp(&table(false, false, false), &a, &b), Ordering::Less);
    assert_eq!(cmp(&table(true, false, false), &a, &b), Ordering::Greater, "DESC reverses the tail");
    let (pad, bare) = (Row::new([b"same", b"abcdefghij   ", b"x"]), Row::new([b"same", b"abcdefghij", b"x"]));
    assert_eq!(cmp(&table(false, false, false), &bare, &pad), Ordering::Less, "text: trailing blanks count");
    assert_eq!(cmp(&table(false, true, false), &bare, &pad), Ordering::Equal, "bpchar: trailing blanks trimmed");
}

#[test]
fn key_nineteen_hosted_orders_by_its_comparator() {
    // prefix words tie ("abcdefgh"); byteacmp orders the payloads: a prefix first, then the bytes
    let (short, long, other) = (Row::new([b"s", b"s", b"abcdefgh"]), Row::new([b"s", b"s", b"abcdefgh\x01"]), Row::new([b"s", b"s", b"abcdefgh\x00\x02"]));
    let t = table(false, false, true);
    assert_eq!(cmp(&t, &short, &long), Ordering::Less);
    assert_eq!(cmp(&t, &other, &long), Ordering::Less, "byteacmp: 0x00 0x02 < 0x01");
    assert_eq!(cmp(&t, &long, &Row::new([b"s", b"s", b"abcdefgh\x01"])), Ordering::Equal, "equal copies tie");
    assert!(!RowCmp::failed());
}

#[test]
fn a_table_with_no_var_section_orders_keys_past_sixteen_by_their_pairs() {
    let t = vec![(16 * NKEY) as u32, 2 * NKEY as u32, NKEY as u32, 0, 0, 0];
    let row = |w: u64| {
        let mut r = vec![0u64; 2 * NKEY];
        (0..NKEY).for_each(|k| r[2 * k] = 1);
        r[2 * 18 + 1] = w;
        r
    };
    let (a, b) = (row(1), row(2));
    let c = |x: &[u64], y: &[u64]| unsafe { RowCmp::of(t.as_ptr()).cmp(x.as_ptr() as *const u8, y.as_ptr() as *const u8) };
    assert_eq!(c(&a, &b), Ordering::Less, "key 18's word");
    assert_eq!(c(&b, &a), Ordering::Greater);
    assert_eq!(c(&a, &row(1)), Ordering::Equal);
}
