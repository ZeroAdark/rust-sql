// Client-side TLS: SSLRequest negotiation, the per-connection SSL context,
// the nonblocking handshake, hostname verification, the channel-binding
// certificate hash, and the read/write error surface. Consumers see the
// exact libpq error strings; the fd-based latch waits stay in lib.rs.
use std::cell::Cell;
use std::io::Write;
use std::os::fd::RawFd;

use foreign_types::{ForeignType, ForeignTypeRef};
use openssl::error::ErrorStack;
use openssl::hash::MessageDigest;
use openssl::nid::Nid;
use openssl::pkey::PKey;
use openssl::ssl::{Ssl, SslContext, SslMethod, SslMode, SslOptions, SslVerifyMode, SslVersion};
use openssl::x509::verify::X509VerifyFlags;
use openssl::x509::X509;
use types_error::PgResult;
use types_storage::waiteventset::{WL_SOCKET_READABLE, WL_SOCKET_WRITEABLE};

use crate::conninfo::opt;
use crate::{strerror, SERVER_CLOSED_MSG};

const ALPN_PROTOS: &[u8] = b"\x0apostgresql";
const PG_ALPN_PROTOCOL: &[u8] = b"postgresql";
const ROOT_CERT_FILE: &str = ".postgresql/root.crt";
const ROOT_CRL_FILE: &str = ".postgresql/root.crl";
const USER_CERT_FILE: &str = ".postgresql/postgresql.crt";
const USER_KEY_FILE: &str = ".postgresql/postgresql.key";
const MIN_OPENSSL_TLS_VERSION: &str = "TLSv1.2";
const MAX_OPENSSL_TLS_VERSION: &str = "TLSv1.3";

const ERR_LIB_SSL: libc::c_int = 20;
const SSL_AD_REASON_OFFSET: libc::c_int = 1000;
const SSL_AD_NO_APPLICATION_PROTOCOL: libc::c_int = 120;
const X509_V_ERR_UNABLE_TO_GET_ISSUER_CERT_LOCALLY: libc::c_int = 20;
const NID_MD5: libc::c_int = 4;
const NID_SHA1: libc::c_int = 64;
const PROTO_HINT_REASONS: [libc::c_int; 11] = [
    191,  // SSL_R_NO_PROTOCOLS_AVAILABLE
    258,  // SSL_R_UNSUPPORTED_PROTOCOL
    116,  // SSL_R_BAD_PROTOCOL_VERSION_NUMBER
    252,  // SSL_R_UNKNOWN_PROTOCOL
    254,  // SSL_R_UNKNOWN_SSL_VERSION
    259,  // SSL_R_UNSUPPORTED_SSL_VERSION
    266,  // SSL_R_WRONG_SSL_VERSION
    267,  // SSL_R_WRONG_VERSION_NUMBER
    1070, // SSL_R_TLSV1_ALERT_PROTOCOL_VERSION
    166,  // SSL_R_VERSION_TOO_HIGH
    396,  // SSL_R_VERSION_TOO_LOW
];

