//! The sort family's raw half in lowerv4: the host entries its lowered IR calls by address, moved unchanged from
//! incremental.rs (`incsort_span`, a closed prefix group sorted in place, with its span sort), strategies/fetch.rs
//! (`fetch_site`, the by-rowid fetch site's call) and strategies/resident.rs (`sort_image_key`, a key tail's payload
//! copied behind its header). Their callers take each entry's address for the IR's host call.
#![allow(unsafe_code)]
use gravity_abi::PtrWord;
use core::alloc::Layout;
use core::cmp::Ordering;
use core::mem::MaybeUninit;

use gravity_abi::breaker::SortRun;
use gravity_abi::ctx::{lane, Ctx, Lane};
use gravity_abi::status::{unsupported, ERROR, UNSUPPORTED_SHAPE};
use gravity_fn::arena::{Kind, OpArena};
use gravity_kernels::host_abi::{self as h, cmp_rows, var_shape, RowCmp, SortSpec};

/// Rows a closed group sorts through the stack scratch (`SPAN_ROWS` indexes, `SPAN_BYTES` of rows); a larger group allocates.
const SPAN_ROWS: usize = 256;
const SPAN_BYTES: usize = 16 << 10;

gravity_abi::host! {
    /// Rows `[from, to)` of sort run `run` ordered in place by the full row compare (`cmp_rows` over `nkey` keys and `spec`,
    /// or a var row's `RowCmp` over its emit table `tbl`): a closed prefix group. A var column's slot (`[size u32, off u32]`,
    /// the copy `off` bytes past the slot; the slot offsets are the table's var section, `var_shape`) is re-pointed as its row
    /// moves. 0, or `ERROR` when a large group's scratch cannot be allocated or a collation compare fails (stashed).
    /// D3294: under the landing pad. A hosted key's comparator is the user's function (a PL body), whose unwinds (a memory
    /// limit's or the governor's out of memory, a FATAL's thread exit) must not cross this `extern "C"` frame: the server
    /// would abort. The pad parks the unwind and answers `ERROR`; it resumes once the program returns.
    /// CONVERSION WAVE 1 (cvsort): a group past the stack scratch takes its index and row copy from a scratch arena charged
    /// to the sort's own ledger owner (`ctx`'s pipeline is the sort's producer: its slot is the sort's), on the allocator's
    /// road as the `Vec`s were; no owner (a rig, a full operator table): the bytes stay on the thread's account.
    /// # Safety
    /// `ctx` is null or the calling program's bound ctx; `run` is a live sort run whose rows `[from, to)` are written;
    /// `nkey`, `spec` and `tbl` (0: none, a run with no var column) are its shape's.
    #[guard(ERROR)]
    pub(crate) unsafe fn incsort_span(ctx: *mut Ctx, run: *mut u8, from: u64, to: u64, nkey: u64, spec: u64, tbl: u64) -> i64 {
        let (nkey, count) = (nkey & !COUNT, nkey & COUNT != 0);
        let r = &*(run as *const SortRun);
        let (from, to, rb) = (from as usize, to as usize, r.row_bytes as usize);
        if to <= from + 1 || to > r.len as usize {
            return 0;
        }
        let (n, base) = (to - from, r.row(from) as *mut u8);
        // never zeroed: `span` writes every index and every scratch byte before it reads it
        let (mut si, mut sb) = ([MaybeUninit::<u32>::uninit(); SPAN_ROWS], [MaybeUninit::<u8>::uninit(); SPAN_BYTES]);
        // past the stack scratch: one carve, the `n` indexes then the `n * rb` row bytes; the arena lives to the return
        let mut held: Option<OpArena> = None;
        let (idx, tmp): (*mut u32, *mut u8) = if n <= SPAN_ROWS && n * rb <= SPAN_BYTES {
            (si.as_mut_ptr() as *mut u32, sb.as_mut_ptr() as *mut u8)
        } else {
            let a = held.insert(OpArena::scratch_of(OpArena::slot_owner(ctx), Kind::Sort));
            let at = n * core::mem::size_of::<u32>();
            let Some(p) = n.checked_mul(rb).and_then(|b| b.checked_add(at)).and_then(|s| Layout::from_size_align(s, 4).ok()).and_then(|l| a.alloc(l).ok()) else {
                return gravity_abi::status::ERROR;
            };
            (p.as_ptr() as *mut u32, p.as_ptr().add(at))
        };
        (0..n).for_each(|i| idx.add(i).write(i as u32));
        // SAFETY: `idx` holds `n` initialised u32s; `tmp` has `n * rb` writable bytes (the stack arrays, or the arena's carve)
        let tbl = tbl as usize as *const u32;
        let rc = (!tbl.is_null()).then(|| RowCmp::of(tbl));
        let vars = if tbl.is_null() { None } else { var_shape(tbl) };
        let vars: &[u32] = vars.map_or(&[], |v| core::slice::from_raw_parts(v.slots, 2 * v.nvar));
        let cmp = |a: *const u8, b: *const u8| rc.as_ref().map_or_else(|| cmp_rows(nkey as usize, SortSpec(spec), a, b), |c| c.cmp(a, b));
        RowCmp::failed();
        span(base, rb, core::slice::from_raw_parts_mut(idx, n), tmp, &cmp, vars);
        // group_core-0259 (`COUNT`): the sorted group's distinct rows, adjacent rows unequal under the same compare, plus one
        let row = |i: usize| base.add(i * rb) as *const u8;
        let d = if count { 1 + (1..n).filter(|&i| cmp(row(i - 1), row(i)) != Ordering::Equal).count() as i64 } else { 0 };
        if RowCmp::failed() { gravity_abi::status::ERROR } else { d }
    }
}

