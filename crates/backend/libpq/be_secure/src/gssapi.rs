// be-secure-gssapi.c: GSSAPI-encrypted transport. Packets are a 4-byte
// network-order length followed by a gss_wrap'd payload.

use core::cell::{Cell, RefCell};
use core::ffi::c_void;
use std::ffi::CString;

use be_gssapi_common::gss_ffi::{
    self, gss_buffer_desc, gss_cred_id_t, gss_ctx_id_t, gss_error, gss_name_t, GssApi,
    GSS_C_QOP_DEFAULT, GSS_S_COMPLETE, GSS_S_CONTINUE_NEEDED,
};
use be_gssapi_common::{acquire_keytab_cred, gss_setenv, pg_GSS_error, pg_store_delegated_credential};
use elog::{ereport, PCT_M};
use types_error::{PgResult, COMMERROR, FATAL};
use types_storage::waiteventset::{WL_LATCH_SET, WL_SOCKET_READABLE, WL_SOCKET_WRITEABLE};

use crate::{errno, loc, secure_raw_read, secure_raw_write, would_block};

const PQ_GSS_MAX_PACKET_SIZE: usize = 16384;
const PQ_GSS_AUTH_BUFFER_SIZE: usize = 65536;
const HDR: usize = 4;

// WAIT_EVENT_GSS_OPEN_SERVER: PG_WAIT_CLIENT | 2.
const WAIT_EVENT_GSS_OPEN_SERVER: u32 = 0x0600_0000 | 2;

struct Buffers {
    send: Vec<u8>,
    send_len: usize,
    send_next: usize,
    send_consumed: usize,
    recv: Vec<u8>,
    recv_len: usize,
    result: Vec<u8>,
    result_len: usize,
    result_next: usize,
}

impl Buffers {
    const fn new() -> Buffers {
        Buffers {
            send: Vec::new(),
            send_len: 0,
            send_next: 0,
            send_consumed: 0,
            recv: Vec::new(),
            recv_len: 0,
            result: Vec::new(),
            result_len: 0,
            result_next: 0,
        }
    }

    fn reset(&mut self, size: usize) -> PgResult<()> {
        for b in [&mut self.send, &mut self.recv, &mut self.result] {
            *b = ::mcx::std_vec_from_elem("libpq", 0u8, size)?;
        }
        self.send_len = 0;
        self.send_next = 0;
        self.send_consumed = 0;
        self.recv_len = 0;
        self.result_len = 0;
        self.result_next = 0;
        Ok(())
    }
}

// Per-connection (backend thread) transport state: C's port->gss context and
// the file-static PqGSS* buffers.
struct GssTransport {
    enc: Cell<bool>,
    ctx: Cell<gss_ctx_id_t>,
    name: Cell<gss_name_t>,
    delegated: Cell<bool>,
    max_pkt: Cell<u32>,
    bufs: RefCell<Buffers>,
}

thread_local! {
    static GSS: GssTransport = const {
        GssTransport {
            enc: Cell::new(false),
            ctx: Cell::new(core::ptr::null_mut()),
            name: Cell::new(core::ptr::null_mut()),
            delegated: Cell::new(false),
            max_pkt: Cell::new(0),
            bufs: RefCell::new(Buffers::new()),
        }
    };
}

/// be_gssapi_get_enc: this connection runs GSSAPI-encrypted.
pub fn be_gssapi_get_enc() -> bool {
    GSS.with(|g| g.enc.get())
}

/// be_gssapi_get_delegation.
pub fn be_gssapi_get_delegation() -> bool {
    GSS.with(|g| g.delegated.get())
}

/// The client name of the established context (port->gss->name), for
/// pg_GSS_checkauth on an encrypted connection.
pub fn be_gssapi_context_name() -> gss_name_t {
    GSS.with(|g| g.name.get())
}

/// Whether this build can offer GSSAPI encryption (C: ENABLE_GSS).
pub fn gssenc_available() -> bool {
    gss_ffi::try_gss().is_ok_and(|api| {
        api.gss_wrap.is_some() && api.gss_unwrap.is_some() && api.gss_wrap_size_limit.is_some()
    })
}

/// Releases the context at backend exit (C: process death).
pub fn be_gssapi_close() {
    let (ctx, name) = GSS.with(|g| {
        g.enc.set(false);
        (g.ctx.replace(core::ptr::null_mut()), g.name.replace(core::ptr::null_mut()))
    });
    let Ok(api) = gss_ffi::try_gss() else { return };
    let mut minor: u32 = 0;
    if !ctx.is_null() {
        let mut ctx = ctx;
        // SAFETY: the context gss_accept_sec_context established, released once.
        unsafe { (api.gss_delete_sec_context)(&mut minor, &mut ctx, core::ptr::null_mut()) };
    }
    if !name.is_null() {
        let mut name = name;
        // SAFETY: the client name from gss_accept_sec_context, released once.
        unsafe { (api.gss_release_name)(&mut minor, &mut name) };
    }
}

