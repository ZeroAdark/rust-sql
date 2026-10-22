//! Re-entrancy (docs/gravity/entry.md §8): the backend state is a stack of levels, one per served statement running
//! on this thread; a statement started inside a running program's host call takes the next level (its own node
//! pool, exec slots, memo and program instances: host state is never shared by two running executions).

use std::cell::{Cell, RefCell};
use std::mem::ManuallyDrop;
use std::rc::Rc;

use crate::Backend;

/// Statement levels on one thread whose backend state is kept for the session (a served statement whose SQL function or
/// trigger runs another takes the next): one static slot per level. Deeper levels are built on demand (`Deep`) and freed
/// when the run of the statement below them ends (D3881: `Above`), as PostgreSQL frees a callee's state with its caller's
/// executor state; recursion is bounded by `check_stack_depth` (the entry's start), as PostgreSQL's is.
pub const MAX_DEPTH: usize = gravity_program::cost::entry::ENTRY_LEVELS_MAX;
pub const AWAY_REFUSAL: &str = "entry: a cursor over a program with host calls fetched inside a statement of the level it was opened at";

type Level = RefCell<ManuallyDrop<Option<Box<Backend>>>>;
pub type Build = fn(usize) -> Box<Backend>;

/// D3360: a level past the static ones (`MAX_DEPTH + i` for `DEEP[i]`), with that level's flush mark, pending drops and
/// retired admissions (D3346; last, so a dropped deep level's parked Nodes go before the admissions they were built over).
struct Deep {
    level: Level,
    flush_seen: Cell<u64>,
    pending: RefCell<Vec<gravity_program::Handle>>,
    retired: Retired,
}

type Retired = RefCell<ManuallyDrop<Vec<(gravity_program::PlanKey, Box<crate::Admitted>)>>>;

thread_local! {
    static LEVELS: [Level; MAX_DEPTH] = const { [const { RefCell::new(ManuallyDrop::new(None)) }; MAX_DEPTH] };
    /// D3098: per level, the plansources dropped while it ran (`drop_handle`), dropped at its next borrow (`enter`).
    static PENDING: [RefCell<Vec<gravity_program::Handle>>; MAX_DEPTH] = const { [const { RefCell::new(Vec::new()) }; MAX_DEPTH] }; // alloc: Exec
    /// D3360: the levels past `MAX_DEPTH`, in depth order; no destructor, as `LEVELS` has none (`release_thread_mappings`).
    static DEEP: ManuallyDrop<RefCell<Vec<Rc<Deep>>>> = const { ManuallyDrop::new(RefCell::new(Vec::new())) }; // alloc: Exec
}

leader_tls::leader_tls! {
    thread_local! {
        static FLUSH_GEN: Cell<u64> = const { Cell::new(0) };
        static FLUSH_SEEN: [Cell<u64>; MAX_DEPTH] = const { [const { Cell::new(0) }; MAX_DEPTH] };
        /// D3346: per level, the admissions dropped while it ran (`retire`), each with its key: at its next borrow the key's
        /// parked Nodes drop first, then the admission (`Backend::drop_admission`). ManuallyDrop as `LEVELS`: never dropped
        /// by the thread's TLS destructor, after the contexts they reach.
        static RETIRED: [Retired; MAX_DEPTH] = const { [const { RefCell::new(ManuallyDrop::new(Vec::new())) }; MAX_DEPTH] }; // alloc: Exec
        static DEEPEST: Cell<usize> = const { Cell::new(0) };
        /// D3465: the index in `DEEP` a deep level's search starts at: one past the deep level the innermost deep `enter`
        /// runs at. Every deep level below it is held by a frame outside that run (a level is held only by a running `enter`
        /// or its held home), so the search finds the level a scan from `DEEP[0]` would, in O(1) per level instead of
        /// O(depth) (a recursion N deep paid O(N^2)).
        static DEEP_FROM: Cell<usize> = const { Cell::new(0) };
        /// D3465: deep levels looked at by `enter_deep` on this thread (`deep_probes`: the test that pins O(1) per level).
        static DEEP_PROBES: Cell<u64> = const { Cell::new(0) };
        /// The level of the innermost `enter` running on this thread (D622: the grants `knobs::grant::hand` gave its statement).
        static CURRENT: Cell<Option<usize>> = const { Cell::new(None) };
        /// D3881: a deep level outlived its caller's run (a statement still homed there, a home run, an unwind): the next
        /// top-level `enter` frees what is idle by then.
        static LINGER: Cell<bool> = const { Cell::new(false) };
    }
}

