//! An entry is keyed by the relation and its storage (OID, relfilelocator, backend: a TRUNCATE, rewrite or
//! renumber that moves the storage misses), and is reused only under the command id and extent-table version it was
//! taken at ([`rowid_alloc::extents_version`]: any extent added, pruned or forgotten in this server moves it), so a hit
//! is the table an uncached read would return.
//!
//! Beside them, a statement's views ([`statement_view`]: `ring::statement_view`, one `catalog::visible` walk a
//! statement, not a row) and the position the executor last located ([`was_located`]); both are dropped at transaction
//! end and at a subtransaction's abort.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use pgrc_am::catalog::Generation;
use pgrc_am::rowid_alloc::{self, Extents};
use types_core::{CommandId, Oid, TransactionId};
use types_error::PgResult;
use types_rel::Relation;
use types_snapshot::SnapshotData;

const STALE_POINT: &str = "pgrc-dmlpath-stale";
const LOCATED_STALE_POINT: &str = "pgrc-located-stale";
const VIEW_STALE_POINT: &str = "pgrc-view-stale";
const SLOTS: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Key {
    relid: Oid,
    spc: Oid,
    db: Oid,
    rel: u64,
    backend: i32,
}

struct Entry {
    key: Key,
    cid: CommandId,
    ver: u64,
    dir: Rc<str>,
    ext: Arc<Extents>,
    over: bool,
}

#[derive(Default)]
struct Cache {
    entries: Vec<Entry>,
    clock: usize,
    located: Option<Located>,
    views: Vec<ViewEntry>,
    view_clock: usize,
    hooked: bool,
    view_hits: u64,
    view_misses: u64,
}

/// What a statement's view of a table's storage is a function of besides its MVCC snapshot ([`SnapKey`]) and the
/// catalog's head generation of the table ([`ViewEntry::head`]): the command and the subtransaction (an abort makes its
/// publishes another's).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ViewAt {
    pub(crate) cid: CommandId,
    pub(crate) subid: u32,
}

/// An MVCC snapshot's content as `XidInMVCCSnapshot` reads it, and its curcid: equal content, the same answer for every
/// publisher (one outside the snapshot completed before it, so its clog verdict is final). Compared in place with the
/// live snapshot: snapmgr's copies (the active snapshot is one) carry no completion count.
#[derive(Default)]
struct SnapKey {
    xmin: TransactionId,
    xmax: TransactionId,
    curcid: CommandId,
    suboverflowed: bool,
    recovery: bool,
    xip: Vec<TransactionId>,
    subxip: Vec<TransactionId>,
}

fn live_xids<'s>(s: &'s SnapshotData<'_>) -> (&'s [TransactionId], &'s [TransactionId]) {
    (&s.xip[..s.xcnt as usize], &s.subxip[..s.subxcnt.max(0) as usize])
}

impl SnapKey {
    /// The bounds and counts first, the xid arrays last (hundreds of xids with some 256 backends).
    fn matches(&self, s: &SnapshotData<'_>) -> bool {
        let (xip, subxip) = live_xids(s);
        self.xip.len() == xip.len()
            && self.subxip.len() == subxip.len()
            && self.xmin == s.xmin
            && self.xmax == s.xmax
            && self.curcid == s.curcid.get()
            && self.suboverflowed == s.suboverflowed
            && self.recovery == s.takenDuringRecovery
            && self.xip[..] == *xip
            && self.subxip[..] == *subxip
    }

    fn set(&mut self, s: &SnapshotData<'_>) {
        let (xip, subxip) = live_xids(s);
        (self.xmin, self.xmax, self.curcid, self.suboverflowed, self.recovery) = (s.xmin, s.xmax, s.curcid.get(), s.suboverflowed, s.takenDuringRecovery);
        self.xip.clear();
        self.xip.extend_from_slice(xip); // alloc: Cold
        self.subxip.clear();
        self.subxip.extend_from_slice(subxip); // alloc: Cold
    }
}

