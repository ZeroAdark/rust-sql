// pqcomm.c socket half: pq_init wiring, socket_close, listen/accept,
// TCP keepalive knobs.

#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]

use std::cell::{Cell, RefCell};
use std::ffi::CString;

use elog::{ereport, PCT_M};
use ip::{sockaddr_family, AddrInfoHint, PgAddrInfo};
use types_core::{pgsocket, PGINVALID_SOCKET, STATUS_ERROR, STATUS_OK};
use types_error::{ErrorLocation, PgResult, FATAL, LOG};
use types_startup::{ClientSocket, Port};
use types_storage::latch::LatchHandle;
use types_storage::waiteventset::{
    WaitEventSetHandle, WL_LATCH_SET, WL_SOCKET_CLOSED, WL_SOCKET_WRITEABLE,
};

use init_small::globals as g;

pub const FeBeWaitSetSocketPos: i32 = 0;
pub const FeBeWaitSetLatchPos: i32 = 1;
pub const FeBeWaitSetNEvents: i32 = 3;

#[track_caller]
fn loc(funcname: &'static str) -> ErrorLocation {
    // pgrust is Rust: report where in OUR source this was raised.
    // #[track_caller] resolves to the call site, not this helper.
    let site = core::panic::Location::caller();
    ErrorLocation::new(site.file(), site.line() as i32, funcname)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn errno() -> i32 {
    unsafe { *libc::__error() }
}

#[cfg(not(any(target_os = "macos", target_os = "ios")))]
fn errno() -> i32 {
    unsafe { *libc::__errno_location() }
}

thread_local! {
    // WaitEventSet *FeBeWaitSet; never explicitly freed, as C (lives for the
    // backend). C reclaims it at process death; the thread model reclaims at
    // session-thread exit via WaitEventSetReleaseGuard (chaos F2: without it
    // every connection leaked this set's epoll fd). Beside the handle, the
    // socket position's mask as last set (D4484, `pq_modify_fe_be_wait_set_socket`).
    static FE_BE_WAIT_SET: Cell<Option<(WaitEventSetHandle, u32)>> = const { Cell::new(None) };
    // static List *sock_paths (postmaster-thread state).
    static SOCK_PATHS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    static INHERITED: RefCell<(Vec<pgsocket>, String, String)> =
        const { RefCell::new((Vec::new(), String::new(), String::new())) };
    // Live (sock, noblock, ssl_in_use), the transport's runtime truth.
    // INVARIANT: the send/recv path reads these, never MyProcPort — a FATAL
    // raised under WithMyProcPort (auth) sends to the client; a RefCell
    // re-borrow there panics the backend. Port.sock/noblock stay pq_init-time.
    static CLIENT_STATE: Cell<Option<(pgsocket, bool, bool)>> = const { Cell::new(None) };
}

pub fn client_socket_state() -> Option<(pgsocket, bool, bool)> {
    CLIENT_STATE.get()
}

pub fn set_ssl_in_use(ssl_in_use: bool) {
    if let Some((sock, noblock, _)) = CLIENT_STATE.get() {
        CLIENT_STATE.set(Some((sock, noblock, ssl_in_use)));
    }
}

// GUC storage declared in pqcomm.c; boot values from guc_tables.c.
mod cfg {
    use std::cell::{Cell, RefCell};

    thread_local! {
        pub static TCP_KEEPALIVES_IDLE: Cell<i32> = const { Cell::new(0) };
        pub static TCP_KEEPALIVES_INTERVAL: Cell<i32> = const { Cell::new(0) };
        pub static TCP_KEEPALIVES_COUNT: Cell<i32> = const { Cell::new(0) };
        pub static TCP_USER_TIMEOUT: Cell<i32> = const { Cell::new(0) };
        pub static UNIX_SOCKET_PERMISSIONS: Cell<i32> = const { Cell::new(0o777) };
        pub static UNIX_SOCKET_GROUP: RefCell<String> = const { RefCell::new(String::new()) };
        // client_connection_check_interval (ms, 0 = disabled): the in-query
        // dead-client poll (GL-DISCONNECT-WEDGE-1). A killed client's parked
        // parallel leader has no other cancel vector: it never touches the
        // socket from a blocking tuple-queue receive, and the disconnect
        // took the only session that could cancel it.
        pub static CLIENT_CONNECTION_CHECK_INTERVAL: Cell<i32> = const { Cell::new(0) };
    }
}

pub fn unix_socket_group() -> String {
    cfg::UNIX_SOCKET_GROUP.with(|s| s.borrow().clone())
}

pub fn unix_socket_permissions() -> i32 {
    cfg::UNIX_SOCKET_PERMISSIONS.get()
}

fn setsockopt_int(sock: pgsocket, level: i32, optname: i32, val: i32) -> Result<(), ()> {
    let val: libc::c_int = val;
    // SAFETY: val outlives the call; optlen matches.
    let rc = unsafe {
        ip::sys::setsockopt(
            sock,
            level,
            optname,
            std::ptr::from_ref(&val).cast(),
            std::mem::size_of::<libc::c_int>() as ip::sys::socklen_t,
        )
    };
    if rc < 0 {
        Err(())
    } else {
        Ok(())
    }
}

fn getsockopt_int(sock: pgsocket, level: i32, optname: i32) -> Result<i32, ()> {
    let mut val: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>() as ip::sys::socklen_t;
    // SAFETY: out-pointers sized by `size`.
    let rc = unsafe {
        ip::sys::getsockopt(
            sock,
            level,
            optname,
            std::ptr::from_mut(&mut val).cast(),
            &mut size,
        )
    };
    if rc < 0 {
        Err(())
    } else {
        Ok(val)
    }
}

pub fn pq_init(client_sock: &ClientSocket) -> PgResult<Port> {
    let mut port = Port::new(client_sock);

    port.laddr.salen = port.laddr.addr.len() as u32;
    // SAFETY: laddr.addr is sockaddr_storage-sized; salen is in/out.
    if unsafe {
        ip::sys::getsockname(
            port.sock,
            port.laddr.addr.as_mut_ptr().cast::<ip::sys::sockaddr>(),
            &mut port.laddr.salen,
        )
    } < 0
    {
        ereport(FATAL)
            .with_saved_errno(errno())
            .errmsg(format!("getsockname() failed: {PCT_M}"))
            .finish(loc("pq_init"))?;
    }

    if sockaddr_family(&port.laddr) != ip::sys::AF_UNIX {
        if setsockopt_int(port.sock, ip::sys::IPPROTO_TCP, ip::sys::TCP_NODELAY, 1).is_err() {
            ereport(FATAL)
                .with_saved_errno(errno())
                .errmsg(format!("setsockopt(TCP_NODELAY) failed: {PCT_M}"))
                .finish(loc("pq_init"))?;
        }
        if setsockopt_int(port.sock, ip::sys::SOL_SOCKET, ip::sys::SO_KEEPALIVE, 1).is_err() {
            ereport(FATAL)
                .with_saved_errno(errno())
                .errmsg(format!("setsockopt(SO_KEEPALIVE) failed: {PCT_M}"))
                .finish(loc("pq_init"))?;
        }

        // Keepalive GUC failures don't error out (not universally supported).
        let _ = pq_setkeepalivesidle(cfg::TCP_KEEPALIVES_IDLE.get(), Some(&mut port));
        let _ = pq_setkeepalivesinterval(cfg::TCP_KEEPALIVES_INTERVAL.get(), Some(&mut port));
        let _ = pq_setkeepalivescount(cfg::TCP_KEEPALIVES_COUNT.get(), Some(&mut port));
        let _ = pq_settcpusertimeout(cfg::TCP_USER_TIMEOUT.get(), Some(&mut port));
    }

    crate::pq_init_buffers()?;

    CLIENT_STATE.set(Some((port.sock, port.noblock, port.ssl_in_use)));

    ipc_seams::on_proc_exit::call(socket_close, 0);

    // The socket runs in nonblocking mode from here on; latches provide the
    // blocking semantics (safely interruptible reads/writes). Inlined
    // pg_set_noblock (port/noblock.c): F_GETFL | O_NONBLOCK.
    let flags = unsafe { libc::fcntl(port.sock, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(port.sock, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        ereport(FATAL)
            .with_saved_errno(errno())
            .errmsg(format!("could not set socket to nonblocking mode: {PCT_M}"))
            .finish(loc("pq_init"))?;
    }

    if unsafe { libc::fcntl(port.sock, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        ereport(FATAL)
            .with_saved_errno(errno())
            .errmsg_internal(format!("fcntl(F_SETFD) failed on socket: {PCT_M}"))
            .finish(loc("pq_init"))?;
    }

    let set = waiteventset_seams::create_wait_event_set::call(FeBeWaitSetNEvents)?;
    let socket_pos = waiteventset_seams::add_wait_event_to_set::call(
        set,
        WL_SOCKET_WRITEABLE,
        port.sock,
        None,
        None,
    )?;
    let latch = g::MyLatch().expect("pq_init: MyLatch is not set");
    let latch_pos = waiteventset_seams::add_wait_event_to_set::call(
        set,
        WL_LATCH_SET,
        PGINVALID_SOCKET,
        Some(latch),
        None,
    )?;
    // C adds WL_POSTMASTER_DEATH third; the threaded waiteventset has none
    // (postmaster exit takes the whole process down).
    FE_BE_WAIT_SET.set(Some((set, WL_SOCKET_WRITEABLE)));

    debug_assert_eq!(socket_pos, FeBeWaitSetSocketPos);
    debug_assert_eq!(latch_pos, FeBeWaitSetLatchPos);

    Ok(port)
}

/// D4682 (client backend thread reuse, launch_backend cpool): the client wire state a parked thread drops before its
/// next connection. FE_BE_WAIT_SET's set goes (its socket is closed; the next pq_init makes a new set), as a thread exit
/// frees it (WaitEventSetReleaseGuard), and CLIENT_STATE's socket with it. PQ's pointers and the send buffer are reset
/// by pq_init_buffers.
pub fn reset_for_thread_reuse() {
    if let Some((set, _)) = FE_BE_WAIT_SET.take() {
        waiteventset_seams::free_wait_event_set::call(set);
    }
    CLIENT_STATE.set(None);
}

// on_proc_exit hook: stop I/O but leave the fd open until process death.
fn socket_close(_code: i32, _arg: usize) {
    if let Some((_, noblock, ssl_in_use)) = CLIENT_STATE.get() {
        if ssl_in_use {
            be_secure_seams::secure_close::call();
        }
        CLIENT_STATE.set(Some((PGINVALID_SOCKET, noblock, false)));
    }
}

fn set_port_noblock(noblock: bool) -> bool {
    let Some((sock, _, ssl_in_use)) = CLIENT_STATE.get() else {
        return false;
    };
    CLIENT_STATE.set(Some((sock, noblock, ssl_in_use)));
    true
}

pub fn pq_modify_fe_be_wait_set_latch(latch: LatchHandle) -> PgResult<()> {
    match FE_BE_WAIT_SET.get() {
        Some((set, _)) => waiteventset_seams::modify_wait_event::call(
            set,
            FeBeWaitSetLatchPos,
            WL_LATCH_SET,
            Some(latch),
        ),
        None => Ok(()),
    }
}

/// `ModifyWaitEvent(FeBeWaitSet, FeBeWaitSetSocketPos, events, NULL)`. D4484: every wait on the client socket asks
/// for its mask again (a read wait after a read wait asks READABLE again), and ModifyWaitEvent's own fast path then
/// changes nothing (a socket position holds no latch: `events == old_events` returns at once). That no-op is
/// answered here, from the mask this function last set (or `pq_init`'s WL_SOCKET_WRITEABLE), without the seam,
/// MyProcPid or the set's RefCell. Every modify of the socket position goes through this function (secure_read /
/// secure_write, the GSSAPI and OpenSSL handshakes, the walsender, pq_check_connection), so the mask cannot drift
/// from the set's; any other mask (a blocked send's WRITEABLE, pq_check_connection's CLOSED) still goes to
/// ModifyWaitEvent. A failed modify forgets the mask (ModifyWaitEvent stores the new mask before its epoll_ctl):
/// the next call always goes through.
pub fn pq_modify_fe_be_wait_set_socket(events: u32) -> PgResult<()> {
    let (set, mask) = FE_BE_WAIT_SET.get().expect("FeBeWaitSet not created");
    if mask == events {
        return Ok(());
    }
    let r = waiteventset_seams::modify_wait_event::call(set, FeBeWaitSetSocketPos, events, None);
    FE_BE_WAIT_SET.set(Some((set, if r.is_ok() { events } else { MASK_UNKNOWN })));
    r
}

/// No wait mask (the WL_* bits fit well below it): the socket position's mask after a failed modify.
const MASK_UNKNOWN: u32 = u32::MAX;

/// One-event `WaitEventSetWait(FeBeWaitSet, ...)`; returns the fired event's
/// wakeup bits (0 on timeout).
pub fn pq_wait_event_set_wait_fe_be(timeout: i64, wait_event_info: u32) -> PgResult<u32> {
    let (set, _) = FE_BE_WAIT_SET.get().expect("FeBeWaitSet not created");
    let event = waiteventset_seams::wait_event_set_wait_one::call(set, timeout, wait_event_info)?;
    Ok(event.map_or(0, |e| e.events))
}

/// `pq_check_connection` (pqcomm.c): true = the client connection still
/// looks alive; false = the peer closed/reset it. Zero-timeout poll of
/// FeBeWaitSet with the socket filter switched to WL_SOCKET_CLOSED
/// (EPOLLRDHUP / kqueue EV_EOF — fires on orderly EOF and on RST, even
/// with unread data pending). Leaving the filter modified is fine: every
/// FeBeWaitSet socket wait site (secure_read/secure_write, walsender)
/// re-modifies before waiting, exactly C's contract.
pub fn pq_check_connection() -> PgResult<bool> {
    pq_modify_fe_be_wait_set_socket(WL_SOCKET_CLOSED)?;
    loop {
        let events = pq_wait_event_set_wait_fe_be(0, 0)?;
        if events & WL_SOCKET_CLOSED != 0 {
            return Ok(false);
        }
        if events & WL_LATCH_SET != 0 {
            // C: consume the latch and re-poll so a set latch cannot mask a
            // closed socket (the one-event wait reports the latch first).
            // Eating a set here is C-sanctioned: every blocking wait site is
            // a recheck loop.
            latch_seams::reset_latch_my_latch::call();
            continue;
        }
        return Ok(true);
    }
}

// UNIXSOCK_PATH snprintf into ListenServerPort's char[MAXPGPATH]: the "too
// long" LOG (pqcomm.c:457) echoes at most MAXPGPATH-1 bytes of it.
pub(crate) fn unix_socket_path_for(dir: &str, port_number: u16) -> String {
    let mut path = format!("{}/.s.PGSQL.{}", dir, port_number);
    if path.len() >= ::types_core::primitive::MAXPGPATH {
        let mut cut = ::types_core::primitive::MAXPGPATH - 1;
        while !path.is_char_boundary(cut) {
            cut -= 1;
        }
        path.truncate(cut);
    }
    path
}

fn sun_path_buflen() -> usize {
    // SAFETY: plain-old-data zero pattern.
    let su: ip::sys::sockaddr_un = unsafe { std::mem::zeroed() };
    su.sun_path.len()
}

fn gai_strerror_string(err: i32) -> String {
    // SAFETY: gai_strerror returns a static NUL-terminated message.
    unsafe { std::ffi::CStr::from_ptr(ip::sys::gai_strerror(err)) }
        .to_string_lossy()
        .into_owned()
}

/// Open a listen socket; opened fds are appended to `listen_sockets`
/// (C `ListenSockets[]`/`*NumListenSockets`, `max_listen` = MaxListen).
pub fn ListenServerPort(
    family: i32,
    host_name: Option<&str>,
    port_number: u16,
    unix_socket_dir: Option<&str>,
    listen_sockets: &mut Vec<pgsocket>,
    max_listen: usize,
) -> PgResult<i32> {
    let hint = AddrInfoHint {
        flags: ip::sys::AI_PASSIVE,
        family,
        socktype: ip::sys::SOCK_STREAM,
    };

    let mut unix_socket_path = String::new();
    let service: String;
    if family == ip::sys::AF_UNIX {
        let dir = unix_socket_dir.expect("ListenServerPort: AF_UNIX requires unixSocketDir");
        debug_assert!(!dir.is_empty());
        unix_socket_path = unix_socket_path_for(dir, port_number);
        if unix_socket_path.len() >= sun_path_buflen() {
            let _ = ereport(LOG)
                .errmsg(format!(
                    "Unix-domain socket path \"{}\" is too long (maximum {} bytes)",
                    unix_socket_path,
                    sun_path_buflen() - 1
                ))
                .finish(loc("ListenServerPort"));
            return Ok(STATUS_ERROR);
        }
        if Lock_AF_UNIX(dir, &unix_socket_path, true)? != STATUS_OK {
            return Ok(STATUS_ERROR);
        }
        service = unix_socket_path.clone();
    } else {
        service = format!("{}", port_number);
    }

    let mut addrs: Vec<PgAddrInfo> = Vec::new();
    let ret = ip::pg_getaddrinfo_all(host_name, Some(&service), &hint, &mut addrs);
    if ret != 0 || addrs.is_empty() {
        let gai = gai_strerror_string(ret);
        let _ = match host_name {
            Some(host_name) => ereport(LOG).errmsg(format!(
                "could not translate host name \"{}\", service \"{}\" to address: {}",
                host_name, service, gai
            )),
            None => ereport(LOG).errmsg(format!(
                "could not translate service \"{}\" to address: {}",
                service, gai
            )),
        }
        .finish(loc("ListenServerPort"));
        return Ok(STATUS_ERROR);
    }

    let mut added = 0usize;
    for addr in &addrs {
        // Unix sockets only when asked for (the service/port differs then).
        if family != ip::sys::AF_UNIX && addr.family == ip::sys::AF_UNIX {
            continue;
        }

        if listen_sockets.len() == max_listen {
            let _ = ereport(LOG)
                .errmsg(format!(
                    "could not bind to all requested addresses: MAXLISTEN ({}) exceeded",
                    max_listen
                ))
                .finish(loc("ListenServerPort"));
            break;
        }

        let family_desc: String = match addr.family {
            x if x == ip::sys::AF_INET => "IPv4".to_owned(),
            x if x == ip::sys::AF_INET6 => "IPv6".to_owned(),
            x if x == ip::sys::AF_UNIX => "Unix".to_owned(),
            other => format!("unrecognized address family {}", other),
        };
        let addr_desc: String = if addr.family == ip::sys::AF_UNIX {
            unix_socket_path.clone()
        } else {
            let mut node = String::new();
            ip::pg_getnameinfo_all(&addr.addr, Some(&mut node), None, ip::sys::NI_NUMERICHOST);
            node
        };

        // SAFETY: plain socket(2).
        let fd = unsafe { ip::sys::socket(addr.family, ip::sys::SOCK_STREAM, 0) };
        if fd == PGINVALID_SOCKET {
            let _ = ereport(LOG)
                .with_saved_errno(errno())
                .errcode_for_socket_access()
                .errmsg(format!(
                    "could not create {} socket for address \"{}\": {PCT_M}",
                    family_desc, addr_desc
                ))
                .finish(loc("ListenServerPort"));
            continue;
        }

        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            ereport(FATAL)
                .with_saved_errno(errno())
                .errmsg_internal(format!("fcntl(F_SETFD) failed on socket: {PCT_M}"))
                .finish(loc("ListenServerPort"))?;
        }

        // Without SO_REUSEADDR a new postmaster can't start right away after
        // a stop or crash.
        if addr.family != ip::sys::AF_UNIX
            && setsockopt_int(fd, ip::sys::SOL_SOCKET, ip::sys::SO_REUSEADDR, 1).is_err()
        {
            let _ = ereport(LOG)
                .with_saved_errno(errno())
                .errcode_for_socket_access()
                .errmsg(format!(
                    "setsockopt(SO_REUSEADDR) failed for {} address \"{}\": {PCT_M}",
                    family_desc, addr_desc
                ))
                .finish(loc("ListenServerPort"));
            unsafe { libc::close(fd) };
            continue;
        }

        if addr.family == ip::sys::AF_INET6
            && setsockopt_int(fd, ip::sys::IPPROTO_IPV6, ip::sys::IPV6_V6ONLY, 1).is_err()
        {
            let _ = ereport(LOG)
                .with_saved_errno(errno())
                .errcode_for_socket_access()
                .errmsg(format!(
                    "setsockopt(IPV6_V6ONLY) failed for {} address \"{}\": {PCT_M}",
                    family_desc, addr_desc
                ))
                .finish(loc("ListenServerPort"));
            unsafe { libc::close(fd) };
            continue;
        }

        // SAFETY: addr.addr holds salen valid sockaddr bytes.
        let err = unsafe {
            ip::sys::bind(
                fd,
                addr.addr.addr.as_ptr().cast::<ip::sys::sockaddr>(),
                addr.addr.salen as ip::sys::socklen_t,
            )
        };
        if err < 0 {
            let saved_errno = errno();
            let mut b = ereport(LOG)
                .with_saved_errno(saved_errno)
                .errcode_for_socket_access()
                .errmsg(format!(
                    "could not bind {} address \"{}\": {PCT_M}",
                    family_desc, addr_desc
                ));
            if saved_errno == libc::EADDRINUSE {
                b = if addr.family == ip::sys::AF_UNIX {
                    b.errhint(format!(
                        "Is another postmaster already running on port {}?",
                        port_number
                    ))
                } else {
                    b.errhint(format!(
                        "Is another postmaster already running on port {}? If not, wait a few seconds and retry.",
                        port_number
                    ))
                };
            }
            let _ = b.finish(loc("ListenServerPort"));
            unsafe { libc::close(fd) };
            continue;
        }

        if addr.family == ip::sys::AF_UNIX && Setup_AF_UNIX(&service)? != STATUS_OK {
            unsafe { libc::close(fd) };
            break;
        }

        // Accept-queue length: similar to the maximum number of children the
        // postmaster will permit.
        let maxconn = g::MaxConnections() * 2;

        if unsafe { ip::sys::listen(fd, maxconn) } < 0 {
            let _ = ereport(LOG)
                .with_saved_errno(errno())
                .errcode_for_socket_access()
                .errmsg(format!(
                    "could not listen on {} address \"{}\": {PCT_M}",
                    family_desc, addr_desc
                ))
                .finish(loc("ListenServerPort"));
            unsafe { libc::close(fd) };
            continue;
        }

        let _ = if addr.family == ip::sys::AF_UNIX {
            ereport(LOG).errmsg(format!("listening on Unix socket \"{}\"", addr_desc))
        } else {
            ereport(LOG).errmsg(format!(
                "listening on {} address \"{}\", port {}",
                family_desc, addr_desc, port_number
            ))
        }
        .finish(loc("ListenServerPort"));

        listen_sockets.push(fd);
        added += 1;
    }

    if added == 0 {
        return Ok(STATUS_ERROR);
    }
    Ok(STATUS_OK)
}

fn Lock_AF_UNIX(unix_socket_dir: &str, unix_socket_path: &str, remove_stale: bool) -> PgResult<i32> {
    // No lock file for abstract sockets.
    if unix_socket_path.starts_with('@') {
        return Ok(STATUS_OK);
    }

    miscinit_seams::create_socket_lock_file::call(unix_socket_path, true, unix_socket_dir)?;

    if remove_stale {
        // Interlock held: delete any pre-existing socket file before bind().
        let c = CString::new(unix_socket_path).expect("socket path contains NUL");
        unsafe { libc::unlink(c.as_ptr()) };
    }

    SOCK_PATHS.with(|p| p.borrow_mut().push(unix_socket_path.to_owned()));

    Ok(STATUS_OK)
}

pub fn inherit_listen_sockets(fds: &[pgsocket], listen_line: &str, socket_dir_line: &str) {
    INHERITED.with(|v| {
        let mut v = v.borrow_mut();
        v.0.clear();
        v.0.extend_from_slice(fds);
        v.1.clear();
        v.1.push_str(listen_line);
        v.2.clear();
        v.2.push_str(socket_dir_line);
    });
}

fn local_sockaddr(fd: pgsocket) -> Option<ip::SockAddr> {
    let mut a = ip::SockAddr::zeroed();
    let mut len = a.addr.len() as ip::sys::socklen_t;
    // SAFETY: getsockname(2) into a sockaddr_storage-sized buffer.
    let rc = unsafe { ip::sys::getsockname(fd, a.addr.as_mut_ptr().cast::<ip::sys::sockaddr>(), &mut len) };
    if rc != 0 {
        return None;
    }
    a.salen = len as u32;
    Some(a)
}

fn unix_path_of(a: &ip::SockAddr) -> Option<String> {
    if sockaddr_family(a) != ip::sys::AF_UNIX {
        return None;
    }
    let path = &a.addr[2..(a.salen as usize).clamp(2, a.addr.len())];
    let end = path.iter().position(|&b| b == 0).unwrap_or(path.len());
    std::str::from_utf8(&path[..end]).ok().map(str::to_owned)
}

/// C divergence: a crash-restarted generation keeps the first one's sockets.
pub fn adopt_inherited_listen_sockets(
    listen_sockets: &mut Vec<pgsocket>,
) -> PgResult<(String, String)> {
    let (rest, listen_line, socket_dir_line) = INHERITED.with(|v| std::mem::take(&mut *v.borrow_mut()));
    for fd in rest {
        if let Some(path) = local_sockaddr(fd).as_ref().and_then(unix_path_of).filter(|p| !p.is_empty()) {
            let dir = path.rsplit_once('/').map_or(".", |(d, _)| d).to_owned();
            if Lock_AF_UNIX(&dir, &path, false)? != STATUS_OK {
                continue;
            }
        }
        listen_sockets.push(fd);
    }
    Ok((listen_line, socket_dir_line))
}

// C strtoul(s, &endptr, 10) with the `*endptr == '\0'` full-consumption test.
fn parse_strtoul_full(s: &str) -> Option<u64> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') {
        i += 1;
    }
    let mut negative = false;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        negative = bytes[i] == b'-';
        i += 1;
    }
    let start = i;
    let mut value: u64 = 0;
    let mut overflowed = false;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        let (mul, o1) = value.overflowing_mul(10);
        let (add, o2) = mul.overflowing_add(u64::from(bytes[i] - b'0'));
        overflowed = overflowed || o1 || o2;
        value = add;
        i += 1;
    }
    if i == start || i != bytes.len() {
        return None;
    }
    if overflowed {
        // strtoul clamps to ULONG_MAX on overflow; the C caller ignores ERANGE.
        return Some(u64::MAX);
    }
    Some(if negative {
        value.wrapping_neg()
    } else {
        value
    })
}

