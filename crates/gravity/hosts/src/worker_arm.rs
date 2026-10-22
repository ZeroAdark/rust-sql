use std::cell::Cell;

use types_error::PgResult;
use types_resowner::ResourceOwner;

pub use gravity_fn::typesem::collation::{arm_database_encoding, Armed, StatementArm};

pub type ArmFn = fn() -> PgResult<()>;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    /// The standing owner, made current for each claim (`arm_once`) and parked at the claim's end (`Carried`): a bound
    /// serve between two claims (VACUUM's index pass and lazy-scan morsels, an nbtsort build) binds only with no owner
    /// current (D3401), and runs its own transaction's.
    static OWNER: Cell<ResourceOwner> = const { Cell::new(ResourceOwner::NULL) };
    /// D3401: the owner `arm_once` displaced for this claim (`None`: it displaced none), taken by the claim's carry and
    /// put back when the carry drops.
    static DISPLACED: Cell<Option<ResourceOwner>> = const { Cell::new(None) };
}

/// The standing owner ("GravityWorker"), made current. Registered as the thread's standing owner: parked and holding
/// nothing it is no leak at a bound serve's boundary (resowner `SetStandingResourceOwner`).
pub fn arm_owner() -> PgResult<()> {
    let owner = resowner::ResourceOwnerCreate(ResourceOwner::NULL, "GravityWorker")?;
    OWNER.with(|o| o.set(owner));
    resowner::SetStandingResourceOwner(owner);
    resowner::SetCurrentResourceOwner(owner);
    Ok(())
}

/// Once per pool thread: a standing resource owner and file access (the PGPROC is the rtpool's).
pub fn arm_worker() -> PgResult<()> {
    arm_owner()?;
    // once per thread, whoever came first: a pool worker's standing PGPROC bring-up already ran
    if !fd::FileAccessReady() {
        fd::InitFileAccess();
        if aio_seams::pgaio_init_backend::is_installed() {
            aio_seams::pgaio_init_backend::call()?;
        }
    }
    if !fd::TempFileAccessReady() {
        fd::InitTemporaryFileAccess()?;
    }
    Ok(())
}

/// D2572: whether this thread's session has a `temp_file_limit` (a pool claim reads its leader's, `SessionCarry`): a sort
/// run is written compressed only then (driver runcodec), the one case its temp bytes decide an error.
pub fn temp_file_limited() -> bool {
    guc_tables::vars::temp_file_limit.read() >= 0
}

/// D2572-d: this thread's session's `temp_file_limit` in bytes (`None` without one; a pool claim reads its leader's): a
/// sort run far under it is written raw (driver runcodec `raw_fits`), since its bytes cannot decide a 53400 there.
pub fn temp_file_limit_bytes() -> Option<u64> {
    let kb = guc_tables::vars::temp_file_limit.read();
    (kb >= 0).then(|| (kb as u64).saturating_mul(1024))
}

/// C7 (b): one kind of session-derived thread state a pool claim takes from its leader, as PostgreSQL's parallel worker
/// restores its leader's state piece by piece (ParallelWorkerMain): captured on the leader at submit (`Lead`), armed on
/// the claim's thread for the claim (`Armed`, whose drop puts the thread's own back). Every member is a row of
/// `claim_registry!` below; the thread-state registry (lint/THREAD-STATE, rule RT) names the member of each thread-local a
/// claim reads (`claim_census_is_pinned`).
trait ClaimState {
    /// The member's name in the thread-state registry's claim rows.
    const NAME: &'static str;
    /// The thread-locals it carries (`<path under crates/>\t<static>`, session tls-census.tsv rows).
    const STATICS: &'static [&'static str];
    type Lead: Clone + Send + Sync;
    type Armed;
    fn capture() -> Self::Lead;
    fn arm(lead: &Self::Lead) -> PgResult<Self::Armed>;
}

/// The registry: `SessionCarry` holds each member's capture, `SessionCarry::arm` arms them in the order listed, and the
/// claim's end (`Carried`, an unwind too) disarms them in the reverse order, as does a failed arm the ones before it.
macro_rules! claim_registry {
    (@disarm $s:ident;) => {};
    (@disarm $s:ident; $first:ident $($rest:ident)*) => {
        claim_registry!(@disarm $s; $($rest)*);
        drop($s.$first.take());
    };
    ($($field:ident: $member:ty),* $(,)?) => {
        /// What a pool claim needs of its leader's session (the registry's members), captured once on the leader at
        /// submit; `arm` per claim, nothing per row.
        #[derive(Clone)]
        pub struct SessionCarry {
            $($field: <$member as ClaimState>::Lead,)*
        }

        /// A claim's carry: dropped at the claim's end (an unwind too), it disarms every member, last armed first.
        #[must_use = "the session stays carried only while the guard lives"]
        pub struct Carried {
            $($field: Option<<$member as ClaimState>::Armed>,)*
        }

        impl Drop for Carried {
            fn drop(&mut self) {
                #[cfg(debug_assertions)]
                claim_leave();
                claim_registry!(@disarm self; $($field)*);
            }
        }

        /// Each member's registry name and the thread-locals it carries, in arm order.
        pub const CLAIM_REGISTRY: &[(&str, &[&str])] = &[$((<$member as ClaimState>::NAME, <$member as ClaimState>::STATICS)),*];

        impl SessionCarry {
            /// The leader's session, at submit.
            pub fn capture() -> SessionCarry {
                SessionCarry { $($field: <$member as ClaimState>::capture()),* }
            }

            /// Arm this pool thread with the leader's session until the guard drops.
            pub fn arm(&self) -> PgResult<Carried> {
                $(let $field = <$member as ClaimState>::arm(&self.$field)?;)*
                let carried = Carried { $($field: Some($field)),* };
                #[cfg(debug_assertions)]
                claim_enter();
                Ok(carried)
            }
        }
    };
}

