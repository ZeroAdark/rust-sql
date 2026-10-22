//! The hosted aggregate arena's raw state work (R6 home; a child of hosted.rs, so it reads the arena's own fields): the
//! transitions, combines and finals over the kernels' state slots (v1 `aggstate.rs`'s `GenericAgg` steps), a final's direct
//! arguments, and the `AggHead` entries the kernels call (host_abi `m6_agg_*`). Moved from hosted.rs unchanged.
#![allow(unsafe_code)]
use super::*;
use gravity_abi::Ctx;

impl AggArena {
    /// `advance_transition_function` (v1 `advance_at` / `advance_loaded`): the strict input check, the first input as
    /// the state (`ExecAggInitGroup`), a NULL state kept, else the call and the by-reference copy (`ExecAggCopyTransValue`).
    unsafe fn trans(&mut self, ordinal: u16, aggno: u32, s: *mut u8, args: *const u64, nulls: *const u8) -> PgResult<()> {
        if self.handed {
            let k = self.slot(ordinal)?;
            self.next_group(k);
        }
        if let Some(j) = self.kept_of(aggno) {
            let k = self.slot(ordinal)?;
            return self.keep_first(k, j, aggno, s, args.read(), nulls.read() != 0);
        }
        let (i, k) = (self.agg(aggno)?, self.slot(ordinal)?);
        self.collations_armed(k)?;
        if s.add(STATE_INIT).read() != 0 && (s.add(STATE_AGGNO) as *const u32).read_unaligned() != aggno {
            return Ok(()); // V4R-424: PG's shared transition state (one aggtransno): the aggregate that opened it steps it
        }
        match self.specs[i].distinct {
            Some(_) => self.collect(k, i, aggno, s, args, nulls), // D1210: kept for the final's sort
            None => self.advance(k, i, aggno, s, args, nulls),
        }
    }

    /// The transition proper of aggregate `i` in slot `k` (a DISTINCT aggregate's per distinct value, m6_agg_distinct.rs).
    pub(crate) unsafe fn advance(&mut self, k: usize, i: usize, aggno: u32, s: *mut u8, args: *const u64, nulls: *const u8) -> PgResult<()> {
        let spec = &self.specs[i];
        let Slot { aggs, node, tmp, calls, .. } = &mut self.slots[k];
        let h = &mut aggs[i];
        AggArena::init_parts(node, spec, s, aggno)?;
        let strict = h.transfn.fn_strict;
        let mut anynull = false;
        // bounded("small-fixed: one argument of the transition call, at most FUNC_MAX_ARGS (100)")
        for k in 0..spec.ninputs {
            if nulls.add(k).read() != 0 {
                anynull = true;
                h.trans.set_arg_null(k + 1);
            } else {
                h.trans.set_arg(k + 1, Datum::from_u64(args.add(k).read()));
            }
        }
        let value = s.add(STATE_VALUE) as *mut u64;
        if strict && anynull {
            return Ok(());
        }
        if strict && s.add(STATE_NOTRANS).read() != 0 {
            let first = h.trans.arg(1);
            let v = if spec.transbyval { first } else { execexpr::agg_datum_copy(node.aggcontext(), first, spec.translen)? };
            value.write_unaligned(v.as_u64());
            s.add(STATE_NULL).write(0);
            s.add(STATE_NOTRANS).write(0);
            return Ok(());
        }
        let old_null = s.add(STATE_NULL).read() != 0;
        if strict && old_null {
            return Ok(());
        }
        let old = Datum::from_u64(value.read_unaligned());
        if old_null { h.trans.set_arg_null(0) } else { h.trans.set_arg(0, old) }
        h.trans.nargs = (spec.ninputs + 1) as i16;
        h.trans.isnull = false;
        h.trans.context = node.fm_node_ptr();
        h.trans.set_result_mcx(tmp.mcx());
        node.set_current_agg(spec.aggref, spec.shared);
        let r = h.transfn.invoke(&mut *h.trans);
        node.clear_current_agg();
        let (r, rnull) = (r?, h.trans.isnull);
        let new = if !spec.transbyval && r.as_u64() != old.as_u64() {
            let copied = if rnull { Datum::null() } else { execexpr::agg_trans_value_copy(node, r, spec.translen)? };
            if !old_null {
                execexpr::agg_datum_free(node.aggcontext(), old, spec.translen);
            }
            copied
        } else {
            r
        };
        value.write_unaligned(new.as_u64());
        s.add(STATE_NULL).write(rnull as u8);
        tmp.reset();
        *calls = calls.wrapping_add(1);
        if *calls % 256 == 0 {
            self.slots[k].charge();
        }
        Ok(())
    }

