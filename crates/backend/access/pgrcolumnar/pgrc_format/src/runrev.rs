//! SORTRUNS-E0 (pgrc-plans/SORTRUNS-E0-STEP1.md D2): a sorted run's reverse, `runrev-<run_id>.pgrcv`: from a row ID
//! the run homes ([`crate::sortrun`]) to the member and the frame of the member's map that hold it.
//!
//! The file is read a 4 KiB page at a time and never whole: every page is a [`PAYLOAD`]-byte payload and a `u32`
//! crc32c of the payload followed by the page number (`u64`), so a page read at the wrong place fails like a torn one.
//! A page whose crc or structure fails is `CrcMismatch` / `Corrupt` (the reader maps it to `ERRCODE_DATA_CORRUPTED`),
//! never an answer. The file is immutable for the run's life: a member rewritten in place (its DV folded) keeps its
//! map's virtual frames (`crate::rowmap::RevForm::Run`), so every (slot, frame) address here stays valid.
//!
//! Page 0: a [`SidecarFileHeader`] (kind [`SidecarKind::RunReverse`], `key_fingerprint` = the run ID, `payload_len`
//! = the file's length), the 56-byte header, then the section directory (per section `u32 first_page, u32 pages,
//! u64 count`), zeros to the crc. The manifest's `EXT_RUNS` record holds the file's length and page 0's crc.
//!
//! Sections, in file order (each a whole number of pages, contiguous from page 1; a page holds whole entries only):
//! - `S_PAGES` (rank SET): the run's ID set at build as runs of consecutive IDs; per page `u32 n, u8 wlo, u8 wr,
//!   u16 0`, then `n` offsets of each run's first ID from the page's first (`wlo` bits each), then `n` offsets of each
//!   run's first rank from the page's (`wr` bits each); a run's length is the next run's rank less its own.
//! - `LABELS`: per rank, `label_bits` bits. LABELS form: the rank's group; FRAMEIX form: its run-global frame. The
//!   label `nsym` marks an ID found in `EXCEPTIONS` (or absent: a DENSE rank no row took).
//! - `GROUPS` (LABELS form): per group `u16 slot, u16 0, u32 first frame, u32 end frame` of the member's map.
//! - `EXCEPTIONS`: sorted `u64 id, u16 slot, u16 0, u32 frame`: the IDs whose group's fast path misses them.
//! - `S_TOP` (rank SET): per `S_PAGES` page, `u64 first ID, u64 first rank`. Read at open.
//! - `EXC_TOP`: per `EXCEPTIONS` page, its first ID. Read at open.
//! - `FRAME_PREFIX` (FRAMEIX form): per slot, the slot's first run-global frame, and the total last. Read at open.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use crate::rowid::ROWID_LIMIT;
use crate::sidecar::{SidecarFileHeader, SidecarKind, SIDECAR_FILE_HEADER_LEN, SIDECAR_MAGIC};
use crate::sortrun::{RankForm, RunRevForm, SortRun};
use crate::wire::{put_u32, put_u64, put_u8, Cur};
use crate::{FormatError, FormatResult, FORMAT_VERSION};

/// A page of the file.
pub const PAGE: usize = 4096;
/// A page's payload (the rest is its crc).
pub const PAYLOAD: usize = PAGE - 4;
/// The header's body version.
pub const BODY_VERSION: u8 = 1;
const HEADER_LEN: usize = 56;
const NSECT: usize = 7;
const DIR_LEN: usize = NSECT * 16;

const S_PAGES: usize = 0;
const LABELS: usize = 1;
const GROUPS: usize = 2;
const EXCEPTIONS: usize = 3;
const S_TOP: usize = 4;
const EXC_TOP: usize = 5;
const PREFIX: usize = 6;

const S_HEAD: usize = 8;
const S_TOP_PER: usize = PAYLOAD / 16;
const GROUP_LEN: usize = 12;
const GROUPS_PER: usize = PAYLOAD / GROUP_LEN;
const EXC_LEN: usize = 16;
const EXC_PER: usize = PAYLOAD / EXC_LEN;
const EXC_TOP_PER: usize = PAYLOAD / 8;
const PREFIX_PER: usize = PAYLOAD / 4;

const AT: &str = "run reverse";

fn corrupt() -> FormatError {
    FormatError::Corrupt { at: AT }
}

fn check(ok: bool) -> FormatResult<()> {
    if ok {
        Ok(())
    } else {
        Err(corrupt())
    }
}

fn contract(detail: &'static str) -> FormatError {
    FormatError::EncodeContract { detail }
}

#[inline]
fn bits_for(x: u64) -> u32 {
    64 - x.leading_zeros()
}

/// Labels per `LABELS` page at `lw` bits each.
#[inline]
fn labels_per(lw: u32) -> u64 {
    (PAYLOAD as u64 * 8) / u64::from(lw)
}

/// `w` (at most 57) bits at bit `pos` of `b`, least significant first; bytes past the end read as zeros.
#[inline]
fn bits_at(b: &[u8], pos: u64, w: u32) -> u64 {
    if w == 0 {
        return 0;
    }
    let i = (pos / 8) as usize;
    let mut x = [0u8; 8];
    let n = b.len().saturating_sub(i).min(8);
    x[..n].copy_from_slice(&b[i..i + n]);
    (u64::from_le_bytes(x) >> (pos % 8)) & ((1u64 << w) - 1)
}