claim_registry! {
    kind: RegionKind,
    acct: MemoryAccount,
    owner: StandingOwner,
    db: Database,
    mem: StatementMemory,
    collations: Collations,
    catalog: Catalog,
    gucs: Gucs,
    gen: ClaimGeneration,
    ts: StartTimestamps,
    temp: TempAccount,
    toast: Toast,
    xmin: TransactionXmin,
}

impl SessionCarry {
    pub fn statement(&self) -> &StatementArm {
        &self.collations
    }
}

/// G1-B: the region kind the leader's submit ran under (its pipeline's breaker's, `gravity_parallel::run_with_late`), so
/// the regions a claim or an owner task of it draws count under that kind.
struct RegionKind;

impl ClaimState for RegionKind {
    const NAME: &'static str = "region_kind";
    const STATICS: &'static [&'static str] = &["_support/memledger/src/lib.rs\tDOOR"];
    type Lead = memledger::Kind;
    type Armed = memledger::KindScope;
    fn capture() -> memledger::Kind {
        memledger::kind()
    }
    fn arm(kind: &memledger::Kind) -> PgResult<memledger::KindScope> {
        Ok(memledger::with_kind(*kind))
    }
}

/// G1-B: the leader's memory ledger account (its backend's), which the claim's memory charges: a pool thread's own
/// account (its pool identity's backend slot) would take a worker's allocations and leave the leader's frees of them
/// below zero. Its pending bytes land on that account at the claim's end.
struct MemoryAccount;

impl ClaimState for MemoryAccount {
    const NAME: &'static str = "memory_account";
    const STATICS: &'static [&'static str] = &["_support/memledger/src/lib.rs\tDOOR"];
    type Lead = memledger::Acct;
    type Armed = memledger::Entered;
    fn capture() -> memledger::Acct {
        memledger::current()
    }
    fn arm(acct: &memledger::Acct) -> PgResult<memledger::Entered> {
        Ok(memledger::charge::enter_as(memledger::charge::context_of(*acct).0))
    }
}

/// D3401: the standing owner `arm_once` made current, parked at the claim's end, the owner it displaced put back.
struct StandingOwner;

impl ClaimState for StandingOwner {
    const NAME: &'static str = "standing_owner";
    const STATICS: &'static [&'static str] = &["gravity/hosts/src/worker_arm.rs\tOWNER", "gravity/hosts/src/worker_arm.rs\tDISPLACED", "backend/utils/resowner/resowner/src/lib.rs\tCURRENT_OWNER"];
    type Lead = ();
    type Armed = Restore;
    fn capture() {}
    fn arm(_: &()) -> PgResult<Restore> {
        Ok(Restore(DISPLACED.with(Cell::take)))
    }
}

/// memfree Part 2: the leader's statement memory ledger (none when pgrust.statement_mem_limit and the cancel are off): the
/// claim's blocks charge it and may fail it with 53200 (the governor's armed rules).
struct StatementMemory;

impl ClaimState for StatementMemory {
    const NAME: &'static str = "statement_memory";
    const STATICS: &'static [&'static str] = &["_support/mcx/src/governor.rs\tLEDGER", "_support/mcx/src/governor.rs\tARMED"];
    type Lead = Option<std::sync::Arc<mcx::governor::StmtMem>>;
    type Armed = mcx::governor::Entered;
    fn capture() -> Self::Lead {
        mcx::governor::current()
    }
    fn arm(mem: &Self::Lead) -> PgResult<mcx::governor::Entered> {
        Ok(mcx::governor::enter(mem.clone()))
    }
}

/// The `StatementArm`: encoding, default locale and database ctype (C7 c), collations, timezone (V4R-452).
struct Collations;

impl ClaimState for Collations {
    const NAME: &'static str = "collations";
    const STATICS: &'static [&'static str] = &[
        "gravity/fn/src/typesem/collation.rs\tCLAIM",
        "backend/utils/adt/pg_locale/src/lib.rs\tARMED_COLLATIONS",
        "backend/utils/adt/pg_locale/src/lib.rs\tDEFAULT_LOCALE",
        "backend/utils/adt/pg_locale/src/lib.rs\tRESOLVED",
        "backend/timezone/localtime/src/globals.rs\tSESSION_TIMEZONE",
        "backend/utils/mb/mbutils/src/lib.rs\tDATABASE_ENCODING",
        "backend/utils/adt/pg_locale/src/setup.rs\tDATABASE_CTYPE_IS_C",
        "backend/utils/adt/pg_locale/src/setup.rs\tTHREAD_LOCALES",
        "backend/utils/adt/pg_locale/src/libc_locale.rs\tBACKEND_CTYPE",
    ];
    type Lead = StatementArm;
    type Armed = Armed;
    fn capture() -> StatementArm {
        StatementArm::capture()
    }
    fn arm(arm: &StatementArm) -> PgResult<Armed> {
        arm.arm()
    }
}

/// B2: the section's catalog arm (its program's hosted scalar sites' images, gravity_fn pool_catalog), none when it has
/// none.
struct Catalog;

impl ClaimState for Catalog {
    const NAME: &'static str = "catalog";
    const STATICS: &'static [&'static str] = &[
        "gravity/fn/src/typesem/collation.rs\tLEADER_CATALOG",
        "backend/utils/cache/catcache/src/lib.rs\tPOOL_STRICT",
        "backend/utils/cache/syscache_seams/src/lib.rs\tSTATEMENT_TYPE_ROWS",
        "backend/utils/cache/typcache/src/armed.rs\tSTATEMENT",
        "backend/utils/mb/mbutils/src/lib.rs\tSTATEMENT_CONVERSIONS",
    ];
    type Lead = Option<std::sync::Arc<gravity_fn::pool_catalog::StatementCatalog>>;
    type Armed = Option<gravity_fn::pool_catalog::CatalogArmed>;
    fn capture() -> Self::Lead {
        gravity_fn::pool_catalog::leader_statement()
    }
    fn arm(catalog: &Self::Lead) -> PgResult<Self::Armed> {
        Ok(catalog.as_ref().map(gravity_fn::pool_catalog::StatementCatalog::arm))
    }
}

