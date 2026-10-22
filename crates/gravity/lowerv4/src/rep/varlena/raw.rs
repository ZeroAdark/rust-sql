//! rep/varlena's unsafe host entries, the module's one home of unsafe code: the varlena compares, copy, numeric
//! arithmetic and order prefix, moved unchanged from the files named on each module below, which import them back, so
//! the addresses the IR takes, host names and IR are unchanged.
#![allow(unsafe_code)]

/// CLASS E: one value's heap copy (a detoasted payload, a re-headered array image) is charged to the
/// owner of the breaker slot the running pipeline feeds, for the copy's life: a memledger `ChargedVec` (door 1: its
/// capacity committed to the owner, given back with the data), filled with the same `Vec` calls as before (`with_vec`),
/// so the copy's bytes, contents and allocation failure are as they were. A copy under `VALUE_FLOOR` (256 KiB) stays
/// uncharged: no host call, no atomics on the per-row paths.
pub(crate) mod value {
    use gravity_fn::arena::{ChargedVec, Kind, OpArena, Owner, VALUE_FLOOR};

    /// The owner a copy of `bytes` is charged to and its kind lane (`OpArena::slot_owner_kind`: the produced slot's breaker
    /// and its kind, else the statement's owner and `Kind::Region`); `None` under the floor, with no ctx (0: a test) or no
    /// owner (no host, a rig).
    pub(crate) fn owner(ctx: u64, bytes: usize) -> Option<(Owner, Kind)> {
        if bytes < VALUE_FLOOR || ctx == 0 {
            return None;
        }
        OpArena::slot_owner_kind(ctx as usize as *mut gravity_abi::ctx::Ctx)
    }

    /// An empty buffer for one value's copy of `bytes`, charged to `ctx`'s owner (`owner`) as it grows.
    pub(crate) fn vec(ctx: u64, bytes: usize) -> ChargedVec<u8> {
        let o = owner(ctx, bytes);
        ChargedVec::new(o.map(|o| o.0), o.map_or(Kind::Region, |o| o.1))
    }

    /// `parts` end to end in one value's buffer (`vec`), its capacity exactly their sum (as `to_vec` / `concat` gave).
    pub(crate) fn copy_of(ctx: u64, parts: &[&[u8]]) -> ChargedVec<u8> {
        let n = parts.iter().map(|p| p.len()).sum();
        let mut v = vec(ctx, n);
        v.with_vec(|v| {
            v.reserve_exact(n); // alloc: Cold charged owner(ctx, n)
            parts.iter().for_each(|p| v.extend_from_slice(p)); // alloc: Cold charged owner(ctx, n)
        });
        v
    }
}

/// `cmp_num.rs`'s entries.
pub(crate) mod cmp_num {
    use gravity_fn::agg_final::numeric_cmp;

    gravity_kernels::contract::host! {
        /// `cmp_numerics(a, b)` of two numeric datums (detoasted): -1 / 0 / 1, or `i64::MIN` with the detoast's error stashed.
        /// # Safety — `a` and `b` are live numeric varlenas.
        pub(crate) unsafe fn num_cmp_cold(a: u64, b: u64) -> i64 {
            let img = |d: u64| gravity_fn::with_payload(d as usize as *const u8, |x| x.to_vec()); // alloc: Cold
            match (img(a), img(b)) {
                (Ok(x), Ok(y)) => numeric_cmp(core::ptr::null_mut(), gravity_fn::View::of(&x), gravity_fn::View::of(&y)).val,
                (Err(e), _) | (_, Err(e)) => (gravity_fn::stash_error(e), i64::MIN).1,
            }
        }
    }
}

/// `cmp_text.rs`'s entries.
pub(crate) mod cmp_text {
    use gravity_expr::nodes::array::{ElemFacts, Walk};
    use super::super::cmp_text::trim;
    use super::value;

