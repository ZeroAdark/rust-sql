//! hosted_kept's unsafe (R6 home; its child): the kept column's first-row keep and final over the state slot, the
//! partials' combine, and a by-reference image's length. Moved from hosted_kept.rs unchanged.
#![allow(unsafe_code)]
use datum::Datum;
use gravity_kernels::ops::agg::trans::hosted::{AGG_STATE_BYTES, STATE_AGGNO, STATE_INIT, STATE_NOTRANS, STATE_NULL, STATE_VALUE};
use types_error::PgResult;

use crate::ops::agg::trans::hosted::AggArena;

impl AggArena {
    /// D345: the first call on a group's kept slot keeps its value (a NULL too) as PG's representative tuple holds it: a
    /// by-reference one copied into slot `k`'s aggcontext, an external varlena detoasted there; later calls keep it.
    pub(crate) unsafe fn keep_first(&mut self, k: usize, j: usize, aggno: u32, s: *mut u8, v: u64, null: bool) -> PgResult<()> {
        if s.add(STATE_INIT).read() != 0 {
            return Ok(());
        }
        let (typlen, byval) = self.kept[j];
        let mcx = self.slots[k].node.aggcontext();
        let p = v as *const u8;
        let v = match (null || byval, typlen) {
            (true, _) => v,
            (false, -1) if types_tuple::varatt::varatt_is_1b_e(p) => {
                let img = core::slice::from_raw_parts(p, types_tuple::varatt::varsize_any(p));
                detoast_seams::detoast_attr::call(mcx, img)?.leak().as_ptr() as u64
            }
            _ => execexpr::agg_datum_copy(mcx, Datum::from_u64(v), typlen)?.as_u64(),
        };
        (s.add(STATE_VALUE) as *mut u64).write_unaligned(if null { 0 } else { v });
        s.add(STATE_NULL).write(null as u8);
        s.add(STATE_NOTRANS).write(0);
        (s.add(STATE_AGGNO) as *mut u32).write_unaligned(aggno);
        s.add(STATE_INIT).write(1);
        Ok(())
    }

    /// The kept value at `out[0..3]` as a no-finalfn aggregate's final (a group with no row: NULL, PG's all-NULL
    /// representative tuple).
    pub(crate) unsafe fn kept_final(&self, j: usize, s: *mut u8, out: *mut u64) {
        let (typlen, byval) = self.kept[j];
        let n = s.add(STATE_INIT).read() == 0 || s.add(STATE_NULL).read() != 0;
        let v = if n { 0 } else { (s.add(STATE_VALUE) as *const u64).read_unaligned() };
        out.write(v);
        out.add(1).write(n as u64);
        out.add(2).write(if n || byval { 0 } else { image_len(v as *const u8, typlen) as u64 });
    }
}

/// Two partials of one group (D798): the kept column keeps `into`'s row, the earlier partial's; an empty `into` takes
/// `from`'s (its image lives in the same arena).
pub(crate) unsafe fn combine(into: *mut u8, from: *const u8) {
    if into.add(STATE_INIT).read() == 0 {
        core::ptr::copy_nonoverlapping(from, into, AGG_STATE_BYTES as usize);
    }
}

/// The bytes of a by-reference image of `typlen` at `p`.
pub(crate) unsafe fn image_len(p: *const u8, typlen: i16) -> usize {
    match typlen {
        -1 => types_tuple::varatt::varsize_any(p),
        -2 => std::ffi::CStr::from_ptr(p.cast()).to_bytes_with_nul().len(),
        l => l as usize,
    }
}