fn close_at_exit(_code: i32, _arg: usize) {
    be_gssapi_close();
}

fn wrap_api(api: &GssApi) -> (WrapFn, UnwrapFn) {
    (
        api.gss_wrap.expect("gssenc_available checked gss_wrap"),
        api.gss_unwrap.expect("gssenc_available checked gss_unwrap"),
    )
}

type WrapFn = unsafe extern "C" fn(
    *mut u32,
    gss_ctx_id_t,
    core::ffi::c_int,
    u32,
    *mut gss_buffer_desc,
    *mut core::ffi::c_int,
    *mut gss_buffer_desc,
) -> u32;
type UnwrapFn = unsafe extern "C" fn(
    *mut u32,
    gss_ctx_id_t,
    *mut gss_buffer_desc,
    *mut gss_buffer_desc,
    *mut core::ffi::c_int,
    *mut u32,
) -> u32;

/// be_gssapi_write: (n, errno); n = -1 on failure.
pub(crate) fn be_gssapi_write(sock: i32, ptr: &[u8]) -> PgResult<(isize, i32)> {
    let api = gss_ffi::try_gss().expect("encrypted connection without a GSSAPI library");
    let (wrap, _) = wrap_api(api);
    let gctx = GSS.with(|g| g.ctx.get());
    let max_pkt = GSS.with(|g| g.max_pkt.get()) as usize;
    GSS.with(|g| -> PgResult<(isize, i32)> {
        let mut b = g.bufs.borrow_mut();
        let len = ptr.len();
        if len < b.send_consumed {
            ereport(COMMERROR)
                .errmsg_internal("GSSAPI caller failed to retransmit all data needing to be retried")
                .finish(loc("be_gssapi_write"))?;
            return Ok((-1, libc::ECONNRESET));
        }
        let mut bytes_to_encrypt = len - b.send_consumed;
        let mut bytes_encrypted = b.send_consumed;
        while bytes_to_encrypt > 0 || b.send_len > 0 {
            if b.send_len > 0 {
                let amount = b.send_len - b.send_next;
                let ret = secure_raw_write(sock, &b.send[b.send_next..b.send_len]);
                if ret <= 0 {
                    return Ok((ret, errno()));
                }
                if (ret as usize) < amount {
                    b.send_next += ret as usize;
                    continue;
                }
                b.send_len = 0;
                b.send_next = 0;
            }
            if bytes_to_encrypt == 0 {
                break;
            }
            let chunk = bytes_to_encrypt.min(max_pkt);
            let mut input = gss_buffer_desc {
                length: chunk,
                value: ptr[bytes_encrypted..].as_ptr() as *mut c_void,
            };
            let mut output = gss_buffer_desc::empty();
            let mut conf_state: core::ffi::c_int = 0;
            let mut minor: u32 = 0;
            // SAFETY: gctx is the established context; input borrows ptr.
            let major = unsafe {
                wrap(&mut minor, gctx, 1, GSS_C_QOP_DEFAULT, &mut input, &mut conf_state, &mut output)
            };
            if major != GSS_S_COMPLETE {
                pg_GSS_error("GSSAPI wrap error", major, minor)?;
                return Ok((-1, libc::ECONNRESET));
            }
            let out = take_buffer(api, &mut output)?;
            if conf_state == 0 {
                ereport(COMMERROR)
                    .errmsg("outgoing GSSAPI message would not use confidentiality")
                    .finish(loc("be_gssapi_write"))?;
                return Ok((-1, libc::ECONNRESET));
            }
            if out.len() > PQ_GSS_MAX_PACKET_SIZE - HDR {
                ereport(COMMERROR)
                    .errmsg(format!(
                        "server tried to send oversize GSSAPI packet ({} > {})",
                        out.len(),
                        PQ_GSS_MAX_PACKET_SIZE - HDR
                    ))
                    .finish(loc("be_gssapi_write"))?;
                return Ok((-1, libc::ECONNRESET));
            }
            bytes_encrypted += chunk;
            bytes_to_encrypt -= chunk;
            b.send_consumed += chunk;
            let at = b.send_len;
            b.send[at..at + HDR].copy_from_slice(&(out.len() as u32).to_be_bytes());
            b.send[at + HDR..at + HDR + out.len()].copy_from_slice(&out);
            b.send_len += HDR + out.len();
        }
        debug_assert_eq!(len, b.send_consumed);
        b.send_consumed = 0;
        Ok((bytes_encrypted as isize, 0))
    })
}

