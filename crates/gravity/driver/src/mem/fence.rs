//! D3190 (the co-split merge join's cut over spilled runs): a put sort run's fences. While a run is put
//! (`runcodec::put_run`), one record every `FENCE` raw bytes or more is a fence: its bytes are copied (a cut compares them
//! as kernels' `RowCmp` compares any put record), with its row index and where the run's bytes after it resume (the codec
//! block's part offset and the decoded bytes before them in that block). A reader then opens the run at any fence
//! (`Records::at`), so a cut reads one fence interval per boundary instead of the run, and a slice is read from the fence
//! before its first row. A compacted run codes each record against the record before (`runcodec::RowEncoder`): its reader
//! resumes against the fence's own copy. The run's bytes on disk are the same as without fences; the fences live beside
//! the run in its stream (`spill::Stream`), their copies at most about 1/256 of the run's bytes.
#![allow(unsafe_code)]

use crate::mem::runcodec::{self, RowShape, BLOCK};
use crate::mem::spill::{RunReader, SpillFile};
use crate::{Error, Result};

/// A fence every this many raw bytes of a run's records or more (two codec blocks): a cut reads at most about this much
/// of a run per boundary.
pub const FENCE: usize = 2 * BLOCK;
/// A fenced record's copy is at most 1/`SPREAD` of the raw bytes since the fence before it: a run of long records keeps
/// fewer fences, never a copy of its bytes (a 16 KiB fence spacing holds for rows up to 64 bytes).
const SPREAD: usize = 256;
/// D3974: fence 0's copy is kept when it is at most `FIRST_FREE` bytes or at most 1/`SPREAD` of the run's raw bytes; past
/// both the run keeps no fences (its cut falls back as for a run put without them). A run of a few long records (one
/// 640 KB text row a run at work_mem 64kB) otherwise kept its first record whole beside it for the statement: ~390 KB
/// resident per run, ~39 GB over 100k runs, where the merge reads the runs from their files.
const FIRST_FREE: usize = 1024;

/// Whether a run of `raw` bytes keeps fence 0, a copy of its first record of `first` bytes (`FIRST_FREE`).
fn first_kept(first: usize, raw: usize) -> bool {
    first <= FIRST_FREE || first.saturating_mul(SPREAD) <= raw
}

/// A reader's first buffer, in words.
const BUF_WORDS: usize = BLOCK / 8;

/// One fence of a put run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fence {
    /// The fenced record's row index in the run.
    pub row: u64,
    /// Its copy: the word offset in `Fences::recs`, its bytes.
    word: usize,
    len: usize,
    /// The part offset of the codec block holding the run's first byte after the record (the part's length when none
    /// follows), and the decoded bytes of that block before that byte.
    at: u64,
    skip: u32,
}

/// A put run's fences, fence 0 its first record, in row order.
#[derive(Debug)]
pub struct Fences {
    /// LEDGER2 step 5: the fences' heap bytes (copies and entries) claimed for the run's slot owner (`put_run`'s), released
    /// with them (`Charge::NONE` without an owner). Declared FIRST (D4633): the charge goes before the heap it counts.
    _claim: memledger::Charge,
    recs: Vec<u64>,
    at: Vec<Fence>,
    shape: RowShape,
    compact: bool,
    rows: u64,
}

/// The fences `pick` chose: `(row, raw offset, bytes)` of each fenced record, and the run's rows.
pub(crate) struct Picks {
    pub(crate) at: Vec<(u64, usize, usize)>,
    pub(crate) rows: u64,
}

/// The records of `raw` that are fences under `shape`, record 0 first; `None` when the shape does not frame `raw` (a
/// record shorter than the head or not in words, or one past the bytes), or when fence 0's copy would pass its bound
/// (`first_kept`): the run is then put without fences.
pub(crate) fn pick(shape: &RowShape, raw: &[u8]) -> Option<Picks> {
    let head = shape.head as usize;
    if shape.var.is_none() {
        // fixed records: every `step`-th row
        if raw.len() % head != 0 || !first_kept(head, raw.len()) {
            return None;
        }
        let rows = raw.len() / head;
        let step = FENCE.max(SPREAD * head).div_ceil(head);
        let at = (0..rows).step_by(step).map(|r| (r as u64, r * head, head)).collect(); // alloc: Exec
        return Some(Picks { at, rows: rows as u64 });
    }
    let (mut at, mut off, mut row, mut last) = (Vec::new(), 0usize, 0u64, None::<usize>); // alloc: Exec
    while off < raw.len() {
        let n = runcodec::record_len(shape, &raw[off..])?;
        if n < head || n % 8 != 0 || off + n > raw.len() || (last.is_none() && !first_kept(n, raw.len())) {
            return None;
        }
        if last.is_none_or(|l| off - l >= FENCE.max(SPREAD * n)) {
            at.push((row, off, n)); // alloc: Exec
            last = Some(off);
        }
        (off, row) = (off + n, row + 1);
    }
    Some(Picks { at, rows: row })
}

