#![allow(unsafe_code)]
use core::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use pg_clock::MonoStamp;

/// The execution's breaker memory (§2 decision 2, `breakers.md`): `work_mem × hash_mem_multiplier`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grant {
    pub bytes: u64,
    /// D2587: PostgreSQL's own per-node budget, the least each participant's share of a
    /// breaker is (`floor_of`): `sort_floor` work_mem (a Sort, top-N or Material), `hash_floor` work_mem ×
    /// hash_mem_multiplier (a hash aggregate, hash build or hashed DISTINCT). One node's budget, not the width's: PostgreSQL
    /// gives a serial node one and each parallel participant one, so the leader's share and each worker's own share are
    /// floored at it (`split_at_least`). The split across the live breakers only adds above them. 0 = none (`of`,
    /// `from_gucs`: a fixed grant, the pool's trimmed grants under pressure).
    pub sort_floor: u64,
    pub hash_floor: u64,
    /// The breakers' ceilings: PostgreSQL's per-node budgets as the most one participant's region of a breaker holds
    /// before its spill road runs (`ceil_of`, `ceiling_of`): `sort_ceil` work_mem (a Sort, top-N or Material), `hash_ceil`
    /// work_mem x hash_mem_multiplier (a hash aggregate, hash build or hashed DISTINCT). As PostgreSQL's work_mem, a hard
    /// bound per node and participant: past it the breaker spills even with the pool free; under it only a pool with no
    /// room spills it. A trim keeps them (the floors it drops). 0 = none (`of`: today's roads, W then G).
    pub sort_ceil: u64,
    pub hash_ceil: u64,
    /// exactsize: `pgrust.gravity_exact_size` at the admission, a bit set (`EXACT_BUILD`, `EXACT_FINAL`): what the statement
    /// sizes from its count once the count is known. 0 = none (`of`): today's sizes, byte for byte. A trim keeps it.
    pub exact: u8,
    /// D3798 S2: how the statement's hash breakers start (`decide_start`), decided once at its admission and read by its
    /// admission's footprint, its shares and every region it opens (leader and workers), so they agree. `of`: small, as
    /// before S2. A trim keeps it.
    pub start: StartSize,
    /// The forced-spill test fault (`pgrust.gravity_test_spill_every`, read with the grant knobs): every Nth room ask of
    /// a breaker region's spill-capable roads is refused, so its breaker takes the road its ceiling takes there
    /// (`grow::Fault`); 0 = off (`of`). A test lever, never product surface. A trim keeps it.
    pub spill_every: u32,
    /// C5's revoke test lever (`pgrust.gravity_test_revoke_every`, read with the grant knobs): every Nth room ask of a
    /// breaker region's spill-capable roads asks a revoke of its own statement (`grow::Fault`), so the revoke road runs
    /// inside one statement; 0 = off (`of`). A test lever, never product surface. A trim keeps it.
    pub revoke_every: u32,
    /// D4078: the one-owner wall (`mempolicy::admit::owner_wall`: half the pool's armed big lane, at most the format's
    /// `section::OWNER_WALL`), read once at the admission (`owner_wall_for`) and carried here, so its footprint's owner pins
    /// and its owner phases' split (`section::own_all`) read one value: a re-arm of the pool mid-statement moves neither.
    /// 0 = the format's wall (`of`: a grant no admission sized). A trim keeps it.
    pub owner_wall: u64,
    /// NR-6's test lever (`pgrust.gravity_test_refuse_debt`, read with the grant knobs): a breaker region's draw past its
    /// cap on a road a SPILL-DEBT row names is refused (`grow::Fault`), as the pool refuses one; false = off (`of`). A
    /// test lever, never product surface. A trim keeps it.
    pub refuse_debt: bool,
    /// D4043: the pool's trim (`trimmed`) with no floors given back (`with_floors` clears it): its breakers' shares are its
    /// own bytes' split, no learned start raises them (`share_of`). false: `of`, and an estimate's grant with its floors.
    pub trimmed: bool,
    /// Grow mode (`pgrust.gravity_grow`, read once at the admission): the grant is the statement's estimate
    /// start (`mempolicy::grow::grow_want`: no grant floor), every hash breaker starts small
    /// (`mempolicy::admit::decide_start_in`), the floors carry the PostgreSQL per-node budgets as CAPS of each share, never
    /// raised from feedback (`grow_share`), and with work_mem unset the ceilings are the region wall
    /// (`mempolicy::grow::grow_ceiling`), so a breaker grows on demand past its start and spills where a grow is refused.
    /// false = off (`of`): today's grants, byte for byte. A trim keeps it (a trim drops the floors: its shares are its own
    /// bytes' split, held to the ceilings).
    pub grow: bool,
}

/// D3798 S2: a statement's start and its rule live in the memory policy (`mempolicy::admit`); the driver carries the
/// answer in the statement's `Grant`.
pub use mempolicy::admit::{decide_start, StartSize};

/// exactsize B1: a hash build's ask is its records (`cost::breaker::of_at`), its leader region grown to the counted need at
/// the build's end (`State::grow_exact`).
pub const EXACT_BUILD: u8 = 1;
/// exactsize B2 / B4: the owner side sized from its count; D3205: a serial pipeline's breaker sized by the planner's rows at
/// its end, not 0 (`Grant::exact_rows`).
pub const EXACT_FINAL: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Share {
    pub bytes: u32,
}

/// The leader region's share, a worker's, an owner's cut (the leader region behind its `Owned` header over `width`), and `images`:
/// the per-owner text-image cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Split {
    pub breaker: Share,
    pub worker: Share,
    pub owner: Share,
    pub images: Share,
}

pub use gravity_program::cost::runtime::{FLOOR_CHARGED_MIN, GRANT_FLOOR, IMAGES_MIN, SHARE_FLOOR};
use gravity_program::cost::runtime::LEADER_CAP;

/// An owner's part of the leader region `breaker`: behind the 64-byte `Owned` header, cut `width` ways (`Open::owner_region`).
pub fn owner_cut(breaker: Share, width: u16) -> Share {
    Share { bytes: (breaker.bytes.saturating_sub(gravity_abi::breaker::OWNED_HDR as u32) / width.max(1) as u32) & !7 }
}

pub fn image_cut(share: u32) -> u32 {
    let room = share.saturating_sub(SHARE_FLOOR);
    (share / 8).max(IMAGES_MIN).min(room) & !7
}

impl Grant {
    pub const fn of(bytes: u64) -> Grant {
        Grant { bytes, sort_floor: 0, hash_floor: 0, sort_ceil: 0, hash_ceil: 0, exact: 0, start: StartSize::Small, spill_every: 0, revoke_every: 0, owner_wall: 0, refuse_debt: false, trimmed: false, grow: false }
    }

    /// `self` in grow mode (`grow`) or not.
    pub const fn with_grow(self, grow: bool) -> Grant {
        Grant { grow, ..self }
    }

    /// D4078: `self` with its one-owner wall at `owner_wall` bytes (`owner_wall_for`; 0 = the format's).
    pub const fn with_owner_wall(self, owner_wall: u64) -> Grant {
        Grant { owner_wall, ..self }
    }

    /// D4078: the one-owner wall this grant's statement lays its owners under: its admission's (`owner_wall`), else the
    /// format's (`section::OWNER_WALL`).
    pub const fn owner_wall_bytes(&self) -> u64 {
        let format = crate::section::OWNER_WALL as u64;
        if self.owner_wall == 0 || self.owner_wall > format {
            format
        } else {
            self.owner_wall
        }
    }

    /// D3798 S2: `self` with its breakers starting at `start` (`decide_start`).
    pub const fn with_start(self, start: StartSize) -> Grant {
        Grant { start, ..self }
    }

    /// `self` sizing from counts per `exact` (`EXACT_BUILD` | `EXACT_FINAL`, `pgrust.gravity_exact_size`).
    pub const fn with_exact(self, exact: u8) -> Grant {
        Grant { exact, ..self }
    }

    /// `self` with the forced-spill test fault at every `n`th room ask (`spill_every`; 0 = off).
    pub const fn with_spill_every(self, spill_every: u32) -> Grant {
        Grant { spill_every, ..self }
    }

    /// `self` with C5's revoke test lever at every `n`th room ask (`revoke_every`; 0 = off).
    pub const fn with_revoke_every(self, revoke_every: u32) -> Grant {
        Grant { revoke_every, ..self }
    }

    /// `self` with NR-6's test lever (`refuse_debt`; false = off).
    pub const fn with_refuse_debt(self, refuse_debt: bool) -> Grant {
        Grant { refuse_debt, ..self }
    }

    /// The pool's trim of `self` to `bytes`: no floors (as `of`), its ceilings, exact sizing, start, owner wall, test
    /// fault and grow mode kept; marked `trimmed` (D4043: its shares are its own bytes' split).
    pub const fn trimmed(self, bytes: u64) -> Grant {
        Grant { exact: self.exact, sort_ceil: self.sort_ceil, hash_ceil: self.hash_ceil, start: self.start, spill_every: self.spill_every, revoke_every: self.revoke_every, owner_wall: self.owner_wall, refuse_debt: self.refuse_debt, trimmed: true, grow: self.grow, ..Grant::of(bytes) }
    }

    /// exactsize B1 (`EXACT_BUILD`).
    pub const fn exact_build(&self) -> bool {
        self.exact & EXACT_BUILD != 0
    }

    /// D3205 (`EXACT_FINAL`): a serial pipeline's breaker asks by the planner's rows at its end (`cost::breaker::sized_rows`).
    pub const fn exact_rows(&self) -> bool {
        self.exact & EXACT_FINAL != 0
    }

    pub fn from_gucs(work_mem_kb: i64, hash_mem_multiplier: f64) -> Grant {
        let kb = work_mem_kb.max(64) as f64;
        Grant::of((kb * 1024.0 * hash_mem_multiplier.max(1.0)) as u64)
    }

    /// `self` with PostgreSQL's per-node budgets as its breakers' floors (D2587); a trim with its floors given back (an
    /// estimate's grant, exec `ask`) is no longer `trimmed`: its learned starts are capped at those budgets.
    pub const fn with_floors(self, sort_floor: u64, hash_floor: u64) -> Grant {
        Grant { sort_floor, hash_floor, trimmed: false, ..self }
    }

    /// `self` with PostgreSQL's per-node budgets as its breakers' ceilings (`sort_ceil`, `hash_ceil`).
    pub const fn with_ceilings(self, sort_ceil: u64, hash_ceil: u64) -> Grant {
        Grant { sort_ceil, hash_ceil, ..self }
    }

    /// The most one participant's region of a breaker of `kind` holds before it spills: PostgreSQL's budget for its node
    /// (work_mem, x hash_mem_multiplier for a hash node); a plain aggregate's, or a grant's without ceilings, none (0).
    pub const fn ceil_of(&self, kind: gravity_abi::breaker::BreakerKind) -> u64 {
        use gravity_abi::breaker::BreakerKind::*;
        match kind {
            HashAgg | HashBuild | Distinct => self.hash_ceil,
            Sort | TopK | Materialize => self.sort_ceil,
            PlainAgg => 0,
        }
    }

    /// The least share a breaker of `kind` gets (D2587): PostgreSQL's budget for its node, a plain aggregate's none.
    pub const fn floor_of(&self, kind: gravity_abi::breaker::BreakerKind) -> u64 {
        use gravity_abi::breaker::BreakerKind::*;
        match kind {
            HashAgg | HashBuild | Distinct => self.hash_floor,
            Sort | TopK | Materialize => self.sort_floor,
            PlainAgg => 0,
        }
    }

    /// Across the breakers live at once (`SlotPlan::max_live`), then across the width; floored and capped, a multiple of 8
    /// like the owner and image cuts: a table keeps its images down from its region's end, 8-aligned only if that end is (D335).
    /// D651: past 8 GiB a breaker's share (the leader region at its u32 cap), the W workers split what the leader leaves;
    /// dividing the capped region held them to 4 GiB / W whatever the grant. At 8 GiB and under the split is as it was.
    pub fn split(&self, live: u16, width: u16) -> Split {
        self.split_at_least(live, width, 0)
    }

    /// `split` with each participant's part at least `floor` (D2587: its kind's `floor_of`, one node's budget): the
    /// breaker's (leader) share, then the width's cut, each worker's own share floored again. A floor of the width's
    /// budgets on the whole breaker (W x a node's) gave every breaker, a serial one too, a W x budget leader region: a
    /// serial 8-row hash build mapped 64 MiB at W8 (8 MiB at W0), and a W8 pool admitted half as many statements.
    pub fn split_at_least(&self, live: u16, width: u16, floor: u64) -> Split {
        self.split_at_least_w(live, width, floor, floor)
    }

    /// `split_at_least` with each worker's own share floored at `worker_floor` (exactsize D3215, `worker_floor`), the
    /// leader's at `floor`.
    pub fn split_at_least_w(&self, live: u16, width: u16, floor: u64, worker_floor: u64) -> Split {
        let per = |bytes: u64, n: u64| -> Share { Share { bytes: (bytes / n.max(1)).clamp(SHARE_FLOOR as u64, u32::MAX as u64) as u32 & !7 } };
        let t = (self.bytes / live.max(1) as u64).max(floor);
        let breaker = per(t, 1);
        let cut = if width >= 2 && t > 2 * LEADER_CAP { per(t - LEADER_CAP, width as u64) } else { per(breaker.bytes as u64, width as u64) };
        // never past the leader's share (t >= floor): an owner cut or an image cut taken from it stays inside the breaker
        let worker = Share { bytes: cut.bytes.max(per(worker_floor, 1).bytes.min(breaker.bytes)) };
        Split { breaker, worker, owner: owner_cut(breaker, width), images: Share { bytes: 0 } }
    }

    /// `split` with the image cut taken when the program pins group images: one cut per owner, sized from the worker share.
    pub fn split_for(&self, live: u16, width: u16, images: bool) -> Split {
        self.split_for_at_least(live, width, images, 0)
    }

    /// `split_for` over `split_at_least`.
    pub fn split_for_at_least(&self, live: u16, width: u16, images: bool, floor: u64) -> Split {
        self.split_within(live, width, images, floor, floor, u64::MAX)
    }

    /// `split_for_at_least` over `split_at_least_w`.
    pub fn split_for_at_least_w(&self, live: u16, width: u16, images: bool, floor: u64, worker_floor: u64) -> Split {
        self.split_within(live, width, images, floor, worker_floor, u64::MAX)
    }

    /// `split_for_at_least_w` with the leader's share and each worker's at most `cap` (a row top's bounded region,
    /// `top_share_cap`; `u64::MAX` = none) and the floors held to it, so no floor raises a share past it; an image cut
    /// comes out of the capped share.
    pub fn split_within(&self, live: u16, width: u16, images: bool, floor: u64, worker_floor: u64, cap: u64) -> Split {
        // at width >= 2 the leader merges every participant's run into its own region: its cap is theirs together, so a
        // bounded top's merge never grows past G where the floor-sized share held it
        let lead = if width >= 2 { cap.saturating_mul(width as u64 + 1) } else { cap };
        self.split_capped(live, width, images, floor, worker_floor, cap, lead)
    }

    /// `split_within` with the leader's share at most `lead` (a breaker's leader ceiling, `leader_ceiling`).
    pub fn split_capped(&self, live: u16, width: u16, images: bool, floor: u64, worker_floor: u64, cap: u64, lead: u64) -> Split {
        let s = self.split_at_least_w(live, width, floor.min(cap), worker_floor.min(cap));
        let held = |sh: Share, cap: u64| Share { bytes: (sh.bytes as u64).min(cap) as u32 & !7 };
        let s = Split { breaker: held(s.breaker, lead), worker: held(s.worker, cap), owner: owner_cut(held(s.breaker, lead), width), images: s.images };
        if !images {
            return s;
        }
        let cut = image_cut(s.worker.bytes);
        let breaker = Share { bytes: s.breaker.bytes - cut };
        Split { breaker, worker: Share { bytes: s.worker.bytes - cut }, owner: owner_cut(breaker, width), images: Share { bytes: cut } }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Accounting {
    pub opens: u64,
    pub frees: u64,
    pub armed: u64,
    pub largest_share: u32,
    pub grown_bytes: u64,
    pub spills: u64,
    /// Sections whose partials folded partition-parallel (phase 2), and phase-2 refusals (serial
    /// fold) by step: the plan (the destination), phase 2a (a partial), phase 2b (a partition).
    pub merged: u64,
    pub p2_refused: u64,
    pub p2_refused_at: [u64; 3],
    /// The last phase-2b refusal: `[part, destination cap, destination pbits, destination len]`.
    pub p2_fail: [u32; 4],
    /// The peak of the live region limits: what `footprint_regions` reserves (S2).
    pub limits: u64,
    /// D254: regions reopened past their share at run time (`region_grow`, `State::grow`), and the bytes grown past the shares.
    pub past_share: u64,
    pub past_share_bytes: u64,
}

#[derive(Default)]
pub struct Counters {
    pub(crate) opens: Cell<u64>,
    pub(crate) frees: Cell<u64>,
    pub(crate) armed: Cell<u64>,
    pub(crate) largest_share: Cell<u32>,
    pub(crate) grown_bytes: Cell<u64>,
    pub(crate) spills: Cell<u64>,
    pub(crate) merged: Cell<u64>,
    pub(crate) p2_refused: Cell<u64>,
    pub(crate) p2_refused_at: [Cell<u64>; 3],
    pub(crate) p2_fail: Cell<[u32; 4]>,
    pub(crate) limits: Cell<u64>,
    live: Cell<u64>,
    pub(crate) past_share: Cell<u64>,
    pub(crate) past_share_bytes: Cell<u64>,
    /// The statement logged its first grow past a share (reset at its end, `settle_limits`).
    pub(crate) past_share_logged: Cell<bool>,
}

impl Counters {
    pub fn snapshot(&self) -> Accounting {
        Accounting { opens: self.opens.get(), frees: self.frees.get(), armed: self.armed.get(), largest_share: self.largest_share.get(), grown_bytes: self.grown_bytes.get(), spills: self.spills.get(), merged: self.merged.get(), p2_refused: self.p2_refused.get(), p2_refused_at: [self.p2_refused_at[0].get(), self.p2_refused_at[1].get(), self.p2_refused_at[2].get()], p2_fail: self.p2_fail.get(), limits: self.limits.get(), past_share: self.past_share.get(), past_share_bytes: self.past_share_bytes.get() }
    }

    /// A limit opened at `add` in place of one of `sub` (0 = fresh; `add` 0 = freed): `limits` keeps the peak of the live sum.
    pub(crate) fn limit(&self, add: u64, sub: u64) {
        self.live.set(self.live.get() + add - sub);
        self.limits.set(self.limits.get().max(self.live.get()));
    }

    pub(crate) fn settle_limits(&self) {
        self.live.set(0);
        self.past_share_logged.set(false);
    }

    pub(crate) fn armed(&self, share: u32) {
        self.armed.set(self.armed.get() + 1);
        self.largest_share.set(self.largest_share.get().max(share));
    }
}

use core::sync::atomic::{AtomicU64, Ordering};

// The admission policy (its constants, D3214's stall clock, D3127's settle, the floor's road) lives in `mempolicy::admit`.
pub use mempolicy::admit::{Admitted, StallBounds, BYTES_WAIT, GOVERNOR_SETTLE, QUEUE_STALL, SETTLE_FALL, SETTLE_FLAT_POLLS, STALL_PROGRESS, TURN_CAP};
use mempolicy::admit::{self as policy, settle_on_track, StallClock};

/// A site that takes pool bytes past the capacity, never refused and never waiting (D1378 and D622 share one code path:
/// `GrantPool::overdraw_take`): what PostgreSQL holds there has no spill road and no refusal, so a refusal would be a 53200
/// PostgreSQL never raises.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overdraw {
    /// D622: a nested statement's admission past the pool (`GrantPool::admit_overdrawn`).
    Nested = 0,
    /// D1378: a recursive UNION's seen set's doubling (exec setop/generation.rs `Seen`; `GrantPool::overdraw`).
    Seen = 1,
    /// D4329: a verbatim part's dictionary build's working memory past the pool (driver `scratch_draw`;
    /// `GrantPool::overdraw_pool`).
    Scratch = 2,
}

impl Overdraw {
    pub const N: usize = 3;
    pub const ALL: [Overdraw; Overdraw::N] = [Overdraw::Nested, Overdraw::Seen, Overdraw::Scratch];

    /// Its grant report field (`<site>_past=<n>/<bytes>`).
    pub const fn name(self) -> &'static str {
        match self {
            Overdraw::Nested => "nested_past",
            Overdraw::Seen => "seen_past",
            Overdraw::Scratch => "scratch_past",
        }
    }
}

/// The process-wide grant pool (breakers.md "The grant pool"): every breaker region's bytes, live or parked, plus the statements'
/// reservations, against the capacity the runner arms (0 = unarmed). `admit` fits a grant's regions beside what is held or waits,
/// then shrinks to the floor; its `Reservation` is drawn down by the statement's allocations and gives the rest back at drop
/// (D188); a refused allocation is `Error::OutOfMemory`.
pub struct GrantPool {
    capacity: AtomicU64,
    held: AtomicU64,
    /// The part of `held` that `keep` holds: resident for a session, so no statement's end gives it back.
    kept: AtomicU64,
    /// Statements in flight and the most the machine's headroom supports (0 = ungated): the
    /// per-statement bytes outside the pool (the reader's images, the contexts) are bounded by count.
    inflight: AtomicU64,
    max_inflight: AtomicU64,
    waits: AtomicU64,
    wait_ns: AtomicU64,
    trims: AtomicU64,
    sheds: AtomicU64,
    refusals: AtomicU64,
    /// The small lane (`gravity_grant_mode=lanes`): a slice of the capacity only small statements' reservations take, by statement.
    reserve: AtomicU64,
    small_held: AtomicU64,
    small_admits: AtomicU64,
    small_fallbacks: AtomicU64,
    small_overflows: AtomicU64,
    /// D4403: the small buffer's configured size (`set_buffer`; 0 = none, or the lanes slice's `set_reserve`). `reserve` is
    /// it clamped to a sixteenth of the capacity and, budget mode on, of B (`clamp_buffer`, `policy::small_buffer`), at every
    /// arm (the broker's pool follow included) and every B the budget reads.
    buffer: AtomicU64,
    /// D4403: B at the budget's last snapshot under mode on (0 = no B clamp).
    buffer_b: AtomicU64,
    /// D4403: the draws with no reservation the buffer served; the small asks (statements and draws) that waited because
    /// the buffer and the big lane were both full; those whose wait ended refused.
    small_draws: AtomicU64,
    small_waits: AtomicU64,
    small_refusals: AtomicU64,
    borrows: AtomicU64,
    borrowed: AtomicU64,
    unadmitted: AtomicU64,
    unadmitted_bytes: AtomicU64,
    /// The overdrafts per site (`Overdraw`, its index): their count and bytes (`overdraws`), taken past the capacity.
    overdraw_n: [AtomicU64; Overdraw::N],
    overdraw_bytes: [AtomicU64; Overdraw::N],
    /// Statements whose grant came back (G's last holder gone): the bytes wait reads it as the pool turning over.
    ended: AtomicU64,
    /// The oldest waiting statement whose whole need later admissions leave free (`Turn`): its ticket << 32 | its need in MiB
    /// (0 = none); and the waiters' tickets.
    turn: AtomicU64,
    tickets: AtomicU64,
    /// D3380: a statement holds its floor alone past the big lane (`take_solo`); its reservation's drop clears it.
    solo: core::sync::atomic::AtomicBool,
}

/// D4923 diag: the pool's failed takes by why (`mempolicy::admit::TAKE_FAIL_*`), admissions [1] and draws [0] apart:
/// counts and bytes asked; no decision reads them (the ladder view's `takefail` rows).
static TAKE_FAILS: [[AtomicU64; mempolicy::admit::TAKE_FAILS]; 2] = [const { [const { AtomicU64::new(0) }; mempolicy::admit::TAKE_FAILS] }; 2];
static TAKE_FAIL_BYTES: [[AtomicU64; mempolicy::admit::TAKE_FAILS]; 2] = [const { [const { AtomicU64::new(0) }; mempolicy::admit::TAKE_FAILS] }; 2];

fn note_take_fail(admit: bool, why: usize, bytes: u64) {
    TAKE_FAILS[usize::from(admit)][why].fetch_add(1, Ordering::Relaxed);
    TAKE_FAIL_BYTES[usize::from(admit)][why].fetch_add(bytes, Ordering::Relaxed);
}

/// D4923 diag (the view): the failed takes by why, draws [0] and admissions [1]: (count, bytes asked).
pub fn take_fails() -> [[(u64, u64); mempolicy::admit::TAKE_FAILS]; 2] {
    core::array::from_fn(|a| core::array::from_fn(|w| (TAKE_FAILS[a][w].load(Ordering::Relaxed), TAKE_FAIL_BYTES[a][w].load(Ordering::Relaxed))))
}

pub static POOL: GrantPool = GrantPool { capacity: AtomicU64::new(0), held: AtomicU64::new(0), kept: AtomicU64::new(0), inflight: AtomicU64::new(0), max_inflight: AtomicU64::new(0), waits: AtomicU64::new(0), wait_ns: AtomicU64::new(0), trims: AtomicU64::new(0), sheds: AtomicU64::new(0), refusals: AtomicU64::new(0), reserve: AtomicU64::new(0), small_held: AtomicU64::new(0), small_admits: AtomicU64::new(0), small_fallbacks: AtomicU64::new(0), small_overflows: AtomicU64::new(0), buffer: AtomicU64::new(0), buffer_b: AtomicU64::new(0), small_draws: AtomicU64::new(0), small_waits: AtomicU64::new(0), small_refusals: AtomicU64::new(0), borrows: AtomicU64::new(0), borrowed: AtomicU64::new(0), unadmitted: AtomicU64::new(0), unadmitted_bytes: AtomicU64::new(0), overdraw_n: [const { AtomicU64::new(0) }; Overdraw::N], overdraw_bytes: [const { AtomicU64::new(0) }; Overdraw::N], ended: AtomicU64::new(0), turn: AtomicU64::new(0), tickets: AtomicU64::new(0), solo: core::sync::atomic::AtomicBool::new(false) };

std::thread_local! {
    /// In-flight slots this thread's live reservations hold (a count: they end in any order).
    static SLOTS_HERE: core::cell::Cell<u32> = const { core::cell::Cell::new(0) };
    /// The pool bytes this thread's live statements reserve (their G's, a parked cursor's included): a statement nested in
    /// them waits for none of these to come back.
    static HELD_HERE: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
    /// D3242: this thread's live grant holders (its share of `HOLDERS`): a wait on this thread never sees them give back.
    static HOLDERS_HERE: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
    /// D3910 (a): this thread's holders whose portal sits suspended (its share of `IDLE_HOLDERS`).
    static IDLE_HERE: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
}

/// D3242: the live grant holders, every statement's root `StmtCtx` (any session's, a nested one's and a parked cursor's
/// included), counted from its creation to its drop, which gives its bytes back first: a holder that has left the count has
/// no reserved bytes left to give. `Others` reads it: a wait for pool bytes waits only while a holder besides the caller's
/// own could give some back.
static HOLDERS: AtomicU64 = AtomicU64::new(0);

/// D3910 (a): grant holders whose portal is suspended (`park_slot` to unpark or drop): no wait waits on them (as no
/// PostgreSQL session waits on another's idle cursor); the memory policy shares count them, never cancel them.
static IDLE_HOLDERS: AtomicU64 = AtomicU64::new(0);

/// D3982: per ledger account (a worker's draws show on its leader's), its statement's part of the pool for its workers'
/// draws past W and G: the granted floor in whole MiB above 20 bits of the fraction's parts per million; 0 = no cap.
static WORKER_TERMS: [AtomicU64; memledger::ACCOUNT_SLOTS] = [const { AtomicU64::new(0) }; memledger::ACCOUNT_SLOTS];