// Copies a library-allocated output token and releases it.
fn take_buffer(api: &GssApi, buf: &mut gss_buffer_desc) -> PgResult<Vec<u8>> {
    let out = if buf.length == 0 || buf.value.is_null() {
        Ok(Vec::new())
    } else {
        // SAFETY: the library returned length valid bytes at value.
        ::mcx::std_vec_from_slice("libpq", unsafe {
            core::slice::from_raw_parts(buf.value as *const u8, buf.length)
        })
    };
    let mut minor: u32 = 0;
    // SAFETY: buf came from the GSSAPI library.
    unsafe { (api.gss_release_buffer)(&mut minor, buf) };
    out
}

/// be_gssapi_read: (n, errno); n = -1 on failure or would-block.
pub(crate) fn be_gssapi_read(sock: i32, ptr: &mut [u8]) -> PgResult<(isize, i32)> {
    let api = gss_ffi::try_gss().expect("encrypted connection without a GSSAPI library");
    let (_, unwrap) = wrap_api(api);
    let gctx = GSS.with(|g| g.ctx.get());
    GSS.with(|g| -> PgResult<(isize, i32)> {
        let mut b = g.bufs.borrow_mut();
        let len = ptr.len();
        let mut bytes_returned = 0usize;
        while bytes_returned < len {
            if b.result_next < b.result_len {
                let n = (b.result_len - b.result_next).min(len - bytes_returned);
                let from = b.result_next;
                ptr[bytes_returned..bytes_returned + n].copy_from_slice(&b.result[from..from + n]);
                b.result_next += n;
                bytes_returned += n;
                break;
            }
            b.result_len = 0;
            b.result_next = 0;

            if b.recv_len < HDR {
                let at = b.recv_len;
                let ret = secure_raw_read(sock, &mut b.recv[at..HDR]);
                if ret <= 0 {
                    return Ok((ret, errno()));
                }
                b.recv_len += ret as usize;
                if b.recv_len < HDR {
                    return Ok((-1, libc::EWOULDBLOCK));
                }
            }
            let plen = u32::from_be_bytes(b.recv[..HDR].try_into().unwrap()) as usize;
            if plen > PQ_GSS_MAX_PACKET_SIZE - HDR {
                ereport(COMMERROR)
                    .errmsg(format!(
                        "oversize GSSAPI packet sent by the client ({} > {})",
                        plen,
                        PQ_GSS_MAX_PACKET_SIZE - HDR
                    ))
                    .finish(loc("be_gssapi_read"))?;
                return Ok((-1, libc::ECONNRESET));
            }
            let at = b.recv_len;
            let ret = secure_raw_read(sock, &mut b.recv[at..HDR + plen]);
            if ret <= 0 {
                return Ok((ret, errno()));
            }
            b.recv_len += ret as usize;
            if b.recv_len - HDR < plen {
                return Ok((-1, libc::EWOULDBLOCK));
            }
            let mut input =
                gss_buffer_desc { length: plen, value: b.recv[HDR..].as_ptr() as *mut c_void };
            let mut output = gss_buffer_desc::empty();
            let mut conf_state: core::ffi::c_int = 0;
            let mut minor: u32 = 0;
            // SAFETY: gctx is the established context; input borrows recv.
            let major = unsafe {
                unwrap(&mut minor, gctx, &mut input, &mut output, &mut conf_state, core::ptr::null_mut())
            };
            if major != GSS_S_COMPLETE {
                pg_GSS_error("GSSAPI unwrap error", major, minor)?;
                return Ok((-1, libc::ECONNRESET));
            }
            let out = take_buffer(api, &mut output)?;
            if conf_state == 0 {
                ereport(COMMERROR)
                    .errmsg("incoming GSSAPI message did not use confidentiality")
                    .finish(loc("be_gssapi_read"))?;
                return Ok((-1, libc::ECONNRESET));
            }
            b.result[..out.len()].copy_from_slice(&out);
            b.result_len = out.len();
            b.recv_len = 0;
        }
        Ok((bytes_returned as isize, 0))
    })
}

