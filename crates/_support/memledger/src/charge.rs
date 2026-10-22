//! C8 "owned, measured charges" (the user-approved step-2 spec; design lanes/memlead/C8-DESIGN.md): the one typed charge.
//! A charge says WHO inside an account owns its bytes (`Owner`), WHEN they must be gone (`Release`), and how much of its
//! claim is open pages (`committed`, on the owner's account) against promised room (`reserved`, never resident). Reserving
//! books `reserved` only; committing moves bytes to `committed` as pages open; dropping discharges both; an owner change
//! is `Charge::transfer`, the one place bytes move between owners. Each owner's numbers sit in a static cell (no
//! allocation, a computed index), so the owner-end checks and the per-owner rank read them in O(1).

use core::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering::{self, AcqRel, Acquire, Relaxed}};

use crate::{current, pool_back, pool_charge, post, post_q, switch, Acct, Consumer, Entered, Kind, ACCOUNT_SLOTS, CONSUMERS, GIVEN, SLOTS};

/// A cache-class owner: a consumer's own account, the depots' parked memory, the reaper's pending unmaps, or the memory
/// of threads that never took an account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Holder {
    Of(Consumer),
    Parked,
    Reaping,
    Unbound,
    /// RESIDUE: the process-wide slot depot's parked worker-slot sets, their sources' kept heap
    /// (`gravity_driver::Source::transfer_held` at the park; the take moves it to the taking statement), on its own
    /// account (`Acct::SLOT_DEPOT`).
    SlotDepot,
}

impl Holder {
    /// The holder whose account is `a` (a non-backend account).
    pub const fn of_acct(a: Acct) -> Holder {
        match a.0 {
            i if i == Acct::PARKED.0 => Holder::Parked,
            i if i == Acct::REAPING.0 => Holder::Reaping,
            i if i == Acct::SLOT_DEPOT.0 => Holder::SlotDepot,
            i if i >= 1 && ((i - 1) as usize) < CONSUMERS => Holder::Of(Consumer::ALL[(i - 1) as usize]),
            _ => Holder::Unbound,
        }
    }

    pub const fn acct(self) -> Acct {
        match self {
            Holder::Of(c) => Acct::of(c),
            Holder::Parked => Acct::PARKED,
            Holder::Reaping => Acct::REAPING,
            Holder::Unbound => Acct::UNBOUND,
            Holder::SlotDepot => Acct::SLOT_DEPOT,
        }
    }

    const fn index(self) -> usize {
        match self {
            Holder::Of(c) => c as usize,
            Holder::Parked => CONSUMERS,
            Holder::Reaping => CONSUMERS + 1,
            Holder::Unbound => CONSUMERS + 2,
            Holder::SlotDepot => CONSUMERS + 3,
        }
    }
}

/// Who a charge's bytes belong to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    /// A backend's statement: its account and its process-wide id (`running_statement`; never 0, never reused).
    Statement { acct: Acct, stmt: u64 },
    /// A backend's portal: its account and the portal's token (the runner's exec slot, `portal_begin`).
    Portal { acct: Acct, portal: u64 },
    /// A worker set, by its id (`worker_set_begin`); its bytes sit on the account it was begun on.
    WorkerSet(u32),
    Cache(Holder),
    /// A backend's estate between statements (its memo's parked Nodes, their kept cursors).
    Session(Acct),
    /// LEDGER2: an operator's (or a worker arena's) bytes inside its parent (`op_begin`), on the parent's account.
    Op { acct: Acct, id: u64 },
}

impl Owner {
    /// The account the owner's committed bytes sit on.
    pub fn acct(self) -> Acct {
        match self {
            Owner::Statement { acct, .. } | Owner::Portal { acct, .. } | Owner::Session(acct) | Owner::Op { acct, .. } => acct,
            Owner::WorkerSet(id) => match find(SET_AT, SET_CELLS, id as u64 + 1, None) {
                Some(i) => Acct(CELLS[i].acct.load(Relaxed)),
                None => Acct::PARKED,
            },
            Owner::Cache(h) => h.acct(),
        }
    }

    /// The owner's generation within its account: a statement's or a portal's id (`running_statement`, `active_portal`:
    /// one id space, `OWNER_IDS`), a worker set's id tagged `WORKER_SET_KEY` (never equal to a statement's or a portal's);
    /// 0 for a session and a cache (whose uses the per-owner rank never counts).
    pub const fn key(self) -> u64 {
        match self {
            Owner::Statement { stmt, .. } => stmt,
            Owner::Portal { portal, .. } => portal,
            Owner::Op { id, .. } => id,
            Owner::WorkerSet(id) => WORKER_SET_KEY | id as u64,
            Owner::Session(_) | Owner::Cache(_) => 0,
        }
    }

    pub const fn class(self) -> OwnerClass {
        match self {
            Owner::Statement { .. } => OwnerClass::Statement,
            Owner::Portal { .. } => OwnerClass::Portal,
            Owner::WorkerSet(_) => OwnerClass::WorkerSet,
            Owner::Cache(_) => OwnerClass::Cache,
            Owner::Session(_) => OwnerClass::Session,
            Owner::Op { .. } => OwnerClass::Op,
        }
    }

    /// LEDGER2 step 3: the owner as two words for a C-ABI call (an operator arena's chunk ask, `SPILL_ARENA_CHUNK`): its
    /// class and account, then its key (a worker set's bare id); `from_words` reads them back.
    pub fn words(self) -> [u64; 2] {
        let a = match self {
            Owner::WorkerSet(_) => 0,
            o => o.acct().0,
        };
        let k = match self {
            Owner::WorkerSet(id) => id as u64,
            o => o.key(),
        };
        [self.class() as u64 | (a as u64) << 32, k]
    }

    /// `words` read back; None for words no owner writes.
    pub fn from_words(w: [u64; 2]) -> Option<Owner> {
        let (class, acct, key) = (w[0] & 0xff, Acct((w[0] >> 32) as u32), w[1]);
        Some(match class {
            0 => Owner::Statement { acct, stmt: key },
            1 => Owner::Portal { acct, portal: key },
            2 => Owner::WorkerSet(u32::try_from(key).ok()?),
            3 => Owner::Cache(Holder::of_acct(acct)),
            4 => Owner::Session(acct),
            5 => Owner::Op { acct, id: key },
            _ => return None,
        })
    }
}

/// `Owner::key`'s tag on a worker set's id: `OWNER_IDS` counts up from 1 and never reaches it.
pub const WORKER_SET_KEY: u64 = 1 << 63;

/// C8 (D4087): the owner keys a cancel of account `a`'s running statement frees, (its statement id, the portal it executes),
/// 0 for none: what the per-owner rank counts on `a` (`mempolicy::watchdog::cancel_frees`).
pub fn running_owners(a: Acct) -> (u64, u64) {
    (crate::running_statement(a), active_portal(a))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum OwnerClass {
    Statement = 0,
    Portal = 1,
    WorkerSet = 2,
    Cache = 3,
    Session = 4,
    Op = 5,
}

pub const OWNER_CLASSES: usize = 6;

impl OwnerClass {
    pub const ALL: [OwnerClass; OWNER_CLASSES] = [OwnerClass::Statement, OwnerClass::Portal, OwnerClass::WorkerSet, OwnerClass::Cache, OwnerClass::Session, OwnerClass::Op];

    pub const fn name(self) -> &'static str {
        match self {
            OwnerClass::Statement => "statement",
            OwnerClass::Portal => "portal",
            OwnerClass::WorkerSet => "worker_set",
            OwnerClass::Cache => "cache",
            OwnerClass::Session => "session",
            OwnerClass::Op => "operator",
        }
    }
}

/// When a charge's bytes must be gone: past each row its scan has passed, at its statement's end, its portal's, its
/// session's, or when its cache evicts it (`Evict`: no owner end checks a cache's charge).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Release {
    Row = 0,
    Statement = 1,
    Portal = 2,
    Session = 3,
    Evict = 4,
}

/// The release an owner's charges carry by default: a portal's at its drop, a session's at its end, a holder's at its
/// eviction, everything else (statement, worker set, operator) at the statement's end. CONVERSION WAVE 2: one mapping for
/// `ChargedVec`, a cache scope that names a non-holder owner (`arena::cache_scope_for`) and `OpArena`'s constructors.
pub fn release_of(o: Owner) -> Release {
    match o {
        Owner::Portal { .. } => Release::Portal,
        Owner::Session(_) => Release::Session,
        Owner::Cache(_) => Release::Evict,
        Owner::Statement { .. } | Owner::WorkerSet(_) | Owner::Op { .. } => Release::Statement,
    }
}

/// Which door counted the committed bytes: the global allocator (1: a heap block, counted where it was allocated), the
/// charge's own calls (2: a mapping's writable pages), or the charge's own calls for bytes the page pool served (a region's
/// or an arena's under `PGRUST_POOL_GRAVITY`: on its owner at the commit with no move off the calling thread, since no
/// door counted them there, and among the account's pool bytes, `AccountRow::pagepool_bytes`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Door {
    Heap = 0,
    Mapped = 1,
    Pool = 2,
}

/// One owner's numbers: the sum of its live charges.
pub(crate) struct OwnerCell {
    /// The owner's generation (a statement's or a session's id, a portal's token, a worker set's id + 1); 0 = free. A
    /// charge keeps the key it was made under: a different one means its owner ended under it.
    pub(crate) key: AtomicU64,
    /// The account (portal and worker-set cells: the one they were begun on).
    pub(crate) acct: AtomicU32,
    pub(crate) committed: AtomicI64,
    pub(crate) reserved: AtomicI64,
    pub(crate) uses: AtomicI64,
    /// LEDGER2: of `committed`, door 2's bytes; of `reserved`, the room its draws back (`Charge::back`).
    pub(crate) mapped: AtomicI64,
    pub(crate) backed: AtomicI64,
    /// LEDGER2: memory-context blocks charged to this owner, by a named root or inferred from the creating thread.
    pub(crate) ctx_named: AtomicI64,
    pub(crate) ctx_inferred: AtomicI64,
    /// LEDGER2: an operator's budget (its share), and the bytes its spills hold on disk.
    pub(crate) budget: AtomicI64,
    pub(crate) spilled: AtomicI64,
    /// LEDGER2 (design A): of `committed`, the bytes the residency pass found not resident: the listed ranges whose charge
    /// books here (`NrHome`), their charged bytes less their resident pages at the last read.
    pub(crate) nr: AtomicI64,
    /// PINS: the bytes of cache entries this owner's scans hold a reference to (`PinGuard`), of those the cache has already
    /// evicted (still resident until the last holder lets go), and the owner's share of those (each evicted entry's bytes
    /// divided by its holders). Counts beside `committed`, not charges: the bytes stay on the cache's account.
    pub(crate) pins: AtomicI64,
    pub(crate) pin_ev: AtomicI64,
    pub(crate) pin_sh: AtomicI64,
    /// LEDGER2: an operator cell's parent cell + 1 (0: none, so the table stays zero-initialized) and the key the parent
    /// held at the begin.
    pub(crate) parent: AtomicU32,
    pub(crate) pkey: AtomicU64,
    /// LEDGER2: an operator cell's kind (`Kind as u32`), `WORKER_ARENA` set for a worker arena.
    pub(crate) label: AtomicU32,
    /// LEDGER2: an operator cell's children keyed now (it frees only after them: their chain runs through it).
    pub(crate) kids: AtomicU32,
    /// A statement's or a portal's (any non-operator) cell: the operator cells keyed now whose chain tops here under its
    /// current key, so its end sweeps the operator table (`end_ops`, two passes over `OP_CELLS`) only when one is (a
    /// nested SPI portal of a statement that runs operators ended in two full passes per call: memlead#3276's D3865 e1).
    pub(crate) ops: AtomicU32,
    /// An operator cell: its top cell + 1 (0: not counted anywhere) and that cell's key at the begin (`ops` counted there).
    pub(crate) top: AtomicU32,
    pub(crate) tkey: AtomicU64,
}

pub(crate) const NO_PARENT: u32 = u32::MAX;
pub const WORKER_ARENA: u32 = 1 << 8;
/// An operator cell whose `op_end` ran: it stays keyed (in its parent's subtree) until its last byte leaves.
const ENDED: u32 = 1 << 31;
/// An operator cell a sweep (`end_ops`) takes: marked first, so a child is matched through its parent before either frees.
const SWEPT: u32 = 1 << 30;
/// The key an operator cell holds while it frees: no charge matches it and no begin claims it.
const OP_FREEING: u64 = u64::MAX;
/// The most parent links a chain walk follows (statement → operator → worker arena is 2).
const DEPTH: usize = 8;

#[allow(clippy::declare_interior_mutable_const)]
const CELL0: OwnerCell = OwnerCell {
    key: AtomicU64::new(0),
    acct: AtomicU32::new(0),
    committed: AtomicI64::new(0),
    reserved: AtomicI64::new(0),
    uses: AtomicI64::new(0),
    mapped: AtomicI64::new(0),
    backed: AtomicI64::new(0),
    ctx_named: AtomicI64::new(0),
    ctx_inferred: AtomicI64::new(0),
    budget: AtomicI64::new(0),
    spilled: AtomicI64::new(0),
    nr: AtomicI64::new(0),
    pins: AtomicI64::new(0),
    pin_ev: AtomicI64::new(0),
    pin_sh: AtomicI64::new(0),
    parent: AtomicU32::new(0),
    pkey: AtomicU64::new(0),
    label: AtomicU32::new(0),
    kids: AtomicU32::new(0),
    ops: AtomicU32::new(0),
    top: AtomicU32::new(0),
    tkey: AtomicU64::new(0),
};

const SLOTS_N: usize = ACCOUNT_SLOTS;
/// Per backend slot: its running statement's cell, its session's, and its portals' when the portal table is full.
const STMT_AT: usize = 0;
const SESSION_AT: usize = SLOTS_N;
const PORTAL_SPILL_AT: usize = 2 * SLOTS_N;
/// Begun portals (`portal_begin`), probed by token.
pub const PORTAL_CELLS: usize = 4096;
const PORTAL_AT: usize = 3 * SLOTS_N;
/// Begun worker sets (`worker_set_begin`), then one cell for the sets past the table.
pub const SET_CELLS: usize = 1024;
const SET_AT: usize = PORTAL_AT + PORTAL_CELLS;
const SET_SPILL: usize = SET_AT + SET_CELLS;
const HOLDER_AT: usize = SET_SPILL + 1;
/// The numbers of charges whose owner AND session ended while they lived (a residue no owner end could hand on).
const ORPHAN: usize = HOLDER_AT + CONSUMERS + 4;
/// C8 checker 1 (B2): the cells of sessions that unbound (ProcKill's `unbind_backend`) before their teardown ended
/// (`session_drained`), probed by session generation: their estate's late releases find their numbers here (`redirect`).
pub const EXIT_CELLS: usize = 256;
const EXIT_AT: usize = ORPHAN + 1;
/// LEDGER2: begun operator and worker-arena owners (`op_begin`), probed by id; past the table an operator charges its parent.
pub const OP_CELLS: usize = 16384;
pub(crate) const OP_AT: usize = EXIT_AT + EXIT_CELLS;
pub(crate) const NCELLS: usize = OP_AT + OP_CELLS;
/// Probes before a begin spills (a full table: the owner's numbers go to its spill cell, its own end check is skipped).
const PROBES: usize = 16;

pub(crate) static CELLS: [OwnerCell; NCELLS] = [CELL0; NCELLS];

/// The next owner generation (statements, sessions): never 0, never reused, so a backend that takes over a slot never
/// matches its predecessor's.
pub(crate) static OWNER_IDS: AtomicU64 = AtomicU64::new(1);

/// Charges that outlived their owner and moved to its session's (or the orphan) cell; begins that found their table
/// full. The ends that found a residue are `owner_end`'s.
static STALE: AtomicU64 = AtomicU64::new(0);
static OVERFLOW: AtomicU64 = AtomicU64::new(0);
/// LEDGER2: operator cells keyed now, operator begins past a full table (they charge their parent), operator ends.
static OPS_KEYED: AtomicI64 = AtomicI64::new(0);
static OP_OVERFLOW: AtomicU64 = AtomicU64::new(0);
static OP_ENDS: AtomicU64 = AtomicU64::new(0);
/// LEDGER2: per backend slot, its operator cells keyed now: an end sweeps the table only when some are.
static OPS_ON: [AtomicU32; SLOTS_N] = [const { AtomicU32::new(0) }; SLOTS_N];

std::thread_local! {
    /// B2: the session this thread unbound and has not drained (`session_drained`): its generation, its account and its
    /// pid (the slot's may be the next session's by then); 0 for none. No destructor: the exit fold's pthread-key
    /// destructor reads it after every thread-local's.
    static EXITING: core::cell::Cell<(u64, u32, u32)> = const { core::cell::Cell::new((0, 0, 0)) };
}

#[inline]
fn slot_ix(a: Acct) -> Option<usize> {
    (a.is_backend() && (a.0 as usize) < SLOTS_N).then_some(a.0 as usize)
}

#[inline]
fn mix(key: u64, acct: u32) -> usize {
    (key ^ ((acct as u64) << 40)).wrapping_mul(0x9E37_79B9_7F4A_7C15) as usize
}

/// The table cell holding `key` (and `acct` when given) among `n` from `at`.
fn find(at: usize, n: usize, key: u64, acct: Option<u32>) -> Option<usize> {
    let h = mix(key, acct.unwrap_or(0));
    (0..PROBES).map(|i| at + (h.wrapping_add(i) % n)).find(|&i| {
        CELLS[i].key.load(Relaxed) == key && acct.is_none_or(|a| CELLS[i].acct.load(Relaxed) == a)
    })
}

/// A free cell for `key` among `n` from `at` (claimed with the account first, then the key), or None when the probes
/// find none.
fn claim(at: usize, n: usize, key: u64, acct: u32) -> Option<usize> {
    let h = mix(key, acct);
    for i in 0..PROBES {
        let c = &CELLS[at + (h.wrapping_add(i) % n)];
        if c.key.load(Relaxed) == 0 && c.key.compare_exchange(0, key, Relaxed, Relaxed).is_ok() {
            c.acct.store(acct, Relaxed);
            return Some(at + (h.wrapping_add(i) % n));
        }
    }
    OVERFLOW.fetch_add(1, Relaxed);
    None
}

/// The session generation of backend account `a` (0: none bound).
#[inline]
fn session_key(a: Acct) -> u64 {
    slot_ix(a).map_or(0, |i| CELLS[SESSION_AT + i].key.load(Relaxed))
}

