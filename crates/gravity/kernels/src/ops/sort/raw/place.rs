//! A full sort run grown in place before it is put (memstart E3: start-small sorts). The driver opens a Sort's region at
//! its start (`grant::starts_small`, `grant::START_BYTES`) inside a reservation of its work_mem (the leader's at its
//! width), as it opens a hash breaker's. The run fills its start; at its full the IR asks `sort_grow` before it puts or
//! cuts: the region doubles in place (`SPILL_REGION_PLACE`, the base unchanged) and the run takes the room: the rows stay
//! where they are, the var copies move up to the new rows' end and every held row's var slot offsets move by as much (as
//! mergejoin's `grow` moves them). A region that did not start small, or a refusal (past the reservation, no pool room
//! now), answers 0 and the IR puts the run as before, as PostgreSQL's Sort spills past work_mem.
use gravity_abi::breaker::{SortRun, SORTRUN_HDR, SORTRUN_VAR_WORDS};
use gravity_abi::ctx::cancel_pending;
use gravity_abi::program::{partial, region_grow_place};
use gravity_abi::status::{CANCELLED, ERROR};
use gravity_abi::Ctx;

use crate::host_abi::var_shape;

use super::super::polled::{Poll, Stopped};
use super::super::rowsort::{SEALED, SORTRUN_BIG, WANT};

/// D4412: the bytes of a grown run's var copies moved between two polls (`cancelpoll::PIECE`'s value): a 1 MiB move
/// faults in at most 256 fresh pages, a few ms where the kernel's own faults are slow.
const MOVE_PIECE: usize = 1 << 20;

crate::contract::host! {
    /// The full run `run` grown in place within its region's reservation (`grew`): 1, the row is written again into it;
    /// 0, the IR puts (or cuts) the run as before; `CANCELLED` (D4412), the statement's abort seen while the grow moved
    /// the run's var copies (the IR leaves through its exit; the run is left empty). `tbl` is the run's emit table for a
    /// var row, null for a fixed one.
    /// # Safety
    /// `ctx` is bound; `run` is the run the IR fills (the pipeline's partial, or a D1330 big run); `tbl` is null or its
    /// emit table.
    pub unsafe fn sort_grow(ctx: *mut Ctx, run: *mut u8, tbl: *const u32) -> i64 {
        guard!(ERROR, {
            let run = run as *mut SortRun;
            let pending = || unsafe { cancel_pending(ctx) };
            match grew(ctx, run, tbl, &mut Poll::new(&pending), MOVE_PIECE) {
                Ok(true) => {}
                Ok(false) => return 0,
                Err(Stopped) => return CANCELLED,
            }
            // D1330: an empty run that had no room for one row's copies armed its want (`want_room`): the room it now has
            // is tried first, so the want is spent; armed again when the row still does not fit
            WANT.with(|c| if c.get().0 == run as usize { c.set((0, 0, 0)) });
            1
        })
    }
}

