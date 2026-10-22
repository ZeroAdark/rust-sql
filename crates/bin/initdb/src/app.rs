//! initdb, ported from src/bin/initdb/initdb.c for pgrust: creates a
//! cluster by driving the pgrust `postgres` binary — `--check` for the
//! configuration probe, `--boot` over postgres.bki for template1's
//! catalogs, `--single` for the post-bootstrap SQL phases — with the share
//! inputs vendored beside this binary (share/, see PROVENANCE.md).
//!
//! Divergences from C (docs/design/native-initdb.md): UTF8 is the only
//! encoding (pgrust's UTF-8 carve), so a C/POSIX locale defaults to UTF8
//! instead of SQL_ASCII; the ICU provider behaves as
//! in a C build without ICU; the default timezone comes from
//! TZ / /etc/localtime instead of findtimezone.c's zone-database scan;
//! `--sync-method` accepts only fsync; `--pwprompt` reads /dev/tty.

mod conf;
mod fsutil;
mod pgrust_rows;
mod tsearch_langs;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use conf::{escape_quotes, escape_quotes_bki, readfile, replace_guc_value, replace_token, writefile};

const PG_MAJORVERSION: &str = "18";
const PG_VERSION: &str = "18.6";
const BLCKSZ: i32 = 8192;
const DEFAULT_MIN_WAL_SEGS: i32 = 5;
const DEFAULT_MAX_WAL_SEGS: i32 = 64;
const TEMPLATE0_DB_OID: u32 = 4;
const POSTGRES_DB_OID: u32 = 5;
const BOOTSTRAP_SUPERUSERID: u32 = 10;
const DEF_PGPORT_STR: &str = "5432";
const DEFAULT_PGSOCKET_DIR: &str = match option_env!("PGRUST_DEFAULT_PGSOCKET_DIR") {
    Some(d) => d,
    None => "/tmp",
};
const PG_UTF8: i32 = 6;
const COLLPROVIDER_BUILTIN: char = 'b';
const COLLPROVIDER_LIBC: char = 'c';
const COLLPROVIDER_ICU: char = 'i';
const BOOT_OPTIONS: &str = "-F -c log_checkpoints=false";
const BACKEND_OPTIONS: &[&str] = &["--single", "-F", "-O", "-j", "-c", "search_path=pg_catalog", "-c", "exit_on_error=true", "-c", "log_checkpoints=false"];
const AUTHTRUST_WARNING: &str = "# CAUTION: Configuring the system for local \"trust\" authentication\n\
# allows any local user to connect as any PostgreSQL user, including\n\
# the database superuser.  If you do not trust all your local users,\n\
# use another authentication method.\n";
const AUTH_METHODS_HOST: &[&str] = &["trust", "reject", "scram-sha-256", "md5", "password", "ident", "radius", "gss", "sspi", "pam", "ldap", "cert", "bsd"];
const AUTH_METHODS_LOCAL: &[&str] = &["trust", "reject", "scram-sha-256", "md5", "password", "peer", "radius", "pam", "ldap", "bsd"];
const SUBDIRS: &[&str] = &[
    "global", "pg_wal/archive_status", "pg_wal/summaries", "pg_commit_ts", "pg_dynshmem", "pg_notify",
    "pg_serial", "pg_snapshots", "pg_subtrans", "pg_twophase", "pg_multixact", "pg_multixact/members",
    "pg_multixact/offsets", "base", "pg_replslot", "pg_tblspc", "pg_stat", "pg_stat_tmp", "pg_xact",
    "pg_logical", "pg_logical/snapshots", "pg_logical/mappings",
];

struct Opts {
    pg_data: Option<String>,
    encoding: Option<String>,
    locale: Option<String>,
    lc: [Option<String>; 6], // collate, ctype, monetary, numeric, time, messages
    locale_provider: char,
    datlocale: Option<String>,
    builtin_locale_specified: bool,
    icu_locale_specified: bool,
    icu_rules: Option<String>,
    text_search_config: Option<String>,
    authmethodlocal: Option<String>,
    authmethodhost: Option<String>,
    pwprompt: bool,
    pwfile: Option<String>,
    username: Option<String>,
    debug: bool,
    show: bool,
    noclean: bool,
    do_sync: bool,
    sync_only: bool,
    noinstructions: bool,
    extra_gucs: Vec<(String, String)>,
    xlog_dir: Option<String>,
    wal_segsize_mb: i32,
    data_checksums: bool,
    group_access: bool,
    share_path: Option<String>,
    extra_options: Vec<String>,
}

struct Ctx {
    progname: String,
    pg_data: PathBuf,
    share: PathBuf,
    backend: PathBuf,
    username: String,
    superuser_password: Option<String>,
    encodingid: i32,
    lc_collate: String,
    lc_ctype: String,
    lc_monetary: String,
    lc_numeric: String,
    lc_time: String,
    lc_messages: String,
    locale_provider: char,
    datlocale: Option<String>,
    default_text_search_config: String,
    authmethodlocal: String,
    authmethodhost: String,
    n_connections: i32,
    n_buffers: i32,
    n_av_slots: i32,
    default_timezone: Option<String>,
    infoversion: String,
    dir_mode: u32,
    file_mode: u32,
    made_new_pgdata: bool,
    made_new_xlogdir: bool,
    found_existing_pgdata: bool,
    found_existing_xlogdir: bool,
    o: Opts,
}

fn fatal(progname: &str, msg: impl AsRef<str>) -> ! {
    eprintln!("{progname}: error: {}", msg.as_ref());
    std::process::exit(1)
}

fn usage(progname: &str) {
    println!("{progname} initializes a PostgreSQL database cluster (pgrust).\n");
    println!("Usage:\n  {progname} [OPTION]... [DATADIR]\n");
    println!("Options:");
    println!("  -A, --auth=METHOD         default authentication method for local connections");
    println!("      --auth-host=METHOD    default authentication method for local TCP/IP connections");
    println!("      --auth-local=METHOD   default authentication method for local-socket connections");
    println!(" [-D, --pgdata=]DATADIR     location for this database cluster");
    println!("  -E, --encoding=ENCODING   set default encoding for new databases (UTF8 only)");
    println!("  -g, --allow-group-access  allow group read/execute on data directory");
    println!("      --icu-locale=LOCALE   set ICU locale ID for new databases");
    println!("      --icu-rules=RULES     set additional ICU collation rules for new databases");
    println!("      --locale=LOCALE       set default locale for new databases");
    println!("      --lc-collate=, --lc-ctype=, --lc-messages=LOCALE");
    println!("      --lc-monetary=, --lc-numeric=, --lc-time=LOCALE");
    println!("                            set default locale in the respective category for new databases");
    println!("      --no-locale           equivalent to --locale=C");
    println!("      --locale-provider={{builtin|libc|icu}}");
    println!("      --builtin-locale=LOCALE   set builtin locale name for new databases");
    println!("      --pwfile=FILE         read password for the new superuser from file");
    println!("  -T, --text-search-config=CFG  default text search configuration");
    println!("  -U, --username=NAME       database superuser name");
    println!("  -W, --pwprompt            prompt for a password for the new superuser");
    println!("  -X, --waldir=WALDIR       location for the write-ahead log directory");
    println!("      --wal-segsize=SIZE    size of WAL segments, in megabytes");
    println!("\nLess commonly used options:");
    println!("  -c, --set NAME=VALUE      override default setting for server parameter");
    println!("  -d, --debug               generate lots of debugging output");
    println!("      --discard-caches      set debug_discard_caches=1");
    println!("  -k, --data-checksums      use data page checksums (default)");
    println!("      --no-data-checksums   do not use data page checksums");
    println!("  -L DIRECTORY              where to find the input files");
    println!("  -n, --no-clean            do not clean up after errors");
    println!("  -N, --no-sync             do not wait for changes to be written safely to disk");
    println!("      --no-instructions     do not print instructions for next steps");
    println!("  -s, --show                show internal settings, then exit");
    println!("  -S, --sync-only           only sync database files to disk, then exit");
    println!("\nOther options:");
    println!("  -V, --version             output version information, then exit");
    println!("  -?, --help                show this help, then exit");
    println!("\nIf the data directory is not specified, the environment variable PGDATA is used.");
}

