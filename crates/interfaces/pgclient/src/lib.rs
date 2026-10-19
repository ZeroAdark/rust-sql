// In-server postgres-wire CLIENT (fast carries no libpq): conninfo, startup +
// auth (trust, password, md5, SCRAM-SHA-256), simple query (buffered and
// row-streamed), async send/poll, cancel protocol, COPY framing. Consumers:
// walreceiver, dblink; designed for postgres_fdw (docs/design/pgclient-seam.md).
//
// Blocking discipline (invariant): every wait is WaitLatchOrSocket(latch |
// socket) -> ResetLatch -> CHECK_FOR_INTERRUPTS -> nonblocking pump; no
// blocking syscall outside the EAGAIN wait (libpq-be-fe-helpers.h contract).
use std::os::fd::RawFd;

use types_core::{pgsocket, Oid};
use types_error::PgResult;
use types_storage::waiteventset::{
    WL_EXIT_ON_PM_DEATH, WL_LATCH_SET, WL_SOCKET_READABLE, WL_SOCKET_WRITEABLE, WL_TIMEOUT,
};

pub mod conninfo;
mod errmsg;
pub use conninfo::{lookup_option, opt, parse_conninfo, resolve_conninfo, ConnOption, CONNINFO_OPTIONS};

mod auth;
#[cfg(test)]
mod conninfo_uri_tests;

pub const PG_PROTOCOL_3_0: u32 = 3 << 16;
const PG_PROTOCOL_3_2: u32 = (3 << 16) | 2;
// PQ_QUERY_PARAM_MAX_LIMIT (libpq-fe.h).
const PQ_QUERY_PARAM_MAX_LIMIT: usize = 65535;
// wait_classes.h; libpqsrv_cancel's socket wait reports PG_WAIT_CLIENT.
const PG_WAIT_CLIENT: u32 = 0x0600_0000;
const CANCEL_REQUEST_CODE: u32 = (1234 << 16) | 5678;

// Largest message length we accept from the server (the wire length is a
// SIGNED i32 that includes its own 4 bytes). PostgreSQL never sends a
// message anywhere near 2^30 (values are capped at 1GB), so anything
// larger — and anything below the 4-byte self-inclusive minimum, negative
// values included — is framing loss, not a big row.
const MAX_MESSAGE_LEN: i32 = 0x3FFF_FFFF;

#[derive(Clone, Copy)]
pub struct WaitEvents {
    pub connect: u32,
    pub receive: u32,
}

enum Stream {
    Tcp(std::net::TcpStream),
    // wasm32: std::os::unix is absent on wasi (no AF_UNIX on WASI p1); the
    // unix-socket conninfo arm refuses at connect instead.
    #[cfg(not(target_family = "wasm"))]
    Unix(std::os::unix::net::UnixStream),
}

#[derive(Clone)]
enum DialTarget {
    Tcp(String, u16),
    Unix(String),
}

// PQfinish -> closePGconn -> sendTerminateConn (fe-connect.c): a connection
// that reached CONNECTION_OK says goodbye with Terminate, so the server's
// read sees 'X' rather than EOF (walsender.c:2289 logs the latter).
impl Drop for PgConn {
    fn drop(&mut self) {
        if self.conn_ok && !self.terminate_sent {
            let _ = self.send_all(&msg(b'X', &[]));
        }
        if self.external_fd {
            file_seams::release_external_fd::call();
        }
    }
}

// libpq-be-fe-helpers.h:131 libpqsrv_connect_prepare: a PGconn counts as one
// long-lived FD against fd.c's limit; not installed = no backend fd layer.
fn reserve_external_fd() -> PgResult<bool> {
    if !file_seams::acquire_external_fd::is_installed() {
        return Ok(false);
    }
    if file_seams::acquire_external_fd::call()? {
        return Ok(true);
    }
    Err(Box::new(
        types_error::PgError::error("could not establish connection")
            .with_sqlstate(types_error::ERRCODE_SQLCLIENT_UNABLE_TO_ESTABLISH_SQLCONNECTION)
            .with_detail("There are too many open files on the local server.")
            .with_hint("Raise the server's \"max_files_per_process\" and/or \"ulimit -n\" limits."),
    ))
}

pub struct PgConn {
    _stream: Stream,
    fd: RawFd,
    external_fd: bool,
    target: DialTarget,
    inbuf: Vec<u8>,
    inpos: usize,
    conn_ok: bool,
    terminate_sent: bool,
    in_copy: bool,
    copy_server_done: bool,
    copy_client_done: bool,
    // conn->raddr: the address the connection was made to; a cancel request
    // (PQcancelCreate, fe-cancel.c:170) goes there, never through a fresh
    // name lookup.
    peer: Option<std::net::SocketAddr>,
    // ErrorResponse text already appended by is_busy's parse (PQisBusy's
    // parseInput), not to be appended again when the result is collected.
    err_prescanned: bool,
    // A query was sent and ReadyForQuery not yet consumed (libpq asyncStatus
    // != IDLE); get_result on an idle connection returns None immediately.
    pending_results: bool,
    // libpq conn->xactStatus: the status byte of the last ReadyForQuery
    // ('I' idle / 'T' in transaction / 'E' failed transaction).
    txn_status: u8,
    err: String,
    opts: Vec<(String, String)>,
    display_host: String,
    display_port: i32,
    be_pid: i32,
    be_key: Vec<u8>,
    server_version: i32,
    used_password: bool,
    // conn->last_sqlstate: the SQLSTATE of the last ErrorResponse.
    last_sqlstate: String,
    params: Vec<(String, String)>,
    notifies: std::collections::VecDeque<Notify>,
    // cmd_queue_head->query: the text of the command in flight, for the
    // error-cursor display (None for PQsendQueryPrepared).
    last_query: Option<String>,
    we: WaitEvents,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExecStatus {
    CommandOk,
    TuplesOk,
    CopyBoth,
    CopyIn,
    CopyOut,
    Empty,
    Error,
}

// ErrorResponse diag fields (PQresultErrorField's PG_DIAG_* surface).
#[derive(Default, Clone)]
pub struct ErrorFields {
    pub severity: String,
    pub sqlstate: String,
    pub primary: String,
    pub detail: String,
    pub hint: String,
    pub context: String,
    pub position: String,
    pub internal_position: String,
    pub internal_query: String,
}

pub struct QueryResult {
    pub status: ExecStatus,
    pub nfields: usize,
    pub rows: Vec<Vec<Option<Vec<u8>>>>,
    // CommandComplete tag (PQcmdStatus), e.g. "INSERT 0 1".
    pub cmd_tag: String,
    pub diag: Option<ErrorFields>,
    pub err: String,
}

impl QueryResult {
    /// A connection-level failure as a result value (no server diag) —
    /// callers shaping libpq's "PGresult == NULL" arms (pgfdw_report_error
    /// falls back to PQerrorMessage) build these.
    pub fn conn_error(err: String) -> Self {
        Self::error(err)
    }

    fn error(err: String) -> Self {
        QueryResult {
            status: ExecStatus::Error,
            nfields: 0,
            rows: Vec::new(),
            cmd_tag: String::new(),
            diag: None,
            err,
        }
    }

