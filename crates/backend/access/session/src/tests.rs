use std::collections::HashSet;
use std::sync::Once;

use types_core::{InvalidOid, BOOTSTRAP_SUPERUSERID};
use types_error::{PgError, ERRCODE_QUERY_CANCELED, ERROR};
use types_guc::{GucContext::PGC_USERSET, GucSource::PGC_S_SESSION};

use super::*;

fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "true" | "on" | "yes" | "1" => Some(true),
        "false" | "off" | "no" | "0" => Some(false),
        _ => None,
    }
}

fn setup() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        guc_tables::init_seams();
        init_small::init_seams();
        elog::init_seams();
        guc::init_seams();
        xact_seams::is_in_parallel_mode::set(|| false);
        scalar_seams::parse_bool::set(parse_bool);
        aclchk_seams::pg_parameter_aclcheck_set::set(|_, _| Ok(true));
        mbutils_seams::get_database_encoding::set(|| 6);
        timestamp_seams::get_current_timestamp::set(|| 42);
    });
    guc::store::initialize_guc_options().unwrap();
    init_small::globals::SetMyDatabaseId(42);
    init_small::globals::SetMyDatabaseTableSpace(1663);
    init_small::globals::SetInterruptPending(false);
    init_small::globals::SetQueryCancelPending(false);
    init_small::globals::SetProcDiePending(false);
    init_small::globals::SetInterruptHoldoffCount(0);
    init_small::globals::SetQueryCancelHoldoffCount(0);
    init_small::globals::SetCritSectionCount(0);
    set_state(BOOTSTRAP_SUPERUSERID, 4096, (InvalidOid, InvalidOid));
}

fn identity(user: Oid) -> miscinit::SessionIdentityState {
    miscinit::SessionIdentityState {
        authenticated_user_id: user,
        session_user_id: user,
        outer_user_id: user,
        current_user_id: user,
        system_user: Some("trust:test"),
        session_user_is_superuser: user == BOOTSTRAP_SUPERUSERID,
        security_restriction_context: if user == 23 {
            types_core::SECURITY_NOFORCE_RLS
        } else {
            0
        },
        set_role_is_active: false,
    }
}

fn set_state(user: Oid, work_mem: i32, temp: (Oid, Oid)) {
    miscinit::ReplaceSessionIdentityState(identity(user));
    catalog_namespace::ReplaceTempNamespaceState(temp.0, temp.1);
    guc::ResetAllOptions();
    guc::SetConfigOption(
        "work_mem",
        Some(&work_mem.to_string()),
        PGC_USERSET,
        PGC_S_SESSION,
    )
    .unwrap();
    miscinit::ReplaceSessionIdentityState(identity(user));
}

fn install_context(context: &SessionContext) {
    guc::store::replace_exact_guc_state(&context.gucs);
    catalog_namespace::ReplaceSessionNamespaceState(&context.namespace);
    miscinit::ReplaceSessionIdentityState(context.identity);
    CURRENT_SESSION.set(context.session_exists);
}

fn assert_state(user: Oid, work_mem: i32, temp: (Oid, Oid)) {
    assert_eq!(miscinit::CaptureSessionIdentityState(), identity(user));
    assert_eq!(init_small::globals::work_mem(), work_mem);
    assert_eq!(catalog_namespace::GetTempNamespaceState(), temp);
}

fn contexts() -> (SessionContext, SessionContext, SessionContext) {
    set_state(BOOTSTRAP_SUPERUSERID, 4096, (InvalidOid, InvalidOid));
    let base = SessionContext::capture();
    InitializeSession().unwrap();
    set_state(22, 8192, (2200, 2201));
    let a = SessionContext::capture();
    set_state(23, 16384, (2300, 2301));
    let b = SessionContext::capture();
    install_context(&base);
    (base, a, b)
}

