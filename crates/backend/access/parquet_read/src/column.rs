//! Per-column-chunk cursor: walks pages, dispatches the decode kernel PER
//! PAGE (dictionary-to-PLAIN fallback happens mid-chunk on exactly the
//! wide columns of the target workload class), and fills row-dense batches.
//!
//! Every page's levels decode whole-page into reusable lanes at page open;
//! the DELTA_* and BYTE_STREAM_SPLIT families normalize to the PLAIN
//! layout of their physical type in a reusable buffer, so one PLAIN kernel
//! set serves every encoding. Dictionary pages decode ONCE per chunk into
//! a typed dictionary; data pages under dictionary encoding decode as index
//! streams mapped through it. String dictionaries are UTF-8 validated once,
//! so index pages need no per-value validation.
//!
//! Flat leaves (`max_rep == 0`) fill one slot per row. Repeated leaves fill
//! one slot per level entry, `n` RECORDS per batch (a record starts at a
//! repetition level of zero), with the per-slot definition and repetition
//! levels exposed through `levels()` for the binder's reassembly.

use types_error::{PgError, PgResult};

use crate::codec::{crc32, crc_enabled, decompress_page, PAD};
use crate::held::{cap_bytes, HeldCharge};
use crate::delta::{
    byte_stream_split_to_plain, delta_byte_array_to_plain, delta_ints_to_plain,
    delta_length_to_plain,
};
use crate::levels::decode_levels;
use crate::meta::{unsupported, CodecId, Phys};
use crate::page::{parse_page_header, Enc, PageKind};
use crate::plain::{
    invalid_utf8, plain_byte_array, plain_f32, plain_f64, plain_fixed_bytes, plain_i32,
    plain_i64, plain_i96, utf8_ok,
};
use crate::rle::{corrupt_page, BoolBits, HybridState};
use crate::thrift::corrupt;

pub struct ColumnBatch {
    pub nulls: Vec<bool>,
    pub data: BatchData,
}

pub enum BatchData {
    Bool(Vec<bool>),
    I32(Vec<i32>),
    I64(Vec<i64>),
    F32(Vec<f32>),
    F64(Vec<f64>),
    /// `ends[i]` is the arena end of slot i; slot start = previous end.
    /// BYTE_ARRAY and FIXED_LEN_BYTE_ARRAY (every slot `type_length` wide).
    Bytes { ends: Vec<u32>, arena: Vec<u8> },
    /// INT96 as stored: three little-endian words (nanoseconds of day in the
    /// first two, Julian day in the third); the binder converts.
    I96(Vec<[u32; 3]>),
}

impl ColumnBatch {
    pub fn new_for(phys: Phys) -> ColumnBatch {
        let data = match phys {
            Phys::Boolean => BatchData::Bool(Vec::new()),
            Phys::Int32 => BatchData::I32(Vec::new()),
            Phys::Int64 => BatchData::I64(Vec::new()),
            Phys::Float => BatchData::F32(Vec::new()),
            Phys::Double => BatchData::F64(Vec::new()),
            Phys::ByteArray | Phys::Flba(_) => {
                BatchData::Bytes { ends: Vec::new(), arena: Vec::new() }
            }
            Phys::Int96 => BatchData::I96(Vec::new()),
        };
        ColumnBatch { nulls: Vec::new(), data }
    }

    /// CONVERT wave 3 (cvwrite job 3): the heap bytes the batch holds (capacities): its null flags and its values (a
    /// byte column's ends and arena). The caller that keeps the batch charges them ([`batches_held`]).
    pub fn held_bytes(&self) -> u64 {
        let data = match &self.data {
            BatchData::Bool(v) => cap_bytes(v.capacity(), v),
            BatchData::I32(v) => cap_bytes(v.capacity(), v),
            BatchData::I64(v) => cap_bytes(v.capacity(), v),
            BatchData::F32(v) => cap_bytes(v.capacity(), v),
            BatchData::F64(v) => cap_bytes(v.capacity(), v),
            BatchData::I96(v) => cap_bytes(v.capacity(), v),
            BatchData::Bytes { ends, arena } => cap_bytes(ends.capacity(), ends) + arena.capacity() as u64,
        };
        data + self.nulls.capacity() as u64
    }

    pub fn len(&self) -> usize {
        self.nulls.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nulls.is_empty()
    }

    #[inline]
    pub fn is_null(&self, i: usize) -> bool {
        self.nulls[i]
    }

    #[inline]
    pub fn bytes_at(&self, i: usize) -> &[u8] {
        let BatchData::Bytes { ends, arena } = &self.data else {
            unreachable!("bytes_at on a non-bytes batch");
        };
        let start = if i == 0 { 0 } else { ends[i - 1] as usize };
        &arena[start..ends[i] as usize]
    }

    fn clear(&mut self) {
        self.nulls.clear();
        match &mut self.data {
            BatchData::Bool(v) => v.clear(),
            BatchData::I32(v) => v.clear(),
            BatchData::I64(v) => v.clear(),
            BatchData::F32(v) => v.clear(),
            BatchData::F64(v) => v.clear(),
            BatchData::I96(v) => v.clear(),
            BatchData::Bytes { ends, arena } => {
                ends.clear();
                arena.clear();
            }
        }
    }
}

/// CONVERT wave 3 (cvwrite job 3): the heap bytes a set of batches holds ([`ColumnBatch::held_bytes`] summed). A caller
/// that keeps batches across [`RowGroupReader::read_batches`](crate::RowGroupReader::read_batches) calls brings its
/// charge to this after each call, on the decoding thread (copy's pgrc parquet load: the load's owner).
pub fn batches_held(batches: &[ColumnBatch]) -> u64 {
    batches.iter().map(ColumnBatch::held_bytes).sum()
}

/// Per-slot definition and repetition levels of the last batch, filled for
/// leaves that need them (`max_rep > 0` or `max_def > 1`); empty otherwise.
#[derive(Default)]
pub struct Levels {
    pub def: Vec<u16>,
    pub rep: Vec<u16>,
}

enum Dict {
    I32(Vec<i32>),
    I64(Vec<i64>),
    F32(Vec<f32>),
    F64(Vec<f64>),
    I96(Vec<[u32; 3]>),
    Bytes { ends: Vec<u32>, arena: Vec<u8> },
}

