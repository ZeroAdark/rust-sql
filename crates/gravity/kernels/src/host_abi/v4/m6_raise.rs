//! M6's errors raised in IR (`m6_raise`, D1292 Option 1, main 09-28): PostgreSQL's error for a code, stashed as the
//! statement's error, `ERROR` returned for the IR to leave with (host_abi/v4/m11.rs's `m11_raise` pattern). Owner M6-REPS.
use crate::contract::Host;
use gravity_abi::Ctx;

/// `m6_raise` code 0 (PG `varstr_sortsupport`'s `check_collation_set`, at `tuplesort_begin_heap`): a sort key over a
/// collatable type with no collation (runs/sort_raise.rs).
pub const RAISE_INDETERMINATE_COLL: u64 = 0;
/// Code 1 (PG `hashtext` / `hashbpchar`'s check, a hashed group's first row with the key not NULL): a hash key over a
/// collatable type with no collation (lowerv4 ops/agg/hash_raise.rs).
pub const RAISE_INDETERMINATE_HASH: u64 = 1;

/// PG's error `code` names, stashed as the statement's error: `ERROR` (below zero). Cold path only. Its IR symbol is
/// `contract::entry1::<M6Raise>`; `ctx` is unused.
pub struct M6Raise;
impl Host for M6Raise {
    type Args = (u64,);
    unsafe fn run(_ctx: *mut Ctx, (code,): (u64,)) -> u64 {
        raise(code) as u64
    }
}

/// D2849: `m6_raise_elems(ctx, datum)` (lowerv4 ops/subplan/hashed.rs, a hashed SubPlan keyed by an array whose element
/// hash reads its collation under none): PG's `hash_array` raises 42P22 ("could not determine which collation to use for
/// string hashing") at the first non-NULL element it hashes. The array varlena at `datum` (any header form) raises it when
/// it holds one: `ERROR`, stashed. 0 for an empty array or one of only NULL elements; a detoast failure is its own error.
pub struct M6RaiseElems;
impl Host for M6RaiseElems {
    type Args = (u64,);
    unsafe fn run(_ctx: *mut Ctx, (datum,): (u64,)) -> u64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        match gravity_fn::with_payload(datum as *const u8, any_element) {
            Ok(false) => 0,
            Ok(true) => raise(RAISE_INDETERMINATE_HASH) as u64,
            Err(e) => {
                gravity_fn::stash_error(e);
                gravity_abi::status::ERROR as u64
            }
        }
    }
}

/// Whether the array whose payload (past its varlena header: `ndim`, `dataoffset`, the element type, `dims`, `lbounds`,
/// then the null bitmap when `dataoffset` is not 0) is `p` holds a non-NULL element: some item (the product of `dims`), and
/// no bitmap or a set bit among the first `nitems` (array.h `ARR_NULLBITMAP`: a set bit is a non-NULL item). A payload too
/// short for its own header answers true, so the caller raises rather than reads past it.
pub fn any_element(p: &[u8]) -> bool {
    let word = |at: usize| p.get(at..at + 4).map(|b| i32::from_ne_bytes([b[0], b[1], b[2], b[3]]));
    let (Some(ndim), Some(dataoffset)) = (word(0), word(4)) else { return true };
    if ndim <= 0 {
        return false;
    }
    let mut n: u64 = 1;
    for d in 0..ndim as usize {
        let Some(dim) = word(12 + 4 * d) else { return true };
        n = n.saturating_mul(dim.max(0) as u64);
    }
    if n == 0 {
        return false;
    }
    if dataoffset == 0 {
        return true;
    }
    let at = 12 + 8 * ndim as usize;
    let full = (n / 8) as usize;
    let Some(bits) = p.get(at..at + full + usize::from(n % 8 != 0)) else { return true };
    bits[..full].iter().any(|&b| b != 0) || (n % 8 != 0 && bits[full] & ((1u8 << (n % 8)) - 1) != 0)
}

