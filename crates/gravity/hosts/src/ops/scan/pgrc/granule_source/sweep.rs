//! D3115: a statement's shared sweep of one part's `PROGRAM` table. Each source swept its own copy of the part's whole
//! dictionary, so W workers paid every entry's body W times and the sweep's wall stayed the serial one. The sources that
//! reach the part now claim `SWEEP_CHUNK`-code chunks of one table (`LANE_CLAIM`) and fill disjoint words of it; each
//! publishes its own header over the shared words once every chunk is swept.

use std::sync::Weak;

use gravity_abi::site::{CLAIM_WAIT, SWEEP_CHUNK};
use gravity_dict::lane::{ELEM_BITS, ELEM_FP128, ELEM_I64};

use super::*;

/// How long a claimer that found every chunk claimed and one still being swept sleeps before the kernel asks again
/// (after `SWEEP_SPINS` checks): a chunk is a few hundred microseconds of bodies.
const SWEEP_WAIT: core::time::Duration = core::time::Duration::from_micros(50);
const SWEEP_SPINS: u32 = 1024;

/// One part's table for the statement: the data words, then the fault bitmap.
pub(super) struct SharedSweep {
    /// CONVERT wave 2: the words on the statement (or portal) whose sources share them (`gravity_driver::stmt_context`; no
    /// one breaker owns a table every source of the site reads to the last drop); first, so it drops before them.
    _charge: memledger::Charge,
    words: Box<[AtomicU64]>,
    data_words: usize,
    elem: u8,
    ncodes: u32,
    next: AtomicU64,
    /// Chunks swept to the end.
    done: AtomicU64,
    /// Chunks a source claimed and left unswept (its claim ended in an error or a cancel), for the next claimer.
    orphans: Mutex<Vec<u64>>,
    norphans: AtomicU64,
    /// The fault count plus one, once a source counted it (0: not yet).
    total: AtomicU64,
}

impl SharedSweep {
    fn new(elem: u8, ncodes: u32) -> PgResult<SharedSweep> {
        let n = ncodes as usize;
        let data_words = match elem {
            ELEM_BITS => n.div_ceil(64),
            ELEM_I64 => n,
            ELEM_FP128 => 2 * n,
            _ => return Err(crate::pg_err("gravity: a program table of an element the program cannot fill")),
        }
        .max(1);
        let words = super::raw::zeroed_words(data_words + n.div_ceil(64).max(1));
        let bytes = core::mem::size_of_val(&*words) as u64;
        let charge = gravity_driver::stmt_context().map_or(memledger::Charge::NONE, |(o, r)| memledger::Charge::open(o, r, memledger::Kind::Region, memledger::charge::Door::Heap, bytes));
        Ok(SharedSweep { _charge: charge, words, data_words, elem, ncodes, next: AtomicU64::new(0), done: AtomicU64::new(0), orphans: Mutex::new(Vec::new()), norphans: AtomicU64::new(0), total: AtomicU64::new(0) }) // alloc: Exec
    }

    pub(super) fn elem(&self) -> u8 {
        self.elem
    }

    pub(super) fn ncodes(&self) -> u32 {
        self.ncodes
    }

    pub(super) fn data(&self) -> *const u8 {
        self.words.as_ptr() as *const u8
    }

    pub(super) fn faults(&self) -> *const u64 {
        self.words[self.data_words..].as_ptr() as *const u64
    }

    pub(super) fn complete(&self) -> bool {
        self.done.load(Ordering::Acquire) == (self.ncodes as u64).div_ceil(SWEEP_CHUNK)
    }

    /// The table's fault count once every chunk is swept (a source's own count covers its chunks only).
    pub(super) fn total_faults(&self) -> u32 {
        let t = self.total.load(Ordering::Acquire);
        if t != 0 {
            return (t - 1) as u32;
        }
        let n: u64 = self.words[self.data_words..].iter().map(|w| w.load(Ordering::Relaxed).count_ones() as u64).sum();
        self.total.store(n + 1, Ordering::Release);
        n as u32
    }

    fn orphan(&self) -> Option<u64> {
        if self.norphans.load(Ordering::Acquire) == 0 {
            return None;
        }
        let c = pgsync::lock(&self.orphans).pop()?;
        self.norphans.fetch_sub(1, Ordering::AcqRel);
        Some(c)
    }
}

/// A source's part in one sweep: its table lane, the shared sweep (`None`: a table this source sweeps alone, an adaptive
/// call's or one its rows fill), the chunk it is sweeping, and a lone sweep's next chunk and code count.
pub(super) struct SweepShare {
    pub(super) table: usize,
    pub(super) sweep: Option<Arc<SharedSweep>>,
    cur: Option<u64>,
    next: u64,
    ncodes: u32,
}

