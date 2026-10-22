//! C8 checker 1 (lanes/memlead/C8-DESIGN.md §6): the owner-end residues. An owner's end that finds bytes or uses still on
//! its cell (a statement's at `statement_end`, a portal's at the runner's PortalDrop, a session's at `unbind_backend`)
//! notes a residue here: a counter per owner class, its bytes and uses, and a row in a fixed ring of the latest `RING`
//! (no allocation). Part uses are owned (area move 2: a `PartUse` is its taker's charge, a kept cursor passes to its
//! session), so a use left on a statement's or a session's cell is its residue. Beside the cells the checker reads one
//! residue the ledger's legacy books show until the area moves own every byte: the bytes each unbind moves to UNBOUND,
//! and UNBOUND's net now (D4084: UNBOUND grows past the sessions that left it).
//!
//! Report mode: `pgrust_memory_owner_end()` shows the counters and the ring; the stress key `owner-end` reads it at the
//! run's end. `set_assert` makes a debug build panic at an owner cell's residue (never at a legacy one: those are what the
//! area moves remove); nothing turns it on before the key has been green (it gets its knob then).

use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering::Acquire, Ordering::Relaxed, Ordering::Release};

use crate::charge::{OwnerClass, Residue, OWNER_CLASSES};
use crate::Acct;

/// Where a residue was read: an owner cell, or the account's legacy books (before the area moves give its charges owners),
/// or (C8 move 6, D2953) the cells of a session that died by a kill-class exit, ended at the crash cycle's quiesce point by
/// `unbind_dead` in a server that reinitializes in its own process (postgres --no-supervisor, wasm, pgrust_sim).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Source {
    Owner = 0,
    Legacy = 1,
    Crashed = 2,
}

impl Source {
    pub const ALL: [Source; SOURCES] = [Source::Owner, Source::Legacy, Source::Crashed];

    pub const fn name(self) -> &'static str {
        match self {
            Source::Owner => "owner",
            Source::Legacy => "legacy",
            Source::Crashed => "crashed",
        }
    }
}

pub const SOURCES: usize = 3;

/// The latest residues kept, newest overwriting oldest.
pub const RING: usize = 64;

struct Slot {
    /// The residue's number (1 = the process's first); 0 while unwritten or being rewritten.
    seq: AtomicU64,
    /// Its class, and its source in the second byte.
    tag: AtomicU32,
    acct: AtomicU32,
    pid: AtomicU32,
    committed: AtomicI64,
    reserved: AtomicI64,
    uses: AtomicI64,
}

#[allow(clippy::declare_interior_mutable_const)]
const SLOT0: Slot = Slot {
    seq: AtomicU64::new(0),
    tag: AtomicU32::new(0),
    acct: AtomicU32::new(0),
    pid: AtomicU32::new(0),
    committed: AtomicI64::new(0),
    reserved: AtomicI64::new(0),
    uses: AtomicI64::new(0),
};

static RING_SLOTS: [Slot; RING] = [SLOT0; RING];
static NOTED: AtomicU64 = AtomicU64::new(0);

#[allow(clippy::declare_interior_mutable_const)]
const ZEROS: [AtomicU64; OWNER_CLASSES] = [const { AtomicU64::new(0) }; OWNER_CLASSES];
/// Per source and owner class: the ends that found a residue, its committed bytes (the positive ones) and its uses.
static EVENTS: [[AtomicU64; OWNER_CLASSES]; SOURCES] = [ZEROS; SOURCES];
static BYTES: [[AtomicU64; OWNER_CLASSES]; SOURCES] = [ZEROS; SOURCES];
static USES: [[AtomicU64; OWNER_CLASSES]; SOURCES] = [ZEROS; SOURCES];

/// The unbinds, and the bytes they moved to UNBOUND (signed: a session that freed more than it took moves a debt).
static UNBINDS: AtomicU64 = AtomicU64::new(0);
static UNBOUND_MOVED: AtomicI64 = AtomicI64::new(0);

