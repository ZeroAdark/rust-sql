//! merge.join's host entries, as its lowered IR names them: PG's merge join errors raised in IR (`m11_raise`,
//! `m11_raise_no_proc`), the merge's view of its inner run (`m11_run_view`; D3105 (b): a Sort's put runs merged into one
//! and read through a window, `m11_run_open` / `m11_window`; D3190: a co-split claim's slice of it, `m11_run_slice`) and
//! the run's growth (`m11_run_grow` resident, `m11_run_full` within the owner's limit, else put). The entries live in the
//! family's raw half (raw.rs, rules R6 / R8); the errors' texts are here.
pub use super::raw::window::{MJ_WINDOW_KIND, W_CNT, W_IDX, W_J0, W_KIND, W_MARK};
pub use super::raw::{m11_raise, m11_raise_no_proc, m11_run_full, m11_run_grow, m11_run_held, m11_run_open, m11_run_slice, m11_run_view, m11_window};

/// `m11_raise` code 0 (PG `ExecMergeJoin`'s EXEC_MJ_NEXTINNER / EXEC_MJ_TESTOUTER): the inputs are not in the merge order.
pub const RAISE_OUT_OF_ORDER: u64 = 0;

#[cold]
#[inline(never)]
pub(super) fn raise_no_proc(typ: u64, family: u64) -> i64 {
    crate::contract::raise(format!("missing support function 1({typ},{typ}) in opfamily {family}"), None, *b"XX000") // alloc: Cold
}

