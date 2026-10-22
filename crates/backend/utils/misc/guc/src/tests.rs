use std::cell::RefCell;
use std::sync::Once;

use types_core::BOOTSTRAP_SUPERUSERID;
use types_error::ErrorLevel;
use types_guc::*;

use crate::*;

thread_local! {
    static SENT: RefCell<Vec<(u8, Vec<u8>)>> = const { RefCell::new(Vec::new()) };
}

// Emit-log-hook capture; the hook is per thread, but EMITTED is
// process-global, so every test that installs capture_emitted holds
// EMITTED_LOCK for its whole capture window (two tests overlapping on
// parallel test threads steal each other's entries).
static EMITTED: std::sync::Mutex<Vec<types_error::PgError>> = std::sync::Mutex::new(Vec::new());
static EMITTED_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

// A role pg_parameter_aclcheck_set denies (see setup).
const DENIED_ROLE: types_core::Oid = 0xB168;

fn capture_emitted(error: &types_error::PgError, _output_to_server: &mut bool) {
    EMITTED.lock().unwrap_or_else(|e| e.into_inner()).push(error.clone());
}

// application_name's value backing (guc_tables::backing) is process-global;
// tests that read or write it must not overlap across test threads.
static APPLICATION_NAME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn test_parse_bool(value: &str) -> Option<bool> {
    let lower = value.to_ascii_lowercase();
    if lower.is_empty() {
        return None;
    }
    for (word, result) in [("true", true), ("false", false), ("yes", true), ("no", false)] {
        if word.starts_with(&lower) {
            return Some(result);
        }
    }
    match lower.as_str() {
        "on" => Some(true),
        "off" | "of" => Some(false),
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    }
}

fn setup() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        guc_tables::init_seams();
        elog::init_seams();
        crate::init_seams();
        xact_seams::is_in_parallel_mode::set(|| false);
        scalar_seams::parse_bool::set(test_parse_bool);
        aclchk_seams::pg_parameter_aclcheck_set::set(|_, role| Ok(role != DENIED_ROLE));
        mbutils_seams::get_database_encoding::set(|| 6);
        mbutils_seams::pg_server_to_client::set(|mcx, s| {
            if !s.starts_with(b"conv:") {
                return Ok(None);
            }
            let mut out = mcx::vec_with_capacity_in(mcx, s.len()).expect("test alloc");
            out.extend(s.iter().map(u8::to_ascii_uppercase));
            Ok(Some(out))
        });
        pqcomm_seams::pq_putmessage::set(|msgtype, body| {
            SENT.with(|s| s.borrow_mut().push((msgtype, body.to_vec())));
            Ok(0)
        });
        timestamp_seams::get_current_timestamp::set(|| 42);
        conffiles_seams::absolute_config_location::set(|location, calling_file| {
            let p = std::path::Path::new(&location);
            if p.is_absolute() {
                p.to_path_buf()
            } else if let Some(calling) = calling_file {
                calling.parent().unwrap_or(std::path::Path::new(".")).join(p)
            } else if let Some(dd) = init_small::globals::DataDir() {
                // C AbsoluteConfigLocation: a relative name with no calling
                // file resolves against DataDir (how postgresql.auto.conf is
                // found).
                std::path::Path::new(&dd).join(p)
            } else {
                p.to_path_buf()
            }
        });
        conffiles_seams::get_conf_files_in_dir::set(|_, _, _| {
            Ok(conffiles_seams::ConfFilesInDir::default())
        });
    });
    initialize_guc_options().unwrap();
}

fn set_session(name: &str, value: Option<&str>) -> PgResult<i32> {
    set_config_option_ext(
        name,
        value,
        PGC_USERSET,
        PGC_S_SESSION,
        BOOTSTRAP_SUPERUSERID,
        GUC_ACTION_SET,
        true,
        ErrorLevel(0),
        false,
    )
}

fn show(name: &str) -> Option<String> {
    with_store(|reg| get_config_option_by_name(reg, name, true).unwrap()).unwrap()
}

#[test]
fn boot_defaults_seeded() {
    setup();
    let _guard = APPLICATION_NAME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(get_int("work_mem"), Some(4096));
    assert_eq!(get_bool("enable_seqscan"), Some(true));
    assert_eq!(get_real("cursor_tuple_fraction"), Some(0.1));
    assert_eq!(get_string("application_name"), Some(Some(String::new())));
    assert_eq!(show("bytea_output"), Some("hex".to_string()));
}

#[test]
fn set_with_units_and_show() {
    setup();
    assert_eq!(set_session("work_mem", Some("8MB")).unwrap(), 1);
    assert_eq!(get_int("work_mem"), Some(8192));
    assert_eq!(show("work_mem"), Some("8MB".to_string()));
    assert_eq!(set_session("work_mem", Some("30720")).unwrap(), 1);
    assert_eq!(show("work_mem"), Some("30MB".to_string()));
}

#[test]
fn invalid_values_error_at_session_source() {
    setup();
    let e = set_session("work_mem", Some("banana")).unwrap_err();
    assert!(e.message().contains("invalid value for parameter \"work_mem\""));

    let e = set_session("work_mem", Some("1XB")).unwrap_err();
    assert_eq!(e.hint(), Some(MEMORY_UNITS_HINT));

    let e = set_session("work_mem", Some("1")).unwrap_err();
    assert!(e.message().contains("outside the valid range"), "{}", e.message());

    let e = set_session("statement_timeout", Some("5banana")).unwrap_err();
    assert_eq!(e.hint(), Some(TIME_UNITS_HINT));

    let e = set_session("enable_seqscan", Some("maybe")).unwrap_err();
    assert!(e.message().contains("requires a Boolean value"));
}

#[test]
fn file_source_rejection_returns_zero() {
    setup();
    let rc = set_config_option_ext(
        "work_mem",
        Some("banana"),
        PGC_SIGHUP,
        PGC_S_FILE,
        BOOTSTRAP_SUPERUSERID,
        GUC_ACTION_SET,
        true,
        ErrorLevel(0),
        false,
    )
    .unwrap();
    assert_eq!(rc, 0);
}

/// Per-role/db settings (pg_db_role_setting -> ProcessGUCArray, sources
/// PGC_S_USER / PGC_S_DATABASE[_USER] / PGC_S_GLOBAL) resolve elevel 0 to
/// WARNING (C set_config_option): a bad stored value is SKIPPED (rc 0),
/// never an Err — an Err at login would refuse every new connection where
/// C connects with a WARNING (`ALTER ROLE x SET role = <dropped role>`
/// locked pgrust sessions out in the round-11 role-DDL soak).
#[test]
fn user_source_rejection_skips_not_errors() {
    setup();
    for source in [PGC_S_USER, PGC_S_DATABASE, PGC_S_DATABASE_USER, PGC_S_GLOBAL] {
        let rc = set_config_option_ext(
            "work_mem",
            Some("banana"),
            PGC_SUSET,
            source,
            BOOTSTRAP_SUPERUSERID,
            GUC_ACTION_SET,
            true,
            ErrorLevel(0),
            false,
        )
        .unwrap();
        assert_eq!(rc, 0, "source {source:?} must skip, not error");
    }
}

/// The report emitted for a demoted rejection carries the DEMOTED level:
/// C's set_config_option ereport()s at elevel, so the client sees 'N'
/// (WARNING), not 'E' — an 'E' during connection startup makes libpq
/// abort the whole connection.
#[test]
fn demoted_rejection_reports_at_demoted_level() {
    let e = types_error::PgError::new(types_error::ERROR, "role \"gone\" does not exist");
    let demoted = crate::registry::demoted_for_report(e, types_error::WARNING);
    assert_eq!(demoted.level, types_error::WARNING);
    assert_eq!(demoted.message, "role \"gone\" does not exist");
}

#[test]
fn postmaster_param_cannot_change_at_runtime() {
    setup();
    let e = set_session("shared_buffers", Some("1000")).unwrap_err();
    assert!(e.message().contains("cannot be changed without restarting the server"));
    let e = set_session("wal_level", Some("logical")).unwrap_err();
    assert!(e.message().contains("cannot be changed without restarting the server"));
}

#[test]
fn sighup_reread_of_postmaster_param() {
    setup();
    let same = set_config_option_ext(
        "shared_buffers",
        Some("16384"),
        PGC_SIGHUP,
        PGC_S_FILE,
        BOOTSTRAP_SUPERUSERID,
        GUC_ACTION_SET,
        true,
        ErrorLevel(0),
        false,
    )
    .unwrap();
    assert_eq!(same, -1);

    let changed = set_config_option_ext(
        "shared_buffers",
        Some("32768"),
        PGC_SIGHUP,
        PGC_S_FILE,
        BOOTSTRAP_SUPERUSERID,
        GUC_ACTION_SET,
        true,
        ErrorLevel(0),
        false,
    )
    .unwrap();
    assert_eq!(changed, 0);
    let status = with_store(|reg| reg.find_option("shared_buffers").unwrap().gen().status).unwrap();
    assert!(status & crate::model::GUC_PENDING_RESTART != 0);
}

#[test]
fn higher_source_wins_and_seeds_reset_default() {
    setup();
    assert_eq!(set_session("work_mem", Some("8192")).unwrap(), 1);
    let rc = set_config_option_ext(
        "work_mem",
        Some("2048"),
        PGC_SIGHUP,
        PGC_S_FILE,
        BOOTSTRAP_SUPERUSERID,
        GUC_ACTION_SET,
        true,
        ErrorLevel(0),
        false,
    )
    .unwrap();
    assert_eq!(rc, -1);
    assert_eq!(get_int("work_mem"), Some(8192));
    assert_eq!(GetConfigOptionResetString("work_mem"), Some("2048".to_string()));
    assert_eq!(set_session("work_mem", None).unwrap(), 1);
    assert_eq!(get_int("work_mem"), Some(2048));
}

#[test]
fn transaction_abort_restores_prior_value() {
    setup();
    AtStart_GUC();
    assert_eq!(set_session("work_mem", Some("8192")).unwrap(), 1);
    AtEOXact_GUC(false, 1);
    assert_eq!(get_int("work_mem"), Some(4096));
}

#[test]
fn transaction_commit_keeps_set_value() {
    setup();
    AtStart_GUC();
    assert_eq!(set_session("work_mem", Some("8192")).unwrap(), 1);
    AtEOXact_GUC(true, 1);
    assert_eq!(get_int("work_mem"), Some(8192));
}

#[test]
fn set_local_reverts_on_commit() {
    setup();
    AtStart_GUC();
    let rc = set_config_option_ext(
        "work_mem",
        Some("8192"),
        PGC_USERSET,
        PGC_S_SESSION,
        BOOTSTRAP_SUPERUSERID,
        GUC_ACTION_LOCAL,
        true,
        ErrorLevel(0),
        false,
    )
    .unwrap();
    assert_eq!(rc, 1);
    assert_eq!(get_int("work_mem"), Some(8192));
    AtEOXact_GUC(true, 1);
    assert_eq!(get_int("work_mem"), Some(4096));
}

#[test]
fn set_then_set_local_commit_restores_set_value() {
    setup();
    AtStart_GUC();
    assert_eq!(set_session("work_mem", Some("8192")).unwrap(), 1);
    let rc = set_config_option_ext(
        "work_mem",
        Some("16384"),
        PGC_USERSET,
        PGC_S_SESSION,
        BOOTSTRAP_SUPERUSERID,
        GUC_ACTION_LOCAL,
        true,
        ErrorLevel(0),
        false,
    )
    .unwrap();
    assert_eq!(rc, 1);
    assert_eq!(get_int("work_mem"), Some(16384));
    AtEOXact_GUC(true, 1);
    assert_eq!(get_int("work_mem"), Some(8192));
}

#[test]
fn save_scope_pops_at_function_exit() {
    setup();
    AtStart_GUC();
    let nest = NewGUCNestLevel();
    assert_eq!(nest, 2);
    let rc = set_config_option_ext(
        "work_mem",
        Some("8192"),
        PGC_USERSET,
        PGC_S_SESSION,
        BOOTSTRAP_SUPERUSERID,
        GUC_ACTION_SAVE,
        true,
        ErrorLevel(0),
        false,
    )
    .unwrap();
    assert_eq!(rc, 1);
    AtEOXact_GUC(true, nest);
    assert_eq!(get_int("work_mem"), Some(4096));
    AtEOXact_GUC(true, 1);
}

