//! ops::agg::numeric's unsafe host entries, the family's one home of unsafe code: the entries the lowered IR calls with
//! a numeric datum, a region or a word pointer, one module per file they came from (num_carry.rs, num_datum.rs,
//! num_wide.rs, num_word.rs), each re-exported there, so callers, host names and IR are unchanged.
#![allow(unsafe_code)]

/// `num_carry.rs`'s entries.
pub(crate) mod carry {
    use gravity_fn::agg_final::{self, SENT_HI, SUM_HI_LIMIT};
    use gravity_fn::{ErrSlot, Region};
    use crate::ops::agg::numeric::num_word::kept;
    use super::word::{num_avg_ds, num_sum_ds};

    gravity_kernels::contract::host! {
        /// D830: class_num.rs's carry-form sum (`cy` × 2^126 + the i128 `hi:lo`; avg: `n` > 0) at scale `sd >> 32`, dscale `sd`'s
        /// low word: inside D299's range `num_sum_ds`, past it gravity_fn's i256 final at the dscale (exact: every term's digits
        /// below it are zero). # Safety — as `num_sum_ds`.
        pub(crate) unsafe fn num_carry_final(r: u64, lo: u64, hi: u64, cy: u64, n: u64, sd: u64) -> u64 {
            carried(r, [lo, hi, cy], (n != 0).then_some(n), sd)
        }
    }

    unsafe fn carried(r: u64, [lo, hi, cy]: [u64; 3], n: Option<u64>, sd: u64) -> u64 {
        let (scale, d) = (sd >> 32, sd & 0xffff_ffff);
        let sx = ((hi as i64) >> 63) as u64;
        let add = |x: u64, y: u64, k: bool| { let (s, c1) = x.overflowing_add(y); let (t, c2) = s.overflowing_add(k as u64); (t, c1 || c2) };
        let (w1, k1) = add(hi, cy << 62, false);
        let (w2, k2) = add(sx, ((cy as i64) >> 2) as u64, k1);
        let (w3, _) = add(sx, ((cy as i64) >> 63) as u64, k2);
        let inside = (w1 as i64).wrapping_add(SUM_HI_LIMIT) >= 0 && w2 == ((w1 as i64) >> 63) as u64 && w3 == w2;
        if hi as i64 == SENT_HI || inside {
            let hi = if hi as i64 == SENT_HI { hi } else { w1 };
            return match n { None => num_sum_ds(r, lo, hi, scale, d), Some(n) => num_avg_ds(r, lo, hi, n, scale, d) };
        }
        let neg = (w3 as i64) < 0;
        let neg4 = |m: [u64; 4]| { let mut o = [0u64; 4]; let mut k = true; for i in 0..4 { let (v, c) = (!m[i]).overflowing_add(k as u64); o[i] = v; k = c; } o };
        let mut m = [lo, w1, w2, w3];
        if neg { m = neg4(m); }
        (d.min(scale)..scale).for_each(|_| _ = agg_final::wide_divrem(&mut m, 10));
        if neg { m = neg4(m); }
        let mut e = ErrSlot::NONE;
        let v = match n {
            None => agg_final::num_sum256(&mut e, r as *mut Region, m.as_ptr(), d.min(scale) as i64),
            Some(n) => agg_final::num_avg256(&mut e, r as *mut Region, m.as_ptr(), n as i64, d.min(scale) as i64),
        };
        kept(v, &mut e)
    }
}

/// `num_datum.rs`'s entries.
pub(crate) mod datum {
    use crate::ops::agg::numeric::num_datum::read;

    gravity_kernels::contract::host! {
        /// Numeric datum `p`'s value × 10^`scale` as an exact i128 into `out[0..2]`, its image's dscale into `out[2]`: 0, a special's
        /// code (dscale 0), or -1 with the error stashed (a failed detoast; a dscale past `scale` or a value past i128, which
        /// decompose's 37-digit bound keeps from its arms). # Safety — `p` a live numeric varlena in any header form, `out` 3 words.
        pub(crate) unsafe fn term_ds(p: u64, scale: i64, out: u64) -> i64 {
            // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
            let out = out as *mut u64;
            let r = gravity_fn::with_payload(p as usize as *const u8, |x| read(x, scale));
            match r.and_then(|r| r.map_err(|m| Box::new(gravity_fn::PgError::error(m)))) { // alloc: Cold
                Ok((code, v, d)) => { out.write(v as u64); out.add(1).write((v >> 64) as u64); out.add(2).write(d); code }
                Err(e) => (gravity_fn::stash_error(e), -1).1,
            }
        }
    }
}

