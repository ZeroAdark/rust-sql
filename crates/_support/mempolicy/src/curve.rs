//! Each cache's cost curve (cache plan C2; eviction design section 11): the query time a cache would lose, in ns a
//! second, if it held s bytes instead of its size S. A [`Tracker`] per cache is fed the door's sampled references
//! (SHARDS, fixed size, byte-weighted as in FLOWS) by one background drain; it keeps reuse distances in bytes, a price
//! per reference, 1-min histograms, an EWMA for the ranking and a 2 h max for floors, corrects the LRU model with
//! S3-FIFO mini-simulations, and reads out the lower convex hull H, the next batch's slope v and the knee h.
//! Pure and single-threaded: no clock, no global, every array sized at construction.

use std::collections::TryReserveError;

/// Sampled keys a tracker holds (fixed-size SHARDS): the door's threshold falls as the sample fills.
pub const SAMPLES: usize = 2048;
pub const EPOCH_NS: u64 = 60_000_000_000;
/// The ranking curve's EWMA half-life, in epochs (design: 10-15 min).
pub const HALF_LIFE_EPOCHS: f64 = 12.0;
/// The floor curve is the per-bin max over `BLOCKS - 1` full blocks of `BLOCK_EPOCHS` plus the open one (2 h).
pub const BLOCK_EPOCHS: u32 = 15;
pub const BLOCKS: usize = 9;
/// The batch a reclaim takes (design 11.9) and the knee's share of the full loss (11.10; a user decision).
pub const BATCH: u64 = 32 << 20;
pub const EPS: f64 = 0.05;
/// The mini-simulations' sizes as shares of S; the last is the baseline the others are measured against. At size 0
/// every reference misses, so kappa(0) needs no simulation.
pub const SIM_FRACS: [f64; 4] = [0.25, 0.5, 0.75, 1.0];
const KAPPA_AT: [f64; 4] = [0.0, 0.25, 0.5, 0.75];
const KAPPA_MIN: f64 = 0.125;
const KAPPA_MAX: f64 = 8.0;
/// Distance bins: [0, 1 KiB), then four linear quarters of each octave from 2^10 to 2^47 bytes.
const LO_LOG: u32 = 10;
const HI_LOG: u32 = 46;
pub const BINS: usize = 1 + 4 * (HI_LOG - LO_LOG + 1) as usize;
pub const POINTS: usize = BINS + 2;
/// The door threshold that passes every key: a lookup is sampled iff `hash24 / size < door / 2^40`.
pub const DOOR_ALL: u64 = 1 << 40;
const NIL: u32 = u32::MAX;

/// The doors' sampling hash and test (memledger::value: the doors sample, the tracker agrees with them).
pub use memledger::value::{door_pass, hash24, UNIFORM};

fn filled<T: Clone>(n: usize, x: T) -> Result<Vec<T>, TryReserveError> {
    let mut v = Vec::new();
    v.try_reserve_exact(n)?;
    v.resize(n, x);
    Ok(v)
}

fn bin(d: u64) -> usize {
    if d < 1 << LO_LOG {
        return 0;
    }
    let e = 63 - d.leading_zeros();
    if e > HI_LOG {
        return BINS - 1;
    }
    1 + 4 * (e - LO_LOG) as usize + ((d >> (e - 2)) & 3) as usize
}

fn edges(b: usize) -> (u64, u64) {
    if b == 0 {
        return (0, 1 << LO_LOG);
    }
    let e = LO_LOG + ((b - 1) / 4) as u32;
    let j = ((b - 1) % 4) as u64;
    ((4 + j) << (e - 2), (5 + j) << (e - 2))
}

/// Distance bin `b`'s bytes, [lo, hi) (the histograms behind a curve, `Curve::open_hist`).
pub fn bin_edges(b: usize) -> (u64, u64) {
    edges(b)
}

/// Loss a second from references whose distance is above `s` (uniform within a bin).
fn above(h: &[f64; BINS], s: u64) -> f64 {
    let mut g = 0.0;
    for (b, &c) in h.iter().enumerate() {
        let (lo, hi) = edges(b);
        if s <= lo {
            g += c;
        } else if s < hi {
            g += c * (hi - s) as f64 / (hi - lo) as f64;
        }
    }
    g
}

/// The lower convex hull of points sorted by size (Andrew's monotone chain).
fn hull(pts: &[(u64, f64)], out: &mut [(u64, f64); POINTS]) -> usize {
    let mut n = 0;
    for &p in pts {
        while n >= 2 {
            let (o, a) = (out[n - 2], out[n - 1]);
            let cross = (a.0 as f64 - o.0 as f64) * (p.1 - o.1) - (a.1 - o.1) * (p.0 as f64 - o.0 as f64);
            if cross > 0.0 {
                break;
            }
            n -= 1;
        }
        out[n] = p;
        n += 1;
    }
    n
}

fn eval(h: &[(u64, f64)], s: u64) -> f64 {
    match h.iter().position(|p| p.0 >= s) {
        None => h.last().map_or(0.0, |p| p.1),
        Some(0) => h[0].1,
        Some(i) => {
            let (a, b) = (h[i - 1], h[i]);
            a.1 + (b.1 - a.1) * (s - a.0) as f64 / (b.0 - a.0) as f64
        }
    }
}

/// The smallest size where the hull is at most `EPS` of its value at 0.
fn knee(h: &[(u64, f64)]) -> u64 {
    let l0 = h.first().map_or(0.0, |p| p.1);
    if l0 <= 0.0 {
        return 0;
    }
    let target = EPS * l0;
    for w in h.windows(2) {
        let (a, b) = (w[0], w[1]);
        if b.1 <= target {
            let f = if a.1 > b.1 { (a.1 - target) / (a.1 - b.1) } else { 0.0 };
            return a.0 + (f * (b.0 - a.0) as f64) as u64;
        }
    }
    h.last().map_or(0, |p| p.0)
}

/// One cache's curve: the hull's points (bytes, ns/s lost at that size vs `size`), the next batch's price `v` (ns/s
/// per byte), the knee (ranking and 2 h floor curves), and the sample's state.
#[derive(Clone, Copy, Debug)]
pub struct Curve {
    pub size: u64,
    pub n: usize,
    pub hull: [(u64, f64); POINTS],
    pub v: f64,
    pub knee: u64,
    pub floor_knee: u64,
    pub loss0: f64,
    pub samples: u32,
    pub refs: u64,
    pub rate: f64,
    pub door: u64,
    /// kappa at 0, 0.25, 0.5 and 0.75 x size (1 = the LRU model stands).
    pub kappa: [f64; 4],
    /// The raw model (before the hull, the policy correction applied) at [`PREDICT_AT`] x size, ns/s: the predictions.
    pub raw: [f64; 3],
    /// The open epoch so far: the seconds it covers and the LRU model's losses (totals, not rates) at [`PREDICT_AT`] x
    /// size. With the switch at 2 (count mode) they are misses: a validation reads them across a window.
    pub open_secs: f64,
    pub open: [f64; 3],
    /// The open epoch's losses by distance bin ([`bin_edges`]): the LRU model's loss at any size `s` is the part above
    /// `s` less the part above the size (a validation reads it at the sizes a cache really held, A6).
    pub open_hist: [f64; BINS],
}

/// The fractions of the size the predictions are read at (`Curve::raw`, `Curve::open`).
pub const PREDICT_AT: [f64; 3] = [0.5, 0.25, 0.1];

pub const NO_CURVE: Curve = Curve {
    size: 0,
    n: 0,
    hull: [(0, 0.0); POINTS],
    v: 0.0,
    knee: 0,
    floor_knee: 0,
    loss0: 0.0,
    samples: 0,
    refs: 0,
    rate: 0.0,
    door: DOOR_ALL,
    kappa: [1.0; 4],
    raw: [0.0; 3],
    open_secs: 0.0,
    open: [0.0; 3],
    open_hist: [0.0; BINS],
};

impl Curve {
    pub fn points(&self) -> &[(u64, f64)] {
        &self.hull[..self.n]
    }

    /// H at `s` bytes.
    pub fn at(&self, s: u64) -> f64 {
        eval(self.points(), s)
    }

    /// A curve from raw (size, loss) points ending at `size`: hull, slope of the batch below `size`, knee.
    fn finish(mut self, pts: &[(u64, f64)], floor: Option<&[(u64, f64)]>) -> Curve {
        self.n = hull(pts, &mut self.hull);
        let h = &self.hull[..self.n];
        self.loss0 = h.first().map_or(0.0, |p| p.1);
        let d = BATCH.min(self.size);
        self.v = if d == 0 { 0.0 } else { (eval(h, self.size - d) - eval(h, self.size)) / d as f64 };
        self.knee = knee(h);
        self.floor_knee = match floor {
            Some(f) => {
                let mut fh = [(0, 0.0); POINTS];
                let n = hull(f, &mut fh);
                knee(&fh[..n])
            }
            None => self.knee,
        };
        self
    }
}