/// Notes residue `r` read from `src`: its counters and a ring row; an owner cell's panics under the debug assert.
pub(crate) fn note(src: Source, r: Residue) {
    note_as(src, r, None);
}

/// `note` with the residue's pid given (None: its account's slot's now).
pub(crate) fn note_as(src: Source, r: Residue, pid: Option<u32>) {
    let (s, c) = (src as usize, r.class as usize);
    EVENTS[s][c].fetch_add(1, Relaxed);
    BYTES[s][c].fetch_add(r.committed.max(0) as u64, Relaxed);
    USES[s][c].fetch_add(r.uses.max(0) as u64, Relaxed);
    let n = NOTED.fetch_add(1, Relaxed) + 1;
    let row = &RING_SLOTS[((n - 1) % RING as u64) as usize];
    row.seq.store(0, Relaxed);
    row.tag.store(c as u32 | (s as u32) << 8, Relaxed);
    row.acct.store(r.acct.0, Relaxed);
    row.pid.store(pid.unwrap_or_else(|| crate::SLOTS.get(r.acct.0 as usize).map_or(0, |x| x.pid.load(Relaxed))), Relaxed);
    row.committed.store(r.committed, Relaxed);
    row.reserved.store(r.reserved, Relaxed);
    row.uses.store(r.uses, Relaxed);
    row.seq.store(n, Release);
    if src == Source::Owner && asserting() {
        let (c, rv, u) = (r.committed, r.reserved, r.uses);
        panic!("C8 owner end: a {} ended owning {c} committed bytes, {rv} reserved and {u} uses (account {})", r.class.name(), r.acct.0);
    }
}

/// Legacy residue (D4084): an unbind moved `left` bytes to UNBOUND.
pub(crate) fn unbound(left: i64) {
    UNBINDS.fetch_add(1, Relaxed);
    UNBOUND_MOVED.fetch_add(left, Relaxed);
}

/// C8 move 6: the dead sessions `unbind_dead` unbound, the bytes it moved to UNBOUND and the pool reservations it dropped.
static CRASHED: AtomicU64 = AtomicU64::new(0);
static CRASHED_MOVED: AtomicI64 = AtomicI64::new(0);
static CRASHED_RESERVED: AtomicI64 = AtomicI64::new(0);

/// C8 move 6: dead backend account `a`'s slot was unbound at the crash cycle (`unbind_dead`): `left` bytes moved to UNBOUND
/// and `reserved` pool bytes dropped; `noted` of its cells ended as residues. With none, its session is still one row (its
/// moved bytes and its dropped room), so every crashed session shows. Before the slot's pid clears (the row names it).
pub(crate) fn crashed(a: Acct, left: i64, reserved: i64, noted: u32) {
    CRASHED.fetch_add(1, Relaxed);
    CRASHED_MOVED.fetch_add(left, Relaxed);
    CRASHED_RESERVED.fetch_add(reserved, Relaxed);
    if noted == 0 {
        note(Source::Crashed, Residue { class: OwnerClass::Session, acct: a, committed: left, reserved, uses: 0 });
    }
}

/// (the dead sessions unbound at crash cycles, the bytes moved to UNBOUND, the pool reservations dropped).
pub fn crashed_unbinds() -> (u64, i64, i64) {
    (CRASHED.load(Relaxed), CRASHED_MOVED.load(Relaxed), CRASHED_RESERVED.load(Relaxed))
}

static ASSERT: AtomicBool = AtomicBool::new(false);

/// Checker 1's assert mode: from now on an owner cell's residue panics in a debug build (`false`: report only).
pub fn set_assert(on: bool) {
    ASSERT.store(on, Relaxed);
}

#[inline]
fn asserting() -> bool {
    cfg!(debug_assertions) && ASSERT.load(Relaxed)
}

/// One source's and class's totals since the process started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    pub events: u64,
    pub committed: u64,
    pub uses: u64,
}

