// The type-facts table: its rows are well formed, and every type a corpus key or scan lane uses has a row.
use std::collections::BTreeMap;
use std::fs;

use gravity_abi::breaker::KeyKind;
use gravity_decompose::catalog::typefacts::{facts_of, key_of, opaque, row_of, unify, KeyColl, WordImage, ROWS};
use gravity_decompose::{corpus, decompose_in, Facts, NoFacts, Op};
use gravity_program::{Arena, LaneId, NumTypmod};
use mcx::MemoryContext;

mod harness;

#[test]
fn rows_are_sorted_unique_and_well_formed() {
    for w in ROWS.windows(2) {
        assert!(w[0].typid < w[1].typid, "rows sorted by oid: {} then {}", w[0].typid, w[1].typid);
    }
    for r in ROWS {
        let words = match (r.kind, r.image) {
            (KeyKind::Fp128, _) | (_, WordImage::Fixed { .. }) => 2,
            _ => 1,
        };
        assert_eq!(r.words, words, "{}: words follow the kind", r.typid);
        if r.byval {
            assert!((1..=8).contains(&r.typlen), "{}: a by-value type is 1-8 bytes", r.typid);
            assert!(matches!(r.image, WordImage::Raw { .. } | WordImage::Float { .. } | WordImage::Enum), "{}: a by-value image is its word", r.typid);
        }
        assert!(r.input != 0 && r.output != 0, "{}: io procs", r.typid);
        assert_eq!(r.base, r.typid);
        if matches!(r.image, WordImage::Bytes { text: true, .. }) {
            assert!(r.collatable, "{}: a text image is collatable", r.typid);
        }
    }
}

#[test]
fn procs_match_the_catalog() {
    let int4 = row_of(23).unwrap();
    assert_eq!((int4.eq, int4.cmp, int4.hash, int4.input, int4.output), (65, 351, 450, 42, 43));
    let text = row_of(25).unwrap();
    assert_eq!((text.eq, text.cmp, text.hash), (67, 360, 400));
    assert_eq!((row_of(1043).unwrap().eq, row_of(1043).unwrap().hash), (67, 400), "varchar uses text's opclasses");
    let num = row_of(1700).unwrap();
    assert_eq!((num.eq, num.cmp, num.hash), (1718, 1769, 432));
    assert_eq!(row_of(2950).unwrap().typlen, 16);
    assert!(row_of(424242).is_none());
}

#[test]
fn numeric_keys_by_mantissa_only_when_narrow() {
    let num = row_of(1700).unwrap();
    assert_eq!(key_of(num, NumTypmod::of(15, 2)), (KeyKind::I64, WordImage::NumMantissa, 1));
    assert_eq!(key_of(num, NumTypmod::of(25, 2)), (KeyKind::Fp128, WordImage::NumNormalized, 2), "D197");
    assert_eq!(key_of(num, NumTypmod::NONE), (KeyKind::Fp128, WordImage::NumNormalized, 2));
}

#[test]
fn only_canonical_images_are_lossy() {
    for r in ROWS {
        assert_eq!(r.image.equality(), r.image != WordImage::Canonical, "{}", r.typid);
    }
}

#[test]
fn unlisted_types_key_by_storage() {
    assert_eq!(opaque(90000, 90000, 4, true, false).kind, KeyKind::I32);
    assert_eq!(opaque(90000, 90000, 8, true, false).kind, KeyKind::I64);
    assert_eq!(opaque(90000, 90000, 4, true, true).image, WordImage::Enum);
    assert_eq!(opaque(90000, 90000, -1, false, false).image, WordImage::Canonical);
    assert!(facts_of(&NoFacts, 90000).is_none());
    assert_eq!(facts_of(&NoFacts, 23).unwrap().kind, KeyKind::I32);
}

/// A catalog with one enum (90001), a domain over it (90002) and a domain over int4 (90003) (V4R-84).
struct Catalog;

impl Facts for Catalog {
    fn rel(&self, _: u32) -> Option<gravity_decompose::RelFact> {
        None
    }
    fn att(&self, _: u32, _: i16) -> Option<gravity_decompose::AttFact> {
        None
    }
    fn index(&self, _: u32) -> Option<gravity_decompose::IndexFact> {
        None
    }
    fn indexes_of(&self, _: u32, _: &mut dyn FnMut(gravity_decompose::IndexFact)) {}
    fn index_expr(&self, _: u32, _: u8) -> Option<gravity_decompose::ExprFp> {
        None
    }
    fn type_row(&self, typid: u32) -> Option<gravity_decompose::TypeRow> {
        let row = |typtype, base| Some(gravity_decompose::TypeRow { typlen: 4, byval: true, typtype, base });
        match typid {
            90001 => row(b'e', 90001),
            90002 => row(b'd', 90001),
            90003 => row(b'd', 23),
            _ => None,
        }
    }
}

