//! A breaker region with a ceiling whose breaker cannot spill past its grant (a debt home: a resident nested-loop inner
//! build) is no revoke target: its participant notes 0 spillable bytes on its statement's revoke handle, so a revoke never
//! asks it for bytes it cannot give back. A region whose breaker spills is noted on the leader (a serial claim) and on each
//! worker's share (a section's claims), once its table is laid out (D4314 (b) narrowed: a region whose kind byte names no
//! giving kind yet, `Other`, notes 0; the kind-7 build is noted from the claim after it laid its table out), by the bytes
//! its cut gives back (D4394): what it committed in place past its share, none while it sits at its share.
use gravity_driver_rig as common;

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use common::*;
use gravity_abi::breaker::{Breaker, BreakerKind, Budget, Spill};
use gravity_abi::MorselRange;
use gravity_driver::{Claimed, Entry, Grant, Step, SHARE_FLOOR};
use gravity_program::spill::{Holds, NoSpill, Proof};
use gravity_program::{Arena, Code, End, Layout, OutCol, Pipeline, Program, Reason, SiteDesc, WritePlan};

const MIB: u64 = 1 << 20;
const SPILLS: Holds = Holds::Spills(Spill::Partitioned { bits: 3 });
const DEBT: Holds = Holds::Bounded(Proof::Estimate(NoSpill::BuildNestInner));
/// The share is its 4 MiB ceiling: the region holds its share and cannot grow under the ceiling.
const AT: Grant = Grant::of(64 * MIB).with_ceilings(4 * MIB, 4 * MIB);
/// A 64 MiB hash floor over a 1000-row model charges the floor's first 8 MiB (d2894_floor_grow) inside a 32 MiB ceiling's
/// reservation: the region grows in place past its share, under its ceiling.
const ROOM: Grant = Grant::of(SHARE_FLOOR as u64).with_floors(64 * MIB, 64 * MIB).with_ceilings(32 * MIB, 32 * MIB);

/// The build's claim: its region laid out as a kind-7 build (the kind byte its lowered end writes at its open).
unsafe extern "C-unwind" fn k_lay(ctx: *mut gravity_abi::Ctx) -> i64 {
    let p = gravity_abi::program::partial(ctx);
    if !p.is_null() {
        *p.add(gravity_abi::breaker::TABLE_KIND_OFF) = gravity_abi::breaker::TABLE_V4BUILD;
    }
    k_noop(ctx)
}

/// The bytes `k_lay_grow`'s grows committed in place, and the grows that moved their region instead.
static GROWN: AtomicU64 = AtomicU64::new(0);
static MOVED: AtomicU64 = AtomicU64::new(0);

/// `k_lay`, then on its participant's first claim one grow (`region_grow`) to twice its armed length: what the armed limit
/// (offset 8) moved by is summed in `GROWN`, a grow that moved the region counted in `MOVED`.
unsafe extern "C-unwind" fn k_lay_grow(ctx: *mut gravity_abi::Ctx) -> i64 {
    let p = gravity_abi::program::partial(ctx);
    if p.is_null() {
        return k_noop(ctx);
    }
    *p.add(gravity_abi::breaker::TABLE_KIND_OFF) = gravity_abi::breaker::TABLE_V4BUILD;
    let h = grow_hdr(p);
    if h.count == 0 {
        let was = h.limit as u64;
        let b = gravity_abi::program::region_grow(ctx, was, h.head, h.tail) as usize as *mut u8;
        if b.is_null() {
            return gravity_abi::status::ERROR;
        }
        if b != p {
            MOVED.fetch_add(1, Ordering::Relaxed);
        }
        let h = grow_hdr(b);
        GROWN.fetch_add(h.limit as u64 - was, Ordering::Relaxed);
        h.count = 1;
    }
    k_noop(ctx)
}

#[derive(Clone, Copy, PartialEq)]
enum Lay {
    No,
    Laid,
    Grown,
}