struct ViewEntry {
    key: Key,
    at: ViewAt,
    head: u64,
    snap: SnapKey,
    view: Option<Arc<Generation>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Located {
    key: Key,
    cid: CommandId,
    subid: u32,
    stmt: i64,
    block: u32,
    off: u16,
}

thread_local! {
    static CACHE: RefCell<Cache> = RefCell::new(Cache::default());
}

pub(crate) struct Path {
    pub dir: Rc<str>,
    pub ext: Arc<Extents>,
}

impl Cache {
    /// `ver` is read before `load` runs, so a copy is never older than the version it is filed under.
    fn entry(
        &mut self,
        key: Key,
        cid: CommandId,
        ver: u64,
        stale: &dyn Fn() -> bool,
        dir_of: &dyn Fn() -> String,
        load: &dyn Fn(&str) -> PgResult<Arc<Extents>>,
    ) -> PgResult<&Entry> {
        let at = self.entries.iter().position(|e| e.key == key);
        if let Some(i) = at {
            if self.entries[i].cid == cid && self.entries[i].ver == ver {
                return Ok(&self.entries[i]);
            }
        }
        if stale() {
            if let Some(i) = self.entries.iter().position(|e| e.key.relid == key.relid) {
                return Ok(&self.entries[i]);
            }
        }
        let dir: Rc<str> = match at {
            Some(i) => Rc::clone(&self.entries[i].dir),
            None => Rc::from(dir_of()), // alloc: Cold
        };
        let ext = load(&dir)?;
        let over = crate::route::extents_over(&ext);
        let fresh = Entry { key, cid, ver, dir, ext, over };
        let i = match at {
            Some(i) => {
                self.entries[i] = fresh;
                i
            }
            None if self.entries.len() < SLOTS => {
                self.entries.push(fresh); // alloc: Cold
                self.entries.len() - 1
            }
            None => {
                let i = self.clock;
                self.entries[i] = fresh;
                self.clock = (self.clock + 1) % SLOTS;
                i
            }
        };
        Ok(&self.entries[i])
    }

    fn view(&mut self, key: Key, at: &ViewAt, snap: &SnapshotData<'_>, head: &dyn Fn() -> Option<u64>, stale: &dyn Fn() -> bool) -> Option<Option<Arc<Generation>>> {
        let hit = self.views.iter().find(|e| e.key == key && e.at == *at && e.snap.matches(snap)).filter(|e| head() == Some(e.head));
        let hit = hit.or_else(|| if stale() { self.views.iter().find(|e| e.key.relid == key.relid) } else { None }).map(|e| e.view.clone());
        match hit {
            Some(_) => self.view_hits += 1,
            None => self.view_misses += 1,
        }
        hit
    }

    fn put_view(&mut self, key: Key, at: ViewAt, head: u64, snap: &SnapshotData<'_>, view: Option<Arc<Generation>>) {
        let i = match self.views.iter().position(|e| e.key == key) {
            Some(i) => i,
            None if self.views.len() < SLOTS => {
                self.views.push(ViewEntry { key, at, head, snap: SnapKey::default(), view: None }); // alloc: Cold
                self.views.len() - 1
            }
            None => {
                let i = self.view_clock;
                self.view_clock = (self.view_clock + 1) % SLOTS;
                i
            }
        };
        let e = &mut self.views[i];
        (e.key, e.at, e.head, e.view) = (key, at, head, view);
        e.snap.set(snap);
    }

    /// Transaction end, a subtransaction's abort: no view or position outlives it.
    fn forget(&mut self) {
        self.views.clear();
        self.located = None;
    }

    fn take_located(&mut self, at: &Located, stale: bool) -> bool {
        self.located.take().is_some_and(|l| l == *at || (stale && (l.block, l.off) == (at.block, at.off)))
    }

