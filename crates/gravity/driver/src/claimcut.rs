//! skewsplit (D3320): a running claim cut for an idle worker. A pgrc section claims whole parts; when a filter or a
//! join keeps its rows in a few parts (a date-ordered table under a date or join filter), one claim carries the section
//! while the other workers idle (one claim 1,012 ms of a 1,033 ms pipeline at W8). Every part looks alike before the
//! scan, so no census can say which; the claim's own progress can. A worker that finds the section's cursor exhausted
//! (the runtime's `MorselSource::steal`) asks the running claim with the most time left (its granules left x the time
//! per granule it has taken so far) for its back half; the claim's owner answers at its next granule step (the pgrc
//! source's stage loop top, before any part-grain step reads its end), moves its end to the midpoint and hands
//! `[mid, end)` over. The owner moves its own end: the source has multi-granule steps (a whole part as one count.only
//! window, a part skipped whole) that a cut from outside could land inside. The part's dictionary and memo state is
//! then rebuilt only by workers that had nothing else to run.
//!
//! One `Cut` per pool ordinal, kept in the pool's own per-ordinal cell (no allocation here) and reused by every claim the
//! ordinal runs. The pool runs each claim inside `Board::run`, which begins the cut, offers it to the claim's node and
//! ends it when the claim returns. The section's `claim_on` enters it (`enter`: the thread's current cut for the claim's
//! length). Only the pipeline's driving range site's source adopts it (`Source::claim_cut`); the `CutToken` it keeps
//! steps the thread's current cut while that cut still runs the adopted claim, so a token never reaches a cut by itself.
//! A second range site in the claim forbids any further cut and follows a cut already made. No constant: the floor a
//! half must clear is the runtime's photo-finish floor (`t_min`), the shortest claim the runtime's own sizer hands out at
//! a section's end.
mod raw;

pub use raw::with_current;

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// No claim runs on the ordinal.
const IDLE: u32 = 0;
/// A claim runs that no source adopted (yet), or one a second range site forbade: never asked.
const WHOLE: u32 = 1;
/// A claim runs whose source adopted the cut: a worker may ask it.
const RUN: u32 = 2;
/// A worker asked for the claim's back half: the owner answers at its next granule step (or its claim's end).
const ASK: u32 = 3;
/// The owner answered (`gift`, empty = declined): the asker takes it and the state goes back to `RUN`.
const GIVEN: u32 = 4;

/// One ordinal's running claim, as an asker sees it.
pub struct Cut {
    state: AtomicU32,
    /// The claim as handed out (`begin`): what the adopting source's seek must match.
    lo: AtomicU64,
    first_hi: AtomicU64,
    /// The claim's current end (moved down by each cut) and the owner's next unit.
    hi: AtomicU64,
    at: AtomicU64,
    /// The units of the claim its owner skipped so far (zone maps, census, runtime filters): not read, so they do not set the
    /// claim's pace (D3247: a claim that skipped most of its part in microseconds still holds its kept granules' work).
    skipped: AtomicU64,
    /// The claim's start, `pg_clock::mono_ns`.
    t0: AtomicU64,
    /// The answer to an ask: `[gift_lo, gift_hi)`, empty when declined.
    gift_lo: AtomicU64,
    gift_hi: AtomicU64,
    /// The claim was cut at least once (a later range site follows the cut end).
    cut: AtomicBool,
    adopted: AtomicBool,
    /// The claims begun on this cut so far: the adopted claim's number is in its `CutToken`.
    claims: AtomicU64,
}

impl Default for Cut {
    fn default() -> Cut {
        Cut::new()
    }
}

impl AsRef<Cut> for Cut {
    fn as_ref(&self) -> &Cut {
        self
    }
}

impl Cut {
    pub const fn new() -> Cut {
        Cut { state: AtomicU32::new(IDLE), lo: AtomicU64::new(0), first_hi: AtomicU64::new(0), hi: AtomicU64::new(0), at: AtomicU64::new(0), skipped: AtomicU64::new(0), t0: AtomicU64::new(0), gift_lo: AtomicU64::new(0), gift_hi: AtomicU64::new(0), cut: AtomicBool::new(false), adopted: AtomicBool::new(false), claims: AtomicU64::new(0) }
    }