#[cold]
#[inline(never)]
fn raise(code: u64) -> i64 {
    let (msg, hint, state) = match code {
        RAISE_INDETERMINATE_COLL => ("could not determine which collation to use for string comparison", "Use the COLLATE clause to set the collation explicitly.", *b"42P22"),
        RAISE_INDETERMINATE_HASH => ("could not determine which collation to use for string hashing", "Use the COLLATE clause to set the collation explicitly.", *b"42P22"),
        _ => return crate::host_abi::bare(&format!("m6_raise: no error for code {code}")), // alloc: Cold
    };
    crate::contract::raise(msg, Some(hint), state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{entry1, sqlstate};
    use gravity_abi::status::ERROR;

    #[test]
    fn the_indeterminate_collation_raise_is_pgs_42p22_with_its_hint() {
        assert_eq!(unsafe { entry1::<M6Raise>(0, RAISE_INDETERMINATE_COLL) } as i64, ERROR);
        let e = gravity_fn::take_error().expect("m6_raise stashes the error");
        assert_eq!(e.sqlstate().0, sqlstate(*b"42P22"));
        assert_eq!(e.message(), "could not determine which collation to use for string comparison");
        assert_eq!(e.hint(), Some("Use the COLLATE clause to set the collation explicitly."));
        assert_eq!(unsafe { entry1::<M6Raise>(0, RAISE_INDETERMINATE_HASH) } as i64, ERROR);
        let e = gravity_fn::take_error().expect("m6_raise stashes the error");
        assert_eq!(e.sqlstate().0, sqlstate(*b"42P22"));
        assert_eq!(e.message(), "could not determine which collation to use for string hashing");
    }

    /// An array payload (past the varlena header) of `dims`, with a null bitmap `bits` when given.
    fn payload(dims: &[i32], bits: Option<&[u8]>) -> Vec<u8> {
        let mut p = Vec::new(); // alloc: Cold
        p.extend_from_slice(&(dims.len() as i32).to_ne_bytes());
        p.extend_from_slice(&(if bits.is_some() { 64i32 } else { 0 }).to_ne_bytes());
        p.extend_from_slice(&25u32.to_ne_bytes());
        for d in dims {
            p.extend_from_slice(&d.to_ne_bytes());
        }
        for _ in dims {
            p.extend_from_slice(&1i32.to_ne_bytes());
        }
        if let Some(b) = bits {
            p.extend_from_slice(b);
        }
        p.extend_from_slice(&[0; 8]);
        p
    }

    #[test]
    fn an_array_raises_only_at_a_non_null_element() {
        assert!(!any_element(&payload(&[], None)), "an empty array: no element");
        assert!(!any_element(&payload(&[0], None)), "a zero-length dimension: no element");
        assert!(any_element(&payload(&[1], None)), "one element, no bitmap: every element non-NULL");
        assert!(!any_element(&payload(&[1], Some(&[0]))), "ARRAY[NULL]");
        assert!(!any_element(&payload(&[3], Some(&[0b1111_1000]))), "three NULLs: bits past nitems do not count");
        assert!(any_element(&payload(&[3], Some(&[0b0000_0100]))), "NULL, NULL, then a non-NULL element");
        assert!(!any_element(&payload(&[9], Some(&[0, 0]))), "nine NULLs over two bitmap bytes");
        assert!(any_element(&payload(&[9], Some(&[0, 1]))), "the ninth element non-NULL");
        assert!(any_element(&payload(&[2, 2], Some(&[0b1000]))), "a 2x2 array whose last element is non-NULL");
        assert!(!any_element(&payload(&[2, 2], Some(&[0b1_0000]))), "a 2x2 array of NULLs (bit 4 is past its 4 items)");
        assert!(any_element(&[1, 0]), "a payload too short for its header raises");
    }

    #[test]
    fn the_elements_raise_is_pgs_42p22_and_a_null_only_array_is_quiet() {
        let mut quiet = vec![0u8; 4]; // alloc: Cold
        quiet.extend_from_slice(&payload(&[2], Some(&[0])));
        let len = (quiet.len() as u32) << 2;
        quiet[..4].copy_from_slice(&len.to_ne_bytes());
        assert_eq!(unsafe { entry1::<M6RaiseElems>(0, quiet.as_ptr() as u64) }, 0);
        let mut loud = vec![0u8; 4]; // alloc: Cold
        loud.extend_from_slice(&payload(&[2], Some(&[0b10])));
        let len = (loud.len() as u32) << 2;
        loud[..4].copy_from_slice(&len.to_ne_bytes());
        assert_eq!(unsafe { entry1::<M6RaiseElems>(0, loud.as_ptr() as u64) } as i64, ERROR);
        let e = gravity_fn::take_error().expect("m6_raise_elems stashes the error");
        assert_eq!(e.sqlstate().0, sqlstate(*b"42P22"));
        assert_eq!(e.message(), "could not determine which collation to use for string hashing");
    }
}
