//! fmgr.c fmgr_security_definer around a natively dispatched support proc:
//! fmgr_info_cxt_security routes a proc whose pg_proc row has prosecdef or
//! proconfig through the wrapper, so initGinState's fmgr_info_copy'd
//! FmgrInfos carry it; the native dispatch keeps the same bracket here.
//! Transaction abort restores user and GUC state after an error.

use ::types_core::{InvalidOid, Oid};
use ::types_error::PgResult;

pub(crate) fn call<T>(proc_oid: Oid, f: impl FnOnce() -> PgResult<T>) -> PgResult<T> {
    if proc_oid == InvalidOid {
        return f();
    }
    let Some(row) = ::syscache_seams::lookup_pg_proc_secdef::call(proc_oid)? else {
        return Err(Box::new(
            ::types_error::PgError::error(format!("cache lookup failed for function {proc_oid}"))
                .with_sqlstate(::types_error::ERRCODE_INTERNAL_ERROR),
        ));
    };
    let (save_userid, save_sec_context) = ::miscinit_seams::get_user_id_and_sec_context::call();
    let save_nestlevel = if row.proconfig.is_some() {
        ::guc_seams::new_guc_nest_level::call()
    } else {
        0
    };
    if row.prosecdef {
        ::miscinit_seams::set_user_id_and_sec_context::call(
            row.proowner,
            save_sec_context | ::types_core::SECURITY_LOCAL_USERID_CHANGE,
        );
    }
    if let Some(cfg) = &row.proconfig {
        ::guc_seams::process_guc_array_secdef::call(cfg)?;
    }
    let result = f()?;
    if row.proconfig.is_some() {
        ::guc_seams::at_eoxact_guc::call(true, save_nestlevel)?;
    }
    if row.prosecdef {
        ::miscinit_seams::set_user_id_and_sec_context::call(save_userid, save_sec_context);
    }
    Ok(result)
}
