//! D1340: a table lane past the window's data lanes. The driver's window publishes only a site's data lanes (`nlanes`), and
//! the source fills a table lane (a dictionary / fingerprint table) on demand past them, inside the lane array the node bind
//! sized by the descriptor (`lanes_cap`). host_abi's table reads go through `abi::site::table_lane`, bounded by `lanes_cap`;
//! through `lane` (bounded by `nlanes`) a debug build asserted "lane 4 of 3" on pgrcolumnar's hashed text keys.
use gravity_abi::{Ctx, Lane, Site};
use gravity_kernels::host_abi::agg_table_ncodes;

/// D1340: the driver's window publishes only a site's data lanes (`nlanes`: 3 here, `g`, `t`, `v`), and the source fills a
/// table lane (lane 4: natts + 1 + table 0, `t`'s Fp128 fingerprints) on demand, past them but inside the descriptor's lane
/// array (`lanes_cap` 5). A kernel reads it through `table_lane`: the parent's `lane` asserted "lane 4 of 3" in a debug
/// build (pgrcolumnar's hashed text keys), which `guard!` turned into a statement error.
#[test]
fn a_table_lane_past_the_windows_data_lanes_reads() {
    let hdr = gravity_dict::table::Header {
        elem: gravity_dict::lane::ELEM_FP128,
        flags: 0,
        pad: [0; 2],
        ncodes: 11,
        faults: core::ptr::null(),
        data: core::ptr::null(),
        base: core::ptr::null(),
        canon: core::ptr::null(),
        hashes: core::ptr::null(),
        ndistinct: 11,
        nfaults: 0,
    };
    let mut lanes = [Lane::NONE; 5];
    // SAFETY: `hdr` is a table header that outlives the lane.
    unsafe { gravity_dict::lane::publish(&hdr, 0, 1, &mut lanes[4]) };
    // SAFETY: every field of `Site` and `Ctx` is a pointer, an integer or `Option<fn>`, all valid zeroed.
    let mut site: Site = unsafe { core::mem::zeroed() };
    site.lanes = lanes.as_mut_ptr();
    site.nlanes = 3;
    // SAFETY: `lanes_cap` (u16) sits at byte 174 of `Site` (abi tests/golden/ctx-layout.txt); written by offset so the
    // parent, whose field there is padding, compiles for the proof (closed.py prove) and fails on the assert
    unsafe { ((&mut site as *mut Site as *mut u8).add(174) as *mut u16).write_unaligned(5) };
    let mut ctx: Box<Ctx> = Box::new(unsafe { core::mem::zeroed() });
    ctx.sites = &mut site;
    ctx.nsites = 1;
    // SAFETY: the ctx's one site is bound above; lane 4 is a filled table.
    assert_eq!(unsafe { agg_table_ncodes((&mut *ctx) as *mut _ as u64, 0, 4) }, 11, "the table's code count, no assert");
    assert_eq!(site.nlanes, 3);
}