    /// `initialize_aggregate` for a slot not yet holding a state: the initcond (a per-group copy when by reference), and
    /// the aggregate's number for the merge (`STATE_AGGNO`).
    unsafe fn init_parts(node: &AggStateNode, spec: &Spec, s: *mut u8, aggno: u32) -> PgResult<()> {
        if s.add(STATE_INIT).read() == 0 {
            let v = if spec.initval.isnull || spec.transbyval { spec.initval.value } else { execexpr::agg_datum_copy(node.aggcontext(), spec.initval.value, spec.translen)? };
            (s.add(STATE_VALUE) as *mut u64).write_unaligned(v.as_u64());
            s.add(STATE_NULL).write(spec.initval.isnull as u8);
            s.add(STATE_NOTRANS).write(spec.initval.isnull as u8);
            (s.add(STATE_AGGNO) as *mut u32).write_unaligned(aggno);
            s.add(STATE_INIT).write(1);
        }
        Ok(())
    }

    /// `advance_combine_function` on the leader's merge slot (a serial arena's one slot): the partial state at `from` (a
    /// held state slot) combined into the held state at `into` by the combinefn (a strict one skips a NULL partial and
    /// takes the first partial as the state as `ExecAggInitGroup` does), the result copied into the slot's aggcontext
    /// when by reference. The old state is not freed: it may be another slot's, whose context the arena frees at the
    /// statement's end.
    /// # Safety — `into` / `from` are `AGG_STATE_BYTES` state slots of aggregate `aggno`; no worker runs a slot.
    pub unsafe fn combine(&mut self, aggno: u32, into: *mut u8, from: *const u8) -> PgResult<()> {
        self.combine_in(self.slots.len() - 1, aggno, into, from)
    }

    /// `combine` in slot `k` (D798: two partials of one group in a keyed record fold, in the folding worker's slot).
    unsafe fn combine_in(&mut self, k: usize, aggno: u32, into: *mut u8, from: *const u8) -> PgResult<()> {
        if self.kept_of(aggno).is_some() {
            return Ok(crate::ops::agg::trans::hosted_kept::combine(into, from));
        }
        let i = self.agg(aggno)?;
        self.collations_armed(k)?;
        if self.specs[i].distinct.is_some() {
            return self.combine_sorted(k, aggno, into, from);
        }
        let spec = &self.specs[i];
        let Slot { aggs, node, tmp, .. } = &mut self.slots[k];
        let Some((f, fc)) = aggs[i].combine.as_mut() else {
            return Err(Box::new(PgError::error(format!("gravity: two partial states of hosted aggregate {aggno} without its combinefn")))); // alloc: Cold
        };
        AggArena::init_parts(node, spec, into, aggno)?;
        let value = into.add(STATE_VALUE) as *mut u64;
        let (nv, nnull) = (Datum::from_u64((from.add(STATE_VALUE) as *const u64).read_unaligned()), from.add(STATE_NULL).read() != 0);
        if f.fn_strict {
            if nnull {
                return Ok(());
            }
            if into.add(STATE_NOTRANS).read() != 0 {
                let v = if spec.transbyval { nv } else { execexpr::agg_datum_copy(node.aggcontext(), nv, spec.translen)? };
                value.write_unaligned(v.as_u64());
                into.add(STATE_NULL).write(0);
                into.add(STATE_NOTRANS).write(0);
                return Ok(());
            }
            if into.add(STATE_NULL).read() != 0 {
                return Ok(());
            }
        }
        let (old, old_null) = (Datum::from_u64(value.read_unaligned()), into.add(STATE_NULL).read() != 0);
        if old_null { fc.set_arg_null(0) } else { fc.set_arg(0, old) }
        if nnull { fc.set_arg_null(1) } else { fc.set_arg(1, read_only(nv, spec.translen)) }
        fc.nargs = 2;
        fc.isnull = false;
        fc.context = node.fm_node_ptr();
        fc.set_result_mcx(tmp.mcx());
        node.set_current_agg(spec.aggref, spec.shared);
        let r = f.invoke(&mut **fc);
        node.clear_current_agg();
        let (r, rnull) = (r?, fc.isnull);
        let new = if !spec.transbyval && !rnull && r.as_u64() != old.as_u64() { execexpr::agg_datum_copy(node.aggcontext(), r, spec.translen)? } else { r };
        value.write_unaligned(new.as_u64());
        into.add(STATE_NULL).write(rnull as u8);
        into.add(STATE_NOTRANS).write(0);
        tmp.reset();
        Ok(())
    }

