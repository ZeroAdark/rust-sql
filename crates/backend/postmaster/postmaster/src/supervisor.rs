// Crash supervisor: the process rendering of C's HandleChildCrash cycle.
//
// C's postmaster survives a backend killed by a signal: it logs "was
// terminated by signal N", quits the other children, reinitializes shared
// memory, reruns crash recovery and accepts connections again, all on the
// listen sockets it opened once. Under the thread model a hard signal kills
// the whole server process, so before `PostmasterMain` the process forks:
// the parent stays a single-purpose supervisor (no shared memory, no SQL
// state) that waits on the server child and launches the next generation
// when the child dies by a signal or asks for a crash restart.
//
// Child -> supervisor status pipe: b'R' once the postmaster accepts
// connections, read-write or hot standby (a signal death before that, or of
// the startup process itself, is a startup-process crash: C exits 1 instead
// of looping), b'y'/b'n' with the current restart_after_crash, b'C' when the
// child ran C's crash choreography itself (a PANIC or panic escaping a
// thread: peers already got 57P02) and exits for a fresh generation, and
// b'K' + "<backend type>\0<pid>\0<signal>\0<activity>\0" from the crash-signal handler
// (the process is doomed; the supervisor SIGKILLs it if it has not died
// within DOOMED_GRACE_MS). Supervisor -> child life pipe: EOF kills the
// child.
// Config pipe: after startup and after every reload the child sends a
// length-prefixed image of PgStartTime plus its settings
// (guc::store::write_postmaster_gucs); the next generation restores the last
// complete one instead of rereading the configuration files, as C's
// postmaster keeps its settings across a crash restart.
// Handoff socket: the first generation passes its listen sockets
// (SCM_RIGHTS), DataDir and postmaster.pid socket lines to the supervisor;
// every later generation inherits them instead of binding, so the kernel
// queues connections across the restart and the new postmaster answers them
// the way C's does mid-recovery.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};

#[cfg(not(any(target_family = "wasm", pgrust_sim)))]
use init_small::globals::{process_id, SetMyProcPid};
use types_error::PgResult;

use crate::MAXLISTEN;

/// Write end of the status pipe in the supervised child; -1 unsupervised.
static STATUS_FD: AtomicI32 = AtomicI32::new(-1);
/// Child end of the handoff socket until the listen sockets are sent.
static HANDOFF_FD: AtomicI32 = AtomicI32::new(-1);
/// The live child's pid in the supervisor (0 between launches).
#[cfg(not(any(target_family = "wasm", pgrust_sim)))]
static CHILD_PID: AtomicI32 = AtomicI32::new(0);
/// Set in a generation launched after a crash of the previous one.
static CRASH_RESTART: AtomicBool = AtomicBool::new(false);
/// Write end of the config pipe in the supervised child; -1 unsupervised.
static CONFIG_FD: AtomicI32 = AtomicI32::new(-1);
/// C's crash report lines (LogChildExit, HandleChildCrash,
/// PostmasterStateMachine) for the next generation to log through its own
/// log destination, so they land in the logging collector's file.
static RESTART_REPORT: pgsync::Mutex<Vec<CrashLine>> = pgsync::Mutex::new(Vec::new());
/// The previous generation's last config image, for a crash-restarted one.
static RESTART_CONFIG: pgsync::Mutex<Vec<u8>> = pgsync::Mutex::new(Vec::new());
/// Listen sockets the supervisor holds (inherited by the next generation).
static HELD: [AtomicI32; MAXLISTEN] = [const { AtomicI32::new(-1) }; MAXLISTEN];
static HELD_N: AtomicUsize = AtomicUsize::new(0);

#[cfg(not(any(target_family = "wasm", pgrust_sim)))]
const FORWARDED: [i32; 5] = [libc::SIGHUP, libc::SIGINT, libc::SIGTERM, libc::SIGQUIT, libc::SIGUSR1];

const NO_SUPERVISOR: &str = "--no-supervisor";

/// C's SIGKILL_CHILDREN_AFTER_SECS for a child whose crash handler ran but
/// which never died.
#[cfg(not(any(target_family = "wasm", pgrust_sim)))]
const DOOMED_GRACE_MS: i32 = 5000;

/// `postgres --no-supervisor ...` (or the sim/wasm builds) runs the server
/// in this process, the pre-supervisor shape debuggers expect.
pub fn wants_supervisor(argv: &[String]) -> bool {
    !argv.iter().any(|a| a == NO_SUPERVISOR)
}

/// The argv PostmasterMain sees: the switch is ours, not a GUC.
fn strip_switch(argv: &[String]) -> PgResult<Vec<String>> {
    let mut v = Vec::new();
    v.try_reserve(argv.len()).map_err(|_| mcx::oom_named("Postmaster", argv.len()))?;
    v.extend(argv.iter().filter(|a| *a != NO_SUPERVISOR).cloned());
    Ok(v)
}

/// What the status pipe said about one generation.
#[derive(Debug, Default, PartialEq)]
struct ChildReport {
    ready: bool,
    restart_after_crash: bool,
    restart_requested: bool,
    doomed: bool,
    startup_crashed: bool,
    /// "<backend type> (PID <pid>)" and signal of the thread whose crash
    /// handler ran.
    crashed: Option<(String, i32)>,
    activity: Option<String>,
}

