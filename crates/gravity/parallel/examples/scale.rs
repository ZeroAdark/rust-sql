//! gravity-parallel-scale, table A of docs/gravity/parallel.md: the driver
//! (claims, per-worker bindings, the Source protocol, merge, barrier) over
//! in-memory int arrays vs the lab loops for the same operation over the same
//! arrays, at W = 1, 4, 8, 12, 16 on the fixture pool. `tsv` writes the same
//! rows out for tables B (pgrc) and C (heap).
//!
//!   scale a   [rows] [outer] [inner] [w,w,..]   the driver / lab table (LANE_WIDTH=8: datum-wide lanes, widened per window)
//!   scale tsv <dir> [rows] [outer] [inner]      t.tsv, inner.tsv, outer.tsv

#![allow(dead_code)]

#[path = "../tests/common/mod.rs"]
mod common;

use std::io::Write;
use std::sync::atomic::AtomicU8;
use std::sync::Arc;
use std::time::Instant;

use common::parts;
use gravity_abi::ctx::lane;
use gravity_abi::{Datum, Lane, ScanPos};
use gravity_driver::{box_source, ArenaBox, ArenaRef, SourceBox, Binding, BorrowedBatch, Cursor, Derive, Entry, Flow, Locker, Merge, LegRef, Result, Sink, SinkLeg, Source, Window, WindowBase, Writer};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use gravity_kernels::table::params as par_hashagg;
use gravity_kernels::par_hashjoin;
use gravity_program::hand as programs;
use gravity_parallel::{run_program, BlockRange, ClaimSize, FixturePool, HashAggPartial, HashBuildPartial, MorselSource, Plan, PlainAggPartial, Pool, RowsUnordered};
use gravity_abi::breaker::{Breaker, BreakerKind};
use gravity_program::{End, Program, SiteDesc, WritePlan};

const BLOCK: u64 = 8192;
const CLAIM: u64 = 8;
const REL_A: u32 = 101;
const REL_KV: u32 = 102;
const REL_IN: u32 = 103;
const REL_OUT: u32 = 104;
const THR: i64 = 2_500_000;
const GROUPS: u64 = 100;

// ---- the rows (the same formula feeds the TSVs for tables B and C)
//
// Formula v2 (2026-09-13, docs/gravity/record-vm.md "reference corrected"; the
// template key in scripts/gravity-parallel-scale.sh is `FORMULA=v2`): the JOIN
// KEYS ARE RANDOM. v1's inner `k = 100·j` and outer `k = (7919·i) mod
// 10,000,019` were arithmetic progressions: a multiplicative (non-avalanching)
// home hash walked the build table in stride, the prefetcher followed, and a
// join cell measured the key pattern, not the kernel (and the two low zero
// bits of `100·j` skewed any low-bit partition 4:1). Every key is now a
// fixed-seed draw indexable by position (`fill` stays parallel, `tsv` a
// stream), with the same cardinalities and match rate as v1:
//   inner  k_j = bij(j, 30)                    1 M DISTINCT keys in [0, 2^30)
//   outer  row i is a hit iff perm(i, OUTER) < OUTER / 100 — exactly 100 k
//          hits at random positions; a hit's key is the inner key of index
//          perm(rank, INNER) (100 k DISTINCT inner keys matched, as in v1);
//          a miss's key is 2^30 | a 30-bit draw (never an inner key)
// `t_row` (tc / th) is v1's: k % 100, v = 31·i % 1000, a = 7919·i % 5,000,011.

fn t_row(i: u64) -> (i32, i32, i32) {
    ((i % GROUPS) as i32, ((i * 31) % 1000) as i32, ((i * 7919) % 5_000_011) as i32)
}

const SEED: u64 = 0x5EED_0000_0000_0002;
const KEY_BITS: u32 = 30;

/// splitmix64's finaliser.
fn splitmix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// The i-th draw of the fixed-seed splitmix64 stream (state `SEED + (i+1)·φ`), by position.
fn draw(i: u64) -> u64 {
    splitmix(SEED.wrapping_add((i + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15)))
}

