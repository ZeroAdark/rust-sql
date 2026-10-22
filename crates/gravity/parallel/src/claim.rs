use std::sync::atomic::{AtomicI64, AtomicU64, AtomicU8, Ordering};
use pgsync::OnceLock;

use gravity_abi::MorselRange;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    Block,
    Granule,
    Partition,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimSize {
    /// Every claim `n` units (the tests, `examples/scale.rs`).
    Fixed(u64),
    /// The pool's sizer cuts the claims (the runtime's duration-adaptive
    /// sizer through `gravity_pool`, ramping from `startup_c0` units); a
    /// self-served cursor (`claim()`, the `FixturePool`) hands out
    /// `startup_c0` units per claim (§1.10 increment 3 item 1).
    Adaptive { startup_c0: u64 },
}

impl ClaimSize {
    /// Units per self-served claim.
    pub const fn units(self) -> u64 {
        match self {
            ClaimSize::Fixed(n) | ClaimSize::Adaptive { startup_c0: n } => n,
        }
    }

    /// The first claim size a duration-adaptive sizer ramps from; 1 for a fixed size (the size is not the sizer's).
    pub const fn startup_c0(self) -> u64 {
        match self {
            ClaimSize::Fixed(_) => 1,
            ClaimSize::Adaptive { startup_c0 } => startup_c0,
        }
    }
}

/// The startup claim a short statement is right-sized by (sqe's
/// `part_claim_chunks`: about four claims per worker from the geometry alone):
/// `clamp(total / (4·width), floor, bound)`; the runtime's DEFAULT and SHUTDOWN
/// phases still govern a long one (§1.10 increment 3 item 2).
pub const fn startup_c0(total: u64, width: u16, floor: u64, bound: u64) -> u64 {
    let w = if width == 0 { 1 } else { width as u64 };
    let c = total / (4 * w);
    let hi = if bound > floor { bound } else { floor };
    if c < floor {
        floor
    } else if c > hi {
        hi
    } else {
        c
    }
}

pub trait MorselSource: Sync {
    fn total(&self) -> u64;
    fn unit(&self) -> Unit;
    fn claim(&self) -> Option<MorselRange>;
    fn close(&self);
    fn remaining(&self) -> u64;
    fn reset(&self);
    /// D3014 S2': the source claimed again from its start for a rerun's next run: the cursor and any bound a run observed
    /// (a bound is one run's, never the next rescan's).
    fn rescan(&self) {
        self.reset()
    }
    /// The claim size a duration-adaptive pool ramps from (the runtime's STARTUP C0); 1 by default.
    fn startup_c0(&self) -> u64 {
        1
    }
    /// The first hard boundary after `unit` (R4: a claim never crosses one); `total()` when none.
    fn next_boundary_after(&self, _unit: u64) -> u64 {
        self.total()
    }
    /// The first unit at or after `unit` a claim may cover (`unit` itself when live).
    fn first_live_at(&self, unit: u64) -> u64 {
        unit
    }
    /// Every claim runs to its next boundary (the runtime's dict-epoch posture);
    /// sources whose per-boundary state is cheap leave it off and get sizer-cut claims.
    fn whole_boundary_claims(&self) -> bool {
        false
    }
    /// The first unit whole-boundary claims stop at: a claim from it on is cut inside its boundary (the runtime's sizer,
    /// or `startup_c0` units self-served); `u64::MAX` when every claim is whole.
    fn whole_until(&self) -> u64 {
        u64::MAX
    }
    /// Whether the runtime's end-game split may cut a whole claim inside its boundary (width > 32); false for a source whose
    /// claims each fill one boundary's state alone (one writer per part entry).
    fn endgame_split(&self) -> bool {
        true
    }
    /// skewsplit (D3320): whether a running claim of this source may give its remaining units to an idle worker (the pool's
    /// split board, `gravity_driver::claimcut`); false by default and for every source whose claims each fill one boundary's
    /// state alone or answer at part grain.
    fn split_running(&self) -> bool {
        false
    }
    /// A worker's partial proved `bound` (the zone walk): units whose key minimum lies past it are not handed out.
    fn observe_bound(&self, _bound: i64) {}
    /// Whether unit `unit` lies past the observed bound (its claim is skipped and the cursor closed).
    fn beyond_bound(&self, _unit: u64) -> bool {
        false
    }
}