/// The totals of residues of class `class` read from `src`.
pub fn totals(src: Source, class: OwnerClass) -> Totals {
    let (s, c) = (src as usize, class as usize);
    Totals { events: EVENTS[s][c].load(Relaxed), committed: BYTES[s][c].load(Relaxed), uses: USES[s][c].load(Relaxed) }
}

/// (the unbinds, the bytes they moved to UNBOUND, UNBOUND's net now).
pub fn unbinds() -> (u64, i64, i64) {
    (UNBINDS.load(Relaxed), UNBOUND_MOVED.load(Relaxed), crate::SLOTS[Acct::UNBOUND.0 as usize].net.load(Relaxed))
}

/// One residue of the ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Recent {
    pub seq: u64,
    pub source: Source,
    pub class: OwnerClass,
    pub acct: Acct,
    pub pid: u32,
    pub committed: i64,
    pub reserved: i64,
    pub uses: i64,
}

/// `f` over the ring's residues, newest first (a row being rewritten as it is read is skipped).
pub fn for_each_recent(mut f: impl FnMut(&Recent)) {
    let n = NOTED.load(Relaxed);
    for k in 0..n.min(RING as u64) {
        let want = n - k;
        let row = &RING_SLOTS[((want - 1) % RING as u64) as usize];
        if row.seq.load(Acquire) != want {
            continue;
        }
        let tag = row.tag.load(Relaxed);
        let r = Recent {
            seq: want,
            source: Source::ALL[((tag >> 8) as usize).min(SOURCES - 1)],
            class: OwnerClass::ALL[((tag & 0xff) as usize).min(OWNER_CLASSES - 1)],
            acct: Acct(row.acct.load(Relaxed)),
            pid: row.pid.load(Relaxed),
            committed: row.committed.load(Relaxed),
            reserved: row.reserved.load(Relaxed),
            uses: row.uses.load(Relaxed),
        };
        if row.seq.load(Acquire) == want {
            f(&r);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::charge::{context, live_portals, new_owner_id, owner_row, portal_begin, portal_end, OwnerRow};
    use crate::{bind_backend, statement_begin, statement_end, unbind_backend, Charge, Owner, Release};

    /// Each test on its own thread and backend slot (3907..3910: no other test's).
    fn on(procno: u32, f: impl FnOnce(Acct) + Send + 'static) {
        std::thread::spawn(move || {
            bind_backend(procno, 8000 + procno);
            f(Acct::backend(procno));
            unbind_backend();
        })
        .join()
        .unwrap();
    }

    /// The ring's residues on account `a`, newest first: (source, class, uses).
    fn recent(a: Acct) -> Vec<(Source, OwnerClass, i64)> {
        let mut v = Vec::new();
        for_each_recent(|r| {
            if r.acct == a {
                v.push((r.source, r.class, r.uses));
            }
        });
        v
    }

    #[test]
    fn a_use_left_on_a_statement_is_its_residue_and_a_kept_one_passes_to_the_session() {
        let _unbound = crate::unbound_shared(); // D4356: this test binds a backend, so it may move UNBOUND's books
        on(3907, |a| {
            statement_begin();
            let (o, rel) = context();
            let mut kept = Charge::use_of(o, rel);
            let left = Charge::use_of(o, rel);
            kept.transfer(Owner::Session(a), Release::Session);
            statement_end();
            assert_eq!(recent(a), [(Source::Owner, OwnerClass::Statement, 1)], "the use no one passed on (D4087)");
            assert_eq!(owner_row(Owner::Session(a)).uses, 2, "both on the session now");
            drop(left);
            statement_begin();
            statement_end();
            assert_eq!(recent(a).len(), 1, "the kept use is the session's: no residue");
            drop(kept);
            assert_eq!(owner_row(Owner::Session(a)), OwnerRow::default());
        });
    }

    #[test]
    fn a_session_that_ends_owning_a_use_is_its_residue() {
        let _unbound = crate::unbound_shared(); // D4356: this test binds a backend, so it may move UNBOUND's books
        let a = Acct::backend(3908);
        let (n0, _, _) = unbinds();
        let held = std::thread::spawn(move || {
            bind_backend(3908, 1);
            let u = Charge::use_of(Owner::Session(a), Release::Session);
            unbind_backend();
            u
        })
        .join()
        .unwrap();
        assert!(unbinds().0 > n0);
        assert_eq!(recent(a), [(Source::Owner, OwnerClass::Session, 1)], "a set's cursor past the backend's exit");
        drop(held);
    }

    /// C8 checker 1 (B2): ProcKill unbinds a session before launch_backend's run_session_teardown drops its estate (the
    /// memo's kept cursors, its kept arena), so the unbind is not the session's end: a use the teardown releases is no
    /// residue, one it leaves is. Red on trunk: the unbind noted both uses as the session's residue.
    #[test]
    fn a_sessions_end_check_waits_for_its_teardown() {
        let _unbound = crate::unbound_shared(); // D4356: this test binds a backend, so it may move UNBOUND's books
        let a = Acct::backend(3921);
        let (held, at_unbind) = std::thread::spawn(move || {
            bind_backend(3921, 1);
            let kept = Charge::use_of(Owner::Session(a), Release::Session);
            let held = Charge::use_of(Owner::Session(a), Release::Session);
            unbind_backend();
            let at_unbind = recent(a);
            // the teardown: the memo's kept cursor goes; `held` is one it never releases
            drop(kept);
            (held, at_unbind)
        })
        .join()
        .unwrap();
        assert_eq!(at_unbind, [], "the unbind is not the session's end: its teardown has not run");
        assert_eq!(recent(a), [(Source::Owner, OwnerClass::Session, 1)], "what the teardown left, at the thread's end");
        drop(held);
    }

    /// B2: `session_drained` (mcxt_stats' run_session_teardown, at its end) is the session's end: a statement's use left
    /// at the unbind is still the statement's residue at once; the session's charges released between the unbind and the
    /// drain find its exit cell (none reaches the orphan cell or the slot's next session); what the drain finds is the
    /// session's residue, with the session's pid though the slot is the next session's by then.
    #[test]
    fn a_drained_session_ends_with_what_its_teardown_left() {
        let _unbound = crate::unbound_shared(); // D4356: this test binds a backend, so it may move UNBOUND's books
        use crate::charge::Door;
        use crate::Kind;
        let a = Acct::backend(3922);
        let (pages, before) = std::thread::spawn(move || {
            bind_backend(3922, 7922);
            statement_begin();
            let (o, rel) = context();
            let left = Charge::use_of(o, rel);
            let kept = Charge::use_of(Owner::Session(a), Release::Session);
            let pages = Charge::open(Owner::Session(a), Release::Session, Kind::Mapped, Door::Mapped, 1 << 20);
            unbind_backend();
            assert_eq!(recent(a), [(Source::Owner, OwnerClass::Statement, 1)], "the statement's residue, at the unbind");
            // the slot's next session binds on another thread before this one's teardown ends
            let next = std::thread::spawn(|| {
                bind_backend(3922, 8922);
                let r = owner_row(Owner::Session(Acct::backend(3922)));
                unbind_backend();
                r
            });
            assert_eq!(next.join().unwrap(), OwnerRow::default(), "the next session inherits nothing");
            // the teardown releases the statement's use and the kept cursor
            drop(left);
            drop(kept);
            let before = recent(a).len();
            crate::charge::session_drained();
            (pages, before)
        })
        .join()
        .unwrap();
        let mut rows = Vec::new();
        for_each_recent(|r| {
            if r.acct == a && r.class == OwnerClass::Session {
                rows.push((r.pid, r.committed, r.uses));
            }
        });
        assert_eq!(before, 1, "nothing noted between the unbind and the drain");
        assert_eq!(rows, [(7922, 1 << 20, 0)], "the drain: the pages the teardown left, under the session's own pid");
        drop(pages);
    }

    #[test]
    fn a_session_that_never_unbound_ends_at_the_next_bind_of_its_slot() {
        let _unbound = crate::unbound_shared(); // D4356: this test binds a backend, so it may move UNBOUND's books
        let a = Acct::backend(3910);
        // a kill-class exit: the session's thread ends with no unbind (ProcKill skipped)
        let held = std::thread::spawn(move || {
            bind_backend(3910, 1);
            Charge::use_of(Owner::Session(a), Release::Session)
        })
        .join()
        .unwrap();
        on(3910, move |a| {
            assert_eq!(owner_row(Owner::Session(a)), OwnerRow::default(), "the next session inherits nothing");
            assert_eq!(recent(a), [(Source::Owner, OwnerClass::Session, 1)], "the dead session's use is its residue");
        });
        drop(held);
    }

    /// C8 move 6 (D2953): a session that died by a kill-class exit (its thread ended with no unbind, holding a session
    /// charge and pool room) is unbound by the ledger alone at the crash cycle's quiesce point (`unbind_dead`): its cells end
    /// as 'crashed' residues, its net moves to UNBOUND, its reservations are dropped and the slot's next bind starts at 0.
    #[test]
    fn a_dead_sessions_slot_is_unbound_at_the_crash_cycle_and_its_next_bind_starts_at_zero() {
        use crate::charge::Door;
        use crate::{account_row, pool_charge, Kind};
        const P: u32 = 3990;
        const MIB: u64 = 1 << 20;
        let a = Acct::backend(P);
        let held = std::thread::spawn(move || {
            bind_backend(P, 4242);
            let c = Charge::open(Owner::Session(a), Release::Session, Kind::Mapped, Door::Mapped, MIB);
            pool_charge(a, 3 * MIB as i64);
            c
        })
        .join()
        .unwrap();
        let r = account_row(a);
        assert_eq!((r.account_bytes, r.pool_reserved_bytes, r.pid), (MIB as i64, 3 * MIB as i64, 4242), "the dead session's books");
        // D4356: no other test moves UNBOUND between the two reads (every test that binds a backend holds the turn shared)
        let alone = crate::unbound_alone();
        let unbound = account_row(Acct::UNBOUND).account_bytes;
        assert!(crate::unbind_dead(P), "a bound slot at the crash cycle");
        assert!(!crate::unbind_dead(P), "once");
        let r = account_row(a);
        assert_eq!((r.account_bytes, r.pool_reserved_bytes, r.pid), (0, 0, 0), "nothing left on the slot");
        assert_eq!(account_row(Acct::UNBOUND).account_bytes - unbound, MIB as i64, "its net on UNBOUND, as an unbind's");
        drop(alone);
        let mut rows = Vec::new();
        for_each_recent(|r| {
            if r.acct == a {
                rows.push((r.source.name(), r.class, r.committed, r.pid));
            }
        });
        assert_eq!(rows, [("crashed", OwnerClass::Session, MIB as i64, 4242)], "one crashed row: its session's charge");
        on(P, move |a| {
            assert_eq!(account_row(a).account_bytes, 0, "the next session starts at zero");
            assert_eq!(owner_row(Owner::Session(a)), OwnerRow::default(), "and owns nothing");
        });
        drop(held);
    }

    #[test]
    fn a_portal_no_portal_drop_reached_ends_with_its_session() {
        let _unbound = crate::unbound_shared(); // D4356: this test binds a backend, so it may move UNBOUND's books
        let p = new_owner_id();
        on(3909, move |a| {
            portal_begin(p);
            assert_eq!(live_portals(a), 1);
        });
        on(3909, move |a| {
            assert_eq!(live_portals(a), 0, "the next session starts with none live");
            assert_eq!(owner_row(Owner::Portal { acct: a, portal: p }), OwnerRow::default());
            assert_eq!(portal_end(a, p), None, "its cell went with its session");
        });
        assert!(recent(Acct::backend(3909)).is_empty(), "an empty portal's end is no residue");
    }
}
