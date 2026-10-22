use std::cell::RefCell;
use std::collections::HashMap;

use pgrc_colcache::image::{verify, Images};
use pgrc_colcache::vis::SnapView;
use pgrc_colcache::{Kind, PageSource, TxnOracle};
use pgrc_marks::{Locator, Registry, WriterCache};
use types_core::{CommandId, TransactionId, BLCKSZ};
use types_error::{PgError, PgResult};
use types_storage::bufpage::{ItemIdData, PageMut, PAI_IS_HEAP};

const LOC: Locator = (1663, 5, 70001);
const XMIN_COMMITTED: u16 = 0x0100;
const XMIN_INVALID: u16 = 0x0200;
const XMAX_COMMITTED: u16 = 0x0400;
const XMAX_INVALID: u16 = 0x0800;
const FROZEN: u16 = XMIN_COMMITTED | XMIN_INVALID;
const WORDS: usize = BLCKSZ / 8;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum XSt {
    Running,
    Committed,
    Aborted,
}

#[derive(Clone, Debug)]
struct Snap {
    xmin: u32,
    xmax: u32,
    xip: Vec<u32>,
    own: Option<u32>,
    curcid: u32,
}

struct World {
    tomb: bool,
    pages: Vec<Box<[u64]>>,
    xids: HashMap<u32, XSt>,
    next_xid: u32,
    own: Option<u32>,
    own_cid: u32,
    held: Vec<Snap>,
    oldest_clog: u32,
    next_val: i64,
    added: usize,
}

fn new_page() -> Box<[u64]> {
    vec![0u64; WORDS].into_boxed_slice()
}

fn page_mut(p: &mut [u64]) -> PageMut<'_> {
    // SAFETY: an 8-aligned BLCKSZ buffer owned by the model.
    unsafe { PageMut::from_raw(std::ptr::NonNull::new(p.as_mut_ptr() as *mut u8).unwrap()) }
}

fn bytes(p: &[u64]) -> &[u8] {
    // SAFETY: BLCKSZ bytes of an 8-aligned buffer.
    unsafe { std::slice::from_raw_parts(p.as_ptr() as *const u8, BLCKSZ) }
}

fn bytes_mut(p: &mut [u64]) -> &mut [u8] {
    // SAFETY: as above, exclusively borrowed.
    unsafe { std::slice::from_raw_parts_mut(p.as_mut_ptr() as *mut u8, BLCKSZ) }
}

fn tuple(xmin: u32, cid: u32, val: i64) -> [u8; 32] {
    let mut t = [0u8; 32];
    t[0..4].copy_from_slice(&xmin.to_ne_bytes());
    t[8..12].copy_from_slice(&cid.to_ne_bytes());
    t[18..20].copy_from_slice(&1u16.to_ne_bytes());
    t[20..22].copy_from_slice(&XMAX_INVALID.to_ne_bytes());
    t[22] = 24;
    t[24..32].copy_from_slice(&val.to_ne_bytes());
    t
}

fn items(p: &[u64]) -> Vec<(u16, usize)> {
    let b = bytes(p);
    let lower = u16::from_ne_bytes([b[12], b[13]]) as usize;
    if u16::from_ne_bytes([b[14], b[15]]) == 0 {
        return Vec::new();
    }
    let n = lower.saturating_sub(24) / 4;
    let mut out = Vec::new();
    for i in 0..n {
        let raw = u32::from_ne_bytes([b[24 + 4 * i], b[25 + 4 * i], b[26 + 4 * i], b[27 + 4 * i]]);
        let (off, flags) = ((raw & 0x7fff) as usize, (raw >> 15) & 3);
        if flags == 1 {
            out.push(((i + 1) as u16, off));
        }
    }
    out
}

fn rd32(b: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}
fn rd16(b: &[u8], at: usize) -> u16 {
    u16::from_ne_bytes([b[at], b[at + 1]])
}

impl World {
    fn new(tomb: bool) -> World {
        World { tomb, pages: Vec::new(), xids: HashMap::new(), next_xid: 100, own: None, own_cid: 0, held: Vec::new(), oldest_clog: 3, next_val: 1, added: 0 }
    }

    fn begin(&mut self) -> u32 {
        let x = self.next_xid;
        self.next_xid += 1;
        self.xids.insert(x, XSt::Running);
        x
    }

    fn running(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self.xids.iter().filter(|(_, s)| **s == XSt::Running).map(|(x, _)| *x).collect();
        v.sort_unstable();
        v
    }

