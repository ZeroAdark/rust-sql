//! D4291 (D4287's fix): a pipeline open whose draw past W and G the grant pool refuses puts its statement's finished sort
//! to its temp file first, gives the sort's growth back, and opens again. The fixture is PostgreSQL 18.6's plan of
//! sum(i), count(*) over generate_series(1, 1000000) ORDER BY i % 1000 OFFSET 500500: an Aggregate over a Limit over a
//! Sort whose keys tie a thousand ways. The offset cuts key 500's thousand ties in half, so the sum pins the order the
//! reader takes the run in, ties included. The statement is admitted with 8 MiB (G) and a 64 MiB sort ceiling, so the
//! sort's region is a floor reservation it grows in place past G, as under work_mem = 'auto'. When the sort's pipeline
//! ends, G is held whole and the pool's room kept: the d4168 edge, the sort grown past G and sealed in memory while its
//! reader has not begun. Before: the aggregate's open was refused, out of memory (53200). Now its answer is the plentiful
//! run's, read back from the one run the open put. A write that fails (a cancel inside the put) ends the statement with
//! that error and leaves no file and no pool bytes.

use gravity_lowerv4_it as common;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, Ordering};
use std::time::Duration;

use common::multiset::Outcome;
use gravity_abi::breaker::BreakerKind;
use gravity_driver::{Error, Grant, Held, Result, POOL, REGIONS};

const FIXTURE: &str = include_str!("fixtures/d4291/ownspill-001.plan");
const MIB: u64 = 1 << 20;

static FILES_LIVE: AtomicI64 = AtomicI64::new(0);
static FAIL_WRITES: AtomicBool = AtomicBool::new(false);

struct VecFile(Vec<Vec<u8>>, u64);

impl Drop for VecFile {
    fn drop(&mut self) {
        FILES_LIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

impl gravity_driver::SpillFile for VecFile {
    fn write_epoch(&mut self, parts: &[(u32, &[u8])]) -> Result<()> {
        if FAIL_WRITES.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
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

/// The fixture run under an admitted lease over a pool armed at `capacity`; `edge`: when the sort's pipeline ends, G is
/// held whole and the pool's room kept (the d4168 edge), and the run's answer.
fn run(capacity: u64, edge: bool) -> Outcome {
    POOL.gate(0);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    POOL.arm(capacity);
    let res = Rc::new(POOL.admit_need_until(grant(), &|g| g.bytes, Duration::from_secs(5), &|| false).unwrap_or_else(|e| panic!("admitted: {e:?}")));
    let kept: Rc<RefCell<Vec<Held>>> = Rc::new(RefCell::new(Vec::new()));
    let (r, k) = (res.clone(), kept.clone());
    common::LEASE.with(|l| *l.borrow_mut() = Some(res.clone()));
    common::BETWEEN.with(|h| {
        *h.borrow_mut() = edge.then(|| {
            Box::new(move |p: usize, exec: &gravity_driver::Exec, _abort: &AtomicU8| {
                if exec.node().program().pipelines[p].breaker().is_none_or(|b| b.kind != BreakerKind::Sort) {
                    return;
                }
                REGIONS.trim();
                gravity_driver::DEPOT.trim();
                if let Some(h) = r.hold(r.left()) {
                    k.borrow_mut().push(h);
                }
                let room = POOL.capacity().saturating_sub(POOL.held());
                k.borrow_mut().push(POOL.keep(room));
            }) as Box<dyn FnMut(usize, &gravity_driver::Exec, &AtomicU8)>
        })
    });
    let out = common::run_fixture(FIXTURE, common::Mode::Straight).expect("decomposes").unwrap_or_else(|e| panic!("the rig: {e}"));
    common::BETWEEN.with(|h| *h.borrow_mut() = None);
    common::LEASE.with(|l| *l.borrow_mut() = None);
    kept.borrow_mut().clear();
    drop(res);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    gravity_driver::mem::reap::drain();
    assert_eq!(POOL.held(), 0, "every byte back at the statement's end (capacity {capacity}, edge {edge})");
    assert_eq!(FILES_LIVE.load(Ordering::Relaxed), 0, "every temp file dropped with its statement");
    POOL.arm(0);
    out
}

#[test]
fn a_finished_sort_goes_to_its_file_before_its_readers_open_is_refused() {
    // the driver reads the kernel's sealed mark from its own copy (it does not link the kernels)
    assert_eq!(gravity_driver::node::SORT_RUN_SEALED, gravity_kernels::ops::sort::entries::RUN_SEALED, "the sealed mark");
    gravity_driver::install_spill_factory(&FACTORY);
    let plenty = run(1 << 30, false);
    let Outcome::Rows(want) = &plenty else { panic!("the plentiful run: {plenty:?}") };
    assert_eq!(want.len(), 1, "one row: {want:?}");

    // the d4168 edge: the sort sealed in memory past G, the pool's last byte kept, its reader's open next
    let edge = run(256 * MIB, true);
    match &edge {
        Outcome::Rows(got) => assert_eq!(got, want, "the run read back from its file: the in-memory read's order, ties included"),
        Outcome::Error { sqlstate, message } => panic!("the reader's open beside a finished sort: {sqlstate} {message} (before: out of memory, 53200)"),
    }

    // a write the put cannot finish (a cancel inside it): the statement's error, no file, no pool byte left
    FAIL_WRITES.store(true, Ordering::Relaxed);
    let failed = run(256 * MIB, true);
    FAIL_WRITES.store(false, Ordering::Relaxed);
    match &failed {
        Outcome::Error { sqlstate, .. } => assert_eq!(sqlstate, "57014", "the put's cancel ends the statement: {failed:?}"),
        Outcome::Rows(_) => panic!("served though its sort's put failed"),
    }
}
