//! The hosted DISTINCT form's multi-input fold (a `#[path]` child of hosts v4/m6_agg_distinct.rs, D1283): PG 18.6's
//! process_ordered_aggregate_multi over a group's kept rows. It sits in the family's raw half for its tuple descriptor's
//! lifetime erasure for the tuplesort API and its unsafe; the sort's root context comes from mem (`Root`, the same boxed
//! MemoryContext::new), where every root context is made.
#![allow(unsafe_code)]
use std::rc::Rc;

use datum::{Datum, NullableDatum};
use tuplesort::{Tuplesort, TUPLESORT_NONE};
use types_core::Oid;
use types_error::PgResult;
use types_fmgr::LocalFcinfo;
use types_slot::{SlotBase, TupleSlotKind};
use types_tuple::TupleDescData;

use crate::ops::group::hosted_distinct::shape;
use crate::ops::agg::trans::hosted::AggArena;

impl AggArena {
    /// D1283: process_ordered_aggregate_multi over the run at state slot `s` of DISTINCT aggregate `i`: the rows sorted as
    /// tuples of its inputs (ExecTypeFromTL of its arguments) by its DISTINCT columns (tuplesort_begin_heap under work_mem),
    /// a row equal to the one before it in every DISTINCT column dropped (NULL equals NULL; each column's eqop under its
    /// collation, as execTuplesMatchPrepare), each other row's inputs advanced into the fresh state at `out` in sort order.
    /// # Safety — as `fold_sorted`.
    pub(crate) unsafe fn fold_multi(&mut self, k: usize, i: usize, aggno: u32, s: *const u8, out: *mut u8) -> PgResult<()> {
        let d = self.specs[i].distinct.as_ref().expect("a DISTINCT spec");
        let (n, keys, dedup) = (d.cols.len(), d.keys.clone(), d.dedup());
        let ctx = gravity_driver::mem::root::Root::new("gravity DISTINCT sort");
        let m = ctx.mcx();
        let desc = execscan::exec_type_from_tl(m, &self.specs[i].a.args)?;
        // SAFETY: lifetime erasure for the tuplesort API only: the sort ends before `ctx` drops, as nodeagg's does.
        let sd: Rc<TupleDescData<'static>> = core::mem::transmute(desc.clone());
        let att: Vec<i16> = keys.iter().map(|x| x.0).collect(); // alloc: Exec
        let (ops, colls, nf): (Vec<Oid>, Vec<Oid>, Vec<bool>) = (keys.iter().map(|x| x.1).collect(), keys.iter().map(|x| x.2).collect(), keys.iter().map(|x| x.3).collect()); // alloc: Exec
        let mut sort = Tuplesort::begin_heap(sd, &att, &ops, &colls, &nf, crate::inputs::work_mem_kb(), TUPLESORT_NONE)?;
        let mut put = exectuples::make_tuple_table_slot(m, TupleSlotKind::Virtual, Some(desc.clone()));
        let mut step = cancelpoll::step("fold_multi: a DISTINCT aggregate's kept rows sorted and folded");
        for row in super::distinct::chunks(s).flat_map(|c| c.chunks_exact(2 * n)) {
            step.tick().map_err(|_| super::distinct::stopped())?;
            exectuples::exec_clear_tuple(&mut put, m);
            let b = put.base_mut();
            // bounded("key-columns: one cell per sort column of a row; PG polls no deeper (a datum copy is one unit there too), at most MaxTupleAttributeNumber (1664) columns")
            for c in 0..n {
                (b.tts_values[c], b.tts_isnull[c]) = (Datum::from_u64(row[2 * c]), row[2 * c + 1] != 0);
            }
            exectuples::exec_store_virtual_tuple(&mut put);
            sort.puttupleslot(&mut put, m)?;
        }
        sort.performsort()?;
        let mut cur = exectuples::make_tuple_table_slot(m, TupleSlotKind::MinimalTuple, Some(desc.clone()));
        let mut old = exectuples::make_tuple_table_slot(m, TupleSlotKind::MinimalTuple, Some(desc));
        let (mut have, mut vals, mut nulls) = (false, vec![0u64; n], vec![0u8; n]); // alloc: Exec
        while sort.gettupleslot(true, true, &mut cur, m)? {
            step.tick().map_err(|_| super::distinct::stopped())?;
            exectuples::slot_getsomeattrs(&mut cur, n as i32);
            if !(have && dedup && self.same_row(k, i, &keys, cur.base(), old.base())?) {
                let b = cur.base();
                // bounded("key-columns: one cell per sort column of a row; PG polls no deeper (a datum copy is one unit there too), at most MaxTupleAttributeNumber (1664) columns")
                for c in 0..n {
                    (vals[c], nulls[c]) = (b.tts_values[c].as_u64(), b.tts_isnull[c] as u8);
                }
                self.advance(k, i, aggno, out, vals.as_ptr(), nulls.as_ptr())?;
                core::mem::swap(&mut cur, &mut old);
                have = true;
            }
            exectuples::exec_clear_tuple(&mut cur, m);
        }
        sort.end();
        Ok(())
    }

    /// Whether rows `a` and `b` agree in every DISTINCT column `keys` (execTuplesMatch: both NULL, or neither and the
    /// column's equality function true under its collation).
    fn same_row(&mut self, k: usize, i: usize, keys: &[(i16, Oid, Oid, bool)], a: &SlotBase<'_>, b: &SlotBase<'_>) -> PgResult<bool> {
        let slot = &mut self.slots[k];
        // bounded("key-columns: one compare per DISTINCT key of a row; PG polls no deeper (a datum compare is one unit there too), at most MaxTupleAttributeNumber (1664) keys")
        for (c, &(att, _, coll, _)) in keys.iter().enumerate().rev() {
            let j = att as usize - 1;
            match (a.tts_isnull[j], b.tts_isnull[j]) {
                (true, true) => continue,
                (false, false) => {}
                _ => return Ok(false),
            }
            let Some(eq) = slot.aggs[i].eq.get_mut(c) else { return Err(shape("a DISTINCT aggregate without its equality function")) };
            let mut fc = LocalFcinfo::<2>::fresh(coll);
            // SAFETY: the result context is the slot's scratch, reset only after the call's result is read.
            unsafe { fc.set_result_mcx(slot.tmp.mcx()) };
            fc.args[0] = NullableDatum { value: a.tts_values[j], isnull: false };
            fc.args[1] = NullableDatum { value: b.tts_values[j], isnull: false };
            let r = eq.invoke(&mut fc);
            slot.tmp.reset();
            if !r?.as_bool() {
                return Ok(false);
            }
        }
        Ok(true)
    }
}
