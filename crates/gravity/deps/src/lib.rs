//! Recorded dependencies (C3): what one lowering read from the catalog, each with the generation it had at its first
//! read. The caches (catcache search and list, relcache open, typcache lookup, a catalog's systable scan) note their
//! reads while a `Recording` is armed on this thread; a holder of the lowering's result asks `Deps::current`.
#![forbid(unsafe_code)]

use core::cell::{Cell, RefCell};
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

use types_core::Oid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Dep {
    /// D4010: a setting a read resolved under (`SETTING_*`). Its generation is the value's fingerprint (`note_setting`,
    /// `set_setting_probe`), so a read under one value holds under no other. First in order: `current_since` reads these
    /// at every ask (a SET or a role change moves no view).
    Setting(u32),
    /// A relation's relcache entry: every catalog row a relcache invalidation of it covers.
    Rel(Oid),
    /// One syscache row by its key hash, a negative lookup too: what a catcache invalidation names.
    Cat { cache: i16, hash: u32 },
    /// A list or partial-key search of a syscache, a scan of its catalog, or a cache built from its rows: any change.
    CatList(i16),
    /// C3 S5 (D3889): a relation's storage or size statistics (pg_class relfilenode, relpages, reltuples, relallvisible):
    /// any relcache message of the relation, and one scoped to its storage (TRUNCATE, a REINDEX), which leaves `Rel`.
    Stats(Oid),
}

/// D4010: the search path a name lookup walked, as resolved (namespace OIDs in order: `$user` as the current role
/// expands it, pg_temp's and the implicit pg_catalog's positions).
pub const SETTING_SEARCH_PATH: u32 = 1;

/// A `current_since` mark no check has set.
pub const UNCHECKED: u64 = u64::MAX;

/// The syscaches whose rows a type's ordering or hashing support is built from (pg_amop by operator and by strategy,
/// pg_amproc, pg_opclass, pg_operator, pg_opfamily): no pg_type invalidation covers them. An index's relcache entry and a
/// partition key are built from them too, and their changes send the entry's own relcache message (D3888).
pub const SUPPORT_LISTS: [i16; 6] = [3, 4, 5, 14, 40, 42];

/// `cacheinfo[id].reloid` for every syscache id (PostgreSQL 18.6 genbki order); cache_syscache's tests hold it equal.
pub const SYSCACHE_RELOIDS: [Oid; 85] = [
    2600, 2601, 2601, 2602, 2602, 2603, 1249, 1249, 1261, 1261, 1260, 1260, 2605, 2616, 2616, 3456, 3456, 2607, 2607, 2606, 2607, 1262, 826, 3501,
    3501, 3466, 3466, 3079, 3079, 2328, 2328, 1417, 1417, 3118, 2610, 2612, 2612, 2615, 2615, 2617, 2617, 2753, 2753, 6243, 6243, 3350, 1255, 1255,
    6104, 6237, 6237, 6104, 6106, 6106, 3541, 3541, 1259, 1259, 6000, 6000, 2618, 2224, 3429, 3381, 3381, 2619, 6100, 6100, 6102, 1213, 3576, 3576,
    3603, 3602, 3602, 3600, 3600, 3601, 3601, 3764, 3764, 1247, 1247, 1418, 1418,
];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Deps {
    items: Vec<(Dep, u64)>,
    lagged: bool,
    wraps: Option<u64>,
    /// Read under this thread's own uncommitted DDL: the own-invalidation count then (l2cache `own_seq`); 0: read under
    /// the committed catalog state.
    own: u64,
}

impl Deps {
    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn heap_bytes(&self) -> u64 {
        (self.items.capacity() * core::mem::size_of::<(Dep, u64)>()) as u64
    }

