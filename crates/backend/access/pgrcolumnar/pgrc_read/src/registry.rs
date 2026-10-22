//! The process-shared part registry: parsed-metadata sharing + the budgeted
//! part cache with pins and the inline LRU janitor (charter §1 "sharing is
//! day-one architecture"; spec §11 "equal identity ⇒ identical bytes").
//!
//! - **Key**: `(dev, ino, len)` — the stat-only identity prefix, so a cache
//!   hit costs one fstat and ZERO content reads (the stat-before-read
//!   discipline). The full §11 quadruple (+ uuid) lives on the entry;
//!   sealed-part immutability means one `(dev, ino, len)` can only ever name
//!   one byte image (the ino-reuse hole is closed by the AM integration via
//!   [`PartRegistry::invalidate`] — the named M3-G/H seam).
//! - **Pins are law**: a pinned part is NEVER evicted. Budget is capacity
//!   guidance — the janitor evicts least-recently-used unpinned parts until
//!   under budget; if everything left is pinned the cache runs over budget
//!   and counts the event (witness).
//! - **The janitor is inline** (at insert and [`PartRegistry::maintain`]),
//!   never a thread (no new thread populations), and LRU time is a logical
//!   clock — never wall time.
//! - **Eviction is a map removal, not a free**: in-flight `Arc<OpenPart>`
//!   holders (scans, dict handles) keep their part alive and
//!   generation-stable; its memory becomes the holder's working set and
//!   stops counting against the registry budget.

use std::collections::BTreeMap;
use std::sync::Arc;

use pgsync::atomic::{AtomicU64, Ordering};
use pgsync::Mutex;

use crate::io::PartIo;
use crate::openpart::{OpenPart, PartExpect};
use crate::ReadResult;

/// Stat-only registry key: `(dev, ino, len)`.
pub type PartKey = (u64, u64, u64);

/// Default cap on the number of live cache entries (charter §1 "sharing"
/// budget is guidance, not law). An entry's part fd is a slot of the
/// process-wide pool (`crate::fdpool`, D2873), which bounds open descriptors
/// itself; an entry whose part was opened but never decoded has
/// `resident()==0` and exerts zero byte-budget pressure, so a stream of such
/// entries (scans that fetch no rows, self-scans of aborted publishes) would
/// still accumulate entries and their unlinked files without bound. A count
/// cap makes the janitor reclaim LRU-unpinned entries by entry count as well.
/// A miss re-resolves from the manifest, so eviction is correctness-preserving.
pub const DEFAULT_MAX_ENTRIES: u64 = 4096;

struct RegState {
    parts: BTreeMap<PartKey, Arc<OpenPart>>,
}

/// Measured-only registry counters (witnesses; no benchmark semantics).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RegistryCounters {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub over_budget_events: u64,
}

/// Memory step 6a: the part cache's re-use census at one broker second ([`PartRegistry::reuse_census`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReuseCensus {
    /// The NEED (`OpenPart::need_bytes`) of the parts whose statement use before their latest is inside 60 / 300 / 900 s.
    pub reused: [u64; 3],
    pub reused_parts: [u64; 3],
    /// The file bytes of the streams opened inside 900 s (the page-cache floor's signal).
    pub file_bytes_900: u64,
    pub parts: u64,
    /// The parts a statement used inside 900 s.
    pub used_parts_900: u64,
}

/// D4910 step 1 (`ladder_reconcile`'s pin rows): what the in-use parts' pins are made of ([`PartRegistry::pin_breakdown`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PinBreakdown {
    /// The parts with a live use (`OpenPart::live_users`), those a live claim holds (the floor's, `live_claimed`), and those
    /// only deferring statements hold.
    pub parts_live: u64,
    pub parts_claimed: u64,
    pub parts_deferred: u64,
    /// The most live users one part has (a wide statement's clones).
    pub users_max: u64,
    /// Of the claimed parts: their segment images (`OpenPart::resident`), their passed extents' bytes, their part-grain
    /// dictionary images, and the bytes they pin (`OpenPart::pinned_bytes`: what `in_use_bytes` sums).
    pub images: u64,
    pub passed: u64,
    pub dicts: u64,
    pub pinned: u64,
    /// The claimed parts whose segment images are all passed (pinned by their dictionary images alone), and their pinned
    /// bytes.
    pub parts_fully_passed: u64,
    pub fully_passed_pinned: u64,
    /// Of the parts only deferring statements hold: the bytes they would pin (`OpenPart::pinned_bytes`).
    pub deferred_pinned: u64,
}

/// D4910 / D4908: the part cache's bytes its trim cannot drop now, by what holds each part
/// ([`PartRegistry::untrimmable_by_holder`]), and the parked gate's answer at the walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UntrimmableByHolder {
    /// A live claim holds the part (`OpenPart::live_claimed`).
    pub live: u64,
    /// Only the live uses of statements that deferred it hold the part.
    pub deferred: u64,
    /// Only parked executors' uses hold the part (`live_users` 0, `users` not): a depot's worker set, a kept cursor.
    pub parked: u64,
    /// No use: images another clone holds (a part list's, D4407), or a part in flight.
    pub none: u64,
    /// Whether a trim may evict the parts only parked executors use now (`partcache::parked_evictable`).
    pub parked_gate: bool,
}

/// Memory step 6a: the re-use windows (seconds) of [`ReuseCensus::reused`].
pub const REUSE_WINDOWS: [u32; 3] = [60, 300, crate::partcache::REUSE_WINDOW_S];

/// An RAII pin: while alive, the pinned part cannot be evicted.
pub struct PartPin {
    part: Arc<OpenPart>,
}

impl PartPin {
    /// Pin an already-shared part.
    pub fn pin(part: &Arc<OpenPart>) -> PartPin {
        part.pin_count.fetch_add(1, Ordering::Relaxed);
        PartPin { part: part.clone() }
    }

