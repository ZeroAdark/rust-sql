use super::*;
use types_guc::*;

fn find(name: &str) -> GucSetting {
    all_settings()
        .find(|setting| setting.name() == name)
        .unwrap_or_else(|| panic!("no built-in GUC named {name}"))
}

/// PostgreSQL's side of each table: PG 18.6's own GUCs (less its debug-build ones) and the extension GUCs this build
/// defines statically (pg_cron's cron.*, pgvector's, the oauth test module's). auto_explain, pg_stat_statements,
/// pg_prewarm, PL/pgSQL, pg_trgm and pgcrypto register theirs when their library loads, as PostgreSQL does, so they are
/// not in the tables. Only a change to that side edits these.
const PG_BOOL: usize = 121;
const PG_INT: usize = 150;
const PG_REAL: usize = 27;
const PG_STRING: usize = 79;
const PG_ENUM: usize = 43;

/// pgrust-only GUCs outside the `pgrust.` and `jwt_validator.` namespaces.
const PGRUST_UNPREFIXED: &[&str] = &[
    "regex_engine",
    "preload_contrib",
    "max_active_queries",
    "connection_queue_size",
    "connection_queue_timeout",
    "idle_passivate_timeout",
    "idle_passivate_stack",
    "catcache_size_limit",
    "relcache_size_limit",
    "shared_catalog_cache",
];

fn pgrust_only(name: &str) -> bool {
    name.starts_with("pgrust.") || name.starts_with("jwt_validator.") || PGRUST_UNPREFIXED.contains(&name)
}

fn postgres_side<'a>(names: impl Iterator<Item = &'a str>) -> usize {
    names.filter(|name| !pgrust_only(name)).count()
}

#[test]
fn table_counts_match_compiled_backend_shape() {
    let test_validator = usize::from(cfg!(feature = "oauth-test-validator"));
    let pg_side = [
        ("bool", postgres_side(ConfigureNamesBool.iter().map(|s| s.name)), PG_BOOL + test_validator),
        ("int", postgres_side(ConfigureNamesInt.iter().map(|s| s.name)), PG_INT),
        ("real", postgres_side(ConfigureNamesReal.iter().map(|s| s.name)), PG_REAL),
        ("string", postgres_side(ConfigureNamesString.iter().map(|s| s.name)), PG_STRING + test_validator),
        ("enum", postgres_side(ConfigureNamesEnum.iter().map(|s| s.name)), PG_ENUM),
    ];
    for (kind, got, want) in pg_side {
        assert_eq!(got, want, "{kind}: PostgreSQL's side changed (a pgrust-only GUC is named pgrust.*)");
    }
    for name in PGRUST_UNPREFIXED {
        assert!(all_settings().any(|s| s.name() == *name), "{name} is gone: drop it from PGRUST_UNPREFIXED");
    }
    assert_eq!(
        all_settings().count(),
        ConfigureNamesBool.len()
            + ConfigureNamesInt.len()
            + ConfigureNamesReal.len()
            + ConfigureNamesString.len()
            + ConfigureNamesEnum.len()
    );
    assert_eq!(GucContext_Names.len(), PGC_USERSET as usize + 1);
    assert_eq!(GucSource_Names.len(), PGC_S_SESSION as usize + 1);
    assert_eq!(config_group_names.len(), DEVELOPER_OPTIONS as usize + 1);
    assert_eq!(config_type_names.len(), PGC_ENUM as usize + 1);
}

