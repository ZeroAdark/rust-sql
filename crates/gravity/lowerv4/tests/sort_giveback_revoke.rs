//! Sort give-back (D4291 + D4265's sort): a revoked finished sort gives its pages back, over PostgreSQL 18.6's plan of
//! sum(i), count(*) over generate_series(1, 1000000) ORDER BY i % 1000 OFFSET 500500 (D4291's fixture: the offset cuts
//! key 500's ties in half, so the sum pins the order the reader takes the rows in, ties included). The statement is
//! admitted with 8 MiB (G) and a 64 MiB sort ceiling over a plentiful pool, so its sort grows its region in place.
//! (1) A growing sort: the grant's revoke lever asks a revoke at the sort's second room ask; the kernel's next poll cuts
//! its region (the server's poll, exec glue `poll_kernel`, as `revoke_poll` here), it stops growing and spills from then
//! on; its pages leave at its section's end, not inside it (D4341), so only its answer is checked. (2) A finished sort: a
//! revoke asked once the sort's pipeline ended (its run sealed in memory, its reader not begun) puts the run at the next
//! claim and gives the region's growth back, counted as the revoke's (`revoke_counts`); the answer is the plentiful
//! run's. Before: nothing given back (the sealed run kept its region until the statement ended).

use gravity_lowerv4_it as common;

use std::rc::Rc;
use std::sync::atomic::{AtomicI64, AtomicU8, Ordering};
use std::time::Duration;

use common::multiset::Outcome;
use common::Mode;
use gravity_abi::breaker::BreakerKind;
use gravity_driver::{Grant, Result, POOL, REGIONS};

const FIXTURE: &str = include_str!("fixtures/d4291/ownspill-001.plan");
const MIB: u64 = 1 << 20;

/// The statement's abort byte, registered as its backend's revoke byte (the server's is the backend's abort byte).
static BYTE: AtomicU8 = AtomicU8::new(0);

/// The kernels' poll as the server's (exec glue `poll_kernel`): a set revoke bit (no abort pending) is cleared and the
/// participant's region cut (`revoke_poll`); the abort bit stops.
unsafe fn poll(ctx: *mut gravity_abi::Ctx) -> bool {
    use gravity_abi::ctx::{ABORT_BIT, REVOKE_BIT};
    use gravity_abi::PtrWord;
    let byte = &*((*ctx).cancel.ptr() as *const AtomicU8);
    let v = byte.load(Ordering::Acquire);
    if v & REVOKE_BIT != 0 && v & ABORT_BIT == 0 {
        byte.fetch_and(!REVOKE_BIT, Ordering::AcqRel);
        gravity_driver::mem::grow::revoke_poll(ctx);
    }
    gravity_abi::ctx::abort_set(byte)
}

static FILES_LIVE: AtomicI64 = AtomicI64::new(0);

struct VecFile(Vec<Vec<u8>>, u64);

impl Drop for VecFile {
    fn drop(&mut self) {
        FILES_LIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

impl gravity_driver::SpillFile for VecFile {
    fn write_epoch(&mut self, parts: &[(u32, &[u8])]) -> Result<()> {
        for (part, bytes) in parts {
            self.0[*part as usize].extend_from_slice(bytes);
            self.1 += bytes.len() as u64;
        }
        Ok(())
    }

    fn read_part(&self, part: u32, sink: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
        sink(&self.0[part as usize])
    }

    fn open_part(&self, part: u32) -> Result<Option<Box<dyn gravity_driver::RunReader>>> {
        let bytes = self.0[part as usize].clone();
        Ok((!bytes.is_empty()).then(|| Box::new(VecReader(bytes, 0)) as Box<dyn gravity_driver::RunReader>))
    }

    fn spilled_bytes(&self) -> u64 {
        self.1
    }
}

struct VecReader(Vec<u8>, usize);

impl gravity_driver::RunReader for VecReader {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let n = buf.len().min(self.0.len() - self.1);
        buf[..n].copy_from_slice(&self.0[self.1..self.1 + n]);
        self.1 += n;
        Ok(n)
    }
}

struct VecSet;

impl gravity_driver::SpillSet for VecSet {
    fn open(&mut self, _purpose: &str, _generation: u64, _worker: usize, nparts: u32) -> Result<Box<dyn gravity_driver::SpillFile>> {
        FILES_LIVE.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(VecFile((0..nparts).map(|_| Vec::new()).collect(), 0)))
    }
}

