use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use instaux_seams::{ExtValue, ExternalSample};
use openssl::asn1::Asn1Time;
use openssl::bn::BigNum;
use openssl::ec::{EcGroup, EcKey};
use openssl::hash::MessageDigest;
use openssl::nid::Nid;
use openssl::pkey::{PKey, Private};
use openssl::ssl::{SslAcceptor, SslConnector, SslFiletype, SslMethod, SslSession, SslSessionCacheMode, SslVersion};
use openssl::x509::extension::{BasicConstraints, SubjectAlternativeName};
use openssl::x509::{X509Name, X509};
use serde_json::Value;

use crate::config::{self, ConfigState, Effective};
use crate::http::{read_request, HttpError, Request};
use crate::liveness::LiveView;
use crate::probes::{self, Thresholds, View};
use crate::sample::{Identity, Recovery, Sample, Usage};
use crate::shared::{PM_HOT_STANDBY, PM_RUN, PM_STARTUP};
use crate::tls::{admin_allowed, build_acceptor, peer_identities, CertInfo, TlsFiles};

const GOLDEN: &str = include_str!("../tests/golden/status_v1.json");

const FROZEN_KEYS: &[&str] = &[
    "config", "config.applied_hash", "config.decreased_sensitive", "config.pending_restart", "config.restart_ready",
    "connections", "connections.current", "connections.max", "crash_restart", "disk", "disk.data", "disk.wal",
    "disk.wal_budget_bytes", "epoch", "ext", "fence_reason", "fenced", "identity", "identity.cluster",
    "identity.incarnation", "identity.instance", "identity.system_identifier", "identity.version", "lag", "lag.bytes",
    "lag.ms", "liveness", "liveness.commit_age_ms", "liveness.lsn", "liveness.seq", "liveness.source",
    "liveness.write_wedged", "lsn", "lsn.durable", "lsn.flush", "lsn.insert", "lsn.log_end", "lsn.receive",
    "lsn.replay", "probes", "probes.live", "probes.live.ok", "probes.live.reason", "probes.ready", "probes.ready.ok",
    "probes.ready.reason", "probes.startup", "probes.startup.ok", "probes.startup.reason", "role", "sample_age_ms",
    "schema", "server_state", "term", "timeline", "tls", "tls.aux", "tls.aux.not_after", "tls.aux.serial", "tls.server",
    "tls.server.not_after", "tls.server.serial", "disk.data.avail_bytes", "disk.data.total_bytes", "disk.wal.avail_bytes",
    "disk.wal.total_bytes",
];

const NULLABLE_OBJECTS: [&str; 5] = ["ext", "disk.data", "disk.wal", "tls.aux", "tls.server"];

fn limits() -> Thresholds {
    Thresholds { ready_commit_age_ms: 5_000, ready_lag_ms: 10_000, startup_stall_ms: 60_000, wedge_ms: 10_000 }
}

fn fixture() -> Sample {
    Sample {
        taken_ms: 10_000,
        identity: Identity {
            cluster: "t-123".into(),
            instance: "t-123-1".into(),
            incarnation: "3".into(),
            system_identifier: 7_412_345_678_901_234_567,
            version: "0.3 (PostgreSQL 18.6)".into(),
        },
        recovery: Recovery::Done,
        hot_standby: false,
        timeline: 2,
        insert_lsn: Some(0x3_0000_0060),
        flush_lsn: Some(0x3_0000_0060),
        receive_lsn: None,
        replay_lsn: None,
        progress_ms: 9_000,
        external: ExternalSample {
            term: Some(5),
            durable_lsn: Some(0x3_0000_0060),
            log_end_lsn: Some(0x3_0000_0060),
            ext: vec![("archive_lsn".into(), ExtValue::Text("3/0".into())), ("consumers".into(), ExtValue::Int(2))],
        },
        live: LiveView { source: "local", seq: 42, lsn: Some(0x3_0000_0060), acked_ms: Some(9_800), external_wedged: false },
        lag_bytes: None,
        lag_ms: None,
        config: ConfigState {
            applied_hash: Some("ab".repeat(32)),
            pending_restart: vec!["shared_buffers".into()],
            decreased: vec![("max_connections".into(), 50)],
            restart_ready: true,
        },
        data_disk: Some(Usage { total_bytes: 100 << 30, avail_bytes: 60 << 30 }),
        wal_disk: Some(Usage { total_bytes: 20 << 30, avail_bytes: 19 << 30 }),
        wal_budget_bytes: 1 << 30,
        connections: 3,
        max_connections: 100,
        tls_aux: Some(CertInfo { serial: "1A2B".into(), not_after: 1_800_000_000 }),
        tls_server: Some(CertInfo { serial: "3C4D".into(), not_after: 1_790_000_000 }),
    }
}

