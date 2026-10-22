//! `nest.subplan`'s hashed set over several key columns (RT-2d, V4R-415):
//! PostgreSQL's `buildSubPlanHash` / `ExecHashSubPlan` for a row comparison `(a, b, ..) [NOT] IN (SELECT x, y, ..)`.
//!
//! Layout (`arg`, LAZY bit taken off): ncols in bits 0-7, bits 8-15 zero (the one-column form's width byte), from bit 24
//! two bits per column, log2 of its width in bytes (1, 2, 4 or 8), and from bit 40 one bit per column, set for a byte key
//! (its width bits 0). A row (insert and probe alike) is the key words in column order, one per fixed column and two per
//! byte key (a pointer to its bytes, no header, and their length: M8-SUB's lowering hands text / varchar / bytea detoasted
//! and bpchar trailing-blank-trimmed), then one null word per column: 0 a value, 1 NULL, and at a probe only, 2: a
//! cross-type value outside the set column's type (M8-SUB's integer cross-type equalities), not NULL and equal to no key.
//! The several-column form has 2 to 8 columns; with a byte key, 1 to 8 (a byte key has no one-column KeyTable form).
//! D3674: the wide form (`abi::SUBHASH_WIDE`) has 9 to `abi::SUBHASH_WIDE_MAX` fixed columns (PostgreSQL's
//! TupleHashTable has no column limit: a check view's EXISTS over 11 columns of a row is one set), their widths from bit
//! 24 as above, no byte key.
//!
//! Key equality is the width-masked word's, and a byte key's (length, bytes): M8-SUB's lowering admits only operators that
//! answer by their operands' datum bits (integer-like by-value types, float4 / float8 refused by name: -0 and the NaNs are equal values with different bits)
//! or by their bytes (text / varchar under a deterministic collation, bytea, trimmed bpchar; nondeterministic collations
//! refused by name), so the words and bytes are the values. A set row's byte keys are copied at insert.
//!
//! The build keeps PG's two tables: rows with no NULL column in the exact set (`hashtable`), rows with any NULL column in
//! the null side (`hashnulls`), each without duplicates. The probe answers as `ExecHashSubPlan` (`findPartialMatch`: on
//! every column either side NULL, or both values and equal):
//! - an empty subquery: FALSE, whatever the probe;
//! - a probe with no NULL column: an exact hit TRUE, else a partial match on the null side NULL, else FALSE;
//! - a probe whose every column is NULL: NULL;
//! - else: a partial match on the null side, then in the exact set, NULL; else FALSE.
//!
//! A null word 2 never takes an exact hit and matches only a NULL set cell. The IR maps the verdict to its SubLink's
//! (NOT IN negates TRUE / FALSE and keeps NULL; `unknownEqFalse` reads NULL as FALSE, PG's answer in every case).
//! Created by M8-RUNTIME; byte keys by M8-RUNTIME (V4R-463).
use core::cell::Cell;
use std::collections::HashSet;

use gravity_lower::ops::subplan::keyset::width_mask;
use memledger::charge::{Charge, Door, Owner, Release};
use memledger::Kind;

use crate::ops::subplan::hash::SetObj;
use gravity_kernels::ops::shared::object::entries::{SUBHASH_WIDE, SUBHASH_WIDE_MAX};

/// The most columns a set lays (the wide form's, D3674); the null words of a row.
pub(super) const MAX_COLS: usize = SUBHASH_WIDE_MAX;

pub(crate) const TRUE: i64 = 1;
pub(crate) const FALSE: i64 = 0;
pub(crate) const NULL: i64 = 2;

/// A probe column's null word: a cross-type value outside the set column's type (M8-SUB's), equal to no key.
pub(super) const NONE: u64 = 2;

/// The most a set's charge lags its heap bytes between two syncs (RULINGS-1 R1: a batched commit, the lag bounded and
/// stated); exact at the seal and the clear.
pub(super) const SYNC_SLACK: u64 = 64 << 10;

/// A refused insert: a bad row (its error by name), or the set's growth the allocator refused (the bytes asked).
#[derive(Debug)]
pub(crate) enum Refused {
    Row(String), // alloc: Cold
    Memory(u64),
}

impl core::fmt::Display for Refused {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Refused::Row(e) => f.write_str(e),
            Refused::Memory(n) => write!(f, "out of memory: {n} bytes for a hashed SubPlan's set"),
        }
    }
}

