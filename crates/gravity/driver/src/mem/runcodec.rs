//! D2572: a sort run's bytes on disk as LZ4 blocks. A run row is laid for compares in memory (a `[flags, word]` pair per
//! key, a datum word and a null byte per column, 8-rounded images and copies), about 2.7x PostgreSQL's MinimalTuple on its
//! tape, so a spilled sort used 2.7x PG's temp bytes and raised 53400 under a `temp_file_limit` PG answers. The rows of a
//! sorted run repeat their neighbours (the flags, the zero padding, the key words' shared prefixes), so each block of a
//! run is written LZ4-compressed (Spark's and Velox's spill codec) and read back whole: the run's rows are the same bytes
//! after the reader, every merge and emit unchanged. A block that does not shrink is kept as written.
//!
//! The compress costs CPU (LZ4 at about 0.6 GB/s here: a spilled sort at 256 kB to 4 MB took 1.3x to 1.6x the time it takes with
//! raw runs), and the bytes matter only under a `temp_file_limit` (the 53400 PostgreSQL does not raise), so a run is
//! compressed only when its file says so (`SpillFile::compress_runs`: the host's temp file while the session has a
//! `temp_file_limit`); else every block is stored raw behind its header, the same format, read by the same reader.
//! Debt: a compact run row on disk (the compare layout stripped, PostgreSQL's MinimalTuple role) would cut
//! the bytes and this CPU both.
//!
//! D2572-c: under a limit a sort run's rows are compacted instead (`RowEncoder`: each head word without its zero high
//! bytes or as its difference from the row before's, a var record's pointer words, copy padding and plain varlena
//! headers rebuilt on read, a copy repeating the row before's left out), the shape declared by the run's kernel
//! (`SPILL_RUN_SHAPE`) or its fixed width, and its blocks stored raw: the compact rows come to 0.30x-1.00x PostgreSQL's
//! temp bytes on the spill rigs where LZ4 took 0.43x-1.25x, at less CPU on most. The run's bytes begin with the shape
//! (`put_header`) and its first block is marked `HEADED`, so the run reads back the same whatever the reading session's
//! limit. A run the codec has no shape for keeps the LZ4 blocks.
//!
//! A block: the raw length (`u32` LE; `STORED` set when its bytes are kept raw, `HEADED` on a compacted run's first), the
//! encoded length (`u32` LE), the bytes.

use crate::mem::fence::{self, Fences};
use crate::mem::spill::{HeapClaim, RunReader, SpillFile};
use cancelpoll::Step;
use crate::{Error, Result};

/// The raw bytes of one block: a reader holds one block decoded (PostgreSQL's tape reader holds one BLCKSZ block).
pub const BLOCK: usize = 8 << 10;
/// A run's encoded bytes are written as an epoch past this many (each epoch opens and closes the file).
pub const EPOCH: usize = 256 << 10;
/// A raw run's (no compress) epoch: larger, so a put run is one epoch as before the codec (16 epochs of a 4 MB run cost a
/// spilled sort about 10% of its time at 4 MB), the copy it gathers bounded.
pub const RAW_EPOCH: usize = 4 << 20;
const STORED: u32 = 1 << 31;
/// D2572-c: set on a run's first block when the run's bytes begin with its rows' header (`put_header`): the run then
/// reads back as written whatever the reading session's `temp_file_limit`.
const HEADED: u32 = 1 << 30;
const HDR: usize = 8;

/// `raw` (at most `BLOCK` bytes) appended to `out` as one block, LZ4-compressed when `compress` and it shrinks, else
/// stored raw. E-kern R: every coder's out-buffer is a live `ChargedVecMut` (its owner the caller's), never a `Vec`.
pub fn encode_block(raw: &[u8], out: &mut memledger::ChargedVecMut<'_, u8>, compress: bool) {
    encode_marked(raw, out, compress, false)
}

/// `encode_block`, the block marked `HEADED` when `headed` (a run's first block).
fn encode_marked(raw: &[u8], out: &mut memledger::ChargedVecMut<'_, u8>, compress: bool, headed: bool) {
    debug_assert!(raw.len() <= BLOCK);
    let mark = if headed { HEADED } else { 0 };
    let at = out.len();
    let packed = if compress {
        out.resize(at + HDR + lz4_flex::block::get_maximum_output_size(raw.len()), 0); // alloc: Exec charged out.owner()
        lz4_flex::block::compress_into(raw, &mut out[at + HDR..]).ok().filter(|&n| n < raw.len())
    } else {
        None
    };
    let Some(n) = packed else {
        out.truncate(at);
        out.extend_from_slice(&(raw.len() as u32 | STORED | mark).to_le_bytes()); // alloc: Exec charged out.owner()
        out.extend_from_slice(&(raw.len() as u32).to_le_bytes()); // alloc: Exec charged out.owner()
        out.extend_from_slice(raw); // alloc: Exec charged out.owner()
        return;
    };
    out[at..at + 4].copy_from_slice(&(raw.len() as u32 | mark).to_le_bytes());
    out[at + 4..at + HDR].copy_from_slice(&(n as u32).to_le_bytes());
    out.truncate(at + HDR + n);
}

/// D2572-d: a run is written raw, whatever the limit, while the statement's sort-run bytes with it (every stream's put
/// runs, raw: `SpillTable::put_raw_bytes`) stay under 1/`RAW_HEADROOM` of the `temp_file_limit`. Compacting buys bytes
/// only where they can decide a 53400 (the m5 sort at 4 MB under a 1 GB limit paid the codec, ~3 ms of 27, for bytes 1%
/// of the limit). Bound on the error PostgreSQL does not raise: the raw put runs come to at most limit/32 past what their
/// compacted bytes would be (a merge pass's rewrite of them is raw only when they fit beside the put runs too), so
/// Gravity reaches the limit early only when PostgreSQL's own bytes sit within about that 1/32 under it; a statement past
/// limit/32 compacts every later run as before.
pub const RAW_HEADROOM: u64 = 32;

/// D2572-d: whether `bytes` more written raw after `spilled` stay under 1/`RAW_HEADROOM` of a `temp_file_limit` of `limit` bytes.
pub fn raw_fits(spilled: u64, bytes: u64, limit: u64) -> bool {
    spilled.checked_add(bytes).and_then(|b| b.checked_mul(RAW_HEADROOM)).is_some_and(|b| b <= limit)
}

/// `raw` written as part `part` of `file`, encoded (compressed when the file says so for a run of its size after
/// `spilled` bytes, `SpillFile::compress_run`), in epochs of about `EPOCH` bytes.
pub fn write_run(file: &mut dyn SpillFile, part: u32, raw: &[u8], spilled: u64) -> Result<()> {
    let compress = file.compress_run(spilled, raw.len() as u64);
    write_blocks(file, part, raw, compress, false)
}

/// `raw` written as part `part` of `file` in blocks, LZ4-compressed when `compress`, the first marked `HEADED` when
/// `headed`, in epochs.
fn write_blocks(file: &mut dyn SpillFile, part: u32, raw: &[u8], compress: bool, headed: bool) -> Result<()> {
    write_blocks_at(file, part, raw, compress, headed, None, None).map(|_| ())
}

/// `write_blocks`, each block's offset in the part (from the bytes this call writes) pushed to `starts` (D3190: a
/// put run's fences); the bytes written. The epoch buffer (up to `EPOCH` / `RAW_EPOCH`) is charged to `owner`, the run's
/// slot owner when `put_run` names one, re-synced after each block's encode.
fn write_blocks_at(file: &mut dyn SpillFile, part: u32, raw: &[u8], compress: bool, headed: bool, mut starts: Option<&mut memledger::ChargedVec<u64>>, owner: Option<memledger::Owner>) -> Result<u64> {
    let epoch = if compress { EPOCH } else { RAW_EPOCH };
    let mut buf = memledger::ChargedVec::<u8>::new(owner, memledger::Kind::Sort);
    let mut out = buf.vec_mut();
    out.reserve_exact(epoch.min(raw.len() + raw.len() / 64 + HDR * 2)); // alloc: Exec charged owner
    let mut written = 0u64;
    // C9: a block is an 8 KiB encode (an LZ4 compress or a copy), its epoch's write past `epoch`: a tick each
    let mut step = cancelpoll::step("spill run write: a run's blocks encoded and written");
    for (i, chunk) in raw.chunks(BLOCK).enumerate() {
        step.tick()?;
        if let Some(s) = starts.as_deref_mut() {
            let at = written + out.len() as u64;
            s.with_vec(|v| v.push(at)); // alloc: Exec charged s.owner()
        }
        encode_marked(chunk, &mut out, compress, headed && i == 0);
        out.sync();
        if out.len() >= epoch {
            file.write_epoch(&[(part, &out[..])])?;
            written += out.len() as u64;
            out.clear();
        }
    }
    if !out.is_empty() {
        file.write_epoch(&[(part, &out[..])])?;
        written += out.len() as u64;
    }
    Ok(written)
}

/// A merge pass's writes to one merged run gathered into blocks (`raw`) and the blocks into an epoch (`enc`); `token` the
/// run they go to (`SpillTable::merge_write`'s staging, flushed when the next run begins and before any run is read);
/// `compress` its file's `compress_runs`, taken when the run begins.
/// E-kern R: its buffers (and its `RowEncoder`'s) on `owner`, the merged run's stream owner `begin` names: the stream's
/// own claim counts none of them.
#[derive(Default)]
pub struct Staged {
    pub token: Option<u32>,
    pub raw: memledger::ChargedVec<u8>,
    pub enc: memledger::ChargedVec<u8>,
    pub compress: bool,
    /// D2572-c: a compressing file's run's rows compacted (`begin` with a shape), its header written ahead of its first
    /// bytes, its blocks stored raw.
    rows: Option<RowEncoder>,
    head: bool,
    mark: bool,
    scratch: memledger::ChargedVec<u8>,
    owner: Option<memledger::Owner>,
}

