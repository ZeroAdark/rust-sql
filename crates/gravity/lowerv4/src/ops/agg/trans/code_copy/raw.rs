//! code_copy.rs's unsafe half: the three host entries its IR calls (`contract::host` functions, no landing pad) and the
//! copy work under them, over the slot's varlena copy and its three-word header (capacity, winner word, losing
//! threshold) and the end's image cursor.
#![allow(unsafe_code)]
use core::cmp::Ordering;

use gravity_abi::lane::EMPTY_CODE_NONE;
use gravity_abi::Ctx;
use gravity_kernels::dict::strategies::percode::{HDR, NO_THR};

use super::{gains, IMAGE};

gravity_kernels::contract::host! {
    /// The better by bytes of copy `src` (never 0) and the carry `(cur, at, n)`: none when `n` is 0, a copy's address when
    /// `at` is `IMAGE`, else code `cur` rendered through site / lane `at` (a tie keeps the carry's): its address, 0 when the
    /// dictionary read failed (its error stashed, D344). # Safety — `src` (and `cur` under `IMAGE`) a live copy of this
    /// class, else `at` a bound site's code lane.
    pub(super) unsafe fn pick(ctx: u64, cur: u64, at: u64, n: u64, src: u64, min: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let ctx = ctx as *mut Ctx;
        let cur = match (n, at == IMAGE as u64) {
            (0, _) => return src as i64,
            (_, true) => cur as usize as *const u8,
            _ => match gravity_kernels::dict::reps::render(gravity_abi::ctx::site_ptr(ctx, (at >> 8) as usize), (at & 0xff) as u8, cur) {
                Ok(Some(img)) => img,
                Ok(None) => return src as i64,
                Err(_) => return 0,
            },
        };
        let o = gravity_kernels::dict::reps::varlena_bytes(src as usize as *const u8).cmp(gravity_kernels::dict::reps::varlena_bytes(cur));
        (if o == Ordering::Less && min != 0 || o == Ordering::Greater && min == 0 { src } else { cur as u64 }) as i64
    }

    /// Folds row word `word` (site / lane `at`) into the copy at `slot` (0: none yet): a row no better than the winner of its
    /// part, or than the part's losing threshold, returns at once; else the row's image is rendered and compared by bytes
    /// (C collation's order, abi `varlena_cmp`) unless it is the winner's part; a win overwrites the copy (or copies), a loss
    /// becomes the part's threshold. 1 (the slot holds an image), 2 (the code renders NULL, nothing stored), 0 (the pinned
    /// pool refused or the dictionary read failed, its error stashed: raised at once, never a NULL, D344). # Safety — `slot` is a live record's `MinVar` / `MaxVar` word holding 0 or a copy of
    /// this class, `images` null or an end's cursor words, `at` a bound site's code lane.
    pub(super) unsafe fn fold_row(ctx: *mut Ctx, slot: *mut u64, images: *mut u64, at: u64, word: u64, min: u64) -> i64 {
        let (min, cur) = (min != 0, slot.read_unaligned());
        if cur != 0 && loses(cur, word, min) {
            return 1;
        }
        let img = match gravity_kernels::dict::reps::render(gravity_abi::ctx::site_ptr(ctx, (at >> 8) as usize), (at & 0xff) as u8, word) {
            Ok(Some(img)) => img,
            Ok(None) => return 2,
            Err(_) => return 0,
        };
        put(ctx, slot, images, gravity_kernels::dict::reps::varlena_bytes(img), word, min)
    }

    /// Folds the copy at `src` (a record slot's image of this class, 0: none) into the copy at `slot` (a record merge).
    pub(super) unsafe fn fold_copy(ctx: *mut Ctx, slot: *mut u64, images: *mut u64, src: u64, min: u64) -> i64 {
        if src == 0 {
            return 2;
        }
        // compared by bytes: the source's winner word is not read (an uncoded word never takes the part's fast path)
        put(ctx, slot, images, gravity_kernels::dict::reps::varlena_bytes(src as usize as *const u8), EMPTY_CODE_NONE as u64, min != 0)
    }
}

