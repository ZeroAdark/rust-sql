//! The process's row maps (ROWMAP2 M0, pgrc-plans/ROWMAP2.md): each open part has one [`MapCell`], shared with
//! every generation that carries the part, holding its decoded `RowIdentMap` once something has asked for it. The
//! cells together hold at most `pgrust.pgrc_rowmap_cache_size` bytes (default 64 MB; read once per process), kept by a
//! CLOCK ring of their own (`pgrc_read::mem::Clock`, the second-chance approximation of LRU the decoded-column cache
//! uses):
//!
//! - **Hit**: the cell's own mutex, an `Arc` clone and a relaxed store of its reference bit; no process-wide lock, no
//!   setting read.
//! - **Miss**: the map loads outside every lock; then, under the ring, the hand evicts other cells' maps until the new
//!   one fits, and the new one is admitted (always: a map larger than the budget is held alone, so a per-row locate never
//!   reloads per row). Lock order: ring, then cell (a visit locks the cell it visits).
//! - **Eviction** takes only the cell's `Arc`: a scan or locate holding the map keeps it until it lets go. A dropped
//!   part's cell is dead to the ring, and its bytes leave the total with it.
//!
//! [`stats_text`] is `pgrc_rowid_stats`'s report: the process's loads, evictions, held bytes, budget and peak, and per
//! form one table's parts, rows, map bytes held and on disk.
//!
//! ROWMAP2 M1: a FRAMEIX map (`pgrc_format::rowmap`) builds its reverse on its first ID → ordinal lookup; the cell
//! holding it is charged the reverse's bytes then ([`MapCell::grew`]), and the build's count, rows and time are
//! counted for the report (`map_rev_builds`, `map_rev_rows`, `map_rev_build_ns`).
//!
//! SORTRUNS-E0 L1: a sorted run's reverse pages (`crate::runcache`) share the budget and the ring: each page is
//! admitted as a map is ([`admit_other`]) and is a ring slot of its own (`run_page_reads`, `run_page_evictions`).

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::ffi::CString;
use std::sync::{Arc, Weak};

use pgrc_format::manifest::{PartRecord, PART_FLAG_ROWMAP, PART_ROWMAP_FORM_MASK, PART_ROWMAP_FORM_SHIFT, PART_ROWMAP_GRANULES};
use pgrc_format::rowmap::RevForm;
use pgrc_read::mem::{Clock, Sweep, Victim};
use types_error::PgResult;

use crate::catalog::{Generation, GranuleMap, RowMap};

/// The budget's setting (PG memory units; a bare number is kB) and its default.
const BUDGET_GUC: &str = "pgrust.pgrc_rowmap_cache_size";
pub const DEFAULT_BUDGET: u64 = 64 << 20;
const UNREAD: u64 = u64::MAX;

/// The cells' shared state: their ring, the bytes they hold and its budget, and the counters.
pub struct Cache {
    ring: pgsync::Mutex<Clock>,
    cached: AtomicU64,
    peak: AtomicU64,
    loads: AtomicU64,
    evictions: AtomicU64,
    budget: AtomicU64,
    rev_builds: AtomicU64,
    rev_rows: AtomicU64,
    rev_ns: AtomicU64,
    run_pages: AtomicU64,
    run_evictions: AtomicU64,
}

impl Cache {
    const fn new() -> Cache {
        Cache {
            ring: pgsync::Mutex::new(Clock::new()),
            cached: AtomicU64::new(0),
            peak: AtomicU64::new(0),
            loads: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
            budget: AtomicU64::new(UNREAD),
            rev_builds: AtomicU64::new(0),
            rev_rows: AtomicU64::new(0),
            rev_ns: AtomicU64::new(0),
            run_pages: AtomicU64::new(0),
            run_evictions: AtomicU64::new(0),
        }
    }

    /// The budget in bytes: the setting, read on the first ask of the process (a restart-only setting, like
    /// `pgrust.pgrc_cache_size`: the cache is process-wide).
    fn budget(&self) -> u64 {
        match self.budget.load(Ordering::Relaxed) {
            UNREAD => {
                let b = budget_setting();
                self.budget.store(b, Ordering::Relaxed);
                b
            }
            b => b,
        }
    }
}

