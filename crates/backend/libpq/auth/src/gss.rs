//! auth.c GSSAPI arm (pg_GSS_recvauth / pg_GSS_checkauth) over the dlopened
//! system GSSAPI library. When the connection negotiated GSSAPI
//! encryption (be_secure::gssapi) the context already exists and only the
//! principal check runs.

use core::ffi::c_void;
use std::ffi::CString;

use elog::{elog, ereport, PCT_M};
use mcx::MemoryContext;
use types_error::{PgResult, COMMERROR, DEBUG2, DEBUG4, DEBUG5, FATAL};
use types_startup::Port;

use be_gssapi_common::gss_ffi::{
    self, gss_buffer_desc, gss_cred_id_t, gss_ctx_id_t, gss_name_t, GssApi, GSS_C_DELEG_FLAG,
    GSS_S_COMPLETE, GSS_S_CONTINUE_NEEDED,
};
use be_gssapi_common::{
    acquire_keytab_cred, gss_setenv, pg_GSS_error, pg_store_delegated_credential,
};
use crate::{
    loc, sendAuthRequest, set_authn_id, AUTH_REQ_GSS_CONT, PG_MAX_AUTH_TOKEN_LENGTH,
    PqMsg_GSSResponse, STATUS_ERROR,
};

struct GssState<'a> {
    api: &'static GssApi,
    ctx: gss_ctx_id_t,
    name: gss_name_t,
    // Acceptor credential acquired from the configured keytab via the
    // thread-safe credential-store API (GSS_C_NO_CREDENTIAL when the keytab is
    // selected via the environment fallback instead). Released on drop.
    acceptor_cred: gss_cred_id_t,
    outbuf: gss_buffer_desc,
    port: &'a mut Port,
}

impl Drop for GssState<'_> {
    fn drop(&mut self) {
        let mut lmin_s: u32 = 0;
        if !self.ctx.is_null() {
            // SAFETY: live context from gss_accept_sec_context.
            unsafe {
                (self.api.gss_delete_sec_context)(&mut lmin_s, &mut self.ctx, core::ptr::null_mut())
            };
        }
        if !self.name.is_null() {
            // SAFETY: live name from gss_accept_sec_context.
            unsafe { (self.api.gss_release_name)(&mut lmin_s, &mut self.name) };
        }
        if !self.acceptor_cred.is_null() {
            // SAFETY: live credential from gss_acquire_cred_from.
            unsafe { (self.api.gss_release_cred)(&mut lmin_s, &mut self.acceptor_cred) };
        }
        if self.outbuf.length != 0 {
            // SAFETY: an output token gss_accept_sec_context allocated that an
            // early return left unreleased.
            unsafe { (self.api.gss_release_buffer)(&mut lmin_s, &mut self.outbuf) };
        }
    }
}

