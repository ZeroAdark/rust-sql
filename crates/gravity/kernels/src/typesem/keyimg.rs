use gravity_abi::PtrWord;
use crate::host_abi::*;

// ---- the key-image hosts (M4-work §B.4 / §B.5): the bad-image fingerprint, the bpchar trim, the provider's sort key, the collation compare, the numeric mantissa, the process seed ----

/// `[lo, hi]` of `bytes` under the process seed into `out`; `ERROR` before the seed is installed.
unsafe fn fp_out(bytes: &[u8], out: *mut u64) -> i64 {
    let Some(k) = gravity_dict::fp::keys() else { return bare("a fingerprint before the process seed is installed") };
    let fp = k.fp_bytes(bytes);
    out.write_unaligned(fp[0]);
    out.add(1).write_unaligned(fp[1]);
    0
}

crate::contract::host! {
    /// The fingerprint of the text at `datum` in any header form (`siphash::varlena`'s `bad` arm: a compressed inline image or a TOAST pointer is detoasted into the scratch context): `[lo, hi]` at `out`, `ERROR` on a detoast failure (stashed).
    pub unsafe fn text_fp(datum: u64, out: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let datum = datum as *const u8;
        let out = out as *mut u64;
        match gravity_fn::with_payload(datum, |bytes| fp_out(bytes, out)) {
            Ok(st) => st,
            Err(e) => {
                gravity_fn::stash_error(e);
                ERROR
            }
        }
    }
}

/// The payload `(ptr, len)` of a bad varlena image (a compressed inline value or a TOAST pointer) detoasted into the ctx bump arena (the
/// Call's scratch: the run compare reads it within the window, the group's representative copies it into the pinned arena at once)
/// written at `out`; `ERROR` (stashed) when the detoast fails or the arena is exhausted.
pub const TEXT_DETOAST_LIFE: gravity_fn::ResultLife = gravity_fn::ResultLife::CallerRegion;
crate::contract::host! {
    pub unsafe fn text_detoast(ctx: *mut Ctx, datum: *const u8, out: *mut u64) -> i64 {
        match gravity_fn::with_payload(datum, |bytes| {
            let p = util::scratch(ctx, bytes.len().max(1));
            if p.is_null() {
                gravity_fn::stash_error(Box::new(gravity_fn::PgError::error("gravity: a detoasted group key past the statement's scratch pool"))); // alloc: Cold
                return ERROR;
            }
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
            out.write_unaligned(p as u64);
            out.add(1).write_unaligned(bytes.len() as u64);
            0
        }) {
            Ok(st) => st,
            Err(e) => {
                gravity_fn::stash_error(e);
                ERROR
            }
        }
    }

    /// `bcTruelen`: the length of the `len` bytes at `ptr` without trailing spaces (a bpchar's key image).
    pub unsafe fn text_trim(ptr: u64, len: u64) -> u64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let ptr = ptr as *const u8;
        let mut n = len as usize;
        while n > 0 && *ptr.add(n - 1) == b' ' {
            n -= 1;
        }
        n as u64
    }

    /// The provider's sort key of the `len` bytes at `ptr` under `coll` (`pg_strnxfrm`), fingerprinted into `out` (a non-deterministic collation's equality: two strings equal under the collation have one sort key); `ERROR` when the provider has no trusted sort key (libc) or the collation cannot be opened (stashed).
    /// E-kern: `ctx` (null or bound) names the owner of a sort key past the 2 KiB stack buffer.
    pub unsafe fn text_strnxfrm(ctx: u64, ptr: u64, len: u64, coll: u64, out: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let ctx = ctx as *mut gravity_abi::Ctx;
        let ptr = ptr as *const u8;
        let out = out as *mut u64;
        guard!(ERROR, {
            let src = core::slice::from_raw_parts(ptr, len as usize);
            let mut stack = [0u8; 2048];
            let n = match gravity_fn::typesem::collation::sort_key(coll as u32, src, &mut stack) {
                Ok(Some(n)) if n < stack.len() => return fp_out(&stack[..n], out),
                Ok(Some(n)) => n,
                Ok(None) => return bare("a collation sort key the collation cannot produce"),
                Err(e) => {
                    gravity_fn::stash_error(e);
                    return ERROR;
                }
            };
            // a sort key past the 2 KiB stack buffer: on the breaker slot `ctx`'s pipeline produces into, else the
            // statement's (`OpArena::slot_owner_kind`), asked only here
            let o = gravity_fn::arena::OpArena::slot_owner_kind(ctx);
            let mut big = gravity_fn::arena::ChargedVec::<u8>::new(o.map(|o| o.0), o.map_or(gravity_fn::arena::Kind::Region, |o| o.1));
            big.with_vec(|v| v.resize(n + 1, 0)); // alloc: Exec charged o
            match gravity_fn::typesem::collation::sort_key(coll as u32, src, &mut big[..]) {
                Ok(Some(m)) => fp_out(&big[..m.min(n)], out),
                Ok(None) => ERROR,
                Err(e) => {
                    gravity_fn::stash_error(e);
                    ERROR
                }
            }
        })
    }

    /// `varstr_cmp` of two byte strings under `coll`: −1 / 0 / 1, or `2` when the compare raised (stashed).
    pub unsafe fn text_cmp_coll(a: *const u8, alen: u64, b: *const u8, blen: u64, coll: u64) -> i64 {
        let x = core::slice::from_raw_parts(a, alen as usize);
        let y = core::slice::from_raw_parts(b, blen as usize);
        match gravity_fn::typesem::collation::compare(x, y, coll as u32) {
            Ok(c) => c.signum() as i64,
            Err(e) => {
                gravity_fn::stash_error(e);
                2
            }
        }
    }

    /// The numeric varlena's mantissa at `scale` (`gravity_fn::agg_final::num_mantissa`: the sentinels for the specials, `NO_MANTISSA` for another scale or a toasted image).
    pub unsafe fn num_mantissa(datum: *const u8, scale: u64) -> i64 {
        guard!(gravity_fn::agg_final::NO_MANTISSA, {
            gravity_fn::agg_final::num_mantissa(core::ptr::null_mut(), datum, scale as i64).val
        })
    }

    /// The mantissa `m` at `scale` against the numeric varlena `datum`: -1 / 0 / 1, 2 for an unreadable image (`gravity_fn::agg_final::num_cmp_mantissa`).
    pub unsafe fn num_cmp_mantissa(m: i64, scale: u64, datum: *const u8) -> i64 {
        guard!(2, {
            gravity_fn::agg_final::num_cmp_mantissa(core::ptr::null_mut(), m, scale as i64, datum).val
        })
    }

    /// The process fingerprint seed `(k0, k1)` into `out` (read once per function open, never an immediate: the code cache is keyed by content); `ERROR` before it is installed.
    pub unsafe fn fp_seed(out: *mut u64) -> i64 {
        let Some((k0, k1)) = gravity_dict::fp::seed() else { return bare("a fingerprint seed before it is installed") };
        out.write_unaligned(k0);
        out.add(1).write_unaligned(k1);
        0
    }
}

