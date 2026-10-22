//! The lanes of `GranuleRangeSource`: table lanes built on demand, the all-match lanes and lane elision.

use super::*;

impl GranuleRangeSource {
    /// Table lane `k` of the current part, built on first demand (per part, per statement) and published.
    pub(super) fn lane_pg(&mut self, k: usize, sel: &[u32], out: &mut Lane) -> PgResult<u32> {
        let natts = self.recipe.natts;
        let at = k & !(LANE_PACKED as usize);
        if k & LANE_PACKED as usize != 0 && at < natts {
            let packed = at < 64 && self.packed_win >> at & 1 != 0;
            return Ok(if packed { ((natts * ROWS + at * PF_WORDS - at * ROWS) * 8) as u32 } else { 0 });
        }
        if k as u32 & gravity_abi::site::LANE_CLAIM != 0 {
            let Some(i) = ((k as u32 & !gravity_abi::site::LANE_CLAIM) as usize).checked_sub(natts + 1) else { return Err(err("gravity: a sweep claim outside the table lanes")) };
            let Some(s) = self.tables.sweeps.iter_mut().find(|s| s.table == i) else { return Err(err("gravity: a sweep claim without its shared sweep")) };
            return Ok(s.claim());
        }
        if k as u32 & gravity_abi::site::LANE_DONE != 0 {
            let i = (k as u32 & !gravity_abi::site::LANE_DONE) as usize - natts - 1;
            let Some(at) = self.tables.pending.iter().position(|(t, _, _)| *t == i) else { return Err(err("gravity: a table done without its pending claim")) };
            let (_, b, hdr) = self.tables.pending.swap_remove(at);
            if let Some(s) = self.tables.sweeps.iter().position(|s| s.table == i) {
                if let Some(sweep) = self.tables.sweeps.swap_remove(s).sweep {
                    if !sweep.complete() {
                        return Err(err("gravity: a shared sweep done before its last chunk"));
                    }
                    super::raw::sweep_finish(hdr, &sweep);
                    self.tables.swept.push(sweep); // alloc: Node
                }
            }
            b.publish(hdr);
            return Ok(1);
        }
        if k < natts {
            return self.late_fill(k, sel);
        }
        let Some(i) = k.checked_sub(natts + 1) else { return Err(err("gravity: lane fill outside the table lanes")) };
        let Some(spec) = self.recipe.tables.get(i).cloned() else { return Err(err("gravity: lane fill outside the table lanes")) };
        let spec = self.bound_spec(spec); // X5: a bound code set's values, read at this window's bind
        let p = self.cur_part;
        if p == usize::MAX {
            return Err(err("gravity: table lane before a window"));
        }
        // an answered window reads no row, and its part may be answered from the list's records, never pinned (a warm list:
        // `faces_known`): its tables bind empty, no dictionary opened, no table built; a kind with no empty table opens the
        // part for its build
        if self.answered {
            if crate::ops::scan::pgrc::raw::publish_empty_table(spec.elem(), i as u32, spec.over, out).is_some() {
                return Ok(1);
            }
            self.parts().part(p, &self.recipe.registry)?;
        }
        self.open_dict(p, spec.over as usize)?;
        self.tables.enter(p, self.recipe.tables.len());
        if self.absent(p, spec.over as usize) {
            // M6 R3: a shred code lane the part did not elect has no entries: every row of it is invalid, never answered
            if crate::ops::scan::pgrc::raw::publish_empty_table(spec.elem(), i as u32, spec.over, out).is_none() {
                return Err(err("gravity: a table of this kind over a shred path the part did not elect"));
            }
            return Ok(1);
        }
        // bound (a window was staged); `resolved` is read here, before the loop's borrows of the cursors
        let resolved = self.resolved_unbound();
        // CONVERT wave 2: a build's working memory goes on the breaker this source's pipeline feeds, else the statement
        let owner = self.held.owner();
        let dict = &self.dict_at(p, spec.over as usize).expect("dictionary").dict;
        // D3156: a kept-granule table's codes are its own (kept.rs): the statement's tables over it are keyed apart from
        // another site's whole-part table of the same part
        let salt = match dict {
            Dict::Ranked { table, .. } => table.ranks().map_or(0, |r| r.salt()),
            Dict::Part(_) => 0,
        };
        let key = gravity_dict::once::Key { part: p as u32, epoch: dict.epoch64() as u32, fingerprint: spec.fingerprint() };
        let once = self.tables.once.ok_or_else(|| err("gravity: table once-cells"))?;
        let face = self.face(spec.over as usize);
        loop {
            match once.claim(key) {
                gravity_dict::once::Claim::Ready(h) => {
                    // a header this source published into its own region for this part generation
                    super::raw::publish_lane(h, i as u32, spec.over, out);
                    return Ok(1);
                }
                gravity_dict::once::Claim::Full => return Err(err("gravity: more table lanes than once-cells")),
                // D319: a second table lane of this key while this source's own build of it is pending (a program's table
                // until its LANE_DONE): the lowering's bug (one table lane per key), an ERROR, never a wait for itself
                gravity_dict::once::Claim::Held => {
                    return Err(Box::new(PgError::error(format!("gravity: Reason::Bug: D319: table lane {i} claims the table of part {p} (fingerprint {:#x}) that this execution's pending build holds", key.fingerprint)).with_sqlstate(types_error::ERRCODE_INTERNAL_ERROR))) // alloc: Cold
                }
                // the once-cells are this source's own: no other thread's build to wait for, no wait to cancel
                gravity_dict::once::Claim::Cancelled => return Err(err("gravity: a table lane's claim was cancelled")),
                gravity_dict::once::Claim::Build(b) => {
                    // FASTADD: the lane's part (a stand-in for a column the part lacks) keys its tables
                    let over_part = self.lane_part(p, spec.over as usize)?;
                    let tkey = self.table_key(p, spec.over as usize, &over_part)?;
                    let read_err_cell = core::cell::Cell::new(None);
                    let base = p * natts;
                    let fp_part = (matches!(dict, Dict::Part(_)) && matches!(spec.func, crate::dict::text_hosts::TableFn::Fingerprint | crate::dict::text_hosts::TableFn::FingerprintBpchar)).then(|| Arc::clone(&over_part));
                    // textkeys step 3 (TableFn::Canon): the part's resident Fp128 table of the same form, the canonical image
                    // table's long entries (no second hash); none: hashed in its build
                    let canon_raw = match (&spec.func, dict) {
                        (crate::dict::text_hosts::TableFn::Canon | crate::dict::text_hosts::TableFn::CanonBpchar, Dict::Part(_)) => crate::dict::text_hosts::fp_resident_as(&over_part, tkey, spec.func == crate::dict::text_hosts::TableFn::CanonBpchar),
                        (crate::dict::text_hosts::TableFn::Canon, Dict::Ranked { table, .. }) => Some(table.arc_clone()),
                        _ => None,
                    };
                    let like_part = matches!(spec.func, crate::dict::text_hosts::TableFn::Like { .. } | crate::dict::text_hosts::TableFn::Eq { .. } | crate::dict::text_hosts::TableFn::Range { .. } | crate::dict::text_hosts::TableFn::Faults).then(|| Arc::clone(&over_part));
                    let (cur_cell, scratch_cell);
                    let view = match dict {
                        Dict::Part(d) => crate::dict::text_hosts::DictView::Part(d, &read_err_cell),
                        Dict::Ranked { table, .. } => {
                            if spec.func == crate::dict::text_hosts::TableFn::Fingerprint {
                                // the fingerprint table is the ranks': cached with them, pinned for the part
                                let st = &mut self.tables;
                                st.built += 1;
                                TABLES_BUILT.fetch_add(1, Ordering::Relaxed);
                                b.publish(table.header());
                                st.pinned.push(table.clone()); // alloc: Node
                                continue;
                            }
                            cur_cell = core::cell::RefCell::new(self.cursors[base + spec.over as usize].as_deref_mut().expect("cursor"));
                            scratch_cell = core::cell::RefCell::new(&mut self.scratch);
                            crate::dict::text_hosts::DictView::Ranked { ranks: table.ranks().expect("ranks"), cur: &cur_cell, scratch: &scratch_cell, err: &read_err_cell }
                        }
                    };
                    let st = &mut self.tables;
                    if let Some(part) = like_part.as_ref() {
                        let t = crate::dict::text_hosts::cond_table_in(Some(resolved.fp_memo()), part, tkey, &spec, &view).map_err(table_err)?;
                        st.built += 1;
                        TABLES_BUILT.fetch_add(1, Ordering::Relaxed);
                        b.publish(t.header());
                        st.pinned.push(crate::dict::text_hosts::FpPin::of(&t)); // alloc: Node
                        continue;
                    }
                    if spec.shared() {
                        let key = StmtKey { rel: self.rel as u32, part: p as u32, attno: face, fingerprint: spec.fingerprint() ^ salt };
                        let tables = &resolved.tables;
                        // a derived fingerprint table is the statement's like the rest: a query constant keys it, so
                        // it is never cached across statements
                        // CONVERT wave 2: the table and its build on the statement whose sources share it (no one breaker owns it)
                        // (`gravity_driver::stmt_context`), from the build to the last pin's drop
                        // textkeys step 3 (road key.canon): a canonical image table is the statement's too, from the part's
                        // resident Fp128 table of its form where there is one (no second hash)
                        let t = tables.get_or_build(key, &mut || match spec.func {
                            crate::dict::text_hosts::TableFn::Canon | crate::dict::text_hosts::TableFn::CanonBpchar => crate::dict::text_hosts::build_canon_for(&view, canon_raw.as_deref(), spec.func == crate::dict::text_hosts::TableFn::CanonBpchar, HeldCharge::of_statement()),
                            _ => crate::dict::text_hosts::build_shared_for(&spec, &view, &mut st.fmgr, HeldCharge::of_statement()),
                        }.map_err(table_err))?;
                        if matches!(spec.func, crate::dict::text_hosts::TableFn::Canon | crate::dict::text_hosts::TableFn::CanonBpchar) {
                            fires::fire_shared("key.canon.table"); // textkeys witness signal: a code lane's canonical image table
                        }
                        st.built += 1;
                        TABLES_BUILT.fetch_add(1, Ordering::Relaxed);
                        b.publish(t.header());
                        st.pinned.push(crate::dict::text_hosts::FpPin::of(&t)); // alloc: Node
                        continue;
                    }
                    if let Some(part) = fp_part.as_ref() {
                        // the fingerprint table is the part's, process-resident (R2): published from the cache, pinned for the part
                        let t = match crate::dict::text_hosts::fp_table_in(Some(resolved.fp_memo()), part, tkey, &view, spec.over, spec.func == crate::dict::text_hosts::TableFn::FingerprintBpchar) {
                            Ok(t) => t,
                            Err(crate::dict::text_hosts::TableBuildError::Region(e)) => return Err(crate::pg_err(match e {
                                gravity_dict::BuildError::TooManyCodes => "gravity: a dictionary past the table's code range",
                                gravity_dict::BuildError::NoSeed => "gravity: the fingerprint seed is not installed",
                                _ => "gravity: table build",
                            })),
                            Err(crate::dict::text_hosts::TableBuildError::Pg(e)) => return Err(e),
                            Err(crate::dict::text_hosts::TableBuildError::Read(e)) => return Err(read_err(e)),
                        };
                        st.built += 1;
                        TABLES_BUILT.fetch_add(1, Ordering::Relaxed);
                        b.publish(t.header());
                        st.pinned.push(crate::dict::text_hosts::FpPin::of(&t)); // alloc: Node
                        continue;
                    }
                    // (2') DK1-MISEST: an adaptive call table is the hosted build once this source's last unfilled part's rows
                    // filled at least ADAPT_HOSTED_PCT % of its entries; before that (the source's first part too) it is
                    // published unfilled as a Program Fp128 table, the rows' re-evaluation storing each entry they read
                    let adaptive = matches!(spec.func, crate::dict::text_hosts::TableFn::DerivedCall { adaptive: true, .. });
                    let lazy = adaptive && (i >= 64 || st.adapt_hosted >> i & 1 == 0);
                    let unfilled;
                    let bspec = match lazy {
                        true => {
                            unfilled = crate::dict::text_hosts::TableSpec { over: spec.over, func: crate::dict::text_hosts::TableFn::Program { elem: gravity_dict::lane::ELEM_FP128, swept: false, fingerprint: spec.fingerprint() } };
                            &unfilled
                        }
                        false => &spec,
                    };
                    if adaptive {
                        fires::fire_shared(if lazy { "percode.adaptive_lazy" } else { "percode.adaptive_hosted" });
                    }
                    if let (false, crate::dict::text_hosts::TableFn::Program { elem, swept: true, .. }) = (lazy, &spec.func) {
                        // D3115: the part's table is the statement's, swept once in chunks by every source that reaches it
                        if let crate::dict::text_hosts::DictView::Part(d, _) = &view {
                            d.prewarm_payload().map_err(read_err)?;
                        }
                        let n = gravity_dict::Dict::ncodes(&view);
                        if n >= 1 << gravity_abi::status::FAULT_CODE_BITS {
                            return Err(crate::pg_err("gravity: a dictionary past the table's code range"));
                        }
                        let skey = StmtKey { rel: self.rel as u32, part: p as u32, attno: face, fingerprint: spec.fingerprint() ^ salt };
                        let (sweep, made) = resolved.sweeps.share(skey, *elem, n)?;
                        if st.bufs.last().is_none_or(|c| c.len() * 8 < st.used + TABLE_REGION_MIN) {
                            let last = st.bufs.last().map_or(0, Vec::len);
                            st.push_chunk((TABLE_REGION_MIN * 4).max(last * 16 * 8), &mut self.held);
                        }
                        let (mut r, _) = st.tail_region();
                        let h = super::raw::sweep_header(&mut r, &sweep).map_err(|_| crate::pg_err("gravity: table build"))?;
                        st.used += r.used();
                        st.built += 1;
                        if made {
                            TABLES_BUILT.fetch_add(1, Ordering::Relaxed);
                            fires::fire_shared("percode.shared_sweep");
                        } else {
                            TABLES_SHARED.fetch_add(1, Ordering::Relaxed);
                            fires::fire_shared("percode.shared_sweep_join");
                        }
                        if sweep.complete() {
                            super::raw::sweep_finish(h, &sweep);
                            st.swept.push(sweep); // alloc: Node
                            b.publish(h);
                            continue;
                        }
                        st.pending.push((i, b, h)); // alloc: Node
                        st.sweeps.push(SweepShare::shared(i, sweep)); // alloc: Node
                        super::raw::publish_lane(h, i as u32, spec.over, out);
                        return Ok(gravity_abi::site::LANE_UNFILLED as u32);
                    }
                    loop {
                        if st.bufs.last().is_none_or(|c| c.len() * 8 < st.used + TABLE_REGION_MIN) {
                            let last = st.bufs.last().map_or(0, Vec::len);
                            st.push_chunk((TABLE_REGION_MIN * 4).max(last * 16 * 8), &mut self.held);
                        }
                        let (mut r, cap) = st.tail_region();
                        match crate::dict::text_hosts::build_table_for(bspec, &view, &mut r, &mut st.fmgr, owner) {
                            Ok(h) => {
                                st.used += r.used();
                                st.built += 1;
                                TABLES_BUILT.fetch_add(1, Ordering::Relaxed);
                                if matches!(bspec.func, crate::dict::text_hosts::TableFn::Program { .. }) {
                                    // the program sweeps the entries and calls `LANE_DONE`; the lane points at the unfilled table meanwhile
                                    st.pending.push((i, b, h)); // alloc: Node
                                    // D3115: a table this source sweeps alone hands its chunks out in order
                                    st.sweeps.push(SweepShare::alone(i, gravity_dict::Dict::ncodes(&view))); // alloc: Node
                                    if lazy {
                                        st.adapt_lazy.push((i, h)); // alloc: Node
                                    }
                                    // a header this source allocated in its own region for this part generation
                                    super::raw::publish_lane(h, i as u32, spec.over, out);
                                    return Ok(gravity_abi::site::LANE_UNFILLED as u32);
                                }
                                b.publish(h);
                                break;
                            }
                            Err(crate::dict::text_hosts::TableBuildError::Region(gravity_dict::BuildError::RegionFull { needed })) => {
                                // a new chunk holds this table; the earlier ones stay valid for the kernels holding them
                                st.push_chunk((st.used + needed + TABLE_REGION_MIN).max(cap * 2), &mut self.held);
                                continue;
                            }
                            Err(crate::dict::text_hosts::TableBuildError::Region(e)) => return Err(crate::pg_err(match e {
                                gravity_dict::BuildError::TooManyCodes => "gravity: a dictionary past the table's code range",
                                gravity_dict::BuildError::NoSeed => "gravity: the fingerprint seed is not installed",
                                _ => "gravity: table build",
                            })),
                            Err(crate::dict::text_hosts::TableBuildError::Pg(e)) => return Err(e),
                            Err(crate::dict::text_hosts::TableBuildError::Read(e)) => return Err(read_err(e)),
                        }
                    }
                }
            }
        }
    }

