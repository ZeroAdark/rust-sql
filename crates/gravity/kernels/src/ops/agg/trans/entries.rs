//! The agg family's host entries for varlena keys and values: the detoast of a compressed or external key
//! (`m6_detoast`), the fingerprint seed (`m6_fp_seed`), a numeric's exact image (`m6_num_fp`), a hosted key's canonical
//! image (`m6_hosted_fp`), an integral extract's group key word (`m6_extract_word`) and the ICU sort-key prefix
//! (`m6_coll_prefix`). Each is listed in the `hosts:` rows of the lowerv4 file that calls it. The entries are written in
//! the raw child as `contract::host` functions, each body under the one landing pad (PG's error stashed, `ERROR`
//! returned), and re-exported here, so the lowerers' addresses and the IR's host names are unchanged. An entry whose body
//! needs only `gravity_fn` / `gravity_dict` lives in this crate (lowerv4 reaches it, not the hosts crate); what needs the
//! hosts crate's dependencies is reached through a handle word (hosted.rs's `AggHead`).
mod raw;

pub use raw::{m6_coll_prefix, m6_detoast, m6_extract_word, m6_fp_seed, m6_hosted_fp, m6_num_fp};

use gravity_abi::status::ERROR;
use gravity_fn::PgError;

use crate::contract::sqlstate;

/// The canonical image's classes (its first byte): a finite nonzero value is followed by its sign, weight and digits.
const NUM_ZERO: u8 = 0;
const NUM_FINITE: u8 = 1;
const NUM_NAN: u8 = 2;
const NUM_PINF: u8 = 3;
const NUM_NINF: u8 = 4;

/// The Fp128 of numeric payload `p`'s canonical image (numeric.h's two header forms: `NUMERIC_SHORT` 0x8000 with the sign
/// 0x2000, the weight's sign 0x40 and its low 6 bits, the digits after the 2-byte header; the long form's sign in
/// 0xC000 (0x4000 negative) and an i16 weight, the digits after 4 bytes; `NUMERIC_SPECIAL` 0xC000 with 0xF000 naming
/// NaN / +Inf / -Inf). E-kern: an image past the 256-byte stack buffer (a value past 126 base-10000 digits) is a
/// `ChargedVec` on the breaker slot `ctx`'s pipeline produces into, else the statement's (`OpArena::slot_owner_kind`),
/// asked only then; `ctx` null (a test): uncharged.
fn num_fp(ctx: *mut gravity_abi::Ctx, p: &[u8]) -> Result<[u64; 2], Box<PgError>> {
    let bad = || Box::new(PgError::error(format!("gravity: a numeric image of {} bytes that is no numeric", p.len()))); // alloc: Cold
    let keys = gravity_dict::fp::keys().ok_or_else(|| Box::new(PgError::error("gravity: a fingerprint seed before it is installed")))?; // alloc: Cold
    let h = u16::from_ne_bytes(*p.first_chunk::<2>().ok_or_else(bad)?);
    let (neg, weight, digits) = match h & 0xC000 {
        0xC000 => {
            let class = match h & 0xF000 {
                0xC000 => NUM_NAN,
                0xD000 => NUM_PINF,
                0xF000 => NUM_NINF,
                _ => return Err(bad()),
            };
            return Ok(keys.fp_bytes(&[class]));
        }
        0x8000 => (h & 0x2000 != 0, (h & 0x3F) as i16 | if h & 0x40 != 0 { !0x3F } else { 0 }, &p[2..]),
        s => (s == 0x4000, i16::from_ne_bytes(*p[2..].first_chunk::<2>().ok_or_else(bad)?), &p[4..]),
    };
    let digit = |i: usize| u16::from_ne_bytes([digits[2 * i], digits[2 * i + 1]]);
    let n = digits.len() / 2;
    let Some(first) = (0..n).find(|&i| digit(i) != 0) else { return Ok(keys.fp_bytes(&[NUM_ZERO])) };
    let last = (0..n).rfind(|&i| digit(i) != 0).expect("a nonzero digit");
    let body = &digits[2 * first..2 * last + 2];
    let w = (weight as i32 - first as i32).to_le_bytes();
    let head = [NUM_FINITE, neg as u8, w[0], w[1], w[2], w[3]];
    let (mut stack, len) = ([0u8; 256], head.len() + body.len());
    let mut heap: gravity_fn::arena::ChargedVec<u8>;
    let img = if len <= stack.len() {
        &mut stack[..len]
    } else {
        // a value past 126 base-10000 digits
        let o = gravity_fn::arena::OpArena::slot_owner_kind(ctx);
        heap = gravity_fn::arena::ChargedVec::<u8>::new(o.map(|o| o.0), o.map_or(gravity_fn::arena::Kind::Region, |o| o.1));
        heap.with_vec(|v| v.resize(len, 0)); // alloc: Exec charged o
        &mut heap[..]
    };
    img[..head.len()].copy_from_slice(&head);
    img[head.len()..].copy_from_slice(body);
    Ok(keys.fp_bytes(img))
}

