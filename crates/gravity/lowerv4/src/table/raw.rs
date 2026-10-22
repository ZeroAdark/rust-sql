//! lowerv4 table's raw half (rule R6: the module's unsafe code lives here): the group key copies' host entries the IR
//! calls, written through the contract's `host!`. key_copy.rs re-exports them, and their readers declare them by address
//! (`declare_host_once`), so the IR's host names are unchanged.
#![allow(unsafe_code)]
use gravity_abi::breaker::{img_bump, T1};
use gravity_abi::ctx::Ctx;

gravity_abi::host! {
    /// M6-TYPES R1c: the copy of the `len`-byte fixed image at `datum` (0: NULL, 0 back), bumped from the images cursor as
    /// `key_copy`'s. Below zero: no room could be pinned. # Safety — `datum` is 0 or `len` readable bytes; `images` the
    /// table's cursor words.
    pub(crate) unsafe fn key_copy_fixed(ctx: u64, images: u64, datum: u64, len: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let ctx = ctx as *mut Ctx;
        let images = images as *mut u64;
        if datum == 0 {
            return 0;
        }
        let need = (len as usize).next_multiple_of(8);
        let src = core::slice::from_raw_parts(datum as usize as *const u8, len as usize);
        // D3312: a table's re-opened group takes the image its bytes already have (`T1::img_cache`, armed by sparse evictions)
        let cache = dedupe_cache(ctx, images);
        let mut entry = None;
        if !cache.is_null() {
            match T1::img_find(cache, T1::img_hash(0, src), src.len(), |q| core::slice::from_raw_parts(q, src.len()) == src) {
                Ok(q) => return q as i64,
                Err(e) => entry = Some(e),
            }
        }
        let images = T1::img_resolve(images); // D1392 (a): a state's words name its table's
        let d = img_bump(images, need, |bytes| gravity_abi::program::pin(ctx, bytes)); // D3312: the arena's one bump
        if d.is_null() { gravity_kernels::host_abi::pin_refused(need); return -1; } // D1392 (b): named, 53200
        core::ptr::copy_nonoverlapping(src.as_ptr(), d, src.len());
        if let Some(e) = entry {
            T1::img_note(cache, e, d, src.len());
        }
        d as i64
    }

    /// The copy of the varlena at `datum` (0: NULL, 0 back), bumped from the images cursor. An inline compressed datum is
    /// kept as it is, as PostgreSQL's `datumCopy` keeps it (D2239: a detoasted copy per group held the payload, ~100x the
    /// compressed bytes); a reader detoasts it as it does a row's (`with_payload`, the lowered `payload`'s bad arm). Any
    /// other form keeps its payload behind a 4-byte header. Below zero: a failed detoast or pin, its error stashed.
    /// # Safety — `datum` is 0 or a live varlena; `images` the table's cursor words.
    pub(crate) unsafe fn key_copy(ctx: u64, images: u64, datum: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let ctx = ctx as *mut Ctx;
        let images = images as *mut u64;
        if datum == 0 {
            return 0;
        }
        let p = datum as usize as *const u8;
        if *p & 3 == 2 {
            // VARATT_IS_4B_C: the 4-byte header's size counts the header and the compressed bytes
            let size = ((p as *const u32).read_unaligned() >> 2) as usize;
            return key_copy_fixed(ctx as u64, images as u64, datum, size as u64);
        }
        let cache = dedupe_cache(ctx, images);
        let copy = gravity_fn::with_payload(p, |x| {
            let need = (4 + x.len()).next_multiple_of(8);
            let head = ((4 + x.len()) as u32) << 2;
            // D3312: a table's re-opened group takes the image its bytes already have (`T1::img_cache`)
            let mut entry = None;
            if !cache.is_null() {
                let same = |q: *const u8| (q as *const u32).read_unaligned() == head && core::slice::from_raw_parts(q.add(4), x.len()) == x;
                match T1::img_find(cache, T1::img_hash(head, x), 4 + x.len(), same) {
                    Ok(q) => return q as i64,
                    Err(e) => entry = Some(e),
                }
            }
            let images = T1::img_resolve(images); // D1392 (a): a state's words name its table's
            let d = img_bump(images, need, |bytes| gravity_abi::program::pin(ctx, bytes)); // D3312: the arena's one bump
            if d.is_null() { gravity_kernels::host_abi::pin_refused(need); return -1; } // D1392 (b): named, 53200
            (d as *mut u32).write_unaligned(head);
            core::ptr::copy_nonoverlapping(x.as_ptr(), d.add(4), x.len());
            if let Some(e) = entry {
                T1::img_note(cache, e, d, 4 + x.len());
            }
            d as i64
        });
        copy.unwrap_or_else(|e| {
            gravity_fn::stash_error(e);
            -1
        })
    }
}

/// D3312: the dedupe cache of the table whose image words `images` names (`T1::img_table`), null for any other cursor or
/// a table not armed. Its pin is the arena's own (`Program.pin`), so the cache lives as long as the images it names.
/// # Safety
/// `images` is null or three live words; `ctx` is bound.
unsafe fn dedupe_cache(ctx: *mut Ctx, images: *mut u64) -> *mut u64 {
    let t = T1::img_table(images);
    if t.is_null() {
        return core::ptr::null_mut();
    }
    T1::img_cache(t, |bytes| gravity_abi::program::pin(ctx, bytes))
}