/// `num_wide.rs`'s entries.
pub(crate) mod wide {
    use gravity_abi::breaker::NumWide;
    use gravity_fn::agg_final::{wide_mul_add, SP_NAN, SP_NINF, SP_PINF};
    use crate::ops::agg::numeric::num_wide::{kept, mantissa, neg256, UNREADABLE};

    gravity_kernels::contract::host! {
        /// Numeric datum `p`'s value × 10^`scale` × `mult` as an i256 into `out` (four words): 0 for a finite term, the specials
        /// codes for NaN / ±Infinity, SP_OVERFLOW past |2^254|, `UNREADABLE` (numeric.c's on-disk image: a 1- or 4-byte varlena
        /// header, the short or long numeric header, base-10000 digits).
        /// # Safety — `p` is a live numeric varlena, `out` four writable words.
        pub(crate) unsafe fn term(p: u64, scale: i64, mult: i64, out: u64) -> i64 {
            // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
            let out = out as *mut u64;
            let p = p as usize as *const u8;
            let (body, len) = match *p {
                1 => return UNREADABLE,
                b0 if b0 & 1 != 0 => (p.add(1), (b0 >> 1) as usize - 1),
                _ => match p.cast::<u32>().read_unaligned() {
                    w if w & 3 != 0 => return UNREADABLE,
                    w => (p.add(4), (w >> 2) as usize - 4),
                },
            };
            let rd = |i: usize| body.add(2 * i).cast::<u16>().read_unaligned();
            let h = rd(0);
            let (neg, weight, first) = match h & 0xC000 {
                0xC000 => return match h & 0xF000 { 0xD000 => SP_PINF, 0xF000 => SP_NINF, _ => SP_NAN },
                0x8000 => (h & 0x2000 != 0, if h & 0x0040 != 0 { (h | !0x3F) as i16 } else { (h & 0x3F) as i16 }, 1),
                s => (s == 0x4000, rd(1) as i16, 2),
            };
            // CLASS E removed: the digits are read in place, no copy of the value
            match mantissa((first..len / 2).map(|i| rd(i) as u64), weight as i32, scale as i32, mult as u64, neg) {
                Ok(w) => {
                    core::ptr::write_unaligned(out.cast::<NumWide>(), w);
                    0
                }
                Err(code) => code,
            }
        }

        /// join.mult: the i256 at `p` × `m` in place; 1 when it passes |2^254| (the caller's OVERFLOW code).
        /// # Safety — `p` four writable words.
        pub(crate) unsafe fn scaled(p: u64, m: i64) -> i64 {
            // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
            let p = p as *mut u64;
            let w = core::ptr::read_unaligned(p.cast::<NumWide>());
            let (mut abs, neg) = gravity_fn::agg_final::wide_abs(w);
            if !wide_mul_add(&mut abs, m as u64, 0) || abs[3] >= 1 << 62 {
                return 1;
            }
            let out = if neg { neg256(abs) } else { abs };
            core::ptr::write_unaligned(p.cast::<NumWide>(), out);
            0
        }

        /// gravity_fn's `num_sum256` into region `r`, its error kept by name (num_word.rs's `num_sum_ds` rule: stashed for the
        /// driver, 0 returned). # Safety — `r` a live `Region`, `w` four readable words.
        pub(crate) unsafe fn sum_final(r: u64, w: u64, scale: u64) -> u64 {
            let mut e = gravity_fn::ErrSlot::NONE;
            kept(gravity_fn::agg_final::num_sum256(&mut e, r as *mut gravity_fn::Region, w as *const u64, scale as i64), &mut e)
        }

        /// `sum_final` for avg. # Safety — as `sum_final`.
        pub(crate) unsafe fn avg_final(r: u64, w: u64, n: u64, scale: u64) -> u64 {
            let mut e = gravity_fn::ErrSlot::NONE;
            kept(gravity_fn::agg_final::num_avg256(&mut e, r as *mut gravity_fn::Region, w as *const u64, n as i64, scale as i64), &mut e)
        }
    }
}