extern "C" {
    fn SSL_check_private_key(ssl: *const openssl_sys::SSL) -> libc::c_int;
    fn SSL_CTX_set_cert_cb(
        ctx: *mut openssl_sys::SSL_CTX,
        cb: Option<unsafe extern "C" fn(*mut openssl_sys::SSL, *mut libc::c_void) -> libc::c_int>,
        arg: *mut libc::c_void,
    );
    fn X509_STORE_load_locations(
        store: *mut openssl_sys::X509_STORE,
        file: *const libc::c_char,
        dir: *const libc::c_char,
    ) -> libc::c_int;
    fn X509_get_signature_info(
        x: *mut openssl_sys::X509,
        mdnid: *mut libc::c_int,
        pknid: *mut libc::c_int,
        secbits: *mut libc::c_int,
        flags: *mut u32,
    ) -> libc::c_int;
    fn inet_aton(cp: *const libc::c_char, inp: *mut libc::in_addr) -> libc::c_int;
    fn inet_pton(af: libc::c_int, src: *const libc::c_char, dst: *mut libc::c_void) -> libc::c_int;
    fn SSL_set_fd(ssl: *mut openssl_sys::SSL, fd: libc::c_int) -> libc::c_int;
    fn SSL_set_options(ssl: *mut openssl_sys::SSL, op: u64) -> u64;
    fn SSL_clear_options(ssl: *mut openssl_sys::SSL, op: u64) -> u64;
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn errno() -> i32 {
    // SAFETY: reads this thread's errno.
    unsafe { *libc::__error() }
}

#[cfg(not(any(target_os = "macos", target_os = "ios")))]
fn errno() -> i32 {
    // SAFETY: reads this thread's errno.
    unsafe { *libc::__errno_location() }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn set_errno(v: i32) {
    // SAFETY: writes this thread's errno.
    unsafe { *libc::__error() = v }
}

#[cfg(not(any(target_os = "macos", target_os = "ios")))]
fn set_errno(v: i32) {
    // SAFETY: writes this thread's errno.
    unsafe { *libc::__errno_location() = v }
}

// cert_cb's bookkeeping (fe-secure-openssl.c): whether the server asked for
// a client certificate and whether one was on hand to send.
struct CertFlags {
    requested: Cell<bool>,
    sent: Cell<bool>,
}

unsafe extern "C" fn cert_cb(ssl: *mut openssl_sys::SSL, arg: *mut libc::c_void) -> libc::c_int {
    let flags = &*(arg as *const CertFlags);
    flags.requested.set(true);
    if !openssl_sys::SSL_get_certificate(ssl).is_null() {
        flags.sent.set(true);
    }
    1
}

pub(crate) struct Pending {
    ssl: Ssl,
    flags: Box<CertFlags>,
}

pub(crate) struct ClientTls {
    pub(crate) ssl: Ssl,
    pub(crate) peer: X509,
    flags: Box<CertFlags>,
}

impl ClientTls {
    pub(crate) fn cert_requested(&self) -> bool {
        self.flags.requested.get()
    }

    pub(crate) fn cert_sent(&self) -> bool {
        self.flags.sent.get()
    }

    pub(crate) fn shutdown(&mut self) {
        // SAFETY: live SSL; a failed close_notify is ignored, as libpq does.
        unsafe { openssl_sys::SSL_shutdown(self.ssl.as_ptr()) };
    }
}

fn clear_err_queue() {
    // SAFETY: clears this thread's OpenSSL error queue; no preconditions.
    unsafe { openssl_sys::ERR_clear_error() }
}

// SSLerrmessage(ERR_get_error()).
fn errmessage(stack: &ErrorStack) -> String {
    let Some(err) = stack.errors().first() else {
        return "no SSL error reported".to_string();
    };
    if let Some(reason) = err.reason() {
        return reason.to_string();
    }
    if err.library_code() == ERR_LIB_SSL
        && err.reason_code() == SSL_AD_REASON_OFFSET + SSL_AD_NO_APPLICATION_PROTOCOL
    {
        return "no application protocol".to_string();
    }
    let code = err.code();
    // OpenSSL 3 stops mapping system errnos; ERR_SYSTEM_ERROR is the high
    // bit and ERR_GET_REASON is then code & ERR_SYSTEM_MASK.
    if code & 0x8000_0000 != 0 {
        return strerror((code & 0x7fff_ffff) as i32);
    }
    format!("SSL error code {code}")
}

fn err_now() -> String {
    errmessage(&ErrorStack::get())
}

fn protocol_version_to_openssl(v: &str) -> Option<SslVersion> {
    if v.eq_ignore_ascii_case("TLSv1") {
        Some(SslVersion::TLS1)
    } else if v.eq_ignore_ascii_case("TLSv1.1") {
        Some(SslVersion::TLS1_1)
    } else if v.eq_ignore_ascii_case("TLSv1.2") {
        Some(SslVersion::TLS1_2)
    } else if v.eq_ignore_ascii_case("TLSv1.3") {
        Some(SslVersion::TLS1_3)
    } else {
        None
    }
}

fn nonempty<'a>(opts: &'a [(String, String)], key: &str) -> Option<&'a str> {
    opt(opts, key).filter(|v| !v.is_empty())
}

fn stat(path: &str) -> Result<libc::stat, i32> {
    let c = std::ffi::CString::new(path).map_err(|_| libc::ENOENT)?;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: c is NUL-terminated and st is a valid out-buffer.
    if unsafe { libc::stat(c.as_ptr(), &mut st) } == 0 {
        Ok(st)
    } else {
        Err(errno())
    }
}