    gravity_kernels::contract::host! {
        /// Whether the (detoasted) payloads of `datum` and the constant `needle` are equal (bpchar: without trailing blanks):
        /// 1 / 0, or -1 with the detoast's error stashed. The datum's copy is charged to `ctx`'s owner (`value`).
        /// # Safety — both are live varlenas; `ctx` is 0 or the caller's bound ctx.
        pub(crate) unsafe fn text_eq_cold(ctx: u64, datum: u64, needle: u64, bpchar: u64) -> i64 {
            let a = match gravity_fn::with_payload(datum as usize as *const u8, |a| value::copy_of(ctx, &[trim(a, bpchar != 0)])) {
                Ok(a) => a,
                Err(e) => return (gravity_fn::stash_error(e), -1).1,
            };
            match gravity_fn::with_payload(needle as usize as *const u8, |n| trim(n, bpchar != 0) == a.as_slice()) {
                Ok(eq) => eq as i64,
                Err(e) => (gravity_fn::stash_error(e), -1).1,
            }
        }

        /// `datum = ANY(array)` over varlena elements aligned `align`: 1 / 0, 2 when none matched and an element is NULL, or -1
        /// with the detoast's error stashed. `datum` 0 is NULL: 0 over an empty array, else 2 (ExecEvalScalarArrayOp tests the
        /// item count before the strict operand). The array's copy is charged to `ctx`'s owner (`value`).
        /// # Safety — `array` is a live varlena, `datum` 0 or one; `ctx` is 0 or the caller's bound ctx.
        pub(crate) unsafe fn text_any_eq(ctx: u64, datum: u64, array: u64, align: u64) -> i64 {
            let (arr, elem) = (array as usize as *const u8, ElemFacts { typ: 25, len: -1, byval: false, align: align as i8 });
            // a plain image walks in place; any other is copied out plain (a bound literal is plain)
            let copy = match *arr & 3 {
                0 => None,
                _ => match gravity_fn::with_payload(arr, |img| value::copy_of(ctx, &[&((img.len() as u32 + 4) << 2).to_le_bytes()[..], img])) {
                    Ok(v) => Some(v),
                    Err(e) => return (gravity_fn::stash_error(e), -1).1,
                },
            };
            let img = copy.as_ref().map_or(arr, |v| v.as_ptr());
            let walk = || Walk::over(gravity_fn::Datum::from_usize(img as usize), elem, &mut gravity_fn::Region::over(&mut []));
            if datum == 0 {
                return match walk() {
                    Ok(w) => if w.is_empty() { 0 } else { 2 },
                    Err(e) => (gravity_fn::stash_error(e), -1).1,
                };
            }
            let r = gravity_fn::with_payload(datum as usize as *const u8, |a| {
                let mut w = walk()?;
                let mut null = false;
                while let Some(e) = w.next() {
                    match e.isnull {
                        true => null = true,
                        false if gravity_fn::with_payload(e.value.as_usize() as *const u8, |n| n == a)? => return Ok(1),
                        false => {}
                    }
                }
                Ok(if null { 2 } else { 0 })
            });
            match r.and_then(|r| r) {
                Ok(r) => r,
                Err(e) => (gravity_fn::stash_error(e), -1).1,
            }
        }
    }
}

/// `text_view.rs`'s entries (D3151).
pub(crate) mod text_view {
    use gravity_expr::nodes::array::{ElemFacts, Walk};
    use super::super::cmp_text::trim;
    use super::value;

    /// `array`'s text elements (aligned `align`) walked against `eq`: 1 at the first match, 2 when none matched and an
    /// element is NULL, 0 otherwise. An array image that is not plain is copied out plain first (a bound literal is plain),
    /// the copy charged to `ctx`'s owner (`value`).
    fn any_of(ctx: u64, array: u64, align: u64, eq: &mut dyn FnMut(&[u8]) -> bool) -> Result<i64, Box<types_error::PgError>> {
        let (arr, elem) = (array as usize as *const u8, ElemFacts { typ: 25, len: -1, byval: false, align: align as i8 });
        // SAFETY: `array` is a live varlena (the caller's contract).
        let copy = match unsafe { *arr } & 3 {
            0 => None,
            _ => Some(unsafe { gravity_fn::with_payload(arr, |img| value::copy_of(ctx, &[&((img.len() as u32 + 4) << 2).to_le_bytes()[..], img])) }?),
        };
        let img = copy.as_ref().map_or(arr, |v| v.as_ptr());
        let mut w = Walk::over(gravity_fn::Datum::from_usize(img as usize), elem, &mut gravity_fn::Region::over(&mut []))?;
        let mut null = false;
        while let Some(e) = w.next() {
            match e.isnull {
                true => null = true,
                // SAFETY: a non-NULL element of a live array is a live varlena.
                false if unsafe { gravity_fn::with_payload(e.value.as_usize() as *const u8, |n| eq(n)) }? => return Ok(1),
                false => {}
            }
        }
        Ok(if null { 2 } else { 0 })
    }

