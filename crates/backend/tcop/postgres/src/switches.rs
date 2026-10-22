// process_postgres_switches (postgres.c:3790). The client-argv arm is the hot
// caller (postinit startup options); the secure arm (ctx == PGC_POSTMASTER)
// is the single-user command line, with the -E/-j/-r/-D/-b/-v specials and
// the trailing DBNAME pickup.

use std::cell::RefCell;

use ::types_error::{PgResult, ERRCODE_SYNTAX_ERROR, ERROR, FATAL};
use elog::ereport;
use types_guc::{GucContext, GucSource};

use crate::{
    get_stats_option_name, guc_context_from_u8, loc, set_debug_options,
    set_plan_disabling_options,
};

const OPTSTRING: &[u8] = b"BbCcDdEeFfhijklNnOPprSsTtvW-";
const ARG_TAKING_FLAGS: &[u8] = b"BCcDdfhkNprStvW-";

thread_local! {
    // userDoption (postgres.c:106): -D, consumed by SelectConfigFiles in
    // PostgresSingleUserMain.
    static USER_D_OPTION: RefCell<Option<String>> = const { RefCell::new(None) };
}

pub(crate) fn user_d_option() -> Option<String> {
    USER_D_OPTION.with_borrow(|v| v.clone())
}

// atoi = (int) strtol(s, NULL, 10) on the LP64 reference platforms: strtol
// saturates at LONG_MIN/LONG_MAX, then the int conversion truncates.
fn c_atoi(s: &str) -> i32 {
    // atoi skips C-locale isspace only, not Unicode whitespace.
    let t = s.trim_start_matches(|c: char| c.is_ascii() && pg_string::isspace_c_locale(c as u8));
    let (neg, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let mut v: u64 = 0;
    for b in digits.bytes().take_while(|b| b.is_ascii_digit()) {
        v = (v.saturating_mul(10).saturating_add((b - b'0') as u64)).min(1 << 63);
    }
    let long = if neg { (v as i64).wrapping_neg() } else { v.min(i64::MAX as u64) as i64 };
    long as i32
}

fn is_dispatch_option(name: &str) -> bool {
    // DispatchOptionNames (main.c) minus forkchild (EXEC_BACKEND only);
    // parse_dispatch_option strcmp's the whole optarg, so "single=1" is a
    // GUC assignment, not a misplaced dispatch option.
    // + the pgrust-extension stdio-wire / sim-net modes (main_main dispatch).
    matches!(name, "check" | "boot" | "describe-config" | "single" | "stdio-wire" | "sim-net")
}

pub fn process_postgres_switches(argv: &[String], gucctx: u8) -> PgResult<()> {
    process_postgres_switches_inner(argv, gucctx, None)
}

/// The &dbname form (single-user): first bare argument becomes the database
/// name instead of an error.
pub fn process_postgres_switches_dbname(
    argv: &[String],
    gucctx: u8,
    dbname: &mut Option<String>,
) -> PgResult<()> {
    process_postgres_switches_inner(argv, gucctx, Some(dbname))
}

// glibc getopt(3) in its default PERMUTE ordering (postgres.c:3833: the
// optstring has no leading '+' and POSIXLY_CORRECT is unset): non-option
// elements are skipped, exchanged behind the options that follow them and
// left at the tail for the caller. BSD getopt stops at the first
// non-option instead; the reference platform is glibc.
struct Getopt {
    argv: Vec<String>,
    optind: usize,
    nextchar: Vec<u8>,
    first_nonopt: usize,
    last_nonopt: usize,
}

enum Opt {
    Flag(u8, Option<String>),
    Bad,
    End,
}

impl Getopt {
    fn new(argv: Vec<String>) -> Self {
        Getopt { argv, optind: 1, nextchar: Vec::new(), first_nonopt: 1, last_nonopt: 1 }
    }

    fn exchange(&mut self) {
        let (f, l, o) = (self.first_nonopt, self.last_nonopt, self.optind);
        self.argv[f..o].rotate_left(l - f);
        self.first_nonopt += o - l;
        self.last_nonopt = o;
    }

    fn next(&mut self) -> Opt {
        let argc = self.argv.len();
        if self.nextchar.is_empty() {
            if self.last_nonopt > self.optind {
                self.last_nonopt = self.optind;
            }
            if self.first_nonopt > self.optind {
                self.first_nonopt = self.optind;
            }
            if self.first_nonopt != self.last_nonopt && self.last_nonopt != self.optind {
                self.exchange();
            } else if self.last_nonopt != self.optind {
                self.first_nonopt = self.optind;
            }
            while self.optind < argc && is_nonoption(&self.argv[self.optind]) {
                self.optind += 1;
            }
            self.last_nonopt = self.optind;
            if self.optind != argc && self.argv[self.optind] == "--" {
                self.optind += 1;
                if self.first_nonopt != self.last_nonopt && self.last_nonopt != self.optind {
                    self.exchange();
                } else if self.first_nonopt == self.last_nonopt {
                    self.first_nonopt = self.optind;
                }
                self.last_nonopt = argc;
                self.optind = argc;
            }
            if self.optind == argc {
                if self.first_nonopt != self.last_nonopt {
                    self.optind = self.first_nonopt;
                }
                return Opt::End;
            }
            self.nextchar = self.argv[self.optind].as_bytes()[1..].to_vec();
        }
        let flag = self.nextchar.remove(0);
        if self.nextchar.is_empty() {
            self.optind += 1;
        }
        if !OPTSTRING.contains(&flag) {
            return Opt::Bad;
        }
        if !ARG_TAKING_FLAGS.contains(&flag) {
            return Opt::Flag(flag, None);
        }
        let optarg = if !self.nextchar.is_empty() {
            let rest = core::mem::take(&mut self.nextchar);
            self.optind += 1;
            String::from_utf8(rest).expect("rest after an ASCII flag")
        } else if self.optind == argc {
            return Opt::Bad;
        } else {
            self.optind += 1;
            self.argv[self.optind - 1].clone()
        };
        Opt::Flag(flag, Some(optarg))
    }
}

fn is_nonoption(arg: &str) -> bool {
    let b = arg.as_bytes();
    b.first() != Some(&b'-') || b.len() < 2
}

fn process_postgres_switches_inner(
    argv: &[String],
    gucctx: u8,
    dbname: Option<&mut Option<String>>,
) -> PgResult<()> {
    let ctx = guc_context_from_u8(gucctx);
    let secure = ctx == GucContext::PGC_POSTMASTER;
    let source = if secure { GucSource::PGC_S_ARGV } else { GucSource::PGC_S_CLIENT };
    let fname = "process_postgres_switches";

    let set = |name: &str, value: &str| guc::SetConfigOption(name, Some(value), ctx, source);

    // Ignore the initial --single (or pgrust --stdio-wire / --sim-net)
    // argument, if present: C shifts argv so it becomes argv[0].
    let skip = usize::from(
        secure
            && argv
                .get(1)
                .is_some_and(|a| a == "--single" || a == "--stdio-wire" || a == "--sim-net"),
    );
    let mut g = Getopt::new(argv.get(skip..).unwrap_or_default().to_vec());

    let mut errs = 0usize;
    loop {
        let (flag, optarg) = match g.next() {
            Opt::End => break,
            Opt::Bad => {
                errs += 1;
                break;
            }
            Opt::Flag(flag, optarg) => (flag, optarg.unwrap_or_default()),
        };
        let optarg = optarg.as_str();
        match flag {
            b'B' => set("shared_buffers", optarg)?,
            // 'C'/'n'/'T' are always ignored (consistency with the
            // postmaster); the rest are secure-only.
            b'C' | b'n' | b'T' => {}
            b'b' => {
                /* Undocumented flag used for binary upgrades */
                if secure {
                    init_small::globals::SetIsBinaryUpgrade(true);
                }
            }
            b'D' => {
                if secure {
                    USER_D_OPTION.with_borrow_mut(|v| *v = Some(optarg.to_string()));
                }
            }
            b'E' => {
                if secure {
                    crate::set_echo_query(true);
                }
            }
            b'j' => {
                if secure {
                    crate::set_use_semi_newline_newline(true);
                }
            }
            b'r' => {
                /* send output (stdout and stderr) to the given file */
                if secure {
                    let mut buf = [0u8; types_core::MAXPGPATH];
                    let n = optarg.len().min(types_core::MAXPGPATH - 1);
                    buf[..n].copy_from_slice(&optarg.as_bytes()[..n]);
                    init_small::globals::SetOutputFileName(buf);
                    // C has one OutputFileName global; this tree has two
                    // slots (elog can't depend on init_small). The only
                    // consumer, elog::DebugFileOpen (BaseInit), reads the
                    // elog mirror — keep it in sync, same truncation.
                    elog::config::set_output_file_name(Some(
                        String::from_utf8_lossy(&buf[..n]).into_owned(),
                    ));
                }
            }
            // -v (FrontendProtocol override): kept by C only for a
            // hypothetical FE/BE-protocol standalone mode; storage exists
            // (init_small::globals::SetFrontendProtocol) but nothing
            // standalone reads it, so it is deliberately parsed and
            // dropped here.
            b'v' => {}
            b'-' | b'c' => {
                if flag == b'-' && is_dispatch_option(optarg) {
                    return ereport(ERROR)
                        .errcode(ERRCODE_SYNTAX_ERROR)
                        .errmsg(format!("--{optarg} must be first argument"))
                        .finish(loc(3975, fname));
                }
                let (name, value) = guc::ParseLongOption(optarg);
                let Some(value) = value else {
                    let msg = if flag == b'-' {
                        format!("--{optarg} requires a value")
                    } else {
                        format!("-c {optarg} requires a value")
                    };
                    return ereport(ERROR)
                        .errcode(ERRCODE_SYNTAX_ERROR)
                        .errmsg(msg)
                        .finish(loc(3994, fname));
                };
                set(&name, &value)?;
            }
            b'd' => set_debug_options(c_atoi(optarg), gucctx)?,
            b'e' => set("datestyle", "euro")?,
            b'F' => set("fsync", "false")?,
            b'f' => {
                if !set_plan_disabling_options(optarg, gucctx)? {
                    errs += 1;
                }
            }
            b'h' => set("listen_addresses", optarg)?,
            b'i' => set("listen_addresses", "*")?,
            b'k' => set("unix_socket_directories", optarg)?,
            b'l' => set("ssl", "true")?,
            b'N' => set("max_connections", optarg)?,
            b'O' => set("allow_system_table_mods", "true")?,
            b'P' => set("ignore_system_indexes", "true")?,
            b'p' => set("port", optarg)?,
            b'S' => set("work_mem", optarg)?,
            b's' => set("log_statement_stats", "true")?,
            b't' => match get_stats_option_name(optarg) {
                Some(name) => set(name, "true")?,
                None => errs += 1,
            },
            b'W' => set("post_auth_delay", optarg)?,
            _ => errs += 1,
        }
        if errs > 0 {
            break;
        }
    }

    let Getopt { argv, mut optind, .. } = g;
    if errs == 0 {
        if let Some(dbname) = dbname {
            if dbname.is_none() && argv.len() - optind >= 1 {
                *dbname = Some(argv[optind].clone());
                optind += 1;
            }
        }
    }

    if errs > 0 || argv.len() != optind {
        if errs > 0 {
            /* complain about the previous argument */
            optind -= 1;
        }
        let badarg = &argv[optind];
        // Spelled differently depending on context, as in C (postgres.c:3999).
        let msg = if init_small::globals::IsUnderPostmaster() {
            format!("invalid command-line argument for server process: {badarg}")
        } else {
            format!("postgres: invalid command-line argument: {badarg}")
        };
        return ereport(FATAL)
            .errcode(ERRCODE_SYNTAX_ERROR)
            .errmsg(msg)
            .errhint("Try \"postgres --help\" for more information.")
            .finish(loc(4165, fname));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // FATAL emits and proc_exits, so FATAL arms assert on a panicking stub.
    fn setup() {
        crate::session_tests::install_shared_stubs();
    }

    fn av(args: &[&str]) -> Vec<String> {
        let mut v = vec!["postgres".to_string()];
        v.extend(args.iter().map(|s| s.to_string()));
        v
    }

    // SetConfigOption arms need the full guc harness; these pin the arms
    // that fail before any GUC write.
    #[test]
    fn empty_argv_is_ok() {
        assert!(process_postgres_switches(&av(&[]), GucContext::PGC_BACKEND as u8).is_ok());
    }

    /// the proc_exit stub unwinds the exit payload (D2933), as the real one does
    fn assert_proc_exit_1(f: impl FnOnce()) {
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
            .expect_err("expected proc_exit(1)");
        let code = payload.downcast_ref::<ipc::ProcExitThread>().map(|e| e.code);
        assert_eq!(code, Some(1), "expected proc_exit(1)");
    }

    #[test]
    fn unknown_switch_is_fatal() {
        setup();
        assert_proc_exit_1(|| {
            let _ = process_postgres_switches(&av(&["-Z"]), GucContext::PGC_BACKEND as u8);
        });
    }

    #[test]
    fn multibyte_switch_is_fatal_not_a_panic() {
        setup();
        assert_proc_exit_1(|| {
            let _ = process_postgres_switches(&av(&["-é"]), GucContext::PGC_BACKEND as u8);
        });
    }

    #[test]
    fn trailing_dbname_rejected_when_no_out_param() {
        setup();
        assert_proc_exit_1(|| {
            let _ = process_postgres_switches(&av(&["mydb"]), GucContext::PGC_BACKEND as u8);
        });
    }

    // postgres.c:3896 atoi(optarg): values past INT_MAX wrap through the
    // long -> int conversion (2147483648 -> INT_MIN, 4294967296 -> 0) and
    // magnitudes past LONG_MAX saturate first (-> -1 / 0); all <= 0, so -d
    // sets log_min_messages=notice instead of failing on "debug2147483647".
    #[test]
    fn c_atoi_wraps_like_strtol_to_int() {
        assert_eq!(c_atoi("3"), 3);
        assert_eq!(c_atoi(" +12x"), 12);
        assert_eq!(c_atoi("-5"), -5);
        assert_eq!(c_atoi("2147483647"), i32::MAX);
        assert_eq!(c_atoi("2147483648"), i32::MIN);
        assert_eq!(c_atoi("4294967296"), 0);
        assert_eq!(c_atoi("99999999999999999999"), -1);
        assert_eq!(c_atoi("-9999999999999999999999"), 0);
        assert_eq!(c_atoi("-9223372036854775808"), 0);
        assert_eq!(c_atoi("9223372036854775807"), -1);
        assert_eq!(c_atoi("abc"), 0);
    }

    #[test]
    fn c_without_value_errors() {
        let err = process_postgres_switches(&av(&["-c", "work_mem"]), GucContext::PGC_BACKEND as u8)
            .unwrap_err();
        assert!(format!("{err:?}").contains("-c work_mem requires a value"));
    }

    #[test]
    fn misplaced_dispatch_option_errors() {
        let err = process_postgres_switches(&av(&["--single"]), GucContext::PGC_BACKEND as u8)
            .unwrap_err();
        assert!(format!("{err:?}").contains("--single must be first argument"));
    }

    // parse_dispatch_option (main.c:260) strcmp's the whole optarg, so
    // "single=1" is a configuration assignment, not a misplaced --single.
    #[test]
    fn dispatch_name_with_value_is_a_guc_assignment() {
        setup();
        let outcome = std::panic::catch_unwind(|| {
            process_postgres_switches(&av(&["--single=1"]), GucContext::PGC_BACKEND as u8)
        });
        if let Ok(Err(err)) = outcome {
            assert!(!format!("{err:?}").contains("must be first argument"), "{err:?}");
        }
    }

    // postgres.c:4006-4011: errs stops the getopt loop, and optind-- names
    // argv[optind - 1], which is the previous argument while a cluster is
    // still being consumed.
    #[test]
    fn unknown_switch_stops_the_cluster_and_blames_getopt_argument() {
        use std::sync::Mutex;
        setup();
        crate::session_tests::install_fixtures();
        static SEEN: Mutex<Option<String>> = Mutex::new(None);
        fn capture(e: &types_error::PgError, _output_to_server: &mut bool) {
            if e.level() == FATAL {
                *SEEN.lock().unwrap() = Some(e.message.clone());
            }
        }
        let run = |args: &[&str]| -> String {
            *SEEN.lock().unwrap() = None;
            let argv = av(args);
            let prev = elog::sink::set_emit_log_hook(Some(capture));
            let outcome = std::panic::catch_unwind(|| {
                process_postgres_switches(&argv, GucContext::PGC_BACKEND as u8)
            });
            elog::sink::set_emit_log_hook(prev);
            assert!(!matches!(outcome, Ok(Ok(_))), "no FATAL for {args:?}");
            SEEN.lock().unwrap().take().expect("FATAL emitted")
        };
        assert_eq!(run(&["-Zcsingle=1"]), "postgres: invalid command-line argument: postgres");
        assert_eq!(run(&["-Z"]), "postgres: invalid command-line argument: -Z");
        assert_eq!(run(&["-e", "-Zc"]), "postgres: invalid command-line argument: -e");
        assert_eq!(run(&["-f", "xyz"]), "postgres: invalid command-line argument: xyz");
        assert_eq!(run(&["-fxyz"]), "postgres: invalid command-line argument: -fxyz");
    }

    // postgres.c:4000: -W sets post_auth_delay (it was in ARG_TAKING_FLAGS
    // without a match arm, so every -W fell into the invalid-argument FATAL).
    #[test]
    fn w_switch_sets_post_auth_delay() {
        crate::session_tests::install_fixtures();
        process_postgres_switches(&av(&["-W", "0"]), GucContext::PGC_BACKEND as u8).unwrap();
        assert_eq!(
            guc::GetConfigOption("post_auth_delay", false, false)
                .unwrap()
                .as_deref(),
            Some("0")
        );
    }

    // postgres.c:4016: getopt consumes "--" and the database name may
    // still follow it.
    #[test]
    fn dbname_after_double_dash() {
        let mut dbname = None;
        process_postgres_switches_dbname(
            &av(&["--", "mydb"]),
            GucContext::PGC_POSTMASTER as u8,
            &mut dbname,
        )
        .unwrap();
        assert_eq!(dbname.as_deref(), Some("mydb"));
    }

    #[test]
    fn second_argument_after_double_dash_dbname_is_fatal() {
        setup();
        let mut dbname = None;
        // The harness's proc_exit stub unwinds the exit payload as the real
        // proc_exit does (D2933), not a message.
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = process_postgres_switches_dbname(
                &av(&["--", "mydb", "extra"]),
                GucContext::PGC_POSTMASTER as u8,
                &mut dbname,
            );
        }))
        .expect_err("a second argument after -- exits");
        let exit = payload.downcast_ref::<ipc::ProcExitThread>().expect("the proc_exit payload");
        assert_eq!(exit.code, 1);
    }

    // glibc getopt permutes argv (postgres.c:3833 optstring has no leading
    // '+'): options after a non-option are still processed, and the
    // non-options collect at the tail where the database-name pickup and
    // the argc != optind check see them. Expected values are hand-derived
    // from glibc's getopt.c and confirmed on a Linux 18.6 build (the macOS
    // oracle's BSD getopt stops at the first non-option).
    #[test]
    fn options_after_the_dbname_are_processed() {
        crate::session_tests::install_fixtures();
        let mut dbname = None;
        process_postgres_switches_dbname(
            &av(&["mydb", "-W", "0"]),
            GucContext::PGC_POSTMASTER as u8,
            &mut dbname,
        )
        .unwrap();
        assert_eq!(dbname.as_deref(), Some("mydb"));
        assert_eq!(
            guc::GetConfigOption("post_auth_delay", false, false)
                .unwrap()
                .as_deref(),
            Some("0")
        );
    }

    #[test]
    fn double_dash_ends_the_option_scan() {
        crate::session_tests::install_fixtures();
        let mut dbname = None;
        process_postgres_switches_dbname(
            &av(&["-W", "0", "--", "-Z"]),
            GucContext::PGC_POSTMASTER as u8,
            &mut dbname,
        )
        .unwrap();
        assert_eq!(dbname.as_deref(), Some("-Z"));
    }

    #[test]
    fn permuted_argv_names_the_bad_argument() {
        use std::sync::Mutex;
        setup();
        crate::session_tests::install_fixtures();
        static SEEN: Mutex<Option<String>> = Mutex::new(None);
        fn capture(e: &types_error::PgError, _output_to_server: &mut bool) {
            if e.level() == FATAL {
                *SEEN.lock().unwrap() = Some(e.message.clone());
            }
        }
        let run = |args: &[&str], with_dbname: bool| -> String {
            *SEEN.lock().unwrap() = None;
            let argv = av(args);
            let prev = elog::sink::set_emit_log_hook(Some(capture));
            let outcome = std::panic::catch_unwind(|| {
                if with_dbname {
                    let mut dbname = None;
                    process_postgres_switches_dbname(
                        &argv,
                        GucContext::PGC_POSTMASTER as u8,
                        &mut dbname,
                    )
                } else {
                    process_postgres_switches(&argv, GucContext::PGC_BACKEND as u8)
                }
            });
            elog::sink::set_emit_log_hook(prev);
            assert!(!matches!(outcome, Ok(Ok(_))), "no FATAL for {args:?}");
            SEEN.lock().unwrap().take().expect("FATAL emitted")
        };
        assert_eq!(run(&["junk", "-Z"], false), "postgres: invalid command-line argument: -Z");
        assert_eq!(run(&["junk", "-Zc"], false), "postgres: invalid command-line argument: junk");
        assert_eq!(
            run(&["a", "-W", "0", "b", "-Z"], false),
            "postgres: invalid command-line argument: -Z"
        );
        assert_eq!(
            run(&["junk", "-W", "0"], false),
            "postgres: invalid command-line argument: junk"
        );
        assert_eq!(run(&["a", "-W", "0", "b"], true), "postgres: invalid command-line argument: b");
        assert_eq!(
            run(&["a", "--", "b", "-W", "0"], true),
            "postgres: invalid command-line argument: b"
        );
    }
}