// initialize_SSL (fe-secure-openssl.c): the per-connection SSL_CTX and SSL
// object, loaded with the root store, client certificate and key.
pub(crate) fn initialize(opts: &[(String, String)], host: &str) -> Result<Pending, String> {
    let sslmode = opt(opts, "sslmode").unwrap_or("prefer");
    let sslcert = nonempty(opts, "sslcert");
    let sslkey = nonempty(opts, "sslkey");
    let sslrootcert = nonempty(opts, "sslrootcert");
    let sslcrl = nonempty(opts, "sslcrl");
    let sslcrldir = nonempty(opts, "sslcrldir");
    let homedir = if sslcert.is_none()
        || sslkey.is_none()
        || sslrootcert.is_none()
        || (sslcrl.is_none() && sslcrldir.is_none())
    {
        crate::conninfo::home_directory()
    } else {
        None
    };

    clear_err_queue();
    let mut ctx = SslContext::builder(SslMethod::tls())
        .map_err(|e| format!("could not create SSL context: {}", errmessage(&e)))?;
    let flags = Box::new(CertFlags { requested: Cell::new(false), sent: Cell::new(false) });
    // SAFETY: the callback only dereferences `flags`, which ClientTls keeps
    // alive for as long as the SSL object that can call it.
    unsafe {
        SSL_CTX_set_cert_cb(ctx.as_ptr(), Some(cert_cb), &*flags as *const CertFlags as *mut _);
    }
    ctx.set_options(SslOptions::NO_SSLV2 | SslOptions::NO_SSLV3);
    if let Some(v) = nonempty(opts, "ssl_min_protocol_version") {
        let ver = protocol_version_to_openssl(v)
            .ok_or_else(|| format!("invalid value \"{v}\" for minimum SSL protocol version"))?;
        ctx.set_min_proto_version(Some(ver))
            .map_err(|e| format!("could not set minimum SSL protocol version: {}", errmessage(&e)))?;
    }
    if let Some(v) = nonempty(opts, "ssl_max_protocol_version") {
        let ver = protocol_version_to_openssl(v)
            .ok_or_else(|| format!("invalid value \"{v}\" for maximum SSL protocol version"))?;
        ctx.set_max_proto_version(Some(ver))
            .map_err(|e| format!("could not set maximum SSL protocol version: {}", errmessage(&e)))?;
    }
    ctx.set_mode(SslMode::ACCEPT_MOVING_WRITE_BUFFER);

    let rootfile = match (sslrootcert, &homedir) {
        (Some(f), _) => f.to_string(),
        (None, Some(h)) => format!("{h}/{ROOT_CERT_FILE}"),
        (None, None) => String::new(),
    };
    let have_rootcert = if rootfile == "system" {
        ctx.set_default_verify_paths()
            .map_err(|e| format!("could not load system root certificate paths: {}", errmessage(&e)))?;
        true
    } else if !rootfile.is_empty() && stat(&rootfile).is_ok() {
        ctx.set_ca_file(&rootfile)
            .map_err(|e| format!("could not read root certificate file \"{rootfile}\": {}", errmessage(&e)))?;
        let crl_default;
        let (fname, dname) = match (sslcrl, sslcrldir, &homedir) {
            (None, None, Some(h)) => {
                crl_default = format!("{h}/{ROOT_CRL_FILE}");
                (Some(crl_default.as_str()), None)
            }
            (f, d, _) => (f, d),
        };
        if fname.is_some() || dname.is_some() {
            let cf = fname.and_then(|f| std::ffi::CString::new(f).ok());
            let cd = dname.and_then(|d| std::ffi::CString::new(d).ok());
            let store = ctx.cert_store_mut();
            // SAFETY: store is the live context's X509_STORE; the paths are
            // NUL-terminated for the duration of the call.
            let loaded = unsafe {
                X509_STORE_load_locations(
                    store.as_ptr(),
                    cf.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
                    cd.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
                )
            };
            if loaded == 1 {
                let _ = store.set_flags(X509VerifyFlags::CRL_CHECK | X509VerifyFlags::CRL_CHECK_ALL);
            }
            clear_err_queue();
        }
        true
    } else {
        if sslmode.starts_with('v') {
            return Err(if rootfile.is_empty() {
                "could not get home directory to locate root certificate file\n\
                 Either provide the file, use the system's trusted roots with sslrootcert=system, or change sslmode to disable server certificate verification."
                    .to_string()
            } else {
                format!(
                    "root certificate file \"{rootfile}\" does not exist\n\
                     Either provide the file, use the system's trusted roots with sslrootcert=system, or change sslmode to disable server certificate verification."
                )
            });
        }
        false
    };

    let certfile = match (sslcert, &homedir) {
        (Some(f), _) => f.to_string(),
        (None, Some(h)) => format!("{h}/{USER_CERT_FILE}"),
        (None, None) => String::new(),
    };
    let sslcertmode = opt(opts, "sslcertmode").unwrap_or("allow");
    let have_cert = if sslcertmode.starts_with('d') || certfile.is_empty() {
        false
    } else {
        match stat(&certfile) {
            Err(e) => {
                if e != libc::ENOENT && e != libc::ENOTDIR {
                    return Err(format!(
                        "could not open certificate file \"{certfile}\": {}",
                        strerror(e)
                    ));
                }
                false
            }
            Ok(_) => {
                ctx.set_certificate_chain_file(&certfile).map_err(|e| {
                    format!("could not read certificate file \"{certfile}\": {}", errmessage(&e))
                })?;
                true
            }
        }
    };

    if let Some(keylog) = nonempty(opts, "sslkeylogfile") {
        let path = keylog.to_string();
        ctx.set_keylog_callback(move |_, line| {
            if let Ok(mut f) =
                std::fs::OpenOptions::new().create(true).append(true).open(&path)
            {
                let _ = writeln!(f, "{line}");
            }
        });
    }

    let ctx = ctx.build();
    let mut ssl = Ssl::new(&ctx)
        .map_err(|e| format!("could not establish SSL connection: {}", errmessage(&e)))?;
    drop(ctx);

    // RFC 6066: no SNI for a literal IP address.
    if opt(opts, "sslsni").unwrap_or("1").starts_with('1')
        && !host.is_empty()
        && !(host.bytes().all(|b| b.is_ascii_digit() || b == b'.') || host.contains(':'))
    {
        ssl.set_hostname(host).map_err(|e| {
            format!("could not set SSL Server Name Indication (SNI): {}", errmessage(&e))
        })?;
    }
    ssl.set_alpn_protos(ALPN_PROTOS)
        .map_err(|e| format!("could not set SSL ALPN extension: {}", errmessage(&e)))?;

    let keyfile = match (have_cert, sslkey, &homedir) {
        (true, Some(k), _) => k.to_string(),
        (_, _, Some(h)) => format!("{h}/{USER_KEY_FILE}"),
        _ => String::new(),
    };
    if have_cert && !keyfile.is_empty() {
        let st = match stat(&keyfile) {
            Ok(st) => st,
            Err(libc::ENOENT) => {
                return Err(format!("certificate present, but not private key file \"{keyfile}\""))
            }
            Err(e) => {
                return Err(format!(
                    "could not stat private key file \"{keyfile}\": {}",
                    strerror(e)
                ))
            }
        };
        if st.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err(format!("private key file \"{keyfile}\" is not a regular file"));
        }
        let bad = if st.st_uid == 0 {
            st.st_mode & (libc::S_IWGRP | libc::S_IXGRP | libc::S_IRWXO)
        } else {
            st.st_mode & (libc::S_IRWXG | libc::S_IRWXO)
        };
        if bad != 0 {
            return Err(format!(
                "private key file \"{keyfile}\" has group or world access; file must have permissions u=rw (0600) or less if owned by the current user, or permissions u=rw,g=r (0640) or less if owned by root"
            ));
        }
        let bytes = std::fs::read(&keyfile)
            .map_err(|e| format!("could not load private key file \"{keyfile}\": {e}"))?;
        clear_err_queue();
        let sslpassword = nonempty(opts, "sslpassword");
        let pem = match sslpassword {
            Some(pw) => PKey::private_key_from_pem_passphrase(&bytes, pw.as_bytes()),
            None => PKey::private_key_from_pem_callback(&bytes, |_| Ok(0)),
        };
        let pkey = match pem {
            Ok(k) => k,
            Err(pem_err) => match PKey::private_key_from_der(&bytes) {
                Ok(k) => k,
                Err(_) => {
                    return Err(format!(
                        "could not load private key file \"{keyfile}\": {}",
                        errmessage(&pem_err)
                    ))
                }
            },
        };
        clear_err_queue();
        ssl.set_private_key(&pkey)
            .map_err(|e| format!("could not load private key file \"{keyfile}\": {}", errmessage(&e)))?;
    }
    // SAFETY: ssl is a live SSL object.
    if have_cert && unsafe { SSL_check_private_key(ssl.as_ptr()) } != 1 {
        return Err(format!(
            "certificate does not match private key file \"{keyfile}\": {}",
            err_now()
        ));
    }
    if have_rootcert {
        ssl.set_verify(SslVerifyMode::PEER);
    }
    // SAFETY: ssl is a live SSL object.
    unsafe {
        if opt(opts, "sslcompression").unwrap_or("0").starts_with('0') {
            SSL_set_options(ssl.as_ptr(), openssl_sys::SSL_OP_NO_COMPRESSION as u64);
        } else {
            SSL_clear_options(ssl.as_ptr(), openssl_sys::SSL_OP_NO_COMPRESSION as u64);
        }
    }
    Ok(Pending { ssl, flags })
}

