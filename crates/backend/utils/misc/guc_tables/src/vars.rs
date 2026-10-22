#![allow(non_upper_case_globals)]

use crate::slots::{GucBoolVar, GucEnumVar, GucIntVar, GucRealVar, GucSlot, GucStringVar};

pub static GucPlaceholderVariable: GucStringVar = GucSlot::new("guc_placeholder_variable");

pub static AllowAlterSystem: GucBoolVar = GucSlot::new("AllowAlterSystem");
pub static Array_nulls: GucBoolVar = GucSlot::new("Array_nulls");
pub static AuthenticationTimeout: GucIntVar = GucSlot::new("AuthenticationTimeout");
pub static BgWriterDelay: GucIntVar = GucSlot::new("BgWriterDelay");
pub static CheckPointCompletionTarget: GucRealVar = GucSlot::new("CheckPointCompletionTarget");
pub static CheckPointTimeout: GucIntVar = GucSlot::new("CheckPointTimeout");
pub static CheckPointWarning: GucIntVar = GucSlot::new("CheckPointWarning");
pub static CommitDelay: GucIntVar = GucSlot::new("CommitDelay");
pub static CommitSiblings: GucIntVar = GucSlot::new("CommitSiblings");
pub static ConfigFileName: GucStringVar = GucSlot::new("ConfigFileName");
pub static DeadlockTimeout: GucIntVar = GucSlot::new("DeadlockTimeout");
pub static Debug_pretty_print: GucBoolVar = GucSlot::new("Debug_pretty_print");
pub static Debug_print_parse: GucBoolVar = GucSlot::new("Debug_print_parse");
pub static Debug_print_plan: GucBoolVar = GucSlot::new("Debug_print_plan");
pub static Debug_print_rewritten: GucBoolVar = GucSlot::new("Debug_print_rewritten");
pub static DefaultXactDeferrable: GucBoolVar = GucSlot::new("DefaultXactDeferrable");
pub static DefaultXactIsoLevel: GucEnumVar = GucSlot::new("DefaultXactIsoLevel");
pub static DefaultXactReadOnly: GucBoolVar = GucSlot::new("DefaultXactReadOnly");
pub static Dynamic_library_path: GucStringVar = GucSlot::new("Dynamic_library_path");
pub static EnableHotStandby: GucBoolVar = GucSlot::new("EnableHotStandby");
pub static EnableSSL: GucBoolVar = GucSlot::new("EnableSSL");
pub static ExitOnAnyError: GucBoolVar = GucSlot::new("ExitOnAnyError");
pub static Extension_control_path: GucStringVar = GucSlot::new("Extension_control_path");
pub static Geqo_effort: GucIntVar = GucSlot::new("Geqo_effort");
pub static Geqo_generations: GucIntVar = GucSlot::new("Geqo_generations");
pub static Geqo_pool_size: GucIntVar = GucSlot::new("Geqo_pool_size");
pub static Geqo_seed: GucRealVar = GucSlot::new("Geqo_seed");
pub static Geqo_selection_bias: GucRealVar = GucSlot::new("Geqo_selection_bias");
pub static GinFuzzySearchLimit: GucIntVar = GucSlot::new("GinFuzzySearchLimit");
pub static HbaFileName: GucStringVar = GucSlot::new("HbaFileName");
pub static IdentFileName: GucStringVar = GucSlot::new("IdentFileName");
pub static IdleInTransactionSessionTimeout: GucIntVar = GucSlot::new("IdleInTransactionSessionTimeout");
pub static IdleSessionTimeout: GucIntVar = GucSlot::new("IdleSessionTimeout");
pub static IgnoreSystemIndexes: GucBoolVar = GucSlot::new("IgnoreSystemIndexes");
pub static IntervalStyle: GucEnumVar = GucSlot::new("IntervalStyle");
pub static ListenAddresses: GucStringVar = GucSlot::new("ListenAddresses");
pub static LockTimeout: GucIntVar = GucSlot::new("LockTimeout");
pub static Log_RotationAge: GucIntVar = GucSlot::new("Log_RotationAge");
pub static Log_RotationSize: GucIntVar = GucSlot::new("Log_RotationSize");
pub static Log_autovacuum_min_duration: GucIntVar = GucSlot::new("Log_autovacuum_min_duration");
pub static Log_destination_string: GucStringVar = GucSlot::new("Log_destination_string");
pub static Log_directory: GucStringVar = GucSlot::new("Log_directory");
pub static Log_disconnections: GucBoolVar = GucSlot::new("Log_disconnections");
pub static Log_error_verbosity: GucEnumVar = GucSlot::new("Log_error_verbosity");
pub static Log_file_mode: GucIntVar = GucSlot::new("Log_file_mode");
pub static Log_filename: GucStringVar = GucSlot::new("Log_filename");
pub static Log_line_prefix: GucStringVar = GucSlot::new("Log_line_prefix");
pub static Log_truncate_on_rotation: GucBoolVar = GucSlot::new("Log_truncate_on_rotation");
pub static Logging_collector: GucBoolVar = GucSlot::new("Logging_collector");
pub static MaxConnections: GucIntVar = GucSlot::new("MaxConnections");
pub static NBuffers: GucIntVar = GucSlot::new("NBuffers");
pub static Password_encryption: GucEnumVar = GucSlot::new("Password_encryption");
pub static PostAuthDelay: GucIntVar = GucSlot::new("PostAuthDelay");
pub static PostPortNumber: GucIntVar = GucSlot::new("PostPortNumber");
pub static PreAuthDelay: GucIntVar = GucSlot::new("PreAuthDelay");
pub static PrimaryConnInfo: GucStringVar = GucSlot::new("PrimaryConnInfo");
pub static PrimarySlotName: GucStringVar = GucSlot::new("PrimarySlotName");
pub static ReservedConnections: GucIntVar = GucSlot::new("ReservedConnections");
pub static SSLCipherList: GucStringVar = GucSlot::new("SSLCipherList");
pub static SSLCipherSuites: GucStringVar = GucSlot::new("SSLCipherSuites");
pub static SSLECDHCurve: GucStringVar = GucSlot::new("SSLECDHCurve");
pub static SSLPreferServerCiphers: GucBoolVar = GucSlot::new("SSLPreferServerCiphers");
pub static SessionReplicationRole: GucEnumVar = GucSlot::new("SessionReplicationRole");
pub static StatementTimeout: GucIntVar = GucSlot::new("StatementTimeout");
pub static SuperuserReservedConnections: GucIntVar = GucSlot::new("SuperuserReservedConnections");
pub static SyncRepStandbyNames: GucStringVar = GucSlot::new("SyncRepStandbyNames");
pub static TSCurrentConfig: GucStringVar = GucSlot::new("TSCurrentConfig");
pub static Trace_connection_negotiation: GucBoolVar = GucSlot::new("Trace_connection_negotiation");
pub static Trace_notify: GucBoolVar = GucSlot::new("Trace_notify");
pub static TransactionTimeout: GucIntVar = GucSlot::new("TransactionTimeout");
pub static Transform_null_equals: GucBoolVar = GucSlot::new("Transform_null_equals");
pub static Unix_socket_directories: GucStringVar = GucSlot::new("Unix_socket_directories");
pub static Unix_socket_group: GucStringVar = GucSlot::new("Unix_socket_group");
pub static Unix_socket_permissions: GucIntVar = GucSlot::new("Unix_socket_permissions");
pub static VacuumBufferUsageLimit: GucIntVar = GucSlot::new("VacuumBufferUsageLimit");
pub static VacuumCostDelay: GucRealVar = GucSlot::new("VacuumCostDelay");
pub static VacuumCostLimit: GucIntVar = GucSlot::new("VacuumCostLimit");
pub static VacuumCostPageDirty: GucIntVar = GucSlot::new("VacuumCostPageDirty");
pub static VacuumCostPageHit: GucIntVar = GucSlot::new("VacuumCostPageHit");
pub static VacuumCostPageMiss: GucIntVar = GucSlot::new("VacuumCostPageMiss");
pub static WalWriterDelay: GucIntVar = GucSlot::new("WalWriterDelay");
pub static WalWriterFlushAfter: GucIntVar = GucSlot::new("WalWriterFlushAfter");
pub static XLOGbuffers: GucIntVar = GucSlot::new("XLOGbuffers");
pub static XLogArchiveCommand: GucStringVar = GucSlot::new("XLogArchiveCommand");
pub static XLogArchiveLibrary: GucStringVar = GucSlot::new("XLogArchiveLibrary");
pub static XLogArchiveMode: GucEnumVar = GucSlot::new("XLogArchiveMode");
pub static XLogArchiveTimeout: GucIntVar = GucSlot::new("XLogArchiveTimeout");
pub static XactDeferrable: GucBoolVar = GucSlot::new("XactDeferrable");
pub static XactIsoLevel: GucEnumVar = GucSlot::new("XactIsoLevel");
pub static XactReadOnly: GucBoolVar = GucSlot::new("XactReadOnly");
pub static allowSystemTableMods: GucBoolVar = GucSlot::new("allowSystemTableMods");
pub static allow_in_place_tablespaces: GucBoolVar = GucSlot::new("allow_in_place_tablespaces");
pub static application_name: GucStringVar = GucSlot::new("application_name");
pub static archiveCleanupCommand: GucStringVar = GucSlot::new("archiveCleanupCommand");
pub static assert_enabled: GucBoolVar = GucSlot::new("assert_enabled");
pub static autovacuum_anl_scale: GucRealVar = GucSlot::new("autovacuum_anl_scale");
pub static autovacuum_anl_thresh: GucIntVar = GucSlot::new("autovacuum_anl_thresh");
pub static autovacuum_freeze_max_age: GucIntVar = GucSlot::new("autovacuum_freeze_max_age");
pub static autovacuum_max_workers: GucIntVar = GucSlot::new("autovacuum_max_workers");
pub static autovacuum_multixact_freeze_max_age: GucIntVar = GucSlot::new("autovacuum_multixact_freeze_max_age");
pub static autovacuum_naptime: GucIntVar = GucSlot::new("autovacuum_naptime");
pub static autovacuum_start_daemon: GucBoolVar = GucSlot::new("autovacuum_start_daemon");
pub static autovacuum_vac_cost_delay: GucRealVar = GucSlot::new("autovacuum_vac_cost_delay");
pub static autovacuum_vac_cost_limit: GucIntVar = GucSlot::new("autovacuum_vac_cost_limit");
pub static autovacuum_vac_ins_scale: GucRealVar = GucSlot::new("autovacuum_vac_ins_scale");
pub static autovacuum_vac_ins_thresh: GucIntVar = GucSlot::new("autovacuum_vac_ins_thresh");
pub static autovacuum_vac_max_thresh: GucIntVar = GucSlot::new("autovacuum_vac_max_thresh");
pub static autovacuum_vac_scale: GucRealVar = GucSlot::new("autovacuum_vac_scale");
pub static autovacuum_vac_thresh: GucIntVar = GucSlot::new("autovacuum_vac_thresh");
pub static autovacuum_work_mem: GucIntVar = GucSlot::new("autovacuum_work_mem");
pub static autovacuum_worker_slots: GucIntVar = GucSlot::new("autovacuum_worker_slots");
pub static backend_flush_after: GucIntVar = GucSlot::new("backend_flush_after");
pub static backslash_quote: GucEnumVar = GucSlot::new("backslash_quote");
pub static backtrace_functions: GucStringVar = GucSlot::new("backtrace_functions");
pub static bgwriter_flush_after: GucIntVar = GucSlot::new("bgwriter_flush_after");
pub static bgwriter_lru_maxpages: GucIntVar = GucSlot::new("bgwriter_lru_maxpages");
pub static bgwriter_lru_multiplier: GucRealVar = GucSlot::new("bgwriter_lru_multiplier");
pub static block_size: GucIntVar = GucSlot::new("block_size");
pub static bonjour_name: GucStringVar = GucSlot::new("bonjour_name");
pub static bytea_output: GucEnumVar = GucSlot::new("bytea_output");
pub static check_function_bodies: GucBoolVar = GucSlot::new("check_function_bodies");
pub static checkpoint_flush_after: GucIntVar = GucSlot::new("checkpoint_flush_after");
pub static client_connection_check_interval: GucIntVar = GucSlot::new("client_connection_check_interval");
pub static client_encoding_string: GucStringVar = GucSlot::new("client_encoding_string");
pub static client_min_messages: GucEnumVar = GucSlot::new("client_min_messages");
pub static cluster_name: GucStringVar = GucSlot::new("cluster_name");
pub static commit_timestamp_buffers: GucIntVar = GucSlot::new("commit_timestamp_buffers");
pub static compute_query_id: GucEnumVar = GucSlot::new("compute_query_id");
pub static constraint_exclusion: GucEnumVar = GucSlot::new("constraint_exclusion");
pub static cpu_index_tuple_cost: GucRealVar = GucSlot::new("cpu_index_tuple_cost");
pub static cpu_operator_cost: GucRealVar = GucSlot::new("cpu_operator_cost");
pub static cpu_tuple_cost: GucRealVar = GucSlot::new("cpu_tuple_cost");
pub static createrole_self_grant: GucStringVar = GucSlot::new("createrole_self_grant");
pub static current_role_is_superuser: GucBoolVar = GucSlot::new("current_role_is_superuser");
pub static cursor_tuple_fraction: GucRealVar = GucSlot::new("cursor_tuple_fraction");
pub static data_checksums: GucBoolVar = GucSlot::new("data_checksums");
pub static data_directory: GucStringVar = GucSlot::new("data_directory");
pub static data_directory_mode: GucIntVar = GucSlot::new("data_directory_mode");
pub static data_sync_retry: GucBoolVar = GucSlot::new("data_sync_retry");
pub static datestyle_string: GucStringVar = GucSlot::new("datestyle_string");
pub static debug_discard_caches: GucIntVar = GucSlot::new("debug_discard_caches");
pub static debug_io_direct_string: GucStringVar = GucSlot::new("debug_io_direct_string");
pub static debug_logical_replication_streaming: GucEnumVar = GucSlot::new("debug_logical_replication_streaming");
pub static debug_parallel_query: GucEnumVar = GucSlot::new("debug_parallel_query");
pub static default_statistics_target: GucIntVar = GucSlot::new("default_statistics_target");
pub static default_table_access_method: GucStringVar = GucSlot::new("default_table_access_method");
pub static default_tablespace: GucStringVar = GucSlot::new("default_tablespace");
pub static default_toast_compression: GucEnumVar = GucSlot::new("default_toast_compression");
pub static default_with_oids: GucBoolVar = GucSlot::new("default_with_oids");
pub static dynamic_shared_memory_type: GucEnumVar = GucSlot::new("dynamic_shared_memory_type");
pub static effective_cache_size: GucIntVar = GucSlot::new("effective_cache_size");
pub static effective_io_concurrency: GucIntVar = GucSlot::new("effective_io_concurrency");
pub static enableFsync: GucBoolVar = GucSlot::new("enableFsync");
pub static enable_async_append: GucBoolVar = GucSlot::new("enable_async_append");
pub static enable_bitmapscan: GucBoolVar = GucSlot::new("enable_bitmapscan");
pub static enable_bonjour: GucBoolVar = GucSlot::new("enable_bonjour");
pub static enable_distinct_reordering: GucBoolVar = GucSlot::new("enable_distinct_reordering");
pub static enable_gathermerge: GucBoolVar = GucSlot::new("enable_gathermerge");
pub static enable_geqo: GucBoolVar = GucSlot::new("enable_geqo");
pub static enable_group_by_reordering: GucBoolVar = GucSlot::new("enable_group_by_reordering");
pub static enable_hashagg: GucBoolVar = GucSlot::new("enable_hashagg");
pub static enable_hashjoin: GucBoolVar = GucSlot::new("enable_hashjoin");
pub static enable_incremental_sort: GucBoolVar = GucSlot::new("enable_incremental_sort");
pub static enable_indexonlyscan: GucBoolVar = GucSlot::new("enable_indexonlyscan");
pub static enable_indexscan: GucBoolVar = GucSlot::new("enable_indexscan");
pub static enable_material: GucBoolVar = GucSlot::new("enable_material");
pub static enable_memoize: GucBoolVar = GucSlot::new("enable_memoize");
pub static enable_mergejoin: GucBoolVar = GucSlot::new("enable_mergejoin");
pub static enable_nestloop: GucBoolVar = GucSlot::new("enable_nestloop");
pub static enable_parallel_append: GucBoolVar = GucSlot::new("enable_parallel_append");
pub static enable_parallel_hash: GucBoolVar = GucSlot::new("enable_parallel_hash");
pub static enable_partition_pruning: GucBoolVar = GucSlot::new("enable_partition_pruning");
pub static enable_partitionwise_aggregate: GucBoolVar = GucSlot::new("enable_partitionwise_aggregate");
pub static enable_partitionwise_join: GucBoolVar = GucSlot::new("enable_partitionwise_join");
pub static enable_presorted_aggregate: GucBoolVar = GucSlot::new("enable_presorted_aggregate");
pub static enable_self_join_elimination: GucBoolVar = GucSlot::new("enable_self_join_elimination");
pub static enable_seqscan: GucBoolVar = GucSlot::new("enable_seqscan");
pub static enable_sort: GucBoolVar = GucSlot::new("enable_sort");
pub static enable_tidscan: GucBoolVar = GucSlot::new("enable_tidscan");
pub static escape_string_warning: GucBoolVar = GucSlot::new("escape_string_warning");
pub static event_source: GucStringVar = GucSlot::new("event_source");
pub static event_triggers: GucBoolVar = GucSlot::new("event_triggers");
pub static external_pid_file: GucStringVar = GucSlot::new("external_pid_file");
pub static extra_float_digits: GucIntVar = GucSlot::new("extra_float_digits");
pub static file_copy_method: GucEnumVar = GucSlot::new("file_copy_method");
pub static file_extend_method: GucEnumVar = GucSlot::new("file_extend_method");
pub static from_collapse_limit: GucIntVar = GucSlot::new("from_collapse_limit");
pub static fullPageWrites: GucBoolVar = GucSlot::new("fullPageWrites");
pub static geqo_threshold: GucIntVar = GucSlot::new("geqo_threshold");
pub static gin_pending_list_limit: GucIntVar = GucSlot::new("gin_pending_list_limit");
pub static hash_mem_multiplier: GucRealVar = GucSlot::new("hash_mem_multiplier");
pub static hot_standby_feedback: GucBoolVar = GucSlot::new("hot_standby_feedback");
pub static huge_page_size: GucIntVar = GucSlot::new("huge_page_size");
pub static huge_pages: GucEnumVar = GucSlot::new("huge_pages");
pub static huge_pages_status: GucEnumVar = GucSlot::new("huge_pages_status");
pub static icu_validation_level: GucEnumVar = GucSlot::new("icu_validation_level");
pub static idle_replication_slot_timeout_secs: GucIntVar = GucSlot::new("idle_replication_slot_timeout_secs");
pub static ignore_checksum_failure: GucBoolVar = GucSlot::new("ignore_checksum_failure");
pub static ignore_invalid_pages: GucBoolVar = GucSlot::new("ignore_invalid_pages");
pub static in_hot_standby_guc: GucBoolVar = GucSlot::new("in_hot_standby_guc");
pub static integer_datetimes: GucBoolVar = GucSlot::new("integer_datetimes");
pub static io_combine_limit_guc: GucIntVar = GucSlot::new("io_combine_limit_guc");
pub static io_max_combine_limit: GucIntVar = GucSlot::new("io_max_combine_limit");
pub static io_max_concurrency: GucIntVar = GucSlot::new("io_max_concurrency");
pub static io_method: GucEnumVar = GucSlot::new("io_method");
pub static io_workers: GucIntVar = GucSlot::new("io_workers");
pub static jit_above_cost: GucRealVar = GucSlot::new("jit_above_cost");
pub static jit_debugging_support: GucBoolVar = GucSlot::new("jit_debugging_support");
pub static jit_dump_bitcode: GucBoolVar = GucSlot::new("jit_dump_bitcode");
pub static jit_enabled: GucBoolVar = GucSlot::new("jit_enabled");
pub static jit_expressions: GucBoolVar = GucSlot::new("jit_expressions");
pub static jit_inline_above_cost: GucRealVar = GucSlot::new("jit_inline_above_cost");
pub static jit_optimize_above_cost: GucRealVar = GucSlot::new("jit_optimize_above_cost");
pub static jit_profiling_support: GucBoolVar = GucSlot::new("jit_profiling_support");
pub static jit_provider: GucStringVar = GucSlot::new("jit_provider");
pub static jit_tuple_deforming: GucBoolVar = GucSlot::new("jit_tuple_deforming");
pub static join_collapse_limit: GucIntVar = GucSlot::new("join_collapse_limit");
pub static regex_engine: GucEnumVar = GucSlot::new("regex_engine");
pub static lo_compat_privileges: GucBoolVar = GucSlot::new("lo_compat_privileges");
pub static local_preload_libraries_string: GucStringVar = GucSlot::new("local_preload_libraries_string");
pub static locale_messages: GucStringVar = GucSlot::new("locale_messages");
pub static locale_monetary: GucStringVar = GucSlot::new("locale_monetary");
pub static locale_numeric: GucStringVar = GucSlot::new("locale_numeric");
pub static locale_time: GucStringVar = GucSlot::new("locale_time");
pub static log_checkpoints: GucBoolVar = GucSlot::new("log_checkpoints");
pub static log_connections_string: GucStringVar = GucSlot::new("log_connections_string");
pub static log_duration: GucBoolVar = GucSlot::new("log_duration");
pub static log_executor_stats: GucBoolVar = GucSlot::new("log_executor_stats");
pub static log_hostname: GucBoolVar = GucSlot::new("log_hostname");
pub static log_lock_failures: GucBoolVar = GucSlot::new("log_lock_failures");
pub static log_lock_waits: GucBoolVar = GucSlot::new("log_lock_waits");
pub static log_min_duration_sample: GucIntVar = GucSlot::new("log_min_duration_sample");
pub static log_min_duration_statement: GucIntVar = GucSlot::new("log_min_duration_statement");
pub static log_min_error_statement: GucEnumVar = GucSlot::new("log_min_error_statement");
pub static log_min_messages: GucEnumVar = GucSlot::new("log_min_messages");
pub static log_parameter_max_length: GucIntVar = GucSlot::new("log_parameter_max_length");
pub static log_parameter_max_length_on_error: GucIntVar = GucSlot::new("log_parameter_max_length_on_error");
pub static log_parser_stats: GucBoolVar = GucSlot::new("log_parser_stats");
pub static log_planner_stats: GucBoolVar = GucSlot::new("log_planner_stats");
pub static log_recovery_conflict_waits: GucBoolVar = GucSlot::new("log_recovery_conflict_waits");
pub static log_replication_commands: GucBoolVar = GucSlot::new("log_replication_commands");
pub static log_startup_progress_interval: GucIntVar = GucSlot::new("log_startup_progress_interval");
pub static log_statement: GucEnumVar = GucSlot::new("log_statement");
pub static log_statement_sample_rate: GucRealVar = GucSlot::new("log_statement_sample_rate");
pub static log_statement_stats: GucBoolVar = GucSlot::new("log_statement_stats");
pub static log_temp_files: GucIntVar = GucSlot::new("log_temp_files");
pub static log_timezone_string: GucStringVar = GucSlot::new("log_timezone_string");
pub static log_xact_sample_rate: GucRealVar = GucSlot::new("log_xact_sample_rate");
pub static logical_decoding_work_mem: GucIntVar = GucSlot::new("logical_decoding_work_mem");
pub static maintenance_io_concurrency: GucIntVar = GucSlot::new("maintenance_io_concurrency");
pub static maintenance_work_mem: GucIntVar = GucSlot::new("maintenance_work_mem");
pub static max_active_replication_origins: GucIntVar = GucSlot::new("max_active_replication_origins");
// pgrust-only (docs/design/connection-scaling.md D1): statement-admission gate.
pub static max_active_queries: GucIntVar = GucSlot::new("max_active_queries");
// pgrust-only (docs/design/connection-scaling.md D6): connection admission queue.
pub static connection_queue_size: GucIntVar = GucSlot::new("connection_queue_size");
pub static connection_queue_timeout: GucIntVar = GucSlot::new("connection_queue_timeout");
// pgrust-only (connection-scaling.md §D6 admission bypass, no C symbol):
// the startup-packet claim flag and the interactive-client application_name
// prefix list that implies it.
pub static pgrust_admission_bypass: GucBoolVar = GucSlot::new("pgrust_admission_bypass");
pub static pgrust_admission_bypass_applications: GucStringVar = GucSlot::new("pgrust_admission_bypass_applications");
// pgrust-only (docs/design/connection-scaling.md D3.4): idle passivation.
pub static idle_passivate_timeout: GucIntVar = GucSlot::new("idle_passivate_timeout");
// pgrust-only (connection-scaling wave 4): stack release at passivation.
pub static idle_passivate_stack: GucBoolVar = GucSlot::new("idle_passivate_stack");
// pgrust-only (docs/design/connection-scaling.md D3.1): bounded L1 caches.
pub static catcache_size_limit: GucIntVar = GucSlot::new("catcache_size_limit");
pub static relcache_size_limit: GucIntVar = GucSlot::new("relcache_size_limit");
// pgrust-only (docs/design/connection-scaling.md D3.2): shared immutable L2.
pub static shared_catalog_cache: GucBoolVar = GucSlot::new("shared_catalog_cache");
pub static max_files_per_process: GucIntVar = GucSlot::new("max_files_per_process");
pub static max_function_args: GucIntVar = GucSlot::new("max_function_args");
pub static max_identifier_length: GucIntVar = GucSlot::new("max_identifier_length");
pub static max_index_keys: GucIntVar = GucSlot::new("max_index_keys");
pub static max_locks_per_xact: GucIntVar = GucSlot::new("max_locks_per_xact");
pub static max_logical_replication_workers: GucIntVar = GucSlot::new("max_logical_replication_workers");
pub static max_notify_queue_pages: GucIntVar = GucSlot::new("max_notify_queue_pages");
pub static max_parallel_apply_workers_per_subscription: GucIntVar = GucSlot::new("max_parallel_apply_workers_per_subscription");
pub static max_parallel_maintenance_workers: GucIntVar = GucSlot::new("max_parallel_maintenance_workers");
pub static max_parallel_workers: GucIntVar = GucSlot::new("max_parallel_workers");
pub static max_parallel_workers_per_gather: GucIntVar = GucSlot::new("max_parallel_workers_per_gather");
pub static max_predicate_locks_per_page: GucIntVar = GucSlot::new("max_predicate_locks_per_page");
pub static max_predicate_locks_per_relation: GucIntVar = GucSlot::new("max_predicate_locks_per_relation");
pub static max_predicate_locks_per_xact: GucIntVar = GucSlot::new("max_predicate_locks_per_xact");
pub static max_prepared_xacts: GucIntVar = GucSlot::new("max_prepared_xacts");
pub static max_replication_slots: GucIntVar = GucSlot::new("max_replication_slots");
pub static max_slot_wal_keep_size_mb: GucIntVar = GucSlot::new("max_slot_wal_keep_size_mb");
pub static max_stack_depth: GucIntVar = GucSlot::new("max_stack_depth");
pub static max_standby_archive_delay: GucIntVar = GucSlot::new("max_standby_archive_delay");
pub static max_standby_streaming_delay: GucIntVar = GucSlot::new("max_standby_streaming_delay");
pub static max_sync_workers_per_subscription: GucIntVar = GucSlot::new("max_sync_workers_per_subscription");
pub static max_wal_senders: GucIntVar = GucSlot::new("max_wal_senders");
pub static max_wal_size_mb: GucIntVar = GucSlot::new("max_wal_size_mb");
pub static max_worker_processes: GucIntVar = GucSlot::new("max_worker_processes");
pub static md5_password_warnings: GucBoolVar = GucSlot::new("md5_password_warnings");
pub static min_dynamic_shared_memory: GucIntVar = GucSlot::new("min_dynamic_shared_memory");
pub static min_parallel_index_scan_size: GucIntVar = GucSlot::new("min_parallel_index_scan_size");
pub static min_parallel_table_scan_size: GucIntVar = GucSlot::new("min_parallel_table_scan_size");
pub static min_wal_size_mb: GucIntVar = GucSlot::new("min_wal_size_mb");
pub static multixact_member_buffers: GucIntVar = GucSlot::new("multixact_member_buffers");
pub static multixact_offset_buffers: GucIntVar = GucSlot::new("multixact_offset_buffers");
pub static namespace_search_path: GucStringVar = GucSlot::new("namespace_search_path");
pub static notify_buffers: GucIntVar = GucSlot::new("notify_buffers");
pub static num_os_semaphores: GucIntVar = GucSlot::new("num_os_semaphores");
pub static num_temp_buffers: GucIntVar = GucSlot::new("num_temp_buffers");
pub static oauth_validator_libraries_string: GucStringVar = GucSlot::new("oauth_validator_libraries_string");
pub static output_plugin_libraries_string: GucStringVar = GucSlot::new("output_plugin_libraries_string");
pub static jwt_validator_jwks_uri: GucStringVar = GucSlot::new("jwt_validator_jwks_uri");
pub static jwt_validator_audience: GucStringVar = GucSlot::new("jwt_validator_audience");
pub static jwt_validator_identity_claim: GucStringVar = GucSlot::new("jwt_validator_identity_claim");
pub static jwt_validator_introspection_uri: GucStringVar = GucSlot::new("jwt_validator_introspection_uri");
pub static jwt_validator_introspection_client_id: GucStringVar = GucSlot::new("jwt_validator_introspection_client_id");
pub static jwt_validator_introspection_client_secret: GucStringVar = GucSlot::new("jwt_validator_introspection_client_secret");
pub static jwt_validator_ca_file: GucStringVar = GucSlot::new("jwt_validator_ca_file");
pub static jwt_validator_require_scopes: GucBoolVar = GucSlot::new("jwt_validator_require_scopes");
pub static jwt_validator_allow_insecure_http: GucBoolVar = GucSlot::new("jwt_validator_allow_insecure_http");
pub static jwt_validator_clock_skew: GucIntVar = GucSlot::new("jwt_validator_clock_skew");
pub static jwt_validator_jwks_cache_ttl: GucIntVar = GucSlot::new("jwt_validator_jwks_cache_ttl");
pub static jwt_validator_http_timeout: GucIntVar = GucSlot::new("jwt_validator_http_timeout");
#[cfg(feature = "oauth-test-validator")]
pub static oauth_validator_authn_id: GucStringVar = GucSlot::new("oauth_validator_authn_id");
#[cfg(feature = "oauth-test-validator")]
pub static oauth_validator_authorize_tokens: GucBoolVar = GucSlot::new("oauth_validator_authorize_tokens");
pub static parallel_leader_participation: GucBoolVar = GucSlot::new("parallel_leader_participation");
pub static parallel_setup_cost: GucRealVar = GucSlot::new("parallel_setup_cost");
pub static parallel_tuple_cost: GucRealVar = GucSlot::new("parallel_tuple_cost");
pub static pg_gss_accept_delegation: GucBoolVar = GucSlot::new("pg_gss_accept_delegation");
pub static pgrust_condition_cache: GucEnumVar = GucSlot::new("pgrust_condition_cache");
pub static pgrust_gravity_prewarm: GucBoolVar = GucSlot::new("pgrust_gravity_prewarm");
pub static pgrust_zone_store: GucBoolVar = GucSlot::new("pgrust_zone_store");
pub static pgrust_dict_filter_cache: GucBoolVar = GucSlot::new("pgrust_dict_filter_cache");
// pgrust-only: pgrust.condition_cache_unlogged - unlogged pgrcolumnar relations enter the condition cache (default on).
pub static pgrust_condition_cache_unlogged: GucBoolVar = GucSlot::new("pgrust_condition_cache_unlogged");
// pgrust-only: pgrust.pgrc_merge_scan - the planner's merged sorted path over a pgrcolumnar sorted_by table.
pub static pgrust_pgrc_merge_scan: GucBoolVar = GucSlot::new("pgrust_pgrc_merge_scan");
// pgrust-only: pgrust.pgrc_merge_skip_hash_group (D4568 part 2) - no sorted path whose order only presorts a hashable GROUP BY.
pub static pgrust_pgrc_merge_skip_hash_group: GucBoolVar = GucSlot::new("pgrust_pgrc_merge_skip_hash_group");
// pgrust-only: pgrust.jsonb_path_stats (no C symbol) - per-path jsonb statistics.
pub static pgrust_jsonb_path_stats: GucBoolVar = GucSlot::new("pgrust_jsonb_path_stats");
// pgrust-only: pgrust.gravity_admit (no C symbol) - `graph` (the default)
// admits from the plan's graph, `shape` turns that off.
pub static pgrust_gravity_admit: GucEnumVar = GucSlot::new("pgrust_gravity_admit");
pub static pgrust_gravity_lowerer: GucEnumVar = GucSlot::new("pgrust_gravity_lowerer");
// pgrust-only: pgrust.point_path (no C symbol) - off, on, strict.
pub static pgrust_point_path: GucEnumVar = GucSlot::new("pgrust_point_path");
// pgrust-only: pgrust.point_noplan (no C symbol) - off, on, strict.
pub static pgrust_point_noplan: GucEnumVar = GucSlot::new("pgrust_point_noplan");
// pgrust-only: pgrust.point_write (no C symbol) - pglpoint S3, on by default (D4572).
pub static pgrust_point_write: GucBoolVar = GucSlot::new("pgrust_point_write");
// pgrust-only: pgrust.point_keep (no C symbol) - pglpoint S3 option (a), on by default (D4572).
pub static pgrust_point_keep: GucBoolVar = GucSlot::new("pgrust_point_keep");
// pgrust-only: pgrust.point_portal_keep (no C symbol) - an S1x portal's probe kept per generic plan, on by default.
pub static pgrust_point_portal_keep: GucBoolVar = GucSlot::new("pgrust_point_portal_keep");
// pgrust-only: pgrust.gravity_dump_dir (no C symbol) - superuser only; non-empty: every Gravity program compiled
// (a program-cache miss) is written there as IR + machine code + meta (gravity_backend::dump). Env seed PGRUST_GRAVITY_DUMP_IR.
pub static pgrust_gravity_dump_dir: GucStringVar = GucSlot::new("pgrust_gravity_dump_dir");
// pgrust-only: pgrust.gravity_groupingsets_rewrite (no C symbol) - the planner's
// union-of-groupings alternative for GROUPING SETS: off, on (costed, the default), force.
pub static pgrust_gravity_groupingsets_rewrite: GucEnumVar = GucSlot::new("pgrust_gravity_groupingsets_rewrite");
// pgrust-only: pgrust.gravity_unnest (no C symbol) - the planner's unnesting alternative for correlated
// scalar aggregates: off (the default), strict, contained.
pub static pgrust_gravity_unnest: GucEnumVar = GucSlot::new("pgrust_gravity_unnest");
// pgrust-only: pgrust.gravity_cte_pushdown (no C symbol, D3342) - the OR of a multiply-read CTE's readers' filters pushed
// into its body: off (the default), strict, contained, fill (the readers' filters inside Gravity's CTE fill, D4779).
pub static pgrust_gravity_cte_pushdown: GucEnumVar = GucSlot::new("pgrust_gravity_cte_pushdown");
// pgrust-only: pgrust.gravity_exact_size (no C symbol) - a developer switch: Gravity sizes a structure from its count once
// the count is known (none, the default; build; final; all).
pub static pgrust_gravity_exact_size: GucEnumVar = GucSlot::new("pgrust_gravity_exact_size");
// pgrust-only: pgrust.gravity_jit_perfmap (no C symbol) - superuser only; the JIT perf map /tmp/perf-<pid>.map (off, func, tags).
pub static pgrust_gravity_jit_perfmap: GucEnumVar = GucSlot::new("pgrust_gravity_jit_perfmap");
pub static pgrust_condition_cache_size: GucIntVar = GucSlot::new("pgrust_condition_cache_size");
// pgrust-only: pgrust.version_string_style (dl-verstring ruling 2026-08-06,
// no C symbol) — selects which identity leads in version()'s banner
// (consts::VERSION_STRING_*). Default postgres_first so first-number parsers
// read the PostgreSQL compatibility version; `server_version` is unaffected.
pub static pgrust_version_string_style: GucEnumVar = GucSlot::new("pgrust_version_string_style");
// pgrust-only (env-to-guc train, no C symbol): pgrust.runtime is the M0 master
// switch for the runtime pool (PGC_POSTMASTER); pgrust.mem_autotune gates the
// boot-time memory auto-tune (PGC_POSTMASTER).
pub static pgrust_runtime: GucBoolVar = GucSlot::new("pgrust_runtime");
pub static pgrust_mem_autotune: GucBoolVar = GucSlot::new("pgrust_mem_autotune");
pub static pgrust_runtime_vacuum_pool: GucBoolVar = GucSlot::new("pgrust_runtime_vacuum_pool");
// pgrust-only (regression, no C symbol): the memory watchdog family —
// master switch, breach context-dump fan-out, sampler cadence (ms), base
// warn threshold (percent of limit), absolute limit (MB, 0 = cgroup auto),
// and the developer per-query context hog for the standing e2e.
pub static pgrust_memory_watchdog: GucBoolVar = GucSlot::new("pgrust_memory_watchdog");
pub static pgrust_memory_watchdog_dump: GucBoolVar = GucSlot::new("pgrust_memory_watchdog_dump");
pub static pgrust_memory_revoke: GucBoolVar = GucSlot::new("pgrust_memory_revoke");
pub static pgrust_memory_ledger_v2: GucBoolVar = GucSlot::new("pgrust_memory_ledger_v2");
pub static pgrust_memory_watchdog_interval: GucIntVar = GucSlot::new("pgrust_memory_watchdog_interval");
pub static pgrust_memory_watchdog_threshold: GucIntVar = GucSlot::new("pgrust_memory_watchdog_threshold");
pub static pgrust_memory_watchdog_limit: GucIntVar = GucSlot::new("pgrust_memory_watchdog_limit");
// pgrust-only (no C symbol): the online-resizable buffer pool.
pub static pgrust_buffer_pool_max: GucIntVar = GucSlot::new("pgrust_buffer_pool_max");
pub static pgrust_buffer_pool_thp: GucBoolVar = GucSlot::new("pgrust_buffer_pool_thp");
pub static pgrust_buffer_pool_relocate: GucBoolVar = GucSlot::new("pgrust_buffer_pool_relocate");
pub static pgrust_buffer_pool_drain_rate: GucIntVar = GucSlot::new("pgrust_buffer_pool_drain_rate");
pub static pgrust_memory_governor_threshold: GucIntVar = GucSlot::new("pgrust_memory_governor_threshold");
pub static pgrust_statement_mem_limit: GucIntVar = GucSlot::new("pgrust_statement_mem_limit");
pub static pgrust_memory_cancel_threshold: GucIntVar = GucSlot::new("pgrust_memory_cancel_threshold");
// pgrust-only: pgrust.memory_cancel_rule (no C symbol) - threshold (the default) | projection (mempolicy::decide).
pub static pgrust_memory_cancel_rule: GucEnumVar = GucSlot::new("pgrust_memory_cancel_rule");
// pgrust-only: pgrust.memory_ladder (no C symbol) - off (the default) | shadow | on (mempolicy::ladder, mcx::ladder, LADDER-DESIGN).
pub static pgrust_memory_ladder: GucEnumVar = GucSlot::new("pgrust_memory_ladder");
// pgrust-only (no C symbol): idle time (ms) after which a session hands freed
// allocator memory back to the OS.
pub static pgrust_memory_release_idle: GucIntVar = GucSlot::new("pgrust_memory_release_idle");
pub static pgrust_memory_watchdog_test_hog: GucIntVar = GucSlot::new("pgrust_memory_watchdog_test_hog");
// pgrust-only (no C symbol): context-byte growth (kB) past which a finished
// statement hands freed allocator memory back to the OS.
pub static pgrust_memory_release_threshold: GucIntVar = GucSlot::new("pgrust_memory_release_threshold");
// pgrust-only (no C symbol; lane membroker, memory step 5): Gravity's one memory budget and its broker.
pub static pgrust_memory_target: GucIntVar = GucSlot::new("pgrust_memory_target");
pub static pgrust_memory_broker: GucEnumVar = GucSlot::new("pgrust_memory_broker");
pub static pgrust_memory_broker_actuators: GucStringVar = GucSlot::new("pgrust_memory_broker_actuators");
pub static pgrust_memory_broker_interval: GucIntVar = GucSlot::new("pgrust_memory_broker_interval");
pub static pgrust_memory_page_cache_floor_mb: GucIntVar = GucSlot::new("pgrust_memory_page_cache_floor_mb");
pub static pgrust_memory_reserve_mb: GucIntVar = GucSlot::new("pgrust_memory_reserve_mb");
pub static pgrust_pgrc_part_cache_mb: GucIntVar = GucSlot::new("pgrust_pgrc_part_cache_mb");
// pgrust-only (docs/design/test-views.md D1, no C symbol): the ephemeral-
// database janitor. prefix (PGC_POSTMASTER, '' = feature off) arms janitor
// registration; grace (PGC_SIGHUP, seconds) is the reap idle threshold the
// janitor re-reads every tick.
pub static pgrust_ephemeral_db_prefix: GucStringVar = GucSlot::new("pgrust_ephemeral_db_prefix");
pub static pgrust_ephemeral_db_grace: GucIntVar = GucSlot::new("pgrust_ephemeral_db_grace");
// pgrust-only (docs/design/test-views.md D2, no C symbol): mint-on-connect
// security posture, all PGC_SIGHUP. mint_roles ('' = minting disabled) is
// the master arm; max_per_role (0 = unlimited) caps live minted databases
// per role.
pub static pgrust_ephemeral_db_mint_roles: GucStringVar = GucSlot::new("pgrust_ephemeral_db_mint_roles");
pub static pgrust_ephemeral_db_max_per_role: GucIntVar = GucSlot::new("pgrust_ephemeral_db_max_per_role");
// pgrust-only (docs/design/test-views.md D3 warm-pool addendum, no C
// symbol): how many pre-minted spare clones PER POOLED TEMPLATE the
// janitor keeps warm (0 = off). PGC_SIGHUP, re-read every janitor tick.
pub static pgrust_ephemeral_db_pool_size: GucIntVar = GucSlot::new("pgrust_ephemeral_db_pool_size");
// pgrust-only (test-views.md mint-strategy addendum, no C symbol): swept-
// relation count at or above which a janitor mint picks CREATE DATABASE
// STRATEGY wal_log (zero checkpoints) over file_copy; -1 = never (always
// file_copy). PGC_SIGHUP, read at each strategy pick.
pub static pgrust_ephemeral_db_wal_log_threshold: GucIntVar = GucSlot::new("pgrust_ephemeral_db_wal_log_threshold");
// pgrust-only (test-views.md prewarm addendum, no C symbol): whether the
// janitor launches a one-shot prewarm worker against every database it
// mints (relcache init file + warm catalog pages), off the waiter critical
// path. PGC_SIGHUP, read at each enqueue/dispatch site.
pub static pgrust_ephemeral_db_prewarm: GucBoolVar = GucSlot::new("pgrust_ephemeral_db_prewarm");
// pgrust-only (env-to-guc train, no C symbol): the Gather read-fairness stride.
pub static pgrust_gather_fair_stride: GucIntVar = GucSlot::new("pgrust_gather_fair_stride");
// pgrust-only: pgrust.regex_re2_linked (preset, read-only) — whether the RE2
// engine was linked into this build. regexp_alt installs the accessor (a
// build-cfg constant); the table row's boot_val is never what SHOW reports.
pub static pgrust_regex_re2_linked: GucBoolVar = GucSlot::new("pgrust_regex_re2_linked");
pub static aex_log_min_duration: GucIntVar = GucSlot::new("auto_explain_log_min_duration");
pub static aex_log_parameter_max_length: GucIntVar =
    GucSlot::new("auto_explain_log_parameter_max_length");