    /// The claim `[lo, hi)` begins on this ordinal: never asked until its source adopts it.
    fn begin(&self, lo: u64, hi: u64) {
        self.claims.store(self.claims.load(Ordering::Relaxed).wrapping_add(1), Ordering::Relaxed);
        self.lo.store(lo, Ordering::Relaxed);
        self.first_hi.store(hi, Ordering::Relaxed);
        self.hi.store(hi, Ordering::Relaxed);
        self.at.store(lo, Ordering::Relaxed);
        self.skipped.store(0, Ordering::Relaxed);
        self.t0.store(pg_clock::mono_ns(), Ordering::Relaxed);
        self.cut.store(false, Ordering::Relaxed);
        self.adopted.store(false, Ordering::Relaxed);
        self.state.store(WHOLE, Ordering::Release);
    }

    /// The claim as handed out, `(lo, hi)`.
    pub fn claim(&self) -> (u64, u64) {
        (self.lo.load(Ordering::Relaxed), self.first_hi.load(Ordering::Relaxed))
    }

    /// The claim's current end.
    pub fn end(&self) -> u64 {
        self.hi.load(Ordering::Acquire)
    }

    /// Whether the claim was cut (its end moved below the claim's).
    pub fn was_cut(&self) -> bool {
        self.cut.load(Ordering::Acquire)
    }

    /// The driving source adopts the claim (once per claim): from now on a worker may ask it, and the token steps it.
    pub fn adopt(&self) -> Option<CutToken> {
        (!self.adopted.swap(true, Ordering::AcqRel) && self.state.compare_exchange(WHOLE, RUN, Ordering::AcqRel, Ordering::Acquire).is_ok()).then(|| self.token())
    }

    fn token(&self) -> CutToken {
        CutToken { cut: self as *const Cut as usize, claim: self.claims.load(Ordering::Relaxed) }
    }

    /// No further cut of this claim (a second range site reads it, or the source re-seeks): a pending ask is declined.
    pub fn forbid(&self) {
        self.adopted.store(true, Ordering::Release);
        // bounded("other: an asker takes its answer within one of its waits (Board::wait, 200 us), then the state is RUN")
        loop {
            match self.state.load(Ordering::Acquire) {
                RUN => {
                    if self.state.compare_exchange(RUN, WHOLE, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                        return;
                    }
                }
                ASK => self.answer(None),
                GIVEN => std::thread::yield_now(),
                _ => return,
            }
        }
    }

    /// The owner's granule step: `at` is its next unit, `skipped` how many of the claim's units before it were skipped
    /// unread. When a worker asked, the owner's new end (the midpoint of what is left, the owner keeping the first half
    /// with the part it has open) is returned and `[mid, end)` is handed over; fewer than two units left is a decline. Two
    /// relaxed stores and one relaxed load when nobody asked.
    fn step(&self, at: u64, skipped: u64) -> Option<u64> {
        self.at.store(at, Ordering::Relaxed);
        self.skipped.store(skipped, Ordering::Relaxed);
        if self.state.load(Ordering::Relaxed) != ASK {
            return None;
        }
        let hi = self.hi.load(Ordering::Relaxed);
        if hi.saturating_sub(at) < 2 {
            self.answer(None);
            return None;
        }
        let mid = at + (hi - at).div_ceil(2);
        self.hi.store(mid, Ordering::Release);
        self.cut.store(true, Ordering::Release);
        self.answer(Some((mid, hi)));
        Some(mid)
    }

    fn answer(&self, gift: Option<(u64, u64)>) {
        let (lo, hi) = gift.unwrap_or((0, 0));
        self.gift_lo.store(lo, Ordering::Relaxed);
        self.gift_hi.store(hi, Ordering::Relaxed);
        self.state.store(GIVEN, Ordering::Release);
    }

    /// The claim ends: a pending ask is declined and an answer not yet taken waits for its asker.
    fn finish(&self) {
        // bounded("other: an asker takes its answer within one of its waits (Board::wait, 200 us), then the state is RUN")
        loop {
            match self.state.load(Ordering::Acquire) {
                s @ (WHOLE | RUN) => {
                    if self.state.compare_exchange(s, IDLE, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                        return;
                    }
                }
                ASK => self.answer(None),
                GIVEN => std::thread::yield_now(),
                _ => return,
            }
        }
    }