    /// D1323: `fold_sorted` into `out` the first time group state `s` is finalized, the folded state then kept in `s`
    /// (`FOLDED`) and copied into `out` at any later final. PostgreSQL folds a group's sorted run once (its transfn's
    /// side effects, a plpgsql NOTICE or a write, once per kept row); an emit may finalize a group twice (a top over groups
    /// builds each group's row in its order pass, then again at the emit: keyed/order_slot.rs `run`).
    /// # Safety — as `fold_sorted`; `s` is writable.
    pub(crate) unsafe fn fold_once(&mut self, k: usize, i: usize, aggno: u32, s: *mut u8, out: *mut u8) -> PgResult<()> {
        if s.add(STATE_INIT).read() == FOLDED {
            // the folded state as `fold_sorted` left it: opened (STATE_INIT 1), so the final's init_parts keeps its value
            core::ptr::copy_nonoverlapping(s, out, AGG_STATE_BYTES as usize);
            out.add(STATE_INIT).write(1);
            return Ok(());
        }
        self.fold_sorted(k, i, aggno, s, out)?;
        if out.add(STATE_INIT).read() == 0 {
            return Ok(()); // an empty run: nothing folded, every final starts from the initcond (fold_sorted's early return)
        }
        core::ptr::copy_nonoverlapping(out, s, AGG_STATE_BYTES as usize);
        s.add(STATE_INIT).write(FOLDED);
        Ok(())
    }

    /// D1371: host-sorted aggregate `aggno`'s run sorted, deduplicated and folded into its group state `s` ahead of the
    /// group's finals (`fold_once`, which leaves `s` folded, so the final's own `fold_once` copies it): PG's
    /// finalize_aggregates runs every sorted transition state's process_ordered_aggregate_* before any finalfn. An
    /// aggregate that keeps no run (no DISTINCT or ORDER BY sort here) folds nothing.
    unsafe fn fold_ahead(&mut self, ordinal: u16, aggno: u32, s: *mut u8) -> PgResult<()> {
        if self.kept_of(aggno).is_some() {
            return Ok(());
        }
        let (i, k) = (self.agg(aggno)?, self.slot(ordinal)?);
        if self.specs[i].distinct.is_none() {
            return Ok(());
        }
        self.arm_catalog();
        self.slots[k].tmp.reset();
        self.slots.iter().for_each(Slot::charge);
        let mut folded = [0u64; AGG_STATE_BYTES as usize / 8];
        self.fold_once(k, i, aggno, s, folded.as_mut_ptr() as *mut u8)
    }

