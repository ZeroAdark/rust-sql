//! The per-row text key face (lower.md §15.11) on both backends: `siphash::text_key` over varlena images
//! with 1-byte and 4-byte headers of every length up to a few words and beyond, under random process
//! keys, equals `gravity_dict::fp::siphash128` word for word through TPDE and the interpreter; a
//! compressed inline image and a TOAST pointer set `bad` and hash nothing.
use gravity_expr::siphash::text_key;
use gravity_ir::builder::ProgramBuilder;
use gravity_ir::{Func, Type};

/// The ctx the test function reads: the datum pointer and the keys in, the words and the verdict out.
#[repr(C)]
struct Frame {
    datum: u64,
    k0: u64,
    k1: u64,
    lo: u64,
    hi: u64,
    bad: u64,
}

fn ir() -> (gravity_ir::Program, Func) {
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("text_key", &[Type::Ptr], &[Type::I64]);
    let mut b = pb.define(f);
    let ctx = b.params()[0];
    let datum = b.load(Type::Ptr, ctx, 0);
    let k0 = b.load(Type::I64, ctx, 8);
    let k1 = b.load(Type::I64, ctx, 16);
    let (lo, hi, bad) = text_key(&mut b, datum, k0, k1);
    b.store(ctx, 24, lo);
    b.store(ctx, 32, hi);
    b.store(ctx, 40, bad);
    let zero = b.iconst(Type::I64, 0);
    b.ret(&[zero]);
    drop(b);
    (pb.finish(), f)
}

/// A varlena image of `bytes`: the 1-byte header when it fits (`short`), else the 4-byte header; `flag`
/// ORed into the first byte (2: compressed; a `0x01` first byte alone: a TOAST pointer).
fn image(bytes: &[u8], short: bool, flag: u8) -> Vec<u8> {
    let mut v = Vec::new();
    if short {
        assert!(bytes.len() + 1 <= 127);
        v.push((((bytes.len() + 1) as u8) << 1 | 1) | flag);
    } else {
        v.extend_from_slice(&((((bytes.len() + 4) as u32) << 2) | flag as u32).to_le_bytes());
    }
    v.extend_from_slice(bytes);
    v.extend_from_slice(&[0xa5; 16]); // slack: the words loop never reads past `len`, but the buffer is real
    v
}

fn run(code: &gravity_program::Code, img: &[u8], k0: u64, k1: u64) -> (u64, u64, u64) {
    let mut fr = Frame { datum: img.as_ptr() as u64, k0, k1, lo: 0, hi: 0, bad: 0 };
    // SAFETY: the frame is the ctx the function reads; the image is live for the call.
    let r = unsafe { code.call((&mut fr as *mut Frame).cast()) };
    assert_eq!(r, 0);
    (fr.lo, fr.hi, fr.bad)
}

#[test]
fn the_text_key_equals_the_reference_on_both_backends() {
    let (ir, f) = ir();
    gravity_ir::verify::verify(&ir).unwrap_or_else(|e| panic!("{e:?}"));
    let codes = [("tpde", gravity_backend::mapped(&ir, f).unwrap_or_else(|e| panic!("tpde: {}", e.message()))), ("interp", gravity_backend::interp(ir.clone(), f).unwrap())];
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let lens: Vec<usize> = (0..=40).chain([63, 64, 100, 122, 123, 127, 128, 200, 1000, 4097]).collect();
    for &len in &lens {
        let bytes: Vec<u8> = (0..len).map(|_| next() as u8).collect();
        let (k0, k1) = (next(), next());
        let want = gravity_dict::fp::siphash128(k0, k1, &bytes);
        for short in [true, false] {
            if short && len + 1 > 127 {
                continue;
            }
            let img = image(&bytes, short, 0);
            for (arm, code) in &codes {
                let (lo, hi, bad) = run(code, &img, k0, k1);
                assert_eq!(bad, 0, "{arm} len {len} short {short}");
                assert_eq!([lo, hi], want, "{arm} len {len} short {short}");
            }
        }
    }
    // the empty string under both headers, keys zero: the reference's words
    for short in [true, false] {
        let img = image(&[], short, 0);
        for (arm, code) in &codes {
            assert_eq!(run(code, &img, 0, 0), (gravity_dict::fp::siphash128(0, 0, &[])[0], gravity_dict::fp::siphash128(0, 0, &[])[1], 0), "{arm} empty short {short}");
        }
    }
}

#[test]
fn a_compressed_or_external_image_is_bad_and_hashes_nothing() {
    let (ir, f) = ir();
    let codes = [("tpde", gravity_backend::mapped(&ir, f).unwrap_or_else(|e| panic!("tpde: {}", e.message()))), ("interp", gravity_backend::interp(ir.clone(), f).unwrap())];
    let compressed = image(b"twelve bytes", false, 2);
    let mut toast = vec![0x01u8, 18];
    toast.extend_from_slice(&[0u8; 24]);
    for (arm, code) in &codes {
        let (_, _, bad) = run(code, &compressed, 1, 2);
        assert_eq!(bad, 1, "{arm}: a compressed inline image");
        let (_, _, bad) = run(code, &toast, 1, 2);
        assert_eq!(bad, 1, "{arm}: a TOAST pointer");
    }
}

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
    fn munmap(addr: *mut u8, len: usize) -> i32;
    fn getpagesize() -> i32;
}

/// A 1-byte-header datum whose last byte is a mapping's last byte (the next page PROT_NONE): the header
/// read stays inside the image — no 4-byte load at a short datum — and the key is the reference's.
#[test]
fn a_short_datum_at_a_page_edge_reads_no_byte_past_its_image() {
    let (ir, f) = ir();
    let codes = [("tpde", gravity_backend::mapped(&ir, f).unwrap_or_else(|e| panic!("tpde: {}", e.message()))), ("interp", gravity_backend::interp(ir.clone(), f).unwrap())];
    // SAFETY: a private anonymous two-page mapping, the second page made inaccessible; unmapped below.
    let (base, page) = unsafe {
        let page = getpagesize() as usize;
        let map_anon = if cfg!(target_os = "macos") { 0x1000 } else { 0x20 };
        let base = mmap(core::ptr::null_mut(), 2 * page, 3, 0x02 | map_anon, -1, 0);
        assert!(!base.is_null() && base as isize != -1, "mmap");
        assert_eq!(mprotect(base.add(page), page, 0), 0, "mprotect");
        (base, page)
    };
    for len in 0..=2usize {
        let bytes: Vec<u8> = (0..len).map(|i| 0x61 + i as u8).collect();
        let n = len + 1;
        // SAFETY: the image's `n` bytes end at the first page's last byte.
        let img = unsafe {
            let at = base.add(page - n);
            *at = ((n as u8) << 1) | 1;
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), at.add(1), len);
            core::slice::from_raw_parts(at, n)
        };
        let want = gravity_dict::fp::siphash128(7, 9, &bytes);
        for (arm, code) in &codes {
            assert_eq!(run(code, img, 7, 9), (want[0], want[1], 0), "{arm} len {len}");
        }
    }
    // SAFETY: the mapping made above; no image outlives it.
    assert_eq!(unsafe { munmap(base, 2 * page) }, 0);
}
