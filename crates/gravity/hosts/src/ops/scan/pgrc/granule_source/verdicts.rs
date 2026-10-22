//! The granule verdicts of `GranuleRangeSource`: the zone, runtime-filter, condition-cache, bloom and PSMA answers
//! that skip, cover or narrow a granule before it is decoded.

use super::*;

impl GranuleRangeSource {
    /// The verdict of granule `lg` of part `p` (`rows` rows) into `verdict`, `lane_all`, `cond_hit` / `cond_mixed` /
    /// `cond_words` and `win`: the zone verdicts, the runtime filter, the condition cache, then bloom and PSMA.
    pub(super) fn verdict(&mut self, p: usize, lg: u32, rows: u32) {
        self.arm_part_dec(p);
        let (verdict, mixed) = if self.nzone > 0 { self.verdicts_of(p, lg, rows) } else { (V_MIXED, ALL_PREDS) };
        self.ne_granule(verdict, mixed);
        if self.nzone > 0 {
            fires::fire_shared(if verdict == V_NONE { "pgrc.zone_skip" } else { "pgrc.zone" });
        }
        // D3009: a disjunction every arm of which this granule's verdicts prove none
        let verdict = if verdict != V_NONE && self.any_none(p, lg, rows) { V_NONE } else { verdict };
        self.verdict = verdict;
        self.tallied_win = self.tallies(verdict, mixed);
        let mut la = core::mem::take(&mut self.lane_all);
        self.lane_all_of(mixed, &mut la);
        self.lane_all = la;
        if self.verdict != V_NONE && self.rt_cut_any(p, Some(lg)) {
            self.tally.rt_skipped += 1;
            self.verdict = V_NONE;
        }
        // the condition cache (H02i): a known granule is skipped (none), covered (all) or selected by its words. Asked
        // before bloom and PSMA, which only narrow toward the covered quals a hit already decides (PSMA's window per
        // granule was a sixth of a warm hit's time on a selective date range). The entry is taken and put back, never cloned
        self.cond_hit = 0;
        self.cond_mixed = false;
        if self.verdict != V_NONE && self.cond_ready(p) {
            let (q, e) = self.cond_entry.take().expect("ready");
            {
                memledger::value::lookup(memledger::Consumer::CondCache, crate::ops::scan::pgrc::cond_cache::value_key(&e, lg as usize), 0);
                let hit = e.get(lg as usize);
                self.tally.cond[(hit == crate::ops::scan::pgrc::cond_cache::Hit::Unknown) as usize] += 1;
                match hit {
                    crate::ops::scan::pgrc::cond_cache::Hit::None => {
                        self.tally.cond_skipped += 1;
                        self.verdict = V_NONE;
                    }
                    crate::ops::scan::pgrc::cond_cache::Hit::All => self.cond_hit = V_ALL,
                    crate::ops::scan::pgrc::cond_cache::Hit::Mixed(w) => {
                        self.cond_words.copy_from_slice(&w[..VALIDITY_WORDS]);
                        self.cond_hit = V_ALL;
                        self.cond_mixed = true;
                    }
                    crate::ops::scan::pgrc::cond_cache::Hit::Unknown => {
                        self.cond_words.fill(0);
                        self.cond_hit = V_MIXED;
                        // the miss's price: its granule's evaluation up to the words' publish (`Entry::put`), sampled
                        memledger::value::arm(memledger::Consumer::CondCache, crate::ops::scan::pgrc::cond_cache::value_key(&e, lg as usize));
                    }
                }
            }
            self.cond_entry = Some((q, e));
        }
        // the bloom verdict and the PSMA candidate rows over a Mixed granule the cache did not decide (H02 b / c); never
        // over one the tally answers (SRC-21: Mixed on tallied predicates alone), whose rows are not read
        self.win = None;
        if self.verdict == V_MIXED && self.cond_hit != V_ALL && self.nzone_decl > 0 && !self.tallied_win {
            if self.bloom_absent(p, lg, rows) {
                self.tally.bloom += 1;
                self.verdict = V_NONE;
                // a bloom-absent granule fails one covered qual on every row (no false negatives; every zone predicate is a
                // covered qual's): the cache learns its none as a scan would, so a part the blooms decide reaches
                // `whole_part_hit` (no bloom section read, no granule walk) once every granule is known
                if self.cond_hit == V_MIXED {
                    if let Some((_, e)) = self.cond_entry.as_ref().filter(|(q, _)| *q == p) {
                        e.put(lg as usize, &[0; VALIDITY_WORDS], rows as usize, 0);
                    }
                }
            } else {
                match self.psma_rows(p, lg, rows) {
                    Some((lo, hi)) if lo >= hi => {
                        self.tally.psma_skipped += 1;
                        self.verdict = V_NONE;
                    }
                    Some((0, hi)) if hi >= rows => {}
                    // D1536: a counting execution narrows no window (row_select's rule) unless its scan runs to its end
                    // (VERDICT_COUNT_SELECT): the program counts its rows
                    Some(_) if self.recipe.skipped.is_some() && !self.on(VERDICT_COUNT_SELECT) => {}
                    Some(w) => {
                        self.tally.psma_narrowed += 1;
                        self.win = Some(w);
                    }
                    None => {}
                }
            }
        }
    }

    /// Takes this execution's runtime filters once their builds have armed them (a build finishes before its probe stages):
    /// a filter's column must be a lane whose exact key is the datum word itself (an integer, date or timestamp column), or,
    /// for a text key's fingerprints (PS1-22), a code lane (its parts' dictionaries answer: `rt_text_cuts`); a filter on no
    /// such lane is off for the bind. D2856 (colskip): taken once per generation, not once per bind: a part entered after
    /// the cell's generation moved (a rerun set's build armed its run's keys, or the set re-opened and cleared them) drops
    /// the filters it holds and takes the cell's, so a rerun's probe skips by that run's keys only. The measuring windows
    /// stay (`rt_count`): a filter re-armed per run is not in warm-up again, and one window of `RT_PROBE` checks may span
    /// the runs.
    pub(super) fn rt_arm(&mut self) {
        let gen = self.cell.rt_gen();
        if gen == self.rt_gen {
            return;
        }
        if self.rt_gen != u64::MAX {
            if self.rt.iter().any(|s| s.f.is_some()) {
                fires::fire_shared("pgrc.rt_rearm");
            }
            // bounded("small-fixed: the runtime filter slots, RT_SLOTS (4)")
            for s in &mut self.rt {
                (s.lane, s.off, s.f) = (usize::MAX, false, None);
            }
        }
        let (gen, fs) = self.cell.rt_at();
        self.rt_gen = gen;
        // bounded("small-fixed: the runtime filter slots, RT_SLOTS (4)")
        for (k, f) in fs.into_iter().enumerate() {
            let Some(f) = f else { continue };
            match self.rt_lane_of(&f) {
                Some(lane) => {
                    fires::fire_shared("pgrc.rt_filter");
                    if k > 0 {
                        fires::fire_shared("pgrc.rt_multi"); // shapecheck fire signal (D3245): a site armed a filter past its first
                    }
                    (self.rt[k].lane, self.rt[k].f) = (lane, Some(f));
                }
                None => self.rt[k].off = true,
            }
        }
    }

    /// The lane of the site filter `f` reads: its column's, when the column's exact key is the datum word (or the filter is
    /// empty, or a text key's over a code lane).
    fn rt_lane_of(&self, f: &RtFilter) -> Option<usize> {
        let k = (0..self.recipe.natts).position(|k| self.recipe.attnos[k] == f.attno && self.recipe.shred_of(k).is_none());
        let text = |k: usize| !f.fps.is_empty() && self.recipe.kinds[k] == lane::KIND_CODES;
        let faithful = |k: usize| f.lo > f.hi || text(k) || f.fps.is_empty() && self.recipe.profiles.get(k).copied().flatten().is_some_and(|p| p.key.exact_key_to_datum_word(f.lo) == Some(f.lo as u64) && p.key.exact_key_to_datum_word(f.hi) == Some(f.hi as u64));
        k.filter(|&k| faithful(k))
    }

    /// Whether some armed runtime filter, measuring (not idle: `rt_count`), proves granule `lg` of part `p` (the whole part
    /// for `None`) holds no key of its build: the claim's read plan asks it.
    pub(super) fn rt_cuts(&self, p: usize, lg: Option<u32>) -> bool {
        (0..RT_SLOTS).any(|k| self.rt[k].measuring() && self.rt_test(k, p, lg))
    }

