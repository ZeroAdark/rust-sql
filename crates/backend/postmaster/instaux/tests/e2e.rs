use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use openssl::asn1::Asn1Time;
use openssl::bn::{BigNum, MsbOption};
use openssl::ec::{EcGroup, EcKey};
use openssl::hash::MessageDigest;
use openssl::nid::Nid;
use openssl::pkey::{PKey, Private};
use openssl::ssl::{SslConnector, SslFiletype, SslMethod};
use openssl::x509::extension::{BasicConstraints, ExtendedKeyUsage, KeyUsage, SubjectAlternativeName};
use openssl::x509::{X509Builder, X509NameBuilder, X509};
use serde_json::{json, Value};

static SEQ: AtomicU32 = AtomicU32::new(0);

struct Env {
    postgres: PathBuf,
    pgbin: PathBuf,
}

fn env() -> Option<Env> {
    let postgres = PathBuf::from(std::env::var_os("PGRUST_BIN")?);
    let pgbin = PathBuf::from(std::env::var_os("PGBIN")?);
    (postgres.exists() && pgbin.join("initdb").exists()).then_some(Env { postgres, pgbin })
}

macro_rules! need_env {
    () => {
        match env() {
            Some(e) => e,
            None => {
                eprintln!("skipped: set PGRUST_BIN (pgrust postgres) and PGBIN (C 18.6 bin dir)");
                return;
            }
        }
    };
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn new_key() -> PKey<Private> {
    PKey::from_ec_key(EcKey::generate(&EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap()).unwrap()).unwrap()
}

enum Usage {
    Ca,
    Server,
    Client,
}

fn make_cert(cn: &str, sans: &[&str], usage: Usage, issuer: Option<(&X509, &PKey<Private>)>, key: &PKey<Private>, expired: bool) -> X509 {
    let mut name = X509NameBuilder::new().unwrap();
    name.append_entry_by_text("CN", cn).unwrap();
    let name = name.build();
    let mut b = X509Builder::new().unwrap();
    b.set_version(2).unwrap();
    let mut serial = BigNum::new().unwrap();
    serial.rand(64, MsbOption::MAYBE_ZERO, false).unwrap();
    b.set_serial_number(&serial.to_asn1_integer().unwrap()).unwrap();
    b.set_subject_name(&name).unwrap();
    b.set_issuer_name(issuer.map_or(&*name, |(c, _)| c.subject_name())).unwrap();
    b.set_pubkey(key).unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let (from, to) = if expired { (now - 3 * 86_400, now - 86_400) } else { (now - 3600, now + 2 * 86_400) };
    b.set_not_before(&Asn1Time::from_unix(from).unwrap()).unwrap();
    b.set_not_after(&Asn1Time::from_unix(to).unwrap()).unwrap();
    match usage {
        Usage::Ca => {
            b.append_extension(BasicConstraints::new().critical().ca().build().unwrap()).unwrap();
            b.append_extension(KeyUsage::new().critical().key_cert_sign().crl_sign().build().unwrap()).unwrap();
        }
        Usage::Server => b.append_extension(ExtendedKeyUsage::new().server_auth().build().unwrap()).unwrap(),
        Usage::Client => b.append_extension(ExtendedKeyUsage::new().client_auth().build().unwrap()).unwrap(),
    }
    if !sans.is_empty() {
        let mut san = SubjectAlternativeName::new();
        for s in sans {
            if s.parse::<std::net::IpAddr>().is_ok() {
                san.ip(s);
            } else {
                san.dns(s);
            }
        }
        let ext = san.build(&b.x509v3_context(issuer.map(|(c, _)| &**c), None)).unwrap();
        b.append_extension(ext).unwrap();
    }
    b.sign(issuer.map_or(key, |(_, k)| k), MessageDigest::sha256()).unwrap();
    b.build()
}

struct Ident {
    cert: PathBuf,
    key: PathBuf,
}

struct Certs {
    ca: PathBuf,
    server_cert: PathBuf,
    server_key: PathBuf,
    admin: Ident,
    reader: Ident,
    expired: Ident,
    foreign: Ident,
}

fn write_pair(dir: &Path, name: &str, cert: &X509, key: &PKey<Private>) -> Ident {
    let c = dir.join(format!("{name}.crt"));
    let k = dir.join(format!("{name}.key"));
    std::fs::write(&c, cert.to_pem().unwrap()).unwrap();
    std::fs::write(&k, key.private_key_to_pem_pkcs8().unwrap()).unwrap();
    std::fs::set_permissions(&k, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    Ident { cert: c, key: k }
}

fn make_certs(dir: &Path) -> Certs {
    std::fs::create_dir_all(dir).unwrap();
    let ca_key = new_key();
    let ca = make_cert("c1-test-ca", &[], Usage::Ca, None, &ca_key, false);
    let issuer = Some((&ca, &ca_key));
    let sk = new_key();
    let server = write_pair(dir, "server", &make_cert("localhost", &["localhost", "127.0.0.1"], Usage::Server, issuer, &sk, false), &sk);
    let ak = new_key();
    let admin = write_pair(dir, "admin", &make_cert("pfd", &["pfd"], Usage::Client, issuer, &ak, false), &ak);
    let rk = new_key();
    let reader = write_pair(dir, "reader", &make_cert("pfgw", &["pfgw"], Usage::Client, issuer, &rk, false), &rk);
    let ek = new_key();
    let expired = write_pair(dir, "expired", &make_cert("pfd", &["pfd"], Usage::Client, issuer, &ek, true), &ek);
    let other_ca_key = new_key();
    let other_ca = make_cert("other-ca", &[], Usage::Ca, None, &other_ca_key, false);
    let fk = new_key();
    let foreign = write_pair(dir, "foreign", &make_cert("pfd", &["pfd"], Usage::Client, Some((&other_ca, &other_ca_key)), &fk, false), &fk);
    let ca_path = dir.join("ca.crt");
    std::fs::write(&ca_path, ca.to_pem().unwrap()).unwrap();
    Certs { ca: ca_path, server_cert: server.cert, server_key: server.key, admin, reader, expired, foreign }
}

#[derive(Clone)]
enum Target {
    Tcp(u16),
    Unix(PathBuf),
}

trait Stream: Read + Write + std::fmt::Debug {}
impl<T: Read + Write + std::fmt::Debug> Stream for T {}

fn call(ca: &Path, target: &Target, who: Option<&Ident>, method: &str, path: &str, body: Option<&Value>, timeout: Duration) -> Result<(u16, Value), String> {
    let mut b = SslConnector::builder(SslMethod::tls_client()).map_err(|e| e.to_string())?;
    b.set_ca_file(ca).map_err(|e| e.to_string())?;
    if let Some(id) = who {
        b.set_certificate_file(&id.cert, SslFiletype::PEM).map_err(|e| e.to_string())?;
        b.set_private_key_file(&id.key, SslFiletype::PEM).map_err(|e| e.to_string())?;
    }
    let conn = b.build();
    let stream: Box<dyn Stream> = match target {
        Target::Tcp(p) => {
            let s = TcpStream::connect_timeout(&format!("127.0.0.1:{p}").parse().unwrap(), timeout).map_err(|e| format!("connect: {e}"))?;
            s.set_read_timeout(Some(timeout)).unwrap();
            s.set_write_timeout(Some(timeout)).unwrap();
            Box::new(s)
        }
        Target::Unix(p) => {
            let s = UnixStream::connect(p).map_err(|e| format!("connect: {e}"))?;
            s.set_read_timeout(Some(timeout)).unwrap();
            s.set_write_timeout(Some(timeout)).unwrap();
            Box::new(s)
        }
    };
    let mut tls = conn.connect("localhost", stream).map_err(|e| format!("handshake: {e}"))?;
    let payload = body.map(|v| v.to_string()).unwrap_or_default();
    let req = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}", payload.len());
    tls.write_all(req.as_bytes()).map_err(|e| format!("write: {e}"))?;
    let mut out = Vec::new();
    tls.read_to_end(&mut out).map_err(|e| format!("read: {e}"))?;
    let text = String::from_utf8_lossy(&out).to_string();
    let (head, body) = text.split_once("\r\n\r\n").ok_or_else(|| format!("no response ({} bytes)", out.len()))?;
    let status = head.split(' ').nth(1).and_then(|s| s.parse().ok()).ok_or("bad status line")?;
    Ok((status, serde_json::from_str(body).unwrap_or(Value::Null)))
}

struct Server {
    env_postgres: PathBuf,
    pgbin: PathBuf,
    dir: PathBuf,
    data: PathBuf,
    port: u16,
    aux_port: u16,
    certs: Certs,
    child: Option<Child>,
    incarnation: String,
}

impl Server {
    fn init(e: &Env, name: &str) -> Server {
        let dir = PathBuf::from(format!("/tmp/c1e2e-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let data = dir.join("data");
        let out = Command::new(e.pgbin.join("initdb"))
            .args(["-D", data.to_str().unwrap(), "-U", "postgres", "-A", "trust", "--no-sync", "--no-instructions"])
            .output()
            .unwrap();
        assert!(out.status.success(), "initdb: {}", String::from_utf8_lossy(&out.stderr));
        let certs = make_certs(&dir.join("certs"));
        let s = Server {
            env_postgres: e.postgres.clone(),
            pgbin: e.pgbin.clone(),
            dir,
            data,
            port: free_port(),
            aux_port: free_port(),
            certs,
            child: None,
            incarnation: "1".into(),
        };
        s.write_aux_conf(name);
        s
    }

    fn write_aux_conf(&self, name: &str) {
        let c = &self.certs;
        let conf = format!(
            "\ninclude_if_exists = 'pgrust.managed.conf'\npgrust.instance_aux = on\npgrust.instance_aux_listen = '127.0.0.1:{}'\npgrust.instance_aux_socket = '{}'\npgrust.instance_aux_cert_file = '{}'\npgrust.instance_aux_key_file = '{}'\npgrust.instance_aux_ca_file = '{}'\npgrust.instance_aux_admin_identities = 'pfd, pfa@*'\npgrust.instance_aux_cluster = 'c1-e2e'\npgrust.instance_aux_instance = '{name}'\npgrust.instance_aux_incarnation = '{}'\ncheckpoint_timeout = '1h'\nmax_wal_size = '8GB'\nfsync = off\n",
            self.aux_port,
            self.dir.join("aux.sock").display(),
            c.server_cert.display(),
            c.server_key.display(),
            c.ca.display(),
            self.incarnation
        );
        let mut f = std::fs::OpenOptions::new().append(true).open(self.data.join("postgresql.conf")).unwrap();
        f.write_all(conf.as_bytes()).unwrap();
    }

    fn start(&mut self) {
        let tz = self.pgbin.join("../share/postgresql/timezone");
        let cmd = format!(
            "ulimit -s 65520 2>/dev/null; exec '{}' -D '{}' -k '{}' -p {} -c listen_addresses=127.0.0.1 -c max_stack_depth=60000 -c io_method=sync -c autovacuum=off",
            self.env_postgres.display(),
            self.data.display(),
            self.dir.display(),
            self.port
        );
        let log = std::fs::File::create(self.dir.join(format!("server-{}.log", SEQ.fetch_add(1, Ordering::Relaxed)))).unwrap();
        let child = Command::new("sh")
            .args(["-c", &cmd])
            .env("RUST_MIN_STACK", "67108864")
            .env("PGRUST_TZDIR", &tz)
            .env("PGRUST_PGSHAREDIR", tz.parent().unwrap())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        self.child = Some(child);
        let deadline = Instant::now() + Duration::from_secs(120);
        while Instant::now() < deadline {
            if self.status().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        panic!("server did not start; see {}", self.dir.display());
    }

    fn pid(&self) -> i32 {
        self.child.as_ref().unwrap().id() as i32
    }

    fn server_pid(&self) -> i32 {
        let out = Command::new("pgrep").args(["-P", &self.pid().to_string()]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).lines().next().and_then(|l| l.trim().parse().ok()).unwrap_or(0)
    }

    fn signal_server(&self, sig: i32) {
        let pid = self.server_pid();
        assert!(pid > 0, "no server process under the supervisor");
        // SAFETY: plain kill(2) on the server process this test started.
        unsafe { libc::kill(pid, sig) };
    }

    fn signal(&self, sig: i32) {
        // SAFETY: plain kill(2) on our own child.
        unsafe { libc::kill(self.pid(), sig) };
    }

    fn wait_exit(&mut self, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if let Some(c) = self.child.as_mut() {
                if c.try_wait().unwrap().is_some() {
                    self.child = None;
                    return true;
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    fn stop(&mut self) {
        if self.child.is_none() {
            return;
        }
        self.signal(libc::SIGCONT);
        self.signal(libc::SIGINT);
        if !self.wait_exit(60) {
            self.kill9();
        }
    }

    fn kill9(&mut self) {
        if self.child.is_some() {
            self.signal(libc::SIGKILL);
            self.wait_exit(30);
        }
    }

    fn tcp(&self) -> Target {
        Target::Tcp(self.aux_port)
    }

    fn unix(&self) -> Target {
        Target::Unix(self.dir.join("aux.sock"))
    }

    fn admin(&self, method: &str, path: &str, body: Value) -> (u16, Value) {
        call(&self.certs.ca, &self.tcp(), Some(&self.certs.admin), method, path, Some(&body), Duration::from_secs(150)).unwrap()
    }

    fn get(&self, path: &str) -> Result<(u16, Value), String> {
        call(&self.certs.ca, &self.unix(), Some(&self.certs.reader), "GET", path, None, Duration::from_secs(1))
    }

    fn status(&self) -> Option<Value> {
        self.get("/v1/status").ok().map(|(_, v)| v)
    }

    fn wait_for(&self, what: &str, secs: u64, f: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(secs);
        let mut last = Value::Null;
        while Instant::now() < deadline {
            if let Some(s) = self.status() {
                if f(&s) {
                    return s;
                }
                last = s;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("timed out waiting for {what}; last status {last}");
    }

    fn psql(&self, user: &str, host: &str, password: Option<&str>, sql: &str) -> Result<String, String> {
        let mut c = Command::new(self.pgbin.join("psql"));
        c.args(["-X", "-qAt", "-h", host, "-p", &self.port.to_string(), "-U", user, "-d", "postgres", "-c", sql]);
        if let Some(p) = password {
            c.env("PGPASSWORD", p);
        }
        let out = c.stdin(Stdio::null()).output().unwrap();
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).to_string())
        }
    }

    fn sql(&self, sql: &str) -> String {
        self.psql("postgres", self.dir.to_str().unwrap(), None, sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

fn ready(s: &Value) -> bool {
    s["probes"]["ready"]["ok"] == json!(true)
}

const ENDPOINTS: [(&str, &str); 12] = [
    ("GET", "/v1/status"),
    ("GET", "/v1/role"),
    ("GET", "/v1/probe/startup"),
    ("GET", "/v1/probe/ready"),
    ("GET", "/v1/probe/live"),
    ("POST", "/v1/config/check"),
    ("POST", "/v1/config/apply"),
    ("POST", "/v1/promote"),
    ("POST", "/v1/fence"),
    ("POST", "/v1/roles/apply"),
    ("POST", "/v1/tls/reload"),
    ("POST", "/update"),
];

#[test]
fn every_endpoint_requires_a_valid_client_certificate() {
    let e = need_env!();
    let mut s = Server::init(&e, "mtls-1");
    s.start();
    let c = &s.certs;
    let body = json!({"epoch": 1, "incarnation": "1", "parameters": {}});
    for target in [s.tcp(), s.unix()] {
        for (m, p) in ENDPOINTS {
            for (who, label) in [(None, "no certificate"), (Some(&c.foreign), "foreign CA"), (Some(&c.expired), "expired")] {
                let r = call(&c.ca, &target, who, m, p, Some(&body), Duration::from_secs(5));
                assert!(r.is_err(), "{m} {p} answered with {label}: {r:?}");
            }
            let (status, _) = call(&c.ca, &target, Some(&c.reader), m, p, Some(&body), Duration::from_secs(30)).unwrap();
            let admin_only = matches!(p, "/v1/config/apply" | "/v1/promote" | "/v1/fence" | "/v1/roles/apply" | "/v1/tls/reload");
            if admin_only {
                assert_eq!(status, 403, "{m} {p} must refuse a non-admin identity");
            } else if p == "/update" {
                assert_eq!(status, 410);
            } else {
                assert!(status == 200 || status == 503, "{m} {p}: {status}");
            }
        }
    }
    let (st, v) = s.admin("POST", "/v1/tls/reload", json!({"epoch": 1, "incarnation": "1"}));
    assert_eq!(st, 200, "{v}");
    assert!(v["aux"]["serial"].is_string());
}

#[test]
fn status_schema_on_a_live_primary() {
    let e = need_env!();
    let mut s = Server::init(&e, "status-1");
    s.start();
    let st = s.wait_for("ready primary", 60, ready);
    assert_eq!(st["schema"], "pgrust.instance_aux.status/v1");
    assert_eq!(st["role"], "primary");
    assert_eq!(st["identity"]["instance"], "status-1");
    assert_eq!(st["identity"]["incarnation"], "1");
    assert_eq!(st["liveness"]["source"], "local");
    assert!(st["liveness"]["seq"].as_u64().unwrap() >= 1);
    assert!(st["lsn"]["flush"].is_string() && st["lsn"]["replay"].is_null());
    let golden: Value = serde_json::from_str(include_str!("golden/status_v1.json")).unwrap();
    fn keys(v: &Value, p: &str, out: &mut Vec<String>) {
        if let Value::Object(m) = v {
            for (k, c) in m {
                let q = format!("{p}.{k}");
                out.push(q.clone());
                if ![".ext", ".disk.data", ".disk.wal", ".tls.aux", ".tls.server"].contains(&q.as_str()) {
                    keys(c, &q, out);
                }
            }
        }
    }
    let (mut a, mut b) = (Vec::new(), Vec::new());
    keys(&st, "", &mut a);
    keys(&golden, "", &mut b);
    a.sort();
    b.sort();
    assert_eq!(a, b, "live status keys must equal the frozen golden keys");
    let (code, role) = s.get("/v1/role").unwrap();
    assert_eq!((code, role["role"].as_str()), (200, Some("primary")));
    assert_eq!(s.get("/v1/probe/live").unwrap().0, 200);
}

fn check(s: &Server, params: Value) -> (u16, Value) {
    call(&s.certs.ca, &s.tcp(), Some(&s.certs.reader), "POST", "/v1/config/check", Some(&json!({"parameters": params})), Duration::from_secs(30)).unwrap()
}

fn first_code(v: &Value) -> String {
    v["refusals"][0]["code"].as_str().unwrap_or("").to_string()
}

#[test]
fn config_check_refuses_invalid_values_and_apply_orders_restarts() {
    let e = need_env!();
    let mut s = Server::init(&e, "config-1");
    s.start();
    let cases = [
        (json!({"log_diconnections=": "off"}), "invalid_name"),
        (json!({"#\narchive_command": "x"}), "invalid_name"),
        (json!({"log_diconnections": "off"}), "unrecognized_parameter"),
        (json!({"pgrust.no_such_setting": "1"}), "unrecognized_parameter"),
        (json!({"log_disconnections": "maybe"}), "invalid_value"),
        (json!({"shared_buffers": "1GiB"}), "invalid_value"),
        (json!({"max_connections": "0"}), "invalid_value"),
        (json!({"superuser_reserved_connections": "500"}), "startup_check"),
        (json!({"wal_level": "minimal"}), "startup_check"),
        (json!({"min_wal_size": "16MB"}), "startup_check"),
        (json!({"pgrust.instance_aux_listen": "0.0.0.0:1"}), "forbidden_parameter"),
        (json!({"data_directory": "/x"}), "forbidden_parameter"),
    ];
    for (params, code) in cases {
        let (st, v) = check(&s, params.clone());
        assert_eq!(st, 422, "{params} should be refused: {v}");
        assert_eq!(first_code(&v), code, "{params}: {v}");
    }
    let (st, v) = check(&s, json!({"work_mem": "64MB", "shared_buffers": "256MB", "custom.knob": "x"}));
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["requires_restart"], json!(["shared_buffers"]));
    let before = s.status().unwrap()["config"]["applied_hash"].clone();
    let (st, v) = s.admin("POST", "/v1/config/apply", json!({"epoch": 1, "incarnation": "1", "parameters": {"work_mem": "bogus"}}));
    assert_eq!(st, 422, "{v}");
    assert_eq!(s.status().unwrap()["config"]["applied_hash"], before, "a refused apply must leave the config untouched");
    let (st, v) = s.admin("POST", "/v1/config/apply", json!({"epoch": 1, "incarnation": "1", "hba": "host all all 127.0.0.1/32 bogusmethod\n"}));
    assert_eq!((st, first_code(&v).as_str()), (422, "hba_invalid"), "{v}");
    let (st, v) = s.admin("POST", "/v1/config/apply", json!({"epoch": 1, "incarnation": "1", "parameters": {"work_mem": "64MB"}}));
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["applied"], json!(true));
    assert_eq!(s.sql("SHOW work_mem"), "64MB");
    assert_eq!(v["applied_hash"], v["hash"]);
    let (st, v) = s.admin(
        "POST",
        "/v1/config/apply",
        json!({"epoch": 1, "incarnation": "1", "parameters": {"work_mem": "64MB", "max_connections": "50", "shared_buffers": "64MB"}}),
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["restart_order"][0]["order"], "primary_first", "{v}");
    let pending: Vec<&str> = v["pending_restart"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    assert!(pending.contains(&"max_connections") && pending.contains(&"shared_buffers"), "{v}");
    assert_eq!(v["decreased_sensitive"][0]["parameter"], "max_connections", "{v}");
    assert_eq!(v["restart_ready"], json!(true));
    let (st, v) = s.admin("POST", "/v1/config/apply", json!({"epoch": 1, "incarnation": "1", "parameters": {"work_mem": "1MB"}, "hash": "00"}));
    assert_eq!((st, v["error"].as_str()), (409, Some("hash_mismatch")));
}

#[test]
fn mutations_check_epoch_and_incarnation_across_restarts() {
    let e = need_env!();
    let mut s = Server::init(&e, "epoch-1");
    s.start();
    let apply = |s: &Server, epoch: Value, inc: &str| s.admin("POST", "/v1/config/apply", json!({"epoch": epoch, "incarnation": inc, "parameters": {"work_mem": "8MB"}}));
    assert_eq!(apply(&s, json!(5), "1").0, 200);
    assert_eq!(apply(&s, json!(4), "1").1["error"], "stale_epoch");
    assert_eq!(apply(&s, json!(6), "2").1["error"], "wrong_incarnation");
    assert_eq!(apply(&s, Value::Null, "1").0, 400);
    s.stop();
    s.start();
    assert_eq!(s.status().unwrap()["epoch"], json!(5));
    assert_eq!(apply(&s, json!(4), "1").1["error"], "stale_epoch");
    assert_eq!(apply(&s, json!(5), "1").0, 200);
}

#[test]
fn fence_terminates_sessions_refuses_connections_and_persists() {
    let e = need_env!();
    let mut s = Server::init(&e, "fence-1");
    s.start();
    s.wait_for("ready", 60, ready);
    let sleeper = Command::new(s.pgbin.join("psql"))
        .args(["-X", "-h", s.dir.to_str().unwrap(), "-p", &s.port.to_string(), "-U", "postgres", "-c", "SELECT pg_sleep(60)"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(2));
    let (st, v) = s.admin("POST", "/v1/fence", json!({"epoch": 1, "incarnation": "1", "fenced": true, "reason": "term superseded"}));
    assert_eq!(st, 200, "{v}");
    assert!(v["terminated"].as_i64().unwrap() >= 1, "{v}");
    let out = sleeper.wait_with_output().unwrap();
    assert!(!out.status.success(), "the running session must be terminated");
    let err = s.psql("postgres", s.dir.to_str().unwrap(), None, "SELECT 1").unwrap_err();
    assert!(err.contains("the database system is fenced"), "{err}");
    let st = s.status().unwrap();
    assert_eq!((st["fenced"].clone(), st["fence_reason"].clone()), (json!(true), json!("term superseded")));
    assert!(!ready(&st));
    s.stop();
    s.start();
    let st = s.wait_for("accepting while fenced", 60, |s| s["server_state"] == "run");
    assert_eq!(st["fenced"], json!(true));
    let err = s.psql("postgres", s.dir.to_str().unwrap(), None, "SELECT 1").unwrap_err();
    assert!(err.contains("fenced"), "the fence must survive a restart: {err}");
    let (st, _) = s.admin("POST", "/v1/fence", json!({"epoch": 1, "incarnation": "1", "fenced": false}));
    assert_eq!(st, 200);
    assert_eq!(s.sql("SELECT 1"), "1");
    s.wait_for("ready after unfence", 30, ready);
}

fn scram_verifier(password: &str, salt: &[u8]) -> String {
    use openssl::sign::Signer;
    let mut salted = [0u8; 32];
    openssl::pkcs5::pbkdf2_hmac(password.as_bytes(), salt, 4096, MessageDigest::sha256(), &mut salted).unwrap();
    let hmac = |key: &[u8], msg: &[u8]| {
        let k = PKey::hmac(key).unwrap();
        let mut s = Signer::new(MessageDigest::sha256(), &k).unwrap();
        s.update(msg).unwrap();
        s.sign_to_vec().unwrap()
    };
    let client_key = hmac(&salted, b"Client Key");
    let stored = openssl::sha::sha256(&client_key);
    let server = hmac(&salted, b"Server Key");
    let b64 = openssl::base64::encode_block;
    format!("SCRAM-SHA-256$4096:{}${}:{}", b64(salt), b64(&stored), b64(&server))
}

#[test]
fn roles_apply_in_engine_with_client_side_verifiers() {
    let e = need_env!();
    let mut s = Server::init(&e, "roles-1");
    s.start();
    s.wait_for("ready", 60, ready);
    let hba = "local all all trust\nhost all app 127.0.0.1/32 scram-sha-256\nhost all all 127.0.0.1/32 trust\n";
    let (st, v) = s.admin("POST", "/v1/config/apply", json!({"epoch": 1, "incarnation": "1", "hba": hba}));
    assert_eq!(st, 200, "{v}");
    let v1 = scram_verifier("first-secret", b"0123456789abcdef");
    let apply = |roles: Value| s.admin("POST", "/v1/roles/apply", json!({"epoch": 1, "incarnation": "1", "roles": roles}));
    let (st, v) = apply(json!([{"name": "app", "login": true, "verifier": v1, "in_roles": ["pg_read_all_data"]}]));
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["roles"][0]["action"], "created");
    assert_eq!(v["roles"][0]["verifier"], json!(v1), "the stored verifier is read back");
    assert_eq!(s.psql("app", "127.0.0.1", Some("first-secret"), "SELECT current_user").unwrap(), "app");
    assert!(s.psql("app", "127.0.0.1", Some("wrong"), "SELECT 1").is_err());
    assert_eq!(s.sql("SELECT count(*) FROM pg_auth_members m JOIN pg_roles r ON r.oid = m.roleid WHERE r.rolname = 'pg_read_all_data' AND m.member = 'app'::regrole"), "1");
    let (_, v) = apply(json!([{"name": "app", "login": true, "verifier": v1, "in_roles": ["pg_read_all_data"]}]));
    assert_eq!(v["roles"][0]["action"], "unchanged", "{v}");
    let v2 = scram_verifier("second-secret", b"fedcba9876543210");
    let (_, v) = apply(json!([{"name": "app", "login": true, "verifier": v2, "connection_limit": 5}]));
    assert_eq!(v["roles"][0]["action"], "updated", "{v}");
    assert!(s.psql("app", "127.0.0.1", Some("first-secret"), "SELECT 1").is_err());
    assert_eq!(s.psql("app", "127.0.0.1", Some("second-secret"), "SELECT 1").unwrap(), "1");
    assert_eq!(s.sql("SELECT rolconnlimit FROM pg_roles WHERE rolname = 'app'"), "5");
    assert_eq!(s.sql("SELECT count(*) FROM pg_auth_members WHERE member = 'app'::regrole"), "0", "memberships converge to the request");
    let (_, v) = apply(json!([{"name": "app", "login": true, "valid_until": "2001-01-01T00:00:00Z"}]));
    assert_eq!(v["roles"][0]["action"], "updated", "{v}");
    assert!(s.psql("app", "127.0.0.1", Some("second-secret"), "SELECT 1").is_err(), "an expired role cannot log in");
    let (st, v) = apply(json!([{"name": "app", "verifier": "plaintext"}]));
    assert_eq!((st, v["error"].as_str()), (422, Some("plaintext_password_refused")));
    let (_, v) = apply(json!([{"name": "app", "ensure": "absent"}, {"name": "pg_monitor", "ensure": "absent"}]));
    assert_eq!(v["roles"][0]["action"], "dropped", "{v}");
    assert_eq!(v["roles"][1]["action"], "reserved", "{v}");
    assert_eq!(s.sql("SELECT count(*) FROM pg_roles WHERE rolname = 'app'"), "0");
    let log = std::fs::read_dir(&s.dir).unwrap().flatten().filter(|f| f.file_name().to_string_lossy().starts_with("server-")).map(|f| std::fs::read_to_string(f.path()).unwrap_or_default()).collect::<String>();
    assert!(!log.contains(&v1) && !log.contains(&v2), "verifiers never reach the log");
}

#[test]
fn probes_under_sigstop_of_the_postmaster() {
    let e = need_env!();
    let mut s = Server::init(&e, "sigstop-1");
    s.start();
    s.wait_for("ready", 60, ready);
    s.signal(libc::SIGSTOP);
    std::thread::sleep(Duration::from_secs(2));
    let st = s.status().expect("the server answers while only its crash supervisor is stopped");
    assert!(ready(&st), "a stopped supervisor does not affect the server: {st}");
    s.signal(libc::SIGCONT);
    let pid = s.server_pid();
    s.signal_server(libc::SIGSTOP);
    std::thread::sleep(Duration::from_millis(500));
    for p in ["/v1/probe/live", "/v1/probe/ready", "/v1/probe/startup", "/v1/status"] {
        let r = s.get(p);
        assert!(r.is_err(), "{p} must not answer while the postmaster is stopped: {r:?}");
    }
    std::thread::sleep(Duration::from_secs(7));
    s.signal_server(libc::SIGCONT);
    let resumed = Instant::now();
    let deadline = resumed + Duration::from_secs(5);
    let mut live = None;
    while Instant::now() < deadline && live.is_none() {
        live = s.get("/v1/probe/live").ok().filter(|(c, _)| *c == 200);
    }
    assert!(live.is_some(), "liveness answers right after SIGCONT");
    let st = s.wait_for("ready after SIGCONT", 10, ready);
    assert_eq!(s.server_pid(), pid, "the postmaster was never restarted");
    assert_eq!((st["crash_restart"].clone(), st["role"].clone()), (json!(false), json!("primary")));
    eprintln!("ready again {} ms after SIGCONT", resumed.elapsed().as_millis());
}

#[test]
fn probes_during_crash_recovery() {
    let e = need_env!();
    let mut s = Server::init(&e, "crash-1");
    s.start();
    s.wait_for("ready", 60, ready);
    let rows = std::env::var("C1_CRASH_ROWS").unwrap_or_else(|_| "1500000".into());
    s.sql(&format!("CREATE TABLE t AS SELECT g, repeat('x', 200) AS pad FROM generate_series(1, {rows}) g"));
    let crashed = s.server_pid();
    s.signal_server(libc::SIGKILL);
    let mut answered = false;
    let mut saw_recovering = 0;
    let (mut ready_503, mut startup_200) = (0, 0);
    let mut live_failures = 0;
    let killed = Instant::now();
    let deadline = Instant::now() + Duration::from_secs(600);
    let final_status = loop {
        assert!(Instant::now() < deadline, "crash recovery did not finish");
        let pid = s.server_pid();
        if pid == crashed || pid == 0 {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        let live = s.get("/v1/probe/live").map(|r| r.0).unwrap_or(0);
        if live == 200 {
            answered = true;
        } else if answered {
            live_failures += 1;
        }
        if let Some(st) = s.status() {
            if st["role"] == "recovering" {
                assert_eq!(st["probes"]["startup"]["ok"], json!(true), "startup passes while recovery progresses: {st}");
                assert_eq!(st["probes"]["ready"]["ok"], json!(false), "not ready during crash recovery: {st}");
                saw_recovering += 1;
                if let Ok((503, v)) = s.get("/v1/probe/ready") {
                    let why = v["reason"].as_str().unwrap_or("");
                    if why.contains("recover") || why.contains("not accepting") {
                        ready_503 += 1;
                    }
                }
                if let Ok((200, _)) = s.get("/v1/probe/startup") {
                    startup_200 += 1;
                }
            }
            if ready(&st) {
                break st;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    eprintln!("ready {} ms after SIGKILL; {saw_recovering} recovering samples", killed.elapsed().as_millis());
    assert!(ready_503 > 0 && startup_200 > 0, "the probe endpoints answered 503/200 during recovery ({ready_503}, {startup_200})");
    assert!(saw_recovering > 0, "crash recovery was observed through the aux (raise C1_CRASH_ROWS if it finished too fast)");
    assert_eq!(live_failures, 0, "liveness never failed once the aux answered");
    assert_eq!(final_status["role"], "primary");
    assert!(s.child.as_mut().unwrap().try_wait().unwrap().is_none(), "the supervisor restarted the server itself");
    assert_eq!(s.sql("SELECT count(*) > 0 FROM t"), "t");
}

#[test]
fn promote_a_standby_through_the_aux() {
    let e = need_env!();
    let mut p = Server::init(&e, "promote-p");
    p.sql_ready_start();
    let mut sb = Server::init(&e, "promote-s");
    std::fs::remove_dir_all(&sb.data).unwrap();
    let out = Command::new(e.pgbin.join("pg_basebackup"))
        .args(["-D", sb.data.to_str().unwrap(), "-R", "-X", "stream", "-h", p.dir.to_str().unwrap(), "-p", &p.port.to_string(), "-U", "postgres"])
        .output()
        .unwrap();
    assert!(out.status.success(), "pg_basebackup: {}", String::from_utf8_lossy(&out.stderr));
    let _ = std::fs::remove_file(sb.data.join("pgrust_instance_aux.state"));
    sb.incarnation = "2".into();
    sb.write_aux_conf("promote-s");
    sb.start();
    let st = sb.wait_for("hot standby ready", 120, ready);
    assert_eq!(st["role"], "standby");
    assert!(st["lsn"]["replay"].is_string());
    let (code, v) = sb.admin("POST", "/v1/roles/apply", json!({"epoch": 1, "incarnation": "2", "roles": []}));
    assert_eq!((code, v["error"].as_str()), (409, Some("not_primary")));
    let (code, v) = sb.admin("POST", "/v1/promote", json!({"epoch": 1, "incarnation": "2", "expected_term": 3}));
    let refused = v["error"].as_str().unwrap_or("");
    assert!(code == 409 && ["no_term_source", "term_mismatch"].contains(&refused), "an unverifiable expected_term fails closed: {v}");
    let (code, v) = sb.admin("POST", "/v1/promote", json!({"epoch": 1, "incarnation": "1"}));
    assert_eq!((code, v["error"].as_str()), (409, Some("wrong_incarnation")));
    let (code, v) = sb.admin("POST", "/v1/promote", json!({"epoch": 1, "incarnation": "2"}));
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["promoted"], json!(true));
    let st = sb.wait_for("promoted and ready", 60, |s| s["role"] == "primary" && ready(s));
    assert!(st["timeline"].as_u64().unwrap() >= 2);
    let (code, v) = sb.admin("POST", "/v1/promote", json!({"epoch": 1, "incarnation": "2"}));
    assert_eq!((code, v["error"].as_str()), (409, Some("not_in_recovery")));
    p.stop();
}

impl Server {
    fn sql_ready_start(&mut self) {
        self.start();
        self.wait_for("ready", 60, ready);
    }
}
