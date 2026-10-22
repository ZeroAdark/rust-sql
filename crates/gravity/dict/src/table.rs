//! The per-code tables: a header and its arrays in the region, a fault bitmap set at build and
//! consulted only by a gather that selects the code (expr.md §2.2 deferral).
use gravity_abi::site::SPARSE_GATHER_MAX;
use gravity_abi::{word, PtrWord, Word};

use crate::{BuildError, Dict, Fault, Region, View};

pub const FLAG_FAULTS: u8 = 1;

#[repr(C)]
pub struct Header {
    pub elem: u8,
    pub flags: u8,
    pub pad: [u8; 2],
    pub ncodes: u32,
    pub faults: Word<*const u64>,
    pub data: Word<*const u8>,
    pub base: Word<*const u8>,
    pub canon: Word<*const u32>,
    pub hashes: Word<*const u64>,
    pub ndistinct: u32,
    pub nfaults: u32,
}

const _: () = assert!(core::mem::size_of::<Header>() == 56);

/// The header's field offsets for generated code (ir-probe.md §10.1.5 item 1): `elem`, `flags`,
/// `pad[0]` (an index-view lane's length mode), `ncodes`, `faults`, `data`.
pub const HDR_ELEM: i32 = core::mem::offset_of!(Header, elem) as i32;
pub const HDR_FLAGS: i32 = core::mem::offset_of!(Header, flags) as i32;
pub const HDR_MODE: i32 = core::mem::offset_of!(Header, pad) as i32;
pub const HDR_NCODES: i32 = core::mem::offset_of!(Header, ncodes) as i32;
pub const HDR_FAULTS: i32 = core::mem::offset_of!(Header, faults) as i32;
pub const HDR_DATA: i32 = core::mem::offset_of!(Header, data) as i32;
const _: () = assert!(HDR_ELEM == 0 && HDR_FLAGS == 1 && HDR_MODE == 2 && HDR_NCODES == 4 && HDR_FAULTS == 8 && HDR_DATA == 16);

/// A code lane element: `u32` (pgrc code lanes), the datum word, or a narrow lane.
pub trait CodeCell: Copy {
    fn code(self) -> u32;
}

impl CodeCell for u16 {
    #[inline(always)]
    fn code(self) -> u32 {
        self as u32
    }
}

impl CodeCell for u32 {
    #[inline(always)]
    fn code(self) -> u32 {
        self
    }
}

impl CodeCell for u64 {
    #[inline(always)]
    fn code(self) -> u32 {
        self as u32
    }
}

/// The first row of a gather that selected a faulting entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Deferred {
    pub row: u32,
    pub code: u32,
}

/// `ncodes` codes and no entries: the dictionary of a build whose values come by code alone.
pub struct Codes {
    pub ncodes: u32,
    pub null: Option<u32>,
}

impl Dict for Codes {
    fn ncodes(&self) -> u32 {
        self.ncodes
    }

    fn null_code(&self) -> Option<u32> {
        self.null
    }

    fn entries(&self, _f: &mut dyn FnMut(u32, View)) {}
}

pub(crate) struct Build {
    pub hdr: *mut Header,
    faults: *mut u64,
    nfaults: u32,
}

impl Build {
    pub(crate) fn start(dict: &dyn Dict, region: &mut Region, elem: u8) -> Result<Build, BuildError> {
        let ncodes = dict.ncodes();
        if ncodes >= 1 << gravity_abi::status::FAULT_CODE_BITS {
            return Err(BuildError::TooManyCodes);
        }
        let hdr = region.alloc_zeroed(core::mem::size_of::<Header>(), 8)? as *mut Header;
        // SAFETY: a fresh zeroed header inside the region.
        unsafe {
            (*hdr).elem = elem;
            (*hdr).ncodes = ncodes;
            (*hdr).base = word(region.base());
        }
        Ok(Build { hdr, faults: core::ptr::null_mut(), nfaults: 0 })
    }

    pub(crate) fn ncodes(&self) -> u32 {
        // SAFETY: `hdr` is this build's header.
        unsafe { (*self.hdr).ncodes }
    }

    pub(crate) fn fault(&mut self, region: &mut Region, code: u32) -> Result<(), BuildError> {
        if self.faults.is_null() {
            self.faults = region.alloc_zeroed((self.ncodes() as usize).div_ceil(64) * 8, 8)? as *mut u64;
        }
        // SAFETY: `code < ncodes`, the bitmap has `ncodes` bits.
        unsafe { *self.faults.add(code as usize / 64) |= 1 << (code % 64) };
        self.nfaults += 1;
        Ok(())
    }

    pub(crate) fn faults_ptr(&self) -> *const u64 {
        self.faults
    }

    pub(crate) fn finish(self, data: *const u8) -> *const Header {
        // SAFETY: `hdr` is this build's header.
        unsafe {
            (*self.hdr).data = word(data);
            (*self.hdr).faults = word(self.faults);
            (*self.hdr).nfaults = self.nfaults;
            if !self.faults.is_null() {
                (*self.hdr).flags |= FLAG_FAULTS;
            }
        }
        self.hdr
    }
}

/// What stops one entry's evaluation: it raised (a fault bit) or the region is exhausted (the
/// build aborts; the caller grows the region and retries).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Halt {
    Fault(Fault),
    Region(BuildError),
}

impl From<Fault> for Halt {
    fn from(f: Fault) -> Halt {
        Halt::Fault(f)
    }
}

