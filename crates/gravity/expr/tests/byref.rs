//! The by-reference output column face (lower.md §15.13): `byref::sizes` / `fits` / `oversize` / `store` in an
//! IR function around a fake out block, run through the interpreter and TPDE. The images: a 1-byte-header
//! varlena, a 4-byte-header varlena, an on-disk toast pointer, a fixed-width by-reference type, a NULL.

use gravity_abi::ctx::Out;
use gravity_expr::byref::{self, ByRef};
use gravity_ir::builder::ProgramBuilder;
use gravity_ir::{Type, Value};

const COLS: &[i16] = &[-1, -1, -1, 16, -1];

fn build() -> (gravity_ir::Program, gravity_ir::Func) {
    build_for(COLS)
}

/// `f(frame)`: frame = [out, vp, np, (value, valid) × cols]; 2 = oversize, 1 = does not fit, 0 = stored.
fn build_for(typlens: &[i16]) -> (gravity_ir::Program, gravity_ir::Func) {
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("byref_row", &[Type::Ptr], &[Type::I64]);
    let mut b = pb.define(f);
    let frame = b.params()[0];
    let o = b.load(Type::Ptr, frame, 0);
    let vp = b.load(Type::Ptr, frame, 8);
    let np = b.load(Type::Ptr, frame, 16);
    let zero = b.iconst(Type::I64, 0);
    let cols: Vec<ByRef> = typlens
        .iter()
        .enumerate()
        .map(|(k, &typlen)| {
            let value: Value = b.load(Type::I64, frame, 24 + 16 * k as i32);
            let valid_w = b.load(Type::I64, frame, 32 + 16 * k as i32);
            let valid = b.icmp(gravity_ir::IntCond::Ne, valid_w, zero);
            ByRef { value, valid, typlen }
        })
        .collect();
    let (sizes, total) = byref::sizes(&mut b, &cols);
    let l_over = b.create_block(&[]);
    let l_check = b.create_block(&[]);
    let l_no = b.create_block(&[]);
    let l_store = b.create_block(&[]);
    let over = byref::oversize(&mut b, o, total);
    b.cond_br(over, (l_over, &[]), (l_check, &[]));
    b.switch_to(l_over);
    let two = b.iconst(Type::I64, 2);
    b.ret(&[two]);
    b.switch_to(l_check);
    let fits = byref::fits(&mut b, o, total);
    b.cond_br(fits, (l_store, &[]), (l_no, &[]));
    b.switch_to(l_no);
    let one = b.iconst(Type::I64, 1);
    b.ret(&[one]);
    b.switch_to(l_store);
    for (k, c) in cols.iter().enumerate() {
        byref::store(&mut b, o, vp, np, k as i32, *c, sizes[k]);
    }
    b.ret(&[zero]);
    drop(b);
    let ir = pb.finish();
    gravity_ir::verify::verify(&ir).unwrap();
    (ir, f)
}

fn images() -> Vec<Option<Vec<u8>>> {
    let mut short = vec![((6u8) << 1) | 1];
    short.extend_from_slice(b"hello");
    let mut long = (300u32 << 2).to_le_bytes().to_vec();
    long.extend((0..296).map(|i| (i * 7 % 251) as u8));
    let mut toast = vec![1u8, 18];
    toast.extend_from_slice(&[0xAB; 16]);
    let fixed = (1u8..=16).collect::<Vec<u8>>();
    vec![Some(short), Some(long), Some(toast), Some(fixed), None]
}

fn run(code: gravity_program::Code, region_cap: usize) -> (i64, Vec<u64>, Vec<u8>, Vec<u8>, i64) {
    let imgs = images();
    // each image in its own 8-aligned chunk (the word copy over-reads at most 7 bytes)
    let chunks: Vec<Vec<u64>> = imgs.iter().map(|i| vec![0u64; i.as_ref().map_or(1, |v| v.len().div_ceil(8) + 1)]).collect();
    let mut chunks = chunks;
    for (c, i) in chunks.iter_mut().zip(imgs.iter()) {
        if let Some(v) = i {
            unsafe { core::ptr::copy_nonoverlapping(v.as_ptr(), c.as_mut_ptr() as *mut u8, v.len()) };
        }
    }
    let n = COLS.len();
    let mut vals = vec![0u64; n];
    let mut nulls = vec![0xEEu8; n];
    let mut region = vec![0u8; region_cap.max(1)];
    let mut o = Out { values: vals.as_mut_ptr(), isnull: nulls.as_mut_ptr(), count: 0, cap: 64, region: region.as_mut_ptr(), region_cap: region_cap as i64, region_len: 0 };
    let mut frame = vec![0u64; 3 + 2 * n];
    frame[0] = &mut o as *mut Out as u64;
    frame[1] = vals.as_mut_ptr() as u64;
    frame[2] = nulls.as_mut_ptr() as u64;
    for k in 0..n {
        frame[3 + 2 * k] = chunks[k].as_ptr() as u64;
        frame[4 + 2 * k] = imgs[k].is_some() as u64;
    }
    // SAFETY: the frame's pointers are live for the call; the region is `region_cap` bytes.
    let r = unsafe { code.call(frame.as_mut_ptr().cast()) };
    (r, vals, nulls, region, o.region_len)
}