#[test]
fn subtransaction_abort_restores_within_transaction() {
    setup();
    AtStart_GUC();
    assert_eq!(set_session("work_mem", Some("8192")).unwrap(), 1);
    let sub = NewGUCNestLevel();
    assert_eq!(set_session("work_mem", Some("16384")).unwrap(), 1);
    AtEOXact_GUC(false, sub);
    assert_eq!(get_int("work_mem"), Some(8192));
    AtEOXact_GUC(true, 1);
    assert_eq!(get_int("work_mem"), Some(8192));
}

#[test]
fn at_eoxact_without_store_is_noop() {
    AtStart_GUC();
    AtEOXact_GUC(true, 1);
}

#[test]
fn report_list_is_o_changed() {
    setup();
    let _guard = APPLICATION_NAME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    elog::config::set_where_to_send_output(types_dest::CommandDest::Remote);
    begin_reporting_guc_options();
    let initial = SENT.with(|s| std::mem::take(&mut *s.borrow_mut()));
    assert!(initial
        .iter()
        .any(|(t, body)| *t == b'S' && body.starts_with(b"application_name\0")));

    report_changed_guc_options();
    assert_eq!(SENT.with(|s| s.borrow().len()), 0);

    assert_eq!(set_session("application_name", Some("psql")).unwrap(), 1);
    report_changed_guc_options();
    let frames = SENT.with(|s| std::mem::take(&mut *s.borrow_mut()));
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].1, b"application_name\0psql\0".to_vec());

    report_changed_guc_options();
    assert_eq!(SENT.with(|s| s.borrow().len()), 0);

    assert_eq!(set_session("application_name", Some("psql")).unwrap(), 1);
    report_changed_guc_options();
    assert_eq!(SENT.with(|s| s.borrow().len()), 0);
}

#[test]
fn report_order_follows_guc_report_list() {
    setup();
    let _guard = APPLICATION_NAME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    elog::config::set_where_to_send_output(types_dest::CommandDest::Remote);
    begin_reporting_guc_options();
    SENT.with(|s| s.borrow_mut().clear());
    let names = || -> Vec<String> {
        SENT.with(|s| std::mem::take(&mut *s.borrow_mut()))
            .iter()
            .filter(|(t, _)| *t == b'S')
            .map(|(_, body)| String::from_utf8_lossy(body.split(|b| *b == 0).next().unwrap()).into_owned())
            .collect()
    };

    // guc.c:4272 head-pushes: the newest change in a command reports first.
    AtStart_GUC();
    assert_eq!(set_session("application_name", Some("b227")).unwrap(), 1);
    assert_eq!(set_session("IntervalStyle", Some("sql_standard")).unwrap(), 1);
    report_changed_guc_options();
    assert_eq!(names(), ["IntervalStyle", "application_name"]);

    // guc.c:2276 walks guc_stack_list newest-first and head-pushes each
    // restore: the first variable stacked reports first at abort.
    AtEOXact_GUC(false, 1);
    report_changed_guc_options();
    assert_eq!(names(), ["application_name", "IntervalStyle"]);

    // ResetAllOptions (guc.c:2008) walks guc_nondef_list oldest-first and
    // head-pushes: the most recently changed variable reports first.
    AtStart_GUC();
    assert_eq!(set_session("IntervalStyle", Some("iso_8601")).unwrap(), 1);
    assert_eq!(set_session("application_name", Some("b227")).unwrap(), 1);
    report_changed_guc_options();
    names();
    ResetAllOptions();
    report_changed_guc_options();
    assert_eq!(names(), ["application_name", "IntervalStyle"]);
    AtEOXact_GUC(true, 1);
}

#[test]
fn explain_guc_options_follow_nondef_order() {
    setup();
    let explain_names = || -> Vec<String> {
        with_store(|reg| {
            crate::registry::get_explain_guc_options(reg, &mut |_| Ok(true))
                .unwrap()
                .into_iter()
                .map(|(name, _)| name.to_string())
                .collect()
        })
        .unwrap()
    };
    AtStart_GUC();
    assert_eq!(set_session("work_mem", Some("65536")).unwrap(), 1);
    assert_eq!(set_session("enable_seqscan", Some("off")).unwrap(), 1);
    assert_eq!(explain_names(), ["work_mem", "enable_seqscan"]);
    AtEOXact_GUC(false, 1);
    assert_eq!(explain_names(), Vec::<&str>::new());

    AtStart_GUC();
    assert_eq!(set_session("enable_seqscan", Some("off")).unwrap(), 1);
    assert_eq!(set_session("work_mem", Some("65536")).unwrap(), 1);
    assert_eq!(explain_names(), ["enable_seqscan", "work_mem"]);
    AtEOXact_GUC(false, 1);
}

#[test]
fn reset_and_reset_all() {
    setup();
    let _guard = APPLICATION_NAME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(set_session("work_mem", Some("8192")).unwrap(), 1);
    assert_eq!(set_session("application_name", Some("x")).unwrap(), 1);
    assert_eq!(set_session("work_mem", None).unwrap(), 1);
    assert_eq!(get_int("work_mem"), Some(4096));

    assert_eq!(set_session("work_mem", Some("8192")).unwrap(), 1);
    ResetAllOptions();
    assert_eq!(get_int("work_mem"), Some(4096));
    assert_eq!(get_string("application_name"), Some(Some(String::new())));
}

#[test]
fn custom_placeholder_variables() {
    setup();
    assert_eq!(set_session("my.custom", Some("hello")).unwrap(), 1);
    assert_eq!(show("my.custom"), Some("hello".to_string()));

    let e = set_session("my..bad", Some("x")).unwrap_err();
    assert!(e.message().contains("invalid configuration parameter name"));

    let e = set_session("no_such_parameter", Some("x")).unwrap_err();
    assert!(e.message().contains("unrecognized configuration parameter"));
}

// DefineCustomStringVariable (guc.c:5224 / define_custom_variable
// guc.c:4937): a placeholder set before the library loaded is replaced and
// its committed value and transactional (SET LOCAL) value re-applied in
// order; a fresh definition SHOWs its NULL boot value as ''; redefining a
// real parameter is an internal error; MarkGUCPrefixReserved afterwards
// refuses new names under the prefix but keeps the defined ones settable.
#[test]
fn define_custom_string_variable_adopts_placeholder_values() {
    setup();
    AtStart_GUC();
    assert_eq!(set_session("b021ext.app_name", Some("early")).unwrap(), 1);
    let rc = set_config_option_ext(
        "b021ext.app_name",
        Some("local1"),
        PGC_USERSET,
        PGC_S_SESSION,
        BOOTSTRAP_SUPERUSERID,
        GUC_ACTION_LOCAL,
        true,
        ErrorLevel(0),
        false,
    )
    .unwrap();
    assert_eq!(rc, 1);
    DefineCustomStringVariable("b021ext.app_name", Some("desc"), None, None, PGC_USERSET, 0)
        .unwrap();
    assert_eq!(show("b021ext.app_name"), Some("local1".to_string()));
    let flags = with_store(|reg| get_config_option_flags(reg, "b021ext.app_name", false).unwrap())
        .unwrap();
    assert_eq!(flags & GUC_CUSTOM_PLACEHOLDER, 0);
    AtEOXact_GUC(true, 1);
    assert_eq!(show("b021ext.app_name"), Some("early".to_string()));

    DefineCustomStringVariable("b021ext.other", Some("desc"), None, None, PGC_USERSET, 0).unwrap();
    assert_eq!(show("b021ext.other"), Some(String::new()));
    let e = DefineCustomStringVariable("b021ext.other", Some("desc"), None, None, PGC_USERSET, 0)
        .unwrap_err();
    assert_eq!(e.message(), "attempt to redefine parameter \"b021ext.other\"");

    MarkGUCPrefixReserved("b021ext");
    let e = set_session("b021ext.nope", Some("x")).unwrap_err();
    assert!(e.message().contains("invalid configuration parameter name"));
    assert_eq!(e.detail(), Some("\"b021ext\" is a reserved prefix."));
    assert_eq!(set_session("b021ext.app_name", Some("later")).unwrap(), 1);
    assert_eq!(show("b021ext.app_name"), Some("later".to_string()));
}

// get_config_option_at reads what GetConfigOption(name, true, false) reads,
// through the caller's remembered slot: a placeholder absent, appended by a
// SET, given a new value, RESET, removed by a reserved prefix (a tombstone),
// a defined variable under its old name, and a reinstalled registry.
#[test]
fn get_config_option_at_reads_what_get_config_option_reads() {
    setup();
    let same = |slot: &std::cell::Cell<OptionSlot>, name: &str| {
        let at = get_config_option_at(slot, name).expect("this thread has a store");
        assert_eq!(at, GetConfigOption(name, true, false).unwrap(), "{name}");
        at
    };
    let knob = std::cell::Cell::new(OptionSlot::Unknown);
    assert_eq!(same(&knob, "d2896x.knob"), None);
    assert!(matches!(knob.get(), OptionSlot::Missing { .. }));
    assert_eq!(same(&knob, "d2896x.knob"), None, "the registry did not grow: still absent");
    assert_eq!(set_session("d2896x.other", Some("1")).unwrap(), 1);
    assert_eq!(same(&knob, "d2896x.knob"), None, "another name appended: looked up again, still absent");
    assert_eq!(set_session("d2896x.knob", Some("7")).unwrap(), 1);
    assert_eq!(same(&knob, "d2896x.knob"), Some("7".to_string()), "a SET appends the placeholder");
    assert!(matches!(knob.get(), OptionSlot::Found { .. }));
    assert_eq!(set_session("d2896x.knob", Some(" 8 ")).unwrap(), 1);
    assert_eq!(same(&knob, "d2896x.knob"), Some(" 8 ".to_string()));
    assert_eq!(set_session("d2896x.knob", None).unwrap(), 1);
    same(&knob, "d2896x.knob");

    let wm = std::cell::Cell::new(OptionSlot::Unknown);
    assert_eq!(set_session("work_mem", Some("8192")).unwrap(), 1);
    assert_eq!(same(&wm, "sort_mem"), Some("8192".to_string()), "an old name reads its new variable");
    assert_eq!(set_session("work_mem", Some("4096")).unwrap(), 1);
    assert_eq!(same(&wm, "sort_mem"), Some("4096".to_string()));

    MarkGUCPrefixReserved("d2896x");
    assert_eq!(same(&knob, "d2896x.knob"), None, "a reserved prefix leaves the placeholder a tombstone");
    assert!(matches!(knob.get(), OptionSlot::Missing { .. }));

    let k2 = std::cell::Cell::new(OptionSlot::Unknown);
    assert_eq!(set_session("d2896y.k", Some("1")).unwrap(), 1);
    assert_eq!(same(&k2, "d2896y.k"), Some("1".to_string()));
    initialize_guc_options().unwrap();
    assert_eq!(same(&k2, "d2896y.k"), None, "a new registry: the remembered index is not read");
    assert_eq!(same(&wm, "sort_mem"), GetConfigOption("work_mem", true, false).unwrap());
}

