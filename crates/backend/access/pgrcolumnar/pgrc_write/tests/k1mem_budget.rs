//! K1-MEM: a part's seal memory, read from a counting allocator (live bytes and their peak; a realloc counts its copy
//! window). The whole-part seal (`append_chunk` then `finish`, the path a compaction's `SealWriter` took) holds every
//! column of the part and then seals it; the streamed seal (`seal_streamed`, the compaction's path now) holds one
//! column, or one pooled batch of narrow columns, at a time. Its own test binary: the allocator is process-wide.
//!
//! `k1mem_probe` (ignored) prints both for the shapes of the measurements: `K1MEM_ROWS`, `K1MEM_TABLE` (a comma list
//! of d259, cb, wide), `K1MEM_THREADS` (a comma list; 0 = no pool).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::Arc;

use pgrc_format::class::ColSchema;
use pgrc_write::dict::TextSemantics;
use pgrc_write::elect::{CandidateSource, CodecCandidates, ColumnPosture, DictPolicy};
use pgrc_write::ingest::{ColBuffer, NoExternalDetoast, RawDatum};
use pgrc_write::par::RowChunk;
use pgrc_write::seal::{CodecResolver, ColumnPool, VerifyResolver};
use pgrc_write::testkit::*;
use pgrc_write::writer::{PartCutPolicy, SealEnv, SealPool, TableWriter};
use pgrc_write::wvfs::{MemVfs, WriteVfs, WvFd};
use pgrc_write::WriteResult;

struct Counting;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: forwards to `System`; the counters are plain atomics.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            let n = LIVE.fetch_add(l.size(), Relaxed) + l.size();
            PEAK.fetch_max(n, Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        LIVE.fetch_sub(l.size(), Relaxed);
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let n = LIVE.fetch_add(new, Relaxed) + new;
        PEAK.fetch_max(n, Relaxed);
        let q = unsafe { System.realloc(p, l, new) };
        LIVE.fetch_sub(if q.is_null() { new } else { l.size() }, Relaxed);
        q
    }
}

#[global_allocator]
static A: Counting = Counting;

const MB: f64 = 1048576.0;
const CHUNK: u32 = 65_536;

/// Writes nowhere: the part's bytes are not the seal's memory.
struct NullVfs {
    inner: MemVfs,
    written: u64,
}

impl WriteVfs for NullVfs {
    fn create_rw(&mut self, p: &str) -> WriteResult<WvFd> {
        self.inner.create_rw(p)
    }
    fn open_rw(&mut self, p: &str) -> WriteResult<WvFd> {
        self.inner.open_rw(p)
    }
    fn pwrite_at(&mut self, _fd: &WvFd, _off: u64, b: &[u8]) -> WriteResult<()> {
        self.written += b.len() as u64;
        Ok(())
    }
    fn fsync_file(&mut self, _fd: &WvFd) -> WriteResult<()> {
        Ok(())
    }
    fn close_file(&mut self, _fd: WvFd) -> WriteResult<()> {
        Ok(())
    }
    fn read_full(&mut self, _p: &str) -> WriteResult<Vec<u8>> {
        Ok(Vec::new())
    }
    fn rename_path(&mut self, _f: &str, _t: &str) -> WriteResult<()> {
        Ok(())
    }
    fn unlink_path(&mut self, _p: &str) -> WriteResult<()> {
        Ok(())
    }
    fn fsync_dir(&mut self, _d: &str) -> WriteResult<()> {
        Ok(())
    }
    fn list_dir(&mut self, _d: &str) -> WriteResult<Vec<String>> {
        Ok(Vec::new())
    }
    fn mkdir_path(&mut self, _d: &str) -> WriteResult<()> {
        Ok(())
    }
    fn exists_path(&mut self, _p: &str) -> WriteResult<bool> {
        Ok(true)
    }
}

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

fn xs(mut x: u64) -> u64 {
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

/// `bytes` random bytes as hex (md5-like text).
fn hex(seed: u64, bytes: usize) -> String {
    let mut s = String::with_capacity(bytes * 2 + 16);
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    while s.len() < bytes * 2 {
        x = xs(x);
        s.push_str(&format!("{x:016x}"));
    }
    s.truncate(bytes * 2);
    s
}

enum Cell {
    W(u64),
    B(Vec<u8>),
    N,
}

impl Cell {
    fn raw(&self) -> RawDatum<'_> {
        match self {
            Cell::W(w) => RawDatum::Word(*w),
            Cell::B(b) => RawDatum::Bytes(b),
            Cell::N => RawDatum::Null,
        }
    }
}

fn txt(s: &str) -> Cell {
    Cell::B(img_4b_u(s.as_bytes()))
}