    fn snapshot(&self) -> Snap {
        let xip: Vec<u32> = self.running().into_iter().filter(|x| Some(*x) != self.own).collect();
        let xmax = self.next_xid;
        let xmin = xip.first().copied().unwrap_or(xmax).min(self.own.unwrap_or(u32::MAX));
        Snap { xmin, xmax, xip, own: self.own, curcid: self.own_cid }
    }

    fn horizon(&self) -> u32 {
        let mut h = self.next_xid;
        for x in self.running() {
            h = h.min(x);
        }
        for s in &self.held {
            h = h.min(s.xmin);
        }
        h
    }

    fn state(&self, xid: u32) -> XSt {
        *self.xids.get(&xid).unwrap_or(&XSt::Aborted)
    }

    fn has_room(&self, blk: usize) -> bool {
        let b = bytes(&self.pages[blk]);
        let (lower, upper) = (rd16(b, 12) as usize, rd16(b, 14) as usize);
        upper == 0 || upper >= lower + 4 + 32
    }

    fn add(&mut self, blk: usize, xmin: u32, cid: u32) -> u16 {
        let val = self.next_val;
        self.next_val += 1;
        self.added += 1;
        let p = &mut self.pages[blk];
        if rd16(bytes(p), 14) == 0 {
            page_mut(p).init(0);
        }
        let t = tuple(xmin, cid, val);
        let off = page_mut(p).add_item(&t, 0, PAI_IS_HEAP).expect("room");
        let at = items(p).into_iter().find(|(o, _)| *o == off).unwrap().1;
        let b = bytes_mut(p);
        b[at + 12..at + 14].copy_from_slice(&((blk as u32 >> 16) as u16).to_ne_bytes());
        b[at + 14..at + 16].copy_from_slice(&(blk as u16).to_ne_bytes());
        b[at + 16..at + 18].copy_from_slice(&off.to_ne_bytes());
        off
    }

    fn infomask(&self, blk: usize, at: usize) -> u16 {
        rd16(bytes(&self.pages[blk]), at + 20)
    }

    fn set_infomask(&mut self, blk: usize, at: usize, m: u16) {
        bytes_mut(&mut self.pages[blk])[at + 20..at + 22].copy_from_slice(&m.to_ne_bytes());
    }

    fn in_snap(s: &Snap, xid: u32) -> bool {
        if xid < s.xmin {
            return false;
        }
        if xid >= s.xmax {
            return true;
        }
        s.xip.contains(&xid)
    }

    fn naive(&self, s: &Snap) -> Vec<(u32, u16, i64)> {
        let mut out = Vec::new();
        for (blk, p) in self.pages.iter().enumerate() {
            let b = bytes(p);
            for (off, at) in items(p) {
                let (xmin, xmax, cid, im) = (rd32(b, at), rd32(b, at + 4), rd32(b, at + 8), rd16(b, at + 20));
                let val = i64::from_ne_bytes(b[at + 24..at + 32].try_into().unwrap());
                let frozen = im & FROZEN == FROZEN;
                let ins = if frozen {
                    true
                } else if xmin == 0 {
                    false
                } else if Some(xmin) == s.own {
                    cid < s.curcid
                } else if Self::in_snap(s, xmin) {
                    false
                } else {
                    self.state(xmin) == XSt::Committed
                };
                if !ins {
                    continue;
                }
                let del = if !self.tomb || xmax == 0 {
                    false
                } else if Some(xmax) == s.own {
                    cid < s.curcid
                } else if Self::in_snap(s, xmax) {
                    false
                } else {
                    self.state(xmax) == XSt::Committed
                };
                if !del {
                    out.push((blk as u32, off, val));
                }
            }
        }
        out
    }
}

struct Src<'w>(&'w RefCell<World>);

impl PageSource for Src<'_> {
    fn nblocks(&self) -> PgResult<u32> {
        Ok(self.0.borrow().pages.len() as u32)
    }
    fn read(&self, block: u32, f: &mut dyn FnMut(&[u8]) -> PgResult<()>) -> PgResult<()> {
        let w = self.0.borrow();
        f(bytes(&w.pages[block as usize]))
    }
}

struct Txn<'w>(&'w RefCell<World>);