    /// The lanes every one of whose predicates is `AllMatch` (`mixed` the predicates that are not), into `out`.
    pub(super) fn lane_all_of(&self, mixed: u64, out: &mut LaneBits) {
        out.clear();
        // bounded("other: one zone predicate of the statement's own quals each (an IN list's member is one): the query text bounds them, and PG's ExecQual tests the same quals per row with no poll")
        for z in &self.zone[..self.nzone] {
            out.set(z.lane as usize);
        }
        // bounded("other: one zone predicate of the statement's own quals each (an IN list's member is one): the query text bounds them, and PG's ExecQual tests the same quals per row with no poll")
        for (i, z) in self.zone[..self.nzone].iter().enumerate() {
            if mixed & pred_bit(i) != 0 {
                out.unset(z.lane as usize);
            }
        }
    }

    /// The lanes a granule (or part) of zone verdict `verdict` leaves undecoded, into `out`: every filter-only lane when
    /// `AllMatch` or a condition-cache `hit` decided it, else those whose own predicates all hold (`lane_elide`). Under
    /// `VERDICT_ELIDE_HIT` only a hit decides them (the zone does not cover their quals).
    #[inline(always)]
    pub(super) fn lane_elision_into(&self, verdict: u8, hit: bool, lane_all: &LaneBits, out: &mut LaneBits) {
        if self.enables != VerdictCell::LEGACY && self.enables & VERDICT_ELIDE == 0 {
            out.clear();
        } else if self.on(VERDICT_ELIDE_HIT) {
            match hit {
                true => {
                    out.assign(&self.filter_lanes);
                    if out.any() {
                        fires::fire_shared("pgrc.elide_hit");
                    }
                }
                false => out.clear(),
            }
        } else if verdict == V_ALL || hit {
            out.assign(&self.filter_lanes);
        } else if self.recipe.lane_elide {
            out.assign(&self.filter_lanes);
            out.and(lane_all);
        } else {
            out.clear();
        }
    }
}