pub struct CrashLine {
    pub message: String,
    pub detail: Option<String>,
    pub line: i32,
    pub func: &'static str,
}

impl CrashLine {
    #[cfg(not(any(target_family = "wasm", pgrust_sim)))]
    fn new(message: String, line: i32, func: &'static str) -> Self {
        Self { message, detail: None, line, func }
    }
}

impl ChildReport {
    fn new() -> Self {
        Self { restart_after_crash: true, ..Self::default() }
    }

    fn feed(&mut self, bytes: &[u8], pending: &mut Vec<u8>) {
        for &b in bytes {
            if !pending.is_empty() {
                if pending.len() < CRASH_RECORD_MAX {
                    pending.push(b);
                }
                if b == 0 && pending.iter().filter(|&&c| c == 0).count() == 4 {
                    self.startup_crashed = pending[1..].starts_with(b"startup\0");
                    if let Some((who, sig, activity)) = parse_crash_record(&pending[1..]) {
                        self.crashed = Some((who, sig));
                        self.activity = activity;
                    }
                    pending.clear();
                }
                continue;
            }
            match b {
                b'R' => self.ready = true,
                b'y' => self.restart_after_crash = true,
                b'n' => self.restart_after_crash = false,
                b'C' => self.restart_requested = true,
                b'K' => {
                    self.doomed = true;
                    pending.push(b);
                }
                _ => {}
            }
        }
    }
}

const CRASH_RECORD_MAX: usize = 1200;

fn parse_crash_record(rec: &[u8]) -> Option<(String, i32, Option<String>)> {
    let mut parts = rec.split(|&b| b == 0);
    let desc = std::str::from_utf8(parts.next()?).ok()?;
    let pid = std::str::from_utf8(parts.next()?).ok()?;
    let sig: i32 = std::str::from_utf8(parts.next()?).ok()?.parse().ok()?;
    if desc.is_empty() || pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let activity = parts.next().filter(|a| !a.is_empty()).map(|a| String::from_utf8_lossy(a).into_owned());
    Some((format!("{desc} (PID {pid})"), sig, activity))
}

/// Fork the server child; return only in the child (or when supervision is
/// off). The parent supervises until the child ends for good and exits with
/// its status.
#[cfg(not(any(target_family = "wasm", pgrust_sim)))]
pub fn supervise(argv: &[String]) -> PgResult<Vec<String>> {
    if !wants_supervisor(argv) {
        return strip_switch(argv);
    }
    install_forwarders();
    let mut handoff: Vec<u8> = Vec::new();
    let mut config: Vec<u8> = Vec::new();
    let mut restart = false;
    loop {
        let (status_rd, status_wr) = pipe();
        let (config_rd, config_wr) = pipe();
        let (life_rd, life_wr) = pipe();
        let (hand_rd, hand_wr) = socket_pair();
        // SAFETY: the process is single-threaded here (before any backend or
        // pool thread), the one state in which fork() is sound.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            let err = std::io::Error::last_os_error();
            elog::write_stderr(&format!("postgres: could not fork server process: {err}\n"));
            std::process::exit(1);
        }
        if pid == 0 {
            // SAFETY: closing this side's unused ends.
            unsafe {
                libc::close(status_rd);
                libc::close(config_rd);
                libc::close(life_wr);
                libc::close(hand_rd);
            }
            STATUS_FD.store(status_wr, Ordering::Release);
            CONFIG_FD.store(config_wr, Ordering::Release);
            if restart {
                *pgsync::lock(&RESTART_CONFIG) = std::mem::take(&mut config);
            }
            if restart {
                // SAFETY: the supervisor already holds this generation's
                // sockets; nothing to hand over.
                unsafe {
                    libc::close(hand_wr);
                }
            } else {
                HANDOFF_FD.store(hand_wr, Ordering::Release);
            }
            CRASH_RESTART.store(restart, Ordering::Release);
            if restart {
                let n = HELD_N.load(Ordering::Acquire);
                let mut fds = [-1i32; MAXLISTEN];
                for (i, slot) in fds.iter_mut().enumerate().take(n) {
                    *slot = HELD[i].load(Ordering::Acquire);
                }
                if pqcomm_seams::inherit_listen_sockets::is_installed() {
                    let text = |i| String::from_utf8_lossy(handoff_field(&handoff, i)).into_owned();
                    pqcomm_seams::inherit_listen_sockets::call(&fds[..n], &text(1), &text(2));
                }
            }
            // SAFETY: getppid(2) cannot fail; the parent is the supervisor.
            init_small::globals::set_launched_pid(unsafe { libc::getppid() });
            SetMyProcPid(process_id() as i32);
            restore_default_dispositions();
            spawn_life_watcher(life_rd);
            return strip_switch(argv);
        }
        // SAFETY: parent side; the child owns the other ends.
        unsafe {
            libc::close(status_wr);
            libc::close(config_wr);
            libc::close(life_rd);
            libc::close(hand_wr);
        }
        CHILD_PID.store(pid, Ordering::Release);
        let mut images = ConfigImages { fd: config_rd, buf: Vec::new(), latest: std::mem::take(&mut config) };
        let (status, report) = watch_child(pid, status_rd, hand_rd, &mut handoff, &mut images);
        config = images.latest;
        CHILD_PID.store(0, Ordering::Release);
        // SAFETY: fds owned by the parent, closed once.
        unsafe {
            libc::close(config_rd);
            libc::close(status_rd);
            libc::close(life_wr);
            libc::close(hand_rd);
        }

        let mut lines = Vec::new();
        let (who, handler_sig) = report
            .crashed
            .clone()
            .map_or_else(|| (format!("server process (PID {pid})"), None), |(w, s)| (w, Some(s)));
        if libc::WIFEXITED(status) {
            let code = libc::WEXITSTATUS(status);
            if report.restart_requested {
                // The child logged C's whole cycle and its peers got 57P02.
                pgsync::lock(&RESTART_REPORT).clear();
                restart = true;
                continue;
            }
            if code <= 1 || !report.ready {
                std::process::exit(code);
            }
            // postmaster.c LogChildExit: a non-0/1 exit is a crash too.
            lines.push(CrashLine::new(format!("{who} exited with exit code {code}"), 3830, "LogChildExit"));
        } else {
            // A doomed child the supervisor had to SIGKILL died of the
            // signal its crash handler reported.
            let signo = handler_sig.unwrap_or(if libc::WIFSIGNALED(status) {
                libc::WTERMSIG(status)
            } else {
                libc::SIGKILL
            });
            lines.push(CrashLine::new(
                format!("{who} was terminated by signal {signo}: {}", wait_error::pg_strsignal(signo)),
                3844,
                "LogChildExit",
            ));
        }
        if let Some(first) = lines.first_mut() {
            first.detail = report.activity.clone().map(|a| format!("Failed process was running: {a}"));
        }
        if !report.ready || report.startup_crashed {
            lines.push(CrashLine::new("shutting down due to startup process failure".into(), 3190, ""));
            log_lines(&lines);
            remove_leftovers(handoff_field(&handoff, 0));
            std::process::exit(1);
        }
        lines.push(CrashLine::new(
            "terminating any other active server processes".into(),
            2804,
            "HandleChildCrash",
        ));
        if !report.restart_after_crash {
            lines.push(CrashLine::new("shutting down because \"restart_after_crash\" is off".into(), 3202, ""));
            log_lines(&lines);
            remove_leftovers(handoff_field(&handoff, 0));
            std::process::exit(1);
        }
        lines.push(CrashLine::new(
            "all server processes terminated; reinitializing".into(),
            3205,
            "PostmasterStateMachine",
        ));
        *pgsync::lock(&RESTART_REPORT) = lines;
        restart = true;
    }
}

