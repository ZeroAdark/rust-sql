//! `seal::cmp`: the cluster sort's comparator, one cell at a time (`key_slot_at` / `slot_cmp`), equals its whole-column
//! form (`key_slots` / `key_cmp`), with PG's NULL placement and float law.

use super::*;
use crate::ingest::ColBuffer;
use crate::seal::cmp::{key_cmp, key_slot_at, key_slots, slot_cmp, KeySlot};
use crate::structural::{ClusterKeyDecl, NullsOrder, SortDir};
use std::cmp::Ordering::{self, Equal, Greater, Less};

fn decl(dir: SortDir, nulls: NullsOrder) -> ClusterKeyDecl {
    ClusterKeyDecl { attno: 1, dir, nulls, path: Vec::new() }
}

fn decls() -> [ClusterKeyDecl; 4] {
    [
        decl(SortDir::Asc, NullsOrder::Last),
        decl(SortDir::Asc, NullsOrder::First),
        decl(SortDir::Desc, NullsOrder::Last),
        decl(SortDir::Desc, NullsOrder::First),
    ]
}

fn f64_col(attno: u32) -> ColSchema {
    ColSchema {
        attno,
        class: StorageClass::F64,
        typlen: 8,
        typbyval: true,
        typalign: b'd',
        collation_class: CollationClass::C,
        semantics: TypeSemantics::Float,
    }
}

fn int8_buf(vals: &[Option<i64>]) -> ColBuffer {
    let mut c = ColBuffer::new(int8_col(1));
    for v in vals {
        match v {
            Some(x) => c.append_word(*x as u64).unwrap(),
            None => c.append_null(),
        }
    }
    c
}

fn f64_buf(vals: &[Option<f64>]) -> ColBuffer {
    let mut c = ColBuffer::new(f64_col(1));
    for v in vals {
        match v {
            Some(x) => c.append_word(x.to_bits()).unwrap(),
            None => c.append_null(),
        }
    }
    c
}

fn text_buf(vals: &[Option<&str>]) -> ColBuffer {
    let mut c = ColBuffer::new(text_col(1));
    for v in vals {
        match v {
            Some(x) => c.append_varlena_payload(x.as_bytes()).unwrap(),
            None => c.append_null(),
        }
    }
    c
}

fn fixed_buf(vals: &[Option<[u8; 16]>]) -> ColBuffer {
    let mut c = ColBuffer::new(fixed16_col(1));
    for v in vals {
        match v {
            Some(x) => c.append_fixed(x).unwrap(),
            None => c.append_null(),
        }
    }
    c
}

/// Every pair of rows: the cell form equals the column form, under each of the four declarations.
fn cells_equal_columns(c: &ColBuffer) {
    let slots = key_slots(c).unwrap();
    assert_eq!(slots.len() as u64, c.rows());
    for d in &decls() {
        for a in 0..c.rows() {
            for b in 0..c.rows() {
                let one = slot_cmp(&key_slot_at(c, a).unwrap(), &key_slot_at(c, b).unwrap(), d);
                assert_eq!(one, key_cmp(&slots, d, a as usize, b as usize), "rows {a} {b} {:?} {:?}", d.dir, d.nulls);
            }
        }
    }
}

#[test]
fn cell_comparator_equals_the_column_comparator() {
    cells_equal_columns(&int8_buf(&[Some(3), None, Some(-7), Some(i64::MIN), Some(i64::MAX), Some(3), None, Some(0)]));
    cells_equal_columns(&f64_buf(&[Some(1.5), Some(f64::NAN), None, Some(-0.0), Some(0.0), Some(f64::NEG_INFINITY), Some(f64::INFINITY), Some(-2.0)]));
    cells_equal_columns(&text_buf(&[Some("b"), Some(""), None, Some("a"), Some("ab"), Some("B"), Some("\u{e9}"), Some("a")]));
    cells_equal_columns(&fixed_buf(&[Some([1; 16]), None, Some([0; 16]), Some([0xff; 16]), Some([1; 16])]));
}

#[test]
fn nulls_place_absolutely_and_desc_reverses_values_only() {
    let n = KeySlot::Null;
    let one = KeySlot::I(1);
    let two = KeySlot::I(2);
    let cases: [(SortDir, NullsOrder, Ordering, Ordering); 4] = [
        (SortDir::Asc, NullsOrder::Last, Greater, Less),
        (SortDir::Asc, NullsOrder::First, Less, Less),
        (SortDir::Desc, NullsOrder::Last, Greater, Greater),
        (SortDir::Desc, NullsOrder::First, Less, Greater),
    ];
    for (dir, nulls, null_vs_value, one_vs_two) in cases {
        let d = decl(dir, nulls);
        assert_eq!(slot_cmp(&n, &one, &d), null_vs_value, "{dir:?} {nulls:?}");
        assert_eq!(slot_cmp(&one, &n, &d), null_vs_value.reverse(), "{dir:?} {nulls:?}");
        assert_eq!(slot_cmp(&n, &n, &d), Equal);
        assert_eq!(slot_cmp(&one, &two, &d), one_vs_two, "{dir:?} {nulls:?}");
    }
}

#[test]
fn floats_follow_pg_float_order() {
    let d = decl(SortDir::Asc, NullsOrder::Last);
    let f = KeySlot::F;
    assert_eq!(slot_cmp(&f(f64::NAN), &f(f64::NAN), &d), Equal);
    assert_eq!(slot_cmp(&f(f64::NAN), &f(f64::INFINITY), &d), Greater);
    assert_eq!(slot_cmp(&f(-0.0), &f(0.0), &d), Equal);
    assert_eq!(slot_cmp(&f(f64::NAN), &KeySlot::Null, &d), Less);
    assert_eq!(slot_cmp(&KeySlot::U(u64::MAX), &KeySlot::U(1), &d), Greater);
    assert_eq!(slot_cmp(&KeySlot::B(b"ab"), &KeySlot::B(b"b"), &d), Less);
    assert_eq!(slot_cmp(&KeySlot::B(b""), &KeySlot::B(b"\0"), &d), Less);
}