fn wait_socket(waitfor: u32) -> PgResult<()> {
    pqcomm::pq_modify_fe_be_wait_set_socket(waitfor)?;
    let events = pqcomm::pq_wait_event_set_wait_fe_be(-1, WAIT_EVENT_GSS_OPEN_SERVER)?;
    // The latch is in FeBeWaitSet (C's WaitLatchOrSocket has none): consume
    // it and drain interrupts so authentication_timeout still applies.
    if events & WL_LATCH_SET != 0 {
        latch_seams::reset_latch_my_latch::call();
        if waitfor == WL_SOCKET_READABLE {
            postgres_seams::process_client_read_interrupt::call(true)?;
        } else {
            postgres_seams::process_client_write_interrupt::call(true)?;
        }
    }
    Ok(())
}

// read_or_wait: fill recv[..len]; false = permanent error or EOF.
fn read_or_wait(sock: i32, len: usize) -> PgResult<bool> {
    loop {
        let (have, ret, e) = GSS.with(|g| {
            let mut b = g.bufs.borrow_mut();
            let at = b.recv_len;
            if at >= len {
                return (true, 0, 0);
            }
            let ret = secure_raw_read(sock, &mut b.recv[at..len]);
            let e = errno();
            if ret > 0 {
                b.recv_len += ret as usize;
            }
            (false, ret, e)
        });
        if have {
            return Ok(true);
        }
        if ret < 0 && !(would_block(e) || e == libc::EINTR) {
            return Ok(false);
        }
        if ret <= 0 {
            wait_socket(WL_SOCKET_READABLE)?;
            if ret == 0 {
                let again = GSS.with(|g| {
                    let mut b = g.bufs.borrow_mut();
                    let at = b.recv_len;
                    let r = secure_raw_read(sock, &mut b.recv[at..len]);
                    if r > 0 {
                        b.recv_len += r as usize;
                    }
                    r
                });
                if again == 0 {
                    return Ok(false);
                }
            }
        }
    }
}

fn write_all_or_wait(sock: i32) -> PgResult<bool> {
    loop {
        let (done, ret, e) = GSS.with(|g| {
            let mut b = g.bufs.borrow_mut();
            if b.send_next >= b.send_len {
                return (true, 0, 0);
            }
            let ret = secure_raw_write(sock, &b.send[b.send_next..b.send_len]);
            let e = errno();
            if ret > 0 {
                b.send_next += ret as usize;
            }
            (false, ret, e)
        });
        if done {
            GSS.with(|g| {
                let mut b = g.bufs.borrow_mut();
                b.send_len = 0;
                b.send_next = 0;
            });
            return Ok(true);
        }
        if ret < 0 && !(would_block(e) || e == libc::EINTR) {
            return Ok(false);
        }
        if ret <= 0 {
            wait_socket(WL_SOCKET_WRITEABLE)?;
        }
    }
}

/// secure_open_gssapi: accept the client's context, then switch the
/// connection to encrypted transport. Ok(-1) = failure (already logged).
pub fn secure_open_gssapi(sock: i32) -> PgResult<i32> {
    injection_point::injection_point("backend-gssapi-startup")?;
    let api = gss_ffi::try_gss().expect("GSSENCRequest accepted without a GSSAPI library");
    GSS.with(|g| g.bufs.borrow_mut().reset(PQ_GSS_AUTH_BUFFER_SIZE))?;
    GSS.with(|g| g.delegated.set(false));

    // The configured keytab binds through the thread-safe credential store
    // where the library has it; the process environment is shared by every
    // backend thread (see be_gssapi_common::gss_setenv).
    let keyfile = guc_tables::vars::pg_krb_server_keyfile.read().unwrap_or_default();
    let mut cred: gss_cred_id_t = core::ptr::null_mut();
    if !keyfile.is_empty() {
        let kt = CString::new(keyfile.as_str()).unwrap_or_default();
        if api.gss_acquire_cred_from.is_some() {
            match acquire_keytab_cred(api, &kt) {
                Ok(c) => cred = c,
                Err((major, minor)) => {
                    pg_GSS_error("could not accept GSSAPI security context", major, minor)?;
                    return Ok(-1);
                }
            }
        } else if gss_setenv(c"KRB5_KTNAME", kt.as_c_str()) != 0 {
            let errnum = elog::errno::current_errno();
            ereport(FATAL)
                .with_saved_errno(errnum)
                .errcode(types_error::ERRCODE_OUT_OF_MEMORY)
                .errmsg(format!("could not set environment: {PCT_M}"))
                .finish(loc("secure_open_gssapi"))?;
        }
    }
    let r = open_loop(api, sock, cred);
    if !cred.is_null() {
        let mut minor: u32 = 0;
        // SAFETY: the acceptor credential acquired above, released once.
        unsafe { (api.gss_release_cred)(&mut minor, &mut cred) };
    }
    if !matches!(r, Ok(0)) {
        be_gssapi_close();
    }
    r
}

