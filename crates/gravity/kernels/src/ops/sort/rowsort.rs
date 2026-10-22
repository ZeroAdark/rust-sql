use crate::host_abi::*;
use super::polled::{sort_by, Poll, Stopped, SORT_RUN};

pub use super::raw::rowsort::{cmp_rows, sort_adopt, sort_begin, sort_begin_at, sort_held_end, sort_full, sort_full_var, sort_image, sort_image_raw, sort_last, sort_last_var, sort_next, sort_next_at, sort_open, sort_open_put, sort_open_var, sort_open_var_put, sort_stop, sort_stop_at, sort_total, sort_total_at, spool_begin, spool_full, spool_last, spool_open, spool_view, spool_view_slice, var_shape, RowCmp};
pub use super::raw::rowsort::{begin_slices, next_slices, slices_area_bytes};
pub(crate) use super::raw::rowsort::{big_put, full_var_by, hosted_procs, last, last_big, next_in, open_big, permute_rows, permute_small, point_images, put, reader_of, relaid, sort_rows_by, spool_pointed, stash_of, stash_row, tbl_shape, VarPrep, VarPut, Head};

// ---- the full sort's entries (rowsort.rs h_rowsort_open / full / last, begin / next / stop) ----

/// A run row's comparison packed 16 bits per key (four at most): the image's word offset (8 bits), its width (7 bits; 0 = the key words alone), DESC (bit 15).
#[derive(Clone, Copy)]
pub struct SortSpec(pub u64);

impl SortSpec {
    pub fn pack(keys: &[(Option<(u32, u16)>, bool)]) -> Option<SortSpec> {
        let mut s = 0u64;
        for (k, (img, desc)) in keys.iter().enumerate() {
            let mut f = (*desc as u64) << 15;
            if let Some((off, typlen)) = img {
                if off % 8 != 0 || off / 8 > 255 || *typlen > 127 {
                    return None;
                }
                f |= (off / 8) as u64 | (*typlen as u64) << 8;
            }
            s |= f << (16 * k);
        }
        Some(SortSpec(s))
    }

    pub const fn key(self, k: usize) -> (usize, usize, bool) {
        if k >= 4 {
            return (0, 0, false); // V4R-132: a fifth key on compares by its [flags, word] pair alone
        }
        let f = (self.0 >> (16 * k)) & 0xffff;
        ((f & 0xff) as usize * 8, ((f >> 8) & 0x7f) as usize, f >> 15 != 0)
    }
}

/// How a sort orders its rows (`RowCmp::order`). TOTAL-ORDER (user ruling 10-04: an inconsistent user comparator may
/// answer any valid order, but gravity never panics, hangs or crashes on it): `Images`, a pure image / word compare, or a
/// key ordered through fmgr by a builtin comparator that reaches no user code (`KFLAG_TOTAL`: a total order, std's sort);
/// `Lenient`, any other key ordered by its comparator through fmgr (`KFLAG_HOSTED`: a support function that may reach user
/// code, as a record's, an array's or a range's over a user type does), where std's sort (Rust 1.81+) panics on an order
/// that is not total ("user-provided comparison function does not correctly implement a total order"), so PostgreSQL's
/// qsort orders them (`pg_qsort`: a permutation of the input whatever the comparator answers, no panic, no read out of
/// bounds); `Exact`, a user comparator's key (`KFLAG_USER`): PostgreSQL's qsort over the rows in arrival order, stopping at
/// the first raise (`sort_exact`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Order {
    Images,
    Lenient,
    Exact,
}

/// `idx` in `cmp`'s order under `order` (`Order`): never a panic on a comparator whose order is not total, but `Images`'s.
pub(crate) fn sort_by_order<T: Copy>(idx: &mut [T], order: Order, cmp: &dyn Fn(T, T) -> Ordering) {
    match order {
        Order::Exact => sort_exact(idx, cmp),
        Order::Lenient => gravity_fn::pg_qsort::pg_qsort(idx, |&a, &b| cmp(a, b) as i32),
        Order::Images => idx.sort_unstable_by(|&a, &b| cmp(a, b)),
    }
}