pub(crate) struct Multi {
    /// CONVERT wave 2 (site 7): the set's heap bytes (`heap`: the keys' boxes and the two tables' blocks) charged to the
    /// build's owner (`own`, at its first insert: the hashed SubPlan's build pipeline's Distinct slot), `charged` of them
    /// committed: re-synced once it lags by `SYNC_SLACK` (`sync`), exactly at the seal and the clear. Declared first, so
    /// dropped first (its door-1 drop moves the bytes to the freeing thread's account, where the tables' frees land next).
    charge: Charge,
    charged: u64,
    /// The keys' boxes' bytes (the encoded keys kept).
    pub(super) key_bytes: u64,
    /// `own` ran; `on`: with an owner (the charge is open).
    owned: bool,
    on: bool,
    pub(super) masks: Box<[u64]>,
    /// The byte-key columns (bit c: column c's words are a (ptr, len) pair; the byte-key form).
    pub(super) bytes: u64,
    /// Rows with no NULL column (PG's `hashtable`): each row's encoded key (`encode`).
    pub(super) exact: HashSet<Box<[u8]>>,
    /// Rows with a NULL column (PG's `hashnulls`): the encoded key with each NULL column empty, and the NULL columns' mask.
    pub(super) nulls: HashSet<(Box<[u8]>, u64)>,
}

impl Multi {
    /// The layout of `arg` (the LAZY bit off), `None` when it is neither the several-column form nor the byte-key one.
    pub(crate) fn of(arg: u64) -> Option<Multi> {
        if arg & SUBHASH_WIDE != 0 {
            let (ncols, widths) = ((arg & 0xff) as usize, arg >> 24);
            let past = if ncols < SUBHASH_WIDE_MAX { widths >> (2 * ncols) } else { 0 };
            if !(9..=SUBHASH_WIDE_MAX).contains(&ncols) || arg >> 8 & 0xffff != SUBHASH_WIDE >> 8 || past != 0 {
                return None;
            }
            let masks = (0..ncols).map(|c| width_mask(1u8 << (widths >> (2 * c) & 3))).collect();
            return Some(Multi { masks, bytes: 0, exact: Default::default(), nulls: Default::default(), ..Multi::uncharged() });
        }
        let (ncols, bytes, widths) = ((arg & 0xff) as usize, arg >> 40 & 0xff, arg >> 24 & 0xffff);
        let lo = if bytes != 0 { 1 } else { 2 };
        let fixed_widths = (0..ncols).all(|c| bytes >> c & 1 == 0 || widths >> (2 * c) & 3 == 0);
        if !(lo..=8).contains(&ncols) || arg >> 8 & 0xffff != 0 || widths >> (2 * ncols) != 0 || bytes >> ncols != 0 || arg >> 48 != 0 || !fixed_widths {
            return None;
        }
        let masks = (0..ncols).map(|c| width_mask(1u8 << (widths >> (2 * c) & 3))).collect();
        Some(Multi { masks, bytes, exact: Default::default(), nulls: Default::default(), ..Multi::uncharged() })
    }

    /// No layout, nothing held, charged to no one (`of`'s rest).
    fn uncharged() -> Multi {
        Multi { charge: Charge::NONE, charged: 0, key_bytes: 0, owned: false, on: false, masks: Default::default(), bytes: 0, exact: Default::default(), nulls: Default::default() }
    }

    pub(crate) fn clear(&mut self) {
        (self.exact, self.nulls, self.key_bytes) = (Default::default(), Default::default(), 0);
        self.sync(0);
    }

    /// Whether the set still waits for its owner (`own`): its first insert's caller reads it, so the owner lookup is once.
    pub(crate) fn needs_owner(&self) -> bool {
        !self.owned
    }

    /// The owner the set's bytes are charged to from now (`None`: no one; a unit test's set, a slot with no owner).
    pub(crate) fn own(&mut self, owner: Option<(Owner, Kind)>) {
        self.owned = true;
        if let Some((o, k)) = owner {
            (self.charge, self.on) = (Charge::open(o, Release::Statement, k, Door::Heap, 0), true);
            self.sync(0);
        }
    }

