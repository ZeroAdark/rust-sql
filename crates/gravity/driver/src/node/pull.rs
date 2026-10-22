//! Runs out of order inside a call: pulled sets, the hold and reattach of the call they run inside, nesting (split out
//! of `node/mod.rs`, contract-runtime landing 3).

use super::*;

/// M8 (RT-1): what `Node::nest` saves of the leader call a nested run interrupts (`Node::unnest` puts it back).
pub(crate) struct Nest {
    flags: u32,
    cap: i64,
}

/// D824: what `Node::hold` keeps of a pipeline stopped on `BuildWait` (`Node::reattach` puts it back).
/// M8 (RT-2f): a pulled member set (`Entry::pull_nested`), keyed by its last member.
pub(crate) struct Pull {
    last: u16,
    /// The set's producers (every member but the last) already run whole this execution: never run again.
    ran: Vec<u16>,
    phase: PullPhase,
    /// setop_rec-0043: the bases of the chunks this set's pulls kept pinned past their reader that no set's run files
    /// (`reattach_pull`), unpinned when a rerun set re-arms the pull (`pull_rearm`).
    pins: Vec<usize>,
    /// D3244: the arena window its parked member left inside a chunk filed in `pins` (`reattach_pull`); its next resume
    /// bumps on in it (`unpark_pull` takes it), else each resume's first scratch draw took a fresh chunk kept to the
    /// statement's end. `resumed_in`: the end of the window that resume started in.
    win: Option<(*mut u8, *mut u8)>,
    resumed_in: Option<*mut u8>,
}

/// M8 (RT-2f): where a pulled set's last member stands.
pub(crate) enum PullPhase {
    /// Not started, or running now (its call on the stack).
    Fresh,
    /// Stopped resumably after its budget: the call's resume word and claim, and the sites its call left live (kept out of
    /// every open's L8 sweep until the next pull resumes the call).
    Parked { resume: i64, claim: MorselRange, fill: (usize, *mut u8), sites: Vec<i16> },
    /// Ran to its end: every later pull answers `Pulled::Done` (sticky).
    Done,
    /// A pull raised: every later pull is an error by name.
    Failed,
}

pub(crate) struct Hold {
    resume: i64,
    claim: MorselRange,
    arena: (*mut u8, *mut u8),
    scratch: (*mut u8, *mut u8),
    chunks: (usize, usize),
    fill: (usize, *mut u8),
    /// `held_sites`' length before this hold's sites (`reattach` truncates back to it).
    sites: usize,
    /// The held call's `Node::pin_floor`.
    floor: usize,
}

/// D2344 / D2379: the arena chunks a nested run left, kept at its reattach. `hold` set `pinned` to the held call's
/// `chunk_next`, so the nested run's pins are chunks `[held next, nested_end)`: one-owner regions (a group table's kept key
/// copies, an owner region) that their readers use after the run, and for a parked member also the chunks its resume reads.
/// Restoring the held `(chunk_next, pinned)` put them back under the held call's window, whose next draw reused them
/// (m6_agg_final's by-ref scratch overwrote a CTE group's key copies). They move down behind the held call's own pins and
/// the held window's chunks move up past them. Returns the new `(chunk_next, pinned)`.
pub(crate) fn keep_nested<T>(chunks: &mut [T], held: (usize, usize), nested_end: usize) -> (usize, usize) {
    let (next, pinned) = held;
    let k = nested_end.saturating_sub(next);
    if k > 0 {
        chunks[pinned..next + k].rotate_right(k);
    }
    (next + k, pinned + k)
}

