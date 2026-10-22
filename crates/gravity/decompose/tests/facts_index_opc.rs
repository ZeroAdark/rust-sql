//! D2063 (V4R-578): `IndexOpc` reads a column as initdb only when the source knows it and the column is a key column;
//! a source without the catalog answers `UNKNOWN`, every column a user's.
use gravity_decompose::catalog::{IndexOpc, NoFacts};
use gravity_decompose::Facts;

#[test]
fn unknown_reads_every_column_as_a_users() {
    let u = IndexOpc::UNKNOWN;
    assert!(!u.known && !u.col(0) && !u.through(0) && !u.all());
    assert_eq!(NoFacts.index_opc(16384), IndexOpc::UNKNOWN);
    let forged = IndexOpc { builtin: 0b111, nkeys: 3, known: false };
    assert!(!forged.col(0) && !forged.through(2) && !forged.all(), "bits without knowledge are not a pass");
}

#[test]
fn col_through_and_all() {
    let o = IndexOpc { builtin: 0b101, nkeys: 3, known: true };
    assert!(o.col(0) && !o.col(1) && o.col(2) && !o.col(3));
    assert!(o.through(0) && !o.through(1) && !o.through(2), "a key-less middle column fails every prefix past it (skip scan)");
    assert!(!o.all());
    let o = IndexOpc { builtin: 0b11, nkeys: 2, known: true };
    assert!(o.all() && o.through(1) && !o.through(2) && !o.col(2), "INCLUDE columns past nkeys have no operator class");
    let o = IndexOpc { builtin: 0, nkeys: 0, known: true };
    assert!(!o.all() && !o.col(0), "an index with no key column");
    let o = IndexOpc { builtin: u32::MAX, nkeys: 32, known: true };
    assert!(o.all() && o.through(31) && !o.through(32) && !o.col(32));
}
