//! D1596 (W1-D1596-PRESORTED; a `#[path]` child of hosts v4/m6_agg_distinct.rs): PG 18.6's presorted DISTINCT aggregate
//! (Aggref `aggpresorted`: the planner sorts the Agg's input by the DISTINCT, adjust_group_pathkeys_for_groupagg) when
//! lowerv4 feeds the Agg in that Sort's order (regroup.rs `presorted`; the agg.arena Proto arg's PRESORTED bit, on which
//! exec v4/m6.rs calls `presorted_fed`). Each input row is advanced as it comes, beside the riders in aggno order, as
//! nodeAgg's per-row transition does: past the strict input check (EEOP_AGG_STRICT_INPUT_CHECK_ARGS), a row equal to the
//! last one advanced is dropped (EEOP_AGG_PRESORTED_DISTINCT_SINGLE: the eqfn over (last, row) under the aggregate's input
//! collation; _MULTI: execTuplesMatch over (row, last), every DISTINCT column, the last first, each under its collation;
//! NULL equals NULL), any other remembered (a by-reference image copied) and advanced at once. No run is kept: the final
//! hands the state on (no sort, no fold at the final), so every finalfn runs after the last transition, in aggno order
//! (finalize_aggregates). A fresh state (the group's first row, a rescan's zeroed record) or another state slot forgets
//! the last row, as finalize_aggregates resets `haslast`. A fed spec keeps the arena serial (bind.rs: no partial
//! DISTINCT), so exec runs the program width 1. It sits in the family's raw half: its per-row transition and the state
//! hand-on / rehome are unsafe.
#![allow(unsafe_code)]
use datum::{Datum, NullableDatum};
use gravity_kernels::ops::agg::trans::hosted::{AGG_STATE_BYTES, STATE_INIT};
use types_error::PgResult;
use types_fmgr::LocalFcinfo;

use crate::ops::agg::trans::hosted_kept::image_len;
use crate::ops::agg::trans::hosted::{AggArena, Slot, FOLDED};
use crate::ops::group::hosted_distinct::{shape, Sorted};

/// The last row a fed DISTINCT advanced (PG's `lastdatum` / `uniqslot`): whether there is one, the state slot it belongs
/// to, each input's (datum, NULL), a by-reference datum pointing at its image in `bufs` (the spec's own copy).
#[derive(Default)]
pub(crate) struct Last {
    has: bool,
    state: usize,
    vals: Vec<(u64, bool)>, // alloc: Exec
    bufs: Vec<Vec<u64>>,    // alloc: Exec
}

impl AggArena {
    /// Under the agg.arena Proto arg's PRESORTED bit (exec v4/m6.rs): lowerv4 feeds this Agg the rows of PG's Sort in its
    /// order, so each aggpresorted DISTINCT folds per row (`fed_row`) and none sorts at the final.
    pub fn presorted_fed(&mut self) {
        // bounded("other: the Agg node's aggregate calls, one flag each at the arena's open: the query text bounds them, as PG's ExecInitAgg walks the same aggregates with no poll")
        for s in self.specs.iter_mut().filter(|s| s.a.aggpresorted) {
            if let Some(d) = s.distinct.as_mut().filter(|d| d.dedup()) {
                d.fed = Some(Last::default());
            }
        }
    }

    /// Under the agg.arena Proto arg's `AGG_ORDERED_FED` bit (exec v4/m6.rs): lowerv4 feeds this Agg the rows of
    /// PG's Sort in its order (fold/hosted.rs `ordered_fed`), so each aggpresorted ORDER BY aggregate (no DISTINCT) advances
    /// per row as nodeAgg does when `aggsortrequired` is false (`fed_row`): no run, no sort at the final.
    pub fn ordered_fed(&mut self) {
        // bounded("other: the Agg node's aggregate calls, one flag each at the arena's open: the query text bounds them, as PG's ExecInitAgg walks the same aggregates with no poll")
        for s in self.specs.iter_mut().filter(|s| s.a.aggpresorted) {
            if let Some(d) = s.distinct.as_mut().filter(|d| !d.dedup()) {
                d.fed = Some(Last::default());
            }
        }
    }

    /// Whether aggregate `i` is a presorted DISTINCT (or, T6k, ORDER BY) fed per row.
    pub(crate) fn fed(&self, i: usize) -> bool {
        self.specs[i].distinct.as_ref().is_some_and(|d| d.fed.is_some())
    }