/// `num_word.rs`'s entries.
pub(crate) mod word {
    /// N13: `num_dyn_key`'s answer for an image it cannot read inline (no order key is this low: typesem numeric `KEY_NINF`
    /// is `i64::MIN + 16`).
    pub(crate) const KEY_TOAST: i64 = i64::MIN + 1;
    use gravity_fn::{agg_final, ErrSlot, Region};
    use gravity_fn::agg_final::{MANT_NAN, MANT_NINF, MANT_PINF, NO_MANTISSA};
    use crate::ops::agg::numeric::num_word::{down, kept};

    gravity_kernels::contract::host! {
        /// The image at `p`'s mantissa at `scale` (`NO_MANTISSA` past it). With `dsp` (D256: a sum over CASE arms of different
        /// dscales) the image's own dscale goes there (0 for a special, which PG's maxScale skips) and a smaller one is widened.
        /// # Safety — `p` is a live numeric varlena, `dsp` null or a writable word.
        pub(crate) unsafe fn mantissa(p: u64, scale: i64, dsp: u64) -> i64 {
            // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
            let dsp = dsp as *mut i64;
            use gravity_fn::agg_final::num_mantissa;
            let p = p as usize as *const u8;
            if dsp.is_null() {
                return num_mantissa(core::ptr::null_mut(), p, scale).val;
            }
            let hdr = match *p {
                1 => return NO_MANTISSA,
                b0 if b0 & 1 != 0 => p.add(1),
                _ if p.cast::<u32>().read_unaligned() & 3 != 0 => return NO_MANTISSA,
                _ => p.add(4),
            };
            // numeric.c's header word: 0xC000 special, 0x8000 short (dscale in bits 7..12), else long (its low 14 bits)
            let h = hdr.cast::<u16>().read_unaligned();
            let d = match h & 0xC000 {
                0xC000 => 0,
                0x8000 => ((h & 0x1F80) >> 7) as i64,
                _ => (h & 0x3FFF) as i64,
            };
            *dsp = d;
            // D4567: a quotient exact at the sum's scale, its display scale past it (decompose `div_scales`)
            if d > scale {
                return gravity_fn::agg_final::num_mantissa_within(p, scale);
            }
            let m = num_mantissa(core::ptr::null_mut(), p, d).val;
            if matches!(m, MANT_NAN | MANT_PINF | MANT_NINF | NO_MANTISSA) {
                return m;
            }
            10i64.pow((scale - d) as u32).checked_mul(m).filter(|v| (MANT_NINF + 1..MANT_PINF).contains(v)).unwrap_or(NO_MANTISSA)
        }

        /// gravity_fn's numeric sum final at `d`, its error kept by name (D299's past-range 22003: gravity_expr's host drops the
        /// message for a replay, so it reached PG's client as "an error without a message"), as `fold_error` stashes it.
        /// # Safety — `r` is a live `Region` (gravity_expr's `num_sum`).
        pub(crate) unsafe fn num_sum_ds(r: u64, lo: u64, hi: u64, scale: u64, d: u64) -> u64 {
            let mut e = ErrSlot::NONE;
            // D4567: a display scale past the sum's (a sum of quotients, decompose `div_scales`): zeros padded
            if d > scale {
                return kept(agg_final::num_sum_pad(&mut e, r as *mut Region, lo as i64, hi as i64, scale as i64, d as i64), &mut e);
            }
            let (lo, hi, d) = down(lo, hi, scale, d);
            kept(agg_final::num_sum(&mut e, r as *mut Region, lo as i64, hi as i64, d as i64), &mut e)
        }

        /// `num_sum_ds` for avg. # Safety — as `num_sum_ds`.
        pub(crate) unsafe fn num_avg_ds(r: u64, lo: u64, hi: u64, n: u64, scale: u64, d: u64) -> u64 {
            let mut e = ErrSlot::NONE;
            if d > scale {
                return kept(agg_final::num_avg_pad(&mut e, r as *mut Region, lo as i64, hi as i64, n as i64, scale as i64, d as i64), &mut e);
            }
            let (lo, hi, d) = down(lo, hi, scale, d);
            kept(agg_final::num_avg(&mut e, r as *mut Region, lo as i64, hi as i64, n as i64, d as i64), &mut e)
        }

        /// N14: a packed code's order key: mantissa `m` at its chunk's elected scale `s` (typesem numeric `order_key_of`, the
        /// key of the image numeric.c writes for it; `NO_KEY` past 16 significant digits or past dscale 15).
        pub(crate) fn num_pack_key(m: i64, s: i64) -> i64 {
            gravity_fn::typesem::numeric::order_key_of(m, s)
        }

        /// NEW-numeric-1: the image at `p`'s order key (typesem numeric `order_key`; `NO_KEY` past 16 significant digits or
        /// past dscale 15; N13: `KEY_TOAST` for a toasted or compressed image, which `num_key_slow` reads).
        /// # Safety — `p` is a live numeric varlena.
        pub(crate) unsafe fn num_dyn_key(p: u64) -> i64 {
            use gravity_fn::typesem::numeric::order_key;
            let p = p as usize as *const u8;
            let (data, len) = match *p {
                1 => return KEY_TOAST,
                b0 if b0 & 1 != 0 => (p.add(1), ((b0 >> 1) as usize).saturating_sub(1)),
                _ => {
                    let h = p.cast::<u32>().read_unaligned();
                    if h & 3 != 0 {
                        return KEY_TOAST;
                    }
                    (p.add(4), ((h >> 2) as usize).saturating_sub(4))
                }
            };
            order_key(core::slice::from_raw_parts(data, len))
        }

        /// NEW-numeric-1: an order key's value as its image (gravity_fn's `num_word` at the key's dscale: the stored digits
        /// and dscale, a special's image). # Safety — `r` is a live `Region`.
        pub(crate) unsafe fn num_dyn_word(r: u64, key: u64) -> u64 {
            let (m, d) = gravity_fn::typesem::numeric::order_key_value(key as i64);
            let mut e = ErrSlot::NONE;
            kept(agg_final::num_word(&mut e, r as *mut Region, m, d), &mut e)
        }
    }

