//! entries.rs's host entries and the unsafe reads under them. Each entry is a `contract::host` function under the one
//! landing pad (`#[guard(ERROR)]`): a panic in its body is parked for the program's caller and the entry answers
//! `ERROR`; PG's own errors are stashed the same way (`failed`).
use gravity_abi::status::ERROR;
use gravity_abi::Ctx;
use gravity_fn::PgError;

use super::{failed, num_fp, out_of_memory};

crate::contract::host! {
    /// `m6_detoast` (face/varkey.rs `payload`): the payload of the varlena at `datum` (`pg_detoast_datum_packed`: a
    /// compressed inline value decompressed, an external one fetched) copied into the ctx's scratch (the Call's bump
    /// window, rewound at the window's end: the key's image is taken from it at once, a key kept past its row copies it),
    /// `(ptr, len)` at `out[0..2]`, 0. On a failed detoast PG's error (message and SQLSTATE) is stashed and `ERROR`
    /// returned (the IR leaves through `l_exit`); a refused scratch is PG's "out of memory". The IR decodes the 1-byte
    /// and 4-byte uncompressed headers inline and calls this only for the others (any form is served).
    /// # Safety — `ctx` is bound, `datum` is a live varlena, `out` is writable for two words.
    #[guard(ERROR)]
    pub unsafe fn m6_detoast(ctx: u64, datum: u64, out: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let ctx = ctx as *mut Ctx;
        let datum = datum as *const u8;
        let out = out as *mut u64;
        detoast(ctx, datum, out)
    }

    /// `m6_fp_seed`: the process fingerprint seed `(k0, k1)` at `out[0..2]` (read once per function open, never an
    /// immediate: the code cache is keyed by content), 0; `ERROR` before it is installed. The varlena hash image is the
    /// Fp128 of the payload under this seed, in IR (the same image as a pgrc code's Fp128 table).
    /// # Safety — `out` is writable for two words.
    #[guard(ERROR)]
    pub unsafe fn m6_fp_seed(out: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let out = out as *mut u64;
        match gravity_dict::fp::seed() {
            Some((k0, k1)) => {
                out.write_unaligned(k0);
                out.add(1).write_unaligned(k1);
                0
            }
            None => failed(Box::new(PgError::error("gravity: a fingerprint seed before it is installed"))), // alloc: Cold
        }
    }

    /// `m6_num_fp`: the exact Fp128 of the numeric at `datum` (any header form; a compressed or external one detoasted)
    /// under the process seed at `out[0..2]`, 0. Numerics equal under `numeric_eq` (PG's `cmp_numerics`: 1.0 = 1.00,
    /// every zero of any sign or scale, NaN = NaN, each infinity only itself) have one image, unequal ones distinct
    /// canonical bytes: what `hash_numeric` (numeric.c:2822) hashes, the digits without leading or trailing zeros and the
    /// weight, plus the sign and the specials it folds together (its hash may collide; this image is exact). ERROR on a
    /// detoast failure, a malformed image or before the seed is installed (stashed).
    /// E-kern: `ctx` names the owner of an image past the 256-byte stack buffer (`num_fp`).
    /// # Safety — `ctx` is null or bound, `datum` is a live numeric varlena, `out` is writable for two words.
    #[guard(ERROR)]
    pub unsafe fn m6_num_fp(ctx: u64, datum: u64, out: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let ctx = ctx as *mut Ctx;
        let datum = datum as *const u8;
        let out = out as *mut u64;
        match gravity_fn::with_payload(datum, |p| num_fp(ctx, p)).and_then(|r| r) {
            Ok(fp) => {
                out.write_unaligned(fp[0]);
                out.add(1).write_unaligned(fp[1]);
                0
            }
            Err(e) => failed(e),
        }
    }

    /// `m6_hosted_fp` (lowerv4 rep.hosted.hash): the exact Fp128 of the key value at `datum` (a varlena of any header
    /// form, or a fixed by-reference value's bytes) of the canonical form whose id is `form` (canon.rs `by_id`: the
    /// program's, from its lowering) under the process seed at `out[0..2]`, 0: the Fp128 of its canonical form
    /// (gravity_fn canon.rs: array / composite / range walked, floats, numerics, intervals, names and bpchars folded to
    /// one image per equal value), so equal values under the type's `=` have one image. ERROR (stashed) on a type with
    /// no canonical form (the lowering refused it first), a malformed value, a detoast failure, or before the seed is
    /// installed.
    /// # Safety — `datum` is a live value of `form`'s type, `out` is writable for two words.
    #[guard(ERROR)]
    pub unsafe fn m6_hosted_fp(datum: u64, form: u64, out: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let datum = datum as *const u8;
        let out = out as *mut u64;
        match gravity_fn::typesem::by_id(form).and_then(|f| hosted_fp(datum, f)) {
            Ok(fp) => {
                out.write_unaligned(fp[0]);
                out.add(1).write_unaligned(fp[1]);
                0
            }
            Err(e) => failed(e),
        }
    }

    /// `m6_extract_word` (lowerv4 rep.hosted.hash, D1326): the int4 group key word at `out[0]` of the integral
    /// `extract(field FROM ts)` whose numeric result is the varlena at `datum` (gravity_fn canon.rs `integral_word`:
    /// ±Infinity past int4's range), 0. ERROR (stashed): PG's "integer out of range" past int4, a NaN, a fraction, or a
    /// detoast failure.
    /// # Safety — `datum` is a live numeric varlena, `out` is writable for one word.
    #[guard(ERROR)]
    pub unsafe fn m6_extract_word(datum: u64, out: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let datum = datum as *const u8;
        let out = out as *mut u64;
        match gravity_fn::with_payload(datum, gravity_fn::typesem::integral_word).and_then(|r| r) {
            Ok(w) => {
                out.write_unaligned(w as u64);
                0
            }
            Err(e) => failed(e),
        }
    }

    /// `m6_coll_prefix` (lowerv4 rep.varlena.coll `order`, road key.coll, D1582): the order prefix word of a key under an
    /// ICU collation, the first 8 bytes of its sort key (gravity_fn `sort_key_prefix`, PostgreSQL's abbreviated key)
    /// big-endian with the top bit flipped (rep.varlena.order's `prefix8` convention: words compare signed), at
    /// `out[0]`, 0. Two keys whose words differ order as the words do; equal words are the comparator's
    /// (`TailForm::Collated`) to order. A collation without a prefix at run time gives every key one word (the
    /// comparator orders them all). ERROR (stashed): the provider's sort-key failure, as PostgreSQL's
    /// `strnxfrm_prefix_icu`.
    /// # Safety — `ptr` is readable for `len` bytes (the payload, a bpchar trimmed), `out` is writable for one word.
    #[guard(ERROR)]
    pub unsafe fn m6_coll_prefix(ptr: u64, len: u64, coll: u64, out: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let ptr = ptr as *const u8;
        let out = out as *mut u64;
        let src = if len == 0 { &[][..] } else { core::slice::from_raw_parts(ptr, len as usize) };
        match gravity_fn::typesem::collation::sort_key_prefix(coll as u32, src) {
            Ok(w) => {
                out.write_unaligned(w.unwrap_or(0) ^ (1 << 63));
                0
            }
            Err(e) => failed(e),
        }
    }
}