    /// `finalize_aggregate`: the finalfn over the state (an ordered-set aggregate's direct arguments, V4R-425, then the extra
    /// arguments NULL; a strict one with a NULL argument is
    /// NULL), or the state itself; `out` = (datum, null, by-reference bytes), the image live until the slot's next final.
    unsafe fn finalize(&mut self, ordinal: u16, aggno: u32, s: *mut u8, out: *mut u64) -> PgResult<()> {
        if let Some(j) = self.kept_of(aggno) {
            return Ok(self.kept_final(j, s, out));
        }
        let (i, k) = (self.agg(aggno)?, self.slot(ordinal)?);
        self.handed |= self.streamed;
        self.arm_catalog(); // V4R-453: a final on a pool claim reads the armed facts too
        self.slots[k].tmp.reset();
        self.slots.iter().for_each(Slot::charge);
        let spec = &self.specs[i];
        let memo = spec.distinct.is_none() && (spec.shape.aggfinalmodify == AGGMODIFY_READ_WRITE || (spec.shape.aggfinalmodify == AGGMODIFY_SHAREABLE && !spec.shared));
        if memo && s.add(STATE_INIT).read() == MEMO {
            let (v, n) = ((s.add(STATE_VALUE) as *const u64).read_unaligned(), s.add(STATE_NULL).read() != 0);
            let len = if n || self.specs[i].reslen == 0 { 0 } else { image_len(v as *const u8, self.specs[i].reslen) };
            out.write(result_word(self.specs[i].resword, v));
            out.add(1).write(n as u64);
            out.add(2).write(len as u64);
            return Ok(());
        }
        let given = (self.head.direct, self.head.given);
        let mut folded = [0u64; AGG_STATE_BYTES as usize / 8];
        let s = match self.specs[i].distinct {
            Some(_) => {
                // D1210: the group's run sorted, deduplicated and folded from the initcond into a state of its own (D1323: once)
                let f = folded.as_mut_ptr() as *mut u8;
                self.fold_once(k, i, aggno, s, f)?;
                f
            }
            None => s,
        };
        let spec = &self.specs[i];
        let Slot { aggs, node, tmp, .. } = &mut self.slots[k];
        let h = &mut aggs[i];
        AggArena::init_parts(node, spec, s, aggno)?;
        let (mut v, mut n) = (Datum::from_u64((s.add(STATE_VALUE) as *const u64).read_unaligned()), s.add(STATE_NULL).read() != 0);
        if let Some((f, fc, nargs)) = h.finalfn.as_mut() {
            if n { fc.set_arg_null(0) } else { fc.set_arg(0, read_only(v, spec.translen)) }
            (1..*nargs).for_each(|k| fc.set_arg_null(k));
            // V4R-425: an ordered-set aggregate's direct arguments after the state (finalize_aggregate's aggdirectargs; D1520:
            // one that is no constant as this final was given it, the group's)
            let mut anynull = n || *nargs > spec.direct.len() + 1;
            // bounded("small-fixed: one direct argument of the final call, at most FUNC_MAX_ARGS (100)")
            for k in 0..spec.direct.len().min(*nargs - 1) {
                let d = direct_arg(spec, k, given)?;
                if d.isnull { anynull = true } else { fc.set_arg(k + 1, d.value) }
            }
            if f.fn_strict && anynull {
                (v, n) = (Datum::null(), true);
            } else {
                fc.nargs = *nargs as i16;
                fc.isnull = false;
                fc.context = node.fm_node_ptr();
                fc.set_result_mcx(tmp.mcx());
                node.set_current_agg(spec.aggref, spec.shared);
                let r = f.invoke(&mut **fc);
                node.clear_current_agg();
                (v, n) = (r?, fc.isnull);
            }
        }
        let mut len = 0usize;
        if !n && spec.reslen != 0 {
            if spec.reslen == -1 && types_tuple::varatt::varatt_is_external_expanded(v.as_u64() as *const u8) {
                v = execexpr::agg_datum_copy(tmp.mcx(), v, -1)?;
            }
            len = image_len(v.as_u64() as *const u8, spec.reslen);
        }
        if memo {
            let kept = if n || spec.reslen == 0 { v } else { execexpr::agg_datum_copy(node.aggcontext(), v, spec.reslen)? };
            (s.add(STATE_VALUE) as *mut u64).write_unaligned(kept.as_u64());
            s.add(STATE_NULL).write(n as u8);
            s.add(STATE_INIT).write(MEMO);
        }
        out.write(if n { v.as_u64() } else { result_word(spec.resword, v.as_u64()) });
        out.add(1).write(n as u64);
        out.add(2).write(len as u64);
        Ok(())
    }
}

