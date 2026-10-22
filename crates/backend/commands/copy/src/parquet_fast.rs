//! PARQUET-FAST: the fused page-at-a-time decode of one row group into the
//! pgrc driver's currency — the same `DecodedRg` carve (head fragment,
//! full-grain `RowChunk`s, tail fragment) as `DecodeShared::decode`, built
//! column by column from the fused decoder's typed page runs
//! (`parquet_read::FastRowGroup`) instead of 1024-row `ColumnBatch`es
//! pumped row by row through `raw_cell`.
//!
//! Chunks are filled in row slabs (`SLAB_ROWS` rows): within a slab each
//! column writes its cells straight into the chunk (strided by the row
//! width), so the slab's cells stay cache resident while the columns take
//! turns. Dictionary-coded text writes each referenced dictionary entry's
//! image into a chunk's arena ONCE and points every row at it (`normalize`
//! reads through the span).
//!
//! Always on: the decode worker tries the fused path first for every row
//! group. Any error — an unsupported shape or bad data — falls back to the
//! batch path (`DecodeShared::decode`) for that row group, which is also
//! the error path of record.

use std::sync::atomic::Ordering;

use parquet_read::{FastCol, FastRowGroup, FastRun, FastVals};
use pgrc_write::par::RowChunk;
use types_error::{PgError, PgResult};

use crate::fromparquet::{
    fast_lane_ok, text_image, word_f32, word_f64, word_i32, word_i64, Conv, EpochLane,
};
use crate::parquet_pgrc::{DecodeShared, DecodedRg, Fragment, Lane};

/// Rows per fill slab: the slab's cells of every column stay cache resident
/// while the columns take turns.
const SLAB_ROWS: usize = 256;

fn fast_err(what: &str) -> Box<PgError> {
    Box::new(PgError::error(format!("fastdec: {what}")))
}

// ---------------------------------------------------------------------------
// Destinations
// ---------------------------------------------------------------------------

trait Dst {
    fn word(&mut self, r: u32, w: u64);
    fn null(&mut self, r: u32);
    fn arena(&mut self) -> &mut Vec<u8>;
    fn span(&mut self, r: u32, off: u32, len: u32);
}

/// One column of a fresh chunk reserved for `grain` rows (every row of the
/// column is written by `fill_col` before `fill_commit`).
struct ChunkDst<'a> {
    ch: &'a mut RowChunk,
    col: usize,
}

impl Dst for ChunkDst<'_> {
    #[inline(always)]
    fn word(&mut self, r: u32, w: u64) {
        // SAFETY: r < the reserved grain, col < ncols (fast_ok).
        unsafe { self.ch.fill_word(r, self.col, w) }
    }
    #[inline(always)]
    fn null(&mut self, r: u32) {
        // SAFETY: as word.
        unsafe { self.ch.fill_null(r, self.col) }
    }
    #[inline(always)]
    fn arena(&mut self) -> &mut Vec<u8> {
        self.ch.arena_mut()
    }
    #[inline(always)]
    fn span(&mut self, r: u32, off: u32, len: u32) {
        // SAFETY: as word.
        unsafe { self.ch.fill_bytes(r, self.col, u64::from(off), u64::from(len)) }
    }
}

/// A fragment's word lane (rows arrive in order).
struct WordsDst<'a> {
    words: &'a mut Vec<u64>,
    nulls: &'a mut Vec<bool>,
}

impl Dst for WordsDst<'_> {
    #[inline(always)]
    fn word(&mut self, _r: u32, w: u64) {
        self.words.push(w);
        self.nulls.push(false);
    }
    #[inline(always)]
    fn null(&mut self, _r: u32) {
        self.words.push(0);
        self.nulls.push(true);
    }
    fn arena(&mut self) -> &mut Vec<u8> {
        unreachable!("word lane has no arena (fast_ok checks byval)")
    }
    fn span(&mut self, _r: u32, _off: u32, _len: u32) {
        unreachable!("word lane takes no spans (fast_ok checks byval)")
    }
}

/// A fragment's blob lane (rows arrive in order).
struct BlobsDst<'a> {
    arena: &'a mut Vec<u8>,
    spans: &'a mut Vec<(u32, u32)>,
    nulls: &'a mut Vec<bool>,
}