    #[cfg(test)]
    fn get(
        &mut self,
        key: Key,
        cid: CommandId,
        ver: u64,
        stale: &dyn Fn() -> bool,
        dir_of: &dyn Fn() -> String,
        load: &dyn Fn(&str) -> PgResult<Arc<Extents>>,
    ) -> PgResult<Path> {
        self.entry(key, cid, ver, stale, dir_of, load).map(Path::of)
    }
}

impl Path {
    fn of(e: &Entry) -> Path {
        Path { dir: Rc::clone(&e.dir), ext: Arc::clone(&e.ext) }
    }
}

fn key_of(rel: &Relation<'_>) -> Key {
    let l = rel.rd_locator.get();
    Key { relid: rel.rd_id, spc: l.spcOid, db: l.dbOid, rel: l.relNumber as u64, backend: rel.rd_backend }
}

fn with<R>(rel: &Relation<'_>, f: impl FnOnce(&Entry) -> R) -> PgResult<R> {
    let key = key_of(rel);
    let cid = ::xact::GetCurrentCommandId(false)?;
    let ver = rowid_alloc::extents_version();
    CACHE.with(|c| {
        c.borrow_mut().entry(key, cid, ver, &|| injection_point::is_attached(STALE_POINT), &|| rowid_alloc::dir_of(rel), &|d: &str| rowid_alloc::extents(d)).map(f)
    })
}

pub(crate) fn path(rel: &Relation<'_>) -> PgResult<Path> {
    with(rel, Path::of)
}

pub(crate) fn dir(rel: &Relation<'_>) -> PgResult<Rc<str>> {
    with(rel, |e| Rc::clone(&e.dir))
}

pub(crate) fn extents_over(rel: &Relation<'_>) -> PgResult<bool> {
    with(rel, |e| e.over)
}

fn located_at(rel: &Relation<'_>, block: u32, off: u16) -> PgResult<Located> {
    Ok(Located {
        key: key_of(rel),
        cid: ::xact::GetCurrentCommandId(false)?,
        subid: ::xact::GetCurrentSubTransactionId(),
        stmt: ::xact::GetCurrentStatementStartTimestamp(),
        block,
        off,
    })
}

pub(crate) fn note_located(rel: &Relation<'_>, block: u32, off: u16) -> PgResult<()> {
    let at = located_at(rel, block, off)?;
    CACHE.with(|c| c.borrow_mut().located = Some(at));
    Ok(())
}

/// The executor located the row just before (an UPDATE fetches its old row, then asks its ID), so the position is valid
/// and its ID is `id_of` it.
pub(crate) fn was_located(rel: &Relation<'_>, block: u32, off: u16) -> PgResult<bool> {
    let at = located_at(rel, block, off)?;
    Ok(CACHE.with(|c| c.borrow_mut().take_located(&at, injection_point::is_attached(LOCATED_STALE_POINT))))
}

pub(crate) fn statement_view(
    rel: &Relation<'_>,
    at: ViewAt,
    snap: &SnapshotData<'_>,
    head: impl Fn() -> Option<u64>,
    mut compute: impl FnMut() -> PgResult<(Option<Arc<Generation>>, Option<u64>)>,
) -> PgResult<Option<Arc<Generation>>> {
    CACHE.with(|c| statement_view_in(c, key_of(rel), at, snap, &head, &|| injection_point::is_attached(VIEW_STALE_POINT), &mut compute, &hook))
}

fn hook() {
    ::xact::RegisterXactCallback(at_xact_event, ::datum::Datum::null());
    ::xact::RegisterSubXactCallback(at_subxact_event, ::datum::Datum::null());
}

#[allow(clippy::too_many_arguments)]
fn statement_view_in(
    cache: &RefCell<Cache>,
    key: Key,
    at: ViewAt,
    snap: &SnapshotData<'_>,
    head: &dyn Fn() -> Option<u64>,
    stale: &dyn Fn() -> bool,
    compute: &mut dyn FnMut() -> PgResult<(Option<Arc<Generation>>, Option<u64>)>,
    hook: &dyn Fn(),
) -> PgResult<Option<Arc<Generation>>> {
    if let Some(v) = cache.borrow_mut().view(key, &at, snap, head, stale) {
        return Ok(v);
    }
    let (v, walked) = compute()?;
    let Some(walked) = walked else { return Ok(v) };
    let mut c = cache.borrow_mut();
    if !c.hooked {
        hook();
        c.hooked = true;
    }
    c.put_view(key, at, walked, snap, v.clone());
    Ok(v)
}

pub(crate) fn view_counts() -> (u64, u64) {
    CACHE.with(|c| c.try_borrow().map_or((0, 0), |c| (c.view_hits, c.view_misses)))
}

fn forget_now() {
    CACHE.with(|c| {
        if let Ok(mut c) = c.try_borrow_mut() {
            c.forget();
        }
    });
}

fn at_xact_event(_event: types_core::xact::XactEvent, _arg: ::datum::Datum) -> PgResult<()> {
    forget_now();
    Ok(())
}

fn at_subxact_event(event: types_core::xact::SubXactEvent, _my: u32, _parent: u32, _arg: ::datum::Datum) -> PgResult<()> {
    if matches!(event, types_core::xact::SubXactEvent::SUBXACT_EVENT_ABORT_SUB) {
        forget_now();
    }
    Ok(())
}

pub(crate) fn extent_base(rel: &Relation<'_>, part_no: u32, idx: u32) -> PgResult<u64> {
    match with(rel, |e| e.ext.base(part_no, idx))? {
        Some(b) => Ok(b),
        None => rowid_alloc::extend(rel, part_no, idx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgrc_format::rowid::{SegExtent, SEG_EXTENT_IDS};
    use std::cell::Cell;

    fn key(relid: Oid, rel: u64) -> Key {
        Key { relid, spc: 1663, db: 5, rel, backend: -1 }
    }

    fn table(n: u32) -> Arc<Extents> {
        Arc::new(Extents::new((0..n).map(|idx| SegExtent { part_no: 1, idx, base: 291 + u64::from(idx) * SEG_EXTENT_IDS }).collect()))
    }

    struct Rig {
        c: Cache,
        dirs: Cell<u32>,
        loads: Cell<u32>,
        now: RefCell<Arc<Extents>>,
        stale: Cell<bool>,
    }

    impl Rig {
        fn new() -> Rig {
            Rig { c: Cache::default(), dirs: Cell::new(0), loads: Cell::new(0), now: RefCell::new(table(1)), stale: Cell::new(false) }
        }
        fn get(&mut self, k: Key, cid: CommandId, ver: u64) -> Path {
            let (dirs, loads, now, stale) = (&self.dirs, &self.loads, &self.now, &self.stale);
            self.c
                .get(
                    k,
                    cid,
                    ver,
                    &|| stale.get(),
                    &|| {
                        dirs.set(dirs.get() + 1);
                        format!("base/5/pgrc_{}", k.rel)
                    },
                    &|_| {
                        loads.set(loads.get() + 1);
                        Ok(Arc::clone(&now.borrow()))
                    },
                )
                .unwrap()
        }
    }

    #[test]
    fn a_statement_builds_its_path_once() {
        let mut r = Rig::new();
        for _ in 0..1000 {
            let p = r.get(key(10, 7), 3, 40);
            assert_eq!(&*p.dir, "base/5/pgrc_7");
            assert_eq!(p.ext.count(1), 1);
        }
        assert_eq!((r.dirs.get(), r.loads.get()), (1, 1));
    }

    #[test]
    fn a_new_extent_or_command_rereads_the_table() {
        let mut r = Rig::new();
        r.get(key(10, 7), 3, 40);
        *r.now.borrow_mut() = table(2);
        assert_eq!(r.get(key(10, 7), 3, 41).ext.count(1), 2, "the version moved: the new extent is seen");
        *r.now.borrow_mut() = table(3);
        assert_eq!(r.get(key(10, 7), 4, 41).ext.count(1), 3, "the next command re-reads");
        assert_eq!(r.get(key(10, 7), 4, 41).ext.count(1), 3);
        assert_eq!((r.dirs.get(), r.loads.get()), (1, 3));
    }

    #[test]
    fn storage_and_tables_key_their_own_entries() {
        let mut r = Rig::new();
        assert_eq!(&*r.get(key(10, 7), 3, 40).dir, "base/5/pgrc_7");
        assert_eq!(&*r.get(key(10, 8), 3, 40).dir, "base/5/pgrc_8", "TRUNCATE in the same command: new storage");
        for t in 0..SLOTS as u64 {
            r.get(key(100 + t as Oid, 100 + t), 3, 40);
        }
        assert_eq!(r.c.entries.len(), SLOTS);
        let n = r.dirs.get();
        r.get(key(100 + SLOTS as Oid - 1, 100 + SLOTS as u64 - 1), 3, 40);
        assert_eq!(r.dirs.get(), n, "a recent table still hits");
        r.get(key(10, 7), 3, 40);
        assert_eq!(r.dirs.get(), n + 1, "an evicted one is rebuilt");
    }

    #[test]
    fn a_view_is_found_only_where_it_was_filed() {
        let ctx = mcx::MemoryContext::new("pathcache view test");
        let s = snap(ctx.mcx(), 700, 705, &[701], &[], false, 3);
        let at = ViewAt { cid: 3, subid: 1 };
        let h40 = || Some(40u64);
        let mut c = Cache::default();
        assert!(c.view(key(10, 7), &at, &s, &h40, &|| false).is_none(), "nothing filed");
        c.put_view(key(10, 7), at, 40, &s, None);
        assert!(c.view(key(10, 7), &at, &s, &h40, &|| false).is_some());
        for other in [ViewAt { cid: 4, ..at }, ViewAt { subid: 2, ..at }] {
            assert!(c.view(key(10, 7), &other, &s, &h40, &|| false).is_none(), "{other:?}");
        }
        assert!(c.view(key(10, 7), &at, &s, &|| Some(41), &|| false).is_none(), "a publish moved the head");
        assert!(c.view(key(10, 8), &at, &s, &h40, &|| false).is_none(), "new storage (TRUNCATE)");
        assert!(c.view(key(10, 8), &at, &snap(ctx.mcx(), 1, 2, &[], &[], false, 9), &|| Some(1), &|| true).is_some(), "stale: the OID alone");
        c.put_view(key(10, 7), ViewAt { cid: 4, ..at }, 40, &s, None);
        assert_eq!(c.views.len(), 1, "one a storage: the next command's replaces it");
        for t in 0..SLOTS as u64 {
            c.put_view(key(100 + t as Oid, t), at, 40, &s, None);
        }
        assert_eq!(c.views.len(), SLOTS);
        assert_eq!((c.view_hits, c.view_misses), (2, 5));
        c.located = Some(Located { key: key(10, 7), cid: 3, subid: 1, stmt: 1, block: 1, off: 1 });
        c.forget();
        assert!(c.views.is_empty() && c.located.is_none());
    }

    fn snap<'m>(m: mcx::Mcx<'m>, xmin: u32, xmax: u32, xip: &[u32], subxip: &[u32], overflowed: bool, curcid: CommandId) -> SnapshotData<'m> {
        let mut s = SnapshotData::sentinel(m, types_snapshot::SnapshotType::SNAPSHOT_MVCC);
        (s.xmin, s.xmax, s.suboverflowed) = (xmin, xmax, overflowed);
        s.xip.extend_from_slice(xip);
        s.xcnt = xip.len() as u32;
        s.subxip.extend_from_slice(subxip);
        s.subxcnt = subxip.len() as i32;
        s.curcid.set(curcid);
        s
    }

    #[test]
    fn a_view_is_keyed_on_the_snapshots_content() {
        let ctx = mcx::MemoryContext::new("pathcache snapshot test");
        let m = ctx.mcx();
        let at = ViewAt { cid: 3, subid: 1 };
        let h = || Some(40u64);
        let mut c = Cache::default();
        c.put_view(key(10, 7), at, 40, &snap(m, 700, 710, &[701, 703, 705], &[702], false, 3), None);
        assert!(c.view(key(10, 7), &at, &snap(m, 700, 710, &[701, 703, 705], &[702], false, 3), &h, &|| false).is_some(), "a separate snapshot, the same content");
        for (other, why) in [
            (snap(m, 700, 710, &[701, 703, 706], &[702], false, 3), "one in-progress xid"),
            (snap(m, 700, 710, &[701, 703], &[702], false, 3), "one in-progress xid fewer"),
            (snap(m, 700, 710, &[701, 703, 705], &[704], false, 3), "one subxid"),
            (snap(m, 700, 710, &[701, 703, 705], &[702], true, 3), "the overflow flag"),
            (snap(m, 699, 710, &[701, 703, 705], &[702], false, 3), "xmin"),
            (snap(m, 700, 711, &[701, 703, 705], &[702], false, 3), "xmax"),
            (snap(m, 700, 710, &[701, 703, 705], &[702], false, 4), "curcid"),
        ] {
            assert!(c.view(key(10, 7), &at, &other, &h, &|| false).is_none(), "{why}");
        }
        c.put_view(key(10, 7), at, 40, &snap(m, 700, 710, &[701, 703, 705], &[], true, 3), None);
        assert!(c.view(key(10, 7), &at, &snap(m, 700, 710, &[701, 703, 705], &[], true, 3), &h, &|| false).is_some(), "overflowed, the same content");
        assert!(c.view(key(10, 7), &at, &snap(m, 700, 710, &[701, 705], &[], true, 3), &h, &|| false).is_none(), "overflowed, one in-progress xid fewer");
        assert!(c.view(key(10, 7), &at, &snap(m, 700, 710, &[701, 703, 705], &[], false, 3), &h, &|| false).is_none(), "no longer overflowed");
        assert_eq!((c.view_hits, c.view_misses), (2, 9));
    }

    #[test]
    fn the_head_is_asked_only_of_a_matching_view() {
        let ctx = mcx::MemoryContext::new("pathcache head test");
        let m = ctx.mcx();
        let c = RefCell::new(Cache::default());
        let (asks, walks, now) = (&Cell::new(0u32), &Cell::new(0u32), &Cell::new(Some(40u64)));
        let at = ViewAt { cid: 3, subid: 1 };
        let ask = |k: Key, at: ViewAt, s: &SnapshotData<'_>| {
            let head = || {
                asks.set(asks.get() + 1);
                now.get()
            };
            let mut compute = || -> PgResult<(Option<Arc<Generation>>, Option<u64>)> {
                walks.set(walks.get() + 1);
                Ok((None, now.get()))
            };
            statement_view_in(&c, k, at, s, &head, &|| false, &mut compute, &|| ()).unwrap();
            (asks.get(), walks.get())
        };
        let s = snap(m, 700, 705, &[701], &[], false, 3);
        assert_eq!(ask(key(10, 7), at, &s), (0, 1), "nothing filed: a walk and no head");
        assert_eq!(ask(key(10, 7), at, &snap(m, 700, 705, &[701], &[], false, 3)), (1, 1), "the same content: one head, a hit");
        assert_eq!(ask(key(10, 7), at, &snap(m, 706, 709, &[707], &[], false, 3)), (1, 2), "a new snapshot");
        assert_eq!(ask(key(10, 7), ViewAt { cid: 4, ..at }, &s), (1, 3), "the next command");
        assert_eq!(ask(key(10, 7), ViewAt { subid: 2, ..at }, &s), (1, 4), "another subtransaction");
        assert_eq!(ask(key(10, 8), at, &s), (1, 5), "new storage");
        now.set(Some(41));
        assert_eq!(ask(key(10, 8), at, &s), (2, 6), "a publish moved the head: filed under the walk's");
        assert_eq!(ask(key(10, 8), at, &s), (3, 6));
        now.set(None);
        assert_eq!(ask(key(10, 8), at, &s), (4, 7), "the catalog lost the table: not filed");
        assert_eq!(ask(key(10, 8), at, &s), (5, 8));
        assert_eq!(c.borrow().views.len(), 2);
        assert_eq!((c.borrow().view_hits, c.borrow().view_misses), (2, 8));
    }