#[derive(Clone, Copy)]
struct Slot {
    key: u64,
    size: u64,
    /// The size the door sampled the key at: `size`, or [`UNIFORM`] for a door that knows no size at a lookup.
    dsize: u64,
    /// Bytes this key stands for in a distance: size / r = max(size, 2^40 / door).
    wb: u64,
    /// `(hash24 << 16) / size`: sampled iff below the door; the max goes first when the sample is full.
    pri: u64,
    price: u64,
    coupled: u64,
    coupled_acc: u64,
    last: u32,
    hpos: u32,
    /// Its last reference's weight while its outcome is pending (0: none), and its distance ([`COLD_D`]: cold).
    pend_w: f64,
    pend_d: u64,
    /// A fill of it was seen: its next fill is a re-fill (a miss under pressure), which prices it.
    filled: bool,
    /// A6: the most one use of it read back (0: none seen), what a miss of it costs in bytes ([`Tracker::reload`]); and the
    /// bytes read back in its current use.
    reload: u64,
    reload_acc: u64,
}

const NO_SLOT: Slot = Slot {
    key: 0,
    size: 0,
    dsize: 0,
    wb: 0,
    pri: 0,
    price: 0,
    coupled: 0,
    coupled_acc: 0,
    last: 0,
    hpos: 0,
    pend_w: 0.0,
    pend_d: 0,
    filled: false,
    reload: 0,
    reload_acc: 0,
};

/// A pending reference with no distance: the key's first in the sample.
const COLD_D: u64 = u64::MAX;

const SMALL: u8 = 0;
const MAIN: u8 = 1;
const GHOST: u8 = 2;
const OUT: u8 = 3;
/// W-TinyLFU's segments in the same three queues (each an LRU list: a hit moves its entry to the head).
const T_WIN: u8 = 0;
const T_PROB: u8 = 1;
const T_PROT: u8 = 2;
/// W-TinyLFU's eviction order at the budget (the condition cache's `EVICT_ORDER`).
const T_EVICT_ORDER: [u8; 3] = [T_PROB, T_WIN, T_PROT];

#[derive(Clone, Copy)]
struct Node {
    prev: u32,
    next: u32,
    b: u64,
    st: u8,
    freq: u8,
}

const NO_NODE: Node = Node { prev: NIL, next: NIL, b: 0, st: OUT, freq: 0 };

#[derive(Clone, Copy)]
struct Fifo {
    head: u32,
    tail: u32,
    bytes: u64,
}

/// An S3-FIFO cache (small 10 %, main, ghost), or a W-TinyLFU one (window, probation, protected), over the tracker's
/// slots, in weighted bytes: the policy correction.
struct Sim {
    nodes: Vec<Node>,
    q: [Fifo; 3],
    cap: u64,
    loss: f64,
    rank: f64,
}

impl Sim {
    fn new(n: usize) -> Result<Sim, TryReserveError> {
        let q = [Fifo { head: NIL, tail: NIL, bytes: 0 }; 3];
        Ok(Sim { nodes: filled(n, NO_NODE)?, q, cap: 0, loss: 0.0, rank: 0.0 })
    }

    fn unlink(&mut self, i: u32) {
        let n = self.nodes[i as usize];
        if n.st == OUT {
            return;
        }
        let f = &mut self.q[n.st as usize];
        if n.prev == NIL { f.head = n.next } else { self.nodes[n.prev as usize].next = n.next }
        if n.next == NIL { f.tail = n.prev } else { self.nodes[n.next as usize].prev = n.prev }
        f.bytes -= n.b;
        self.nodes[i as usize] = NO_NODE;
    }

    fn push(&mut self, i: u32, st: u8, b: u64, freq: u8) {
        let f = &mut self.q[st as usize];
        self.nodes[i as usize] = Node { prev: NIL, next: f.head, b, st, freq };
        if f.head == NIL { f.tail = i } else { self.nodes[f.head as usize].prev = i }
        f.head = i;
        f.bytes += b;
    }

    fn reference(&mut self, i: u32, b: u64, cost: f64) {
        let n = &mut self.nodes[i as usize];
        if n.st == SMALL || n.st == MAIN {
            n.freq = (n.freq + 1).min(3);
            return;
        }
        self.loss += cost;
        let st = n.st;
        self.unlink(i);
        if b > self.cap {
            return;
        }
        self.push(i, if st == GHOST { MAIN } else { SMALL }, b, 0);
        self.evict();
    }

    fn evict(&mut self) {
        while self.q[0].bytes + self.q[1].bytes > self.cap {
            if self.q[0].bytes > self.cap / 10 || self.q[1].head == NIL {
                let t = self.q[0].tail;
                let n = self.nodes[t as usize];
                self.unlink(t);
                self.push(t, if n.freq > 0 { MAIN } else { GHOST }, n.b, 0);
            } else {
                let t = self.q[1].tail;
                let n = self.nodes[t as usize];
                self.unlink(t);
                if n.freq > 0 {
                    self.push(t, MAIN, n.b, n.freq - 1);
                }
            }
        }
        while self.q[2].bytes > self.cap - self.cap / 10 {
            let t = self.q[2].tail;
            self.unlink(t);
        }
    }

    fn resize(&mut self, cap: u64, policy: Policy) {
        self.cap = cap;
        match policy {
            Policy::TinyLfu => self.tlfu_trim(),
            _ => self.evict(),
        }
    }

    fn held(&self) -> u64 {
        self.q[0].bytes + self.q[1].bytes + self.q[2].bytes
    }

    /// W-TinyLFU: a sampled reference to slot `i` (`b` weighted bytes, `cost` its miss's price), its count already up in
    /// `freq`. A hit moves its entry to its segment's head, a probation one into protected (protected past its share hands
    /// its least recent back to probation); a miss enters the window, then the window's overflow is admitted or not
    /// (`tlfu_admit`).
    fn tlfu_reference(&mut self, i: u32, b: u64, cost: f64, freq: &[u8]) {
        let n = self.nodes[i as usize];
        match n.st {
            T_WIN | T_PROT => {
                self.unlink(i);
                self.push(i, n.st, n.b, 0);
                return;
            }
            T_PROB => {
                self.unlink(i);
                self.push(i, T_PROT, n.b, 0);
                let share = crate::cache::tinylfu_protected(self.cap);
                while self.q[T_PROT as usize].bytes > share {
                    let t = self.q[T_PROT as usize].tail;
                    if t == i {
                        break;
                    }
                    let tb = self.nodes[t as usize].b;
                    self.unlink(t);
                    self.push(t, T_PROB, tb, 0);
                }
                return;
            }
            _ => {}
        }
        self.loss += cost;
        if b > self.cap {
            return;
        }
        self.push(i, T_WIN, b, 0);
        self.tlfu_admit(freq);
    }

    /// W-TinyLFU (the condition cache's `admit`): the window past its share hands its least recent entries to main, into
    /// probation while the cache is within its size, else only when counted more often than the main entries it would push
    /// out (probation's least recent first, then protected's), summed, which then go; a refused one goes itself. Then the
    /// cache back under its size.
    fn tlfu_admit(&mut self, freq: &[u8]) {
        let window = crate::cache::tinylfu_window(self.cap);
        while self.q[T_WIN as usize].bytes > window {
            let cand = self.q[T_WIN as usize].tail;
            let cb = self.nodes[cand as usize].b;
            let over = self.held().saturating_sub(self.cap);
            if over == 0 {
                self.unlink(cand);
                self.push(cand, T_PROB, cb, 0);
                continue;
            }
            let (mut bytes, mut count, mut n) = (0u64, 0u32, 0usize);
            let (mut seg, mut v) = (T_PROB, self.q[T_PROB as usize].tail);
            while bytes < over {
                if v == NIL {
                    if seg == T_PROB {
                        (seg, v) = (T_PROT, self.q[T_PROT as usize].tail);
                        continue;
                    }
                    break;
                }
                bytes += self.nodes[v as usize].b;
                count = count.saturating_add(u32::from(freq[v as usize]));
                n += 1;
                v = self.nodes[v as usize].prev;
            }
            if bytes >= over && crate::cache::tinylfu_admit(u32::from(freq[cand as usize]), count) {
                for _ in 0..n {
                    let t = match self.q[T_PROB as usize].tail {
                        NIL => self.q[T_PROT as usize].tail,
                        t => t,
                    };
                    if t == NIL {
                        break;
                    }
                    self.unlink(t);
                }
                self.unlink(cand);
                self.push(cand, T_PROB, cb, 0);
            } else {
                self.unlink(cand);
            }
        }
        self.tlfu_trim();
    }

    /// W-TinyLFU: entries out by `T_EVICT_ORDER` (each segment's least recent) until the cache is within its size.
    fn tlfu_trim(&mut self) {
        while self.held() > self.cap {
            let Some(t) = T_EVICT_ORDER.iter().map(|&g| self.q[g as usize].tail).find(|&t| t != NIL) else { break };
            self.unlink(t);
        }
    }
}

/// The eviction policy a cache runs, which the curve's policy correction simulates (design 11, step 6: "mini-simulations
/// of the real policy"). Under LRU the stack-distance model is the policy itself, so its correction is 1 and no
/// simulation runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    /// Least recently used, by bytes (every tracked cache today: `track::policy`).
    Lru,
    /// S3-FIFO in bytes (cache plan C3), simulated at `SIM_FRACS` of the size.
    S3Fifo,
    /// W-TinyLFU in bytes (cache plan C5, the condition cache under `cache_policy` = 2; D4889): a window of 1 % of the
    /// size, then main (probation, protected at 80 %), the window's least recent entry admitted into main only when it was
    /// looked up more often than the main entries it would push out, summed (`cache::tinylfu_*`). Its frequencies are the
    /// sample's exact counts (4 bits, halved every `cache::TINYLFU_RESET_ADDS` lookups the sample stands for): the
    /// cache's count-min sketch with no collisions.
    TinyLfu,
}

