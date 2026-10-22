//! The Governator's reconciliation check (`ladder_reconcile`, a debug knob, default 0): every term the ladder's reads,
//! rooms and the grant pool's capacity are built from carries the kind of bytes it counts (`Kind`) and the consumers
//! that read it (`Consumer`); each ladder sample checks the terms against those consumers and against the limiting
//! cgroup's own split of memory.current (`Split`). Nothing here acts: a violation is a row in the ladder's view.
//!
//! The rules, each a row per term or check (`Row`), with the excess bytes of the sample that broke it:
//! 1. a term of a kind its consumer may not read: a mapped length, a reservation or an ask where resident bytes belong
//!    (an additive term over the tolerance; a bound, `TermDef::bound`, that bound the consumer under the resident room);
//! 2. a term a resident-only consumer reads larger than memory.current;
//! 3. each consumer's terms against the split they stand for (`Check`; its name carries the split, so a row reads
//!    without the code);
//! 4. the allocator's free bytes counted as used (B's used side holds `memledger::arena::retained`).
//!
//! The tolerance is max(64 MiB, 2 % of the limit) plus one sample's growth of memory.current (`tolerance`): the terms
//! are published where they are computed, at their own cadence, so two of them can be one sample apart.

use crate::consts::MIB;

/// What a term's bytes are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// In memory now (the cgroup's anon, shmem or file pages, or a residency pass's count of them).
    Resident,
    /// An address range's length: untouched or purged pages are in it.
    Mapped,
    /// A promise of room (a grant, a claim, a capacity): no page behind it yet.
    Reserved,
    /// A ledger account's committed bytes (resident unless the residency pass says otherwise).
    Account,
    /// Bytes asked of another thread that has not answered yet.
    Asked,
    /// A sum of parts of two or more kinds (the audit's B-7: B's used side adds the work pool's whole grants, Reserved,
    /// and shared_buffers' extents, Mapped, to the accounts). No consumer may read one: rule 1 names every reader.
    Mixed,
}

impl Kind {
    pub const fn name(self) -> &'static str {
        match self {
            Kind::Resident => "resident",
            Kind::Mapped => "mapped",
            Kind::Reserved => "reserved",
            Kind::Account => "account",
            Kind::Asked => "asked",
            Kind::Mixed => "mixed",
        }
    }

    const fn bit(self) -> u8 {
        1 << self as u8
    }
}

/// Who reads the terms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Consumer {
    /// The shrink rungs' read (memory.current less the clean inactive page cache), netted by the hook pass's relief.
    LadderRead,
    /// The landing the background pass nets from the read (`mcx::ladder::unmap_landing`).
    Landing,
    /// A raise's room: the window's low less the landing, at most the claims' low.
    RaiseRoom,
    /// The raise window's low: the samples' rooms less their landing, and the fresh maps between samples.
    RaiseWindow,
    /// The grant pool's capacity and what it counts as held.
    PoolCapacity,
    /// The kill reads (R6, the cancel line, the block gate): the governed resident.
    KillRead,
    /// The cache bytes the ladder's takes cannot reach (`mcx::ladder::caches_stuck`).
    Stuck,
}

pub const CONSUMERS: usize = 7;

impl Consumer {
    pub const ALL: [Consumer; CONSUMERS] = [Consumer::LadderRead, Consumer::Landing, Consumer::RaiseRoom, Consumer::RaiseWindow, Consumer::PoolCapacity, Consumer::KillRead, Consumer::Stuck];

    pub const fn name(self) -> &'static str {
        match self {
            Consumer::LadderRead => "ladder_read",
            Consumer::Landing => "landing",
            Consumer::RaiseRoom => "raise_room",
            Consumer::RaiseWindow => "raise_window",
            Consumer::PoolCapacity => "pool_capacity",
            Consumer::KillRead => "kill_read",
            Consumer::Stuck => "stuck",
        }
    }

    /// The kinds the consumer may read (a bit per `Kind`).
    pub const fn allowed(self) -> u8 {
        match self {
            Consumer::PoolCapacity => Kind::Resident.bit() | Kind::Reserved.bit() | Kind::Account.bit(),
            Consumer::Stuck => Kind::Resident.bit() | Kind::Account.bit(),
            _ => Kind::Resident.bit(),
        }
    }

    /// Whether the consumer reads resident bytes only (rule 2).
    pub const fn resident_only(self) -> bool {
        self.allowed() == Kind::Resident.bit()
    }

    const fn bit(self) -> u8 {
        1 << self as u8
    }
}

