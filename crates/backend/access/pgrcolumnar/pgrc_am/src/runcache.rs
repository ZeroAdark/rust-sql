//! SORTRUNS-E0 L1 (pgrc-plans/SORTRUNS-E0-STEP1.md D2): the process's run reverses. The IDs a sorted run homes
//! (`pgrc_format::sortrun`, a run of form LABELS or FRAMEIX) are found through its reverse, `runrev-<run_id>.pgrcv`
//! (`pgrc_format::runrev`): one [`RunCell`] per run of a table directory, bound by the catalog to every generation
//! that holds the run (carried from the generation a publish or a load builds on, `crate::catalog`), holds the open
//! reverse (page 0 and the sections read at open) and the pages lookups read.
//!
//! The pages count in the row-map cache's budget and ring (`pgrust.pgrc_rowmap_cache_size`, [`crate::mapcache`]): a
//! page is admitted and evicted as a map is, and a page a lookup holds stays valid until it lets go. The file is
//! immutable for the run's life (a member rewritten in place keeps its virtual frames), so a cached page never goes
//! stale; the cell, and its pages, go when the last generation holding the run does.
//!
//! Every page read checks its crc (bound to the page number) and, on first use, its section's structure; a bad page,
//! a short file, or a page 0 that does not match the run's `EXT_RUNS` entry is `ERRCODE_DATA_CORRUPTED`, never an
//! answer. An I/O failure is `ERRCODE_IO_ERROR`.

use std::collections::HashMap;
use std::ffi::CString;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use pgrc_format::runrev::{Hit, Page, Pages, RunRev, PAGE};
use pgrc_format::sortrun::SortRun;
use pgrc_format::{FormatError, FormatResult};
use pgrc_read::mem::{Sweep, Victim};
use pgrc_read::{PartIo, ReadError};
use types_error::{PgError, PgResult};

const AT: &str = "run reverse read";

/// Where a cell's pages come from.
enum Source {
    /// The run's file in the table directory, opened on the first read.
    File(CString),
    /// A whole file in memory (tests).
    Image(Arc<[u8]>),
}

/// A cached page and its CLOCK reference bit.
struct Slot {
    /// D4537 T18c: the page's bytes on the part cache, for as long as the cell holds it.
    _charge: memledger::Charge,
    page: Arc<Page>,
    referenced: AtomicBool,
}

const CHARGE_OWNER: memledger::Owner = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::PartCache));

fn charge_for(bytes: u64) -> memledger::Charge {
    memledger::Charge::open(CHARGE_OWNER, memledger::Release::Evict, memledger::Kind::Region, memledger::charge::Door::Heap, bytes)
}

/// One run's reverse in this process: its file, the open reverse and the pages read (see the module note).
pub struct RunCell {
    /// D4537 T18c: the open reverse's bytes on the part cache (set with `rev`, dropped with the cell).
    rev_charge: pgsync::Mutex<memledger::Charge>,
    run_id: u64,
    rev_crc: u32,
    src: Source,
    me: Weak<RunCell>,
    io: pgsync::Mutex<Option<Arc<dyn PartIo>>>,
    rev: pgsync::Mutex<Option<Arc<RunRev>>>,
    pages: pgsync::Mutex<HashMap<u64, Slot>>,
    /// The last read's I/O failure: the format reader sees a short page; the lookup reports the errno.
    io_error: pgsync::Mutex<Option<ReadError>>,
    reads: AtomicU64,
}

impl RunCell {
    fn new(run: &SortRun, src: Source) -> Arc<RunCell> {
        Arc::new_cyclic(|me| RunCell {
            run_id: run.run_id,
            rev_crc: run.rev_crc,
            src,
            me: me.clone(),
            io: pgsync::Mutex::new(None),
            rev: pgsync::Mutex::new(None),
            rev_charge: pgsync::Mutex::new(memledger::Charge::NONE),
            pages: pgsync::Mutex::new(HashMap::new()),
            io_error: pgsync::Mutex::new(None),
            reads: AtomicU64::new(0),
        })
    }

    /// The cell of `run`'s reverse in table directory `dir`. Nothing is read until a lookup.
    pub fn open(dir: &str, run: &SortRun) -> Arc<RunCell> {
        let path = format!("{dir}/{}", pgrc_format::dirlayout::runrev_file_name(run.run_id));
        // a directory path holds no NUL (the catalog's own paths); one that did reads as a missing file
        RunCell::new(run, Source::File(CString::new(path).unwrap_or_default()))
    }

    /// SORTRUNS-E0 L2: a cell over `run`'s reverse at `path` before its publish names it (the run job's staged temp:
    /// its DV carry routes through it).
    pub fn open_path(path: &str, run: &SortRun) -> Arc<RunCell> {
        RunCell::new(run, Source::File(CString::new(path).unwrap_or_default()))
    }

    /// A cell over a whole reverse in memory (tests).
    #[doc(hidden)]
    pub fn image(run: &SortRun, bytes: Vec<u8>) -> Arc<RunCell> {
        RunCell::new(run, Source::Image(bytes.into()))
    }

    /// Whether this cell is `run`'s (the same run and the same file).
    pub fn is_of(&self, run: &SortRun) -> bool {
        self.run_id == run.run_id && self.rev_crc == run.rev_crc
    }

    /// The pages read from the file in this process (a cached page is not read again).
    pub fn reads(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }

    /// D4537 T18c (tests): the bytes the cell has charged to the part cache, its reverse's and its pages'.
    #[doc(hidden)]
    pub fn charged(&self) -> (u64, u64) {
        (pgsync::lock(&self.rev_charge).committed(), pgsync::lock(&self.pages).values().map(|s| s._charge.committed()).sum())
    }

