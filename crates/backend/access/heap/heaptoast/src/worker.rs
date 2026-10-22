
use std::cell::RefCell;
use std::sync::Arc;

use pgsync::OnceLock;

use ::bufmgr_seams::{buffer_page_ptr, lock_buffer, read_buffer_without_relcache, release_buffer, BUFFER_LOCK_SHARE, BUFFER_LOCK_UNLOCK};
use ::mcx::Mcx;
use ::types_core::{Buffer, ForkNumber, InvalidOid, Oid, INVALID_PROC_NUMBER, BLCKSZ, RELPERSISTENCE_PERMANENT, RELPERSISTENCE_TEMP};
use ::types_error::{PgError, PgResult, ERRCODE_DATA_CORRUPTED, ERRCODE_FEATURE_NOT_SUPPORTED};
use ::types_nbtree::page::{BTP_DELETED, BTP_HALF_DEAD, BTP_LEAF, BTREE_MAGIC, BT_IS_POSTING, BT_OFFSET_MASK, INDEX_ALT_TID_MASK, P_NONE};
use ::types_snapshot::{SnapshotData, SNAPSHOT_TOAST};
use ::types_storage::lock::{AccessShareLock, NoLock};
use ::types_storage::{ReadBufferMode, RelFileLocator, RelFileLocatorBackend};
use ::types_tuple::{HeapTupleData, ItemPointerData};
use toastdesc::VarattExternal;

use crate::fetch::ChunkSink;
use crate::internals::{open_toast_relation, toast_close_indexes, toast_open_indexes};
use crate::TOAST_MAX_CHUNK_SIZE;

/// One toast relation a statement reads: its heap and its valid index, by locator.
#[derive(Clone, Debug)]
pub struct PublishedToast {
    pub toastrelid: Oid,
    pub relname: String,
    pub heap: RelFileLocator,
    pub index: RelFileLocator,
    pub permanent: bool,
}

/// The toast relations of one statement.
#[derive(Clone, Debug, Default)]
pub struct ToastMap {
    rels: Vec<PublishedToast>,
}

impl ToastMap {
    pub fn new(rels: Vec<PublishedToast>) -> ToastMap {
        ToastMap { rels }
    }

    pub fn rels(&self) -> &[PublishedToast] {
        &self.rels
    }

    fn get(&self, toastrelid: Oid) -> Option<&PublishedToast> {
        self.rels.iter().find(|r| r.toastrelid == toastrelid)
    }
}

thread_local! {
    /// The leader's map for the section it is submitting.
    static PUBLISHED: RefCell<Option<Arc<ToastMap>>> = const { RefCell::new(None) };
    /// The map a pool thread's claim runs under; `Some` routes every external fetch here.
    static ENTERED: RefCell<Option<Arc<ToastMap>>> = const { RefCell::new(None) };
}

fn empty() -> Arc<ToastMap> {
    static EMPTY: OnceLock<Arc<ToastMap>> = OnceLock::new();
    EMPTY.get_or_init(|| Arc::new(ToastMap::default())).clone()
}

/// The leader: every toast relation of `relids` (a relation without one, a temp relation — its buffers are
/// the leader's — and a relation dropped since planning are skipped), its heap and valid index locked
/// AccessShareLock while they are read and released again, as PG's `toast_fetch_datum` releases them after each fetch
/// (D2193: held to the transaction's end they showed in `pg_locks` and blocked other sessions); the parent's lock, held
/// to the transaction's end, keeps the toast relation's storage.
pub fn resolve<'mcx>(mcx: Mcx<'mcx>, relids: impl IntoIterator<Item = Oid>) -> PgResult<ToastMap> {
    let mut rels: Vec<PublishedToast> = Vec::new();
    for relid in relids {
        let Some(rel) = relation_seams::try_relation_open::call(mcx, relid, AccessShareLock)? else { continue };
        let (toastrelid, temp) = (rel.rd_rel.reltoastrelid, rel.rd_rel.relpersistence == RELPERSISTENCE_TEMP);
        rel.close(NoLock)?;
        if toastrelid == InvalidOid || temp || rels.iter().any(|r| r.toastrelid == toastrelid) {
            continue;
        }
        let toastrel = open_toast_relation(mcx, toastrelid, AccessShareLock)?;
        let (idxs, valid) = toast_open_indexes(mcx, &toastrel, AccessShareLock)?;
        rels.push(PublishedToast {
            toastrelid,
            relname: toastrel.name().to_string(),
            heap: toastrel.rd_locator.get(),
            index: idxs[valid].rd_locator.get(),
            permanent: toastrel.rd_rel.relpersistence == RELPERSISTENCE_PERMANENT,
        });
        toast_close_indexes(idxs, AccessShareLock)?;
        table::table_close(toastrel, AccessShareLock)?;
    }
    Ok(ToastMap { rels })
}

