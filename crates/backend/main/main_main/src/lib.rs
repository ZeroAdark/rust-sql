#![allow(non_snake_case)]

use mcx::Mcx;
use types_error::{ErrorLocation, PgResult, FATAL, LOG};

// wasm32: no LC_* names in the wasi libc crate; musl numbering (the
// pg_locale wasm arm's convention, matching the linked wasi-libc).
#[cfg(not(target_family = "wasm"))]
use libc::{LC_COLLATE, LC_CTYPE, LC_MESSAGES, LC_MONETARY, LC_NUMERIC, LC_TIME};
#[cfg(target_family = "wasm")]
mod wasm_lc {
    pub const LC_CTYPE: i32 = 0;
    pub const LC_NUMERIC: i32 = 1;
    pub const LC_TIME: i32 = 2;
    pub const LC_COLLATE: i32 = 3;
    pub const LC_MONETARY: i32 = 4;
    pub const LC_MESSAGES: i32 = 5;
}
#[cfg(target_family = "wasm")]
use wasm_lc::*;

// Byte-identical to C: C pg_ctl / initdb / pg_upgrade / pg_rewind refuse any other line.
pub const PG_BACKEND_VERSIONSTR: &str = "postgres (PostgreSQL) 18.6\n";
pub const PGRUST_BUILD_ID: &str = env!("PGRUST_BUILD_ID");

const SRC: &str = "src/backend/main/main.c";

fn loc(line: i32, func: &'static str) -> ErrorLocation {
    ErrorLocation::new(SRC, line, func)
}

pub use init_small::dispatch::{parse_dispatch_option, DispatchOption};

// get_progname (src/port/path.c): basename of argv[0]; the .exe strip is Windows-only.
pub fn get_progname(argv0: &str) -> &str {
    argv0.rsplit('/').next().unwrap_or(argv0)
}

// ---------------------------------------------------------------------------
// pgrust extension (additive, no C counterpart): `postgres --profile <name>`.
//
// CONTRACT: `--profile <name>` is macro-expansion into `-c` arguments at that
// argv position, performed here as a literal argv rewrite BEFORE the existing
// (ported-C) option parse ever sees the vector. Precedence therefore falls
// out of the stock PGC_S_ARGV rules by construction: later explicit -c flags
// override profile values (same source, later SetConfigOption call wins), and
// profile values override postgresql.conf exactly as -c does. Invocations
// without `--profile` return the borrowed argv untouched — the stock parse
// path is byte-identical.
//
// 'profile' here = config preset. Unrelated to build/perf profiles
// (SERVER_PROFILE / JANITOR_PROFILE / cargo --profile).
// ---------------------------------------------------------------------------

/// Profile names `--profile` accepts. Grow this table (and a matching
/// `profile_settings` arm) to add a profile.
pub const KNOWN_PROFILES: &[&str] = &["test"];

/// The `--profile test` expansion list — THE single place it is defined.
///
/// First section = conf/test.conf verbatim (same keys, same values, same
/// order, module GUC-file quoting); the `conf_sync` test parses that file
/// and fails on any drift — a divergence between file and flag would be a
/// silent doc lie. Second section = janitor arming (prewarm is already
/// default-on when armed).
///
/// The profile INCLUDES `pgrust.ephemeral_db_mint_roles = *` (user ruling
/// 2026-08-05: the profile declares a DISPOSABLE test server — mint gating
/// there is friction without protection). The GUC's own default (`''` =
/// minting off) is unchanged, so a janitor armed WITHOUT the profile — a
/// durable dev/preview box — keeps the fail-closed allowlist posture. A
/// later explicit `-c` still overrides the profile (same source, later
/// SetConfigOption wins; the argv tests pin this).
///
/// There is no default-template setting to include: the template is ALWAYS
/// in the database name (`tdb_<template>__<token>` is the only mint form;
/// the former `pgrust.ephemeral_db_default_template` GUC was deleted with
/// the bare form, ruling 2026-08-05).
pub const PROFILE_TEST_SETTINGS: &[(&str, &str)] = &[
    // conf/test.conf — non-durable + quiet-background + test-shaped defaults
    ("fsync", "off"),
    ("synchronous_commit", "off"),
    ("full_page_writes", "off"),
    ("wal_level", "minimal"),
    ("max_wal_senders", "0"),
    ("autovacuum", "off"),
    ("checkpoint_timeout", "1h"),
    ("max_wal_size", "8GB"),
    ("file_copy_method", "clone"),
    ("jit", "off"),
    ("shared_buffers", "128MB"),
    ("pgrust.ephemeral_db_wal_log_threshold", "50"),
    ("pgrust.ephemeral_db_mint_roles", "*"),
    // janitor arming (docs/design/test-views.md D1); tdb_ = "test db"
    // (ruling 2026-08-05, renamed from tv_)
    ("pgrust.ephemeral_db_prefix", "tdb_"),
    ("pgrust.ephemeral_db_grace", "15s"),
];