    fn status_only(status: ExecStatus) -> Self {
        QueryResult {
            status,
            nfields: 0,
            rows: Vec::new(),
            cmd_tag: String::new(),
            diag: None,
            err: String::new(),
        }
    }
}

pub struct Notify {
    pub channel: String,
    pub be_pid: i32,
    pub extra: String,
}

pub enum CopyData {
    Msg(Vec<u8>),
    Block,
    End,
}

// Sink for row-streamed execution (libpq single-row mode's shape): one
// result_start per resultset (dblink recreates its tuplestore there), one
// row per DataRow. result_start sees the connection read-only at
// first-row-in-hand — after any ParameterStatus messages, which dblink's
// remote-GUC mirroring depends on. Errors unwind through exec_streaming;
// the caller is expected to drain() the connection on the error path.
pub trait RowSink {
    fn result_start(&mut self, conn: &PgConn, nfields: usize) -> PgResult<()>;
    fn row(&mut self, cols: &[Option<&[u8]>]) -> PgResult<()>;
}

pub(crate) fn msg(t: u8, body: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(5 + body.len());
    m.push(t);
    m.extend_from_slice(&((body.len() as u32 + 4).to_be_bytes()));
    m.extend_from_slice(body);
    m
}

pub(crate) const SERVER_CLOSED_MSG: &str = "server closed the connection unexpectedly\n\
    \tThis probably means the server terminated abnormally\n\
    \tbefore or while processing the request.";

pub(crate) fn be_i32(b: &[u8]) -> i32 {
    i32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

pub(crate) fn cstr_at(b: &[u8], pos: usize) -> (String, usize) {
    // Server-controlled offsets: a body whose previous field ran to the end
    // without a NUL hands us pos == b.len() + 1; clamp instead of slicing
    // out of range.
    let pos = pos.min(b.len());
    let end = b[pos..].iter().position(|&c| c == 0).map(|e| pos + e).unwrap_or(b.len());
    (String::from_utf8_lossy(&b[pos..end]).into_owned(), end + 1)
}

pub(crate) fn parse_diag(body: &[u8]) -> ErrorFields {
    let mut f = ErrorFields { severity: "ERROR".into(), ..Default::default() };
    let mut i = 0;
    while i < body.len() && body[i] != 0 {
        let code = body[i];
        let (val, next) = cstr_at(body, i + 1);
        match code {
            b'S' => f.severity = val,
            b'C' => f.sqlstate = val,
            b'M' => f.primary = val,
            b'D' => f.detail = val,
            b'H' => f.hint = val,
            b'W' => f.context = val,
            b'P' => f.position = val,
            b'p' => f.internal_position = val,
            b'q' => f.internal_query = val,
            _ => {}
        }
        i = next;
    }
    f
}

// ErrorResponse body -> libpq's PQerrorMessage text (pqBuildErrorMessage3,
// trailing newline dropped; callers pchomp anyway). `query` is the command
// in flight for the LINE/caret display. The walreceiver relays the whole
// thing into the local log — 019_replslot_limit greps the standby log for
// the primary's DETAIL line.
pub(crate) fn parse_error_fields(body: &[u8], query: Option<&str>) -> String {
    let mut out = errmsg::build_message(&parse_diag(body), query, true);
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

pub(crate) fn wait_socket(fd: RawFd, events: u32, wait_event_info: u32) -> PgResult<()> {
    loop {
        let rc = latch::WaitLatchOrSocket(
            init_small::globals::MyLatch(),
            WL_LATCH_SET | WL_EXIT_ON_PM_DEATH | events,
            fd,
            -1,
            wait_event_info,
        )?;
        if rc & WL_LATCH_SET != 0 {
            if let Some(l) = init_small::globals::MyLatch() {
                latch::ResetLatch(l);
            }
            postgres_seams::check_for_interrupts::call()?;
        }
        if rc & events != 0 {
            return Ok(());
        }
    }
}

#[cfg(not(target_family = "wasm"))]
pub fn os_user_name() -> String {
    if let Ok(u) = std::env::var("PGUSER") {
        if !u.is_empty() {
            return u;
        }
    }
    // SAFETY: getpwuid returns a static struct; read name under it.
    unsafe {
        let pw = libc::getpwuid(libc::geteuid());
        if !pw.is_null() && !(*pw).pw_name.is_null() {
            return std::ffi::CStr::from_ptr((*pw).pw_name).to_string_lossy().into_owned();
        }
    }
    std::env::var("USER").unwrap_or_default()
}

// wasm32: no uids and no passwd db on WASI; identity comes through the
// environment (wasmtime --env USER=<name>), the main_main synthetic-identity
// arm's convention.
#[cfg(target_family = "wasm")]
pub fn os_user_name() -> String {
    if let Ok(u) = std::env::var("PGUSER") {
        if !u.is_empty() {
            return u;
        }
    }
    std::env::var("USER").unwrap_or_default()
}

pub fn resolve_user(opts: &[(String, String)]) -> String {
    opt(opts, "user")
        .filter(|u| !u.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(os_user_name)
}

pub fn resolve_password(opts: &[(String, String)]) -> Option<String> {
    opt(opts, "password")
        .filter(|p| !p.is_empty())
        .map(|s| s.to_string())
        .or_else(|| std::env::var("PGPASSWORD").ok().filter(|p| !p.is_empty()))
}

// libpq's unix-socket connect leg.
#[cfg(not(target_family = "wasm"))]
#[cfg(not(target_family = "wasm"))]
fn peer_uid(sock: RawFd) -> Result<libc::uid_t, String> {
    #[cfg(target_os = "linux")]
    {
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: getsockopt writes at most `len` bytes into `cred`.
        let rc = unsafe {
            libc::getsockopt(
                sock,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                std::ptr::addr_of_mut!(cred).cast(),
                &mut len,
            )
        };
        if rc != 0 {
            return Err(format!(
                "could not get peer credentials: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(cred.uid)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let (mut uid, mut gid) = (0, 0);
        // SAFETY: valid socket fd and out-params.
        if unsafe { libc::getpeereid(sock, &mut uid, &mut gid) } != 0 {
            return Err(format!(
                "could not get peer credentials: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(uid)
    }
}

#[cfg(not(target_family = "wasm"))]
fn check_requirepeer(sock: RawFd, want: &str) -> Result<(), String> {
    let uid = peer_uid(sock)?;
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0u8; 16 * 1024];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: getpwuid_r fills `pw` and strings into `buf` of the given size.
    let rc = unsafe {
        libc::getpwuid_r(uid, &mut pw, buf.as_mut_ptr().cast(), buf.len(), &mut result)
    };
    if rc != 0 || result.is_null() {
        return Err(format!("could not look up local user ID {uid}: user does not exist"));
    }
    // SAFETY: pw_name points into `buf`, NUL-terminated by getpwuid_r.
    let name = unsafe { std::ffi::CStr::from_ptr(pw.pw_name) }.to_string_lossy();
    if name != want {
        return Err(format!(
            "requirepeer specifies \"{want}\", but actual peer user name is \"{name}\""
        ));
    }
    Ok(())
}

#[cfg(target_family = "wasm")]
fn check_requirepeer(_sock: RawFd, _want: &str) -> Result<(), String> {
    Err("requirepeer is not supported on this platform".into())
}

fn is_unixsock_path(path: &str) -> bool {
    path.starts_with('/') || path.starts_with('@')
}

// getaddrinfo_unix (ip.c:215) turns a leading '@' into an abstract-namespace
// address; std's UnixStream::connect only knows filesystem paths.
#[cfg(not(target_family = "wasm"))]
fn unix_stream_connect(path: &str) -> Result<std::os::unix::net::UnixStream, String> {
    use std::os::fd::FromRawFd;
    if !path.starts_with('@') {
        return std::os::unix::net::UnixStream::connect(path).map_err(|e| match e.raw_os_error() {
            Some(errno) => connect_failure_message(errno, true),
            None => e.to_string(),
        });
    }
    let hint = ip::AddrInfoHint { flags: 0, family: libc::AF_UNIX, socktype: libc::SOCK_STREAM };
    let mut addrs = Vec::new();
    let rc = ip::pg_getaddrinfo_all(None, Some(path), &hint, &mut addrs);
    let Some(ai) = addrs.first().filter(|_| rc == 0) else {
        // SAFETY: gai_strerror returns a static NUL-terminated string.
        let why = unsafe { std::ffi::CStr::from_ptr(ip::sys::gai_strerror(rc)) }.to_string_lossy();
        return Err(format!("could not translate Unix-domain socket path \"{path}\" to address: {why}"));
    };
    // SAFETY: socket syscalls on a descriptor this function owns until it is
    // wrapped or closed.
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return Err(strerror(last_errno()));
        }
        if libc::connect(fd, ai.addr.addr.as_ptr().cast::<libc::sockaddr>(), ai.addr.salen as libc::socklen_t) < 0 {
            let e = last_errno();
            libc::close(fd);
            return Err(connect_failure_message(e, true));
        }
        Ok(std::os::unix::net::UnixStream::from_raw_fd(fd))
    }
}

#[cfg(not(target_family = "wasm"))]
fn unix_connect(path: &str) -> Result<(Stream, RawFd), String> {
    use std::os::fd::AsRawFd;
    match unix_stream_connect(path) {
        Ok(s) => {
            let fd = s.as_raw_fd();
            Ok((Stream::Unix(s), fd))
        }
        Err(e) => Err(format!("connection to server on socket \"{path}\" failed: {e}")),
    }
}

#[cfg(not(target_family = "wasm"))]
fn strerror(errno: i32) -> String {
    // SAFETY: strerror returns a NUL-terminated string valid until the next
    // call on this thread; it is copied out at once.
    unsafe { std::ffi::CStr::from_ptr(libc::strerror(errno)) }.to_string_lossy().into_owned()
}

#[cfg(target_family = "wasm")]
fn strerror(errno: i32) -> String {
    std::io::Error::from_raw_os_error(errno).to_string()
}

// connectFailureMessage (fe-connect.c:2681): the OS error and libpq's hint.
fn connect_failure_message(errno: i32, unix: bool) -> String {
    let hint = if unix {
        "Is the server running locally and accepting connections on that socket?"
    } else {
        "Is the server running on that host and accepting TCP/IP connections?"
    };
    format!("{}\n\t{hint}", strerror(errno))
}

// getHostaddr (fe-connect.c:2386): pg_inet_net_ntop over the address bytes,
// so a scoped IPv6 address prints without its %scope suffix.
#[cfg(not(target_family = "wasm"))]
fn host_addr_text(addr: &ip::SockAddr) -> String {
    match ip::sockaddr_family(addr) {
        libc::AF_INET => {
            // SAFETY: the buffer is sockaddr_storage-sized; a family of AF_INET
            // means it holds a sockaddr_in.
            let sin: libc::sockaddr_in = unsafe { std::ptr::read_unaligned(addr.addr.as_ptr().cast()) };
            std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)).to_string()
        }
        libc::AF_INET6 => {
            // SAFETY: as above, for sockaddr_in6.
            let sin6: libc::sockaddr_in6 = unsafe { std::ptr::read_unaligned(addr.addr.as_ptr().cast()) };
            std::net::Ipv6Addr::from(sin6.sin6_addr.s6_addr).to_string()
        }
        _ => String::new(),
    }
}

#[cfg(not(target_family = "wasm"))]
fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

// PQconnectPoll's CONNECTION_NEEDED / CONNECTION_STARTED arms (fe-connect.c
// :3055, :3480) as libpqsrv_connect_params drives them (libpq-be-fe-helpers.h
// :193): every resolved address gets a nonblocking connect() whose
// EINPROGRESS wait rides WaitLatchOrSocket, so a cancel or shutdown ends a
// black-holed SYN instead of the OS connect timeout. Each failed address
// appends its own host-identified message.
#[cfg(not(target_family = "wasm"))]
fn tcp_dial(
    ch: &ConnHost,
    port: u16,
    prng: Option<&mut pg_prng::PgPrng>,
    wait_event: u32,
) -> PgResult<Result<(Stream, RawFd, DialTarget), String>> {
    use std::os::fd::FromRawFd;
    let numeric = !ch.hostaddr.is_empty();
    let name = if numeric { ch.hostaddr.as_str() } else { ch.host.as_str() };
    let hint = ip::AddrInfoHint {
        flags: if numeric { ip::sys::AI_NUMERICHOST } else { 0 },
        family: ip::sys::AF_UNSPEC,
        socktype: ip::sys::SOCK_STREAM,
    };
    let mut addrs = Vec::new();
    let rc = ip::pg_getaddrinfo_all(Some(name), Some(&port.to_string()), &hint, &mut addrs);
    if rc != 0 || addrs.is_empty() {
        // SAFETY: gai_strerror returns a static NUL-terminated string.
        let why = unsafe { std::ffi::CStr::from_ptr(ip::sys::gai_strerror(rc)) }.to_string_lossy();
        return Ok(Err(if numeric {
            format!("could not parse network address \"{name}\": {why}")
        } else {
            format!("could not translate host name \"{name}\" to address: {why}")
        }));
    }
    if let Some(p) = prng {
        shuffle(&mut addrs, p);
    }
    let mut errs = String::new();
    for ai in &addrs {
        let host_addr = host_addr_text(&ai.addr);
        // emitHostIdentityInfo (fe-connect.c:2620).
        let identity = if numeric || host_addr == name {
            format!("connection to server at \"{name}\", port {port} failed: ")
        } else {
            format!("connection to server at \"{name}\" ({host_addr}), port {port} failed: ")
        };
        match tcp_connect_addr(ai, wait_event)? {
            Ok(fd) => {
                // SAFETY: a connected socket this function owns.
                let s = unsafe { std::net::TcpStream::from_raw_fd(fd) };
                let _ = s.set_nodelay(true);
                return Ok(Ok((Stream::Tcp(s), fd, DialTarget::Tcp(name.to_string(), port))));
            }
            Err(e) => append_conn_error(&mut errs, &format!("{identity}{e}")),
        }
    }
    Ok(Err(errs))
}

// A bare client context (no backend latch, e.g. the crate's own tests) has
// no interrupts to serve: a plain poll stands in for WaitLatchOrSocket.
#[cfg(not(target_family = "wasm"))]
fn wait_writable(fd: RawFd, wait_event: u32) -> PgResult<()> {
    if init_small::globals::MyLatch().is_some() {
        return wait_socket(fd, WL_SOCKET_WRITEABLE, wait_event);
    }
    let mut pfd = libc::pollfd { fd, events: libc::POLLOUT, revents: 0 };
    // SAFETY: pfd is a valid pollfd for the duration of the call.
    while unsafe { libc::poll(&mut pfd, 1, -1) } < 0 && last_errno() == libc::EINTR {}
    Ok(())
}

#[cfg(not(target_family = "wasm"))]
fn tcp_connect_addr(ai: &ip::PgAddrInfo, wait_event: u32) -> PgResult<Result<RawFd, String>> {
    // SAFETY: socket syscalls on a descriptor this function owns until it is
    // returned or closed on the failure paths.
    unsafe {
        let fd = libc::socket(ai.family, libc::SOCK_STREAM, ai.protocol);
        if fd < 0 {
            return Ok(Err(format!("could not create socket: {}", strerror(last_errno()))));
        }
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            let e = last_errno();
            libc::close(fd);
            return Ok(Err(format!("could not set socket to nonblocking mode: {}", strerror(e))));
        }
        if libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) < 0 {
            let e = last_errno();
            libc::close(fd);
            return Ok(Err(format!("could not set socket to close-on-exec mode: {}", strerror(e))));
        }
        let rc = libc::connect(
            fd,
            ai.addr.addr.as_ptr() as *const libc::sockaddr,
            ai.addr.salen as libc::socklen_t,
        );
        if rc < 0 {
            let e = last_errno();
            if e != libc::EINPROGRESS && e != libc::EWOULDBLOCK && e != libc::EINTR {
                libc::close(fd);
                return Ok(Err(connect_failure_message(e, false)));
            }
            if let Err(pe) = wait_writable(fd, wait_event) {
                libc::close(fd);
                return Err(pe);
            }
            let mut optval: libc::c_int = 0;
            let mut optlen = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            if libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                &mut optval as *mut libc::c_int as *mut libc::c_void,
                &mut optlen,
            ) < 0
            {
                let e = last_errno();
                libc::close(fd);
                return Ok(Err(format!("could not get socket error status: {}", strerror(e))));
            }
            if optval != 0 {
                libc::close(fd);
                return Ok(Err(connect_failure_message(optval, false)));
            }
        }
        Ok(Ok(fd))
    }
}

// wasm32: no AF_UNIX on WASI p1 — refuse with the C error shape
// (52 = WASI ENOSYS), matching the tablespace wasm arm.
#[cfg(target_family = "wasm")]
fn unix_connect(path: &str) -> Result<(Stream, RawFd), String> {
    Err(format!(
        "connection to server on socket \"{path}\" failed: {}",
        std::io::Error::from_raw_os_error(52)
    ))
}

pub fn validate_port(opts: &[(String, String)]) -> Result<u16, String> {
    let hosts = conn_hosts(opts)?;
    parse_port(&hosts[0].port)
}

// PQconnectPoll's try-next-host arm (fe-connect.c): the port option is
// validated BEFORE address resolution or any socket attempt. Regress
// relies on this ordering — CREATE SUBSCRIPTION ... 'port=-1' must fail
// "invalid port number: \"-1\"" WITHOUT a connection ever being tried.
// pqSaveParameterStatus (fe-exec.c:1158): sscanf("%d.%d.%d") on the
// server_version text.
fn parse_server_version(value: &str) -> i32 {
    let mut parts = [0i32; 3];
    let mut cnt = 0;
    let mut rest = value;
    while cnt < 3 {
        let t = if cnt == 0 {
            rest.trim_start_matches(|c: char| c.is_ascii() && pg_string::isspace_c_locale(c as u8))
        } else {
            match rest.strip_prefix('.') {
                Some(r) => r,
                None => break,
            }
        };
        let (neg, digits) = match t.strip_prefix('-') {
            Some(r) => (true, r),
            None => (false, t.strip_prefix('+').unwrap_or(t)),
        };
        let end = digits.bytes().position(|b| !b.is_ascii_digit()).unwrap_or(digits.len());
        if end == 0 {
            break;
        }
        let v: i64 = digits[..end].parse().unwrap_or(0);
        parts[cnt] = (if neg { -v } else { v }) as i32;
        cnt += 1;
        rest = &digits[end..];
    }
    let (vmaj, vmin, vrev) = (parts[0], parts[1], parts[2]);
    match cnt {
        3 => (100 * vmaj + vmin) * 100 + vrev,
        2 if vmaj >= 10 => 100 * 100 * vmaj + vmin,
        2 => (100 * vmaj + vmin) * 100,
        1 => 100 * 100 * vmaj,
        _ => 0,
    }
}

// pqParseIntParam (fe-connect.c:8196): strtol with nothing but whitespace
// allowed around the number.
fn parse_int_param(value: &str, context: &str) -> Result<i32, String> {
    let err = || format!("invalid integer value \"{value}\" for connection option \"{context}\"");
    let sp = |c: char| c.is_ascii() && pg_string::isspace_c_locale(c as u8);
    let t = value.trim_start_matches(sp);
    let (neg, digits) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let end = digits.bytes().position(|b| !b.is_ascii_digit()).unwrap_or(digits.len());
    if end == 0 || !digits[end..].trim_start_matches(sp).is_empty() {
        return Err(err());
    }
    let v: i64 = digits[..end].parse().map_err(|_| err())?;
    let v = if neg { -v } else { v };
    i32::try_from(v).map_err(|_| err())
}

// pqParseProtocolVersion (fe-connect.c:8241).
fn parse_protocol_version(value: &str, context: &str) -> Result<u32, String> {
    match value {
        "latest" | "3.2" => Ok(PG_PROTOCOL_3_2),
        "3.0" => Ok(PG_PROTOCOL_3_0),
        _ => Err(format!("invalid {context} value: \"{value}\"")),
    }
}

// pqConnectOptions2 (fe-connect.c:2107): both bounds must parse and the
// range must not be inverted. The startup packet still asks for 3.0.
fn check_protocol_versions(opts: &[(String, String)]) -> Result<(), String> {
    let min = match opt(opts, "min_protocol_version").filter(|v| !v.is_empty()) {
        Some(v) => parse_protocol_version(v, "min_protocol_version")?,
        None => PG_PROTOCOL_3_0,
    };
    let max = match opt(opts, "max_protocol_version").filter(|v| !v.is_empty()) {
        Some(v) => parse_protocol_version(v, "max_protocol_version")?,
        None if min > PG_PROTOCOL_3_0 => PG_PROTOCOL_3_2,
        None => PG_PROTOCOL_3_0,
    };
    if min > max {
        return Err("\"min_protocol_version\" is greater than \"max_protocol_version\"".into());
    }
    Ok(())
}

// PQconnectPoll's TCP socket setup (fe-connect.c:3394-3428): SO_KEEPALIVE
// unless keepalives=0, then the timer options and tcp_user_timeout; every
// value is validated even where the platform has no such option.
#[cfg(not(target_family = "wasm"))]
fn setup_tcp_socket(fd: RawFd, opts: &[(String, String)]) -> Result<(), String> {
    fn setopt(fd: RawFd, level: i32, name: i32, name_str: &str, val: i32) -> Result<(), String> {
        // SAFETY: fd is an open socket; val outlives the call.
        let rc = unsafe {
            libc::setsockopt(
                fd,
                level,
                name,
                (&val as *const i32).cast(),
                std::mem::size_of::<i32>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(format!(
                "setsockopt({name_str}) failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }
    let int_opt = |key: &str| -> Result<Option<i32>, String> {
        match opt(opts, key) {
            Some(v) => Ok(Some(parse_int_param(v, key)?.max(0))),
            None => Ok(None),
        }
    };
    let use_keepalives = match opt(opts, "keepalives") {
        Some(v) => parse_int_param(v, "keepalives")? != 0,
        None => true,
    };
    if use_keepalives {
        setopt(fd, libc::SOL_SOCKET, libc::SO_KEEPALIVE, "SO_KEEPALIVE", 1)?;
        if let Some(idle) = int_opt("keepalives_idle")? {
            #[cfg(target_os = "linux")]
            setopt(fd, libc::IPPROTO_TCP, libc::TCP_KEEPIDLE, "TCP_KEEPIDLE", idle)?;
            #[cfg(target_os = "macos")]
            setopt(fd, libc::IPPROTO_TCP, libc::TCP_KEEPALIVE, "TCP_KEEPALIVE", idle)?;
        }
        if let Some(interval) = int_opt("keepalives_interval")? {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            setopt(fd, libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, "TCP_KEEPINTVL", interval)?;
        }
        if let Some(count) = int_opt("keepalives_count")? {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            setopt(fd, libc::IPPROTO_TCP, libc::TCP_KEEPCNT, "TCP_KEEPCNT", count)?;
        }
    }
    if let Some(timeout) = int_opt("tcp_user_timeout")? {
        #[cfg(target_os = "linux")]
        setopt(fd, libc::IPPROTO_TCP, libc::TCP_USER_TIMEOUT, "TCP_USER_TIMEOUT", timeout)?;
        let _ = timeout;
    }
    Ok(())
}

#[cfg(target_family = "wasm")]
fn setup_tcp_socket(_fd: RawFd, _opts: &[(String, String)]) -> Result<(), String> {
    Ok(())
}

// PQhost (fe-connect.c:7516): the host as given, else hostaddr, else the
// default socket directory pqConnectOptions2 filled in.
fn pq_host(ch: &ConnHost) -> String {
    if !ch.host.is_empty() {
        ch.host.clone()
    } else if !ch.hostaddr.is_empty() {
        ch.hostaddr.clone()
    } else {
        conninfo::DEFAULT_PGSOCKET_DIR.to_string()
    }
}

fn parse_port(port_s: &str) -> Result<u16, String> {
    if port_s.is_empty() {
        return Ok(5432); // DEF_PGPORT
    }
    // pqParseIntParam: strtol with surrounding whitespace allowed (C-locale
    // isspace, VT included), no trailing garbage, overflow-checked into int.
    match port_s
        .trim_matches(|c: char| c.is_ascii() && pg_string::isspace_c_locale(c as u8))
        .parse::<i32>()
    {
        Err(_) => Err(format!(
            "invalid integer value \"{port_s}\" for connection option \"port\""
        )),
        Ok(n) if !(1..=65535).contains(&n) => Err(format!("invalid port number: \"{port_s}\"")),
        Ok(n) => Ok(n as u16),
    }
}

#[derive(Debug)]
struct ConnHost {
    host: String,
    hostaddr: String,
    port: String,
}

// pqConnectOptions2 (fe-connect.c:1247): one slot per element of the
// hostaddr list (else the host list); a single port is broadcast, otherwise
// the counts must match.
fn conn_hosts(opts: &[(String, String)]) -> Result<Vec<ConnHost>, String> {
    let host = opt(opts, "host").unwrap_or("");
    let hostaddr = opt(opts, "hostaddr").unwrap_or("");
    let port = opt(opts, "port").unwrap_or("");
    let n = if !hostaddr.is_empty() {
        hostaddr.split(',').count()
    } else if !host.is_empty() {
        host.split(',').count()
    } else {
        1
    };
    let mut hosts: Vec<ConnHost> = (0..n)
        .map(|_| ConnHost { host: String::new(), hostaddr: String::new(), port: String::new() })
        .collect();
    if !hostaddr.is_empty() {
        for (h, a) in hosts.iter_mut().zip(hostaddr.split(',')) {
            h.hostaddr = a.to_string();
        }
    }
    if !host.is_empty() {
        let names: Vec<&str> = host.split(',').collect();
        if names.len() != n {
            return Err(format!(
                "could not match {} host names to {} hostaddr values",
                names.len(),
                n
            ));
        }
        for (h, name) in hosts.iter_mut().zip(names) {
            h.host = name.to_string();
        }
    }
    if !port.is_empty() {
        let ports: Vec<&str> = port.split(',').collect();
        if ports.len() == 1 {
            for h in &mut hosts {
                h.port = port.to_string();
            }
        } else if ports.len() != n {
            return Err(format!("could not match {} port numbers to {} hosts", ports.len(), n));
        } else {
            for (h, p) in hosts.iter_mut().zip(ports) {
                h.port = p.to_string();
            }
        }
    }
    Ok(hosts)
}

// pqParseInput3 (fe-protocol3.c:220): a RowDescription makes the result
// PGRES_TUPLES_OK whatever its column count (a zero-column SELECT included).
fn tuples_status(saw_row_description: bool) -> ExecStatus {
    if saw_row_description { ExecStatus::TuplesOk } else { ExecStatus::CommandOk }
}

// libpq_append_conn_error: every attempt's message stays in the buffer.
fn append_conn_error(errs: &mut String, msg: &str) {
    if !errs.is_empty() {
        errs.push('\n');
    }
    errs.push_str(msg);
}

// One connhost's socket. Err((message, terminal)): a terminal failure
// (requirepeer, C's error_return) ends the host walk.
// pqConnectOptions2 (fe-connect.c:1323): a nonempty hostaddr is dialed
// as such whatever host says; else host, a Unix-socket directory when it
// starts with '/'; else DEFAULT_PGSOCKET_DIR.
fn dial(
    ch: &ConnHost,
    port: u16,
    opts: &[(String, String)],
    prng: Option<&mut pg_prng::PgPrng>,
    wait_event: u32,
) -> PgResult<Result<(Stream, RawFd, DialTarget), (String, bool)>> {
    let host = if ch.host.is_empty() && ch.hostaddr.is_empty() {
        conninfo::DEFAULT_PGSOCKET_DIR
    } else {
        ch.host.as_str()
    };
    if ch.hostaddr.is_empty() && is_unixsock_path(host) {
        let path = format!("{host}/.s.PGSQL.{port}");
        let (s, fd) = match unix_connect(&path) {
            Ok(d) => d,
            Err(e) => return Ok(Err((e, false))),
        };
        // fe-connect.c PQconnectPoll: requirepeer verifies the socket peer's
        // OS identity before the startup packet (and any credential) is
        // sent — the only server authentication available on a Unix socket
        // in a world-writable directory.
        if let Some(want) = opt(opts, "requirepeer").filter(|s| !s.is_empty()) {
            if let Err(e) = check_requirepeer(fd, want) {
                return Ok(Err((e, true)));
            }
        }
        return Ok(Ok((s, fd, DialTarget::Unix(path))));
    }
    #[cfg(not(target_family = "wasm"))]
    {
        Ok(tcp_dial(ch, port, prng, wait_event)?.map_err(|e| (e, false)))
    }
    #[cfg(target_family = "wasm")]
    {
        let _ = (wait_event, prng);
        let target = if !ch.hostaddr.is_empty() { ch.hostaddr.clone() } else { host.to_string() };
        match std::net::TcpStream::connect((target.as_str(), port)) {
            Ok(s) => {
                use std::os::fd::AsRawFd;
                let _ = s.set_nodelay(true);
                let fd = s.as_raw_fd();
                Ok(Ok((Stream::Tcp(s), fd, DialTarget::Tcp(target, port))))
            }
            Err(e) => Ok(Err((
                format!("connection to server at \"{target}\", port {port} failed: {e}"),
                false,
            ))),
        }
    }
}

// The cancel connection's dial under libpqsrv_cancel's endtime: each
// resolved address gets the time left; nothing blocks past the deadline.
#[cfg(test)]
fn tcp_connect_deadline(
    host: &str,
    port: u16,
    deadline_ns: u64,
) -> Result<std::net::TcpStream, String> {
    tcp_connect_addrs_deadline(host, port, cancel_addrs(host, port, None)?, deadline_ns)
}

fn cancel_addrs(
    host: &str,
    port: u16,
    peer: Option<std::net::SocketAddr>,
) -> Result<Vec<std::net::SocketAddr>, String> {
    use std::net::ToSocketAddrs;
    if let Some(a) = peer {
        return Ok(vec![a]);
    }
    match (host, port).to_socket_addrs() {
        Ok(a) => Ok(a.collect()),
        Err(e) => Err(format!("could not translate host name \"{host}\" to address: {e}")),
    }
}

fn tcp_connect_addrs_deadline(
    host: &str,
    port: u16,
    addrs: Vec<std::net::SocketAddr>,
    deadline_ns: u64,
) -> Result<std::net::TcpStream, String> {
    let mut last = format!("could not translate host name \"{host}\" to address: no address");
    for addr in addrs {
        let left = deadline_ns.saturating_sub(pg_clock::mono_ns());
        if left == 0 {
            return Err("cancel request timed out".into());
        }
        match std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_nanos(left)) {
            Ok(s) => return Ok(s),
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                return Err("cancel request timed out".into())
            }
            Err(e) => {
                last = format!("connection to server at \"{host}\", port {port} failed: {e}")
            }
        }
    }
    Err(last)
}

// Dial + startup + auth, stopping at ReadyForQuery. `startup_params` is the
// full parameter list for the startup packet (caller decides user/database/
// replication/...). Ok(Err(msg)) is libpq's CONNECTION_BAD-with-errorMessage
// shape; the caller wraps it into its own ereport.
// Enforce libpq's sslmode contract for a client with no TLS support. Mirrors
// fe-connect.c connectOptions2's #ifndef USE_SSL arm: an unknown sslmode is an
// "invalid sslmode value" error, and require/verify-ca/verify-full are rejected
// with "sslmode value \"...\" invalid when SSL support is not compiled in".
// disable/allow/prefer are accepted and proceed without encryption. An absent
// sslmode is treated as the compiled default ("prefer"), which does not require
// encryption. Refusing here is what prevents a silent cleartext downgrade of a
// caller that asked for a TLS-required connection.
fn check_sslmode(opts: &[(String, String)]) -> Result<(), String> {
    match opt(opts, "sslmode") {
        None | Some("disable") | Some("allow") | Some("prefer") => Ok(()),
        Some(m @ ("require" | "verify-ca" | "verify-full")) => Err(format!(
            "sslmode value \"{m}\" invalid when SSL support is not compiled in"
        )),
        Some(other) => Err(format!("invalid sslmode value: \"{other}\"")),
    }
}

// fe-connect.c connectOptions2 "validate channel_binding option": returns
// whether channel binding is required. Enforcement happens in the auth
// handshake, where libpq refuses every non-channel-bound exchange.
fn check_channel_binding(opts: &[(String, String)]) -> Result<bool, String> {
    match opt(opts, "channel_binding") {
        None | Some("disable") | Some("prefer") => Ok(false),
        Some("require") => Ok(true),
        Some(other) => Err(format!("invalid channel_binding value: \"{other}\"")),
    }
}

// The #ifndef USE_SSL arms of connectOptions2 that precede sslmode:
// sslrootcert=system changes the default sslmode, so it is refused first.
fn check_sslrootcert(opts: &[(String, String)]) -> Result<(), String> {
    match opt(opts, "sslrootcert") {
        Some("system") => {
            Err("sslrootcert value \"system\" invalid when SSL support is not compiled in".into())
        }
        _ => Ok(()),
    }
}

fn check_sslnegotiation(opts: &[(String, String)]) -> Result<(), String> {
    match opt(opts, "sslnegotiation") {
        None | Some("postgres") => Ok(()),
        Some("direct") => Err(
            "sslnegotiation value \"direct\" invalid when SSL support is not compiled in".into(),
        ),
        Some(other) => Err(format!("invalid sslnegotiation value: \"{other}\"")),
    }
}

// sslVerifyProtocolVersion / sslVerifyProtocolRange (fe-connect.c:7620): the
// bounds are checked on their text, independently of any TLS implementation.
fn ssl_protocol_version_ok(v: &str) -> bool {
    v.is_empty() || ["TLSv1", "TLSv1.1", "TLSv1.2", "TLSv1.3"].iter().any(|k| k.eq_ignore_ascii_case(v))
}

fn ssl_protocol_range_ok(min: &str, max: &str) -> bool {
    if min.is_empty() || max.is_empty() || min.eq_ignore_ascii_case("TLSv1") {
        return true;
    }
    if max.eq_ignore_ascii_case("TLSv1") {
        return false;
    }
    min.to_ascii_lowercase() <= max.to_ascii_lowercase()
}

fn check_ssl_protocol_versions(opts: &[(String, String)]) -> Result<(), String> {
    let min = opt(opts, "ssl_min_protocol_version").unwrap_or("");
    let max = opt(opts, "ssl_max_protocol_version").unwrap_or("");
    for (name, v) in [("ssl_min_protocol_version", min), ("ssl_max_protocol_version", max)] {
        if !ssl_protocol_version_ok(v) {
            return Err(format!("invalid \"{name}\" value: \"{v}\""));
        }
    }
    if !ssl_protocol_range_ok(min, max) {
        return Err("invalid SSL protocol version range".into());
    }
    Ok(())
}

fn check_sslcertmode(opts: &[(String, String)]) -> Result<(), String> {
    match opt(opts, "sslcertmode") {
        None | Some("disable") | Some("allow") => Ok(()),
        Some("require") => {
            Err("sslcertmode value \"require\" invalid when SSL support is not compiled in".into())
        }
        Some(other) => Err(format!("invalid sslcertmode value: \"{other}\"")),
    }
}

// The #ifndef ENABLE_GSS arm: this client has no GSSAPI transport either.
fn check_gssencmode(opts: &[(String, String)]) -> Result<(), String> {
    match opt(opts, "gssencmode") {
        None | Some("disable") | Some("prefer") => Ok(()),
        Some("require") => Err(
            "gssencmode value \"require\" invalid when GSSAPI support is not compiled in".into(),
        ),
        Some(other) => Err(format!("invalid gssencmode value: \"{other}\"")),
    }
}

// connectOptions2's transport-security validation in libpq's order
// (channel_binding, sslrootcert, sslmode, sslnegotiation, sslcertmode,
// gssencmode). Every option this client cannot honour is a hard error before
// any socket is opened; silently proceeding in cleartext is the downgrade
// libpq exists to prevent.
fn check_transport_options(opts: &[(String, String)]) -> Result<bool, String> {
    let channel_binding_require = check_channel_binding(opts)?;
    check_sslrootcert(opts)?;
    check_sslmode(opts)?;
    check_sslnegotiation(opts)?;
    check_ssl_protocol_versions(opts)?;
    check_sslcertmode(opts)?;
    check_gssencmode(opts)?;
    Ok(channel_binding_require)
}

// pqConnectOptions2 (fe-connect.c:2065): load_balance_hosts is validated and,
// for random, the connhost list is shuffled; PQconnectPoll (:3116) shuffles
// each host's resolved addresses with the same generator.
fn load_balance_random(opts: &[(String, String)]) -> Result<bool, String> {
    match opt(opts, "load_balance_hosts") {
        None | Some("disable") => Ok(false),
        Some("random") => Ok(true),
        Some(other) => Err(format!("invalid load_balance_hosts value: \"{other}\"")),
    }
}

// libpq_prng_init (fe-connect.c:1169): strong random bits, else a time/pid seed.
fn libpq_prng_init() -> pg_prng::PgPrng {
    let mut seed = [0u8; 16];
    if pg_strong_random::pg_strong_random(&mut seed) {
        let s0 = u64::from_ne_bytes(seed[..8].try_into().unwrap());
        let s1 = u64::from_ne_bytes(seed[8..].try_into().unwrap());
        let mut prng = pg_prng::PgPrng::from_raw(s0, s1);
        prng.ensure_seeded();
        return prng;
    }
    pg_prng::PgPrng::seeded(pg_clock::mono_ns() ^ u64::from(std::process::id()))
}

// The inside-out Fisher-Yates walk libpq uses for both lists.
fn shuffle<T>(items: &mut [T], prng: &mut pg_prng::PgPrng) {
    for i in 1..items.len() {
        let j = prng.u64_range(0, i as u64) as usize;
        items.swap(i, j);
    }
}

pub fn connect(
    opts: Vec<(String, String)>,
    startup_params: &[(&str, &str)],
    we: WaitEvents,
) -> PgResult<Result<PgConn, String>> {
    let reserved = reserve_external_fd()?;
    match connect_unreserved(opts, startup_params, we) {
        Ok(Ok(mut conn)) => {
            conn.external_fd = reserved;
            Ok(Ok(conn))
        }
        other => {
            if reserved {
                file_seams::release_external_fd::call();
            }
            other
        }
    }
}

fn connect_unreserved(
    opts: Vec<(String, String)>,
    startup_params: &[(&str, &str)],
    we: WaitEvents,
) -> PgResult<Result<PgConn, String>> {
    // libpq connectOptions2 (fe-connect.c): sslmode is validated BEFORE any
    // socket is opened. This client carries no TLS implementation, so — exactly
    // like a libpq built without USE_SSL — the SSL-requiring modes MUST be a
    // hard error rather than a silent downgrade to a cleartext connection.
    // disable/allow/prefer never require encryption and proceed in plaintext
    // (allow/prefer would merely *attempt* SSL first, which we cannot, so they
    // fall through to plaintext just as no-SSL libpq does). require/verify-ca/
    // verify-full demand encryption and cannot be satisfied, so we refuse.
    // connectOptions2 validates require_auth (fe-connect.c:1469) before
    // sslmode, and decodes the SCRAM pass-through keys (:2018) after it —
    // all before any socket is opened, so none of these carry a host
    // identity prefix.
    let policy = match auth::AuthPolicy::parse(opt(&opts, "require_auth")) {
        Ok(p) => p,
        Err(e) => return Ok(Err(e)),
    };
    let policy = match check_transport_options(&opts) {
        Ok(cb) => policy.with_channel_binding_required(cb),
        Err(e) => return Ok(Err(e)),
    };
    let mut server_type = match TargetServerType::parse(opt(&opts, "target_session_attrs")) {
        Ok(t) => t,
        Err(e) => return Ok(Err(e)),
    };
    let keys = match auth::ScramKeys::parse(
        opt(&opts, "scram_client_key"),
        opt(&opts, "scram_server_key"),
    ) {
        Ok(k) => k,
        Err(e) => return Ok(Err(e)),
    };
    let mut hosts = match conn_hosts(&opts) {
        Ok(h) => h,
        Err(e) => return Ok(Err(e)),
    };
    let mut prng = match load_balance_random(&opts) {
        Ok(true) => Some(libpq_prng_init()),
        Ok(false) => None,
        Err(e) => return Ok(Err(e)),
    };
    if let Some(p) = prng.as_mut() {
        shuffle(&mut hosts, p);
    }
    if let Err(e) = check_protocol_versions(&opts) {
        return Ok(Err(e));
    }
    // md5/SCRAM hash the user the server was told about — the startup
    // packet's, not a re-derivation from the options.
    let user = startup_params
        .iter()
        .find(|(k, _)| *k == "user")
        .map(|(_, v)| v.to_string())
        .unwrap_or_else(|| resolve_user(&opts));
    let explicit_password = resolve_password(&opts);
    // pqConnectOptions2: with no password given the file is consulted per
    // host, keyed by the database the startup packet names (physical
    // replication says "replication").
    let pw_dbname = startup_params
        .iter()
        .find(|(k, _)| *k == "database")
        .map(|(_, v)| v.to_string())
        .or_else(|| opt(&opts, "dbname").filter(|s| !s.is_empty()).map(|s| s.to_string()))
        .unwrap_or_else(|| user.clone());
    // pqConnectOptions2 (fe-connect.c:1429): the password file path is
    // always resolved into the options (PQconninfo shows it).
    let mut opts = opts;
    if opt(&opts, "passfile").is_none_or(str::is_empty) {
        if let Some(f) = conninfo::passfile_path(&opts) {
            opts.retain(|(k, _)| k != "passfile");
            opts.push(("passfile".to_string(), f));
        }
    }
    let passfile = if explicit_password.is_none() { conninfo::passfile_path(&opts) } else { None };

    // PQconnectPoll's host walk: a failed attempt appends its message and
    // the next host is tried; a server error other than "cannot connect
    // now" ends the walk.
    let mut errs = String::new();
    loop {
        for ch in &hosts {
            let port = match parse_port(&ch.port) {
                Ok(p) => p,
                Err(e) => {
                    append_conn_error(&mut errs, &e);
                    continue;
                }
            };
            let mut from_file = None;
            let password = match &explicit_password {
                Some(p) => Some(p.clone()),
                None => passfile.as_deref().and_then(|f| {
                    let pwhost = if ch.host.is_empty() { &ch.hostaddr } else { &ch.host };
                    let p = conninfo::password_from_file(pwhost, &ch.port, &pw_dbname, &user, f)?;
                    from_file = Some(f);
                    Some(p)
                }),
            };
            let display_host = pq_host(ch);
            let (stream, fd, target) = match dial(ch, port, &opts, prng.as_mut(), we.connect)? {
                Ok(d) => d,
                Err((e, terminal)) => {
                    append_conn_error(&mut errs, &e);
                    if terminal {
                        return Ok(Err(errs));
                    }
                    continue;
                }
            };

            match &stream {
                Stream::Tcp(s) => s.set_nonblocking(true).expect("set_nonblocking"),
                #[cfg(not(target_family = "wasm"))]
                Stream::Unix(s) => s.set_nonblocking(true).expect("set_nonblocking"),
            }

            // libpq's emitHostIdentityInfo (fe-connect.c), emitted SPECULATIVELY
            // into conn->errorMessage the moment the target address is
            // identified: every later failure of this attempt — the
            // startup-packet send, a server-sent FATAL during auth, fe_sendauth,
            // a SCRAM failure — carries the prefix. Dial failures above already
            // build it inline.
            let host_identity = match &target {
                DialTarget::Unix(path) => {
                    format!("connection to server on socket \"{path}\" failed: ")
                }
                DialTarget::Tcp(disp_host, disp_port) => {
                    // C: a CHT_HOST_ADDRESS target (hostaddr=) displays bare; a
                    // host NAME whose looked-up address differs textually
                    // displays "name" (addr) (emitHostIdentityInfo's strcmp arm).
                    let peer_ip = match &stream {
                        Stream::Tcp(s) => {
                            s.peer_addr().map(|a| a.ip().to_string()).unwrap_or_default()
                        }
                        #[cfg(not(target_family = "wasm"))]
                        Stream::Unix(_) => String::new(),
                    };
                    if !ch.hostaddr.is_empty() || peer_ip.is_empty() || peer_ip == *disp_host {
                        format!("connection to server at \"{disp_host}\", port {disp_port} failed: ")
                    } else {
                        format!(
                            "connection to server at \"{disp_host}\" ({peer_ip}), port {disp_port} failed: "
                        )
                    }
                }
            };

            if let Stream::Tcp(_) = &stream {
                if let Err(e) = setup_tcp_socket(fd, &opts) {
                    append_conn_error(&mut errs, &format!("{host_identity}{e}"));
                    continue;
                }
            }

            let peer = match &stream {
                Stream::Tcp(s) => s.peer_addr().ok(),
                #[cfg(not(target_family = "wasm"))]
                Stream::Unix(_) => None,
            };
            let mut conn = PgConn {
                _stream: stream,
                fd,
                external_fd: false,
                target,
                inbuf: Vec::new(),
                inpos: 0,
                conn_ok: true,
                terminate_sent: false,
                in_copy: false,
                copy_server_done: false,
                copy_client_done: false,
                peer,
                err_prescanned: false,
                pending_results: false,
                txn_status: b'I',
                err: String::new(),
                opts: opts.clone(),
                display_host,
                display_port: port as i32,
                be_pid: 0,
                be_key: Vec::new(),
                server_version: 0,
                used_password: false,
                last_sqlstate: String::new(),
                params: Vec::new(),
                notifies: std::collections::VecDeque::new(),
                last_query: None,
                we,
            };

            let mut body = Vec::new();
            body.extend_from_slice(&PG_PROTOCOL_3_0.to_be_bytes());
            for (k, v) in startup_params {
                body.extend_from_slice(k.as_bytes());
                body.push(0);
                body.extend_from_slice(v.as_bytes());
                body.push(0);
            }
            body.push(0);
            let mut pkt = Vec::with_capacity(4 + body.len());
            pkt.extend_from_slice(&((body.len() as u32 + 4).to_be_bytes()));
            pkt.extend_from_slice(&body);
            if let Err(e) = conn.send_all(&pkt) {
                append_conn_error(&mut errs, &format!("{host_identity}{e}"));
                continue;
            }

            let auth_opts = auth::AuthOptions {
                password: password.as_deref(),
                keys: keys.clone(),
                policy: policy.clone(),
                passfile: from_file,
            };
            match auth::handshake(&mut conn, &user, &auth_opts)? {
                Ok(()) => match conn.check_target(server_type, &host_identity)? {
                    TargetCheck::Ok => return Ok(Ok(conn)),
                    TargetCheck::NextHost(e) => append_conn_error(&mut errs, &e),
                    TargetCheck::Fail(e) => {
                        append_conn_error(&mut errs, &e);
                        return Ok(Err(errs));
                    }
                },
                Err(e) => {
                    append_conn_error(&mut errs, &format!("{host_identity}{e}"));
                    if conn.last_sqlstate != "57P03" {
                        return Ok(Err(errs));
                    }
                }
            }
        }
        // PQconnectPoll (fe-connect.c:3010): prefer-standby found no standby;
        // drop the requirement and walk the hosts again.
        if server_type == TargetServerType::PreferStandby && !hosts.is_empty() {
            server_type = TargetServerType::Any;
            continue;
        }
        break;
    }
    Ok(Err(errs))
}

// Standard SQL connection (libpq PQconnectdb shape): resolve conninfo, build
// the usual startup params, force client_encoding to `encoding` when given
// (dblink pins the remote to the local database encoding). Ok(Err(msg)) is
// libpq's CONNECTION_BAD-with-errorMessage arm.
pub fn connect_db(
    conninfo: &str,
    encoding: Option<&str>,
    we: WaitEvents,
) -> PgResult<Result<PgConn, String>> {
    let opts = match resolve_conninfo(conninfo) {
        Ok(o) => o,
        Err(e) => return Ok(Err(e)),
    };
    let params = db_startup_params(&opts, encoding);
    let params: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, v.as_str())).collect();
    connect(opts, &params, we)
}

// build_startup_packet (fe-protocol3.c:2478) for a plain SQL connection:
// user, database, replication, options, application_name (an explicit
// empty one suppresses fallback_application_name), client_encoding.
fn db_startup_params(
    opts: &[(String, String)],
    encoding: Option<&str>,
) -> Vec<(&'static str, String)> {
    let user = resolve_user(opts);
    let dbname = opt(opts, "dbname").filter(|s| !s.is_empty()).unwrap_or(&user).to_string();
    let appname = opt(opts, "application_name")
        .or_else(|| opt(opts, "fallback_application_name"))
        .unwrap_or("");
    let enc = encoding.or_else(|| opt(opts, "client_encoding")).unwrap_or("");
    let mut params = vec![("user", user.clone()), ("database", dbname)];
    for (k, v) in [
        ("replication", opt(opts, "replication").unwrap_or("")),
        ("options", opt(opts, "options").unwrap_or("")),
        ("application_name", appname),
        ("client_encoding", enc),
    ] {
        if !v.is_empty() {
            params.push((k, v.to_string()));
        }
    }
    params
}

/// libpq PGTransactionStatusType (PQtransactionStatus).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransactionStatus {
    /// 'I' — connection idle, no transaction open.
    Idle,
    /// A command is in flight (libpq: asyncStatus != PGASYNC_IDLE).
    Active,
    /// 'T' — idle, inside a transaction block.
    InTrans,
    /// 'E' — idle, inside a FAILED transaction block.
    InError,
    /// Bad connection or unrecognized status byte.
    Unknown,
}

impl PgConn {
    pub fn error_message(&self) -> String {
        self.err.clone()
    }

    // PQtransactionStatus (fe-exec.c): UNKNOWN on a bad connection, ACTIVE
    // while a query is pending, else the last ReadyForQuery status byte.
    pub fn transaction_status(&self) -> TransactionStatus {
        if !self.conn_ok {
            return TransactionStatus::Unknown;
        }
        if self.pending_results {
            return TransactionStatus::Active;
        }
        match self.txn_status {
            b'I' => TransactionStatus::Idle,
            b'T' => TransactionStatus::InTrans,
            b'E' => TransactionStatus::InError,
            _ => TransactionStatus::Unknown,
        }
    }

    pub fn connection_bad(&self) -> bool {
        !self.conn_ok
    }

    pub fn socket(&self) -> pgsocket {
        self.fd as pgsocket
    }

    pub fn backend_pid(&self) -> i32 {
        self.be_pid
    }

    pub fn server_version(&self) -> i32 {
        self.server_version
    }

    /// PQconnectionUsedPassword.
    pub fn used_password(&self) -> bool {
        self.used_password
    }

    /// PQparameterStatus.
    pub fn parameter_status(&self, name: &str) -> Option<&str> {
        self.params.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// PQclientEncoding, as the encoding NAME the server last reported.
    pub fn client_encoding(&self) -> Option<&str> {
        self.parameter_status("client_encoding")
    }

    pub fn conn_options(&self) -> &[(String, String)] {
        &self.opts
    }

    // libpqrcv_get_conninfo (libpqwalreceiver.c:365-388): walk PQconninfo()'s
    // table-ordered option copy, skip debug ('D' dispchar) and empty options,
    // and obfuscate every option whose dispchar carries '*' (password,
    // sslpassword, oauth_client_secret) as "********".
    pub fn display_conninfo(&self) -> String {
        let mut out = String::new();
        for o in CONNINFO_OPTIONS {
            if o.dispchar.contains('D') {
                continue;
            }
            let Some(v) = opt(&self.opts, o.keyword).filter(|v| !v.is_empty()) else {
                continue;
            };
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(o.keyword);
            out.push('=');
            out.push_str(if o.dispchar.contains('*') { "********" } else { v });
        }
        out
    }

    pub fn senderinfo(&self) -> (String, i32) {
        (self.display_host.clone(), self.display_port)
    }

    /// PQnotifies (fe-exec.c:2692): parse whatever is buffered first, then
    /// pop the queue.
    pub fn next_notify(&mut self) -> Option<Notify> {
        self.parse_idle_input();
        self.notifies.pop_front()
    }

    // parseInput's idle arm, run only from PQnotifies: with no command in
    // flight every complete buffered message is asynchronous traffic
    // (NotificationResponse, NoticeResponse, ParameterStatus). consume_input
    // itself must not parse — the startup handshake and the result loops
    // read through it with pending_results still false.
    fn parse_idle_input(&mut self) {
        while !self.pending_results && !self.in_copy {
            match self.next_message() {
                Ok(Some((t, body))) => self.note_async(t, &body),
                Ok(None) | Err(_) => return,
            }
        }
    }

    // wasm32: the wasi libc crate exposes no send/recv (WASI p1 has no
    // socket-open surface, so connect() can never have succeeded and these
    // are unreachable); the stubs keep the crate compiling and fail like a
    // dead connection if ever reached (the ip::sys stub convention).
    #[cfg(target_family = "wasm")]
    pub(crate) fn send_all(&mut self, _buf: &[u8]) -> Result<(), String> {
        self.conn_ok = false;
        self.err = format!(
            "could not send data to server: {}",
            std::io::Error::from_raw_os_error(52) // WASI ENOSYS
        );
        Err(self.err.clone())
    }

    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn send_all(&mut self, buf: &[u8]) -> Result<(), String> {
        let mut off = 0;
        while off < buf.len() {
            let n = unsafe {
                libc::send(
                    self.fd,
                    buf[off..].as_ptr().cast(),
                    buf.len() - off,
                    libc::MSG_NOSIGNAL,
                )
            };
            if n > 0 {
                off += n as usize;
                continue;
            }
            let e = std::io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::EAGAIN) => {
                    if let Err(pe) = wait_socket(self.fd, WL_SOCKET_WRITEABLE, self.we.receive) {
                        self.conn_ok = false;
                        return Err(format!("{pe:?}"));
                    }
                }
                _ => {
                    self.conn_ok = false;
                    self.err = format!("could not send data to server: {e}");
                    return Err(self.err.clone());
                }
            }
        }
        Ok(())
    }

    // PQconsumeInput: drain whatever is readable right now. false == the
    // connection died (self.err set).
    // wasm32: no recv in the wasi libc crate; unreachable stub (see send_all).
    #[cfg(target_family = "wasm")]
    pub fn consume_input(&mut self) -> bool {
        self.conn_ok = false;
        self.err = format!(
            "could not receive data from server: {}",
            std::io::Error::from_raw_os_error(52) // WASI ENOSYS
        );
        false
    }

    #[cfg(not(target_family = "wasm"))]
    pub fn consume_input(&mut self) -> bool {
        if !self.conn_ok {
            return false;
        }
        let mut buf = [0u8; 16384];
        let mut someread = false;
        loop {
            let n = unsafe { libc::recv(self.fd, buf.as_mut_ptr().cast(), buf.len(), 0) };
            if n > 0 {
                self.inbuf.extend_from_slice(&buf[..n as usize]);
                if (n as usize) < buf.len() {
                    return true;
                }
                someread = true;
                continue;
            }
            if n == 0 {
                // pqReadData_internal: a zero read after data was read this
                // call is reported next time (fe-misc.c:751); a bare one is
                // EOF, with libpq's two explanatory lines (fe-misc.c:833).
                if someread {
                    return true;
                }
                self.conn_ok = false;
                self.append_conn_error(SERVER_CLOSED_MSG);
                return false;
            }
            let e = std::io::Error::last_os_error();
            return match e.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::EAGAIN) => true,
                _ => {
                    self.conn_ok = false;
                    self.err = format!("could not receive data from server: {e}");
                    false
                }
            };
        }
    }

    // Ok(None) = need more bytes; Err = the frame header is malformed. The
    // wire length is a SIGNED i32 the server controls: validate it BEFORE
    // it becomes a usize, or a negative/short value turns into an enormous
    // index and a panic. Once framing is lost there is no way to find the
    // next message boundary, so the connection is marked dead (libpq's
    // handleSyncLoss discipline).
    // The type byte of the next complete message, without consuming it.
    fn peek_message_type(&self) -> Option<u8> {
        let avail = self.inbuf.len() - self.inpos;
        if avail < 5 {
            return None;
        }
        let p = self.inpos;
        let wire_len = be_i32(&self.inbuf[p + 1..p + 5]);
        if !(4..=MAX_MESSAGE_LEN).contains(&wire_len) || avail < 1 + wire_len as usize {
            return None;
        }
        Some(self.inbuf[p])
    }

    pub(crate) fn next_message(&mut self) -> Result<Option<(u8, Vec<u8>)>, String> {
        let avail = self.inbuf.len() - self.inpos;
        if avail < 5 {
            return Ok(None);
        }
        let p = self.inpos;
        let t = self.inbuf[p];
        let wire_len = be_i32(&self.inbuf[p + 1..p + 5]);
        if !(4..=MAX_MESSAGE_LEN).contains(&wire_len) {
            self.conn_ok = false;
            self.err = format!(
                "lost synchronization with server: got message type \"{}\", length {wire_len}",
                t as char
            );
            return Err(self.err.clone());
        }
        let len = wire_len as usize;
        if avail < 1 + len {
            return Ok(None);
        }
        let body = self.inbuf[p + 5..p + 1 + len].to_vec();
        self.inpos = p + 1 + len;
        if self.inpos == self.inbuf.len() {
            self.inbuf.clear();
            self.inpos = 0;
        } else if self.inpos > 65536 {
            self.inbuf.drain(..self.inpos);
            self.inpos = 0;
        }
        Ok(Some((t, body)))
    }

    pub(crate) fn read_message(
        &mut self,
        wait_event_info: u32,
    ) -> PgResult<Result<(u8, Vec<u8>), String>> {
        loop {
            match self.next_message() {
                Ok(Some(m)) => return Ok(Ok(m)),
                Ok(None) => {}
                Err(e) => return Ok(Err(e)),
            }
            if !self.conn_ok {
                return Ok(Err(self.socket_dropped()));
            }
            wait_socket(self.fd, WL_SOCKET_READABLE, wait_event_info)?;
            if !self.consume_input() {
                return Ok(Err(self.socket_dropped()));
            }
        }
    }

    // libpqsrv_get_result after PQconsumeInput fails: PQgetResult is still
    // BUSY, pqWait finds the dropped socket (pqSocketCheck "invalid socket",
    // fe-misc.c:1244) and pqSaveErrorResult goes PGASYNC_IDLE.
    fn socket_dropped(&mut self) -> String {
        self.append_conn_error("invalid socket");
        self.pending_results = false;
        self.in_copy = false;
        self.err.clone()
    }

    // A lost or poisoned connection ends the query (libpq: pqSaveErrorResult
    // + PGASYNC_IDLE): the error comes back once, then get_result yields None.
    // upstream c318777da8b8 (18.4): postgres_fdw: Fix handling of abort-cleanup-failed connections.
    fn proto_error(&mut self, e: String) -> QueryResult {
        self.conn_ok = false;
        self.err = e.clone();
        self.pending_results = false;
        self.in_copy = false;
        QueryResult::error(e)
    }

    // Async-message bookkeeping shared by every read loop.
    pub(crate) fn note_async(&mut self, t: u8, body: &[u8]) {
        match t {
            b'S' => {
                let (name, next) = cstr_at(body, 0);
                let (value, _) = cstr_at(body, next);
                if name == "server_version" {
                    self.server_version = parse_server_version(&value);
                }
                if let Some(slot) = self.params.iter_mut().find(|(k, _)| *k == name) {
                    slot.1 = value;
                } else {
                    self.params.push((name, value));
                }
            }
            b'A' => {
                // NotificationResponse needs at least the 4-byte pid; a
                // shorter body is malformed — drop it rather than index
                // past the frame (losing a corrupt notify is harmless).
                let Some(pid_bytes) = body.get(0..4) else {
                    return;
                };
                let be_pid = be_i32(pid_bytes);
                let (channel, next) = cstr_at(body, 4);
                let (extra, _) = cstr_at(body, next);
                self.notifies.push_back(Notify { channel, be_pid, extra });
            }
            b'N' => {
                // defaultNoticeProcessor (fe-connect.c:7857): remote notices
                // go to this process's stderr, formatted like an error.
                let text = errmsg::build_message(&parse_diag(body), self.last_query.as_deref(), false);
                eprint!("{text}");
            }
            b'K' => {
                // BackendKeyData shorter than its 4-byte pid is malformed;
                // keep the zero pid/key (cancel then simply can't target).
                let Some(pid_bytes) = body.get(0..4) else {
                    return;
                };
                self.be_pid = be_i32(pid_bytes);
                self.be_key = body[4..].to_vec();
            }
            _ => {}
        }
    }

    // libpqsrv_exec (libpq-be-fe-helpers.h:252): PQsendQuery + collect until
    // ReadyForQuery keeping the last resultset. It has PQsendQuery's
    // preconditions, not PQexec's — a busy connection is refused, never
    // silently drained (dblink_exec, the cursor commands and the walreceiver
    // all go through this shape in C). CopyBothResponse/CopyOutResponse
    // return immediately with the stream open.
    pub fn exec(&mut self, query: &str) -> PgResult<QueryResult> {
        if !self.send_query_raw(query) {
            return Ok(QueryResult::error(self.err.clone()));
        }

        let mut nfields = 0usize;
        let mut tuples = false;
        let mut rows: Vec<Vec<Option<Vec<u8>>>> = Vec::new();
        let mut result = QueryResult::status_only(ExecStatus::CommandOk);
        let mut error: Option<(String, ErrorFields)> = None;
        loop {
            let (t, mbody) = match self.read_message(self.we.receive)? {
                Ok(m) => m,
                Err(e) => return Ok(QueryResult::error(e)),
            };
            match t {
                b'T' => {
                    nfields = match row_desc_nfields(&mbody) {
                        Ok(n) => n,
                        Err(e) => return Ok(self.proto_error(e)),
                    };
                    tuples = true;
                    rows.clear();
                }
                b'D' => match parse_data_row(&mbody) {
                    Ok(r) if r.len() != nfields => {
                        return Ok(self.proto_error(unexpected_field_count(r.len(), nfields)))
                    }
                    Ok(r) => rows.push(r),
                    Err(e) => return Ok(self.proto_error(e)),
                },
                b'C' => {
                    let (tag, _) = cstr_at(&mbody, 0);
                    result = QueryResult {
                        status: tuples_status(tuples),
                        nfields,
                        rows: std::mem::take(&mut rows),
                        cmd_tag: tag,
                        diag: None,
                        err: String::new(),
                    };
                    nfields = 0;
                    tuples = false;
                }
                b'I' => result = QueryResult::status_only(ExecStatus::Empty),
                b'E' => {
                    let diag = parse_diag(&mbody);
                    self.note_error_response(&mbody);
                    error = Some((self.err.clone(), diag));
                }
                b'W' => {
                    self.in_copy = true;
                    self.copy_server_done = false;
                    self.copy_client_done = false;
                    return Ok(QueryResult::status_only(ExecStatus::CopyBoth));
                }
                b'H' => {
                    // CopyOutResponse: the caller drains with get_copy_data
                    // until CopyData::End, then get_result for the tail.
                    self.in_copy = true;
                    self.copy_server_done = false;
                    self.copy_client_done = true; // nothing to send on COPY OUT
                    return Ok(QueryResult::status_only(ExecStatus::CopyOut));
                }
                b'S' | b'N' | b'A' => self.note_async(t, &mbody),
                b'Z' => {
                    self.txn_status = mbody.first().copied().unwrap_or(b'I');
                    self.pending_results = false;
                    break;
                }
                other => {
                    self.conn_ok = false;
                    return Ok(QueryResult::error(format!(
                        "unexpected message type \"{}\" from server",
                        other as char
                    )));
                }
            }
        }
        if let Some((e, diag)) = error {
            let mut r = QueryResult::error(e);
            r.diag = Some(diag);
            return Ok(r);
        }
        Ok(result)
    }

    // pqGetErrorNotice3 (fe-protocol3.c:995): an ErrorResponse's text joins
    // whatever the cycle already accumulated.
    fn note_error_response(&mut self, body: &[u8]) {
        if std::mem::take(&mut self.err_prescanned) {
            return;
        }
        let text = parse_error_fields(body, self.last_query.as_deref());
        self.append_conn_error(&text);
    }

    // libpq_append_conn_error: messages accumulate in conn->errorMessage
    // until the next query cycle clears it.
    fn append_conn_error(&mut self, msg: &str) {
        if !self.err.is_empty() {
            self.err.push('\n');
        }
        self.err.push_str(msg);
    }

    // PQsendQueryStart (fe-exec.c:1700): a new query cycle clears the error
    // state (not while a command still owns the buffer), a dead connection
    // refuses every send, and so does a busy one (asyncStatus != IDLE).
    // upstream c318777da8b8 (18.4): postgres_fdw: Fix handling of abort-cleanup-failed connections.
    fn send_query_start(&mut self) -> bool {
        let busy = self.pending_results || self.in_copy;
        if !busy {
            self.err.clear();
            self.err_prescanned = false;
        }
        if !self.conn_ok {
            self.append_conn_error("no connection to the server");
            return false;
        }
        if busy {
            self.append_conn_error("another command is already in progress");
            return false;
        }
        true
    }

    // PQsendQueryParams/PQsendPrepare/PQsendQueryPrepared (fe-exec.c:1527).
    fn check_param_count(&mut self, n: usize) -> bool {
        if n > PQ_QUERY_PARAM_MAX_LIMIT {
            self.append_conn_error(&format!(
                "number of parameters must be between 0 and {PQ_QUERY_PARAM_MAX_LIMIT}"
            ));
            return false;
        }
        true
    }

    fn send_query_raw(&mut self, query: &str) -> bool {
        if !self.send_query_start() {
            return false;
        }
        self.last_query = Some(query.to_string());
        let mut body = query.as_bytes().to_vec();
        body.push(0);
        if let Err(e) = self.send_all(&msg(b'Q', &body)) {
            self.err = e;
            return false;
        }
        self.pending_results = true;
        true
    }

    /// PQsendQuery: dispatch without waiting. false == send failed (err set).
    pub fn send_query(&mut self, query: &str) -> bool {
        self.send_query_raw(query)
    }

    // ---- extended query protocol (Parse/Bind/Describe/Execute/Sync) ----
    //
    // The wire shapes mirror libpq fe-exec.c: pqSendQueryGuts (PQsendQueryParams
    // / PQsendQueryPrepared), PQsendPrepare. Params and results travel as TEXT
    // (postgres_fdw transmits params textually and needs no binary results).

    /// PQsendQueryParams: Parse(unnamed) + Bind + Describe(portal) +
    /// Execute + Sync in one flush. `param_types` is either empty (server
    /// infers every type, libpq's paramTypes==NULL) or one Oid per param
    /// (0 = infer). `params` are text values, None = NULL.
    pub fn send_query_params(
        &mut self,
        query: &str,
        param_types: &[Oid],
        params: &[Option<&str>],
    ) -> bool {
        debug_assert!(param_types.is_empty() || param_types.len() == params.len());
        if !self.send_query_start() || !self.check_param_count(params.len()) {
            return false;
        }
        self.last_query = Some(query.to_string());
        let mut pkt = msg(b'P', &parse_body("", query, param_types, params.len()));
        pkt.extend_from_slice(&msg(b'B', &bind_body("", "", params)));
        pkt.extend_from_slice(&msg(b'D', b"P\0"));
        pkt.extend_from_slice(&msg(b'E', &execute_body("", 0)));
        pkt.extend_from_slice(&msg(b'S', &[]));
        if let Err(e) = self.send_all(&pkt) {
            self.err = e;
            return false;
        }
        self.pending_results = true;
        true
    }

    /// PQsendPrepare: Parse(name) + Sync (no Describe, exactly libpq).
    pub fn send_prepare(&mut self, stmt_name: &str, query: &str, param_types: &[Oid]) -> bool {
        if !self.send_query_start() || !self.check_param_count(param_types.len()) {
            return false;
        }
        self.last_query = Some(query.to_string());
        let mut pkt = msg(b'P', &parse_body(stmt_name, query, param_types, param_types.len()));
        pkt.extend_from_slice(&msg(b'S', &[]));
        if let Err(e) = self.send_all(&pkt) {
            self.err = e;
            return false;
        }
        self.pending_results = true;
        true
    }

    /// PQsendQueryPrepared: Bind(stmt) + Describe(portal) + Execute + Sync.
    pub fn send_query_prepared(&mut self, stmt_name: &str, params: &[Option<&str>]) -> bool {
        if !self.send_query_start() || !self.check_param_count(params.len()) {
            return false;
        }
        self.last_query = None;
        let mut pkt = msg(b'B', &bind_body("", stmt_name, params));
        pkt.extend_from_slice(&msg(b'D', b"P\0"));
        pkt.extend_from_slice(&msg(b'E', &execute_body("", 0)));
        pkt.extend_from_slice(&msg(b'S', &[]));
        if let Err(e) = self.send_all(&pkt) {
            self.err = e;
            return false;
        }
        self.pending_results = true;
        true
    }

    /// PQexecParams: parameterized query, text params/results, keep the last
    /// resultset (PQexecStart discard discipline included).
    pub fn exec_params(
        &mut self,
        query: &str,
        param_types: &[Oid],
        params: &[Option<&str>],
    ) -> PgResult<QueryResult> {
        if self.pending_results {
            self.drain();
        }
        if !self.send_query_params(query, param_types, params) {
            return Ok(QueryResult::error(self.err.clone()));
        }
        self.collect_last_result()
    }

    /// PQprepare: create a named prepared statement; CommandOk on success.
    pub fn prepare(
        &mut self,
        stmt_name: &str,
        query: &str,
        param_types: &[Oid],
    ) -> PgResult<QueryResult> {
        if self.pending_results {
            self.drain();
        }
        if !self.send_prepare(stmt_name, query, param_types) {
            return Ok(QueryResult::error(self.err.clone()));
        }
        self.collect_last_result()
    }

    /// PQexecPrepared: execute a named prepared statement with text params.
    pub fn exec_prepared(
        &mut self,
        stmt_name: &str,
        params: &[Option<&str>],
    ) -> PgResult<QueryResult> {
        if self.pending_results {
            self.drain();
        }
        if !self.send_query_prepared(stmt_name, params) {
            return Ok(QueryResult::error(self.err.clone()));
        }
        self.collect_last_result()
    }

    // PQexec's collect loop, shared by the extended-protocol entry points:
    // gather results until ReadyForQuery, keep the last resultset. Handles
    // the extended-protocol acknowledgements ('1' ParseComplete, '2'
    // BindComplete, '3' CloseComplete, 'n' NoData, 't' ParameterDescription)
    // and PortalSuspended ('s', treated as result-complete like libpq).
    fn collect_last_result(&mut self) -> PgResult<QueryResult> {
        let mut nfields = 0usize;
        let mut tuples = false;
        let mut rows: Vec<Vec<Option<Vec<u8>>>> = Vec::new();
        let mut result = QueryResult::status_only(ExecStatus::CommandOk);
        let mut error: Option<(String, ErrorFields)> = None;
        loop {
            let (t, mbody) = match self.read_message(self.we.receive)? {
                Ok(m) => m,
                Err(e) => return Ok(QueryResult::error(e)),
            };
            match t {
                b'T' => {
                    nfields = match row_desc_nfields(&mbody) {
                        Ok(n) => n,
                        Err(e) => return Ok(self.proto_error(e)),
                    };
                    tuples = true;
                    rows.clear();
                }
                b'D' => match parse_data_row(&mbody) {
                    Ok(r) if r.len() != nfields => {
                        return Ok(self.proto_error(unexpected_field_count(r.len(), nfields)))
                    }
                    Ok(r) => rows.push(r),
                    Err(e) => return Ok(self.proto_error(e)),
                },
                b'C' | b's' => {
                    let tag =
                        if t == b'C' { cstr_at(&mbody, 0).0 } else { String::new() };
                    result = QueryResult {
                        status: tuples_status(tuples),
                        nfields,
                        rows: std::mem::take(&mut rows),
                        cmd_tag: tag,
                        diag: None,
                        err: String::new(),
                    };
                    nfields = 0;
                    tuples = false;
                }
                b'I' => result = QueryResult::status_only(ExecStatus::Empty),
                b'E' => {
                    let diag = parse_diag(&mbody);
                    self.note_error_response(&mbody);
                    error = Some((self.err.clone(), diag));
                }
                b'1' | b'2' | b'3' | b'n' | b't' => {}
                b'S' | b'N' | b'A' => self.note_async(t, &mbody),
                b'Z' => {
                    self.txn_status = mbody.first().copied().unwrap_or(b'I');
                    self.pending_results = false;
                    break;
                }
                other => {
                    self.conn_ok = false;
                    return Ok(QueryResult::error(format!(
                        "unexpected message type \"{}\" from server",
                        other as char
                    )));
                }
            }
        }
        if let Some((e, diag)) = error {
            let mut r = QueryResult::error(e);
            r.diag = Some(diag);
            return Ok(r);
        }
        Ok(result)
    }

    // Deadline-bounded readable wait (postgres_fdw's abort-cleanup
    // discipline pumps with timeouts). Ok(true) = readable; Ok(false) =
    // timed out. Latch wakeups run CHECK_FOR_INTERRUPTS and keep waiting.
    pub fn wait_readable_timeout(&self, timeout_ms: i64) -> PgResult<bool> {
        self.wait_readable_timeout_ev(timeout_ms, self.we.receive)
    }

    // The same wait under a caller-chosen wait event (C passes the event to
    // WaitLatchOrSocket per call site: PostgresFdwCleanupResult,
    // LogicalApplyMain).
    pub fn wait_readable_timeout_ev(&self, timeout_ms: i64, wait_event_info: u32) -> PgResult<bool> {
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_millis(timeout_ms.max(0) as u64);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return Ok(false);
            }
            let rc = latch::WaitLatchOrSocket(
                init_small::globals::MyLatch(),
                WL_LATCH_SET | WL_EXIT_ON_PM_DEATH | WL_SOCKET_READABLE | WL_TIMEOUT,
                self.fd,
                left.as_millis().min(i64::MAX as u128) as i64,
                wait_event_info,
            )?;
            if rc & WL_LATCH_SET != 0 {
                if let Some(l) = init_small::globals::MyLatch() {
                    latch::ResetLatch(l);
                }
                postgres_seams::check_for_interrupts::call()?;
            }
            if rc & WL_SOCKET_READABLE != 0 {
                return Ok(true);
            }
            if rc & WL_TIMEOUT != 0 {
                return Ok(false);
            }
        }
    }

    // PQisBusy (after a consume_input): would get_result block? Not busy once
    // a whole result's terminator (or ReadyForQuery) is buffered, or when no
    // query is pending at all.
    pub fn is_busy(&mut self) -> bool {
        if !self.pending_results || !self.conn_ok {
            return false;
        }
        let mut p = self.inpos;
        loop {
            if self.inbuf.len() - p < 5 {
                return true;
            }
            let wire_len = be_i32(&self.inbuf[p + 1..p + 5]);
            if !(4..=MAX_MESSAGE_LEN).contains(&wire_len) {
                // Framing loss: report not-busy so the caller's get_result
                // runs and surfaces the connection error.
                return false;
            }
            let len = wire_len as usize;
            if self.inbuf.len() - p < 1 + len {
                return true;
            }
            match self.inbuf[p] {
                b'E' => {
                    // pqGetErrorNotice3 appends the text to errorMessage as
                    // it parses; PQgetResult does not append it again.
                    if !self.err_prescanned {
                        let body = self.inbuf[p + 5..p + 1 + len].to_vec();
                        let text = parse_error_fields(&body, self.last_query.as_deref());
                        self.append_conn_error(&text);
                        self.err_prescanned = true;
                    }
                    return false;
                }
                b'C' | b'I' | b'Z' | b'W' | b'H' | b'G' => return false,
                _ => {}
            }
            p += 1 + len;
        }
    }

    // libpqrcv_receive's wait arm: block until the socket is readable (or a
    // latch interrupt); the caller then calls consume_input + get_copy_data.
    pub fn wait_readable(&mut self) -> PgResult<()> {
        wait_socket(self.fd, WL_SOCKET_READABLE, self.we.receive)
    }

    // PQgetCopyData(async=true).
    pub fn get_copy_data(&mut self) -> Result<CopyData, String> {
        loop {
            // fe-protocol3.c getCopyDataMessage's default arm: any message
            // that is not copy data, CopyDone, an error or an async notice
            // "means the server is done with the copy" — it is left in the
            // buffer for the next get_result and the copy reports its end.
            // The walsender's shutdown path is exactly this: CommandComplete
            // ("COPY 0") with no CopyDone first (WalSndDone). Consuming it
            // here as "unexpected message type during COPY" killed the
            // connection and the walreceiver with it.
            if let Some(t) = self.peek_message_type() {
                if !matches!(t, b'd' | b'c' | b'E' | b'S' | b'N' | b'A') {
                    self.copy_server_done = true;
                    self.copy_client_done = true;
                    return Ok(CopyData::End);
                }
            }
            let (t, body) = match self.next_message() {
                Ok(Some(m)) => m,
                Ok(None) => return Ok(CopyData::Block),
                Err(e) => return Err(e),
            };
            match t {
                // fe-protocol3.c pqGetCopyData3: a zero-length CopyData
                // message is dropped, never surfaced as a 0-byte read.
                b'd' if body.is_empty() => continue,
                b'd' => return Ok(CopyData::Msg(body)),
                b'c' => {
                    self.copy_server_done = true;
                    return Ok(CopyData::End);
                }
                b'E' => {
                    self.note_error_response(&body);
                    return Err(self.err.clone());
                }
                b'S' | b'N' | b'A' => self.note_async(t, &body),
                other => {
                    self.conn_ok = false;
                    self.err = format!("unexpected message type \"{}\" during COPY", other as char);
                    return Err(self.err.clone());
                }
            }
        }
    }

    // PQgetResult: one result per call; None once ReadyForQuery is consumed
    // (or immediately when no query is pending — libpq's IDLE arm).
    // A half-closed CopyBoth (server CopyDone, ours unsent) reports COPY_IN
    // without reading, exactly as libpq does — the walreceiver may still send.
    pub fn get_result(&mut self) -> PgResult<Option<QueryResult>> {
        if self.in_copy && self.copy_server_done && !self.copy_client_done {
            return Ok(Some(QueryResult::status_only(ExecStatus::CopyIn)));
        }
        // The other half-closed shape: our CopyDone sent (PQputCopyEnd moves a
        // COPY_BOTH connection to COPY_OUT), the server's not yet seen — or a
        // plain COPY OUT still streaming. libpq's PQgetResult answers
        // PGRES_COPY_OUT from getCopyResult without reading; the caller drains
        // with get_copy_data (PQendcopy) and then reads the completion(s).
        if self.in_copy && self.copy_client_done && !self.copy_server_done {
            return Ok(Some(QueryResult::status_only(ExecStatus::CopyOut)));
        }
        if !self.pending_results && !self.in_copy {
            return Ok(None);
        }
        let mut nfields = 0usize;
        let mut tuples = false;
        let mut rows: Vec<Vec<Option<Vec<u8>>>> = Vec::new();
        loop {
            let (t, body) = match self.read_message(self.we.receive)? {
                Ok(m) => m,
                Err(e) => return Ok(Some(self.proto_error(e))),
            };
            match t {
                b'T' => {
                    nfields = match row_desc_nfields(&body) {
                        Ok(n) => n,
                        Err(e) => return Ok(Some(self.proto_error(e))),
                    };
                    tuples = true;
                    rows.clear();
                }
                b'D' => match parse_data_row(&body) {
                    Ok(r) if r.len() != nfields => {
                        return Ok(Some(self.proto_error(unexpected_field_count(r.len(), nfields))))
                    }
                    Ok(r) => rows.push(r),
                    Err(e) => return Ok(Some(self.proto_error(e))),
                },
                b'C' | b's' => {
                    let tag = if t == b'C' { cstr_at(&body, 0).0 } else { String::new() };
                    let status = tuples_status(tuples);
                    return Ok(Some(QueryResult {
                        status,
                        nfields,
                        rows,
                        cmd_tag: tag,
                        diag: None,
                        err: String::new(),
                    }));
                }
                b'1' | b'2' | b'3' | b'n' | b't' => {}
                b'I' => return Ok(Some(QueryResult::status_only(ExecStatus::Empty))),
                b'E' => {
                    let diag = parse_diag(&body);
                    self.note_error_response(&body);
                    let mut r = QueryResult::error(self.err.clone());
                    r.diag = Some(diag);
                    return Ok(Some(r));
                }
                b'W' => {
                    self.in_copy = true;
                    self.copy_server_done = false;
                    self.copy_client_done = false;
                    return Ok(Some(QueryResult::status_only(ExecStatus::CopyBoth)));
                }
                b'H' => {
                    // fe-protocol3.c: CopyOutResponse moves the connection to
                    // PGASYNC_COPY_OUT; every PQgetResult answers COPY_OUT until
                    // the caller drains (PQgetCopyData / PQendcopy).
                    self.in_copy = true;
                    self.copy_server_done = false;
                    self.copy_client_done = true;
                    return Ok(Some(QueryResult::status_only(ExecStatus::CopyOut)));
                }
                b'd' => {}
                b'c' => self.copy_server_done = true,
                b'S' | b'N' | b'A' => self.note_async(t, &body),
                b'Z' => {
                    self.txn_status = body.first().copied().unwrap_or(b'I');
                    self.in_copy = false;
                    self.pending_results = false;
                    return Ok(None);
                }
                other => {
                    let e = format!("unexpected message type \"{}\" from server", other as char);
                    return Ok(Some(self.proto_error(e)));
                }
            }
        }
    }

    // dblink's storeQueryResult shape (libpq single-row mode): rows stream
    // into `sink` as they arrive, the LAST resultset's terminal status is
    // returned (PQexec's keep-the-last rule), and CHECK_FOR_INTERRUPTS runs
    // per row. On sink error the connection still holds unread results — the
    // caller drains in its error path (C's PG_CATCH does the same).
    pub fn exec_streaming(
        &mut self,
        query: &str,
        sink: &mut dyn RowSink,
    ) -> PgResult<QueryResult> {
        if !self.send_query_raw(query) {
            return Ok(QueryResult::error(self.err.clone()));
        }
        self.stream_results(sink)
    }

    // The collect half of exec_streaming, for callers that send first and
    // treat a refused send differently from a failed query (storeQueryResult's
    // elog(ERROR) on PQsendQuery).
    pub fn stream_results(&mut self, sink: &mut dyn RowSink) -> PgResult<QueryResult> {
        let mut nfields = 0usize;
        let mut tuples = false;
        let mut started = false;
        let mut result = QueryResult::status_only(ExecStatus::CommandOk);
        let mut error: Option<(String, ErrorFields)> = None;
        loop {
            let (t, body) = match self.read_message(self.we.receive)? {
                Ok(m) => m,
                Err(e) => return Ok(QueryResult::error(e)),
            };
            match t {
                b'T' => {
                    nfields = match row_desc_nfields(&body) {
                        Ok(n) => n,
                        Err(e) => return Ok(self.proto_error(e)),
                    };
                    tuples = true;
                    started = false;
                }
                b'D' => {
                    postgres_seams::check_for_interrupts::call()?;
                    let mut cols: Vec<Option<&[u8]>> = Vec::new();
                    // Validate the row BEFORE result_start: a malformed row
                    // must not reach the sink at all.
                    if let Err(e) = parse_data_row_borrowed(&body, &mut cols) {
                        return Ok(self.proto_error(e));
                    }
                    if !started {
                        sink.result_start(&*self, nfields)?;
                        started = true;
                    }
                    sink.row(&cols)?;
                }
                b'C' => {
                    let (tag, _) = cstr_at(&body, 0);
                    if tuples && !started {
                        // Empty resultset still announces its shape (C's
                        // "if empty resultset, fill tuplestore header").
                        sink.result_start(&*self, nfields)?;
                    }
                    result = QueryResult {
                        status: tuples_status(tuples),
                        nfields,
                        rows: Vec::new(),
                        cmd_tag: tag,
                        diag: None,
                        err: String::new(),
                    };
                    nfields = 0;
                    tuples = false;
                    started = false;
                }
                b'I' => result = QueryResult::status_only(ExecStatus::Empty),
                b'E' => {
                    let diag = parse_diag(&body);
                    self.note_error_response(&body);
                    error = Some((self.err.clone(), diag));
                }
                b'1' | b'2' | b'3' | b'n' | b't' => {}
                b'S' | b'N' | b'A' => self.note_async(t, &body),
                b'Z' => {
                    self.txn_status = body.first().copied().unwrap_or(b'I');
                    self.pending_results = false;
                    break;
                }
                other => {
                    self.conn_ok = false;
                    return Ok(QueryResult::error(format!(
                        "unexpected message type \"{}\" from server",
                        other as char
                    )));
                }
            }
        }
        if let Some((e, diag)) = error {
            let mut r = QueryResult::error(e);
            r.diag = Some(diag);
            return Ok(r);
        }
        Ok(result)
    }

    // Error-path cleanup: swallow everything through ReadyForQuery so the
    // connection is reusable (C's PG_CATCH PQgetResult-until-NULL loop).
    pub fn drain(&mut self) {
        if !self.pending_results || !self.conn_ok {
            return;
        }
        loop {
            match self.read_message(self.we.receive) {
                Ok(Ok((b'Z', body))) => {
                    self.txn_status = body.first().copied().unwrap_or(b'I');
                    self.pending_results = false;
                    self.in_copy = false;
                    return;
                }
                Ok(Ok((t, body))) => self.note_async(t, &body),
                Ok(Err(_)) | Err(_) => return,
            }
        }
    }

    // libpqsrv_cancel: dial the same endpoint, send CancelRequest with the
    // saved BackendKeyData, wait (interruptibly, deadline-bounded) for the
    // server to close. None = accepted; Some(msg) = failure text.
    pub fn cancel(&self, timeout_ms: i64) -> PgResult<Option<String>> {
        let deadline_ns = pg_clock::mono_ns().saturating_add(timeout_ms.max(0) as u64 * 1_000_000);
        let sock = match &self.target {
            DialTarget::Tcp(host, port) => {
                let addrs = match cancel_addrs(host, *port, self.peer) {
                    Ok(a) => a,
                    Err(e) => return Ok(Some(e)),
                };
                match tcp_connect_addrs_deadline(host, *port, addrs, deadline_ns) {
                    Ok(s) => CancelSock::Tcp(s),
                    Err(e) => return Ok(Some(e)),
                }
            }
            #[cfg(not(target_family = "wasm"))]
            DialTarget::Unix(path) => match unix_stream_connect(path) {
                Ok(s) => CancelSock::Unix(s),
                Err(e) => {
                    return Ok(Some(format!(
                        "connection to server on socket \"{path}\" failed: {e}"
                    )))
                }
            },
            #[cfg(target_family = "wasm")]
            DialTarget::Unix(path) => {
                return Ok(Some(format!(
                    "connection to server on socket \"{path}\" failed: {}",
                    std::io::Error::from_raw_os_error(52)
                )))
            }
        };
        let mut key = self.be_key.clone();
        if key.len() != 4 {
            key.resize(4, 0);
        }
        let mut pkt = Vec::with_capacity(12 + key.len());
        pkt.extend_from_slice(&((12 + key.len() as u32).to_be_bytes()));
        pkt.extend_from_slice(&CANCEL_REQUEST_CODE.to_be_bytes());
        pkt.extend_from_slice(&self.be_pid.to_be_bytes());
        pkt.extend_from_slice(&key);
        if let Err(e) = sock.write_all(&pkt) {
            return Ok(Some(format!("could not send cancel request: {e}")));
        }
        sock.set_nonblocking();
        // Await EOF: the server closes the cancel connection once processed.
        let fd = sock.raw_fd();
        loop {
            let mut buf = [0u8; 16];
            let n = unsafe { libc::recv(fd, buf.as_mut_ptr().cast(), buf.len(), 0) };
            if n == 0 {
                return Ok(None);
            }
            if n > 0 {
                continue;
            }
            let e = std::io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::EAGAIN) => {
                    // TimestampDifferenceMilliseconds rounds up.
                    let left_ns = deadline_ns.saturating_sub(pg_clock::mono_ns());
                    if left_ns == 0 {
                        return Ok(Some("cancel request timed out".into()));
                    }
                    let rc = latch::WaitLatchOrSocket(
                        init_small::globals::MyLatch(),
                        WL_LATCH_SET | WL_EXIT_ON_PM_DEATH | WL_SOCKET_READABLE | WL_TIMEOUT,
                        fd,
                        left_ns.div_ceil(1_000_000).min(i64::MAX as u64) as i64,
                        PG_WAIT_CLIENT,
                    )?;
                    if rc & WL_LATCH_SET != 0 {
                        if let Some(l) = init_small::globals::MyLatch() {
                            latch::ResetLatch(l);
                        }
                        postgres_seams::check_for_interrupts::call()?;
                    }
                }
                _ => return Ok(Some(format!("could not receive cancel response: {e}"))),
            }
        }
    }

    pub fn put_copy_data(&mut self, data: &[u8]) -> Result<(), String> {
        self.send_all(&msg(b'd', data))
    }

    pub fn put_copy_end(&mut self) -> Result<(), String> {
        self.copy_client_done = true;
        self.send_all(&msg(b'c', &[]))
    }

    pub fn terminate(&mut self) {
        let _ = self.send_all(&msg(b'X', &[]));
        self.terminate_sent = true;
    }

    /// PQsetClientEncoding (fe-connect.c:7736): a SET on the wire; false is
    /// C's -1. The server's ParameterStatus report updates client_encoding().
    pub fn set_client_encoding(&mut self, encoding: &str) -> PgResult<bool> {
        if !self.conn_ok {
            return Ok(false);
        }
        let res = self.exec(&format!("set client_encoding to '{encoding}'"))?;
        Ok(res.status == ExecStatus::CommandOk)
    }

    // PQconnectPoll CONNECTION_CHECK_TARGET (fe-connect.c:4380): the
    // server's default_transaction_read_only / in_hot_standby reports (or a
    // SHOW / pg_is_in_recovery() round trip when it sent none) decide whether
    // this host satisfies target_session_attrs. A rejected host is told
    // Terminate and the walk moves on; a failed round trip ends the walk.
    fn check_target(&mut self, target: TargetServerType, host_identity: &str) -> PgResult<TargetCheck> {
        let reported = |conn: &PgConn, name: &str| conn.parameter_status(name).map(|v| v == "on");
        match target {
            TargetServerType::Any => {}
            TargetServerType::ReadWrite | TargetServerType::ReadOnly => {
                let mut read_only = reported(self, "default_transaction_read_only");
                let mut hot_standby = reported(self, "in_hot_standby");
                if read_only.is_none() || hot_standby.is_none() {
                    match self.target_probe("SHOW transaction_read_only", host_identity)? {
                        Ok(Some(v)) => {
                            read_only = Some(v.starts_with("on"));
                            hot_standby = read_only;
                        }
                        Ok(None) => return Ok(self.reject_target(host_identity, "\"SHOW transaction_read_only\" failed")),
                        Err(e) => return Ok(TargetCheck::Fail(e)),
                    }
                }
                let read_only_server = read_only == Some(true) || hot_standby == Some(true);
                let (wrong, why) = if target == TargetServerType::ReadWrite {
                    (read_only_server, "session is read-only")
                } else {
                    (!read_only_server, "session is not read-only")
                };
                if wrong {
                    return Ok(self.reject_target(host_identity, why));
                }
            }
            TargetServerType::Primary | TargetServerType::Standby | TargetServerType::PreferStandby => {
                let mut hot_standby = reported(self, "in_hot_standby");
                if self.server_version < 90000 {
                    hot_standby = Some(false);
                }
                if hot_standby.is_none() {
                    match self.target_probe("SELECT pg_catalog.pg_is_in_recovery()", host_identity)? {
                        Ok(Some(v)) => hot_standby = Some(v.starts_with('t')),
                        Ok(None) => return Ok(self.reject_target(host_identity, "\"SELECT pg_is_in_recovery()\" failed")),
                        Err(e) => return Ok(TargetCheck::Fail(e)),
                    }
                }
                let (wrong, why) = if target == TargetServerType::Primary {
                    (hot_standby == Some(true), "server is in hot standby mode")
                } else {
                    (hot_standby == Some(false), "server is not in hot standby mode")
                };
                if wrong {
                    return Ok(self.reject_target(host_identity, why));
                }
            }
        }
        Ok(TargetCheck::Ok)
    }

    // CONNECTION_CHECK_WRITABLE / CHECK_STANDBY: Ok(Some(value)) for a
    // one-row reply, Ok(None) when the query itself failed (next host),
    // Err(message) when the connection did (error_return).
    fn target_probe(&mut self, query: &str, host_identity: &str) -> PgResult<Result<Option<String>, String>> {
        let res = self.exec(query)?;
        if !self.conn_ok || (res.status == ExecStatus::Error && res.diag.is_none()) {
            return Ok(Err(format!("{host_identity}{}", res.err)));
        }
        if res.status != ExecStatus::TuplesOk || res.rows.len() != 1 {
            return Ok(Ok(None));
        }
        let v = res.rows[0].first().and_then(|c| c.as_deref()).unwrap_or(b"");
        Ok(Ok(Some(String::from_utf8_lossy(v).into_owned())))
    }

    fn reject_target(&mut self, host_identity: &str, why: &str) -> TargetCheck {
        self.terminate();
        TargetCheck::NextHost(format!("{host_identity}{why}"))
    }
}