/// Whether `run` grew in place: the pipeline's own run (never a D1330 big run, a sized spool run or a sealed one) in a
/// region that started small, its extent at least doubled by the driver (`region_grow_place`), its rows kept and its
/// var copies, trailer and slot offsets moved up to the new rows' end.
/// D4412: the move and the slots' re-pointing poll the statement through `poll` (a tick per `piece` bytes moved, a
/// batch per row; box 19's sortlat rig measured one unpolled move at 0.95-1.3 s per worker, the new extent's faults).
/// `Err` when its abort is pending: the run is then emptied in its new extent (`SortRun::clear`), a valid run that
/// nothing reads (the IR leaves through its exit and the statement stops).
/// # Safety
/// As `sort_grow`.
pub(crate) unsafe fn grew(ctx: *mut Ctx, run: *mut SortRun, tbl: *const u32, poll: &mut Poll<'_>, piece: usize) -> Result<bool, Stopped> {
    if run as *mut u8 != partial(ctx) {
        return Ok(false);
    }
    let r = &*run;
    if r.region() != 0 || r.pad[1] == SORTRUN_BIG || r.pad[0] == SEALED || r.row_bytes == 0 || r.cap == 0 {
        return Ok(false);
    }
    let (cap, rb, n, words) = (r.cap as usize, r.row_bytes as usize, r.len as usize, SORTRUN_VAR_WORDS as usize);
    let end = SORTRUN_HDR + cap * rb;
    let (top, var_bytes) = (r.var_top() as usize, r.var_bytes());
    if top > end || top < SORTRUN_HDR + n * rb {
        return Ok(false);
    }
    // one more row past the run's extent at least; the driver doubles the region (`grow::place_len`)
    let len = region_grow_place(ctx, (rb + words) as u64, (end + words) as u64) as usize;
    if len == 0 {
        return Ok(false);
    }
    let base = run as *mut u8;
    // the re-arm wrote the new length over the run's `row_bytes` (offset 8): restored
    (base.add(8) as *mut u32).write(rb as u32);
    let ncap = ((len - SORTRUN_HDR - words) / rb).min(u32::MAX as usize);
    if ncap <= cap {
        return Ok(false);
    }
    let delta = (ncap - cap) * rb;
    if move_up(poll, base.add(top), end - top, delta, piece).is_err() {
        return stopped(run, ncap);
    }
    (*run).cap = ncap as u32;
    let w = base.add(end + delta) as *mut u32;
    w.write_unaligned((top + delta) as u32);
    w.add(1).write_unaligned(var_bytes);
    if let (Some(v), true) = (if tbl.is_null() { None } else { var_shape(tbl) }, top < end) {
        for i in 0..n {
            if poll.batch(i).is_err() {
                return stopped(run, ncap);
            }
            let row = base.add(SORTRUN_HDR + i * rb);
            for c in 0..v.nvar {
                let slot = row.add(*v.slots.add(2 * c + 1) as usize);
                if (slot as *const u32).read_unaligned() != 0 {
                    let off = slot.add(4) as *mut u32;
                    off.write_unaligned(off.read_unaligned() + delta as u32);
                }
            }
        }
    }
    Ok(true)
}

/// D4412: a grow stopped for its statement's abort, its move or its slots part done: the run emptied in its new extent
/// of `ncap` rows (no rows, no var copies: a valid run that nothing reads).
/// # Safety
/// `run` is the live run `grew` grows, its region holding `ncap` rows.
unsafe fn stopped(run: *mut SortRun, ncap: usize) -> Result<bool, Stopped> {
    (*run).cap = ncap as u32;
    (*run).clear();
    Err(Stopped)
}

