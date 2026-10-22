//! `twopass`' unsafe steps (rule R6, the module's raw home): the pass-1 result's kind test and the declared readers
//! called over it, and the codes fold's view of the merged table, its rendered words and its records' key words.
#![allow(unsafe_code)]

use super::*;

/// Whether `region` is a finished `Owned` leader region (at least an `Owned` header long, its kind byte `TABLE_OWNED`).
pub(super) fn is_owned(region: &[u8]) -> bool {
    region.len() >= gravity_abi::breaker::OWNED_HDR && unsafe { gravity_abi::breaker::table_kind(region.as_ptr()) } == gravity_abi::breaker::TABLE_OWNED
}

/// The declared `export` over the pass-1 result `region` (`is_owned`), each exported key to `f`.
pub(super) fn export_keys(export: gravity_program::ExportKeys, region: &[u8], p1: &[u64], f: &mut dyn FnMut(u64)) -> Option<usize> {
    // SAFETY: the leader region after the owner phase carries the `Owned` header.
    unsafe { export(region.as_ptr(), p1, f) }
}

/// The declared `marks` over the pass-1 result `region` (`is_owned`), each key's granule marks to `f`.
pub(super) fn granule_marks(marks: gravity_program::GranuleMarks, region: &[u8], p1: &[u64], f: &mut dyn FnMut(u64, u64, u64)) -> Option<usize> {
    // SAFETY: `region` is the owned pass-1 result `build_admit_set` read.
    unsafe { marks(region.as_ptr(), p1, f) }
}

/// The merged phase-1 table at the leader region `ptr` (at least `HASHAGG_HDR` bytes, which the caller checked), or
/// None when its kind byte names another layout.
pub(super) fn agg_table<'a>(ptr: *mut u8) -> Option<&'a mut gravity_abi::breaker::HashAgg> {
    // SAFETY: the leader state is the merged table its own header describes (layout, capacity); a
    // phase-1 table (the width-1 arm of the tier-3 kernel) is the emit's to fold.
    if unsafe { gravity_abi::breaker::table_kind(ptr) } != gravity_abi::breaker::TABLE_AGG {
        return None;
    }
    // SAFETY: the kind check above says the region starts with a `HashAgg` header (HASHAGG_HDR bytes, checked), and
    // `exec.node_mut()` holds the only borrow of the slot region for this fold.
    Some(unsafe { &mut *(ptr as *mut gravity_abi::breaker::HashAgg) })
}

/// The payload of the rendered word `d`.
#[inline(always)]
pub(super) fn rendered_bytes<'a>(d: u64) -> &'a [u8] {
    // SAFETY: a rendered word points at a 4B-U varlena image resident with its part for the Node's life.
    unsafe { core::slice::from_raw_parts((d as *const u8).add(4), (*(d as *const u32) >> 2) as usize - 4) }
}

/// The key word at `off` of the record at `r`.
#[inline(always)]
pub(super) fn record_word(r: *const u8, off: usize) -> u64 {
    // SAFETY: `r` is a live record of the table's layout and `off` one of its key offsets.
    unsafe { *(r.add(off) as *const u64) }
}