/// Owner `o`'s cell, the key its charges carry and the session key behind it, as the tables hold them now (a keyed
/// owner's cell may hold another key: the owner ended).
fn locate(o: Owner) -> (usize, u64, u64) {
    let holder = |a: Acct| (HOLDER_AT + Holder::of_acct(a).index(), 0, 0);
    match o {
        Owner::Statement { acct, stmt } => match slot_ix(acct) {
            Some(i) => (STMT_AT + i, stmt, session_key(acct)),
            None => holder(acct),
        },
        Owner::Portal { acct, portal } => match slot_ix(acct) {
            Some(i) => match find(PORTAL_AT, PORTAL_CELLS, portal, Some(acct.0)) {
                Some(c) => (c, portal, session_key(acct)),
                None => (PORTAL_SPILL_AT + i, session_key(acct), 0),
            },
            None => holder(acct),
        },
        Owner::Session(acct) => match slot_ix(acct) {
            Some(i) => (SESSION_AT + i, session_key(acct), 0),
            None => holder(acct),
        },
        Owner::WorkerSet(id) => match find(SET_AT, SET_CELLS, id as u64 + 1, None) {
            Some(c) => (c, id as u64 + 1, 0),
            None => (SET_SPILL, 0, 0),
        },
        Owner::Cache(h) => (HOLDER_AT + h.index(), 0, 0),
        Owner::Op { acct, id } => match find(OP_AT, OP_CELLS, id, Some(acct.0)) {
            Some(c) => (c, id, session_key(acct)),
            None => (ORPHAN, id, session_key(acct)),
        },
    }
}

/// Whether owner `o` keyed by `key` in cell `cell` is live: a cache's and the spill cells always; a backend's owner while
/// its cell holds its key (a session bound, a statement running, a portal begun).
#[inline]
fn live(o: Owner, cell: usize, key: u64) -> bool {
    let keyed = matches!(o, Owner::Statement { .. } | Owner::Portal { .. } | Owner::Session(_)) && slot_ix(o.acct()).is_some();
    (!keyed && key == 0) || (key != 0 && CELLS[cell].key.load(Relaxed) == key)
}

/// The cell a new (or transferred) charge of owner `o` books on: its own while it lives, else its session's, else the
/// orphan's (counted as stale).
fn cell_of(o: Owner) -> (u32, u64, u64) {
    let (cell, key, skey) = locate(o);
    if live(o, cell, key) {
        return (cell as u32, key, skey);
    }
    STALE.fetch_add(1, Relaxed);
    redirect(o.acct(), skey)
}

/// Where a charge whose owner ended goes: its session's cell while that session lives, its exit cell while the session
/// unbound and has not drained (B2), else the orphan cell.
fn redirect(a: Acct, skey: u64) -> (u32, u64, u64) {
    match slot_ix(a) {
        Some(i) if skey != 0 && CELLS[SESSION_AT + i].key.load(Relaxed) == skey => ((SESSION_AT + i) as u32, skey, 0),
        Some(_) if skey != 0 => match find(EXIT_AT, EXIT_CELLS, skey, Some(a.0)) {
            Some(x) => (x as u32, skey, 0),
            None => (ORPHAN as u32, 0, 0),
        },
        _ => (ORPHAN as u32, 0, 0),
    }
}

/// Cell `c` holds the numbers of a session that unbound (an exit cell, or the orphan cell): the unbind moved its account's
/// bytes to UNBOUND, so a charge booked there books on UNBOUND too.
#[inline]
fn past_session(c: u32) -> bool {
    let c = c as usize;
    c == ORPHAN || (EXIT_AT..EXIT_AT + EXIT_CELLS).contains(&c)
}

/// What an owner's end found still owned: committed and reserved bytes, uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Residue {
    pub class: OwnerClass,
    pub acct: Acct,
    pub committed: i64,
    pub reserved: i64,
    pub uses: i64,
}

/// One owner's numbers now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OwnerRow {
    pub committed: i64,
    pub reserved: i64,
    pub uses: i64,
}

fn row_of(c: &OwnerCell) -> OwnerRow {
    OwnerRow { committed: c.committed.load(Relaxed), reserved: c.reserved.load(Relaxed), uses: c.uses.load(Relaxed) }
}

/// Owner `o`'s numbers now (zeros for an owner that ended).
pub fn owner_row(o: Owner) -> OwnerRow {
    let (cell, key, _) = locate(o);
    if live(o, cell, key) { row_of(&CELLS[cell]) } else { OwnerRow::default() }
}

/// Ends owner cell `i`: its numbers move to cell `to` (a statement's or a portal's residue to its session, a session's
/// or a worker set's to the orphan cell), the key frees; the residue, when any, is noted (`owner_end`) and returned.
fn end_cell(i: usize, to: usize, class: OwnerClass, acct: Acct) -> Option<Residue> {
    end_cell_as(i, to, class, acct, crate::owner_end::Source::Owner)
}

/// `end_cell` with its residue noted under `src` (C8 move 6: a dead session's, 'crashed').
fn end_cell_as(i: usize, to: usize, class: OwnerClass, acct: Acct, src: crate::owner_end::Source) -> Option<Residue> {
    end_cell_noted(i, to, class, acct, src, None)
}

/// `end_cell_as` with the residue's pid given (B2: a drained session's, whose slot may hold the next session by then);
/// None: the slot's now.
fn end_cell_noted(i: usize, to: usize, class: OwnerClass, acct: Acct, src: crate::owner_end::Source, pid: Option<u32>) -> Option<Residue> {
    // a table cell's account clears before this (its claimer writes the account after the key: a cell freed here is never
    // a new owner's with its account cleared)
    let c = &CELLS[i];
    // the key first: a charge that moves after this point finds its owner gone and goes to `to` itself
    c.key.store(0, Relaxed);
    let r = move_cell(c, &CELLS[to]);
    if r == OwnerRow::default() {
        return None;
    }
    let r = Residue { class, acct, committed: r.committed, reserved: r.reserved, uses: r.uses };
    crate::owner_end::note_as(src, r, pid);
    Some(r)
}

/// Every number of cell `c` moved onto cell `t`; `c`'s committed, reserved and uses (a residue's numbers) returned.
fn move_cell(c: &OwnerCell, t: &OwnerCell) -> OwnerRow {
    for (f, g) in [(&c.mapped, &t.mapped), (&c.backed, &t.backed), (&c.ctx_named, &t.ctx_named), (&c.ctx_inferred, &t.ctx_inferred), (&c.spilled, &t.spilled), (&c.nr, &t.nr), (&c.pins, &t.pins), (&c.pin_ev, &t.pin_ev), (&c.pin_sh, &t.pin_sh)] {
        let n = f.swap(0, Relaxed);
        if n != 0 {
            g.fetch_add(n, Relaxed);
        }
    }
    let r = OwnerRow { committed: c.committed.swap(0, Relaxed), reserved: c.reserved.swap(0, Relaxed), uses: c.uses.swap(0, Relaxed) };
    t.committed.fetch_add(r.committed, Relaxed);
    t.reserved.fetch_add(r.reserved, Relaxed);
    t.uses.fetch_add(r.uses, Relaxed);
    r
}

/// A statement starts on backend account `a` with id `stmt`: its cell takes the id (a stray residue goes to the session).
pub(crate) fn begin_statement(a: Acct, stmt: u64) {
    if let Some(i) = slot_ix(a) {
        let old = CELLS[STMT_AT + i].key.load(Relaxed);
        end_ops(a, Some((STMT_AT + i, old)), SESSION_AT + i, crate::owner_end::Source::Owner);
        end_cell(STMT_AT + i, SESSION_AT + i, OwnerClass::Statement, a);
        CELLS[STMT_AT + i].key.store(stmt, Relaxed);
    }
}

/// The statement on backend account `a` ends: what it and its operators still own moves to its session (the owner-end
/// residues).
pub(crate) fn end_statement(a: Acct) -> Option<Residue> {
    let i = slot_ix(a)?;
    let k = CELLS[STMT_AT + i].key.load(Relaxed);
    end_ops(a, Some((STMT_AT + i, k)), SESSION_AT + i, crate::owner_end::Source::Owner);
    end_cell(STMT_AT + i, SESSION_AT + i, OwnerClass::Statement, a)
}

/// A session binds backend account `a`: its session (and portal spill) cells take a new generation. A slot whose last
/// session ended with no unbind (a kill-class exit skips ProcKill) still holds that session's cells: they end first, as its
/// residues, so the new session inherits none of them.
pub(crate) fn begin_session(a: Acct) {
    if let Some(i) = slot_ix(a) {
        if CELLS[SESSION_AT + i].key.load(Relaxed) != 0 {
            end_session(a);
        }
        let k = OWNER_IDS.fetch_add(1, Relaxed);
        CELLS[SESSION_AT + i].key.store(k, Relaxed);
        CELLS[PORTAL_SPILL_AT + i].key.store(k, Relaxed);
    }
}

/// The session on backend account `a` ends: what its statement, its portals no PortalDrop reached (the runner's
/// `forget_live`), its spilled portals and itself still own moves to the orphan cell; the session's residue is returned
/// (a statement's or a portal's is noted under its class).
pub(crate) fn end_session(a: Acct) -> Option<Residue> {
    end_session_as(a, crate::owner_end::Source::Owner).0
}

/// `end_session` with its residues noted under `src` (C8 move 6: a dead session's, 'crashed'): the session's residue and
/// how many residues were noted.
pub(crate) fn end_session_as(a: Acct, src: crate::owner_end::Source) -> (Option<Residue>, u32) {
    let Some(i) = slot_ix(a) else { return (None, 0) };
    let mut n = u32::from(end_cell_as(STMT_AT + i, ORPHAN, OwnerClass::Statement, a, src).is_some());
    for c in PORTAL_AT..PORTAL_AT + PORTAL_CELLS {
        if CELLS[c].key.load(Relaxed) != 0 && CELLS[c].acct.load(Relaxed) == a.0 {
            CELLS[c].acct.store(0, Relaxed);
            n += u32::from(end_cell_as(c, ORPHAN, OwnerClass::Portal, a, src).is_some());
        }
    }
    SLOTS[i].portals.store(0, Relaxed);
    n += u32::from(end_cell_as(PORTAL_SPILL_AT + i, ORPHAN, OwnerClass::Portal, a, src).is_some());
    n += end_ops(a, None, ORPHAN, src);
    let r = end_cell_as(SESSION_AT + i, ORPHAN, OwnerClass::Session, a, src);
    (r, n + u32::from(r.is_some()))
}

/// C8 checker 1 (B2): the session on backend account `a` unbinds (ProcKill's `unbind_backend`) before its teardown has
/// dropped its estate (launch_backend's run_session_teardown runs after ProcKill: the memo's parked Nodes and their kept
/// cursors, its kept arena), so the unbind is not the session's end. Its statement's and portals' cells end as at
/// `end_session` (their residues noted now), into an exit cell under the session's generation instead of the orphan cell;
/// its own cell's numbers move there with no residue noted: the estate's late releases find them (`redirect`), and
/// `session_drained`, after the teardown, notes what is still there as the session's residue. No session, or no exit cell
/// free: `end_session` (the residue noted now, as before).
pub(crate) fn exit_session(a: Acct) {
    // a session this thread unbound before and never drained (a pooled thread bound again): its end first
    session_drained();
    let Some(i) = slot_ix(a) else { return };
    let skey = CELLS[SESSION_AT + i].key.load(Relaxed);
    // claimed before any cell ends: a release racing the ends below finds the exit cell, not the orphan's
    let Some(x) = (skey != 0).then(|| claim(EXIT_AT, EXIT_CELLS, skey, a.0)).flatten() else {
        end_session(a);
        return;
    };
    let src = crate::owner_end::Source::Owner;
    end_cell_as(STMT_AT + i, x, OwnerClass::Statement, a, src);
    for c in PORTAL_AT..PORTAL_AT + PORTAL_CELLS {
        if CELLS[c].key.load(Relaxed) != 0 && CELLS[c].acct.load(Relaxed) == a.0 {
            CELLS[c].acct.store(0, Relaxed);
            end_cell_as(c, x, OwnerClass::Portal, a, src);
        }
    }
    SLOTS[i].portals.store(0, Relaxed);
    end_cell_as(PORTAL_SPILL_AT + i, x, OwnerClass::Portal, a, src);
    // the session's own cell: its key first (a release after this point finds the exit cell), then its numbers, unnoted
    let s = &CELLS[SESSION_AT + i];
    s.key.store(0, Relaxed);
    move_cell(s, &CELLS[x]);
    end_ops(a, None, x, src);
    let pid = SLOTS[i].pid.load(Relaxed);
    let _ = EXITING.try_with(|e| e.set((skey, a.0, pid)));
}

/// C8 checker 1 (B2): the session the calling thread unbound has drained (mcxt_stats' run_session_teardown ended; else
/// the thread's next bind or its exit): what its exit cell still holds is the session's residue, noted now (`owner_end`)
/// and moved to the orphan cell, where a later release still finds it. No pending exit on this thread: nothing.
pub fn session_drained() {
    let (skey, a, pid) = EXITING.try_with(|e| e.replace((0, 0, 0))).unwrap_or((0, 0, 0));
    if skey == 0 {
        return;
    }
    if let Some(x) = find(EXIT_AT, EXIT_CELLS, skey, Some(a)) {
        CELLS[x].acct.store(0, Relaxed);
        end_cell_noted(x, ORPHAN, OwnerClass::Session, Acct(a), crate::owner_end::Source::Owner, Some(pid));
    }
}

/// Backend account `a` has a session (its cell holds a generation): bound, or (C8 move 6) dead with no unbind.
pub(crate) fn session_bound(a: Acct) -> bool {
    slot_ix(a).is_some_and(|i| CELLS[SESSION_AT + i].key.load(Relaxed) != 0)
}

/// A new owner id (a portal's token for `portal_begin`): never 0, never reused.
pub fn new_owner_id() -> u64 {
    OWNER_IDS.fetch_add(1, Relaxed)
}

/// Portal `token` of the calling thread's backend begins: its own cell, and it counts as live until its end (a full
/// table: the session's spill cell, its end check skipped, and it is not counted).
pub fn portal_begin(token: u64) {
    let a = current();
    if let Some(i) = slot_ix(a).filter(|_| token != 0) {
        if find(PORTAL_AT, PORTAL_CELLS, token, Some(a.0)).is_none() && claim(PORTAL_AT, PORTAL_CELLS, token, a.0).is_some() {
            SLOTS[i].portals.fetch_add(1, Relaxed);
        }
    }
}

/// Portal `token` of backend account `a` ends: what it still owns moves to its session; the residue, when any.
pub fn portal_end(a: Acct, token: u64) -> Option<Residue> {
    let i = slot_ix(a)?;
    let c = find(PORTAL_AT, PORTAL_CELLS, token, Some(a.0))?;
    end_ops(a, Some((c, token)), SESSION_AT + i, crate::owner_end::Source::Owner);
    CELLS[c].acct.store(0, Relaxed);
    let r = end_cell(c, SESSION_AT + i, OwnerClass::Portal, a);
    let _ = SLOTS[i].portals.fetch_update(Relaxed, Relaxed, |n| n.checked_sub(1));
    r
}

/// The portals of backend account `a` begun and not ended.
pub fn live_portals(a: Acct) -> u32 {
    slot_ix(a).map_or(0, |i| SLOTS[i].portals.load(Relaxed))
}

/// D3204: who holds a use keyed (account `a`, owner key `key`: `Owner::acct`, `Owner::key`, a part's user list) now, by name:
/// 'statement' / 'portal' while that owner runs or is open (one id space: whichever's cell holds the key), else 'ended' (a
/// use that outlived its statement or portal; a portal begun past the full portal table reads 'ended' too); 'worker_set';
/// 'session' (a backend's own: a kept cursor); 'parked' (the depot's parked worker sets); 'cache' (another holder).
pub fn holder_name(a: Acct, key: u64) -> &'static str {
    if key & WORKER_SET_KEY != 0 {
        // a set past the set table books on the spill cell, which every set shares: its end is not seen here
        return "worker_set";
    }
    let Some(i) = slot_ix(a) else { return if a == Acct::PARKED || a == Acct::SLOT_DEPOT { "parked" } else { "cache" } };
    if key == 0 {
        "session"
    } else if CELLS[STMT_AT + i].key.load(Relaxed) == key {
        "statement"
    } else if find(PORTAL_AT, PORTAL_CELLS, key, Some(a.0)).is_some() {
        // its own cell only: an ended portal's owner locates on its session's portal spill cell, live while the session is
        "portal"
    } else {
        "ended"
    }
}

/// `holder_name`'s names, in the order the views list them.
pub const HOLDER_NAMES: [&str; 7] = ["statement", "portal", "worker_set", "session", "parked", "cache", "ended"];

/// `f(account, its session's numbers, its live portals)` over each bound backend whose session owns something now or has a
/// live portal (checker 1's at-rest read: a use on a session is a kept cursor, D1507).
pub fn for_each_session(mut f: impl FnMut(Acct, OwnerRow, u32)) {
    for i in 0..SLOTS_N {
        let a = Acct(i as u32);
        if !a.is_backend() || CELLS[SESSION_AT + i].key.load(Relaxed) == 0 {
            continue;
        }
        let (row, live) = (row_of(&CELLS[SESSION_AT + i]), SLOTS[i].portals.load(Relaxed));
        if row != OwnerRow::default() || live != 0 {
            f(a, row, live);
        }
    }
}

/// `f(holder, its numbers)` over each cache holder that owns something now (checker 1's at-rest read: a use on `Parked` is
/// a parked worker set's kept cursor, D4087).
pub fn for_each_holder(mut f: impl FnMut(Holder, OwnerRow)) {
    let fixed = [Holder::Parked, Holder::Reaping, Holder::Unbound, Holder::SlotDepot];
    for h in Consumer::ALL.iter().map(|&c| Holder::Of(c)).chain(fixed) {
        let row = row_of(&CELLS[HOLDER_AT + h.index()]);
        if row != OwnerRow::default() {
            f(h, row);
        }
    }
}

/// The cache holders' (`Consumer::is_cache`) door-1 bytes now, as `OwnerView::heap` counts an owner's: what the caches
/// keep on the heap by the ledger (`arena::road`'s `holders_heap`).
pub fn cache_holders_heap() -> i64 {
    let mut s = 0i64;
    for c in Consumer::ALL {
        if c.is_cache() {
            let v = &CELLS[HOLDER_AT + Holder::Of(c).index()];
            let held = v.committed.load(Relaxed).wrapping_add(v.ctx_named.load(Relaxed)).wrapping_add(v.ctx_inferred.load(Relaxed));
            s = s.wrapping_add(held.wrapping_sub(v.mapped.load(Relaxed)));
        }
    }
    s
}

/// Worker set `id` begins on account `a` (its leader's; the depot's while parked).
pub fn worker_set_begin(id: u32, a: Acct) {
    if find(SET_AT, SET_CELLS, id as u64 + 1, None).is_none() {
        claim(SET_AT, SET_CELLS, id as u64 + 1, a.0);
    }
}

