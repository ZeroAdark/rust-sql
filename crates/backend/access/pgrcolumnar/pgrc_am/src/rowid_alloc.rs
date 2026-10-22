//! IX0 L1 (OC-2 a): a pgrc table's row-ID counter. It is non-transactional: an ID handed out is never handed out
//! again, whether its transaction commits, aborts or the server crashes.
//!
//! - **State:** one entry per table directory (`pgrc_<relfilenumber>`, or a temp table's `pgrc_t<procno>_<n>`) in a
//!   `process_global` map. pgrust's backends are threads of one process, so there is one map for all of them, as
//!   there is one catalog. A reservation takes `[next, next + align_up(n))` under the map's mutex. Keying by directory,
//!   not relation OID, gives a new relfilenumber (TRUNCATE, a rewrite) a fresh counter, and leaves a rolled-back
//!   TRUNCATE's old directory with its own counter.
//! - **Durability:** `high` is the durable high-water mark. A reservation past it first raises it to the reservation's
//!   end plus a prefetch batch ([`PREFETCH`] IDs): `ROWID_RESERVE` goes into WAL, then the 16-byte `rowid` file is written and
//!   its sync request queued, all under `DELAY_CHKPT_START` (`pgrc_wal_seams::log_rowid_reserve`). That runs inside the
//!   mutex, so no ID is used before its reservation is in WAL. Prefetch is rare (one batch per 64 Ki IDs; an extent
//!   at L3), so one global mutex across the WAL insert is accepted (main 2026-10-04). If B6 shows contention, it
//!   becomes per-directory locks.
//! - **Give-back (IDBURN, main 10-05):** a bulk writer's unused tail `[end, hi)` goes back when nothing was handed out
//!   after it (`next == hi`: `next = end`), in memory only; `high` never drops, so a crash burns the tail instead. Else
//!   it is burned. IDs below `end` were used and are never handed out again.
//! - **Load:** the first reservation of a directory in this server's life starts at `max(rowid file, newest
//!   manifest's EXT_ROWID floor, ROWID_FIRST)`. IDs below the old high that were never handed out are burned, never
//!   reused. Redo raises the file and drops the entry, so a promoted standby loads from the file.
//! - **Forgetting:** a directory created, emptied or removed drops its entry ([`forget`]), so a reused relfilenumber
//!   never inherits an old in-memory `high` that its own file and WAL do not hold.
//! - **Segment extents (DV-1, L3):** a live segment's IDs are reserved 1,024 heap blocks (297,984 IDs) at a time,
//!   the first at activation, each further one by the inserter whose block first passes the last. The table of every
//!   live segment's extents is not transactional: an extent reserved stays reserved whether its transaction commits
//!   or not. It is one `segext` file per directory, rewritten whole in place; `ROWID_EXTENTS` carries the whole table
//!   (WAL first, then the file and its sync request, under `DELAY_CHKPT_START`, as `ROWID_RESERVE`), so a torn file
//!   is repaired by redo and a replay is identical. A segment's entries are pruned when its slot is activated again
//!   (with any entry no ring record names: a leaked activation's). Readers see the table as an `Arc` snapshot
//!   ([`extents`]) and never wait on its WAL insert; writers serialize on the counters' mutex.
//! - **Budget (OC-3 a, user 2026-10-04):** a WARNING per reservation once fewer than 10 % of the IDs are left; past
//!   99 % a reservation is refused with 54000. VACUUM FULL renumbers (L4); autovacuum never does.
//! - **Renumbering (L4, OC-4 C + fold-gated, main 2026-10-05):** VACUUM FULL rewrites each part's rows under dense IDs
//!   from a cursor ([`renumber_next`], [`renumber_place`]), then lowers the counter ([`reset`]: `ROWID_SET`, the file
//!   written exactly). [`note_renumber`] keeps the last outcome for `pgrc_rowid_stats` ([`renumber_text`]).

use std::collections::BTreeMap;

use std::sync::Arc;

use pgrc_format::dirlayout::{ROWID_FILE_NAME, SEGEXT_FILE_NAME};
use pgrc_format::rowid::{align_up, decode_rowid_file, decode_segext, encode_rowid_file, encode_segext, SegExtent, IDS_PER_BLOCK, ROWID_FIRST, ROWID_LIMIT, SEGEXT_MAX_ENTRIES, SEG_EXTENT_IDS, SEG_MAX_EXTENTS, SEG_SOFT_EXTENTS};
use pgrc_write::wvfs::WriteVfs;
use pgrc_write::{WriteError, WriteResult};
use types_error::{PgError, PgResult};
use types_rel::RelationData;

/// A row's ctid from its ID (re-exported for the AM dispatch, which does not see the format crate).
pub use pgrc_format::rowid::tid_of;

const _: () = assert!(IDS_PER_BLOCK == types_storage::bufpage::MaxHeapTuplesPerPage as u64, "a virtual block is a heap page's offsets");

/// The smallest batch a reservation raises the durable mark by (the `VAR_OID_PREFETCH` pattern).
pub const PREFETCH: u64 = align_up(64 * 1024);
/// Past this a reservation warns: fewer than 10 % of the IDs are left.
pub const WARN_AT: u64 = ROWID_LIMIT / 10 * 9;
/// Past this a reservation is refused: fewer than 1 % are left (the xidStopLimit pattern).
pub const REFUSE_AT: u64 = ROWID_LIMIT / 100 * 99;

/// A reservation: `[lo, hi)`, both multiples of 291.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reserved {
    pub lo: u64,
    pub hi: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct State {
    /// The next ID to hand out.
    next: u64,
    /// Every ID below this is reserved durably (WAL, then the file).
    high: u64,
}

/// What a reservation ending at `hi` leaves of the budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Budget {
    Ok,
    Warn,
    Refuse,
}

pub fn budget(hi: u64) -> Budget {
    if hi > REFUSE_AT {
        Budget::Refuse
    } else if hi > WARN_AT {
        Budget::Warn
    } else {
        Budget::Ok
    }
}

/// The counter's side effects, so the allocator runs against a mock in unit tests.
pub trait Io {
    /// The durable high-water mark a directory had: the larger of its `rowid` file and its newest manifest's floor,
    /// 0 for neither.
    fn load(&mut self, dir: &str) -> PgResult<u64>;
    /// Makes `high` durable: `ROWID_RESERVE` (when the table is WAL-logged), the file, its sync request.
    fn persist(&mut self, dir: &str, high: u64) -> PgResult<()>;
    /// A reservation that leaves less than 10 % of the IDs.
    fn warn(&mut self, left: u64) -> PgResult<()>;
    /// The directory's extent table (its `segext` file; empty without one). A file that does not decode is an ERROR.
    fn load_extents(&mut self, dir: &str) -> PgResult<Vec<SegExtent>>;
    /// Makes the extent table `image` durable: `ROWID_EXTENTS` (when the table is WAL-logged), the file, its sync.
    fn persist_extents(&mut self, dir: &str, image: &[u8]) -> PgResult<()>;
    /// Segment `part_no` reached `extents` extents, the soft cap ([`SEG_SOFT_EXTENTS`]): its rotation is late (main
    /// 2026-10-04: a WARNING once per segment, so the hard cap's 54000 never comes unannounced).
    fn warn_wide(&mut self, part_no: u32, extents: u32) -> PgResult<()>;
}

/// A directory's live segment extents (DV-1), as readers see them.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Extents {
    /// By `(part_no, idx)`.
    table: Vec<SegExtent>,
    /// Indices into `table`, by base.
    by_base: Vec<u32>,
    /// The most extents any one segment has.
    widest: u32,
}

impl Extents {
    pub fn new(table: Vec<SegExtent>) -> Extents {
        let mut by_base: Vec<u32> = (0..table.len() as u32).collect();
        by_base.sort_unstable_by_key(|&i| table[i as usize].base);
        let widest = table.iter().map(|e| e.idx + 1).max().unwrap_or(0);
        Extents { table, by_base, widest }
    }

    /// The most extents any one segment has (the soft per-segment cap's check, `rowid::SEG_SOFT_EXTENTS`).
    pub fn widest(&self) -> u32 {
        self.widest
    }

    /// How many segments have at least `n` extents.
    pub fn segments_at(&self, n: u32) -> usize {
        self.table.iter().filter(|e| e.idx + 1 == n).count()
    }

    /// The base of extent `idx` of segment `part_no`, if reserved.
    pub fn base(&self, part_no: u32, idx: u32) -> Option<u64> {
        self.table.binary_search_by_key(&(part_no, idx), |e| (e.part_no, e.idx)).ok().map(|i| self.table[i].base)
    }

    pub fn table(&self) -> &[SegExtent] {
        &self.table
    }

    pub fn len(&self) -> usize {
        self.table.len()
    }

    pub fn is_empty(&self) -> bool {
        self.table.is_empty()
    }

    /// The segment `id` belongs to and its local position there: `(part_no, local)`.
    pub fn seg_of(&self, id: u64) -> Option<(u32, u32)> {
        let at = self.by_base.partition_point(|&i| self.table[i as usize].base <= id).checked_sub(1)?;
        let e = self.table[self.by_base[at] as usize];
        (id - e.base < SEG_EXTENT_IDS).then(|| (e.part_no, (u64::from(e.idx) * SEG_EXTENT_IDS + (id - e.base)) as u32))
    }

    /// The bases of segment `part_no`'s extents, in extent order (`rowid::seg_id`'s `bases`).
    pub fn bases(&self, part_no: u32) -> Vec<u64> {
        let at = self.table.partition_point(|e| e.part_no < part_no);
        self.table[at..].iter().take_while(|e| e.part_no == part_no).map(|e| e.base).collect()
    }

