//! M6-CODES' code.heads host (V4R-310; lowerv4 runs/heads.rs): an entries row's multiplicity, the rows of its code in its
//! part, counted through the count site (the key column's code lane over rows) from the part's first granule until the
//! part changes (v2 hosts/heads.rs `count`). D3096: the second entry of a part counts every code of the part in that one
//! pass into a table pinned for the statement (one slot per dictionary code, the last the part's NULL rows), so every
//! later entry of the part is a lookup: a deep OFFSET passes many entries, each once a full part scan. A part's first
//! entry counts its one code (the vectorized compare): a LIMIT that reads one entry a part never builds a table. The
//! tables hang off a directory behind the emit's memo word (a frame word the execution's reset zeroes). D3027: a skewed
//! part (its rows often repeat the row before: a text column's frequent '') counts into four interleaved tables, folded
//! into its table at the pass's end, so an increment does not wait on the store of the one before. D3027 (B): at width
//! 2 or more a count pass between the top and its emit (lowerv4 heads.rs) counts each part the top kept on the pool,
//! one whole part per claim (`heads_tally`), into a directory the emit reads first (`counted`). A #[path] submodule of
//! host_abi/v4/m6.rs.
use gravity_abi::PtrWord;
use gravity_abi::status::ERROR;
use gravity_abi::Ctx;

use crate::util;

/// A mark's code for its part's NULL row (lowerv4 heads.rs): the part's rows whose key is NULL (no dictionary code: pgrc
/// caps a part's codes below 2^24).
pub const NULL_CODE: u32 = u32::MAX;

crate::contract::host! {
    /// The rows of `mark`'s code in its part (`NULL_CODE`: its NULL rows): from the count pass's table for the part in
    /// `dir` (`heads_tally`) when it has one, else from the part's table behind `memo` (built by one pass of count site
    /// `k` over the part's granules on the part's first entry); a null `memo`, a binding without a pin or a part whose
    /// code lane names no code count counts the one code by that pass instead.
    pub unsafe fn heads_count(ctx: u64, k: u64, mark: u64, memo: u64, dir: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let ctx = ctx as *mut Ctx;
        let memo = memo as usize as *mut u64;
        guard!(ERROR, match counted(dir as usize as *const u64, mark) {
            Some(n) => n,
            None => count(ctx, k as usize, mark, memo),
        })
    }

    /// D3027 (B): the count pass's window of site `k` counted into its part's table in `dir` (the pass's directory,
    /// `PASS_DIR_HDR`), the table pinned at the part's first window; a part whose code lane names no code count, or holds
    /// a code past it, takes none (`NO_TABLE`: its emit counts as without the pass). 0, or the error status.
    pub unsafe fn heads_tally(ctx: u64, k: u64, dir: u64) -> i64 {
        let ctx = ctx as *mut Ctx;
        guard!(ERROR, tally_window(ctx, k as usize, dir as usize as *mut u64))
    }
}

/// The count pass's directory (exec's code.heads protocol lays it out, one entry per part of the execution's part list,
/// in its order, so first granules ascend): `[n, (first granule, slots, table) x n]`; `slots` 0 until a worker counts the
/// part (its claims are whole parts: one writer per entry), `NO_TABLE` for a part that takes no table.
pub const PASS_DIR_HDR: usize = 1;
pub const PASS_DIR_ENTRY: usize = 3;
/// D4554: an entry's `slots` with this bit is a SPARSE table the protocol filed from the part's kept counts (no claim
/// counts the part): `slots & !PASS_SPARSE` words `(code << 32) | rows`, codes ascending; a code not in it counts by the
/// emit's own pass.
pub const PASS_SPARSE: u64 = 1 << 62;
/// An entry's `slots` for a part that takes no table (`NO_TABLE`).
pub const PASS_NO_TABLE: u64 = NO_TABLE as u64;

/// `code`'s rows in a dense count table of `slots` (`heads_tally`'s: the last slot the part's NULL rows).
///
/// # Safety
/// `t` points at `slots` (at least 1) readable words.
pub unsafe fn pass_slot(t: *const u32, slots: usize, code: u32) -> i64 {
    slot(t, slots, code)
}