// Fix socket ownership/permission before listen(), closing the window where
// unwanted connections could get accepted.
fn Setup_AF_UNIX(sock_path: &str) -> PgResult<i32> {
    // No file system permissions for abstract sockets.
    if sock_path.starts_with('@') {
        return Ok(STATUS_OK);
    }

    let path_c = CString::new(sock_path).expect("socket path contains NUL");

    let group = unix_socket_group();
    if !group.is_empty() {
        let gid: libc::gid_t = if let Some(val) = parse_strtoul_full(&group) {
            val as libc::gid_t
        } else {
            let group_c = CString::new(group.as_str()).expect("group name contains NUL");
            // SAFETY: NUL-terminated name; result checked for NULL before use.
            let gr = unsafe { ip::sys::getgrnam(group_c.as_ptr()) };
            if gr.is_null() {
                let _ = ereport(LOG)
                    .errmsg(format!("group \"{}\" does not exist", group))
                    .finish(loc("Setup_AF_UNIX"));
                return Ok(STATUS_ERROR);
            }
            unsafe { (*gr).gr_gid }
        };
        // uid_t::MAX is C's (uid_t) -1 "don't change owner".
        if unsafe { ip::sys::chown(path_c.as_ptr(), libc::uid_t::MAX, gid) } == -1 {
            let _ = ereport(LOG)
                .with_saved_errno(errno())
                .errcode_for_file_access()
                .errmsg(format!("could not set group of file \"{}\": {PCT_M}", sock_path))
                .finish(loc("Setup_AF_UNIX"));
            return Ok(STATUS_ERROR);
        }
    }

    if unsafe { libc::chmod(path_c.as_ptr(), unix_socket_permissions() as libc::mode_t) } == -1 {
        let _ = ereport(LOG)
            .with_saved_errno(errno())
            .errcode_for_file_access()
            .errmsg(format!(
                "could not set permissions of file \"{}\": {PCT_M}",
                sock_path
            ))
            .finish(loc("Setup_AF_UNIX"));
        return Ok(STATUS_ERROR);
    }
    Ok(STATUS_OK)
}