impl Fences {
    /// The fences of a run whose records are `raw`, chosen by `pick`: `ends[i]` the offset in the run's coded stream
    /// just past fence i's record (its raw end, or a compacted run's coded end), `starts[b]` block b's part offset,
    /// `total` the part's bytes; `owner` the run's slot owner, whose the fences' heap bytes are (`None`: no one's).
    pub(crate) fn new(raw: &[u8], shape: &RowShape, picks: Picks, ends: &[usize], starts: &[u64], total: u64, compact: bool, owner: Option<memledger::Owner>) -> Fences {
        let copied: usize = picks.at.iter().map(|p| p.2 / 8).sum();
        let mut recs = Vec::with_capacity(copied); // alloc: Exec charged owner
        let mut at = Vec::with_capacity(picks.at.len()); // alloc: Exec charged owner
        for (&(row, off, len), &end) in picks.at.iter().zip(ends) {
            let word = recs.len();
            recs.extend(raw[off..off + len].chunks_exact(8).map(|c| u64::from_ne_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]))); // alloc: Exec charged owner
            let b = end / BLOCK;
            at.push(Fence { row, word, len, at: starts.get(b).copied().unwrap_or(total), skip: (end % BLOCK) as u32 }); // alloc: Exec charged owner
        }
        let bytes = recs.capacity() * 8 + at.capacity() * core::mem::size_of::<Fence>();
        let claim = owner.map_or(memledger::Charge::NONE, |o| memledger::Charge::open(o, memledger::Release::Statement, memledger::Kind::Sort, memledger::charge::Door::Heap, bytes as u64));
        Fences { recs, at, shape: shape.clone(), compact, rows: picks.rows, _claim: claim }
    }

    pub fn len(&self) -> usize {
        self.at.len()
    }

    pub fn is_empty(&self) -> bool {
        self.at.is_empty()
    }

    /// The run's rows.
    pub fn rows(&self) -> u64 {
        self.rows
    }

    /// Fence `i`'s row index.
    pub fn row(&self, i: usize) -> u64 {
        self.at[i].row
    }

    /// Fence `i`'s record, 8-aligned.
    pub fn rec(&self, i: usize) -> &[u8] {
        let f = &self.at[i];
        let words = &self.recs[f.word..f.word + f.len / 8];
        // SAFETY: `len / 8` initialized words of `recs`, viewed as their bytes for as long as `self` is borrowed.
        unsafe { core::slice::from_raw_parts(words.as_ptr() as *const u8, f.len) }
    }

    /// The last fence at or before row `row` (fence 0 is row 0).
    pub fn before(&self, row: u64) -> usize {
        self.at.partition_point(|f| f.row <= row).saturating_sub(1)
    }

    /// The bytes the fences hold resident.
    pub fn bytes(&self) -> usize {
        self.recs.len() * 8 + self.at.len() * core::mem::size_of::<Fence>()
    }

    pub fn shape(&self) -> &RowShape {
        &self.shape
    }
}

/// A put run's records in row order, from its first or from a fence: each record's bytes whole and 8-aligned (a sort
/// run's compares read words).
pub struct Records {
    r: Option<Box<dyn RunReader>>,
    shape: RowShape,
    buf: Vec<u64>,
    /// The bytes of `buf` held: the next record at `lo`.
    lo: usize,
    hi: usize,
    row: u64,
}