// auth.c pg_GSS_recvauth (auth.c:921).
pub(crate) fn pg_GSS_recvauth(port: &mut Port) -> PgResult<i32> {
    let api = match gss_ffi::try_gss() {
        Ok(api) => api,
        Err(e) => {
            // An ENABLE_GSS C build links the library at build time; the
            // runtime dlopen miss degrades to the context-failure report.
            ereport(COMMERROR)
                .errmsg_internal("accepting GSS security context failed")
                .errdetail_internal(e.to_string())
                .finish(loc(1049, "pg_GSS_recvauth"))?;
            return Ok(STATUS_ERROR);
        }
    };

    // Use the configured keytab, if there is one. C sets KRB5_KTNAME in the
    // per-backend process environment (auth.c:944) and lets the first
    // gss_accept_sec_context call open the keytab; that env write is unsafe
    // here because every backend is a thread in one shared process. On MIT
    // builds the keytab is bound to an acceptor credential through the
    // thread-safe credential-store API instead — acquired inside the loop
    // below, right before the first gss_accept_sec_context call, so an
    // unusable keytab fails where C's does (after the client's first token
    // has been read) and with C's report ("accepting GSS security context
    // failed", auth.c:1045-1049). Only fall back to the (serialized)
    // environment variable when the extension is unavailable (e.g. Heimdal /
    // macOS GSS.framework).
    let keyfile = guc_tables::vars::pg_krb_server_keyfile.read().unwrap_or_default();
    let mut pending_keytab: Option<CString> = None;
    if !keyfile.is_empty() {
        match CString::new(keyfile.as_str()) {
            Ok(kt) if api.gss_acquire_cred_from.is_some() => pending_keytab = Some(kt),
            // Extension unavailable, or a NUL in the configured path (which
            // can never name a real keytab): the env fallback / default
            // keytab handles it exactly as before.
            _ => {
                let key = c"KRB5_KTNAME";
                let val = CString::new(keyfile).unwrap_or_default();
                if gss_setenv(key, val.as_c_str()) != 0 {
                    let errnum = elog::errno::current_errno();
                    // The only likely failure cause is OOM, so use that errcode.
                    return ereport(FATAL)
                        .with_saved_errno(errnum)
                        .errcode(types_error::ERRCODE_OUT_OF_MEMORY)
                        .errmsg(format!("could not set environment: {PCT_M}"))
                        .finish(loc(944, "pg_GSS_recvauth"))
                        .map(|()| STATUS_ERROR);
                }
            }
        }
    }

    let mut state = GssState {
        api,
        ctx: core::ptr::null_mut(),
        name: core::ptr::null_mut(),
        acceptor_cred: core::ptr::null_mut(),
        outbuf: gss_buffer_desc::empty(),
        port,
    };
    let mut delegated_creds: gss_cred_id_t = core::ptr::null_mut();

    loop {
        pqcomm::pq_startmsgread()?;
        postgres_seams::check_for_interrupts::call()?;

        let mtype = pqcomm::pq_getbyte()?;
        if mtype != PqMsg_GSSResponse as i32 {
            // Only log error if client didn't disconnect.
            if mtype != -1 {
                ereport(types_error::ERROR)
                    .errcode(types_error::ERRCODE_PROTOCOL_VIOLATION)
                    .errmsg(format!("expected GSS response, got message type {mtype}"))
                    .finish(loc(983, "pg_GSS_recvauth"))?;
            }
            return Ok(STATUS_ERROR);
        }

        let scratch = MemoryContext::new("pg_GSS_recvauth");
        let mut buf = stringinfo::StringInfo::new_in(scratch.mcx())?;
        if pqcomm::pq_getmessage(&mut buf, PG_MAX_AUTH_TOKEN_LENGTH)? != 0 {
            return Ok(STATUS_ERROR); // EOF - pq_getmessage already logged
        }

        let token = buf.as_bytes();
        let mut gbuf = gss_buffer_desc {
            length: token.len(),
            value: token.as_ptr() as *mut c_void,
        };

        elog(DEBUG4, format!("processing received GSS token of length {}", gbuf.length))?;

        // First token: bind the configured keytab to the acceptor credential
        // now (see above). C's gss_accept_sec_context with GSS_C_NO_CREDENTIAL
        // opens KRB5_KTNAME at this same point, so a bad keytab is reported
        // here as a context-accept failure (auth.c:1049).
        if let Some(kt) = pending_keytab.take() {
            match acquire_keytab_cred(api, &kt) {
                Ok(cred) => state.acceptor_cred = cred,
                Err((maj_stat, min_stat)) => {
                    pg_GSS_error("accepting GSS security context failed", maj_stat, min_stat)?;
                    return Ok(STATUS_ERROR);
                }
            }
        }

        let mut min_stat: u32 = 0;
        let mut gflags: u32 = 0;
        let accept_delegation = guc_tables::vars::pg_gss_accept_delegation.read();
        // SAFETY: all pointers are live for the call; gbuf borrows the
        // message buffer which outlives it; outbuf/name are owned by state.
        let maj_stat = unsafe {
            (api.gss_accept_sec_context)(
                &mut min_stat,
                &mut state.ctx,
                state.acceptor_cred,
                &mut gbuf,
                core::ptr::null_mut(),
                &mut state.name,
                core::ptr::null_mut(),
                &mut state.outbuf,
                &mut gflags,
                core::ptr::null_mut(),
                if accept_delegation { &mut delegated_creds } else { core::ptr::null_mut() },
            )
        };

        elog(
            DEBUG5,
            format!(
                "gss_accept_sec_context major: {}, minor: {}, outlen: {}, outflags: {:x}",
                maj_stat, min_stat, state.outbuf.length, gflags
            ),
        )?;

        postgres_seams::check_for_interrupts::call()?;

        if !delegated_creds.is_null() && gflags & GSS_C_DELEG_FLAG != 0 {
            pg_store_delegated_credential(api, delegated_creds)?;
            state.port.gss.as_mut().unwrap().delegated = true;
            delegated_creds = core::ptr::null_mut();
        }

        if state.outbuf.length != 0 {
            elog(DEBUG4, format!("sending GSS response token of length {}", state.outbuf.length))?;

            // SAFETY: outbuf holds length valid bytes from the library.
            let out = unsafe {
                core::slice::from_raw_parts(state.outbuf.value as *const u8, state.outbuf.length)
            };
            let send_result = sendAuthRequest(state.port, AUTH_REQ_GSS_CONT, out);

            let mut lmin_s: u32 = 0;
            // SAFETY: outbuf came from gss_accept_sec_context.
            unsafe { (api.gss_release_buffer)(&mut lmin_s, &mut state.outbuf) };
            state.outbuf = gss_buffer_desc::empty();
            send_result?;
        }

        if maj_stat != GSS_S_COMPLETE && maj_stat != GSS_S_CONTINUE_NEEDED {
            // state's Drop deletes the security context.
            pg_GSS_error("accepting GSS security context failed", maj_stat, min_stat)?;
            return Ok(STATUS_ERROR);
        }

        if maj_stat == GSS_S_CONTINUE_NEEDED {
            elog(DEBUG4, "GSS continue needed")?;
            continue;
        }
        break;
    }

    let (api, name) = (state.api, state.name);
    pg_GSS_checkauth(api, name, &mut *state.port)
}

