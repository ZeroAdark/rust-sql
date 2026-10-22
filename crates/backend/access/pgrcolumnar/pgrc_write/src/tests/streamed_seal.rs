//! K1-MEM: a streamed seal (`TableWriter::seal_streamed`: columns fed one at a time, each stream written through as
//! it seals) writes the whole-part seal's bytes for the same rows (`append_chunk` then `finish`, the path a
//! compaction's `SealWriter` took): unsorted and `sorted_by`, serial and pooled, in one batch or several, with NULLs,
//! dict-coded and demoted text, overflow values, several bands and shredded jsonb lanes. A part that needs a grain
//! finer than the default is handed back with nothing left behind; a feed out of key order is refused.

use super::*;
use crate::dict::TextSemantics;
use crate::elect::{CandidateSource, CodecCandidates, ColumnPosture, DictPolicy};
use crate::ingest::ColBuffer;
use crate::par::RowChunk;
use crate::seal::{CodecResolver, ColumnPool, SealReport, SealedPart, SerialPool, VerifyResolver};
use crate::shred_jsonb::JsonbShredSource;
use crate::structural::{ClusterKeyDecl, NullsOrder, SortDir, StructuralPolicy};
use crate::walmodel::{RecordingSink, WalRecord};
use crate::writer::SealPool;
use crate::WriteError;
use pgsync::lock;
use std::cell::RefCell;
use std::sync::Arc;

const FXID: u64 = 91;
/// `SealWriter`'s chunk (eight default granules).
const CHUNK: u32 = 65_536;

enum Val {
    Null,
    Word(u64),
    Bytes(Vec<u8>),
}

impl Val {
    fn raw(&self) -> RawDatum<'_> {
        match self {
            Val::Null => RawDatum::Null,
            Val::Word(w) => RawDatum::Word(*w),
            Val::Bytes(b) => RawDatum::Bytes(b),
        }
    }
}

fn text(s: &str) -> Val {
    Val::Bytes(img_4b_u(s.as_bytes()))
}

