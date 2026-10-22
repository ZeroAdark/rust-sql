//! Wave 3 A3 (cvformat): a meta builder made with an owner charges its own heap to that owner as it grows, fitted at
//! each granule's seal and each part step, gives it back with the builder, and builds the same bytes as one without.

use super::{Rng, TestColumn, TestValue};
use crate::builder::ColumnMeta;
use crate::format::abi::ColumnMetaBuilder;
use crate::format::class::{CollationClass, StorageClass};
use crate::profile::{MetaProfile, TypeSemantics};
use memledger::charge::{context, owner_row};
use memledger::{bind_backend, statement_begin, statement_end, unbind_backend, Kind, Owner};

fn int_class() -> StorageClass {
    StorageClass::ByvalWord { width: 8, signed: true }
}

fn text_class() -> StorageClass {
    StorageClass::VarlenaVerbatim
}

fn committed(o: Owner) -> i64 {
    owner_row(o).committed
}

/// Four granules of 8192 rows: shuffled low-NDV words (bloom blocks, HLLs, PSMA) or distinct strings (the hash arm's
/// value arena).
fn granules(text: bool) -> Vec<TestColumn> {
    let mut rng = Rng::new(0xC0F0_0A03);
    (0..4u64)
        .map(|g| {
            let mut rows: Vec<Option<TestValue>> = (0..8192u64)
                .map(|i| {
                    if text {
                        Some(TestValue::Bytes(format!("value-{g}-{i}-padding-padding").into_bytes()))
                    } else if i % 97 == 0 {
                        None
                    } else {
                        Some(TestValue::Word((i * 7 + g) % 1500))
                    }
                })
                .collect();
            rng.shuffle(&mut rows);
            TestColumn::new(if text { text_class() } else { int_class() }, &rows)
        })
        .collect()
}

fn profile(text: bool) -> MetaProfile {
    if text {
        MetaProfile::derive(text_class(), CollationClass::C, TypeSemantics::TextCollated).unwrap()
    } else {
        MetaProfile::derive(int_class(), CollationClass::C, TypeSemantics::SignedInt).unwrap()
    }
}

#[allow(clippy::type_complexity)]
fn run(b: &mut ColumnMeta, cols: &[TestColumn], mut each: impl FnMut(&ColumnMeta)) -> (Vec<(crate::format::part::SectionKind, Vec<u8>)>, Option<crate::format::sidecar::ColDistribution>) {
    for (g, c) in cols.iter().enumerate() {
        b.observe_granule(&c.input(), g as u32);
        each(b);
    }
    for g in 0..cols.len() as u32 {
        b.seal_granule(g);
        each(b);
    }
    b.seal_band(0);
    b.seal_part();
    each(b);
    let aux = b.aux_sections();
    each(b);
    let d = b.distribution();
    each(b);
    (aux, d)
}

#[test]
fn a_builder_with_an_owner_charges_its_heap_and_gives_it_back() {
    std::thread::spawn(|| {
        bind_backend(3971, 8171);
        statement_begin();
        let (o, _) = context();
        for text in [false, true] {
            let cols = granules(text);
            let mut b = ColumnMeta::with_owner(profile(text), Some((o, Kind::Region)));
            assert_eq!(b.owner(), Some((o, Kind::Region)));
            let mut peak = 0u64;
            let got = run(&mut b, &cols, |b| {
                assert_eq!(committed(o), b.charged_bytes() as i64, "the owner holds exactly the builder's fitted bytes");
                peak = peak.max(b.charged_bytes());
            });
            // 4 sealed granules hold at least their HLL register blocks (1 KiB each); the text arm's accumulator holds
            // the 32768 distinct values' bytes.
            assert!(peak >= 4 * 1024, "text={text}: peak {peak}");
            if text {
                assert!(peak >= 32768 * 20, "text: the value arena is charged: {peak}");
            }
            let mut plain = ColumnMeta::new(profile(text));
            assert_eq!(plain.owner(), None);
            let want = run(&mut plain, &cols, |p| assert_eq!(p.charged_bytes(), 0));
            assert_eq!(got, want, "text={text}: the same sections and sketch with or without an owner");
            drop(b);
            assert_eq!(committed(o), 0, "text={text}: given back with the builder");
        }
        statement_end();
        unbind_backend();
    })
    .join()
    .unwrap();
}

/// D4539: the distinct-set accumulator's own charge follows its blocks at every value, not at the granule's end: a slot
/// table that doubles inside a granule (48 MiB on a seal thread in m1_d327_wide_spill) is on the owner as soon as it
/// exists, on both arms, and goes back with the accumulator.
#[test]
fn the_accumulator_charges_each_grow_as_it_happens() {
    std::thread::spawn(|| {
        bind_backend(3973, 8173);
        statement_begin();
        let (o, _) = context();
        for words in [true, false] {
            let mut d = if words { crate::sketch::DistAcc::for_words() } else { crate::sketch::DistAcc::with_hash_arm(true) };
            d.charge_to(Some((o, Kind::Region)));
            let (mut grows, mut last) = (0u32, d.heap_bytes());
            for i in 0..100_000u64 {
                let b = i.wrapping_mul(0x9E37_79B9_7F4A_7C15).to_le_bytes();
                let (h1, h2) = crate::hash::meta_hash128(&b);
                if words {
                    d.observe_hashed_word(h1, h2, u64::from_le_bytes(b), 8);
                } else {
                    d.observe_hashed(h1, h2, &b);
                }
                let now = d.heap_bytes();
                grows += u32::from(now != last);
                last = now;
                assert_eq!(committed(o), now as i64, "words={words}: the owner holds the accumulator's blocks at value {i}");
                assert_eq!(d.charged(), now);
            }
            assert!(grows >= 10, "words={words}: the table grew {grows} times");
            assert!(last >= 100_000 * 8, "words={words}: {last} bytes");
            drop(d);
            assert_eq!(committed(o), 0, "words={words}: given back with the accumulator");
        }
        statement_end();
        unbind_backend();
    })
    .join()
    .unwrap();
}

/// D4545: a working vector the sketch's finish fills (`collect_on`, the one-length keyed table) is the owner's before its
/// fill's first element, not once the fill ends: the fill of a 1M-entry distinct set ran for tens of ms with 16 MiB on
/// the seal thread's account, unattributed.
#[test]
fn a_finish_working_vector_is_the_owners_before_its_fill() {
    std::thread::spawn(|| {
        // a backend slot (procno < ACCOUNT_SLOTS - BACKEND_SLOT_BASE; 3971 / 3973 are the siblings'), not UNBOUND
        bind_backend(3975, 8175);
        statement_begin();
        let (o, _) = context();
        let n = 1usize << 20;
        let first = std::cell::Cell::new(-1i64);
        let v = crate::kept::collect_on(
            Some((o, Kind::Region)),
            (0..n).map(|i| {
                if i == 0 {
                    first.set(committed(o));
                }
                i as u64
            }),
        )
        .expect("a 1 Mi-entry reserve");
        assert_eq!(first.get(), (n * 8) as i64, "the block is charged before the fill starts");
        assert_eq!(committed(o), (v.capacity() * 8) as i64, "and stays fitted to its capacity after it");
        drop(v);
        assert_eq!(committed(o), 0, "given back with the vector");
        statement_end();
        unbind_backend();
    })
    .join()
    .unwrap();
}
