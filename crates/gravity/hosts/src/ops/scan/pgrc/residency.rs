//! Resident parts per pgrc relation (docs/gravity/hosts.md `residency.rs`): the per-generation part lists over the columnar
//! catalog's open parts (`resident_parts`, `LISTS`), and this backend's relcache callback.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use types_core::Oid;
use types_error::PgResult;
use types_rel::Relation;
use types_snapshot::SnapshotData;

use crate::ops::scan::pgrc::granule_source::{PartIoSpec, PartList, PartSpec};

leader_tls::leader_tls! {
    thread_local! {
        /// D1134: `relcache_callback` is registered (`ensure_release_registered`).
        static RELEASE_REGISTERED: Cell<bool> = const { Cell::new(false) };
    }
}

/// The relation's process-wide part lists and its parked sources' kept cursors go with its relcache entry (D1134, D1507).
/// C3 S4e: no epoch counts here any more: what a program, a front variant, a plan memo entry or a facts memo read is its
/// own recorded dependencies (gravity_deps `Held`).
fn relcache_callback(_arg: datum::Datum, relid: Oid) {
    LISTS.invalidate_relid(relid);
    // D1507: parked sources' kept cursors on the relation (a DROP's unlinked part files) go at the next statement boundary;
    // this only records (no lock, no allocation: the callback runs inside catalog access)
    gravity_driver::slots::note_released(relid);
}

/// lowerv4's `road_off::ROAD_OFF_GUC`: the roads forced off, read at the top of every lowering (D255).
pub const ROAD_OFF_GUC: &str = "pgrust.gravity_road_off";

leader_tls::leader_tls! {
    thread_local! {
        /// The GUC store's mutation count `road_watch` last looked at, and the `ROAD_OFF_GUC` text it saw then.
        static ROAD_AT: Cell<u64> = const { Cell::new(u64::MAX) };
        static ROAD_SEEN: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
        /// `ROAD_OFF_GUC`'s registry slot (D2896: looked up once, then read by its index).
        static ROAD_SLOT: Cell<crate::inputs::OptionSlot> = const { Cell::new(crate::inputs::OptionSlot::Unknown) };
    }
}

/// D311: the road set is a lowering input no content key carries, so the program tiers' key holds it (`tier_salt`: the
/// same statement under another set is another program). The text is read once per GUC store mutation (every SET / RESET
/// / SET LOCAL / set_config and the transaction-end restore take one), so a statement with no GUC change reads a counter.
fn road_watch() {
    let at = crate::inputs::guc_store_mutations();
    if ROAD_AT.with(|c| c.replace(at)) == at {
        return;
    }
    let now = ROAD_SLOT.with(|slot| crate::inputs::custom_guc_text_at(slot, ROAD_OFF_GUC));
    // PGRCSERVE G4: the rows road's twin lever (am_rows.rs `ROWS_ROAD_GUC`) changes what decompose maps, which the front
    // key does not walk: it moves the epoch as the road set does, and stamps the shared tier with it
    let now = match crate::ops::scan::am_rows::forced_now() {
        true => Some(format!("{}\u{1}rows", now.unwrap_or_default())), // alloc: Cold
        false => now,
    };
    if ROAD_SEEN.with(|s| *s.borrow() != now) {
        ROAD_SEEN.with(|s| *s.borrow_mut() = now);
    }
}

/// Whether this session's `ROAD_OFF_GUC` text names road `name` off (one of its comma-separated ids, as lowerv4's
/// `road_off::parse` reads them; a `+<id>` turns a default-off road on and names nothing off). For a road whose mechanism
/// runs before the lowering reads the option (decompose's D4568 `scan.mergefree`, through `Facts::scan_mergefree`); the
/// text is the one `tier_salt` keys the program tiers by, so a program never serves the other setting.
pub fn road_named_off(name: &str) -> bool {
    road_watch();
    ROAD_SEEN.with(|s| s.borrow().as_deref().is_some_and(|t| t.split(',').map(str::trim).any(|id| id == name)))
}

/// Whether this session's `ROAD_OFF_GUC` text turns default-off road `name` on (`+<name>`, lowerv4 road_off.rs `DEFAULT_OFF`
/// and `parse`): for a default-off road whose mechanism runs before the lowering reads the option (D4585:
/// `scan.mergefree.build`); the same text `tier_salt` keys the program tiers by.
pub fn road_named_on(name: &str) -> bool {
    road_watch();
    ROAD_SEEN.with(|s| s.borrow().as_deref().is_some_and(|t| t.split(',').map(str::trim).any(|id| id.strip_prefix('+').is_some_and(|r| r.trim() == name))))
}

/// C3 S3: whether this thread may read (`publish` false) or publish the process-wide program tier: the tier is on
/// (`l2cache::enabled`) and this thread does not build privately (uncommitted DDL, a parallel worker, a historic
/// snapshot). D3866 (step 2): a READ under this backend's own uncommitted DDL too (l2cache `private_by_own_ddl_only`),
/// whose entry is served only when that DDL touches none of the entry's dependencies (C3 S4b: gravity_deps
/// `Deps::current`); what this backend lowers under its own DDL stays its own.
pub fn shared_tier(publish: bool) -> bool {
    l2cache::enabled() && (!l2cache::private_build_mode() || !publish && l2cache::private_by_own_ddl_only())
}

/// D3866 (step 2): this backend builds privately (l2cache `private_build_mode`): the report's `tier=own-ddl`.
pub fn private_build() -> bool {
    l2cache::private_build_mode()
}

/// The program tiers' key beside the content key and what the plan names (exec `tier_key`): the database and the
/// road-off text (D311: a lowering input no content key carries).
pub fn tier_salt() -> u64 {
    road_watch();
    let mut m = gravity_abi::hash::Mix128::new();
    m.word(init_small::globals::MyDatabaseId() as u64);
    ROAD_SEEN.with(|s| match s.borrow().as_deref() {
        Some(t) => t.bytes().for_each(|b| m.word(b as u64 | 0x100)),
        None => m.word(u64::MAX),
    });
    let k = m.key128();
    k as u64 ^ (k >> 64) as u64
}

