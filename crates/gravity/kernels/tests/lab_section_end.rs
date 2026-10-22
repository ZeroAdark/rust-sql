//! The rig issues the section end of a `finishing` pipeline (`Pipeline.finish`, item 20): `par_sort`'s run
//! writer keeps its last buffer until `SECTION_END`, so a sorted result through plain `run_pipelines` is the proof.

use gravity_kernels_rig as common;

use common::*;
use gravity_abi::Datum;
use gravity_kernels::par_sort;
use gravity_program::hand::{self as programs, Oids};

const BLOCKS: u64 = 6;
const PER_BLOCK: u64 = 10;

fn value(r: u64) -> i64 {
    (r as i64 * 37) % 61 - 30
}

fn t_rows(keys: &[Datum], _: &[u8]) -> Vec<Row> {
    let (lo, hi) = (keys[0], keys[1].min(BLOCKS));
    (lo..hi).flat_map(|b| (0..PER_BLOCK).map(move |i| b * PER_BLOCK + i)).map(|r| (vec![value(r) as u64], vec![0u8])).collect()
}

#[test]
fn run_pipelines_issues_the_section_end_of_a_finishing_pipeline() {
    let program = programs::par_sort_with(Oids::T_A, 16, BLOCKS * PER_BLOCK, &[1]);
    assert!(program.pipelines[0].finish && !program.pipelines[1].finish);
    let mut rig = Rig::new(program, par_sort::params(1, false, false).to_vec());
    rig.site(0).rows_for = t_rows;
    let (rows, rets) = rig.run_pipelines();
    assert!(rets.iter().all(|r| matches!(r, Ret::Done | Ret::Suspended { .. })), "{rets:?}");
    assert_eq!(rig.host.section_ends, 1, "one section end: the finishing pipeline's, none for the emit");
    assert!(!rig.host.runs.is_empty(), "the section end put the writer's last buffer as a run");
    let mut want: Vec<Vec<Datum>> = (0..BLOCKS * PER_BLOCK).map(|r| vec![value(r) as u64]).collect();
    want.sort_by_key(|r| r[0] as i64);
    assert_eq!(rows, want);
}

#[test]
fn the_hooked_runner_issues_it_before_the_fold_and_the_hook() {
    let program = programs::par_sort_with(Oids::T_A, 16, BLOCKS * PER_BLOCK, &[1]);
    let mut rig = Rig::new(program, par_sort::params(1, false, false).to_vec());
    rig.site(0).rows_for = t_rows;
    let mut seen = Vec::new();
    let (rows, _) = rig.run_pipelines_hooked(&|_, _| None, &mut |rig, p| seen.push((p, rig.host.section_ends, rig.host.runs.len())));
    assert_eq!(seen, vec![(0, 1, 1)], "the hook runs after the section end's put");
    assert_eq!(rows.len(), (BLOCKS * PER_BLOCK) as usize);
}