impl SweepShare {
    pub(super) fn shared(table: usize, sweep: Arc<SharedSweep>) -> SweepShare {
        SweepShare { table, sweep: Some(sweep), cur: None, next: 0, ncodes: 0 }
    }

    pub(super) fn alone(table: usize, ncodes: u32) -> SweepShare {
        SweepShare { table, sweep: None, cur: None, next: 0, ncodes }
    }

    /// `LANE_CLAIM`: the first code of this source's next chunk (the previous one is swept), the code count once every
    /// chunk is swept, else `CLAIM_WAIT` (the kernel checks the cancel byte and asks again).
    pub(super) fn claim(&mut self) -> u32 {
        let Some(sw) = self.sweep.as_deref() else {
            let c = self.next.min(self.ncodes as u64);
            self.next = c + SWEEP_CHUNK;
            return c as u32;
        };
        if self.cur.take().is_some() {
            sw.done.fetch_add(1, Ordering::AcqRel);
        }
        let n = sw.ncodes as u64;
        for spin in 0..=SWEEP_SPINS {
            if let Some(c) = sw.orphan() {
                self.cur = Some(c);
                return c as u32;
            }
            if sw.next.load(Ordering::Relaxed) < n {
                let c = sw.next.fetch_add(SWEEP_CHUNK, Ordering::Relaxed);
                if c < n {
                    self.cur = Some(c);
                    return c as u32;
                }
            }
            if sw.complete() {
                return sw.ncodes;
            }
            if spin < SWEEP_SPINS {
                core::hint::spin_loop();
            }
        }
        waiter::sleep(SWEEP_WAIT);
        CLAIM_WAIT as u32
    }

    /// The chunk this source claimed and never swept (its claim ended mid-sweep), handed to the next claimer.
    pub(super) fn abandon(&mut self) {
        if let (Some(c), Some(sw)) = (self.cur.take(), self.sweep.as_deref()) {
            pgsync::lock(&sw.orphans).push(c); // alloc: Cold
            sw.norphans.fetch_add(1, Ordering::AcqRel);
        }
    }
}

/// The statement's sweeps by (relation, part, column, table), held while a source pins one: a part every source left
/// sweeps again if a source comes back to it.
pub struct StmtSweeps {
    map: Mutex<HashMap<StmtKey, Weak<SharedSweep>>>,
}

impl StmtSweeps {
    pub(super) fn new() -> StmtSweeps {
        StmtSweeps { map: Mutex::new(HashMap::new()) } // alloc: Exec
    }

    /// The sweep at `key`, and whether this call made it (a source that joins one counts as a shared table).
    pub(super) fn share(&self, key: StmtKey, elem: u8, ncodes: u32) -> PgResult<(Arc<SharedSweep>, bool)> {
        let mut m = pgsync::lock(&self.map);
        if let Some(s) = m.get(&key).and_then(Weak::upgrade) {
            return Ok((s, false));
        }
        if m.len() >= 64 {
            m.retain(|_, w| w.strong_count() > 0);
        }
        let s = Arc::new(SharedSweep::new(elem, ncodes)?); // alloc: Exec
        m.insert(key, Arc::downgrade(&s)); // alloc: Exec
        Ok((s, true))
    }
}

#[cfg(test)]
mod tests {
    /// CONVERT wave 2: a shared sweep's table is one charge on the statement, whichever source made it, held while any
    /// sharer holds the table and freed at the last drop.
    #[test]
    fn a_shared_sweep_is_one_statement_charge_freed_at_the_last_drop() {
        std::thread::spawn(|| {
            memledger::bind_backend(3993, 0);
            memledger::statement_begin();
            let (stmt, _) = memledger::charge::context();
            let c = || memledger::charge::owner_row(stmt).committed;
            let sweeps = super::StmtSweeps::new();
            let key = super::StmtKey { rel: 0, part: 3, attno: 1, fingerprint: 7 };
            let (a, made_a) = sweeps.share(key, super::ELEM_I64, 10_000).unwrap();
            let (b, made_b) = sweeps.share(key, super::ELEM_I64, 10_000).unwrap();
            assert!(made_a && !made_b && std::sync::Arc::ptr_eq(&a, &b), "the second sharer joins the first's table");
            let words = 10_000 + 10_000usize.div_ceil(64);
            assert_eq!(c(), 8 * words as i64, "one charge on the statement: the data words and the fault bitmap");
            drop(a);
            assert_eq!(c(), 8 * words as i64, "a sharer still holds it");
            drop(b);
            assert_eq!(c(), 0, "freed at the last drop");
            memledger::statement_end();
            memledger::unbind_backend();
        })
        .join()
        .unwrap();
    }
}
