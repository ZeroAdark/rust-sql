//! `SPILL_RUN_BASE` (lower.md §15.20, rung 11 need 7): a run fill lands where the kernel declared, not implicitly at
//! `Partials.own` — a pipeline that merges runs AND produces a breaker keeps its table. The rig's host, as the driver's.

use gravity_kernels_rig as common;

use common::*;
use gravity_abi::program::{partial, SPILL_RUNS, SPILL_RUN_BASE, SPILL_RUN_CLOSE, SPILL_RUN_FILL, SPILL_RUN_OPEN};
use gravity_abi::Datum;
use gravity_kernels::par_sort;
use gravity_kernels::util::spill;
use gravity_program::hand::{self as programs, Oids};

fn t_rows(keys: &[Datum], _: &[u8]) -> Vec<Row> {
    (keys[0]..keys[1].min(4)).flat_map(|b| (0..10).map(move |i| b * 10 + i)).map(|r| (vec![(r * 37) % 61], vec![0u8])).collect()
}

#[test]
fn a_fill_lands_at_the_declared_base_and_at_own_without_one() {
    let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 16, 40, &[1]), par_sort::params(1, false, false).to_vec());
    rig.site(0).rows_for = t_rows;
    let (rows, _) = rig.run_pipelines();
    assert_eq!(rows.len(), 40);
    let run = rig.host.runs[0].clone();
    assert!(run.len() >= 64);
    let ctx = rig.ctx();
    unsafe {
        let own = partial(ctx);
        let before = core::slice::from_raw_parts(own, 64).to_vec();
        let mut buf = vec![0u64; 16];
        let fill = |h: i64, off: u64, cap: u64| spill(ctx, SPILL_RUN_FILL, (h as u64 | off << 16 | cap << 48) as i64);
        assert_eq!(spill(ctx, SPILL_RUNS, 1), rig.host.runs.len() as i64);
        // the stream word rides in the open's argument, not in the handle
        let h = spill(ctx, SPILL_RUN_OPEN, (1i64 << 32) | 0);
        assert_eq!(spill(ctx, SPILL_RUN_BASE, buf.as_mut_ptr() as i64), 0);
        assert_eq!(fill(h, 8, 48), 48);
        let got = core::slice::from_raw_parts((buf.as_ptr() as *const u8).add(8), 48);
        assert_eq!(got, &run[..48], "the fill's offset is relative to the declared base");
        assert_eq!(core::slice::from_raw_parts(own, 64), &before[..], "own untouched under a declared base");
        assert_eq!(spill(ctx, SPILL_RUN_CLOSE, h), 0);
        // back to the default: the fill lands in own, as every run merge before this op
        assert_eq!(spill(ctx, SPILL_RUN_BASE, 0), 0);
        let h = spill(ctx, SPILL_RUN_OPEN, 0);
        assert_eq!(fill(h, 16, 32), 32);
        assert_eq!(core::slice::from_raw_parts(own.add(16), 32), &run[..32]);
    }
}
