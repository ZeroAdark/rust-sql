//! agg's raw half (rules R6 / R8: the family's unsafe code lives here): the aggregate slice's host entries, written
//! through the contract's `host!`, and `like_frame`'s body. entries.rs re-exports the entries under their
//! old path, so the lowerers' addresses and the IR's host names are unchanged.
use gravity_abi::PtrWord;
use gravity_abi::Ctx;

crate::contract::host! {
    /// Road `text.frame` (step/text_frame.rs, M1-STEP): LIKE `pattern` (a varlena pointer; 0 = a NULL pattern) over rows
    /// `[0, rows)` of site `site`'s window on lane `lane` (`KIND_BYREF` width 8: a datum pointer per row). When the valid
    /// rows' images tile (ascending, none reaching into the next: the pgrc lane arena) the pattern's required literal (its
    /// longest run with no `%`, `_` or escape) is found in ONE rare-pair scan of their bytes, each hit mapped to its row by a
    /// forward cursor and confirmed by `Matcher` (a contains pattern's hit is its match), the scan resumed past that row;
    /// otherwise each row is searched on its own. `out[w]`, `w <= rows / 64`: bit `i` = row `i` passes (NOT LIKE when
    /// `negated`), a NULL row 0 in both forms, the bits past `rows` 0. 0 = done; 1 = declined, `out` unspecified: a selection,
    /// another lane kind, an undecoded lane, more words than `cap`, a toasted or compressed image or pattern, no literal, or
    /// a matcher fault (the per-row path raises it only where PG evaluates, D237).
    pub unsafe fn like_frame(ctx: *mut Ctx, site: u64, lane: u64, pattern: u64, negated: u64, out: *mut u64, cap: u64) -> u64 {
        guard!(1, like_frame_in(ctx, site, lane, pattern, negated, out, cap))
    }
}

/// `like_frame`'s body, run under `guard!` (V4R-16): a panic in it declines (1) and the program's caller raises it.
unsafe fn like_frame_in(ctx: *mut Ctx, site: u64, lane: u64, pattern: u64, negated: u64, out: *mut u64, cap: u64) -> u64 {
    use gravity_dict::like::{Class, Matcher, Mode};
    const DECLINED: u64 = 1;
    let s = gravity_abi::ctx::site_ptr(ctx, site as usize);
    let (rows, words) = ((*s).rows as usize, (*s).rows as usize / 64 + 1);
    if !(*s).sel.ptr().is_null() || lane >= (*s).nlanes as u64 || words as u64 > cap {
        return DECLINED;
    }
    let l = gravity_abi::site::lane(s, lane as usize);
    if l.kind != gravity_abi::ctx::lane::KIND_BYREF || l.width != 8 || (l.ptr.ptr().is_null() && rows > 0) {
        return DECLINED;
    }
    let bits = core::slice::from_raw_parts_mut(out, words);
    bits.fill(0);
    if pattern == 0 {
        return 0;
    }
    let Some((ps, pe)) = plain_image(pattern as *const u8) else { return DECLINED };
    let pat = core::slice::from_raw_parts(ps, pe.offset_from(ps) as usize);
    let lit = pat.split(|&c| matches!(c, b'%' | b'_' | b'\\')).max_by_key(|r| r.len()).unwrap_or(&[]);
    let mut scratch = [0u64; 32];
    let mut region = gravity_dict::Region::new(scratch.as_mut_ptr() as *mut u8, 256);
    let (true, Ok(m)) = (!lit.is_empty(), Matcher::new(pat, Mode::Like, &mut region)) else { return DECLINED };
    let whole = m.class() == Class::Contains && m.literal() == lit;
    let pair = gravity_dict::substr::RarePair::new(lit);
    let find = |hay: &[u8], from: usize| match &pair {
        Some(p) => p.find(lit, hay, from),
        None => hay.get(from..)?.iter().position(|&b| b == lit[0]).map(|o| o + from),
    };
    let ptrs = core::slice::from_raw_parts(l.ptr.ptr() as *const u64, rows);
    let data = |i: usize| plain_image(ptrs[i] as *const u8);
    // one pass over the headers: every valid image plain, and whether they tile (ascending, and their span no wider than
    // the images plus alignment, so the scan never reads past them)
    let (mut tiled, mut lo, mut hi, mut bytes) = (true, None, 0usize, 0usize);
    for i in (0..rows).filter(|&i| l.is_valid(i)) {
        let Some((ds, de)) = data(i) else { return DECLINED };
        tiled &= ptrs[i] as usize >= hi;
        lo = lo.or(Some(ds as usize));
        hi = de as usize;
        bytes += de as usize - ptrs[i] as usize + 8;
    }
    tiled &= lo.is_some_and(|lo| hi >= lo && hi - lo <= bytes);
    let mut row = |i: usize, d: &[u8]| -> Result<(), ()> {
        if whole || m.matches(d).map_err(|_| ())? {
            bits[i / 64] |= 1 << (i % 64);
        }
        Ok(())
    };
    if let (true, Some(lo)) = (tiled, lo) {
        let hay = core::slice::from_raw_parts(lo as *const u8, hi - lo);
        let (mut from, mut i) = (0usize, 0usize);
        while let Some(off) = find(hay, from) {
            let h = lo + off;
            while i < rows && (!l.is_valid(i) || data(i).is_some_and(|(_, de)| de as usize <= h)) {
                i += 1;
            }
            if i >= rows {
                break;
            }
            let Some((ds, de)) = data(i) else { return DECLINED };
            if (ds as usize) <= h && h + lit.len() <= de as usize {
                if row(i, core::slice::from_raw_parts(ds, de.offset_from(ds) as usize)).is_err() {
                    return DECLINED;
                }
                from = de as usize - lo;
                i += 1;
            } else {
                from = off + 1;
            }
        }
    } else {
        for i in (0..rows).filter(|&i| l.is_valid(i)) {
            let Some((ds, de)) = data(i) else { return DECLINED };
            let d = core::slice::from_raw_parts(ds, de.offset_from(ds) as usize);
            if find(d, 0).is_some() && row(i, d).is_err() {
                return DECLINED;
            }
        }
    }
    if negated != 0 {
        for (w, b) in bits.iter_mut().enumerate() {
            let valid = if l.valid.ptr().is_null() { !0 } else if w * 64 < rows { *l.valid.ptr().add(w) } else { 0 };
            let live = if (w + 1) * 64 <= rows { !0 } else { (1u64 << (rows - (w * 64).min(rows))) - 1 };
            *b = !*b & valid & live;
        }
    }
    0
}

/// The data bytes `[start, end)` of the varlena image at `p` when it is stored plainly (a 1-byte or an uncompressed
/// 4-byte header), `None` when toasted or compressed.
unsafe fn plain_image(p: *const u8) -> Option<(*const u8, *const u8)> {
    let b0 = *p;
    if b0 & 1 == 1 {
        return (b0 != 1).then(|| (p.add(1), p.add((b0 >> 1) as usize)));
    }
    let h = u32::from_le_bytes([b0, *p.add(1), *p.add(2), *p.add(3)]);
    (h & 3 == 0).then(|| (p.add(4), p.add((h >> 2) as usize)))
}
