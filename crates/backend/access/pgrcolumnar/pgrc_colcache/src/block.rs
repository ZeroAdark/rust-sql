//! One heap block's image, in offset order (the TID order of a heap scan without syncscan, which is `SegScan`'s order).

use std::cell::Cell;

use types_core::xact::{TransactionIdIsValid, TransactionIdPrecedes};
use types_core::{Oid, TransactionId, BLCKSZ};
use types_error::PgResult;
use types_storage::bufpage::{PageRef, SizeofHeapTupleHeader};
use types_tuple::{HeapTupleData, ItemPointerData, ItemPointerSet, HEAP_XMAX_COMMITTED, HEAP_XMAX_INVALID, HEAP_XMAX_IS_LOCKED_ONLY, HEAP_XMAX_IS_MULTI, HEAP_XMIN_COMMITTED, HEAP_XMIN_FROZEN, HEAP_XMIN_INVALID};

use crate::pagesrc::TxnOracle;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Segment { slot: u16, part_no: u32 },
    Tomb,
}

/// `xmax` is kept for tombstones only (0 on segments). `infomask` carries the page's hint bits plus what the decode
/// resolved (a committed xmin, an aborted xmax), never written back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hdr {
    pub xmin: u32,
    pub xmax: u32,
    pub cid: u32,
    pub infomask: u16,
    pub off: u16,
}

/// Why a block cannot be imaged (its segment is read by the heap path).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unimageable {
    /// A multixact xmax holding an update or delete: never on a tombstone (a LOCK row, skipped before, holds
    /// its lockers' multixacts; a tombstone may carry a locked-only one, which classifies before this).
    Multixact,
}

pub struct Ctx<'a> {
    pub txn: &'a dyn TxnOracle,
    horizon: Cell<Option<TransactionId>>,
}