/// Worker set `id` ends (its release): what it still owns moves to the orphan cell; the residue, when any.
pub fn worker_set_end(id: u32) -> Option<Residue> {
    let c = find(SET_AT, SET_CELLS, id as u64 + 1, None)?;
    let a = Acct(CELLS[c].acct.load(Relaxed));
    CELLS[c].acct.store(0, Relaxed);
    let r = end_cell(c, ORPHAN, OwnerClass::WorkerSet, a);
    r
}

/// An operator cell's parent cell (`NO_PARENT as usize`: none) and the key it was begun under.
#[inline]
fn parent_of(c: &OwnerCell) -> (usize, u64) {
    match c.parent.load(Relaxed) {
        0 => (NO_PARENT as usize, 0),
        p => (p as usize - 1, c.pkey.load(Relaxed)),
    }
}

#[inline]
fn is_op(cell: usize) -> bool {
    (OP_AT..NCELLS).contains(&cell)
}

/// LEDGER2: a child owner of `parent` begins (an operator's arena at its open, its statement or portal the parent; a worker
/// arena, `label | WORKER_ARENA`, its operator the parent) with `budget` bytes promised to it (its share): its charges,
/// contexts and spills book on its own cell, on the parent's account, and the parent's subtree (`usage`) sums them. Past a
/// full table or under an ended parent the child IS its parent (`op_overflow` counts the begin): its bytes book there.
pub fn op_begin(parent: Owner, label: u32, budget: u64) -> Owner {
    let (pcell, pkey, _) = locate(parent);
    if !live(parent, pcell, pkey) {
        OP_OVERFLOW.fetch_add(1, Relaxed);
        return parent;
    }
    let acct = parent.acct();
    let id = new_owner_id();
    let h = mix(id, acct.0);
    let Some(i) = (0..PROBES).map(|k| OP_AT + (h.wrapping_add(k) % OP_CELLS)).find(|&i| {
        let c = &CELLS[i];
        c.key.load(Relaxed) == 0 && c.key.compare_exchange(0, id, Relaxed, Relaxed).is_ok()
    }) else {
        OP_OVERFLOW.fetch_add(1, Relaxed);
        return parent;
    };
    let c = &CELLS[i];
    c.acct.store(acct.0, Relaxed);
    // a charge that read the last owner's key before its free and booked after it: never this owner's
    if move_cell(c, &CELLS[ORPHAN]) != OwnerRow::default() {
        STALE.fetch_add(1, Relaxed);
    }
    c.parent.store(pcell as u32 + 1, Relaxed);
    c.pkey.store(pkey, Relaxed);
    c.label.store(label & !(ENDED | SWEPT), Relaxed);
    c.budget.store(Charge::signed(budget), Relaxed);
    c.kids.store(0, Relaxed);
    if is_op(pcell) {
        CELLS[pcell].kids.fetch_add(1, Relaxed);
    }
    // the chain's top (the statement, portal or session it ends with) counts it, so that end sweeps only when one is
    match if is_op(pcell) { top_of(pcell) } else { Some((pcell, pkey)) } {
        Some((t, tk)) => {
            c.tkey.store(tk, Relaxed);
            c.top.store(t as u32 + 1, Relaxed);
            CELLS[t].ops.fetch_add(1, Relaxed);
        }
        None => c.top.store(0, Relaxed),
    }
    OPS_KEYED.fetch_add(1, Relaxed);
    if let Some(s) = slot_ix(acct) {
        OPS_ON[s].fetch_add(1, Relaxed);
    }
    Owner::Op { acct, id }
}

/// LEDGER2: operator owner `o` ends (its arena's drop). Its cell stays keyed while anything is still on it (a charge another
/// thread drops later, a child still keyed) and frees with the last of it (`op_drained`); its statement's, portal's or
/// session's end sweeps what is left (`end_ops`). A begin past the table (`o` is not an operator): nothing.
pub fn op_end(o: Owner) {
    let Owner::Op { acct, id } = o else { return };
    let Some(i) = find(OP_AT, OP_CELLS, id, Some(acct.0)) else { return };
    CELLS[i].label.fetch_or(ENDED, Relaxed);
    OP_ENDS.fetch_add(1, Relaxed);
    op_drained(i as u32);
}

/// LEDGER2: operator owner `o`'s budget (its share) is `bytes` now. Not an operator: nothing (a statement's budget is its
/// charges' reservations).
pub fn op_budget(o: Owner, bytes: u64) {
    if let Owner::Op { acct, id } = o {
        if let Some(i) = find(OP_AT, OP_CELLS, id, Some(acct.0)) {
            CELLS[i].budget.store(Charge::signed(bytes), Relaxed);
        }
    }
}

/// LEDGER2: owner `o`'s spills hold `delta` more bytes on disk (less, negative: a run file deleted), on its cell while it
/// lives, else where its charges go (its session's, the orphan's).
pub fn spilled(o: Owner, delta: i64) {
    if delta == 0 {
        return;
    }
    let (cell, key, skey) = locate(o);
    let cell = if live(o, cell, key) { cell } else { redirect(o.acct(), skey).0 as usize };
    CELLS[cell].spilled.fetch_add(delta, Relaxed);
}

/// An ended operator's cell `cell` frees once nothing is on it: no committed bytes, reservation, use, context block,
/// not-resident bytes of a listed range (design A: its entry moves them off when it moves or leaves) or keyed child.
#[inline]
fn op_drained(cell: u32) {
    let i = cell as usize;
    if !is_op(i) || CELLS[i].label.load(Relaxed) & ENDED == 0 {
        return;
    }
    op_drained_slow(i);
}

#[cold]
fn op_drained_slow(i: usize) {
    let c = &CELLS[i];
    let busy = [&c.committed, &c.reserved, &c.uses, &c.ctx_named, &c.ctx_inferred, &c.nr, &c.pins].iter().any(|x| x.load(Relaxed) != 0);
    if busy || c.kids.load(Relaxed) != 0 {
        return;
    }
    let k = c.key.load(Relaxed);
    if k == 0 || k == OP_FREEING || c.key.compare_exchange(k, OP_FREEING, Relaxed, Relaxed).is_err() {
        return;
    }
    op_unkey(i);
}

/// Operator cell `i`, its key `OP_FREEING` (its taker's), frees: its spills go to its parent while that lives, its parent's
/// child count drops (an ended parent may free with it), its fields clear, then its key.
fn op_unkey(i: usize) {
    let c = &CELLS[i];
    let (p, pk) = parent_of(c);
    let parent_live = p != NO_PARENT as usize && CELLS[p].key.load(Relaxed) == pk;
    let s = c.spilled.swap(0, Relaxed);
    if s != 0 && parent_live {
        CELLS[p].spilled.fetch_add(s, Relaxed);
    }
    if let Some(slot) = slot_ix(Acct(c.acct.load(Relaxed))) {
        let _ = OPS_ON[slot].fetch_update(Relaxed, Relaxed, |n| n.checked_sub(1));
    }
    let (t, tk) = (c.top.swap(0, Relaxed), c.tkey.swap(0, Relaxed));
    if t != 0 && CELLS[t as usize - 1].key.load(Relaxed) == tk {
        let _ = CELLS[t as usize - 1].ops.fetch_update(Relaxed, Relaxed, |n| n.checked_sub(1));
    }
    c.parent.store(0, Relaxed);
    c.pkey.store(0, Relaxed);
    c.label.store(0, Relaxed);
    c.budget.store(0, Relaxed);
    c.kids.store(0, Relaxed);
    c.acct.store(0, Relaxed);
    OPS_KEYED.fetch_sub(1, Relaxed);
    c.key.store(0, Relaxed);
    if parent_live && is_op(p) {
        let _ = CELLS[p].kids.fetch_update(Relaxed, Relaxed, |n| n.checked_sub(1));
        op_drained(p as u32);
    }
}

/// The non-operator owner cell at the top of operator cell `i`'s chain and the key it was begun under; None for a chain
/// broken by an ended operator.
fn top_of(i: usize) -> Option<(usize, u64)> {
    let mut j = i;
    for _ in 0..DEPTH {
        let c = &CELLS[j];
        let (p, pk) = parent_of(c);
        if p == NO_PARENT as usize {
            return None;
        }
        if !is_op(p) {
            return Some((p, pk));
        }
        if CELLS[p].key.load(Relaxed) != pk {
            return None;
        }
        j = p;
    }
    None
}

/// Whether operator cell `i`'s chain passes through owner cell `cell` begun under `key`.
fn reaches(i: usize, cell: usize, key: u64) -> bool {
    let mut j = i;
    for _ in 0..DEPTH {
        let c = &CELLS[j];
        let (p, pk) = parent_of(c);
        if p == NO_PARENT as usize {
            return false;
        }
        if p == cell && pk == key {
            return true;
        }
        if !is_op(p) || CELLS[p].key.load(Relaxed) != pk {
            return false;
        }
        j = p;
    }
    false
}

/// LEDGER2: the operator cells of backend account `a` end, those under owner cell `root` begun under its key when given (a
/// statement's or a portal's end), else all of them (the session's): their numbers move to cell `to`, each residue noted
/// under class `Op`, their cells free. Marked first, then taken, so a worker arena is matched through its operator before
/// either frees. Nothing to do (no cell keyed on the account): no sweep. The residues noted.
/// The operator-table sweeps backend account `a`'s ends ran so far (an end whose root had no operator keyed runs none).
pub fn op_sweeps(a: Acct) -> u64 {
    slot_ix(a).map_or(0, |s| SLOTS[s].sweeps.load(Relaxed))
}

fn end_ops(a: Acct, root: Option<(usize, u64)>, to: usize, src: crate::owner_end::Source) -> u32 {
    let Some(s) = slot_ix(a) else { return 0 };
    if OPS_ON[s].load(Relaxed) == 0 {
        return 0;
    }
    // a root none of whose operators is keyed now: nothing to sweep (its count is exact while its key holds; a stale
    // count only costs a sweep)
    if let Some((r, k)) = root {
        if CELLS[r].key.load(Relaxed) == k && CELLS[r].ops.load(Relaxed) == 0 {
            return 0;
        }
    }
    SLOTS[s].sweeps.fetch_add(1, Relaxed);
    let mine = |c: &OwnerCell| !matches!(c.key.load(Relaxed), 0 | OP_FREEING) && c.acct.load(Relaxed) == a.0;
    for i in OP_AT..NCELLS {
        if mine(&CELLS[i]) && root.is_none_or(|r| top_of(i) == Some(r)) {
            CELLS[i].label.fetch_or(SWEPT, Relaxed);
        }
    }
    let mut n = 0;
    for i in OP_AT..NCELLS {
        let c = &CELLS[i];
        let k = c.key.load(Relaxed);
        if !mine(c) || c.label.load(Relaxed) & SWEPT == 0 || c.key.compare_exchange(k, OP_FREEING, Relaxed, Relaxed).is_err() {
            continue;
        }
        let r = move_cell(c, &CELLS[to]);
        if r != OwnerRow::default() {
            crate::owner_end::note_as(src, Residue { class: OwnerClass::Op, acct: a, committed: r.committed, reserved: r.reserved, uses: r.uses }, None);
            n += 1;
        }
        // its parent is being taken too (or lives on): the child count is the sweep's, not a free's
        c.parent.store(0, Relaxed);
        op_unkey(i);
    }
    n
}

/// LEDGER2 (`usage`): an owner's numbers. `held` is measured (its charges' committed bytes and its contexts' blocks),
/// `budget` promised (its charges' reservations and an operator's share), `spilled` its spills' bytes on disk; the subtree
/// sums add every operator cell whose chain reaches it (a session's: every owner on its account).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub held: i64,
    pub budget: i64,
    pub spilled: i64,
    pub subtree: i64,
    pub subtree_spilled: i64,
    /// Design A: of `held` and of `subtree`, the bytes the residency pass found not resident (its listed ranges').
    pub not_resident: i64,
    pub subtree_not_resident: i64,
}

impl Usage {
    /// Design A: the subtree's bytes in memory: its held less what the residency pass found not resident.
    pub fn subtree_resident(&self) -> i64 {
        self.subtree.wrapping_sub(self.subtree_not_resident)
    }
}

#[inline]
fn held_of(c: &OwnerCell) -> i64 {
    c.committed.load(Relaxed).wrapping_add(c.ctx_named.load(Relaxed)).wrapping_add(c.ctx_inferred.load(Relaxed))
}

#[inline]
fn budget_of(c: &OwnerCell) -> i64 {
    c.reserved.load(Relaxed).wrapping_add(c.budget.load(Relaxed))
}

/// LEDGER2: owner `o`'s `Usage` now (zeros for an owner that ended). The subtree is summed on read: one pass over the
/// operator cells when any is keyed (the views, R6's pick and the shadow logs read it; no charge path does).
pub fn usage(o: Owner) -> Usage {
    let (cell, key, _) = locate(o);
    if !live(o, cell, key) {
        return Usage::default();
    }
    let own = &CELLS[cell];
    let (held, spilled, nr) = (held_of(own), own.spilled.load(Relaxed), own.nr.load(Relaxed));
    let mut u = Usage { held, budget: budget_of(own), spilled, subtree: held, subtree_spilled: spilled, not_resident: nr, subtree_not_resident: nr };
    let add = |u: &mut Usage, c: &OwnerCell| {
        u.subtree = u.subtree.wrapping_add(held_of(c));
        u.subtree_spilled = u.subtree_spilled.wrapping_add(c.spilled.load(Relaxed));
        u.subtree_not_resident = u.subtree_not_resident.wrapping_add(c.nr.load(Relaxed));
    };
    if let (Owner::Session(a), Some(s)) = (o, slot_ix(o.acct())) {
        add(&mut u, &CELLS[STMT_AT + s]);
        add(&mut u, &CELLS[PORTAL_SPILL_AT + s]);
        for c in &CELLS[PORTAL_AT..PORTAL_AT + PORTAL_CELLS] {
            if c.key.load(Relaxed) != 0 && c.acct.load(Relaxed) == a.0 {
                add(&mut u, c);
            }
        }
        if OPS_ON[s].load(Relaxed) != 0 {
            let t0 = pg_clock::mono_ns();
            for c in &CELLS[OP_AT..NCELLS] {
                if !matches!(c.key.load(Relaxed), 0 | OP_FREEING) && c.acct.load(Relaxed) == a.0 {
                    add(&mut u, c);
                }
            }
            scanned(t0);
        }
        return u;
    }
    if OPS_KEYED.load(Relaxed) > 0 {
        let t0 = pg_clock::mono_ns();
        for i in OP_AT..NCELLS {
            let c = &CELLS[i];
            if i != cell && !matches!(c.key.load(Relaxed), 0 | OP_FREEING) && reaches(i, cell, key) {
                add(&mut u, c);
            }
        }
        scanned(t0);
    }
    u
}

/// LEDGER2 (Q2): `usage`'s scans of the `OP_CELLS` operator cells (made while any operator is keyed): how many since start,
/// their nanoseconds in all, the longest. R6's pick and the shadow lines report them (mcx `ledger2`).
static SCANS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

fn scanned(t0: u64) {
    let ns = pg_clock::mono_ns().saturating_sub(t0);
    SCANS[0].fetch_add(1, Relaxed);
    SCANS[1].fetch_add(ns, Relaxed);
    SCANS[2].fetch_max(ns, Relaxed);
}

/// `(scans, total ns, max ns)` of `usage`'s operator-cell scans since start.
pub fn scan_cost() -> (u64, u64, u64) {
    (SCANS[0].load(Relaxed), SCANS[1].load(Relaxed), SCANS[2].load(Relaxed))
}

/// PINS: owner `o`'s `(pins, evicted, share)` now: the cache-entry bytes its scans reference, of those the evicted ones, and its
/// share of the evicted ones (zeros for an owner that ended: its numbers moved to its session's).
pub fn pins_of(o: Owner) -> (i64, i64, i64) {
    let (cell, key, _) = locate(o);
    if !live(o, cell, key) {
        return (0, 0, 0);
    }
    let c = &CELLS[cell];
    (c.pins.load(Relaxed), c.pin_ev.load(Relaxed), c.pin_sh.load(Relaxed))
}

/// LEDGER2 step 2: owner `o`'s memory-context bytes now, (named, inferred); zeros for an owner that ended.
pub fn owner_ctx(o: Owner) -> (i64, i64) {
    let (cell, key, _) = locate(o);
    if !live(o, cell, key) {
        return (0, 0);
    }
    (CELLS[cell].ctx_named.load(Relaxed), CELLS[cell].ctx_inferred.load(Relaxed))
}

/// LEDGER2: one owner cell as the owners view lists it (`for_each_owner`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnerView {
    /// Its cell (`OWNER_CELLS` of them) and its parent's (`NO_OWNER`: none): a statement's and a portal's parent is their
    /// session; an operator's its begin's parent while that holds the key it was begun under, else its account's session.
    pub cell: u32,
    pub parent: u32,
    pub name: &'static str,
    pub acct: Acct,
    pub key: u64,
    /// An operator's kind (`Kind as u32`, `WORKER_ARENA` for a worker arena); 0 for every other owner.
    pub label: u32,
    pub ended: bool,
    pub committed: i64,
    pub mapped: i64,
    pub reserved: i64,
    pub backed: i64,
    pub uses: i64,
    pub ctx_named: i64,
    pub ctx_inferred: i64,
    pub budget: i64,
    pub spilled: i64,
    /// Design A: of `committed`, the bytes the residency pass found not resident.
    pub nr: i64,
    /// PINS: the cache-entry bytes this owner's scans reference; of those the entries the cache already evicted; and the
    /// owner's share of the evicted ones (each entry's bytes over its holders). Counts beside `held`.
    pub pins: i64,
    pub pins_evicted: i64,
    pub pins_share: i64,
}

impl OwnerView {
    /// Measured: its charges' committed bytes and its contexts' blocks.
    pub fn held(&self) -> i64 {
        self.committed.wrapping_add(self.ctx_named).wrapping_add(self.ctx_inferred)
    }

    /// Promised: its charges' reservations and an operator's share.
    pub fn promised(&self) -> i64 {
        self.reserved.wrapping_add(self.budget)
    }

    /// Of `held`, door 1's (heap): its heap charges' bytes and its contexts' blocks (a context's mapped block is a door-2
    /// charge, in `mapped`).
    pub fn heap(&self) -> i64 {
        self.held().wrapping_sub(self.mapped)
    }
}

/// `OwnerView::cell`'s range and the no-parent mark.
pub const OWNER_CELLS: usize = NCELLS;
pub const NO_OWNER: u32 = NO_PARENT;