    /// D3245: each armed integer-key filter that is due (`rt_due`), the one that cut last first, asked whether granule `lg`
    /// of part `p` (the whole part for `None`) holds no key of its build; the first that proves it cuts, and the filters
    /// after it are not asked (so each window counts the granules the filters before it left).
    pub(super) fn rt_cut_any(&mut self, p: usize, lg: Option<u32>) -> bool {
        for i in 0..RT_SLOTS {
            let k = (self.rt_first + i) % RT_SLOTS;
            if !self.rt[k].armed() || self.rt[k].text() || !self.rt_due(k) {
                continue;
            }
            let cut = self.rt_test(k, p, lg);
            self.rt_count(k, cut);
            if cut {
                self.rt_first = k;
                return true;
            }
        }
        false
    }

    /// Whether the runtime filter of slot `k` proves granule `lg` of part `p` (the whole part for `None`) holds no key of
    /// its build.
    pub(super) fn rt_test(&self, k: usize, p: usize, lg: Option<u32>) -> bool {
        let s = &self.rt[k];
        if !s.armed() {
            return false;
        }
        let Some(f) = s.f.as_deref() else { return false };
        if f.lo > f.hi {
            return true;
        }
        // PS1-22: a text key's filter decides whole parts by their dictionaries (`rt_text_cuts`), never by zone records
        if !f.fps.is_empty() {
            return false;
        }
        // D3155: an idle filter's granule body is not read (`granule_stats`): its part's record bounds the granule
        let rec = match lg {
            Some(lg) => self.facts_of(p, s.lane, lg).or_else(|| self.part_facts_of(p, s.lane)),
            None => self.part_facts_of(p, s.lane),
        };
        let Some(rec) = rec else { return false };
        if rec.nonnull == 0 {
            return true;
        }
        if rec.key_kind != KeyKind::Exact.as_u8() || rec.min_key == i64::MIN && rec.max_key == i64::MAX {
            return false;
        }
        rec.max_key < f.lo || rec.min_key > f.hi || f.misses(rec.min_key, rec.max_key)
    }

    /// PS1-22: whether the armed text-key filter of slot `k` proves part `p` (pinned) holds no build key: every non-NULL
    /// value of the lane in a part is one of its dictionary's entries (DICT_CODES codes every row; a verbatim part's ranks
    /// are its distinct values), and no entry's `Fp128` image is among the build's. A faulted table, a table that does not
    /// fit, or a lane the part does not hold keeps the part: a fingerprint false positive keeps a part, never drops one, and
    /// no row is ever filtered (the probe still tests every row of a kept part).
    pub(super) fn rt_text_cuts(&mut self, k: usize, p: usize) -> PgResult<bool> {
        let lane = self.rt[k].lane;
        if !self.rt[k].armed() || !self.rt[k].text() || self.parts().list.faces.absent(self.face(lane), p) {
            return Ok(false);
        }
        self.open_dict(p, lane)?;
        if self.parts().part_if_open(p).is_none() {
            return Ok(false);
        }
        // FASTADD: the lane's part (a stand-in for a column the part lacks)
        let part = self.lane_part(p, lane)?;
        let tkey = self.table_key(p, lane, &part)?;
        let read_err_cell = core::cell::Cell::new(None);
        let t = match self.dict_at(p, lane).map(|e| &e.dict) {
            Some(Dict::Part(d)) => crate::dict::text_hosts::fp_table_in(self.fp_memo(), &part, tkey, &crate::dict::text_hosts::DictView::Part(d, &read_err_cell), 0, false),
            Some(Dict::Ranked { table, .. }) => Ok(table.arc_clone()),
            None => return Ok(false),
        };
        let t = match t {
            Ok(t) => t,
            Err(crate::dict::text_hosts::TableBuildError::Pg(e)) => return Err(e),
            Err(crate::dict::text_hosts::TableBuildError::Read(e)) => return Err(read_err(e)),
            Err(crate::dict::text_hosts::TableBuildError::Region(_)) => return Ok(false),
        };
        let fps = &self.rt[k].f.as_deref().expect("armed").fps;
        let cut = super::raw::fp_none_in(&t.table(), fps) == Some(true);
        if cut {
            fires::fire_shared("pgrc.rt_text");
        }
        Ok(cut)
    }

    /// Whether some runtime filter is armed and measuring (not off, not idle): its lane's Stats bodies and meta sections
    /// are read for it, and a whole-part answer waits for it (as the claim's read plan does).
    pub(super) fn rt_measuring(&self) -> bool {
        self.rt.iter().any(RtSlot::measuring)
    }

    /// Whether a measuring runtime filter reads lane `k`.
    pub(super) fn rt_measuring_lane(&self, k: usize) -> bool {
        self.rt.iter().any(|s| s.lane == k && s.measuring())
    }

    /// The measuring slots, one bit each (`granule_stats` reads a part's bodies again when they change).
    pub(super) fn rt_mask(&self) -> u8 {
        self.rt.iter().enumerate().fold(0, |m, (k, s)| m | (s.measuring() as u8) << k)
    }

    /// Whether this granule (or part) asks the armed runtime filter of slot `k`: every one while it measures, one in
    /// `rt_div()` while it is idle (`rt_count`).
    pub(super) fn rt_due(&mut self, k: usize) -> bool {
        let s = &mut self.rt[k];
        if !s.idle {
            return true;
        }
        s.rest += 1;
        if (s.rest as usize) < gravity_program::cost::pgrc::rt_div() {
            return false;
        }
        s.rest = 0;
        true
    }

    /// One check of the runtime filter of slot `k` counted. A filter measures in windows of `RT_PROBE` checks: a window
    /// that cuts under one in `rt_div()` (the cost home's share on this host where the decodes its cuts save no longer pay
    /// for its checks) idles it, and idle, the sampled check of one granule in `rt_div()` that cuts arms it again, so
    /// cutting granules anywhere in the scan are found (a column sorted or loaded in runs cuts late as well as early).
    pub(super) fn rt_count(&mut self, k: usize, cut: bool) {
        let s = &mut self.rt[k];
        if s.idle {
            if cut {
                fires::fire_shared("pgrc.rt_on");
                (s.idle, s.seen, s.cut) = (false, 0, 0);
            }
            return;
        }
        s.seen += 1;
        s.cut += cut as u32;
        if s.seen == RT_PROBE {
            if (s.cut as usize) * gravity_program::cost::pgrc::rt_div() < RT_PROBE as usize {
                fires::fire_shared("pgrc.rt_off");
                (s.idle, s.rest) = (true, 0);
            }
            (s.seen, s.cut) = (0, 0);
        }
    }

    /// The zone verdicts: always without a protocol, else when its zone road is on.
    /// D3009: the site's lanes the disjunctions read: their Stats bodies and blooms are read as a zone lane's.
    pub(super) fn any_lanes(&mut self) {
        // bounded("other: the arms of the statement's own OR quals, one lane bit each: the query text bounds them, and PG's ExecQual tests the same arms per row with no poll")
        for z in self.any.iter() {
            self.zone_lanes.set(z.lane as usize);
        }
        self.arm_record_lanes();
    }