/// D4411: `sort_by_order` as a polled step (polled.rs): a run's full sort was one kernel call with no poll, so a cancel
/// waited for all of it (seconds on a large run). `Images` is `polled::sort_by` (runs of `SORT_RUN`
/// by std's sort, merged into `scratch`: up to `SORT_RUN` elements it orders as `sort_by_order` does); `Lenient` and
/// `Exact` are PostgreSQL's qsort with its interrupt check (the same compares in the same order as `sort_by_order`'s, the
/// check where sort_template.h's CHECK_FOR_INTERRUPTS sits), a tick per `BATCH` checks. `scratch` holds `idx.len()`
/// elements for an `Images` sort past `SORT_RUN` (`polled_scratch`), else none. `Err`: `idx` holds its elements in no order.
pub(crate) fn sort_by_order_polled<T: Copy>(poll: &mut Poll<'_>, idx: &mut [T], scratch: &mut [T], order: Order, cmp: &dyn Fn(T, T) -> Ordering) -> Result<(), Stopped> {
    let mut k = 0usize;
    match order {
        Order::Exact => {
            let r = gravity_fn::pg_qsort::pg_qsort_arg_interruptible(
                idx,
                |&a, &b| {
                    let o = cmp(a, b);
                    if CMP_ERR.with(|c| c.get()) { Err(None) } else { Ok(o as i32) }
                },
                || {
                    k += 1;
                    poll.batch(k).map_err(Some)
                },
            );
            match r {
                Err(Some(s)) => Err(s),
                // a raise stops the sort as `sort_exact`'s does (`CMP_ERR` set for the caller)
                _ => Ok(()),
            }
        }
        Order::Lenient => gravity_fn::pg_qsort::pg_qsort_interruptible(idx, |&a, &b| cmp(a, b) as i32, || {
            k += 1;
            poll.batch(k)
        }),
        Order::Images => sort_by(poll, idx, scratch, |&a, &b| cmp(a, b)),
    }
}

/// D4411: the elements `sort_by_order_polled`'s `scratch` holds for `n` elements under `order`.
pub(crate) fn polled_scratch(n: usize, order: Order) -> usize {
    if order == Order::Images && n > SORT_RUN { n } else { 0 }
}

/// The small road's bounds (`small_on`): a run's order and its permutation on the stack (raw/runs.rs `small_index`), a
/// group emit's text sort over heads taken once per record (emit.rs `small_generic`).
pub use gravity_abi::ctx::{SMALL_SORT_ROWS, SMALL_SORT_ROW_BYTES};

/// The small road's switch (`set_small_sort`): on until the process's first statement sets it from
/// `PGRUST_GRAVITY_SMALL_SORT` (exec inputs/knobs/diag.rs, glue.rs). Both roads put the same rows in the same order.
static SMALL_SORT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);

/// The small road is on. Its callers' `_by` forms take it as an argument, so a test drives both roads in one process.
pub fn small_on() -> bool {
    SMALL_SORT.load(core::sync::atomic::Ordering::Relaxed)
}

/// Sets the small road's switch for the process (exec glue.rs, once, before the first statement's kernels run).
pub fn set_small_sort(on: bool) {
    SMALL_SORT.store(on, core::sync::atomic::Ordering::Relaxed);
}

/// D1330: `pad[1]` of a big run (`big_run`): its rows go to the stream by their own put, never kept.
pub const SORTRUN_BIG: u8 = 0x42;

thread_local! {
    /// D1330: (the run whose empty run had no room for a row's copy, the sort open's generation, the bytes the row's copies
    /// want): `image_into` arms it and answers "no room"; the put that follows (`full_var`) takes it and hands out a big run.
    /// A second miss on the same armed run is today's `UNSUPPORTED_SCRATCH` (a put form that grows no big run).
    pub(crate) static WANT: core::cell::Cell<(usize, u64, usize)> = const { core::cell::Cell::new((0, 0, 0)) };
    /// D1330: bumped by every var sort open, so an arm left by an ERROR or an earlier statement never matches.
    pub(crate) static WANT_GEN: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
}

/// D1330: a var sort's call opened: no arm survives from before.
pub(crate) fn want_begin() {
    WANT_GEN.with(|g| g.set(g.get().wrapping_add(1)));
    WANT.with(|w| w.set((0, 0, 0)));
}

pub(crate) const SEALED: u8 = 0x5e;

/// The emit table's words: `row_bytes, nkey (words), natts, nimg, spec lo, spec hi`, then `col, off, typlen` per image.
pub const SORT_TBL_HDR: usize = 6;
pub const SORT_MODE_MEM: i64 = 1;
pub const SORT_MODE_MERGE: i64 = 2;