#[test]
fn manifest_is_unique_exhaustive_and_phase0_actions_are_explicit() {
    let expected: HashSet<_> = [
        EnvelopeMemberId::DatabaseIdentity,
        EnvelopeMemberId::DatabasePaths,
        EnvelopeMemberId::ProcessIdentity,
        EnvelopeMemberId::SessionLifecycle,
        EnvelopeMemberId::UserIdentity,
        EnvelopeMemberId::TempNamespace,
        EnvelopeMemberId::SearchPath,
        EnvelopeMemberId::SnapshotState,
        EnvelopeMemberId::TransactionState,
        EnvelopeMemberId::GucStore,
        EnvelopeMemberId::GucFlatBackings,
        EnvelopeMemberId::GucNesting,
        EnvelopeMemberId::ResourceOwnerCells,
        EnvelopeMemberId::ResourceOwnerArena,
        EnvelopeMemberId::ErrorStack,
        EnvelopeMemberId::ErrorCallbacks,
        EnvelopeMemberId::InterruptPending,
        EnvelopeMemberId::InterruptHoldoffs,
        EnvelopeMemberId::Catcache,
        EnvelopeMemberId::Relcache,
        EnvelopeMemberId::Typcache,
        EnvelopeMemberId::Plancache,
        EnvelopeMemberId::InvalidationCallbacks,
        EnvelopeMemberId::InvalidationMessages,
        EnvelopeMemberId::PendingInvalidations,
        EnvelopeMemberId::SyscacheArrays,
        EnvelopeMemberId::Relmapper,
        EnvelopeMemberId::Partcache,
        EnvelopeMemberId::TsCache,
        EnvelopeMemberId::EventCache,
    ]
    .into_iter()
    .collect();
    let mut ids = HashSet::new();
    let mut names = HashSet::new();
    for member in SESSION_ENVELOPE_MANIFEST {
        assert!(
            ids.insert(member.id),
            "duplicate manifest id: {:?}",
            member.id
        );
        assert!(
            names.insert(member.name),
            "duplicate manifest name: {}",
            member.name
        );
        assert!(
            !member.declaration.is_empty(),
            "unlocated TLS member: {}",
            member.name
        );
        match member.phase0 {
            Phase0Action::CaptureApply => assert_eq!(member.kind, EnvelopeBindKind::SwapRoot),
            Phase0Action::RestoreScalar | Phase0Action::RequireSameDatabase => {
                assert_eq!(member.kind, EnvelopeBindKind::ScalarRestore)
            }
            Phase0Action::Drain => assert_eq!(member.kind, EnvelopeBindKind::DrainSameDatabase),
            Phase0Action::CheckEmpty => assert_eq!(member.kind, EnvelopeBindKind::MustBeEmpty),
            Phase0Action::Refuse => assert!(
                member.blocker.is_some(),
                "refusal without blocker: {}",
                member.name
            ),
        }
    }
    assert_eq!(ids, expected);
}

/// The statics of every `thread_local!` block in `src`, in order (`static NAME:` at the block's top level).
fn tls_names(src: &str) -> Vec<String> {
    let b = src.as_bytes();
    let mut names = Vec::new();
    let mut resume = 0;
    let mut start = 0;
    while start < b.len() {
        let end = src[start..].find('\n').map_or(b.len(), |i| start + i);
        let line = src[start..end].trim_start();
        if start >= resume
            && ["thread_local!", "std::thread_local!", "::std::thread_local!"].iter().any(|p| line.starts_with(p))
        {
            let mut j = start + src[start..].find('!').unwrap() + 1;
            while j < b.len() && !b"({[".contains(&b[j]) {
                j += 1;
            }
            let mut depth = 0;
            while j < b.len() {
                let c = b[j];
                if b[j..].starts_with(b"//") {
                    j = src[j..].find('\n').map_or(b.len(), |i| j + i);
                    continue;
                }
                if c == b'"' {
                    j += 1;
                    while j < b.len() && b[j] != b'"' {
                        j += if b[j] == b'\\' { 2 } else { 1 };
                    }
                    j += 1;
                    continue;
                }
                if c == b'\'' && j + 2 < b.len() && b[j + 2] == b'\'' {
                    j += 3;
                    continue;
                }
                if b"({[".contains(&c) {
                    depth += 1;
                } else if b")}]".contains(&c) {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                } else if depth == 1
                    && b[j..].starts_with(b"static")
                    && (j == 0 || !(b[j - 1].is_ascii_alphanumeric() || b[j - 1] == b'_' || b[j - 1] == b'\''))
                {
                    let mut k = j + 6;
                    let ws = k;
                    while k < b.len() && b[k].is_ascii_whitespace() {
                        k += 1;
                    }
                    let id = k;
                    if k > ws && k < b.len() && (b[k].is_ascii_alphabetic() || b[k] == b'_' || b[k] == b'$') {
                        k += 1;
                        while k < b.len() && (b[k].is_ascii_alphanumeric() || b[k] == b'_') {
                            k += 1;
                        }
                        let name = &src[id..k];
                        while k < b.len() && b[k].is_ascii_whitespace() {
                            k += 1;
                        }
                        if k < b.len() && b[k] == b':' {
                            names.push(name.to_string());
                            j = k + 1;
                            continue;
                        }
                    }
                }
                j += 1;
            }
            resume = j;
        }
        start = end + 1;
    }
    names
}