/// Every bit of `b` from bit `from` on is zero.
fn zeros_from(b: &[u8], from: u64) -> bool {
    let whole = from.div_ceil(8) as usize;
    (from % 8 == 0 || bits_at(b, from, 8 - (from % 8) as u32) == 0) && b.get(whole..).is_none_or(|t| t.iter().all(|&x| x == 0))
}

fn put_bits(b: &mut [u8], pos: u64, w: u32, v: u64) {
    let (mut v, mut pos, mut left) = (v, pos, w);
    while left > 0 {
        let (i, sh) = ((pos / 8) as usize, (pos % 8) as u32);
        let take = (8 - sh).min(left);
        b[i] |= ((v & ((1u64 << take) - 1)) << sh) as u8;
        v >>= take;
        pos += u64::from(take);
        left -= take;
    }
}

/// A page's crc: crc32c of its payload, then its number.
fn page_crc(payload: &[u8], no: u64) -> u32 {
    let s = ::crc32c::pg_comp_crc32c(::crc32c::CRC32C_INIT, payload);
    ::crc32c::fin_crc32c(::crc32c::pg_comp_crc32c(s, &no.to_le_bytes()))
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().expect("2 bytes"))
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}

// ---------------------------------------------------------------------------------------------------------------- pages

/// A page whose crc held ([`Page::check`]); its section's structure is checked once, on first use.
#[derive(Debug)]
pub struct Page {
    no: u64,
    bytes: Box<[u8]>,
    /// 0 unchecked, 1 checked, 2 refused.
    shape: AtomicU8,
}

impl Page {
    /// Page `no` of a file: `bytes` must be [`PAGE`] long and its crc hold.
    pub fn check(no: u64, bytes: Box<[u8]>) -> FormatResult<Page> {
        if bytes.len() != PAGE {
            return Err(FormatError::Truncated { at: AT });
        }
        if u32_at(&bytes, PAYLOAD) != page_crc(&bytes[..PAYLOAD], no) {
            return Err(FormatError::CrcMismatch { at: AT });
        }
        Ok(Page { no, bytes, shape: AtomicU8::new(0) })
    }

    pub fn no(&self) -> u64 {
        self.no
    }

    fn payload(&self) -> &[u8] {
        &self.bytes[..PAYLOAD]
    }

    /// The page's bytes held in memory.
    pub fn mem_bytes(&self) -> usize {
        PAGE + std::mem::size_of::<Page>()
    }

    /// Run `f` (the section's structure check) once per page; a refusal sticks.
    fn shaped(&self, f: impl FnOnce(&[u8]) -> FormatResult<()>) -> FormatResult<()> {
        match self.shape.load(Ordering::Acquire) {
            1 => Ok(()),
            2 => Err(corrupt()),
            _ => {
                let r = f(self.payload());
                self.shape.store(if r.is_ok() { 1 } else { 2 }, Ordering::Release);
                r
            }
        }
    }
}

/// Where a reader's pages come from (pgrc_am: the map cache's cells over the file).
pub trait Pages {
    /// Page `no`, its crc checked ([`Page::check`]).
    fn page(&self, no: u64) -> FormatResult<Arc<Page>>;
}

/// A whole file in memory as [`Pages`] (tests, verify).
pub struct BytePages<'a>(pub &'a [u8]);

impl Pages for BytePages<'_> {
    fn page(&self, no: u64) -> FormatResult<Arc<Page>> {
        let at = usize::try_from(no).ok().and_then(|n| n.checked_mul(PAGE)).ok_or_else(corrupt)?;
        let b = self.0.get(at..at + PAGE).ok_or(FormatError::Truncated { at: AT })?;
        Ok(Arc::new(Page::check(no, b.into())?))
    }
}

// ---------------------------------------------------------------------------------------------------------------- writer

/// What a reverse is built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spec {
    pub run_id: u64,
    /// LABELS or FRAMEIX.
    pub form: RunRevForm,
    pub rank: RankForm,
    /// The labels: DENSE the ranges' span, SET the ID set's size.
    pub ranks: u64,
    /// The run's rows at build.
    pub rows: u64,
    /// LABELS: the groups; FRAMEIX: the run's frames (the members' virtual frames, slot by slot). The label `nsym`
    /// marks an exception.
    pub nsym: u32,
    pub members: u32,
}

/// A finished reverse: page 0 (written last, at offset 0) and the file's length and page-0 crc for `EXT_RUNS`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    pub page0: Vec<u8>,
    pub rev_len: u64,
    pub rev_crc: u32,
}