    pub fn part(&self) -> &Arc<OpenPart> {
        &self.part
    }
}

impl Clone for PartPin {
    fn clone(&self) -> PartPin {
        PartPin::pin(&self.part)
    }
}

impl Drop for PartPin {
    fn drop(&mut self) {
        if self.part.pin_count.fetch_sub(1, Ordering::Relaxed) == 1 {
            UNPINS.fetch_add(1, Ordering::Release);
        }
    }
}

pgsync::process_global! {
    /// Pins released to zero, by any part of any registry (P-1 L4a): the janitor's witness that an entry may have become
    /// evictable since a pass that found none (entries enter a registry pinned).
    static UNPINS: AtomicU64 = AtomicU64::new(0);
}

/// The registry. Construct once per process scope and share by reference;
/// budget is runtime-settable (the GUC seam — zero env knobs, charter §11).
pub struct PartRegistry {
    _charge: memledger::Charge,
    state: Mutex<RegState>,
    /// Logical LRU clock (monotone counter).
    clock: AtomicU64,
    budget: AtomicU64,
    /// Cap on live entry count — bounds held kernel fds independently of the
    /// resident-byte budget (zero-resident entries still each hold one fd).
    max_entries: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
    /// Memory step 6a: the segment bytes the entries the janitor removed still held (C2's registry removals).
    evicted_bytes: AtomicU64,
    over_budget_events: AtomicU64,
    /// `UNPINS + 1` as of the start of a janitor pass that found every entry pinned (0: none). While it still matches,
    /// nothing is evictable, so the pass is skipped: O(1) where it was O(entries) at every open (P-1 L4a).
    all_pinned_at: AtomicU64,
    /// D3110 (b): the parts a holder keeps open beside the entries ([`Self::set_held_parts`]); none when unset.
    held: Mutex<Option<HeldParts>>,
    /// D4029 (d): the in-use parts `in_use_by_account` gathers (`Arc` clones into a buffer sized at construction): the
    /// memory watchdog's victim pick runs it, so it allocates nothing. Past the capacity the rest go uncounted.
    in_use: Mutex<Box<[Option<Arc<OpenPart>>]>>,
    /// D4802: the content trims' real evictions since start, `[runs, bytes]`: the trims that took bytes out of the census
    /// and those bytes, every tier's, a part's faces only as they left it (a face a running statement holds stays; each
    /// trim that reaches the faces tier counts it again in its return and `partcache::counters`).
    trims_real: [AtomicU64; 2],
    /// D4802: the faces tier since start, `[runs, bytes counted as freed, bytes of those that left the census]`.
    trims_faces: [AtomicU64; 3],
}

/// D3110 (b): a holder's parts, each passed to the visitor (the holder's lock held across it).
pub type HeldParts = fn(&mut dyn FnMut(&Arc<OpenPart>));

const IN_USE_CAP: usize = 8192;