    /// The heap bytes the set holds: its keys' boxes, and each table's block (hashbrown: its buckets x (entry + a control
    /// byte) + a group of control bytes; the buckets a power of two holding the capacity at 7 / 8).
    fn heap(&self) -> u64 {
        fn table(cap: usize, entry: usize) -> u64 {
            match cap {
                0 => 0,
                c if c < 8 => ((c + 1).next_power_of_two() * (entry + 1) + 16) as u64,
                c => ((c * 8 / 7).next_power_of_two() * (entry + 1) + 16) as u64,
            }
        }
        // an entry's size, read off the table's own element type
        fn entry<'a, T: 'a>(_: impl IntoIterator<Item = &'a T>) -> usize {
            core::mem::size_of::<T>()
        }
        self.key_bytes + table(self.exact.capacity(), entry(&self.exact)) + table(self.nulls.capacity(), entry(&self.nulls))
    }

    /// The charge re-synced to `heap` once it lags by `slack` bytes or more (0: exactly): a commit of what the set allocated
    /// (on this thread, the build's: the allocator counted it there), a free of what it gave back (the tables a clear
    /// dropped, on this thread too).
    pub(super) fn sync(&mut self, slack: u64) {
        if !self.on {
            return;
        }
        let h = self.heap();
        if h >= self.charged + slack.max(1) {
            self.charge.commit(h - self.charged);
            self.charged = h;
        } else if h < self.charged {
            self.charge.free(self.charged - h);
            self.charged = h;
        }
    }




    /// The encoded key's column `c` from byte `at`: (its bytes, the next column's start).
    fn col<'a>(&self, enc: &'a [u8], c: usize, at: usize) -> (&'a [u8], usize) {
        let len = if self.bytes >> c & 1 != 0 { 8 + u64::from_le_bytes(enc[at..at + 8].try_into().unwrap_or_default()) as usize } else { 8 };
        (&enc[at..at + len], at + len)
    }

    /// `findPartialMatch`'s test of one set row (`keys`, NULL columns `nullmask`): on every column either side NULL, or both
    /// values and equal. A probe column with null word 2 is a value no key equals.
    pub(super) fn partial(&self, probe: &[u8], null: &[u64; MAX_COLS], keys: &[u8], nullmask: u64) -> bool {
        let (mut p, mut s) = (0, 0);
        (0..self.masks.len()).all(|c| {
            let ((pk, pn), (sk, sn)) = (self.col(probe, c, p), self.col(keys, c, s));
            (p, s) = (pn, sn);
            null[c] == 1 || nullmask >> c & 1 != 0 || (null[c] == 0 && pk == sk)
        })
    }
}