/// D1134: this backend's relcache callback, at every statement the entry seam sees (exec runner `prime_once`), so a DROP or
/// TRUNCATE from a session that never ran a graph admission still lets the process-wide lists and the parked sources give
/// back the relation's parts: they held its unlinked part files open for the server's life once the sessions that scanned
/// it had ended.
pub fn ensure_release_registered() -> PgResult<()> {
    RELEASE_REGISTERED.with(|c| -> PgResult<()> {
        if !c.get() {
            inval::invalidate::CacheRegisterRelcacheCallback(relcache_callback, datum::Datum::null())?;
            c.set(true);
        }
        Ok(())
    })
}

/// This backend's relcache callback (`ensure_release_registered`), registered before its first resident insert or graph
/// admission. C3 S6: KeySem's support epoch is gone too (its run-time bound calls hold what they read, keysem call.rs).
pub fn ensure_registered() -> PgResult<()> {
    ensure_release_registered()
}

/// The relation's parts under `snapshot`: the visible generation from the process catalog
/// (`pgrc_am::catalog`, no file touched), its part list built once per generation and kept.
/// A hot standby reads the relation too: its catalog is kept live by redo (`prime::prime_columnar`), so a standby's
/// read is served, never refused.
pub fn resident_parts(rel: &Relation<'_>, snapshot: Option<&SnapshotData<'_>>) -> PgResult<Arc<PartList>> {
    ensure_registered()?;
    listen();
    let locator = rel.rd_locator.get();
    let slot: pgrc_am::catalog::RelSlot = (locator.spcOid, locator.dbOid, locator.relNumber as u64);
    pgrc_am::catalog::record_relid(rel.rd_id, slot);
    let check = pgrc_am::probe::SnapshotCommitCheck::new(snapshot);
    let dir = pgrc_am::dirpath::table_dir_path(locator, rel.rd_backend); // alloc: Cold
    let found = pgrc_am::catalog::resolve(rel, slot, &dir, &check)?;
    check.take_error()?;
    let gen = match found {
        pgrc_am::catalog::Lookup::Gen(g) => g,
        pgrc_am::catalog::Lookup::Empty => return Ok(LISTS.empty()),
        pgrc_am::catalog::Lookup::Unknown => return Err(pgrc_am::catalog::not_loaded(rel, &dir)),
    };
    // Keyed by the Generation allocation (D843: Weak identity) under its view key; a composed view's list lives with the
    // view (COMPOSED, `Lists::list_for`)
    LISTS.list_for(slot, &gen, rel.rd_id)
}

/// D1113: the process's one pgrc slot-event listener, installed before the first list is built (`resident_parts`):
/// a retirement drops the retired generations' lists (`slot_event`).
fn listen() {
    static LISTENING: pgsync::OnceLock<()> = pgsync::OnceLock::new();
    LISTENING.get_or_init(|| pgrc_am::catalog::on_slot_event(slot_event));
}

/// D1113: pgrc's catalog changes that reclaim files. `Retired(slot, keep_from)` (`catalog::retire_below`, after
/// compaction reclaimed the generations below `keep_from`): their part lists (`LISTS`) held an `Arc<OpenPart>` per part,
/// so the unlinked files stayed open until the process exited. `Removed(slot, _)` (DROP / TRUNCATE / a rewrite's old
/// locator / an aborted creation) does the same for every generation of the slot, and with `Created(slot, _)` starts the
/// slot's numbering over, so its retired floor goes.
/// D1507: `Retired` and `Removed` also record a release for the parked worker slots and parks
/// (`gravity_driver::slots::note_released(0)`: the event names a slot, not a relid, so every relation): their sources'
/// kept cursors on the gone parts close at the next statement boundary. The relcache callback records only in a
/// session that registered it; this listener is process-wide, so a DROP from any session reaches the parked slots.
/// The other events change nothing here: `LISTS` is the one holder of part lists.
fn slot_event(ev: &pgrc_am::catalog::SlotEvent<'_>) {
    use pgrc_am::catalog::SlotEvent;
    match *ev {
        SlotEvent::Retired(slot, keep_from) => {
            LISTS.retire(slot, keep_from);
            gravity_driver::slots::note_released(0);
        }
        SlotEvent::Removed(slot, _) => {
            LISTS.invalidate_slot(slot);
            gravity_driver::slots::note_released(0);
        }
        SlotEvent::Created(slot, _) => LISTS.invalidate_slot(slot),
        SlotEvent::Published(..) | SlotEvent::Relid(_) => {}
    }
}

/// A kept list: the generation allocation it was built from (D843), the list, and the relation it was built for (D1507).
type Kept = (Weak<pgrc_am::catalog::Generation>, Arc<PartList>, Oid);

struct ListState {
    /// D1507: each entry names the relation it was built for, so a relcache invalidation of that relation (a DROP) lets
    /// its parts go (the entry held the dropped table's unlinked part files open for the server's life). Keyed by
    /// `(slot, Generation::view_key)`: a published generation's number.
    lists: BTreeMap<(pgrc_am::catalog::RelSlot, u64), Kept>,
    /// D1113: per slot, the generation its last retirement kept from: a list of a lower generation (a reader that
    /// resolved it before the retirement) is built for that reader and never kept.
    floor: BTreeMap<pgrc_am::catalog::RelSlot, u64>,
}

/// The per-generation part lists (`Arc<PartList>` over the catalog's open parts), built once per live catalog generation.
/// D843: an entry is that generation's own (`Weak` of the `Arc<Generation>` it was built from, compared by address): a
/// generation number reused after a rolled-back publish (the catalog replaces the same-numbered entry) never hands a
/// later reader the rolled-back generation's parts. The `Weak` keeps the allocation, so its address is never another
/// generation's while the entry lives. COMPOSED: a composed view's list lives with the view (`Generation::reader_state`).
pub struct Lists {
    state: pgsync::Mutex<ListState>,
    /// D4768: the slot and relation of the lists dropped lately (at most `RECENT_LISTS`, the oldest overwritten): a removal
    /// names the relation of a slot whose lists a relcache invalidation dropped first (DROP TABLE invalidates its relation
    /// before its storage goes).
    recent: pgsync::Mutex<([(pgrc_am::catalog::RelSlot, Oid); RECENT_LISTS], usize)>,
    empty: pgsync::Mutex<Option<Arc<PartList>>>,
    built: AtomicU64,
}