pub static aex_log_analyze: GucBoolVar = GucSlot::new("auto_explain_log_analyze");
pub static aex_log_settings: GucBoolVar = GucSlot::new("auto_explain_log_settings");
pub static aex_log_verbose: GucBoolVar = GucSlot::new("auto_explain_log_verbose");
pub static aex_log_buffers: GucBoolVar = GucSlot::new("auto_explain_log_buffers");
pub static aex_log_wal: GucBoolVar = GucSlot::new("auto_explain_log_wal");
pub static aex_log_triggers: GucBoolVar = GucSlot::new("auto_explain_log_triggers");
pub static aex_log_timing: GucBoolVar = GucSlot::new("auto_explain_log_timing");
pub static aex_log_nested_statements: GucBoolVar =
    GucSlot::new("auto_explain_log_nested_statements");
pub static aex_log_format: GucEnumVar = GucSlot::new("auto_explain_log_format");
pub static aex_log_level: GucEnumVar = GucSlot::new("auto_explain_log_level");
pub static aex_sample_rate: GucRealVar = GucSlot::new("auto_explain_sample_rate");

pub static cron_database_name: GucStringVar = GucSlot::new("CronTableDatabaseName");
pub static cron_log_statement: GucBoolVar = GucSlot::new("CronLogStatement");
pub static cron_log_run: GucBoolVar = GucSlot::new("CronLogRun");
pub static cron_enable_superuser_jobs: GucBoolVar = GucSlot::new("EnableSuperuserJobs");
pub static cron_host: GucStringVar = GucSlot::new("CronHost");
pub static cron_use_background_workers: GucBoolVar = GucSlot::new("UseBackgroundWorkers");
pub static cron_launch_active_jobs: GucBoolVar = GucSlot::new("LaunchActiveJobs");
pub static cron_max_running_jobs: GucIntVar = GucSlot::new("MaxRunningTasks");
pub static cron_log_min_messages: GucEnumVar = GucSlot::new("CronLogMinMessages");
pub static cron_timezone: GucStringVar = GucSlot::new("cron_timezone");
pub static cron_match_dom_and_dow: GucBoolVar = GucSlot::new("CronDomDowAndLogic");
pub static autoprewarm_interval: GucIntVar = GucSlot::new("autoprewarm_interval");
// autoprewarm.c:119 static bool autoprewarm (:144 _PG_init
// DefineCustomBoolVariable, PGC_POSTMASTER, preload-only in C); the cell
// lives in crates/contrib/pg_prewarm.
pub static autoprewarm: GucBoolVar = GucSlot::new("autoprewarm");
// passwordcheck.c:152 _PG_init DefineCustomIntVariable; the cell lives in
// crates/contrib/passwordcheck.
pub static passwordcheck_min_password_length: GucIntVar = GucSlot::new("passwordcheck_min_password_length");
// isn.c:939 _PG_init DefineCustomBoolVariable over the isn.c g_weak static;
// the cell lives in crates/contrib/isn.
pub static isn_weak: GucBoolVar = GucSlot::new("g_weak");
pub static pg_trgm_similarity_threshold: GucRealVar = GucSlot::new("similarity_threshold");
pub static pg_trgm_word_similarity_threshold: GucRealVar =
    GucSlot::new("word_similarity_threshold");