/// `incsort_span`'s key-word flag (group_core-0259, a stop's `Batches`): answer the sorted group's distinct rows, not 0.
pub(crate) const COUNT: u64 = 1 << 32;

/// `incsort_span`'s sort of the `idx.len()` rows at `base` (`idx`: `0..n` on entry) through `tmp` (`idx.len() * rb` writable
/// bytes); `vars`: `(col, slot offset)` per var column.
unsafe fn span(base: *mut u8, rb: usize, idx: &mut [u32], tmp: *mut u8, cmp: &dyn Fn(*const u8, *const u8) -> Ordering, vars: &[u32]) {
    let n = idx.len();
    let row = |i: u32| base.add(i as usize * rb) as *const u8;
    idx.sort_unstable_by(|&a, &b| match cmp(row(a), row(b)) {
        Ordering::Equal => a.cmp(&b),
        o => o,
    });
    if idx.iter().enumerate().all(|(i, &j)| j as usize == i) {
        return;
    }
    for (at, &j) in idx.iter().enumerate() {
        let dst = tmp.add(at * rb);
        core::ptr::copy_nonoverlapping(row(j), dst, rb);
        // the copy stays where it is: its slot's offset moves by the row's shift
        let shift = (j as i64 - at as i64) * rb as i64;
        for so in vars.chunks_exact(2).map(|cs| cs[1] as usize) {
            let slot = dst.add(so);
            if (slot as *const u32).read_unaligned() != 0 {
                let off = (slot.add(4) as *const u32).read_unaligned() as i64 + shift;
                (slot.add(4) as *mut u32).write_unaligned(off as u32);
            }
        }
    }
    core::ptr::copy_nonoverlapping(tmp, base, n * rb);
}

gravity_abi::host! {
    /// The fetch site `k` filled for `n` rowids at `rowids`: its request lane (after the `natts` attribute lanes and the offset
    /// lane) pointed at them, one `Site.fetch`.
    pub(crate) unsafe fn fetch_site(ctx: u64, k: u64, natts: u64, rowids: u64, n: u64) -> i64 {
        let s = gravity_kernels::util::site(ctx as *mut Ctx, k as usize);
        let req = natts as usize + 1;
        s.nlanes = natts as u16 + 2;
        *s.lanes.ptr().add(req) = Lane::byval(rowids as *const u8, core::ptr::null(), 8, lane::REGION_ARENA, lane::id(lane::ID_ROWID, 0));
        match gravity_abi::site::fetch(s, n as u32) {
            got if got < 0 => ERROR,
            got if got != n as i64 => unsupported(UNSUPPORTED_SHAPE),
            _ => 0,
        }
    }
}