impl Staged {
    /// Merged run `token` begins under its file's `compress_runs` (`compress`): its records compacted when the stream's
    /// rows have a `shape` (the run's bytes begun with their header, `write_run_rows`'s, its blocks stored raw), else its
    /// blocks LZ4-compressed. E-kern R: `owner`, the merged run's stream owner, takes the buffers (made anew, empty, when
    /// it is not the one they are on).
    pub fn begin(&mut self, token: u32, compress: bool, shape: Option<&RowShape>, owner: Option<memledger::Owner>) {
        if owner != self.owner {
            self.raw = memledger::ChargedVec::<u8>::new(owner, memledger::Kind::Sort);
            self.enc = memledger::ChargedVec::<u8>::new(owner, memledger::Kind::Sort);
            self.scratch = memledger::ChargedVec::<u8>::new(owner, memledger::Kind::Sort);
            self.owner = owner;
        }
        self.token = Some(token);
        self.rows = if compress { shape.cloned().map(|s| RowEncoder::owned(s, owner)) } else { None };
        (self.head, self.mark) = (self.rows.is_some(), self.rows.is_some());
        self.compress = compress && self.rows.is_none();
    }

    /// `bytes` appended (a compressed run's records compacted); the epoch's bytes past `EPOCH` handed to `write`.
    pub fn push(&mut self, bytes: &[u8], write: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
        if core::mem::take(&mut self.head) {
            let mut h = memledger::ChargedVec::<u8>::new(self.owner, memledger::Kind::Sort);
            if let Some(r) = &self.rows {
                put_header(&r.shape, &mut h.vec_mut());
            }
            self.push_raw(&h, write)?;
        }
        let Some(mut rows) = self.rows.take() else { return self.push_raw(bytes, write) };
        let mut tmp = core::mem::take(&mut self.scratch);
        tmp.clear();
        let r = rows.push(bytes, &mut tmp.vec_mut());
        let r = r.and_then(|()| self.push_raw(&tmp, write));
        (self.rows, self.scratch) = (Some(rows), tmp);
        r
    }

    /// `bytes` appended as they are; the epoch's bytes past `EPOCH` handed to `write`.
    fn push_raw(&mut self, mut bytes: &[u8], write: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
        while !bytes.is_empty() {
            let take = (BLOCK - self.raw.len()).min(bytes.len());
            self.raw.with_vec(|v| v.extend_from_slice(&bytes[..take])); // alloc: Exec charged self.owner
            bytes = &bytes[take..];
            if self.raw.len() == BLOCK {
                encode_marked(&self.raw, &mut self.enc.vec_mut(), self.compress, core::mem::take(&mut self.mark));
                self.raw.clear();
                if self.enc.len() >= EPOCH {
                    write(&self.enc)?;
                    self.enc.clear();
                }
            }
        }
        Ok(())
    }

    /// The bytes held handed to `write` as the run's last epoch; the staging emptied.
    pub fn finish(&mut self, write: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
        let rows = self.rows.take().map_or(Ok(()), |mut r| r.finish());
        self.head = false;
        if !self.raw.is_empty() {
            encode_marked(&self.raw, &mut self.enc.vec_mut(), self.compress, core::mem::take(&mut self.mark));
            self.raw.clear();
        }
        let r = if self.enc.is_empty() { Ok(()) } else { write(&self.enc) };
        self.enc.clear();
        self.token = None;
        rows.and(r)
    }
}

/// A run's reader over its encoded part: one block decoded at a time.
pub struct Decoder {
    inner: Box<dyn RunReader>,
    raw: Vec<u8>,
    pos: usize,
    enc: Vec<u8>,
    /// D2572-c: whether the run's first block is marked `HEADED` (`None` before it is read).
    first: Option<bool>,
}

impl Decoder {
    pub fn new(inner: Box<dyn RunReader>) -> Decoder {
        Decoder { inner, raw: Vec::new(), pos: 0, enc: Vec::new(), first: None } // alloc: Exec
    }

    /// D2572-c: whether the run begins with its rows' header (its first block read ahead when none was).
    pub fn headed(&mut self) -> Result<bool> {
        if self.first.is_none() {
            match self.next_block()? {
                Some((raw_len, enc_len, stored)) => {
                    self.raw.resize(raw_len, 0); // alloc: Exec
                    Self::decode(&self.enc[..enc_len], &mut self.raw, stored)?;
                    self.pos = 0;
                }
                None => self.first = Some(false),
            }
        }
        Ok(self.first == Some(true))
    }

    /// `dst` filled from the inner reader; false at its end before the first byte when `may_end` (a block cut short is an
    /// error).
    fn fill(inner: &mut dyn RunReader, dst: &mut [u8], may_end: bool) -> Result<bool> {
        let mut got = 0;
        let mut step = cancelpoll::step("spill run read: a block's bytes read");
        while got < dst.len() {
            step.tick()?;
            let n = inner.read(&mut dst[got..])?;
            if n == 0 {
                if got == 0 && may_end {
                    return Ok(false);
                }
                return Err(Error::host(format!("gravity: spill: a sort run's block cut short ({got} of {} bytes)", dst.len()))); // alloc: Cold
            }
            got += n;
        }
        Ok(true)
    }

    /// The next block's header and encoded bytes read (`self.enc[..enc_len]`): `(raw_len, enc_len, stored)`, `None` at
    /// the run's end.
    fn next_block(&mut self) -> Result<Option<(usize, usize, bool)>> {
        let mut hdr = [0u8; HDR];
        if !Self::fill(self.inner.as_mut(), &mut hdr, true)? {
            return Ok(None);
        }
        let word = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
        let (raw_len, stored) = ((word & !(STORED | HEADED)) as usize, word & STORED != 0);
        match self.first {
            None => self.first = Some(word & HEADED != 0),
            Some(_) if word & HEADED != 0 => return Err(Error::host("gravity: spill: a sort run's block past its first is marked headed")),
            Some(_) => {}
        }
        let enc_len = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]) as usize;
        if raw_len > BLOCK || enc_len > lz4_flex::block::get_maximum_output_size(BLOCK) || (stored && enc_len != raw_len) {
            return Err(Error::host(format!("gravity: spill: a sort run's block header reads {raw_len} raw / {enc_len} encoded bytes"))); // alloc: Cold
        }
        if self.enc.len() < enc_len {
            self.enc.resize(enc_len, 0); // alloc: Exec
        }
        Self::fill(self.inner.as_mut(), &mut self.enc[..enc_len], false)?;
        Ok(Some((raw_len, enc_len, stored)))
    }

    /// The block in `self.enc` decoded into `dst` (exactly `dst.len()` bytes).
    fn decode(enc: &[u8], dst: &mut [u8], stored: bool) -> Result<()> {
        if stored {
            dst.copy_from_slice(&enc[..dst.len()]);
            return Ok(());
        }
        match lz4_flex::block::decompress_into(enc, dst) {
            Ok(n) if n == dst.len() => Ok(()),
            Ok(n) => Err(Error::host(format!("gravity: spill: a sort run's block decoded to {n} of {} bytes", dst.len()))), // alloc: Cold
            Err(e) => Err(Error::host(format!("gravity: spill: a sort run's block does not decode: {e}"))), // alloc: Cold
        }
    }
}

impl RunReader for Decoder {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let mut n = 0;
        let mut step = cancelpoll::step("spill run read: a run's blocks decoded");
        while n < buf.len() {
            step.tick()?;
            if self.pos < self.raw.len() {
                let k = (self.raw.len() - self.pos).min(buf.len() - n);
                buf[n..n + k].copy_from_slice(&self.raw[self.pos..self.pos + k]);
                self.pos += k;
                n += k;
                continue;
            }
            let Some((raw_len, enc_len, stored)) = self.next_block()? else { break };
            if buf.len() - n >= raw_len {
                // the caller's room holds the whole block: decoded straight into it
                Self::decode(&self.enc[..enc_len], &mut buf[n..n + raw_len], stored)?;
                n += raw_len;
            } else {
                self.raw.resize(raw_len, 0); // alloc: Exec
                Self::decode(&self.enc[..enc_len], &mut self.raw, stored)?;
                self.pos = 0;
            }
        }
        Ok(n)
    }
}

// D2572-c: compact run rows. A run row is laid for compares: its head is 8-byte words (the key pairs, the datum words,
// the null bytes, the record word, the images, the var slots), mostly zero high bytes, and a var run's record carries
// pointer words (the record's length, each slot's offset) and the copies' zero padding, all of them rebuilt from the
// sizes. The row codec writes each record without them, losslessly for any bytes: a record whose pointers or padding do
// not follow the layout is kept as it lies.

/// A run row's shape as the row codec reads it: the fixed head's bytes (a multiple of 8) and, for a var run, its var
/// layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowShape {
    pub head: u32,
    pub var: Option<VarRows>,
    /// Per head word, the bits the codec writes: all but a var record's pointers (the record word's low `u32`, each
    /// slot's offset), rebuilt on read.
    masks: Vec<u64>,
}

/// A var run's records (kernels ops/sort/runs.rs `RunShape::compact`): each its head, then the copy of each non-null var
/// slot in slot order, `var_alloc(size)` bytes each; a slot is `[size u32, off u32]` with `off` from the slot to its
/// copy, and the record word's low `u32` (at `rec_off`) the record's bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VarRows {
    pub rec_off: u32,
    pub slots: Vec<u32>,
}

impl RowShape {
    /// The shape, or `None` when the codec cannot read it (a head not in words, a record word or a slot past the head);
    /// the run is then kept as it lies.
    pub fn new(head: u32, var: Option<VarRows>) -> Option<RowShape> {
        if head == 0 || head % 8 != 0 {
            return None;
        }
        let mut masks = vec![u64::MAX; head as usize / 8]; // alloc: Exec
        if var.is_none() {
            return Some(RowShape { head, var, masks });
        }
        let v = var.as_ref()?;
        if v.slots.len() > u16::MAX as usize {
            return None;
        }
        // the record word's low u32 and each slot's 8 bytes, all inside the head and none over another
        let mut spans: Vec<(u64, u64)> = v.slots.iter().map(|&s| (s as u64, s as u64 + 8)).collect(); // alloc: Exec
        spans.push((v.rec_off as u64, v.rec_off as u64 + 4));
        // bounded("key-columns: one per var column of a sort run row, at most MaxTupleAttributeNumber (1664) columns; a span each, PG polls no deeper")
        spans.sort_unstable();
        let ok = spans.iter().all(|&(a, b)| a % 4 == 0 && b <= head as u64) && spans.windows(2).all(|w| w[0].1 <= w[1].0);
        if !ok {
            return None;
        }
        // a 4-aligned u32 sits inside one word
        // bounded("key-columns: one per var column of a sort run row, at most MaxTupleAttributeNumber (1664) columns; a mask each, PG polls no deeper")
        for at in core::iter::once(v.rec_off).chain(v.slots.iter().map(|&s| s + 4)) {
            masks[at as usize / 8] &= !(0xffff_ffffu64 << (8 * (at % 8)));
        }
        Some(RowShape { head, var, masks })
    }
}

