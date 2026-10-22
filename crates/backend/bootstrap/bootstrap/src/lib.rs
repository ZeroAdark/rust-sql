//! Design memo: docs/design/native-initdb.md.

#![allow(non_snake_case)]

pub mod bki;
pub mod exec;
pub mod typinfo;

use types_error::{PgResult, ERRCODE_SYNTAX_ERROR, PgError};
use types_guc::{GucContext, GucSource};

const PROGNAME: &str = "postgres";

pub fn BootstrapModeMain(argv: &[String], check_only: bool) -> ! {
    // unwind-ok: log-then-die
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> core::convert::Infallible {
        let err = match bootstrap_main_inner(argv, check_only) {
            Ok(never) => match never {},
            Err(err) => err,
        };
        elog::emit_unhandled_error_report(&err);
        ipc::proc_exit(1, init_small::globals::MyProcPid())
    }));
    let payload = match outcome {
        Ok(never) => match never {},
        Err(payload) => payload,
    };
    match payload.downcast_ref::<ipc::ProcExitThread>() {
        Some(p) => std::process::exit(p.code),
        None => std::panic::resume_unwind(payload),
    }
}

fn set_argv(name: &str, value: &str) -> PgResult<()> {
    guc::SetConfigOption(name, Some(value), GucContext::PGC_POSTMASTER, GucSource::PGC_S_ARGV)
}

fn usage_exit(progname: &str) -> ! {
    eprintln!("Try \"{progname} --help\" for more information.");
    std::process::exit(1)
}