/// D2641: rerun set pins from the set's last run that lie under the running call's floor, `stale` (a pull's reattach kept them
/// under the reader's pins; `rerun_keep_begin` cannot move them: every held call counts the chunks below its floor), at the
/// set's next `reattach`: each trades places with one of this run's pins `[from, from + kept)`, so the live new pin sits
/// where the stale one was (no count changes), then the stale chunks leave the pinned prefix for the free tail at the
/// cursor, as `rerun_keep_begin` moves a chunk at or over the floor. Returns the stale bases left (more than this run's pins;
/// a base no longer pinned is dropped) and the new `(chunk_next, pinned)`.
pub(crate) fn trade_stale<T>(chunks: &mut Vec<T>, base: impl Fn(&T) -> usize, stale: &[usize], from: usize, kept: usize, at: (usize, usize)) -> (Vec<usize>, (usize, usize)) {
    let (mut next, mut pinned) = at;
    let (mut traded, mut left) = (0, Vec::new()); // alloc: Exec
    for &p in stale {
        match chunks[..from].iter().position(|c| base(c) == p) {
            Some(b) if traded < kept => {
                chunks.swap(b, from + traded);
                traded += 1;
            }
            Some(_) => left.push(p), // alloc: Exec
            None => {}
        }
    }
    for _ in 0..traded {
        let c = chunks.remove(from);
        (next, pinned) = (next - 1, pinned - 1);
        chunks.insert(next, c); // alloc: Exec
    }
    (left, (next, pinned))
}

/// D2751: of chunk bases `bases` (what a pull's reattach kept past its reader, `reattach_pull`), those no rerun set holds
/// filed (`sets`: `Node::rerun_pins`); only these become the pull's. A set's filed pin (a recursive term's owner region,
/// filed by the set's run inside the pull) stays the set's, so the set's next run unpins it or trades it from under its
/// floor (`trade_stale`, D2641); taken as the pull's, it stayed pinned to the statement's end: one chunk per generation.
pub(crate) fn unheld(bases: Vec<usize>, sets: &[(u16, Vec<usize>)]) -> Vec<usize> {
    bases.into_iter().filter(|b| !sets.iter().any(|(_, l)| l.contains(b))).collect() // alloc: Exec
}

#[cfg(test)]
pub(crate) mod d2641_trade_stale {
    use super::{keep_nested, trade_stale};

    fn id(c: &usize) -> usize {
        *c
    }

    /// Chunks as bases: 1 the reader's pin, 2 the set's last pin (kept under the reader's pins by a pull's reattach), 3 4 the
    /// reader's window, 5 6 the driver's (the held call's) window at the set's reattach, 7 the set's new pin, 8 free.
    #[test]
    fn a_stale_pin_under_the_floor_trades_places_with_the_new_pin_and_goes_to_the_free_tail() {
        // after the set's run: hold (chunk_next, pinned) = (6, 4); the run pinned chunk 7, which keep_nested moves behind
        // the held pins
        let mut c = vec![1, 2, 3, 4, 5, 6, 7, 8]; // alloc: Cold
        let (next, pinned) = keep_nested(&mut c, (6, 4), 7);
        assert_eq!((next, pinned), (7, 5));
        assert_eq!(c, [1, 2, 3, 4, 7, 5, 6, 8]);
        let (left, (next, pinned)) = trade_stale(&mut c, id, &[2], 4, 1, (next, pinned));
        assert!(left.is_empty());
        // the new pin 7 sits where 2 was (every count under the floor unchanged), 2 is the first free chunk past the window
        assert_eq!((next, pinned), (6, 4));
        assert_eq!(c, [1, 7, 3, 4, 5, 6, 2, 8]);
        assert!(c[..pinned].contains(&7) && !c[..next].contains(&2));
    }

    #[test]
    fn more_stale_pins_than_new_ones_keep_the_rest_filed_and_a_gone_one_is_dropped() {
        let mut c = vec![1, 2, 3, 4, 9, 5]; // alloc: Cold
        // pinned [0, 5): 2 and 3 stale, 9 the run's one new pin at index 4; 42 is no chunk now
        let (left, (next, pinned)) = trade_stale(&mut c, id, &[42, 2, 3], 4, 1, (5, 5));
        assert_eq!(left, [3]);
        assert_eq!((next, pinned), (4, 4));
        assert_eq!(c, [1, 9, 3, 4, 2, 5]);
    }

