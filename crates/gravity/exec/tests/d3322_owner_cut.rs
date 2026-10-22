//! D3322: a leader region opened smaller than the grant's share holds every owner's cut. The crash: a 15780 MiB grant at W8
//! put a hash aggregate's leader share at the u32 cap (its region 2^32 B), the full pool refused it, `share_at_room` (D3226
//! (c1)) opened the region at the pool's room, and the section cut its W owners at the grant's split (0x1FFFFFF0 each) from
//! that smaller region: the owners folded past its end (SIGSEGV, two faults one cut apart). Here the allocator refuses the
//! leader's region (this test binary's allocator fails any allocation of `CAP` bytes or more while armed), so the leader
//! takes the same road (`past_g`, then `share_at_room` at the pool's headroom) while the workers' smaller shares bind from G.
#[path = "../../parallel/tests/common/mod.rs"]
mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use gravity_abi::breaker::ORDER_NONE;
use gravity_driver::{footprint, Entry, Grant, OWNER_CUT_HELD, POOL};
use gravity_kernels::table::params as par_hashagg;
use gravity_kernels::table::params::DEFAULT_PARAMS;
use gravity_parallel::{FixturePool, HashAggPartial};
use gravity_program::{hand as programs, Code, End, OutCol, OwnerDesc, Pipeline, Program, Reason, SiteDesc, WritePlan};

/// While non-zero, an allocation of this many bytes or more is refused (null), as an allocator out of address space refuses.
static CAP: AtomicUsize = AtomicUsize::new(0);

struct Capped;

unsafe impl GlobalAlloc for Capped {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let cap = CAP.load(Ordering::Relaxed);
        if cap != 0 && l.size() >= cap {
            return core::ptr::null_mut();
        }
        unsafe { System.alloc(l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let cap = CAP.load(Ordering::Relaxed);
        if cap != 0 && l.size() >= cap {
            return core::ptr::null_mut();
        }
        unsafe { System.alloc_zeroed(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
}

#[global_allocator]
static GLOBAL: Capped = Capped;

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

fn gen_20k(r: u64) -> Option<[i64; 2]> {
    Some([(r * 7919 % 20_000) as i64, (r % 5) as i64])
}

#[test]
fn a_leader_region_the_allocator_refused_opens_at_the_room_and_holds_every_owner_cut() {
    const MIB: u64 = 1 << 20;
    let table = FixtureTable::new(T_KV, 80, 500, gen_20k);
    let mut derive = FixtureDerive::new(&[&table]);
    let pool = FixturePool::new(2);
    let mut entry = Entry::new(2);
    let mut groups: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
    for [k, v] in table.rows() {
        let g = groups.entry(k).or_insert((0, 0));
        g.0 += v;
        g.1 += 1;
    }
    let mut expect: Vec<Vec<u64>> = groups.iter().map(|(k, (s, c))| vec![*k as u64, *s as u64, *c as u64]).collect();
    expect.sort();
    let d = OwnerDesc { groups: groups.len() as u32, ..OwnerDesc::generic(par_hashagg::LAYOUT, ORDER_NONE, 0) };
    let program = Arc::new(declared(40_000, d));
    let params = DEFAULT_PARAMS;
    // W2 over a 256 MiB grant: the leader's share 256 MiB, each worker's 128 MiB, the grant's owner cut ~128 MiB each
    let (grant, w) = (Grant::of(256 * MIB), 2u16);
    let split = grant.split(1, w);
    assert_eq!(split.breaker.bytes as u64, 256 * MIB);
    assert!(split.worker.bytes as u64 <= 128 * MIB && split.owner.bytes as u64 > 100 * MIB);
    // the pool: the statement's footprint (G covers the leader and the workers) and 32 MiB of headroom past it
    let need = footprint(grant, &program, w);
    POOL.arm(need + 32 * MIB);
    let a = Rc::new(POOL.admit_need(grant, &|g| footprint(g, &program, w), Duration::from_millis(10)).unwrap());
    let (held0, opened0) = (OWNER_CUT_HELD.load(Ordering::Relaxed), gravity_driver::mem::region::OPENED_SMALLER.load(Ordering::Relaxed));
    let mut sink = LegSink::new(true);
    // the leader's 256 MiB region refused by the allocator; the workers' 128 MiB ones are not
    CAP.store(200 * MIB as usize, Ordering::Relaxed);
    let plan = Keyed { inner: FixturePlan::new(&program, &derive, w, 7), merge: HashAggPartial::new(0, Some(gravity_exec::table::owner::bind(&d, &mut params.clone(), 3, None))) };
    let rows = run_planned_in(&mut entry, &program, &mut derive, &pool, &plan, a.clone(), &params, &NO_ABORT, &mut sink);
    CAP.store(0, Ordering::Relaxed);
    let rows = rows.unwrap_or_else(|e| panic!("the statement: {e:?}"));
    assert_eq!(rows, expect.len() as u64);
    assert_eq!(sink.sorted(), expect);
    assert!(gravity_driver::mem::region::OPENED_SMALLER.load(Ordering::Relaxed) > opened0, "the leader region opened at the pool's room");
    assert!(OWNER_CUT_HELD.load(Ordering::Relaxed) > held0, "the owners cut from the region the leader holds, not the grant's split");
    drop(a);
    drop(entry);
    POOL.arm(0);
}