/// A bijection on `bits`-bit values: three xorshift-multiply rounds, each invertible mod 2^bits
/// (a right xorshift is invertible at any width; an odd multiplier is a unit mod 2^bits).
fn bij(x: u64, bits: u32) -> u64 {
    let m = if bits >= 64 { u64::MAX } else { (1u64 << bits) - 1 };
    let s1 = (bits / 2).max(1);
    let s2 = (bits / 3).max(1);
    let mut x = (x ^ (SEED & m)) & m;
    for mul in [0x9E37_79B9_7F4A_7C15u64, 0xBF58_476D_1CE4_E5B9, 0x94D0_49BB_1331_11EB] {
        x ^= x >> s1;
        x = x.wrapping_mul(mul | 1) & m;
        x ^= x >> s2;
    }
    x
}

/// A bijection on [0, n): `bij` over the next power of two, cycle-walked until it lands in range.
fn perm(i: u64, n: u64) -> u64 {
    debug_assert!(i < n);
    let bits = (64 - (n - 1).leading_zeros()).max(1);
    let mut x = i;
    loop {
        x = bij(x, bits);
        if x < n {
            return x;
        }
    }
}

/// The j-th build key: distinct across j (a bijection on 30 bits), < 2^30.
fn inner_key(j: u64) -> i32 {
    bij(j, KEY_BITS) as i32
}

fn inner_row(j: u64) -> (i32, i32) {
    (inner_key(j), ((j * 3) % 1_000_000) as i32)
}

/// Outer row i of `outer` rows against `inner` build keys: a 1 % hit rate, hits at random
/// positions on distinct build keys (`inner >= outer / 100`, else `rank % inner`).
fn outer_row(i: u64, outer: u64, inner: u64) -> (i32, i32) {
    let hits = (outer / 100).max(1);
    let rank = perm(i, outer);
    let k = if rank < hits {
        inner_key(if hits <= inner { perm(rank, inner) } else { rank % inner })
    } else {
        ((1u64 << KEY_BITS) | (draw(i) & ((1u64 << KEY_BITS) - 1))) as i32
    };
    (k, (i % 1000) as i32)
}

fn fill(n: u64, f: impl Fn(u64) -> i32 + Sync) -> Arc<Vec<i32>> {
    let mut v = vec![0i32; n as usize];
    let threads = 8;
    let chunk = (n as usize).div_ceil(threads);
    std::thread::scope(|s| {
        for (c, part) in v.chunks_mut(chunk).enumerate() {
            let f = &f;
            s.spawn(move || {
                for (i, x) in part.iter_mut().enumerate() {
                    *x = f((c * chunk + i) as u64);
                }
            });
        }
    });
    Arc::new(v)
}

// ---- the array source: what a lab kernel sees, behind the Source protocol

struct Cols {
    rel: u32,
    rows: u64,
    cols: Vec<Arc<Vec<i32>>>,
}

impl Cols {
    fn blocks(&self) -> u64 {
        self.rows.div_ceil(BLOCK)
    }
}

struct ArraySource {
    t: Arc<Cols>,
    natts: usize,
    attnos: [usize; 4],
    row: u64,
    end: u64,
    block: u64,
    lo: u64,
    cur: Cursor,
    /// `LANE_WIDTH=8`: the block widened into datum words per window (what a pgrc decode writes), else the i32 arrays as they are.
    wide: Vec<u64>,
}

impl Source for ArraySource {
    fn bind(&mut self, _b: &gravity_driver::BindingRef<'_>) -> Result<()> {
        Ok(())
    }

    fn seek(&mut self, keys: &[Datum], _null: &[u8]) -> Result<()> {
        let blocks = self.t.blocks();
        let hi = keys[1].min(blocks);
        let lo = keys[0].min(hi);
        self.lo = lo;
        self.block = lo;
        self.row = lo * BLOCK;
        self.end = (hi * BLOCK).min(self.t.rows);
        self.cur.clear();
        Ok(())
    }

    /// One block per window: the `i32` arrays themselves as width-4 lanes, no copy.
    #[inline]
    fn window(&mut self, w: &mut Window<'_>) -> Result<u32> {
        if self.row >= self.end {
            return Ok(0);
        }
        let r = self.row as usize;
        let n = (self.end - self.row).min(BLOCK) as usize;
        for k in 0..self.natts {
            let src = &self.t.cols[self.attnos[k]][r..r + n];
            if self.wide.is_empty() {
                w.set(k, Lane::byval(src.as_ptr() as *const u8, core::ptr::null(), 4, lane::REGION_ARENA, lane::id(lane::ID_ATTR, k as u32)));
            } else {
                let dst = &mut self.wide[k * BLOCK as usize..k * BLOCK as usize + n];
                for (d, s) in dst.iter_mut().zip(src) {
                    *d = *s as i64 as u64;
                }
                w.set(k, Lane::byval(dst.as_ptr() as *const u8, core::ptr::null(), 8, lane::REGION_ARENA, lane::id(lane::ID_ATTR, k as u32)));
            }
        }
        w.rows = n as u32;
        let mut base = WindowBase::at(ScanPos::new(self.t.rel, self.block as u32, 0));
        base.claim_id = self.lo as u32;
        w.set_base(base);
        self.row += n as u64;
        self.block += 1;
        Ok(n as u32)
    }