pub(crate) fn f64_image(f: f64) -> i64 {
    if f.is_nan() {
        return i64::MAX;
    }
    let b = f.to_bits() as i64;
    b ^ ((b >> 63) & i64::MAX)
}

crate::contract::host! {
    /// The dense front's drain (M3-L6K, PERFDIAG3 §C fix 3): every used record of the `points` records at `front` (the packed index `idx = (k0 − lo0) + n0 · (k1 − lo1)` over `nkeys` keys — `keysw`'s low byte, the raw-f64 slot mask above it — the layout's NULL-flags word 0) gets its keys and is folded into the claim's table through `locate`; a status below zero on failure.
    pub unsafe fn agg_front_drain(ctx: *mut Ctx, st: *mut u8, front: *mut u8, points: u64, keysw: u64, lo0: i64, n0: u64, lo1: i64) -> i64 {
        guard!(ERROR, {
            let (nkeys, fmask) = (keysw & 0xff, keysw >> 8);
            let st = st as *mut AggState;
            let layout = &*(*st).layout.ptr();
            let size = layout.size() as usize;
            for idx in 0..points {
                let base = front.add(idx as usize * size);
                let src = Rec::at(base, layout);
                if !src.used() {
                    continue;
                }
                // the float min / max slots hold the f64 bits (M3-L6O): imaged once here, in the record's order
                let mut m = fmask;
                while m != 0 {
                    let j = m.trailing_zeros() as usize;
                    m &= m - 1;
                    let p = base.add(layout.slot_off(j) as usize) as *mut i64;
                    *p = f64_image(f64::from_bits(*p as u64));
                }
                src.set_key(0, lo0.wrapping_add((idx % n0) as i64) as u64);
                if nkeys == 2 {
                    src.set_key(1, lo1.wrapping_add((idx / n0) as i64) as u64);
                }
                for k in nkeys as usize..layout.nkeys() {
                    src.set_key(k, 0);
                }
                if layout.flags & LAYOUT_IMPLICIT_COUNT != 0 {
                    // the front counts rows from 0; `fold` takes the record form (`1 + rows`)
                    let so = base.add(layout.sentinel_off() as usize) as *mut u64;
                    *so += 1;
                }
                match locate(ctx, st, src.keys()) {
                    Ok(p) => {
                        if let Err(o) = Rec::at(p, layout).fold(&src) {
                            return o_status(o);
                        }
                    }
                    Err(e) => return e,
                }
            }
            0
        })
    }
}