/// Writes a reverse page by page, in file order: the ID set's runs (SET), the labels by rank, the groups (LABELS),
/// the exceptions by ID, then [`Writer::finish`] (the read-at-open sections and page 0). Completed pages from page 1
/// on accumulate until [`Writer::take`] (the builder's reverse steps flush them to the run's temp file in order).
#[derive(Debug)]
pub struct Writer {
    spec: Spec,
    lw: u32,
    out: Vec<u8>,
    next_page: u64,
    dir: [(u32, u32, u64); NSECT],
    phase: usize,
    page: Vec<u8>,
    // S
    s_page: Vec<(u64, u64)>,
    s_top: Vec<(u64, u64)>,
    s_end: u64,
    s_rank: u64,
    // labels
    in_page: u64,
    labels: u64,
    // groups, exceptions
    groups: u64,
    exc: u64,
    exc_top: Vec<u64>,
    last_exc: Option<u64>,
}

impl Writer {
    pub fn new(spec: Spec) -> FormatResult<Writer> {
        let ok = matches!(spec.form, RunRevForm::Labels | RunRevForm::FrameIx)
            && spec.nsym >= 1
            && (1..=crate::sortrun::MAX_MEMBERS as u32).contains(&spec.members)
            && spec.ranks >= 1
            && spec.rows >= 1
            && spec.rows <= spec.ranks
            && (spec.rank == RankForm::Dense || spec.rows == spec.ranks)
            && spec.run_id != 0;
        if !ok {
            return Err(contract("run reverse spec"));
        }
        let lw = bits_for(u64::from(spec.nsym));
        Ok(Writer {
            spec,
            lw,
            out: Vec::new(),
            next_page: 1,
            dir: [(1, 0, 0); NSECT],
            phase: S_PAGES,
            page: vec![0u8; PAYLOAD],
            s_page: Vec::new(),
            s_top: Vec::new(),
            s_end: 0,
            s_rank: 0,
            in_page: 0,
            labels: 0,
            groups: 0,
            exc: 0,
            exc_top: Vec::new(),
            last_exc: None,
        })
    }

    /// The label width.
    pub fn label_bits(&self) -> u32 {
        self.lw
    }

    fn emit(&mut self) {
        let crc = page_crc(&self.page, self.next_page);
        self.out.extend_from_slice(&self.page);
        self.out.extend_from_slice(&crc.to_le_bytes());
        self.page.iter_mut().for_each(|b| *b = 0);
        self.next_page += 1;
    }

    /// Close the current section and the ones up to `sec`.
    fn advance(&mut self, sec: usize) -> FormatResult<()> {
        if sec < self.phase {
            return Err(contract("run reverse sections out of order"));
        }
        while self.phase < sec {
            match self.phase {
                S_PAGES => self.flush_s(),
                LABELS if self.in_page > 0 => {
                    self.emit();
                    self.in_page = 0;
                }
                GROUPS if self.groups as usize % GROUPS_PER != 0 => self.emit(),
                EXCEPTIONS if self.exc as usize % EXC_PER != 0 => self.emit(),
                _ => {}
            }
            let first = self.dir[self.phase].0;
            self.dir[self.phase].1 = (self.next_page - u64::from(first)) as u32;
            self.phase += 1;
            self.dir[self.phase].0 = self.next_page as u32;
        }
        Ok(())
    }

    fn flush_s(&mut self) {
        let Some(&(lo0, r0)) = self.s_page.first() else { return };
        let &(lo1, r1) = self.s_page.last().expect("a run");
        let (wlo, wr) = (bits_for(lo1 - lo0), bits_for(r1 - r0));
        let n = self.s_page.len();
        self.page[0..4].copy_from_slice(&(n as u32).to_le_bytes());
        self.page[4] = wlo as u8;
        self.page[5] = wr as u8;
        let base = S_HEAD as u64 * 8;
        for (k, &(lo, r)) in self.s_page.iter().enumerate() {
            put_bits(&mut self.page, base + k as u64 * u64::from(wlo), wlo, lo - lo0);
            put_bits(&mut self.page, base + n as u64 * u64::from(wlo) + k as u64 * u64::from(wr), wr, r - r0);
        }
        self.s_top.push((lo0, r0));
        self.dir[S_PAGES].2 += n as u64;
        self.s_page.clear();
        self.emit();
    }

    /// The next run of the ID set (SET): `len` IDs from `lo`, above every run before (one adjacent to the last
    /// extends it).
    pub fn push_s_run(&mut self, lo: u64, len: u64) -> FormatResult<()> {
        if self.spec.rank != RankForm::Set || self.phase != S_PAGES || len == 0 || lo.checked_add(len).is_none_or(|e| e > ROWID_LIMIT) {
            return Err(contract("run reverse ID set"));
        }
        if self.s_rank > 0 && lo < self.s_end {
            return Err(contract("run reverse ID set not ascending"));
        }
        if self.s_rank > 0 && lo == self.s_end {
            self.s_end += len;
            self.s_rank += len;
            return Ok(());
        }
        if let Some(&(lo0, r0)) = self.s_page.first() {
            let bits = S_HEAD as u64 * 8 + (self.s_page.len() as u64 + 1) * u64::from(bits_for(lo - lo0) + bits_for(self.s_rank - r0));
            if bits > PAYLOAD as u64 * 8 {
                self.flush_s();
            }
        }
        self.s_page.push((lo, self.s_rank));
        self.s_end = lo + len;
        self.s_rank += len;
        Ok(())
    }

