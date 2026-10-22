//! TY-3 shredded-jsonb READ SUPPLY gates (the lanev4 JSON-band substrate:
//! typed path lanes served as scan columns through the one staging/emit
//! path, dark until routed — zero cells here).
//!
//! Differential-twin discipline (the house law: differential-vs-C where C
//! exists; HERE the twin is the NORMAL read path on the same data): every
//! lane-served value stream is compared BYTE-IDENTICAL against a plain
//! part whose root column carries the extracted values — same rows, same
//! grain, same canon currency. The trichotomy twin pins the NULL law
//! (SQL-NULL / jsonb-null / absent / exception all present as lane NULLs
//! — the residual, not the lane, separates them; a consumer needing the
//! `->` jsonb-null-vs-absent distinction must NOT route to lanes: the
//! recognizer admission law). The refusal suite is the typed-refusal
//! guard's teeth: absent election, unelected path, kind mismatch, the
//! NumericFs scale gap (spec §6.5), non-lane declared class — each a
//! deterministic `ReadError::ShredLaneRefused`, never a fabricated NULL
//! column. The dirsha law rides: serial vs DOP-4-skew byte identity over
//! multi-part scans whose parts elect DIFFERENT path_ords for the same
//! path (per-part resolution witnessed, not assumed).

use std::sync::Arc;
use std::sync::Once;

use mcx::MemoryContext;
use pgrc_format::dirlayout::part_file_name;
use pgrc_format::shredlane::ShredLaneKind;
use pgrc_qa::adapters::{full_binding, QaResolver};
use pgrc_qa::corpus::open_part_bytes;
use pgrc_read::{OpenPart, ReadError};
use pgrc_write::dict::TextSemantics;
use pgrc_write::elect::{CandidateSource, CodecCandidates, ColumnPosture, DictPolicy};
use pgrc_write::ingest::RawDatum;
use pgrc_write::publish::TxnVerdict;
use pgrc_write::shred_jsonb::JsonbShredSource;
use pgrc_write::testkit::{
    bool_col, img_4b_u, mem_with_dir, open_writer, stamp, text_col, Kit, Probe, UnshreddedDocs, DIR,
};
use pgrc_write::writer::SealEnv;
use pgrc_write::wvfs::WriteVfs;

use crate::scan::{ScanColumn, ScanOptions, TableScan};

fn setup() {
    let _ = mbutils::SetDatabaseEncoding(wchar::PG_UTF8);
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        mbutils::init_seams();
    });
}

/// One row of the standard corpus shape: `None` = SQL NULL, `Some(doc)` =
/// a jsonb document text.
type Docs = Vec<Option<String>>;

/// The standard doc: two auto-electable scalar paths bracketing the text
/// path ("flag" Bool, "name" Text, "score" NumericFs) — canonical
/// (length-then-bytes) election order flag=1, name=2, score=3.
fn std_doc(i: u64) -> String {
    format!(
        r#"{{"flag":{},"name":"tok{:04}","score":{}.25}}"#,
        i % 2 == 0,
        i % 97,
        i % 1000
    )
}

fn std_docs(rows: u64) -> Docs {
    (0..rows)
        .map(|i| {
            if i % 13 == 5 {
                None
            } else {
                Some(std_doc(i))
            }
        })
        .collect()
}

/// Seal `docs` as one part through the REAL writer with the production
/// shred source (`with_shred` = the two-arm election's shred arm; false =
/// NoShred — the lanes-free twin of the very same documents). Optional
/// extra candidate source (the dict-lane arm registers postures here).
fn seal_jsonb(docs: &Docs, fxid: u64, with_shred: bool, extra: Option<&CodecCandidates>) -> Vec<u8> {
    setup();
    let ctx = MemoryContext::new("scan-shred-test");
    let mcx = ctx.mcx();
    let images: Vec<Option<Vec<u8>>> = docs
        .iter()
        .map(|d| {
            d.as_ref().map(|d| {
                adt_jsonb::io::jsonb_in(mcx, d.as_bytes(), None)
                    .unwrap_or_else(|e| panic!("jsonb_in {d:?}: {}", e.message()))
                    .expect("hard path")[..]
                    .to_vec()
            })
        })
        .collect();
    let mut vfs = mem_with_dir();
    let mut kit = Kit::new();
    let mut jshred = JsonbShredSource::new().with_column(1);
    let mut unshredded = UnshreddedDocs;
    let mut w = open_writer(vec![text_col(1)], stamp(fxid, 1));
    for img in &images {
        let sources: Vec<&dyn CandidateSource> = match extra {
            Some(c) => vec![c, &kit.cands],
            None => vec![&kit.cands],
        };
        let mut env = SealEnv {
            vfs: &mut vfs,
            sources: &sources,
            resolver: &QaResolver,
            shred: if with_shred { &mut jshred } else { &mut unshredded },
            shred_opts: &kit.opts,
        };
        let d = match img {
            None => RawDatum::Null,
            Some(b) => RawDatum::Bytes(b),
        };
        w.append_row(&[d], &mut kit.ext, &mut env).expect("append");
    }
    let sources: Vec<&dyn CandidateSource> = match extra {
        Some(c) => vec![c, &kit.cands],
        None => vec![&kit.cands],
    };
    let mut env = SealEnv {
        vfs: &mut vfs,
        sources: &sources,
        resolver: &QaResolver,
        shred: if with_shred { &mut jshred } else { &mut unshredded },
        shred_opts: &kit.opts,
    };
    w.finish(&mut env).expect("finish");
    let probe = Probe::new(TxnVerdict::InProgress).set(fxid, TxnVerdict::Committed);
    w.publish(&mut vfs, &probe).expect("publish");
    vfs.read_full(&format!("{DIR}/{}", part_file_name(0)))
        .expect("part bytes")
}