// pqConnectOptions2 (fe-connect.c:1992): target_session_attrs is validated
// before any socket is opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TargetServerType {
    Any,
    ReadWrite,
    ReadOnly,
    Primary,
    Standby,
    PreferStandby,
}

impl TargetServerType {
    fn parse(value: Option<&str>) -> Result<Self, String> {
        Ok(match value {
            None | Some("any") => Self::Any,
            Some("read-write") => Self::ReadWrite,
            Some("read-only") => Self::ReadOnly,
            Some("primary") => Self::Primary,
            Some("standby") => Self::Standby,
            Some("prefer-standby") => Self::PreferStandby,
            Some(other) => return Err(format!("invalid target_session_attrs value: \"{other}\"")),
        })
    }
}

enum TargetCheck {
    Ok,
    NextHost(String),
    Fail(String),
}

enum CancelSock {
    Tcp(std::net::TcpStream),
    #[cfg(not(target_family = "wasm"))]
    Unix(std::os::unix::net::UnixStream),
}

impl CancelSock {
    fn write_all(&self, buf: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        match self {
            CancelSock::Tcp(s) => (&mut &*s).write_all(buf),
            #[cfg(not(target_family = "wasm"))]
            CancelSock::Unix(s) => (&mut &*s).write_all(buf),
        }
    }