    /// The time this claim still needs, ns, by its own pace so far (units left x ns per unit READ: the units it skipped
    /// unread do not count, so a claim that has read none yet is paced by its whole elapsed time per unit), or `None` when
    /// it is not askable or holds fewer than two units. `PGRUST_GRAVITY_CLAIM_SPLIT=2` (measurement only) paces by every
    /// unit taken, skipped ones included (A2's first estimate).
    fn left_ns(&self, now: u64) -> Option<u64> {
        if self.state.load(Ordering::Acquire) != RUN {
            return None;
        }
        let (lo, at, hi) = (self.lo.load(Ordering::Relaxed), self.at.load(Ordering::Relaxed), self.hi.load(Ordering::Relaxed));
        let left = hi.saturating_sub(at);
        if left < 2 {
            return None;
        }
        let spent = now.saturating_sub(self.t0.load(Ordering::Relaxed));
        let taken = at.saturating_sub(lo);
        let read = if pace_all() { taken } else { taken.saturating_sub(self.skipped.load(Ordering::Relaxed)) };
        Some(((left as u128 * spent as u128) / read.max(1) as u128).min(u64::MAX as u128) as u64)
    }

    /// The asker's take of the owner's answer: `Some(Some(range))` handed over, `Some(None)` declined, `None` not yet.
    fn take(&self) -> Option<Option<(u64, u64)>> {
        if self.state.load(Ordering::Acquire) != GIVEN {
            return None;
        }
        let (lo, hi) = (self.gift_lo.load(Ordering::Relaxed), self.gift_hi.load(Ordering::Relaxed));
        self.state.store(RUN, Ordering::Release);
        Some((lo < hi).then_some((lo, hi)))
    }
}

/// The adopting source's hold on its claim: which cut and which of the cut's claims. It reaches the cut only as the
/// thread's current one (`with_current`), so a token kept past its claim steps nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CutToken {
    cut: usize,
    claim: u64,
}

impl CutToken {
    /// The owner's granule step (`at` its next unit, `skipped` the claim's units it skipped unread so far) while the
    /// thread still runs the adopted claim: the claim's new end when a worker asked (`[end, old end)` handed over), `None`
    /// otherwise. One thread-local read, two relaxed stores and one relaxed load when nobody asked.
    pub fn step(self, at: u64, skipped: u64) -> Option<u64> {
        with_current(|c| if c.token() == self { c.step(at, skipped) } else { None }).flatten()
    }
}

/// A section's split state: the floor a half must clear and the ranges handed over not yet begun by their taker. Its
/// cuts are the pool's (one per ordinal, `ask`'s `cuts`).
pub struct Board {
    /// A half must take at least this long, ns (the runtime's photo-finish floor).
    floor_ns: u64,
    /// Ranges handed over and not yet begun (a waiting asker counts them as pending work); the taker's thread holds its
    /// own as `CLAIM.gift` until its claim begins.
    pending: AtomicUsize,
    /// Ranges handed over, for the census.
    pub cuts_made: AtomicU64,
}

/// What one asking round found.
pub enum Ask {
    /// A range handed over: run it as a claim.
    Took(u64, u64),
    /// Nothing to ask now, but an ask or a handed range is pending: wait and ask again.
    Wait,
    /// No running claim is worth cutting: the section's end.
    Nothing,
}

impl Board {
    pub const fn new(floor_ns: u64) -> Board {
        Board { floor_ns, pending: AtomicUsize::new(0), cuts_made: AtomicU64::new(0) }
    }

