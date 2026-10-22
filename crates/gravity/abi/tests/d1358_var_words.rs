//! D1358: a sort run's var words move on every reservation. `SortRun::reserve_var` stored the new var_top from a closure
//! through `var_words(&self)`, a pointer taken from a shared borrow of the run: undefined behaviour, and once the closure
//! was out of line (a `-Zub-checks` build of gravity_abi) LLVM's instcombine dropped the store, so every by-reference copy
//! of a var run took the same slot (a numeric ORDER BY returned one value on every row). Under Miri (tree borrows) the
//! parent's write through the frozen pointer is reported here; the fix writes through `var_words_mut(&mut self)`.
use gravity_abi::breaker::SortRun;

const ROW: u32 = 32;

fn run_in(buf: &mut [u64]) -> &mut SortRun {
    let p = buf.as_mut_ptr() as *mut u8;
    // SAFETY: a share-armed region of the buffer's bytes (word 2 = the share: `at_rows`'s first touch); `buf` outlives it.
    unsafe {
        (*(p as *mut SortRun)).row_bytes = (buf.len() * 8) as u32;
        SortRun::at_rows(p, ROW, 8)
    }
}

#[test]
fn every_reservation_takes_its_own_slot_below_the_last() {
    let mut buf = vec![0u64; 1024];
    let run = run_in(&mut buf);
    let top0 = run.var_top();
    let mut last = top0;
    for n in [40u32, 8, 16, 72] {
        let at = run.reserve_var(n).expect("room");
        assert_eq!(at, last - n, "a reservation of {n} B sits right below the previous one");
        assert_eq!(run.var_top(), at, "var_top moved to the reservation");
        last = at;
    }
    assert_eq!(top0 - last, 40 + 8 + 16 + 72);
}

#[test]
fn a_reservation_past_the_next_row_is_refused_and_leaves_var_top() {
    let mut buf = vec![0u64; 256];
    let run = run_in(&mut buf);
    let top = run.var_top();
    let floor = run.row(0) as usize - run as *const SortRun as usize + ROW as usize;
    assert!(run.reserve_var(top - floor as u32 + 8).is_none());
    assert_eq!(run.var_top(), top);
    run.push();
    run.clear();
    assert_eq!(run.var_top(), top, "clear puts var_top back at the rows' end");
    assert_eq!(run.var_bytes(), 0);
}