fn parse_args(progname: &str, args: &[String]) -> Opts {
    let mut o = Opts {
        pg_data: None, encoding: None, locale: None, lc: Default::default(), locale_provider: COLLPROVIDER_LIBC,
        datlocale: None, builtin_locale_specified: false, icu_locale_specified: false, icu_rules: None, text_search_config: None, authmethodlocal: None,
        authmethodhost: None, pwprompt: false, pwfile: None, username: None, debug: false, show: false,
        noclean: false, do_sync: true, sync_only: false, noinstructions: false, extra_gucs: Vec::new(),
        xlog_dir: None, wal_segsize_mb: 16, data_checksums: true, group_access: false, share_path: None,
        extra_options: Vec::new(),
    };
    let mut positional: Vec<String> = Vec::new();
    let mut it = args.iter().skip(1).peekable();
    let bad = |m: String| -> ! {
        eprintln!("{progname}: error: {m}");
        eprintln!("Try \"{progname} --help\" for more information.");
        std::process::exit(1)
    };
    // The rest of a short-option bundle (`-kN` = `-k -N`), read next as its own word (getopt_long, initdb.c:3242).
    let mut bundle_rest: Option<String> = None;
    while let Some(a) = bundle_rest.take().or_else(|| it.next().cloned()) {
        if a == "--help" || a == "-?" { usage(progname); std::process::exit(0); }
        if a == "--version" || a == "-V" { println!("initdb (pgrust) {PG_VERSION}"); std::process::exit(0); }
        if a == "--" {
            positional.extend(it.by_ref().cloned());
            break;
        }
        let (name, inline): (String, Option<String>) = if let Some(l) = a.strip_prefix("--") {
            match l.split_once('=') { Some((n, v)) => (format!("--{n}"), Some(v.to_string())), None => (a.clone(), None) }
        } else if a.starts_with('-') && a.len() > 1 {
            let c = a[1..].chars().next().unwrap_or('-');
            let tail = &a[1 + c.len_utf8()..];
            // initdb.c's optstring "A:c:dD:E:gkL:nNsST:U:WX:": an option without ':' takes no argument, so the rest
            // of its word is more options.
            let takes_arg = matches!(c, 'A' | 'c' | 'D' | 'E' | 'L' | 'T' | 'U' | 'X');
            if !takes_arg && !tail.is_empty() {
                bundle_rest = Some(format!("-{tail}"));
                (format!("-{c}"), None)
            } else {
                (format!("-{c}"), if tail.is_empty() { None } else { Some(tail.to_string()) })
            }
        } else {
            positional.push(a);
            continue;
        };
        let mut val = |needs: bool| -> Option<String> {
            if !needs { return None; }
            match inline.clone().or_else(|| it.next().cloned()) {
                Some(v) => Some(v),
                None => bad(format!("option '{name}' requires an argument")),
            }
        };
        match name.as_str() {
            "-A" | "--auth" => { let v = val(true).unwrap(); o.authmethodlocal = Some(v.clone()); o.authmethodhost = Some(v); }
            "--auth-local" => o.authmethodlocal = val(true),
            "--auth-host" => o.authmethodhost = val(true),
            "-c" | "--set" => {
                let v = val(true).unwrap();
                let (n, vv) = v.split_once('=').unwrap_or_else(|| bad(format!("-c {v} requires a value")));
                o.extra_gucs.push((n.trim().to_string(), vv.to_string()));
            }
            "-D" | "--pgdata" => o.pg_data = val(true),
            "-E" | "--encoding" => o.encoding = val(true),
            "-W" | "--pwprompt" => o.pwprompt = true,
            "-U" | "--username" => o.username = val(true),
            "-d" | "--debug" => { o.debug = true; eprintln!("Running in debug mode."); }
            "-n" | "--noclean" | "--no-clean" => { o.noclean = true; eprintln!("Running in no-clean mode.  Mistakes will not be cleaned up."); }
            "-N" | "--nosync" | "--no-sync" => o.do_sync = false,
            "-S" | "--sync-only" => o.sync_only = true,
            "-k" | "--data-checksums" => o.data_checksums = true,
            "--no-data-checksums" => o.data_checksums = false,
            "-L" => o.share_path = val(true),
            "--locale" => o.locale = val(true),
            "--lc-collate" => o.lc[0] = val(true),
            "--lc-ctype" => o.lc[1] = val(true),
            "--lc-monetary" => o.lc[2] = val(true),
            "--lc-numeric" => o.lc[3] = val(true),
            "--lc-time" => o.lc[4] = val(true),
            "--lc-messages" => o.lc[5] = val(true),
            "--no-locale" => o.locale = Some("C".to_string()),
            "--pwfile" => o.pwfile = val(true),
            "-s" | "--show" => o.show = true,
            "-T" | "--text-search-config" => o.text_search_config = val(true),
            "-X" | "--waldir" => o.xlog_dir = val(true),
            "--wal-segsize" => {
                let v = val(true).unwrap();
                o.wal_segsize_mb = v.parse().ok().filter(|n| (1..=1024).contains(n))
                    .unwrap_or_else(|| bad(format!("invalid value \"{v}\" for option --wal-segsize")));
            }
            "--no-instructions" => o.noinstructions = true,
            "-g" | "--allow-group-access" => o.group_access = true,
            "--discard-caches" => { o.extra_options.push("-c".into()); o.extra_options.push("debug_discard_caches=1".into()); }
            "--locale-provider" => {
                let v = val(true).unwrap();
                o.locale_provider = match v.as_str() {
                    "builtin" => COLLPROVIDER_BUILTIN,
                    "libc" => COLLPROVIDER_LIBC,
                    "icu" => COLLPROVIDER_ICU,
                    _ => fatal(progname, format!("unrecognized locale provider: {v}")),
                };
            }
            "--builtin-locale" => { o.datlocale = val(true); o.builtin_locale_specified = true; }
            "--icu-locale" => { o.datlocale = val(true); o.icu_locale_specified = true; }
            "--icu-rules" => o.icu_rules = val(true),
            "--sync-method" => {
                let v = val(true).unwrap();
                if v != "fsync" { fatal(progname, format!("unrecognized sync method: {v} (pgrust initdb supports fsync)")); }
            }
            "--no-sync-data-files" => {}
            _ => bad(format!("unrecognized option '{a}'")),
        }
    }
    if o.pg_data.is_none() && !positional.is_empty() {
        o.pg_data = Some(positional.remove(0));
    }
    if let Some(extra) = positional.first() {
        bad(format!("too many command-line arguments (first is \"{extra}\")"));
    }
    if o.builtin_locale_specified && o.locale_provider != COLLPROVIDER_BUILTIN {
        fatal(progname, "--builtin-locale cannot be specified unless locale provider \"builtin\" is chosen");
    }
    if o.icu_locale_specified && o.locale_provider != COLLPROVIDER_ICU {
        fatal(progname, "--icu-locale cannot be specified unless locale provider \"icu\" is chosen");
    }
    if o.icu_rules.is_some() && o.locale_provider != COLLPROVIDER_ICU {
        fatal(progname, "--icu-rules cannot be specified unless locale provider \"icu\" is chosen");
    }
    o
}

