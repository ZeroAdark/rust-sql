//! Record-image order (matview#113 / #191 / #251: REFRESH MATERIALIZED VIEW CONCURRENTLY's merge joins on `*=` over
//! Sorts by `*<`): a Sort's key of a non-default btree family is declared under that family (`Order::fams`, from
//! `Facts::op_family`), and only a consumer asking that family's order matches it (`ordered_as_order`, a merge join whose
//! clause family is not its side type's default, `Facts::default_family`); a default-family consumer never takes it for
//! its own order, nor a record-image merge a default-family sort.

use gravity_decompose::graph::{Col, Order, OrderKey};
use gravity_decompose::ops::shared::order::{leads_with, ordered_as, ordered_as_order};
use gravity_decompose::Kind;

const RECORD_IMAGE_OPS: u32 = 3194;

fn key(attno: i16) -> OrderKey {
    OrderKey { col: Col::outer(attno), desc: false, nulls_first: false }
}

#[test]
fn a_record_image_order_is_matched_only_under_its_family() {
    // Sort Key: mv.type, mv.* USING *<
    let mut sorted = Order::NONE;
    sorted.push(key(1));
    sorted.push_fam(key(2), RECORD_IMAGE_OPS);
    // Merge Cond: mv.type = newdata.type AND mv.* *= newdata.*
    let mut image = Order::NONE;
    image.push(key(1));
    image.push_fam(key(2), RECORD_IMAGE_OPS);
    assert!(ordered_as_order(&sorted, None, &image, Kind::MergeJoin, 1, "none", "other").is_ok(), "the merge by the family takes the sort by it");
    // a default-family consumer of the same columns (a merge on mv.* = newdata.*, a sorted group, a window) does not
    let mut plain_want = Order::NONE;
    plain_want.push(key(1));
    plain_want.push(key(2));
    assert!(ordered_as_order(&sorted, None, &plain_want, Kind::MergeJoin, 1, "none", "other").is_err());
    assert!(ordered_as(&sorted, None, &[key(1), key(2)], Kind::MergeJoin, 1, "none", "other").is_err());
    let mut image_only = Order::NONE;
    image_only.push_fam(key(2), RECORD_IMAGE_OPS);
    assert!(!leads_with(&image_only, None, &[key(2)], false), "not even for equal runs: rows = under record_ops need not be adjacent in *< order");
    assert!(leads_with(&sorted, None, &[key(1)], true), "the default-family prefix still stands");
    // a record-image merge over a sort by the default family's order refuses
    let mut plain = Order::NONE;
    plain.push(key(1));
    plain.push(key(2));
    assert!(ordered_as_order(&plain, None, &image, Kind::MergeJoin, 1, "none", "other").is_err());
    assert!(ordered_as_order(&plain, None, &plain_want, Kind::MergeJoin, 1, "none", "other").is_ok(), "default against default as before");
    // a wanted key repeated under another family is a key of its own
    let mut twice = Order::NONE;
    twice.push(key(2));
    twice.push_fam(key(2), RECORD_IMAGE_OPS);
    assert!(ordered_as_order(&plain, None, &twice, Kind::MergeJoin, 1, "none", "other").is_err());
    // an equality (the default family's `=`: numeric 1.0 = 1.00) carries the default order to its other column, never the image order
    let mut eq_image = Order::NONE;
    eq_image.push_fam(key(2), RECORD_IMAGE_OPS);
    eq_image.push_eq(Col::outer(2), Col::outer(3));
    let mut want3 = Order::NONE;
    want3.push_fam(key(3), RECORD_IMAGE_OPS);
    assert!(ordered_as_order(&eq_image, None, &want3, Kind::MergeJoin, 1, "none", "other").is_err());
    let mut eq_plain = Order::NONE;
    eq_plain.push(key(2));
    eq_plain.push_eq(Col::outer(2), Col::outer(3));
    assert!(ordered_as(&eq_plain, None, &[key(3)], Kind::MergeJoin, 1, "none", "other").is_ok(), "the default order goes by its equalities as before");
    // an order led by an image key is no known order to a default-family consumer (its refusals name "no known order")
    assert!(!image_only.is_known() && sorted.is_known() && eq_image.n == 1);
    // the dump names a non-default family
    let mut s = String::new();
    sorted.dump(&mut s).unwrap();
    assert_eq!(s, "1+,2+@3194");
}