    /// The `<>` lanes on trial (`ne_watch`, `ne_trial`), none a record lane yet (`record_lanes`): the code lanes whose
    /// every zone predicate is a text `<>` constant (`Ne` over a prefix key) and that no disjunction arm reads. A record
    /// lane is read for its part-grain record alone (the catalog's), with no granule Stats body, meta section or code zones.
    /// Cost: `<> c` fails only the rows equal to `c`, so a granule record decides it only as `AllMatch` (no row is `c`) or
    /// `NoneMatch` (every row is). An `AllMatch` granule saves a read only by eliding a filter-only lane or answering the
    /// window (`ne_elides`); a lane read past its filters decodes anyway, and the per-row selection on `c`'s code
    /// (`row_code`) is exact. A `NoneMatch` granule is skipped. The records cost a pread per part and lane on a cold part,
    /// and only reading them tells whether `c` is spread over every granule or held by a few. `=` keeps them: its
    /// `NoneMatch` granules are every granule without `c`.
    pub(super) fn arm_record_lanes(&mut self) {
        let natts = self.recipe.natts;
        let mut rec = core::mem::take(&mut self.record_lanes);
        rec.clear();
        let ne = |z: &ZonePred| z.op == ZoneOp::Ne && z.prefix;
        // bounded("other: one zone predicate of the statement's own quals each (an IN list's member is one): the query text bounds them, and PG's ExecQual tests the same quals per row with no poll")
        for z in self.zone.iter().filter(|z| ne(z) && (z.lane as usize) < natts && self.recipe.kinds[z.lane as usize] == lane::KIND_CODES) {
            rec.set(z.lane as usize);
        }
        // bounded("other: one zone predicate of the statement's own quals each (an IN list's member is one): the query text bounds them, and PG's ExecQual tests the same quals per row with no poll")
        for z in self.zone.iter().filter(|z| !ne(z)).chain(self.any.iter()).filter(|z| (z.lane as usize) < natts) {
            rec.unset(z.lane as usize);
        }
        let preds = |f: &dyn Fn(usize) -> bool| self.zone.iter().take(63).enumerate().filter(|(_, z)| f(z.lane as usize)).fold(0, |m, (i, _)| m | pred_bit(i));
        let watch = preds(&|k| rec.has(k));
        let elides = watch & preds(&|k| self.filter_lanes.has(k) || self.answer_lanes.has(k));
        (self.ne_watch, self.ne_elides, self.ne_paid, self.ne_asked, self.ne_dry) = (watch, elides, 0, false, 0);
        rec.clear();
        self.record_lanes = rec;
    }

    /// At a part's pin: a lane on trial (`ne_watch`) whose granule records paid for no granule of the last two parts asked
    /// (`ne_granule`) becomes a record lane for the rest of the bind: a constant in nearly every granule (a common value
    /// spread over the part) has no granule its records skip or elide, and a part's record cannot tell it from one held by
    /// a few granules.
    pub(super) fn ne_trial(&mut self) {
        if !core::mem::take(&mut self.ne_asked) {
            return;
        }
        let dry = self.ne_watch & !core::mem::take(&mut self.ne_paid);
        let out = dry & core::mem::replace(&mut self.ne_dry, dry);
        if out == 0 {
            return;
        }
        // bounded("small-fixed: the zone predicates a trial watches, at most 63")
        for i in (0..63).filter(|i| out >> i & 1 != 0) {
            let k = self.zone[i].lane;
            let of_k = self.zone.iter().take(63).enumerate().filter(|(_, z)| z.lane == k).fold(0, |m, (j, _)| m | pred_bit(j)) & self.ne_watch;
            if of_k & !out == 0 {
                self.record_lanes.set(k as usize);
                self.ne_watch &= !of_k;
            }
        }
    }

    /// A granule's zone verdict and its predicates not `AllMatch` (`verdicts_of`) for the trial (`ne_trial`): the watched
    /// predicates its records paid for, by a `NoneMatch` (skipped: every one, the mask does not say which predicate) or an
    /// `AllMatch` on a lane it elides or answers (`ne_elides`).
    #[inline]
    pub(super) fn ne_granule(&mut self, verdict: u8, mixed: u64) {
        if self.ne_watch != 0 {
            self.ne_asked = true;
            self.ne_paid |= self.ne_watch & if verdict == V_NONE { u64::MAX } else { !mixed & self.ne_elides };
        }
    }

    /// D3009: some disjunction of the site (`zone_any`) has every arm's predicate proved `NoneMatch` over granule `lg` of
    /// part `p`: by its zone record (the lane's resident map, else its Stats record) or, for an equality, its lane's bloom.
    /// A granule no arm passes holds no row of the OR. Never an `AllMatch`: the qual's filter runs on every window read.
    pub(super) fn any_none(&self, p: usize, lg: u32, rows: u32) -> bool {
        if !self.any_ok || self.any_ends.is_empty() {
            return false;
        }
        let (zone, bloom) = (self.zone_on(), self.on(VERDICT_BLOOM));
        let none = |z: &ZonePred| {
            let zoned = zone
                && match self.map_verdict(p, lg, rows, z) {
                    Some(v) => v == V_NONE,
                    None => self.facts_of(p, z.lane as usize, lg).is_some_and(|f| zone_verdict(&f, rows, z) == V_NONE),
                };
            zoned || bloom && z.op == ZoneOp::Eq && !z.prefix && self.bloom_lacks(p, lg, rows, z.lane, z.value)
        };
        let mut lo = 0usize;
        // bounded("other: the statement's own OR quals, one zone test per arm: the query text bounds them, and PG's ExecQual tests the same arms per row with no poll")
        for &hi in &self.any_ends {
            if self.any[lo..hi as usize].iter().all(none) {
                fires::fire_shared("pgrc.zone_any");
                return true;
            }
            lo = hi as usize;
        }
        false
    }

    pub(super) fn zone_on(&self) -> bool {
        self.enables == VerdictCell::LEGACY || self.enables & VERDICT_ZONE != 0
    }

    /// Whether the declared predicates are proved to match no row of granule `lg` of part `p` by their lanes' bloom blocks
    /// (H02b, v1's AM consult): an equality's constant absent, or every member of a lane's IN list (its `InMember`
    /// predicates, one disjunction per lane) absent.
    pub(super) fn bloom_absent(&self, p: usize, lg: u32, rows: u32) -> bool {
        if !self.on(VERDICT_BLOOM) {
            return false;
        }
        let decl = &self.zone[..self.nzone_decl];
        // the declared predicates: a bloom stands on its own road when the zone road is forced off. P5-11: a text constant's
        // probe word (`HashEq`, the writer's hash) as the bytes
        let lacks = |z: &ZonePred| match z.op {
            ZoneOp::Eq => !z.prefix && self.bloom_lacks(p, lg, rows, z.lane, z.value),
            ZoneOp::HashEq => self.bloom_lacks_packed(p, lg, z.lane, z.value),
            _ => false,
        };
        if decl.iter().any(|z| matches!(z.op, ZoneOp::Eq | ZoneOp::HashEq) && lacks(z)) {
            fires::fire_shared("pgrc.bloom");
            return true;
        }
        // an IN list: every member of a lane's list absent (`InMember` over a by-value lane, `HashIn` over a text lane)
        let absent = decl.iter().enumerate().any(|(i, z)| {
            let list = |y: &&ZonePred| y.op == z.op && y.lane == z.lane;
            matches!(z.op, ZoneOp::InMember | ZoneOp::HashIn)
                && !decl[..i].iter().any(|y| list(&y))
                && self.bloom_lacks_all(p, lg, z.lane, z.op == ZoneOp::HashIn, decl[i..].iter().filter(list).map(|y| y.value))
        });
        if absent {
            fires::fire_shared("pgrc.bloom");
        }
        absent
    }

    /// P5-11: whether the value whose probe word is `packed` (`HashEq` / `HashIn`: a text constant's payload under the
    /// writer's hash, `order_key::bloom::bloom_pack`) is proved absent from lane `k` in granule `lg` of part `p` by the
    /// lane's bloom block. The writer blooms a lane only where byte equality is value equality (its profile's
    /// `eq_bloomable`), and the lowering declared the word only under a collation whose equality is byte equality.
    fn bloom_lacks_packed(&self, p: usize, lg: u32, lane: u16, packed: i64) -> bool {
        let k = lane as usize;
        if !self.recipe.profiles.get(k).copied().flatten().is_some_and(|pr| pr.eq_bloomable) {
            return false;
        }
        let rp = self.parts();
        let granules = rp.list.parts()[p].granules;
        let Some(body) = rp.meta_if_read(p, self.face(k)).and_then(|m| m.bloom.as_deref()) else { return false };
        let Ok(Some((bk, block))) = bloom::bloom_block_for(body, granules, lg) else { return false };
        !bloom::bloom_may_contain_packed(block, bk, packed)
    }

