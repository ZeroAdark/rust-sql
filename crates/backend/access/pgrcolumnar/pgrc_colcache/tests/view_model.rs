use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use pgrc_am::scan::DeletionFilter;
use pgrc_colcache::image::Images;
use pgrc_colcache::tomb::{settle, settled, Keyer};
use pgrc_colcache::view::{build, SegIn, TombIn};
use pgrc_colcache::vis::SnapView;
use pgrc_colcache::{Kind, PageSource, TxnOracle};
use pgrc_format::manifest::{SegState, SegmentRecord};
use pgrc_format::rowid::seg_local as segment_pos;

/// Every part and segment `p` homes one extent at `base(p)`; a row at position `pos` has ID `base(p) + pos`.
fn base(part_no: u32) -> u64 {
    291 * (1000 + 2000 * u64::from(part_no))
}

fn rowident(part_no: u32, pos: u32) -> u64 {
    base(part_no) + u64::from(pos)
}

fn split(id: u64) -> (u32, u32) {
    let p = ((id / 291 - 1000) / 2000) as u32;
    (p, (id - base(p)) as u32)
}
use pgrc_marks::{Locator, Registry, WriterCache};
use types_core::{CommandId, TransactionId, BLCKSZ};
use types_error::PgResult;
use types_storage::bufpage::{PageMut, PAI_IS_HEAP};

const XMAX_INVALID: u16 = 0x0800;
const WORDS: usize = BLCKSZ / 8;
const GRAIN: u32 = 8;

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

fn bytes(p: &[u64]) -> &[u8] {
    // SAFETY: BLCKSZ bytes of an 8-aligned buffer.
    unsafe { std::slice::from_raw_parts(p.as_ptr() as *const u8, BLCKSZ) }
}
fn bytes_mut(p: &mut [u64]) -> &mut [u8] {
    // SAFETY: as above, exclusively borrowed.
    unsafe { std::slice::from_raw_parts_mut(p.as_mut_ptr() as *mut u8, BLCKSZ) }
}
fn rd32(b: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}
fn rd16(b: &[u8], at: usize) -> u16 {
    u16::from_ne_bytes([b[at], b[at + 1]])
}

fn items(p: &[u64]) -> Vec<(u16, usize)> {
    let b = bytes(p);
    if rd16(b, 14) == 0 {
        return Vec::new();
    }
    let n = (rd16(b, 12) as usize).saturating_sub(24) / 4;
    (0..n)
        .filter_map(|i| {
            let raw = rd32(b, 24 + 4 * i);
            ((raw >> 15) & 3 == 1).then_some(((i + 1) as u16, (raw & 0x7fff) as usize))
        })
        .collect()
}

#[derive(Default)]
struct Heap {
    pages: Vec<Box<[u64]>>,
}

impl Heap {
    fn add(&mut self, rng: &mut Rng, xmin: u32, cid: u32, vals: &[i64]) -> (u32, u16) {
        let len = 24 + 8 * vals.len();
        let mut blk = None;
        for _ in 0..3 {
            if self.pages.is_empty() {
                break;
            }
            let b = rng.below(self.pages.len() as u64) as usize;
            let p = bytes(&self.pages[b]);
            let (lower, upper) = (rd16(p, 12) as usize, rd16(p, 14) as usize);
            if upper == 0 || upper >= lower + 4 + len {
                blk = Some(b);
                break;
            }
        }
        let b = blk.unwrap_or_else(|| {
            self.pages.push(vec![0u64; WORDS].into_boxed_slice());
            self.pages.len() - 1
        });
        let p = &mut self.pages[b];
        // SAFETY: an 8-aligned BLCKSZ buffer owned by the model.
        let mut pm = unsafe { PageMut::from_raw(std::ptr::NonNull::new(p.as_mut_ptr() as *mut u8).unwrap()) };
        if rd16(bytes(p), 14) == 0 {
            pm.init(0);
        }
        let mut t = vec![0u8; len];
        t[0..4].copy_from_slice(&xmin.to_ne_bytes());
        t[8..12].copy_from_slice(&cid.to_ne_bytes());
        t[18..20].copy_from_slice(&(vals.len() as u16).to_ne_bytes());
        t[20..22].copy_from_slice(&XMAX_INVALID.to_ne_bytes());
        t[22] = 24;
        for (k, v) in vals.iter().enumerate() {
            t[24 + 8 * k..32 + 8 * k].copy_from_slice(&v.to_ne_bytes());
        }
        let off = pm.add_item(&t, 0, PAI_IS_HEAP).expect("room");
        (b as u32, off)
    }