// open_client_SSL: the nonblocking SSL_connect loop, the direct-SSL ALPN
// check, the peer certificate, and verify-full's name check.
pub(crate) fn open(
    pending: Pending,
    fd: RawFd,
    opts: &[(String, String)],
    host: &str,
    direct: bool,
    wait: &mut dyn FnMut(u32) -> PgResult<()>,
) -> PgResult<Result<ClientTls, String>> {
    let Pending { ssl, flags } = pending;
    // The socket BIO (BIO_NOCLOSE) rather than a Rust stream adapter: only
    // it answers BIO_CTRL_EOF, which is what turns a peer hangup into
    // OpenSSL 3's "unexpected eof while reading" instead of a bare SYSCALL.
    // SAFETY: live SSL; the fd outlives it and stays owned by the caller.
    if unsafe { SSL_set_fd(ssl.as_ptr(), fd) } != 1 {
        return Ok(Err(format!("SSL error: {}", err_now())));
    }
    loop {
        clear_err_queue();
        set_errno(0);
        // SAFETY: live SSL with its BIO attached.
        let r = unsafe { openssl_sys::SSL_connect(ssl.as_ptr()) };
        if r == 1 {
            break;
        }
        let save_errno = errno();
        // SAFETY: r is the return value of the SSL_connect just made on ssl.
        let err = unsafe { openssl_sys::SSL_get_error(ssl.as_ptr(), r) };
        let stack = ErrorStack::get();
        let msg = match err {
            openssl_sys::SSL_ERROR_WANT_READ => {
                wait(WL_SOCKET_READABLE)?;
                continue;
            }
            openssl_sys::SSL_ERROR_WANT_WRITE => {
                wait(WL_SOCKET_WRITEABLE)?;
                continue;
            }
            openssl_sys::SSL_ERROR_SYSCALL => {
                let vcode = ssl.verify_result();
                if save_errno == 0
                    && vcode.as_raw() == X509_V_ERR_UNABLE_TO_GET_ISSUER_CERT_LOCALLY
                    && opt(opts, "sslrootcert") == Some("system")
                {
                    format!("SSL error: certificate verify failed: {}", vcode.error_string())
                } else if r == -1 && save_errno != 0 {
                    format!("SSL SYSCALL error: {}", strerror(save_errno))
                } else {
                    "SSL SYSCALL error: EOF detected".to_string()
                }
            }
            openssl_sys::SSL_ERROR_SSL => {
                let mut msg = format!("SSL error: {}", errmessage(&stack));
                let reason = stack.errors().first().map(|e| e.reason_code());
                if reason.is_some_and(|r| PROTO_HINT_REASONS.contains(&r)) {
                    msg.push('\n');
                    msg.push_str(&format!(
                        "This may indicate that the server does not support any SSL protocol version between {} and {}.",
                        opt(opts, "ssl_min_protocol_version").unwrap_or(MIN_OPENSSL_TLS_VERSION),
                        opt(opts, "ssl_max_protocol_version").unwrap_or(MAX_OPENSSL_TLS_VERSION)
                    ));
                }
                msg
            }
            code => format!("unrecognized SSL error code: {code}"),
        };
        return Ok(Err(msg));
    }

    if direct {
        match ssl.selected_alpn_protocol() {
            None => {
                return Ok(Err(
                    "direct SSL connection was established without ALPN protocol negotiation extension".into(),
                ))
            }
            Some(p) if p != PG_ALPN_PROTOCOL => {
                return Ok(Err(
                    "SSL connection was established with unexpected ALPN protocol".into(),
                ))
            }
            Some(_) => {}
        }
    }

    let Some(peer) = ssl.peer_certificate() else {
        return Ok(Err(format!("certificate could not be obtained: {}", err_now())));
    };
    if let Err(e) = verify_peer_name_matches_certificate(opts, host, &peer) {
        return Ok(Err(e));
    }
    Ok(Ok(ClientTls { ssl, peer, flags }))
}

