//! Full-registry election wiring (A-lane amendment M3-A2): the goldens for
//! elected outcomes on representative columns, and the cross-crate proofs
//! that a full-election part READS BACK through `pgrc_read` + the codec
//! registry — including the dict width-byte resolution seam (M3-G gap 1):
//! the value stream's §6.3 width byte carries the max code width, the raw
//! entry fields can NOT resolve a kernel (tooth), and the normalized
//! `stream_kernel_key` path decodes byte-exact (the fix, proven end-to-end).

use super::*;
use crate::elect::{CodecCandidates, ColumnPosture, DictPolicy};
use crate::dict::TextSemantics;
use crate::publish::TxnVerdict;
use crate::seal::CodecResolver;
use pgrc_format::abi::{datum_canonical_bytes, ByteArena, DecodeOut, KernelKey};
use pgrc_format::enc::{EncodingId, Wrapper};
use pgrc_format::part::{StreamSectionHdr, STREAMF_DICT_EXEC};
use pgrc_format::{FormatError, FormatResult};
use pgrc_read::io::MemPartIo;
use pgrc_read::{CodecBinding, OpenPart, PartExpect, SectionUnwrapper, StreamCursor};
use std::sync::Arc;

fn splitmix(seed: &mut u64) -> u64 {
    *seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *seed;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The LZ4 section unwrapper adapter (the lx_source `pgrc_codec_binding`
/// shape, rebuilt here because dep direction keeps it out of pgrc_codec).
struct Lz4Unwrap;

impl SectionUnwrapper for Lz4Unwrap {
    fn wrapper(&self) -> Wrapper {
        Wrapper::Lz4
    }
    fn unwrap_section(&self, _hdr: &StreamSectionHdr, section: &[u8]) -> FormatResult<Vec<u8>> {
        let mut out = Vec::new();
        pgrc_codec::wrapper::unwrap_section(section, &mut out)?;
        Ok(out)
    }
}

static LZ4_UNWRAP: Lz4Unwrap = Lz4Unwrap;

/// The Zstd twin (the CMP-A slot-fill; same codec entry point).
struct ZstdUnwrap(Wrapper);

impl SectionUnwrapper for ZstdUnwrap {
    fn wrapper(&self) -> Wrapper {
        self.0
    }
    fn unwrap_section(&self, _hdr: &StreamSectionHdr, section: &[u8]) -> FormatResult<Vec<u8>> {
        let mut out = Vec::new();
        pgrc_codec::wrapper::unwrap_section(section, &mut out)?;
        Ok(out)
    }
}

static ZSTD_UNWRAP: ZstdUnwrap = ZstdUnwrap(Wrapper::Zstd);
static ONPAIR_VALUES_UNWRAP: ZstdUnwrap = ZstdUnwrap(Wrapper::OnPairZstd);

struct FrontZstdUnwrap(Wrapper);
impl SectionUnwrapper for FrontZstdUnwrap {
    fn wrapper(&self) -> Wrapper {
        self.0
    }
    fn unwrap_section(&self, _hdr: &StreamSectionHdr, section: &[u8]) -> FormatResult<Vec<u8>> {
        let mut out = Vec::new();
        pgrc_codec::wrapper::unwrap_section(section, &mut out)?;
        Ok(out)
    }
    fn unwrap_dict_payload_section(&self, _hdr: &StreamSectionHdr, section: &[u8], index: &[u8]) -> FormatResult<Vec<u8>> {
        let mut out = Vec::new();
        pgrc_codec::wrapper::unwrap_dict_payload_section(section, index, &mut out)?;
        Ok(out)
    }
    fn decode_side(&self, side: &[u8]) -> FormatResult<Option<pgrc_read::SideTable>> {
        if !self.0.side_block() {
            return Ok(None);
        }
        let t = pgrc_codec::onpairwrap::decode_side(side)?;
        let bytes = t.bytes();
        Ok(Some(pgrc_read::SideTable { table: Box::new(t), bytes }))
    }
    fn unwrap_dict_payload_block_side(&self, side: &pgrc_read::SideTable, src: &[u8], dst: &mut [u8], records: &[u8]) -> FormatResult<bool> {
        let Some(t) = side.table.downcast_ref::<pgrc_codec::onpair::DecodeTable>() else { return Ok(false) };
        pgrc_codec::onpairwrap::unwrap_frame_block(t, src, dst, records)?;
        Ok(true)
    }
    fn unwrap_dict_payload_block(&self, src: &[u8], dst: &mut [u8], records: &[u8]) -> FormatResult<bool> {
        if self.0 != Wrapper::FrontZstd {
            return Ok(false);
        }
        pgrc_codec::wrapper::unwrap_dict_payload_block(Wrapper::FrontZstd, src, dst, records)?;
        Ok(true)
    }
}
struct PackedZstdUnwrap;
impl SectionUnwrapper for PackedZstdUnwrap {
    fn wrapper(&self) -> Wrapper {
        Wrapper::PackedZstd
    }
    fn unwrap_section(&self, _hdr: &StreamSectionHdr, section: &[u8]) -> FormatResult<Vec<u8>> {
        let mut out = Vec::new();
        pgrc_codec::wrapper::unwrap_section(section, &mut out)?;
        Ok(out)
    }
    fn unwrap_block(&self, src: &[u8], dst: &mut [u8]) -> FormatResult<bool> {
        pgrc_codec::wrapper::unwrap_block_into(Wrapper::PackedZstd, src, dst)?;
        Ok(true)
    }
}
static FRONT_UNWRAP: FrontZstdUnwrap = FrontZstdUnwrap(Wrapper::FrontZstd);
static FRONT_ONPAIR_UNWRAP: FrontZstdUnwrap = FrontZstdUnwrap(Wrapper::FrontOnPairZstd);
static PACKED_UNWRAP: PackedZstdUnwrap = PackedZstdUnwrap;

/// The integer pre-pass unwrapper (id 8).
struct IntPreUnwrap;
impl SectionUnwrapper for IntPreUnwrap {
    fn wrapper(&self) -> Wrapper {
        Wrapper::IntPre
    }
    fn unwrap_section(&self, _hdr: &StreamSectionHdr, section: &[u8]) -> FormatResult<Vec<u8>> {
        let mut out = Vec::new();
        pgrc_codec::wrapper::unwrap_section(section, &mut out)?;
        Ok(out)
    }
}
static INTPRE_UNWRAP: IntPreUnwrap = IntPreUnwrap;

fn codec_binding() -> CodecBinding<'static> {
    static UNWRAPPERS: [&dyn SectionUnwrapper; 7] = [
        &LZ4_UNWRAP,
        &ZSTD_UNWRAP,
        &FRONT_UNWRAP,
        &PACKED_UNWRAP,
        &FRONT_ONPAIR_UNWRAP,
        &ONPAIR_VALUES_UNWRAP,
        &INTPRE_UNWRAP,
    ];
    CodecBinding {
        registry: pgrc_codec::registry(),
        unwrappers: &UNWRAPPERS,
    }
}

/// Open a published part's bytes through the REAL reader.
fn open_part(vfs: &mut MemVfs, name: &str) -> Arc<OpenPart> {
    let bytes = vfs.read_full(&format!("{DIR}/{name}")).expect("part bytes");
    let io = MemPartIo::new(bytes, 4242, 7);
    Arc::new(OpenPart::open(Box::new(io), &PartExpect::none()).expect("open part"))
}

/// Decode granule `g` fully; return (datums, arena) with the arena kept
/// alive so pointer datums stay valid.
fn decode_granule(cur: &mut StreamCursor<'_>, g: u32) -> (Vec<u64>, Vec<u8>) {
    let n = cur.values_in_granule(g).expect("values") as usize;
    let mut datums = vec![0u64; n];
    let mut arena_buf = vec![0u8; 4 << 20];
    let wrote = {
        let mut out = DecodeOut {
            datums: &mut datums,
            arena: ByteArena::new(&mut arena_buf),
        };
        cur.decode_full(g, &mut out).expect("decode_full")
    };
    assert_eq!(wrote as usize, n);
    (datums, arena_buf)
}

fn assert_varlena_datum(datum: u64, want_payload: &[u8]) {
    let mut scratch = [0u8; 8];
    // SAFETY: decode arena is live in the caller.
    let got = unsafe {
        datum_canonical_bytes(StorageClass::VarlenaVerbatim, datum, &mut scratch)
    }
    .expect("canonical bytes");
    assert_eq!(got, want_payload, "varlena payload mismatch");
}

struct FullKit {
    cands: CodecCandidates,
    resolver: CodecResolver,
    shred: crate::shred::NoShred,
    opts: ShredOptions,
    ext: crate::ingest::NoExternalDetoast,
}

impl FullKit {
    fn new(cands: CodecCandidates) -> FullKit {
        FullKit {
            cands,
            resolver: CodecResolver,
            shred: crate::shred::NoShred,
            opts: ShredOptions::default(),
            ext: crate::ingest::NoExternalDetoast,
        }
    }
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

/// Decimal string with exactly `scale` fraction digits (the codec suite's
/// shape; `numeric_in` yields dscale == scale).
fn decimal_string(mant: i64, scale: i32) -> String {
    let neg = mant < 0;
    let mut a = mant.unsigned_abs().to_string();
    let s = scale as usize;
    if a.len() <= s {
        a = format!("{}{a}", "0".repeat(s + 1 - a.len()));
    }
    let dot = a.len() - s;
    let body = if s == 0 {
        a
    } else {
        format!("{}.{}", &a[..dot], &a[dot..])
    };
    if neg {
        format!("-{body}")
    } else {
        body
    }
}

fn publish(w: &mut TableWriter, vfs: &mut MemVfs, kit: &mut FullKit, fxid: u64) {
    let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
    let mut env = SealEnv {
        vfs,
        sources: &sources,
        resolver: &kit.resolver,
        shred: &mut kit.shred,
        shred_opts: &kit.opts,
    };
    w.finish(&mut env).expect("finish");
    let probe = Probe::new(TxnVerdict::InProgress).set(fxid, TxnVerdict::Committed);
    w.publish(vfs, &probe).expect("publish");
}

// ---------------------------------------------------------------------------
// election goldens on representative columns
// ---------------------------------------------------------------------------

#[test]
fn full_registry_election_goldens() {
    let mut vfs = mem_with_dir();
    let cands = CodecCandidates::new(ColumnPosture::default())
        .with_column(
            4,
            0,
            ColumnPosture {
                dict: Some(DictPolicy {
                    ndv_cap: 10_000,
                    exec_ok: true,
                    sem: TextSemantics::BytesOnly,
                }),
                ..ColumnPosture::default()
            },
        )
        .with_column(
            5,
            0,
            ColumnPosture {
                numeric: true,
                ..ColumnPosture::default()
            },
        );
    let mut kit = FullKit::new(cands);
    let schema = vec![
        int8_col(1),
        f64_col(2),
        bool_col(3),
        text_col(4),
        // numeric images ride the varlena class, typmod-scaled (no octet-length lane: not a text or bytea column)
        ColSchema { semantics: TypeSemantics::PackedNumeric { scale: 2 }, ..text_col(5) },
        text_col(6), // incompressible bytea-like payloads
    ];
    let mut w = open_writer(schema, stamp(91, 1));
    let mut seed = 0xE1EC_7104_u64;
    let rows = 10_000u64;
    for i in 0..rows {
        let text_img = img_4b_u(format!("k{:03}", i % 97).as_bytes());
        let num_s = decimal_string((i % 5000) as i64 - 2500, 2);
        let num_img = adt_numeric::io::numeric_in(&num_s, -1, None)
            .expect("parse")
            .expect("non-soft")
            .as_bytes()
            .to_vec();
        let mut rand_payload = [0u8; 32];
        for chunk in rand_payload.chunks_mut(8) {
            chunk.copy_from_slice(&splitmix(&mut seed).to_le_bytes());
        }
        let rand_img = img_4b_u(&rand_payload);
        let images = [text_img, num_img, rand_img];
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv {
            vfs: &mut vfs,
            sources: &sources,
            resolver: &kit.resolver,
            shred: &mut kit.shred,
            shred_opts: &kit.opts,
        };
        let row = [
            RawDatum::Word(i),                                     // 1: seq int8
            RawDatum::Word(((i % 1000) as f64 / 100.0).to_bits()), // 2: decimal f64
            RawDatum::Word((i % 3 == 0) as u64),                   // 3: bool
            RawDatum::Bytes(&images[0]),                           // 4: dict text
            RawDatum::Bytes(&images[1]),                           // 5: numeric
            RawDatum::Bytes(&images[2]),                           // 6: incompressible
        ];
        w.append_row(&row, &mut kit.ext, &mut env).expect("append");
    }
    publish(&mut w, &mut vfs, &mut kit, 91);

    // The elected outcomes, GOLDEN (attno order).
    let r = &w.seal_reports()[0];
    let encodings: Vec<(u32, u16)> = r
        .elections
        .iter()
        .map(|e| (e.attno, e.encoding))
        .collect();
    assert_eq!(
        encodings,
        vec![
            (1, EncodingId::ByteFor.as_u16()),
            (2, EncodingId::Alp.as_u16()),
            (3, EncodingId::BoolBitmap.as_u16()),
            (4, EncodingId::DictCodes.as_u16()),
            (5, EncodingId::PackedNumeric.as_u16()),
            (6, EncodingId::Verbatim.as_u16()),
            // column 6's octet-length lane (D3038): every payload 32 bytes
            (6, EncodingId::Const.as_u16()),
        ],
        "elected encodings diverged from the golden"
    );
    // Every winner cleared the ≥10% gate; the demotion kept the baseline.
    for e in &r.elections {
        if e.encoding == EncodingId::Verbatim.as_u16() {
            assert_eq!(e.chosen_len, e.baseline_len);
        } else {
            assert!(e.chosen_len * 10 <= e.baseline_len * 9, "gate breached");
        }
    }
    // verify ran: 7 value streams (6 columns + 1 octet-length lane) × 2 granules.
    assert_eq!(r.granules_verified, 14);

    // Stream-entry facts, GOLDEN (the §6.3 vocabulary the reader consumes).
    let pv = PartView::open(&mut vfs, "part-0.pgrc");
    let (e1, _) = pv.stream(1, 0, StreamRole::Values).expect("col1");
    assert_eq!((e1.encoding, e1.width), (EncodingId::ByteFor.as_u16(), 2));
    let (e2, _) = pv.stream(2, 0, StreamRole::Values).expect("col2");
    assert_eq!((e2.encoding, e2.width), (EncodingId::Alp.as_u16(), 0));
    let (e3, _) = pv.stream(3, 0, StreamRole::Values).expect("col3");
    assert_eq!((e3.encoding, e3.width), (EncodingId::BoolBitmap.as_u16(), 0));
    let (e4, _) = pv.stream(4, 0, StreamRole::Values).expect("col4");
    // 97 NDV bit-packs at 7; the pgrc21-widths byte-align default
    // (PGRUST_PGRC_CODE_WIDTH_BYTE, ON since 2026-08-16) rounds the
    // granule code width up to the byte boundary — golden is 8.
    assert_eq!((e4.encoding, e4.width), (EncodingId::DictCodes.as_u16(), 8));
    assert_ne!(e4.flags & STREAMF_DICT_EXEC, 0, "zero-null dict lane must publish");
    let (e5, _) = pv.stream(5, 0, StreamRole::Values).expect("col5");
    assert_eq!(
        (e5.encoding, e5.width, e5.aux32),
        (EncodingId::PackedNumeric.as_u16(), 2, 2),
        "PACKED_NUMERIC width/scale golden"
    );
    let (e6, _) = pv.stream(6, 0, StreamRole::Values).expect("col6");
    assert_eq!((e6.encoding, e6.width), (EncodingId::Verbatim.as_u16(), 0));
    // Dict companion streams exist, DICT_CODES-stamped. 97 entries = one
    // dict frame, so the SB-7 frame-cut payload extent table has exactly
    // one extent here (multi-frame geometry is pinned by
    // `dict_payload_frame_extents_cut_and_read_back`).
    let (di, di_ext) = pv.stream(4, 0, StreamRole::DictIndex).expect("DictIndex");
    let (dp, dp_ext) = pv.stream(4, 0, StreamRole::DictPayload).expect("DictPayload");
    assert_eq!(di.encoding, EncodingId::DictCodes.as_u16());
    assert_eq!(dp.encoding, EncodingId::DictCodes.as_u16());
    assert_eq!((di.values, dp.values), (97, 97));
    assert_eq!((di_ext.len(), dp_ext.len()), (1, 1), "one frame = one extent");
}

#[test]
fn dict_above_cap_demotes_to_verbatim() {
    let mut vfs = mem_with_dir();
    let cands = CodecCandidates::new(ColumnPosture {
        dict: Some(DictPolicy {
            ndv_cap: 10, // 97 distinct values > 10
            exec_ok: true,
            sem: TextSemantics::BytesOnly,
        }),
        ..ColumnPosture::default()
    });
    let mut kit = FullKit::new(cands);
    let mut w = open_writer(vec![text_col(1)], stamp(92, 1));
    for i in 0..9_000u64 {
        let img = img_4b_u(format!("k{:03}", i % 97).as_bytes());
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv {
            vfs: &mut vfs,
            sources: &sources,
            resolver: &kit.resolver,
            shred: &mut kit.shred,
            shred_opts: &kit.opts,
        };
        w.append_row(&[RawDatum::Bytes(&img)], &mut kit.ext, &mut env)
            .expect("append");
    }
    publish(&mut w, &mut vfs, &mut kit, 92);
    let r = &w.seal_reports()[0];
    assert_eq!(r.elections[0].encoding, EncodingId::Verbatim.as_u16());
    let pv = PartView::open(&mut vfs, "part-0.pgrc");
    assert!(pv.stream(1, 0, StreamRole::DictIndex).is_none());
}

// ---------------------------------------------------------------------------
// The Utf8Chars posture is a CLAIM the election verifies per part —
// verified, never trusted.
// ---------------------------------------------------------------------------

/// A verified Utf8Chars claim on a dict-loser (cap-breach, url-shaped)
/// column runs verbatim with NO degrade note.
#[test]
fn utf8_verified_claim_carries_no_degrade_note() {
    let mut vfs = mem_with_dir();
    let cands = CodecCandidates::new(ColumnPosture {
        dict: Some(DictPolicy {
            ndv_cap: 10, // near-unique url class: cap breach demotes dict
            exec_ok: true,
            sem: TextSemantics::Utf8Chars,
        }),
        ..ColumnPosture::default()
    });
    let mut kit = FullKit::new(cands);
    let mut w = open_writer(vec![text_col(1)], stamp(93, 1));
    for i in 0..9_000u64 {
        // Valid UTF-8 (multibyte included), near-unique.
        let v = format!(
            "https://static.example-cdn.example/assets/catalog/item-{i:07}/image-größe-large.jpeg"
        );
        let img = img_4b_u(v.as_bytes());
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv {
            vfs: &mut vfs,
            sources: &sources,
            resolver: &kit.resolver,
            shred: &mut kit.shred,
            shred_opts: &kit.opts,
        };
        w.append_row(&[RawDatum::Bytes(&img)], &mut kit.ext, &mut env)
            .expect("append");
    }
    publish(&mut w, &mut vfs, &mut kit, 93);
    let r = &w.seal_reports()[0];
    assert_eq!(
        r.elections[0].encoding,
        EncodingId::Verbatim.as_u16(),
        "verified utf8 claim + dict cap breach runs verbatim"
    );
    assert!(r.notes.is_empty(), "no degrade note on a verified claim");
}

/// The SAME shape with ONE invalid-UTF-8 payload: the claim fails byte
/// verification, the stream runs BytesOnly (verbatim),
/// ingest does NOT fail, and the typed degrade note rides the report.
#[test]
fn utf8_claim_degrade_is_typed() {
    let mut vfs = mem_with_dir();
    let cands = CodecCandidates::new(ColumnPosture {
        dict: Some(DictPolicy {
            ndv_cap: 10,
            exec_ok: true,
            sem: TextSemantics::Utf8Chars,
        }),
        ..ColumnPosture::default()
    });
    let mut kit = FullKit::new(cands);
    let mut w = open_writer(vec![text_col(1)], stamp(94, 1));
    for i in 0..9_000u64 {
        // Row 4321 carries an ill-formed sequence (a lone continuation
        // byte) — arbitrary-bytes String data under a UTF8 annotation.
        let v = if i == 4_321 {
            let mut b = format!("https://static.example-cdn.example/assets/catalog/item-{i:07}/")
                .into_bytes();
            b.push(0xBF);
            b
        } else {
            format!(
                "https://static.example-cdn.example/assets/catalog/item-{i:07}/image-large.jpeg"
            )
            .into_bytes()
        };
        let img = img_4b_u(&v);
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv {
            vfs: &mut vfs,
            sources: &sources,
            resolver: &kit.resolver,
            shred: &mut kit.shred,
            shred_opts: &kit.opts,
        };
        w.append_row(&[RawDatum::Bytes(&img)], &mut kit.ext, &mut env)
            .expect("append never fails on invalid utf8 under a claim");
    }
    publish(&mut w, &mut vfs, &mut kit, 94);
    let r = &w.seal_reports()[0];
    assert_eq!(
        r.elections[0].encoding,
        EncodingId::Verbatim.as_u16(),
        "degraded claim runs BytesOnly: dict cap-breaches, verbatim wins"
    );
    assert_eq!(
        r.notes,
        vec![crate::elect::SealNote::Utf8ClaimDegraded {
            attno: 1,
            path_ord: 0
        }],
        "the degrade is a typed census fact"
    );
}

// ---------------------------------------------------------------------------
// THE dict round-trip proof (M3-G gap 1, closed): writer emits a real dict
// stream with a NONZERO §6.3 width byte → the raw-field key cannot resolve
// (tooth) → the reader's normalized path resolves, decode_codes returns the
// exact global codes, decode_full materializes the original strings.
// ---------------------------------------------------------------------------

#[test]
fn dict_stream_reads_back_through_pgrc_read() {
    let mut vfs = mem_with_dir();
    let cands = CodecCandidates::new(ColumnPosture {
        dict: Some(DictPolicy {
            ndv_cap: 100_000,
            exec_ok: true,
            sem: TextSemantics::BytesOnly,
        }),
        ..ColumnPosture::default()
    });
    let mut kit = FullKit::new(cands);
    let mut w = open_writer(vec![text_col(1)], stamp(93, 1));
    let rows = 10_000u64;
    let payload_of = |i: u64| format!("v{:04}", i % 211);
    for i in 0..rows {
        let img = img_4b_u(payload_of(i).as_bytes());
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv {
            vfs: &mut vfs,
            sources: &sources,
            resolver: &kit.resolver,
            shred: &mut kit.shred,
            shred_opts: &kit.opts,
        };
        w.append_row(&[RawDatum::Bytes(&img)], &mut kit.ext, &mut env)
            .expect("append");
    }
    publish(&mut w, &mut vfs, &mut kit, 93);

    let part = open_part(&mut vfs, "part-0.pgrc");
    let binding = codec_binding();
    let mut cur = StreamCursor::open(Arc::clone(&part), &binding, 1, 0).expect("cursor");
    let entry = cur.entry();
    assert_eq!(entry.encoding, EncodingId::DictCodes.as_u16());
    // 211 codes ⇒ max code width 8: the §6.3 width byte is REAL (nonzero) —
    // this stream is exactly the shape the raw-field key could not resolve.
    assert_eq!(entry.width, 8, "max code width stats fact");
    assert_ne!(entry.flags & STREAMF_DICT_EXEC, 0);

    // TOOTH (born-RED shape): the raw entry fields do NOT resolve a kernel.
    let raw = KernelKey {
        encoding: entry.encoding,
        class: entry.class,
        width: entry.width,
    };
    assert!(
        matches!(
            pgrc_codec::registry().resolve(raw),
            Err(FormatError::KernelMissing { .. })
        ),
        "raw-field resolution must refuse — the normalization is load-bearing"
    );
    // The normalization maps the same fields onto the registered key.
    let norm = pgrc_format::enc::stream_kernel_key(entry.encoding, entry.class, entry.width)
        .expect("normalize");
    assert_eq!(norm.width, 0);
    assert!(pgrc_codec::registry().resolve(norm).is_ok());

    // decode_codes: exact global codes (byte-rank order of "v0000".."v0210"
    // == numeric order, so code == i % 211).
    let gcount = cur.granule_count();
    let mut row_base = 0u64;
    for g in 0..gcount {
        let n = cur.values_in_granule(g).expect("values");
        let mut codes = vec![0u32; n as usize];
        let wrote = cur.decode_codes(g, &mut codes).expect("decode_codes");
        assert_eq!(wrote, n);
        for r in 0..n as u64 {
            assert_eq!(
                codes[r as usize] as u64,
                (row_base + r) % 211,
                "global code mismatch at row {}",
                row_base + r
            );
        }
        // decode_full: materialized values byte-identical to the input.
        let (datums, _arena) = decode_granule(&mut cur, g);
        for r in 0..n as u64 {
            assert_varlena_datum(datums[r as usize], payload_of(row_base + r).as_bytes());
        }
        row_base += n as u64;
    }
    assert_eq!(row_base, rows);
}

// ---------------------------------------------------------------------------
// SB-7 (M3-L2): frame-boundary dict extents. UNWRAPPED DictPayload extent
// tables are cut at dict-frame boundaries (per-frame CRC, extent 0 carries
// the header, the tail extent the frame table) and read back through the
// reader's multi-extent assembly; a WRAPPED payload stays single-extent
// whole-section (P-6) with ONE ZSTD BLOCK PER FRAME inside the image.
// ---------------------------------------------------------------------------

/// 60 incompressible bytes per distinct id (splitmix64 stream): long
/// enough that the periodic 4-byte varlena headers (the one repeating
/// pattern in the payload region) stay under the >=20% wrapper gate.
fn sb7_random_payload(i: u64) -> [u8; 60] {
    let mut z = i.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut out = [0u8; 60];
    for c in out.chunks_mut(8) {
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        let b = z.to_le_bytes();
        let n = c.len();
        c.copy_from_slice(&b[..n]);
    }
    out
}

#[test]
fn dict_payload_frame_extents_cut_and_read_back() {
    let mut vfs = mem_with_dir();
    let cands = CodecCandidates::new(ColumnPosture {
        dict: Some(DictPolicy {
            ndv_cap: 100_000,
            exec_ok: true,
            sem: TextSemantics::BytesOnly,
        }),
        ..ColumnPosture::default()
    });
    let mut kit = FullKit::new(cands);
    let mut w = open_writer(vec![text_col(1)], stamp(94, 1));
    let distinct = 2_500u64; // 3 dict frames: 1024 + 1024 + 452
    let rows = 10_000u64;
    for i in 0..rows {
        let payload = sb7_random_payload(i % distinct);
        let img = img_4b_u(&payload);
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv {
            vfs: &mut vfs,
            sources: &sources,
            resolver: &kit.resolver,
            shred: &mut kit.shred,
            shred_opts: &kit.opts,
        };
        w.append_row(&[RawDatum::Bytes(&img)], &mut kit.ext, &mut env)
            .expect("append");
    }
    publish(&mut w, &mut vfs, &mut kit, 94);

    // Geometry leg: incompressible payloads refuse the wrapper (SB-2 gate),
    // so the UNWRAPPED extent table is cut at frame boundaries.
    let pv = PartView::open(&mut vfs, "part-0.pgrc");
    let (dp, dp_ext) = pv.stream(1, 0, StreamRole::DictPayload).expect("DictPayload");
    assert_eq!(
        dp.wrapper,
        0,
        "random payloads must refuse the wrapper (the unwrapped-extents leg needs it)"
    );
    assert_eq!(dp_ext.len(), 3, "ceil(2500/1024) frame extents");
    assert_eq!(
        dp_ext.iter().map(|e| e.values).collect::<Vec<_>>(),
        vec![1024, 1024, 452],
        "entries per frame"
    );
    for (f, e) in dp_ext.iter().enumerate() {
        assert_eq!(e.granule_start, f as u32, "extent {f} carries its frame ordinal");
        assert_eq!(e.granule_count, 1);
    }
    for wpair in dp_ext.windows(2) {
        assert_eq!(
            wpair[0].file_off + wpair[0].len,
            wpair[1].file_off,
            "frame extents tile the stored section contiguously"
        );
    }
    // The index stays single-extent (the small always-resident half).
    let (_, di_ext) = pv.stream(1, 0, StreamRole::DictIndex).expect("DictIndex");
    assert_eq!(di_ext.len(), 1);

    // Read-back leg: the reader ASSEMBLES the multi-extent payload (per-
    // extent CRC validation is the frame-grain fault) and materializes the
    // exact input strings through the dict.
    let part = open_part(&mut vfs, "part-0.pgrc");
    let binding = codec_binding();
    let mut cur = StreamCursor::open(Arc::clone(&part), &binding, 1, 0).expect("cursor");
    assert_eq!(cur.entry().encoding, EncodingId::DictCodes.as_u16());
    let n = cur.values_in_granule(0).expect("values");
    let (datums, _arena) = decode_granule(&mut cur, 0);
    for r in 0..n as u64 {
        assert_varlena_datum(datums[r as usize], &sb7_random_payload(r % distinct));
    }
}

// ---------------------------------------------------------------------------
// Wrapper election (CMP-A shape): offered on EVERY election (O-CMP-3(a)),
// both arms priced exactly under the ≥20% wrapper law (O-CMP-4(a)), the
// smaller image elected; wrapped streams read back byte-exact through the
// reader's unwrap path with decompress-once residency (O-CMP-5(a)); a
// binding without the unwrapper refuses BEFORE any payload fault.
// ---------------------------------------------------------------------------

#[test]
fn wrapped_stream_reads_back_and_refuses_without_unwrapper() {
    let mut vfs = mem_with_dir();
    // DEFAULT posture: the wrapper offer no longer needs `cold` —
    // O-CMP-3(a)'s widening is exactly what this fixture witnesses.
    let cands = CodecCandidates::new(ColumnPosture::default());
    let mut kit = FullKit::new(cands);
    let mut w = open_writer(vec![text_col(1)], stamp(94, 1));
    let rows = 10_000u64;
    let payload_of = |i: u64| format!("{}{}", "pad-".repeat(15), i % 5);
    for i in 0..rows {
        let img = img_4b_u(payload_of(i).as_bytes());
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv {
            vfs: &mut vfs,
            sources: &sources,
            resolver: &kit.resolver,
            shred: &mut kit.shred,
            shred_opts: &kit.opts,
        };
        w.append_row(&[RawDatum::Bytes(&img)], &mut kit.ext, &mut env)
            .expect("append");
    }
    publish(&mut w, &mut vfs, &mut kit, 94);

    let pv = PartView::open(&mut vfs, "part-0.pgrc");
    let (e, _) = pv.stream(1, 0, StreamRole::Values).expect("col1");
    assert_eq!(e.encoding, EncodingId::Verbatim.as_u16());
    // Every arm clears the ≥20% law on this shape and the smallest exact
    // image is elected: the OnPair verbatim arm's when it is priced, else zstd's.
    let want = if pgrc_codec::onpairwrap::onpair_verbatim_on() { Wrapper::OnPairZstd } else { Wrapper::Zstd };
    assert_eq!(e.wrapper, want.as_u8(), "the wrapper election engaged");

    // Reads back byte-exact through the unwrap path.
    let part = open_part(&mut vfs, "part-0.pgrc");
    let binding = codec_binding();
    let mut cur = StreamCursor::open(Arc::clone(&part), &binding, 1, 0).expect("cursor");
    let mut row_base = 0u64;
    for g in 0..cur.granule_count() {
        let n = cur.values_in_granule(g).expect("values");
        let (datums, _arena) = decode_granule(&mut cur, g);
        for r in 0..n as u64 {
            assert_varlena_datum(datums[r as usize], payload_of(row_base + r).as_bytes());
        }
        row_base += n as u64;
    }
    assert_eq!(row_base, rows);

    // O-CMP-5(a) residency: the unwrapped image is cached on the PART —
    // a SECOND cursor over the same extents faults nothing new (the fault
    // log is the decompress-once witness; a cache hit records nothing).
    let faults_after_first = part.faults().len();
    let resident_after_first = part.resident();
    let mut cur2 = StreamCursor::open(Arc::clone(&part), &binding, 1, 0).expect("cursor 2");
    let (_datums, _arena) = decode_granule(&mut cur2, 0);
    assert_eq!(
        part.faults().len(),
        faults_after_first,
        "second cursor must hit the unwrapped-image cache (decompress once per residency)"
    );
    assert_eq!(
        part.resident(),
        resident_after_first,
        "a cache hit must not grow residency"
    );

    // TOOTH: no unwrapper registered ⇒ typed refusal at cursor open.
    let bare = CodecBinding {
        registry: pgrc_codec::registry(),
        unwrappers: &[],
    };
    let part2 = open_part(&mut vfs, "part-0.pgrc");
    assert!(
        StreamCursor::open(part2, &bare, 1, 0).is_err(),
        "wrapped stream without an unwrapper must refuse at open"
    );
    // TOOTH: an LZ4-only binding is equally refused for the elected arm's
    // stream — the old-binary posture (§8 refusal-before-fault, arm-specific).
    let lz4_only = CodecBinding {
        registry: pgrc_codec::registry(),
        unwrappers: &[&LZ4_UNWRAP],
    };
    let part3 = open_part(&mut vfs, "part-0.pgrc");
    let err = StreamCursor::open(part3, &lz4_only, 1, 0).expect_err("lz4-only binding");
    assert!(
        matches!(
            err,
            pgrc_read::ReadError::Format(FormatError::WrapperUnsupported { wrapper }) if wrapper == want.as_u8()
        ),
        "a binding without the elected arm's unwrapper must refuse WrapperUnsupported naming it, got {err:?}"
    );
}

#[test]
fn hot_fixed_width_stream_wraps_and_noise_does_not() {
    // The O-CMP-3(a) widening witness at seal grain, with its negative
    // control: a compressible HOT int8 ByteFor stream (a shape the old
    // cold-only policy never offered) wraps; a full-entropy int8 stream
    // (the incompressible guard) stays unwrapped.
    let mut vfs = mem_with_dir();
    let cands = CodecCandidates::new(ColumnPosture::default());
    let mut kit = FullKit::new(cands);
    let mut w = open_writer(vec![int8_col(1), int8_col(2)], stamp(95, 1));
    let mut seed = 0x77AB_u64;
    let rows = 10_000u64;
    for i in 0..rows {
        let noise = splitmix(&mut seed);
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv {
            vfs: &mut vfs,
            sources: &sources,
            resolver: &kit.resolver,
            shred: &mut kit.shred,
            shred_opts: &kit.opts,
        };
        w.append_row(
            &[RawDatum::Word(i), RawDatum::Word(noise)],
            &mut kit.ext,
            &mut env,
        )
        .expect("append");
    }
    publish(&mut w, &mut vfs, &mut kit, 95);

    let pv = PartView::open(&mut vfs, "part-0.pgrc");
    let (e1, _) = pv.stream(1, 0, StreamRole::Values).expect("col1");
    assert_eq!(e1.encoding, EncodingId::ByteFor.as_u16(), "seq int8 stays ByteFor");
    assert!(
        e1.wrapper == Wrapper::Zstd.as_u8() || e1.wrapper == Wrapper::IntPre.as_u8(),
        "hot compressible ByteFor stream must take the widened wrapper offer (zstd, or its integer pre-pass), got {}",
        e1.wrapper
    );
    let (e2, _) = pv.stream(2, 0, StreamRole::Values).expect("col2");
    assert_eq!(e2.encoding, EncodingId::Verbatim.as_u16(), "noise demotes to verbatim");
    assert_eq!(
        e2.wrapper,
        Wrapper::None.as_u8(),
        "full-entropy stream must fail the ≥20% wrapper law (negative control)"
    );

    // Byte-exact read-back of the wrapped hot stream.
    let part = open_part(&mut vfs, "part-0.pgrc");
    let binding = codec_binding();
    let mut cur = StreamCursor::open(Arc::clone(&part), &binding, 1, 0).expect("cursor");
    let mut row_base = 0u64;
    for g in 0..cur.granule_count() {
        let n = cur.values_in_granule(g).expect("values");
        let (datums, _arena) = decode_granule(&mut cur, g);
        for r in 0..n as u64 {
            assert_eq!(datums[r as usize], row_base + r, "wrapped ByteFor decode");
        }
        row_base += n as u64;
    }
    assert_eq!(row_base, rows);
}

/// Delegating source that pins `offer_wrapper = false` — the A/B control
/// for the #597 operand law below.
struct NoWrapSource(CodecCandidates);

impl crate::elect::CandidateSource for NoWrapSource {
    fn propose(&self, _input: &crate::elect::FullElectInput<'_>) -> Vec<crate::elect::Candidate> {
        Vec::new()
    }
    fn elect_full(
        &self,
        input: &crate::elect::FullElectInput<'_>,
    ) -> Option<crate::WriteResult<crate::elect::FullElection>> {
        self.0.elect_full(input).map(|r| {
            r.map(|mut f| {
                f.offer_wrapper = false;
                f
            })
        })
    }
}

#[test]
fn wrapped_parts_are_byte_identical_across_runs() {
    // The byte-identical-parts law extended to wrapped parts: part bytes
    // are a pure function of (schema, elections, input row partition) —
    // which requires the zstd arm itself to be deterministic at a pinned
    // level + library version. Two independent runs over identical input
    // must produce byte-identical part files, wrapped streams included.
    let run = || -> Vec<u8> {
        let mut vfs = mem_with_dir();
        let cands = CodecCandidates::new(ColumnPosture::default());
        let mut kit = FullKit::new(cands);
        let mut w = open_writer(vec![text_col(1), int8_col(2)], stamp(97, 1));
        let payload_of = |i: u64| format!("{}{}", "pad-".repeat(15), i % 5);
        for i in 0..10_000u64 {
            let img = img_4b_u(payload_of(i).as_bytes());
            let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
            let mut env = SealEnv {
                vfs: &mut vfs,
                sources: &sources,
                resolver: &kit.resolver,
                shred: &mut kit.shred,
                shred_opts: &kit.opts,
            };
            w.append_row(&[RawDatum::Bytes(&img), RawDatum::Word(i)], &mut kit.ext, &mut env)
                .expect("append");
        }
        publish(&mut w, &mut vfs, &mut kit, 97);
        // Sanity: the fixture actually wrapped (else the pin is vacuous).
        let pv = PartView::open(&mut vfs, "part-0.pgrc");
        let (e, _) = pv.stream(1, 0, StreamRole::Values).expect("col1");
        assert_ne!(e.wrapper, Wrapper::None.as_u8(), "fixture must wrap");
        vfs.read_full(&format!("{DIR}/part-0.pgrc")).expect("part bytes")
    };
    assert_eq!(run(), run(), "wrapped part bytes diverged across identical runs");
}

#[test]
fn two_witness_null_law_holds_on_wrapped_parts() {
    // The seal's two-witness nonnull cross-check runs on the UNWRAPPED
    // emitted bytes and must be unperturbed by a downstream wrapper
    // election: a NULL-carrying compressible column wraps AND banks its
    // crosschecks, and the wrapped part reads back with nulls intact.
    let mut vfs = mem_with_dir();
    let cands = CodecCandidates::new(ColumnPosture::default());
    let mut kit = FullKit::new(cands);
    let mut w = open_writer(vec![text_col(1)], stamp(98, 1));
    let rows = 10_000u64;
    let payload_of = |i: u64| format!("{}{}", "pad-".repeat(15), i % 5);
    for i in 0..rows {
        let img = img_4b_u(payload_of(i).as_bytes());
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv {
            vfs: &mut vfs,
            sources: &sources,
            resolver: &kit.resolver,
            shred: &mut kit.shred,
            shred_opts: &kit.opts,
        };
        let row = if i % 7 == 0 {
            [RawDatum::Null]
        } else {
            [RawDatum::Bytes(&img)]
        };
        w.append_row(&row, &mut kit.ext, &mut env).expect("append");
    }
    publish(&mut w, &mut vfs, &mut kit, 98);
    let r = &w.seal_reports()[0];
    assert!(r.nonnull_crosschecks > 0, "two-witness checks must have RUN");

    let pv = PartView::open(&mut vfs, "part-0.pgrc");
    let (e, _) = pv.stream(1, 0, StreamRole::Values).expect("col1");
    assert_ne!(e.wrapper, Wrapper::None.as_u8(), "fixture must wrap");
    pv.stream(1, 0, StreamRole::Validity).expect("validity stream exists");

    // Read back: the validity mask agrees with the null pattern and valid
    // rows decode byte-exactly through the unwrap path.
    let part = open_part(&mut vfs, "part-0.pgrc");
    let binding = codec_binding();
    let mut cur = StreamCursor::open(Arc::clone(&part), &binding, 1, 0).expect("cursor");
    let mut row_base = 0u64;
    for g in 0..cur.granule_count() {
        let n = cur.values_in_granule(g).expect("values");
        let mut mask = vec![0u64; (n as usize).div_ceil(64)];
        cur.validity(g, &mut mask).expect("validity");
        let (datums, _arena) = decode_granule(&mut cur, g);
        for r in 0..n as u64 {
            let i = row_base + r;
            let valid = (mask[(r / 64) as usize] >> (r % 64)) & 1 != 0;
            assert_eq!(valid, i % 7 != 0, "validity mismatch at row {i}");
            if valid {
                assert_varlena_datum(datums[r as usize], payload_of(i).as_bytes());
            }
        }
        row_base += n as u64;
    }
    assert_eq!(row_base, rows);
}

#[test]
fn wrapping_never_moves_a_cut_boundary() {
    // The #597 should_cut operand law, RELEASE-effective on wrapped parts:
    // `bytes` is the pre-encode `ColBuffer::approx_bytes` sum, wrapping
    // happens strictly downstream at seal — so a wrapper-ON run and a
    // wrapper-OFF run over identical input must cut IDENTICAL partitions
    // (same part count, same per-part row counts), while their part bytes
    // legally differ.
    let policy = PartCutPolicy {
        max_rows: 1 << 20,
        max_bytes: 640 << 10, // byte-budget governs on this corpus
        ..PartCutPolicy::default()
    };
    let payload_of = |i: u64| format!("{}{}", "pad-".repeat(15), i % 5);
    let run = |wrap: bool| -> (Vec<u64>, bool) {
        let mut vfs = mem_with_dir();
        let on = CodecCandidates::new(ColumnPosture::default());
        let off = NoWrapSource(CodecCandidates::new(ColumnPosture::default()));
        let mut kit = FullKit::new(CodecCandidates::new(ColumnPosture::default()));
        let mut w = open_writer_policy(vec![text_col(1)], stamp(96, 1), policy);
        for i in 0..24_000u64 {
            let img = img_4b_u(payload_of(i).as_bytes());
            let sources: [&dyn crate::elect::CandidateSource; 1] =
                if wrap { [&on] } else { [&off] };
            let mut env = SealEnv {
                vfs: &mut vfs,
                sources: &sources,
                resolver: &kit.resolver,
                shred: &mut kit.shred,
                shred_opts: &kit.opts,
            };
            w.append_row(&[RawDatum::Bytes(&img)], &mut kit.ext, &mut env)
                .expect("append");
        }
        {
            let sources: [&dyn crate::elect::CandidateSource; 1] =
                if wrap { [&on] } else { [&off] };
            let mut env = SealEnv {
                vfs: &mut vfs,
                sources: &sources,
                resolver: &kit.resolver,
                shred: &mut kit.shred,
                shred_opts: &kit.opts,
            };
            w.finish(&mut env).expect("finish");
        }
        let rows: Vec<u64> = w.sealed_parts().iter().map(|p| p.rows).collect();
        // Whether ANY stream in the run actually wrapped (arm sanity).
        let mut any_wrapped = false;
        for seq in 0..w.sealed_parts().len() as u32 {
            let name = pgrc_format::dirlayout::temp_file_name(96, seq);
            let bytes = vfs.read_full(&format!("{DIR}/{name}")).expect("tmp bytes");
            let io = MemPartIo::new(bytes, 4242, 7);
            let part = OpenPart::open(Box::new(io), &PartExpect::none()).expect("open");
            let dir = part.stream_directory().expect("dir");
            if let Some(ps) = dir.lookup(1, 0, StreamRole::Values) {
                if ps.entry.wrapper != Wrapper::None.as_u8() {
                    any_wrapped = true;
                }
            }
        }
        (rows, any_wrapped)
    };
    let (rows_on, wrapped_on) = run(true);
    let (rows_off, wrapped_off) = run(false);
    assert!(rows_on.len() > 1, "the fixture must actually cut (else vacuous)");
    assert!(wrapped_on, "the ON arm must actually wrap (else vacuous)");
    assert!(!wrapped_off, "the OFF control must stay unwrapped");
    assert_eq!(
        rows_on, rows_off,
        "wrapping moved a cut boundary — the #597 operand law is breached"
    );
}

// ---------------------------------------------------------------------------
// The v1 single-extent overflow law (spec §6.8, amended): a multi-band part
// with oversize values in EVERY band emits ONE overflow extent, and the
// merged reader (which refuses multi-extent overflow) decodes it.
// ---------------------------------------------------------------------------

#[test]
fn multi_band_overflow_single_extent_reads_back() {
    let mut vfs = mem_with_dir();
    let mut kit = Kit::new(); // legacy stats-only sources: the fallback path
    let mut w = open_writer(vec![text_col(1)], stamp(95, 1));
    let rows = 70_000u64; // 2 bands (65,536 + 4,464), 9 granules
    let big_a = "A".repeat(40_000);
    let big_b = "B".repeat(40_000);
    let payload_of = |i: u64| -> String {
        if i == 100 {
            big_a.clone()
        } else if i == 66_000 {
            big_b.clone()
        } else {
            format!("s{}", i % 41)
        }
    };
    for i in 0..rows {
        let img = img_4b_u(payload_of(i).as_bytes());
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv {
            vfs: &mut vfs,
            sources: &sources,
            resolver: &kit.resolver,
            shred: &mut kit.shred,
            shred_opts: &kit.opts,
        };
        w.append_row(&[RawDatum::Bytes(&img)], &mut kit.ext, &mut env)
            .expect("append");
    }
    let probe = Probe::new(TxnVerdict::InProgress).set(95, TxnVerdict::Committed);
    finish_and_publish(&mut w, &mut vfs, &mut kit, &probe);

    let pv = PartView::open(&mut vfs, "part-0.pgrc");
    let (oe, oext) = pv.stream(1, 0, StreamRole::Overflow).expect("overflow stream");
    assert_eq!(oext.len(), 1, "v1 single-extent law: one overflow extent");
    assert_eq!(oe.values, 2, "both bands' oversize values in the one region");

    // The merged reader decodes BOTH bands' oversize rows.
    let part = open_part(&mut vfs, "part-0.pgrc");
    let binding = codec_binding();
    let mut cur = StreamCursor::open(Arc::clone(&part), &binding, 1, 0).expect("cursor");
    // Row 100 lives in granule 0; row 66,000 in granule 8 (row 464 there).
    let (datums0, _a0) = decode_granule(&mut cur, 0);
    assert_varlena_datum(datums0[100], big_a.as_bytes());
    let (datums8, _a8) = decode_granule(&mut cur, 8);
    assert_varlena_datum(datums8[464], big_b.as_bytes());
}

// ---------------------------------------------------------------------------
// CMP-B election widening (docs/design/pgrc-compression.md §3 CMP-B row,
// ruled 2026-08-10 with O-CMP-1..8): (1) the cold/size STORAGE posture
// admits DELTA_FOR as a size candidate exactly where its exact bytes win;
// (2) the wrapper offer widens to the byte-run stream CLASS (overflow /
// dict index / dict payload) under the same ≥20% wrapper law, decompressed
// once per part residency, with typed refusal before any payload fault.
// ---------------------------------------------------------------------------

static TEST_UNWRAPPERS: [&dyn SectionUnwrapper; 6] = [&LZ4_UNWRAP, &ZSTD_UNWRAP, &FRONT_UNWRAP, &PACKED_UNWRAP, &FRONT_ONPAIR_UNWRAP, &ONPAIR_VALUES_UNWRAP];

/// The §9.2 timestamp shape at unit scale: µs-since-epoch steps defeat
/// byte-aligned FOR (per-frame range > 4 bytes ⇒ width 8 ⇒ BelowWinGate ⇒
/// VERBATIM), while DELTA_FOR's per-frame zigzag deltas are 3 bytes wide.
/// Born-RED pair: the default (non-storage) posture ships VERBATIM — the
/// gate CMP-B flips; the storage posture elects DELTA_FOR; answers are
/// byte-identical either way (elections change layout, never answers).
#[test]
fn deltafor_size_arm_elects_under_storage_posture_where_it_wins() {
    let base: u64 = 1_600_000_000_000_000;
    let step: u64 = 5_000_000; // 5s in µs: frame range ≈ 5.1e9 > u32
    let value_of = |i: u64| base + i * step;
    let rows = 20_000u64;
    let run = |cold: bool| -> (u16, Vec<u8>) {
        let mut vfs = mem_with_dir();
        let posture = ColumnPosture {
            cold,
            ..ColumnPosture::default()
        };
        let cands = CodecCandidates::new(posture);
        let mut kit = FullKit::new(cands);
        let mut w = open_writer(vec![int8_col(1)], stamp(99, 1));
        for i in 0..rows {
            let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
            let mut env = SealEnv {
                vfs: &mut vfs,
                sources: &sources,
                resolver: &kit.resolver,
                shred: &mut kit.shred,
                shred_opts: &kit.opts,
            };
            w.append_row(&[RawDatum::Word(value_of(i))], &mut kit.ext, &mut env)
                .expect("append");
        }
        publish(&mut w, &mut vfs, &mut kit, 99);
        let pv = PartView::open(&mut vfs, "part-0.pgrc");
        let (e, _) = pv.stream(1, 0, StreamRole::Values).expect("col1");

        // Decode-backed answers for the identity cross-check.
        let part = open_part(&mut vfs, "part-0.pgrc");
        let binding = codec_binding();
        let mut cur = StreamCursor::open(Arc::clone(&part), &binding, 1, 0).expect("cursor");
        let mut all: Vec<u8> = Vec::new();
        let mut row_base = 0u64;
        for g in 0..cur.granule_count() {
            let n = cur.values_in_granule(g).expect("values");
            let (datums, _arena) = decode_granule(&mut cur, g);
            for r in 0..n as u64 {
                assert_eq!(
                    datums[r as usize],
                    value_of(row_base + r),
                    "decode mismatch at row {}",
                    row_base + r
                );
                all.extend_from_slice(&datums[r as usize].to_le_bytes());
            }
            row_base += n as u64;
        }
        assert_eq!(row_base, rows);
        (e.encoding, all)
    };
    let (enc_default, vals_default) = run(false);
    let (enc_storage, vals_storage) = run(true);
    // v4 delta (ledger SB-3): the control arm's demotion WAS the v3 gap —
    // widths {1,2,4,8} forced width 8 and the >=10% gate failed. With the
    // completed {1..8} ladder this per-frame range (~5.1e9, 33 bits) elects
    // ByteFor at width 5 — the exact CMP-B recovery this shape existed to
    // demonstrate. The storage-posture contrast below is unchanged:
    // DELTA_FOR still displaces plain FOR as the size candidate.
    assert_eq!(
        enc_default,
        EncodingId::ByteFor.as_u16(),
        "control arm must elect the widened FOR ladder (SB-3; was Verbatim in v3)"
    );
    // The widening: DELTA_FOR competes as a size candidate and wins.
    assert_eq!(
        enc_storage,
        EncodingId::DeltaFor.as_u16(),
        "storage posture must elect DELTA_FOR where it wins"
    );
    // Elections change layout, never answers.
    assert_eq!(vals_default, vals_storage, "answers moved across postures");
}

/// Dict payload/index take the byte-run wrapper offer: a compressible dict
/// payload wraps (≥20% law), reads back byte-exact through cursor AND
/// DictHandle, decompresses once per part residency, and refuses TYPED at
/// open without the unwrapper (before any payload fault). The negative
/// control (incompressible dict payload) stays unwrapped.
#[test]
fn wrapped_dict_sections_read_back_with_residency_and_refusal() {
    let mut vfs = mem_with_dir();
    let dict_posture = ColumnPosture {
        dict: Some(DictPolicy {
            ndv_cap: 100_000,
            exec_ok: true,
            sem: TextSemantics::BytesOnly,
        }),
        ..ColumnPosture::default()
    };
    let cands = CodecCandidates::new(ColumnPosture::default())
        .with_column(1, 0, dict_posture)
        .with_column(2, 0, dict_posture);
    let mut kit = FullKit::new(cands);
    let mut w = open_writer(vec![text_col(1), text_col(2)], stamp(101, 1));
    let rows = 10_000u64;
    // Col 1: compressible dict payload (shared prefix + padding), with a
    // pseudo-random VALUE order so the code stream itself stays unwrapped
    // (the companion-refusal tooth below must bite on the DICT stream, not
    // the values stream).
    let comp_of = |i: u64| {
        let mut s = i.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        s ^= s >> 29;
        format!("shared-prefix-{:03}-padpadpadpadpadpadpadpad", s % 211)
    };
    // Col 2: incompressible dict payload (random 32-B hex-free bytes).
    let mut noise_entries: Vec<Vec<u8>> = Vec::new();
    let mut seed = 0xD1C7_u64;
    for _ in 0..211 {
        let mut e = [0u8; 32];
        for chunk in e.chunks_mut(8) {
            chunk.copy_from_slice(&splitmix(&mut seed).to_le_bytes());
        }
        noise_entries.push(e.to_vec());
    }
    for i in 0..rows {
        let img1 = img_4b_u(comp_of(i).as_bytes());
        let img2 = img_4b_u(&noise_entries[(i % 211) as usize]);
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv {
            vfs: &mut vfs,
            sources: &sources,
            resolver: &kit.resolver,
            shred: &mut kit.shred,
            shred_opts: &kit.opts,
        };
        w.append_row(
            &[RawDatum::Bytes(&img1), RawDatum::Bytes(&img2)],
            &mut kit.ext,
            &mut env,
        )
        .expect("append");
    }
    publish(&mut w, &mut vfs, &mut kit, 101);

    let pv = PartView::open(&mut vfs, "part-0.pgrc");
    let (v1, _) = pv.stream(1, 0, StreamRole::Values).expect("col1 values");
    assert_eq!(v1.encoding, EncodingId::DictCodes.as_u16(), "col1 dict-elects");
    assert_eq!(
        v1.wrapper,
        Wrapper::None.as_u8(),
        "fixture contract: col1 CODE stream must stay unwrapped (random code order)"
    );
    let (dp1, _) = pv.stream(1, 0, StreamRole::DictPayload).expect("col1 payload");
    assert_ne!(
        dp1.wrapper,
        Wrapper::None.as_u8(),
        "compressible dict PAYLOAD must take the byte-run wrapper offer"
    );
    let (dp2, _) = pv.stream(2, 0, StreamRole::DictPayload).expect("col2 payload");
    assert_eq!(
        dp2.wrapper,
        Wrapper::None.as_u8(),
        "incompressible dict payload must fail the ≥20% law (negative control)"
    );

    // Byte-exact read-back through the cursor face (dict materialization
    // reads the wrapped payload region through the unwrap path).
    let part = open_part(&mut vfs, "part-0.pgrc");
    let binding = codec_binding();
    let mut cur = StreamCursor::open(Arc::clone(&part), &binding, 1, 0).expect("cursor");
    let mut row_base = 0u64;
    for g in 0..cur.granule_count() {
        let n = cur.values_in_granule(g).expect("values");
        let (datums, _arena) = decode_granule(&mut cur, g);
        for r in 0..n as u64 {
            assert_varlena_datum(datums[r as usize], comp_of(row_base + r).as_bytes());
        }
        row_base += n as u64;
    }
    assert_eq!(row_base, rows);

    // O-CMP-5(a) residency on the byte-run class: a SECOND cursor faults
    // nothing new and grows nothing — the unwrapped dict region is cached
    // on the part (this is also the StrView §7b stability argument: the
    // rebuilt region is part-resident for the part's life).
    let faults_after_first = part.faults().len();
    let resident_after_first = part.resident();
    let mut cur2 = StreamCursor::open(Arc::clone(&part), &binding, 1, 0).expect("cursor 2");
    let (_d, _a) = decode_granule(&mut cur2, 0);
    assert_eq!(part.faults().len(), faults_after_first, "decompress once per residency");
    assert_eq!(part.resident(), resident_after_first, "cache hit must not grow residency");

    // DictHandle face over the wrapped payload: entries materialize.
    let h = pgrc_read::dicthandle::DictHandle::open(
        Arc::clone(&part),
        None,
        &TEST_UNWRAPPERS,
        1,
        0,
    )
    .expect("dict handle");
    assert_eq!(h.ncodes(), 211);
    for code in 0..h.ncodes() {
        let e = h.entry(code).expect("entry");
        assert!(e.bytes.starts_with(b"shared-prefix-"), "entry payload shape");
    }

    // TOOTH (refusal-before-fault, the old-binary posture): a binding
    // without unwrappers refuses TYPED at cursor open on the COMPANION
    // dict stream (the values stream is unwrapped by fixture contract),
    // and DictHandle::open refuses the same way.
    let bare = CodecBinding {
        registry: pgrc_codec::registry(),
        unwrappers: &[],
    };
    let part2 = open_part(&mut vfs, "part-0.pgrc");
    let err = StreamCursor::open(Arc::clone(&part2), &bare, 1, 0).expect_err("bare binding");
    assert!(
        matches!(
            err,
            pgrc_read::ReadError::Format(FormatError::WrapperUnsupported { .. })
        ),
        "wrapped dict stream without an unwrapper must refuse typed at open, got {err:?}"
    );
    let herr = match pgrc_read::dicthandle::DictHandle::open(part2, None, &[], 1, 0) {
        Ok(_) => panic!("bare dict handle must refuse"),
        Err(e) => e,
    };
    assert!(
        matches!(
            herr,
            pgrc_read::ReadError::Format(FormatError::WrapperUnsupported { .. })
        ),
        "DictHandle over a wrapped dict stream without an unwrapper must refuse typed, got {herr:?}"
    );
}

/// The overflow stream takes the byte-run wrapper offer: compressible
/// oversize values wrap the ONE overflow extent; the reader rebuilds the
/// region and materializes the oversize rows byte-exactly.
#[test]
fn wrapped_overflow_region_reads_back() {
    let mut vfs = mem_with_dir();
    let cands = CodecCandidates::new(ColumnPosture::default());
    let mut kit = FullKit::new(cands);
    let mut w = open_writer(vec![text_col(1)], stamp(102, 1));
    let rows = 9_000u64;
    let big = "Z".repeat(40_000); // ≥ OVERSIZE_THRESHOLD, highly compressible
    let payload_of = |i: u64| -> String {
        if i == 7 || i == 8_500 {
            big.clone()
        } else {
            format!("s{}", i % 41)
        }
    };
    for i in 0..rows {
        let img = img_4b_u(payload_of(i).as_bytes());
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv {
            vfs: &mut vfs,
            sources: &sources,
            resolver: &kit.resolver,
            shred: &mut kit.shred,
            shred_opts: &kit.opts,
        };
        w.append_row(&[RawDatum::Bytes(&img)], &mut kit.ext, &mut env)
            .expect("append");
    }
    publish(&mut w, &mut vfs, &mut kit, 102);

    let pv = PartView::open(&mut vfs, "part-0.pgrc");
    let (oe, oext) = pv.stream(1, 0, StreamRole::Overflow).expect("overflow stream");
    assert_eq!(oext.len(), 1, "v1 single-extent law preserved");
    assert_eq!(oe.values, 2);
    assert_ne!(
        oe.wrapper,
        Wrapper::None.as_u8(),
        "compressible overflow region must take the byte-run wrapper offer"
    );

    let part = open_part(&mut vfs, "part-0.pgrc");
    let binding = codec_binding();
    let mut cur = StreamCursor::open(Arc::clone(&part), &binding, 1, 0).expect("cursor");
    let (d0, _a0) = decode_granule(&mut cur, 0);
    assert_varlena_datum(d0[7], big.as_bytes());
    let g = 8_500 / 8_192;
    let (dg, _ag) = decode_granule(&mut cur, g);
    assert_varlena_datum(dg[(8_500 % 8_192) as usize], big.as_bytes());
}

/// Corrupt tooth on the widened class: a flipped byte inside a WRAPPED dict
/// payload extent refuses TYPED (CRC first — the wrapper CRC law covers the
/// wrapped bytes on disk), never a panic, never silent wrong bytes.
#[test]
fn corrupt_wrapped_dict_payload_refuses_typed() {
    let mut vfs = mem_with_dir();
    let dict_posture = ColumnPosture {
        dict: Some(DictPolicy {
            ndv_cap: 100_000,
            exec_ok: true,
            sem: TextSemantics::BytesOnly,
        }),
        ..ColumnPosture::default()
    };
    let cands = CodecCandidates::new(ColumnPosture::default()).with_column(1, 0, dict_posture);
    let mut kit = FullKit::new(cands);
    let mut w = open_writer(vec![text_col(1)], stamp(103, 1));
    let comp_of = |i: u64| {
        let mut s = i.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        s ^= s >> 29;
        format!("shared-prefix-{:03}-padpadpadpadpadpadpadpad", s % 211)
    };
    for i in 0..10_000u64 {
        let img = img_4b_u(comp_of(i).as_bytes());
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv {
            vfs: &mut vfs,
            sources: &sources,
            resolver: &kit.resolver,
            shred: &mut kit.shred,
            shred_opts: &kit.opts,
        };
        w.append_row(&[RawDatum::Bytes(&img)], &mut kit.ext, &mut env)
            .expect("append");
    }
    publish(&mut w, &mut vfs, &mut kit, 103);

    let pv = PartView::open(&mut vfs, "part-0.pgrc");
    let (dp, dpx) = pv.stream(1, 0, StreamRole::DictPayload).expect("payload");
    assert_ne!(dp.wrapper, Wrapper::None.as_u8(), "fixture must wrap");
    let rec = dpx[0];

    let mut bytes = vfs
        .read_full(&format!("{DIR}/part-0.pgrc"))
        .expect("part bytes");
    let mid = rec.file_off as usize + (rec.len as usize) / 2;
    bytes[mid] ^= 0x40;
    let io = MemPartIo::new(bytes, 4242, 7);
    let part = Arc::new(OpenPart::open(Box::new(io), &PartExpect::none()).expect("open part"));
    let binding = codec_binding();
    let mut cur = StreamCursor::open(Arc::clone(&part), &binding, 1, 0).expect("cursor opens");
    let n = cur.values_in_granule(0).expect("values") as usize;
    let mut datums = vec![0u64; n];
    let mut arena_buf = vec![0u8; 4 << 20];
    let res = {
        let mut out = DecodeOut {
            datums: &mut datums,
            arena: ByteArena::new(&mut arena_buf),
        };
        cur.decode_full(0, &mut out)
    };
    assert!(res.is_err(), "corrupt wrapped dict payload must refuse, not decode");
}

// ---------------------------------------------------------------------------
// DICT-DEDUP: seal-grain distribution equivalence (born-RED charter)
// ---------------------------------------------------------------------------

/// The dedup's end-to-end law at SEALED-BYTES grain: on a dict-elected
/// column the Stats-sidecar distribution is served from the dict build's
/// counted set (the feed), and it must equal what the #971 accumulator
/// computes over the same non-null payload sequence — byte-for-byte, long
/// values (dict-legal, list-ineligible) and nulls included. RED if the
/// feed path drifts from the accumulator in either content or gating.
#[test]
fn dict_elected_sidecar_distribution_matches_accumulator_reference() {
    let mut vfs = mem_with_dir();
    let cands = CodecCandidates::new(ColumnPosture::default()).with_column(
        1,
        0,
        ColumnPosture {
            dict: Some(DictPolicy {
                ndv_cap: 10_000,
                exec_ok: true,
                sem: TextSemantics::Utf8Chars,
            }),
            ..ColumnPosture::default()
        },
    );
    let mut kit = FullKit::new(cands);
    let mut w = open_writer(vec![text_col(1)], stamp(97, 1));
    let rows = 2_000u64;
    let long_val = vec![b'L'; 300]; // > STATS_MCV_VALUE_MAX: counted, list-ineligible
    let mut payloads: Vec<Option<Vec<u8>>> = Vec::new();
    for i in 0..rows {
        let p = if i % 17 == 0 {
            None
        } else if i % 501 == 0 {
            Some(long_val.clone())
        } else {
            Some(format!("k{:03}", i % 97).into_bytes())
        };
        payloads.push(p);
    }
    for p in &payloads {
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv {
            vfs: &mut vfs,
            sources: &sources,
            resolver: &kit.resolver,
            shred: &mut kit.shred,
            shred_opts: &kit.opts,
        };
        let img;
        let row = match p {
            Some(bytes) => {
                img = img_4b_u(bytes);
                [RawDatum::Bytes(&img)]
            }
            None => [RawDatum::Null],
        };
        w.append_row(&row, &mut kit.ext, &mut env).expect("append");
    }
    {
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv {
            vfs: &mut vfs,
            sources: &sources,
            resolver: &kit.resolver,
            shred: &mut kit.shred,
            shred_opts: &kit.opts,
        };
        w.finish(&mut env).expect("finish");
    }
    // The dict MUST have elected, or this test proves nothing.
    let r = &w.seal_reports()[0];
    let e1 = r.elections.iter().find(|e| e.attno == 1).expect("attno 1");
    assert_eq!(
        e1.encoding,
        EncodingId::DictCodes.as_u16(),
        "corpus failed to dict-elect — the feed path was not exercised"
    );
    // The sealed sidecar payload (a seal byproduct, captured pre-publish).
    let payload = &w.sealed_parts()[0].stats_payload;
    let cols = pgrc_format::sidecar::decode_stats_payload(payload).expect("payload decodes");
    let got = cols
        .iter()
        .find(|(a, p, _)| (*a, *p) == (1, 0))
        .map(|(_, _, d)| d)
        .expect("dict column sketch");
    // The #971 accumulator reference over the same non-null sequence.
    let mut reference = pgrc_meta::sketch::DistAcc::default();
    for p in payloads.iter().flatten() {
        reference.observe(p);
    }
    assert_eq!(
        *got,
        reference.finalize(),
        "feed-served sidecar distribution diverged from the accumulator reference"
    );
}

// ---------------------------------------------------------------------------
// pgrc format 2.2 dictionary layers (spec §6.4/§7): the seal stamps FrontZstd on the
// DictPayload and PackedZstd on the DictIndex, and every reader face expands
// them to the images the v1 writer emits — byte-identical — over synthetic
// dictionaries at the layout's edges.
// ---------------------------------------------------------------------------

fn seal_one_text_column(vfs: &mut MemVfs, values: &[Vec<u8>], fxid: u64) {
    let dict_posture = ColumnPosture {
        dict: Some(DictPolicy {
            ndv_cap: 1_000_000,
            exec_ok: true,
            sem: TextSemantics::Utf8Chars,
        }),
        ..ColumnPosture::default()
    };
    let cands = CodecCandidates::new(ColumnPosture::default()).with_column(1, 0, dict_posture);
    let mut kit = FullKit::new(cands);
    let mut w = open_writer(vec![text_col(1)], stamp(fxid, 1));
    for v in values {
        let img = img_4b_u(v);
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv {
            vfs,
            sources: &sources,
            resolver: &kit.resolver,
            shred: &mut kit.shred,
            shred_opts: &kit.opts,
        };
        w.append_row(&[RawDatum::Bytes(&img)], &mut kit.ext, &mut env).expect("append");
    }
    publish(&mut w, vfs, &mut kit, fxid);
}

#[test]
fn dict_layers_seal_and_expand_byte_identically() {
    use pgrc_format::dict::{utf8_char_count, DictCharLenForm};
    use pgrc_format::geom::OVERSIZE_THRESHOLD;
    // Synthetic dictionaries: an empty entry, non-ASCII, restart (32) and
    // frame (1024) boundaries, a frame of near-identical entries, and an
    // entry just under the oversize threshold.
    let mut cases: Vec<Vec<Vec<u8>>> = Vec::new();
    // (one distinct value elects CONST, never a dictionary — n starts at 2)
    for n in [2usize, 31, 32, 33, 1023, 1024, 1025, 2200] {
        cases.push((0..n).map(|i| format!("https://example.org/Москва/path-{:04}/{i:06}", i % 13).into_bytes()).collect());
    }
    let mut edge: Vec<Vec<u8>> = vec![Vec::new(), b"a".to_vec(), "\u{1F600}".as_bytes().to_vec()];
    edge.push(vec![b'x'; OVERSIZE_THRESHOLD as usize - 64]);
    edge.extend((0..1100u32).map(|i| format!("{}{}", "p".repeat(300), i % 1100).into_bytes()));
    cases.push(edge);
    for (ci, entries) in cases.iter().enumerate() {
        let mut vfs = mem_with_dir();
        // Rows cycle the entries so every entry is observed; order is not
        // sorted (the builder sorts).
        let rows: Vec<Vec<u8>> = (0..8192usize).map(|r| entries[(r * 7 + 3) % entries.len()].clone()).collect();
        seal_one_text_column(&mut vfs, &rows, 300 + ci as u64);
        // The v1 images the old writer would emit for the same distinct set.
        let mut b = crate::dict::DictBuilder::new(TextSemantics::Utf8Chars);
        for e in entries {
            b.observe(e);
        }
        let built = b.build();
        let v1 = built.emit_sections(DictCharLenForm::Absolute).expect("emit");
        let n = built.entry_count();

        let pv = PartView::open(&mut vfs, "part-0.pgrc");
        let (dp, _) = pv.stream(1, 0, StreamRole::DictPayload).expect("payload");
        let (di, _) = pv.stream(1, 0, StreamRole::DictIndex).expect("index");
        let wraps = n >= 64;
        if wraps {
            assert_eq!(dp.wrapper, Wrapper::FrontZstd.as_u8(), "case {ci}: payload arm");
            assert_eq!(di.wrapper, Wrapper::PackedZstd.as_u8(), "case {ci}: index arm");
        }
        assert_eq!(di.flags & 0b11000, 0, "packed index rides the Absolute char form");
        let part = open_part(&mut vfs, "part-0.pgrc");
        let h = pgrc_read::dicthandle::DictHandle::open(Arc::clone(&part), None, &TEST_UNWRAPPERS, 1, 0).expect("handle");
        assert_eq!(h.ncodes(), n);
        h.prewarm_payload().expect("prewarm");
        assert!(h.index_bytes().expect("index") == crate::dict::section_payload(&v1.index_section).expect("v1"), "case {ci}: index image");
        assert!(h.payload_bytes().expect("payload").expect("resident") == crate::dict::section_payload(&v1.payload_section).expect("v1"), "case {ci}: payload image");
        let mut sorted = entries.clone();
        sorted.sort();
        sorted.dedup();
        for (code, e) in sorted.iter().enumerate() {
            let got = h.entry(code as u32).expect("entry");
            assert!(got.bytes == e.as_slice(), "case {ci}: entry {code}");
            assert_eq!(h.lengths(code as u32).expect("lengths"), (e.len() as u32, utf8_char_count(e)), "case {ci}: real UTF-8 char count");
        }
        // The cursor face (KernelCtx.dict through the whole-section unwrap).
        let binding = codec_binding();
        let mut cur = StreamCursor::open(Arc::clone(&part), &binding, 1, 0).expect("cursor");
        let (datums, _arena) = decode_granule(&mut cur, 0);
        let g0 = cur.values_in_granule(0).expect("values") as usize;
        for r in 0..g0 {
            assert_varlena_datum(datums[r], &rows[r]);
        }
        // Old-binary posture: a binding without the layers refuses typed.
        if wraps {
            let part2 = open_part(&mut vfs, "part-0.pgrc");
            static V1_UNWRAPPERS: [&dyn SectionUnwrapper; 2] = [&LZ4_UNWRAP, &ZSTD_UNWRAP];
            let err = match pgrc_read::dicthandle::DictHandle::open(part2, None, &V1_UNWRAPPERS, 1, 0) {
                Ok(_) => panic!("must refuse"),
                Err(e) => e,
            };
            assert!(matches!(err, pgrc_read::ReadError::Format(FormatError::WrapperUnsupported { wrapper: 3 | 4 })), "{err:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// D302: RIG's repro dictionary — 2,100 identical characters plus a number, 800 values — front-codes and compresses past
// 1024:1, which is valid writer output. Both dict arms (block-lazy, whole) read every entry and agree; a header its index
// contradicts refuses in both before any image-sized reservation; and a block fill a failed read or a failed decompress
// cut short is never served.
// ---------------------------------------------------------------------------

/// The repro's distinct `ext` values: `repeat(chr(97 + i % 26), 2100) || i` for every `i % 500 = 0` of 1..=400000.
fn d302_entries() -> Vec<Vec<u8>> {
    (1..=800u32)
        .map(|k| {
            let i = k * 500;
            let c = (b'a' + (i % 26) as u8) as char;
            format!("{}{i}", c.to_string().repeat(2100)).into_bytes()
        })
        .collect()
}

/// A sealed part carrying [`d302_entries`] as a FrontZstd dictionary: the part's bytes and its DictPayload extent's
/// `(file_off, len)`.
fn d302_part() -> (Vec<u8>, (usize, usize)) {
    let entries = d302_entries();
    let mut vfs = mem_with_dir();
    let rows: Vec<Vec<u8>> = (0..8192usize).map(|r| entries[(r * 7 + 3) % entries.len()].clone()).collect();
    seal_one_text_column(&mut vfs, &rows, 302);
    let pv = PartView::open(&mut vfs, "part-0.pgrc");
    let (dp, ext) = pv.stream(1, 0, StreamRole::DictPayload).expect("payload");
    assert_eq!(dp.wrapper, Wrapper::FrontZstd.as_u8(), "the repro's payload arm");
    assert_eq!(ext.len(), 1, "one wrapped extent");
    let span = (ext[0].file_off as usize, ext[0].len as usize);
    (pv.bytes, span)
}

fn d302_open_io(io: Box<dyn pgrc_read::PartIo>) -> Arc<OpenPart> {
    Arc::new(OpenPart::open(io, &PartExpect::none()).expect("open part"))
}

fn d302_open(bytes: &[u8]) -> Arc<OpenPart> {
    d302_open_io(Box::new(MemPartIo::new(bytes.to_vec(), 4242, 7)))
}

/// The block-lazy arm (a sparse consumer's open: the stream's render) or the whole arm (a table build's).
fn d302_handle(
    part: Arc<OpenPart>,
    block_lazy: bool,
    unwrappers: &'static [&'static dyn SectionUnwrapper],
) -> pgrc_read::ReadResult<pgrc_read::dicthandle::DictHandle> {
    use pgrc_read::dicthandle::{DictFaultMode, DictHandle, DictTouch};
    if block_lazy {
        DictHandle::open_touch(part, None, unwrappers, 1, 0, DictTouch::Sparse)
    } else {
        DictHandle::open_with_mode(part, None, unwrappers, 1, 0, DictFaultMode::WholeFault)
    }
}

fn d302_assert_every_entry(h: &pgrc_read::dicthandle::DictHandle, what: &str) {
    let mut want = d302_entries();
    want.sort();
    assert_eq!(h.ncodes() as usize, want.len(), "{what}");
    for (code, e) in want.iter().enumerate() {
        let got = h.entry(code as u32).unwrap_or_else(|err| panic!("{what}: entry {code}: {err:?}"));
        assert!(got.bytes == e.as_slice(), "{what}: entry {code}");
    }
}

#[test]
fn d302_a_front_coded_dict_past_any_ratio_reads_through_both_arms() {
    let (bytes, (off, len)) = d302_part();
    let hdr = StreamSectionHdr::decode(&bytes[off..off + len]).expect("hdr");
    let comp = hdr.frame_table_off as usize - pgrc_format::part::STREAM_SECTION_HDR_LEN;
    assert!(hdr.uncompressed_len as usize > 1024 * comp, "past the old 1024x ratio: {} over {comp}", hdr.uncompressed_len);
    for block_lazy in [true, false] {
        let h = d302_handle(d302_open(&bytes), block_lazy, &TEST_UNWRAPPERS).expect("handle");
        d302_assert_every_entry(&h, if block_lazy { "block-lazy" } else { "whole" });
        assert_eq!(h.frame_lazy_frames() > 0, block_lazy, "the arm under test");
    }
}

/// Title-like distinct entries (words from one vocabulary): the OnPair dict payload arm's case.
fn onpair_entries() -> Vec<Vec<u8>> {
    let mut st = 0x0a1u64;
    let mut next = || {
        st = st.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let z = (st ^ (st >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB) ^ (z >> 31)
    };
    let vocab: Vec<String> = (0..3000).map(|_| (0..3 + next() % 8).map(|_| char::from_u32(0x430 + (next() % 32) as u32).unwrap()).collect()).collect();
    let mut v: Vec<Vec<u8>> = (0..20_000).map(|_| (0..4 + next() % 8).map(|_| vocab[(next() % 3000) as usize].as_str()).collect::<Vec<_>>().join(" ").into_bytes()).collect();
    v.sort();
    v.dedup();
    v
}

#[test]
fn onpair_dict_payload_reads_every_entry_through_both_arms() {
    if !pgrc_codec::onpairwrap::onpair_on() {
        return;
    }
    let entries = onpair_entries();
    let mut vfs = mem_with_dir();
    let rows: Vec<Vec<u8>> = (0..3 * entries.len()).map(|r| entries[(r * 7919) % entries.len()].clone()).collect();
    seal_one_text_column(&mut vfs, &rows, 303);
    let pv = PartView::open(&mut vfs, "part-0.pgrc");
    let (dp, _) = pv.stream(1, 0, StreamRole::DictPayload).expect("payload");
    assert_eq!(dp.wrapper, Wrapper::FrontOnPairZstd.as_u8(), "the payload arm the election keeps");
    for block_lazy in [true, false] {
        let what = if block_lazy { "block-lazy" } else { "whole" };
        let h = d302_handle(d302_open(&pv.bytes), block_lazy, &TEST_UNWRAPPERS).expect("handle");
        assert_eq!(h.ncodes() as usize, entries.len(), "{what}");
        for (code, e) in entries.iter().enumerate().rev() {
            let got = h.entry(code as u32).unwrap_or_else(|err| panic!("{what}: entry {code}: {err:?}"));
            assert!(got.bytes == e.as_slice(), "{what}: entry {code}");
        }
        assert_eq!(h.frame_lazy_frames() > 0, block_lazy, "{what}: the arm under test");
    }
    static V1_UNWRAPPERS: [&dyn SectionUnwrapper; 4] = [&LZ4_UNWRAP, &ZSTD_UNWRAP, &FRONT_UNWRAP, &PACKED_UNWRAP];
    let err = match pgrc_read::dicthandle::DictHandle::open(d302_open(&pv.bytes), None, &V1_UNWRAPPERS, 1, 0) {
        Ok(_) => panic!("an older binding must refuse"),
        Err(e) => e,
    };
    assert!(matches!(err, pgrc_read::ReadError::Format(FormatError::WrapperUnsupported { wrapper: 5 })), "{err:?}");
}

/// Distinct title-like rows demote the dictionary to verbatim values: under `PGRUST_PGRC_ONPAIR_VERBATIM=1` the
/// stream is the OnPair verbatim arm and the cursor reads every row; a binding without the arm refuses typed.
#[test]
fn onpair_verbatim_values_read_back_through_the_cursor() {
    if !pgrc_codec::onpairwrap::onpair_verbatim_on() {
        return;
    }
    let entries = onpair_entries();
    let mut vfs = mem_with_dir();
    let rows: Vec<Vec<u8>> = (0..entries.len()).map(|r| entries[(r * 7919) % entries.len()].clone()).collect();
    seal_one_text_column(&mut vfs, &rows, 304);
    let pv = PartView::open(&mut vfs, "part-0.pgrc");
    let (vs, _) = pv.stream(1, 0, StreamRole::Values).expect("values");
    assert_eq!(vs.wrapper, Wrapper::OnPairZstd.as_u8(), "the values arm the election keeps");
    let part = open_part(&mut vfs, "part-0.pgrc");
    let binding = codec_binding();
    let mut cur = StreamCursor::open(Arc::clone(&part), &binding, 1, 0).expect("cursor");
    let mut r0 = 0usize;
    for g in 0..cur.granule_count() {
        let (datums, _arena) = decode_granule(&mut cur, g);
        for (i, d) in datums.iter().enumerate() {
            assert_varlena_datum(*d, &rows[r0 + i]);
        }
        r0 += datums.len();
    }
    assert_eq!(r0, rows.len());
    let old = CodecBinding { registry: pgrc_codec::registry(), unwrappers: &[&LZ4_UNWRAP, &ZSTD_UNWRAP] };
    let refused = StreamCursor::open(part, &old, 1, 0).and_then(|mut c| c.values_in_granule(0).map(drop));
    assert!(matches!(refused, Err(pgrc_read::ReadError::Format(FormatError::WrapperUnsupported { wrapper: 6 }))), "{:?}", refused.err());
}

#[test]
fn d302_a_header_its_index_contradicts_refuses_in_both_arms() {
    let (bytes, (off, len)) = d302_part();
    let hdr = StreamSectionHdr::decode(&bytes[off..off + len]).expect("hdr");
    let index = {
        let h = d302_handle(d302_open(&bytes), false, &TEST_UNWRAPPERS).expect("handle");
        h.index_bytes().expect("index").to_vec()
    };
    let law = "dict payload uncompressed_len vs dict index";
    for bad in [u32::MAX - 7, hdr.uncompressed_len + 8, hdr.uncompressed_len - 1] {
        let mut b = bytes.clone();
        // StreamSectionHdr: uncompressed_len is the u32 at byte 20
        b[off + 20..off + 24].copy_from_slice(&bad.to_le_bytes());
        // block-lazy: the head arrives by an unchecked sub-extent pread, so the law is its witness
        let h = d302_handle(d302_open(&b), true, &TEST_UNWRAPPERS).expect("handle");
        let err = h.entry(0).map(|_| ()).expect_err("block-lazy refuses");
        assert!(matches!(err, pgrc_read::ReadError::Format(FormatError::Corrupt { at }) if at == law), "block-lazy {bad}: {err:?}");
        // whole: the extent is CRC-checked before its unwrap, so the law is witnessed at the unwrap itself (the
        // reservation is after it: a u32::MAX - 7 claim that reached it would zero-fill 4 GiB here)
        let mut out = Vec::new();
        let err = pgrc_codec::wrapper::unwrap_dict_payload_section(&b[off..off + len], &index, &mut out).expect_err("whole refuses");
        assert!(matches!(err, FormatError::Corrupt { at } if at == law), "whole {bad}: {err:?}");
        assert!(out.capacity() < 1 << 20, "no image-sized reservation before the refusal");
    }
}

/// A part io whose one pread at `fail_off` fails once, after scribbling over the buffer (a read cut off mid-fill).
struct D302FlakyIo {
    inner: MemPartIo,
    fail_off: u64,
    tripped: std::sync::atomic::AtomicBool,
}

impl pgrc_read::PartIo for D302FlakyIo {
    fn len(&self) -> u64 {
        pgrc_read::PartIo::len(&self.inner)
    }
    fn dev_ino(&self) -> (u64, u64) {
        pgrc_read::PartIo::dev_ino(&self.inner)
    }
    fn pread_exact(&self, off: u64, buf: &mut [u8], at: &'static str) -> pgrc_read::ReadResult<()> {
        if off == self.fail_off && !self.tripped.swap(true, std::sync::atomic::Ordering::SeqCst) {
            let half = buf.len() / 2;
            buf[..half].fill(0xAB);
            return Err(pgrc_read::ReadError::Io { at, errno: 4 });
        }
        pgrc_read::PartIo::pread_exact(&self.inner, off, buf, at)
    }
}

/// The FrontZstd unwrapper whose first block decompress stops part-way: half the block's span scribbled, then an error.
struct D302FlakyFront;
static D302_FLAKY_TRIPPED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
impl SectionUnwrapper for D302FlakyFront {
    fn wrapper(&self) -> Wrapper {
        Wrapper::FrontZstd
    }
    fn unwrap_section(&self, hdr: &StreamSectionHdr, section: &[u8]) -> FormatResult<Vec<u8>> {
        FRONT_UNWRAP.unwrap_section(hdr, section)
    }
    fn unwrap_dict_payload_section(&self, hdr: &StreamSectionHdr, section: &[u8], index: &[u8]) -> FormatResult<Vec<u8>> {
        FRONT_UNWRAP.unwrap_dict_payload_section(hdr, section, index)
    }
    fn unwrap_dict_payload_block(&self, src: &[u8], dst: &mut [u8], records: &[u8]) -> FormatResult<bool> {
        if !D302_FLAKY_TRIPPED.swap(true, std::sync::atomic::Ordering::SeqCst) {
            let half = dst.len() / 2;
            dst[..half].fill(0xAB);
            return Err(FormatError::Corrupt { at: "d302 test: a block decompress cut short" });
        }
        FRONT_UNWRAP.unwrap_dict_payload_block(src, dst, records)
    }
}
static D302_FLAKY_FRONT: D302FlakyFront = D302FlakyFront;
static D302_FLAKY_UNWRAPPERS: [&dyn SectionUnwrapper; 4] = [&LZ4_UNWRAP, &ZSTD_UNWRAP, &D302_FLAKY_FRONT, &PACKED_UNWRAP];

// ---------------------------------------------------------------------------
// D4084: a block-lazy dictionary image's reservation on the memory ledger
// ---------------------------------------------------------------------------

/// D4084's dictionary: 8192 distinct 128-byte entries (eight 1024-entry blocks).
fn d4084_entries() -> Vec<Vec<u8>> {
    (0..8192u32).map(|i| format!("d4084-{i:06}-{}", "x".repeat(115)).into_bytes()).collect()
}

/// A sealed part carrying [`d4084_entries`] (each on four rows) as a FrontZstd dictionary: its bytes and its payload's
/// uncompressed length.
fn d4084_part() -> (Vec<u8>, usize) {
    let entries = d4084_entries();
    let mut vfs = mem_with_dir();
    let rows: Vec<Vec<u8>> = (0..4 * entries.len()).map(|r| entries[(r * 7 + 3) % entries.len()].clone()).collect();
    seal_one_text_column(&mut vfs, &rows, 4084);
    let pv = PartView::open(&mut vfs, "part-0.pgrc");
    let (dp, ext) = pv.stream(1, 0, StreamRole::DictPayload).expect("payload");
    assert_eq!(dp.wrapper, Wrapper::FrontZstd.as_u8(), "a wrapped payload");
    assert_eq!(ext.len(), 1, "one wrapped extent");
    let (off, len) = (ext[0].file_off as usize, ext[0].len as usize);
    let unc = StreamSectionHdr::decode(&pv.bytes[off..off + len]).expect("hdr").uncompressed_len as usize;
    (pv.bytes, unc)
}

/// D4084 (born red): a block-lazy dictionary image's reservation is allocated and freed on the part cache's memory ledger
/// account and its faulted blocks are counted in the cache's census with no ledger move, so the backend that opens the
/// image and faults one block nets zero over it, and so does another backend whose trim frees it. Before, door 1
/// charged the whole reservation to the opener and a block fault moved only its own bytes on to the cache's account: the
/// opener kept the unfaulted rest (on 'unbound' once its session ended) and the evicting backend went that much
/// negative. Door 1 counts this test binary's allocations (`tests::LedgerDoor`).
#[test]
fn d4084_a_block_lazy_image_nets_zero_on_its_opener_and_its_evictor() {
    let (bytes, unc) = d4084_part();
    let part = d302_open(&bytes);
    part.stream_directory().expect("the directory, parsed before the backends under test");
    // as backend `procno`, on a thread of its own: `f`'s result and the account's change over it, door 1 folded in
    let as_backend = |procno: u32, f: &(dyn Fn() -> u64 + Sync)| -> (u64, i64) {
        std::thread::scope(|s| {
            s.spawn(|| {
                memledger::bind_backend(procno, 90_000 + procno);
                let acct = memledger::Acct::backend(procno);
                memledger::fold();
                let before = memledger::account_row(acct).account_bytes;
                let r = f();
                memledger::fold();
                let moved = memledger::account_row(acct).account_bytes - before;
                memledger::unbind_backend();
                (r, moved)
            })
            .join()
            .expect("the backend's thread")
        })
    };
    let (faulted, opener) = as_backend(3084, &|| {
        let h = d302_handle(Arc::clone(&part), true, &TEST_UNWRAPPERS).expect("handle");
        assert_eq!(h.entry(0).expect("entry 0").bytes.len(), 128, "the first entry");
        assert!(h.frame_lazy_frames() > 1, "the block-lazy arm, several blocks");
        h.resident_payload_bytes()
    });
    assert!(faulted > 0 && faulted * 4 < unc as u64, "one block of the {unc}-byte image faulted, not the image: {faulted}");
    let (evicted, evictor) = as_backend(3085, &|| part.evict_contents().1);
    assert_eq!(evicted, faulted, "the trim freed the image");
    let slack = 64 << 10;
    let rest = unc as u64 - faulted;
    assert!(opener.abs() < slack, "the opener's account moved {opener} bytes over open + fault + drop (the unfaulted rest: {rest})");
    assert!(evictor.abs() < slack, "the evicting backend's account moved {evictor} bytes freeing the image (the unfaulted rest: {rest})");
}

#[test]
fn d302_an_interrupted_fill_is_never_trusted() {
    let (bytes, (off, _)) = d302_part();
    let hdr_len = pgrc_format::part::STREAM_SECTION_HDR_LEN;
    // the block-offset table heads the wrapped payload; its first word is its own length, block 0's start
    let block0 = u32::from_le_bytes(bytes[off + hdr_len..off + hdr_len + 4].try_into().expect("len 4")) as usize;
    // block-lazy: block 0's sub-extent pread fails mid-fill; whole: the extent's read does
    for (block_lazy, fail_off) in [(true, off + hdr_len + block0), (false, off)] {
        let what = if block_lazy { "block-lazy" } else { "whole" };
        let io = D302FlakyIo { inner: MemPartIo::new(bytes.clone(), 4242, 7), fail_off: fail_off as u64, tripped: false.into() };
        let h = d302_handle(d302_open_io(Box::new(io)), block_lazy, &TEST_UNWRAPPERS).expect("handle");
        let err = h.entry(0).map(|_| ()).expect_err("the cut-off read surfaces");
        assert!(matches!(err, pgrc_read::ReadError::Io { errno: 4, .. }), "{what}: {err:?}");
        d302_assert_every_entry(&h, what);
    }
    // block-lazy: block 0's decompress stops part-way through its span
    let h = d302_handle(d302_open(&bytes), true, &D302_FLAKY_UNWRAPPERS).expect("handle");
    let err = h.entry(0).map(|_| ()).expect_err("the cut-off decompress surfaces");
    assert!(matches!(err, pgrc_read::ReadError::Format(FormatError::Corrupt { at: "d302 test: a block decompress cut short" })), "{err:?}");
    assert_eq!(h.frame_lazy_frames(), 1, "the block-lazy arm");
    d302_assert_every_entry(&h, "block-lazy after a cut-off decompress");
}

// ---------------------------------------------------------------------------
// D3038: the octet-length lane
// ---------------------------------------------------------------------------

/// A verbatim text column and a bytea column carry their octet-length lanes (one int4 word per row, the payload's byte
/// length, an overflowed value's whole length, NULL where the column is), equal to the reader's `decode_lengths` of the
/// column; a column sealed as dictionary codes carries none, and the PathTable lists only the lanes sealed, so the
/// verbatim columns' lanes take path_ords 1 and 2.
#[test]
fn octet_lane_rides_verbatim_text_not_dict_codes() {
    use pgrc_format::abi::ValidityVerdict;
    use pgrc_format::class::CLASS_BYVAL;
    use pgrc_format::shredlane::OCTET_LENGTH_PATH;
    let mut vfs = mem_with_dir();
    let dict = ColumnPosture {
        dict: Some(DictPolicy { ndv_cap: 100_000, exec_ok: true, sem: TextSemantics::BytesOnly }),
        ..ColumnPosture::default()
    };
    let mut kit = FullKit::new(CodecCandidates::new(ColumnPosture::default()).with_column(1, 0, dict));
    let bytea = ColSchema { semantics: TypeSemantics::MemcmpOrdered, ..text_col(4) };
    let mut w = open_writer(vec![text_col(1), int8_col(2), text_col(3), bytea], stamp(94, 1));
    let rows = 10_000u64;
    let coded = |i: u64| format!("v{:04}", i % 211);
    let verbatim = |i: u64| {
        (i % 13 != 0).then(|| match i % 5 {
            0 => String::new(),
            1 => format!("{}-{i}", "\u{e9}".repeat((i % 17) as usize)),
            _ => format!("{}-{i}", "x".repeat((i % 29) as usize)),
        })
    };
    // one value past the 32 KiB inline ceiling (the overflow stream)
    let bytes = |i: u64| (i % 7 != 2).then(|| if i == 4321 { vec![0xA5; 40_000] } else { i.to_le_bytes().repeat((i % 5) as usize) });
    for i in 0..rows {
        let a = img_4b_u(coded(i).as_bytes());
        let c = verbatim(i).map(|s| img_4b_u(s.as_bytes()));
        let e = bytes(i).map(|b| img_4b_u(&b));
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv {
            vfs: &mut vfs,
            sources: &sources,
            resolver: &kit.resolver,
            shred: &mut kit.shred,
            shred_opts: &kit.opts,
        };
        let cd = c.as_deref().map_or(RawDatum::Null, RawDatum::Bytes);
        let ed = e.as_deref().map_or(RawDatum::Null, RawDatum::Bytes);
        w.append_row(&[RawDatum::Bytes(&a), RawDatum::Word(i), cd, ed], &mut kit.ext, &mut env)
            .expect("append");
    }
    publish(&mut w, &mut vfs, &mut kit, 94);

    let part = open_part(&mut vfs, "part-0.pgrc");
    assert_eq!(pgrc_read::read_path_table(&part).expect("path table"), Some(vec![OCTET_LENGTH_PATH.to_string(); 2]));
    let binding = codec_binding();
    let codes = StreamCursor::open(Arc::clone(&part), &binding, 1, 0).expect("coded column");
    assert_eq!(codes.entry().encoding, EncodingId::DictCodes.as_u16());
    for ord in 1..3 {
        assert!(StreamCursor::open(Arc::clone(&part), &binding, 1, ord).is_err(), "no lane beside dictionary codes");
    }
    let check = |attno: u32, ord: u32, want: &dyn Fn(u64) -> Option<usize>| {
        let mut lane = StreamCursor::open(Arc::clone(&part), &binding, attno, ord).expect("octet lane");
        let mut col = StreamCursor::open(Arc::clone(&part), &binding, attno, 0).expect("column");
        assert_eq!(lane.entry().class, CLASS_BYVAL);
        let mut row = 0u64;
        for g in 0..lane.granule_count() {
            let n = lane.rows_in_granule(g) as usize;
            let mut lens = vec![0i32; n];
            assert_eq!(lane.decode_i32(g, &mut lens).expect("decode_i32") as usize, n);
            // the reader's length walk where the column's encoding answers it (verbatim)
            let mut read = vec![0u32; n];
            let walked = col.decode_lengths(g, &mut read).is_ok();
            let mut mask = vec![0u64; n.div_ceil(64).max(1)];
            let all = matches!(lane.validity(g, &mut mask).expect("validity"), ValidityVerdict::AllValid);
            for (r, len) in lens.iter().enumerate() {
                let valid = all || (mask[r / 64] >> (r % 64)) & 1 == 1;
                match want(row) {
                    Some(l) => {
                        assert!(valid, "att {attno} row {row} is not NULL");
                        assert_eq!(*len as usize, l, "att {attno} row {row}");
                        assert!(!walked || read[r] as usize == l, "att {attno} row {row}: the column's decode_lengths {}", read[r]);
                    }
                    None => assert!(!valid, "att {attno} row {row} is NULL"),
                }
                row += 1;
            }
        }
        assert_eq!(row, rows);
    };
    check(3, 1, &|i| verbatim(i).map(|s| s.len()));
    check(4, 2, &|i| bytes(i).map(|b| b.len()));
}

/// `PGRUST_PGRC_INTPRE` on (forced on this thread): a second-granular timestamp-like int8 stream seals under the integer
/// pre-pass wrapper and reads back exactly; a full-entropy stream settles to Zstd and then fails the 20 % law (unwrapped);
/// a binding without the IntPre unwrapper refuses typed at open (the older-binary posture); the band images rebuild
/// through `unwrap_kept` against the writer's CRC; two runs write identical bytes.
#[test]
fn intpre_wraps_int_streams_reads_back_and_refuses_without_unwrapper() {
    let rows = 30_000u64;
    let data = |i: u64, seed: &mut u64, ts: &mut u64| {
        let r = splitmix(seed);
        *ts = if i % 6 == 0 { 1_372_708_800_000_000 + (r % 2_678_400) * 1_000_000 } else { *ts + (r % 120) * 1_000_000 };
        (*ts, splitmix(seed), i / 37)
    };
    let run = || -> MemVfs {
        pgrc_codec::intpre::force(Some(true));
        let mut vfs = mem_with_dir();
        let mut kit = FullKit::new(CodecCandidates::new(ColumnPosture::default()));
        let mut w = open_writer(vec![int8_col(1), int8_col(2), int8_col(3)], stamp(96, 1));
        let (mut seed, mut ts) = (0x1D7E_u64, 0u64);
        for i in 0..rows {
            let (a, b, c) = data(i, &mut seed, &mut ts);
            let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
            let mut env = SealEnv { vfs: &mut vfs, sources: &sources, resolver: &kit.resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
            w.append_row(&[RawDatum::Word(a), RawDatum::Word(b), RawDatum::Word(c)], &mut kit.ext, &mut env).expect("append");
        }
        publish(&mut w, &mut vfs, &mut kit, 96);
        pgrc_codec::intpre::force(None);
        vfs
    };
    let mut vfs = run();
    let pv = PartView::open(&mut vfs, "part-0.pgrc");
    let (e1, x1) = pv.stream(1, 0, StreamRole::Values).expect("col1");
    assert_eq!(e1.encoding, EncodingId::ByteFor.as_u16());
    assert_eq!(e1.wrapper, Wrapper::IntPre.as_u8(), "the timestamp-like stream takes the pre-pass");
    let (e2, _) = pv.stream(2, 0, StreamRole::Values).expect("col2");
    assert_eq!(e2.wrapper, Wrapper::None.as_u8(), "noise settles to Zstd and fails the 20 % law");
    let (e3, _) = pv.stream(3, 0, StreamRole::Values).expect("col3");
    assert!(e3.wrapper == Wrapper::IntPre.as_u8() || e3.wrapper == Wrapper::Zstd.as_u8());

    let part = open_part(&mut vfs, "part-0.pgrc");
    let binding = codec_binding();
    let (mut seed, mut ts) = (0x1D7E_u64, 0u64);
    let want: Vec<(u64, u64, u64)> = (0..rows).map(|i| data(i, &mut seed, &mut ts)).collect();
    for (attno, pick) in [(1u32, 0usize), (2, 1), (3, 2)] {
        let mut cur = StreamCursor::open(Arc::clone(&part), &binding, attno, 0).expect("cursor");
        let mut row = 0usize;
        for g in 0..cur.granule_count() {
            let (datums, _arena) = decode_granule(&mut cur, g);
            for d in datums {
                let w = want[row];
                assert_eq!(d, [w.0, w.1, w.2][pick], "att {attno} row {row}");
                row += 1;
            }
        }
        assert_eq!(row as u64, rows);
    }

    let old = CodecBinding { registry: pgrc_codec::registry(), unwrappers: &[&LZ4_UNWRAP, &ZSTD_UNWRAP] };
    let err = StreamCursor::open(open_part(&mut vfs, "part-0.pgrc"), &old, 1, 0).expect_err("binding without IntPre");
    assert!(
        matches!(err, pgrc_read::ReadError::Format(FormatError::WrapperUnsupported { wrapper: 8 })),
        "an older binding must refuse id 8 typed, got {err:?}"
    );

    for x in &x1 {
        let image = &pv.bytes[x.file_off as usize..(x.file_off + x.len) as usize];
        let mut plain = Vec::new();
        pgrc_codec::wrapper::unwrap_section(image, &mut plain).expect("unwrap");
        let crc = pgrc_format::wire::crc32c(&plain);
        assert_eq!(crate::seal::unwrap_kept(image, crc).expect("unwrap_kept"), plain);
        assert!(crate::seal::unwrap_kept(image, crc ^ 1).is_err(), "a CRC the rebuild misses refuses");
    }

    let mut again = run();
    assert_eq!(
        vfs.read_full(&format!("{DIR}/part-0.pgrc")).expect("a"),
        again.read_full(&format!("{DIR}/part-0.pgrc")).expect("b"),
        "pre-passed part bytes diverged across identical runs"
    );
}