struct Tbl {
    schema: Vec<ColSchema>,
    /// The text columns offered the dict arm (production's posture for text).
    dict: Vec<u32>,
    cell: fn(u64, usize) -> Cell,
}

/// The D259 table's ten columns (the compaction e2e's shape): ints, an md5, 3 x md5, a 50-value text, a 5-value text
/// with NULLs, a numeric's text, a float8's bits, a date.
fn d259_cell(i: u64, c: usize) -> Cell {
    match c % 10 {
        0 => Cell::W(i),
        1 => Cell::W(i % 1000),
        2 => Cell::W(i.wrapping_mul(7919)),
        3 => txt(&hex(i + 1_000_003 * (c as u64 / 10), 16)),
        4 => txt(&(hex(i, 16) + &hex(i + 1, 16) + &hex(i + 2, 16))),
        5 => txt(&format!("cat{}", i % 50)),
        6 if i % 7 == 0 => Cell::N,
        6 => txt(&format!("c{}", i % 5)),
        7 => txt(&format!("{}", (i % 10000) as f64 / 4.0)),
        8 => Cell::W((i as f64 * 0.5).to_bits()),
        _ => Cell::W(i % 700),
    }
}

fn d259_schema(ncols: u32) -> Vec<ColSchema> {
    (1..=ncols).map(|a| if (3..=7).contains(&((a - 1) % 10)) { text_col(a) } else { int8_col(a) }).collect()
}

fn d259() -> Tbl {
    let schema = d259_schema(10);
    Tbl { dict: schema.iter().filter(|s| s.typlen == -1).map(|s| s.attno).collect(), schema, cell: d259_cell }
}

/// Three D259s side by side (thirty columns): a part far wider than any one of its columns.
fn wide() -> Tbl {
    let schema = d259_schema(30);
    Tbl { dict: schema.iter().filter(|s| s.typlen == -1).map(|s| s.attno).collect(), schema, cell: d259_cell }
}

/// compbench: an id and two 128-hex texts.
fn cb() -> Tbl {
    let cell = |i: u64, c: usize| match c {
        0 => Cell::W(i),
        _ => txt(&hex(i * 2 + 10 + c as u64, 64)),
    };
    Tbl { schema: vec![int8_col(1), text_col(2), text_col(3)], dict: vec![2, 3], cell }
}

fn cands(t: &Tbl) -> CodecCandidates {
    let storage = ColumnPosture { cold: true, ..ColumnPosture::default() };
    let dict = ColumnPosture { dict: Some(DictPolicy { ndv_cap: 1 << 24, exec_ok: true, sem: TextSemantics::Utf8Chars }), ..storage };
    t.dict.iter().fold(CodecCandidates::new(storage), |c, &a| c.with_column(a, 0, dict))
}

#[derive(Debug)]
struct Measure {
    /// The whole-part seal's column buffers before the seal (the part's columns).
    buffered: usize,
    /// The peak live bytes over the start: the appends and the seal (whole), the feed and the seal (streamed).
    peak: usize,
    /// The live bytes over the start once the seal is done. LIVE counts the whole process, so bytes allocated before
    /// the start and freed during the measure can take it under the start: that reads 0 (nothing kept).
    after: usize,
    part: u64,
    secs: f64,
}

fn writer(t: &Tbl, threads: usize) -> TableWriter {
    let mut w = open_writer_policy(t.schema.clone(), stamp(77, 1), PartCutPolicy { max_rows: u64::MAX, max_bytes: u64::MAX, ..PartCutPolicy::default() });
    if threads > 0 {
        let resolver: Arc<dyn VerifyResolver + Send + Sync> = Arc::new(CodecResolver);
        w.set_pool(SealPool { pool: Arc::new(ThreadPool(threads)), sources: vec![Arc::new(cands(t))], resolver });
    }
    w
}