impl From<BuildError> for Halt {
    fn from(e: BuildError) -> Halt {
        Halt::Region(e)
    }
}

/// The build loop shared by every element type: entries in code order, the null code skipped,
/// a fault recorded where `f` raises.
pub(crate) fn sweep(dict: &dyn Dict, region: &mut Region, b: &mut Build, f: &mut dyn FnMut(u32, View, &mut Region) -> Result<(), Halt>) -> Result<(), BuildError> {
    let null = dict.null_code();
    let mut err: Option<BuildError> = None;
    dict.entries(&mut |code, v| {
        if err.is_some() || Some(code) == null {
            return;
        }
        match f(code, v, region) {
            Ok(()) => {}
            Err(Halt::Fault(_)) => err = b.fault(region, code).err(),
            Err(Halt::Region(e)) => err = Some(e),
        }
    });
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

#[inline(always)]
pub(crate) unsafe fn bit(words: *const u64, i: u32) -> bool {
    (*words.add(i as usize / 64) >> (i % 64)) & 1 != 0
}

#[inline(always)]
unsafe fn faulted(faults: *const u64, code: u32) -> bool {
    !faults.is_null() && bit(faults, code)
}

/// The word `w` of a row mask: the caller's mask (or all rows when `first`), the validity word,
/// the tail cut at `rows`.
#[inline(always)]
pub(crate) unsafe fn keep_word(valid: *const u64, rows: usize, first: bool, out: &[u64], w: usize) -> u64 {
    let mut keep = if first { !0u64 } else { out[w] };
    if !valid.is_null() {
        keep &= *valid.add(w);
    }
    let n = rows - w * 64;
    if n < 64 {
        keep &= (1u64 << n) - 1;
    }
    keep
}

/// The first kept row of word `w` whose code faults, as a `Deferred`.
#[inline(always)]
unsafe fn first_fault<C: CodeCell>(faults: *const u64, codes: *const C, w: usize, keep: u64) -> Result<(), Deferred> {
    if faults.is_null() {
        return Ok(());
    }
    let base = w * 64;
    let mut m = keep;
    while m != 0 {
        let i = m.trailing_zeros() as usize;
        let c = (*codes.add(base + i)).code();
        if bit(faults, c) {
            return Err(Deferred { row: (base + i) as u32, code: c });
        }
        m &= m - 1;
    }
    Ok(())
}

/// Independent accumulators of a mask word: the serial `m |= bit << i` chain is the gather's
/// bound (64 dependent ORs per word), split eight ways. A word with `SPARSE_GATHER_MAX` kept rows
/// or fewer (an earlier predicate was selective) gathers those rows only.
const LANES: usize = 8;

/// One bit per code: a predicate over the dictionary column.
#[derive(Clone, Copy, Debug)]
pub struct Bits {
    hdr: *const Header,
    words: *const u64,
    faults: *const u64,
    ncodes: u32,
}

impl Bits {
    pub fn build(dict: &dyn Dict, region: &mut Region, f: &mut dyn FnMut(View) -> Result<bool, Fault>) -> Result<Bits, BuildError> {
        let mut b = Build::start(dict, region, crate::lane::ELEM_BITS)?;
        let words = region.alloc_zeroed((b.ncodes() as usize).div_ceil(64) * 8, 8)? as *mut u64;
        sweep(dict, region, &mut b, &mut |code, v, _| {
            if f(v)? {
                // SAFETY: `code < ncodes`; the words hold `ncodes` bits.
                unsafe { *words.add(code as usize / 64) |= 1 << (code % 64) };
            }
            Ok(())
        })?;
        // SAFETY: the header this build wrote.
        Ok(unsafe { Bits::at(b.finish(words as *const u8)) })
    }

    /// B2-20: the table of a code set, built from its codes (`lookup::code_of`), no entry read: exactly `codes`
    /// (`negated`: every code but those and the dictionary's NULL code). A code past `ncodes` is none of the dictionary's.
    pub fn of_codes(dict: &dyn Dict, codes: &[u32], negated: bool, region: &mut Region) -> Result<Bits, BuildError> {
        let b = Build::start(dict, region, crate::lane::ELEM_BITS)?;
        let n = b.ncodes() as usize;
        let words = region.alloc_zeroed(n.div_ceil(64) * 8, 8)? as *mut u64;
        // SAFETY: every word written is one of the `ncodes`-bit table's; the tail past `ncodes` stays clear.
        unsafe {
            for &c in codes.iter().filter(|&&c| (c as usize) < n) {
                *words.add(c as usize / 64) |= 1 << (c % 64);
            }
            if negated {
                for w in 0..n.div_ceil(64) {
                    let keep = if n - w * 64 < 64 { (1u64 << (n - w * 64)) - 1 } else { !0 };
                    *words.add(w) = !*words.add(w) & keep;
                }
                if let Some(c) = dict.null_code().filter(|&c| (c as usize) < n) {
                    *words.add(c as usize / 64) &= !(1 << (c % 64));
                }
            }
        }
        // SAFETY: the header this build wrote.
        Ok(unsafe { Bits::at(b.finish(words as *const u8)) })
    }

    /// X5: a table that answers no entry: every code's fault bit set (its data clear), so a reader re-evaluates each row
    /// (a code set whose bound value it cannot read: a NULL parameter, PG's row answer).
    pub fn all_faults(dict: &dyn Dict, region: &mut Region) -> Result<Bits, BuildError> {
        let mut b = Build::start(dict, region, crate::lane::ELEM_BITS)?;
        let n = b.ncodes();
        let words = region.alloc_zeroed((n as usize).div_ceil(64) * 8, 8)? as *mut u64;
        for c in 0..n {
            b.fault(region, c)?;
        }
        // SAFETY: the header this build wrote.
        Ok(unsafe { Bits::at(b.finish(words as *const u8)) })
    }

    /// P5-06 / SRC-17: the table of the code run `[lo, hi)` (the entries of one byte interval of a byte-ordered
    /// dictionary, its ends `lookup::lower_bound`'s), no entry read: exactly those codes (`negated`: every other code),
    /// never the dictionary's NULL code. The run is cut at `ncodes`; an empty one (`hi <= lo`) holds no code.
    pub fn of_run(dict: &dyn Dict, lo: u32, hi: u32, negated: bool, region: &mut Region) -> Result<Bits, BuildError> {
        let b = Build::start(dict, region, crate::lane::ELEM_BITS)?;
        let n = b.ncodes() as usize;
        let (lo, hi) = ((lo as usize).min(n), (hi as usize).min(n));
        let words = region.alloc_zeroed(n.div_ceil(64) * 8, 8)? as *mut u64;
        // the bits of codes [a, z) in the word whose first code is `base`
        let span = |a: usize, z: usize, base: usize| if a >= z { 0 } else if z - a == 64 { !0u64 } else { ((1u64 << (z - a)) - 1) << (a - base) };
        // SAFETY: every word written is one of the `ncodes`-bit table's; the bits past `ncodes` stay clear.
        unsafe {
            for w in 0..n.div_ceil(64) {
                let (w0, w1) = (w * 64, (w * 64 + 64).min(n));
                let run = span(lo.max(w0), hi.min(w1), w0);
                *words.add(w) = if negated { span(w0, w1, w0) & !run } else { run };
            }
            if let Some(c) = dict.null_code().filter(|&c| (c as usize) < n) {
                *words.add(c as usize / 64) &= !(1 << (c % 64));
            }
        }
        // SAFETY: the header this build wrote.
        Ok(unsafe { Bits::at(b.finish(words as *const u8)) })
    }

    /// The table of the codes `fill` sets (through the setter it is handed; `negated`: every other code), never the
    /// dictionary's NULL code: a predicate decided by a scan rather than entry by entry (`Matcher::scan_entries`).
    pub fn of_hits(dict: &dyn Dict, negated: bool, region: &mut Region, fill: &mut dyn FnMut(&mut dyn FnMut(u32))) -> Result<Bits, BuildError> {
        let b = Build::start(dict, region, crate::lane::ELEM_BITS)?;
        let n = b.ncodes() as usize;
        let words = region.alloc_zeroed(n.div_ceil(64) * 8, 8)? as *mut u64;
        fill(&mut |c| {
            if (c as usize) < n {
                // SAFETY: `c < ncodes`; the words hold `ncodes` bits.
                unsafe { *words.add(c as usize / 64) |= 1 << (c % 64) };
            }
        });
        // SAFETY: every word written is one of the `ncodes`-bit table's; the bits past `ncodes` stay clear.
        unsafe {
            if negated {
                for w in 0..n.div_ceil(64) {
                    let keep = if n - w * 64 < 64 { (1u64 << (n - w * 64)) - 1 } else { !0 };
                    *words.add(w) = !*words.add(w) & keep;
                }
            }
            if let Some(c) = dict.null_code().filter(|&c| (c as usize) < n) {
                *words.add(c as usize / 64) &= !(1 << (c % 64));
            }
        }
        // SAFETY: the header this build wrote.
        Ok(unsafe { Bits::at(b.finish(words as *const u8)) })
    }

    /// # Safety — `hdr` is a `Bits` table header (`ELEM_BITS`).
    pub unsafe fn at(hdr: *const Header) -> Bits {
        debug_assert_eq!((*hdr).elem, crate::lane::ELEM_BITS);
        Bits { hdr, words: (*hdr).data.ptr() as *const u64, faults: (*hdr).faults.ptr(), ncodes: (*hdr).ncodes }
    }

    pub fn header(&self) -> *const Header {
        self.hdr
    }

    pub fn ncodes(&self) -> u32 {
        self.ncodes
    }

    pub fn has_faults(&self) -> bool {
        !self.faults.is_null()
    }

    /// # Safety — `code < ncodes()`.
    #[inline(always)]
    pub unsafe fn get(&self, code: u32) -> bool {
        bit(self.words, code)
    }

    /// # Safety — `code < ncodes()`.
    #[inline(always)]
    pub unsafe fn faulted(&self, code: u32) -> bool {
        faulted(self.faults, code)
    }

    /// H08 (road code.sparse's table form): the table answers false for `code` for certain, its bit clear and its entry
    /// unfaulted (a faulting entry's rows go on to the program, which raises there). A code past `ncodes` is no answer.
    pub fn rejects(&self, code: u32) -> bool {
        // SAFETY: `code < ncodes` is checked first.
        code < self.ncodes && unsafe { !self.get(code) && !self.faulted(code) }
    }

    /// P5-03: the table answers false for every code for certain (no bit set, no entry faulted).
    pub fn rejects_all(&self) -> bool {
        // SAFETY: the data words hold `ncodes` bits, the tail past `ncodes` clear; a table with a faulted entry has a bitmap.
        self.faults.is_null() && (0..(self.ncodes as usize).div_ceil(64)).all(|w| unsafe { *self.words.add(w) == 0 })
    }

    /// `out` ← (`first` ? all : `out`) ∧ `valid` ∧ table[code], `rows` rows of `codes`; the first
    /// kept row whose entry faulted is returned instead (row order), so an unselected fault is silent.
    /// # Safety — `codes` holds `rows` codes below `ncodes()`, `valid` null or `rows` bits, `out` ≥ `rows` bits.
    #[inline(always)]
    pub unsafe fn gather_mask<C: CodeCell>(&self, codes: *const C, valid: *const u64, rows: usize, first: bool, out: &mut [u64]) -> Result<(), Deferred> {
        for w in 0..rows.div_ceil(64) {
            let keep = keep_word(valid, rows, first, out, w);
            if keep == 0 {
                out[w] = 0;
                continue;
            }
            let base = w * 64;
            if keep.count_ones() <= SPARSE_GATHER_MAX {
                let mut m = keep;
                let mut got = 0u64;
                while m != 0 {
                    let i = m.trailing_zeros();
                    let c = (*codes.add(base + i as usize)).code();
                    got |= (bit(self.words, c) as u64) << i;
                    m &= m - 1;
                }
                out[w] = got;
                first_fault(self.faults, codes, w, keep)?;
                continue;
            }
            let n = (rows - base).min(64);
            let mut acc = [0u64; LANES];
            let mut i = 0;
            while i + LANES <= n {
                for (k, a) in acc.iter_mut().enumerate() {
                    let c = (*codes.add(base + i + k)).code();
                    *a |= (bit(self.words, c) as u64) << (i + k);
                }
                i += LANES;
            }
            while i < n {
                let c = (*codes.add(base + i)).code();
                acc[0] |= (bit(self.words, c) as u64) << i;
                i += 1;
            }
            out[w] = keep & acc.iter().fold(0, |m, a| m | a);
            first_fault(self.faults, codes, w, keep)?;
        }
        Ok(())
    }
}

/// One `i64` per code: a by-value result (`octet_length`, a parsed number, a date).
/// Per-code `i64` values: a built table (one `i64` per code) or a view over the dictionary index
/// (`index_len_header`: one 8-byte read per code at the entry's byte/char fields, the value
/// `((v & m1) >> s1) − ((v & m2) >> s2)` — the byte length, the char field, or their delta — no branch).
#[derive(Clone, Copy, Debug)]
pub struct I64 {
    hdr: *const Header,
    vals: *const u8,
    faults: *const u64,
    ncodes: u32,
    stride: u32,
    m1: u64,
    m2: u64,
    s1: u8,
    s2: u8,
}

impl I64 {
    pub fn build(dict: &dyn Dict, region: &mut Region, f: &mut dyn FnMut(View) -> Result<i64, Fault>) -> Result<I64, BuildError> {
        let mut b = Build::start(dict, region, crate::lane::ELEM_I64)?;
        let vals = region.alloc_zeroed(b.ncodes() as usize * 8, 8)? as *mut i64;
        sweep(dict, region, &mut b, &mut |code, v, _| {
            // SAFETY: `code < ncodes`.
            unsafe { *vals.add(code as usize) = f(v)? };
            Ok(())
        })?;
        // SAFETY: the header this build wrote.
        Ok(unsafe { I64::at(b.finish(vals as *const u8)) })
    }

    /// One value per code from `f(code)` — the entry itself never read (lengths from a dictionary index).
    pub fn build_by_code(codes: &Codes, region: &mut Region, f: &mut dyn FnMut(u32) -> Result<i64, Fault>) -> Result<I64, BuildError> {
        let mut b = Build::start(codes, region, crate::lane::ELEM_I64)?;
        let vals = region.alloc_zeroed(b.ncodes() as usize * 8, 8)? as *mut i64;
        for code in 0..b.ncodes() {
            if Some(code) == codes.null {
                continue;
            }
            match f(code) {
                // SAFETY: `code < ncodes`.
                Ok(v) => unsafe { *vals.add(code as usize) = v },
                Err(_) => b.fault(region, code)?,
            }
        }
        // SAFETY: the header this build wrote.
        Ok(unsafe { I64::at(b.finish(vals as *const u8)) })
    }

    /// # Safety — `hdr` is an `I64` table header.
    pub unsafe fn at(hdr: *const Header) -> I64 {
        let (stride, m1, s1, m2, s2) = match ((*hdr).elem, (*hdr).pad[0]) {
            (crate::lane::ELEM_I64, _) => (8, u64::MAX, 0, 0, 0),
            (_, LEN_MODE_CHARS_ABSOLUTE) => (INDEX_ENTRY_LEN as u32, u64::MAX, 32, 0, 0),
            (_, LEN_MODE_CHARS_DELTA) => (INDEX_ENTRY_LEN as u32, u32::MAX as u64, 0, u64::MAX, 32),
            _ => (INDEX_ENTRY_LEN as u32, u32::MAX as u64, 0, 0, 0),
        };
        I64 { hdr, vals: (*hdr).data.ptr(), faults: (*hdr).faults.ptr(), ncodes: (*hdr).ncodes, stride, m1, m2, s1, s2 }
    }

    pub fn header(&self) -> *const Header {
        self.hdr
    }

    pub fn ncodes(&self) -> u32 {
        self.ncodes
    }

    pub fn has_faults(&self) -> bool {
        !self.faults.is_null()
    }

    /// # Safety — `code < ncodes()`.
    #[inline(always)]
    pub unsafe fn get(&self, code: u32) -> i64 {
        let v = core::ptr::read_unaligned(self.vals.add(code as usize * self.stride as usize) as *const u64);
        ((v & self.m1) >> self.s1) as i64 - ((v & self.m2) >> self.s2) as i64
    }

    /// # Safety — `code < ncodes()`.
    #[inline(always)]
    pub unsafe fn faulted(&self, code: u32) -> bool {
        faulted(self.faults, code)
    }

    /// `out[i]` ← table[codes[i]] for the valid rows (a null row's cell is left as it was: the
    /// result's validity is the lane's); the first valid row of a faulting entry is returned.
    /// # Safety — as `Bits::gather_mask`, `out` holds `rows` cells.
    #[inline(always)]
    pub unsafe fn gather<C: CodeCell>(&self, codes: *const C, valid: *const u64, rows: usize, out: *mut i64) -> Result<(), Deferred> {
        gather_words(self.vals as *const u64, self.faults, codes, valid, rows, out as *mut u64)
    }
}

/// One 64-bit hash per code, of the value `f` returns (the entry itself under the identity):
/// cross-part identity of dictionary values (abi `hash_bytes64`).
#[derive(Clone, Copy, Debug)]
pub struct Hash {
    hdr: *const Header,
    vals: *const u64,
    faults: *const u64,
    ncodes: u32,
}

impl Hash {
    pub fn build(dict: &dyn Dict, region: &mut Region, f: &mut dyn FnMut(View) -> Result<View, Fault>) -> Result<Hash, BuildError> {
        let mut b = Build::start(dict, region, crate::lane::ELEM_HASH)?;
        let vals = region.alloc_zeroed(b.ncodes() as usize * 8, 8)? as *mut u64;
        sweep(dict, region, &mut b, &mut |code, v, _| {
            let d = f(v)?;
            // SAFETY: `code < ncodes`; `d` is the caller's live view.
            unsafe { *vals.add(code as usize) = gravity_abi::hash::hash_bytes64(d.bytes()) };
            Ok(())
        })?;
        // SAFETY: the header this build wrote.
        Ok(unsafe { Hash::at(b.finish(vals as *const u8)) })
    }

    /// # Safety — `hdr` is a `Hash` table header.
    pub unsafe fn at(hdr: *const Header) -> Hash {
        debug_assert_eq!((*hdr).elem, crate::lane::ELEM_HASH);
        Hash { hdr, vals: (*hdr).data.ptr() as *const u64, faults: (*hdr).faults.ptr(), ncodes: (*hdr).ncodes }
    }

    pub fn header(&self) -> *const Header {
        self.hdr
    }

    pub fn ncodes(&self) -> u32 {
        self.ncodes
    }

    pub fn has_faults(&self) -> bool {
        !self.faults.is_null()
    }

    /// # Safety — `code < ncodes()`.
    #[inline(always)]
    pub unsafe fn get(&self, code: u32) -> u64 {
        *self.vals.add(code as usize)
    }

    /// # Safety — `code < ncodes()`.
    #[inline(always)]
    pub unsafe fn faulted(&self, code: u32) -> bool {
        faulted(self.faults, code)
    }

    /// # Safety — as `I64::gather`.
    #[inline(always)]
    pub unsafe fn gather<C: CodeCell>(&self, codes: *const C, valid: *const u64, rows: usize, out: *mut u64) -> Result<(), Deferred> {
        gather_words(self.vals, self.faults, codes, valid, rows, out)
    }
}

/// One 128-bit fingerprint per code (`fp::fp128` of the value `f` returns): the cross-part identity of
/// text keys (16 B per code, `[lo, hi]`).
#[derive(Clone, Copy, Debug)]
pub struct Fp128 {
    hdr: *const Header,
    vals: *const [u64; 2],
    faults: *const u64,
    ncodes: u32,
}

impl Fp128 {
    /// `NoSeed` when the process seed is not installed (`fp::install`).
    pub fn build(dict: &dyn Dict, region: &mut Region, f: &mut dyn FnMut(View) -> Result<View, Fault>) -> Result<Fp128, BuildError> {
        let (k0, k1) = crate::fp::seed().ok_or(BuildError::NoSeed)?;
        let mut b = Build::start(dict, region, crate::lane::ELEM_FP128)?;
        let vals = region.alloc_zeroed(b.ncodes() as usize * 16, 8)? as *mut [u64; 2];
        sweep(dict, region, &mut b, &mut |code, v, _| {
            let d = f(v)?;
            // SAFETY: `code < ncodes`; `d` is the caller's live view.
            unsafe { *vals.add(code as usize) = crate::fp::siphash128(k0, k1, d.bytes()) };
            Ok(())
        })?;
        // SAFETY: the header this build wrote.
        Ok(unsafe { Fp128::at(b.finish(vals as *const u8)) })
    }

    /// textkeys step 3 (lowerv4 road `key.canon`): the canonical text key image table (`fp::canon_of`) of `f`'s value per
    /// entry (a bpchar's trimmed render), in the `Fp128` layout: an entry of at most `fp::INLINE_BYTES` bytes is its own
    /// image, a longer one its fingerprint with `fp::HASHED` set, read from `raw` (the part's EXISTING `Fp128` table over
    /// the same values, when one is resident: no second hash) else hashed here, as that table's build would. The null code
    /// stays `[0, 0]`.
    pub fn build_canon(dict: &dyn Dict, region: &mut Region, f: &mut dyn FnMut(View) -> Result<View, Fault>, raw: Option<&Fp128>) -> Result<Fp128, BuildError> {
        let keys = crate::fp::keys().ok_or(BuildError::NoSeed)?;
        let mut b = Build::start(dict, region, crate::lane::ELEM_FP128)?;
        let raw = raw.filter(|r| r.ncodes == b.ncodes() && !r.has_faults());
        let vals = region.alloc_zeroed(b.ncodes() as usize * 16, 8)? as *mut [u64; 2];
        sweep(dict, region, &mut b, &mut |code, v, _| {
            let d = f(v)?;
            // SAFETY: `d` is the caller's live view.
            let bytes = unsafe { d.bytes() };
            // SAFETY: `code < ncodes` (`raw`'s too, checked above).
            let img = crate::fp::canon_of(bytes, || raw.map_or_else(|| keys.fp_bytes(bytes), |r| unsafe { r.get(code) }));
            // SAFETY: `code < ncodes`.
            unsafe { *vals.add(code as usize) = img };
            Ok(())
        })?;
        // SAFETY: the header this build wrote.
        Ok(unsafe { Fp128::at(b.finish(vals as *const u8)) })
    }

    /// The fingerprints of `f(code)`'s bytes over a code range (a derived value per code: the bytes
    /// need only live through the call); a faulting code is a fault bit.
    pub fn build_by_code(codes: &Codes, region: &mut Region, f: &mut dyn FnMut(u32) -> Result<View, Fault>) -> Result<Fp128, BuildError> {
        let (k0, k1) = crate::fp::seed().ok_or(BuildError::NoSeed)?;
        let mut b = Build::start(codes, region, crate::lane::ELEM_FP128)?;
        let vals = region.alloc_zeroed(b.ncodes() as usize * 16, 8)? as *mut [u64; 2];
        for code in 0..b.ncodes() {
            if Some(code) == codes.null {
                continue;
            }
            match f(code) {
                // SAFETY: `code < ncodes`; `d` is live for the call.
                Ok(d) => unsafe { *vals.add(code as usize) = crate::fp::siphash128(k0, k1, d.bytes()) },
                Err(_) => b.fault(region, code)?,
            }
        }
        // SAFETY: the header this build wrote.
        Ok(unsafe { Fp128::at(b.finish(vals as *const u8)) })
    }

    /// `build_by_code` over values known equal by a canonical index (`Derived::canonical`): the
    /// SipHash runs once per distinct value (`ndistinct` of them), every other code copies its
    /// canonical's fingerprint. `canon(code)` is `u32::MAX` for a faulting code.
    pub fn build_by_canon(codes: &Codes, region: &mut Region, ndistinct: u32, canon: &mut dyn FnMut(u32) -> u32, f: &mut dyn FnMut(u32) -> View) -> Result<Fp128, BuildError> {
        let (k0, k1) = crate::fp::seed().ok_or(BuildError::NoSeed)?;
        let mut b = Build::start(codes, region, crate::lane::ELEM_FP128)?;
        let vals = region.alloc_zeroed(b.ncodes() as usize * 16, 8)? as *mut [u64; 2];
        let nd = ndistinct as usize;
        // the faults first: their bitmap lives in the region under the scratch popped below
        for code in 0..b.ncodes() {
            if Some(code) != codes.null && canon(code) == u32::MAX {
                b.fault(region, code)?;
            }
        }
        let mark = region.mark();
        let by = region.alloc_zeroed(nd.max(1) * 16, 8)? as *mut [u64; 2];
        let seen = region.alloc_zeroed(nd.div_ceil(64).max(1) * 8, 8)? as *mut u64;
        for code in 0..b.ncodes() {
            if Some(code) == codes.null {
                continue;
            }
            let c = canon(code);
            if c == u32::MAX {
                continue;
            }
            if c as usize >= nd {
                return Err(BuildError::TooManyCodes);
            }
            // SAFETY: `code < ncodes`, `c < ndistinct`; the bitmap has `ndistinct` bits.
            unsafe {
                let w = seen.add(c as usize / 64);
                if *w >> (c % 64) & 1 == 0 {
                    *w |= 1 << (c % 64);
                    *by.add(c as usize) = crate::fp::siphash128(k0, k1, f(code).bytes());
                }
                *vals.add(code as usize) = *by.add(c as usize);
            }
        }
        region.reset_to(mark);
        // SAFETY: the header this build wrote.
        Ok(unsafe { Fp128::at(b.finish(vals as *const u8)) })
    }

    /// A derived key's fingerprint table in one sweep: `f` maps each entry to its derived bytes, a subslice
    /// of `over` (the part's resident payload: the in-place plan), and each code's `siphash128` goes straight into the
    /// table. Entries come in byte order, so a derived value repeats in runs: a code whose bytes equal the code before's
    /// copies its fingerprint, and a run hashes once. No derived views, hashes or canonical indexes are kept
    /// (`Derived::build_over` then `Fp128::build_by_canon` kept 20 bytes a code and walked the codes twice more), and the
    /// table is the statement's like any other (built per statement, dropped with it). A code `f` faults stays [0, 0] and
    /// is recorded as a fault; the null code stays [0, 0].
    pub fn build_derived_over(dict: &dyn Dict, region: &mut Region, over: &[u8], f: &mut dyn FnMut(View, &mut Region) -> Result<View, Halt>) -> Result<Fp128, BuildError> {
        let (k0, k1) = crate::fp::seed().ok_or(BuildError::NoSeed)?;
        let mut b = Build::start(dict, region, crate::lane::ELEM_FP128)?;
        let vals = region.alloc_zeroed(b.ncodes() as usize * 16, 8)? as *mut [u64; 2];
        let (lo, hi) = (over.as_ptr() as usize, over.as_ptr() as usize + over.len());
        // the previous code's derived bytes (a view into `over`, live through the build) and fingerprint
        let mut prev: Option<(&[u8], [u64; 2])> = None;
        sweep(dict, region, &mut b, &mut |code, v, region| {
            let d = f(v, region)?;
            if d.len != 0 && !(lo <= d.ptr as usize && d.ptr as usize + d.len as usize <= hi) {
                return Err(Halt::Region(BuildError::OutsideBase));
            }
            // SAFETY: `d` lies in `over` (or is empty), live for the build.
            let bytes = unsafe { d.bytes() };
            let fp = match prev {
                Some((pb, pf)) if pb == bytes => pf,
                _ => crate::fp::siphash128(k0, k1, bytes),
            };
            // SAFETY: `code < ncodes`; the table holds `ncodes` cells.
            unsafe { *vals.add(code as usize) = fp };
            prev = Some((bytes, fp));
            Ok(())
        })?;
        // SAFETY: the header this build wrote.
        Ok(unsafe { Fp128::at(b.finish(vals as *const u8)) })
    }

    /// `build_derived_over`'s table from a pulled sequence (V4R-30, L-DERIVE): `next` yields each entry's code, its derived
    /// bytes (inside `over`) and whether they are the previous entry's (a memo hit: the fingerprint is reused unread), in
    /// code order, `None` at the end. One monomorphized loop, no per-entry dyn call; the same table as the swept form.
    /// Never inlined: the loop keeps its own frame (and registers) with `next` and its per-entry work inlined into it, not
    /// its caller's (D3424).
    #[inline(never)]
    pub fn build_derived_seq<'o>(dict: &dyn Dict, region: &mut Region, over: &'o [u8], mut next: impl FnMut() -> Option<Result<(u32, &'o [u8], bool), BuildError>>) -> Result<Fp128, BuildError> {
        let (k0, k1) = crate::fp::seed().ok_or(BuildError::NoSeed)?;
        let null = dict.null_code();
        let b = Build::start(dict, region, crate::lane::ELEM_FP128)?;
        let n = b.ncodes();
        // unzeroed (V4R-160): each cell is written once, in code order: a pulled code's print, else [0, 0] for a code the
        // sequence skips (the null code, a gap) or never reaches; no zeroing pass over the whole table first. The cells
        // are reached only through the raw `vals` until every one is written (the `Fp128` over them is made after the
        // tail's zero-fill; an error return makes none), so no reference ever covers an unwritten byte. Every build of
        // the table, over a fresh or a reused (dirty) region, runs this fill: no path returns a table it skipped.
        let vals = region.alloc(n as usize * 16, 8)? as *mut [u64; 2];
        let mut unset = 0u32; // cells `unset..n` not yet written
        let zero_to = |unset: &mut u32, to: u32| {
            if *unset < to {
                // SAFETY: `unset < to <= n`; the table holds `n` cells.
                unsafe { core::ptr::write_bytes(vals.add(*unset as usize), 0, (to - *unset) as usize) };
                *unset = to;
            }
        };
        let (lo, hi) = (over.as_ptr() as usize, over.as_ptr() as usize + over.len());
        let mut prev: Option<(&[u8], [u64; 2])> = None;
        while let Some(r) = next() {
            let (code, bytes, same) = r?;
            if Some(code) == null || code >= n {
                continue;
            }
            zero_to(&mut unset, code);
            if !bytes.is_empty() && !(lo <= bytes.as_ptr() as usize && bytes.as_ptr() as usize + bytes.len() <= hi) {
                return Err(BuildError::OutsideBase);
            }
            let fp = match prev {
                Some((_, pf)) if same => pf,
                Some((pb, pf)) if pb == bytes => pf,
                _ => crate::fp::siphash128(k0, k1, bytes),
            };
            // SAFETY: `code < ncodes`; the table holds `ncodes` cells.
            unsafe { *vals.add(code as usize) = fp };
            unset = unset.max(code + 1);
            prev = Some((bytes, fp));
        }
        zero_to(&mut unset, n);
        // SAFETY: the header this build wrote.
        Ok(unsafe { Fp128::at(b.finish(vals as *const u8)) })
    }

    /// # Safety — `hdr` is an `Fp128` table header.
    pub unsafe fn at(hdr: *const Header) -> Fp128 {
        debug_assert_eq!((*hdr).elem, crate::lane::ELEM_FP128);
        Fp128 { hdr, vals: (*hdr).data.ptr() as *const [u64; 2], faults: (*hdr).faults.ptr(), ncodes: (*hdr).ncodes }
    }

    pub fn header(&self) -> *const Header {
        self.hdr
    }

    pub fn ncodes(&self) -> u32 {
        self.ncodes
    }

    pub fn has_faults(&self) -> bool {
        !self.faults.is_null()
    }

    /// # Safety — `code < ncodes()`.
    #[inline(always)]
    pub unsafe fn get(&self, code: u32) -> [u64; 2] {
        *self.vals.add(code as usize)
    }

    /// The entry's address (a prefetch target).
    /// # Safety — `code < ncodes()`.
    pub unsafe fn entry_ptr(&self, code: u32) -> *const u8 {
        self.vals.add(code as usize) as *const u8
    }

    /// # Safety — `code < ncodes()`.
    #[inline(always)]
    pub unsafe fn faulted(&self, code: u32) -> bool {
        faulted(self.faults, code)
    }
}

#[inline(always)]
pub(crate) unsafe fn gather_words<C: CodeCell>(vals: *const u64, faults: *const u64, codes: *const C, valid: *const u64, rows: usize, out: *mut u64) -> Result<(), Deferred> {
    if valid.is_null() {
        for i in 0..rows {
            *out.add(i) = *vals.add((*codes.add(i)).code() as usize);
        }
    } else {
        for i in 0..rows {
            if bit(valid, i as u32) {
                *out.add(i) = *vals.add((*codes.add(i)).code() as usize);
            }
        }
    }
    if faults.is_null() {
        return Ok(());
    }
    for w in 0..rows.div_ceil(64) {
        first_fault(faults, codes, w, keep_word(valid, rows, true, &[], w))?;
    }
    Ok(())
}

/// The bytes of one dictionary index entry (`pgrc_format::dict::DICT_INDEX_ENTRY_LEN`): payload offset,
/// byte length, the char field.
pub const INDEX_ENTRY_LEN: usize = 12;
/// A byte length per code read from the part's resident dictionary index (the `byte_len` field
/// of entry `code`), or with `DELTA` the char length `byte_len − char_field` (the index's delta
/// form) — no table is built, per statement or across statements; the lane points into the index.
#[derive(Clone, Copy, Debug)]
pub struct IdxLen<const DELTA: bool> {
    data: *const u8,
    ncodes: u32,
}

impl<const DELTA: bool> IdxLen<DELTA> {
    /// # Safety — `hdr` is an `ELEM_LEN_IDX` / `ELEM_LEN_IDX_DELTA` header (`index_len_header`).
    pub unsafe fn at(hdr: *const Header) -> IdxLen<DELTA> {
        IdxLen { data: (*hdr).data.ptr(), ncodes: (*hdr).ncodes }
    }

    pub fn ncodes(&self) -> u32 {
        self.ncodes
    }

    /// # Safety — `code < ncodes()`.
    #[inline(always)]
    pub unsafe fn get(&self, code: u32) -> i64 {
        let p = self.data.add(code as usize * INDEX_ENTRY_LEN);
        let bytes = core::ptr::read_unaligned(p as *const u32);
        if DELTA {
            bytes.wrapping_sub(core::ptr::read_unaligned(p.add(4) as *const u32)) as i64
        } else {
            bytes as i64
        }
    }

}

/// A per-code length: an `I64` table (a verbatim part's, built by a sweep) or the index view.
pub trait Lens: Copy {
    /// # Safety — `code` is a code of the lane's dictionary.
    unsafe fn get(&self, code: u32) -> i64;
}

impl Lens for I64 {
    #[inline(always)]
    unsafe fn get(&self, code: u32) -> i64 {
        I64::get(self, code)
    }
}

impl<const DELTA: bool> Lens for IdxLen<DELTA> {
    #[inline(always)]
    unsafe fn get(&self, code: u32) -> i64 {
        IdxLen::get(self, code)
    }
}

/// Which length an index-view lane reads: the byte length, the char field as an absolute count
/// (the writer's form of record), or the char length as `byte_len − char_field` (the delta form).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LenField {
    Bytes,
    CharsAbsolute,
    CharsDelta,
}