/// A by-reference copy's bytes in a var run: the kernels' `var_alloc` (ops/sort/rowsort.rs), the size 8-rounded and at
/// least 16.
#[inline(always)]
const fn var_alloc(size: u32) -> u32 {
    let a = size.next_multiple_of(8);
    if a < 16 { 16 } else { a }
}

#[inline(always)]
fn rd32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}

#[inline(always)]
fn wr32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

#[inline(always)]
fn word_at(w: &[u8]) -> u64 {
    u64::from_le_bytes(w[..8].try_into().expect("8 bytes"))
}

/// A word's bytes without its zero high bytes.
#[inline(always)]
fn sig(w: u64) -> usize {
    8 - (w.leading_zeros() / 8) as usize
}

/// A head word's tag: its significant bytes (0 - 8), or `XOR_TAG +` those of its difference from the record before's
/// same word (0 - `XOR_LONGEST`, the tag's last value).
const XOR_TAG: usize = 9;
const XOR_LONGEST: usize = 0xf - XOR_TAG;

/// `head`'s words (each with its shape's `masks`: a pointer rebuilt on read is written zero) appended to `out` against
/// the record before's head (`prev`, masked the same): a 4-bit tag per word (two to a byte), then the bytes of each, the
/// word itself without its zero high bytes or its difference from the word before, whichever is shorter (a sorted run's
/// neighbours share their key words' high bytes and their flags).
#[inline(always)]
fn put_words(head: &[u8], prev: &[u8], masks: &[u64], out: &mut memledger::ChargedVecMut<'_, u8>) {
    let (words, tags) = (masks.len(), out.len());
    let body = tags + words.div_ceil(2);
    // room for every word whole: each written as 8 bytes, the next written over its zero high bytes
    out.resize(body + 8 * words, 0); // alloc: Exec charged out.owner()
    let (tag, rest) = out[tags..].split_at_mut(words.div_ceil(2));
    // each tag byte gathered in a register and stored once (two words' tags)
    let (mut at, mut pair) = (0, 0u8);
    // bounded("key-columns: one per 8-byte head word of a sort run row, a few words per column of at most MaxTupleAttributeNumber (1664) columns; a word coded each, PG polls no deeper")
    for (i, ((w, p), &m)) in head.chunks_exact(8).zip(prev.chunks_exact(8)).zip(masks).enumerate() {
        let (w, p) = (word_at(w) & m, word_at(p) & m);
        let (n, x) = (sig(w), sig(w ^ p));
        let (t, v, k) = if x <= XOR_LONGEST && x < n { (XOR_TAG + x, w ^ p, x) } else { (n, w, n) };
        pair |= (t as u8) << (4 * (i & 1));
        if i & 1 == 1 {
            tag[i / 2] = pair;
            pair = 0;
        }
        rest[at..at + 8].copy_from_slice(&v.to_le_bytes());
        at += k;
    }
    if words & 1 == 1 {
        tag[words / 2] = pair;
    }
    out.truncate(body + at);
}

/// The words `put_words` wrote read back from `enc` into `dst` (`masks.len()` words) against the record before's head
/// (`prev`): the bytes consumed, `None` when `enc` holds less than all of them. A masked pointer reads back zero.
#[inline(always)]
fn get_words(enc: &[u8], dst: &mut [u8], prev: &[u8], masks: &[u64]) -> Result<Option<usize>> {
    let words = masks.len();
    let tags = words.div_ceil(2);
    if enc.len() < tags {
        return Ok(None);
    }
    let mut at = tags;
    // bounded("key-columns: one per 8-byte head word of a sort run row, a few words per column of at most MaxTupleAttributeNumber (1664) columns; a word decoded each, PG polls no deeper")
    for (i, ((d, p), &m)) in dst.chunks_exact_mut(8).zip(prev.chunks_exact(8)).zip(masks).enumerate() {
        let t = (enc[i / 2] >> (4 * (i & 1)) & 0xf) as usize;
        let n = if t >= XOR_TAG { t - XOR_TAG } else { t };
        if n > 8 {
            return Err(Error::host(format!("gravity: spill: a sort run's row word reads tag {t}"))); // alloc: Cold
        }
        let v = match enc.get(at..at + 8) {
            Some(b) => word_at(b) & if n == 8 { u64::MAX } else { (1u64 << (8 * n)) - 1 },
            None if enc.len() < at + n => return Ok(None),
            None => {
                let mut b = [0u8; 8];
                b[..n].copy_from_slice(&enc[at..at + n]);
                u64::from_le_bytes(b)
            }
        };
        let w = if t >= XOR_TAG { v ^ (word_at(p) & m) } else { v };
        d.copy_from_slice(&w.to_le_bytes());
        at += n;
    }
    Ok(Some(at))
}

/// A var record's flags byte: its slot offsets and record word rebuilt (`PTRS`), its copies' zero padding dropped
/// (`PADS`), copies equal to the record before's in their slot left out (`SAME`: a bitmap of the slots follows the flags),
/// each copy written without its 4-byte varlena header when that header is its size (`HDRS`: `size << 2`, a plain
/// varlena's); 0 is a record kept as it lies (`u32` length, the bytes).
const PTRS: u8 = 1;
const PADS: u8 = 2;
const SAME: u8 = 4;
const HDRS: u8 = 8;

/// The record before's (`prev`, its whole bytes) copy in slot `so` when it is `size` bytes: where its `n` bytes start.
#[inline(always)]
fn copy_at(prev: &[u8], so: usize, size: u32, n: usize) -> Option<usize> {
    if prev.len() < so + 8 || rd32(prev, so) != size {
        return None;
    }
    let at = so + rd32(prev, so + 4) as usize;
    (at + n <= prev.len()).then_some(at)
}

/// The record at the front of `raw`'s length (a fixed head, or a var record's record word), `None` when `raw` holds
/// less than its head.
#[inline(always)]
pub(crate) fn record_len(shape: &RowShape, raw: &[u8]) -> Option<usize> {
    let h = shape.head as usize;
    match &shape.var {
        _ if raw.len() < h => None,
        None => Some(h),
        Some(v) => Some(rd32(raw, v.rec_off as usize) as usize),
    }
}

/// One record (`rec`, its whole bytes) appended to `out` compacted against the record before (`prev`, its whole bytes;
/// the shape's zero head before a run's first record).
fn encode_record(shape: &RowShape, rec: &[u8], prev: &[u8], out: &mut memledger::ChargedVecMut<'_, u8>) {
    let h = shape.head as usize;
    let Some(v) = &shape.var else {
        put_words(rec, prev, &shape.masks, out);
        return;
    };
    // the layout the record should follow: its copies after the head in slot order, each var_alloc'd, a NULL's slot zero
    let (mut at, mut ptrs, mut pads) = (h, rec.len() >= h, true);
    if ptrs {
        // bounded("key-columns: one per var column of a sort run row, at most MaxTupleAttributeNumber (1664) columns; a datum compared or copied each, PG polls no deeper")
        for &so in &v.slots {
            let so = so as usize;
            let size = rd32(rec, so);
            if size == 0 {
                ptrs &= rd32(rec, so + 4) == 0;
                continue;
            }
            let len = var_alloc(size) as usize;
            ptrs &= rd32(rec, so + 4) as usize == at - so && at + len <= rec.len();
            if !ptrs {
                break;
            }
            pads &= rec[at + size as usize..at + len].iter().all(|&b| b == 0);
            at += len;
        }
        ptrs &= at == rec.len() && rd32(rec, v.rec_off as usize) as usize == at;
    }
    if !ptrs {
        out.push(0); // alloc: Exec charged out.owner()
        out.extend_from_slice(&(rec.len() as u32).to_le_bytes()); // alloc: Exec charged out.owner()
        out.extend_from_slice(rec); // alloc: Exec charged out.owner()
        return;
    }
    // the copies equal to the record before's in their slot (a sorted run's neighbours repeat a key's value): a bit
    // each, on the stack for up to 64 slots
    let nmap = v.slots.len().div_ceil(8);
    let (mut small, mut big) = ([0u8; 8], Vec::new()); // alloc: Exec
    let bits: &mut [u8] = if nmap <= small.len() {
        &mut small[..nmap]
    } else {
        big.resize(nmap, 0);
        &mut big[..]
    };
    let (mut at, mut same, mut hdrs) = (h, false, true);
    // bounded("key-columns: one per var column of a sort run row, at most MaxTupleAttributeNumber (1664) columns; a datum compared or copied each, PG polls no deeper")
    for (c, &so) in v.slots.iter().enumerate() {
        let size = rd32(rec, so as usize);
        if size == 0 {
            continue;
        }
        let len = var_alloc(size) as usize;
        let n = if pads { size as usize } else { len };
        if copy_at(prev, so as usize, size, n).is_some_and(|p| prev[p..p + n] == rec[at..at + n]) {
            bits[c / 8] |= 1 << (c % 8);
            same = true;
        } else {
            hdrs &= size >= 4 && rd32(rec, at) == size << 2;
        }
        at += len;
    }
    out.push(PTRS | if pads { PADS } else { 0 } | if same { SAME } else { 0 } | if hdrs { HDRS } else { 0 }); // alloc: Exec charged out.owner()
    if same {
        out.extend_from_slice(bits); // alloc: Exec charged out.owner()
    }
    // the head (its pointers masked: rebuilt on read)
    put_words(&rec[..h], &prev[..h], &shape.masks, out);
    let mut at = h;
    // bounded("key-columns: one per var column of a sort run row, at most MaxTupleAttributeNumber (1664) columns; a datum compared or copied each, PG polls no deeper")
    for (c, &so) in v.slots.iter().enumerate() {
        let size = rd32(rec, so as usize) as usize;
        if size != 0 {
            let len = var_alloc(size as u32) as usize;
            if bits[c / 8] & 1 << (c % 8) == 0 {
                out.extend_from_slice(&rec[at + 4 * hdrs as usize..at + if pads { size } else { len }]); // alloc: Exec charged out.owner()
            }
            at += len;
        }
    }
}