/// D4412: `len` bytes at `src` moved `delta` bytes up (one overlapping move) in `piece`-byte pieces from the top down,
/// a tick before each: every piece's destination lies above every byte not yet moved, so the pieces give the bytes the
/// single move gave. `Err` when the statement's abort is pending (the move left part done).
/// # Safety
/// `src .. src + len + delta` is writable and no one else reads it.
unsafe fn move_up(poll: &mut Poll<'_>, src: *mut u8, len: usize, delta: usize, piece: usize) -> Result<(), Stopped> {
    let mut hi = len;
    while hi > 0 {
        poll.tick()?;
        let lo = hi.saturating_sub(piece.max(1));
        core::ptr::copy(src.add(lo), src.add(lo + delta), hi - lo);
        hi = lo;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_abi::program::SPILL_REGION_PLACE;
    use gravity_abi::{Partials, ProgramBlock};

    /// The region's reservation and its start, in bytes.
    const RESERVE: usize = 64 << 10;
    const START: usize = 1024;

    /// The fake driver's region length (the u32 at offset 8 it re-arms); a process static, not a thread-local (the TLS
    /// census): only this module's tests reach it, each under `SERIAL`.
    static LEN: std::sync::Mutex<usize> = std::sync::Mutex::new(0);
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The driver's `region_grow_place` as grow.rs does it: at least twice the length and `head + need`, at most the
    /// reservation, in place (the base unchanged), the new length re-armed as the u32 at offset 8; 0 past the reservation.
    unsafe extern "C" fn place(ctx: *mut Ctx, op: u32, arg: u64) -> i64 {
        if op == gravity_abi::program::SPILL_ARENA_CHUNK {
            return 0; // a kernel scratch's owner ask (0 bytes, `OpArena::own_owner_kind` / `slot_owner`): no slot owner, uncharged
        }
        assert_eq!(op, SPILL_REGION_PLACE, "the grow's only host op");
        let [need, head] = *(arg as *const [u64; 2]);
        let mut len = LEN.lock().expect("len");
        let want = (head + need) as usize;
        let new = (2 * *len).max(want).min(RESERVE);
        if want > RESERVE || new <= *len {
            return 0;
        }
        (partial(ctx).add(8) as *mut u32).write(new as u32);
        *len = new;
        new as i64
    }

    /// A host with no such region (any other breaker's): every ask refused.
    unsafe extern "C" fn refuse(_ctx: *mut Ctx, op: u32, _arg: u64) -> i64 {
        if op == gravity_abi::program::SPILL_ARENA_CHUNK {
            return 0; // a kernel scratch's owner ask (0 bytes, `OpArena::own_owner_kind` / `slot_owner`): no slot owner, uncharged
        }
        assert_eq!(op, SPILL_REGION_PLACE);
        0
    }

    /// Row `i`'s value: NULL every 5th, a long one every 7th (and the first), else short.
    fn value(i: usize) -> Option<Vec<u8>> {
        match i {
            _ if i % 5 == 1 => None,
            _ if i % 7 == 0 => Some(format!("{i:0>3000}").into_bytes()), // alloc: Cold
            _ => Some(format!("v{i}").into_bytes()), // alloc: Cold
        }
    }

    /// Every row's var slot against its value: NULL's slot empty, else its offset reaches the copy.
    unsafe fn check(run: *const u8, n: usize, rb: usize) {
        for i in 0..n {
            let slot = run.add(SORTRUN_HDR + i * rb + 24);
            let size = (slot as *const u32).read_unaligned() as usize;
            assert_eq!((run.add(SORTRUN_HDR + i * rb) as *const u64).read_unaligned(), i as u64, "row {i}: its tag");
            match value(i) {
                None => assert_eq!(size, 0, "row {i}: NULL's slot"),
                Some(v) => {
                    let off = (slot.add(4) as *const u32).read_unaligned() as usize;
                    assert_eq!(core::slice::from_raw_parts(slot.add(off), size), &v[..], "row {i}: the copy its slot reaches");
                }
            }
        }
    }

    /// A bound ctx whose partial is a region of `RESERVE` bytes armed at `START`, the host `spill`.
    struct Bound {
        ctx: Vec<u64>,
        _program: Box<ProgramBlock>,
        _partials: Box<Partials>,
        own: Vec<u64>,
    }

    impl Bound {
        fn new(spill: unsafe extern "C" fn(*mut Ctx, u32, u64) -> i64) -> Bound {
            let mut own = vec![0u64; RESERVE / 8]; // alloc: Cold
            unsafe { (own.as_mut_ptr() as *mut u32).add(2).write(START as u32) };
            *LEN.lock().expect("len") = START;
            let mut partials = Box::new(Partials::two(own.as_mut_ptr() as *mut u8, [core::ptr::null_mut(); 2])); // alloc: Cold
            let mut program = Box::new(ProgramBlock::NONE); // alloc: Cold
            program.partial = &mut *partials;
            program.spill = Some(spill);
            let mut ctx = vec![0u64; gravity_abi::ctx::ctx_size(0) / 8]; // alloc: Cold
            unsafe { (*(ctx.as_mut_ptr() as *mut Ctx)).program = &mut *program };
            Bound { ctx, _program: program, _partials: partials, own }
        }

        fn ctx(&mut self) -> *mut Ctx {
            self.ctx.as_mut_ptr() as *mut Ctx
        }
    }

    /// A var run that started small grows in place at each full, the base never moving: NULLs, long values and an empty
    /// run whose first row's copy passes its start (D1330's want: spent by the grow, armed again only if it still does
    /// not fit) keep every slot's copy, the run's `row_bytes` and tags; past the reservation the grow answers 0.
    #[test]
    fn a_started_small_run_grows_in_place_and_keeps_its_var_copies() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let rb = 32usize;
        // the sort table: no key, no image, one var column (slot at 24), the record word at 16
        let mut tbl = [0u32; crate::host_abi::SORT_TBL_HDR + 4];
        tbl[0] = rb as u32;
        tbl[2] = crate::host_abi::VAR_TBL;
        tbl[crate::host_abi::SORT_TBL_HDR..].copy_from_slice(&[1, 16, 0, 24]);
        let mut b = Bound::new(place);
        let (c, run) = (b.ctx(), b.own.as_mut_ptr() as *mut u8);
        unsafe { SortRun::at_share(run, rb as u32) };
        let (mut grows, mut n) = (0, 0);
        'rows: loop {
            loop {
                let r = unsafe { &mut *(run as *mut SortRun) };
                let row = unsafe { run.add(SORTRUN_HDR + r.len as usize * rb) };
                let fits = r.len < r.cap && match value(n) {
                    None => true,
                    Some(v) => unsafe {
                        core::ptr::write_bytes(row, 0, rb);
                        let st = crate::host_abi::sort_image_raw(run, v.as_ptr(), v.len() as u64, 24, 16);
                        assert!(st == 0 || st == 1, "row {n}: the copy's status {st} (an armed want left unspent answers below zero)");
                        st == 0
                    },
                };
                if fits {
                    if value(n).is_none() {
                        unsafe { core::ptr::write_bytes(row, 0, rb) };
                    }
                    unsafe { (row as *mut u64).write_unaligned(n as u64) };
                    r.push();
                    n += 1;
                    break;
                }
                let before = r.cap;
                match unsafe { sort_grow(c, run, tbl.as_ptr()) } {
                    1 => {}
                    0 => break 'rows,
                    st => panic!("grow {grows}: status {st}"),
                }
                let g = unsafe { &*(run as *const SortRun) };
                assert_eq!(g.row_bytes as usize, rb, "grow {grows}: row_bytes restored after the re-arm");
                assert!(g.cap > before, "grow {grows}: the capacity grew");
                assert_eq!(g.len as usize, n, "grow {grows}: the rows kept");
                assert_eq!(WANT.with(|w| w.get().0), 0, "grow {grows}: the want spent");
                unsafe { check(run, n, rb) };
                grows += 1;
            }
        }
        unsafe { check(run, n, rb) };
        assert!(grows >= 5, "only {grows} grows");
        assert_eq!(*LEN.lock().expect("len"), RESERVE, "grown to the reservation, then refused");
        let r = unsafe { &*(run as *const SortRun) };
        assert!(r.len as usize == n && n > 64, "{n} rows held");
    }

    /// A fixed run whose region did not start small (the host refuses), a run that is not the partial (a D1330 big run)
    /// and a sealed run never grow, and the run is left as it was.
    #[test]
    fn a_run_that_cannot_grow_answers_0_and_is_unchanged() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let rb = 40u32;
        let mut b = Bound::new(refuse);
        let (c, run) = (b.ctx(), b.own.as_mut_ptr() as *mut u8);
        let r = unsafe { SortRun::at_share(run, rb) };
        while r.len < r.cap {
            r.push();
        }
        let (cap, len) = (r.cap, r.len);
        assert_eq!(unsafe { sort_grow(c, run, core::ptr::null()) }, 0, "a refused region: put as before");
        let r = unsafe { &*(run as *const SortRun) };
        assert_eq!((r.cap, r.len, r.row_bytes), (cap, len, rb));
        let mut b = Bound::new(place);
        let c = b.ctx();
        let mut other = vec![0u64; START / 8]; // alloc: Cold
        let o = other.as_mut_ptr() as *mut u8;
        unsafe { (o.add(8) as *mut u32).write(START as u32) };
        unsafe { SortRun::at_share(o, rb) };
        assert_eq!(unsafe { sort_grow(c, o, core::ptr::null()) }, 0, "not the partial: a big run is put");
        let own = b.own.as_mut_ptr() as *mut u8;
        let r = unsafe { SortRun::at_share(own, rb) };
        r.pad[0] = SEALED;
        assert_eq!(unsafe { sort_grow(c, own, core::ptr::null()) }, 0, "a sealed run never grows");
        assert_eq!(*LEN.lock().expect("len"), START, "nothing asked of the host");
    }

    /// D4412's rows: NULL every 5th, else a 21-byte value (every row's copy fits a start).
    fn small(i: usize) -> Option<Vec<u8>> {
        (i % 5 != 1).then(|| format!("w{i:0>20}").into_bytes()) // alloc: Cold
    }

    /// The run filled with `small` rows from row `n` on until the next row does not fit (its tag at 0, its slot at 24);
    /// the rows held then. A row needs its own bytes below the copies (the IR's `var_room`) before its copy is taken.
    unsafe fn fill(run: *mut u8, rb: usize, mut n: usize) -> usize {
        loop {
            let r = &mut *(run as *mut SortRun);
            if r.len >= r.cap || (r.var_top() as usize) < SORTRUN_HDR + (r.len as usize + 1) * rb {
                return n;
            }
            let row = run.add(SORTRUN_HDR + r.len as usize * rb);
            core::ptr::write_bytes(row, 0, rb);
            if let Some(v) = small(n) {
                if crate::host_abi::sort_image_raw(run, v.as_ptr(), v.len() as u64, 24, 16) != 0 {
                    return n;
                }
            }
            (row as *mut u64).write_unaligned(n as u64);
            r.push();
            n += 1;
        }
    }

    /// Every `small` row's tag and the copy its slot reaches.
    unsafe fn check_small(run: *const u8, n: usize, rb: usize) {
        for i in 0..n {
            let slot = run.add(SORTRUN_HDR + i * rb + 24);
            let size = (slot as *const u32).read_unaligned() as usize;
            assert_eq!((run.add(SORTRUN_HDR + i * rb) as *const u64).read_unaligned(), i as u64, "row {i}: its tag");
            match small(i) {
                None => assert_eq!(size, 0, "row {i}: NULL's slot"),
                Some(v) => {
                    let off = (slot.add(4) as *const u32).read_unaligned() as usize;
                    assert_eq!(core::slice::from_raw_parts(slot.add(off), size), &v[..], "row {i}: the copy its slot reaches");
                }
            }
        }
    }

    /// D4412's abort byte: set by `place_then_abort` once the region grew, as the ladder's R6 lands while a run's grow
    /// moves its var copies (one take per test, under `SERIAL`).
    static ABORT: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

    /// `place`, then the statement's abort set: the cancel arrives after the driver grew the region, during the move.
    unsafe extern "C" fn place_then_abort(ctx: *mut Ctx, op: u32, arg: u64) -> i64 {
        let r = place(ctx, op, arg);
        ABORT.store(gravity_abi::ctx::ABORT_BIT, core::sync::atomic::Ordering::Release);
        r
    }

    /// D4412 (born red): a grow in place polls its statement while it moves the run's var copies up to the new rows' end
    /// and re-points the slots. Box 19's sortlat rig (memtest cell e2's sort, work_mem 2GB, 4 workers) measured that
    /// move at 0.95-1.3 s per worker with no poll (628 MiB of the new extent faulted in, ~95 % system time), and an R6
    /// aimed at the input phase took 946-966 ms to answer. A cancel that arrives once the region grew stops the grow
    /// (`CANCELLED`, the IR's exit), and the run is left a valid empty run (the statement stops; nothing reads its rows).
    #[test]
    fn a_grow_whose_statement_is_canceled_during_the_move_stops_and_leaves_a_valid_run() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let rb = 32usize;
        let mut tbl = [0u32; crate::host_abi::SORT_TBL_HDR + 4];
        tbl[0] = rb as u32;
        tbl[2] = crate::host_abi::VAR_TBL;
        tbl[crate::host_abi::SORT_TBL_HDR..].copy_from_slice(&[1, 16, 0, 24]);
        ABORT.store(0, core::sync::atomic::Ordering::Release);
        let mut b = Bound::new(place_then_abort);
        let (c, run) = (b.ctx(), b.own.as_mut_ptr() as *mut u8);
        unsafe { (*c).cancel = gravity_abi::word(&ABORT as *const core::sync::atomic::AtomicU8 as *const u8) };
        unsafe { SortRun::at_share(run, rb as u32) };
        // the start filled with rows and their copies until the next row does not fit
        let n = unsafe { fill(run, rb, 0) };
        assert!(n > 2, "{n} rows in the start");
        unsafe { check_small(run, n, rb) };
        let st = unsafe { sort_grow(c, run, tbl.as_ptr()) };
        assert_eq!(*LEN.lock().expect("len"), 2 * START, "the region grew before the cancel");
        assert_eq!(st, gravity_abi::status::CANCELLED, "a cancel during the grow's move stops it (got {st}: moved unpolled)");
        let r = unsafe { &*(run as *const SortRun) };
        assert_eq!(r.row_bytes as usize, rb, "row_bytes restored after the re-arm");
        let (top, end) = (r.var_top() as usize, SORTRUN_HDR + r.cap as usize * rb);
        assert!(r.len <= r.cap && top <= end && top >= SORTRUN_HDR + r.len as usize * rb, "a valid run: len {} cap {} top {top} end {end}", r.len, r.cap);
        assert_eq!((r.len, top, r.var_bytes()), (0, end, 0), "stopped mid-grow: the run emptied in its new extent");
        ABORT.store(0, core::sync::atomic::Ordering::Release);
    }

    /// D4412: the move in pieces gives the bytes the one overlapping move gave, whatever the piece, the overlap or the
    /// length (the pieces run from the top down).
    #[test]
    fn a_move_in_pieces_gives_the_single_moves_bytes() {
        let never = || false;
        for (len, delta) in [(0usize, 8usize), (1, 1), (100, 3), (100, 100), (100, 250), (4099, 17), (4099, 4096)] {
            for piece in [1usize, 7, 64, 4096, 1 << 20] {
                let src: Vec<u8> = (0..len + delta).map(|i| (i * 31 % 251) as u8).collect(); // alloc: Cold
                let (mut want, mut got) = (src.clone(), src); // alloc: Cold
                unsafe { core::ptr::copy(want.as_ptr(), want.as_mut_ptr().add(delta), len) };
                assert_eq!(unsafe { move_up(&mut Poll::new(&never), got.as_mut_ptr(), len, delta, piece) }, Ok(()));
                assert_eq!(got, want, "len {len} delta {delta} piece {piece}");
            }
        }
    }

    /// D4412: a grow in 64-byte pieces with no cancel keeps every row's copy (the single move's result); a grow whose
    /// statement is canceled at its fourth piece stops there (three pieces moved) and leaves a valid empty run in the new
    /// extent.
    #[test]
    fn a_grow_in_pieces_keeps_every_copy_and_one_stopped_mid_move_leaves_an_empty_run() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let rb = 32usize;
        let mut tbl = [0u32; crate::host_abi::SORT_TBL_HDR + 4];
        tbl[0] = rb as u32;
        tbl[2] = crate::host_abi::VAR_TBL;
        tbl[crate::host_abi::SORT_TBL_HDR..].copy_from_slice(&[1, 16, 0, 24]);
        let mut b = Bound::new(place);
        let (c, run) = (b.ctx(), b.own.as_mut_ptr() as *mut u8);
        unsafe { SortRun::at_share(run, rb as u32) };
        let n = unsafe { fill(run, rb, 0) };
        let r = unsafe { &*(run as *const SortRun) };
        let (cap, top) = (r.cap, r.var_top() as usize);
        let var = SORTRUN_HDR + cap as usize * rb - top;
        assert!(n > 2 && var > 3 * 64, "{n} rows, {var} bytes of copies");
        let never = || false;
        assert_eq!(unsafe { grew(c, run as *mut SortRun, tbl.as_ptr(), &mut Poll::new(&never), 64) }, Ok(true));
        let r = unsafe { &*(run as *const SortRun) };
        assert!(r.cap > cap && r.len as usize == n, "grown: cap {} -> {}, {} rows", cap, r.cap, r.len);
        unsafe { check_small(run, n, rb) };
        // the grown extent filled again, then a cancel at the fourth piece of the next grow's move
        let n = unsafe { fill(run, rb, n) };
        unsafe { check_small(run, n, rb) };
        let polls = core::cell::Cell::new(0usize);
        let late = || {
            polls.set(polls.get() + 1);
            polls.get() > 3
        };
        let before = *LEN.lock().expect("len");
        assert_eq!(unsafe { grew(c, run as *mut SortRun, tbl.as_ptr(), &mut Poll::new(&late), 64) }, Err(Stopped));
        assert_eq!(polls.get(), 4, "stopped at the fourth piece");
        let len = *LEN.lock().expect("len");
        assert!(len > before, "the region grew before the stop");
        let r = unsafe { &*(run as *const SortRun) };
        let end = SORTRUN_HDR + r.cap as usize * rb;
        assert_eq!(r.cap as usize, (len - SORTRUN_HDR - SORTRUN_VAR_WORDS as usize) / rb, "the run spans the new extent");
        assert_eq!((r.len, r.var_top() as usize, r.var_bytes(), r.row_bytes as usize), (0, end, 0, rb), "a valid empty run");
    }
}