impl Lists {
    pub const fn new() -> Lists {
        Lists { state: pgsync::Mutex::new(ListState { lists: BTreeMap::new(), floor: BTreeMap::new() }), recent: pgsync::Mutex::new(([((0, 0, 0), 0); RECENT_LISTS], 0)), empty: pgsync::Mutex::new(None), built: AtomicU64::new(0) }
    }

    fn empty(&self) -> Arc<PartList> {
        let mut e = pgsync::lock(&self.empty);
        Arc::clone(e.get_or_insert_with(|| Arc::new(PartList::new(Vec::new()).arc_held()))) // alloc: Cold
    }

    /// The list of `gen` as the statement resolved it. COMPOSED: a composed view's list is built once per view and lives
    /// with it (`Generation::reader_state`): the view shares its number with a published generation, so it is never
    /// kept here; a published generation's is kept by `(slot, view_key)`, its number, while it is that allocation (D843).
    fn list_for(&self, slot: pgrc_am::catalog::RelSlot, gen: &Arc<pgrc_am::catalog::Generation>, relid: Oid) -> PgResult<Arc<PartList>> {
        if gen.composed() {
            return Ok(gen.reader_state(|| self.build(gen, relid)).unwrap_or_else(|| self.build(gen, relid)));
        }
        let key = (slot, gen.view_key());
        if let Some((_, l, _)) = pgsync::lock(&self.state).lists.get(&key).filter(|(w, _, _)| Weak::as_ptr(w) == Arc::as_ptr(gen)) {
            return Ok(Arc::clone(l));
        }
        let list = self.build(gen, relid);
        let mut st = pgsync::lock(&self.state);
        if st.floor.get(&slot).is_some_and(|&f| gen.gen < f) {
            return Ok(list); // D1113: retired while this reader built it: its reader's alone
        }
        // D2873 step B: the lists of generations the catalog dropped go (one list per generation would grow with
        // generations x parts again); freed after the lock, as invalidate_relid does
        let dead: Vec<_> = st.lists.iter().filter(|((s, _), e)| *s == slot && e.0.strong_count() == 0).map(|(k, _)| *k).collect(); // alloc: Cold
        let gone: Vec<_> = dead.iter().filter_map(|k| st.lists.remove(k)).collect(); // alloc: Cold
        let e = st.lists.entry(key).or_insert_with(|| (Arc::downgrade(gen), Arc::clone(&list), relid));
        if Weak::as_ptr(&e.0) != Arc::as_ptr(gen) {
            *e = (Arc::downgrade(gen), Arc::clone(&list), relid); // a republished number: the rolled-back generation's list goes
        }
        let out = Arc::clone(&e.1);
        drop(st);
        drop(gone);
        Ok(out)
    }

    /// `gen`'s part list over the catalog's open parts, with its answers.
    fn build(&self, gen: &pgrc_am::catalog::Generation, relid: Oid) -> Arc<PartList> {
        let parts = gen
            .parts
            .iter()
            .zip(&gen.manifest.parts)
            .map(|(p, rec)| {
                let ident = p.part.ident();
                pgrc_am::inval::record_part_key(relid, (ident.dev, ident.ino, ident.len));
                PartSpec { io: PartIoSpec::Path(p.path.clone()), expect: p.expect, rows: rec.rows, granules: rec.granule_count, deletes: p.deletes.clone(), part: Some(Arc::clone(&p.part)) }
            })
            .collect(); // alloc: Cold
        let list = Arc::new(PartList::with_answers(parts, gen.answers()).arc_held()); // alloc: Cold
        self.built.fetch_add(1, Ordering::Relaxed);
        list
    }

    /// D1113: the slot's generations below `keep_from` retired: their lists go (a reader holding one keeps its own
    /// `Arc`, so its parts stay open until it is done); a live generation's list stays. -> lists dropped. Freed after the
    /// lock (an `OpenPart`'s drop closes its file).
    pub fn retire(&self, slot: pgrc_am::catalog::RelSlot, keep_from: u64) -> usize {
        let gone: Vec<Kept> = {
            let mut st = pgsync::lock(&self.state);
            let f = st.floor.entry(slot).or_insert(0);
            *f = (*f).max(keep_from);
            let keys: Vec<_> = st.lists.keys().filter(|(s, g)| *s == slot && *g < keep_from).copied().collect(); // alloc: Cold
            keys.iter().filter_map(|k| st.lists.remove(k)).collect() // alloc: Cold
        };
        gone.len()
    }

    /// The relcache flush face: lists of the relation's slot go (a rewrite's new locator builds its own); D1113: so
    /// does its retired floor (a removed or re-created slot numbers its generations over).
    pub fn invalidate_slot(&self, slot: pgrc_am::catalog::RelSlot) {
        let gone: Vec<Kept> = {
            let mut st = pgsync::lock(&self.state);
            st.floor.remove(&slot);
            let keys: Vec<_> = st.lists.keys().filter(|(s, _)| *s == slot).copied().collect(); // alloc: Cold
            keys.iter().filter_map(|k| st.lists.remove(k)).collect() // alloc: Cold
        };
        drop(gone);
    }

    /// D4570: the columnar catalog removed relation slots' entries (`pgrc_am::catalog::set_removed_hook`: DROP TABLE /
    /// DATABASE, TRUNCATE, a rewrite's old locator; `removed(slot)`): their lists go, freed after the lock, and each
    /// relation they were built for is noted released, so the parked sources' kept cursors on it go at the next statement
    /// boundary (`slots::note_released`: no lock, no allocation). DROP DATABASE invalidates no relcache entry of its
    /// relations, and with the metacache on no trim lets a list go: a dropped database's lists kept their parts and faces
    /// for the server's life.
    /// D4768: what went, for the slot depot (`SlotDepot::evict_reading`): the dropped lists with their relations, and the
    /// relations of recent dropped lists whose slots the removal names (`Removed`).
    pub fn invalidate_slots(&self, removed: &dyn Fn(pgrc_am::catalog::RelSlot) -> bool) -> Removed {
        let gone = self.drop_lists(&|slot, _| removed(slot), true);
        let mut lately = [0; RECENT_LISTS];
        {
            let r = pgsync::lock(&self.recent);
            // bounded("small-fixed: RECENT_LISTS entries")
            for (l, &(slot, rel)) in lately.iter_mut().zip(r.0.iter()) {
                if rel != 0 && removed(slot) {
                    *l = rel;
                }
            }
        }
        Removed { gone, lately }
    }