fn view_of(i: usize, name: &'static str, acct: Acct, parent: u32) -> OwnerView {
    let c = &CELLS[i];
    let label = c.label.load(Relaxed);
    let op = is_op(i);
    OwnerView {
        cell: i as u32,
        parent,
        name,
        acct,
        key: c.key.load(Relaxed),
        label: if op { label & !(ENDED | SWEPT) } else { 0 },
        ended: op && label & ENDED != 0,
        committed: c.committed.load(Relaxed),
        mapped: c.mapped.load(Relaxed),
        reserved: c.reserved.load(Relaxed),
        backed: c.backed.load(Relaxed),
        uses: c.uses.load(Relaxed),
        ctx_named: c.ctx_named.load(Relaxed),
        ctx_inferred: c.ctx_inferred.load(Relaxed),
        budget: c.budget.load(Relaxed),
        spilled: c.spilled.load(Relaxed),
        nr: c.nr.load(Relaxed),
        pins: c.pins.load(Relaxed),
        pins_evicted: c.pin_ev.load(Relaxed),
        pins_share: c.pin_sh.load(Relaxed),
    }
}

/// LEDGER2: `f` over every owner cell that is keyed or holds anything now: each backend's session (its statement, its
/// portals, its portal spill cell under it), the worker sets, the cache holders, the exit cells and the orphan cell, then
/// the operator cells. Read now, cell by cell (a racing charge may show in one row and not its parent's sum).
pub fn for_each_owner(mut f: impl FnMut(&OwnerView)) {
    let some = |v: &OwnerView| v.committed != 0 || v.reserved != 0 || v.uses != 0 || v.ctx_named != 0 || v.ctx_inferred != 0 || v.spilled != 0 || v.nr != 0 || v.pins != 0;
    for s in 0..SLOTS_N {
        let a = Acct(s as u32);
        if !a.is_backend() {
            continue;
        }
        let skey = CELLS[SESSION_AT + s].key.load(Relaxed);
        let sv = view_of(SESSION_AT + s, "session", a, NO_PARENT);
        let parent = if skey != 0 { (SESSION_AT + s) as u32 } else { NO_PARENT };
        if skey != 0 || some(&sv) {
            f(&sv);
        }
        let st = view_of(STMT_AT + s, "statement", a, parent);
        if st.key != 0 || some(&st) {
            f(&st);
        }
        let ps = view_of(PORTAL_SPILL_AT + s, "portal", a, parent);
        if some(&ps) {
            f(&ps);
        }
    }
    let session_of = |a: Acct| slot_ix(a).filter(|&s| CELLS[SESSION_AT + s].key.load(Relaxed) != 0).map_or(NO_PARENT, |s| (SESSION_AT + s) as u32);
    for i in PORTAL_AT..PORTAL_AT + PORTAL_CELLS {
        if CELLS[i].key.load(Relaxed) != 0 {
            let a = Acct(CELLS[i].acct.load(Relaxed));
            f(&view_of(i, "portal", a, session_of(a)));
        }
    }
    for i in SET_AT..=SET_SPILL {
        let v = view_of(i, "worker_set", Acct(CELLS[i].acct.load(Relaxed)), NO_PARENT);
        if v.key != 0 || some(&v) {
            f(&v);
        }
    }
    let fixed = [Holder::Parked, Holder::Reaping, Holder::Unbound, Holder::SlotDepot];
    for h in Consumer::ALL.iter().map(|&c| Holder::Of(c)).chain(fixed) {
        let v = view_of(HOLDER_AT + h.index(), "cache", h.acct(), NO_PARENT);
        if some(&v) {
            f(&v);
        }
    }
    for i in EXIT_AT..EXIT_AT + EXIT_CELLS {
        if CELLS[i].key.load(Relaxed) != 0 {
            f(&view_of(i, "exit", Acct::UNBOUND, NO_PARENT));
        }
    }
    let o = view_of(ORPHAN, "orphan", Acct::UNBOUND, NO_PARENT);
    if some(&o) {
        f(&o);
    }
    if OPS_KEYED.load(Relaxed) <= 0 {
        return;
    }
    for i in OP_AT..NCELLS {
        let c = &CELLS[i];
        if matches!(c.key.load(Relaxed), 0 | OP_FREEING) {
            continue;
        }
        let a = Acct(c.acct.load(Relaxed));
        let (p, pk) = parent_of(c);
        let parent = if p != NO_PARENT as usize && CELLS[p].key.load(Relaxed) == pk { p as u32 } else { session_of(a) };
        let name = if c.label.load(Relaxed) & WORKER_ARENA != 0 { "worker_arena" } else { "operator" };
        f(&view_of(i, name, a, parent));
    }
}

/// `unattr`: what the owners on backend account `a` hold now (both doors), as `for_each_owner`'s rows sum for
/// the owners view's 'unattributed' row: its session, statement and portal spill cells, its keyed portals, the worker sets
/// on it and its keyed operator cells. Read cell by cell (a racing charge may be in or out); no allocation, no lock.
pub(crate) fn acct_held(a: Acct) -> i64 {
    let Some(s) = slot_ix(a) else { return 0 };
    let held = |c: &OwnerCell| c.committed.load(Relaxed).wrapping_add(c.ctx_named.load(Relaxed)).wrapping_add(c.ctx_inferred.load(Relaxed));
    let mut sum = held(&CELLS[SESSION_AT + s]).wrapping_add(held(&CELLS[STMT_AT + s])).wrapping_add(held(&CELLS[PORTAL_SPILL_AT + s]));
    for c in &CELLS[PORTAL_AT..PORTAL_AT + PORTAL_CELLS] {
        if c.key.load(Relaxed) != 0 && c.acct.load(Relaxed) == a.0 {
            sum = sum.wrapping_add(held(c));
        }
    }
    for c in &CELLS[SET_AT..=SET_SPILL] {
        if c.acct.load(Relaxed) == a.0 {
            sum = sum.wrapping_add(held(c));
        }
    }
    if OPS_KEYED.load(Relaxed) > 0 {
        for c in &CELLS[OP_AT..NCELLS] {
            if !matches!(c.key.load(Relaxed), 0 | OP_FREEING) && c.acct.load(Relaxed) == a.0 {
                sum = sum.wrapping_add(held(c));
            }
        }
    }
    sum
}

/// LEDGER2: (operator cells keyed now, operator begins that charged their parent: a full table or an ended parent,
/// operator ends).
pub fn op_counters() -> (i64, u64, u64) {
    (OPS_KEYED.load(Relaxed), OP_OVERFLOW.load(Relaxed), OP_ENDS.load(Relaxed))
}

/// The portal the backend on account `a` is executing now (0: none).
pub fn active_portal(a: Acct) -> u64 {
    SLOTS.get(a.0 as usize).map_or(0, |s| s.portal.load(Relaxed))
}

/// The calling thread's backend executes portal `token` until the guard drops (the runner's Execute step): the charges
/// made in that time without an explicit owner are the portal's.
#[must_use]
pub fn enter_portal(token: u64) -> PortalScope {
    let a = current();
    let back = SLOTS.get(a.0 as usize).filter(|_| a.is_backend()).map_or(0, |s| s.portal.swap(token, Relaxed));
    PortalScope { acct: a, back }
}

/// The portal the backend executed before an `enter_portal`, restored when it drops.
#[derive(Debug)]
pub struct PortalScope {
    acct: Acct,
    back: u64,
}

impl Drop for PortalScope {
    fn drop(&mut self) {
        if self.acct.is_backend() {
            if let Some(s) = SLOTS.get(self.acct.0 as usize) {
                s.portal.store(self.back, Relaxed);
            }
        }
    }
}

/// The owner (and its release point) a charge made now on the calling thread takes: the portal its backend executes,
/// else its running statement, else its session; a non-backend account's holder.
pub fn context() -> (Owner, Release) {
    context_of(current())
}

/// The calling thread's running statement (never its portal: a plan or an admission is the statement's, at Bind and in a
/// nested statement too), else its session; any other owner `context` gives as it is.
pub fn statement_owner() -> Owner {
    let acct = match context().0 {
        Owner::Portal { acct, .. } => acct,
        o => return o,
    };
    match crate::running_statement(acct) {
        0 => Owner::Session(acct),
        stmt => Owner::Statement { acct, stmt },
    }
}

/// The plan-time child owner's label (`op_begin(statement_owner(), LABEL_PLAN, 0)`).
pub const LABEL_PLAN: u32 = Kind::Region as u32;

/// `context` for account `a`.
pub fn context_of(a: Acct) -> (Owner, Release) {
    if let Some(i) = slot_ix(a) {
        let s = &SLOTS[i];
        let p = s.portal.load(Relaxed);
        if p != 0 {
            return (Owner::Portal { acct: a, portal: p }, Release::Portal);
        }
        let st = s.stmt.load(Relaxed);
        if st != 0 {
            return (Owner::Statement { acct: a, stmt: st }, Release::Statement);
        }
        return (Owner::Session(a), Release::Session);
    }
    (Owner::Cache(Holder::of_acct(a)), Release::Evict)
}

/// The calling thread charges owner `o`'s account until the guard drops (a worker serving its leader's statement, a
/// cache's insert): the typed `enter`.
#[must_use]
pub fn enter_as(o: Owner) -> Entered {
    Entered { back: switch(o.acct()), paused: 0, visit: false }
}

/// C8 (move 7, C1; memlead 2026-10-12T17:00:54Z, main 17:02:28Z): door 2 of a memory context's mapped block, mcx heap.rs's
/// door its one caller (RO fences it there). The block is raw allocator memory with no header to hold a `Charge`, so the
/// home keeps its owner by address (`DOOR_TABLE`): the map (`on`) books `bytes` on the calling thread's context owner
/// (`context`: the portal it executes, else its running statement, else its session; a holder off a backend), its cell and
/// its account, and remembers that owner for `ptr`; the unmap (`!on`) gives them back to THAT owner, never the freeing
/// thread's, its stale redirect followed as `Charge::cell` follows a charge's (an ended owner's numbers are on its session's
/// or the orphan cell). A block the table has no room for is counted on the calling thread's account only, as before C8,
/// and in `door_overflow` (checker 1 reads it red): never a residue the block did not leave. Kind `Mapped`.
#[cold]
#[inline(never)]
pub fn door_mapped(ptr: usize, bytes: u64, on: bool) {
    let b = bytes.min(i64::MAX as u64) as i64;
    if on {
        let (o, _) = context();
        let (cell, key, skey) = cell_of(o);
        let acct = if past_session(cell) && o.acct().is_backend() { Acct::UNBOUND } else { o.acct() };
        door_map_on(ptr, bytes, cell, key, skey, acct);
        return;
    }
    match door_find(ptr) {
        Some(i) => {
            let (cell, _, _, acct) = DOOR_TABLE[i].owner();
            let n = DOOR_TABLE[i].bytes.load(Relaxed).min(i64::MAX as u64) as i64;
            debug_assert_eq!(n, b, "memledger: a mapped block unmapped with another size than it was mapped with");
            mapped_on(cell, -n);
            post(acct, -n, -n, Kind::Mapped, -n);
            DOOR_TABLE[i].ptr.store(DOOR_TOMB, Ordering::Release);
        }
        None => post(current(), -b, -b, Kind::Mapped, -b),
    }
}

/// `door_mapped`'s map for the owner in cell `cell` (taken under `key`, its session's `skey`) on account `acct`.
fn door_map_on(ptr: usize, bytes: u64, cell: u32, key: u64, skey: u64, acct: Acct) {
    let b = bytes.min(i64::MAX as u64) as i64;
    match door_claim(ptr) {
        Some(i) => {
            DOOR_TABLE[i].set(cell, key, skey, acct, bytes);
            mapped_on(cell, b);
            post(acct, b, b, Kind::Mapped, b);
        }
        None => {
            DOOR_OVERFLOW.fetch_add(1, Relaxed);
            post(current(), b, b, Kind::Mapped, b);
        }
    }
}

/// LEDGER2 step 2: `door_mapped`'s map for a block of a memory context whose owner is `o` (mcx heap.rs, from the context's
/// arena): the block is that owner's, not the calling thread's context owner's. An owner of no cell (`CtxOwner::NONE`):
/// `door_mapped`.
#[cold]
#[inline(never)]
pub fn door_mapped_owned(ptr: usize, bytes: u64, o: &mut CtxOwner) {
    if o.cell == NO_CTX {
        return door_mapped(ptr, bytes, true);
    }
    o.follow();
    door_map_on(ptr, bytes, o.cell, o.key, o.skey, o.acct);
}

/// LEDGER2 step 2: a memory context's owner as its blocks post to it (mcx's arenas hold one; a child takes its parent's, a
/// root names its owner or takes the creating thread's): the owner's cell, the key it was taken under and its session's
/// (a session owner's own), its account, whether a root named it, and an unnamed root's name slot (`unnamed_roots`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CtxOwner {
    cell: u32,
    acct: Acct,
    key: u64,
    skey: u64,
    named: bool,
    root: u16,
}

/// No owner: its posts go nowhere (a context made before the ledger has a thread, a build without the ledger).
const NO_CTX: u32 = u32::MAX;

impl CtxOwner {
    pub const NONE: CtxOwner = CtxOwner { cell: NO_CTX, acct: Acct::UNBOUND, key: 0, skey: 0, named: false, root: 0 };

    /// Owner `o`, named by its root (a session root's session, a portal's PortalContext).
    pub fn named(o: Owner) -> CtxOwner {
        let (cell, key, skey) = cell_of(o);
        let acct = if past_session(cell) && o.acct().is_backend() { Acct::UNBOUND } else { o.acct() };
        let skey = if skey == 0 && matches!(o, Owner::Session(_)) { key } else { skey };
        CtxOwner { cell, acct, key, skey, named: true, root: 0 }
    }

    /// The creating thread's owner now (`context`), once, at an unnamed root's creation: its bytes are inferred, and
    /// counted under the root's `name` too.
    pub fn inferred(name: &'static str) -> CtxOwner {
        let o = CtxOwner::named(context().0);
        CtxOwner { named: false, root: root_slot(name), ..o }
    }

    pub fn is_named(&self) -> bool {
        self.named
    }

    /// This owner where its posts land now: an ended owner's redirect applied (`follow`, without counting it stale). A
    /// context and its child took one owner, and one of them followed its end while the other has not posted since: they
    /// are one owner's still, and compare equal this way.
    pub fn followed(self) -> CtxOwner {
        let mut o = self;
        if o.key != 0 && CELLS[o.cell as usize].key.load(Relaxed) != o.key {
            (o.cell, o.key, o.skey) = redirect(o.acct, o.skey);
            if past_session(o.cell) && o.acct.is_backend() {
                o.acct = Acct::UNBOUND;
            }
        }
        o
    }

    /// An owner that ended under it: its numbers' place now (`Charge::cell`'s redirect).
    #[inline]
    fn follow(&mut self) {
        if self.key != 0 && CELLS[self.cell as usize].key.load(Relaxed) != self.key {
            STALE.fetch_add(1, Relaxed);
            (self.cell, self.key, self.skey) = redirect(self.acct, self.skey);
            if past_session(self.cell) && self.acct.is_backend() {
                self.acct = Acct::UNBOUND;
            }
        }
    }

    /// `delta` bytes of the context's door-1 blocks on (off, negative) its owner (a block of `heap::MMAP_THRESHOLD` and more
    /// is door 2's: `door_mapped_owned`). Block grain: an mcx arena's new, freed and reset blocks. Door 1 counted the block
    /// on the calling thread's account: on another account's owner the bytes move there with it (a free moves them back to
    /// the freeing thread's, where the allocator takes them off), as a door-1 `Charge`'s commit and free do, so an account
    /// holds what its owners hold whichever thread allocates or frees.
    #[inline]
    pub fn post(&mut self, delta: i64) {
        if self.cell == NO_CTX || delta == 0 {
            return;
        }
        self.follow();
        let cur = current();
        if cur != self.acct {
            post(cur, -delta, 0, Kind::Region, 0);
            post(self.acct, delta, 0, Kind::Region, 0);
        }
        self.on_cell(delta);
    }

    #[inline]
    fn on_cell(&self, delta: i64) {
        let c = &CELLS[self.cell as usize];
        if self.named {
            c.ctx_named.fetch_add(delta, Relaxed);
        } else {
            c.ctx_inferred.fetch_add(delta, Relaxed);
            ROOT_BYTES[self.root as usize].fetch_add(delta, Relaxed);
        }
        if delta < 0 {
            op_drained(self.cell);
        }
    }

    /// `post` for blocks the page pool served: no door counted them on the calling thread, so they land on the owner's
    /// account and cell with no move off `current()` (an owner of no cell: on `current()`, as door 1 would).
    #[inline]
    pub fn post_pool(&mut self, delta: i64) {
        if delta == 0 {
            return;
        }
        if self.cell == NO_CTX {
            return post_q(current(), delta, 0, delta, Kind::Region, 0);
        }
        self.follow();
        post_q(self.acct, delta, 0, delta, Kind::Region, 0);
        self.on_cell(delta);
    }

    /// The lifetime of the owner's cell kind. An owner that ended under it (its cell's key moved on: a statement, portal or
    /// operator kept past its end) follows first, as `post` / `post_pool` do, so it reads its session's.
    pub fn lifetime(&mut self) -> OwnerLife {
        self.follow();
        match self.cell as usize {
            _ if self.cell == NO_CTX => OwnerLife::Process,
            c if c < SESSION_AT => OwnerLife::Statement,
            c if c < PORTAL_SPILL_AT => OwnerLife::Session,
            c if c < HOLDER_AT => OwnerLife::Statement,
            // a consumer's own, and the depots' parked memory (Holder::Parked): both go at the idle release
            c if c <= HOLDER_AT + Holder::Parked.index() => OwnerLife::Cache,
            c if c < ORPHAN => OwnerLife::Process,
            c if c < OP_AT => OwnerLife::Session,
            _ => OwnerLife::Statement,
        }
    }

    /// The context's `bytes` move from this owner to `to` in one step (`MemoryContext::set_owner`): a free never lands on an
    /// owner that did not take it.
    pub fn transfer(&mut self, to: CtxOwner, bytes: i64) {
        self.post(-bytes);
        *self = to;
        self.post(bytes);
    }
}

/// What a context owner's blocks live as long as (`CtxOwner::lifetime`): the page pool keeps the classes apart.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum OwnerLife {
    Statement = 0,
    Session = 1,
    Cache = 2,
    Process = 3,
}

/// The page pool's free memory: claimed from the OS, the free parts of it (warm, on thread shelves, stranded in
/// partly used superpages). Owned bytes are on their owners' accounts (`post_pool`).
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct PoolReading {
    pub claimed: u64,
    pub warm_free: u64,
    pub shelved: u64,
    pub stranded: u64,
}

