//! `nest.subplan`'s ARRAY sublink accumulator (RT-2b, V4R-428; a #[path] child of m8_sub.rs; the face agreed with M8-SUB):
//! a SubResult proto whose `arg` bits 0-31 name the element type (0 = the copy-only object) keeps one hosts `ArrayAcc` per
//! calling lane (`ctx.worker_ordinal`: pool workers and D1036's nested lanes), each made and touched only by that lane's
//! thread. `m8_array_begin` / `_push` / `_end` are PG's `initArrayResultAny` / `accumArrayResultAny` / `makeArrayResultAny`
//! (ExecScanSubPlan's ARRAY_SUBLINK): the array is valid until the same lane's next begin (PG frees `curArray` per call).
//! Created by M8-RUNTIME.
use core::cell::UnsafeCell;

use gravity_hosts::ops::subplan::stores::{elem_shape, ArrayAcc, ElemShape};
use types_error::PgResult;

use crate::ops::subplan::hash::LANES;

/// The object's ARRAY half: the element shape (none for the copy-only form) and one lazily made accumulator per lane.
pub struct Arrays {
    pub(super) elem: Option<ElemShape>,
    pub(super) lanes: Box<[UnsafeCell<Option<ArrayAcc>>]>, // alloc: Exec
    /// D1421: the lanes' bytes charged to the grant so far
    pub(super) charged: core::cell::Cell<u64>,
}

impl Arrays {

    /// The proto's `arg`: 0 = copy-only; else the element type oid in bits 0-31 (upper bits reserved at 0).
    pub fn of(arg: u64) -> PgResult<Arrays> {
        if arg >> 32 != 0 {
            return Err(crate::glue::internal(&format!("gravity: nest.subplan.result: arg {arg:#x} has bits past the element type"))); // alloc: Cold
        }
        Ok(Arrays::with((arg != 0).then(|| elem_shape(arg as u32)).transpose()?))
    }