    /// The next rank's label (at most `nsym`).
    pub fn push_label(&mut self, label: u32) -> FormatResult<()> {
        self.advance(LABELS)?;
        if label > self.spec.nsym || self.labels >= self.spec.ranks {
            return Err(contract("run reverse label"));
        }
        put_bits(&mut self.page, self.in_page * u64::from(self.lw), self.lw, u64::from(label));
        self.in_page += 1;
        self.labels += 1;
        if self.in_page == labels_per(self.lw) {
            self.emit();
            self.in_page = 0;
        }
        Ok(())
    }

    /// The next group (LABELS form): frames `va..vb` of slot `slot`'s map.
    pub fn push_group(&mut self, slot: u16, va: u32, vb: u32) -> FormatResult<()> {
        self.advance(GROUPS)?;
        if self.spec.form != RunRevForm::Labels || u32::from(slot) >= self.spec.members || va >= vb || self.groups >= u64::from(self.spec.nsym) {
            return Err(contract("run reverse group"));
        }
        let at = (self.groups as usize % GROUPS_PER) * GROUP_LEN;
        self.page[at..at + 2].copy_from_slice(&slot.to_le_bytes());
        self.page[at + 4..at + 8].copy_from_slice(&va.to_le_bytes());
        self.page[at + 8..at + 12].copy_from_slice(&vb.to_le_bytes());
        self.groups += 1;
        if self.groups as usize % GROUPS_PER == 0 {
            self.emit();
        }
        Ok(())
    }

    /// The next exception (IDs ascending): `id` is in frame `vframe` of slot `slot`'s map.
    pub fn push_exception(&mut self, id: u64, slot: u16, vframe: u32) -> FormatResult<()> {
        self.advance(EXCEPTIONS)?;
        if self.spec.form != RunRevForm::Labels || u32::from(slot) >= self.spec.members || id >= ROWID_LIMIT || self.last_exc.is_some_and(|l| l >= id) {
            return Err(contract("run reverse exception"));
        }
        let k = self.exc as usize % EXC_PER;
        if k == 0 {
            self.exc_top.push(id);
        }
        let at = k * EXC_LEN;
        self.page[at..at + 8].copy_from_slice(&id.to_le_bytes());
        self.page[at + 8..at + 10].copy_from_slice(&slot.to_le_bytes());
        self.page[at + 12..at + 16].copy_from_slice(&vframe.to_le_bytes());
        self.exc += 1;
        self.last_exc = Some(id);
        if self.exc as usize % EXC_PER == 0 {
            self.emit();
        }
        Ok(())
    }

    /// The completed pages since the last call (page 1 on, in order).
    pub fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }

    /// The page number the next byte [`Writer::take`] returns goes to (its offset is this times [`PAGE`]).
    pub fn next_out_page(&self) -> u64 {
        self.next_page - (self.out.len() / PAGE) as u64
    }

    /// Write the read-at-open sections (with `prefix`, FRAMEIX: the slots' first frames and the total), then page 0.
    /// The pages [`Writer::take`] still returns, then `page0` at offset 0, are the whole file.
    pub fn finish(mut self, prefix: &[u32]) -> FormatResult<(Vec<u8>, Finished)> {
        self.advance(S_TOP)?;
        let sp = self.spec;
        let frameix = sp.form == RunRevForm::FrameIx;
        let groups = if frameix { 0 } else { u64::from(sp.nsym) };
        let prefix_ok = if frameix {
            prefix.len() == sp.members as usize + 1 && prefix[0] == 0 && prefix.windows(2).all(|w| w[0] < w[1]) && prefix[sp.members as usize] == sp.nsym
        } else {
            prefix.is_empty()
        };
        let ok = self.labels == sp.ranks && (sp.rank == RankForm::Dense || self.s_rank == sp.ranks) && self.groups == groups && (!frameix || self.exc == 0) && prefix_ok;
        if !ok {
            return Err(contract("run reverse counts"));
        }
        self.dir[S_TOP].2 = self.s_top.len() as u64;
        for (i, &(id, r)) in self.s_top.clone().iter().enumerate() {
            let at = (i % S_TOP_PER) * 16;
            self.page[at..at + 8].copy_from_slice(&id.to_le_bytes());
            self.page[at + 8..at + 16].copy_from_slice(&r.to_le_bytes());
            if (i + 1) % S_TOP_PER == 0 || i + 1 == self.s_top.len() {
                self.emit();
            }
        }
        self.advance(EXC_TOP)?;
        self.dir[EXC_TOP].2 = self.exc_top.len() as u64;
        for (i, &id) in self.exc_top.clone().iter().enumerate() {
            let at = (i % EXC_TOP_PER) * 8;
            self.page[at..at + 8].copy_from_slice(&id.to_le_bytes());
            if (i + 1) % EXC_TOP_PER == 0 || i + 1 == self.exc_top.len() {
                self.emit();
            }
        }
        self.advance(PREFIX)?;
        self.dir[PREFIX].2 = prefix.len() as u64;
        for (i, &f) in prefix.iter().enumerate() {
            let at = (i % PREFIX_PER) * 4;
            self.page[at..at + 4].copy_from_slice(&f.to_le_bytes());
            if (i + 1) % PREFIX_PER == 0 || i + 1 == prefix.len() {
                self.emit();
            }
        }
        self.dir[PREFIX].1 = (self.next_page - u64::from(self.dir[PREFIX].0)) as u32;
        self.dir[LABELS].2 = self.labels;
        self.dir[GROUPS].2 = self.groups;
        self.dir[EXCEPTIONS].2 = self.exc;
        let pages = self.next_page;
        let rev_len = pages * PAGE as u64;
        let mut p0 = Vec::with_capacity(PAGE);
        SidecarFileHeader {
            part_uuid: [0; 16],
            key_fingerprint: sp.run_id,
            payload_len: rev_len,
            magic: SIDECAR_MAGIC,
            kind: SidecarKind::RunReverse.as_u16(),
            flags: 0,
            version: FORMAT_VERSION,
            pad: 0,
        }
        .encode_into(&mut p0);
        put_u8(&mut p0, BODY_VERSION);
        put_u8(&mut p0, sp.form.code());
        put_u8(&mut p0, sp.rank.code());
        put_u8(&mut p0, self.lw as u8);
        put_u32(&mut p0, PAGE as u32);
        put_u64(&mut p0, sp.run_id);
        put_u64(&mut p0, sp.ranks);
        put_u64(&mut p0, sp.rows);
        put_u32(&mut p0, sp.nsym);
        put_u32(&mut p0, sp.members);
        put_u64(&mut p0, self.exc);
        put_u64(&mut p0, pages);
        for &(first, n, count) in &self.dir {
            put_u32(&mut p0, first);
            put_u32(&mut p0, n);
            put_u64(&mut p0, count);
        }
        debug_assert_eq!(p0.len(), SIDECAR_FILE_HEADER_LEN + HEADER_LEN + DIR_LEN);
        p0.resize(PAYLOAD, 0);
        let crc = page_crc(&p0, 0);
        put_u32(&mut p0, crc);
        let rev_crc = crate::wire::crc32c(&p0);
        Ok((std::mem::take(&mut self.out), Finished { page0: p0, rev_len, rev_crc }))
    }
}