/// Every `(path under crates/, static)` a `thread_local!` declares below `dir`.
fn tls_census(crates: &std::path::Path, dir: &std::path::Path, found: &mut std::collections::BTreeSet<(String, String)>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            tls_census(crates, &path, found);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            let source = std::fs::read_to_string(&path).unwrap();
            let rel = path.strip_prefix(crates).unwrap().to_string_lossy().replace('\\', "/");
            for name in tls_names(&source) {
                found.insert((rel.clone(), name));
            }
        }
    }
}

/// A new thread-local is classified before it lands: `tls-census.tsv` holds a line per
/// `<path under crates/>\t<static>\t<session | non-session>[\t<note>]`, in any order (the file merges by union).
/// `session` (session envelope state, see SESSION_ENVELOPE_MANIFEST) is only in the session sources below, each of
/// which holds one.
#[test]
fn tls_source_census_and_session_surface_are_pinned() {
    let dir = std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)");
    let crates = std::path::Path::new(&dir).ancestors().nth(3).unwrap();
    let mut found = std::collections::BTreeSet::new();
    tls_census(crates, crates, &mut found);
    let manifest = std::fs::read_to_string(std::path::Path::new(&dir).join("tls-census.tsv")).unwrap();
    let mut classed = std::collections::BTreeMap::new();
    for (n, line) in manifest.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        assert!(
            fields.len() >= 3 && (fields[2] == "session" || fields[2] == "non-session"),
            "tls-census.tsv:{}: <path>\\t<static>\\t<session | non-session>[\\t<note>]",
            n + 1
        );
        let key = (fields[0].to_string(), fields[1].to_string());
        if let Some(class) = classed.insert(key, fields[2]) {
            assert_eq!(class, fields[2], "tls-census.tsv:{}: two classes for {} {}", n + 1, fields[0], fields[1]);
        }
    }
    let missing: Vec<String> = found
        .iter()
        .filter(|key| !classed.contains_key(*key))
        .map(|(path, name)| format!("{path}\t{name}\t<session | non-session>"))
        .collect();
    assert!(
        missing.is_empty(),
        "unclassified thread-locals: add each to session/tls-census.tsv as session envelope state or non-session TLS:\n{}",
        missing.join("\n")
    );
    let stale: Vec<String> = classed
        .keys()
        .filter(|key| !found.contains(*key))
        .map(|(path, name)| format!("{path}\t{name}"))
        .collect();
    assert!(stale.is_empty(), "tls-census.tsv lines whose thread-local is gone:\n{}", stale.join("\n"));
    let session_sources = [
        "backend/access/session/src/lib.rs",
        "backend/utils/init/init_small/src/globals.rs",
        "backend/utils/init/miscinit/src/userid.rs",
        "backend/catalog/catalog_namespace/src/lib.rs",
        "backend/catalog/catalog_namespace/src/path.rs",
        "backend/utils/time/snapmgr/src/lib.rs",
        "backend/access/transam/xact/src/state.rs",
        "backend/access/transam/xact/src/engine.rs",
        "backend/access/transam/xact/src/lib.rs",
        "backend/utils/misc/guc/src/store.rs",
        "backend/utils/misc/guc/src/lib.rs",
        "backend/utils/misc/guc_tables/src/session.rs",
        "backend/utils/resowner/resowner/src/lib.rs",
        "backend/utils/error/elog/src/stack.rs",
        "backend/storage/lmgr/lmgr_proc/src/lib.rs",
        "backend/utils/cache/catcache/src/lib.rs",
        "backend/utils/cache/catcache/src/graph.rs",
        "backend/utils/cache/relcache/src/lib.rs",
        "backend/utils/cache/typcache/src/lib.rs",
        "backend/utils/cache/plancache/src/lib.rs",
        "backend/utils/cache/inval/src/lib.rs",
        "backend/utils/cache/cache_syscache/src/lib.rs",
        "backend/utils/cache/relmapper/src/lib.rs",
        "backend/utils/cache/partcache/src/lib.rs",
        "backend/utils/cache/ts_cache/src/lib.rs",
        "backend/utils/cache/cache_evtcache/src/lib.rs",
    ];
    for ((path, name), class) in &classed {
        assert!(
            *class != "session" || session_sources.contains(&path.as_str()),
            "{path} {name}: session TLS lives in the session sources (and SESSION_ENVELOPE_MANIFEST)"
        );
    }
    for path in session_sources {
        assert!(
            classed.iter().any(|((p, _), class)| p == path && *class == "session"),
            "session source {path} holds no session thread-local"
        );
    }
}