/// V4R-84: a key over an unlisted type takes the catalog's facts. An enum keys as its oid word (never the opaque
/// by-reference row), a domain as its base (a domain over an enum is an enum) under its own typid, and every ORDER key
/// over an enum takes the by-reference image: it orders by enumsortorder, never by the oid word.
#[test]
fn keys_take_the_catalogs_facts() {
    use gravity_decompose::ops::shared::sort::KeyImage;
    use gravity_decompose::ops::sort::order_image;
    use gravity_decompose::ops::shared::sort::key_kind_of;
    use gravity_decompose::ops::shared::sort::SortKey;
    let e = facts_of(&Catalog, 90001).unwrap();
    assert_eq!((e.image, e.kind, e.byval), (WordImage::Enum, KeyKind::I32, true));
    let de = facts_of(&Catalog, 90002).unwrap();
    assert_eq!((de.typid, de.image, de.kind), (90002, WordImage::Enum, KeyKind::I32), "a domain over an enum keys as the enum, under its own typid");
    let di = facts_of(&Catalog, 90003).unwrap();
    assert_eq!((di.typid, di.image, di.kind, di.eq), (90003, row_of(23).unwrap().image, KeyKind::I32, row_of(23).unwrap().eq), "a domain over int4 is int4's row");
    assert_eq!(key_kind_of(&Catalog, 90001), KeyKind::I32, "an enum's equality key is its word");
    assert_eq!(key_kind_of(&NoFacts, 90001), KeyKind::Fp128, "a type nothing knows keys by its canonical image");
    for t in [90001, 90002] {
        assert_eq!(order_image(&Catalog, t, KeyKind::I32, KeyImage::Raw), (KeyKind::Fp128, KeyImage::ByRef), "{t}: an enum orders by enumsortorder");
        let k = SortKey::of_type(&Catalog, 1, 3518, false, false, t, KeyColl::of(0));
        assert_eq!((k.kind, k.image), (KeyKind::Fp128, KeyImage::ByRef), "{t}: a sort key over an enum is never its oid word");
    }
    assert_eq!(order_image(&Catalog, 90003, KeyKind::I32, KeyImage::Raw), (KeyKind::I32, KeyImage::Raw), "a domain over int4 orders as int4");
}

#[test]
fn unify_widens_within_a_family() {
    use KeyKind::*;
    for (a, b, u) in [(I32, I64, Some(I64)), (F32, F64, Some(F64)), (I32, Fp128, None), (Date, Date, Some(Date)), (Date, Timestamp, None)] {
        assert_eq!(unify(a, b), u);
        assert_eq!(unify(b, a), u, "symmetric");
    }
}

#[test]
fn collations_read_once() {
    assert!(KeyColl::of(0).bytes_order && KeyColl::of(950).bytes_order);
    assert!(KeyColl::of(100).bytes_eq && !KeyColl::of(100).bytes_order);
    assert!(!KeyColl::of(20000).bytes_eq);
}

/// Every built-in type a corpus scan lane or group key uses has a row; initdb's and users' types (information_schema
/// domains, enums, composites) come from `Facts::type_row`.
#[test]
fn every_corpus_type_has_a_row() {
    let mut missing: BTreeMap<u32, usize> = BTreeMap::new();
    let mut files = harness::fixtures_in(&harness::plans_root());
    files.extend(harness::fixtures_in(&harness::plans_root().join("regress-full")));
    for f in files {
        let text = fs::read_to_string(&f).unwrap();
        let ctx = MemoryContext::new("fixture");
        let Ok(fx) = corpus::load(ctx.mcx(), &text) else { continue };
        let arena = Arena::new(1 << 20);
        let Ok(g) = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx) else { continue };
        let mut note = |t: u32| {
            if row_of(t).is_none() {
                *missing.entry(t).or_default() += 1;
            }
        };
        for p in g.pipelines.iter() {
            let mut sources = vec![&p.source];
            for op in p.ops.iter() {
                if let Op::Join(gravity_decompose::join::JoinDesc::Nest { inner, .. }) = op {
                    sources.push(&inner.source);
                }
            }
            for s in sources {
                if let Some((_, relid, site)) = s.relation() {
                    for l in site.lanes() {
                        if let LaneId::Attr(a) = l.id {
                            if let Some(att) = fx.facts.att(relid, a) {
                                note(att.typid);
                            }
                        }
                    }
                }
            }
        }
        for gd in g.groups.iter() {
            for k in gd.keys.iter() {
                if let Some(t) = gravity_decompose::catalog::expr_type::type_of_listed(k.expr.node()).or_else(|| gravity_decompose::catalog::expr_type::type_of(k.expr.node())) {
                    note(t);
                }
            }
        }
    }
    let got: Vec<String> = missing.iter().map(|(t, n)| format!("{t}:{n}")).collect();
    println!("corpus types without a row: {}", got.join(" "));
    let unexpected: Vec<&u32> = missing.keys().filter(|t| **t < FIRST_INITDB_OID).collect();
    assert!(unexpected.is_empty() || harness::filter().is_some(), "corpus types with no type-facts row: {unexpected:?} (add a row, or list a storage-only type)");
}

const FIRST_INITDB_OID: u32 = 10000;
