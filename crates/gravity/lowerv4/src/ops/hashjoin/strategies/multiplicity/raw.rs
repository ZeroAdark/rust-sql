//! multiplicity.rs's unsafe region work (rule R6: a module's unsafe code lives in its raw.rs), moved unchanged from it.
//! The parent re-exports these names, so its callers and the build region's raw.rs read them as before.
#![allow(unsafe_code)]
#[allow(unused_imports)]
use super::*;

/// Adds record `from`'s count to the link word `to` (the kept record of its key, or its dense slot).
///
/// # Safety
/// `to` is a record's or slot's link word; `t` is the build region holding record `from`.
pub(crate) unsafe fn bump(t: &Rt, to: *mut u8, from: usize) {
    let add = (*t.link(from) >> 32) + 1;
    wr::<u64>(to, 0, rd::<u64>(to, 0).wrapping_add(add << 32));
}

/// Record `i`'s key already in the chain from `head` (record index + 1, 0: none): the record holding it, which takes
/// `i`'s count (`bump`); `None` when `i` is the key's first record.
///
/// # Safety
/// `t` is a build region whose chains below `head` are laid.
pub(crate) unsafe fn collapse(t: &Rt, head: u64, i: usize) -> Option<usize> {
    let mut j1 = head;
    while j1 != 0 {
        let j = j1 as usize - 1;
        if (0..t.words).all(|w| rd::<u64>(t.rec(j), 8 * w) == rd::<u64>(t.rec(i), 8 * w)) {
            bump(t, t.link(j) as *mut u8, i);
            return Some(j);
        }
        j1 = *t.link(j) & LOW;
    }
    None
}
