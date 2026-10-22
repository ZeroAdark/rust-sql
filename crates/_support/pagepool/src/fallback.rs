//! wasm32-wasip1: no mmap, so the pool never turns on and every face refuses (the callers keep their present road).

use core::ffi::{c_int, CStr};

pub(crate) fn phys_bytes() -> usize {
    0
}

pub(crate) fn env_is_one(_name: &CStr) -> bool {
    false
}

/// wasm: the pool never turns on.
pub(crate) fn env_on_by_default(_name: &CStr) -> bool {
    false
}

pub(crate) fn env_str(_name: &CStr) -> Option<&'static str> {
    None
}

pub(crate) fn page_size() -> usize {
    65536
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

pub(crate) unsafe fn reserve(_len: usize, _align: usize) -> Option<usize> {
    None
}

pub(crate) unsafe fn open(_at: usize, _len: usize, _huge: bool, _name: &CStr) -> bool {
    false
}

pub(crate) unsafe fn guard(_at: usize, _len: usize) -> bool {
    false
}

pub(crate) unsafe fn populate(_at: usize, _len: usize) -> bool {
    false
}

pub(crate) unsafe fn release(_at: usize, _len: usize) -> c_int {
    -1
}

pub(crate) unsafe fn release_base(_at: usize, _len: usize) -> c_int {
    -1
}

pub(crate) unsafe fn move_pages(_from: usize, _to: usize, _len: usize, _keep_from: bool) -> bool {
    false
}

pub(crate) unsafe fn set_prot(_at: usize, _len: usize, _rw: bool) -> bool {
    false
}

pub(crate) fn map_headroom() -> Option<usize> {
    None
}

pub(crate) unsafe fn age_release(at: usize, len: usize) -> Option<usize> {
    // SAFETY: the caller's contract.
    unsafe { age_release_with(at, len, true) }
}

pub(crate) unsafe fn age_release_with(_at: usize, _len: usize, _split: bool) -> Option<usize> {
    None
}