    #[test]
    fn no_stale_pins_leave_the_chunks_and_counts_as_they_were() {
        let mut c = vec![1, 2, 3]; // alloc: Cold
        assert_eq!(trade_stale(&mut c, id, &[], 1, 1, (3, 2)), (vec![], (3, 2))); // alloc: Cold
        assert_eq!(c, [1, 2, 3]);
    }

    /// One pull per generation, as the CTE's reader pulls the recursive driver: the pull's reattach keeps the set's pin
    /// under the reader's pins, the next generation's run pins a new one, and its reattach trades: two chunks hold the set's
    /// pins over any number of generations (the base mapped one per generation, D2641's 776 MiB at 12,000).
    #[test]
    fn a_set_re_run_once_per_pull_holds_two_chunks_over_many_generations() {
        let mut c: Vec<usize> = Vec::new(); // alloc: Cold
        let (mut next, mut pinned) = (0usize, 0usize);
        let mut filed: Vec<usize> = Vec::new(); // alloc: Cold
        let mut mapped = 0usize;
        for _gen in 0..1000 {
            // the pull's hold: the reader's cursor is the floor
            let held_pull = (next, pinned);
            pinned = next;
            let floor = next;
            // rerun_keep_begin: every filed pin lies under the floor (stale)
            assert!(filed.iter().all(|p| c[..floor].contains(p)));
            // the driver's hold, then the set's member pins one chunk: the first free one at the cursor, else a new one
            let held_set = (next, pinned);
            pinned = next;
            if next == c.len() {
                mapped += 1;
                c.push(1000 + mapped);
            }
            next += 1;
            pinned += 1;
            // the set's reattach: keep_nested, then file_pins' trade
            let (n2, p2) = keep_nested(&mut c, held_set, pinned);
            let (from, kept) = (held_set.1, p2 - held_set.1);
            let new: Vec<usize> = c[from..from + kept].to_vec();
            let (left, (n3, p3)) = trade_stale(&mut c, id, &filed, from, kept, (n2, p2));
            (next, pinned) = (n3, p3);
            filed = left.into_iter().chain(new).collect();
            // the pull's parked reattach keeps what the driver pinned under the reader's pins (nothing new after a trade)
            let nested_end = next.max(pinned);
            (next, pinned) = keep_nested(&mut c, held_pull, nested_end);
        }
        assert!(mapped <= 2, "{mapped} chunks mapped over 1000 generations");
        assert_eq!(filed.len(), 1);
    }
}

#[cfg(test)]
pub(crate) mod d2751_pull_keeps_set_pins {
    use super::{keep_nested, trade_stale, unheld};

    fn id(c: &usize) -> usize {
        *c
    }

    #[test]
    fn a_pull_takes_only_the_chunks_no_set_holds() {
        let sets = [(1u16, vec![7, 9]), (2, vec![])]; // alloc: Cold
        assert_eq!(unheld(vec![5, 7, 8, 9], &sets), [5, 8]); // alloc: Cold
        assert_eq!(unheld(vec![7], &sets), Vec::<usize>::new()); // alloc: Cold
        assert_eq!(unheld(vec![3], &[]), [3]); // alloc: Cold
    }

