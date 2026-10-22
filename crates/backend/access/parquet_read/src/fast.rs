//! The fused page-at-a-time parquet decoder (PARQUET-FAST, landed for
//! COPY FROM parquet into pgrcolumnar). A page is decompressed and its
//! levels turned into a presence mask when the consumer first reaches it;
//! the values are then decoded PER RUN, straight from the page cursors
//! into run-sized buffers reused across runs (dictionary indices through
//! the hybrid RLE / bit-packed stream, PLAIN BYTE_ARRAY length-walked into
//! an offset lane with the UTF-8 / NUL screen over the run's bytes). The
//! consumer is handed typed views of the run ([`FastRun`]) and converts
//! straight into its own destination — no `ColumnBatch`, no per-cell
//! dispatch, string bytes are never copied before the consumer copies
//! them once into its arena.
//!
//! Scope: flat leaves (`max_rep == 0`) of INT32 / INT64 / FLOAT / DOUBLE /
//! BYTE_ARRAY under PLAIN and PLAIN_/RLE_DICTIONARY, data pages v1 and v2,
//! RLE definition levels. Anything else is refused at open or at the page
//! (an `Err`), and the caller falls back to the [`crate::RowGroupReader`]
//! path for the row group — which is also the error path of record.

use std::fs::File;

use types_error::{PgError, PgResult};

use crate::codec::{crc32, crc_enabled, decompress_page};
use crate::column::ChunkBytes;
use crate::held::{cap_bytes, HeldCharge};
use crate::levels::decode_levels;
use crate::meta::{CodecId, FileMeta, Phys};
use crate::page::{parse_page_header, Enc, PageKind};
use crate::plain::{invalid_utf8, utf8_ok};
use crate::rle::{corrupt_page, HybridState};

fn unsupported_fast(what: &str) -> Box<PgError> {
    Box::new(PgError::error(format!("fastdec: unsupported {what}"))) // alloc: Cold
}

fn oom() -> Box<PgError> {
    crate::oom_in("decoding parquet column")
}