static POOL_CLAIMED: AtomicI64 = AtomicI64::new(0);
static POOL_PROBE: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

/// The page pool's door: `delta` bytes of RAM claimed from the OS (+) or released (-), superpage grain. A process total
/// on no account, so `doors_total` never counts a pooled byte twice.
#[inline]
pub fn door_pool(delta: i64) {
    POOL_CLAIMED.fetch_add(delta, Relaxed);
}

pub fn pool_claimed() -> i64 {
    POOL_CLAIMED.load(Relaxed)
}

/// Of `doors_total`, the page pool's bytes on every account (`AccountRow::pagepool_bytes` summed).
pub fn pagepool_total() -> i64 {
    SLOTS.iter().fold(0i64, |t, s| t.wrapping_add(s.pooled.load(Relaxed)))
}

/// The pool's resident bytes no owner holds (free, shelved, stranded, rounding): claimed less the owners' (`post_pool`)
/// and less door 1's (`pool_door1`: `PoolAlloc`'s blocks, on their accounts in `doors_total` already, so a door-1 byte the
/// pool serves is counted once, as owned; pppress card 9).
pub fn pool_unowned() -> i64 {
    pool_claimed().wrapping_sub(pagepool_total()).wrapping_sub(pool_door1()).max(0)
}

static POOL_DOOR1: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

/// pppress card 9: how to read the pool's bytes door 1 charged, on demand (the pool's `PoolAlloc` as the global
/// allocator: `pagepool::global_live`, its blocks at their good size, which door 1 charges as the layout's bytes on their
/// accounts plus the rounding on `Acct::ROUNDING`). Unset (no `PoolAlloc`): none.
pub fn set_pool_door1(f: fn() -> u64) {
    POOL_DOOR1.store(f as *mut (), Ordering::Release);
}

/// pppress card 9: of the pool's claimed bytes, the ones door 1 charged (`set_pool_door1`); 0 when unset. Owned bytes:
/// `doors_total` counts them on their accounts, so `pool_unowned` and the views' pool rows leave them out.
pub fn pool_door1() -> i64 {
    let p = POOL_DOOR1.load(Ordering::Acquire);
    if p.is_null() {
        return 0;
    }
    // SAFETY: only set_pool_door1 stores here, always from fn() -> u64.
    let f = unsafe { core::mem::transmute::<*mut (), fn() -> u64>(p) };
    i64::try_from(f()).unwrap_or(i64::MAX)
}

/// The views' and the ladder's reading of the pool (`pool_reading`), read on demand: no post per take.
pub fn set_pool_probe(f: fn() -> PoolReading) {
    POOL_PROBE.store(f as *mut (), Ordering::Release);
}

/// A pool probe is set (the pool is armed for a consumer).
pub fn pool_probed() -> bool {
    !POOL_PROBE.load(Relaxed).is_null()
}

/// The pool now; with no probe set, its door's claimed bytes alone.
pub fn pool_reading() -> PoolReading {
    let p = POOL_PROBE.load(Ordering::Acquire);
    if p.is_null() {
        return PoolReading { claimed: pool_claimed().max(0) as u64, ..PoolReading::default() };
    }
    // SAFETY: only set_pool_probe stores here, always from fn() -> PoolReading.
    let f = unsafe { core::mem::transmute::<*mut (), fn() -> PoolReading>(p) };
    f()
}

/// LEDGER2 step 2: the unnamed roots' names (one slot per name: its `&'static str`'s address, as mcx's debug census keys
/// them) and the bytes the contexts under them hold, so the biggest get named one by one; slot 0 takes the names past the
/// table.
pub const ROOT_SLOTS: usize = 512;
static ROOT_NAME: [AtomicUsize; ROOT_SLOTS] = [const { AtomicUsize::new(0) }; ROOT_SLOTS];
static ROOT_LEN: [AtomicUsize; ROOT_SLOTS] = [const { AtomicUsize::new(0) }; ROOT_SLOTS];
static ROOT_BYTES: [AtomicI64; ROOT_SLOTS] = [const { AtomicI64::new(0) }; ROOT_SLOTS];

fn root_slot(name: &'static str) -> u16 {
    let p = name.as_ptr() as usize;
    let h = (p >> 3).wrapping_mul(0x9E37_79B9) % (ROOT_SLOTS - 1);
    for k in 0..ROOT_SLOTS - 1 {
        let i = 1 + (h + k) % (ROOT_SLOTS - 1);
        let cur = ROOT_NAME[i].load(Relaxed);
        if cur == p {
            return i as u16;
        }
        if cur == 0 && ROOT_NAME[i].compare_exchange(0, p, Relaxed, Relaxed).is_ok() {
            ROOT_LEN[i].store(name.len(), Relaxed);
            return i as u16;
        }
    }
    0
}

/// LEDGER2 step 2: `f(name, bytes)` over the unnamed roots' names holding bytes now ('' for the names past the table).
pub fn unnamed_roots(mut f: impl FnMut(&'static str, i64)) {
    for i in 0..ROOT_SLOTS {
        let b = ROOT_BYTES[i].load(Relaxed);
        if b == 0 {
            continue;
        }
        let (p, n) = (ROOT_NAME[i].load(Relaxed), ROOT_LEN[i].load(Relaxed));
        // SAFETY: a slot's address and length are a `&'static str`'s (`root_slot`), the length stored after the address
        // and read after it here: a slot whose length is not stored yet reads as ''.
        let name = if p == 0 || n == 0 { "" } else { unsafe { core::str::from_utf8_unchecked(core::slice::from_raw_parts(p as *const u8, n)) } };
        f(name, b);
    }
}

/// Door 2's `n` bytes on (or off) owner cell `cell`.
#[inline]
fn mapped_on(cell: u32, n: i64) {
    let c = &CELLS[cell as usize];
    c.committed.fetch_add(n, Relaxed);
    c.mapped.fetch_add(n, Relaxed);
    if n < 0 {
        op_drained(cell);
    }
}

/// C8 (move 7, C1): `door_mapped` for a mapped block resized in place or moved (mcx heap.rs's remap, after its
/// `door_moving(old, true)`): the block stays its owner's, the difference is booked on that owner (never the resizing
/// thread's), and its entry follows it to `new`. A moved block that finds no room under its new address leaves the table:
/// its bytes go to the calling thread's account (where a later unmap of an untracked block takes them) and `door_overflow`
/// counts it.
#[cold]
#[inline(never)]
pub fn door_remapped(old: usize, new: usize, old_bytes: u64, new_bytes: u64) {
    let d = (new_bytes.min(i64::MAX as u64) as i64).wrapping_sub(old_bytes.min(i64::MAX as u64) as i64);
    let Some(i) = door_find(old | DOOR_MOVING).or_else(|| door_find(old)) else {
        post(current(), d, d, Kind::Mapped, d);
        return;
    };
    let (cell, key, skey, acct) = DOOR_TABLE[i].owner();
    mapped_on(cell, d);
    post(acct, d, d, Kind::Mapped, d);
    if new == old {
        DOOR_TABLE[i].bytes.store(new_bytes, Relaxed);
        DOOR_TABLE[i].ptr.store(old, Ordering::Release);
        return;
    }
    match door_claim(new) {
        Some(j) => DOOR_TABLE[j].set(cell, key, skey, acct, new_bytes),
        None => {
            DOOR_OVERFLOW.fetch_add(1, Relaxed);
            let n = new_bytes.min(i64::MAX as u64) as i64;
            mapped_on(cell, -n);
            post(acct, -n, -n, Kind::Mapped, -n);
            post(current(), n, n, Kind::Mapped, n);
        }
    }
    DOOR_TABLE[i].ptr.store(DOOR_TOMB, Ordering::Release);
}

/// C8 (move 7, C1): the mapped block at `ptr` is being remapped (`on`, before the remap) or stays where it was (`!on`, the
/// remap failed). A moving block's entry is keyed apart from its address: the remap may free the old range before
/// `door_remapped` runs, and a block another thread maps there in between is a new entry, never this one.
#[cold]
#[inline(never)]
pub fn door_moving(ptr: usize, on: bool) {
    let (from, to) = if on { (ptr, ptr | DOOR_MOVING) } else { (ptr | DOOR_MOVING, ptr) };
    if let Some(i) = door_find(from) {
        DOOR_TABLE[i].ptr.store(to, Ordering::Release);
    }
}

/// The mapped blocks `door_mapped` failed to remember (the table full under their probes): counted on an account only.
pub fn door_overflow() -> u64 {
    DOOR_OVERFLOW.load(Relaxed)
}

/// The mapped blocks the table remembers now (tests, the census).
pub fn door_blocks() -> usize {
    DOOR_TABLE.iter().filter(|e| !matches!(e.ptr.load(Relaxed), 0 | DOOR_TOMB)).count()
}

/// `door_mapped`'s table size: mcx maps a block only at 32 MiB and more, so a full table is 128 GiB of live mapped blocks.
pub const DOOR_BLOCKS: usize = 1 << DOOR_BITS;
const DOOR_BITS: u32 = 12;
/// Probes before a block counts as overflow.
const DOOR_PROBES: usize = 64;
/// A freed entry (a probe walks past it; a new block may take it).
const DOOR_TOMB: usize = usize::MAX;
/// The address bit a moving block's entry sets (`door_moving`): a mapping starts on a page, so its bit 0 is clear.
const DOOR_MOVING: usize = 1;

/// One remembered mapped block: its address (0 = never used, `DOOR_TOMB` = freed) and the owner its map booked.
struct DoorBlock {
    ptr: AtomicUsize,
    cell: AtomicU32,
    acct: AtomicU32,
    key: AtomicU64,
    skey: AtomicU64,
    bytes: AtomicU64,
}

#[allow(clippy::declare_interior_mutable_const)]
const DOOR0: DoorBlock = DoorBlock {
    ptr: AtomicUsize::new(0),
    cell: AtomicU32::new(0),
    acct: AtomicU32::new(0),
    key: AtomicU64::new(0),
    skey: AtomicU64::new(0),
    bytes: AtomicU64::new(0),
};

static DOOR_TABLE: [DoorBlock; DOOR_BLOCKS] = [DOOR0; DOOR_BLOCKS];
static DOOR_OVERFLOW: AtomicU64 = AtomicU64::new(0);

impl DoorBlock {
    /// The owner fields, written by the thread that claimed the entry before the block is handed out.
    fn set(&self, cell: u32, key: u64, skey: u64, acct: Acct, bytes: u64) {
        self.cell.store(cell, Relaxed);
        self.key.store(key, Relaxed);
        self.skey.store(skey, Relaxed);
        self.acct.store(acct.0, Relaxed);
        self.bytes.store(bytes, Ordering::Release);
    }

    /// The block's owner now: (cell, key, session key, account), an ended owner's stale redirect followed as
    /// `Charge::cell` follows it (to its session's cell while that lives, its exit cell while that session unbound and has
    /// not drained, else the orphan's; past its session a backend's account is UNBOUND).
    fn owner(&self) -> (u32, u64, u64, Acct) {
        let _ = self.bytes.load(Acquire);
        let (mut cell, mut key, mut skey) = (self.cell.load(Relaxed), self.key.load(Relaxed), self.skey.load(Relaxed));
        let mut acct = Acct(self.acct.load(Relaxed));
        if key != 0 && CELLS[cell as usize].key.load(Relaxed) != key {
            STALE.fetch_add(1, Relaxed);
            // a session's own block carries its session's generation as its key (`locate`), as a session's charge does
            let session = (SESSION_AT..SESSION_AT + SLOTS_N).contains(&(cell as usize));
            (cell, key, skey) = redirect(acct, if skey == 0 && session { key } else { skey });
            if past_session(cell) && acct.is_backend() {
                acct = Acct::UNBOUND;
            }
        }
        (cell, key, skey, acct)
    }
}

/// The `k`th probe for `ptr`: the product's top bits (a mapping's address is page- and often 2 MiB-aligned, so its low
/// bits carry nothing), the moving bit outside them.
#[inline]
fn door_at(ptr: usize, k: usize) -> usize {
    (((ptr as u64 >> 12).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - DOOR_BITS)) as usize).wrapping_add(k) & (DOOR_BLOCKS - 1)
}

/// A free entry for `ptr` (never used or freed), claimed; None when the probes find none.
fn door_claim(ptr: usize) -> Option<usize> {
    (0..DOOR_PROBES).map(|k| door_at(ptr, k)).find(|&i| {
        let e = &DOOR_TABLE[i].ptr;
        let cur = e.load(Relaxed);
        (cur == 0 || cur == DOOR_TOMB) && e.compare_exchange(cur, ptr, AcqRel, Relaxed).is_ok()
    })
}

/// The entry remembering `ptr`, if any (a probe stops at a never-used entry: no block was placed past it).
fn door_find(ptr: usize) -> Option<usize> {
    for k in 0..DOOR_PROBES {
        let i = door_at(ptr, k);
        match DOOR_TABLE[i].ptr.load(Acquire) {
            p if p == ptr => return Some(i),
            0 => return None,
            _ => {}
        }
    }
    None
}

/// The charges' process counters: (charges that outlived their owner, begins past a full table, per owner class the ends
/// that found a residue in its cell and the residues' committed bytes: `owner_end`'s owner-source totals).
pub fn counters() -> (u64, u64, [u64; OWNER_CLASSES], [u64; OWNER_CLASSES]) {
    let t = OwnerClass::ALL.map(|c| crate::owner_end::totals(crate::owner_end::Source::Owner, c));
    (STALE.load(Relaxed), OVERFLOW.load(Relaxed), t.map(|x| x.events), t.map(|x| x.committed))
}

/// One owned charge (see the module doc). No `Clone`: a charge is moved, transferred or dropped, never copied.
#[must_use]
#[derive(Debug)]
pub struct Charge {
    owner: Owner,
    /// The owner's account when the charge was made or last transferred: every byte the charge booked is there.
    acct: Acct,
    release: Release,
    kind: Kind,
    door: Door,
    cell: u32,
    key: u64,
    skey: u64,
    committed: u64,
    reserved: u64,
    /// C8 move 3c: of `reserved`, the room its owner's draws back with pages on the doors (`back`): still the claim, not
    /// the account's unbacked room.
    backed: u64,
    /// Door 1: of the heap block's bytes, the pages given back to the kernel (off the account until committed again).
    given: u64,
    uses: u32,
    /// Its session ended under it: the unbind moved its bytes to UNBOUND (its `acct` now) and dropped its slot's reserved
    /// bytes, so its reservation is on no account.
    orphan: bool,
}

impl Charge {
    /// No charge: nothing booked, its drop a no-op (a region not made yet, one whose bytes went with a hand-off). Commit
    /// nothing on it: its owner is the unbound holder.
    pub const NONE: Charge = Charge {
        owner: Owner::Cache(Holder::Unbound),
        acct: Acct::UNBOUND,
        release: Release::Evict,
        kind: Kind::Region,
        door: Door::Heap,
        cell: (HOLDER_AT + Holder::Unbound.index()) as u32,
        key: 0,
        skey: 0,
        committed: 0,
        reserved: 0,
        backed: 0,
        given: 0,
        uses: 0,
        orphan: false,
    };

    fn new(owner: Owner, release: Release, kind: Kind, door: Door) -> Charge {
        let (cell, key, skey) = cell_of(owner);
        let acct = if past_session(cell) && owner.acct().is_backend() { Acct::UNBOUND } else { owner.acct() };
        Charge { owner, acct, release, kind, door, cell, key, skey, committed: 0, reserved: 0, backed: 0, given: 0, uses: 0, orphan: false }
    }

    /// `bytes` of promised room for owner `o`: on its cell and its account's reserved bytes, never in its `net`.
    pub fn reserve(o: Owner, release: Release, kind: Kind, door: Door, bytes: u64) -> Charge {
        let mut c = Charge::new(o, release, kind, door);
        c.grow(bytes);
        c
    }

    /// `bytes` committed at once for owner `o` (no reservation before them).
    pub fn open(o: Owner, release: Release, kind: Kind, door: Door, bytes: u64) -> Charge {
        let mut c = Charge::new(o, release, kind, door);
        c.commit(bytes);
        c
    }

    /// One use of something another owner's bytes hold (a part-cache use): 0 bytes, 1 use on owner `o`.
    pub fn use_of(o: Owner, release: Release) -> Charge {
        let mut c = Charge::new(o, release, Kind::Region, Door::Heap);
        c.uses = 1;
        c.cell().uses.fetch_add(1, Relaxed);
        c
    }

