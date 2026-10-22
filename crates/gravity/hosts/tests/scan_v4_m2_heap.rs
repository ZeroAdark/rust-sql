//! M2's heap page-window host extras (hosts v4/m2_heap.rs, M2-inventory rows 3, 8, 10) over the fixture heap: the
//! all-visible flag, the count-only walk that touches no tuple, the hint-bit test's boundary against the MVCC seam, and
//! late decode (a lane deformed for the site's selection only), and the single pass over an all-visible page's line
//! pointers (V4R-371: `PAGE_UNREAD`, the headers left to the program) beside the header walk it falls back to.
use gravity_hosts_rig as common;

use std::rc::Rc;
use std::sync::atomic::AtomicU8;

use common::*;
use gravity_abi::site::site::{PAGE_FULL, PAGE_IOTA, PAGE_NONULLS, PAGE_UNREAD};
use gravity_abi::Lane;
use gravity_driver::{blank_site, BindingRef, CommandId, Identity, PerTuple, Pins, Snapshot, Source, Window};
use gravity_hosts::ops::scan::heap_window::{hinted_census, LANE_LATE, PAGE_ALL_VISIBLE};
use gravity_hosts::{HeapRangeSource, RangeRecipe};
use mcx::MemoryContext;
use types_rel::Relation;
use types_snapshot::{SnapshotData, SnapshotType};
use types_tuple::{HEAP_HASNULL, HEAP_NATTS_MASK, HEAP_XMAX_INVALID, HEAP_XMIN_COMMITTED};

static FLAG: AtomicU8 = AtomicU8::new(0);
const MAX_LANES: usize = 8;

fn binding_ref(snap: &Rc<SnapshotData<'static>>) -> BindingRef<'static> {
    BindingRef { snapshot: Snapshot(snap as *const _ as u64), command_id: CommandId(0), params: &[], abort_flag: &FLAG, pins: Pins(0), per_tuple: PerTuple(0), identity: Identity::default(), resolved: None, ordinal: None }
}

fn mcx(name: &'static str) -> mcx::Mcx<'static> {
    let ctx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new(name)));
    ctx.mcx()
}

fn source(mcx: mcx::Mcx<'static>, attnos: &[i16], snap: &Rc<SnapshotData<'static>>) -> HeapRangeSource {
    let heap = Relation::open(heap_relation_data(mcx), None);
    let mut src = HeapRangeSource::new(RangeRecipe::from_plan(&heap, attnos).unwrap()).raw(true);
    src.bind(&binding_ref(snap)).unwrap();
    src
}

/// Block `blk` of the fixture heap, its raw bytes handed to `f` while pinned.
fn with_page(blk: u32, f: impl FnOnce(&mut types_storage::bufpage::PageMut<'_>)) {
    let loc = types_storage::RelFileLocator::new(1663, 5, HEAP_OID);
    let buf = bufmgr_seams::read_buffer_without_relcache::call(loc, types_core::ForkNumber::MAIN_FORKNUM, blk, types_storage::ReadBufferMode::Normal, None, true).unwrap();
    let mut page = unsafe { types_storage::bufpage::PageMut::from_raw(bufmgr_seams::buffer_get_page::call(buf)) };
    f(&mut page);
    bufmgr_seams::release_buffer::call(buf).unwrap();
}

/// Every normal line pointer of `blk`: `(offset, tuple offset, tuple length)`.
fn lines(blk: u32) -> Vec<(u16, usize, usize)> {
    let mut out = Vec::new();
    with_page(blk, |p| {
        let base = p.as_mut_ptr();
        let lower = unsafe { core::ptr::read_unaligned(base.add(12) as *const u16) } as usize;
        for off in 1..=((lower - 24) / 4) as u16 {
            let lp = unsafe { core::ptr::read_unaligned(base.add(24 + (off as usize - 1) * 4) as *const u32) };
            if (lp >> 15) & 3 == 1 {
                out.push((off, (lp & 0x7fff) as usize, (lp >> 17) as usize));
            }
        }
    });
    out
}

/// `bits` or-ed into the infomask of every tuple of `blk`.
fn hint_page(blk: u32, bits: u16) {
    let ls = lines(blk);
    with_page(blk, |p| {
        let base = p.as_mut_ptr();
        for (_, toff, _) in ls {
            unsafe {
                let m = base.add(toff + 20) as *mut u16;
                m.write_unaligned(m.read_unaligned() | bits);
            }
        }
    });
}

/// Each window's `(rows, page_flags, block)`, the claim `0..nblocks` walked.
fn windows(src: &mut HeapRangeSource) -> Vec<(u32, u16, u32)> {
    let n = src.nblocks() as u64;
    src.seek(&[0, n], &[0, 0]).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let mut out = Vec::new();
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let rows = src.window(&mut w).unwrap();
        if rows == 0 {
            return out;
        }
        drop(w);
        out.push((rows, site.page_flags, site.pos.block));
    }
}