// knob_read is Knob::read of what GetConfigOption(name, true, false) reads, through the kept reading: absent, a SET
// (appends the placeholder), a new value, a value that reads as none of the words, a RESET, another option's SET, and a
// reinstalled registry. Each knob's own words: the width knobs untrimmed, the CTE knob trimmed, both without case.
#[test]
fn knob_read_reads_what_get_config_option_reads() {
    use guc_seams::{Knob, KnobRead};
    setup();
    let same = |knob: Knob| {
        let read = crate::knob::knob_read(knob);
        assert_eq!(read, knob.read(GetConfigOption(knob.name(), true, false).unwrap().as_deref()), "{knob:?}");
        assert_eq!(read, crate::knob::knob_read(knob), "{knob:?}: the kept reading");
        read
    };
    let (w, t, c) = (Knob::GravityWidthCosting, Knob::GravityWidthCostingTrace, Knob::GravityDecorrelateCtes);
    initialize_guc_options().unwrap();
    assert_eq!(same(w), KnobRead::Unset);
    assert_eq!(same(c), KnobRead::Unset);
    assert_eq!(set_session(w.name(), Some("On")).unwrap(), 1);
    assert_eq!(same(w), KnobRead::Word(0), "a SET appends the placeholder; read without case");
    assert_eq!(same(t), KnobRead::Unset, "another knob stays absent");
    assert_eq!(set_session(w.name(), Some(" yes")).unwrap(), 1);
    assert_eq!(same(w), KnobRead::Other, "the width knobs are not trimmed");
    assert_eq!(set_session(w.name(), Some("1")).unwrap(), 1);
    assert_eq!(same(w), KnobRead::Word(3));
    assert_eq!(set_session(c.name(), Some(" ALL ")).unwrap(), 1);
    assert_eq!(same(c), KnobRead::Word(6), "the CTE knob is trimmed");
    assert_eq!(same(w), KnobRead::Word(3), "another option's SET leaves the reading");
    assert_eq!(set_session(c.name(), Some("materialized")).unwrap(), 1);
    assert_eq!(same(c), KnobRead::Word(7));
    assert_eq!(set_session(c.name(), Some("sometimes")).unwrap(), 1);
    assert_eq!(same(c), KnobRead::Other);
    assert_eq!(set_session(c.name(), None).unwrap(), 1);
    same(c); // RESET: the placeholder's reset value, whatever it reads as
    let f = Knob::GravityDecorrelateOrFactor;
    assert_eq!(same(f), KnobRead::Unset);
    assert_eq!(set_session(f.name(), Some(" OFF")).unwrap(), 1);
    assert_eq!(same(f), KnobRead::Word(0), "the OR-factoring knob is trimmed");
    assert_eq!(set_session(f.name(), Some("yes")).unwrap(), 1);
    assert_eq!(same(f), KnobRead::Word(7));
    let r = Knob::GravityUnnestReach;
    assert_eq!(same(r), KnobRead::Unset);
    assert_eq!(set_session(r.name(), Some(" P2")).unwrap(), 1);
    assert_eq!(same(r), KnobRead::Word(1), "the unnest reach knob is trimmed");
    assert_eq!(set_session(r.name(), Some("wide")).unwrap(), 1);
    assert_eq!(same(r), KnobRead::Word(2));
    let a = Knob::GravityUnnestArgs;
    assert_eq!(same(a), KnobRead::Unset);
    assert_eq!(set_session(a.name(), Some("no")).unwrap(), 1);
    assert_eq!(same(a), KnobRead::Word(3), "the UN-P1 knob reads like the OR-factoring knob");
    assert_eq!(same(f), KnobRead::Word(7), "another knob's SET leaves the reading");
    assert_eq!(set_session(w.name(), Some("off")).unwrap(), 1);
    assert_eq!(same(w), KnobRead::Other);
    initialize_guc_options().unwrap();
    assert_eq!(same(w), KnobRead::Unset, "a new registry: the kept reading is not read");
    assert_eq!(same(c), KnobRead::Unset);
}

#[test]
fn old_guc_names_map() {
    setup();
    assert_eq!(set_session("sort_mem", Some("8192")).unwrap(), 1);
    assert_eq!(get_int("work_mem"), Some(8192));
}

#[test]
fn case_insensitive_lookup() {
    setup();
    assert_eq!(set_session("WORK_MEM", Some("8192")).unwrap(), 1);
    assert_eq!(get_int("work_mem"), Some(8192));
}

#[test]
fn guc_is_name_truncates_long_values() {
    setup();
    let _guard = APPLICATION_NAME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let long = "a".repeat(100);
    assert_eq!(set_session("application_name", Some(&long)).unwrap(), 1);
    assert_eq!(get_string("application_name"), Some(Some("a".repeat(63))));
}

#[test]
fn enum_values_and_hint() {
    setup();
    assert_eq!(set_session("bytea_output", Some("escape")).unwrap(), 1);
    assert_eq!(get_enum("bytea_output"), Some(0));
    let e = set_session("bytea_output", Some("wat")).unwrap_err();
    assert!(e.hint().unwrap().contains("escape, hex"));
}

#[test]
fn name_compare_and_hash() {
    use core::cmp::Ordering;
    assert_eq!(guc_name_compare("work_mem", "WORK_MEM"), Ordering::Equal);
    assert_eq!(guc_name_compare("a", "ab"), Ordering::Less);
    assert_eq!(guc_name_compare("ab", "a"), Ordering::Greater);
    assert_eq!(guc_name_hash("Work_Mem"), guc_name_hash("work_mem"));
    assert_eq!(convert_guc_name_for_parameter_acl("Sort_Mem"), "work_mem");
}

#[test]
fn parse_int_units() {
    match parse_int("1GB", GUC_UNIT_KB) {
        ParseNum::Ok(v) => assert_eq!(v, 1048576),
        _ => panic!(),
    }
    match parse_int("30s", GUC_UNIT_MS) {
        ParseNum::Ok(v) => assert_eq!(v, 30000),
        _ => panic!(),
    }
    match parse_int("0x10", 0) {
        ParseNum::Ok(v) => assert_eq!(v, 16),
        _ => panic!(),
    }
    match parse_int("10000000000", 0) {
        ParseNum::Err { hint } => assert_eq!(hint, Some("Value exceeds integer range.")),
        _ => panic!(),
    }
    match parse_int("100 MB", GUC_UNIT_KB) {
        ParseNum::Ok(v) => assert_eq!(v, 102400),
        _ => panic!(),
    }
}

#[test]
fn fmt_g_matches_c_printf() {
    assert_eq!(fmt_g(0.0), "0");
    assert_eq!(fmt_g(1.5), "1.5");
    assert_eq!(fmt_g(100.0), "100");
    assert_eq!(fmt_g(1.23456789), "1.23457");
    assert_eq!(fmt_g(1234567.0), "1.23457e+06");
    assert_eq!(fmt_g(0.0001), "0.0001");
    assert_eq!(fmt_g(0.00001), "1e-05");
    assert_eq!(fmt_e(1.5, 2), "1.50e+00");
    assert_eq!(fmt_e(1234.0, 2), "1.23e+03");
}

#[test]
fn parse_long_option_splits_and_underscores() {
    assert_eq!(
        ParseLongOption("some-option=some value"),
        ("some_option".to_string(), Some("some value".to_string()))
    );
    assert_eq!(ParseLongOption("flag-only"), ("flag_only".to_string(), None));
}

#[test]
fn valid_custom_names() {
    assert!(valid_custom_variable_name("foo.bar"));
    assert!(valid_custom_variable_name("foo.bar.baz"));
    assert!(valid_custom_variable_name("foo._bar$2"));
    assert!(!valid_custom_variable_name("foo"));
    assert!(!valid_custom_variable_name("foo."));
    assert!(!valid_custom_variable_name(".bar"));
    assert!(!valid_custom_variable_name("foo..bar"));
    assert!(!valid_custom_variable_name("1foo.bar"));
}

/// A test's scratch directory under the temp dir, removed on drop (the
/// test's end, or a panic's unwind).
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl std::ops::Deref for Scratch {
    type Target = std::path::Path;
    fn deref(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn scratch_dirs_leave_nothing_behind() {
    let dir = Scratch::new("guc_gone_test");
    std::fs::write(dir.join("postgresql.conf"), "work_mem = 2MB\n").unwrap();
    let path = dir.0.clone();
    drop(dir);
    assert!(!path.exists(), "{} left behind", path.display());
}

// guc.c:1613: InitializeGUCOptionsFromEnvironment re-applies the rlimit
// default on every reload, so dropping max_stack_depth from the file lands
// on the platform default (2MB/DYNAMIC_DEFAULT), not the 100kB boot value.
#[test]
fn reload_restores_rlimit_stack_depth_default() {
    setup();
    let rlimit = stack_depth_core::get_stack_depth_rlimit();
    let new_limit = rlimit.saturating_sub(stack_depth_core::STACK_DEPTH_SLOP) / 1024;
    if rlimit <= 0 || new_limit <= 100 {
        return;
    }
    let _guard = APPLICATION_NAME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = Scratch::new("guc_msd_test");
    let conf = dir.join("postgresql.conf");
    init_small::globals::SetDataDir(dir.to_str().unwrap());
    SetConfigOption("config_file", Some(conf.to_str().unwrap()), PGC_POSTMASTER, PGC_S_OVERRIDE)
        .unwrap();
    let source_of = || {
        with_store(|reg| reg.find_option("max_stack_depth").unwrap().gen().source).unwrap()
    };

    std::fs::write(&conf, "max_stack_depth = '1MB'\n").unwrap();
    crate::process_config::process_config_file_internal(PGC_SIGHUP, true, types_error::LOG)
        .unwrap();
    assert_eq!((get_int("max_stack_depth"), source_of()), (Some(1024), PGC_S_FILE));

    std::fs::write(&conf, "").unwrap();
    crate::process_config::process_config_file_internal(PGC_SIGHUP, true, types_error::LOG)
        .unwrap();
    let (expected, source) = crate::store::boot_limit_and_source(new_limit);
    assert_eq!((get_int("max_stack_depth"), source_of()), (Some(expected), source));
}

// guc.c:1873: a set-but-empty data_directory / hba_file is not "unset"; it
// goes through make_absolute_path and resolves to the working directory.
#[test]
fn select_config_files_empty_paths_resolve_to_cwd() {
    setup();
    let _guard = APPLICATION_NAME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = Scratch::new("guc_scf_test");
    std::fs::write(dir.join("postgresql.conf"), "data_directory = ''\nhba_file = ''\n").unwrap();

    assert!(crate::select::SelectConfigFiles(Some(dir.to_str().unwrap()), "postgres").unwrap());
    let cwd = std::env::current_dir().unwrap().to_str().unwrap().to_string();
    assert_eq!(get_string("data_directory"), Some(Some(cwd.clone())));
    assert_eq!(get_string("hba_file"), Some(Some(cwd)));
    assert_eq!(
        get_string("ident_file"),
        Some(Some(dir.join("pg_ident.conf").to_str().unwrap().to_string()))
    );
}

#[test]
fn process_config_file_applies_and_reverts() {
    setup();
    let _guard = APPLICATION_NAME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = Scratch::new("guc_pcf_test");
    let conf = dir.join("postgresql.conf");
    init_small::globals::SetDataDir(dir.to_str().unwrap());

    std::fs::write(&conf, "work_mem = 2MB\nwork_mem = 8MB\napplication_name = 'from_file'\nnot.known = 'kept'\n").unwrap();
    SetConfigOption("config_file", Some(conf.to_str().unwrap()), PGC_POSTMASTER, PGC_S_OVERRIDE)
        .unwrap();

    let clean =
        crate::process_config::process_config_file_internal(PGC_SIGHUP, true, types_error::LOG)
            .unwrap();
    assert!(clean);
    assert_eq!(get_int("work_mem"), Some(8192));
    assert_eq!(get_string("application_name"), Some(Some("from_file".to_string())));
    assert_eq!(show("not.known"), Some("kept".to_string()));
    assert_eq!(pg_reload_time(), 42);
    let (source, sourcefile) = with_store(|reg| {
        let gen = reg.find_option("work_mem").unwrap().gen();
        (gen.source, gen.sourcefile.clone())
    })
    .unwrap();
    assert_eq!(source, PGC_S_FILE);
    assert_eq!(sourcefile.as_deref(), conf.to_str());

    // Removal from the file reverts to the boot default on reload.
    std::fs::write(&conf, "application_name = 'from_file'\n").unwrap();
    let clean =
        crate::process_config::process_config_file_internal(PGC_SIGHUP, true, types_error::LOG)
            .unwrap();
    assert!(clean);
    assert_eq!(get_int("work_mem"), Some(4096));
    assert_eq!(with_store(|reg| reg.find_option("work_mem").unwrap().gen().source).unwrap(), PGC_S_DEFAULT);

    // An unknown non-custom name is a recorded error; settings are not applied.
    std::fs::write(&conf, "no_such_thing = 1\nwork_mem = 3MB\n").unwrap();
    let clean =
        crate::process_config::process_config_file_internal(PGC_SIGHUP, true, types_error::LOG)
            .unwrap();
    assert!(!clean);
    assert_eq!(get_int("work_mem"), Some(4096));
}

// guc.c:459-476: a PGC_POSTMASTER parameter that came from the file and is
// no longer in it is reported AND recorded as an error item (name NULL,
// no file/line, ignore) so pg_file_settings shows the removal. Audit
// a186-candidate-fp-misc-guc-p1-e735f2236e8cc368e9e6-1.
#[test]
fn process_config_file_removed_postmaster_param_records_error_item() {
    setup();
    let _guard = APPLICATION_NAME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = Scratch::new("guc_pcf_removed");
    let conf = dir.join("postgresql.conf");
    init_small::globals::SetDataDir(dir.to_str().unwrap());

    // Initial load (postmaster context): port comes from the file.
    std::fs::write(&conf, "port = 5499\n").unwrap();
    SetConfigOption("config_file", Some(conf.to_str().unwrap()), PGC_POSTMASTER, PGC_S_OVERRIDE)
        .unwrap();
    let clean = crate::process_config::process_config_file_internal(
        PGC_POSTMASTER,
        true,
        types_error::LOG,
    )
    .unwrap();
    assert!(clean);
    assert_eq!(get_int("port"), Some(5499));
    assert_eq!(with_store(|reg| reg.find_option("port").unwrap().gen().reset_source).unwrap(), PGC_S_FILE);

    // Removed from the file: a show_all_file_settings-style scan (SIGHUP,
    // apply_settings=false) must return the error item C records.
    std::fs::write(&conf, "application_name = 'unrelated'\n").unwrap();
    let (clean, items) = crate::process_config::process_config_file_internal_list(
        PGC_SIGHUP,
        false,
        types_error::LOG,
    )
    .unwrap();
    assert!(!clean);
    let err_items: Vec<&guc_file::ConfigVariable> = items.iter().filter(|i| i.errmsg.is_some()).collect();
    assert_eq!(err_items.len(), 1, "items: {items:?}");
    let item = err_items[0];
    assert_eq!(
        item.errmsg.as_deref(),
        Some("parameter \"port\" cannot be changed without restarting the server")
    );
    assert!(item.name.is_none());
    assert!(item.value.is_none());
    assert!(item.filename.is_none());
    assert_eq!(item.sourceline, 0);
    assert!(item.ignore);
    assert!(!item.applied);
    // The error item is appended after the parsed entries (C's tail append).
    assert_eq!(items.last().map(|i| i.errmsg.is_some()), Some(true));
    // Not applied: the value from the file stands.
    assert_eq!(get_int("port"), Some(5499));
}

// gucdup corpus: C's ProcessConfigFileInternal is LAST-wins for duplicate
// entries within one pass (earlier occurrences are marked ignorable), across
// include files, and postgresql.auto.conf — parsed after the main file — must
// override it all. Byte-verified against C 18.3 twin boots by
// scripts/gucdup-repro-e2e.sh.
#[test]
fn process_config_file_duplicate_orderings_last_wins() {
    setup();
    let _guard = APPLICATION_NAME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = Scratch::new("guc_dup_test");
    let conf = dir.join("postgresql.conf");
    let auto_conf = dir.join("postgresql.auto.conf");
    init_small::globals::SetDataDir(dir.to_str().unwrap());
    SetConfigOption("config_file", Some(conf.to_str().unwrap()), PGC_POSTMASTER, PGC_S_OVERRIDE)
        .unwrap();

    let reload = || {
        crate::process_config::process_config_file_internal(PGC_SIGHUP, true, types_error::LOG)
            .unwrap()
    };

    // Inline duplicate: the later entry wins.
    std::fs::write(&conf, "work_mem = 2MB\nwork_mem = 8MB\n").unwrap();
    assert!(reload());
    assert_eq!(get_int("work_mem"), Some(8192));

    // Duplicate via an include placed after the inline entry: include wins.
    std::fs::write(dir.join("extra.conf"), "work_mem = 16MB\n").unwrap();
    std::fs::write(&conf, "work_mem = 2MB\ninclude 'extra.conf'\n").unwrap();
    assert!(reload());
    assert_eq!(get_int("work_mem"), Some(16384));

    // Include first, inline later: the inline entry wins.
    std::fs::write(&conf, "include 'extra.conf'\nwork_mem = 2MB\n").unwrap();
    assert!(reload());
    assert_eq!(get_int("work_mem"), Some(2048));

    // postgresql.auto.conf is parsed after the main file: the ALTER SYSTEM
    // value wins over the main file, even over a later main-file duplicate.
    std::fs::write(&conf, "work_mem = 2MB\nwork_mem = 8MB\n").unwrap();
    std::fs::write(&auto_conf, "work_mem = 32MB\n").unwrap();
    assert!(reload());
    assert_eq!(get_int("work_mem"), Some(32768));

    // Case-variant duplicate: find_option matches case-insensitively, but dup
    // pruning compares exact spellings (C strcmp); both entries survive and
    // apply in file order, so the later spelling still wins.
    std::fs::remove_file(&auto_conf).unwrap();
    std::fs::write(&conf, "work_mem = 2MB\nWORK_MEM = 8MB\n").unwrap();
    assert!(reload());
    assert_eq!(get_int("work_mem"), Some(8192));
}

#[test]
fn seams_route_to_bodies() {
    setup();
    let _guard = APPLICATION_NAME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let level = NewGUCNestLevel();
    AtEOXact_GUC(true, level);
    AtStart_GUC();
    AtEOXact_GUC(true, 1);
    guc_seams::set_config_option_internal_dynamic_default::call("application_name", "seamtest")
        .unwrap();
    assert_eq!(get_string("application_name"), Some(Some("seamtest".to_string())));
}

// GUCArrayAdd/Delete + the secdef proconfig seam (fmgr_security_definer's
// GUC push/pop protocol).
fn array_setup() {
    setup();
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        superuser_seams::superuser::set(|| Ok(true));
    });
    miscinit::SetUserIdAndSecContext(BOOTSTRAP_SUPERUSERID, 0);
}

