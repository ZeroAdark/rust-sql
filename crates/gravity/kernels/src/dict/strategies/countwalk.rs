//! The count-walk front's host entries (lowerv4 dict/strategies/countwalk.rs, V4R-30; a fused code count + part walk): a
//! window the per-code front takes whole is counted per code into the front's counts (m1_percode.rs's region: no row
//! loop, no touched list, no per-row key image), and at the part's end its live codes are walked in code order: each
//! code's key image from the part's derived Fp128 table and its `f` from the dictionary index (R4: no length table), a
//! run of codes of one key (a byte-rank dictionary keeps a `scheme://host/` prefix contiguous) folded once into the map
//! of the part's per-key records (`fold_code`), which folds on as the per-code front's does. The walk runs when a
//! window of another part comes (`agg_cw_window`, `agg_percode_window`) and at the claim's end (`agg_percode_flush`).
//! A window the host cannot take runs the rows (the per-code front's row path, into the same counts and map): a
//! condition-cache miss window, a code lane that may hold a NULL or has more codes than the counts, and every window of a
//! part whose derived table has a faulted entry (its rows re-evaluate the key and raise where PostgreSQL does).
//! Budget 300 lines (files-extra.tsv, +25 % gate). Owner M2-COUNTWALK.
use gravity_abi::PtrWord;
use gravity_abi::breaker::RecordLayout;
use gravity_abi::ctx::{lane, site_ptr, Lane};
use gravity_abi::Ctx;

use crate::dict::strategies::percode::{flush, fold_code, PC_CODES, PC_COUNTS};
use crate::dict::strategies::percode_const::Verdicts;
use crate::host_abi::{AggState, BIND_FORM_IDX_BYTES, BIND_FORM_IDX_CHARS, BIND_FORM_IDX_DELTA, BIND_FORM_WORDS};
use crate::table::image::AlignedImage;

/// `agg_cw_window`'s flags: the group folds a per-code length, and the pipeline keeps only the codes every one of its
/// verdict tables passes (B3-06, BG-02: `vs`, a `Verdicts` list relative to the key's table, all in the window's one bind
/// out array; a comparison with any constant is one).
pub const CW_LEN: u64 = 2;
pub const CW_VERDICT: u64 = 4;

/// The front header's count-walk words (m1_percode.rs uses words 0 and 1): the counted part + 1 (0: none), the part's
/// derived table, its length index and form, its codes, and `DECLINED` when the part runs its rows (else 0).
const H_PART: usize = 2;
const H_KEYS: usize = 3;
const H_LEN: usize = 4;
const H_FORM: usize = 5;
const H_NCODES: usize = 6;
const H_DECLINED: usize = 7;
const DECLINED: u64 = u64::MAX;

crate::contract::host! {
    /// A window taken whole: rows `cursor .. end` of site `at >> 8`'s window (its selection, else its rows) counted per code of
    /// lane `at & 0xff` into the front's counts, a code whose verdict bit is clear skipped under `CW_VERDICT`; `rows` = the
    /// rows counted. `key`: the bind's out words of the key's derived Fp128 table; `len`:
    /// of the length table (`CW_LEN`); under `CW_VERDICT` the pipeline's per-code verdict table's (a window reading a code whose
    /// verdict entry is faulted, a raise or an entry the rows fill, runs its rows). A window of another part
    /// walks the counted part first (`settle`). 0 taken, 1 the rows run instead, or a negative status (the walk's fold).
    ///
    /// # Safety
    /// `st` is the claim's `AggState`, `front` its per-code front (`agg_percode_open`) or null; the bind words are the window's.
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn agg_cw_window(ctx: *mut Ctx, st: *mut u8, front: *mut u8, at: u64, cursor: u64, end: u64, key: *const u64, len: *const u64, flags: u64, vs: *const i64, ops: u64, rows: *mut u64) -> i64 {
        guard!(gravity_abi::status::ERROR, agg_cw_window_in(ctx, st, front, at, cursor, end, key, len, flags, vs, ops, rows))
    }
}

