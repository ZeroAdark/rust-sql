// C macro values from the headers guc_tables.c includes, proven build config.
pub const ARCHIVE_MODE_OFF: i32 = 0;
pub const BACKSLASH_QUOTE_OFF: i32 = 0;
pub const BACKSLASH_QUOTE_ON: i32 = 1;
pub const BACKSLASH_QUOTE_SAFE_ENCODING: i32 = 2;
pub const BLCKSZ: i32 = 8192;
pub const DEFAULT_PGSOCKET_DIR: &str = match option_env!("PGRUST_DEFAULT_PGSOCKET_DIR") {
    Some(d) => d,
    None => "/tmp",
};
pub const BYTEA_OUTPUT_ESCAPE: i32 = 0;
pub const BYTEA_OUTPUT_HEX: i32 = 1;
pub const COMPUTE_QUERY_ID_AUTO: i32 = 2;
pub const COMPUTE_QUERY_ID_OFF: i32 = 0;
pub const COMPUTE_QUERY_ID_ON: i32 = 1;
pub const COMPUTE_QUERY_ID_REGRESS: i32 = 3;
// pgcrypto px.h BuiltinCryptoModes (pgcrypto.builtin_crypto_enabled).
pub const BC_ON: i32 = 0;
pub const BC_OFF: i32 = 1;
pub const BC_FIPS: i32 = 2;
// plpgsql.h PLPGSQL_RESOLVE_* (plpgsql.variable_conflict, pl_handler.c:40-45).
pub const PLPGSQL_RESOLVE_ERROR: i32 = 0;
pub const PLPGSQL_RESOLVE_VARIABLE: i32 = 1;
pub const PLPGSQL_RESOLVE_COLUMN: i32 = 2;
// pg_stat_statements.c PGSSTrackLevel.
pub const PGSS_TRACK_NONE: i32 = 0;
pub const PGSS_TRACK_TOP: i32 = 1;
pub const PGSS_TRACK_ALL: i32 = 2;
// explain_state.h ExplainFormat (pinned; the explain crate's enum repr
// matches — auto_explain.log_format stores these directly).
pub const EXPLAIN_FORMAT_TEXT: i32 = 0;
pub const EXPLAIN_FORMAT_XML: i32 = 1;
pub const EXPLAIN_FORMAT_JSON: i32 = 2;
pub const EXPLAIN_FORMAT_YAML: i32 = 3;
pub const CONSTRAINT_EXCLUSION_OFF: i32 = 0;
pub const CONSTRAINT_EXCLUSION_ON: i32 = 1;
pub const CONSTRAINT_EXCLUSION_PARTITION: i32 = 2;
pub const DATA_DIR_SYNC_METHOD_FSYNC: i32 = 0;
pub const DATA_DIR_SYNC_METHOD_SYNCFS: i32 = 1;
pub const DEBUG1: i32 = types_error::DEBUG1.0;
pub const DEBUG2: i32 = types_error::DEBUG2.0;
pub const DEBUG3: i32 = types_error::DEBUG3.0;
pub const DEBUG4: i32 = types_error::DEBUG4.0;
pub const DEBUG5: i32 = types_error::DEBUG5.0;
pub const DEBUG_LOGICAL_REP_STREAMING_BUFFERED: i32 = 0;
pub const DEBUG_LOGICAL_REP_STREAMING_IMMEDIATE: i32 = 1;
pub const DEBUG_PARALLEL_OFF: i32 = 0;
pub const DEBUG_PARALLEL_ON: i32 = 1;
pub const DEBUG_PARALLEL_REGRESS: i32 = 2;
pub const DEFAULT_BACKEND_FLUSH_AFTER: i32 = 0;
#[cfg(target_os = "linux")]
pub const DEFAULT_BGWRITER_FLUSH_AFTER: i32 = 64;
#[cfg(not(target_os = "linux"))]
pub const DEFAULT_BGWRITER_FLUSH_AFTER: i32 = 0;
#[cfg(target_os = "linux")]
pub const DEFAULT_CHECKPOINT_FLUSH_AFTER: i32 = 32;
#[cfg(not(target_os = "linux"))]
pub const DEFAULT_CHECKPOINT_FLUSH_AFTER: i32 = 0;
pub const DEFAULT_CPU_INDEX_TUPLE_COST: f64 = 0.005;
pub const DEFAULT_CPU_OPERATOR_COST: f64 = 0.0025;
pub const DEFAULT_CPU_TUPLE_COST: f64 = 0.01;
pub const DEFAULT_CURSOR_TUPLE_FRACTION: f64 = 0.1;
pub const DEFAULT_DYNAMIC_SHARED_MEMORY_TYPE: i32 = 1;
pub const DEFAULT_EFFECTIVE_CACHE_SIZE: i32 = 524288;
pub const DEFAULT_EFFECTIVE_IO_CONCURRENCY: i32 = 16;
pub const DEFAULT_FILE_EXTEND_METHOD: i32 = 0;
pub const DEFAULT_GEQO_EFFORT: i32 = 5;
pub const DEFAULT_GEQO_SELECTION_BIAS: f64 = 2.0;
pub const DEFAULT_MAINTENANCE_IO_CONCURRENCY: i32 = 16;
pub const DEFAULT_MAX_WAL_SEGS: i32 = 64;
pub const DEFAULT_MIN_WAL_SEGS: i32 = 5;
// pgrust ships parallelism priced for the THREAD-NATIVE warm work-stealing
// pool, not C's fork+DSM. C's 1000 prices fork + DSM/shm_toc setup + GUC
// serialize (~1-2ms; costsize/gucs.rs provenance). pgrust's shipped engine
// (pgrust.parallel_engine=runtime, pool spawned at postmaster start) hands a
// morsel to an already-parked standby: measured launch->worker-main 175us
// (notes/ppool-lane.md:58), standing-executor worker-launch eliminated
// (notes/m2-pool-binding.md:293). Anchoring C's ~1-2ms=1000 => 175us ~= 100.
// CONTINGENCY: honest only while the runtime pool is the executor; a shape
// that falls to the LEGACY cold-spawn Gather (~11ms full backend boot) is
// under-priced here — the runtime router's
// coverage is the mitigation (docs/design/jit-parallel-defaults.md).
pub const DEFAULT_PARALLEL_SETUP_COST: f64 = 100.0;
// C's 0.1 prices a cross-process shm_mq copy of one tuple. pgrust moves tuples
// worker->leader in-process through a chunked Arc ring at ~27ns/tuple (the
// pure-pointer-walk chunked path). 0.01 = 10x below
// C, the in-process MinimalTuple-memcpy rate ~= one tuple's cpu_tuple_cost.
pub const DEFAULT_PARALLEL_TUPLE_COST: f64 = 0.01;
pub const DEFAULT_RANDOM_PAGE_COST: f64 = 4.0;
pub const DEFAULT_RECURSIVE_WORKTABLE_FACTOR: f64 = 10.0;
pub const DEFAULT_SEQ_PAGE_COST: f64 = 1.0;
pub const DEFAULT_SYSLOG_FACILITY: i32 = 128;
pub const DEFAULT_WAL_WRITER_FLUSH_AFTER: i32 = 128;
pub const DEFAULT_XLOG_SEG_SIZE: i32 = 16777216;
pub const DEF_PGPORT: i32 = 5432;
pub const ERROR: i32 = types_error::ERROR.0;
pub const FATAL: i32 = types_error::FATAL.0;
pub const FILE_COPY_METHOD_CLONE: i32 = 1;
pub const FILE_COPY_METHOD_COPY: i32 = 0;
// fd.h FileExtendMethod: POSIX_FALLOCATE exists only with HAVE_POSIX_FALLOCATE.
#[cfg(target_os = "linux")]
pub const FILE_EXTEND_METHOD_POSIX_FALLOCATE: i32 = 0;
#[cfg(target_os = "linux")]
pub const FILE_EXTEND_METHOD_WRITE_ZEROS: i32 = 1;
#[cfg(not(target_os = "linux"))]
pub const FILE_EXTEND_METHOD_WRITE_ZEROS: i32 = 0;
pub const FUNC_MAX_ARGS: i32 = 100;
pub const HOURS_PER_DAY: i32 = 24;
pub const HUGE_PAGES_OFF: i32 = 0;
pub const HUGE_PAGES_ON: i32 = 1;
pub const HUGE_PAGES_TRY: i32 = 2;
pub const HUGE_PAGES_UNKNOWN: i32 = 3;
pub const INDEX_MAX_KEYS: i32 = 32;
pub const INFO: i32 = types_error::INFO.0;
pub const INTSTYLE_ISO_8601: i32 = 3;
pub const INTSTYLE_POSTGRES: i32 = 0;
pub const INTSTYLE_POSTGRES_VERBOSE: i32 = 1;
pub const INTSTYLE_SQL_STANDARD: i32 = 2;
pub const IOMETHOD_SYNC: i32 = 0;
pub const IOMETHOD_WORKER: i32 = 1;
// C enum IoMethod's IOMETHOD_IO_URING ordinal. Not listed in
// IO_METHOD_OPTIONS yet (io_uring stays unlisted until inc-2), so no GUC
// source can select it today — it exists so io_method-gated code (the uring
// prefetch route in bufmgr) states its enabling condition explicitly.
pub const IOMETHOD_IO_URING: i32 = 2;
pub const LOG: i32 = types_error::LOG.0;
pub const LOGSTMT_ALL: i32 = 3;
pub const LOGSTMT_DDL: i32 = 1;
pub const LOGSTMT_MOD: i32 = 2;
pub const LOGSTMT_NONE: i32 = 0;
pub const LOG_LOCAL0: i32 = 128;
pub const LOG_LOCAL1: i32 = 136;
pub const LOG_LOCAL2: i32 = 144;
pub const LOG_LOCAL3: i32 = 152;
pub const LOG_LOCAL4: i32 = 160;
pub const LOG_LOCAL5: i32 = 168;
pub const LOG_LOCAL6: i32 = 176;
pub const LOG_LOCAL7: i32 = 184;
pub const MAX_BACKENDS: i32 = 262143;
pub const MAX_BAS_VAC_RING_SIZE_KB: i32 = 16777216;
pub const MAX_GEQO_EFFORT: i32 = 10;
pub const MAX_GEQO_SELECTION_BIAS: f64 = 2.0;
pub const MAX_IO_CONCURRENCY: i32 = 1000;
pub const MAX_IO_WORKERS: i32 = 32;
pub const MAX_KILOBYTES: i32 = 2147483647;
pub const MAX_PARALLEL_WORKER_LIMIT: i32 = 1024;
pub const MAX_STATISTICS_TARGET: i32 = 10000;
pub const MINS_PER_HOUR: i32 = 60;
pub const MIN_GEQO_EFFORT: i32 = 1;
pub const MIN_GEQO_SELECTION_BIAS: f64 = 1.5;
pub const MaxAllocSize: i32 = 1073741823;
pub const NAMEDATALEN: i32 = 64;
pub const NOTICE: i32 = types_error::NOTICE.0;
pub const PANIC: i32 = types_error::PANIC.0;
pub const PASSWORD_TYPE_MD5: i32 = 1;
pub const PASSWORD_TYPE_SCRAM_SHA_256: i32 = 2;
pub const PGERROR_DEFAULT: i32 = 1;
pub const PGERROR_TERSE: i32 = 0;
pub const PGERROR_VERBOSE: i32 = 2;
pub const PGSTAT_FETCH_CONSISTENCY_CACHE: i32 = 1;
pub const PGSTAT_FETCH_CONSISTENCY_NONE: i32 = 0;
pub const PGSTAT_FETCH_CONSISTENCY_SNAPSHOT: i32 = 2;
pub const PG_TLS1_1_VERSION: i32 = 2;
pub const PG_TLS1_2_VERSION: i32 = 3;
pub const PG_TLS1_3_VERSION: i32 = 4;
pub const PG_TLS1_VERSION: i32 = 1;
pub const PG_TLS_ANY: i32 = 0;
pub const PG_VERSION_NUM: i32 = 180006;
pub const PLAN_CACHE_MODE_AUTO: i32 = 0;
pub const PLAN_CACHE_MODE_FORCE_CUSTOM_PLAN: i32 = 2;
pub const PLAN_CACHE_MODE_FORCE_GENERIC_PLAN: i32 = 1;
pub const RECOVERY_PREFETCH_OFF: i32 = 0;
pub const RECOVERY_PREFETCH_ON: i32 = 1;
pub const RECOVERY_PREFETCH_TRY: i32 = 2;
// pgrust-only regexp engine dispatch: auto sends provably RE2-compatible
// patterns to RE2 and everything else to Spencer (fail-closed).
pub const REGEX_ENGINE_SPENCER: i32 = 0;
pub const REGEX_ENGINE_RE2: i32 = 1;
pub const REGEX_ENGINE_AUTO: i32 = 2;
// pgrust-only version identities — the SINGLE source of truth for both
// numbers (dl-verstring ruling 2026-08-06). Every rendered banner
// (version(), the postmaster startup line) derives from these two consts;
// never write either number anywhere else in the server source. Hosted here
// because guc_tables already owns the SQL-visible `server_version` default,
// which references PG_COMPAT_VERSION below.
pub const PGRUST_VERSION: &str = "0.4-beta";
pub const PG_COMPAT_VERSION: &str = "18.6";
// The build id beside PGRUST_VERSION ("pgrust 0.4-beta, build 1a2b3c4"). It is
// NOT a build-time const here: a per-commit value in guc_tables would
// recompile every crate that depends on guc_tables after each commit. The
// server binary's own crate (main_main, which nothing depends on) stamps the
// id at build time (PGRUST_BUILD_ID, else the checkout's HEAD) and installs it
// once at process start; the banners read it from here at run time. Not
// installed (a tree without .git built without PGRUST_BUILD_ID, a unit test)
// = no id.
static PGRUST_BUILD_SUFFIX: pgsync::OnceLock<String> = pgsync::OnceLock::new();