/// One cache's sample, distances, histograms and mini-simulations.
pub struct Tracker {
    cap: usize,
    /// Live keys: `heap[..n]`; slots below `fresh` that are not live are on `free`.
    n: usize,
    fresh: usize,
    free: Vec<u32>,
    slots: Vec<Slot>,
    table: Vec<u32>,
    shift: u32,
    heap: Vec<u32>,
    /// Fenwick tree over reference times: each sampled key's `wb` at its last reference.
    fw: Vec<u64>,
    at: Vec<u32>,
    now: u32,
    total: u64,
    door: u64,
    built_door: u64,
    sims: [Sim; 4],
    policy: Policy,
    /// W-TinyLFU (D4889): each sampled key's lookup count (4 bits, its slot's), and the lookups the sample stood for since
    /// the counts last halved.
    freq: Vec<u8>,
    freq_adds: f64,
    outcomes: bool,
    /// The switch's bytes mode ([`Self::set_price_bytes`]): a miss costs its entry's bytes.
    price_bytes: bool,
    /// A6: a miss's price per byte it reloads (ns; 0: price by the stall alone), [`Self::set_ns_per_byte`].
    ns_per_byte: f64,
    size: u64,
    price_mean: f64,
    /// The mean entry a fill brought back (bytes): a key whose door knows no size counts this until its fill does.
    size_mean: f64,
    started: bool,
    epoch_start: u64,
    last_ns: u64,
    epochs: u64,
    refs: u64,
    cur: [f64; BINS],
    cur_cold: f64,
    cur_refs: f64,
    rank: [f64; BINS],
    rank_cold: f64,
    blocks: [[f64; BINS]; BLOCKS],
    block: usize,
    block_epochs: u32,
}

impl Tracker {
    /// A tracker holding at most `cap` sampled keys ([`SAMPLES`] by default); every array is allocated here, once.
    pub fn new(cap: usize) -> Result<Tracker, TryReserveError> {
        let cap = cap.clamp(2, 1 << 24);
        let tbl = (2 * cap).next_power_of_two();
        let sims = [Sim::new(cap)?, Sim::new(cap)?, Sim::new(cap)?, Sim::new(cap)?];
        Ok(Tracker {
            cap,
            n: 0,
            fresh: 0,
            free: {
                let mut v = Vec::new();
                v.try_reserve_exact(cap)?;
                v
            },
            slots: filled(cap, NO_SLOT)?,
            table: filled(tbl, 0)?,
            shift: 64 - tbl.trailing_zeros(),
            heap: filled(cap, 0)?,
            fw: filled(4 * cap + 1, 0)?,
            at: filled(4 * cap + 1, 0)?,
            now: 0,
            total: 0,
            door: DOOR_ALL,
            built_door: DOOR_ALL,
            sims,
            policy: Policy::S3Fifo,
            freq: filled(cap, 0)?,
            freq_adds: 0.0,
            outcomes: false,
            price_bytes: false,
            ns_per_byte: 0.0,
            size: 0,
            price_mean: 0.0,
            size_mean: 0.0,
            started: false,
            epoch_start: 0,
            last_ns: 0,
            epochs: 0,
            refs: 0,
            cur: [0.0; BINS],
            cur_cold: 0.0,
            cur_refs: 0.0,
            rank: [0.0; BINS],
            rank_cold: 0.0,
            blocks: [[0.0; BINS]; BLOCKS],
            block: 0,
            block_epochs: 0,
        })
    }

    /// Heap bytes the tracker holds (fixed from construction).
    pub fn heap_bytes(&self) -> u64 {
        let s = core::mem::size_of::<Slot>() * self.slots.capacity()
            + 4 * (self.table.capacity() + self.heap.capacity() + self.at.capacity() + self.free.capacity())
            + 8 * self.fw.capacity()
            + self.sims.iter().map(|s| core::mem::size_of::<Node>() * s.nodes.capacity()).sum::<usize>()
            + self.freq.capacity();
        s as u64
    }

    /// The threshold the door samples at ([`door_pass`]).
    pub fn door(&self) -> u64 {
        self.door
    }

    /// The eviction policy the curve corrects for ([`Policy::S3Fifo`] until set).
    pub fn set_policy(&mut self, p: Policy) {
        if p == self.policy {
            return;
        }
        self.policy = p;
        // the simulations restart empty under the new policy (their queues mean another thing)
        for s in &mut self.sims {
            for i in 0..s.nodes.len() as u32 {
                s.unlink(i);
            }
            s.loss = 0.0;
            s.rank = 0.0;
        }
        self.freq.iter_mut().for_each(|f| *f = 0);
        self.freq_adds = 0.0;
    }

    /// The cache's door reports its sampled misses (a timed fill of the key it referenced): each reference is binned once
    /// its outcome is known, a hit under the size's distance bin (the LRU cache held it), a miss over it. That
    /// takes the sample's error out of the distances near the size, where an under-pressure-free cache's whole working
    /// set sits; a door whose fills carry another key (the part cache's) leaves it off.
    pub fn set_outcomes(&mut self, on: bool) {
        if !on {
            self.settle_all();
        }
        self.outcomes = on;
    }

    /// The switch's bytes mode (`cache_track` = 3, a validation against measured miss bytes): every miss costs the bytes it
    /// reloads ([`Self::reload`]: its key's last read-back, else its entry's bytes), so a curve's losses are the bytes a
    /// cache that size would read back. A miss count and a miss byte count differ where one entry refills in many faults
    /// (a decoded dictionary payload, block by block).
    pub fn set_price_bytes(&mut self, on: bool) {
        self.price_bytes = on;
    }

    /// A6: a key with no re-fill stall measured yet prices a miss by the bytes it reloads ([`Self::reload`]; else its size)
    /// at `ns` a byte (the part cache: C0's measured slowdown per missed byte), not at the cache's flat seed. 0: off.
    pub fn set_ns_per_byte(&mut self, ns: f64) {
        self.ns_per_byte = ns;
    }

    /// A6: `bytes` read back into the cache for `key` (an image, a decoded dictionary block its door's lookup stands for),
    /// summed over one use of the key (until a reference that starts a new one, [`Self::use_distance`]): the blocks a
    /// statement read back, not the entry's whole size. The largest use's sum is what a miss of it costs in bytes. A key the
    /// sample does not hold is dropped.
    pub fn reload(&mut self, key: u64, bytes: u64, now_ns: u64) {
        self.clock(now_ns);
        if let Some(s) = self.find(key) {
            let sl = &mut self.slots[s];
            sl.reload_acc = sl.reload_acc.saturating_add(bytes);
        }
    }

    /// A6: the distance past which a reference is a new use of its key (it misses at the smallest size the curve predicts
    /// at, [`PREDICT_AT`]); 0 while the cache's size is unknown (every reference is).
    fn use_distance(&self) -> u64 {
        (self.size as f64 * PREDICT_AT.iter().copied().fold(1.0, f64::min)) as u64
    }

    /// The bytes a miss of slot `s` reloads: the largest use's read-back, else its size.
    fn reload_bytes(&self, s: usize) -> u64 {
        let sl = &self.slots[s];
        if sl.reload > 0 {
            sl.reload
        } else {
            sl.size
        }
    }

    /// Every pending reference binned as a hit (no fill came): before an epoch closes, or a reading.
    pub fn settle_all(&mut self) {
        for i in 0..self.n {
            let s = self.heap[i] as usize;
            self.settle(s, true);
        }
    }

    fn settle(&mut self, s: usize, hit: bool) {
        let (w, d) = (self.slots[s].pend_w, self.slots[s].pend_d);
        if w == 0.0 {
            return;
        }
        self.slots[s].pend_w = 0.0;
        let cost = w * self.price(s);
        if d == COLD_D {
            self.cur_cold += cost;
            return;
        }
        // the curve spreads a bin's mass evenly over it: a hit goes under the size's bin, a miss over it, so neither is
        // read on the wrong side of the size
        let d = match (hit, self.size) {
            (_, 0) => d,
            (true, z) => d.min(edges(bin(z)).0.saturating_sub(1)),
            (false, z) => d.max(edges(bin(z)).1),
        };
        self.cur[bin(d)] += cost;
    }

    /// The losses counted so far dropped, the sample and its distances kept: a measurement window starts (a validation
    /// replay's hot pass). Only while no epoch has closed: the window is the current one.
    pub fn restart_window(&mut self) {
        self.settle_all();
        self.cur = [0.0; BINS];
        self.cur_cold = 0.0;
        self.cur_refs = 0.0;
        self.refs = 0;
        for s in &mut self.sims {
            s.loss = 0.0;
        }
    }

    /// The cache's size now; the mini-simulations follow it.
    pub fn set_size(&mut self, size: u64) {
        if size == self.size {
            return;
        }
        self.size = size;
        let policy = self.policy;
        for (s, f) in self.sims.iter_mut().zip(SIM_FRACS) {
            s.resize((size as f64 * f) as u64, policy);
        }
    }

    /// The price of a reference to a key with no measured miss yet, until fills measure one.
    pub fn seed_price(&mut self, ns: u64) {
        if self.price_mean == 0.0 {
            self.price_mean = ns as f64;
        }
    }

    fn home(&self, key: u64) -> usize {
        (key.wrapping_mul(0xD6E8_FEB8_6659_FD93) >> self.shift) as usize
    }

