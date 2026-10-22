// Every sslmode, verify-full name matching, hostssl cert auth, the client-key
// permission check, channel binding (SCRAM-SHA-256-PLUS), a wrong CA, direct
// negotiation, and a cleartext-only server, each driven through pgclient AND
// through the C oracle's psql (libpq) against the same live C 18.x server;
// the answer (pg_stat_ssl.ssl) or the connection error text must be
// byte-identical. Oracle: $PGRUST_C_ORACLE / $PGINSTALL / $PG_BINDIR, else
// the usual install dirs; absent = SKIP unless PGRUST_REQUIRE_ORACLE=1.
#![cfg(not(target_family = "wasm"))]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use openssl::asn1::Asn1Time;
use openssl::bn::BigNum;
use openssl::ec::{EcGroup, EcKey};
use openssl::hash::MessageDigest;
use openssl::nid::Nid;
use openssl::pkey::{PKey, Private};
use openssl::x509::extension::{BasicConstraints, ExtendedKeyUsage, SubjectAlternativeName};
use openssl::x509::{X509Name, X509NameBuilder, X509};

const QUERY: &str = "select ssl::text from pg_stat_ssl where pid = pg_backend_pid()";

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

fn keypair() -> PKey<Private> {
    let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
    PKey::from_ec_key(EcKey::generate(&group).unwrap()).unwrap()
}

fn name(cn: &str) -> X509Name {
    let mut b = X509NameBuilder::new().unwrap();
    b.append_entry_by_nid(Nid::COMMONNAME, cn).unwrap();
    b.build()
}

struct Sans {
    dns: &'static [&'static str],
    ip: &'static [&'static str],
}

fn cert(
    cn: &str,
    key: &PKey<Private>,
    issuer: Option<(&X509, &PKey<Private>)>,
    ca: bool,
    sans: Option<Sans>,
    client: bool,
) -> X509 {
    let mut b = X509::builder().unwrap();
    b.set_version(2).unwrap();
    let serial = BigNum::from_u32(rand_u32()).unwrap();
    b.set_serial_number(&serial.to_asn1_integer().unwrap()).unwrap();
    let subject = name(cn);
    b.set_subject_name(&subject).unwrap();
    match issuer {
        Some((ic, _)) => b.set_issuer_name(ic.subject_name()).unwrap(),
        None => b.set_issuer_name(&subject).unwrap(),
    }
    b.set_pubkey(key).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    b.set_not_before(&Asn1Time::from_unix(now - 3600).unwrap()).unwrap();
    b.set_not_after(&Asn1Time::days_from_now(2).unwrap()).unwrap();
    let bc = if ca { BasicConstraints::new().critical().ca().build().unwrap() } else { BasicConstraints::new().build().unwrap() };
    b.append_extension(bc).unwrap();
    if let Some(s) = sans {
        let mut san = SubjectAlternativeName::new();
        for d in s.dns {
            san.dns(d);
        }
        for i in s.ip {
            san.ip(i);
        }
        let ctx = b.x509v3_context(issuer.map(|(c, _)| &**c), None);
        let ext = san.build(&ctx).unwrap();
        b.append_extension(ext).unwrap();
    }
    if client {
        b.append_extension(ExtendedKeyUsage::new().client_auth().build().unwrap()).unwrap();
    }
    let signer = issuer.map_or(key, |(_, k)| k);
    b.sign(signer, MessageDigest::sha256()).unwrap();
    b.build()
}

fn rand_u32() -> u32 {
    let mut b = [0u8; 4];
    openssl::rand::rand_bytes(&mut b).unwrap();
    u32::from_le_bytes(b) & 0x7fff_ffff
}

fn write_mode(path: &Path, bytes: &[u8], mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, bytes).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

struct Certs {
    ca: PathBuf,
    wrong_ca: PathBuf,
    server_crt: PathBuf,
    server_key: PathBuf,
    client_crt: PathBuf,
    client_key: PathBuf,
    client_key_open: PathBuf,
}