#[cfg(test)]
mod key_image_tests {
    use super::*;

    fn varlena4(bytes: &[u8]) -> Vec<u8> {
        let mut v = ((bytes.len() as u32 + 4) << 2).to_ne_bytes().to_vec();
        v.extend_from_slice(bytes);
        v
    }

    fn varlena1(bytes: &[u8]) -> Vec<u8> {
        let mut v = vec![((bytes.len() as u8 + 1) << 1) | 1]; // alloc: Cold
        v.extend_from_slice(bytes);
        v
    }

    #[test]
    fn text_fp_matches_the_dictionary_fingerprint_in_both_header_forms() {
        let _ = gravity_dict::fp::install(0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908);
        let want = gravity_dict::fp::fp128(b"hello world").unwrap();
        for img in [varlena4(b"hello world"), varlena1(b"hello world")] {
            let mut out = [0u64; 2];
            assert_eq!(unsafe { text_fp(img.as_ptr() as u64, out.as_mut_ptr() as u64) }, 0);
            assert_eq!(out, want);
        }
        let mut out = [0u64; 2];
        let empty = varlena1(b"");
        assert_eq!(unsafe { text_fp(empty.as_ptr() as u64, out.as_mut_ptr() as u64) }, 0);
        assert_eq!(out, gravity_dict::fp::fp128(b"").unwrap());
    }

    #[test]
    fn text_trim_drops_trailing_spaces_only() {
        for (s, n) in [("abc  ", 3usize), ("  abc", 5), ("     ", 0), ("", 0), ("a b ", 3)] {
            assert_eq!(unsafe { text_trim(s.as_ptr() as u64, s.len() as u64) }, n as u64, "{s:?}");
        }
    }

    #[test]
    fn a_name_image_is_its_bytes_before_the_nul() {
        let mut n = [0u8; 64];
        n[..3].copy_from_slice(b"abc");
        assert_eq!(unsafe { name_image(n.as_ptr()) }, b"abc");
        assert_eq!(unsafe { name_image([0u8; 64].as_ptr()) }, b"");
        let full = [b'z'; 64];
        assert_eq!(unsafe { name_image(full.as_ptr()) }.len(), 64, "a 63-byte name's NUL, or none: never past 64");
        // namecmp under C (strncmp over NAMEDATALEN): "ab" < "abc", as the trimmed images compare
        let mut ab = [0u8; 64];
        ab[..2].copy_from_slice(b"ab");
        assert!(unsafe { name_image(ab.as_ptr()) } < unsafe { name_image(n.as_ptr()) });
        assert_eq!(SK_NAME & (SK_SLOT | SK_DESC | SK_NULLS_FIRST | SK_TEXT | SK_HEAP | SK_FLOAT | SK_UNSIGNED | SK_BPCHAR), 0);
    }

    /// SK_CMP (M6-HOSTED): a comparator whose bind or call fails stashes its error once and compares Equal; the error
    /// the sort returns is that first error (`copy_cmp` makes no call after it), never a later one.
    #[test]
    fn a_failed_comparator_stashes_its_error_once() {
        let _ = gravity_fn::take_error();
        CMP_ERR.with(|c| c.set(false));
        assert_eq!(gravity_fn::typesem::keysem::call::order(0xdead_beef, 0, 0, 0).unwrap_or_else(order_failed), Ordering::Equal, "an id no lowering resolved binds nothing");
        assert!(CMP_ERR.with(|c| c.replace(false)));
        assert!(gravity_fn::take_error().is_some_and(|e| e.message.contains("no lowering resolved")));
        assert!(gravity_fn::take_error().is_none(), "one error, stashed once");
        assert_eq!(SK_CMP & (SK_SLOT | SK_DESC | SK_NULLS_FIRST | SK_TEXT | SK_HEAP | SK_FLOAT | SK_UNSIGNED | SK_BPCHAR | SK_NAME), 0);
    }

    /// D1165 residual: `SK_CMP` on a key word (an enum's oid) orders through its comparator, never the word: classified
    /// apart from a text copy's `SK_CMP` and a slot's; a failed call stashes its error once and every later pair is Equal.
    #[test]
    fn a_key_word_ordered_by_its_comparator_is_never_its_raw_word() {
        assert!(sk_key_cmp(SK_CMP | 3) && sk_key_cmp(SK_CMP | SK_DESC | 2 << SK_NULL_SHIFT));
        assert!(!sk_key_cmp(SK_CMP | SK_TEXT | SK_HEAP) && !sk_key_cmp(SK_CMP | SK_SLOT | 1) && !sk_key_cmp(3) && !sk_key_cmp(SK_TEXT | 2 << SK_TEXT_LANE_SHIFT));
        let _ = gravity_fn::take_error();
        let err = core::cell::Cell::new(0i64);
        assert_eq!(unsafe { word_cmp(2, 4, 0xdead_beef, &err) }, Ordering::Equal, "no raw order: the comparator decides");
        assert_eq!(err.get(), ERROR);
        assert!(gravity_fn::take_error().is_some());
        assert_eq!(unsafe { word_cmp(4, 2, 0xdead_beef, &err) }, Ordering::Equal);
        assert!(gravity_fn::take_error().is_none(), "no call after the first error");
    }