impl TxnOracle for Txn<'_> {
    fn is_current(&self, xid: TransactionId) -> bool {
        self.0.borrow().own == Some(xid)
    }
    fn in_progress(&self, xid: TransactionId) -> PgResult<bool> {
        Ok(self.0.borrow().state(xid) == XSt::Running)
    }
    fn committed(&self, xid: TransactionId) -> PgResult<bool> {
        let w = self.0.borrow();
        if xid >= 3 && xid < w.oldest_clog {
            return Err(Box::new(PgError::error(format!("could not access status of transaction {xid} (clog truncated below {})", w.oldest_clog))));
        }
        Ok(w.state(xid) == XSt::Committed)
    }
    fn horizon(&self) -> PgResult<TransactionId> {
        Ok(self.0.borrow().horizon())
    }
    fn oldest_clog(&self) -> TransactionId {
        self.0.borrow().oldest_clog
    }
}

struct SV<'w> {
    w: &'w RefCell<World>,
    s: Snap,
}

impl SnapView for SV<'_> {
    fn is_current(&self, xid: TransactionId) -> bool {
        self.s.own == Some(xid) && self.w.borrow().own == Some(xid)
    }
    fn in_snapshot(&self, xid: TransactionId) -> PgResult<bool> {
        Ok(World::in_snap(&self.s, xid))
    }
    fn did_commit(&self, xid: TransactionId) -> PgResult<bool> {
        Txn(self.w).committed(xid)
    }
    fn curcid(&self) -> CommandId {
        self.s.curcid
    }
}

struct Harness {
    w: RefCell<World>,
    reg: Registry,
    imgs: Images,
    cache: WriterCache,
    rng: Rng,
    kind: Kind,
    skip_mark_at: Option<u64>,
    writes: u64,
    stats: [u64; 7],
    last_seq: u64,
}

