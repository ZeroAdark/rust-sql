use core::cell::Cell;

use elog::ereport;
use types_error::{ErrorLocation, PgResult, ERROR};
use types_startup::{ClientSocket, Port};

const STDIN_FD: i32 = 0;
const STDOUT_FD: i32 = 1;

thread_local! {
    static STATE: Cell<Option<bool>> = const { Cell::new(None) };
}

#[track_caller]
fn loc(funcname: &'static str) -> ErrorLocation {
    let site = core::panic::Location::caller();
    ErrorLocation::new(site.file(), site.line() as i32, funcname)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn errno() -> i32 {
    // SAFETY: __error returns this thread's valid errno location.
    unsafe { *libc::__error() }
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "ios"))))]
fn errno() -> i32 {
    // SAFETY: __errno_location returns this thread's valid errno location.
    unsafe { *libc::__errno_location() }
}

#[cfg(target_family = "wasm")]
fn errno() -> i32 {
    // SAFETY: same contract as the unix arms.
    unsafe { *libc::__errno_location() }
}

/// `Ok(0)` is EOF, `Err(errno)` is the C `-1`.
fn ssize_result(n: isize, e: i32) -> Result<usize, i32> {
    if n >= 0 {
        Ok(n as usize)
    } else {
        Err(e)
    }
}

fn noblock() -> bool {
    STATE.get().unwrap_or(false)
}

pub fn secure_read(buf: &mut [u8]) -> PgResult<Result<usize, i32>> {
    postgres_seams::process_client_read_interrupt::call(false)?;

    let (n, e) = loop {
        let (n, e) = if noblock() {
            fdnb::read_noblock(STDIN_FD, buf)
        } else {
            // SAFETY: buf is valid writable memory of buf.len() bytes.
            let n = unsafe { libc::read(STDIN_FD, buf.as_mut_ptr().cast(), buf.len()) };
            (n as isize, errno())
        };
        if n < 0 && e == libc::EINTR {
            postgres_seams::process_client_read_interrupt::call(true)?;
            continue;
        }
        break (n, e);
    };

    postgres_seams::process_client_read_interrupt::call(false)?;

    Ok(ssize_result(n, e))
}

/// The write is capped at fdnb::NB_WRITE_CAP so POLLOUT actually licenses it.
pub fn secure_write(buf: &[u8]) -> PgResult<Result<usize, i32>> {
    postgres_seams::process_client_write_interrupt::call(false)?;

    let (n, e) = loop {
        let (n, e) = if noblock() {
            fdnb::write_noblock(STDOUT_FD, buf)
        } else {
            // SAFETY: buf is valid readable memory of buf.len() bytes.
            let n = unsafe { libc::write(STDOUT_FD, buf.as_ptr().cast(), buf.len()) };
            (n as isize, errno())
        };
        if n < 0 && e == libc::EINTR {
            postgres_seams::process_client_write_interrupt::call(true)?;
            continue;
        }
        break (n, e);
    };

    postgres_seams::process_client_write_interrupt::call(false)?;

    Ok(ssize_result(n, e))
}

fn set_port_noblock(noblock: bool) -> bool {
    if STATE.get().is_none() {
        return false;
    }
    STATE.set(Some(noblock));
    true
}

fn secure_close() {}

fn pq_init(client_sock: &ClientSocket) -> PgResult<Port> {
    let port = Port::new(client_sock);
    pqcomm::pq_init_buffers()?;
    STATE.set(Some(false));
    Ok(port)
}

// No FeBeWaitSet on this transport, so there is nothing to repoint.
fn modify_fe_be_wait_set_latch(_latch: types_storage::latch::LatchHandle) -> PgResult<()> {
    Ok(())
}

pub fn init_transport_seams() {
    be_secure_seams::secure_read::set(secure_read);
    be_secure_seams::secure_write::set(secure_write);
    be_secure_seams::secure_close::set(secure_close);
    be_secure_seams::set_port_noblock::set(set_port_noblock);
    be_secure_seams::be_tls_get_certificate_hash::set(|| {
        // Reachable only from SCRAM channel binding with ssl_in_use, which
        // never becomes true on this transport.
        ereport(ERROR)
            .errmsg_internal("channel binding is not supported on the stdio transport")
            .finish(loc("be_tls_get_certificate_hash"))
            .map(|()| Vec::new())
    });
    pqcomm_seams::pq_init::set(pq_init);
    pqcomm_seams::modify_fe_be_wait_set_latch::set(modify_fe_be_wait_set_latch);
}