pub static pg_trgm_strict_word_similarity_threshold: GucRealVar =
    GucSlot::new("strict_word_similarity_threshold");
pub static plpgsql_variable_conflict: GucEnumVar = GucSlot::new("plpgsql_variable_conflict");
pub static plpgsql_print_strict_params: GucBoolVar = GucSlot::new("plpgsql_print_strict_params");
pub static plpgsql_check_asserts: GucBoolVar = GucSlot::new("plpgsql_check_asserts");
pub static plpgsql_extra_warnings_string: GucStringVar =
    GucSlot::new("plpgsql_extra_warnings_string");
pub static plpgsql_extra_errors_string: GucStringVar = GucSlot::new("plpgsql_extra_errors_string");
// pgrust-only: pgrust.resource_counters (no C symbol; PGC_INTERNAL,
// value computed by hooks::show_resource_counters — the simharness F8
// resource-baseline hook channel; the fd crate owns backing + hook).
pub static pgrust_resource_counters: GucStringVar = GucSlot::new("pgrust_resource_counters");
pub static pgrust_allocator: GucStringVar = GucSlot::new("pgrust_allocator");
pub static pgss_max: GucIntVar = GucSlot::new("pgss_max");
pub static pgss_track: GucEnumVar = GucSlot::new("pgss_track");
pub static pgss_track_utility: GucBoolVar = GucSlot::new("pgss_track_utility");
pub static pgss_track_planning: GucBoolVar = GucSlot::new("pgss_track_planning");
pub static pgss_save: GucBoolVar = GucSlot::new("pgss_save");
pub static pgcrypto_builtin_crypto_enabled: GucEnumVar = GucSlot::new("builtin_crypto_enabled");
pub static hnsw_ef_search: GucIntVar = GucSlot::new("hnsw_ef_search");
pub static hnsw_iterative_scan: GucEnumVar = GucSlot::new("hnsw_iterative_scan");
pub static hnsw_max_scan_tuples: GucIntVar = GucSlot::new("hnsw_max_scan_tuples");
pub static hnsw_scan_mem_multiplier: GucRealVar = GucSlot::new("hnsw_scan_mem_multiplier");
pub static pg_krb_caseins_users: GucBoolVar = GucSlot::new("pg_krb_caseins_users");
pub static pg_krb_server_keyfile: GucStringVar = GucSlot::new("pg_krb_server_keyfile");
pub static pgstat_fetch_consistency: GucEnumVar = GucSlot::new("pgstat_fetch_consistency");
pub static pgstat_track_activities: GucBoolVar = GucSlot::new("pgstat_track_activities");
pub static pgstat_track_activity_query_size: GucIntVar = GucSlot::new("pgstat_track_activity_query_size");
pub static pgstat_track_counts: GucBoolVar = GucSlot::new("pgstat_track_counts");
pub static pgstat_track_functions: GucEnumVar = GucSlot::new("pgstat_track_functions");
pub static phony_random_seed: GucRealVar = GucSlot::new("phony_random_seed");
pub static plan_cache_mode: GucEnumVar = GucSlot::new("plan_cache_mode");
pub static quote_all_identifiers: GucBoolVar = GucSlot::new("quote_all_identifiers");
pub static random_page_cost: GucRealVar = GucSlot::new("random_page_cost");
pub static recoveryEndCommand: GucStringVar = GucSlot::new("recoveryEndCommand");
pub static recoveryRestoreCommand: GucStringVar = GucSlot::new("recoveryRestoreCommand");
pub static recoveryTargetAction: GucEnumVar = GucSlot::new("recoveryTargetAction");
pub static recoveryTargetInclusive: GucBoolVar = GucSlot::new("recoveryTargetInclusive");
pub static recovery_init_sync_method: GucEnumVar = GucSlot::new("recovery_init_sync_method");
pub static recovery_min_apply_delay: GucIntVar = GucSlot::new("recovery_min_apply_delay");
pub static recovery_prefetch: GucEnumVar = GucSlot::new("recovery_prefetch");
pub static recovery_target_lsn_string: GucStringVar = GucSlot::new("recovery_target_lsn_string");
pub static recovery_target_name_string: GucStringVar = GucSlot::new("recovery_target_name_string");
pub static recovery_target_string: GucStringVar = GucSlot::new("recovery_target_string");
pub static recovery_target_time_string: GucStringVar = GucSlot::new("recovery_target_time_string");
pub static recovery_target_timeline_string: GucStringVar = GucSlot::new("recovery_target_timeline_string");
pub static recovery_target_xid_string: GucStringVar = GucSlot::new("recovery_target_xid_string");
pub static recursive_worktable_factor: GucRealVar = GucSlot::new("recursive_worktable_factor");
pub static remove_temp_files_after_crash: GucBoolVar = GucSlot::new("remove_temp_files_after_crash");
pub static restart_after_crash: GucBoolVar = GucSlot::new("restart_after_crash");
pub static restrict_nonsystem_relation_kind_string: GucStringVar = GucSlot::new("restrict_nonsystem_relation_kind_string");
pub static role_string: GucStringVar = GucSlot::new("role_string");
pub static row_security: GucBoolVar = GucSlot::new("row_security");
pub static scram_sha_256_iterations: GucIntVar = GucSlot::new("scram_sha_256_iterations");
pub static segment_size: GucIntVar = GucSlot::new("segment_size");
pub static send_abort_for_crash: GucBoolVar = GucSlot::new("send_abort_for_crash");
pub static send_abort_for_kill: GucBoolVar = GucSlot::new("send_abort_for_kill");
pub static seq_page_cost: GucRealVar = GucSlot::new("seq_page_cost");
pub static serializable_buffers: GucIntVar = GucSlot::new("serializable_buffers");
pub static server_encoding_string: GucStringVar = GucSlot::new("server_encoding_string");
pub static server_version_num: GucIntVar = GucSlot::new("server_version_num");
pub static server_version_string: GucStringVar = GucSlot::new("server_version_string");
pub static session_authorization_string: GucStringVar = GucSlot::new("session_authorization_string");
pub static session_preload_libraries_string: GucStringVar = GucSlot::new("session_preload_libraries_string");
pub static shared_memory_size_in_huge_pages: GucIntVar = GucSlot::new("shared_memory_size_in_huge_pages");
pub static shared_memory_size_mb: GucIntVar = GucSlot::new("shared_memory_size_mb");
pub static shared_memory_type: GucEnumVar = GucSlot::new("shared_memory_type");
pub static shared_preload_libraries_string: GucStringVar = GucSlot::new("shared_preload_libraries_string");
pub static preload_contrib_string: GucStringVar = GucSlot::new("preload_contrib_string");
pub static ssl_ca_file: GucStringVar = GucSlot::new("ssl_ca_file");
pub static ssl_cert_file: GucStringVar = GucSlot::new("ssl_cert_file");
pub static ssl_crl_dir: GucStringVar = GucSlot::new("ssl_crl_dir");
pub static ssl_crl_file: GucStringVar = GucSlot::new("ssl_crl_file");
pub static ssl_dh_params_file: GucStringVar = GucSlot::new("ssl_dh_params_file");
pub static ssl_key_file: GucStringVar = GucSlot::new("ssl_key_file");
pub static ssl_library: GucStringVar = GucSlot::new("ssl_library");
pub static ssl_max_protocol_version: GucEnumVar = GucSlot::new("ssl_max_protocol_version");
pub static ssl_min_protocol_version: GucEnumVar = GucSlot::new("ssl_min_protocol_version");
pub static ssl_passphrase_command: GucStringVar = GucSlot::new("ssl_passphrase_command");
pub static ssl_passphrase_command_supports_reload: GucBoolVar = GucSlot::new("ssl_passphrase_command_supports_reload");
pub static ssl_renegotiation_limit: GucIntVar = GucSlot::new("ssl_renegotiation_limit");
pub static standard_conforming_strings: GucBoolVar = GucSlot::new("standard_conforming_strings");
pub static subtransaction_buffers: GucIntVar = GucSlot::new("subtransaction_buffers");
pub static summarize_wal: GucBoolVar = GucSlot::new("summarize_wal");
pub static sync_replication_slots: GucBoolVar = GucSlot::new("sync_replication_slots");
pub static synchronize_seqscans: GucBoolVar = GucSlot::new("synchronize_seqscans");
pub static synchronized_standby_slots: GucStringVar = GucSlot::new("synchronized_standby_slots");
pub static synchronous_commit: GucEnumVar = GucSlot::new("synchronous_commit");
pub static syslog_facility: GucEnumVar = GucSlot::new("syslog_facility");
pub static syslog_ident_str: GucStringVar = GucSlot::new("syslog_ident_str");
pub static syslog_sequence_numbers: GucBoolVar = GucSlot::new("syslog_sequence_numbers");
pub static syslog_split_messages: GucBoolVar = GucSlot::new("syslog_split_messages");
pub static tcp_keepalives_count: GucIntVar = GucSlot::new("tcp_keepalives_count");
pub static tcp_keepalives_idle: GucIntVar = GucSlot::new("tcp_keepalives_idle");
pub static tcp_keepalives_interval: GucIntVar = GucSlot::new("tcp_keepalives_interval");
pub static tcp_user_timeout: GucIntVar = GucSlot::new("tcp_user_timeout");
pub static temp_file_limit: GucIntVar = GucSlot::new("temp_file_limit");
pub static temp_tablespaces: GucStringVar = GucSlot::new("temp_tablespaces");
pub static timezone_abbreviations_string: GucStringVar = GucSlot::new("timezone_abbreviations_string");
pub static timezone_string: GucStringVar = GucSlot::new("timezone_string");
pub static trace_sort: GucBoolVar = GucSlot::new("trace_sort");
pub static track_commit_timestamp: GucBoolVar = GucSlot::new("track_commit_timestamp");
pub static track_cost_delay_timing: GucBoolVar = GucSlot::new("track_cost_delay_timing");
pub static track_io_timing: GucBoolVar = GucSlot::new("track_io_timing");
pub static track_wal_io_timing: GucBoolVar = GucSlot::new("track_wal_io_timing");
pub static transaction_buffers: GucIntVar = GucSlot::new("transaction_buffers");
pub static update_process_title: GucBoolVar = GucSlot::new("update_process_title");
pub static vacuum_failsafe_age: GucIntVar = GucSlot::new("vacuum_failsafe_age");
pub static vacuum_freeze_min_age: GucIntVar = GucSlot::new("vacuum_freeze_min_age");
pub static vacuum_freeze_table_age: GucIntVar = GucSlot::new("vacuum_freeze_table_age");
pub static vacuum_max_eager_freeze_failure_rate: GucRealVar = GucSlot::new("vacuum_max_eager_freeze_failure_rate");
pub static vacuum_multixact_failsafe_age: GucIntVar = GucSlot::new("vacuum_multixact_failsafe_age");
pub static vacuum_multixact_freeze_min_age: GucIntVar = GucSlot::new("vacuum_multixact_freeze_min_age");
pub static vacuum_multixact_freeze_table_age: GucIntVar = GucSlot::new("vacuum_multixact_freeze_table_age");
pub static vacuum_truncate: GucBoolVar = GucSlot::new("vacuum_truncate");
pub static wal_block_size: GucIntVar = GucSlot::new("wal_block_size");
pub static wal_compression: GucEnumVar = GucSlot::new("wal_compression");
pub static wal_consistency_checking_string: GucStringVar = GucSlot::new("wal_consistency_checking_string");
pub static wal_decode_buffer_size: GucIntVar = GucSlot::new("wal_decode_buffer_size");
pub static wal_init_zero: GucBoolVar = GucSlot::new("wal_init_zero");
pub static wal_keep_size_mb: GucIntVar = GucSlot::new("wal_keep_size_mb");
pub static wal_level: GucEnumVar = GucSlot::new("wal_level");
pub static wal_log_hints: GucBoolVar = GucSlot::new("wal_log_hints");
pub static wal_receiver_create_temp_slot: GucBoolVar = GucSlot::new("wal_receiver_create_temp_slot");
pub static wal_receiver_status_interval: GucIntVar = GucSlot::new("wal_receiver_status_interval");
pub static wal_receiver_timeout: GucIntVar = GucSlot::new("wal_receiver_timeout");
pub static wal_recycle: GucBoolVar = GucSlot::new("wal_recycle");
pub static wal_retrieve_retry_interval: GucIntVar = GucSlot::new("wal_retrieve_retry_interval");
pub static wal_segment_size: GucIntVar = GucSlot::new("wal_segment_size");
pub static wal_sender_timeout: GucIntVar = GucSlot::new("wal_sender_timeout");
pub static wal_skip_threshold: GucIntVar = GucSlot::new("wal_skip_threshold");
pub static wal_summary_keep_time: GucIntVar = GucSlot::new("wal_summary_keep_time");
pub static wal_sync_method: GucEnumVar = GucSlot::new("wal_sync_method");
pub static work_mem: GucIntVar = GucSlot::new("work_mem");
pub static xmlbinary: GucEnumVar = GucSlot::new("xmlbinary");
pub static xmloption: GucEnumVar = GucSlot::new("xmloption");
pub static zero_damaged_pages: GucBoolVar = GucSlot::new("zero_damaged_pages");