/// C7 (a), D3919: the leader's session GUCs, every one a claim's code may read (an output shim's DateStyle or
/// extra_float_digits, `to_char`'s lc_monetary, `quote_ident`'s quote_all_identifiers, a spill's temp_file_limit): its
/// query pin (guc layers `current_query_pin`), applied by guc carry `arm_claim`.
struct Gucs;

impl ClaimState for Gucs {
    const NAME: &'static str = "gucs";
    /// The GUC backings and what their assign hooks set, in the thread-state registry's scope beside guc's own.
    const STATICS: &'static [&'static str] = &[
        "backend/utils/misc/guc/src/carry.rs\tPLAN",
        "backend/utils/misc/guc/src/carry.rs\tAPPLIED",
        "backend/utils/misc/guc_tables/src/session.rs\t$tls",
        "backend/access/transam/transam_xlog/src/guc_vars.rs\tWAL_CONSISTENCY_CHECKING_STRING",
        "backend/access/transam/transam_xlog/src/guc_vars.rs\tWAL_COMPRESSION",
        "backend/access/transam/transam_xlog/src/guc_vars.rs\tWAL_INIT_ZERO",
        "backend/access/transam/transam_xlog/src/guc_vars.rs\tWAL_RECYCLE",
        "backend/access/transam/transam_xlog/src/guc_vars.rs\tCOMMIT_DELAY",
        "backend/access/transam/transam_xlog/src/guc_vars.rs\tCOMMIT_SIBLINGS",
        "backend/access/transam/transam_xlog/src/guc_vars.rs\tTRACK_WAL_IO_TIMING",
        "backend/access/transam/transam_xlog/src/guc_vars.rs\tWAL_SYNC_METHOD",
        "backend/access/transam/transam_xlog/src/lib.rs\tWCC_FLAGS",
        "backend/access/transam/transam_xlog/src/lib.rs\tWCC_ANY",
        "backend/commands/vacuum/src/lib.rs\tVACUUM_FREEZE_MIN_AGE",
        "backend/commands/vacuum/src/lib.rs\tVACUUM_FREEZE_TABLE_AGE",
        "backend/commands/vacuum/src/lib.rs\tVACUUM_MXID_FREEZE_MIN_AGE",
        "backend/commands/vacuum/src/lib.rs\tVACUUM_MXID_FREEZE_TABLE_AGE",
        "backend/commands/vacuum/src/lib.rs\tVACUUM_FAILSAFE_AGE",
        "backend/commands/vacuum/src/lib.rs\tVACUUM_MXID_FAILSAFE_AGE",
        "backend/executor/execmain/src/lib.rs\tPLP",
        "backend/optimizer/path/costsize/src/gucs.rs\tCOSTSIZE_GUCS",
        "backend/optimizer/plan/planner/src/geqo/mod.rs\tGEQO_GUCS",
        "backend/optimizer/plan/planner/src/lib.rs\tPLANNER_GUCS",
        "backend/storage/file/fd/src/vfd.rs\tTEMP_TABLESPACES_GUC",
        "backend/utils/adt/adt_datetime/src/settings.rs\tDATE_ORDER",
        "backend/utils/adt/adt_datetime/src/settings.rs\tDATE_STYLE",
        "backend/utils/adt/adt_datetime/src/tz.rs\tZONEABBREVTBL",
        "backend/utils/adt/arrayfuncs/src/lib.rs\tARRAY_NULLS",
        "backend/utils/adt/float/src/lib.rs\tEXTRA_FLOAT_DIGITS",
        "backend/utils/adt/pg_locale/src/setup.rs\tICU_VALIDATION_LEVEL",
        "backend/utils/adt/pg_locale/src/setup.rs\tLOCALE_MESSAGES",
        "backend/utils/adt/pg_locale/src/setup.rs\tLOCALE_MONETARY",
        "backend/utils/adt/pg_locale/src/setup.rs\tLOCALE_NUMERIC",
        "backend/utils/adt/pg_locale/src/setup.rs\tLOCALE_TIME",
        "backend/utils/adt/regexp_alt/src/lib.rs\tREGEXP_ALT_GUCS",
        "backend/utils/adt/varlena/src/lib.rs\tBYTEA_OUTPUT",
        "backend/utils/adt/xml/src/lib.rs\tXMLBINARY",
        "backend/utils/adt/xml/src/lib.rs\tXMLOPTION",
        "backend/utils/cache/ts_cache/src/lib.rs\tGUC_VALUE",
        "backend/utils/error/elog/src/config.rs\tBACKTRACE_FUNCTION_LIST",
        "backend/utils/error/elog/src/config.rs\tCLIENT_MIN_MESSAGES",
        "backend/utils/error/elog/src/config.rs\tLOG_DESTINATION",
        "backend/utils/error/elog/src/config.rs\tLOG_DESTINATION_STRING",
        "backend/utils/error/elog/src/config.rs\tLOG_ERROR_VERBOSITY",
        "backend/utils/error/elog/src/config.rs\tLOG_LINE_PREFIX",
        "backend/utils/error/elog/src/config.rs\tLOG_MIN_ERROR_STATEMENT",
        "backend/utils/error/elog/src/config.rs\tLOG_MIN_MESSAGES",
        "backend/utils/error/elog/src/config.rs\tSYSLOG_SEQUENCE_NUMBERS",
        "backend/utils/error/elog/src/config.rs\tSYSLOG_SPLIT_MESSAGES",
        "backend/utils/fmgr/dfmgr/src/lib.rs\tDYNAMIC_LIBRARY_PATH",
        "backend/utils/init/init_small/src/globals.rs\tALLOW_SYSTEM_TABLE_MODS",
        "backend/utils/init/init_small/src/globals.rs\tENABLE_FSYNC",
        "backend/utils/init/init_small/src/globals.rs\tHASH_MEM_MULTIPLIER",
        "backend/utils/init/init_small/src/globals.rs\tINTERVAL_STYLE",
        "backend/utils/init/init_small/src/globals.rs\tMAINTENANCE_WORK_MEM",
        "backend/utils/init/init_small/src/globals.rs\tMAX_PARALLEL_MAINTENANCE_WORKERS",
        "backend/utils/init/init_small/src/globals.rs\tMAX_PARALLEL_WORKERS",
        "backend/utils/init/init_small/src/globals.rs\tVACUUM_BUFFER_USAGE_LIMIT",
        "backend/utils/init/init_small/src/globals.rs\tVACUUM_COST_DELAY",
        "backend/utils/init/init_small/src/globals.rs\tVACUUM_COST_LIMIT",
        "backend/utils/init/init_small/src/globals.rs\tVACUUM_COST_PAGE_DIRTY",
        "backend/utils/init/init_small/src/globals.rs\tVACUUM_COST_PAGE_HIT",
        "backend/utils/init/init_small/src/globals.rs\tVACUUM_COST_PAGE_MISS",
        "backend/utils/init/init_small/src/globals.rs\tWORK_MEM",
        "backend/utils/misc/guc_tables/src/backing.rs\tBACKING_SESSION_GUCS",
        "backend/utils/misc/guc_tables/src/backing.rs\tPGRUST_STATEMENT_MEM_LIMIT",
        "backend/utils/misc/guc_tables/src/backing.rs\tRESTRICT_NONSYSTEM_RELATION_KIND",
        "backend/utils/misc/stack_depth_core/src/lib.rs\tMAX_STACK_DEPTH",
        "backend/utils/misc/stack_depth_core/src/lib.rs\tMAX_STACK_DEPTH_BYTES",
        "backend/utils/misc/stack_depth_core/src/lib.rs\tSCALED_STACK_BUDGET",
        "backend/utils/sort/tuplesort/src/lib.rs\tTRACE_SORT_GUC",
        "backend/parser/parse_expr/src/lib.rs\tTRANSFORM_NULL_EQUALS",
        "backend/parser/scan_fgram/src/lib.rs\tBACKSLASH_QUOTE",
        "backend/parser/scan_fgram/src/lib.rs\tESCAPE_STRING_WARNING",
        "backend/parser/scan_fgram/src/lib.rs\tSTANDARD_CONFORMING_STRINGS",
        "contrib/pgvector_hnsw/src/lib.rs\tEF_SEARCH",
        "contrib/pgvector_hnsw/src/lib.rs\tITERATIVE_SCAN",
        "contrib/pgvector_hnsw/src/lib.rs\tMAX_SCAN_TUPLES",
        "contrib/pgvector_hnsw/src/lib.rs\tSCAN_MEM_MULTIPLIER",
        "_support/types/types_error/src/severity.rs\tEXIT_ON_ANY_ERROR",
        "backend/access/gin/gin/src/lib.rs\tGIN_FUZZY_SEARCH_LIMIT",
        "backend/access/gin/gin/src/lib.rs\tGIN_PENDING_LIST_LIMIT",
        "backend/access/table/tableam/src/lib.rs\tDEFAULT_TABLE_ACCESS_METHOD_GUC",
        "backend/access/table/tableam_vocab/src/lib.rs\tSYNCHRONIZE_SEQSCANS_GUC",
        "backend/commands/analyze/src/lib.rs\tDEFAULT_STATISTICS_TARGET",
        "backend/commands/commands_async/src/lib.rs\tTRACE_NOTIFY",
        "backend/commands/event_trigger/src/lib.rs\tEVENT_TRIGGERS_GUC",
        "backend/commands/event_trigger/src/lib.rs\tSESSION_REPLICATION_ROLE",
        "backend/commands/explain/src/lib.rs\tQUOTE_ALL_IDENTIFIERS",
        "backend/commands/extension/src/control.rs\tEXTENSION_CONTROL_PATH",
        "backend/commands/tablespace/src/lib.rs\tALLOW_IN_PLACE_TABLESPACES",
        "backend/commands/tablespace/src/lib.rs\tDEFAULT_TABLESPACE",
        "backend/commands/user/src/lib.rs\tPASSWORD_ENCRYPTION",
        "backend/commands/vacuum/src/lib.rs\tTRACK_COST_DELAY_TIMING",
        "backend/commands/vacuum/src/lib.rs\tVACUUM_MAX_EAGER_FREEZE_FAILURE_RATE",
        "backend/commands/vacuum/src/lib.rs\tVACUUM_TRUNCATE",
        "backend/replication/logical/reorderbuffer/src/startup.rs\tDEBUG_LOGICAL_REPLICATION_STREAMING",
        "backend/replication/logical/reorderbuffer/src/startup.rs\tLOGICAL_DECODING_WORK_MEM",
        "backend/storage/buffer/bufmgr/src/gucs.rs\tEFFECTIVE_IO_CONCURRENCY",
        "backend/storage/buffer/bufmgr/src/gucs.rs\tIO_COMBINE_LIMIT_GUC",
        "backend/storage/buffer/bufmgr/src/gucs.rs\tBACKEND_FLUSH_AFTER",
        "backend/storage/buffer/bufmgr/src/gucs.rs\tZERO_DAMAGED_PAGES",
        "backend/storage/buffer/bufmgr/src/gucs.rs\tIGNORE_CHECKSUM_FAILURE",
        "backend/storage/buffer/bufmgr/src/gucs.rs\tTRACK_IO_TIMING",
        "backend/utils/activity/pgstat/src/lib.rs\tTRACK_COUNTS",
        "backend/utils/activity/pgstat/src/function.rs\tTRACK_FUNCTIONS",
    ];
    type Lead = std::sync::Arc<guc::layers::GucQuerySnapshot>;
    type Armed = guc::carry::ClaimGucs;
    fn capture() -> Self::Lead {
        guc::layers::current_query_pin()
    }
    fn arm(pin: &Self::Lead) -> PgResult<guc::carry::ClaimGucs> {
        guc::carry::arm_claim(pin)
    }
}