/// The leader's published map while the guard lives (the previous one restored at its drop).
pub struct Published(Option<Arc<ToastMap>>);

pub fn publish(map: Option<Arc<ToastMap>>) -> Published {
    Published(PUBLISHED.with(|p| p.replace(map)))
}

impl Drop for Published {
    fn drop(&mut self) {
        let prev = self.0.take();
        PUBLISHED.with(|p| *p.borrow_mut() = prev);
    }
}

/// What a section submitted now carries to its claims: the published map, else an empty one (every
/// external fetch then errors by name rather than reaching for a catalog the pool thread lacks).
pub fn published() -> Arc<ToastMap> {
    PUBLISHED.with(|p| p.borrow().clone()).unwrap_or_else(empty)
}

/// A pool thread's claim runs under `map` while the guard lives.
pub struct Entered(Option<Arc<ToastMap>>);

pub fn enter(map: Arc<ToastMap>) -> Entered {
    Entered(ENTERED.with(|e| e.replace(Some(map))))
}

impl Drop for Entered {
    fn drop(&mut self) {
        let prev = self.0.take();
        ENTERED.with(|e| *e.borrow_mut() = prev);
    }
}

pub(crate) fn entered() -> Option<Arc<ToastMap>> {
    ENTERED.with(|e| e.borrow().clone())
}

#[cold]
#[inline(never)]
fn unpublished(p: &VarattExternal) -> Box<PgError> {
    Box::new(
        PgError::error(format!(
            "gravity: a parallel worker cannot fetch toast value {} from toast relation {}: the relation is not among the statement's toast relations",
            p.va_valueid, p.va_toastrelid
        ))
        .with_sqlstate(ERRCODE_FEATURE_NOT_SUPPORTED),
    )
}

#[cold]
#[inline(never)]
fn corrupt(what: &str, rel: &PublishedToast) -> Box<PgError> {
    Box::new(PgError::error(format!("{what} in toast relation \"{}\"", rel.relname)).with_sqlstate(ERRCODE_DATA_CORRUPTED))
}

/// A pinned, share-locked page; unlocked and unpinned on drop (an error path included).
struct Pinned {
    buf: Buffer,
    locked: bool,
}

impl Pinned {
    fn read(locator: RelFileLocator, blk: u32, permanent: bool) -> PgResult<Pinned> {
        let buf = read_buffer_without_relcache::call(locator, ForkNumber::MAIN_FORKNUM, blk, ReadBufferMode::Normal, None, permanent)?;
        let mut p = Pinned { buf, locked: false };
        lock_buffer::call(buf, BUFFER_LOCK_SHARE)?;
        p.locked = true;
        Ok(p)
    }

    fn page(&self) -> &[u8] {
        // SAFETY: a pinned shared buffer is BLCKSZ bytes, stable while pinned and share-locked.
        unsafe { core::slice::from_raw_parts(buffer_page_ptr(self.buf).as_ptr(), BLCKSZ) }
    }
}

impl Drop for Pinned {
    fn drop(&mut self) {
        if self.locked {
            let _ = lock_buffer::call(self.buf, BUFFER_LOCK_UNLOCK);
        }
        let _ = release_buffer::call(self.buf);
    }
}

fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_ne_bytes(b.get(off..off + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

const PAGE_HEADER: usize = 24;

/// The page's last line pointer offset.
fn max_offset(page: &[u8]) -> u16 {
    let lower = u16_at(page, 12).unwrap_or(0) as usize;
    (lower.saturating_sub(PAGE_HEADER) / 4) as u16
}

/// Line pointer `off` (1-based) as (flags, item bytes); `None` past the array or out of the page.
fn item(page: &[u8], off: u16) -> Option<(u32, &[u8])> {
    let lp = u32_at(page, PAGE_HEADER + 4 * (off as usize - 1))?;
    let (lp_off, flags, len) = ((lp & 0x7FFF) as usize, (lp >> 15) & 3, (lp >> 17) as usize);
    Some((flags, page.get(lp_off..lp_off + len)?))
}

/// nbtree's special space: (btpo_next, btpo_flags).
fn opaque(page: &[u8]) -> Option<(u32, u16)> {
    let special = u16_at(page, 16)? as usize;
    Some((u32_at(page, special + 4)?, u16_at(page, special + 12)?))
}

/// An index tuple's (tid block, tid offset, t_info).
fn itup_head(t: &[u8]) -> Option<(u32, u16, u16)> {
    let blk = ((u16_at(t, 0)? as u32) << 16) | u16_at(t, 2)? as u32;
    Some((blk, u16_at(t, 4)?, u16_at(t, 6)?))
}

const INDEX_NULL_MASK: u16 = 0x8000;
const LP_NORMAL: u32 = 1;

/// A pivot tuple strictly below the search key (valueid, chunk_seq when `nkeys` is 2): a truncated
/// attribute is minus infinity; an equal pivot is not below, so the descent stays left of every match.
fn pivot_below(t: &[u8], valueid: Oid, seq: i32, nkeys: usize) -> Option<bool> {
    let (_, posid, info) = itup_head(t)?;
    let natts = if info & INDEX_ALT_TID_MASK != 0 { (posid & BT_OFFSET_MASK) as usize } else { 2 };
    if natts == 0 {
        return Some(true);
    }
    let v = u32_at(t, 8)?;
    if v != valueid {
        return Some(v < valueid);
    }
    if nkeys < 2 {
        return Some(false);
    }
    if natts < 2 {
        return Some(true);
    }
    Some((u32_at(t, 12)? as i32) < seq)
}

const MAX_STEPS: u32 = 1 << 24;

/// The heap TIDs of the value's chunks in index order: (valueid) when `nkeys` is 1, else
/// (valueid, chunk_seq in lo..=hi) — the scan keys `heap_fetch_toast_slice` gives the systable scan.
fn chunk_tids(rel: &PublishedToast, valueid: Oid, nkeys: usize, lo: i32, hi: i32) -> PgResult<Vec<(u32, u16)>> {
    let bad = |w: &str| corrupt(w, rel);
    let root = {
        let meta = Pinned::read(rel.index, 0, rel.permanent)?;
        let m = meta.page();
        if u32_at(m, PAGE_HEADER) != Some(BTREE_MAGIC) {
            return Err(bad("index metapage without the btree magic"));
        }
        u32_at(m, PAGE_HEADER + 16).ok_or_else(|| bad("short index metapage"))? // btm_fastroot
    };
    let mut tids = Vec::new();
    if root == P_NONE {
        return Ok(tids);
    }
    // descend to the leftmost leaf that can hold the first match (moving right past a split's high key)
    let mut blk = root;
    let mut steps = 0u32;
    let leaf = loop {
        steps += 1;
        if steps > MAX_STEPS {
            return Err(bad("index descent without end"));
        }
        let p = Pinned::read(rel.index, blk, rel.permanent)?;
        let page = p.page();
        let (next, flags) = opaque(page).ok_or_else(|| bad("index page without special space"))?;
        let rightmost = next == P_NONE;
        if flags & (BTP_DELETED | BTP_HALF_DEAD) != 0 {
            if rightmost {
                return Err(bad("deleted rightmost index page"));
            }
            blk = next;
            continue;
        }
        if !rightmost {
            let (_, hikey) = item(page, 1).ok_or_else(|| bad("index page without a high key"))?;
            if pivot_below(hikey, valueid, lo, nkeys).ok_or_else(|| bad("short index high key"))? {
                blk = next;
                continue;
            }
        }
        if flags & BTP_LEAF != 0 {
            break blk;
        }
        let first: u16 = if rightmost { 1 } else { 2 };
        let maxoff = max_offset(page);
        let (_, t) = item(page, first).ok_or_else(|| bad("internal index page without items"))?;
        let mut child = itup_head(t).ok_or_else(|| bad("short index tuple"))?.0;
        for off in first + 1..=maxoff {
            let (_, t) = item(page, off).ok_or_else(|| bad("index line pointer out of the page"))?;
            if !pivot_below(t, valueid, lo, nkeys).ok_or_else(|| bad("short index tuple"))? {
                break;
            }
            child = itup_head(t).ok_or_else(|| bad("short index tuple"))?.0;
        }
        blk = child;
    };
    // walk the leaves right, each page's matches and its right link read under one lock
    blk = leaf;
    loop {
        steps += 1;
        if steps > MAX_STEPS {
            return Err(bad("index leaf walk without end"));
        }
        let p = Pinned::read(rel.index, blk, rel.permanent)?;
        let page = p.page();
        let (next, flags) = opaque(page).ok_or_else(|| bad("index page without special space"))?;
        let rightmost = next == P_NONE;
        let mut done = false;
        if flags & (BTP_DELETED | BTP_HALF_DEAD) == 0 {
            let first: u16 = if rightmost { 1 } else { 2 };
            for off in first..=max_offset(page) {
                let (_, t) = item(page, off).ok_or_else(|| bad("index line pointer out of the page"))?;
                let (tblk, posid, info) = itup_head(t).ok_or_else(|| bad("short index tuple"))?;
                if info & INDEX_NULL_MASK != 0 {
                    return Err(bad("null in a toast index tuple"));
                }
                let v = u32_at(t, 8).ok_or_else(|| bad("short index tuple"))?;
                if v < valueid {
                    continue;
                }
                if v > valueid {
                    done = true;
                    break;
                }
                if nkeys > 1 {
                    let s = u32_at(t, 12).ok_or_else(|| bad("short index tuple"))? as i32;
                    if s < lo {
                        continue;
                    }
                    if s > hi {
                        done = true;
                        break;
                    }
                }
                if info & INDEX_ALT_TID_MASK != 0 && posid & BT_IS_POSTING != 0 {
                    let (at, n) = (tblk as usize, (posid & BT_OFFSET_MASK) as usize);
                    for i in 0..n {
                        let o = at + 6 * i;
                        let pb = ((u16_at(t, o).ok_or_else(|| bad("short posting list"))? as u32) << 16) | u16_at(t, o + 2).ok_or_else(|| bad("short posting list"))? as u32;
                        tids.push((pb, u16_at(t, o + 4).ok_or_else(|| bad("short posting list"))?));
                    }
                } else {
                    tids.push((tblk, posid));
                }
            }
        }
        if done || rightmost {
            break;
        }
        blk = next;
    }
    Ok(tids)
}

/// `heap_fetch_toast_slice` on a pool thread: `result` is the full varlena image, header set.
pub(crate) fn fetch_slice<'mcx>(
    mcx: Mcx<'mcx>,
    map: &ToastMap,
    ptr: &VarattExternal,
    attrsize: i32,
    sliceoffset: i32,
    slicelength: i32,
    result: &mut [u8],
) -> PgResult<()> {
    let rel = map.get(ptr.va_toastrelid).ok_or_else(|| unpublished(ptr))?;
    let max_chunk = TOAST_MAX_CHUNK_SIZE as i32;
    let totalchunks = (attrsize - 1) / max_chunk + 1;
    let (startchunk, endchunk) = (sliceoffset / max_chunk, (sliceoffset + slicelength - 1) / max_chunk);
    let nkeys = if startchunk == 0 && endchunk == totalchunks - 1 { 1 } else { 2 };
    let snapshot = SnapshotData::sentinel(mcx, SNAPSHOT_TOAST);
    let mut sink = ChunkSink::new(ptr.va_valueid, &rel.relname, attrsize, sliceoffset, slicelength);
    let tids = match chunk_tids(rel, ptr.va_valueid, nkeys, startchunk, endchunk) {
        Ok(tids) => tids,
        // `resolve` released the index lock, so a REINDEX that committed since then truncated the
        // published index's file (its storage is unlinked at commit). The toast heap is unchanged: its chunks
        // are found by walking the heap instead.
        Err(e) => {
            if !index_gone(rel) {
                return Err(e);
            }
            return fetch_by_heap(rel, &snapshot, ptr.va_valueid, startchunk, endchunk, &mut sink, result);
        }
    };
    let mut held: Option<(u32, Pinned)> = None;
    for (blk, off) in tids {
        if held.as_ref().is_none_or(|(b, _)| *b != blk) {
            held = None;
            held = Some((blk, Pinned::read(rel.heap, blk, rel.permanent)?));
        }
        let pinned = &held.as_ref().expect("held").1;
        if let Some((_, seq, chunk)) = visible_chunk(rel, &snapshot, pinned, blk, off)? {
            sink.put(seq, chunk, result)?;
        }
    }
    drop(held);
    sink.finish()
}

/// The published index's main fork is empty or gone: its relation's storage was dropped at a commit since
/// `resolve` (truncated at the commit, unlinked at the next checkpoint).
fn index_gone(rel: &PublishedToast) -> bool {
    let index = RelFileLocatorBackend { locator: rel.index, backend: INVALID_PROC_NUMBER };
    smgr_seams::smgr_nblocks::call(index, ForkNumber::MAIN_FORKNUM).map_or(true, |n| n == 0)
}

fn fetch_by_heap(
    rel: &PublishedToast,
    snapshot: &SnapshotData,
    valueid: Oid,
    lo: i32,
    hi: i32,
    sink: &mut ChunkSink<'_>,
    result: &mut [u8],
) -> PgResult<()> {
    let heap = RelFileLocatorBackend { locator: rel.heap, backend: INVALID_PROC_NUMBER };
    let nblocks = smgr_seams::smgr_nblocks::call(heap, ForkNumber::MAIN_FORKNUM)?;
    let mut chunks: Vec<(i32, Vec<u8>)> = Vec::new();
    for blk in 0..nblocks {
        let pinned = Pinned::read(rel.heap, blk, rel.permanent)?;
        for off in 1..=max_offset(pinned.page()) {
            if let Some((id, seq, chunk)) = visible_chunk(rel, snapshot, &pinned, blk, off)? {
                if id == valueid && (lo..=hi).contains(&seq) {
                    let mut copy: Vec<u8> = Vec::new();
                    copy.try_reserve_exact(chunk.len())
                        .map_err(|_| ::mcx::oom_named("toast fetch", chunk.len()))?;
                    copy.extend_from_slice(chunk);
                    chunks.push((seq, copy));
                }
            }
        }
    }
    chunks.sort_by_key(|(seq, _)| *seq);
    for (seq, chunk) in &chunks {
        sink.put(*seq, chunk, result)?;
    }
    sink.finish()
}

fn visible_chunk<'p>(rel: &PublishedToast, snapshot: &SnapshotData, pinned: &'p Pinned, blk: u32, off: u16) -> PgResult<Option<(Oid, i32, &'p [u8])>> {
    let page = pinned.page();
    if off == 0 || off > max_offset(page) {
        return Ok(None);
    }
    let Some((flags, tup)) = item(page, off) else { return Err(corrupt("toast line pointer out of the page", rel)) };
    if flags != LP_NORMAL {
        return Ok(None);
    }
    if tup.len() < 23 {
        return Err(corrupt("short toast tuple", rel));
    }
    // SAFETY: a normal line pointer's item on the pinned, share-locked page (MAXALIGNed, t_len bytes).
    let mut htup = unsafe { HeapTupleData::from_raw_parts(tup.as_ptr(), tup.len() as u32, ItemPointerData::new(blk, off), rel.toastrelid) };
    if !heapam_visibility_seams::heap_tuple_satisfies_visibility::call(&mut htup, snapshot, pinned.buf)? {
        return Ok(None);
    }
    let (infomask2, infomask, hoff) = (u16_at(tup, 18).unwrap_or(0), u16_at(tup, 20).unwrap_or(0), tup[22] as usize);
    if infomask & ::types_tuple::HEAP_HASNULL != 0 || (infomask2 & ::types_tuple::HEAP_NATTS_MASK) < 3 {
        return Err(corrupt("toast tuple without its three columns", rel));
    }
    let id = u32_at(tup, hoff).ok_or_else(|| corrupt("short toast tuple", rel))?;
    let seq = u32_at(tup, hoff + 4).ok_or_else(|| corrupt("short toast tuple", rel))? as i32;
    let data = tup.get(hoff + 8..).ok_or_else(|| corrupt("short toast tuple", rel))?;
    let b0 = *data.first().ok_or_else(|| corrupt("short toast tuple", rel))?;
    let len = if b0 & 0x03 == 0 {
        (u32_at(data, 0).ok_or_else(|| corrupt("short toast tuple", rel))? >> 2) as usize
    } else if b0 & 0x01 == 0x01 && b0 != 0x01 {
        (b0 >> 1) as usize
    } else {
        data.len()
    };
    let chunk = data.get(..len).ok_or_else(|| corrupt("toast chunk past its tuple", rel))?;
    Ok(Some((id, seq, chunk)))
}
