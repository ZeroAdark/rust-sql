//! pool_catalog's unsafe (R6 home; its child): a statement catalog's slices widened for the claim that arms them.
#![allow(unsafe_code)]

/// `s` as `'static`. The caller is `StatementCatalog::arm`: the guard it returns holds an `Arc` of the catalog that
/// owns `s`, and its drop empties the armed slots before that `Arc` goes.
pub(super) fn claim_slice<T: 'static>(s: &[T]) -> &'static [T] {
    // SAFETY: the caller's contract above.
    unsafe { &*(s as *const [T]) }
}

/// The NameData image (NAMEDATALEN bytes) of a constant `name` argument at `addr`, up to its NUL. The caller is
/// `name_encoding` over a lowered program's non-null by-reference `name` Const, whose image the program holds.
pub(super) fn name_bytes<'a>(addr: usize) -> &'a [u8] {
    // SAFETY: the caller's contract above: `addr` is a live NAMEDATALEN-byte image.
    let all = unsafe { core::slice::from_raw_parts(addr as *const u8, types_core::fmgr::NAMEDATALEN as usize) };
    &all[..all.iter().position(|&b| b == 0).unwrap_or(all.len())]
}
