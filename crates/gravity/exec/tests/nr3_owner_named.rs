//! NR-3: an owner phase's region the grant pool refuses, in a session whose temp-file road maps nothing (`map_temp`
//! `Ok(None)`: a refused mmap, or wasm), ends its statement in the same out of memory (53200) as before, now naming the
//! road's SPILL-DEBT row (`AggOwnerAll`, the road `pin_owner` counts), so exec writes the row's line in the server log only.
//! The D440 shape (table_owner_leader.rs): an estimate a hundredth of the groups, so the one owner outgrows its bounded
//! region and pins past the grant into a pool with no headroom. One test: it installs the process's spill factory.
#[path = "../../parallel/tests/common/mod.rs"]
mod common;

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use gravity_abi::breaker::ORDER_NONE;
use gravity_driver::{footprint, Entry, Error, Grant, Reservation, SpillFactory, SpillSet, TempMap, POOL, REGIONS};
use gravity_kernels::table::params as par_hashagg;
use gravity_kernels::table::params::DEFAULT_PARAMS;
use gravity_parallel::{FixturePool, HashAggPartial};
use gravity_program::spill::NoSpill;
use gravity_program::{hand as programs, Code, End, OutCol, OwnerDesc, Pipeline, Program, Reason, SiteDesc, WritePlan};

/// Spill sets in memory, and a temp-file road that maps nothing.
struct NoFile;

impl SpillFactory for NoFile {
    fn create(&self) -> gravity_driver::Result<Box<dyn SpillSet>> {
        MEM_SPILL.create()
    }

    fn map_temp(&self, _bytes: usize) -> gravity_driver::Result<Option<TempMap>> {
        Ok(None)
    }
}

static NO_FILE: NoFile = NoFile;

struct Keyed {
    inner: FixturePlan,
    merge: HashAggPartial,
}

impl gravity_parallel::Plan for Keyed {
    fn width_of(&self, p: usize) -> u16 {
        self.inner.width_of(p)
    }
    fn claims_for(&self, p: usize) -> &dyn gravity_parallel::MorselSource {
        self.inner.claims_for(p)
    }
    fn merge_for(&self, _p: usize) -> &dyn gravity_driver::Merge {
        &self.merge
    }
}

fn declared(rows: u64, d: OwnerDesc) -> Program {
    let st = gravity_program::Arena::new(1 << 12);
    let sites = [SiteDesc::range_source(st.handle(), T_KV, &[1, 2])];
    let out = [OutCol::new("k", programs::INT4, -1, 4, true).origin(T_KV, 1), OutCol::new("sum", programs::INT8, -1, 8, true), OutCol::new("count", programs::INT8, -1, 8, true)];
    let pipes = [
        (Pipeline::eligible(0, End::Breaker(par_hashagg::breaker(0)), rows).owner(d), Code::hand(par_hashagg::par_hashagg)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(par_hashagg::par_hashagg_emit)),
    ];
    Program::with_pipelines(gravity_program::Layout { nsites: 1, out_natts: 3, out_rows: 16, scratch_bytes: 0, exec_bytes: 0 }, &sites, &out, WritePlan::SELECT, pipes)
}

fn gen_wide(r: u64) -> Option<[i64; 2]> {
    Some([r as i64, 1])
}

/// One statement admitted at the program's footprint over a pool of exactly that many bytes (no headroom).
fn admitted(program: &Arc<Program>, grant: Grant, w: u16, run: impl FnOnce(Rc<Reservation>) -> gravity_driver::Result<u64>) -> gravity_driver::Result<u64> {
    let need = footprint(grant, program, w);
    POOL.arm(need);
    let a = Rc::new(POOL.admit_need(grant, &|g| footprint(g, program, w), Duration::from_secs(5)).unwrap());
    let r = run(a);
    POOL.arm(0);
    r
}

#[test]
fn an_owner_region_refused_on_every_road_names_its_row() {
    assert!(gravity_driver::install_spill_factory(&NO_FILE), "this binary owns the process's spill factory");
    let table = FixtureTable::new(T_KV, 2000, 100, gen_wide);
    let mut derive = FixtureDerive::new(&[&table]);
    let pool = FixturePool::new(4);
    let mut entry = Entry::new(2);
    let d = OwnerDesc { groups: 2000, ..OwnerDesc::generic(par_hashagg::LAYOUT, ORDER_NONE, 0) };
    let program = Arc::new(declared(2_000, d));
    let params = DEFAULT_PARAMS;
    let mut wrong = Vec::new();
    for w in [4u16, 1] {
        REGIONS.clear();
        table.fresh();
        let mut sink = LegSink::new(true);
        let r = admitted(&program, Grant::of(1 << 20), w, |lease| {
            let plan = Keyed { inner: FixturePlan::new(&program, &derive, w, 33), merge: HashAggPartial::new(0, Some(gravity_exec::table::owner::bind(&d, &mut params.clone(), 3, None))) };
            run_planned_in(&mut entry, &program, &mut derive, &pool, &plan, lease, &params, &NO_ABORT, &mut sink)
        });
        match r {
            Err(Error::OutOfMemory { why: Some(NoSpill::AggOwnerAll), .. }) => {}
            other => wrong.push(format!("W{w}: {other:?}")),
        }
    }
    REGIONS.clear();
    assert!(wrong.is_empty(), "an owner region the pool and the temp-file road refuse is the out of memory naming AggOwnerAll:\n{}", wrong.join("\n"));
}
