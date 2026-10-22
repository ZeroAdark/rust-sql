//! Reseal one text column of a REAL part under another dictionary NDV cap (an
//! ignored tool, not a gate): the source part's values are decoded through
//! `pgrc_read`, sealed as a one-column part with `DictPolicy { ndv_cap }`,
//! read back, compared value for value, a top-10 by frequency folded on both,
//! and the bytes per stream role printed.
//!
//! `PGRC_RESEAL_PART=<part file> PGRC_RESEAL_ATTNO=14 PGRC_RESEAL_CAP=256000 \
//!  cargo test -p pgrc_write reseal -- --ignored --nocapture`

use std::collections::HashMap;
use std::ffi::CString;
use std::sync::Arc;

use pgrc_format::abi::{datum_canonical_bytes, ByteArena, DecodeOut};
use pgrc_format::class::StorageClass;
use pgrc_format::enc::Wrapper;
use pgrc_format::part::StreamSectionHdr;
use pgrc_format::FormatResult;
use pgrc_read::io::MemPartIo;
use pgrc_read::{CodecBinding, OpenPart, PartExpect, SectionUnwrapper, StreamCursor, VfsPartIo};

use crate::dict::TextSemantics;
use crate::elect::{CodecCandidates, ColumnPosture, DictPolicy};
use crate::ingest::RawDatum;
use crate::publish::TxnVerdict;
use crate::seal::CodecResolver;
use crate::testkit::{img_4b_u, mem_with_dir, open_writer_policy, stamp, text_col, Probe, DIR};
use crate::writer::{PartCutPolicy, SealEnv};
use crate::wvfs::{MemVfs, WriteVfs};

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
static ZSTD_UNWRAP: ZstdUnwrap = ZstdUnwrap(Wrapper::Zstd);
static ONPAIR_VALUES_UNWRAP: ZstdUnwrap = ZstdUnwrap(Wrapper::OnPairZstd);
static LZ4_UNWRAP: Lz4Unwrap = Lz4Unwrap;

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
fn binding() -> CodecBinding<'static> {
    static UNWRAPPERS: [&dyn SectionUnwrapper; 6] = [&LZ4_UNWRAP, &ZSTD_UNWRAP, &FRONT_UNWRAP, &PACKED_UNWRAP, &FRONT_ONPAIR_UNWRAP, &ONPAIR_VALUES_UNWRAP];
    CodecBinding { registry: pgrc_codec::registry(), unwrappers: &UNWRAPPERS }
}

/// Every value of `attno` in `part`, in row order (the column must be null-free).
fn column_values(part: &Arc<OpenPart>, attno: u32) -> Vec<Vec<u8>> {
    let b = binding();
    let mut cur = StreamCursor::open(Arc::clone(part), &b, attno, 0).expect("cursor");
    assert!(!cur.has_validity_stream(), "reseal expects a null-free column");
    let mut out = Vec::new();
    let mut arena_buf = vec![0u8; 16 << 20];
    for g in 0..cur.granule_count() {
        let n = cur.values_in_granule(g).expect("values") as usize;
        let mut datums = vec![0u64; n];
        let wrote = cur.decode_full(g, &mut DecodeOut { datums: &mut datums, arena: ByteArena::new(&mut arena_buf) }).expect("decode_full");
        assert_eq!(wrote as usize, n);
        for &d in &datums {
            let mut scratch = [0u8; 8];
            let bytes = unsafe { datum_canonical_bytes(StorageClass::VarlenaVerbatim, d, &mut scratch) }.expect("varlena");
            out.push(bytes.to_vec());
        }
    }
    out
}

/// Bytes on disk per stream role of `attno` and the values entry's encoding / wrapper / ncodes.
fn stream_bytes(part: &OpenPart, attno: u32) -> Vec<(String, u64, u16, u8, u64)> {
    let dir = part.stream_directory().expect("dir");
    dir.streams()
        .iter()
        .filter(|ps| ps.entry.attno == attno)
        .map(|ps| (format!("{:?}", ps.role), ps.extents.iter().map(|r| r.len).sum(), ps.entry.encoding, ps.entry.wrapper, ps.entry.values))
        .collect()
}