#[cold]
pub(crate) fn failed(e: Box<PgError>) -> i64 {
    gravity_fn::stash_error(e);
    ERROR
}

/// PG's `MemoryContextAllocationFailure` ("out of memory", ERRCODE_OUT_OF_MEMORY) for `bytes` the scratch refused.
#[cold]
pub(crate) fn out_of_memory(bytes: usize) -> Box<PgError> {
    out_of_memory_in(bytes, "a Gravity scratch region")
}

/// `out_of_memory` for a request `bytes` refused in `region` (D1392: a statement-lived pinned chunk the pool refused).
#[cold]
pub(crate) fn out_of_memory_in(bytes: usize, region: &str) -> Box<PgError> {
    let e = PgError::error("out of memory").with_detail(format!("Failed on request of size {bytes} in {region}.")); // alloc: Cold
    // this crate names no types_error item: the SqlState is set through the value PgError hands back
    let mut code = e.sqlstate();
    code.0 = sqlstate(*b"53200");
    Box::new(e.with_sqlstate(code)) // alloc: Cold
}

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_abi::{Ctx, ProgramBlock};
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    fn varlena1(bytes: &[u8]) -> Vec<u8> {
        let mut v = vec![((bytes.len() as u8 + 1) << 1) | 1]; // alloc: Cold
        v.extend_from_slice(bytes);
        v
    }

    fn varlena4(bytes: &[u8]) -> Vec<u8> {
        let mut v = ((bytes.len() as u32 + 4) << 2).to_ne_bytes().to_vec();
        v.extend_from_slice(bytes);
        v
    }

    /// An inline pglz-compressed image (`VARATT_IS_4B_C`: header `size << 2 | 2`, then `tcinfo` = the raw size with method
    /// 0 = pglz) of the pglz `stream` decompressing to `raw` bytes.
    fn pglz(raw: u32, stream: &[u8]) -> Vec<u8> {
        let mut v = ((stream.len() as u32 + 8) << 2 | 2).to_ne_bytes().to_vec();
        v.extend_from_slice(&raw.to_ne_bytes());
        v.extend_from_slice(stream);
        v
    }

    /// Eight literals: a control byte of 0 (every item a literal byte), then the bytes.
    fn literals(bytes: &[u8; 8]) -> Vec<u8> {
        let mut s = vec![0u8]; // alloc: Cold
        s.extend_from_slice(bytes);
        s
    }

    static GROWS: AtomicU64 = AtomicU64::new(0);

    unsafe extern "C" fn refuse(_ctx: *mut Ctx, _bytes: u64) -> i64 {
        GROWS.fetch_add(1, Relaxed);
        0
    }

    /// A grow host whose body panics: its pad parks the panic and it refuses (hosts never unwind into a program).
    unsafe extern "C" fn panicking(_ctx: *mut Ctx, _bytes: u64) -> i64 {
        gravity_abi::status::unwind::pad(|| -> i64 { panic!("scratch host bug") }).unwrap_or(0)
    }

    /// `m6_detoast` over a ctx whose bump window is `scratch` and whose grow is `grow`: the status, the out words and
    /// the error it stashed.
    fn detoast_in(scratch: &mut [u64], grow: unsafe extern "C" fn(*mut Ctx, u64) -> i64, img: &[u8]) -> (i64, [u64; 2], Option<Box<PgError>>) {
        let mut prog: ProgramBlock = unsafe { core::mem::zeroed() };
        prog.arena_grow = Some(grow);
        let mut ctx: Ctx = unsafe { core::mem::zeroed() };
        ctx.program = &mut prog;
        ctx.arena_cur = scratch.as_mut_ptr() as *mut u8;
        ctx.arena_end = unsafe { ctx.arena_cur.add(scratch.len() * 8) };
        let mut out = [0u64; 2];
        let _ = gravity_fn::take_error();
        let st = unsafe { m6_detoast(&mut ctx as *mut _ as u64, img.as_ptr() as u64, out.as_mut_ptr() as u64) };
        (st, out, gravity_fn::take_error())
    }

    #[test]
    fn m6_detoast_copies_the_payload_of_every_header_form_into_the_scratch() {
        for (img, want) in [(varlena1(b"short key"), &b"short key"[..]), (varlena4(b"a 4-byte header key"), b"a 4-byte header key"), (varlena4(b""), b""), (pglz(8, &literals(b"abcdefgh")), b"abcdefgh")] {
            let mut scratch = vec![0u64; 16]; // alloc: Cold
            let (lo, hi) = (scratch.as_ptr() as u64, scratch.as_ptr() as u64 + 128);
            let (st, out, err) = detoast_in(&mut scratch, refuse, &img);
            assert_eq!((st, err.map(|e| e.message().to_string())), (0, None)); // alloc: Cold
            assert!(out[0] >= lo && out[0] + out[1] <= hi, "the payload lives in the ctx's scratch window");
            assert_eq!(unsafe { core::slice::from_raw_parts(out[0] as *const u8, out[1] as usize) }, want);
        }
    }

    #[test]
    fn a_corrupt_compressed_value_raises_pgs_error_with_its_sqlstate() {
        // a stream that ends after 3 of its 8 raw bytes: pglz_decompress with check_complete fails
        let img = pglz(8, &[0, b'a', b'b', b'c']);
        let (st, _, err) = detoast_in(&mut [0u64; 16], refuse, &img);
        assert_eq!(st, ERROR);
        let e = err.expect("PG's error is stashed");
        assert_eq!(e.message(), "compressed pglz data is corrupt");
        assert_eq!(e.sqlstate().0, sqlstate(*b"XX001"), "ERRCODE_DATA_CORRUPTED");
    }

    #[test]
    fn a_refused_scratch_raises_out_of_memory() {
        let before = GROWS.load(Relaxed);
        let (st, _, err) = detoast_in(&mut [], refuse, &varlena4(b"no room for me"));
        assert_eq!(st, ERROR);
        assert!(GROWS.load(Relaxed) > before, "the empty window asked the driver to grow first");
        let e = err.expect("out of memory is stashed");
        assert_eq!((e.message(), e.sqlstate().0), ("out of memory", sqlstate(*b"53200")));
        assert_eq!(e.detail(), Some("Failed on request of size 14 in a Gravity scratch region."));
    }

    #[test]
    fn a_panic_in_the_scratch_grow_is_parked_and_the_entry_returns_error() {
        let (st, _, err) = detoast_in(&mut [], panicking, &varlena4(b"x"));
        assert_eq!(st, ERROR);
        assert!(err.is_some(), "the failed call stashes an error for the raise exit");
        let resumed = std::panic::catch_unwind(gravity_abi::status::unwind::resume);
        let payload = resumed.expect_err("the parked panic resumes at the caller");
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"scratch host bug"));
    }

    #[test]
    fn m6_fp_seed_hands_back_the_installed_seed() {
        let _ = gravity_dict::fp::install(0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908);
        let mut out = [0u64; 2];
        assert_eq!(unsafe { m6_fp_seed(out.as_mut_ptr() as u64) }, 0);
        let (k0, k1) = gravity_dict::fp::seed().expect("installed");
        assert_eq!(out, [k0, k1]);
    }

    /// A long-form numeric payload: `n_sign_dscale` (0x4000 when `neg`), the weight, the base-10000 digits.
    fn long(neg: bool, dscale: u16, weight: i16, digits: &[u16]) -> Vec<u8> {
        let mut v = (if neg { 0x4000 } else { 0 } | dscale).to_ne_bytes().to_vec();
        v.extend_from_slice(&weight.to_ne_bytes());
        digits.iter().for_each(|d| v.extend_from_slice(&d.to_ne_bytes()));
        v
    }

    /// A short-form payload (`NUMERIC_SHORT`): the header word (sign, dscale, weight sign, weight), then the digits.
    fn short(neg: bool, dscale: u16, weight: i16, digits: &[u16]) -> Vec<u8> {
        let h = 0x8000 | if neg { 0x2000 } else { 0 } | (dscale << 7 & 0x1F80) | if weight < 0 { 0x40 } else { 0 } | (weight as u16 & 0x3F);
        let mut v = h.to_ne_bytes().to_vec();
        digits.iter().for_each(|d| v.extend_from_slice(&d.to_ne_bytes()));
        v
    }

    fn num_fp_of(payload: &[u8], header4: bool) -> [u64; 2] {
        let img = if header4 { varlena4(payload) } else { varlena1(payload) };
        let mut out = [0u64; 2];
        assert_eq!(unsafe { m6_num_fp(0, img.as_ptr() as u64, out.as_mut_ptr() as u64) }, 0, "{:?}", gravity_fn::take_error().map(|e| e.message().to_owned()));
        out
    }

    #[test]
    fn m6_num_fp_images_are_equal_exactly_when_numeric_eq_says_so() {
        let _ = gravity_dict::fp::install(0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908);
        let vals = [
            ("1", long(false, 0, 0, &[1])),
            ("1.0", long(false, 1, 0, &[1])),
            ("1.00 short", short(false, 2, 0, &[1])),
            ("1.0000 with a trailing zero digit", long(false, 4, 0, &[1, 0])),
            ("1 with a leading zero digit", long(false, 0, 1, &[0, 1])),
            ("-1", long(true, 0, 0, &[1])),
            ("-1.0 short", short(true, 1, 0, &[1])),
            ("10000", long(false, 0, 1, &[1])),
            ("0.0001 short", short(false, 4, -1, &[1])),
            ("0.0001", long(false, 4, -1, &[1])),
            ("-0.0001", long(true, 4, -1, &[1])),
            ("12345.6789", long(false, 4, 1, &[1, 2345, 6789])),
            ("12345.67890", long(false, 5, 1, &[1, 2345, 6789, 0])),
            ("12345.6788", long(false, 4, 1, &[1, 2345, 6788])),
            ("0", long(false, 0, 0, &[])),
            ("0.000", long(false, 3, 0, &[])),
            ("-0", long(true, 0, 0, &[])),
            ("0 short", short(false, 0, 0, &[])),
            ("-0.00 short", short(true, 2, 0, &[])),
            ("NaN", 0xC000u16.to_ne_bytes().to_vec()),
            ("+Inf", 0xD000u16.to_ne_bytes().to_vec()),
            ("-Inf", 0xF000u16.to_ne_bytes().to_vec()),
        ];
        // (all-zero digits with ndigits > 0 are left out: make_result never stores them, and PG itself disagrees there,
        // hash_numeric calling it zero and cmp_numerics greater than zero)
        let eq = |x: &[u8], y: &[u8]| gravity_fn::shims::byref_val::numeric_eq(core::ptr::null_mut(), gravity_fn::View::of(x), gravity_fn::View::of(y)).val != 0;
        for (a, x) in &vals {
            assert_eq!(num_fp_of(x, true), num_fp_of(x, false), "{a}: one image in both header forms");
            for (b, y) in &vals {
                assert_eq!(num_fp_of(x, true) == num_fp_of(y, true), eq(x, y), "{a} vs {b}");
            }
        }
        assert!(eq(&vals[0].1, &vals[2].1) && eq(&vals[14].1, &vals[16].1) && eq(&vals[19].1, &vals[19].1), "the classes the test means are numeric_eq's");
    }

    #[test]
    fn m6_num_fp_of_no_numeric_is_an_error_not_a_value() {
        let _ = gravity_dict::fp::install(0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908);
        for payload in [vec![0x00u8], 0xE000u16.to_ne_bytes().to_vec(), vec![0, 0, 1]] { // alloc: Cold
            let img = varlena4(&payload);
            let _ = gravity_fn::take_error();
            assert_eq!(unsafe { m6_num_fp(0, img.as_ptr() as u64, ([0u64; 2].as_mut_ptr()) as u64) }, ERROR);
            assert!(gravity_fn::take_error().is_some_and(|e| e.message().contains("that is no numeric")));
        }
    }

    /// D1582: a collation without a sort-key prefix at run time ("C" here) gives every key one word, the comparator's to
    /// order; a collation that cannot be looked up is PG's error, stashed (never a panic).
    #[test]
    fn m6_coll_prefix_without_a_prefix_is_one_word_and_a_bad_collation_an_error() {
        let mut w = [7u64; 2];
        for s in [&b""[..], b"a", b"zzzzzzzzzz"] {
            assert_eq!(unsafe { m6_coll_prefix(s.as_ptr() as u64, s.len() as u64, 950, w.as_mut_ptr() as u64) }, 0);
            assert_eq!(w[0], 1 << 63);
        }
        let _ = gravity_fn::take_error();
        assert_eq!(unsafe { m6_coll_prefix((b"a".as_ptr()) as u64, 1, 0, w.as_mut_ptr() as u64) }, ERROR);
        assert!(gravity_fn::take_error().is_some());
    }

    /// D1582: sort-key prefixes compared unsigned (PostgreSQL's abbreviated keys) order as their flipped words compared
    /// signed (the order prefix word's convention).
    #[test]
    fn flipped_prefix_words_order_as_the_unsigned_prefixes() {
        let ws = [0u64, 1, 0x7fff_ffff_ffff_ffff, 1 << 63, (1 << 63) + 1, u64::MAX - 1, u64::MAX];
        for &a in &ws {
            for &b in &ws {
                assert_eq!(((a ^ (1 << 63)) as i64).cmp(&((b ^ (1 << 63)) as i64)), a.cmp(&b), "{a:#x} {b:#x}");
            }
        }
    }

    #[test]
    fn sqlstate_is_pgs_make_sqlstate() {
        // PGSIXBIT('5') | PGSIXBIT('3') << 6 | PGSIXBIT('2') << 12 (ERRCODE_OUT_OF_MEMORY's 53200)
        assert_eq!(sqlstate(*b"53200"), 5 | 3 << 6 | 2 << 12);
        assert_eq!(sqlstate(*b"XX001"), 40 | 40 << 6 | 1 << 24);
    }
}