    gravity_kernels::contract::host! {
        /// N13: `num_dyn_key` for an image it could not read inline (a compressed or external one), detoasted as
        /// PG_GETARG_NUMERIC detoasts it: its order key; `NO_KEY` past the key, or when it could not be read (the side step
        /// then reads it again and raises the error). # Safety — `p` is a live numeric varlena.
        pub(crate) unsafe fn num_key_slow(p: u64) -> i64 {
            use gravity_fn::typesem::numeric::{order_key, NO_KEY};
            gravity_fn::with_payload(p as usize as *const u8, order_key).unwrap_or(NO_KEY)
        }

        /// N13: a row's value past the order key folded into the side copy whose word is `side` (0: none; abi `NUM_SIDE_T`
        /// in its low bit): compared by value (abi `numeric_payload_cmp`), it takes the copy on equal too (numeric_smaller /
        /// numeric_larger return their second argument, the later row); a take overwrites the copy in place when it fits,
        /// else copies into the end's image cursor `images` (null: a pinned region of its own), and sets the tie bit. The
        /// new word; 0 when the image could not be read or the pool refused (the error stashed: the IR raises).
        /// # Safety — `p` is a live numeric varlena, `side` 0 or a word of this class, `images` null or an end's cursor.
        pub(crate) unsafe fn num_side_step(ctx: u64, side: u64, images: u64, p: u64, min: u64) -> u64 {
            // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
            let ctx = ctx as *mut gravity_abi::Ctx;
            let images = images as *mut u64;
            match gravity_fn::with_payload(p as usize as *const u8, |pay| side_put(ctx, side, images, pay, min != 0)) {
                Ok(w) => w,
                Err(e) => {
                    gravity_fn::stash_error(e);
                    0
                }
            }
        }

        /// N13: side word `from` (non-zero) merged into `into` (abi `side_fold`, the source holding the later rows, as the
        /// row fold and `Rec::fold_newer` hold them), `key_took` whether the paired key took the source's member.
        /// # Safety — both words 0 or of this class.
        pub(crate) unsafe fn num_side_merge(into: u64, from: u64, key_took: u64, min: u64) -> u64 {
            gravity_abi::breaker::side_fold::<true>(into, from, key_took != 0, min != 0).0
        }

        /// N13: the final over key `key` (valid when `kn` is non-zero) and side word `side` (non-zero): the lower (min) /
        /// higher (max) value; on equal values the member the tie bit names (abi `NUM_SIDE_T`: the side's, else the
        /// key's). The side copy's address, or the key's image rendered in `r` (`num_dyn_word`); 0 on an error (stashed).
        /// # Safety — `r` is a live `Region`, `side` a word of this class.
        pub(crate) unsafe fn num_dyn_side_final(r: u64, key: u64, kn: u64, side: u64, min: u64) -> u64 {
            use core::cmp::Ordering;
            let img = gravity_abi::breaker::side_image(side);
            if kn == 0 {
                return img;
            }
            let kd = num_dyn_word(r, key);
            if kd == 0 {
                return 0;
            }
            let c = match gravity_fn::with_payload(kd as usize as *const u8, |k| gravity_abi::breaker::numeric_payload_cmp(k, copy_payload(img))) {
                Ok(c) => c,
                Err(e) => {
                    gravity_fn::stash_error(e);
                    return 0;
                }
            };
            let key_wins = match c {
                Ordering::Equal => side & gravity_abi::breaker::NUM_SIDE_T == 0,
                c => (c == Ordering::Less) == (min != 0),
            };
            if key_wins { kd } else { img }
        }
    }