pub(crate) enum Io {
    Done(usize),
    WantRead,
    WantWrite,
    Failed(String),
}

// pgtls_read / pgtls_write's error classification.
fn classify(ssl: &Ssl, n: libc::c_int, save_errno: i32, op: &str) -> Io {
    // SAFETY: n is the return value of the SSL_read/SSL_write just made on ssl.
    let err = unsafe { openssl_sys::SSL_get_error(ssl.as_ptr(), n) };
    match err {
        openssl_sys::SSL_ERROR_NONE if n >= 0 => Io::Done(n as usize),
        openssl_sys::SSL_ERROR_NONE => {
            Io::Failed(format!("{op} failed but did not provide error information"))
        }
        openssl_sys::SSL_ERROR_WANT_READ => Io::WantRead,
        openssl_sys::SSL_ERROR_WANT_WRITE => Io::WantWrite,
        openssl_sys::SSL_ERROR_SYSCALL if n < 0 && save_errno != 0 => {
            Io::Failed(if save_errno == libc::EPIPE || save_errno == libc::ECONNRESET {
                SERVER_CLOSED_MSG.to_string()
            } else {
                format!("SSL SYSCALL error: {}", strerror(save_errno))
            })
        }
        openssl_sys::SSL_ERROR_SYSCALL => Io::Failed("SSL SYSCALL error: EOF detected".into()),
        openssl_sys::SSL_ERROR_SSL => {
            Io::Failed(format!("SSL error: {}", errmessage(&ErrorStack::get())))
        }
        openssl_sys::SSL_ERROR_ZERO_RETURN => {
            Io::Failed("SSL connection has been closed unexpectedly".into())
        }
        code => Io::Failed(format!("unrecognized SSL error code: {code}")),
    }
}