impl Records {
    /// Part `part` of `file`, a put run of records of `shape`, from its first record. E-kern R: `owner` (the run's stream
    /// owner, the caller's) is its reader's (`runcodec::reader`).
    pub fn first(file: &dyn SpillFile, part: u32, shape: &RowShape, owner: Option<memledger::Owner>) -> Result<Records> {
        let r = file.open_part(part)?.map(|r| runcodec::reader(r, owner)).transpose()?;
        Ok(Records { r, shape: shape.clone(), buf: vec![0; BUF_WORDS], lo: 0, hi: 0, row: 0 }) // alloc: Exec
    }

    /// Part `part` of `file`, the put run fenced by `f`, from fence `i` on: the fence's record (its copy), then the run's
    /// records after it.
    pub fn at(file: &dyn SpillFile, part: u32, f: &Fences, i: usize) -> Result<Records> {
        let x = f.at[i];
        let rec = f.rec(i);
        let r = match file.open_part_at(part, x.at)? {
            None => None,
            Some(inner) => {
                let mut d = runcodec::Decoder::new(inner);
                let mut scratch = [0u8; 512];
                let mut left = x.skip as usize;
                while left > 0 {
                    let n = d.read(&mut scratch[..left.min(512)])?;
                    if n == 0 {
                        return Err(Error::host("gravity: spill: a put run ends before its fence"));
                    }
                    left -= n;
                }
                Some(match f.compact {
                    true => Box::new(runcodec::RowReader::resume(Box::new(d), f.shape.clone(), rec)) as Box<dyn RunReader>, // alloc: Exec
                    false => Box::new(d) as Box<dyn RunReader>, // alloc: Exec
                })
            }
        };
        let mut buf = vec![0u64; BUF_WORDS.max(x.len / 8 * 2)]; // alloc: Exec
        buf[..x.len / 8].copy_from_slice(&f.recs[x.word..x.word + x.len / 8]);
        Ok(Records { r, shape: f.shape.clone(), buf, lo: 0, hi: x.len, row: x.row })
    }

    /// The row index of the next record.
    pub fn row(&self) -> u64 {
        self.row
    }

    /// The next record's bytes (valid until the next call), `None` past the run's last.
    pub fn next(&mut self) -> Result<Option<&[u8]>> {
        let Some(n) = self.fill()? else { return Ok(None) };
        let at = self.lo;
        (self.lo, self.row) = (at + n, self.row + 1);
        Ok(Some(&self.bytes()[at..at + n]))
    }

    /// The next record's bytes, not passed.
    pub fn peek(&mut self) -> Result<Option<&[u8]>> {
        let Some(n) = self.fill()? else { return Ok(None) };
        let at = self.lo;
        Ok(Some(&self.bytes()[at..at + n]))
    }

    /// Past the next `n` records (fewer at the run's end): the records passed.
    pub fn skip(&mut self, n: u64) -> Result<u64> {
        let mut k = 0;
        while k < n && self.next()?.is_some() {
            k += 1;
        }
        Ok(k)
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: the buffer's initialized words viewed as their bytes while `self` is borrowed.
        unsafe { core::slice::from_raw_parts(self.buf.as_ptr() as *const u8, self.buf.len() * 8) }
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as `bytes`, borrowed mutably.
        unsafe { core::slice::from_raw_parts_mut(self.buf.as_mut_ptr() as *mut u8, self.buf.len() * 8) }
    }

    /// A reader over this run's next `rows` records (fewer at its end), their bytes whole and in order (`SliceReader`).
    pub fn slice(self, rows: u64) -> SliceReader {
        SliceReader { recs: self, left: rows, held: Vec::new(), at: 0 } // alloc: Exec
    }