pub struct RangeCursor {
    /// Wave 2: the vectors' heap bytes on the owner `charge_to` names (the statement's: a cursor is cut before the Node
    /// binds), re-synced when they grow; unset: uncharged.
    charge: OnceLock<memledger::Charge>,
    next: AtomicU64,
    total: AtomicU64,
    size: ClaimSize,
    bounds: Vec<u64>,
    /// Unit ranges never handed out (ascending, disjoint): the parts the zone census pruned whole.
    dead: Vec<(u64, u64)>,
    whole: bool,
    /// `MorselSource::whole_until` (`u64::MAX`: none) and `endgame_split`.
    tail_from: u64,
    endgame: bool,
    /// `MorselSource::split_running` (skewsplit).
    split: bool,
    /// The ordered walk (step 5): unit `i` is granule `order[i]` with key minimum `mins[i]`; `bound` the workers' proven bound.
    order: Vec<u32>,
    mins: Vec<i64>,
    bound: AtomicI64,
    /// An adaptive size re-aimed once the units are known (`set_total_aimed`); 0: the size's own.
    aimed: AtomicU64,
}

/// The release a charge of `o` states (read back only): a portal's own end, else its statement's.
fn release_of(o: memledger::Owner) -> memledger::Release {
    match o {
        memledger::Owner::Portal { .. } => memledger::Release::Portal,
        _ => memledger::Release::Statement,
    }
}

impl RangeCursor {
    pub const fn new(total: u64, size: ClaimSize) -> RangeCursor {
        assert!(size.units() > 0, "claim size");
        RangeCursor { next: AtomicU64::new(0), total: AtomicU64::new(total), size, bounds: Vec::new(), dead: Vec::new(), whole: false, tail_from: u64::MAX, endgame: true, split: false, order: Vec::new(), mins: Vec::new(), bound: AtomicI64::new(i64::MAX), aimed: AtomicU64::new(0), charge: OnceLock::new() } // alloc: Program
    }

    /// An explicit ordered walk (`mins` ascending); boundaries where consecutive granules change part (R4).
    pub fn ordered(order: &[u32], mins: &[i64], part_of: impl Fn(u32) -> usize, size: ClaimSize) -> RangeCursor {
        assert_eq!(order.len(), mins.len(), "walk order");
        let mut c = RangeCursor::new(order.len() as u64, size);
        c.order.extend_from_slice(order); // alloc: Exec charged charge_to's owner
        c.mins.extend_from_slice(mins); // alloc: Exec charged charge_to's owner
        for i in 1..order.len() {
            if part_of(order[i]) != part_of(order[i - 1]) {
                c.bounds.push(i as u64); // alloc: Exec charged charge_to's owner
            }
        }
        c
    }

    /// Wave 2: the cursor's vectors (one word or two per unit of an ordered walk, one per part edge and dead range) charged
    /// to `owner` from now to the cursor's drop, on the thread that cut them (the runner's leader, once its Node is bound).
    /// A second call keeps the first owner; `None`: uncharged.
    pub fn charge_to(&self, owner: Option<memledger::Owner>) {
        if let (Some(o), None) = (owner, self.charge.get()) {
            let _ = self.charge.set(memledger::Charge::open(o, release_of(o), memledger::Kind::Region, memledger::charge::Door::Heap, self.heap_bytes()));
        }
    }

    fn heap_bytes(&self) -> u64 {
        (self.bounds.capacity() * 8 + self.dead.capacity() * 16 + self.order.capacity() * 4 + self.mins.capacity() * 8) as u64
    }

