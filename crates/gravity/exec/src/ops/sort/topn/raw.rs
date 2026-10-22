//! topn's raw half in exec (rule R6: the family's unsafe code lives here): proto.rs's calls of
//! a region reader over a producer's finished region, and keyed.twopass's header cell (pass 2's IR reads it through its
//! address), each with the SAFETY reason it had in proto.rs.
#![allow(unsafe_code)]
use core::cell::UnsafeCell;

use gravity_program::protos::RegionFn;

/// Reader `read` over `region`, a producer's finished breaker region the caller found at least `MIN_REGION` bytes long
/// (proto.rs `Reader::after`, `BuildSideLive::after_pipeline`), with `words` and its slot's ledger `owner`, its rows to `emit`.
pub(super) fn region_read(read: RegionFn, region: &[u8], words: &[u64], owner: Option<memledger::Owner>, emit: &mut dyn FnMut(u64, u64, u64)) -> Option<usize> {
    // SAFETY: the producer's finished region of at least `MIN_REGION` bytes; the reader checks its header first.
    unsafe { read(region.as_ptr(), words, owner, emit) }
}

/// Whether `region` is a finished `Owned` leader region (at least an `Owned` header long, its kind byte `TABLE_OWNED`):
/// the owner phase's result over every partition, the spilled epochs included (proto.rs `Reader::spilled`, D3158).
pub(super) fn owned(region: &[u8]) -> bool {
    // SAFETY: a region at least an `Owned` header long; its kind byte is read before any view of the layout.
    region.len() >= gravity_abi::breaker::OWNED_HDR && unsafe { gravity_abi::breaker::table_kind(region.as_ptr()) } == gravity_abi::breaker::TABLE_OWNED
}

/// A header pass 2's IR reads through its address (keyed.twopass's `AdmitHdr`, behind its protocol word), written by the
/// runtime once pass 1 finished.
pub(super) struct IrCell<T>(UnsafeCell<T>);

impl<T> IrCell<T> {
    pub(super) fn new(v: T) -> IrCell<T> {
        IrCell(UnsafeCell::new(v))
    }

    /// The address the protocol word carries.
    pub(super) fn addr(&self) -> u64 {
        self.0.get() as u64
    }

    /// The header `v`, written after pass 1 and before pass 2 starts.
    pub(super) fn set(&self, v: T) {
        // SAFETY: pass 2 has not started; its IR reads the header only at its open, after this.
        unsafe { *self.0.get() = v };
    }
}

/// A test build's region read as `join.buildside`'s reading (proto.rs's after-build tests): word 0 is its rows, emitted
/// as the one `(0, rows, 0)`.
#[cfg(test)]
pub(super) unsafe fn test_rows(region: *const u8, _words: &[u64], _owner: Option<memledger::Owner>, emit: &mut dyn FnMut(u64, u64, u64)) -> Option<usize> {
    // SAFETY: a test region of at least `MIN_REGION` bytes; word 0 is its row count.
    emit(0, unsafe { core::ptr::read_unaligned(region as *const u64) }, 0);
    Some(1)
}

/// A test build's region read as `join.rtfilter`'s export: word 0 is its rows, each next word a row's key; `(0, lo, hi)`
/// the keys' range, then `(1, key, 0)` per row. No item at all is an empty build.
#[cfg(test)]
pub(super) unsafe fn test_keys(region: *const u8, _words: &[u64], _owner: Option<memledger::Owner>, emit: &mut dyn FnMut(u64, u64, u64)) -> Option<usize> {
    // SAFETY: a test region: word 0 its row count `n`, then `n` key words, all within the region.
    let key = |i: usize| unsafe { core::ptr::read_unaligned((region as *const u64).add(i)) } as i64;
    let n = key(0) as usize;
    if let (Some(lo), Some(hi)) = ((1..=n).map(key).min(), (1..=n).map(key).max()) {
        emit(0, lo as u64, hi as u64);
    }
    (1..=n).for_each(|i| emit(1, key(i) as u64, 0));
    Some(n)
}