/// D1520: direct argument `k` of `spec` for one final: as `given` (the head's `direct` / `given` addresses, 0 outside
/// `m6_agg_final_direct`) gives it, else the constant bound at EXECUTE. One that is no constant is always given (lowerv4
/// face/aggcall.rs `finish`'s callers); a final without it is a named ERROR, never a stale value.
unsafe fn direct_arg(spec: &Spec, k: usize, (dv, dg): (u64, u64)) -> PgResult<NullableDatum> {
    let g = if dg == 0 { DIRECT_BOUND } else { (dg as *const u8).add(k).read() };
    match (g, spec.direct[k]) {
        (DIRECT_VALUE, _) => Ok(NullableDatum::value(Datum::from_u64((dv as *const u64).add(k).read()))),
        (DIRECT_NULL, _) => Ok(NullableDatum::null()),
        (_, Some(d)) => Ok(d),
        (_, None) => Err(Box::new(PgError::error(format!("gravity: hosted aggregate {} finalized without its direct argument {k}", spec.a.aggno)))), // alloc: Cold
    }
}

pub(super) unsafe fn trans_of(h: *mut AggHead, ordinal: u16, aggno: u32, state: *mut u8, args: *const u64, nulls: *const u8) -> Result<(), ()> {
    (*(h as *mut AggArena)).trans(ordinal, aggno, state, args, nulls).map_err(gravity_fn::stash_error)
}

pub(super) unsafe fn combine_of(h: *mut AggHead, ordinal: u16, aggno: u32, into: *mut u8, from: *mut u8) -> Result<(), ()> {
    let a = &mut *(h as *mut AggArena);
    let r = a.slot(ordinal).and_then(|k| a.combine_in(k, aggno, into, from));
    core::ptr::write_bytes(from, 0, AGG_STATE_BYTES as usize);
    r.map_err(gravity_fn::stash_error)
}

/// V4R-375: `combine_of` leaving `from` as it was (a keyed owner fold's read-only source partial).
pub(super) unsafe fn combine_keep_of(h: *mut AggHead, ordinal: u16, aggno: u32, into: *mut u8, from: *const u8) -> Result<(), ()> {
    let a = &mut *(h as *mut AggArena);
    a.slot(ordinal).and_then(|k| a.combine_in(k, aggno, into, from)).map_err(gravity_fn::stash_error)
}

/// V4R-375: keyed group `keys`'s home, `n` zeroed state slots at its first record (kernels `AggHead.home`); a wide
/// arena's holds `wide` per aggregate. Only the locked shards are shared: a wide arena's workers open homes at once.
/// CONVERT (wave 2, H1): the state and key boxes and the map's growth are noted on the shard's charge (`Homes::note`),
/// whose owner and kind are the keyed HashAgg slot's (`OpArena::own_owner_kind`: the slot `ctx`'s pipeline folds into, or
/// the one an emit's owner fold reads), read at the arena's first home.
pub(super) unsafe fn home_of(h: *mut AggHead, ctx: *mut Ctx, keys: *const u64, nkeys: usize, n: usize) -> *mut u8 {
    let a = &*(h as *const AggArena);
    let keys = core::slice::from_raw_parts(keys, nkeys);
    let mut hm = a.shard(home_shard(keys)).lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = hm.map.get_mut(keys) {
        return s.as_mut_ptr() as *mut u8;
    }
    let owner = *a.owner.get_or_init(|| gravity_fn::arena::OpArena::own_owner_kind(ctx));
    let mut s = vec![0u64; n * a.head.wide.max(1) as usize * AGG_STATE_BYTES as usize / 8].into_boxed_slice(); // alloc: Exec charged AggArena::owner (Homes::note)
    let p = s.as_mut_ptr() as *mut u8;
    let heap = 8 * (s.len() + nkeys) as u64;
    hm.bytes += heap + 32;
    hm.map.insert(keys.into(), s); // alloc: Exec charged AggArena::owner (Homes::note)
    hm.note(owner, heap, a.homes_batch());
    p
}