/// Bit 31 of the table's `natts` word: the var section follows the images — `nvar, rec_off`, `(col, slot_off)` per var column, `(coll, kflags)` per key.
pub const VAR_TBL: u32 = 1 << 31;
/// `SortSpec` width of a variable image key: the slot `[size u32, off u32]` at the key's offset, the tail compared through `RowCmp`.
pub const VAR_WIDTH: u16 = 127;
/// `SortSpec` width of a by-value key ordered by its comparator (rep.hosted.order.fixed: an enum, V4R-386): its datum
/// word at the key's offset is the comparator's argument (`KFLAG_HOSTED`; only a var table carries the comparator).
pub const WORD_WIDTH: u16 = 126;
/// The widest fixed by-reference image a run row holds: the largest `SortSpec` width below `VAR_WIDTH`, the 7-bit width
/// field's top value, which names a variable image. A wider fixed column refuses by shape (lowerv4 face/runs.rs).
pub const RUN_IMAGE_MAX: i16 = VAR_WIDTH as i16 - 1;
pub const KFLAG_BPCHAR: u32 = 1;
/// A key ordered by its type's btree comparator through fmgr (rep.hosted.order, M6): its var copy is the Datum. A table with
/// any such key ends with the hosted section, one support-function oid per key (0 for a key that is not hosted).
pub const KFLAG_HOSTED: u32 = 2;
/// A hosted key whose comparator cannot raise on two equal copies (the lowering's word, from the key's type: no collatable
/// part, so D2043's 42P22 cannot arise): equal var payloads tie with no call and no `order_seen` entry. Without it an
/// equal copy under no collation goes through `keysem::call::order_seen` (a set of at most `SEEN_VALUES`, emptied at every
/// `RowCmp::of`: each put and each merge), which called bitcmp / record_cmp on every equal copy past it.
pub const KFLAG_EQTIE: u32 = 4;
/// A hosted key whose prefix word marks itself whole by its low bit (the lowering's word, from the key's type: a bit /
/// varbit's `typesem::prefix::bits_word` up to 56 bits): an equal word whose low bit is clear (set under DESC, whose word
/// is the image's `!`) is an equal value, so the tie ends with no payload read and no call.
pub const KFLAG_WORDEXACT: u32 = 8;
/// USEROP: a hosted key whose comparator is user code (a user btree family's FUNCTION 1, or a comparator that reaches one:
/// a range over a user subtype opclass): PostgreSQL's tuplesort meets the pairs it meets, in its order, so a comparator
/// that raises raises on PG's pair (its message names the pair). A run holding one orders by PostgreSQL's own qsort over
/// its rows in arrival order (`sort_exact`: pg_qsort_arg, stopping at the first raise), and the key calls its comparator
/// on equal copies too (no equal-bytes tie), as PG's `ApplySortComparator` does.
pub const KFLAG_USER: u32 = 32;
/// TOTAL-ORDER: a hosted key whose comparator is a builtin's that reaches no user code (the lowering's word, rep/keylane.rs
/// `Key::total`: a builtin support function over a type no user opclass orders, as numeric_cmp, bttextcmp, enum_cmp or an
/// array's over builtin elements), so its order is total and its sorts keep std's sort (`Order::Images`). A hosted key
/// without it orders by PostgreSQL's qsort (`Order::Lenient`), which never panics on an order that is not total.
pub const KFLAG_TOTAL: u32 = 64;
/// A key at 4 or past: its `SortSpec` entry in `kflags >> KFLAG_SPEC_SHIFT` (a var run's `RowCmp` compares its tail; `cmp_rows` never does).
pub const KFLAG_SPEC_SHIFT: u32 = 16;
/// The keys a `RowCmp` holds; a var run's keys past them are read from its table (`RowCmp::cmp_far`, W8-SETOP-WIDEKEYS).
pub const ROWCMP_KEYS: usize = 16;

/// A by-reference copy's bytes in the run: a 4-byte header + `len`, zero-padded to 8 and to 16 (the key word's load past a short image stays in the copy).
pub const fn var_alloc(size: u32) -> u32 {
    let a = size.next_multiple_of(8);
    if a < 16 {
        16
    } else {
        a
    }
}

/// The var section of a table (`VAR_TBL`), parsed once per call.
#[derive(Clone, Copy)]
pub struct VarShape {
    pub nvar: usize,
    pub rec_off: usize,
    /// `(col, slot offset)` per var column.
    pub slots: *const u32,
    pub(crate) keys: *const u32,
    /// D2572-c: the null bytes' padding (after the `natts` null bytes, up to the record word), never read: a run's put
    /// writes it zero (else it holds a run before's bytes); `(0, 0)` off the plain layout.
    pub(crate) pad: (usize, usize),
}