    fn set_nonblocking(&self) {
        match self {
            CancelSock::Tcp(s) => {
                let _ = s.set_nonblocking(true);
            }
            #[cfg(not(target_family = "wasm"))]
            CancelSock::Unix(s) => {
                let _ = s.set_nonblocking(true);
            }
        }
    }

    fn raw_fd(&self) -> RawFd {
        use std::os::fd::AsRawFd;
        match self {
            CancelSock::Tcp(s) => s.as_raw_fd(),
            #[cfg(not(target_family = "wasm"))]
            CancelSock::Unix(s) => s.as_raw_fd(),
        }
    }
}

// Parse ('P') message body: stmt name, query, param-type OIDs (0 = infer;
// zeros are sent for every param when the caller supplied no types, matching
// libpq's paramTypes==NULL arm).
fn parse_body(stmt_name: &str, query: &str, param_types: &[Oid], nparams: usize) -> Vec<u8> {
    let mut b = Vec::with_capacity(stmt_name.len() + query.len() + 4 + 4 * nparams);
    b.extend_from_slice(stmt_name.as_bytes());
    b.push(0);
    b.extend_from_slice(query.as_bytes());
    b.push(0);
    b.extend_from_slice(&(nparams as u16).to_be_bytes());
    for i in 0..nparams {
        let oid = param_types.get(i).copied().map(u32::from).unwrap_or(0);
        b.extend_from_slice(&oid.to_be_bytes());
    }
    b
}