    pub fn with(elem: Option<ElemShape>) -> Arrays {
        let n = if elem.is_some() { LANES } else { 0 };
        Arrays { elem, lanes: (0..n).map(|_| UnsafeCell::new(None)).collect(), charged: core::cell::Cell::new(0) } // alloc: Exec
    }

}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use crate::ops::subplan::hash::new_result;
    use super::*;
    use gravity_abi::status::ERROR;
    use gravity_abi::Ctx;
    use gravity_kernels::ops::shared::object::entries as abi;

    const INT4: ElemShape = ElemShape { oid: 23, typlen: 4, byval: true, align: b'i', sub: None };
    const TEXT: ElemShape = ElemShape { oid: 25, typlen: -1, byval: false, align: b'i', sub: None };
    const INT8: ElemShape = ElemShape { oid: 20, typlen: 8, byval: true, align: b'd', sub: None };

    fn ctx(worker: u8) -> Ctx {
        // SAFETY: Ctx is plain data; zero is its idle state.
        let mut c: Ctx = unsafe { core::mem::zeroed() };
        c.worker_ordinal = worker as _;
        c
    }

    fn h(head: &abi::Head) -> u64 {
        head as *const abi::Head as u64
    }

    fn text(s: &[u8]) -> Vec<u8> {
        let mut v = ((s.len() as u32 + 4) << 2).to_le_bytes().to_vec(); // alloc: Cold
        v.extend_from_slice(s);
        v
    }

    /// PG's ArrayType as read back: (ndim, dims, lbounds, elemtype, per-element Some(bytes) / None).
    #[allow(clippy::type_complexity)]
    fn read(p: i64, e: &ElemShape) -> (i32, Vec<i32>, Vec<i32>, u32, Vec<Option<Vec<u8>>>) {
        assert!(p > 0, "end returned {p}");
        // SAFETY: `p` is the accumulator's live array (valid until the lane's next begin).
        unsafe {
            let base = p as *const u8;
            let rd = |o: usize| i32::from_le_bytes(*(base.add(o) as *const [u8; 4]));
            let (ndim, dataoff, elemtype) = (rd(4), rd(8), rd(12) as u32);
            let dims: Vec<i32> = (0..ndim as usize).map(|i| rd(16 + 4 * i)).collect();
            let lbs: Vec<i32> = (0..ndim as usize).map(|i| rd(16 + 4 * ndim as usize + 4 * i)).collect();
            let n = dims.first().copied().unwrap_or(0) as usize;
            let bitmap = base.add(16 + 8 * ndim as usize);
            let hdr = if dataoff != 0 { dataoff as usize } else { (16 + 8 * ndim as usize + 7) & !7 };
            let mut at = hdr;
            let mut out = vec![]; // alloc: Cold
            for i in 0..n {
                if dataoff != 0 && *bitmap.add(i / 8) & (1 << (i % 8)) == 0 {
                    out.push(None);
                    continue;
                }
                let al = match e.align { b'd' => 8, b'i' => 4, b's' => 2, _ => 1 };
                let len = if e.typlen > 0 {
                    at = (at + al - 1) & !(al - 1);
                    e.typlen as usize
                } else {
                    let b0 = *base.add(at);
                    if b0 & 1 == 1 {
                        (b0 >> 1) as usize // short header: no alignment
                    } else {
                        at = (at + al - 1) & !(al - 1);
                        (u32::from_le_bytes(*(base.add(at) as *const [u8; 4])) >> 2) as usize
                    }
                };
                out.push(Some(std::slice::from_raw_parts(base.add(at), len).to_vec()));
                at += len;
            }
            (ndim, dims, lbs, elemtype, out)
        }
    }

    /// A varlena element's payload, whichever header it carries.
    fn body(v: &[u8]) -> &[u8] {
        if v[0] & 1 == 1 { &v[1..] } else { &v[4..] }
    }

    fn arr(e: ElemShape) -> Box<crate::ops::subplan::hash::ResultObj> {
        new_result(Arrays::with(Some(e)))
    }

    #[test]
    fn by_value_elements_with_nulls_lower_bound_one() {
        let o = arr(INT4);
        let mut c = ctx(0);
        unsafe {
            assert_eq!(abi::m8_array_begin(&mut c, h(&o.head)), 0);
            for (v, n) in [(7u64, 0u64), (0, 1), (u32::MAX as u64, 0)] {
                assert_eq!(abi::m8_array_push(&mut c, h(&o.head), v, n), 0);
            }
            let (ndim, dims, lbs, t, els) = read(abi::m8_array_end(&mut c, h(&o.head)), &INT4);
            assert_eq!((ndim, dims, lbs, t), (1, vec![3], vec![1], 23)); // alloc: Cold
            assert_eq!(els, vec![Some(7i32.to_le_bytes().to_vec()), None, Some((-1i32).to_le_bytes().to_vec())]); // alloc: Cold
        }
    }

    #[test]
    fn no_rows_is_the_empty_array() {
        let o = arr(INT8);
        let mut c = ctx(1);
        unsafe {
            assert_eq!(abi::m8_array_begin(&mut c, h(&o.head)), 0);
            let p = abi::m8_array_end(&mut c, h(&o.head));
            let (ndim, dims, lbs, t, els) = read(p, &INT8);
            assert_eq!((ndim, dims, lbs, t, els.len()), (0, vec![], vec![], 20, 0)); // alloc: Cold
            assert_eq!(i32::from_le_bytes(*(p as *const [u8; 4])) >> 2, 16, "PG's empty array is its 16-byte header");
        }
    }

    #[test]
    fn by_reference_elements_are_copied_including_one_past_a_page() {
        let o = arr(TEXT);
        let mut c = ctx(3);
        let big: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect(); // alloc: Cold
        let mut vals = vec![text(b"a"), text(b""), text(&big), text(b"zz")]; // alloc: Cold
        unsafe {
            assert_eq!(abi::m8_array_begin(&mut c, h(&o.head)), 0);
            for v in &vals {
                assert_eq!(abi::m8_array_push(&mut c, h(&o.head), v.as_ptr() as u64, 0), 0);
            }
            assert_eq!(abi::m8_array_push(&mut c, h(&o.head), 0, 1), 0);
            // the inner's rows move on: the accumulator holds its own copies
            for v in vals.iter_mut() {
                v.iter_mut().skip(4).for_each(|b| *b = 0xEE);
            }
            let (_, dims, lbs, _, els) = read(abi::m8_array_end(&mut c, h(&o.head)), &TEXT);
            assert_eq!((dims, lbs), (vec![5], vec![1])); // alloc: Cold
            let want = [b"a".to_vec(), vec![], big.clone(), b"zz".to_vec()]; // alloc: Cold
            for (i, w) in want.iter().enumerate() {
                assert_eq!(body(els[i].as_ref().unwrap()), &w[..], "element {i}");
            }
            assert_eq!(els[4], None);
        }
    }

    #[test]
    fn each_call_resets_and_the_previous_array_holds_until_the_next_begin() {
        // by-reference elements: their copies are the context's (freed only by the reset), so a missing reset grows it
        let o = arr(TEXT);
        let mut c = ctx(2);
        let (mut early, mut late) = (0usize, 0usize);
        let want = |call: u64, k: u64| format!("call {call:05} element {k}").into_bytes(); // alloc: Cold
        unsafe {
            let mut prev: Option<(i64, u64)> = None;
            for call in 0..10_000u64 {
                // the previous call's array is intact up to this lane's next begin
                if let Some((p, pc)) = prev {
                    let (_, _, _, _, els) = read(p, &TEXT);
                    assert!(els.iter().enumerate().all(|(k, e)| body(e.as_ref().unwrap()) == &want(pc, k as u64)[..]));
                }
                assert_eq!(abi::m8_array_begin(&mut c, h(&o.head)), 0);
                for k in 0..(call % 7) {
                    let v = text(&want(call, k));
                    assert_eq!(abi::m8_array_push(&mut c, h(&o.head), v.as_ptr() as u64, 0), 0);
                }
                let p = abi::m8_array_end(&mut c, h(&o.head));
                let (_, dims, _, _, _) = read(p, &TEXT);
                assert_eq!(dims.first().copied().unwrap_or(0) as u64, call % 7);
                prev = Some((p, call));
                let used = (*o.arrays.lanes[2].get()).as_ref().unwrap().bytes();
                if call < 100 {
                    early = early.max(used);
                } else {
                    late = late.max(used);
                }
            }
        }
        assert!(early > 0 && late <= early, "the lane's context stays flat: {late} bytes late vs {early} in the first 100 calls");
    }

    #[test]
    fn eight_lanes_on_eight_threads_keep_their_own_arrays() {
        let o = arr(TEXT);
        let at = h(&o.head);
        std::thread::scope(|s| {
            for w in 0..8u8 {
                s.spawn(move || {
                    let mut c = ctx(w);
                    let vals: Vec<Vec<u8>> = (0..250_000u32 / 8).map(|i| text(format!("w{w}-{i}").as_bytes())).collect(); // alloc: Cold
                    for round in 0..4 {
                        unsafe {
                            assert_eq!(abi::m8_array_begin(&mut c, at), 0);
                            for (i, v) in vals.iter().enumerate() {
                                let null = (i + round) % 97 == 0;
                                assert_eq!(abi::m8_array_push(&mut c, at, v.as_ptr() as u64, null as u64), 0);
                            }
                            let (_, dims, lbs, _, els) = read(abi::m8_array_end(&mut c, at), &TEXT);
                            assert_eq!((dims, lbs), (vec![vals.len() as i32], vec![1])); // alloc: Cold
                            for (i, e) in els.iter().enumerate() {
                                if (i + round) % 97 == 0 {
                                    assert_eq!(e, &None);
                                } else {
                                    assert_eq!(body(e.as_ref().unwrap()), format!("w{w}-{i}").as_bytes(), "lane {w} element {i}"); // alloc: Cold
                                }
                            }
                        }
                    }
                });
            }
        });
    }

    #[test]
    fn misuse_is_an_error_by_name() {
        let copy_only = new_result(Arrays::with(None));
        let o = arr(INT4);
        let mut c = ctx(0);
        unsafe {
            assert_eq!(abi::m8_array_begin(&mut c, h(&copy_only.head)), ERROR);
            assert_eq!(abi::m8_array_push(&mut c, h(&o.head), 1, 0), ERROR, "a push before any begin");
            assert_eq!(abi::m8_array_end(&mut c, h(&o.head)), ERROR, "an end before any begin");
        }
        assert!(Arrays::of(1 << 32).is_err(), "bits past the element type");
        assert!(Arrays::of(0).map(|a| a.elem.is_none()).unwrap_or(false), "arg 0 = copy-only");
    }
}