/// D3881: a deep level held by a statement homed there (its `Serve`: started, running, shelved or parked), so the level
/// outlives its caller's run while that statement lives; holds nothing for a static level.
pub struct Pin(Option<Rc<Deep>>);

impl Pin {
    pub const NONE: Pin = Pin(None);
}

/// D3881: the pin of a statement homed at level `home`.
pub fn pin(home: usize) -> Pin {
    Pin(deep_at(home))
}

/// D3881: the end of a run at level `.0` (normal, or an ERROR returned through it): the deep levels above it ran only
/// statements nested in that run, so each one idle and unpinned is freed with what is shelved there. An unwind leaves them
/// to the next top-level `enter`.
struct Above(usize);

impl Drop for Above {
    #[inline]
    fn drop(&mut self) {
        let from = (self.0 + 1).saturating_sub(MAX_DEPTH);
        if DEEP.with(|d| d.borrow().len()) > from {
            if std::thread::panicking() {
                LINGER.set(true);
            } else {
                free_above(from);
            }
        }
    }
}

/// The levels a pin or an unwind left, freed (`free_deep`) at the next top-level statement.
#[cold]
#[inline(never)]
fn free_lingering() {
    free_deep(0);
    super::kept::abort_settled();
}

/// D3881: an abort: the levels its statements left go at the next top-level statement, nothing of them kept (`kept`).
pub(crate) fn linger() {
    LINGER.set(true);
}

#[inline(never)]
fn free_above(from: usize) {
    super::entry::retire_shelved_from(MAX_DEPTH + from);
    free_deep(from);
}

/// D3881: every deep level from `DEEP[from]` that no running statement, home run or pin holds and no live portal's exec slot
/// uses gives its backend state back (its retired admissions after their parked Nodes, as `trim_deep`; its filed ones,
/// with their Nodes, to the thread's pool, `kept`); the freed tail
/// leaves `DEEP`, and the level-indexed thread state shrinks with it.
fn free_deep(from: usize) {
    let mut kept = false;
    let mut i = from;
    while let Some(x) = DEEP.with(|d| d.borrow().get(i).cloned()) {
        let depth = MAX_DEPTH + i;
        i += 1;
        // `DEEP`'s reference and `x`: no pin, home run or `enter_deep` holds it
        if Rc::strong_count(&x) > 2 || crate::runner::away_at(depth) {
            kept = true;
            continue;
        }
        let Ok(mut g) = x.level.try_borrow_mut() else {
            kept = true;
            continue;
        };
        if let Some(b) = g.as_mut() {
            if b.named_home != 0 || b.execs.iter().flatten().any(|s| s.is_live()) {
                kept = true;
                continue;
            }
            let retired: Vec<_> = x.retired.borrow_mut().drain(..).collect(); // alloc: Exec
            retired.into_iter().for_each(|(key, a)| b.drop_admission(key, a));
            // filed since the last flush: a flush during its run left them stale
            if x.flush_seen.get() == FLUSH_GEN.get() && x.pending.borrow().is_empty() {
                super::kept::keep_level(b);
            }
        }
        let b = g.take();
        drop(g);
        if let Some(b) = b {
            super::kept::shelve(b);
        }
        drop(core::mem::take(&mut **x.retired.borrow_mut()));
        x.pending.borrow_mut().clear();
    }
    let levels = DEEP.with(|d| {
        let mut d = d.borrow_mut();
        let keep = d.iter().rposition(|x| Rc::strong_count(x) > 1 || x.level.try_borrow().map_or(true, |g| g.is_some())).map_or(0, |i| i + 1);
        d.truncate(keep);
        MAX_DEPTH + keep
    });
    LINGER.set(kept || (from > 0 && LINGER.get()));
    gravity_kernels::ops::sort::trim_levels(levels);
    gravity_fn::srf::trim_levels(levels);
}