    /// The charge (when one is open) re-synced to the vectors' capacity after a growth on this thread.
    fn recharge(&mut self) {
        let now = self.heap_bytes();
        if let Some(c) = self.charge.get_mut() {
            let was = c.committed();
            if now > was {
                c.commit(now - was);
            } else if now < was {
                c.free(was - now);
            }
        }
    }

    /// The walk's granule behind unit `i` (`i` itself without an order).
    pub fn granule_of(&self, i: u64) -> u64 {
        self.order.get(i as usize).map_or(i, |&g| g as u64)
    }

    pub fn order(&self) -> &[u32] {
        &self.order
    }

    pub fn observe_bound(&self, bound: i64) {
        self.bound.fetch_min(bound, Ordering::AcqRel);
    }

    pub fn beyond_bound(&self, unit: u64) -> bool {
        self.mins.get(unit as usize).is_some_and(|&m| m > self.bound.load(Ordering::Acquire))
    }

    /// The bound observed so far (`i64::MAX` without one).
    pub fn bound(&self) -> i64 {
        self.bound.load(Ordering::Acquire)
    }

    /// Every claim runs to its next boundary (a dictionary code lane is in the window).
    pub fn set_whole_boundary_claims(&mut self, whole: bool) {
        self.whole = whole;
    }

    pub const fn whole_boundary_claims(&self) -> bool {
        self.whole
    }

    /// Whole-boundary claims stop at unit `unit` (a boundary): the claims from it on are cut inside their boundary.
    pub fn set_tail_from(&mut self, unit: u64) {
        self.tail_from = unit;
    }

    pub const fn whole_until(&self) -> u64 {
        self.tail_from
    }

    /// Whether the runtime's end-game split may cut a whole claim (false: each claim fills its boundary's state alone).
    pub fn set_endgame_split(&mut self, on: bool) {
        self.endgame = on;
    }

    pub const fn endgame_split(&self) -> bool {
        self.endgame
    }

    /// skewsplit (D3320): a running claim may give its remaining units to an idle worker (`MorselSource::split_running`).
    pub fn set_split_running(&mut self, on: bool) {
        self.split = on;
    }

    pub const fn split_running(&self) -> bool {
        self.split
    }

    /// `bounds`: ascending unit indexes a claim never crosses (part edges), all below `total`.
    pub fn with_boundaries(total: u64, size: ClaimSize, bounds: &[u64]) -> RangeCursor {
        let mut c = RangeCursor::new(total, size);
        c.reset_to(total, bounds);
        c
    }

    /// Re-aim the cursor at a new geometry (the next execution's part list);
    /// the boundary vector keeps its capacity, so a steady state allocates nothing.
    pub fn reset_to(&mut self, total: u64, bounds: &[u64]) {
        self.total.store(total, Ordering::Release);
        self.bounds.clear();
        self.bounds.extend_from_slice(bounds); // alloc: Exec charged charge_to's owner
        self.dead.clear();
        self.order.clear();
        self.mins.clear();
        self.bound.store(i64::MAX, Ordering::Release);
        self.tail_from = u64::MAX;
        self.split = false;
        self.next.store(0, Ordering::Release);
        self.recharge();
    }

    pub fn set_dead(&mut self, dead: &[(u64, u64)]) {
        self.dead.clear();
        self.dead.extend_from_slice(dead); // alloc: Exec charged charge_to's owner
        self.recharge();
    }

    pub fn first_live_at(&self, unit: u64) -> u64 {
        self.dead_end_of(unit).unwrap_or(unit)
    }

    fn dead_end_of(&self, lo: u64) -> Option<u64> {
        let i = self.dead.partition_point(|&(s, _)| s <= lo).checked_sub(1)?;
        let (_, e) = self.dead[i];
        (lo < e).then_some(e)
    }

    pub fn next_boundary_after(&self, unit: u64) -> u64 {
        let total = self.total();
        let i = self.bounds.partition_point(|&b| b <= unit);
        match self.bounds.get(i) {
            Some(&b) => b.min(total),
            None => total,
        }
    }