#[test]
fn guc_array_add_replaces_in_place_and_deletes() {
    array_setup();
    let a = GUCArrayAdd(&[], "work_mem", "64MB").unwrap();
    assert_eq!(a, vec!["work_mem=64MB".to_string()]);
    let a = GUCArrayAdd(&a, "enable_seqscan", "off").unwrap();
    assert_eq!(a.len(), 2);
    let a = GUCArrayAdd(&a, "work_mem", "128MB").unwrap();
    assert_eq!(a, vec!["work_mem=128MB".to_string(), "enable_seqscan=off".to_string()]);
    let a = GUCArrayDelete(&a, "work_mem").unwrap().unwrap();
    assert_eq!(a, vec!["enable_seqscan=off".to_string()]);
    assert!(GUCArrayDelete(&a, "enable_seqscan").unwrap().is_none());
}

#[test]
fn guc_array_add_validates_name_and_value() {
    array_setup();
    let e = GUCArrayAdd(&[], "no_such_setting", "x").unwrap_err();
    assert!(e.message().contains("unrecognized configuration parameter"), "{}", e.message());
    let e = GUCArrayAdd(&[], "work_mem", "banana").unwrap_err();
    assert!(e.message().contains("invalid value for parameter"), "{}", e.message());
}

// guc.c:6745: validate_option_array_item looks the name up with
// skip_errors = skipIfNoPermissions || reset_custom, so an unknown custom
// name under a reserved prefix raises assignable_custom_variable_name's
// 42602 ("... is a reserved prefix.") from GUCArrayAdd, while RESET (value
// NULL, reset_custom) skips the lookup errors and reaches the placeholder
// permission check. Audit a186-candidate-fp-misc-guc-p3-4c87efc31a803236318e-1.
#[test]
fn guc_array_add_reserved_prefix_is_invalid_name() {
    array_setup();
    MarkGUCPrefixReserved("b035rsv");
    let e = GUCArrayAdd(&[], "b035rsv.bogus", "x").unwrap_err();
    assert_eq!(e.sqlstate(), types_error::ERRCODE_INVALID_NAME, "{}", e.message());
    assert_eq!(e.message(), "invalid configuration parameter name \"b035rsv.bogus\"");
    assert_eq!(e.detail(), Some("\"b035rsv\" is a reserved prefix."));
    // Malformed custom names take the same skip_errors=false path (42602).
    let e = GUCArrayAdd(&[], "b035rsv.bad..name", "x").unwrap_err();
    assert_eq!(e.sqlstate(), types_error::ERRCODE_INVALID_NAME, "{}", e.message());
    // RESET of the same unknown reserved-prefix name is allowed (superuser).
    assert!(GUCArrayDelete(&["other.x=1".to_string()], "b035rsv.bogus").unwrap().is_some());
    // A plain unknown name is still 42704.
    let e = GUCArrayAdd(&[], "no_such_setting_b035", "x").unwrap_err();
    assert_eq!(e.sqlstate(), types_error::ERRCODE_UNDEFINED_OBJECT, "{}", e.message());
}

#[test]
fn guc_array_add_rejects_postmaster_guc() {
    array_setup();
    // C validate_option_array_item calls set_config_option with
    // superuser()?PGC_SUSET:PGC_USERSET (guc.c:6780-6782), never the
    // variable's PGC_POSTMASTER context. Unfixed: Ok.
    let e = GUCArrayAdd(&[], "max_connections", "100").unwrap_err();
    assert_eq!(e.sqlstate(), types_error::ERRCODE_CANT_CHANGE_RUNTIME_PARAM);
    assert!(
        e.message().contains("cannot be changed without restarting the server"),
        "{}",
        e.message()
    );
}

#[test]
fn process_guc_array_secdef_pushes_and_nest_pop_restores() {
    array_setup();
    assert_eq!(get_int("work_mem"), Some(4096));
    let nest = NewGUCNestLevel();
    guc_seams::process_guc_array_secdef::call(&["work_mem=64MB".to_string()]).unwrap();
    assert_eq!(get_int("work_mem"), Some(65536));
    AtEOXact_GUC(true, nest);
    assert_eq!(get_int("work_mem"), Some(4096));
}

#[test]
fn session_bind_transfers_leader_state() {
    setup();
    assert_eq!(set_session("cursor_tuple_fraction", Some("0.25")).unwrap(), 1);
    assert_eq!(
        set_config_option_ext(
            "statement_timeout",
            Some("7s"),
            PGC_SIGHUP,
            PGC_S_FILE,
            BOOTSTRAP_SUPERUSERID,
            GUC_ACTION_SET,
            true,
            ErrorLevel(0),
            false,
        )
        .unwrap(),
        1
    );
    let caps = crate::store::capture_session_gucs();
    std::thread::spawn(move || {
        setup();
        assert!(!crate::store::session_bound());
        let binding = crate::store::bind_session_gucs(&caps).unwrap();
        assert!(crate::store::session_bound());
        assert_eq!(get_real("cursor_tuple_fraction"), Some(0.25));
        assert_eq!(get_int("statement_timeout"), Some(7000));
        // Session-sourced bind resets to the boot default; file-sourced bind
        // became the reset value (make_default), exactly as restore would.
        set_config_option_ext(
            "cursor_tuple_fraction",
            None,
            PGC_USERSET,
            PGC_S_SESSION,
            BOOTSTRAP_SUPERUSERID,
            GUC_ACTION_SET,
            true,
            ErrorLevel(0),
            false,
        )
        .unwrap();
        assert_eq!(get_real("cursor_tuple_fraction"), Some(0.1));
        set_config_option_ext(
            "statement_timeout",
            None,
            PGC_USERSET,
            PGC_S_SESSION,
            BOOTSTRAP_SUPERUSERID,
            GUC_ACTION_SET,
            true,
            ErrorLevel(0),
            false,
        )
        .unwrap();
        assert_eq!(get_int("statement_timeout"), Some(7000));
        drop(binding);
        assert!(!crate::store::session_bound());
    })
    .join()
    .unwrap();
}

#[test]
fn session_bind_guard_rejects_double_bind() {
    setup();
    let caps = crate::store::capture_session_gucs();
    std::thread::spawn(move || {
        setup();
        let _binding = crate::store::bind_session_gucs(&caps).unwrap();
        let again = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::store::bind_session_gucs(&caps)
        }));
        assert!(again.is_err(), "second bind on a bound thread must panic");
    })
    .join()
    .unwrap();
}

#[test]
fn session_bind_matches_string_restore_end_state() {
    setup();
    let _guard = APPLICATION_NAME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(set_session("work_mem", Some("8MB")).unwrap(), 1);
    assert_eq!(set_session("application_name", Some("bindcheck")).unwrap(), 1);
    // Restrict both legs to the vars this test set: the unit-test env lacks
    // the owning units of several always-nondefault vars (external enum
    // options slots), which the string-restore leg would have to re-parse.
    let touched = ["work_mem", "application_name"];
    let mut caps = crate::store::capture_session_gucs();
    caps.retain(|c| touched.contains(&c.name()));
    let mut strings = crate::store::capture_nondefault_variables();
    strings.retain(|v| touched.contains(&v.name.as_str()));
    assert_eq!(caps.len(), 2);
    assert_eq!(strings.len(), 2);
    let bound = std::thread::spawn(move || {
        setup();
        let _binding = crate::store::bind_session_gucs(&caps).unwrap();
        with_store(|reg| {
            reg.iter()
                .filter(|v| ["work_mem", "application_name"].contains(&v.name()))
                .map(|v| {
                    (
                        v.name().to_string(),
                        crate::registry::show_guc_option(v, false),
                        v.gen().source,
                        v.gen().scontext,
                    )
                })
                .collect::<Vec<_>>()
        })
        .unwrap()
    })
    .join()
    .unwrap();
    let restored = std::thread::spawn(move || {
        setup();
        crate::store::restore_nondefault_variables(&strings).unwrap();
        with_store(|reg| {
            reg.iter()
                .filter(|v| ["work_mem", "application_name"].contains(&v.name()))
                .map(|v| {
                    (
                        v.name().to_string(),
                        crate::registry::show_guc_option(v, false),
                        v.gen().source,
                        v.gen().scontext,
                    )
                })
                .collect::<Vec<_>>()
        })
        .unwrap()
    })
    .join()
    .unwrap();
    assert_eq!(bound, restored);
}

