//! D4410: a statement holds exactly the dictionaries it reads (`Held`, or its `StmtDicts` cells); a list's dictionary
//! cells are a warm cache for the next statement (`Warm`), which the part cache's evicted hook empties (`let_warm_go`).
//! Readers borrowed the list's cells before, so a running statement's clone of its list kept every dictionary the list
//! held, its released parts' too, and no trim could free them.

use super::*;
use pgsync::atomic::{AtomicBool, AtomicPtr};
use std::sync::Weak;

/// One (face, part) slot of a list's warm cache: the entry a statement opened there for the next statement over the
/// list's generation. Statements take a clone (`get`) and hold it; the part cache's evicted hook empties the slot.
pub(super) struct Warm<T> {
    slot: Mutex<Option<Arc<T>>>,
    /// D4554: the slot holds an entry (set and cleared under its lock): a read of an empty slot takes no lock (a part whose
    /// faces no statement read, its Stats answered from the catalog, is asked at every granule a verdict reads).
    set: AtomicBool,
}

impl<T> Warm<T> {
    pub(super) fn new() -> Warm<T> {
        Warm { slot: Mutex::new(None), set: AtomicBool::new(false) }
    }

    pub(super) fn get(&self) -> Option<Arc<T>> {
        if !self.set.load(Ordering::Acquire) {
            return None;
        }
        pgsync::lock(&self.slot).as_ref().map(Arc::clone)
    }

    pub(super) fn is_set(&self) -> bool {
        self.set.load(Ordering::Acquire)
    }

    /// Keep `e` for the next statement, unless another statement's entry is already here: the slot's entry.
    pub(super) fn put(&self, e: Arc<T>) -> Arc<T> {
        self.put_live(e, |_| false)
    }

    /// D4504: keep `e` for the next statement unless a live entry is already here (`stale` says which are not, and a
    /// stale one goes): the slot's entry.
    pub(super) fn put_live(&self, e: Arc<T>, stale: impl Fn(&T) -> bool) -> Arc<T> {
        let mut g = pgsync::lock(&self.slot);
        if let Some(w) = g.as_ref().filter(|w| !stale(w)) {
            return Arc::clone(w);
        }
        let gone = g.replace(Arc::clone(&e));
        self.set.store(true, Ordering::Release);
        drop(g);
        // the stale entry frees after the slot's lock (its holds' bytes leave the census at its last drop)
        drop(gone);
        e
    }

    /// D4504: the slot's entry goes when `stale` says so (the faces hook; nothing while another thread holds the slot on
    /// the memory ladder's pass, D4398).
    fn take_if(&self, stale: impl Fn(&T) -> bool) -> Option<Arc<T>> {
        let mut g = memledger::ladder_pass::lock_or_skip(&self.slot)?;
        if g.as_deref().is_some_and(stale) {
            self.set.store(false, Ordering::Release);
            g.take()
        } else {
            None
        }
    }

    /// The evicted hook's face: the slot's entry goes (`None` too while another thread holds the slot on the memory
    /// ladder's pass, D4398).
    fn take(&self) -> Option<Arc<T>> {
        let mut g = memledger::ladder_pass::lock_or_skip(&self.slot)?;
        self.set.store(false, Ordering::Release);
        g.take()
    }
}

/// D4504: faces per chunk of a statement's face table ([`Held`]), one word of its face bitmap: a statement allocates the
/// chunks of the faces it reads (most read one: the columns below attno 64), never one charged slot per possible face
/// (1,665 slots of a charged slice, 104 B each: 169 KiB per statement relation and kind; now 27 slots and a chunk of 64,
/// 9.3 KiB).
const CHUNK: usize = u64::BITS as usize;

/// One statement relation's own `Arc` of each dictionary entry it read, by face and part (`Arc::into_raw` in each cell):
/// readers borrow an entry for the statement's life. The face table is allocated at the first dictionary the statement
/// reads (a statement that reads none pays nothing), a chunk of [`CHUNK`] faces at a time.
pub(super) struct Held<T> {
    nparts: usize,
    faces: OnceLock<StmtSlice<OnceLock<StmtSlice<OnceLock<StmtSlice<AtomicPtr<T>>>>>>>,
    /// One bit per allocated face, set before the face publishes: the drop walks these, never every face slot.
    touched: OnceLock<StmtSlice<AtomicU64>>,
}

impl<T> Held<T> {
    pub(super) fn new(nparts: usize) -> Held<T> {
        Held { nparts, faces: OnceLock::new(), touched: OnceLock::new() }
    }

    #[inline]
    pub(super) fn get(&self, p: usize, face: u32) -> Option<&T> {
        let f = face as usize;
        let c = self.faces.get()?.get(f / CHUNK)?.get()?.get(f % CHUNK)?.get()?.get(p)?;
        raw::arc_cell_get(c)
    }