    /// The owner's cell now; a charge whose owner ended under it follows its numbers to its session's cell or the orphan's.
    #[inline]
    fn cell(&mut self) -> &'static OwnerCell {
        if self.key != 0 && CELLS[self.cell as usize].key.load(Relaxed) != self.key {
            STALE.fetch_add(1, Relaxed);
            // a session's own charge carries its session's generation as its key (`locate`)
            let skey = if self.skey == 0 && matches!(self.owner, Owner::Session(_)) { self.key } else { self.skey };
            (self.cell, self.key, self.skey) = redirect(self.acct, skey);
            if past_session(self.cell) && self.acct.is_backend() {
                (self.acct, self.orphan) = (Acct::UNBOUND, true);
            }
        }
        &CELLS[self.cell as usize]
    }

    /// PINS: the owner's pin counts (and its account's) moved by `(pins, evicted, share)`; a charge whose owner ended
    /// follows its numbers as `commit` does.
    pub(crate) fn pin_book(&mut self, d: (i64, i64, i64)) {
        let cell = self.cell();
        cell.pins.fetch_add(d.0, Relaxed);
        cell.pin_ev.fetch_add(d.1, Relaxed);
        cell.pin_sh.fetch_add(d.2, Relaxed);
        crate::pin_slot(self.acct, d);
        if d.0 < 0 {
            op_drained(self.cell);
        }
    }

    /// A zero-byte charge of owner `o`: the handle `pin_book` resolves its cell through.
    pub(crate) fn handle(o: Owner) -> Charge {
        Charge::new(o, Release::Statement, Kind::Region, Door::Heap)
    }

    /// The account's promised room moved by `d` (not for an orphan's own reservation: its unbind dropped it).
    #[inline]
    fn pool(&self, a: Acct, d: i64) {
        if !(self.orphan && a == self.acct) {
            pool_charge(a, d);
        }
    }

    /// C8 move 3c: the account's backed room moved by `d` (as `pool`: not for an orphan's own reservation).
    #[inline]
    fn backs(&self, a: Acct, d: i64) {
        if !(self.orphan && a == self.acct) {
            pool_back(a, d);
        }
    }

    /// The backed room cut to what the reservation still promises (a shrink, a commit from the room).
    #[inline]
    fn fit_backed(&mut self) {
        if self.backed > self.reserved {
            let d = self.backed - self.reserved;
            self.backs(self.acct, -Self::signed(d));
            CELLS[self.cell as usize].backed.fetch_sub(Self::signed(d), Relaxed);
            self.backed = self.reserved;
        }
    }

    #[inline]
    fn signed(n: u64) -> i64 {
        n.min(i64::MAX as u64) as i64
    }

    /// The cell's committed bytes moved by `d` (door 2's in its `mapped` too); an ended operator's cell frees once empty.
    #[inline]
    fn cell_commit(&self, cell: &OwnerCell, d: i64) {
        if d == 0 {
            return;
        }
        cell.committed.fetch_add(d, Relaxed);
        if self.door == Door::Mapped {
            cell.mapped.fetch_add(d, Relaxed);
        }
        if d < 0 {
            op_drained(self.cell);
        }
    }

    /// `n` bytes on (or, negative, off) the owner's account as the door says: door 2's pages with their kind, door 1's kind
    /// lane only (the allocator counts the bytes themselves), the pool's with their kind among the account's pool bytes.
    #[inline]
    fn book(&self, a: Acct, n: i64) {
        match self.door {
            Door::Mapped => post(a, n, n, self.kind, n),
            Door::Heap => post(a, 0, 0, self.kind, n),
            Door::Pool => post_q(a, n, 0, n, self.kind, n),
        }
    }

    /// `n` more pages open: from the reservation first, past it the claim grows. Door 1: given-back pages come back
    /// first; the rest must be a block the caller just allocated, which the allocator counted on the calling thread's
    /// account: they move to the owner's.
    pub fn commit(&mut self, n: u64) {
        if n == 0 {
            return;
        }
        let cell = self.cell();
        let a = self.acct;
        let from_res = n.min(self.reserved);
        let b = Self::signed(n);
        if self.door == Door::Heap {
            let back = n.min(self.given);
            if back > 0 {
                self.given -= back;
                GIVEN.fetch_sub(Self::signed(back), Relaxed);
                post(a, Self::signed(back), 0, Kind::Region, 0);
            }
            let fresh = Self::signed(n - back);
            let cur = current();
            if fresh > 0 && cur != a {
                post(cur, -fresh, 0, Kind::Region, 0);
                post(a, fresh, 0, Kind::Region, 0);
            }
        }
        self.book(a, b);
        if from_res > 0 {
            self.pool(a, -Self::signed(from_res));
        }
        self.reserved -= from_res;
        self.fit_backed();
        self.committed += n;
        if from_res > 0 {
            cell.reserved.fetch_sub(Self::signed(from_res), Relaxed);
        }
        self.cell_commit(cell, b);
    }

    /// `n` committed pages given back with no room kept for them (an allocator region's tail advised away, a mapping's pages
    /// closed again): off the account and the claim; door 1 remembers them as given back, so a later `commit` takes them
    /// first. `uncommit` keeps the room.
    pub fn give(&mut self, n: u64) {
        let n = n.min(self.committed);
        if n == 0 {
            return;
        }
        let cell = self.cell();
        let a = self.acct;
        let b = Self::signed(n);
        self.book(a, -b);
        if self.door == Door::Heap {
            self.given += n;
            GIVEN.fetch_add(b, Relaxed);
            post(a, -b, 0, Kind::Region, 0);
        }
        self.committed -= n;
        self.cell_commit(cell, -b);
    }

    /// `n` committed pages given back (made inaccessible, advised away): off the account, back into the reservation (the
    /// claim is unchanged).
    pub fn uncommit(&mut self, n: u64) {
        let n = n.min(self.committed);
        if n == 0 {
            return;
        }
        let cell = self.cell();
        let a = self.acct;
        let b = Self::signed(n);
        self.book(a, -b);
        if self.door == Door::Heap {
            self.given += n;
            GIVEN.fetch_add(b, Relaxed);
            post(a, -b, 0, Kind::Region, 0);
        }
        self.pool(a, b);
        self.committed -= n;
        self.reserved += n;
        cell.reserved.fetch_add(b, Relaxed);
        self.cell_commit(cell, -b);
    }

    /// Door 1: up to `n` committed bytes the caller frees now, on this thread, while the charge lives (a cache's contents
    /// evicted, a staged image handed on): they move from the owner's account to this thread's, where the free lands (the
    /// drop's step for `n` bytes). The reservation is untouched. A mapping's given-back pages are `uncommit`, its unmap the
    /// drop.
    pub fn free(&mut self, n: u64) {
        debug_assert!(self.door == Door::Heap, "a mapping's pages leave by uncommit or the drop");
        let n = n.min(self.committed);
        if n == 0 {
            return;
        }
        let cell = self.cell();
        let a = self.acct;
        let b = Self::signed(n);
        self.book(a, -b);
        if self.door == Door::Heap {
            let cur = current();
            if cur != a {
                post(a, -b, 0, Kind::Region, 0);
                post(cur, b, 0, Kind::Region, 0);
            }
        }
        self.committed -= n;
        self.cell_commit(cell, -b);
    }

    /// `n` more promised room (a draw past the reservation, a lend taken back).
    pub fn grow(&mut self, n: u64) {
        if n == 0 {
            return;
        }
        let cell = self.cell();
        let b = Self::signed(n);
        self.pool(self.acct, b);
        self.reserved += n;
        cell.reserved.fetch_add(b, Relaxed);
    }

    /// Up to `n` of the promised room given back (a lend, a draw returned): the unbacked room first, then the backed.
    pub fn shrink(&mut self, n: u64) {
        let n = n.min(self.reserved);
        if n == 0 {
            return;
        }
        let cell = self.cell();
        let b = Self::signed(n);
        self.pool(self.acct, -b);
        self.reserved -= n;
        self.fit_backed();
        cell.reserved.fetch_sub(b, Relaxed);
        op_drained(self.cell);
    }

    /// C8 move 3c: up to `n` of the promised room now backed by pages a draw inside it committed: the claim is unchanged,
    /// the account's unbacked room (the views' `pool_reserved_bytes`) drops by them, so a drawn byte counts once.
    pub fn back(&mut self, n: u64) {
        let n = n.min(self.reserved - self.backed);
        if n == 0 {
            return;
        }
        let cell = self.cell();
        self.backs(self.acct, Self::signed(n));
        cell.backed.fetch_add(Self::signed(n), Relaxed);
        self.backed += n;
    }

    /// C8 move 3c: up to `n` backed bytes unbacked again (the draw given back, settled or moved off its owner).
    pub fn unback(&mut self, n: u64) {
        let n = n.min(self.backed);
        if n == 0 {
            return;
        }
        let cell = self.cell();
        self.backs(self.acct, -Self::signed(n));
        cell.backed.fetch_sub(Self::signed(n), Relaxed);
        self.backed -= n;
    }

    /// THE owner change: the charge's committed bytes (with their kind and door), its reservation and its uses move from
    /// its owner to `to` in one step, released at `release` from now on.
    pub fn transfer(&mut self, to: Owner, release: Release) {
        let old = self.cell();
        let (a1, a2) = (self.acct, to.acct());
        let (c, r, u) = (Self::signed(self.committed), Self::signed(self.reserved), self.uses as i64);
        let (cell, key, skey) = cell_of(to);
        let a2 = if past_session(cell) && a2.is_backend() { Acct::UNBOUND } else { a2 };
        if a1 != a2 {
            if c != 0 {
                let m = if self.door == Door::Mapped { c } else { 0 };
                let q = if self.door == Door::Pool { c } else { 0 };
                post_q(a1, -c, -m, -q, self.kind, -c);
                post_q(a2, c, m, q, self.kind, c);
            }
            if r != 0 {
                self.pool(a1, -r);
                pool_charge(a2, r);
            }
            if self.backed != 0 {
                let k = Self::signed(self.backed);
                self.backs(a1, -k);
                pool_back(a2, k);
            }
            self.orphan = false;
        }
        let (m, k) = (if self.door == Door::Mapped { c } else { 0 }, Self::signed(self.backed));
        // D4554: only the counters that hold something move (a part use moves its one use, a heap charge its committed
        // bytes): an add of 0 is a locked add on the cell's line all the same, and a backend moves thousands of kept
        // cursors' charges at every statement's bind and end
        let shift = |cell: &OwnerCell, s: i64| {
            if c != 0 {
                cell.committed.fetch_add(s * c, Relaxed);
            }
            if r != 0 {
                cell.reserved.fetch_add(s * r, Relaxed);
            }
            if u != 0 {
                cell.uses.fetch_add(s * u, Relaxed);
            }
            if m != 0 {
                cell.mapped.fetch_add(s * m, Relaxed);
            }
            if k != 0 {
                cell.backed.fetch_add(s * k, Relaxed);
            }
        };
        shift(old, -1);
        op_drained(self.cell);
        (self.owner, self.acct, self.release, self.cell, self.key, self.skey) = (to, a2, release, cell, key, skey);
        shift(&CELLS[cell as usize], 1);
    }

    pub fn owner(&self) -> Owner {
        self.owner
    }

    pub fn release(&self) -> Release {
        self.release
    }

    pub fn committed(&self) -> u64 {
        self.committed
    }

    /// CACHE-CHARGE: `other`'s committed bytes taken over by this charge (door 1 both: one claim from now on, its drop
    /// gives them back), moved to this charge's owner first when `other` has another (`transfer`), to its kind lane when
    /// another. `other`'s reservation, uses and given-back pages stay with it (they leave at its drop). Returns the bytes
    /// taken over.
    pub fn absorb(&mut self, mut other: Charge) -> u64 {
        debug_assert!(self.door == Door::Heap && other.door == Door::Heap, "absorb joins door-1 charges");
        let c = other.committed;
        if c == 0 || self.door != Door::Heap || other.door != Door::Heap {
            return 0;
        }
        if other.owner != self.owner || other.acct != self.acct || other.cell != self.cell {
            other.transfer(self.owner, self.release);
        }
        if other.kind != self.kind {
            other.book(other.acct, -Self::signed(c));
            self.book(self.acct, Self::signed(c));
        }
        // the same cell and account now: the bytes change claims, not totals
        other.committed = 0;
        self.committed += c;
        c
    }
    /// D4834: up to `n` of this charge's committed bytes as a charge of their own, [`Self::absorb`]'s reverse (door 1 both:
    /// the same owner, account, kind, cell and release; the bytes change claims, not totals), given back at its drop. Past
    /// the committed bytes it takes what there is.
    pub fn split_off(&mut self, n: u64) -> Charge {
        debug_assert!(self.door == Door::Heap, "split_off parts door-1 charges");
        let n = n.min(self.committed);
        self.committed -= n;
        Charge {
            owner: self.owner,
            acct: self.acct,
            release: self.release,
            kind: self.kind,
            door: self.door,
            cell: self.cell,
            key: self.key,
            skey: self.skey,
            committed: n,
            reserved: 0,
            backed: 0,
            given: 0,
            uses: 0,
            orphan: self.orphan,
        }
    }

    /// CACHE-CHARGE: the committed bytes brought to `n` on this thread (door 1): a commit of what is missing (a block this
    /// thread allocated), a free of the rest (bytes this thread frees).
    pub fn fit_committed(&mut self, n: u64) {
        if n > self.committed {
            self.commit(n - self.committed);
        } else if n < self.committed {
            self.free(self.committed - n);
        }
    }

    pub fn reserved(&self) -> u64 {
        self.reserved
    }

    /// C8 move 3c: of `reserved`, the bytes backed (`back`).
    pub fn backed(&self) -> u64 {
        self.backed
    }

    pub fn uses(&self) -> u32 {
        self.uses
    }

    /// The account its bytes are on now (its owner's; the unbound account once its session ended under it).
    pub fn acct(&self) -> Acct {
        self.acct
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// Design A: where a range listed for this charge posts its not-resident bytes: the cell its bytes book on now, followed
    /// as the charge follows it (`cell`) when its owner ends.
    pub fn nr_home(&self) -> NrHome {
        // a session's own charge carries its session's generation as its key (`locate`), as `cell` reads it
        let skey = if self.skey == 0 && matches!(self.owner, Owner::Session(_)) { self.key } else { self.skey };
        NrHome { cell: self.cell, key: self.key, skey, acct: self.acct }
    }
}

/// LEDGER2 design A: the owner cell a listed range's not-resident bytes book on (the residency registry keeps one per
/// range, from its charge's `nr_home`): its charge's cell, key and session key, so an owner that ended sends them where
/// its charges went (its session's, an exit cell, the orphan's). `NONE`: a range no charge owns (they stay on its account
/// alone).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NrHome {
    cell: u32,
    key: u64,
    skey: u64,
    acct: Acct,
}

impl NrHome {
    pub const NONE: NrHome = NrHome { cell: u32::MAX, key: 0, skey: 0, acct: Acct::UNBOUND };

    /// `d` more not-resident bytes on the home's cell (fewer, negative), followed first as `Charge::cell` follows an ended
    /// owner; an ended operator's cell frees once its last such byte leaves. No allocation, no lock: the registry calls it
    /// under its own.
    pub fn post(&mut self, d: i64) {
        if d == 0 || self.cell == u32::MAX {
            return;
        }
        if self.key != 0 && CELLS[self.cell as usize].key.load(Relaxed) != self.key {
            (self.cell, self.key, self.skey) = redirect(self.acct, self.skey);
        }
        CELLS[self.cell as usize].nr.fetch_add(d, Relaxed);
        if d < 0 {
            op_drained(self.cell);
        }
    }
}