/// The rows of `mark`'s code from the count pass's table for its part, when the pass counted it.
unsafe fn counted(dir: *const u64, mark: u64) -> Option<i64> {
    if dir.is_null() {
        return None;
    }
    let (start, n) = ((mark >> 32) & 0x7fff_ffff, *dir as usize);
    let e = |i: usize| dir.add(PASS_DIR_HDR + i * PASS_DIR_ENTRY);
    let (mut lo, mut hi) = (0, n);
    while lo < hi {
        let m = (lo + hi) / 2;
        if *e(m) < start { lo = m + 1 } else { hi = m }
    }
    if lo == n || *e(lo) != start {
        return None;
    }
    let x = e(lo);
    match *x.add(1) as usize {
        slots if slots != NO_TABLE && slots as u64 & PASS_SPARSE != 0 => sparse(*x.add(2) as *const u64, (slots as u64 & !PASS_SPARSE) as usize, mark as u32),
        slots if slots != SEEN && slots != NO_TABLE => Some(slot(*x.add(2) as *const u32, slots, mark as u32)),
        _ => None,
    }
}

/// D4554: `code`'s rows in a sparse table of `n` words (`PASS_SPARSE`), when it holds the code.
unsafe fn sparse(t: *const u64, n: usize, code: u32) -> Option<i64> {
    let (mut lo, mut hi) = (0, n);
    while lo < hi {
        let m = (lo + hi) / 2;
        match ((*t.add(m) >> 32) as u32).cmp(&code) {
            core::cmp::Ordering::Less => lo = m + 1,
            core::cmp::Ordering::Greater => hi = m,
            core::cmp::Ordering::Equal => return Some((*t.add(m) & 0xffff_ffff) as i64),
        }
    }
    None
}

/// `heads_tally`'s window: its rows from the site's cursor (its selection's, a NULL row into the table's last slot).
unsafe fn tally_window(ctx: *mut Ctx, k: usize, dir: *mut u64) -> i64 {
    let s = util::site(ctx, k);
    let part = s.part_id as usize;
    if dir.is_null() || part >= *dir as usize {
        return 0;
    }
    let e = dir.add(PASS_DIR_HDR + part * PASS_DIR_ENTRY);
    // D4554: a sparse entry is the protocol's (no claim counts its part)
    if *e.add(1) as usize == NO_TABLE || *e.add(1) & PASS_SPARSE != 0 {
        return 0;
    }
    if (*s.lanes.ptr()).ptr.ptr().is_null() && gravity_abi::site::lane_fill(s, 0) < 0 {
        return ERROR;
    }
    let l = &*s.lanes.ptr();
    let (t, slots) = match *e.add(2) as *mut u32 {
        t if !t.is_null() => (t, *e.add(1) as usize),
        _ => {
            let ncodes = l.ncodes_or_scale as usize;
            if ncodes == 0 {
                *e.add(1) = NO_TABLE as u64;
                return 0;
            }
            let bytes = 4 * (ncodes + 1);
            let t = gravity_abi::program::pin_statement(ctx, bytes as u64) as *mut u32;
            if t.is_null() {
                crate::host_abi::pin_refused(bytes);
                return ERROR;
            }
            (*e.add(1), *e.add(2)) = ((ncodes + 1) as u64, t as u64);
            (t, ncodes + 1)
        }
    };
    let past = if s.sel.ptr().is_null() && l.valid.ptr().is_null() && l.width == 4 {
        let (from, to) = (s.cursor as usize, s.rows as usize);
        !tally_runs(t, slots, core::slice::from_raw_parts((l.ptr.ptr() as *const u32).add(from), to.saturating_sub(from)))
    } else {
        let at = |i: usize| match l.width { 2 => *(l.ptr.ptr() as *const u16).add(i) as u32, 8 => *(l.ptr.ptr() as *const u64).add(i) as u32, _ => *(l.ptr.ptr() as *const u32).add(i) };
        let mut past = false;
        let _ = util::for_each_row(s, |i| {
            let x = if util::row_valid(l.valid.ptr() as *const u64, i) { at(i) as usize } else { slots - 1 };
            match x < slots {
                true => *t.add(x) += 1,
                false => past = true,
            }
            true
        });
        past
    };
    if past {
        *e.add(1) = NO_TABLE as u64;
    }
    0
}