#[test]
fn common_options_are_present_with_postgres_defaults() {
    let seqscan = find("enable_seqscan");
    assert_eq!(seqscan.value_kind(), GucValueKind::Bool);
    assert_eq!(seqscan.default_value(), GucDefaultValue::Bool(true));
    assert_eq!(seqscan.group(), QUERY_TUNING_METHOD);
    assert_eq!(seqscan.variable_c_symbol(), "enable_seqscan");

    let GucSetting::Int(stack) = find("max_stack_depth") else {
        panic!("max_stack_depth should be an int GUC");
    };
    assert_eq!(stack.boot_val, GucDefaultValue::Int(100));
    assert_eq!(
        stack.check_hook.unwrap().c_symbol(),
        "check_max_stack_depth"
    );
    assert_eq!(
        stack.assign_hook.unwrap().c_symbol(),
        "assign_max_stack_depth"
    );
    assert!(std::ptr::eq(
        stack.check_hook.unwrap(),
        &hooks::check_max_stack_depth
    ));

    let GucSetting::String(log_destination) = find("log_destination") else {
        panic!("log_destination should be a string GUC");
    };
    assert_eq!(
        log_destination.boot_val,
        GucDefaultValue::String(Some("stderr"))
    );
    assert_eq!(
        log_destination.check_hook.unwrap().c_symbol(),
        "check_log_destination"
    );

    let bytea_output = find("bytea_output");
    assert_eq!(bytea_output.value_kind(), GucValueKind::Enum);
    assert_eq!(
        bytea_output.default_value(),
        GucDefaultValue::Enum(consts::BYTEA_OUTPUT_HEX)
    );
    let opts = bytea_output.options().unwrap().entries();
    assert_eq!(opts[0].name, "escape");
    assert_eq!(opts[0].val, consts::BYTEA_OUTPUT_ESCAPE);

    assert_eq!(
        find("default_table_access_method").default_value(),
        GucDefaultValue::String(Some("heap"))
    );
    assert_eq!(
        find("server_version").default_value(),
        GucDefaultValue::String(Some("18.6"))
    );
}

#[test]
fn build_id_is_installed_once_and_named_in_the_identity() {
    // The only test in this binary that installs a build id.
    consts::set_pgrust_build_id(" 1a2b3c4\n");
    consts::set_pgrust_build_id("ignored");
    assert_eq!(consts::pgrust_build_suffix(), ", build 1a2b3c4");
    assert_eq!(consts::pgrust_identity(), "pgrust 0.4-beta, build 1a2b3c4");
}

#[test]
fn version_string_style_defaults_postgres_first() {
    // dl-verstring ruling 2026-08-06: the DEFAULT must be postgres_first so
    // first-number parsers of version() read the PostgreSQL compatibility
    // version. Flipping this default is a client-visible ecosystem break.
    let style = find("pgrust.version_string_style");
    assert_eq!(style.value_kind(), GucValueKind::Enum);
    assert_eq!(
        style.default_value(),
        GucDefaultValue::Enum(consts::VERSION_STRING_POSTGRES_FIRST)
    );
    assert_eq!(style.group(), CUSTOM_OPTIONS);
    let opts = style.options().unwrap().entries();
    assert_eq!(opts.len(), 2);
    assert_eq!(opts[0].name, "postgres_first");
    assert_eq!(opts[0].val, consts::VERSION_STRING_POSTGRES_FIRST);
    assert_eq!(opts[1].name, "pgrust_first");
    assert_eq!(opts[1].val, consts::VERSION_STRING_PGRUST_FIRST);
}

#[test]
fn extern_option_sets_are_typed_slots() {
    let GucSetting::Enum(wal_level) = find("wal_level") else {
        panic!("wal_level should be an enum GUC");
    };
    match wal_level.options {
        GucEnumOptions::External(slot) => {
            assert_eq!(slot.c_symbol(), "wal_level_options");
            assert!(std::ptr::eq(slot, &option_sets::wal_level_options));
        }
        GucEnumOptions::Inline(_) => panic!("wal_level_options is owned by another unit"),
    }
    assert!(matches!(
        find("backslash_quote").options().unwrap(),
        GucEnumOptions::Inline(_)
    ));
}

#[test]
fn message_level_options_match_elog_values() {
    let level = find("log_min_messages");
    let opts = level.options().unwrap().entries();
    let warning = opts.iter().find(|o| o.name == "warning").unwrap();
    assert_eq!(warning.val, types_error::WARNING.0);
    assert_eq!(
        level.default_value(),
        GucDefaultValue::Enum(types_error::WARNING.0)
    );
}

#[test]
fn installed_hook_dispatches_through_the_table_entry() {
    use std::sync::atomic::{AtomicI32, Ordering};

    static SEEN: AtomicI32 = AtomicI32::new(0);

    fn recording_check(
        newval: &mut i32,
        extra: &mut Option<GucHookExtra>,
        _source: GucSource,
    ) -> types_error::PgResult<bool> {
        SEEN.store(*newval, Ordering::SeqCst);
        *extra = Some(Box::new(*newval * 2));
        *newval += 1;
        Ok(true)
    }

    hooks::check_max_stack_depth.install(recording_check);

    let GucSetting::Int(stack) = find("max_stack_depth") else {
        panic!("max_stack_depth should be an int GUC");
    };
    let mut newval = 2048;
    let mut extra = None;
    let ok = stack.check_hook.unwrap().get()(&mut newval, &mut extra, PGC_S_TEST).unwrap();
    assert!(ok);
    assert_eq!(SEEN.load(Ordering::SeqCst), 2048);
    assert_eq!(newval, 2049);
    assert_eq!(*extra.unwrap().downcast::<i32>().unwrap(), 4096);
}