/// The pages a reverse of `form` spends on what its form decides (SORTRUNS-E0 L2, the builder's election by exact
/// size): its labels (`ranks` of them at `nsym`'s width) and its groups (LABELS: `nsym` groups) or its frame prefix
/// (FRAMEIX: `members + 1` entries), with the page of each read-at-open section. Page 0, the ID set (SET) and the
/// exceptions are the same for either form, so they are left out.
pub fn form_pages(form: RunRevForm, ranks: u64, nsym: u32, members: u32) -> u64 {
    let labels = ranks.div_ceil(labels_per(bits_for(u64::from(nsym))));
    match form {
        RunRevForm::Labels => labels + u64::from(nsym).div_ceil(GROUPS_PER as u64),
        RunRevForm::FrameIx => labels + (u64::from(members) + 1).div_ceil(PREFIX_PER as u64),
        RunRevForm::None => 0,
    }
}

/// A whole reverse in memory (tests, small runs): `s_runs` the ID set's runs (SET), `labels` by rank, `groups`
/// (LABELS), `exceptions` by ID, `prefix` (FRAMEIX). The file, its length and its page-0 crc.
pub fn encode(spec: Spec, s_runs: &[(u64, u64)], labels: &[u32], groups: &[(u16, u32, u32)], exceptions: &[(u64, u16, u32)], prefix: &[u32]) -> FormatResult<(Vec<u8>, Finished)> {
    let mut w = Writer::new(spec)?;
    for &(lo, len) in s_runs {
        w.push_s_run(lo, len)?;
    }
    for &l in labels {
        w.push_label(l)?;
    }
    for &(s, a, b) in groups {
        w.push_group(s, a, b)?;
    }
    for &(id, s, f) in exceptions {
        w.push_exception(id, s, f)?;
    }
    let (tail, fin) = w.finish(prefix)?;
    let mut file = fin.page0.clone();
    file.extend_from_slice(&tail);
    debug_assert_eq!(file.len() as u64, fin.rev_len);
    Ok((file, fin))
}

// ---------------------------------------------------------------------------------------------------------------- reader

/// Where a run's reverse sends a row ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    /// The run holds no such ID (outside its ID set, a DENSE rank no row took, or a label naming no exception).
    Absent,
    /// LABELS: the ID is in one of frames `va..vb` of slot `slot`'s map (the member map's group search).
    Group { slot: u16, va: u32, vb: u32 },
    /// FRAMEIX or an exception: the ID is in frame `vframe` of slot `slot`'s map.
    Frame { slot: u16, vframe: u32 },
}

/// An open reverse: page 0 and the read-at-open sections, checked against the run's `EXT_RUNS` entry.
#[derive(Debug)]
pub struct RunRev {
    run_id: u64,
    form: RunRevForm,
    rank: RankForm,
    lw: u32,
    ranks: u64,
    nsym: u32,
    members: u32,
    exceptions: u64,
    dir: [(u32, u32, u64); NSECT],
    s_top: Box<[(u64, u64)]>,
    exc_top: Box<[u64]>,
    prefix: Box<[u32]>,
    /// DENSE: the run's ranges with each one's first rank.
    dense: Box<[(u64, u64, u64)]>,
}