#[cold]
#[inline(never)]
pub(super) fn raise(code: u64) -> i64 {
    let (msg, state) = match code {
        // elog(ERROR, ...): ERRCODE_INTERNAL_ERROR
        RAISE_OUT_OF_ORDER => ("mergejoin input data is out of order", *b"XX000"),
        _ => return crate::host_abi::bare(&format!("m11_raise: no error for code {code}")), // alloc: Cold
    };
    crate::contract::raise(msg, None, state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::sqlstate;
    use gravity_abi::breaker::{SortRun, SORTRUN_HDR};
    use gravity_abi::program::SPILL_REGION_GROW;
    use gravity_abi::status::ERROR;
    use gravity_abi::{Ctx, ProgramBlock};

    /// The fake driver's regions, every one kept alive (a grown run's old buffer is never read again). A process static,
    /// not a thread-local (the TLS census); private to this module, so only this module's one test (and its `fake_spill`)
    /// reaches it: the test clears it at its start and its end, each access under the lock.
    static REGIONS: std::sync::Mutex<Vec<Vec<u64>>> = std::sync::Mutex::new(Vec::new()); // alloc: Cold

    /// The driver's D254 `region_grow` as grow.rs does it: at least twice the length, `[0, head)` kept, the new length
    /// re-armed as the u32 at offset 8 (what a SortRun reads as its `row_bytes`).
    unsafe extern "C" fn fake_spill(_ctx: *mut Ctx, op: u32, arg: u64) -> i64 {
        if op == gravity_abi::program::SPILL_ARENA_CHUNK {
            return 0; // a kernel scratch's owner ask (0 bytes, `OpArena::own_owner_kind` / `slot_owner`): no slot owner, uncharged
        }
        assert_eq!(op, SPILL_REGION_GROW);
        let [need, head, tail] = *(arg as *const [u64; 3]);
        {
            let mut r = REGIONS.lock().expect("regions");
            let old = r.last().expect("a region");
            let len = old.len() * 8;
            let new = (2 * len).max((head + tail + need) as usize).next_multiple_of(8);
            let mut buf = vec![0u64; new / 8]; // alloc: Cold
            let (src, dst) = (old.as_ptr() as *const u8, buf.as_mut_ptr() as *mut u8);
            core::ptr::copy_nonoverlapping(src, dst, head as usize);
            core::ptr::copy_nonoverlapping(src.add(len - tail as usize), dst.add(new - tail as usize), tail as usize);
            (dst.add(8) as *mut u32).write(new as u32);
            r.push(buf);
            dst as i64
        }
    }

    /// Row `i`'s value: NULL every 5th, a long one every 7th, else short.
    fn value(i: usize) -> Option<Vec<u8>> {
        match i {
            _ if i % 5 == 0 => None,
            _ if i % 7 == 0 => Some(format!("{i:0>3000}").into_bytes()), // alloc: Cold
            _ => Some(format!("v{i}").into_bytes()), // alloc: Cold
        }
    }

    /// Every row's var slot against its value: NULL's slot empty, else its offset reaches the copy.
    unsafe fn check(run: *const u8, n: usize, rb: usize) {
        for i in 0..n {
            let slot = run.add(SORTRUN_HDR + i * rb + 24);
            let size = (slot as *const u32).read_unaligned() as usize;
            match value(i) {
                None => assert_eq!(size, 0, "row {i}: NULL's slot"),
                Some(v) => {
                    let off = (slot.add(4) as *const u32).read_unaligned() as usize;
                    assert_eq!(core::slice::from_raw_parts(slot.add(off), size), &v[..], "row {i}: the copy its slot reaches");
                }
            }
        }
    }

    /// W8-EQUIVCLASS-SORTOP: PG 18.6's text and SQLSTATE for a Sort by equivclass.sql's int8alias1 `<` (rig database OIDs).
    #[test]
    fn the_no_order_proc_raise_is_pgs() {
        assert_eq!(unsafe { m11_raise_no_proc(0, 16562, 1976) }, ERROR);
        let e = gravity_fn::take_error().expect("the raise stashes its error");
        assert_eq!(e.message(), "missing support function 1(16562,16562) in opfamily 1976");
        assert_eq!(e.sqlstate().0, sqlstate(*b"XX000"));
    }

    /// main 18:12Z (2): variable-width rows (NULLs, long values) across several grows keep every slot's copy and the run's
    /// `row_bytes` (region_grow's re-arm of offset 8 undone).
    #[test]
    fn a_grown_run_keeps_its_var_copies_and_row_bytes() {
        REGIONS.lock().expect("regions").clear();
        let rb = 32usize;
        // the sort table: no key, no image, one var column (slot at 24), the record word at 16
        let mut tbl = [0u32; crate::host_abi::SORT_TBL_HDR + 4];
        tbl[0] = rb as u32;
        tbl[2] = crate::host_abi::VAR_TBL;
        tbl[crate::host_abi::SORT_TBL_HDR..].copy_from_slice(&[1, 16, 0, 24]);
        let mut program = Box::new(ProgramBlock::NONE); // alloc: Cold
        program.spill = Some(fake_spill);
        let mut ctx = vec![0u64; gravity_abi::ctx::ctx_size(0) / 8]; // alloc: Cold
        let c = ctx.as_mut_ptr() as *mut Ctx;
        unsafe { (*c).program = &mut *program };
        let mut run = {
            let mut buf = vec![0u64; 1024 / 8]; // alloc: Cold
            unsafe { (buf.as_mut_ptr() as *mut u32).add(2).write(1024) };
            let p = buf.as_mut_ptr() as *mut u8;
            REGIONS.lock().expect("regions").push(buf);
            p
        };
        unsafe { SortRun::at_share(run, rb as u32) };
        let (mut grows, n) = (0, 400);
        for i in 0..n {
            loop {
                let r = unsafe { &mut *(run as *mut SortRun) };
                let fits = r.len < r.cap && match value(i) {
                    None => true,
                    Some(v) => unsafe {
                        core::ptr::write_bytes(run.add(SORTRUN_HDR + r.len as usize * rb), 0, rb);
                        crate::host_abi::sort_image_raw(run, v.as_ptr(), v.len() as u64, 24, 16) == 0
                    },
                };
                if fits {
                    if value(i).is_none() {
                        unsafe { core::ptr::write_bytes(run.add(SORTRUN_HDR + r.len as usize * rb), 0, rb) };
                    }
                    r.push();
                    break;
                }
                let before = r.cap;
                let nb = unsafe { m11_run_grow(c, run, tbl.as_ptr()) };
                assert!(nb > 0, "grow {grows}: {nb}");
                run = nb as usize as *mut u8;
                let g = unsafe { &*(run as *const SortRun) };
                assert_eq!(g.row_bytes as usize, rb, "grow {grows}: row_bytes restored after the re-arm");
                assert!(g.cap > before, "grow {grows}: the capacity grew");
                assert_eq!(g.len as usize, i, "grow {grows}: the rows kept");
                unsafe { check(run, i, rb) };
                grows += 1;
            }
        }
        unsafe { check(run, n, rb) };
        assert!(grows >= 3, "only {grows} grows");
        REGIONS.lock().expect("regions").clear();
    }
}