    /// Whether every member of lane `lane`'s IN list is proved absent from granule `lg` of part `p` by the lane's bloom
    /// block (`hashed`: `HashIn` probe words; else `InMember` exact keys of a by-value lane, each also outside the granule's
    /// exact key range or absent from the block, as `bloom_lacks` decides one). The profile, block and record are resolved
    /// once for the list, and a member costs a range test and the block's bit tests: a list as long as a granule's members
    /// are rare would otherwise pay `bloom_lacks`' whole lowering per member and granule until its first maybe.
    fn bloom_lacks_all(&self, p: usize, lg: u32, lane: u16, hashed: bool, mut members: impl Iterator<Item = i64>) -> bool {
        let k = lane as usize;
        let Some(profile) = self.recipe.profiles.get(k).copied().flatten().filter(|pr| pr.eq_bloomable) else { return false };
        let rp = self.parts();
        let granules = rp.list.parts()[p].granules;
        let Some(body) = rp.meta_if_read(p, self.face(k)).and_then(|m| m.bloom.as_deref()) else { return false };
        let Ok(Some((bk, block))) = bloom::bloom_block_for(body, granules, lg) else { return false };
        if hashed {
            return members.all(|w| !bloom::bloom_may_contain_packed(block, bk, w));
        }
        // the constant's equality bytes as `lower_const` makes them for a word: its little-endian bytes at the class width
        let width = profile.class.width() as usize;
        if !matches!(profile.class, StorageClass::ByvalWord { .. } | StorageClass::Bool) || width == 0 {
            return false;
        }
        let range = self.facts_of(p, k, lg).filter(|r| r.key_kind == KeyKind::Exact.as_u8()).map(|r| (r.min_key, r.max_key));
        members.all(|v| {
            range.is_some_and(|(lo, hi)| v < lo || v > hi)
                || profile.key.exact_key_to_datum_word(v).is_some_and(|w| {
                    let w = if profile.class == StorageClass::Bool { (w != 0) as u64 } else { w };
                    !bloom::bloom_may_contain_packed(block, bk, bloom::bloom_pack(&w.to_le_bytes()[..width]))
                })
        })
    }

    /// Whether exact zone key `value` is proved absent from lane `k` in granule `lg` of part `p` by the lane's bloom block:
    /// the constant lowered through the lane's profile, so only lanes whose byte equality is value equality answer.
    fn bloom_lacks(&self, p: usize, lg: u32, rows: u32, lane: u16, value: i64) -> bool {
        let k = lane as usize;
        let rp = self.parts();
        let granules = rp.list.parts()[p].granules;
        let Some(profile) = self.recipe.profiles.get(k).copied().flatten() else { return false };
        let Some(word) = profile.key.exact_key_to_datum_word(value) else { return false };
        let Some(body) = rp.meta_if_read(p, self.face(k)).and_then(|m| m.bloom.as_deref()) else { return false };
        let Some(rec) = self.record_of(p, k, lg) else { return false };
        let Ok(Some((bk, block))) = bloom::bloom_block_for(body, granules, lg) else { return false };
        let Lowering::Lowered(lc) = lower_const(&profile, ConstInput::Word(word)) else { return false };
        evaluate(&profile, GrainFacts { rows: rows as u64 }, &rec, &ZonePredicate::Eq(lc), Some(BloomEvidence { k: bk, block })) == MetaVerdict::AllFail
    }

    /// P5-13: whether lane `lane`'s prefix predicates are a memcmp-ordered fixed image's order keys (its profile's class is
    /// `Fixed`: uuid, macaddr, macaddr8), which its PSMA blocks are built over; a text prefix predicate's lane is a varlena.
    pub(super) fn fixed_keyed(&self, lane: u16) -> bool {
        self.recipe.profiles.get(lane as usize).copied().flatten().is_some_and(|pr| matches!(pr.class, StorageClass::Fixed { .. }))
    }

    /// The candidate rows of granule `lg` of part `p` (H02c, v1's `psma_window`): the intersection over the constant
    /// predicates' PSMA blocks on exact keys (never `<>`, never the derived-SMA sentinel); `None` = no narrowing.
    pub(super) fn psma_rows(&self, p: usize, lg: u32, rows: u32) -> Option<(u32, u32)> {
        if !self.on(VERDICT_PSMA) {
            return None;
        }
        let rp = self.parts();
        let granules = rp.list.parts()[p].granules;
        let mut win: Option<(u32, u32)> = None;
        // bounded("other: one zone predicate of the statement's own quals each (an IN list's member is one): the query text bounds them, and PG's ExecQual tests the same quals per row with no poll")
        for z in self.zone[..self.nzone_decl].iter().filter(|z| !z.prefix || self.fixed_keyed(z.lane)) {
            let k = z.lane as usize;
            let Some(body) = rp.meta_if_read(p, self.face(k)).and_then(|m| m.psma.as_deref()) else { continue };
            // P5-13: a fixed image's prefix predicate reads a Coarse record never strict (`key(x) = key(c)` may hold for
            // `x < c`, so `<` bounds by `key(c)` itself), an Exact one as a word's
            let Some(rec) = self.facts_of(p, k, lg).filter(|r| r.key_kind == KeyKind::Exact.as_u8() || z.prefix && r.key_kind == KeyKind::Coarse.as_u8()) else { continue };
            let (kmin, kmax, c) = (rec.min_key, rec.max_key, z.value);
            if kmin == i64::MIN && kmax == i64::MAX {
                continue;
            }
            let Ok(Some(block)) = psma::psma_block_for(body, granules, lg) else { continue };
            let strict = rec.key_kind == KeyKind::Exact.as_u8();
            let cand = match z.op {
                ZoneOp::Eq => psma::psma_candidates_eq(block, kmin, kmax, c),
                ZoneOp::Lt if strict => c.checked_sub(1).and_then(|h| psma::psma_candidates_range(block, kmin, kmax, kmin, h)),
                ZoneOp::Lt | ZoneOp::Le => psma::psma_candidates_range(block, kmin, kmax, kmin, c),
                ZoneOp::Gt if strict => c.checked_add(1).and_then(|l| psma::psma_candidates_range(block, kmin, kmax, l, kmax)),
                ZoneOp::Gt | ZoneOp::Ge => psma::psma_candidates_range(block, kmin, kmax, c, kmax),
                ZoneOp::Ne | ZoneOp::NotNull | ZoneOp::IsNull | ZoneOp::InMember | ZoneOp::HashEq | ZoneOp::HashIn => None,
            };
            let Some((lo, hi)) = cand else { continue };
            let (lo, hi) = (lo as u32, (hi as u32).min(rows));
            win = Some(match win {
                Some((a, b)) => (a.max(lo), b.min(hi)),
                None => (lo, hi),
            });
        }
        win
    }

    /// Part `p`'s condition-cache entry under this execution's fingerprint in `cond_entry`, keyed by the part's uuid and
    /// owned by its open part (a reopened or recreated part never reads another's words); false when the cache is off.
    /// Callers borrow or take the entry, never clone it: a clone per granule put every worker on its refcount line.
    pub(super) fn cond_ready(&mut self, p: usize) -> bool {
        if self.cond_fp == [0, 0] {
            return false;
        }
        if self.cond_entry.as_ref().is_some_and(|(q, _)| *q == p) {
            return true;
        }
        let Some(part) = self.parts().part_if_open(p).map(Arc::clone) else { return false };
        let Some(granules) = self.list.as_ref().map(|l| l.parts()[p].granules as usize) else { return false };
        let u = part.uuid();
        let (Ok(lo), Ok(hi)) = (u[..8].try_into(), u[8..].try_into()) else { return false };
        // FASTADD: a part that lacks a column is read with its stand-in's fill, which the key carries
        let Some(unit) = self.list.as_ref().map(|l| l.content_key(p, [u64::from_le_bytes(lo), u64::from_le_bytes(hi)])) else { return false };
        let owner: Arc<dyn std::any::Any + Send + Sync> = part;
        self.cond_entry = Some((p, crate::ops::scan::pgrc::cond_cache::entry_for_mode(self.cond_fp, unit, &owner, granules, ROWS, self.cond_mode)));
        true
    }