impl RunRev {
    /// Open the reverse of `run` (a HOMES run of `EXT_RUNS`): page 0 must match the entry (form, rank, ID, length,
    /// crc, members, rank count) and the directory be the canonical one for its counts; the read-at-open sections
    /// are loaded and checked.
    pub fn open(run: &SortRun, src: &dyn Pages) -> FormatResult<RunRev> {
        check(run.homes())?;
        let p0 = src.page(0)?;
        check(crate::wire::crc32c(&p0.bytes) == run.rev_crc)?;
        let b = p0.payload();
        let h = SidecarFileHeader::decode(b)?;
        check(h.kind == SidecarKind::RunReverse.as_u16() && h.part_uuid == [0; 16] && h.key_fingerprint == run.run_id && h.payload_len == run.rev_len && h.flags == 0 && h.pad == 0)?;
        let mut c = Cur::new(&b[SIDECAR_FILE_HEADER_LEN..]);
        let (bv, form, rank, lw) = (c.u8(AT)?, c.u8(AT)?, c.u8(AT)?, u32::from(c.u8(AT)?));
        if bv != BODY_VERSION {
            return Err(FormatError::BadVersion { at: AT, got: u32::from(bv) });
        }
        let (page_size, run_id, ranks, rows) = (c.u32(AT)?, c.u64(AT)?, c.u64(AT)?, c.u64(AT)?);
        let (nsym, members, exceptions, pages) = (c.u32(AT)?, c.u32(AT)?, c.u64(AT)?, c.u64(AT)?);
        let mut dir = [(0u32, 0u32, 0u64); NSECT];
        for d in dir.iter_mut() {
            *d = (c.u32(AT)?, c.u32(AT)?, c.u64(AT)?);
        }
        let used = SIDECAR_FILE_HEADER_LEN + HEADER_LEN + DIR_LEN;
        check(b[used..].iter().all(|&x| x == 0))?;
        let form = RunRevForm::of(form).ok_or_else(corrupt)?;
        let rank = RankForm::of(rank).ok_or_else(corrupt)?;
        let frameix = form == RunRevForm::FrameIx;
        check(form == run.rev && rank == run.rank && page_size == PAGE as u32 && run_id == run.run_id && rows == run.rows && members as usize == run.members.len())?;
        check(pages.checked_mul(PAGE as u64) == Some(run.rev_len) && nsym >= 1 && lw == bits_for(u64::from(nsym)) && ranks >= rows)?;
        check(match rank {
            RankForm::Dense => ranks == run.span(),
            RankForm::Set => ranks == rows,
        })?;
        check(!frameix || exceptions == 0)?;
        // the canonical directory: contiguous from page 1, each section the pages its count needs
        let s_pages = dir[S_PAGES].1 as u64;
        let need = |sec: usize| -> Option<u64> {
            Some(match sec {
                S_PAGES => s_pages,
                LABELS => ranks.div_ceil(labels_per(lw)),
                GROUPS => u64::from(if frameix { 0 } else { nsym }).div_ceil(GROUPS_PER as u64),
                EXCEPTIONS => exceptions.div_ceil(EXC_PER as u64),
                S_TOP => s_pages.div_ceil(S_TOP_PER as u64),
                EXC_TOP => exceptions.div_ceil(EXC_PER as u64).div_ceil(EXC_TOP_PER as u64),
                _ => if frameix { (u64::from(members) + 1).div_ceil(PREFIX_PER as u64) } else { 0 },
            })
        };
        let count = |sec: usize| -> u64 {
            match sec {
                LABELS => ranks,
                GROUPS => if frameix { 0 } else { u64::from(nsym) },
                EXCEPTIONS => exceptions,
                S_TOP => s_pages,
                EXC_TOP => exceptions.div_ceil(EXC_PER as u64),
                PREFIX => if frameix { u64::from(members) + 1 } else { 0 },
                _ => dir[S_PAGES].2,
            }
        };
        let mut at = 1u64;
        for (sec, &(first, n, cnt)) in dir.iter().enumerate() {
            check(u64::from(first) == at && Some(u64::from(n)) == need(sec) && cnt == count(sec))?;
            at += u64::from(n);
        }
        check(at == pages)?;
        check((rank == RankForm::Set) == (s_pages > 0) && dir[S_PAGES].2 >= s_pages)?;
        let mut rr = RunRev {
            run_id,
            form,
            rank,
            lw,
            ranks,
            nsym,
            members,
            exceptions,
            dir,
            s_top: Box::new([]),
            exc_top: Box::new([]),
            prefix: Box::new([]),
            dense: Box::new([]),
        };
        // the read-at-open sections
        let mut s_top = crate::try_vec(s_pages as usize, "runrev s_top")?;
        rr.read_top(src, S_TOP, 16, &mut |e| s_top.push((u64_at(e, 0), u64_at(e, 8))))?;
        check(s_top.first().is_none_or(|t| t.1 == 0) && s_top.windows(2).all(|w| w[0].0 < w[1].0 && w[0].1 < w[1].1) && s_top.iter().all(|t| t.0 < ROWID_LIMIT && t.1 < ranks))?;
        let mut exc_top = Vec::new();
        rr.read_top(src, EXC_TOP, 8, &mut |e| exc_top.push(u64_at(e, 0)))?;
        check(exc_top.windows(2).all(|w| w[0] < w[1]) && exc_top.iter().all(|&x| x < ROWID_LIMIT))?;
        let mut prefix = Vec::new();
        rr.read_top(src, PREFIX, 4, &mut |e| prefix.push(u32_at(e, 0)))?;
        check(!frameix || (prefix[0] == 0 && prefix.windows(2).all(|w| w[0] < w[1]) && prefix[members as usize] == nsym))?;
        rr.s_top = s_top.into();
        rr.exc_top = exc_top.into();
        rr.prefix = prefix.into();
        if rank == RankForm::Dense {
            let mut r0 = 0u64;
            rr.dense = run
                .ranges
                .iter()
                .map(|&(lo, hi)| {
                    let e = (lo, hi, r0);
                    r0 += hi - lo;
                    e
                })
                .collect();
        }
        Ok(rr)
    }

