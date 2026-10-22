//! `receivers`' unsafe step (rule R6, the module's raw home): a DestSink's receiver, borrowed back from its pointer.
#![allow(unsafe_code)]

use super::*;

impl DestSink {
    /// The receiver `new` was given, through the sink's `dest` field (a field borrow, so the sink's other fields stay
    /// free beside it).
    pub(super) fn dest_mut<'a>(dest: &'a mut *mut DestReceiver<'static>) -> &'a mut DestReceiver<'static> {
        // SAFETY: the receiver of the running execute call outlives the run.
        unsafe { &mut **dest }
    }
}

impl DestSink {
    /// `sqlfunction_receive`'s first-row keep (sql_functions lib.rs): the row through the junk map (1-based source columns,
    /// 0 = a dropped column's NULL) into the junkfilter's slot, materialized there; a later row is ignored.
    pub(super) fn keep_first(dest: &mut DestReceiver<'static>, values: &[u64], isnull: &[u8]) -> types_error::PgResult<()> {
        let DestReceiver::SqlFunction(st) = dest else { return Ok(()) };
        let Some((mut keep, keep_mcx)) = st.row_slot else { return Ok(()) };
        // SAFETY: row_slot is the live fcache's junkfilter slot (postquel_start), borrowed by nothing else while the
        // function's statement runs.
        let keep = unsafe { keep.as_mut() };
        if !keep.base().is_empty() {
            return Ok(());
        }
        let k = keep.base_mut();
        for (j, &src) in st.clean_map.iter().enumerate() {
            match src {
                0 => (k.tts_values[j], k.tts_isnull[j]) = (datum::Datum::null(), true),
                s => (k.tts_values[j], k.tts_isnull[j]) = (datum::Datum::from_u64(values[s as usize - 1]), isnull[s as usize - 1] != 0),
            }
        }
        exectuples::exec_store_virtual_tuple(keep);
        exectuples::exec_materialize_slot(keep, keep_mcx)
    }
}