impl Dst for BlobsDst<'_> {
    fn word(&mut self, _r: u32, _w: u64) {
        unreachable!("blob lane takes no words (fast_ok checks byval)")
    }
    #[inline(always)]
    fn null(&mut self, _r: u32) {
        self.spans.push((0, 0));
        self.nulls.push(true);
    }
    #[inline(always)]
    fn arena(&mut self) -> &mut Vec<u8> {
        self.arena
    }
    #[inline(always)]
    fn span(&mut self, _r: u32, off: u32, len: u32) {
        self.spans.push((off, len));
        self.nulls.push(false);
    }
}

/// Per-column dictionary-image memo: entry id -> (generation, off, len) in
/// the CURRENT destination arena; a new destination bumps the generation.
struct Memo {
    gen: u32,
    slots: Vec<(u32, u32, u32)>,
}

impl Memo {
    fn new() -> Memo {
        Memo { gen: 1, slots: Vec::new() }
    }
    #[inline]
    fn bump(&mut self) {
        self.gen = self.gen.wrapping_add(1);
        if self.gen == 0 {
            self.slots.iter_mut().for_each(|s| s.0 = 0);
            self.gen = 1;
        }
    }
}

// ---------------------------------------------------------------------------
// The fill kernels
// ---------------------------------------------------------------------------

/// Convert one page run into `dst` rows `base..base + run.n`. The match on
/// the conversion is hoisted out of the per-value loops; the common arms
/// are instantiated with a CONSTANT conversion so the shared value-level
/// helper folds to straight-line code.
#[inline(always)]
fn fill_run<D: Dst>(conv: Conv, run: &FastRun<'_>, base: u32, dst: &mut D, memo: &mut Memo) -> PgResult<()> {
    macro_rules! slots {
        (|$j:ident, $r:ident| $body:expr) => {{
            match run.present {
                None => {
                    for $j in 0..run.n {
                        let $r = base + $j as u32;
                        $body;
                    }
                }
                Some(p) => {
                    let mut $j = 0usize;
                    for (s, &pr) in p.iter().enumerate() {
                        let $r = base + s as u32;
                        if pr {
                            $body;
                            $j += 1;
                        } else {
                            dst.null($r);
                        }
                    }
                }
            }
        }};
    }
    macro_rules! words {
        ($v:expr, $f:ident, [$($arm:expr),*]) => {{
            let v = $v;
            match conv {
                $( c if c_eq(c, $arm) => slots!(|j, r| dst.word(r, $f($arm, v[j])?)), )*
                c => slots!(|j, r| dst.word(r, $f(c, v[j])?)),
            }
        }};
    }
    match &run.vals {
        FastVals::I32(v) => words!(
            v,
            word_i32,
            [
                Conv::I32,
                Conv::I16FromI32,
                Conv::I64FromI32,
                Conv::U32AsI64,
                Conv::DateFromEpochDays(EpochLane::I32),
                Conv::DateFromEpochDays(EpochLane::U32),
                Conv::TsFromEpochSecs(EpochLane::I32),
                Conv::TsFromEpochSecs(EpochLane::U32)
            ]
        ),
        FastVals::I64(v) => words!(
            v,
            word_i64,
            [
                Conv::I64,
                Conv::TsFromEpochSecs(EpochLane::I64),
                Conv::TsFromEpochSecs(EpochLane::U64),
                Conv::I32FromI64,
                Conv::I16FromI64
            ]
        ),
        FastVals::F32(v) => slots!(|j, r| dst.word(r, word_f32(conv, v[j])?)),
        FastVals::F64(v) => slots!(|j, r| dst.word(r, word_f64(conv, v[j])?)),
        FastVals::Plain { buf, offs } => {
            let nul_free = run.nul_free;
            match conv {
                Conv::Text => slots!(|j, r| {
                    let b = &buf[offs[j] as usize + 4..offs[j + 1] as usize];
                    let (o, l) = text_image(Conv::Text, b, nul_free, dst.arena())?;
                    dst.span(r, o, l)
                }),
                c => slots!(|j, r| {
                    let b = &buf[offs[j] as usize + 4..offs[j + 1] as usize];
                    let (o, l) = text_image(c, b, nul_free, dst.arena())?;
                    dst.span(r, o, l)
                }),
            }
        }
        FastVals::Dict { ids, offs, arena } => {
            let nul_free = run.nul_free;
            let dlen = offs.len() - 1;
            if memo.slots.len() < dlen {
                ::mcx::stdvec::try_resize(&mut memo.slots, dlen, (0, 0, 0), "parquet_fast")?;
            }
            let gen = memo.gen;
            slots!(|j, r| {
                let id = ids[j] as usize;
                let s = memo.slots[id];
                if s.0 == gen {
                    dst.span(r, s.1, s.2)
                } else {
                    let b = &arena[offs[id] as usize..offs[id + 1] as usize];
                    let (o, l) = text_image(conv, b, nul_free, dst.arena())?;
                    memo.slots[id] = (gen, o, l);
                    dst.span(r, o, l)
                }
            })
        }
    }
    Ok(())
}