    /// `f` runs the claim `[lo, hi)` on the ordinal whose cut is `cut`: the cut begins, is offered to the claim's node for
    /// `f`'s length, and ends (a pending ask declined) when `f` returns or unwinds. A range this thread was handed
    /// (`ask`) is no longer pending.
    pub fn run<R>(&self, cut: &Cut, lo: u64, hi: u64, f: impl FnOnce() -> R) -> R {
        cut.begin(lo, hi);
        let me = self as *const Board as usize;
        if CLAIM.with(|c| c.gift.replace(NO_GIFT)) == (me, lo, hi) {
            let _ = self.pending.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1));
        }
        let was = CLAIM.with(|c| c.offered.replace(cut as *const Cut));
        let _begun = Begun { cut, was };
        f()
    }

    /// One asking round over the section's cuts: the running claim with the most time left whose half clears the floor
    /// is asked; then wait for its owner's answer with `wait` between looks.
    pub fn ask<C: AsRef<Cut>>(&self, cuts: &[C], wait: &mut dyn FnMut()) -> Ask {
        let now = pg_clock::mono_ns();
        let best = cuts.iter().map(AsRef::as_ref).filter_map(|c| c.left_ns(now).filter(|&t| t / 2 >= self.floor_ns).map(|t| (t, c))).max_by_key(|&(t, _)| t);
        let Some((_, cut)) = best else {
            // a claim begun and not yet adopted (its source has not reached its seek: a range just handed over is one) may
            // become askable: wait for it rather than leave the section while it still holds the work
            let maybe = cuts.iter().map(AsRef::as_ref).any(|c| match c.state.load(Ordering::Acquire) {
                ASK | GIVEN => true,
                WHOLE => !c.adopted.load(Ordering::Acquire),
                _ => false,
            });
            if maybe || self.pending.load(Ordering::Acquire) > 0 {
                return Ask::Wait;
            }
            if trace() {
                // what the asker saw when it left: each running claim's state, units and pace
                for c in cuts.iter().map(AsRef::as_ref).filter(|c| c.state.load(Ordering::Relaxed) != IDLE) {
                    eprintln!("SPLITTRACE left: claim [{}, {}) at {} end {} state={} adopted={} after {} us", c.lo.load(Ordering::Relaxed), c.first_hi.load(Ordering::Relaxed), c.at.load(Ordering::Relaxed), c.hi.load(Ordering::Relaxed), c.state.load(Ordering::Relaxed), c.adopted.load(Ordering::Relaxed), now.saturating_sub(c.t0.load(Ordering::Relaxed)) / 1000); // alloc: Cold
                }
            }
            return Ask::Nothing;
        };
        if cut.state.compare_exchange(RUN, ASK, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return Ask::Wait;
        }
        // bounded("other: the owner answers at its next granule step or at its claim's end (Cut::finish), and every claim ends")
        loop {
            match cut.take() {
                Some(Some((lo, hi))) => {
                    if trace() {
                        eprintln!("SPLITTRACE took [{lo}, {hi}) from claim [{}, {}) at {} after {} us", cut.lo.load(Ordering::Relaxed), cut.first_hi.load(Ordering::Relaxed), cut.at.load(Ordering::Relaxed), now.saturating_sub(cut.t0.load(Ordering::Relaxed)) / 1000); // alloc: Cold
                    }
                    CLAIM.with(|c| c.gift.set((self as *const Board as usize, lo, hi)));
                    self.pending.fetch_add(1, Ordering::AcqRel);
                    self.cuts_made.fetch_add(1, Ordering::Relaxed);
                    return Ask::Took(lo, hi);
                }
                Some(None) => return Ask::Wait,
                None => wait(),
            }
        }
    }
}

/// A running claim's cut, offered to the claim's node until the claim ends (`Board::run`).
struct Begun<'a> {
    cut: &'a Cut,
    was: *const Cut,
}

impl Drop for Begun<'_> {
    fn drop(&mut self) {
        let mine = self.cut as *const Cut;
        CLAIM.with(|c| {
            c.offered.set(self.was);
            // no scope of this claim outlives it (the section's `Entered` drops inside the claim)
            if c.current.get() == mine {
                c.current.set(core::ptr::null());
            }
        });
        if trace() {
            let c = self.cut;
            eprintln!("SPLITTRACE end [{}, {}) -> {} adopted={} state={} cut={} {} us", c.lo.load(Ordering::Relaxed), c.first_hi.load(Ordering::Relaxed), c.hi.load(Ordering::Relaxed), c.adopted.load(Ordering::Relaxed), c.state.load(Ordering::Relaxed), c.cut.load(Ordering::Relaxed), pg_clock::mono_ns().saturating_sub(c.t0.load(Ordering::Relaxed)) / 1000); // alloc: Cold
        }
        self.cut.finish();
    }
}

