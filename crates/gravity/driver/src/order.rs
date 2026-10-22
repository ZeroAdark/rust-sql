//! Ordered output while a section runs (R1D: one general mechanism): a W > 1 pipeline ending in an emit
//! hands its rows to the leader claim by claim in claim order (ascending `lo`, the claims tiling the source), the leader
//! emitting each finished prefix as it forms. A window bounds what waits: once `window` + W claims are finished or
//! admitted, no claim but the head starts, so at most the window plus the W running claims (and the head) are buffered.
//! The interface names claims only (no source kind).

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use gravity_abi::MorselRange;
use pgsync::{Mutex, MutexGuard};

mod raw;

/// The thread primitives the pool lends the order (the driver owns no thread and links no waiter): the leader's park word
/// and the unpark of a word (armed before the section starts), the running thread's word and a timed park of it (for a
/// claim that waits for room in the window: the pool admits before a claim takes a slot, so the park holds none, D278).
/// Unarmed, nothing wakes and no claim waits (the bound on buffered claims slips, the order holds).
#[derive(Default)]
pub struct Wake {
    word: AtomicU64,
    unpark: AtomicUsize,
    current: AtomicUsize,
    park_ms: AtomicUsize,
}

impl Wake {
    pub fn arm(&self, word: u64, unpark: fn(u64)) {
        self.unpark.store(unpark as usize, Ordering::Release);
        self.word.store(word, Ordering::Release);
    }

    /// The pool's parking for a worker thread: `current` = the running thread's word, `park_ms` = park it up to n ms (an
    /// unpark of its word ends the park early).
    pub fn arm_park(&self, current: fn() -> u64, park_ms: fn(u64)) {
        self.park_ms.store(park_ms as usize, Ordering::Release);
        self.current.store(current as usize, Ordering::Release);
    }

    pub fn wake(&self) {
        self.unpark_word(self.word.load(Ordering::Acquire));
    }
}

struct St {
    /// The head: the `lo` the next claim to emit starts at.
    next: u64,
    /// Finished claims not yet emitted, `(lo, hi, ok)`.
    done: Vec<(u64, u64, bool)>,
    /// Admitted claims not yet finished: running, or waiting for a pool slot (the pool admits before the lease, D278).
    admitted: usize,
    stopped: bool,
    /// The lowest claim that failed (`u64::MAX`: none): a claim above it is skipped, one below it starts at once.
    failed_at: u64,
    /// The workers parked for room, `(claim lo, park word)`. A release wakes the head (when parked) and the lowest; each
    /// claim admitted then wakes the next two while room is left (D278: the leader's futex wakes stay two per release,
    /// not one per parked claim, each of which could preempt it); a stop wakes every one.
    parked: Vec<(u64, u64)>,
    /// The admitted claims not yet finished, by `lo` (D2985: a running head is one the leader's release follows).
    running: Vec<u64>,
}

pub struct ClaimOrder {
    /// The claims past the W running ones that may wait for the leader; 0 once G refused their bytes (only the head runs).
    window: AtomicUsize,
    /// The section's width: the running claims the window adds to (`admitted` counts them with those waiting for a slot).
    width: usize,
    st: Mutex<St>,
    pub wake: Wake,
    /// Claims that waited for room, and the waits that timed out (the window overrun instead of stalling a head it missed).
    pub waits: AtomicU64,
    pub overruns: AtomicU64,
    /// How long a claim waits for room: `ROOM_WAIT` (tests set it long so a wake, never the timeout, decides).
    room_wait: core::time::Duration,
}

pub use gravity_program::cost::runtime::WINDOW_FLOOR;

/// A claim waits at most this long for room: the head is found by `first_live_at`, and a source whose hand-out skips
/// differently must not stall the section (the order holds regardless; only the bound on buffered claims slips). Once G
/// narrowed the window and a claim holds the head (`head_held`), only up to `overrun_cap` claims are finished or admitted
/// past it; then the head's finish and the leader's release make room (D2985). The cap keeps W workers busy behind a slow
/// head (a cold one faulting its part's dictionaries: none overrunning idled W - 1 workers, 2x a cold W8 scan) while a
/// leader blocked on a slow sink buffers at most that many claims.
const ROOM_WAIT: core::time::Duration = core::time::Duration::from_millis(20);

impl ClaimOrder {
    /// A section's order: the window 2 x W, at least `WINDOW_FLOOR`.
    pub fn for_section(width: usize, first: u64) -> ClaimOrder {
        ClaimOrder::new((2 * width).max(WINDOW_FLOOR), width, first)
    }