    /// Hold `e` as part `p`'s face `face` (a racing reader's entry wins, this clone drops): the held entry.
    pub(super) fn publish(&self, p: usize, face: u32, e: Arc<T>) -> &T {
        let n = MAX_FACE_ATTNO + MAX_SHRED_FACES;
        let f = face as usize;
        let chunks = self.faces.get_or_init(|| StmtSlice::collect((0..n.div_ceil(CHUNK)).map(|_| OnceLock::new()))); // alloc: Exec charged gravity_driver::stmt_context()
        let chunk = chunks[f / CHUNK].get_or_init(|| StmtSlice::collect((0..CHUNK).map(|_| OnceLock::new()))); // alloc: Exec charged gravity_driver::stmt_context()
        let cells = chunk[f % CHUNK].get_or_init(|| {
            let touched = self.touched.get_or_init(|| StmtSlice::collect((0..n.div_ceil(64)).map(|_| AtomicU64::new(0)))); // alloc: Exec charged gravity_driver::stmt_context()
            touched[face as usize / 64].fetch_or(1 << (face % 64), Ordering::Release);
            StmtSlice::collect((0..self.nparts).map(|_| AtomicPtr::new(core::ptr::null_mut()))) // alloc: Exec charged gravity_driver::stmt_context()
        });
        raw::arc_cell_publish(&cells[p], e).0
    }
}

impl<T> Drop for Held<T> {
    fn drop(&mut self) {
        let (Some(faces), Some(touched)) = (self.faces.get(), self.touched.get()) else { return };
        // bounded("other: the statement's end lets go of every dictionary it read, one release per face and part; a teardown runs to its end (a Drop cannot stop), as PG's MemoryContextDelete at the executor's end polls no deeper")
        for (w, word) in touched.iter().enumerate() {
            let mut bits = word.load(Ordering::Acquire);
            // bounded("key-columns: the set bits of one word of the face bitmap, at most 64")
            while bits != 0 {
                let f = w * 64 + bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let Some(cells) = faces[f / CHUNK].get().and_then(|c| c[f % CHUNK].get()) else { continue };
                // bounded("other: the statement's end lets go of every dictionary it read, one release per face and part")
                for c in cells.iter() {
                    // the statement is over
                    drop(raw::arc_cell_take(c));
                }
            }
        }
    }
}

/// The part lists statements installed (`Resolved::install`), each once: the evicted hook's walk (`let_warm_go`).
static LIVE: Mutex<Vec<Weak<PartList>>> = Mutex::new(Vec::new()); // alloc: Cold

/// `list` joins the live lists (once per list).
pub(super) fn register(list: &Arc<PartList>) {
    if list.registered.swap(true, Ordering::AcqRel) {
        return;
    }
    let mut v = pgsync::lock(&LIVE);
    // the dead lists go once the walk doubles (amortized: one prune per doubling)
    if v.len() >= 16 && v.len().is_power_of_two() {
        v.retain(|w| w.strong_count() > 0);
    }
    v.push(Arc::downgrade(list)); // alloc: Cold
}

/// The live part lists, each upgraded once (the hooks walk them with no list lock held); `None` on the memory ladder's
/// pass while another thread holds the registry (D4398).
fn live_lists() -> Option<Vec<Arc<PartList>>> {
    let v = memledger::ladder_pass::lock_or_skip(&LIVE)?;
    Some(v.iter().filter_map(Weak::upgrade).collect()) // alloc: Cold
}

/// D4410: the part cache's evicted hook (residency.rs `part_cache_evicted`): every live list's warm dictionary cells go
/// and the list keeps none from now on (`warm_off`: `LISTS` let it go, and the next statement builds its list again).
/// What stays of each dictionary is what running statements hold. The entries free after every lock (a `DictHandle`'s
/// drop frees its images). On the memory ladder's pass a list or a slot another thread holds is skipped (D4398).
pub fn let_warm_go() {
    let Some(lists) = live_lists() else { return };
    let mut gone: Vec<Arc<DictEntry>> = Vec::new(); // alloc: Cold
    let mut gone_codes: Vec<Arc<CodesEntry>> = Vec::new(); // alloc: Cold
    // bounded("other: the live part lists, one per relation generation statements hold now")
    for l in &lists {
        // D4504: with the metacache on the list stays in `LISTS` (residency.rs `part_cache_evicted`) and warms again
        if !crate::inputs::metacache_enabled() {
            l.warm_off.store(true, Ordering::Release);
        }
        // bounded("catalog-sized: the list's opened faces (the columns statements read) x its parts, one slot each")
        for lane in l.faces.lanes.iter().filter_map(OnceLock::get) {
            gone.extend(lane.dicts.iter().filter_map(Warm::take)); // alloc: Cold
            gone_codes.extend(lane.codes.iter().filter_map(Warm::take)); // alloc: Cold
        }
    }
    drop(gone);
    drop(gone_codes);
    drop(lists);
}

/// D4504: the part cache's faces hook (residency.rs `part_cache_faces_evicted`): every live list's metadata entries whose
/// part's faces the content trim let go (`OpenPart::evict_faces`) leave its warm cells; what stays of each is what
/// running statements hold, and its bytes leave the part cache's census at its last drop. On the memory ladder's pass a
/// list or a slot another thread holds is skipped (D4398).
pub fn let_stale_faces_go() {
    let Some(lists) = live_lists() else { return };
    // bounded("other: the live part lists, one per relation generation statements hold now")
    for l in &lists {
        // bounded("catalog-sized: the list's opened faces (the columns statements read) x its parts, one slot each")
        for lane in l.faces.lanes.iter().filter_map(OnceLock::get) {
            // bounded("catalog-sized: the list's parts, one slot each")
            for w in lane.faces.iter() {
                // each entry frees after its slot's lock (`take_if` returns it unlocked), no other lock held
                drop(w.take_if(FaceBodies::stale));
            }
        }
    }
    drop(lists);
}

/// Whether a list keeps what its statements open for the next statement (false once the evicted hook let it go).
pub(super) fn warm(list: &PartList) -> bool {
    !list.warm_off.load(Ordering::Acquire)
}