/// B2: the claim generation (`gravity_fn::session::new_claim_gen`): a pool lane entered under another drops its twins'
/// `fn_extra`.
struct ClaimGeneration;

/// The claim generation a claim displaced, put back at its end.
struct GenBack(u64);

impl Drop for GenBack {
    fn drop(&mut self) {
        gravity_fn::session::arm_claim_gen(self.0);
    }
}

impl ClaimState for ClaimGeneration {
    const NAME: &'static str = "claim_generation";
    const STATICS: &'static [&'static str] = &["gravity/fn/src/typesem/collation.rs\tCLAIM_GEN"];
    type Lead = u64;
    type Armed = GenBack;
    fn capture() -> u64 {
        gravity_fn::session::new_claim_gen()
    }
    fn arm(gen: &u64) -> PgResult<GenBack> {
        Ok(GenBack(gravity_fn::session::arm_claim_gen(*gen)))
    }
}

/// D2983: the transaction's and the statement's start (`now()`, `statement_timestamp()`, CURRENT_DATE).
struct StartTimestamps;

/// The start timestamps a claim displaced (`None`: it moved none), put back at its end.
struct TsBack(Option<(i64, i64)>);

impl Drop for TsBack {
    fn drop(&mut self) {
        if let Some((x, st)) = self.0 {
            let _ = xact_seams::arm_claim_start_timestamps::call(x, st);
        }
    }
}