fn check(code: gravity_program::Code) {
    let imgs = images();
    let (r, vals, nulls, region, len) = run(code, 4096);
    assert_eq!(r, 0);
    let base = region.as_ptr() as u64;
    let mut at = 0usize;
    for (k, img) in imgs.iter().enumerate() {
        match img {
            None => {
                assert_eq!((vals[k], nulls[k]), (0, 1), "column {k}");
            }
            Some(v) => {
                assert_eq!(nulls[k], 0, "column {k}");
                assert_eq!(vals[k], base + at as u64, "column {k} points at its copy");
                assert_eq!(&region[at..at + v.len()], &v[..], "column {k}'s image");
                at += v.len().next_multiple_of(8);
            }
        }
    }
    assert_eq!(len as usize, at, "region_len is the aligned sum: {at}");
    assert_eq!(at, 8 + 304 + 24 + 16);
}

#[test]
fn the_images_are_copied_into_the_region_and_the_datums_point_at_the_copies() {
    let (ir, f) = build();
    check(gravity_backend::mapped(&ir, f).unwrap());
    check(gravity_backend::interp(ir, f).unwrap());
}

#[test]
fn a_row_that_does_not_fit_is_reported_before_any_store_and_an_oversize_row_by_name() {
    let (ir, f) = build();
    for code in [gravity_backend::mapped(&ir, f).unwrap(), gravity_backend::interp(ir.clone(), f).unwrap()] {
        let (r, _, nulls, _, len) = run(code, 8 + 304 + 24 + 16 + 8);
        assert_eq!((r, len), (0, 352));
        assert_eq!(nulls[0], 0);
    }
    let (r, _, nulls, _, len) = run(gravity_backend::interp(ir, f).unwrap(), 351);
    assert_eq!((r, len), (2, 0), "352 bytes exceed a 351-byte region: refused by name, nothing stored");
    assert_eq!(nulls[0], 0xEE, "no null byte written");
}

#[test]
fn a_full_block_refuses_by_name_only_past_the_whole_region() {
    let (ir, f) = build();
    // the region holds the row once; a second row would not fit but is not oversize
    let (r, _, _, _, len) = run(gravity_backend::interp(ir.clone(), f).unwrap(), 600);
    assert_eq!((r, len), (0, 352));
    let (r, _, _, _, len) = run_twice(gravity_backend::interp(ir, f).unwrap(), 600);
    assert_eq!((r, len), (1, 352), "the second row does not fit behind the first: the block is to be pushed");
}

/// The row emitted once, then the same row again into the same block.
fn run_twice(code: gravity_program::Code, region_cap: usize) -> (i64, Vec<u64>, Vec<u8>, Vec<u8>, i64) {
    let imgs = images();
    let mut chunks: Vec<Vec<u64>> = imgs.iter().map(|i| vec![0u64; i.as_ref().map_or(1, |v| v.len().div_ceil(8) + 1)]).collect();
    for (c, i) in chunks.iter_mut().zip(imgs.iter()) {
        if let Some(v) = i {
            unsafe { core::ptr::copy_nonoverlapping(v.as_ptr(), c.as_mut_ptr() as *mut u8, v.len()) };
        }
    }
    let n = COLS.len();
    let mut vals = vec![0u64; 2 * n];
    let mut nulls = vec![0xEEu8; 2 * n];
    let mut region = vec![0u8; region_cap];
    let mut o = Out { values: vals.as_mut_ptr(), isnull: nulls.as_mut_ptr(), count: 0, cap: 64, region: region.as_mut_ptr(), region_cap: region_cap as i64, region_len: 0 };
    let mut frame = vec![0u64; 3 + 2 * n];
    frame[0] = &mut o as *mut Out as u64;
    for k in 0..n {
        frame[3 + 2 * k] = chunks[k].as_ptr() as u64;
        frame[4 + 2 * k] = imgs[k].is_some() as u64;
    }
    let mut r = 0;
    for row in 0..2 {
        frame[1] = unsafe { vals.as_mut_ptr().add(row * n) } as u64;
        frame[2] = unsafe { nulls.as_mut_ptr().add(row * n) } as u64;
        // SAFETY: as `run`.
        r = unsafe { code.call(frame.as_mut_ptr().cast()) };
    }
    (r, vals, nulls, region, o.region_len)
}

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
    fn munmap(addr: *mut u8, len: usize) -> i32;
    fn getpagesize() -> i32;
}