/// The present values of one run, dense (nulls carry no entry).
pub enum FastVals<'a> {
    I32(&'a [i32]),
    I64(&'a [i64]),
    F32(&'a [f32]),
    F64(&'a [f64]),
    /// PLAIN BYTE_ARRAY: value j = `buf[offs[j] + 4 .. offs[j + 1]]`
    /// (`offs` holds one more entry than the run has values).
    Plain { buf: &'a [u8], offs: &'a [u32] },
    /// Dictionary BYTE_ARRAY: value j = `arena[offs[ids[j]] .. offs[ids[j] + 1]]`
    /// (`ids` already bound-checked against the dictionary).
    Dict { ids: &'a [u32], offs: &'a [u32], arena: &'a [u8] },
}

/// One contiguous slot run of a page.
pub struct FastRun<'a> {
    /// Slots in the run.
    pub n: usize,
    /// `None` = every slot present; else one flag per slot (true = present).
    pub present: Option<&'a [bool]>,
    pub vals: FastVals<'a>,
    /// Byte arrays: no 0x00 byte in any value this run can reference.
    pub nul_free: bool,
}

enum FDict {
    I32(Vec<i32>),
    I64(Vec<i64>),
    F32(Vec<f32>),
    F64(Vec<f64>),
    /// `offs[0] = 0`, value i = `arena[offs[i]..offs[i + 1]]`.
    Bytes { offs: Vec<u32>, arena: Vec<u8>, nul_free: bool },
}

impl FDict {
    fn len(&self) -> usize {
        match self {
            FDict::I32(v) => v.len(),
            FDict::I64(v) => v.len(),
            FDict::F32(v) => v.len(),
            FDict::F64(v) => v.len(),
            FDict::Bytes { offs, .. } => offs.len() - 1,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PgKind {
    None,
    /// The value encoding of the open page (values decode per run).
    PlainFixed,
    DictFixed,
    PlainBytes,
    DictBytes,
}

/// Fused per-column-chunk decoder.
pub struct FastCol {
    /// CONVERT wave 3 (cvwrite job 2): the loading statement's charge on [`FastCol::held_bytes`] (the page, levels,
    /// dictionary and run buffers; the chunk is the row group's), synced after a page opens and at each run, on the
    /// decode thread. Declared first: it drops before the blocks are freed.
    charge: HeldCharge,
    chunk: ChunkBytes,
    chunk_len: usize,
    codec: CodecId,
    phys: Phys,
    max_def: u16,
    validate_utf8: bool,
    name: String,
    /// Slots remaining across the chunk.
    values_left: u64,
    walk: usize,
    dict: Option<FDict>,
    scratch: Vec<u8>,
    /// Current page: values live in the chunk (true) or `scratch`.
    src_chunk: bool,
    kind: PgKind,
    pg_n: usize,
    pg_pos: usize,
    has_nulls: bool,
    present: Vec<bool>,
    lv: Vec<u16>,
    ids: Vec<u32>,
    offs: Vec<u32>,
    i32s: Vec<i32>,
    i64s: Vec<i64>,
    f32s: Vec<f32>,
    f64s: Vec<f64>,
    /// Page cursors: the next value byte (PLAIN) and the page's value end.
    vpos: usize,
    vend: usize,
    /// The page's dictionary-index stream (dictionary-encoded pages).
    hyb: Option<HybridState>,
}

/// One row group's fused column decoders, in requested-column order.
pub struct FastRowGroup {
    /// CONVERT wave 3 (cvwrite job 2): the loading statement's charge on the row group's column chunks. Declared
    /// first: it drops before the columns free the chunks.
    chunk_charge: HeldCharge,
    pub cols: Vec<FastCol>,
    pub num_rows: u64,
    pub compressed_bytes: u64,
}

impl FastRowGroup {
    pub fn open(
        file: &File,
        path: &str,
        meta: &FileMeta,
        rg_idx: usize,
        columns: &[usize],
        validate_utf8: &[bool],
    ) -> PgResult<FastRowGroup> {
        FastRowGroup::open_on(file, path, meta, rg_idx, columns, validate_utf8, None)
    }

    /// CONVERT wave 3 (cvwrite job 2): [`FastRowGroup::open`] with the decoder's buffers (the row group's chunks,
    /// every column's page, levels, dictionary and run buffers) charged to `owner` (the loading statement's), kind
    /// `Region`; `None`: uncharged.
    pub fn open_on(
        file: &File,
        path: &str,
        meta: &FileMeta,
        rg_idx: usize,
        columns: &[usize],
        validate_utf8: &[bool],
        owner: Option<memledger::Owner>,
    ) -> PgResult<FastRowGroup> {
        // Shape refusals first (before any read).
        for &c in columns {
            let s = &meta.columns[c];
            if s.max_rep > 0 {
                return Err(unsupported_fast("repeated leaf"));
            }
            match s.phys {
                Phys::Int32 | Phys::Int64 | Phys::Float | Phys::Double | Phys::ByteArray => {}
                _ => return Err(unsupported_fast("physical type")),
            }
        }
        let mut chunk_charge = HeldCharge::new(owner);
        let (mut loaded, compressed_bytes) = crate::load_rg_chunks(file, path, meta, rg_idx, columns, &mut chunk_charge)?;
        let mut cols = Vec::new();
        cols.try_reserve_exact(loaded.len()).map_err(|_| oom())?;
        // memsig E-pub t5: the chunks move out of their charged list (lib.rs `LoadedChunks`) into the columns
        loaded.with_vec(|loaded| for ((ch, bytes), &vutf8) in loaded.drain(..).zip(validate_utf8.iter()) {
            let schema = &meta.columns[ch.column];
            let chunk_len = bytes.bytes().len();
            cols.push(FastCol {
                charge: HeldCharge::new(owner),
                chunk: bytes,
                chunk_len,
                codec: ch.codec,
                phys: schema.phys,
                max_def: schema.max_def,
                validate_utf8: vutf8,
                name: schema.name.clone(),
                values_left: ch.num_values as u64,
                walk: 0,
                dict: None,
                scratch: Vec::new(),
                src_chunk: false,
                kind: PgKind::None,
                pg_n: 0,
                pg_pos: 0,
                has_nulls: false,
                present: Vec::new(),
                lv: Vec::new(),
                ids: Vec::new(),
                offs: Vec::new(),
                i32s: Vec::new(),
                i64s: Vec::new(),
                f32s: Vec::new(),
                f64s: Vec::new(),
                vpos: 0,
                vend: 0,
                hyb: None,
            });
        });
        drop(loaded);
        chunk_charge.sync(crate::chunks_held(cols.iter().map(|c| &c.chunk)));
        Ok(FastRowGroup {
            chunk_charge,
            cols,
            num_rows: meta.row_groups[rg_idx].num_rows as u64,
            compressed_bytes,
        })
    }
}

// ---------------------------------------------------------------------------
// Kernels
// ---------------------------------------------------------------------------

/// Per-byte zero mask of a u32 (0x80 in each zero byte), exact.
#[inline(always)]
fn zero_bytes32(x: u32) -> u32 {
    (!(((x & 0x7f7f_7f7f).wrapping_add(0x7f7f_7f7f)) | x | 0x7f7f_7f7f)).count_ones()
}

/// Page-wide screen: (any byte >= 0x80, count of 0x00 bytes).
fn screen(region: &[u8]) -> (bool, usize) {
    let mut hi: u64 = 0;
    let mut zeros: usize = 0;
    let mut it = region.chunks_exact(8);
    for c in it.by_ref() {
        let x = u64::from_le_bytes(c.try_into().expect("8"));
        hi |= x & 0x8080_8080_8080_8080;
        let z = !(((x & 0x7f7f_7f7f_7f7f_7f7f).wrapping_add(0x7f7f_7f7f_7f7f_7f7f)) | x | 0x7f7f_7f7f_7f7f_7f7f);
        zeros += z.count_ones() as usize;
    }
    for &b in it.remainder() {
        hi |= u64::from(b & 0x80);
        zeros += (b == 0) as usize;
    }
    (hi != 0, zeros)
}

/// PLAIN BYTE_ARRAY length walk over `n` values from `pos`: `offs` gets
/// n + 1 prefix positions (value j = buf[offs[j]+4 .. offs[j+1]]).
/// Returns the zero bytes counted inside the length prefixes.
fn walk_lengths(buf: &[u8], pos: usize, end: usize, n: usize, offs: &mut Vec<u32>) -> PgResult<usize> {
    offs.clear();
    offs.try_reserve(n + 1).map_err(|_| oom())?;
    if end > buf.len() || end > u32::MAX as usize {
        return Err(corrupt_page("string values end before the declared count"));
    }
    let mut p = pos;
    let mut zeros = 0usize;
    // SAFETY: capacity n + 1 reserved; every slot written before set_len.
    unsafe {
        let dst = offs.as_mut_ptr();
        for j in 0..n {
            if p + 4 > end {
                return Err(corrupt_page("string values end before the declared count"));
            }
            *dst.add(j) = p as u32;
            let len = u32::from_le_bytes([buf[p], buf[p + 1], buf[p + 2], buf[p + 3]]);
            zeros += zero_bytes32(len) as usize;
            let next = p + 4 + len as usize;
            if next > end {
                return Err(corrupt_page("string value overruns page"));
            }
            p = next;
        }
        *dst.add(n) = p as u32;
        offs.set_len(n + 1);
    }
    Ok(zeros)
}

#[inline]
fn copy_le<T: Copy>(src: &[u8], n: usize, out: &mut Vec<T>) -> PgResult<()> {
    let w = core::mem::size_of::<T>();
    let need = n.checked_mul(w).ok_or_else(|| corrupt_page("value count overflow"))?;
    if need > src.len() {
        return Err(corrupt_page("values end before the declared count"));
    }
    out.clear();
    out.try_reserve(n).map_err(|_| oom())?; // alloc: Exec charged the cursor's charge (ColumnCursor / FastCol held_bytes)
    #[cfg(target_endian = "little")]
    // SAFETY: `need` bytes of initialized source into reserved capacity;
    // T is a plain little-endian numeric type on a little-endian target.
    unsafe {
        core::ptr::copy_nonoverlapping(src.as_ptr(), out.as_mut_ptr() as *mut u8, need);
        out.set_len(n);
    }
    #[cfg(not(target_endian = "little"))]
    compile_error!("fastdec: little-endian targets only (experiment)");
    Ok(())
}

impl FastCol {
    /// CONVERT wave 3: the heap bytes the column holds besides its chunk (capacity): the decompressed page, the
    /// levels and presence, the dictionary and the run buffers.
    pub(crate) fn held_bytes(&self) -> u64 {
        let dict = match &self.dict {
            None => 0,
            Some(FDict::I32(v)) => cap_bytes(v.capacity(), v),
            Some(FDict::I64(v)) => cap_bytes(v.capacity(), v),
            Some(FDict::F32(v)) => cap_bytes(v.capacity(), v),
            Some(FDict::F64(v)) => cap_bytes(v.capacity(), v),
            Some(FDict::Bytes { offs, arena, .. }) => cap_bytes(offs.capacity(), offs) + arena.capacity() as u64,
        };
        dict + self.scratch.capacity() as u64
            + self.present.capacity() as u64
            + cap_bytes(self.lv.capacity(), &self.lv)
            + cap_bytes(self.ids.capacity(), &self.ids)
            + cap_bytes(self.offs.capacity(), &self.offs)
            + cap_bytes(self.i32s.capacity(), &self.i32s)
            + cap_bytes(self.i64s.capacity(), &self.i64s)
            + cap_bytes(self.f32s.capacity(), &self.f32s)
            + cap_bytes(self.f64s.capacity(), &self.f64s)
    }

    #[inline]
    fn sync_charge(&mut self) {
        let held = self.held_bytes();
        self.charge.sync(held);
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn phys(&self) -> Phys {
        self.phys
    }

    #[inline]
    fn page_buf(&self) -> &[u8] {
        if self.src_chunk {
            self.chunk.bytes()
        } else {
            &self.scratch
        }
    }

    /// Up to `max` slots of the current page (opening the next page when
    /// the current one is spent). Never returns an empty run.
    #[inline]
    pub fn next_run(&mut self, max: usize) -> PgResult<FastRun<'_>> {
        if self.pg_pos == self.pg_n {
            self.open_page()?;
        }
        // CONVERT wave 3: the page just opened and the previous run's buffer growth (one run behind at most)
        self.sync_charge();
        let n = max.min(self.pg_n - self.pg_pos);
        let at = self.pg_pos;
        let nv = if self.has_nulls {
            self.present[at..at + n].iter().map(|&b| b as usize).sum::<usize>()
        } else {
            n
        };
        if n as u64 > self.values_left {
            return Err(corrupt_page("pages hold more values than the chunk declares"));
        }
        self.pg_pos += n;
        self.values_left -= n as u64;
        self.decode_run(n, at, nv)
    }

    /// Decode exactly the run's `nv` present values (slots `at..at + n` of
    /// the page) from the page cursors into run-sized buffers (reused
    /// across runs: no per-page lane allocation).
    fn decode_run(&mut self, n: usize, at: usize, nv: usize) -> PgResult<FastRun<'_>> {
        let FastCol {
            chunk, scratch, src_chunk, kind, vpos, vend, hyb, dict, ids, offs, i32s, i64s, f32s, f64s,
            phys, validate_utf8, name, present, has_nulls, ..
        } = self;
        let buf: &[u8] = if *src_chunk { chunk.bytes() } else { scratch };
        let present: Option<&[bool]> = if *has_nulls { Some(&present[at..at + n]) } else { None };
        let mut nul_free = false;
        let vals = match *kind {
            PgKind::PlainFixed => {
                let src = &buf[*vpos..*vend];
                match *phys {
                    Phys::Int32 => {
                        copy_le(src, nv, i32s)?;
                        *vpos += nv * 4;
                        FastVals::I32(i32s)
                    }
                    Phys::Int64 => {
                        copy_le(src, nv, i64s)?;
                        *vpos += nv * 8;
                        FastVals::I64(i64s)
                    }
                    Phys::Float => {
                        copy_le(src, nv, f32s)?;
                        *vpos += nv * 4;
                        FastVals::F32(f32s)
                    }
                    _ => {
                        copy_le(src, nv, f64s)?;
                        *vpos += nv * 8;
                        FastVals::F64(f64s)
                    }
                }
            }
            PgKind::DictFixed | PgKind::DictBytes => {
                let st = hyb.as_mut().expect("dictionary page stream");
                ids.clear();
                ids.try_reserve(nv).map_err(|_| oom())?; // alloc: Exec charged the cursor's charge (ColumnCursor / FastCol held_bytes)
                ids.resize(nv, 0); // alloc: Exec charged the cursor's charge (ColumnCursor / FastCol held_bytes)
                st.fill(buf, ids)?;
                let d = dict.as_ref().expect("dictionary page without a dictionary");
                let dlen = d.len() as u32;
                if ids.iter().fold(false, |a, &i| a | (i >= dlen)) {
                    return Err(corrupt_page("dictionary index out of range"));
                }
                match d {
                    FDict::I32(dv) => {
                        i32s.clear();
                        i32s.extend(ids.iter().map(|&i| dv[i as usize])); // alloc: Exec charged the cursor's charge (ColumnCursor / FastCol held_bytes)
                        FastVals::I32(i32s)
                    }
                    FDict::I64(dv) => {
                        i64s.clear();
                        i64s.extend(ids.iter().map(|&i| dv[i as usize])); // alloc: Exec charged the cursor's charge (ColumnCursor / FastCol held_bytes)
                        FastVals::I64(i64s)
                    }
                    FDict::F32(dv) => {
                        f32s.clear();
                        f32s.extend(ids.iter().map(|&i| dv[i as usize])); // alloc: Exec charged the cursor's charge (ColumnCursor / FastCol held_bytes)
                        FastVals::F32(f32s)
                    }
                    FDict::F64(dv) => {
                        f64s.clear();
                        f64s.extend(ids.iter().map(|&i| dv[i as usize])); // alloc: Exec charged the cursor's charge (ColumnCursor / FastCol held_bytes)
                        FastVals::F64(f64s)
                    }
                    FDict::Bytes { offs: doffs, arena, nul_free: nf } => {
                        nul_free = *nf;
                        FastVals::Dict { ids, offs: doffs, arena }
                    }
                }
            }
            PgKind::PlainBytes => {
                let prefix_zeros = walk_lengths(buf, *vpos, *vend, nv, offs)?;
                let end = offs[nv] as usize;
                let (hi, zeros) = screen(&buf[*vpos..end]);
                nul_free = zeros == prefix_zeros;
                if *validate_utf8 && hi {
                    for j in 0..nv {
                        if !utf8_ok(&buf[offs[j] as usize + 4..offs[j + 1] as usize]) {
                            return Err(invalid_utf8(name));
                        }
                    }
                }
                *vpos = end;
                FastVals::Plain { buf, offs }
            }
            _ => unreachable!("run from an unopened page"),
        };
        Ok(FastRun { n, present, vals, nul_free })
    }

    /// Walk to the next data page and open it (levels decoded, value
    /// cursors set).
    fn open_page(&mut self) -> PgResult<()> {
        loop {
            if self.values_left == 0 {
                return Err(corrupt_page("column chunk ends before the declared row count"));
            }
            if self.walk >= self.chunk_len {
                return Err(corrupt_page("column chunk ends before the declared value count"));
            }
            let head = parse_page_header(&self.chunk.bytes()[..self.chunk_len], self.walk, &self.name)?;
            let ps = head.payload_at;
            let pe = ps + head.compressed_size;
            if pe > self.chunk_len {
                return Err(corrupt_page("page payload overruns chunk"));
            }
            self.walk = pe;
            if let Some(want) = head.crc {
                if crc_enabled() && crc32(&self.chunk.bytes()[ps..pe]) != want {
                    return Err(corrupt_page("page crc32 mismatch"));
                }
            }
            match head.kind {
                PageKind::Skip => continue,
                PageKind::Dict { num_values, encoding } => {
                    if self.dict.is_some() {
                        return Err(corrupt_page("second dictionary page in one column chunk"));
                    }
                    if !matches!(encoding, Enc::Plain | Enc::PlainDictionary) {
                        return Err(unsupported_fast("dictionary page encoding"));
                    }
                    let (in_chunk, s, e) = self.payload(ps, head.compressed_size, head.uncompressed_size)?;
                    self.src_chunk = in_chunk;
                    let d = self.decode_dict(s, e, num_values)?;
                    self.dict = Some(d);
                    continue;
                }
                PageKind::Data { num_values, encoding, def_encoding, rep_encoding: _ } => {
                    let (in_chunk, s, e) = self.payload(ps, head.compressed_size, head.uncompressed_size)?;
                    self.src_chunk = in_chunk;
                    let mut vpos = s;
                    if self.max_def > 0 {
                        if def_encoding != Enc::Rle {
                            return Err(unsupported_fast("level encoding"));
                        }
                        let buf = self.page_buf();
                        let Some(lb) = buf.get(vpos..vpos + 4).filter(|_| vpos + 4 <= e) else {
                            return Err(corrupt_page("levels overrun page"));
                        };
                        let len = u32::from_le_bytes(lb.try_into().expect("4")) as usize;
                        if vpos + 4 + len > e {
                            return Err(corrupt_page("levels overrun page"));
                        }
                        self.levels(vpos + 4, vpos + 4 + len, num_values, false)?;
                        vpos += 4 + len;
                    } else {
                        self.has_nulls = false;
                    }
                    self.decode_values(vpos, e, encoding, num_values)?;
                    return Ok(());
                }
                PageKind::DataV2 { num_values, num_nulls, num_rows, encoding, def_len, rep_len, compressed } => {
                    if num_rows != num_values {
                        return Err(corrupt_page("data page v2 row count disagrees with its value count"));
                    }
                    let p = ps + rep_len;
                    if self.max_def > 0 {
                        self.src_chunk = true;
                        self.levels(p, p + def_len, num_values, true)?;
                        let present = if self.has_nulls {
                            self.present.iter().filter(|&&b| b).count()
                        } else {
                            num_values
                        };
                        if self.max_def == 1 && present + num_nulls != num_values {
                            return Err(corrupt_page("data page v2 null count disagrees with its levels"));
                        }
                    } else {
                        if num_nulls != 0 {
                            return Err(corrupt_page("nulls declared on a required column"));
                        }
                        self.has_nulls = false;
                    }
                    let p = p + def_len;
                    let vcomp = head.compressed_size - def_len - rep_len;
                    let vuncomp = head.uncompressed_size - def_len - rep_len;
                    let (s, e) = if vcomp == 0 && vuncomp == 0 {
                        self.src_chunk = true;
                        (p, p)
                    } else if compressed && self.codec != CodecId::Uncompressed {
                        let src = self
                            .chunk
                            .bytes()
                            .get(p..p + vcomp)
                            .ok_or_else(|| corrupt_page("page payload overruns chunk"))?;
                        decompress_page(self.codec, src, &mut self.scratch, vuncomp)?;
                        self.src_chunk = false;
                        (0, vuncomp)
                    } else {
                        if vcomp != vuncomp {
                            return Err(corrupt_page("uncompressed page sizes disagree"));
                        }
                        self.src_chunk = true;
                        (p, p + vcomp)
                    };
                    self.decode_values(s, e, encoding, num_values)?;
                    return Ok(());
                }
            }
        }
    }

    /// (values in chunk?, start, end) of a v1 / dictionary page payload.
    fn payload(&mut self, start: usize, compressed: usize, uncompressed: usize) -> PgResult<(bool, usize, usize)> {
        if self.codec == CodecId::Uncompressed {
            if compressed != uncompressed {
                return Err(corrupt_page("uncompressed page sizes disagree"));
            }
            return Ok((true, start, start + compressed));
        }
        let src = self
            .chunk
            .bytes()
            .get(start..start + compressed)
            .ok_or_else(|| corrupt_page("page payload overruns chunk"))?;
        decompress_page(self.codec, src, &mut self.scratch, uncompressed)?;
        Ok((false, 0, uncompressed))
    }

    /// Definition levels of the page -> presence mask (`has_nulls` false
    /// when every slot is present: no mask is kept).
    fn levels(&mut self, s: usize, e: usize, n: usize, in_chunk: bool) -> PgResult<()> {
        let FastCol { chunk, scratch, lv, present, max_def, has_nulls, src_chunk, .. } = self;
        let buf: &[u8] = if in_chunk || *src_chunk { chunk.bytes() } else { scratch };
        decode_levels(Enc::Rle, buf, s, e, *max_def, n, lv)?;
        let md = *max_def;
        let all = lv.iter().fold(true, |a, &d| a & (d == md));
        *has_nulls = !all;
        if !all {
            present.clear();
            present.extend(lv.iter().map(|&d| d == md)); // alloc: Exec charged the cursor's charge (ColumnCursor / FastCol held_bytes)
        }
        Ok(())
    }

    /// Page open: the value cursors only (the values decode per run).
    fn decode_values(&mut self, s: usize, e: usize, encoding: Enc, n: usize) -> PgResult<()> {
        let nv = if self.has_nulls { self.present.iter().filter(|&&b| b).count() } else { n };
        self.pg_n = n;
        self.pg_pos = 0;
        self.vpos = s;
        self.vend = e;
        self.hyb = None;
        match encoding {
            Enc::Plain => match self.phys {
                Phys::Int32 | Phys::Int64 | Phys::Float | Phys::Double => {
                    let w = if matches!(self.phys, Phys::Int32 | Phys::Float) { 4 } else { 8 };
                    if nv.checked_mul(w).is_none_or(|need| s + need > e) {
                        return Err(corrupt_page("values end before the declared count"));
                    }
                    self.kind = PgKind::PlainFixed;
                }
                Phys::ByteArray => self.kind = PgKind::PlainBytes,
                _ => return Err(unsupported_fast("physical type")),
            },
            Enc::PlainDictionary | Enc::RleDictionary => {
                let buf = self.page_buf();
                let Some(&w) = buf.get(s).filter(|_| s < e) else {
                    return Err(corrupt_page("dictionary page bit width missing"));
                };
                if w > 32 {
                    return Err(corrupt_page("dictionary bit width above 32"));
                }
                let d = self
                    .dict
                    .as_ref()
                    .ok_or_else(|| corrupt_page("dictionary-encoded page without a dictionary"))?;
                self.kind = if matches!(d, FDict::Bytes { .. }) { PgKind::DictBytes } else { PgKind::DictFixed };
                self.hyb = Some(HybridState::new(s + 1, e, w as usize)?);
            }
            _ => return Err(unsupported_fast("value encoding")),
        }
        Ok(())
    }

    fn decode_dict(&mut self, s: usize, e: usize, n: usize) -> PgResult<FDict> {
        let buf = self.page_buf();
        Ok(match self.phys {
            Phys::Int32 => {
                let mut v = Vec::new();
                copy_le(&buf[s..e], n, &mut v)?;
                FDict::I32(v)
            }
            Phys::Int64 => {
                let mut v = Vec::new();
                copy_le(&buf[s..e], n, &mut v)?;
                FDict::I64(v)
            }
            Phys::Float => {
                let mut v = Vec::new();
                copy_le(&buf[s..e], n, &mut v)?;
                FDict::F32(v)
            }
            Phys::Double => {
                let mut v = Vec::new();
                copy_le(&buf[s..e], n, &mut v)?;
                FDict::F64(v)
            }
            Phys::ByteArray => {
                let mut pre = Vec::new();
                walk_lengths(buf, s, e, n, &mut pre)?;
                let mut offs = Vec::new();
                offs.try_reserve_exact(n + 1).map_err(|_| oom())?; // alloc: Exec charged the cursor's charge (ColumnCursor / FastCol held_bytes)
                let mut arena = Vec::new();
                arena.try_reserve(pre[n] as usize - s).map_err(|_| oom())?; // alloc: Exec charged the cursor's charge (ColumnCursor / FastCol held_bytes)
                offs.push(0u32); // alloc: Exec charged the cursor's charge (ColumnCursor / FastCol held_bytes)
                for j in 0..n {
                    arena.extend_from_slice(&buf[pre[j] as usize + 4..pre[j + 1] as usize]); // alloc: Exec charged the cursor's charge (ColumnCursor / FastCol held_bytes)
                    offs.push(arena.len() as u32); // alloc: Exec charged the cursor's charge (ColumnCursor / FastCol held_bytes)
                }
                if self.validate_utf8 {
                    // Batched, once per chunk (the old reader's rule): the
                    // concatenation valid + every value starting on a
                    // character boundary.
                    if !utf8_ok(&arena) {
                        return Err(invalid_utf8(&self.name));
                    }
                    let mut bad = false;
                    for j in 0..n {
                        let st = offs[j] as usize;
                        if st < offs[j + 1] as usize {
                            bad |= (arena[st] & 0xc0) == 0x80;
                        }
                    }
                    if bad {
                        return Err(invalid_utf8(&self.name));
                    }
                }
                let nul_free = !arena.contains(&0);
                FDict::Bytes { offs, arena, nul_free }
            }
            _ => return Err(unsupported_fast("dictionary physical type")),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_byte_counts() {
        assert_eq!(zero_bytes32(0), 4);
        assert_eq!(zero_bytes32(5), 3);
        assert_eq!(zero_bytes32(0x0100), 3);
        assert_eq!(zero_bytes32(0x8080_8080), 0);
        assert_eq!(zero_bytes32(0x0001_0000), 3);
        let (hi, z) = screen(&[1, 0, 0x80, 0, 5, 6, 7, 8, 9, 0]);
        assert!(hi);
        assert_eq!(z, 3);
    }

    /// One present slot of a fused run, rendered (floats as bit patterns,
    /// bytes as hex); `nul_free` runs must hold no 0x00 byte.
    fn render_fast(run: &FastRun<'_>, j: usize) -> String {
        let bytes = |b: &[u8]| {
            assert!(!run.nul_free || !b.contains(&0), "nul_free run holds a 0x00 byte");
            format!("x{b:02x?}")
        };
        match &run.vals {
            FastVals::I32(v) => format!("i{}", v[j]),
            FastVals::I64(v) => format!("l{}", v[j]),
            FastVals::F32(v) => format!("f{:08x}", v[j].to_bits()),
            FastVals::F64(v) => format!("d{:016x}", v[j].to_bits()),
            FastVals::Plain { buf, offs } => bytes(&buf[offs[j] as usize + 4..offs[j + 1] as usize]),
            FastVals::Dict { ids, offs, arena } => {
                let id = ids[j] as usize;
                bytes(&arena[offs[id] as usize..offs[id + 1] as usize])
            }
        }
    }

    fn render_batch(b: &crate::ColumnBatch, k: usize) -> String {
        use crate::BatchData;
        match &b.data {
            BatchData::I32(v) => format!("i{}", v[k]),
            BatchData::I64(v) => format!("l{}", v[k]),
            BatchData::F32(v) => format!("f{:08x}", v[k].to_bits()),
            BatchData::F64(v) => format!("d{:016x}", v[k].to_bits()),
            BatchData::Bytes { .. } => format!("x{:02x?}", b.bytes_at(k)),
            // Types the fused decoder refuses at open (never compared).
            BatchData::Bool(v) => format!("b{}", v[k]),
            BatchData::I96(v) => format!("t{:?}", v[k]),
        }
    }

    /// Every fixture, every row group, every leaf the fused decoder opens:
    /// its runs (at two run lengths) render slot for slot what the batch
    /// reader reads, and a fused decode that completes never passes a row
    /// group the batch reader refuses (the COPY falls back to the batch
    /// path on any fused error, so the batch reader stays the error path
    /// of record).
    #[test]
    fn fused_runs_equal_the_batch_reader_on_every_fixture() {
        use crate::meta::Logical;
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata");
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("testdata")
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.ends_with(".parquet"))
            .collect();
        names.sort();
        let (mut fused_rgs, mut files) = (0usize, 0usize);
        for name in &names {
            let path = format!("{dir}/{name}");
            let Ok(mut fr) = crate::FileReader::open(&path) else { continue };
            files += 1;
            let meta = fr.meta_arc();
            let file = fr.try_clone_file().expect("clone file");
            for c in 0..meta.columns.len() {
                let schema = &meta.columns[c];
                let vutf8 = [schema.phys == Phys::ByteArray
                    && matches!(schema.logical, Logical::String | Logical::Enum | Logical::Json)];
                for rg in 0..meta.row_groups.len() {
                    // The batch reader's slots (None = it refuses the row group).
                    let want: Option<Vec<Option<String>>> = (|| {
                        let mut rgr = fr.row_group(rg, &[c], &vutf8).ok()?;
                        let mut batches = vec![crate::ColumnBatch::new_for(schema.phys)];
                        let mut out = Vec::new();
                        while rgr.rows_remaining() > 0 {
                            let n = rgr.rows_remaining().min(1000) as usize;
                            rgr.read_batches(&mut batches, n).ok()?;
                            let b = &batches[0];
                            for k in 0..b.len() {
                                out.push((!b.is_null(k)).then(|| render_batch(b, k)));
                            }
                        }
                        Some(out)
                    })();
                    for step in [1000usize, 7] {
                        let Ok(mut frg) = FastRowGroup::open(&file, &path, &meta, rg, &[c], &vutf8) else { continue };
                        let rows = frg.num_rows as usize;
                        let col = &mut frg.cols[0];
                        let mut got: Vec<Option<String>> = Vec::with_capacity(rows);
                        let mut ok = true;
                        while got.len() < rows {
                            let Ok(run) = col.next_run(step.min(rows - got.len())) else {
                                ok = false;
                                break;
                            };
                            let mut j = 0usize;
                            for s in 0..run.n {
                                if run.present.is_none_or(|p| p[s]) {
                                    got.push(Some(render_fast(&run, j)));
                                    j += 1;
                                } else {
                                    got.push(None);
                                }
                            }
                        }
                        if !ok {
                            continue;
                        }
                        let Some(want) = &want else {
                            panic!("{name} rg {rg} col {c}: fused decode passed a row group the batch reader refuses");
                        };
                        assert_eq!(&got, want, "{name} rg {rg} col {c} step {step}");
                        fused_rgs += 1;
                    }
                }
            }
        }
        eprintln!("fixtures read {files}, fused leaf row groups compared {fused_rgs}");
        assert!(files >= 20 && fused_rgs >= 600, "fixtures read {files}, fused leaf row groups compared {fused_rgs}");
    }
}