/// Installs the build id; the first call wins. "" = no id.
pub fn set_pgrust_build_id(id: &str) {
    let id = id.trim();
    let _ = PGRUST_BUILD_SUFFIX.set(if id.is_empty() {
        String::new()
    } else {
        format!(", build {id}")
    });
}

/// ", build <id>", or "" when no id is installed.
pub fn pgrust_build_suffix() -> &'static str {
    PGRUST_BUILD_SUFFIX.get().map_or("", String::as_str)
}

/// pgrust's identity as every banner prints it: "pgrust 0.4-beta, build
/// 1a2b3c4", or "pgrust 0.4-beta" with no build id. `postgres --version`
/// never carries it: C pg_ctl / initdb / pg_upgrade / pg_rewind compare that
/// line byte for byte with "postgres (PostgreSQL) 18.6".
pub fn pgrust_identity() -> String {
    format!("pgrust {PGRUST_VERSION}{}", pgrust_build_suffix())
}
pub const RECOVERY_TARGET_ACTION_PAUSE: i32 = 0;
pub const RELSEG_SIZE: i32 = 131072;
// tcopprot.h restrict_nonsystem_relation_kind flag bits.
pub const RESTRICT_RELKIND_VIEW: i32 = 0x01;
pub const RESTRICT_RELKIND_FOREIGN_TABLE: i32 = 0x02;
pub const SCRAM_SHA_256_DEFAULT_ITERATIONS: i32 = 4096;
pub const SECS_PER_MINUTE: i32 = 60;
pub const SESSION_REPLICATION_ROLE_LOCAL: i32 = 2;
pub const SESSION_REPLICATION_ROLE_ORIGIN: i32 = 0;
pub const SESSION_REPLICATION_ROLE_REPLICA: i32 = 1;
pub const SHMEM_TYPE_MMAP: i32 = 2;
pub const SHMEM_TYPE_SYSV: i32 = 1;
pub const SLRU_MAX_ALLOWED_BUFFERS: i32 = 131072;
pub const SYNCHRONOUS_COMMIT_LOCAL_FLUSH: i32 = 1;
pub const SYNCHRONOUS_COMMIT_OFF: i32 = 0;
pub const SYNCHRONOUS_COMMIT_REMOTE_APPLY: i32 = 4;
pub const SYNCHRONOUS_COMMIT_REMOTE_FLUSH: i32 = 3;
pub const SYNCHRONOUS_COMMIT_REMOTE_WRITE: i32 = 2;
pub const TOAST_LZ4_COMPRESSION: i32 = 108;
pub const TOAST_PGLZ_COMPRESSION: i32 = 112;
pub const TRACK_FUNC_ALL: i32 = 2;
pub const TRACK_FUNC_OFF: i32 = 0;
pub const TRACK_FUNC_PL: i32 = 1;
// pgrust-only pgrust.version_string_style values: which identity leads in
// version()'s banner. postgres_first (the default) leads with the
// PostgreSQL compatibility version so first-number parsers (the
// duckdb-postgres ExtractPostgresVersion class) read it; pgrust_first
// restores the legacy pgrust-led form.
pub const VERSION_STRING_POSTGRES_FIRST: i32 = 0;
pub const VERSION_STRING_PGRUST_FIRST: i32 = 1;
pub const WAL_COMPRESSION_LZ4: i32 = 2;
pub const WAL_COMPRESSION_NONE: i32 = 0;
pub const WAL_COMPRESSION_PGLZ: i32 = 1;
pub const WAL_COMPRESSION_ZSTD: i32 = 3;
pub const WAL_LEVEL_REPLICA: i32 = 1;
pub const WAL_SYNC_METHOD_OPEN_DSYNC: i32 = 4;
pub const WAL_SYNC_METHOD_FDATASYNC: i32 = 1;
// xlogdefs.h DEFAULT_WAL_SYNC_METHOD (port/linux.h pins fdatasync).
#[cfg(target_os = "linux")]
pub const DEFAULT_WAL_SYNC_METHOD: i32 = WAL_SYNC_METHOD_FDATASYNC;
#[cfg(not(target_os = "linux"))]
pub const DEFAULT_WAL_SYNC_METHOD: i32 = WAL_SYNC_METHOD_OPEN_DSYNC;
pub const WARNING: i32 = types_error::WARNING.0;
pub const WRITEBACK_MAX_PENDING_FLUSHES: i32 = 256;
pub const WalSegMaxSize: i32 = 1073741824;
pub const WalSegMinSize: i32 = 1048576;
pub const XACT_READ_COMMITTED: i32 = 1;
pub const XACT_READ_UNCOMMITTED: i32 = 0;
pub const XACT_REPEATABLE_READ: i32 = 2;
pub const XACT_SERIALIZABLE: i32 = 3;
pub const XLOG_BLCKSZ: i32 = 8192;
pub const XMLBINARY_BASE64: i32 = 0;
pub const XMLBINARY_HEX: i32 = 1;
pub const XMLOPTION_CONTENT: i32 = 1;
pub const XMLOPTION_DOCUMENT: i32 = 0;