/// The first level no running statement holds, built on demand; `Ok(None)` = not built and no `build`.
/// `home` = the level the statement was admitted at (its program instances are that level's): run away from
/// it, the home level is held too while idle, and refused by name while it runs when the programs have host
/// state (`hosted`), since the running statement may be inside the same instance.
pub fn enter<R>(build: Option<Build>, home: Option<usize>, hosted: bool, f: impl FnOnce(&mut Backend, usize) -> R) -> Result<Option<R>, &'static str> {
    // D3620: the run is no closure of the thread-local's `with` (whose frame would sit on every nested statement's stack
    // path): the levels have no destructor (each is `ManuallyDrop`), so the run takes them by reference outside it
    let levels = super::raw::thread_ref(&LEVELS);
    let levels: &[Level; MAX_DEPTH] = &levels;
    // D3763: on wasm32 the level's frames sit on the engine's own stack the guard does not read; it is charged for them
    let _charge = stack_depth_core::EngineLevel::enter();
    // D3881: a top-level entry frees the deep levels a pin or an unwind left behind
    if LINGER.get() && levels[0].try_borrow_mut().is_ok() {
        free_lingering();
    }
    match levels.iter().position(|l| l.try_borrow_mut().is_ok()) {
        Some(depth) => enter_static(levels, depth, build, home, hosted, f),
        None => enter_deep(levels, build, home, hosted, f),
    }
}

/// `enter` at static level `depth`, which no running statement holds.
#[inline(never)]
fn enter_static<R>(levels: &[Level; MAX_DEPTH], depth: usize, build: Option<Build>, home: Option<usize>, hosted: bool, f: impl FnOnce(&mut Backend, usize) -> R) -> Result<Option<R>, &'static str> {
    // free: `enter`'s search found it so on this thread, nothing between
    let mut guard = levels[depth].borrow_mut();
    if guard.is_none() {
        let Some(build) = build else { return Ok(None) };
        **guard = Some(build(depth));
    }
    let home_deep = home.filter(|&h| h != depth).and_then(deep_at);
    let _held = hold_home(levels, home_deep.as_deref(), home, depth, hosted)?;
    let _above = Above(depth);
    let out = FLUSH_SEEN.with(|s| PENDING.with(|p| RETIRED.with(|r| run_at(&mut guard, depth, &s[depth], &p[depth], &r[depth], f))));
    Ok(Some(out))
}

/// D3360: `enter` past the static levels: the first deep level no running statement holds, appended when every one is.
/// Its admission arena is unmapped again after each borrow (an admission's scratch: `admit::graph` resets it), so the
/// pool keeps no arena's hold per depth.
#[inline(never)]
fn enter_deep<R>(levels: &[Level; MAX_DEPTH], build: Option<Build>, home: Option<usize>, hosted: bool, f: impl FnOnce(&mut Backend, usize) -> R) -> Result<Option<R>, &'static str> {
    let mut i = DEEP_FROM.get().min(DEEP.with(|d| d.borrow().len()));
    loop {
        DEEP_PROBES.set(DEEP_PROBES.get() + 1);
        let depth = MAX_DEPTH + i;
        let deep = match deep_at(depth) {
            Some(d) => d,
            None if build.is_none() => return Ok(None),
            None => new_deep(),
        };
        let Ok(mut guard) = deep.level.try_borrow_mut() else {
            i += 1;
            continue;
        };
        if guard.is_none() {
            let Some(build) = build else { return Ok(None) };
            // D3881: a freed deep level's emptied state when one is shelved
            **guard = Some(super::kept::shell().unwrap_or_else(|| build(depth)));
            // D3881: a level built now has nothing filed for a flush to drop
            deep.flush_seen.set(FLUSH_GEN.get());
        }
        let home_deep = home.filter(|&h| h != depth).and_then(deep_at);
        let _held = hold_home(levels, home_deep.as_deref(), home, depth, hosted)?;
        let _from = DeepFrom(DEEP_FROM.replace(i + 1));
        let _above = Above(depth);
        let out = run_at(&mut guard, depth, &deep.flush_seen, &deep.pending, &deep.retired, f);
        if let Some(b) = guard.as_mut() {
            b.release_graph_arena();
        }
        return Ok(Some(out));
    }
}