#[cfg(any(target_family = "wasm", pgrust_sim))]
pub fn supervise(argv: &[String]) -> PgResult<Vec<String>> {
    strip_switch(argv)
}

/// True in a server process that runs under the supervisor.
pub fn supervised() -> bool {
    STATUS_FD.load(Ordering::Acquire) >= 0
}

/// True in a generation launched after the previous one crashed: its
/// postmaster starts inside C's crash cycle (FatalError set, sockets
/// inherited, no startup banner).
pub fn is_crash_restart() -> bool {
    CRASH_RESTART.load(Ordering::Acquire)
}

/// The supervised child's status report; a no-op unsupervised.
pub fn notify(byte: u8) {
    write_status(&[byte]);
}

fn write_status(bytes: &[u8]) {
    let fd = STATUS_FD.load(Ordering::Acquire);
    if fd < 0 {
        return;
    }
    // SAFETY: a short write to a pipe fd this process owns (atomic below
    // PIPE_BUF); a failed write (the supervisor is gone) is ignored.
    unsafe {
        libc::write(fd, bytes.as_ptr().cast(), bytes.len());
    }
}

/// PM_RUN or PM_HOT_STANDBY reached: report readiness, then the current restart_after_crash.
pub fn notify_ready() {
    if !supervised() {
        return;
    }
    notify(b'R');
    notify_restart_after_crash();
}

pub fn notify_restart_after_crash() {
    notify(if guc_tables::vars::restart_after_crash.read() { b'y' } else { b'n' });
}

/// Send PgStartTime and the postmaster's current settings for the next
/// generation; a no-op unsupervised.
pub fn publish_config(start_time: i64) {
    let fd = CONFIG_FD.load(Ordering::Acquire);
    if fd < 0 {
        return;
    }
    let mut image = Vec::new();
    image.extend_from_slice(&[0u8; 4]);
    image.extend_from_slice(&start_time.to_le_bytes());
    guc::store::write_postmaster_gucs(&mut image);
    let len = (image.len() - 4) as u32;
    image[..4].copy_from_slice(&len.to_le_bytes());
    let mut at = 0;
    while at < image.len() {
        // SAFETY: write(2) from a live buffer to a pipe fd this process owns.
        let n = unsafe { libc::write(fd, image[at..].as_ptr().cast(), image.len() - at) };
        if n < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        if n <= 0 {
            return;
        }
        at += n as usize;
    }
}

/// In a crash-restarted generation: C's report of the previous generation's
/// crash, for the postmaster to log once its log destination is set up.
pub fn take_crash_report() -> Vec<CrashLine> {
    if !is_crash_restart() {
        return Vec::new();
    }
    std::mem::take(&mut *pgsync::lock(&RESTART_REPORT))
}