/// `agg_cw_window`'s body, run under `guard!` (V4R-16 / V4R-40).
#[inline(always)]
#[allow(clippy::too_many_arguments)]
unsafe fn agg_cw_window_in(ctx: *mut Ctx, st: *mut u8, front: *mut u8, at: u64, cursor: u64, end: u64, key: *const u64, len: *const u64, flags: u64, vs: *const i64, ops: u64, rows: *mut u64) -> i64 {
    if front.is_null() {
        return 1;
    }
    // the verdicts' bits and fault words bound (their faulted entries are read per window below)
    let Some(v) = Verdicts::of(key, if flags & CW_VERDICT != 0 { vs } else { core::ptr::null() }) else { return 1 };
    let s = site_ptr(ctx, (at >> 8) as usize);
    let lanes = (*s).lanes.ptr() as *const Lane;
    // a condition-cache miss window publishes survivor words the rows set (m1_percode_const.rs)
    let verdict = lane::id(lane::ID_VERDICT, 0);
    if (0..(*s).nlanes as usize).map(|l| &*lanes.add(l)).any(|l| l.id == verdict && !l.ptr.ptr().is_null() && l.flags & lane::VERDICT_MASK != lane::VERDICT_ALL_MATCH) {
        return 1;
    }
    let c = &*lanes.add((at & 0xff) as usize);
    if c.kind != lane::KIND_CODES || c.width != 4 || c.ptr.ptr().is_null() || (!c.valid.ptr().is_null() && c.flags & lane::FLAG_ALL_VALID == 0) || c.ncodes_or_scale as usize > PC_CODES {
        return 1;
    }
    let (a, hdr, part) = (st as *mut AggState, front as *mut u64, (*s).part_id as u64);
    if *hdr.add(H_PART) != part + 1 {
        if let Err(e) = settle(ctx, a, front, ops, at) {
            return e;
        }
        let (keys, n) = (*key as *const [u64; 2], c.ncodes_or_scale as usize);
        // a faulted entry ([0, 0]: an input the key raised on) re-evaluates per row: the part runs its rows, unless a
        // verdict drops the code's rows before the key is evaluated (the verdicts are the part's, as the key table is)
        let faulted = keys.is_null() || (0..n).any(|i| *keys.add(i) == [0, 0] && v.is_none_or(|v| v.keeps(i as u32)));
        let (idx, form) = if flags & CW_LEN != 0 { (*len, *len.add(2)) } else { (0, u64::MAX) };
        let bad_len = flags & CW_LEN != 0 && (idx == 0 || !matches!(form, BIND_FORM_WORDS | BIND_FORM_IDX_BYTES | BIND_FORM_IDX_DELTA | BIND_FORM_IDX_CHARS));
        *hdr.add(H_PART) = part + 1;
        (*hdr.add(H_KEYS), *hdr.add(H_LEN), *hdr.add(H_FORM), *hdr.add(H_NCODES)) = (keys as u64, idx, form, n as u64);
        *hdr.add(H_DECLINED) = if faulted || bad_len { DECLINED } else { 0 };
    }
    if *hdr.add(H_DECLINED) == DECLINED {
        return 1;
    }
    let counts = front.add(PC_COUNTS) as *mut u32;
    let (codes, sel, (from, to)) = (c.ptr.ptr() as *const u32, (*s).sel.ptr(), (cursor as usize, end as usize));
    let mut n = 0u64;
    // the selection is read as it is: the sparse stage writes the codes of the rows it selects alone (D3179)
    let plain = sel.is_null();
    if let Some(v) = v {
        // the window's own bind words: each verdict's part bits, `code >> 6` the word, `code & 63` the bit. A row whose
        // code's entry is faulted in any (a raise, or a D2074 table the rows fill, every entry faulting until a row filled
        // it) would raise or fill it: the window takes its rows, before anything is counted
        for j in from..to {
            let code = *codes.add(if plain { j } else { *sel.add(j) as usize });
            if v.faulted(code) {
                return 1;
            }
        }
        for j in from..to {
            let code = *codes.add(if plain { j } else { *sel.add(j) as usize });
            let keep = v.keeps(code);
            *counts.add(code as usize) += keep as u32;
            n += keep as u64;
        }
    } else {
        for j in from..to {
            let code = *codes.add(if plain { j } else { *sel.add(j) as usize });
            *counts.add(code as usize) += 1;
        }
        n = to.saturating_sub(from) as u64;
    }
    *rows = n;
    0
}