fn gen_certs(dir: &Path) -> Certs {
    let ca_key = keypair();
    let ca = cert("pgclient-tls-test-ca", &ca_key, None, true, None, false);
    let wrong_key = keypair();
    let wrong = cert("pgclient-tls-wrong-ca", &wrong_key, None, true, None, false);
    let srv_key = keypair();
    let srv = cert(
        "localhost",
        &srv_key,
        Some((&ca, &ca_key)),
        false,
        Some(Sans { dns: &["localhost"], ip: &["127.0.0.1"] }),
        false,
    );
    let cli_key = keypair();
    let cli = cert("certuser", &cli_key, Some((&ca, &ca_key)), false, None, true);
    let c = Certs {
        ca: dir.join("ca.crt"),
        wrong_ca: dir.join("wrong-ca.crt"),
        server_crt: dir.join("server.crt"),
        server_key: dir.join("server.key"),
        client_crt: dir.join("client.crt"),
        client_key: dir.join("client.key"),
        client_key_open: dir.join("client-open.key"),
    };
    write_mode(&c.ca, &ca.to_pem().unwrap(), 0o644);
    write_mode(&c.wrong_ca, &wrong.to_pem().unwrap(), 0o644);
    write_mode(&c.server_crt, &srv.to_pem().unwrap(), 0o644);
    write_mode(&c.server_key, &srv_key.private_key_to_pem_pkcs8().unwrap(), 0o600);
    write_mode(&c.client_crt, &cli.to_pem().unwrap(), 0o644);
    write_mode(&c.client_key, &cli_key.private_key_to_pem_pkcs8().unwrap(), 0o600);
    write_mode(&c.client_key_open, &cli_key.private_key_to_pem_pkcs8().unwrap(), 0o644);
    c
}

// initdb and postgres refuse to run as root; a root test process (the CI
// pod) runs them as the `pg` user through gosu, which then owns the datadir,
// socket dir and server-side key. None = root without gosu/pg: not runnable.
fn server_user() -> Option<Option<u32>> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return Some(None);
    }
    let name = std::ffi::CString::new("pg").unwrap();
    // SAFETY: getpwnam returns a static struct or null; only pw_uid is read.
    let uid = unsafe {
        let pw = libc::getpwnam(name.as_ptr());
        if pw.is_null() {
            return None;
        }
        (*pw).pw_uid
    };
    let gosu = ["/usr/sbin/gosu", "/usr/local/bin/gosu", "/usr/bin/gosu"].iter().any(|g| Path::new(g).exists());
    gosu.then_some(Some(uid))
}

fn server_command(bin: &Path, prog: &str, uid: Option<u32>) -> Command {
    match uid {
        Some(_) => {
            let mut c = Command::new("gosu");
            c.arg("pg").arg(bin.join(prog));
            c
        }
        None => Command::new(bin.join(prog)),
    }
}

fn give_to(path: &Path, uid: Option<u32>) {
    if let Some(uid) = uid {
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: chown on a path this test created.
        assert_eq!(unsafe { libc::chown(c.as_ptr(), uid, u32::MAX) }, 0, "chown {}", path.display());
    }
}

struct Server {
    child: Child,
}