/// Each code's rows of a flat window into `t`; a skewed window (`skewed`) by runs of one code, so an increment does not
/// wait on the store of the one before; false at a code past `slots`.
unsafe fn tally_runs(t: *mut u32, slots: usize, codes: &[u32]) -> bool {
    let Some((&first, rest)) = codes.split_first() else { return true };
    if !skewed(codes) {
        return tally(t, core::ptr::null_mut(), slots, codes);
    }
    if codes.iter().fold(0, |m, &x| m.max(x)) as usize >= slots {
        return false;
    }
    let (mut x, mut run) = (first, 1u32);
    for &y in rest {
        if y == x {
            run += 1;
        } else {
            *t.add(x as usize) += run;
            (x, run) = (y, 1);
        }
    }
    *t.add(x as usize) += run;
    true
}

/// The memo directory's words: `[cap, n, (first granule, slots, table) x n]` in a statement pin.
const DIR_HDR: usize = 2;
const DIR_ENTRY: usize = 3;
/// A directory entry's `slots` for a part with one entry counted and no table yet.
const SEEN: usize = 0;
/// A directory entry's `slots` for a part that takes no table (its code lane names no code count, or holds a code past it).
const NO_TABLE: usize = usize::MAX;
/// The directory key (no part's: a first granule is below 2^31) of the statement's side tables, the three a skewed
/// part's pass counts into beside its own table, its `slots` their words.
const SIDE: u64 = u64::MAX;

unsafe fn count(ctx: *mut Ctx, k: usize, mark: u64, memo: *mut u64) -> i64 {
    let start = (mark >> 32) & 0x7fff_ffff;
    if memo.is_null() || (*ctx).program.ptr().is_null() {
        return walk(ctx, k, mark, None);
    }
    match lookup(*memo as *const u64, start) {
        None => {
            if !file(ctx, memo, start, SEEN, core::ptr::null_mut()) {
                return ERROR;
            }
            walk(ctx, k, mark, None)
        }
        Some((_, SEEN)) => walk(ctx, k, mark, Some(memo)),
        Some((_, NO_TABLE)) => walk(ctx, k, mark, None),
        Some((t, slots)) => slot(t, slots, mark as u32),
    }
}

/// Part `start`'s table in directory `d`: `(table, slots)`.
unsafe fn lookup(d: *const u64, start: u64) -> Option<(*const u32, usize)> {
    if d.is_null() {
        return None;
    }
    let n = *d.add(1) as usize;
    (0..n).map(|i| d.add(DIR_HDR + i * DIR_ENTRY)).find(|e| **e == start).map(|e| (*e.add(2) as *const u32, *e.add(1) as usize))
}

/// Code `code`'s rows in a table of `slots` (the last: the part's NULL rows); a code past the table holds none.
unsafe fn slot(t: *const u32, slots: usize, code: u32) -> i64 {
    match code {
        NULL_CODE => *t.add(slots - 1) as i64,
        c if (c as usize) < slots - 1 => *t.add(c as usize) as i64,
        _ => 0,
    }
}

/// Files part `start`'s entry (`SEEN`, `NO_TABLE` or its table) in the directory behind `memo`, over the part's entry
/// when there is one, else appended, doubling the directory in a new statement pin when full; false: the pool refused.
unsafe fn file(ctx: *mut Ctx, memo: *mut u64, start: u64, slots: usize, t: *mut u32) -> bool {
    let mut d = *memo as *mut u64;
    let n = if d.is_null() { 0 } else { *d.add(1) as usize };
    if let Some(e) = (0..n).map(|i| d.add(DIR_HDR + i * DIR_ENTRY)).find(|e| **e == start) {
        (*e.add(1), *e.add(2)) = (slots as u64, t as u64);
        return true;
    }
    if d.is_null() || n == *d as usize {
        let cap = if d.is_null() { 8 } else { 2 * n };
        let bytes = 8 * (DIR_HDR + cap * DIR_ENTRY);
        let g = gravity_abi::program::pin_statement(ctx, bytes as u64) as *mut u64;
        if g.is_null() {
            crate::host_abi::pin_refused(bytes);
            return false;
        }
        if !d.is_null() {
            core::ptr::copy_nonoverlapping(d.add(DIR_HDR), g.add(DIR_HDR), n * DIR_ENTRY);
        }
        (*g, *g.add(1)) = (cap as u64, n as u64);
        d = g;
        *memo = d as u64;
    }
    let e = d.add(DIR_HDR + n * DIR_ENTRY);
    (*e, *e.add(1), *e.add(2)) = (start, slots as u64, t as u64);
    *d.add(1) = n as u64 + 1;
    true
}