// ---- locale / encoding -------------------------------------------------------

fn check_locale_name(progname: &str, category: libc::c_int, locale: Option<&str>) -> String {
    let loc = locale.unwrap_or("");
    if !loc.is_ascii() {
        fatal(progname, format!("locale name \"{loc}\" contains non-ASCII characters"));
    }
    let c = std::ffi::CString::new(loc).unwrap();
    // SAFETY: probing setlocale in a single-threaded program; the old value
    // is saved and restored.
    unsafe {
        let old = libc::setlocale(category, std::ptr::null());
        let saved = if old.is_null() { None } else { Some(std::ffi::CStr::from_ptr(old).to_owned()) };
        let res = libc::setlocale(category, c.as_ptr());
        let canon = if res.is_null() { None } else { Some(std::ffi::CStr::from_ptr(res).to_string_lossy().into_owned()) };
        if let Some(s) = saved {
            libc::setlocale(category, s.as_ptr());
        }
        match canon {
            Some(s) => s,
            None if !loc.is_empty() => {
                eprintln!("{progname}: error: invalid locale name \"{loc}\"");
                std::process::exit(1)
            }
            None => fatal(progname, "invalid locale settings; check LANG and LC_* environment variables"),
        }
    }
}

fn encoding_from_locale(lc_ctype: &str) -> i32 {
    if lc_ctype == "C" || lc_ctype == "POSIX" {
        return PG_UTF8;
    }
    let up = lc_ctype.to_ascii_uppercase();
    if up.ends_with("UTF-8") || up.ends_with("UTF8") || up.ends_with(".UTF-8@EURO") {
        return PG_UTF8;
    }
    let c = std::ffi::CString::new(lc_ctype).unwrap();
    // SAFETY: setlocale + nl_langinfo probe, restored afterwards.
    unsafe {
        let old = libc::setlocale(libc::LC_CTYPE, std::ptr::null());
        let saved = if old.is_null() { None } else { Some(std::ffi::CStr::from_ptr(old).to_owned()) };
        let mut enc = -1;
        if !libc::setlocale(libc::LC_CTYPE, c.as_ptr()).is_null() {
            let cs = libc::nl_langinfo(libc::CODESET);
            if !cs.is_null() {
                let cs = std::ffi::CStr::from_ptr(cs).to_string_lossy().to_ascii_uppercase();
                enc = match cs.as_str() {
                    "UTF-8" | "UTF8" => PG_UTF8,
                    _ => -1,
                };
            }
        }
        if let Some(s) = saved {
            libc::setlocale(libc::LC_CTYPE, s.as_ptr());
        }
        enc
    }
}

fn encoding_id(name: &str) -> Result<i32, String> {
    let key: String = name.chars().filter(char::is_ascii_alphanumeric).collect();
    match key.to_ascii_uppercase().as_str() {
        "UTF8" | "UNICODE" => Ok(PG_UTF8),
        _ => Err(format!("\"{name}\" is not a valid server encoding name for pgrust (only UTF8 is accepted)")),
    }
}

fn find_matching_ts_config(lc_ctype: &str) -> Option<&'static str> {
    // Language part: up to the first '_' or '.' or '@'.
    let lang: String = lc_ctype.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    tsearch_langs::TSEARCH_CONFIG_LANGUAGES
        .iter()
        .find(|(_, l)| l.eq_ignore_ascii_case(&lang))
        .map(|(cfg, _)| *cfg)
}

/// The default timezone name for postgresql.conf: TZ, then /etc/localtime's
/// target relative to a zoneinfo dir, else None (the server falls back to
/// GMT). A pgrust divergence from findtimezone.c's scan.
fn select_default_timezone() -> Option<String> {
    if let Ok(tz) = std::env::var("TZ") {
        let tz = tz.trim_start_matches(':').to_string();
        if !tz.is_empty() && !tz.starts_with('/') {
            return Some(tz);
        }
    }
    if let Ok(target) = std::fs::read_link("/etc/localtime") {
        let s = target.to_string_lossy().replace('\\', "/");
        if let Some(pos) = s.find("zoneinfo/") {
            return Some(s[pos + "zoneinfo/".len()..].to_string());
        }
    }
    None
}

fn effective_user() -> String {
    // SAFETY: getpwuid on the current uid; the returned record is copied.
    unsafe {
        let pw = libc::getpwuid(libc::geteuid());
        if !pw.is_null() && !(*pw).pw_name.is_null() {
            return std::ffi::CStr::from_ptr((*pw).pw_name).to_string_lossy().into_owned();
        }
    }
    std::env::var("USER").unwrap_or_else(|_| "postgres".to_string())
}

fn set_info_version() -> String {
    // "18.6" -> "18.06.0000" (+ trailing letters, if any).
    let v = PG_VERSION;
    let numeric_end = v.rfind(|c: char| c.is_ascii_digit()).map_or(v.len(), |p| p + 1);
    let letters = &v[numeric_end..];
    let mut parts = v[..numeric_end].split('.').map(|p| p.parse::<i64>().unwrap_or(0));
    let major = parts.next().unwrap_or(0);
    let minor = parts.next().unwrap_or(0);
    let micro = parts.next().unwrap_or(0);
    format!("{major:02}.{minor:02}.{micro:04}{letters}")
}

// ---- paths ------------------------------------------------------------------

fn find_backend(progname: &str) -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_else(|e| fatal(progname, format!("could not find own program executable: {e}")));
    let dir = exe.parent().unwrap_or(Path::new("."));
    let cand = dir.join("postgres");
    if !cand.is_file() {
        fatal(progname, format!(
            "program \"postgres\" is needed by {progname} but was not found in the same directory as \"{}\"",
            exe.display()
        ));
    }
    match Command::new(&cand).arg("--version").output() {
        Ok(out) => {
            let s = String::from_utf8_lossy(&out.stdout);
            if !s.contains("PostgreSQL") || !s.contains(PG_MAJORVERSION) {
                fatal(progname, format!(
                    "program \"postgres\" was found by \"{}\" but was not the same version as {progname} ({})",
                    exe.display(), s.trim()
                ));
            }
        }
        Err(e) => fatal(progname, format!("could not execute \"{}\": {e}", cand.display())),
    }
    cand
}

fn find_share(progname: &str, backend: &Path, override_: Option<&str>) -> PathBuf {
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Some(o) = override_ { cands.push(PathBuf::from(o)); }
    if let Some(dir) = backend.parent() {
        cands.push(dir.join("share"));
        cands.push(dir.join("../share/postgresql"));
    }
    if let Ok(s) = std::env::var("PGRUST_PGSHAREDIR") { cands.push(PathBuf::from(s)); }
    for c in &cands {
        if c.join("postgres.bki").is_file() {
            return c.clone();
        }
    }
    fatal(progname, format!(
        "input file \"postgres.bki\" does not exist (looked in {}); use -L to point at the share directory",
        cands.iter().map(|c| c.display().to_string()).collect::<Vec<_>>().join(", ")
    ))
}

// ---- backend invocations --------------------------------------------------------

impl Ctx {
    fn backend_cmd(&self) -> Command {
        let mut c = Command::new(&self.backend);
        c.env("PGDATA", &self.pg_data);
        c.env_remove("PGCLIENTENCODING");
        c.current_dir(&self.pg_data);
        c
    }