#[test]
fn installed_variable_accessors_read_and_write_the_owner_storage() {
    use std::cell::Cell;

    thread_local! {
        static STORAGE: Cell<bool> = const { Cell::new(true) };
    }

    vars::enable_seqscan.install(GucVarAccessors {
        get: || STORAGE.with(Cell::get),
        set: |v| STORAGE.with(|c| c.set(v)),
    });

    let GucSetting::Bool(seqscan) = find("enable_seqscan") else {
        panic!("enable_seqscan should be a bool GUC");
    };
    assert!(seqscan.variable.read());
    seqscan.variable.write(false);
    assert!(!seqscan.variable.read());
}

#[test]
fn default_toast_compression_is_installed_by_the_product_boot() {
    // Gate-blindness regression (main-red adjudication 2026-08-04):
    // heaptoast's toast_compress_datum falls back to this slot for an
    // invalid attcompression (heaptoast internals.rs — C
    // toast_internals.c:59), so it is read on EVERY default-compression
    // TOAST compress of a real server. Drift commit 0eb4c6c1fd1 landed
    // that reader with only heaptoast's own test-local install; every
    // TOAST insert on mains 882239cc94/dc5c2ba5ed died with the
    // slots.rs used-before-install panic (regress leg ERRORs + the
    // cbstore-lane-e2e backend panics). init_seams() — the product boot
    // path — must install it; reverting the install_if_absent in
    // install_guc_tables_owned_vars() must turn this test red
    // (mutation-witnessed 2026-08-05: revert reproduced the exact CI cluster
    // panic signature locally).
    crate::init_seams();
    assert!(
        vars::default_toast_compression.installed(),
        "product boot (init_seams) must install default_toast_compression — \
         a test-only install leaves every real-server TOAST compress panicking"
    );
    // Boot value is pglz (C toast_compression.c boot default) and the
    // accessors must round-trip through the session backing cell.
    assert_eq!(vars::default_toast_compression.read(), crate::consts::TOAST_PGLZ_COMPRESSION);
    vars::default_toast_compression.write(crate::consts::TOAST_LZ4_COMPRESSION);
    assert_eq!(vars::default_toast_compression.read(), crate::consts::TOAST_LZ4_COMPRESSION);
    vars::default_toast_compression.write(crate::consts::TOAST_PGLZ_COMPRESSION);
}

#[test]
#[should_panic(expected = "check_bonjour used before its owning unit installed it")]
fn uninstalled_hook_slot_panics_loudly() {
    let _ = hooks::check_bonjour.get();
}

#[test]
#[should_panic(expected = "enable_indexscan used before its owning unit installed it")]
fn uninstalled_variable_slot_panics_loudly() {
    let _ = vars::enable_indexscan.read();
}

#[test]
#[should_panic(expected = "installed twice")]
fn duplicate_install_panics() {
    fn show() -> String {
        String::new()
    }
    hooks::show_archive_command.install(show);
    hooks::show_archive_command.install(show);
}

#[test]
fn name_tables_round_trip_indices() {
    assert_eq!(GucContext_Names[PGC_INTERNAL as usize], "internal");
    assert_eq!(GucContext_Names[PGC_USERSET as usize], "user");
    assert_eq!(GucSource_Names[PGC_S_DEFAULT as usize], "default");
    assert_eq!(GucSource_Names[PGC_S_FILE as usize], "configuration file");
    assert_eq!(config_group_names[FILE_LOCATIONS as usize], "File Locations");
    assert_eq!(config_type_names[PGC_BOOL as usize], "bool");
    assert_eq!(config_type_names[PGC_ENUM as usize], "enum");
}

#[test]
fn setting_names_are_unique() {
    let mut names: Vec<&str> = all_settings().map(|s| s.name()).collect();
    names.sort_unstable();
    let before = names.len();
    names.dedup();
    assert_eq!(before, names.len());
}