// Bind ('B') message body: all params text (0 param-format codes, libpq's
// paramFormats==NULL), one result-format code 0 (text), exactly
// pqSendQueryGuts with resultFormat=0.
fn bind_body(portal: &str, stmt_name: &str, params: &[Option<&str>]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(portal.as_bytes());
    b.push(0);
    b.extend_from_slice(stmt_name.as_bytes());
    b.push(0);
    b.extend_from_slice(&0u16.to_be_bytes()); // param format codes: none = all text
    b.extend_from_slice(&(params.len() as u16).to_be_bytes());
    for p in params {
        match p {
            None => b.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(v) => {
                b.extend_from_slice(&(v.len() as u32).to_be_bytes());
                b.extend_from_slice(v.as_bytes());
            }
        }
    }
    b.extend_from_slice(&1u16.to_be_bytes()); // one result-format code...
    b.extend_from_slice(&0u16.to_be_bytes()); // ...text
    b
}

// Execute ('E') message body.
fn execute_body(portal: &str, maxrows: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(portal.len() + 5);
    b.extend_from_slice(portal.as_bytes());
    b.push(0);
    b.extend_from_slice(&maxrows.to_be_bytes());
    b
}

// RowDescription ('T') field count. A body shorter than its 2-byte count
// header is malformed.
fn row_desc_nfields(body: &[u8]) -> Result<usize, String> {
    if body.len() < 2 {
        return Err("insufficient data in \"T\" message".to_string());
    }
    Ok(u16::from_be_bytes([body[0], body[1]]) as usize)
}

// DataRow ('D') body: every count and length in it is server-controlled.
// Enforced here: the 2-byte column-count header is present; each column has
// its 4-byte length; a negative length is the NULL marker -1 and nothing
// else; a column never runs past the frame; and the frame has no trailing
// bytes after the last declared column (leftover data means we and the
// server disagree about the row shape — reject, don't guess).
pub(crate) fn parse_data_row(body: &[u8]) -> Result<Vec<Option<Vec<u8>>>, String> {
    let mut cols = Vec::new();
    parse_data_row_borrowed(body, &mut cols)?;
    Ok(cols.into_iter().map(|c| c.map(|s| s.to_vec())).collect())
}

// fe-protocol3.c getAnotherTuple: "unexpected field count in \"D\" message".
fn unexpected_field_count(got: usize, want: usize) -> String {
    format!("unexpected field count in \"D\" message (got {got}, expected {want})")
}

