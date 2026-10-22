//! KeySem slice 4: the corpus fixtures' shared image equalities (decompose src/corpus/imageeq.facts, test data every
//! fixture reads) are the key home's list (gravity_fn typesem `keysem::image`, what the server answers
//! `Facts::op_image_eq` from), row for row. This crate's tests see both decompose's corpus and the home.
use gravity_decompose::corpus::{FixtureFacts, SHARED_IMAGEEQ_LINES};
use gravity_decompose::Facts;
use gravity_fn::typesem::keysem::image;

#[test]
fn shared_fixture_rows_are_the_key_homes_rows() {
    let mut f = FixtureFacts::default();
    for line in SHARED_IMAGEEQ_LINES.lines().filter(|l| l.starts_with("-- imageeq ")) {
        assert!(f.line(&line[3..]).expect("a well-formed imageeq line"), "an imageeq line: {line}");
    }
    let mut home: Vec<(u32, u32, u32)> = image::IMAGE_EQ.iter().map(|&(op, t)| (op, t, t)).chain(image::INT_CROSS_EQ.iter().copied()).collect();
    let mut file = f.image_eqs.clone();
    home.sort_unstable();
    file.sort_unstable();
    assert_eq!(file, home, "decompose src/corpus/imageeq.facts is not the key home's list (gravity_fn typesem keysem::image)");
    for &(op, l, r) in &home {
        assert_eq!(f.op_image_eq(op), Some((l, r)), "operator {op}");
        assert_eq!(f.op_image_eq(op), image::eq(op), "operator {op}");
    }
    assert_eq!(f.op_image_eq(98), None, "texteq is no image equality");
}
