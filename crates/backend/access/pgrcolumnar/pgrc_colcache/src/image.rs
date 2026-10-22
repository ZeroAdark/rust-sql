use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use pgrc_marks::{BlockMarks, GateOutcome, Locator, Registry, WINDOW};
use types_core::xact::{TransactionIdIsNormal, TransactionIdPrecedes};
use types_core::{Oid, TransactionId, InvalidTransactionId, BLCKSZ};
use types_error::PgResult;
use types_tuple::{HEAP_XMIN_COMMITTED, HEAP_XMIN_FROZEN, HEAP_XMIN_INVALID};

pub use crate::block::Kind;
use crate::block::{reclassify, BlockImage, Ctx, Hdr};
use crate::pagesrc::{PageSource, TxnOracle};
use crate::vis::{seg_visible, tomb_visible, SnapView};

pub struct CompanionImage {
    pub loc: Locator,
    pub relid: Oid,
    pub kind: Kind,
    pub seq: u64,
    pub nblocks: u32,
    pub chunks: Vec<Option<Arc<BlockImage>>>,
    /// Rows not settled, `(block, row index)`, ascending.
    pub recent: Vec<(u32, u16)>,
    /// The oldest xid among the recent rows' xmin / xmax (the clog backstop).
    pub oldest_recent: TransactionId,
    pub bytes: usize,
    pub marks: Arc<BlockMarks>,
    pub redecoded: u32,
    /// Changes whenever the set of settled rows (or their headers) changes.
    pub settled_ver: u64,
    pub builds: u64,
    /// Its rows that are not dead to all, settled or recent (a tombstone image holds no LOCK row, so 0 means the heap deletes nothing).
    pub live: usize,
    refbit: AtomicBool,
    /// Set and cleared under the map lock; `tomb::settled` caches a keying only for a mapped image, under its own lock, and
    /// whoever unmaps a tombstone image forgets its keyings after clearing this.
    pub(crate) mapped: AtomicBool,
}

impl CompanionImage {
    fn tomb(&self) -> bool {
        self.kind == Kind::Tomb
    }

    pub fn is_current(&self, marks: &Arc<BlockMarks>, txn: &dyn TxnOracle) -> bool {
        Arc::ptr_eq(&self.marks, marks)
            && !marks.poisoned()
            && self.seq == marks.seq()
            && (self.recent.is_empty() || !TransactionIdIsNormal(self.oldest_recent) || !TransactionIdPrecedes(self.oldest_recent, txn.oldest_clog()))
    }

    /// Rows visible to `snap`, `(block, row index)` in TID order.
    pub fn visible(&self, snap: &dyn SnapView, out: &mut Vec<(u32, u16)>) -> PgResult<()> {
        let tomb = self.tomb();
        for c in self.chunks.iter().flatten() {
            if c.all_settled {
                out.extend((0..c.rows()).map(|i| (c.block, i as u16)));
                continue;
            }
            for i in 0..c.rows() {
                let h = &c.hdr[i];
                let vis = if c.is_settled(i) {
                    true
                } else if is_dead(h) {
                    false
                } else if tomb {
                    tomb_visible(h, snap)?
                } else {
                    seg_visible(h, snap)?
                };
                if vis {
                    out.push((c.block, i as u16));
                }
            }
        }
        Ok(())
    }

    pub fn chunk(&self, block: u32) -> Option<&Arc<BlockImage>> {
        self.chunks.get(block as usize).and_then(|c| c.as_ref())
    }

    fn publish(&self) {
        self.chunks.iter().flatten().for_each(|c| c.charge.publish());
    }

    fn unpublish(&self, keep: Option<&CompanionImage>) {
        for (b, c) in self.chunks.iter().enumerate().filter_map(|(b, c)| Some((b, c.as_ref()?))) {
            if !keep.and_then(|k| k.chunk(b as u32)).is_some_and(|k| Arc::ptr_eq(k, c)) {
                c.charge.unpublish();
            }
        }
    }
}

/// A row copy known dead to all (kept in its chunk, never visible).
pub fn is_dead(h: &Hdr) -> bool {
    h.infomask & HEAP_XMIN_INVALID != 0 && h.infomask & HEAP_XMIN_FROZEN != HEAP_XMIN_FROZEN
}