/// D3661: an image whose last byte is a mapping's last byte (the next page PROT_NONE), as a heap page's first tuple ends
/// in the pool's last buffer: a 1-byte-header varlena of each length 1..=40, a 4-byte-header one of 9..=40, fixed widths
/// 6, 12 and 16. The copy reads no byte past the image (that read is a SIGSEGV), the region holds the image, its last
/// word's bytes above it are zero, and the datum is the copy's address.
#[test]
fn an_image_ending_its_mapping_is_copied_without_reading_past_it() {
    // SAFETY: a private anonymous two-page mapping, the second page made inaccessible; unmapped below.
    let (base, page) = unsafe {
        let page = getpagesize() as usize;
        let map_anon = if cfg!(target_os = "macos") { 0x1000 } else { 0x20 };
        let base = mmap(core::ptr::null_mut(), 2 * page, 3, 0x02 | map_anon, -1, 0);
        assert!(!base.is_null() && base as isize != -1, "mmap");
        assert_eq!(mprotect(base.add(page), page, 0), 0, "mprotect");
        (base, page)
    };
    let mut cases: Vec<(i16, Vec<u8>)> = Vec::new();
    for n in 1..=40usize {
        let mut v = vec![((n as u8) << 1) | 1];
        v.extend((1..n).map(|i| 0x40 + i as u8));
        cases.push((-1, v));
    }
    for n in 9..=40usize {
        let mut v = ((n as u32) << 2).to_le_bytes().to_vec();
        v.extend((4..n).map(|i| 0x80 + i as u8));
        cases.push((-1, v));
    }
    for n in [6usize, 12, 16] {
        cases.push((n as i16, (1..=n as u8).collect()));
    }
    for typlen in [-1i16, 6, 12, 16] {
        let (ir, f) = build_for(&[typlen]);
        let codes = [("tpde", gravity_backend::mapped(&ir, f).unwrap()), ("interp", gravity_backend::interp(ir, f).unwrap())];
        for (_, img) in cases.iter().filter(|(t, _)| *t == typlen) {
            // SAFETY: the image's bytes end at the first page's last byte.
            let at = unsafe {
                let at = base.add(page - img.len());
                core::ptr::copy_nonoverlapping(img.as_ptr(), at, img.len());
                at
            };
            for (arm, code) in &codes {
                let mut vals = [0u64; 1];
                let mut nulls = [0xEEu8; 1];
                let mut region = vec![0xEEu8; 64];
                let mut o = Out { values: vals.as_mut_ptr(), isnull: nulls.as_mut_ptr(), count: 0, cap: 64, region: region.as_mut_ptr(), region_cap: 64, region_len: 0 };
                let mut frame = [&mut o as *mut Out as u64, vals.as_mut_ptr() as u64, nulls.as_mut_ptr() as u64, at as u64, 1];
                // SAFETY: the frame's pointers are live for the call; the region is 64 bytes, the image at most 40.
                let r = unsafe { code.call(frame.as_mut_ptr().cast()) };
                let size = img.len().next_multiple_of(8);
                let what = format!("{arm} typlen {typlen} len {}", img.len());
                assert_eq!((r, o.region_len, nulls[0]), (0, size as i64, 0), "{what}");
                assert_eq!(vals[0], region.as_ptr() as u64, "{what}: the datum is the copy");
                assert_eq!(&region[..img.len()], &img[..], "{what}: the image");
                assert!(region[img.len()..size].iter().all(|&b| b == 0), "{what}: the last word's bytes above the image are zero");
            }
        }
    }
    // SAFETY: the mapping made above; no image outlives it.
    assert_eq!(unsafe { munmap(base, 2 * page) }, 0);
}