impl Drop for Charge {
    /// Discharges both sides. Door 2's pages leave the owner's account; door 1's block is about to be freed by the caller
    /// on the calling thread, so its bytes (given-back pages included) move to that thread's account, where the free lands.
    fn drop(&mut self) {
        let (c, r, u, g) = (Self::signed(self.committed), Self::signed(self.reserved), self.uses as i64, Self::signed(self.given));
        if c == 0 && r == 0 && u == 0 && g == 0 {
            return;
        }
        let cell = self.cell();
        let a = self.acct;
        self.book(a, -c);
        if self.door == Door::Heap && c + g != 0 {
            if g != 0 {
                GIVEN.fetch_sub(g, Relaxed);
                post(a, g, 0, Kind::Region, 0);
            }
            let cur = current();
            if cur != a {
                post(a, -(c + g), 0, Kind::Region, 0);
                post(cur, c + g, 0, Kind::Region, 0);
            }
        }
        if r != 0 {
            self.pool(a, -r);
        }
        if self.backed != 0 {
            self.backs(a, -Self::signed(self.backed));
            cell.backed.fetch_sub(Self::signed(self.backed), Relaxed);
        }
        // D4709: only the counters that hold something move (`transfer`'s rule): a use's drop is one add, not three
        if r != 0 {
            cell.reserved.fetch_sub(r, Relaxed);
        }
        if u != 0 {
            cell.uses.fetch_sub(u, Relaxed);
        }
        self.cell_commit(cell, -c);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{account_row, bind_backend, door_heap, fold, running_statement, statement_begin, statement_end, unbind_backend};

    const MIB: u64 = 1 << 20;
    const M: i64 = MIB as i64;

    /// Each test on its own thread and backend slot (3900..3909: no other test's).
    fn on(procno: u32, f: impl FnOnce(Acct) + Send + 'static) {
        std::thread::spawn(move || {
            bind_backend(procno, 8000 + procno);
            f(Acct::backend(procno));
            unbind_backend();
        })
        .join()
        .unwrap();
    }

    /// Account `a`'s (net, mapped, reserved), folded.
    fn nums(a: Acct) -> (i64, i64, i64) {
        fold();
        let r = account_row(a);
        (r.account_bytes, r.mapped_bytes, r.pool_reserved_bytes)
    }

    fn row(committed: i64, reserved: i64, uses: i64) -> OwnerRow {
        OwnerRow { committed, reserved, uses }
    }

    #[test]
    fn a_reservation_is_room_and_a_commit_moves_it_to_pages() {
        let _unbound = crate::unbound_shared(); // D4356: this test binds a backend, so it may move UNBOUND's books
        on(3900, |a| {
            statement_begin();
            let (o, rel) = context();
            assert_eq!((o, rel), (Owner::Statement { acct: a, stmt: running_statement(a) }, Release::Statement));
            let b = nums(a);
            let mut c = Charge::reserve(o, rel, Kind::HashAgg, Door::Mapped, 10 * MIB);
            assert_eq!(nums(a), (b.0, b.1, b.2 + 10 * M), "room only: never in net");
            assert_eq!(owner_row(o), row(0, 10 * M, 0));
            c.commit(4 * MIB);
            assert_eq!(nums(a), (b.0 + 4 * M, b.1 + 4 * M, b.2 + 6 * M), "no byte counted twice");
            c.commit(8 * MIB);
            assert_eq!(nums(a), (b.0 + 12 * M, b.1 + 12 * M, b.2), "6 from the reservation, 2 past it");
            assert_eq!(owner_row(o), row(12 * M, 0, 0));
            c.uncommit(2 * MIB);
            assert_eq!(nums(a), (b.0 + 10 * M, b.1 + 10 * M, b.2 + 2 * M), "given back: room again");
            c.shrink(MIB);
            c.grow(3 * MIB);
            assert_eq!(owner_row(o), row(10 * M, 4 * M, 0));
            drop(c);
            assert_eq!((nums(a), owner_row(o)), (b, OwnerRow::default()));
            statement_end();
        });
    }

    #[test]
    fn a_transfer_moves_pages_and_room_once_and_a_use_stays_with_its_owner() {
        let _unbound = crate::unbound_shared(); // D4356: this test binds a backend, so it may move UNBOUND's books
        on(3901, |a| {
            statement_begin();
            let (o, rel) = context();
            let (cache, ca) = (Owner::Cache(Holder::Of(Consumer::ZoneStore)), Acct::of(Consumer::ZoneStore));
            let (b, k) = (nums(a), nums(ca));
            let mut c = Charge::reserve(o, rel, Kind::Sort, Door::Mapped, 6 * MIB);
            c.commit(4 * MIB);
            let u = Charge::use_of(o, Release::Row);
            c.transfer(cache, Release::Evict);
            assert_eq!(nums(a), b, "nothing of it left on the statement's account");
            assert_eq!(nums(ca), (k.0 + 4 * M, k.1 + 4 * M, k.2 + 2 * M));
            assert_eq!((owner_row(o), owner_row(cache)), (row(0, 0, 1), row(4 * M, 2 * M, 0)));
            assert_eq!((c.owner(), c.release()), (cache, Release::Evict));
            drop(u);
            drop(c);
            assert_eq!((nums(ca), owner_row(o), owner_row(cache)), (k, OwnerRow::default(), OwnerRow::default()));
            statement_end();
        });
    }

    #[test]
    fn a_heap_block_moves_to_its_owner_and_back_to_the_thread_that_frees_it() {
        let _unbound = crate::unbound_shared(); // D4356: this test binds a backend, so it may move UNBOUND's books
        on(3902, |a| {
            statement_begin();
            let (cache, ca) = (Owner::Cache(Holder::Of(Consumer::BuildPool)), Acct::of(Consumer::BuildPool));
            let (b, k) = (nums(a).0, nums(ca).0);
            door_heap(8 << 20); // the block, allocated on the backend's thread
            let mut c = Charge::open(cache, Release::Evict, Kind::Region, Door::Heap, 8 * MIB);
            assert_eq!((nums(a).0, nums(ca).0), (b, k + 8 * M), "the cache's bytes, not the opener's (D4084)");
            c.uncommit(2 * MIB);
            assert_eq!(nums(ca).0, k + 6 * M, "pages given back leave the account");
            c.commit(MIB);
            assert_eq!(nums(ca).0, k + 7 * M, "a page touched again comes back");
            drop(c);
            door_heap(-(8 << 20)); // its free, on the dropping thread
            assert_eq!((nums(a).0, nums(ca).0), (b, k));
            statement_end();
        });
    }

    #[test]
    fn a_heap_block_freed_while_its_charge_lives_leaves_from_the_thread_that_frees_it() {
        let _unbound = crate::unbound_shared(); // D4356: this test binds a backend, so it may move UNBOUND's books
        let (cache, ca) = (Owner::Cache(Holder::Of(Consumer::ProgramCaches)), Acct::of(Consumer::ProgramCaches));
        let held = std::sync::Arc::new(std::sync::Mutex::new(None::<(Charge, i64)>));
        let h = held.clone();
        on(3907, move |a| {
            let (b, k) = (nums(a).0, nums(ca).0);
            door_heap(8 << 20);
            let c = Charge::open(cache, Release::Evict, Kind::Region, Door::Heap, 8 * MIB);
            assert_eq!((nums(a).0, nums(ca).0), (b, k + 8 * M), "the opener nets zero");
            *h.lock().unwrap() = Some((c, k));
        });
        on(3908, move |a| {
            let (mut c, k) = held.lock().unwrap().take().unwrap();
            let (b, r) = (nums(a).0, owner_row(cache).committed);
            c.free(3 * MIB);
            door_heap(-(3 << 20)); // the evicted bytes' free, on this thread
            assert_eq!((nums(a).0, nums(ca).0, c.committed()), (b, k + 5 * M, 5 * MIB), "the freeing thread nets zero");
            assert_eq!(owner_row(cache).committed, r - 3 * M, "the owner's cell");
            c.free(8 * MIB);
            assert_eq!((nums(a).0, c.committed()), (b + 5 * M, 0), "at most what is committed");
            door_heap(-(5 << 20));
            drop(c);
            assert_eq!((nums(a).0, nums(ca).0), (b, k));
        });
    }

    #[test]
    fn a_charge_that_outlives_its_statement_is_its_sessions_residue() {
        let _unbound = crate::unbound_shared(); // D4356: this test binds a backend, so it may move UNBOUND's books
        on(3903, |a| {
            statement_begin();
            let (o, rel) = context();
            let b = nums(a);
            let mut c = Charge::open(o, rel, Kind::Spool, Door::Mapped, 3 * MIB);
            let r0 = counters().2[OwnerClass::Statement as usize];
            statement_end();
            assert!(counters().2[OwnerClass::Statement as usize] > r0, "the statement's end found a residue");
            assert_eq!((owner_row(o), owner_row(Owner::Session(a))), (OwnerRow::default(), row(3 * M, 0, 0)));
            statement_begin();
            assert_eq!(owner_row(context().0), OwnerRow::default(), "the next statement starts clean");
            c.commit(MIB);
            assert_eq!(owner_row(Owner::Session(a)), row(4 * M, 0, 0), "a stale charge books on its session");
            drop(c);
            assert_eq!((nums(a), owner_row(Owner::Session(a))), (b, OwnerRow::default()));
            statement_end();
        });
    }

    #[test]
    fn a_portal_owns_what_its_execute_makes_and_its_end_finds_a_use_left() {
        let _unbound = crate::unbound_shared(); // D4356: this test binds a backend, so it may move UNBOUND's books
        on(3904, |a| {
            statement_begin();
            portal_begin(77);
            let u = {
                let _p = enter_portal(77);
                let (o, rel) = context();
                assert_eq!((o, rel), (Owner::Portal { acct: a, portal: 77 }, Release::Portal));
                Charge::use_of(o, rel)
            };
            assert_eq!(context().0, Owner::Statement { acct: a, stmt: running_statement(a) }, "outside the Execute");
            statement_end();
            assert_eq!(owner_row(Owner::Session(a)), OwnerRow::default(), "a suspended portal's use is not the statement's");
            let r = portal_end(a, 77).expect("a use left at the portal's end");
            assert_eq!((r.class, r.uses, r.committed), (OwnerClass::Portal, 1, 0));
            assert_eq!(owner_row(Owner::Session(a)), row(0, 0, 1));
            drop(u);
            assert_eq!(owner_row(Owner::Session(a)), OwnerRow::default());
            assert_eq!(portal_end(a, 77), None, "ended once");
        });
    }

    #[test]
    fn a_charge_that_outlives_its_session_leaves_unbound_not_the_next_backend() {
        let _unbound = crate::unbound_shared(); // D4356: this test binds a backend, so it may move UNBOUND's books
        let held = std::thread::spawn(|| {
            bind_backend(3905, 1);
            let a = Acct::backend(3905);
            let c = Charge::open(Owner::Session(a), Release::Session, Kind::Mapped, Door::Mapped, 5 * MIB);
            unbind_backend();
            c
        })
        .join()
        .unwrap();
        on(3905, move |a| {
            let n = nums(a);
            assert_eq!(owner_row(Owner::Session(a)), OwnerRow::default(), "a new session starts at zero");
            drop(held);
            assert_eq!(nums(a), n, "the old session's bytes left UNBOUND, where its unbind put them");
        });
    }

    #[test]
    fn a_worker_set_moves_its_bytes_to_the_depot_and_ends_empty() {
        let _unbound = crate::unbound_shared(); // D4356: this test binds a backend, so it may move UNBOUND's books
        on(3906, |a| {
            worker_set_begin(3906, a);
            let ws = Owner::WorkerSet(3906);
            assert_eq!(ws.acct(), a);
            let mut c = Charge::open(ws, Release::Session, Kind::Arena, Door::Mapped, MIB);
            assert_eq!(owner_row(ws), row(M, 0, 0));
            c.transfer(Owner::Cache(Holder::Parked), Release::Evict);
            assert_eq!((owner_row(ws), worker_set_end(3906)), (OwnerRow::default(), None));
            drop(c);
        });
    }

    #[test]
    fn a_thread_with_no_backend_charges_its_accounts_holder() {
        std::thread::spawn(|| {
            assert_eq!(context(), (Owner::Cache(Holder::Unbound), Release::Evict));
            let _in = enter_as(Owner::Cache(Holder::Of(Consumer::CondCache)));
            assert_eq!(context(), (Owner::Cache(Holder::Of(Consumer::CondCache)), Release::Evict));
        })
        .join()
        .unwrap();
        assert_eq!([Holder::Parked, Holder::Reaping, Holder::Unbound, Holder::SlotDepot].map(|h| Holder::of_acct(h.acct())), [Holder::Parked, Holder::Reaping, Holder::Unbound, Holder::SlotDepot]);
        for c in Consumer::ALL {
            assert_eq!(Holder::of_acct(Acct::of(c)), Holder::Of(c));
        }
    }

    #[test]
    fn a_give_takes_pages_off_the_account_and_the_claim_and_a_commit_takes_them_back() {
        let _unbound = crate::unbound_shared(); // D4356: this test binds a backend, so it may move UNBOUND's books
        on(3911, |a| {
            let s = Owner::Session(a);
            // door 2: a mapping's pages closed again, then opened again
            let mut m = Charge::open(s, Release::Session, Kind::Region, Door::Mapped, 8 * MIB);
            assert_eq!(nums(a), (8 * M, 8 * M, 0));
            m.give(3 * MIB);
            assert_eq!((nums(a), owner_row(s)), ((5 * M, 5 * M, 0), row(5 * M, 0, 0)), "off the account; no room kept");
            m.give(9 * MIB);
            assert_eq!(nums(a), (0, 0, 0), "at most what it holds");
            m.commit(2 * MIB);
            assert_eq!((nums(a), m.committed()), ((2 * M, 2 * M, 0), 2 * MIB));
            drop(m);
            assert_eq!((nums(a), owner_row(s)), ((0, 0, 0), OwnerRow::default()));
            // door 1: an allocator region's tail given back (the allocator still counts the block), then taken again
            let base = nums(a).0;
            let mut h = Charge::open(s, Release::Session, Kind::Region, Door::Heap, 4 * MIB);
            assert_eq!(nums(a).0, base, "the allocator counted the block on this thread: no move");
            h.give(MIB);
            assert_eq!((nums(a).0, owner_row(s)), (base - M, row(3 * M, 0, 0)));
            h.commit(MIB);
            assert_eq!((nums(a).0, owner_row(s)), (base, row(4 * M, 0, 0)), "the given-back pages come back first");
            h.give(MIB);
            drop(h);
            assert_eq!((nums(a).0, owner_row(s)), (base, OwnerRow::default()), "the drop takes the given-back pages back for the free");
        });
    }

    #[test]
    fn c8_3c_a_draw_backs_its_room_so_the_account_counts_a_drawn_byte_once() {
        let _unbound = crate::unbound_shared(); // D4356: this test binds a backend, so it may move UNBOUND's books
        on(3912, |a| {
            let s = Owner::Session(a);
            let claim = |a: Acct| {
                let r = account_row(a);
                (r.pool_reserved_bytes, r.pool_backed_bytes)
            };
            let mut room = Charge::reserve(s, Release::Session, Kind::Region, Door::Heap, 64 * MIB);
            assert_eq!(claim(a), (64 * M, 0));
            // a 16 MiB region drawn inside the room: its pages go on the account (door 2 here), its room is backed
            let pages = Charge::open(s, Release::Session, Kind::Region, Door::Mapped, 16 * MIB);
            room.back(16 * MIB);
            assert_eq!((claim(a), nums(a).0, crate::pool_reserved(a)), ((48 * M, 16 * M), 16 * M, 64 * M), "pages + unbacked room = the claim; the claim unchanged");
            assert_eq!(owner_row(s), row(16 * M, 64 * M, 0), "the owner's cell keeps the whole promise");
            room.back(64 * MIB);
            assert_eq!(claim(a), (0, 64 * M), "at most the room");
            room.unback(48 * MIB);
            assert_eq!(claim(a), (48 * M, 16 * M));
            // a lend gives back unbacked room only while some is left; past it the backed part goes too
            room.shrink(56 * MIB);
            assert_eq!((claim(a), room.reserved(), room.backed()), ((0, 8 * M), 8 * MIB, 8 * MIB));
            // a transfer takes the backed part with it
            let p = Owner::Cache(Holder::Parked);
            room.transfer(p, Release::Evict);
            assert_eq!(claim(a), (0, 0));
            assert!(claim(Acct::PARKED).1 >= 8 * M, "on the parked holder's account now");
            room.transfer(s, Release::Session);
            assert_eq!(claim(a), (0, 8 * M));
            drop(pages);
            drop(room);
            assert_eq!((claim(a), owner_row(s)), ((0, 0), OwnerRow::default()), "the drop takes the backed part off too");
        });
    }

    /// The door table is the process's: its two tests take turns (the overflow one fills it).
    static DOOR_TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// C8 C1: a mapped block's owner is the map's context owner, remembered by address: a resize under another statement
    /// and the free move that owner's numbers (its statement ended: its session's), never the resizing or freeing one's;
    /// a block mapped at the old address while it moves is a new entry, and a failed remap leaves the block where it was.
    #[test]
    fn a_mapped_block_moves_its_owners_numbers_wherever_it_is_resized_and_freed() {
        let _unbound = crate::unbound_shared(); // D4356: this test binds a backend, so it may move UNBOUND's books
        let _turn = DOOR_TURN.lock().unwrap_or_else(|e| e.into_inner());
        on(3913, |a| {
            statement_begin();
            let x = Owner::Statement { acct: a, stmt: crate::running_statement(a) };
            let (p, q) = (0x7f13_0000_0000usize, 0x7f13_4000_0000usize);
            let n0 = nums(a).0;
            door_mapped(p, 4 * MIB, true);
            assert_eq!((owner_row(x).committed, nums(a).0 - n0), (4 * M, 4 * M), "the mapping statement's, on its account");
            statement_end();
            statement_begin();
            let (y, s) = (Owner::Statement { acct: a, stmt: crate::running_statement(a) }, Owner::Session(a));
            door_moving(p, true);
            door_remapped(p, p, 4 * MIB, 6 * MIB);
            assert_eq!((owner_row(y).committed, owner_row(s).committed), (0, 6 * M), "grown in place: the session's (X ended)");
            door_moving(p, true);
            door_moving(p, false);
            door_moving(p, true);
            door_mapped(p, 2 * MIB, true);
            assert_eq!((owner_row(y).committed, owner_row(s).committed), (2 * M, 6 * M), "a block mapped at the old address is Y's");
            door_remapped(p, q, 6 * MIB, 8 * MIB);
            assert_eq!((owner_row(y).committed, owner_row(s).committed), (2 * M, 8 * M), "moved: its entry follows it");
            door_mapped(p, 2 * MIB, false);
            door_mapped(q, 8 * MIB, false);
            assert_eq!((owner_row(y).committed, owner_row(s).committed, nums(a).0 - n0), (0, 0, 0), "the frees take them off");
            assert_eq!(door_find(p).or(door_find(p | DOOR_MOVING)).or(door_find(q)), None, "no entry is left");
            statement_end();
        });
    }

    /// C8 C1: past the table's room a block is counted on the account only (as before C8) and in `door_overflow`: no owner
    /// cell holds what its free could not find, so no end reads a residue the blocks did not leave.
    #[test]
    fn mapped_blocks_past_the_table_count_the_account_only() {
        let _unbound = crate::unbound_shared(); // D4356: this test binds a backend, so it may move UNBOUND's books
        let _turn = DOOR_TURN.lock().unwrap_or_else(|e| e.into_inner());
        on(3914, |a| {
            statement_begin();
            let x = Owner::Statement { acct: a, stmt: crate::running_statement(a) };
            let (n0, o0, b0) = (nums(a).0, door_overflow(), door_blocks());
            let n = DOOR_BLOCKS + 16;
            let ptr = |k: usize| 0x5a00_0000_0000usize + (k << 21);
            for k in 0..n {
                door_mapped(ptr(k), 4096, true);
            }
            let over = (door_overflow() - o0) as usize;
            let kept = door_blocks() - b0;
            assert!(over >= 16 && kept + over == n, "{kept} remembered + {over} past the table = {n}");
            assert_eq!((owner_row(x).committed, nums(a).0 - n0), (kept as i64 * 4096, n as i64 * 4096), "the account has all");
            for k in 0..n {
                door_mapped(ptr(k), 4096, false);
            }
            assert_eq!((owner_row(x).committed, nums(a).0 - n0, door_blocks() - b0), (0, 0, 0), "every free found its owner or none");
            statement_end();
            let mut left = 0;
            crate::owner_end::for_each_recent(|r| left += usize::from(r.acct == a));
            assert_eq!(left, 0, "no end of this backend read a residue");
        });
    }

    /// C8 C1 + checker 1 (B2): a session's mapped block that its teardown frees (mcxt_stats' run_session_teardown, after
    /// ProcKill's unbind and before the drain) gives its numbers back to the session's exit cell, on UNBOUND where the
    /// unbind moved the session's bytes, as a session's charge does: the session ends with no residue. Red before B2 (the
    /// unbind noted the block as the session's residue) and on B2 without the door's session key (the free reached the
    /// orphan cell and the drain read the block).
    #[test]
    fn a_sessions_mapped_block_its_teardown_frees_is_no_residue() {
        let _unbound = crate::unbound_shared(); // D4356: this test binds a backend, so it may move UNBOUND's books
        let _turn = DOOR_TURN.lock().unwrap_or_else(|e| e.into_inner());
        let a = Acct::backend(3923);
        let p = 0x7f23_0000_0000usize;
        let ends = move || {
            let mut n = 0;
            crate::owner_end::for_each_recent(|r| n += usize::from(r.acct == a));
            n
        };
        let (mapped, at_unbind, moved) = std::thread::spawn(move || {
            bind_backend(3923, 1);
            assert_eq!(context().0, Owner::Session(a), "no statement runs: the session maps it");
            door_mapped(p, 4 * MIB, true);
            let mapped = owner_row(Owner::Session(a)).committed;
            unbind_backend();
            let (at_unbind, n0) = (ends(), nums(a).0);
            // the teardown frees the session's context and its mapped block
            door_mapped(p, 4 * MIB, false);
            (mapped, at_unbind, nums(a).0 - n0)
        })
        .join()
        .unwrap();
        assert_eq!((mapped, at_unbind), (4 * M, 0), "the session's block; the unbind is not the session's end");
        assert_eq!(moved, 0, "the free books UNBOUND, where the unbind moved the session's bytes");
        assert_eq!(ends(), 0, "the teardown freed it: the session's end reads no residue");
        assert_eq!(door_find(p), None, "its entry is freed");
    }

    /// D3204: `holder_name` names who holds a use keyed (account, owner key) now: the running statement, an open portal, a
    /// worker set, the session, the depot's parked sets, another cache, or an owner that ended.
    #[test]
    fn a_uses_holder_is_named_by_its_key() {
        let _unbound = crate::unbound_shared(); // D4356: this test binds a backend, so it may move UNBOUND's books
        on(3925, |a| {
            assert_eq!(holder_name(a, 0), "session", "a backend's own key: its session (a kept cursor)");
            assert_eq!(holder_name(Acct::PARKED, 0), "parked");
            statement_begin();
            let stmt = crate::running_statement(a);
            assert_eq!(holder_name(a, stmt), "statement");
            let p = new_owner_id();
            portal_begin(p);
            assert_eq!(holder_name(a, p), "portal");
            let _ = portal_end(a, p);
            assert_eq!(holder_name(a, p), "ended", "a closed portal's key");
            statement_end();
            assert_eq!(holder_name(a, stmt), "ended", "an ended statement's key");
            assert_eq!(holder_name(a, Owner::WorkerSet(3925).key()), "worker_set");
        });
    }

    fn keyed(o: Owner) -> bool {
        let (c, k, _) = locate(o);
        is_op(c) && live(o, c, k)
    }

    /// LEDGER2 step 1: an operator and its worker arena book on their own cells; the statement's subtree sums them, the
    /// session's sums everything on the backend; an operator's share is its budget and an ended one frees with its last
    /// byte.
    #[test]
    fn ledger2_an_operators_bytes_sum_into_its_statements_subtree() {
        let _unbound = crate::unbound_shared(); // D4356: this test binds a backend, so it may move UNBOUND's books
        on(3930, |a| {
            statement_begin();
            let st = context().0;
            let op = op_begin(st, Kind::Sort as u32, 64 * MIB);
            let wa = op_begin(op, Kind::Sort as u32 | WORKER_ARENA, 0);
            assert!(keyed(op) && keyed(wa), "both begun on their own cells");
            let n0 = nums(a);
            let s = Charge::open(st, Release::Statement, Kind::Region, Door::Mapped, MIB);
            let o = Charge::open(op, Release::Statement, Kind::Sort, Door::Mapped, 4 * MIB);
            let mut w = Charge::open(wa, Release::Statement, Kind::Sort, Door::Mapped, 2 * MIB);
            assert_eq!(nums(a).0 - n0.0, 7 * M, "on the statement's account, each byte once");
            assert_eq!(usage(st), Usage { held: M, budget: 0, spilled: 0, subtree: 7 * M, subtree_spilled: 0, not_resident: 0, subtree_not_resident: 0 });
            assert_eq!(usage(op), Usage { held: 4 * M, budget: 64 * M, spilled: 0, subtree: 6 * M, subtree_spilled: 0, not_resident: 0, subtree_not_resident: 0 });
            assert_eq!(usage(wa).subtree, 2 * M);
            assert_eq!(usage(Owner::Session(a)).subtree, 7 * M, "the backend total");
            let mut rows = 0;
            for_each_owner(|v| {
                if v.acct == a && v.name == "worker_arena" {
                    rows += 1;
                    assert_eq!((v.held(), v.label & 0xff, v.parent), (2 * M, Kind::Sort as u32, locate(op).0 as u32));
                }
            });
            assert_eq!(rows, 1);
            spilled(wa, 3 * M);
            assert_eq!(usage(st).subtree_spilled, 3 * M);
            op_end(wa);
            assert!(keyed(wa), "ended with bytes on it: still keyed");
            assert_eq!(usage(st).subtree, 7 * M, "still in the subtree");
            op_end(op);
            drop(o);
            assert!(keyed(op), "its worker arena still keyed under it");
            w.give(2 * MIB);
            assert!(!keyed(wa) && !keyed(op), "the last byte left: both free, the child first");
            assert_eq!(usage(st), Usage { held: M, budget: 0, spilled: 3 * M, subtree: M, subtree_spilled: 3 * M, not_resident: 0, subtree_not_resident: 0 }, "the spill went up the chain");
            w.commit(MIB);
            assert_eq!(owner_row(Owner::Session(a)).committed, M, "a charge outliving its freed operator books on the session");
            drop(w);
            spilled(st, -3 * M);
            drop(s);
            statement_end();
            assert_eq!((owner_row(Owner::Session(a)), usage(Owner::Session(a)).spilled), (OwnerRow::default(), 0));
        });
    }

    /// LEDGER2 step 1: a statement that ends with an operator still keyed ends it too: the operator's bytes are a residue
    /// (class 'operator') that moves to the session, as the statement's own; a portal's end does the same for its operators.
    #[test]
    fn ledger2_an_operator_left_at_its_statements_end_is_a_residue_on_the_session() {
        let _unbound = crate::unbound_shared(); // D4356: this test binds a backend, so it may move UNBOUND's books
        on(3931, |a| {
            statement_begin();
            let st = context().0;
            let op = op_begin(st, Kind::HashAgg as u32, 0);
            let c = Charge::open(op, Release::Statement, Kind::HashAgg, Door::Mapped, 5 * MIB);
            let e0 = crate::owner_end::totals(crate::owner_end::Source::Owner, OwnerClass::Op).events;
            statement_end();
            assert_eq!(crate::owner_end::totals(crate::owner_end::Source::Owner, OwnerClass::Op).events, e0 + 1);
            assert!(!keyed(op), "its cell freed");
            assert_eq!(owner_row(Owner::Session(a)).committed, 5 * M);
            drop(c);
            assert_eq!(owner_row(Owner::Session(a)), OwnerRow::default());
            statement_begin();
            let p = new_owner_id();
            portal_begin(p);
            let po = Owner::Portal { acct: a, portal: p };
            let op = op_begin(po, Kind::Sort as u32, 0);
            let c = Charge::open(op, Release::Portal, Kind::Sort, Door::Mapped, MIB);
            assert_eq!(usage(po).subtree, M);
            statement_end();
            assert!(keyed(op), "a portal's operator outlives the statement");
            let _ = portal_end(a, p);
            assert!(!keyed(op));
            assert_eq!(owner_row(Owner::Session(a)).committed, M);
            drop(c);
        });
    }

    /// memlead#3276 (D3865 e1): a portal (or statement) whose own operators are all gone ends with no sweep of the operator
    /// table, while its statement's operators stay keyed (OPS_ON on the account); one with an operator still keyed sweeps
    /// it, and a chain's top counts a worker arena under an operator too.
    #[test]
    fn an_end_with_no_operator_of_its_own_keyed_sweeps_nothing() {
        let _unbound = crate::unbound_shared(); // D4356: this test binds a backend, so it may move UNBOUND's books
        on(3933, |a| {
            statement_begin();
            let st = context().0;
            let outer = op_begin(st, Kind::HashAgg as u32, 0);
            let (ci, _, _) = locate(st);
            assert_eq!(CELLS[ci].ops.load(Relaxed), 1, "the statement counts its operator");
            for _ in 0..3 {
                let p = new_owner_id();
                portal_begin(p);
                let po = Owner::Portal { acct: a, portal: p };
                let op = op_begin(po, Kind::Sort as u32, 0);
                let w = op_begin(op, Kind::Sort as u32 | WORKER_ARENA, 0);
                let (pc, _, _) = locate(po);
                assert_eq!(CELLS[pc].ops.load(Relaxed), 2, "the portal counts its operator and the worker arena under it");
                op_end(w);
                op_end(op);
                assert_eq!(CELLS[pc].ops.load(Relaxed), 0, "drained: uncounted");
                let s0 = op_sweeps(a);
                let _ = portal_end(a, p);
                assert_eq!(op_sweeps(a), s0, "no operator of its own: no sweep");
            }
            assert!(keyed(outer), "the statement's operator untouched");
            let p = new_owner_id();
            portal_begin(p);
            let op = op_begin(Owner::Portal { acct: a, portal: p }, Kind::Sort as u32, 0);
            let s0 = op_sweeps(a);
            let _ = portal_end(a, p);
            assert_eq!(op_sweeps(a), s0 + 1, "an operator still keyed: swept");
            assert!(!keyed(op));
            let s0 = op_sweeps(a);
            statement_end();
            assert_eq!(op_sweeps(a), s0 + 1);
            assert!(!keyed(outer));
        });
    }

    /// LEDGER2 step 1: a begin under an ended parent charges the parent's place and is counted, never lost.
    #[test]
    fn ledger2_a_begin_under_an_ended_parent_is_counted_and_charges_where_the_parent_went() {
        let _unbound = crate::unbound_shared(); // D4356: this test binds a backend, so it may move UNBOUND's books
        on(3932, |a| {
            statement_begin();
            let st = context().0;
            statement_end();
            let o0 = op_counters().1;
            let op = op_begin(st, Kind::Sort as u32, 0);
            assert_eq!((op, op_counters().1), (st, o0 + 1), "the parent itself");
            let c = Charge::open(op, Release::Statement, Kind::Sort, Door::Mapped, MIB);
            assert_eq!(owner_row(Owner::Session(a)).committed, M, "its session's, as a late charge of the statement's");
            drop(c);
            op_end(op);
        });
    }

    #[test]
    fn a_pool_post_lands_on_its_owner_and_moves_nothing_off_the_posting_thread() {
        let _unbound = crate::unbound_shared(); // D4356: this test binds a backend, so it may move UNBOUND's books
        on(3940, |a| {
            let s = Owner::Session(a);
            let (before, ctx0) = (nums(a).0, owner_ctx(s));
            let o = CtxOwner::named(s);
            let (o, other) = std::thread::spawn(move || {
                bind_backend(3941, 8000 + 3941);
                let b = Acct::backend(3941);
                let mut o = o;
                let n0 = nums(b).0;
                o.post_pool(M);
                let n1 = nums(b).0;
                unbind_backend();
                (o, (n0, n1))
            })
            .join()
            .unwrap();
            assert_eq!(other.0, other.1, "no move off the posting thread's account");
            assert_eq!(nums(a).0, before + M);
            assert_eq!(owner_ctx(s), (ctx0.0 + M, ctx0.1));
            assert_eq!(account_row(a).pagepool_bytes, M);
            let mut o = o;
            o.post_pool(-M);
            assert_eq!((nums(a).0, owner_ctx(s), account_row(a).pagepool_bytes), (before, ctx0, 0));
            let mut d = CtxOwner::named(s);
            d.post(M);
            assert_eq!(nums(a).0, before, "door 1's post on the owner's own thread moves nothing");
            d.post(-M);
        });
    }

    /// P3 (Door::Pool): a region's or an arena's pool bytes land on its owner at the commit, among its account's pool
    /// bytes, with no move off the committing thread's account; a transfer moves them with their pool share; the drop takes
    /// them off the account the owner's bytes are on.
    #[test]
    fn a_pool_door_charge_lands_on_its_owner_and_moves_with_it() {
        let _unbound = crate::unbound_shared(); // D4356: this test binds a backend, so it may move UNBOUND's books
        on(3946, |a| {
            let s = Owner::Session(a);
            let (n0, m0, q0, c0) = (nums(a).0, nums(a).1, account_row(a).pagepool_bytes, owner_row(s).committed);
            let (c, other) = std::thread::spawn(move || {
                bind_backend(3947, 8000 + 3947);
                let b = Acct::backend(3947);
                let before = (nums(b).0, account_row(b).pagepool_bytes);
                let c = Charge::open(s, Release::Session, Kind::Arena, Door::Pool, 2 * MIB);
                let after = (nums(b).0, account_row(b).pagepool_bytes);
                unbind_backend();
                (c, (before, after))
            })
            .join()
            .unwrap();
            assert_eq!(other.0, other.1, "no move off the committing thread's account, its pool bytes untouched");
            fold();
            assert_eq!((nums(a).0 - n0, account_row(a).pagepool_bytes - q0, owner_row(s).committed - c0), (2 * M, 2 * M, 2 * M));
            assert_eq!(nums(a).1, m0, "never door 2's mapped bytes");
            let mut c = c;
            c.give(MIB);
            c.commit(MIB);
            fold();
            assert_eq!((nums(a).0 - n0, account_row(a).pagepool_bytes - q0), (2 * M, 2 * M), "a give and a commit of the pool's pages, back where they were");
            let (cache, cacct) = (Owner::Cache(Holder::Of(Consumer::FpTables)), Acct::of(Consumer::FpTables));
            let (cn0, cq0) = (nums(cacct).0, account_row(cacct).pagepool_bytes);
            c.transfer(cache, Release::Evict);
            fold();
            assert_eq!((nums(a).0 - n0, account_row(a).pagepool_bytes - q0), (0, 0), "moved off the session");
            assert_eq!((nums(cacct).0 - cn0, account_row(cacct).pagepool_bytes - cq0), (2 * M, 2 * M), "onto the cache, pool bytes and all");
            drop(c);
            fold();
            assert_eq!((nums(cacct).0, account_row(cacct).pagepool_bytes), (cn0, cq0), "the drop takes them off");
            assert_eq!(owner_row(s).committed, c0);
        });
    }

    #[test]
    fn an_owner_lifetime_follows_its_cell_kind() {
        let _unbound = crate::unbound_shared(); // D4356: this test binds a backend, so it may move UNBOUND's books
        on(3942, |a| {
            statement_begin();
            let st = context().0;
            assert_eq!(CtxOwner::named(st).lifetime(), OwnerLife::Statement);
            let op = op_begin(st, Kind::Sort as u32, 0);
            assert_eq!(CtxOwner::named(op).lifetime(), OwnerLife::Statement);
            op_end(op);
            statement_end();
            assert_eq!(CtxOwner::named(st).lifetime(), OwnerLife::Session, "an ended statement's: its session's");
            assert_eq!(CtxOwner::named(Owner::Session(a)).lifetime(), OwnerLife::Session);
        });
        assert_eq!(CtxOwner::named(Owner::Cache(Holder::Of(Consumer::ALL[0]))).lifetime(), OwnerLife::Cache);
        assert_eq!(CtxOwner::named(Owner::Cache(Holder::Parked)).lifetime(), OwnerLife::Cache, "the depots' parked memory");
        assert_eq!(CtxOwner::named(Owner::Cache(Holder::Reaping)).lifetime(), OwnerLife::Process);
        let mut none = CtxOwner::NONE;
        assert_eq!(none.lifetime(), OwnerLife::Process);
    }

    /// memlead's E2 review: an owner KEPT past its end (a context made during a statement, a portal or an operator and asked
    /// after it) reads its session's lifetime, not its reused cell's.
    #[test]
    fn a_kept_owner_reads_its_session_s_lifetime_after_its_end() {
        let _unbound = crate::unbound_shared(); // D4356: this test binds a backend, so it may move UNBOUND's books
        on(3945, |a| {
            statement_begin();
            let mut stmt = CtxOwner::named(context().0);
            assert_eq!(stmt.lifetime(), OwnerLife::Statement);
            let op = op_begin(context().0, Kind::Sort as u32, 0);
            let mut kept_op = CtxOwner::named(op);
            assert_eq!(kept_op.lifetime(), OwnerLife::Statement);
            op_end(op);
            assert_eq!(kept_op.lifetime(), OwnerLife::Session, "an ended operator's: its session's");
            statement_end();
            statement_begin();
            assert_eq!(CtxOwner::named(context().0).lifetime(), OwnerLife::Statement, "the next statement, on the reused cell");
            assert_eq!(stmt.lifetime(), OwnerLife::Session, "the ended statement's: its session's, not the reused cell's");
            statement_end();
            portal_begin(0x5e5_0001);
            let mut portal = CtxOwner::named(Owner::Portal { acct: a, portal: 0x5e5_0001 });
            assert_eq!(portal.lifetime(), OwnerLife::Statement);
            let _ = portal_end(a, 0x5e5_0001);
            portal_begin(0x5e5_0002);
            assert_eq!(portal.lifetime(), OwnerLife::Session, "an ended portal's: its session's, whoever took its cell");
            let _ = portal_end(a, 0x5e5_0002);
        });
    }

    #[test]
    fn the_pool_door_counts_claims_and_the_probe_reads_the_pool() {
        fn probe() -> PoolReading {
            PoolReading { claimed: 6 * MIB, warm_free: 2 * MIB, shelved: MIB, stranded: 0 }
        }
        let _unbound = crate::unbound_shared(); // pppress card 9: its claims stay out of a_door_one_pool_byte_is_counted_once
        let c0 = pool_claimed();
        door_pool(4 * M);
        assert_eq!(pool_claimed(), c0 + 4 * M);
        assert_eq!(pool_reading(), PoolReading { claimed: (c0 + 4 * M) as u64, ..PoolReading::default() });
        door_pool(-4 * M);
        assert!(!pool_probed());
        set_pool_probe(probe);
        assert!(pool_probed());
        assert_eq!(pool_reading(), probe());
    }

    /// pppress card 9: a pool byte that door 1 charged (`PoolAlloc`'s block, read through `set_pool_door1`) is on its
    /// account in `doors_total`, so it is owned, not unowned: the attributed sum the projection subtracts (`doors_total` +
    /// `pool_unowned`, memruntime's ring) counts it once. Red before the fix: `pool_unowned` kept door 1's bytes, so the sum
    /// rose by them twice.
    #[test]
    fn a_door_one_pool_byte_is_counted_once() {
        static DOOR1: AtomicI64 = AtomicI64::new(0);
        fn door1() -> u64 {
            DOOR1.load(Relaxed).max(0) as u64
        }
        let _alone = crate::unbound_alone(); // no other test's posts move doors_total or the pool's books between the reads
        set_pool_door1(door1);
        // the pool claims more than every owner's pooled bytes, so pool_unowned reads above 0 (no clamp) throughout
        let claim = 64 * M + pagepool_total().max(0);
        door_pool(claim);
        crate::fold();
        let (t0, u0) = (crate::doors_total(), pool_unowned());
        // PoolAlloc hands out 3 MiB of what the pool claimed: door 1 charges it on this thread's account
        crate::door_heap((3 * M) as isize);
        crate::fold();
        DOOR1.store(3 * M, Relaxed);
        assert_eq!(crate::doors_total() - t0, 3 * M, "door 1 charged the block");
        assert_eq!(pool_door1(), 3 * M);
        assert_eq!(pool_unowned(), u0 - 3 * M, "the block is owned: it leaves the pool's unowned bytes");
        assert_eq!(crate::doors_total() + pool_unowned(), t0 + u0, "counted once: on its account, not unowned too");
        crate::door_heap(-(3 * M) as isize);
        crate::fold();
        DOOR1.store(0, Relaxed);
        assert_eq!((crate::doors_total(), pool_unowned()), (t0, u0));
        door_pool(-claim);
    }

    #[test]
    fn a_pool_free_is_never_floored() {
        let _unbound = crate::unbound_alone(); // switches the floor and reads its counts
        crate::set_floor(true);
        let f0 = crate::floored();
        on(3943, |a| {
            let mut o = CtxOwner::named(Owner::Session(a));
            let n0 = nums(a).0;
            o.post_pool(-M);
            fold();
            let r = account_row(a);
            assert_eq!((r.account_bytes - n0, r.pagepool_bytes), (-M, -M), "door 1's floor leaves the pool's bytes alone");
            o.post_pool(M);
        });
        crate::set_floor(false);
        assert_eq!(crate::floored(), f0, "nothing floored");
    }

    #[test]
    fn a_pool_free_after_its_sessions_unbind_lands_on_unbound_never_the_next_session() {
        let _unbound = crate::unbound_alone(); // reads UNBOUND's delta
        on(3944, |a| {
            let mut o = CtxOwner::named(Owner::Session(a));
            o.post_pool(M);
            fold();
            unbind_backend();
            bind_backend(3944, 8000 + 3944);
            let (n1, u1) = (nums(a).0, nums(Acct::UNBOUND).0);
            o.post_pool(-M);
            assert_eq!(nums(a).0, n1, "not on the slot's next session");
            assert_eq!(nums(Acct::UNBOUND).0, u1 - M, "on UNBOUND, where the unbind moved the block's bytes");
            assert_eq!(account_row(a).pagepool_bytes, 0);
        });
    }
}