// auth.c uaGSS arm with port->gss->enc: the encryption handshake already
// established the context; only the principal check runs.
pub(crate) fn pg_GSS_checkauth_encrypted(port: &mut Port) -> PgResult<i32> {
    let api = gss_ffi::try_gss().expect("encrypted connection without a GSSAPI library");
    pg_GSS_checkauth(api, be_secure::gssapi::be_gssapi_context_name(), port)
}

// auth.c pg_GSS_checkauth (auth.c:1074): map the authenticated principal to
// the claimed username.
fn pg_GSS_checkauth(api: &GssApi, name: gss_name_t, port: &mut Port) -> PgResult<i32> {

    let mut min_stat: u32 = 0;
    let mut gbuf = gss_buffer_desc::empty();
    // SAFETY: name is the live src_name from the completed context.
    let maj_stat =
        unsafe { (api.gss_display_name)(&mut min_stat, name, &mut gbuf, core::ptr::null_mut()) };
    if maj_stat != GSS_S_COMPLETE {
        pg_GSS_error("retrieving GSS user name failed", maj_stat, min_stat)?;
        return Ok(STATUS_ERROR);
    }

    // SAFETY: gss_display_name returned length valid bytes.
    let princ_bytes =
        unsafe { core::slice::from_raw_parts(gbuf.value as *const u8, gbuf.length) };
    // C copies gbuf by length into a NUL-terminated string (memcpy + trailing
    // '\0'); an interior NUL survives that copy and later truncation-matches the
    // realm/usermap via pg_strcasecmp/strcmp. Reject it here so the principal is
    // a single, NUL-free value for realm matching, usermap matching, and the
    // authenticated identity — matching how the cert DN/CN identity rejects
    // embedded nulls before its C-string comparisons.
    let has_interior_nul = princ_bytes.contains(&0);
    let princ = String::from_utf8_lossy(princ_bytes).into_owned();
    let mut lmin_s: u32 = 0;
    // SAFETY: gbuf came from gss_display_name.
    unsafe { (api.gss_release_buffer)(&mut lmin_s, &mut gbuf) };

    if has_interior_nul {
        ereport(COMMERROR)
            .errcode(types_error::ERRCODE_PROTOCOL_VIOLATION)
            .errmsg("GSS authenticated name contains embedded null")
            .finish(loc(1100, "pg_GSS_checkauth"))?;
        return Ok(STATUS_ERROR);
    }

    // The principal is our authenticated identity: set it before the usermap
    // check, because authentication has already succeeded.
    set_authn_id(port, &princ)?;
    port.gss.as_mut().unwrap().principal = Some(princ.clone());

    let hba = port.hba.as_ref().expect("pg_GSS_checkauth: port->hba is NULL");
    let caseins = guc_tables::vars::pg_krb_caseins_users.read();

    let map_user =
        match map_principal(&princ, hba.include_realm, hba.krb_realm.as_deref(), caseins) {
            Ok(u) => u,
            Err(RealmMismatch::Mismatch { realm, configured }) => {
                elog(
                    DEBUG2,
                    format!("GSSAPI realm ({realm}) and configured realm ({configured}) don't match"),
                )?;
                return Ok(STATUS_ERROR);
            }
            Err(RealmMismatch::NoRealm) => {
                elog(DEBUG2, "GSSAPI did not return realm but realm matching was requested")?;
                return Ok(STATUS_ERROR);
            }
        };

    hba::check_usermap(
        hba.usermap.as_deref(),
        port.user_name.as_deref().unwrap_or(""),
        map_user.as_bytes(),
        caseins,
    )
}