/// `Conv` has no PartialEq (it is plan data); compare the arms the fill
/// kernels specialize by discriminant + payload through the debug-free
/// byte image (Conv is one word, `Copy`, no padding-sensitive arms here).
#[inline(always)]
fn c_eq(a: Conv, b: Conv) -> bool {
    match (a, b) {
        (Conv::I32, Conv::I32)
        | (Conv::I16FromI32, Conv::I16FromI32)
        | (Conv::I64FromI32, Conv::I64FromI32)
        | (Conv::U32AsI64, Conv::U32AsI64)
        | (Conv::I64, Conv::I64)
        | (Conv::I32FromI64, Conv::I32FromI64)
        | (Conv::I16FromI64, Conv::I16FromI64) => true,
        (Conv::DateFromEpochDays(x), Conv::DateFromEpochDays(y))
        | (Conv::TsFromEpochSecs(x), Conv::TsFromEpochSecs(y)) => lane_eq(x, y),
        _ => false,
    }
}

#[inline(always)]
fn lane_eq(a: EpochLane, b: EpochLane) -> bool {
    matches!(
        (a, b),
        (EpochLane::I32, EpochLane::I32)
            | (EpochLane::U32, EpochLane::U32)
            | (EpochLane::I64, EpochLane::I64)
            | (EpochLane::U64, EpochLane::U64)
    )
}

/// Fill rows `r0..r0 + n` of one column.
#[inline(always)]
fn fill_col<D: Dst>(
    col: &mut FastCol,
    conv: Conv,
    notnull: bool,
    r0: u32,
    n: usize,
    dst: &mut D,
    memo: &mut Memo,
) -> PgResult<()> {
    let mut done = 0usize;
    while done < n {
        let run = col.next_run(n - done)?;
        if notnull {
            if let Some(p) = run.present {
                if p.iter().any(|&x| !x) {
                    return Err(fast_err("not-null violation (serial replay owns the message)"));
                }
            }
        }
        fill_run(conv, &run, r0 + done as u32, dst, memo)?;
        done += run.n;
    }
    Ok(())
}

fn is_text(conv: Conv) -> bool {
    matches!(conv, Conv::Text | Conv::VarcharN(_) | Conv::Bpchar(_) | Conv::Bytea)
}

impl DecodeShared {
    /// The fused path's admission: every attribute bound exactly once, every
    /// conversion one the fused kernels take, lane shape = byval.
    fn fast_ok(&self) -> PgResult<()> {
        let natts = self.byval.len();
        if self.plan.bindings.len() != natts {
            return Err(fast_err("partial column list"));
        }
        let mut seen = ::mcx::stdvec::try_filled(natts, false, "parquet_fast")?;
        for b in self.plan.bindings.iter() {
            if b.attidx >= natts || seen[b.attidx] {
                return Err(fast_err("binding set"));
            }
            seen[b.attidx] = true;
            let phys = self.meta.columns[self.plan.cols[b.batch]].phys;
            if !fast_lane_ok(b.conv, phys) {
                return Err(fast_err("conversion"));
            }
            if self.byval[b.attidx] == is_text(b.conv) {
                return Err(fast_err("lane shape"));
            }
        }
        Ok(())
    }

    fn fast_open(&self, seq: u64) -> PgResult<FastRowGroup> {
        self.fast_ok()?;
        let rg_idx = self.rg_order[seq as usize];
        let rg = FastRowGroup::open_on(
            &self.file,
            &self.path,
            &self.meta,
            rg_idx,
            &self.plan.cols,
            &self.plan.vutf8,
            self.owner,
        )?;
        Ok(rg)
    }

    fn fill_fragment(&self, rg: &mut FastRowGroup, frag: &mut Fragment, n: usize, memos: &mut [Memo]) -> PgResult<()> {
        // CONVERT W3-2b: the fill's growth is charged once it is in
        let r = self.fill_lanes(rg, frag, n, memos);
        frag.sync_charge();
        r
    }