    /// The claims past the W running ones that may wait for the leader (0 once narrowed).
    pub fn window(&self) -> usize {
        self.window.load(Ordering::Relaxed)
    }

    pub fn new(window: usize, width: usize, first: u64) -> ClaimOrder {
        ClaimOrder { window: AtomicUsize::new(window.max(1)), width, st: Mutex::new(St { next: first, done: Vec::with_capacity(window * 2), admitted: 0, stopped: false, failed_at: u64::MAX, parked: Vec::new(), running: Vec::new() }), wake: Wake::default(), waits: AtomicU64::new(0), overruns: AtomicU64::new(0), room_wait: ROOM_WAIT } // alloc: Exec
    }

    /// The same order with claims waiting up to `wait` for room instead of `ROOM_WAIT`.
    pub fn with_room_wait(mut self, wait: core::time::Duration) -> ClaimOrder {
        self.room_wait = wait;
        self
    }

    fn lock(&self) -> MutexGuard<'_, St> {
        pgsync::lock(&self.st)
    }

    /// The head always; below a failed claim at once (its rows and error come first); else while the finished and the
    /// admitted claims leave room in the window plus the width. Narrowed (window 0), only the head.
    fn has_room(&self, s: &St, claim: MorselRange) -> bool {
        let window = self.window.load(Ordering::Relaxed);
        claim.lo == s.next || (s.failed_at != u64::MAX && claim.lo < s.failed_at) || (window > 0 && s.done.len() + s.admitted < window + self.width)
    }

    /// The head is held by a claim: finished (waiting for the leader), running, or parked (the next release wakes it). In a
    /// narrowed window only a head no claim holds (a hand-out that skips it) lets a claim start at `ROOM_WAIT` (D2985).
    fn head_held(s: &St) -> bool {
        s.done.iter().any(|&(lo, _, _)| lo == s.next) || s.running.contains(&s.next) || s.parked.iter().any(|&(lo, _)| lo == s.next)
    }

    /// D2985: the claims a narrowed window lets be finished or admitted behind a held head (`ROOM_WAIT` overruns): 2 x W.
    fn overrun_cap(&self) -> usize {
        2 * self.width.max(1)
    }

    /// Room for claims other than the head: the window plus the width past the finished and the admitted claims.
    fn room(&self, s: &St) -> usize {
        match self.window.load(Ordering::Relaxed) {
            0 => 0,
            window => (window + self.width).saturating_sub(s.done.len() + s.admitted),
        }
    }

    /// Up to two parked claims to wake, taken off the list: the head when it waits, then the lowest while room lasts
    /// (0: none).
    fn wakes(&self, s: &mut St) -> [u64; 2] {
        let mut out = [0u64; 2];
        let mut room = self.room(s);
        for w in out.iter_mut() {
            let head = s.parked.iter().position(|&(lo, _)| lo == s.next);
            let lowest = || s.parked.iter().enumerate().min_by_key(|&(_, &(lo, _))| lo).map(|(i, _)| i);
            let Some(i) = head.or_else(|| if room > 0 { lowest() } else { None }) else { break };
            let (lo, word) = s.parked.swap_remove(i);
            room -= (lo != s.next) as usize;
            *w = word;
        }
        out
    }

    fn unpark(&self, words: [u64; 2]) {
        for w in words {
            self.wake.unpark_word(w);
        }
    }

    /// Admitted claims not yet finished (0 once a section's every admitted claim settled).
    pub fn admitted(&self) -> usize {
        self.lock().admitted
    }

    /// Before a claim runs (its worker's thread): false once the leader stopped (the claim is skipped). Past the window
    /// only the head starts; another claim parks until the leader releases a prefix, stops, or `ROOM_WAIT` passes. A pool
    /// calls it before the claim leases a slot (D278): a claim parked here holding one kept it from the head, whose finish
    /// alone makes room, and every round ended only at `ROOM_WAIT`.
    pub fn admit(&self, claim: MorselRange) -> bool {
        let mut g = self.lock();
        if g.stopped || claim.lo > g.failed_at {
            return false;
        }
        if self.has_room(&g, claim) {
            g.admitted += 1;
            g.running.push(claim.lo); // alloc: Exec
            let wake = if g.parked.is_empty() { [0; 2] } else { self.wakes(&mut g) };
            drop(g);
            self.unpark(wake);
            return true;
        }
        self.waits.fetch_add(1, Ordering::Relaxed);
        let Some(me) = self.wake.current() else {
            self.overruns.fetch_add(1, Ordering::Relaxed);
            g.admitted += 1;
            g.running.push(claim.lo); // alloc: Exec
            return true;
        };
        let mut deadline = pg_clock::Deadline::after(self.room_wait);
        loop {
            if !g.parked.iter().any(|&(_, w)| w == me) {
                g.parked.push((claim.lo, me)); // alloc: Exec
            }
            drop(g);
            self.wake.park(deadline.remaining_ms().max(1) as u64);
            g = self.lock();
            let admitted = if g.stopped || claim.lo > g.failed_at {
                Some(false)
            } else if self.has_room(&g, claim) {
                Some(true)
            } else if deadline.expired() && self.window.load(Ordering::Relaxed) == 0 && Self::head_held(&g) && g.done.len() + g.admitted >= self.overrun_cap() {
                // D2985: the head runs or is finished and waits for the leader, whose sink is slow (a COPY TO a client
                // that reads slower than the workers produce): the head's finish and the leader's release make room.
                // Overrun here and every parked claim ran once per `ROOM_WAIT` while the leader blocked on the socket
                // (narrowed too: only the head was to run), so the legs held the table's rows (28.9 GB anon RSS,
                // OOM-killed).
                deadline = pg_clock::Deadline::after(self.room_wait);
                None
            } else if deadline.expired() {
                self.overruns.fetch_add(1, Ordering::Relaxed);
                Some(true)
            } else {
                None
            };
            if let Some(a) = admitted {
                g.parked.retain(|&(_, w)| w != me);
                g.admitted += a as usize;
                if a {
                    g.running.push(claim.lo); // alloc: Exec
                }
                // admitted: the wake passes on while room is left
                let wake = if a { self.wakes(&mut g) } else { [0; 2] };
                drop(g);
                self.unpark(wake);
                return a;
            }
        }
    }

    /// An admitted claim settled (its rows in its leg; `ok` false when it failed or never ran: every admitted claim
    /// settles, else the head never moves past it): the leader is woken when it completes the head.
    pub fn finish(&self, claim: MorselRange, ok: bool) {
        let head = {
            let mut g = self.lock();
            g.admitted = g.admitted.saturating_sub(1);
            if let Some(i) = g.running.iter().position(|&lo| lo == claim.lo) {
                g.running.swap_remove(i);
            }
            g.done.push((claim.lo, claim.hi, ok)); // alloc: Exec
            claim.lo == g.next
        };
        if head {
            self.wake.wake();
        }
    }

    /// The leader: the finished prefix from the head, in claim order, appended to `out` (their `lo`) and released; the
    /// head then moves past each (`first_live_at` bridges units no claim covers). A failed claim ends the prefix and the
    /// order: its rows before the failing one are the last emitted (ENTRY E3), in claim order across the workers.
    pub fn ready(&self, first_live_at: &dyn Fn(u64) -> u64, out: &mut Vec<u64>) {
        let mut g = self.lock();
        let n = out.len();
        while let Some(i) = g.done.iter().position(|&(lo, _, _)| lo == g.next) {
            let (lo, hi, ok) = g.done.swap_remove(i);
            out.push(lo); // alloc: Exec
            if !ok {
                g.stopped = true;
                g.next = u64::MAX;
                break;
            }
            g.next = first_live_at(hi);
        }
        let wake = if out.len() > n { self.wakes(&mut g) } else { [0; 2] };
        drop(g);
        self.unpark(wake);
    }

    /// G refused the window's bytes: from now only the head starts (the rows already buffered stay; nothing more waits).
    pub fn narrow(&self) {
        self.window.store(0, Ordering::Relaxed);
    }

    /// The leader stopped (the receiver's stop, a LIMIT, an error): waiting claims leave, no claim is admitted.
    pub fn stop(&self) {
        let parked = {
            let mut g = self.lock();
            g.stopped = true;
            core::mem::take(&mut g.parked)
        };
        self.unpark_all(parked);
    }

    /// Claim `lo` failed (a worker's error): claims above it are skipped, those below it (handed out before it, their rows
    /// and a lower error first) start without waiting for room; claims parked for room leave or start now.
    pub fn stop_above(&self, lo: u64) {
        let parked = {
            let mut g = self.lock();
            g.failed_at = g.failed_at.min(lo);
            core::mem::take(&mut g.parked)
        };
        self.unpark_all(parked);
    }

    fn unpark_all(&self, parked: Vec<(u64, u64)>) {
        for (_, w) in parked {
            self.wake.unpark_word(w);
        }
    }
}