fn view(s: Option<&Sample>) -> View<'_> {
    View {
        now_ms: 10_100,
        pm_state: PM_RUN,
        fatal_error: false,
        pm_state_ms: 1_000,
        started_ms: 500,
        sample: s,
        epoch: 7,
        fenced: false,
        fence_reason: String::new(),
        write_wedged: false,
        limits: limits(),
    }
}

fn key_paths(v: &Value, prefix: &str, full: bool, out: &mut Vec<String>) {
    if let Value::Object(m) = v {
        for (k, child) in m {
            let p = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
            out.push(p.clone());
            if p != "ext" && (full || !NULLABLE_OBJECTS.contains(&p.as_str())) {
                key_paths(child, &p, full, out);
            }
        }
    }
}

fn keys_of(json: &str, full: bool) -> Vec<String> {
    let v: Value = serde_json::from_str(json).expect("status is JSON");
    let mut out = Vec::new();
    key_paths(&v, "", full, &mut out);
    out.sort();
    out
}

fn frozen(full: bool) -> Vec<String> {
    let mut f: Vec<String> = FROZEN_KEYS
        .iter()
        .filter(|k| full || !NULLABLE_OBJECTS.iter().any(|n| k.starts_with(&format!("{n}."))))
        .map(|k| k.to_string())
        .collect();
    f.sort();
    f
}