// pgrust.memory_broker (pgrust-only, lane membroker, memory step 5): Gravity's one memory budget. off = today's caps byte
// for byte and nothing computed; advise (default) = the budget and its consumers computed and reported, nothing applied;
// on = the actuators in pgrust.memory_broker_actuators apply it.
pub const MEMORY_BROKER_OFF: i32 = 0;
pub const MEMORY_BROKER_ADVISE: i32 = 1;
pub const MEMORY_BROKER_ON: i32 = 2;

// pgrust.gravity_admit (pgrust-only, docs/gravity/exec.md "Admission from the
// graph"): graph admits from the plan's graph; shape turns that off (a refusal).
pub const GRAVITY_ADMIT_SHAPE: i32 = 0;
pub const GRAVITY_ADMIT_GRAPH: i32 = 1;
// pgrust.point_path (pgrust-only): the statements served without the planner or Gravity (gravity exec runner/point.rs).
pub const POINT_PATH_OFF: i32 = 0;
pub const POINT_PATH_ON: i32 = 1;
pub const POINT_PATH_STRICT: i32 = 2;
// pgrust.point_noplan (pgrust-only): a point statement's plan reused from the session's memo of the plans the planner
// built for its shape (gravity exec runner/point/skip.rs).
pub const POINT_NOPLAN_OFF: i32 = 0;
pub const POINT_NOPLAN_ON: i32 = 1;
pub const POINT_NOPLAN_STRICT: i32 = 2;
// pgrust.gravity_lowerer (pgrust-only): v1 (the ladder) and v2 (lowerv2) are removed (tags ladder-final,
// lowerv2-final; the check hook refuses them); auto = v3.
pub const GRAVITY_LOWERER_V1: i32 = 0;
pub const GRAVITY_LOWERER_V2: i32 = 1;
pub const GRAVITY_LOWERER_AUTO: i32 = 2;
pub const GRAVITY_LOWERER_V3: i32 = 3;
// pgrust.gravity_groupingsets_rewrite (planner::groupingsets_union): off | on | force.
pub const GRAVITY_GS_REWRITE_OFF: i32 = 0;
pub const GRAVITY_GS_REWRITE_ON: i32 = 1;
pub const GRAVITY_GS_REWRITE_FORCE: i32 = 2;
// pgrust.gravity_unnest (planner::unnest): off | strict | contained.
pub const GRAVITY_UNNEST_OFF: i32 = 0;
pub const GRAVITY_UNNEST_STRICT: i32 = 1;
pub const GRAVITY_UNNEST_CONTAINED: i32 = 2;
// pgrust.gravity_cte_pushdown (planner::cte_or, D3342): off | strict | contained | fill (D4779: the readers' filters
// applied inside Gravity's fill of the CTE, PostgreSQL's plan as without the push).
pub const GRAVITY_CTE_PUSHDOWN_OFF: i32 = 0;
pub const GRAVITY_CTE_PUSHDOWN_STRICT: i32 = 1;
pub const GRAVITY_CTE_PUSHDOWN_CONTAINED: i32 = 2;
pub const GRAVITY_CTE_PUSHDOWN_FILL: i32 = 3;
// pgrust.gravity_exact_size (gravity exec knobs/grant.rs, exactsize): none | build | final | all, a bit set: build = the
// hash build's counted sizes, final = the owner side's, all = both.
pub const GRAVITY_EXACT_SIZE_NONE: i32 = 0;
pub const GRAVITY_EXACT_SIZE_BUILD: i32 = 1;
pub const GRAVITY_EXACT_SIZE_FINAL: i32 = 2;
pub const GRAVITY_EXACT_SIZE_ALL: i32 = 3;
// pgrust.gravity_jit_perfmap (gravity_backend perfmap): off | func | tags.
pub const GRAVITY_JIT_PERFMAP_OFF: i32 = 0;
pub const GRAVITY_JIT_PERFMAP_FUNC: i32 = 1;
pub const GRAVITY_JIT_PERFMAP_TAGS: i32 = 2;
// pgrust.memory_cancel_rule (mempolicy::decide::Rule): threshold (today's 90 % cancel, the default) | projection (C3's
// ladder: tranches, throttle, the projection cancel by fair share).
pub const MEMORY_CANCEL_RULE_THRESHOLD: i32 = 0;
pub const MEMORY_CANCEL_RULE_PROJECTION: i32 = 1;
// pgrust.memory_ladder (mempolicy::ladder, LADDER-DESIGN section 10): off (the default) | shadow (the ladder decides at
// every sample and logs it; the watchdog still acts) | on (section 10 step 3: the gates climb R1-R4b and wait on the free
// epoch; R2b and R6 stay the watchdog's until step 6).
pub const MEMORY_LADDER_OFF: i32 = 0;
pub const MEMORY_LADDER_SHADOW: i32 = 1;
pub const MEMORY_LADDER_ON: i32 = 2;