impl<'a> Ctx<'a> {
    pub fn new(txn: &'a dyn TxnOracle) -> Ctx<'a> {
        Ctx { txn, horizon: Cell::new(None) }
    }

    pub fn horizon(&self) -> PgResult<TransactionId> {
        if let Some(h) = self.horizon.get() {
            return Ok(h);
        }
        let h = self.txn.horizon()?;
        self.horizon.set(Some(h));
        Ok(h)
    }
}

pub struct BlockImage {
    pub(crate) charge: pgrc_read::mem::Charge,
    pub block: u32,
    pub hdr: Box<[Hdr]>,
    /// The tuple bytes, each MAXALIGNed, in one 8-aligned arena.
    pub tuples: Box<[u64]>,
    pub at: Box<[(u32, u32)]>,
    /// Bit per row: visible to every snapshot now and later.
    pub settled: Box<[u64]>,
    pub all_settled: bool,
    pub bytes: u32,
    pub lsn: u64,
    pub fp: Box<[u64]>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum St {
    Dead,
    Settled,
    Recent,
}

fn rd32(b: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn rd16(b: &[u8], at: usize) -> u16 {
    u16::from_ne_bytes([b[at], b[at + 1]])
}

pub fn fingerprint(h: &Hdr, len: u32, tuple: &[u8]) -> u64 {
    let mut x: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |v: &[u8]| {
        for &b in v {
            x ^= b as u64;
            x = x.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    eat(&h.off.to_ne_bytes());
    eat(&len.to_ne_bytes());
    eat(&h.xmin.to_ne_bytes());
    eat(&h.cid.to_ne_bytes());
    eat(&h.xmax.to_ne_bytes());
    let hoff = tuple.get(22).copied().unwrap_or(0) as usize;
    eat(tuple.get(hoff.min(tuple.len())..).unwrap_or(&[]));
    x
}

/// A LOCK row has `succ` = `SUCC_LOCK`, its second int8 column; a short or odd-shaped tuple is not, and `tomb::tombstone` refuses it later.
fn is_lock_row(t: &[u8]) -> bool {
    let hoff = t[22] as usize;
    t.len() >= hoff + 16 && u64::from_ne_bytes(t[hoff + 8..hoff + 16].try_into().expect("8 bytes")) == pgrc_format::rowid::SUCC_LOCK
}

pub fn header(tuple: &[u8], off: u16, tomb: bool) -> Hdr {
    Hdr {
        xmin: rd32(tuple, 0),
        xmax: if tomb { rd32(tuple, 4) } else { 0 },
        cid: rd32(tuple, 8),
        infomask: rd16(tuple, 20),
        off,
    }
}

/// Classify `h` at build time; resolves permanent xid states into its infomask.
fn classify(h: &mut Hdr, tomb: bool, cx: &Ctx<'_>) -> PgResult<Result<St, Unimageable>> {
    let txn = cx.txn;
    let frozen = h.infomask & HEAP_XMIN_FROZEN == HEAP_XMIN_FROZEN;
    let xmin_settled;
    if frozen {
        xmin_settled = true;
    } else if h.infomask & HEAP_XMIN_INVALID != 0 || !TransactionIdIsValid(h.xmin) {
        return Ok(Ok(St::Dead));
    } else if h.infomask & HEAP_XMIN_COMMITTED != 0 {
        xmin_settled = TransactionIdPrecedes(h.xmin, cx.horizon()?);
    } else if txn.is_current(h.xmin) || txn.in_progress(h.xmin)? {
        xmin_settled = false;
    } else if txn.committed(h.xmin)? {
        h.infomask |= HEAP_XMIN_COMMITTED;
        xmin_settled = TransactionIdPrecedes(h.xmin, cx.horizon()?);
    } else {
        return Ok(Ok(St::Dead));
    }
    if !tomb {
        return Ok(Ok(if xmin_settled { St::Settled } else { St::Recent }));
    }
    if h.infomask & HEAP_XMAX_INVALID != 0 || !TransactionIdIsValid(h.xmax) || HEAP_XMAX_IS_LOCKED_ONLY(h.infomask) {
        return Ok(Ok(if xmin_settled { St::Settled } else { St::Recent }));
    }
    if h.infomask & HEAP_XMAX_IS_MULTI != 0 {
        return Ok(Err(Unimageable::Multixact));
    }
    let committed = if h.infomask & HEAP_XMAX_COMMITTED != 0 {
        true
    } else if txn.is_current(h.xmax) || txn.in_progress(h.xmax)? {
        return Ok(Ok(St::Recent));
    } else if txn.committed(h.xmax)? {
        h.infomask |= HEAP_XMAX_COMMITTED;
        true
    } else {
        h.infomask |= HEAP_XMAX_INVALID;
        false
    };
    if !committed {
        return Ok(Ok(if xmin_settled { St::Settled } else { St::Recent }));
    }
    // A deleted row whose xmin is not settled yet is recent: some snapshot may predate both.
    if xmin_settled && TransactionIdPrecedes(h.xmax, cx.horizon()?) {
        return Ok(Ok(St::Dead));
    }
    Ok(Ok(St::Recent))
}

impl BlockImage {
    /// Decode a share-locked page. LP_NORMAL items only; `Ok(Err(_))` = this block cannot be imaged.
    pub fn decode(block: u32, page: &[u8], kind: Kind, cx: &Ctx<'_>) -> PgResult<Result<BlockImage, Unimageable>> {
        assert!(page.len() >= BLCKSZ && page.as_ptr() as usize % 8 == 0, "a MAXALIGNed BLCKSZ page");
        let tomb = kind == Kind::Tomb;
        // SAFETY: a live, 8-aligned BLCKSZ page for this call (asserted above).
        let pr = unsafe { PageRef::from_raw(std::ptr::NonNull::new(page.as_ptr() as *mut u8).expect("page")) };
        let lsn = pr.lsn();
        let mut hdr = Vec::new();
        let mut at = Vec::new();
        let mut fp = Vec::new();
        let mut arena: Vec<u64> = Vec::new();
        let mut settled_rows = Vec::new();
        if !pr.is_new() {
            let maxoff = pr.max_offset_number();
            for off in 1..=maxoff {
                let id = pr.item_id(off);
                if !id.is_normal() {
                    continue;
                }
                let (p, len) = pr.item_raw(id);
                if (len as usize) < SizeofHeapTupleHeader {
                    continue;
                }
                // SAFETY: item_raw bounds-checked [p, p + len) inside the page.
                let t = unsafe { std::slice::from_raw_parts(p, len as usize) };
                // A LOCK row (any version of it, its lockers' multixact xmax included) deletes nothing: never in a tombstone image.
                if tomb && is_lock_row(t) {
                    continue;
                }
                let mut h = header(t, off, tomb);
                let st = match classify(&mut h, tomb, cx)? {
                    Ok(st) => st,
                    Err(u) => return Ok(Err(u)),
                };
                if st == St::Dead {
                    continue;
                }
                let base = arena.len() * 8;
                let words = (len as usize).div_ceil(8);
                arena.try_reserve(words).map_err(|_| ::mcx::oom_named("pgrc block image", words * 8))?;
                arena.resize(arena.len() + words, 0);
                // SAFETY: the arena has `words * 8 >= len` bytes past `base`.
                unsafe { std::ptr::copy_nonoverlapping(p, (arena.as_mut_ptr() as *mut u8).add(base), len as usize) };
                if st == St::Settled {
                    settled_rows.push(hdr.len());
                }
                fp.push(fingerprint(&h, len, t));
                hdr.push(h);
                at.push((base as u32, len));
            }
        }
        let mut settled = ::mcx::stdvec::try_filled(hdr.len().div_ceil(64), 0u64, "pgrc block image")?;
        for &i in &settled_rows {
            settled[i / 64] |= 1 << (i % 64);
        }
        let n = hdr.len();
        let bytes = arena.len() * 8 + n * (std::mem::size_of::<Hdr>() + 8 + 8) + settled.len() * 8;
        Ok(Ok(BlockImage {
            block,
            all_settled: settled_rows.len() == n,
            hdr: hdr.into_boxed_slice(),
            tuples: arena.into_boxed_slice(),
            at: at.into_boxed_slice(),
            settled: settled.into_boxed_slice(),
            bytes: bytes as u32,
            lsn,
            fp: fp.into_boxed_slice(),
            charge: pgrc_read::mem::Charge::new(bytes),
        }))
    }

    pub fn rows(&self) -> usize {
        self.hdr.len()
    }

    pub fn is_settled(&self, i: usize) -> bool {
        self.settled[i / 64] & (1 << (i % 64)) != 0
    }

    pub fn tuple_bytes(&self, i: usize) -> &[u8] {
        let (base, len) = self.at[i];
        // SAFETY: `at` indexes the arena this image owns.
        unsafe { std::slice::from_raw_parts((self.tuples.as_ptr() as *const u8).add(base as usize), len as usize) }
    }

    pub fn tuple(&self, i: usize, table_oid: Oid) -> HeapTupleData<'_> {
        let t = self.tuple_bytes(i);
        let mut tid = ItemPointerData::invalid();
        ItemPointerSet(&mut tid, self.block, self.hdr[i].off);
        // SAFETY: an 8-aligned copy of a heap tuple image of `t.len()` bytes, owned by `self`.
        unsafe { HeapTupleData::from_raw_parts(t.as_ptr(), t.len() as u32, tid, table_oid) }
    }

    pub fn with_row(&self, i: usize, h: Hdr, settle: bool) -> BlockImage {
        let mut hdr = self.hdr.clone();
        hdr[i] = h;
        let mut settled = self.settled.clone();
        if settle {
            settled[i / 64] |= 1 << (i % 64);
        }
        let all_settled = (0..hdr.len()).all(|j| settled[j / 64] & (1 << (j % 64)) != 0);
        BlockImage {
            block: self.block,
            hdr,
            tuples: self.tuples.clone(),
            at: self.at.clone(),
            settled,
            all_settled,
            bytes: self.bytes,
            lsn: self.lsn,
            fp: self.fp.clone(),
            charge: pgrc_read::mem::Charge::new(self.bytes as usize),
        }
    }
}

/// `Some(true)` settle, `Some(false)` dead to all, `None` still recent; the resolved states land in `h.infomask`.
pub fn reclassify(h: &mut Hdr, tomb: bool, cx: &Ctx<'_>) -> PgResult<Option<bool>> {
    Ok(match classify(h, tomb, cx)? {
        Ok(St::Settled) => Some(true),
        Ok(St::Dead) => {
            h.infomask |= HEAP_XMIN_INVALID;
            h.infomask &= !HEAP_XMIN_COMMITTED;
            Some(false)
        }
        Ok(St::Recent) | Err(_) => None,
    })
}