    /// Segment `part_no`'s extent count.
    pub fn count(&self, part_no: u32) -> u32 {
        let at = self.table.partition_point(|e| e.part_no < part_no);
        self.table[at..].iter().take_while(|e| e.part_no == part_no).count() as u32
    }
}

/// Segment `part_no`'s extents as the sealed part's `EXT_RANGES` entry: one range per extent, in extent order (the
/// counter is monotone, so that is ID order; a table whose extents are not is refused).
pub fn extent_ranges(bases: &[u64]) -> Option<pgrc_format::manifest::IdRanges> {
    bases.windows(2).all(|w| w[0] + SEG_EXTENT_IDS <= w[1]).then(|| bases.iter().map(|&b| (b, b + SEG_EXTENT_IDS)).collect())
}

fn too_many_extents(name: &str) -> Box<PgError> {
    Box::new(
        PgError::error(format!("pgrc table \"{name}\" has {SEGEXT_MAX_ENTRIES} live segment extents"))
            .with_sqlstate(types_error::ERRCODE_PROGRAM_LIMIT_EXCEEDED)
            .with_hint("Its segments are waiting to be sealed; retry once they are."),
    )
}

/// D4537 T18c: `charge` fitted to `bytes` on the part cache (opened at the first bytes, closed at 0): the row-ID bookkeeping
/// of a table directory leaves with the directory (`forget`, `forget_under`).
fn fit(charge: &mut memledger::Charge, bytes: usize) {
    const OWNER: memledger::Owner = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::PartCache));
    if charge.owner() != OWNER {
        *charge = memledger::Charge::open(OWNER, memledger::Release::Evict, memledger::Kind::Region, memledger::charge::Door::Heap, 0);
    }
    charge.fit_committed(bytes as u64);
}

/// A tree node's links, at two words.
const NODE: usize = 2 * size_of::<usize>();

/// The counters of every table directory.
#[derive(Debug)]
pub struct Counters {
    /// D4537 T18c: `by_dir`'s bytes, refit when a directory enters or leaves it.
    charge: memledger::Charge,
    by_dir: BTreeMap<String, State>,
}

impl Default for Counters {
    fn default() -> Counters {
        Counters::new()
    }
}

impl Counters {
    fn refit(&mut self) {
        let bytes = self.by_dir.keys().map(|k| size_of::<(String, State)>() + NODE + k.capacity()).sum();
        fit(&mut self.charge, bytes);
    }

    /// `st` entered `dir`'s slot; the bytes are refit when the directory is new.
    fn put(&mut self, dir: &str, st: State) {
        if self.by_dir.insert(dir.to_string(), st).is_none() {
            // alloc: Cold
            self.refit();
        }
    }

    #[cfg(test)]
    pub(crate) fn charged(&self) -> u64 {
        self.charge.committed()
    }
}

/// IX1 L3 (design B, main 2026-10-07): the bulk reservations whose rows may have index entries before a committed
/// publish gives them a home in the newest generation. A bulk writer's rows get IDs from its grants and index entries
/// as it goes; an index cleanup that finds an ID with no home reads this table first ([`in_flight`]) and leaves the
/// ID's entries alone while a reservation holding it is in flight. Each grant registers before its first ID is handed
/// out ([`reserve_in_flight`]). The owner's transaction end drops its entries ([`end_of_xact`], run after the commit
/// is visible: a reader that no longer finds an entry finds its publish in the generation it reads next); PREPARE and
/// a parallel worker's end detach them instead, and a detached entry is in flight while its top-level transaction is
/// in progress. `K` is the owner (the backend's thread; tests use numbers).
#[derive(Debug)]
pub struct Flights<K> {
    /// D4537 T18c: `all`'s bytes on the part cache, refit when entries enter or leave.
    charge: memledger::Charge,
    all: Vec<Flight<K>>,
}

#[derive(Debug, Clone)]
struct Flight<K> {
    dir: String,
    owner: Option<K>,
    xid: types_core::TransactionId,
    lo: u64,
    hi: u64,
}

impl<K: Copy + PartialEq> Flights<K> {
    pub const fn new() -> Flights<K> {
        Flights { all: Vec::new(), charge: memledger::Charge::NONE }
    }

    fn refit(&mut self) {
        let bytes = self.all.iter().map(|f| size_of::<Flight<K>>() + f.dir.capacity()).sum();
        fit(&mut self.charge, bytes);
    }

    /// The bytes charged now.
    #[cfg(test)]
    pub(crate) fn charged(&self) -> u64 {
        self.charge.committed()
    }

    pub fn len(&self) -> usize {
        self.all.len()
    }

    pub fn is_empty(&self) -> bool {
        self.all.is_empty()
    }

    /// `[lo, hi)` of the table in `dir`, granted to `owner`'s transaction `xid`, is in flight.
    pub fn register(&mut self, dir: &str, owner: K, xid: types_core::TransactionId, lo: u64, hi: u64) {
        self.all.push(Flight { dir: dir.to_string(), owner: Some(owner), xid, lo, hi }); // alloc: Cold
        self.refit();
    }

    /// `owner`'s transaction ended: its entries go, or (`detach`) stay ownerless.
    pub fn end(&mut self, owner: K, detach: bool) {
        match detach {
            true => self.all.iter_mut().filter(|f| f.owner == Some(owner)).for_each(|f| f.owner = None),
            false => self.all.retain(|f| f.owner != Some(owner)),
        }
        self.refit();
    }

    /// The ranges of `dir` in flight, sorted by start: every owned entry, and every detached one whose transaction
    /// `in_progress` says still is (the others go).
    pub fn of(&mut self, dir: &str, in_progress: &mut dyn FnMut(types_core::TransactionId) -> PgResult<bool>) -> PgResult<Vec<(u64, u64)>> {
        let mut gone = Vec::new();
        let mut out = Vec::new();
        for (i, f) in self.all.iter().enumerate() {
            let live = f.owner.is_some() || in_progress(f.xid)?;
            if !live {
                gone.push(i);
            } else if f.dir == dir {
                out.push((f.lo, f.hi));
            }
        }
        let any = !gone.is_empty();
        for i in gone.into_iter().rev() {
            self.all.swap_remove(i);
        }
        if any {
            self.refit();
        }
        out.sort_unstable();
        Ok(out)
    }
}

pgsync::process_global! {
    /// IX1 L3: the bulk reservations in flight ([`Flights`]). Taken alone.
    static FLIGHTS: pgsync::Mutex<Flights<std::thread::ThreadId>> = pgsync::Mutex::new(Flights::new());
}

/// [`FLIGHTS`]' length, stored under it: a transaction end and a cleanup skip the mutex while nothing is in flight. A
/// registration's store happens before its first index entry, so a cleanup that saw the entry sees it.
static FLIGHTS_N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pgsync::process_global! {
    /// The extent tables readers see, by directory. Taken alone, or inside [`COUNTERS`] (never the other way round).
    static EXTENTS: pgsync::Mutex<BTreeMap<String, Arc<Extents>>> = pgsync::Mutex::new(BTreeMap::new());
    /// D4537 T18c: the bytes of EXTENTS (the map and the tables it holds; a reader's copy outlives its entry by its own
    /// hold, not counted twice), refit under the map's lock at each change.
    static EXTENTS_CHARGE: pgsync::Mutex<memledger::Charge> = pgsync::Mutex::new(memledger::Charge::NONE);
}

fn extents_bytes(m: &BTreeMap<String, Arc<Extents>>) -> usize {
    m.iter().map(|(d, e)| size_of::<(String, Arc<Extents>)>() + NODE + d.capacity() + 2 * size_of::<usize>() + size_of::<Extents>() + e.table.capacity() * size_of::<SegExtent>() + e.by_base.capacity() * size_of::<u32>()).sum()
}

fn refit_extents(m: &BTreeMap<String, Arc<Extents>>) {
    fit(&mut pgsync::lock(&EXTENTS_CHARGE), extents_bytes(m));
}

/// Bumped under [`EXTENTS`] before any entry changes: a reader holding an [`extents`] copy taken after reading `v` holds
/// the current one while [`extents_version`] still reads `v` (IXDMLPATH: the per-row DML path's cache).
static EXTENTS_VERSION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The extent tables' version (any directory's); read it before taking the copy it vouches for.
pub fn extents_version() -> u64 {
    EXTENTS_VERSION.load(std::sync::atomic::Ordering::Acquire)
}

fn extents_changed() {
    EXTENTS_VERSION.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
}

fn published(dir: &str) -> Option<Arc<Extents>> {
    pgsync::lock(&EXTENTS).get(dir).cloned()
}

fn publish_extents(dir: &str, e: Extents) -> Arc<Extents> {
    let a = Arc::new(e);
    let mut m = pgsync::lock(&EXTENTS);
    extents_changed();
    m.insert(dir.to_string(), Arc::clone(&a)); // alloc: Cold
    refit_extents(&m);
    a
}

fn invalid(msg: String) -> Box<PgError> {
    Box::new(PgError::error(msg).with_sqlstate(types_error::ERRCODE_INVALID_PARAMETER_VALUE))
}

pub fn out_of_ids(name: &str) -> Box<PgError> {
    Box::new(
        PgError::error(format!("pgrc table \"{name}\" is out of row IDs"))
            .with_sqlstate(types_error::ERRCODE_PROGRAM_LIMIT_EXCEEDED)
            .with_hint("VACUUM FULL renumbers the table."),
    )
}

impl Counters {
    pub const fn new() -> Counters {
        Counters { by_dir: BTreeMap::new(), charge: memledger::Charge::NONE }
    }