    fn lanes_hint(&self) -> usize {
        self.natts + 1
    }

    fn cursor(&self) -> &Cursor {
        &self.cur
    }

    fn cursor_mut(&mut self) -> &mut Cursor {
        &mut self.cur
    }

    fn refetch(&mut self, _v: &mut [Datum], _n: &mut [u8]) -> Result<bool> {
        Ok(false)
    }

    fn substitute(&mut self, _pos: ScanPos) {}

    fn unbind(&mut self) {}
}

struct ArrayDerive {
    tables: Vec<Arc<Cols>>,
}

impl ArrayDerive {
    fn table(&self, rel: u32) -> &Arc<Cols> {
        self.tables.iter().find(|t| t.rel == rel).expect("table")
    }
}

impl Derive for ArrayDerive {
    fn size_hint(&self, _d: &SiteDesc) -> usize {
        core::mem::size_of::<ArraySource>()
    }

    fn derive_source(&mut self, d: &SiteDesc, a: ArenaRef) -> SourceBox {
        let mut attnos = [0usize; 4];
        for (k, slot) in attnos.iter_mut().enumerate().take(d.natts()) {
            *slot = d.out_attnos[k] as usize - 1;
        }
        let wide = if std::env::var("LANE_WIDTH").is_ok_and(|w| w == "8") { vec![0u64; d.natts() * BLOCK as usize] } else { Vec::new() };
        box_source(ArraySource { t: self.table(d.relation).clone(), natts: d.natts(), attnos, row: 0, end: 0, block: 0, lo: 0, cur: Cursor::new(), wide }, a)
    }

    fn writer_size_hint(&self) -> usize {
        0
    }

    fn derive_writer(&mut self, _p: &WritePlan, _a: ArenaRef) -> ArenaBox<dyn Writer> {
        unreachable!()
    }

    fn locker_size_hint(&self) -> usize {
        0
    }

    fn derive_locker(&mut self, _p: &WritePlan, _a: ArenaRef) -> ArenaBox<dyn Locker> {
        unreachable!()
    }
}

struct ArrayPlan {
    program: Arc<Program>,
    width: u16,
    cursors: [BlockRange; 2],
    /// The join's third phase: the build stream's partitions, sized after the build ran.
    parts: gravity_parallel::PartitionRange,
    hash_build: HashBuildPartial,
}

const PLAIN: PlainAggPartial = PlainAggPartial;
static HASHAGG: HashAggPartial = HashAggPartial::new(0, None);
const ROWS: RowsUnordered = RowsUnordered;

impl Plan for ArrayPlan {
    fn width_of(&self, _p: usize) -> u16 {
        self.width
    }

    fn claims_for(&self, p: usize) -> &dyn MorselSource {
        if matches!(self.program.pipelines[p].claims, gravity_program::ClaimKind::SpillPartitions { .. }) {
            return &self.parts;
        }
        &self.cursors[p]
    }

    fn merge_for(&self, p: usize) -> &dyn Merge {
        match self.program.pipelines[p].ends_in {
            End::Breaker(Breaker { kind: BreakerKind::PlainAgg, .. }) => &PLAIN,
            End::Breaker(Breaker { kind: BreakerKind::HashAgg, .. }) => &HASHAGG,
            End::Breaker(Breaker { kind: BreakerKind::HashBuild, .. }) => &self.hash_build,
            _ => &ROWS,
        }
    }

    fn after_pipeline(&self, p: usize, exec: &mut gravity_driver::Exec) -> Result<()> {
        if let Some(slot) = self.program.pipelines[p].produces() {
            if self.program.pipelines.iter().any(|q| q.claims == gravity_program::ClaimKind::SpillPartitions { slot }) {
                self.parts.set_total(exec.node().spill_parts_of(slot) as u64);
            }
        }
        Ok(())
    }
}

