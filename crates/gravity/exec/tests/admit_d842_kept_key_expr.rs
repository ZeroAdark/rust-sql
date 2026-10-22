//! D842: a statement the simple protocol serves builds its key expressions' ExprStates in the kept context (admit.rs
//! `bind_words`, execute.rs KEPT); an ExprState's result cell (execexpr's `resnd`) is freed by its context's reset only, as
//! C's palloc, so an exact context reset at the statement's end raised "context "GravitySimple" reset with 16 bytes still
//! charged" in a debug server. Found by commands_matview's refresh_freeze_e2e (its `SELECT relfilenode FROM pg_class WHERE
//! relname = '..'`). Here: the same statement shape, served (fallback off), against this workspace's debug server.
//!
//! Needs a PostgreSQL 18 install for initdb / psql (PGINSTALL, as refresh_freeze_e2e); without one it prints SKIPPED on stderr.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// D2589: the server's port, from this process's pid (as gravity-e2e.sh derives its ports): with no TCP listener it only
/// names the socket in the run's own socket dir (`sock_dir`), so two checks on one box never share a socket or a lock file.
fn port() -> u16 {
    20000 + (std::process::id() % 20000) as u16
}

/// D2589: a socket dir of this run alone, under /tmp (the Unix socket path's 103-byte kernel cap rules out target/).
fn sock_dir() -> PathBuf {
    PathBuf::from(format!("/tmp/pgd842.{}.{}", std::process::id(), port()))
}

fn find_pgbin() -> Option<PathBuf> {
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Ok(p) = std::env::var("PGINSTALL") {
        if !p.is_empty() {
            cands.push(PathBuf::from(p));
        }
    }
    cands.push(PathBuf::from("/tmp/pgrust_pginstall/bin"));
    cands.push(PathBuf::from("/opt/homebrew/bin"));
    cands.push(PathBuf::from("/opt/homebrew/opt/postgresql@18/bin"));
    cands.into_iter().find(|c| c.join("initdb").is_file())
}

/// This workspace's postgres binary for the running profile, built (a no-op when current), never merely reused.
/// PGRUST_TEST_SERVER_BIN: the binary a harness built for THIS tree and this test's profile (check.sh's ct server build)
/// is used as is, so no cargo build runs inside the crate-test pool (BD-14); unset or empty = the build below.
fn server_binary() -> PathBuf {
    if let Some(p) = std::env::var_os("PGRUST_TEST_SERVER_BIN").filter(|p| !p.is_empty()) {
        let bin = PathBuf::from(p);
        assert!(bin.is_file(), "PGRUST_TEST_SERVER_BIN={} is not a file", bin.display());
        return bin;
    }
    let exe = std::env::current_exe().expect("current_exe");
    let profile_dir = exe.parent().expect("deps dir").parent().expect("profile dir").to_path_buf();
    let bin = profile_dir.join("postgres");
    // Build in the SAME cargo profile as this test binary (target/<dir>; dir "debug" = profile "dev"),
    // so custom profiles (check.sh's check-test) find their own postgres.
    let dir = profile_dir.file_name().and_then(|n| n.to_str()).unwrap_or("debug").to_string();
    let profile = if dir == "debug" { "dev".to_string() } else { dir };
    let root = manifest_dir().ancestors().nth(3).expect("workspace root");
    let mut cmd = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cmd.current_dir(root).args(["build", "-p", "main_main", "--bin", "postgres"]);
    cmd.args(["--profile", profile.as_str()]);
    assert!(cmd.status().expect("spawn cargo build").success(), "cargo build of the postgres binary failed");
    assert!(bin.is_file(), "built server binary missing at {}", bin.display());
    bin
}

struct Server {
    child: Child,
    sock: PathBuf,
    pgbin: PathBuf,
    log: PathBuf,
}