/// Whether row word `word` loses to the copy at `cur` without its image: no better than the winner of its own part, or
/// than its part's losing threshold (codes of one part order as their bytes).
pub(super) unsafe fn loses(cur: u64, word: u64, min: bool) -> bool {
    let (p, c) = ((word >> 32) as u32, word as u32);
    let same = |w: u64| c != EMPTY_CODE_NONE && (w >> 32) as u32 == p && w as u32 != EMPTY_CODE_NONE;
    let h = cur as *const u64;
    let (win, thr) = (*h.sub(2), *h.sub(1));
    same(win) && !gains(min, c, win as u32) || thr != NO_THR && same(thr) && !gains(min, c, thr as u32)
}

/// Folds `new` (the winner word `word`) into the copy at `slot`: compared by bytes unless `coded` said it is the winner's
/// part and better; see `fold_row`.
pub(super) unsafe fn put(ctx: *mut Ctx, slot: *mut u64, images: *mut u64, new: &[u8], word: u64, min: bool) -> i64 {
    let cur = slot.read_unaligned();
    if cur == 0 {
        return copy(ctx, slot, images, new, new.len() + 4, word, NO_THR);
    }
    let h = cur as *mut u64;
    let (win, p, c) = (*h.sub(2), (word >> 32) as u32, word as u32);
    let same_part = c != EMPTY_CODE_NONE && (win >> 32) as u32 == p && win as u32 != EMPTY_CODE_NONE;
    if !same_part || !gains(min, c, win as u32) {
        let o = new.cmp(gravity_kernels::dict::reps::varlena_bytes(cur as usize as *const u8));
        if !(o == Ordering::Less && min || o == Ordering::Greater && !min) {
            if c != EMPTY_CODE_NONE && !same_part {
                *h.sub(1) = word;
            }
            return 1;
        }
    }
    let cap = *h.sub(3) as usize;
    if new.len() + 4 <= cap {
        write_image(cur as *mut u8, new);
        *h.sub(2) = word;
        return 1;
    }
    copy(ctx, slot, images, new, (2 * cap).max(new.len() + 4), word, *h.sub(1))
}

/// A copy of capacity `cap` (rounded to 8) holding `new`, its header `(cap, win, thr)`, its varlena's address at `slot`.
unsafe fn copy(ctx: *mut Ctx, slot: *mut u64, images: *mut u64, new: &[u8], cap: usize, win: u64, thr: u64) -> i64 {
    let cap = (cap + 7) & !7;
    let d = alloc(ctx, images, HDR + cap);
    if d.is_null() {
        gravity_kernels::host_abi::pin_refused(HDR + cap); // D1392 (b)
        return 0;
    }
    let h = d as *mut u64;
    (h.write(cap as u64), h.add(1).write(win), h.add(2).write(thr));
    write_image(d.add(HDR), new);
    slot.write_unaligned(d.add(HDR) as u64);
    1
}

/// `new` as a 4-byte-header varlena at `d`.
pub(super) unsafe fn write_image(d: *mut u8, new: &[u8]) {
    core::ptr::write_unaligned(d as *mut u32, ((new.len() as u32 + 4) << 2).to_le());
    core::ptr::copy_nonoverlapping(new.as_ptr(), d.add(4), new.len());
}

/// `need` bytes (a multiple of 8) that outlive the Call (N13: class_num's side copies take them too): bumped from the end's cursor `images` (`cur, end, chunk`, as the
/// keyed ends' `AggState` arena: chunks from 64 KiB doubling to 32 MiB), or a pinned region of their own (null cursor).
pub(crate) unsafe fn alloc(ctx: *mut Ctx, images: *mut u64, need: usize) -> *mut u8 {
    if images.is_null() {
        return gravity_abi::program::pin(ctx, need as u64);
    }
    let images = gravity_abi::breaker::T1::img_resolve(images); // D1392 (a): a state's words name its table's
    gravity_abi::breaker::img_bump(images, need, |bytes| gravity_abi::program::pin(ctx, bytes)) // D3312: the arena's one bump
}