    pub fn iter(&self) -> impl Iterator<Item = Dep> + '_ {
        self.items.iter().map(|e| e.0)
    }

    pub fn contains(&self, d: Dep) -> bool {
        self.items.binary_search_by(|e| e.0.cmp(&d)).is_ok()
    }

    /// `d` is held, or a read of it is (a row of a syscache whose whole cache is held).
    pub fn covers(&self, d: Dep) -> bool {
        self.contains(d) || matches!(d, Dep::Cat { cache, .. } if self.contains(Dep::CatList(cache)))
    }

    /// A view lagged its generation at a read: what was read may straddle a commit this backend has not processed.
    pub fn lagged(&self) -> bool {
        self.lagged
    }

    /// `d` as if read now (an admission's seed: the plan's own dependency lists).
    pub fn add(&mut self, d: Dep) {
        if let Err(i) = self.items.binary_search_by(|e| e.0.cmp(&d)) {
            if self.wraps.is_none() && matches!(d, Dep::Cat { cache, .. } if l2cache::row_stamped(i32::from(cache))) {
                self.wraps = Some(l2cache::oid_wraps());
            }
            let (g, lag) = capture(d);
            self.lagged |= lag;
            self.read_privately(d);
            self.items.insert(i, (d, g)); // alloc: Cold
        }
    }

    /// D4010: setting `key` read at value fingerprint `value`; two values of one key in one recording hold nowhere.
    fn add_setting(&mut self, key: u32, value: u64) {
        let d = Dep::Setting(key);
        match self.items.binary_search_by(|e| e.0.cmp(&d)) {
            Ok(i) => self.lagged |= self.items[i].1 != value,
            Err(i) => self.items.insert(i, (d, value)), // alloc: Cold
        }
    }

    /// C3 S4b: a read while this thread builds privately. Under its own uncommitted DDL, the reads hold while that DDL
    /// stands as it was (`own`: no command end, subtransaction or transaction end since), C3 S5d: those it touches (a
    /// read it does not touch is of the committed state); in a parallel worker or under a historic snapshot, never past
    /// the reading statement (what was read may be another transaction's uncommitted state or a past one).
    fn read_privately(&mut self, d: Dep) {
        if !l2cache::private_build_mode() {
            return;
        }
        if !l2cache::private_by_own_ddl_only() {
            self.lagged = true;
            return;
        }
        // C3 S5d: a read this thread's own DDL did not touch reads the committed state (every change a transaction makes
        // to what a cache holds registers that cache's or that entry's invalidation: inval's summary of them)
        if !touched_by_own_ddl(core::slice::from_ref(&d), true) {
            return;
        }
        let now = l2cache::own_seq();
        if self.own == 0 {
            self.own = now;
        } else if self.own != now {
            self.lagged = true;
        }
    }

    /// Every dependency still has the generation it was read under and this thread's view of it is that generation, and
    /// no view lagged at a read. One generation per relation and per syscache: a cache's rows are read once. C3 S4b:
    /// what was read under this thread's own uncommitted DDL holds while that DDL stands as it was; what was read under
    /// the committed state holds under such DDL only where it touches none of the dependencies (inval
    /// `PendingInvalidationsTouch`); in a parallel worker or under a historic snapshot nothing read elsewhere holds.
    pub fn current(&self) -> bool {
        if self.lagged || self.wraps.is_some_and(|w| w != l2cache::oid_wraps()) {
            return false;
        }
        if self.own != 0 {
            if self.own != l2cache::own_seq() {
                return false;
            }
        } else if l2cache::private_build_mode() && (!l2cache::private_by_own_ddl_only() || self.touched_by_own_ddl()) {
            return false;
        }
        // C3 S5b: every dependency is read; two rows of one cache read at equal generations are not one read (each row
        // holds its own generation, which only its own messages move)
        self.items.iter().all(|&(d, g)| {
            let (now, lag) = capture(d);
            !lag && stands(d, g, now)
        })
    }

    /// D4579: what was read can never stand again in database `db`, by the generations committed there (whatever any
    /// thread's view): a relation's, its storage's or a syscache row's or list's read whose generation moved, or an OID
    /// wrap since. `current` asks at least as much, and a generation never moves back, so a holder that drops what is
    /// superseded loses nothing it could serve. A setting read is not asked (its value can come back).
    pub fn superseded_in(&self, db: Oid) -> bool {
        if self.wraps.is_some_and(|w| w != l2cache::oid_wraps()) {
            return true;
        }
        self.items.iter().any(|&(d, g)| !matches!(d, Dep::Setting(_)) && !stands(d, g, committed(d, db)))
    }

    /// Why `current` answers false, for a report (cold: a retired entry's next lowering costs far more): the first
    /// cause it meets, in its order: a lagged read, an OID wrap, this thread's own DDL, a private build, or the first
    /// dependency whose generation moved (`Dep@read->now`). `None` while current.
    #[cold]
    pub fn moved(&self) -> Option<String> {
        if self.lagged {
            return Some("a read lagged its view".to_string()); // alloc: Cold
        }
        if self.wraps.is_some_and(|w| w != l2cache::oid_wraps()) {
            return Some("an OID wrap".to_string()); // alloc: Cold
        }
        if self.own != 0 {
            if self.own != l2cache::own_seq() {
                return Some("this backend's own DDL moved".to_string()); // alloc: Cold
            }
        } else if l2cache::private_build_mode() && (!l2cache::private_by_own_ddl_only() || self.touched_by_own_ddl()) {
            return Some("a private build".to_string()); // alloc: Cold
        }
        self.items.iter().find_map(|&(d, g)| {
            let (now, lag) = capture(d);
            (lag || !stands(d, g, now)).then(|| format!("{d:?}@{g}->{now}{}", if lag { " (lagging)" } else { "" })) // alloc: Cold
        })
    }

    /// `current`, asked by a holder that keeps `mark` (start it at `UNCHECKED`): while this thread's views have not moved
    /// since the last check that held, it holds without a read (a change to any dependency reaches this thread as a view
    /// move when it processes the invalidation, as the backend's own catalog caches see it).
    pub fn current_since(&self, mark: &core::cell::Cell<u64>) -> bool {
        if matches!(self.items.first(), Some((Dep::Setting(_), _))) && !self.settings_hold() {
            mark.set(UNCHECKED);
            return false;
        }
        let now = l2cache::view_moves();
        if mark.get() == now {
            return true;
        }
        let ok = self.current();
        mark.set(if ok { now } else { UNCHECKED });
        ok
    }

    /// D4010: every setting read holds its value now (a setting moves no view, so `current_since` asks at every ask).
    fn settings_hold(&self) -> bool {
        self.items.iter().take_while(|e| matches!(e.0, Dep::Setting(_))).all(|&(d, g)| capture(d) == (g, false))
    }

    /// `other`'s dependencies too, at the older generation where both hold one (the plan's own lists beside what the
    /// lowering read).
    pub fn absorb(&mut self, other: &Deps) {
        self.merge(other);
    }

    /// The relations depended on.
    pub fn rels(&self) -> impl Iterator<Item = Oid> + '_ {
        self.items.iter().filter_map(|e| if let Dep::Rel(r) | Dep::Stats(r) = e.0 { Some(r) } else { None })
    }

    /// What is depended on, without the generations: two plans' lists name the same objects iff their identities agree
    /// (`Names::identity` of the same objects is the same).
    pub fn identity(&self) -> u128 {
        identity_of(self.items.iter().map(|e| e.0))
    }

    /// D3938: what was read with the generations it was read at, one value for a process-wide cache's key: an entry made
    /// from these reads is found again only while they would read the same (a moved generation, an OID wrap, is another
    /// key; the old entry ages out). `None` when what was read may not be the committed catalog state (a lagged view, this
    /// thread's own uncommitted DDL, a parallel worker or a historic snapshot): nothing made from it may be shared.
    pub fn committed_stamp(&self) -> Option<u128> {
        if self.lagged || self.own != 0 {
            return None;
        }
        let ws = self.items.iter().flat_map(|&(d, g)| [encode(d), g]);
        Some(identity_of_words(core::iter::once(self.wraps.map_or(0, |w| w.wrapping_add(1))).chain(ws)))
    }

    /// This thread's own uncommitted DDL registered an invalidation of a dependency (a superset: inval's summary).
    #[cold]
    fn touched_by_own_ddl(&self) -> bool {
        let ds: Vec<Dep> = self.iter().collect(); // alloc: Cold
        touched_by_own_ddl(&ds, false)
    }

    /// `other`'s dependencies too, each at the older of the two generations read.
    fn merge(&mut self, other: &Deps) {
        for &(d, g) in &other.items {
            match self.items.binary_search_by(|e| e.0.cmp(&d)) {
                Ok(i) if matches!(d, Dep::Setting(_)) => self.lagged |= self.items[i].1 != g,
                Ok(i) => self.items[i].1 = if keyed(d) { l2cache::keyed_older(self.items[i].1, g) } else { self.items[i].1.min(g) },
                Err(i) => self.items.insert(i, (d, g)), // alloc: Cold
            }
        }
        self.lagged |= other.lagged;
        if other.own != 0 {
            if self.own == 0 {
                self.own = other.own;
            } else if self.own != other.own {
                self.lagged = true;
            }
        }
        self.wraps = match (self.wraps, other.wraps) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
    }
}