    gravity_kernels::contract::host! {
        /// `view = ANY(array)` for the `len` bytes at `data` (a view of a plain image, never NULL) over text elements
        /// aligned `align`: 1 / 0, 2 when none matched and an element is NULL, or -1 with the array's detoast error
        /// stashed. # Safety — `data` is readable for `len` bytes, `array` is a live varlena; `ctx` is 0 or the caller's
        /// bound ctx.
        pub(crate) unsafe fn text_any_view(ctx: u64, data: u64, len: u64, array: u64, align: u64) -> i64 {
            let v = if len == 0 { &[][..] } else { core::slice::from_raw_parts(data as usize as *const u8, len as usize) };
            match any_of(ctx, array, align, &mut |n| n == v) {
                Ok(r) => r,
                Err(e) => (gravity_fn::stash_error(e), -1).1,
            }
        }

        /// The cold view (text_view.rs `compare`): `datum` (0: NULL) detoasted, trimmed of its trailing blanks when `form`
        /// bit 0 says so, cut by PostgreSQL's text_substring when bit 1 does (`start`, and `count` when bit 2 does, else
        /// to the end), then compared with the constant `needle`: equal (bit 3 clear), or `= ANY` of it (bit 3, the
        /// elements aligned by `form >> 8`). 1 / 0, 2 for NULL (an `= ANY` of a NULL datum over a non-empty array, or no
        /// match beside a NULL element), or -1 with the error stashed (a detoast's, text_substring's negative count).
        /// A copy (the array's, the substring's whole value) is charged to `ctx`'s owner (`value`).
        /// # Safety — `datum` is 0 or a live varlena, `needle` a live varlena; `ctx` is 0 or the caller's bound ctx.
        pub(crate) unsafe fn text_view_cold(ctx: u64, datum: u64, needle: u64, form: u64, start: u64, count: u64) -> i64 {
            let (any, align) = (form & 8 != 0, form >> 8);
            if datum == 0 {
                // a NULL datum reaches here only under `= ANY` (an empty array is false, any other NULL)
                let mut seen = false;
                let r = any_of(ctx, needle, align, &mut |_| { seen = true; false });
                return match r {
                    Ok(r) => if seen || r == 2 { 2 } else { 0 },
                    Err(e) => (gravity_fn::stash_error(e), -1).1,
                };
            }
            let cut = |v: &[u8]| -> Result<i64, Box<types_error::PgError>> {
                let mut test = |x: &[u8]| -> Result<i64, Box<types_error::PgError>> {
                    match any {
                        true => any_of(ctx, needle, align, &mut |n| n == x),
                        false => gravity_fn::with_payload(needle as usize as *const u8, |n| (n == x) as i64),
                    }
                };
                match form & 2 != 0 {
                    true => gravity_fn::shims::text::substring_of(v, start as i32, count as i32, form & 4 == 0, value::owner(ctx, v.len() + 4), |x| test(x))?,
                    false => test(v),
                }
            };
            let r = gravity_fn::with_payload(datum as usize as *const u8, |a| cut(trim(a, form & 1 != 0)));
            match r.and_then(|r| r) {
                Ok(r) => r,
                Err(e) => (gravity_fn::stash_error(e), -1).1,
            }
        }
    }
}

/// `text_set.rs`'s entry (road text.set, textkeys step 2).
pub(crate) mod text_set {
    use super::super::cmp_text::trim;
    use super::value;