/// The section's `claim_on` enters the cut its pool claim offered (taken once: a node nested inside the claim sees
/// none): the thread's current cut until the guard drops, the enclosing claim's back then. `adoptable`: the claim's
/// pipeline has a driving site; a claim without one is never asked (forbidden at once).
pub(crate) fn enter(adoptable: bool) -> Entered {
    let offered = CLAIM.with(|c| c.offered.replace(core::ptr::null()));
    let was = CLAIM.with(|c| c.current.replace(offered));
    let entered = Entered { was, on: !offered.is_null() };
    if entered.on && !adoptable {
        with_current(Cut::forbid);
    }
    entered
}

/// `f` inside the cut its pool claim offered, as the section's `claim_on` enters it (`enter`); `f` is told whether a
/// cut was offered. A pool test's job stands in for a source with it.
pub fn entered<R>(adoptable: bool, f: impl FnOnce(bool) -> R) -> R {
    let e = enter(adoptable);
    f(e.on)
}

/// An entered claim's cut (`enter`): the thread's current cut until the drop.
pub(crate) struct Entered {
    was: *const Cut,
    on: bool,
}

impl Entered {
    /// Whether the claim offered a cut.
    pub(crate) fn on(&self) -> bool {
        self.on
    }
}

impl Drop for Entered {
    fn drop(&mut self) {
        CLAIM.with(|c| c.current.set(self.was));
    }
}

/// The claim split's measurement switches, read once: `PGRUST_GRAVITY_SPLIT_TRACE=1` (a stderr line per range handed
/// over, per claim's end and per running claim an asker saw when it left) and `PGRUST_GRAVITY_CLAIM_SPLIT=2` (the split
/// on with A2's first pace, which counts skipped units as taken: the A/B arm against the read-units pace of the default, unset
/// or `=1`; `=0` turns the split off, exec `claim_split_on`).
fn flags() -> (bool, bool) {
    static ON: pgsync::OnceLock<(bool, bool)> = pgsync::OnceLock::new();
    *ON.get_or_init(|| (crate::env::num::<u64>("PGRUST_GRAVITY_SPLIT_TRACE").is_some_and(|v| v != 0), crate::env::num::<u64>("PGRUST_GRAVITY_CLAIM_SPLIT") == Some(2)))
}

fn trace() -> bool {
    flags().0
}

fn pace_all() -> bool {
    flags().1
}

/// This thread's pool-claim cuts (`CLAIM`).
struct Claim {
    /// The cut of the pool claim this thread is about to run (`Board::run`), until the claim's node enters it.
    offered: Cell<*const Cut>,
    /// The cut of the claim this thread's claim node runs (`enter`): the only cut a token steps.
    current: Cell<*const Cut>,
    /// The range `Board::ask` handed this thread (board, lo, hi), until this thread's next claim begins.
    gift: Cell<(usize, u64, u64)>,
}

const NO_GIFT: (usize, u64, u64) = (0, 0, 0);