#[test]
fn file_copy_method_options_match_platform_clone_support() {
    let opts = find("file_copy_method").options().unwrap().entries();
    let copy = opts.iter().find(|o| o.name == "copy").unwrap();
    assert_eq!(copy.val, consts::FILE_COPY_METHOD_COPY);
    assert!(!copy.hidden);
    #[cfg(not(pgrust_sim))]
    {
        let clone = opts.iter().find(|o| o.name == "clone").unwrap();
        assert_eq!(clone.val, consts::FILE_COPY_METHOD_CLONE);
        assert!(!clone.hidden);
    }
    #[cfg(pgrust_sim)]
    assert!(!opts.iter().any(|o| o.name == "clone"));
    assert_eq!(
        find("file_copy_method").default_value(),
        GucDefaultValue::Enum(consts::FILE_COPY_METHOD_COPY)
    );
}

#[test]
fn file_extend_method_values_match_storage_layer() {
    let opts = find("file_extend_method").options().unwrap().entries();
    let wz = opts.iter().find(|o| o.name == "write_zeros").unwrap();
    assert_eq!(wz.val, types_storage::FILE_EXTEND_METHOD_WRITE_ZEROS);
    #[cfg(target_os = "linux")]
    {
        let pf = opts.iter().find(|o| o.name == "posix_fallocate").unwrap();
        assert_eq!(pf.val, types_storage::FILE_EXTEND_METHOD_POSIX_FALLOCATE);
        assert_eq!(opts[0].name, "posix_fallocate");
    }
    #[cfg(not(target_os = "linux"))]
    assert!(!opts.iter().any(|o| o.name == "posix_fallocate"));
    assert_eq!(
        find("file_extend_method").default_value(),
        GucDefaultValue::Enum(types_storage::DEFAULT_FILE_EXTEND_METHOD)
    );
}

#[test]
fn recovery_init_sync_method_offers_syncfs_where_available() {
    let opts = find("recovery_init_sync_method").options().unwrap().entries();
    let syncfs = opts.iter().find(|o| o.name == "syncfs");
    #[cfg(target_os = "linux")]
    assert_eq!(syncfs.unwrap().val, types_storage::DATA_DIR_SYNC_METHOD_SYNCFS);
    #[cfg(not(target_os = "linux"))]
    assert!(syncfs.is_none());
}

#[test]
fn lz4_zstd_build_config_is_reflected_in_option_sets() {
    let opts = find("default_toast_compression").options().unwrap().entries();
    assert!(opts.iter().any(|o| o.name == "lz4" && o.val == consts::TOAST_LZ4_COMPRESSION));
    // guc_tables.c wal_compression_options with USE_LZ4 and USE_ZSTD.
    let wal = find("wal_compression").options().unwrap().entries();
    let visible: Vec<_> = wal.iter().filter(|o| !o.hidden).map(|o| (o.name, o.val)).collect();
    assert_eq!(
        visible,
        [
            ("pglz", consts::WAL_COMPRESSION_PGLZ),
            ("lz4", consts::WAL_COMPRESSION_LZ4),
            ("zstd", consts::WAL_COMPRESSION_ZSTD),
            ("on", consts::WAL_COMPRESSION_PGLZ),
            ("off", consts::WAL_COMPRESSION_NONE),
        ]
    );
    let GucSetting::Enum(style) = find("IntervalStyle") else {
        panic!("IntervalStyle should be an enum GUC");
    };
    assert!(style.assign_hook.is_none());
    let GucSetting::Int(stack) = find("max_stack_depth") else {
        panic!("max_stack_depth should be an int GUC");
    };
    assert_eq!(stack.boot_val, GucDefaultValue::Int(100));
}



// "pg_prewarm.autoprewarm" — autoprewarm.c:144-153 (_PG_init, preload arm):
// DefineCustomBoolVariable, default true, PGC_POSTMASTER, flags 0, no hooks,
// "Starts the autoprewarm worker." Regression for audit-18.6 w2-038
// (a186-candidate-fp-contrib-pg_prewarm-autoprewarm-95c06274c0f082d7efcc-1):
// pgrust knew no such GUC (42704 on SHOW under shared_preload_libraries).
#[test]
fn extension_settings_are_not_builtins() {
    for prefix in ["plpgsql.", "pg_stat_statements.", "pg_trgm.", "pgcrypto.", "auto_explain.", "pg_prewarm.", "isn.", "passwordcheck.", "jwt_validator."] {
        assert!(all_settings().all(|setting| !setting.name().starts_with(prefix)));
    }
}