/// `enter_deep`'s next deep level, appended to `DEEP`. D3620: out of line, so `enter_deep`'s frame (on every nested
/// statement's stack path past the static levels) does not hold its build.
#[inline(never)]
fn new_deep() -> Rc<Deep> {
    let d = Rc::new(Deep { level: RefCell::new(ManuallyDrop::new(None)), flush_seen: Cell::new(0), pending: RefCell::new(Vec::new()), retired: RefCell::new(ManuallyDrop::new(Vec::new())) }); // alloc: Exec
    DEEP.with(|v| v.borrow_mut().push(d.clone())); // alloc: Exec
    d
}

/// D3465: `DEEP_FROM` back to the outer run's value when a deep level's run ends (an error unwinding through it too).
struct DeepFrom(usize);

impl Drop for DeepFrom {
    fn drop(&mut self) {
        DEEP_FROM.set(self.0);
    }
}

fn deep_at(depth: usize) -> Option<Rc<Deep>> {
    depth.checked_sub(MAX_DEPTH).and_then(|i| DEEP.with(|d| d.borrow().get(i).cloned()))
}

/// The home level held for a run away from it (`enter`'s `home`): a static level, or the deep one `deep` (`deep_at(home)`).
fn hold_home<'a>(levels: &'a [Level; MAX_DEPTH], deep: Option<&'a Deep>, home: Option<usize>, depth: usize, hosted: bool) -> Result<Option<std::cell::RefMut<'a, ManuallyDrop<Option<Box<Backend>>>>>, &'static str> {
    let Some(h) = home.filter(|&h| h != depth) else { return Ok(None) };
    let level = match (levels.get(h), deep) {
        (Some(l), _) => l,
        (None, Some(d)) => &d.level,
        (None, None) => return Ok(None),
    };
    match level.try_borrow_mut() {
        Ok(g) => Ok(Some(g)),
        Err(_) if hosted => Err(AWAY_REFUSAL),
        Err(_) => Ok(None),
    }
}

/// `run_at`'s level settled before its statement runs: the flushes, plansource drops and retired admissions since the
/// level last ran, a catalog change under its refusals, its named portals. D3620: out of line, so `run_at`'s frame (on every nested
/// statement's stack path) does not hold its locals.
#[inline(never)]
fn settle(backend: &mut Backend, depth: usize, flush_seen: &Cell<u64>, pending: &RefCell<Vec<gravity_program::Handle>>, retired: &Retired) {
    let generation = FLUSH_GEN.get();
    if flush_seen.replace(generation) != generation {
        backend.drop_plans(None);
    }
    // D3098: the plansources dropped while this level ran, each alone (never the whole level)
    if !pending.borrow().is_empty() {
        let mut p = pending.borrow_mut();
        p.drain(..).for_each(|h| backend.drop_plans(Some(h)));
    }
    // D3346: the admissions retired while this level ran, after the Nodes parked under their keys
    if !retired.borrow().is_empty() {
        let gone: Vec<_> = retired.borrow_mut().drain(..).collect(); // alloc: Exec
        gone.into_iter().for_each(|(key, a)| backend.drop_admission(key, a));
    }
    // D2663, C3 S4c: a catalog row a refusal of this level read moved (a filed plan answers for itself: `catalog_stale`)
    backend.follow_refusals();
    // D3547: the level's live named portals go away before its statement holds it (one bit test when none), so a
    // statement nested in this one may still FETCH, MOVE or CLOSE them (runner portal.rs)
    crate::runner::send_away(backend, depth);
}

