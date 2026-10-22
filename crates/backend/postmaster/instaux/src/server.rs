use std::ffi::CString;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Duration;

use types_error::{PgError, PgResult, ERRCODE_CONFIG_FILE_ERROR, FATAL};

use crate::shared::{now_ms, SHARED};

const MAX_CONNECTIONS: usize = 32;
const IO_TIMEOUT: Duration = Duration::from_secs(10);

static INFLIGHT: AtomicUsize = AtomicUsize::new(0);

fn start_error(msg: String) -> Box<PgError> {
    Box::new(PgError::new(FATAL, msg).with_sqlstate(ERRCODE_CONFIG_FILE_ERROR))
}

/// PostmasterMain, after the configuration is loaded and before shared
/// memory is sized: sizes the aux slot, binds the listeners, and starts the
/// listener thread, which outlives crash restarts.
pub fn postmaster_start() -> PgResult<()> {
    use guc_tables::backing as b;
    if !b::pgrust_instance_aux() {
        return Ok(());
    }
    if let Some(Err(msg)) = instaux_seams::startup_check::call_if_or(None, |f| Some(f())) {
        return Err(start_error(format!("pgrust.instance_aux is refused: {msg}")));
    }
    let listen = b::pgrust_instance_aux_listen().unwrap_or_default();
    let socket = b::pgrust_instance_aux_socket().unwrap_or_default();
    if listen.is_empty() && socket.is_empty() {
        return Err(start_error(
            "pgrust.instance_aux requires pgrust.instance_aux_listen or pgrust.instance_aux_socket".into(),
        ));
    }
    let acceptor = crate::tls::build_acceptor(&crate::tls::files_from_gucs())
        .map_err(|m| start_error(format!("instance aux TLS: {m}")))?;
    crate::shared::install_acceptor(acceptor);
    crate::state::adopt(&crate::state::load());
    init_small::instaux::set_instance_aux_enabled(true);
    let tcp = match listen.as_str() {
        "" => None,
        addr => Some(
            TcpListener::bind(addr)
                .map_err(|e| start_error(format!("could not bind instance aux address \"{addr}\": {e}")))?,
        ),
    };
    let unix = match socket.as_str() {
        "" => None,
        path => {
            if let Ok(c) = CString::new(path) {
                vfs::unlink(&c);
            }
            Some(
                UnixListener::bind(path)
                    .map_err(|e| start_error(format!("could not bind instance aux socket \"{path}\": {e}")))?,
            )
        }
    };
    SHARED.started_ms.store(now_ms(), Relaxed);
    // DST: the control listener is a postmaster-lifetime thread outside the spawn door.
    std::thread::Builder::new()
        .name("pg:instaux".into())
        .stack_size(256 * 1024)
        .spawn(pagegive::pinned(move || accept_loop(tcp, unix)))
        .map_err(|e| start_error(format!("could not start the instance aux listener: {e}")))?;
    Ok(())
}

fn accept_loop(tcp: Option<TcpListener>, unix: Option<UnixListener>) {
    if let Some(l) = &tcp {
        let _ = l.set_nonblocking(true);
    }
    if let Some(l) = &unix {
        let _ = l.set_nonblocking(true);
    }
    let mut fds = Vec::new();
    for fd in [tcp.as_ref().map(|l| l.as_raw_fd()), unix.as_ref().map(|l| l.as_raw_fd())].into_iter().flatten() {
        fds.push(libc::pollfd { fd, events: libc::POLLIN, revents: 0 });
    }
    loop {
        // SAFETY: `fds` is a live, correctly sized pollfd array.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 500) };
        if n <= 0 {
            continue;
        }
        let mut stalled = false;
        if let Some(l) = &tcp {
            loop {
                match l.accept() {
                    Ok((s, _)) => {
                        let _ = s.set_nonblocking(false);
                        let _ = s.set_read_timeout(Some(IO_TIMEOUT));
                        let _ = s.set_write_timeout(Some(IO_TIMEOUT));
                        spawn_conn(s);
                    }
                    Err(e) => {
                        stalled |= e.kind() != std::io::ErrorKind::WouldBlock;
                        break;
                    }
                }
            }
        }
        if let Some(l) = &unix {
            loop {
                match l.accept() {
                    Ok((s, _)) => {
                        let _ = s.set_nonblocking(false);
                        let _ = s.set_read_timeout(Some(IO_TIMEOUT));
                        let _ = s.set_write_timeout(Some(IO_TIMEOUT));
                        spawn_conn(s);
                    }
                    Err(e) => {
                        stalled |= e.kind() != std::io::ErrorKind::WouldBlock;
                        break;
                    }
                }
            }
        }
        if stalled {
            // SAFETY: poll(2) with no descriptors only waits.
            unsafe { libc::poll(std::ptr::null_mut(), 0, 100) };
        }
    }
}

fn spawn_conn<S: Read + Write + Send + std::fmt::Debug + 'static>(stream: S) {
    if INFLIGHT.fetch_add(1, Relaxed) >= MAX_CONNECTIONS {
        INFLIGHT.fetch_sub(1, Relaxed);
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("pg:instaux-conn".into())
        .stack_size(512 * 1024)
        .spawn(pagegive::pinned(move || {
            serve(stream);
            INFLIGHT.fetch_sub(1, Relaxed);
        }));
    if spawned.is_err() {
        INFLIGHT.fetch_sub(1, Relaxed);
    }
}

fn serve<S: Read + Write + std::fmt::Debug>(stream: S) {
    let Some(acceptor) = crate::shared::acceptor() else { return };
    let Ok(mut tls) = acceptor.accept(stream) else { return };
    let identities = crate::tls::peer_identities(tls.ssl());
    let response = match crate::http::read_request(&mut tls) {
        Ok(req) => {
            // unwind-ok: worker-containment
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| crate::api::handle(&req, &identities))) {
                Ok(r) => Some(r),
                Err(_) => Some(crate::http::Response::error(500, "internal", "the control request failed")),
            }
        }
        Err(e) => crate::http::error_response(&e),
    };
    if let Some(r) = response {
        let _ = crate::http::write_response(&mut tls, &r);
    }
    let _ = tls.shutdown();
}