    /// D1507: the relcache callback's face: the lists built for `relid` go (0: every list), so a dropped relation's parts
    /// are no longer held here; a live relation's next statement builds its list again from the catalog's open parts.
    pub fn invalidate_relid(&self, relid: Oid) {
        drop(self.drop_lists(&|_, rel| relid == 0 || rel == relid, false));
    }

    /// The lists `pick(slot, relid)` names go, each built-for relation noted released when `released`, and remembered
    /// (`recent`, D4768). The dropped lists with their relations, for the caller to free after the lock (an `OpenPart`'s
    /// drop closes its file).
    fn drop_lists(&self, pick: &dyn Fn(pgrc_am::catalog::RelSlot, Oid) -> bool, released: bool) -> Vec<(Arc<PartList>, Oid)> {
        // D4398: on the memory ladder's pass (the part cache's evicted hook), nothing while another thread holds them
        let Some(mut st) = memledger::ladder_pass::lock_or_skip(&self.state) else { return Vec::new() }; // alloc: Cold
        let keys: Vec<_> = st.lists.iter().filter(|((s, _), e)| pick(*s, e.2)).map(|(k, _)| *k).collect(); // alloc: Cold
        let mut recent = pgsync::lock(&self.recent);
        keys.iter()
            .filter_map(|k| st.lists.remove(k).map(|e| (k.0, e)))
            .map(|(slot, (_, list, rel))| {
                if released {
                    gravity_driver::slots::note_released(rel);
                }
                if !recent.0.contains(&(slot, rel)) {
                    let at = recent.1 % RECENT_LISTS;
                    recent.0[at] = (slot, rel);
                    recent.1 = recent.1.wrapping_add(1);
                }
                (list, rel)
            })
            .collect() // alloc: Cold
    }

    pub fn built(&self) -> u64 {
        self.built.load(Ordering::Relaxed)
    }
}

pub static LISTS: Lists = Lists::new();

/// D4768: the dropped lists `Lists::recent` remembers.
const RECENT_LISTS: usize = 64;

/// D4768: what a removal let go (`Lists::invalidate_slots`): the dropped lists with their relations (freed when this
/// drops, after the lists' lock) and the relations of recent dropped lists the removal names (a DROP TABLE's relcache
/// invalidation dropped its lists before its storage went). A fixed array and the lists' own vector: nothing allocated
/// for the relations.
pub struct Removed {
    gone: Vec<(Arc<PartList>, Oid)>,
    lately: [Oid; RECENT_LISTS],
}

impl Removed {
    /// Whether `rel` (0: none) is a removed relation.
    pub fn has(&self, rel: Oid) -> bool {
        rel != 0 && (self.gone.iter().any(|g| g.1 == rel) || self.lately.contains(&rel))
    }

    /// `f` once for each removed relation.
    pub fn each_rel(&self, mut f: impl FnMut(Oid)) {
        let n = self.gone.len() + RECENT_LISTS;
        // bounded("catalog-sized: the relations one removal names, each against the ones before it")
        for i in 0..n {
            let rel = self.at(i);
            if rel != 0 && !(0..i).any(|j| self.at(j) == rel) {
                f(rel);
            }
        }
    }

    /// The `i`th removed relation, the dropped lists' first (0: none).
    fn at(&self, i: usize) -> Oid {
        match self.gone.get(i) {
            Some(g) => g.1,
            None => self.lately[i - self.gone.len()],
        }
    }
}

/// D3003: the pgrc part cache's budget option (MB; -1 = auto, `cost::part_cache_cap`; 0 = nothing kept across statements
/// past the parts in use). D3675: a server-level setting (PGC_SIGHUP, `inputs::part_cache_server_mb`): one budget for
/// every session.
pub const PART_CACHE_GUC: &str = "pgrust.pgrc_part_cache_mb";

/// The server's explicit part cache budget in bytes as the last statement start read it (u64::MAX: none, derived): the
/// part cache's floor under the memory broker (`memory_budget`'s floor hook, which reads no GUC).
static SERVER_BYTES: AtomicU64 = AtomicU64::new(u64::MAX);

/// The server's explicit part cache budget in bytes (`SERVER_BYTES`), None when derived.
pub fn part_cache_server_bytes() -> Option<u64> {
    match SERVER_BYTES.load(Ordering::Relaxed) {
        u64::MAX => None,
        b => Some(b),
    }
}

/// Memory step 7: whether the memory broker sizes the part cache now (on, with the `part_cache` actuator). Two relaxed
/// loads: a statement start reads it.
fn follows_broker() -> bool {
    use gravity_driver::mem::budget::{self, actuator, Mode};
    budget::mode() == Mode::On && budget::actuators() & actuator::PART_CACHE != 0
}

/// Who sizes the part cache now (`mempolicy::cache::part_cache_sizer`): the ladder under C1, else the broker when its
/// part-cache actuator acts.
fn sizer_now() -> Sizer {
    part_cache_sizer(follows_broker(), mcx::ladder::floors_on())
}

// The part cache's budget at a statement start and the short hook's pressure level are the memory policy's.
use mempolicy::cache::{part_cache_budget, part_cache_sizer, short_pressure, short_release, ShortRelease, Sizer};

/// The part cache's evicted hook: the part lists go (their dictionary handles keep the evicted dictionary images
/// alive); a running statement keeps its own list, the next one builds its list again from the catalog's open parts
/// (the images of parts not evicted are shared again, never rebuilt).
/// D4315: the trim a scan's pass past the part cache's budget runs (`partcache::set_trim_hook`).
fn part_cache_trim() {
    pgrc_am::inval::registry().trim_if_over();
}