/// `d`'s generation now, and whether this thread's view of it lags. A catalog row's moves with its own messages only (C3
/// S5b, l2cache `row_gen`), a row-stamped cache's only when a committed row changes (D3866); a list moves with every
/// message of its cache (`prog_gen`), which a created row sends too.
fn domain(d: Dep) -> l2cache::Domain {
    match d {
        Dep::Setting(_) => unreachable!("gravity_deps: a setting has no catalog domain (capture reads its probe)"),
        Dep::Rel(r) | Dep::Stats(r) => l2cache::Domain::Rel(r),
        Dep::Cat { cache, .. } | Dep::CatList(cache) => l2cache::Domain::Cat(i32::from(cache)),
    }
}

/// The generation is this database's (l2cache `prog_gen` / `row_gen` / `stor_gen`: another database's catalog changes move nothing
/// a program here read); the lag test is the domain's, which every database's message moves.
/// D4028: `d`'s generation is keyed (l2cache `KeyedGens`: a catalog row's, a relation's, a relation's storage): two
/// halves, moved only when both moved.
#[inline]
fn keyed(d: Dep) -> bool {
    matches!(d, Dep::Cat { .. } | Dep::Rel(_) | Dep::Stats(_))
}

/// `d` read at generation `then` stands at `now` (D4028: a keyed read unless both halves moved).
#[inline]
fn stands(d: Dep, then: u64, now: u64) -> bool {
    if keyed(d) { l2cache::keyed_stands(then, now) } else { then == now }
}

fn capture(d: Dep) -> (u64, bool) {
    if let Dep::Setting(key) = d {
        return SETTING_PROBE.get().and_then(|f| f(key)).map_or((u64::MAX, true), |v| (v, false));
    }
    let dom = domain(d);
    let g = committed(d, init_small::globals::MyDatabaseId());
    (g, l2cache::view_gen(dom) != l2cache::current_gen(dom))
}

/// The generation `d` has committed in database `db` (not a setting).
fn committed(d: Dep, db: Oid) -> u64 {
    match d {
        // C3 S5b: a row's own generation (a row-stamped cache's moves only when a committed row changes, D3866)
        Dep::Cat { cache, hash } => l2cache::row_gen(i32::from(cache), hash, db),
        Dep::Stats(r) => l2cache::stor_gen(r, db),
        _ => l2cache::prog_gen(domain(d), db),
    }
}

/// C3 S5d: this thread's own uncommitted DDL registered an invalidation of what `ds` read, by part (inval
/// `PendingInvalidationsTouchParts`: a relation's entry, its storage, a catalog cache); `under`: read under the DDL.
#[cold]
fn touched_by_own_ddl(ds: &[Dep], under: bool) -> bool {
    let mut parts: (Vec<Oid>, Vec<Oid>, Vec<i32>) = Default::default(); // alloc: Cold
    for &d in ds {
        match d {
            Dep::Rel(r) => parts.0.push(r),
            Dep::Stats(r) => parts.1.push(r),
            Dep::Cat { cache, .. } | Dep::CatList(cache) => parts.2.push(i32::from(cache)),
            Dep::Setting(_) => {}
        }
    }
    l2cache::pending_touch(&parts.0, &parts.1, &parts.2, under)
}

/// The identity of objects `ds`, in `Dep` order, each once.
fn identity_of(ds: impl Iterator<Item = Dep>) -> u128 {
    identity_of_words(ds.map(encode))
}

fn identity_of_words(ws: impl Iterator<Item = u64>) -> u128 {
    let (mut lo, mut hi) = (0xcbf2_9ce4_8422_2325u64, 0x6c62_272e_07bb_0142u64);
    for w in ws {
        lo = (lo ^ w).wrapping_mul(0x0000_0100_0000_01b3);
        hi = (hi ^ w.rotate_left(29)).wrapping_mul(0x0000_0100_0000_01b3).rotate_left(17);
    }
    (u128::from(hi) << 64) | u128::from(lo)
}

/// What a plan names, without generations (C3 S4b): its `identity` keys both program tiers, so two plans whose graphs
/// hash alike over different objects, or over a dropped and recreated one, are two keys. No generation is read: a
/// statement keys by it at every admission that decomposes.
#[derive(Clone, Debug, Default)]
pub struct Names(Vec<Dep>);

impl Names {
    pub fn add(&mut self, d: Dep) {
        if let Err(i) = self.0.binary_search(&d) {
            self.0.insert(i, d); // alloc: Exec
        }
    }

    /// `Deps::identity` of the same objects.
    pub fn identity(&self) -> u128 {
        identity_of(self.0.iter().copied())
    }
}

