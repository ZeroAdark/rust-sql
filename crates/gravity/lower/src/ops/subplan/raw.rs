//! `ops::subplan`'s raw half in lower (rule R6: the family's unsafe code lives here): the hashed SubPlan's key-set probe
//! over the set's raw header (PG's `ExecHashSubPlan` lookup, `FindTupleHashEntry`); keyset.rs re-exports it under its path.
#![allow(unsafe_code)]
use super::keyset::{slot_of, width_mask, KeyTable, HAS_NULL, NONEMPTY};
use gravity_abi::PtrWord;

/// # Safety
/// `t` is a live `KeyTable` whose arrays are `mask + 1` long with at least one empty tag.
pub unsafe fn lookup(t: *const KeyTable, key: u64, width: u8) -> (bool, bool, bool) {
    // SAFETY: the caller's.
    let t = unsafe { &*t };
    let (nonempty, has_null) = (t.flags & NONEMPTY != 0, t.flags & HAS_NULL != 0);
    if !nonempty {
        return (false, false, has_null);
    }
    let k = key & width_mask(width);
    let mut h = slot_of(k, t.mask);
    // Bounded walk: every slot at most once; a key not met in `mask + 1` slots is absent.
    for _ in 0..=t.mask {
        // SAFETY: `h` is masked into the arrays.
        let (tag, at) = unsafe { (*t.tags.ptr().add(h as usize), *t.keys.ptr().add(h as usize)) };
        if tag == 0 {
            return (true, false, has_null);
        }
        if at == k {
            return (true, true, has_null);
        }
        h = (h + 1) & t.mask;
    }
    (true, false, has_null)
}