fn open_loop(api: &'static GssApi, sock: i32, cred: gss_cred_id_t) -> PgResult<i32> {
    let accept_delegation = guc_tables::vars::pg_gss_accept_delegation.read();
    loop {
        if !read_or_wait(sock, HDR)? {
            return Ok(-1);
        }
        let plen = GSS.with(|g| {
            let mut b = g.bufs.borrow_mut();
            b.recv_len = 0;
            u32::from_be_bytes(b.recv[..HDR].try_into().unwrap()) as usize
        });
        if plen > PQ_GSS_AUTH_BUFFER_SIZE - HDR {
            ereport(COMMERROR)
                .errmsg(format!(
                    "oversize GSSAPI packet sent by the client ({} > {})",
                    plen,
                    PQ_GSS_AUTH_BUFFER_SIZE - HDR
                ))
                .finish(loc("secure_open_gssapi"))?;
            return Ok(-1);
        }
        if !read_or_wait(sock, plen)? {
            return Ok(-1);
        }

        let mut ctx = GSS.with(|g| g.ctx.get());
        let mut name = GSS.with(|g| g.name.get());
        let mut delegated_creds: gss_cred_id_t = core::ptr::null_mut();
        let mut output = gss_buffer_desc::empty();
        let mut minor: u32 = 0;
        let major = GSS.with(|g| {
            let b = g.bufs.borrow();
            let mut input = gss_buffer_desc { length: plen, value: b.recv.as_ptr() as *mut c_void };
            // SAFETY: input borrows the receive buffer for the call; the
            // context, name and output token are owned by this connection.
            unsafe {
                (api.gss_accept_sec_context)(
                    &mut minor,
                    &mut ctx,
                    cred,
                    &mut input,
                    core::ptr::null_mut(),
                    &mut name,
                    core::ptr::null_mut(),
                    &mut output,
                    core::ptr::null_mut(),
                    core::ptr::null_mut(),
                    if accept_delegation { &mut delegated_creds } else { core::ptr::null_mut() },
                )
            }
        });
        GSS.with(|g| {
            g.ctx.set(ctx);
            g.name.set(name);
        });
        let out = take_buffer(api, &mut output)?;
        if gss_error(major) {
            pg_GSS_error("could not accept GSSAPI security context", major, minor)?;
            return Ok(-1);
        }
        let complete_next = major & GSS_S_CONTINUE_NEEDED == 0;

        if !delegated_creds.is_null() {
            pg_store_delegated_credential(api, delegated_creds)?;
            GSS.with(|g| g.delegated.set(true));
        }
        GSS.with(|g| g.bufs.borrow_mut().recv_len = 0);

        if !out.is_empty() {
            if out.len() > PQ_GSS_AUTH_BUFFER_SIZE - HDR {
                ereport(COMMERROR)
                    .errmsg(format!(
                        "server tried to send oversize GSSAPI packet ({} > {})",
                        out.len(),
                        PQ_GSS_AUTH_BUFFER_SIZE - HDR
                    ))
                    .finish(loc("secure_open_gssapi"))?;
                return Ok(-1);
            }
            GSS.with(|g| {
                let mut b = g.bufs.borrow_mut();
                b.send[..HDR].copy_from_slice(&(out.len() as u32).to_be_bytes());
                b.send[HDR..HDR + out.len()].copy_from_slice(&out);
                b.send_len = HDR + out.len();
                b.send_next = 0;
            });
            if !write_all_or_wait(sock)? {
                return Ok(-1);
            }
        }
        if complete_next {
            break;
        }
    }

    GSS.with(|g| g.bufs.borrow_mut().reset(PQ_GSS_MAX_PACKET_SIZE))?;
    let wrap_size_limit = api.gss_wrap_size_limit.expect("gssenc_available checked gss_wrap_size_limit");
    let mut max_pkt: u32 = 0;
    let mut minor: u32 = 0;
    let ctx = GSS.with(|g| g.ctx.get());
    // SAFETY: ctx is the established context.
    let major = unsafe {
        wrap_size_limit(
            &mut minor,
            ctx,
            1,
            GSS_C_QOP_DEFAULT,
            (PQ_GSS_MAX_PACKET_SIZE - HDR) as u32,
            &mut max_pkt,
        )
    };
    if gss_error(major) {
        pg_GSS_error("GSSAPI size check error", major, minor)?;
        return Ok(-1);
    }
    GSS.with(|g| {
        g.max_pkt.set(max_pkt);
        g.enc.set(true);
    });
    ipc_seams::on_proc_exit::call(close_at_exit, 0);
    Ok(0)
}