fn parse_data_row_borrowed<'a>(
    body: &'a [u8],
    cols: &mut Vec<Option<&'a [u8]>>,
) -> Result<(), String> {
    const MALFORMED: &str = "insufficient data in \"D\" message";
    if body.len() < 2 {
        return Err(MALFORMED.to_string());
    }
    let ncols = u16::from_be_bytes([body[0], body[1]]) as usize;
    cols.reserve(ncols.min((body.len() - 2) / 4 + 1));
    let mut p = 2usize;
    for _ in 0..ncols {
        let Some(hdr) = body.get(p..p + 4) else {
            return Err(MALFORMED.to_string());
        };
        let len = be_i32(hdr);
        p += 4;
        if len == -1 {
            cols.push(None);
            continue;
        }
        if len < 0 {
            return Err(format!("invalid column length {len} in \"D\" message"));
        }
        // len <= i32::MAX and p <= body.len(): p + len cannot overflow usize.
        let Some(val) = body.get(p..p + len as usize) else {
            return Err(MALFORMED.to_string());
        };
        cols.push(Some(val));
        p += len as usize;
    }
    if p != body.len() {
        return Err("extraneous data in \"D\" message".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conninfo_parse_basic() {
        let opts =
            parse_conninfo("host=/tmp/x port=5433 user=al application_name='my app'").unwrap();
        assert_eq!(opt(&opts, "host"), Some("/tmp/x"));
        assert_eq!(opt(&opts, "port"), Some("5433"));
        assert_eq!(opt(&opts, "application_name"), Some("my app"));
    }

    #[test]
    fn conninfo_parse_quoting() {
        let opts = parse_conninfo(r"password='a\'b' dbname = rep").unwrap();
        assert_eq!(opt(&opts, "password"), Some("a'b"));
        assert_eq!(opt(&opts, "dbname"), Some("rep"));
    }

    #[test]
    fn conninfo_unquoted_backslash_escapes_next_byte_and_drops_a_terminal_one() {
        let opts = parse_conninfo(r"host=a\ b dbname=x\").unwrap();
        assert_eq!(opt(&opts, "host"), Some("a b"));
        assert_eq!(opt(&opts, "dbname"), Some("x"));
        assert!(parse_conninfo(r"host='a\").is_err());
    }

    #[test]
    fn busy_connection_refuses_sends_and_a_new_cycle_clears_the_error() {
        use std::io::Read;
        let (mut conn, mut server) = test_conn();
        server.set_nonblocking(true).unwrap();
        conn.err = "stale".into();
        assert!(conn.send_query("SELECT 1"));
        assert_eq!(conn.error_message(), "");
        assert_eq!(conn.last_query.as_deref(), Some("SELECT 1"));
        assert!(!conn.send_query("SELECT 2"));
        assert_eq!(conn.error_message(), "another command is already in progress");
        assert!(!conn.send_query_params("SELECT 2", &[], &[]));
        assert_eq!(
            conn.error_message(),
            "another command is already in progress\nanother command is already in progress"
        );
        let r = conn.exec("SELECT 3").unwrap();
        assert_eq!(r.status, ExecStatus::Error);
        assert!(r.err.ends_with("another command is already in progress"));
        assert!(conn.pending_results);
        let mut wire = Vec::new();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let _ = server.read_to_end(&mut wire);
        assert_eq!(wire, msg(b'Q', b"SELECT 1\0"), "only the first query reached the server");
        conn.inbuf = msg(b'Z', b"I");
        assert!(conn.get_result().unwrap().is_none());
        assert!(conn.send_query("SELECT 4"));
        assert_eq!(conn.error_message(), "");
    }

    #[test]
    fn idle_input_is_parsed_for_notifies_and_busy_input_is_left_alone() {
        use std::io::Write;
        let (mut conn, mut server) = test_conn();
        let mut body = 42i32.to_be_bytes().to_vec();
        body.extend_from_slice(b"chan\0payload\0");
        server.write_all(&msg(b'A', &body)).unwrap();
        let mut got = None;
        for _ in 0..400 {
            assert!(conn.consume_input());
            assert!(conn.notifies.is_empty());
            got = conn.next_notify();
            if got.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let n = got.unwrap();
        assert_eq!((n.channel.as_str(), n.be_pid, n.extra.as_str()), ("chan", 42, "payload"));
        assert!(conn.next_notify().is_none());

        conn.pending_results = true;
        conn.inbuf = msg(b'A', &body);
        assert!(conn.next_notify().is_none());
        assert_eq!(conn.inbuf.len(), 5 + body.len());
        conn.pending_results = false;
        assert_eq!(conn.next_notify().unwrap().channel, "chan");
        assert!(conn.inbuf.is_empty());
    }

    #[test]
    fn parameter_count_is_capped_like_libpq() {
        let (mut conn, _server) = test_conn();
        let params: Vec<Option<&str>> = vec![Some("1"); PQ_QUERY_PARAM_MAX_LIMIT + 1];
        assert!(!conn.send_query_params("SELECT 1", &[], &params));
        assert_eq!(conn.error_message(), "number of parameters must be between 0 and 65535");
        assert!(!conn.pending_results);
        assert!(!conn.send_query_prepared("s", &params));
        assert!(!conn.send_prepare("s", "SELECT 1", &vec![0; PQ_QUERY_PARAM_MAX_LIMIT + 1]));
    }

    #[test]
    fn error_text_carries_the_cursor_display_and_notices_skip_context() {
        let (mut conn, _server) = test_conn();
        assert!(conn.send_query("SELCT 1 +"));
        let body = b"SERROR\0C42601\0Msyntax error at or near \"SELCT\"\0P1\0\0";
        conn.inbuf = msg(b'E', body);
        conn.inbuf.extend_from_slice(&msg(b'Z', b"I"));
        let r = conn.get_result().unwrap().unwrap();
        assert_eq!(r.status, ExecStatus::Error);
        assert_eq!(
            conn.error_message(),
            "ERROR:  syntax error at or near \"SELCT\"\nLINE 1: SELCT 1 +\n        ^"
        );
        assert_eq!(r.diag.as_ref().unwrap().position, "1");
        assert!(conn.get_result().unwrap().is_none());
        assert!(conn.send_query_prepared("s", &[]));
        conn.inbuf = msg(b'E', body);
        conn.inbuf.extend_from_slice(&msg(b'Z', b"I"));
        conn.get_result().unwrap().unwrap();
        assert_eq!(conn.error_message(), "ERROR:  syntax error at or near \"SELCT\" at character 1");
    }

    #[test]
    fn server_version_parses_every_libpq_shape() {
        assert_eq!(parse_server_version("18.6"), 180006);
        assert_eq!(parse_server_version("9.6.1"), 90601);
        assert_eq!(parse_server_version("8.4.22"), 80422);
        assert_eq!(parse_server_version("10devel"), 100000);
        assert_eq!(parse_server_version("18beta1"), 180000);
        assert_eq!(parse_server_version("9.6devel"), 90600);
        assert_eq!(parse_server_version("18.6 (Debian 18.6-1)"), 180006);
        assert_eq!(parse_server_version("junk"), 0);
        let (mut conn, _server) = test_conn();
        conn.note_async(b'S', b"server_version\08.4.22\0");
        assert_eq!(conn.server_version(), 80422);
    }

    #[test]
    fn int_and_protocol_version_params_validate_like_libpq() {
        assert_eq!(parse_int_param(" 12 ", "keepalives_idle"), Ok(12));
        assert_eq!(parse_int_param("-3", "keepalives_idle"), Ok(-3));
        for bad in ["abc", "12x", "", " ", "+", "99999999999"] {
            assert_eq!(
                parse_int_param(bad, "keepalives_idle"),
                Err(format!("invalid integer value \"{bad}\" for connection option \"keepalives_idle\""))
            );
        }
        let pv = |s: &str| check_protocol_versions(&parse_conninfo(s).unwrap());
        assert_eq!(pv("min_protocol_version=abc"), Err("invalid min_protocol_version value: \"abc\"".into()));
        assert_eq!(pv("max_protocol_version=3.1"), Err("invalid max_protocol_version value: \"3.1\"".into()));
        assert_eq!(
            pv("min_protocol_version=3.2 max_protocol_version=3.0"),
            Err("\"min_protocol_version\" is greater than \"max_protocol_version\"".into())
        );
        assert_eq!(pv("min_protocol_version=3.2"), Ok(()));
        assert_eq!(pv("min_protocol_version=latest max_protocol_version=latest"), Ok(()));
        assert_eq!(pv("max_protocol_version=3.0"), Ok(()));
    }

    #[test]
    fn dial_prefers_hostaddr_and_defaults_to_the_socket_dir() {
        let free = || {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let we = WaitEvents { connect: 0, receive: 0 };
        let p = free();
        let opts = parse_conninfo(&format!("host=/nonexistent hostaddr=127.0.0.1 port={p} user=u")).unwrap();
        let err = connect(opts, &[("user", "u")], we).unwrap().err().unwrap();
        assert!(err.starts_with(&format!("connection to server at \"127.0.0.1\", port {p} failed: ")), "{err}");
        let opts = parse_conninfo(&format!("port={p} user=u")).unwrap();
        let err = connect(opts, &[("user", "u")], we).unwrap().err().unwrap();
        assert!(err.starts_with(&format!("connection to server on socket \"/tmp/.s.PGSQL.{p}\" failed: ")), "{err}");
        assert!(err.ends_with("\n\tIs the server running locally and accepting connections on that socket?"), "{err}");
        let ch = |h: &str, a: &str| ConnHost { host: h.into(), hostaddr: a.into(), port: String::new() };
        assert_eq!(pq_host(&ch("h", "1.2.3.4")), "h");
        assert_eq!(pq_host(&ch("", "1.2.3.4")), "1.2.3.4");
        assert_eq!(pq_host(&ch("", "")), "/tmp");
    }

    #[test]
    fn tcp_options_are_validated_after_the_dial() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let p = listener.local_addr().unwrap().port();
        let we = WaitEvents { connect: 0, receive: 0 };
        for (o, what) in [("keepalives_idle=abc", "keepalives_idle"), ("keepalives=x", "keepalives"), ("tcp_user_timeout=1.5", "tcp_user_timeout")] {
            let opts = parse_conninfo(&format!("hostaddr=127.0.0.1 port={p} user=u {o}")).unwrap();
            let err = connect(opts, &[("user", "u")], we).unwrap().err().unwrap();
            let (_, v) = o.split_once('=').unwrap();
            assert_eq!(
                err,
                format!("connection to server at \"127.0.0.1\", port {p} failed: invalid integer value \"{v}\" for connection option \"{what}\"")
            );
        }
        let s = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        use std::os::fd::AsRawFd;
        let opts = parse_conninfo("keepalives_idle=30 keepalives_interval=5 keepalives_count=3 tcp_user_timeout=1000").unwrap();
        assert_eq!(setup_tcp_socket(s.as_raw_fd(), &opts), Ok(()));
        let mut on = 0i32;
        let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
        // SAFETY: s is an open socket; `on` receives an int.
        let rc = unsafe { libc::getsockopt(s.as_raw_fd(), libc::SOL_SOCKET, libc::SO_KEEPALIVE, (&mut on as *mut i32).cast(), &mut len) };
        assert_eq!((rc, on != 0), (0, true));
        let opts = parse_conninfo("keepalives=0").unwrap();
        let s2 = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        assert_eq!(setup_tcp_socket(s2.as_raw_fd(), &opts), Ok(()));
        let rc = unsafe { libc::getsockopt(s2.as_raw_fd(), libc::SOL_SOCKET, libc::SO_KEEPALIVE, (&mut on as *mut i32).cast(), &mut len) };
        assert_eq!((rc, on), (0, 0));
    }

    #[test]
    fn db_startup_params_forward_replication_and_fallback_application_name() {
        let keys = |s: &str| {
            db_startup_params(&parse_conninfo(s).unwrap(), None)
                .into_iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            keys("user=u dbname=d replication=database options=-cx=1 fallback_application_name=fb client_encoding=UTF8"),
            ["user=u", "database=d", "replication=database", "options=-cx=1", "application_name=fb", "client_encoding=UTF8"]
        );
        assert_eq!(keys("user=u dbname=d application_name=a fallback_application_name=fb"), ["user=u", "database=d", "application_name=a"]);
        assert_eq!(keys("user=u dbname=d application_name='' fallback_application_name=fb"), ["user=u", "database=d"]);
        assert_eq!(PG_WAIT_CLIENT, 0x0600_0000);
    }

    #[test]
    fn conninfo_parse_last_dup_wins() {
        let opts = parse_conninfo("host=a host=b").unwrap();
        assert_eq!(opt(&opts, "host"), Some("b"));
    }

    #[test]
    fn conninfo_parse_errors() {
        assert!(parse_conninfo("hostonly").err().unwrap().contains("missing \"=\""));
        assert!(parse_conninfo("host='x").err().unwrap().contains("unterminated"));
    }

    // libpq dispatches "postgresql://" / "postgres://" strings to
    // conninfo_uri_parse (fe-connect.c uri_prefix_length); that parser is
    // unported, so the URI form must be refused by name rather than fall
    // into the keyword=value parser (audit-18.6 b045 row 7).
    #[test]
    fn conninfo_uri_routes_to_uri_parser() {
        // fe-connect.c parse_connection_string: both designators route to
        // conninfo_uri_parse, and resolve_conninfo layers the defaults on.
        let o = parse_conninfo("postgresql://user:secret@localhost:5432/postgres").unwrap();
        assert_eq!(opt(&o, "user"), Some("user"));
        assert_eq!(opt(&o, "password"), Some("secret"));
        assert_eq!(opt(&o, "host"), Some("localhost"));
        assert_eq!(opt(&o, "port"), Some("5432"));
        assert_eq!(opt(&o, "dbname"), Some("postgres"));
        let o = parse_conninfo("postgres://localhost/db").unwrap();
        assert_eq!(opt(&o, "host"), Some("localhost"));
        assert_eq!(opt(&o, "dbname"), Some("db"));
        let o = resolve_conninfo("postgresql:///postgres?host=/tmp&port=5433").unwrap();
        assert_eq!(opt(&o, "host"), Some("/tmp"));
        assert_eq!(opt(&o, "port"), Some("5433"));
        assert_eq!(opt(&o, "dbname"), Some("postgres"));
        assert_eq!(opt(&o, "sslmode"), Some("prefer"));
        // Not a prefix match: a keyword that merely contains the designator.
        assert!(parse_conninfo("dbname=postgresql://x").is_ok());
    }

    #[test]
    fn conninfo_rejects_unknown_keyword() {
        assert_eq!(
            resolve_conninfo("dbname=x frobnicate=1").err().unwrap(),
            "invalid connection option \"frobnicate\""
        );
    }

    #[test]
    fn port_validation() {
        let parse = |s: &str| validate_port(&parse_conninfo(s).unwrap());
        assert_eq!(parse("port=-1").err().unwrap(), "invalid port number: \"-1\"");
        assert_eq!(parse("port=70000").err().unwrap(), "invalid port number: \"70000\"");
        assert_eq!(parse("port=0").err().unwrap(), "invalid port number: \"0\"");
        assert_eq!(
            parse("port=1foo").err().unwrap(),
            "invalid integer value \"1foo\" for connection option \"port\""
        );
        assert_eq!(parse("").unwrap(), 5432);
        assert_eq!(parse("port= 5433 ").unwrap(), 5433);
    }

    // pqConnectOptions2's host/hostaddr/port list matching.
    #[test]
    fn multi_host_lists() {
        let hosts = |s: &str| conn_hosts(&parse_conninfo(s).unwrap());
        let h = hosts("host=a,b port=1,2").unwrap();
        assert_eq!(h.len(), 2);
        assert_eq!((h[0].host.as_str(), h[0].port.as_str()), ("a", "1"));
        assert_eq!((h[1].host.as_str(), h[1].port.as_str()), ("b", "2"));
        let h = hosts("host=a,b port=7").unwrap();
        assert_eq!((h[0].port.as_str(), h[1].port.as_str()), ("7", "7"));
        let h = hosts("hostaddr=1.1.1.1,2.2.2.2").unwrap();
        assert_eq!(
            (h[1].hostaddr.as_str(), h[1].host.as_str(), h[1].port.as_str()),
            ("2.2.2.2", "", "")
        );
        assert_eq!(
            hosts("host=a,b port=1,2,3").unwrap_err(),
            "could not match 3 port numbers to 2 hosts"
        );
        assert_eq!(
            hosts("host=a,b hostaddr=1.1.1.1").unwrap_err(),
            "could not match 2 host names to 1 hostaddr values"
        );
        assert_eq!(hosts("").unwrap().len(), 1);
        assert_eq!(
            validate_port(&parse_conninfo("port=1,2").unwrap()).unwrap_err(),
            "could not match 2 port numbers to 1 hosts"
        );
    }

    // The host walk: every refused host leaves its own line, in order.
    #[test]
    fn connect_reports_every_host_attempt() {
        let free = || {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let (p1, p2) = (free(), free());
        let opts =
            parse_conninfo(&format!("host=127.0.0.1,127.0.0.1 port={p1},{p2} user=u")).unwrap();
        let err = connect(
            opts,
            &[("user", "u"), ("database", "u")],
            WaitEvents { connect: 0, receive: 0 },
        )
        .unwrap()
        .err()
        .unwrap();
        // connectFailureMessage (fe-connect.c:2681): the OS error line and
        // libpq's hint line per refused address.
        let lines: Vec<&str> = err.split('\n').collect();
        assert_eq!(lines.len(), 4, "{err}");
        let prefix = |p: u16| format!("connection to server at \"127.0.0.1\", port {p} failed: ");
        assert!(lines[0].starts_with(&prefix(p1)), "{err}");
        assert!(lines[2].starts_with(&prefix(p2)), "{err}");
        let hint = "\tIs the server running on that host and accepting TCP/IP connections?";
        assert_eq!((lines[1], lines[3]), (hint, hint), "{err}");
    }

    #[test]
    fn cancel_dial_is_bounded_by_the_deadline() {
        let start = std::time::Instant::now();
        let deadline_ns = pg_clock::mono_ns() + 300_000_000;
        assert!(tcp_connect_deadline("10.255.255.1", 9, deadline_ns).is_err());
        assert!(start.elapsed() < std::time::Duration::from_secs(10), "{:?}", start.elapsed());
        let r = tcp_connect_deadline("127.0.0.1", 9, 0);
        assert_eq!(r.unwrap_err(), "cancel request timed out");
    }

    #[test]
    fn msg_framing() {
        let m = msg(b'Q', b"SELECT 1\0");
        assert_eq!(m[0], b'Q');
        assert_eq!(be_i32(&m[1..5]) as usize, m.len() - 1);
    }

    #[test]
    fn parse_body_shape() {
        // Unnamed statement, 2 params, no explicit types -> two zero OIDs
        // (libpq paramTypes==NULL).
        let b = parse_body("", "SELECT $1, $2", &[], 2);
        assert_eq!(&b[..2], b"\0S");
        let ntypes = u16::from_be_bytes([b[b.len() - 10], b[b.len() - 9]]);
        assert_eq!(ntypes, 2);
        assert_eq!(&b[b.len() - 8..], &[0, 0, 0, 0, 0, 0, 0, 0]);
        // Named + explicit types (PQprepare).
        let b = parse_body("st1", "SELECT $1", &[Oid::from(23u32)], 1);
        assert!(b.starts_with(b"st1\0SELECT $1\0"));
        assert_eq!(&b[b.len() - 6..], &[0, 1, 0, 0, 0, 23]);
    }

    #[test]
    fn bind_body_shape() {
        let b = bind_body("", "st1", &[Some("42"), None]);
        // portal "" NUL, stmt "st1" NUL, 0 param-format codes, 2 params:
        // "42" (len 2), NULL (-1); then 1 result-format code = 0 (text).
        let mut want: Vec<u8> = Vec::new();
        want.extend_from_slice(b"\0st1\0");
        want.extend_from_slice(&0u16.to_be_bytes());
        want.extend_from_slice(&2u16.to_be_bytes());
        want.extend_from_slice(&2u32.to_be_bytes());
        want.extend_from_slice(b"42");
        want.extend_from_slice(&(-1i32).to_be_bytes());
        want.extend_from_slice(&1u16.to_be_bytes());
        want.extend_from_slice(&0u16.to_be_bytes());
        assert_eq!(b, want);
    }

    #[test]
    fn execute_body_shape() {
        assert_eq!(execute_body("", 0), vec![0, 0, 0, 0, 0]);
    }

    #[test]
    fn error_fields() {
        let mut body = Vec::new();
        body.extend_from_slice(b"SFATAL\0");
        body.extend_from_slice(b"C57P01\0");
        body.extend_from_slice(b"Mgoodbye\0");
        body.push(0);
        assert_eq!(parse_error_fields(&body, None), "FATAL:  goodbye");
        let d = parse_diag(&body);
        assert_eq!(d.sqlstate, "57P01");
        assert_eq!(d.severity, "FATAL");
        assert_eq!(d.primary, "goodbye");
    }

    #[test]
    fn service_file_syntax_error() {
        let dir = std::env::temp_dir().join(format!("pgclient-svc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("pg_service.conf");
        std::fs::write(&f, "# c\n\n[test_ldap]\nldap://127.0.0.1:9/base?attribute?one?filter\n")
            .unwrap();
        let mut opts = Vec::new();
        let mut found = false;
        let err = super::conninfo::parse_service_file(
            f.to_str().unwrap(),
            "test_ldap",
            &mut opts,
            &mut found,
        )
        .err().unwrap();
        assert_eq!(
            err,
            format!("syntax error in service file \"{}\", line 4", f.to_str().unwrap())
        );
        std::fs::write(&f, "[svc]\nhost=h1\nport=5455\n[other]\nhost=h2\n").unwrap();
        let mut opts = vec![("port".to_string(), "1111".to_string())];
        let mut found = false;
        super::conninfo::parse_service_file(f.to_str().unwrap(), "svc", &mut opts, &mut found)
            .unwrap();
        assert!(found);
        assert_eq!(opt(&opts, "host"), Some("h1"));
        assert_eq!(opt(&opts, "port"), Some("1111"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- hostile-server framing / DataRow decoding ----
    //
    // Every byte below is something a broken or malicious server could put
    // on the wire; none may panic, all must surface as connection errors
    // (or be safely dropped where the message is advisory).

    // A PgConn over a real localhost socketpair, with the server end kept
    // alive so sends succeed. Tests preload `inbuf` directly; as long as a
    // complete (or provably invalid) message is buffered, the read loops
    // never touch the socket.
    fn test_conn() -> (PgConn, std::net::TcpStream) {
        use std::os::fd::AsRawFd;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::net::TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        client.set_nonblocking(true).unwrap();
        let fd = client.as_raw_fd();
        let conn = PgConn {
            _stream: Stream::Tcp(client),
            fd,
            external_fd: false,
            target: DialTarget::Tcp("127.0.0.1".into(), addr.port()),
            inbuf: Vec::new(),
            inpos: 0,
            conn_ok: true,
            terminate_sent: false,
            in_copy: false,
            copy_server_done: false,
            copy_client_done: false,
            peer: None,
            err_prescanned: false,
            pending_results: false,
            txn_status: b'I',
            err: String::new(),
            opts: Vec::new(),
            display_host: "127.0.0.1".into(),
            display_port: addr.port() as i32,
            be_pid: 0,
            be_key: Vec::new(),
            server_version: 0,
            used_password: false,
            last_sqlstate: String::new(),
            params: Vec::new(),
            notifies: std::collections::VecDeque::new(),
            last_query: None,
            we: WaitEvents { connect: 0, receive: 0 },
        };
        (conn, server)
    }

    fn frame(t: u8, wire_len: i32, body: &[u8]) -> Vec<u8> {
        let mut f = vec![t];
        f.extend_from_slice(&wire_len.to_be_bytes());
        f.extend_from_slice(body);
        f
    }

    #[test]
    fn frame_negative_length_is_connection_error() {
        let (mut conn, _srv) = test_conn();
        conn.inbuf = frame(b'Z', -1, b"");
        let err = conn.next_message().err().unwrap();
        assert!(err.contains("lost synchronization"), "{err}");
        assert!(err.contains("length -1"), "{err}");
        assert!(conn.connection_bad());
    }

    #[test]
    fn frame_length_below_protocol_minimum_is_connection_error() {
        // The wire length includes its own 4 bytes; 0 and 3 are both
        // impossible and previously became huge usizes.
        for bad in [0i32, 3] {
            let (mut conn, _srv) = test_conn();
            conn.inbuf = frame(b'D', bad, b"xxxx");
            assert!(conn.next_message().is_err());
            assert!(conn.connection_bad());
        }
    }

    #[test]
    fn frame_absurdly_large_length_is_connection_error() {
        let (mut conn, _srv) = test_conn();
        conn.inbuf = frame(b'D', i32::MAX, b"");
        assert!(conn.next_message().is_err());
        assert!(conn.connection_bad());
    }

    #[test]
    fn frame_incomplete_blocks_without_error() {
        let (mut conn, _srv) = test_conn();
        // Header claims 10 more body bytes than buffered: not an error,
        // just not there yet.
        conn.inbuf = frame(b'D', 14, b"abc");
        assert!(matches!(conn.next_message(), Ok(None)));
        assert!(!conn.connection_bad());
        // Short of even a full header: same.
        conn.inbuf = vec![b'D', 0, 0];
        conn.inpos = 0;
        assert!(matches!(conn.next_message(), Ok(None)));
    }

    #[test]
    fn frame_valid_roundtrip_still_works() {
        let (mut conn, _srv) = test_conn();
        conn.inbuf = msg(b'C', b"SELECT 1\0");
        let (t, body) = conn.next_message().unwrap().unwrap();
        assert_eq!(t, b'C');
        assert_eq!(body, b"SELECT 1\0");
        assert!(matches!(conn.next_message(), Ok(None)));
    }

    #[test]
    fn datarow_valid_values_and_null() {
        let mut body = 2u16.to_be_bytes().to_vec();
        body.extend_from_slice(&2i32.to_be_bytes());
        body.extend_from_slice(b"42");
        body.extend_from_slice(&(-1i32).to_be_bytes());
        let cols = parse_data_row(&body).unwrap();
        assert_eq!(cols, vec![Some(b"42".to_vec()), None]);
    }

    #[test]
    fn datarow_short_count_header_rejected() {
        assert!(parse_data_row(b"").is_err());
        assert!(parse_data_row(&[0]).is_err());
    }

    #[test]
    fn datarow_missing_column_length_rejected() {
        // Declares 1 column but has no 4-byte length word.
        let body = 1u16.to_be_bytes().to_vec();
        assert!(parse_data_row(&body).err().unwrap().contains("insufficient data"));
        // Only half a length word.
        let mut body = 1u16.to_be_bytes().to_vec();
        body.extend_from_slice(&[0, 0]);
        assert!(parse_data_row(&body).is_err());
    }

    #[test]
    fn datarow_truncated_column_rejected() {
        // Column claims 5 bytes, frame carries 2.
        let mut body = 1u16.to_be_bytes().to_vec();
        body.extend_from_slice(&5i32.to_be_bytes());
        body.extend_from_slice(b"ab");
        assert!(parse_data_row(&body).err().unwrap().contains("insufficient data"));
    }

    #[test]
    fn datarow_negative_length_other_than_null_marker_rejected() {
        for bad in [-2i32, i32::MIN] {
            let mut body = 1u16.to_be_bytes().to_vec();
            body.extend_from_slice(&bad.to_be_bytes());
            let err = parse_data_row(&body).err().unwrap();
            assert!(err.contains("invalid column length"), "{err}");
        }
    }

    #[test]
    fn datarow_trailing_bytes_rejected() {
        let mut body = 1u16.to_be_bytes().to_vec();
        body.extend_from_slice(&1i32.to_be_bytes());
        body.extend_from_slice(b"x");
        body.push(0xEE); // leftover byte after the declared columns
        assert!(parse_data_row(&body).err().unwrap().contains("extraneous data"));
        // Same via the borrowed (streaming) parser.
        let mut cols = Vec::new();
        assert!(parse_data_row_borrowed(&body, &mut cols).is_err());
    }

    #[test]
    fn datarow_trailing_null_marker_exact_fit_ok() {
        let mut body = 1u16.to_be_bytes().to_vec();
        body.extend_from_slice(&(-1i32).to_be_bytes());
        assert_eq!(parse_data_row(&body).unwrap(), vec![None]);
    }

    #[test]
    fn row_desc_short_body_rejected() {
        assert!(row_desc_nfields(&[]).is_err());
        assert!(row_desc_nfields(&[1]).is_err());
        assert_eq!(row_desc_nfields(&[0, 3, 9, 9]).unwrap(), 3);
    }

    #[test]
    fn get_result_buffered_path_reports_malformed_datarow_as_conn_error() {
        let (mut conn, _srv) = test_conn();
        conn.pending_results = true;
        // RowDescription announcing 1 field, then a DataRow whose column
        // runs past its frame.
        let mut wire = msg(b'T', &1u16.to_be_bytes());
        let mut dbody = 1u16.to_be_bytes().to_vec();
        dbody.extend_from_slice(&100i32.to_be_bytes());
        dbody.extend_from_slice(b"short");
        wire.extend_from_slice(&msg(b'D', &dbody));
        conn.inbuf = wire;
        let r = conn.get_result().unwrap().unwrap();
        assert_eq!(r.status, ExecStatus::Error);
        assert!(r.err.contains("insufficient data"), "{}", r.err);
        assert!(conn.connection_bad());
    }

    // bug_adf1ee39 residual: a COPY OUT started with send_query is reported
    // as COPY_OUT by get_result, and again without reading until drained.
    #[test]
    fn get_result_reports_copy_out_until_drained() {
        let (mut conn, _srv) = test_conn();
        conn.pending_results = true;
        let mut wire = msg(b'H', &[0, 0, 1, 0, 0]);
        wire.extend_from_slice(&msg(b'd', b"1\n"));
        wire.extend_from_slice(&msg(b'c', b""));
        wire.extend_from_slice(&msg(b'C', b"COPY 1\0"));
        wire.extend_from_slice(&msg(b'Z', b"I"));
        conn.inbuf = wire;
        let r = conn.get_result().unwrap().unwrap();
        assert_eq!(r.status, ExecStatus::CopyOut);
        assert!(!conn.connection_bad());
        let r = conn.get_result().unwrap().unwrap();
        assert_eq!(r.status, ExecStatus::CopyOut);
        assert!(!conn.send_query("SELECT 7"));
        assert_eq!(conn.error_message(), "another command is already in progress");
        assert!(matches!(conn.get_copy_data().unwrap(), CopyData::Msg(_)));
        assert!(matches!(conn.get_copy_data().unwrap(), CopyData::End));
        let r = conn.get_result().unwrap().unwrap();
        assert_eq!(r.status, ExecStatus::CommandOk);
        assert_eq!(r.cmd_tag, "COPY 1");
        assert!(conn.get_result().unwrap().is_none());
    }

    #[test]
    fn get_result_buffered_path_reports_short_rowdesc_as_conn_error() {
        let (mut conn, _srv) = test_conn();
        conn.pending_results = true;
        conn.inbuf = msg(b'T', &[7]); // 1-byte RowDescription body
        let r = conn.get_result().unwrap().unwrap();
        assert_eq!(r.status, ExecStatus::Error);
        assert!(conn.connection_bad());
    }

    #[test]
    fn streaming_path_reports_malformed_datarow_before_sink_sees_it() {
        struct CountSink {
            starts: usize,
            rows: usize,
        }
        impl RowSink for CountSink {
            fn result_start(&mut self, _c: &PgConn, _n: usize) -> PgResult<()> {
                self.starts += 1;
                Ok(())
            }
            fn row(&mut self, _cols: &[Option<&[u8]>]) -> PgResult<()> {
                self.rows += 1;
                Ok(())
            }
        }
        // exec_streaming runs CHECK_FOR_INTERRUPTS per row; give the seam a
        // no-op body for this test binary (set-once, race-safe via Once).
        static SEAM: std::sync::Once = std::sync::Once::new();
        SEAM.call_once(|| {
            if !postgres_seams::check_for_interrupts::is_installed() {
                postgres_seams::check_for_interrupts::set(|| Ok(()));
            }
        });
        let (mut conn, _srv) = test_conn();
        // Preload the whole hostile conversation; exec_streaming's send of
        // the Query message goes to the live socketpair.
        let mut wire = msg(b'T', &1u16.to_be_bytes());
        let mut dbody = 1u16.to_be_bytes().to_vec();
        dbody.extend_from_slice(&1i32.to_be_bytes());
        dbody.extend_from_slice(b"x");
        dbody.push(0xEE); // trailing byte
        wire.extend_from_slice(&msg(b'D', &dbody));
        conn.inbuf = wire;
        let mut sink = CountSink { starts: 0, rows: 0 };
        let r = conn.exec_streaming("SELECT hostile", &mut sink).unwrap();
        assert_eq!(r.status, ExecStatus::Error);
        assert!(r.err.contains("extraneous data"), "{}", r.err);
        assert_eq!(sink.starts, 0);
        assert_eq!(sink.rows, 0);
        assert!(conn.connection_bad());
    }

    #[test]
    fn is_busy_survives_hostile_length() {
        let (mut conn, _srv) = test_conn();
        conn.pending_results = true;
        conn.inbuf = frame(b'D', -1, b"");
        // Framing loss: report not-busy so get_result runs and errors.
        assert!(!conn.is_busy());
        // Incomplete-but-valid frame: genuinely busy.
        conn.inbuf = frame(b'D', 100, b"partial");
        assert!(conn.is_busy());
        // Complete terminator: not busy.
        conn.inbuf = msg(b'Z', b"I");
        assert!(!conn.is_busy());
    }

    #[test]
    fn get_copy_data_reports_bad_frame() {
        let (mut conn, _srv) = test_conn();
        conn.in_copy = true;
        conn.inbuf = frame(b'd', -5, b"");
        assert!(conn.get_copy_data().is_err());
        assert!(conn.connection_bad());
    }

    // getCopyDataMessage's default arm: a CommandComplete arriving inside a
    // COPY with no CopyDone first (the walsender's shutdown "COPY 0") ends
    // the copy, is NOT consumed, and the next get_result reads it.
    #[test]
    fn get_copy_data_foreign_message_ends_the_copy_and_stays_readable() {
        let (mut conn, _srv) = test_conn();
        conn.in_copy = true;
        conn.pending_results = true;
        let mut buf = frame(b'd', 5, b"x");
        buf.extend_from_slice(&frame(b'C', 11, b"COPY 0\0"));
        conn.inbuf = buf;
        assert!(matches!(conn.get_copy_data(), Ok(CopyData::Msg(m)) if m == b"x"));
        assert!(matches!(conn.get_copy_data(), Ok(CopyData::End)));
        assert!(!conn.connection_bad());
        let r = conn.get_result().unwrap().expect("the CommandComplete is still there");
        assert_eq!(r.status, ExecStatus::CommandOk);
        assert_eq!(r.cmd_tag, "COPY 0");
    }

    #[test]
    fn note_async_tolerates_short_bodies() {
        let (mut conn, _srv) = test_conn();
        conn.note_async(b'A', b"\x01"); // NotificationResponse, 1 byte
        assert!(conn.next_notify().is_none());
        conn.note_async(b'K', b"\x01\x02"); // BackendKeyData, 2 bytes
        assert_eq!(conn.backend_pid(), 0);
        // ParameterStatus with no NUL terminators anywhere.
        conn.note_async(b'S', b"no_nul_here");
        // NotificationResponse whose channel runs to the end un-terminated.
        conn.note_async(b'A', b"\0\0\0\x07chan");
        assert!(conn.next_notify().is_some());
    }

    #[test]
    fn cstr_at_clamps_out_of_range_pos() {
        let (s, next) = cstr_at(b"ab", 5);
        assert_eq!(s, "");
        assert_eq!(next, 3);
    }

    #[test]
    fn sslmode_refuses_tls_required_modes() {
        // No TLS support here, so require/verify-* must be a hard error (no
        // silent cleartext downgrade), matching libpq's no-USE_SSL wording.
        for m in ["require", "verify-ca", "verify-full"] {
            let opts = vec![("sslmode".to_string(), m.to_string())];
            let err = check_sslmode(&opts).err().unwrap();
            assert_eq!(
                err,
                format!("sslmode value \"{m}\" invalid when SSL support is not compiled in")
            );
        }
        // Non-encrypting modes (and the absent/default "prefer") are accepted.
        for m in ["disable", "allow", "prefer"] {
            let opts = vec![("sslmode".to_string(), m.to_string())];
            assert!(check_sslmode(&opts).is_ok(), "sslmode={m:?} should be ok");
        }
        assert!(check_sslmode(&[]).is_ok());
        // An unrecognized value is rejected as invalid; so is an explicitly
        // empty one (fe-connect.c:1767 keeps "" and rejects it).
        for m in ["bogus", ""] {
            let opts = vec![("sslmode".to_string(), m.to_string())];
            assert_eq!(check_sslmode(&opts).err().unwrap(), format!("invalid sslmode value: \"{m}\""));
        }
        let opts = vec![("sslmode".to_string(), "bogus".to_string())];
        assert_eq!(
            check_sslmode(&opts).err().unwrap(),
            "invalid sslmode value: \"bogus\""
        );
    }

    #[test]
    fn transport_options_refused_without_ssl_or_gss() {
        let o = |k: &str, v: &str| vec![(k.to_string(), v.to_string())];
        let ssl = |k: &str, v: &str| {
            format!("{k} value \"{v}\" invalid when SSL support is not compiled in")
        };
        assert_eq!(check_transport_options(&o("sslrootcert", "system")).unwrap_err(), ssl("sslrootcert", "system"));
        assert_eq!(check_transport_options(&o("sslnegotiation", "direct")).unwrap_err(), ssl("sslnegotiation", "direct"));
        assert_eq!(check_transport_options(&o("sslcertmode", "require")).unwrap_err(), ssl("sslcertmode", "require"));
        assert_eq!(
            check_transport_options(&o("gssencmode", "require")).unwrap_err(),
            "gssencmode value \"require\" invalid when GSSAPI support is not compiled in"
        );
        assert_eq!(check_transport_options(&o("sslnegotiation", "x")).unwrap_err(), "invalid sslnegotiation value: \"x\"");
        assert_eq!(check_transport_options(&o("sslcertmode", "x")).unwrap_err(), "invalid sslcertmode value: \"x\"");
        assert_eq!(check_transport_options(&o("gssencmode", "x")).unwrap_err(), "invalid gssencmode value: \"x\"");
        assert_eq!(check_transport_options(&o("channel_binding", "x")).unwrap_err(), "invalid channel_binding value: \"x\"");
        // sslrootcert=system is checked before sslmode, as fe-connect.c does.
        let both = vec![("sslmode".to_string(), "bogus".to_string()), ("sslrootcert".to_string(), "system".to_string())];
        assert_eq!(check_transport_options(&both).unwrap_err(), ssl("sslrootcert", "system"));
        for (k, v) in [
            ("sslrootcert", "/x/ca.crt"), ("sslnegotiation", "postgres"), ("sslcertmode", "disable"),
            ("sslcertmode", "allow"), ("gssencmode", "disable"), ("gssencmode", "prefer"),
            ("channel_binding", "disable"), ("channel_binding", "prefer"),
        ] {
            assert_eq!(check_transport_options(&o(k, v)), Ok(false), "{k}={v}");
        }
        assert_eq!(check_transport_options(&o("channel_binding", "require")), Ok(true));
        assert_eq!(check_transport_options(&[]), Ok(false));
    }

    // connect() must refuse before dialing: an unroutable port proves no
    // socket was opened (a dial would fail with a connection error instead).
    #[test]
    fn connect_refuses_ignored_security_options_before_dial() {
        for (k, v) in [
            ("gssencmode", "require"), ("sslnegotiation", "direct"),
            ("sslcertmode", "require"), ("sslrootcert", "system"),
        ] {
            let opts = vec![
                ("host".to_string(), "127.0.0.1".to_string()),
                ("port".to_string(), "1".to_string()),
                (k.to_string(), v.to_string()),
            ];
            let err = match connect(opts, &[("user", "u")], WaitEvents { connect: 0, receive: 0 }) {
                Ok(Err(e)) => e,
                _ => panic!("{k}={v}: connect must refuse before dialing"),
            };
            assert!(err.contains("not compiled in"), "{k}={v}: {err}");
        }
    }

    // channel_binding=require against a server that asks for a cleartext
    // password: the request is refused and no 'p' message reaches the wire.
    #[test]
    fn channel_binding_require_never_sends_password() {
        use std::io::Read;
        for (areq, want) in [
            (3i32, "channel binding required but not supported by server's authentication request"),
            (5, "channel binding required but not supported by server's authentication request"),
            (0, "channel binding required, but server authenticated client without channel binding"),
            (10, "channel binding required, but SSL not in use"),
        ] {
            let (mut conn, mut srv) = test_conn();
            let mut body = areq.to_be_bytes().to_vec();
            if areq == 5 {
                body.extend_from_slice(&[1, 2, 3, 4]);
            }
            if areq == 10 {
                body.extend_from_slice(b"SCRAM-SHA-256\0\0");
            }
            conn.inbuf = frame(b'R', 4 + body.len() as i32, &body);
            let policy = auth::AuthPolicy::parse(None).unwrap().with_channel_binding_required(true);
            let auth_opts = auth::AuthOptions {
                password: Some("secret"),
                keys: auth::ScramKeys::default(),
                policy,
                passfile: None,
            };
            let err = auth::handshake(&mut conn, "u", &auth_opts).unwrap().unwrap_err();
            assert_eq!(err, want, "areq {areq}");
            srv.set_nonblocking(true).unwrap();
            let mut buf = [0u8; 64];
            match srv.read(&mut buf) {
                Ok(0) => {}
                Ok(n) => panic!("areq {areq}: client sent {n} bytes: {:?}", &buf[..n]),
                Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::WouldBlock),
            }
        }
    }

    #[test]
    fn dispchar_table_shape() {
        assert_eq!(lookup("password").unwrap().dispchar, "*");
        assert_eq!(lookup("sslpassword").unwrap().dispchar, "*");
        assert_eq!(lookup("oauth_client_secret").unwrap().dispchar, "*");
        assert_eq!(lookup("replication").unwrap().dispchar, "D");
        assert_eq!(lookup("scram_client_key").unwrap().dispchar, "D");
        assert_eq!(lookup("sslkeylogfile").unwrap().dispchar, "D");
        assert!(lookup("server").is_none());
        fn lookup(k: &str) -> Option<&'static ConnOption> {
            conninfo::lookup_option(k)
        }
    }

    // libpqwalreceiver.c:365-388 libpqrcv_get_conninfo walks PQconninfo()'s
    // table-ordered copy of the options: debug ('D') and empty options are
    // skipped, and EVERY option whose dispchar carries '*' (password,
    // sslpassword, oauth_client_secret) is shown as "********" — the string
    // lands in pg_stat_wal_receiver.conninfo.
    #[test]
    fn display_conninfo_masks_every_secure_option_in_table_order() {
        let (mut conn, _server) = test_conn();
        conn.opts = [
            ("sslpassword", "keysecret"),
            ("port", "5433"),
            ("oauth_client_secret", "oauthsecret"),
            ("replication", "true"),
            ("host", "primary"),
            ("application_name", ""),
            ("scram_client_key", "abc"),
            ("password", "pwsecret"),
            ("sslkeylogfile", "/tmp/keys"),
            ("user", "rep"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let shown = conn.display_conninfo();
        assert_eq!(
            shown,
            "user=rep password=******** host=primary port=5433 \
             sslpassword=******** oauth_client_secret=********"
        );
        for secret in ["keysecret", "oauthsecret", "pwsecret", "/tmp/keys", "abc"] {
            assert!(!shown.contains(secret), "{secret:?} leaked into {shown:?}");
        }
    }
    // PQsendQueryStart's CONNECTION_BAD arm: every sender refuses with libpq's text.
    // upstream c318777da8b8 (18.4): postgres_fdw: Fix handling of abort-cleanup-failed connections.
    #[test]
    fn dead_connection_refuses_sends_with_libpq_text() {
        let (mut conn, _server) = test_conn();
        conn.conn_ok = false;
        assert!(!conn.send_query("SELECT 1"));
        assert_eq!(conn.error_message(), "no connection to the server");
        assert!(!conn.send_query_params("SELECT $1", &[], &[Some("1")]));
        assert_eq!(conn.error_message(), "no connection to the server");
        assert!(!conn.send_prepare("s", "SELECT 1", &[]));
        assert_eq!(conn.error_message(), "no connection to the server");
        assert!(!conn.send_query_prepared("s", &[]));
        assert_eq!(conn.error_message(), "no connection to the server");
        assert!(!conn.pending_results);
        let r = conn.exec("SELECT 1").unwrap();
        assert_eq!(r.status, ExecStatus::Error);
        assert_eq!(r.err, "no connection to the server");
    }

    // PQgetResult on a lost connection: the error comes back once, then None
    // (libpq goes PGASYNC_IDLE), so a drain-until-None loop cannot spin.
    // upstream c318777da8b8 (18.4): postgres_fdw: Fix handling of abort-cleanup-failed connections.
    #[test]
    fn get_result_on_lost_connection_reports_once_then_idles() {
        fn once_then_none(conn: &mut PgConn, expect: &str) {
            let r = conn.get_result().unwrap().unwrap();
            assert_eq!(r.status, ExecStatus::Error);
            assert!(r.err.contains(expect), "{}", r.err);
            assert!(conn.connection_bad());
            assert!(conn.get_result().unwrap().is_none());
            assert!(conn.get_result().unwrap().is_none());
        }
        // Framing loss (libpq handleSyncLoss).
        let (mut conn, _srv) = test_conn();
        conn.pending_results = true;
        conn.inbuf = frame(b'D', -1, b"");
        once_then_none(&mut conn, "lost synchronization");
        // Server death mid-result (pqReadData EOF), seen by consume_input.
        let (mut conn, srv) = test_conn();
        conn.pending_results = true;
        conn.inbuf = msg(b'T', &1u16.to_be_bytes());
        drop(srv);
        let mut dead = false;
        for _ in 0..400 {
            if !conn.consume_input() {
                dead = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(dead);
        assert_eq!(conn.error_message(), SERVER_CLOSED_MSG);
        once_then_none(&mut conn, "before or while processing the request.\ninvalid socket");
        // Malformed DataRow (proto_error).
        let (mut conn, _srv) = test_conn();
        conn.pending_results = true;
        let mut wire = msg(b'T', &1u16.to_be_bytes());
        let mut dbody = 1u16.to_be_bytes().to_vec();
        dbody.extend_from_slice(&100i32.to_be_bytes());
        dbody.extend_from_slice(b"short");
        wire.extend_from_slice(&msg(b'D', &dbody));
        conn.inbuf = wire;
        once_then_none(&mut conn, "insufficient data");
    }

    fn row_desc(names: &[&str]) -> Vec<u8> {
        let mut b = (names.len() as i16).to_be_bytes().to_vec();
        for n in names {
            b.extend_from_slice(n.as_bytes());
            b.push(0);
            b.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 25, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0, 0]);
        }
        b
    }

    fn data_row(vals: &[&str]) -> Vec<u8> {
        let mut b = (vals.len() as i16).to_be_bytes().to_vec();
        for v in vals {
            b.extend_from_slice(&(v.len() as i32).to_be_bytes());
            b.extend_from_slice(v.as_bytes());
        }
        b
    }

    #[test]
    fn drop_sends_terminate_once_like_pqfinish() {
        use std::io::Read;
        let (conn, mut server) = test_conn();
        drop(conn);
        let mut got = Vec::new();
        server.read_to_end(&mut got).unwrap();
        assert_eq!(got, msg(b'X', &[]));

        let (mut conn, mut server) = test_conn();
        conn.terminate();
        drop(conn);
        let mut got = Vec::new();
        server.read_to_end(&mut got).unwrap();
        assert_eq!(got, msg(b'X', &[]));

        let (mut conn, mut server) = test_conn();
        conn.conn_ok = false;
        drop(conn);
        let mut got = Vec::new();
        server.read_to_end(&mut got).unwrap();
        assert!(got.is_empty());
    }

    fn preload(conn: &mut PgConn, frames: &[Vec<u8>]) {
        conn.inbuf = frames.concat();
        conn.inpos = 0;
    }

    // pqParseInput3 (fe-protocol3.c:220): a RowDescription makes the result
    // PGRES_TUPLES_OK whatever its column count — dblink's rowtype check
    // depends on a zero-column SELECT not passing as a command tag.
    #[test]
    fn zero_column_select_is_tuples_ok() {
        let (mut conn, _srv) = test_conn();
        let reply = [msg(b'T', &row_desc(&[])), msg(b'C', b"SELECT 0\0"), msg(b'Z', b"I")];
        preload(&mut conn, &reply);
        let r = conn.exec("SELECT WHERE false").unwrap();
        assert_eq!((r.status, r.nfields, r.cmd_tag.as_str()), (ExecStatus::TuplesOk, 0, "SELECT 0"));
        preload(&mut conn, &reply);
        assert!(conn.send_query("SELECT WHERE false"));
        let r = conn.get_result().unwrap().unwrap();
        assert_eq!((r.status, r.nfields), (ExecStatus::TuplesOk, 0));
        assert!(conn.get_result().unwrap().is_none());
        preload(&mut conn, &[msg(b'C', b"BEGIN\0"), msg(b'Z', b"T")]);
        let r = conn.exec("BEGIN").unwrap();
        assert_eq!((r.status, r.nfields), (ExecStatus::CommandOk, 0));
    }

    // A FATAL followed by EOF (pg_terminate_backend on the remote): the
    // error buffer carries the FATAL text, libpq's three-line EOF notice and
    // PQgetResult's "invalid socket", and the next send starts a clean cycle.
    #[test]
    fn fatal_then_eof_accumulates_libpqs_error_lines() {
        let (mut conn, srv) = test_conn();
        conn.pending_results = true;
        conn.last_query = Some("SELECT pg_terminate_backend(pg_backend_pid())".into());
        conn.inbuf = msg(
            b'E',
            b"SFATAL\0VFATAL\0C57P01\0Mterminating connection due to administrator command\0\0",
        );
        let r = conn.get_result().unwrap().unwrap();
        assert_eq!(r.status, ExecStatus::Error);
        assert_eq!(r.err, "FATAL:  terminating connection due to administrator command");
        drop(srv);
        let mut dead = false;
        for _ in 0..400 {
            if !conn.consume_input() {
                dead = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(dead);
        let r = conn.get_result().unwrap().unwrap();
        assert_eq!(r.status, ExecStatus::Error);
        assert_eq!(
            r.err,
            "FATAL:  terminating connection due to administrator command\n\
             server closed the connection unexpectedly\n\
             \tThis probably means the server terminated abnormally\n\
             \tbefore or while processing the request.\n\
             invalid socket"
        );
        assert_eq!(conn.error_message(), r.err);
        assert!(conn.get_result().unwrap().is_none());
        let r = conn.exec("SELECT 1").unwrap();
        assert_eq!(r.err, "no connection to the server");
        assert_eq!(conn.error_message(), "no connection to the server");
    }

    fn handshake_with(conn: &mut PgConn, require_auth: Option<&str>) -> Result<(), String> {
        let policy = auth::AuthPolicy::parse(require_auth).unwrap();
        let opts = auth::AuthOptions {
            password: Some("secret"),
            keys: auth::ScramKeys::default(),
            policy,
            passfile: None,
        };
        auth::handshake(conn, "u", &opts).unwrap()
    }

    // PQconnectPoll CONNECTION_AWAITING_RESPONSE (fe-connect.c:4010): until
    // AuthenticationOk only an authentication request or an error is
    // accepted, so a server that skips authentication cannot bypass
    // require_auth by sending ReadyForQuery outright.
    #[test]
    fn ready_for_query_before_authentication_ok_is_rejected() {
        for require_auth in [None, Some("scram-sha-256"), Some("!none"), Some("password")] {
            let (mut conn, _srv) = test_conn();
            preload(&mut conn, &[msg(b'Z', b"I")]);
            assert_eq!(
                handshake_with(&mut conn, require_auth),
                Err("expected authentication request from server, but received Z".into()),
                "{require_auth:?}"
            );
        }
        let (mut conn, _srv) = test_conn();
        preload(&mut conn, &[msg(b'S', b"server_version\x0018.6\0"), msg(b'Z', b"I")]);
        assert_eq!(
            handshake_with(&mut conn, None),
            Err("expected authentication request from server, but received S".into())
        );
        let (mut conn, _srv) = test_conn();
        preload(&mut conn, &[msg(b'R', &0i32.to_be_bytes()), msg(b'R', &0i32.to_be_bytes())]);
        assert_eq!(
            handshake_with(&mut conn, None),
            Err("unexpected response from server; first received character was \"R\"".into())
        );
        let (mut conn, _srv) = test_conn();
        preload(
            &mut conn,
            &[
                msg(b'R', &0i32.to_be_bytes()),
                msg(b'S', b"server_version\x0018.6\0"),
                msg(b'K', &[0, 0, 0, 7, 0, 0, 0, 9]),
                msg(b'N', b"SNOTICE\0Mhello\0\0"),
                msg(b'Z', b"I"),
            ],
        );
        assert_eq!(handshake_with(&mut conn, None), Ok(()));
        assert_eq!(conn.parameter_status("server_version"), Some("18.6"));
    }

    // pqConnectOptions2 (fe-connect.c:1992) validates target_session_attrs
    // before any socket is opened (port 1 would otherwise be dialed).
    #[test]
    fn target_session_attrs_is_validated_before_dial() {
        let opts = parse_conninfo("host=127.0.0.1 port=1 user=u target_session_attrs=bogus").unwrap();
        let err = connect(opts, &[("user", "u")], WaitEvents { connect: 0, receive: 0 }).unwrap().err().unwrap();
        assert_eq!(err, "invalid target_session_attrs value: \"bogus\"");
        assert_eq!(TargetServerType::parse(Some("")).unwrap_err(), "invalid target_session_attrs value: \"\"");
        assert_eq!(TargetServerType::parse(None), Ok(TargetServerType::Any));
        assert_eq!(TargetServerType::parse(Some("any")), Ok(TargetServerType::Any));
        assert_eq!(TargetServerType::parse(Some("read-write")), Ok(TargetServerType::ReadWrite));
        assert_eq!(TargetServerType::parse(Some("read-only")), Ok(TargetServerType::ReadOnly));
        assert_eq!(TargetServerType::parse(Some("primary")), Ok(TargetServerType::Primary));
        assert_eq!(TargetServerType::parse(Some("standby")), Ok(TargetServerType::Standby));
        assert_eq!(TargetServerType::parse(Some("prefer-standby")), Ok(TargetServerType::PreferStandby));
    }

    fn next_host(check: TargetCheck) -> String {
        match check {
            TargetCheck::NextHost(e) => e,
            TargetCheck::Ok => panic!("host accepted"),
            TargetCheck::Fail(e) => panic!("walk ended: {e}"),
        }
    }

    // CONNECTION_CHECK_TARGET (fe-connect.c:4380): the startup reports decide;
    // without them SHOW transaction_read_only / pg_is_in_recovery() are asked,
    // and a failed probe moves to the next host with C's message.
    #[test]
    fn check_target_reads_reports_then_probes_the_server() {
        let id = "connection to server at \"h\", port 1 failed: ";
        let (mut conn, _srv) = test_conn();
        conn.server_version = 180006;
        conn.params.push(("in_hot_standby".into(), "on".into()));
        conn.params.push(("default_transaction_read_only".into(), "off".into()));
        assert!(matches!(conn.check_target(TargetServerType::Any, id).unwrap(), TargetCheck::Ok));
        assert!(matches!(conn.check_target(TargetServerType::Standby, id).unwrap(), TargetCheck::Ok));
        assert!(matches!(conn.check_target(TargetServerType::PreferStandby, id).unwrap(), TargetCheck::Ok));
        assert!(matches!(conn.check_target(TargetServerType::ReadOnly, id).unwrap(), TargetCheck::Ok));
        assert_eq!(next_host(conn.check_target(TargetServerType::Primary, id).unwrap()), format!("{id}server is in hot standby mode"));
        assert_eq!(next_host(conn.check_target(TargetServerType::ReadWrite, id).unwrap()), format!("{id}session is read-only"));
        conn.params.clear();
        conn.params.push(("in_hot_standby".into(), "off".into()));
        conn.params.push(("default_transaction_read_only".into(), "off".into()));
        assert!(matches!(conn.check_target(TargetServerType::Primary, id).unwrap(), TargetCheck::Ok));
        assert!(matches!(conn.check_target(TargetServerType::ReadWrite, id).unwrap(), TargetCheck::Ok));
        assert_eq!(next_host(conn.check_target(TargetServerType::Standby, id).unwrap()), format!("{id}server is not in hot standby mode"));
        assert_eq!(next_host(conn.check_target(TargetServerType::ReadOnly, id).unwrap()), format!("{id}session is not read-only"));

        let (mut conn, _srv) = test_conn();
        conn.server_version = 180006;
        let show = |v: &str| {
            [msg(b'T', &row_desc(&["transaction_read_only"])), msg(b'D', &data_row(&[v])), msg(b'C', b"SHOW\0"), msg(b'Z', b"I")]
        };
        preload(&mut conn, &show("on"));
        assert_eq!(next_host(conn.check_target(TargetServerType::ReadWrite, id).unwrap()), format!("{id}session is read-only"));
        preload(&mut conn, &show("off"));
        assert!(matches!(conn.check_target(TargetServerType::ReadWrite, id).unwrap(), TargetCheck::Ok));
        let recovery = |v: &str| {
            [msg(b'T', &row_desc(&["pg_is_in_recovery"])), msg(b'D', &data_row(&[v])), msg(b'C', b"SELECT 1\0"), msg(b'Z', b"I")]
        };
        preload(&mut conn, &recovery("t"));
        assert_eq!(next_host(conn.check_target(TargetServerType::Primary, id).unwrap()), format!("{id}server is in hot standby mode"));
        preload(&mut conn, &recovery("f"));
        assert!(matches!(conn.check_target(TargetServerType::Standby, id).unwrap(), TargetCheck::NextHost(_)));
        preload(&mut conn, &recovery("f"));
        assert!(matches!(conn.check_target(TargetServerType::Primary, id).unwrap(), TargetCheck::Ok));
        preload(&mut conn, &[msg(b'E', b"SERROR\0C42501\0Mdenied\0\0"), msg(b'Z', b"I")]);
        assert_eq!(next_host(conn.check_target(TargetServerType::ReadOnly, id).unwrap()), format!("{id}\"SHOW transaction_read_only\" failed"));
        preload(&mut conn, &[msg(b'E', b"SERROR\0C42501\0Mdenied\0\0"), msg(b'Z', b"I")]);
        assert_eq!(next_host(conn.check_target(TargetServerType::Standby, id).unwrap()), format!("{id}\"SELECT pg_is_in_recovery()\" failed"));
        // Servers before 9.0 have no standby mode: no probe is sent.
        conn.server_version = 80400;
        assert!(matches!(conn.check_target(TargetServerType::Primary, id).unwrap(), TargetCheck::Ok));
    }

    #[test]
    fn empty_transport_option_values_are_invalid() {
        for k in ["sslmode", "channel_binding", "sslnegotiation", "sslcertmode", "gssencmode"] {
            let opts = vec![(k.to_string(), String::new())];
            assert_eq!(check_transport_options(&opts).unwrap_err(), format!("invalid {k} value: \"\""));
        }
        assert!(check_transport_options(&[]).is_ok());
    }

    #[test]
    fn ssl_protocol_versions_are_validated_like_libpq() {
        let o = |min: &str, max: &str| {
            vec![
                ("ssl_min_protocol_version".to_string(), min.to_string()),
                ("ssl_max_protocol_version".to_string(), max.to_string()),
            ]
        };
        assert_eq!(
            check_transport_options(&o("bogus", "")).unwrap_err(),
            "invalid \"ssl_min_protocol_version\" value: \"bogus\""
        );
        assert_eq!(
            check_transport_options(&o("", "bogus")).unwrap_err(),
            "invalid \"ssl_max_protocol_version\" value: \"bogus\""
        );
        assert_eq!(check_transport_options(&o("TLSv1.3", "TLSv1.2")).unwrap_err(), "invalid SSL protocol version range");
        assert_eq!(check_transport_options(&o("TLSv1.2", "TLSv1")).unwrap_err(), "invalid SSL protocol version range");
        assert!(check_transport_options(&o("tlsv1.2", "TLSV1.3")).is_ok());
        assert!(check_transport_options(&o("TLSv1", "TLSv1")).is_ok());
        assert!(check_transport_options(&o("TLSv1.1", "TLSv1.1")).is_ok());
        assert!(check_transport_options(&o("", "TLSv1")).is_ok());
    }

    #[test]
    fn load_balance_hosts_is_validated_and_random_permutes() {
        let o = |v: &str| vec![("load_balance_hosts".to_string(), v.to_string())];
        assert_eq!(load_balance_random(&o("bogus")).unwrap_err(), "invalid load_balance_hosts value: \"bogus\"");
        assert_eq!(load_balance_random(&o("")).unwrap_err(), "invalid load_balance_hosts value: \"\"");
        assert!(!load_balance_random(&[]).unwrap());
        assert!(!load_balance_random(&o("disable")).unwrap());
        assert!(load_balance_random(&o("random")).unwrap());
        let ordered: Vec<u32> = (0..16).collect();
        let mut v = ordered.clone();
        shuffle(&mut v, &mut pg_prng::PgPrng::seeded(285));
        let mut sorted = v.clone();
        sorted.sort();
        assert_eq!(sorted, ordered);
        assert_ne!(v, ordered);
        let mut single = vec![7u32];
        shuffle(&mut single, &mut pg_prng::PgPrng::seeded(1));
        assert_eq!(single, vec![7]);
    }

    #[test]
    fn abstract_socket_hosts_dial_as_unix_sockets() {
        assert!(is_unixsock_path("@abstract"));
        assert!(is_unixsock_path("/tmp"));
        assert!(!is_unixsock_path("localhost"));
        let e = unix_connect("@pgclient-b285-nonexistent/.s.PGSQL.1").err().expect("nothing listens there");
        assert!(
            e.starts_with("connection to server on socket \"@pgclient-b285-nonexistent/.s.PGSQL.1\" failed: "),
            "{e}"
        );
        assert!(e.ends_with("\n\tIs the server running locally and accepting connections on that socket?"), "{e}");
    }

    #[test]
    fn is_busy_parses_a_buffered_error_response_once() {
        let (mut conn, _srv) = test_conn();
        conn.pending_results = true;
        let mut wire = msg(b'E', b"SERROR\0C42703\0Mboom\0\0");
        wire.extend(msg(b'Z', b"I"));
        conn.inbuf = wire;
        assert!(!conn.is_busy());
        assert_eq!(conn.error_message(), "ERROR:  boom");
        assert!(!conn.is_busy());
        assert_eq!(conn.error_message(), "ERROR:  boom");
        let r = conn.get_result().unwrap().unwrap();
        assert_eq!(r.status, ExecStatus::Error);
        assert_eq!(r.err, "ERROR:  boom");
        assert_eq!(conn.error_message(), "ERROR:  boom");
        assert!(conn.get_result().unwrap().is_none());
        assert!(!conn.err_prescanned);
        // A partial row ahead of anything else: busy, and nothing is parsed.
        conn.pending_results = true;
        conn.inbuf = frame(b'D', 100, b"partial");
        assert!(conn.is_busy());
        assert_eq!(conn.error_message(), "ERROR:  boom");
    }

    #[test]
    fn cancel_dials_the_connected_peer_not_a_fresh_lookup() {
        let peer: std::net::SocketAddr = "10.255.255.1:9".parse().unwrap();
        assert_eq!(cancel_addrs("localhost", 5432, Some(peer)).unwrap(), vec![peer]);
        let (mut conn, _srv) = test_conn();
        conn.peer = Some(peer);
        conn.target = DialTarget::Tcp("no-such-host.invalid".into(), 5432);
        assert_eq!(conn.cancel(0).unwrap(), Some("cancel request timed out".into()));
        conn.peer = None;
        assert!(conn.cancel(0).unwrap().unwrap().starts_with("could not translate host name \"no-such-host.invalid\""));
    }

    #[test]
    fn host_addr_text_drops_the_ipv6_scope() {
        let mut sa = ip::SockAddr::zeroed();
        // SAFETY: zeroed, correctly sized sockaddr structs copied into
        // sockaddr_storage-sized bytes.
        unsafe {
            let mut sin6: libc::sockaddr_in6 = std::mem::zeroed();
            sin6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            sin6.sin6_addr.s6_addr = std::net::Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1).octets();
            sin6.sin6_scope_id = 1;
            let n = std::mem::size_of::<libc::sockaddr_in6>();
            std::ptr::copy_nonoverlapping(std::ptr::from_ref(&sin6).cast::<u8>(), sa.addr.as_mut_ptr(), n);
            sa.salen = n as u32;
            assert_eq!(host_addr_text(&sa), "fe80::1");
            let mut sa4 = ip::SockAddr::zeroed();
            let mut sin: libc::sockaddr_in = std::mem::zeroed();
            sin.sin_family = libc::AF_INET as libc::sa_family_t;
            sin.sin_addr.s_addr = u32::from(std::net::Ipv4Addr::new(127, 0, 0, 1)).to_be();
            let n = std::mem::size_of::<libc::sockaddr_in>();
            std::ptr::copy_nonoverlapping(std::ptr::from_ref(&sin).cast::<u8>(), sa4.addr.as_mut_ptr(), n);
            sa4.salen = n as u32;
            assert_eq!(host_addr_text(&sa4), "127.0.0.1");
        }
    }
}