fn whole(t: &Tbl, n: u64, threads: usize) -> Measure {
    let base = LIVE.load(Relaxed);
    PEAK.store(base, Relaxed);
    let (mut vfs, mut kit, c) = (NullVfs { inner: mem_with_dir(), written: 0 }, Kit::new(), cands(t));
    let mut w = writer(t, threads);
    let t0 = std::time::Instant::now();
    let ncols = t.schema.len();
    let mut chunk = RowChunk::new(ncols, CHUNK);
    for i in 0..n {
        let cells: Vec<Cell> = (0..ncols).map(|col| (t.cell)(i, col)).collect();
        chunk.push_row(&cells.iter().map(Cell::raw).collect::<Vec<_>>()).expect("capture");
        if chunk.rows() == CHUNK || i + 1 == n {
            w.append_chunk(&chunk).expect("chunk");
            chunk = RowChunk::new(ncols, CHUNK);
        }
    }
    drop(chunk);
    let buffered = LIVE.load(Relaxed).saturating_sub(base);
    let resolver = CodecResolver;
    let sources: [&dyn CandidateSource; 1] = [&c];
    let mut env = SealEnv { vfs: &mut vfs, sources: &sources, resolver: &resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
    w.finish(&mut env).expect("finish");
    let (peak, after, secs) = (PEAK.load(Relaxed) - base, LIVE.load(Relaxed).saturating_sub(base), t0.elapsed().as_secs_f64());
    Measure { buffered, peak, after, part: vfs.written, secs }
}

fn streamed(t: &Tbl, n: u64, threads: usize) -> Measure {
    let base = LIVE.load(Relaxed);
    PEAK.store(base, Relaxed);
    let (mut vfs, mut kit, c) = (NullVfs { inner: mem_with_dir(), written: 0 }, Kit::new(), cands(t));
    let mut w = writer(t, threads);
    let t0 = std::time::Instant::now();
    let resolver = CodecResolver;
    let sources: [&dyn CandidateSource; 1] = [&c];
    let mut env = SealEnv { vfs: &mut vfs, sources: &sources, resolver: &resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
    let mut feed = |col: usize, out: &mut ColBuffer| -> WriteResult<()> {
        let (mut chunk, mut scratch) = (RowChunk::new(1, CHUNK), Vec::new());
        for i in 0..n {
            chunk.push_row(&[(t.cell)(i, col).raw()])?;
            if chunk.rows() == CHUNK {
                chunk.append_column(0, out, &mut NoExternalDetoast, &mut scratch)?;
                chunk = RowChunk::new(1, CHUNK);
            }
        }
        chunk.append_column(0, out, &mut NoExternalDetoast, &mut scratch)
    };
    assert!(w.seal_streamed(&mut env, n, &mut feed).expect("streamed seal"), "the default grain");
    let (peak, after, secs) = (PEAK.load(Relaxed) - base, LIVE.load(Relaxed).saturating_sub(base), t0.elapsed().as_secs_f64());
    Measure { buffered: 0, peak, after, part: vfs.written, secs }
}

/// A part of thirty D259 columns (about 11 MB of column buffers per 100,000 rows... per column set) seals in well
/// under its column buffers when streamed; the whole-part seal holds all of them and then some.
#[test]
fn a_streamed_seal_holds_a_column_not_the_part() {
    const ROWS: u64 = 60_000;
    const BUDGET: usize = 40 << 20;
    let t = wide();
    for threads in [0, 4] {
        let w = whole(&t, ROWS, threads);
        let s = streamed(&t, ROWS, threads);
        eprintln!("K1MEM wide rows={ROWS} threads={threads}: whole {w:?}; streamed {s:?}");
        assert_eq!(s.part, w.part, "the same part");
        assert!(w.peak > 3 * BUDGET, "the part is far larger than the budget: whole-part seal peak {:.1} MB", w.peak as f64 / MB);
        assert!(s.peak <= BUDGET, "{threads} threads: streamed seal peak {:.1} MB over the budget {:.1} MB", s.peak as f64 / MB, BUDGET as f64 / MB);
        assert!(s.after < 1 << 20, "nothing kept: {} bytes", s.after);
    }
}

#[test]
#[ignore = "measurement: K1MEM_ROWS / K1MEM_TABLE / K1MEM_THREADS"]
fn k1mem_probe() {
    let n: u64 = std::env::var("K1MEM_ROWS").ok().and_then(|s| s.parse().ok()).unwrap_or(200_000);
    let which = std::env::var("K1MEM_TABLE").unwrap_or_else(|_| "d259,cb".into());
    let ths: Vec<usize> = std::env::var("K1MEM_THREADS").map_or(vec![0, 4], |s| s.split(',').map(|x| x.parse().expect("threads")).collect());
    for th in ths {
        for (name, t) in [("d259", d259()), ("cb", cb()), ("wide", wide())] {
            if !which.split(',').any(|w| w == name) {
                continue;
            }
            let w = whole(&t, n, th);
            let s = streamed(&t, n, th);
            eprintln!(
                "K1MEM {name} rows={n} threads={th}: buffered {:.1} MB; whole peak {:.1} MB {:.2}s; streamed peak {:.1} MB {:.2}s; part {:.1} MB{}",
                w.buffered as f64 / MB,
                w.peak as f64 / MB,
                w.secs,
                s.peak as f64 / MB,
                s.secs,
                s.part as f64 / MB,
                if s.part == w.part { "" } else { " PART SIZE DIFFERS" }
            );
        }
    }
}