    /// d2641_trade_stale's generations with the pull's filing (`reattach_pull`, setop_rec-0043) after each parked reattach:
    /// the chunks the pull kept past the reader that no set holds become the pull's (pinned to the statement's end unless a
    /// re-arm comes). With `unheld` the set keeps its pin, its next run trades it, and two chunks serve every generation; a
    /// pull that took the set's pin (`steal`, the base before D2751) leaves the set nothing to trade and maps one chunk
    /// per generation.
    fn generations(steal: bool) -> (usize, usize) {
        let mut c: Vec<usize> = Vec::new(); // alloc: Cold
        let (mut next, mut pinned) = (0usize, 0usize);
        let mut filed: Vec<usize> = Vec::new(); // alloc: Cold
        let mut pull_pins: Vec<usize> = Vec::new(); // alloc: Cold
        let mut mapped = 0usize;
        for _gen in 0..100 {
            let held_pull = (next, pinned);
            pinned = next;
            let held_set = (next, pinned);
            pinned = next;
            if next == c.len() {
                mapped += 1;
                c.push(1000 + mapped);
            }
            next += 1;
            pinned += 1;
            let (n2, p2) = keep_nested(&mut c, held_set, pinned);
            let (from, kept) = (held_set.1, p2 - held_set.1);
            let new: Vec<usize> = c[from..from + kept].to_vec();
            let (left, (n3, p3)) = trade_stale(&mut c, id, &filed, from, kept, (n2, p2));
            (next, pinned) = (n3, p3);
            filed = left.into_iter().chain(new).collect();
            let nested_end = next.max(pinned);
            (next, pinned) = keep_nested(&mut c, held_pull, nested_end);
            // the pull's filing: what it kept past the reader
            let kept = pinned.saturating_sub(held_pull.1);
            let bases: Vec<usize> = c[held_pull.1..held_pull.1 + kept].to_vec();
            let took = if steal { bases } else { unheld(bases, &[(1, filed.clone())]) };
            filed.retain(|p| !took.contains(p));
            pull_pins.extend(took);
        }
        (mapped, pull_pins.len())
    }

    #[test]
    fn a_set_re_run_inside_a_pull_keeps_its_pin_and_two_chunks_serve_every_generation() {
        let (mapped, taken) = generations(false);
        assert!(mapped <= 2 && taken == 0, "{mapped} chunks mapped, {taken} taken by the pull over 100 generations");
        let (mapped, taken) = generations(true);
        assert!(mapped >= 50 && taken >= 50, "the base's leak: {mapped} chunks mapped, {taken} taken by the pull over 100 generations");
    }
}

#[cfg(test)]
pub(crate) mod d2344_keep_nested {
    use super::keep_nested;

    /// Chunks: `P` the held call's pins, `W` its window, `N` the nested run's pins, `F` free past them.
    #[test]
    fn a_nested_runs_pins_move_behind_the_held_pins_and_the_next_draw_never_reaches_them() {
        let mut c = ["P0", "W1", "W2", "N3", "N4", "F5"];
        // hold: (chunk_next, pinned) = (3, 1); the nested run pinned chunks 3 and 4
        let (next, pinned) = keep_nested(&mut c, (3, 1), 5);
        assert_eq!((next, pinned), (5, 3));
        assert_eq!(c, ["P0", "N3", "N4", "W1", "W2", "F5"]);
        // a fresh call rewinds `chunk_next = pinned`: every chunk it may draw from is the held window's or free
        assert!(c[pinned..].iter().all(|x| !x.starts_with('N') && !x.starts_with('P')));
        assert!(c[..pinned].iter().all(|x| !x.starts_with('W')));
    }

    #[test]
    fn a_nested_run_that_pinned_nothing_leaves_the_held_chunks_as_they_were() {
        let mut c = ["P0", "W1", "W2", "F3"];
        assert_eq!(keep_nested(&mut c, (3, 1), 3), (3, 1));
        assert_eq!(keep_nested(&mut c, (3, 1), 2), (3, 1));
        assert_eq!(c, ["P0", "W1", "W2", "F3"]);
    }
}