    /// `test_specific_config_settings`: exit 0 from `postgres --check`.
    fn check_settings(&self, conns: i32, av_slots: i32, bufs: i32) -> bool {
        let mut c = self.backend_cmd();
        c.arg("--check");
        c.args(BOOT_OPTIONS.split(' '));
        c.args(&self.o.extra_options);
        c.arg("-c").arg(format!("max_connections={conns}"));
        c.arg("-c").arg(format!("autovacuum_worker_slots={av_slots}"));
        c.arg("-c").arg(format!("shared_buffers={bufs}"));
        c.arg("-c").arg("dynamic_shared_memory_type=posix");
        for (n, v) in &self.o.extra_gucs {
            c.arg("-c").arg(format!("{n}={v}"));
        }
        c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        c.status().map(|s| s.success()).unwrap_or(false)
    }

    /// `test_config_settings`.
    fn test_config_settings(&mut self) {
        const TRIAL_CONNS: [i32; 5] = [100, 50, 40, 30, 20];
        const TRIAL_BUFS: [i32; 19] = [16384, 8192, 4096, 3584, 3072, 2560, 2048, 1536, 1000, 900, 800, 700, 600, 500, 400, 300, 200, 100, 50];
        print!("selecting dynamic shared memory implementation ... ");
        let _ = std::io::stdout().flush();
        println!("posix");
        print!("selecting default \"max_connections\" ... ");
        let _ = std::io::stdout().flush();
        let mut ok_buffers = 0;
        let mut i = 0;
        while i < TRIAL_CONNS.len() {
            let conns = TRIAL_CONNS[i];
            self.n_av_slots = conns / 6;
            let bufs = conns * 10;
            if self.check_settings(conns, self.n_av_slots, bufs) {
                ok_buffers = bufs;
                break;
            }
            i += 1;
        }
        if i >= TRIAL_CONNS.len() {
            i = TRIAL_CONNS.len() - 1;
        }
        self.n_connections = TRIAL_CONNS[i];
        println!("{}", self.n_connections);
        print!("selecting default \"shared_buffers\" ... ");
        let _ = std::io::stdout().flush();
        let mut bufs = 0;
        for &t in &TRIAL_BUFS {
            bufs = (t * 8192) / BLCKSZ;
            if bufs <= ok_buffers {
                bufs = ok_buffers;
                break;
            }
            if self.check_settings(self.n_connections, self.n_av_slots, bufs) {
                break;
            }
        }
        self.n_buffers = bufs;
        println!("{}", pretty_bufs(self.n_buffers));
        print!("selecting default time zone ... ");
        let _ = std::io::stdout().flush();
        self.default_timezone = select_default_timezone();
        println!("{}", self.default_timezone.as_deref().unwrap_or("GMT"));
    }

    fn share_file(&self, name: &str) -> String {
        self.share.join(name).to_string_lossy().into_owned()
    }

    /// `setup_config`.
    fn setup_config(&self) -> Result<(), String> {
        print!("creating configuration files ... ");
        let _ = std::io::stdout().flush();
        let mut lines = readfile(&self.share_file("postgresql.conf.sample"))?;
        lines = replace_guc_value(lines, "max_connections", &self.n_connections.to_string(), false);
        lines = replace_guc_value(lines, "autovacuum_worker_slots", &self.n_av_slots.to_string(), false);
        lines = replace_guc_value(lines, "shared_buffers", &pretty_bufs(self.n_buffers), false);
        lines = replace_guc_value(lines, "lc_messages", &self.lc_messages, false);
        lines = replace_guc_value(lines, "lc_monetary", &self.lc_monetary, false);
        lines = replace_guc_value(lines, "lc_numeric", &self.lc_numeric, false);
        lines = replace_guc_value(lines, "lc_time", &self.lc_time, false);
        lines = replace_guc_value(lines, "datestyle", "iso, mdy", false);
        lines = replace_guc_value(lines, "default_text_search_config", &format!("pg_catalog.{}", self.default_text_search_config), false);
        if let Some(tz) = &self.default_timezone {
            lines = replace_guc_value(lines, "timezone", tz, false);
            lines = replace_guc_value(lines, "log_timezone", tz, false);
        }
        lines = replace_guc_value(lines, "dynamic_shared_memory_type", "posix", false);
        lines = replace_guc_value(lines, "min_wal_size", &pretty_wal_size(DEFAULT_MIN_WAL_SEGS, self.o.wal_segsize_mb), false);
        lines = replace_guc_value(lines, "max_wal_size", &pretty_wal_size(DEFAULT_MAX_WAL_SEGS, self.o.wal_segsize_mb), false);
        lines = replace_guc_value(lines, "unix_socket_directories", DEFAULT_PGSOCKET_DIR, true);
        lines = replace_guc_value(lines, "port", DEF_PGPORT_STR, true);
        // DEFAULT_BACKEND_FLUSH_AFTER / BGWRITER / CHECKPOINT are 0 / 64 / 32 pages on Linux.
        if cfg!(target_os = "linux") {
            lines = replace_guc_value(lines, "bgwriter_flush_after", &format!("{}kB", 64 * (BLCKSZ / 1024)), true);
            lines = replace_guc_value(lines, "checkpoint_flush_after", &format!("{}kB", 32 * (BLCKSZ / 1024)), true);
        }
        if (self.authmethodlocal == "md5" && self.authmethodhost != "scram-sha-256")
            || (self.authmethodhost == "md5" && self.authmethodlocal != "scram-sha-256")
        {
            lines = replace_guc_value(lines, "password_encryption", "md5", false);
        }
        if self.o.group_access {
            lines = replace_guc_value(lines, "log_file_mode", "0640", false);
        }
        for (n, v) in &self.o.extra_gucs {
            lines = replace_guc_value(lines, n, v, false);
        }
        let path = self.pg_data.join("postgresql.conf");
        writefile(&path.to_string_lossy(), &lines)?;
        fsutil::chmod(&path, self.file_mode).map_err(|e| e.to_string())?;

        let auto = self.pg_data.join("postgresql.auto.conf");
        writefile(&auto.to_string_lossy(), &["# Do not edit this file manually!\n".to_string(), "# It will be overwritten by the ALTER SYSTEM command.\n".to_string()])?;
        fsutil::chmod(&auto, self.file_mode).map_err(|e| e.to_string())?;

        let mut hba = readfile(&self.share_file("pg_hba.conf.sample"))?;
        hba = replace_token(hba, "@remove-line-for-nolocal@", "");
        if !ipv6_available() {
            hba = replace_token(hba, "host    all             all             ::1", "#host    all             all             ::1");
            hba = replace_token(hba, "host    replication     all             ::1", "#host    replication     all             ::1");
        }
        hba = replace_token(hba, "@authmethodhost@", &self.authmethodhost);
        hba = replace_token(hba, "@authmethodlocal@", &self.authmethodlocal);
        let warn = if self.authmethodlocal == "trust" || self.authmethodhost == "trust" { AUTHTRUST_WARNING } else { "" };
        hba = replace_token(hba, "@authcomment@", warn);
        let hpath = self.pg_data.join("pg_hba.conf");
        writefile(&hpath.to_string_lossy(), &hba)?;
        fsutil::chmod(&hpath, self.file_mode).map_err(|e| e.to_string())?;

        let ident = readfile(&self.share_file("pg_ident.conf.sample"))?;
        let ipath = self.pg_data.join("pg_ident.conf");
        writefile(&ipath.to_string_lossy(), &ident)?;
        fsutil::chmod(&ipath, self.file_mode).map_err(|e| e.to_string())?;
        println!("ok");
        Ok(())
    }