#[test]
fn nested_bind_restores_roots_and_scalars_in_lifo_order() {
    std::thread::spawn(|| {
        setup();
        let (_base, a, b) = contexts();
        let mut drains = 0;

        let outer = bind_session_envelope_with(&a, || {
            drains += 1;
            Ok(())
        })
        .unwrap();
        assert_state(22, 8192, (2200, 2201));
        assert!(CurrentSessionExists());
        assert!(!SessionEnvelopeBoundaryClean());
        assert_eq!(
            catalog_namespace::CaptureSessionNamespaceState(),
            a.namespace
        );

        let inner = bind_session_envelope_with(&b, || {
            drains += 1;
            Ok(())
        })
        .unwrap();
        assert_state(23, 16384, (2300, 2301));
        assert_eq!(
            catalog_namespace::CaptureSessionNamespaceState(),
            b.namespace
        );
        drop(inner);
        assert_state(22, 8192, (2200, 2201));
        assert_eq!(
            catalog_namespace::CaptureSessionNamespaceState(),
            a.namespace
        );
        drop(outer);
        assert_state(BOOTSTRAP_SUPERUSERID, 4096, (InvalidOid, InvalidOid));
        assert!(!CurrentSessionExists());
        assert!(SessionEnvelopeBoundaryClean());
        assert_eq!(drains, 2);
    })
    .join()
    .unwrap();
}

#[test]
fn panic_and_cancel_paths_restore_without_clearing_cancel() {
    std::thread::spawn(|| {
        setup();
        let (_base, a, _) = contexts();

        let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _binding = bind_session_envelope_with(&a, || Ok(())).unwrap();
            assert_state(22, 8192, (2200, 2201));
            panic!("task panic");
        }));
        assert!(panic_result.is_err());
        assert_state(BOOTSTRAP_SUPERUSERID, 4096, (InvalidOid, InvalidOid));

        let binding = bind_session_envelope_with(&a, || Ok(())).unwrap();
        init_small::globals::SetQueryCancelPending(true);
        let cancelled: PgResult<()> = Err(PgError::new(ERROR, "cancelled")
            .with_sqlstate(ERRCODE_QUERY_CANCELED)
            .into());
        binding.finish().unwrap();
        assert!(cancelled.is_err());
        assert_state(BOOTSTRAP_SUPERUSERID, 4096, (InvalidOid, InvalidOid));
        assert!(init_small::globals::QueryCancelPending());
        init_small::globals::SetQueryCancelPending(false);
    })
    .join()
    .unwrap();
}