thread_local! {
    pub(crate) static CMP_ERR: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

gravity_fn::leader_tls::leader_tls! {
    thread_local! {
        /// USEROP K7 (raw.rs `SortHeapStep`): each exact top's PostgreSQL bounded heap, `(run, its rows at the last step,
        /// the heap's row indexes)`; one sort thread's runs only (a user comparator's pipeline runs on the leader).
        pub(crate) static HEAPS: core::cell::RefCell<Vec<(usize, u32, Vec<u32>)>> = const { core::cell::RefCell::new(Vec::new()) }; // alloc: Exec
        /// USEROP K7: this thread's session teardown empties `HEAPS` (raw.rs `sweep_heaps`, registered with its first heap).
        pub(crate) static HEAPS_SWEPT: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    }
}

/// `RowCmp::var_cmp` of a key under collation `coll` (0: bytes), `bpchar` trimming trailing blanks.
#[inline(always)]
pub(crate) fn var_cmp_as(coll: u32, bpchar: bool, x: &[u8], y: &[u8]) -> Ordering {
    let trim = |s: &[u8]| if bpchar { s.iter().rposition(|&c| c != b' ').map_or(0, |i| i + 1) } else { s.len() };
    let (x, y) = (&x[..trim(x)], &y[..trim(y)]);
    // the equal-short-key shortcut reads the prefix word: only a bytes-ordered key has one
    if coll == 0 && x.len() <= 8 && y.len() <= 8 {
        return x.len().cmp(&y.len());
    }
    if coll == 0 {
        return x.cmp(y);
    }
    match gravity_fn::typesem::collation::sort_compare(x, y, coll) {
        Ok(c) => c.cmp(&0),
        Err(e) => {
            gravity_fn::stash_error(e);
            CMP_ERR.with(|c| c.set(true));
            Ordering::Equal
        }
    }
}

/// USEROP (`KFLAG_USER`): `idx` (a run's rows in arrival order) in PostgreSQL's qsort order (pg_qsort_arg: port/qsort.c's
/// sort_template, the same pairs in the same order as PG's tuplesort over its memtuples), stopping at the first compare
/// that raised (`CMP_ERR` set, its error stashed for the caller), as PG's error leaves the sort.
pub(crate) fn sort_exact<T: Copy>(idx: &mut [T], cmp: &dyn Fn(T, T) -> Ordering) {
    let _ = gravity_fn::pg_qsort::pg_qsort_arg(idx, |&a, &b| {
        let o = cmp(a, b);
        if CMP_ERR.with(|c| c.get()) { Err(()) } else { Ok(o as i32) }
    });
}

/// A sort's key comparator that failed to bind or raised (`keysem::call::order`'s `Err`): its error stashed and `CMP_ERR`
/// set (the caller reports it after the sort); the pair compares equal.
pub(crate) fn order_failed(e: Box<gravity_fn::PgError>) -> Ordering {
    gravity_fn::stash_error(e);
    CMP_ERR.with(|c| c.set(true));
    Ordering::Equal
}

thread_local! {
    /// (the stash, the fixed record bytes or 0 for a var record, the var length word's offset, the stash's room to the
    /// region's end: a copy is bounded by it, D1328)
    pub(crate) static STASH: core::cell::Cell<(*mut u8, usize, usize, usize)> = const { core::cell::Cell::new((core::ptr::null_mut(), 0, 0, 0)) };
    /// D1327: `stash_big`'s (region stash bytes, big stash address, its bytes, the row stashed in it, the merge region).
    pub(crate) static BIG: core::cell::Cell<(usize, *mut u8, usize, bool, usize)> = const { core::cell::Cell::new((0, core::ptr::null_mut(), 0, false, 0)) };
}

/// D3375 N7: this thread's sort state that lives across a compare: `CMP_ERR`, a merge emit's `STASH` / `BIG`, and D1330's
/// `WANT` arm with its generation. A SQL / PL comparator's own statements are served (gravity_fn keysem `call` holds no
/// borrow across the call) and may sort on this thread in turn; the exec entry saves this before a statement runs at a
/// level and puts it back after (exec admit reentry `run_at`), so a nested sort never hands its flag, its stash or its
/// arm to the compare loop it ran under (a comparator that caught its body's sort error compares on, as in PostgreSQL).
pub struct SortThreadState {
    cmp_err: bool,
    stash: (*mut u8, usize, usize, usize),
    big: (usize, *mut u8, usize, bool, usize),
    want: (usize, u64, usize),
    want_gen: u64,
}

/// D3375 N7: the state `SortThreadState` names, as it is now.
pub fn save_thread_state() -> SortThreadState {
    SortThreadState {
        cmp_err: CMP_ERR.with(|c| c.get()),
        stash: STASH.with(|c| c.get()),
        big: BIG.with(|c| c.get()),
        want: WANT.with(|c| c.get()),
        want_gen: WANT_GEN.with(|c| c.get()),
    }
}

/// D3375 N7: `s` put back (the statement that ran since `save_thread_state` has ended or suspended).
pub fn restore_thread_state(s: SortThreadState) {
    CMP_ERR.with(|c| c.set(s.cmp_err));
    STASH.with(|c| c.set(s.stash));
    BIG.with(|c| c.set(s.big));
    WANT.with(|c| c.set(s.want));
    WANT_GEN.with(|c| c.set(s.want_gen));
}

gravity_fn::leader_tls::leader_tls! {
    thread_local! {
        /// D3620: per reentry level, the sort state `save_at` kept for the statement running at that level.
        static SAVED: core::cell::RefCell<Vec<SortThreadState>> = const { core::cell::RefCell::new(Vec::new()) }; // alloc: Exec
    }
}

/// D3620: `save_thread_state` kept in reentry level `level`'s slot until `restore_at(level)`, not in the caller's frame:
/// the exec entry saves it on every nested statement's stack path (exec admit reentry `run_at`), where each frame costs
/// recursion depth. A level is held by one running statement, so its slot is that statement's; `run_at`'s guard restores
/// it on any exit, an unwind too.
pub fn save_at(level: usize) {
    let s = save_thread_state();
    SAVED.with(|v| {
        let mut v = v.borrow_mut();
        if v.len() <= level {
            v.resize_with(level + 1, SortThreadState::idle); // alloc: Exec
        }
        v[level] = s;
    });
}

/// D3881: the levels past `levels` are gone (exec reentry frees deep levels): their slots go too.
pub fn trim_levels(levels: usize) {
    SAVED.with(|v| {
        let mut v = v.borrow_mut();
        if v.len() > levels {
            v.truncate(levels);
            if v.capacity() > 4 * levels.max(16) {
                v.shrink_to(2 * levels.max(16));
            }
        }
    });
}

/// D3620: the state `save_at(level)` kept, put back.
pub fn restore_at(level: usize) {
    if let Some(s) = SAVED.with(|v| v.borrow_mut().get_mut(level).map(|s| core::mem::replace(s, SortThreadState::idle()))) {
        restore_thread_state(s);
    }
}

impl SortThreadState {
    /// A slot no save has filled (`save_at`'s growth).
    fn idle() -> SortThreadState {
        SortThreadState { cmp_err: false, stash: (core::ptr::null_mut(), 0, 0, 0), big: (0, core::ptr::null_mut(), 0, false, 0), want: (0, 0, 0), want_gen: 0 }
    }
}

// D1061: a region a reader's merge overwrote is told from a run by the kind bytes: a run's byte 15 is TABLE_SORT, a
// merge's is its heap_len's top byte (0); the slot's open zeroes the first 64 bytes (driver ZERO_PREFIX), so no life
// reads another's merge.
const _: () = assert!(TABLE_SORT != 0 && gravity_abi::breaker::TABLE_KIND_OFF == 15 && core::mem::offset_of!(SortRun, kind) == 15);
const _: () = assert!(core::mem::offset_of!(runmerge::RunMerge, kind) == 31 && core::mem::offset_of!(runmerge::RunMerge, heap_len) == 12);
const _: () = assert!(size_of::<runmerge::RunMerge>() <= 64);

/// A spool past its region (M8 L1: the run is never put; D114 serves it).
pub const SPOOL_SPILLED_WHAT: &str = "a spool past its share (M8 L5a: the spilled spool)";

pub(crate) static SPOOL_EMPTY: [u64; SORTRUN_HDR / 8 + 2] = [0; SORTRUN_HDR / 8 + 2];

#[cfg(test)]
mod d1328_stash_bound_tests {
    use super::*;

    /// A var record one byte past the stash's room: a named Bug, the stash and the bytes behind it untouched.
    #[test]
    fn stash_row_raises_past_the_room() {
        let mut region = [0xa5u8; 64];
        let dst = region.as_mut_ptr();
        let mut row = [7u8; 48];
        row[..4].copy_from_slice(&33u32.to_le_bytes());
        STASH.with(|c| c.set((dst, 0, 0, 32)));
        let r = unsafe { stash_row(core::ptr::null_mut(), row.as_ptr()) };
        assert_eq!(r, ERROR);
        let e = gravity_fn::take_error().map(|e| e.message().to_owned()).unwrap_or_default();
        assert!(e.contains("Bug: a sort merge's emit stash of 32 bytes under a record of 33"), "{e}");
        assert!(region.iter().all(|&b| b == 0xa5));
        // at the room: copied whole
        row[..4].copy_from_slice(&32u32.to_le_bytes());
        assert_eq!(unsafe { stash_row(core::ptr::null_mut(), row.as_ptr()) }, HOST_STOP);
        assert_eq!(&region[..32], &row[..32]);
        assert!(region[32..].iter().all(|&b| b == 0xa5));
    }
}

#[cfg(test)]
mod d1301_reread_tests {
    use super::*;

    /// D1301: a merge emit read again at its last row (a stop inside that row, no hold word) hands that row again, from the
    /// stash, or from the big stash when the last row went there (`big_last`, a record wider than the stash: D1327); no
    /// drain runs, so neither is written before the next row's read.
    #[test]
    fn a_reread_of_the_last_row_hands_the_stash_or_the_big_stash() {
        const TBL: [u32; 6] = [40, 2, 2, 0, 0, 0];
        let mut region = vec![0u64; 1024]; // alloc: Cold
        let own = region.as_mut_ptr() as *mut u8;
        let mut big = vec![0u64; 16]; // alloc: Cold
        big[2] = 77;
        unsafe {
            let m = &mut *(own as *mut runmerge::RunMerge);
            (m.nruns, m.buf, m.emitted, m.kind, m.region, m.heap_len) = (2, 64, 5, runmerge::RUNMERGE_KIND, 8192, 0);
            let stash = stash_of(own).add(8);
            *(stash as *mut u64).add(2) = 42;
            let row = next_in(core::ptr::null_mut(), own, TBL.as_ptr(), SORT_MODE_MERGE as u64, 4, 0);
            assert_eq!(row, stash as *const u8, "row 4 again: the stash");
            assert_eq!(*(row as *const u64).add(2), 42);
            let m = &mut *(own as *mut runmerge::RunMerge);
            (m.big_last, m.big_stash) = (1, big.as_mut_ptr() as u64);
            let row = next_in(core::ptr::null_mut(), own, TBL.as_ptr(), SORT_MODE_MERGE as u64, 4, 0);
            assert_eq!(row, big.as_ptr() as *const u8, "a big last row again: the big stash");
            assert_eq!(*(row as *const u64).add(2), 77);
            assert_eq!((*(own as *const runmerge::RunMerge)).emitted, 5, "no drain ran");
        }
    }
}

#[cfg(test)]
mod d1330_want_tests {
    use super::*;

    /// A var copy past an empty run's room arms the put once (1: no room), a second miss on the same run with no big run
    /// grown between is today's kind 2, and a sort open disarms (an ERROR or a finished statement leaves nothing behind).
    #[test]
    fn an_empty_run_miss_arms_once_and_a_sort_open_disarms() {
        let mut buf = vec![0u64; 64]; // alloc: Cold
        let run = buf.as_mut_ptr() as *mut u8;
        unsafe {
            (*(run as *mut SortRun)).row_bytes = 512;
            let r = SortRun::at_share(run, 64);
            assert_eq!(r.len, 0);
            let big = vec![7u8; 4096]; // alloc: Cold
            want_begin();
            assert_eq!(sort_image(run, big.as_ptr(), big.len() as u64, 16, 32), 1, "armed: the put grows a big run");
            assert_eq!(sort_image(run, big.as_ptr(), big.len() as u64, 16, 32), unsupported(UNSUPPORTED_SCRATCH), "armed twice: kind 2");
            // an arm left behind (an ERROR before the put) never outlives the next open
            assert_eq!(sort_image(run, big.as_ptr(), big.len() as u64, 16, 32), 1);
            want_begin();
            assert_eq!(sort_image(run, big.as_ptr(), big.len() as u64, 16, 32), 1, "a new open: armed afresh, not kind 2");
            let (w, _, want) = WANT.with(|c| c.get());
            assert_eq!((w, want), (run as usize, var_alloc(4 + 4096) as usize));
            // a copy that fits is stored, no arm
            want_begin();
            let small = [1u8; 8];
            assert_eq!(sort_image(run, small.as_ptr(), 8, 16, 32), 0);
            assert_eq!(WANT.with(|c| c.get()).0, 0);
        }
    }
}

#[cfg(test)]
mod d2339_spool_pointed_tests {
    use super::*;

    /// D2339 / D2374: a sealed run sorted NULLs first (row 0's by-reference cell NULL) whose later rows were never pointed
    /// reads "not pointed", so `spool_view` points them; once pointed, it reads pointed; a run with no non-NULL
    /// by-reference cell has nothing to point.
    #[test]
    fn a_run_whose_row_0_is_null_is_judged_by_its_first_non_null_cell() {
        // one key word, one column: datum word at 8, NULL byte at 16, its image at 24 (typlen -1)
        const TBL: [u32; 9] = [48, 1, 1, 1, 0, 0, 0, 24, u32::MAX];
        let mut buf = vec![0u64; (SORTRUN_HDR + 2 * 48) / 8]; // alloc: Cold
        let p = buf.as_mut_ptr() as *mut u8;
        unsafe {
            *(p as *mut SortRun) = SortRun { cap: 2, len: 2, row_bytes: 48, pad: [SEALED, 0, 0], kind: TABLE_SORT };
            let r = &*(p as *const SortRun);
            let (r0, r1) = (r.row(0) as *mut u8, r.row(1) as *mut u8);
            *r0.add(16) = 1;
            assert!(!spool_pointed(r, TBL.as_ptr()), "row 1's datum 0: not pointed");
            point_images(r1, TBL.as_ptr());
            assert_eq!(*(r1.add(8) as *const u64), r1.add(24) as u64);
            assert!(spool_pointed(r, TBL.as_ptr()));
            *r1.add(16) = 1;
            assert!(spool_pointed(r, TBL.as_ptr()), "every cell NULL: nothing to point");
        }
    }

    /// A var table's run that reserved no copy (every var cell NULL) reads pointed off its trailer;
    /// once a row holds a copy below the rows' end, the rows are walked again: a stale datum reads "not pointed" and a
    /// pointed one "pointed".
    #[test]
    fn a_run_with_no_var_copy_reads_pointed_off_its_trailer() {
        // no key, one varlena column: its datum word at 0, its NULL byte at 8, the record word at 16, its var slot at 24
        const TBL: [u32; 10] = [32, 0, 1 | VAR_TBL, 0, 0, 0, 1, 16, 0, 24];
        let (cap, len, rb) = (8usize, 4usize, 32usize);
        let mut buf = vec![0u64; (SORTRUN_HDR + cap * rb) / 8 + 2]; // alloc: Cold
        let p = buf.as_mut_ptr() as *mut u8;
        unsafe {
            *(p as *mut SortRun) = SortRun { cap: cap as u32, len: 0, row_bytes: rb as u32, pad: [0; 3], kind: TABLE_SORT };
            let r = &mut *(p as *mut SortRun);
            r.clear();
            (r.len, r.pad[0]) = (len as u32, SEALED);
            for j in 0..len {
                *(r.row(j) as *mut u8).add(8) = 1;
            }
            assert!(spool_pointed(r, TBL.as_ptr()), "no copy reserved: nothing to point");
            let top = r.reserve_var(8).expect("room") as usize;
            let row3 = r.row(3) as *mut u8;
            *row3.add(8) = 0;
            let slot = row3.add(24);
            (slot as *mut u32).write_unaligned(5);
            (slot.add(4) as *mut u32).write_unaligned((top - (SORTRUN_HDR + 3 * rb + 24)) as u32);
            assert!(!spool_pointed(r, TBL.as_ptr()), "row 3's copy, its datum stale: not pointed");
            point_images(row3, TBL.as_ptr());
            assert_eq!(*(row3 as *const u64), p.add(top) as u64);
            assert!(spool_pointed(r, TBL.as_ptr()));
        }
    }
}

#[cfg(test)]
mod exact_tests {
    use super::*;

    /// USEROP (K7): over 12 rows in arrival order, PostgreSQL's qsort (its presorted check) first compares row 0 with row 1,
    /// so a comparator raising on that pair raises on it, its arguments in that order; the raise stops the sort there.
    /// With no raise the rows come out in the comparator's order.
    #[test]
    fn a_user_comparator_meets_postgres_first_pair() {
        let seen = core::cell::RefCell::new(Vec::new()); // alloc: Exec
        let mut idx: Vec<u32> = (0..12).collect();
        CMP_ERR.with(|c| c.set(false));
        sort_exact(&mut idx, &|a, b| {
            seen.borrow_mut().push((a, b));
            CMP_ERR.with(|c| c.set(true));
            Ordering::Equal
        });
        assert_eq!(seen.borrow().as_slice(), &[(0, 1)]);
        let mut sorted = idx.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..12).collect::<Vec<u32>>(), "a raise leaves a permutation");
        CMP_ERR.with(|c| c.set(false));
        let v = [1, 9, 1, 9, 1, 9, 5, 9, 1, 9, 1, 9];
        let mut idx: Vec<usize> = (0..12).collect();
        sort_exact(&mut idx, &|a, b| v[a].cmp(&v[b]));
        assert!(idx.windows(2).all(|w| v[w[0]] <= v[w[1]]));
    }
}