pub(crate) fn read(ssl: &Ssl, buf: &mut [u8]) -> Io {
    clear_err_queue();
    set_errno(0);
    // SAFETY: live SSL; buf is valid writable memory of buf.len() bytes.
    let n = unsafe { openssl_sys::SSL_read(ssl.as_ptr(), buf.as_mut_ptr().cast(), buf.len() as libc::c_int) };
    classify(ssl, n, errno(), "SSL_read")
}

pub(crate) fn write(ssl: &Ssl, buf: &[u8]) -> Io {
    clear_err_queue();
    set_errno(0);
    // SAFETY: live SSL; buf is valid readable memory of buf.len() bytes.
    let n = unsafe { openssl_sys::SSL_write(ssl.as_ptr(), buf.as_ptr().cast(), buf.len() as libc::c_int) };
    classify(ssl, n, errno(), "SSL_write")
}

// pgtls_get_peer_certificate_hash: the tls-server-end-point channel-binding
// data (RFC 5929: MD5/SHA-1 signatures hash with SHA-256).
pub(crate) fn peer_certificate_hash(peer: &X509) -> Result<Vec<u8>, String> {
    let mut algo_nid: libc::c_int = 0;
    // SAFETY: live X509; only mdnid is requested.
    let ok = unsafe {
        X509_get_signature_info(
            peer.as_ptr(),
            &mut algo_nid,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err("could not determine server certificate signature algorithm".into());
    }
    let algo = match algo_nid {
        NID_MD5 | NID_SHA1 => MessageDigest::sha256(),
        nid => match MessageDigest::from_nid(Nid::from_raw(nid)) {
            Some(md) => md,
            None => {
                return Err(format!(
                    "could not find digest for NID {}",
                    Nid::from_raw(nid).short_name().unwrap_or("undef")
                ))
            }
        },
    };
    match peer.digest(algo) {
        Ok(h) => Ok(h.to_vec()),
        Err(_) => Err("could not generate peer certificate hash".into()),
    }
}

fn is_ip_address(host: &str) -> bool {
    let Ok(c) = std::ffi::CString::new(host) else { return false };
    let mut a4: libc::in_addr = unsafe { std::mem::zeroed() };
    let mut a6: libc::in6_addr = unsafe { std::mem::zeroed() };
    // SAFETY: c is NUL-terminated; the out-buffers are valid.
    unsafe {
        inet_aton(c.as_ptr(), &mut a4) != 0
            || inet_pton(libc::AF_INET6, c.as_ptr(), (&mut a6 as *mut libc::in6_addr).cast()) == 1
    }
}

// wildcard_certificate_match (fe-secure-common.c).
fn wildcard_certificate_match(pattern: &str, string: &str) -> bool {
    let (lenpat, lenstr) = (pattern.len(), string.len());
    if lenpat < 3 || !pattern.starts_with("*.") || lenpat > lenstr {
        return false;
    }
    if !pattern[1..].eq_ignore_ascii_case(&string[lenstr - lenpat + 1..]) {
        return false;
    }
    match string.find('.') {
        Some(dot) => dot >= lenstr - lenpat,
        None => true,
    }
}

// pq_verify_peer_name_matches_certificate_name: Ok(Some(match)) with the
// name to report; Err is the libpq error text.
fn name_matches(host: &str, namedata: &[u8]) -> Result<(bool, String), String> {
    if host.is_empty() {
        return Err("host name must be specified".into());
    }
    if namedata.contains(&0) {
        return Err("SSL certificate's name contains embedded null".into());
    }
    let name = String::from_utf8_lossy(namedata).into_owned();
    let matched = name.eq_ignore_ascii_case(host) || wildcard_certificate_match(&name, host);
    Ok((matched, name))
}

// pq_verify_peer_name_matches_certificate_ip.
fn ip_matches(host: &str, ipdata: &[u8]) -> Result<(bool, String), String> {
    if host.is_empty() {
        return Err("host name must be specified".into());
    }
    let c = std::ffi::CString::new(host).map_err(|_| "host name must be specified".to_string())?;
    match ipdata.len() {
        4 => {
            let mut addr: libc::in_addr = unsafe { std::mem::zeroed() };
            // SAFETY: c is NUL-terminated; addr is a valid out-buffer.
            let matched = unsafe { inet_aton(c.as_ptr(), &mut addr) } != 0
                && addr.s_addr.to_ne_bytes() == ipdata[..4];
            let a: [u8; 4] = ipdata.try_into().unwrap();
            Ok((matched, std::net::Ipv4Addr::from(a).to_string()))
        }
        16 => {
            let mut addr: libc::in6_addr = unsafe { std::mem::zeroed() };
            // SAFETY: as above.
            let matched = unsafe {
                inet_pton(libc::AF_INET6, c.as_ptr(), (&mut addr as *mut libc::in6_addr).cast())
            } == 1
                && addr.s6_addr[..] == ipdata[..];
            let a: [u8; 16] = ipdata.try_into().unwrap();
            Ok((matched, crate::pg_inet6_ntop(&a)))
        }
        n => Err(format!("certificate contains IP address with invalid length {n}")),
    }
}

// pgtls_verify_peer_name_matches_certificate_guts: SANs of the host's kind
// (DNS or IP) shadow the CN; otherwise the CN is consulted too.
fn verify_peer_name_guts(host: &str, peer: &X509) -> Result<(bool, usize, Option<String>), String> {
    let host_is_ip = is_ip_address(host);
    let mut names_examined = 0usize;
    let mut first_name: Option<String> = None;
    let mut check_cn = true;
    if let Some(sans) = peer.subject_alt_names() {
        for name in sans.iter() {
            let (is_dns, is_ip) = (name.dnsname().is_some(), name.ipaddress().is_some());
            if (host_is_ip && is_ip) || (!host_is_ip && is_dns) {
                check_cn = false;
            }
            let r = if let Some(dns) = name.dnsname() {
                names_examined += 1;
                name_matches(host, dns.as_bytes())?
            } else if let Some(ip) = name.ipaddress() {
                names_examined += 1;
                ip_matches(host, ip)?
            } else {
                continue;
            };
            first_name.get_or_insert(r.1);
            if r.0 {
                return Ok((true, names_examined, first_name));
            }
        }
    }
    if check_cn {
        if let Some(cn) = peer.subject_name().entries_by_nid(Nid::COMMONNAME).next() {
            names_examined += 1;
            let r = name_matches(host, cn.data().as_slice())?;
            first_name.get_or_insert(r.1);
            if r.0 {
                return Ok((true, names_examined, first_name));
            }
        }
    }
    Ok((false, names_examined, first_name))
}

// pq_verify_peer_name_matches_certificate.
fn verify_peer_name_matches_certificate(
    opts: &[(String, String)],
    host: &str,
    peer: &X509,
) -> Result<(), String> {
    if opt(opts, "sslmode") != Some("verify-full") {
        return Ok(());
    }
    if host.is_empty() {
        return Err("host name must be specified for a verified SSL connection".into());
    }
    let (matched, names_examined, first_name) = verify_peer_name_guts(host, peer)?;
    if matched {
        return Ok(());
    }
    let first_name = first_name.unwrap_or_default();
    Err(match names_examined {
        0 => "could not get server's host name from server certificate".to_string(),
        1 => format!("server certificate for \"{first_name}\" does not match host name \"{host}\""),
        n => format!(
            "server certificate for \"{first_name}\" (and {} other name{}) does not match host name \"{host}\"",
            n - 1,
            if n - 1 == 1 { "" } else { "s" }
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_match_follows_libpq() {
        assert!(wildcard_certificate_match("*.example.com", "db.example.com"));
        assert!(wildcard_certificate_match("*.EXAMPLE.com", "db.example.COM"));
        assert!(!wildcard_certificate_match("*.example.com", "a.db.example.com"));
        assert!(!wildcard_certificate_match("*.example.com", "example.com"));
        assert!(!wildcard_certificate_match("*example.com", "db.example.com"));
        assert!(!wildcard_certificate_match("*.", "x."));
        assert!(!wildcard_certificate_match("db.example.com", "db.example.com"));
    }

    #[test]
    fn ip_address_detection_accepts_inet_aton_forms() {
        assert!(is_ip_address("127.0.0.1"));
        assert!(is_ip_address("127.1"));
        assert!(is_ip_address("::1"));
        assert!(!is_ip_address("localhost"));
        assert!(!is_ip_address(""));
    }

    #[test]
    fn name_checks_reject_embedded_nul_and_report_the_name() {
        assert_eq!(name_matches("localhost", b"LOCALHOST").unwrap(), (true, "LOCALHOST".into()));
        assert_eq!(name_matches("localhost", b"other").unwrap(), (false, "other".into()));
        assert_eq!(
            name_matches("localhost", b"local\0host").unwrap_err(),
            "SSL certificate's name contains embedded null"
        );
        assert_eq!(name_matches("", b"x").unwrap_err(), "host name must be specified");
        assert_eq!(ip_matches("127.0.0.1", &[127, 0, 0, 1]).unwrap(), (true, "127.0.0.1".into()));
        assert_eq!(ip_matches("127.0.0.2", &[127, 0, 0, 1]).unwrap(), (false, "127.0.0.1".into()));
        assert_eq!(ip_matches("::1", &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]).unwrap(), (true, "::1".into()));
        assert_eq!(
            ip_matches("::1", &[1, 2, 3]).unwrap_err(),
            "certificate contains IP address with invalid length 3"
        );
    }

    #[test]
    fn proto_hint_reason_codes_match_libssl() {
        openssl::init();
        fn reason_text(code: libc::c_int) -> String {
            // SAFETY: ERR_reason_error_string returns a static string or NULL.
            unsafe {
                let p = openssl_sys::ERR_reason_error_string(openssl_sys::ERR_PACK(ERR_LIB_SSL, 0, code));
                if p.is_null() {
                    "<unknown reason>".to_string()
                } else {
                    std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
                }
            }
        }
        let texts = [
            "no protocols available",
            "unsupported protocol",
            "bad protocol version number",
            "unknown protocol",
            "unknown ssl version",
            "unsupported ssl version",
            "wrong ssl version",
            "wrong version number",
            "tlsv1 alert protocol version",
            "version too high",
            "version too low",
        ];
        for (code, text) in PROTO_HINT_REASONS.iter().zip(texts) {
            assert_eq!(reason_text(*code), text, "SSL_R_ reason code {code}");
        }
    }
}