/// In a crash-restarted generation: the previous generation's PgStartTime
/// and settings image (None when it never sent one).
pub fn take_restart_config() -> Option<(i64, Vec<u8>)> {
    if !is_crash_restart() {
        return None;
    }
    let mut image = std::mem::take(&mut *pgsync::lock(&RESTART_CONFIG));
    let start = i64::from_le_bytes(*image.first_chunk::<8>()?);
    image.drain(..8);
    Some((start, image))
}

/// The supervisor's view of the config pipe: bytes read so far and the last
/// complete image (without its length prefix).
#[cfg(not(any(target_family = "wasm", pgrust_sim)))]
struct ConfigImages {
    fd: i32,
    buf: Vec<u8>,
    latest: Vec<u8>,
}

#[cfg(not(any(target_family = "wasm", pgrust_sim)))]
impl ConfigImages {
    fn read(&mut self) -> bool {
        let mut chunk = [0u8; 4096];
        // SAFETY: read(2) into a stack buffer.
        let n = unsafe { libc::read(self.fd, chunk.as_mut_ptr().cast(), chunk.len()) };
        if n <= 0 {
            return n == 0;
        }
        self.feed(&chunk[..n as usize]);
        false
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        while let Some((len, rest)) = self.buf.split_first_chunk::<4>() {
            let len = u32::from_le_bytes(*len) as usize;
            if rest.len() < len {
                break;
            }
            self.latest.clear();
            self.latest.extend_from_slice(&rest[..len]);
            self.buf.drain(..4 + len);
        }
    }

    fn drain(&mut self) {
        // SAFETY: fcntl(2) on a pipe fd the supervisor owns; the writer is
        // gone, so a nonblocking read only ends a short final image sooner.
        unsafe {
            libc::fcntl(self.fd, libc::F_SETFL, libc::O_NONBLOCK);
        }
        loop {
            let mut chunk = [0u8; 4096];
            // SAFETY: read(2) into a stack buffer.
            let n = unsafe { libc::read(self.fd, chunk.as_mut_ptr().cast(), chunk.len()) };
            if n < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            if n <= 0 {
                return;
            }
            self.feed(&chunk[..n as usize]);
        }
    }
}

/// Crash-signal handler hook: async-signal-safe (stack buffer + write(2)).
pub fn notify_doomed(backend_type: &str, pid: i32, sig: i32, activity: &[u8]) {
    if STATUS_FD.load(Ordering::Acquire) < 0 {
        return;
    }
    let mut buf = [0u8; CRASH_RECORD_MAX];
    let mut at = 0;
    let mut put = |s: &[u8]| {
        for &b in s {
            if at < buf.len() - 1 {
                buf[at] = b;
                at += 1;
            }
        }
    };
    put(b"K");
    put(backend_type.as_bytes());
    for v in [pid, sig] {
        put(b"\0");
        let mut digits = [0u8; 12];
        let mut n = digits.len();
        let mut v = v.unsigned_abs();
        loop {
            n -= 1;
            digits[n] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        put(&digits[n..]);
    }
    put(b"\0");
    put(activity);
    buf[at] = 0;
    write_status(&buf[..at + 1]);
}

/// The postmaster finished C's crash choreography (every other child has
/// exited, the peers got their 57P02): end this generation so the
/// supervisor launches a fresh one on the same sockets. `_exit` skips the
/// exit callbacks: the socket files, lock files and postmaster.pid stay for
/// the next generation, which runs crash recovery.
pub fn exit_for_restart() -> ! {
    // D4812: best effort, the memory threads' queued log lines first (bounded: the generation is ending)
    let _ = memruntime::memlog::drain(core::time::Duration::from_millis(memruntime::memlog::CRASH_WAIT_MS));
    notify(b'C');
    // SAFETY: immediate process exit; nothing may run after this.
    unsafe { libc::_exit(1) }
}

/// Pass this generation's listen sockets to the supervisor with
/// `fields` = [DataDir, postmaster.pid listen-address line, socket-dir line];
/// the next generation inherits them. A no-op unsupervised.
#[cfg(not(any(target_family = "wasm", pgrust_sim)))]
pub fn hand_off_listen_sockets(fds: &[i32], fields: &[&str; 3]) {
    let sock = HANDOFF_FD.swap(-1, Ordering::AcqRel);
    if sock < 0 {
        return;
    }
    let fds = &fds[..fds.len().min(MAXLISTEN)];
    let mut payload = [0u8; HANDOFF_BYTES];
    let mut at = 0;
    for f in fields {
        let b = &f.as_bytes()[..f.len().min(types_core::MAXPGPATH)];
        payload[at..at + 4].copy_from_slice(&(b.len() as u32).to_ne_bytes());
        payload[at + 4..at + 4 + b.len()].copy_from_slice(b);
        at += 4 + b.len();
    }
    let mut cmsg = [0u64; cmsg_words()];
    // SAFETY: msghdr/cmsghdr built per sendmsg(2)/cmsg(3) over buffers that
    // outlive the call; the control buffer is sized by CMSG_SPACE.
    unsafe {
        let mut iov = libc::iovec { iov_base: payload.as_mut_ptr().cast(), iov_len: at };
        let mut msg: libc::msghdr = core::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        if !fds.is_empty() {
            let bytes = (fds.len() * core::mem::size_of::<i32>()) as u32;
            msg.msg_control = cmsg.as_mut_ptr().cast();
            msg.msg_controllen = libc::CMSG_SPACE(bytes) as _;
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(bytes) as _;
            core::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(c).cast::<i32>(), fds.len());
        }
        libc::sendmsg(sock, &msg, 0);
        libc::close(sock);
    }
}