    fn state(&self, dir: &str, io: &mut dyn Io) -> PgResult<State> {
        Ok(match self.by_dir.get(dir) {
            Some(s) => *s,
            None => {
                let h = io.load(dir)?.max(ROWID_FIRST);
                if h > ROWID_LIMIT {
                    return Err(Box::new(PgError::error(format!("pgrc row-ID mark {h} of \"{dir}\" is past the ID space")).with_sqlstate(types_error::ERRCODE_DATA_CORRUPTED)));
                }
                State { next: h, high: h }
            }
        })
    }

    /// Reserves `align_up(n)` IDs of the table in `dir` (`name` for messages): `1 <= n <= 2^40`.
    pub fn reserve(&mut self, dir: &str, name: &str, n: u64, io: &mut dyn Io) -> PgResult<Reserved> {
        if n == 0 || n > ROWID_LIMIT {
            return Err(invalid(format!("pgrc row-ID reservation of {n} IDs is out of range")));
        }
        let st = self.state(dir, io)?;
        let lo = st.next;
        let hi = lo + align_up(n);
        match budget(hi) {
            Budget::Refuse => return Err(out_of_ids(name)),
            Budget::Warn => io.warn(ROWID_LIMIT - hi)?,
            Budget::Ok => {}
        }
        let mut high = st.high;
        if hi > high {
            let want = hi.saturating_add(PREFETCH).min(ROWID_LIMIT);
            io.persist(dir, want)?;
            high = want;
        }
        self.put(dir, State { next: hi, high });
        Ok(Reserved { lo, hi })
    }

    /// IDBURN: `[from, to)`, the unused tail of a grant, goes back if nothing was handed out after it (`next == to`); the
    /// durable `high` stays. Whether it went back (else it is burned).
    pub fn give_back(&mut self, dir: &str, from: u64, to: u64) -> bool {
        match self.by_dir.get_mut(dir) {
            Some(st) if st.next == to && ROWID_FIRST <= from && from < to && from % IDS_PER_BLOCK == 0 => {
                st.next = from;
                true
            }
            _ => false,
        }
    }

    /// `(next, high)` for `pgrc_rowid_stats`, loading the directory's state as a reservation would.
    pub fn marks(&mut self, dir: &str, io: &mut dyn Io) -> PgResult<(u64, u64)> {
        // IXPERF F3c: a held entry is read in place (a TID scan's begin asks every statement)
        if let Some(st) = self.by_dir.get(dir) {
            return Ok((st.next, st.high));
        }
        let st = self.state(dir, io)?;
        self.put(dir, st);
        Ok((st.next, st.high))
    }

    /// The directory's extent table (loaded once, under this mutex, so a load never reads a file a writer is
    /// rewriting).
    fn extents(&mut self, dir: &str, io: &mut dyn Io) -> PgResult<Arc<Extents>> {
        if let Some(e) = published(dir) {
            return Ok(e);
        }
        Ok(publish_extents(dir, Extents::new(io.load_extents(dir)?)))
    }

    fn store_extents(&mut self, dir: &str, table: Vec<SegExtent>, io: &mut dyn Io) -> PgResult<Arc<Extents>> {
        let image = encode_segext(&table).map_err(|e| Box::new(PgError::error(format!("pgrc segment extent table of \"{dir}\": {e}"))))?;
        io.persist_extents(dir, &image)?;
        Ok(publish_extents(dir, Extents::new(table)))
    }

    /// Extents `0..=idx` of segment `part_no` reserved (DV-1): the missing ones in order, each [`SEG_EXTENT_IDS`] IDs
    /// of the counter, then the table made durable once. The base of extent `idx`. Refused past
    /// [`SEGEXT_MAX_ENTRIES`] entries (the inserter's backpressure waits at the soft cap well before).
    pub fn extend(&mut self, dir: &str, name: &str, part_no: u32, idx: u32, io: &mut dyn Io) -> PgResult<u64> {
        let cur = self.extents(dir, io)?;
        let have = cur.count(part_no);
        if idx < have {
            return Ok(cur.bases(part_no)[idx as usize]);
        }
        if cur.len() + (idx - have) as usize + 1 > SEGEXT_MAX_ENTRIES {
            return Err(too_many_extents(name));
        }
        let mut table = crate::alloc::try_to_vec(cur.table(), "rowid_alloc")?;
        for k in have..=idx {
            let r = self.reserve(dir, name, SEG_EXTENT_IDS, io)?;
            table.push(SegExtent { part_no, idx: k, base: r.lo });
        }
        table.sort_unstable_by_key(|e| (e.part_no, e.idx));
        let base = self.store_extents(dir, table, io)?.bases(part_no)[idx as usize];
        if have < SEG_SOFT_EXTENTS && idx + 1 >= SEG_SOFT_EXTENTS {
            io.warn_wide(part_no, idx + 1)?;
        }
        Ok(base)
    }

    /// The extent table keeping only the segments `keep` names (DV-1's prune, at a slot's activation). Nothing is
    /// written when nothing goes. How many entries went.
    pub fn prune(&mut self, dir: &str, keep: &dyn Fn(u32) -> bool, io: &mut dyn Io) -> PgResult<usize> {
        let cur = self.extents(dir, io)?;
        let table: Vec<SegExtent> = cur.table().iter().filter(|e| keep(e.part_no)).copied().collect();
        let gone = cur.len() - table.len();
        if gone > 0 {
            self.store_extents(dir, table, io)?;
        }
        Ok(gone)
    }

    /// IX0 L4: the counter lowered to `next` (a renumbering VACUUM FULL's reset, made durable by the caller first).
    pub fn set(&mut self, dir: &str, next: u64) {
        self.put(dir, State { next, high: next });
    }

    pub fn forget(&mut self, dir: &str) {
        self.by_dir.remove(dir);
        self.refit();
        let mut m = pgsync::lock(&EXTENTS);
        extents_changed();
        m.remove(dir);
        refit_extents(&m);
    }

    /// Every entry under a removed database directory.
    pub fn forget_under(&mut self, db_path: &str) {
        let p = format!("{db_path}/");
        self.by_dir.retain(|d, _| !d.starts_with(&p));
        self.refit();
        let mut m = pgsync::lock(&EXTENTS);
        extents_changed();
        m.retain(|d, _| !d.starts_with(&p));
        refit_extents(&m);
    }

    pub fn held(&self, dir: &str) -> bool {
        self.by_dir.contains_key(dir)
    }
}

pgsync::process_global! {
    /// Never taken while holding the catalog's mutex, and the catalog's is never taken while holding this one.
    static COUNTERS: pgsync::Mutex<Counters> = pgsync::Mutex::new(Counters::new());
}

/// `dir/rowid`.
pub fn file_path(dir: &str) -> String {
    format!("{dir}/{ROWID_FILE_NAME}")
}