#[test]
fn status_v1_matches_golden() {
    let s = fixture();
    let rendered = crate::status::render(&view(Some(&s)));
    if std::env::var_os("INSTAUX_BLESS").is_some() {
        std::fs::write(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/status_v1.json"), format!("{rendered}\n")).unwrap();
        return;
    }
    assert_eq!(rendered, GOLDEN.trim_end(), "status schema v1 is frozen: bump the schema, never edit in place");
}

#[test]
fn status_v1_key_set_is_frozen_with_and_without_a_sample() {
    let s = fixture();
    assert_eq!(keys_of(&crate::status::render(&view(Some(&s))), true), frozen(true));
    assert_eq!(keys_of(&crate::status::render(&view(None)), false), frozen(false));
    assert_eq!(keys_of(GOLDEN, true), frozen(true));
    assert_eq!(crate::status::STATUS_SCHEMA, "pgrust.instance_aux.status/v1");
}

#[test]
fn ready_follows_acknowledged_liveness_commits() {
    let mut s = fixture();
    assert!(probes::ready(&view(Some(&s))).ok);
    s.live.acked_ms = Some(10_100 - 5_001);
    let r = probes::ready(&view(Some(&s)));
    assert!(!r.ok && r.reason.unwrap().contains("acknowledged liveness commit"));
    s.live.acked_ms = None;
    assert!(!probes::ready(&view(Some(&s))).ok);
    let s = fixture();
    let mut v = view(Some(&s));
    v.write_wedged = true;
    assert!(!probes::ready(&v).ok);
    v.write_wedged = false;
    v.fenced = true;
    assert!(!probes::ready(&v).ok);
}

#[test]
fn ready_and_role_during_recovery_and_on_standby() {
    let mut s = fixture();
    s.recovery = Recovery::Crash;
    let mut v = view(Some(&s));
    v.pm_state = PM_STARTUP;
    assert_eq!(probes::role(&v), "recovering");
    assert!(!probes::ready(&v).ok);
    assert!(probes::live(&v).ok);
    assert!(probes::startup(&v).ok, "recovery made progress within the stall window");
    v.now_ms = 9_000 + 60_001;
    assert!(!probes::startup(&v).ok, "no progress for longer than the stall window");
    assert!(probes::live(&v).ok, "liveness never depends on the server");
    v.fatal_error = true;
    assert_eq!(probes::role(&v), "recovering");
    let mut s = fixture();
    s.recovery = Recovery::Archive;
    s.hot_standby = true;
    s.lag_ms = Some(9_000);
    let mut v = view(Some(&s));
    v.pm_state = PM_HOT_STANDBY;
    assert_eq!(probes::role(&v), "standby");
    assert!(probes::ready(&v).ok);
    s.lag_ms = Some(10_001);
    let mut v = view(Some(&s));
    v.pm_state = PM_HOT_STANDBY;
    assert!(!probes::ready(&v).ok);
}

#[test]
fn ready_fails_on_a_stale_sample() {
    let s = fixture();
    let mut v = view(Some(&s));
    v.now_ms = s.taken_ms + probes::SAMPLE_STALE_MS + 1;
    let r = probes::ready(&v);
    assert!(!r.ok && r.reason.unwrap().contains("sample"));
}

#[test]
fn cnpg_7045_parameter_names_are_refused_by_shape() {
    for bad in ["log_diconnections=", "#\narchive_command", "1work_mem", "a..b", "", "work mem", "a.", ".a"] {
        assert!(!config::name_shape_ok(bad), "{bad:?}");
    }
    for good in ["work_mem", "log_disconnections", "pgrust.instance_aux", "auto_explain.log_min_duration", "a$b"] {
        assert!(config::name_shape_ok(good), "{good:?}");
    }
}

#[test]
fn rendered_values_escape_like_the_config_lexer() {
    let params = vec![
        ("a".to_string(), "it's".to_string()),
        ("b".to_string(), "c:\\x\n\t\u{8}\u{c}\r".to_string()),
    ];
    let text = config::render(&params);
    assert!(text.contains("a = 'it''s'\n"), "{text}");
    assert!(text.contains("b = 'c:\\\\x\\n\\t\\b\\f\\r'\n"), "{text}");
    let mut vars = Vec::new();
    guc_file::ParseConfigFp(text.as_bytes(), std::path::Path::new("managed.conf"), 0, types_error::LOG, &mut vars)
        .expect("the rendering parses");
    let got: Vec<(String, String)> =
        vars.iter().map(|v| (v.name.clone().unwrap_or_default(), v.value.clone().unwrap_or_default())).collect();
    assert_eq!(got, params);
}

#[test]
fn startup_cross_checks_refuse_what_kills_the_next_start() {
    let parsed = vec![
        ("superuser_reserved_connections".to_string(), 3),
        ("reserved_connections".to_string(), 0),
        ("max_connections".to_string(), 3),
        ("wal_level".to_string(), 0),
        ("archive_mode".to_string(), 1),
        ("max_wal_senders".to_string(), 10),
        ("summarize_wal".to_string(), 0),
        ("min_wal_size".to_string(), 16),
        ("max_wal_size".to_string(), 1024),
    ];
    let r = config::startup_checks(&Effective::for_tests(&parsed), 16);
    let msgs: Vec<&str> = r.iter().map(|r| r.message.as_str()).collect();
    assert_eq!(r.len(), 4, "{msgs:?}");
    assert!(msgs[0].contains("must be less than \"max_connections\""));
    assert!(msgs[1].contains("WAL archival cannot be enabled"));
    assert!(msgs[2].contains("WAL streaming"));
    assert!(msgs[3].contains("\"min_wal_size\" must be at least twice"));
}

#[test]
fn http_requests_parse_and_bound() {
    let raw = b"POST /v1/config/check?x=1 HTTP/1.1\r\nHost: a\r\nContent-Length: 5\r\n\r\nhello";
    let r = read_request(&mut &raw[..]).unwrap();
    assert_eq!(r, Request { method: "POST".into(), path: "/v1/config/check".into(), body: b"hello".to_vec() });
    let chunked = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n";
    assert_eq!(read_request(&mut &chunked[..]), Err(HttpError::Unsupported("transfer-encoding")));
    let big = format!("POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n", crate::http::MAX_BODY + 1);
    assert_eq!(read_request(&mut big.as_bytes()), Err(HttpError::TooLarge));
    assert_eq!(read_request(&mut &b"GARBAGE\r\n\r\n"[..]), Err(HttpError::Malformed("request line")));
    assert_eq!(read_request(&mut &b""[..]), Err(HttpError::Closed));
}

#[test]
fn admin_identities_match_exactly_or_by_prefix() {
    let ids = vec!["pfa@node-7".to_string(), "spiffe://pf/pfa".to_string()];
    assert!(admin_allowed(&ids, "pfd, pfa@*"));
    assert!(admin_allowed(&ids, "\"spiffe://pf/pfa\""));
    assert!(!admin_allowed(&ids, "pfd"));
    assert!(!admin_allowed(&ids, ""));
    assert!(!admin_allowed(&[], "*"));
}

#[test]
fn persisted_state_round_trips() {
    let p = crate::state::Persisted { epoch: 42, fenced: true, reason: "term superseded\nby 7".into() };
    let q = crate::state::parse(&crate::state::render(&p));
    assert_eq!((q.epoch, q.fenced, q.reason.as_str()), (42, true, "term supersededby 7"));
    assert_eq!(crate::state::parse(""), crate::state::Persisted::default());
}

#[test]
fn role_timestamps_convert_to_postgres_epoch() {
    assert_eq!(crate::roles::parse_rfc3339_utc("2000-01-01T00:00:00Z"), Some(0));
    let ts = crate::roles::parse_rfc3339_utc("2026-12-31T23:59:58.5Z").unwrap();
    assert_eq!(crate::roles::pg_timestamp_text(ts), "2026-12-31 23:59:58.500000+00");
    assert_eq!(crate::roles::parse_rfc3339_utc("2026-12-31T23:59:58"), None);
    assert_eq!(crate::roles::parse_rfc3339_utc("2026-13-01T00:00:00Z"), None);
}

#[test]
fn role_requests_refuse_plaintext_passwords() {
    let body = br#"{"roles":[{"name":"app","verifier":"hunter2"}]}"#;
    let err = crate::roles::parse_request(body).unwrap_err();
    assert_eq!(err.status, 422);
    assert!(err.body.contains("plaintext_password_refused"));
    let body = br#"{"roles":[{"name":"app","login":true,"in_roles":["b","a","a"],"valid_until":"2030-01-01T00:00:00Z"}]}"#;
    let d = crate::roles::parse_request(body).unwrap();
    assert_eq!(d[0].in_roles, vec!["a", "b"]);
    assert!(d[0].login && d[0].inherit && !d[0].superuser && d[0].connection_limit == -1);
    assert!(crate::roles::is_reserved("pg_monitor"));
}

#[test]
fn update_and_unknown_endpoints_are_refused() {
    let r = crate::api::handle(&Request { method: "POST".into(), path: "/update".into(), body: vec![] }, &[]);
    assert_eq!(r.status, 410);
    let r = crate::api::handle(&Request { method: "GET".into(), path: "/v1/nope".into(), body: vec![] }, &[]);
    assert_eq!(r.status, 404);
    let r = crate::api::handle(&Request { method: "GET".into(), path: "/v1/promote".into(), body: vec![] }, &[]);
    assert_eq!(r.status, 405);
}

fn tls_key() -> PKey<Private> {
    let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
    PKey::from_ec_key(EcKey::generate(&group).unwrap()).unwrap()
}

fn tls_cert(serial: u32, key: &PKey<Private>, san: Option<&str>, issuer: Option<(&X509, &PKey<Private>)>) -> X509 {
    let mut name = X509Name::builder().unwrap();
    name.append_entry_by_nid(Nid::COMMONNAME, san.unwrap_or("instaux test ca")).unwrap();
    let name = name.build();
    let mut b = X509::builder().unwrap();
    b.set_version(2).unwrap();
    b.set_serial_number(&BigNum::from_u32(serial).unwrap().to_asn1_integer().unwrap()).unwrap();
    b.set_subject_name(&name).unwrap();
    b.set_issuer_name(issuer.map_or(&*name, |(ca, _)| ca.subject_name())).unwrap();
    b.set_pubkey(key).unwrap();
    b.set_not_before(&Asn1Time::days_from_now(0).unwrap()).unwrap();
    b.set_not_after(&Asn1Time::days_from_now(1).unwrap()).unwrap();
    let ext = match san {
        None => BasicConstraints::new().critical().ca().build().unwrap(),
        Some(dns) => {
            SubjectAlternativeName::new().dns(dns).build(&b.x509v3_context(issuer.map(|(ca, _)| &**ca), None)).unwrap()
        }
    };
    b.append_extension(ext).unwrap();
    b.sign(issuer.map_or(key, |(_, k)| k), MessageDigest::sha256()).unwrap();
    b.build()
}

fn tls_exchange(
    acceptor: &SslAcceptor,
    connector: &SslConnector,
    ticket: Option<&SslSession>,
) -> Result<(bool, Vec<String>), String> {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::scope(|s| {
        let server = s.spawn(|| -> Result<(bool, Vec<String>), String> {
            let mut tls = acceptor.accept(listener.accept().unwrap().0).map_err(|e| e.to_string())?;
            let mut byte = [0u8];
            tls.read_exact(&mut byte).and_then(|_| tls.write_all(&byte)).map_err(|e| e.to_string())?;
            let _ = tls.shutdown();
            Ok((tls.ssl().session_reused(), peer_identities(tls.ssl())))
        });
        let mut ssl = connector.configure().unwrap().into_ssl("localhost").unwrap();
        if let Some(t) = ticket {
            // SAFETY: the session was issued to an earlier connection through this connector's context.
            unsafe { ssl.set_session(t) }.unwrap();
        }
        let client = ssl.connect(TcpStream::connect(addr).unwrap()).map_err(|e| e.to_string()).and_then(|mut tls| {
            let mut byte = [b'x'];
            tls.write_all(&byte).and_then(|_| tls.read_exact(&mut byte)).map_err(|e| e.to_string())?;
            // Without a close_notify, OpenSSL marks the client's session unresumable.
            let _ = tls.shutdown();
            Ok(())
        });
        match (client, server.join().unwrap()) {
            (Ok(()), server) => server,
            (Err(c), server) => Err(format!("client: {c}; server: {server:?}")),
        }
    })
}

#[test]
fn tls_clients_that_try_to_resume_get_a_full_handshake_with_their_identity() {
    let dir = std::env::temp_dir().join(format!("instaux-tls-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = |name: &str| dir.join(name).to_string_lossy().into_owned();
    let (ca_key, server_key, client_key) = (tls_key(), tls_key(), tls_key());
    let ca = tls_cert(1, &ca_key, None, None);
    let server = tls_cert(2, &server_key, Some("localhost"), Some((&ca, &ca_key)));
    let client = tls_cert(3, &client_key, Some("pfa"), Some((&ca, &ca_key)));
    std::fs::write(path("ca.pem"), ca.to_pem().unwrap()).unwrap();
    std::fs::write(path("server.pem"), server.to_pem().unwrap()).unwrap();
    std::fs::write(path("server.key"), server_key.private_key_to_pem_pkcs8().unwrap()).unwrap();
    let files = TlsFiles { cert: path("server.pem"), key: path("server.key"), ca: path("ca.pem") };
    let pfa = || Ok((false, vec!["pfa".to_string()]));
    for version in [SslVersion::TLS1_3, SslVersion::TLS1_2] {
        let issued = Arc::new(Mutex::new(None));
        let mut b = SslConnector::builder(SslMethod::tls_client()).unwrap();
        b.set_ca_file(path("ca.pem")).unwrap();
        b.set_certificate(&client).unwrap();
        b.set_private_key(&client_key).unwrap();
        b.set_max_proto_version(Some(version)).unwrap();
        b.set_session_cache_mode(SslSessionCacheMode::CLIENT);
        let slot = issued.clone();
        b.set_new_session_callback(move |_, s| *slot.lock().unwrap() = Some(s));
        let connector = b.build();

        let aux = build_acceptor(&files).unwrap();
        assert_eq!(tls_exchange(&aux, &connector, None), pfa(), "{version:?}");
        let ticket = issued.lock().unwrap().take();
        assert_eq!(tls_exchange(&aux, &connector, ticket.as_ref()), pfa(), "{version:?}");
        assert!(ticket.is_none(), "{version:?}: the aux issued a resumable session");

        let mut other = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls_server()).unwrap();
        other.set_certificate_chain_file(path("server.pem")).unwrap();
        other.set_private_key_file(path("server.key"), SslFiletype::PEM).unwrap();
        assert_eq!(tls_exchange(&other.build(), &connector, None), Ok((false, vec![])), "{version:?}");
        let stale = issued.lock().unwrap().take();
        assert!(stale.is_some(), "{version:?}");
        let reloaded = build_acceptor(&files).unwrap();
        assert_eq!(tls_exchange(&reloaded, &connector, stale.as_ref()), pfa(), "{version:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
