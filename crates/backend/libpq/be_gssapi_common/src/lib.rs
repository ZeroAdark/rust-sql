//! be-gssapi-common.c over the dlopened system GSSAPI library (gss_ffi).

#![allow(non_snake_case)]

pub mod gss_ffi;

use std::ffi::CString;

use pgsync::Mutex;

use elog::ereport;
use types_error::{ErrorLocation, PgResult, COMMERROR};

use crate::gss_ffi::{
    gss_buffer_desc, gss_cred_id_t, gss_key_value_element_desc, gss_key_value_set_desc, GssApi,
    GSS_C_ACCEPT, GSS_C_GSS_CODE, GSS_C_INITIATE, GSS_C_MECH_CODE, GSS_S_COMPLETE,
};

pub fn gloc(line: i32, func: &'static str) -> ErrorLocation {
    ErrorLocation::new("src/backend/libpq/be-gssapi-common.c", line, func)
}

fn pg_gss_error_int(api: &GssApi, stat: u32, status_type: i32) -> String {
    let mut s = String::new();
    let mut msg_ctx: u32 = 0;
    loop {
        let mut lmin_s: u32 = 0;
        let mut gmsg = gss_buffer_desc::empty();
        // SAFETY: out-params are valid; gmsg is released after copying.
        let maj = unsafe {
            (api.gss_display_status)(
                &mut lmin_s,
                stat,
                status_type,
                core::ptr::null_mut(),
                &mut msg_ctx,
                &mut gmsg,
            )
        };
        if maj != GSS_S_COMPLETE {
            break;
        }
        if !s.is_empty() {
            s.push(' ');
        }
        if !gmsg.value.is_null() {
            // SAFETY: gss_display_status returned length valid bytes.
            let bytes = unsafe {
                core::slice::from_raw_parts(gmsg.value as *const u8, gmsg.length)
            };
            s.push_str(&String::from_utf8_lossy(bytes));
        }
        // SAFETY: gmsg came from gss_display_status.
        unsafe { (api.gss_release_buffer)(&mut lmin_s, &mut gmsg) };
        if msg_ctx == 0 {
            break;
        }
    }
    s
}

pub fn pg_GSS_error(errmsg: &str, maj_stat: u32, min_stat: u32) -> PgResult<()> {
    let api = match crate::gss_ffi::try_gss() {
        Ok(api) => api,
        Err(_) => {
            return ereport(COMMERROR)
                .errmsg_internal(errmsg.to_string())
                .finish(gloc(91, "pg_GSS_error"));
        }
    };
    let msg_major = pg_gss_error_int(api, maj_stat, GSS_C_GSS_CODE);
    let msg_minor = pg_gss_error_int(api, min_stat, GSS_C_MECH_CODE);
    ereport(COMMERROR)
        .errmsg_internal(errmsg.to_string())
        .errdetail_internal(format!("{msg_major}: {msg_minor}"))
        .finish(gloc(91, "pg_GSS_error"))
}

const GSS_MEMORY_CACHE: &str = "MEMORY:";

// setenv is process-global here (backends are threads): serialize the
// fallback keytab / delegated-ccache writes.
static GSS_ENV_LOCK: Mutex<()> = Mutex::new(());

pub fn gss_setenv(key: &std::ffi::CStr, val: &std::ffi::CStr) -> i32 {
    let _guard = GSS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: NUL-terminated strings; serialized against other GSS env writes.
    unsafe { libc::setenv(key.as_ptr(), val.as_ptr(), 1) }
}

pub fn pg_store_delegated_credential(api: &GssApi, mut cred: gss_cred_id_t) -> PgResult<()> {
    let key = c"ccache";
    let value = CString::new(GSS_MEMORY_CACHE).expect("no interior NUL");
    let mut cc = gss_key_value_element_desc { key: key.as_ptr(), value: value.as_ptr() };
    let ccset = gss_key_value_set_desc { count: 1, elements: &mut cc };

    let Some(store_cred_into) = api.gss_store_cred_into else {
        pg_GSS_error("gss_store_cred", GSS_S_COMPLETE, 0)?;
        return Ok(());
    };

    let mut minor: u32 = 0;
    let mut mech: crate::gss_ffi::gss_OID_set = core::ptr::null_mut();
    let mut usage: crate::gss_ffi::gss_cred_usage_t = 0;
    // SAFETY: cred is a live delegated credential; ccset points at live data.
    let major = unsafe {
        store_cred_into(
            &mut minor,
            cred,
            GSS_C_INITIATE,
            core::ptr::null_mut(),
            1,
            1,
            &ccset,
            &mut mech,
            &mut usage,
        )
    };
    if major != GSS_S_COMPLETE {
        pg_GSS_error("gss_store_cred", major, minor)?;
    }

    // SAFETY: cred is live; released exactly once.
    let major = unsafe { (api.gss_release_cred)(&mut minor, &mut cred) };
    if major != GSS_S_COMPLETE {
        pg_GSS_error("gss_release_cred", major, minor)?;
    }

    gss_setenv(c"KRB5CCNAME", value.as_c_str());
    Ok(())
}

pub fn acquire_keytab_cred(
    api: &GssApi,
    keyfile: &std::ffi::CStr,
) -> Result<gss_cred_id_t, (u32, u32)> {
    let acquire_cred_from = api
        .gss_acquire_cred_from
        .expect("acquire_keytab_cred: caller checked gss_acquire_cred_from");

    let key = c"keytab";
    let mut kt = gss_key_value_element_desc { key: key.as_ptr(), value: keyfile.as_ptr() };
    let ktset = gss_key_value_set_desc { count: 1, elements: &mut kt };

    let mut minor: u32 = 0;
    let mut cred: gss_cred_id_t = core::ptr::null_mut();
    // SAFETY: null name/mechs select the default acceptor; ktset outlives the call.
    let major = unsafe {
        acquire_cred_from(
            &mut minor,
            core::ptr::null_mut(),
            0,
            core::ptr::null_mut(),
            GSS_C_ACCEPT,
            &ktset,
            &mut cred,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
        )
    };
    if major != GSS_S_COMPLETE {
        return Err((major, minor));
    }
    Ok(cred)
}