const HANDOFF_BYTES: usize = 3 * (4 + types_core::MAXPGPATH);

/// Field `i` of a handoff payload (empty when absent or malformed).
fn handoff_field(payload: &[u8], i: usize) -> &[u8] {
    let mut at = 0;
    for k in 0..=i {
        let Some(len) = payload.get(at..at + 4) else { return &[] };
        let len = u32::from_ne_bytes([len[0], len[1], len[2], len[3]]) as usize;
        let Some(field) = payload.get(at + 4..at + 4 + len) else { return &[] };
        if k == i {
            return field;
        }
        at += 4 + len;
    }
    &[]
}


#[cfg(any(target_family = "wasm", pgrust_sim))]
pub fn hand_off_listen_sockets(_fds: &[i32], _fields: &[&str; 3]) {}

#[cfg(not(any(target_family = "wasm", pgrust_sim)))]
const fn cmsg_words() -> usize {
    // CMSG_SPACE(MAXLISTEN * sizeof(int)) rounded up to u64 words, plus slack
    // for the header on every platform.
    (MAXLISTEN * 4 + 64) / 8
}

/// Read the last handoff this generation sent (if any): it replaces the
/// held sockets and the recorded payload.
#[cfg(not(any(target_family = "wasm", pgrust_sim)))]
/// True once the sending end is closed.
fn receive_handoff(sock: i32, handoff: &mut Vec<u8>) -> bool {
    loop {
        let mut payload = [0u8; HANDOFF_BYTES];
        let mut cmsg = [0u64; cmsg_words()];
        // SAFETY: recvmsg(2) into stack buffers; control data parsed with the
        // cmsg(3) accessors within msg_controllen.
        let (n, fds, nfds) = unsafe {
            let mut iov = libc::iovec { iov_base: payload.as_mut_ptr().cast(), iov_len: payload.len() };
            let mut msg: libc::msghdr = core::mem::zeroed();
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = cmsg.as_mut_ptr().cast();
            msg.msg_controllen = (cmsg.len() * 8) as _;
            let n = libc::recvmsg(sock, &mut msg, libc::MSG_DONTWAIT);
            let mut fds = [-1i32; MAXLISTEN];
            let mut nfds = 0;
            if n > 0 {
                let mut c = libc::CMSG_FIRSTHDR(&msg);
                while !c.is_null() {
                    if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                        let data = libc::CMSG_DATA(c).cast::<i32>();
                        let len = (*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                        for i in 0..len / 4 {
                            let fd = core::ptr::read_unaligned(data.add(i));
                            if nfds < MAXLISTEN {
                                fds[nfds] = fd;
                                nfds += 1;
                            } else {
                                libc::close(fd);
                            }
                        }
                    }
                    c = libc::CMSG_NXTHDR(&msg, c);
                }
            }
            (n, fds, nfds)
        };
        if n <= 0 {
            return n == 0;
        }
        handoff.clear();
        handoff.extend_from_slice(&payload[..n as usize]);
        let old = HELD_N.swap(0, Ordering::AcqRel);
        for slot in HELD.iter().take(old) {
            // SAFETY: an fd the supervisor received earlier and owns.
            unsafe {
                libc::close(slot.swap(-1, Ordering::AcqRel));
            }
        }
        for (i, fd) in fds.iter().take(nfds).enumerate() {
            // SAFETY: a received fd; the next generation inherits it across
            // fork, anything the server execs must not.
            unsafe {
                libc::fcntl(*fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
            HELD[i].store(*fd, Ordering::Release);
        }
        HELD_N.store(nfds, Ordering::Release);
    }
}

/// C's ExitPostmaster runs the proc_exit callbacks that remove
/// postmaster.pid, the socket files and their lock files; a child killed by
/// a signal never ran them, so the supervisor does it on its final exit,
/// only for files that still carry its pid (the lock owner, see
/// init_small::globals::launched_pid).
#[cfg(not(any(target_family = "wasm", pgrust_sim)))]
fn remove_leftovers(data_dir: &[u8]) {
    if !data_dir.is_empty() {
        remove_if_owned(data_dir, b"/postmaster.pid", None);
    }
    for slot in HELD.iter().take(HELD_N.load(Ordering::Acquire)) {
        if let Some(sock_path) = unix_path_of(slot.load(Ordering::Acquire)) {
            remove_if_owned(&sock_path, b".lock", Some(&sock_path));
        }
    }
}

/// Remove `<base><suffix>` (and `sock`) if its first line is our pid.
#[cfg(not(any(target_family = "wasm", pgrust_sim)))]
fn remove_if_owned(base: &[u8], suffix: &[u8], sock: Option<&[u8]>) {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let mut lock = Vec::new();
    if lock.try_reserve(base.len() + suffix.len()).is_err() {
        return;
    }
    lock.extend_from_slice(base);
    lock.extend_from_slice(suffix);
    let Ok(text) = std::fs::read(OsStr::from_bytes(&lock)) else {
        return;
    };
    let first = text.split(|&b| b == b'\n').next().unwrap_or(&[]);
    if std::str::from_utf8(first).ok().and_then(|s| s.trim().parse::<i32>().ok())
        != Some(process_id() as i32)
    {
        return;
    }
    if let Some(sock) = sock {
        let _ = std::fs::remove_file(OsStr::from_bytes(sock));
    }
    let _ = std::fs::remove_file(OsStr::from_bytes(&lock));
}

/// The filesystem path of a Unix-domain listen socket (None for TCP or an
/// abstract socket).
#[cfg(not(any(target_family = "wasm", pgrust_sim)))]
fn unix_path_of(fd: i32) -> Option<Vec<u8>> {
    // SAFETY: getsockname(2) into a zeroed sockaddr_un of the stated size.
    let (su, len) = unsafe {
        let mut su: libc::sockaddr_un = core::mem::zeroed();
        let mut len = core::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
        if libc::getsockname(fd, (&raw mut su).cast(), &mut len) != 0 {
            return None;
        }
        (su, len)
    };
    if su.sun_family as i32 != libc::AF_UNIX || len as usize <= core::mem::offset_of!(libc::sockaddr_un, sun_path) {
        return None;
    }
    let len = su.sun_path.iter().position(|&c| c == 0).unwrap_or(su.sun_path.len());
    let mut path = Vec::new();
    path.try_reserve(len).ok()?;
    path.extend(su.sun_path[..len].iter().map(|&c| c as u8));
    if path.is_empty() || path[0] == b'@' {
        return None;
    }
    Some(path)
}

/// The supervisor writes the stock `%m [%p] LOG:  ` line itself (it never
/// loads GUCs); the zone is the system's, which initdb records as
/// log_timezone.
#[cfg(not(any(target_family = "wasm", pgrust_sim)))]
fn log_lines(lines: &[CrashLine]) {
    for l in lines {
        log(&l.message);
        if let Some(d) = &l.detail {
            log_at("DETAIL", d);
        }
    }
}

#[cfg(not(any(target_family = "wasm", pgrust_sim)))]
fn log(msg: &str) {
    log_at("LOG", msg);
}

#[cfg(not(any(target_family = "wasm", pgrust_sim)))]
fn log_at(level: &str, msg: &str) {
    let mut tv = libc::timeval { tv_sec: 0, tv_usec: 0 };
    let mut tm: libc::tm = unsafe { core::mem::zeroed() };
    let mut stamp = [0u8; 32];
    // SAFETY: valid out-pointers; strftime bounds itself to the buffer.
    let n = unsafe {
        libc::gettimeofday(&mut tv, core::ptr::null_mut());
        libc::localtime_r(&tv.tv_sec, &mut tm);
        libc::strftime(stamp.as_mut_ptr().cast(), stamp.len(), c"%Y-%m-%d %H:%M:%S".as_ptr(), &tm)
    };
    let zone = if tm.tm_zone.is_null() {
        String::new()
    } else {
        // SAFETY: tm_zone is a NUL-terminated static string from libc.
        unsafe { std::ffi::CStr::from_ptr(tm.tm_zone) }.to_string_lossy().into_owned()
    };
    elog::write_stderr(&format!(
        "{}.{:03} {zone} [{}] {level}:  {msg}\n",
        String::from_utf8_lossy(&stamp[..n]),
        tv.tv_usec / 1000,
        process_id()
    ));
}

#[cfg(not(any(target_family = "wasm", pgrust_sim)))]
fn cloexec_pair(fds: [i32; 2], what: &str, rc: i32) -> (i32, i32) {
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        elog::write_stderr(&format!("postgres: could not create {what}: {err}\n"));
        std::process::exit(1);
    }
    for fd in fds {
        // SAFETY: valid fd just created; nothing the server execs may
        // inherit it.
        unsafe {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
    }
    (fds[0], fds[1])
}

#[cfg(not(any(target_family = "wasm", pgrust_sim)))]
fn pipe() -> (i32, i32) {
    let mut fds = [0i32; 2];
    // SAFETY: pipe(2) fills the two-element array.
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    cloexec_pair(fds, "pipe", rc)
}

#[cfg(not(any(target_family = "wasm", pgrust_sim)))]
fn socket_pair() -> (i32, i32) {
    let mut fds = [0i32; 2];
    // SAFETY: socketpair(2) fills the two-element array.
    let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
    cloexec_pair(fds, "socket pair", rc)
}

/// Wait for the server child while reading its status pipe. Also reaps any
/// other exited process (a supervisor that is PID 1 in a container inherits
/// orphans). A child whose crash handler ran is SIGKILLed if it is still
/// alive DOOMED_GRACE_MS later: a process that logged its crash but never
/// died would otherwise hold the sockets and answer nobody.
#[cfg(not(any(target_family = "wasm", pgrust_sim)))]
fn watch_child(
    pid: i32,
    status_rd: i32,
    hand_rd: i32,
    handoff: &mut Vec<u8>,
    images: &mut ConfigImages,
) -> (i32, ChildReport) {
    let mut report = ChildReport::new();
    let mut pending = Vec::new();
    let mut eof = false;
    let mut killed = false;
    let mut hand_open = true;
    let mut config_open = true;
    loop {
        if let Some(status) = reap(pid, !eof) {
            drain(status_rd, &mut report, &mut pending);
            images.drain();
            if hand_open {
                receive_handoff(hand_rd, handoff);
            }
            return (status, report);
        }
        if eof {
            continue;
        }
        let timeout = if report.doomed && !killed { DOOMED_GRACE_MS } else { 1000 };
        let mut pfds = [
            libc::pollfd { fd: status_rd, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: if hand_open { hand_rd } else { -1 }, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: if config_open { images.fd } else { -1 }, events: libc::POLLIN, revents: 0 },
        ];
        // SAFETY: three valid pollfds.
        let r = unsafe { libc::poll(pfds.as_mut_ptr(), 3, timeout) };
        if r > 0 && pfds[2].revents != 0 {
            config_open = !images.read();
        }
        if r == 0 && report.doomed && !killed {
            // SAFETY: kill(2) of our own child.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
            killed = true;
        }
        if r > 0 && pfds[1].revents != 0 {
            // Take the sockets off the wire at once: rights left in flight
            // on a unix socket can be flushed by the kernel's descriptor
            // garbage collector (macOS), killing the listen socket.
            hand_open = !receive_handoff(hand_rd, handoff);
        }
        if r > 0 && pfds[0].revents != 0 {
            let mut buf = [0u8; 256];
            // SAFETY: read(2) into a stack buffer.
            let n = unsafe { libc::read(status_rd, buf.as_mut_ptr().cast(), buf.len()) };
            if n == 0 {
                eof = true;
            } else if n > 0 {
                report.feed(&buf[..n as usize], &mut pending);
            }
        }
    }
}

/// Reap exited processes; Some(status) once `pid` is among them. `nohang`
/// false blocks until something exits.
#[cfg(not(any(target_family = "wasm", pgrust_sim)))]
fn reap(pid: i32, nohang: bool) -> Option<i32> {
    loop {
        let mut status = 0i32;
        let flags = if nohang { libc::WNOHANG } else { 0 };
        // SAFETY: status is a valid out-pointer.
        let r = unsafe { libc::waitpid(-1, &mut status, flags) };
        if r == pid {
            return Some(status);
        }
        if r > 0 {
            continue;
        }
        if r < 0 {
            match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::EINTR) => continue,
                // ECHILD: nothing left to reap, our child included.
                Some(libc::ECHILD) => return Some(0),
                _ => return None,
            }
        }
        return None;
    }
}