// D3620: inlined into `enter_static` / `enter_deep`, so a statement's stack path holds one frame for both
#[inline(always)]
fn run_at<R>(guard: &mut ManuallyDrop<Option<Box<Backend>>>, depth: usize, flush_seen: &Cell<u64>, pending: &RefCell<Vec<gravity_program::Handle>>, retired: &Retired, f: impl FnOnce(&mut Backend, usize) -> R) -> R {
    let backend = guard.as_mut().expect("built");
    settle(backend, depth, flush_seen, pending, retired);
    DEEPEST.set(DEEPEST.get().max(depth));
    let outer = CURRENT.replace(Some(depth));
    // D3375 N7: a statement nested in a compare (a SQL / PL comparator's own) may sort on this thread; the compare loop
    // under it gets its sort state back (kernels rowsort `SortThreadState`)
    // (D3620: kept in the level's slot, not in this frame)
    gravity_kernels::ops::sort::save_at(depth);
    let _level = AtLevel { outer, depth };
    // the run's stashed errors are its own (gravity_hosts `StashScope`): an outer level's wait aside, and one this run
    // left (an unwind) goes at its end
    let _stash = gravity_hosts::StashScope::enter();
    // M5 E1: the level's own SRF slot bank (gravity_fn srf.rs `at_level`)
    gravity_fn::srf::at_level(depth, || f(backend, depth))
}

/// `run_at`'s end on any exit (an unwind too: a statement nested in a compare that a PL exception block catches): the
/// compare loop's sort state back from the level's slot, and the outer level current again.
struct AtLevel {
    outer: Option<usize>,
    depth: usize,
}

impl Drop for AtLevel {
    #[inline]
    fn drop(&mut self) {
        gravity_kernels::ops::sort::restore_at(self.depth);
        CURRENT.set(self.outer);
    }
}

/// D2355: level `level` runs a statement now (its state is borrowed): `enter` with it as the home of a hosted program
/// would be refused (`AWAY_REFUSAL`). Asked before a run starts its receiver, so the refusal may still fall back.
pub fn running(level: usize) -> bool {
    match LEVELS.with(|levels| levels.get(level).map(|l| l.try_borrow_mut().is_err())) {
        Some(busy) => busy,
        None => deep_at(level).is_some_and(|d| d.level.try_borrow_mut().is_err()),
    }
}

pub fn built(depth: usize) -> Box<Backend> {
    crate::ensure_session_end();
    // D3881: a deep level is built at every nested call of a deep recursion: timed only when its line prints
    let t0 = (depth < MAX_DEPTH || crate::deep_report()).then(std::time::Instant::now);
    let b = Box::new(Backend::for_level(depth)); // alloc: Cold
    crate::inputs::knobs::grant::arm_grant_pool();
    if let Some(t0) = t0.filter(|_| b.reporting()) {
        let us = t0.elapsed().as_micros();
        b.report_line(&format!("gravity: backend state built in {us} us (level {depth}: first serve at this depth)")); // alloc: Cold
    }
    b
}

pub fn depth() -> usize {
    let busy = LEVELS.with(|levels| levels.iter().filter(|l| l.try_borrow_mut().is_err()).count());
    busy + DEEP.with(|d| d.borrow().iter().filter(|x| x.level.try_borrow_mut().is_err()).count())
}

/// D3465: some level runs a statement now (`depth() > 0`, stopping at the first busy level: the entry asks it for
/// every statement, and in a recursion N deep `depth` counts N levels).
pub fn nested() -> bool {
    LEVELS.with(|levels| levels.iter().any(|l| l.try_borrow_mut().is_err()))
        || DEEP.with(|d| d.borrow().iter().any(|x| x.level.try_borrow_mut().is_err()))
}

