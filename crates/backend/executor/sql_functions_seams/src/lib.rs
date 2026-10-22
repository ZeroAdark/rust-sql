use std::ptr::NonNull;
use std::rc::Rc;

use datum::Datum;
use mcx::{Mcx, PgVec};
use types_error::PgResult;
use types_portal::TuplestoreHandle;
use types_slot::SlotData;
use types_tuple::TupleDescData;

/// DR_sqlfunction marshal shape; clean_map holds 1-based source resnos, 0 = dropped-column NULL.
pub struct SqlFunctionDestState<'mcx> {
    pub tstore: TuplestoreHandle,
    pub clean_desc: Rc<TupleDescData<'mcx>>,
    pub clean_map: &'mcx [i16],
    pub row_slot: Option<(NonNull<SlotData<'mcx>>, Mcx<'mcx>)>,
    pub values: PgVec<'mcx, Datum>,
    pub isnull: PgVec<'mcx, bool>,
}

seam_core::seam!(
    pub fn sqlfunction_receive<'mcx>(
        state: &mut SqlFunctionDestState<'mcx>,
        slot: &mut SlotData<'mcx>,
    ) -> PgResult<bool>
);