    fn find(&self, key: u64) -> Option<usize> {
        let mask = self.table.len() - 1;
        let mut i = self.home(key);
        loop {
            let e = self.table[i] as usize;
            if e == 0 {
                return None;
            }
            if self.slots[e - 1].key == key {
                return Some(e - 1);
            }
            i = (i + 1) & mask;
        }
    }

    fn tbl_insert(&mut self, s: usize) {
        let mask = self.table.len() - 1;
        let mut i = self.home(self.slots[s].key);
        while self.table[i] != 0 {
            i = (i + 1) & mask;
        }
        self.table[i] = s as u32 + 1;
    }

    fn tbl_remove(&mut self, s: usize) {
        let mask = self.table.len() - 1;
        let mut p = self.home(self.slots[s].key);
        while self.table[p] as usize != s + 1 {
            p = (p + 1) & mask;
        }
        let mut j = p;
        loop {
            j = (j + 1) & mask;
            let e = self.table[j] as usize;
            if e == 0 {
                break;
            }
            let k = self.home(self.slots[e - 1].key);
            if (j.wrapping_sub(k) & mask) >= (j.wrapping_sub(p) & mask) {
                self.table[p] = self.table[j];
                p = j;
            }
        }
        self.table[p] = 0;
    }

    fn heap_swap(&mut self, a: usize, b: usize) {
        self.heap.swap(a, b);
        self.slots[self.heap[a] as usize].hpos = a as u32;
        self.slots[self.heap[b] as usize].hpos = b as u32;
    }

    fn pri_at(&self, i: usize) -> u64 {
        self.slots[self.heap[i] as usize].pri
    }

    fn sift(&mut self, mut i: usize) {
        while i > 0 && self.pri_at((i - 1) / 2) < self.pri_at(i) {
            self.heap_swap(i, (i - 1) / 2);
            i = (i - 1) / 2;
        }
        loop {
            let (l, r) = (2 * i + 1, 2 * i + 2);
            let mut m = i;
            if l < self.n && self.pri_at(l) > self.pri_at(m) {
                m = l;
            }
            if r < self.n && self.pri_at(r) > self.pri_at(m) {
                m = r;
            }
            if m == i {
                return;
            }
            self.heap_swap(i, m);
            i = m;
        }
    }

    fn fw_add(&mut self, i: u32, d: u64) {
        let mut i = i as usize;
        while i < self.fw.len() {
            self.fw[i] = self.fw[i].wrapping_add(d);
            i += i & i.wrapping_neg();
        }
    }

    fn fw_prefix(&self, i: u32) -> u64 {
        let (mut s, mut i) = (0u64, i as usize);
        while i > 0 {
            s = s.wrapping_add(self.fw[i]);
            i &= i - 1;
        }
        s
    }

    /// The bytes a key stands for in a distance: size / r, r = min(1, door * dsize / 2^40).
    fn wb(&self, size: u64, dsize: u64) -> u64 {
        let pass = u128::from(self.door.max(1)) * u128::from(dsize.max(1));
        if pass >= u128::from(DOOR_ALL) {
            return size;
        }
        (u128::from(size) * u128::from(DOOR_ALL) / pass).min(u128::from(u64::MAX)) as u64
    }

    fn stamp(&mut self, s: usize) {
        if self.now as usize + 1 >= self.fw.len() {
            self.rebuild();
        }
        self.now += 1;
        self.slots[s].last = self.now;
        self.at[self.now as usize] = s as u32 + 1;
        let wb = self.slots[s].wb;
        self.fw_add(self.now, wb);
        self.total += wb;
    }

    fn unstamp(&mut self, s: usize) {
        let (l, wb) = (self.slots[s].last, self.slots[s].wb);
        self.at[l as usize] = 0;
        self.fw_add(l, wb.wrapping_neg());
        self.total -= wb;
    }

    /// Renumbers the live keys' times from 1 and re-weighs them at the current door, in one linear pass.
    fn rebuild(&mut self) {
        let mut j = 0;
        for t in 1..=self.now as usize {
            let a = self.at[t];
            if a != 0 {
                j += 1;
                self.at[j] = a;
                self.slots[a as usize - 1].last = j as u32;
            }
        }
        self.at[j + 1..=self.now as usize].fill(0);
        self.now = j as u32;
        self.fw.fill(0);
        self.total = 0;
        for t in 1..=j {
            let s = self.at[t] as usize - 1;
            let wb = self.wb(self.slots[s].size, self.slots[s].dsize);
            self.slots[s].wb = wb;
            self.fw[t] = wb;
            self.total += wb;
        }
        let len = self.fw.len();
        for i in 1..len {
            let p = i + (i & i.wrapping_neg());
            if p < len {
                self.fw[p] = self.fw[p].wrapping_add(self.fw[i]);
            }
        }
        self.built_door = self.door;
    }

    fn forget(&mut self, s: usize) {
        self.settle(s, true);
        self.tbl_remove(s);
        self.unstamp(s);
        for sim in &mut self.sims {
            sim.unlink(s as u32);
        }
        self.freq[s] = 0;
    }

    /// A new key into the sample: a free slot, or the slot of the highest priority when this key's is lower.
    fn admit(&mut self, key: u64, size: u64, dsize: u64, pri: u64) -> Option<usize> {
        let (s, hpos) = if self.n < self.cap {
            let s = match self.free.pop() {
                Some(f) => f as usize,
                None => {
                    self.fresh += 1;
                    self.fresh - 1
                }
            };
            self.heap[self.n] = s as u32;
            self.n += 1;
            (s, self.n - 1)
        } else {
            let top = self.heap[0] as usize;
            let tp = self.slots[top].pri;
            if pri >= tp {
                self.door = self.door.min(pri);
                self.reweigh();
                return None;
            }
            self.forget(top);
            self.door = self.door.min(tp);
            (top, 0)
        };
        self.slots[s] = Slot { key, size, dsize, pri, hpos: hpos as u32, ..NO_SLOT };
        self.slots[s].wb = self.wb(size, dsize);
        self.sift(hpos);
        self.tbl_insert(s);
        self.reweigh();
        Some(s)
    }

    /// The door no higher than `door` (the drain's rate limit): the keys at or past it leave the sample.
    pub fn lower_door(&mut self, door: u64) {
        self.door = self.door.min(door.max(1));
        while self.n > 0 && self.slots[self.heap[0] as usize].pri >= self.door {
            let top = self.heap[0] as usize;
            self.forget(top);
            self.n -= 1;
            if self.n > 0 {
                self.heap[0] = self.heap[self.n];
                self.slots[self.heap[0] as usize].hpos = 0;
                self.sift(0);
            }
            self.free.push(top as u32);
        }
        self.reweigh();
    }

    /// The largest live sampled entry (bytes).
    pub fn max_size(&self) -> u64 {
        self.heap[..self.n].iter().map(|&s| self.slots[s as usize].size).max().unwrap_or(0)
    }

    /// Live sampled keys.
    pub fn samples(&self) -> usize {
        self.n
    }

    /// Re-weighs the sample once the door has fallen a sixteenth since the last time.
    fn reweigh(&mut self) {
        if self.door < self.built_door - self.built_door / 16 {
            self.rebuild();
        }
    }

    fn price(&self, s: usize) -> f64 {
        let sl = &self.slots[s];
        if self.price_bytes {
            return self.reload_bytes(s) as f64;
        }
        let base = if sl.price > 0 {
            sl.price as f64
        } else if self.ns_per_byte > 0.0 {
            self.reload_bytes(s) as f64 * self.ns_per_byte
        } else {
            self.price_mean
        };
        base + sl.coupled.max(sl.coupled_acc) as f64
    }

    /// A sampled lookup (hit or miss) of `key`, `bytes` large (0: its door knows no size; it was sampled at
    /// [`UNIFORM`]), at monotonic time `now_ns`.
    pub fn reference(&mut self, key: u64, bytes: u64, now_ns: u64) {
        self.reference_hashed(key, hash24(key), bytes, now_ns);
    }