impl Harness {
    fn new(seed: u64, tomb: bool, window: u32) -> Harness {
        Harness {
            w: RefCell::new(World::new(tomb)),
            reg: Registry::with_chunk_words(1),
            imgs: Images::with_window(window),
            cache: WriterCache::new(),
            rng: Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1),
            kind: if tomb { Kind::Tomb } else { Kind::Segment { slot: 0, part_no: 1 } },
            skip_mark_at: None,
            writes: 0,
            stats: [0; 7],
            last_seq: u64::MAX,
        }
    }

    fn mark(&mut self, blk: usize) {
        self.writes += 1;
        if self.skip_mark_at == Some(self.writes) {
            return;
        }
        self.reg.written(&mut self.cache, LOC, blk as u32);
    }

    fn target(&mut self) -> usize {
        let n = self.w.borrow().pages.len();
        for _ in 0..4 {
            if n == 0 {
                break;
            }
            let b = self.rng.below(n as u64) as usize;
            if self.w.borrow().has_room(b) {
                return b;
            }
        }
        self.w.borrow_mut().pages.push(new_page());
        n
    }

    fn writer(&mut self) -> Option<(u32, u32)> {
        let w = self.w.borrow();
        let mut cands: Vec<(u32, u32)> = w.running().into_iter().filter(|x| Some(*x) != w.own).map(|x| (x, 0)).collect();
        if let Some(o) = w.own {
            cands.push((o, w.own_cid));
        }
        drop(w);
        if cands.is_empty() {
            return None;
        }
        let i = self.rng.below(cands.len() as u64) as usize;
        Some(cands[i])
    }

    fn truncate(&mut self) {
        let mut w = self.w.borrow_mut();
        w.pages.clear();
        w.added = 0;
        drop(w);
        self.reg.truncated(LOC);
    }

    fn step(&mut self) {
        if self.w.borrow().added > 2500 {
            self.truncate();
        }
        let op = self.rng.below(100);
        match op {
            0..=7 => {
                if self.w.borrow().running().len() < 5 {
                    self.w.borrow_mut().begin();
                }
            }
            8..=11 => {
                if self.w.borrow().own.is_none() {
                    let x = self.w.borrow_mut().begin();
                    let mut w = self.w.borrow_mut();
                    w.own = Some(x);
                    w.own_cid = 0;
                }
            }
            12..=35 => {
                if let Some((x, cid)) = self.writer() {
                    let b = self.target();
                    self.w.borrow_mut().add(b, x, cid);
                    self.mark(b);
                }
            }
            36..=43 => {
                // multi insert across blocks: all page changes first, then one mark per block
                if let Some((x, cid)) = self.writer() {
                    let k = 2 + self.rng.below(12);
                    let mut blocks = Vec::new();
                    for _ in 0..k {
                        let b = self.target();
                        self.w.borrow_mut().add(b, x, cid);
                        if blocks.last() != Some(&b) {
                            blocks.push(b);
                        }
                    }
                    for b in blocks {
                        self.mark(b);
                    }
                }
            }
            44..=53 => {
                let r: Vec<u32> = { let w = self.w.borrow(); w.running().into_iter().filter(|x| Some(*x) != w.own).collect() };
                if !r.is_empty() {
                    let x = r[self.rng.below(r.len() as u64) as usize];
                    let st = if self.rng.chance(70) { XSt::Committed } else { XSt::Aborted };
                    self.w.borrow_mut().xids.insert(x, st);
                }
            }
            54..=57 => {
                let mut w = self.w.borrow_mut();
                if let Some(o) = w.own {
                    match self.rng.below(4) {
                        0 => {
                            w.xids.insert(o, XSt::Committed);
                            w.own = None;
                            w.held.retain(|s| s.own != Some(o));
                        }
                        1 => {
                            w.xids.insert(o, XSt::Aborted);
                            w.own = None;
                            w.held.retain(|s| s.own != Some(o));
                        }
                        _ => w.own_cid += 1,
                    }
                }
            }
            58..=62 => {
                let mut w = self.w.borrow_mut();
                if w.held.len() < 5 && self.rng.chance(60) {
                    let s = w.snapshot();
                    w.held.push(s);
                } else if !w.held.is_empty() {
                    let i = self.rng.below(w.held.len() as u64) as usize;
                    w.held.remove(i);
                }
            }
            63..=66 => self.hint_bits(),
            67..=69 => self.prune(),
            70..=71 => self.freeze(),
            72..=74 => {
                let k = 1 + self.rng.below(64) as usize;
                let mut w = self.w.borrow_mut();
                for _ in 0..k {
                    w.pages.push(new_page());
                }
            }
            75 => {
                if self.rng.chance(10) && self.w.borrow().pages.len() < 600 {
                    let mut w = self.w.borrow_mut();
                    for _ in 0..256 {
                        w.pages.push(new_page());
                    }
                }
            }
            76 => {
                if self.rng.chance(20) {
                    self.truncate();
                }
            }
            77..=84 if self.w.borrow().tomb => self.tomb_op(),
            _ => {
                if let Some((x, cid)) = self.writer() {
                    let b = self.target();
                    self.w.borrow_mut().add(b, x, cid);
                    self.mark(b);
                }
            }
        }
    }

    fn tomb_op(&mut self) {
        if self.rng.chance(50) {
            if let Some((x, cid)) = self.writer() {
                let b = self.target();
                let off = self.w.borrow_mut().add(b, x, cid);
                self.mark(b);
                let at = items(&self.w.borrow().pages[b]).into_iter().find(|(o, _)| *o == off).unwrap().1;
                bytes_mut(&mut self.w.borrow_mut().pages[b])[at..at + 4].copy_from_slice(&0u32.to_ne_bytes());
                self.mark(b);
            }
        } else {
            let horizon = self.w.borrow().horizon();
            let r: Vec<u32> = { let w = self.w.borrow(); w.running().into_iter().filter(|x| Some(*x) != w.own).collect() };
            let Some(&deleter) = r.first() else { return };
            let n = self.w.borrow().pages.len();
            if n == 0 {
                return;
            }
            let b = self.rng.below(n as u64) as usize;
            let cands: Vec<usize> = {
                let w = self.w.borrow();
                let p = bytes(&w.pages[b]);
                items(&w.pages[b])
                    .into_iter()
                    .filter(|&(_, at)| {
                        let (xmin, xmax, im) = (rd32(p, at), rd32(p, at + 4), rd16(p, at + 20));
                        xmax == 0 && xmin != 0 && (im & FROZEN == FROZEN || (w.state(xmin) == XSt::Committed && xmin < horizon))
                    })
                    .map(|(_, at)| at)
                    .collect()
            };
            if let Some(&at) = cands.first() {
                {
                    let mut w = self.w.borrow_mut();
                    let im = w.infomask(b, at) & !XMAX_INVALID;
                    w.set_infomask(b, at, im);
                    bytes_mut(&mut w.pages[b])[at + 4..at + 8].copy_from_slice(&deleter.to_ne_bytes());
                }
                self.mark(b);
            }
        }
    }

    /// Hint bits (no mark; a legitimate bypass): committed / aborted xmin, xmax.
    fn hint_bits(&mut self) {
        let mut w = self.w.borrow_mut();
        let n = w.pages.len();
        if n == 0 {
            return;
        }
        let b = self.rng.below(n as u64) as usize;
        for (_, at) in items(&w.pages[b]) {
            let p = bytes(&w.pages[b]);
            let (xmin, xmax, mut im) = (rd32(p, at), rd32(p, at + 4), rd16(p, at + 20));
            if xmin >= w.oldest_clog {
                match w.state(xmin) {
                    XSt::Committed => im |= XMIN_COMMITTED,
                    XSt::Aborted if xmin != 0 => im |= XMIN_INVALID,
                    _ => {}
                }
            }
            if xmax != 0 && xmax >= w.oldest_clog {
                match w.state(xmax) {
                    XSt::Committed => im |= XMAX_COMMITTED,
                    XSt::Aborted => im |= XMAX_INVALID,
                    _ => {}
                }
            }
            w.set_infomask(b, at, im);
        }
    }

    /// Prune (no mark; a legitimate bypass).
    fn prune(&mut self) {
        let horizon = self.w.borrow().horizon();
        let mut w = self.w.borrow_mut();
        let n = w.pages.len();
        if n == 0 {
            return;
        }
        let b = self.rng.below(n as u64) as usize;
        let mut dead = Vec::new();
        for (off, at) in items(&w.pages[b]) {
            let p = bytes(&w.pages[b]);
            let (xmin, xmax, im) = (rd32(p, at), rd32(p, at + 4), rd16(p, at + 20));
            let aborted_ins = im & FROZEN != FROZEN && (xmin == 0 || w.state(xmin) == XSt::Aborted);
            let deleted = w.tomb && xmax != 0 && w.state(xmax) == XSt::Committed && xmax < horizon;
            if aborted_ins || deleted {
                dead.push(off);
            }
        }
        if dead.is_empty() {
            return;
        }
        let mut pm = page_mut(&mut w.pages[b]);
        for off in dead {
            pm.set_item_id(off, ItemIdData::new(0, 0, 0));
        }
        pm.set_has_free_line_pointers();
    }

    fn freeze(&mut self) {
        self.prune();
        let horizon = self.w.borrow().horizon();
        let mut w = self.w.borrow_mut();
        let mut oldest = horizon;
        for b in 0..w.pages.len() {
            for (_, at) in items(&w.pages[b]) {
                let p = bytes(&w.pages[b]);
                let (xmin, xmax, im) = (rd32(p, at), rd32(p, at + 4), rd16(p, at + 20));
                if im & FROZEN != FROZEN && xmin != 0 && w.state(xmin) == XSt::Committed && xmin < horizon {
                    w.set_infomask(b, at, im | FROZEN);
                } else if im & FROZEN != FROZEN && xmin != 0 {
                    oldest = oldest.min(xmin);
                }
                if xmax != 0 {
                    oldest = oldest.min(xmax);
                }
            }
        }
        // Aborted rows not pruned yet keep their xids readable.
        for b in 0..w.pages.len() {
            for (_, at) in items(&w.pages[b]) {
                let p = bytes(&w.pages[b]);
                let (xmin, im) = (rd32(p, at), rd16(p, at + 20));
                if im & FROZEN != FROZEN && xmin != 0 {
                    oldest = oldest.min(xmin);
                }
            }
        }
        if oldest > w.oldest_clog {
            w.oldest_clog = oldest;
        }
    }

    fn check(&mut self) -> Result<(), String> {
        let src = Src(&self.w);
        let txn = Txn(&self.w);
        let img = match self.imgs.current(&self.reg, LOC, 16384, self.kind, &src, &txn) {
            Ok(Ok(img)) => img,
            Ok(Err(e)) => return Err(format!("no image: {e:?}")),
            Err(e) => return Err(format!("current() failed: {}", e.message())),
        };
        self.stats[0] += 1;
        if img.seq != self.last_seq {
            self.stats[2] += 1;
            self.stats[3] += img.redecoded as u64;
            if img.seq % 2 == 1 && self.last_seq != u64::MAX && img.seq == self.last_seq + 2 {
                self.stats[6] += 1;
            }
            self.last_seq = img.seq;
        }
        self.stats[4] = self.stats[4].max(img.recent.len() as u64);
        let bad = verify(&img, &src, &txn).map_err(|e| e.message().to_string())?;
        if !bad.is_empty() {
            return Err(format!("verify: blocks {bad:?} differ from the image"));
        }
        let mut snaps: Vec<Snap> = self.w.borrow().held.clone();
        snaps.push(self.w.borrow().snapshot());
        let k = 1 + self.rng.below(3);
        for _ in 0..k {
            let mut s = self.w.borrow().snapshot();
            if s.own.is_some() && self.rng.chance(50) {
                s.curcid = self.rng.below(s.curcid as u64 + 2) as u32;
            }
            snaps.push(s);
        }
        snaps.truncate(8);
        for s in snaps {
            let sv = SV { w: &self.w, s: s.clone() };
            let mut rows = Vec::new();
            img.visible(&sv, &mut rows).map_err(|e| format!("visible: {}", e.message()))?;
            let got: Vec<(u32, u16, i64)> = rows
                .iter()
                .map(|&(b, i)| {
                    let c = img.chunk(b).unwrap();
                    let t = c.tuple_bytes(i as usize);
                    (b, c.hdr[i as usize].off, i64::from_ne_bytes(t[24..32].try_into().unwrap()))
                })
                .collect();
            let want = self.w.borrow().naive(&s);
            self.stats[1] += want.len() as u64;
            if got != want {
                let gs: std::collections::BTreeSet<_> = got.iter().collect();
                let ws: std::collections::BTreeSet<_> = want.iter().collect();
                let extra: Vec<_> = gs.difference(&ws).take(5).collect();
                let missing: Vec<_> = ws.difference(&gs).take(5).collect();
                return Err(format!("snapshot {s:?}: image {} rows, naive {}; extra {extra:?} missing {missing:?}", got.len(), want.len()));
            }
        }
        Ok(())
    }
}