    /// The next record whole at `lo`: its bytes, `None` at the run's end (a record cut short is an error). Every record
    /// is a whole number of words, so `lo` stays 8-aligned.
    fn fill(&mut self) -> Result<Option<usize>> {
        let head = self.shape.head as usize;
        loop {
            let held = self.hi - self.lo;
            let need = match runcodec::record_len(&self.shape, &self.bytes()[self.lo..self.hi]) {
                Some(n) if n < head || n % 8 != 0 => return Err(Error::host(format!("gravity: spill: a put run's record reads {n} bytes"))), // alloc: Cold
                Some(n) if held >= n => return Ok(Some(n)),
                Some(n) => n,
                None => head,
            };
            let Some(mut r) = self.r.take() else {
                return match held {
                    0 => Ok(None),
                    _ => Err(Error::host(format!("gravity: spill: a put run ends inside a record ({held} of {need} bytes)"))), // alloc: Cold
                };
            };
            if self.lo > 0 {
                let (lo, hi) = (self.lo, self.hi);
                self.bytes_mut().copy_within(lo..hi, 0);
                (self.lo, self.hi) = (0, held);
            }
            if self.buf.len() * 8 < need {
                self.buf.resize(need.div_ceil(8).max(2 * self.buf.len()), 0); // alloc: Exec
            }
            // the reader stays taken at its end (`None`: the run read whole)
            let hi = self.hi;
            let k = r.read(&mut self.bytes_mut()[hi..])?;
            if k > 0 {
                self.hi += k;
                self.r = Some(r);
            }
        }
    }
}

/// A slice of a put run as a run reader's bytes (`SPILL_RUN_SLICE`: a co-split claim's merge over each outer run's slice
/// fills from it as from `SpillTable::run_open`'s reader): the records of `Records::slice`, whole and in order. A record
/// longer than the buffer a read asks for goes in pieces (the merge's fill tops its buffer up read after read).
pub struct SliceReader {
    recs: Records,
    /// The records still to hand out.
    left: u64,
    /// A record a read cut: its bytes from `at` on go first.
    held: Vec<u8>,
    at: usize,
}

impl RunReader for SliceReader {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let mut n = 0;
        if self.at < self.held.len() {
            let k = (self.held.len() - self.at).min(buf.len());
            buf[..k].copy_from_slice(&self.held[self.at..self.at + k]);
            self.at += k;
            n = k;
        }
        while n < buf.len() && self.left > 0 {
            let Some(rec) = self.recs.peek()? else {
                self.left = 0;
                break;
            };
            let len = rec.len();
            if len <= buf.len() - n {
                buf[n..n + len].copy_from_slice(rec);
                n += len;
            } else if n == 0 {
                // a record longer than the whole buffer: its first bytes now, the rest held for the reads after
                self.held.clear();
                self.held.extend_from_slice(rec); // alloc: Exec
                buf.copy_from_slice(&self.held[..buf.len()]);
                (self.at, n) = (buf.len(), buf.len());
            } else {
                break;
            }
            self.recs.next()?;
            self.left -= 1;
        }
        Ok(n)
    }
}