// wio P180: the retired lane-v2 settings are not built-ins, so SET/SHOW treat
// them as C treats any unknown custom setting (placeholders).
#[test]
fn retired_lanev2_settings_are_not_builtins() {
    for name in [
        "pgrust.lane_executor",
        "pgrust.explain_runtime_verdicts",
        "pgrust.parallel_engine",
        "pgrust.runtime_dop",
        "pgrust.runtime_scan_pool",
        "pgrust.runtime_agg_pool",
        "pgrust.runtime_distinct_pool",
        "pgrust.runtime_hashjoin_pool",
        "pgrust.runtime_sort_pool",
        "pgrust.runtime_bitmap_pool",
        "pgrust.lane_parallel_pool",
    ] {
        assert!(all_settings().all(|setting| setting.name() != name), "{name}");
    }
}

#[test]
fn buffer_pool_resize_settings() {
    let GucSetting::Int(max) = find("pgrust.buffer_pool_max") else {
        panic!("pgrust.buffer_pool_max should be an int GUC");
    };
    assert_eq!(max.context, PGC_POSTMASTER);
    assert_eq!(max.boot_val, GucDefaultValue::Int(0));
    assert_eq!(max.flags & GUC_UNIT_BLOCKS, GUC_UNIT_BLOCKS);
    assert_eq!(max.group, CUSTOM_OPTIONS);
    let GucSetting::Int(rate) = find("pgrust.buffer_pool_drain_rate") else {
        panic!("pgrust.buffer_pool_drain_rate should be an int GUC");
    };
    assert_eq!(rate.context, PGC_SIGHUP);
    assert_eq!(rate.boot_val, GucDefaultValue::Int(50000));
    let thp = find("pgrust.buffer_pool_thp");
    assert_eq!(thp.default_value(), GucDefaultValue::Bool(false));
    assert_eq!(thp.context(), PGC_POSTMASTER);
    let relocate = find("pgrust.buffer_pool_relocate");
    assert_eq!(relocate.default_value(), GucDefaultValue::Bool(true));
    assert_eq!(relocate.context(), PGC_SIGHUP);
}

// wio P181: regex_engine is listed in pg_settings / SHOW ALL.
#[test]
fn regex_engine_is_shown_in_pg_settings() {
    assert_eq!(find("regex_engine").flags() & GUC_NO_SHOW_ALL, 0);
}

#[test]
fn writeback_flush_after_defaults_follow_sync_file_range() {
    let linux = cfg!(target_os = "linux");
    for (name, on_linux) in [("bgwriter_flush_after", 64), ("checkpoint_flush_after", 32), ("backend_flush_after", 0)] {
        let GucSetting::Int(g) = find(name) else { panic!("{name} should be an int GUC") };
        assert_eq!(g.boot_val, GucDefaultValue::Int(if linux { on_linux } else { 0 }), "{name}");
        assert_eq!(g.max, consts::WRITEBACK_MAX_PENDING_FLUSHES, "{name}");
    }
}

#[test]
fn condition_cache_modes_and_boolean_aliases() {
    use ConditionCacheMode::{Full, Granule, Off, Rows};
    let GucSetting::Enum(setting) = find("pgrust.condition_cache") else {
        panic!("pgrust.condition_cache should be an enum GUC");
    };
    assert_eq!(setting.boot_val, GucDefaultValue::Enum(Full as i32));
    assert_eq!(setting.context, PGC_USERSET);
    let GucEnumOptions::Inline(options) = setting.options else {
        panic!("condition cache modes should be inline");
    };
    assert_eq!(options.iter().filter(|o| !o.hidden).map(|o| o.name).collect::<Vec<_>>(), ["off", "granule", "rows", "full"]);
    assert!(!options.iter().any(|o| o.name == "o"));
    for (names, mode) in [
        (&["off", "false", "no", "0", "f", "fa", "fal", "fals", "n", "of"][..], Off),
        (&["granule"][..], Granule),
        (&["rows"][..], Rows),
        (&["full", "on", "true", "yes", "1", "t", "tr", "tru", "y", "ye"][..], Full),
    ] {
        for name in names {
            let option = options.iter().find(|o| o.name == *name).unwrap();
            assert_eq!(option.val, mode as i32, "{name}");
            backing::set_pgrust_condition_cache(option.val);
            assert_eq!(condition_cache_mode(), mode, "{name}");
        }
    }
    backing::set_pgrust_condition_cache(Full as i32);
}