/// Each window's `(page_flags, visible offsets)`: the offset lane is lane `natts` (the recipe's attribute count).
fn offsets(src: &mut HeapRangeSource, natts: usize) -> Vec<(u16, Vec<u16>)> {
    let n = src.nblocks() as u64;
    src.seek(&[0, n], &[0, 0]).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let mut out = Vec::new();
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let rows = src.window(&mut w).unwrap() as usize;
        if rows == 0 {
            return out;
        }
        let offs = unsafe { core::slice::from_raw_parts(w.lanes()[natts].ptr as *const u16, rows) }.to_vec();
        drop(w);
        out.push((site.page_flags, offs));
    }
}

/// A line pointer of `base`'s page at `off`.
unsafe fn lp_at(base: *mut u8, off: usize) -> *mut u32 {
    base.add(24 + (off - 1) * 4) as *mut u32
}

/// The all-visible flag rides only on a page answered from its line pointers; the raw window names its pinned buffer.
#[test]
fn the_all_visible_flag_marks_only_the_pages_answered_from_their_line_pointers() {
    let _g = serial();
    install();
    reset_fixture();
    let mcx = mcx("m2-heap-allvis");
    load_rows(mcx, &(0..700).collect::<Vec<i32>>());
    let snap = static_mvcc_snapshot();
    let mut src = source(mcx, &[1], &snap);
    with_page(1, |p| p.set_all_visible());
    let ws = windows(&mut src);
    assert!(ws.len() >= 3);
    for (rows, flags, blk) in ws.iter().copied() {
        let walked = if blk == 1 { PAGE_UNREAD } else { PAGE_NONULLS | PAGE_FULL };
        assert_eq!(flags & (PAGE_NONULLS | PAGE_FULL | PAGE_UNREAD), walked, "block {blk}: an all-visible page's headers unread");
        assert_eq!(flags & PAGE_ALL_VISIBLE != 0, blk == 1, "block {blk}: all-visible only where the page says so");
        assert!(rows > 0);
    }
    // a snapshot taken in recovery tests every tuple: no page is answered from its line pointers
    let mut rec = SnapshotData::sentinel(mcx, SnapshotType::SNAPSHOT_MVCC);
    rec.takenDuringRecovery = true;
    src.unbind();
    let rec = Rc::new(rec);
    src.bind(&binding_ref(&rec)).unwrap();
    assert!(windows(&mut src).iter().all(|w| w.1 & PAGE_ALL_VISIBLE == 0));
    src.unbind();
    quiesced();
}

/// H3 (row 10): a recipe that wants no attribute (count(*)) walks an all-visible page's line pointers and touches no tuple:
/// a line pointer whose length the header-reading walk refuses as corrupt is counted here, and the attribute walk refuses it.
#[test]
fn a_count_only_walk_reads_no_tuple_header() {
    let _g = serial();
    install();
    reset_fixture();
    let mcx = mcx("m2-heap-count");
    load_rows(mcx, &(0..400).collect::<Vec<i32>>());
    let snap = static_mvcc_snapshot();
    with_page(0, |p| p.set_all_visible());
    let (off, _, _) = lines(0)[3];
    let before = windows(&mut source(mcx, &[], &snap));
    // a zero-length tuple: `lp_len` bits 17.. cleared
    with_page(0, |p| unsafe {
        let lp = p.as_mut_ptr().add(24 + (off as usize - 1) * 4) as *mut u32;
        lp.write_unaligned(lp.read_unaligned() & 0x1ffff);
    });
    let mut count = source(mcx, &[], &snap);
    let after = windows(&mut count);
    assert_eq!(after, before, "the count-only walk sees line pointers only");
    assert_eq!(after[0].1, PAGE_NONULLS | PAGE_FULL | PAGE_ALL_VISIBLE | PAGE_IOTA, "no attribute wanted: the flags hold trivially");
    count.unbind();
    let mut attrs = source(mcx, &[1], &snap);
    let n = attrs.nblocks() as u64;
    attrs.seek(&[0, n], &[0, 0]).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let mut site = blank_site(lanes.as_mut_ptr());
    assert!(attrs.window(&mut Window::over(&mut site)).is_err(), "the attribute walk reads the header and refuses the line pointer");
    attrs.unbind();
    quiesced();
}