thread_local! {
    static HAS_PRIVS: Cell<bool> = const { Cell::new(false) };
}

// GetConfigOption(restrict_privileged=true) over a GUC_SUPERUSER_ONLY
// option: C's ConfigOptionIsVisible gate — has_privs_of_role(GetUserId(),
// ROLE_PG_READ_ALL_SETTINGS) — with the exact 42501 error. Pre-fix this
// panicked instead of resolving the privilege check.
#[test]
fn get_config_option_superuser_only_gate() {
    setup();
    static SEAM: Once = Once::new();
    SEAM.call_once(|| {
        acl_seams::has_privs_of_role::set(|_member, role| {
            assert_eq!(role, 3374, "pg_read_all_settings");
            Ok(HAS_PRIVS.get())
        });
    });
    miscinit::SetUserIdAndSecContext(BOOTSTRAP_SUPERUSERID, 0);

    HAS_PRIVS.set(false);
    let err = GetConfigOption("krb_server_keyfile", false, true).unwrap_err();
    assert_eq!(err.sqlstate(), types_error::ERRCODE_INSUFFICIENT_PRIVILEGE);
    assert_eq!(err.message(), "permission denied to examine \"krb_server_keyfile\"");
    assert_eq!(
        err.detail(),
        Some(
            "Only roles with privileges of the \"pg_read_all_settings\" role may examine this parameter."
        )
    );
    // Unprivileged read without the restriction still passes (C callers
    // that pass restrict_privileged=false skip the gate entirely).
    assert!(GetConfigOption("krb_server_keyfile", false, false).unwrap().is_some());
    // With the role privilege the restricted read passes too.
    HAS_PRIVS.set(true);
    assert!(GetConfigOption("krb_server_keyfile", false, true).unwrap().is_some());
    // Non-superuser-only options never consult the gate.
    HAS_PRIVS.set(false);
    assert!(GetConfigOption("work_mem", false, true).unwrap().is_some());
}

// idx 114 regression: a reused pooled parallel-worker thread must not carry a
// PRIOR session's client-startup / ALTER ROLE|DATABASE SET GUCs (sources <=
// PGC_S_OVERRIDE) into another session's task. ResetAllOptions cannot evict
// that class (it preserves those values and their reset_val stamps, by C
// parity); reset_store_to_process_base scrubs the thread to the fresh-backend
// baseline a C worker process would start from.
#[test]
fn reset_store_to_process_base_evicts_prior_session_low_source_gucs() {
    setup();

    let name = "maintenance_work_mem";
    let boot = with_store(|reg| match reg.find_option(name).unwrap() {
        GucVariable::Int(c) => c.boot_val,
        _ => unreachable!(),
    })
    .unwrap();

    let int_state = |name: &str| -> (i32, GucSource) {
        with_store(|reg| {
            let v = reg.find_option(name).unwrap();
            let val = match v {
                GucVariable::Int(c) => c.value.unwrap(),
                _ => unreachable!(),
            };
            (val, v.gen().source)
        })
        .unwrap()
    };

    // A prior session installs a value at PGC_S_CLIENT (a client startup
    // option). This is the make_default path: it also stamps reset_val.
    let attacker = boot + 1024;
    set_config_option_ext(
        name,
        Some(&attacker.to_string()),
        PGC_USERSET,
        PGC_S_CLIENT,
        BOOTSTRAP_SUPERUSERID,
        GUC_ACTION_SET,
        true,
        ErrorLevel(0),
        false,
    )
    .unwrap();
    assert_eq!(int_state(name).0, attacker);

    // RESET ALL preserves it (source <= PGC_S_OVERRIDE) -- exactly why it is
    // insufficient on a reused pooled thread.
    crate::store::reset_all_options();
    assert_eq!(
        int_state(name).0,
        attacker,
        "ResetAllOptions must (C parity) keep a <= PGC_S_OVERRIDE value"
    );

    // The fix: scrub the reused thread to the session-neutral fresh baseline.
    // (Exercise the registry-level scrub directly; the public
    // reset_store_to_process_base wraps this and then re-overlays the
    // process-global postmaster base, which is not deterministic under the
    // shared test binary.)
    scrub_to_boot();

    let (value, source) = int_state(name);
    assert_eq!(value, boot, "scrub must return the value to its boot default");
    assert_eq!(source, PGC_S_DEFAULT, "scrub must clear the per-session source");

    // The poisoned reset_val stamp must be gone too, so a later RESET cannot
    // resurrect the prior session's value.
    let reset_val = with_store(|reg| match reg.find_option(name).unwrap() {
        GucVariable::Int(c) => c.reset_val,
        _ => unreachable!(),
    })
    .unwrap();
    assert_eq!(reset_val, boot, "scrub must rebuild reset_val to boot");
}

fn scrub_to_boot() {
    let mut hooks: Vec<crate::registry::DeferredAssignHook> = Vec::new();
    with_store_mut(|reg| crate::registry::reset_session_options_to_boot(reg, &mut hooks)).unwrap();
    for hook in hooks {
        hook();
    }
}

fn string_state(name: &str) -> (Option<String>, GucSource) {
    with_store(|reg| {
        let v = reg.find_option(name).unwrap();
        let val = match v {
            GucVariable::String(c) => c.value.clone().flatten(),
            _ => unreachable!(),
        };
        (val, v.gen().source)
    })
    .unwrap()
}

// The warm-claimed worker scrub emulates a fresh fork: a GUC_NO_RESET_ALL
// session value (session_authorization, transaction_read_only, ...) that
// ResetAllOptions preserves by C parity must not survive into the next
// leader's engagement either.
#[test]
fn reset_store_to_process_base_evicts_no_reset_all_session_values() {
    setup();
    let name = "transaction_read_only";
    let state = || {
        with_store(|reg| {
            let v = reg.find_option(name).unwrap();
            let val = match v {
                GucVariable::Bool(c) => c.value.unwrap(),
                _ => unreachable!(),
            };
            (val, v.gen().source)
        })
        .unwrap()
    };
    set_config_option_ext(
        name,
        Some("on"),
        PGC_USERSET,
        PGC_S_SESSION,
        BOOTSTRAP_SUPERUSERID,
        GUC_ACTION_SET,
        true,
        ErrorLevel(0),
        false,
    )
    .unwrap();
    assert_eq!(state(), (true, PGC_S_SESSION));

    crate::store::reset_all_options();
    assert_eq!(state().0, true, "RESET ALL keeps GUC_NO_RESET_ALL values (C parity)");

    scrub_to_boot();
    assert_eq!(state(), (false, PGC_S_DEFAULT), "scrub must not skip GUC_NO_RESET_ALL");
}

// assign_session_authorization's shape: SetSessionAuthorization re-enters the
// store through SetConfigOption("is_superuser"); the scrub must fire it past
// its own borrow (pooled workers died "RefCell already borrowed" inline).
fn reentrant_session_authorization_assign(
    _newval: Option<&str>,
    _extra: Option<&guc_tables::GucHookExtra>,
) {
    // Installed process-wide: stay inert for another test's InitializeGUCOptions.
    if with_store(|_| ()).is_none() {
        return;
    }
    crate::store::set_config_option_global(
        "is_superuser",
        Some("off"),
        PGC_INTERNAL,
        PGC_S_DYNAMIC_DEFAULT,
        BOOTSTRAP_SUPERUSERID,
        GUC_ACTION_SET,
        true,
        ErrorLevel(0),
        false,
    )
    .unwrap();
}

#[test]
fn scrub_runs_reentrant_no_reset_all_assign_hooks_outside_the_store_borrow() {
    setup();
    guc_tables::hooks::assign_session_authorization
        .install_if_absent(reentrant_session_authorization_assign);
    let name = "session_authorization";
    set_config_option_ext(
        name,
        Some("alice"),
        PGC_USERSET,
        PGC_S_SESSION,
        BOOTSTRAP_SUPERUSERID,
        GUC_ACTION_SET,
        true,
        ErrorLevel(0),
        false,
    )
    .unwrap();
    assert_eq!(string_state(name), (Some("alice".to_string()), PGC_S_SESSION));

    scrub_to_boot();
    assert_eq!(string_state(name), (None, PGC_S_DEFAULT));
}

fn reentrant_string_check(
    newval: &mut Option<String>,
    _extra: &mut Option<guc_tables::GucHookExtra>,
    _source: GucSource,
) -> types_error::PgResult<bool> {
    // check_datestyle's shape: 'DEFAULT' resolves against the reset value.
    if newval.as_deref() == Some("reset") {
        *newval = Some(GetConfigOptionResetString("createrole_self_grant").expect("reset value"));
    }
    Ok(true)
}

#[test]
fn check_hook_may_read_the_store() {
    setup();
    guc_tables::hooks::check_createrole_self_grant.install_if_absent(reentrant_string_check);
    assert_eq!(set_session("createrole_self_grant", Some("set, inherit")).unwrap(), 1);
    assert_eq!(get_string("createrole_self_grant"), Some(Some("set, inherit".to_string())));
    assert_eq!(set_session("createrole_self_grant", Some("reset")).unwrap(), 1);
    assert_eq!(get_string("createrole_self_grant"), Some(Some(String::new())));
}

// help_config.c printMixedStruct rows as postgres 18.6 --describe-config
// prints them (bool/int/real show the pre-InitializeOneGUCOption reset_val
// zero; strings/enums their boot_val), displayStruct hiding, C ordering.
#[test]
fn describe_config_rows_match_c() {
    setup();
    // Enum option sets that live in GUC slots are installed by their owning
    // units (transam_xlog/dsm/aio), which this test binary does not link;
    // stub them empty (the guc_funcs SHOW ALL tests do the same) — their
    // rows print an empty value column here, so the asserted ENUM row is an
    // Inline-options one.
    for slot in [
        &guc_tables::option_sets::archive_mode_options,
        &guc_tables::option_sets::dynamic_shared_memory_options,
        &guc_tables::option_sets::io_method_options,
        &guc_tables::option_sets::recovery_target_action_options,
        &guc_tables::option_sets::wal_level_options,
        &guc_tables::option_sets::wal_sync_method_options,
    ] {
        slot.install_if_absent(&[]);
    }
    let text = crate::help_config::guc_info_text();
    let lines: Vec<&str> = text.lines().collect();
    for want in [
        "enable_seqscan\tuser\tQuery Tuning / Planner Method Configuration\tBOOLEAN\tFALSE\t\t\tEnables the planner's use of sequential-scan plans.\t",
        "shared_buffers\tpostmaster\tResource Usage / Memory\tINTEGER\t0\t16\t1073741823\tSets the number of shared memory buffers used by the server.\t",
        "cpu_tuple_cost\tuser\tQuery Tuning / Planner Cost Constants\tREAL\t0\t0\t1.79769e+308\tSets the planner's estimate of the cost of processing each tuple (row).\t",
        "log_line_prefix\tsighup\tReporting and Logging / What to Log\tSTRING\t%m [%p] \t\t\tControls information prefixed to each log line.\tAn empty string means no prefix.",
        "wal_compression\tsuperuser\tWrite-Ahead Log / Settings\tENUM\toff\t\t\tCompresses full-page writes written in WAL file with specified method.\t",
    ] {
        assert!(lines.contains(&want), "missing row: {want:?}");
    }
    // displayStruct: GUC_DISALLOW_IN_FILE / GUC_NOT_IN_SAMPLE / GUC_NO_SHOW_ALL
    // rows never print (config_file is GUC_DISALLOW_IN_FILE; C omits it).
    assert!(!lines.iter().any(|l| l.starts_with("config_file\t")), "config_file must be hidden");
    // guc_var_compare order.
    for w in lines.windows(2) {
        let (a, b) = (w[0].split('\t').next().unwrap(), w[1].split('\t').next().unwrap());
        assert_ne!(guc_name_compare(a, b), std::cmp::Ordering::Greater, "{a} before {b}");
    }
    assert!(lines.len() >= 300, "only {} rows", lines.len());
}