/// The record at the front of `enc` decoded onto `out` against the record before (`out[prev]`, its whole bytes; the
/// shape's zero head, `zero`, before a run's first record): the bytes consumed, `None` (with `out` as it was) when `enc`
/// holds less than the whole record.
fn decode_record(shape: &RowShape, enc: &[u8], out: &mut Vec<u8>, prev: Option<(usize, usize)>, zero: &[u8]) -> Result<Option<usize>> {
    let h = shape.head as usize;
    let base = out.len();
    let Some(v) = &shape.var else {
        out.resize(base + h, 0); // alloc: Exec
        let (before, cur) = out.split_at_mut(base);
        let r = get_words(enc, cur, prev.map_or(zero, |(a, _)| &before[a..a + h]), &shape.masks);
        if !matches!(r, Ok(Some(_))) {
            out.truncate(base);
        }
        return r;
    };
    let Some(&f) = enc.first() else { return Ok(None) };
    if f == 0 {
        if enc.len() < 5 {
            return Ok(None);
        }
        let len = rd32(enc, 1) as usize;
        if enc.len() < 5 + len {
            return Ok(None);
        }
        out.extend_from_slice(&enc[5..5 + len]);
        return Ok(Some(5 + len));
    }
    if f & !(PTRS | PADS | SAME | HDRS) != 0 || f & PTRS == 0 {
        return Err(Error::host(format!("gravity: spill: a sort run's record flags read {f:#x}"))); // alloc: Cold
    }
    let map = if f & SAME != 0 { v.slots.len().div_ceil(8) } else { 0 };
    if enc.len() < 1 + map {
        return Ok(None);
    }
    let same = |c: usize| map != 0 && enc[1 + c / 8] & 1 << (c % 8) != 0;
    out.resize(base + h, 0); // alloc: Exec
    let words = {
        let (before, cur) = out.split_at_mut(base);
        get_words(&enc[1 + map..], cur, prev.map_or(zero, |(a, _)| &before[a..a + h]), &shape.masks)
    };
    let mut at_enc = match words {
        Ok(Some(n)) => 1 + map + n,
        r => {
            out.truncate(base);
            return r;
        }
    };
    let mut at = h;
    // bounded("key-columns: one per var column of a sort run row, at most MaxTupleAttributeNumber (1664) columns; a datum compared or copied each, PG polls no deeper")
    for (c, &so) in v.slots.iter().enumerate() {
        let so = so as usize;
        let size = rd32(&out[base..], so) as usize;
        if size == 0 {
            continue;
        }
        let len = var_alloc(size as u32) as usize;
        let n = if f & PADS != 0 { size } else { len };
        wr32(&mut out[base..], so + 4, (at - so) as u32);
        if same(c) {
            let Some(p) = prev.and_then(|(a, b)| copy_at(&out[a..b], so, size as u32, n).map(|p| a + p)) else {
                out.truncate(base);
                return Err(Error::host("gravity: spill: a sort run's record repeats a copy the record before does not hold"));
            };
            out.extend_from_within(p..p + n); // alloc: Exec
        } else {
            // a copy without its header (`HDRS`): the header rebuilt from its size
            let hdr = if f & HDRS != 0 { 4 } else { 0 };
            if n < hdr {
                out.truncate(base);
                return Err(Error::host(format!("gravity: spill: a sort run's copy of {size} bytes without its header"))); // alloc: Cold
            }
            if enc.len() < at_enc + n - hdr {
                out.truncate(base);
                return Ok(None);
            }
            if hdr != 0 {
                out.extend_from_slice(&((size as u32) << 2).to_le_bytes());
            }
            out.extend_from_slice(&enc[at_enc..at_enc + n - hdr]);
            at_enc += n - hdr;
        }
        out.resize(base + at + len, 0); // alloc: Exec
        at += len;
    }
    wr32(&mut out[base..], v.rec_off as usize, at as u32);
    Ok(Some(at_enc))
}

/// A run's records compacted as they come: `push` encodes each whole record onto its `out` and holds a record cut
/// across two pushes until its rest comes (a merge's writes).
/// E-kern R: its own buffers on `owner` (the run's slot owner, `RowEncoder::new`'s).
pub struct RowEncoder {
    pub shape: RowShape,
    carry: memledger::ChargedVec<u8>,
    /// The record before when it is not in the bytes being pushed (a push's last, a carried one); empty before the first.
    last: memledger::ChargedVec<u8>,
    zero: memledger::ChargedVec<u8>,
}

impl RowEncoder {
    pub fn new(shape: RowShape) -> RowEncoder {
        RowEncoder::owned(shape, None)
    }

    /// E-kern R: the encoder's buffers (carry, last, the head's zeros) charged to `owner` by construction (None: uncharged).
    pub fn owned(shape: RowShape, owner: Option<memledger::Owner>) -> RowEncoder {
        let mut zero = memledger::ChargedVec::<u8>::new(owner, memledger::Kind::Sort);
        zero.with_vec(|v| v.resize(shape.head as usize, 0)); // alloc: Exec charged owner
        let (carry, last) = (memledger::ChargedVec::<u8>::new(owner, memledger::Kind::Sort), memledger::ChargedVec::<u8>::new(owner, memledger::Kind::Sort));
        RowEncoder { shape, carry, last, zero }
    }

    /// E-kern R: a new encoder rebuilt on `owner` (its buffers hold nothing yet but the head's zeros).
    pub fn charge_to(self, owner: Option<memledger::Owner>) -> RowEncoder {
        RowEncoder::owned(self.shape, owner)
    }

    /// `bytes`' whole records appended to `out` compacted; a cut record held.
    pub fn push(&mut self, bytes: &[u8], out: &mut memledger::ChargedVecMut<'_, u8>) -> Result<()> {
        let mut rest = bytes;
        // bounded("other: completes the one record a push before cut: a pass to its head, a pass to its length (record_len reads it from the head, then fixed), then the carry is coded and emptied")
        while !self.carry.is_empty() && !rest.is_empty() {
            let need = match record_len(&self.shape, &self.carry) {
                Some(n) => n,
                None => self.shape.head as usize,
            };
            if need < self.shape.head as usize {
                return Err(Error::host(format!("gravity: spill: a sort run's record reads {need} bytes"))); // alloc: Cold
            }
            let take = need.saturating_sub(self.carry.len()).min(rest.len());
            self.carry.with_vec(|v| v.extend_from_slice(&rest[..take])); // alloc: Exec charged self.carry.owner()
            rest = &rest[take..];
            if self.carry.len() == need && record_len(&self.shape, &self.carry) == Some(need) {
                let prev = if self.last.is_empty() { &self.zero[..] } else { &self.last[..] };
                encode_record(&self.shape, &self.carry, prev, out);
                core::mem::swap(&mut self.last, &mut self.carry);
                self.carry.clear();
            }
        }
        // the records whole in `rest`, each against the one before it in place; C9: a record's code is ~50 ns or more
        // (a word each), so a tick each
        let mut prev: Option<&[u8]> = None;
        let mut step = cancelpoll::step("spill run encode: a piece's records coded");
        while let Some(n) = record_len(&self.shape, rest) {
            step.tick()?;
            if n < self.shape.head as usize {
                return Err(Error::host(format!("gravity: spill: a sort run's record reads {n} bytes"))); // alloc: Cold
            }
            if rest.len() < n {
                break;
            }
            let p = prev.unwrap_or(if self.last.is_empty() { &self.zero[..] } else { &self.last[..] });
            encode_record(&self.shape, &rest[..n], p, out);
            prev = Some(&rest[..n]);
            rest = &rest[n..];
        }
        if let Some(p) = prev {
            self.last.clear();
            self.last.with_vec(|v| v.extend_from_slice(p)); // alloc: Exec charged self.last.owner()
        }
        self.carry.with_vec(|v| v.extend_from_slice(rest)); // alloc: Exec charged self.carry.owner()
        Ok(())
    }

    /// The run ended: an error when a record was cut short.
    pub fn finish(&mut self) -> Result<()> {
        if self.carry.is_empty() {
            return Ok(());
        }
        let n = self.carry.len();
        self.carry.clear();
        Err(Error::host(format!("gravity: spill: a sort run ends inside a record ({n} bytes held)"))) // alloc: Cold
    }
}

/// A compacted run read back as its records' bytes (over its block `Decoder`).
pub struct RowReader {
    /// E-kern R: the header's slot list (`shape`'s) claimed for the run's stream owner (`reader`), FIRST so it drops before
    /// the bytes it counts; `None` for a shape not read off the run (`resume`'s copy) or no owner.
    claim: Option<HeapClaim>,
    inner: Box<dyn RunReader>,
    shape: RowShape,
    zero: Vec<u8>,
    enc: Vec<u8>,
    lo: usize,
    hi: usize,
    /// The records decoded, served from `pos`; the record before the first of them kept at the front (`prev`, not served).
    out: Vec<u8>,
    pos: usize,
    prev: Option<(usize, usize)>,
    done: bool,
}

impl RowReader {
    pub fn new(inner: Box<dyn RunReader>, shape: RowShape) -> RowReader {
        let zero = vec![0; shape.head as usize]; // alloc: Exec
        RowReader { claim: None, inner, shape, zero, enc: vec![0; BLOCK], lo: 0, hi: 0, out: Vec::new(), pos: 0, prev: None, done: false } // alloc: Exec
    }

    /// D3190: a compacted run read from a fence on: `inner` its coded records past the fenced record, each decoded
    /// against the record before, the first against the fence's copy `prev` (never served).
    pub fn resume(inner: Box<dyn RunReader>, shape: RowShape, prev: &[u8]) -> RowReader {
        let mut r = RowReader::new(inner, shape);
        r.out.extend_from_slice(prev); // alloc: Exec
        (r.prev, r.pos) = (Some((0, prev.len())), prev.len());
        r
    }

    /// The next records decoded into `out` (about a block's worth); false at the run's end. `step` ticks per record and
    /// per inner read.
    fn refill(&mut self, step: &mut Step) -> Result<bool> {
        // the record before stays at the front, the next record coded against it
        match self.prev {
            Some((a, _)) => {
                self.out.drain(..a);
                self.prev = Some((0, self.out.len()));
            }
            None => self.out.clear(),
        }
        self.pos = self.out.len();
        loop {
            step.tick()?;
            while self.out.len() - self.pos < BLOCK {
                step.tick()?;
                let base = self.out.len();
                match decode_record(&self.shape, &self.enc[self.lo..self.hi], &mut self.out, self.prev, &self.zero)? {
                    Some(n) => {
                        self.lo += n;
                        self.prev = Some((base, self.out.len()));
                    }
                    None => break,
                }
            }
            if self.out.len() > self.pos {
                return Ok(true);
            }
            if self.done {
                if self.lo < self.hi {
                    return Err(Error::host(format!("gravity: spill: a sort run's last record cut short ({} bytes)", self.hi - self.lo))); // alloc: Cold
                }
                return Ok(false);
            }
            self.enc.copy_within(self.lo..self.hi, 0);
            (self.hi, self.lo) = (self.hi - self.lo, 0);
            if self.hi == self.enc.len() {
                self.enc.resize(2 * self.enc.len(), 0); // alloc: Exec
            }
            let n = self.inner.read(&mut self.enc[self.hi..])?;
            self.hi += n;
            self.done = n == 0;
        }
    }
}