/// The level of the innermost statement running on this thread (`enter`'s), `None` outside every level.
pub fn current() -> Option<usize> {
    CURRENT.get()
}

/// D3465: the deep levels `enter` has looked at on this thread so far (one per deep statement when no level is skipped).
pub fn deep_probes() -> u64 {
    DEEP_PROBES.get()
}

/// The deepest level this session served at (a test pins that a nested body was served nested).
pub fn deepest() -> usize {
    DEEPEST.get()
}

/// Every idle level (invalidation); the count of running ones, which `flush_later` reaches at their next borrow.
pub fn for_each_idle(mut f: impl FnMut(&mut Backend)) -> usize {
    let mut busy = 0;
    each_level(|level, _| match level.try_borrow_mut() {
        Ok(mut g) => {
            if let Some(b) = g.as_mut() {
                f(b);
            }
        }
        Err(_) => busy += 1,
    });
    busy
}

/// Every level with its pending drops: the static ones, then the deep ones (D3360) in depth order.
fn each_level(mut f: impl FnMut(&Level, &RefCell<Vec<gravity_program::Handle>>)) {
    LEVELS.with(|levels| PENDING.with(|pending| levels.iter().zip(pending.iter()).for_each(|(l, p)| f(l, p))));
    let mut i = 0;
    while let Some(d) = DEEP.with(|v| v.borrow().get(i).cloned()) {
        f(&d.level, &d.pending);
        i += 1;
    }
}

/// D3346 (D2190's rule wherever an admission drops): admission `a` of `key`, admitted at level `home`, dropped away from that
/// level's borrow (a shelved serve at the statement's or the session's end, a keyed release off its home): the Nodes parked
/// under the key at `home` go first, since one may be built over this admission (its recipes read its parameter copy), then
/// the admission. A home level running a statement now keeps it until its next borrow (`enter`); a level never built
/// parks no Node.
pub fn retire(home: usize, key: gravity_program::PlanKey, a: Box<crate::Admitted>) {
    let parked = match deep_at(home) {
        Some(d) => retire_at(&d.level, &d.retired, key, a),
        None => LEVELS.with(|levels| match levels.get(home) {
            Some(level) => RETIRED.with(|r| retire_at(level, &r[home], key, a)),
            None => Some(a),
        }),
    };
    drop(parked);
}

/// D3881: `f` over level `depth`'s state when it is built and no statement runs there (`None` otherwise).
pub(crate) fn at_idle<R>(depth: usize, f: impl FnOnce(&mut Backend) -> R) -> Option<R> {
    let run = |level: &Level| level.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(|b| f(b)));
    match deep_at(depth) {
        Some(d) => run(&d.level),
        None => LEVELS.with(|levels| levels.get(depth).and_then(run)),
    }
}

/// `retire` at one level (static or deep, D3360): `Some(a)` when the level was never built (no Node is parked there).
fn retire_at(level: &Level, retired: &Retired, key: gravity_program::PlanKey, a: Box<crate::Admitted>) -> Option<Box<crate::Admitted>> {
    match level.try_borrow_mut() {
        Ok(mut g) => match g.as_mut() {
            Some(b) => {
                b.drop_admission(key, a);
                None
            }
            None => Some(a),
        },
        Err(_) => {
            retired.borrow_mut().push((key, a)); // alloc: Exec
            None
        }
    }
}

/// D3346: the Nodes parked under plan handle `h` at idle level `home` dropped (a running level's go at their key's next
/// take: `Entry::acquire` tears down a Node another admission built).
pub fn purge(home: usize, h: gravity_program::Handle) {
    let at = |level: &Level| {
        if let Ok(mut g) = level.try_borrow_mut() {
            if let Some(b) = g.as_mut() {
                b.entry.park().drop_for(h);
            }
        }
    };
    match deep_at(home) {
        Some(d) => at(&d.level),
        None => LEVELS.with(|levels| levels.get(home).into_iter().for_each(at)),
    }
}