    /// Section `sec`'s entries of `len` bytes, page by page (whole entries per page, zeros after the last).
    fn read_top(&self, src: &dyn Pages, sec: usize, len: usize, f: &mut dyn FnMut(&[u8])) -> FormatResult<()> {
        let (first, n, count) = self.dir[sec];
        let per = (PAYLOAD / len) as u64;
        for p in 0..u64::from(n) {
            let page = src.page(u64::from(first) + p)?;
            let k = (count - p * per).min(per) as usize;
            let b = page.payload();
            check(b[k * len..].iter().all(|&x| x == 0))?;
            for i in 0..k {
                f(&b[i * len..(i + 1) * len]);
            }
        }
        Ok(())
    }

    pub fn run_id(&self) -> u64 {
        self.run_id
    }

    pub fn form(&self) -> RunRevForm {
        self.form
    }

    pub fn rank_form(&self) -> RankForm {
        self.rank
    }

    /// The labels (ranks) the file holds.
    pub fn ranks(&self) -> u64 {
        self.ranks
    }

    pub fn exceptions(&self) -> u64 {
        self.exceptions
    }

    /// The file's pages.
    pub fn pages(&self) -> u64 {
        self.dir[PREFIX].0 as u64 + u64::from(self.dir[PREFIX].1)
    }

    /// The open state's bytes in memory (the pages a lookup reads are the cache's).
    pub fn mem_bytes(&self) -> usize {
        std::mem::size_of::<RunRev>() + self.s_top.len() * 16 + self.exc_top.len() * 8 + self.prefix.len() * 4 + self.dense.len() * 24
    }

    /// The ID's rank, `None` outside the run's ID set (DENSE: its ranges).
    pub fn rank_of(&self, id: u64, src: &dyn Pages) -> FormatResult<Option<u64>> {
        match self.rank {
            RankForm::Dense => {
                let Some(i) = self.dense.partition_point(|e| e.0 <= id).checked_sub(1) else { return Ok(None) };
                let (lo, hi, r0) = self.dense[i];
                Ok((id < hi).then(|| r0 + id - lo))
            }
            RankForm::Set => {
                let Some(p) = self.s_top.partition_point(|e| e.0 <= id).checked_sub(1) else { return Ok(None) };
                let page = src.page(u64::from(self.dir[S_PAGES].0) + p as u64)?;
                let (lo0, r0) = self.s_top[p];
                let ranks_here = self.s_top.get(p + 1).map_or(self.ranks, |e| e.1) - r0;
                let next_id = self.s_top.get(p + 1).map_or(ROWID_LIMIT + 1, |e| e.0);
                page.shaped(|b| check_s_page(b, lo0, ranks_here, next_id))?;
                let b = page.payload();
                let n = u32_at(b, 0) as u64;
                let (wlo, wr) = (u32::from(b[4]), u32::from(b[5]));
                let base = S_HEAD as u64 * 8;
                let off = id - lo0;
                // the last run starting at or below `id`
                let (mut lo, mut hi) = (0u64, n);
                while hi - lo > 1 {
                    let mid = lo + (hi - lo) / 2;
                    if bits_at(b, base + mid * u64::from(wlo), wlo) <= off {
                        lo = mid;
                    } else {
                        hi = mid;
                    }
                }
                let k = lo;
                let start = bits_at(b, base + k * u64::from(wlo), wlo);
                let rk = bits_at(b, base + n * u64::from(wlo) + k * u64::from(wr), wr);
                let rn = if k + 1 < n { bits_at(b, base + n * u64::from(wlo) + (k + 1) * u64::from(wr), wr) } else { ranks_here };
                Ok((off - start < rn - rk).then(|| r0 + rk + off - start))
            }
        }
    }

    /// The label of `rank` (below [`RunRev::ranks`]).
    pub fn label_of(&self, rank: u64, src: &dyn Pages) -> FormatResult<u32> {
        check(rank < self.ranks)?;
        let per = labels_per(self.lw);
        let (p, k) = (rank / per, rank % per);
        let page = src.page(u64::from(self.dir[LABELS].0) + p)?;
        let here = (self.ranks - p * per).min(per);
        let (lw, nsym) = (self.lw, self.nsym);
        page.shaped(|b| {
            check((0..here).all(|i| bits_at(b, i * u64::from(lw), lw) <= u64::from(nsym)))?;
            check(zeros_from(b, here * u64::from(lw)))
        })?;
        Ok(bits_at(page.payload(), k * u64::from(lw), lw) as u32)
    }