impl Node {
    /// M8 (RT-2f): pulled set `last`'s state, a fresh one the first time.
    pub(crate) fn pull_of(&mut self, last: u16) -> &mut Pull {
        let i = match self.pulls.iter().position(|p| p.last == last) {
            Some(i) => i,
            None => {
                self.pulls.push(Pull { last, ran: Vec::new(), phase: PullPhase::Fresh, pins: Vec::new(), win: None, resumed_in: None }); // alloc: Exec
                self.pulls.len() - 1
            }
        };
        &mut self.pulls[i]
    }

    /// M8 (RT-2f): 0 fresh / parked, 1 done, 2 failed.
    pub(crate) fn pull_phase(&mut self, last: u16) -> u8 {
        match self.pull_of(last).phase {
            PullPhase::Done => 1,
            PullPhase::Failed => 2,
            _ => 0,
        }
    }

    pub(crate) fn pull_ran(&mut self, last: u16, m: u16) -> bool {
        self.pull_of(last).ran.contains(&m)
    }

    pub(crate) fn mark_pull_ran(&mut self, last: u16, m: u16) {
        self.pull_of(last).ran.push(m); // alloc: Exec
    }

    /// M8 (RT-2f): the last member stopped after its budget: its call parked (resume word, claim, and every site its call
    /// left live that no outer hold keeps), for the next pull to resume.
    pub(crate) fn park_pull(&mut self, last: u16) {
        let (resume, claim) = raw::resume_claim(self.inner.ctx);
        let sites: Vec<i16> = (0..self.program.sites.len()).filter(|&i| self.site_words_live(i) && !self.held_sites.contains(&(i as i16))).map(|i| i as i16).collect(); // alloc: Exec
        let fill = (self.inner.refill_off, self.inner.fill_base);
        self.pull_of(last).phase = PullPhase::Parked { resume, claim, fill, sites };
    }

    /// M8 (RT-2f): the parked call of `last` back in the context for its resume; `false`: nothing parked (its first call).
    pub(crate) fn unpark_pull(&mut self, last: u16) -> Result<bool> {
        let ctx = self.inner.ctx;
        match core::mem::replace(&mut self.pull_of(last).phase, PullPhase::Fresh) {
            PullPhase::Parked { resume, claim, fill, .. } => {
                // its regions, its fill cursors, its resume word and claim back (the reader's call pointed them at its own)
                let pointed = self.point_at(last as usize);
                (self.inner.refill_off, self.inner.fill_base) = fill;
                raw::set_resume_claim(ctx, resume, claim);
                let p = self.pull_of(last);
                let win = p.win.take();
                p.resumed_in = win.map(|w| w.1);
                if let Some((cur, end)) = win {
                    raw::set_arena_window(ctx, cur, end);
                }
                pointed.map(|()| true)
            }
            other => {
                self.pull_of(last).phase = other;
                Ok(false)
            }
        }
    }

    /// setop_rec-0043: pulled set `last` re-armed (`Entry::rearm_pulled`): fresh, its producers not run, a parked call
    /// dropped (its sites leave the parked list, so the next open's sweep closes them), and the chunks its pulls kept pinned
    /// past the reader (`reattach_pull`) unpinned, as a set's next run unpins its last run's (`rerun_keep_begin`): nothing
    /// reads them past the re-arm (the store is reset, the parked call gone). One under the running call's floor stays
    /// pinned (the held calls count the chunks below their floor).
    /// with#170 / #171: only `members` run again; a pulled member the re-arm leaves out stays run (its product kept).
    pub(crate) fn pull_rearm(&mut self, last: u16, members: &[u16]) {
        let p = self.pull_of(last);
        p.phase = PullPhase::Fresh;
        p.ran.retain(|m| !members.contains(m));
        (p.win, p.resumed_in) = (None, None);
        let pins = core::mem::take(&mut p.pins);
        self.unpin(&pins);
    }