/// Read what is left in the status pipe (the child's end died with it).
#[cfg(not(any(target_family = "wasm", pgrust_sim)))]
fn drain(fd: i32, report: &mut ChildReport, pending: &mut Vec<u8>) {
    let mut buf = [0u8; 256];
    loop {
        // SAFETY: read(2) into a stack buffer.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        if n <= 0 {
            return;
        }
        report.feed(&buf[..n as usize], pending);
    }
}

#[cfg(not(any(target_family = "wasm", pgrust_sim)))]
extern "C" fn forward_signal(sig: libc::c_int) {
    let pid = CHILD_PID.load(Ordering::Acquire);
    if pid > 0 {
        // SAFETY: kill(2) is async-signal-safe.
        unsafe {
            libc::kill(pid, sig);
        }
    }
}

#[cfg(not(any(target_family = "wasm", pgrust_sim)))]
fn install_forwarders() {
    for sig in FORWARDED {
        // SAFETY: plain handler install at single-threaded boot; the handler
        // only calls kill(2).
        unsafe {
            let mut sa: libc::sigaction = core::mem::zeroed();
            sa.sa_sigaction = forward_signal as extern "C" fn(libc::c_int) as usize;
            libc::sigemptyset(&mut sa.sa_mask);
            sa.sa_flags = libc::SA_RESTART;
            libc::sigaction(sig, &sa, core::ptr::null_mut());
        }
    }
}

