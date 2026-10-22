//! Miri: the reservation is one zeroed heap allocation; mappings are always open, release and guard zero the bytes.

use core::ffi::{c_int, CStr};

pub(crate) fn phys_bytes() -> usize {
    1 << 30
}

pub(crate) fn env_is_one(_name: &CStr) -> bool {
    false
}

/// Miri reads no environment: the switches stay off, as before; the tests force them through the seams.
pub(crate) fn env_on_by_default(_name: &CStr) -> bool {
    false
}

pub(crate) fn env_str(_name: &CStr) -> Option<&'static str> {
    None
}

pub(crate) fn page_size() -> usize {
    4096
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Thp {
    Always,
    Madvise,
    Never,
}

pub(crate) fn thp() -> Thp {
    Thp::Never
}

pub(crate) fn nodes() -> u8 {
    1
}

/// The reservation, kept reachable from a static (Miri's leak check).
static BASE: core::sync::atomic::AtomicPtr<u8> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

pub(crate) unsafe fn reserve(len: usize, _align: usize) -> Option<usize> {
    let layout = std::alloc::Layout::from_size_align(len, crate::superpage::SUPER).ok()?;
    // SAFETY: a non-zero layout; the allocation is the pool's for the process's life.
    let p = unsafe { std::alloc::alloc_zeroed(layout) };
    BASE.store(p, core::sync::atomic::Ordering::Relaxed);
    (!p.is_null()).then_some(p as usize)
}

pub(crate) unsafe fn open(_at: usize, _len: usize, _huge: bool, _name: &CStr) -> bool {
    true
}

pub(crate) unsafe fn guard(at: usize, len: usize) -> bool {
    // SAFETY: the caller's own range of the reservation.
    unsafe { core::ptr::write_bytes(at as *mut u8, 0, len) };
    true
}

pub(crate) unsafe fn populate(_at: usize, _len: usize) -> bool {
    true
}

pub(crate) unsafe fn release(at: usize, len: usize) -> c_int {
    // SAFETY: as `guard`.
    unsafe { core::ptr::write_bytes(at as *mut u8, 0, len) };
    0
}

/// pagepool P3 (F1 / F2): under miri no swap device, and no residency is read.
pub(crate) fn swap_on() -> bool {
    false
}

/// pagepool P3 (F1 / F2): as `release`, the whole range counted (no residency is read under miri).
pub(crate) unsafe fn age_release(at: usize, len: usize) -> Option<usize> {
    // SAFETY: the caller's contract is `release`'s.
    (unsafe { release(at, len) } == 0).then_some(len)
}

/// As `age_release`: no folio to split under miri.
pub(crate) unsafe fn age_release_with(at: usize, len: usize, _split: bool) -> Option<usize> {
    // SAFETY: the caller's contract is `release`'s.
    unsafe { age_release(at, len) }
}

pub(crate) fn split_partial(_at: usize, _len: usize) {}

pub(crate) unsafe fn release_base(at: usize, len: usize) -> c_int {
    // SAFETY: as `guard`.
    unsafe { release(at, len) }
}

pub(crate) unsafe fn move_pages(_from: usize, _to: usize, _len: usize, _keep_from: bool) -> bool {
    false
}

pub(crate) unsafe fn set_prot(_at: usize, _len: usize, _rw: bool) -> bool {
    true
}

pub(crate) fn map_headroom() -> Option<usize> {
    None
}