    fn at(&self, blk: u32, off: u16) -> usize {
        items(&self.pages[blk as usize]).into_iter().find(|(o, _)| *o == off).unwrap().1
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
    gen: usize,
}

#[derive(Clone)]
struct MPart {
    part_no: u32,
    rows: u64,
    map: Option<Vec<u32>>,
}

#[derive(Clone)]
struct MGen {
    gen: u64,
    segs: Vec<SegmentRecord>,
    parts: Vec<MPart>,
    segnos: Vec<u32>,
}

impl Keyer for MGen {
    fn gen(&self) -> u64 {
        self.gen
    }
    fn nparts(&self) -> usize {
        self.parts.len()
    }
    fn seg_of(&self, id: u64) -> Option<(u32, u32)> {
        let (p, pos) = split(id);
        self.segnos.contains(&p).then_some((p, pos))
    }
    fn live_segment(&self, part_no: u32) -> bool {
        self.segs.iter().any(|r| r.state != SegState::Sealed && r.part_no == part_no)
    }
    fn homes(&self, id: u64) -> bool {
        self.home_of_id(id).is_some()
    }
    fn locate(&mut self, rowident: u64) -> PgResult<Option<(usize, u32, u32)>> {
        let Some(i) = self.home_of_id(rowident) else { return Ok(None) };
        let pos = split(rowident).1;
        let p = &self.parts[i];
        let ord = match &p.map {
            None => Some(pos),
            Some(m) => m.binary_search(&pos).ok().map(|o| o as u32),
        };
        Ok(ord.filter(|&o| (o as u64) < p.rows).map(|o| (i, o / GRAIN, o % GRAIN)))
    }
}

impl MGen {
    fn home_of_id(&self, id: u64) -> Option<usize> {
        let part_no = split(id).0;
        self.parts.iter().position(|p| p.part_no == part_no)
    }
}

struct World {
    xids: HashMap<u32, XSt>,
    next_xid: u32,
    own: Option<u32>,
    own_cid: u32,
    held: Vec<Snap>,
    segs: HashMap<u32, Heap>,
    tombs: [Heap; 2],
    tomb_locs: [Locator; 2],
    cur: usize,
    gens: Vec<MGen>,
    next_part_no: u32,
    next_relid: u32,
    next_val: i64,
}

impl World {
    /// Every segment `part_no` any generation records (the model never prunes the extent table).
    fn segnos(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self.gens.iter().flat_map(|g| g.segs.iter().map(|r| r.part_no)).collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    fn state(&self, x: u32) -> XSt {
        *self.xids.get(&x).unwrap_or(&XSt::Aborted)
    }
    fn running(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self.xids.iter().filter(|(_, s)| **s == XSt::Running).map(|(x, _)| *x).collect();
        v.sort_unstable();
        v
    }
    fn begin(&mut self) -> u32 {
        let x = self.next_xid;
        self.next_xid += 1;
        self.xids.insert(x, XSt::Running);
        x
    }
    fn snapshot(&self) -> Snap {
        let xip: Vec<u32> = self.running().into_iter().filter(|x| Some(*x) != self.own).collect();
        let xmax = self.next_xid;
        let xmin = xip.first().copied().unwrap_or(xmax).min(self.own.unwrap_or(u32::MAX));
        Snap { xmin, xmax, xip, own: self.own, curcid: self.own_cid, gen: self.gens.len() - 1 }
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
    fn in_snap(s: &Snap, x: u32) -> bool {
        x >= s.xmin && (x >= s.xmax || s.xip.contains(&x))
    }
    fn visible(&self, s: &Snap, b: &[u8], at: usize, tomb: bool) -> bool {
        let (xmin, xmax, cid) = (rd32(b, at), rd32(b, at + 4), rd32(b, at + 8));
        let ins = if xmin == 0 {
            false
        } else if Some(xmin) == s.own {
            cid < s.curcid
        } else {
            !Self::in_snap(s, xmin) && self.state(xmin) == XSt::Committed
        };
        if !ins || !tomb || xmax == 0 {
            return ins;
        }
        let del = if Some(xmax) == s.own { cid < s.curcid } else { !Self::in_snap(s, xmax) && self.state(xmax) == XSt::Committed };
        !del
    }
    fn active(&self) -> SegmentRecord {
        *self.gens.last().unwrap().segs.iter().find(|r| r.state == SegState::Active).unwrap()
    }
    fn scan_segs(&self, s: &Snap) -> Vec<SegmentRecord> {
        let view = &self.gens[s.gen];
        let newest = self.gens.last().unwrap();
        let mut v: Vec<SegmentRecord> = view.segs.iter().filter(|r| r.state != SegState::Sealed).copied().collect();
        if s.gen != self.gens.len() - 1 {
            for r in newest.segs.iter().filter(|r| r.state != SegState::Sealed) {
                if !view.segs.iter().any(|v| v.state != SegState::Sealed && v.relid == r.relid && v.part_no == r.part_no) {
                    v.push(*r);
                }
            }
        }
        v
    }
}

struct Src<'w> {
    w: &'w RefCell<World>,
    tomb: Option<usize>,
    relid: u32,
}

impl PageSource for Src<'_> {
    fn nblocks(&self) -> PgResult<u32> {
        let w = self.w.borrow();
        Ok(match self.tomb {
            Some(t) => w.tombs[t].pages.len(),
            None => w.segs[&self.relid].pages.len(),
        } as u32)
    }
    fn read(&self, block: u32, f: &mut dyn FnMut(&[u8]) -> PgResult<()>) -> PgResult<()> {
        let w = self.w.borrow();
        let h = match self.tomb {
            Some(t) => &w.tombs[t],
            None => &w.segs[&self.relid],
        };
        f(bytes(&h.pages[block as usize]))
    }
}

struct Txn<'w>(&'w RefCell<World>);