pub fn profile_settings(name: &str) -> Option<&'static [(&'static str, &'static str)]> {
    match name {
        "test" => Some(PROFILE_TEST_SETTINGS),
        _ => None,
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ProfileArgvError {
    /// `--profile` with no value (or an empty `--profile=`).
    MissingValue,
    /// `--profile <name>` where <name> is not in KNOWN_PROFILES.
    UnknownProfile(String),
}

/// Rewrite argv, replacing each `--profile <name>` / `--profile=<name>` with
/// the profile's `-c name=value` pairs at that position. Returns Ok(None)
/// when no `--profile` token is present (the stock path: caller keeps the
/// original slice, untouched).
pub fn expand_profile_argv(argv: &[String]) -> Result<Option<Vec<String>>, ProfileArgvError> {
    // main.c:163-176: a first-argument --help/--version answers before any
    // other argument is read.
    if let Some(arg1) = argv.get(1) {
        if matches!(arg1.as_str(), "--help" | "-?" | "--version" | "-V") {
            return Ok(None);
        }
    }
    if !argv
        .iter()
        .skip(1)
        .any(|a| a == "--profile" || a.starts_with("--profile="))
    {
        return Ok(None);
    }
    let mut out: Vec<String> = Vec::with_capacity(argv.len() + 2 * PROFILE_TEST_SETTINGS.len());
    let mut it = argv.iter();
    if let Some(argv0) = it.next() {
        out.push(argv0.clone()); // argv[0] never participates
    }
    while let Some(arg) = it.next() {
        let name: String = if arg == "--profile" {
            match it.next() {
                Some(v) if !v.is_empty() => v.clone(),
                _ => return Err(ProfileArgvError::MissingValue),
            }
        } else if let Some(v) = arg.strip_prefix("--profile=") {
            if v.is_empty() {
                return Err(ProfileArgvError::MissingValue);
            }
            v.to_string()
        } else {
            out.push(arg.clone());
            continue;
        };
        let Some(settings) = profile_settings(&name) else {
            return Err(ProfileArgvError::UnknownProfile(name));
        };
        for (k, v) in settings {
            out.push("-c".to_string());
            out.push(format!("{k}={v}"));
        }
    }
    Ok(Some(out))
}

fn init_locale(mcx: Mcx<'_>, categoryname: &str, category: i32, locale: &str) -> PgResult<()> {
    if pg_locale::pg_perm_setlocale(mcx, category, locale)?.is_some()
        || pg_locale::pg_perm_setlocale(mcx, category, "C")?.is_some()
    {
        return Ok(());
    }
    elog::ereport(FATAL)
        .errmsg(format!(
            "could not adopt \"{locale}\" locale nor C locale for {categoryname}"
        ))
        .finish(loc(407, "init_locale"))
}

// wasm32: WASI has no uids — root cannot exist and there is nothing to
// refuse (C's WIN32 arm skips the check the same way).
#[cfg(target_family = "wasm")]
fn check_root(_progname: &str) {}

#[cfg(not(target_family = "wasm"))]
fn check_root(progname: &str) {
    // SAFETY: geteuid/getuid have no preconditions and never fail.
    let (uid, euid) = unsafe { (libc::getuid(), libc::geteuid()) };
    if euid == 0 {
        elog::write_stderr(
            "\"root\" execution of the PostgreSQL server is not permitted.\n\
             The server must be started under an unprivileged user ID to prevent\n\
             possible system security compromise.  See the documentation for\n\
             more information on how to properly start the server.\n",
        );
        std::process::exit(1);
    }
    if uid != euid {
        elog::write_stderr(&format!("{progname}: real and effective user IDs must match\n"));
        std::process::exit(1);
    }
}

/// main.c:165-176 ignores printf/fputs failures on --help/--version output
/// and still exits 0.
fn write_stdout_ignoring_errors(w: &mut dyn std::io::Write, s: &str) {
    let _ = w.write_all(s.as_bytes());
    let _ = w.flush();
}

/// C's argv is raw bytes (main.c:163); a non-UTF-8 argument must not panic.
pub fn argv_from_os<I: IntoIterator<Item = std::ffi::OsString>>(args: I) -> Vec<String> {
    args.into_iter().map(|a| a.to_string_lossy().into_owned()).collect()
}

pub fn pg_main(argv: &[String]) -> PgResult<()> {
    guc_tables::consts::set_pgrust_build_id(PGRUST_BUILD_ID);
    let mut do_check_root = true;
    let mut dispatch_option = DispatchOption::Postmaster;

    let progname = get_progname(argv.first().map(|s| s.as_str()).unwrap_or("postgres")).to_string();

    // pgrust extension: `--profile <name>` macro-expansion into -c arguments
    // at that argv position (see expand_profile_argv). Stock invocations
    // (no --profile token) keep the original argv slice untouched. Note the
    // expanded vector is what downstream sees everywhere — including
    // postmaster.opts, which therefore records the equivalent -c list.
    let expanded_argv;
    let argv: &[String] = match expand_profile_argv(argv) {
        Ok(None) => argv,
        Ok(Some(v)) => {
            expanded_argv = v;
            &expanded_argv
        }
        Err(ProfileArgvError::MissingValue) => {
            elog::write_stderr(&format!(
                "{progname}: option requires an argument -- profile (known profiles: {})\n",
                KNOWN_PROFILES.join(", ")
            ));
            std::process::exit(1);
        }
        Err(ProfileArgvError::UnknownProfile(name)) => {
            elog::write_stderr(&format!(
                "{progname}: unknown profile \"{name}\" (known profiles: {})\n",
                KNOWN_PROFILES.join(", ")
            ));
            std::process::exit(1);
        }
    };

    startup_hacks(&progname);

    ps_status::save_ps_display_args();

    init_small::globals::SetMyProcPid(init_small::globals::process_id() as i32);
    // MemoryContextInit: top-level contexts are owner-created here; ErrorContext is PgResult.

    stack_depth::set_stack_base();
    // Backend threads get scaled-provisioned stacks (launch_backend); the
    // main thread runs on the process stack (RLIMIT_STACK), so clamp the
    // guard's budget to what actually exists — the guard must raise 54001
    // before the real stack ends.
    {
        let rlim = stack_depth::get_stack_depth_rlimit();
        if rlim > 0 && rlim < isize::MAX {
            stack_depth::set_thread_stack_ceiling(rlim as usize);
        }
        #[cfg(target_family = "wasm")]
        {
            let engine = std::env::var("PGRUST_WASM_ENGINE_STACK")
                .ok()
                .and_then(|v| v.trim().parse::<usize>().ok())
                .filter(|&b| b > 0)
                .unwrap_or(stack_depth::DEFAULT_WASM_ENGINE_STACK);
            stack_depth::set_wasm_stack_ceiling(engine);
        }
    }

    let main_context = mcx::MemoryContext::new("Main");
    let mcx = main_context.mcx();

    // main.c:125. Its find_my_exec reports failures at LOG level (exec.c:65
    // log_error) — the first of the two LOG lines C prints on a bad argv[0];
    // NLS is not built, so only the PGSYSCONFDIR half remains after that.
    pg_path::set_pglocale_pgservice(
        argv.first().map(|s| s.as_str()).unwrap_or("postgres"),
        pg_path::PG_TEXTDOMAIN_POSTGRES,
        |code, m| {
            let b = elog::ereport(LOG);
            let b = match code.sqlstate() {
                Some(s) => b.errcode(types_error::make_sqlstate(s)),
                None => b.errcode_for_file_access(),
            };
            let _ = b.errmsg(m).finish(loc(125, "set_pglocale_pgservice"));
        },
    );
    init_locale(mcx, "LC_COLLATE", LC_COLLATE, "")?;
    init_locale(mcx, "LC_CTYPE", LC_CTYPE, "")?;
    init_locale(mcx, "LC_MESSAGES", LC_MESSAGES, "")?;
    init_locale(mcx, "LC_MONETARY", LC_MONETARY, "C")?;
    init_locale(mcx, "LC_NUMERIC", LC_NUMERIC, "C")?;
    init_locale(mcx, "LC_TIME", LC_TIME, "C")?;
    // SAFETY: single-threaded process startup; no concurrent getenv.
    unsafe {
        libc::unsetenv(c"LC_ALL".as_ptr());
    }

    if argv.len() > 1 {
        let arg1 = argv[1].as_str();
        if arg1 == "--help" || arg1 == "-?" {
            write_stdout_ignoring_errors(&mut std::io::stdout(), &help(&progname));
            std::process::exit(0);
        }
        if arg1 == "--version" {
            write_stdout_ignoring_errors(&mut std::io::stdout(), &version_banner());
            std::process::exit(0);
        }
        // -V keeps PostgreSQL's string: initdb and pg_ctl run `postgres -V` and require it (find_other_exec).
        if arg1 == "-V" {
            write_stdout_ignoring_errors(&mut std::io::stdout(), PG_BACKEND_VERSIONSTR);
            std::process::exit(0);
        }
        if arg1 == "--describe-config" {
            do_check_root = false;
        } else if argv.len() > 2 && arg1 == "-C" {
            do_check_root = false;
        }
    }

    if do_check_root {
        check_root(&progname);
    }

    if let Some(rest) = argv.get(1).and_then(|a| a.strip_prefix("--")) {
        dispatch_option = parse_dispatch_option(rest);
    }

    match dispatch_option {
        // main.c:206: BootstrapModeMain(argc, argv, true). Exits the process.
        DispatchOption::Check => bootstrap::BootstrapModeMain(argv, true),
        // main.c:210: BootstrapModeMain(argc, argv, false). Exits the process.
        DispatchOption::Boot => bootstrap::BootstrapModeMain(argv, false),
        DispatchOption::Forkchild => {
            panic!("DISPATCH_FORKCHILD reached without EXEC_BACKEND")
        }
        // main.c:220: GucInfoMain() prints the GUC table and exits 0.
        DispatchOption::DescribeConfig => guc::help_config::GucInfoMain(),
        DispatchOption::Single => {
            // main.c:222: PostgresSingleUserMain(argc, argv,
            // strdup(get_user_name_or_exit(progname))). Exits the process.
            let username = get_user_name_or_exit(&progname);
            postgres_seams::postgres_single_user_main::call(argv, &username)
        }
        DispatchOption::StdioWire => {
            // pgrust extension: identity ultimately comes from the startup
            // packet; the OS/env user is the single-user-style fallback.
            let username = get_user_name_or_exit(&progname);
            postgres_seams::postgres_stdio_wire_main::call(argv, &username)
        }
        #[cfg(pgrust_sim)]
        DispatchOption::SimNet => {
            // P4 sim-net (sim builds only): same identity story as the
            // stdio wire mode.
            let username = get_user_name_or_exit(&progname);
            postgres_seams::postgres_sim_net_main::call(argv, &username)
        }
        DispatchOption::Postmaster => {
            // The crash supervisor forks here; only the server child returns
            // (postmaster::supervisor).
            let argv = postmaster::supervisor::supervise(argv)?;
            postmaster::PostmasterMain(&argv)
        }
    }
}

fn startup_hacks(_progname: &str) {}

// get_user_name_or_exit (src/common/username.c:74): effective user's
// pw_name, or print the lookup error and exit(1).
// wasm32: no uids and no passwd db on WASI; the operator supplies the
// identity through the environment (wasmtime --env USER=<name>, matching
// the role the datadir was initdb'd with). Absent that, C's exit(1) shape.
#[cfg(target_family = "wasm")]
fn get_user_name_or_exit(progname: &str) -> String {
    match std::env::var("USER") {
        Ok(u) if !u.is_empty() => u,
        _ => {
            elog::write_stderr(&format!(
                "{progname}: could not determine the effective user name: \
                 set the USER environment variable\n"
            ));
            std::process::exit(1);
        }
    }
}

#[cfg(not(target_family = "wasm"))]
fn get_user_name_or_exit(progname: &str) -> String {
    // SAFETY: geteuid never fails; getpwuid returns a static-storage struct
    // (single-threaded startup, per C's use) or NULL with errno set.
    let (user_id, pw) = unsafe {
        let uid = libc::geteuid();
        set_errno_zero();
        (uid, libc::getpwuid(uid))
    };
    if pw.is_null() {
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        let errstr = user_lookup_errstr(user_id as i64, errno);
        elog::write_stderr(&format!("{progname}: {errstr}\n"));
        std::process::exit(1);
    }
    // SAFETY: non-NULL passwd from getpwuid has a NUL-terminated pw_name.
    unsafe { std::ffi::CStr::from_ptr((*pw).pw_name) }
        .to_string_lossy()
        .into_owned()
}

// get_user_name (src/common/username.c:55-57): the getpwuid failure text,
// "could not look up effective user ID %ld: %s" with errno's strerror text
// or "user does not exist" when errno stayed clear.
#[cfg(not(target_family = "wasm"))]
fn user_lookup_errstr(user_id: i64, errno: i32) -> String {
    let detail = if errno != 0 {
        // SAFETY: strerror returns a NUL-terminated static string.
        unsafe { std::ffi::CStr::from_ptr(libc::strerror(errno)) }
            .to_string_lossy()
            .into_owned()
    } else {
        "user does not exist".to_string()
    };
    format!("could not look up effective user ID {user_id}: {detail}")
}

#[cfg(not(target_family = "wasm"))] // wasm32: only the native getpwuid path clears errno
fn set_errno_zero() {
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    // SAFETY: __error returns this thread's valid errno location.
    unsafe {
        *libc::__error() = 0;
    }
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    // SAFETY: __errno_location returns this thread's valid errno location.
    unsafe {
        *libc::__errno_location() = 0;
    }
}

/// `postgres --version`: which pgrust this is and the PostgreSQL release it is compatible with.
pub fn version_banner() -> String {
    format!("pgrust {} (PostgreSQL {} compatible)\n", guc_tables::consts::PGRUST_VERSION, guc_tables::consts::PG_COMPAT_VERSION)
}

pub fn help(progname: &str) -> String {
    let mut s = String::with_capacity(2048);
    s.push_str(&format!("{progname} is the PostgreSQL server.\n\n"));
    s.push_str(&format!("Usage:\n  {progname} [OPTION]...\n\n"));
    s.push_str("Options:\n");
    s.push_str("  -B NBUFFERS        number of shared buffers\n");
    s.push_str("  -c NAME=VALUE      set run-time parameter\n");
    s.push_str("  -C NAME            print value of run-time parameter, then exit\n");
    s.push_str("  -d 1-5             debugging level\n");
    s.push_str("  -D DATADIR         database directory\n");
    s.push_str("  -e                 use European date input format (DMY)\n");
    s.push_str("  -F                 turn fsync off\n");
    s.push_str("  -h HOSTNAME        host name or IP address to listen on\n");
    s.push_str("  -i                 enable TCP/IP connections (deprecated)\n");
    s.push_str("  -k DIRECTORY       Unix-domain socket location\n");
    s.push_str("  -l                 enable SSL connections\n");
    s.push_str("  -N MAX-CONNECT     maximum number of allowed connections\n");
    s.push_str("  -p PORT            port number to listen on\n");
    s.push_str("  -s                 show statistics after each query\n");
    s.push_str("  -S WORK-MEM        set amount of memory for sorts (in kB)\n");
    s.push_str("  -V, --version      output version information, then exit\n");
    s.push_str("  --NAME=VALUE       set run-time parameter\n");
    s.push_str("  --describe-config  describe configuration parameters, then exit\n");
    s.push_str("  -?, --help         show this help, then exit\n");
    s.push_str("\nDeveloper options:\n");
    s.push_str("  -f s|i|o|b|t|n|m|h forbid use of some plan types\n");
    s.push_str("  -O                 allow system table structure changes\n");
    s.push_str("  -P                 disable system indexes\n");
    s.push_str("  -t pa|pl|ex        show timings after each query\n");
    s.push_str("  -T                 send SIGABRT to all backend processes if one dies\n");
    s.push_str("  -W NUM             wait NUM seconds to allow attach from a debugger\n");
    s.push_str("\nOptions for single-user mode:\n");
    s.push_str("  --single           selects single-user mode (must be first argument)\n");
    s.push_str("  DBNAME             database name (defaults to user name)\n");
    s.push_str("  -d 0-5             override debugging level\n");
    s.push_str("  -E                 echo statement before execution\n");
    s.push_str("  -j                 do not use newline as interactive query delimiter\n");
    s.push_str("  -r FILENAME        send stdout and stderr to given file\n");
    s.push_str("\nOptions for bootstrapping mode:\n");
    s.push_str("  --boot             selects bootstrapping mode (must be first argument)\n");
    s.push_str("  --check            selects check mode (must be first argument)\n");
    s.push_str("  DBNAME             database name (mandatory argument in bootstrapping mode)\n");
    s.push_str("  -r FILENAME        send stdout and stderr to given file\n");
    s.push_str(
        "\nPlease read the documentation for the complete list of run-time\n\
         configuration settings and how to set them on the command line or in\n\
         the configuration file.\n\n\
         Report bugs to <pgsql-bugs@lists.postgresql.org>.\n",
    );
    s.push_str("PostgreSQL home page: <https://www.postgresql.org/>\n");
    s
}

/// the process's transparent-huge-page policy, set once at start. Small heaps stay on base
/// pages, large blocks take huge pages:
/// - mimalloc is built `no_thp` (Cargo.toml), so it never advises its arenas MADV_HUGEPAGE. That advice made each
///   thread's sparsely touched 64 KiB pages resident as whole 2 MiB pages, ~6 MB per idle session (wio P273), where C
///   backends' glibc heaps never opt in.
/// - The global allocator advises every block of `PGRUST_HEAP_HUGE_MIN_MB` (default 4, 0 = off) or more huge and
///   gives its range back to base pages at its free (`init_small::oom::set_huge_advise`): hash tables, dictionaries
///   and segments that span gigabytes run up to 1.5x slower on 4 KiB pages (an analytic scan suite, hot W0: 1.25x).
/// - `PR_SET_THP_DISABLE` with `PR_THP_DISABLE_EXCEPT_ADVISED` (Linux 6.18+) turns a THP=always host into THP=madvise
///   for this process, so the unadvised arenas stay on base pages there too. An older kernel refuses the flag: on a
///   THP=always host the whole process then goes off huge pages (P273's lean-first policy) unless
///   `PGRUST_HEAP_THP=keep`; on a THP=madvise or never host nothing needs to change.
/// An explicit `MIMALLOC_ALLOW_THP` keeps mimalloc's own choice and skips the prctl (=0 is mimalloc's full off).
pub fn heap_thp_policy() -> HeapThp {
    #[cfg(target_os = "linux")]
    {
        let policy = linux_heap_thp();
        // P6: not while PoolAlloc serves (the pool advises its own superpages; a block's advice would split them)
        if policy != HeapThp::Off && !pool_allocates() {
            let min_mb = std::env::var_os("PGRUST_HEAP_HUGE_MIN_MB").and_then(|v| v.to_str()?.trim().parse::<usize>().ok()).unwrap_or(4);
            let pmd = std::fs::read_to_string("/sys/kernel/mm/transparent_hugepage/hpage_pmd_size").ok().and_then(|t| t.trim().parse::<usize>().ok()).unwrap_or(0);
            init_small::oom::set_huge_advise(min_mb.saturating_mul(1 << 20), pmd);
        }
        policy
    }
    #[cfg(not(target_os = "linux"))]
    HeapThp::Kept
}

/// P6 / pphatch: the page pool's `PoolAlloc` serves this process's allocations (the `pagepool-global` build, unless
/// mimalloc was chosen at the first allocation: PGRUST_ALLOCATOR=mimalloc, or the server's default off Linux
/// aarch64). False in every other build.
#[inline]
pub fn pool_allocates() -> bool {
    #[cfg(all(feature = "pagepool-global", not(target_family = "wasm")))]
    return pagepool::pool_allocates();
    #[cfg(not(all(feature = "pagepool-global", not(target_family = "wasm"))))]
    false
}

/// pphatch: the global allocator's name for `pgrust.allocator` and the start's log line.
pub fn allocator_name() -> &'static str {
    if pool_allocates() {
        "hearth"
    } else if cfg!(target_family = "wasm") {
        "system"
    } else {
        "mimalloc"
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeapThp {
    /// THP off for the process except advised ranges (Linux 6.18+): the large blocks keep huge pages on any host.
    ExceptAdvised,
    /// No prctl (a pre-6.18 kernel): the host's THP=madvise or never already keeps the unadvised arenas on base pages.
    HostMadvise,
    /// A pre-6.18 kernel on a THP=always host: THP off for the whole process (P273), large blocks included.
    Off,
    /// `MIMALLOC_ALLOW_THP` or `PGRUST_HEAP_THP=keep` set, or not Linux: the host's and mimalloc's own choice.
    Kept,
}

#[cfg(target_os = "linux")]
fn linux_heap_thp() -> HeapThp {
    const PR_THP_DISABLE_EXCEPT_ADVISED: libc::c_ulong = 1 << 1;
    if std::env::var_os("MIMALLOC_ALLOW_THP").is_some() {
        return HeapThp::Kept;
    }
    // SAFETY: integer-argument prctl; changes only this process's THP policy (EINVAL before Linux 6.18).
    if unsafe { libc::prctl(libc::PR_SET_THP_DISABLE, 1 as libc::c_ulong, PR_THP_DISABLE_EXCEPT_ADVISED, 0 as libc::c_ulong, 0 as libc::c_ulong) } == 0 {
        return HeapThp::ExceptAdvised;
    }
    let always = std::fs::read_to_string("/sys/kernel/mm/transparent_hugepage/enabled").is_ok_and(|t| t.contains("[always]"));
    if !always {
        return HeapThp::HostMadvise;
    }
    if std::env::var_os("PGRUST_HEAP_THP").is_some_and(|v| v == "keep") {
        return HeapThp::Kept;
    }
    // SAFETY: as above, without the flag.
    unsafe { libc::prctl(libc::PR_SET_THP_DISABLE, 1 as libc::c_ulong, 0 as libc::c_ulong, 0 as libc::c_ulong, 0 as libc::c_ulong) };
    HeapThp::Off
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_names_pgrust_and_the_compatible_postgres() {
        assert_eq!(
            version_banner(),
            format!("pgrust {} (PostgreSQL 18.6 compatible)\n", guc_tables::consts::PGRUST_VERSION)
        );
        assert_eq!(PG_BACKEND_VERSIONSTR, "postgres (PostgreSQL) 18.6\n", "-V stays PostgreSQL's for initdb / pg_ctl");
    }

    /// the policy's prctl state, by what it set: off except advised ranges (1 | PR_THP_DISABLE_EXCEPT_ADVISED),
    /// off (1), or untouched (0).
    #[cfg(target_os = "linux")]
    #[test]
    fn heap_thp_policy_keeps_advised_ranges_huge() {
        if std::env::var_os("MIMALLOC_ALLOW_THP").is_some() || std::env::var_os("PGRUST_HEAP_THP").is_some() {
            return;
        }
        let policy = heap_thp_policy();
        // SAFETY: integer-argument prctl query.
        let got = unsafe { libc::prctl(libc::PR_GET_THP_DISABLE, 0, 0, 0, 0) };
        let want = match policy {
            HeapThp::ExceptAdvised => 3,
            HeapThp::Off => 1,
            HeapThp::HostMadvise | HeapThp::Kept => 0,
        };
        assert_eq!(got, want, "{policy:?}");
        if policy != HeapThp::Off && std::fs::read_to_string("/sys/kernel/mm/transparent_hugepage/hpage_pmd_size").is_ok() {
            // while PoolAlloc serves, the pool advises its own superpages: no block advice
            let want = if pool_allocates() { 0 } else { 4 << 20 };
            assert_eq!(init_small::oom::huge_advise_min(), want, "the default threshold");
        }
    }

    /// D3143 P1 (wio P273): mimalloc advises none of its own memory huge (built `no_thp`): a small block's mapping
    /// carries no `hg` flag, so an idle session's first pages fault base pages.
    #[cfg(target_os = "linux")]
    #[test]
    fn mimalloc_advises_no_small_block_huge() {
        use std::alloc::{GlobalAlloc, Layout};
        let l = Layout::from_size_align(64 << 10, 16).unwrap();
        let addr = std::thread::spawn(move || {
            // SAFETY: a non-zero layout; freed with the same layout.
            let p = unsafe { mimalloc::MiMalloc.alloc(l) };
            assert!(!p.is_null());
            // SAFETY: the block is 64 KiB.
            unsafe { p.write_bytes(1, l.size()) };
            let flags = std::fs::read_to_string("/proc/self/smaps").unwrap_or_default();
            let mut inside = false;
            let mut vm = String::new();
            for line in flags.lines() {
                let range = line.split_whitespace().next().unwrap_or("");
                if let Some((lo, hi)) = range.split_once('-').and_then(|(a, b)| Some((usize::from_str_radix(a, 16).ok()?, usize::from_str_radix(b, 16).ok()?))) {
                    inside = lo <= p as usize && (p as usize) < hi;
                } else if inside && line.starts_with("VmFlags:") {
                    vm = line.to_string();
                    break;
                }
            }
            // SAFETY: allocated above with `l`.
            unsafe { mimalloc::MiMalloc.dealloc(p, l) };
            vm
        })
        .join()
        .unwrap();
        assert!(!addr.is_empty() && !addr.split_whitespace().any(|f| f == "hg"), "mimalloc advised its arena huge: {addr}");
    }

    // main.c:400 under USE_SSL (be_secure's default-on ssl feature).
    #[test]
    fn help_lists_the_ssl_switch() {
        let h = help("postgres");
        let k = h.find("  -k DIRECTORY       Unix-domain socket location\n").unwrap();
        assert!(h[k..].starts_with(
            "  -k DIRECTORY       Unix-domain socket location\n  -l                 enable SSL connections\n  -N MAX-CONNECT"
        ));
    }

    #[test]
    fn dispatch_names_match_c() {
        assert_eq!(parse_dispatch_option("check"), DispatchOption::Check);
        assert_eq!(parse_dispatch_option("boot"), DispatchOption::Boot);
        assert_eq!(parse_dispatch_option("describe-config"), DispatchOption::DescribeConfig);
        assert_eq!(parse_dispatch_option("single"), DispatchOption::Single);
        assert_eq!(parse_dispatch_option("stdio-wire"), DispatchOption::StdioWire);
        assert_eq!(parse_dispatch_option("forkchild"), DispatchOption::Postmaster);
        assert_eq!(parse_dispatch_option("nonsense"), DispatchOption::Postmaster);
        assert_eq!(parse_dispatch_option(""), DispatchOption::Postmaster);
    }

    // username.c:55: `errno ? strerror(errno) : _("user does not exist")` —
    // the strerror text alone, never Rust's "(os error N)" suffix.
    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn user_lookup_errstr_is_strerror() {
        // SAFETY: strerror returns a NUL-terminated static string.
        let want = unsafe { std::ffi::CStr::from_ptr(libc::strerror(libc::EIO)) }
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            user_lookup_errstr(7, libc::EIO),
            format!("could not look up effective user ID 7: {want}")
        );
        assert_eq!(
            user_lookup_errstr(7, 0),
            "could not look up effective user ID 7: user does not exist"
        );
    }

    #[test]
    fn progname_is_basename() {
        assert_eq!(get_progname("/usr/local/bin/postgres"), "postgres");
        assert_eq!(get_progname("postgres"), "postgres");
    }

    fn sv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn help_and_version_answer_before_profile_expansion() {
        // main.c:163-176 never reads argv[2..] once argv[1] is --help/--version.
        assert_eq!(expand_profile_argv(&sv(&["postgres", "--help", "--profile"])), Ok(None));
        assert_eq!(expand_profile_argv(&sv(&["postgres", "-?", "--profile=nope"])), Ok(None));
        assert_eq!(
            expand_profile_argv(&sv(&["postgres", "--version", "--profile=unknown"])),
            Ok(None)
        );
        assert_eq!(expand_profile_argv(&sv(&["postgres", "-V", "--profile"])), Ok(None));
        assert_eq!(
            expand_profile_argv(&sv(&["postgres", "-D", "dd", "--help", "--profile"])),
            Err(ProfileArgvError::MissingValue)
        );
    }

    #[test]
    fn version_option_answers_exactly_as_c_postgres() {
        assert_eq!(PG_BACKEND_VERSIONSTR, "postgres (PostgreSQL) 18.6\n");
        assert!(!PG_BACKEND_VERSIONSTR.contains("pgrust"));
    }

    #[test]
    fn build_id_stamp_is_empty_or_a_plain_token() {
        assert!(PGRUST_BUILD_ID.len() <= 40, "{PGRUST_BUILD_ID:?}");
        assert!(
            PGRUST_BUILD_ID.bytes().all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b)),
            "{PGRUST_BUILD_ID:?}"
        );
    }

    struct FailingWriter;

    impl std::io::Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from_raw_os_error(28))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::from_raw_os_error(28))
        }
    }

    #[test]
    fn help_output_ignores_write_failure() {
        write_stdout_ignoring_errors(&mut FailingWriter, &help("postgres"));
        write_stdout_ignoring_errors(&mut FailingWriter, PG_BACKEND_VERSIONSTR);
        let mut buf = Vec::new();
        write_stdout_ignoring_errors(&mut buf, PG_BACKEND_VERSIONSTR);
        assert_eq!(buf, PG_BACKEND_VERSIONSTR.as_bytes());
    }

    #[cfg(unix)]
    #[test]
    fn argv_from_os_tolerates_non_utf8_bytes() {
        use std::os::unix::ffi::OsStringExt;
        let argv = argv_from_os([
            std::ffi::OsString::from("postgres"),
            std::ffi::OsString::from("--help"),
            std::ffi::OsString::from_vec(vec![0xff]),
        ]);
        assert_eq!(argv[0], "postgres");
        assert_eq!(argv[1], "--help");
        assert_eq!(argv[2], "\u{fffd}");
    }

    #[test]
    fn profile_absent_is_untouched() {
        // Stock invocations: expansion returns None (caller keeps the slice).
        assert_eq!(expand_profile_argv(&sv(&["postgres", "-D", "dd"])), Ok(None));
        assert_eq!(expand_profile_argv(&sv(&["postgres"])), Ok(None));
        // argv[0] never participates, even if pathological.
        assert_eq!(expand_profile_argv(&sv(&["--profile"])), Ok(None));
    }

    #[test]
    fn profile_expands_in_place() {
        // Expansion happens AT THAT POSITION: -D before, explicit -c after.
        let out = expand_profile_argv(&sv(&[
            "postgres", "-D", "dd", "--profile", "test", "-c", "fsync=on",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(&out[..3], &sv(&["postgres", "-D", "dd"])[..]);
        let mut want = Vec::new();
        for (k, v) in PROFILE_TEST_SETTINGS {
            want.push("-c".to_string());
            want.push(format!("{k}={v}"));
        }
        assert_eq!(&out[3..3 + want.len()], &want[..]);
        // Later explicit -c stays AFTER the expansion — the precedence claim
        // ("as-if -c at that argv position") holds by construction: same
        // PGC_S_ARGV source, later SetConfigOption call wins.
        assert_eq!(&out[3 + want.len()..], &sv(&["-c", "fsync=on"])[..]);
        // fsync appears profile-first, override-second in effective order.
        let fsync_positions: Vec<usize> = out
            .iter()
            .enumerate()
            .filter(|(_, a)| a.starts_with("fsync="))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(out[fsync_positions[0]], "fsync=off");
        assert_eq!(out[fsync_positions[1]], "fsync=on");
    }

    #[test]
    fn profile_equals_form_and_errors() {
        let a = expand_profile_argv(&sv(&["postgres", "--profile", "test"])).unwrap().unwrap();
        let b = expand_profile_argv(&sv(&["postgres", "--profile=test"])).unwrap().unwrap();
        assert_eq!(a, b);
        assert_eq!(
            expand_profile_argv(&sv(&["postgres", "--profile"])),
            Err(ProfileArgvError::MissingValue)
        );
        assert_eq!(
            expand_profile_argv(&sv(&["postgres", "--profile="])),
            Err(ProfileArgvError::MissingValue)
        );
        assert_eq!(
            expand_profile_argv(&sv(&["postgres", "--profile", "prod"])),
            Err(ProfileArgvError::UnknownProfile("prod".into()))
        );
        assert!(profile_settings("test").is_some());
        assert!(profile_settings("prod").is_none());
        for name in KNOWN_PROFILES {
            assert!(profile_settings(name).is_some(), "KNOWN_PROFILES lists {name} but profile_settings has no arm");
        }
    }

    // Drift gate: the `--profile test` table's conf-file section must equal
    // conf/test.conf key-for-key, value-for-value, in file order — plus
    // exactly the two janitor-arming GUCs. A divergence between file and
    // flag would be a silent doc lie (docs/testmode.md documents the flag
    // as a macro for the file).
    #[test]
    fn conf_sync() {
        // include_str! (not fs::read): the file lives INSIDE this crate so any
        // tree slice that carries the code carries the file (release cuts
        // stripped top-level conf/ and broke the fs::read form, 2026-08-04).
        let text = include_str!("../conf/test.conf");
        let mut file_settings: Vec<(String, String)> = Vec::new();
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let (k, v) = line.split_once('=').expect("conf line must be name = value");
            // GUC-file quoting is file-syntax only: a quoted file value
            // ('*') and the argv form (-c ...=*) denote the same setting,
            // so strip one layer of single quotes before comparing.
            let v = v.trim();
            let v = v
                .strip_prefix('\'')
                .and_then(|m| m.strip_suffix('\''))
                .unwrap_or(v);
            file_settings.push((k.trim().to_string(), v.to_string()));
        }
        assert!(!file_settings.is_empty(), "parsed zero settings from conf/test.conf");
        let table: Vec<(String, String)> = PROFILE_TEST_SETTINGS
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        // Conf section: identical, in order.
        assert_eq!(
            &table[..file_settings.len()],
            &file_settings[..],
            "PROFILE_TEST_SETTINGS conf section drifted from conf/test.conf"
        );
        // Remainder: exactly the janitor arming pair. mint_roles = '*' now
        // lives in the conf section BY RULING (2026-08-05: the profile
        // declares a disposable test server).
        let rest = &table[file_settings.len()..];
        assert_eq!(
            rest,
            &[
                ("pgrust.ephemeral_db_prefix".to_string(), "tdb_".to_string()),
                ("pgrust.ephemeral_db_grace".to_string(), "15s".to_string()),
            ],
            "profile extras must be exactly the janitor arming pair"
        );
        assert!(
            table
                .iter()
                .any(|(k, v)| k == "pgrust.ephemeral_db_mint_roles" && v == "*"),
            "the test profile must enable minting for all roles (ruling 2026-08-05)"
        );
        for (k, _) in &table {
            assert!(
                !k.contains("default_template"),
                "the default-template GUC was DELETED (2026-08-05): the template is always \
                 in the database name — the profile must never resurrect it: {k}"
            );
        }
    }
}