#[cfg(test)]
mod d4411_polled_order {
    use super::*;
    use super::super::polled::BATCH;

    /// `0..n` shuffled (Fisher-Yates over a fixed xorshift).
    fn shuffled(n: usize) -> Vec<u32> {
        let mut v: Vec<u32> = (0..n as u32).collect(); // alloc: Cold
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        for i in (1..n).rev() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            v.swap(i, (x % (i as u64 + 1)) as usize);
        }
        v
    }

    /// D4411: the polled order is `sort_by_order`'s. `Lenient` and `Exact` (PostgreSQL's qsort with its interrupt check)
    /// give the same permutation, an order that is not total included; `Images` gives std's up to `SORT_RUN` elements and
    /// a key order past it (rows tied on the whole key may lie otherwise). A pending abort stops each, every element kept.
    #[test]
    fn the_polled_order_is_sort_by_orders_and_a_pending_abort_stops_it() {
        let bad = |a: u32, b: u32| match (a.wrapping_mul(7).wrapping_add(b.wrapping_mul(13))) % 3 {
            0 => Ordering::Less,
            1 => Ordering::Equal,
            _ => Ordering::Greater,
        };
        let tied = |a: u32, b: u32| (a % 1000).cmp(&(b % 1000));
        let never = || false;
        let big = 2 * SORT_RUN + 77;
        for n in [0usize, 1, 2, 7, 40, 300, 5000, SORT_RUN, big] {
            let input = shuffled(n);
            for order in [Order::Lenient, Order::Exact] {
                CMP_ERR.with(|c| c.set(false));
                let mut want = input.clone(); // alloc: Cold
                sort_by_order(&mut want, order, &bad);
                let mut got = input.clone(); // alloc: Cold
                assert_eq!(sort_by_order_polled(&mut Poll::new(&never), &mut got, &mut [], order, &bad), Ok(()));
                assert_eq!(got, want, "{order:?} over {n}: sort_by_order's permutation");
            }
            let mut want = input.clone(); // alloc: Cold
            sort_by_order(&mut want, Order::Images, &tied);
            let (mut got, mut scratch) = (input.clone(), vec![0u32; polled_scratch(n, Order::Images)]); // alloc: Cold
            assert_eq!(sort_by_order_polled(&mut Poll::new(&never), &mut got, &mut scratch, Order::Images, &tied), Ok(()));
            if n <= SORT_RUN {
                assert_eq!(got, want, "Images over {n}: std's order");
            } else {
                assert!(got.windows(2).all(|w| tied(w[0], w[1]).is_le()), "Images over {n}: in key order");
                let mut all = got.clone(); // alloc: Cold
                all.sort_unstable();
                assert_eq!(all, (0..n as u32).collect::<Vec<u32>>(), "Images over {n}: a permutation");
            }
        }
        let yes = || true;
        let input = shuffled(big);
        for order in [Order::Images, Order::Lenient, Order::Exact] {
            CMP_ERR.with(|c| c.set(false));
            let (mut v, mut scratch) = (input.clone(), vec![0u32; polled_scratch(big, order)]); // alloc: Cold
            let cmp: &dyn Fn(u32, u32) -> Ordering = if order == Order::Images { &tied } else { &bad };
            assert_eq!(sort_by_order_polled(&mut Poll::new(&yes), &mut v, &mut scratch, order, cmp), Err(Stopped), "{order:?}: stopped");
            v.sort_unstable();
            assert_eq!(v, (0..big as u32).collect::<Vec<u32>>(), "{order:?}: a stopped sort keeps every element");
        }
        assert!(big > BATCH, "the qsort's checks tick past one batch");
    }
}