    /// Re-aim at `total` units (the spill partition count known only after the producer ran).
    pub fn set_total(&self, total: u64) {
        self.total.store(total, Ordering::Release);
        self.next.store(0, Ordering::Release);
    }

    /// `set_total` with an adaptive size's startup claim re-aimed at `c0` (at least 1): a bitmap's units are known only
    /// once it is built, and a startup claim sized from the plan's estimate in rows can take most of them in one claim.
    pub fn set_total_aimed(&self, total: u64, c0: u64) {
        if matches!(self.size, ClaimSize::Adaptive { .. }) {
            self.aimed.store(c0.max(1), Ordering::Release);
        }
        self.set_total(total);
    }

    fn units(&self) -> u64 {
        match self.aimed.load(Ordering::Acquire) {
            0 => self.size.units(),
            a => a,
        }
    }

    pub fn claim(&self) -> Option<MorselRange> {
        let n = self.units();
        let total = self.total();
        loop {
            let lo = self.next.load(Ordering::Acquire);
            if lo >= total {
                return None;
            }
            if self.beyond_bound(lo) {
                self.close();
                return None;
            }
            if let Some(e) = self.dead_end_of(lo) {
                let _ = self.next.compare_exchange_weak(lo, e.min(total), Ordering::AcqRel, Ordering::Acquire);
                continue;
            }
            let bound = self.next_boundary_after(lo);
            let hi = if self.whole && lo < self.tail_from { bound } else { (lo + n).min(bound) };
            if self.next.compare_exchange_weak(lo, hi, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                return Some(MorselRange::new(lo, hi));
            }
        }
    }

    pub fn close(&self) {
        self.next.fetch_max(self.total(), Ordering::AcqRel);
    }

    pub fn remaining(&self) -> u64 {
        self.total().saturating_sub(self.next.load(Ordering::Acquire))
    }

    pub fn reset(&self) {
        self.next.store(0, Ordering::Release);
    }

    /// D3014 S2': `MorselSource::rescan`.
    pub fn rescan(&self) {
        self.bound.store(i64::MAX, Ordering::Release);
        self.next.store(0, Ordering::Release);
    }

    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Acquire)
    }

    pub fn startup_c0(&self) -> u64 {
        match self.size {
            ClaimSize::Fixed(_) => 1,
            ClaimSize::Adaptive { .. } => self.units(),
        }
    }
}

pub struct BlockRange(pub RangeCursor);

/// Phase 2's claims: the `P` partitions of a partitioned merge, `per_claim` at a time.
pub struct PartitionRange(pub RangeCursor);

impl PartitionRange {
    pub const fn new(partitions: u64, per_claim: u64) -> PartitionRange {
        PartitionRange(RangeCursor::new(partitions, ClaimSize::Fixed(if per_claim == 0 { 1 } else { per_claim })))
    }

    pub const fn for_width(partitions: u64, width: u16) -> PartitionRange {
        let w = if width == 0 { 1 } else { width as u64 };
        PartitionRange::new(partitions, partitions.div_ceil(w))
    }

    /// The partitioned join's third phase: sized at 0 in the plan, re-aimed at the build stream's
    /// partition count once the build ran (`set_total`); one partition per claim.
    pub const fn deferred() -> PartitionRange {
        PartitionRange::new(0, 1)
    }

    pub fn set_total(&self, partitions: u64) {
        self.0.set_total(partitions)
    }
}

pub struct GranuleRange(pub RangeCursor);

impl BlockRange {
    pub const fn new(blocks: u64, size: ClaimSize) -> BlockRange {
        BlockRange(RangeCursor::new(blocks, size))
    }
}

impl GranuleRange {
    pub const fn new(granules: u64, size: ClaimSize) -> GranuleRange {
        GranuleRange(RangeCursor::new(granules, size))
    }

    /// Granules of several parts: `part_starts[i]` is part i's first granule; a claim stays inside one part.
    pub fn over_parts(granules: u64, size: ClaimSize, part_starts: &[u64]) -> GranuleRange {
        GranuleRange(RangeCursor::with_boundaries(granules, size, part_starts))
    }