    /// Where `id` is: [`Hit::Absent`] when the run holds no such ID.
    pub fn lookup(&self, id: u64, src: &dyn Pages) -> FormatResult<Hit> {
        let Some(rank) = self.rank_of(id, src)? else { return Ok(Hit::Absent) };
        let label = self.label_of(rank, src)?;
        if label == self.nsym {
            return self.exception(id, src);
        }
        match self.form {
            RunRevForm::Labels => self.group(label, src),
            _ => {
                let slot = self.prefix.partition_point(|&f| f <= label) - 1;
                Ok(Hit::Frame { slot: slot as u16, vframe: label - self.prefix[slot] })
            }
        }
    }

    /// Group `g`'s frames.
    pub fn group(&self, g: u32, src: &dyn Pages) -> FormatResult<Hit> {
        check(self.form == RunRevForm::Labels && g < self.nsym)?;
        let (p, k) = (g as usize / GROUPS_PER, g as usize % GROUPS_PER);
        let page = src.page(u64::from(self.dir[GROUPS].0) + p as u64)?;
        let here = (self.nsym as usize - p * GROUPS_PER).min(GROUPS_PER);
        let members = self.members;
        page.shaped(|b| {
            check((0..here).all(|i| {
                let at = i * GROUP_LEN;
                u32::from(u16_at(b, at)) < members && u16_at(b, at + 2) == 0 && u32_at(b, at + 4) < u32_at(b, at + 8)
            }))?;
            check(b[here * GROUP_LEN..].iter().all(|&x| x == 0))
        })?;
        let b = page.payload();
        let at = k * GROUP_LEN;
        Ok(Hit::Group { slot: u16_at(b, at), va: u32_at(b, at + 4), vb: u32_at(b, at + 8) })
    }

    fn exception(&self, id: u64, src: &dyn Pages) -> FormatResult<Hit> {
        let Some(p) = self.exc_top.partition_point(|&x| x <= id).checked_sub(1) else { return Ok(Hit::Absent) };
        let page = src.page(u64::from(self.dir[EXCEPTIONS].0) + p as u64)?;
        let here = (self.exceptions as usize - p * EXC_PER).min(EXC_PER);
        let (first, next, members) = (self.exc_top[p], self.exc_top.get(p + 1).copied().unwrap_or(ROWID_LIMIT), self.members);
        page.shaped(|b| {
            check(u64_at(b, 0) == first)?;
            check((0..here).all(|i| {
                let at = i * EXC_LEN;
                let x = u64_at(b, at);
                x < next && (i == 0 || u64_at(b, at - EXC_LEN) < x) && u32::from(u16_at(b, at + 8)) < members && u16_at(b, at + 10) == 0
            }))?;
            check(b[here * EXC_LEN..].iter().all(|&x| x == 0))
        })?;
        let b = page.payload();
        let (mut lo, mut hi) = (0usize, here);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let x = u64_at(b, mid * EXC_LEN);
            if x == id {
                let at = mid * EXC_LEN;
                return Ok(Hit::Frame { slot: u16_at(b, at + 8), vframe: u32_at(b, at + 12) });
            }
            if x < id {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Ok(Hit::Absent)
    }
}

/// An `S_PAGES` page's structure: at least one run, widths that fit the page, first offsets 0, runs ascending with a
/// gap between them, ranks ascending and inside the page's `ranks`, the last run below `next_id` (the next page's
/// first ID) with a gap, zeros after the bits.
fn check_s_page(b: &[u8], lo0: u64, ranks: u64, next_id: u64) -> FormatResult<()> {
    let n = u64::from(u32_at(b, 0));
    let (wlo, wr) = (u32::from(b[4]), u32::from(b[5]));
    check(n >= 1 && wlo <= 40 && wr <= 40 && u16_at(b, 6) == 0)?;
    let end = S_HEAD as u64 * 8 + n * u64::from(wlo + wr);
    check(end <= PAYLOAD as u64 * 8)?;
    let base = S_HEAD as u64 * 8;
    let lo_at = |k: u64| bits_at(b, base + k * u64::from(wlo), wlo);
    let r_at = |k: u64| bits_at(b, base + n * u64::from(wlo) + k * u64::from(wr), wr);
    check(lo_at(0) == 0 && r_at(0) == 0)?;
    for k in 0..n {
        let r1 = if k + 1 < n { r_at(k + 1) } else { ranks };
        let len = r1.checked_sub(r_at(k)).filter(|&l| l >= 1).ok_or_else(corrupt)?;
        let stop = lo0.checked_add(lo_at(k)).and_then(|x| x.checked_add(len)).ok_or_else(corrupt)?;
        let next = if k + 1 < n { lo0 + lo_at(k + 1) } else { next_id };
        check(stop < next && stop <= ROWID_LIMIT)?;
    }
    check(zeros_from(b, end))
}
