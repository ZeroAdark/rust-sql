// pg_stat_wal.wal_buffers_full differential (#2462): one session's insert
// workload under the same wal_buffers pressure, with the walwriter parked so
// the only page write-outs are the forced ones AdvanceXLInsertBuffer counts,
// must count the same on pgrust and on the C 18 oracle. Oracle:
// $PGRUST_C_ORACLE / $PGINSTALL / $PG_BINDIR, else the usual install dirs;
// pgrust: $PGRUST_BIN's dir, else the postgres + initdb beside this test
// binary's profile dir. Either absent = SKIP unless PGRUST_REQUIRE_ORACLE=1.
#![cfg(all(not(target_family = "wasm"), not(pgrust_sim)))]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const WAL_BUFFERS: &str = "64kB";
const ROWS: u32 = 20_000;
const STAT: &str = "select wal_records, wal_bytes, wal_buffers_full from pg_catalog.pg_stat_wal";

fn oracle_bin() -> Option<PathBuf> {
    let mut cands: Vec<PathBuf> = ["PGRUST_C_ORACLE", "PGINSTALL", "PG_BINDIR"]
        .iter()
        .filter_map(|k| std::env::var_os(k))
        .map(PathBuf::from)
        .collect();
    if let Some(h) = std::env::var_os("HOME") {
        cands.push(Path::new(&h).join("dev/pg186/bin"));
    }
    cands.push("/usr/lib/postgresql/18/bin".into());
    cands.push("/opt/homebrew/opt/postgresql@18/bin".into());
    cands.into_iter().find(|d| d.join("initdb").is_file() && d.join("psql").is_file())
}

fn pgrust_bin() -> Option<PathBuf> {
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Some(b) = std::env::var_os("PGRUST_BIN") {
        if let Some(d) = Path::new(&b).parent() {
            cands.push(d.to_path_buf());
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        // target/<profile>/deps/<test> -> target/<profile>
        if let Some(d) = exe.parent().and_then(Path::parent) {
            cands.push(d.to_path_buf());
        }
    }
    cands.into_iter().find(|d| d.join("postgres").is_file() && d.join("initdb").is_file())
}

fn skip(reason: &str) -> bool {
    if std::env::var_os("PGRUST_REQUIRE_ORACLE").is_some() {
        panic!("{reason}");
    }
    eprintln!("SKIP wal_buffers_full_matches_oracle: {reason}");
    true
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

struct Server {
    child: Child,
    port: u16,
    psql: PathBuf,
    datadir: PathBuf,
}

impl Server {
    fn start(bin: &Path, psql: &Path, datadir: &Path) -> Server {
        let _ = std::fs::remove_dir_all(datadir);
        std::fs::create_dir_all(datadir).unwrap();
        let log = std::fs::File::create(datadir.with_extension("initdb.log")).unwrap();
        let st = Command::new(bin.join("initdb"))
            .args(["-D", datadir.to_str().unwrap(), "-U", "postgres", "--no-sync"])
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .status()
            .unwrap();
        assert!(st.success(), "initdb failed: {}", bin.display());
        let port = free_port();
        let log = std::fs::File::create(datadir.with_extension("server.log")).unwrap();
        let child = Command::new(bin.join("postgres"))
            .args(["-D", datadir.to_str().unwrap(), "-p", &port.to_string()])
            .args(["-c", "listen_addresses=127.0.0.1", "-c", "unix_socket_directories="])
            .args(["-c", "fsync=off", "-c", "shared_buffers=128MB"])
            .args(["-c", &format!("wal_buffers={WAL_BUFFERS}"), "-c", "wal_writer_delay=10000"])
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap();
        let s = Server { child, port, psql: psql.to_path_buf(), datadir: datadir.to_path_buf() };
        let deadline = Instant::now() + Duration::from_secs(60);
        while s.try_sql("select 1").is_none() {
            assert!(Instant::now() < deadline, "server did not come up: {}", datadir.display());
            std::thread::sleep(Duration::from_millis(200));
        }
        s
    }

    fn try_sql(&self, sql: &str) -> Option<String> {
        let out = Command::new(&self.psql)
            .args(["-h", "127.0.0.1", "-p", &self.port.to_string(), "-U", "postgres", "-d", "postgres"])
            .args(["-X", "-At", "-v", "ON_ERROR_STOP=1", "-c", sql])
            .env_remove("PGHOST")
            .env_remove("PGPORT")
            .output()
            .ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn sql(&self, sql: &str) -> String {
        self.try_sql(sql).unwrap_or_else(|| panic!("psql failed on port {}: {sql}", self.port))
    }

    fn stat(&self) -> (i64, u64, i64) {
        let row = self.sql(STAT);
        let mut it = row.split('|').map(str::trim);
        let mut next = || it.next().unwrap();
        (next().parse().unwrap(), next().parse().unwrap(), next().parse().unwrap())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // SAFETY: plain signal to the child pid this struct owns; SIGINT is
        // the fast-shutdown request both engines honor.
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGINT);
        }
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.datadir);
    }
}

fn workload(s: &Server) -> (i64, u64, i64) {
    s.sql("create table t(a int, b text)");
    let before = s.stat();
    s.sql(&format!("insert into t select g, repeat('x',100) from generate_series(1,{ROWS}) g"));
    // pg_stat_wal is flushed at the writer session's exit.
    std::thread::sleep(Duration::from_millis(300));
    let after = s.stat();
    (after.0 - before.0, after.1 - before.1, after.2 - before.2)
}

#[test]
fn wal_buffers_full_matches_oracle() {
    let Some(oracle) = oracle_bin() else {
        if skip("no C PostgreSQL 18 oracle (set PGRUST_C_ORACLE)") {
            return;
        }
        unreachable!()
    };
    let Some(pgrust) = pgrust_bin() else {
        if skip("no pgrust postgres+initdb beside the test binary (cargo build -p main_main -p initdb, or set PGRUST_BIN)") {
            return;
        }
        unreachable!()
    };
    let psql = oracle.join("psql");
    let work = std::env::temp_dir().join(format!("pgrust-walbuf-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).unwrap();

    let c = Server::start(&oracle, &psql, &work.join("c"));
    let (c_recs, c_bytes, c_full) = workload(&c);
    drop(c);
    let r = Server::start(&pgrust, &psql, &work.join("r"));
    let (r_recs, r_bytes, r_full) = workload(&r);
    drop(r);
    let _ = std::fs::remove_dir_all(&work);

    eprintln!("C: records={c_recs} bytes={c_bytes} buffers_full={c_full}");
    eprintln!("pgrust: records={r_recs} bytes={r_bytes} buffers_full={r_full}");
    assert!(c_full > 0, "oracle saw no buffer pressure at wal_buffers={WAL_BUFFERS}");
    assert!(r_full > 0, "pgrust counted no forced write-outs at wal_buffers={WAL_BUFFERS}");
    // Same WAL volume (the catalogs' start LSNs differ, so a few page-header
    // crossings of slack) and the same write-out count to within a page or
    // two of that slack; a wrong increment site is off by 10x either way.
    assert!((c_recs - r_recs).abs() <= 1, "wal_records C={c_recs} pgrust={r_recs}");
    assert!(c_bytes.abs_diff(r_bytes) <= 512, "wal_bytes C={c_bytes} pgrust={r_bytes}");
    let tol = (c_full / 50).max(4);
    assert!(
        (c_full - r_full).abs() <= tol,
        "wal_buffers_full C={c_full} pgrust={r_full} (tolerance ±{tol})"
    );
}