    /// `bootstrap_template1`.
    fn bootstrap_template1(&self) -> Result<(), String> {
        print!("running bootstrap script ... ");
        let _ = std::io::stdout().flush();
        let bki_path = self.share_file("postgres.bki");
        let mut lines = readfile(&bki_path)?;
        let header = format!("# PostgreSQL {PG_MAJORVERSION}\n");
        if lines.first().map(|l| l.as_str()) != Some(header.as_str()) {
            return Err(format!("input file \"{bki_path}\" does not belong to PostgreSQL {PG_VERSION}\nSpecify the correct path using the option -L."));
        }
        for (tok, rep) in [
            ("NAMEDATALEN", "64".to_string()),
            ("SIZEOF_POINTER", (core::mem::size_of::<usize>()).to_string()),
            ("ALIGNOF_POINTER", if core::mem::size_of::<usize>() == 4 { "i" } else { "d" }.to_string()),
            ("FLOAT8PASSBYVAL", "true".to_string()),
            ("POSTGRES", escape_quotes_bki(&self.username)),
            ("ENCODING", self.encodingid.to_string()),
            ("LC_COLLATE", escape_quotes_bki(&self.lc_collate)),
            ("LC_CTYPE", escape_quotes_bki(&self.lc_ctype)),
            ("DATLOCALE", self.datlocale.as_deref().map(escape_quotes_bki).unwrap_or_else(|| "_null_".to_string())),
            ("ICU_RULES", "_null_".to_string()),
            ("LOCALE_PROVIDER", self.locale_provider.to_string()),
        ] {
            lines = replace_token(lines, tok, &rep);
        }
        let lines = pgrust_rows::splice(lines)?;
        let mut c = self.backend_cmd();
        c.arg("--boot");
        c.args(BOOT_OPTIONS.split(' '));
        c.args(&self.o.extra_options);
        c.arg("-X").arg((self.o.wal_segsize_mb as i64 * 1024 * 1024).to_string());
        if self.o.data_checksums {
            c.arg("-k");
        }
        if self.o.debug {
            c.arg("-d").arg("5");
        }
        self.run_backend(c, lines.concat().as_bytes(), true)?;
        println!("ok");
        Ok(())
    }

    fn run_backend(&self, mut c: Command, input: &[u8], inherit_stdout: bool) -> Result<(), String> {
        c.stdin(Stdio::piped());
        if !inherit_stdout && !self.o.debug {
            c.stdout(Stdio::null());
        }
        let mut child = c.spawn().map_err(|e| format!("could not execute command \"{}\": {e}", self.backend.display()))?;
        {
            let mut stdin = child.stdin.take().expect("piped stdin");
            if let Err(e) = stdin.write_all(input) {
                if e.kind() != std::io::ErrorKind::BrokenPipe {
                    return Err(format!("could not write to child process: {e}"));
                }
            }
        }
        let status = child.wait().map_err(|e| format!("child process failed: {e}"))?;
        if !status.success() {
            return Err(match status.code() {
                Some(code) => format!("child process exited with exit code {code}"),
                None => "child process was terminated by a signal".to_string(),
            });
        }
        Ok(())
    }

    /// The post-bootstrap phases over ONE `postgres --single template1`.
    fn post_bootstrap(&self) -> Result<(), String> {
        print!("performing post-bootstrap initialization ... ");
        let _ = std::io::stdout().flush();
        let mut s = String::new();
        let esc_user = escape_quotes(&self.username);
        // setup_auth
        s.push_str("REVOKE ALL ON pg_authid FROM public;\n\n");
        if let Some(pw) = &self.superuser_password {
            s.push_str(&format!("ALTER USER \"{esc_user}\" WITH PASSWORD E'{}';\n\n", escape_quotes(pw)));
        }
        s.push_str(&self.run_file_text("system_constraints.sql")?);
        s.push_str(&self.run_file_text("system_functions.sql")?);
        // setup_depend
        s.push_str("SELECT pg_stop_making_pinned_objects();\n\n");
        s.push_str(&self.run_file_text("system_views.sql")?);
        // setup_description
        s.push_str("WITH funcdescs AS ( SELECT p.oid as p_oid, o.oid as o_oid, oprname FROM pg_proc p JOIN pg_operator o ON oprcode = p.oid ) INSERT INTO pg_description   SELECT p_oid, 'pg_proc'::regclass, 0,     'implementation of ' || oprname || ' operator'   FROM funcdescs   WHERE NOT EXISTS (SELECT 1 FROM pg_description    WHERE objoid = p_oid AND classoid = 'pg_proc'::regclass)   AND NOT EXISTS (SELECT 1 FROM pg_description    WHERE objoid = o_oid AND classoid = 'pg_operator'::regclass         AND description LIKE 'deprecated%');\n\n");
        // setup_collation
        s.push_str("UPDATE pg_collation SET collversion = pg_collation_actual_version(oid) WHERE collname = 'unicode';\n\n");
        s.push_str("SELECT pg_import_system_collations('pg_catalog');\n\n");
        s.push_str(&self.run_file_text("snowball_create.sql")?);
        // setup_privileges
        s.push_str(&format!(
            "UPDATE pg_class   SET relacl = (SELECT array_agg(a.acl) FROM  (SELECT E'=r/\"{esc_user}\"' as acl   UNION SELECT unnest(pg_catalog.acldefault(    CASE WHEN relkind = 'S' THEN 's'          ELSE 'r' END::\"char\",{BOOTSTRAP_SUPERUSERID}::oid)) ) as a)   WHERE relkind IN ('r', 'v', 'm', 'S')  AND relacl IS NULL;\n\n"
        ));
        s.push_str("GRANT USAGE ON SCHEMA pg_catalog, public TO PUBLIC;\n\n");
        s.push_str("GRANT USAGE ON SCHEMA pg_pgrust TO PUBLIC;\n\n");
        s.push_str("REVOKE ALL ON pg_largeobject FROM PUBLIC;\n\n");
        for (cls, table, col, join) in [
            ("pg_class", "pg_class", "relacl", ""),
            ("pg_proc", "pg_proc", "proacl", ""),
            ("pg_type", "pg_type", "typacl", ""),
            ("pg_language", "pg_language", "lanacl", ""),
            ("pg_largeobject_metadata", "pg_largeobject_metadata", "lomacl", ""),
            ("pg_namespace", "pg_namespace", "nspacl", ""),
            ("pg_foreign_data_wrapper", "pg_foreign_data_wrapper", "fdwacl", ""),
            ("pg_foreign_server", "pg_foreign_server", "srvacl", ""),
        ] {
            let _ = join;
            if cls == "pg_class" {
                s.push_str("INSERT INTO pg_init_privs   (objoid, classoid, objsubid, initprivs, privtype)    SELECT        oid,        (SELECT oid FROM pg_class WHERE relname = 'pg_class'),        0,        relacl,        'i'    FROM        pg_class    WHERE        relacl IS NOT NULL        AND relkind IN ('r', 'v', 'm', 'S');\n\n");
                s.push_str("INSERT INTO pg_init_privs   (objoid, classoid, objsubid, initprivs, privtype)    SELECT        pg_class.oid,        (SELECT oid FROM pg_class WHERE relname = 'pg_class'),        pg_attribute.attnum,        pg_attribute.attacl,        'i'    FROM        pg_class        JOIN pg_attribute ON (pg_class.oid = pg_attribute.attrelid)    WHERE        pg_attribute.attacl IS NOT NULL        AND pg_class.relkind IN ('r', 'v', 'm', 'S');\n\n");
            } else {
                s.push_str(&format!("INSERT INTO pg_init_privs   (objoid, classoid, objsubid, initprivs, privtype)    SELECT        oid,        (SELECT oid FROM pg_class WHERE relname = '{cls}'),        0,        {col},        'i'    FROM        {table}    WHERE        {col} IS NOT NULL;\n\n"));
            }
        }
        // setup_schema
        s.push_str(&self.run_file_text("information_schema.sql")?);
        s.push_str(&format!("UPDATE information_schema.sql_implementation_info   SET character_value = '{}'   WHERE implementation_info_name = 'DBMS VERSION';\n\n", self.infoversion));
        s.push_str(&format!("COPY information_schema.sql_features   (feature_id, feature_name, sub_feature_id,   sub_feature_name, is_supported, comments)  FROM E'{}';\n\n", escape_quotes(&self.share_file("sql_features.txt"))));
        // load_plpgsql
        s.push_str("CREATE EXTENSION plpgsql;\n\n");
        // vacuum_db
        s.push_str("ANALYZE;\n\nVACUUM FREEZE;\n\n");
        // make_template0
        s.push_str(&format!("CREATE DATABASE template0 IS_TEMPLATE = true ALLOW_CONNECTIONS = false OID = {TEMPLATE0_DB_OID} STRATEGY = file_copy;\n\n"));
        s.push_str("UPDATE pg_database SET datcollversion = NULL WHERE datname = 'template0';\n\n");
        s.push_str("UPDATE pg_database SET datcollversion = pg_database_collation_actual_version(oid) WHERE datname = 'template1';\n\n");
        s.push_str("REVOKE CREATE,TEMPORARY ON DATABASE template1 FROM public;\n\n");
        s.push_str("REVOKE CREATE,TEMPORARY ON DATABASE template0 FROM public;\n\n");
        s.push_str("COMMENT ON DATABASE template0 IS 'unmodifiable empty database';\n\n");
        s.push_str("VACUUM pg_database;\n\n");
        // make_postgres
        s.push_str(&format!("CREATE DATABASE postgres OID = {POSTGRES_DB_OID} STRATEGY = file_copy;\n\n"));
        s.push_str("COMMENT ON DATABASE postgres IS 'default administrative connection database';\n\n");

        let mut c = self.backend_cmd();
        c.args(BACKEND_OPTIONS);
        c.args(&self.o.extra_options);
        c.arg("template1");
        self.run_backend(c, s.as_bytes(), false)?;
        println!("ok");
        Ok(())
    }