impl SetObj {
    /// The word a LAZY build's `ensure` reads as "sealed": the one-column form's published header, else the multi mark.
    pub(crate) fn seal_word(&self) -> &Cell<u64> {
        if self.multi.is_some() { &self.mseal } else { &self.table }
    }

}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use super::*;
    use gravity_abi::status::ERROR;

    /// Two int4 columns (log2 width 2 each).
    const ARG: u64 = 2 | 2 << 24 | 2 << 26;

    fn set(rows: &[[Option<u64>; 2]]) -> Multi {
        let mut m = Multi::of(ARG).unwrap();
        for r in rows {
            let row = [r[0].unwrap_or(0), r[1].unwrap_or(0), r[0].is_none() as u64, r[1].is_none() as u64];
            // SAFETY: four words.
            unsafe { m.insert(row.as_ptr(), false) }.unwrap();
        }
        m
    }

    /// A probe column: `Some(v)` a value, `None` NULL; `X` a cross-type value outside the column's type (null word 2).
    #[derive(Clone, Copy)]
    enum P {
        V(u64),
        Null,
        X,
    }
    use P::{Null, V, X};

    fn probe(m: &Multi, p: [P; 2]) -> i64 {
        let word = |p: P| match p {
            V(v) => (v, 0),
            Null => (0, 1),
            X => (0xdead_beef, NONE),
        };
        let ((k0, w0), (k1, w1)) = (word(p[0]), word(p[1]));
        // SAFETY: four words.
        unsafe { m.probe([k0, k1, w0, w1].as_ptr()) }.unwrap()
    }

    /// NOT IN is NOT (= ANY): TRUE and FALSE swap, NULL stays.
    fn not_in(v: i64) -> i64 {
        match v {
            TRUE => FALSE,
            FALSE => TRUE,
            _ => NULL,
        }
    }

    #[test]
    fn not_in_over_an_empty_subquery_is_true_even_for_a_null_probe_column() {
        let m = set(&[]);
        for p in [[V(1), Null], [Null, Null], [V(1), V(2)], [X, V(2)]] {
            assert_eq!(not_in(probe(&m, p)), TRUE);
        }
    }

    #[test]
    fn a_probe_with_every_column_null_is_null_over_any_nonempty_set() {
        for rows in [&[[Some(1), Some(2)]][..], &[[None, Some(2)]], &[[None, None]]] {
            assert_eq!(probe(&set(rows), [Null, Null]), NULL, "{rows:?}");
        }
    }

    #[test]
    fn a_partial_match_on_the_non_null_columns_is_null_not_false() {
        let m = set(&[[Some(1), Some(2)]]);
        assert_eq!(probe(&m, [V(1), Null]), NULL, "(1, NULL) against (1, 2): NULL");
        assert_eq!(probe(&m, [V(9), Null]), FALSE, "(9, NULL) against (1, 2): FALSE");
        assert_eq!(not_in(probe(&m, [V(1), Null])), NULL);
        assert_eq!(probe(&m, [V(1), V(2)]), TRUE);
        assert_eq!(probe(&m, [V(1), V(3)]), FALSE);
    }

    #[test]
    fn set_rows_null_in_different_columns_each_match_partially() {
        let m = set(&[[None, Some(2)], [Some(5), None]]);
        assert_eq!(probe(&m, [V(7), V(2)]), NULL, "(7, 2) partially matches (NULL, 2)");
        assert_eq!(probe(&m, [V(5), V(9)]), NULL, "(5, 9) partially matches (5, NULL)");
        assert_eq!(probe(&m, [V(7), V(9)]), FALSE, "(7, 9) matches neither");
        assert_eq!(probe(&m, [Null, V(9)]), NULL, "(NULL, 9) partially matches (5, NULL)");
        assert_eq!(probe(&m, [Null, V(3)]), NULL, "(NULL, 3) partially matches (5, NULL)");
        assert_eq!(probe(&set(&[[None, Some(2)]]), [V(1), V(3)]), FALSE, "(1, 3) against (NULL, 2): 3 <> 2");
        assert_eq!(not_in(probe(&m, [V(7), V(9)])), TRUE);
    }

    #[test]
    fn duplicate_set_rows_are_one_row_and_answer_alike() {
        let once = set(&[[Some(1), Some(2)], [None, Some(4)]]);
        let twice = set(&[[Some(1), Some(2)], [Some(1), Some(2)], [None, Some(4)], [None, Some(4)]]);
        assert_eq!((twice.exact.len(), twice.nulls.len()), (1, 1));
        for p in [[V(1), V(2)], [V(1), V(4)], [V(3), V(4)], [Null, V(2)], [V(8), V(8)], [Null, Null]] {
            assert_eq!(probe(&once, p), probe(&twice, p));
        }
    }

    /// main 09-27: null word 2 (a cross-type probe value outside the set column's type) matches PG: never an exact hit,
    /// a partial match only against a NULL set cell.
    #[test]
    fn a_cross_type_value_outside_the_keys_type_matches_only_a_null_set_cell() {
        assert_eq!(probe(&set(&[[Some(1), Some(2)], [Some(0xdead_beef & 0xffff_ffff), Some(2)]]), [X, V(2)]), FALSE, "(a) no NULLs: FALSE");
        let b = set(&[[None, Some(2)]]);
        assert_eq!(probe(&b, [X, V(2)]), NULL, "(b) a row whose other column matches and whose column is NULL: NULL");
        assert_eq!(probe(&set(&[[Some(3), Some(4)]]), [X, V(2)]), FALSE, "(c) a row that matches on no column: FALSE");
        assert_eq!(probe(&set(&[[None, Some(4)]]), [X, V(2)]), FALSE, "the NULL cell matches, the other column does not");
        let d = [&[[Some(1), Some(2)]][..], &[[None, Some(2)]], &[[Some(3), Some(4)]]].map(|r| not_in(probe(&set(r), [X, V(2)])));
        assert_eq!(d, [TRUE, NULL, TRUE], "(d) NOT IN over (a) to (c)");
        assert_eq!(probe(&b, [X, Null]), NULL, "(X, NULL) against (NULL, 2): not all NULL, partial: NULL");
        assert_eq!(probe(&set(&[[Some(1), Some(2)]]), [X, Null]), FALSE, "(X, NULL) against (1, 2): X equals nothing");
    }

    #[test]
    fn a_bad_layout_or_null_word_is_refused() {
        for arg in [1 | 4 << 8, 1, 9, 2 | 1 << 8, 2 | 1 << 28, 2 | 1 << 17] {
            assert!(Multi::of(arg).is_none(), "{arg:#x}");
        }
        let mut m = Multi::of(ARG).unwrap();
        // SAFETY: four words each.
        unsafe {
            assert!(m.insert([1, 2, 2, 0].as_ptr(), false).unwrap_err().to_string().contains("null word 2 in column 0")); // alloc: Cold
            m.insert([1, 2, 0, 0].as_ptr(), false).unwrap();
            assert!(m.probe([1, 2, 0, 3].as_ptr()).unwrap_err().contains("null word 3 in column 1"));
        }
        let wide = Multi::of(2 | 3 << 24).unwrap();
        assert_eq!(wide.masks[..], [u64::MAX, 0xff]);
    }

    /// Byte keys: an int4 column then a byte-key column (`TXT`), and one byte-key column alone (`ONE`).
    const TXT: u64 = 2 | 2 << 24 | 1 << 41;
    const ONE: u64 = 1 | 1 << 40;

    /// A two-column (int4, bytes) row: the int word, the (ptr, len) pair, then the two null words.
    fn row2(i: Option<u64>, t: Option<&[u8]>) -> [u64; 5] {
        let (p, l) = t.map_or((0, 0), |t| (t.as_ptr() as u64, t.len() as u64));
        [i.unwrap_or(0), p, l, i.is_none() as u64, t.is_none() as u64]
    }

    #[test]
    fn byte_keys_equal_by_length_and_bytes_and_are_copied_at_insert() {
        let mut m = Multi::of(ONE).expect("one byte-key column");
        let owned = b"abc".to_vec();
        // SAFETY: each row is (ptr, len, null word) over live bytes.
        unsafe {
            m.insert([owned.as_ptr() as u64, 3, 0].as_ptr(), false).unwrap();
            m.insert([0, 0, 0].as_ptr(), false).unwrap();
        }
        drop(owned);
        let p = |t: &[u8]| unsafe { m.probe([t.as_ptr() as u64, t.len() as u64, 0].as_ptr()) }.unwrap();
        assert_eq!([p(b"abc"), p(b"ab"), p(b"abcd"), p(b""), p(b"ABC")], [TRUE, FALSE, FALSE, TRUE, FALSE], "the set's own copy, (len, bytes)");
        // SAFETY: a NULL probe column: its words are not read.
        assert_eq!(unsafe { m.probe([0, 7, 1].as_ptr()) }.unwrap(), NULL);
    }

    #[test]
    fn a_byte_key_column_beside_an_int_column_keeps_the_several_column_verdicts() {
        let mut m = Multi::of(TXT).expect("(int4, bytes)");
        // SAFETY: five words each, over live bytes.
        unsafe {
            for r in [row2(Some(1), Some(b"x")), row2(Some(2), None), row2(None, Some(b"yy"))] {
                m.insert(r.as_ptr(), false).unwrap();
            }
        }
        let p = |i: Option<u64>, t: Option<&[u8]>| unsafe { m.probe(row2(i, t).as_ptr()) }.unwrap();
        assert_eq!(p(Some(1), Some(b"x")), TRUE);
        assert_eq!(p(Some(2), Some(b"q")), NULL, "(2, 'q') partially matches (2, NULL)");
        assert_eq!(p(Some(7), Some(b"yy")), NULL, "(7, 'yy') partially matches (NULL, 'yy')");
        assert_eq!(p(Some(7), Some(b"y")), FALSE, "'y' is not 'yy'");
        assert_eq!(p(Some(1), None), NULL, "(1, NULL) partially matches (1, 'x')");
        assert_eq!(p(Some(9), None), NULL, "(9, NULL) partially matches (NULL, 'yy'): every column has a NULL side");
        assert_eq!(p(None, Some(b"z")), NULL, "(NULL, 'z') partially matches (2, NULL)");
        assert_eq!(p(None, None), NULL);
        let mut one = Multi::of(TXT).unwrap();
        // SAFETY: five words over live bytes.
        unsafe { one.insert(row2(Some(1), Some(b"x")).as_ptr(), false) }.unwrap();
        let q = |i: Option<u64>, t: Option<&[u8]>| unsafe { one.probe(row2(i, t).as_ptr()) }.unwrap();
        assert_eq!([q(Some(9), None), q(None, Some(b"xx")), q(None, Some(b"x"))], [FALSE, FALSE, NULL], "against (1, 'x') alone");
    }

    /// main 09-28: equal lengths differing only in the last byte are different keys; a zero-length key is a value, never
    /// NULL, and a NULL is a verdict, never an empty key.
    #[test]
    fn a_last_byte_difference_and_a_zero_length_key_versus_null() {
        let one = |rows: &[Option<&[u8]>]| {
            let mut m = Multi::of(ONE).unwrap();
            for r in rows {
                let w = r.map_or([0, 0, 1], |t| [t.as_ptr() as u64, t.len() as u64, 0]);
                // SAFETY: (ptr, len, null word) over live bytes.
                unsafe { m.insert(w.as_ptr(), false) }.unwrap();
            }
            m
        };
        let p = |m: &Multi, t: Option<&[u8]>| {
            let w = t.map_or([0, 0, 1], |t| [t.as_ptr() as u64, t.len() as u64, 0]);
            // SAFETY: as above.
            unsafe { m.probe(w.as_ptr()) }.unwrap()
        };
        let m = one(&[Some(b"abcd")]);
        assert_eq!([p(&m, Some(b"abcd")), p(&m, Some(b"abce")), p(&m, Some(b"abc\0"))], [TRUE, FALSE, FALSE], "the last byte decides");
        let empty = one(&[Some(b"")]);
        assert_eq!((empty.exact.len(), empty.nulls.len()), (1, 0), "'' is an exact row, not a NULL one");
        assert_eq!([p(&empty, Some(b"")), p(&empty, Some(b"a")), p(&empty, None)], [TRUE, FALSE, NULL]);
        let null = one(&[None]);
        assert_eq!((null.exact.len(), null.nulls.len()), (0, 1), "NULL is a NULL row, not ''");
        assert_eq!([p(&null, Some(b"")), p(&null, Some(b"a")), p(&null, None)], [NULL, NULL, NULL], "a NULL set row: no probe is TRUE");
        let both = one(&[Some(b""), None]);
        assert_eq!((both.exact.len(), both.nulls.len()), (1, 1), "'' and NULL are two rows");
        assert_eq!([p(&both, Some(b"")), p(&both, Some(b"z"))], [TRUE, NULL]);
    }

    #[test]
    fn a_byte_key_layout_or_pointer_out_of_the_face_is_refused() {
        for arg in [1 | 1 << 41, 1 | 1 << 40 | 2 << 24, 2 | 1 << 40 | 1 << 48, 9 | 1 << 40, 1 | 1 << 40 | 1 << 8] {
            assert!(Multi::of(arg).is_none(), "{arg:#x}");
        }
        let mut m = Multi::of(ONE).unwrap();
        // SAFETY: a null pointer with a length is refused before any read.
        let e = unsafe { m.insert([0, 4, 0].as_ptr(), false) }.unwrap_err().to_string(); // alloc: Cold
        assert!(e.contains("a null pointer for the 4 bytes of column 0"), "{e}");
    }

    /// Through the kernels' entries on a laid set: insert, seal, then `m8_subhash_probe` at width 8 (the sealed set is only
    /// read); a probe before the seal of a set built in program order, and a one-column set's probe, are errors by name.
    #[test]
    fn the_entries_build_seal_and_probe_a_several_column_set_at_any_width() {
        use gravity_kernels::ops::shared::object::entries as abi;
        let o = crate::ops::subplan::hash::new_set(ARG, 0, super::super::lazy::Lazy::new(Box::default())).expect("a set");
        let h = &o.head as *const abi::Head as u64;
        let mut w8: gravity_abi::Ctx = unsafe { core::mem::zeroed() };
        w8.width = 8;
        let message = || gravity_fn::take_error().map(|e| e.message().to_owned()).unwrap_or_default();
        // SAFETY: `h` is a live set; each row is four words.
        unsafe {
            assert_eq!(abi::m8_subhash_probe(&mut w8, h, [1, 2, 0, 0].as_ptr()), ERROR);
            assert_eq!(message(), "gravity: m8_subhash_run: a set built in program order (not LAZY) probed before its seal");
            for row in [[1, 2, 0, 0], [1, 2, 0, 0], [0, 4, 1, 0]] {
                assert_eq!(abi::m8_subhash_insert(core::ptr::null_mut(), h, row.as_ptr()), 0);
            }
            assert_eq!(abi::m8_subhash_insert(core::ptr::null_mut(), h, [1, 2, 3, 0].as_ptr()), ERROR);
            assert!(message().contains("null word 3 in column 0"));
            assert_eq!(abi::m8_subhash_seal(core::ptr::null_mut(), h), 0);
            assert_eq!(*(h as *const u8).add(abi::SUBHASH_TABLE_AT as usize).cast::<u64>(), 0, "no KeyTable for several columns");
            let mut p = |row: [u64; 4]| abi::m8_subhash_probe(&mut w8, h, row.as_ptr());
            assert_eq!([p([1, 2, 0, 0]), p([7, 4, 0, 0]), p([7, 5, 0, 0]), p([0, 0, 1, 1]), p([9, 4, 2, 0])], [TRUE, NULL, FALSE, NULL, NULL]);
            assert_eq!(abi::m8_subhash_exact(core::ptr::null_mut(), h), 1, "D2849: a row with no NULL key (PG's havehashrows)");
            assert_eq!(abi::m8_subhash_insert(core::ptr::null_mut(), h, [5, 5, 0, 0].as_ptr()), ERROR, "sealed");
            assert_eq!(abi::m8_subhash_reset(core::ptr::null_mut(), h), 0);
            assert_eq!(abi::m8_subhash_seal(core::ptr::null_mut(), h), 0);
            assert_eq!(p([1, 2, 0, 0]), FALSE, "after a reset: the empty set");
            assert_eq!(abi::m8_subhash_exact(core::ptr::null_mut(), h), 0, "the empty set has no exact row");
            assert_eq!(abi::m8_subhash_reset(core::ptr::null_mut(), h), 0);
            assert_eq!(abi::m8_subhash_insert(core::ptr::null_mut(), h, [0, 4, 1, 0].as_ptr()), 0);
            assert_eq!(abi::m8_subhash_seal(core::ptr::null_mut(), h), 0);
            assert_eq!(abi::m8_subhash_exact(core::ptr::null_mut(), h), 0, "only a row with a NULL key: PG's hashnulls, no hashtable row");
        }
        // unknownEqFalse (no hashnulls): rows with a NULL column are dropped; only such rows is the empty set
        let o = crate::ops::subplan::hash::new_set(ARG | abi::SUBHASH_NO_NULLS, 0, super::super::lazy::Lazy::new(Box::default())).expect("a set");
        let h = &o.head as *const abi::Head as u64;
        // SAFETY: `h` is a live set; each row is four words.
        unsafe {
            for row in [[0, 4, 1, 0], [0, 0, 1, 1]] {
                assert_eq!(abi::m8_subhash_insert(core::ptr::null_mut(), h, row.as_ptr()), 0);
            }
            assert_eq!(abi::m8_subhash_seal(core::ptr::null_mut(), h), 0);
            assert_eq!(abi::m8_subhash_probe(&mut w8, h, [0, 0, 1, 1].as_ptr()), FALSE, "only NULL rows: empty");
            assert_eq!(abi::m8_subhash_reset(core::ptr::null_mut(), h), 0);
            for row in [[0, 4, 1, 0], [1, 2, 0, 0]] {
                assert_eq!(abi::m8_subhash_insert(core::ptr::null_mut(), h, row.as_ptr()), 0);
            }
            assert_eq!(abi::m8_subhash_seal(core::ptr::null_mut(), h), 0);
            let mut p = |row: [u64; 4]| abi::m8_subhash_probe(&mut w8, h, row.as_ptr());
            assert_eq!([p([1, 2, 0, 0]), p([7, 4, 0, 0]), p([0, 0, 1, 1])], [TRUE, FALSE, NULL]);
        }
        let one = crate::ops::subplan::hash::new_set(1 | 4 << 8, 0, super::super::lazy::Lazy::new(Box::default())).expect("a set");
        // SAFETY: a live one-column set.
        unsafe { assert_eq!(abi::m8_subhash_probe(&mut w8, &one.head as *const abi::Head as u64, [1, 0].as_ptr()), ERROR) };
        assert_eq!(message(), "gravity: m8_subhash_probe: a one-column set (its probe is the IR's, over its KeyTable)");
    }
}