    fn fill_lanes(&self, rg: &mut FastRowGroup, frag: &mut Fragment, n: usize, memos: &mut [Memo]) -> PgResult<()> {
        if n == 0 {
            return Ok(());
        }
        for b in self.plan.bindings.iter() {
            let memo = &mut memos[b.batch];
            memo.bump();
            let col = &mut rg.cols[b.batch];
            let nn = self.notnull[b.attidx];
            match &mut frag.lanes[b.attidx] {
                Lane::Words { words, nulls } => {
                    fill_col(col, b.conv, nn, 0, n, &mut WordsDst { words, nulls }, memo)?
                }
                Lane::Blobs { arena, spans, nulls } => {
                    fill_col(col, b.conv, nn, 0, n, &mut BlobsDst { arena, spans, nulls }, memo)?
                }
            }
        }
        frag.rows += n as u32;
        Ok(())
    }

    /// [`DecodeShared::decode`] through the fused decoder: the same carve
    /// (head fragment, full-grain chunks, tail fragment), the same chunk
    /// and fragment contents. The caller falls back to `decode` on any
    /// error; the row group's bytes count toward `bytes_read` only on
    /// success (the fallback counts them itself).
    pub(crate) fn decode_fast(&self, seq: u64) -> PgResult<DecodedRg> {
        let mut rg = self.fast_open(seq)?;
        let rows_total = rg.num_rows;
        let natts = self.byval.len();
        let grain = self.grain;
        let base = self.row_base[seq as usize];
        let end = base + rows_total;
        let head_end = end.min(base.div_ceil(grain) * grain);
        let full_end = head_end + (end - head_end) / grain * grain;
        let mut memos: Vec<Memo> = (0..rg.cols.len()).map(|_| Memo::new()).collect();

        let mut chunks: Vec<RowChunk> = ::mcx::stdvec::try_vec(((full_end - head_end) / grain) as usize, "parquet_fast")?;
        let head_n = (head_end - base) as usize;
        let mut head = Fragment::empty(natts, &self.byval, head_n, self.owner)?;
        self.fill_fragment(&mut rg, &mut head, head_n, &mut memos)?;

        let slab = SLAB_ROWS;
        let g = grain as usize;
        let mut r = head_end;
        while r < full_end {
            if self.cancelled.load(Ordering::SeqCst) {
                return Err(fast_err("parquet parallel decode cancelled"));
            }
            let mut ch = RowChunk::with_owner(natts, grain as u32, self.owner);
            ch.fill_reserve(grain as u32);
            memos.iter_mut().for_each(Memo::bump);
            let mut s0 = 0usize;
            while s0 < g {
                let n = slab.min(g - s0);
                for b in self.plan.bindings.iter() {
                    let mut d = ChunkDst { ch: &mut ch, col: b.attidx };
                    fill_col(
                        &mut rg.cols[b.batch],
                        b.conv,
                        self.notnull[b.attidx],
                        s0 as u32,
                        n,
                        &mut d,
                        &mut memos[b.batch],
                    )?;
                }
                s0 += n;
            }
            // SAFETY: every binding (= every column, fast_ok) wrote every
            // row 0..grain of this chunk above.
            unsafe { ch.fill_commit(grain as u32) };
            chunks.push(ch);
            r += grain;
        }

        let tail_n = (end - full_end) as usize;
        let mut tail = Fragment::empty(natts, &self.byval, tail_n, self.owner)?;
        self.fill_fragment(&mut rg, &mut tail, tail_n, &mut memos)?;
        self.bytes_read.fetch_add(rg.compressed_bytes, Ordering::Relaxed);
        Ok(DecodedRg { head, chunks, tail })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use types_core::Oid;

    use super::*;
    use crate::fromparquet::resolve_plan_types;

    /// The pgrcpar fixtures' table (scripts/pgrc-parquet-parallel-e2e.sh):
    /// (name, typid, byval), every column NOT NULL.
    const ATTS: [(&str, Oid, bool); 4] = [("id", 20, true), ("v16", 21, true), ("cat", 25, false), ("payload", 25, false)];

    /// Content digest of one fragment (column-major lanes), order-stable.
    fn frag_digest(f: &Fragment) -> u64 {
        let mut h: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut mix = |x: u64| {
            h = (h ^ x).wrapping_mul(0x0000_0100_0000_01b3).rotate_left(29);
        };
        mix(u64::from(f.rows));
        for l in f.lanes.iter() {
            match l {
                Lane::Words { words, nulls } => {
                    for (w, n) in words.iter().zip(nulls.iter()) {
                        mix(if *n { 0xdead } else { *w });
                    }
                }
                Lane::Blobs { arena, spans, nulls } => {
                    for (s, n) in spans.iter().zip(nulls.iter()) {
                        if *n {
                            mix(0xdead);
                        } else {
                            mix(u64::from(s.1));
                            for c in arena[s.0 as usize..(s.0 + s.1) as usize].chunks(8) {
                                let mut b = [0u8; 8];
                                b[..c.len()].copy_from_slice(c);
                                mix(u64::from_le_bytes(b));
                            }
                        }
                    }
                }
            }
        }
        h
    }

    /// One decoded row group's digest: head, every full-grain chunk in
    /// order, tail.
    fn rg_digest(d: &DecodedRg) -> u64 {
        let mut h = frag_digest(&d.head);
        for c in d.chunks.iter() {
            h = h.rotate_left(7) ^ c.content_digest();
        }
        h.rotate_left(7) ^ frag_digest(&d.tail).rotate_left(3)
    }

    fn shared(path: &str) -> PgResult<DecodeShared> {
        let reader = parquet_read::FileReader::open(path)?;
        let meta = reader.meta_arc();
        let atts: Vec<(String, Oid, i32)> = ATTS.iter().map(|a| (a.0.to_string(), a.1, -1)).collect();
        let plan = Arc::new(resolve_plan_types(&meta, &atts, true)?);
        let file = reader.try_clone_file()?;
        let byval = ATTS.iter().map(|a| a.2).collect();
        Ok(DecodeShared::standalone(file, path.to_string(), meta, plan, byval, vec![true; ATTS.len()]))
    }

    /// Every row group of a file in order through `decode`, folded; the
    /// first error stops the walk (the COPY's rule).
    fn walk(sh: &DecodeShared, decode: impl Fn(&DecodeShared, u64) -> PgResult<DecodedRg>) -> Result<u64, String> {
        let mut fold = 0u64;
        for seq in 0..sh.rg_order.len() as u64 {
            let d = decode(sh, seq).map_err(|e| e.message().to_string())?;
            fold = fold.rotate_left(11) ^ rg_digest(&d);
        }
        Ok(fold)
    }

    /// PARQUET-FAST identity: the fused decode builds the same chunks and
    /// fragments (content digest) as the batch path on every fixture it
    /// takes and fails where the batch path fails; the decode worker's rule
    /// (fused, else batch) yields exactly the batch path's result — rows or
    /// error — and counts each row group's bytes once.
    #[test]
    fn fused_decode_equals_batch_decode_on_the_pgrcpar_fixtures() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../access/parquet_read/testdata/");
        let mut compared = 0;
        for f in [
            "pgrcpar.parquet",
            "pgrcpar-swap.parquet",
            "pgrcpar-valflip.parquet",
            "pgrcpar-poison.parquet",
            "pgrcpar-nullpoison.parquet",
        ] {
            let path = format!("{dir}{f}");
            let Ok(sh) = shared(&path) else {
                continue;
            };
            let old = walk(&sh, |s, q| s.decode(q));
            let new = walk(&sh, |s, q| s.decode_fast(q));
            match (&old, &new) {
                (Ok(o), Ok(n)) => {
                    assert_eq!(o, n, "{f}");
                    compared += 1;
                }
                (Err(_), Err(_)) => {}
                _ => panic!("{f}: batch {old:?} vs fused {new:?}"),
            }
            let batch_only = shared(&path).expect("reopen");
            let worker = shared(&path).expect("reopen");
            assert_eq!(walk(&batch_only, |s, q| s.decode(q)), old, "{f}");
            assert_eq!(walk(&worker, |s, q| s.decode_fast(q).or_else(|_| s.decode(q))), old, "{f} worker rule");
            if old.is_ok() {
                assert_eq!(
                    worker.bytes_read.load(Ordering::Relaxed),
                    batch_only.bytes_read.load(Ordering::Relaxed),
                    "{f} bytes_read"
                );
            }
        }
        assert!(compared >= 3, "fixtures compared: {compared}");
    }
}