leader_tls::leader_tls! {
    thread_local! {
        /// `take_miss`: the first held hit check that failed (C3 S6c); set and taken on the admission's thread only.
        static MISS: RefCell<Option<String>> = const { RefCell::new(None) };
    }
}

thread_local! {
    /// `Checking`.
    static CHECKING: Cell<bool> = const { Cell::new(false) };
    /// Recording armed and not suppressed: the one test a cache makes on its hot path.
    static ARMED: Cell<bool> = const { Cell::new(false) };
}

leader_tls::leader_tls! {
    thread_local! {
        static SUPPRESS: Cell<u32> = const { Cell::new(0) };
        /// The recordings in progress on this thread, innermost last.
        static STACK: RefCell<Vec<Deps>> = const { RefCell::new(Vec::new()) };
        /// Inside a catalog-free region (`catalog_free`).
        static FREE: Cell<u32> = const { Cell::new(0) };
    }
}

/// Catalog reads inside a catalog-free region, process-wide, and the first one (`encode`): `take_free_read`.
static FREE_READS: AtomicU64 = AtomicU64::new(0);
static FREE_FIRST: AtomicU64 = AtomicU64::new(0);

fn rearm() {
    let open = STACK.with(|s| s.try_borrow().map_or(true, |s| !s.is_empty())) || FREE.with(Cell::get) > 0;
    ARMED.with(|a| a.set(open && SUPPRESS.with(Cell::get) == 0));
}

#[inline]
pub fn armed() -> bool {
    ARMED.with(Cell::get)
}

/// One lowering's recording. An inner recording's reads are its outer's too; one dropped unfinished (an early return,
/// an error) still gives them to its outer.
pub struct Recording {
    depth: usize,
    done: bool,
}

impl Recording {
    pub fn start() -> Recording {
        let depth = STACK.with(|s| {
            let mut s = s.borrow_mut();
            s.push(Deps::default()); // alloc: Cold
            s.len()
        });
        rearm();
        Recording { depth, done: false }
    }

    pub fn finish(mut self) -> Deps {
        self.done = true;
        pop(self.depth)
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        if !self.done {
            pop(self.depth);
        }
    }
}

fn pop(depth: usize) -> Deps {
    let mine = STACK.with(|s| {
        let mut s = s.borrow_mut();
        debug_assert_eq!(s.len(), depth, "gravity_deps: recordings end innermost first");
        let mine = s.pop().unwrap_or_default();
        if let Some(outer) = s.last_mut() {
            outer.merge(&mine);
        }
        mine
    });
    rearm();
    mine
}

/// A cache entry's or memo's recorded reads (C3: the one form every catalog-derived cache keeps): what its fill read,
/// with its check mark. `Held::fill` records a fill; `holds` asks whether those reads stand, and when they do replays
/// them into the asker's recording (a hit read what the fill read). A `Held` of nothing read always stands.
#[derive(Clone, Debug)]
pub struct Held {
    deps: Deps,
    mark: Cell<u64>,
}

impl Held {
    /// `f` under a recording: its result and what it read, held. It stands, unread, while this thread's views stay where
    /// they were when `f` started (what `f` read is what this thread's caches hold until they move: a thread's own memo
    /// needs no more, even of a read a lagging view gave); at a move, its reads are checked (`Deps::current`).
    pub fn fill<T>(f: impl FnOnce() -> T) -> (T, Held) {
        let at = l2cache::view_moves();
        let (t, deps) = recording(f);
        (t, Held { deps, mark: Cell::new(at) })
    }

    /// Reads recorded elsewhere (an admission's), held: checked at its first ask.
    pub fn of(deps: Deps) -> Held {
        Held { deps, mark: Cell::new(UNCHECKED) }
    }

    /// The reads stand (`Deps::current_since`); replayed into the asker's recording when they do.
    #[inline]
    pub fn holds(&self) -> bool {
        let ok = self.stands();
        if ok {
            self.replay();
        }
        ok
    }

    /// The reads stand, nothing replayed (a holder choosing among entries replays the one it serves).
    #[inline]
    pub fn stands(&self) -> bool {
        self.deps.current_since(&self.mark)
    }

    /// The reads, replayed into the asker's recording (an entry `stands` found and serves).
    #[inline]
    pub fn replay(&self) {
        replay(&self.deps);
    }

    pub fn deps(&self) -> &Deps {
        &self.deps
    }

    /// `other`'s reads too, in place (a memo entry that later keeps pieces whose making read more): checked at the next ask,
    /// as `of` holds them.
    pub fn absorb(&mut self, other: &Deps) {
        self.deps.absorb(other);
        self.mark.set(UNCHECKED);
    }

    /// C3's recursive check (check mode, `checking`): a hit this entry serves, against its fill done again now
    /// (`same_again` fills and compares with the entry's answer). The fill's reads must be covered by the entry's: a read
    /// it misses is a change that would not drop the entry, a cache keyed short of its producer (the WRONG class D3938
    /// was), however deep the entry sits under the admission that asked. The first failure is kept for the admission to
    /// raise (`take_miss`); the fill's reads join the asker's recording, as the hit's replay did.
    #[cold]
    #[inline(never)]
    pub fn check(&self, holder: &'static str, same_again: impl FnOnce() -> bool) {
        let (same, again) = Held::fill(same_again);
        let missed: Vec<Dep> = again.deps.iter().filter(|&d| !self.deps.covers(d)).take(6).collect(); // alloc: Cold
        let why = if !missed.is_empty() {
            Some(format!("{holder}: its fill here read {missed:?}, which the entry's recorded reads miss ({} here, {} held)", again.deps.len(), self.deps.len())) // alloc: Cold
        } else if !same {
            Some(format!("{holder}: its fill here answers otherwise while the entry's reads stand ({} held)", self.deps.len())) // alloc: Cold
        } else {
            None
        };
        if let Some(w) = why {
            MISS.with(|m| {
                m.borrow_mut().get_or_insert(w);
            });
        }
    }
}