/// The rows that reach the client, folded to (rows, sum of every cell): the
/// same fold the lab loops return, so both answers compare as one number.
#[derive(Default)]
struct CheckSink {
    rows: AtomicU64,
    sum: AtomicI64,
}

impl CheckSink {
    fn fold(&self, b: BorrowedBatch<'_>) -> Result<Flow> {
        let mut s = 0i64;
        for (v, _) in b.iter() {
            for x in v {
                s = s.wrapping_add(*x as i64);
            }
        }
        self.rows.fetch_add(b.rows as u64, Ordering::Relaxed);
        self.sum.fetch_add(s, Ordering::Relaxed);
        Ok(Flow::Continue)
    }

    fn take(&self) -> (u64, i64) {
        (self.rows.swap(0, Ordering::Relaxed), self.sum.swap(0, Ordering::Relaxed))
    }
}

impl Sink for CheckSink {
    fn push(&mut self, b: BorrowedBatch<'_>) -> Result<Flow> {
        self.fold(b)
    }

    fn leg(&self, _ordinal: u16) -> Option<LegRef<'_>> {
        Some(LegRef::of(self))
    }
}

impl SinkLeg for CheckSink {
    fn push(&self, b: BorrowedBatch<'_>) -> Result<Flow> {
        self.fold(b)
    }
}

fn run(entry: &mut Entry, program: &Arc<Program>, derive: &mut ArrayDerive, pool: &dyn Pool, width: u16, params: &[Datum]) -> (u64, i64) {
    static ABORT: AtomicU8 = AtomicU8::new(0);
    let nulls = [0u8; 16];
    let blocks = |p: usize| {
        let site = program.pipelines.get(p).map_or(-1, |q| q.driving_site);
        if site < 0 {
            0
        } else {
            derive.table(program.sites[site as usize].relation).blocks()
        }
    };
    let plan = ArrayPlan { program: program.clone(), width, cursors: [BlockRange::new(blocks(0), ClaimSize::Fixed(CLAIM)), BlockRange::new(blocks(1), ClaimSize::Fixed(CLAIM))], parts: gravity_parallel::PartitionRange::deferred(), hash_build: HashBuildPartial };
    let mut sink = CheckSink::default();
    let mut b = Binding::new(parts(params, &nulls[..params.len().max(1)], &ABORT), &mut sink);
    let mut exec = entry.start(None, program, derive, &mut b, std::rc::Rc::new(gravity_driver::Reservation::unadmitted(gravity_driver::Grant::of(8 << 20)))).expect("start");
    let r = run_program(entry, &mut exec, &mut b, derive, pool, &plan);
    entry.end(exec, r.is_ok());
    r.expect("run");
    sink.take()
}

fn median(ms: &mut [f64]) -> f64 {
    ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    ms[ms.len() / 2]
}

fn timed<T: Default>(runs: usize, mut f: impl FnMut() -> T) -> (f64, T) {
    let mut out = T::default();
    let mut ms = Vec::new();
    for _ in 0..=runs {
        let t = Instant::now();
        out = f();
        ms.push(t.elapsed().as_secs_f64() * 1e3);
    }
    ms.remove(0);
    (median(&mut ms), out)
}

// ---- the lab loops (a filter-count over a
// decoded granule, fpgroup's per-thread tables merged at the end, a join
// lab's per-thread build maps merged then probed), over the same arrays.

#[inline(always)]
fn mix(h: u64) -> u64 {
    let h = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    let h = (h ^ (h >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^ (h >> 31)
}

fn chunks<'a>(v: &'a [i32], w: usize) -> impl Iterator<Item = &'a [i32]> {
    v.chunks(v.len().div_ceil(w).max(1))
}

fn lab_count(a: &[i32], w: usize, thr: i32) -> (u64, i64) {
    let n: u64 = std::thread::scope(|s| {
        let hs: Vec<_> = chunks(a, w).map(|c| s.spawn(move || c.iter().filter(|&&x| x < thr).count() as u64)).collect();
        hs.into_iter().map(|h| h.join().unwrap()).sum()
    });
    (1, n as i64)
}

#[derive(Clone, Copy, Default)]
struct Slot {
    used: u32,
    key: i32,
    sum: i64,
    cnt: i64,
}