impl Dict {
    fn len(&self) -> usize {
        match self {
            Dict::I32(v) => v.len(),
            Dict::I64(v) => v.len(),
            Dict::F32(v) => v.len(),
            Dict::F64(v) => v.len(),
            Dict::I96(v) => v.len(),
            Dict::Bytes { ends, .. } => ends.len(),
        }
    }
}

enum ValState {
    /// Dictionary-index stream (bit-width byte already consumed).
    Dict(HybridState),
    PlainFixed { pos: usize, end: usize },
    /// FIXED_LEN_BYTE_ARRAY / INT96 values of `width` bytes each.
    PlainFixedBytes { pos: usize, end: usize, width: usize },
    PlainBytes { pos: usize, end: usize },
    PlainBool { bits: BoolBits, end: usize },
    /// RLE-encoded boolean data page (4-byte length prefix consumed).
    RleBool(HybridState),
}

/// Which buffer a page's values live in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Src {
    /// Directly in the chunk (UNCOMPRESSED codec, or v2 values marked
    /// uncompressed).
    Chunk,
    /// The decompression scratch buffer.
    Scratch,
    /// The PLAIN-normalized buffer (DELTA_* / BYTE_STREAM_SPLIT pages).
    Plain,
}

struct PageState {
    src: Src,
    /// Slots (levels) of this page not yet consumed.
    values_left: usize,
    /// True when `def_lv`/`rep_lv` hold this page's levels (`max_def > 0`).
    has_levels: bool,
    lvl_pos: usize,
    val: ValState,
}

/// Chunk byte storage: owned (per-chunk pread) or a slice of one shared
/// whole-row-group read (the coalesced-read lane — sequential range read +
/// in-memory demux).
pub(crate) enum ChunkBytes {
    Owned(Vec<u8>),
    Shared { buf: std::sync::Arc<Vec<u8>>, start: usize, len: usize },
}

impl ChunkBytes {
    /// CONVERT wave 3: the heap bytes this chunk owns (`Owned`), or the shared buffer's (`Shared`, the same block for
    /// every chunk of the row group: the reader counts it once).
    pub(crate) fn held(&self) -> (u64, Option<u64>) {
        match self {
            ChunkBytes::Owned(v) => (v.capacity() as u64, None),
            ChunkBytes::Shared { buf, .. } => (0, Some(buf.capacity() as u64)),
        }
    }

    #[inline]
    pub(crate) fn bytes(&self) -> &[u8] {
        match self {
            ChunkBytes::Owned(v) => v,
            ChunkBytes::Shared { buf, start, len } => &buf[*start..*start + *len],
        }
    }
}

pub struct ColumnCursor {
    /// CONVERT wave 3 (cvwrite job 2): the loading statement's charge on [`ColumnCursor::held_bytes`] (the page,
    /// levels, dictionary and decode scratch; the chunk is the reader's), synced after a page opens and after each
    /// batch, on the decode thread. Declared first: it drops before the blocks are freed.
    charge: HeldCharge,
    /// Whole chunk bytes (dictionary page + data pages), 64-byte padded.
    chunk: ChunkBytes,
    chunk_len: usize,
    codec: CodecId,
    phys: Phys,
    max_def: u16,
    max_rep: u16,
    validate_utf8: bool,
    name: String,
    /// Values (slots, nulls included) remaining across the chunk.
    values_left: u64,
    /// Next page-header offset inside `chunk`.
    walk: usize,
    dict: Option<Dict>,
    page: Option<PageState>,
    scratch: Vec<u8>,
    plain: Vec<u8>,
    def_lv: Vec<u16>,
    rep_lv: Vec<u16>,
    tmp64: Vec<i64>,
    tmp64b: Vec<i64>,
    pres: Vec<bool>,
    ids: Vec<u32>,
    lanes: Levels,
    want_lanes: bool,
}

fn oom() -> Box<PgError> {
    crate::oom_in("decoding parquet column")
}