#[test]
fn startup_prewarm_is_independent_of_condition_cache_mode() {
    let setting = find("pgrust.gravity_prewarm");
    assert_eq!(setting.default_value(), GucDefaultValue::Bool(true));
    assert_eq!(setting.context(), PGC_POSTMASTER);
    let saved = backing::pgrust_gravity_prewarm();
    for on in [false, true] {
        backing::set_pgrust_gravity_prewarm(on);
        for mode in [ConditionCacheMode::Off, ConditionCacheMode::Granule, ConditionCacheMode::Rows, ConditionCacheMode::Full] {
            backing::set_pgrust_condition_cache(mode as i32);
            assert_eq!(backing::pgrust_gravity_prewarm(), on);
        }
    }
    backing::set_pgrust_gravity_prewarm(saved);
    backing::set_pgrust_condition_cache(ConditionCacheMode::Full as i32);
}

#[test]
fn condition_cache_unlogged_boots_on_for_every_session() {
    // an UNLOGGED pgrcolumnar relation enters the condition cache by default; a session may still SET it off (a
    // temporary relation never enters: hosts GranuleRecipe::cond_unlogged)
    let g = find("pgrust.condition_cache_unlogged");
    assert_eq!(g.default_value(), GucDefaultValue::Bool(true));
    assert_eq!(g.context(), PGC_USERSET);
    assert!(backing::pgrust_condition_cache_unlogged());
}

#[test]
fn condition_cache_size_defaults_to_the_derived_budget_and_shows_the_budget_in_effect() {
    let GucSetting::Int(t) = find("pgrust.condition_cache_size") else {
        panic!("pgrust.condition_cache_size should be an int GUC");
    };
    assert!(matches!(t.boot_val, GucDefaultValue::Int(-1)));
    assert_eq!((t.min, t.flags & GUC_UNIT_KB), (-1, GUC_UNIT_KB));
    assert_eq!(backing::pgrust_condition_cache_size(), -1);
    // nothing installed the derivation: the default keeps nothing
    assert_eq!((pgrust_condition_cache_bytes(), show_pgrust_condition_cache_size().as_str()), (0, "0"));
    hooks::derive_pgrust_condition_cache_size.install_if_absent(|| 3 << 30);
    assert_eq!((pgrust_condition_cache_bytes(), show_pgrust_condition_cache_size().as_str()), (3 << 30, "3145728"));
    // a set value as set, a bare number of kB (D3791: pg_settings.setting carries no unit)
    for (kb, shown) in [(102400, "102400"), (1536, "1536"), (1 << 30, "1073741824"), (0, "0")] {
        backing::set_pgrust_condition_cache_size(kb);
        assert_eq!((pgrust_condition_cache_bytes(), show_pgrust_condition_cache_size().as_str()), (kb as u64 * 1024, shown));
    }
    backing::set_pgrust_condition_cache_size(-1);
    assert_eq!(pgrust_condition_cache_bytes(), 3 << 30);
}

#[test]
fn zone_store_and_dict_filter_cache_boot_on_and_are_independent_of_the_mode() {
    for name in ["pgrust.zone_store", "pgrust.dict_filter_cache"] {
        let setting = find(name);
        assert_eq!(setting.default_value(), GucDefaultValue::Bool(true), "{name}");
        assert_eq!(setting.context(), PGC_USERSET, "{name}");
    }
    backing::set_pgrust_zone_store(false);
    backing::set_pgrust_dict_filter_cache(false);
    for mode in [ConditionCacheMode::Off, ConditionCacheMode::Granule, ConditionCacheMode::Rows, ConditionCacheMode::Full] {
        backing::set_pgrust_condition_cache(mode as i32);
        assert!(!backing::pgrust_zone_store() && !backing::pgrust_dict_filter_cache());
    }
    backing::set_pgrust_zone_store(true);
    backing::set_pgrust_dict_filter_cache(true);
    backing::set_pgrust_condition_cache(ConditionCacheMode::Full as i32);
}