/// The child inherits the forwarders; PostmasterMain installs its own for
/// these signals, but until then the defaults must apply.
#[cfg(not(any(target_family = "wasm", pgrust_sim)))]
fn restore_default_dispositions() {
    for sig in FORWARDED {
        // SAFETY: resetting to SIG_DFL in the fresh child.
        unsafe {
            let mut sa: libc::sigaction = core::mem::zeroed();
            sa.sa_sigaction = libc::SIG_DFL;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(sig, &sa, core::ptr::null_mut());
        }
    }
}

/// Block on the life pipe; EOF means the supervisor died, and a server
/// without its supervisor dies with it, as a C backend dies with its
/// postmaster.
#[cfg(not(any(target_family = "wasm", pgrust_sim)))]
fn spawn_life_watcher(fd: i32) {
    let spawned = std::thread::Builder::new()
        .name("pg:supervisor-life".into())
        .stack_size(64 * 1024)
        .spawn(pagegive::pinned(move || {
            let mut b = 0u8;
            loop {
                // SAFETY: one-byte read on a pipe fd this thread owns.
                let n = unsafe { libc::read(fd, (&mut b as *mut u8).cast(), 1) };
                if n < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                if n <= 0 {
                    break;
                }
            }
            // SAFETY: self-directed SIGKILL.
            unsafe {
                libc::kill(process_id() as i32, libc::SIGKILL);
            }
        }));
    if spawned.is_err() {
        elog::write_stderr("postgres: could not start the supervisor watcher thread\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_supervisor_switch_is_recognised_and_stripped() {
        let argv: Vec<String> =
            ["postgres", "-D", "dd", "--no-supervisor"].iter().map(|s| s.to_string()).collect();
        assert!(!wants_supervisor(&argv));
        let plain: Vec<String> = ["postgres", "-D", "dd"].iter().map(|s| s.to_string()).collect();
        assert!(wants_supervisor(&plain));
        assert_eq!(strip_switch(&argv).expect("small argv"), plain);
    }

    #[test]
    fn notify_is_inert_unsupervised() {
        assert_eq!(STATUS_FD.load(Ordering::Acquire), -1);
        assert!(!supervised());
        notify(b'R');
        notify_doomed("client backend", 1040, 11, b"SELECT 1");
    }

    #[test]
    fn status_stream_parses_across_reads() {
        let mut r = ChildReport::new();
        let mut pending = Vec::new();
        r.feed(b"Ryn", &mut pending);
        assert!(r.ready && !r.restart_after_crash && !r.doomed);
        r.feed(b"Kclient back", &mut pending);
        r.feed(b"end\x001040\x0011\x00SELECT\n  '5'::bt4", &mut pending);
        assert!(r.doomed && r.crashed.is_none());
        r.feed(b"\x00", &mut pending);
        assert_eq!(r.crashed, Some(("client backend (PID 1040)".into(), 11)));
        assert_eq!(r.activity.as_deref(), Some("SELECT\n  '5'::bt4"));
        r.feed(b"yC", &mut pending);
        assert!(r.restart_after_crash && r.restart_requested && !r.startup_crashed);
    }

    #[test]
    fn startup_crash_record_is_flagged_after_ready() {
        let mut r = ChildReport::new();
        let mut pending = Vec::new();
        r.feed(b"Ry", &mut pending);
        r.feed(b"Kstartup\x0077\x0011\x00\x00", &mut pending);
        assert!(r.ready && r.startup_crashed);
        assert_eq!(r.crashed, Some(("startup (PID 77)".into(), 11)));
        assert_eq!(r.activity, None);
        let mut b = ChildReport::new();
        b.feed(b"RyKstartup worker\x0078\x0011\x00\x00", &mut pending);
        assert!(!b.startup_crashed);
    }

    #[test]
    fn malformed_crash_record_falls_back() {
        assert_eq!(parse_crash_record(b"client backend\x00"), None);
        assert_eq!(parse_crash_record(b"\x001\x0011"), None);
        assert_eq!(parse_crash_record(b"x\x001a\x0011"), None);
        assert_eq!(parse_crash_record(b"x\x0012"), None);
        assert_eq!(
            parse_crash_record(b"checkpointer\x0012\x006\x00"),
            Some(("checkpointer (PID 12)".into(), 6, None))
        );
    }

    #[cfg(not(any(target_family = "wasm", pgrust_sim)))]
    #[test]
    fn handoff_passes_listen_sockets_and_data_dir() {
        let (rd, wr) = socket_pair();
        // SAFETY: a throwaway TCP listen socket on an ephemeral loopback port.
        let lsock = unsafe {
            let s = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
            let mut sa: libc::sockaddr_in = core::mem::zeroed();
            sa.sin_family = libc::AF_INET as _;
            sa.sin_addr.s_addr = u32::from_ne_bytes([127, 0, 0, 1]);
            assert_eq!(
                libc::bind(s, (&raw const sa).cast(), core::mem::size_of::<libc::sockaddr_in>() as _),
                0
            );
            assert_eq!(libc::listen(s, 4), 0);
            s
        };
        HANDOFF_FD.store(wr, Ordering::Release);
        hand_off_listen_sockets(&[lsock], &["/tmp/some/data", "127.0.0.1", ""]);
        assert_eq!(HANDOFF_FD.load(Ordering::Acquire), -1);
        let mut payload = Vec::new();
        receive_handoff(rd, &mut payload);
        assert_eq!(handoff_field(&payload, 0), b"/tmp/some/data");
        assert_eq!(handoff_field(&payload, 1), b"127.0.0.1");
        assert_eq!(handoff_field(&payload, 2), b"");
        assert_eq!(handoff_field(&payload, 3), b"");
        assert_eq!(handoff_field(&payload[..6], 0), b"");
        assert_eq!(HELD_N.load(Ordering::Acquire), 1);
        let got = HELD[0].load(Ordering::Acquire);
        assert!(got >= 0 && got != lsock);
        // SAFETY: both fds name the same socket: same local address.
        unsafe {
            let mut a: libc::sockaddr_in = core::mem::zeroed();
            let mut b: libc::sockaddr_in = core::mem::zeroed();
            let mut la = core::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
            let mut lb = la;
            libc::getsockname(lsock, (&raw mut a).cast(), &mut la);
            libc::getsockname(got, (&raw mut b).cast(), &mut lb);
            assert_eq!(a.sin_port, b.sin_port);
            libc::close(lsock);
            libc::close(got);
            libc::close(rd);
        }
        HELD[0].store(-1, Ordering::Release);
        HELD_N.store(0, Ordering::Release);
    }
}