/// A lowered program: a share-budgeted hash build of slot 0 that `holds` past its grant (its claims lay the region out as a
/// kind-7 build unless `Lay::No`, and grow it once when `Lay::Grown`), then the emit that reads it.
fn program(holds: Holds, lay: Lay) -> Program {
    let st = Arena::new(1 << 14);
    let sites = [SiteDesc::range_source(st.handle(), 2001, &[1, 2])];
    let b = End::Breaker(Breaker { kind: BreakerKind::HashBuild, slot: 0, bytes: 0, budget: Budget::Share, spill: Spill::Partitioned { bits: 3 } });
    let pipes = [
        (Pipeline { holds: Some(holds), ..Pipeline::eligible(0, b, 1000) }, Code::hand(match lay { Lay::No => k_noop, Lay::Laid => k_lay, Lay::Grown => k_lay_grow })),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming_one(0), Code::hand(k_noop)),
    ];
    Program::with_pipelines(Layout { nsites: 1, out_natts: 1, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 }, &sites, &[OutCol::new("c0", 20, -1, 8, true)], WritePlan::SELECT, pipes).with_used(1)
}

fn spillable() -> u64 {
    mcx::governor::current().map_or(0, |s| s.revoke().spillable())
}

/// The spillable bytes the producer's participants note on their statement's revoke handle: the leader's serial claims
/// (`width` 0, read before its claims end), else each worker's claims of a section of `width`, read before the section
/// closes. Two claims per participant: the second one's start notes the region its first laid out.
fn noted(holds: Holds, lay: Lay, width: u16, grant: Grant) -> u64 {
    mcx::governor::statement_start(0, true);
    let program = Arc::new(program(holds, lay));
    let mut derive = FakeDerive::ranges(RangeTable::new(4, 4));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[0], &[0], &mut sink, &flag);
    let mut exec = entry.start(None, &program, &mut derive, &mut b, lease(grant)).unwrap();
    let noted = if width == 0 {
        for r in [MorselRange::new(0, 2), MorselRange::new(2, 4)] {
            entry.run_claim(&mut exec, &mut b, r).unwrap();
        }
        let n = spillable();
        entry.end_claims(&mut exec).unwrap();
        n
    } else {
        let open = entry.open_section(&mut exec, &b, &mut derive, 0, width).unwrap();
        for o in 0..width {
            for k in 0..2u64 {
                let at = 2 * o as u64 + k;
                assert_eq!(open.run_claim(o, MorselRange::new(at, at + 1)).unwrap(), Claimed::Done);
            }
        }
        let n = spillable();
        entry.close_section(open, &mut b, &NoMerge, true).unwrap();
        n
    };
    entry.end(exec, false);
    mcx::governor::statement_end();
    noted
}

/// One test: the revoke switch and the statement are the process's.
#[test]
fn a_debt_home_notes_no_spillable_bytes_and_a_spilling_home_does() {
    mcx::governor::register(47, 0);
    mcx::revoke::set_enabled(true);
    for width in [0u16, 2] {
        // D4394: a spilling kind-7 build is noted by what its cut gives back, its committed growth past its share
        GROWN.store(0, Ordering::Relaxed);
        MOVED.store(0, Ordering::Relaxed);
        let n = noted(SPILLS, Lay::Grown, width, ROOM);
        let (grown, moved) = (GROWN.load(Ordering::Relaxed), MOVED.load(Ordering::Relaxed));
        assert!(grown > 0 && moved == 0, "W{width}: the build grew in place ({grown} bytes, {moved} moves)");
        assert_eq!(n, grown, "W{width}: a spilling build that grew in place notes exactly its committed growth");
        assert_eq!(noted(SPILLS, Lay::Laid, width, ROOM), 0, "W{width}: the same build at its share offers nothing to give back");
        assert_eq!(noted(SPILLS, Lay::Laid, width, AT), 0, "W{width}: a build whose share is its ceiling offers nothing");
        assert_eq!(noted(DEBT, Lay::Laid, width, AT), 0, "W{width}: a resident build's region (a ceiling, no spill road) is none");
        // D4314 (b) narrowed: a region whose table was never laid out (its kind byte names no giving kind) offers nothing
        assert_eq!(noted(SPILLS, Lay::No, width, AT), 0, "W{width}: a region with no table laid out (Other) is no revoke target");
    }
}