    /// [`Self::reference`] with the key's sampling hash `h` as its door computed it (`memledger::value::Sample::h`: a
    /// door that holds its own uniform hash samples by it, `lookup_hashed`).
    pub fn reference_hashed(&mut self, key: u64, h: u32, bytes: u64, now_ns: u64) {
        self.clock(now_ns);
        let dsize = if bytes == 0 { UNIFORM } else { bytes };
        if !door_pass(h, dsize, self.door) {
            return;
        }
        self.refs += 1;
        let pri = (u64::from(h) << 16) / dsize;
        let (s, d) = match self.find(key) {
            Some(s) => {
                self.settle(s, true);
                let sl = self.slots[s];
                let size = if bytes == 0 { sl.size } else { bytes };
                let d = self.total.wrapping_sub(self.fw_prefix(sl.last)).saturating_add(size);
                // A6: a reference that misses at the smallest size the curve predicts at starts a new use of the key; what
                // was read back of it since the last such reference was one use's reload (a use's own block faults pass
                // the door too, as near references, and add to it). The largest use's stands as what a miss reloads.
                if d >= self.use_distance() {
                    let sl = &mut self.slots[s];
                    sl.reload = sl.reload.max(sl.reload_acc);
                    sl.reload_acc = 0;
                }
                self.unstamp(s);
                if size != sl.size || dsize != sl.dsize {
                    let sl = &mut self.slots[s];
                    (sl.size, sl.dsize, sl.pri) = (size, dsize, pri);
                    self.sift(self.slots[s].hpos as usize);
                }
                self.slots[s].wb = self.wb(size, dsize);
                (s, Some(d))
            }
            None => {
                let size = if bytes == 0 { (self.size_mean as u64).max(1) } else { bytes };
                match self.admit(key, size, dsize, pri) {
                    Some(s) => (s, None),
                    None => return,
                }
            }
        };
        let w = self.slots[s].wb as f64 / self.slots[s].size as f64;
        let cost = w * self.price(s);
        self.cur_refs += w;
        if self.outcomes {
            (self.slots[s].pend_w, self.slots[s].pend_d) = (w, d.unwrap_or(COLD_D));
        } else {
            match d {
                Some(d) => self.cur[bin(d)] += cost,
                None => self.cur_cold += cost,
            }
        }
        self.stamp(s);
        match self.policy {
            Policy::S3Fifo => {
                let wb = self.slots[s].wb;
                for sim in &mut self.sims {
                    sim.reference(s as u32, wb, cost);
                }
            }
            Policy::TinyLfu => {
                // the cache's sketch counts every lookup; this one stands for `w` of them
                self.freq[s] = (self.freq[s] + 1).min(15);
                self.freq_adds += w;
                if self.freq_adds >= crate::cache::TINYLFU_RESET_ADDS as f64 {
                    self.freq_adds -= crate::cache::TINYLFU_RESET_ADDS as f64;
                    self.freq.iter_mut().for_each(|f| *f /= 2);
                }
                let wb = self.slots[s].wb;
                for sim in &mut self.sims {
                    sim.tlfu_reference(s as u32, wb, cost, &self.freq);
                }
            }
            Policy::Lru => {}
        }
    }

    /// The fill after a sampled miss, `bytes` brought back; a key whose door knows no size takes its size from it.
    /// Price (broker ruling 10-16, C2 validation): only a RE-fill prices -- a fill of a key the sample saw filled before,
    /// or one settling a reference with a distance (a miss under pressure): its stall becomes the key's price and feeds
    /// the cache's mean. A first fill (a cold build, which also pays its inputs' cold reads) never prices; until a
    /// re-fill is seen the cache's seed ([`Self::seed_price`]: C0's measured cost per extra miss) stands.
    pub fn fill(&mut self, key: u64, bytes: u64, refill_ns: u64, now_ns: u64) {
        self.clock(now_ns);
        let ewma = |m: f64, x: f64| if m == 0.0 { x } else { m + (x - m) / 32.0 };
        if bytes > 0 {
            self.size_mean = ewma(self.size_mean, bytes as f64);
        }
        let Some(s) = self.find(key) else { return };
        let refill = {
            let sl = &self.slots[s];
            sl.filled || (sl.pend_w > 0.0 && sl.pend_d != COLD_D)
        };
        if refill_ns > 0 && refill {
            self.price_mean = ewma(self.price_mean, refill_ns as f64);
        }
        let sl = &mut self.slots[s];
        sl.filled = true;
        if refill_ns > 0 && refill {
            sl.price = refill_ns;
            sl.coupled = sl.coupled_acc;
            sl.coupled_acc = 0;
        }
        if bytes > 0 && sl.dsize == UNIFORM && sl.size != bytes {
            let (old, last) = (sl.wb, sl.last);
            sl.size = bytes;
            let new = self.wb(bytes, UNIFORM);
            self.slots[s].wb = new;
            self.fw_add(last, new.wrapping_sub(old));
            self.total = self.total - old + new;
        }
        self.settle(s, false);
    }

    /// Work that died with `key` (a part's fp tables, its condition entries) and was redone after its last refill.
    pub fn couple(&mut self, key: u64, ns: u64) {
        if let Some(s) = self.find(key) {
            self.slots[s].coupled_acc = self.slots[s].coupled_acc.saturating_add(ns);
        }
    }

    /// Advances the clock, closing every finished 1-min epoch.
    pub fn tick(&mut self, now_ns: u64) {
        self.clock(now_ns);
    }

    fn clock(&mut self, now: u64) {
        if !self.started {
            self.started = true;
            self.epoch_start = now;
        }
        self.last_ns = self.last_ns.max(now);
        if now < self.epoch_start.saturating_add(EPOCH_NS) {
            return;
        }
        let k = (now - self.epoch_start) / EPOCH_NS;
        self.fold();
        for _ in 1..k.min(BLOCKS as u64 * BLOCK_EPOCHS as u64 + 1) {
            self.fold();
        }
        let left = k.saturating_sub(1 + BLOCKS as u64 * BLOCK_EPOCHS as u64);
        if left > 0 {
            let keep = 0.5f64.powf(left as f64 / HALF_LIFE_EPOCHS);
            self.rank.iter_mut().for_each(|x| *x *= keep);
            self.rank_cold *= keep;
            self.sims.iter_mut().for_each(|s| s.rank *= keep);
        }
        self.epoch_start += k * EPOCH_NS;
    }

    /// Closes one epoch: its rates into the EWMA and the open floor block, then an empty epoch starts.
    fn fold(&mut self) {
        self.settle_all();
        let secs = EPOCH_NS as f64 / 1e9;
        let a = if self.epochs == 0 { 1.0 } else { 1.0 - 0.5f64.powf(1.0 / HALF_LIFE_EPOCHS) };
        let blk = &mut self.blocks[self.block];
        for b in 0..BINS {
            let x = self.cur[b] / secs;
            self.rank[b] += a * (x - self.rank[b]);
            blk[b] = blk[b].max(x);
        }
        self.rank_cold += a * (self.cur_cold / secs - self.rank_cold);
        for s in &mut self.sims {
            s.rank += a * (s.loss / secs - s.rank);
            s.loss = 0.0;
        }
        self.cur = [0.0; BINS];
        self.cur_cold = 0.0;
        self.cur_refs = 0.0;
        self.epochs += 1;
        self.block_epochs += 1;
        if self.block_epochs == BLOCK_EPOCHS {
            self.block_epochs = 0;
            self.block = (self.block + 1) % BLOCKS;
            self.blocks[self.block] = [0.0; BINS];
        }
    }

    /// The histograms the curve reads, in ns/s: the ranking one, the floor one, the simulations' losses, and the loss
    /// of every reference (all miss at size 0).
    fn rates(&self) -> ([f64; BINS], [f64; BINS], [f64; 4], f64) {
        if self.epochs == 0 {
            let secs = (self.last_ns.saturating_sub(self.epoch_start) as f64 / 1e9).max(1.0);
            let mut h = self.cur;
            h.iter_mut().for_each(|x| *x /= secs);
            let sims = [0, 1, 2, 3].map(|i| self.sims[i].loss / secs);
            let all = h.iter().sum::<f64>() + self.cur_cold / secs;
            return (h, h, sims, all);
        }
        let mut f = [0.0f64; BINS];
        for blk in &self.blocks {
            for b in 0..BINS {
                f[b] = f[b].max(blk[b]);
            }
        }
        let all = self.rank.iter().sum::<f64>() + self.rank_cold;
        (self.rank, f, [0, 1, 2, 3].map(|i| self.sims[i].rank), all)
    }

    fn lru(&self, h: &[f64; BINS], s: u64) -> f64 {
        (above(h, s) - above(h, self.size)).max(0.0)
    }

    /// kappa at `KAPPA_AT` x size: the policy's extra loss over the LRU model's, clamped. At 0 the policy misses `all`.
    fn kappas(&self, h: &[f64; BINS], sims: [f64; 4], all: f64) -> [f64; 4] {
        [0, 1, 2, 3].map(|i| {
            let d = self.lru(h, (self.size as f64 * KAPPA_AT[i]) as u64);
            let m = if i == 0 { all } else { sims[i - 1] };
            if d <= 1e-12 * (1.0 + all) {
                return 1.0;
            }
            ((m - sims[3]) / d).clamp(KAPPA_MIN, KAPPA_MAX)
        })
    }

    fn kappa_at(&self, k: [f64; 4], s: u64) -> f64 {
        let x = s as f64 / self.size.max(1) as f64;
        match KAPPA_AT.iter().rposition(|&a| a <= x) {
            Some(i) if i + 1 < KAPPA_AT.len() => k[i] + (x - KAPPA_AT[i]) / (KAPPA_AT[i + 1] - KAPPA_AT[i]) * (k[i + 1] - k[i]),
            _ => k[KAPPA_AT.len() - 1],
        }
    }

    /// Raw points (size, kappa-corrected loss vs the current size): 0, every bin edge below the size, the size.
    fn points(&self, h: &[f64; BINS], k: Option<[f64; 4]>, out: &mut [(u64, f64); POINTS]) -> usize {
        let y = |s: u64| self.lru(h, s) * k.map_or(1.0, |k| self.kappa_at(k, s));
        let mut n = 0;
        out[n] = (0, y(0));
        n += 1;
        for b in 1..BINS {
            let lo = edges(b).0;
            if lo >= self.size {
                break;
            }
            out[n] = (lo, y(lo));
            n += 1;
        }
        if self.size > 0 {
            out[n] = (self.size, 0.0);
            n += 1;
        }
        n
    }