/// C3 check mode on this thread for an extent (the harness's checks; exec sets it per admission): a held hit fills again
/// (`Held::check`). Dropped (an unwind too), it puts the previous setting back.
#[must_use = "the check mode stays set only while the guard lives"]
pub struct Checking(bool);

impl Checking {
    pub fn set(on: bool) -> Checking {
        Checking(CHECKING.with(|c| c.replace(on)))
    }
}

impl Drop for Checking {
    fn drop(&mut self) {
        CHECKING.with(|c| c.set(self.0));
    }
}

#[inline]
pub fn checking() -> bool {
    CHECKING.with(Cell::get)
}

/// The first held hit check that failed on this thread since the last take (`Held::check`): the holder and why.
pub fn take_miss() -> Option<String> {
    MISS.with(|m| m.borrow_mut().take())
}

/// C3: state built from catalog reads no recording sees (a function's `fn_extra` memo, which C rebuilds per query: a
/// record's tuple descriptor, a type cache entry), its holder's mark: such state may depend on anything this thread's
/// catalog caches return, so it stands while this thread's views stand (`Opaque::now` is the same value; any catalog
/// cache invalidation, reset or flush this thread takes, and any generation it syncs, moves it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct Opaque(u64);

impl Opaque {
    /// A mark no `now` equals: the holder's first check finds it moved.
    pub const NONE: Opaque = Opaque(u64::MAX);

    /// This thread's views, now.
    #[inline]
    pub fn now() -> Opaque {
        Opaque(l2cache::view_moves())
    }
}

/// Tests: relation `r`'s relcache entry changed, as DDL over it invalidates it.
#[doc(hidden)]
pub fn invalidate_rel_for_test(r: Oid) {
    l2cache::invalidate_key(l2cache::Domain::Rel(r), l2cache::rel_key(r, 0));
}

/// `f` under a recording: its result and what it read.
pub fn recording<T>(f: impl FnOnce() -> T) -> (T, Deps) {
    let r = Recording::start();
    let t = f();
    (t, r.finish())
}

struct Suppress;

impl Drop for Suppress {
    fn drop(&mut self) {
        SUPPRESS.with(|n| n.set(n.get() - 1));
        rearm();
    }
}

/// `f` with recording suppressed: a cache filling an entry whose own note covers what the fill reads.
pub fn suppressed<T>(f: impl FnOnce() -> T) -> T {
    SUPPRESS.with(|n| n.set(n.get() + 1));
    ARMED.with(|a| a.set(false));
    let _g = Suppress;
    f()
}

fn encode(d: Dep) -> u64 {
    match d {
        Dep::Setting(key) => u64::from(key),
        Dep::Rel(r) => (1 << 62) | u64::from(r),
        Dep::Stats(r) => (1 << 62) | (2 << 32) | u64::from(r),
        Dep::Cat { cache, hash } => (2 << 62) | (u64::from(cache as u16) << 32) | u64::from(hash),
        Dep::CatList(cache) => (3 << 62) | (u64::from(cache as u16) << 32),
    }
}

fn decode(e: u64) -> Dep {
    match e >> 62 {
        2 => Dep::Cat { cache: (e >> 32) as u16 as i16, hash: e as u32 },
        3 => Dep::CatList((e >> 32) as u16 as i16),
        0 => Dep::Setting(e as u32),
        1 if e & (2 << 32) != 0 => Dep::Stats(e as u32),
        _ => Dep::Rel(e as u32),
    }
}

struct Free;

impl Drop for Free {
    fn drop(&mut self) {
        FREE.with(|n| n.set(n.get() - 1));
        rearm();
    }
}

/// `f` as a catalog-free region: the code generator, whose output is a function of the program alone and which may run
/// on another thread than the lowering's recording. A catalog read inside one is counted, never recorded.
pub fn catalog_free<T>(f: impl FnOnce() -> T) -> T {
    FREE.with(|n| n.set(n.get() + 1));
    rearm();
    let _g = Free;
    f()
}

/// The first catalog read inside a catalog-free region since the last call (any thread) and how many there were.
pub fn take_free_read() -> Option<(Dep, u64)> {
    let n = FREE_READS.swap(0, Relaxed);
    let first = FREE_FIRST.swap(0, Relaxed);
    (n > 0).then(|| (decode(first), n))
}

/// A memo's hit: what its entry's fill read (`recording` around the fill), at the generations then, read again here.
#[inline]
pub fn replay(d: &Deps) {
    if armed() && !d.is_empty() {
        replay_cold(d);
    }
}

#[cold]
#[inline(never)]
fn replay_cold(d: &Deps) {
    if FREE.with(Cell::get) > 0 {
        d.iter().for_each(note);
        return;
    }
    STACK.with(|s| {
        if let Ok(mut s) = s.try_borrow_mut() {
            if let Some(top) = s.last_mut() {
                top.merge(d);
            }
        }
    });
}

#[cold]
#[inline(never)]
fn note(d: Dep) {
    if FREE.with(Cell::get) > 0 {
        FREE_READS.fetch_add(1, Relaxed);
        let _ = FREE_FIRST.compare_exchange(0, encode(d), Relaxed, Relaxed);
        return;
    }
    STACK.with(|s| {
        if let Ok(mut s) = s.try_borrow_mut() {
            if let Some(top) = s.last_mut() {
                top.add(d);
            }
        }
    });
}

/// D4010: the reader of a setting's value fingerprint by key (catalog_namespace installs the search path's). `None` when
/// it cannot read one: the read then holds nowhere.
static SETTING_PROBE: pgsync::OnceLock<fn(u32) -> Option<u64>> = pgsync::OnceLock::new();

pub fn set_setting_probe(f: fn(u32) -> Option<u64>) {
    let _ = SETTING_PROBE.set(f);
}