/// The terms, published where they are computed (`mcx::reconcile::put`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Term {
    LandingReaper,
    LandingOwn,
    LandingGiveback,
    LandingPool,
    LandingParkedJobs,
    LandingBuffers,
    LadderRead,
    Room,
    RoomLow,
    MapsSince,
    ClaimLow,
    BFree,
    PoolCapacity,
    PoolHeld,
    PoolRoom,
    PoolEvictable,
    Stuck,
    BUsed,
    BRetained,
    Governed,
    DepotMapped,
    DepotResident,
    BufGrown,
    CapB,
    CapRHigh,
    CapFixed,
    CapUnattributed,
    CapSharedBuffers,
    CapCachesKept,
}

pub const TERMS: usize = 29;

/// A term's kind, the consumers that read it, and whether it bounds them (a cap: too small is the error) rather than
/// adding to or taking from them (too large is).
#[derive(Clone, Copy, Debug)]
pub struct TermDef {
    pub name: &'static str,
    pub kind: Kind,
    pub feeds: u8,
    pub bound: bool,
}

const fn def(name: &'static str, kind: Kind, feeds: &[Consumer], bound: bool) -> TermDef {
    let mut bits = 0u8;
    let mut i = 0;
    while i < feeds.len() {
        bits |= feeds[i].bit();
        i += 1;
    }
    TermDef { name, kind, feeds: bits, bound }
}

use Consumer as C;

/// Indexed by `Term as usize`.
pub const DEFS: [TermDef; TERMS] = [
    // the driver's reap::landing_credit, part by part: the reaper's credit (each handed region's resident bytes at the
    // residency pass's last read), the cache runs waiting for the page releaser and the give-back service's jobs (their
    // lengths), the page pool's R1 ask its purger has not served
    def("landing.reaper", Kind::Resident, &[C::Landing, C::RaiseWindow], false),
    def("landing.own", Kind::Mapped, &[C::Landing, C::RaiseWindow], false),
    def("landing.giveback", Kind::Mapped, &[C::Landing, C::RaiseWindow], false),
    def("landing.pool", Kind::Asked, &[C::Landing, C::RaiseWindow], false),
    // R2p's jobs posted to the give-back service (the parked regions' and slot sets' lengths): asked of the service,
    // which has not answered (the audit's D-12: the label said Mapped)
    def("landing.parked_jobs", Kind::Asked, &[C::Landing, C::RaiseWindow], false),
    // shared_buffers' give-back on its way (the extents asked of the background writer), in the hook pass's relief
    def("landing.buffers", Kind::Asked, &[C::LadderRead], false),
    def("ladder.read", Kind::Resident, &[C::LadderRead], false),
    // the line less the read at the last sample
    def("room", Kind::Resident, &[C::RaiseRoom, C::RaiseWindow], false),
    def("room_low", Kind::Resident, &[C::RaiseRoom], false),
    // what the statements mapped since the last sample (inline_on_draw's note_mapped; 0 with it off)
    def("maps_since", Kind::Mapped, &[C::RaiseWindow], false),
    def("claim_low", Kind::Reserved, &[C::RaiseRoom], true),
    // B's free bytes, used counted at least resident (free_B; under the broker its draws' room bounds the claims' window)
    def("b.free", Kind::Reserved, &[C::RaiseRoom], true),
    def("pool.capacity", Kind::Reserved, &[C::PoolCapacity], false),
    def("pool.held", Kind::Reserved, &[C::PoolCapacity], false),
    // the room the pool's tick arms with (the limit less the headroom margin, less the ladder's read)
    def("pool.room", Kind::Resident, &[C::PoolCapacity], false),
    def("pool.evictable", Kind::Account, &[C::PoolCapacity], false),
    def("stuck", Kind::Account, &[C::Stuck, C::PoolCapacity], false),
    // B's used side: every account's bytes and the allocator's retained ones, the work pool's row its whole grants
    // (Reserved) and shared_buffers' its extents (Mapped): mixed (B-7)
    def("b.used", Kind::Mixed, &[C::PoolCapacity], false),
    def("b.retained", Kind::Account, &[C::PoolCapacity], false),
    def("kill.governed", Kind::Resident, &[C::KillRead], false),
    // the parked regions' lengths (the region depot's count) and the parked and slot_depot accounts less what the
    // residency pass found not resident
    def("depot.mapped", Kind::Mapped, &[], false),
    def("depot.resident", Kind::Account, &[C::Stuck], false),
    // the shared_buffers account above its configured size (buf_grow's extents): their lengths, untouched pages in them
    // (the audit's E6: the label said Account)
    def("buf.grown", Kind::Mapped, &[], false),
    // the grant pool's capacity at its tick, term by term (sizes::work_pool_capacity: B less R_high, the fixed
    // consumers, the unattributed resident bytes, shared_buffers and what the caches keep)
    def("pool.cap.b", Kind::Reserved, &[C::PoolCapacity], false),
    def("pool.cap.r_high", Kind::Reserved, &[C::PoolCapacity], false),
    def("pool.cap.fixed", Kind::Account, &[C::PoolCapacity], false),
    // resident less B's used side (b.used, mixed): mixed too (B-7)
    def("pool.cap.unattributed", Kind::Mixed, &[C::PoolCapacity], false),
    // shared_buffers' live extents (`shared_live`): lengths (E6, the label said Account)
    def("pool.cap.shared_buffers", Kind::Mapped, &[C::PoolCapacity], false),
    def("pool.cap.caches_kept", Kind::Account, &[C::PoolCapacity], false),
];