fn h(i: u64) -> u64 {
    let x = i.wrapping_add(1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^ (x >> 29)
}

struct Table {
    schema: Vec<ColSchema>,
    key: Option<ClusterKeyDecl>,
    rows: u64,
    cell: Box<dyn Fn(u64, usize) -> Val>,
    /// Registered jsonb columns (attnos): the shred source.
    jsonb: Vec<u32>,
    /// D3038: no column takes an octet-length lane (the shred source owns every column as a document, `covers`): the
    /// seal as it was before the lane.
    no_octet: bool,
}

/// Ints, dict-coded low-cardinality text with NULLs, spread ints, 64-hex high-cardinality text (the dict arm
/// demotes), mid-cardinality text with NULLs and a few overflow values, and ints with NULLs; nine granules, two bands.
/// `sorted`: column 3 is a nondecreasing key with ties, NULLs last, declared `sorted_by`.
fn mixed(rows: u64, sorted: bool) -> Table {
    let schema = vec![int8_col(1), text_col(2), int8_col(3), text_col(4), text_col(5), int8_col(6)];
    let cell = Box::new(move |i: u64, c: usize| match c {
        0 if i % 31 == 0 => Val::Null,
        0 => Val::Word((i as i64 * 7 - 1000) as u64),
        1 if i % 13 == 0 => Val::Null,
        1 => text(&format!("cat{}", i % 40)),
        2 if sorted && i + 40 >= rows => Val::Null,
        2 if sorted => Val::Word(i / 3),
        2 => Val::Word(h(i) % 1_000_003),
        3 => text(&format!("{:016x}{:016x}{:016x}{:016x}", h(i), h(i + 1), h(i + 2), h(i + 3))),
        4 if i % 7 == 3 => Val::Null,
        4 if i % 20_000 == 11 => text(&"x".repeat(40_000 + i as usize % 97)),
        4 => text(&format!("w{}-{}", i % 1000, i % 7)),
        _ if i % 29 == 0 => Val::Null,
        _ => Val::Word(i),
    });
    let key = sorted.then_some(ClusterKeyDecl { attno: 3, dir: SortDir::Asc, nulls: NullsOrder::Last, path: Vec::new() });
    Table { schema, key, rows, cell, jsonb: Vec::new(), no_octet: false }
}

/// K1-MEM B's shapes: a near-unique 96-hex text (the dict arm demotes: every value is a distinct-set entry, and the
/// zstd wrapper wins), incompressible bytes (no wrapper: the unwrapped bands are the image), short texts and empty
/// ones, a 16-B fixed-width column with repeats, bools, and ints with NULLs; nine granules, two bands.
fn b_shapes(rows: u64) -> Table {
    let schema = vec![int8_col(1), text_col(2), text_col(3), text_col(4), fixed16_col(5), bool_col(6)];
    let cell = Box::new(move |i: u64, c: usize| match c {
        0 if i % 17 == 0 => Val::Null,
        0 => Val::Word(h(i) % 5_000),
        1 => text(&format!("{:016x}{:016x}{:016x}{:016x}{:016x}{:016x}", h(i), h(i + 7), h(i + 11), h(i + 13), h(i + 17), h(i + 19))),
        2 => Val::Bytes(img_4b_u(&(0..40 + h(i) % 41).map(|k| (h(i * 131 + k) >> 17) as u8).collect::<Vec<u8>>())),
        3 if i % 5 == 0 => text(""),
        3 => text(&format!("s{}", h(i) % 20_000)),
        4 if i % 23 == 0 => Val::Null,
        4 => Val::Bytes((0..16).map(|k| (h(i % 3_000 + k) >> 9) as u8).collect()),
        _ => Val::Word(u64::from(h(i) % 3 == 0)),
    });
    Table { schema, key: None, rows, cell, jsonb: Vec::new(), no_octet: false }
}

/// Production's storage posture, dict offered to the text columns that are not jsonb.
fn codec_cands(t: &Table) -> CodecCandidates {
    let storage = ColumnPosture { cold: true, ..ColumnPosture::default() };
    let dict = ColumnPosture { dict: Some(DictPolicy { ndv_cap: 1 << 24, exec_ok: true, sem: TextSemantics::BytesOnly }), ..storage };
    t.schema
        .iter()
        .filter(|s| s.class == pgrc_format::class::StorageClass::VarlenaVerbatim && !t.jsonb.contains(&s.attno))
        .fold(CodecCandidates::new(storage), |c, s| c.with_column(s.attno, 0, dict))
}

fn shred_of(t: &Table) -> Shreds {
    Shreds(t.jsonb.iter().fold(JsonbShredSource::new(), |s, &a| s.with_column(a)), t.no_octet)
}

/// The table's jsonb shred source; with `Table::no_octet`, it also owns every other column (`covers`).
struct Shreds(JsonbShredSource, bool);

impl crate::shred::ShredLaneSource for Shreds {
    fn shred(&mut self, parent: &ColBuffer, opts: &pgrc_format::relopt::ShredOptions) -> crate::WriteResult<Vec<crate::shred::ShredLane>> {
        self.0.shred(parent, opts)
    }

    fn covers(&self, attno: u32) -> bool {
        self.1 || self.0.covers(attno)
    }
}

struct Sealed {
    bytes: Vec<u8>,
    part: SealedPart,
    report: SealReport,
    files: Vec<String>,
}

fn writer(t: &Table, pool: Option<Arc<dyn ColumnPool>>) -> TableWriter {
    let mut w = open_writer_policy(t.schema.clone(), stamp(FXID, 1), PartCutPolicy { max_rows: u64::MAX, max_bytes: u64::MAX, ..PartCutPolicy::default() });
    if let Some(k) = &t.key {
        w.set_structural(StructuralPolicy::new().with_cluster_key(vec![k.clone()]));
    }
    if let Some(pool) = pool {
        let resolver: Arc<dyn VerifyResolver + Send + Sync> = Arc::new(CodecResolver);
        w.set_pool(SealPool { pool, sources: vec![Arc::new(codec_cands(t))], resolver });
    }
    w
}

fn sealed(vfs: &mut MemVfs, w: &TableWriter) -> Sealed {
    assert_eq!(w.sealed_parts().len(), 1, "one part");
    let part = w.sealed_parts()[0].clone();
    let bytes = vfs.read_full(&format!("{DIR}/{}", part.tmp_name)).expect("tmp bytes");
    let mut files = vfs.list_dir(DIR).expect("list");
    files.sort();
    Sealed { bytes, part, report: w.seal_reports()[0].clone(), files }
}

/// Every row of `t` through chunks and `finish`: the whole-part seal.
fn append_all(t: &Table, w: &mut TableWriter, vfs: &mut MemVfs) {
    let ncols = t.schema.len();
    let mut chunk = RowChunk::new(ncols, CHUNK);
    for i in 0..t.rows {
        let vals: Vec<Val> = (0..ncols).map(|c| (t.cell)(i, c)).collect();
        chunk.push_row(&vals.iter().map(Val::raw).collect::<Vec<_>>()).expect("capture");
        if chunk.rows() == CHUNK || i + 1 == t.rows {
            w.append_chunk(&chunk).expect("chunk");
            chunk = RowChunk::new(ncols, CHUNK);
        }
    }
    let (cands, resolver, mut shred, opts) = (codec_cands(t), CodecResolver, shred_of(t), pgrc_format::relopt::ShredOptions::default());
    let sources: [&dyn CandidateSource; 1] = [&cands];
    let mut env = SealEnv { vfs, sources: &sources, resolver: &resolver, shred: &mut shred, shred_opts: &opts };
    w.finish(&mut env).expect("finish");
}

fn whole(t: &Table, pool: Option<Arc<dyn ColumnPool>>) -> Sealed {
    let mut vfs = mem_with_dir();
    let mut w = writer(t, pool);
    append_all(t, &mut w, &mut vfs);
    sealed(&mut vfs, &w)
}

/// Column `c` of `t` in row order, through the seal writer's chunks (a compaction's column feed).
fn column(t: &Table, c: usize, out: &mut ColBuffer) -> crate::WriteResult<()> {
    let (mut chunk, mut scratch) = (RowChunk::new(1, CHUNK), Vec::new());
    for i in 0..t.rows {
        chunk.push_row(&[(t.cell)(i, c).raw()])?;
        if chunk.rows() == CHUNK {
            chunk.append_column(0, out, &mut NoExternalDetoast, &mut scratch)?;
            chunk = RowChunk::new(1, CHUNK);
        }
    }
    chunk.append_column(0, out, &mut NoExternalDetoast, &mut scratch)
}

/// `t` through `seal_streamed` with `feed`; the result and the columns fetched, in order.
fn streamed_with(
    t: &Table,
    vfs: &mut MemVfs,
    w: &mut TableWriter,
    feed: &mut dyn FnMut(usize, &mut ColBuffer) -> crate::WriteResult<()>,
) -> (crate::WriteResult<bool>, Vec<usize>) {
    let (cands, resolver, mut shred, opts) = (codec_cands(t), CodecResolver, shred_of(t), pgrc_format::relopt::ShredOptions::default());
    let sources: [&dyn CandidateSource; 1] = [&cands];
    let mut env = SealEnv { vfs, sources: &sources, resolver: &resolver, shred: &mut shred, shred_opts: &opts };
    let fetched = RefCell::new(Vec::new());
    let out = w.seal_streamed(&mut env, t.rows, &mut |c, out| {
        fetched.borrow_mut().push(c);
        feed(c, out)
    });
    (out, fetched.into_inner())
}

fn streamed(t: &Table, pool: Option<Arc<dyn ColumnPool>>, batch: Option<u64>) -> (Sealed, Vec<usize>) {
    let mut vfs = mem_with_dir();
    let mut w = writer(t, pool);
    if let Some(b) = batch {
        w.set_streamed_batch(b);
    }
    let (out, fetched) = streamed_with(t, &mut vfs, &mut w, &mut |c, out| column(t, c, out));
    assert!(out.expect("streamed seal"), "the default grain");
    (sealed(&mut vfs, &w), fetched)
}

fn assert_same(what: &str, got: &Sealed, want: &Sealed) {
    assert!(got.bytes == want.bytes, "{what}: the part's bytes moved ({} vs {} bytes)", got.bytes.len(), want.bytes.len());
    assert_eq!(got.part, want.part, "{what}: the sealed part's facts (footer, stats sidecar, plane and facts slices)");
    assert_eq!(got.files, want.files, "{what}: the directory");
    let (g, e) = (&got.report, &want.report);
    assert_eq!(format!("{:?}", g.elections), format!("{:?}", e.elections), "{what}: elections");
    assert_eq!(g.col_hashes, e.col_hashes, "{what}");
    assert_eq!(g.granules_verified, e.granules_verified, "{what}");
    assert_eq!(g.nonnull_crosschecks, e.nonnull_crosschecks, "{what}");
    assert_eq!(g.cluster_keys, e.cluster_keys, "{what}");
}

/// The threads of a scope, each taking every `threads`-th stream.
struct ThreadPool(usize);

impl ColumnPool for ThreadPool {
    fn run(&self, n: usize, f: &(dyn Fn(usize) + Sync)) {
        std::thread::scope(|s| {
            for t in 0..self.0 {
                s.spawn(move || (t..n).step_by(self.0).for_each(f));
            }
        });
    }

    fn run_beside(&self, n: usize, f: &(dyn Fn(usize) + Sync), beside: &mut dyn FnMut()) {
        std::thread::scope(|s| {
            for t in 0..self.0 {
                s.spawn(move || (t..n).step_by(self.0).for_each(f));
            }
            beside();
        });
    }
}

/// Streams (and bands) in reverse, the caller's work first: stitching must not depend on completion order.
struct Reversed;

impl ColumnPool for Reversed {
    fn run(&self, n: usize, f: &(dyn Fn(usize) + Sync)) {
        (0..n).rev().for_each(f)
    }

    fn run_beside(&self, n: usize, f: &(dyn Fn(usize) + Sync), beside: &mut dyn FnMut()) {
        beside();
        self.run(n, f)
    }
}

fn every_way(t: &Table) -> Sealed {
    let want = whole(t, None);
    assert_same("whole part on 4 threads", &whole(t, Some(Arc::new(ThreadPool(4)))), &want);
    let serial = streamed(t, None, None);
    assert_same("streamed, serial", &serial.0, &want);
    for (name, pool, batch) in [
        ("streamed, 4 threads, default batches", Arc::new(ThreadPool(4)) as Arc<dyn ColumnPool>, None),
        ("streamed, 4 threads, one batch", Arc::new(ThreadPool(4)), Some(u64::MAX)),
        ("streamed, reversed, batches bound by the widest stream alone", Arc::new(Reversed), Some(1)),
        ("streamed, 3 threads, a 30 MB floor", Arc::new(ThreadPool(3)), Some(30 << 20)),
        ("streamed, serial pool, a 45 MB floor", Arc::new(SerialPool), Some(45 << 20)),
    ] {
        let (got, fetched) = streamed(t, Some(pool), batch);
        assert_same(name, &got, &want);
        assert_eq!(fetched, serial.1, "{name}: the feed order");
    }
    want
}

#[test]
fn an_unsorted_streamed_part_is_the_whole_part_seal_byte_for_byte() {
    let t = mixed(70_000, false);
    let want = every_way(&t);
    assert_eq!(want.part.band_count, 2);
    assert!(want.report.cluster_keys.is_empty());
    assert_eq!(streamed(&t, None, None).1, vec![0, 1, 2, 3, 4, 5], "every column once, in schema order");
}

#[test]
fn a_sorted_by_streamed_part_is_the_whole_part_seal_byte_for_byte() {
    let t = mixed(70_000, true);
    let want = every_way(&t);
    assert_eq!(want.report.cluster_keys, vec![3], "the part attests its key");
    assert_eq!(streamed(&t, None, None).1, vec![2, 0, 1, 3, 4, 5], "the key column first, held to its turn");
}

#[test]
fn a_streamed_part_with_shredded_jsonb_lanes_is_the_whole_part_seal_byte_for_byte() {
    super::shred_jsonb_tests::setup();
    let ctx = mcx::MemoryContext::new("k1mem-jsonb");
    let jsonb = |d: String| {
        adt_jsonb::io::jsonb_in(ctx.mcx(), d.as_bytes(), None).unwrap_or_else(|e| panic!("jsonb_in {d}: {}", e.message())).expect("hard path")[..].to_vec()
    };
    let rows = 20_000u64;
    let a: Arc<Vec<Option<Vec<u8>>>> = Arc::new(
        (0..rows).map(|i| (i % 13 != 5).then(|| jsonb(format!(r#"{{"active":{},"id":{},"name":"tok{}","score":{}.25}}"#, i % 2 == 0, i, i % 97, i % 1000)))).collect(),
    );
    let b: Arc<Vec<Option<Vec<u8>>>> = Arc::new((0..rows).map(|i| Some(jsonb(format!(r#"{{"k":{},"tag":"t{}"}}"#, h(i) % 5000, i % 11)))).collect());
    let t = Table {
        schema: vec![int8_col(1), text_col(2), text_col(3), text_col(4)],
        key: None,
        rows,
        cell: Box::new(move |i, c| match c {
            0 => Val::Word(i),
            1 => a[i as usize].clone().map_or(Val::Null, Val::Bytes),
            2 => text(&format!("p{}", i % 300)),
            _ => Val::Bytes(b[i as usize].clone().expect("doc")),
        }),
        jsonb: vec![2, 4],
        no_octet: false,
    };
    let want = every_way(&t);
    let lanes: Vec<(u32, u32)> = want.report.elections.iter().filter(|e| e.path_ord > 0).map(|e| (e.attno, e.path_ord)).collect();
    assert!(lanes.iter().any(|l| l.0 == 2) && lanes.iter().any(|l| l.0 == 4), "both jsonb columns shredded: {lanes:?}");
    assert_eq!(streamed(&t, None, None).1, vec![0, 1, 2, 3, 1, 3], "each shredded root again, for its lanes");
}

/// A seal's bytes, facts and report, as a line of crc32c digests.
fn digest(what: &str, s: &Sealed) -> String {
    let c = |b: &[u8]| pgrc_format::wire::crc32c(b);
    let r = &s.report;
    let report = format!("{:?}|{:?}|{}|{}|{:?}", r.elections, r.col_hashes, r.granules_verified, r.nonnull_crosschecks, r.cluster_keys);
    format!(
        "{what}: {} bytes {:08x} stats {:08x} part {:08x} report {:08x}",
        s.bytes.len(),
        c(&s.bytes),
        c(&s.part.stats_payload),
        c(format!("{:?}", s.part).as_bytes()),
        c(report.as_bytes())
    )
}

/// K1-MEM B: the seal's bytes, footer, stats sidecar, slices and report for fixed tables, pinned as digests taken on
/// the tree before B (91120476dda). The distinct set keyed into the column and the value bands moved into the stream
/// image leave every one where it was; the tests above compare two paths through one seal, this one the seal with its
/// earlier self (every path of `b_shapes`, too). Re-pinned by lane AUTOVAC unit 3 (FOLDSTATS): the stats sidecar's
/// histogram bounds of the word columns are in value order and repeat a value that spans ranks, so the stats payload
/// (and the part record that carries it) moved; the part file's bytes and the report did not. Re-pinned by
/// lane RENAME (2026-10-03): the part record's tmp_name is a byte shorter (the temp suffix is `.pgrct` now), so its Debug text and digest moved;
/// the part file's bytes, stats and report did not. Re-pinned by lane IX0 (L3): the sealed part's Debug text carries its
/// row-ID ranges (`ids: []` from a seal without a grant), so the part digest moved; bytes, stats and report did not.
/// Re-pinned by V5SYNC (trunk's format-additive ports, 10-08): the equality blooms are sized and kept by cost (P5-12,
/// F4, D3070: `mixed` keeps one 97-byte bloom on column 2 where it had 12, 12 and 1.7 KB ones on columns 3-5) and a
/// dictionary-coded stream carries its code zones (a CodeZones section, kind 12, 88 bytes on column 2), so the part's
/// bytes, its stream directory's crc and the part digest moved; every stream, stats, PSMA, NDV, flat-stats and digest
/// section, the stats sidecar and the report did not (section table compared by kind, line against merged).
/// Re-pinned for D3038 (trunk's octet-length lane, 10-08): each text column that does not seal as dictionary codes or
/// one constant takes an int4 octet-length lane after the other streams, so the part's bytes, the part record and the
/// report (the lanes' elections and hashes) moved; the stats sidecar did not. Sealed without the lanes, every digest is
/// the pin before them ([`without_octet_lanes_the_seals_bytes_are_the_digests_pinned_before_them`]).
/// Re-pinned for the integer pre-pass default (`PGRUST_PGRC_INTPRE`): the int streams it wraps moved the part's bytes and
/// record of both mixed tables; stats and report did not. Re-pinned again for the OnPair defaults
/// (`PGRUST_PGRC_ONPAIR`, `PGRUST_PGRC_ONPAIR_VERBATIM`): the text streams they wrap moved the same digests.
#[test]
fn the_seals_bytes_are_the_pinned_digests() {
    let got = vec![
        digest("mixed", &whole(&mixed(70_000, false), None)),
        digest("mixed sorted", &whole(&mixed(70_000, true), None)),
        digest("b shapes", &every_way(&b_shapes(70_000))),
    ];
    let want = [
        "mixed: 1193912 bytes 95205ee8 stats 5eb5674c part 8cdfefdf report 728174cf",
        "mixed sorted: 992792 bytes 6a41b7ed stats a94fe109 part d06b8196 report 9ff2ccc5",
        "b shapes: 4077528 bytes ac72ad69 stats 733e48bf part a471a5b3 report 1ca8d64a",
    ];
    assert_eq!(got, want, "the seal's bytes moved");
}

/// D3038: the octet-length lanes are the whole of the move above. With no column taking one (`Table::no_octet`), every
/// path of the seal writes the bytes, part record, stats and report pinned before the lane, but for the mixed tables'
/// bytes and records, which the integer pre-pass and OnPair defaults moved.
#[test]
fn without_octet_lanes_the_seals_bytes_are_the_digests_pinned_before_them() {
    let no = |t: Table| Table { no_octet: true, ..t };
    let got = vec![
        digest("mixed", &whole(&no(mixed(70_000, false)), None)),
        digest("mixed sorted", &whole(&no(mixed(70_000, true)), None)),
        digest("b shapes", &every_way(&no(b_shapes(70_000)))),
    ];
    let want = [
        "mixed: 1181616 bytes 564cd45f stats 5eb5674c part e81cadaa report e0ee35ee",
        "mixed sorted: 980496 bytes 6a72ad1b stats a94fe109 part 1ba0ae96 report e9b5078f",
        "b shapes: 4026016 bytes 2efb1308 stats 733e48bf part 12d25988 report 15c44677",
    ];
    assert_eq!(got, want, "a seal without octet-length lanes moved");
}

#[test]
fn a_part_past_the_default_grain_is_handed_back_and_seals_whole() {
    // 8,192 values of about 2,000 B: one default granule of about 16 MB, past the 10 MiB bound (SB-10).
    let t = Table {
        schema: vec![int8_col(1), text_col(2)],
        key: None,
        rows: 8_192,
        cell: Box::new(|i, c| match c {
            0 => Val::Word(i),
            _ => text(&format!("{:016x}", h(i)).repeat(125)),
        }),
        jsonb: Vec::new(),
        no_octet: false,
    };
    let mut vfs = mem_with_dir();
    let mut w = writer(&t, None);
    let (out, fetched) = streamed_with(&t, &mut vfs, &mut w, &mut |c, out| column(&t, c, out));
    assert!(!out.expect("handed back"), "the part needs a finer grain");
    assert_eq!(fetched, vec![0, 1], "handed back at the first stream past the bound");
    assert!(w.sealed_parts().is_empty());
    assert!(vfs.list_dir(DIR).expect("list").is_empty(), "the tmp it began is reclaimed");
    append_all(&t, &mut w, &mut vfs);
    let got = sealed(&mut vfs, &w);
    let want = whole(&t, None);
    assert!(got.bytes == want.bytes, "the rows then seal whole, as ever");
    assert!(got.part.granule_count > 1, "at a finer grain");
}

#[test]
fn a_streamed_feed_out_of_key_order_is_refused_before_any_file() {
    let t = mixed(20_000, true);
    let mut vfs = mem_with_dir();
    let mut w = writer(&t, None);
    let src = mixed(20_000, true);
    let rev = Table { cell: Box::new(move |i, c| (src.cell)(19_999 - i, c)), ..mixed(20_000, true) };
    let (out, fetched) = streamed_with(&t, &mut vfs, &mut w, &mut |c, out| column(&rev, c, out));
    assert!(matches!(out, Err(WriteError::Contract { detail: "a streamed seal's feed is not in cluster-key order" })), "{:?}", out.err());
    assert_eq!(fetched, vec![2], "only the key column was read");
    assert!(vfs.list_dir(DIR).expect("list").is_empty());
}

#[test]
fn a_streamed_part_goes_by_reference_with_the_whole_parts_masks_and_crc() {
    let t = mixed(20_000, false);
    let run = |streamed_seal: bool| {
        let log = spool_log(1_000, None);
        let mut vfs = mem_with_spool();
        let mut w = writer(&t, None).with_sink(Box::new(RecordingSink::new(DIR, log.clone())));
        match streamed_seal {
            false => append_all(&t, &mut w, &mut vfs),
            true => assert!(streamed_with(&t, &mut vfs, &mut w, &mut |c, out| column(&t, c, out)).0.expect("streamed")),
        }
        let bytes = vfs.read_full(&format!("{DIR}/{}", w.sealed_parts()[0].tmp_name)).expect("tmp");
        let refs: Vec<_> = lock(&log)
            .records
            .iter()
            .filter_map(|r| match r {
                WalRecord::FileRef { name, len, crc, masks, .. } => Some((name.clone(), *len, *crc, *masks)),
                _ => None,
            })
            .collect();
        (bytes, refs, w.sealed_parts()[0].footer_off)
    };
    let (want, got) = (run(false), run(true));
    assert!(got.0 == want.0, "the part's bytes");
    assert_eq!(want.1.len(), 1, "the whole-part seal went by reference");
    assert_eq!(want.1[0].3, crate::seal::part_ref_masks(want.2), "header and footer masked");
    assert_eq!(got.1, want.1, "the same reference: name, length, masks and crc");
}

#[test]
fn a_pooled_batch_never_holds_more_than_the_widest_stream_so_far() {
    use crate::seal::BatchBound;
    let mut serial = BatchBound::new(false, 1 << 30);
    assert!(!serial.admit(10, false));
    assert!(serial.admit(10, true), "without a pool every stream seals alone");
    let mut b = BatchBound::new(true, 100);
    let cuts: Vec<bool> = [(50, false), (40, true), (30, true), (500, true), (200, true), (250, true), (60, true)].iter().map(|&(ws, g)| b.admit(ws, g)).collect();
    // 50 | 40 (90 <= 100) | 30 cuts (120) | 500 cuts (the bound rises to 500, 530) | 200 cuts (700) | 250 joins (450)
    // | 60 cuts (510)
    assert_eq!(cuts, vec![false, false, true, true, true, false, true]);
}
