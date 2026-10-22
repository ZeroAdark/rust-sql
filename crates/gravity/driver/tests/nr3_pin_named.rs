//! NR-3: a kernel pin the grant pool refused, in a session with no temp-file road (`map_temp` maps nothing: a refused
//! mmap, or wasm), on a pipeline whose home names a SPILL-DEBT row (a resident build's: `KeyCopies` here) ends its
//! statement in the same out of memory as before (53200, `Entry`'s pool_refused, D2845), now naming the home's row, so
//! exec writes its line in the server log only. A home that spills names none. The kernel's own raise on the null:
//! nr3_pin_raised.rs. Under a pool armed at the admission's need
//! and no more; no spill factory is installed, so the temp-file road maps nothing. One test: it arms the process-wide pool.
use gravity_driver_rig as common;

use std::rc::Rc;
use std::sync::atomic::{AtomicU8, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use gravity_abi::breaker::{Breaker, BreakerKind, Budget, Spill};
use gravity_abi::status::{unsupported, UNSUPPORTED_SCRATCH};
use gravity_abi::{Ctx, MorselRange};
use gravity_driver::{footprint, Call, Entry, Error, Grant, Merge, Partial, Result, State, DEPOT, POOL, REGIONS};
use gravity_program::spill::{Holds, NoSpill, Proof};
use gravity_program::{Arena, Code, End, Layout, OutCol, Pipeline, Program, Reason, SiteDesc, WritePlan};

const MIB: u64 = 1 << 20;
/// Past G and past the pool's room: every pin below is refused.
const GROW: u64 = 64 * MIB;
const SPILLS: Holds = Holds::Spills(Spill::Partitioned { bits: 3 });
const DEBT: Holds = Holds::Bounded(Proof::Estimate(NoSpill::KeyCopies));

/// The statement's abort byte (never set here).
static ABORT: AtomicU8 = AtomicU8::new(0);

/// A kernel pin past the pool (`Program.pin`): the scratch refusal (kind 2) on its null.
unsafe extern "C-unwind" fn k_pin(ctx: *mut Ctx) -> i64 {
    // SAFETY: the driver binds `ctx` for the call
    if unsafe { gravity_abi::program::pin(ctx, GROW) }.is_null() { unsupported(UNSUPPORTED_SCRATCH) } else { gravity_abi::status::DONE }
}

/// No merge runs at W0.
struct NoMerge;

impl Merge for NoMerge {
    fn merge_into(&self, _leader: &mut State<'_>, _partial: Partial<'_>) -> Result<()> {
        Ok(())
    }
}

/// A lowered program: a share-budgeted hash build of slot 0 (its kernel `k`) that `holds` past its grant, then the emit.
fn program(holds: Holds, k: unsafe extern "C-unwind" fn(*mut Ctx) -> i64) -> Program {
    let st = Arena::new(1 << 14);
    let sites = [SiteDesc::range_source(st.handle(), 2001, &[1, 2])];
    let b = End::Breaker(Breaker { kind: BreakerKind::HashBuild, slot: 0, bytes: 0, budget: Budget::Share, spill: Spill::Partitioned { bits: 3 } });
    let pipes = [
        (Pipeline { holds: Some(holds), ..Pipeline::eligible(0, b, 1000) }, Code::hand(k)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming_one(0), Code::hand(k_noop)),
    ];
    Program::with_pipelines(Layout { nsites: 1, out_natts: 1, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 }, &sites, &[OutCol::new("c0", 20, -1, 8, true)], WritePlan::SELECT, pipes).with_used(1)
}

/// The producer of `program` run by the leader (`width` 0) or by a section of `width` closed with `merge`, admitted under a
/// pool armed at its need and no more (`held_g`: G's room held whole too, so the slot's open is refused): its error.
fn refused(program: Program, width: u16, merge: &dyn Merge, held_g: bool) -> Option<Error> {
    let program = Arc::new(program);
    let w = width.max(1);
    POOL.gate(0);
    REGIONS.trim();
    DEPOT.trim();
    let grant = Grant::of(MIB);
    POOL.arm(footprint(grant, &program, w));
    let res = Rc::new(POOL.admit_need(grant, &|g| footprint(g, &program, w), Duration::from_millis(10)).expect("admitted"));
    let mut derive = FakeDerive::ranges(RangeTable::new(4, 4));
    let mut entry = Entry::new(2);
    let mut sink = LegSink::new();
    let mut b = binding(&[0], &[0], &mut sink, &ABORT);
    let mut exec = entry.start(None, &program, &mut derive, &mut b, res.clone()).expect("started");
    let carved = held_g.then(|| res.hold(res.left()).expect("G's room held"));
    let r = if width == 0 {
        entry.run_pipeline(&mut exec, &mut b, Call::batch(64)).map(|_| ())
    } else {
        let open = entry.open_section(&mut exec, &b, &mut derive, 0, width).expect("the section opens");
        let claims = (0..width).try_for_each(|o| open.run_claim(o, MorselRange::new(2 * o as u64, 2 * o as u64 + 2)).map(|_| ()));
        let closed = entry.close_section(open, &mut b, merge, claims.is_ok());
        claims.and(closed)
    };
    ABORT.store(0, Relaxed);
    entry.end(exec, r.is_ok());
    drop((carved, b, res, entry));
    REGIONS.trim();
    DEPOT.trim();
    assert_eq!(POOL.held(), 0, "every byte back at the statement's end");
    POOL.arm(0);
    r.err()
}

fn why(e: &Option<Error>) -> Option<NoSpill> {
    match e {
        Some(Error::OutOfMemory { why, .. }) => *why,
        other => panic!("a refused grow or open is an out of memory: {other:?}"),
    }
}

#[test]
fn a_kernel_pin_refused_on_every_road_names_its_debt_home() {
    let none = NoMerge;
    assert_eq!(why(&refused(program(DEBT, k_pin), 0, &none, false)), Some(NoSpill::KeyCopies), "W0: a resident home's refused pin names its row");
    assert_eq!(why(&refused(program(SPILLS, k_pin), 0, &none, false)), None, "W0: a spilling home's refused pin: a plain out of memory");
}