    /// Granule `lg` of part `p` is admitted by the verdicts that skip it unread (zone, bloom, an empty PSMA window, the
    /// condition cache's none): `read.claim`'s extents cover only these.
    pub(super) fn admits(&mut self, p: usize, lg: u32, rows: u32) -> bool {
        self.arm_part_dec(p);
        if self.nzone > 0 && self.verdicts_of(p, lg, rows).0 == V_NONE {
            return false;
        }
        if self.rt_cuts(p, Some(lg)) || self.any_none(p, lg, rows) {
            return false;
        }
        // the condition cache before bloom and PSMA, as in stage: a granule it knows needs neither (the claim's read
        // plan asks every granule at claim entry)
        if self.cond_ready(p) {
            match self.cond_entry.as_ref().map(|(_, e)| e.get(lg as usize)) {
                Some(crate::ops::scan::pgrc::cond_cache::Hit::None) => return false,
                Some(crate::ops::scan::pgrc::cond_cache::Hit::All | crate::ops::scan::pgrc::cond_cache::Hit::Mixed(_)) => return true,
                _ => {}
            }
        }
        !(self.nzone_decl > 0 && (self.bloom_absent(p, lg, rows) || self.psma_rows(p, lg, rows).is_some_and(|(lo, hi)| lo >= hi)))
    }

    /// A sparse mixed hit's selection straight from the cache's survivor list (cond_cache `Entry::rows`: the same rows its
    /// words set, kept beside them), when granule `lg` of part `p` has no delete mask; `None` takes the words (a dense hit,
    /// a delete mask). The list widens into `sel` and copies into `sel16`, both reserved ROWS at bind.
    pub(super) fn memo_select(&mut self, p: usize, lg: u32, nrows: usize) -> Option<usize> {
        if self.has_dv {
            return None;
        }
        let (Some((q, e)), sel, sel16) = (&self.cond_entry, &mut self.sel, &mut self.sel16) else { return None };
        let rows = e.rows(lg as usize).filter(|_| *q == p)?;
        // the list's tail was masked at the publish's rows: a granule's rows never change on an immutable part
        debug_assert!(rows.last().is_none_or(|&r| (r as usize) < nrows), "a kept row past the granule's rows");
        sel16.clear();
        sel16.extend_from_slice(rows);
        sel.clear();
        sel.extend(rows.iter().map(|&r| r as u32));
        self.sel16_ok = true;
        Some(rows.len())
    }

    /// The cached words of a mixed hit as the selection (the delete mask applied after them): (true, survivors).
    pub(super) fn cond_select(&mut self, rows: usize) -> (bool, usize) {
        let n = rows.div_ceil(64);
        let mut words = [0u64; VALIDITY_WORDS];
        // bounded("small-fixed: the mask words of one granule, at most VALIDITY_WORDS (128)")
        for (w, o) in words[..n].iter_mut().enumerate() {
            *o = if self.has_dv { self.cond_words[w] & !self.dv[w] } else { self.cond_words[w] };
        }
        if rows % 64 != 0 {
            words[n - 1] &= (1u64 << (rows % 64)) - 1;
        }
        self.sel16_ok = false;
        (true, words_to_sel32(&words[..n], &mut self.sel, &mut self.sel16))
    }

    /// The verdict of the registered predicates over the whole part `p` (its part-grain records), with the predicates that
    /// are not `AllMatch` over the part, as a mask over the registered predicates (`ALL_PREDS`: no lane is all-match);
    /// `lane_all_of` turns it into lanes.
    pub(super) fn part_verdicts_of(&self, p: usize, rows: u32) -> (u8, u64) {
        let mut mixed = 0u64;
        // bounded("other: one zone predicate of the statement's own quals each (an IN list's member is one): the query text bounds them, and PG's ExecQual tests the same quals per row with no poll")
        for (i, z) in self.zone[..self.nzone].iter().enumerate() {
            let v = match self.code_verdict(p, i, None, rows) {
                Some(v) => v,
                None => {
                    let Some(rec) = self.part_facts_of(p, z.lane as usize) else { return (V_MIXED, ALL_PREDS) };
                    self.text_verdict(i, &rec, rows, z)
                }
            };
            match v {
                V_NONE => return (V_NONE, ALL_PREDS),
                V_ALL => {}
                _ => mixed |= pred_bit(i),
            }
        }
        (if mixed == 0 { V_ALL } else { V_MIXED }, mixed)
    }

    /// The verdict of the registered predicates over granule `lg` of the pinned part `p`, with the
    /// lanes whose every predicate is `AllMatch` over it.
    pub(super) fn verdicts_of(&self, p: usize, lg: u32, rows: u32) -> (u8, u64) {
        let mut mixed = 0u64;
        // bounded("other: one zone predicate of the statement's own quals each (an IN list's member is one): the query text bounds them, and PG's ExecQual tests the same quals per row with no poll")
        for (i, z) in self.zone[..self.nzone].iter().enumerate() {
            let v = match self.code_verdict(p, i, Some(lg), rows).or_else(|| self.map_verdict(p, lg, rows, z)) {
                Some(v) => v,
                None => match self.facts_of(p, z.lane as usize, lg) {
                    Some(rec) => self.text_verdict(i, &rec, rows, z),
                    // D3160: no granule record held: the part's record decides each granule where it decides the part
                    None => match self.part_decides_memo(p, i, z) {
                        Some(v) => {
                            fires::fire_shared("pgrc.zone_part_decides");
                            v
                        }
                        None => return (V_MIXED, ALL_PREDS),
                    },
                },
            };
            match v {
                V_NONE => return (V_NONE, ALL_PREDS),
                V_ALL => {}
                _ => mixed |= pred_bit(i),
            }
        }
        (if mixed == 0 { V_ALL } else { V_MIXED }, mixed)
    }

    /// D4554: part `p`'s part-grain verdicts (`part_dec`) once a part under the memo switch, for `verdicts_of`'s D3160 step:
    /// it asked `part_decides` (the part's Stats face and its record) at every granule, the same answer each time.
    #[inline]
    pub(super) fn arm_part_dec(&mut self, p: usize) {
        if self.part_dec_p == p || !self.part_dec_on {
            return;
        }
        self.arm_part_dec_cold(p);
    }

    #[cold]
    fn arm_part_dec_cold(&mut self, p: usize) {
        let n = self.nzone;
        if self.part_dec.len() < n {
            resize_charged(&mut self.part_dec, n, PD_ASK, &mut self.held);
        }
        // bounded("other: one zone predicate of the statement's own quals each (an IN list's member is one): the query text bounds them, and PG's ExecQual tests the same quals per row with no poll")
        for i in 0..n {
            let d = self.part_dec_of(p, i, &self.zone[i]);
            self.part_dec[i] = d;
        }
        self.part_dec_p = p;
    }

    /// `part_decides`' answer as `part_dec` keeps it.
    fn part_dec_of(&self, p: usize, i: usize, z: &ZonePred) -> u8 {
        let Some(rows) = self.list.as_ref().and_then(|l| l.parts().get(p)).and_then(|s| u32::try_from(s.rows).ok()) else { return PD_NOTHING };
        match self.part_facts_of(p, z.lane as usize) {
            None => PD_ASK,
            Some(rec) => match self.text_verdict(i, &rec, rows, z) {
                V_MIXED => PD_NOTHING,
                v => v,
            },
        }
    }

    /// `part_decides` through the part's memo (`part_dec`) when it holds part `p`'s answer for predicate `i`.
    #[inline]
    fn part_decides_memo(&self, p: usize, i: usize, z: &ZonePred) -> Option<u8> {
        if self.part_dec_p == p {
            match self.part_dec.get(i).copied() {
                Some(PD_NOTHING) => return None,
                Some(PD_ASK) | None => {}
                Some(v) => return Some(v),
            }
        }
        self.part_decides(p, i, z)
    }

    /// D3160: predicate `z`'s verdict over the whole part `p` from its part-grain record (`part_facts_of`, the catalog's
    /// or the held body's last record), when it is `AllMatch` or `NoneMatch`: every row of the part passes (or none
    /// does), so every granule of it does, whatever its own record would say. `None` for `Mixed`, a part past `u32`
    /// rows or no record. A zone verdict never reads a delete vector, so the part's and its granules' records agree.
    fn part_decides(&self, p: usize, i: usize, z: &ZonePred) -> Option<u8> {
        let rows = u32::try_from(self.list.as_ref()?.parts().get(p)?.rows).ok()?;
        let rec = self.part_facts_of(p, z.lane as usize)?;
        let v = self.text_verdict(i, &rec, rows, z);
        (v != V_MIXED).then_some(v)
    }

    /// Zone predicate `i` (`z`) over record `rec` of `rows` rows: `zone_verdict`, and where that leaves it `Mixed`, a text
    /// equality's verdict by its constant's byte length (`len_verdict`, `zlen`).
    #[inline]
    pub(super) fn text_verdict(&self, i: usize, rec: &ZoneFacts, rows: u32, z: &ZonePred) -> u8 {
        let v = zone_verdict(rec, rows, z);
        match self.zlen.get(i) {
            Some(&len) if v == V_MIXED && len != NO_LEN => len_verdict(rec, rows, z.op, len),
            _ => v,
        }
    }