gravity_abi::host! {
    /// `sort_image` of a key tail's payload (M6 L0-a): the varlena at `datum` read as PostgreSQL reads it
    /// (`pg_detoast_datum_packed`, then `VARDATA_ANY`: a short header in place, a compressed or TOAST image detoasted), copied
    /// behind a 4-byte header, so `RowCmp` compares it and the emit sends it as the column's datum. 0, 1 (no room), or `ERROR`
    /// with the detoast's error stashed. # Safety — `run` is a live var sort run, `datum` a live varlena.
    pub(crate) unsafe fn sort_image_key(run: *mut u8, datum: u64, slot_off: u64, rec_off: u64) -> i64 {
        match gravity_fn::with_payload(datum as usize as *const u8, |p| h::sort_image(run, p.as_ptr(), p.len() as u64, slot_off, rec_off)) {
            Ok(r) => r,
            Err(e) => {
                gravity_fn::stash_error(e);
                gravity_abi::status::ERROR
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_abi::breaker::SORTRUN_HDR;
    use gravity_kernels::host_abi::{SORT_TBL_HDR, VAR_TBL};

    /// A run of `rows` rows: key pair `[1, k]` at 0 (one key, no spec), then `NVAR` var slots (more than the eight words
    /// the span took before its slots came from the emit table), each pointing past the rows at a copy holding the row's
    /// key plus its column; the span sort keeps every slot pointing at its own row's copy.
    #[test]
    fn a_span_sorts_in_place_and_keeps_var_copies() {
        const NVAR: usize = 10;
        let (rows, cap) = (6usize, 8usize);
        let rb = 16 + 8 * NVAR;
        let keys = [5u64, 3, 9, 1, 3, 7];
        let var_at = SORTRUN_HDR + cap * rb + 8 * SORTRUN_HDR;
        let mut buf = vec![0u8; var_at + 8 * NVAR * rows];
        let run = buf.as_mut_ptr();
        // the emit table: header, no image, the var section (`nvar, rec_off`, `(col, slot)` each), one key's `(coll, kflags)`
        let mut tbl = vec![rb as u32, 2, (1 + NVAR) as u32 | VAR_TBL, 0, 0, 0, NVAR as u32, 0];
        (0..NVAR).for_each(|c| tbl.extend([1 + c as u32, 16 + 8 * c as u32]));
        tbl.extend([0, 0]);
        assert_eq!(tbl.len(), SORT_TBL_HDR + 2 + 2 * NVAR + 2);
        unsafe {
            let r = &mut *(run as *mut SortRun);
            (r.cap, r.len, r.row_bytes) = (cap as u32, rows as u32, rb as u32);
            for (i, &k) in keys.iter().enumerate() {
                let at = run.add(SORTRUN_HDR + i * rb);
                (at as *mut u64).write_unaligned(1);
                (at.add(8) as *mut u64).write_unaligned(k);
                for c in 0..NVAR {
                    let (so, copy) = (16 + 8 * c, var_at + 8 * (NVAR * i + c));
                    (at.add(so) as *mut u32).write_unaligned(8);
                    (at.add(so + 4) as *mut u32).write_unaligned((copy - (SORTRUN_HDR + i * rb + so)) as u32);
                    (run.add(copy) as *mut u64).write_unaligned(100 * k + c as u64);
                }
            }
            // rows 1..5 only: row 0 and row 5 stay
            assert_eq!(incsort_span(core::ptr::null_mut(), run, 1, 5, 1, 0, tbl.as_ptr() as u64), 0);
            let got: Vec<u64> = (0..rows).map(|i| (run.add(SORTRUN_HDR + i * rb + 8) as *const u64).read_unaligned()).collect();
            assert_eq!(got, vec![5, 1, 3, 3, 9, 7]);
            for i in 0..rows {
                for c in 0..NVAR {
                    let slot = run.add(SORTRUN_HDR + i * rb + 16 + 8 * c);
                    let off = (slot.add(4) as *const u32).read_unaligned() as usize;
                    assert_eq!((slot.add(off) as *const u64).read_unaligned(), 100 * got[i] + c as u64, "row {i}'s copy of column {c}");
                }
            }
        }
    }

    /// group_core-0259: under `COUNT` the span answers the group's distinct rows by the full compare once sorted (adjacent
    /// unequal rows plus one), 0 without it.
    #[test]
    fn a_counted_span_answers_its_distinct_rows() {
        let (rows, rb) = (8usize, 16usize);
        let mut buf = vec![0u8; SORTRUN_HDR + rows * rb + 64];
        let run = buf.as_mut_ptr();
        unsafe {
            let r = &mut *(run as *mut SortRun);
            (r.cap, r.len, r.row_bytes) = (rows as u32, rows as u32, rb as u32);
            for (i, k) in [3u64, 1, 9, 3, 1, 3, 2, 7].into_iter().enumerate() {
                let at = run.add(SORTRUN_HDR + i * rb);
                (at as *mut u64).write_unaligned(1);
                (at.add(8) as *mut u64).write_unaligned(k);
            }
            // rows 0..6 hold {3, 1, 9, 3, 1, 3}: three distinct
            assert_eq!(incsort_span(core::ptr::null_mut(), run, 0, 6, 1 | COUNT, 0, 0), 3);
            assert_eq!(incsort_span(core::ptr::null_mut(), run, 6, 8, 1 | COUNT, 0, 0), 2);
            assert_eq!(incsort_span(core::ptr::null_mut(), run, 0, 8, 1, 0, 0), 0);
            let got: Vec<u64> = (0..rows).map(|i| (run.add(SORTRUN_HDR + i * rb + 8) as *const u64).read_unaligned()).collect();
            assert_eq!(got, vec![1, 1, 2, 3, 3, 3, 7, 9]);
            assert_eq!(incsort_span(core::ptr::null_mut(), run, 0, 8, 1 | COUNT, 0, 0), 5);
        }
    }

    /// A group past the stack scratch (300 rows) sorts through the heap form, the same order.
    #[test]
    fn a_large_span_sorts_through_the_heap() {
        let (rows, rb) = (300usize, 16usize);
        let mut buf = vec![0u8; SORTRUN_HDR + rows * rb + 64];
        let run = buf.as_mut_ptr();
        unsafe {
            let r = &mut *(run as *mut SortRun);
            (r.cap, r.len, r.row_bytes) = (rows as u32, rows as u32, rb as u32);
            for i in 0..rows {
                let at = run.add(SORTRUN_HDR + i * rb);
                (at as *mut u64).write_unaligned(1);
                (at.add(8) as *mut u64).write_unaligned(((i * 7919) % 1009) as u64);
            }
            assert_eq!(incsort_span(core::ptr::null_mut(), run, 0, rows as u64, 1, 0, 0), 0);
            let got: Vec<u64> = (0..rows).map(|i| (run.add(SORTRUN_HDR + i * rb + 8) as *const u64).read_unaligned()).collect();
            assert!(got.windows(2).all(|w| w[0] <= w[1]), "sorted");
            let mut want: Vec<u64> = (0..rows).map(|i| ((i * 7919) % 1009) as u64).collect();
            want.sort();
            assert_eq!(got, want);
        }
    }

    /// M6 L0-a: a key tail's copy is its payload behind a 4-byte header whatever the datum's header (PostgreSQL's
    /// `pg_detoast_datum_packed` then `VARDATA_ANY`): a short 1-byte one, a plain 4-byte one, a pglz-compressed inline one
    /// (detoasted); the slot points at the copy, the record word grows by the copy. A TOAST pointer is the e2e's
    /// (m6_l0_sort_keys, a detoast reads the toast relation).
    #[test]
    fn a_key_copy_is_its_payload_in_every_header_form() {
        let text = b"strawberry fool, strawberry jam!";
        let short = [&[((text.len() as u8 + 1) << 1) | 1][..], text].concat();
        let plain = [&((text.len() as u32 + 4) << 2).to_le_bytes()[..], text].concat();
        // pglz: a control byte of eight literal flags (0) before each eight literal bytes; the header's size, 0b10, the raw size
        let lz: Vec<u8> = text.chunks(8).flat_map(|c| [&[0u8][..], c].concat()).collect();
        let comp = [&(((lz.len() as u32 + 8) << 2) | 2).to_le_bytes()[..], &(text.len() as u32).to_le_bytes(), &lz].concat();
        for (form, d) in [("1-byte", short), ("4-byte", plain), ("compressed", comp)] {
            let (rb, cap) = (16usize, 2usize);
            let mut buf = vec![0u64; 64];
            let run = buf.as_mut_ptr() as *mut u8;
            unsafe {
                let r = &mut *(run as *mut SortRun);
                (r.cap, r.len, r.row_bytes) = (cap as u32, 0, rb as u32);
                (run.add(SORTRUN_HDR + cap * rb) as *mut u32).write_unaligned(512);
                assert_eq!(sort_image_key(run, d.as_ptr() as u64, 0, 8), 0, "{form}");
                let slot = run.add(SORTRUN_HDR);
                let (size, off) = ((slot as *const u32).read_unaligned() as usize, (slot.add(4) as *const u32).read_unaligned() as usize);
                assert_eq!(size, text.len() + 4, "{form}");
                assert_eq!((slot.add(off) as *const u32).read_unaligned(), (size as u32) << 2, "{form}: a 4-byte header");
                assert_eq!(core::slice::from_raw_parts(slot.add(off + 4), size - 4), &text[..], "{form}");
                assert_eq!((slot.add(8) as *const u32).read_unaligned(), h::var_alloc(size as u32), "{form}: the record word");
            }
        }
    }
}

gravity_abi::host! {
    /// EXPLAIN ANALYZE: a row a bounded incremental sort's top read, into its replay of PostgreSQL's batches
    /// (inc_replay.rs `Replay::row`; `flags`: `CHANGED`, `LAST`). 0.
    /// # Safety
    /// `st` is the top's replay words (`inc_replay::WORDS` of them, 8-aligned, written only here and at its open).
    #[guard(0)]
    pub(crate) unsafe fn incsort_replay_row(st: *mut u8, flags: u64) -> u64 {
        (*(st as *mut crate::ops::sort::inc_replay::Replay)).row(flags);
        0
    }

    /// EXPLAIN ANALYZE: the top's input ended or its walk stopped (inc_replay.rs `Replay::end`). 0.
    /// # Safety
    /// As `incsort_replay_row`.
    #[guard(0)]
    pub(crate) unsafe fn incsort_replay_end(st: *mut u8) -> u64 {
        (*(st as *mut crate::ops::sort::inc_replay::Replay)).end();
        0
    }
}