#[cfg(test)]
mod total_order {
    use super::*;

    /// TOTAL-ORDER (user ruling 10-04): a comparator whose order is not total (neither antisymmetric nor transitive, as
    /// a user's may be) never panics a `Lenient` or `Exact` sort, which leaves a permutation of its input; an `Images`
    /// sort of a total order still orders.
    #[test]
    fn an_order_that_is_not_total_never_panics_and_leaves_a_permutation() {
        let bad = |a: u32, b: u32| match (a.wrapping_mul(7).wrapping_add(b.wrapping_mul(13))) % 3 {
            0 => Ordering::Less,
            1 => Ordering::Equal,
            _ => Ordering::Greater,
        };
        for n in [0usize, 1, 2, 7, 40, 300, 5000] {
            for order in [Order::Lenient, Order::Exact] {
                CMP_ERR.with(|c| c.set(false));
                let mut idx: Vec<u32> = (0..n as u32).rev().collect(); // alloc: Exec
                sort_by_order(&mut idx, order, &bad);
                idx.sort_unstable();
                assert_eq!(idx, (0..n as u32).collect::<Vec<u32>>(), "{order:?} over {n}: a permutation");
            }
        }
        let mut idx: Vec<u32> = (0..300).rev().collect(); // alloc: Exec
        sort_by_order(&mut idx, Order::Images, &|a, b| a.cmp(&b));
        assert!(idx.windows(2).all(|w| w[0] < w[1]));
    }