/// Seal the PLAIN twin: one root column carrying `rows` — the values the
/// lane is expected to serve, through the ordinary (non-shred) path.
fn seal_plain(
    schema: pgrc_format::class::ColSchema,
    rows: &[Option<RawDatum<'_>>],
    fxid: u64,
) -> Vec<u8> {
    setup();
    let mut vfs = mem_with_dir();
    let mut kit = Kit::new();
    let mut w = open_writer(vec![schema], stamp(fxid, 1));
    for d in rows {
        let sources: [&dyn CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv {
            vfs: &mut vfs,
            sources: &sources,
            resolver: &QaResolver,
            shred: &mut kit.shred,
            shred_opts: &kit.opts,
        };
        let d = d.unwrap_or(RawDatum::Null);
        w.append_row(&[d], &mut kit.ext, &mut env).expect("append");
    }
    let sources: [&dyn CandidateSource; 1] = [&kit.cands];
    let mut env = SealEnv {
        vfs: &mut vfs,
        sources: &sources,
        resolver: &QaResolver,
        shred: &mut kit.shred,
        shred_opts: &kit.opts,
    };
    w.finish(&mut env).expect("finish");
    let probe = Probe::new(TxnVerdict::InProgress).set(fxid, TxnVerdict::Committed);
    w.publish(&mut vfs, &probe).expect("publish");
    vfs.read_full(&format!("{DIR}/{}", part_file_name(0)))
        .expect("part bytes")
}

fn scan_of(parts: Vec<Arc<OpenPart>>, columns: Vec<ScanColumn>) -> TableScan {
    let binding = full_binding();
    TableScan {
        parts,
        binding,
        unwrappers: binding.unwrappers,
        columns,
        predicate: None,
        strview_cols: vec![],
        code_key_cols: vec![],
        deletes: Vec::new(),
        opts: ScanOptions::default(),
    }
}

/// The expected per-row TEXT extraction of `docs` at "name" through the
/// LANE law: value where the path holds a string; NULL for SQL-NULL rows,
/// jsonb null, absent path, and exceptions (non-string at the path).
fn expected_name(docs: &Docs) -> Vec<Option<String>> {
    docs.iter()
        .map(|d| {
            let d = d.as_ref()?;
            // The tests' docs put "name":"<tok>" literally; rows crafted
            // as jsonb-null/absent/exception return None here by
            // construction (the doc builders below).
            let key = "\"name\":\"";
            let at = d.find(key)?;
            let rest = &d[at + key.len()..];
            let end = rest.find('"')?;
            Some(rest[..end].to_string())
        })
        .collect()
}

/// Grain identity guard: canon maps are only comparable when both sides
/// tile rows identically (SB-10 elects per-part grains; tiny fixtures land
/// on the same grain — assert, never assume).
fn assert_same_geometry(a: &Arc<OpenPart>, b: &Arc<OpenPart>) {
    assert_eq!(a.rows(), b.rows(), "twin row counts");
    assert_eq!(a.grain(), b.grain(), "twin granule grain");
    assert_eq!(
        a.footer().granule_count,
        b.footer().granule_count,
        "twin granule count"
    );
}

// ---------------------------------------------------------------------------
// Differential twins (lane read vs the normal path, byte identity)
// ---------------------------------------------------------------------------

/// Text lane vs plain-text-column twin: the canon byte streams (the
/// dirsha currency) are IDENTICAL — the lane supplies exactly what the
/// normal path serves for the same values.
#[test]
fn text_lane_scan_is_byte_identical_to_the_plain_column_twin() {
    let docs = std_docs(400);
    let shredded = open_part_bytes(&seal_jsonb(&docs, 41, true, None), 9101).expect("open");
    let expect = expected_name(&docs);
    let imgs: Vec<Option<Vec<u8>>> = expect
        .iter()
        .map(|v| v.as_ref().map(|s| img_4b_u(s.as_bytes())))
        .collect();
    let rows: Vec<Option<RawDatum<'_>>> = imgs
        .iter()
        .map(|o| o.as_deref().map(RawDatum::Bytes))
        .collect();
    let plain = open_part_bytes(&seal_plain(text_col(1), &rows, 42), 9102).expect("open");
    assert_same_geometry(&shredded, &plain);

    let lane = scan_of(
        vec![Arc::clone(&shredded)],
        vec![ScanColumn::shred_lane(1, "name", ShredLaneKind::Text)],
    );
    let twin = scan_of(vec![Arc::clone(&plain)], vec![ScanColumn::root(text_col(1), true)]);
    let a = lane.run(1, None).expect("lane scan");
    let b = twin.run(1, None).expect("twin scan");
    assert_eq!(a.canon, b.canon, "lane vs normal-path byte identity");
    assert_eq!(a.census.rows_emitted, b.census.rows_emitted);
    assert!(a.census.rows_emitted == 400, "non-vacuous");
}

/// Bool lane vs plain-bool-column twin (the second lane kind the writer
/// auto-elects; ByvalWord canon currency).
#[test]
fn bool_lane_scan_is_byte_identical_to_the_plain_column_twin() {
    let docs = std_docs(400);
    let shredded = open_part_bytes(&seal_jsonb(&docs, 43, true, None), 9103).expect("open");
    let rows: Vec<Option<RawDatum<'_>>> = docs
        .iter()
        .enumerate()
        .map(|(i, d)| d.as_ref().map(|_| RawDatum::Word((i as u64 % 2 == 0) as u64)))
        .collect();
    let plain = open_part_bytes(&seal_plain(bool_col(1), &rows, 44), 9104).expect("open");
    assert_same_geometry(&shredded, &plain);

    let lane = scan_of(
        vec![Arc::clone(&shredded)],
        vec![ScanColumn::shred_lane(1, "flag", ShredLaneKind::Bool)],
    );
    let twin = scan_of(vec![Arc::clone(&plain)], vec![ScanColumn::root(bool_col(1), true)]);
    let a = lane.run(1, None).expect("lane scan");
    let b = twin.run(1, None).expect("twin scan");
    assert_eq!(a.canon, b.canon, "bool lane vs normal-path byte identity");
}

/// The NULL law through the SCAN face under the exception-freedom seal
/// law: an EMITTED lane's NULL geometry is exactly SQL-NULL ∪ absent-path
/// — byte identical to a twin whose root column is NULL at exactly those
/// rows. (This IS the `->>`-class extraction semantics: both categories
/// answer SQL NULL on the normal path too.)
#[test]
fn lane_null_geometry_matches_the_twin() {
    let mut docs: Docs = (0..200u64)
        .map(|i| {
            Some(format!(
                r#"{{"flag":true,"name":"tok{:04}","score":1.25}}"#,
                i % 41
            ))
        })
        .collect();
    docs[10] = None; // SQL NULL row
    docs[30] = Some(r#"{"flag":true,"score":1.25}"#.to_string()); // absent path
    let shredded = open_part_bytes(&seal_jsonb(&docs, 45, true, None), 9105).expect("open");

    let expect = expected_name(&docs);
    assert!(expect[10].is_none() && expect[30].is_none());
    let imgs: Vec<Option<Vec<u8>>> = expect
        .iter()
        .map(|v| v.as_ref().map(|s| img_4b_u(s.as_bytes())))
        .collect();
    let rows: Vec<Option<RawDatum<'_>>> = imgs
        .iter()
        .map(|o| o.as_deref().map(RawDatum::Bytes))
        .collect();
    let plain = open_part_bytes(&seal_plain(text_col(1), &rows, 46), 9106).expect("open");
    assert_same_geometry(&shredded, &plain);

    let lane = scan_of(
        vec![Arc::clone(&shredded)],
        vec![ScanColumn::shred_lane(1, "name", ShredLaneKind::Text)],
    );
    let twin = scan_of(vec![plain], vec![ScanColumn::root(text_col(1), true)]);
    let a = lane.run(1, None).expect("lane scan");
    let b = twin.run(1, None).expect("twin scan");
    assert_eq!(a.canon, b.canon, "null-geometry byte identity");
}

/// The exception-freedom seal law's teeth: a part where the path holds a
/// wrong-type occurrence (a scalar of another kind, a container) carries NO
/// lane for that path, and a declared lane REFUSES typed ("not elected").
/// Sibling exception-free paths keep their lanes (the forfeit is per-path,
/// never per-column). A jsonb null is no exception (P3a: its `->>` is SQL
/// NULL, an absent path's): `jsonb_null_at_the_path_keeps_the_lane`.
#[test]
fn exception_bearing_paths_forfeit_their_lanes() {
    for (fx, ino, bad_doc) in [
        (61u64, 9121u64, r#"{"flag":true,"name":123,"score":1.25}"#), // wrong type
        (62, 9122, r#"{"flag":true,"name":{"x":1},"score":1.25}"#),   // a container
    ] {
        let mut docs: Docs = (0..120u64)
            .map(|i| {
                Some(format!(
                    r#"{{"flag":true,"name":"tok{:04}","score":1.25}}"#,
                    i % 31
                ))
            })
            .collect();
        docs[50] = Some(bad_doc.to_string());
        let part = open_part_bytes(&seal_jsonb(&docs, fx, true, None), ino).expect("open");
        // The name lane refuses...
        let scan = scan_of(
            vec![Arc::clone(&part)],
            vec![ScanColumn::shred_lane(1, "name", ShredLaneKind::Text)],
        );
        match refusal_of(&scan) {
            ReadError::ShredLaneRefused { why, .. } => {
                assert!(why.contains("not elected"), "cause: {why}")
            }
            e => panic!("wrong refusal: {e}"),
        }
        // ...while the exception-free sibling lane still serves.
        let sibling = scan_of(
            vec![part],
            vec![ScanColumn::shred_lane(1, "flag", ShredLaneKind::Bool)],
        );
        let r = sibling.run(1, None).expect("sibling lane serves");
        assert_eq!(r.census.rows_emitted, 120);
    }
}

/// Dual-store additivity through the scan face: the PARENT image column of
/// a lanes-bearing part scans byte-identical to a NoShred seal of the very
/// same documents (lanes are additive substreams; the image lane stays the
/// read truth — O-4).
#[test]
fn parent_image_column_is_unchanged_by_lane_presence() {
    let docs = std_docs(400);
    let with_lanes = open_part_bytes(&seal_jsonb(&docs, 47, true, None), 9107).expect("open");
    let without = open_part_bytes(&seal_jsonb(&docs, 48, false, None), 9108).expect("open");
    assert_same_geometry(&with_lanes, &without);
    let a = scan_of(vec![with_lanes], vec![ScanColumn::root(text_col(1), true)])
        .run(1, None)
        .expect("scan");
    let b = scan_of(vec![without], vec![ScanColumn::root(text_col(1), true)])
        .run(1, None)
        .expect("scan");
    assert_eq!(a.canon, b.canon, "image-lane additivity byte identity");
}

// ---------------------------------------------------------------------------
// The dirsha law over per-part elections
// ---------------------------------------------------------------------------

/// Serial vs DOP-4-skew byte identity across parts whose elections assign
/// DIFFERENT path_ords to the same dotted path (part A: flag,name,score →
/// name=2; part B: a,b,flag,name → name=4): per-part resolution is
/// witnessed by construction, and the folded census holds at every DOP.
#[test]
fn serial_eq_dop4_lane_scan_across_parts_with_skewed_elections() {
    let docs_a = std_docs(400);
    let docs_b: Docs = (0..300u64)
        .map(|i| {
            if i % 17 == 3 {
                None
            } else {
                Some(format!(
                    r#"{{"a":{},"b":{},"flag":{},"name":"tok{:04}"}}"#,
                    i,
                    i * 2,
                    i % 3 == 0,
                    i % 89
                ))
            }
        })
        .collect();
    let pa = open_part_bytes(&seal_jsonb(&docs_a, 49, true, None), 9109).expect("open");
    let pb = open_part_bytes(&seal_jsonb(&docs_b, 50, true, None), 9110).expect("open");
    let scan = scan_of(
        vec![pa, pb],
        vec![ScanColumn::shred_lane(1, "name", ShredLaneKind::Text)],
    );
    let serial = scan.run(1, None).expect("serial");
    let par = scan.run(4, Some(1)).expect("dop4 skew");
    assert_eq!(serial.canon, par.canon, "serial vs DOP-4-skew byte identity");
    assert_eq!(serial.census, par.census, "folded census identity (PC-6.3)");
    assert_eq!(par.pins.pins_taken, par.pins.pins_released, "pin balance");
    assert_eq!(serial.census.rows_emitted, 700);
}

// ---------------------------------------------------------------------------
// The typed-refusal guard (born-RED teeth: each arm fails the scan typed;
// deleting any refusal arm in resolve_stream_ord turns its test RED)
// ---------------------------------------------------------------------------

fn refusal_of(scan: &TableScan) -> ReadError {
    match scan.run(1, None) {
        Err(e) => e,
        Ok(_) => panic!("scan must refuse typed"),
    }
}

/// A lanes-free part cannot serve any lane declaration: refusal, never a
/// fabricated all-NULL column.
#[test]
fn absent_shred_election_refuses_typed() {
    let docs = std_docs(200);
    let part = open_part_bytes(&seal_jsonb(&docs, 51, false, None), 9111).expect("open");
    let scan = scan_of(
        vec![part],
        vec![ScanColumn::shred_lane(1, "name", ShredLaneKind::Text)],
    );
    match refusal_of(&scan) {
        ReadError::ShredLaneRefused { attno: 1, why, .. } => {
            assert!(why.contains("no PathTable"), "cause names the gap: {why}")
        }
        e => panic!("wrong refusal: {e}"),
    }
}

/// A shredded part refuses paths its election never admitted.
#[test]
fn unelected_path_refuses_typed() {
    let docs = std_docs(200);
    let part = open_part_bytes(&seal_jsonb(&docs, 52, true, None), 9112).expect("open");
    let scan = scan_of(
        vec![part],
        vec![ScanColumn::shred_lane(1, "nope", ShredLaneKind::Text)],
    );
    match refusal_of(&scan) {
        ReadError::ShredLaneRefused { why, .. } => {
            assert!(why.contains("not elected"), "cause: {why}")
        }
        e => panic!("wrong refusal: {e}"),
    }
}

/// Declared kind vs sealed kind disagreement refuses (never reinterprets).
#[test]
fn lane_kind_mismatch_refuses_typed() {
    let docs = std_docs(200);
    let part = open_part_bytes(&seal_jsonb(&docs, 53, true, None), 9113).expect("open");
    let scan = scan_of(
        vec![part],
        vec![ScanColumn::shred_lane(1, "name", ShredLaneKind::Bool)],
    );
    match refusal_of(&scan) {
        ReadError::ShredLaneRefused { why, .. } => {
            assert!(why.contains("disagrees"), "cause: {why}")
        }
        e => panic!("wrong refusal: {e}"),
    }
}

/// The NumericFs scale law after the RULING (2026-08-14, aux32 slot):
/// a NON-ZERO-scale lane ("score" is uniformly N.25 → sealed scale 2,
/// STREAMF_LANE_SCALE set) still refuses TYPED — the rendering family
/// for decimal lanes is unrouted. Born-RED direction: were the non-zero
/// arm deleted, this scan would serve raw mantissas (1225 for 12.25) as
/// int words — silently wrong answers, which is exactly what the typed
/// refusal makes impossible.
#[test]
fn nonzero_scale_numeric_lane_refuses_typed() {
    let docs = std_docs(200);
    let part = open_part_bytes(&seal_jsonb(&docs, 54, true, None), 9114).expect("open");
    let scan = scan_of(
        vec![part],
        vec![ScanColumn::shred_lane(1, "score", ShredLaneKind::NumericFs)],
    );
    match refusal_of(&scan) {
        ReadError::ShredLaneRefused { why, .. } => {
            assert!(why.contains("non-zero-scale"), "cause: {why}")
        }
        e => panic!("wrong refusal: {e}"),
    }
}

/// The RULED slot's serve arm: a SCALE-0 numeric lane (integer values at
/// the path — the µs-epoch class) serves as signed int words, byte
/// identical to a plain int8-column twin of the same values (the normal
/// path on the same data).
#[test]
fn scale_zero_numeric_lane_serves_as_int_words() {
    let docs: Docs = (0..300u64)
        .map(|i| {
            if i % 11 == 7 {
                None
            } else {
                Some(format!(
                    r#"{{"flag":{},"name":"tok{:03}","num":{}}}"#,
                    i % 2 == 0,
                    i % 37,
                    (i as i64 * 13) % 5000 - 300
                ))
            }
        })
        .collect();
    let shredded = open_part_bytes(&seal_jsonb(&docs, 57, true, None), 9117).expect("open");
    let rows: Vec<Option<RawDatum<'_>>> = docs
        .iter()
        .enumerate()
        .map(|(i, d)| {
            d.as_ref()
                .map(|_| RawDatum::Word(((i as i64 * 13) % 5000 - 300) as u64))
        })
        .collect();
    let plain =
        open_part_bytes(&seal_plain(pgrc_write::testkit::int8_col(1), &rows, 58), 9118)
            .expect("open");
    assert_same_geometry(&shredded, &plain);
    let lane = scan_of(
        vec![Arc::clone(&shredded)],
        vec![ScanColumn::shred_lane(1, "num", ShredLaneKind::NumericFs)],
    );
    let twin = scan_of(
        vec![plain],
        vec![ScanColumn::root(pgrc_write::testkit::int8_col(1), true)],
    );
    let a = lane.run(1, None).expect("int lane scan");
    let b = twin.run(1, None).expect("twin scan");
    assert_eq!(a.canon, b.canon, "scale-0 lane vs plain int twin byte identity");
    let par = lane.run(4, Some(1)).expect("dop4");
    assert_eq!(a.canon, par.canon, "serial vs DOP-4-skew identity");
}

/// A declared class outside the lane vocabulary refuses at declaration
/// grain (nothing in a part can serve it).
#[test]
fn non_lane_declared_class_refuses_typed() {
    use pgrc_format::class::{ColSchema, CollationClass, StorageClass, TypeSemantics};
    let docs = std_docs(200);
    let part = open_part_bytes(&seal_jsonb(&docs, 55, true, None), 9115).expect("open");
    let scan = scan_of(
        vec![part],
        vec![ScanColumn {
            schema: ColSchema {
                attno: 1,
                class: StorageClass::ByvalWord {
                    width: 4,
                    signed: true,
                },
                typlen: 4,
                typbyval: true,
                typalign: b'i',
                collation_class: CollationClass::C,
                semantics: TypeSemantics::SignedInt,
            },
            dict_value_order: true,
            shred_path: Some("name".to_string()),
        }],
    );
    match refusal_of(&scan) {
        ReadError::ShredLaneRefused { why, .. } => {
            assert!(why.contains("not a shred lane class"), "cause: {why}")
        }
        e => panic!("wrong refusal: {e}"),
    }
}

/// The engagement probe (`verify_columns`): Ok on a lane every part
/// serves; a typed refusal when ANY part cannot (part B lacks the path)
/// — the pre-engagement decline the executor seam consumes; and a cheap
/// no-op for root-only scans.
#[test]
fn verify_columns_probes_every_part() {
    let docs_a = std_docs(200);
    let docs_b: Docs = (0..150u64)
        .map(|i| Some(format!(r#"{{"flag":{},"other":"x{}"}}"#, i % 2 == 0, i % 7)))
        .collect();
    let pa = open_part_bytes(&seal_jsonb(&docs_a, 63, true, None), 9123).expect("open");
    let pb = open_part_bytes(&seal_jsonb(&docs_b, 64, true, None), 9124).expect("open");
    // Both parts serve "flag": Ok.
    let served = scan_of(
        vec![Arc::clone(&pa), Arc::clone(&pb)],
        vec![ScanColumn::shred_lane(1, "flag", ShredLaneKind::Bool)],
    );
    served.verify_columns().expect("both parts serve flag");
    // Only part A serves "name": typed refusal from the probe.
    let unserved = scan_of(
        vec![Arc::clone(&pa), pb],
        vec![ScanColumn::shred_lane(1, "name", ShredLaneKind::Text)],
    );
    match unserved.verify_columns() {
        Err(ReadError::ShredLaneRefused { why, .. }) => {
            assert!(why.contains("not elected"), "cause: {why}")
        }
        r => panic!("probe must refuse typed, got {r:?}"),
    }
    // Root-only scans: trivially Ok.
    scan_of(vec![pa], vec![ScanColumn::root(text_col(1), true)])
        .verify_columns()
        .expect("root-only no-op");
}

// ---------------------------------------------------------------------------
// The dict plane over a lane (the dict-code group-key substrate: a
// low-NDV text lane dict-elects like any text stream and publishes codes)
// ---------------------------------------------------------------------------

/// A dict-elected text lane publishes dict-code lanes through the one
/// gate (dict-coded values ∧ exec flag ∧ zero-null proof) at its resolved
/// path_ord, with serial/DOP-4 byte identity intact. This is the probed
/// substrate the dict-code group-key routing lands on.
#[test]
fn dict_elected_text_lane_publishes_code_lanes() {
    // All rows present, low NDV, no nulls anywhere (the zero-null proof
    // needs the lane validity stream ABSENT).
    let docs: Docs = (0..4000u64)
        .map(|i| {
            Some(format!(
                r#"{{"flag":{},"name":"cat{:02}"}}"#,
                i % 2 == 0,
                i % 8
            ))
        })
        .collect();
    // Dict posture for the LANE stream: (attno 1, path_ord 2) — flag=1,
    // name=2 in canonical election order.
    let dict_posture = ColumnPosture {
        cold: true,
        dict: Some(DictPolicy {
            ndv_cap: 1_000_000,
            exec_ok: true,
            sem: TextSemantics::BytesOnly,
        }),
        ..Default::default()
    };
    let cands = CodecCandidates::new(ColumnPosture {
        cold: true,
        ..Default::default()
    })
    .with_column(1, 2, dict_posture);
    let part = open_part_bytes(&seal_jsonb(&docs, 56, true, Some(&cands)), 9116).expect("open");
    let scan = scan_of(
        vec![part],
        vec![ScanColumn::shred_lane(1, "name", ShredLaneKind::Text)],
    );
    let serial = scan.run(1, None).expect("serial");
    assert!(
        serial.census.dict_lanes_published > 0,
        "the lane dict-elected and published codes: {:?}",
        serial.census
    );
    let par = scan.run(4, Some(1)).expect("dop4");
    assert_eq!(serial.canon, par.canon, "dict-lane byte identity");
    assert_eq!(serial.census, par.census, "census identity");
    // The per-part dict face resolves the lane ordinal the same way.
    let space = scan.dict_space_of(0, 0).expect("dict face");
    assert!(space.is_some(), "dict_space_of serves the lane column");
}

// ---------------------------------------------------------------------------
// P-a (M6 Wave 0; the port of 4736aa2a64f): nullable
// dictionary lanes executable as codes (`STREAMF_DICT_EXEC_NULLABLE`)
// ---------------------------------------------------------------------------

use std::collections::BTreeMap;

use pgrc_batch::{Batch, ColRep, GuardWord, ESCAPE_CODE_U32};
use pgrc_claim::{ClaimCursor, ClaimOutcome, NoObserver};
use pgrc_format::enc::EncodingId;
use pgrc_format::part::{StreamRole, STREAMF_DICT_EXEC, STREAMF_DICT_EXEC_NULLABLE};
use pgrc_read::ReadResult;

use crate::scan::{ScanWorker, SharedCounters};
use crate::spans::GranuleSpans;

/// The production lane posture (`pgrc_am` ingest `codec_candidates`, its [json-rung1] arm): every shred lane of jsonb
/// column 1 carries the dict posture with `exec_ok` (a Text lane is C-collated by construction), `BytesOnly`.
fn lane_dict_cands(ndv_cap: u64, exec_ok: bool) -> CodecCandidates {
    CodecCandidates::new(ColumnPosture { cold: true, ..Default::default() }).with_lane_default(
        1,
        ColumnPosture {
            cold: true,
            dict: Some(DictPolicy { ndv_cap, exec_ok, sem: TextSemantics::BytesOnly }),
            ..Default::default()
        },
    )
}

/// `name` is SQL NULL (the root), absent, or one of 8 strings. `j->>'name'` (and `j->'name'`) is SQL NULL for the first
/// two in PG 18.6; a jsonb `null` at the path is the case the lane cannot hold (see
/// `jsonb_null_at_the_path_forfeits_the_dict_lane`).
fn nullable_name_docs(rows: u64) -> Docs {
    (0..rows)
        .map(|i| {
            if i % 11 == 3 {
                None
            } else if i % 17 == 5 {
                Some(r#"{"flag":true}"#.to_string())
            } else {
                Some(format!(r#"{{"flag":{},"name":"cat{:02}"}}"#, i % 2 == 0, i % 8))
            }
        })
        .collect()
}

/// Lane `path` of column 1 in `part`: (path_ord, Values encoding, Values flags, Validity stream present).
fn lane_stream(part: &Arc<OpenPart>, path: &str) -> (u32, u16, u16, bool) {
    let paths = pgrc_read::read_path_table(part).expect("path table").expect("a shredded part");
    let ord = paths.iter().position(|p| p == path).expect("the path is elected") as u32 + 1;
    let dir = part.stream_directory().expect("dir");
    let v = dir.lookup(1, ord, StreamRole::Values).expect("values");
    (ord, v.entry.encoding, v.entry.flags, dir.lookup(1, ord, StreamRole::Validity).is_some())
}

/// The plain text twin of lane `name`: a root text column holding exactly the values `->>` gives (NULL where it gives NULL).
fn name_twin(docs: &Docs, fxid: u64, ino: u64) -> Arc<OpenPart> {
    let expect = expected_name(docs);
    let imgs: Vec<Option<Vec<u8>>> = expect.iter().map(|v| v.as_ref().map(|s| img_4b_u(s.as_bytes()))).collect();
    let rows: Vec<Option<RawDatum<'_>>> = imgs.iter().map(|o| o.as_deref().map(RawDatum::Bytes)).collect();
    open_part_bytes(&seal_plain(text_col(1), &rows, fxid), ino).expect("open")
}

/// The canon with every window's representation marker (`d` codes, `s` string views, `b` datums: the ABI path, which
/// differs between arms by design) normalized to `b`, so only the VALUE / NULL bytes are compared. These fixtures'
/// value bytes hold no `W`, so a marker is exactly the byte after a `W`.
fn unmark(m: &BTreeMap<u64, Vec<u8>>) -> BTreeMap<u64, Vec<u8>> {
    m.iter()
        .map(|(k, v)| {
            let mut v = v.clone();
            for i in 1..v.len() {
                if v[i - 1] == b'W' && matches!(v[i], b'd' | b's') {
                    v[i] = b'b';
                }
            }
            (*k, v)
        })
        .collect()
}

/// Drive `scan` serially and check column 0 of every window published as a code lane: a NULL row carries the reserved
/// escape sentinel and an invalid validity bit, a valid row a code inside the dictionary (a code never stands for NULL,
/// and the sentinel is never a value). Returns (code-lane windows, NULL rows, valid rows).
fn code_lane_null_law(scan: &TableScan) -> (u64, u64, u64) {
    let counts: Vec<u32> = scan.parts.iter().map(|p| p.footer().granule_count).collect();
    let spans = GranuleSpans::new(&counts);
    let cursor = ClaimCursor::new();
    let shared = SharedCounters::new();
    let mut worker = ScanWorker::new(scan);
    let (mut windows, mut nulls, mut valids) = (0u64, 0u64, 0u64);
    loop {
        let mut guard = match cursor.begin_claim(&spans, 0, &NoObserver) {
            Ok(g) => g,
            Err(ClaimOutcome::Drained) => break,
            Err(_) => continue,
        };
        let mut visit = |batch: &Batch, _gw: GuardWord, _unit: u64, _win: u32| -> ReadResult<()> {
            let col = &batch.cols[0];
            if let ColRep::DictCodes(lane) = col.rep {
                windows += 1;
                let n = batch.nrows as usize;
                // SAFETY: claim-scoped consume of the published lane inside the visit callback (R1).
                let codes = unsafe { lane.codes(n) };
                let ncodes = unsafe { lane.dict().space() }.ncodes();
                for (r, &c) in codes.iter().enumerate() {
                    if col.validity.is_valid(r) {
                        assert!(c < ncodes, "a valid row carries a dictionary code ({c} of {ncodes})");
                        valids += 1;
                    } else {
                        assert_eq!(c, ESCAPE_CODE_U32, "a NULL row carries the escape sentinel, never a code");
                        nulls += 1;
                    }
                }
            }
            Ok(())
        };
        worker.drive_claim(&mut guard, &spans, &shared, &mut visit).expect("drive_claim");
    }
    let _ = worker.finish(&shared);
    (windows, nulls, valids)
}

/// P-a's tooth (fails before: the stream sealed no exec flag and never published): a NULL-bearing Text lane (SQL-NULL
/// roots and absent paths) under the production lane posture seals DictCodes + `STREAMF_DICT_EXEC_NULLABLE` (never the
/// zero-null flag) + its Validity plane, and the scan serves it as codes: values and NULLs byte-identical to the hydrated
/// arm and to the plain twin of `j->>'name'`, serial == DOP-4, every NULL row the escape sentinel. The lane is the
/// `->>` text (a Text lane's values are the string's bytes, never a jsonb): a consumer of `->` reads the image.
#[test]
fn nullable_text_lane_seals_nullable_flag_and_serves_codes() {
    let docs = nullable_name_docs(3000);
    let expect = expected_name(&docs);
    let want_nulls = expect.iter().filter(|v| v.is_none()).count() as u64;
    assert!(docs.iter().any(|d| d.is_none()) && want_nulls > docs.iter().filter(|d| d.is_none()).count() as u64);
    let part = open_part_bytes(&seal_jsonb(&docs, 71, true, Some(&lane_dict_cands(1_000_000, true))), 9141).expect("open");
    let (_, enc, flags, validity) = lane_stream(&part, "name");
    assert_eq!(enc, EncodingId::DictCodes.as_u16(), "the NULL-bearing text lane dict-elects");
    assert_eq!(flags & STREAMF_DICT_EXEC, 0, "the zero-null flag stays null-free only");
    assert_ne!(flags & STREAMF_DICT_EXEC_NULLABLE, 0, "the nullable twin is sealed");
    assert!(validity, "its Validity plane pairs with the flag");

    let cols = || vec![ScanColumn::shred_lane(1, "name", ShredLaneKind::Text)];
    let lane = scan_of(vec![Arc::clone(&part)], cols());
    let a = lane.run(1, None).expect("serial");
    assert!(a.census.dict_lanes_published > 0, "the nullable lane publishes codes: {:?}", a.census);
    assert_eq!(a.census.rows_emitted, 3000);
    let par = lane.run(4, Some(1)).expect("dop4");
    assert_eq!(a.canon, par.canon, "serial vs DOP-4-skew byte identity");
    let mut off = scan_of(vec![Arc::clone(&part)], cols());
    off.opts.publish_dict_lanes = false;
    let h = off.run(1, None).expect("hydrated arm");
    assert_eq!(h.census.dict_lanes_published, 0);
    assert_eq!(unmark(&a.canon), unmark(&h.canon), "published vs hydrated value / NULL identity");
    let twin_part = name_twin(&docs, 72, 9142);
    assert_same_geometry(&part, &twin_part);
    let t = scan_of(vec![twin_part], vec![ScanColumn::root(text_col(1), true)]).run(1, None).expect("twin");
    assert_eq!(unmark(&a.canon), unmark(&t.canon), "codes vs the plain twin of j->>'name'");

    let mut codes_only = scan_of(vec![part], cols());
    codes_only.opts.build_strviews = false;
    let (windows, nulls, valids) = code_lane_null_law(&codes_only);
    assert!(windows > 0, "non-vacuous: the lane crossed as codes");
    assert_eq!(nulls, want_nulls, "every NULL row (SQL-NULL root, absent path) is the sentinel + invalid");
    assert_eq!(nulls + valids, 3000);
}

/// The NULL trichotomy under the dict posture (P3a, lanes/shreddesign DESIGN.md R1): `->>` is SQL NULL for a jsonb `null`
/// at the path and for an absent one (PG 18.6), so a part holding jsonb `null`s at the path keeps the lane, those rows
/// lane NULLs: the lane is the `->>` text, never `->`'s jsonb (`->` and `?` read the image). Its values and NULLs are
/// byte-identical to the plain twin of `j->>'name'`; the sibling keeps its lane.
#[test]
fn jsonb_null_at_the_path_keeps_the_lane() {
    let mut docs = nullable_name_docs(600);
    docs[7] = Some(r#"{"flag":true,"name":null}"#.to_string());
    docs[8] = Some(r#"{"flag":false,"name":null}"#.to_string());
    let part = open_part_bytes(&seal_jsonb(&docs, 73, true, Some(&lane_dict_cands(1_000_000, true))), 9143).expect("open");
    let paths = pgrc_read::read_path_table(&part).expect("path table").expect("a shredded part");
    assert!(paths.iter().any(|p| p == "name"), "a jsonb null at the path keeps its lane: {paths:?}");
    assert!(paths.iter().any(|p| p == "flag"), "the sibling keeps its lane: {paths:?}");
    let a = scan_of(vec![Arc::clone(&part)], vec![ScanColumn::shred_lane(1, "name", ShredLaneKind::Text)]).run(1, None).expect("lane");
    assert_eq!(a.census.rows_emitted, 600);
    let twin_part = name_twin(&docs, 74, 9144);
    assert_same_geometry(&part, &twin_part);
    let t = scan_of(vec![twin_part], vec![ScanColumn::root(text_col(1), true)]).run(1, None).expect("twin");
    assert_eq!(unmark(&a.canon), unmark(&t.canon), "the lane vs the plain twin of j->>'name' (jsonb nulls NULL)");
    let sibling = scan_of(vec![part], vec![ScanColumn::shred_lane(1, "flag", ShredLaneKind::Bool)]);
    assert_eq!(sibling.run(1, None).expect("sibling serves").census.rows_emitted, 600);
}

/// The bench `gc` shape (bench.sql: `j` = {"k": one of 7 strings, "n": i}, `k` never NULL): the production lane posture
/// seals the `k` lane as DictCodes under the zero-null `STREAMF_DICT_EXEC` (no Validity stream, no nullable flag),
/// byte for byte as before P-a: the lane C4 / C5 / C12 read is exec-ready without the nullable twin.
#[test]
fn gc_shape_text_lane_keeps_the_zero_null_dict_exec_form() {
    const K: [&str; 7] = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta"];
    let docs: Docs = (1..=4000u64).map(|i| Some(format!(r#"{{"k":"{}","n":{}}}"#, K[(i % 7) as usize], i))).collect();
    let part = open_part_bytes(&seal_jsonb(&docs, 74, true, Some(&lane_dict_cands(1_000_000, true))), 9144).expect("open");
    let (_, enc, flags, validity) = lane_stream(&part, "k");
    assert_eq!(enc, EncodingId::DictCodes.as_u16(), "k dict-elects");
    assert_ne!(flags & STREAMF_DICT_EXEC, 0, "the zero-null flag");
    assert_eq!(flags & STREAMF_DICT_EXEC_NULLABLE, 0, "never the nullable twin on a null-free lane");
    assert!(!validity, "no Validity stream: the zero-null proof");
    let r = scan_of(vec![part], vec![ScanColumn::shred_lane(1, "k", ShredLaneKind::Text)]).run(1, None).expect("scan");
    assert!(r.census.dict_lanes_published > 0, "the k lane serves as codes: {:?}", r.census);
}

/// Old parts read exactly as before: a NULL-bearing dict lane sealed with neither exec flag (what every seal before P-a
/// wrote for a stream with a NULL row; here `exec_ok: false` writes the same flags) never publishes codes, and its values
/// and NULLs read unchanged, equal to what a new part serves as codes.
#[test]
fn pre_flag_nullable_dict_lane_reads_unchanged() {
    let docs = nullable_name_docs(3000);
    let old = open_part_bytes(&seal_jsonb(&docs, 75, true, Some(&lane_dict_cands(1_000_000, false))), 9145).expect("open");
    let (_, enc, flags, validity) = lane_stream(&old, "name");
    assert_eq!(enc, EncodingId::DictCodes.as_u16());
    assert_eq!(flags & (STREAMF_DICT_EXEC | STREAMF_DICT_EXEC_NULLABLE), 0, "the pre-flag form: no exec flag");
    assert!(validity);
    let cols = || vec![ScanColumn::shred_lane(1, "name", ShredLaneKind::Text)];
    let a = scan_of(vec![old], cols()).run(1, None).expect("old part");
    assert_eq!(a.census.dict_lanes_published, 0, "a pre-flag NULL-bearing stream never executes as codes");
    let new = open_part_bytes(&seal_jsonb(&docs, 76, true, Some(&lane_dict_cands(1_000_000, true))), 9146).expect("open");
    let b = scan_of(vec![new], cols()).run(1, None).expect("new part");
    assert!(b.census.dict_lanes_published > 0);
    assert_eq!(unmark(&a.canon), unmark(&b.canon), "old part values / NULLs == the new part's codes");
}

/// A lane whose values do not fit the dictionary budget (NDV over `ndv_cap`) falls back as today: no DictCodes, no
/// exec flag, the values read through the normal path, equal to the twin.
#[test]
fn over_budget_text_lane_stays_off_the_dictionary() {
    let docs = nullable_name_docs(3000);
    let part = open_part_bytes(&seal_jsonb(&docs, 77, true, Some(&lane_dict_cands(4, true))), 9147).expect("open");
    let (_, enc, flags, _) = lane_stream(&part, "name");
    assert_ne!(enc, EncodingId::DictCodes.as_u16(), "8 distinct values over a cap of 4: no dictionary");
    assert_eq!(flags & (STREAMF_DICT_EXEC | STREAMF_DICT_EXEC_NULLABLE), 0);
    let twin_part = name_twin(&docs, 78, 9148);
    let a = scan_of(vec![part], vec![ScanColumn::shred_lane(1, "name", ShredLaneKind::Text)]).run(1, None).expect("lane");
    assert_eq!(a.census.dict_lanes_published, 0);
    let t = scan_of(vec![twin_part], vec![ScanColumn::root(text_col(1), true)]).run(1, None).expect("twin");
    assert_eq!(unmark(&a.canon), unmark(&t.canon), "over-budget lane == twin");
}