    /// The byte length of zone predicate `z`'s constant for `len_verdict` (`zlen`), `NO_LEN` where it has none: an `=` /
    /// `<>` prefix predicate over a varlena lane whose profile keeps length stats, every image this execution holds for
    /// its lane, op and key one length (two constants that tie on their key may differ in length).
    pub(super) fn const_len(&self, z: &ZonePred) -> u32 {
        if !z.prefix || !matches!(z.op, ZoneOp::Eq | ZoneOp::Ne) || self.fixed_keyed(z.lane) {
            return NO_LEN;
        }
        let lens = self.recipe.profiles.get(z.lane as usize).copied().flatten().is_some_and(|pr| pr.len_stats != pgrc_meta::profile::LenStats::None);
        let mut hits = self.zcode_images.iter().filter(|c| c.lane == z.lane && c.op == z.op && c.key == z.value);
        let Some(c) = hits.next().filter(|_| lens) else { return NO_LEN };
        if hits.any(|o| o.image.len() != c.image.len()) {
            return NO_LEN;
        }
        u32::try_from(c.image.len()).ok().filter(|&l| l != NO_LEN).unwrap_or(NO_LEN)
    }

    /// sortpath (code-keyed zones): part `p`'s compare of each zone predicate on its codes (`zcode_preds`), at its pin: a
    /// text prefix predicate (SRC-09) whose constant this execution holds (`zcode_images`, exactly one image for its lane,
    /// op and key: two constants that tie on their key are left to the prefix keys) over a lane whose part sealed code
    /// zones. The constant's place in the part's byte-rank sorted dictionary turns the compare into an exact one on codes.
    pub(super) fn pin_codes(&mut self, p: usize, part: &Arc<OpenPart>) -> PgResult<()> {
        if self.zcode_part == p {
            return Ok(());
        }
        let mut preds = core::mem::take(&mut self.zcode_preds);
        preds.clear();
        preds.resize(self.nzone, None); // alloc: Exec
        if !self.zcode_images.is_empty() {
            // a tick per predicate: each may read its lane's code zones and search its dictionary
            let mut step = cancelpoll::step("granule_source: a part's compares on codes");
            for (i, z) in self.zone[..self.nzone].iter().enumerate() {
                step.tick().map_err(|_| err(WALK_CANCELED))?;
                // a record lane's `<>` constant is placed at the window (`row_code`), with no code zones read
                if !z.prefix || z.tallied || self.fixed_keyed(z.lane) || self.record_lanes.has(z.lane as usize) {
                    continue;
                }
                let mut hits = self.zcode_images.iter().filter(|c| c.lane == z.lane && c.op == z.op && c.key == z.value);
                let Some(c) = hits.next() else { continue };
                if hits.any(|o| o.image != c.image) {
                    continue;
                }
                let Some((_, d)) = self.parts().code_zones(p, self.face(z.lane as usize), part)? else { continue };
                preds[i] = code_pred(d, z, &c.image)?;
            }
        }
        self.zcode_preds = preds;
        self.zcode_part = p;
        Ok(())
    }

    /// sortpath (code-keyed zones): predicate `i`'s verdict over granule `lg` of part `p` (`None`: the whole part, its
    /// rollup) from the part's code zones, exact; `None` where the part has no compare on codes for it (`pin_codes`).
    #[inline]
    pub(super) fn code_verdict(&self, p: usize, i: usize, lg: Option<u32>, rows: u32) -> Option<u8> {
        if self.zcode_part != p {
            return None;
        }
        let c = self.zcode_preds.get(i)?.as_ref()?;
        let (body, _) = self.parts().code_zones_if_read(p, self.face(c.lane as usize))?;
        let k = c.lane as usize;
        let ((min, max), nonnull) = match lg {
            Some(lg) => (pgrc_format::meta::codezone_at(body, lg as usize)?, self.facts_of(p, k, lg)?.nonnull),
            None => (pgrc_format::meta::codezone_at(body, pgrc_format::meta::codezones_len(body)? - 1)?, self.part_facts_of(p, k)?.nonnull),
        };
        // a range and a count that disagree (none valid, yet a non-NULL row) decide nothing
        if (min > max) != (nonnull == 0) {
            return None;
        }
        let f = ZoneFacts { min_key: min as i64, max_key: max as i64, nonnull, byte_len_min: 0, byte_len_max: 0, computed: false, key_kind: KeyKind::Exact.as_u8(), sorted: 0 };
        let v = zone_verdict(&f, rows, c);
        fires::fire_shared(if v == V_NONE { "pgrc.code_zone_skip" } else { "pgrc.code_zone" });
        Some(v)
    }

    /// M2-ZONEREAD card A: predicate `z`'s verdict over granule `lg` of part `p` from its lane's resident zone map, `None`
    /// for a lane without one (its Stats body decides). The map holds the granule's exact non-NULL [min, max] (an all-NULL
    /// granule is the empty range, one without an exact key the full range: Mixed); an `AllMatch` also needs every row
    /// non-NULL, which the part's catalog record proves for the whole part (`map_all`), else it stays Mixed.
    #[inline(always)]
    pub(super) fn map_verdict(&self, p: usize, lg: u32, rows: u32, z: &ZonePred) -> Option<u8> {
        // the map holds exact keys only: a prefix predicate (SRC-09) reads its Coarse record
        if z.prefix {
            return None;
        }
        let r = self.map_cols.get(z.lane as usize)?.as_ref()?;
        let list = self.list.as_ref()?;
        let (min, max) = *r.get((list.starts()[p] + lg as u64) as usize)?;
        let all = self.map_all_part == p && self.map_all.has(z.lane as usize);
        if z.op == ZoneOp::IsNull {
            return Some(if min > max { V_ALL } else if all { V_NONE } else { V_MIXED });
        }
        if min > max {
            return Some(V_NONE);
        }
        if (min, max) == (i64::MIN, i64::MAX) {
            return Some(if z.op == ZoneOp::NotNull && all { V_ALL } else { V_MIXED });
        }
        let f = ZoneFacts { min_key: min, max_key: max, nonnull: rows, byte_len_min: 0, byte_len_max: 0, computed: false, key_kind: KeyKind::Exact.as_u8(), sorted: 0 };
        let v = zone_verdict(&f, rows, z);
        Some(if v == V_ALL && !all { V_MIXED } else { v })
    }

    /// M2-ZONEREAD card A: the mapped lanes (`map_cols`) of this execution: a zone-predicate lane, not an answer lane,
    /// whose column the list's resident zone map covers granule for granule.
    pub(super) fn arm_map(&mut self) {
        self.mapped.clear();
        self.map_all_part = usize::MAX;
        // bounded("key-columns: one zone-map slot per scanned column, at most MaxTupleAttributeNumber (1664) columns")
        for c in self.map_cols.iter_mut() {
            *c = None;
        }
        let Some(list) = self.list.as_ref() else { return };
        let Some(zones) = list.zones.get() else { return };
        // bounded("other: one zone predicate of the statement's own quals each (an IN list's member is one): the query text bounds them, and PG's ExecQual tests the same quals per row with no poll")
        for z in &self.zone[..self.nzone] {
            let k = z.lane as usize;
            if z.prefix || k >= self.recipe.natts || self.answer_lanes.has(k) || self.mapped.has(k) || Faces::is_shred(self.face(k)) {
                continue;
            }
            let attno = self.recipe.attnos[k] as u32;
            if let Some((_, col)) = zones.iter().find(|(a, _)| *a == attno) {
                let r = col.ranges();
                if r.len() as u64 == list.granules() {
                    self.map_cols[k] = Some(Arc::clone(r));
                    self.mapped.set(k);
                }
            }
        }
    }
}