/// D4570: the columnar catalog's removed hook (`pgrc_am::catalog::set_removed_hook`).
fn catalog_removed(removed: &dyn Fn(pgrc_am::catalog::RelSlot) -> bool) {
    let gone = LISTS.invalidate_slots(removed);
    // D4768: a parked worker set that read a removed relation can never be taken again: it goes now, on this thread (and
    // a set taken before this drops at its put)
    let mut any = false;
    gone.each_rel(|rel| {
        gravity_driver::slots::note_removed(rel);
        any = true;
    });
    if any {
        gravity_driver::slots::DEPOT.evict_reading(&|rel| gone.has(rel));
    }
}

fn part_cache_evicted() {
    // D4504: with the metacache on, a list's metadata faces are their parts' part-cache contents (the faces hook lets each
    // go with its part, after its data), so the lists stay; off, every list goes, as before
    if !crate::inputs::metacache_enabled() {
        LISTS.invalidate_relid(0);
    }
    // D4410: a running statement's list kept every dictionary in its cells; what stays now is what statements hold
    crate::ops::scan::pgrc::granule_source::let_warm_go();
}

/// D4504: the part cache's faces hook: the faces the content trim let go (`OpenPart::evict_faces`) leave the lists' warm
/// cells.
fn part_cache_faces_evicted() {
    crate::ops::scan::pgrc::granule_source::let_stale_faces_go();
}

/// D3268: the part cache's short hook (a content trim ended past its target with parts in use). A worker set parked in the
/// slot depot keeps its sources' decode cursors across statements, each a use of its part, and a part in use keeps every
/// stream any statement cached in it: after a few dozen statement shapes the parked sets pinned every part of a 511-part
/// table (18.6 GB of a 3.6 GB budget) and no statement's trim could evict anything until the idle release. Here the kept
/// cursors go (D1507's release of every relation: the depot's sets now, each backend's park at its next statement; they
/// reopen lazily), once per statement epoch: a statement whose own working set is past the budget does not release them at
/// every trim. All of them only under memory pressure ([`short_pressure`]). The level is read before the epoch's latch, so
/// a statement that crosses it part way still releases once at its next trim.
/// D3110 (a): below the level, the parked sets but the NEWEST let their parts go, once per epoch (memfree (ii)'s rule,
/// `SlotDepot::trim_but_newest`): a hot repeat retakes the set its warm run just parked (D3169), and the older shapes' sets no
/// longer hold the cache past the budget the server set (a 50 % container's run of many scan shapes over one wide table:
/// 9.9 GB of a 5.9 GB cap, every statement past the memory watchdog's 90 %).
fn part_cache_short() -> bool {
    static LAST: AtomicU64 = AtomicU64::new(0);
    static LAST_OLDER: AtomicU64 = AtomicU64::new(0);
    let bar = mcx::governor::bar() as u64;
    let projected = if bar == 0 { 0 } else { mcx::governor::projected() as u64 };
    let (bytes, budget) = (pgrc_read::partcache::bytes(), pgrc_read::partcache::budget());
    let epoch = pgrc_read::partcache::epoch();
    let depot = &gravity_driver::slots::DEPOT;
    let pressure = short_pressure(bar, projected, bytes, budget);
    let sets = if pressure { 0 } else { depot.stats().entries };
    match short_release(pressure, sets, || LAST_OLDER.swap(epoch, Ordering::Relaxed) == epoch, || LAST.swap(epoch, Ordering::Relaxed) == epoch) {
        ShortRelease::None => return false,
        ShortRelease::Older => {
            depot.trim_but_newest(usize::MAX);
            OLDER_RELEASES.fetch_add(1, Ordering::Relaxed);
            return true;
        }
        ShortRelease::All => {}
    }
    SHORT_RELEASES.fetch_add(1, Ordering::Relaxed);
    if gravity_driver::env::set("PGRUST_GRAVITY_REPORT") {
        // the server log's line, before the statement's own `gravity: served` line: which statements reach the level
        let mib = |b: u64| b >> 20;
        eprintln!("LOG:  gravity: part cache short release past the pressure level: projected={} bar={} part_cache={}/{} (MiB)", mib(projected), mib(bar), mib(bytes), mib(budget)); // alloc: Cold
    }
    gravity_driver::slots::note_released(0);
    gravity_driver::slots::drain_released();
    true
}

/// D3268: the short hook's releases (tests; the census).
static SHORT_RELEASES: AtomicU64 = AtomicU64::new(0);
/// D3110 (a): the short hook's releases of the older parked sets below the pressure level (tests; the census).
static OLDER_RELEASES: AtomicU64 = AtomicU64::new(0);

pub fn part_cache_older_releases() -> u64 {
    OLDER_RELEASES.load(Ordering::Relaxed)
}

pub fn part_cache_short_releases() -> u64 {
    SHORT_RELEASES.load(Ordering::Relaxed)
}

/// D3994 (B.1 W): `PGRUST_GRAVITY_PARK_EVICT=1` lets every content trim evict the parts only parked executors use, memory
/// pressure or not (a check run's lever: every rebound kept cursor then faults its images again); read at the arm.
static PARK_EVICT_ALWAYS: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// D3994 (B.1 W): the part cache's parked gate (`pgrc_read::partcache::set_parked_hook`): whether a trim evicts the parts
/// only parked executors use (their kept cursors hold none of their images), the memory policy's one switch
/// (`mempolicy::cache::PARKED_GATE`: under memory pressure; `PGRUST_GRAVITY_PARK_EVICT=1`: always, a check run's lever).
/// Atomics only: it runs on the reclaimer's and the memory watchdog's threads too.
fn part_cache_parked() -> bool {
    if PARK_EVICT_ALWAYS.load(Ordering::Relaxed) {
        return true;
    }
    mempolicy::cache::parked_evictable(mempolicy::cache::PARKED_GATE, &pressure_signals())
}