/// The header of an index-view length lane over `index` (the part's dictionary index, `ncodes`
/// entries of `INDEX_ENTRY_LEN` bytes, live for the part's open).
/// # Safety — `index` holds `ncodes * INDEX_ENTRY_LEN` bytes and outlives every lane published from the header.
pub unsafe fn index_len_header(region: &mut Region, index: *const u8, ncodes: u32, field: LenField) -> Result<*const Header, BuildError> {
    let codes = Codes { ncodes, null: None };
    let (elem, mode) = match field {
        LenField::Bytes => (crate::lane::ELEM_LEN_IDX, LEN_MODE_BYTES),
        LenField::CharsAbsolute => (crate::lane::ELEM_LEN_IDX, LEN_MODE_CHARS_ABSOLUTE),
        LenField::CharsDelta => (crate::lane::ELEM_LEN_IDX_DELTA, LEN_MODE_CHARS_DELTA),
    };
    let b = Build::start(&codes, region, elem)?;
    (*b.hdr).pad[0] = mode;
    Ok(b.finish(index.add(4)))
}

/// `Header.pad[0]` of an index-view lane: which field the 8-byte read at entry offset 4 yields.
pub const LEN_MODE_BYTES: u8 = 1;
pub const LEN_MODE_CHARS_ABSOLUTE: u8 = 2;
pub const LEN_MODE_CHARS_DELTA: u8 = 3;