    /// `reattach` after a pull of set `last` read by pipeline `q`. D2344: when its last member parked (`parked`:
    /// `Pulled::Stopped`), its call's scratch (agg_gather's record pointer array, a final's rendered images) is what its
    /// resume reads, so the window it used is kept: the chunks it grew stay pinned with its pins, and the held call's cursor
    /// goes on past it (to its window's end when the member grew past it). setop_rec-0043: the chunks it kept past the
    /// reader (`keep_nested`) that no set's run at this depth files (`rerun_keep`: a pull inside a set's run is the set's,
    /// unpinned at its next run) are filed as the pull's. A re-armed pull is read again in the same reader call at every
    /// owner run: unfiled, each pull's by-reference images would stay pinned for the whole call.
    /// D3244: a parked member whose window ends in a chunk filed here (one it grew now, or the one its resume started in)
    /// keeps that window for its next resume.
    pub(crate) fn reattach_pull(&mut self, q: usize, h: Hold, parked: bool, last: u16) -> Result<()> {
        let (from, filed) = (h.chunks.1, self.rerun_keep.is_some_and(|(_, d)| d == h.sites));
        let (win, held_end) = (raw::arena_window(self.inner.ctx), h.arena.1);
        let pointed = self.reattach_keeping(q, h, parked);
        let kept = self.inner.pinned.saturating_sub(from);
        let mut grown_in = false;
        if kept > 0 && !filed {
            // D2751: a chunk a rerun set's run inside the pull filed stays the set's (`unheld`)
            let bases: Vec<usize> = self.inner.chunks[from..from + kept].iter().map(|c| c.as_ptr() as usize).collect(); // alloc: Exec
            let bases = unheld(bases, &self.rerun_pins);
            let end = win.1 as usize;
            grown_in = self.inner.chunks[from..from + kept].iter().any(|c| c.as_ptr() as usize + c.len() == end && bases.contains(&(c.as_ptr() as usize)));
            self.unfile_pulls(&bases);
            self.pull_of(last).pins.extend(bases); // alloc: Exec
        }
        let p = self.pull_of(last);
        let own = parked && !filed && win.1 != held_end && (grown_in || p.resumed_in.take() == Some(win.1));
        (p.win, p.resumed_in) = (own.then_some(win), None);
        pointed
    }

    /// D3236 (B): whether a pulled set keeps the chunk at `base` pinned (`Pull::pins`): its pull re-arm unpins it by its base.
    pub(crate) fn pull_pinned(&self, base: usize) -> bool {
        self.pulls.iter().any(|p| p.pins.contains(&base))
    }

    /// Chunk bases `bases` taken off every pull's filed pins (`unfile`: one owner per pinned chunk).
    pub(crate) fn unfile_pulls(&mut self, bases: &[usize]) {
        self.pulls.iter_mut().for_each(|p| p.pins.retain(|b| !bases.contains(b)));
    }

    pub(crate) fn pull_over(&mut self, last: u16, ok: bool) {
        self.pull_of(last).phase = if ok { PullPhase::Done } else { PullPhase::Failed };
    }

    /// D2498: a pulled set producing `slot` ran to its end (`Done`): no pull or statement-end drain writes the store again.
    pub(crate) fn pull_done_into(&self, slot: u8) -> bool {
        self.pulls.iter().any(|p| matches!(p.phase, PullPhase::Done) && self.program.pipelines[p.last as usize].produces() == Some(slot))
    }

    /// M8 (RT-2f): the sites a parked pulled call keeps live between pulls.
    pub(crate) fn parked_site(&self, i: usize) -> bool {
        self.pulls.iter().any(|p| matches!(&p.phase, PullPhase::Parked { sites, .. } if sites.contains(&(i as i16))))
    }

    /// The sites a held call keeps (`hold`: the outer call's driving site and its other live sites) or a parked pull keeps
    /// (`parked_site`): an open's sweep (`sweep_site_words`) and op 14's hold of a nested member's error (entry.rs
    /// `hold_and_pass`, D1018) leave them as they are.
    pub(crate) fn kept_site(&self, i: usize) -> bool {
        self.held_sites.iter().any(|&h| h as isize == i as isize) || self.parked_site(i)
    }

