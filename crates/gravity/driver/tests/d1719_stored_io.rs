//! D1719 (M7-SPILL B4c, EXPRESS): a pair pass's claim writes its partition's drain part U(p) into its own file of the build
//! slot's stored stream (`SpillTable::stage_put` / `stage_flush` -> `SpillFile::write_epoch`) while another claim of the same
//! pass opens that file's build and kept parts (`SpillTable::open_stored` -> `SpillFile::open_part`). spillset's `read_part`
//! iterates the epochs an epoch write appends to: under load, a torn read ended the statement with SIGSEGV (fault address
//! 0x0) or missed an owner's extents (W8's k47: 900 build rows lost, PG 300000). The Defect-Test forces the interleaving: a
//! stored file whose epoch write takes 150 ms, and an open of the same file by another owner once the write has begun. The
//! parent's open lands inside the write (every run); the fix's waits for the stored-stream lock, so no open meets a write.
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use gravity_driver::{install_spill_factory, Result, RunReader, SpillFactory, SpillFile, SpillSet, SpillTable};

/// A stored file's epoch write in progress (the window a torn read needs).
static WRITING: AtomicBool = AtomicBool::new(false);
/// Opens of a stored file that met its write in progress.
static RACED: AtomicUsize = AtomicUsize::new(0);
/// Opens made (the test's reader ran at all).
static OPENED: AtomicUsize = AtomicUsize::new(0);
/// The next epoch write signals here once it has begun (the reader's cue).
static SLOW: AtomicBool = AtomicBool::new(false);

struct Factory;
struct Set;
struct File;

static TX: std::sync::Mutex<Option<mpsc::Sender<()>>> = std::sync::Mutex::new(None);

impl SpillFactory for Factory {
    fn create(&self) -> Result<Box<dyn SpillSet>> {
        Ok(Box::new(Set))
    }
}

impl SpillSet for Set {
    fn open(&mut self, _purpose: &str, _generation: u64, _worker: usize, _nparts: u32) -> Result<Box<dyn SpillFile>> {
        Ok(Box::new(File))
    }
}

impl SpillFile for File {
    fn write_epoch(&mut self, _parts: &[(u32, &[u8])]) -> Result<()> {
        WRITING.store(true, Ordering::SeqCst);
        if SLOW.swap(false, Ordering::SeqCst) {
            if let Some(tx) = TX.lock().unwrap().take() {
                tx.send(()).unwrap();
            }
            // the epoch list is being appended to: a reader of this file now reads it torn
            std::thread::sleep(Duration::from_millis(150));
        }
        WRITING.store(false, Ordering::SeqCst);
        Ok(())
    }
    fn read_part(&self, _part: u32, _sink: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
        Ok(())
    }
    fn open_part(&self, _part: u32) -> Result<Option<Box<dyn RunReader>>> {
        OPENED.fetch_add(1, Ordering::SeqCst);
        if WRITING.load(Ordering::SeqCst) {
            RACED.fetch_add(1, Ordering::SeqCst);
        }
        Ok(None)
    }
    fn spilled_bytes(&self) -> u64 {
        0
    }
}

static FACTORY: Factory = Factory;

/// The Node's pointer as the ctx owners hold it (`Inner::spill`): one heap table shared by the pass's owners.
struct Shared(*mut SpillTable);
unsafe impl Send for Shared {}
impl Shared {
    fn get(&self) -> *mut SpillTable {
        self.0
    }
}

#[test]
fn d1719_a_pair_claims_open_never_meets_another_claims_drain_part_write() {
    assert!(install_spill_factory(&FACTORY), "this binary owns the process's spill factory");
    const BITS: u8 = 3;
    let mut table = Box::new(SpillTable::with_slots(1));
    table.provision(3); // the leader sizes every owner's slot before the section
    // owner 1's build part 5, written in the build's section (its file now exists; the pass reads it)
    table.stage_put(0, 1, BITS, 5, b"build records").unwrap();
    table.stage_flush(0, 1, BITS).unwrap();
    for round in 0..4 {
        let (tx, rx) = mpsc::channel();
        *TX.lock().unwrap() = Some(tx);
        SLOW.store(true, Ordering::SeqCst);
        let (w, r) = (Shared(&mut *table), Shared(&mut *table));
        // owner 1's claim: its partition's drain part U(p) (`2 << bits` + p), staged and flushed as one epoch
        let writer = std::thread::spawn(move || {
            // SAFETY: the production law: the table outlives the pass; owner 1 writes its own file alone
            let t = unsafe { &mut *w.get() };
            t.stage_put(0, 1, BITS, (2 << BITS) + round, b"drain part").unwrap();
            t.stage_flush(0, 1, BITS).unwrap();
        });
        // owner 2's claim: partition 5 of every owner's file, owner 1's among them, once owner 1's write has begun
        let reader = std::thread::spawn(move || {
            rx.recv_timeout(Duration::from_secs(5)).expect("the drain part's write began");
            // SAFETY: as the pass's claims: `open_stored` reads the stream (the stored-stream lock is the fix)
            let t = unsafe { &*r.get() };
            t.open_stored(0, 1, 5).unwrap();
        });
        writer.join().unwrap();
        reader.join().unwrap();
    }
    assert_eq!(OPENED.load(Ordering::SeqCst), 4, "every round's reader opened owner 1's part");
    assert_eq!(RACED.load(Ordering::SeqCst), 0, "{} of 4 opens met owner 1's epoch write in progress (D1719)", RACED.load(Ordering::SeqCst));
}