    /// The pages the cell holds now.
    pub fn held(&self) -> usize {
        pgsync::lock(&self.pages).len()
    }

    /// The open reverse of `run` (this cell's): opened on the first ask, checked against the run's `EXT_RUNS` entry.
    pub fn rev(&self, run: &SortRun) -> PgResult<Arc<RunRev>> {
        debug_assert!(self.is_of(run), "a run's cell");
        if let Some(r) = pgsync::lock(&self.rev).as_ref() {
            return Ok(Arc::clone(r));
        }
        let r = Arc::new(RunRev::open(run, self).map_err(|e| self.error(e))?);
        let mut slot = pgsync::lock(&self.rev);
        if slot.is_none() {
            *pgsync::lock(&self.rev_charge) = charge_for(r.mem_bytes() as u64);
        }
        Ok(Arc::clone(slot.get_or_insert(r)))
    }

    /// Where `run`'s reverse sends ID `id`.
    pub fn lookup(&self, run: &SortRun, id: u64) -> PgResult<Hit> {
        let rev = self.rev(run)?;
        rev.lookup(id, self).map_err(|e| self.error(e))
    }

    /// A reverse read's error as SQL sees it: the I/O failure behind a short page, else corruption.
    pub fn error(&self, e: FormatError) -> Box<PgError> {
        if let Some(io) = pgsync::lock(&self.io_error).take() {
            return crate::read_error(io);
        }
        crate::corrupt(&format!("sorted run {} reverse ({}): {e}", self.run_id, pgrc_format::dirlayout::runrev_file_name(self.run_id)))
    }

    fn io(&self, path: &CString) -> FormatResult<Arc<dyn PartIo>> {
        let mut slot = pgsync::lock(&self.io);
        if let Some(io) = slot.as_ref() {
            return Ok(Arc::clone(io));
        }
        let io: Arc<dyn PartIo> = Arc::new(pgrc_read::VfsPartIo::open(path).map_err(|e| self.io_failed(e))?);
        *slot = Some(Arc::clone(&io));
        Ok(io)
    }

    /// A read's failure for the format reader: a format error as is, an I/O one kept for [`Self::error`].
    fn io_failed(&self, e: ReadError) -> FormatError {
        match e {
            ReadError::Format(f) => f,
            other => {
                *pgsync::lock(&self.io_error) = Some(other);
                FormatError::Truncated { at: AT }
            }
        }
    }

    /// Page `no` from the source, its crc checked.
    fn fetch(&self, no: u64) -> FormatResult<Page> {
        let at = no.checked_mul(PAGE as u64).ok_or(FormatError::Corrupt { at: AT })?;
        let mut buf = vec![0u8; PAGE].into_boxed_slice();
        match &self.src {
            Source::Image(b) => {
                let s = usize::try_from(at).ok().and_then(|a| b.get(a..a + PAGE)).ok_or(FormatError::Truncated { at: AT })?;
                buf.copy_from_slice(s);
            }
            Source::File(path) => {
                let io = self.io(path)?;
                if at + PAGE as u64 > io.len() {
                    return Err(FormatError::Truncated { at: AT });
                }
                io.pread_exact(at, &mut buf, AT).map_err(|e| self.io_failed(e))?;
            }
        }
        self.reads.fetch_add(1, Ordering::Relaxed);
        Page::check(no, buf)
    }
}

impl Pages for RunCell {
    fn page(&self, no: u64) -> FormatResult<Arc<Page>> {
        if let Some(s) = pgsync::lock(&self.pages).get(&no) {
            s.referenced.store(true, Ordering::Relaxed);
            return Ok(Arc::clone(&s.page));
        }
        let page = Arc::new(self.fetch(no)?);
        let bytes = page.mem_bytes() as u64;
        // the ring's lock before the cell's (the admission order); a racing read's page is kept and this one dropped
        let mut kept = None;
        crate::mapcache::admit_other(
            bytes,
            || {
                let mut pages = pgsync::lock(&self.pages);
                if let Some(s) = pages.get(&no) {
                    kept = Some(Arc::clone(&s.page));
                    return false;
                }
                pages.insert(no, Slot { page: Arc::clone(&page), referenced: AtomicBool::new(true), _charge: charge_for(bytes) });
                true
            },
            || Box::new(PageSlot { cell: self.me.clone(), no, bytes }),
        );
        Ok(kept.unwrap_or(page))
    }
}

impl Drop for RunCell {
    /// The pages still held leave the cache's total with the cell.
    fn drop(&mut self) {
        let bytes: u64 = pgsync::lock(&self.pages).values().map(|s| s.page.mem_bytes() as u64).sum();
        crate::mapcache::release_other(bytes);
    }
}

/// A cached page's ring slot.
struct PageSlot {
    cell: Weak<RunCell>,
    no: u64,
    bytes: u64,
}

impl Victim for PageSlot {
    fn visit(&self) -> Sweep {
        let Some(cell) = self.cell.upgrade() else { return Sweep::Dead };
        let mut pages = pgsync::lock(&cell.pages);
        let Some(s) = pages.get(&self.no) else { return Sweep::Dead };
        if s.referenced.swap(false, Ordering::Relaxed) {
            return Sweep::Referenced;
        }
        pages.remove(&self.no);
        drop(pages);
        crate::mapcache::release_other(self.bytes);
        crate::mapcache::count_eviction();
        Sweep::Evicted(self.bytes)
    }

    fn dead(&self) -> bool {
        self.cell.upgrade().is_none_or(|c| !pgsync::lock(&c.pages).contains_key(&self.no))
    }
}