unsafe fn detoast(ctx: *mut Ctx, datum: *const u8, out: *mut u64) -> i64 {
    let copied = gravity_fn::with_payload(datum, |bytes| {
        let p = crate::util::scratch(ctx, bytes.len().max(1));
        if !p.is_null() {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        }
        (p, bytes.len())
    });
    match copied {
        Ok((p, n)) if !p.is_null() => {
            out.write_unaligned(p as u64);
            out.add(1).write_unaligned(n as u64);
            0
        }
        Ok((_, n)) => failed(out_of_memory(n)),
        Err(e) => failed(e),
    }
}

thread_local! {
    /// The canonical bytes of the last key (reused per call: its capacity is the widest key's).
    static CANON: core::cell::RefCell<Vec<u8>> = const { core::cell::RefCell::new(Vec::new()) }; // alloc: Cold
}

unsafe fn hosted_fp(datum: *const u8, form: &gravity_fn::typesem::Form) -> Result<[u64; 2], Box<PgError>> {
    let keys = gravity_dict::fp::keys().ok_or_else(|| Box::new(PgError::error("gravity: a fingerprint seed before it is installed")))?; // alloc: Cold
    // the exact form the program's lowering built (the leader's catalog read): a pool worker has no catalog, and a later
    // ALTER TYPE never changes it
    CANON.with(|c| {
        let mut buf = c.borrow_mut();
        buf.clear();
        gravity_fn::typesem::canonical(form, datum, &mut buf)?;
        Ok(keys.fp_bytes(&buf))
    })
}