/// H2 (row 3): the hint bits settle a committed live tuple only when its xmin precedes the snapshot's xmin; at the
/// snapshot's xmin the MVCC seam decides (xmin is in the snapshot's running set), never the hints alone. The rig's seam
/// answers "visible" to every tuple, so the census tells the two paths apart.
#[test]
fn the_hint_bits_answer_only_below_the_snapshot_xmin() {
    let _g = serial();
    install();
    reset_fixture();
    let mcx = mcx("m2-heap-hint");
    load_rows(mcx, &(0..600).collect::<Vec<i32>>());
    hint_page(1, HEAP_XMIN_COMMITTED | HEAP_XMAX_INVALID);
    let on1 = lines(1).len() as u32;
    let snap = |xmin: u32, xip: &[u32]| {
        let mut s = SnapshotData::sentinel(mcx, SnapshotType::SNAPSHOT_MVCC);
        (s.xmin, s.xmax) = (xmin, 11);
        for &x in xip {
            s.xip.push(x);
        }
        s.xcnt = xip.len() as u32;
        Rc::new(s)
    };
    let past = snap(11, &[]);
    let mut src = source(mcx, &[1], &past);
    let h0 = hinted_census();
    let ws = windows(&mut src);
    assert_eq!(ws.iter().find(|w| w.2 == 1).map(|w| w.0), Some(on1), "every hinted tuple of block 1 is visible to a later snapshot");
    assert_eq!(hinted_census() - h0, on1 as u64, "block 1's tuples settled by their hint bits, the other pages' by the seam");
    src.unbind();
    let running = snap(10, &[10]);
    let mut src = source(mcx, &[1], &running);
    let h0 = hinted_census();
    windows(&mut src);
    assert_eq!(hinted_census(), h0, "xmin 10 runs for this snapshot: the seam decides every tuple");
    src.unbind();
    quiesced();
}

/// I8 (row 8): a `LANE_LATE` request under a selection deforms the selected rows only (the rest read NULL); a repeated
/// request over the same selection keeps them; a plain request deforms every row; a row outside the window is refused.
#[test]
fn a_late_lane_is_deformed_for_the_survivors_only() {
    let _g = serial();
    install();
    reset_fixture();
    let mcx = mcx("m2-heap-late");
    let values: Vec<i32> = (0..300).collect();
    load_rows(mcx, &values);
    let snap = static_mvcc_snapshot();
    let mut src = source(mcx, &[1, 1], &snap);
    let n = src.nblocks() as u64;
    src.seek(&[0, n], &[0, 0]).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let mut site = blank_site(lanes.as_mut_ptr());
    let rows = src.window(&mut Window::over(&mut site)).unwrap() as usize;
    assert!(rows > 8);
    let sel = [1u32, 4, 7];
    let mut a = Lane::NONE;
    assert_eq!(src.lane(LANE_LATE as u16 | 1, &sel, &mut a).unwrap() as usize, rows);
    for r in 0..rows {
        let picked = sel.contains(&(r as u32));
        assert_eq!(unsafe { a.is_valid(r) }, picked, "row {r}");
        if picked {
            assert_eq!(unsafe { a.datum(r) } as u32 as i32, values[r]);
        }
    }
    let mut b = Lane::NONE;
    src.lane(LANE_LATE as u16, &sel, &mut b).unwrap();
    assert!(sel.iter().all(|&r| unsafe { b.is_valid(r as usize) && b.datum(r as usize) as u32 as i32 == values[r as usize] }), "the same selection: the rows stay deformed");
    let mut c = Lane::NONE;
    src.lane(1, &sel, &mut c).unwrap();
    assert!(c.all_valid(), "a plain request deforms every row");
    assert!((0..rows).all(|r| unsafe { c.datum(r) } as u32 as i32 == values[r]));
    let mut off = Lane::NONE;
    assert_eq!(src.lane(2, &[], &mut off).unwrap() as usize, rows, "the offset lane as it stands");
    assert_eq!(off.width, 2);
    src.seek(&[0, n], &[0, 0]).unwrap();
    let mut site = blank_site(lanes.as_mut_ptr());
    src.window(&mut Window::over(&mut site)).unwrap();
    assert!(src.lane(LANE_LATE as u16, &[rows as u32 + 5], &mut a).is_err(), "a selected row outside the window");
    src.unbind();
    quiesced();
}