/// D3994: the memory policy's pressure signals for the part cache (`mempolicy::cache::ParkedSignals`): the governor's bar
/// and projection, B and its free bytes, the broker's hold on the cache, the cache's bytes and budget. Atomics only.
fn pressure_signals() -> mempolicy::cache::ParkedSignals {
    let bar = mcx::governor::bar() as u64;
    let limit = match mcx::governor::limit() as u64 {
        0 => memledger::fallback_limit(),
        l => l,
    };
    mempolicy::cache::ParkedSignals {
        bar,
        projected: if bar == 0 { 0 } else { mcx::governor::projected() as u64 },
        free_b: gravity_driver::mem::budget::free_now(),
        b: mempolicy::snapshot::b_now(limit, bar),
        broker: sizer_now() == Sizer::Broker,
        bytes: pgrc_read::partcache::bytes(),
        budget: pgrc_read::partcache::budget(),
    }
}

/// D3994 (level gate): the memory policy's switch says a running statement's part every claim left goes back now though the
/// part cache is inside its budget (`mempolicy::cache::release_short` under `PARKED_GATE`: memory short under Pressure).
/// A release check reads it per part credit: the signals its arm needs only, cheapest first (`release_short_by`), never the
/// whole `pressure_signals`.
pub(crate) fn release_short() -> bool {
    let bar = mcx::governor::bar() as u64;
    let limit = || match mcx::governor::limit() as u64 {
        0 => memledger::fallback_limit(),
        l => l,
    };
    // B as the last snapshot composed it (the one free_now's bytes come from); composed here only before any snapshot
    let b = || match memledger::b_at() {
        0 => mempolicy::snapshot::b_now(limit(), bar),
        b => b,
    };
    mempolicy::cache::release_short_by(mempolicy::cache::PARKED_GATE, bar, || mcx::governor::projected() as u64, gravity_driver::mem::budget::free_now, b)
}

/// D3063 residual: the part cache down to its trim target outside a statement's start (the process idle release, the
/// memory governor's pressure release): the parts no statement uses give their contents back, the parts in use keep
/// theirs. Before this, only a Gravity statement's start trimmed, so a server whose next statements failed earlier (the
/// governor refusing them past its bar) kept a statement's 20 GB of part cache for good. The bytes evicted.
pub fn trim_part_cache() -> u64 {
    let target = pgrc_read::partcache::trim_target();
    if pgrc_read::partcache::bytes() <= target {
        return 0;
    }
    pgrc_am::inval::registry().trim_contents(target)
}

/// The part cache's LOAD GATE (LADDER-DESIGN section 3; `pgrc_read::partcache::load_gate`): a load of `n` bytes past the
/// cache's budget. The one place the memory ladder's `climb()` replaces; until it lands, today's trim and a take_wait-like
/// wait:
/// - the ELASTIC bytes first: the parts no running statement uses give their contents back down to the budget less the
///   load (`PartRegistry::load_room`);
/// - only bytes running statements pin can keep the load from fitting. A demand load waits for OTHER running statements'
///   pins (its own cannot go while it waits) when they could make the room, polling as the grant pool's take_wait does
///   (2 ms, the statement's cancel ends it) for at most `bytes_wait_ms`, and makes room again as they let parts go. A wait
///   that runs out is spent for the statement epoch (`partcache::load_wait_spent`), so the pins cost a statement one wait.
/// - then the load goes on, past the budget if pins still fill it: a running scan's read is never refused here (the
///   watchdog stays the backstop until the ladder's R6).
fn part_cache_load(n: u64, kind: pgrc_read::partcache::Load) {
    use pgrc_read::partcache;
    let registry = pgrc_am::inval::registry();
    let short = registry.load_room(n);
    let demand = kind == partcache::Load::Demand;
    if short == 0 || !demand || partcache::load_wait_spent() {
        return;
    }
    let mine = memledger::charge::context().0.acct().0;
    let mut others = 0u64;
    registry.in_use_by_running(|acct, bytes| {
        if acct != mine {
            others = others.saturating_add(bytes);
        }
    });
    if !mempolicy::cache::part_cache_load_waits(demand, false, others, short) {
        return;
    }
    let budget = mempolicy::consts::get_ms(mempolicy::consts::K::BytesWaitMs);
    let t0 = pg_clock::MonoStamp::now();
    let mut step = cancelpoll::step("part cache load gate: a wait for other statements' pinned parts");
    let mut pinned = registry.in_use_bytes();
    let mut polls = 0u32;
    let ran_out = loop {
        if step.tick().is_err() {
            break false;
        }
        waiter::sleep(core::time::Duration::from_millis(2));
        if mempolicy::cache::part_cache_load_fits(partcache::bytes(), n, partcache::budget()) {
            break false;
        }
        // every fifth poll (10 ms): a statement's end or a part every claim of it left unpins bytes; they are elastic
        // now and the trim makes the room
        polls += 1;
        if polls % 5 == 0 {
            let now = registry.in_use_bytes();
            if now < pinned && registry.load_room(n) == 0 {
                break false;
            }
            pinned = now;
        }
        if core::time::Duration::from_nanos(t0.elapsed_ns()) >= budget {
            break true;
        }
    };
    partcache::note_load_wait(t0.elapsed_ns() / 1_000_000, ran_out);
}

/// Ladder C1b: the part cache's floor at the memory ladder's last sample (`mcx::ladder::sized_floors`: the bytes its
/// trim cannot drop) and its hold's reach (`mcx::ladder::part_hold_reach`), which bound its own trims at or past its
/// budget (`pgrc_read::partcache::own_cushion`). With cache_hold = 0 (H5a) none: no floor holds the own trims.
fn part_cache_ladder_hold() -> (u64, u64) {
    if mempolicy::consts::get(mempolicy::consts::K::CacheHold) == 0 {
        return (0, 0);
    }
    (mcx::ladder::sized_floors()[mcx::ladder::SIZED_PART], mcx::ladder::part_hold_reach())
}

/// The LOAD GATE's process counters (`pgrc_read::partcache::load_counters`): loads that asked past the budget, bytes their
/// trims evicted, waits, waits' ms, loads that went on past the budget. The runner's report line diffs them per statement.
pub fn load_counts() -> [u64; 5] {
    let (g, t, w, ms, o) = pgrc_read::partcache::load_counters();
    [g, t, w, ms, o]
}