pub fn AcceptConnection(server_fd: pgsocket, client_sock: &mut ClientSocket) -> i32 {
    client_sock.raddr.salen = client_sock.raddr.addr.len() as u32;
    // SAFETY: raddr.addr is sockaddr_storage-sized; salen is in/out.
    let fd = unsafe {
        ip::sys::accept(
            server_fd,
            client_sock.raddr.addr.as_mut_ptr().cast::<ip::sys::sockaddr>(),
            &mut client_sock.raddr.salen,
        )
    };
    if fd == PGINVALID_SOCKET {
        client_sock.sock = PGINVALID_SOCKET;
        let _ = ereport(LOG)
            .with_saved_errno(errno())
            .errcode_for_socket_access()
            .errmsg(format!("could not accept new connection: {PCT_M}"))
            .finish(loc("AcceptConnection"));

        // The postmaster retries immediately on read-ready; delay a bit.
        std::thread::sleep(std::time::Duration::from_micros(100000));
        return STATUS_ERROR;
    }
    client_sock.sock = fd;

    STATUS_OK
}

/// Mark socket files recently accessed, protecting them from /tmp cleaners.
#[cfg(not(target_family = "wasm"))]
pub fn TouchSocketFiles() {
    SOCK_PATHS.with(|p| {
        for sock_path in p.borrow().iter() {
            if let Ok(c) = CString::new(sock_path.as_str()) {
                // Errors ignored; NULL utimbuf sets times to now.
                unsafe { libc::utime(c.as_ptr(), std::ptr::null()) };
            }
        }
    });
}