/// One pass of count site `k` over the part whose first granule `mark` names: with `memo`, every code's rows into a new
/// table (filed, then read for `mark`'s code); without it (or for a part whose lane names no code count, or a code past
/// it), `mark`'s code alone.
unsafe fn walk(ctx: *mut Ctx, k: usize, mark: u64, mut memo: Option<*mut u64>) -> i64 {
    let (start, code) = ((mark >> 32) & 0x7fff_ffff, mark as u32);
    let s = util::site(ctx, k);
    (*s.keys.ptr(), *s.keys.ptr().add(1), *s.keys_null.ptr(), *s.keys_null.ptr().add(1)) = (start, u64::MAX, 0, 0);
    if util::seek(ctx, k) < 0 {
        return ERROR;
    }
    let (mut part, mut c, mut table, mut side) = (None, 0i64, None::<(*mut u32, usize)>, None::<*mut u32>);
    loop {
        match util::advance(ctx, k) {
            r if r < 0 => return r,
            0 => break,
            _ => {}
        }
        let s = util::site(ctx, k);
        if *part.get_or_insert(s.part_id) != s.part_id {
            break;
        }
        if (*s.lanes.ptr()).ptr.ptr().is_null() && gravity_abi::site::lane_fill(s, 0) < 0 {
            return ERROR;
        }
        let l = &*s.lanes.ptr();
        if let (Some(_), None) = (memo, table) {
            let ncodes = l.ncodes_or_scale as usize;
            if ncodes == 0 {
                if !file(ctx, memo.unwrap(), start, NO_TABLE, core::ptr::null_mut()) {
                    return ERROR;
                }
                memo = None;
            } else {
                let bytes = 4 * (ncodes + 1);
                let t = gravity_abi::program::pin_statement(ctx, bytes as u64) as *mut u32;
                if t.is_null() {
                    crate::host_abi::pin_refused(bytes);
                    return ERROR;
                }
                table = Some((t, ncodes + 1));
            }
        }
        let at = |i: usize| match l.width { 2 => *(l.ptr.ptr() as *const u16).add(i) as u32, 8 => *(l.ptr.ptr() as *const u64).add(i) as u32, _ => *(l.ptr.ptr() as *const u32).add(i) };
        match table {
            Some((t, slots)) => {
                let past = if s.sel.ptr().is_null() && l.valid.ptr().is_null() && l.width == 4 {
                    let (from, to) = (s.cursor as usize, s.rows as usize);
                    let codes = core::slice::from_raw_parts((l.ptr.ptr() as *const u32).add(from), to.saturating_sub(from));
                    let sides = match side {
                        Some(x) => x,
                        None => {
                            let x = if skewed(codes) { side_tables(ctx, memo.unwrap(), slots) } else { core::ptr::null_mut() };
                            if x == ERROR_TABLES {
                                return ERROR;
                            }
                            *side.insert(x)
                        }
                    };
                    !tally(t, sides, slots, codes)
                } else {
                    let mut past = false;
                    let _ = util::for_each_row(s, |i| {
                        let x = if util::row_valid(l.valid.ptr() as *const u64, i) { at(i) as usize } else { slots - 1 };
                        if x < slots {
                            *t.add(x) += 1;
                        } else {
                            past = true;
                        }
                        true
                    });
                    past
                };
                if past {
                    // a code past the lane's count: no table for this part (never again: its pin stays the statement's
                    // alone); the pass counts `mark`'s code from here
                    if !file(ctx, memo.unwrap(), start, NO_TABLE, core::ptr::null_mut()) {
                        return ERROR;
                    }
                    return walk(ctx, k, mark, None);
                }
            }
            // A window with no selection, every row valid and 4-byte codes counts as one flat compare loop (the
            // ladder's heads_emit count; NEON auto-vectorized); any other window takes the per-row walk below
            None if s.sel.ptr().is_null() && l.valid.ptr().is_null() && l.width == 4 && code != NULL_CODE => {
                let (from, to) = (s.cursor as usize, s.rows as usize);
                let codes = core::slice::from_raw_parts((l.ptr.ptr() as *const u32).add(from), to.saturating_sub(from));
                c += codes.iter().filter(|&&x| x == code).count() as i64;
            }
            None => {
                util::for_each_row(s, |i| {
                    let valid = util::row_valid(l.valid.ptr() as *const u64, i);
                    c += if code == NULL_CODE { !valid } else { valid && at(i) == code } as i64;
                    true
                });
            }
        }
        s.cursor = util::end(s);
    }
    match (memo, table) {
        (Some(m), Some((t, slots))) => {
            if let Some(x) = side.filter(|x| !x.is_null()) {
                for i in 0..slots {
                    *t.add(i) += *x.add(i) + *x.add(slots + i) + *x.add(2 * slots + i);
                }
            }
            if !file(ctx, m, start, slots, t) {
                return ERROR;
            }
            slot(t, slots, code)
        }
        _ => c,
    }
}