/// D3982: a worker's draw of `bytes` past W and G would take its leader's account past the statement's part of the pool.
pub(crate) fn worker_past_cap(bytes: u64) -> bool {
    let a = memledger::current();
    let terms = WORKER_TERMS.get(a.0 as usize).map_or(0, |t| t.load(Ordering::Acquire));
    let ppm = (terms & 0xF_FFFF) as u32;
    if ppm == 0 {
        return false;
    }
    let held = (memledger::pool_reserved(a).max(0) as u64).saturating_add(bytes);
    let past = !within_bounds(held, (terms >> 20) << 20, POOL.statement_cap(ppm));
    if past {
        WORKER_CAP_REFUSED[0].fetch_add(1, Ordering::Relaxed);
        WORKER_CAP_REFUSED[1].fetch_add(bytes, Ordering::Relaxed);
    }
    past
}

static WORKER_CAP_REFUSED: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// `(draws, bytes)` of the workers' draws past W and G refused at their statement's part of the pool (`worker_past_cap`).
pub fn worker_cap_refused() -> (u64, u64) {
    (WORKER_CAP_REFUSED[0].load(Ordering::Relaxed), WORKER_CAP_REFUSED[1].load(Ordering::Relaxed))
}

/// D3910 (a): a holder's portal suspended (`true`) or resumed / ended suspended (`false`).
fn note_idle(suspended: bool) {
    if suspended {
        IDLE_HOLDERS.fetch_add(1, Ordering::AcqRel);
        IDLE_HERE.with(|h| h.set(h.get() + 1));
    } else {
        let _ = IDLE_HOLDERS.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| Some(n.saturating_sub(1)));
        IDLE_HERE.with(|h| h.set(h.get().saturating_sub(1)));
    }
    mcx::governor::idle_holder(suspended);
}

/// A statement the pool could not admit (S7): the floor's need did not fit within the wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdmitRefused {
    pub grant: Grant,
    pub need: u64,
    pub held: u64,
    pub capacity: u64,
    pub waited_ms: u64,
    /// The wait ended early because the caller's `stop` probe reported a pending interrupt (D599): not an out-of-memory
    /// refusal; the caller processes the interrupt (a cancel, a statement timeout, a termination) or waits again.
    pub interrupted: bool,
}

/// W1: a memory wait (the admission queue, the bytes wait) shows in `pg_stat_activity` as the backend layer's wait event
/// (`set_mem_wait`; the driver takes no backend dependency), from its first pause to its end. Only a thread's outermost
/// wait reports, so a nested wait never ends an outer one's event.
#[derive(Default)]
struct MemWait {
    reported: bool,
    /// Ladder 1.3 (`fair_share`): its statement marked waiting (`mcx::revoke::set_waiting`): a waiter is never a victim.
    waiting: bool,
}

thread_local! {
    static MEM_WAITING: Cell<bool> = const { Cell::new(false) };
}

static MEM_WAIT_START: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());
static MEM_WAIT_END: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

/// W1: the backend layer's wait-event report around a memory wait; `end` is stored first, so a wait that saw `start`
/// always finds `end`.
pub fn set_mem_wait(start: fn(), end: fn()) {
    MEM_WAIT_END.store(end as *mut (), Ordering::Release);
    MEM_WAIT_START.store(start as *mut (), Ordering::Release);
}

impl MemWait {
    /// The admission poll's pause between tries (2 ms). Through the waiter, not `thread::sleep`: the sleep rides the clock
    /// provider, so a virtual clock (DST) drives the admission waits and their deadlines coherently.
    fn pause(&mut self) {
        self.report();
        waiter::sleep(core::time::Duration::from_millis(2));
    }

    fn report(&mut self) {
        if !self.waiting && mcx::revoke::fair_on() {
            mcx::revoke::set_waiting(true);
            self.waiting = true;
        }
        if !self.reported && !MEM_WAITING.with(Cell::get) {
            let start = MEM_WAIT_START.load(Ordering::Acquire);
            if !start.is_null() {
                // SAFETY: only `set_mem_wait` stores here, always from a `fn()`.
                let f: fn() = unsafe { core::mem::transmute(start) };
                f();
                MEM_WAITING.with(|w| w.set(true));
                self.reported = true;
            }
        }
    }

    /// Ladder step 1.1 (`gate_now`): the pause parked on the free epoch instead of the 2 ms poll. `seen` is the epoch the
    /// caller read (`mcx::ladder::epoch_seq`) before its last take: the waiter publishes its handle in the park table,
    /// re-reads the epoch and parks only if no free moved it since, for `PARK_NET` at most (a safety net for the relief
    /// that comes without a free: a cancel landing's clock, a revoke's window, and the caller's own cancel, which
    /// cancelpoll ticks at every wake). Every free (`mcx::ladder::note_free`) wakes the published handles. Through the
    /// waiter's park, which rides the installed clock provider, so a virtual clock (DST) drives it as it drives `pause`.
    /// A full table falls back to `pause` (counted, `PARK_FALLBACKS`).
    fn park_free(&mut self, seen: u64) {
        self.report();
        let word = waiter::current_handle().as_u64();
        let Some(cell) = PARKED_DRAWS.iter().find(|c| c.compare_exchange(0, word, Ordering::SeqCst, Ordering::Relaxed).is_ok()) else {
            PARK_FALLBACKS.fetch_add(1, Ordering::Relaxed);
            waiter::sleep(core::time::Duration::from_millis(2));
            return;
        };
        PARKED_N.fetch_add(1, Ordering::SeqCst);
        mcx::ladder::set_free_wake(wake_parked);
        if mcx::ladder::epoch_seq() == seen {
            PARKS.fetch_add(1, Ordering::Relaxed);
            let _ = waiter::park_timeout(PARK_NET);
        }
        PARKED_N.fetch_sub(1, Ordering::SeqCst);
        cell.store(0, Ordering::SeqCst);
    }
}

/// Ladder step 1.1: the safety net of a draw parked on the free epoch (LADDER-DESIGN section 8's 50 ms).
const PARK_NET: core::time::Duration = core::time::Duration::from_millis(50);
/// Ladder step 1.1: the waiter handles of the draws parked on the free epoch (0 = a free cell), and how many there are.
static PARKED_DRAWS: [AtomicU64; 64] = [const { AtomicU64::new(0) }; 64];
static PARKED_N: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// Ladder step 1.1: parks on the free epoch, and the waits that found the park table full and paused 2 ms instead.
pub static PARKS: AtomicU64 = AtomicU64::new(0);
pub static PARK_FALLBACKS: AtomicU64 = AtomicU64::new(0);

/// Ladder C1: the process pool's held bytes (`mcx::ladder::set_pool_held`).
fn pool_held() -> u64 {
    POOL.held()
}

/// Ladder step 1.1: a free moved the epoch (`mcx::ladder::note_free`): every parked draw re-tries its take.
fn wake_parked() {
    if PARKED_N.load(Ordering::SeqCst) == 0 {
        return;
    }
    for c in &PARKED_DRAWS {
        let w = c.load(Ordering::SeqCst);
        if w != 0 {
            let _ = waiter::unpark_word(w);
        }
    }
}

impl Drop for MemWait {
    fn drop(&mut self) {
        if self.waiting {
            mcx::revoke::set_waiting(false);
        }
        if self.reported {
            MEM_WAITING.with(|w| w.set(false));
            let end = MEM_WAIT_END.load(Ordering::Acquire);
            if !end.is_null() {
                // SAFETY: only `set_mem_wait` stores here, always from a `fn()`.
                let f: fn() = unsafe { core::mem::transmute(end) };
                f();
            }
        }
    }
}

/// D4353: an admission's R5 under the ladder: ends (its run and measured yield) however the admission ends.
struct AdmitWait(Option<mcx::ladder::Waiting>);

impl Drop for AdmitWait {
    fn drop(&mut self) {
        mcx::ladder::wait_end(self.0.take());
    }
}

/// Time since `t0` on the process monotonic clock (`pg_clock`, the virtual clock under DST): the admission deadlines
/// (`BYTES_WAIT`, `pgrust.gravity_grant_wait_ms`) and the waited counters read it.
/// D4439 (`draw_wait_cap`): the draws past the pool refused at once because their statement's draw-gate waits spent
/// the cap (`GrantPool::take_capped`), this process.
pub static DRAW_CAPPED: AtomicU64 = AtomicU64::new(0);

/// D4439: what is left of this thread's statement's draw-wait cap with the ladder on (`None`: no cap: the ladder off,
/// `draw_wait_cap` = 0, or no statement charged on this thread).
fn draw_cap_left() -> Option<core::time::Duration> {
    if !mcx::ladder::on() {
        return None;
    }
    let cap = policy::draw_wait_cap()?;
    mcx::governor::stmt_draw_waited_ns().map(|w| policy::draw_wait_left(cap, w))
}

/// D4439: a draw's wait from `t0` counted on its statement, where its cap applies (`cap_left`).
fn note_draw_wait(cap_left: Option<core::time::Duration>, t0: MonoStamp) {
    if cap_left.is_some() {
        mcx::governor::note_stmt_draw_wait(waited(t0).as_nanos().min(u64::MAX as u128) as u64);
    }
}

/// D4914: a draw of `bytes` past W and G refused on branch `why`: counted per branch and kept on the statement, so the
/// 53200 a refusing road raises names it (exec glue's detail).
#[cold]
pub(crate) fn note_refusal(why: mcx::ladder_stats::Why, bytes: u64) {
    mcx::ladder_stats::note_why(why, bytes);
    mcx::governor::note_stmt_refusal(why as u8, bytes);
}

fn waited(t0: MonoStamp) -> core::time::Duration {
    core::time::Duration::from_nanos(t0.elapsed_ns())
}

/// D4764: a test's clock for the D3127 settle (bit 63 = set, the low bits its ns; 0 = the monotonic clock, as in a server).
static SETTLE_CLOCK_FOR_TEST: AtomicU64 = AtomicU64::new(0);

pub fn set_settle_clock_for_test(ns: Option<u64>) {
    SETTLE_CLOCK_FOR_TEST.store(ns.map_or(0, |n| (1 << 63) | n), Ordering::Relaxed);
}

fn settle_now_ns() -> u64 {
    match SETTLE_CLOCK_FOR_TEST.load(Ordering::Relaxed) {
        0 => MonoStamp::now().as_ns(),
        v => v & !(1 << 63),
    }
}

/// C3: a start's throttle wait (`mcx::governor::throttled`), `bound` and `throttle_max_ms` at most, reported to the
/// governor's counts; false when `stop` (a pending interrupt) ended it.
fn throttle_wait(bound: core::time::Duration, stop: &dyn Fn() -> bool, w: &mut MemWait) -> bool {
    let t0 = MonoStamp::now();
    let cap = mempolicy::decide::throttle_cap(bound);
    let mut ok = true;
    while mcx::governor::throttled() && waited(t0) < cap {
        if stop() {
            ok = false;
            break;
        }
        w.pause();
    }
    mcx::governor::note_throttle_wait(waited(t0).as_millis() as u64);
    ok
}

/// The error of a region drawn under a strict reservation (`Reservation::unadmitted_strict`), raised as XX000 `gravity: ...`.
pub const UNADMITTED_DREW: &str = "unadmitted program drew grant memory";

/// A statement's admission (refactor-ledger.md §4.2): its grant, the bytes the pool holds for it, the small lane's slice, the
/// in-flight slot, and G, the grant context its draws are charged to (`None` unadmitted). Dropping it gives back the pool's part.
#[derive(Debug)]
pub struct Reservation {
    pub grant: Grant,
    pub asked: Grant,
    pub reserved: u64,
    pub waited_ms: u64,
    pub small: bool,
    /// D3127: how the admission came to `grant` (`Admitted`); the grant line reports it.
    pub how: Admitted,
    g: Option<Rc<StmtCtx>>,
    drawn: Cell<u64>,
    borrowed: Cell<u64>,
    /// Arena bytes reserved past G at their draw (D225), pool-held by their chunks.
    grown: Cell<u64>,
    /// `grown`'s live part: `note_grown` adds, and each such draw's hold takes its bytes back when it is released or parked
    /// (`Held::past`), on whichever thread drops it. Made at the statement's first draw past G.
    past: core::cell::OnceCell<Arc<PastCount>>,
    /// The most `charged` was.
    charged_peak: Cell<u64>,
    /// The resident peak: the most the Node held at once as seen at a section close (`drawn` is a flow).
    peak: Cell<u64>,
    /// D1421: the hosted stores' bytes charged past G (`charge_hosted`), given back at the drop.
    hosted: Cell<u64>,
    /// The in-flight slot it holds; `parked`: given back while its portal sits suspended (D601), re-taken at the next fetch.
    slot: Cell<bool>,
    parked: Cell<bool>,
    /// D2845: the bytes of the last draw past W and G the pool refused (`region::road_region`); 0 when none.
    refused: Cell<u64>,
    id: u64,
    /// Outside the pool on the premise that it draws no region (`unadmitted_strict`).
    strict: bool,
    /// D2990: the part of the pool's big lane this statement may hold, in parts per million (`grow_fits`; the session's
    /// `pgrust.gravity_statement_pool_fraction`, set at its admission: `set_pool_fraction`); `POOL_FRACTION_WHOLE` = no cap.
    pool_ppm: Cell<u32>,
    /// D3982: the account's worker terms before `set_pool_fraction` published this statement's, put back at the drop.
    terms_prev: Cell<Option<u64>>,
    /// G1-A: the memory ledger account that shows its pool charge (`ledger_charge_bytes`: `reserved` + its live draws past
    /// G), the creating thread's (its backend's).
    ledger: memledger::Acct,
    /// The owner its pool charge sits on (`owner`): the creating thread's context owner at the admission (its portal, else
    /// its running statement, else its session).
    owner: memledger::Owner,
    /// D4043: G's room its sections' workers draw past their W (`Purse`), made at the first section's open; the sections
    /// open on it now (`fill_purse`, `drain_purse`).
    purse: core::cell::RefCell<Option<Arc<Purse>>>,
    purse_depth: Cell<u32>,
    /// D4291: a pipeline open's own-spill road (`OWN_TRY`: the statement holds a sealed sort run no reader has begun, so a
    /// refused draw past W and G skips the wait, `OWN_REFUSED`: one did); the open puts the runs and retries once.
    own: Cell<u8>,
}

/// D4291: `Reservation::own`'s bits.
const OWN_TRY: u8 = 1;
const OWN_REFUSED: u8 = 2;

/// A statement's live draws past G (`Reservation::charged`), and (G1-A, C8) the same bytes as promised room of its
/// reservation's owner on the account that shows its reservation, so a hold that gives them back on any thread lowers
/// that account's charge.
#[derive(Debug)]
pub struct PastCount {
    n: AtomicU64,
    room: pgsync::Mutex<memledger::Charge>,
    /// D4795: the same bytes on the statement's governor count, given back as `room`'s are (`StmtMem::fits` reads it).
    gov: pgsync::Mutex<mcx::governor::Charge>,
}

impl PastCount {
    fn room(&self) -> pgsync::MutexGuard<'_, memledger::Charge> {
        self.room.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn gov(&self) -> pgsync::MutexGuard<'_, mcx::governor::Charge> {
        self.gov.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// D4043: a statement's reserved-but-unused bytes its sections' workers draw past their W before the pool's capacity. A
/// worker has no statement (D187: its draws past W took capacity alone), so a statement whose own reservation filled the
/// pool (an Estimate admission alone past it, nearly all of it the parallel owner pin) refused its workers' first grow
/// past W at once (`take_wait`: no other holder) and failed 53200 with G's room untouched. G's room after the
/// W carves is carved into it at a section's open (`Reservation::fill_purse`: kept, as a W is), drawn on any thread
/// (`worker_purse_draw`; the leader's `GrantPool::hold` past G's room), drained back to G at the outermost section's
/// close (a count of the sections open on it: one closing never takes the purse from another's live workers), and closed at
/// the reservation's drop. Its bytes and its closed bit are one atomic word: a draw given back returns to it while it is
/// open, else to the pool (a hold dropped after the statement ended); a hold that outlives its use (parked, reaped)
/// leaves it and gives its bytes to the pool, as a W's do (a recharged one gives back what its charge covers, D4279). A
/// draw is whole or none: one larger than the purse goes whole to the pool's road, so a big grow never drains it in pieces.
/// A draw from it is inside the reservation: no second charge on the memory ledger or the governor (admission charged
/// `reserved` once), no count past G (`PastCount`), and never refused at the statement's part of the pool. While G's room
/// sits in it, `lend` does not lend that room.
#[derive(Debug, Default)]
pub struct Purse {
    /// The bytes in it; `PURSE_CLOSED` once its reservation dropped.
    n: AtomicU64,
}

const PURSE_CLOSED: u64 = 1 << 63;

/// D4043: the draws a purse served (`Purse`), this process.
pub static PURSE_DRAWS: AtomicU64 = AtomicU64::new(0);
pub static PURSE_BYTES: AtomicU64 = AtomicU64::new(0);
/// D4043: a leader's draws past G's room its purse did not hold while a section was open on it (`GrantPool::hold`: the
/// caller's `left` counted bytes the section's workers drew before the take), left to the caller's road past G, this process.
pub static PURSE_MISSES: AtomicU64 = AtomicU64::new(0);

impl Purse {
    /// `bytes` taken whole; false when it holds fewer or is closed.
    fn take(&self, bytes: u64) -> bool {
        let mut cur = self.n.load(Ordering::Acquire);
        loop {
            if cur & PURSE_CLOSED != 0 || cur < bytes {
                return false;
            }
            match self.n.compare_exchange_weak(cur, cur - bytes, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return true,
                Err(now) => cur = now,
            }
        }
    }

    /// Up to `bytes` taken (none once closed); what moved.
    fn take_up_to(&self, bytes: u64) -> u64 {
        let mut cur = self.n.load(Ordering::Acquire);
        loop {
            let m = if cur & PURSE_CLOSED != 0 { 0 } else { cur.min(bytes) };
            if m == 0 {
                return 0;
            }
            match self.n.compare_exchange_weak(cur, cur - m, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return m,
                Err(now) => cur = now,
            }
        }
    }

    /// `bytes` drawn from it given back: into it while open, else to the pool (its reservation's G no longer holds them).
    fn put(&self, bytes: u64) {
        let mut cur = self.n.load(Ordering::Acquire);
        loop {
            if cur & PURSE_CLOSED != 0 {
                POOL.release(bytes as usize);
                return;
            }
            match self.n.compare_exchange_weak(cur, cur + bytes, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return,
                Err(now) => cur = now,
            }
        }
    }

    /// Everything in it taken out (for G's room); `close`: closed too, so a draw given back later goes to the pool.
    fn empty(&self, close: bool) -> u64 {
        let was = if close { self.n.swap(PURSE_CLOSED, Ordering::AcqRel) } else { self.n.fetch_and(PURSE_CLOSED, Ordering::AcqRel) };
        was & !PURSE_CLOSED
    }

    /// The bytes in it now.
    pub fn bytes(&self) -> u64 {
        self.n.load(Ordering::Acquire) & !PURSE_CLOSED
    }

    /// A draw of `bytes` it served: the hold that gives them back to it.
    fn held(self: &Arc<Self>, bytes: u64) -> Held {
        PURSE_DRAWS.fetch_add(1, Ordering::Relaxed);
        PURSE_BYTES.fetch_add(bytes, Ordering::Relaxed);
        fires::fire_shared("grant.purse_draw"); // shapecheck: a draw past W or G's room served from the statement's purse (D4043)
        Held { charged: None, pool: bytes, kept: false, past: None, shown: None, gov: None, purse: Some((self.clone(), bytes)), small: 0 }
    }
}

std::thread_local! {
    /// D4043: the purse of the claim this thread runs (`claim_purse`), for its draws past W (`worker_purse_draw`).
    static CLAIM_PURSE: core::cell::RefCell<Option<Arc<Purse>>> = const { core::cell::RefCell::new(None) };
}

/// D4043: the claim's purse set on this thread while the guard lives (the one before it back at its drop).
pub(crate) struct ClaimPurse(Option<Arc<Purse>>);

pub(crate) fn claim_purse(p: Option<&Arc<Purse>>) -> ClaimPurse {
    ClaimPurse(CLAIM_PURSE.with(|c| c.replace(p.cloned())))
}

impl Drop for ClaimPurse {
    fn drop(&mut self) {
        let was = self.0.take();
        CLAIM_PURSE.with(|c| *c.borrow_mut() = was);
    }
}

/// D4043: a worker's draw of `bytes` past its W from its claim's purse (its statement's reserved-but-unused bytes); `None`
/// when the claim has none or it holds fewer (the pool's capacity is the road then, as before).
pub(crate) fn worker_purse_draw(bytes: u64) -> Option<Held> {
    let p = CLAIM_PURSE.with(|c| c.borrow().as_ref().filter(|p| p.take(bytes)).cloned())?;
    Some(p.held(bytes))
}

pub use gravity_program::cost::runtime::{POOL_FRACTION_DEFAULT, POOL_FRACTION_WHOLE};

impl Reservation {
    /// A statement outside the pool (tests; the runner's unadmitted pull of a program with breakers): its draws take from the capacity.
    pub fn unadmitted(grant: Grant) -> Reservation {
        Reservation { grant, asked: grant, reserved: 0, waited_ms: 0, small: false, how: Admitted::Whole, g: None, drawn: Cell::new(0), borrowed: Cell::new(0), grown: Cell::new(0), past: core::cell::OnceCell::new(), charged_peak: Cell::new(0), peak: Cell::new(0), hosted: Cell::new(0), slot: Cell::new(false), parked: Cell::new(false), refused: Cell::new(0), id: crate::grant_ledger::next_id(), strict: false, pool_ppm: Cell::new(POOL_FRACTION_DEFAULT), terms_prev: Cell::new(None), ledger: memledger::current(), owner: memledger::charge::context().0, purse: core::cell::RefCell::new(None), purse_depth: Cell::new(0), own: Cell::new(0) }
    }

    /// H16 (a): a serial, breakerless, single-pipeline program outside the pool on either protocol, on the premise
    /// that it has nothing to reserve. The premise is guarded (`region_draw`): a slot or program-constant region, a `hold` or a
    /// grow under it is `UNADMITTED_DREW` (a debug assertion first). Its arena chunks come from the capacity, as an unadmitted
    /// pull's do (counted in `PoolStats::unadmitted*`).
    pub fn unadmitted_strict(grant: Grant) -> Reservation {
        Reservation { grant, asked: grant, reserved: 0, waited_ms: 0, small: false, how: Admitted::Whole, g: None, drawn: Cell::new(0), borrowed: Cell::new(0), grown: Cell::new(0), past: core::cell::OnceCell::new(), charged_peak: Cell::new(0), peak: Cell::new(0), hosted: Cell::new(0), slot: Cell::new(false), parked: Cell::new(false), refused: Cell::new(0), id: crate::grant_ledger::next_id(), strict: true, pool_ppm: Cell::new(POOL_FRACTION_DEFAULT), terms_prev: Cell::new(None), ledger: memledger::current(), owner: memledger::charge::context().0, purse: core::cell::RefCell::new(None), purse_depth: Cell::new(0), own: Cell::new(0) }
    }

    fn new(grant: Grant, asked: Grant, reserved: u64, waited_ms: u64, small: bool, slot: bool, how: Admitted) -> Reservation {
        if slot {
            RUNNING.fetch_add(1, Ordering::AcqRel); // D3147: an admitted statement (`running`)
        }
        let ledger = memledger::current();
        let (owner, release) = memledger::charge::context();
        let room = memledger::Charge::reserve(owner, release, memledger::Kind::Region, memledger::charge::Door::Heap, reserved);
        Reservation { grant, asked, reserved, waited_ms, small, how, g: Some(StmtCtx::root(reserved, room, mcx::governor::Charge::promise("Gravity grant", reserved as usize))), drawn: Cell::new(0), borrowed: Cell::new(0), grown: Cell::new(0), past: core::cell::OnceCell::new(), charged_peak: Cell::new(reserved), peak: Cell::new(0), hosted: Cell::new(0), slot: Cell::new(slot), parked: Cell::new(false), refused: Cell::new(0), id: crate::grant_ledger::next_id(), strict: false, pool_ppm: Cell::new(POOL_FRACTION_DEFAULT), terms_prev: Cell::new(None), ledger, owner, purse: core::cell::RefCell::new(None), purse_depth: Cell::new(0), own: Cell::new(0) }
    }

    pub fn is_strict(&self) -> bool {
        self.strict
    }

    /// The owner its pool charge sits on (its admission's context owner): what a buffer the statement's operators make for
    /// it, on any thread, charges (a `memledger::ChargedVec` opened on it).
    pub fn owner(&self) -> memledger::Owner {
        self.owner
    }

    /// A region draw (a slot or program-constant region, a `hold`, a grow) under this reservation: refused by name under a strict
    /// one, whose premise it breaks (a debug build asserts).
    pub fn region_draw(&self) -> crate::Result<()> {
        if self.strict {
            debug_assert!(false, "gravity: {UNADMITTED_DREW}");
            return Err(crate::Error::host(UNADMITTED_DREW));
        }
        Ok(())
    }

    /// G's room, its purse's included (D4043: carved from G's room for a section's workers; the leader's `hold` past
    /// G's room draws it too).
    pub fn left(&self) -> u64 {
        self.g.as_ref().map_or(0, |g| g.room()) + self.purse_bytes()
    }

    /// D4043: the bytes in its purse now (0 before its first section).
    pub fn purse_bytes(&self) -> u64 {
        self.purse.borrow().as_ref().map_or(0, |p| p.bytes())
    }

    /// D4043: G's room alone, its purse's not counted (`left` before the purse: what decides a draw's road past G).
    pub(crate) fn g_left(&self) -> u64 {
        self.g.as_ref().map_or(0, |g| g.room())
    }

    /// D4043: a section is open on its purse, so its workers draw it on their own threads.
    fn purse_open(&self) -> bool {
        self.purse_depth.get() > 0
    }

    /// D4043: a section opens (its W carved): G's room put in the statement's purse for its workers' draws past W
    /// (`Purse`; carved, as a W is), the purse returned for its claims (`claim_purse`). `None` unadmitted (no G).
    pub(crate) fn fill_purse(&self) -> Option<Arc<Purse>> {
        let g = self.g.as_ref()?;
        let p = self.purse.borrow_mut().get_or_insert_with(|| Arc::new(Purse::default())).clone(); // alloc: Exec
        self.purse_depth.set(self.purse_depth.get() + 1);
        let m = g.carve(g.room());
        if m != 0 {
            p.put(m);
        }
        Some(p)
    }

    /// D4043: a section closed (`close`; else after its close's folds): once no section is open the purse's bytes go back to
    /// G's room (its workers are done; what their draws give back later goes back into it until the next close or the drop).
    pub(crate) fn drain_purse(&self, close: bool) {
        let Some(p) = self.purse.borrow().clone() else { return };
        if close {
            self.purse_depth.set(self.purse_depth.get().saturating_sub(1));
        }
        if self.purse_depth.get() == 0 {
            self.unkeep(p.empty(false));
        }
    }

    /// `n` carved bytes back in G's room.
    fn unkeep(&self, n: u64) {
        if let Some(g) = self.g.as_ref().filter(|_| n != 0) {
            let g = g.g();
            debug_assert!(g.kept.get() >= n, "gravity: a purse gave back {n} B past G's kept {}", g.kept.get());
            g.kept.set(g.kept.get().saturating_sub(n));
        }
    }