impl TxnOracle for Txn<'_> {
    fn is_current(&self, x: TransactionId) -> bool {
        self.0.borrow().own == Some(x)
    }
    fn in_progress(&self, x: TransactionId) -> PgResult<bool> {
        Ok(self.0.borrow().state(x) == XSt::Running)
    }
    fn committed(&self, x: TransactionId) -> PgResult<bool> {
        Ok(self.0.borrow().state(x) == XSt::Committed)
    }
    fn horizon(&self) -> PgResult<TransactionId> {
        Ok(self.0.borrow().horizon())
    }
    fn oldest_clog(&self) -> TransactionId {
        3
    }
}

struct SV<'w> {
    w: &'w RefCell<World>,
    s: Snap,
}

impl SnapView for SV<'_> {
    fn is_current(&self, x: TransactionId) -> bool {
        self.s.own == Some(x) && self.w.borrow().own == Some(x)
    }
    fn in_snapshot(&self, x: TransactionId) -> PgResult<bool> {
        Ok(World::in_snap(&self.s, x))
    }
    fn did_commit(&self, x: TransactionId) -> PgResult<bool> {
        Ok(self.w.borrow().state(x) == XSt::Committed)
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
    stats: [u64; 5],
}

fn loc(relid: u32) -> Locator {
    (1663, 5, relid)
}