    fn run_file_text(&self, name: &str) -> Result<String, String> {
        let lines = readfile(&self.share_file(name))?;
        let mut s = lines.concat();
        s.push_str("\n\n");
        Ok(s)
    }

    fn write_version_file(&self, extra: Option<&str>) -> Result<(), String> {
        let path = match extra {
            Some(e) => self.pg_data.join(e).join("PG_VERSION"),
            None => self.pg_data.join("PG_VERSION"),
        };
        std::fs::write(&path, format!("{PG_MAJORVERSION}\n")).map_err(|e| format!("could not write file \"{}\": {e}", path.display()))?;
        fsutil::chmod(&path, self.file_mode).map_err(|e| e.to_string())
    }

    /// `initialize_data_directory`.
    fn initialize_data_directory(&mut self) -> Result<(), String> {
        self.create_data_directory()?;
        self.create_xlog_or_symlink()?;
        print!("creating subdirectories ... ");
        let _ = std::io::stdout().flush();
        for d in SUBDIRS {
            let p = self.pg_data.join(d);
            fsutil::mkdir(&p, self.dir_mode).map_err(|e| format!("could not create directory \"{}\": {e}", p.display()))?;
        }
        println!("ok");
        self.write_version_file(None)?;
        std::fs::write(self.pg_data.join("postgresql.conf"), "").map_err(|e| e.to_string())?;
        self.test_config_settings();
        self.setup_config()?;
        self.bootstrap_template1()?;
        self.write_version_file(Some("base/1"))?;
        self.post_bootstrap()
    }

    fn create_data_directory(&mut self) -> Result<(), String> {
        match fsutil::pg_check_dir(&self.pg_data) {
            0 => {
                print!("creating directory {} ... ", self.pg_data.display());
                let _ = std::io::stdout().flush();
                fsutil::mkdir_p(&self.pg_data, self.dir_mode).map_err(|e| format!("could not create directory \"{}\": {e}", self.pg_data.display()))?;
                println!("ok");
                self.made_new_pgdata = true;
            }
            1 => {
                print!("fixing permissions on existing directory {} ... ", self.pg_data.display());
                let _ = std::io::stdout().flush();
                fsutil::chmod(&self.pg_data, self.dir_mode).map_err(|e| format!("could not change permissions of directory \"{}\": {e}", self.pg_data.display()))?;
                println!("ok");
                self.found_existing_pgdata = true;
            }
            2 | 3 | 4 => {
                return Err(format!(
                    "directory \"{}\" exists but is not empty\nIf you want to create a new database system, either remove or empty the directory \"{}\" or run {} with an argument other than \"{}\".",
                    self.pg_data.display(), self.pg_data.display(), self.progname, self.pg_data.display()
                ));
            }
            _ => return Err(format!("could not access directory \"{}\"", self.pg_data.display())),
        }
        Ok(())
    }

    fn create_xlog_or_symlink(&mut self) -> Result<(), String> {
        let subdirloc = self.pg_data.join("pg_wal");
        if let Some(xd) = self.o.xlog_dir.clone() {
            let xdir = PathBuf::from(&xd);
            if !xdir.is_absolute() {
                return Err("WAL directory location must be an absolute path".to_string());
            }
            match fsutil::pg_check_dir(&xdir) {
                0 => {
                    print!("creating directory {} ... ", xdir.display());
                    let _ = std::io::stdout().flush();
                    fsutil::mkdir_p(&xdir, self.dir_mode).map_err(|e| format!("could not create directory \"{}\": {e}", xdir.display()))?;
                    println!("ok");
                    self.made_new_xlogdir = true;
                }
                1 => {
                    fsutil::chmod(&xdir, self.dir_mode).map_err(|e| e.to_string())?;
                    self.found_existing_xlogdir = true;
                }
                2 | 3 | 4 => return Err(format!("directory \"{}\" exists but is not empty", xdir.display())),
                _ => return Err(format!("could not access directory \"{}\"", xdir.display())),
            }
            std::os::unix::fs::symlink(&xdir, &subdirloc).map_err(|e| format!("could not create symbolic link \"{}\": {e}", subdirloc.display()))?;
        } else {
            fsutil::mkdir(&subdirloc, self.dir_mode).map_err(|e| format!("could not create directory \"{}\": {e}", subdirloc.display()))?;
        }
        Ok(())
    }

    /// `cleanup_directories_atexit`.
    fn cleanup_on_failure(&self) {
        if self.o.noclean {
            eprintln!("{}: data directory \"{}\" not removed at user's request", self.progname, self.pg_data.display());
            return;
        }
        if self.made_new_pgdata {
            eprintln!("{}: removing data directory \"{}\"", self.progname, self.pg_data.display());
            let _ = std::fs::remove_dir_all(&self.pg_data);
        } else if self.found_existing_pgdata {
            eprintln!("{}: removing contents of data directory \"{}\"", self.progname, self.pg_data.display());
            if let Ok(entries) = std::fs::read_dir(&self.pg_data) {
                for e in entries.flatten() {
                    let p = e.path();
                    let _ = if p.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
                }
            }
        }
        if let Some(xd) = &self.o.xlog_dir {
            if self.made_new_xlogdir {
                let _ = std::fs::remove_dir_all(xd);
            } else if self.found_existing_xlogdir {
                if let Ok(entries) = std::fs::read_dir(xd) {
                    for e in entries.flatten() {
                        let _ = std::fs::remove_dir_all(e.path()).or_else(|_| std::fs::remove_file(e.path()));
                    }
                }
            }
        }
    }
}