    #[test]
    fn text_cmp_coll_under_c_is_the_byte_order() {
        for (a, b, want) in [("a", "b", -1i64), ("b", "a", 1), ("ab", "ab", 0), ("ab", "abc", -1), ("", "", 0)] {
            let got = unsafe { text_cmp_coll(a.as_ptr(), a.len() as u64, b.as_ptr(), b.len() as u64, 950) };
            assert_eq!(got, want, "{a:?} vs {b:?}");
        }
    }

    #[test]
    fn num_mantissa_reads_the_scaled_word() {
        // numeric 12.34 in the long form: n_sign_dscale (positive, dscale 2), weight 0, digits [12, 3400]
        let mut payload = Vec::new(); // alloc: Cold
        for h in [2u16, 0, 12, 3400] {
            payload.extend_from_slice(&h.to_ne_bytes());
        }
        let img = varlena4(&payload);
        assert_eq!(unsafe { num_mantissa(img.as_ptr(), 2) }, 1234);
        assert_eq!(unsafe { num_mantissa(img.as_ptr(), 3) }, gravity_fn::agg_final::NO_MANTISSA);
    }

    /// `n / 10^ds` as a long-form numeric varlena (base-10000 groups, zero groups trimmed).
    fn num_image(n: i128, ds: u32) -> Vec<u8> {
        let pad = (4 - ds % 4) % 4;
        let mut big = n.unsigned_abs() * 10u128.pow(pad);
        let fracg = ((ds + pad) / 4) as i32;
        let mut groups = Vec::new(); // alloc: Cold
        while big > 0 {
            groups.push((big % 10_000) as u16);
            big /= 10_000;
        }
        groups.reverse();
        let mut weight = groups.len() as i32 - fracg - 1;
        while groups.first() == Some(&0) {
            groups.remove(0);
            weight -= 1;
        }
        while groups.last() == Some(&0) {
            groups.pop();
        }
        if groups.is_empty() {
            weight = 0;
        }
        let sign = if n < 0 { 0x4000u16 } else { 0 };
        let mut payload = Vec::new(); // alloc: Cold
        for h in [sign | ds as u16, weight as i16 as u16].into_iter().chain(groups) {
            payload.extend_from_slice(&h.to_ne_bytes());
        }
        varlena4(&payload)
    }

    #[test]
    fn num_cmp_mantissa_is_the_exact_order() {
        let vals = [0i128, 1, -1, 5, -5, 199, -199, 200, 2000, 12345, -12345, 99_999_999, 10_000_000_000_000_001, -7_000_000_000_000_000_003];
        for &n in &vals {
            for ds in [0u32, 1, 2, 3, 5, 8, 20] {
                let img = num_image(n, ds);
                for scale in [0u32, 2, 3] {
                    for m in [-201i64, -200, -20, -2, -1, 0, 1, 2, 19, 20, 21, 199, 200, 1234, 1235, 12345, 123450, i64::MIN + 2, i64::MAX - 2] {
                        let Some(lhs) = (m as i128).checked_mul(10i128.pow(ds)) else { continue };
                        let rhs = n * 10i128.pow(scale);
                        let want = (lhs.cmp(&rhs) as i64).signum();
                        let got = unsafe { num_cmp_mantissa(m, scale as u64, img.as_ptr()) };
                        assert_eq!(got, want, "m {m} @{scale} vs {n}/10^{ds}");
                    }
                }
            }
        }
        let nan = varlena4(&0xC000u16.to_ne_bytes());
        assert_eq!(unsafe { num_cmp_mantissa(5, 2, nan.as_ptr()) }, -1);
        assert_eq!(unsafe { num_cmp_mantissa(gravity_fn::agg_final::MANT_NAN, 2, nan.as_ptr()) }, 0);
        assert_eq!(unsafe { num_cmp_mantissa(gravity_fn::agg_final::MANT_NINF, 2, num_image(-5, 0).as_ptr()) }, -1);
    }

    #[test]
    fn fp_seed_hands_the_installed_seed() {
        let _ = gravity_dict::fp::install(0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908);
        let mut out = [0u64; 2];
        assert_eq!(unsafe { fp_seed(out.as_mut_ptr()) }, 0);
        assert_eq!(Some((out[0], out[1])), gravity_dict::fp::seed());
    }
}