// wasm32: AF_UNIX socket files are never created on WASI (no sockets), so
// there is nothing to touch; the wasi libc crate also exposes no utime.
#[cfg(target_family = "wasm")]
pub fn TouchSocketFiles() {}

pub fn RemoveSocketFiles() {
    SOCK_PATHS.with(|p| {
        let mut paths = p.borrow_mut();
        for sock_path in paths.iter() {
            if let Ok(c) = CString::new(sock_path.as_str()) {
                unsafe { libc::unlink(c.as_ptr()) };
            }
        }
        paths.clear();
    });
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
const PG_TCP_KEEPALIVE_IDLE: i32 = libc::TCP_KEEPALIVE;
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(crate) const PG_TCP_KEEPALIVE_IDLE_STR: &str = "TCP_KEEPALIVE";
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
const PG_TCP_KEEPALIVE_IDLE: i32 = ip::sys::TCP_KEEPIDLE;
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
pub(crate) const PG_TCP_KEEPALIVE_IDLE_STR: &str = "TCP_KEEPIDLE";

fn log_sockopt_failure(call: &str, optname: &str, funcname: &'static str) {
    let _ = ereport(LOG)
        .with_saved_errno(errno())
        .errmsg(format!("{}({}) failed: {PCT_M}", call, optname))
        .finish(loc(funcname));
}

/// A Port's keepalive knobs, copied out so the syscalls and their LOG never run under a
/// `MyProcPort` borrow: the log sink re-borrows the Port inside every ereport (the logconn
/// hazard, `c52e793288b`), so an exclusive borrow spans a field store only — `of` under the
/// shared borrow, the kernel calls on the copy, `store` under the exclusive one.
#[derive(Clone, Copy)]
struct Keepalives {
    sock: pgsocket,
    unix: bool,
    default_idle: i32,
    default_interval: i32,
    default_count: i32,
    default_user_timeout: i32,
    idle: i32,
    interval: i32,
    count: i32,
    user_timeout: i32,
}

impl Keepalives {
    fn of(port: &Port) -> Keepalives {
        Keepalives {
            sock: port.sock,
            unix: sockaddr_family(&port.laddr) == ip::sys::AF_UNIX,
            default_idle: port.default_keepalives_idle,
            default_interval: port.default_keepalives_interval,
            default_count: port.default_keepalives_count,
            default_user_timeout: port.default_tcp_user_timeout,
            idle: port.keepalives_idle,
            interval: port.keepalives_interval,
            count: port.keepalives_count,
            user_timeout: port.tcp_user_timeout,
        }
    }

    fn store(&self, port: &mut Port) {
        port.default_keepalives_idle = self.default_idle;
        port.default_keepalives_interval = self.default_interval;
        port.default_keepalives_count = self.default_count;
        port.default_tcp_user_timeout = self.default_user_timeout;
        port.keepalives_idle = self.idle;
        port.keepalives_interval = self.interval;
        port.keepalives_count = self.count;
        port.tcp_user_timeout = self.user_timeout;
    }
}

struct Knob {
    opt: i32,
    name: &'static str,
    getter: &'static str,
    setter: &'static str,
    cur: fn(&mut Keepalives) -> &mut i32,
    dflt: fn(&mut Keepalives) -> &mut i32,
}

const IDLE: Knob = Knob {
    opt: PG_TCP_KEEPALIVE_IDLE,
    name: PG_TCP_KEEPALIVE_IDLE_STR,
    getter: "pq_getkeepalivesidle",
    setter: "pq_setkeepalivesidle",
    cur: |k| &mut k.idle,
    dflt: |k| &mut k.default_idle,
};
const INTERVAL: Knob = Knob {
    opt: ip::sys::TCP_KEEPINTVL,
    name: "TCP_KEEPINTVL",
    getter: "pq_getkeepalivesinterval",
    setter: "pq_setkeepalivesinterval",
    cur: |k| &mut k.interval,
    dflt: |k| &mut k.default_interval,
};
const COUNT: Knob = Knob {
    opt: ip::sys::TCP_KEEPCNT,
    name: "TCP_KEEPCNT",
    getter: "pq_getkeepalivescount",
    setter: "pq_setkeepalivescount",
    cur: |k| &mut k.count,
    dflt: |k| &mut k.default_count,
};
#[cfg(target_os = "linux")]
const USER_TIMEOUT: Knob = Knob {
    opt: libc::TCP_USER_TIMEOUT,
    name: "TCP_USER_TIMEOUT",
    getter: "pq_gettcpusertimeout",
    setter: "pq_settcpusertimeout",
    cur: |k| &mut k.user_timeout,
    dflt: |k| &mut k.default_user_timeout,
};

fn knob_get(k: &mut Keepalives, knob: &Knob) -> i32 {
    if k.unix {
        return 0;
    }
    if *(knob.cur)(k) != 0 {
        return *(knob.cur)(k);
    }
    if *(knob.dflt)(k) == 0 {
        match getsockopt_int(k.sock, ip::sys::IPPROTO_TCP, knob.opt) {
            Ok(v) => *(knob.dflt)(k) = v,
            Err(()) => {
                log_sockopt_failure("getsockopt", knob.name, knob.getter);
                *(knob.dflt)(k) = -1;
            }
        }
    }
    *(knob.dflt)(k)
}

// pq_set*: 0 restores the kernel default (unknown default: 0 is a no-op, anything else an error).
fn knob_set(value: i32, k: &mut Keepalives, knob: &Knob) -> i32 {
    if k.unix {
        return STATUS_OK;
    }
    if value == *(knob.cur)(k) {
        return STATUS_OK;
    }
    if *(knob.dflt)(k) <= 0 && knob_get(k, knob) < 0 {
        if value == 0 {
            return STATUS_OK;
        }
        return STATUS_ERROR;
    }
    let value = if value == 0 { *(knob.dflt)(k) } else { value };
    if setsockopt_int(k.sock, ip::sys::IPPROTO_TCP, knob.opt, value).is_err() {
        log_sockopt_failure("setsockopt", knob.name, knob.setter);
        return STATUS_ERROR;
    }
    *(knob.cur)(k) = value;
    STATUS_OK
}

fn on_port<R>(port: Option<&mut Port>, none: R, f: impl FnOnce(&mut Keepalives) -> R) -> R {
    let Some(port) = port else { return none };
    let mut k = Keepalives::of(port);
    let r = f(&mut k);
    k.store(port);
    r
}

pub fn pq_getkeepalivesidle(port: Option<&mut Port>) -> i32 {
    on_port(port, 0, |k| knob_get(k, &IDLE))
}

pub fn pq_setkeepalivesidle(idle: i32, port: Option<&mut Port>) -> i32 {
    on_port(port, STATUS_OK, |k| knob_set(idle, k, &IDLE))
}

pub fn pq_getkeepalivesinterval(port: Option<&mut Port>) -> i32 {
    on_port(port, 0, |k| knob_get(k, &INTERVAL))
}

pub fn pq_setkeepalivesinterval(interval: i32, port: Option<&mut Port>) -> i32 {
    on_port(port, STATUS_OK, |k| knob_set(interval, k, &INTERVAL))
}

pub fn pq_getkeepalivescount(port: Option<&mut Port>) -> i32 {
    on_port(port, 0, |k| knob_get(k, &COUNT))
}

pub fn pq_setkeepalivescount(count: i32, port: Option<&mut Port>) -> i32 {
    on_port(port, STATUS_OK, |k| knob_set(count, k, &COUNT))
}

#[cfg(target_os = "linux")]
pub fn pq_gettcpusertimeout(port: Option<&mut Port>) -> i32 {
    on_port(port, 0, |k| knob_get(k, &USER_TIMEOUT))
}

#[cfg(target_os = "linux")]
pub fn pq_settcpusertimeout(timeout: i32, port: Option<&mut Port>) -> i32 {
    on_port(port, STATUS_OK, |k| knob_set(timeout, k, &USER_TIMEOUT))
}

// Non-Linux: no TCP_USER_TIMEOUT (the C #else arms).
#[cfg(not(target_os = "linux"))]
pub fn pq_gettcpusertimeout(_port: Option<&mut Port>) -> i32 {
    0
}

#[cfg(not(target_os = "linux"))]
fn user_timeout_unsupported(timeout: i32, k: &mut Keepalives) -> i32 {
    if k.unix || timeout == 0 {
        return STATUS_OK;
    }
    let _ = ereport(LOG)
        .errmsg("setsockopt(TCP_USER_TIMEOUT) not supported")
        .finish(loc("pq_settcpusertimeout"));
    STATUS_ERROR
}

#[cfg(not(target_os = "linux"))]
pub fn pq_settcpusertimeout(timeout: i32, port: Option<&mut Port>) -> i32 {
    on_port(port, STATUS_OK, |k| user_timeout_unsupported(timeout, k))
}

fn on_my_port<R>(none: R, f: impl FnOnce(&mut Keepalives) -> R) -> R {
    if !g::HaveMyProcPort() {
        return none;
    }
    let mut k = g::WithMyProcPort(Keepalives::of);
    let r = f(&mut k);
    g::WithMyProcPortMut(|port| k.store(port));
    r
}

// The kernel API can't test a keepalive value without setting it, so GUC
// assignment happens on demand and show reads back the kernel truth.
pub(crate) fn assign_tcp_keepalives_idle(newval: i32) {
    cfg::TCP_KEEPALIVES_IDLE.set(newval);
    let _ = on_my_port(STATUS_OK, |k| knob_set(newval, k, &IDLE));
}

pub(crate) fn show_tcp_keepalives_idle() -> String {
    on_my_port(0, |k| knob_get(k, &IDLE)).to_string()
}

pub(crate) fn assign_tcp_keepalives_interval(newval: i32) {
    cfg::TCP_KEEPALIVES_INTERVAL.set(newval);
    let _ = on_my_port(STATUS_OK, |k| knob_set(newval, k, &INTERVAL));
}

pub(crate) fn show_tcp_keepalives_interval() -> String {
    on_my_port(0, |k| knob_get(k, &INTERVAL)).to_string()
}

pub(crate) fn assign_tcp_keepalives_count(newval: i32) {
    cfg::TCP_KEEPALIVES_COUNT.set(newval);
    let _ = on_my_port(STATUS_OK, |k| knob_set(newval, k, &COUNT));
}

pub(crate) fn show_tcp_keepalives_count() -> String {
    on_my_port(0, |k| knob_get(k, &COUNT)).to_string()
}

#[cfg(target_os = "linux")]
pub(crate) fn assign_tcp_user_timeout(newval: i32) {
    cfg::TCP_USER_TIMEOUT.set(newval);
    let _ = on_my_port(STATUS_OK, |k| knob_set(newval, k, &USER_TIMEOUT));
}

#[cfg(target_os = "linux")]
pub(crate) fn show_tcp_user_timeout() -> String {
    on_my_port(0, |k| knob_get(k, &USER_TIMEOUT)).to_string()
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn assign_tcp_user_timeout(newval: i32) {
    cfg::TCP_USER_TIMEOUT.set(newval);
    let _ = on_my_port(STATUS_OK, |k| user_timeout_unsupported(newval, k));
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn show_tcp_user_timeout() -> String {
    "0".to_string()
}

/// Install the socket-half seams and this file's GUC slots. Kept apart from
/// [`crate::init_seams`]: test binaries that stub the transport install that
/// one alone.
///
/// P4 split (wasm-net-seam worklog §8 pin): the listen/accept pair moved
/// HERE from `crate::init_seams` — the postmaster-side transport half is
/// provider-owned exactly like the per-connection half, so a SimNet
/// provider can install its own virtual accept pair into the same
/// set-once slots.
pub fn init_socket_seams() {
    pqcomm_seams::pq_init::set(pq_init);
    pqcomm_seams::touch_socket_files::set(TouchSocketFiles);
    pqcomm_seams::remove_socket_files::set(RemoveSocketFiles);
    pqcomm_seams::modify_fe_be_wait_set_latch::set(pq_modify_fe_be_wait_set_latch);
    pqcomm_seams::pq_check_connection::set(pq_check_connection);
    be_secure_seams::set_port_noblock::set(set_port_noblock);

    pqcomm_seams::accept_connection::set(|server_fd| {
        let mut cs = types_startup::ClientSocket {
            sock: types_core::PGINVALID_SOCKET,
            raddr: ip::SockAddr::zeroed(),
        };
        if AcceptConnection(server_fd, &mut cs) == types_core::STATUS_OK {
            Ok(cs)
        } else {
            Err(Box::new(types_error::PgError::new(
                types_error::LOG,
                "could not accept new connection",
            )))
        }
    });
    pqcomm_seams::inherit_listen_sockets::set(inherit_listen_sockets);
    pqcomm_seams::adopt_inherited_listen_sockets::set(adopt_inherited_listen_sockets);
    pqcomm_seams::listen_server_port::set(
        |hostname, port, unix_socket_dir, listen_sockets, max_listen| {
            let family =
                if unix_socket_dir.is_some() { ip::sys::AF_UNIX } else { ip::sys::AF_UNSPEC };
            let status = ListenServerPort(
                family,
                hostname,
                port,
                unix_socket_dir,
                listen_sockets,
                max_listen,
            )?;
            if status == types_core::STATUS_OK {
                Ok(())
            } else {
                Err(Box::new(types_error::PgError::new(
                    types_error::WARNING,
                    "could not create listen socket",
                )))
            }
        },
    );

    init_socket_gucs();
}

/// This file's GUC storage installs alone: an alternative transport provider
/// (pqcomm_stdio; P4 sim-net) owns the pq_init/noblock slots but the
/// keepalive/unix-socket GUCs must still exist for guc boot — their assign
/// hooks no-op without a TCP MyProcPort, as on an AF_UNIX connection.
pub fn init_socket_gucs() {
    use guc_tables::{hooks, vars, GucVarAccessors};

    vars::tcp_keepalives_idle.install(GucVarAccessors {
        get: || cfg::TCP_KEEPALIVES_IDLE.get(),
        set: |v| cfg::TCP_KEEPALIVES_IDLE.set(v),
    });
    vars::tcp_keepalives_interval.install(GucVarAccessors {
        get: || cfg::TCP_KEEPALIVES_INTERVAL.get(),
        set: |v| cfg::TCP_KEEPALIVES_INTERVAL.set(v),
    });
    vars::tcp_keepalives_count.install(GucVarAccessors {
        get: || cfg::TCP_KEEPALIVES_COUNT.get(),
        set: |v| cfg::TCP_KEEPALIVES_COUNT.set(v),
    });
    vars::tcp_user_timeout.install(GucVarAccessors {
        get: || cfg::TCP_USER_TIMEOUT.get(),
        set: |v| cfg::TCP_USER_TIMEOUT.set(v),
    });
    vars::Unix_socket_permissions.install(GucVarAccessors {
        get: || cfg::UNIX_SOCKET_PERMISSIONS.get(),
        set: |v| cfg::UNIX_SOCKET_PERMISSIONS.set(v),
    });
    // Boots to "" and GUC string storage never goes back to NULL after.
    vars::Unix_socket_group.install(GucVarAccessors {
        get: || Some(unix_socket_group()),
        set: |v| cfg::UNIX_SOCKET_GROUP.with(|s| *s.borrow_mut() = v.unwrap_or_default()),
    });

    hooks::assign_tcp_keepalives_idle.install(|v, _extra| assign_tcp_keepalives_idle(v));
    hooks::assign_tcp_keepalives_interval.install(|v, _extra| assign_tcp_keepalives_interval(v));
    hooks::assign_tcp_keepalives_count.install(|v, _extra| assign_tcp_keepalives_count(v));
    hooks::assign_tcp_user_timeout.install(|v, _extra| assign_tcp_user_timeout(v));
    hooks::show_tcp_keepalives_idle.install(show_tcp_keepalives_idle);
    hooks::show_tcp_keepalives_interval.install(show_tcp_keepalives_interval);
    hooks::show_tcp_keepalives_count.install(show_tcp_keepalives_count);
    hooks::show_tcp_user_timeout.install(show_tcp_user_timeout);

    vars::client_connection_check_interval.install(GucVarAccessors {
        get: || cfg::CLIENT_CONNECTION_CHECK_INTERVAL.get(),
        set: |v| cfg::CLIENT_CONNECTION_CHECK_INTERVAL.set(v),
    });
    // C's check hook rejects a nonzero interval when the wait-event backend
    // cannot report socket closure (WaitEventSetCanReportClosed). Both
    // native backends here can (epoll EPOLLRDHUP / kqueue EV_EOF), and this
    // hook only installs with the real socket transport — other transports
    // leave the pq_check_connection seam vacant and the interval inert.
    hooks::check_client_connection_check_interval.install(|_newval, _extra, _source| Ok(true));
}

#[cfg(test)]
mod fe_be_mask_tests {
    //! D4484: `pq_modify_fe_be_wait_set_socket` answers only an equal mask itself; every other mask (a blocked
    //! send's WRITEABLE, pq_check_connection's CLOSED, the switch back to READABLE) reaches ModifyWaitEvent, and a
    //! failed modify makes the next one go through.
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering::SeqCst};
    use types_storage::waiteventset::WL_SOCKET_READABLE;

    static CALLS: AtomicUsize = AtomicUsize::new(0);
    static LAST: AtomicU32 = AtomicU32::new(0);
    static FAIL_NEXT: AtomicBool = AtomicBool::new(false);

    fn modify(events: u32) -> (bool, usize) {
        let ok = pq_modify_fe_be_wait_set_socket(events).is_ok();
        (ok, CALLS.load(SeqCst))
    }

    #[test]
    fn only_an_unchanged_mask_skips_modify_wait_event() {
        waiteventset_seams::modify_wait_event::set(|_set, pos, events, latch| {
            assert_eq!(pos, FeBeWaitSetSocketPos);
            assert!(latch.is_none());
            CALLS.fetch_add(1, SeqCst);
            LAST.store(events, SeqCst);
            if FAIL_NEXT.swap(false, SeqCst) {
                return Err(Box::new(types_error::PgError::new(types_error::ERROR, "epoll_ctl() failed".to_string())));
            }
            Ok(())
        });
        // pq_init's state: the socket position added with WL_SOCKET_WRITEABLE
        FE_BE_WAIT_SET.set(Some((WaitEventSetHandle::new(1), WL_SOCKET_WRITEABLE)));
        assert_eq!(modify(WL_SOCKET_READABLE), (true, 1), "the first read wait switches WRITEABLE -> READABLE");
        for _ in 0..3 {
            assert_eq!(modify(WL_SOCKET_READABLE), (true, 1), "a read wait after a read wait changes nothing");
        }
        assert_eq!(modify(WL_SOCKET_WRITEABLE), (true, 2), "a blocked send still switches to WRITEABLE");
        assert_eq!(modify(WL_SOCKET_WRITEABLE), (true, 2));
        assert_eq!(modify(WL_SOCKET_READABLE), (true, 3), "and the next read switches back");
        assert_eq!(modify(WL_SOCKET_CLOSED), (true, 4), "pq_check_connection's CLOSED reaches the set");
        assert_eq!(modify(WL_SOCKET_READABLE), (true, 5));
        assert_eq!(LAST.load(SeqCst), WL_SOCKET_READABLE);
        FAIL_NEXT.store(true, SeqCst);
        assert_eq!(modify(WL_SOCKET_WRITEABLE), (false, 6), "a failed modify is raised");
        assert_eq!(modify(WL_SOCKET_READABLE), (true, 7), "after a failure even the old mask goes through");
        assert_eq!(modify(WL_SOCKET_READABLE), (true, 7));
        FE_BE_WAIT_SET.set(None);
    }
}
