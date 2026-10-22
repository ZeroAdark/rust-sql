//! Dictionary evaluation (docs/gravity/expr.md §2.2): a sub-expression over a dictionary column is
//! evaluated once per entry into a per-code table in a part-resident region, built under a
//! once-cell per (part, epoch, expression) and gathered from the kernel's loop; faults are
//! deferred to the gather that selects them.
#![no_std]

pub mod derived;
pub mod fp;
pub mod lane;
pub mod like;
pub mod lookup;
pub mod once;
pub mod regexp;
pub mod substr;
pub mod table;

pub use derived::Derived;
pub use lane::{publish, ELEM_BITS, ELEM_DERIVED, ELEM_FP128, ELEM_HASH, ELEM_I64, ELEM_LEN_IDX, ELEM_LEN_IDX_DELTA};
pub use like::{EntryIndex, Matcher, Mode, PayloadDict};
pub use once::{Claim, Key, OnceTable};
pub use table::{index_len_header, Bits, CodeCell, Codes, Deferred, Fp128, Halt, Hash, Header, IdxLen, LenField, Lens, HDR_DATA, HDR_ELEM, HDR_FAULTS, HDR_FLAGS, HDR_MODE, HDR_NCODES, I64};

/// A byte string by address: a dictionary entry's payload, a derived value, a pattern.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct View {
    pub ptr: *const u8,
    pub len: u32,
}

impl View {
    pub const EMPTY: View = View { ptr: core::ptr::null(), len: 0 };

    pub const fn of(b: &[u8]) -> View {
        View { ptr: b.as_ptr(), len: b.len() as u32 }
    }

    /// # Safety — the bytes are live for `'a`.
    #[inline(always)]
    pub unsafe fn bytes<'a>(self) -> &'a [u8] {
        if self.ptr.is_null() {
            &[]
        } else {
            core::slice::from_raw_parts(self.ptr, self.len as usize)
        }
    }
}

/// A per-entry evaluation that raises in PostgreSQL: recorded as a bit, re-evaluated at the raise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fault(pub u32);

/// The dictionary a table is built over: `ncodes` entries, each code once, in any order (every
/// build sets by code); a code that stands for NULL (none in pgrc, where validity is a bitmap) is
/// skipped by every build.
pub trait Dict {
    fn ncodes(&self) -> u32;
    fn null_code(&self) -> Option<u32> {
        None
    }
    fn entries(&self, f: &mut dyn FnMut(u32, View));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildError {
    RegionFull { needed: usize },
    TooManyCodes,
    /// An `Fp128` build before `fp::install`.
    NoSeed,
    /// A `Derived::build_over` value outside the payload it was to lie in.
    OutsideBase,
}

/// The arena tables are built in: a bump over caller memory (the part-resident region), LIFO
/// resets for scratch, no other allocation anywhere in the crate.
pub struct Region {
    base: *mut u8,
    cur: *mut u8,
    end: *mut u8,
}

impl Region {
    /// # Safety — `ptr..ptr+len` is writable for the region's lifetime and 8-aligned.
    pub unsafe fn new(ptr: *mut u8, len: usize) -> Region {
        Region { base: ptr, cur: ptr, end: ptr.add(len) }
    }

    pub fn alloc(&mut self, bytes: usize, align: usize) -> Result<*mut u8, BuildError> {
        let start = (self.cur as usize).next_multiple_of(align.max(1));
        let stop = start.checked_add(bytes).ok_or(BuildError::RegionFull { needed: bytes })?;
        if stop > self.end as usize {
            return Err(BuildError::RegionFull { needed: stop - self.cur as usize });
        }
        self.cur = stop as *mut u8;
        Ok(start as *mut u8)
    }

    pub fn alloc_zeroed(&mut self, bytes: usize, align: usize) -> Result<*mut u8, BuildError> {
        let p = self.alloc(bytes, align)?;
        // SAFETY: `p..p+bytes` was just reserved inside the region.
        unsafe { core::ptr::write_bytes(p, 0, bytes) };
        Ok(p)
    }

    pub fn copy(&mut self, b: &[u8]) -> Result<View, BuildError> {
        let p = self.alloc(b.len(), 1)?;
        // SAFETY: `p..p+len` was just reserved; `b` lies outside it or is read before the write.
        unsafe { core::ptr::copy(b.as_ptr(), p, b.len()) };
        Ok(View { ptr: p, len: b.len() as u32 })
    }

    pub fn base(&self) -> *const u8 {
        self.base
    }

    pub fn used(&self) -> usize {
        self.cur as usize - self.base as usize
    }

    pub fn remaining(&self) -> usize {
        self.end as usize - self.cur as usize
    }

    pub fn contains(&self, p: *const u8) -> bool {
        (self.base as usize..self.cur as usize).contains(&(p as usize)) || (p == self.cur && self.cur == self.end)
    }

    pub fn mark(&self) -> usize {
        self.used()
    }

    pub fn reset_to(&mut self, mark: usize) {
        debug_assert!(mark <= self.used());
        // SAFETY: `mark` is a previous `used()`, inside the region.
        self.cur = unsafe { self.base.add(mark) };
    }
}