fn table_add(t: &mut [Slot], key: i32, v: i64, n: i64) {
    let mask = t.len() - 1;
    let mut i = mix(key as u32 as u64) as usize & mask;
    loop {
        let s = &mut t[i];
        if s.used == 0 {
            *s = Slot { used: 1, key, sum: v, cnt: n };
            return;
        }
        if s.key == key {
            s.sum += v;
            s.cnt += n;
            return;
        }
        i = (i + 1) & mask;
    }
}

/// The lab's open-addressing table (increment 2's A table): 4096 slots.
const LAB_SLOTS: usize = 4096;
/// The lab's join table: the §1 big build table (2^21 slots).
const LAB_JOIN_CAP: usize = 1 << 21;

fn lab_group(k: &[i32], v: &[i32], w: usize) -> (u64, i64) {
    let n = k.len().div_ceil(w).max(1);
    let tables: Vec<Vec<Slot>> = std::thread::scope(|s| {
        let hs: Vec<_> = k
            .chunks(n)
            .zip(v.chunks(n))
            .map(|(kc, vc)| {
                s.spawn(move || {
                    let mut t = vec![Slot::default(); LAB_SLOTS];
                    for (x, y) in kc.iter().zip(vc) {
                        table_add(&mut t, *x, *y as i64, 1);
                    }
                    t
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut all = vec![Slot::default(); LAB_SLOTS];
    for t in &tables {
        for s in t.iter().filter(|s| s.used != 0) {
            table_add(&mut all, s.key, s.sum, s.cnt);
        }
    }
    (all.iter().filter(|s| s.used != 0).count() as u64, all.iter().filter(|s| s.used != 0).map(|s| s.key as i64 + s.sum + s.cnt).sum())
}

#[derive(Clone, Copy, Default)]
struct Entry2 {
    used: u32,
    key: i32,
    payload: i64,
}

fn build_insert(t: &mut [Entry2], key: i32, payload: i64) {
    let mask = t.len() - 1;
    let mut i = mix(key as u32 as u64) as usize & mask;
    while t[i].used != 0 {
        i = (i + 1) & mask;
    }
    t[i] = Entry2 { used: 1, key, payload };
}

fn lab_join(ik: &[i32], ia: &[i32], ok: &[i32], ob: &[i32], w: usize, cap: usize) -> (u64, i64) {
    let n = ik.len().div_ceil(w).max(1);
    let tables: Vec<Vec<Entry2>> = std::thread::scope(|s| {
        let hs: Vec<_> = ik
            .chunks(n)
            .zip(ia.chunks(n))
            .map(|(kc, ac)| {
                s.spawn(move || {
                    let mut t = vec![Entry2::default(); cap];
                    for (x, y) in kc.iter().zip(ac) {
                        build_insert(&mut t, *x, *y as i64);
                    }
                    t
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut all = vec![Entry2::default(); cap];
    for t in &tables {
        for e in t.iter().filter(|e| e.used != 0) {
            build_insert(&mut all, e.key, e.payload);
        }
    }
    let all = &all;
    let m = ok.len().div_ceil(w).max(1);
    std::thread::scope(|s| {
        let hs: Vec<_> = ok
            .chunks(m)
            .zip(ob.chunks(m))
            .map(|(kc, bc)| {
                s.spawn(move || {
                    let mask = cap - 1;
                    let (mut rows, mut acc) = (0u64, 0i64);
                    for (x, y) in kc.iter().zip(bc) {
                        let mut i = mix(*x as u32 as u64) as usize & mask;
                        while all[i].used != 0 {
                            if all[i].key == *x {
                                rows += 1;
                                acc += *x as i64 + all[i].payload + *y as i64;
                            }
                            i = (i + 1) & mask;
                        }
                    }
                    (rows, acc)
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1))
    })
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("a");
    let (rows, outer, inner) = match mode {
        "tsv" => (args.get(3).and_then(|s| s.parse().ok()).unwrap_or(100_000_000u64), args.get(4).and_then(|s| s.parse().ok()).unwrap_or(10_000_000u64), args.get(5).and_then(|s| s.parse().ok()).unwrap_or(1_000_000u64)),
        _ => (args.get(2).and_then(|s| s.parse().ok()).unwrap_or(100_000_000u64), args.get(3).and_then(|s| s.parse().ok()).unwrap_or(10_000_000u64), args.get(4).and_then(|s| s.parse().ok()).unwrap_or(1_000_000u64)),
    };
    if mode == "tsv" {
        let dir = args.get(2).expect("tsv <dir>");
        let mut w = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(format!("{dir}/t.tsv")).expect("t.tsv"));
        for i in 0..rows {
            let (k, v, a) = t_row(i);
            writeln!(w, "{k}\t{v}\t{a}").unwrap();
        }
        let mut w = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(format!("{dir}/inner.tsv")).expect("inner.tsv"));
        for j in 0..inner {
            let (k, a) = inner_row(j);
            writeln!(w, "{k}\t{a}").unwrap();
        }
        let mut w = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(format!("{dir}/outer.tsv")).expect("outer.tsv"));
        for i in 0..outer {
            let (k, b) = outer_row(i, outer, inner);
            writeln!(w, "{k}\t{b}").unwrap();
        }
        println!("wrote {rows} + {inner} + {outer} rows to {dir}");
        return;
    }
    let widths: Vec<u16> = args.get(5).map(|s| s.split(',').filter_map(|w| w.parse().ok()).collect()).unwrap_or_else(|| vec![1, 4, 8, 12, 16]);
    let runs: usize = std::env::var("RUNS").ok().and_then(|s| s.parse().ok()).unwrap_or(5);
    let t0 = Instant::now();
    let k = fill(rows, |i| t_row(i).0);
    let v = fill(rows, |i| t_row(i).1);
    let a = fill(rows, |i| t_row(i).2);
    let ik = fill(inner, |j| inner_row(j).0);
    let ia = fill(inner, |j| inner_row(j).1);
    let ok = fill(outer, |i| outer_row(i, outer, inner).0);
    let ob = fill(outer, |i| outer_row(i, outer, inner).1);
    eprintln!("arrays: {rows} rows (k, v, a), join {outer} x {inner}, built in {:.1} s", t0.elapsed().as_secs_f64());
    let mut derive = ArrayDerive {
        tables: vec![
            Arc::new(Cols { rel: REL_A, rows, cols: vec![a.clone()] }),
            Arc::new(Cols { rel: REL_KV, rows, cols: vec![k.clone(), v.clone()] }),
            Arc::new(Cols { rel: REL_IN, rows: inner, cols: vec![ik.clone(), ia.clone()] }),
            Arc::new(Cols { rel: REL_OUT, rows: outer, cols: vec![ok.clone(), ob.clone()] }),
        ],
    };
    let maxw = *widths.iter().max().unwrap_or(&1) as usize;
    let pool = FixturePool::new(maxw.max(2) as u16);
    let mut entry = Entry::new(4);
    let count = Arc::new(programs::par_scan_count_with(REL_A, rows));
    let group = Arc::new(programs::par_hashagg_with(REL_KV, 64, rows));
    let join = Arc::new(programs::par_hashjoin_with(REL_IN, REL_OUT, 64, inner, outer));
    println!("| shape | rows | W | driver ms | lab ms | driver / lab | driver answer | lab answer |");
    println!("|---|---|---|---|---|---|---|---|");
    let show = |x: (u64, i64)| format!("{} rows, sum {}", x.0, x.1);
    for &w in &widths {
        let (d, dn) = timed(runs, || run(&mut entry, &count, &mut derive, &pool, w, &[THR as u64]));
        let (l, ln) = timed(runs, || lab_count(&a, w as usize, THR as i32));
        println!("| count-filter | {rows} | {w} | {d:.1} | {l:.1} | {:.2} | {} | {} |", d / l, show(dn), show(ln));
        let (d, dn) = timed(runs, || run(&mut entry, &group, &mut derive, &pool, w, &par_hashagg::DEFAULT_PARAMS));
        let (l, ln) = timed(runs, || lab_group(&k, &v, w as usize));
        println!("| group-by sum,count | {rows} | {w} | {d:.1} | {l:.1} | {:.2} | {} | {} |", d / l, show(dn), show(ln));
        let (d, dn) = timed(runs, || run(&mut entry, &join, &mut derive, &pool, w, &par_hashjoin::DEFAULT_PARAMS));
        let (l, ln) = timed(runs, || lab_join(&ik, &ia, &ok, &ob, w as usize, LAB_JOIN_CAP));
        println!("| hash join | {outer} x {inner} | {w} | {d:.1} | {l:.1} | {:.2} | {} | {} |", d / l, show(dn), show(ln));
    }
}