/// The counted part's walk into the map (see the module doc), its counts cleared: a no-op when no part is counted. The
/// map folds its other part's records first (`flush`).
///
/// # Safety
/// As `agg_cw_window`'s; the counted part's tables are resident for the statement (the part's dictionary index, the
/// statement's derived table).
pub(crate) unsafe fn settle(ctx: *mut Ctx, a: *mut AggState, front: *mut u8, ops: u64, at: u64) -> Result<(), i64> {
    let hdr = front as *mut u64;
    let Some(part) = (*hdr.add(H_PART)).checked_sub(1) else { return Ok(()) };
    *hdr.add(H_PART) = 0;
    if *hdr.add(H_DECLINED) == DECLINED {
        return Ok(());
    }
    if *hdr.add(1) != part {
        flush(ctx, a, front, ops, at)?;
        *hdr.add(1) = part;
    }
    let (layout, nk) = (&*(*a).layout.ptr(), (*(*a).layout.ptr()).nkeys());
    let (keys, idx, form, n) = (*hdr.add(H_KEYS) as *const [u64; 2], *hdr.add(H_LEN) as *const u8, *hdr.add(H_FORM), *hdr.add(H_NCODES) as usize);
    let counts = front.add(PC_COUNTS) as *mut u32;
    // the open run: its image, rows, n x f, first and last code
    let mut run: Option<([u64; 2], i64, i64, u32, u32)> = None;
    for code in 0..n as u32 {
        let c = counts.add(code as usize);
        if *c == 0 {
            continue;
        }
        let (rows, img) = (*c as i64, *keys.add(code as usize));
        *c = 0;
        let f = if idx.is_null() { 0 } else { len_of(idx, form, code) as i64 };
        run = match run {
            Some((k, r, nf, lo, _)) if k == img => Some((k, r + rows, nf + rows * f, lo, code)),
            prev => {
                if let Some((k, r, nf, lo, hi)) = prev {
                    fold_code(ctx, a, front, &image(layout, k, part).0[..nk], ops, at, (r, nf, 0), part as u32, (lo, hi))?;
                }
                Some((img, rows, rows * f, code, code))
            }
        };
    }
    if let Some((k, r, nf, lo, hi)) = run {
        fold_code(ctx, a, front, &image(layout, k, part).0[..nk], ops, at, (r, nf, 0), part as u32, (lo, hi))?;
    }
    Ok(())
}

/// The map record's image of key words `k` (its flags word 0: a listed or walked key is never NULL), the part word last
/// when the layout is parted (partitioned by fewer words than its keys: lowerv4 keyed/parted.rs, V4R-105). 16-byte aligned
/// (table/image.rs, D4691): its pair stores never straddle a page.
#[inline(always)]
pub(crate) fn image(layout: &RecordLayout, k: [u64; 2], part: u64) -> AlignedImage<4> {
    let mut w = AlignedImage([k[0], k[1], 0, 0]);
    if layout.partition_keys() < layout.nkeys() {
        w.0[layout.nkeys() - 1] = part;
    }
    w
}

/// Code `c`'s length from a bind's length table `idx` of form `form` (m1_percode_const.rs `fold`'s reading).
#[inline(always)]
unsafe fn len_of(idx: *const u8, form: u64, c: u32) -> u64 {
    match form {
        BIND_FORM_WORDS => (idx.add(8 * c as usize) as *const u64).read_unaligned(),
        _ => {
            let v = (idx.add(12 * c as usize) as *const u64).read_unaligned();
            let (lo, hi) = (v & 0xffff_ffff, v >> 32);
            match form {
                BIND_FORM_IDX_BYTES => lo,
                BIND_FORM_IDX_DELTA => lo.wrapping_sub(hi) & 0xffff_ffff,
                _ => hi,
            }
        }
    }
}