fn pretty_bufs(n_buffers: i32) -> String {
    if (n_buffers * (BLCKSZ / 1024)) % 1024 == 0 {
        format!("{}MB", (n_buffers * (BLCKSZ / 1024)) / 1024)
    } else {
        format!("{}kB", n_buffers * (BLCKSZ / 1024))
    }
}

fn pretty_wal_size(segment_count: i32, segsize_mb: i32) -> String {
    let mb = segment_count * segsize_mb;
    if mb % 1024 == 0 { format!("{}GB", mb / 1024) } else { format!("{mb}MB") }
}

fn ipv6_available() -> bool {
    // getaddrinfo("::1", AI_NUMERICHOST) as in initdb.c; a libc without IPv6
    // support fails here and the ::1 pg_hba lines get commented out.
    let node = std::ffi::CString::new("::1").unwrap();
    // SAFETY: standard getaddrinfo/freeaddrinfo pairing.
    unsafe {
        let mut hints: libc::addrinfo = std::mem::zeroed();
        hints.ai_flags = libc::AI_NUMERICHOST;
        hints.ai_family = libc::AF_UNSPEC;
        let mut res: *mut libc::addrinfo = std::ptr::null_mut();
        let rc = libc::getaddrinfo(node.as_ptr(), std::ptr::null(), &hints, &mut res);
        if rc == 0 && !res.is_null() {
            libc::freeaddrinfo(res);
        }
        rc == 0
    }
}

fn read_password(progname: &str, o: &Opts) -> Option<String> {
    if let Some(f) = &o.pwfile {
        let text = std::fs::read_to_string(f).unwrap_or_else(|e| fatal(progname, format!("could not open file \"{f}\" for reading: {e}")));
        let first = text.lines().next().unwrap_or("").trim_end_matches('\r').to_string();
        return Some(first);
    }
    if o.pwprompt {
        let p1 = prompt_password("Enter new superuser password: ");
        let p2 = prompt_password("Enter it again: ");
        if p1 != p2 {
            eprintln!("Passwords didn't match.");
            std::process::exit(1);
        }
        return Some(p1);
    }
    None
}

fn prompt_password(prompt: &str) -> String {
    use std::io::{BufRead, Read};
    let tty = std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty");
    let Ok(mut tty) = tty else {
        let mut s = String::new();
        std::io::stdin().lock().read_line(&mut s).ok();
        return s.trim_end_matches(['\n', '\r']).to_string();
    };
    let _ = tty.write_all(prompt.as_bytes());
    let _ = tty.flush();
    use std::os::fd::AsRawFd;
    let fd = tty.as_raw_fd();
    // SAFETY: termios echo toggle on the tty we opened; restored after.
    let mut term: libc::termios = unsafe { std::mem::zeroed() };
    let have = unsafe { libc::tcgetattr(fd, &mut term) } == 0;
    if have {
        let mut noecho = term;
        noecho.c_lflag &= !libc::ECHO;
        unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &noecho) };
    }
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while let Ok(1) = tty.read(&mut byte) {
        if byte[0] == b'\n' { break; }
        buf.push(byte[0]);
    }
    if have {
        unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &term) };
    }
    let _ = tty.write_all(b"\n");
    String::from_utf8_lossy(&buf).trim_end_matches('\r').to_string()
}

