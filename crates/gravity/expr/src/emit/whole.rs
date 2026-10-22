//! The whole-row `Var` in the emitter (expr.md §7; the seam for setops' `srf_wholerow.rs`): the scan slot's arrays and
//! the rowtype's descriptor come from `RowRefs::whole` (the owner checks the slot at the fill), the host forms the tuple

use gravity_fn::{Datum, Region};
use gravity_ir::{HostFlags, IntCond, StateMask, Type};
use types_tuple::{varatt, TupleDescData};

use super::scalar::Em;
use super::{EmitError, Lowered};
use crate::eval::Source;
use crate::nodes::tuple::{form, row_cells};
use crate::views::{datum_of, detoast_into};

gravity_abi::host! {
    /// `heap_form_tuple` of the slot's arrays over `desc` into `region`: the image's address, 0 when it raised.
    /// D3294: under the landing pad. An external field's detoast reads the TOAST relation into a memory context, whose
    /// allocation may raise out of memory by unwinding (a statement memory limit, the governor); that unwind must not cross
    /// this `extern "C"` frame (the server would abort). The pad parks it and answers 0; it resumes once the program returns.
    /// # Safety — `region` is a live `Region`, `desc` a live `TupleDescData`, the slot arrays `desc.natts` long.
    #[guard(0)]
    pub unsafe fn form_row(region: u64, values: u64, nulls: u64, desc: u64) -> u64 {
        let region = &mut *(region as *mut Region);
        let desc = &*(desc as *const TupleDescData<'static>);
        let natts = desc.natts as usize;
        let values = core::slice::from_raw_parts(values as *const i64, natts);
        let nulls = core::slice::from_raw_parts(nulls as *const u8, natts);
        let Ok((vals, nls)) = row_cells(region, natts) else { return 0 };
        for i in 0..natts {
            vals[i] = Datum::from_i64(values[i]);
            nls[i] = nulls[i] != 0;
            if !nls[i] && desc.attrs[i].attlen == -1 {
                let p = vals[i].as_usize() as *const u8;
                if varatt::varatt_is_1b_e(p) {
                    match detoast_into(region, p) {
                        Ok(v) => vals[i] = datum_of(v),
                        Err(_) => return 0,
                    }
                }
            }
        }
        match form(desc, vals, nls, region) {
            Ok(v) => v.value.as_u64(),
            Err(_) => 0,
        }
    }
}

impl Em<'_, '_, '_> {
    pub(super) fn whole_row(&mut self, src: Source) -> Result<Lowered, EmitError> {
        let (Some(w), Source::Scan) = (self.row.whole, src) else { return Err(EmitError::NoColumn(0)) };
        let Some(region) = self.row.region else { return Err(EmitError::NoCells) };
        let flags = HostFlags { may_error: true, pure: false, touches_memory: true, worker: false, state: StateMask::NONE };
        let h = self.b.declare_host_once("gravity_expr::form_row", &[Type::Ptr, Type::Ptr, Type::Ptr, Type::I64], Some(Type::I64), form_row as *const () as usize, flags);
        let p = self.b.call_host(h, &[region, w.values, w.nulls, w.desc], Some(Type::I64)).unwrap();
        let failed = self.b.icmp(IntCond::Eq, p, self.zero);
        let err = super::prim::raise(self.b, failed, gravity_abi::status::raise::HOST);
        Ok(Lowered { value: p, valid: self.t, err })
    }
}