/// The model runs sweep the process ring as at cap 0 on one step in four; the seeded run excludes them (an eviction's full rebuild would hide its missing mark).
static SWEEPS: std::sync::RwLock<()> = std::sync::RwLock::new(());

fn run(seed: u64, steps: usize, tomb: bool, window: u32, skip_mark_at: Option<u64>) -> Result<(), String> {
    let mut h = Harness::new(seed, tomb, window);
    h.skip_mark_at = skip_mark_at;
    for i in 0..steps {
        h.step();
        if skip_mark_at.is_none() && i % 4 == 3 {
            pgrc_read::mem::sweep_all_for_tests();
        }
        h.check().map_err(|e| format!("seed {seed} step {i}: {e}"))?;
    }
    if skip_mark_at.is_none() {
        let s = h.stats;
        eprintln!("seed {seed} tomb={tomb} window={window}: checks {} rows-compared {} refreshes {} redecoded {} max-recent {} clog-oldest {}", s[0], s[1], s[2], s[3], s[4], h.w.borrow().oldest_clog);
    }
    Ok(())
}

fn steps() -> usize {
    // PGRC_COLCACHE_MODEL_STEPS overrides the gate's 10,000 (quick local runs).
    option_env!("PGRC_COLCACHE_MODEL_STEPS").and_then(|s| s.parse().ok()).unwrap_or(10_000)
}

#[test]
fn model_segments_20_seeds() {
    let _g = SWEEPS.read().unwrap_or_else(|e| e.into_inner());
    for seed in 1..=20u64 {
        run(seed, steps(), false, if seed % 2 == 0 { 4 } else { pgrc_marks::WINDOW }, None).unwrap();
    }
}

#[test]
fn model_tombstones_20_seeds() {
    let _g = SWEEPS.read().unwrap_or_else(|e| e.into_inner());
    for seed in 101..=120u64 {
        run(seed, steps(), true, if seed % 2 == 0 { 4 } else { pgrc_marks::WINDOW }, None).unwrap();
    }
}

#[test]
fn seeded_missing_mark_is_caught() {
    let _g = SWEEPS.write().unwrap_or_else(|e| e.into_inner());
    let mut caught = 0;
    for seed in 1..=10u64 {
        for at in [5u64, 17, 40] {
            if let Err(e) = run(seed, 400, seed % 2 == 0, 4, Some(at)) {
                eprintln!("caught: {}", &e[..e.len().min(160)]);
                caught += 1;
            }
        }
    }
    assert!(caught >= 20, "only {caught}/30 seeded missing marks were caught");
}