    /// D4043: `bytes` from its purse whole (the leader's `hold` past G's room), or none.
    fn purse_take(&self, bytes: u64) -> Option<Arc<Purse>> {
        self.purse.borrow().as_ref().filter(|p| p.take(bytes)).cloned()
    }

    pub fn drawn(&self) -> u64 {
        self.drawn.get()
    }

    pub(crate) fn note_drawn(&self, bytes: u64) {
        self.drawn.set(self.drawn.get() + bytes);
    }

    /// The resident peak: `note_resident`'s maximum (M7-plan §3.13; the SB1 cell is judged on it beside reserved + grown).
    pub fn resident_peak(&self) -> u64 {
        self.peak.get()
    }

    pub(crate) fn note_resident(&self, bytes: u64) {
        self.peak.set(self.peak.get().max(bytes));
    }

    pub fn borrowed(&self) -> u64 {
        self.borrowed.get()
    }

    /// D1421: `bytes` more that a hosted store (a PostgreSQL aggregate's, window's or ARRAY accumulator's memory
    /// context, outside G) grew by, sampled at a pipeline's end: taken from the pool past G and never refused (PostgreSQL's
    /// own contexts have no cap), so admission sees them; counted as borrowed; given back when the reservation drops.
    pub fn charge_hosted(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        POOL.held.fetch_add(bytes, Ordering::AcqRel);
        POOL.borrows.fetch_add(1, Ordering::Relaxed);
        POOL.borrowed.fetch_add(bytes, Ordering::Relaxed);
        self.hosted.set(self.hosted.get() + bytes);
        self.borrowed.set(self.borrowed.get() + bytes);
    }

    /// D1421: a hosted store's growth past its last charge `mark` (its bytes now `now`) charged; `mark` moves up only.
    pub fn charge_growth(&self, mark: &Cell<u64>, now: u64) {
        if now > mark.get() {
            self.charge_hosted(now - mark.replace(now));
        }
    }

    /// D1421: the hosted bytes charged so far (their high-water mark: a store's charge only grows).
    pub fn hosted(&self) -> u64 {
        self.hosted.get()
    }

    pub fn holds_slot(&self) -> bool {
        self.slot.get()
    }

    /// D601: the in-flight slot given back while the statement's portal sits suspended between fetches (PostgreSQL never
    /// makes another session wait on an idle cursor), and with it the room the memory policy lends (D3910 (b)). `unpark_slot`
    /// re-takes both.
    pub fn park_slot(&self) {
        if self.slot.replace(false) {
            POOL.inflight.fetch_sub(1, Ordering::AcqRel);
            SLOTS_HERE.with(|s| s.set(s.get().saturating_sub(1)));
            self.parked.set(true);
            self.lend();
            if self.g.is_some() {
                note_idle(true);
            }
        }
    }

    /// D601: the parked slot and the lent room re-taken at the portal's next fetch, forced (past `max_inflight` and the
    /// capacity if others took them meanwhile): a FETCH never waits on admission, as PostgreSQL's never does.
    pub fn unpark_slot(&self) {
        if self.parked.replace(false) {
            if self.g.is_some() {
                note_idle(false);
            }
            POOL.inflight.fetch_add(1, Ordering::AcqRel);
            SLOTS_HERE.with(|s| s.set(s.get() + 1));
            self.slot.set(true);
            self.unlend();
        }
    }

    /// D3910 (b): G's room handed back to the pool, its ledger account and the statement's memory ledger while its portal
    /// sits suspended, so another statement admits into it instead of waiting on a holder that gives nothing back until
    /// its client acts.
    fn lend(&self) {
        let Some(g) = self.g.as_ref() else { return };
        let n = mempolicy::keep::suspend_lend(g.room(), self.small);
        if n == 0 {
            return;
        }
        g.lent.set(n);
        POOL.held.fetch_sub(n, Ordering::AcqRel);
        HELD_HERE.with(|h| h.set(h.get().saturating_sub(n)));
        g.charge.borrow_mut().shrink(n);
        g.mem.borrow().shrink(n as usize);
        // ladder step 1.1: the lent room is a free (the epoch moves and parked draws wake)
        if policy::gate_now() {
            mcx::ladder::note_free();
        }
    }

    fn unlend(&self) {
        let Some(g) = self.g.as_ref() else { return };
        let n = g.lent.replace(0);
        if n == 0 {
            return;
        }
        POOL.held.fetch_add(n, Ordering::AcqRel);
        HELD_HERE.with(|h| h.set(h.get() + n));
        g.charge.borrow_mut().grow(n);
        g.mem.borrow().grow_room(n as usize);
    }

    /// D3910 (b): the room lent now (0 unless its portal sits suspended).
    pub fn lent(&self) -> u64 {
        self.g.as_ref().map_or(0, |g| g.lent.get())
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    /// G, the statement's grant context (refactor-ledger.md §4.2; fmgr result contexts bind under it).
    pub fn grant_ctx(&self) -> Option<&mcx::MemoryContext> {
        self.g.as_deref().map(|g| &g.ctx)
    }

    pub fn life(&self, name: &'static str, limit: u64) -> Option<Rc<StmtCtx>> {
        self.g.as_ref().map(|g| g.child(name, limit))
    }

    /// `bytes` held for the statement outside the driver's draws (the top-k copy, F6); `None` = refused (and always under a
    /// strict reservation: the caller asks `region_draw` first for the named error).
    pub fn hold(&self, bytes: u64) -> Option<Held> {
        self.region_draw().ok()?;
        POOL.hold(bytes as usize, Some(self), None)
    }

    /// Grow mode (`Grant::grow`, D4559): `hold`, else (G's room short) grown past G from the pool's room now, as a
    /// breaker's grow is (`region::road_held_now`: no wait, within the statement's part of the pool, counted as grown);
    /// `None` when the pool has no room now (the caller's refusal road, as `hold`'s). Off: `hold`.
    pub fn hold_growing(&self, bytes: u64) -> Option<Held> {
        if !self.grant.grow || self.left() >= bytes || !self.admitted() {
            return self.hold(bytes);
        }
        self.region_draw().ok()?;
        crate::mem::region::road_held_now(bytes as usize, None, Some(self), false)
    }

    pub(crate) fn g(&self) -> Option<&Rc<StmtCtx>> {
        self.g.as_ref()
    }

    /// Admitted through the pool (a G to draw against), not an unadmitted pull.
    pub(crate) fn admitted(&self) -> bool {
        self.g.is_some()
    }

    pub fn grown(&self) -> u64 {
        self.grown.get()
    }

    /// The pool bytes the statement holds now: its reservation and its draws past G not yet given back (`grown` only rises).
    /// Nothing decides on it yet; the grant end line reports it.
    pub fn charged(&self) -> u64 {
        self.reserved + self.past.get().map_or(0, |p| p.n.load(Ordering::Acquire))
    }

    /// The most `charged` was while the statement ran (its reservation at least).
    pub fn charged_peak(&self) -> u64 {
        self.charged_peak.get()
    }

    /// D2990: whether `bytes` more drawn past G keep this statement's pool holds (its reservation and its growth past G so
    /// far) within the user's hard limits: one statement's part of the pool (`GrantPool::statement_cap`, the session's
    /// `pgrust.gravity_statement_pool_fraction`; the whole lane by default) and its memory limit (pgrust.statement_mem_limit,
    /// or the watchdog's cancel). Past either the draw is refused at once and the caller takes the refusal's road (a spill,
    /// the serial owners' mapped temp file, D491). Unarmed (capacity 0): no cap.
    /// No share of the pool among the statements running bounds a draw: each breaker's own bound is its PostgreSQL per-node
    /// budget (work_mem, x hash_mem_multiplier for a hash node), and beside it only the pool's own room and wait and the
    /// governor bound a statement, as PostgreSQL's per-node work_mem and nothing per statement does.
    /// exactsize D3226: the cap never refuses a draw that keeps the statement within one floor past its own granted bytes
    /// (`granted_floor`): an Estimate admission grants more than it reserves, and the breakers' shares are cut from the
    /// grant, so their region opens past the reservation are draws past G, and a region open has no spill road.
    pub(crate) fn grow_fits(&self, bytes: u64) -> bool {
        let held = self.reserved.saturating_add(self.grown.get()).saturating_add(bytes);
        if !within_bounds(held, granted_floor(self.grant.bytes, self.reserved), POOL.statement_cap(self.pool_ppm.get())) {
            fires::fire_shared("grant.statement_cap"); // shapecheck: a draw past G refused at the statement's part of the pool (D2990)
            return false;
        }
        if !self.g.as_ref().is_none_or(|g| g.mem.borrow().fits(bytes as usize)) {
            fires::fire_shared("grant.statement_mem_limit"); // shapecheck: a draw past G refused at the statement's memory limit
            return false;
        }
        true
    }

    /// D2990: the part of the pool's big lane this statement may hold (`grow_fits`), in parts per million: the session's
    /// `pgrust.gravity_statement_pool_fraction` at its admission, clamped to (0, 1]; `POOL_FRACTION_WHOLE` = no cap.
    pub fn set_pool_fraction(&self, ppm: u32) {
        self.pool_ppm.set(ppm.clamp(1, POOL_FRACTION_WHOLE));
        // D3982: its parallel workers' draws past W and G are held to the same part, counted on its ledger account
        let terms = if self.pool_ppm.get() >= POOL_FRACTION_WHOLE { 0 } else { ((granted_floor(self.grant.bytes, self.reserved) >> 20) << 20) | self.pool_ppm.get() as u64 };
        if let Some(t) = WORKER_TERMS.get(self.ledger.0 as usize) {
            let prev = t.swap(terms, Ordering::AcqRel);
            if self.terms_prev.get().is_none() {
                self.terms_prev.set(Some(prev));
            }
        }
    }

    /// An arena chunk of `bytes` past G, reserved at its draw (D225): counted as drawn and grown, and charged until its hold
    /// gives them back: the count returned, which that hold carries (`Held::grown`, `Held::count_past`).
    pub(crate) fn note_grown(&self, bytes: u64) -> Arc<PastCount> {
        self.grown.set(self.grown.get() + bytes);
        self.drawn.set(self.drawn.get() + bytes);
        let past = self.past.get_or_init(|| {
            // the reservation's owner; one made with no room (unadmitted) takes its account's context owner now
            let room = self.g.as_ref().map(|g| {
                let r = g.charge.borrow();
                (r.acct(), r.owner(), r.release())
            });
            let (owner, release) = match room {
                Some((a, owner, release)) if a == self.ledger => (owner, release),
                _ => memledger::charge::context_of(self.ledger),
            };
            let room = memledger::Charge::reserve(owner, release, memledger::Kind::Region, memledger::charge::Door::Heap, 0);
            let gov = if self.g.is_some() { mcx::governor::Charge::new("Gravity grant", 0) } else { mcx::governor::Charge::NONE };
            Arc::new(PastCount { n: AtomicU64::new(0), room: pgsync::Mutex::new(room), gov: pgsync::Mutex::new(gov) }) // alloc: Exec
        });
        past.n.fetch_add(bytes, Ordering::AcqRel);
        // a draw past G is pages at once (C8 move 3c): committed on the statement's memory ledger, not room
        past.gov().grow(bytes as usize);
        // C8 move 3c: a draw past G is pages its hold commits at once (a region's allocation, a reservation's commit in
        // place): its room is backed as it is promised, so the account counts it once
        let mut room = past.room();
        room.grow(bytes);
        room.back(bytes);
        drop(room);
        self.charged_peak.set(self.charged_peak.get().max(self.charged()));
        past.clone()
    }

    /// D2845: the bytes of the last draw past W and G the grant pool refused this statement; 0 when it refused none.
    pub fn refused(&self) -> u64 {
        self.refused.get()
    }

    /// D2845: a draw of `bytes` past W and G the pool refused (`region::road_region`): the kernel that asked reads it as its
    /// scratch running out (kind 2); `Entry::run_pipeline` names it.
    pub(crate) fn note_refused(&self, bytes: u64) {
        self.refused.set(bytes);
    }

    /// D4291: a pipeline open with the statement's own sealed sort runs to put first (`on`), or its end (off, the refusal
    /// forgotten): while on, a draw past W and G the pool refuses after its depots skips the wait (`own_first`).
    pub(crate) fn set_own_try(&self, on: bool) {
        self.own.set(if on { OWN_TRY } else { 0 });
    }

    /// D4291: the own-spill road is open: the depots, then the statement's own sorts, before any wait (`region::own_first`).
    pub(crate) fn own_try(&self) -> bool {
        self.own.get() & OWN_TRY != 0
    }

    /// D4291: a draw on the own-spill road was refused (no refusal is kept for the kernel: the open retries).
    pub(crate) fn note_own_refused(&self) {
        self.own.set(self.own.get() | OWN_REFUSED);
    }

    /// D4291: whether a draw on the own-spill road was refused since `set_own_try`.
    pub(crate) fn own_refused(&self) -> bool {
        self.own.get() & OWN_REFUSED != 0
    }
}

/// memfree Part 2: under `pgrust.statement_mem_limit` the grant asked halves (`mempolicy::admit::halve_for_limit`).
fn fit_statement_limit(mut want: Grant, need_of: &dyn Fn(Grant) -> u64) -> Grant {
    let limit = mcx::governor::current().map_or(0, |l| l.limit()) as u64;
    if limit == 0 {
        return want;
    }
    while let Some(half) = mempolicy::admit::halve_for_limit(need_of(want), limit, want.bytes, GRANT_FLOOR) {
        want = want.trimmed(half);
    }
    want
}

/// A statement memory context: G (the grant) or one breaker life's L under it; charges hold it alive.
pub struct StmtCtx {
    /// G only, C8 (area move 3): its pool bytes on the reservation's ledger account as its owner's promised room
    /// (`reserved` less the lent part; the owner the context's at admission: the portal its backend executes, else its
    /// statement), given back at the reservation's drop. Move 3c: a hold charged here backs its bytes (`Held::back_g`), so
    /// the account counts a drawn byte once (its pages on the doors), not again as room.
    charge: core::cell::RefCell<memledger::Charge>,
    /// G only, memfree Part 2: its pool bytes (`reserved` + `grown`) on the statement's memory ledger, once (G's charges and
    /// the regions are not memory-context blocks); given back to that ledger at the reservation's drop. C8 move 3c (b): the
    /// reservation is promised room there (`Charge::promise`) until a hold charged here backs it, so the statement's
    /// committed count (what the projection's victim pick and the backstop's fallback rank read) leaves untouched room out.
    mem: core::cell::RefCell<mcx::governor::Charge>,
    ctx: mcx::MemoryContext,
    parent: Option<Rc<StmtCtx>>,
    /// G only: the pool bytes the admission holds and the part handed to holds that outlive the statement.
    reserved: u64,
    kept: Cell<u64>,
    /// G only, D3910 (b): the room lent back to the pool while the statement's portal sits suspended (`Reservation::lend`).
    lent: Cell<u64>,
}

impl Drop for StmtCtx {
    /// G's last holder gone (the reservation and every charge): the pool bytes no settled hold took and none lent go back.
    fn drop(&mut self) {
        if self.parent.is_some() {
            return;
        }
        POOL.held.fetch_sub(self.reserved - self.kept.get() - self.lent.get(), Ordering::AcqRel);
        POOL.ended.fetch_add(1, Ordering::AcqRel);
        HELD_HERE.with(|h| h.set(h.get().saturating_sub(self.reserved - self.lent.get())));
        // D3242: after its bytes went back, so a wait that sees the holder gone finds them in the pool
        HOLDERS_HERE.with(|h| h.set(h.get().saturating_sub(1)));
        HOLDERS.fetch_sub(1, Ordering::AcqRel);
        // ladder step 1.1: a statement's end is the biggest free (the epoch moves and parked draws wake)
        if policy::gate_now() {
            mcx::ladder::note_free();
        }
    }
}

impl core::fmt::Debug for StmtCtx {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "StmtCtx({} of {})", self.ctx.subtree_used(), self.ctx.limit())
    }
}

impl StmtCtx {
    fn root(reserved: u64, charge: memledger::Charge, mem: mcx::governor::Charge) -> Rc<StmtCtx> {
        HELD_HERE.with(|h| h.set(h.get() + reserved));
        HOLDERS.fetch_add(1, Ordering::AcqRel);
        HOLDERS_HERE.with(|h| h.set(h.get() + 1));
        Rc::new(StmtCtx { ctx: mcx::MemoryContext::new("GravityGrant").with_limit(reserved as usize), parent: None, reserved, kept: Cell::new(0), lent: Cell::new(0), charge: core::cell::RefCell::new(charge), mem: core::cell::RefCell::new(mem) }) // alloc: Exec
    }

    fn g(&self) -> &StmtCtx {
        self.parent.as_deref().map_or(self, StmtCtx::g)
    }

    pub fn room(&self) -> u64 {
        let g = self.g();
        g.reserved.saturating_sub(g.used() + g.kept.get() + g.lent.get())
    }

    pub(crate) fn carve(&self, bytes: u64) -> u64 {
        let m = self.room().min(bytes);
        let g = self.g();
        g.kept.set(g.kept.get() + m);
        m
    }

    pub(crate) fn child(self: &Rc<Self>, name: &'static str, limit: u64) -> Rc<StmtCtx> {
        Rc::new(StmtCtx { ctx: self.ctx.new_child(name).with_limit(limit as usize), parent: Some(self.clone()), reserved: 0, kept: Cell::new(0), lent: Cell::new(0), charge: core::cell::RefCell::new(memledger::Charge::NONE), mem: core::cell::RefCell::new(mcx::governor::Charge::NONE) }) // alloc: Exec
    }

    pub fn used(&self) -> u64 {
        self.ctx.subtree_used() as u64
    }

    /// Up to `bytes` charged here and on every limited ancestor: what fits this context's limit and G's room.
    fn charge_fit(self: &Rc<Self>, bytes: u64) -> Option<(mcx::ExternalAllocationGuard<'static>, Rc<StmtCtx>, u64)> {
        let own = (self.ctx.limit() as u64).saturating_sub(self.used());
        let n = bytes.min(self.room()).min(own);
        if n == 0 {
            return None;
        }
        let guard = self.ctx.mcx().reserve_external(n as usize).ok()?;
        // SAFETY: the guard borrows `self.ctx`; the charge tuple keeps this Rc (so the context) alive and drops the guard first.
        let guard: mcx::ExternalAllocationGuard<'static> = unsafe { core::mem::transmute(guard) };
        Some((guard, self.clone(), n))
    }
}

/// What holds a draw's bytes: a statement context's charge, and the pool's capacity for the rest (past the context, no statement).
#[derive(Default)]
pub struct Held {
    /// G1-B: a worker's draw past W and G (no statement count: `past` is a leader's): the bytes shown as promised room of
    /// the claim's owner (its leader's: C8, the context's at the draw) on that owner's account, given back as `past`'s are.
    /// Read by no decision.
    shown: Option<memledger::Charge>,
    /// D4373 (a): a worker's draw past W and G charged on its statement's memory ledger (the governor's count of the
    /// leader's statement, which the worker's claim entered: `mcx::governor::current`), the counter the leader's
    /// `Reservation::note_grown` charges, so the watchdog's victim and the ladder see a parallel sort's grown regions;
    /// given back as `shown`'s are.
    gov: Option<(mcx::governor::Charge, u64)>,
    charged: Option<(mcx::ExternalAllocationGuard<'static>, Rc<StmtCtx>, u64)>,
    pool: u64,
    /// `pool` is a `GrantPool::keep` (counted in `kept` too); such a hold is never recharged.
    kept: bool,
    /// A leader's draw past G: its statement's count of live draws past G (`Reservation::charged`) and the bytes of this hold
    /// counted there, given back to it as `pool` shrinks, when the hold parks (`settle`) and when it drops.
    past: Option<(Arc<PastCount>, u64)>,
    /// D4043: the purse `pool`'s bytes (at most these) came from, which they go back to at the drop; none once the hold
    /// outlives its use (`unpurse`: its bytes the pool's then, as a W's).
    purse: Option<(Arc<Purse>, u64)>,
    /// D4403: the part of `pool` the small buffer holds (a draw with no reservation, `GrantPool::hold`), given back to it
    /// at the drop, or to the big lane's account when the hold changes hands (`unsmall`).
    small: u64,
}

impl Drop for Held {
    fn drop(&mut self) {
        self.unsmall();
        self.unback_g();
        self.uncount(0);
        if let Some((p, n)) = self.purse.take() {
            let n = n.min(self.pool);
            p.put(n);
            self.pool -= n;
        }
        give_back(self.pool, self.kept);
    }
}

/// A hold's pool bytes released (its drop's, and the reaper's for `Held::take_pool`'s once their region is unmapped).
pub(crate) fn give_back(pool: u64, kept: bool) {
    if kept {
        POOL.kept.fetch_sub(pool, Ordering::AcqRel);
    }
    if pool != 0 {
        POOL.release(pool as usize);
    }
}

impl Held {
    pub const NONE: Held = Held { charged: None, pool: 0, kept: false, past: None, shown: None, gov: None, purse: None, small: 0 };

    pub(crate) fn pool(n: u64) -> Held {
        Held { charged: None, pool: n, kept: false, past: None, shown: None, gov: None, purse: None, small: 0 }
    }

    /// Take-back: the settled hold `take_pool` took out of a region (`(pool bytes, a keep)`), its region's again.
    pub(crate) fn settled(n: u64, kept: bool) -> Held {
        Held { charged: None, pool: n, kept, past: None, shown: None, gov: None, purse: None, small: 0 }
    }

    /// `n` pool bytes drawn past G, counted on `past` (the leader statement's `Reservation::note_grown`; `None` for a worker's
    /// draw) until they are given back.
    pub(crate) fn grown(n: u64, past: Option<Arc<PastCount>>) -> Held {
        Held { charged: None, pool: n, kept: false, past: past.map(|p| (p, n)), shown: None, gov: None, purse: None, small: 0 }
    }

    /// G1-B: a worker's `n` pool bytes drawn past W and G, shown on the calling thread's memory ledger account (a claim's:
    /// its leader's) until they are given back. The ledger's pool bytes only: no statement count, no decision.
    pub(crate) fn grown_worker(n: u64) -> Held {
        let mut h = Held::pool(n);
        h.show_worker(n);
        h
    }

    /// G1-B: `n` bytes of this hold (a parked region a worker took past W and G) shown on the calling thread's memory ledger
    /// account, as `grown_worker`'s.
    pub(crate) fn show_worker(&mut self, n: u64) {
        self.uncount(0);
        let (owner, release) = memledger::charge::context();
        let mut shown = memledger::Charge::reserve(owner, release, memledger::Kind::Region, memledger::charge::Door::Heap, n);
        // C8 move 3c: the region it holds has its pages on the doors: the room is backed (counted once)
        shown.back(n);
        self.shown = Some(shown);
        // D4373 (a): on the statement's governor count too (none outside a statement's claim: no ledger entered)
        self.gov = Some((mcx::governor::Charge::new("gravity worker grown", n as usize), n));
    }

    /// A parked region's settled hold taken past G by a leader (`road_region`): `n` bytes of it counted on that statement's
    /// `past` (`Reservation::note_grown`).
    pub(crate) fn count_past(&mut self, past: Arc<PastCount>, n: u64) {
        self.uncount(0);
        self.past = Some((past, n));
    }

    /// D4403: the small buffer's part of this hold given back to the buffer; its bytes stay held, on the big lane's account
    /// (a hold that settles, parks, resizes, moves to the pool or recharges is no longer a draw with no reservation).
    fn unsmall(&mut self) {
        if self.small != 0 {
            POOL.small_held.fetch_sub(self.small, Ordering::AcqRel);
            self.small = 0;
        }
    }

    /// The bytes counted on the statement's draws past G cut to `to` (the pool part shrank to it), none at 0.
    fn uncount(&mut self, to: u64) {
        if to == 0 {
            if let Some((p, n)) = self.past.take() {
                p.n.fetch_sub(n, Ordering::AcqRel);
                p.room().shrink(n);
                p.gov().shrink(n as usize);
            }
        } else if let Some((p, n)) = self.past.as_mut().filter(|(_, n)| *n > to) {
            p.n.fetch_sub(*n - to, Ordering::AcqRel);
            p.room().shrink(*n - to);
            p.gov().shrink((*n - to) as usize);
            *n = to;
        }
        if let Some(c) = self.shown.as_mut().filter(|c| c.reserved() > to) {
            let n = c.reserved();
            c.shrink(n - to);
            if to == 0 {
                self.shown = None;
            }
        }
        if let Some((g, n)) = self.gov.as_mut().filter(|(_, n)| *n > to) {
            g.shrink((*n - to) as usize);
            *n = to;
            if to == 0 {
                self.gov = None;
            }
        }
    }

    pub fn is_charged(&self) -> bool {
        self.charged.is_some()
    }

    /// C8 move 3c: the bytes this hold charges to G back G's promised room on the ledger (`StmtCtx::charge`): what it holds
    /// is pages on the doors (a region's allocation, its share mapped writable, a node's or a top-k copy's draw), so the
    /// account counts them once. A purse's part (D4043, carved from G for a section's workers) stays unbacked.
    fn back_g(&self) {
        if let Some((_, c, n)) = self.charged.as_ref() {
            let g = c.g();
            g.charge.borrow_mut().back(*n);
            g.mem.borrow().back(*n as usize);
        }
    }

    /// C8 move 3c: `back_g` undone, before the charge to G goes (the hold settles, drops, moves to the pool or recharges).
    /// After the reservation's drop its room is gone (`Charge::NONE`): nothing to undo.
    fn unback_g(&self) {
        if let Some((_, c, n)) = self.charged.as_ref() {
            let g = c.g();
            g.charge.borrow_mut().unback(*n);
            g.mem.borrow().unback(*n as usize);
        }
    }

    /// A charge made a pool hold outliving the statement: the bytes leave G's reservation (G's drop gives back the rest), and
    /// a draw past G leaves its statement's count (`Reservation::charged`).
    pub(crate) fn settle(&mut self) {
        self.unsmall();
        self.uncount(0);
        self.unpurse();
        self.unback_g();
        if let Some((guard, ctx, n)) = self.charged.take() {
            drop(guard);
            let g = ctx.g();
            g.kept.set(g.kept.get() + n);
            self.pool += n;
        }
    }

    /// D3074 / D3089: a settled hold (a parked region's) resized to `bytes` as its region is fitted to a take's ask: the pool
    /// gets back what the region no longer holds, or holds what it grows by first; false (nothing changed) when the pool
    /// has no room for the growth, or the hold is a `keep` (never resized).
    pub(crate) fn resize_pool(&mut self, bytes: u64) -> bool {
        debug_assert!(self.charged.is_none(), "a parked region's hold is settled");
        if self.kept || self.charged.is_some() {
            return bytes == self.pool;
        }
        self.unsmall();
        self.unpurse();
        if bytes < self.pool {
            POOL.release((self.pool - bytes) as usize);
            self.uncount(bytes);
        } else if bytes > self.pool && !POOL.take(bytes - self.pool) {
            return false;
        }
        self.pool = bytes;
        true
    }

    /// The bytes the pool holds for this draw (its settled or uncharged part).
    pub fn pool_bytes(&self) -> u64 {
        self.pool
    }

    /// The pool's part of this hold taken out for the reaper (`mem::reap`): a charge to G handed to the pool first
    /// (`to_pool`), the statement's count past G given back now; `(pool bytes, a keep)`, this hold then releasing nothing.
    /// `None` (nothing changed) when the pool has no room for the charge.
    pub(crate) fn take_pool(&mut self) -> Option<(u64, bool)> {
        self.unsmall();
        if !self.to_pool() {
            return None;
        }
        self.uncount(0);
        self.unpurse();
        let out = (self.pool, self.kept);
        (self.pool, self.kept) = (0, false);
        Some(out)
    }