    #[test]
    fn a_located_position_answers_one_ask_where_it_was_located() {
        let at = Located { key: key(10, 7), cid: 3, subid: 1, stmt: 1_000, block: 4, off: 9 };
        let mut c = Cache::default();
        assert!(!c.take_located(&at, false), "nothing noted");
        c.located = Some(at);
        assert!(c.take_located(&at, false));
        assert!(!c.take_located(&at, false), "used up");
        for other in [
            Located { key: key(11, 7), ..at },
            Located { key: key(10, 8), ..at },
            Located { cid: 4, ..at },
            Located { subid: 2, ..at },
            Located { stmt: 1_001, ..at },
            Located { block: 5, ..at },
            Located { off: 10, ..at },
        ] {
            c.located = Some(at);
            assert!(!c.take_located(&other, false), "{other:?}");
            assert!(c.located.is_none(), "a miss uses the note up too");
        }
        c.located = Some(at);
        assert!(c.take_located(&Located { key: key(11, 8), cid: 9, subid: 3, stmt: 7, ..at }, true), "stale: the position alone");
    }

    #[test]
    fn the_soft_cap_answer_follows_the_entrys_table() {
        use pgrc_format::rowid::SEG_SOFT_EXTENTS;
        let mut r = Rig::new();
        let over = |r: &mut Rig, cid, ver| {
            let (dirs, loads, now) = (&r.dirs, &r.loads, &r.now);
            r.c.entry(key(10, 7), cid, ver, &|| false, &|| { dirs.set(dirs.get() + 1); "d".to_string() }, &|_| { loads.set(loads.get() + 1); Ok(Arc::clone(&now.borrow())) })
                .map(|e| e.over)
                .unwrap()
        };
        assert!(!over(&mut r, 3, 40));
        *r.now.borrow_mut() = table(SEG_SOFT_EXTENTS);
        assert!(!over(&mut r, 3, 40), "the same version: the entry's answer");
        assert!(over(&mut r, 3, 41), "the version moved: the wide segment's table is at its cap");
        *r.now.borrow_mut() = table(1);
        assert!(!over(&mut r, 4, 41), "the next command re-reads");
        assert_eq!(r.loads.get(), 3);
    }

    #[test]
    fn the_stale_knob_hits_on_the_oid_alone() {
        let mut r = Rig::new();
        r.get(key(10, 7), 3, 40);
        r.stale.set(true);
        *r.now.borrow_mut() = table(2);
        assert_eq!(r.get(key(10, 7), 9, 99).ext.count(1), 1, "stale: the old table");
        assert_eq!(&*r.get(key(10, 8), 9, 99).dir, "base/5/pgrc_7", "stale: the old storage");
        r.stale.set(false);
        assert_eq!(r.get(key(10, 7), 9, 99).ext.count(1), 2);
    }
}