/// D3063 (2a): the part cache's faults and frees reported to the running statement's memory ledger and the memory
/// governor (`mcx::governor::charge_cache`): past pgrust.statement_mem_limit or the governor's bar, the statement's next
/// armed block fails it with 53200 naming the cache, instead of the box swapping under a statement's in-use parts.
/// D4795 (2a): `rec` (the holder's) carries the charging statement's part, which a free takes back while it runs.
fn part_cache_account(n: u64, charged: bool, rec: &mut pgrc_read::partcache::Charged) {
    let n = usize::try_from(n).unwrap_or(usize::MAX);
    let mut c = mcx::governor::CacheCharge { slot: rec.slot, epoch: rec.epoch, bytes: rec.bytes };
    if charged {
        mcx::governor::charge_cache(n, "pgrc part cache", &mut c);
    } else {
        mcx::governor::release_cache(n, &mut c);
    }
    *rec = pgrc_read::partcache::Charged { slot: c.slot, epoch: c.epoch, bytes: c.bytes };
}

/// D3003: at a statement's start (`runner::portal::prime_once`): the part cache's budget from the server's `PART_CACHE_GUC`
/// or the auto cap over this machine and the grant pool as armed now, its evicted hook, and a trim when it is over (one
/// relaxed load pair when it is not). The cache is its own budget beside the pool, never charged to it: a statement whose
/// grant needs nearly the whole pool would otherwise evict its own cached parts (or the parked depots) at every repeat.
/// Memory step 7: under the memory broker the budget follows its target (`part_cache_budget`), and the part registry's
/// byte budget follows the cache's, so its janitor keeps the entries (their re-use history) the cache keeps.
pub fn arm_part_cache() {
    if !ARMED.load(Ordering::Relaxed) && !ARMED.swap(true, Ordering::AcqRel) {
        pgrc_read::partcache::set_evicted_hook(part_cache_evicted);
        pgrc_read::partcache::set_faces_hook(part_cache_faces_evicted);
        pgrc_read::partcache::set_short_hook(part_cache_short);
        // D4315: a scan's passed extents go back as they pass, past the budget
        pgrc_read::partcache::set_trim_hook(part_cache_trim);
        PARK_EVICT_ALWAYS.store(gravity_driver::env::num::<u32>("PGRUST_GRAVITY_PARK_EVICT") == Some(1), Ordering::Relaxed);
        pgrc_read::partcache::set_parked_hook(part_cache_parked);
        // D3994 (B.1 W) under D3823's switch: a trim poisons the images it evicts from a parked-only part
        pgrc_read::partcache::set_park_poison(crate::inputs::release_poison());
        pgrc_read::partcache::set_account_hook(part_cache_account);
        pgrc_read::partcache::set_load_hook(part_cache_load);
        pgrc_read::partcache::set_hold_hook(part_cache_ladder_hold);
        pgrc_am::catalog::set_removed_hook(catalog_removed);
        mcx::set_idle_release(gravity_driver::mem::grant::idle_release);
        mcx::set_pressure_trim(gravity_driver::mem::grant::pressure_trim);
        // P3 (`parked_trim_rung`): the ladder's R2p over the parked regions and slot sets
        mcx::ladder::set_parked_trim(gravity_driver::mem::grant::parked_bytes, gravity_driver::mem::grant::trim_parked);
    }
    let (budget, sized) = budget_now();
    crate::dict::text_hosts::reuse::note_statement();
    pgrc_read::partcache::set_budget(budget);
    pgrc_read::partcache::statement_boundary();
    let registry = pgrc_am::inval::registry();
    registry.set_budget(pgrc_am::inval::registry_budget(sized.then_some(budget)));
    registry.trim_if_over();
}

/// The part cache's hooks are installed (a Gravity statement armed it); before that nothing is cached.
static ARMED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// The part cache's budget now and whether it is sized (it follows the memory broker or, ladder C1b, the ladder; the
/// purger calls it `follows`): the server's `PART_CACHE_GUC` (a process-wide read, no GUC store) or the auto cap, under
/// the broker or the ladder their target. A statement's start and C3's purger read it.
fn budget_now() -> (u64, bool) {
    let server = crate::inputs::part_cache_server_mb().map(|mb| mb.saturating_mul(1 << 20));
    SERVER_BYTES.store(server.unwrap_or(u64::MAX), Ordering::Relaxed);
    // ladder C1b (`cache_floor` with the ladder on): the ladder sizes the part cache, the broker on or not (the one
    // evicter, `part_cache_sizer`). C1d: the derived budget (or a server setting) is where it starts, no cap: the marks
    // bound its raises; a server setting stays its minimum, and its own trim stops `cache_trim_step_mb` under the budget
    let sizer = sizer_now();
    let (follows, ladder) = (sizer == Sizer::Broker, sizer == Sizer::Ladder);
    let derived = || crate::dict::text_hosts::reuse::store_bytes(gravity_program::cost::part_cache_cap) as u64;
    let target = if follows {
        gravity_driver::mem::budget::target(gravity_driver::mem::budget::Consumer::PartCache)
    } else if ladder {
        Some(mcx::ladder::sized_budget(mcx::ladder::SIZED_PART, server.unwrap_or_else(derived)))
    } else {
        None
    };
    pgrc_read::partcache::set_trim_step(if ladder { mempolicy::consts::get(mempolicy::consts::K::CacheTrimStepMb) << 20 } else { 0 });
    (part_cache_budget(server, target, derived), follows || ladder)
}