impl RunReader for RowReader {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let mut n = 0;
        let mut step = cancelpoll::step("spill run read: a compacted run's records decoded");
        while n < buf.len() {
            step.tick()?;
            if self.pos == self.out.len() && !self.refill(&mut step)? {
                break;
            }
            let k = (self.out.len() - self.pos).min(buf.len() - n);
            buf[n..n + k].copy_from_slice(&self.out[self.pos..self.pos + k]);
            (self.pos, n) = (self.pos + k, n + k);
        }
        Ok(n)
    }
}

/// A compacted run's header (the first bytes of its blocks' stream, its first block marked `HEADED`): `MODE_ROWS`, then its
/// shape (the head's bytes `u32`, a var flag `u8`, then for a var run the record word's offset `u32`, the slots `u16` and
/// each slot's offset `u32`). Any other run has none (`write_run`'s bytes).
const MODE_ROWS: u8 = 1;

fn put_header(s: &RowShape, out: &mut memledger::ChargedVecMut<'_, u8>) {
    out.push(MODE_ROWS); // alloc: Exec charged out.owner()
    out.extend_from_slice(&s.head.to_le_bytes()); // alloc: Exec charged out.owner()
    out.push(s.var.is_some() as u8); // alloc: Exec charged out.owner()
    if let Some(v) = &s.var {
        out.extend_from_slice(&v.rec_off.to_le_bytes()); // alloc: Exec charged out.owner()
        out.extend_from_slice(&(v.slots.len() as u16).to_le_bytes()); // alloc: Exec charged out.owner()
        v.slots.iter().for_each(|x| out.extend_from_slice(&x.to_le_bytes())); // alloc: Exec charged out.owner()
    }
}

/// `dst` filled from `r`: false when `r` ends before its first byte and `may_end`; a stream cut inside `dst` is an error.
fn read_exact(r: &mut dyn RunReader, dst: &mut [u8], may_end: bool) -> Result<bool> {
    Decoder::fill(r, dst, may_end)
}

/// A headed run's header read off the front of its stream: its rows' shape.
fn get_header(r: &mut dyn RunReader) -> Result<RowShape> {
    let mut b = [0u8; 4];
    read_exact(r, &mut b[..1], false)?;
    if b[0] != MODE_ROWS {
        return Err(Error::host(format!("gravity: spill: a sort run's header reads mode {}", b[0]))); // alloc: Cold
    }
    read_exact(r, &mut b, false)?;
    let head = u32::from_le_bytes(b);
    read_exact(r, &mut b[..1], false)?;
    let var = if b[0] == 0 {
        None
    } else {
        read_exact(r, &mut b, false)?;
        let rec_off = u32::from_le_bytes(b);
        read_exact(r, &mut b[..2], false)?;
        let mut slots = vec![0u32; u16::from_le_bytes([b[0], b[1]]) as usize]; // alloc: Exec
        let mut step = cancelpoll::step("spill run read: a run's header");
        for x in slots.iter_mut() {
            step.tick()?;
            read_exact(r, &mut b, false)?;
            *x = u32::from_le_bytes(b);
        }
        Some(VarRows { rec_off, slots })
    };
    RowShape::new(head, var).ok_or_else(|| Error::host("gravity: spill: a sort run's header names a shape the codec cannot read"))
}

/// D4373: a run's coded copy (`RowEncoder`'s output, about 3/4 of the run), its bytes taken from the grant pool's room now
/// before it is allocated (`region::scratch_take`) and held while it lives; `None` when the pool has no room: the run is
/// written as `write_run` writes it (blocks of the run itself, LZ4, no copy of the run), never refused.
/// LEDGER2 step 5: its heap bytes (door 1 counts them on the account) claimed for `owner`, the run's slot owner (a door-1
/// owned charge of its capacity, dropped with it), so they leave the account's unattributed row. CONVERSION WAVE 2: a
/// copy the encode grew past `cap` (rows the shape compacts less than 3/4) has its claim follow it (`HeapClaim::fit`).
fn coded_copy(raw: usize, owner: Option<memledger::Owner>) -> Option<(crate::mem::grant::Held, memledger::ChargedVec<u8>)> {
    let cap = raw * 3 / 4 + 64;
    let held = crate::mem::region::scratch_take(cap as u64)?;
    // E-kern R: a ChargedVec (was a Vec and its HeapClaim): the coders grow it through its guard, refitted at the guard's drop
    let mut out = memledger::ChargedVec::<u8>::new(owner, memledger::Kind::Sort);
    out.with_vec(|v| v.reserve_exact(cap)); // alloc: Exec charged owner
    Some((held, out))
}

/// `raw` written as part `part` of `file`: as `write_run` writes it, but a compressing file's run of rows with a `shape`
/// compacted (`RowEncoder`) behind its header, its blocks stored raw (rows that do not follow the shape: the run as
/// `write_run` writes it, LZ4).
/// `owner`: the run's slot owner, whose the coded copy is (`coded_copy`; `None`: no one's).
pub fn write_run_rows(file: &mut dyn SpillFile, part: u32, raw: &[u8], shape: Option<&RowShape>, spilled: u64, owner: Option<memledger::Owner>) -> Result<()> {
    let Some(s) = shape.filter(|_| !raw.is_empty() && file.compress_run(spilled, raw.len() as u64)) else { return write_run(file, part, raw, spilled) };
    let Some((_held, mut out)) = coded_copy(raw.len(), owner) else { return write_run(file, part, raw, spilled) };
    let coded = {
        let mut o = out.vec_mut();
        put_header(s, &mut o);
        let mut e = RowEncoder::new(s.clone());
        e = e.charge_to(owner);
        // D3964 (C9): coded a piece a tick (a sort run's whole encode held a cancel 2.5 s); a record a piece cuts carries
        // into the next (`RowEncoder::push`)
        let mut step = cancelpoll::step("spill run encode: a run's rows coded");
        step.pieces(raw.len(), cancelpoll::PIECE, |r| e.push(&raw[r], &mut o)).and_then(|()| e.finish())
    };
    match coded {
        Ok(()) => write_blocks(file, part, &out, false, true),
        Err(Error::Cancelled) => Err(Error::Cancelled),
        Err(_) => write_run(file, part, raw, spilled),
    }
}

/// D3190: `write_run_rows`, the run's fences built as it is written (`fence::Fences`): `None` without a shape or for
/// rows the shape does not frame. The bytes written are `write_run_rows`'s: a compacted run's records are coded in
/// pieces that end at each fence's record, the coder's record before carried across them.
pub fn put_run(file: &mut dyn SpillFile, part: u32, raw: &[u8], shape: Option<&RowShape>, spilled: u64, owner: Option<memledger::Owner>) -> Result<Option<Fences>> {
    let Some((s, picks)) = shape.and_then(|s| fence::pick(s, raw).map(|p| (s, p))) else {
        write_run_rows(file, part, raw, shape, spilled, owner)?;
        return Ok(None);
    };
    // the blocks' and the fences' offsets, one word a block / a fence (data-sized), on the run's slot owner until Fences
    // copies them
    let mut starts = memledger::ChargedVec::<u64>::new(owner, memledger::Kind::Sort);
    starts.with_vec(|v| v.reserve_exact(raw.len() / BLOCK + 1)); // alloc: Exec charged owner
    if let Some((_held, mut out)) = (!raw.is_empty() && file.compress_run(spilled, raw.len() as u64)).then(|| coded_copy(raw.len(), owner)).flatten() {
        let mut o = out.vec_mut();
        put_header(s, &mut o);
        let mut e = RowEncoder::new(s.clone());
        e = e.charge_to(owner);
        let mut ends = memledger::ChargedVec::<usize>::new(owner, memledger::Kind::Sort);
        ends.with_vec(|v| v.reserve_exact(picks.at.len())); // alloc: Exec charged owner
        let mut from = 0;
        // D3964 (C9): coded a piece a tick, as `write_run_rows`
        let mut step = cancelpoll::step("spill run encode: a run's rows coded");
        let mut coded = || -> Result<()> {
            for &(_, off, len) in picks.at.iter() {
                let fenced = &raw[from..off + len];
                step.pieces(fenced.len(), cancelpoll::PIECE, |r| e.push(&fenced[r], &mut o))?;
                let n = o.len();
                ends.with_vec(|v| v.push(n)); // alloc: Exec charged owner
                from = off + len;
            }
            let rest = &raw[from..];
            step.pieces(rest.len(), cancelpoll::PIECE, |r| e.push(&rest[r], &mut o))?;
            e.finish()
        };
        let coded = coded();
        drop(o);
        match coded {
            Ok(()) => {
                let total = write_blocks_at(file, part, &out, false, true, Some(&mut starts), owner)?;
                return Ok(Some(Fences::new(raw, s, picks, &ends, &starts, total, true, owner)));
            }
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(_) => starts.clear(),
        }
    }
    let mut ends = memledger::ChargedVec::<usize>::new(owner, memledger::Kind::Sort);
    ends.with_vec(|v| v.extend(picks.at.iter().map(|&(_, off, len)| off + len))); // alloc: Exec charged owner
    let compress = file.compress_run(spilled, raw.len() as u64);
    let total = write_blocks_at(file, part, raw, compress, false, Some(&mut starts), owner)?;
    Ok(Some(Fences::new(raw, s, picks, &ends, &starts, total, false, owner)))
}

