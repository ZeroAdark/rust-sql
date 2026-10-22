mod common;

use common::{Buf, VecDict};
use gravity_dict::like::FAULT_TRAILING_ESCAPE;
use gravity_dict::{Bits, Deferred, Derived, Fault, Matcher, Mode, Region, View, I64};

const WORDS: &[&str] = &["alpha", "beta", "x-ray", "gamma", "xylo", "delta"];

fn bits(r: &mut Region) -> (VecDict, Bits) {
    let d = VecDict::of(WORDS);
    let t = Bits::build(&d, r, &mut |v| {
        let b = unsafe { v.bytes() };
        if b.starts_with(b"x") {
            Err(Fault(7))
        } else {
            Ok(b.len() == 5)
        }
    })
    .unwrap();
    (d, t)
}

#[test]
fn an_unselected_fault_is_silent_and_a_selected_one_raises_at_its_row() {
    let mut buf = Buf::new(4096);
    let mut r = buf.region();
    let (d, t) = bits(&mut r);
    assert!(t.has_faults());
    let (xray, xylo) = (d.code_of("x-ray"), d.code_of("xylo"));
    assert!(unsafe { t.faulted(xray) } && unsafe { t.faulted(xylo) } && !unsafe { t.faulted(0) });
    assert!(!unsafe { t.get(xray) }, "a faulting entry reads false");
    let clean = [0u32, 1, 3, 5, 0, 1];
    let mut out = [0u64];
    unsafe { t.gather_mask(clean.as_ptr(), core::ptr::null(), 6, true, &mut out) }.unwrap();
    assert_eq!(out[0], 0b011101, "alpha beta gamma delta alpha beta");
    let dirty = [0u32, 1, xylo, 3, xray, 5];
    assert_eq!(unsafe { t.gather_mask(dirty.as_ptr(), core::ptr::null(), 6, true, &mut out) }, Err(Deferred { row: 2, code: xylo }));
    let valid = [!0u64 & !(1 << 2)];
    assert_eq!(unsafe { t.gather_mask(dirty.as_ptr(), valid.as_ptr(), 6, true, &mut out) }, Err(Deferred { row: 4, code: xray }), "a null row never raises; the next selected fault does");
    let valid = [!0u64 & !(1 << 2) & !(1 << 4)];
    unsafe { t.gather_mask(dirty.as_ptr(), valid.as_ptr(), 6, true, &mut out) }.unwrap();
    assert_eq!(out[0], 0b101001 & valid[0]);
    let mut mask = [0b000011u64];
    unsafe { t.gather_mask(dirty.as_ptr(), core::ptr::null(), 6, false, &mut mask) }.unwrap();
    assert_eq!(mask[0], 0b000001, "rows an earlier predicate dropped are short-circuited faults");
    let tail = [xylo; 70];
    let mut two = [0u64; 2];
    assert_eq!(unsafe { t.gather_mask(tail.as_ptr(), core::ptr::null(), 70, false, &mut two) }, Ok(()), "a cleared mask selects nothing");
    two = [0, 1 << 3];
    assert_eq!(unsafe { t.gather_mask(tail.as_ptr(), core::ptr::null(), 70, false, &mut two) }, Err(Deferred { row: 67, code: xylo }));
}

#[test]
fn value_tables_defer_the_same_way() {
    let mut buf = Buf::new(8192);
    let mut r = buf.region();
    let d = VecDict::of(WORDS);
    let faults = |v: View| unsafe { v.bytes() }.starts_with(b"xy");
    let t = I64::build(&d, &mut r, &mut |v| if faults(v) { Err(Fault(1)) } else { Ok(v.len as i64) }).unwrap();
    let xylo = d.code_of("xylo");
    let codes = [0u32, xylo, 1];
    let mut out = [0i64; 3];
    assert_eq!(unsafe { t.gather(codes.as_ptr(), [0b101u64].as_ptr(), 3, out.as_mut_ptr()) }, Ok(()));
    assert_eq!(out, [5, 0, 4]);
    assert_eq!(unsafe { t.gather(codes.as_ptr(), core::ptr::null(), 3, out.as_mut_ptr()) }, Err(Deferred { row: 1, code: xylo }));
    let dv = Derived::build(&d, &mut r, &mut |v, _| if faults(v) { Err(Fault(1).into()) } else { Ok(View::of(&unsafe { v.bytes() }[..1])) }).unwrap();
    assert_eq!(unsafe { dv.canonical(xylo) }, u32::MAX);
    assert_eq!(dv.ndistinct(), 5, "a b x g d; the faulting xylo is not a value");
    let mut c = [0u32; 3];
    assert_eq!(unsafe { dv.gather_canonical(codes.as_ptr(), core::ptr::null(), 3, c.as_mut_ptr()) }, Err(Deferred { row: 1, code: xylo }));
    assert_eq!(unsafe { dv.gather_canonical(codes.as_ptr(), [0b101u64].as_ptr(), 3, c.as_mut_ptr()) }, Ok(()));
}

#[test]
fn a_trailing_escape_is_a_per_entry_fault_as_in_postgresql() {
    let mut buf = Buf::new(4096);
    let mut r = buf.region();
    let d = VecDict::of(&["alpha", "alp", "beta", "al"]);
    let m = Matcher::new(b"al\\", Mode::Like, &mut r).unwrap();
    assert_eq!(m.class(), gravity_dict::like::Class::General);
    assert_eq!(m.matches(b"beta"), Ok(false), "the match fails before the escape is reached");
    assert_eq!(m.matches(b"al"), Ok(false), "the text ends before the escape");
    assert_eq!(m.matches(b"alp"), Err(FAULT_TRAILING_ESCAPE));
    let mut r2 = Buf::new(4096);
    let mut r2 = r2.region();
    let t = m.build_bits(&d, &mut r2).unwrap();
    assert_eq!((unsafe { t.faulted(0) }, unsafe { t.faulted(1) }, unsafe { t.faulted(2) }, unsafe { t.faulted(3) }), (true, true, false, false));
    let mut out = [0u64];
    assert_eq!(unsafe { t.gather_mask([2u32, 3, 2].as_ptr(), core::ptr::null(), 3, true, &mut out) }, Ok(()));
    assert_eq!(unsafe { t.gather_mask([2u32, 3, 1].as_ptr(), core::ptr::null(), 3, true, &mut out) }, Err(Deferred { row: 2, code: 1 }));
}