impl ClaimState for StartTimestamps {
    const NAME: &'static str = "start_timestamps";
    const STATICS: &'static [&'static str] = &["backend/access/transam/xact/src/state.rs\tSTATE"];
    type Lead = Option<(i64, i64)>;
    type Armed = TsBack;
    fn capture() -> Self::Lead {
        (xact_seams::get_current_transaction_start_timestamp::is_installed() && xact_seams::get_current_statement_start_timestamp::is_installed())
            .then(|| (xact_seams::get_current_transaction_start_timestamp::call(), xact_seams::get_current_statement_start_timestamp::call()))
    }
    fn arm(ts: &Self::Lead) -> PgResult<TsBack> {
        Ok(TsBack(match *ts {
            Some((x, st)) if xact_seams::arm_claim_start_timestamps::is_installed() => {
                let had = xact_seams::arm_claim_start_timestamps::call(x, st);
                (had != (x, st)).then_some(had)
            }
            _ => None,
        }))
    }
}

/// D4252: the session's database identity: a claim's spill opens its temp files in the database's default tablespace
/// (temp_tablespaces empty) and reports them under the database, as PostgreSQL's parallel worker restores both from its
/// leader. A pool thread's own are InvalidOid, or a database a standing serve pinned for the thread's life.
struct Database;

/// The database identity a claim displaced (`None`: it moved none), put back at its end.
struct DatabaseBack(Option<(types_core::Oid, types_core::Oid)>);

impl Drop for DatabaseBack {
    fn drop(&mut self) {
        if let Some((db, ts)) = self.0 {
            init_small::globals::SetMyDatabaseId(db);
            init_small::globals::SetMyDatabaseTableSpace(ts);
        }
    }
}

impl ClaimState for Database {
    const NAME: &'static str = "database";
    const STATICS: &'static [&'static str] = &[
        "backend/utils/init/init_small/src/globals.rs\tMY_DATABASE_ID",
        "backend/utils/init/init_small/src/globals.rs\tMY_DATABASE_TABLE_SPACE",
    ];
    type Lead = (types_core::Oid, types_core::Oid);
    type Armed = DatabaseBack;
    fn capture() -> Self::Lead {
        (init_small::globals::MyDatabaseId(), init_small::globals::MyDatabaseTableSpace())
    }
    fn arm(&(db, ts): &Self::Lead) -> PgResult<DatabaseBack> {
        let had = (init_small::globals::MyDatabaseId(), init_small::globals::MyDatabaseTableSpace());
        if had == (db, ts) {
            return Ok(DatabaseBack(None));
        }
        init_small::globals::SetMyDatabaseId(db);
        init_small::globals::SetMyDatabaseTableSpace(ts);
        Ok(DatabaseBack(Some(had)))
    }
}

/// D2351: the leader's temp-bytes account: a claim's spill counts against the session's `temp_file_limit`, as a serial
/// spill does.
struct TempAccount;

/// The temp account a claim displaced, put back at its end.
struct TempBack(Option<std::sync::Arc<std::sync::atomic::AtomicU64>>);

impl Drop for TempBack {
    fn drop(&mut self) {
        fd::charge_temp_to(self.0.take());
    }
}

impl ClaimState for TempAccount {
    const NAME: &'static str = "temp_account";
    const STATICS: &'static [&'static str] = &["backend/storage/file/fd/src/vfd.rs\tFD"];
    type Lead = std::sync::Arc<std::sync::atomic::AtomicU64>;
    type Armed = TempBack;
    fn capture() -> Self::Lead {
        fd::temp_account()
    }
    fn arm(temp: &Self::Lead) -> PgResult<TempBack> {
        Ok(TempBack(fd::charge_temp_to(Some(temp.clone()))))
    }
}

/// D151: the statement's toast relations: a claim's external fetches read them by locator.
struct Toast;

impl ClaimState for Toast {
    const NAME: &'static str = "toast";
    const STATICS: &'static [&'static str] = &["backend/access/heap/heaptoast/src/worker.rs\tPUBLISHED", "backend/access/heap/heaptoast/src/worker.rs\tENTERED"];
    type Lead = std::sync::Arc<heaptoast::worker::ToastMap>;
    type Armed = heaptoast::worker::Entered;
    fn capture() -> Self::Lead {
        heaptoast::worker::published()
    }
    fn arm(map: &Self::Lead) -> PgResult<heaptoast::worker::Entered> {
        Ok(heaptoast::worker::enter(map.clone()))
    }
}

/// D4246: the leader's TransactionXmin, where a claim's SubTransGetTopmostTransaction stops its pg_subtrans walk (below
/// it pg_subtrans may be truncated), as PostgreSQL's parallel worker restores it (RestoreTransactionSnapshot).
struct TransactionXmin;

/// The thread's own TransactionXmin, put back at the claim's end.
struct XminBack(types_core::TransactionId);

impl Drop for XminBack {
    fn drop(&mut self) {
        procarray::set_transaction_xmin(self.0);
    }
}