    /// One input row of fed DISTINCT aggregate `i` in slot `k` into its group's state `s`: the strict check, the presorted
    /// DISTINCT check against the last row, then the transition at once (`advance`).
    /// # Safety — as `collect`: `s` is the aggregate's state slot, `args` / `nulls` its inputs.
    pub(crate) unsafe fn fed_row(&mut self, k: usize, i: usize, aggno: u32, s: *mut u8, args: *const u64, nulls: *const u8) -> PgResult<()> {
        if self.specs[i].distinct.as_ref().is_some_and(|d| !d.dedup()) {
            // T6k: an ORDER BY's row, no DISTINCT check: the transition at once (its strict input check, `advance`)
            if s.add(STATE_INIT).read() == FOLDED {
                return Err(shape("a row into an ORDER BY aggregate's group after its final (T6k)"));
            }
            return self.advance(k, i, aggno, s, args, nulls);
        }
        let (spec, Slot { aggs, tmp, .. }) = (&mut self.specs[i], &mut self.slots[k]);
        let coll1 = spec.a.inputcollid;
        let Some(Sorted { cols, keys, ninputs, fed: Some(last), .. }) = spec.distinct.as_mut() else { return Err(shape("a presorted DISTINCT row without its spec (D1596)")) };
        if aggs[i].transfn.fn_strict && (0..*ninputs).any(|c| nulls.add(c).read() != 0) {
            return Ok(()); // EEOP_AGG_STRICT_INPUT_CHECK_ARGS runs first: the row never reaches the DISTINCT check
        }
        match s.add(STATE_INIT).read() {
            FOLDED => return Err(shape("a row into a DISTINCT aggregate's group after its final folded it (D1323)")),
            0 => last.has = false,
            _ if last.state != s as usize => last.has = false,
            _ => {}
        }
        let (n, single) = (keys.len(), cols.len() == 1);
        let mut same = last.has;
        // bounded("key-columns: one compare per grouping key of a row; PG polls no deeper (a datum compare is one unit there too), at most MaxTupleAttributeNumber (1664) keys")
        for (c, &(att, _, coll, _)) in keys.iter().enumerate().rev() {
            if !same {
                break;
            }
            let j = if single { 0 } else { att as usize - 1 };
            let ((ov, on), (nv, nn)) = (last.vals[j], (args.add(j).read(), nulls.add(j).read() != 0));
            same = match (on, nn) {
                (true, true) => true,
                (false, false) => {
                    let Some(eq) = aggs[i].eq.get_mut(c) else { return Err(shape("a DISTINCT aggregate without its equality function")) };
                    let mut fc = LocalFcinfo::<2>::fresh(if single { coll1 } else { coll });
                    fc.set_result_mcx(tmp.mcx());
                    let (a, b) = if single { (ov, nv) } else { (nv, ov) };
                    fc.args[0] = NullableDatum { value: Datum::from_u64(a), isnull: false };
                    fc.args[1] = NullableDatum { value: Datum::from_u64(b), isnull: false };
                    let r = eq.invoke(&mut fc);
                    tmp.reset();
                    r?.as_bool()
                }
                _ => false,
            };
        }
        if same && n > 0 {
            return Ok(());
        }
        (last.has, last.state) = (true, s as usize);
        last.vals.clear();
        last.bufs.resize_with(cols.len(), Vec::new);
        // bounded("key-columns: one cell per sort column of a row; PG polls no deeper (a datum copy is one unit there too), at most MaxTupleAttributeNumber (1664) columns")
        for (c, &(typlen, byval)) in cols.iter().enumerate() {
            let (v, null) = (args.add(c).read(), nulls.add(c).read() != 0);
            let v = match null || byval || typlen == 0 {
                true => v,
                false => {
                    let (len, buf) = (image_len(v as *const u8, typlen), &mut last.bufs[c]);
                    buf.clear();
                    buf.resize(len.div_ceil(8), 0);
                    core::ptr::copy_nonoverlapping(v as *const u8, buf.as_mut_ptr() as *mut u8, len);
                    buf.as_ptr() as u64
                }
            };
            last.vals.push((v, null));
        }
        self.advance(k, i, aggno, s, args, nulls)
    }
}

/// `fold_sorted` of a fed spec: its state `s` handed on to `out` as it is (the rows were advanced as they came); a state no
/// row reached leaves `out` zeroed, so the final starts from the initcond.
/// # Safety — `s` / `out` are `AGG_STATE_BYTES` state slots.
pub(crate) unsafe fn hand_on(s: *const u8, out: *mut u8) {
    if s.add(STATE_INIT).read() != 0 {
        core::ptr::copy_nonoverlapping(s, out, AGG_STATE_BYTES as usize);
    }
}

/// `combine_sorted` of a fed spec: a lone state moved into an empty one (D1404's rehome); two partials of one group never
/// meet (the arena is serial), so a second is an ERROR by name.
/// # Safety — `into` / `from` are `AGG_STATE_BYTES` state slots of one aggregate.
pub(crate) unsafe fn rehome(into: *mut u8, from: *const u8) -> PgResult<()> {
    match (into.add(STATE_INIT).read(), from.add(STATE_INIT).read()) {
        (_, 0) => Ok(()),
        (0, _) => Ok(core::ptr::copy_nonoverlapping(from, into, AGG_STATE_BYTES as usize)),
        _ => Err(shape("two partial states of a presorted DISTINCT aggregate (D1596: its arena is serial)")),
    }
}