    /// The curve now; `policy` applies the S3-FIFO correction (else the plain LRU model).
    pub fn curve_with(&self, policy: bool) -> Curve {
        let (h, f, sims, all) = self.rates();
        let k = self.correction(policy, &h, sims, all);
        let mut c = NO_CURVE;
        c.size = self.size;
        c.samples = self.n as u32;
        c.refs = self.refs;
        c.door = self.door;
        c.kappa = k.unwrap_or([1.0; 4]);
        let live = self.heap[..self.n].iter().map(|&s| &self.slots[s as usize]);
        c.rate = live.map(|s| s.size as f64 / s.wb.max(1) as f64).sum::<f64>() / self.n.max(1) as f64;
        c.raw = PREDICT_AT.map(|f| {
            let s = (self.size as f64 * f) as u64;
            self.lru(&h, s) * k.map_or(1.0, |k| self.kappa_at(k, s))
        });
        c.open_secs = self.last_ns.saturating_sub(self.epoch_start) as f64 / 1e9;
        c.open = PREDICT_AT.map(|f| self.lru(&self.cur, (self.size as f64 * f) as u64));
        c.open_hist = self.cur;
        let (mut p, mut fp) = ([(0, 0.0); POINTS], [(0, 0.0); POINTS]);
        let n = self.points(&h, k, &mut p);
        let fnn = self.points(&f, k, &mut fp);
        c.finish(&p[..n], Some(&fp[..fnn]))
    }

    pub fn curve(&self) -> Curve {
        self.curve_with(true)
    }

    fn correction(&self, policy: bool, h: &[f64; BINS], sims: [f64; 4], all: f64) -> Option<[f64; 4]> {
        (policy && self.size > 0 && self.policy != Policy::Lru).then(|| self.kappas(h, sims, all))
    }

    /// The model's loss at `s` bytes before the hull (ns/s over the current size; the policy correction applied when
    /// `policy`): what a cache cut to `s` would lose, for checking against a measured slowdown.
    pub fn loss_at(&self, s: u64, policy: bool) -> f64 {
        let (h, _, sims, all) = self.rates();
        let k = self.correction(policy, &h, sims, all);
        self.lru(&h, s) * k.map_or(1.0, |k| self.kappa_at(k, s))
    }
}

/// Minutes of the build pool's window (design 11.13: the max over 10 min of the bytes in use at once).
pub const POOL_MINUTES: usize = 10;

/// The build pool's curve: it has no keys, so its value is its peak concurrent demand K. Bytes above K are worth 0;
/// each byte below K costs the allocations it would force: v = allocation cost * take rate / K.
pub struct PoolCurve {
    peak: [u64; POOL_MINUTES],
    takes: [u64; POOL_MINUTES],
    i: usize,
    minutes: u64,
    started: bool,
    start: u64,
    last_ns: u64,
    alloc_ns: f64,
}

impl Default for PoolCurve {
    fn default() -> Self {
        Self::new()
    }
}

impl PoolCurve {
    pub const fn new() -> PoolCurve {
        PoolCurve {
            peak: [0; POOL_MINUTES],
            takes: [0; POOL_MINUTES],
            i: 0,
            minutes: 0,
            started: false,
            start: 0,
            last_ns: 0,
            alloc_ns: 0.0,
        }
    }

    fn roll(&mut self, now: u64) {
        if !self.started {
            self.started = true;
            self.start = now;
        }
        self.last_ns = self.last_ns.max(now);
        let mut k = 0;
        while now >= self.start + EPOCH_NS {
            self.start += EPOCH_NS;
            self.minutes += 1;
            k += 1;
            if k <= POOL_MINUTES {
                self.i = (self.i + 1) % POOL_MINUTES;
                self.peak[self.i] = 0;
                self.takes[self.i] = 0;
            }
        }
    }

    /// The pool's bytes in use at `now`.
    pub fn observe(&mut self, now: u64, in_use: u64) {
        self.roll(now);
        self.peak[self.i] = self.peak[self.i].max(in_use);
    }

    /// One buffer taken from the pool; `alloc_ns` is what a fresh allocation of it costs.
    pub fn take(&mut self, now: u64, alloc_ns: u64) {
        self.roll(now);
        self.takes[self.i] += 1;
        let x = alloc_ns as f64;
        self.alloc_ns = if self.alloc_ns == 0.0 { x } else { self.alloc_ns + (x - self.alloc_ns) / 32.0 };
    }

    /// The peak concurrent demand K over the window.
    pub fn peak(&self) -> u64 {
        self.peak.iter().copied().max().unwrap_or(0)
    }