impl Drop for Server {
    fn drop(&mut self) {
        // SAFETY: SIGINT to a child this test spawned (fast shutdown).
        unsafe { libc::kill(self.child.id() as i32, libc::SIGINT) };
        for _ in 0..50 {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn start_server(bin: &Path, work: &Path, tag: &str, ssl: Option<&Certs>, uid: Option<u32>) -> (Server, u16) {
    let dd = work.join(format!("dd-{tag}"));
    let sock = work.join("sock");
    std::fs::create_dir_all(&sock).unwrap();
    std::fs::create_dir_all(&dd).unwrap();
    give_to(&sock, uid);
    give_to(&dd, uid);
    if let Some(c) = ssl {
        for f in [&c.ca, &c.server_crt, &c.server_key] {
            give_to(f, uid);
        }
    }
    let out = server_command(bin, "initdb", uid)
        .args(["-D"])
        .arg(&dd)
        .args(["--no-locale", "-E", "UTF8", "-U", "postgres", "-A", "trust", "--no-sync"])
        .output()
        .unwrap();
    assert!(out.status.success(), "initdb: {}", String::from_utf8_lossy(&out.stderr));
    let hba = if ssl.is_some() {
        "local all all trust\n\
         hostssl all certuser 127.0.0.1/32 cert\n\
         hostssl all scramuser 127.0.0.1/32 scram-sha-256\n\
         hostnossl all scramuser 127.0.0.1/32 scram-sha-256\n\
         hostnossl all certuser 127.0.0.1/32 reject\n\
         host all all 127.0.0.1/32 trust\n"
    } else {
        "local all all trust\nhost all all 127.0.0.1/32 trust\n"
    };
    std::fs::write(dd.join("pg_hba.conf"), hba).unwrap();
    let port = free_port();
    let mut cmd = server_command(bin, "postgres", uid);
    cmd.arg("-D")
        .arg(&dd)
        .arg("-k")
        .arg(&sock)
        .args(["-p", &port.to_string(), "-c", "listen_addresses=127.0.0.1", "-c", "fsync=off"])
        .args(["-c", "autovacuum=off", "-c", "log_min_messages=warning"]);
    match ssl {
        Some(c) => {
            cmd.args(["-c", "ssl=on"])
                .arg(format!("-cssl_cert_file={}", c.server_crt.display()))
                .arg(format!("-cssl_key_file={}", c.server_key.display()))
                .arg(format!("-cssl_ca_file={}", c.ca.display()));
        }
        None => {
            cmd.args(["-c", "ssl=off"]);
        }
    }
    let log = std::fs::File::create(work.join(format!("server-{tag}.log"))).unwrap();
    cmd.stdout(Stdio::null()).stderr(log);
    let child = cmd.spawn().unwrap();
    let mut server = Server { child };
    let mut up = false;
    for _ in 0..100 {
        if matches!(server.child.try_wait(), Ok(Some(_))) {
            break;
        }
        let st = Command::new(bin.join("psql"))
            .arg("-h")
            .arg(&sock)
            .args(["-p", &port.to_string(), "-U", "postgres", "-X", "-c", "select 1"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        if st.success() {
            up = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    assert!(
        up,
        "{tag} server did not come up:\n{}",
        std::fs::read_to_string(work.join(format!("server-{tag}.log"))).unwrap_or_default()
    );
    if ssl.is_some() {
        let out = Command::new(bin.join("psql"))
            .arg("-h")
            .arg(&sock)
            .args(["-p", &port.to_string(), "-U", "postgres", "-X", "-v", "ON_ERROR_STOP=1"])
            .args(["-c", "set password_encryption = 'scram-sha-256'"])
            .args(["-c", "create role scramuser login password 'secret'"])
            .args(["-c", "create role certuser login"])
            .output()
            .unwrap();
        assert!(out.status.success(), "setup: {}", String::from_utf8_lossy(&out.stderr));
    }
    (server, port)
}

// psql's answer for the same conninfo: Ok(first cell) or Err(libpq's
// connection error text, "psql: error: " stripped).
fn psql_cell(bin: &Path, home: &Path, conninfo: &str) -> Result<String, String> {
    let out = Command::new(bin.join("psql"))
        .arg(conninfo)
        .args(["-X", "-At", "-c", QUERY])
        .env_clear()
        .env("HOME", home)
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim_end().to_string();
        Err(err.strip_prefix("psql: error: ").unwrap_or(&err).to_string())
    }
}

// pgclient waits on the socket through the backend's latch/wait-event-set
// seams, so the test process becomes a minimal backend first.
fn setup_backend() {
    waitevent_seams::pgstat_report_wait_start::set(|_| {});
    waitevent_seams::pgstat_report_wait_end::set(|| {});
    postgres_seams::check_for_interrupts::set(|| Ok(()));
    init_small::init_seams();
    waiteventset::init_seams();
    latch::init_seams();
    init_small::globals::SetMyProcPid(std::process::id() as i32);
    fd::vfd::set_max_safe_fds_value(1000);
    waiteventset::InitializeWaitEventSupport().unwrap();
    let l = latch::allocate_local_latch();
    latch::InitLatch(l);
    init_small::globals::SetMyLatch(Some(l));
}

fn pgclient_cell(conninfo: &str) -> Result<String, String> {
    let we = pgclient::WaitEvents { connect: 0, receive: 0 };
    let mut conn = pgclient::connect_db(conninfo, None, we).unwrap()?;
    let r = conn.exec(QUERY).unwrap();
    assert_eq!(r.status, pgclient::ExecStatus::TuplesOk, "{conninfo}: {}", r.err);
    Ok(String::from_utf8(r.rows[0][0].clone().unwrap()).unwrap())
}

#[test]
fn tls_matrix_matches_libpq() {
    let Some(bin) = oracle_bin() else {
        if std::env::var_os("PGRUST_REQUIRE_ORACLE").is_some() {
            panic!("no C PostgreSQL 18 oracle found (set PGRUST_C_ORACLE)");
        }
        eprintln!("SKIP tls_matrix_matches_libpq: no C PostgreSQL 18 oracle (set PGRUST_C_ORACLE)");
        return;
    };
    let Some(uid) = server_user() else {
        if std::env::var_os("PGRUST_REQUIRE_ORACLE").is_some() {
            panic!("running as root without gosu and a `pg` user: the C oracle cannot start");
        }
        eprintln!("SKIP tls_matrix_matches_libpq: root without gosu and a `pg` user");
        return;
    };
    setup_backend();
    let work = std::env::temp_dir().join(format!("pgclient-tls-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(work.join("certs")).unwrap();
    let home = work.join("home");
    std::fs::create_dir_all(&home).unwrap();
    for k in [
        "PGSSLMODE", "PGSSLCERT", "PGSSLKEY", "PGSSLROOTCERT", "PGSSLCRL", "PGSSLCRLDIR", "PGSSLSNI",
        "PGSSLNEGOTIATION", "PGSSLCERTMODE", "PGCHANNELBINDING", "PGREQUIRESSL", "PGPASSFILE",
        "PGPASSWORD", "PGSERVICE", "PGSERVICEFILE", "PGHOST", "PGPORT", "PGUSER", "PGDATABASE",
        "PGSSLMINPROTOCOLVERSION", "PGSSLMAXPROTOCOLVERSION", "PGGSSENCMODE",
    ] {
        std::env::remove_var(k);
    }
    std::env::set_var("HOME", &home);

    let c = gen_certs(&work.join("certs"));
    let (ssl_server, s) = start_server(&bin, &work, "ssl", Some(&c), uid);
    let (plain_server, p) = start_server(&bin, &work, "plain", None, uid);

    let ca = c.ca.display();
    let cells: Vec<(&str, String)> = vec![
        ("disable", format!("host=127.0.0.1 port={s} user=postgres dbname=postgres sslmode=disable")),
        ("allow", format!("host=127.0.0.1 port={s} user=postgres dbname=postgres sslmode=allow")),
        ("prefer", format!("host=127.0.0.1 port={s} user=postgres dbname=postgres sslmode=prefer")),
        ("require", format!("host=127.0.0.1 port={s} user=postgres dbname=postgres sslmode=require")),
        ("verify-ca no root", format!("host=127.0.0.1 port={s} user=postgres dbname=postgres sslmode=verify-ca")),
        ("verify-ca", format!("host=127.0.0.1 port={s} user=postgres dbname=postgres sslmode=verify-ca sslrootcert={ca}")),
        ("verify-ca wrong CA", format!("host=127.0.0.1 port={s} user=postgres dbname=postgres sslmode=verify-ca sslrootcert={}", c.wrong_ca.display())),
        ("verify-full dns", format!("host=localhost hostaddr=127.0.0.1 port={s} user=postgres dbname=postgres sslmode=verify-full sslrootcert={ca}")),
        ("verify-full ip", format!("host=127.0.0.1 port={s} user=postgres dbname=postgres sslmode=verify-full sslrootcert={ca}")),
        ("verify-full mismatch", format!("host=wrong.example hostaddr=127.0.0.1 port={s} user=postgres dbname=postgres sslmode=verify-full sslrootcert={ca}")),
        ("verify-full no host", format!("hostaddr=127.0.0.1 port={s} user=postgres dbname=postgres sslmode=verify-full sslrootcert={ca}")),
        ("verify-full system roots", format!("host=localhost hostaddr=127.0.0.1 port={s} user=postgres dbname=postgres sslmode=verify-full sslrootcert=system")),
        ("cert auth", format!("host=127.0.0.1 port={s} user=certuser dbname=postgres sslmode=require sslcert={} sslkey={}", c.client_crt.display(), c.client_key.display())),
        ("cert auth verify-full", format!("host=localhost hostaddr=127.0.0.1 port={s} user=certuser dbname=postgres sslmode=verify-full sslrootcert={ca} sslcert={} sslkey={}", c.client_crt.display(), c.client_key.display())),
        ("cert auth no cert", format!("host=127.0.0.1 port={s} user=certuser dbname=postgres sslmode=require")),
        ("cert auth sslcertmode=disable", format!("host=127.0.0.1 port={s} user=certuser dbname=postgres sslmode=require sslcertmode=disable sslcert={} sslkey={}", c.client_crt.display(), c.client_key.display())),
        ("cert auth open key", format!("host=127.0.0.1 port={s} user=certuser dbname=postgres sslmode=require sslcert={} sslkey={}", c.client_crt.display(), c.client_key_open.display())),
        ("cert auth missing key", format!("host=127.0.0.1 port={s} user=certuser dbname=postgres sslmode=require sslcert={} sslkey={}/nope.key", c.client_crt.display(), work.display())),
        ("cert auth plaintext", format!("host=127.0.0.1 port={s} user=certuser dbname=postgres sslmode=disable")),
        ("sslcertmode=require without cert", format!("host=127.0.0.1 port={s} user=postgres dbname=postgres sslmode=require sslcertmode=require")),
        ("sslcertmode=require with cert", format!("host=127.0.0.1 port={s} user=certuser dbname=postgres sslmode=require sslcertmode=require sslcert={} sslkey={}", c.client_crt.display(), c.client_key.display())),
        ("scram plus", format!("host=127.0.0.1 port={s} user=scramuser password=secret dbname=postgres sslmode=require channel_binding=require")),
        ("scram plus prefer", format!("host=127.0.0.1 port={s} user=scramuser password=secret dbname=postgres sslmode=require")),
        ("scram no binding", format!("host=127.0.0.1 port={s} user=scramuser password=secret dbname=postgres sslmode=require channel_binding=disable")),
        ("scram binding required cleartext", format!("host=127.0.0.1 port={s} user=scramuser password=secret dbname=postgres sslmode=disable channel_binding=require")),
        ("scram cleartext", format!("host=127.0.0.1 port={s} user=scramuser password=secret dbname=postgres sslmode=disable")),
        ("scram wrong password", format!("host=127.0.0.1 port={s} user=scramuser password=wrong dbname=postgres sslmode=require")),
        ("direct", format!("host=127.0.0.1 port={s} user=postgres dbname=postgres sslmode=require sslnegotiation=direct")),
        ("direct verify-full", format!("host=localhost hostaddr=127.0.0.1 port={s} user=postgres dbname=postgres sslmode=verify-full sslnegotiation=direct sslrootcert={ca}")),
        ("min proto above server", format!("host=127.0.0.1 port={s} user=postgres dbname=postgres sslmode=require ssl_min_protocol_version=TLSv1.3 ssl_max_protocol_version=TLSv1.3")),
        ("plain server require", format!("host=127.0.0.1 port={p} user=postgres dbname=postgres sslmode=require")),
        ("plain server prefer", format!("host=127.0.0.1 port={p} user=postgres dbname=postgres sslmode=prefer")),
        ("plain server verify-full", format!("host=127.0.0.1 port={p} user=postgres dbname=postgres sslmode=verify-full sslrootcert={ca}")),
        ("plain server direct", format!("host=127.0.0.1 port={p} user=postgres dbname=postgres sslmode=require sslnegotiation=direct")),
    ];
    let mut failures = Vec::new();
    let mut transcript = String::new();
    for (name, conninfo) in &cells {
        let oracle = psql_cell(&bin, &home, conninfo);
        let ours = pgclient_cell(conninfo);
        let shown = |r: &Result<String, String>| match r {
            Ok(v) => format!("OK {v}"),
            Err(e) => format!("ERR {e}"),
        };
        transcript.push_str(&format!("[{name}] {}\n", shown(&oracle)));
        if ours != oracle {
            failures.push(format!("[{name}] {conninfo}\n  libpq:    {}\n  pgclient: {}", shown(&oracle), shown(&ours)));
        }
    }
    drop(ssl_server);
    drop(plain_server);
    eprintln!("{transcript}");
    assert!(failures.is_empty(), "pgclient diverges from libpq:\n{}", failures.join("\n"));
    // The oracle must have exercised both arms, or the comparison is vacuous.
    assert!(transcript.contains("[require] OK t"), "{transcript}");
    assert!(transcript.contains("[scram plus] OK t"), "{transcript}");
    assert!(transcript.contains("[verify-ca wrong CA] ERR"), "{transcript}");
    assert!(transcript.contains("[plain server require] ERR"), "{transcript}");
    let _ = std::fs::remove_dir_all(&work);
}