/// C3 (`cache_purge_now`): the part cache's cut applied off every query thread (memruntime's purger, through
/// `memory_budget::cut_caches`): the budget re-read as a statement's start reads it, and the trim a statement's start
/// would run when the cache is past it. Before it, a lowered budget (a reload, the broker's target) waited for the next
/// Gravity statement, which paid the trim (~50 ms for a 4 GiB cut). `(bytes evicted, bytes still past the budget)`: the
/// second is what in-use and shared parts hold. `due(budget)`: whether the purger trims at this budget (a cut); when not,
/// nothing is set or trimmed.
pub fn cut_part_cache(due: impl FnOnce(u64) -> bool) -> (u64, u64) {
    if !ARMED.load(Ordering::Acquire) {
        return (0, 0);
    }
    let (budget, follows) = budget_now();
    if !due(budget) {
        return (0, 0);
    }
    pgrc_read::partcache::set_budget(budget);
    let registry = pgrc_am::inval::registry();
    registry.set_budget(pgrc_am::inval::registry_budget(follows.then_some(budget)));
    let freed = registry.trim_if_over();
    (freed, pgrc_read::partcache::bytes().saturating_sub(budget))
}
#[cfg(test)]
mod tests {
    use super::*;
    use pgrc_format::manifest::{Manifest, ManifestHeader};

    /// H5a: cache_hold = 1 (the default) is the landed shape: the hook reads the ladder's floor and the hold's reach; 0 =
    /// no floor holds the part cache's own trims (`pgrc_read::partcache::own_cushion` sees none).
    #[test]
    fn the_hold_knob_on_is_the_landed_hold_and_off_reads_no_floor() {
        use mempolicy::consts::{get, set, K};
        struct HoldOn;
        impl Drop for HoldOn {
            fn drop(&mut self) {
                set(K::CacheHold, 1);
            }
        }
        // the knob table is process-wide: no other test reads cache_hold = 0, and a failing assert restores it
        let _one = pgsync::lock(&mempolicy::consts::TEST_LOCK);
        let _on = HoldOn;
        assert_eq!(get(K::CacheHold), 1, "on by default");
        assert_eq!(part_cache_ladder_hold(), (mcx::ladder::sized_floors()[mcx::ladder::SIZED_PART], mcx::ladder::part_hold_reach()), "on: the ladder's floor and reach");
        set(K::CacheHold, 0);
        let off = part_cache_ladder_hold();
        set(K::CacheHold, 1);
        assert_eq!(off, (0, 0), "off: no floor, no reach");
        assert_eq!(mempolicy::cache::part_cache_own_cushion(1 << 30, off.0, 256 << 20, off.1), None, "so no cushion: the budget bounds the own trims");
    }

    /// COMPOSED: two statements resolving one composed view at once share one list built for it; it is not kept by
    /// number, so the published generation of that number gets its own.
    #[test]
    fn a_composed_views_list_is_built_once_and_lives_with_the_view() {
        let lists = Lists::new();
        let header = ManifestHeader { gen: 3, prev_gen: 2, publisher_fxid: 30, relfilenumber: 9, schema_fingerprint: 1, magic: 0, format_version: 0, spc: 1663, db: 5, part_count: 0, next_part_no: 0, flags: 0, reserved: 0 };
        let view = pgrc_am::catalog::Generation::detached_view(Manifest::new(header, Vec::new())).unwrap(); // alloc: Cold
        let slot = (1663, 5, 9_160);
        let gate = std::sync::Barrier::new(2);
        let got: Vec<Arc<PartList>> = std::thread::scope(|s| {
            let hs: Vec<_> = (0..2)
                .map(|_| {
                    s.spawn(|| {
                        gate.wait();
                        lists.list_for(slot, &view, 1).unwrap()
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(Arc::ptr_eq(&got[0], &got[1]) && lists.built() == 1, "one build, one list");
        assert!(pgsync::lock(&lists.state).lists.is_empty(), "a composed view's list is not kept by number");
        let plain = Arc::new(pgrc_am::catalog::Generation::detached(Manifest::new(header, Vec::new())).unwrap()); // alloc: Cold
        let p = lists.list_for(slot, &plain, 1).unwrap();
        assert!(!Arc::ptr_eq(&p, &got[0]) && lists.built() == 2, "generation 3 itself: its own list");
        assert!(Arc::ptr_eq(&lists.list_for(slot, &view, 1).unwrap(), &got[0]) && lists.built() == 2, "the view's again");
    }

    /// D3675 / memory step 7: the server's setting or the auto cap; under the broker its target, never under the setting.
    #[test]
    fn the_budget_is_the_servers_setting_and_under_the_broker_its_target_never_under_the_setting() {
        const M: u64 = 1 << 20;
        assert_eq!(part_cache_budget(None, None, || 77 * M), 77 * M, "derived");
        assert_eq!(part_cache_budget(Some(6 * M), None, || 77 * M), 6 * M, "the server's setting");
        assert_eq!(part_cache_budget(Some(0), None, || 77 * M), 0, "0 keeps only the parts in use");
        assert_eq!(part_cache_budget(None, Some(40 * M), || 77 * M), 40 * M, "the broker's target");
        assert_eq!(part_cache_budget(Some(60 * M), Some(40 * M), || 77 * M), 60 * M, "a setting is the minimum");
        assert_eq!(part_cache_budget(Some(20 * M), Some(40 * M), || 77 * M), 40 * M);
    }

    /// D3268 (B): the short hook releases the parked sets' parts only past its pressure level: three quarters of the
    /// governor's bar, or with the governor off four times the part cache's budget (unbounded: never).
    #[test]
    fn d3268_b_short_hook_releases_only_past_the_pressure_level() {
        const G: u64 = 1 << 30;
        let bar = 27 * G;
        // governor on: the cache's size against its budget does not matter, the process's resident bytes do
        assert!(!short_pressure(bar, 16 * G, 18 * G, 3 * G));
        assert!(!short_pressure(bar, bar - bar / 4, 18 * G, 3 * G));
        assert!(short_pressure(bar, bar - bar / 4 + 1, 2 * G, 3 * G));
        assert!(short_pressure(bar, 26 * G, 18 * G, 3 * G));
        // governor off: the part cache against four times its budget
        assert!(!short_pressure(0, 0, 12 * G, 3 * G));
        assert!(!short_pressure(0, 0, 12 * G + 3, 3 * G));
        assert!(short_pressure(0, 0, 12 * G + 4, 3 * G));
        assert!(!short_pressure(0, 0, u64::MAX, u64::MAX));
    }
}