impl Term {
    pub const ALL: [Term; TERMS] = [
        Term::LandingReaper,
        Term::LandingOwn,
        Term::LandingGiveback,
        Term::LandingPool,
        Term::LandingParkedJobs,
        Term::LandingBuffers,
        Term::LadderRead,
        Term::Room,
        Term::RoomLow,
        Term::MapsSince,
        Term::ClaimLow,
        Term::BFree,
        Term::PoolCapacity,
        Term::PoolHeld,
        Term::PoolRoom,
        Term::PoolEvictable,
        Term::Stuck,
        Term::BUsed,
        Term::BRetained,
        Term::Governed,
        Term::DepotMapped,
        Term::DepotResident,
        Term::BufGrown,
        Term::CapB,
        Term::CapRHigh,
        Term::CapFixed,
        Term::CapUnattributed,
        Term::CapSharedBuffers,
        Term::CapCachesKept,
    ];

    pub const fn def(self) -> TermDef {
        DEFS[self as usize]
    }

    pub const fn name(self) -> &'static str {
        DEFS[self as usize].name
    }

    /// The consumers it feeds that may not read its kind (a bit per `Consumer`; rule 1's).
    pub const fn wrong_feeds(self) -> u8 {
        let d = DEFS[self as usize];
        let mut bits = 0u8;
        let mut i = 0;
        while i < CONSUMERS {
            let c = Consumer::ALL[i];
            if d.feeds & c.bit() != 0 && c.allowed() & d.kind.bit() == 0 {
                bits |= c.bit();
            }
            i += 1;
        }
        bits
    }

    /// A headroom: a line or limit less a read (the raise room, its window's low, the pool tick's room). It holds no
    /// bytes, so it passes memory.current whenever the read is under half the line; rule 3 (`Check::RoomLow`) checks it.
    pub const fn headroom(self) -> bool {
        matches!(self, Term::Room | Term::RoomLow | Term::PoolRoom)
    }

    /// Whether a resident-only consumer adds or takes it (rule 2's); never a headroom's.
    pub const fn resident_read(self) -> bool {
        if self.headroom() {
            return false;
        }
        let d = DEFS[self as usize];
        let mut i = 0;
        while i < CONSUMERS {
            let c = Consumer::ALL[i];
            if !d.bound && d.feeds & c.bit() != 0 && c.resident_only() {
                return true;
            }
            i += 1;
        }
        false
    }
}

/// The landing terms the background pass nets (`Check::Landing`'s sum).
pub const LANDING: [Term; 5] = [Term::LandingReaper, Term::LandingOwn, Term::LandingGiveback, Term::LandingPool, Term::LandingParkedJobs];

/// The limiting cgroup at the sample, with the ladder's line and the limit (bytes; memory.stat's fields).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Split {
    pub current: u64,
    pub anon: u64,
    pub shmem: u64,
    /// memory.stat's file (shmem is in it).
    pub file: u64,
    /// The clean inactive page cache: inactive_file less file_dirty and file_writeback.
    pub inactive: u64,
    /// The clean page cache: active_file + inactive_file less file_dirty and file_writeback.
    pub clean: u64,
    pub line: u64,
    pub limit: u64,
}