    /// M8 (RT-2f): pulled sets parked now (none after a cancel, an error, or the statement's end).
    pub fn parked_pulls(&self) -> usize {
        self.pulls.iter().filter(|p| matches!(p.phase, PullPhase::Parked { .. })).count()
    }

    /// D824: the leader's attachment to pipeline `q`, stopped on `BuildWait`, kept while its lazy build runs out of order:
    /// the resume word, the claim and the arena its stop left. The build's calls bump from `q`'s arena cursor on (its
    /// scratch and chunk base moved there), so `q`'s per-row scratch (a held row's by-reference images) is never reused.
    pub(crate) fn hold(&mut self, q: usize) -> Hold {
        let ctx = self.inner.ctx;
        let (resume, claim) = raw::resume_claim(ctx);
        let h = Hold { resume, claim, arena: raw::arena_window(ctx), scratch: (self.inner.scratch, self.inner.scratch_end), chunks: (self.inner.chunk_next, self.inner.pinned), fill: (self.inner.refill_off, self.inner.fill_base), sites: self.held_sites.len(), floor: self.pin_floor };
        self.inner.scratch = h.arena.0;
        self.inner.scratch_end = h.arena.1;
        self.inner.pinned = h.chunks.0;
        self.pin_floor = h.chunks.0;
        let d = self.program.pipelines[q].driving_site;
        self.held_sites.push(d); // alloc: Node
        // the call's other sites with live per-open words: a member without a driving site sweeps every site not held.
        // D2178 / D2179: a parked pull's sites stay its own (a held one would drop out of its re-park, then an open's sweep
        // closes it and the pull re-reads from its first block)
        for i in 0..self.program.sites.len() {
            if i as isize != d as isize && self.site_words_live(i) && !self.parked_site(i) {
                self.held_sites.push(i as i16); // alloc: Node
            }
        }
        h
    }

    /// L8: site `i` holds a per-open word an open left (an OPEN state: its next `next` skips the seek; a `cont`: it resumes a
    /// probe chain), the words `stale_site_words` resets.
    pub(crate) fn site_words_live(&self, i: usize) -> bool {
        let s = raw::site_ref(self.ctx(), i);
        s.state == gravity_abi::site::site::STATE_OPEN || s.cont != 0
    }

    /// D3014 S2': a nested run (`nest`) is in progress on this node.
    pub(crate) fn is_nested(&self) -> bool {
        self.nested > 0
    }

    /// M8 (RT-1): what `hold` does not keep of the leader call in flight, saved around a nested run (`Entry::run_nested`): the
    /// call's flags and its out block's cap, which a member's call sets; the nested count up until `unnest`.
    pub(crate) fn nest(&mut self) -> Nest {
        self.nested += 1;
        Nest { flags: raw::ctx_flags(self.inner.ctx), cap: self.inner.out.cap }
    }

    /// M8 (D1036): an out-of-order run starts: the context's worker ordinal (hir.rs's hosted lane, and the hosted aggregate
    /// arena's slot, which a nested lane's ordinal takes as the leader's, D1192; past them two diagnostics, host_abi.rs's
    /// agg_flush text and `bare_error`) moves to the next nested lane, `NESTED_LANE_TOP - depth`,
    /// so the run's hosted calls, window resets and region rewinds are that lane's, never the outer row's (lane 0 or a
    /// shallower nested one). The ordinal before, for `outer_lane`; past `NESTED_LANES` deep a Bug guard (lowering bounds
    /// the depth: program::v4::on_demand_groups). Per ctx:
    /// the leader's (D1035: every reader of an Init param is pinned to the leader); RT-2's worker-side run revisits it.
    pub(crate) fn nested_lane(&mut self) -> Result<u16> {
        // a Bug guard: lowerv4 refuses a program with more on-demand groups than lanes (program::v4::on_demand_groups)
        if self.lane_depth >= crate::NESTED_LANES {
            return Err(Error::host(format!("bug: runs nested more than {} deep inside one row, past lowering's bound (D1036)", crate::NESTED_LANES))); // alloc: Cold
        }
        self.lane_depth += 1;
        let ctx = self.inner.ctx;
        let outer = raw::worker_ordinal(ctx);
        raw::set_worker_ordinal(ctx, crate::NESTED_LANE_TOP + 1 - self.lane_depth);
        Ok(outer)
    }