struct VecFactory;

impl gravity_driver::SpillFactory for VecFactory {
    fn create(&self) -> Result<Box<dyn gravity_driver::SpillSet>> {
        Ok(Box::new(VecSet))
    }
}

static FACTORY: VecFactory = VecFactory;

/// The statement's grant: G 8 MiB, the sort's ceiling 64 MiB (its region a floor reservation it grows in place past G).
fn grant() -> Grant {
    Grant { sort_floor: 4 * MIB, sort_ceil: 64 * MIB, ..Grant::of(8 * MIB) }
}

/// The revoke's bytes given back so far, this process.
fn given() -> u64 {
    gravity_driver::mem::grow::revoke_counts()[2]
}

/// The fixture run under `grant` admitted over a plentiful pool, as a governed statement; `finished`: a revoke is asked
/// once the sort's pipeline ended. Its answer and the revoke's bytes given back.
fn run(grant: Grant, mode: Mode, finished: bool) -> (Outcome, u64) {
    POOL.gate(0);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    POOL.arm(1 << 30);
    let res = Rc::new(POOL.admit_need_until(grant, &|g| g.bytes, Duration::from_secs(5), &|| false).unwrap_or_else(|e| panic!("admitted: {e:?}")));
    common::LEASE.with(|l| *l.borrow_mut() = Some(res.clone()));
    common::BETWEEN.with(|h| {
        *h.borrow_mut() = finished.then(|| {
            Box::new(move |p: usize, exec: &gravity_driver::Exec, _abort: &AtomicU8| {
                if exec.node().program().pipelines[p].breaker().is_some_and(|b| b.kind == BreakerKind::Sort) {
                    mcx::revoke::request_self();
                }
            }) as Box<dyn FnMut(usize, &gravity_driver::Exec, &AtomicU8)>
        })
    });
    mcx::governor::statement_start(0, true);
    let before = given();
    let out = common::run_fixture(FIXTURE, mode).expect("decomposes").unwrap_or_else(|e| panic!("the rig: {e}"));
    let gave = given() - before;
    mcx::governor::statement_end();
    common::BETWEEN.with(|h| *h.borrow_mut() = None);
    common::LEASE.with(|l| *l.borrow_mut() = None);
    drop(res);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    gravity_driver::mem::reap::drain();
    assert_eq!(POOL.held(), 0, "every byte back at the statement's end");
    assert_eq!(FILES_LIVE.load(Ordering::Relaxed), 0, "every temp file dropped with its statement");
    POOL.arm(0);
    (out, gave)
}

/// One test: the pool, the spill factory and the governor slot are the process's.
#[test]
fn a_revoked_sort_gives_its_pages_back() {
    gravity_driver::install_spill_factory(&FACTORY);
    mcx::governor::register(53, 0);
    mcx::revoke::register_byte(&BYTE);
    gravity_abi::ctx::set_poll(poll);
    common::ABORT.with(|a| a.set(Some(&BYTE)));
    let (plenty, _) = run(grant(), Mode::Straight, false);
    let Outcome::Rows(want) = &plenty else { panic!("the plentiful run: {plenty:?}") };
    assert_eq!(want.len(), 1, "one row: {want:?}");

    // (1) a growing sort, revoked at its second room ask: cut, it spills from then on (its pages leave at its section's end)
    let (grown, _) = run(grant().with_revoke_every(2), Mode::Straight, false);
    assert_eq!(&grown, &plenty, "a revoked growing sort's answer, read back from its runs");

    // (2) a finished sort, revoked before its reader began: its run goes to its file and its growth back
    let (finished, gave) = run(grant(), Mode::Straight, true);
    assert_eq!(&finished, &plenty, "a revoked finished sort's answer, read back from its file");
    assert!(gave >= 4 * MIB, "a revoked finished sort gave {gave} bytes back (before: 0, its sealed run kept its region)");
}