impl Split {
    /// The ladder's read of the cgroup: memory.current less the clean inactive page cache.
    pub const fn ladder(&self) -> u64 {
        self.current.saturating_sub(self.inactive)
    }

    /// The governed resident: anon + shmem, at least memory.current less the clean page cache.
    pub const fn governed(&self) -> u64 {
        let own = self.anon.saturating_add(self.shmem);
        let unreclaimable = self.current.saturating_sub(self.clean);
        if own > unreclaimable { own } else { unreclaimable }
    }

    /// The room under the line by the ladder's read.
    pub const fn room(&self) -> u64 {
        self.line.saturating_sub(self.ladder())
    }

    /// The room under the limit by the ladder's read.
    pub const fn free(&self) -> u64 {
        self.limit.saturating_sub(self.ladder())
    }
}

/// Rule 3's checks: a consumer's terms against the split they stand for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Check {
    /// The landing netted from the read is at most the read.
    Landing,
    /// The published ladder read is the split's (at least the governed resident, as `ladder_read_of`).
    LadderRead,
    /// The published kill read is the split's governed resident.
    KillRead,
    /// The raise window's low is at most the room the split shows now.
    RoomLow,
    /// The grant pool's free (capacity less held) is at most the split's room under the limit plus the caches' evictable
    /// bytes.
    PoolFree,
    /// The stuck cache bytes are at most anon.
    Stuck,
    /// B's used side is at most anon + file.
    BUsed,
    /// The depots' resident bytes are at most anon.
    DepotResident,
    /// The depots' resident bytes are at most their mapped length.
    DepotMapped,
    /// buf_grow's extents are at most anon (the pool is a private anonymous mapping).
    BufGrown,
}

pub const CHECKS: usize = 10;

impl Check {
    pub const ALL: [Check; CHECKS] = [Check::Landing, Check::LadderRead, Check::KillRead, Check::RoomLow, Check::PoolFree, Check::Stuck, Check::BUsed, Check::DepotResident, Check::DepotMapped, Check::BufGrown];

    /// The row's name: the consumer's terms, the comparison and the split checked against.
    pub const fn name(self) -> &'static str {
        match self {
            Check::Landing => "landing.sum <= current-inactive_file",
            Check::LadderRead => "ladder.read == max(current-inactive_file, governed)",
            Check::KillRead => "kill.governed == max(anon+shmem, current-clean_file)",
            Check::RoomLow => "room_low <= line-(current-inactive_file)",
            Check::PoolFree => "pool.capacity-pool.held <= limit-(current-inactive_file)+pool.evictable",
            Check::Stuck => "stuck <= anon",
            Check::BUsed => "b.used <= anon+file",
            Check::DepotResident => "depot.resident <= anon",
            Check::DepotMapped => "depot.resident <= depot.mapped",
            Check::BufGrown => {
                if Check::BufGrown.compares() {
                    "buf.grown <= anon"
                } else {
                    "buf.grown <= anon: skipped, kind mismatch (Mapped vs Resident)"
                }
            }
        }
    }

    /// Whether the check compares its term's bytes with the split's resident pages: not for buf.grown while it is extent
    /// lengths (E6, Mapped). Its resident pages are anon (the extents are MAP_PRIVATE | MAP_ANONYMOUS, bufmgr extent.rs),
    /// so the check comes back as buf.grown's resident bytes <= anon once a term carries them; until then its row keeps
    /// the value, breaks nothing and says it was skipped.
    pub const fn compares(self) -> bool {
        match self {
            Check::BufGrown => matches!(DEFS[Term::BufGrown as usize].kind, Kind::Resident | Kind::Account),
            _ => true,
        }
    }
}

/// A row of the check: rule 1 and rule 2 per term, rule 3 per check, rule 4 once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    WrongKind(Term),
    OverCurrent(Term),
    Split(Check),
    RetainedAsUsed,
}

pub const ROWS: usize = 2 * TERMS + CHECKS + 1;