    gravity_kernels::contract::host! {
        /// expr.saop's byte set `set` (step/raw/saop_bytes.rs) answered for the view `form` names of `datum` (0: NULL):
        /// trimmed of its trailing blanks when bit 0 says so, cut by PostgreSQL's text_substring when bit 1 does (`start`,
        /// and `count` when bit 2 does, else to the end), then probed; bit 4 `= ANY` (else `<> ALL`). 1 / 0, 2 for NULL
        /// (a NULL datum: the list is never empty; no match beside a NULL element), or -1 with the error stashed (a
        /// detoast's, text_substring's negative count). A substring's copy is charged to `ctx`'s owner (`value`).
        /// # Safety — `datum` is 0 or a live varlena, `set` a region `build_bytes` built; `ctx` is 0 or the caller's
        /// bound ctx.
        pub(crate) unsafe fn text_set_view(ctx: u64, datum: u64, set: u64, form: u64, start: u64, count: u64) -> i64 {
            if datum == 0 {
                return 2;
            }
            let any = form & 16 != 0;
            let test = |x: &[u8]| -> Result<i64, Box<types_error::PgError>> {
                let (found, nulls) = crate::step::raw::saop_bytes::probe_payload(set, x);
                Ok(if found { any as i64 } else if nulls { 2 } else { !any as i64 })
            };
            let cut = |v: &[u8]| -> Result<i64, Box<types_error::PgError>> {
                match form & 2 != 0 {
                    true => gravity_fn::shims::text::substring_of(v, start as i32, count as i32, form & 4 == 0, value::owner(ctx, v.len() + 4), |x| test(x))?,
                    false => test(v),
                }
            };
            let r = gravity_fn::with_payload(datum as usize as *const u8, |a| cut(trim(a, form & 1 != 0)));
            match r.and_then(|r| r) {
                Ok(r) => r,
                Err(e) => (gravity_fn::stash_error(e), -1).1,
            }
        }
    }
}

/// `datum.rs`'s entries.
pub(crate) mod datum {
    gravity_kernels::contract::host! {
        /// `len` bytes from `src` to `dst`. # Safety — `src` readable and `dst` writable for `len` bytes, not overlapping (the
        /// destination is freshly allocated in the consumer's home).
        pub(crate) unsafe fn keep_copy(dst: u64, src: u64, len: u64) {
            core::ptr::copy_nonoverlapping(src as usize as *const u8, dst as usize as *mut u8, len as usize);
        }
    }
}

/// `num_image.rs`'s entries.
pub(crate) mod num_image {
    gravity_kernels::contract::host! {
        /// The shim's `a op b` (PostgreSQL's numeric_mul / add / sub over the detoasted images) into the 64-byte `slot`: the
        /// datum, or 0 with the error stashed. # Safety — `a`, `b` live numeric varlenas, `slot` 64 writable bytes.
        pub(crate) unsafe fn num_arith_cold(slot: u64, a: u64, b: u64, op: u64) -> i64 {
            use gravity_fn::shims::byref_ref::{numeric_add, numeric_mul, numeric_sub};
            const MUL: u64 = gravity_decompose::catalog::numeric::F_NUMERIC_MUL as u64;
            const ADD: u64 = gravity_decompose::catalog::numeric::F_NUMERIC_ADD as u64;
            let f = match op { MUL => numeric_mul, ADD => numeric_add, _ => numeric_sub };
            let img = |d: u64| gravity_fn::with_payload(d as usize as *const u8, |x| x.to_vec()); // alloc: Cold
            let (x, y) = match (img(a), img(b)) {
                (Ok(x), Ok(y)) => (x, y),
                (Err(e), _) | (_, Err(e)) => return (gravity_fn::stash_error(e), 0).1,
            };
            let (mut e, mut rg) = (gravity_fn::ErrSlot::NONE, gravity_fn::Region::over(core::slice::from_raw_parts_mut(slot as usize as *mut u8, 64)));
            let r = f(&mut e, &mut rg, gravity_fn::View::of(&x), gravity_fn::View::of(&y));
            if r.status == gravity_fn::OK {
                return r.val;
            }
            let err = e.take().unwrap_or_else(|| Box::new(gravity_fn::PgError::error("gravity: num.arith result past its slot"))); // alloc: Cold
            (gravity_fn::stash_error(err), 0).1
        }
    }
}

/// `order.rs`'s entries.
pub(crate) mod order {
    gravity_kernels::contract::host! {
        /// The first 8 of the `len` bytes at `p`, big-endian, zero-padded, top bit flipped (rep.code.order's `prefix8` over a
        /// payload). # Safety — `p` is readable for `len` bytes.
        pub(crate) unsafe fn prefix8(p: *const u8, len: u64) -> u64 {
            let mut w = [0u8; 8];
            let n = (len as usize).min(8);
            core::ptr::copy_nonoverlapping(p, w.as_mut_ptr(), n);
            u64::from_be_bytes(w) ^ (1 << 63)
        }
    }
}