// reapply_stacked_values (guc.c:5054): oldvarstack is captured before the
// recursion, so an entry the recursion pushed takes this level's nest level
// even when this level's own assignment was rejected.
#[test]
fn reapply_adjusts_recursion_pushed_entry_when_own_assignment_fails() {
    setup();
    AtStart_GUC();
    assert_eq!(set_session("b168nest.v", Some("one")).unwrap(), 1);
    let inner = NewGUCNestLevel();
    let rc = set_config_option_ext(
        "b168nest.v",
        Some("two"),
        PGC_USERSET,
        PGC_S_SESSION,
        DENIED_ROLE,
        GUC_ACTION_SET,
        true,
        ErrorLevel(0),
        false,
    )
    .unwrap();
    assert_eq!(rc, 1);
    DefineCustomStringVariable("b168nest.v", Some("desc"), None, None, PGC_SUSET, 0).unwrap();
    assert_eq!(show("b168nest.v"), Some("one".to_string()));
    AtEOXact_GUC(false, inner);
    assert_eq!(show("b168nest.v"), Some(String::new()));
    AtEOXact_GUC(true, 1);
}

// MarkGUCPrefixReserved (guc.c:5298): the removal WARNINGs come in
// guc_hashtab bucket order (C 18.6 on these names: zed b c a f g d e h
// long_name_x) and run the error-context callbacks like any ereport.
#[test]
fn prefix_reservation_warnings_in_hash_order_with_context() {
    setup();
    let _emitted = EMITTED_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for name in ["a", "b", "c", "d", "e", "f", "g", "h", "long_name_x", "zed"] {
        assert_eq!(set_session(&format!("postgres_fdw.{name}"), Some("1")).unwrap(), 1);
    }
    let callback = elog::push_emit_context_callback(Box::new(|e| {
        e.add_context_line("SQL statement \"LOAD 'postgres_fdw'\"");
    }));
    let prev = elog::set_emit_log_hook(Some(capture_emitted));
    MarkGUCPrefixReserved("postgres_fdw");
    elog::set_emit_log_hook(prev);
    elog::pop_emit_context_callback(callback);
    let emitted = std::mem::take(&mut *EMITTED.lock().unwrap_or_else(|e| e.into_inner()));
    let names: Vec<String> = emitted
        .iter()
        .map(|e| {
            assert_eq!(e.level, types_error::WARNING);
            assert_eq!(e.detail(), Some("\"postgres_fdw\" is now a reserved prefix."));
            assert_eq!(e.context(), Some("SQL statement \"LOAD 'postgres_fdw'\""));
            e.message()
                .trim_start_matches("invalid configuration parameter name \"postgres_fdw.")
                .trim_end_matches("\", removing it")
                .to_string()
        })
        .collect();
    assert_eq!(names, ["zed", "b", "c", "a", "f", "g", "d", "e", "h", "long_name_x"]);
}

// ReportGUCOption (guc.c:2645) sends both strings through pq_sendstring's
// client_encoding conversion.
#[test]
fn parameter_status_is_sent_in_client_encoding() {
    setup();
    let _guard = APPLICATION_NAME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    elog::config::set_where_to_send_output(types_dest::CommandDest::Remote);
    begin_reporting_guc_options();
    SENT.with(|s| s.borrow_mut().clear());
    assert_eq!(set_session("application_name", Some("conv:psql")).unwrap(), 1);
    report_changed_guc_options();
    let frames = SENT.with(|s| std::mem::take(&mut *s.borrow_mut()));
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].1, b"application_name\0CONV:PSQL\0".to_vec());
    assert_eq!(show("application_name"), Some("conv:psql".to_string()));
}

// guc.c:5245: a postmaster-time (shared_preload_libraries) custom definition
// survives into a child's rebuilt registry, ahead of its reserved prefix.
#[test]
fn inherited_custom_definitions_survive_child_registry_rebuild() {
    setup();
    DefineCustomStringVariable("b168pre.name", Some("desc"), None, Some("boot"), PGC_USERSET, 0)
        .unwrap();
    MarkGUCPrefixReserved("b168pre");
    let definitions = custom_variable_definitions();
    assert!(definitions.iter().any(|d| matches!(d, CustomVariableDefinition::String { name: "b168pre.name", .. })));
    let prefixes = reserved_class_prefixes();
    std::thread::spawn(move || {
        setup();
        inherit_reserved_class_prefixes(&prefixes);
        inherit_custom_variable_definitions(&definitions);
        crate::store::initialize_guc_options_for_child(&[]).unwrap();
        assert_eq!(show("b168pre.name"), Some("boot".to_string()));
        assert_eq!(set_session("b168pre.name", Some("x")).unwrap(), 1);
        assert_eq!(show("b168pre.name"), Some("x".to_string()));
    })
    .join()
    .unwrap();
}

// guc.c: GUC_PENDING_RESTART set in the postmaster is inherited by every
// later fork, so a fresh session reports pg_settings.pending_restart = true
// for a PGC_POSTMASTER variable ALTER SYSTEM changed since boot.
#[test]
fn pending_restart_status_crosses_the_child_bind() {
    setup();
    let changed = set_config_option_ext(
        "shared_buffers",
        Some("32768"),
        PGC_SIGHUP,
        PGC_S_FILE,
        BOOTSTRAP_SUPERUSERID,
        GUC_ACTION_SET,
        true,
        ErrorLevel(0),
        false,
    )
    .unwrap();
    assert_eq!(changed, 0);
    let caps = crate::store::capture_session_gucs();
    let cap = caps.iter().find(|c| c.name() == "shared_buffers").expect("captured");
    assert!(cap.pending_restart());
    assert_eq!(cap.source(), PGC_S_DEFAULT);
    let base = crate::layers::GucBaseSnapshot::for_tests(crate::store::capture_session_gucs());
    assert!(!base.contains("shared_buffers"));
    std::thread::spawn(move || {
        setup();
        let _binding = crate::store::bind_session_gucs(&caps).unwrap();
        let (status, source) = with_store(|reg| {
            let v = reg.find_option("shared_buffers").unwrap();
            (v.gen().status, v.gen().source)
        })
        .unwrap();
        assert!(status & crate::model::GUC_PENDING_RESTART != 0);
        assert_eq!(source, PGC_S_DEFAULT);
    })
    .join()
    .unwrap();
}

/// postmaster.c fork / launch_backend.c:1034: a fresh backend starts with
/// the postmaster's PgReloadTime, so pg_conf_load_time() reports the
/// configuration load time and not the 2000-01-01 epoch.
#[test]
fn child_inherits_the_postmaster_reload_time() {
    let _g = APPLICATION_NAME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // This thread alone plays the postmaster (MyProcPid == PostmasterPid);
    // other test threads' reloads must not publish over the inherited value.
    let saved_pm = init_small::globals::PostmasterPid();
    init_small::globals::SetMyProcPid(77_782);
    init_small::globals::SetPostmasterPid(77_782);
    set_pg_reload_time(4_242_424_242);
    init_small::globals::SetPostmasterPid(saved_pm);
    let child = std::thread::spawn(|| {
        assert_eq!(pg_reload_time(), 0, "a fresh thread starts at the epoch");
        crate::store::inherit_postmaster_reload_time();
        pg_reload_time()
    })
    .join()
    .unwrap();
    assert_eq!(child, 4_242_424_242);
}

// guc.c:453 walks guc_hashtab, which MarkGUCPrefixReserved (guc.c:5314) has
// already purged invalid placeholders from; the GUC_REMOVED tombstone that
// stands in for the purge must not be reset by name on the next reload
// (that fails the reserved-prefix check and logs a spurious diagnostic).
#[test]
fn process_config_file_skips_removed_placeholder_tombstones() {
    setup();
    let _guard = APPLICATION_NAME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _emitted = EMITTED_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = Scratch::new("guc_pcf_tombstone");
    let conf = dir.join("postgresql.conf");
    init_small::globals::SetDataDir(dir.to_str().unwrap());

    std::fs::write(&conf, "b169rsv.stale = 'x'\n").unwrap();
    SetConfigOption("config_file", Some(conf.to_str().unwrap()), PGC_POSTMASTER, PGC_S_OVERRIDE)
        .unwrap();
    let clean =
        crate::process_config::process_config_file_internal(PGC_SIGHUP, true, types_error::LOG)
            .unwrap();
    assert!(clean);
    assert_eq!(show("b169rsv.stale"), Some("x".to_string()));
    assert_eq!(
        with_store(|reg| reg.find_option("b169rsv.stale").unwrap().gen().reset_source).unwrap(),
        PGC_S_FILE
    );

    let prev = elog::set_emit_log_hook(Some(capture_emitted));
    MarkGUCPrefixReserved("b169rsv");
    let emitted = std::mem::take(&mut *EMITTED.lock().unwrap_or_else(|e| e.into_inner()));
    assert_eq!(emitted.len(), 1, "{emitted:?}");
    assert_eq!(emitted[0].level, types_error::WARNING);
    assert!(with_store(|reg| reg.find_option("b169rsv.stale").is_none()).unwrap());

    // Removed from the file too: C's hash walk no longer sees the variable,
    // so the reload is silent.
    std::fs::write(&conf, "\n").unwrap();
    let clean =
        crate::process_config::process_config_file_internal(PGC_SIGHUP, true, types_error::LOG)
            .unwrap();
    elog::set_emit_log_hook(prev);
    assert!(clean);
    let emitted = std::mem::take(&mut *EMITTED.lock().unwrap_or_else(|e| e.into_inner()));
    let msgs: Vec<String> = emitted
        .iter()
        .map(|e| e.message().to_string())
        .filter(|m| m.contains("b169rsv"))
        .collect();
    assert!(msgs.is_empty(), "{msgs:?}");
    assert!(with_store(|reg| reg.find_option("b169rsv.stale").is_none()).unwrap());
}

// GUC_check_errdetail_raw: a check hook's non-UTF-8 detail bytes reach the
// wire verbatim (user.c:2547 echoes a byte-truncated key word).
#[test]
fn check_hook_raw_detail_reaches_the_error() {
    reset_guc_check_error();
    GUC_check_errdetail_raw(b"Unrecognized key word: \"zz\xff\".".to_vec());
    let e = crate::registry::check_hook_error("invalid value".to_string());
    assert_eq!(e.message(), "invalid value");
    assert_eq!(e.detail(), Some("Unrecognized key word: \"zz\u{fffd}\"."));
    assert_eq!(e.detail_raw.as_deref(), Some(&b"Unrecognized key word: \"zz\xff\"."[..]));
    assert_eq!(e.sqlstate(), ERRCODE_INVALID_PARAMETER_VALUE);
}

// fp-misc-guc-p1#1: a placeholder's name is owned by the registry (C: a
// guc_strdup into GUCMemoryContext, which dies with the backend), never
// leaked into the process. Witness: allocations of one sentinel size that
// nothing else in this test binary requests are censused by the allocator;
// the names must all be released when their registry drops.
mod placeholder_name_census {
    use core::sync::atomic::{AtomicIsize, Ordering};
    use std::alloc::{GlobalAlloc, Layout, System};

    pub const SENTINEL: usize = 4093;
    pub static LIVE: AtomicIsize = AtomicIsize::new(0);

    pub struct Census;