pgsync::process_global! {
    /// The process's row-map cache.
    static CACHE: Cache = Cache::new();
}

/// A FRAMEIX reverse was built over `rows` rows in `ns` nanoseconds (the report's counters).
pub fn rev_built(ns: u64, rows: u64) {
    CACHE.rev_builds.fetch_add(1, Ordering::Relaxed);
    CACHE.rev_rows.fetch_add(rows, Ordering::Relaxed);
    CACHE.rev_ns.fetch_add(ns, Ordering::Relaxed);
}

/// SORTRUNS-E0 L1: `bytes` of a run reverse's page admitted under the budget as a map is: the ring swept for room
/// (an empty ring has nothing to give), then `insert` (the caller's cell takes the page under its own lock; false when
/// a racing read put it first) and, when it did, a ring slot for `victim`. The bytes stay counted until the caller
/// gives them back ([`release_other`]: its slot evicted the page, or its cell dropped).
pub(crate) fn admit_other(bytes: u64, insert: impl FnOnce() -> bool, victim: impl FnOnce() -> Box<dyn Victim>) {
    let (cache, budget) = (&CACHE, CACHE.budget());
    let mut ring = pgsync::lock(&cache.ring);
    if ring.slots() > 0 {
        ring.sweep(&|| cache.cached.load(Ordering::Relaxed).saturating_add(bytes) > budget);
    }
    if insert() {
        let now = cache.cached.fetch_add(bytes, Ordering::Relaxed) + bytes;
        cache.peak.fetch_max(now, Ordering::Relaxed);
        cache.run_pages.fetch_add(1, Ordering::Relaxed);
        ring.register(victim());
    }
}

/// SORTRUNS-E0 L1: bytes [`admit_other`] counted leave the total.
pub(crate) fn release_other(bytes: u64) {
    CACHE.cached.fetch_sub(bytes, Ordering::Relaxed);
}

/// SORTRUNS-E0 L1: a run reverse's page was evicted.
pub(crate) fn count_eviction() {
    CACHE.run_evictions.fetch_add(1, Ordering::Relaxed);
}

fn budget_setting() -> u64 {
    let seam = guc_seams::get_config_option_missing_ok::is_installed();
    let set = seam.then(|| guc_seams::get_config_option_missing_ok::call(BUDGET_GUC).ok().flatten()).flatten();
    set.and_then(|v| parse_bytes(&v)).unwrap_or(DEFAULT_BUDGET)
}

/// A memory setting in PG's units (B, kB, MB, GB, TB, as PG spells them); a bare number is kB (work_mem's base unit).
fn parse_bytes(v: &str) -> Option<u64> {
    let v = v.trim();
    let split = v.find(|c: char| !c.is_ascii_digit()).unwrap_or(v.len());
    let n: u64 = v[..split].parse().ok()?;
    let unit = match v[split..].trim() {
        "B" => 1,
        "" | "kB" => 1 << 10,
        "MB" => 1 << 20,
        "GB" => 1 << 30,
        "TB" => 1 << 40,
        _ => return None,
    };
    n.checked_mul(unit).filter(|&b| b != UNREAD)
}

const CHARGE_OWNER: memledger::Owner = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::PartCache));

/// A map a cell holds; its bytes count in its cache's total while it is held.
struct Held {
    /// D4537 T18c: the map's bytes on the part cache, for as long as the cell holds it (dropped with the `Held`).
    _charge: memledger::Charge,
    map: Arc<RowMap>,
    bytes: u64,
    cache: &'static Cache,
}