impl ClaimState for TransactionXmin {
    const NAME: &'static str = "transaction_xmin";
    const STATICS: &'static [&'static str] = &["backend/storage/ipc/procarray/src/lib.rs\tTRANSACTION_XMIN"];
    type Lead = types_core::TransactionId;
    type Armed = XminBack;
    fn capture() -> Self::Lead {
        procarray::TransactionXmin()
    }
    fn arm(xmin: &Self::Lead) -> PgResult<XminBack> {
        let own = procarray::TransactionXmin();
        procarray::set_transaction_xmin(*xmin);
        Ok(XminBack(own))
    }
}

/// D3401: puts back the owner `arm_once` displaced for a claim (no owner on a pool thread between claims; a serve's
/// own), dropped with the claim's carry, or alone when arming the carry fails. A nested claim displaced none: the claim
/// around it keeps the standing owner.
struct Restore(Option<ResourceOwner>);

impl Drop for Restore {
    fn drop(&mut self) {
        if let Some(prev) = self.0 {
            resowner::SetCurrentResourceOwner(prev);
        }
    }
}

pub fn no_arm() -> PgResult<()> {
    Ok(())
}

static FIRST_ARM_MICROS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Arms this pool thread for a claim: the first time `arm` (the standing owner, file access), then the standing owner made
/// current again. D3401: the owner it displaces is the claim's carry's to put back (`SessionCarry::arm`).
pub fn arm_once(arm: ArmFn) -> PgResult<()> {
    let before = resowner::CurrentResourceOwner();
    if ARMED.with(Cell::get) {
        let owner = OWNER.with(Cell::get);
        if !owner.is_null() && before != owner {
            resowner::SetCurrentResourceOwner(owner);
        }
    } else {
        let start = std::time::Instant::now();
        if let Err(e) = arm() {
            resowner::SetCurrentResourceOwner(before);
            return Err(e);
        }
        ARMED.with(|a| a.set(true));
        // C7 runtime half: from here on a leader_tls! static touched inside a pool claim raises ERROR XX000 (debug
        // builds; a release build declares plain thread-locals and installs nothing)
        #[cfg(debug_assertions)]
        leader_tls::install_pool_probe(in_claim);
        // the process's first arming (a worker's PGPROC bring-up rides the pool gate before it)
        let _ = FIRST_ARM_MICROS.compare_exchange(0, start.elapsed().as_micros().max(1) as u64, std::sync::atomic::Ordering::AcqRel, std::sync::atomic::Ordering::Relaxed);
    }
    let now = resowner::CurrentResourceOwner();
    DISPLACED.with(|d| d.set((now != before).then_some(before)));
    Ok(())
}