/// `side_tables`' refusal (a pin the pool refused): no table is ever at address 1.
const ERROR_TABLES: *mut u32 = 1 as *mut u32;

/// Whether one row in eight or more repeats the row before it: past that, a single table's increments chain on each
/// other's stores (D3027: a skewed text column's parts 51 -> 39 ms on four tables); below it the side tables' zeroing
/// and fold cost more than they save (a near-unique key: about 3x a single table).
fn skewed(codes: &[u32]) -> bool {
    codes.len() > 1 && codes.windows(2).filter(|w| w[0] == w[1]).count() * 8 >= codes.len()
}

/// The statement's three side tables of `slots` each, zeroed: the directory's, grown (a new statement pin) when short.
/// `ERROR_TABLES`: the pool refused.
unsafe fn side_tables(ctx: *mut Ctx, memo: *mut u64, slots: usize) -> *mut u32 {
    let words = 3 * slots;
    let x = match lookup(*memo as *const u64, SIDE) {
        Some((x, have)) if have >= words => x as *mut u32,
        _ => {
            let x = gravity_abi::program::pin_statement(ctx, 4 * words as u64) as *mut u32;
            if x.is_null() {
                crate::host_abi::pin_refused(4 * words);
                return ERROR_TABLES;
            }
            if !file(ctx, memo, SIDE, words, x) {
                return ERROR_TABLES;
            }
            x
        }
    };
    core::ptr::write_bytes(x, 0, words);
    x
}

/// Each code's rows of a flat window into `t` (and, with `sides`, row `i` into side table `i % 4 - 1` behind it, the
/// first row of four into `t`); false at a code past `slots`.
unsafe fn tally(t: *mut u32, sides: *mut u32, slots: usize, codes: &[u32]) -> bool {
    if codes.iter().fold(0, |m, &x| m.max(x)) as usize >= slots {
        return false;
    }
    let mut quads = codes.chunks_exact(4);
    if !sides.is_null() {
        let (a, b, c) = (sides, sides.add(slots), sides.add(2 * slots));
        for q in &mut quads {
            *t.add(q[0] as usize) += 1;
            *a.add(q[1] as usize) += 1;
            *b.add(q[2] as usize) += 1;
            *c.add(q[3] as usize) += 1;
        }
    }
    for &x in if sides.is_null() { codes } else { quads.remainder() } {
        *t.add(x as usize) += 1;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D4554: a sparse entry (`PASS_SPARSE`) answers the codes it holds and leaves every other code to the emit's own
    /// count; a dense entry beside it answers as before.
    #[test]
    fn d4554_a_sparse_entry_answers_its_codes_only() {
        let dense = [3u32, 4, 5, 6];
        let sparse = [(2u64 << 32) | 20, (9u64 << 32) | 90, ((NULL_CODE as u64) << 32) | 7];
        let dir = [2u64, 0, dense.len() as u64, dense.as_ptr() as u64, 8, PASS_SPARSE | sparse.len() as u64, sparse.as_ptr() as u64];
        let mark = |start: u64, code: u32| (1u64 << 63) | (start << 32) | code as u64;
        // SAFETY: `dir` is a directory of two entries whose tables are live above.
        unsafe {
            assert_eq!(counted(dir.as_ptr(), mark(0, 1)), Some(4), "the dense table's code 1");
            assert_eq!(counted(dir.as_ptr(), mark(0, NULL_CODE)), Some(6), "the dense table's NULL rows");
            assert_eq!(counted(dir.as_ptr(), mark(8, 2)), Some(20));
            assert_eq!(counted(dir.as_ptr(), mark(8, 9)), Some(90));
            assert_eq!(counted(dir.as_ptr(), mark(8, NULL_CODE)), Some(7), "a kept NULL count");
            assert_eq!(counted(dir.as_ptr(), mark(8, 5)), None, "a code the sparse table does not hold: the emit counts it");
            assert_eq!(counted(dir.as_ptr(), mark(4, 5)), None, "no entry for the part");
            assert_eq!(pass_slot(dense.as_ptr(), dense.len(), 2), 5);
        }
    }
}