/// V4R-375: the homes forgotten, their states kept (kernels `AggHead.homes_clear`). D3062-b: not a wide arena's: each
/// worker's first claim lays its own region out afresh while the others' records already name the homes, which are the
/// open's (a re-open resets them, `reset`).
pub(super) unsafe fn homes_clear_of(h: *mut AggHead) {
    let a = &*(h as *const AggArena);
    if a.head.wide > 1 {
        return;
    }
    let (owner, batch) = (a.homes_owner(), a.homes_batch());
    // bounded("const-domain: one pass per made home shard, at most HOME_SHARDS (64); a pass moves the shard's states to its forgotten list")
    for m in a.shards() {
        let mut hm = m.lock().unwrap_or_else(|e| e.into_inner());
        let super::Homes { map, forgot, .. } = &mut *hm;
        forgot.extend(map.drain().map(|(_, s)| s)); // alloc: Exec charged AggArena::owner (Homes::note)
        hm.note(owner, 0, batch);
    }
}

/// D3062-b: a wide home's gathered state of one aggregate (kernels `AggHead.gather`). A group one worker reached reads
/// that worker's state as it is (no combine, no copy: a clustered key's usual case); none reached it, the empty last;
/// else each worker's state that holds one is combined, once, into the last (`combine_in` in the merge slot: an empty
/// one takes the first's copy). Null with the error stashed. The emit alone calls it (exec keeps the readers width 1),
/// so no worker runs a slot here.
pub(super) unsafe fn gather_of(h: *mut AggHead, subs: *mut u8) -> *mut u8 {
    let a = &mut *(h as *mut AggArena);
    let k = a.head.wide as usize - 1;
    let last = subs.add(k * AGG_STATE_BYTES as usize);
    let sub = |o: usize| subs.add(o * AGG_STATE_BYTES as usize);
    let mut held = (0..k).filter(|&o| sub(o).add(STATE_INIT).read() != 0);
    match (held.next(), held.next()) {
        (None, _) => return last,
        (Some(o), None) => return sub(o),
        _ => {}
    }
    if !a.gathered.insert(last as usize) {
        return last;
    }
    a.gathered_charged();
    for o in (0..k).filter(|&o| sub(o).add(STATE_INIT).read() != 0) {
        let aggno = (sub(o).add(STATE_AGGNO) as *const u32).read_unaligned();
        if let Err(e) = a.combine_in(k, aggno, last, sub(o)) {
            gravity_fn::stash_error(e);
            return core::ptr::null_mut();
        }
    }
    last
}

pub(super) unsafe fn fold_of(h: *mut AggHead, ordinal: u16, aggno: u32, state: *mut u8) -> Result<(), ()> {
    (*(h as *mut AggArena)).fold_ahead(ordinal, aggno, state).map_err(gravity_fn::stash_error)
}

pub(super) unsafe fn final_of(h: *mut AggHead, ordinal: u16, aggno: u32, state: *mut u8, out: *mut u64) -> Result<(), ()> {
    let a = &mut *(h as *mut AggArena);
    a.finalize(ordinal, aggno, state, out).map_err(gravity_fn::stash_error)?;
    let len = out.add(2).read() as usize;
    if a.head.stable != 0 && out.add(1).read() == 0 && len != 0 {
        // D2332: the image into the aggregate's own buffer (grown, reused), live until its next final
        if a.finals.len() <= aggno as usize {
            a.finals.resize_with(aggno as usize + 1, Vec::new); // alloc: Exec
        }
        let b = &mut a.finals[aggno as usize];
        b.clear();
        b.resize(len.div_ceil(8), 0); // alloc: Exec
        core::ptr::copy_nonoverlapping(out.read() as *const u8, b.as_mut_ptr() as *mut u8, len);
        out.write(b.as_ptr() as u64);
    }
    Ok(())
}
