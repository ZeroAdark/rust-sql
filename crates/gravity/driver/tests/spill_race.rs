//! The Node's `SpillTable` under every ctx owner's first spill at once — the breakers e2e at
//! W = 8 with a 64 kB share, where eight first claims overflow in the same microsecond. The
//! set is made lazily by whichever owner spills first; made without its lock, every owner
//! that saw `None` created one and stored it over the last, dropping a set another owner was
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use gravity_driver::{install_spill_factory, Result, RunReader, SpillFactory, SpillFile, SpillSet, SpillTable};

static CREATED: AtomicUsize = AtomicUsize::new(0);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static OPENS_DURING_CREATE: AtomicUsize = AtomicUsize::new(0);
static CREATING: AtomicUsize = AtomicUsize::new(0);

struct SlowFactory;
struct SlowSet;
struct NoFile;

impl SpillFactory for SlowFactory {
    fn create(&self) -> Result<Box<dyn SpillSet>> {
        CREATING.fetch_add(1, Ordering::SeqCst);
        // the window: every owner that reaches the check now sees no set yet
        std::thread::sleep(Duration::from_millis(5));
        CREATING.fetch_sub(1, Ordering::SeqCst);
        CREATED.fetch_add(1, Ordering::SeqCst);
        LIVE.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(SlowSet))
    }
}

impl Drop for SlowSet {
    fn drop(&mut self) {
        LIVE.fetch_sub(1, Ordering::SeqCst);
    }
}

impl SpillSet for SlowSet {
    fn open(&mut self, _purpose: &str, _generation: u64, _worker: usize, _nparts: u32) -> Result<Box<dyn SpillFile>> {
        // an open that overlaps another owner's create is an open the other's store will orphan
        if CREATING.load(Ordering::SeqCst) > 0 {
            OPENS_DURING_CREATE.fetch_add(1, Ordering::SeqCst);
        }
        std::thread::sleep(Duration::from_millis(2));
        Ok(Box::new(NoFile))
    }
}

impl SpillFile for NoFile {
    fn write_epoch(&mut self, _parts: &[(u32, &[u8])]) -> Result<()> {
        Ok(())
    }
    fn read_part(&self, _part: u32, _sink: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
        Ok(())
    }
    fn open_part(&self, _part: u32) -> Result<Option<Box<dyn RunReader>>> {
        Ok(None)
    }
    fn spilled_bytes(&self) -> u64 {
        0
    }
}

static FACTORY: SlowFactory = SlowFactory;

/// The Node's pointer as the ctx owners hold it (`Inner::spill`): one heap table, every thread its own slot.
struct Shared(*mut SpillTable);
unsafe impl Send for Shared {}
impl Shared {
    fn get(&self) -> *mut SpillTable {
        self.0
    }
}

#[test]
fn w_first_spills_at_once_make_one_set_under_its_lock() {
    assert!(install_spill_factory(&FACTORY), "this binary owns the process's spill factory");
    const W: usize = 8;
    for round in 0..3 {
        let mut table = Box::new(SpillTable::with_slots(2));
        table.provision(W + 1); // the leader sizes every owner's slot before the section
        let created0 = CREATED.load(Ordering::SeqCst);
        let barrier = Arc::new(Barrier::new(W));
        let threads: Vec<_> = (0..W)
            .map(|o| {
                let shared = Shared(&mut *table);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    // SAFETY: the production law — the table outlives the section; owner `1 + o`
                    // touches its own file slot alone, the set is shared under its lock.
                    let t = unsafe { &mut *shared.get() };
                    // the owner's first scatter of each of two streams (gen 0): two opens on the one set
                    t.file(0, 1 + o, 3).map(|_| ()).unwrap();
                    t.file(1, 1 + o, 3).map(|_| ()).unwrap();
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let created = CREATED.load(Ordering::SeqCst) - created0;
        assert_eq!(created, 1, "round {round}: {W} first spills at once made {created} sets");
        assert_eq!(LIVE.load(Ordering::SeqCst), 1, "round {round}: the one set is the table's");
        assert_eq!(OPENS_DURING_CREATE.load(Ordering::SeqCst), 0, "round {round}: no open overlapped a create");
        assert!(table.spilled() && table.stream(0).gen0_parts() == 8 && table.stream(1).gen0_parts() == 8);
        table.clear();
        assert_eq!(LIVE.load(Ordering::SeqCst), 0, "round {round}: clear drops the set");
    }
}