impl Held {
    fn new(cache: &'static Cache, map: Arc<RowMap>, bytes: u64) -> Held {
        let now = cache.cached.fetch_add(bytes, Ordering::Relaxed) + bytes;
        cache.peak.fetch_max(now, Ordering::Relaxed);
        let charge = memledger::Charge::open(CHARGE_OWNER, memledger::Release::Evict, memledger::Kind::Region, memledger::charge::Door::Heap, bytes);
        Held { map, bytes, cache, _charge: charge }
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        self.cache.cached.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

/// One open part's map slot and its CLOCK reference bit.
#[derive(Default)]
pub struct MapCell {
    slot: pgsync::Mutex<Option<Held>>,
    referenced: AtomicBool,
}

impl MapCell {
    /// The part's map: the held one, else `load`'s, admitted to the process's cache (a racing load's map is dropped).
    pub fn get_or_load(self: &Arc<Self>, load: impl FnOnce() -> PgResult<RowMap>) -> PgResult<Arc<RowMap>> {
        self.get_or_load_in(&CACHE, load)
    }

    fn get_or_load_in(self: &Arc<Self>, cache: &'static Cache, load: impl FnOnce() -> PgResult<RowMap>) -> PgResult<Arc<RowMap>> {
        if let Some(h) = pgsync::lock(&self.slot).as_ref() {
            self.referenced.store(true, Ordering::Relaxed);
            return Ok(Arc::clone(&h.map));
        }
        let m = Arc::new(load()?);
        cache.loads.fetch_add(1, Ordering::Relaxed);
        let (bytes, budget) = (m.mem_bytes(), cache.budget());
        let mut ring = pgsync::lock(&cache.ring);
        // Room for the new map among the others (an empty ring has nothing to give: no sweep, so no futility backoff)
        if ring.slots() > 0 {
            ring.sweep(&|| cache.cached.load(Ordering::Relaxed).saturating_add(bytes) > budget);
        }
        let mut slot = pgsync::lock(&self.slot);
        if let Some(h) = slot.as_ref() {
            return Ok(Arc::clone(&h.map));
        }
        *slot = Some(Held::new(cache, Arc::clone(&m), bytes));
        if let RowMap::Granules(g) = &*m {
            g.set_owner(Arc::downgrade(self));
        }
        self.referenced.store(true, Ordering::Relaxed);
        drop(slot);
        ring.register(Box::new(Ring { cell: Arc::downgrade(self), cache }));
        Ok(m)
    }

    /// The map `g` grew by `extra` bytes (its reverse, built after admission): charged while this cell still holds it
    /// (an evicted map's holder keeps an uncounted copy, as with any evicted map), then the ring swept back within
    /// the budget as an admission does (this cell's bit is set, so the others go first).
    pub(crate) fn grew(&self, g: &GranuleMap, extra: u64) {
        let cache = {
            let mut slot = pgsync::lock(&self.slot);
            let Some(h) = slot.as_mut() else { return };
            if !matches!(&*h.map, RowMap::Granules(x) if std::ptr::eq(&**x, g)) {
                return;
            }
            h.bytes += extra;
            h.cache.cached.fetch_add(extra, Ordering::Relaxed);
            h.cache
        };
        // the ring's lock before any cell's (the admission order): this cell's slot is released above
        let budget = cache.budget();
        let mut ring = pgsync::lock(&cache.ring);
        if ring.slots() > 0 {
            ring.sweep(&|| cache.cached.load(Ordering::Relaxed) > budget);
        }
        drop(ring);
        cache.peak.fetch_max(cache.cached.load(Ordering::Relaxed), Ordering::Relaxed);
    }

    /// The bytes of the map the cell holds (0 when it holds none).
    pub fn resident(&self) -> u64 {
        pgsync::lock(&self.slot).as_ref().map_or(0, |h| h.bytes)
    }
}

/// A cell's ring slot.
struct Ring {
    cell: Weak<MapCell>,
    cache: &'static Cache,
}

impl Victim for Ring {
    fn visit(&self) -> Sweep {
        let Some(cell) = self.cell.upgrade() else { return Sweep::Dead };
        let mut slot = pgsync::lock(&cell.slot);
        if slot.is_none() {
            return Sweep::Dead;
        }
        if cell.referenced.swap(false, Ordering::Relaxed) {
            return Sweep::Referenced;
        }
        let bytes = slot.take().map_or(0, |h| h.bytes);
        self.cache.evictions.fetch_add(1, Ordering::Relaxed);
        Sweep::Evicted(bytes)
    }

    fn dead(&self) -> bool {
        self.cell.upgrade().is_none_or(|c| pgsync::lock(&c.slot).is_none())
    }
}

/// A part's map form as its manifest record and directory say (what a load would do): implicit, a ROWMAP2 granules
/// map's elected kind (`mono`) or reverse (`fences`, `labels`, `frameix`), or (ROWMAP2 M2) a form retired with M1
/// (sorted, runs, pairs: the load refuses it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapForm {
    Implicit,
    Retired,
    Granules(RevForm),
}

const FORMS: [(MapForm, &str); 6] = [
    (MapForm::Implicit, "implicit"),
    (MapForm::Granules(RevForm::Mono), "mono"),
    (MapForm::Granules(RevForm::Fences), "fences"),
    (MapForm::Granules(RevForm::Labels), "labels"),
    (MapForm::Granules(RevForm::FrameIx), "frameix"),
    // SORTRUNS-E0: a sorted run's member (its IDs are found through the run's reverse)
    (MapForm::Granules(RevForm::Run), "run"),
];

/// Part `rec`'s map form, its sidecar's bytes and (granules form) its granules per form: the granules form bits; any
/// other flagged form, or an unflagged sidecar (a seal before ROWMAP2 M1), is retired; without either the map is
/// implicit; `None` = a part with no row IDs (written before IX0), or a granules map whose sidecar does not read.
fn disk_form(dir: &str, rec: &PartRecord, ids: &[(u64, u64)]) -> Option<(MapForm, u64, [u32; 4])> {
    let path = format!("{dir}/{}", pgrc_format::dirlayout::sidecar_file_name(rec.part_no, pgrc_format::sidecar::SidecarKind::RowIdentMap, 1));
    let mut info = vfs::FileInfo::zeroed();
    let disk = CString::new(path).ok().filter(|c| vfs::stat(c, &mut info) == 0).map(|_| info.size.max(0) as u64);
    let mut grans = [0u32; 4];
    let form = match (rec.flags & PART_ROWMAP_FORM_MASK) >> PART_ROWMAP_FORM_SHIFT {
        PART_ROWMAP_GRANULES => {
            let dirio = pgrc_read::VfsTableDir::new(dir.to_string()); // alloc: Cold
            let read = pgrc_read::sidecar::read_sidecar(&dirio, rec.part_no, pgrc_format::sidecar::SidecarKind::RowIdentMap, 1, None, None).ok()?;
            let pgrc_read::sidecar::SidecarConsult::Hit { payload, .. } = read else { return None };
            let (rev, g) = pgrc_format::rowmap::summary(&payload).ok()?;
            grans = g;
            MapForm::Granules(rev)
        }
        _ if rec.flags & PART_FLAG_ROWMAP != 0 || disk.is_some() => MapForm::Retired,
        _ if ids.is_empty() => return None,
        _ => MapForm::Implicit,
    };
    Some((form, disk.unwrap_or(0), grans))
}

/// `pgrc_rowid_stats`'s map fields: `map_loads <n> map_evictions <n> map_cache <bytes> map_budget <bytes> map_peak
/// <bytes>` for the process (loads and evictions since start; the bytes the cells hold now, their budget, and the most
/// they have held), `noid_parts <n>` (parts of `gen` with no row IDs), `retired_parts <n>` (ROWMAP2 M2: parts of `gen`
/// whose map is in a form retired with M1), then per form (implicit, and ROWMAP2's mono, fences, labels, frameix)
/// (SORTRUNS-E0: and `run`, a sorted run's member) `<form>_parts _rows _mem _disk _mem_bpr _disk_bpr`: `gen`'s parts of
/// that form, their rows, their maps' bytes held now and on disk, and those per row to 4 places; then (ROWMAP2)
/// `gran_dense gran_runs gran_strided gran_delta` (`gen`'s granules maps' granules per form) and `map_rev_builds
/// map_rev_rows map_rev_build_ns` (the process's FRAMEIX reverse builds, their rows and time); SORTRUNS-E0:
/// `run_page_reads run_page_evictions` (the process's run reverse pages read into the cache and evicted from it), and
/// `sorted_runs homing_runs` (`gen`'s sorted runs, and those that home their IDs).
pub fn stats_text(gen: Option<&Generation>, dir: &str) -> String {
    let mut by = [(0u64, 0u64, 0u64, 0u64); FORMS.len()];
    let mut grans = [0u64; 4];
    let (mut noid, mut retired) = (0u64, 0u64);
    if let Some(g) = gen {
        for (i, rec) in g.manifest.parts.iter().enumerate() {
            let Some((form, disk, gf)) = disk_form(dir, rec, g.ids_of(i)) else {
                noid += 1;
                continue;
            };
            if form == MapForm::Retired {
                retired += 1;
                continue;
            }
            grans.iter_mut().zip(gf).for_each(|(a, b)| *a += u64::from(b));
            let f = &mut by[FORMS.iter().position(|(m, _)| *m == form).expect("every form is listed")];
            f.0 += 1;
            f.1 += rec.rows;
            f.2 += g.parts.get(i).map_or(0, |p| p.row_map.resident());
            f.3 += disk;
        }
    }
    let per_row = |b: u64, rows: u64| if rows == 0 { 0.0 } else { b as f64 / rows as f64 };
    let c = &CACHE;
    let mut out = format!(
        "map_loads {} map_evictions {} map_cache {} map_budget {} map_peak {} noid_parts {noid} retired_parts {retired}",
        c.loads.load(Ordering::Relaxed),
        c.evictions.load(Ordering::Relaxed),
        c.cached.load(Ordering::Relaxed),
        c.budget(),
        c.peak.load(Ordering::Relaxed)
    );
    for ((_, name), (parts, rows, mem, disk)) in FORMS.iter().zip(by) {
        out.push_str(&format!(
            " {name}_parts {parts} {name}_rows {rows} {name}_mem {mem} {name}_disk {disk} {name}_mem_bpr {:.4} {name}_disk_bpr {:.4}",
            per_row(mem, rows),
            per_row(disk, rows)
        ));
    }
    out.push_str(&format!(
        " gran_dense {} gran_runs {} gran_strided {} gran_delta {} map_rev_builds {} map_rev_rows {} map_rev_build_ns {} run_page_reads {} run_page_evictions {}",
        grans[pgrc_format::rowmap::G_DENSE as usize],
        grans[pgrc_format::rowmap::G_RUNS as usize],
        grans[pgrc_format::rowmap::G_STRIDED as usize],
        grans[pgrc_format::rowmap::G_DELTA as usize],
        c.rev_builds.load(Ordering::Relaxed),
        c.rev_rows.load(Ordering::Relaxed),
        c.rev_ns.load(Ordering::Relaxed),
        c.run_pages.load(Ordering::Relaxed),
        c.run_evictions.load(Ordering::Relaxed)
    ));
    let runs = gen.map_or(&[][..], |g| g.runs());
    out.push_str(&format!(" sorted_runs {} homing_runs {}", runs.len(), runs.iter().filter(|h| h.run.homes()).count()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cache of its own (the process's is shared with every other test): `budget` bytes.
    fn cache(budget: u64) -> &'static Cache {
        let c: &'static Cache = Box::leak(Box::new(Cache::new()));
        c.budget.store(budget, Ordering::Relaxed);
        c
    }

    /// A LABELS map of `n` IDs, descending from `2^20 + n - 1`: its bytes grow with `n`.
    fn held(n: u32) -> RowMap {
        let ids: Vec<u64> = (0..u64::from(n)).rev().map(|i| (1 << 20) + i).collect();
        let body = pgrc_format::rowmap::encode(&ids, pgrc_format::rowmap::Opts { force: Some(RevForm::Labels) }).unwrap();
        RowMap::Granules(Arc::new(GranuleMap::new(pgrc_format::rowmap::GranMap::decode(&body, u64::from(n)).unwrap())))
    }

    fn cells(n: usize) -> Vec<Arc<MapCell>> {
        (0..n).map(|_| Arc::new(MapCell::default())).collect()
    }

    #[test]
    fn a_cell_loads_once_and_counts_what_it_holds() {
        let c = cache(1 << 20);
        let cell = Arc::new(MapCell::default());
        assert_eq!(cell.resident(), 0);
        let a = cell.get_or_load_in(c, || Ok(held(1000))).unwrap();
        let b = cell.get_or_load_in(c, || panic!("a held map is not loaded again")).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        let n = a.mem_bytes();
        assert!(n > 0, "a labels map holds bytes");
        assert_eq!((cell.resident(), c.cached.load(Ordering::Relaxed), c.peak.load(Ordering::Relaxed), c.loads.load(Ordering::Relaxed)), (n, n, n, 1));
        let other = Arc::new(MapCell::default());
        assert!(other.get_or_load_in(c, || Err(Box::new(types_error::PgError::error("no")))).is_err());
        assert_eq!((other.resident(), c.loads.load(Ordering::Relaxed)), (0, 1), "a failed load holds and counts nothing");
        drop(cell);
        assert_eq!(c.cached.load(Ordering::Relaxed), 0, "a dropped cell's bytes leave the total");
    }

    /// D4537 T18c: a map a cell holds is the part cache's bytes while it is held, and gone with it.
    #[test]
    fn a_held_map_is_on_the_part_cache() {
        let c = cache(1 << 20);
        let cell = Arc::new(MapCell::default());
        let m = cell.get_or_load_in(c, || Ok(held(1000))).unwrap();
        let n = m.mem_bytes();
        {
            let slot = pgsync::lock(&cell.slot);
            let h = slot.as_ref().expect("held");
            assert_eq!((h._charge.owner(), h._charge.committed()), (CHARGE_OWNER, n));
        }
    }

    #[test]
    fn an_implicit_map_holds_no_bytes() {
        let c = cache(0);
        let cell = Arc::new(MapCell::default());
        assert_eq!(cell.get_or_load_in(c, || Ok(RowMap::Implicit(582))).unwrap().ident(2), 584);
        assert_eq!(cell.resident(), 0);
    }

    #[test]
    fn the_cells_stay_within_the_budget_and_evicted_maps_reload() {
        let n = held(1000).mem_bytes();
        let budget = 2 * n + n / 2;
        let c = cache(budget);
        let k = cells(5);
        let first = k[0].get_or_load_in(c, || Ok(held(1000))).unwrap();
        for cell in &k[1..] {
            cell.get_or_load_in(c, || Ok(held(1000))).unwrap();
            assert!(c.cached.load(Ordering::Relaxed) <= budget);
        }
        assert_eq!(c.cached.load(Ordering::Relaxed), 2 * n, "two {n}-byte maps fit");
        assert_eq!(c.evictions.load(Ordering::Relaxed), 3);
        assert!(c.peak.load(Ordering::Relaxed) <= budget, "room is made before the new map is admitted");
        assert_eq!(k[4].resident(), n, "the newest is held");
        assert_eq!(k[0].resident(), 0, "the oldest went first");
        assert_eq!(first.ident(999), 1 << 20, "a holder keeps its map past the eviction");
        let mut reloaded = false;
        k[0].get_or_load_in(c, || {
            reloaded = true;
            Ok(held(1000))
        })
        .unwrap();
        assert!(reloaded && k[0].resident() == n && c.cached.load(Ordering::Relaxed) <= budget);
    }

    #[test]
    fn a_map_over_the_budget_is_admitted_alone() {
        let c = cache(0);
        let k = cells(2);
        k[0].get_or_load_in(c, || Ok(held(10))).unwrap();
        k[1].get_or_load_in(c, || Ok(held(10))).unwrap();
        assert_eq!((k[0].resident(), k[1].resident()), (0, held(10).mem_bytes()), "budget 0 keeps only the latest");
        k[1].get_or_load_in(c, || panic!("held: no reload")).unwrap();
    }

    #[test]
    fn a_hit_buys_one_more_turn_and_a_dropped_cell_is_dead() {
        let c = cache(1 << 20);
        let cell = Arc::new(MapCell::default());
        cell.get_or_load_in(c, || Ok(held(10))).unwrap();
        let slot = Ring { cell: Arc::downgrade(&cell), cache: c };
        assert_eq!(slot.visit(), Sweep::Referenced, "admitted with its bit set");
        assert_eq!(slot.visit(), Sweep::Evicted(held(10).mem_bytes()));
        assert!(slot.dead(), "an evicted cell's slot is dead");
        cell.get_or_load_in(c, || Ok(held(10))).unwrap();
        let slot = Ring { cell: Arc::downgrade(&cell), cache: c };
        cell.referenced.store(false, Ordering::Relaxed);
        cell.get_or_load_in(c, || panic!("held")).unwrap();
        assert_eq!(slot.visit(), Sweep::Referenced, "a hit sets the bit");
        drop(cell);
        assert_eq!(slot.visit(), Sweep::Dead);
        assert!(slot.dead());
    }

    /// A FRAMEIX map (ROWMAP2 M1) of a permutation of `n` IDs: its reverse is built on the first ID lookup.
    fn frameix(n: u64, seed: u64) -> RowMap {
        let mut ids: Vec<u64> = (0..n).map(|i| 291 + i * 3).collect();
        let mut x = seed | 1;
        for i in (1..ids.len()).rev() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            ids.swap(i, (x % (i as u64 + 1)) as usize);
        }
        let body = pgrc_format::rowmap::encode(&ids, pgrc_format::rowmap::Opts { force: Some(RevForm::FrameIx) }).unwrap();
        RowMap::Granules(Arc::new(GranuleMap::new(pgrc_format::rowmap::GranMap::decode(&body, n).unwrap())))
    }

    #[test]
    fn a_frameix_reverse_is_charged_to_its_cell_and_kept_within_the_budget() {
        let c = cache(1 << 30);
        let cell = Arc::new(MapCell::default());
        let a = cell.get_or_load_in(c, || Ok(frameix(20_000, 7))).unwrap();
        let fwd = cell.resident();
        assert_eq!(fwd, a.mem_bytes(), "admitted at its forward bytes");
        assert_eq!(a.ordinal_of(20_000, a.ident(17)), Some(17));
        let grown = cell.resident();
        assert!(grown > fwd, "the reverse is charged: {fwd} -> {grown}");
        assert_eq!((grown, c.cached.load(Ordering::Relaxed), c.peak.load(Ordering::Relaxed)), (a.mem_bytes(), grown, grown));
        assert_eq!(a.ordinal_of(20_000, a.ident(18)), Some(18));
        assert_eq!(cell.resident(), grown, "built and charged once");
        // two such maps fit forward-only, not once one has its reverse: building it evicts one of them
        let budget = 2 * fwd + (grown - fwd) / 2;
        let c = cache(budget);
        let k = cells(2);
        k[0].get_or_load_in(c, || Ok(frameix(20_000, 7))).unwrap();
        let b = k[1].get_or_load_in(c, || Ok(frameix(20_000, 7))).unwrap();
        assert_eq!((c.cached.load(Ordering::Relaxed), c.evictions.load(Ordering::Relaxed)), (2 * fwd, 0));
        assert_eq!(b.ordinal_of(20_000, b.ident(5)), Some(5));
        let (cached, peak) = (c.cached.load(Ordering::Relaxed), c.peak.load(Ordering::Relaxed));
        assert!(cached <= budget && peak <= budget && c.evictions.load(Ordering::Relaxed) == 1, "budget {budget}: cached {cached} peak {peak}");
    }

    #[test]
    fn the_budget_reads_pg_memory_units() {
        assert_eq!(parse_bytes("64MB"), Some(64 << 20));
        assert_eq!(parse_bytes(" 256kB "), Some(256 << 10));
        assert_eq!(parse_bytes("1024"), Some(1 << 20), "a bare number is kB");
        assert_eq!(parse_bytes("2GB"), Some(2 << 30));
        assert_eq!(parse_bytes("0"), Some(0));
        assert_eq!(parse_bytes("12B"), Some(12));
        assert_eq!(parse_bytes("64mb"), None);
        assert_eq!(parse_bytes("lots"), None);
        assert_eq!(parse_bytes("99999999999999TB"), None);
    }
}