fn bootstrap_main_inner(argv: &[String], check_only: bool) -> PgResult<core::convert::Infallible> {
    assert!(!init_small::globals::IsUnderPostmaster());
    let argv0 = argv.first().map(|s| s.as_str()).unwrap_or(PROGNAME);
    let progname = argv0.rsplit('/').next().unwrap_or(argv0).to_string();

    miscinit::InitStandaloneProcess(argv0)?;
    guc_seams::initialize_guc_options::call()?;
    stack_depth::adjust_max_stack_depth_from_rlimit()?;

    let mut user_d_option: Option<String> = None;
    let mut data_checksum_version: u32 = 0;
    let mut args = argv.iter().skip(2).peekable();
    while let Some(arg) = args.next() {
        let Some(rest) = arg.strip_prefix('-') else {
            eprintln!("{progname}: invalid command-line arguments");
            std::process::exit(1);
        };
        let (opt, inline_val): (char, Option<String>) = if let Some(long) = rest.strip_prefix('-') {
            ('-', Some(long.to_string()))
        } else {
            let mut cs = rest.chars();
            let o = cs.next().unwrap_or('\0');
            let tail: String = cs.collect();
            (o, if tail.is_empty() { None } else { Some(tail) })
        };
        let mut val = || -> String {
            inline_val.clone().or_else(|| args.next().cloned()).unwrap_or_else(|| usage_exit(&progname))
        };
        match opt {
            'B' => set_argv("shared_buffers", &val())?,
            'c' | '-' => {
                let optarg = val();
                // bootstrap.c:233: a misplaced dispatch option.
                if opt == '-'
                    && matches!(
                        optarg.as_str(),
                        "check" | "boot" | "forkchild" | "describe-config" | "single"
                    )
                {
                    return Err(Box::new(
                        PgError::error(format!("--{optarg} must be first argument"))
                            .with_sqlstate(ERRCODE_SYNTAX_ERROR),
                    ));
                }
                let (name, value) = match optarg.split_once('=') {
                    Some((n, v)) => (n.to_string(), Some(v.to_string())),
                    None => (optarg.clone(), None),
                };
                let Some(value) = value else {
                    return Err(Box::new(
                        PgError::error(if opt == '-' {
                            format!("--{optarg} requires a value")
                        } else {
                            format!("-c {optarg} requires a value")
                        })
                        .with_sqlstate(ERRCODE_SYNTAX_ERROR),
                    ));
                };
                set_argv(&name, &value)?;
            }
            'D' => user_d_option = Some(val()),
            'd' => {
                let level = format!("debug{}", val());
                set_argv("log_min_messages", &level)?;
                set_argv("client_min_messages", &level)?;
            }
            'F' => set_argv("fsync", "false")?,
            'k' => data_checksum_version = transam_xlog::PG_DATA_CHECKSUM_VERSION,
            'r' => {
                let _ = val();
            }
            'X' => guc::SetConfigOption(
                "wal_segment_size",
                Some(&val()),
                GucContext::PGC_INTERNAL,
                GucSource::PGC_S_DYNAMIC_DEFAULT,
            )?,
            _ => usage_exit(&progname),
        }
    }

    if !guc_seams::select_config_files::call(user_d_option.as_deref(), &progname)? {
        ipc::proc_exit(1, init_small::globals::MyProcPid());
    }
    miscinit_seams::check_data_dir::call()?;
    miscinit::ChangeToDataDir()?;
    miscinit::CreateDataDirLockFile(false)?;

    miscinit::SetProcessingMode(types_core::init::ProcessingMode::BootstrapProcessing);
    bootstrap_seams::boot_get_type_io_data::set(exec::boot_get_type_io_data);
    miscinit::SetIgnoreSystemIndexes(true);

    postinit::InitializeMaxBackends()?;
    pmchild_seams::init_postmaster_child_slots::call();
    let fastpath_groups = postinit::InitializeFastPathLocks();
    ipci_seams::initialize_shmem_gucs::call(fastpath_groups)?;
    transam_xlog_seams::initialize_wal_consistency_checking::call()?;
    ipci_seams::create_shared_memory_and_semaphores::call(fastpath_groups)?;
    fd::set_max_safe_fds(1, 1)?;

    // bootstrap.c:349: --check stops once startup has validated the
    // configuration, the data directory and the shared-memory sizing.
    if check_only {
        miscinit::SetProcessingMode(types_core::init::ProcessingMode::NormalProcessing);
        ipc::proc_exit(0, init_small::globals::MyProcPid());
    }
    postmaster_seams::set_pg_start_time::call(timestamp_seams::get_current_timestamp::call());

    lmgr_proc::InitProcess(miscinit::GetMyBackendType())?;
    postinit::BaseInit()?;
    libpq_pqsignal::unblock_signals();

    transam_xlog::BootStrapXLOG(data_checksum_version)?;
    multixact::MultiXactSetNextMXact(multixact::FirstMultiXactId, 0)?;
    multixact::SetMultiXactIdLimit(multixact::FirstMultiXactId, postinit::TEMPLATE1_DB_OID, true)?;
    commit_ts::SetCommitTsLimit(types_core::InvalidTransactionId, types_core::InvalidTransactionId)?;
    clog::BootStrapCLOG()?;
    commit_ts::BootStrapCommitTs();
    subtrans::BootStrapSUBTRANS()?;
    multixact::BootStrapMultiXact()?;

    let top = mcx::MemoryContext::new("BootstrapModeMain");
    postinit::InitPostgres(top.mcx(), None, types_core::InvalidOid, None, types_core::InvalidOid, 0, None)?;

    let mut script = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut script)
        .map_err(|e| Box::new(PgError::error(format!("could not read bootstrap script: {e}"))))?;
    let stmts = bki::parse(&script)?;

    xact::StartTransactionCommand()?;
    {
        let cx = mcx::MemoryContext::new("bootstrap interpreter");
        let mut boot = exec::Boot::new(cx.mcx());
        for stmt in &stmts {
            boot.run(stmt)?;
        }
        boot.cleanup()?;
    }
    xact::CommitTransactionCommand()?;

    relmapper::RelationMapFinishBootstrap()?;

    ipc::proc_exit(0, init_small::globals::MyProcPid())
}