#[test]
fn cross_database_and_unimplemented_transaction_state_are_refused_before_drain() {
    std::thread::spawn(|| {
        setup();
        let (base, mut target, _) = contexts();
        let mut drains = 0;
        let error = bind_session_envelope_with(&base, || {
            drains += 1;
            Ok(())
        })
        .err()
        .expect("uninitialized target session must fail");
        assert!(error.message().contains("initialized target session"));
        assert_eq!(drains, 0);

        target.database_id = 43;
        let error = bind_session_envelope_with(&target, || {
            drains += 1;
            Ok(())
        })
        .err()
        .expect("cross-database bind must fail");
        assert!(error.message().contains("cross-database"));
        assert_eq!(drains, 0);
        assert_state(BOOTSTRAP_SUPERUSERID, 4096, (InvalidOid, InvalidOid));

        target.database_id = 42;
        target.xact_nest_level = 1;
        target.transaction_active = true;
        let error = bind_session_envelope_with(&target, || {
            drains += 1;
            Ok(())
        })
        .err()
        .expect("transaction-bearing bind must fail");
        assert!(error.message().contains("transaction/snapshot root"));
        assert_eq!(drains, 0);

        target.xact_nest_level = 0;
        target.transaction_active = false;
        target.guc_nest_level = 1;
        let error = bind_session_envelope_with(&target, || {
            drains += 1;
            Ok(())
        })
        .err()
        .expect("nested GUC target must fail");
        assert!(error.message().contains("SET LOCAL"));
        assert_eq!(drains, 0);

        target.guc_nest_level = 0;
        target.pending_invalidations = true;
        let error = bind_session_envelope_with(&target, || {
            drains += 1;
            Ok(())
        })
        .err()
        .expect("uncommitted invalidations must fail");
        assert!(error.message().contains("uncommitted invalidations"));
        assert_eq!(drains, 0);

        target.pending_invalidations = false;
        target.data_dir = Some("/other-cluster");
        let error = bind_session_envelope_with(&target, || {
            drains += 1;
            Ok(())
        })
        .err()
        .expect("path mismatch must fail");
        assert!(error.message().contains("path identity"));
        assert_eq!(drains, 0);
    })
    .join()
    .unwrap();
}

#[test]
fn drain_failure_and_dirty_exit_restore_without_partial_binding() {
    std::thread::spawn(|| {
        setup();
        let (_base, target, _) = contexts();
        let error = bind_session_envelope_with(&target, || {
            Err(PgError::new(ERROR, "invalidation drain failed").into())
        })
        .err()
        .expect("drain failure must refuse binding");
        assert!(error.message().contains("drain failed"));
        assert_state(BOOTSTRAP_SUPERUSERID, 4096, (InvalidOid, InvalidOid));
        assert_eq!(ENVELOPE_DEPTH.get(), 0);

        let binding = bind_session_envelope_with(&target, || Ok(())).unwrap();
        init_small::globals::SetCritSectionCount(1);
        let error = binding.finish().expect_err("dirty exit must fail");
        assert!(error.message().contains("holdoff"));
        assert_state(BOOTSTRAP_SUPERUSERID, 4096, (InvalidOid, InvalidOid));
        assert_eq!(ENVELOPE_DEPTH.get(), 0);
        init_small::globals::SetCritSectionCount(0);
    })
    .join()
    .unwrap();
}

#[test]
fn dirty_error_resource_holdoff_and_pending_cancel_boundaries_refuse() {
    std::thread::spawn(|| {
        setup();
        let (_base, target, _) = contexts();

        let callback = elog::push_emit_context_callback(Box::new(|_| {}));
        let error = bind_session_envelope_with(&target, || Ok(()))
            .err()
            .expect("dirty error state");
        assert!(error.message().contains("error or callback"));
        elog::pop_emit_context_callback(callback);

        let owner = resowner::ResourceOwnerCreate(
            types_resowner::ResourceOwner::NULL,
            "session envelope dirty-boundary test",
        )
        .unwrap();
        let error = bind_session_envelope_with(&target, || Ok(()))
            .err()
            .expect("dirty resource state");
        assert!(error.message().contains("resource-owner"));
        resowner::ResourceOwnerDelete(owner);

        init_small::globals::SetCritSectionCount(1);
        let error = bind_session_envelope_with(&target, || Ok(()))
            .err()
            .expect("dirty holdoff state");
        assert!(error.message().contains("holdoff"));
        init_small::globals::SetCritSectionCount(0);

        init_small::globals::SetProcDiePending(true);
        init_small::globals::SetInterruptPending(true);
        let error = bind_session_envelope_with(&target, || Ok(()))
            .err()
            .expect("pending die");
        assert!(error.message().contains("cancellation is pending"));
        init_small::globals::SetProcDiePending(false);
        init_small::globals::SetInterruptPending(false);
    })
    .join()
    .unwrap();
}