impl PartRegistry {
    pub fn new(budget_bytes: u64) -> PartRegistry {
        PartRegistry {
            state: Mutex::new(RegState {
                parts: BTreeMap::new(),
            }),
            clock: AtomicU64::new(1),
            budget: AtomicU64::new(budget_bytes),
            max_entries: AtomicU64::new(DEFAULT_MAX_ENTRIES),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
            evicted_bytes: AtomicU64::new(0),
            over_budget_events: AtomicU64::new(0),
            all_pinned_at: AtomicU64::new(0),
            held: Mutex::new(None),
            in_use: Mutex::new(vec![None; IN_USE_CAP].into_boxed_slice()), // alloc: Cold charged crate::partcache::OWNER (the charge below)
            _charge: crate::partcache::holder_charge((IN_USE_CAP * size_of::<Option<Arc<OpenPart>>>()) as u64),
            trims_real: core::array::from_fn(|_| AtomicU64::new(0)),
            trims_faces: core::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    /// D3110 (b): `f` names the parts a holder keeps open beside the entries (pgrcolumnar's catalog: its generations' parts).
    /// A relcache invalidation of a relation (VACUUM / ANALYZE's relstats update) takes the relation's keys out of the
    /// entries while its generations keep the parts open, and the scans read the generations' instances; the content trim
    /// and the in-use floor walk both ([`Self::cache_parts`]), so the budget bounds those parts too.
    pub fn set_held_parts(&self, f: HeldParts) {
        *lock(&self.held) = Some(f);
    }

    /// D3110 (b): the part cache's parts, each instance once: the entries and the parts the holder names
    /// ([`Self::set_held_parts`]). `Arc` clones taken under the registry's lock, then the holder's, neither held after.
    /// D4398: on the memory ladder's pass, none while another thread holds the registry (the pass leaves the cache out).
    pub fn cache_parts(&self) -> Vec<Arc<OpenPart>> {
        let Some(st) = memledger::ladder_pass::lock_or_skip(&self.state) else { return Vec::new() };
        let mut parts: Vec<Arc<OpenPart>> = st.parts.values().cloned().collect();
        drop(st);
        let Some(held) = memledger::ladder_pass::lock_or_skip(&self.held).map(|g| *g) else { return Vec::new() };
        if let Some(f) = held {
            f(&mut |p| parts.push(Arc::clone(p)));
            // bounded("other: the cache's parts by address, the entries (at most max_entries, 4096 by default) and the holder's open generations' parts; one sort of pointers, about a millisecond at 50k parts")
            parts.sort_unstable_by_key(|p| Arc::as_ptr(p) as usize);
            parts.dedup_by(|a, b| Arc::ptr_eq(a, b));
        }
        parts
    }

    pub fn set_budget(&self, bytes: u64) {
        self.budget.store(bytes, Ordering::Relaxed);
    }

    pub fn budget(&self) -> u64 {
        self.budget.load(Ordering::Relaxed)
    }

    /// Runtime-settable cap on live entry count (the fd-bound GUC seam). A
    /// value of 0 is clamped to 1 so the cache always holds at least the entry
    /// it just opened. Lowering it makes the next janitor pass reclaim
    /// LRU-unpinned entries down to the new cap.
    pub fn set_max_entries(&self, n: u64) {
        self.max_entries.store(n.max(1), Ordering::Relaxed);
    }

    pub fn max_entries(&self) -> u64 {
        self.max_entries.load(Ordering::Relaxed)
    }

    /// Open-or-share a part, returning it pinned. `open_io` opens the file
    /// and gathers stat facts; on a registry hit the freshly opened io is
    /// dropped without a single content read.
    pub fn open_pinned<F>(&self, expect: &PartExpect, open_io: F) -> ReadResult<PartPin>
    where
        F: FnOnce() -> ReadResult<Box<dyn PartIo>>,
    {
        // D4552 (a): the io, the part's own lists and its `Arc` block on the part cache's holder as they are allocated
        // (a hit's io, freed in the scope, gives its bytes back): the part carries the charge, so the thread that drops it
        // last, wherever it runs, frees at net zero
        let scope = memledger::arena::cache_scope_for(crate::partcache::OWNER, memledger::Kind::Region);
        let io = open_io()?;
        let (dev, ino) = io.dev_ino();
        let key: PartKey = (dev, ino, io.len());
        if let Some(existing) = self.lookup_touch(key) {
            // GENID's rule (crate doc): an entry keyed on `(dev, ino, len)` that no longer holds its file open (the fd
            // pool closes idle part fds, D2873) is checked on every hit. Another modification time is another file at
            // the reused inode, or the same part rewritten in place: either way the entry is not this open's, so it
            // goes and the open parses the file it has (equal identity still shares; a miss is always correct).
            if existing.io_mtime() == io.mtime() {
                self.hits.fetch_add(1, Ordering::Relaxed);
                drop(io);
                drop(scope);
                return Ok(PartPin::pin(&existing));
            }
            self.forget(&existing);
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        let part = Arc::new(OpenPart::open(io, expect)?);
        part.carry_own(scope.finish());
        let mut st = lock(&self.state);
        // the map's node in the part cache's account too: the janitor or an invalidation frees it on any thread
        let entry = {
            let _pc = crate::partcache::home().enter();
            st.parts.entry(key).or_insert_with(|| part).clone()
        };
        // (A racing open of the same identity may have inserted first — its
        // entry wins; equal identity ⇒ identical bytes makes ours a benign
        // duplicate that drops here.)
        self.touch(&entry);
        let pin = PartPin::pin(&entry);
        self.janitor(&mut st);
        drop(st);
        self.trim_if_over();
        Ok(pin)
    }

    /// Shared lookup without opening (no pin; touches LRU).
    pub fn get(&self, key: PartKey) -> Option<Arc<OpenPart>> {
        self.lookup_touch(key)
    }

    /// Drop the entry for `key` (relcache-invalidation seam: closes the
    /// ino-reuse hole at the AM layer). In-flight holders are unaffected.
    pub fn invalidate(&self, key: PartKey) -> bool {
        // the node freed where it was allocated (`open_pinned`), the part dropped outside
        let p = {
            let mut st = lock(&self.state);
            let _pc = crate::partcache::home().enter();
            st.parts.remove(&key)
        };
        p.is_some()
    }

    /// Drop `part`'s entry if the registry still holds that very part (a retired part: its file is unlinked, so no
    /// open can hit it again, and the entry would hold its fd, and the file's disk space, until eviction). Holders
    /// of the `Arc` keep it and its fd; an entry a later open put under the same key stays.
    pub fn forget(&self, part: &Arc<OpenPart>) -> bool {
        let ident = part.ident();
        let key: PartKey = (ident.dev, ident.ino, ident.len);
        let mut st = lock(&self.state);
        match st.parts.get(&key) {
            Some(p) if Arc::ptr_eq(p, part) => {
                // the node freed where it was allocated (`open_pinned`), the entry's `Arc` dropped outside
                let removed = {
                    let _pc = crate::partcache::home().enter();
                    st.parts.remove(&key)
                };
                drop(st);
                drop(removed);
                true
            }
            _ => false,
        }
    }

    /// Drop every entry (DROP TABLE / testing).
    pub fn clear(&self) {
        let mut st = lock(&self.state);
        // bounded("other: the registry's entries (at most max_entries, 4096 by default), a node a step; DROP TABLE and tests")
        loop {
            // each node freed where it was allocated (`open_pinned`), each part dropped outside
            let next = {
                let _pc = crate::partcache::home().enter();
                st.parts.pop_first()
            };
            match next {
                Some(e) => drop(e),
                None => break,
            }
        }
    }

    /// Run the janitor now (callers with fault-heavy phases between opens), and the D3003 content trim when the
    /// process part cache is over its budget.
    pub fn maintain(&self) {
        {
            let mut st = lock(&self.state);
            self.janitor(&mut st);
        }
        self.trim_if_over();
    }

    /// D3003: [`Self::trim_contents`] down to `partcache::own_trim_target` when the part cache is over its budget (one
    /// relaxed load pair when it is not; ladder C1b: while its floor holds it, past its cushion only,
    /// `partcache::own_cushion`).
    pub fn trim_if_over(&self) -> u64 {
        if !crate::partcache::over_budget() {
            return 0;
        }
        match crate::partcache::own_cushion() {
            Some(cushion) => self.trim_contents_with(cushion, false),
            None => self.trim_contents(crate::partcache::own_trim_target()),
        }
    }

    /// D3003: evict the cached contents of the parts no statement uses until the process part cache is at most `target`
    /// bytes: first the streams each part's last statement did not open (`OpenPart::evict_stale_streams`, D3109), then
    /// whole parts in use-stamp order (`partcache::stamp`: older statements' parts first, a statement's latest first
    /// touches first, D3109), then the extents the scans of parts in use read past (D4315). Pins keep entries, never contents (the catalog pins every live part). Runs
    /// with no registry or part lock held across parts; the evicted hook runs outside every lock: after, and between
    /// the passes when shared images keep the cache past the target (D3113); the short hook when parts in use do (D3268).
    /// Returns the bytes evicted (trim_real_frees: those that left memory, D4802).
    /// D4659: every break check takes the lower of this trim's own count and the cache's census ([`left`]). Two callers
    /// past the gate together (a statement start and a worker's part touch, two rt workers, C3's purger) each counted only
    /// their own evictions from the same snapshot and together took the cache about twice as far under the target (986 +
    /// 988 MiB in one millisecond on an 8-stream scan run; 3647 + 5048 MiB with the purger): now concurrent trims share
    /// the eviction and each stops once the cache is at the target, at worst one part each past it. A part whose images
    /// another clone still holds (D3113) frees nothing: it adds nothing to the count and the walk moves on (each part
    /// once per pass); the census can only end a trim earlier, never make it walk again.
    pub fn trim_contents(&self, target: u64) -> u64 {
        self.trim_contents_with(target, true)
    }

    /// [`Self::trim_contents`]; `deep` false (ladder C1b, an own trim bounded by the floor's cushion): only the unused
    /// parts' streams and contents and the extents no reader of theirs reads again, never the evicted hook's list
    /// drop, the extents other sessions' readers have yet to reach, the short hook's parked cursors or the faces.
    fn trim_contents_with(&self, target: u64, deep: bool) -> u64 {
        let mut step = cancelpoll::step("pgrc_read PartRegistry::trim_contents: a part's contents");
        // D3110 (b): every part of the cache, the holder's too (`cache_parts`), keyed by its identity
        let mut parts: Vec<(u64, PartKey, Arc<OpenPart>)> = self
            .cache_parts()
            .into_iter()
            .map(|p| {
                let id = p.ident();
                (p.cache_used(), (id.dev, id.ino, id.len), p)
            })
            .collect();
        // bounded("other: the cache's parts (cache_parts: the entries, at most max_entries, 4096 by default, and the holder's open generations' parts) by use stamp; one sort of integer keys, about a millisecond at 50k parts, and the walks below tick per part")
        parts.sort_by_key(|(u, k, _)| (*u, *k));
        let mut have = crate::partcache::bytes();
        let (mut n, mut freed, mut dict) = (0u64, 0u64, 0u64);
        // D3994 (B.1 W): whether the parts only parked executors use (their kept cursors hold none of their images) go as
        // unused ones do, the memory policy's gate, read once per trim
        let parked = crate::partcache::parked_evictable();
        // D3109: first the streams each unused part's last statement did not open, then whole parts
        for (_, _, p) in &parts {
            // D3887: per part, the cancel of the statement whose part touch runs the trim (a trim under pressure walks
            // thousands of parts for seconds); the cache stays over its target for the next trim to finish
            if left(&mut have) <= target || step.tick().is_err() {
                break;
            }
            if !evictable(p, parked) {
                continue;
            }
            let s = p.evict_stale_streams();
            if s > 0 {
                n += 1;
                freed += s;
                have = have.saturating_sub(s);
            }
        }
        let shared = Self::evict_unused(&mut step, &parts, target, parked, &mut have, &mut n, &mut freed, &mut dict);
        // D3113: the images still past the target are held by the hook owner's handles (Gravity's part lists): the hook
        // drops them, then the unused parts give those images back in the same order. Those a part already let go of
        // (owed, the line's class: a stale stream's here, a cooling eviction's, a dropped part's) free at that drop.
        if deep && left(&mut have) > target && (shared > 0 || crate::partcache::owed_since_hook()) && crate::partcache::run_evicted_hook() {
            have = crate::partcache::bytes();
            Self::evict_unused(&mut step, &parts, target, parked, &mut have, &mut n, &mut freed, &mut dict);
        }
        // D4315: the parts in use give back the extents their scans read past (no reader of theirs reads them again), in
        // the same order, before any use is let go. D4351: first those no live reader of their stream is still behind,
        // then (still over the target) the ones another session's reader has yet to reach: it faults them again
        if left(&mut have) > target {
            let poison = crate::partcache::park_poison();
            // bounded("other: the trim's two tiers")
            for ahead in [false, true] {
                if ahead && !deep {
                    break;
                }
                for (_, _, p) in &parts {
                    if left(&mut have) <= target || step.tick().is_err() {
                        break;
                    }
                    let s = if ahead { p.evict_passed_ahead(poison) } else { p.evict_passed(poison) };
                    if s > 0 {
                        n += 1;
                        freed += s;
                        have = have.saturating_sub(s);
                    }
                }
            }
        }
        // D3268: parts in use kept the cache past the target. The short hook's owner lets go the uses no running statement
        // needs (a parked worker set's kept cursor pins its whole part: every column's cached streams), then the parts no
        // longer in use give their contents back in the same order.
        if deep && left(&mut have) > target && !cancelpoll::seen() && parts.iter().any(|(_, _, p)| !evictable(p, parked)) && crate::partcache::run_short_hook() {
            have = crate::partcache::bytes();
            let shared = Self::evict_unused(&mut step, &parts, target, parked, &mut have, &mut n, &mut freed, &mut dict);
            if left(&mut have) > target && shared > 0 && crate::partcache::run_evicted_hook() {
                have = crate::partcache::bytes();
                Self::evict_unused(&mut step, &parts, target, parked, &mut have, &mut n, &mut freed, &mut dict);
            }
        }
        // D4504: metadata goes after its data. Still over the target, the parts no statement uses let their faces go in the
        // same order (the part lists' Stats walk, bloom and PSMA bodies, `OpenPart::evict_faces`); the faces hook's owner
        // drops the stale holds, and their bytes leave the census as the last statement holding each ends
        let (mut held_faces, mut face_parts, mut face_parts_gone) = (0u64, 0u64, 0u64);
        if deep && left(&mut have) > target {
            let mut faces = 0u64;
            // D4802: each walked part's faces let go, in its use stamp's place (the sort spent it)
            let mut walked = 0;
            for (u, _, p) in parts.iter_mut() {
                if left(&mut have) <= target || step.tick().is_err() {
                    break;
                }
                walked += 1;
                *u = 0;
                if !evictable(p, parked) {
                    continue;
                }
                let s = p.evict_faces();
                if s > 0 {
                    n += 1;
                    face_parts += 1;
                    faces += s;
                    have = have.saturating_sub(s);
                    *u = s;
                }
            }
            if faces > 0 && crate::partcache::run_faces_hook() {
                freed += faces;
                have = crate::partcache::bytes();
                // D4802: what left the census at the hook, part by part; a running statement's holds keep the rest, which
                // the next trim to reach this tier counts again
                let gone_each = || parts[..walked].iter().map(|(u, _, p)| u.saturating_sub(p.face_bytes()));
                let gone = gone_each().sum::<u64>().min(faces);
                face_parts_gone = gone_each().filter(|g| *g > 0).count() as u64;
                held_faces = faces - gone;
                self.trims_faces[0].fetch_add(1, Ordering::Relaxed);
                self.trims_faces[1].fetch_add(faces, Ordering::Relaxed);
                self.trims_faces[2].fetch_add(gone, Ordering::Relaxed);
            }
        }
        drop(parts);
        crate::partcache::note_result(have, target);
        // D4802: the bytes that left memory; the faces a running statement's holds keep leave as its last holder drops
        // them, and the next trim to reach the faces tier counts them again
        let real = freed.saturating_sub(held_faces);
        // trim_real_frees: the return, the trim counters and the fire signal take those bytes, and the parts whose faces
        // left; off, the faces tier's let-go bytes and parts count as evicted
        let (n, freed) = if mempolicy::consts::get(mempolicy::consts::K::TrimRealFrees) != 0 {
            (n - face_parts + face_parts_gone, real)
        } else {
            (n, freed)
        };
        if n > 0 {
            // shapecheck fire signal (D3109): a content trim evicted parts, in the epoch order `partcache::stamp` keys
            fires::fire_shared("partcache.trim");
            crate::partcache::note_trim(n, freed, dict > 0);
        }
        if real > 0 {
            self.trims_real[0].fetch_add(1, Ordering::Relaxed);
            self.trims_real[1].fetch_add(real, Ordering::Relaxed);
        }
        freed
    }

    /// LOAD GATE (LADDER-DESIGN section 3): room for a load of `n` bytes into the process part cache at its budget. The
    /// elastic bytes go first: the parts no running statement uses give their contents back (`trim_contents`, its order
    /// and hooks) down to the budget less the load, unless a failed trim's level stands (`partcache::load_trim_due`).
    /// Returns the bytes the cache is still over that level: bytes running statements pin, which only they let go.
    /// Ladder C1b, with a step: while the cache's floor holds its own trims, the unused parts past the cushion only
    /// (`partcache::own_cushion`: the floor plus one step) and no wait (the load goes on); else at most one step shed
    /// per load (`partcache::own_level`), the level then that step.
    pub fn load_room(&self, n: u64) -> u64 {
        let have = crate::partcache::bytes();
        if let Some(cushion) = crate::partcache::own_cushion() {
            if have > cushion && crate::partcache::load_trim_due(have.saturating_add(n)) {
                let freed = self.trim_contents_with(cushion, false);
                crate::partcache::note_load_trim(freed);
            }
            crate::partcache::note_load_held();
            return 0;
        }
        let target = crate::partcache::own_level(have, mempolicy::cache::part_cache_load_target(crate::partcache::budget(), n));
        if have <= target {
            return 0;
        }
        if crate::partcache::load_trim_due(have.saturating_add(n)) {
            let freed = self.trim_contents(target);
            crate::partcache::note_load_trim(freed);
        }
        crate::partcache::bytes().saturating_sub(target)
    }

    /// A content trim's whole-part pass: the parts no statement uses, in `parts`' order, give their contents back until the
    /// cache is at most `target` (`have` the bytes left; the counts of `trim_contents`). Returns the bytes of images a holder
    /// still shares (`OpenPart::evict_contents`).
    #[allow(clippy::too_many_arguments)]
    fn evict_unused(step: &mut cancelpoll::Step, parts: &[(u64, PartKey, Arc<OpenPart>)], target: u64, parked: bool, have: &mut u64, n: &mut u64, freed: &mut u64, dict: &mut u64) -> u64 {
        let mut shared = 0u64;
        let poison = parked && crate::partcache::park_poison();
        for (_, _, p) in parts {
            if left(have) <= target || step.tick().is_err() {
                break;
            }
            if !evictable(p, parked) {
                continue;
            }
            // D3994 (B.1 W) under D3823's check: a parked-only part's images freed are poisoned first
            let (s, d, k) = if poison && p.users() != 0 { p.evict_contents_poisoned() } else { p.evict_contents() };
            shared += k;
            if s + d > 0 {
                *n += 1;
                *freed += s + d;
                *dict += d;
                *have = have.saturating_sub(s + d);
            }
        }
        shared
    }

    /// Memory step 5 (lane membroker): the part cache's floor, the pinned bytes (`OpenPart::pinned_bytes`: segments and
    /// part-grain dictionary images, less the extents a scan read past, D4315) of the parts a live claim holds now
    /// (`OpenPart::live_claimed`). Walks the
    /// parts (each part's state lock once) with no registry lock held across them: for the memory budget's snapshots,
    /// never per statement.
    pub fn in_use_bytes(&self) -> u64 {
        // D3110 (b): the holder's parts too, the ones the scans read after a relcache invalidation. D3994 (B.1 W): a part
        // only parked executors use is not in the floor: none of its images is theirs and a trim under the memory policy's
        // gate evicts it, so the cache's floor (the broker's, B0's cache_kept) is what running owners read. D4315: nor is a
        // part only the statements that deferred it hold (D3823's gate: every claim left it): it is reclaimable, and in the
        // floor it lifted the broker's target, the level its own release waits on, so it went back only at the statement's end
        let mut sum = 0u64;
        self.for_each_in_use(&mut |p| {
            if p.live_claimed() {
                sum = sum.saturating_add(p.pinned_bytes());
            }
        });
        sum
    }

    /// D4910 step 1: the in-use parts' pins by what holds them and what they are, one walk with no registry lock held across
    /// the parts (as `in_use_bytes`; the reconcile view's, never per statement).
    pub fn pin_breakdown(&self) -> PinBreakdown {
        let mut b = PinBreakdown::default();
        self.for_each_in_use(&mut |p| {
            let live = p.live_users();
            if live == 0 {
                return;
            }
            b.parts_live += 1;
            b.users_max = b.users_max.max(u64::from(live));
            if !p.live_claimed() {
                b.parts_deferred += 1;
                b.deferred_pinned = b.deferred_pinned.saturating_add(p.pinned_bytes());
                return;
            }
            b.parts_claimed += 1;
            let (cache, images, passed) = (p.cache_bytes(), p.resident(), p.passed_bytes());
            let pinned = cache.saturating_sub(passed);
            b.images = b.images.saturating_add(images);
            b.passed = b.passed.saturating_add(passed);
            b.dicts = b.dicts.saturating_add(cache.saturating_sub(images));
            b.pinned = b.pinned.saturating_add(pinned);
            if passed >= images {
                b.parts_fully_passed += 1;
                b.fully_passed_pinned = b.fully_passed_pinned.saturating_add(pinned);
            }
        });
        b
    }

    /// D4910 / D4908: the cache's bytes its trim cannot drop now (each part's `cache_bytes` less its `trimmable_now`, by
    /// the test `trimmable_bytes` counts with), split by what holds the part, with the parked gate's answer read once (the
    /// reconcile view's, never per statement).
    pub fn untrimmable_by_holder(&self) -> UntrimmableByHolder {
        let parked = crate::partcache::parked_evictable();
        let mut u = UntrimmableByHolder { parked_gate: parked, ..UntrimmableByHolder::default() };
        // bounded("other: the reconcile view's walk over the cache's parts, at most max_entries (4096 by default) and the holder's; two part locks each")
        for p in self.cache_parts().iter() {
            let stuck = p.cache_bytes().saturating_sub(p.trimmable_now(evictable(p, parked)));
            let held = if p.users() == 0 {
                &mut u.none
            } else if p.live_users() == 0 {
                &mut u.parked
            } else if p.live_claimed() {
                &mut u.live
            } else {
                &mut u.deferred
            };
            *held = held.saturating_add(stuck);
        }
        u
    }

    /// D4353: the bytes a content trim (`trim_contents`) could free now, the ladder's R2a count for the part cache: by the
    /// trim's own test, an evictable part's cached bytes, and a part in use's extents its scans read past. Not the cached
    /// bytes less `in_use_bytes`: that floor leaves out the deferred and the parked-only parts no trim may evict.
    /// D4407: and of those bytes, only the images the trim frees now (`OpenPart::trimmable_now`): one another clone holds
    /// stays cached (D3113), so counting it made R2a promise gigabytes it could not give (take 3: 99.8 % of the unused
    /// parts' bytes shared; the evicted hook's pass freed 44 MB in 281 runs, so its yield is not counted).
    pub fn trimmable_bytes(&self) -> u64 {
        let parked = crate::partcache::parked_evictable();
        self.cache_parts().iter().map(|p| p.trimmable_now(evictable(p, parked))).sum()
    }

    /// D3999: the in-use parts' cached bytes by their users' memory ledger accounts, `f(account, bytes)` once per part and
    /// account: split by uses (the views), or `alone`, a part only on its one account, whichever of its owners holds the
    /// uses (the backstop ranks by `in_use_by_running`). D4029 (d): the in-use parts
    /// gathered into `in_use`, sorted and deduplicated in place: no allocation (a busy buffer takes `cache_parts`).
    pub fn in_use_by_account(&self, alone: bool, mut f: impl FnMut(u32, u64)) {
        self.for_each_in_use(&mut |p| by_account(p, alone, &mut f));
    }

    /// D3204: the in-use parts' cached bytes by their users' owners, `f(account, owner key, bytes)` once per part and owner,
    /// a part's bytes split by its owners' uses (the views' split of the cache's in-use floor by holder). No allocation.
    pub fn in_use_by_owner(&self, mut f: impl FnMut(u32, u64, u64)) {
        self.for_each_in_use(&mut |p| {
            if p.users() == 0 {
                return;
            }
            let bytes = p.pinned_bytes();
            p.with_user_accounts(|u| {
                let uses: u64 = u.iter().map(|e| e.2 as u64).sum();
                if uses != 0 {
                    // bounded("other: one entry per owner using this part, a few per running statement (max_connections bounds the statements)")
                    for &(a, key, n) in u {
                        f(a, key, bytes.saturating_mul(n as u64) / uses);
                    }
                }
            });
        });
    }

    /// C8 (D4087, the per-owner rank): the backstop's ranking: the in-use parts' cached bytes on the account whose running
    /// statement and the portal it executes own every use (`mempolicy::watchdog::cancel_frees`): what canceling it lets the
    /// cache evict. A session's kept cursors (D1507), a parked worker set's, an ended owner's and an exited backend's uses
    /// count on no account. No allocation (D4029 (d)).
    pub fn in_use_by_running(&self, mut f: impl FnMut(u32, u64)) {
        self.for_each_in_use(&mut |p| {
            let running = |a| memledger::charge::running_owners(memledger::Acct(a));
            if let Some(a) = p.with_user_accounts(|u| mempolicy::watchdog::cancel_frees(u, running)) {
                f(a, p.pinned_bytes());
            }
        });
    }

    /// `f` over each in-use part of the cache once (the entries and the holder's), from `in_use` with no allocation.
    fn for_each_in_use(&self, f: &mut dyn FnMut(&OpenPart)) {
        let Ok(mut buf) = self.in_use.try_lock() else {
            // bounded("other: the memory budget's walk over the cache's in-use parts (never per statement, as in_use_bytes says); a part only while a running statement uses it")
            for p in self.cache_parts().iter().filter(|p| p.users() != 0) {
                f(p);
            }
            return;
        };
        let mut n = 0;
        let mut keep = |p: &Arc<OpenPart>| {
            if let Some(slot) = buf.get_mut(n).filter(|_| p.users() != 0) {
                *slot = Some(Arc::clone(p));
                n += 1;
            }
        };
        // bounded("other: the memory budget's walk over the registry's entries, at most max_entries (4096 by default); a users() load each")
        for p in lock(&self.state).parts.values() {
            keep(p);
        }
        let held = *lock(&self.held);
        if let Some(h) = held {
            h(&mut keep);
        }
        let parts = buf.get_mut(..n).unwrap_or_default();
        // bounded("other: the in-use parts that fit the preallocated in_use buffer, sorted by address; the buffer's length bounds them")
        parts.sort_unstable_by_key(|p| p.as_ref().map_or(0, |p| Arc::as_ptr(p) as usize));
        let mut prev = core::ptr::null();
        // bounded("other: the in-use parts that fit the preallocated in_use buffer, one call of the memory budget's visitor each")
        for slot in parts.iter_mut() {
            if let Some(p) = slot.take().filter(|p| Arc::as_ptr(p) != prev) {
                prev = Arc::as_ptr(&p);
                f(&p);
            }
        }
    }

    /// Memory step 6a: `reuse_census_of` over the part cache's parts (`cache_parts`: the entries and the holder's).
    pub fn reuse_census(&self, now: u32) -> ReuseCensus {
        reuse_census_of(&self.cache_parts(), now)
    }

    /// Broker (b): publish the part cache's tail, the use stamp at its coldest tenth of cached bytes in the content trim's
    /// order (ascending use stamp, D3109), and the bytes it covers. For the broker's plan on the memory watchdog's thread:
    /// no allocation and no wait (`memledger::value::coldest_tenth` over the registry's parts under `try_lock`; busy, the
    /// last mark stays). A part counts its segment bytes (`resident`, an atomic: its dictionary images are summed under
    /// the part's own lock); the holder's parts (`cache_parts`) are left out (its hook returns a Vec).
    pub fn publish_tail(&self) {
        let Some(st) = memledger::value::try_lock(&self.state) else { return };
        let tail = memledger::value::coldest_tenth(st.parts.len(), |f| {
            // bounded("other: on the memory watchdog's thread, outside every statement's kernel call; the registry's entries, at most max_entries (4096 by default)")
            for p in st.parts.values() {
                let b = p.resident();
                if b != 0 {
                    f.push(p.cache_used(), b);
                }
            }
        });
        drop(st);
        if let Some((stamp, bytes)) = tail {
            crate::partcache::set_tail_stamp(stamp);
            memledger::value::set_tail_bytes(memledger::Consumer::PartCache, bytes);
        }
    }

    /// Memory step 6a: `(entries, segment bytes)` the janitor removed (a removed part's re-use history goes with it, so the
    /// census is a lower bound while these grow).
    pub fn removed(&self) -> (u64, u64) {
        (self.evictions.load(Ordering::Relaxed), self.evicted_bytes.load(Ordering::Relaxed))
    }

    /// D4802: the content trims' real evictions since start: `(trims that took bytes out of the census, bytes)`, a part's
    /// faces counted once, as they leave it.
    pub fn real_evictions(&self) -> (u64, u64) {
        (self.trims_real[0].load(Ordering::Relaxed), self.trims_real[1].load(Ordering::Relaxed))
    }

    /// D4802: the content trims' faces tier since start: `(trims that let faces go, bytes counted as freed, bytes of those
    /// that left the census)`; the difference is the faces running statements held, counted again at each trim.
    pub fn faces_trims(&self) -> (u64, u64, u64) {
        let f = |i: usize| self.trims_faces[i].load(Ordering::Relaxed);
        (f(0), f(1), f(2))
    }

    /// Total segment-cache bytes across live entries.
    pub fn resident_bytes(&self) -> u64 {
        lock(&self.state).parts.values().map(|p| p.resident()).sum()
    }

    pub fn len(&self) -> usize {
        lock(&self.state).parts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn counters(&self) -> RegistryCounters {
        RegistryCounters {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            over_budget_events: self.over_budget_events.load(Ordering::Relaxed),
        }
    }

    fn lookup_touch(&self, key: PartKey) -> Option<Arc<OpenPart>> {
        let st = lock(&self.state);
        let p = st.parts.get(&key)?.clone();
        drop(st);
        self.touch(&p);
        Some(p)
    }

    fn touch(&self, part: &Arc<OpenPart>) {
        let now = self.clock.fetch_add(1, Ordering::Relaxed);
        part.last_used.store(now, Ordering::Relaxed);
    }

    /// Evict LRU unpinned entries while over EITHER budget: resident bytes over
    /// the byte budget, OR live entry count over the entry cap. The count cap
    /// bounds held kernel fds independently of resident bytes — a zero-resident
    /// entry exerts no byte pressure but still pins one fd (and, for an unlinked
    /// part, its inode), so byte-only eviction would let such entries (aborted
    /// self-publish self-scans, zero-row scans) accumulate without bound.
    /// Deterministic: the victim is min by (last_used, key) over a BTreeMap
    /// (stable order).
    ///
    /// A pass that finds every entry pinned is remembered until a pin drops to zero anywhere ([`UNPINS`]): entries enter
    /// pinned (`open_pinned`), so until then nothing is evictable and the next passes return at once, witnessing the
    /// entry cap's overrun (the byte sum is not taken). A table whose generation pins more parts than the cap (10,000 in
    /// the P-1 bench) paid two walks of every entry per part opened.
    fn janitor(&self, st: &mut RegState) {
        let budget = self.budget.load(Ordering::Relaxed);
        let max_entries = self.max_entries.load(Ordering::Relaxed) as usize;
        let unpins = UNPINS.load(Ordering::Acquire);
        if self.all_pinned_at.load(Ordering::Relaxed) == unpins + 1 {
            if st.parts.len() > max_entries {
                self.over_budget_events.fetch_add(1, Ordering::Relaxed);
            }
            return;
        }
        // an eviction a tick: a cancel leaves the registry over its caps for the next pass (an open's or `maintain`'s)
        let mut step = cancelpoll::step("pgrc_read PartRegistry::janitor: an LRU eviction");
        loop {
            if step.tick().is_err() {
                return;
            }
            let total: u64 = st.parts.values().map(|p| p.resident()).sum();
            if total <= budget && st.parts.len() <= max_entries {
                return;
            }
            let victim = st
                .parts
                .iter()
                .filter(|(_, p)| p.pin_count.load(Ordering::Relaxed) == 0)
                .min_by_key(|(k, p)| (p.last_used.load(Ordering::Relaxed), **k))
                .map(|(k, _)| *k);
            match victim {
                Some(k) => {
                    let removed = {
                        let _pc = crate::partcache::home().enter();
                        st.parts.remove(&k)
                    };
                    if let Some(p) = removed {
                        self.evicted_bytes.fetch_add(p.resident(), Ordering::Relaxed);
                    }
                    self.evictions.fetch_add(1, Ordering::Relaxed);
                }
                None => {
                    // Everything resident is pinned: run over budget,
                    // witnessed.
                    self.over_budget_events.fetch_add(1, Ordering::Relaxed);
                    self.all_pinned_at.store(unpins + 1, Ordering::Relaxed);
                    return;
                }
            }
        }
    }
}

/// Memory step 6a: the re-use census of `parts` at broker second `now` (`partcache::now_sec`): each part's NEED summed per
/// window its statement use before the latest falls in, and the file bytes its streams opened inside 900 s. Each part's
/// state lock once, no registry lock held: the broker's, every interval. `parts` names each instance once
/// ([`PartRegistry::cache_parts`]).
fn reuse_census_of(parts: &[Arc<OpenPart>], now: u32) -> ReuseCensus {
    let mut c = ReuseCensus { parts: parts.len() as u64, ..ReuseCensus::default() };
    let since = now.saturating_sub(crate::partcache::REUSE_WINDOW_S);
    // bounded("other: the broker's census on the memory watchdog's thread, outside every statement's kernel call; one pass over the cache's parts (cache_parts)")
    for p in parts {
        let (last, prev) = p.reuse_secs();
        if prev != 0 && prev >= since {
            let need = p.need_bytes();
            // bounded("const-domain: REUSE_WINDOWS, the census's fixed windows")
            for (i, w) in REUSE_WINDOWS.iter().enumerate() {
                if now.saturating_sub(prev) <= *w {
                    c.reused[i] += need;
                    c.reused_parts[i] += 1;
                }
            }
        }
        if last != 0 && last >= since {
            c.used_parts_900 += 1;
            c.file_bytes_900 += p.stream_file_bytes_since(since);
        }
    }
    c
}

/// D4659: the bytes a content trim still has to go below: its own count of what it evicted, or the part cache's census
/// when that is lower (bytes another trimmer or a statement's end let go meanwhile). Never above the count, so one trimmer
/// alone evicts no more than before.
#[inline]
fn left(have: &mut u64) -> u64 {
    *have = (*have).min(crate::partcache::bytes());
    *have
}

/// D3994 (B.1 W): whether a content trim may evict `p` now: no use, or (`parked`, the memory policy's gate) uses only
/// parked executors hold, whose kept cursors hold none of its images and read nothing until a statement binds them again.
fn evictable(p: &OpenPart, parked: bool) -> bool {
    p.users() == 0 || (parked && p.live_users() == 0)
}

/// `in_use_by_account`'s one part: its cached bytes by its users' accounts (split by uses, or `alone` on its one user).
fn by_account(p: &OpenPart, alone: bool, f: &mut impl FnMut(u32, u64)) {
    let counted = |u: &[(u32, u64, u32)]| {
        let uses: u64 = u.iter().map(|e| e.2 as u64).sum();
        (uses != 0 && (!alone || u.iter().all(|e| e.0 == u[0].0))).then_some(uses)
    };
    if p.users() == 0 || p.with_user_accounts(counted).is_none() {
        return;
    }
    let bytes = p.pinned_bytes();
    p.with_user_accounts(|u| {
        if let Some(uses) = counted(u) {
            // once per account, its uses under every owner summed
            // bounded("other: one entry per account and owner using this part, a few per running statement (max_connections bounds the statements); the scans inside are over the same entries")
            for (i, &(a, _, _)) in u.iter().enumerate() {
                if !u[..i].iter().any(|e| e.0 == a) {
                    let n: u64 = u[i..].iter().filter(|e| e.0 == a).map(|e| e.2 as u64).sum();
                    f(a, bytes / uses * n);
                }
            }
        }
    });
}

fn lock<T>(m: &Mutex<T>) -> pgsync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}