/// A top-10 by frequency over one part: the ten most frequent values, ties by value.
fn top10(values: &[Vec<u8>]) -> Vec<(u64, Vec<u8>)> {
    let mut counts: HashMap<&[u8], u64> = HashMap::new();
    for v in values {
        *counts.entry(v.as_slice()).or_insert(0) += 1;
    }
    let mut v: Vec<(u64, Vec<u8>)> = counts.into_iter().map(|(k, c)| (c, k.to_vec())).collect();
    v.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    v.truncate(10);
    v
}

#[test]
#[ignore]
fn reseal_one_column_under_a_dictionary_cap() {
    let Ok(path) = std::env::var("PGRC_RESEAL_PART") else { return };
    let attno: u32 = std::env::var("PGRC_RESEAL_ATTNO").ok().and_then(|v| v.parse().ok()).unwrap_or(14);
    let cap: u64 = std::env::var("PGRC_RESEAL_CAP").ok().and_then(|v| v.parse().ok()).unwrap_or(256_000);
    let cpath = CString::new(path.clone()).expect("path");
    let src = Arc::new(OpenPart::open(Box::new(VfsPartIo::open(&cpath).expect("open")), &PartExpect::none()).expect("part"));
    let values = column_values(&src, attno);
    let distinct = values.iter().collect::<std::collections::HashSet<_>>().len();
    println!("source {path} attno={attno} rows={} distinct={distinct} streams={:?}", values.len(), stream_bytes(&src, attno));

    let mut vfs: MemVfs = mem_with_dir();
    let cands = CodecCandidates::new(ColumnPosture { dict: Some(DictPolicy { ndv_cap: cap, exec_ok: true, sem: TextSemantics::BytesOnly }), ..ColumnPosture::default() });
    let resolver = CodecResolver;
    let mut shred = crate::shred::NoShred;
    let opts = pgrc_format::relopt::ShredOptions::default();
    let mut ext = crate::ingest::NoExternalDetoast;
    let mut w = open_writer_policy(vec![text_col(1)], stamp(93, 1), PartCutPolicy { max_rows: 1 << 21, max_bytes: u64::MAX, ..PartCutPolicy::default() });
    let t0 = std::time::Instant::now();
    for v in &values {
        let img = img_4b_u(v);
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&cands];
        let mut env = SealEnv { vfs: &mut vfs, sources: &sources, resolver: &resolver, shred: &mut shred, shred_opts: &opts };
        w.append_row(&[RawDatum::Bytes(&img)], &mut ext, &mut env).expect("append");
    }
    {
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&cands];
        let mut env = SealEnv { vfs: &mut vfs, sources: &sources, resolver: &resolver, shred: &mut shred, shred_opts: &opts };
        w.finish(&mut env).expect("finish");
    }
    let probe = Probe::new(TxnVerdict::InProgress).set(93, TxnVerdict::Committed);
    w.publish(&mut vfs, &probe).expect("publish");
    let sealed = t0.elapsed();

    let bytes = vfs.read_full(&format!("{DIR}/part-0.pgrc")).expect("part bytes");
    let file_len = bytes.len();
    let new = Arc::new(OpenPart::open(Box::new(MemPartIo::new(bytes, 4242, 7)), &PartExpect::none()).expect("open new"));
    let back = column_values(&new, 1);
    assert_eq!(back.len(), values.len(), "row count");
    let mismatches = back.iter().zip(&values).filter(|(a, b)| a != b).count();
    assert_eq!(mismatches, 0, "values read back byte for byte");
    let (t_src, t_new) = (top10(&values), top10(&back));
    assert_eq!(t_src, t_new, "the top-10-by-frequency folds equal");
    println!("resealed cap={cap} rows={} file={file_len}B seal={:?} streams={:?}", back.len(), sealed, stream_bytes(&new, 1));
    for (c, v) in &t_new {
        println!("  {c} {}", String::from_utf8_lossy(v));
    }
}