#[test]
fn stale_pending_cancel_is_discarded_at_statement_boundary() {
    std::thread::spawn(|| {
        setup();
        let (_base, target, _) = contexts();

        init_small::globals::SetInterruptPending(true);
        init_small::globals::SetQueryCancelPending(true);
        bind_session_envelope_with(&target, || Ok(()))
            .expect("stale cancel must be discarded, not refused")
            .finish()
            .unwrap();
        assert!(!init_small::globals::QueryCancelPending());
        assert!(!init_small::globals::InterruptPending());

        init_small::globals::SetInterruptPending(true);
        init_small::globals::SetQueryCancelPending(true);
        assert_eq!(SessionEnvelopeBoundaryIssue(), None);
        assert!(!init_small::globals::QueryCancelPending());
        assert!(!init_small::globals::InterruptPending());

        init_small::globals::SetInterruptPending(true);
        init_small::globals::SetQueryCancelPending(true);
        assert_eq!(SessionEnvelopeBoundaryIssueForRetainedBind(), None);
        assert!(!init_small::globals::QueryCancelPending());
        assert!(!init_small::globals::InterruptPending());

        init_small::globals::SetInterruptPending(true);
        assert_eq!(SessionEnvelopeBoundaryIssue(), None);
        assert!(!init_small::globals::InterruptPending());
    })
    .join()
    .unwrap();
}

// D334: the stale-cancel discard at a statement boundary clears the abort-class byte (AbortPending) with the cancel:
// a pg_cancel_backend racing the boundary raised it beside InterruptPending, and left set it outlived its cancel into
// the next statement, whose Gravity kernel reads it at its first window and ends that statement with 57014. A pending
// die is never discarded, and its byte stays for the kernels.
#[test]
fn stale_cancel_discard_clears_the_abort_byte() {
    std::thread::spawn(|| {
        setup();
        let _contexts = contexts();

        init_small::globals::SetInterruptPending(true);
        init_small::globals::SetQueryCancelPending(true);
        init_small::globals::SetAbortPending(true);
        let _ = SessionEnvelopeBoundaryIssue();
        assert!(!init_small::globals::QueryCancelPending());
        assert!(!init_small::globals::InterruptPending());
        assert!(!init_small::globals::AbortPending(), "the stale cancel's abort byte must go with it");

        init_small::globals::SetInterruptPending(true);
        init_small::globals::SetProcDiePending(true);
        init_small::globals::SetAbortPending(true);
        let _ = SessionEnvelopeBoundaryIssue();
        assert!(init_small::globals::ProcDiePending());
        assert!(init_small::globals::AbortPending(), "a pending die keeps its byte");
        init_small::globals::SetProcDiePending(false);
        init_small::globals::SetInterruptPending(false);
        init_small::globals::SetAbortPending(false);
    })
    .join()
    .unwrap();
}

// The discard is ONLY for cancels: ProcDiePending stays latched and keeps
// refusing (C: die is never discarded), and a held-off boundary is left
// untouched — the holdoff itself is the boundary defect there.
#[test]
fn proc_die_and_held_off_cancel_are_never_discarded() {
    std::thread::spawn(|| {
        setup();
        let (_base, target, _) = contexts();

        init_small::globals::SetInterruptPending(true);
        init_small::globals::SetProcDiePending(true);
        init_small::globals::SetQueryCancelPending(true);
        let error = bind_session_envelope_with(&target, || Ok(()))
            .err()
            .expect("pending die must refuse");
        assert!(error.message().contains("cancellation is pending"));
        assert!(init_small::globals::ProcDiePending());
        assert!(init_small::globals::QueryCancelPending());
        assert!(init_small::globals::InterruptPending());
        init_small::globals::SetProcDiePending(false);

        init_small::globals::SetQueryCancelHoldoffCount(1);
        let error = bind_session_envelope_with(&target, || Ok(()))
            .err()
            .expect("held-off cancel must refuse");
        assert!(error.message().contains("cancellation is pending"));
        assert!(init_small::globals::QueryCancelPending());
        assert!(init_small::globals::InterruptPending());
        init_small::globals::SetQueryCancelHoldoffCount(0);

        init_small::globals::SetQueryCancelPending(false);
        init_small::globals::SetInterruptPending(false);
    })
    .join()
    .unwrap();
}