impl Server {
    fn start(work: &Path, pgbin: &Path, bin: &Path) -> Server {
        let dd = work.join("dd");
        let sock = sock_dir();
        let log = work.join("server.log");
        let _ = std::fs::remove_dir_all(&sock);
        std::fs::create_dir_all(&sock).unwrap();
        let st = Command::new(pgbin.join("initdb"))
            .arg("-D")
            .arg(&dd)
            .args(["--no-locale", "--encoding=UTF8", "-U", "postgres", "-A", "trust"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("spawn initdb");
        assert!(st.success(), "initdb failed");
        let script = format!("ulimit -s 65520 2>/dev/null; exec \"$1\" -D \"$2\" -k \"$3\" -p {} -c listen_addresses= -c max_stack_depth=60000 -c io_method=sync -c autovacuum=off >>\"$4\" 2>&1", port());
        let child = Command::new("sh")
            .arg("-c")
            .arg(&script)
            .arg("srv")
            .arg(bin)
            .arg(&dd)
            .arg(&sock)
            .arg(&log)
            .env("RUST_MIN_STACK", "67108864")
            .env("RUST_BACKTRACE", "1")
            .spawn()
            .expect("spawn server");
        let mut srv = Server { child, sock, pgbin: pgbin.to_path_buf(), log };
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if let Ok(Some(st)) = srv.child.try_wait() {
                panic!("server exited ({st}) during startup; log tail:\n{}", srv.log_tail());
            }
            if srv.sql("SELECT 1").is_ok() {
                return srv;
            }
            assert!(Instant::now() < deadline, "server never became ready; log tail:\n{}", srv.log_tail());
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    fn log_tail(&self) -> String {
        let s = std::fs::read_to_string(&self.log).unwrap_or_default();
        let lines: Vec<&str> = s.lines().collect();
        lines[lines.len().saturating_sub(40)..].join("\n")
    }

    fn sql(&self, sql: &str) -> Result<String, String> {
        let mut child = Command::new(self.pgbin.join("psql"))
            .arg("-h")
            .arg(&self.sock)
            .args(["-p", &port().to_string(), "-U", "postgres", "-X", "-t", "-A", "-v", "ON_ERROR_STOP=1", "-f", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn psql");
        // A psql that finds no server yet (wait_ready's poll) can exit before it reads its script: the write then gets
        // EPIPE, which is a failed attempt like a failed exit, not a test failure.
        let wrote = child.stdin.take().unwrap().write_all(sql.as_bytes());
        let out = child.wait_with_output().expect("psql wait");
        let (o, e) = (String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned());
        if out.status.success() && wrote.is_ok() {
            Ok(o)
        } else {
            Err(format!("psql failed (stdin {wrote:?})\nstdout: {o}\nstderr: {e}"))
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // the server's own pid (the sh exec'd it): stopped whether the test passed or panicked
        let _ = Command::new("kill").args(["-INT", &self.child.id().to_string()]).status();
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline && !matches!(self.child.try_wait(), Ok(Some(_))) {
            std::thread::sleep(Duration::from_millis(200));
        }
        let _ = self.child.kill();
        let _ = std::fs::remove_dir_all(&self.sock);
    }
}

#[test]
fn a_served_key_expression_leaves_nothing_charged_in_the_kept_context() {
    let Some(pgbin) = find_pgbin() else {
        let _ = std::io::stderr().write_all(b"SKIPPED d842_kept_key_expr: no PostgreSQL 18 install (set PGINSTALL): NOTHING PROVEN (stderr uncaptured)\n");
        return;
    };
    let bin = server_binary();
    let work = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("d842_kept_key_expr");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).unwrap();
    let srv = Server::start(&work, &pgbin, &bin);
    srv.sql("CREATE TABLE mv (id int)").unwrap_or_else(|e| panic!("{e}\nserver log tail:\n{}", srv.log_tail()));
    // each call is its own backend: the statement below is the first (and only) served one of its session, as in the find
    for rel in ["mv", "pg_class"] {
        let q = format!("SELECT relname FROM pg_class WHERE relname = '{rel}';");
        match srv.sql(&q) {
            Ok(out) => assert_eq!(out.lines().last(), Some(rel), "{rel}: {out}"),
            Err(e) => panic!("{rel}: {e}\nserver log tail:\n{}", srv.log_tail()),
        }
    }
    drop(srv);
    let _ = std::fs::remove_dir_all(&work);
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