/// D4010: a read resolved under setting `key` at value fingerprint `value` (a name lookup's search path).
#[inline]
pub fn note_setting(key: u32, value: u64) {
    if armed() {
        note_setting_cold(key, value);
    }
}

#[cold]
#[inline(never)]
fn note_setting_cold(key: u32, value: u64) {
    if FREE.with(Cell::get) > 0 {
        FREE_READS.fetch_add(1, Relaxed);
        let _ = FREE_FIRST.compare_exchange(0, encode(Dep::Setting(key)), Relaxed, Relaxed);
        return;
    }
    STACK.with(|s| {
        if let Ok(mut s) = s.try_borrow_mut() {
            if let Some(top) = s.last_mut() {
                top.add_setting(key, value);
            }
        }
    });
}

#[inline]
pub fn note_rel(relid: Oid) {
    if armed() && relid != 0 {
        note(Dep::Rel(relid));
    }
}

/// C3 S5 (D3889): a read of relation `relid`'s storage or size statistics (its relcache entry's relfilenode, relpages,
/// reltuples, relallvisible) that a lowering bakes: the reader says so, as a storage-scoped message (TRUNCATE) leaves
/// the entry's own generation (`Dep::Rel`).
#[inline]
pub fn note_stats(relid: Oid) {
    if armed() && relid != 0 {
        note(Dep::Stats(relid));
    }
}

#[inline]
pub fn note_cat(cache: i32, hash: u32) {
    if armed() {
        note(Dep::Cat { cache: cache as i16, hash });
    }
}

#[inline]
pub fn note_cat_list(cache: i32) {
    if armed() {
        note(Dep::CatList(cache as i16));
    }
}

/// A type's ordering or hashing support (typcache): built from `SUPPORT_LISTS` rows.
pub fn note_support() {
    if armed() {
        SUPPORT_LISTS.iter().for_each(|&c| note(Dep::CatList(c)));
    }
}