pub fn flush_later() {
    FLUSH_GEN.set(FLUSH_GEN.get() + 1);
}

/// D3881: the flush generation (`flush_later`), which the levels' shared filings follow (admit `LEVEL_TIER`).
pub fn flush_gen() -> u64 {
    FLUSH_GEN.get()
}

/// D3098: plansource `h` dropped (DropCachedPlan): each idle level drops its entries for `h` now, and a running level at its
/// next borrow. Only `h`'s: a plpgsql EXECUTE drops its one-shot plan on every call while the calling statement's level
/// runs, and a whole flush there threw away every program of that level each call (rt_app-027x +2.5 ms a statement).
pub fn drop_handle(h: gravity_program::Handle) {
    let mut overflow = false;
    each_level(|level, p| match level.try_borrow_mut() {
        Ok(mut g) => {
            if let Some(b) = g.as_mut() {
                b.drop_plans(Some(h));
            }
        }
        Err(_) => {
            let mut p = p.borrow_mut();
            if p.len() < gravity_program::cost::entry::ENTRY_PENDING_DROPS_MAX {
                p.push(h); // alloc: Exec
            } else {
                overflow = true;
            }
        }
    });
    if overflow {
        flush_later();
    }
}

/// D656: at the end of a thread whose levels were never torn down (a crash / kill-class exit), each built level's admission
/// arena and its pool hold go (`Backend::release_graph_arena`); the levels stay `ManuallyDrop`. A thread-local destructor
/// calls it (lib.rs `ArenaExit`): `LEVELS` has no destructor of its own, so it is still there; a level still borrowed is skipped.
pub fn release_thread_mappings() {
    let _ = LEVELS.try_with(|levels| {
        for level in levels.iter() {
            if let Ok(mut g) = level.try_borrow_mut() {
                if let Some(b) = g.as_mut() {
                    b.release_graph_arena();
                }
            }
        }
    });
    let _ = DEEP.try_with(|d| {
        for x in d.borrow().iter() {
            if let Ok(mut g) = x.level.try_borrow_mut() {
                if let Some(b) = g.as_mut() {
                    b.release_graph_arena();
                }
            }
        }
    });
}

pub fn teardown() {
    // D3547: the portals sent away go before the levels whose memos lent their admissions
    crate::runner::drain_away();
    super::level_drop();
    super::kept::session_end();
    LEVELS.with(|levels| {
        for level in levels.iter() {
            if let Ok(mut g) = level.try_borrow_mut() {
                // D3598: a portal's exec slot still live here missed its PortalDrop: left, never dropped (runner `forget_live`)
                if let Some(b) = g.as_mut() {
                    crate::runner::forget_live(b);
                }
                **g = None;
            }
        }
    });
    trim_deep();
}

/// D3360: the deep levels' backend state dropped and the list emptied (the session's end, as `teardown` drops the static
/// levels'); a level still borrowed is kept, with every one above it.
fn trim_deep() {
    let gone = DEEP.with(|d| {
        let mut d = d.borrow_mut();
        let keep = d.iter().rposition(|x| x.level.try_borrow_mut().is_err() || Rc::strong_count(x) > 1).map_or(0, |i| i + 1);
        match keep {
            0 => core::mem::take(&mut *d),
            k => d.split_off(k), // alloc: Cold
        }
    });
    for x in &gone {
        if let Ok(mut g) = x.level.try_borrow_mut() {
            // D3346: its retired admissions, after the Nodes parked under their keys (the list is ManuallyDrop)
            if let Some(b) = g.as_mut() {
                let retired: Vec<_> = x.retired.borrow_mut().drain(..).collect(); // alloc: Cold
                retired.into_iter().for_each(|(key, a)| b.drop_admission(key, a));
            }
            **g = None;
        }
    }
}