    // SAFETY: a pure pass-through to System plus a counter.
    unsafe impl GlobalAlloc for Census {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            if layout.size() == SENTINEL {
                LIVE.fetch_add(1, Ordering::SeqCst);
            }
            // SAFETY: forwarded verbatim.
            unsafe { System.alloc(layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            if layout.size() == SENTINEL {
                LIVE.fetch_sub(1, Ordering::SeqCst);
            }
            // SAFETY: forwarded verbatim.
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    #[global_allocator]
    static CENSUS: Census = Census;
}

#[test]
fn placeholder_names_die_with_their_registry() {
    use core::sync::atomic::Ordering;
    use placeholder_name_census::{LIVE, SENTINEL};

    let before = LIVE.load(Ordering::SeqCst);
    let mut reg = GucRegistry::new();
    for i in 0..8 {
        let prefix = format!("lane{i}.");
        let name = format!("{prefix}{}", "x".repeat(SENTINEL - prefix.len()));
        assert_eq!(name.len(), SENTINEL);
        reg.add_placeholder_variable(&name).unwrap();
    }
    assert!(
        LIVE.load(Ordering::SeqCst) - before >= 8,
        "the placeholder names were not allocated at the sentinel size"
    );
    drop(reg);
    assert_eq!(
        LIVE.load(Ordering::SeqCst),
        before,
        "placeholder names outlived their registry (C frees them with GUCMemoryContext at backend exit)"
    );
}

// guc_nondef_list is a dlist (guc.c:2117-2122): leaving and re-entering
// PGC_S_DEFAULT is O(1) per variable, so RESET ALL / AtEOXact_GUC over N
// placeholders is linear, and the list keeps first-left-default order.
#[test]
fn nondef_list_unlink_is_constant_time_and_ordered() {
    let n = 200_000;
    let mut reg = GucRegistry::new();
    let idxs: Vec<usize> =
        (0..n).map(|i| reg.add_placeholder_variable(&format!("my.p{i}")).unwrap()).collect();
    let start = std::time::Instant::now();
    for &idx in &idxs {
        reg.set_source_pub(idx, PGC_S_SESSION);
    }
    for &idx in &idxs {
        reg.set_source_pub(idx, PGC_S_DEFAULT);
    }
    assert!(start.elapsed() < std::time::Duration::from_secs(10), "{:?}", start.elapsed());

    for &i in &[3usize, 1, 2, 0] {
        reg.set_source_pub(idxs[i], PGC_S_SESSION);
    }
    reg.set_source_pub(idxs[1], PGC_S_DEFAULT);
    reg.set_source_pub(idxs[1], PGC_S_SESSION);
    let order: Vec<String> = crate::registry::capture_session_gucs(&reg)
        .into_iter()
        .map(|c| c.name().to_string())
        .collect();
    assert_eq!(order, ["my.p3", "my.p2", "my.p0", "my.p1"]);
}

// C 18.6 BeginReportingGUCOptions on a fresh connection (guc_hashtab
// hash_seq_search order, 512 buckets), captured from the C oracle.
#[test]
fn hash_seq_order_matches_c_guc_hashtab() {
    setup();
    let names: Vec<String> = with_store(|reg| {
        let mut out = Vec::new();
        for idx in crate::name::hash_seq_order(reg) {
            if reg[idx].gen().flags & GUC_REPORT != 0 {
                out.push(reg[idx].name().to_string());
            }
        }
        out
    })
    .unwrap();
    assert_eq!(
        names,
        [
            "in_hot_standby",
            "integer_datetimes",
            "TimeZone",
            "IntervalStyle",
            "search_path",
            "is_superuser",
            "application_name",
            "default_transaction_read_only",
            "scram_iterations",
            "DateStyle",
            "standard_conforming_strings",
            "session_authorization",
            "client_encoding",
            "server_version",
            "server_encoding",
        ]
    );
    assert_eq!(crate::name::guc_name_hash("shared_buffers") & 511, 51);
    assert_eq!(crate::name::guc_name_hash("Max_Connections") & 511, 351);
}

#[test]
fn process_config_file_removed_params_error_items_in_c_hash_order() {
    setup();
    let _guard = APPLICATION_NAME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = Scratch::new("guc_pcf_removed2");
    let conf = dir.join("postgresql.conf");
    init_small::globals::SetDataDir(dir.to_str().unwrap());

    std::fs::write(&conf, "max_connections = 100\nshared_buffers = 16384\n").unwrap();
    SetConfigOption("config_file", Some(conf.to_str().unwrap()), PGC_POSTMASTER, PGC_S_OVERRIDE)
        .unwrap();
    let clean = crate::process_config::process_config_file_internal(
        PGC_POSTMASTER,
        true,
        types_error::LOG,
    )
    .unwrap();
    assert!(clean);

    std::fs::write(&conf, "application_name = 'unrelated'\n").unwrap();
    let (clean, items) = crate::process_config::process_config_file_internal_list(
        PGC_SIGHUP,
        false,
        types_error::LOG,
    )
    .unwrap();
    assert!(!clean);
    let errs: Vec<&str> = items.iter().filter_map(|i| i.errmsg.as_deref()).collect();
    assert_eq!(
        errs,
        [
            "parameter \"shared_buffers\" cannot be changed without restarting the server",
            "parameter \"max_connections\" cannot be changed without restarting the server",
        ]
    );
}

#[test]
fn sighup_reread_of_postmaster_param_reports_canonical_name() {
    setup();
    let e = set_config_option_ext(
        "Shared_Buffers",
        Some("99999"),
        PGC_SIGHUP,
        PGC_S_FILE,
        BOOTSTRAP_SUPERUSERID,
        GUC_ACTION_SET,
        true,
        types_error::ERROR,
        false,
    )
    .unwrap_err();
    assert_eq!(
        e.message(),
        "parameter \"shared_buffers\" cannot be changed without restarting the server"
    );
}

fn throwing_string_check_hook(
    newval: &mut Option<String>,
    _extra: &mut Option<guc_tables::GucHookExtra>,
    _source: GucSource,
) -> PgResult<bool> {
    if newval.as_deref() != Some("xyz") {
        return Ok(true);
    }
    Err(types_error::PgError::new(
        types_error::ERROR,
        "invalid input syntax for type interval: \"xyz\"",
    )
    .into())
}

// guc.c:6954 PG_RE_THROW: an error raised inside a check hook propagates
// even when the rejection level is demoted (file source in a backend).
#[test]
fn check_hook_error_is_rethrown_at_demoted_elevel() {
    setup();
    guc_tables::hooks::check_temp_tablespaces.install_if_absent(throwing_string_check_hook);
    let e = set_config_option_ext(
        "temp_tablespaces",
        Some("xyz"),
        PGC_SIGHUP,
        PGC_S_FILE,
        BOOTSTRAP_SUPERUSERID,
        GUC_ACTION_SET,
        false,
        ErrorLevel(0),
        false,
    )
    .unwrap_err();
    assert_eq!(e.message(), "invalid input syntax for type interval: \"xyz\"");
}

#[test]
fn typed_custom_variables_adopt_stacks_and_survive_child_rebuild() {
    setup();
    let mut setting = guc_tables::ConfigureNamesInt.iter().copied()
        .find(|s| s.name == "work_mem").unwrap();
    static SLOT: guc_tables::GucIntVar = guc_tables::GucSlot::new("typed_extension_limit");
    setting.variable = &SLOT;
    setting.name = "typed_extension.limit";
    setting.min = 0;
    setting.max = 100;
    setting.flags = 0;
    setting.boot_val = guc_tables::GucDefaultValue::Int(10);
    setting.check_hook = None;
    setting.assign_hook = None;
    setting.show_hook = None;
    AtStart_GUC();
    set_session(setting.name, Some("20")).unwrap();
    set_config_option_ext(setting.name, Some("30"), PGC_USERSET, PGC_S_SESSION,
        BOOTSTRAP_SUPERUSERID, GUC_ACTION_LOCAL, true, ErrorLevel(0), false).unwrap();
    DefineCustomVariable(guc_tables::GucSetting::Int(setting)).unwrap();
    assert_eq!(get_int(setting.name), Some(30));
    AtEOXact_GUC(true, 1);
    assert_eq!(get_int(setting.name), Some(20));
    assert!(set_session(setting.name, Some("101")).is_err());
    set_session(setting.name, None).unwrap();
    assert_eq!(get_int(setting.name), Some(10));
    let definitions = custom_variable_definitions();
    std::thread::spawn(move || {
        setup();
        inherit_custom_variable_definitions(&definitions);
        crate::store::initialize_guc_options_for_child(&[]).unwrap();
        assert_eq!(get_int("typed_extension.limit"), Some(10));
    }).join().unwrap();
}

#[test]
fn restored_enum_assign_hook_observes_previous_backing_value() {
    use core::sync::atomic::{AtomicI32, Ordering};
    use guc_tables::{GucEnumAssignHook, GucEnumVar, GucVarAccessors};

    static VALUE: AtomicI32 = AtomicI32::new(0);
    static OBSERVED: AtomicI32 = AtomicI32::new(-1);
    static VARIABLE: GucEnumVar = GucEnumVar::new("restore_test");
    static HOOK: GucEnumAssignHook = GucEnumAssignHook::new("restore_test");
    VARIABLE.install(GucVarAccessors {
        get: || VALUE.load(Ordering::SeqCst),
        set: |v| VALUE.store(v, Ordering::SeqCst),
    });
    HOOK.install(|_, _| {
        OBSERVED.store(VALUE.load(Ordering::SeqCst), Ordering::SeqCst);
    });

    for (action, commit, nested) in [
        (GUC_ACTION_SET, false, false),
        (GUC_ACTION_LOCAL, true, false),
        (GUC_ACTION_SAVE, true, true),
    ] {
        setup();
        VALUE.store(0, Ordering::SeqCst);
        with_store_mut(|reg| {
            let GucVariable::Enum(c) = reg.find_option_mut("session_replication_role").unwrap() else {
                panic!("expected enum");
            };
            c.variable = &VARIABLE;
            c.assign_hook = Some(&HOOK);
        })
        .unwrap();
        AtStart_GUC();
        let nest = if nested { NewGUCNestLevel() } else { 1 };
        set_config_option_ext(
            "session_replication_role",
            Some("replica"),
            PGC_SUSET,
            PGC_S_SESSION,
            BOOTSTRAP_SUPERUSERID,
            action,
            true,
            ErrorLevel(0),
            false,
        )
        .unwrap();
        assert_eq!(OBSERVED.load(Ordering::SeqCst), 0);
        assert_eq!(VALUE.load(Ordering::SeqCst), 1);
        AtEOXact_GUC(commit, nest);
        assert_eq!(OBSERVED.load(Ordering::SeqCst), 1);
        assert_eq!(VALUE.load(Ordering::SeqCst), 0);
        if nested {
            AtEOXact_GUC(true, 1);
        }
    }

    AtStart_GUC();
    for value in ["replica", "origin"] {
        set_config_option_ext(
            "session_replication_role",
            Some(value),
            PGC_SUSET,
            PGC_S_SESSION,
            BOOTSTRAP_SUPERUSERID,
            GUC_ACTION_SET,
            true,
            ErrorLevel(0),
            false,
        )
        .unwrap();
        if value == "replica" {
            NewGUCNestLevel();
        }
    }
    AtEOXact_GUC(false, 1);
    assert_eq!(OBSERVED.load(Ordering::SeqCst), 1);
    assert_eq!(VALUE.load(Ordering::SeqCst), 0);
}

#[test]
fn session_bind_resets_worker_file_values_leader_lacks() {
    setup();
    let caps: Vec<crate::store::CapturedGuc> = Vec::new();
    std::thread::spawn(move || {
        setup();
        let rc = set_config_option_ext(
            "extra_float_digits",
            Some("0"),
            PGC_SIGHUP,
            PGC_S_FILE,
            BOOTSTRAP_SUPERUSERID,
            GUC_ACTION_SET,
            true,
            ErrorLevel(0),
            false,
        )
        .unwrap();
        assert_eq!(rc, 1);
        assert_eq!(get_int("extra_float_digits"), Some(0));
        let _binding = crate::store::bind_session_gucs(&caps).unwrap();
        assert_eq!(get_int("extra_float_digits"), Some(1));
    })
    .join()
    .unwrap();
}

#[test]
fn reset_store_to_process_base_drops_prior_session_placeholders() {
    setup();
    set_session("app_ctx.secret", Some("session-1-value")).unwrap();
    assert!(with_store(|reg| reg.find_option("app_ctx.secret").is_some()).unwrap());
    scrub_to_boot();
    with_store_mut(crate::registry::GucRegistry::remove_all_placeholders).unwrap();
    assert!(with_store(|reg| reg.find_option("app_ctx.secret").is_none()).unwrap());
    assert_eq!(GetConfigOption("app_ctx.secret", true, false).unwrap(), None);
    let len = with_store(|reg| reg.len()).unwrap();
    set_session("app_ctx.other", Some("x")).unwrap();
    assert_eq!(with_store(|reg| reg.len()).unwrap(), len);
    assert_eq!(string_state("app_ctx.other").0.as_deref(), Some("x"));
    assert!(with_store(|reg| reg.find_option("app_ctx.secret").is_none()).unwrap());
}

/// A crash-restarted server generation restores the previous one's settings
/// image (C's postmaster keeps its settings across the crash): values,
/// sources, sourcefile/sourceline, pending_restart and PgReloadTime.
#[test]
fn postmaster_settings_image_round_trips() {
    let _g = APPLICATION_NAME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let image = std::thread::spawn(|| {
        setup();
        set_config_option_ext(
            "work_mem",
            Some("7MB"),
            PGC_POSTMASTER,
            PGC_S_FILE,
            BOOTSTRAP_SUPERUSERID,
            GUC_ACTION_SET,
            true,
            ErrorLevel(0),
            false,
        )
        .unwrap();
        crate::process_config::set_config_sourcefile("work_mem", "/dd/postgresql.conf", 12);
        with_store_mut(|reg| {
            reg.find_option_mut("shared_buffers").unwrap().gen_mut().status |=
                crate::model::GUC_PENDING_RESTART;
        });
        set_pg_reload_time(4_242);
        let mut image = Vec::new();
        crate::store::write_postmaster_gucs(&mut image);
        image
    })
    .join()
    .unwrap();
    std::thread::spawn(move || {
        setup();
        assert!(!crate::store::read_postmaster_gucs(&image[..image.len() - 1]).unwrap());
        setup();
        assert!(crate::store::read_postmaster_gucs(&image).unwrap());
        assert_eq!(show("work_mem"), Some("7MB".to_string()));
        let (source, file, line) = with_store(|reg| {
            let g = reg.find_option("work_mem").unwrap().gen();
            (g.source, g.sourcefile.clone(), g.sourceline)
        })
        .unwrap();
        assert_eq!((source, file.as_deref(), line), (PGC_S_FILE, Some("/dd/postgresql.conf"), 12));
        let status = with_store(|reg| reg.find_option("shared_buffers").unwrap().gen().status).unwrap();
        assert!(status & crate::model::GUC_PENDING_RESTART != 0);
        assert_eq!(pg_reload_time(), 4_242);
    })
    .join()
    .unwrap();
}

/// D4244: a claim carries the leader's PGC_SIGHUP server-log settings whose backing is per thread (carry.rs
/// `THREAD_SIGHUP`), as RestoreGUCState carries them to a parallel worker. A pool thread never rereads the file, so
/// without the carry a log line a claim writes read the values its thread started with. The claim's end puts the
/// thread's own back, and a reload on the leader between two claims reaches the next one: the first claim's undo puts
/// back no stale value.
#[test]
fn claim_carry_applies_the_leaders_thread_sighup_log_settings() {
    let (before, after) = std::thread::spawn(|| {
        setup();
        let file = |name: &str, value: &str| {
            set_config_option_ext(name, Some(value), PGC_SIGHUP, PGC_S_FILE, BOOTSTRAP_SUPERUSERID, GUC_ACTION_SET, true, ErrorLevel(0), false).unwrap()
        };
        assert_eq!(file("log_line_prefix", "leader %p "), 1);
        assert_eq!(file("syslog_split_messages", "off"), 1);
        let before = crate::layers::current_query_pin();
        // the leader's reload (ProcessConfigFile sets the file's values at PGC_SIGHUP)
        assert_eq!(file("log_line_prefix", "reloaded %p "), 1);
        let after = crate::layers::current_query_pin();
        assert!(!std::sync::Arc::ptr_eq(&before, &after), "a reload moves the leader's pin");
        (before, after)
    })
    .join()
    .unwrap();
    std::thread::spawn(move || {
        setup();
        let (prefix, split) = (&guc_tables::vars::Log_line_prefix, &guc_tables::vars::syslog_split_messages);
        let own = (prefix.read(), split.read());
        assert_eq!(own, (Some("%m [%p] ".to_string()), true), "a fresh thread holds the boot values");
        {
            let _claim = crate::carry::arm_claim(&before).unwrap();
            assert_eq!((prefix.read(), split.read()), (Some("leader %p ".to_string()), false), "the leader's file values");
            assert_eq!(elog::config::log_line_prefix_format().as_deref(), Some("leader %p "), "the prefix a log line reads");
        }
        assert_eq!((prefix.read(), split.read()), own, "the claim's end put the thread's own back");
        {
            let _claim = crate::carry::arm_claim(&after).unwrap();
            assert_eq!((prefix.read(), split.read()), (Some("reloaded %p ".to_string()), false), "the next claim reads the reload");
            assert_eq!(elog::config::log_line_prefix_format().as_deref(), Some("reloaded %p "), "the reloaded prefix a log line reads");
        }
        assert_eq!((prefix.read(), split.read()), own, "the second claim's end put the thread's own back");
    })
    .join()
    .unwrap();
}

/// D4240: wal_sync_method is PGC_SIGHUP with a per-thread backing (transam_xlog guc_vars.rs), read by a claim's own WAL
/// write, so carry.rs `THREAD_SIGHUP` carries it. This binary does not link transam_xlog: the backing is a stand-in.
#[test]
fn claim_carry_takes_wal_sync_method() {
    use core::sync::atomic::{AtomicI32, Ordering};
    static METHOD: AtomicI32 = AtomicI32::new(0);
    setup();
    guc_tables::vars::wal_sync_method.install_if_absent(guc_tables::GucVarAccessors {
        get: || METHOD.load(Ordering::Relaxed),
        set: |v| METHOD.store(v, Ordering::Relaxed),
    });
    let context = with_store(|reg| reg.find_option("wal_sync_method").unwrap().gen().context).unwrap();
    assert!(matches!(context, types_guc::GucContext::PGC_SIGHUP), "wal_sync_method is a PGC_SIGHUP variable");
    assert!(crate::carry::is_carried("wal_sync_method"), "a claim carries the leader's wal_sync_method");
}

/// D4240: fsync is PGC_SIGHUP with a per-thread backing (init_small globals.rs ENABLE_FSYNC), read by a claim's own WAL
/// flush (issue_xlog_fsync), so carry.rs `THREAD_SIGHUP` carries it: a claim sees the leader's reloaded value and its end
/// puts the thread's own back.
#[test]
fn claim_carry_takes_the_leaders_fsync() {
    // the server's backing (init_small::init_seams installs it; this binary's setup does not): the per-thread cell
    guc_tables::vars::enableFsync.install_if_absent(guc_tables::GucVarAccessors {
        get: init_small::globals::enableFsync,
        set: init_small::globals::set_enableFsync,
    });
    let pin = std::thread::spawn(|| {
        setup();
        let set = set_config_option_ext("fsync", Some("off"), PGC_SIGHUP, PGC_S_FILE, BOOTSTRAP_SUPERUSERID, GUC_ACTION_SET, true, ErrorLevel(0), false);
        assert_eq!(set.unwrap(), 1);
        crate::layers::current_query_pin()
    })
    .join()
    .unwrap();
    std::thread::spawn(move || {
        setup();
        assert!(init_small::globals::enableFsync(), "a fresh thread holds the boot value");
        {
            let _claim = crate::carry::arm_claim(&pin).unwrap();
            assert!(!init_small::globals::enableFsync(), "the claim reads the leader's fsync = off");
        }
        assert!(init_small::globals::enableFsync(), "the claim's end put the thread's own back");
    })
    .join()
    .unwrap();
}

/// C7 (D3919): a claim on another thread runs with the leader's session values (carry.rs `arm_claim`), only where they
/// differ, and puts the thread's own back when it ends, a nested claim of another session's end the outer claim's.
#[test]
fn claim_carry_applies_the_leaders_session_and_puts_the_threads_back() {
    setup();
    let (rs, admit) = (&guc_tables::vars::row_security, &guc_tables::vars::pgrust_gravity_admit);
    assert_eq!(set_session("row_security", Some("off")).unwrap(), 1);
    assert_eq!(set_session("pgrust.gravity_admit", Some("shape")).unwrap(), 1);
    let shape = admit.read();
    assert_ne!(shape, guc_tables::consts::GRAVITY_ADMIT_GRAPH);
    let leader = crate::layers::current_query_pin();
    assert!(std::sync::Arc::ptr_eq(&leader, &crate::layers::current_query_pin()), "an unmoved store keeps its pin");
    // another session: row_security on by its own SET (captured), the admission at its default (not captured)
    let other = std::thread::spawn(|| {
        setup();
        assert_eq!(set_session("row_security", Some("on")).unwrap(), 1);
        crate::layers::current_query_pin()
    })
    .join()
    .unwrap();
    std::thread::spawn(move || {
        setup();
        let own = (rs.read(), admit.read());
        assert_eq!(own, (true, guc_tables::consts::GRAVITY_ADMIT_GRAPH), "a fresh thread holds the boot values");
        for round in 0..2 {
            {
                let _claim = crate::carry::arm_claim(&leader).unwrap();
                assert_eq!((rs.read(), admit.read()), (false, shape), "round {round}: the leader's values");
                {
                    let _nested = crate::carry::arm_claim(&other).unwrap();
                    assert_eq!((rs.read(), admit.read()), (true, shape), "round {round}: the nested session's SET, the outer's rest");
                }
                assert_eq!((rs.read(), admit.read()), (false, shape), "round {round}: the nested claim's end put the outer's back");
            }
            assert_eq!((rs.read(), admit.read()), own, "round {round}: the claim's end put the thread's own back");
        }
        // a value set on the thread around its store survives a claim that changes it
        rs.write(false);
        drop(crate::carry::arm_claim(&other).unwrap());
        assert!(!rs.read());
        rs.write(true);
    })
    .join()
    .unwrap();
}

/// pgrust.point_path is one USERSET option whose value is its session's: a SET in one
/// session leaves another's value alone, in both directions, and SHOW reads each session's own.
#[test]
fn point_path_set_is_per_session() {
    use guc_tables::consts::{POINT_PATH_OFF, POINT_PATH_STRICT};
    let pp = &guc_tables::vars::pgrust_point_path;
    let gate = std::sync::Arc::new(std::sync::Barrier::new(2));
    let g = gate.clone();
    let a = std::thread::spawn(move || {
        setup();
        assert_eq!(set_session("pgrust.point_path", Some("strict")).unwrap(), 1);
        g.wait();
        g.wait();
        (pp.read(), guc_tables::backing::pgrust_point_path(), show("pgrust.point_path"))
    });
    let b = std::thread::spawn(move || {
        setup();
        gate.wait();
        let before = (pp.read(), show("pgrust.point_path"));
        assert_eq!(set_session("pgrust.point_path", Some("off")).unwrap(), 1);
        gate.wait();
        (before, guc_tables::backing::pgrust_point_path())
    });
    let (before, b_after) = b.join().unwrap();
    assert!(before.0 != POINT_PATH_STRICT && before.1.as_deref() != Some("strict"), "another session's SET left the boot value: {before:?}");
    assert_eq!(b_after, POINT_PATH_OFF);
    assert_eq!(a.join().unwrap(), (POINT_PATH_STRICT, POINT_PATH_STRICT, Some("strict".to_string())), "its own SET, after the other session's");
}

/// pgrust.point_noplan: on at boot (D4550), a USERSET option whose value is its session's,
/// as pgrust.point_path.
#[test]
fn point_noplan_set_is_per_session() {
    use guc_tables::consts::{POINT_NOPLAN_OFF, POINT_NOPLAN_ON};
    let a = std::thread::spawn(|| {
        setup();
        let boot = (guc_tables::backing::pgrust_point_noplan(), show("pgrust.point_noplan"));
        assert_eq!(set_session("pgrust.point_noplan", Some("off")).unwrap(), 1);
        (boot, guc_tables::backing::pgrust_point_noplan(), show("pgrust.point_noplan"))
    });
    let b = std::thread::spawn(|| {
        setup();
        guc_tables::backing::pgrust_point_noplan()
    });
    let (boot, set, shown) = a.join().unwrap();
    assert_eq!(boot, (POINT_NOPLAN_ON, Some("on".to_string())), "on at boot");
    assert_eq!((set, shown), (POINT_NOPLAN_OFF, Some("off".to_string())));
    assert_eq!(b.join().unwrap(), POINT_NOPLAN_ON, "another session's SET leaves this one's value alone");
}

/// pgrust.point_write and pgrust.point_keep: on at boot (D4572), USERSET options whose values are their session's.
#[test]
fn point_write_and_keep_on_at_boot_set_per_session() {
    let a = std::thread::spawn(|| {
        setup();
        let boot = (guc_tables::backing::pgrust_point_write(), show("pgrust.point_write"), guc_tables::backing::pgrust_point_keep(), show("pgrust.point_keep"));
        assert_eq!(set_session("pgrust.point_write", Some("off")).unwrap(), 1);
        assert_eq!(set_session("pgrust.point_keep", Some("off")).unwrap(), 1);
        (boot, guc_tables::backing::pgrust_point_write(), guc_tables::backing::pgrust_point_keep())
    });
    let b = std::thread::spawn(|| {
        setup();
        (guc_tables::backing::pgrust_point_write(), guc_tables::backing::pgrust_point_keep())
    });
    let (boot, write, keep) = a.join().unwrap();
    assert_eq!(boot, (true, Some("on".to_string()), true, Some("on".to_string())), "both on at boot");
    assert_eq!((write, keep), (false, false));
    assert_eq!(b.join().unwrap(), (true, true), "another session's SETs leave this one's values alone");
}