    /// N13: a side copy's payload (its 4-byte-header varlena at `img`).
    unsafe fn copy_payload<'a>(img: u64) -> &'a [u8] {
        let p = img as usize as *const u8;
        let len = ((p.cast::<u32>().read_unaligned() >> 2) as usize).saturating_sub(4);
        core::slice::from_raw_parts(p.add(4), len)
    }

    /// N13: payload `pay` folded into the side copy at word `side` (`num_side_step`).
    unsafe fn side_put(ctx: *mut gravity_abi::Ctx, side: u64, images: *mut u64, pay: &[u8], min: bool) -> u64 {
        use core::cmp::Ordering;
        use gravity_abi::breaker::{side_image, NUM_SIDE_T};
        let (cur, need) = (side_image(side), pay.len() + 4);
        if cur == 0 {
            return side_copy(ctx, images, pay, need.max(24));
        }
        if gravity_abi::breaker::numeric_payload_cmp(pay, copy_payload(cur)) == if min { Ordering::Greater } else { Ordering::Less } {
            return side;
        }
        let p = cur as usize as *mut u8;
        let cap = (p.sub(8) as *const u64).read() as usize;
        if need <= cap {
            side_write(p, pay);
            return cur | NUM_SIDE_T;
        }
        side_copy(ctx, images, pay, (2 * cap).max(need))
    }

    /// N13: a side copy of capacity `cap` (rounded to 8) holding `pay`, behind its capacity word: its word (the tie bit set),
    /// 0 when the pool refused (named, D1392 (b)).
    unsafe fn side_copy(ctx: *mut gravity_abi::Ctx, images: *mut u64, pay: &[u8], cap: usize) -> u64 {
        let cap = (cap + 7) & !7;
        let d = crate::ops::agg::trans::code_copy::image_alloc(ctx, images, 8 + cap);
        if d.is_null() {
            gravity_kernels::host_abi::pin_refused(8 + cap);
            return 0;
        }
        (d as *mut u64).write(cap as u64);
        side_write(d.add(8), pay);
        d.add(8) as u64 | gravity_abi::breaker::NUM_SIDE_T
    }

    /// `pay` as a 4-byte-header varlena at `d`.
    unsafe fn side_write(d: *mut u8, pay: &[u8]) {
        core::ptr::write_unaligned(d as *mut u32, ((pay.len() as u32 + 4) << 2).to_le());
        core::ptr::copy_nonoverlapping(pay.as_ptr(), d.add(4), pay.len());
    }
}