std::thread_local! {
    /// skewsplit: this thread's pool-claim cuts: the offered one (put back by `Board::run`'s `Begun` drop), the entered
    /// one (put back by `Entered`'s drop, cleared by `Begun`'s) and the range handed to it (consumed by its next claim).
    static CLAIM: Claim = const { Claim { offered: Cell::new(core::ptr::null()), current: Cell::new(core::ptr::null()), gift: Cell::new(NO_GIFT) } };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adopt_current() -> Option<CutToken> {
        with_current(|c| c.adopt()).flatten()
    }

    #[test]
    fn an_asked_owner_keeps_the_first_half_and_hands_over_the_rest_once() {
        let b = Board::new(0);
        let cuts = [Cut::new(), Cut::new()];
        let cut = &cuts[0];
        let t = b.run(cut, 10, 20, || {
            entered(true, |on| {
                assert!(on);
                let t = adopt_current().expect("adopted");
                assert!(cut.adopt().is_none(), "one adoption per claim");
                assert_eq!(t.step(12, 0), None);
                // an asker on another thread; the owner answers at its next step; the taker then begins the range
                std::thread::scope(|s| {
                    let h = s.spawn(|| {
                        let a = b.ask(&cuts, &mut || std::thread::yield_now());
                        if let Ask::Took(lo, hi) = a {
                            assert_eq!(b.pending.load(Ordering::Acquire), 1);
                            b.run(&cuts[1], lo, hi, || assert_eq!(b.pending.load(Ordering::Acquire), 0, "begun: no longer pending"));
                        }
                        a
                    });
                    // bounded("test: until the asker's ask lands")
                    while cut.state.load(Ordering::Acquire) != ASK {
                        std::thread::yield_now();
                    }
                    assert_eq!(t.step(13, 0), Some(17));
                    match h.join().unwrap() {
                        Ask::Took(lo, hi) => assert_eq!((lo, hi), (17, 20)),
                        _ => panic!("no range taken"),
                    }
                });
                assert_eq!(cut.end(), 17);
                assert!(cut.was_cut());
                t
            })
        });
        assert_eq!(cut.state.load(Ordering::Acquire), IDLE);
        assert_eq!(t.step(14, 0), None, "outside its claim");
        b.run(cut, 20, 30, || entered(true, |_| assert_ne!(cut.token(), t, "a later claim on the same cut")));
    }

    #[test]
    fn a_claim_no_source_adopted_yet_is_waited_for_and_one_forbidden_is_never_asked() {
        let b = Board::new(0);
        let cuts = [Cut::new()];
        b.run(&cuts[0], 0, 100, || {
            assert!(matches!(b.ask(&cuts, &mut || ()), Ask::Wait), "begun, not adopted yet: it may become askable");
            entered(true, |_| {
                let t = adopt_current().expect("adopted");
                cuts[0].forbid();
                assert!(matches!(b.ask(&cuts, &mut || ()), Ask::Nothing), "forbidden");
                assert_eq!(t.step(5, 0), None);
            });
        });
        assert!(matches!(b.ask(&cuts, &mut || ()), Ask::Nothing), "no claim running");
        b.run(&cuts[0], 0, 100, || {
            entered(false, |on| {
                assert!(on);
                assert!(adopt_current().is_none(), "no driving site: forbidden at once");
                assert!(matches!(b.ask(&cuts, &mut || ()), Ask::Nothing));
            });
        });
        assert!(entered(true, |on| !on && adopt_current().is_none()), "no pool claim: no cut");
    }

    #[test]
    fn a_claim_ending_with_an_ask_pending_declines_it() {
        let b = Board::new(0);
        let cuts = [Cut::new()];
        let cut = &cuts[0];
        std::thread::scope(|s| {
            let h = b.run(cut, 0, 100, || {
                entered(true, |_| assert!(adopt_current().is_some()));
                let h = s.spawn(|| b.ask(&cuts, &mut || std::thread::yield_now()));
                // bounded("test: until the asker's ask lands")
                while cut.state.load(Ordering::Acquire) != ASK {
                    std::thread::yield_now();
                }
                h
            });
            assert!(matches!(h.join().unwrap(), Ask::Wait), "declined");
        });
        assert_eq!(cut.state.load(Ordering::Acquire), IDLE);
    }

    #[test]
    fn a_claim_is_paced_by_the_units_it_read_not_the_ones_it_skipped() {
        let cut = Cut::new();
        cut.begin(0, 100);
        assert!(cut.adopt().is_some());
        // 60 units taken in 1 ms, all of them skipped unread: the 40 left are paced by the whole millisecond
        cut.t0.store(0, Ordering::Relaxed);
        cut.step(60, 60);
        assert_eq!(cut.left_ns(1_000_000), Some(40_000_000));
        // the same 60 units all read: 40 x 1 ms / 60
        cut.step(60, 0);
        assert_eq!(cut.left_ns(1_000_000), Some(666_666));
        // 50 skipped, 10 read: 40 x 1 ms / 10
        cut.step(60, 50);
        assert_eq!(cut.left_ns(1_000_000), Some(4_000_000));
    }

    #[test]
    fn fewer_than_two_units_left_or_a_half_under_the_floor_is_not_asked() {
        let cuts = [Cut::new()];
        let b = Board::new(u64::MAX / 4);
        b.run(&cuts[0], 0, 4, || {
            entered(true, |_| {
                let t = adopt_current().expect("adopted");
                assert_eq!(t.step(1, 0), None);
                assert!(matches!(b.ask(&cuts, &mut || ()), Ask::Nothing), "under the floor");
            })
        });
        let b0 = Board::new(0);
        b0.run(&cuts[0], 0, 4, || {
            entered(true, |_| {
                let t = adopt_current().expect("adopted");
                assert_eq!(t.step(3, 0), None);
                assert!(matches!(b0.ask(&cuts, &mut || ()), Ask::Nothing), "one unit left");
            })
        });
    }
}