/// V4R-371: an all-visible page whose line pointers are all normal is answered from them alone: offsets `1..=n` and no
/// tuple header read (a null bitmap or a short tuple on it is the program's to see, `PAGE_UNREAD`); a count-only walk
/// keeps the trivial flags.
#[test]
fn an_all_visible_page_of_normal_line_pointers_is_answered_unread() {
    let _g = serial();
    install();
    reset_fixture();
    let mcx = mcx("m2-heap-unread");
    load_rows(mcx, &(0..400).collect::<Vec<i32>>());
    let snap = static_mvcc_snapshot();
    with_page(0, |p| p.set_all_visible());
    let ls = lines(0);
    with_page(0, |p| unsafe {
        let base = p.as_mut_ptr();
        let m = base.add(ls[0].1 + 20) as *mut u16;
        m.write_unaligned(m.read_unaligned() | HEAP_HASNULL);
        let m2 = base.add(ls[1].1 + 18) as *mut u16;
        m2.write_unaligned(m2.read_unaligned() & !HEAP_NATTS_MASK);
    });
    let mut src = source(mcx, &[1], &snap);
    let got = offsets(&mut src, 1);
    assert_eq!(got[0], (PAGE_UNREAD | PAGE_ALL_VISIBLE | PAGE_IOTA, (1..=ls.len() as u16).collect::<Vec<u16>>()), "offsets 1..=n, headers unread");
    assert!(got[1..].iter().all(|w| w.0 == PAGE_NONULLS | PAGE_FULL | PAGE_IOTA), "a tested page is walked (every tuple visible)");
    src.unbind();
    let mut count = source(mcx, &[], &snap);
    assert_eq!(windows(&mut count)[0].1, PAGE_NONULLS | PAGE_FULL | PAGE_ALL_VISIBLE | PAGE_IOTA, "no attribute wanted: the flags hold trivially");
    count.unbind();
    quiesced();
}

/// A page mixing normal line pointers with redirect, dead and unused ones (HOT pruning's leftovers) takes the header walk:
/// the others are skipped (not `PAGE_IOTA`) and the flags summarise the visible tuples' headers (a null bitmap, then a
/// short tuple).
#[test]
fn an_all_visible_page_with_other_line_pointers_takes_the_header_walk() {
    let _g = serial();
    install();
    reset_fixture();
    let mcx = mcx("m2-heap-mixed");
    load_rows(mcx, &(0..400).collect::<Vec<i32>>());
    let snap = static_mvcc_snapshot();
    with_page(0, |p| p.set_all_visible());
    let ls = lines(0);
    with_page(0, |p| unsafe {
        let base = p.as_mut_ptr();
        lp_at(base, 2).write_unaligned(5 | (2 << 15));
        lp_at(base, 3).write_unaligned(lp_at(base, 3).read_unaligned() | (3 << 15));
        lp_at(base, 4).write_unaligned(0);
        let m = base.add(ls[5].1 + 20) as *mut u16;
        m.write_unaligned(m.read_unaligned() | HEAP_HASNULL);
    });
    let visible: Vec<u16> = (1..=ls.len() as u16).filter(|o| ![2, 3, 4].contains(o)).collect();
    let mut src = source(mcx, &[1], &snap);
    assert_eq!(offsets(&mut src, 1)[0], (PAGE_FULL | PAGE_ALL_VISIBLE, visible.clone()), "redirect / dead / unused skipped; a null bitmap seen");
    with_page(0, |p| unsafe {
        let m = p.as_mut_ptr().add(ls[6].1 + 18) as *mut u16;
        m.write_unaligned(m.read_unaligned() & !HEAP_NATTS_MASK);
    });
    assert_eq!(offsets(&mut src, 1)[0], (PAGE_ALL_VISIBLE, visible), "a short tuple seen");
    src.unbind();
    quiesced();
}