    /// M8 (D1036): the out-of-order run over: the outer ordinal back, the depth down.
    pub(crate) fn outer_lane(&mut self, outer: u16) {
        self.lane_depth -= 1;
        raw::set_worker_ordinal(self.inner.ctx, outer);
    }

    /// M8 (RT-1): the nested run over: the saved flags and cap back, the count down.
    pub(crate) fn unnest(&mut self, n: Nest) {
        self.nested -= 1;
        raw::set_ctx_flags(self.inner.ctx, n.flags);
        self.inner.out.cap = n.cap;
    }

    /// D824: pipeline `q` re-attached after its lazy build ran: the leader's `Partials` (the build's region read after its
    /// grows), the grow home, the own slot, the fill cursors, the resume word, the claim and the arena its stop left.
    pub(crate) fn reattach(&mut self, q: usize, h: Hold) -> Result<()> {
        self.reattach_keeping(q, h, false)
    }

    /// D3294: the held call's state comes back whole even when a region it reads is no longer live (`point_at`'s error,
    /// the statement's), so the teardown after the error finds the node as the hold left it.
    pub(crate) fn reattach_keeping(&mut self, q: usize, h: Hold, parked: bool) -> Result<()> {
        let held = self.held_sites.get(h.sites).copied();
        debug_assert_eq!(held, Some(self.program.pipelines[q].driving_site), "reattach of a pipeline not the innermost held");
        self.held_sites.truncate(h.sites);
        let pointed = self.point_at(q);
        (self.inner.refill_off, self.inner.fill_base) = h.fill;
        (self.inner.scratch, self.inner.scratch_end) = h.scratch;
        // D2344 / D2379: the pins the nested run took stay pinned (and a parked member's grown chunks with them): see `keep_nested`.
        let nested_end = if parked { self.inner.chunk_next.max(self.inner.pinned) } else { self.inner.pinned };
        (self.inner.chunk_next, self.inner.pinned) = keep_nested(&mut self.inner.chunks, h.chunks, nested_end);
        self.pin_floor = h.floor;
        // D1339 row C: a rerun set's run files the pins it kept, which its next run unpins (`rerun_keep_begin`). D2641: any
        // other reattach (a pull's, a lazy build's, an Init run's) leaves the filing of the sets that ran inside it: it
        // unfiled them (review r2 F1), so a recursive term re-run inside each pull of its CTE pinned a new chunk per
        // generation; their next run trades them from under its floor instead of moving them (`trade_stale`)
        let kept = self.inner.pinned - h.chunks.1;
        if let Some((set, _)) = self.rerun_keep.filter(|&(_, d)| d == h.sites && kept > 0) {
            self.file_pins(set, h.chunks.1, kept);
        }
        let ctx = self.inner.ctx;
        // D2344 (b): a parked member's arena window stays its own: the held cursor goes on past it (to the held
        // window's end when the member grew a chunk of its own).
        let (arena_cur, arena_end) = raw::arena_window(ctx);
        let cur = match parked {
            false => h.arena.0,
            true if arena_end == h.arena.1 => arena_cur.max(h.arena.0),
            true => h.arena.1,
        };
        raw::set_resume_claim(ctx, h.resume, h.claim);
        raw::set_arena_window(ctx, cur, h.arena.1);
        pointed
    }
}