impl Row {
    pub const fn of(i: usize) -> Row {
        if i < TERMS {
            Row::WrongKind(Term::ALL[i])
        } else if i < 2 * TERMS {
            Row::OverCurrent(Term::ALL[i - TERMS])
        } else if i < 2 * TERMS + CHECKS {
            Row::Split(Check::ALL[i - 2 * TERMS])
        } else {
            Row::RetainedAsUsed
        }
    }

    pub const fn index(self) -> usize {
        match self {
            Row::WrongKind(t) => t as usize,
            Row::OverCurrent(t) => TERMS + t as usize,
            Row::Split(c) => 2 * TERMS + c as usize,
            Row::RetainedAsUsed => 2 * TERMS + CHECKS,
        }
    }

    /// Whether the row can break at all (a term with no wrong-kind consumer has no rule-1 row).
    pub const fn applies(self) -> bool {
        match self {
            Row::WrongKind(t) => t.wrong_feeds() != 0,
            Row::OverCurrent(t) => t.resident_read(),
            Row::Split(_) | Row::RetainedAsUsed => true,
        }
    }

    pub const fn rule(self) -> u8 {
        match self {
            Row::WrongKind(_) => 1,
            Row::OverCurrent(_) => 2,
            Row::Split(_) => 3,
            Row::RetainedAsUsed => 4,
        }
    }
}

/// No value (a window before its first note, a bound that is not set): a bound at it binds nothing.
pub const NONE: u64 = u64::MAX;

/// The tolerance: max(64 MiB, 2 % of `limit`), plus `growth` (memory.current's move over the last sample).
pub const fn tolerance(limit: u64, growth: u64) -> u64 {
    let pct = limit / 50;
    let floor = 64 * MIB;
    (if pct > floor { pct } else { floor }).saturating_add(growth)
}

/// One sample's check: per row, the value it compared (`value`) and its excess past `tol` (0 = clean; `excess`).
pub struct Outcome {
    pub excess: [u64; ROWS],
    pub value: [u64; ROWS],
}

#[inline]
fn over(x: u64, cap: u64, tol: u64) -> u64 {
    let lim = cap.saturating_add(tol);
    if x > lim { x - lim } else { 0 }
}

#[inline]
fn apart(x: u64, y: u64, tol: u64) -> u64 {
    let d = x.abs_diff(y);
    if d > tol { d - tol } else { 0 }
}