/// Microseconds the process's first worker arming took (0 before any).
pub fn first_arm_micros() -> u64 {
    FIRST_ARM_MICROS.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn is_armed() -> bool {
    ARMED.with(Cell::get)
}

// C7 runtime half (debug builds): the pool claims in progress on this thread, from SessionCarry::arm to its Carried's
// drop. It is the leader_tls! probe: inside a claim, a leader-class static is the pool thread's own copy, never its
// leader's. Outside one, an armed pool thread runs other work (a parallel VACUUM's workers, its invalidation processing),
// which no leader's session carries into.
#[cfg(debug_assertions)]
thread_local! {
    static IN_CLAIM: Cell<u32> = const { Cell::new(0) };
}

#[cfg(debug_assertions)]
fn claim_enter() {
    IN_CLAIM.with(|n| n.set(n.get() + 1));
}

#[cfg(debug_assertions)]
fn claim_leave() {
    IN_CLAIM.with(|n| n.set(n.get().saturating_sub(1)));
}

/// C7 runtime half: a pool claim is in progress on this thread (debug builds: the leader_tls! probe).
#[cfg(debug_assertions)]
pub fn in_claim() -> bool {
    IN_CLAIM.with(Cell::get) > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// G1-B: a claim's memory charges its leader's ledger account, not the pool thread's own (its pool identity's backend
    /// slot), and its pending bytes are on the leader's account when the claim ends; the thread's own account is back after.
    #[test]
    fn a_claim_charges_its_leaders_memory_account() {
        use memledger::charge::{context, Charge, Door};
        use memledger::{account_row, bind_backend, current, unbind_backend};
        std::thread::spawn(|| {
            bind_backend(3990, 0);
            let leader = current();
            let carry = {
                let _k = memledger::with_kind(memledger::Kind::HashAgg);
                SessionCarry::capture()
            };
            std::thread::spawn(move || {
                bind_backend(3991, 0);
                let own = current();
                let mapped = {
                    let _carried = carry.arm().unwrap();
                    assert_eq!(current(), leader);
                    assert_eq!(account_row(leader).threads, 2, "the claim's thread is on the leader's account");
                    assert_eq!(memledger::kind(), memledger::Kind::HashAgg, "and its regions take the submit's kind");
                    let (o, rel) = context();
                    Charge::open(o, rel, memledger::Kind::Mapped, Door::Mapped, 1 << 20)
                };
                assert_eq!((current(), memledger::kind()), (own, memledger::Kind::Region));
                assert_eq!((account_row(leader).account_bytes, account_row(own).account_bytes), (1 << 20, 0));
                drop(mapped);
                assert_eq!(account_row(leader).account_bytes, 0, "its owner's account takes it back, off the claim");
                unbind_backend();
            })
            .join()
            .unwrap();
            unbind_backend();
        })
        .join()
        .unwrap();
    }

    /// B2: two statements' carries arm two claim generations on the same pool thread, one after the other; each claim's
    /// end puts the thread's own (0) back. A lane keyed on the generation sees the second statement as new.
    #[test]
    fn each_carry_arms_its_own_claim_generation() {
        use gravity_fn::session::claim_gen;
        let (first, second) = (SessionCarry::capture(), SessionCarry::capture());
        std::thread::spawn(move || {
            assert_eq!(claim_gen(), 0, "a bare thread has no claim generation");
            let g1 = {
                let _c = first.arm().unwrap();
                claim_gen()
            };
            assert_eq!(claim_gen(), 0);
            let g2 = {
                let _c = second.arm().unwrap();
                let again = {
                    let c = first.arm().unwrap();
                    let g = claim_gen();
                    drop(c);
                    g
                };
                assert_eq!(again, g1, "the same carry arms the same generation (nested claim)");
                assert_ne!(claim_gen(), g1, "the nested claim's end put the outer generation back");
                claim_gen()
            };
            assert!(g1 != 0 && g2 != 0 && g1 != g2, "two statements, two generations: {g1} {g2}");
            assert_eq!(claim_gen(), 0);
        })
        .join()
        .unwrap();
    }

    /// D4252: a claim reads its leader's database identity (MyDatabaseId, MyDatabaseTableSpace), as PostgreSQL's parallel
    /// worker restores both from its leader: a spill's temp file opens in the database's default tablespace and reports
    /// under the database. The pool thread's own (InvalidOid here) is back after the claim.
    #[test]
    fn a_claim_reads_its_leaders_database_identity() {
        use init_small::globals::{MyDatabaseId, MyDatabaseTableSpace, SetMyDatabaseId, SetMyDatabaseTableSpace};
        std::thread::spawn(|| {
            SetMyDatabaseId(16384);
            SetMyDatabaseTableSpace(16390);
            let carry = SessionCarry::capture();
            std::thread::spawn(move || {
                let own = (MyDatabaseId(), MyDatabaseTableSpace());
                {
                    let _carried = carry.arm().unwrap();
                    assert_eq!((MyDatabaseId(), MyDatabaseTableSpace()), (16384, 16390), "the claim reads its leader's database");
                }
                assert_eq!((MyDatabaseId(), MyDatabaseTableSpace()), own, "the claim's end put the thread's own back");
            })
            .join()
            .unwrap();
        })
        .join()
        .unwrap();
    }

    #[test]
    fn arm_once_reinstalls_the_standing_owner() {
        std::thread::spawn(|| {
            arm_once(arm_owner).unwrap();
            let owner = OWNER.with(Cell::get);
            assert!(!owner.is_null());
            assert_eq!(resowner::CurrentResourceOwner(), owner);
            resowner::SetCurrentResourceOwner(ResourceOwner::NULL);
            arm_once(arm_owner).unwrap();
            assert_eq!(resowner::CurrentResourceOwner(), owner);
            assert_eq!(OWNER.with(Cell::get), owner);
        })
        .join()
        .unwrap();
    }

    /// D3401: a claim's end parks the standing owner and leaves the thread's resource-owner state clean, so a bound serve
    /// on the pool thread between two claims (VACUUM's index pass, a lazy-scan morsel, an nbtsort build) binds. Left
    /// current, the owner refused every such serve at bind ("current resource owner is still live").
    #[test]
    fn a_claims_end_parks_the_standing_owner_for_a_bound_serve() {
        let carry = SessionCarry::capture();
        std::thread::spawn(move || {
            for claim in 0..2 {
                arm_once(arm_owner).unwrap();
                let owner = OWNER.with(Cell::get);
                {
                    let _carried = carry.arm().unwrap();
                    assert_eq!(resowner::CurrentResourceOwner(), owner, "claim {claim} runs under the standing owner");
                }
                assert!(resowner::CurrentResourceOwner().is_null(), "claim {claim}'s end left the standing owner current");
                assert_eq!(resowner::ResourceOwnerStateIssue(), None, "claim {claim}'s end: a bound serve refuses at bind");
            }
        })
        .join()
        .unwrap();
    }

    /// D3401: a claim on a thread whose own owner is current (a serve's transaction owner) puts it back at the claim's
    /// end; a nested claim's end leaves the standing owner current for the claim around it.
    #[test]
    fn a_claims_end_puts_back_the_owner_it_displaced() {
        let carry = SessionCarry::capture();
        std::thread::spawn(move || {
            let txn = resowner::ResourceOwnerCreate(ResourceOwner::NULL, "TopTransaction").unwrap();
            resowner::SetCurrentResourceOwner(txn);
            arm_once(arm_owner).unwrap();
            let owner = OWNER.with(Cell::get);
            let outer = carry.arm().unwrap();
            arm_once(arm_owner).unwrap();
            drop(carry.arm().unwrap());
            assert_eq!(resowner::CurrentResourceOwner(), owner, "a nested claim's end keeps the outer claim's owner");
            drop(outer);
            assert_eq!(resowner::CurrentResourceOwner(), txn, "the claim's end did not put back the serve's owner");
            resowner::SetCurrentResourceOwner(ResourceOwner::NULL);
            resowner::ResourceOwnerDelete(txn);
        })
        .join()
        .unwrap();
    }

    /// C7 (b): the compiled half of the thread-state registry (lint/THREAD-STATE; rule RT checks the rest at every
    /// landing: a row per thread-local in its scope, owner, reset, carry, TLS-DEBT). A `claim` row is carried by the
    /// CLAIM_REGISTRY member it names; every member's static is a thread-local (session tls-census.tsv), and one with a
    /// registry or TLS-DEBT row is carried `claim` there. A member that stops carrying a static, or a claim row whose
    /// member does not, fails here.
    #[test]
    fn claim_census_is_pinned() {
        use std::collections::{BTreeMap, BTreeSet};
        let dir = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"));
        let crates = dir.ancestors().nth(2).unwrap();
        let read = |path: std::path::PathBuf| std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let rows = |text: &str| -> Vec<Vec<String>> { text.lines().filter(|l| !l.is_empty() && !l.starts_with('#')).map(|l| l.split('\t').map(str::to_string).collect()).collect() };
        // a registry or TLS-DEBT directory's rows, README / OWNERS / dot files out (lint/rules/rt_thread_state.py read_dir)
        let ledger = |name: &str| -> Vec<Vec<String>> {
            let at = crates.join("gravity/lint").join(name);
            let mut files: Vec<_> = std::fs::read_dir(&at).unwrap_or_else(|e| panic!("{}: {e}", at.display())).map(|e| e.unwrap().path()).collect(); // alloc: Cold
            files.sort();
            files.into_iter().filter(|p| p.is_file() && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n != "README" && n != "OWNERS" && !n.starts_with('.'))).flat_map(|p| rows(&read(p))).collect()
        };
        let tls: BTreeSet<(String, String)> = rows(&read(crates.join("backend/access/session/tls-census.tsv"))).into_iter().map(|f| (f[0].clone(), f[1].clone())).collect();
        let carried = |member: &str, path: &str, name: &str| CLAIM_REGISTRY.iter().any(|(m, statics)| *m == member && statics.contains(&format!("{path}\t{name}").as_str())); // alloc: Cold
        // <path> <static> <owner> <reset> <carry> [<member | note>]; TLS-DEBT: <path> <static> <owner lane> <carry today> <what is missing>
        let mut carry = BTreeMap::new();
        for f in ledger("THREAD-STATE") {
            assert!(f.len() >= 5, "lint/THREAD-STATE row {f:?}: <path>\\t<static>\\t<owner>\\t<reset>\\t<carry>\\t[<member | note>]");
            if f[4] == "claim" {
                let member = f.get(5).map_or("", String::as_str);
                assert!(carried(member, &f[0], &f[1]), "{} {}: carried claim by {member:?} in lint/THREAD-STATE, which no CLAIM_REGISTRY member of that name carries", f[0], f[1]);
            }
            carry.insert((f[0].clone(), f[1].clone()), f[4].clone());
        }
        for f in ledger("TLS-DEBT") {
            assert!(f.len() >= 5, "lint/TLS-DEBT row {f:?}: <path>\\t<static>\\t<owner lane>\\t<carry today>\\t<what is missing>");
            carry.entry((f[0].clone(), f[1].clone())).or_insert_with(|| f[3].clone());
        }
        let templated: BTreeSet<&str> = tls.iter().filter(|(_, n)| n.starts_with('$')).map(|(p, _)| p.as_str()).collect(); // alloc: Cold
        let defs = std::cell::OnceCell::new();
        for (member, statics) in CLAIM_REGISTRY {
            for s in *statics {
                let (path, name) = s.split_once('\t').unwrap();
                let key = (path.to_string(), name.to_string()); // alloc: Cold
                // a macro instance (RT reads the invocation) stands in the census as its template (`$cell`, `$tls`): of its own
                // file, or of the file defining a macro invoked around its name (guc_tables session_guc_*!) or invoked by
                // that macro's body (vacuum_guc_int!); its row carries it claim
                let instance = || {
                    let defs = defs.get_or_init(|| macro_defs(crates));
                    templated.contains(path) || invoked(&read(crates.join(path)), Some(name)).iter().any(|m| templated_macro(m, defs, &templated, 1))
                };
                let claimed = carry.get(&key).is_some_and(|c| c == "claim");
                assert!(tls.contains(&key) || claimed && instance(), "registry member {member}: {path} {name} is no thread-local (session tls-census.tsv)");
                if let Some(class) = carry.get(&key) {
                    assert_eq!(class, "claim", "registry member {member} carries {path} {name}, carried {class} in lint/THREAD-STATE / TLS-DEBT");
                }
            }
        }
    }

    /// Every `macro_rules!` under `crates`, by name: its file (path under crates/) and body.
    fn macro_defs(crates: &std::path::Path) -> std::collections::BTreeMap<String, Vec<(String, String)>> {
        let mut defs: std::collections::BTreeMap<String, Vec<(String, String)>> = std::collections::BTreeMap::new();
        let mut dirs = vec![crates.to_path_buf()]; // alloc: Cold
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    dirs.push(path);
                    continue;
                }
                let Some(text) = path.extension().filter(|x| *x == "rs").and_then(|_| std::fs::read_to_string(&path).ok()) else { continue };
                let rel = path.strip_prefix(crates).unwrap().to_string_lossy().replace('\\', "/");
                for (i, _) in text.match_indices("macro_rules!") {
                    let rest = text[i + "macro_rules!".len()..].trim_start();
                    let name: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
                    let body = rest.find('{').map_or("", |o| bracketed(&rest[o..]));
                    defs.entry(name).or_default().push((rel.clone(), body.to_string())); // alloc: Cold
                }
            }
        }
        defs
    }

    /// `s` from its opening bracket through the one that closes it (all of `s` when none does).
    fn bracketed(s: &str) -> &str {
        let mut depth = 0i32;
        for (i, c) in s.char_indices() {
            match c {
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return &s[..=i];
                    }
                }
                _ => {}
            }
        }
        s
    }

    /// The macros `text` invokes (`name!(..)`, `path::name! { .. }`): with `word`, those whose brackets hold it.
    fn invoked(text: &str, word: Option<&str>) -> Vec<String> {
        let b = text.as_bytes();
        let mut out = Vec::new(); // alloc: Cold
        for (i, _) in text.match_indices('!') {
            let mut s = i;
            while s > 0 && (b[s - 1].is_ascii_alphanumeric() || b[s - 1] == b'_') {
                s -= 1;
            }
            let rest = text[i + 1..].trim_start();
            if s == i || &text[s..i] == "macro_rules" || !rest.starts_with(|c: char| matches!(c, '(' | '[' | '{')) {
                continue;
            }
            let args = bracketed(rest);
            if word.is_none_or(|w| args.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).any(|t| t == w)) {
                out.push(text[s..i].to_string()); // alloc: Cold
            }
        }
        out
    }

    /// Macro `m` declares through a census template: a file defining it has a `$` line, or (`depth` levels down) a
    /// macro its body invokes does.
    fn templated_macro(m: &str, defs: &std::collections::BTreeMap<String, Vec<(String, String)>>, templated: &std::collections::BTreeSet<&str>, depth: u8) -> bool {
        defs.get(m).is_some_and(|ds| ds.iter().any(|(file, body)| templated.contains(file.as_str()) || depth > 0 && invoked(body, None).iter().any(|m2| templated_macro(m2, defs, templated, depth - 1))))
    }
}