/// A run's reader over its part: its blocks decoded, and a headed run's records expanded past its header. E-kern R: the
/// header's slot list (read from the file: up to 65535 words) is claimed for `owner`, the run's stream owner.
pub fn reader(inner: Box<dyn RunReader>, owner: Option<memledger::Owner>) -> Result<Box<dyn RunReader>> {
    let mut d = Decoder::new(inner);
    if !d.headed()? {
        return Ok(Box::new(d)); // alloc: Exec
    }
    let shape = get_header(&mut d)?;
    let mut r = RowReader::new(Box::new(d), shape); // alloc: Exec
    r.claim = r.shape.var.as_ref().map(|v| HeapClaim::of(owner, memledger::Kind::Sort, &v.slots));
    Ok(Box::new(r)) // alloc: Exec
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test's `RowEncoder::push` onto a plain `Vec` (the coders take a `ChargedVecMut`; this one charged to no one).
    fn push_into(e: &mut RowEncoder, bytes: &[u8], out: &mut Vec<u8>) -> Result<()> {
        let mut cv = memledger::ChargedVec::<u8>::new(None, memledger::Kind::Sort);
        let r = {
            let mut g = cv.vec_mut();
            g.extend_from_slice(out);
            e.push(bytes, &mut g)
        };
        out.clear();
        out.extend_from_slice(&cv);
        r
    }

    struct Bytes {
        b: Vec<u8>,
        pos: usize,
    }

    impl RunReader for Bytes {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
            let n = (self.b.len() - self.pos).min(buf.len()).min(5000); // short reads, as a file's
            buf[..n].copy_from_slice(&self.b[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    /// One part, its epochs appended (`write_run`'s file); `raw`: a file that does not ask for compressed runs.
    #[derive(Default)]
    struct File {
        b: Vec<u8>,
        epochs: usize,
        raw: bool,
    }

    impl SpillFile for File {
        fn write_epoch(&mut self, parts: &[(u32, &[u8])]) -> Result<()> {
            for (_, b) in parts {
                self.b.extend_from_slice(b);
            }
            self.epochs += 1;
            Ok(())
        }
        fn read_part(&self, _part: u32, sink: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
            sink(&self.b)
        }
        fn open_part(&self, _part: u32) -> Result<Option<Box<dyn RunReader>>> {
            Ok(Some(Box::new(Bytes { b: self.b.clone(), pos: 0 }))) // alloc: Cold
        }
        fn spilled_bytes(&self) -> u64 {
            self.b.len() as u64
        }
        fn compress_runs(&self) -> bool {
            !self.raw
        }
    }

    /// Sort-run-like rows (a repeated head, a counter, zero padding) or noise.
    fn rows(n: usize, noise: bool) -> Vec<u8> {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        (0..n)
            .map(|i| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                if noise { x as u8 } else if i % 40 < 24 { b"the quick fox dog\0\0\0\0\0\0\0"[i % 40] } else { (i / 40 + i % 40) as u8 }
            })
            .collect()
    }

    fn decode_all(enc: Vec<u8>, chunk: usize) -> Result<Vec<u8>> {
        let mut d = Decoder::new(Box::new(Bytes { b: enc, pos: 0 })); // alloc: Cold
        let (mut out, mut buf) = (Vec::new(), vec![0u8; chunk]); // alloc: Cold
        loop {
            let n = d.read(&mut buf)?;
            if n == 0 {
                return Ok(out);
            }
            out.extend_from_slice(&buf[..n]);
        }
    }

    #[test]
    fn a_run_reads_back_its_bytes_through_any_read_size() {
        for noise in [false, true] {
            for n in [0, 1, 17, BLOCK - 1, BLOCK, BLOCK + 1, 3 * BLOCK + 17, 40 * BLOCK + 5] {
                let raw = rows(n, noise);
                let mut f = File::default();
                write_run(&mut f, 0, &raw, 0).unwrap();
                if !noise && n >= BLOCK {
                    assert!(f.b.len() * 2 < raw.len(), "{n} B of rows: {} B encoded", f.b.len());
                }
                if noise {
                    assert!(f.b.len() <= raw.len() + raw.len().div_ceil(BLOCK) * HDR, "noise kept raw: {} of {n} B", f.b.len());
                }
                assert!(f.epochs <= f.b.len().div_ceil(EPOCH).max(1), "{} epochs for {} B", f.epochs, f.b.len());
                for chunk in [1, 7, 1000, BLOCK, 64 << 10] {
                    assert_eq!(decode_all(f.b.clone(), chunk).unwrap(), raw, "{n} B (noise {noise}) read {chunk} B at a time");
                }
            }
        }
    }

    #[test]
    fn a_file_without_a_temp_limit_keeps_every_block_raw() {
        let raw = rows(40 * BLOCK + 5, false);
        let mut f = File { raw: true, ..File::default() };
        write_run(&mut f, 0, &raw, 0).unwrap();
        assert_eq!(f.b.len(), raw.len() + raw.len().div_ceil(BLOCK) * HDR, "each block stored raw behind its header");
        assert_eq!(f.epochs, 1, "a raw run under RAW_EPOCH is one epoch");
        for chunk in [1, 1000, BLOCK, 64 << 10] {
            assert_eq!(decode_all(f.b.clone(), chunk).unwrap(), raw, "read {chunk} B at a time");
        }
        let mut st = Staged::default();
        let mut out = Vec::new(); // alloc: Cold
        for piece in raw.chunks(777) {
            st.push(piece, &mut |b| {
                out.extend_from_slice(b);
                Ok(())
            })
            .unwrap();
        }
        st.finish(&mut |b| {
            out.extend_from_slice(b);
            Ok(())
        })
        .unwrap();
        assert_eq!(out, f.b, "a merged run's staged blocks the same bytes as a put run's");
    }

    #[test]
    fn a_merged_runs_writes_staged_read_back_as_one_run() {
        let raw = rows(5 * BLOCK + 333, false);
        let mut out = Vec::new(); // alloc: Cold
        let mut st = Staged { compress: true, ..Staged::default() };
        for piece in raw.chunks(777) {
            st.push(piece, &mut |b| {
                out.extend_from_slice(b);
                Ok(())
            })
            .unwrap();
        }
        st.finish(&mut |b| {
            out.extend_from_slice(b);
            Ok(())
        })
        .unwrap();
        assert!(st.token.is_none() && st.raw.is_empty() && st.enc.is_empty(), "finish empties the staging");
        assert_eq!(decode_all(out, 4096).unwrap(), raw);
    }

    #[test]
    fn a_block_cut_short_is_an_error() {
        let raw = rows(2 * BLOCK, false);
        let mut f = File::default();
        write_run(&mut f, 0, &raw, 0).unwrap();
        for cut in [3, HDR + 1, f.b.len() - 1] {
            let mut b = f.b.clone();
            b.truncate(cut);
            assert!(decode_all(b, 4096).is_err(), "cut at {cut}");
        }
    }

    /// splitmix64: the property tests' rows.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n.max(1)
        }
    }

    /// A head word as a run row's come: zero, a small count, a word with zero high bytes, a full image.
    fn word(r: &mut Rng) -> u64 {
        match r.below(5) {
            0 | 1 => 0,
            2 => r.below(300),
            3 => r.next() >> (8 * r.below(8)),
            _ => r.next(),
        }
    }

    /// A random shape: a head of 1 - 40 words (7 - 46 for a var run), a var run's record word and 0 - 4 slots on distinct
    /// words.
    fn shape(r: &mut Rng, var: bool) -> RowShape {
        let words = 1 + r.below(40) as u32 + if var { 6 } else { 0 };
        if !var {
            return RowShape::new(8 * words, None).unwrap();
        }
        let mut idx: Vec<u32> = (0..words).collect(); // alloc: Cold
        for i in (1..idx.len()).rev() {
            idx.swap(i, r.below(i as u64 + 1) as usize);
        }
        let slots = idx[1..1 + r.below(5) as usize].iter().map(|&w| 8 * w).collect(); // alloc: Cold
        RowShape::new(8 * words, Some(VarRows { rec_off: 8 * idx[0], slots })).unwrap()
    }

    /// `n` records of `s` as `RunShape::compact` lays them; `odd`: about a third of them off the layout (a byte of a slot's
    /// offset or of the record's last pad flipped), which the codec keeps as they lie.
    fn records(r: &mut Rng, s: &RowShape, n: usize, odd: bool) -> Vec<u8> {
        let h = s.head as usize;
        let mut out = Vec::new(); // alloc: Cold
        // each slot's copy before (about a third of the copies repeat it: a sorted run's equal key values)
        let mut last: Vec<Vec<u8>> = vec![Vec::new(); s.var.as_ref().map_or(0, |v| v.slots.len())]; // alloc: Cold
        for _ in 0..n {
            let at0 = out.len();
            for _ in 0..h / 8 {
                out.extend_from_slice(&word(r).to_le_bytes());
            }
            let Some(v) = &s.var else { continue };
            let mut at = h;
            for (c, &so) in v.slots.iter().enumerate() {
                let so = so as usize;
                let again = !last[c].is_empty() && r.below(3) == 0;
                let size = match r.below(20) {
                    _ if again => last[c].len() as u32,
                    0..=4 => 0,
                    5 => 4 + r.below(20_000) as u32,
                    _ => 4 + r.below(60) as u32,
                };
                out[at0 + so..at0 + so + 4].copy_from_slice(&size.to_le_bytes());
                if size == 0 {
                    out[at0 + so + 4..at0 + so + 8].fill(0); // a NULL's slot is zero (the lowering's store)
                    continue;
                }
                let len = var_alloc(size) as usize;
                out[at0 + so + 4..at0 + so + 8].copy_from_slice(&((at - so) as u32).to_le_bytes());
                if !again {
                    last[c] = (0..size).map(|_| r.next() as u8).collect(); // alloc: Cold
                    // most copies are plain varlenas: their header their size
                    if r.below(4) != 0 {
                        last[c][..4].copy_from_slice(&(size << 2).to_le_bytes());
                    }
                }
                out.extend_from_slice(&last[c]);
                out.resize(out.len() + len - size as usize, 0); // alloc: Cold
                at += len;
            }
            let ro = at0 + v.rec_off as usize;
            out[ro..ro + 4].copy_from_slice(&(at as u32).to_le_bytes());
            if odd && r.below(3) == 0 {
                match (r.below(2), v.slots.first()) {
                    (0, Some(&so)) => out[at0 + so as usize + 4] ^= 0x08,
                    _ => *out.last_mut().unwrap() ^= 0x5a,
                }
            }
        }
        out
    }

    /// `raw` compacted (pushed in random pieces), written as a run, read back through its row reader in random reads.
    fn round_trip(r: &mut Rng, s: &RowShape, raw: &[u8], compress: bool) -> (Vec<u8>, usize) {
        let mut enc = RowEncoder::new(s.clone());
        let (mut rows, mut rest) = (Vec::new(), raw); // alloc: Cold
        while !rest.is_empty() {
            let k = (1 + r.below(3000) as usize).min(rest.len());
            push_into(&mut enc, &rest[..k], &mut rows).unwrap();
            rest = &rest[k..];
        }
        enc.finish().unwrap();
        let mut f = File { raw: !compress, ..File::default() };
        write_run(&mut f, 0, &rows, 0).unwrap();
        let mut rd = RowReader::new(Box::new(Decoder::new(Box::new(Bytes { b: f.b.clone(), pos: 0 }))), s.clone()); // alloc: Cold
        let (mut out, mut buf) = (Vec::new(), vec![0u8; 1 + r.below(20_000) as usize]); // alloc: Cold
        loop {
            let n = rd.read(&mut buf).unwrap();
            if n == 0 {
                return (out, rows.len());
            }
            out.extend_from_slice(&buf[..n]);
        }
    }

    #[test]
    fn compact_rows_read_back_byte_exact_for_any_shape() {
        let mut r = Rng(2572);
        for i in 0..600 {
            let s = shape(&mut r, i % 3 != 0);
            let n = match r.below(10) {
                0 => 0,
                1 => 1,
                _ => r.below(400) as usize,
            };
            let raw = records(&mut r, &s, n, i % 4 == 0);
            let (back, enc) = round_trip(&mut r, &s, &raw, i % 2 == 0);
            assert!(back == raw, "shape {s:?}, {n} records, {} raw bytes: read back {} bytes", raw.len(), back.len());
            // a compacted record costs at most its tags and a var record's flags byte (a kept one: its length) more
            let worst = raw.len() + n * ((s.head as usize / 8).div_ceil(2) + 5);
            assert!(enc <= worst, "shape {s:?}: {enc} encoded bytes for {} raw", raw.len());
        }
    }

    #[test]
    fn a_text_and_int_row_compacts_to_about_half() {
        // (tx text, id int4) ordered by (tx, id), as lowerv4 face/runs.rs Shape lays it: the key pairs (32 B), the
        // datum words (16), the null bytes (8), the record word at 56, the var slot at 64, the md5 copy var_alloc'd
        let s = RowShape::new(72, Some(VarRows { rec_off: 56, slots: vec![64] })).unwrap(); // alloc: Cold
        let mut raw = Vec::new(); // alloc: Cold
        for id in 0..10_000u64 {
            let md5: Vec<u8> = (0..32).map(|k| b"0123456789abcdef"[((id * 2_654_435_761 >> (k % 28)) & 15) as usize]).collect(); // alloc: Cold
            let image = u64::from_be_bytes(md5[..8].try_into().unwrap());
            for w in [1, image, 1, id ^ 1 << 63, 0, id, 0, 72 + 40, 36 | 8 << 32] {
                raw.extend_from_slice(&u64::to_le_bytes(w));
            }
            raw.extend_from_slice(&(36u32 << 2).to_le_bytes());
            raw.extend_from_slice(&md5);
            raw.extend_from_slice(&[0; 4]);
        }
        let mut rows = Vec::new(); // alloc: Cold
        let mut enc = RowEncoder::new(s.clone());
        push_into(&mut enc, &raw, &mut rows).unwrap();
        enc.finish().unwrap();
        assert!(rows.len() * 100 <= raw.len() * 60, "{} B compacted of {} B (112 B a row)", rows.len(), raw.len());
        let (back, _) = round_trip(&mut Rng(7), &s, &raw, false);
        assert!(back == raw);
    }

    #[test]
    fn a_run_cut_inside_a_record_is_an_error() {
        let mut r = Rng(9);
        let s = shape(&mut r, true);
        let raw = records(&mut r, &s, 50, false);
        let mut enc = RowEncoder::new(s.clone());
        let mut rows = Vec::new(); // alloc: Cold
        push_into(&mut enc, &raw[..raw.len() - 3], &mut rows).unwrap();
        assert!(enc.finish().is_err(), "a record held at the run's end");
        let mut f = File { raw: true, ..File::default() };
        write_run(&mut f, 0, &rows[..rows.len() - 1], 0).unwrap();
        let mut rd = RowReader::new(Box::new(Decoder::new(Box::new(Bytes { b: f.b, pos: 0 }))), s); // alloc: Cold
        let mut buf = vec![0u8; 1 << 20]; // alloc: Cold
        let mut res = Ok(1);
        while matches!(res, Ok(n) if n > 0) {
            res = rd.read(&mut buf);
        }
        assert!(res.is_err(), "the last record cut short");
    }

    #[test]
    fn a_shape_the_codec_cannot_read_is_refused() {
        assert!(RowShape::new(12, None).is_none(), "a head not in words");
        assert!(RowShape::new(0, None).is_none());
        assert!(RowShape::new(16, Some(VarRows { rec_off: 16, slots: vec![] })).is_none(), "a record word past the head"); // alloc: Cold
        assert!(RowShape::new(32, Some(VarRows { rec_off: 0, slots: vec![8, 12] })).is_none(), "two slots over one another"); // alloc: Cold
        assert!(RowShape::new(32, Some(VarRows { rec_off: 8, slots: vec![8] })).is_none(), "a slot over the record word"); // alloc: Cold
        assert!(RowShape::new(32, Some(VarRows { rec_off: 0, slots: vec![8, 16, 24] })).is_some()); // alloc: Cold
    }
    fn read_all(mut rd: Box<dyn RunReader>, chunk: usize) -> Vec<u8> {
        let (mut out, mut buf) = (Vec::new(), vec![0u8; chunk]); // alloc: Cold
        loop {
            let n = rd.read(&mut buf).unwrap();
            if n == 0 {
                return out;
            }
            out.extend_from_slice(&buf[..n]);
        }
    }

    /// `raw` as a merge writes it (random pieces through `Staged`), the run's bytes.
    fn staged(r: &mut Rng, raw: &[u8], compress: bool, shape: Option<&RowShape>) -> Vec<u8> {
        let mut st = Staged::default();
        st.begin(7, compress, shape, None);
        let (mut out, mut rest) = (Vec::new(), raw); // alloc: Cold
        while !rest.is_empty() {
            let k = (1 + r.below(5000) as usize).min(rest.len());
            st.push(&rest[..k], &mut |b| {
                out.extend_from_slice(b);
                Ok(())
            })
            .unwrap();
            rest = &rest[k..];
        }
        st.finish(&mut |b| {
            out.extend_from_slice(b);
            Ok(())
        })
        .unwrap();
        out
    }

    #[test]
    fn put_and_merged_runs_read_back_through_their_header() {
        let mut r = Rng(2574);
        for i in 0..240 {
            let s = shape(&mut r, i % 3 != 0);
            let n = r.below(300) as usize;
            // a third off the shape's layout; every eighth noise (rows the shape cannot cut: kept as they lie)
            let raw = if i % 8 == 7 { rows(n * s.head as usize + 5, true) } else { records(&mut r, &s, n, i % 4 == 0) };
            let compress = i % 5 != 0;
            let declared = (i % 7 != 0).then_some(&s);
            let mut f = File { raw: !compress, ..File::default() };
            write_run_rows(&mut f, 0, &raw, declared, 0, None).unwrap();
            let back = read_all(reader(f.open_part(0).unwrap().unwrap(), None).unwrap(), 1 + r.below(9000) as usize);
            assert!(back == raw, "put run {i}: shape {s:?} declared {}, compress {compress}: {} of {} B", declared.is_some(), back.len(), raw.len());
            if !compress {
                let mut plain = File { raw: true, ..File::default() };
                write_run(&mut plain, 0, &raw, 0).unwrap();
                assert!(plain.b == f.b, "a file without a limit keeps write_run's bytes");
            }
            if i % 8 == 7 {
                continue; // a merge writes the records its runs hold: never noise
            }
            let enc = staged(&mut r, &raw, compress, declared);
            let back = read_all(reader(Box::new(Bytes { b: enc, pos: 0 }), None).unwrap(), 1 + r.below(9000) as usize); // alloc: Cold
            assert!(back == raw, "merged run {i}: shape {s:?} declared {}, compress {compress}", declared.is_some());
        }
    }

    #[test]
    fn a_header_that_names_a_bad_shape_is_an_error() {
        let open = |b: &[u8], headed: bool| {
            let mut f = File::default();
            write_blocks(&mut f, 0, b, false, headed).unwrap();
            reader(f.open_part(0).unwrap().unwrap(), None).map(|r| read_all(r, 64))
        };
        let mut b = vec![MODE_ROWS]; // alloc: Cold
        b.extend_from_slice(&12u32.to_le_bytes());
        b.push(0);
        assert!(open(&b, true).is_err(), "a head not in words");
        assert!(open(&[9], true).is_err(), "an unknown mode");
        assert!(open(&[MODE_ROWS, 64, 0], true).is_err(), "a header cut short");
        assert_eq!(open(&b, false).unwrap(), b, "a run not headed reads as written");
        assert!(read_all(reader(Box::new(Bytes { b: Vec::new(), pos: 0 }), None).unwrap(), 64).is_empty(), "an empty run"); // alloc: Cold
        // a block past the first marked headed
        let mut f = File::default();
        write_blocks(&mut f, 0, &rows(BLOCK, false), false, false).unwrap();
        write_blocks(&mut f, 0, &rows(BLOCK, false), false, true).unwrap();
        let mut d = Decoder::new(f.open_part(0).unwrap().unwrap());
        let mut buf = vec![0u8; 4 * BLOCK]; // alloc: Cold
        assert!(d.read(&mut buf).is_err());
    }

    /// D3964: a run's encode polls its statement's cancel per piece: canceled, a put stops with the cancel (nothing written,
    /// no fall back to the raw write); outside a canceled kernel call it writes and reads back as before.
    #[test]
    fn d3964_a_canceled_run_put_stops_at_its_next_piece() {
        let mut r = Rng(3964);
        let s = shape(&mut r, true);
        let raw = records(&mut r, &s, 200, false);
        let byte = core::sync::atomic::AtomicU8::new(cancelpoll::ABORT_BIT);
        let mut f = File::default();
        let got = cancelpoll::scoped(&byte, || write_run_rows(&mut f, 0, &raw, Some(&s), 0, None));
        assert!(matches!(got, Err(Error::Cancelled)), "{got:?}");
        let got = cancelpoll::scoped(&byte, || put_run(&mut f, 0, &raw, Some(&s), 0, None).map(|_| ()));
        assert!(matches!(got, Err(Error::Cancelled)), "{got:?}");
        assert_eq!(f.epochs, 0, "a canceled put writes nothing");
        write_run_rows(&mut f, 0, &raw, Some(&s), 0, None).unwrap();
        assert!(read_all(reader(f.open_part(0).unwrap().unwrap(), None).unwrap(), 777) == raw);
    }

    /// A run compacted under a limit reads back the same in a session without one (the reader asks the blocks, not the
    /// session's `temp_file_limit`), and a run written without one in a session with one.
    #[test]
    fn a_run_reads_back_whatever_the_readers_limit() {
        let mut r = Rng(2575);
        let s = shape(&mut r, true);
        let raw = records(&mut r, &s, 200, false);
        for compress in [false, true] {
            let mut f = File { raw: !compress, ..File::default() };
            write_run_rows(&mut f, 0, &raw, Some(&s), 0, None).unwrap();
            f.raw = compress; // the session's limit set or reset after the write
            assert!(read_all(reader(f.open_part(0).unwrap().unwrap(), None).unwrap(), 777) == raw, "written compress {compress}");
        }
    }

    /// D2572-d: under a limit a run is written raw while the statement's spilled bytes with it stay within 1/32 of the
    /// limit, and compacted past it (a run of a statement already past it too); each reads back the same.
    #[test]
    fn d2572d_a_run_far_under_the_limit_is_written_raw() {
        struct Limited(File, u64);
        impl SpillFile for Limited {
            fn write_epoch(&mut self, parts: &[(u32, &[u8])]) -> Result<()> {
                self.0.write_epoch(parts)
            }
            fn read_part(&self, part: u32, sink: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
                self.0.read_part(part, sink)
            }
            fn open_part(&self, part: u32) -> Result<Option<Box<dyn RunReader>>> {
                self.0.open_part(part)
            }
            fn spilled_bytes(&self) -> u64 {
                self.0.spilled_bytes()
            }
            fn compress_runs(&self) -> bool {
                true
            }
            fn compress_run(&self, spilled: u64, bytes: u64) -> bool {
                !raw_fits(spilled, bytes, self.1)
            }
        }
        let mut r = Rng(2610);
        let s = shape(&mut r, true);
        let raw = records(&mut r, &s, 300, false);
        let n = raw.len() as u64;
        // (limit, statement's spilled bytes before the run, compacted?)
        for (limit, spilled, compact) in [(32 * n, 0, false), (32 * n, 1, true), (32 * n + 32 * 7, 7, false), (1 << 30, 0, false), (n, 0, true), (0, 0, true), (u64::MAX, u64::MAX, true)] {
            let mut f = Limited(File::default(), limit);
            write_run_rows(&mut f, 0, &raw, Some(&s), spilled, None).unwrap();
            let mut d = Decoder::new(f.open_part(0).unwrap().unwrap());
            assert_eq!(d.headed().unwrap(), compact, "limit {limit} spilled {spilled}: compacted rows");
            assert!(read_all(reader(f.open_part(0).unwrap().unwrap(), None).unwrap(), 333) == raw, "limit {limit} spilled {spilled}: reads back");
            if !compact {
                assert!(f.0.b.len() as u64 > n, "a raw run keeps every byte behind its block headers");
            }
        }
        assert!(raw_fits(0, 1 << 20, 32 << 20) && !raw_fits(1, 1 << 20, 32 << 20) && !raw_fits(u64::MAX, 1, u64::MAX));
    }

    /// The temp bytes of 20,000 (md5 text, int4) rows keyed (tx, id) under a limit: the compact rows' run (its blocks
    /// stored raw) against LZ4 alone (the run without a shape).
    #[test]
    fn compact_rows_take_fewer_bytes_than_lz4() {
        let s = RowShape::new(72, Some(VarRows { rec_off: 56, slots: vec![64] })).unwrap(); // alloc: Cold
        let mut r = Rng(56);
        let mut md5s: Vec<Vec<u8>> = (0..20_000).map(|_| (0..32).map(|_| b"0123456789abcdef"[r.below(16) as usize]).collect()).collect(); // alloc: Cold
        md5s.sort();
        let mut raw = Vec::new(); // alloc: Cold
        for (id, md5) in md5s.iter().enumerate() {
            let image = u64::from_be_bytes(md5[..8].try_into().unwrap());
            for w in [1, image, 1, id as u64 ^ 1 << 63, 0, id as u64, 0, 72 + 40, 36 | 8 << 32] {
                raw.extend_from_slice(&u64::to_le_bytes(w));
            }
            raw.extend_from_slice(&(36u32 << 2).to_le_bytes());
            raw.extend_from_slice(md5);
            raw.extend_from_slice(&[0; 4]);
        }
        let (mut lz4, mut rows) = (File::default(), File::default());
        write_run(&mut lz4, 0, &raw, 0).unwrap();
        write_run_rows(&mut rows, 0, &raw, Some(&s), 0, None).unwrap();
        let per = |b: usize| b as f64 / md5s.len() as f64;
        println!("D2572-c bytes/row: raw {:.1}, lz4 {:.1}, compact {:.1}", per(raw.len()), per(lz4.b.len()), per(rows.b.len()));
        assert!(rows.b.len() < lz4.b.len(), "compact {} B vs LZ4 {} B", rows.b.len(), lz4.b.len());
        assert!(read_all(reader(rows.open_part(0).unwrap().unwrap(), None).unwrap(), 4096) == raw);
    }
    /// The codec's time per row (encode, decode) on three run shapes, against LZ4 alone: `cargo test --release -- --ignored
    /// codec_time --nocapture` (a measure, not a check).
    #[test]
    #[ignore]
    fn codec_time() {
        let mut r = Rng(3);
        // (f int, t int, label text 'edge N') by f; (md5 text, id) by (tx, id); (k int, id int) fixed
        let gk = RowShape::new(64, Some(VarRows { rec_off: 48, slots: vec![56] })).unwrap(); // alloc: Cold
        let m5 = RowShape::new(72, Some(VarRows { rec_off: 56, slots: vec![64] })).unwrap(); // alloc: Cold
        let kk = RowShape::new(48, None).unwrap();
        let mut sets: Vec<(&str, RowShape, Vec<u8>)> = Vec::new(); // alloc: Cold
        let mut b = Vec::new(); // alloc: Cold
        for i in 0..200_000u64 {
            let f = i / 4;
            let label = format!("edge {}", r.below(200_000)); // alloc: Cold
            let size = 4 + label.len() as u32;
            for w in [1, f ^ 1 << 63, f, r.below(50_000), 0, 0, 64 + var_alloc(size) as u64, size as u64 | 8 << 32] {
                b.extend_from_slice(&w.to_le_bytes());
            }
            b.extend_from_slice(&(size << 2).to_le_bytes());
            b.extend_from_slice(label.as_bytes());
            b.resize(b.len() + (var_alloc(size) - size) as usize, 0); // alloc: Cold
        }
        sets.push(("gk9 label", gk, core::mem::take(&mut b)));
        let mut md5s: Vec<Vec<u8>> = (0..100_000).map(|_| (0..32).map(|_| b"0123456789abcdef"[r.below(16) as usize]).collect()).collect(); // alloc: Cold
        md5s.sort();
        for (id, md5) in md5s.iter().enumerate() {
            let image = u64::from_be_bytes(md5[..8].try_into().unwrap());
            for w in [1, image, 1, id as u64 ^ 1 << 63, id as u64, 0, 0, 72 + 40, 36 | 8 << 32] {
                b.extend_from_slice(&w.to_le_bytes());
            }
            b.extend_from_slice(&(36u32 << 2).to_le_bytes());
            b.extend_from_slice(md5);
            b.extend_from_slice(&[0; 4]);
        }
        sets.push(("m5 md5", m5, core::mem::take(&mut b)));
        for i in 0..100_000u64 {
            let k = i * 7;
            for w in [1, k ^ 1 << 63, 1, r.below(100_000) ^ 1 << 63, k, r.below(100_000)] {
                b.extend_from_slice(&w.to_le_bytes());
            }
        }
        sets.push(("kk ints", kk, b));
        for (name, s, raw) in &sets {
            let rows = match &s.var { None => raw.len() / s.head as usize, Some(_) => if name.starts_with("gk") { 200_000 } else { 100_000 } };
            let t = std::time::Instant::now();
            let mut enc = Vec::with_capacity(raw.len()); // alloc: Cold
            for _ in 0..5 {
                enc.clear();
                let mut e = RowEncoder::new(s.clone());
                for piece in raw.chunks(64 << 10) {
                    push_into(&mut e, piece, &mut enc).unwrap();
                }
                e.finish().unwrap();
            }
            let te = t.elapsed().as_nanos() as f64 / 5.0 / rows as f64;
            let t = std::time::Instant::now();
            let mut back = 0;
            for _ in 0..5 {
                let mut f = File { raw: true, ..File::default() };
                write_blocks(&mut f, 0, &enc, false, false).unwrap();
                let mut rd = RowReader::new(Box::new(Decoder::new(Box::new(Bytes { b: f.b, pos: 0 }))), s.clone()); // alloc: Cold
                let mut buf = vec![0u8; 16 << 10]; // alloc: Cold
                back = 0;
                loop {
                    let n = rd.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    back += n;
                }
            }
            let td = t.elapsed().as_nanos() as f64 / 5.0 / rows as f64;
            assert_eq!(back, raw.len());
            let t = std::time::Instant::now();
            let mut lz = File::default();
            for _ in 0..5 {
                lz = File::default();
                write_run(&mut lz, 0, raw, 0).unwrap();
            }
            let tl = t.elapsed().as_nanos() as f64 / 5.0 / rows as f64;
            let t = std::time::Instant::now();
            for _ in 0..5 {
                let mut d = Decoder::new(Box::new(Bytes { b: lz.b.clone(), pos: 0 })); // alloc: Cold
                let mut buf = vec![0u8; 16 << 10]; // alloc: Cold
                while d.read(&mut buf).unwrap() > 0 {}
            }
            let tld = t.elapsed().as_nanos() as f64 / 5.0 / rows as f64;
            println!("D2572-c time {name}: compact {:.1} B/row encode {te:.1} ns decode {td:.1} ns | lz4 {:.1} B/row encode {tl:.1} ns decode {tld:.1} ns (raw {:.1} B/row)", enc.len() as f64 / rows as f64, lz.b.len() as f64 / rows as f64, raw.len() as f64 / rows as f64);
        }
    }
}