/// The mark the `rowid` file holds; `None` when there is no file.
pub fn read_file(vfs: &mut dyn WriteVfs, dir: &str) -> WriteResult<Option<u64>> {
    match vfs.read_full(&file_path(dir)) {
        Ok(b) => decode_rowid_file(&b).map(Some).map_err(WriteError::Format),
        Err(WriteError::Io { errno: libc::ENOENT, .. }) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Writes `high` into `dir/rowid` in place (created when absent). One 16-byte write. A torn write is repaired by redo,
/// since every mark written after the last checkpoint has its `ROWID_RESERVE` record after that checkpoint's redo
/// pointer, and every earlier one was synced by it.
pub fn write_file(vfs: &mut dyn WriteVfs, dir: &str, high: u64) -> WriteResult<()> {
    let path = file_path(dir);
    let fd = match vfs.open_rw(&path) {
        Err(WriteError::Io { errno: libc::ENOENT, .. }) => vfs.create_rw(&path)?,
        r => r?,
    };
    let w = vfs.pwrite_at(&fd, 0, &encode_rowid_file(high));
    let c = vfs.close_file(fd);
    w.and(c)
}

/// Redo of `ROWID_RESERVE`: the file holds at least `high` (a damaged file is overwritten: WAL is the authority), and
/// the in-memory entry goes, so the next reservation after promotion loads the file.
pub fn redo_reserve(vfs: &mut dyn WriteVfs, dir: &str, high: u64) -> WriteResult<()> {
    let cur = read_file(vfs, dir).unwrap_or(None).unwrap_or(0);
    if high > cur {
        write_file(vfs, dir, high)?;
    }
    forget(dir);
    Ok(())
}

/// Redo of `ROWID_SET` (IX0 L4): the file holds exactly `high` (a renumbering VACUUM FULL lowered it), and the
/// in-memory entry goes.
pub fn redo_set(vfs: &mut dyn WriteVfs, dir: &str, high: u64) -> WriteResult<()> {
    write_file(vfs, dir, high)?;
    forget(dir);
    Ok(())
}

/// `dir/segext`.
pub fn segext_path(dir: &str) -> String {
    format!("{dir}/{SEGEXT_FILE_NAME}")
}

/// The extent table the `segext` file holds; empty when there is no file, an error when it does not decode (a torn
/// write outside recovery: never truncated silently).
pub fn read_segext(vfs: &mut dyn WriteVfs, dir: &str) -> WriteResult<Vec<SegExtent>> {
    match vfs.read_full(&segext_path(dir)) {
        Ok(b) => decode_segext(&b).map_err(WriteError::Format),
        Err(WriteError::Io { errno: libc::ENOENT, .. }) => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// Writes the extent table `image` over `dir/segext` in place (created when absent): one write, never truncated (the
/// header's count says where the table ends). A torn write is repaired by redo, as [`write_file`]'s.
pub fn write_segext(vfs: &mut dyn WriteVfs, dir: &str, image: &[u8]) -> WriteResult<()> {
    let path = segext_path(dir);
    let fd = match vfs.open_rw(&path) {
        Err(WriteError::Io { errno: libc::ENOENT, .. }) => vfs.create_rw(&path)?,
        r => r?,
    };
    let w = vfs.pwrite_at(&fd, 0, image);
    let c = vfs.close_file(fd);
    w.and(c)
}

/// Redo of `ROWID_EXTENTS`: the file holds the record's table (WAL is the authority: a damaged file is overwritten),
/// and the readers of a hot standby see it.
pub fn redo_extents(vfs: &mut dyn WriteVfs, dir: &str, image: &[u8]) -> WriteResult<()> {
    let table = decode_segext(image).map_err(WriteError::Format)?;
    let _serial = pgsync::lock(&COUNTERS);
    write_segext(vfs, dir, image)?;
    publish_extents(dir, Extents::new(table));
    Ok(())
}

/// A table directory was created, emptied or removed: its counter starts over from what the directory holds.
pub fn forget(dir: &str) {
    pgsync::lock(&COUNTERS).forget(dir);
}

pub fn forget_under(db_path: &str) {
    pgsync::lock(&COUNTERS).forget_under(db_path);
    forget_renumbered(|d| d.strip_prefix(db_path).is_some_and(|r| r.starts_with('/')));
}

/// Reserves with `io` in the process-wide counters (the server's path is [`reserve`]).
pub fn reserve_with(dir: &str, name: &str, n: u64, io: &mut dyn Io) -> PgResult<Reserved> {
    Counters::reserve(&mut pgsync::lock(&COUNTERS), dir, name, n, io)
}

/// IDBURN: the unused tails a writer released ([`Counters::give_back`]), latest first.
pub fn give_back_in(dir: &str, tails: &[(u64, u64)]) {
    let mut c = pgsync::lock(&COUNTERS);
    for &(from, to) in tails.iter().rev() {
        c.give_back(dir, from, to);
    }
}

pub fn marks_with(dir: &str, io: &mut dyn Io) -> PgResult<(u64, u64)> {
    pgsync::lock(&COUNTERS).marks(dir, io)
}

/// Whether the process-wide counters hold an entry for `dir` (tests).
pub fn held(dir: &str) -> bool {
    pgsync::lock(&COUNTERS).held(dir)
}

/// `pgrc_rowid_stats`'s text: `next <n> high <h> used <u> left <l> pct <p>`, where used counts every ID handed out
/// or burned (from 0, so a new table shows 291 used), and pct is used / 2^40 in percent to 4 places; then the live
/// segments' extents (DV-1): `extents <entries> widest <most of one segment> wide <segments at the soft cap or past>
/// past64 <segments past 64>`.
pub fn stats_text(next: u64, high: u64, e: &Extents) -> String {
    let pct = next as f64 * 100.0 / ROWID_LIMIT as f64;
    format!(
        "next {next} high {high} used {next} left {} pct {pct:.4} extents {} widest {} wide {} past64 {}",
        ROWID_LIMIT - next,
        e.len(),
        e.widest(),
        e.segments_at(SEG_SOFT_EXTENTS),
        e.segments_at(SEG_MAX_EXTENTS + 1)
    )
}

/// The server's [`Io`] for one relation.
struct RelIo<'a, 'mcx> {
    rel: &'a RelationData<'mcx>,
}

impl Io for RelIo<'_, '_> {
    fn load(&mut self, dir: &str) -> PgResult<u64> {
        let file = read_file(&mut pgrc_write::wvfs::RealVfs, dir).map_err(crate::write_error)?.unwrap_or(0);
        let l = self.rel.rd_locator.get();
        let floor = match crate::catalog::generations((l.spcOid, l.dbOid, l.relNumber as u64)) {
            Some((gens, _)) => match gens.last() {
                Some(g) => pgrc_format::manifest::next_row_id(&g.manifest).map_err(|e| crate::write_error(WriteError::Format(e)))?,
                None => 0,
            },
            None => 0,
        };
        Ok(file.max(floor))
    }

    fn persist(&mut self, dir: &str, high: u64) -> PgResult<()> {
        pgrc_wal_seams::log_rowid_reserve::call(self.rel, dir, high)
    }

    fn load_extents(&mut self, dir: &str) -> PgResult<Vec<SegExtent>> {
        read_segext(&mut pgrc_write::wvfs::RealVfs, dir).map_err(crate::write_error)
    }

    fn persist_extents(&mut self, dir: &str, image: &[u8]) -> PgResult<()> {
        pgrc_wal_seams::log_rowid_extents::call(self.rel, dir, image)
    }

    fn warn_wide(&mut self, part_no: u32, extents: u32) -> PgResult<()> {
        elog::ereport(types_error::WARNING)
            .errmsg(format!("pgrc table \"{}\": segment {part_no} has {extents} extents; its rotation is late", self.rel.name()))
            .errdetail(format!("The table's segments may hold at most {SEGEXT_MAX_ENTRIES} extents together before inserts fail."))
            .errhint("Check that the pgrcolumnar front worker is running and keeping up.")
            .finish(types_error::ErrorLocation::new(file!(), line!() as i32, "rowid_alloc::extend"))
    }

    fn warn(&mut self, left: u64) -> PgResult<()> {
        elog::ereport(types_error::WARNING)
            .errmsg(format!("pgrc table \"{}\" has {left} row IDs left", self.rel.name()))
            .errhint("VACUUM FULL renumbers the table.")
            .finish(types_error::ErrorLocation::new(file!(), line!() as i32, "rowid_alloc::reserve"))
    }
}

/// The table directory of `rel`.
pub fn dir_of(rel: &RelationData<'_>) -> String {
    crate::dirpath::table_dir_path(rel.rd_locator.get(), rel.rd_backend)
}

/// Reserves `align_up(n)` row IDs of `rel` (`1 <= n <= 2^40`): durable before this returns, never handed out again.
pub fn reserve(rel: &RelationData<'_>, n: u64) -> PgResult<Reserved> {
    reserve_with(&dir_of(rel), rel.name(), n, &mut RelIo { rel })
}

/// IX1 L3 (design B, main 2026-10-07): [`reserve`] for a bulk writer's grant, registered in flight before any of its
/// IDs is handed out ([`Flights`]).
pub fn reserve_in_flight(rel: &RelationData<'_>, n: u64) -> PgResult<Reserved> {
    let xid = xact::GetTopTransactionId()?;
    let r = reserve(rel, n)?;
    let mut f = pgsync::lock(&FLIGHTS);
    f.register(&dir_of(rel), std::thread::current().id(), xid, r.lo, r.hi);
    FLIGHTS_N.store(f.len(), std::sync::atomic::Ordering::Release);
    Ok(r)
}

/// IX1 L3: this backend's transaction ended (`pgrc_xact_callback`, after its commit is visible): its in-flight entries
/// go, or (`detach`: PREPARE, a parallel worker's end) stay ownerless, in flight while their transaction is.
pub fn end_of_xact(detach: bool) {
    if FLIGHTS_N.load(std::sync::atomic::Ordering::Acquire) == 0 {
        return;
    }
    let mut f = pgsync::lock(&FLIGHTS);
    f.end(std::thread::current().id(), detach);
    FLIGHTS_N.store(f.len(), std::sync::atomic::Ordering::Release);
}

/// IX1 L3: the reservations of the table in `dir` in flight now, sorted by start (an index cleanup reads this before
/// the newest generation).
pub fn in_flight(dir: &str) -> PgResult<Vec<(u64, u64)>> {
    if FLIGHTS_N.load(std::sync::atomic::Ordering::Acquire) == 0 {
        return Ok(Vec::new());
    }
    let mut f = pgsync::lock(&FLIGHTS);
    let out = f.of(dir, &mut |xid| procarray::TransactionIdIsInProgress(xid));
    FLIGHTS_N.store(f.len(), std::sync::atomic::Ordering::Release);
    out
}

/// IDBURN: the unused tails `rel`'s bulk writer released at its cuts go back to the counter where nothing came after.
pub fn give_back(rel: &RelationData<'_>, tails: &[(u64, u64)]) {
    if !tails.is_empty() {
        give_back_in(&dir_of(rel), tails);
    }
}

/// `(next, high)` of `rel`'s counter.
pub fn marks(rel: &RelationData<'_>) -> PgResult<(u64, u64)> {
    marks_with(&dir_of(rel), &mut RelIo { rel })
}

/// [`marks`] of `rel`, whose table directory `dir` is (IXPERF F3c: a caller that holds it builds no path).
pub fn marks_in(rel: &RelationData<'_>, dir: &str) -> PgResult<(u64, u64)> {
    marks_with(dir, &mut RelIo { rel })
}

/// Extents `0..=idx` of `rel`'s segment `part_no` reserved (DV-1); the base of extent `idx`.
pub fn extend(rel: &RelationData<'_>, part_no: u32, idx: u32) -> PgResult<u64> {
    extend_with(&dir_of(rel), rel.name(), part_no, idx, &mut RelIo { rel })
}

pub fn extend_with(dir: &str, name: &str, part_no: u32, idx: u32, io: &mut dyn Io) -> PgResult<u64> {
    if let Some(b) = published(dir).and_then(|e| e.base(part_no, idx)) {
        return Ok(b);
    }
    pgsync::lock(&COUNTERS).extend(dir, name, part_no, idx, io)
}

/// `rel`'s extent table keeping only the segments `keep` names; the entries pruned.
pub fn prune(rel: &RelationData<'_>, keep: &dyn Fn(u32) -> bool) -> PgResult<usize> {
    prune_with(&dir_of(rel), keep, &mut RelIo { rel })
}

pub fn prune_with(dir: &str, keep: &dyn Fn(u32) -> bool, io: &mut dyn Io) -> PgResult<usize> {
    pgsync::lock(&COUNTERS).prune(dir, keep, io)
}

/// The extent table of the table in `dir` as readers see it (loaded from its file on the first ask of this process).
pub fn extents(dir: &str) -> PgResult<Arc<Extents>> {
    if let Some(e) = published(dir) {
        return Ok(e);
    }
    let mut io = FileIo;
    pgsync::lock(&COUNTERS).extents(dir, &mut io)
}

/// IX0 L4 (OC-4 C): `rel`'s counter lowered to `next` after a renumbering VACUUM FULL's last publish committed, under
/// its AccessExclusiveLock (no reservation runs; nothing at or above `next` is in use): `ROWID_SET` and the file first,
/// then the in-memory entry. The extent table is pruned to the segments `keep` names, and `next` rises to the end of
/// any extent left (a sealed segment an older snapshot still reads). The mark set.
pub fn reset(rel: &RelationData<'_>, next: u64, keep: &dyn Fn(u32) -> bool) -> PgResult<u64> {
    let dir = dir_of(rel);
    let mut io = RelIo { rel };
    let mut c = pgsync::lock(&COUNTERS);
    c.prune(&dir, keep, &mut io)?;
    let next = c.extents(&dir, &mut io)?.table().iter().map(|e| e.base + SEG_EXTENT_IDS).fold(align_up(next.max(ROWID_FIRST)), u64::max);
    pgrc_wal_seams::log_rowid_set::call(rel, &dir, next)?;
    c.set(&dir, next);
    Ok(next)
}

/// IX0 L4: the lowest 291-aligned ID at or above `cursor` where `w` IDs overlap none of `taken` (ranges sorted by
/// start, pairwise disjoint).
pub fn renumber_place(cursor: u64, w: u64, taken: &[(u64, u64)]) -> u64 {
    let mut c = align_up(cursor.max(ROWID_FIRST));
    for &(lo, hi) in taken {
        if hi <= c {
            continue;
        }
        if lo >= c + w {
            break;
        }
        c = align_up(hi);
    }
    c
}

/// One step of a renumber ([`renumber_next`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenumberStep {
    /// Rewrite the part, its survivors taking IDs from `base`.
    Rewrite { part_no: u32, base: u64 },
    /// The part is dense where the cursor stands: keep it; the cursor moves to `hi`.
    Keep { part_no: u32, hi: u64 },
}

/// Whether a part of `rows` rows homing `ids` is dense: one range, `align_up(rows)` wide.
fn dense(rows: u64, ids: &[(u64, u64)]) -> bool {
    matches!(ids, [(lo, hi)] if hi - lo == align_up(rows))
}

/// IX0 L4: the renumber's next step at `cursor` over a generation's parts (`(part_no, rows, ranges)`): the part with the
/// lowest IDs (one without IDs first) that is not dense below the cursor, kept if it is dense where it would go, else
/// rewritten at the lowest place at or above the cursor that no other part's ranges touch. `None`: every part is dense
/// below the cursor. Each step leaves one more part dense below the cursor, so `parts.len()` steps finish.
pub fn renumber_next(cursor: u64, parts: &[(u32, u64, &[(u64, u64)])]) -> Option<RenumberStep> {
    let done = |rows: u64, ids: &[(u64, u64)]| rows == 0 || (dense(rows, ids) && ids[0].1 <= cursor);
    let (part_no, rows, ids) = *parts.iter().filter(|p| !done(p.1, p.2)).min_by_key(|p| (p.2.first().map_or(0, |r| r.0), p.0))?;
    let mut taken: Vec<(u64, u64)> = parts.iter().filter(|p| p.0 != part_no).flat_map(|p| p.2.iter().copied()).collect();
    taken.sort_unstable();
    let base = renumber_place(cursor, align_up(rows), &taken);
    Some(match dense(rows, ids) && ids[0].0 == base {
        true => RenumberStep::Keep { part_no, hi: ids[0].1 },
        false => RenumberStep::Rewrite { part_no, base },
    })
}

/// The last VACUUM FULL renumber of a table, for `pgrc_rowid_stats` (process memory: a restart forgets it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Renumbered {
    /// It ran: the parts it rewrote.
    Done { rewritten: u64 },
    /// It was due but skipped: deleted rows still visible to an older snapshot (their tombstones unfolded).
    Skipped { visible: u64 },
}

pgsync::process_global! {
    static RENUMBERED: pgsync::Mutex<BTreeMap<String, Renumbered>> = pgsync::Mutex::new(BTreeMap::new());
    /// D4537 T18c: the bytes of RENUMBERED, refit when a directory enters it.
    static RENUMBERED_CHARGE: pgsync::Mutex<memledger::Charge> = pgsync::Mutex::new(memledger::Charge::NONE);
}

fn renumbered_bytes(m: &BTreeMap<String, Renumbered>) -> usize {
    m.keys().map(|d| size_of::<(String, Renumbered)>() + NODE + d.capacity()).sum()
}

pub fn note_renumber(rel: &RelationData<'_>, r: Renumbered) {
    let mut m = pgsync::lock(&RENUMBERED);
    m.insert(dir_of(rel), r); // alloc: Cold
    fit(&mut pgsync::lock(&RENUMBERED_CHARGE), renumbered_bytes(&m));
}

/// D4742: the notes of directories that went (`dirpath::remove_table_dir`, DROP DATABASE's [`forget_under`]).
pub fn forget_renumbered(gone: impl Fn(&str) -> bool) {
    let mut m = pgsync::lock(&RENUMBERED);
    let n = m.len();
    m.retain(|d, _| !gone(d));
    if m.len() != n {
        fit(&mut pgsync::lock(&RENUMBERED_CHARGE), renumbered_bytes(&m));
    }
}

/// `renumber <none|done|skipped> rewritten <parts> visible <deleted rows an older snapshot still sees>`.
pub fn renumber_text(rel: &RelationData<'_>) -> String {
    match pgsync::lock(&RENUMBERED).get(&dir_of(rel)) {
        None => "renumber none rewritten 0 visible 0".to_string(),
        Some(Renumbered::Done { rewritten }) => format!("renumber done rewritten {rewritten} visible 0"),
        Some(Renumbered::Skipped { visible }) => format!("renumber skipped rewritten 0 visible {visible}"),
    }
}

/// The reader's [`Io`]: it only loads.
struct FileIo;

impl Io for FileIo {
    fn load(&mut self, _dir: &str) -> PgResult<u64> {
        Err(Box::new(PgError::error("pgrc row-ID counter: a reader's load")))
    }
    fn persist(&mut self, _dir: &str, _high: u64) -> PgResult<()> {
        Err(Box::new(PgError::error("pgrc row-ID counter: a reader's persist")))
    }
    fn warn(&mut self, _left: u64) -> PgResult<()> {
        Ok(())
    }
    fn load_extents(&mut self, dir: &str) -> PgResult<Vec<SegExtent>> {
        read_segext(&mut pgrc_write::wvfs::RealVfs, dir).map_err(crate::write_error)
    }
    fn persist_extents(&mut self, _dir: &str, _image: &[u8]) -> PgResult<()> {
        Err(Box::new(PgError::error("pgrc segment extents: a reader's persist")))
    }
    fn warn_wide(&mut self, _part_no: u32, _extents: u32) -> PgResult<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap as Map;

    #[test]
    fn removed_directories_take_their_renumber_notes() {
        let note = |d: &str| (d.to_string(), Renumbered::Done { rewritten: 1 });
        let (kept, sibling) = ("/d4742/base/1/pgrc_5", "/d4742/base/20/pgrc_5");
        pgsync::lock(&RENUMBERED).extend([note("/d4742/base/2/pgrc_5"), note("/d4742/base/2/pgrc_6"), note(kept), note(sibling)]);
        forget_renumbered(|d| d == "/d4742/base/2/pgrc_6");
        assert!(!pgsync::lock(&RENUMBERED).contains_key("/d4742/base/2/pgrc_6"));
        forget_under("/d4742/base/2");
        let m = pgsync::lock(&RENUMBERED);
        assert!(!m.keys().any(|d| d.starts_with("/d4742/base/2/")));
        assert!(m.contains_key(kept) && m.contains_key(sibling), "another database's notes stay");
    }

    #[test]
    fn a_reservation_is_in_flight_until_its_owner_ends_and_a_detached_one_while_its_xid_runs() {
        let mut f: Flights<u32> = Flights::new();
        let never = &mut |_: types_core::TransactionId| -> PgResult<bool> { panic!("an owned entry asks no xid") };
        f.register("t1", 1, 700, 291, 582);
        f.register("t1", 2, 701, 1164, 1455);
        f.register("t2", 1, 700, 873, 1164);
        f.register("t1", 1, 700, 0, 291);
        assert_eq!(f.of("t1", never).unwrap(), vec![(0, 291), (291, 582), (1164, 1455)]);
        assert_eq!(f.of("t2", never).unwrap(), vec![(873, 1164)]);
        // owner 1 commits: its entries of both tables go
        f.end(1, false);
        assert_eq!(f.of("t1", never).unwrap(), vec![(1164, 1455)]);
        assert!(f.of("t2", never).unwrap().is_empty());
        // owner 2 prepares: its entry stays while xid 701 is in progress, then goes
        f.end(2, true);
        let mut running = |x: types_core::TransactionId| -> PgResult<bool> { Ok(x == 701) };
        assert_eq!(f.of("t1", &mut running).unwrap(), vec![(1164, 1455)]);
        // owner 2's next transaction ends: the detached entry is not its
        f.end(2, false);
        assert_eq!(f.len(), 1);
        let mut ended = |_: types_core::TransactionId| -> PgResult<bool> { Ok(false) };
        assert!(f.of("t1", &mut ended).unwrap().is_empty());
        assert!(f.is_empty());
    }

    /// A mock [`Io`]: files by directory, the persisted marks in order, the warnings.
    #[derive(Default)]
    struct Mock {
        files: Map<String, u64>,
        floor: Map<String, u64>,
        persisted: Vec<(String, u64)>,
        warned: Vec<u64>,
        segext: Map<String, Vec<u8>>,
        extents_logged: usize,
        wide: Vec<(u32, u32)>,
    }

    impl Io for Mock {
        fn load(&mut self, dir: &str) -> PgResult<u64> {
            Ok(self.files.get(dir).copied().unwrap_or(0).max(self.floor.get(dir).copied().unwrap_or(0)))
        }
        fn persist(&mut self, dir: &str, high: u64) -> PgResult<()> {
            self.persisted.push((dir.to_string(), high));
            self.files.insert(dir.to_string(), high);
            Ok(())
        }
        fn warn(&mut self, left: u64) -> PgResult<()> {
            self.warned.push(left);
            Ok(())
        }
        fn load_extents(&mut self, dir: &str) -> PgResult<Vec<SegExtent>> {
            Ok(self.segext.get(dir).map(|b| decode_segext(b).unwrap()).unwrap_or_default())
        }
        fn persist_extents(&mut self, dir: &str, image: &[u8]) -> PgResult<()> {
            self.extents_logged += 1;
            self.segext.insert(dir.to_string(), image.to_vec());
            Ok(())
        }
        fn warn_wide(&mut self, part_no: u32, extents: u32) -> PgResult<()> {
            self.wide.push((part_no, extents));
            Ok(())
        }
    }

    #[test]
    fn a_new_table_starts_at_291_and_persists_before_handing_out() {
        let (mut c, mut io) = (Counters::new(), Mock::default());
        let r = c.reserve("d", "t", 1, &mut io).unwrap();
        assert_eq!(r, Reserved { lo: ROWID_FIRST, hi: ROWID_FIRST + 291 });
        assert_eq!(io.persisted, vec![("d".to_string(), r.hi + PREFETCH)], "one prefetch batch past the end");
        let r2 = c.reserve("d", "t", 292, &mut io).unwrap();
        assert_eq!(r2, Reserved { lo: r.hi, hi: r.hi + 582 }, "aligned, contiguous");
        assert_eq!(io.persisted.len(), 1, "inside the batch: no new WAL");
        let big = c.reserve("d", "t", 2 * PREFETCH, &mut io).unwrap();
        assert_eq!(big.lo, r2.hi);
        assert_eq!(io.persisted.last(), Some(&("d".to_string(), big.hi + PREFETCH)), "a request past the batch persists its own end plus a batch");
        for w in [r, r2, big] {
            assert!(w.lo % 291 == 0 && w.hi % 291 == 0 && w.lo < w.hi);
        }
    }

    /// IDBURN (c): a reservation past the durable mark persists its own end plus a batch, so a grant that ends past the
    /// mark always leaves a batch of slack above it.
    #[test]
    fn idburn_a_reservation_past_the_mark_persists_its_end_plus_a_batch() {
        let (mut c, mut io) = (Counters::new(), Mock::default());
        let r = c.reserve("d", "t", 1, &mut io).unwrap();
        assert_eq!(io.persisted, vec![("d".to_string(), r.hi + PREFETCH)]);
        let big = c.reserve("d", "t", 4 * PREFETCH, &mut io).unwrap();
        assert_eq!(io.persisted.last(), Some(&("d".to_string(), big.hi + PREFETCH)), "a grant larger than a batch");
        let n = io.persisted.len();
        c.reserve("d", "t", PREFETCH - 291, &mut io).unwrap();
        assert_eq!(io.persisted.len(), n, "inside the slack: no new WAL");
    }

    /// IDBURN (b): a tail goes back only while nothing was handed out after it; `high` stays; a given-back counter hands
    /// out from the tail's start without persisting again; after a restart the counter starts at `high`, so a tail
    /// given back in memory is burned, never an ID used before it.
    #[test]
    fn idburn_a_tail_goes_back_only_while_next_is_its_end() {
        let (mut c, mut io) = (Counters::new(), Mock::default());
        let a = c.reserve("d", "t", 1164, &mut io).unwrap();
        let high = io.files["d"];
        assert!(c.give_back("d", a.lo + 291, a.hi), "nothing came after it");
        assert_eq!(c.marks("d", &mut io).unwrap(), (a.lo + 291, high), "next back at the used end; high unchanged");
        let b = c.reserve("d", "t", 1164, &mut io).unwrap();
        assert_eq!(b.lo, a.lo + 291, "the tail is handed out again");
        let other = c.reserve("d", "t", 291, &mut io).unwrap();
        assert!(!c.give_back("d", b.lo + 291, b.hi), "another grant came after: the tail is burned");
        assert_eq!(c.marks("d", &mut io).unwrap().0, other.hi);
        assert!(!c.give_back("d", other.lo + 1, other.hi), "a tail starts on a block");
        assert!(!c.give_back("e", 291, 582), "no counter held: nothing to give back to");
        assert!(!c.held("e"));
        assert_eq!(io.persisted.len(), 1, "every grant inside the first batch");
        let mut after = Counters::new();
        assert_eq!(after.reserve("d", "t", 1, &mut io).unwrap().lo, high, "a restart starts at high");
    }

    #[test]
    fn a_restart_starts_at_the_durable_mark_and_burns_the_rest() {
        let (mut c, mut io) = (Counters::new(), Mock::default());
        let r = c.reserve("d", "t", 10, &mut io).unwrap();
        let high = io.files["d"];
        assert!(r.hi < high);
        let mut after = Counters::new();
        let r2 = after.reserve("d", "t", 10, &mut io).unwrap();
        assert_eq!(r2.lo, high, "IDs between the handed-out end and the mark are never handed out again");
        io.floor.insert("e".into(), 10 * 291);
        assert_eq!(after.reserve("e", "t", 1, &mut io).unwrap().lo, 10 * 291, "the manifest floor counts");
        io.files.insert("f".into(), 5 * 291);
        io.floor.insert("f".into(), 3 * 291);
        assert_eq!(after.reserve("f", "t", 1, &mut io).unwrap().lo, 5 * 291, "the larger of file and floor");
    }

    #[test]
    fn requests_out_of_range_are_rejected_and_change_nothing() {
        let (mut c, mut io) = (Counters::new(), Mock::default());
        for n in [0, ROWID_LIMIT + 1, u64::MAX] {
            let e = c.reserve("d", "t", n, &mut io).unwrap_err();
            assert_eq!(e.sqlstate(), types_error::ERRCODE_INVALID_PARAMETER_VALUE, "{n}");
        }
        assert!(io.persisted.is_empty() && !c.held("d"));
    }

    #[test]
    fn the_budget_warns_under_10_percent_and_refuses_under_1_percent() {
        assert_eq!(budget(WARN_AT), Budget::Ok);
        assert_eq!(budget(WARN_AT + 1), Budget::Warn);
        assert_eq!(budget(REFUSE_AT), Budget::Warn);
        assert_eq!(budget(REFUSE_AT + 1), Budget::Refuse);
        let (mut c, mut io) = (Counters::new(), Mock::default());
        io.files.insert("d".into(), align_up(WARN_AT));
        let r = c.reserve("d", "t", 1, &mut io).unwrap();
        assert_eq!(io.warned, vec![ROWID_LIMIT - r.hi], "a WARNING per reservation past 90 %");
        // Burn to just under the refusal point, then one more is refused and changes nothing.
        let to_edge = REFUSE_AT / 291 * 291 - r.hi;
        let edge = c.reserve("d", "t", to_edge, &mut io).unwrap();
        assert!(edge.hi <= REFUSE_AT);
        let before = io.persisted.len();
        let e = c.reserve("d", "t", 292, &mut io).unwrap_err();
        assert_eq!(e.sqlstate(), types_error::ERRCODE_PROGRAM_LIMIT_EXCEEDED);
        assert!(e.message().contains("out of row IDs"), "{}", e.message());
        assert_eq!(io.persisted.len(), before);
        assert_eq!(c.reserve("d", "t", 1, &mut io).unwrap_err().sqlstate(), types_error::ERRCODE_PROGRAM_LIMIT_EXCEEDED, "refused, not advanced");
        assert!(io.files["d"] <= ROWID_LIMIT, "the prefetch never passes the ID space");
    }

    #[test]
    fn a_recreated_directory_never_inherits_an_old_mark() {
        // main 2026-10-04: a reused relfilenumber whose old key survived would hand out IDs its new file and WAL never
        // reserved. Creating the directory forgets the key: the next reservation starts at 291 and persists.
        let mut io = Mock::default();
        let dir = "t/ix0-recreate-db/16384";
        let r = reserve_with(dir, "t", 1000, &mut io).unwrap();
        assert!(held(dir) && r.lo == ROWID_FIRST);
        // DROP removed the files; the key is left behind (simulated). CREATE of the same relfilenumber runs the hook.
        io.files.remove(dir);
        forget(dir);
        assert!(!held(dir));
        let n = io.persisted.len();
        let again = reserve_with(dir, "t", 1, &mut io).unwrap();
        assert_eq!(again.lo, ROWID_FIRST);
        assert_eq!(io.persisted.len(), n + 1, "the new directory's first reservation is persisted (WAL + file)");
        assert_eq!(io.files[dir], again.hi + PREFETCH);
        forget_under("t/ix0-recreate-db");
        assert!(!held(dir), "a removed database forgets its tables");
    }

    #[test]
    fn concurrent_reservations_never_overlap() {
        let mut io0 = Mock::default();
        let dir = "t/ix0-concurrent";
        reserve_with(dir, "t", 1, &mut io0).unwrap();
        let got: Vec<Reserved> = std::thread::scope(|s| {
            let hs: Vec<_> = (0..8u64)
                .map(|t| {
                    s.spawn(move || {
                        let mut io = Mock::default();
                        io.files.insert(dir.to_string(), ROWID_LIMIT / 2);
                        (0..500u64).map(|i| reserve_with(dir, "t", 1 + (t * 7 + i) % 900, &mut io).unwrap()).collect::<Vec<_>>()
                    })
                })
                .collect();
            hs.into_iter().flat_map(|h| h.join().unwrap()).collect()
        });
        let mut v = got.clone();
        v.sort_by_key(|r| r.lo);
        assert_eq!(v.len(), 8 * 500);
        for w in v.windows(2) {
            assert!(w[0].hi <= w[1].lo, "{:?} overlaps {:?}", w[0], w[1]);
        }
        assert!(v.iter().all(|r| r.lo % 291 == 0 && r.hi % 291 == 0));
        forget(dir);
    }

    /// DV-1: extents come in order from the counter, once each (a second ask is the same base), the table is made
    /// durable once per change, a block that skips extents reserves the ones between, and a fresh process loads the
    /// table from the file.
    #[test]
    fn extents_reserve_in_order_once_and_reload() {
        let (mut c, mut io) = (Counters::new(), Mock::default());
        let dir = "t/ix0-ext-order";
        let b0 = c.extend(dir, "t", 7, 0, &mut io).unwrap();
        assert_eq!(b0, ROWID_FIRST);
        assert_eq!(c.extend(dir, "t", 7, 0, &mut io).unwrap(), b0, "idempotent");
        assert_eq!(io.extents_logged, 1);
        let mid = c.reserve(dir, "t", 1, &mut io).unwrap();
        let b2 = c.extend(dir, "t", 7, 2, &mut io).unwrap();
        assert_eq!(io.extents_logged, 2, "two extents, one durable table");
        let e = extents(dir).unwrap_or_else(|_| published(dir).unwrap());
        assert_eq!(e.bases(7), vec![b0, mid.hi, mid.hi + SEG_EXTENT_IDS]);
        assert_eq!(b2, mid.hi + SEG_EXTENT_IDS);
        assert_eq!(e.seg_of(b0 + 5), Some((7, 5)));
        assert_eq!(e.seg_of(b2 + 1), Some((7, 2 * SEG_EXTENT_IDS as u32 + 1)));
        assert_eq!(e.seg_of(mid.lo), None, "an ID of another reservation is no segment's");
        assert_eq!(e.seg_of(b2 + SEG_EXTENT_IDS), None);
        assert_eq!(extent_ranges(&e.bases(7)).unwrap()[1], (mid.hi, mid.hi + SEG_EXTENT_IDS));
        let b9 = c.extend(dir, "t", 9, 0, &mut io).unwrap();
        // a new process: the counter and the table come back from the files
        forget(dir);
        let mut again = Counters::new();
        assert_eq!(again.extents(dir, &mut io).unwrap().bases(9), vec![b9]);
        assert!(again.extend(dir, "t", 9, 1, &mut io).unwrap() >= b9 + SEG_EXTENT_IDS, "never below a reserved extent");
        forget(dir);
    }

    /// IXDMLPATH: every change a reader's copy could miss moves the version (an extent added, a prune that drops one, a
    /// forget, a first load); an ask that changes nothing leaves the published copy as it was.
    #[test]
    fn the_extents_version_moves_on_every_change() {
        let (mut c, mut io) = (Counters::new(), Mock::default());
        let dir = "t/ixdmlpath-version";
        let moved = |f: &mut dyn FnMut()| {
            let v = extents_version();
            f();
            extents_version() > v
        };
        assert!(moved(&mut || drop(c.extend(dir, "t", 3, 0, &mut io).unwrap())), "the first extent");
        let before = published(dir).unwrap();
        assert!(moved(&mut || drop(c.extend(dir, "t", 3, 1, &mut io).unwrap())), "a new extent");
        let after = published(dir).unwrap();
        assert!(!Arc::ptr_eq(&before, &after) && after.count(3) == 2);
        c.extend(dir, "t", 3, 1, &mut io).unwrap();
        assert!(Arc::ptr_eq(&after, &published(dir).unwrap()), "an extent already reserved publishes nothing");
        assert!(moved(&mut || drop(c.prune(dir, &|p| p != 3, &mut io).unwrap())), "a prune that drops entries");
        assert!(moved(&mut || forget(dir)), "a forgotten directory");
        assert!(moved(&mut || drop(extents(dir).unwrap())), "a load from the file");
        forget(dir);
    }

    /// DV-1 (c): pruning keeps the entry count at the live bound however many segments rotate through.
    #[test]
    fn pruning_keeps_the_table_at_the_live_bound() {
        let (mut c, mut io) = (Counters::new(), Mock::default());
        let dir = "t/ix0-ext-prune";
        for part_no in 0..2_000u32 {
            for k in 0..3 {
                c.extend(dir, "t", part_no, k, &mut io).unwrap();
            }
            // the ring holds the last four segments; activating the next prunes the rest
            let keep = |p: u32| p + 4 > part_no;
            c.prune(dir, &keep, &mut io).unwrap();
            assert!(published(dir).unwrap().len() <= 4 * 3, "{part_no}");
        }
        let n = io.extents_logged;
        assert_eq!(c.prune(dir, &|_| true, &mut io).unwrap(), 0);
        assert_eq!(io.extents_logged, n, "nothing pruned: nothing written");
        forget(dir);
    }

    /// The hard cap refuses with 54000 and changes nothing; a prune makes room.
    #[test]
    fn the_extent_table_refuses_past_its_hard_cap() {
        let (mut c, mut io) = (Counters::new(), Mock::default());
        let dir = "t/ix0-ext-cap";
        for part_no in 0..(SEGEXT_MAX_ENTRIES as u32 / 64) {
            c.extend(dir, "t", part_no, 63, &mut io).unwrap();
        }
        let full = published(dir).unwrap().len();
        assert_eq!(full, SEGEXT_MAX_ENTRIES / 64 * 64);
        let left = SEGEXT_MAX_ENTRIES - full;
        if left > 0 {
            c.extend(dir, "t", 100_000, left as u32 - 1, &mut io).unwrap();
        }
        let n = io.extents_logged;
        let e = c.extend(dir, "t", 100_001, 0, &mut io).unwrap_err();
        assert_eq!(e.sqlstate(), types_error::ERRCODE_PROGRAM_LIMIT_EXCEEDED);
        assert_eq!(io.extents_logged, n, "refused, nothing written");
        c.prune(dir, &|p| p != 0, &mut io).unwrap();
        c.extend(dir, "t", 100_001, 0, &mut io).unwrap();
        forget(dir);
    }

    #[test]
    fn the_segext_file_round_trips_and_redo_repairs_it() {
        let mut vfs = pgrc_write::wvfs::MemVfs::new();
        vfs.mkdir_path("s").unwrap();
        assert_eq!(read_segext(&mut vfs, "s").unwrap(), vec![]);
        let long: Vec<SegExtent> = (0..5).map(|k| SegExtent { part_no: 1, idx: k, base: ROWID_FIRST + u64::from(k) * SEG_EXTENT_IDS }).collect();
        write_segext(&mut vfs, "s", &encode_segext(&long).unwrap()).unwrap();
        assert_eq!(read_segext(&mut vfs, "s").unwrap(), long);
        let short = long[..2].to_vec();
        write_segext(&mut vfs, "s", &encode_segext(&short).unwrap()).unwrap();
        assert_eq!(read_segext(&mut vfs, "s").unwrap(), short, "a shorter table over a longer file");
        let fd = vfs.open_rw("s/segext").unwrap();
        vfs.pwrite_at(&fd, 14, &[0xee]).unwrap();
        vfs.close_file(fd).unwrap();
        assert!(read_segext(&mut vfs, "s").is_err(), "a torn live region is an error, never a truncation");
        redo_extents(&mut vfs, "s", &encode_segext(&short).unwrap()).unwrap();
        assert_eq!(read_segext(&mut vfs, "s").unwrap(), short, "redo repairs it from the record");
        assert_eq!(published("s").unwrap().bases(1), vec![long[0].base, long[1].base], "and a hot standby's readers see it");
        forget("s");
    }

    #[test]
    fn stats_read_used_left_and_percent() {
        let none = Extents::default();
        assert_eq!(stats_text(291, 291 + PREFETCH, &none), format!("next 291 high {} used 291 left {} pct 0.0000 extents 0 widest 0 wide 0 past64 0", 291 + PREFETCH, ROWID_LIMIT - 291));
        assert!(stats_text(ROWID_LIMIT / 2, ROWID_LIMIT / 2, &none).contains("pct 50.0000 "));
        assert!(stats_text(ROWID_LIMIT, ROWID_LIMIT, &none).contains("left 0 pct 100.0000"));
        let ext = |part_no: u32, n: u32| (0..n).map(move |idx| SegExtent { part_no, idx, base: 0 });
        let e = Extents::new(ext(1, 3).chain(ext(2, SEG_SOFT_EXTENTS)).chain(ext(3, SEG_MAX_EXTENTS + 2)).collect());
        assert!(stats_text(291, 291, &e).ends_with(&format!("extents {} widest 66 wide 2 past64 1", 3 + 63 + 66)), "{}", stats_text(291, 291, &e));
    }

    #[test]
    fn the_file_round_trips_in_place_and_redo_only_raises_it() {
        let mut vfs = pgrc_write::wvfs::MemVfs::new();
        vfs.mkdir_path("t").unwrap();
        assert_eq!(read_file(&mut vfs, "t").unwrap(), None);
        write_file(&mut vfs, "t", 582).unwrap();
        assert_eq!(read_file(&mut vfs, "t").unwrap(), Some(582));
        write_file(&mut vfs, "t", 873).unwrap();
        assert_eq!(read_file(&mut vfs, "t").unwrap(), Some(873), "rewritten in place");
        redo_reserve(&mut vfs, "t", 291).unwrap();
        assert_eq!(read_file(&mut vfs, "t").unwrap(), Some(873), "redo never lowers the mark");
        redo_reserve(&mut vfs, "t", 1 << 20).unwrap();
        assert_eq!(read_file(&mut vfs, "t").unwrap(), Some(1 << 20));
        let fd = vfs.open_rw("t/rowid").unwrap();
        vfs.pwrite_at(&fd, 0, &[0xff; 4]).unwrap();
        vfs.close_file(fd).unwrap();
        assert!(read_file(&mut vfs, "t").is_err(), "a damaged file is an error");
        redo_reserve(&mut vfs, "t", 1 << 21).unwrap();
        assert_eq!(read_file(&mut vfs, "t").unwrap(), Some(1 << 21), "redo repairs a damaged file from WAL");
    }

    /// IX0 L4: the placement skips every range it would touch, and lands on 291-aligned IDs at or above the cursor.
    #[test]
    fn renumber_place_takes_the_lowest_clear_aligned_spot() {
        const B: u64 = IDS_PER_BLOCK;
        assert_eq!(renumber_place(0, 2 * B, &[]), ROWID_FIRST, "never below ROWID_FIRST");
        assert_eq!(renumber_place(B + 5, B, &[]), 2 * B, "aligned up");
        let taken = [(2 * B, 3 * B), (5 * B, 6 * B), (100 * B, 200 * B)];
        assert_eq!(renumber_place(B, B, &taken), B, "fits before the first range");
        assert_eq!(renumber_place(B, 2 * B, &taken), 3 * B, "touches [2B, 3B): moved past it, and [3B, 5B) is clear");
        assert_eq!(renumber_place(B, 3 * B, &taken), 6 * B, "too wide for the gap before 5B");
        assert_eq!(renumber_place(B, 95 * B, &taken), 200 * B, "too wide for [6B, 100B)");
        assert_eq!(renumber_place(150 * B, B, &taken), 200 * B, "inside a range: past it");
        assert_eq!(renumber_place(300 * B, B, &taken), 300 * B, "past every range");
    }

    /// IX0 L4: a renumber walks the parts from the lowest IDs (a part without IDs first), rewrites each to the lowest
    /// clear spot at the cursor, keeps one already dense there, and ends with every part dense below the cursor; a
    /// second walk over its result keeps everything (a second VACUUM FULL rewrites nothing).
    #[test]
    fn renumber_next_densifies_and_a_second_walk_keeps_everything() {
        const B: u64 = IDS_PER_BLOCK;
        let ext = SEG_EXTENT_IDS;
        // a sealed segment's part (sparse: 2 extents for 700 rows), a bulk part, a part without IDs, an empty part
        let mut parts: Vec<(u32, u64, Vec<(u64, u64)>)> = vec![
            (3, 700, vec![(ROWID_FIRST + ext, ROWID_FIRST + 2 * ext), (ROWID_FIRST + 5 * ext, ROWID_FIRST + 6 * ext)]),
            (7, 300, vec![(ROWID_FIRST + 3 * ext, ROWID_FIRST + 3 * ext + 2 * B)]),
            (9, 50, vec![]),
            (11, 0, vec![]),
        ];
        fn view(p: &[(u32, u64, Vec<(u64, u64)>)]) -> Vec<(u32, u64, &[(u64, u64)])> {
            p.iter().map(|(n, r, i)| (*n, *r, &i[..])).collect()
        }
        let (mut cursor, mut rewritten, mut steps) = (ROWID_FIRST, Vec::new(), 0);
        loop {
            let Some(step) = renumber_next(cursor, &view(&parts)) else { break };
            steps += 1;
            assert!(steps <= parts.len(), "one step per part at most");
            match step {
                RenumberStep::Rewrite { part_no, base } => {
                    let p = parts.iter_mut().find(|p| p.0 == part_no).unwrap();
                    assert!(base % B == 0 && base >= cursor);
                    p.2 = vec![(base, base + align_up(p.1))];
                    cursor = base + align_up(p.1);
                    rewritten.push(part_no);
                }
                RenumberStep::Keep { hi, .. } => cursor = hi,
            }
            let mut all: Vec<(u64, u64)> = parts.iter().flat_map(|p| p.2.iter().copied()).collect();
            all.sort_unstable();
            assert!(all.windows(2).all(|w| w[0].1 <= w[1].0), "one home per ID after every step: {all:?}");
        }
        assert_eq!(rewritten, vec![9, 3, 7], "the part without IDs first, then by lowest ID");
        assert_eq!(cursor, ROWID_FIRST + B + align_up(700) + align_up(300), "dense: 50 + 700 + 300 rows, each aligned");
        assert!(renumber_next(cursor, &view(&parts)).is_none());
        assert!(renumber_next(ROWID_FIRST, &view(&parts)).is_some_and(|s| matches!(s, RenumberStep::Keep { .. })), "a second walk keeps");
        let (mut c2, mut kept) = (ROWID_FIRST, 0);
        loop {
            let Some(step) = renumber_next(c2, &view(&parts)) else { break };
            match step {
                RenumberStep::Keep { hi, .. } => (c2, kept) = (hi, kept + 1),
                RenumberStep::Rewrite { part_no, .. } => panic!("a second walk rewrote part {part_no}"),
            }
        }
        assert_eq!((c2, kept), (cursor, 3));
    }

    /// IX0 L4: a counter lowered by a renumber hands out from there; redo of `ROWID_SET` writes the file exactly.
    #[test]
    fn a_reset_counter_hands_out_from_its_new_mark() {
        let (mut c, mut io) = (Counters::new(), Mock::default());
        c.reserve("d", "t", REFUSE_AT - ROWID_FIRST - 2 * SEG_EXTENT_IDS, &mut io).unwrap();
        assert!(c.reserve("d", "t", 4 * SEG_EXTENT_IDS, &mut io).is_err(), "refused near the end");
        c.set("d", 10 * IDS_PER_BLOCK);
        let r = c.reserve("d", "t", 1, &mut io).unwrap();
        assert_eq!(r.lo, 10 * IDS_PER_BLOCK, "from the lowered mark");
        assert_eq!(io.persisted.last(), Some(&("d".to_string(), r.hi + PREFETCH)), "the mark set is the durable one: the next batch persists");
        let mut vfs = pgrc_write::wvfs::MemVfs::new();
        vfs.mkdir_path("t").unwrap();
        write_file(&mut vfs, "t", 1 << 30).unwrap();
        redo_set(&mut vfs, "t", 582).unwrap();
        assert_eq!(read_file(&mut vfs, "t").unwrap(), Some(582), "ROWID_SET lowers the file");
    }

    const PART_CACHE: memledger::Owner = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::PartCache));

    /// D4537 T18c: the per-directory counters are the part cache's bytes, refit when a directory enters and when it leaves.
    #[test]
    fn the_counters_are_on_the_part_cache() {
        let (mut c, mut io) = (Counters::new(), Mock::default());
        assert_eq!(c.charged(), 0);
        c.reserve("t/t18c-counters-a", "t", 1, &mut io).unwrap();
        let one = c.charged();
        assert!(one > 0, "a directory's state is counted");
        c.reserve("t/t18c-counters-a", "t", 1, &mut io).unwrap();
        assert_eq!(c.charged(), one, "the same directory again adds nothing");
        c.reserve("t/t18c-counters-b", "t", 1, &mut io).unwrap();
        assert!(c.charged() > one);
        assert_eq!(c.charge.owner(), PART_CACHE);
        c.forget("t/t18c-counters-a");
        c.forget("t/t18c-counters-b");
        assert_eq!(c.charged(), 0, "forgotten directories give their bytes back");
    }

    /// D4537 T18c: the in-flight reservations are the part cache's bytes while they are in flight.
    #[test]
    fn the_flights_are_on_the_part_cache() {
        let mut f: Flights<u32> = Flights::new();
        assert_eq!(f.charged(), 0);
        f.register("t/t18c-flights", 1, 7, 10, 20);
        let one = f.charged();
        assert!(one > 0);
        f.register("t/t18c-flights", 1, 8, 30, 40);
        assert!(f.charged() > one);
        f.end(1, false);
        assert_eq!(f.charged(), 0, "a transaction's end gives its reservations' bytes back");
    }

    /// D4537 T18c: the extent tables (and the renumber notes) are the part cache's bytes, refit under their maps' locks.
    #[test]
    fn the_extent_tables_are_on_the_part_cache() {
        let dir = "t/t18c-extents";
        publish_extents(dir, Extents::new((0..512u32).map(|i| SegExtent { part_no: i, idx: 0, base: 1_000 + u64::from(i) }).collect()));
        {
            let m = pgsync::lock(&EXTENTS);
            let c = pgsync::lock(&EXTENTS_CHARGE);
            assert_eq!((c.owner(), c.committed()), (PART_CACHE, extents_bytes(&m) as u64));
            assert!(c.committed() >= 512 * size_of::<SegExtent>() as u64);
        }
        forget(dir);
        let m = pgsync::lock(&EXTENTS);
        assert_eq!(pgsync::lock(&EXTENTS_CHARGE).committed(), extents_bytes(&m) as u64, "the forgotten table's bytes went");
    }
}