    /// TOTAL-ORDER (b): only a hosted key without `KFLAG_TOTAL` (a comparator that may reach user code) orders leniently,
    /// at any key (past `ROWCMP_KEYS` too); a builtin total comparator's key (`KFLAG_TOTAL`) keeps std's sort, as image keys
    /// do; a user comparator's key orders exactly, whatever else the table holds.
    #[test]
    fn only_a_comparator_that_may_reach_user_code_orders_leniently() {
        let order = |flags: &[u32]| {
            let n = flags.len() as u32;
            let mut t: Vec<u32> = vec![32, 2 * n, 1 | VAR_TBL, 0, 0, 0, 0, 24]; // alloc: Exec
            flags.iter().for_each(|&f| t.extend([0, f]));
            if flags.iter().any(|&f| f & KFLAG_HOSTED != 0) {
                t.extend(flags.iter().map(|&f| if f & KFLAG_HOSTED != 0 { 7 } else { 0 }));
            }
            unsafe { RowCmp::of(t.as_ptr()).order() }
        };
        let (h, tot, u) = (KFLAG_HOSTED, KFLAG_HOSTED | KFLAG_TOTAL, KFLAG_HOSTED | KFLAG_USER);
        assert_eq!(order(&[0, 0]), Order::Images, "image keys");
        assert_eq!(order(&[tot, 0]), Order::Images, "a builtin total comparator keeps std's sort");
        assert_eq!(order(&[tot, h]), Order::Lenient, "a comparator that may reach user code");
        assert_eq!(order(&[h, u]), Order::Exact, "a user comparator");
        let far = |f: u32| -> Vec<u32> { (0..ROWCMP_KEYS + 2).map(|k| if k == ROWCMP_KEYS + 1 { f } else { 0 }).collect() }; // alloc: Exec
        assert_eq!(order(&far(tot)), Order::Images, "past ROWCMP_KEYS, a builtin total comparator");
        assert_eq!(order(&far(h)), Order::Lenient, "past ROWCMP_KEYS, one that may reach user code");
    }
}