/// A systable scan of catalog `catalog`: any change to it, through each syscache over it (a catalog with none changes
/// with the relcache invalidations of the objects its rows describe, as PostgreSQL's plancache takes it).
pub fn note_catalog_scan(catalog: Oid) {
    if armed() {
        SYSCACHE_RELOIDS.iter().enumerate().filter(|e| *e.1 == catalog).for_each(|(i, _)| note(Dep::CatList(i as i16)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stand-in search path's fingerprint (`u64::MAX`: none can be read); only this module's setting test moves it.
    static PATH_NOW: AtomicU64 = AtomicU64::new(7);

    fn path_now(key: u32) -> Option<u64> {
        (key == SETTING_SEARCH_PATH).then(|| PATH_NOW.load(Relaxed)).filter(|&v| v != u64::MAX)
    }

    /// D4010: a read under the search path holds while the path resolves as it did, with no view move in between (a SET
    /// or a role change moves none); two paths in one recording, or merged, hold nowhere.
    #[test]
    fn a_read_under_a_setting_holds_only_while_the_setting_resolves_as_it_did() {
        set_setting_probe(path_now);
        let ((), d) = recording(|| {
            note_setting(SETTING_SEARCH_PATH, 7);
            note_rel(30);
        });
        assert_eq!(d.iter().collect::<Vec<_>>(), vec![Dep::Setting(SETTING_SEARCH_PATH), Dep::Rel(30)]);
        let mark = Cell::new(UNCHECKED);
        assert!(d.current_since(&mark) && mark.get() != UNCHECKED);
        PATH_NOW.store(8, Relaxed);
        assert!(!d.current_since(&mark), "the views did not move, the path did");
        assert!(!d.current());
        PATH_NOW.store(u64::MAX, Relaxed);
        assert!(!d.current(), "a path the probe cannot read holds nothing");
        PATH_NOW.store(7, Relaxed);
        assert!(d.current_since(&mark));
        let ((), two) = recording(|| {
            note_setting(SETTING_SEARCH_PATH, 7);
            let ((), _) = recording(|| note_setting(SETTING_SEARCH_PATH, 8));
        });
        assert!(two.lagged() && !two.current());
        let ((), e) = recording(|| note_setting(SETTING_SEARCH_PATH, 9));
        let mut m = d.clone();
        m.absorb(&e);
        assert!(m.lagged());
        assert_eq!(decode(encode(Dep::Setting(SETTING_SEARCH_PATH))), Dep::Setting(SETTING_SEARCH_PATH));
    }

    #[test]
    fn notes_land_only_while_armed_and_unsuppressed() {
        note_rel(10);
        assert!(!armed());
        let ((), d) = recording(|| {
            assert!(armed());
            note_rel(11);
            note_rel(11);
            note_cat(47, 0xabc);
            suppressed(|| {
                assert!(!armed());
                note_rel(12);
                suppressed(|| note_rel(13));
                assert!(!armed());
            });
            assert!(armed());
            note_cat_list(82);
        });
        assert!(!armed());
        assert_eq!(d.iter().collect::<Vec<_>>(), vec![Dep::Rel(11), Dep::Cat { cache: 47, hash: 0xabc }, Dep::CatList(82)]);
        assert!(!d.contains(Dep::Rel(10)) && !d.contains(Dep::Rel(12)) && !d.contains(Dep::Rel(13)));
    }

    #[test]
    fn an_inner_recording_gives_its_reads_to_the_outer_even_when_dropped() {
        let ((), outer) = recording(|| {
            note_rel(20);
            let ((), inner) = recording(|| note_rel(21));
            assert_eq!(inner.iter().collect::<Vec<_>>(), vec![Dep::Rel(21)]);
            {
                let _r = Recording::start();
                note_rel(22);
            }
            assert!(armed());
        });
        assert_eq!(outer.iter().collect::<Vec<_>>(), vec![Dep::Rel(20), Dep::Rel(21), Dep::Rel(22)]);
    }

    #[test]
    fn another_threads_reads_are_not_this_recordings() {
        let ((), d) = recording(|| {
            std::thread::spawn(|| {
                assert!(!armed());
                note_rel(31);
            })
            .join()
            .unwrap();
            note_rel(30);
        });
        assert_eq!(d.iter().collect::<Vec<_>>(), vec![Dep::Rel(30)]);
    }

    #[test]
    fn a_catalog_read_in_a_catalog_free_region_is_counted_not_recorded() {
        let ((), d) = recording(|| {
            note_rel(40);
            catalog_free(|| {
                assert!(armed());
                note_cat(47, 0x41);
                suppressed(|| note_rel(42));
                std::thread::spawn(|| catalog_free(|| note_cat_list(19))).join().unwrap();
            });
            note_rel(43);
        });
        assert_eq!(d.iter().collect::<Vec<_>>(), vec![Dep::Rel(40), Dep::Rel(43)]);
        assert!(!armed());
        let (first, n) = take_free_read().expect("the reads in the regions");
        assert_eq!(n, 2);
        assert!(first == Dep::Cat { cache: 47, hash: 0x41 } || first == Dep::CatList(19));
        assert_eq!(take_free_read(), None);
        catalog_free(|| ());
        assert!(!armed());
        for d in [Dep::Rel(u32::MAX), Dep::Cat { cache: 84, hash: u32::MAX }, Dep::CatList(-1)] {
            assert_eq!(decode(encode(d)), d);
        }
    }

    #[test]
    fn a_memo_hit_replays_its_fill_at_the_fills_generations() {
        let ((), fill) = recording(|| {
            note_rel(50);
            note_cat_list(19);
        });
        replay(&fill);
        let ((), d) = recording(|| {
            note_rel(51);
            replay(&fill);
            suppressed(|| replay(&fill));
        });
        assert_eq!(d.iter().collect::<Vec<_>>(), vec![Dep::Rel(50), Dep::Rel(51), Dep::CatList(19)]);
        let mut older = fill.clone();
        older.items.iter_mut().for_each(|e| e.1 = 0);
        let ((), m) = recording(|| {
            note_rel(50);
            replay(&older);
        });
        assert!(m.items.iter().all(|e| e.1 == 0), "the older generation wins: {m:?}");
    }

    #[test]
    fn a_check_holds_until_this_threads_views_move() {
        let r: Oid = 0xFFFF_F3B7;
        let ((), d) = recording(|| note_rel(r));
        let mark = Cell::new(UNCHECKED);
        assert_eq!(d.current_since(&mark), d.current());
        if d.current() {
            assert_ne!(mark.get(), UNCHECKED);
            assert!(d.current_since(&mark));
            l2cache::invalidate_key(l2cache::Domain::Rel(r), l2cache::rel_key(r, 0));
            assert!(!d.current_since(&mark), "a moved view checks again");
            assert_eq!(mark.get(), UNCHECKED);
        }
    }

    #[test]
    fn a_catalog_scan_notes_every_syscache_over_it() {
        let ((), d) = recording(|| note_catalog_scan(2602));
        assert_eq!(d.iter().collect::<Vec<_>>(), vec![Dep::CatList(3), Dep::CatList(4)]);
        let ((), s) = recording(note_support);
        assert_eq!(s.len(), SUPPORT_LISTS.len());
    }

    #[test]
    fn a_held_fill_holds_until_its_reads_move_and_replays_them() {
        let (r, other): (Oid, Oid) = (0xFFFF_F3D7, 0xFFFF_F3D8);
        let (v, held) = Held::fill(|| {
            note_rel(r);
            7
        });
        assert_eq!(v, 7);
        assert!(held.deps().contains(Dep::Rel(r)));
        let (ok, replayed) = recording(|| held.holds());
        assert!(ok, "nothing moved");
        assert!(replayed.contains(Dep::Rel(r)), "a hit's recording holds what the fill read");
        l2cache::invalidate_key(l2cache::Domain::Rel(other), l2cache::rel_key(other, 0));
        assert!(held.holds() || other & 1023 == r & 1023, "another relation moved");
        l2cache::invalidate_key(l2cache::Domain::Rel(r), l2cache::rel_key(r, 0));
        let (ok, replayed) = recording(|| held.holds());
        assert!(!ok, "its read moved");
        assert!(replayed.is_empty(), "a miss replays nothing");
    }

    /// C3 S5b: a moved row is found behind an unmoved row of its cache that was read at the same generation (the check
    /// once read a cache once per generation, as when a row read held the cache's generation).
    #[test]
    fn a_moved_row_is_found_behind_another_row_of_its_cache_at_the_same_generation() {
        let db = init_small::globals::MyDatabaseId();
        let h1 = 0x0BAD_2001u32;
        for h2 in h1 + 1..h1 + 4096 {
            if l2cache::row_gen(63, h2, db) != l2cache::row_gen(63, h1, db) {
                continue;
            }
            let g1 = l2cache::row_gen(63, h1, db);
            let ((), both) = recording(|| {
                note_cat(63, h1);
                note_cat(63, h2);
            });
            assert!(both.current());
            l2cache::invalidate_key(l2cache::Domain::Cat(63), l2cache::cat_key(63, db, h2));
            l2cache::sync_view(l2cache::Domain::Cat(63));
            if l2cache::row_gen(63, h1, db) != g1 {
                continue; // the two rows share a generation slot: try another
            }
            assert!(!both.current(), "the second row moved");
            return;
        }
        panic!("no row of cache 63 apart from 0x{h1:x}");
    }

    #[test]
    fn a_row_read_holds_across_another_rows_message_and_a_list_read_does_not() {
        let db = init_small::globals::MyDatabaseId();
        let h1 = 0x0BAD_0001u32;
        let (s1, d1) = l2cache::row_slots_of(60, h1, db);
        let h2 = (h1 + 1..).find(|&h| l2cache::row_slots_of(60, h, db).0 != s1 && l2cache::row_slots_of(60, h, db).1 != d1).unwrap();
        let ((), row) = recording(|| note_cat(60, h1));
        let ((), list) = recording(|| note_cat_list(60));
        assert!(row.current() && list.current());
        l2cache::invalidate_key(l2cache::Domain::Cat(60), l2cache::cat_key(60, db, h2));
        l2cache::sync_view(l2cache::Domain::Cat(60));
        assert!(!list.current(), "a list read: any row of the cache");
        assert!(row.current(), "another row's message leaves a row read");
        l2cache::invalidate_key(l2cache::Domain::Cat(60), l2cache::cat_key(60, db, h1));
        l2cache::sync_view(l2cache::Domain::Cat(60));
        assert!(!row.current(), "its own row's message moves it");
    }

    #[test]
    fn a_stats_read_holds_across_a_part_scoped_message_until_the_storage_moves() {
        let r: Oid = 0xFFFF_F3F1;
        let db = init_small::globals::MyDatabaseId();
        let ((), rel_only) = recording(|| note_rel(r));
        let ((), stats) = recording(|| note_stats(r));
        assert!(rel_only.current() && stats.current());
        l2cache::invalidate_rel_part(r, db, false);
        l2cache::sync_view(l2cache::Domain::Rel(r));
        assert!(rel_only.current() && stats.current(), "D3890: the triggers' scope moves nothing a lowering read");
        l2cache::invalidate_rel_part(r, db, true);
        l2cache::sync_view(l2cache::Domain::Rel(r));
        assert!(rel_only.current() && !stats.current(), "D3889: a storage-scoped message moves the storage only");
        for d in [Dep::Rel(r), Dep::Stats(r), Dep::Stats(0), Dep::Rel(u32::MAX), Dep::Stats(u32::MAX)] {
            assert_eq!(decode(encode(d)), d);
        }
    }

    #[test]
    fn a_held_check_finds_a_fill_read_its_entry_misses_and_a_different_answer() {
        let (r, s): (Oid, Oid) = (0xFFFF_F3E7, 0xFFFF_F3E8);
        let ((), held) = Held::fill(|| note_rel(r));
        let _ = take_miss();
        held.check("test memo", || {
            note_rel(r);
            true
        });
        assert_eq!(take_miss(), None, "the fill read what the entry holds and answers alike");
        held.check("test memo", || {
            note_rel(r);
            note_rel(s);
            true
        });
        let m = take_miss().expect("a read the entry misses");
        assert!(m.starts_with("test memo: its fill here read [Rel(") && m.contains(&format!("{s}")), "{m}");
        held.check("test memo", || false);
        assert!(take_miss().is_some_and(|m| m.contains("answers otherwise")));
        let ((), seen) = recording(|| held.check("test memo", || {
            note_rel(r);
            true
        }));
        assert!(seen.contains(Dep::Rel(r)), "the check's fill joins the asker's recording");
        assert!(!checking());
        let on = Checking::set(true);
        assert!(checking());
        drop(on);
        assert!(!checking(), "the guard puts the previous setting back");
    }

    #[test]
    fn a_committed_stamp_is_the_reads_at_their_generations() {
        let r: Oid = 0xFFFF_F3C5;
        let ((), d) = recording(|| note_rel(r));
        let Some(s) = d.committed_stamp() else {
            assert!(d.lagged(), "only a lagged or private read has no stamp");
            return;
        };
        let ((), again) = recording(|| note_rel(r));
        assert_eq!(again.committed_stamp(), Some(s), "the same reads at the same generations: one stamp");
        let ((), other) = recording(|| note_rel(r + 1));
        assert_ne!(other.committed_stamp(), Some(s), "another object: another stamp");
        assert_ne!(Deps::default().committed_stamp(), Some(s));
        l2cache::invalidate_key(l2cache::Domain::Rel(r), l2cache::rel_key(r, 0));
        let ((), moved) = recording(|| note_rel(r));
        assert_ne!(moved.committed_stamp(), Some(s), "a moved generation (or a view lagging it): never the old stamp");
        let mut lag = again.clone();
        lag.lagged = true;
        assert_eq!(lag.committed_stamp(), None);
    }

    /// D4579: a read is superseded in a database once the generation committed there moves, before any view follows,
    /// and stays so: a storage-scoped message supersedes a storage read only, a whole one the relation's too; a
    /// setting read never is.
    #[test]
    fn a_read_is_superseded_in_its_database_once_its_committed_generation_moves() {
        let r: Oid = 0xFFFF_F3B1;
        let db = init_small::globals::MyDatabaseId();
        let ((), rel_only) = recording(|| note_rel(r));
        let ((), stats) = recording(|| note_stats(r));
        let ((), setting) = recording(|| note_setting(SETTING_SEARCH_PATH, 5));
        assert!(!rel_only.superseded_in(db) && !stats.superseded_in(db) && !setting.superseded_in(db));
        l2cache::invalidate_rel_part(r, db, true);
        assert!(!rel_only.superseded_in(db) && stats.superseded_in(db), "a storage-scoped message supersedes the storage read only");
        l2cache::invalidate_key(l2cache::Domain::Rel(r), l2cache::rel_key(r, db));
        assert!(rel_only.superseded_in(db) && stats.superseded_in(db), "a whole message supersedes the relation read too");
        l2cache::sync_view(l2cache::Domain::Rel(r));
        assert!(rel_only.superseded_in(db) && !rel_only.current(), "and it never stands again");
        assert!(!setting.superseded_in(db), "a setting's value can come back: never superseded");
    }

    #[test]
    fn a_dependency_is_current_until_its_generation_moves() {
        let r: Oid = 0xFFFF_F3A1;
        let ((), d) = recording(|| note_rel(r));
        let lagged = d.lagged();
        assert_eq!(d.current(), !lagged);
        l2cache::invalidate_key(l2cache::Domain::Rel(r), l2cache::rel_key(r, 0));
        assert!(!d.current());
    }
}