/// sortpath (code-keyed zones): text prefix predicate `z` with constant `image` as an exact compare on dictionary `d`'s
/// codes (byte-rank sorted: code order is byte order). `r` = the entries below the constant, `f` = whether it is one:
/// `= c` is `code = r` (none when absent), `<> c` `code <> r`, `< c` `code < r`, `<= c` `code < r + f`, `> c`
/// `code >= r + f`, `>= c` `code >= r`. Any other op has none.
pub(super) fn code_pred(d: &DictHandle, z: &ZonePred, image: &[u8]) -> PgResult<Option<ZonePred>> {
    let n = d.ncodes();
    // the empty image sorts first: only code 0 can hold it, and the index's byte length says whether it does, so no entry
    // is read (an entry's first read loads the dictionary's payload extent whole)
    if image.is_empty() {
        let found = n > 0 && d.byte_len_only(0).map_err(read_err)? == 0;
        return Ok(code_word(z.op, 0, found).map(|(op, v)| ZonePred::word(z.lane, op, v)));
    }
    let (mut lo, mut hi) = (0u32, n);
    // bounded("small-fixed: a binary search over a dictionary's u32 codes, at most 32 probes")
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if d.entry(mid).map_err(read_err)?.bytes < image {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let found = lo < n && d.entry(lo).map_err(read_err)?.bytes == image;
    Ok(code_word(z.op, lo, found).map(|(op, v)| ZonePred::word(z.lane, op, v)))
}

/// `code_pred`'s compare on codes of `op c`, `r` the entries below `c`, `found` whether `c` is one.
fn code_word(op: ZoneOp, r: u32, found: bool) -> Option<(ZoneOp, i64)> {
    let (r, f) = (r as i64, found as i64);
    Some(match op {
        ZoneOp::Eq => (ZoneOp::Eq, if found { r } else { -1 }),
        ZoneOp::Ne => (ZoneOp::Ne, if found { r } else { -1 }),
        ZoneOp::Lt => (ZoneOp::Lt, r),
        ZoneOp::Le => (ZoneOp::Lt, r + f),
        ZoneOp::Gt => (ZoneOp::Ge, r + f),
        ZoneOp::Ge => (ZoneOp::Ge, r),
        _ => return None,
    })
}

#[cfg(test)]
mod rt_window_tests {
    use super::super::{GranuleRangeSource, GranuleRecipe, RT_PROBE};
    use gravity_program::cost::pgrc::rt_div;
    use std::sync::Arc;

    fn armed() -> GranuleRangeSource {
        let mut s = GranuleRangeSource::new(GranuleRecipe::for_test(1, &[1], Arc::new(pgrc_read::PartRegistry::new(1 << 20))));
        s.rt[0].lane = 0;
        s
    }

    /// One granule through the filter's gate: whether it was checked, its cut counted when it was.
    fn granule(s: &mut GranuleRangeSource, cut: bool) -> bool {
        let due = s.rt_due(0);
        if due {
            s.rt_count(0, cut);
        }
        due
    }

    /// P5-17: a window of `RT_PROBE` checks with no cut idles the filter (no Stats reads for its lane, the claim plan stops
    /// asking it); idle, one granule in `rt_div()` is still checked, and the first such check that cuts arms it again, so a
    /// scan whose cutting granules come late is still cut there. A window cutting at least one in `rt_div()` stays armed, and
    /// every window is judged afresh (a filter that stops cutting later idles then).
    #[test]
    fn a_window_without_a_cut_idles_the_filter_and_a_sampled_cut_arms_it_again() {
        let div = rt_div();
        assert!(div as u32 > RT_PROBE, "on the reference box one cut in a window keeps the filter armed");
        let mut s = armed();
        for _ in 0..RT_PROBE {
            assert!(granule(&mut s, false));
        }
        assert!(!s.rt_measuring(), "a window without a cut idles the filter");
        assert!(!s.rt_cuts(0, Some(0)), "the claim's read plan no longer asks it");
        let checked = (0..3 * div).filter(|_| granule(&mut s, false)).count();
        assert_eq!(checked, 3, "idle: one granule in rt_div() is checked");
        let late = (0..div).position(|_| granule(&mut s, true)).expect("a sampled check within rt_div() granules");
        assert_eq!(late, div - 1);
        assert!(s.rt_measuring(), "a sampled cut arms the filter again");
        // armed: a window with one cut in RT_PROBE stays armed (one in RT_PROBE is at least one in rt_div() here)
        for i in 0..RT_PROBE {
            assert!(granule(&mut s, i == 7));
        }
        assert!(s.rt_measuring(), "a window that cuts stays armed");
        for _ in 0..RT_PROBE {
            granule(&mut s, false);
        }
        assert!(!s.rt_measuring(), "the next window is judged afresh: no cut, idle");
    }

    /// H14 (kept: the filter is armed by measurement, `RT_PROBE` checks a window): the near form that must not idle it
    /// does not. A scan that asks the filter fewer than `RT_PROBE` times keeps it armed and measuring however few granules
    /// it cut, and every check it makes is a real check (none sampled away); a window one check short of `RT_PROBE`
    /// without a cut is still measuring, and the check that completes it idles the filter.
    #[test]
    fn a_filter_asked_fewer_than_a_window_of_checks_stays_armed() {
        let mut s = armed();
        for _ in 1..RT_PROBE {
            assert!(granule(&mut s, false), "every granule is checked while the filter measures");
            assert!(s.rt_measuring());
        }
        assert!(granule(&mut s, false));
        assert!(!s.rt_measuring(), "the RT_PROBE-th check completes the window");
        // a fresh bind: the window starts again
        let mut s = armed();
        assert!((0..RT_PROBE / 2).all(|_| granule(&mut s, false)) && s.rt_measuring());
    }

    /// D2856 (colskip): a rerun set's build arms its run's filter (the cell's generation moves) and the source takes it at
    /// its next part, dropping the one it held, but keeps its window: an idle filter stays idle (its sampled check runs on)
    /// and a half-done window goes on counting, so per-run filters never sit in warm-up. A clear (the set re-opened) leaves
    /// the source no filter until the build arms again.
    #[test]
    fn a_re_armed_filter_is_taken_again_and_keeps_its_measuring_window() {
        use super::super::RtFilter;
        let f = |lo: i64, hi: i64| RtFilter::charged(1, lo, hi, &[], &[], None).expect("an uncharged filter");
        let mut s = armed();
        s.cell.arm_rt(f(0, 10));
        s.rt[0].lane = usize::MAX;
        s.rt_arm();
        let first = s.rt_gen;
        assert_ne!(first, u64::MAX, "the first part takes the cell's generation");
        s.rt[0].lane = 0;
        for _ in 0..RT_PROBE {
            granule(&mut s, false);
        }
        assert!(s.rt[0].idle, "a window without a cut idles the filter");
        // the build ran again: a new filter, the source re-takes it at its next part and stays idle
        s.cell.arm_rt(f(20, 30));
        s.rt_arm();
        assert!(s.rt_gen > first, "the re-armed filter's generation is taken");
        assert!(s.rt[0].idle, "a re-armed filter keeps the window's idle verdict (no new warm-up)");
        assert!(s.rt[0].f.as_deref().is_none_or(|r| (r.lo, r.hi) == (20, 30)), "never the first run's keys");
        // the set re-opened: no filter until the build arms
        s.cell.clear_rt();
        s.rt_arm();
        assert!(s.rt[0].f.is_none() && s.rt[0].lane == usize::MAX, "a cleared cell leaves the source no filter");
    }
}

#[cfg(test)]
mod rt_slot_tests {
    use super::super::{GranuleRangeSource, GranuleRecipe, RtFilter, RT_PROBE, RT_SLOTS};
    use std::sync::Arc;

    /// D3245: each slot measures on its own window (one idles while another stays armed), the run form folds consecutive
    /// keys, and a run set meets a range only where a run does.
    #[test]
    fn each_slot_measures_alone_and_runs_fold_consecutive_keys() {
        let mut s = GranuleRangeSource::new(GranuleRecipe::for_test(1, &[1], Arc::new(pgrc_read::PartRegistry::new(1 << 20))));
        (s.rt[0].lane, s.rt[RT_SLOTS - 1].lane) = (0, 0);
        for i in 0..RT_PROBE {
            for (k, cut) in [(0, false), (RT_SLOTS - 1, i % 8 == 0)] {
                assert!(s.rt_due(k));
                s.rt_count(k, cut);
            }
        }
        assert!(s.rt[0].idle && !s.rt[RT_SLOTS - 1].idle, "a slot that cut stays armed beside one that idled");
        assert!(s.rt_measuring() && s.rt_measuring_lane(0) && s.rt_mask() == 1 << (RT_SLOTS - 1));
        assert_eq!(RtFilter::runs_of(&mut cancelpoll::step("test"), &[3, 4, 5, 9, 11, 12, i64::MAX]).unwrap(), [(3, 5), (9, 9), (11, 12), (i64::MAX, i64::MAX)]);
        let f = RtFilter::charged(1, 3, 12, &RtFilter::runs_of(&mut cancelpoll::step("test"), &[3, 4, 5, 9, 11, 12]).unwrap(), &[], None).expect("an uncharged filter"); // alloc: Cold
        for (min, max, miss) in [(6, 8, true), (6, 9, false), (10, 10, true), (0, 2, true), (13, 20, true), (5, 5, false), (0, 100, false)] {
            assert_eq!(f.misses(min, max), miss, "[{min}, {max}]");
        }
        let range = RtFilter { runs: memledger::ChargedVec::none(), ..f };
        assert!(!range.misses(6, 8), "the range alone misses nothing inside it");
    }
}

#[cfg(test)]
mod code_zone_tests {
    use super::{code_word, zone_verdict, KeyKind, ZoneFacts, ZoneOp, ZonePred, V_ALL, V_NONE};

    /// sortpath: every op, every constant (in the dictionary or between, below or past its entries) and every granule
    /// code range [lo, hi] holding each code once: the code zone's verdict never skips a granule with a passing row and
    /// never covers one with a failing row.
    #[test]
    fn code_zone_verdicts_agree_with_the_byte_compare() {
        let dict: [&[u8]; 4] = [b"app.bsky.feed.like", b"app.bsky.feed.post", b"app.bsky.graph.block", b"app.bsky.graph.follow"];
        let consts: [&[u8]; 8] = [b"", b"app.bsky.feed.like", b"app.bsky.feed.m", b"app.bsky.feed.post", b"app.bsky.g", b"app.bsky.graph.block", b"app.bsky.graph.follow", b"z"];
        for c in consts {
            let r = dict.partition_point(|e| *e < c) as u32;
            let found = dict.get(r as usize) == Some(&c);
            for op in [ZoneOp::Eq, ZoneOp::Ne, ZoneOp::Lt, ZoneOp::Le, ZoneOp::Gt, ZoneOp::Ge] {
                let (wop, v) = code_word(op, r, found).expect("a compare");
                for lo in 0..4u32 {
                    for hi in lo..4u32 {
                        let rows = hi - lo + 1;
                        let pass = |x: &[u8]| match op {
                            ZoneOp::Eq => x == c,
                            ZoneOp::Ne => x != c,
                            ZoneOp::Lt => x < c,
                            ZoneOp::Le => x <= c,
                            ZoneOp::Gt => x > c,
                            _ => x >= c,
                        };
                        let n = (lo..=hi).filter(|&k| pass(dict[k as usize])).count() as u32;
                        let f = ZoneFacts { min_key: lo as i64, max_key: hi as i64, nonnull: rows, byte_len_min: 0, byte_len_max: 0, computed: false, key_kind: KeyKind::Exact.as_u8(), sorted: 0 };
                        let got = zone_verdict(&f, rows, &ZonePred::word(0, wop, v));
                        let want = if n == 0 { V_NONE } else if n == rows { V_ALL } else { got };
                        assert_eq!(got, want, "{op:?} {:?} over [{lo}, {hi}]", core::str::from_utf8(c));
                        assert!(got == V_NONE || n > 0);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod ne_trial_tests {
    use super::super::{GranuleRangeSource, GranuleRecipe, ZoneOp, ZonePred, V_ALL, V_MIXED, V_NONE};
    use std::sync::Arc;

    /// Two code lanes, `t <> c` on lane 0 and `u <> d` on lane 1 (and `t <> e` on lane 0 with `second`); lane 0 read for its
    /// filter alone when `filter_only`.
    fn source(filter_only: bool, second: bool) -> GranuleRangeSource {
        let mut s = GranuleRangeSource::new(GranuleRecipe::for_test_codes(1, &[1, 2], &[0b11], Arc::new(pgrc_read::PartRegistry::new(1 << 20))));
        s.zone = vec![ZonePred::prefix(0, ZoneOp::Ne, 7), ZonePred::prefix(1, ZoneOp::Ne, 9)]; // alloc: Cold
        if second {
            s.zone.push(ZonePred::prefix(0, ZoneOp::Ne, 11));
        }
        if filter_only {
            s.filter_lanes.set(0);
        }
        s.arm_record_lanes();
        s
    }

    /// A part's pin, then its granules: each one's `(verdict, mixed)` as `verdicts_of` gives them.
    fn part(s: &mut GranuleRangeSource, granules: &[(u8, u64)]) {
        s.ne_trial();
        for &(v, mixed) in granules {
            s.ne_granule(v, mixed);
        }
    }

    const DRY: (u8, u64) = (V_MIXED, u64::MAX);

    /// Every `<>` code lane starts on trial, none a record lane; an `AllMatch` granule pays only for a lane it elides or
    /// answers (a filter-only lane here), a `NoneMatch` one for every lane.
    #[test]
    fn every_ne_lane_starts_on_trial_and_all_match_pays_only_for_a_lane_it_elides() {
        let s = source(false, false);
        assert!(!s.record_lanes.has(0) && !s.record_lanes.has(1));
        assert_eq!((s.ne_watch, s.ne_elides), (0b11, 0));
        let mut s = source(true, false);
        assert_eq!((s.ne_watch, s.ne_elides), (0b11, 0b01));
        part(&mut s, &[(V_ALL, 0)]);
        part(&mut s, &[(V_ALL, 0)]);
        part(&mut s, &[]);
        assert!(!s.record_lanes.has(0), "its AllMatch granules elide the filter-only lane");
        assert!(s.record_lanes.has(1), "a lane read past its filters decodes its AllMatch granules anyway");
        let mut s = source(false, false);
        part(&mut s, &[(V_NONE, u64::MAX)]);
        part(&mut s, &[DRY, (V_NONE, u64::MAX)]);
        part(&mut s, &[]);
        assert!(!s.record_lanes.has(0) && !s.record_lanes.has(1), "a NoneMatch granule is skipped: paid");
    }

    /// Two parts asked whose records paid for no granule of the lane make it a record lane at the next pin; one paid
    /// granule restarts the count, and a part with no granule asked (one the part records answered) counts for nothing. A
    /// bind re-arms the trial.
    #[test]
    fn a_lane_whose_records_pay_for_no_granule_of_two_parts_becomes_a_record_lane() {
        let mut s = source(true, false);
        part(&mut s, &[DRY, DRY]);
        part(&mut s, &[DRY, DRY]);
        assert!(!s.record_lanes.has(0), "one dry part: still on trial");
        part(&mut s, &[DRY]);
        assert!(s.record_lanes.has(0) && s.record_lanes.has(1), "two dry parts running: record lanes");
        assert_eq!(s.ne_watch, 0);
        s.arm_record_lanes();
        assert!(!s.record_lanes.has(0) && s.ne_watch == 0b11, "the next bind tries them again");

        let mut s = source(true, false);
        part(&mut s, &[DRY, DRY]);
        part(&mut s, &[DRY, (V_MIXED, !0b1), DRY]);
        part(&mut s, &[DRY]);
        part(&mut s, &[]);
        assert!(!s.record_lanes.has(0), "a paid granule restarts the count; a part not asked counts nothing");
        part(&mut s, &[DRY]);
        part(&mut s, &[]);
        assert!(s.record_lanes.has(0));
    }

    /// A lane with two watched predicates leaves the trial only when neither paid for a granule.
    #[test]
    fn a_lane_leaves_the_trial_only_when_none_of_its_predicates_pays() {
        let mut s = source(true, true);
        assert_eq!((s.ne_watch, s.ne_elides), (0b111, 0b101));
        part(&mut s, &[(V_MIXED, !0b100)]);
        part(&mut s, &[(V_MIXED, !0b100)]);
        part(&mut s, &[]);
        assert!(!s.record_lanes.has(0) && s.ne_watch & 0b101 == 0b101);
        part(&mut s, &[DRY]);
        part(&mut s, &[DRY]);
        part(&mut s, &[DRY]);
        assert!(s.record_lanes.has(0) && s.ne_watch == 0);
    }
}