// pgrust-only (no C symbol): the instance aux process (crates/backend/postmaster/instaux).
pub static pgrust_instance_aux: GucBoolVar = GucSlot::new("pgrust_instance_aux");
pub static pgrust_instance_aux_ready_commit_age: GucIntVar = GucSlot::new("pgrust_instance_aux_ready_commit_age");
pub static pgrust_instance_aux_ready_lag: GucIntVar = GucSlot::new("pgrust_instance_aux_ready_lag");
pub static pgrust_instance_aux_startup_stall: GucIntVar = GucSlot::new("pgrust_instance_aux_startup_stall");
pub static pgrust_instance_aux_liveness_interval: GucIntVar = GucSlot::new("pgrust_instance_aux_liveness_interval");
pub static pgrust_instance_aux_wedge_timeout: GucIntVar = GucSlot::new("pgrust_instance_aux_wedge_timeout");
pub static pgrust_instance_aux_listen: GucStringVar = GucSlot::new("pgrust_instance_aux_listen");
pub static pgrust_instance_aux_socket: GucStringVar = GucSlot::new("pgrust_instance_aux_socket");
pub static pgrust_instance_aux_cert_file: GucStringVar = GucSlot::new("pgrust_instance_aux_cert_file");
pub static pgrust_instance_aux_key_file: GucStringVar = GucSlot::new("pgrust_instance_aux_key_file");
pub static pgrust_instance_aux_ca_file: GucStringVar = GucSlot::new("pgrust_instance_aux_ca_file");
pub static pgrust_instance_aux_admin_identities: GucStringVar = GucSlot::new("pgrust_instance_aux_admin_identities");
pub static pgrust_instance_aux_cluster: GucStringVar = GucSlot::new("pgrust_instance_aux_cluster");
pub static pgrust_instance_aux_instance: GucStringVar = GucSlot::new("pgrust_instance_aux_instance");
pub static pgrust_instance_aux_incarnation: GucStringVar = GucSlot::new("pgrust_instance_aux_incarnation");
pub static pgrust_instance_aux_managed_conf: GucStringVar = GucSlot::new("pgrust_instance_aux_managed_conf");
pub static pgrust_instance_aux_state_file: GucStringVar = GucSlot::new("pgrust_instance_aux_state_file");