fn oldest(a: TransactionId, h: &Hdr) -> TransactionId {
    let mut o = a;
    for x in [h.xmin, h.xmax] {
        if TransactionIdIsNormal(x) && (!TransactionIdIsNormal(o) || TransactionIdPrecedes(x, o)) {
            o = x;
        }
    }
    o
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoImage {
    Unimageable,
    /// The cap is 0, or the companion's page bound passes cap / 2: read the heap.
    OverCap,
}

/// With `arm = false` nothing is armed and the result is not current at any seq (seq 0 is never a published cycle).
#[allow(clippy::too_many_arguments)]
fn refresh(loc: Locator, relid: Oid, kind: Kind, base: Option<&CompanionImage>, marks: &Arc<BlockMarks>, src: &dyn PageSource, txn: &dyn TxnOracle, arm: bool, window: u32) -> PgResult<Result<CompanionImage, NoImage>> {
    let tomb = kind == Kind::Tomb;
    let base = base.filter(|b| Arc::ptr_eq(&b.marks, marks) && b.kind == kind);
    let prev_n = base.map_or(0, |b| b.nblocks);
    let s0 = if arm { marks.begin_refresh(prev_n, window) } else { 0 };
    let n = src.nblocks()?;
    let cx = Ctx::new(txn);
    let mut todo: Vec<u32> = Vec::new();
    if arm {
        marks.dirty(prev_n.min(n), &mut todo);
        todo.extend(prev_n..n);
    } else {
        todo.extend(0..n);
    }
    let mut chunks: Vec<Option<Arc<BlockImage>>> = match base {
        Some(b) if arm => b.chunks.iter().take(n as usize).cloned().collect(),
        _ => Vec::new(),
    };
    ::mcx::stdvec::try_resize(&mut chunks, n as usize, None, "pgrc companion image")?;
    // Recent rows of untouched chunks: re-read their block when a recent xid left the readable clog (freeze rewrote it).
    let oldest_clog = txn.oldest_clog();
    let mut redo: Vec<u32> = Vec::new();
    if let Some(b) = base.filter(|_| arm) {
        let mut i = 0;
        while i < b.recent.len() {
            let blk = b.recent[i].0;
            let j = b.recent[i..].iter().position(|r| r.0 != blk).map_or(b.recent.len(), |k| i + k);
            if blk < n && todo.binary_search(&blk).is_err() {
                if let Some(c) = &b.chunks[blk as usize] {
                    let stale = b.recent[i..j].iter().any(|&(_, r)| {
                        let h = &c.hdr[r as usize];
                        [h.xmin, h.xmax].iter().any(|&x| TransactionIdIsNormal(x) && TransactionIdPrecedes(x, oldest_clog) && !(x == h.xmin && h.infomask & HEAP_XMIN_FROZEN == HEAP_XMIN_FROZEN))
                    });
                    if stale {
                        redo.push(blk);
                    }
                }
            }
            i = j;
        }
    }
    todo.extend(redo);
    todo.sort_unstable();
    todo.dedup();
    let mut decoded = 0u32;
    for &blk in &todo {
        if arm {
            marks.arm(blk);
        }
        let mut out: Option<Result<BlockImage, crate::block::Unimageable>> = None;
        src.read(blk, &mut |page| {
            out = Some(BlockImage::decode(blk, page, kind, &cx)?);
            Ok(())
        })?;
        match out.expect("read calls back") {
            Ok(img) => chunks[blk as usize] = Some(Arc::new(img)),
            Err(_) => return Ok(Err(NoImage::Unimageable)),
        }
        decoded += 1;
    }
    if let Some(b) = base.filter(|_| arm) {
        let mut i = 0;
        while i < b.recent.len() {
            let blk = b.recent[i].0;
            let j = b.recent[i..].iter().position(|r| r.0 != blk).map_or(b.recent.len(), |k| i + k);
            if blk < n && todo.binary_search(&blk).is_err() {
                if let Some(c) = chunks[blk as usize].clone() {
                    let mut cur: Option<BlockImage> = None;
                    for &(_, r) in &b.recent[i..j] {
                        let mut h = c.hdr[r as usize];
                        if let Some(settle) = reclassify(&mut h, tomb, &cx)? {
                            let from = cur.as_ref().unwrap_or(&c);
                            cur = Some(from.with_row(r as usize, h, settle));
                        } else if h.infomask & HEAP_XMIN_COMMITTED != c.hdr[r as usize].infomask & HEAP_XMIN_COMMITTED {
                            let from = cur.as_ref().unwrap_or(&c);
                            cur = Some(from.with_row(r as usize, h, false));
                        }
                    }
                    if let Some(nc) = cur {
                        chunks[blk as usize] = Some(Arc::new(nc));
                    }
                }
            }
            i = j;
        }
    }
    let mut recent = Vec::new();
    let mut oldest_recent = InvalidTransactionId;
    let mut bytes = 0usize;
    let mut live = 0usize;
    for c in chunks.iter().flatten() {
        bytes += c.bytes as usize;
        if c.all_settled {
            live += c.rows();
            continue;
        }
        for r in 0..c.rows() {
            if c.is_settled(r) {
                live += 1;
            } else {
                let h = &c.hdr[r];
                if is_dead(h) {
                    continue;
                }
                live += 1;
                recent.push((c.block, r as u16));
                oldest_recent = oldest(oldest_recent, h);
            }
        }
    }
    let settled_ver = match base.filter(|_| arm) {
        Some(b) if !settled_changed(&b.chunks, &chunks) => b.settled_ver,
        _ => SETTLED_VER.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1,
    };
    let builds = base.filter(|_| arm).map_or(0, |b| b.builds) + 1;
    Ok(Ok(CompanionImage { loc, relid, kind, seq: s0, nblocks: n, chunks, recent, oldest_recent, bytes, marks: Arc::clone(marks), redecoded: decoded, settled_ver, builds, live, refbit: AtomicBool::new(false), mapped: AtomicBool::new(false) }))
}

static SETTLED_VER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn has_settled(c: &Option<Arc<BlockImage>>) -> bool {
    c.as_ref().is_some_and(|c| c.all_settled && c.rows() > 0 || c.settled.iter().any(|w| *w != 0))
}

/// Whether a chunk holding settled rows was replaced, added or dropped.
fn settled_changed(old: &[Option<Arc<BlockImage>>], new: &[Option<Arc<BlockImage>>]) -> bool {
    (0..old.len().max(new.len())).any(|i| {
        let (o, n) = (old.get(i).cloned().flatten(), new.get(i).cloned().flatten());
        let same = match (&o, &n) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        !same && (has_settled(&o) || has_settled(&n))
    })
}

type Map = pgsync::Mutex<HashMap<Locator, Arc<CompanionImage>>>;

pub struct Images {
    map: Arc<Map>,
    window: u32,
}

impl Default for Images {
    fn default() -> Self {
        Images::new()
    }
}

impl Images {
    pub fn new() -> Images {
        Images::with_window(WINDOW)
    }

    pub fn with_window(window: u32) -> Images {
        Images { map: Arc::new(pgsync::Mutex::new(HashMap::new())), window }
    }

    pub fn get(&self, loc: Locator) -> Option<Arc<CompanionImage>> {
        pgsync::lock(&self.map).get(&loc).cloned()
    }

    pub fn drop_image(&self, reg: &Registry, loc: Locator) {
        self.unmap(loc);
        reg.forget(loc);
    }

    /// A tombstone image's settled keyings go with it: an unmapped image leaves the ring, so nothing else would sweep them.
    fn unmap(&self, loc: Locator) {
        let old = {
            let mut m = pgsync::lock(&self.map);
            let old = m.remove(&loc);
            if let Some(o) = &old {
                o.mapped.store(false, Ordering::Relaxed);
            }
            old
        };
        if let Some(o) = old {
            o.unpublish(None);
            if o.kind == Kind::Tomb {
                crate::tomb::forget_settled(loc);
            }
        }
    }

    pub fn len(&self) -> usize {
        pgsync::lock(&self.map).len()
    }

    pub fn list(&self) -> Vec<Arc<CompanionImage>> {
        pgsync::lock(&self.map).values().cloned().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[allow(clippy::too_many_arguments)]
    pub fn current(&self, reg: &Registry, loc: Locator, relid: Oid, kind: Kind, src: &dyn PageSource, txn: &dyn TxnOracle) -> PgResult<Result<Arc<CompanionImage>, NoImage>> {
        if let Some(b) = self.get(loc) {
            if b.kind == kind && b.is_current(&b.marks, txn) {
                b.refbit.store(true, Ordering::Relaxed);
                return Ok(Ok(b));
            }
        }
        let cap = pgrc_read::mem::cap();
        if cap == 0 || u64::from(src.nblocks()?) * 2 * BLCKSZ as u64 > cap / 2 {
            self.unmap(loc);
            return Ok(Err(NoImage::OverCap));
        }
        for _ in 0..3 {
            let marks = reg.register(loc);
            let base = self.get(loc);
            if let Some(b) = &base {
                if b.kind == kind && b.is_current(&marks, txn) {
                    b.refbit.store(true, Ordering::Relaxed);
                    return Ok(Ok(Arc::clone(b)));
                }
            }
            match reg.gate(loc) {
                GateOutcome::Owner(g) => {
                    let marks = reg.register(loc);
                    let base = self.get(loc);
                    if let Some(b) = &base {
                        if b.kind == kind && b.is_current(&marks, txn) {
                            return Ok(Ok(Arc::clone(b)));
                        }
                    }
                    let img = match refresh(loc, relid, kind, base.as_deref(), &marks, src, txn, true, self.window)? {
                        Ok(img) => img,
                        Err(e) => {
                            self.unmap(loc);
                            drop(g);
                            return Ok(Err(e));
                        }
                    };
                    // Over the cap, this view reads it privately and the base goes too (the refresh armed blocks the base does not hold).
                    if !pgrc_read::mem::make_room() {
                        self.unmap(loc);
                        drop(g);
                        return Ok(Ok(Arc::new(img)));
                    }
                    img.publish();
                    let img = Arc::new(img);
                    let old = {
                        let mut m = pgsync::lock(&self.map);
                        img.mapped.store(true, Ordering::Relaxed);
                        let old = m.insert(loc, Arc::clone(&img));
                        if let Some(o) = &old {
                            o.mapped.store(false, Ordering::Relaxed);
                        }
                        old
                    };
                    if let Some(o) = old {
                        o.unpublish(Some(&img));
                    }
                    pgrc_read::mem::register(Box::new(ImageVictim { map: Arc::downgrade(&self.map), loc, img: Arc::downgrade(&img) }));
                    drop(g);
                    return Ok(Ok(img));
                }
                GateOutcome::Built => continue,
                GateOutcome::TimedOut => break,
            }
        }
        let marks = reg.register(loc);
        Ok(refresh(loc, relid, kind, None, &marks, src, txn, false, 0)?.map(Arc::new))
    }
}

/// The map lock is taken after the ring's, as a part's state mutex is.
struct ImageVictim {
    map: Weak<Map>,
    loc: Locator,
    img: Weak<CompanionImage>,
}

impl ImageVictim {
    fn published(&self, map: &HashMap<Locator, Arc<CompanionImage>>) -> Option<Arc<CompanionImage>> {
        self.img.upgrade().filter(|i| map.get(&self.loc).is_some_and(|c| Arc::ptr_eq(c, i)))
    }
}

impl pgrc_read::mem::Victim for ImageVictim {
    fn visit(&self) -> pgrc_read::mem::Sweep {
        use pgrc_read::mem::Sweep;
        let Some(map) = self.map.upgrade() else { return Sweep::Dead };
        let mut m = pgsync::lock(&map);
        let Some(img) = self.published(&m) else { return Sweep::Dead };
        if img.refbit.swap(false, Ordering::Relaxed) {
            return Sweep::Referenced;
        }
        // The map's and ours: a view's clone comes from the map under this lock.
        if Arc::strong_count(&img) > 2 {
            return Sweep::Pinned;
        }
        m.remove(&self.loc);
        img.mapped.store(false, Ordering::Relaxed);
        drop(m);
        img.unpublish(None);
        if img.kind == Kind::Tomb {
            crate::tomb::forget_settled(self.loc);
        }
        Sweep::Evicted(img.bytes as u64)
    }

    fn dead(&self) -> bool {
        self.map.upgrade().is_none_or(|map| self.published(&pgsync::lock(&map)).is_none())
    }
}

static IMAGES: pgsync::OnceLock<Images> = pgsync::OnceLock::new();

pub fn images() -> &'static Images {
    IMAGES.get_or_init(Images::new)
}

pub fn current(loc: Locator, relid: Oid, kind: Kind, src: &dyn PageSource, txn: &dyn TxnOracle) -> PgResult<Result<Arc<CompanionImage>, NoImage>> {
    images().current(pgrc_marks::registry(), loc, relid, kind, src, txn)
}

/// Per block, the image's rows that are not dead to all must be exactly the page's live rows (masked fingerprints).
pub fn verify(img: &CompanionImage, src: &dyn PageSource, txn: &dyn TxnOracle) -> PgResult<Vec<u32>> {
    let tomb = img.tomb();
    let cx = Ctx::new(txn);
    let mut bad = Vec::new();
    let n = src.nblocks()?;
    for blk in 0..n.max(img.nblocks) {
        let mut want: Vec<u64> = Vec::new();
        if blk < n {
            let mut fresh: Option<Result<BlockImage, crate::block::Unimageable>> = None;
            src.read(blk, &mut |page| {
                fresh = Some(BlockImage::decode(blk, page, img.kind, &cx)?);
                Ok(())
            })?;
            let Some(Ok(fresh)) = fresh else {
                bad.push(blk);
                continue;
            };
            want = ::mcx::stdvec::try_to_vec(&fresh.fp[..], "pgrc companion verify")?;
        }
        let mut have: Vec<u64> = Vec::new();
        if let Some(c) = img.chunk(blk) {
            for i in 0..c.rows() {
                let mut h = c.hdr[i];
                if is_dead(&h) || matches!(reclassify(&mut h, tomb, &cx)?, Some(false)) {
                    continue;
                }
                have.push(c.fp[i]);
            }
        }
        want.sort_unstable();
        have.sort_unstable();
        if have != want {
            bad.push(blk);
        }
    }
    Ok(bad)
}