impl Harness {
    fn new(seed: u64) -> Harness {
        let seg0 = SegmentRecord { slot: 0, part_no: 1, relid: 90001, state: SegState::Active, fxid: 0 };
        let mut segs = HashMap::new();
        segs.insert(90001, Heap::default());
        Harness {
            w: RefCell::new(World {
                xids: HashMap::new(),
                next_xid: 100,
                own: None,
                own_cid: 0,
                held: Vec::new(),
                segs,
                tombs: [Heap::default(), Heap::default()],
                tomb_locs: [loc(80000), loc(80001)],
                cur: 0,
                gens: vec![MGen { gen: 1, segs: vec![seg0], parts: Vec::new(), segnos: Vec::new() }],
                next_part_no: 2,
                next_relid: 90002,
                next_val: 1,
            }),
            reg: Registry::with_chunk_words(1),
            imgs: Images::with_window(8),
            cache: WriterCache::new(),
            rng: Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1),
            stats: [0; 5],
        }
    }

    fn writer(&mut self) -> Option<(u32, u32)> {
        let w = self.w.borrow();
        let mut c: Vec<(u32, u32)> = w.running().into_iter().filter(|x| Some(*x) != w.own).map(|x| (x, 0)).collect();
        if let Some(o) = w.own {
            c.push((o, w.own_cid));
            c.push((o, w.own_cid));
        }
        drop(w);
        (!c.is_empty()).then(|| c[self.rng.below(c.len() as u64) as usize])
    }

    fn insert(&mut self) {
        let Some((x, cid)) = self.writer() else { return };
        let a = self.w.borrow().active();
        let mut w = self.w.borrow_mut();
        let val = w.next_val;
        w.next_val += 1;
        let (blk, _) = w.segs.get_mut(&a.relid).unwrap().add(&mut self.rng, x, cid, &[val]);
        drop(w);
        self.reg.written(&mut self.cache, loc(a.relid), blk);
    }

    fn delete(&mut self) {
        let Some((x, cid)) = self.writer() else { return };
        let target = {
            let w = self.w.borrow();
            let g = w.gens.last().unwrap();
            let live: Vec<SegmentRecord> = g.segs.iter().filter(|r| r.state != SegState::Sealed).copied().collect();
            if !g.parts.is_empty() && self.rng.chance(40) {
                let p = &g.parts[self.rng.below(g.parts.len() as u64) as usize];
                let pos = match &p.map {
                    Some(m) if !m.is_empty() && self.rng.chance(90) => m[self.rng.below(m.len() as u64) as usize],
                    _ => self.rng.below(p.rows + 3) as u32,
                };
                Some(rowident(p.part_no, pos))
            } else if !live.is_empty() {
                let r = live[self.rng.below(live.len() as u64) as usize];
                let h = &w.segs[&r.relid];
                let all: Vec<(u32, u16)> = (0..h.pages.len()).flat_map(|b| items(&h.pages[b]).into_iter().map(move |(o, _)| (b as u32, o))).collect();
                if all.is_empty() {
                    None
                } else {
                    let (b, o) = all[self.rng.below(all.len() as u64) as usize];
                    Some(rowident(r.part_no, segment_pos(b, o).unwrap()))
                }
            } else {
                None
            }
        };
        let Some(ri) = target else { return };
        let (t, l) = { let w = self.w.borrow(); (w.cur, w.tomb_locs[w.cur]) };
        let (blk, off) = self.w.borrow_mut().tombs[t].add(&mut self.rng, x, cid, &[ri as i64, 0]);
        self.reg.written(&mut self.cache, l, blk);
        if self.rng.chance(15) {
            let mut w = self.w.borrow_mut();
            let at = w.tombs[t].at(blk, off);
            bytes_mut(&mut w.tombs[t].pages[blk as usize])[at..at + 4].copy_from_slice(&0u32.to_ne_bytes());
            drop(w);
            self.reg.written(&mut self.cache, l, blk);
        }
    }

    fn seal(&mut self) {
        let mut w = self.w.borrow_mut();
        let mut g = w.gens.last().unwrap().clone();
        g.gen += 1;
        if let Some(i) = g.segs.iter().position(|r| r.state == SegState::Sealing) {
            let r = g.segs[i];
            let h = &w.segs[&r.relid];
            let mut pos: Vec<u32> = Vec::new();
            for b in 0..h.pages.len() {
                let p = bytes(&h.pages[b]);
                for (o, at) in items(&h.pages[b]) {
                    let xmin = rd32(p, at);
                    if xmin != 0 && w.state(xmin) == XSt::Committed {
                        pos.push(segment_pos(b as u32, o).unwrap());
                    }
                }
            }
            pos.sort_unstable();
            g.segs[i].state = SegState::Sealed;
            g.parts.push(MPart { part_no: r.part_no, rows: pos.len() as u64, map: Some(pos) });
        } else {
            let slot = (0..8u16).find(|s| !g.segs.iter().any(|r| r.slot == *s && r.state != SegState::Sealed)).unwrap();
            let a = g.segs.iter().position(|r| r.state == SegState::Active).unwrap();
            g.segs[a].state = SegState::Sealing;
            g.segs.retain(|r| !(r.slot == slot && r.state == SegState::Sealed));
            let (part_no, relid) = (w.next_part_no, w.next_relid);
            w.next_part_no += 1;
            w.next_relid += 1;
            w.segs.insert(relid, Heap::default());
            g.segs.push(SegmentRecord { slot, part_no, relid, state: SegState::Active, fxid: 0 });
        }
        w.gens.push(g);
    }

    fn bulk(&mut self) {
        let mut w = self.w.borrow_mut();
        let mut g = w.gens.last().unwrap().clone();
        g.gen += 1;
        let part_no = w.next_part_no;
        w.next_part_no += 1;
        g.parts.push(MPart { part_no, rows: 1 + self.rng.below(40), map: None });
        w.gens.push(g);
    }

    fn compile(&mut self) {
        let horizon = self.w.borrow().horizon();
        let deleter = { let w = self.w.borrow(); w.running().into_iter().find(|x| Some(*x) != w.own) };
        let Some(d) = deleter else { return };
        let mut blocks = Vec::new();
        {
            let mut w = self.w.borrow_mut();
            for t in 0..2 {
                for b in 0..w.tombs[t].pages.len() {
                    for (_, at) in items(&w.tombs[t].pages[b]) {
                        let (xmin, xmax) = { let p = bytes(&w.tombs[t].pages[b]); (rd32(p, at), rd32(p, at + 4)) };
                        if xmax == 0 && xmin != 0 && w.state(xmin) == XSt::Committed && xmin < horizon && self.rng.chance(30) {
                            let p = bytes_mut(&mut w.tombs[t].pages[b]);
                            p[at + 4..at + 8].copy_from_slice(&d.to_ne_bytes());
                            let im = rd16(p, at + 20) & !XMAX_INVALID;
                            p[at + 20..at + 22].copy_from_slice(&im.to_ne_bytes());
                            let l = w.tomb_locs[t];
                            if blocks.last() != Some(&(l, b)) {
                                blocks.push((l, b));
                            }
                        }
                    }
                }
            }
        }
        for (l, b) in blocks {
            self.reg.written(&mut self.cache, l, b as u32);
        }
    }

    fn step(&mut self) {
        match self.rng.below(100) {
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
            12..=36 => self.insert(),
            37..=56 => self.delete(),
            57..=66 => {
                let r: Vec<u32> = { let w = self.w.borrow(); w.running().into_iter().filter(|x| Some(*x) != w.own).collect() };
                if !r.is_empty() {
                    let x = r[self.rng.below(r.len() as u64) as usize];
                    let st = if self.rng.chance(75) { XSt::Committed } else { XSt::Aborted };
                    self.w.borrow_mut().xids.insert(x, st);
                }
            }
            67..=71 => {
                let mut w = self.w.borrow_mut();
                if let Some(o) = w.own {
                    match self.rng.below(4) {
                        0 | 1 => {
                            let st = if self.rng.chance(70) { XSt::Committed } else { XSt::Aborted };
                            w.xids.insert(o, st);
                            w.own = None;
                            w.held.retain(|s| s.own != Some(o));
                        }
                        _ => w.own_cid += 1,
                    }
                }
            }
            72..=77 => {
                let mut w = self.w.borrow_mut();
                if w.held.len() < 4 && self.rng.chance(60) {
                    let s = w.snapshot();
                    w.held.push(s);
                } else if !w.held.is_empty() {
                    let i = self.rng.below(w.held.len() as u64) as usize;
                    w.held.remove(i);
                }
            }
            78..=81 => self.seal(),
            82..=83 => self.bulk(),
            84..=87 => self.compile(),
            88..=89 => {
                let mut w = self.w.borrow_mut();
                w.cur ^= 1;
                if self.rng.chance(50) {
                    let (c, r) = (w.cur, w.next_relid);
                    w.next_relid += 1;
                    w.tombs[c] = Heap::default();
                    w.tomb_locs[c] = loc(r);
                }
            }
            _ => self.insert(),
        }
    }

    fn check(&mut self) -> Result<(), String> {
        let txn = Txn(&self.w);
        let mut tomb_imgs = Vec::new();
        for t in 0..2 {
            if self.w.borrow().tombs[t].pages.is_empty() {
                continue;
            }
            let src = Src { w: &self.w, tomb: Some(t), relid: 0 };
            let l = self.w.borrow().tomb_locs[t];
            match self.imgs.current(&self.reg, l, l.2, Kind::Tomb, &src, &txn).map_err(|e| e.message().to_string())? {
                Ok(i) => tomb_imgs.push(i),
                Err(e) => return Err(format!("tomb image {t}: {e:?}")),
            }
        }
        if tomb_imgs.len() == 2 {
            self.stats[4] += 1;
        }
        let mut snaps: Vec<Snap> = self.w.borrow().held.clone();
        for _ in 0..2 {
            let mut s = self.w.borrow().snapshot();
            if s.own.is_some() && self.rng.chance(50) {
                s.curcid = self.rng.below(s.curcid as u64 + 2) as u32;
            }
            snaps.push(s);
        }
        for s in snaps {
            let mut gen = self.w.borrow().gens[s.gen].clone();
            gen.segnos = self.w.borrow().segnos();
            let scan = self.w.borrow().scan_segs(&s);
            let mut ins = Vec::new();
            for r in &scan {
                let src = Src { w: &self.w, tomb: None, relid: r.relid };
                let img = self
                    .imgs
                    .current(&self.reg, loc(r.relid), r.relid, Kind::Segment { slot: r.slot, part_no: r.part_no }, &src, &txn)
                    .map_err(|e| e.message().to_string())?
                    .map_err(|e| format!("seg image {e:?}"))?;
                ins.push(SegIn { rec: *r, img: Some(img) });
            }
            // The shared keying (cached per generation and settled version) must equal a fresh one.
            let mut tombs = Vec::new();
            for img in &tomb_imgs {
                let st = settled(img, &mut gen).map_err(|e| e.message().to_string())?;
                let fresh = settle(img, &mut gen).map_err(|e| e.message().to_string())?;
                if st.count != fresh.count || st.segs != fresh.segs {
                    return Err(format!("settled cache stale: {} vs {} tombstones", st.count, fresh.count));
                }
                tombs.push(TombIn { img, settled: st });
            }
            if scan.len() > gen.segs.iter().filter(|r| r.state != SegState::Sealed).count() {
                self.stats[3] += 1 << 32;
            }
            let sv = SV { w: &self.w, s: s.clone() };
            let (_, views, filter) = build(&mut gen, &tombs, ins, &sv).map_err(|e| e.message().to_string())?;

            let w = self.w.borrow();
            let live: HashMap<u32, u16> = scan.iter().map(|r| (r.part_no, r.slot)).collect();
            let mut dseg: BTreeMap<u16, BTreeSet<u32>> = BTreeMap::new();
            let mut dpart: BTreeMap<(usize, u32), BTreeSet<u32>> = BTreeMap::new();
            let mut kg = w.gens[s.gen].clone();
            kg.segnos = w.segnos();
            for page in w.tombs.iter().flat_map(|h| h.pages.iter()) {
                let p = bytes(page);
                for (_, at) in items(page) {
                    if !w.visible(&s, p, at, true) {
                        continue;
                    }
                    let ri = i64::from_ne_bytes(p[at + 24..at + 32].try_into().unwrap()) as u64;
                    let (part_no, pos) = split(ri);
                    if let Some(&slot) = live.get(&part_no) {
                        dseg.entry(slot).or_default().insert(pos);
                    } else if kg.homes(ri) {
                        if let Some((i, g, r)) = kg.locate(ri).unwrap() {
                            dpart.entry((i, g)).or_default().insert(r);
                        }
                    }
                }
            }
            for v in &views {
                let h = &w.segs[&v.relid];
                let mut want = BTreeSet::new();
                for b in 0..h.pages.len() {
                    let p = bytes(&h.pages[b]);
                    for (o, at) in items(&h.pages[b]) {
                        let pos = segment_pos(b as u32, o).unwrap();
                        if w.visible(&s, p, at, false) && !dseg.get(&v.slot).is_some_and(|d| d.contains(&pos)) {
                            want.insert((pos, i64::from_ne_bytes(p[at + 24..at + 32].try_into().unwrap())));
                        }
                    }
                }
                let pgrc_colcache::SegSource::Image { img, .. } = &v.src else { return Err("heap source".into()) };
                let got: BTreeSet<(u32, i64)> = v
                    .rows()
                    .unwrap()
                    .into_iter()
                    .map(|(b, i)| {
                        let c = img.chunk(b).unwrap();
                        let t = c.tuple_bytes(i as usize);
                        (segment_pos(b, c.hdr[i as usize].off).unwrap(), i64::from_ne_bytes(t[24..32].try_into().unwrap()))
                    })
                    .collect();
                if got != want {
                    return Err(format!(
                        "snapshot {s:?} slot {} part_no {}: view {} rows, naive {}; extra {:?} missing {:?}",
                        v.slot,
                        v.part_no,
                        got.len(),
                        want.len(),
                        got.difference(&want).take(4).collect::<Vec<_>>(),
                        want.difference(&got).take(4).collect::<Vec<_>>()
                    ));
                }
                let fpos: BTreeSet<u32> = filter.segment_positions(v.slot).iter().copied().collect();
                let npos = dseg.get(&v.slot).cloned().unwrap_or_default();
                if fpos != npos {
                    return Err(format!("snapshot {s:?} slot {}: filter positions {fpos:?} != naive {npos:?}", v.slot));
                }
                self.stats[0] += want.len() as u64;
                self.stats[1] += npos.len() as u64;
            }
            for (i, p) in w.gens[s.gen].parts.iter().enumerate() {
                for g in 0..=(p.rows as u32 / GRAIN) {
                    let mut mask = None;
                    filter.granule_into(i, g, &mut mask);
                    let got: BTreeSet<u32> = mask.map_or_else(BTreeSet::new, |m| (0..GRAIN).filter(|&r| m[r as usize / 64] >> (r % 64) & 1 == 1).collect());
                    let want = dpart.get(&(i, g)).cloned().unwrap_or_default();
                    if got != want {
                        return Err(format!("snapshot {s:?} part {i} granule {g}: filter {got:?} != naive {want:?}"));
                    }
                    self.stats[2] += want.len() as u64;
                }
            }
            self.stats[3] += 1;
        }
        Ok(())
    }
}

fn steps() -> usize {
    option_env!("PGRC_COLCACHE_MODEL_STEPS").and_then(|s| s.parse().ok()).unwrap_or(5_000)
}

#[test]
fn view_model_10_seeds() {
    for seed in 1..=10u64 {
        let mut h = Harness::new(seed);
        for i in 0..steps() {
            h.step();
            // The ring swept as at cap 0 on one step in four: images evicted between steps and rebuilt.
            if i % 4 == 3 {
                pgrc_read::mem::sweep_all_for_tests();
            }
            if let Err(e) = h.check() {
                panic!("seed {seed} step {i}: {e}");
            }
        }
        let g = h.w.borrow().gens.len();
        eprintln!(
            "view seed {seed}: views {} (with newer segments {}, over two tombstone heaps {}) seg-rows {} seg-deletes {} part-deletes {} gens {g}",
            h.stats[3] & 0xffff_ffff,
            h.stats[3] >> 32,
            h.stats[4],
            h.stats[0],
            h.stats[1],
            h.stats[2]
        );
        assert!(h.stats[4] > 0, "seed {seed}: no check saw two tombstone heaps with rows");
    }
}