pub fn main() {
    let args: Vec<String> = std::env::args().collect();
    let progname = args.first().and_then(|a| a.rsplit('/').next()).unwrap_or("initdb").to_string();
    let o = parse_args(&progname, &args);

    if o.pwprompt && o.pwfile.is_some() {
        fatal(&progname, "password prompt and password file cannot be specified together");
    }
    let pg_data = o.pg_data.clone().or_else(|| std::env::var("PGDATA").ok().filter(|s| !s.is_empty())).unwrap_or_else(|| {
        eprintln!("{progname}: error: no data directory specified");
        eprintln!("You must identify the directory where the data for this database system\nwill reside.  Do this with either the invocation option -D or the\nenvironment variable PGDATA.");
        std::process::exit(1)
    });
    let pg_data = PathBuf::from(pg_data);
    let pg_data = if pg_data.is_absolute() { pg_data } else { std::env::current_dir().unwrap().join(pg_data) };

    if o.sync_only {
        if fsutil::pg_check_dir(&pg_data) <= 0 {
            fatal(&progname, format!("could not access directory \"{}\"", pg_data.display()));
        }
        print!("syncing data to disk ... ");
        let _ = std::io::stdout().flush();
        fsutil::sync_pgdata(&pg_data);
        println!("ok");
        return;
    }

    let mut authwarning = false;
    let mut resolve_auth = |m: &Option<String>| -> String {
        match m {
            Some(s) => s.clone(),
            None => { authwarning = true; "trust".to_string() }
        }
    };
    let authmethodlocal = resolve_auth(&o.authmethodlocal);
    let authmethodhost = resolve_auth(&o.authmethodhost);
    if !AUTH_METHODS_LOCAL.contains(&authmethodlocal.as_str()) {
        fatal(&progname, format!("invalid authentication method \"{authmethodlocal}\" for \"local\" connections"));
    }
    if !AUTH_METHODS_HOST.contains(&authmethodhost.as_str()) {
        fatal(&progname, format!("invalid authentication method \"{authmethodhost}\" for \"host\" connections"));
    }
    let needs_pw = |m: &str| matches!(m, "md5" | "password" | "scram-sha-256");
    if needs_pw(&authmethodlocal) && needs_pw(&authmethodhost) && !(o.pwprompt || o.pwfile.is_some()) {
        fatal(&progname, "must specify a password for the superuser to enable password authentication");
    }
    if !((o.wal_segsize_mb as u32).is_power_of_two() && (1..=1024).contains(&o.wal_segsize_mb)) {
        fatal(&progname, "argument of --wal-segsize must be a power of two between 1 and 1024");
    }
    // SAFETY: umask is process-wide; set once at startup as C does.
    let (dir_mode, file_mode) = if o.group_access { (0o750, 0o640) } else { (0o700, 0o600) };
    unsafe { libc::umask(if o.group_access { 0o027 } else { 0o077 }) };

    let backend = find_backend(&progname);
    let share = find_share(&progname, &backend, o.share_path.as_deref());
    let eff_user = effective_user();
    let username = o.username.clone().unwrap_or_else(|| eff_user.clone());
    if username.starts_with("pg_") {
        fatal(&progname, format!("superuser name \"{username}\" is disallowed; role names cannot begin with \"pg_\""));
    }
    println!("The files belonging to this database system will be owned by user \"{eff_user}\".\nThis user must also own the server process.\n");

    // setup_locale_encoding
    let pick = |i: usize| o.lc[i].clone().or_else(|| o.locale.clone());
    let lc_collate = check_locale_name(&progname, libc::LC_COLLATE, pick(0).as_deref());
    let lc_ctype = check_locale_name(&progname, libc::LC_CTYPE, pick(1).as_deref());
    let lc_monetary = check_locale_name(&progname, libc::LC_MONETARY, pick(2).as_deref());
    let lc_numeric = check_locale_name(&progname, libc::LC_NUMERIC, pick(3).as_deref());
    let lc_time = check_locale_name(&progname, libc::LC_TIME, pick(4).as_deref());
    let lc_messages = check_locale_name(&progname, libc::LC_MESSAGES, pick(5).as_deref());
    let mut datlocale = o.datlocale.clone();
    if o.locale_provider == COLLPROVIDER_BUILTIN {
        let d = datlocale.clone().or_else(|| o.locale.clone()).unwrap_or_else(|| fatal(&progname, "locale must be specified if provider is builtin"));
        datlocale = Some(match d.as_str() {
            "C" => "C".to_string(),
            "C.UTF-8" | "C.UTF8" => "C.UTF-8".to_string(),
            "PG_UNICODE_FAST" => "PG_UNICODE_FAST".to_string(),
            _ => fatal(&progname, format!("invalid locale name \"{d}\"")),
        });
    } else if o.locale_provider == COLLPROVIDER_ICU {
        if datlocale.is_none() && o.locale.is_none() {
            fatal(&progname, "locale must be specified if provider is icu");
        }
        fatal(&progname, "ICU is not supported in this build");
    }
    let same = [&lc_collate, &lc_time, &lc_numeric, &lc_monetary, &lc_messages].iter().all(|l| **l == lc_ctype)
        && (o.locale_provider != COLLPROVIDER_BUILTIN || datlocale.as_deref() == Some(lc_ctype.as_str()));
    if same {
        println!("The database cluster will be initialized with locale \"{lc_ctype}\".");
    } else {
        println!("The database cluster will be initialized with this locale configuration:");
        println!("  locale provider:   {}", if o.locale_provider == COLLPROVIDER_BUILTIN { "builtin" } else { "libc" });
        if o.locale_provider == COLLPROVIDER_BUILTIN {
            println!("  default collation: {}", datlocale.as_deref().unwrap_or(""));
        }
        println!("  LC_COLLATE:  {lc_collate}\n  LC_CTYPE:    {lc_ctype}\n  LC_MESSAGES: {lc_messages}\n  LC_MONETARY: {lc_monetary}\n  LC_NUMERIC:  {lc_numeric}\n  LC_TIME:     {lc_time}");
    }
    let encodingid = match &o.encoding {
        Some(e) => encoding_id(e).unwrap_or_else(|m| fatal(&progname, m)),
        None => {
            let enc = encoding_from_locale(&lc_ctype);
            if enc == -1 {
                eprintln!("{progname}: error: could not find suitable encoding for locale \"{lc_ctype}\"");
                eprintln!("Rerun {progname} with the -E option (UTF8).");
                std::process::exit(1);
            }
            enc
        }
    };
    println!("The default database encoding has accordingly been set to \"UTF8\".");
    let default_text_search_config = match &o.text_search_config {
        Some(c) => c.clone(),
        None => match find_matching_ts_config(&lc_ctype) {
            Some(c) => c.to_string(),
            None => {
                println!("{progname}: could not find suitable text search configuration for locale \"{lc_ctype}\"");
                "simple".to_string()
            }
        },
    };
    println!("The default text search configuration will be set to \"{default_text_search_config}\".\n");
    println!("{}", if o.data_checksums { "Data page checksums are enabled." } else { "Data page checksums are disabled." });

    let superuser_password = read_password(&progname, &o);
    println!();

    let mut ctx = Ctx {
        progname: progname.clone(), pg_data, share, backend, username, superuser_password, encodingid,
        lc_collate, lc_ctype, lc_monetary, lc_numeric, lc_time, lc_messages, locale_provider: o.locale_provider,
        datlocale, default_text_search_config, authmethodlocal, authmethodhost, n_connections: 0, n_buffers: 0,
        n_av_slots: 0, default_timezone: None, infoversion: set_info_version(), dir_mode, file_mode,
        made_new_pgdata: false, made_new_xlogdir: false, found_existing_pgdata: false, found_existing_xlogdir: false, o,
    };
    if ctx.o.show || ctx.o.debug {
        eprintln!("VERSION={PG_VERSION}\nPGDATA={}\nshare_path={}\nPGPATH={}\nPOSTGRES_SUPERUSERNAME={}\nPOSTGRES_BKI={}",
            ctx.pg_data.display(), ctx.share.display(), ctx.backend.parent().map(|p| p.display().to_string()).unwrap_or_default(),
            ctx.username, ctx.share_file("postgres.bki"));
        if ctx.o.show { return; }
    }
    for f in ["postgres.bki", "pg_hba.conf.sample", "pg_ident.conf.sample", "postgresql.conf.sample", "snowball_create.sql",
              "information_schema.sql", "sql_features.txt", "system_constraints.sql", "system_functions.sql", "system_views.sql"] {
        if !ctx.share.join(f).is_file() {
            fatal(&progname, format!("input file \"{}\" does not exist", ctx.share.join(f).display()));
        }
    }

    if let Err(e) = ctx.initialize_data_directory() {
        println!();
        eprintln!("{progname}: error: {e}");
        ctx.cleanup_on_failure();
        std::process::exit(1);
    }
    if ctx.o.do_sync {
        print!("syncing data to disk ... ");
        let _ = std::io::stdout().flush();
        fsutil::sync_pgdata(&ctx.pg_data);
        println!("ok");
    } else {
        println!("\nSync to disk skipped.\nThe data directory might become corrupt if the operating system crashes.");
    }
    if authwarning {
        println!();
        eprintln!("{progname}: warning: enabling \"trust\" authentication for local connections");
        eprintln!("You can change this by editing pg_hba.conf or using the option -A, or\n--auth-local and --auth-host, the next time you run initdb.");
    }
    if !ctx.o.noinstructions {
        println!("\nSuccess. You can now start the database server using:\n\n    {} -D {}\n",
            ctx.backend.display(), ctx.pg_data.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(v: &[&str]) -> Vec<String> {
        std::iter::once("initdb").chain(v.iter().copied()).map(str::to_string).collect()
    }

    #[test]
    fn bundled_short_flags_all_take_effect() {
        // D4955: `-kN` read N as -k's (ignored) argument and dropped --no-sync.
        let o = parse_args("initdb", &argv(&["-kN", "-D", "/tmp/d"]));
        assert!(o.data_checksums && !o.do_sync);
        let o = parse_args("initdb", &argv(&["-NkUbob", "/tmp/d"]));
        assert!(o.data_checksums && !o.do_sync);
        assert_eq!(o.username.as_deref(), Some("bob"));
        assert_eq!(o.pg_data.as_deref(), Some("/tmp/d"));
        let o = parse_args("initdb", &argv(&["-ND/tmp/e"]));
        assert!(!o.do_sync);
        assert_eq!(o.pg_data.as_deref(), Some("/tmp/e"));
    }

    #[test]
    fn double_dash_ends_options() {
        let o = parse_args("initdb", &argv(&["-N", "--", "-dir"]));
        assert!(!o.do_sync);
        assert_eq!(o.pg_data.as_deref(), Some("-dir"));
    }

    #[test]
    fn utf8_is_the_only_encoding() {
        for n in ["UTF8", "utf8", "UTF-8", "utf_8", "unicode"] {
            assert_eq!(encoding_id(n), Ok(PG_UTF8));
        }
        for n in ["SQL_ASCII", "sql_ascii", "LATIN1"] {
            assert_eq!(
                encoding_id(n),
                Err(format!("\"{n}\" is not a valid server encoding name for pgrust (only UTF8 is accepted)"))
            );
        }
        assert_eq!(encoding_from_locale("C"), PG_UTF8);
        assert_eq!(encoding_from_locale("POSIX"), PG_UTF8);
        assert_eq!(encoding_from_locale("en_US.UTF-8"), PG_UTF8);
    }
}