    pub fn reset_to(&mut self, granules: u64, part_starts: &[u64]) {
        self.0.reset_to(granules, part_starts)
    }

    pub fn set_whole_boundary_claims(&mut self, whole: bool) {
        self.0.set_whole_boundary_claims(whole)
    }

    pub fn set_tail_from(&mut self, unit: u64) {
        self.0.set_tail_from(unit)
    }

    pub fn set_endgame_split(&mut self, on: bool) {
        self.0.set_endgame_split(on)
    }

    pub fn set_split_running(&mut self, on: bool) {
        self.0.set_split_running(on)
    }

    pub fn set_dead(&mut self, dead: &[(u64, u64)]) {
        self.0.set_dead(dead)
    }

    /// The zone walk's claims: granules in `order` (key minima `mins` ascending), one part per claim run.
    pub fn ordered(order: &[u32], mins: &[i64], part_of: impl Fn(u32) -> usize, size: ClaimSize) -> GranuleRange {
        GranuleRange(RangeCursor::ordered(order, mins, part_of, size))
    }

    pub fn cursor(&self) -> &RangeCursor {
        &self.0
    }
}

macro_rules! source_over_cursor {
    ($t:ty, $unit:expr) => {
        impl MorselSource for $t {
            fn total(&self) -> u64 {
                self.0.total()
            }
            fn unit(&self) -> Unit {
                $unit
            }
            fn claim(&self) -> Option<MorselRange> {
                self.0.claim()
            }
            fn close(&self) {
                self.0.close()
            }
            fn remaining(&self) -> u64 {
                self.0.remaining()
            }
            fn reset(&self) {
                self.0.reset()
            }
            fn rescan(&self) {
                self.0.rescan()
            }
            fn startup_c0(&self) -> u64 {
                self.0.startup_c0()
            }
            fn next_boundary_after(&self, unit: u64) -> u64 {
                self.0.next_boundary_after(unit)
            }
            fn first_live_at(&self, unit: u64) -> u64 {
                self.0.first_live_at(unit)
            }
            fn whole_boundary_claims(&self) -> bool {
                self.0.whole_boundary_claims()
            }
            fn whole_until(&self) -> u64 {
                self.0.whole_until()
            }
            fn endgame_split(&self) -> bool {
                self.0.endgame_split()
            }
            fn split_running(&self) -> bool {
                self.0.split_running()
            }
            fn observe_bound(&self, bound: i64) {
                self.0.observe_bound(bound)
            }
            fn beyond_bound(&self, unit: u64) -> bool {
                self.0.beyond_bound(unit)
            }
        }
    };
}

source_over_cursor!(BlockRange, Unit::Block);
source_over_cursor!(PartitionRange, Unit::Partition);
source_over_cursor!(GranuleRange, Unit::Granule);

pub struct ClaimLedger {
    counts: Box<[AtomicU8]>,
}

impl ClaimLedger {
    pub fn new(units: u64) -> ClaimLedger {
        let mut v = Vec::with_capacity(units as usize); // alloc: Cold
        v.resize_with(units as usize, || AtomicU8::new(0));
        ClaimLedger { counts: v.into_boxed_slice() }
    }

    pub fn record(&self, range: MorselRange) {
        for u in range.lo..range.hi.min(self.counts.len() as u64) {
            self.counts[u as usize].fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn consumed(&self) -> u64 {
        self.counts.iter().filter(|c| c.load(Ordering::Relaxed) > 0).count() as u64
    }

    pub fn consumed_once(&self) -> bool {
        self.counts.iter().all(|c| c.load(Ordering::Relaxed) == 1)
    }

    pub fn twice(&self) -> u64 {
        self.counts.iter().filter(|c| c.load(Ordering::Relaxed) > 1).count() as u64
    }

    pub fn clear(&self) {
        for c in self.counts.iter() {
            c.store(0, Ordering::Relaxed);
        }
    }

    pub fn len(&self) -> usize {
        self.counts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.counts.is_empty()
    }
}