/// D3190: rows `[lo, hi)` of `fences`' put run (`open(Some(f))` its records from fence f on) as a run reader: opened at the
/// last fence at or before `lo`, the rows before `lo` passed; `None` for an empty slice (or an empty run).
pub fn slice_of(fences: &Fences, open: &dyn Fn(usize) -> Result<Records>, lo: u64, hi: u64) -> Result<Option<SliceReader>> {
    if lo >= hi || fences.is_empty() || lo >= fences.rows() {
        return Ok(None);
    }
    let f = fences.before(lo);
    let mut r = open(f)?;
    let skip = lo - fences.row(f);
    if r.skip(skip)? != skip {
        return Err(Error::host("gravity: spill: a put run ends before its slice's first row"));
    }
    Ok(Some(r.slice(hi.min(fences.rows()) - lo)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::runcodec::VarRows;

    struct Bytes {
        b: Vec<u8>,
        pos: usize,
    }

    impl RunReader for Bytes {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
            let n = (self.b.len() - self.pos).min(buf.len()).min(3001); // short reads, as a file's
            buf[..n].copy_from_slice(&self.b[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    /// One part, its epochs appended; `raw`: a file that does not ask for compressed runs. Opens at an offset by the
    /// trait's default (`seek` false) or by slicing (`seek` true).
    #[derive(Default)]
    struct File {
        b: Vec<u8>,
        raw: bool,
        seek: bool,
    }

    impl SpillFile for File {
        fn write_epoch(&mut self, parts: &[(u32, &[u8])]) -> Result<()> {
            for (_, b) in parts {
                self.b.extend_from_slice(b);
            }
            Ok(())
        }
        fn read_part(&self, _part: u32, sink: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
            sink(&self.b)
        }
        fn open_part(&self, _part: u32) -> Result<Option<Box<dyn RunReader>>> {
            Ok((!self.b.is_empty()).then(|| Box::new(Bytes { b: self.b.clone(), pos: 0 }) as Box<dyn RunReader>)) // alloc: Cold
        }
        fn open_part_at(&self, part: u32, offset: u64) -> Result<Option<Box<dyn RunReader>>> {
            if !self.seek {
                let Some(mut r) = self.open_part(part)? else { return Ok(None) };
                let mut skip = vec![0u8; offset as usize]; // alloc: Cold
                let mut got = 0;
                while got < skip.len() {
                    match r.read(&mut skip[got..])? {
                        0 => return Ok(None),
                        n => got += n,
                    }
                }
                return Ok((offset < self.b.len() as u64).then_some(r));
            }
            Ok((offset < self.b.len() as u64).then(|| Box::new(Bytes { b: self.b[offset as usize..].to_vec(), pos: 0 }) as Box<dyn RunReader>)) // alloc: Cold
        }
        fn spilled_bytes(&self) -> u64 {
            self.b.len() as u64
        }
        fn compress_runs(&self) -> bool {
            !self.raw
        }
    }

    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n.max(1)
        }
    }

    /// Fixed rows of `head` bytes: word 0 the row's index (the order), the rest repeated noise.
    fn fixed_rows(head: usize, n: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(head * n); // alloc: Cold
        for i in 0..n {
            v.extend_from_slice(&(i as u64).to_ne_bytes());
            v.extend((8..head).map(|j| ((i / 7 + j) % 5) as u8));
        }
        v
    }

    /// Var records under `s` (one slot at 16, the record word at 8) as `RunShape::compact` lays them: word 0 the index,
    /// the slot `[size, offset from the slot to its copy]`, the copy a plain varlena (its header its size), zero padded;
    /// a quarter NULL (slot zero); `long`: every third record's copy past `FENCE`, from record 1 (record 0 stays short, so
    /// the run keeps its fences: D3974 `first_kept`).
    fn var_rows(r: &mut Rng, s: &RowShape, n: usize, long: bool) -> Vec<u8> {
        let mut v = Vec::new(); // alloc: Cold
        for i in 0..n {
            let size = match r.below(4) {
                0 => 0,
                _ if long && i % 3 == 1 => 4 + FENCE + r.below(20000) as usize,
                _ => 4 + r.below(300) as usize,
            };
            push_var(&mut v, s, i, size);
        }
        v
    }

    /// One `var_rows` record of index `i` whose copy is `size` bytes (0: NULL) pushed onto `v`.
    fn push_var(v: &mut Vec<u8>, s: &RowShape, i: usize, size: usize) {
        let head = s.head as usize;
        let copy = if size == 0 { 0 } else { size.next_multiple_of(8).max(16) };
        let at = v.len();
        v.resize(at + head + copy, 0); // alloc: Cold
        v[at..at + 8].copy_from_slice(&(i as u64).to_ne_bytes());
        v[at + 8..at + 12].copy_from_slice(&((head + copy) as u32).to_le_bytes());
        if size > 0 {
            v[at + 16..at + 20].copy_from_slice(&(size as u32).to_le_bytes());
            v[at + 20..at + 24].copy_from_slice(&((head - 16) as u32).to_le_bytes());
            v[at + head..at + head + 4].copy_from_slice(&((size as u32) << 2).to_le_bytes());
            for k in 4..size {
                v[at + head + k] = (i + k) as u8;
            }
        }
    }

    fn var_shape() -> RowShape {
        RowShape::new(32, Some(VarRows { rec_off: 8, slots: vec![16] })).expect("a var shape") // alloc: Cold
    }

    /// Every record read from fence `i` on: the run's records from the fence's row.
    fn all_from(f: &File, fences: &Fences, i: usize) -> Vec<(u64, Vec<u8>)> {
        let mut r = Records::at(f, 0, fences, i).unwrap();
        let mut out = Vec::new(); // alloc: Cold
        loop {
            let row = r.row();
            match r.next().unwrap() {
                Some(b) => {
                    assert_eq!(b.as_ptr() as usize % 8, 0, "a record 8-aligned");
                    out.push((row, b.to_vec())); // alloc: Cold
                }
                None => return out,
            }
        }
    }

    fn records_of(s: &RowShape, raw: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new(); // alloc: Cold
        let mut at = 0;
        while at < raw.len() {
            let n = runcodec::record_len(s, &raw[at..]).unwrap();
            out.push(raw[at..at + n].to_vec()); // alloc: Cold
            at += n;
        }
        out
    }

    /// A run put raw, LZ4 or compacted, fixed or var, reads back from every fence as the run's records from the fence's
    /// row on, whether the file seeks or reads and drops; the run's bytes are `write_run_rows`'s.
    #[test]
    fn a_put_run_reads_back_from_every_fence() {
        let mut r = Rng(0x7a11_c0de);
        for case in 0..36 {
            let var = case % 2 == 1;
            let s = if var { var_shape() } else { RowShape::new(8 * (2 + case as u32 % 7), None).unwrap() };
            let n = [0usize, 1, 2, 77, 900, 4000][case / 6 % 6];
            let raw = if var { var_rows(&mut r, &s, n, case % 4 == 3) } else { fixed_rows(s.head as usize, n * 3) };
            let (compress, seek) = (case % 3, case % 5 != 0);
            let mut f = File { raw: compress == 0, seek, ..File::default() };
            let shape = (compress != 2 || var).then_some(&s);
            let fences = runcodec::put_run(&mut f, 0, &raw, shape, 0, None).unwrap();
            let mut plain = File { raw: compress == 0, ..File::default() };
            runcodec::write_run_rows(&mut plain, 0, &raw, shape, 0, None).unwrap();
            assert!(plain.b == f.b, "case {case}: the bytes on disk are write_run_rows's");
            let want = records_of(&s, &raw);
            let Some(fences) = fences else {
                // D3974: a first record past its bound (`first_kept`) puts the run without fences
                let long = want.first().is_some_and(|r| !first_kept(r.len(), raw.len()));
                assert!(shape.is_none() || long, "case {case}: a run with a shape is fenced");
                continue;
            };
            assert_eq!((fences.rows(), fences.is_empty()), (want.len() as u64, want.is_empty()), "case {case}");
            if !want.is_empty() {
                assert_eq!(fences.row(0), 0, "fence 0 is the first record");
            }
            let copies: usize = (0..fences.len()).map(|i| fences.rec(i).len()).sum();
            let first = if fences.is_empty() { 0 } else { fences.rec(0).len() };
            assert!(copies <= first + raw.len() / SPREAD, "case {case}: copies {copies} of {} B", raw.len());
            for i in 0..fences.len() {
                let row = fences.row(i) as usize;
                assert_eq!(fences.rec(i), want[row].as_slice(), "case {case}: fence {i}'s copy");
                let got = all_from(&f, &fences, i);
                assert_eq!(got.len(), want.len() - row, "case {case}: rows from fence {i}");
                for (k, (gr, gb)) in got.iter().enumerate() {
                    assert!(*gr as usize == row + k && *gb == want[row + k], "case {case}: fence {i}, record {k}");
                }
                assert_eq!(fences.before(fences.row(i)), i);
            }
            let mut from = Records::first(&f, 0, &s, None).unwrap();
            assert_eq!(from.skip(u64::MAX).unwrap(), want.len() as u64, "case {case}: from the first record");
        }
    }

    /// A slice `[lo, hi)` of a put run (fixed or var, raw, LZ4 or compacted, records past a read's buffer) reads back as
    /// those records' bytes in order, whatever the reads' buffer sizes; an empty slice opens no reader.
    #[test]
    fn a_slice_reads_back_its_records_in_any_buffer_size() {
        let mut r = Rng(0x51ce_0001);
        for case in 0..24 {
            let var = case % 2 == 1;
            let s = if var { var_shape() } else { RowShape::new(8 * (2 + case as u32 % 5), None).unwrap() };
            let raw = if var { var_rows(&mut r, &s, 900, case % 4 == 3) } else { fixed_rows(s.head as usize, 3000) };
            let mut f = File { raw: case % 3 == 0, seek: case % 5 != 0, ..File::default() };
            let shape = (case % 3 != 2 || var).then_some(&s);
            let Some(fences) = runcodec::put_run(&mut f, 0, &raw, shape, 0, None).unwrap() else { continue };
            let want = records_of(&s, &raw);
            let n = want.len() as u64;
            let open = |i: usize| Records::at(&f, 0, &fences, i);
            for (lo, hi) in [(0, n), (0, 1), (n / 3, n / 3), (n / 3, n / 2 + 7), (n - 1, n), (n / 2, n + 50), (n, n + 1)] {
                let rd = slice_of(&fences, &open, lo, hi).unwrap();
                let rows = hi.min(n).saturating_sub(lo);
                let Some(mut rd) = rd else {
                    assert_eq!(rows, 0, "case {case}: [{lo}, {hi}) opened no reader");
                    continue;
                };
                let mut got = Vec::new(); // alloc: Cold
                let mut buf = vec![0u8; 70_000]; // alloc: Cold
                loop {
                    let cap = [8usize, 40, 4096, 65_535, 70_000][r.below(5) as usize];
                    match rd.read(&mut buf[..cap]).unwrap() {
                        0 => break,
                        k => got.extend_from_slice(&buf[..k]),
                    }
                }
                let expect: Vec<u8> = want[lo as usize..(lo + rows) as usize].concat();
                assert!(got == expect, "case {case}: slice [{lo}, {hi}) of {n}: {} B read, {} B wanted", got.len(), expect.len());
            }
        }
    }

    /// D3974: a run whose first record passes both `FIRST_FREE` and 1/`SPREAD` of the run's bytes (one 640 KB text row a
    /// run at work_mem 64kB, its whole copy kept beside the run for the statement) is put without fences, its bytes
    /// `write_run_rows`'s and its records read back from its first; a first record within either bound keeps fence 0.
    #[test]
    fn a_run_whose_first_record_is_long_keeps_no_copy_of_it() {
        let s = var_shape();
        let run = |sizes: &[usize]| {
            let mut v = Vec::new(); // alloc: Cold
            sizes.iter().enumerate().for_each(|(i, &n)| push_var(&mut v, &s, i, n));
            v
        };
        let row = 640_000;
        let small = vec![200usize; 4000]; // alloc: Cold
        let long_in_a_long_run: Vec<usize> = [2000].into_iter().chain(core::iter::repeat_n(2000, 300)).collect(); // alloc: Cold
        let small_then_long = [200, row, 300];
        for (case, sizes, fenced) in [
            (0, &[row][..], false),
            (1, &[row, row, row][..], false),
            (2, &[20_000, 30, 20_000][..], false),
            (3, &long_in_a_long_run[..], true),
            (4, &small_then_long[..], true),
            (5, &small[..], true),
            (6, &[0, row][..], true),
        ] {
            let raw = run(sizes);
            for compress in [false, true] {
                let mut f = File { raw: !compress, seek: true, ..File::default() };
                let fences = runcodec::put_run(&mut f, 0, &raw, Some(&s), 0, None).unwrap();
                let mut plain = File { raw: !compress, ..File::default() };
                runcodec::write_run_rows(&mut plain, 0, &raw, Some(&s), 0, None).unwrap();
                assert!(plain.b == f.b, "case {case}: the bytes on disk are write_run_rows's");
                let want = records_of(&s, &raw);
                assert_eq!(fences.is_some(), fenced, "case {case}: fenced");
                if let Some(fences) = &fences {
                    assert!(fences.rec(0).len() <= FIRST_FREE.max(raw.len() / SPREAD), "case {case}: fence 0's copy is bounded");
                    assert_eq!(all_from(&f, fences, 0).len(), want.len(), "case {case}: from fence 0");
                }
                let mut from = Records::first(&f, 0, &s, None).unwrap();
                assert_eq!(from.skip(u64::MAX).unwrap(), want.len() as u64, "case {case}: from the first record");
            }
        }
    }

    /// Rows the shape does not frame are put as before, without fences.
    #[test]
    fn rows_the_shape_does_not_frame_keep_no_fences() {
        let s = var_shape();
        let mut raw = var_rows(&mut Rng(5), &s, 40, false);
        raw[8..12].copy_from_slice(&3u32.to_le_bytes());
        let mut f = File::default();
        assert!(runcodec::put_run(&mut f, 0, &raw, Some(&s), 0, None).unwrap().is_none());
        let fixed = RowShape::new(16, None).unwrap();
        assert!(runcodec::put_run(&mut f, 0, &[0u8; 24], Some(&fixed), 0, None).unwrap().is_none(), "a fixed run not in whole rows");
    }
}