enum RealmMismatch<'a> {
    Mismatch { realm: &'a str, configured: &'a str },
    NoRealm,
}

// The realm split/match half of pg_GSS_checkauth: strip the realm unless
// include_realm, then enforce krb_realm (empty behaves as unset, C's strlen
// check; case-insensitivity follows krb_caseins_users).
fn map_principal<'a>(
    princ: &'a str,
    include_realm: bool,
    krb_realm: Option<&'a str>,
    caseins: bool,
) -> Result<&'a str, RealmMismatch<'a>> {
    match princ.find('@') {
        Some(at) => {
            let realm = &princ[at + 1..];
            if let Some(configured) = krb_realm.filter(|r| !r.is_empty()) {
                // Match the realm part of the name first.
                let matches = if caseins {
                    hba::pg_strcasecmp(configured.as_bytes(), realm.as_bytes()) == 0
                } else {
                    configured == realm
                };
                if !matches {
                    return Err(RealmMismatch::Mismatch { realm, configured });
                }
            }
            Ok(if include_realm { princ } else { &princ[..at] })
        }
        None => {
            if krb_realm.is_some_and(|r| !r.is_empty()) {
                return Err(RealmMismatch::NoRealm);
            }
            Ok(princ)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{gss_setenv, map_principal};

    // An early return (interrupt between gss_accept_sec_context and the
    // send-and-release block) drops GssState with outbuf still populated;
    // Drop must hand that token back to the library.
    #[test]
    fn drop_releases_unsent_output_token() {
        use core::ffi::{c_int, c_void};
        use std::sync::atomic::{AtomicUsize, Ordering};

        use be_gssapi_common::gss_ffi::*;

        static RELEASED: AtomicUsize = AtomicUsize::new(0);
        unsafe extern "C" fn accept(
            _: *mut OM_uint32,
            _: *mut gss_ctx_id_t,
            _: gss_cred_id_t,
            _: *mut gss_buffer_desc,
            _: gss_channel_bindings_t,
            _: *mut gss_name_t,
            _: *mut gss_OID,
            _: *mut gss_buffer_desc,
            _: *mut OM_uint32,
            _: *mut OM_uint32,
            _: *mut gss_cred_id_t,
        ) -> OM_uint32 {
            0
        }
        unsafe extern "C" fn display_name(
            _: *mut OM_uint32,
            _: gss_name_t,
            _: *mut gss_buffer_desc,
            _: *mut gss_OID,
        ) -> OM_uint32 {
            0
        }
        unsafe extern "C" fn display_status(
            _: *mut OM_uint32,
            _: OM_uint32,
            _: c_int,
            _: gss_OID,
            _: *mut OM_uint32,
            _: *mut gss_buffer_desc,
        ) -> OM_uint32 {
            0
        }
        unsafe extern "C" fn release_buffer(_: *mut OM_uint32, b: *mut gss_buffer_desc) -> OM_uint32 {
            RELEASED.fetch_add(1, Ordering::SeqCst);
            // SAFETY: the test passes its own live buffer descriptor.
            unsafe {
                (*b).length = 0;
                (*b).value = core::ptr::null_mut();
            }
            0
        }
        unsafe extern "C" fn release_ptr(_: *mut OM_uint32, _: *mut *mut c_void) -> OM_uint32 {
            0
        }
        unsafe extern "C" fn delete_ctx(
            _: *mut OM_uint32,
            _: *mut gss_ctx_id_t,
            _: *mut gss_buffer_desc,
        ) -> OM_uint32 {
            0
        }
        let api: &'static GssApi = Box::leak(Box::new(GssApi {
            gss_accept_sec_context: accept,
            gss_display_name: display_name,
            gss_display_status: display_status,
            gss_release_buffer: release_buffer,
            gss_release_cred: release_ptr,
            gss_release_name: release_ptr,
            gss_delete_sec_context: delete_ctx,
            gss_wrap: None,
            gss_unwrap: None,
            gss_wrap_size_limit: None,
            gss_acquire_cred_from: None,
            gss_store_cred_into: None,
        }));
        let mut port = crate::tests::unix_port("alice", "db");
        let mut token = *b"token";
        let state = super::GssState {
            api,
            ctx: core::ptr::null_mut(),
            name: core::ptr::null_mut(),
            acceptor_cred: core::ptr::null_mut(),
            outbuf: gss_buffer_desc { length: token.len(), value: token.as_mut_ptr().cast() },
            port: &mut port,
        };
        drop(state);
        assert_eq!(RELEASED.load(Ordering::SeqCst), 1);

        let state = super::GssState {
            api,
            ctx: core::ptr::null_mut(),
            name: core::ptr::null_mut(),
            acceptor_cred: core::ptr::null_mut(),
            outbuf: gss_buffer_desc::empty(),
            port: &mut port,
        };
        drop(state);
        assert_eq!(RELEASED.load(Ordering::SeqCst), 1, "an empty buffer is not released");
    }

    // The keytab/ccache env writes on the GSS auth path must be serialized
    // (GSS_ENV_LOCK) so two backend threads never race libc::setenv against
    // each other (setenv may realloc environ and free the old array). Hammer
    // gss_setenv from many threads: writer-vs-writer serialization must hold —
    // no corruption, and the surviving value is always one that was written.
    // (The primary keytab path no longer writes the environment at all; it goes
    // through the credential store. This lock only guards the Heimdal fallback
    // and the delegated-ccache write.)
    #[test]
    fn gss_setenv_is_serialized() {
        let key = c"PGRUST_TEST_GSS_ENV";
        let vals = [c"MEMORY:a", c"MEMORY:b", c"MEMORY:c"];
        let handles: Vec<_> = (0..8)
            .map(|t| {
                std::thread::spawn(move || {
                    for _ in 0..2000 {
                        assert_eq!(gss_setenv(key, vals[t % vals.len()]), 0);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let final_val = std::env::var("PGRUST_TEST_GSS_ENV").unwrap();
        assert!(["MEMORY:a", "MEMORY:b", "MEMORY:c"].contains(&final_val.as_str()));
    }

    fn ok<'a>(r: Result<&'a str, super::RealmMismatch<'a>>) -> Option<&'a str> {
        r.ok()
    }

    #[test]
    fn principal_realm_mapping() {
        assert_eq!(ok(map_principal("alice@EXAMPLE.COM", true, None, false)), Some("alice@EXAMPLE.COM"));
        assert_eq!(ok(map_principal("alice@EXAMPLE.COM", false, None, false)), Some("alice"));
        assert_eq!(
            ok(map_principal("alice@EXAMPLE.COM", false, Some("EXAMPLE.COM"), false)),
            Some("alice")
        );
        assert_eq!(ok(map_principal("alice@EXAMPLE.COM", false, Some("OTHER.COM"), false)), None);
        assert_eq!(
            ok(map_principal("alice@example.com", false, Some("EXAMPLE.COM"), true)),
            Some("alice")
        );
        assert_eq!(ok(map_principal("alice@example.com", false, Some("EXAMPLE.COM"), false)), None);
        // No realm returned but realm matching requested.
        assert_eq!(ok(map_principal("alice", false, Some("EXAMPLE.COM"), false)), None);
        // Empty krb_realm behaves as unset (C strlen check).
        assert_eq!(ok(map_principal("alice", false, Some(""), false)), Some("alice"));
        assert_eq!(ok(map_principal("alice", true, None, false)), Some("alice"));
        // include_realm with realm match keeps the full principal.
        assert_eq!(
            ok(map_principal("alice@EXAMPLE.COM", true, Some("EXAMPLE.COM"), false)),
            Some("alice@EXAMPLE.COM")
        );
    }
}