/// The check over the terms `v` (`Term as usize`; `NONE` = not set) and the split `s`.
pub fn check(v: &[u64; TERMS], s: &Split, tol: u64) -> Outcome {
    let mut o = Outcome { excess: [0; ROWS], value: [0; ROWS] };
    let at = |t: Term| v[t as usize];
    let set = |t: Term| at(t) != NONE;
    let val = |t: Term| if set(t) { at(t) } else { 0 };
    let mut put = |r: Row, value: u64, excess: u64| {
        o.value[r.index()] = value;
        o.excess[r.index()] = excess;
    };
    let room = s.room();
    for t in Term::ALL {
        let d = t.def();
        if t.wrong_feeds() != 0 && set(t) {
            // a bound binds when it is under the room the split shows; an additive term counts past the tolerance
            let x = at(t);
            put(Row::WrongKind(t), x, if d.bound { over(room, x, tol) } else { over(x, 0, tol) });
        }
        if t.resident_read() && set(t) {
            put(Row::OverCurrent(t), at(t), over(at(t), s.current, tol));
        }
    }
    let landing = LANDING.iter().fold(0u64, |a, t| a.saturating_add(val(*t)));
    put(Row::Split(Check::Landing), landing, over(landing, s.ladder(), tol));
    if set(Term::LadderRead) {
        let want = if s.ladder() > s.governed() { s.ladder() } else { s.governed() };
        put(Row::Split(Check::LadderRead), at(Term::LadderRead), apart(at(Term::LadderRead), want, tol));
    }
    if set(Term::Governed) {
        put(Row::Split(Check::KillRead), at(Term::Governed), apart(at(Term::Governed), s.governed(), tol));
    }
    if set(Term::RoomLow) {
        put(Row::Split(Check::RoomLow), at(Term::RoomLow), over(at(Term::RoomLow), room, tol));
    }
    if set(Term::PoolCapacity) && set(Term::PoolHeld) {
        let free = at(Term::PoolCapacity).saturating_sub(at(Term::PoolHeld));
        put(Row::Split(Check::PoolFree), free, over(free, s.free().saturating_add(val(Term::PoolEvictable)), tol));
    }
    if set(Term::Stuck) {
        put(Row::Split(Check::Stuck), at(Term::Stuck), over(at(Term::Stuck), s.anon, tol));
    }
    if set(Term::BUsed) {
        put(Row::Split(Check::BUsed), at(Term::BUsed), over(at(Term::BUsed), s.anon.saturating_add(s.file), tol));
    }
    if set(Term::DepotResident) {
        put(Row::Split(Check::DepotResident), at(Term::DepotResident), over(at(Term::DepotResident), s.anon, tol));
        if set(Term::DepotMapped) {
            put(Row::Split(Check::DepotMapped), at(Term::DepotResident), over(at(Term::DepotResident), at(Term::DepotMapped), tol));
        }
    }
    if set(Term::BufGrown) {
        let excess = if Check::BufGrown.compares() { over(at(Term::BufGrown), s.anon, tol) } else { 0 };
        put(Row::Split(Check::BufGrown), at(Term::BufGrown), excess);
    }
    if set(Term::BRetained) {
        put(Row::RetainedAsUsed, at(Term::BRetained), over(at(Term::BRetained), 0, tol));
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    fn split() -> Split {
        Split { current: 12 * GIB, anon: 9 * GIB, shmem: 0, file: 3 * GIB, inactive: GIB, clean: 2 * GIB, line: 14 * GIB, limit: 16 * GIB }
    }

    fn none() -> [u64; TERMS] {
        [NONE; TERMS]
    }

    fn row(o: &Outcome, r: Row) -> (u64, u64) {
        (o.value[r.index()], o.excess[r.index()])
    }

    /// The table: exactly the terms the design names have a wrong-kind consumer, and the rows round-trip.
    #[test]
    fn the_table_flags_the_mapped_asked_and_reserved_terms_a_resident_consumer_reads() {
        let flagged: Vec<&str> = Term::ALL.iter().filter(|t| t.wrong_feeds() != 0).map(|t| t.name()).collect();
        assert_eq!(flagged, ["landing.own", "landing.giveback", "landing.pool", "landing.parked_jobs", "landing.buffers", "maps_since", "claim_low", "b.free", "b.used", "pool.cap.unattributed", "pool.cap.shared_buffers"]);
        assert_eq!(Term::MapsSince.wrong_feeds(), Consumer::RaiseWindow.bit());
        assert_eq!(Term::LandingPool.wrong_feeds(), Consumer::Landing.bit() | Consumer::RaiseWindow.bit());
        assert!(!Term::ClaimLow.resident_read(), "a bound is never rule 2's");
        assert!(!Term::Room.resident_read() && !Term::RoomLow.resident_read() && !Term::PoolRoom.resident_read(), "a headroom is never rule 2's");
        assert!(Term::LandingReaper.resident_read() && !Term::PoolHeld.resident_read());
        for (i, t) in Term::ALL.iter().enumerate() {
            assert_eq!(*t as usize, i, "Term::ALL in order");
        }
        for i in 0..ROWS {
            assert_eq!(Row::of(i).index(), i);
        }
        assert_eq!(tolerance(16 * GIB, 0), 16 * GIB / 50);
        assert_eq!(tolerance(GIB, 5 * MIB), 69 * MIB, "64 MiB at least, plus the sample's growth");
    }

    /// Rule 1: an additive wrong-kind term counts past the tolerance; a bound only when it binds under the split's room.
    #[test]
    fn rule_1_counts_a_wrong_kind_term_past_the_tolerance_and_a_bound_that_binds() {
        let s = split();
        let tol = tolerance(s.limit, 0);
        let mut v = none();
        v[Term::LandingPool as usize] = 2 * GIB;
        v[Term::MapsSince as usize] = tol / 2;
        v[Term::ClaimLow as usize] = GIB / 2;
        v[Term::BFree as usize] = 5 * GIB;
        let o = check(&v, &s, tol);
        assert_eq!(row(&o, Row::WrongKind(Term::LandingPool)), (2 * GIB, 2 * GIB - tol), "an ask in the landing");
        assert_eq!(row(&o, Row::WrongKind(Term::MapsSince)), (tol / 2, 0), "under the tolerance");
        assert_eq!(s.room(), 3 * GIB);
        assert_eq!(row(&o, Row::WrongKind(Term::ClaimLow)), (GIB / 2, 3 * GIB - GIB / 2 - tol), "a reservation bound the raise under the room");
        assert_eq!(row(&o, Row::WrongKind(Term::BFree)).1, 0, "a bound over the room binds nothing");
        assert_eq!(row(&o, Row::WrongKind(Term::LandingOwn)), (0, 0), "a term not set");
    }

    /// Rule 2 and rule 3's landing: D4907's 17.6 GiB landing in a 15.7 GiB cgroup breaks both.
    #[test]
    fn rule_2_and_the_landing_check_catch_a_landing_past_memory_current() {
        let s = Split { current: 4856 * MIB + 2 * GIB, inactive: 2 * GIB, ..split() };
        let tol = tolerance(s.limit, 0);
        let mut v = none();
        v[Term::LandingReaper as usize] = 17_566 * MIB;
        let o = check(&v, &s, tol);
        assert_eq!(row(&o, Row::OverCurrent(Term::LandingReaper)).1, 17_566 * MIB - s.current - tol);
        assert_eq!(row(&o, Row::Split(Check::Landing)), (17_566 * MIB, 17_566 * MIB - 4856 * MIB - tol), "past the ladder's read");
        v[Term::LandingReaper as usize] = GIB;
        assert_eq!(row(&check(&v, &s, tol), Row::Split(Check::Landing)).1, 0);
        // s1rerun K R0's r2.room / r2.room_low: a headroom past memory.current under a small read is no fault
        v[Term::Room as usize] = 13_896 * MIB;
        v[Term::RoomLow as usize] = 13_896 * MIB;
        let o = check(&v, &s, tol);
        assert_eq!((row(&o, Row::OverCurrent(Term::Room)).1, row(&o, Row::OverCurrent(Term::RoomLow)).1), (0, 0), "a headroom is never rule 2's");
    }

    /// Rule 3: each check against its split, rule 4 on the retained bytes.
    #[test]
    fn rule_3_checks_each_consumer_against_its_split_and_rule_4_the_retained_bytes() {
        let s = split();
        let tol = tolerance(s.limit, 0);
        assert_eq!((s.ladder(), s.governed(), s.free()), (11 * GIB, 10 * GIB, 5 * GIB));
        let mut v = none();
        v[Term::LadderRead as usize] = 11 * GIB;
        v[Term::Governed as usize] = 9 * GIB;
        v[Term::RoomLow as usize] = 4 * GIB;
        v[Term::PoolCapacity as usize] = 9 * GIB;
        v[Term::PoolHeld as usize] = 2 * GIB;
        v[Term::PoolEvictable as usize] = GIB;
        v[Term::Stuck as usize] = 10 * GIB;
        v[Term::BUsed as usize] = 11 * GIB;
        v[Term::DepotMapped as usize] = GIB;
        v[Term::DepotResident as usize] = 2 * GIB;
        // buf.grown's extents (Mapped) past anon: memfuzz seed 2's 12.5 GB grown beside ~1 GB anon
        v[Term::BufGrown as usize] = 64 * GIB;
        v[Term::BRetained as usize] = GIB;
        let o = check(&v, &s, tol);
        let ex = |c: Check| row(&o, Row::Split(c)).1;
        assert_eq!(ex(Check::LadderRead), 0);
        assert_eq!(ex(Check::KillRead), GIB - tol, "a stale kill read a GiB under the split's");
        assert_eq!(ex(Check::RoomLow), GIB - tol, "a window low over the room now");
        assert_eq!(ex(Check::PoolFree), GIB - tol, "the pool's free past the cgroup's room and the evictable bytes");
        assert_eq!(ex(Check::Stuck), GIB - tol);
        assert_eq!(ex(Check::BUsed), 0, "under anon + file");
        assert_eq!(ex(Check::DepotResident), 0);
        assert_eq!(ex(Check::DepotMapped), GIB - tol, "more resident than mapped");
        assert_eq!(row(&o, Row::Split(Check::BufGrown)), (64 * GIB, 0), "Mapped extents past anon: skipped, the value kept");
        assert!(!Check::BufGrown.compares() && Check::BufGrown.name().contains("skipped"), "the row says it was skipped");
        assert_eq!(row(&o, Row::RetainedAsUsed), (GIB, GIB - tol));
        assert!(Row::RetainedAsUsed.applies() && !Row::WrongKind(Term::Room).applies() && !Row::OverCurrent(Term::DepotMapped).applies());
    }
}