impl ColumnCursor {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        chunk: ChunkBytes,
        codec: CodecId,
        phys: Phys,
        max_def: u16,
        max_rep: u16,
        validate_utf8: bool,
        name: String,
        num_values: i64,
        owner: Option<memledger::Owner>,
    ) -> PgResult<ColumnCursor> {
        // Owned chunks get their 64-byte pad here; shared buffers were
        // padded once at the whole-row-group read.
        let chunk = match chunk {
            ChunkBytes::Owned(mut v) => {
                let n = v.len();
                v.try_reserve(PAD).map_err(|_| oom())?; // alloc: Exec charged chunk_charge (the reader's chunks_held)
                v.resize(n + PAD, 0); // alloc: Exec charged chunk_charge (the reader's chunks_held)
                v.truncate(n);
                ChunkBytes::Owned(v)
            }
            shared => shared,
        };
        let chunk_len = chunk.bytes().len();
        Ok(ColumnCursor {
            charge: HeldCharge::new(owner),
            chunk,
            chunk_len,
            codec,
            phys,
            max_def,
            max_rep,
            validate_utf8,
            name,
            values_left: num_values as u64,
            walk: 0,
            dict: None,
            page: None,
            scratch: Vec::new(),
            plain: Vec::new(),
            def_lv: Vec::new(),
            rep_lv: Vec::new(),
            tmp64: Vec::new(),
            tmp64b: Vec::new(),
            pres: Vec::new(),
            ids: Vec::new(),
            lanes: Levels::default(),
            want_lanes: max_rep > 0 || max_def > 1,
        })
    }

    /// Per-slot levels of the last `read_batch` (see [`Levels`]).
    pub fn levels(&self) -> &Levels {
        &self.lanes
    }

    /// CONVERT wave 3: the chunk this cursor walks (the reader charges the chunks).
    pub(crate) fn chunk(&self) -> &ChunkBytes {
        &self.chunk
    }

    /// CONVERT wave 3: the heap bytes the cursor holds besides its chunk (capacity): the decompressed page, the
    /// normalized plain page, the levels, the dictionary and the batch scratch.
    pub(crate) fn held_bytes(&self) -> u64 {
        let dict = match &self.dict {
            None => 0,
            Some(Dict::I32(v)) => cap_bytes(v.capacity(), v),
            Some(Dict::I64(v)) => cap_bytes(v.capacity(), v),
            Some(Dict::F32(v)) => cap_bytes(v.capacity(), v),
            Some(Dict::F64(v)) => cap_bytes(v.capacity(), v),
            Some(Dict::I96(v)) => cap_bytes(v.capacity(), v),
            Some(Dict::Bytes { ends, arena }) => cap_bytes(ends.capacity(), ends) + arena.capacity() as u64,
        };
        dict + self.scratch.capacity() as u64
            + self.plain.capacity() as u64
            + cap_bytes(self.def_lv.capacity(), &self.def_lv)
            + cap_bytes(self.rep_lv.capacity(), &self.rep_lv)
            + cap_bytes(self.tmp64.capacity(), &self.tmp64)
            + cap_bytes(self.tmp64b.capacity(), &self.tmp64b)
            + self.pres.capacity() as u64
            + cap_bytes(self.ids.capacity(), &self.ids)
            + cap_bytes(self.lanes.def.capacity(), &self.lanes.def)
            + cap_bytes(self.lanes.rep.capacity(), &self.lanes.rep)
    }

    #[inline]
    fn sync_charge(&mut self) {
        let held = self.held_bytes();
        self.charge.sync(held);
    }

    /// Fill `out` with exactly `n` rows (flat leaf) or `n` records (repeated
    /// leaf). The row-group driver never asks for more than the group
    /// holds, so a short chunk is a corruption error, not EOF.
    pub fn read_batch(&mut self, out: &mut ColumnBatch, n: usize) -> PgResult<()> {
        // CONVERT wave 3: the batch's scratch growth is charged once it is in (a page's at its open)
        let r = self.read_batch_in(out, n);
        self.sync_charge();
        r
    }

    fn read_batch_in(&mut self, out: &mut ColumnBatch, n: usize) -> PgResult<()> {
        out.clear();
        self.lanes.def.clear();
        self.lanes.rep.clear();
        out.nulls.try_reserve(n).map_err(|_| oom())?; // alloc: Exec charged the batch keeper's charge (ColumnBatch::held_bytes; copy's parquet loads)
        if self.max_rep == 0 {
            while out.nulls.len() < n {
                if !self.ensure_page()? {
                    return Err(corrupt_page("column chunk ends before the declared row count"));
                }
                let page = self.page.as_ref().expect("page");
                let k = (n - out.nulls.len()).min(page.values_left);
                self.fill_from_page(out, k)?;
            }
            return Ok(());
        }
        // Repeated leaf: slots until the (n+1)-th record boundary.
        let mut records = 0usize;
        loop {
            if !self.ensure_page()? {
                if records == n {
                    return Ok(());
                }
                return Err(corrupt_page("column chunk ends before the declared row count"));
            }
            let page = self.page.as_ref().expect("page");
            let reps = &self.rep_lv[page.lvl_pos..page.lvl_pos + page.values_left];
            let mut k = 0usize;
            let mut done = false;
            for &r in reps {
                if r == 0 {
                    if records == n {
                        done = true;
                        break;
                    }
                    records += 1;
                }
                k += 1;
            }
            if k > 0 {
                self.fill_from_page(out, k)?;
            }
            if done {
                return Ok(());
            }
        }
    }

    /// A page with slots left is current on return `true`; `false` when the
    /// chunk's values are exhausted.
    fn ensure_page(&mut self) -> PgResult<bool> {
        let need = match &self.page {
            Some(p) => p.values_left == 0,
            None => true,
        };
        if need {
            // CONVERT wave 3: the page just opened (decompressed, its levels and any dictionary) is charged here
            let r = self.advance_page();
            self.sync_charge();
            return r;
        }
        Ok(true)
    }

    /// Decode exactly `k` slots from the current page.
    fn fill_from_page(&mut self, out: &mut ColumnBatch, k: usize) -> PgResult<()> {
        let owner = self.charge.owner();
        let ColumnCursor {
            chunk,
            page,
            scratch,
            plain,
            dict,
            validate_utf8,
            name,
            values_left,
            def_lv,
            rep_lv,
            pres,
            ids,
            lanes,
            want_lanes,
            max_def,
            ..
        } = self;
        let page_st = page.as_mut().expect("fill_from_page with a page");
        debug_assert!(k > 0 && k <= page_st.values_left);
        let buf: &[u8] = match page_st.src {
            Src::Chunk => chunk.bytes(),
            Src::Scratch => scratch,
            Src::Plain => plain,
        };

        // Presence mask for the k slots: a slot holds a value exactly when
        // its definition level reaches the leaf's maximum.
        pres.clear();
        pres.try_reserve(k).map_err(|_| oom())?; // alloc: Exec charged the cursor's charge (ColumnCursor / FastCol held_bytes)
        let present = if page_st.has_levels {
            let lv = &def_lv[page_st.lvl_pos..page_st.lvl_pos + k];
            let mut present = 0usize;
            for &d in lv {
                let p = d == *max_def;
                present += p as usize;
                pres.push(p); // alloc: Exec charged the cursor's charge (ColumnCursor / FastCol held_bytes)
                out.nulls.push(!p); // alloc: Exec charged the batch keeper's charge (ColumnBatch::held_bytes; copy's parquet loads)
            }
            if *want_lanes {
                lanes.def.extend_from_slice(lv); // alloc: Exec charged the cursor's charge (ColumnCursor / FastCol held_bytes)
                if !rep_lv.is_empty() {
                    lanes.rep.extend_from_slice(&rep_lv[page_st.lvl_pos..page_st.lvl_pos + k]); // alloc: Exec charged the cursor's charge (ColumnCursor / FastCol held_bytes)
                } else {
                    lanes.rep.extend(core::iter::repeat_n(0u16, k)); // alloc: Exec charged the cursor's charge (ColumnCursor / FastCol held_bytes)
                }
            }
            present
        } else {
            pres.extend(core::iter::repeat_n(true, k)); // alloc: Exec charged the cursor's charge (ColumnCursor / FastCol held_bytes)
            out.nulls.extend(core::iter::repeat_n(false, k)); // alloc: Exec charged the batch keeper's charge (ColumnBatch::held_bytes; copy's parquet loads)
            k
        };
        let dense = present == k;
        let mask: Option<&[bool]> = if dense { None } else { Some(pres) };

        // Decode `present` values, then scatter across nulls if any.
        match &mut page_st.val {
            ValState::Dict(idx) => {
                ids.clear();
                ids.try_reserve(present).map_err(|_| oom())?; // alloc: Exec charged the cursor's charge (ColumnCursor / FastCol held_bytes)
                ids.resize(present, 0); // alloc: Exec charged the cursor's charge (ColumnCursor / FastCol held_bytes)
                idx.fill(buf, ids)?;
                let d = dict
                    .as_ref()
                    .ok_or_else(|| corrupt_page("dictionary-encoded page without a dictionary"))?;
                let dlen = d.len() as u32;
                // Branch-free bound accumulation over the whole batch.
                let mut bad = false;
                for &i in ids.iter() {
                    bad |= i >= dlen;
                }
                if bad {
                    return Err(corrupt_page("dictionary index out of range"));
                }
                match (&mut out.data, d) {
                    (BatchData::I32(v), Dict::I32(dv)) => {
                        scatter_fixed(v, ids, |i| dv[i as usize], mask, k, 0)?
                    }
                    (BatchData::I64(v), Dict::I64(dv)) => {
                        scatter_fixed(v, ids, |i| dv[i as usize], mask, k, 0)?
                    }
                    (BatchData::F32(v), Dict::F32(dv)) => {
                        scatter_fixed(v, ids, |i| dv[i as usize], mask, k, 0.0)?
                    }
                    (BatchData::F64(v), Dict::F64(dv)) => {
                        scatter_fixed(v, ids, |i| dv[i as usize], mask, k, 0.0)?
                    }
                    (BatchData::I96(v), Dict::I96(dv)) => {
                        scatter_fixed(v, ids, |i| dv[i as usize], mask, k, [0; 3])?
                    }
                    (BatchData::Bytes { ends, arena }, Dict::Bytes { ends: de, arena: da }) => {
                        ends.try_reserve(k).map_err(|_| oom())?; // alloc: Exec charged the batch keeper's charge (ColumnBatch::held_bytes; copy's parquet loads)
                        let mut id_it = ids.iter();
                        for row in 0..k {
                            if pres[row] {
                                let i = *id_it.next().expect("present count") as usize;
                                let start = if i == 0 { 0 } else { de[i - 1] as usize };
                                let val = &da[start..de[i] as usize];
                                if arena.len() + val.len() > u32::MAX as usize {
                                    return Err(corrupt_page("string batch exceeds 4GB"));
                                }
                                arena.try_reserve(val.len()).map_err(|_| oom())?; // alloc: Exec charged the batch keeper's charge (ColumnBatch::held_bytes; copy's parquet loads)
                                arena.extend_from_slice(val); // alloc: Exec charged the batch keeper's charge (ColumnBatch::held_bytes; copy's parquet loads)
                            }
                            ends.push(arena.len() as u32); // alloc: Exec charged the batch keeper's charge (ColumnBatch::held_bytes; copy's parquet loads)
                        }
                    }
                    _ => return Err(corrupt_page("dictionary type does not match column type")),
                }
            }
            ValState::PlainFixed { pos, end } => match &mut out.data {
                BatchData::I32(v) => {
                    scatter_plain(v, buf, *end, pos, present, mask, k, 0, plain_i32)?
                }
                BatchData::I64(v) => {
                    scatter_plain(v, buf, *end, pos, present, mask, k, 0, plain_i64)?
                }
                BatchData::F32(v) => {
                    scatter_plain(v, buf, *end, pos, present, mask, k, 0.0, plain_f32)?
                }
                BatchData::F64(v) => {
                    scatter_plain(v, buf, *end, pos, present, mask, k, 0.0, plain_f64)?
                }
                _ => return Err(corrupt_page("PLAIN fixed page on a non-fixed column")),
            },
            ValState::PlainFixedBytes { pos, end, width } => match &mut out.data {
                BatchData::I96(v) => {
                    scatter_plain(v, buf, *end, pos, present, mask, k, [0; 3], plain_i96)?
                }
                BatchData::Bytes { ends, arena } => {
                    ends.try_reserve(k).map_err(|_| oom())?; // alloc: Exec charged the batch keeper's charge (ColumnBatch::held_bytes; copy's parquet loads)
                    let need = present.checked_mul(*width).ok_or_else(|| corrupt_page("value count overflow"))?;
                    if *pos + need > *end {
                        return Err(corrupt_page("fixed-width values end before the declared count"));
                    }
                    if arena.len() + need > u32::MAX as usize {
                        return Err(corrupt_page("value batch exceeds 4GB"));
                    }
                    arena.try_reserve(need).map_err(|_| oom())?; // alloc: Exec charged the batch keeper's charge (ColumnBatch::held_bytes; copy's parquet loads)
                    for row in 0..k {
                        if pres[row] {
                            plain_fixed_bytes(buf, pos, *width, arena);
                        }
                        ends.push(arena.len() as u32); // alloc: Exec charged the batch keeper's charge (ColumnBatch::held_bytes; copy's parquet loads)
                    }
                }
                _ => return Err(corrupt_page("fixed-width byte page on a mismatched column")),
            },
            ValState::PlainBytes { pos, end } => {
                let BatchData::Bytes { ends, arena } = &mut out.data else {
                    return Err(corrupt_page("PLAIN byte-array page on a non-string column"));
                };
                match mask {
                    None => {
                        plain_byte_array(
                            buf,
                            *end,
                            pos,
                            present,
                            ends,
                            arena,
                            *validate_utf8,
                            name,
                        )?;
                    }
                    Some(m) => {
                        // Interleave: walk values for present slots only. A present slot's value is appended and its
                        // end pushed by plain_byte_array itself (CONVERT wave 3, cvwrite job 3: no temporary ends
                        // buffer, so every block here is the batch's); a NULL slot repeats the arena's end.
                        ends.try_reserve(k).map_err(|_| oom())?; // alloc: Exec charged the batch keeper's charge (ColumnBatch::held_bytes; copy's parquet loads)
                        for &p in m.iter() {
                            if p {
                                plain_byte_array(buf, *end, pos, 1, ends, arena, *validate_utf8, name)?;
                            } else {
                                ends.push(arena.len() as u32); // alloc: Exec charged the batch keeper's charge (ColumnBatch::held_bytes; copy's parquet loads)
                            }
                        }
                    }
                }
            }
            ValState::PlainBool { bits, end } => {
                let BatchData::Bool(v) = &mut out.data else {
                    return Err(corrupt_page("boolean page on a non-boolean column"));
                };
                let mut tmp = memledger::ChargedVec::<bool>::new(owner, memledger::Kind::Region);
                tmp.with_vec(|t| t.try_reserve_exact(present)).map_err(|_| oom())?; // alloc: Exec charged owner
                tmp.with_vec(|t| t.resize(present, false)); // alloc: Exec charged owner
                bits.fill(buf, *end, &mut tmp)?;
                scatter_bools(v, &tmp, mask, k)?;
            }
            ValState::RleBool(st) => {
                let BatchData::Bool(v) = &mut out.data else {
                    return Err(corrupt_page("boolean page on a non-boolean column"));
                };
                ids.clear();
                ids.try_reserve(present).map_err(|_| oom())?; // alloc: Exec charged the cursor's charge (ColumnCursor / FastCol held_bytes)
                ids.resize(present, 0); // alloc: Exec charged the cursor's charge (ColumnCursor / FastCol held_bytes)
                st.fill(buf, ids)?;
                let mut tmp = memledger::ChargedVec::<bool>::new(owner, memledger::Kind::Region);
                tmp.with_vec(|t| t.extend(ids.iter().map(|&x| x != 0))); // alloc: Exec charged owner
                scatter_bools(v, &tmp, mask, k)?;
            }
        }

        page_st.values_left -= k;
        page_st.lvl_pos += k;
        if (*values_left as usize) < k {
            return Err(corrupt_page("pages hold more values than the chunk declares"));
        }
        *values_left -= k as u64;
        Ok(())
    }

    /// Walk to the next data page, decoding any dictionary page on the way.
    /// Returns false when the chunk's value count is exhausted.
    fn advance_page(&mut self) -> PgResult<bool> {
        loop {
            if self.values_left == 0 {
                return Ok(false);
            }
            if self.walk >= self.chunk_len {
                return Err(corrupt_page("column chunk ends before the declared value count"));
            }
            let head =
                parse_page_header(&self.chunk.bytes()[..self.chunk_len], self.walk, &self.name)?;
            let payload_start = head.payload_at;
            let payload_end = payload_start + head.compressed_size;
            self.walk = payload_end;
            if let Some(want) = head.crc {
                if crc_enabled() {
                    let got = crc32(&self.chunk.bytes()[payload_start..payload_end]);
                    if got != want {
                        return Err(corrupt_page("page crc32 mismatch"));
                    }
                }
            }
            match head.kind {
                PageKind::Skip => continue,
                PageKind::Dict { num_values, encoding } => {
                    if self.dict.is_some() {
                        return Err(corrupt("second dictionary page in one column chunk"));
                    }
                    if !matches!(encoding, Enc::Plain | Enc::PlainDictionary) {
                        return Err(unsupported(format!( // alloc: Cold
                            "dictionary page encoding {} (column \"{}\")",
                            encoding.name(),
                            self.name
                        )));
                    }
                    let (src, start, end) = self.page_payload(
                        payload_start,
                        head.compressed_size,
                        head.uncompressed_size,
                    )?;
                    let buf: &[u8] = match src {
                        Src::Chunk => self.chunk.bytes(),
                        _ => &self.scratch,
                    };
                    self.dict = Some(decode_dict(
                        buf,
                        start,
                        end,
                        self.phys,
                        num_values,
                        self.validate_utf8,
                        &self.name,
                    )?);
                    continue;
                }
                PageKind::Data { num_values, encoding, def_encoding, rep_encoding } => {
                    if num_values == 0 {
                        continue;
                    }
                    let (src, start, end) = self.page_payload(
                        payload_start,
                        head.compressed_size,
                        head.uncompressed_size,
                    )?;
                    // v1 layout: [rep levels][def levels][values], each
                    // level run length-prefixed (RLE) or implied
                    // (BIT_PACKED), all under the page codec.
                    let mut vpos = start;
                    self.rep_lv.clear();
                    self.def_lv.clear();
                    if self.max_rep > 0 {
                        vpos = self.v1_levels(src, vpos, end, rep_encoding, self.max_rep, num_values, true)?;
                    }
                    if self.max_def > 0 {
                        vpos = self.v1_levels(src, vpos, end, def_encoding, self.max_def, num_values, false)?;
                    }
                    self.open_values(src, vpos, end, encoding, num_values)?;
                    return Ok(true);
                }
                PageKind::DataV2 {
                    num_values,
                    num_nulls,
                    num_rows,
                    encoding,
                    def_len,
                    rep_len,
                    compressed,
                } => {
                    // v2 layout: uncompressed RLE levels (no length prefix)
                    // in the chunk, then the values, compressed unless the
                    // page says otherwise.
                    let chunk = self.chunk.bytes();
                    self.rep_lv.clear();
                    self.def_lv.clear();
                    let mut p = payload_start;
                    if self.max_rep > 0 {
                        decode_levels(Enc::Rle, chunk, p, p + rep_len, self.max_rep, num_values, &mut self.rep_lv)?;
                        let starts = self.rep_lv.iter().filter(|&&r| r == 0).count();
                        if starts != num_rows {
                            return Err(corrupt_page("data page v2 row count disagrees with its levels"));
                        }
                    } else if num_rows != num_values {
                        return Err(corrupt_page("data page v2 row count disagrees with its value count"));
                    }
                    // A flat column may still carry a (zero-valued) level
                    // run from some writers; its declared length skips it.
                    p += rep_len;
                    if self.max_def > 0 {
                        decode_levels(Enc::Rle, chunk, p, p + def_len, self.max_def, num_values, &mut self.def_lv)?;
                        // num_nulls is only reliable for flat optional
                        // leaves: for nested ones arrow records the leaf
                        // statistics' null count (empty lists excluded), so
                        // the levels are the truth there.
                        let present = self.def_lv.iter().filter(|&&d| d == self.max_def).count();
                        if self.max_def == 1 && self.max_rep == 0 && present + num_nulls != num_values {
                            return Err(Box::new(PgError::error(format!( // alloc: Cold
                                "corrupt parquet page: data page v2 declares {num_nulls} nulls over {num_values} values, its levels hold {present} values"
                            ))
                            .with_sqlstate(types_error::ERRCODE_BAD_COPY_FILE_FORMAT)));
                        }
                    } else if num_nulls != 0 {
                        return Err(corrupt_page("nulls declared on a required column"));
                    }
                    p += def_len;
                    let vcomp = head.compressed_size - def_len - rep_len;
                    let vuncomp = head.uncompressed_size - def_len - rep_len;
                    let (src, start, end) = if vcomp == 0 && vuncomp == 0 {
                        // A page of nulls (or no values) has an empty
                        // values section: nothing to decompress.
                        (Src::Chunk, p, p)
                    } else if compressed && self.codec != CodecId::Uncompressed {
                        let src = self
                            .chunk
                            .bytes()
                            .get(p..p + vcomp)
                            .ok_or_else(|| corrupt_page("page payload overruns chunk"))?;
                        decompress_page(self.codec, src, &mut self.scratch, vuncomp)?;
                        (Src::Scratch, 0, vuncomp)
                    } else {
                        if vcomp != vuncomp {
                            return Err(corrupt_page("uncompressed page sizes disagree"));
                        }
                        (Src::Chunk, p, p + vcomp)
                    };
                    self.open_values(src, start, end, encoding, num_values)?;
                    return Ok(true);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn v1_levels(
        &mut self,
        src: Src,
        pos: usize,
        end: usize,
        enc: Enc,
        max: u16,
        n: usize,
        rep: bool,
    ) -> PgResult<usize> {
        let buf: &[u8] = match src {
            Src::Chunk => self.chunk.bytes(),
            _ => &self.scratch,
        };
        let out = if rep { &mut self.rep_lv } else { &mut self.def_lv };
        match enc {
            Enc::Rle => {
                let Some(lb) = buf.get(pos..pos + 4).filter(|_| pos + 4 <= end) else {
                    return Err(corrupt_page("levels overrun page"));
                };
                let len = u32::from_le_bytes(lb.try_into().expect("4-byte slice")) as usize;
                let start = pos + 4;
                if start + len > end {
                    return Err(corrupt_page("levels overrun page"));
                }
                decode_levels(Enc::Rle, buf, start, start + len, max, n, out)?;
                Ok(start + len)
            }
            Enc::BitPacked => decode_levels(Enc::BitPacked, buf, pos, end, max, n, out),
            other => Err(unsupported(format!( // alloc: Cold
                "level encoding {} (column \"{}\")",
                other.name(),
                self.name
            ))),
        }
    }

    /// Install the value decoder for a data page whose levels are decoded;
    /// a page-level error names the column.
    fn open_values(
        &mut self,
        src: Src,
        vpos: usize,
        end: usize,
        encoding: Enc,
        num_values: usize,
    ) -> PgResult<()> {
        self.open_values_inner(src, vpos, end, encoding, num_values).map_err(|mut e| {
            if e.detail.is_none() {
                e.detail = Some(format!("Column \"{}\".", self.name)); // alloc: Cold
            }
            e
        })
    }

    fn open_values_inner(
        &mut self,
        src: Src,
        vpos: usize,
        end: usize,
        encoding: Enc,
        num_values: usize,
    ) -> PgResult<()> {
        let has_levels = self.max_def > 0;
        let present = if has_levels {
            if self.def_lv.len() != num_values {
                return Err(corrupt_page("level count differs from the page's value count"));
            }
            self.def_lv.iter().filter(|&&d| d == self.max_def).count()
        } else {
            num_values
        };
        let unsupported_enc = |name: &str, enc: Enc, phys: Phys| {
            unsupported(format!("encoding {} for {} (column \"{}\")", enc.name(), phys.name(), name)) // alloc: Cold
        };
        let (src, val) = match encoding {
            Enc::Plain => {
                let val = match self.phys {
                    Phys::Boolean => ValState::PlainBool { bits: BoolBits::new(vpos), end },
                    Phys::ByteArray => ValState::PlainBytes { pos: vpos, end },
                    Phys::Int32 | Phys::Int64 | Phys::Float | Phys::Double => {
                        ValState::PlainFixed { pos: vpos, end }
                    }
                    Phys::Int96 => ValState::PlainFixedBytes { pos: vpos, end, width: 12 },
                    Phys::Flba(w) => ValState::PlainFixedBytes { pos: vpos, end, width: w as usize },
                };
                (src, val)
            }
            Enc::PlainDictionary | Enc::RleDictionary => {
                let buf = self.buf(src);
                let Some(&w) = buf.get(vpos).filter(|_| vpos < end) else {
                    return Err(corrupt_page("dictionary page bit width missing"));
                };
                if w > 32 {
                    return Err(corrupt_page("dictionary bit width above 32"));
                }
                (src, ValState::Dict(HybridState::new(vpos + 1, end, w as usize)?))
            }
            Enc::Rle => {
                if self.phys != Phys::Boolean {
                    return Err(unsupported_enc(&self.name, encoding, self.phys));
                }
                let buf = self.buf(src);
                let Some(lb) = buf.get(vpos..vpos + 4).filter(|_| vpos + 4 <= end) else {
                    return Err(corrupt_page("RLE boolean run overruns page"));
                };
                let rlen = u32::from_le_bytes(lb.try_into().expect("4-byte slice")) as usize;
                if vpos + 4 + rlen > end {
                    return Err(corrupt_page("RLE boolean run overruns page"));
                }
                (src, ValState::RleBool(HybridState::new(vpos + 4, vpos + 4 + rlen, 1)?))
            }
            Enc::DeltaBinaryPacked => {
                let width = match self.phys {
                    Phys::Int32 => 4,
                    Phys::Int64 => 8,
                    _ => return Err(unsupported_enc(&self.name, encoding, self.phys)),
                };
                let ColumnCursor { chunk, scratch, plain, tmp64, .. } = self;
                let buf: &[u8] = match src {
                    Src::Chunk => chunk.bytes(),
                    _ => scratch,
                };
                delta_ints_to_plain(buf, vpos, end, width, present, plain, tmp64)?;
                (Src::Plain, ValState::PlainFixed { pos: 0, end: self.plain.len() })
            }
            Enc::DeltaLengthByteArray => {
                if self.phys != Phys::ByteArray {
                    return Err(unsupported_enc(&self.name, encoding, self.phys));
                }
                let ColumnCursor { chunk, scratch, plain, tmp64, .. } = self;
                let buf: &[u8] = match src {
                    Src::Chunk => chunk.bytes(),
                    _ => scratch,
                };
                delta_length_to_plain(buf, vpos, end, present, plain, tmp64)?;
                (Src::Plain, ValState::PlainBytes { pos: 0, end: self.plain.len() })
            }
            Enc::DeltaByteArray => {
                let flba = match self.phys {
                    Phys::ByteArray => None,
                    Phys::Flba(w) => Some(w as usize),
                    _ => return Err(unsupported_enc(&self.name, encoding, self.phys)),
                };
                let ColumnCursor { chunk, scratch, plain, tmp64, tmp64b, .. } = self;
                let buf: &[u8] = match src {
                    Src::Chunk => chunk.bytes(),
                    _ => scratch,
                };
                delta_byte_array_to_plain(buf, vpos, end, present, flba, plain, tmp64, tmp64b)?;
                let val = match flba {
                    None => ValState::PlainBytes { pos: 0, end: self.plain.len() },
                    Some(w) => ValState::PlainFixedBytes { pos: 0, end: self.plain.len(), width: w },
                };
                (Src::Plain, val)
            }
            Enc::ByteStreamSplit => {
                let width = match self.phys {
                    Phys::Float | Phys::Int32 => 4,
                    Phys::Double | Phys::Int64 => 8,
                    Phys::Flba(w) => w as usize,
                    _ => return Err(unsupported_enc(&self.name, encoding, self.phys)),
                };
                let ColumnCursor { chunk, scratch, plain, .. } = self;
                let buf: &[u8] = match src {
                    Src::Chunk => chunk.bytes(),
                    _ => scratch,
                };
                byte_stream_split_to_plain(buf, vpos, end, width, present, plain)?;
                let val = match self.phys {
                    Phys::Flba(w) => ValState::PlainFixedBytes { pos: 0, end: self.plain.len(), width: w as usize },
                    _ => ValState::PlainFixed { pos: 0, end: self.plain.len() },
                };
                (Src::Plain, val)
            }
            other => return Err(unsupported_enc(&self.name, other, self.phys)),
        };
        self.page = Some(PageState { src, values_left: num_values, has_levels, lvl_pos: 0, val });
        Ok(())
    }

    #[inline]
    fn buf(&self, src: Src) -> &[u8] {
        match src {
            Src::Chunk => self.chunk.bytes(),
            Src::Scratch => &self.scratch,
            Src::Plain => &self.plain,
        }
    }

    /// (UNCOMPRESSED) or decompressed into the cursor scratch buffer.
    fn page_payload(
        &mut self,
        start: usize,
        compressed: usize,
        uncompressed: usize,
    ) -> PgResult<(Src, usize, usize)> {
        if self.codec == CodecId::Uncompressed {
            if compressed != uncompressed {
                return Err(corrupt_page("uncompressed page sizes disagree"));
            }
            return Ok((Src::Chunk, start, start + compressed));
        }
        let src = self
            .chunk
            .bytes()
            .get(start..start + compressed)
            .ok_or_else(|| corrupt_page("page payload overruns chunk"))?;
        decompress_page(self.codec, src, &mut self.scratch, uncompressed)?;
        Ok((Src::Scratch, 0, uncompressed))
    }
}

/// PLAIN fixed-width decoder shape shared by the scatter helper.
type PlainDecode<T> = fn(&[u8], usize, &mut usize, usize, &mut Vec<T>) -> PgResult<()>;

/// Scatter `present` decoded fixed-width values across `k` slots.
#[allow(clippy::too_many_arguments)]
fn scatter_plain<T: Copy>(
    out: &mut Vec<T>,
    buf: &[u8],
    end: usize,
    pos: &mut usize,
    present: usize,
    mask: Option<&[bool]>,
    k: usize,
    default: T,
    decode: PlainDecode<T>,
) -> PgResult<()> {
    match mask {
        None => decode(buf, end, pos, k, out),
        Some(m) => {
            // CONVERT wave 3 (cvwrite job 3): the `present` values are decoded onto `out`'s tail, then spread in place
            // over the `k` slots from the back (slot i's value sits at or before i), so every block is the batch's;
            // there is no temporary.
            debug_assert_eq!(m.len(), k);
            let base = out.len();
            out.try_reserve(k).map_err(|_| oom())?; // alloc: Exec charged the batch keeper's charge (ColumnBatch::held_bytes; copy's parquet loads)
            decode(buf, end, pos, present, out)?;
            out.resize(base + k, default); // alloc: Exec charged the batch keeper's charge (ColumnBatch::held_bytes; copy's parquet loads)
            let mut j = base + present;
            for i in (0..k).rev() {
                if m[i] {
                    j -= 1;
                    out[base + i] = out[j];
                } else {
                    out[base + i] = default;
                }
            }
            debug_assert_eq!(j, base, "the mask's present count is the decoded count");
            Ok(())
        }
    }
}

fn scatter_fixed<T: Copy>(
    out: &mut Vec<T>,
    ids: &[u32],
    get: impl Fn(u32) -> T,
    mask: Option<&[bool]>,
    k: usize,
    default: T,
) -> PgResult<()> {
    out.try_reserve(k).map_err(|_| oom())?; // alloc: Exec charged the batch keeper's charge (ColumnBatch::held_bytes; copy's parquet loads)
    match mask {
        None => {
            debug_assert_eq!(ids.len(), k);
            out.extend(ids.iter().map(|&i| get(i))); // alloc: Exec charged the batch keeper's charge (ColumnBatch::held_bytes; copy's parquet loads)
        }
        Some(m) => {
            let mut j = 0usize;
            for &p in m.iter() {
                if p {
                    out.push(get(ids[j])); // alloc: Exec charged the batch keeper's charge (ColumnBatch::held_bytes; copy's parquet loads)
                    j += 1;
                } else {
                    out.push(default); // alloc: Exec charged the batch keeper's charge (ColumnBatch::held_bytes; copy's parquet loads)
                }
            }
        }
    }
    Ok(())
}

fn scatter_bools(
    out: &mut Vec<bool>,
    vals: &[bool],
    mask: Option<&[bool]>,
    k: usize,
) -> PgResult<()> {
    out.try_reserve(k).map_err(|_| oom())?; // alloc: Exec charged the batch keeper's charge (ColumnBatch::held_bytes; copy's parquet loads)
    match mask {
        None => out.extend_from_slice(vals), // alloc: Exec charged the batch keeper's charge (ColumnBatch::held_bytes; copy's parquet loads)
        Some(m) => {
            let mut j = 0usize;
            for &p in m.iter() {
                if p {
                    out.push(vals[j]); // alloc: Exec charged the batch keeper's charge (ColumnBatch::held_bytes; copy's parquet loads)
                    j += 1;
                } else {
                    out.push(false); // alloc: Exec charged the batch keeper's charge (ColumnBatch::held_bytes; copy's parquet loads)
                }
            }
        }
    }
    Ok(())
}

/// Decode a PLAIN dictionary page into a typed dictionary. String
/// dictionaries validate UTF-8 here, once per chunk.
fn decode_dict(
    buf: &[u8],
    start: usize,
    end: usize,
    phys: Phys,
    num_values: usize,
    validate_utf8: bool,
    name: &str,
) -> PgResult<Dict> {
    let mut pos = start;
    match phys {
        Phys::Int32 => {
            let mut v = Vec::new();
            plain_i32(buf, end, &mut pos, num_values, &mut v)?;
            Ok(Dict::I32(v))
        }
        Phys::Int64 => {
            let mut v = Vec::new();
            plain_i64(buf, end, &mut pos, num_values, &mut v)?;
            Ok(Dict::I64(v))
        }
        Phys::Float => {
            let mut v = Vec::new();
            plain_f32(buf, end, &mut pos, num_values, &mut v)?;
            Ok(Dict::F32(v))
        }
        Phys::Double => {
            let mut v = Vec::new();
            plain_f64(buf, end, &mut pos, num_values, &mut v)?;
            Ok(Dict::F64(v))
        }
        Phys::Int96 => {
            let mut v = Vec::new();
            plain_i96(buf, end, &mut pos, num_values, &mut v)?;
            Ok(Dict::I96(v))
        }
        Phys::Flba(w) => {
            let w = w as usize;
            let need = num_values.checked_mul(w).ok_or_else(|| corrupt_page("value count overflow"))?;
            if pos + need > end {
                return Err(corrupt_page("dictionary values end before the declared count"));
            }
            let mut ends = Vec::new();
            ends.try_reserve(num_values).map_err(|_| oom())?; // alloc: Exec charged the cursor's charge (ColumnCursor / FastCol held_bytes)
            let mut arena = Vec::new();
            arena.try_reserve(need).map_err(|_| oom())?; // alloc: Exec charged the cursor's charge (ColumnCursor / FastCol held_bytes)
            for _ in 0..num_values {
                plain_fixed_bytes(buf, &mut pos, w, &mut arena);
                ends.push(arena.len() as u32); // alloc: Exec charged the cursor's charge (ColumnCursor / FastCol held_bytes)
            }
            Ok(Dict::Bytes { ends, arena })
        }
        Phys::ByteArray => {
            let mut ends = Vec::new();
            let mut arena = Vec::new();
            plain_byte_array(buf, end, &mut pos, num_values, &mut ends, &mut arena, false, name)?;
            if validate_utf8 {
                // Batched validation, once per chunk: UTF-8 is closed under
                // concatenation, so an invalid arena means some value is
                // invalid. A valid arena still needs every value to start on
                // a character boundary (a boundary inside a multi-byte
                // sequence makes both neighbors invalid) — one branch-free
                // continuation-byte test per value.
                if !utf8_ok(&arena) {
                    return Err(invalid_utf8(name));
                }
                let mut prev = 0usize;
                let mut bad = false;
                for &e in &ends {
                    if prev < arena.len() {
                        bad |= (arena[prev] & 0xc0) == 0x80;
                    }
                    prev = e as usize;
                }
                if bad {
                    return Err(invalid_utf8(name));
                }
            }
            Ok(Dict::Bytes { ends, arena })
        }
        Phys::Boolean => Err(unsupported(format!( // alloc: Cold
            "dictionary-encoded {} column \"{}\"",
            phys.name(),
            name
        ))),
    }
}

#[cfg(test)]
mod job3_tests {
    use super::*;

    fn le_i32(v: &[i32]) -> Vec<u8> {
        v.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    #[test]
    fn scatter_plain_spreads_the_present_values_in_place_after_what_the_batch_holds() {
        let buf = le_i32(&[10, 20, 30]);
        let mask = [false, true, false, true, true, false];
        let mut out: Vec<i32> = vec![7, 8];
        let mut pos = 0usize;
        scatter_plain(&mut out, &buf, buf.len(), &mut pos, 3, Some(&mask), mask.len(), -1, plain_i32).unwrap();
        assert_eq!(out, vec![7, 8, -1, 10, -1, 20, 30, -1]);
        assert_eq!(pos, buf.len());
        // every slot present, and none present
        let mut all: Vec<i32> = Vec::new();
        let mut pos = 0usize;
        scatter_plain(&mut all, &buf, buf.len(), &mut pos, 3, Some(&[true, true, true]), 3, -1, plain_i32).unwrap();
        assert_eq!(all, vec![10, 20, 30]);
        let mut none: Vec<i32> = vec![5];
        let mut pos = 0usize;
        scatter_plain(&mut none, &buf, buf.len(), &mut pos, 0, Some(&[false, false]), 2, -1, plain_i32).unwrap();
        assert_eq!(none, vec![5, -1, -1]);
        assert_eq!(pos, 0);
        // a short page is the same corruption error as before
        let mut short: Vec<i32> = Vec::new();
        let mut pos = 0usize;
        assert!(scatter_plain(&mut short, &buf[..8], 8, &mut pos, 3, Some(&[true, true, true, false]), 4, -1, plain_i32).is_err());
    }

    #[test]
    fn a_batch_reports_the_capacity_it_holds() {
        let mut b = ColumnBatch::new_for(Phys::ByteArray);
        assert_eq!(b.held_bytes(), 0);
        b.nulls.reserve_exact(16);
        if let BatchData::Bytes { ends, arena } = &mut b.data {
            ends.reserve_exact(16);
            arena.reserve_exact(100);
        }
        let want = b.nulls.capacity() as u64
            + match &b.data {
                BatchData::Bytes { ends, arena } => ends.capacity() as u64 * 4 + arena.capacity() as u64,
                _ => unreachable!(),
            };
        assert_eq!(b.held_bytes(), want);
        let mut i = ColumnBatch::new_for(Phys::Int64);
        if let BatchData::I64(v) = &mut i.data {
            v.reserve_exact(10);
        }
        let iw = match &i.data {
            BatchData::I64(v) => v.capacity() as u64 * 8,
            _ => unreachable!(),
        };
        assert_eq!(batches_held(&[b, i]), want + iw);
    }
}