    pub fn curve(&self, size: u64) -> Curve {
        let k = self.peak();
        let full = self.minutes.min(POOL_MINUTES as u64 - 1) * EPOCH_NS + (self.last_ns - self.start);
        let rate = self.takes.iter().sum::<u64>() as f64 / (full as f64 / 1e9).max(1.0);
        let l0 = rate * self.alloc_ns;
        let g = |s: u64| if k == 0 || s >= k { 0.0 } else { l0 * (k - s) as f64 / k as f64 };
        let mut pts = [(0, 0.0); 3];
        let mut n = 0;
        pts[n] = (0, g(0) - g(size));
        n += 1;
        if k > 0 && k < size {
            pts[n] = (k, 0.0);
            n += 1;
        }
        if size > 0 {
            pts[n] = (size, 0.0);
            n += 1;
        }
        let mut c = NO_CURVE;
        c.size = size;
        c.refs = self.takes.iter().sum();
        c.rate = 1.0;
        c.finish(&pts[..n], None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
        fn unit(&mut self) -> f64 {
            (self.next() >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    /// Exact byte-LRU miss ratios: a reference misses at s iff the distinct bytes since its last use, plus its own,
    /// exceed s. Independent of the tracker: every reference, a plain Fenwick over all times.
    fn exact(trace: &[(u64, u64)], grid: &[u64]) -> Vec<f64> {
        let n = trace.len();
        let mut fw = vec![0i64; n + 1];
        let mut last: HashMap<u64, (usize, u64)> = HashMap::new();
        let mut total = 0i64;
        let mut ds = Vec::with_capacity(n);
        let add = |fw: &mut Vec<i64>, mut i: usize, d: i64| {
            while i <= n {
                fw[i] += d;
                i += i & i.wrapping_neg();
            }
        };
        for (i, &(k, sz)) in trace.iter().enumerate() {
            let t = i + 1;
            if let Some(&(l, lsz)) = last.get(&k) {
                let mut p = 0i64;
                let mut j = l;
                while j > 0 {
                    p += fw[j];
                    j &= j - 1;
                }
                ds.push((total - p) as u64 + sz);
                add(&mut fw, l, -(lsz as i64));
                total -= lsz as i64;
            } else {
                ds.push(u64::MAX);
            }
            add(&mut fw, t, sz as i64);
            total += sz as i64;
            last.insert(k, (t, sz));
        }
        grid.iter().map(|&s| ds.iter().filter(|&&d| d > s).count() as f64 / n as f64).collect()
    }

    /// The sampled miss ratio over the true reference count (SHARDS-adj: a hot key's over- or under-sampling is hits).
    fn est(t: &Tracker, s: u64, n: usize) -> f64 {
        (above(&t.cur, s) + t.cur_cold) / n as f64
    }

    fn grid(lo: u64, hi: u64, n: usize) -> Vec<u64> {
        (0..n).map(|i| (lo as f64 * (hi as f64 / lo as f64).powf(i as f64 / (n - 1) as f64)) as u64).collect()
    }

    /// 100 evenly spaced sizes up to the trace's footprint (the usual MRC MAE grid). Below ~1 % of the footprint a
    /// spatial sample's error is the few hottest keys' luck (in the sample or not), as SHARDS reports.
    fn even(trace: &[(u64, u64)]) -> Vec<u64> {
        let mut seen: HashMap<u64, u64> = HashMap::new();
        for &(k, sz) in trace {
            seen.insert(k, sz);
        }
        let foot: u64 = seen.values().sum();
        (1..=100).map(|i| foot * i / 100).collect()
    }

    fn zipf_trace(rng: &mut Rng, keys: usize, refs: usize, alpha: f64, size: impl Fn(u64) -> u64) -> Vec<(u64, u64)> {
        let mut cdf = Vec::with_capacity(keys);
        let mut acc = 0.0;
        for i in 0..keys {
            acc += 1.0 / ((i + 1) as f64).powf(alpha);
            cdf.push(acc);
        }
        (0..refs)
            .map(|_| {
                let u = rng.unit() * acc;
                let k = cdf.partition_point(|&c| c < u) as u64;
                let key = k.wrapping_mul(0x2545_F491_4F6C_DD1D) ^ 0x5bd1;
                (key, size(k))
            })
            .collect()
    }

    fn feed(t: &mut Tracker, trace: &[(u64, u64)]) {
        t.seed_price(1);
        for (i, &(k, sz)) in trace.iter().enumerate() {
            t.reference(k, sz, i as u64);
        }
    }

    fn mae(t: &Tracker, trace: &[(u64, u64)], g: &[u64]) -> f64 {
        let ex = exact(trace, g);
        g.iter().zip(&ex).map(|(&s, e)| (est(t, s, trace.len()) - e).abs()).sum::<f64>() / g.len() as f64
    }

    #[test]
    fn bins_cover_their_edges() {
        for b in 0..BINS {
            let (lo, hi) = edges(b);
            assert!(lo < hi);
            assert_eq!(bin(lo), b);
            assert_eq!(bin(hi - 1), b);
            if b + 1 < BINS {
                assert_eq!(edges(b + 1).0, hi);
            }
        }
        assert_eq!(bin(u64::MAX), BINS - 1);
    }

    #[test]
    fn door_matches_the_sample() {
        let mut t = Tracker::new(64).unwrap();
        let mut rng = Rng(7);
        for i in 0..5000u64 {
            let k = rng.next();
            t.reference(k, 4096 + (k % 7) * 4096, i);
        }
        assert_eq!(t.n, 64);
        assert!(t.door < DOOR_ALL);
        let check = |t: &Tracker| {
            for &i in &t.heap[..t.n] {
                let s = &t.slots[i as usize];
                assert!(door_pass(hash24(s.key), s.size, t.door));
                assert!(s.pri < t.door);
                assert_eq!(t.find(s.key), Some(i as usize));
            }
        };
        check(&t);
        let half = t.door / 2;
        t.lower_door(half);
        assert!(t.n < 64 && t.n > 0, "about half leave: {}", t.n);
        check(&t);
        for i in 0..5000u64 {
            let k = rng.next();
            t.reference(k, 4096 + (k % 7) * 4096, 5000 + i);
        }
        assert_eq!(t.n, 64, "freed slots are taken again");
        check(&t);
    }

    #[test]
    fn exact_when_everything_is_sampled() {
        let mut rng = Rng(11);
        let trace = zipf_trace(&mut rng, 600, 60_000, 0.9, |k| 4096 * (1 + k % 8));
        let mut t = Tracker::new(1024).unwrap();
        feed(&mut t, &trace);
        assert_eq!(t.door, DOOR_ALL);
        let m = mae(&t, &trace, &grid(8 << 10, 20 << 20, 40));
        assert!(m < 0.01, "mae {m}");
    }

    #[test]
    fn sampled_curve_within_mae_002() {
        let mut rng = Rng(3);
        let trace = zipf_trace(&mut rng, 30_000, 250_000, 0.8, |k| 4096 * (1 + (k * 7919) % 16));
        let mut t = Tracker::new(SAMPLES).unwrap();
        feed(&mut t, &trace);
        assert!(t.door < DOOR_ALL / 4096, "the sample must have filled: door {}", t.door);
        let m = mae(&t, &trace, &even(&trace));
        assert!(m < 0.02, "mae {m}");
    }

    #[test]
    fn sampled_fixed_size_within_mae_002() {
        let mut rng = Rng(5);
        let trace = zipf_trace(&mut rng, 50_000, 250_000, 1.0, |_| 8192);
        let mut t = Tracker::new(SAMPLES).unwrap();
        feed(&mut t, &trace);
        let m = mae(&t, &trace, &even(&trace));
        assert!(m < 0.02, "mae {m}");
    }

    #[test]
    fn hull_prices_the_flat_stretch_before_a_cliff() {
        // 3328 keys of 64 KiB = 208 MiB: mid-bin, so sampling noise keeps the cliff in one distance bin.
        let (keys, sz) = (3328u64, 64u64 << 10);
        let trace: Vec<(u64, u64)> = (0..10 * keys).map(|i| ((i % keys) * 977 + 13, sz)).collect();
        let mut t = Tracker::new(SAMPLES).unwrap();
        t.set_size(272 << 20);
        feed(&mut t, &trace);
        let cliff = (keys * sz) as f64;
        let lru = t.curve_with(false);
        let l0 = lru.loss0;
        assert!(l0 > 0.0);
        let (h, _, _, _) = t.rates();
        for s in [64u64 << 20, 128 << 20, 160 << 20] {
            let raw = t.lru(&h, s - BATCH) - t.lru(&h, s);
            assert!(raw.abs() < 1e-9 * l0, "raw curve is flat before the cliff");
            let slope = (lru.at(s - BATCH) - lru.at(s)) / BATCH as f64;
            let want = l0 / cliff;
            assert!(slope > 0.75 * want && slope < 1.05 * want, "slope {slope} want {want}");
        }
        // within the cliff's distance bin (a quarter octave)
        let k = lru.knee as f64 / (0.95 * cliff);
        assert!(k > 0.95 && k < 1.25, "knee {}", lru.knee);
        assert_eq!(lru.v, 0.0, "the batch above the cliff is free");
        let pol = t.curve();
        for s in [64u64 << 20, 128 << 20, 160 << 20] {
            assert!(pol.at(s - BATCH) > pol.at(s), "policy curve flat at {s}");
        }
    }

    /// Hot set W = 16 MiB referenced at random, one never-again scan key after each hot reference, S = 64 MiB. The
    /// LRU model charges the scan's bytes to the hot set's distances; the S3-FIFO correction must take most of it back.
    #[test]
    fn one_off_scan_is_not_worth_bytes() {
        let (hot, sz) = (512u64, 32u64 << 10);
        let mut rng = Rng(9);
        let mut trace = Vec::new();
        for i in 0..40_000u64 {
            trace.push((rng.below(hot), sz));
            trace.push(((1 << 40) + i, sz));
        }
        let mut t = Tracker::new(8192).unwrap();
        t.set_size(64 << 20);
        feed(&mut t, &trace);
        let (lru, pol) = (t.curve_with(false), t.curve());
        let w = hot * sz;
        assert!(pol.at(w) < 0.5 * lru.at(w), "corrected {} lru {}", pol.at(w), lru.at(w));
        assert!(pol.knee < lru.knee, "knee corrected {} lru {}", pol.knee, lru.knee);
        assert!(pol.v < 0.5 * lru.v, "v corrected {} lru {}", pol.v, lru.v);
    }

    /// A working set that fits the cache exactly: every reference after the first pass hits at a distance of the size.
    /// The sample's distances straddle the size; the hits' outcomes put them back under it.
    #[test]
    fn outcomes_keep_a_working_set_that_fits_at_its_size() {
        let (n, sz) = (4096u64, 4096u64);
        let size = n * sz;
        let mut t = Tracker::new(512).unwrap();
        t.set_size(size);
        t.set_policy(Policy::Lru);
        t.set_outcomes(true);
        t.seed_price(1000);
        for pass in 0..8 {
            for k in 0..n {
                let key = k.wrapping_mul(0x9E37_79B9) + 1;
                t.reference(key, sz, 0);
                if pass == 0 {
                    t.fill(key, sz, 1000, 0);
                }
            }
        }
        t.settle_all();
        assert!(t.door < DOOR_ALL / 4, "the sample must have cut its door: {}", t.door);
        let truth = 7.0 * n as f64 * 1000.0;
        for s in [size / 2, size / 4, size / 10] {
            let got = t.loss_at(s, false);
            assert!((got / truth - 1.0).abs() < 0.15, "at {s}: {got} vs {truth}");
        }
        assert!(t.loss_at(size, false) < 0.15 * truth, "nothing lost at the size: {}", t.loss_at(size, false));
    }

    /// Under LRU the model is the policy: no correction, no simulation, and the raw loss is the model's.
    #[test]
    fn an_lru_cache_takes_no_correction() {
        let (hot, sz) = (512u64, 32u64 << 10);
        let mut rng = Rng(9);
        let mut trace = Vec::new();
        for i in 0..40_000u64 {
            trace.push((rng.below(hot), sz));
            trace.push(((1 << 40) + i, sz));
        }
        let mut t = Tracker::new(8192).unwrap();
        t.set_size(64 << 20);
        t.set_policy(Policy::Lru);
        feed(&mut t, &trace);
        let (lru, pol) = (t.curve_with(false), t.curve());
        assert_eq!(pol.kappa, [1.0; 4]);
        assert!(t.sims.iter().all(|s| s.loss == 0.0 && s.q.iter().all(|q| q.bytes == 0)), "no simulation ran");
        for s in [0, 8 << 20, 16 << 20, 32 << 20, 48 << 20] {
            assert_eq!(pol.at(s), lru.at(s));
            assert_eq!(t.loss_at(s, true), t.loss_at(s, false));
            assert!(t.loss_at(s, false) + 1e-6 >= lru.at(s), "the hull lies under the raw loss at {s}");
        }
        assert_eq!(t.loss_at(64 << 20, false), 0.0, "nothing lost at the current size");
    }

    #[test]
    fn s3fifo_keeps_a_reused_set_against_a_scan() {
        let mut s = Sim::new(4096).unwrap();
        s.resize(100, Policy::S3Fifo);
        let mut misses = 0.0;
        for r in 0..20_000u32 {
            let before = s.loss;
            s.reference(r % 50, 1, 1.0);
            s.reference(50 + r % 4000, 1, 0.0);
            if r >= 10_000 {
                misses += s.loss - before;
            }
        }
        assert!(misses < 0.1 * 10_000.0, "hot misses {misses}");
        assert!(s.q[0].bytes + s.q[1].bytes <= 100);
    }

    #[test]
    fn tinylfu_keeps_a_frequent_set_against_a_scan() {
        let mut s = Sim::new(4096).unwrap();
        s.resize(100, Policy::TinyLfu);
        let mut freq = vec![0u8; 4096];
        let mut misses = 0.0;
        for r in 0..20_000u32 {
            let before = s.loss;
            for (k, cost) in [(r % 50, 1.0), (50 + r % 4000, 0.0)] {
                freq[k as usize] = (freq[k as usize] + 1).min(15);
                s.tlfu_reference(k, 1, cost, &freq);
            }
            if r >= 10_000 {
                misses += s.loss - before;
            }
        }
        assert!(misses < 0.1 * 10_000.0, "hot misses {misses}");
        assert!(s.held() <= 100);
        assert!(s.q[T_PROT as usize].bytes <= crate::cache::tinylfu_protected(100), "protected within its share");
        assert!(s.q[T_WIN as usize].bytes <= crate::cache::tinylfu_window(100), "the window within its share");
    }

    /// D4889: the condition cache under `cache_policy` = 2 runs W-TinyLFU; its curve is corrected by a W-TinyLFU
    /// simulation, not read as LRU.
    #[test]
    fn a_tinylfu_cache_is_corrected_by_its_own_simulation() {
        let (hot, sz) = (512u64, 32u64 << 10);
        let mut rng = Rng(9);
        let mut trace = Vec::new();
        for i in 0..40_000u64 {
            trace.push((rng.below(hot), sz));
            trace.push(((1 << 40) + i, sz));
        }
        let mut t = Tracker::new(8192).unwrap();
        t.set_size(64 << 20);
        t.set_policy(Policy::TinyLfu);
        feed(&mut t, &trace);
        assert!(t.sims.iter().any(|s| s.loss > 0.0), "the simulations ran");
        assert_ne!(t.curve().kappa, [1.0; 4], "corrected");
        // at a quarter of the size LRU's model loses the reused set (its distances span the one-off keys between its
        // uses); W-TinyLFU keeps it, the one-off keys never admitted
        let s = 16u64 << 20;
        let (lru, tlfu) = (t.loss_at(s, false), t.loss_at(s, true));
        assert!(tlfu < 0.75 * lru, "W-TinyLFU's loss {tlfu} vs LRU's {lru}");
        // the policy changed back: the simulations restart empty, the counts too
        t.set_policy(Policy::Lru);
        assert!(t.sims.iter().all(|s| s.loss == 0.0 && s.held() == 0));
        assert!(t.freq.iter().all(|&f| f == 0));
    }

    #[test]
    fn epochs_decay_and_floors_hold_two_hours() {
        let mut t = Tracker::new(256).unwrap();
        t.set_size(4 << 20);
        t.seed_price(1000);
        let mut rng = Rng(1);
        for i in 0..20_000u64 {
            t.reference(rng.below(64), 16 << 10, i * 1000);
        }
        t.tick(EPOCH_NS);
        let c0 = t.curve_with(false);
        let r0 = c0.loss0;
        assert!(r0 > 0.0 && c0.knee > 0 && c0.knee <= 5 << 18, "64 keys of 16 KiB: knee {}", c0.knee);
        t.tick(EPOCH_NS * (1 + HALF_LIFE_EPOCHS as u64));
        let r1 = t.curve_with(false).loss0;
        assert!((r1 / r0 - 0.5).abs() < 0.01, "half-life: {r1} vs {r0}");
        let fk = t.curve_with(false).floor_knee;
        assert!(fk > 0, "the floor keeps the 2 h max");
        t.tick(EPOCH_NS * (2 + (BLOCKS as u64) * BLOCK_EPOCHS as u64));
        assert_eq!(t.curve_with(false).floor_knee, 0, "the max ages out after its window");
    }

    #[test]
    fn an_unsized_door_learns_sizes_from_fills() {
        let mut t = Tracker::new(256).unwrap();
        t.seed_price(1);
        for k in 0..100u64 {
            t.reference(k, 0, k);
            t.fill(k, 64 << 10, 0, k);
        }
        for i in 100..1000u64 {
            t.reference(i % 100, 0, i);
        }
        assert!(est(&t, 6 << 20, 1000) > 0.99, "100 keys of 64 KiB apart: {}", est(&t, 6 << 20, 1000));
        assert!(est(&t, 7 << 20, 1000) < 0.11, "only the first loop misses: {}", est(&t, 7 << 20, 1000));
    }

    /// Broker ruling 10-16: the cache's price is its seed (C0's measured cost per extra miss) until a re-fill; cold first
    /// fills never move it.
    #[test]
    fn bytes_mode_prices_a_miss_by_its_bytes() {
        let mut t = Tracker::new(256).unwrap();
        t.seed_price(1);
        t.set_price_bytes(true);
        // 10 keys of 1 MiB and 10 of 4 KiB in one loop, ten times: every reference past the first loop is 10 MiB + 40 KiB
        // of other keys away, so a cache of 4 MiB misses all of them, each its bytes
        let mut n = 0u64;
        for _ in 0..10 {
            for k in 0..20u64 {
                let sz = if k < 10 { 1 << 20 } else { 4 << 10 };
                t.reference(k, sz, n);
                n += 1;
            }
        }
        let missed = above(&t.cur, 4 << 20) + t.cur_cold;
        let want = 10.0 * (10.0 * (1u64 << 20) as f64 + 10.0 * (4u64 << 10) as f64);
        assert!((missed - want).abs() < want * 1e-9, "missed {missed} want {want}");
    }

    #[test]
    fn a_miss_reloads_what_one_use_read_back() {
        const MIB: u64 = 1 << 20;
        let mut t = Tracker::new(256).unwrap();
        t.seed_price(1);
        t.set_size(100 * MIB); // a new use: a reference at 10 MiB or more
        let (a, mut n) = (7u64, 0u64);
        let mut at = |t: &mut Tracker, k: u64, sz: u64| {
            t.reference(k, sz, n);
            n += 1;
        };
        // the first use: its door's lookup, then two block faults, each read through the door again (near references)
        at(&mut t, a, 4 * MIB);
        t.reload(a, MIB, 0);
        at(&mut t, a, 4 * MIB);
        t.reload(a, MIB, 0);
        let s = t.find(a).unwrap();
        assert_eq!((t.slots[s].reload, t.slots[s].reload_acc), (0, 2 * MIB), "one use so far, open");
        assert_eq!(t.reload_bytes(s), 4 * MIB, "no use closed: its size");
        // 20 MiB of other keys, then a's next use closes the first: a miss of a reloads its 2 MiB, not its 4
        for k in 100..120u64 {
            at(&mut t, k, MIB);
        }
        at(&mut t, a, 4 * MIB);
        assert_eq!((t.slots[s].reload, t.slots[s].reload_acc), (2 * MIB, 0));
        t.set_price_bytes(true);
        assert_eq!(t.price(s), (2 * MIB) as f64, "bytes mode: the bytes it reloads");
        t.set_price_bytes(false);
        t.set_ns_per_byte(0.5);
        assert_eq!(t.price(s), MIB as f64, "ns mode, no stall measured: its reload at 0.5 ns a byte");
        // a later use that reads back less leaves the largest use's
        t.reload(a, MIB / 2, 0);
        for k in 200..220u64 {
            at(&mut t, k, MIB);
        }
        at(&mut t, a, 4 * MIB);
        assert_eq!(t.slots[s].reload, 2 * MIB);
        // a key the sample does not hold: dropped
        t.reload(999, MIB, 0);
        assert!(t.find(999).is_none());
    }

    #[test]
    fn only_re_fills_price_the_cache() {
        let mut t = Tracker::new(64).unwrap();
        t.set_outcomes(true);
        t.seed_price(500);
        for k in 1..=8u64 {
            t.reference(k, 4096, k);
            t.fill(k, 4096, 10_000, k);
        }
        assert_eq!(t.price_mean, 500.0, "cold first fills (a build that also paid its inputs' cold reads) never price");
        t.reference(1, 4096, 20);
        t.fill(1, 4096, 2_100, 20);
        assert_eq!(t.price_mean, 500.0 + (2_100.0 - 500.0) / 32.0, "a re-fill feeds the mean");
        assert_eq!(t.price(t.find(1).unwrap()), 2_100.0, "and prices its key");
    }

    #[test]
    fn coupled_losses_raise_the_part_price() {
        let mut t = Tracker::new(64).unwrap();
        t.reference(1, 4096, 0);
        t.fill(1, 0, 70, 1);
        assert_eq!(t.price(t.find(1).unwrap()), 0.0, "a first fill never prices");
        t.reference(1, 4096, 1);
        t.fill(1, 0, 100, 1);
        assert_eq!(t.price(t.find(1).unwrap()), 100.0, "a re-fill does");
        t.couple(1, 50);
        assert_eq!(t.price(t.find(1).unwrap()), 150.0);
        t.fill(1, 0, 100, 2);
        t.couple(1, 10);
        assert_eq!(t.price(t.find(1).unwrap()), 150.0, "the last cycle's coupled loss stands until a bigger one");
    }

    #[test]
    fn rebuild_keeps_distances() {
        let mut t = Tracker::new(16).unwrap();
        t.seed_price(1);
        for i in 0..1000u64 {
            t.reference(i % 10, 1 << 12, i);
        }
        assert!(t.now as usize <= t.fw.len());
        let l = t.slots[t.find(9).unwrap()].last;
        assert_eq!(t.total - t.fw_prefix(l), 0);
        let l0 = t.slots[t.find(0).unwrap()].last;
        assert_eq!(t.total - t.fw_prefix(l0), 9 << 12);
    }

    #[test]
    fn pool_is_worth_its_peak() {
        let mut p = PoolCurve::new();
        for i in 0..600u64 {
            p.observe(i * 100_000_000, (i % 50) << 20);
            p.take(i * 100_000_000, 20_000);
        }
        let k = p.peak();
        assert_eq!(k, 49 << 20);
        let above = p.curve(256 << 20);
        assert_eq!(above.v, 0.0, "bytes above the peak are free");
        assert!((above.knee as f64 / (0.95 * k as f64) - 1.0).abs() < 0.01);
        let at = p.curve(k);
        let rate = 600.0 / 60.0;
        let want = rate * 20_000.0 / k as f64;
        assert!((at.v / want - 1.0).abs() < 0.02, "v {} want {want}", at.v);
    }
}