    /// A life's charge handed to the pool mid-statement (its region to the free list): G's room comes back; false = no capacity.
    /// D4043: a purse draw too (the dev check's table_owner_pin_free: an owner pin drawn from the purse and given back at its
    /// slot's free stayed carved from G for the rest of the statement): the pool takes its bytes and the purse gets them back.
    pub(crate) fn to_pool(&mut self) -> bool {
        self.unsmall();
        let n = self.charged.as_ref().map_or(0, |c| c.2);
        let m = self.purse.as_ref().map_or(0, |(_, m)| (*m).min(self.pool));
        if n + m == 0 {
            return true;
        }
        if !POOL.take(n + m) {
            return false;
        }
        self.unback_g();
        self.charged = None;
        if let Some((p, _)) = self.purse.take() {
            p.put(m);
        }
        self.pool += n;
        true
    }

    /// D4043: a hold drawn from a purse kept past its use (parked, reaped, resized): its bytes stay out of the
    /// purse (carved from G, as a W's are) and go to the pool at its drop.
    fn unpurse(&mut self) {
        self.purse = None;
    }

    /// The hold charged whole to `stmt`'s `life` (else G; D191); false = it does not fit (S7), the caller takes an exact one.
    pub(crate) fn recharge(&mut self, stmt: Option<&Reservation>, life: Option<&Rc<StmtCtx>>, bytes: usize) -> bool {
        let Some(ctx) = life.or_else(|| stmt.and_then(Reservation::g)) else { return true };
        if self.charged.as_ref().is_some_and(|(_, c, _)| Rc::ptr_eq(c, ctx)) {
            return true;
        }
        self.unsmall();
        self.unback_g();
        self.charged = None;
        self.charged = ctx.charge_fit(bytes as u64);
        self.back_g();
        let rest = bytes as u64 - self.charged.as_ref().map_or(0, |c| c.2);
        // D4279: the bytes the charge now covers leave the purse part first, back to the purse (they went to the pool: two
        // slot opens whose charge did not fit lost their purse draws, and the third, past G, met a full pool: 53200); a hold
        // the charge does not cover whole keeps the rest of its purse part, which its caller's drop puts back
        if rest < self.pool {
            let mut free = self.pool - rest;
            if let Some((p, m)) = self.purse.as_mut() {
                let back = free.min(*m);
                p.put(back);
                *m -= back;
                free -= back;
            }
            POOL.release(free as usize);
            self.pool = rest;
            self.uncount(rest);
        }
        if self.purse.as_ref().is_some_and(|&(_, m)| m == 0 || rest == 0) {
            self.unpurse();
        }
        rest == 0
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        // D4043: the purse closed and its bytes back in G's room before G's drop gives that back (a draw given back after
        // goes to the pool)
        if let Some(p) = self.purse.borrow().clone() {
            self.unkeep(p.empty(true));
        }
        let pool = &POOL;
        if let (Some(prev), Some(t)) = (self.terms_prev.get(), WORKER_TERMS.get(self.ledger.0 as usize)) {
            t.store(prev, Ordering::Release);
        }
        // C8: its promised room (`reserved` less the lent part) given back, though a settled hold keeps G alive
        if let Some(g) = self.g.as_ref() {
            drop(g.charge.replace(memledger::Charge::NONE));
            // memfree Part 2: its pool bytes given back to the statement's memory ledger
            drop(g.mem.replace(mcx::governor::Charge::NONE));
        }
        // a hold that outlives the statement gives nothing back on the next statement's count (it restarts at 0)
        if let Some(p) = self.past.get() {
            drop(core::mem::replace(&mut *p.gov(), mcx::governor::Charge::NONE));
        }
        if self.small {
            pool.small_held.fetch_sub(self.reserved, Ordering::AcqRel);
        }
        if self.how == Admitted::Solo {
            pool.solo.store(false, Ordering::Release); // D3380: the next floor past the pool may run alone once nothing is held
        }
        if self.slot.get() || self.parked.get() {
            RUNNING.fetch_sub(1, Ordering::AcqRel); // D3147: counted at `new` (a parked cursor still holds its bytes)
        }
        if self.parked.get() && self.g.is_some() {
            note_idle(false);
        }
        if self.slot.get() {
            pool.inflight.fetch_sub(1, Ordering::AcqRel);
            SLOTS_HERE.with(|s| s.set(s.get().saturating_sub(1)));
        }
        if self.hosted.get() != 0 {
            pool.release(self.hosted.get() as usize);
        }
        // D3063 (b): an admitted statement's end trims what the pool holds for no statement to a quarter of it
        if self.g.is_some() {
            crate::park::trim_idle_holds();
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PoolStats {
    pub capacity: u64,
    pub held: u64,
    pub inflight: u64,
    pub max_inflight: u64,
    pub waits: u64,
    pub wait_ms: u64,
    pub trims: u64,
    pub sheds: u64,
    pub refusals: u64,
    pub reserve: u64,
    pub small_held: u64,
    pub small_admits: u64,
    pub small_fallbacks: u64,
    pub small_overflows: u64,
    /// D4403: the draws with no reservation the small buffer served, the small asks that waited (buffer and big lane both
    /// full), and those whose wait ended refused.
    pub small_draws: u64,
    pub small_waits: u64,
    pub small_refusals: u64,
    /// Draws with no statement taken from the capacity (the borrowing S7 removes); an unadmitted pull's are its own (D187, not L1s).
    pub borrows: u64,
    pub borrowed: u64,
    pub unadmitted: u64,
    pub unadmitted_bytes: u64,
}

/// The one divisor (S2): pipeline `pipeline`'s shares at `grant` over `width`; every reservation and opened limit reads it.
/// D2587: a breaker's part is at least its kind's PostgreSQL budget (`Grant::floor_of`) up to its modeled need (D2894,
/// `charged_floor`), so the reservation covers the floors too.
pub fn share(program: &gravity_program::Program, pipeline: usize, grant: Grant, width: u16) -> Split {
    share_with(program, pipeline, grant, width, crate::node::program_images(program))
}

/// `share` with the program's `node::program_images` already read (it walks every pipeline): a Node's `lives::Schedule`
/// holds it, `footprint` and exec's run-time bits bind (runner/plan.rs `Bound`) read it once per program.
pub fn share_with(program: &gravity_program::Program, pipeline: usize, grant: Grant, width: u16, images: bool) -> Split {
    share_of(program, pipeline, &program.pipelines[pipeline], grant, width, images)
}

/// `share_with` over `pipe`, `program`'s pipeline `p` as the grant reads it (exactsize B4: `Node::share` passes a
/// slot-fed hash table with its producer's counted rows).
/// A participant's share is at most its breaker's ceiling (`ceiling_of`; the leader's `leader_ceiling`), so no share opens
/// past what the breaker may hold.
pub(crate) fn share_of(program: &gravity_program::Program, p: usize, pipe: &gravity_program::Pipeline, grant: Grant, width: u16, images: bool) -> Split {
    if spooled(program, p, pipe) {
        let (live, floor) = (program.slots.share_live(pipe.produces()), SHARE_FLOOR as u64);
        return grant.split_capped(live, width.max(1), false, 0, 0, floor, floor);
    }
    if grant.grow {
        return grow_share(program, pipe, grant, width, images);
    }
    let budget = floor_for(grant, pipe);
    let floor = pipe.breaker().map_or(0, |b| charged_floor(budget, b.kind, pipe, grant));
    // a breaker that starts small is never raised: it starts small (or on a warm parked region) and grows in place; a
    // learned peak, committed or prepaid, cost more than the growth it saved (fresh pages, a full reservation).
    // D3412: nor is one a serial owner folds (`footprint_parts`' pin of twice the slot's bytes beside it): its slot's
    // learned peak is the fold the pin already holds, so a raise reserved three times that peak and drew it twice
    let serial_owner = pipe.owner.is_some() && !(width >= 2 && pipe.parallel == gravity_program::Parallel::Eligible);
    // D4043: nor is one under a trim (`Grant::trimmed`): the trim is what the pool could seat, its shares are its own bytes'
    // split. Raised (a trim has no budgets, so uncapped), a W4 sort trimmed to 64 MiB of 16384 MiB asked opened a 1.7 GiB
    // leader and four 1 GiB worker shares (reserved 5.8 GiB): halving never lowered the need, the floor road admitted it
    // alone past the pool, and the first open past G's remainder was an unnamed 53200
    let (floor, wf) = match pipe.breaker() {
        Some(b) if !(starts_small(pipe, grant) && !program.code_of(p).is_hand()) && !serial_owner && !grant.trimmed => {
            let worker = worker_floor(pipe, floor, width);
            let (raised, raised_w) = crate::mem::feedback::raise(program.key.0, b.slot, floor, worker);
            // a learned start never passes the kind's PostgreSQL budget (work_mem, hash_mem: `floor_for`): past it the
            // breaker would hold in memory what PostgreSQL spills (a sort's runs under a temp_file_limit PostgreSQL
            // raises at). A grant with no budgets (0) keeps the raise as learned
            if budget == 0 { (raised, raised_w) } else { (raised.min(budget.max(floor)), raised_w.min(worker_floor(pipe, budget, width).max(worker))) }
        }
        _ => (floor, worker_floor(pipe, floor, width)),
    };
    let (live, width) = (program.slots.share_live(pipe.produces()), width.max(1));
    match (ceiling_of(grant, pipe), pipe.breaker()) {
        (c, Some(b)) if c != 0 => grant.split_capped(live, width, images, floor, wf, c, leader_ceiling(c, width, b.kind)),
        _ => grant.split_within(live, width, images, floor, wf, top_share_cap(pipe)),
    }
}

/// Grow mode's shares (`Grant::grow`): the grant's split (the estimate start), no floor, each participant's at most its
/// node's PostgreSQL budget (`floor_for`: under grow mode the grant's floors carry the budgets, `with_floors`, as caps of the
/// start), the leader's at the budget's leader ceiling, and at most the breaker's ceiling: never past the share today's
/// admission would give it, so the start's reservation is never past today's. No learned raise: what today's raise would
/// claim at admission, grow mode leaves to growth, which the ceiling bounds (`ceiling_of`: the region wall with work_mem
/// unset) and the pool's room refuses.
fn grow_share(program: &gravity_program::Program, pipe: &gravity_program::Pipeline, grant: Grant, width: u16, images: bool) -> Split {
    let (live, width) = (program.slots.share_live(pipe.produces()), width.max(1));
    let Some(b) = pipe.breaker() else { return grant.split_within(live, width, images, 0, 0, top_share_cap(pipe)) };
    let cap = match (floor_for(grant, pipe), ceiling_of(grant, pipe)) {
        (0, c) => c,
        (f, 0) => f,
        (f, c) => f.min(c),
    };
    if cap == 0 {
        return grant.split_within(live, width, images, 0, 0, top_share_cap(pipe));
    }
    grant.split_capped(live, width, images, 0, 0, cap, leader_ceiling(cap, width, b.kind))
}

/// Whether pipeline `p` (`pipe`) ends in a Materialize breaker whose rows go to an exec Spool object: a
/// `ProtoKind::Spool` protocol peered to it as its fill (lowerv4 rerun/spool rec.rs `declare`: a hash join's drained probe,
/// nest.spool's Material, a rerun set's spool; exec ops/material). Its slot holds no row (its region is armed, its header
/// written), so each participant's share is `SHARE_FLOOR` and admission reserves that, not work_mem. A slot-run spool (its
/// rows in the slot as a sort run, rowsort `spool_full`, no Spool protocol) keeps its share: DESIGN.md's named debt.
pub(crate) fn spooled(program: &gravity_program::Program, p: usize, pipe: &gravity_program::Pipeline) -> bool {
    use gravity_program::protos::ProtoKind;
    pipe.breaker().is_some_and(|b| b.kind == gravity_abi::breaker::BreakerKind::Materialize)
        && program.v4.as_deref().is_some_and(|v| v.protos.iter().any(|x| x.kind == ProtoKind::Spool && x.peer as usize == p))
}

/// The most one participant's region of `pipe`'s breaker holds before its spill road runs (`Grant::ceil_of`: work_mem, x
/// hash_mem_multiplier for a hash node), held to a row top's bounded region (`top_share_cap`) and to the largest region;
/// 0 = none: a grant without ceilings, a plain aggregate, a sink, and a breaker with no spill road (`Spill::None`: a set
/// operation side, as PostgreSQL's hashed SetOp, grows past work_mem; refusing it would fail the statement), whose grows
/// keep their roads.
pub fn ceiling_of(grant: Grant, pipe: &gravity_program::Pipeline) -> u64 {
    let c = match pipe.breaker() {
        Some(b) if b.spill != gravity_abi::breaker::Spill::None => grant.ceil_of(b.kind),
        _ => 0,
    };
    mempolicy::grow::ceiling(c, top_share_cap(pipe), crate::mem::grow::MAX_REGION)
}

/// The leader region's ceiling for a breaker of `kind` whose participant ceiling is `c` (`ceiling_of`), at `width`, as
/// PostgreSQL budgets the node the leader's region stands for: a hash aggregate's or a hashed DISTINCT's final table is
/// the leader's Finalize node, one hash_mem (`c`); a parallel hash build is Parallel Hash's shared table, hash_mem x its
/// participants, and a Sort's, top-N's or Material's merge holds the participants' runs, each its own work_mem: (width +
/// 1) x `c` at width >= 2, at most the largest region. Serially `c`.
pub fn leader_ceiling(c: u64, width: u16, kind: gravity_abi::breaker::BreakerKind) -> u64 {
    use gravity_abi::breaker::BreakerKind::*;
    mempolicy::grow::leader_ceiling(c, width, matches!(kind, HashAgg | Distinct), crate::mem::grow::MAX_REGION)
}

/// The first bytes a breaker region that starts small commits: the memory policy's (`mempolicy::grow::START_BYTES`).
pub use mempolicy::grow::START_BYTES;

/// `PGRUST_GRAVITY_START` (developer, read once per process), the A/B override of `decide_start`: `share` = every
/// breaker region commits its whole share at its open, as before start-small; `small` = every statement's hash breakers
/// start small, as E1 had it; unset or anything else = `decide_start` by the pool.
pub fn start_forced() -> Option<StartSize> {
    static FORCED: pgsync::OnceLock<Option<StartSize>> = pgsync::OnceLock::new();
    *FORCED.get_or_init(|| match crate::env::text("PGRUST_GRAVITY_START").as_deref().map(str::trim) {
        Some("share") => Some(StartSize::Share),
        Some("small") => Some(StartSize::Small),
        _ => None,
    })
}

/// D3798 S2: the start of a statement about to be admitted now (`decide_start`): the statements in flight on other
/// sessions (the pool's in-flight slots less this thread's own: a statement's nested levels and its suspended portals never
/// count). Traced as `admit.start` under `PGRUST_MEMPOLICY=trace=1`.
pub fn start_for() -> StartSize {
    start_for_in(false)
}

/// `start_for` under grow mode (`grow`: `mempolicy::admit::decide_start_in`, small beside others too).
pub fn start_for_in(grow: bool) -> StartSize {
    let others = POOL.inflight().saturating_sub(SLOTS_HERE.with(core::cell::Cell::get) as u64);
    mempolicy::admit::start(others, running(), start_forced(), grow)
}

/// D4078: the one-owner wall of a statement about to be admitted now (`mempolicy::admit::owner_wall`): half the work pool's
/// row cap when the broker re-arms the pool every tick (`budget::pool_follows`, `budget::row_cap`), else half the pool's
/// armed big lane (its capacity less the small lane's slice); at most the format's `section::OWNER_WALL`, the format's
/// when the pool is unarmed. Read once per admission into its `Grant::owner_wall`, never again while it runs.
pub fn owner_wall_for() -> u64 {
    let follows = crate::mem::budget::pool_follows();
    let row_cap = if follows { crate::mem::budget::row_cap() } else { 0 };
    mempolicy::admit::owner_wall(follows, row_cap, POOL.capacity().saturating_sub(POOL.reserve()), crate::section::OWNER_WALL as u64)
}

/// Whether `pipe`'s breaker region starts small under `grant` (its statement's `Grant::start`): `may_start_small` and a
/// small start, or a cascaded grouping set (`cascade_starts_small`).
pub fn starts_small(pipe: &gravity_program::Pipeline, grant: Grant) -> bool {
    (grant.start == StartSize::Small && may_start_small(pipe)) || cascade_starts_small(pipe)
}

/// D4620: a cascaded grouping set (`Pipeline::cascade`, road agg.gsets.cascade) starts small under any statement start and
/// whatever its spill road (a record holding key copies is resident, `Spill::None`): it folds its parent's records, a few
/// or millions, whatever its share of the node's estimate, so its region commits what its owners take (`owners_in_place`)
/// rather than its share at the open.
pub fn cascade_starts_small(pipe: &gravity_program::Pipeline) -> bool {
    use gravity_abi::breaker::{BreakerKind, Budget};
    pipe.cascade && start_forced() != Some(StartSize::Share) && pipe.breaker().is_some_and(|b| b.budget == Budget::Share && b.kind == BreakerKind::HashAgg)
}

/// Whether `pipe`'s breaker region can start small: a partitioned hash aggregate or DISTINCT (its T1 is laid out at the
/// reservation, `RESERVE_LAYOUT`), a hash build, or a full sort (its run doubles in place at each full before it is put:
/// `SPILL_REGION_PLACE`), budgeted by the share. Its region then commits `start_bytes` and doubles in place up to its
/// reservation (`grow::start_reserve`); the share stays its layout. A table with no spill road (`Spill::None`: a top-k's
/// bounded run too) or a set operation side (`Budget::Pool`) keeps its whole share committed.
pub fn may_start_small(pipe: &gravity_program::Pipeline) -> bool {
    use gravity_abi::breaker::{BreakerKind, Budget, Spill};
    start_forced() != Some(StartSize::Share)
        && pipe.breaker().is_some_and(|b| {
            b.budget == Budget::Share
                && match b.kind {
                    BreakerKind::HashBuild => true,
                    BreakerKind::HashAgg | BreakerKind::Distinct => matches!(b.spill, Spill::Partitioned { .. }),
                    // a full sort's run grows in place at its full before it is put (kernels sort `sort_grow`)
                    BreakerKind::Sort => b.spill == Spill::Runs,
                    _ => false,
                }
        })
}

/// `starts_small` for pipeline `p` of `program` as its code runs it. A hand kernel (`Code::is_hand`: the lab programs the
/// kernel and parallel tests drive) lays its table out at the region's committed limit and never asks the reservation's
/// layout (`RESERVE_LAYOUT`), so a region that started small would spill at its start where its share holds the groups:
/// it opens at its share. Every lowered program's table asks the layout and grows in place.
pub fn starts_small_in(program: &gravity_program::Program, p: usize, grant: Grant) -> bool {
    ((grant.start == StartSize::Small && may_start_small(&program.pipelines[p])) || cascade_starts_small(&program.pipelines[p])) && !program.code_of(p).is_hand()
}

/// `may_start_small` for pipeline `p` of `program` as its code runs it (a hand kernel opens at its share).
pub fn may_start_small_in(program: &gravity_program::Program, p: usize) -> bool {
    may_start_small(&program.pipelines[p]) && !program.code_of(p).is_hand()
}

/// The bytes a started-small region of share `share` commits at its open: `START_BYTES`, at least four phase-1 tables'
/// worth (a T1's table, side words and chain words must fit the start past its header), never past the share.
pub fn start_bytes(share: u32) -> u32 {
    mempolicy::grow::start_bytes(share, gravity_abi::breaker::t1_default_bytes() as u64)
}

/// exactsize D3215: the least share each of the `width` workers of a partitioned record table (a hash aggregate's or a
/// hashed DISTINCT's T1 and block pool) gets: the records they hold (D365's `held`, every worker's together) cut `width`
/// ways, at least `FLOOR_CHARGED_MIN`, never past `floor` (its node's PostgreSQL budget). A worker holds its own claims'
/// records until the owner phase, so the node's whole floor per worker reserved W x hash_mem past the estimate's grant
/// (a near-unique GROUP BY of two keys at W15 and a 512 MiB hash_mem: 11.5 GB on a 2.5 GB grant, then 53200). Past it the
/// worker spills its partitions.
/// Any other breaker, and a serial open: `floor`.
pub(crate) fn worker_floor(pipe: &gravity_program::Pipeline, floor: u64, width: u16) -> u64 {
    use gravity_abi::breaker::{BreakerKind, Spill};
    match pipe.breaker() {
        Some(b) if width >= 2 && matches!(b.kind, BreakerKind::HashAgg | BreakerKind::Distinct) && matches!(b.spill, Spill::Partitioned { .. }) => {
            let held = gravity_program::cost::breaker::of(pipe).map_or(0, |c| c.held);
            floor.min((held / width as u64).max(FLOOR_CHARGED_MIN))
        }
        _ => floor,
    }
}

/// A breaker's PostgreSQL floor (D2587, `Grant::floor_of`) held to its bounded region (`top_share_cap`): the floor both
/// the one divisor (`share_with`) and the floor's reservation (`grow::floor_reserve`) read; 0 for a sink.
pub(crate) fn floor_for(grant: Grant, pipe: &gravity_program::Pipeline) -> u64 {
    pipe.breaker().map_or(0, |b| grant.floor_of(b.kind).min(top_share_cap(pipe)))
}

/// The rounding of a row top's bounded region: the transparent huge page a region of 2 MiB or more asks for (`advise_huge`).
const TOP_SHARE_STEP: u64 = 2 << 20;

/// A row top's region (a Sort breaker whose `Pipeline::top_rows` lowerv4 runs.topk_rows published: the rows its run holds
/// before it cuts): those rows of its record (`cost::breaker::record`) and as many bytes again for the rows' variable-length
/// copies, which grow down from the region's end, page-rounded, then rounded up to `TOP_SHARE_STEP`; `u64::MAX` (no cap)
/// for any other pipeline. Each participant's share is at most it (`split_within`). Past it the run puts its runs
/// (`Spill::Runs`), so copies longer than the record cost a spill, never a row.
pub(crate) fn top_share_cap(pipe: &gravity_program::Pipeline) -> u64 {
    match pipe.breaker() {
        Some(b) if b.kind == gravity_abi::breaker::BreakerKind::Sort && pipe.top_rows != 0 => {
            let bytes = pipe.top_rows.saturating_mul(gravity_program::cost::breaker::record(pipe)).saturating_mul(2).min(u32::MAX as u64);
            (crate::park::region_bytes(bytes as usize) as u64).next_multiple_of(TOP_SHARE_STEP)
        }
        _ => u64::MAX,
    }
}

/// D3088: the part of a hash breaker's share `share_with` charged for its kind's PostgreSQL floor (`charged_floor`) at
/// `grant`; 0 for any other pipeline. `Node::share_left`'s cut keeps at least it: a hash table cut under it commits its
/// floor reservation in place as it fills (D2894), so the cut saves nothing and its doublings overshoot. A Sort, top-N or
/// Material run spills at its armed limit instead (no commit asked), so its cut stands as D2242 had it.
pub(crate) fn charged_share(pipe: &gravity_program::Pipeline, grant: Grant) -> u64 {
    use gravity_abi::breaker::BreakerKind::*;
    match pipe.breaker() {
        Some(b) if matches!(b.kind, HashAgg | HashBuild | Distinct) => charged_floor(grant.floor_of(b.kind), b.kind, pipe, grant),
        _ => 0,
    }
}

/// D2242's cut of an on-demand open's `share` to `left` (the grant's room split with the members still to open), at least
/// `SHARE_FLOOR` and (D3088) at least `charged` (`charged_share`), never past `share`; 8-aligned.
pub(crate) fn cut_share(share: Share, left: u64, charged: u64) -> Share {
    Share { bytes: share.bytes.min(left.max(SHARE_FLOOR as u64).max(charged).min(u32::MAX as u64) as u32) & !7 }
}

/// D2894: a breaker's PostgreSQL floor (D2587) up to what its shape is modeled to fill, `cost::breaker::of`'s largest table
/// or held records: PostgreSQL takes hash_mem as it fills, a share is mapped and reserved whole. At work_mem 512MB x
/// hash_mem_multiplier 8 every hash breaker took a 4 GiB region: a five-table join under a top-N asked 364 MiB and
/// reserved 17 GiB, and a six-table join under a GROUP BY drew 4 GiB past its 16 GiB with the pool held by the statements
/// beside it (53200). Width-free: a serial open (width 1) and the footprint (the statement's width) read one leader share.
///
/// The floor past the charged share stays the region's capacity (`grow::floor_reserve`): address space it commits in place
/// as it fills, so a breaker the model under-counts holds what PostgreSQL's node holds before it spills. A floor of
/// `FLOOR_CHARGED_MIN` or less (every floor at the default work_mem) is charged whole, as D2587 had it.
///
/// Hash kinds only (a hash aggregate, a hash build, a hashed DISTINCT: work_mem x hash_mem_multiplier, the 4 GiB regions):
/// their tables commit their reservation in place as they fill (`region_grow` / `grow_in` / T1's `SPILL_REGION_COMMIT`). A
/// Sort, top-N or Material run spills at its armed limit with no commit asked, so its floor (work_mem) stays charged whole
/// as D2587 had it, until its kernels ask for a commit before they put a run.
///
/// exactsize B1c / D3205: `grant`'s exact sizing reads the breaker as the ask does (`cost::breaker::of_at`: a hash build's
/// records alone, a serial pipeline's breaker by the rows at its end).
fn charged_floor(floor: u64, kind: gravity_abi::breaker::BreakerKind, pipe: &gravity_program::Pipeline, grant: Grant) -> u64 {
    use gravity_abi::breaker::BreakerKind::*;
    match kind {
        HashAgg | HashBuild | Distinct => gravity_program::cost::breaker::of_at(pipe, grant.exact_build(), grant.exact_rows()).map_or(floor, |c| floor.min(c.table.max(c.held).max(FLOOR_CHARGED_MIN))),
        Sort | TopK | Materialize | PlainAgg => floor,
    }
}

/// The region bytes a grant maps: the leader's `live` share regions as the depot maps them + `width` worker shares.
pub fn regions_of(grant: Grant, live: u16, width: u16) -> u64 {
    let split = grant.split(live, width);
    let leader = crate::park::region_bytes(split.breaker.bytes as usize) as u64;
    leader * live.max(1) as u64 + split.worker.bytes as u64 * width as u64
}

/// The region bytes `program` draws at `grant` over `width` (D192): the peak of the live leader slots (each freed after its last
/// consumer) and worker buffers, the `Accounting::limits` a Node running it reads (S2).
pub fn footprint(grant: Grant, program: &gravity_program::Program, width: u16) -> u64 {
    let (regions, pinned) = footprint_parts(grant, program, width);
    regions + pinned
}

/// `footprint`'s region bytes alone: the shares and program-constant regions, before D193's arena allowance and serial pins.
pub fn footprint_regions(grant: Grant, program: &gravity_program::Program, width: u16) -> u64 {
    footprint_parts(grant, program, width).0
}

fn footprint_parts(grant: Grant, program: &gravity_program::Program, width: u16) -> (u64, u64) {
    use gravity_abi::breaker::Budget;
    let plan = &program.slots;
    // D4615: this call's scratch (the slots' held bytes and due pipelines, the arms runs) lives in one parentless bump
    // context, charged to the calling thread's context owner (portal, else statement, else session), dropped on return
    let scratch = mcx::MemoryContext::new_bump("gravity_footprint"); // alloc: Cold
    let mut held: mcx::PgVec<'_, u64> = mcx::PgVec::with_capacity_in(plan.nslots(), scratch.mcx()); // alloc: Exec
    held.resize(plan.nslots(), 0); // alloc: Exec
    // per slot, the pipeline after which it frees: its latest producer's last consumer (an earlier producer's last consumer
    // comes before the slot's next producer, so one per slot is due at a time)
    let mut due: mcx::PgVec<'_, usize> = mcx::PgVec::with_capacity_in(plan.nslots(), scratch.mcx()); // alloc: Exec
    due.resize(plan.nslots(), usize::MAX); // alloc: Exec
    let (mut leader, mut share_w, mut fixed_w, mut peak) = (0u64, 0u64, 0u64, 0u64);
    let parallel = |pipe: &gravity_program::Pipeline| width >= 2 && pipe.parallel == gravity_program::Parallel::Eligible;
    let mut pinned = arena_allowance(program);
    let kept = rerun_kept(program);
    let arms = crate::lives::ArmRuns::of_in(program, scratch.mcx());
    let images = crate::node::program_images(program);
    let wall = grant.owner_wall_bytes();
    for (p, pipe) in program.pipelines.iter().enumerate() {
        if let Some(b) = pipe.breaker() {
            let s = b.slot as usize;
            let split = share_with(program, p, grant, width, images);
            let region = |bytes: u64| crate::park::region_bytes(bytes as usize) as u64;
            // a leader slot counts its cut region and the images its pins charge; a worker its uncut share (`Counters::limit`)
            let opened = if plan.shared[s] { region(split.breaker.bytes as u64) + split.images.bytes as u64 } else { region(plan.bytes[s] as u64) };
            // a region that starts small (`starts_small`) counts its start, what its open commits; its growth is charged as
            // it commits. The owner pins below read the share as before. A full sort's region counts its share, as before it
            // started small: its runs grow in place within what admission reserved for them, so a merge join's Sort inner
            // grows within W and G and puts its run past them (D3105 (b)); counted at its start, the inner's growth passed
            // the grant whenever the statement's other regions had taken G (rowexecfix_d3105_b at W8: 9-39 MiB grown)
            let small = plan.shared[s] && starts_small_in(program, p, grant) && b.kind != gravity_abi::breaker::BreakerKind::Sort;
            let counted = if small { region(start_bytes(split.breaker.bytes) as u64) + split.images.bytes as u64 } else { opened };
            // D193: a serial owner pins its fold beside the slot it reads; a parallel one folds in the idle leader region (`own_single`).
            // An append.arms run's arms all produce one slot and only its last arm runs that fold (D393b), so the pin is counted once
            // per run: per arm, 4150 hash partitions' arms pinned 296 GB at the floor grant and the statement refused (crt-0384).
            // DR-11: the pin's 4 MiB beside twice the slot's bytes is MEASURED-HELDOUT (shapecheck sweep-DR-11.tsv: the held-out
            // grouping corpus with its estimate misses and the bench set at W0, where every owner is serial and the pin is in
            // every footprint, a 256 MiB pool with four statements in flight, 1 / 4 / 16 MiB): the 78 statements every arm served
            // ran 16204-16600 ms (2.4 %, not monotonic), the bench 2990-3050 ms; refused 15 / 16 / 16, 14 of them in every arm
            // D2845: and at least the records' bound the breaker cost holds, the region its owner phase lays (`own_all`)
            if pipe.owner.is_some() && !parallel(pipe) && !arms.continues(p) {
                let bytes = if plan.shared[s] { split.breaker.bytes as u64 + split.images.bytes as u64 } else { plan.bytes[s] as u64 };
                // D4078: at most the statement's one-owner wall (`Grant::owner_wall`), the region past which its owner
                // splits (`own_split`) and a chunk the pool refuses is a temp file (D491), never the format's ~4 GiB past it
                pinned += (4u64 << 20).saturating_add(bytes.saturating_mul(2)).max(owner_bound(pipe, wall)).min(wall);
            }
            // DR-12 (D285): a parallel owner whose records outgrow the idle leader region pins them past G at their bound
            // (`own_all`), reserved here at the records the breaker cost holds (D365's `held`)
            if pipe.owner.is_some() && parallel(pipe) && !arms.continues(p) {
                pinned += owner_past_leader(pipe, opened, wall);
            }
            leader = leader + counted - core::mem::replace(&mut held[s], counted);
            if parallel(pipe) {
                let worker = if small { start_bytes(split.worker.bytes) } else { split.worker.bytes };
                match b.budget {
                    Budget::Share | Budget::Pool => share_w = share_w.max(worker as u64 + split.images.bytes as u64),
                    Budget::Fixed(f) => fixed_w = fixed_w.max(f as u64),
                }
            }
            peak = peak.max(leader + (share_w + fixed_w) * width as u64);
            due[s] = program.last_consumer(p);
        }
        for (s, d) in due.iter_mut().enumerate() {
            if *d == p {
                *d = usize::MAX;
                if !kept[s] {
                    leader -= core::mem::take(&mut held[s]);
                }
            }
        }
    }
    if program.pipelines.iter().any(parallel) {
        pinned += worker_arena_for(program, grant, width) * width as u64;
    }
    (peak, pinned + fmgr_sites(program) * FMGR_SITE_ALLOWANCE)
}

/// M8 (RT-2e, D1430 / D1431): the slots a rerun set keeps for the statement, by slot: each member's own and each slot a
/// member consumes (`Node::defer_rerun` marks them `rerun_kept`, and `free_after` never frees them: a member re-runs inside
/// its owner's pipeline, after its last consumer in program order). Released at a slot's last consumer they left the
/// owner's breaker short: a kept VALUES build's 32 MiB share beside the owner's Sort share against a 36 MiB reservation,
/// so the Sort drew past the grant (a fresh zeroed region and a ledger L1 every statement, W8 1.10 vs W0 0.30 ms).
/// Held to the statement's end as the runtime holds them (`settle_regions`); every slot a member consumes, as `skip_rerun`
/// keeps them (`every`). A pulled set (`skip_pulled`, freed after its readers) has no Rerun proto and is not counted here.
fn rerun_kept(program: &gravity_program::Program) -> Vec<bool> {
    let mut kept = vec![false; program.slots.nslots()]; // alloc: Cold
    let Some(v4) = program.v4.as_deref() else { return kept };
    for m in v4.protos.iter().filter(|x| x.kind == gravity_program::protos::ProtoKind::Rerun).map(|x| x.pipeline as usize) {
        let Some(pipe) = program.pipelines.get(m) else { continue };
        for s in pipe.consumes.iter().copied().chain(pipe.produces()) {
            if let Some(k) = kept.get_mut(s as usize) {
                *k = true;
            }
        }
    }
    kept
}

/// D193: the leader arena's room in every statement's footprint: admission's (`mempolicy::admit`).
pub use mempolicy::admit::ARENA_ALLOWANCE;

/// DR-12: the region a parallel owner phase pins past the idle leader region of `opened` bytes (`section::own_single`: the
/// one owner's records' bound, `one_owner_bytes` over the records the breaker cost says the workers hold, D365), at most
/// `wall` (the statement's, `Grant::owner_wall_bytes`); 0 when it fits the leader region or the breaker holds no records
/// (an unpartitioned one).
pub(crate) fn owner_past_leader(pipe: &gravity_program::Pipeline, opened: u64, wall: u64) -> u64 {
    match owner_bound(pipe, wall) {
        need if need <= opened => 0,
        need => need,
    }
}

/// D2845: an owner phase's records' bound (`one_owner_bytes` over the records the breaker cost holds, D365), at most
/// `wall` (D4078: the statement's, `Grant::owner_wall_bytes`); 0 for a breaker that holds no records (an unpartitioned one).
pub(crate) fn owner_bound(pipe: &gravity_program::Pipeline, wall: u64) -> u64 {
    match gravity_program::cost::breaker::of(pipe) {
        Some(c) if c.held > 0 => (crate::section::one_owner_bytes(c.held as usize, c.record as usize) as u64).min(wall),
        _ => 0,
    }
}
/// A worker's arena in G, carved into its W at bind (S4, D187): grant / 8W, clamped to [3, 4] MiB.
/// MEASURED-HELDOUT (planner-61, full e2e W8 rig, 14,555 parallel sections W2..W16): arena peak p50 0, p99 1.4 MiB,
/// p99.9 14 MiB, max 31 MiB; 0.5% pass 3 MiB and they reserve past W and G as any section does. The 3 MiB floor covers
/// the p99 at every width, so grant / 8W alone would under-reserve small grants into pool trips.
pub fn worker_arena(grant: Grant, width: u16) -> u64 {
    (grant.bytes / (8 * width.max(1) as u64)).clamp(3 << 20, 4 << 20)
}

/// DR-12 (mechanism R: the estimate opens, the run time re-checks): the arena draws each program was SEEN to make, per
/// program content key, a process-wide direct-mapped table (one word a slot: a 16-bit key tag, the leader's grow-chunk
/// bytes past its pins and the widest worker arena, each in KiB, 24 bits). The measured priors (`ARENA_ALLOWANCE`,
/// `worker_arena`) are the least reserved; a program whose run drew past them reserves that draw at its next run.
const SEEN_SLOTS: usize = 4096;
static ARENA_SEEN: [AtomicU64; SEEN_SLOTS] = [const { AtomicU64::new(0) }; SEEN_SLOTS];

fn seen_slot(key: gravity_program::ContentKey) -> (&'static AtomicU64, u64) {
    let h = (key.0 as u64) ^ ((key.0 >> 64) as u64);
    (&ARENA_SEEN[(h as usize) % SEEN_SLOTS], (h >> 48) | 1)
}

/// DR-12: what `key`'s runs drew, `(leader, worker)` bytes, if one ran in this process (and its slot is still its own).
pub fn arena_seen(key: gravity_program::ContentKey) -> Option<(u64, u64)> {
    let (slot, tag) = seen_slot(key);
    let w = slot.load(Ordering::Relaxed);
    (w >> 48 == tag).then(|| ((w >> 24 & 0xFF_FFFF) << 10, (w & 0xFF_FFFF) << 10))
}

/// DR-12: forget every program's seen arena draws (the priors stand again): a re-fit, and tests that pin the priors.
pub fn forget_arena_draws() {
    ARENA_SEEN.iter().for_each(|w| w.store(0, Ordering::Relaxed));
}

/// DR-12: note a run of `key` that drew `leader` bytes of leader grow chunks and `worker` bytes in its widest worker arena
/// (the max with what was seen; another key's slot is taken over).
pub fn note_arena(key: gravity_program::ContentKey, leader: u64, worker: u64) {
    let (slot, tag) = seen_slot(key);
    let kib = |b: u64| b.div_ceil(1024).min(0xFF_FFFF);
    let _ = slot.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |w| {
        let (l0, w0) = if w >> 48 == tag { (w >> 24 & 0xFF_FFFF, w & 0xFF_FFFF) } else { (0, 0) };
        Some(tag << 48 | kib(leader).max(l0) << 24 | kib(worker).max(w0))
    });
}

/// DR-12: the leader arena room `program`'s footprint pins: the prior `ARENA_ALLOWANCE`, or what its runs drew when that
/// was more (a run past the prior reserves its draw next time instead of drawing past its reservation again).
pub fn arena_allowance(program: &gravity_program::Program) -> u64 {
    arena_seen(program.key).map_or(ARENA_ALLOWANCE, |(l, _)| l.max(ARENA_ALLOWANCE))
}

/// DR-12: a worker's arena carved from G for `program`: the prior `worker_arena`, or what its widest worker drew when that
/// was more.
pub fn worker_arena_for(program: &gravity_program::Program, grant: Grant, width: u16) -> u64 {
    let prior = worker_arena(grant, width);
    arena_seen(program.key).map_or(prior, |(_, w)| w.max(prior))
}

/// One fmgr-tier call site's result room in G: admission's (`mempolicy::admit`).
pub use mempolicy::admit::FMGR_SITE_ALLOWANCE;

/// The program's fmgr-tier call sites: 0 until v3's compiled program counts them (swap for `program.fmgr_sites`).
pub fn fmgr_sites(_program: &gravity_program::Program) -> u64 {
    0
}

/// `regions_of` whether or not the program pins group images (each cut comes out of its own share).
pub fn regions_for(grant: Grant, live: u16, width: u16, _images: bool) -> u64 {
    regions_of(grant, live, width)
}

impl GrantPool {
    pub fn arm(&self, capacity: u64) {
        // ladder C1: the ladder's room under its line counts the process pool's draws since its sample
        mcx::ladder::set_pool_held(pool_held);
        // and (C0's finding 1) the cache bytes past their floors the ladder cannot free count as held
        mcx::ladder::set_caches_stuck(crate::mem::reclaim::c1_caches_stuck);
        let before = self.capacity.swap(capacity, Ordering::AcqRel);
        self.clamp_buffer();
        // ladder step 1.1: a capacity that rose is room for the parked draws (the epoch moves and they wake)
        if capacity > before && policy::gate_now() {
            mcx::ladder::note_free();
        }
    }

    pub fn gate(&self, max_inflight: u64) {
        self.max_inflight.store(max_inflight, Ordering::Release);
    }

    /// The small lane's slice (0 = no lane); the big lane's capacity is what remains. DR-13 (keep): the slice is at most half
    /// the capacity, so the big lane always keeps the other half: a fraction of the pool, not a size fitted to a load.
    /// D4403: a small ask the slice and the big lane both refuse waits (`admit_small_until`); the slice is the lanes mode's
    /// small buffer, set once here and not re-clamped.
    pub fn set_reserve(&self, bytes: u64) {
        self.buffer.store(0, Ordering::Release);
        self.reserve.store(bytes.min(gravity_program::cost::fraction(self.capacity(), 1, 2)), Ordering::Release);
    }

    /// D4403: the small buffer of every grant mode but lanes (`policy::small_buffer_default` on a derived pool; an explicit
    /// capacity has none unless `pgrust.gravity_grant_small_reserve_mb` sets one): a part of the capacity no big admission
    /// (a Solo or alone floor, a halved grant, a draw past G) ever takes, that small statements' reservations
    /// (`policy::small_statement`) and draws with no reservation (`policy::small_draw`) take first. `bytes` clamped by
    /// `clamp_buffer`; 0 = none.
    pub fn set_buffer(&self, bytes: u64) {
        self.buffer.store(bytes, Ordering::Release);
        if bytes == 0 {
            self.reserve.store(0, Ordering::Release);
        }
        self.clamp_buffer();
    }

    /// D4403: B as the budget read it, mode on (0: off or advise, no B clamp); the buffer is re-clamped to it.
    pub(crate) fn note_b(&self, b: u64) {
        if self.buffer_b.swap(b, Ordering::AcqRel) != b {
            self.clamp_buffer();
        }
    }

    /// D4403: `reserve` = the configured buffer clamped by `policy::small_buffer` (a sixteenth of the capacity and, budget
    /// mode on, of B): buffer takes skip B's make_room and the memory governor's bar, so their holders sit past B by at
    /// most a sixteenth of it (inside the room B leaves outside itself); nothing changes at 8 GiB and up. No buffer
    /// configured (the lanes slice, or none): untouched.
    fn clamp_buffer(&self) {
        let bytes = self.buffer.load(Ordering::Acquire);
        if bytes == 0 {
            return;
        }
        self.reserve.store(policy::small_buffer(bytes, self.capacity(), self.buffer_b.load(Ordering::Acquire)), Ordering::Release);
    }

    pub fn reserve(&self) -> u64 {
        self.reserve.load(Ordering::Acquire)
    }

    /// D2990: the most one statement holds in the pool, its reservation and its growth past G together: `ppm` parts per
    /// million of the big lane (the capacity less the small lane's slice). The default, half (`POOL_FRACTION_DEFAULT`), is
    /// DR-7's bound for one statement (exec knobs/grant.rs `raise_within`: the statements beside it keep the other half),
    /// CHOSEN, not measured, as that bound is. 0 (no cap) when the pool is unarmed or `ppm` is the whole lane.
    pub fn statement_cap(&self, ppm: u32) -> u64 {
        policy::statement_cap(self.capacity().saturating_sub(self.reserve()), ppm)
    }

    pub fn small_held(&self) -> u64 {
        self.small_held.load(Ordering::Acquire)
    }

    pub fn inflight(&self) -> u64 {
        self.inflight.load(Ordering::Acquire)
    }

    pub fn max_inflight(&self) -> u64 {
        self.max_inflight.load(Ordering::Acquire)
    }

    /// One of the in-flight slots, if the gate allows: `Some(true)` taken, `Some(false)` this thread already holds one (a second
    /// statement on it would wait on itself), `None` the gate is full.
    fn take_slot(&self) -> Option<bool> {
        if SLOTS_HERE.with(|s| s.get()) > 0 {
            return Some(false);
        }
        let max = self.max_inflight();
        let mut cur = self.inflight.load(Ordering::Acquire);
        loop {
            if max != 0 && cur >= max {
                return None;
            }
            match self.inflight.compare_exchange_weak(cur, cur + 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => {
                    SLOTS_HERE.with(|s| s.set(s.get() + 1));
                    return Some(true);
                }
                Err(now) => cur = now,
            }
        }
    }

    pub fn capacity(&self) -> u64 {
        self.capacity.load(Ordering::Acquire)
    }

    pub fn held(&self) -> u64 {
        self.held.load(Ordering::Acquire)
    }

    /// `held` less the kept holds and the parked regions: what the statements in flight hold, the bytes a wait can see come
    /// back. D3254: a parked region is no statement's (an admission trims what it needs of them, `trim_depots`), so an ask
    /// that halves beside nothing else held halves at once, as it did when admission dropped the whole depot first.
    pub fn held_by_statements(&self) -> u64 {
        self.held().saturating_sub(self.kept.load(Ordering::Acquire)).saturating_sub(crate::park::REGIONS.parked_bytes() as u64)
    }

    /// D4403: `held_by_statements` less the small buffer's holders (small statements' reservations, draws with no
    /// reservation): bytes no big admission could take back when they end, so a Solo or alone floor (and a big ask's
    /// "nothing to wait for") reads the big lane alone; a trickle of small statements never starves a Solo.
    pub fn held_by_big_statements(&self) -> u64 {
        self.held_by_statements().saturating_sub(self.small_held())
    }

    pub fn stats(&self) -> PoolStats {
        PoolStats { capacity: self.capacity(), held: self.held(), inflight: self.inflight(), max_inflight: self.max_inflight(), waits: self.waits.load(Ordering::Relaxed), wait_ms: self.wait_ns.load(Ordering::Relaxed) / 1_000_000, trims: self.trims.load(Ordering::Relaxed), sheds: self.sheds.load(Ordering::Relaxed), refusals: self.refusals.load(Ordering::Relaxed), reserve: self.reserve(), small_held: self.small_held(), small_admits: self.small_admits.load(Ordering::Relaxed), small_fallbacks: self.small_fallbacks.load(Ordering::Relaxed), small_overflows: self.small_overflows.load(Ordering::Relaxed), small_draws: self.small_draws.load(Ordering::Relaxed), small_waits: self.small_waits.load(Ordering::Relaxed), small_refusals: self.small_refusals.load(Ordering::Relaxed), borrows: self.borrows.load(Ordering::Relaxed), borrowed: self.borrowed.load(Ordering::Relaxed), unadmitted: self.unadmitted.load(Ordering::Relaxed), unadmitted_bytes: self.unadmitted_bytes.load(Ordering::Relaxed) }
    }

    /// D622 (A): a whole admission past the pool, `want` reserving `need` (its `footprint`), taken at once through
    /// `overdraw_take`, never refused and never waiting, with no in-flight slot: a nested statement's site whose grant does
    /// not fit beside its caller's runs at its floor as PostgreSQL runs the nested statement at work_mem (it has no pool). Its
    /// G gives `need` back when the statement's last holder goes, as any admission's. Bound: the nesting depth cap x the sites
    /// x the floor per backend.
    pub fn admit_overdrawn(&self, want: Grant, need: u64, site: Overdraw) -> Reservation {
        self.overdraw_take(need, site);
        Reservation::new(want, want, need, 0, false, false, Admitted::Overdrawn)
    }

    /// Parks yield when more than half the capacity is held (the parked bytes are a previous statement's);
    /// gated (`max_inflight` != 0) they yield always (`Park::put`).
    pub fn over_park_budget(&self) -> bool {
        let cap = self.capacity();
        cap != 0 && self.held() > cap / 2
    }

    pub(crate) fn note_shed(&self) {
        self.sheds.fetch_add(1, Ordering::Relaxed);
    }

    /// `bytes` more held, if the big lane allows: the capacity less the slice, plus what the slice holds (its bytes are in `held`).
    pub(crate) fn take(&self, bytes: u64) -> bool {
        self.take_leaving(bytes, 0, false)
    }

    /// D3147: `take` for a draw that spills on a refusal (no wait): the idle bytes go first, the depots trimmed as
    /// `take_wait` trims them, then the take once more.
    pub(crate) fn take_trimming(&self, bytes: u64) -> bool {
        if self.take(bytes) {
            return true;
        }
        self.trim_depots(bytes);
        self.take(bytes)
    }

    /// `take` with `kept` more bytes left free. D3941: `admit` = a statement's grant, which decide(Grow)'s spill-self does
    /// not refuse (`governor::fits_raw_admit`: only the draws past a grant are its own road).
    fn take_leaving(&self, bytes: u64, kept: u64, admit: bool) -> bool {
        let budget = crate::mem::reclaim::applies();
        let fits = |n: u64| if admit { mcx::governor::fits_raw_admit(n as usize) } else { mcx::governor::fits_raw(n as usize) };
        // D3063 (c): past the memory governor's bar (the process's sampled resident memory) the pool is full
        if !budget && !fits(bytes) {
            note_take_fail(admit, mempolicy::admit::TAKE_FAIL_BAR, bytes);
            return false;
        }
        let cap = self.capacity();
        // ladder C1 (C0's finding 1): the pool armed with the caches at their floors counts the cache bytes past them its
        // trims cannot drop (`mcx::ladder::caches_stuck`; 0 with C1 off)
        let stuck = mcx::ladder::caches_stuck();
        let limit = cap.saturating_sub(self.reserve()).saturating_add(self.small_held()).saturating_sub(kept).saturating_sub(stuck);
        let mut cur = self.held.load(Ordering::Acquire);
        // take-back TB4 (give_back_spill_overdraft): a draw past the limit holds on the give-back lane's landing (the pool
        // bytes its queued jobs hold, back as they land), under the cancel line only; never an admission
        let od = if cap != 0 && !admit && cur.saturating_add(bytes) > limit { crate::mem::giveback::overdraft_room() } else { 0 };
        if cap != 0 && cur.saturating_add(bytes) > limit.saturating_add(od) {
            note_take_fail(admit, mempolicy::admit::past_limit_why(cur, bytes, limit.saturating_add(od), stuck), bytes);
            return false;
        }
        // memory step 5b: under the one memory budget a draw the pool has room for also needs B's free bytes. The
        // reclaimer frees managed cache bytes inline when B has none, before the governor's bar is tested (B is under the
        // bar, and a full reclaimable cache no longer reads as a full process). A draw B still has no room for takes its
        // own road. Off / advise: the bar alone, as before.
        if budget && (!crate::mem::reclaim::make_room(bytes) || !fits(bytes)) {
            note_take_fail(admit, mempolicy::admit::TAKE_FAIL_BROKER, bytes);
            return false;
        }
        // ladder C1 (`cache_floor`): a draw's or a grant's bytes may land in memory; the ladder evicts above the caches'
        // floors first when they would leave less than LOW under its line (the pool has room: the caches count at floor)
        crate::mem::reclaim::ladder_room(bytes);
        loop {
            if cap != 0 && cur.saturating_add(bytes) > limit.saturating_add(od) {
                note_take_fail(admit, mempolicy::admit::past_limit_why(cur, bytes, limit.saturating_add(od), stuck), bytes);
                return false;
            }
            match self.held.compare_exchange_weak(cur, cur + bytes, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => {
                    if cap != 0 && cur + bytes > limit {
                        crate::mem::giveback::note_overdraft(bytes.min(cur + bytes - limit), cur + bytes - limit);
                    }
                    // the C1 try (`inline_on_draw`) in a pool armed with the caches at their floors: the room this take
                    // left, with the parked regions the pool's own trim frees first, bounds the ladder's raises
                    // (`mcx::ladder::note_pool_room`), so no raise grows the bytes counted above into it. Only a take that
                    // landed: an admission's first ask past the room and a take before the depots' trim are the pool's
                    // own probing, not a cache's bytes in the way
                    if cap != 0 && crate::mem::reclaim::pool_floors() && mempolicy::ladder::inline_on_draw() {
                        let parked = crate::park::REGIONS.parked_bytes() as u64;
                        mcx::ladder::note_pool_room(limit.saturating_add(parked).saturating_sub(cur + bytes));
                    }
                    return true;
                }
                Err(now) => cur = now,
            }
        }
    }

    /// The turn's need an admission holding `ticket` leaves free: none when no waiter holds the turn or `ticket` does.
    fn turn_kept(&self, ticket: u32) -> u64 {
        let turn = self.turn.load(Ordering::Acquire);
        if turn == 0 || (turn >> 32) as u32 == ticket { 0 } else { (turn & u32::MAX as u64) << 20 }
    }

    /// D3924: the need the oldest waiter keeps free (`Turn`; 0 = no waiter holds the turn), whole MiB. Tests wait on it for
    /// a waiter to be queued, never on a sleep.
    #[doc(hidden)]
    pub fn turn_need(&self) -> u64 {
        (self.turn.load(Ordering::Acquire) & u32::MAX as u64) << 20
    }

    /// D3127: whether the pool alone would seat `bytes` for an admission holding `ticket` (`take_admit` without the governor).
    fn seats(&self, bytes: u64, ticket: u32) -> bool {
        let cap = self.capacity();
        cap == 0 || self.held().saturating_add(bytes) <= cap.saturating_sub(self.reserve()).saturating_add(self.small_held()).saturating_sub(self.turn_kept(ticket))
    }

    /// D3214: the bytes a waiter holding `ticket` could take now: the least of the pool's room beside what is held and the
    /// turn's need (`turn_kept`), and the memory governor's (its bar less the projected resident bytes; off: no limit).
    fn room_for(&self, ticket: u32) -> u64 {
        let cap = self.capacity();
        let pool = if cap == 0 { u64::MAX } else { cap.saturating_sub(self.reserve()).saturating_add(self.small_held()).saturating_sub(self.turn_kept(ticket)).saturating_sub(self.held()) };
        let bar = mcx::governor::bar();
        let governor = if bar == 0 { u64::MAX } else { bar.saturating_sub(mcx::governor::projected()) as u64 };
        pool.min(governor)
    }

    /// `take` for an admission: past the turn's need too, unless `ticket` holds the turn (0 = no ticket).
    fn take_admit(&self, bytes: u64, ticket: u32) -> bool {
        self.take_leaving(bytes, self.turn_kept(ticket), true)
    }

    /// D3380: `bytes` held past the big lane's limit for one statement whose floor no room here could ever seat, only while
    /// no statement holds bytes (`held_by_statements`: the kept holds and the parked regions are no statement's) and no other
    /// statement runs alone (`solo`, cleared at its reservation's drop); the memory governor's bar still applies. Every
    /// other big admission then finds `held` past its limit and waits until the statement ends.
    fn take_solo(&self, bytes: u64) -> bool {
        if self.held_by_big_statements() != 0 || !mcx::governor::fits_raw_admit(bytes as usize) {
            return false;
        }
        if self.solo.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return false;
        }
        // D4403: the buffer's holders left out, as `held_by_big_statements`
        let idle = |held: u64| held.saturating_sub(self.kept.load(Ordering::Acquire)).saturating_sub(crate::park::REGIONS.parked_bytes() as u64).saturating_sub(self.small_held()) == 0;
        let mut cur = self.held.load(Ordering::Acquire);
        loop {
            // an admission between the check and the take holds bytes now: wait for it as for any other
            if !idle(cur) {
                self.solo.store(false, Ordering::Release);
                return false;
            }
            match self.held.compare_exchange_weak(cur, cur.saturating_add(bytes), Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return true,
                Err(now) => cur = now,
            }
        }
    }

    /// The kept holds (`keep`: each admitting session's admission-arena prefix, D2845): held while their session lives,
    /// given back by no statement's end.
    pub fn kept(&self) -> u64 {
        self.kept.load(Ordering::Acquire)
    }

    /// D3804: whether a need of `bytes` is past what the big lane (`limit`: the capacity less the small lane's slice) can
    /// ever seat beside the kept holds (`mempolicy::admit::past_lane`): the floor's road and exec's `floor_past_pool`.
    pub fn past_lane(&self, bytes: u64, limit: u64) -> bool {
        policy::past_lane(bytes, self.kept(), limit)
    }

    /// D3804 (memory policy 3.2, the one budget applied): `bytes` held past the pool's capacity and B's free bytes while no
    /// statement holds bytes (`held_by_statements`: the kept holds and the parked regions are no statement's): no wait can
    /// bring room back, so the statement starts at its floor and grows or spills. Unlike `take_solo` it is not held back
    /// by the `solo` flag or another waiter's turn: with no statement holding bytes no statement runs alone either. One
    /// admission wins (a compare-exchange over the held bytes while they are idle); a second then waits for the first
    /// one's bytes. The memory governor's bar still applies. It runs as `Admitted::Solo` (it sets the flag its
    /// reservation's drop clears).
    fn take_alone(&self, bytes: u64) -> bool {
        if !mcx::governor::fits_raw_admit(bytes as usize) {
            return false;
        }
        // D4403: the buffer's holders left out, as `held_by_big_statements`
        let idle = |held: u64| held.saturating_sub(self.kept.load(Ordering::Acquire)).saturating_sub(crate::park::REGIONS.parked_bytes() as u64).saturating_sub(self.small_held()) == 0;
        let mut cur = self.held.load(Ordering::Acquire);
        loop {
            if !idle(cur) {
                return false;
            }
            match self.held.compare_exchange_weak(cur, cur.saturating_add(bytes), Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => {
                    self.solo.store(true, Ordering::Release);
                    return true;
                }
                Err(now) => cur = now,
            }
        }
    }

    /// `bytes` of the slice (the small buffer, D4403) more held, by a small statement's reservation or a draw with no
    /// reservation; false = the slice is full. No make_room and no governor bar: the buffer is part of the capacity and at
    /// most a sixteenth of B (`clamp_buffer`).
    fn take_small(&self, bytes: u64) -> bool {
        let reserve = self.reserve();
        let mut cur = self.small_held.load(Ordering::Acquire);
        loop {
            if cur.saturating_add(bytes) > reserve {
                return false;
            }
            match self.small_held.compare_exchange_weak(cur, cur + bytes, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => {
                    // D4403: `held` may sit past the capacity beside an overdraft, a keep or a Solo floor (none takes the
                    // buffer), so only the buffer's own bound is asserted
                    self.held.fetch_add(bytes, Ordering::AcqRel);
                    debug_assert!(cur + bytes <= reserve);
                    return true;
                }
                Err(now) => cur = now,
            }
        }
    }

    /// `bytes` charged to `life` (else G); past it `None` (S7: the caller spills or ERRORs), with no statement from capacity (L1).
    pub(crate) fn hold(&self, bytes: usize, stmt: Option<&Reservation>, life: Option<&Rc<StmtCtx>>) -> Option<Held> {
        self.hold_draw(bytes, stmt, life, true)
    }

    /// D4920: a hashed IN list's set the Bind builds before any statement exists (`region::in_list_set_region`): `hold`'s
    /// road for a draw with no statement (D4403's small buffer first, then the big lane, then the wait) and its counts, but
    /// no L1: the set is no statement's draw past a grant. Its charge is the Bind's context owner's, the session's once the
    /// plan memo files the admission (D4844); its bytes come back when it drops. Every other draw with no statement is an L1.
    pub(crate) fn hold_in_list_set(&self, bytes: usize) -> Option<Held> {
        self.hold_draw(bytes, None, None, false)
    }

    /// `hold`; `l1` false: no L1 for a draw with no statement (`hold_in_list_set`).
    fn hold_draw(&self, bytes: usize, stmt: Option<&Reservation>, life: Option<&Rc<StmtCtx>>, l1: bool) -> Option<Held> {
        // D4043: past G's room the statement's purse (G's room carved for a section's workers) serves the rest, unless the
        // life's own limit is what cut it (S7: the caller spills)
        let own_fits = life.is_none_or(|l| (l.ctx.limit() as u64).saturating_sub(l.used()) >= bytes as u64);
        let charged = life.or_else(|| stmt.and_then(Reservation::g)).and_then(|c| c.charge_fit(bytes as u64));
        let extra = bytes as u64 - charged.as_ref().map_or(0, |c| c.2);
        if let Some(s) = stmt.filter(|_| extra > 0 && own_fits) {
            if let Some(p) = s.purse_take(extra) {
                let mut h = p.held(extra);
                h.charged = charged;
                h.back_g();
                s.drawn.set(s.drawn.get() + bytes as u64);
                return Some(h);
            }
            // a section is open: its workers draw the purse on their threads, so the caller's `left` may have counted bytes
            // they drew before this take. The draw is one G's room alone does not hold, whose road is past G as before the
            // purse (the region roads take it: `region::purse_missed`; any other caller its road for a draw G does not
            // hold): no refusal counted, no L1 draw past the grant
            if s.purse_open() {
                PURSE_MISSES.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        }
        if extra > 0 && stmt.is_some_and(|s| s.small) {
            self.small_overflows.fetch_add(1, Ordering::Relaxed);
        }
        let pull = stmt.is_some_and(|s| s.g.is_none());
        if extra > 0 && !pull && l1 && crate::grant_ledger::on() {
            let id = stmt.map_or(0, Reservation::id);
            crate::grant_ledger::violation(crate::grant_ledger::Check::L1, id, format_args!("draw of {bytes} B past the grant ({} B left{}){}", stmt.map_or(0, Reservation::left), stmt.and_then(Reservation::g).map_or(String::new(), |g| format!(": reserved {} used {} kept {}", g.g().reserved, g.g().used(), g.g().kept.get())), if stmt.is_none() { " with no statement" } else { "" })); // alloc: Cold
        }
        if extra > 0 && stmt.is_some_and(|s| s.g.is_some()) {
            self.refusals.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        // D4403: a small draw with no reservation (no statement, or an unadmitted statement's; `policy::small_draw`) takes
        // the small buffer first, which no big admission ever takes and whose take skips B's make_room and the governor's
        // bar; else the big lane as before; with both full it waits for either (`small_draw_wait`), never refused at once
        let buffered = extra > 0 && self.reserve() != 0 && policy::small_draw(extra);
        let mut small = 0;
        if buffered && self.take_small(extra) {
            small = extra;
        } else if extra > 0 && !self.take(extra) {
            self.trim_depots(extra);
            if !self.take(extra) {
                match buffered.then(|| self.small_draw_wait(extra)).flatten() {
                    Some(true) => small = extra,
                    Some(false) => {}
                    None => {
                        self.refusals.fetch_add(1, Ordering::Relaxed);
                        return None;
                    }
                }
            }
        }
        if small != 0 {
            self.small_draws.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(s) = stmt {
            s.drawn.set(s.drawn.get() + bytes as u64);
            s.borrowed.set(s.borrowed.get() + extra);
        }
        if extra > 0 {
            let (n, b) = if pull { (&self.unadmitted, &self.unadmitted_bytes) } else { (&self.borrows, &self.borrowed) };
            n.fetch_add(1, Ordering::Relaxed);
            b.fetch_add(extra, Ordering::Relaxed);
        }
        let h = Held { charged, pool: extra, kept: false, past: None, shown: None, gov: None, purse: None, small };
        h.back_g();
        Some(h)
    }

    /// D4403: a small draw with no reservation that the buffer and the big lane both refused waits for either. It retries at
    /// every `MemWait` pause on a cancel-polled step (a cancel ends it, as `take_wait`'s, D3887 / D4098) for at most
    /// `policy::bytes_wait()` (K::BytesWaitMs, 2000 ms): `Some(true)` the buffer took it, `Some(false)` the big lane, `None`
    /// the wait ended (the draw's refusal, as before the buffer).
    fn small_draw_wait(&self, bytes: u64) -> Option<bool> {
        self.small_waits.fetch_add(1, Ordering::Relaxed);
        let ep = mcx::ladder_stats::Wait::begin(mcx::ladder_stats::Site::SmallDraw, bytes);
        let t0 = MonoStamp::now();
        let limit = policy::bytes_wait();
        let mut step = cancelpoll::step("grant small draw");
        let mut w = MemWait::default();
        loop {
            let took = if self.take_small(bytes) {
                Some(true)
            } else if self.take(bytes) {
                Some(false)
            } else if waited(t0) >= limit || step.tick().is_err() {
                ep.refused();
                self.small_refusals.fetch_add(1, Ordering::Relaxed);
                None
            } else {
                w.pause();
                continue;
            };
            self.wait_ns.fetch_add(t0.elapsed_ns(), Ordering::Relaxed);
            return took;
        }
    }

    /// Up to `bytes` of `stmt`'s G carved for a worker's W (pool-held; its image cut, D187); what moved. D4043: past G's
    /// room, from its purse (a section opened inside another's: the outer one's purse holds the room; both are carved).
    pub(crate) fn carve_worker(&self, stmt: Option<&Reservation>, bytes: u64) -> u64 {
        let Some((s, g)) = stmt.and_then(|s| s.g().map(|g| (s, g))) else { return 0 };
        let m = g.carve(bytes);
        m + s.purse.borrow().as_ref().filter(|_| m < bytes).map_or(0, |p| p.take_up_to(bytes - m))
    }

    /// Parked capacity freed for `bytes` more: the parked regions the pool is short of them, least recently put first, then
    /// the slot depot's oldest sets; the region depot whole when the pool has the room and the memory governor refused
    /// (resident bytes). D3254: for `bytes` past what the pool seats with nothing held (an admission's whole ask: it
    /// halves, and each half trims for its own need) every parked slot set goes and no parked region: a set's sources and
    /// cursors hold memory outside the pool's account (left parked across a run of wide statements, ~8 GiB more resident
    /// than trunk's, on which the memory governor refused each halving of the next wide grouping), while dropping the whole region depot at every admission
    /// made each statement under a large work_mem map its regions afresh and run the allocator's purge of the dropped
    /// ones inside a section open (two 1.92 GiB slot regions and 16 worker regions a statement at W16: 2-33 ms in the bind).
    /// The room is read after the reaper gave back what it owes (`reap::drain`, at the start and before the caller's
    /// next take): a region a statement's end or this trim handed to it is the pool's again by then.
    fn trim_depots(&self, bytes: u64) -> usize {
        crate::mem::reap::drain();
        let freed = self.trim_depots_drained(bytes, &mut || {
            crate::mem::reap::drain();
            true
        });
        crate::mem::reap::drain();
        freed
    }

    /// Ladder step 1.1 (`gate_now`): an admission's `trim_depots` whose waits for the reaper end on its interrupt probe
    /// `stop` or its step's tick (`reap::drain_until`); off, `trim_depots`.
    fn trim_depots_until(&self, bytes: u64, stop: &dyn Fn() -> bool) -> usize {
        if !policy::gate_now() {
            return self.trim_depots(bytes);
        }
        let mut step = cancelpoll::step("admission trim");
        let mut stop = || stop() || step.tick().is_err();
        crate::mem::reap::drain_until(&mut stop);
        let freed = self.trim_depots_drained(bytes, &mut || {
            crate::mem::reap::drain_until(&mut stop);
            true
        });
        crate::mem::reap::drain_until(&mut stop);
        freed
    }

    /// D4098: `trim_depots` for a draw inside a kernel call (`take_wait`): its waits for the reaper tick the draw's step, and
    /// the statement's cancel ends the trim there (the draw then refuses as the cancel).
    fn trim_depots_polled(&self, bytes: u64, step: &mut cancelpoll::Step) -> Result<usize, cancelpoll::Canceled> {
        crate::mem::reap::drain_polled(step)?;
        let mut canceled = false;
        let freed = self.trim_depots_drained(bytes, &mut || {
            canceled = canceled || crate::mem::reap::drain_polled(step).is_err();
            !canceled
        });
        if canceled {
            return Err(cancelpoll::Canceled);
        }
        crate::mem::reap::drain_polled(step)?;
        Ok(freed)
    }

    /// The trim between `trim_depots`' two waits for the reaper; `drain` waits for it after a depot's whole trim (false:
    /// the draw's cancel ended the wait, and the trim stops there).
    fn trim_depots_drained(&self, bytes: u64, drain: &mut dyn FnMut() -> bool) -> usize {
        let cap = self.capacity();
        let limit = cap.saturating_sub(self.reserve()).saturating_add(self.small_held());
        if cap != 0 && bytes > limit {
            let slots = crate::slots::DEPOT.trim_for(usize::MAX);
            drain();
            self.trims.fetch_add(1, Ordering::Relaxed);
            return slots;
        }
        let room = || limit.saturating_sub(self.held());
        let regions = match bytes.checked_sub(room()).filter(|&short| cap != 0 && short > 0) {
            Some(short) => crate::park::REGIONS.trim_to(crate::park::REGIONS.parked_bytes().saturating_sub(short as usize)),
            None => {
                let regions = crate::park::REGIONS.trim_handed();
                // pagepool P3 (C.11): the pool has the room and the memory governor refused (resident bytes): pressure,
                // so the regions' pool bytes go back to the OS too (a trim for room alone keeps them warm for the take)
                crate::mem::region::pool_release(regions as u64);
                if !drain() {
                    self.trims.fetch_add(1, Ordering::Relaxed);
                    return regions;
                }
                regions
            }
        };
        let slots = match bytes.checked_sub(room()) {
            Some(short) if short > 0 => crate::slots::DEPOT.trim_for(short as usize),
            _ => 0,
        };
        self.trims.fetch_add(1, Ordering::Relaxed);
        regions + slots
    }

    /// `bytes` more held within `BYTES_WAIT` (depots trimmed first): D225's arena past W and G; `false` = the named OOM.
    /// D3239: it waits only while a grant holder besides the caller's own could give bytes back (`Others`; D3242: holders,
    /// not sessions). With none, no room can come back but the caller's own, so it refuses at once (a lone statement waited
    /// the whole 2 s at every hosted arena chunk past its halved grant, for minutes). `worker`: the caller is a worker of its
    /// statement.
    pub(crate) fn take_wait(&self, bytes: u64, worker: bool) -> bool {
        self.take_wait_at(bytes, worker, mcx::ladder::Gate::Draw)
    }

    /// `take_wait` from `gate` (its climbs counted per gate, `mcx::ladder::gate_climbs`).
    pub(crate) fn take_wait_at(&self, bytes: u64, worker: bool, gate: mcx::ladder::Gate) -> bool {
        self.take_gate(bytes, worker, gate, policy::Asker::Refuses)
    }

    /// The one gate a draw past W and G asks through (ladder step 1.1; a growth step's entry point when grow is built):
    /// `bytes` more held, `asker` naming the caller's road past the pool (`Spills`: a mapped temp file; `Refuses`: its
    /// refusal). With `gate_now` off, `take_wait`'s wait whatever the asker; on, `gate_wait_now`'s.
    pub(crate) fn take_gate(&self, bytes: u64, worker: bool, gate: mcx::ladder::Gate, asker: policy::Asker) -> bool {
        if self.take(bytes) {
            return true;
        }
        // the C1 try (`inline_on_draw`) in a pool armed with the caches at their floors: a draw past its grant the pool
        // had no room for is the pool at its edge (its climb's R2a takes cache bytes next): no raise for a window
        if self.capacity() != 0 && crate::mem::reclaim::pool_floors() && mempolicy::ladder::inline_on_draw() {
            mcx::ladder::note_pool_room(0);
        }
        // D4439 (`draw_wait_cap`, with the ladder on): the statement's draw-gate waits are capped in all; spent, the
        // draw takes its road (or its refusal) at once
        let cap_left = draw_cap_left();
        if cap_left.is_some_and(|l| l.is_zero()) {
            return self.take_capped(bytes, gate);
        }
        if policy::gate_now() {
            return self.gate_wait_now(bytes, worker, gate, asker, cap_left);
        }
        // D3941: a statement decide(Grow) sent down its own road is refused by its own state, which no holder's end
        // changes: it spills at once
        if mcx::governor::spills_itself() {
            mcx::ladder_stats::note_refused(mcx::ladder_stats::Site::of(gate));
            note_refusal(mcx::ladder_stats::Why::SpillsItself, bytes);
            self.refusals.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        self.waits.fetch_add(1, Ordering::Relaxed);
        let ep = mcx::ladder_stats::Wait::begin(mcx::ladder_stats::Site::of(gate), bytes);
        let mut step = cancelpoll::step("grant take_wait");
        // D4914: the branch that ends the wait refused (a cancel unless another sets it)
        let mut why = mcx::ladder_stats::Why::Canceled;
        // D4098: the trim's waits for the reaper tick the step: a cancel ends them and refuses the draw (the cancel)
        'wait: {
            if self.trim_depots_polled(bytes, &mut step).is_err() {
                break 'wait;
            }
            let t0 = MonoStamp::now();
            let mut others = Others::here(worker);
            let mut w = MemWait::default();
            // D4324 step 3 (pgrust.memory_ladder = on): the asker climbs R1-R4b once, then waits (R5) with its budget
            // counted from the last free it saw (LADDER-DESIGN section 8)
            let ladder = mcx::ladder::on();
            // D4353: R5 is counted (with its measured yield) when the climb left a need; a spent wait hands R2b / R6 to
            // the ladder thread once and waits on
            let (wait, mut handed) = if ladder { (mcx::ladder::wait_begin(&mcx::ladder::climb_at(gate, bytes)), false) } else { (None, true) };
            let (mut seen, mut since) = (mcx::ladder::epoch(), t0);
            loop {
                if self.take(bytes) {
                    mcx::ladder::wait_end(wait);
                    note_draw_wait(cap_left, t0);
                    return true;
                }
                // memory step 5b: a draw B had no room for waits for the background reclaimer's next tick as for a holder
                let could = others.could_give();
                let due = !could && crate::mem::reclaim::pending();
                let spent = if ladder {
                    let e = mcx::ladder::epoch();
                    if e != seen {
                        (seen, since) = (e, MonoStamp::now());
                    }
                    let mut spent = mempolicy::ladder::wait_spent(waited(since), waited(t0), policy::bytes_wait());
                    if spent && !handed {
                        handed = true;
                        if mcx::ladder::hand_off(bytes) {
                            (since, spent) = (MonoStamp::now(), false);
                        }
                    }
                    if spent { policy::bytes_wait() } else { core::time::Duration::ZERO }
                } else {
                    waited(t0)
                };
                // D4439: the statement's cap spent while this draw waited: its road
                let capped = cap_left.is_some_and(|l| waited(t0) >= l);
                match if capped { policy::DrawWait::Refuse } else { policy::draw_wait(could, due, spent) } {
                    policy::DrawWait::LastTry => {
                        ep.last_try();
                        // no other holder can give bytes back. One more take after any that ended gave theirs (a holder
                        // leaves the count after its bytes went back; its regions may sit parked in the depots), with one
                        // fresh governor sample (its last one lags a release by up to its period, D3127)
                        if others.seen && self.trim_depots_polled(bytes, &mut step).is_err() {
                            break;
                        }
                        // D3971: the trim's drains give regions back: a cancel it outlasted refuses the draw (the cancel)
                        if step.tick().is_err() {
                            break;
                        }
                        mcx::governor::resample();
                        if self.take(bytes) {
                            mcx::ladder::wait_end(wait);
                            note_draw_wait(cap_left, t0);
                            return true;
                        }
                        why = mcx::ladder_stats::Why::LastTry;
                        break;
                    }
                    policy::DrawWait::Refuse => {
                        why = if capped { mcx::ladder_stats::Why::DrawCapped } else { mcx::ladder_stats::Why::WaitSpent };
                        break;
                    }
                    // D3887: a draw inside a kernel call stops waiting once its statement is canceled (its refusal becomes
                    // the cancel, `cancelpoll::seen`)
                    policy::DrawWait::Wait => {
                        if step.tick().is_err() {
                            break;
                        }
                        w.pause();
                    }
                }
            }
            mcx::ladder::wait_end(wait);
            note_draw_wait(cap_left, t0);
        }
        note_refusal(why, bytes);
        ep.refused();
        self.refusals.fetch_add(1, Ordering::Relaxed);
        false
    }

    /// D4439 (`draw_wait_cap`): `take_gate` for a statement whose draw-gate waits spent their cap: the idle depots
    /// trimmed without waiting for the reaper, one fresh governor sample and one take, else refused at once (the
    /// caller's road), counted as the gate's refusal.
    fn take_capped(&self, bytes: u64, gate: mcx::ladder::Gate) -> bool {
        self.trim_depots_drained(bytes, &mut || true);
        mcx::governor::resample();
        if self.take(bytes) {
            return true;
        }
        DRAW_CAPPED.fetch_add(1, Ordering::Relaxed);
        mcx::ladder_stats::note_refused(mcx::ladder_stats::Site::of(gate));
        note_refusal(mcx::ladder_stats::Why::DrawCapped, bytes);
        self.refusals.fetch_add(1, Ordering::Relaxed);
        false
    }

    /// Ladder step 1.1 (`gate_now`): `take_gate` past its first miss. The asker waits only for bytes already on their way
    /// (`bytes_coming`) within R5's budget from the last free, parked on the free epoch (`MemWait::park_free`); with none
    /// on their way a `Spills` asker trims the depots without waiting for the reaper, re-takes once and takes its road, and
    /// a `Refuses` asker waits `gate_refuse_ms` for room from its first miss, then refuses (`policy::gate_wait`). With the
    /// ladder on the asker climbs R1-R4b once first, and a `Refuses` asker past its wait hands R2b / R6 to the ladder thread
    /// once and waits for one pass to read it: a cancel that pass raises is bytes on their way.
    fn gate_wait_now(&self, bytes: u64, worker: bool, gate: mcx::ladder::Gate, asker: policy::Asker, cap_left: Option<core::time::Duration>) -> bool {
        if mcx::governor::spills_itself() {
            note_refusal(mcx::ladder_stats::Why::SpillsItself, bytes);
            self.refusals.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        self.waits.fetch_add(1, Ordering::Relaxed);
        let ep = mcx::ladder_stats::Wait::begin(mcx::ladder_stats::Site::of(gate), bytes);
        let _ = worker;
        let mut step = cancelpoll::step("grant take_gate");
        // D4914: as `take_gate`'s
        let mut why = mcx::ladder_stats::Why::Canceled;
        'wait: {
            // R2a before R4a: a spill road comes after the idle depots went, without waiting for the reaper's unmaps
            // (they are bytes on their way below); any other asker trims as `take_wait` does
            let trimmed = match asker {
                policy::Asker::Spills => {
                    self.trim_depots_drained(bytes, &mut || true);
                    Ok(())
                }
                policy::Asker::Refuses => self.trim_depots_polled(bytes, &mut step).map(|_| ()),
            };
            if trimmed.is_err() {
                break 'wait;
            }
            let t0 = MonoStamp::now();
            let mut w = MemWait::default();
            let ladder = mcx::ladder::on();
            let wait = if ladder { mcx::ladder::wait_begin(&mcx::ladder::climb_at(gate, bytes)) } else { None };
            let mut handed: Option<u64> = None;
            let (mut seen, mut since) = (mcx::ladder::epoch_seq(), t0);
            loop {
                if self.take(bytes) {
                    mcx::ladder::wait_end(wait);
                    note_draw_wait(cap_left, t0);
                    return true;
                }
                // D4439: the statement's cap spent while this draw waited: its road (no hand-off wait either)
                let capped = cap_left.is_some_and(|l| waited(t0) >= l);
                if capped {
                    handed = handed.or(Some(0));
                }
                match if capped { policy::DrawWait::Refuse } else { policy::gate_wait(asker, bytes_coming(handed), waited(since), waited(t0)) } {
                    policy::DrawWait::Wait => {
                        if step.tick().is_err() {
                            break;
                        }
                        w.park_free(seen);
                        let e = mcx::ladder::epoch_seq();
                        if e != seen {
                            (seen, since) = (e, MonoStamp::now());
                        }
                    }
                    _ => {
                        if ladder && handed.is_none() && asker == policy::Asker::Refuses {
                            let at = mcx::ladder::passes();
                            if mcx::ladder::hand_off(bytes) {
                                handed = Some(at);
                                continue;
                            }
                        }
                        // the road: one fresh governor sample and one more take first (its last sample lags a
                        // release by up to its period, D3127)
                        mcx::governor::resample();
                        if self.take(bytes) {
                            mcx::ladder::wait_end(wait);
                            note_draw_wait(cap_left, t0);
                            return true;
                        }
                        why = if capped { mcx::ladder_stats::Why::DrawCapped } else { mcx::ladder_stats::Why::WaitSpent };
                        break;
                    }
                }
            }
            mcx::ladder::wait_end(wait);
            note_draw_wait(cap_left, t0);
        }
        note_refusal(why, bytes);
        ep.refused();
        self.refusals.fetch_add(1, Ordering::Relaxed);
        false
    }

    pub(crate) fn release(&self, bytes: usize) {
        self.held.fetch_sub(bytes as u64, Ordering::AcqRel);
        // D4324 step 3: the free epoch a waiting asker's budget counts from (LADDER-DESIGN section 8); a read-mostly mode
        // load first, so the shared counter is written only with the ladder on (ladder step 1.1: or the gate's rule)
        if mcx::ladder::on() || policy::gate_now() {
            mcx::ladder::note_free();
        }
    }

    /// D2845: `bytes` resident outside every statement (a level's mapped admission arena's kept prefix) held while the
    /// returned hold lives: taken at once and never refused, past the capacity too (a hosted store's way, D1421), so
    /// admission sees them. Counted in `kept` too: no statement's end frees them, so admission's short wait does not wait
    /// on them (`held_by_statements`).
    pub fn keep(&self, bytes: u64) -> Held {
        self.held.fetch_add(bytes, Ordering::AcqRel);
        self.kept.fetch_add(bytes, Ordering::AcqRel);
        Held { charged: None, pool: bytes, kept: true, past: None, shown: None, gov: None, purse: None, small: 0 }
    }

    /// The one code path of every overdraft (`Overdraw`): `bytes` held at once, past the capacity too, never refused and never
    /// waiting, counted for `site` (`overdraws`).
    fn overdraw_take(&self, bytes: u64, site: Overdraw) {
        self.held.fetch_add(bytes, Ordering::AcqRel);
        self.overdraw_n[site as usize].fetch_add(1, Ordering::Relaxed);
        self.overdraw_bytes[site as usize].fetch_add(bytes, Ordering::Relaxed);
    }

    /// An overdraft for `site` under a running statement (`overdraw_take`): `bytes` held at once, past the capacity too, never
    /// refused and never waiting (`keep`'s way, D1421 / D2845) but not `kept` (the statement's end gives it back, so
    /// admission's wait may wait on it); with `stmt`, counted past G on it (`Reservation::note_grown`, as D225's arena road
    /// counts a chunk). Back to the pool when the hold drops.
    pub fn overdraw(&self, bytes: u64, stmt: Option<&Reservation>, site: Overdraw) -> Held {
        self.overdraw_take(bytes, site);
        match stmt {
            Some(s) => Held::grown(bytes, Some(s.note_grown(bytes))),
            // G1-B: a worker's (no statement count) shows on its claim's memory ledger account
            None => Held::grown_worker(bytes),
        }
    }

    /// D4329: an overdraft for `site` whose bytes are already on the calling thread's ledger account (door 1: heap the
    /// thread allocates itself): pool bytes only, no statement count and nothing shown on the ledger. Back to the pool when
    /// the hold drops.
    pub(crate) fn overdraw_pool(&self, bytes: u64, site: Overdraw) -> Held {
        self.overdraw_take(bytes, site);
        Held::pool(bytes)
    }

    /// The overdrafts taken so far per site, `Overdraw::ALL`'s order: (count, bytes).
    pub fn overdraws(&self) -> [(u64, u64); Overdraw::N] {
        Overdraw::ALL.map(|o| (self.overdraw_n[o as usize].load(Ordering::Relaxed), self.overdraw_bytes[o as usize].load(Ordering::Relaxed)))
    }

    /// `want` when its regions fit beside what is held (and the turn's need), else `want` once enough grants came back, waiting
    /// while they do (both depots trimmed), else the largest halving that fits, never under `GRANT_FLOOR`; unarmed: `want`.
    pub fn admit(&self, want: Grant, live: u16, width: u16, wait: core::time::Duration) -> Result<Reservation, AdmitRefused> {
        self.admit_for(want, live, width, false, wait)
    }

    /// The small lane: `want`'s regions from the slice at once, no slot, no wait; `None` (nothing touched) when the slice
    /// is full or there is none — the statement then goes the big lane's way.
    pub fn admit_small(&self, want: Grant, live: u16, width: u16, images: bool) -> Option<Reservation> {
        self.admit_small_need(want, regions_for(want, live, width, images))
    }

    /// `admit_small` reserving `need` (a program's `footprint` at `want`).
    pub fn admit_small_need(&self, want: Grant, need: u64) -> Option<Reservation> {
        if self.capacity() == 0 || self.reserve() == 0 {
            return None;
        }
        if !self.take_small(need) {
            self.small_fallbacks.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        self.small_admits.fetch_add(1, Ordering::Relaxed);
        Some(Reservation::new(want, want, need, 0, true, false, Admitted::Whole))
    }

    /// D4403: a small statement's admission, `need_of(want)` its reservation at its unchanged ask
    /// (`policy::small_statement`; under lanes the slice's ask), under the statement's own limit as every admission
    /// (`fit_statement_limit`): the small buffer, else the big lane at once (`take_admit`: the turn's need left free, no
    /// halving, no turn taken), else a wait that retries both at every `MemWait` pause (small holders end quickly, and no
    /// big admission takes the buffer) until `wait` (`pgrust.gravity_grant_wait_ms`) or `stop` (the backend's pending
    /// interrupt: an `interrupted` refusal its caller processes), then the refusal (S7). No in-flight slot, as the slice's
    /// admissions. `None` (nothing touched) with no buffer, unarmed, or a need past the buffer: the caller takes the big
    /// lane's road.
    pub fn admit_small_until(&self, want: Grant, need_of: &dyn Fn(Grant) -> u64, wait: core::time::Duration, stop: &dyn Fn() -> bool) -> Option<Result<Reservation, AdmitRefused>> {
        if self.capacity() == 0 || self.reserve() == 0 {
            return None;
        }
        let want = fit_statement_limit(want, need_of);
        let need = need_of(want);
        if need > self.reserve() {
            return None;
        }
        let t0 = MonoStamp::now();
        let mut w = MemWait::default();
        let mut queued = false;
        let out = loop {
            if self.take_small(need) {
                self.small_admits.fetch_add(1, Ordering::Relaxed);
                break Ok(true);
            }
            if self.take_admit(need, 0) {
                break Ok(false);
            }
            if !queued {
                queued = true;
                self.small_fallbacks.fetch_add(1, Ordering::Relaxed);
                self.small_waits.fetch_add(1, Ordering::Relaxed);
            }
            if stop() {
                break Err(true);
            }
            if waited(t0) >= wait {
                break Err(false);
            }
            w.pause();
        };
        let ns = t0.elapsed_ns();
        self.wait_ns.fetch_add(ns, Ordering::Relaxed);
        Some(match out {
            Ok(small) => Ok(Reservation::new(want, want, need, ns / 1_000_000, small, false, Admitted::Whole)),
            Err(interrupted) => {
                if !interrupted {
                    self.small_refusals.fetch_add(1, Ordering::Relaxed);
                    self.refusals.fetch_add(1, Ordering::Relaxed);
                }
                Err(AdmitRefused { grant: want, need, held: self.held(), capacity: self.capacity(), waited_ms: ns / 1_000_000, interrupted })
            }
        })
    }

    pub fn admit_for(&self, want: Grant, live: u16, width: u16, images: bool, wait: core::time::Duration) -> Result<Reservation, AdmitRefused> {
        self.admit_need(want, &|g| regions_for(g, live, width, images), wait)
    }

    /// `admit_for` reserving `need(grant)` per grant tried (`footprint`, D192); at the floor a full pool waits out `wait`, then
    /// refuses (S7: never admitted with nothing reserved). D3380: a floor need past the big lane's whole room waits for the
    /// pool to empty and then runs alone past it (`Admitted::Solo`); a nested one, or one past the memory governor's bar
    /// itself, refuses at once.
    pub fn admit_need(&self, want: Grant, need_of: &dyn Fn(Grant) -> u64, wait: core::time::Duration) -> Result<Reservation, AdmitRefused> {
        self.admit_need_until(want, need_of, wait, &|| false)
    }

    /// An admission wait that a pending interrupt ended (`AdmitRefused::interrupted`): its slot given back, its wait counted.
    fn interrupted(&self, want: Grant, need: u64, t0: MonoStamp, slot: bool) -> AdmitRefused {
        self.wait_ns.fetch_add(t0.elapsed_ns(), Ordering::Relaxed);
        if slot {
            self.inflight.fetch_sub(1, Ordering::AcqRel);
            SLOTS_HERE.with(|s| s.set(s.get().saturating_sub(1)));
        }
        AdmitRefused { grant: want, need, held: self.held(), capacity: self.capacity(), waited_ms: waited(t0).as_millis() as u64, interrupted: true }
    }

    /// `admit_need` whose waits end early when `stop` is true (D599): the backend's pending interrupt (a statement
    /// timeout, a cancel, a termination), polled at every pause as PostgreSQL's lock waits honour them; the refusal says
    /// `interrupted` and releases what the wait took.
    pub fn admit_need_until(&self, want: Grant, need_of: &dyn Fn(Grant) -> u64, wait: core::time::Duration, stop: &dyn Fn() -> bool) -> Result<Reservation, AdmitRefused> {
        self.admit_need_within(want, need_of, wait, stop, StallBounds::current())
    }

    /// `admit_need_until` with the queue's stall bounds given (D3214's tests run them in milliseconds).
    #[doc(hidden)]
    pub fn admit_need_within(&self, want: Grant, need_of: &dyn Fn(Grant) -> u64, wait: core::time::Duration, stop: &dyn Fn() -> bool, bounds: StallBounds) -> Result<Reservation, AdmitRefused> {
        if self.capacity() == 0 {
            return Ok(Reservation::unadmitted(want));
        }
        let want = fit_statement_limit(want, need_of);
        // ladder 1.3 (`fair_share`): the statement's floor, its work_mem: no fair share under it
        if mcx::revoke::fair_knob() {
            mcx::revoke::set_floor(want.sort_floor);
        }
        let t0 = MonoStamp::now();
        let mut w = MemWait::default();
        let slot = loop {
            if let Some(took) = self.take_slot() {
                break took;
            }
            if waited(t0) >= wait || stop() {
                self.waits.fetch_add(1, Ordering::Relaxed);
                break false;
            }
            w.pause();
        };
        if !slot && stop() {
            return Err(AdmitRefused { grant: want, need: need_of(want), held: self.held(), capacity: self.capacity(), waited_ms: waited(t0).as_millis() as u64, interrupted: true });
        }
        // C3 (decide Start): under projection a top-level start waits while F < F* (`throttle_max_ms` and this wait at
        // most; an interrupt ends it), unless its need is within the grant floor (`start_holds`). A nested start never
        // waits: its caller holds bytes here and ends after it.
        let need = need_of(want);
        if mempolicy::decide::start_holds(mcx::governor::throttled(), need) && HELD_HERE.with(core::cell::Cell::get) == 0 && !throttle_wait(wait.saturating_sub(waited(t0)), stop, &mut w) {
            return Err(self.interrupted(want, need, t0, slot));
        }
        let done = |grant: Grant, reserved: u64, how: Admitted| {
            let ns = t0.elapsed_ns();
            self.wait_ns.fetch_add(ns, Ordering::Relaxed);
            Ok(Reservation::new(grant, want, reserved, ns / 1_000_000, false, slot, how))
        };
        if self.take_admit(need, 0) {
            return done(want, need, Admitted::Whole);
        }
        self.waits.fetch_add(1, Ordering::Relaxed);
        let ep = mcx::ladder_stats::Wait::begin(mcx::ladder_stats::Site::Admit, need);
        let _queued = Queued::new(need);
        self.trim_depots_until(need, stop);
        // D4353 (pgrust.memory_ladder = on): admission is a gate (LADDER-DESIGN section 3). Its asker climbs R1-R4b once
        // with its need; the wait below is R5, counted with its measured yield at every exit (`AdmitWait`)
        let ladder = mcx::ladder::on();
        let _r5 = AdmitWait(if ladder { mcx::ladder::wait_begin(&mcx::ladder::climb_at(mcx::ladder::Gate::Admit, need)) } else { None });
        // ladder C1 (C0's finding 1): less the stuck cache bytes, as the take counts them (0 with C1 off)
        let limit = self.capacity().saturating_sub(self.reserve()).saturating_sub(mcx::ladder::caches_stuck());
        // the whole ask, in the queue: the oldest waiter holds the turn, whose need later admissions leave free; a halved grant
        // spills and holds its bytes the longer, so the next waiter halves too. A waiter whose room (`room_for`) made no new
        // high by `STALL_PROGRESS` for `QUEUE_STALL`, or that held its ticket for `TURN_CAP`, halves (D3214). A need that
        // cannot fit whole beside the bytes no statement ahead of it gives back (what this thread's own statements hold: an ask
        // nested in its caller's grant; the kept holds; an ask past the big lane) takes no turn and waits `BYTES_WAIT`, as does
        // a turn whose need the kept holds outgrow while it waits.
        let own = HELD_HERE.with(core::cell::Cell::get);
        let whole = || policy::whole_fits(need, own, self.kept.load(Ordering::Acquire), limit);
        // D3242: a NESTED ask (`own`: this thread's statements hold bytes) whose need cannot fit whole beside its caller's
        // grant takes no turn; its wait and the floor's end as soon as no grant holder besides this thread's own could give
        // bytes back (`Others`), since its caller ends only after it. A nested statement per row of its caller waited
        // `BYTES_WAIT` at each admission, then the floor's whole `wait` when even the floor did not fit. Other asks as before.
        let nested = own > 0;
        let mut others = Others::here(false);
        let mut turn = whole().then(|| {
            let ticket = (self.tickets.fetch_add(1, Ordering::AcqRel) % u32::MAX as u64) as u32 + 1;
            Turn { pool: self, ticket, clock: StallClock::new(self.room_for(ticket), MonoStamp::now().as_ns()) }
        });
        let mut short = wait.min(if turn.is_some() { bounds.stall } else { policy::bytes_wait() });
        let (mut seen, mut since) = (self.ended.load(Ordering::Acquire), MonoStamp::now());
        // D3127: the settle wait's start, its first fresh resident sample, the lowest one and the samples since it fell
        let mut settle: Option<(u64, usize, usize, u32)> = None;
        // ladder step 1.1 (`gate_now`): the wait parks on the free epoch (`MemWait::park_free`), its rule unchanged
        let gate = policy::gate_now();
        let forced = loop {
            let epoch = if gate { mcx::ladder::epoch_seq() } else { 0 };
            if let Some(turn) = turn.as_ref().filter(|_| !wait.is_zero()) {
                turn.claim(need);
            }
            // nothing to wait for: no statement holds bytes, and no other waiter is ahead (its grant would come back); read
            // before the take, so a grant that comes back in between is taken whole, not halved
            let idle = self.held_by_big_statements() == 0 && !turn.as_ref().is_some_and(|t| t.ahead());
            if self.take_admit(need, turn.as_ref().map_or(0, |t| t.ticket)) {
                return done(want, need, Admitted::Whole);
            }
            if stop() {
                // D3069: a pending interrupt ends the wait for the whole ask as an interrupted refusal, never as a halved
                // grant: the caller processes it (a cancel or timeout raises) and an interrupt that raises nothing (an early
                // timer wake, a sinval catch-up) queues again. Halving here spilled every statement a stray wake reached.
                ep.canceled();
                drop(turn);
                return Err(self.interrupted(want, need, t0, slot));
            }
            // D3127: an empty pool that would seat the whole need but the governor refuses may be a release the governor's
            // 100 ms sample has not seen yet: a fresh sample decides, and the need waits on fresh samples while the resident
            // bytes keep falling (`SETTLE_FALL`, `SETTLE_FLAT_POLLS`), `GOVERNOR_SETTLE` at most; a refusal that holds is real
            let ticket = turn.as_ref().map_or(0, |t| t.ticket);
            if !idle {
                settle = None;
            }
            let mut settling = false;
            if idle && !mcx::governor::fits_raw_admit(need as usize) && self.seats(need, ticket) && mcx::governor::resample() {
                if self.take_admit(need, ticket) {
                    return done(want, need, Admitted::Whole);
                }
                let now = mcx::governor::projected();
                let began = settle.is_none();
                match settle.as_mut() {
                    None => settle = Some((settle_now_ns(), now, now, 0)),
                    Some((_, _, low, flat)) if now.saturating_add(policy::settle_fall()) <= *low => (*low, *flat) = (now, 0),
                    Some((_, _, _, flat)) => *flat += 1,
                }
                if let Some((settle_t0, first, _, flat)) = settle {
                    let on = core::time::Duration::from_nanos(settle_now_ns().saturating_sub(settle_t0));
                    let deficit = now.saturating_add(need as usize).saturating_sub(mcx::governor::bar());
                    settling = flat < policy::settle_flat_polls() && on < policy::governor_settle() && (began || settle_on_track(first.saturating_sub(now), deficit, on));
                }
            }
            if idle && !settling {
                break Admitted::Idle;
            }
            if waited(t0) >= wait {
                break Admitted::Bound;
            }
            if turn.is_some() && !whole() {
                turn = None;
                (short, since) = (wait.min(policy::bytes_wait()), MonoStamp::now());
            }
            let ended = self.ended.load(Ordering::Acquire);
            if let Some(t) = turn.as_mut() {
                if ended != seen {
                    seen = ended;
                    // an ended statement's regions may sit parked in the depots
                    self.trim_depots_until(need, stop);
                }
                // D3214: an end is progress only if the room it leaves is a new high
                let room = self.room_for(t.ticket);
                if let Some(how) = t.clock.observe(room, MonoStamp::now().as_ns(), StallBounds { stall: short, ..bounds }) {
                    break how;
                }
            } else if waited(since) >= short {
                break Admitted::Short;
            } else if nested && !others.could_give() {
                // as take_wait's: one more take (an ended holder may have parked its regions; one fresh governor sample)
                if others.seen {
                    self.trim_depots_until(need, stop);
                }
                mcx::governor::resample();
                if self.take_admit(need, 0) {
                    return done(want, need, Admitted::Whole);
                }
                break Admitted::Short;
            }
            if gate {
                w.park_free(epoch);
            } else {
                w.pause();
            }
        };
        drop(turn);
        ep.halved();
        // D4353: a wait spent with the need still there hands R2b / R6 to the ladder thread (the asker never cancels anyone);
        // the asker halves as before
        if ladder && forced != Admitted::Idle {
            mcx::ladder::hand_off(need);
        }
        // halving from `want` down to the floor, the first that fits
        let mut grant = want;
        loop {
            let half = grant.trimmed(grant.bytes / 2);
            if grant.bytes <= GRANT_FLOOR || half.bytes < GRANT_FLOOR {
                break;
            }
            grant = half;
            let need = need_of(grant);
            if self.take_admit(need, 0) || (self.trim_depots(need) > 0 && self.take_admit(need, 0)) {
                return done(grant, need, forced);
            }
        }
        let grant = want.trimmed(grant.bytes.max(GRANT_FLOOR.min(want.bytes)));
        let need = need_of(grant);
        // D3380: the floor's road (`mempolicy::admit::floor`): past the lane and alone-able, it queues with the turn and is
        // held past the lane once no other statement holds bytes (`take_solo`); past the lane otherwise, refused at once
        // D3804: past the lane beside the kept holds; under the one budget applied, a floor also starts once nothing is held
        let on = crate::mem::budget::mode() == crate::mem::budget::Mode::On;
        let road = policy::floor(need, self.kept(), limit, nested, mcx::governor::bar() as u64, on);
        let solo = road.alone;
        let solo_turn = solo.then(|| {
            let ticket = (self.tickets.fetch_add(1, Ordering::AcqRel) % u32::MAX as u64) as u32 + 1;
            Turn { pool: self, ticket, clock: StallClock::new(self.room_for(ticket), MonoStamp::now().as_ns()) }
        });
        loop {
            if let Some(turn) = solo_turn.as_ref() {
                turn.claim(need);
            }
            if self.take_admit(need, 0) {
                return done(grant, need, forced);
            }
            if solo_turn.as_ref().is_some_and(|t| !t.ahead()) && self.take_solo(need) {
                drop(solo_turn);
                return done(grant, need, Admitted::Solo);
            }
            // memory policy 3.2 (D3804): under the one budget applied a floor waits only while another statement holds
            // bytes that could come back; with none held it starts now, past the pool's capacity and B's free bytes
            if road.idle_start && self.take_alone(need) {
                drop(solo_turn);
                return done(grant, need, Admitted::Solo);
            }
            if (road.past && !solo) || waited(t0) >= wait || stop() {
                break;
            }
            self.trim_depots(need);
            if nested && !others.could_give() {
                mcx::governor::resample();
                if self.take_admit(need, 0) {
                    return done(grant, need, forced);
                }
                break;
            }
            w.pause();
        }
        ep.refused();
        self.refusals.fetch_add(1, Ordering::Relaxed);
        let waited_ms = waited(t0).as_millis() as u64;
        self.wait_ns.fetch_add(t0.elapsed_ns(), Ordering::Relaxed);
        if slot {
            self.inflight.fetch_sub(1, Ordering::AcqRel);
            SLOTS_HERE.with(|s| s.set(s.get().saturating_sub(1)));
        }
        Err(AdmitRefused { grant, need, held: self.held(), capacity: self.capacity(), waited_ms, interrupted: stop() })
    }
}

/// A queued waiter's ticket: the oldest one holds the turn (admissions after it leave its need free); dropped (admitted,
/// halving, interrupted) it gives the turn up, and the oldest waiter still queued takes it at its next poll.
struct Turn<'a> {
    pool: &'a GrantPool,
    ticket: u32,
    /// D3214: its stall clock.
    clock: StallClock,
}

impl Turn<'_> {
    /// Another waiter holds the turn.
    fn ahead(&self) -> bool {
        let holder = (self.pool.turn.load(Ordering::Acquire) >> 32) as u32;
        holder != 0 && holder != self.ticket
    }

    fn claim(&self, need: u64) {
        let p = self.pool;
        let mine = (self.ticket as u64) << 32 | need.div_ceil(1 << 20).min(u32::MAX as u64);
        let mut cur = p.turn.load(Ordering::Acquire);
        loop {
            let holder = (cur >> 32) as u32;
            // tickets wrap: older = behind by less than half the ring
            if holder == self.ticket || (cur != 0 && (holder.wrapping_sub(self.ticket) as i32) < 0) {
                return;
            }
            match p.turn.compare_exchange_weak(cur, mine, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return,
                Err(now) => cur = now,
            }
        }
    }
}

/// Memory step 6a: the needs of the admissions waiting for the pool now (the broker's queued work demand).
static QUEUED_BYTES: AtomicU64 = AtomicU64::new(0);

/// Memory step 6a: the bytes the admissions waiting for the pool need now.
pub fn queued_bytes() -> u64 {
    QUEUED_BYTES.load(Ordering::Relaxed)
}

/// A waiting admission's need in `QUEUED_BYTES`, taken back on every exit of the wait (a refusal, an error, an unwind).
struct Queued(u64);

impl Queued {
    fn new(need: u64) -> Queued {
        QUEUED_BYTES.fetch_add(need, Ordering::Relaxed);
        Queued(need)
    }
}

impl Drop for Queued {
    fn drop(&mut self) {
        QUEUED_BYTES.fetch_sub(self.0, Ordering::Relaxed);
    }
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        let p = self.pool;
        let mut cur = p.turn.load(Ordering::Acquire);
        while (cur >> 32) as u32 == self.ticket {
            match p.turn.compare_exchange_weak(cur, 0, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return,
                Err(now) => cur = now,
            }
        }
    }
}

/// D3003 (c): the idle release (`mcx::run_idle_release`, an idle backend's release stage, `pgrust.memory_release_idle`; the
/// hosts' process idle thread): the region depot whole and every parked slot set, so a server that stops running statements
/// hands parked regions back to the pool and the allocator (the depots refill at the next statements, as after a grant's
/// trim). Under `PGRUST_IDLE_TRIM_PRESSURE_ONLY=1` (default off) the trims run only under ladder pressure
/// (`mcx::idle_trim_now`); the backend's own parked regions still go to the depot, where any statement takes them. Under
/// the parked_trim_rung knob (P3) they never run here: the ladder's R2p trims under pressure (`trim_parked`).
pub fn idle_release() {
    let shed = IDLE_SHED.load(Ordering::Acquire);
    if !shed.is_null() {
        // SAFETY: only `set_idle_shed` stores here, always from a `fn()`.
        let f: fn() = unsafe { core::mem::transmute(shed) };
        f();
    }
    if mcx::idle_trim_now() {
        crate::park::REGIONS.trim();
        crate::slots::DEPOT.trim_for(usize::MAX);
    }
    // D4552 (a): the trims' frees land on the account now, at their own site, not at the next statement's start
    memledger::fold();
}

/// P3 (`parked_trim_rung`), the ladder's R2p (`mcx::ladder::set_parked_trim`): the bytes it may trim now (`trim_parked`'s
/// reach): every parked region's account bytes but the newest's (`RegionDepot::trimmable_account_bytes`, never the
/// regions' lengths), and every parked slot set but the newest (the slot depot's own count, as R2a's and the backstop's).
pub fn parked_bytes() -> u64 {
    (crate::park::REGIONS.trimmable_account_bytes() as u64).saturating_add(crate::slots::DEPOT.trimmable_bytes() as u64)
}

/// P3, R2p's trim of up to `want` bytes (R2a's depot step under the knob): the least recently parked regions first, by
/// their account bytes (`RegionDepot::trim_account_but_newest`: what the trim notes is what it takes off the ledger), then
/// the slot sets least recently parked first, the newest of each kept for the next statement; a large mapped region goes
/// to the reaper (no wait in the ladder thread's pass). On the climbing thread, or the give-back service's as its job
/// (`job`, `mcx::ladder::run_parked_job`). The bytes trimmed.
pub fn trim_parked(want: u64, job: bool) -> u64 {
    if want == 0 {
        return 0;
    }
    crate::slots::note_released(0);
    crate::slots::drain_released();
    let mut freed = crate::park::REGIONS.trim_account_but_newest(want as usize) as u64;
    if freed < want {
        freed = freed.saturating_add(crate::slots::DEPOT.trim_but_newest((want - freed) as usize) as u64);
    }
    parked_trim_settle(freed, job);
    // pagepool P3 (C.11): R2p's take is the ladder's, so pressure: the depots' pool bytes go back to the OS too, as at
    // R2a's depot step (`reclaim::ladder_reclaim_steps`). After the settle: its wait for the reaper (`reap::drain`) is
    // what ends the reaper's landing credit for these bytes, so the pool's ask never lands beside it for the same bytes
    crate::mem::region::pool_release(freed);
    // D4552 (a): the trims' frees land on the account now, at their own site
    memledger::fold();
    freed
}

/// After R2p's trims freed some: on the climbing thread the reaper's unmaps waited for (`reap::drain`: at once inside the
/// ladder's pass, D4404), as R2a's depot step. As the give-back service's job (`job`) no wait: what the reaper still owes
/// is the ladder's landing credit, and `reap::drain`'s wait for the service's own whole-region job lane
/// (`giveback::drain_here`) would wait for this very thread.
fn parked_trim_settle(freed: u64, job: bool) {
    if freed != 0 && !job {
        crate::mem::reap::drain();
    }
}

/// PGRUST_PURGE_PRESSURE_BG: the purger's trim under ladder pressure (`mcx::pressure_trim`): the region depot whole and
/// every parked slot set, as the idle release's trims, on the purger's thread whatever the sessions are doing.
pub fn pressure_trim() {
    crate::park::REGIONS.trim();
    crate::slots::DEPOT.trim_for(usize::MAX);
    memledger::fold();
}

/// D3147: the idle backend's own parked Nodes' pool regions (its `Park`, thread-local: no other backend reaches it) put in
/// the depot, which `idle_release` then trims; installed by the layer that owns the backend's `Entry`.
static IDLE_SHED: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

pub fn set_idle_shed(f: fn()) {
    IDLE_SHED.store(f as *mut (), Ordering::Release);
}

/// D3147: the statements admitted to the big lane and holding their reservation (a slot holder's `Reservation::new` to its
/// drop; a cursor parked between fetches still counts, it keeps its bytes), as the grant end line reports them. Not
/// `inflight`: an ungated pool hands the in-flight slot out BEFORE the bytes wait, so the statements queued for bytes hold
/// a slot and none of the lane.
static RUNNING: AtomicU64 = AtomicU64::new(0);

/// D3239 / D3242: whether a grant holder other than the caller's own could still give pool bytes back: `HOLDERS` past
/// `mine`, the caller's own holders (its thread's: its statement, the statements nested in it, a parked cursor of its
/// session; a worker counts its statement's one, held on its leader). Holders, not sessions: two statements of one session
/// end in any order, and the second still gives its bytes back after the first (and its slot) ended.
struct Others {
    mine: u64,
    /// a holder besides the caller's was seen at some poll (its end may have parked regions in the depots)
    seen: bool,
}

impl Others {
    fn here(worker: bool) -> Others {
        // D3910 (a): this thread's suspended portals are no holder a wait could see give back, nor are any others'
        let here = HOLDERS_HERE.with(core::cell::Cell::get).saturating_sub(IDLE_HERE.with(core::cell::Cell::get));
        Others { mine: if worker { here.max(1) } else { here }, seen: false }
    }

    fn could_give(&mut self) -> bool {
        let active = HOLDERS.load(Ordering::Acquire).saturating_sub(IDLE_HOLDERS.load(Ordering::Acquire));
        let some = policy::others_hold(active, self.mine);
        self.seen |= some;
        some
    }
}

/// Ladder step 1.1: bytes already on their way to the pool or to memory (`policy::gate_wait`'s `coming`): a reclaim due,
/// the ladder's relief in flight (R4b's asks, R6's landing, the reaper's unmaps, an asker's hand-off no pass has read:
/// `mcx::ladder::relief_coming`), and the governor's pending revoke asks and cancels within their landing windows.
fn bytes_coming(handed: Option<u64>) -> bool {
    crate::mem::reclaim::pending() || mcx::ladder::relief_coming(handed) || mcx::governor::decide_inputs().pending != 0
}

/// D3147: `RUNNING` now: the statements admitted to the big lane (the grant end line names it).
pub fn running() -> u64 {
    RUNNING.load(Ordering::Acquire)
}

// exactsize D3226's bound past G (`Reservation::grow_fits`) is the memory policy's.
pub(crate) use mempolicy::admit::{granted_floor, within_bounds};

#[cfg(test)]
mod ceilings {
    use super::{ceiling_of, leader_ceiling, Grant};
    use gravity_abi::breaker::{Breaker, BreakerKind, Budget, Spill};
    use gravity_program::{End, Pipeline};

    const MIB: u64 = 1 << 20;

    fn pipe(kind: BreakerKind, spill: Spill) -> Pipeline {
        Pipeline::eligible(0, End::Breaker(Breaker { kind, slot: 0, bytes: 0, budget: Budget::Share, spill }), 1000)
    }

    /// Each breaker's ceiling is PostgreSQL's budget for its node: work_mem for a Sort, top-N or Material, work_mem x
    /// hash_mem_multiplier for a hash node; none for a plain aggregate, a breaker with no spill road, or a grant without
    /// ceilings; a trim keeps the ceilings (it drops the floors).
    #[test]
    fn a_breakers_ceiling_is_its_nodes_postgresql_budget() {
        let g = Grant::of(64 * MIB).with_floors(4 * MIB, 8 * MIB).with_ceilings(4 * MIB, 8 * MIB);
        let parts = Spill::Partitioned { bits: 3 };
        assert_eq!(ceiling_of(g, &pipe(BreakerKind::HashAgg, parts)), 8 * MIB, "a hash aggregate: hash_mem");
        assert_eq!(ceiling_of(g, &pipe(BreakerKind::HashBuild, parts)), 8 * MIB, "a hash build: hash_mem");
        assert_eq!(ceiling_of(g, &pipe(BreakerKind::Distinct, parts)), 8 * MIB, "a hashed DISTINCT: hash_mem");
        assert_eq!(ceiling_of(g, &pipe(BreakerKind::Sort, Spill::Runs)), 4 * MIB, "a Sort: work_mem");
        assert_eq!(ceiling_of(g, &pipe(BreakerKind::Materialize, Spill::Runs)), 4 * MIB, "a Material: work_mem");
        assert_eq!(ceiling_of(g, &pipe(BreakerKind::PlainAgg, Spill::None)), 0, "a plain aggregate: none");
        assert_eq!(ceiling_of(g, &pipe(BreakerKind::HashAgg, Spill::None)), 0, "no spill road (a set operation side): none");
        assert_eq!(ceiling_of(Grant::of(64 * MIB), &pipe(BreakerKind::HashAgg, parts)), 0, "a grant without ceilings: none");
        let t = g.trimmed(16 * MIB);
        assert_eq!((t.sort_floor, t.hash_floor, t.sort_ceil, t.hash_ceil), (0, 0, 4 * MIB, 8 * MIB), "a trim drops the floors, keeps the ceilings");
    }

    /// Each participant its own budget, node by node as PostgreSQL has it: a worker's region the ceiling; the leader's a
    /// hash aggregate's or hashed DISTINCT's Finalize table, one budget; a parallel hash build's shared table (Parallel
    /// Hash) or a Sort's merge, W + 1 of them at width W >= 2; serially one. No participant's share opens past its ceiling.
    #[test]
    fn each_participant_holds_its_own_budget() {
        use BreakerKind::*;
        assert_eq!(leader_ceiling(8 * MIB, 0, HashBuild), 8 * MIB);
        assert_eq!(leader_ceiling(8 * MIB, 1, HashBuild), 8 * MIB);
        assert_eq!(leader_ceiling(8 * MIB, 8, HashBuild), 72 * MIB, "Parallel Hash: hash_mem x the participants");
        assert_eq!(leader_ceiling(8 * MIB, 8, Sort), 72 * MIB, "a Sort's merge: the participants' runs");
        assert_eq!(leader_ceiling(8 * MIB, 8, HashAgg), 8 * MIB, "Finalize HashAgg: one hash_mem");
        assert_eq!(leader_ceiling(8 * MIB, 8, Distinct), 8 * MIB, "a hashed DISTINCT's final table: one hash_mem");
        assert_eq!(leader_ceiling(0, 8, HashBuild), 0, "no ceiling stays none");
        let g = Grant::of(1024 * MIB).with_ceilings(4 * MIB, 8 * MIB);
        let s = g.split_within(1, 1, false, 0, 0, 4 * MIB);
        assert_eq!(s.breaker.bytes as u64, 4 * MIB, "serial: the share held to the ceiling");
        let s = g.split_within(1, 4, false, 0, 0, 8 * MIB);
        assert!(s.worker.bytes as u64 <= 8 * MIB && s.breaker.bytes as u64 <= 40 * MIB, "W4: a worker's share at most one budget, the leader's five ({s:?})");
        let s = g.split_capped(1, 4, false, 0, 0, 8 * MIB, leader_ceiling(8 * MIB, 4, HashAgg));
        assert!(s.worker.bytes as u64 <= 8 * MIB && s.breaker.bytes as u64 <= 8 * MIB, "W4 hash aggregate: the leader's Finalize table one budget ({s:?})");
    }
}

#[cfg(test)]
mod d3147 {
    use super::{granted_floor, within_bounds, GRANT_FLOOR};

    const GIB: u64 = 1 << 30;
    const MIB: u64 = 1 << 20;

    /// The bound past G is the user's cap alone: no share of the pool among the running statements bounds a statement's
    /// draws (each breaker's own bound is its PostgreSQL per-node budget), so beside any number of others a draw is refused
    /// only past the user's fraction, never under the reservation plus one floor (D3064: a 4 KB draw past G was 53200 when
    /// admission had reserved past the bound).
    #[test]
    fn the_bound_past_g_is_the_users_cap_alone() {
        let floor = granted_floor(GIB, 8 * GIB);
        assert!(within_bounds(80 * GIB, floor, 0), "no cap: no bound (the pool's own room and wait)");
        assert!(within_bounds(6 * GIB, floor, 6 * GIB), "within the user's cap");
        assert!(!within_bounds(9 * GIB, floor, 6 * GIB), "past the user's cap and its floor");
        assert!(within_bounds(8 * GIB + GRANT_FLOOR, floor, 6 * GIB), "reserved past the cap: one floor past the reservation");
        assert!(!within_bounds(8 * GIB + GRANT_FLOOR + 1, floor, 6 * GIB));
    }

    /// exactsize D3226: a statement's own region opens past G are never refused by the bound past G, beside any number of
    /// statements. A two-aggregate join at W8 (box 48): a 7870 MiB grant cut into nine region shares that round past
    /// it, an Estimate reservation of 4408 MiB holding five of them and 37.7 MB beside, the other four opened past G on
    /// a 14757 MiB lane; under a half-lane cap the floor still lets them open.
    #[test]
    fn region_opens_within_the_grant_are_never_refused_beside_another_statement() {
        let (granted, lane) = (7870 * MIB, 14757 * MIB);
        let region = crate::park::region_bytes((granted / 9) as usize) as u64;
        assert!(9 * region > granted, "the nine regions round past the grant");
        let reserved = 5 * region + 37_747_776;
        let floor = granted_floor(granted, reserved);
        let mut grown = 0;
        for open in 0..4 {
            assert!(within_bounds(reserved + grown + region, floor, 0), "open {open} past G, no cap");
            assert!(within_bounds(reserved + grown + region, floor, lane / 2), "open {open} past G under a half-lane cap");
            grown += region;
        }
        assert!(lane / 2 < floor, "the case: the half-lane cap is under the grant");
        assert!(!within_bounds(floor + 1, floor, lane / 2), "past the floor the user's cap bounds again");
        assert_eq!(granted_floor(1024 * MIB, 3072 * MIB), 3072 * MIB + GRANT_FLOOR, "reserved past the grant: one floor past the reservation");
    }
}

#[cfg(test)]
mod d3088 {
    use super::{cut_share, Share, SHARE_FLOOR};

    const GIB: u64 = 1 << 30;

    /// D3088: an on-demand open's cut (D2242) keeps the part its breaker was charged for its PostgreSQL floor: a pulled
    /// CTE's HashAgg charged 2.5 GiB stays 2.5 GiB beside a 1 GiB room, where it was cut to 1 GiB and grew to its floor.
    #[test]
    fn the_cut_keeps_the_charged_floor() {
        let share = Share { bytes: (5 * GIB / 2) as u32 };
        assert_eq!(cut_share(share, GIB, 5 * GIB / 2), share, "charged its modeled table: kept whole");
        assert_eq!(cut_share(share, GIB, 0).bytes as u64, GIB, "no floor charged: the room");
        assert_eq!(cut_share(share, GIB, 3 * GIB / 2).bytes as u64, 3 * GIB / 2, "the charged part beside a smaller room");
        assert_eq!(cut_share(share, 4 * GIB, 0), share, "never past the share");
        assert_eq!(cut_share(share, 0, 0).bytes, SHARE_FLOOR as u32 & !7, "at least SHARE_FLOOR");
        assert_eq!(cut_share(Share { bytes: 1 << 20 }, 0, 8 * GIB).bytes, 1 << 20, "a charge past the share keeps the share");
        assert_eq!(cut_share(share, GIB + 3, 0).bytes % 8, 0, "8-aligned");
    }
}

#[cfg(test)]
mod w1 {
    use super::{set_mem_wait, MemWait};
    use core::cell::Cell;

    thread_local! {
        static STARTS: Cell<u32> = const { Cell::new(0) };
        static ENDS: Cell<u32> = const { Cell::new(0) };
    }

    fn start() {
        STARTS.with(|c| c.set(c.get() + 1));
    }

    fn end() {
        ENDS.with(|c| c.set(c.get() + 1));
    }

    fn seen() -> (u32, u32) {
        (STARTS.with(Cell::get), ENDS.with(Cell::get))
    }

    /// W1: a wait reports once, at its first pause, and ends with the wait; a wait nested in it (the same thread) neither
    /// reports again nor ends the outer event. Counted per thread, so other tests' waits never reach these counters.
    #[test]
    fn a_memory_wait_reports_from_its_first_pause_to_its_end_and_a_nested_wait_never_ends_it() {
        set_mem_wait(start, end);
        {
            let mut outer = MemWait::default();
            assert_eq!(seen(), (0, 0), "nothing before the first pause");
            outer.pause();
            outer.pause();
            assert_eq!(seen(), (1, 0), "one report for the whole wait");
            {
                let mut inner = MemWait::default();
                inner.pause();
            }
            assert_eq!(seen(), (1, 0), "the nested wait neither reports nor ends the outer event");
        }
        assert_eq!(seen(), (1, 1), "the event ends with the wait");
        {
            let _never = MemWait::default();
        }
        assert_eq!(seen(), (1, 1), "a wait that never paused reports nothing");
    }
}

#[cfg(test)]
mod top_rows {
    use super::{footprint, footprint_regions, share, top_share_cap, Grant};
    use gravity_abi::breaker::{Breaker, BreakerKind, Budget, Spill};
    use gravity_program::{Arena, Code, End, Layout, OutCol, Pipeline, Program, Reason, SiteDesc, WritePlan};

    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;

    unsafe extern "C-unwind" fn noop(_: *mut gravity_abi::Ctx) -> i64 {
        0
    }

    /// A row top as lowerv4 runs.topk_rows describes it: a parallel scan of 100 M rows into a Sort breaker over the share,
    /// its run row 24 bytes, keeping `top` rows before it cuts (0: none published), then its emit.
    fn program(top: u64) -> Program {
        let st = Arena::new(1 << 14);
        let sites = [SiteDesc::range_source(st.handle(), 3840, &[1, 2])];
        let sort = End::Breaker(Breaker { kind: BreakerKind::Sort, slot: 0, bytes: 0, budget: Budget::Share, spill: Spill::Runs });
        let scan = Pipeline::eligible(0, sort, 100_000_000).storing_rows(24).keeping_rows(top);
        let emit = Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming_one(0);
        Program::with_pipelines(Layout { nsites: 1, out_natts: 3, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 }, &sites, &core::array::from_fn::<_, 3, _>(|i| OutCol::new(["c0", "c1", "c2"][i], 20, -1, 8, true)), WritePlan::SELECT, [(scan, Code::hand(noop)), (emit, Code::hand(noop))])
    }

    /// A row top under LIMIT 1024 (its soft cap 4096 rows of 24 bytes, twice for the copies: one 2 MiB step) at W8 and a 1
    /// GiB sort floor: each worker's share is 2 MiB and the leader's (it merges the nine runs) at most nine of them, not
    /// the floor, and the floor raises none of them;
    /// the same top with no bound published keeps the floor's 1 GiB each, as before.
    #[test]
    fn a_bounded_row_top_reserves_its_rows_not_the_sort_floor() {
        let grant = Grant::of(64 * MIB).with_floors(GIB, GIB);
        let (bounded, open) = (program(4096), program(0));
        assert_eq!(top_share_cap(&bounded.pipelines[0]), 2 * MIB);
        assert_eq!(top_share_cap(&open.pipelines[0]), u64::MAX, "no bound: no cap");
        let s = share(&bounded, 0, grant, 8);
        assert_eq!(s.worker.bytes as u64, 2 * MIB, "each worker's share");
        assert!((2 * MIB..=9 * 2 * MIB).contains(&(s.breaker.bytes as u64)), "the leader merges nine runs: at most nine caps");
        assert!(footprint_regions(grant, &bounded, 8) <= (9 + 8) * 2 * MIB, "the leader's nine caps and eight 2 MiB shares");
        assert!(footprint(grant, &bounded, 8) < 64 * MIB, "a few MiB a participant beside the arenas");
        let s = share(&open, 0, grant, 8);
        assert_eq!(s, grant.split_for_at_least(1, 8, false, GIB), "unbounded: the floored split as before");
        assert_eq!((s.breaker.bytes as u64, s.worker.bytes as u64), (GIB, GIB));
        assert_eq!(footprint_regions(grant, &open, 8), 9 * GIB, "nine 1 GiB floors");
    }
}

#[cfg(test)]
mod e3b {
    use super::{footprint_regions, share, spooled, Grant, Overdraw, POOL, SHARE_FLOOR};
    use gravity_abi::breaker::{Breaker, BreakerKind, Budget, Spill};
    use gravity_program::protos::{Proto, ProtoKind, V4Descs};
    use gravity_program::{Arena, Code, End, Layout, OutCol, Pipeline, Program, Reason, SiteDesc, WritePlan};

    const MIB: u64 = 1 << 20;

    unsafe extern "C-unwind" fn noop(_: *mut gravity_abi::Ctx) -> i64 {
        0
    }

    /// A parallel scan of 10 M rows into a Materialize breaker over the share, then its reader; `spool`: its fill appends to
    /// an exec Spool object (lowerv4 rerun/spool rec.rs `declare`: a Spool protocol peered to pipeline 0), else no protocol
    /// (a slot-run spool).
    fn program(spool: bool) -> Program {
        let st = Arena::new(1 << 14);
        let sites = [SiteDesc::range_source(st.handle(), 3840, &[1, 2])];
        let mat = End::Breaker(Breaker { kind: BreakerKind::Materialize, slot: 0, bytes: 0, budget: Budget::Share, spill: Spill::Runs });
        let scan = Pipeline::eligible(0, mat, 10_000_000).storing_rows(24);
        let read = Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming_one(0);
        let mut p = Program::with_pipelines(Layout { nsites: 1, out_natts: 3, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 }, &sites, &core::array::from_fn::<_, 3, _>(|i| OutCol::new(["c0", "c1", "c2"][i], 20, -1, 8, true)), WritePlan::SELECT, [(scan, Code::hand(noop)), (read, Code::hand(noop))]);
        if spool {
            p.set_v4(&V4Descs::new(vec![Proto::new(ProtoKind::Spool, 1).peer(0).arg(48)])); // alloc: Cold
        }
        p
    }

    /// A Materialize breaker whose rows go to an exec Spool object opens at SHARE_FLOOR, each participant, and
    /// the footprint reserves that; with no Spool protocol (a slot-run spool) it keeps the work_mem floor's share as before.
    #[test]
    fn a_spooled_materialize_reserves_its_header_not_work_mem() {
        let grant = Grant::of(256 * MIB).with_floors(64 * MIB, 64 * MIB);
        let (spool, run) = (program(true), program(false));
        assert!(spooled(&spool, 0, &spool.pipelines[0]) && !spooled(&run, 0, &run.pipelines[0]));
        assert!(!spooled(&spool, 1, &spool.pipelines[1]), "the reader ends in no Materialize breaker");
        for w in [1u16, 8] {
            let s = share(&spool, 0, grant, w);
            assert_eq!((s.breaker.bytes, s.worker.bytes), (SHARE_FLOOR, SHARE_FLOOR), "W{w}: the header's floor, leader and workers");
            let r = share(&run, 0, grant, w);
            assert!(r.breaker.bytes as u64 >= 64 * MIB, "W{w}: a slot-run spool keeps its floor ({})", r.breaker.bytes);
            assert!(footprint_regions(grant, &spool, w) < footprint_regions(grant, &run, w) / 64, "W{w}: the footprint reserves the floor");
        }
    }

    /// D1378 + D622 (`overdraw_take`, one code path): an overdraft is taken at once whatever the pool holds
    /// (no wait, no refusal: here an unarmed pool), counted for its site alone, and not `kept`: its bytes go back to the
    /// pool when the hold drops.
    #[test]
    fn an_overdraft_is_never_refused_and_is_counted_for_its_site() {
        let (seen0, nested0) = (POOL.overdraws()[Overdraw::Seen as usize], POOL.overdraws()[Overdraw::Nested as usize]);
        let h = POOL.overdraw(3 * MIB, None, Overdraw::Seen);
        assert_eq!((h.pool, h.kept, h.past.is_none()), (3 * MIB, false, true), "a pool hold, not kept, no statement to count past G");
        let (seen1, nested1) = (POOL.overdraws()[Overdraw::Seen as usize], POOL.overdraws()[Overdraw::Nested as usize]);
        assert_eq!((seen1.0 - seen0.0, seen1.1 - seen0.1), (1, 3 * MIB), "one overdraft of 3 MiB for the seen set");
        assert_eq!(nested1, nested0, "no other site's");
        assert_eq!((Overdraw::Seen.name(), Overdraw::Nested.name()), ("seen_past", "nested_past"));
        drop(h);
    }
}

#[cfg(test)]
mod d3798_s2 {
    use super::{decide_start, Grant, StartSize};

    /// D3798 S2: a grant no admission decided starts small, as before S2; the start rides the grant through a trim (the rule
    /// itself is mempolicy's `admit::decide_start`, tested there).
    #[test]
    fn the_start_rides_the_grant() {
        assert_eq!(decide_start(0, None), StartSize::Small);
        assert_eq!(Grant::of(1 << 30).start, StartSize::Small, "a grant no admission decided: small, as before S2");
        let g = Grant::of(1 << 30).with_start(StartSize::Share).with_floors(1, 2).with_ceilings(3, 4).with_exact(1);
        assert_eq!(g.trimmed(1 << 20).start, StartSize::Share, "a trim keeps the start");
    }
}

#[cfg(test)]
mod g1b {
    use super::{Held, Overdraw, POOL};

    const MIB: u64 = 1 << 20;

    /// G1-B: a worker's draw past W and G (no statement count) shows on its claim's memory ledger account while held, as
    /// much as its pool part, and nothing once it parks (`settle`) or drops.
    #[test]
    fn a_workers_draw_past_g_shows_on_its_claims_ledger_account() {
        std::thread::spawn(|| {
            memledger::bind_backend(3992, 0);
            let a = memledger::current();
            // C8 move 3c: the claim (backed or not): a draw past G is pages its hold commits, so its room is backed and the
            // account's unbacked room shows none of it
            let shown = || memledger::pool_reserved(a);
            let unbacked = || memledger::account_row(a).pool_reserved_bytes;
            let mut h = Held::grown_worker(8 * MIB);
            assert_eq!((h.pool, h.past.is_none(), shown()), (8 * MIB, true, (8 * MIB) as i64), "no statement count; the ledger shows it");
            assert_eq!(unbacked(), 0, "backed: counted once, as the pages it holds");
            h.uncount(3 * MIB);
            assert_eq!(shown(), (3 * MIB) as i64, "cut with its pool part");
            h.settle();
            assert_eq!(shown(), 0, "a parked hold is no statement's");
            h.show_worker(2 * MIB);
            h.pool = 0; // the pool never took these bytes (no `take` here): its drop releases none
            drop(h);
            assert_eq!(shown(), 0, "given back at the drop");
            let h = POOL.overdraw(MIB, None, Overdraw::Seen);
            assert_eq!(shown(), MIB as i64, "a worker's overdraft too");
            drop(h);
            assert_eq!((shown(), unbacked()), (0, 0));
            memledger::unbind_backend();
        })
        .join()
        .unwrap();
    }
}

#[cfg(test)]
mod d4098 {
    use super::*;

    /// A pool of its own (the process pool's statics, fresh).
    fn fresh() -> GrantPool {
        GrantPool { capacity: AtomicU64::new(0), held: AtomicU64::new(0), kept: AtomicU64::new(0), inflight: AtomicU64::new(0), max_inflight: AtomicU64::new(0), waits: AtomicU64::new(0), wait_ns: AtomicU64::new(0), trims: AtomicU64::new(0), sheds: AtomicU64::new(0), refusals: AtomicU64::new(0), reserve: AtomicU64::new(0), small_held: AtomicU64::new(0), small_admits: AtomicU64::new(0), small_fallbacks: AtomicU64::new(0), small_overflows: AtomicU64::new(0), buffer: AtomicU64::new(0), buffer_b: AtomicU64::new(0), small_draws: AtomicU64::new(0), small_waits: AtomicU64::new(0), small_refusals: AtomicU64::new(0), borrows: AtomicU64::new(0), borrowed: AtomicU64::new(0), unadmitted: AtomicU64::new(0), unadmitted_bytes: AtomicU64::new(0), overdraw_n: [const { AtomicU64::new(0) }; Overdraw::N], overdraw_bytes: [const { AtomicU64::new(0) }; Overdraw::N], ended: AtomicU64::new(0), turn: AtomicU64::new(0), tickets: AtomicU64::new(0), solo: core::sync::atomic::AtomicBool::new(false) }
    }

    /// D4098 (born red): a draw past the pool inside a canceled kernel call stops waiting on the reaper at once. The
    /// draw's trim waits for the reaper to give back what it owes (`reap::drain`) before it reads the pool's room; that wait
    /// polled nothing, so a canceled statement's draw sat through the reaper's whole batch of unmaps (a 323 ms poll gap
    /// held by 'grant take_wait' under memory pressure, m-stress 79bef978b73). Here the reaper is held mid-batch for a
    /// second: the canceled draw refuses as the cancel well inside it.
    #[test]
    fn a_canceled_draw_stops_waiting_on_the_reaper() {
        let pool = fresh();
        pool.arm(64 << 20);
        let byte = core::sync::atomic::AtomicU8::new(cancelpoll::ABORT_BIT);
        let busy = crate::mem::reap::hold_busy();
        let release = std::thread::spawn(move || {
            std::thread::sleep(core::time::Duration::from_secs(1));
            drop(busy);
        });
        let t0 = std::time::Instant::now();
        let (took, seen) = cancelpoll::scoped(&byte, || (pool.take_wait(128 << 20, false), cancelpoll::seen()));
        let waited = t0.elapsed();
        release.join().unwrap();
        assert!(!took && seen, "refused as the statement's cancel (took {took}, seen {seen})");
        assert!(waited < core::time::Duration::from_millis(250), "the cancel ended the wait on the reaper: waited {waited:?}");
    }
}

#[cfg(test)]
mod grow_mode {
    use super::{ceiling_of, leader_ceiling, Grant, StartSize};
    use gravity_abi::breaker::{Breaker, BreakerKind, Budget, Spill};
    use gravity_program::{End, Pipeline};

    const MIB: u64 = 1 << 20;

    fn pipe(kind: BreakerKind, spill: Spill) -> Pipeline {
        Pipeline::eligible(0, End::Breaker(Breaker { kind, slot: 0, bytes: 0, budget: Budget::Share, spill }), 1000)
    }

    /// Grow mode rides the grant: off unless an admission set it (flag off = today's grants), kept by a trim and by the
    /// estimate start's floors (the PostgreSQL budgets, the caps of its shares).
    #[test]
    fn grow_mode_rides_the_grant_and_is_off_by_default() {
        assert!(!Grant::of(64 * MIB).grow, "a grant no grow-mode admission made: off");
        let g = Grant::of(64 * MIB).with_floors(4 * MIB, 8 * MIB).with_ceilings(4 * MIB, 8 * MIB).with_start(StartSize::Small).with_grow(true);
        assert!(g.trimmed(16 * MIB).grow, "a trim keeps it");
        assert!(g.trimmed(16 * MIB).with_floors(4 * MIB, 8 * MIB).grow, "the estimate start keeps it");
        assert!(!Grant::of(64 * MIB).with_grow(false).trimmed(MIB).grow);
    }

    /// Grow mode with work_mem unset: the ceilings are the region wall (`mempolicy::grow::grow_ceiling`), so no breaker's
    /// ceiling is its work_mem budget; a breaker with no spill road still has none (its roads as today); an explicit
    /// work_mem keeps PostgreSQL's per-node ceilings (the exec admission leaves them, `grow_cap`).
    #[test]
    fn with_work_mem_unset_a_breakers_ceiling_is_the_region_wall() {
        let wall = super::super::grow::MAX_REGION;
        let c = mempolicy::grow::grow_ceiling(0, wall);
        let today = Grant::of(64 * MIB).with_ceilings(4 * MIB, 8 * MIB);
        let grow = today.with_grow(true).with_ceilings(c, c);
        let parts = Spill::Partitioned { bits: 3 };
        assert_eq!(ceiling_of(today, &pipe(BreakerKind::HashAgg, parts)), 8 * MIB, "today: hash_mem");
        assert_eq!(ceiling_of(grow, &pipe(BreakerKind::HashAgg, parts)), wall, "grow: the region wall");
        assert_eq!(ceiling_of(grow, &pipe(BreakerKind::Sort, Spill::Runs)), wall, "grow: a Sort's too");
        assert_eq!(ceiling_of(grow, &pipe(BreakerKind::HashAgg, Spill::None)), 0, "no spill road: none, as today");
        assert_eq!(leader_ceiling(wall, 8, BreakerKind::HashBuild), wall, "the leader's at the wall too");
        assert_eq!(ceiling_of(today.with_grow(true), &pipe(BreakerKind::HashAgg, parts)), 8 * MIB, "grow + explicit work_mem: hash_mem");
    }
}

#[cfg(test)]
mod p3_tests {
    /// P3 with H3 step (4): R2p's trim run as the give-back service's job never waits on the service's thread, neither for
    /// the reaper (what it still owes is the ladder's landing credit) nor, through `reap::drain`, for the service's own
    /// whole-region job lane (`giveback::drain_here` would wait for this very thread up to give_back_spill_wait_ms). Here
    /// the reaper is held mid-batch for a second: the job's settle returns at once; on a climbing thread outside the
    /// ladder's pass it waits for the reaper, as R2a's depot step. Born red while the job waited as a climbing thread does.
    #[test]
    fn p3_the_give_back_services_r2p_job_never_waits_for_the_reaper() {
        use core::time::Duration;
        let busy = crate::mem::reap::hold_busy();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(1));
            drop(busy);
        });
        let t0 = std::time::Instant::now();
        super::parked_trim_settle(1, true);
        let job = t0.elapsed();
        let t1 = std::time::Instant::now();
        super::parked_trim_settle(1, false);
        let climbing = t1.elapsed();
        release.join().unwrap();
        assert!(job < Duration::from_millis(100), "the service's job waited for the reaper: {job:?}");
        assert!(climbing >= Duration::from_millis(500), "a climbing thread outside the pass waits for the reaper: {climbing:?}");
    }
}
