//! V4R-265 (M6 L0-b, D16): a var run's `RowCmp` compares the tails of keys 4 to 15. Their `SortSpec` entries ride the var
//! section's per-key `kflags >> KFLAG_SPEC_SHIFT` (the table's `SortSpec` packs four), with the key's collation and
//! `KFLAG_BPCHAR`; before, a tail at key 4 or past was never read and rows tying on its prefix word compared equal.
use core::cmp::Ordering;
use gravity_kernels::host_abi::{RowCmp, SortSpec, KFLAG_BPCHAR, KFLAG_SPEC_SHIFT, VAR_TBL, VAR_WIDTH};

const NKEY: usize = 6;
/// Six `[flags, word]` pairs, then the var slots of keys 4 and 5.
const SLOT: [usize; 2] = [16 * NKEY, 16 * NKEY + 8];
const ROW: usize = 16 * NKEY + 16;
const CAP: usize = ROW + 2 * 64;

/// A run row: every key a value (flags 1), keys 0-3 word 0, keys 4 and 5 their prefix word (the first 8 bytes big-endian,
/// zero-padded) and their tails copied after the row.
struct Row(Box<[u8; CAP]>);

impl Row {
    fn new(t4: &str, t5: &str) -> Row {
        let mut r = Row(Box::new([0; CAP]));
        for k in 0..NKEY {
            r.0[16 * k..16 * k + 8].copy_from_slice(&1u64.to_ne_bytes());
        }
        let mut at = ROW;
        for (k, (slot, t)) in SLOT.into_iter().zip([t4, t5]).enumerate() {
            let mut w = [0u8; 8];
            w[..t.len().min(8)].copy_from_slice(&t.as_bytes()[..t.len().min(8)]);
            r.0[16 * (4 + k) + 8..16 * (4 + k) + 16].copy_from_slice(&u64::from_be_bytes(w).to_ne_bytes());
            let size = (t.len() + 4) as u32;
            r.0[slot..slot + 4].copy_from_slice(&size.to_ne_bytes());
            r.0[slot + 4..slot + 8].copy_from_slice(&((at - slot) as u32).to_ne_bytes());
            r.0[at + 4..at + 4 + t.len()].copy_from_slice(t.as_bytes());
            at += 64;
        }
        r
    }
}

/// The emit table: no images, two var columns; keys 4 and 5 VAR_WIDTH tails in their kflags (key 5 DESC and bpchar by the flags).
fn table(desc5: bool, bpchar5: bool) -> Vec<u32> {
    let entry = |slot: usize, desc: bool| (SortSpec::pack(&[(Some((slot as u32, VAR_WIDTH)), desc)]).unwrap().0 as u32) << KFLAG_SPEC_SHIFT;
    let mut t = vec![ROW as u32, 2 * NKEY as u32, 6 | VAR_TBL, 0, 0, 0, 2, 0, 4, SLOT[0] as u32, 5, SLOT[1] as u32];
    for k in 0..NKEY {
        let kf = match k {
            4 => entry(SLOT[0], false),
            5 => entry(SLOT[1], desc5) | if bpchar5 { KFLAG_BPCHAR } else { 0 },
            _ => 0,
        };
        t.extend([0, kf]);
    }
    t
}

fn cmp(t: &[u32], a: &Row, b: &Row) -> Ordering {
    unsafe { RowCmp::of(t.as_ptr()).cmp(a.0.as_ptr(), b.0.as_ptr()) }
}

#[test]
fn key_five_orders_by_its_tail_past_the_prefix_word() {
    let t = table(false, false);
    let (jam, fool) = (Row::new("strawberry jam", "x"), Row::new("strawberry fool", "x"));
    assert_eq!(cmp(&t, &fool, &jam), Ordering::Less, "the fifth key's tail decides a tie on every pair");
    let mut rows = [Row::new("strawberry jam", "b"), Row::new("strawberry fool", "b"), Row::new("strawberry jam", "a")];
    rows.sort_by(|a, b| cmp(&t, a, b));
    let got: Vec<(&[u8], &[u8])> = rows.iter().map(|r| (&r.0[ROW + 4..ROW + 19], &r.0[ROW + 68..ROW + 69])).collect();
    assert_eq!(got, [(&b"strawberry fool"[..], &b"b"[..]), (&b"strawberry jam\0"[..], &b"a"[..]), (&b"strawberry jam\0"[..], &b"b"[..])]);
}

#[test]
fn key_six_takes_its_own_desc_and_bpchar_flags() {
    let (a, b) = (Row::new("same", "abcdefgh1"), Row::new("same", "abcdefgh2"));
    assert_eq!(cmp(&table(false, false), &a, &b), Ordering::Less);
    assert_eq!(cmp(&table(true, false), &a, &b), Ordering::Greater, "DESC reverses the sixth key's tail");
    let (pad, bare) = (Row::new("same", "abcdefghij   "), Row::new("same", "abcdefghij"));
    assert_eq!(cmp(&table(false, false), &bare, &pad), Ordering::Less, "text: trailing blanks count");
    assert_eq!(cmp(&table(false, true), &bare, &pad), Ordering::Equal, "bpchar: trailing blanks trimmed");
}
