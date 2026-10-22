//! bitmap_param's unsafe (lint R6; construct bundle scan-05; its raw.rs child): `ExecVals::set`, the one write into a
//! parameter cell, moved as is from bitmap_param.rs with its safety contract.
#![allow(unsafe_code)]
use super::*;

impl ExecVals {
    pub(crate) fn set(&self, id: usize, value: Word, isnull: bool) {
        // SAFETY: the leader's source is the only writer and the recheck (on the same thread, after this seek) the only
        // reader (the module doc); a cell never escapes a call
        unsafe { *self.0[id].get() = ParamExecData { value: datum::Datum::from_u64(value), isnull, exec_plan: false } };
    }
}
