use guc_tables::*;
use guc_tables::consts::*;
use types_guc::*;

pub(super) static SETTINGS: &[GucSetting] = &[
    GucSetting::Int(GucIntSetting { name: "pg_stat_statements.max", context: PGC_POSTMASTER, group: CUSTOM_OPTIONS, short_desc: Some("Sets the maximum number of statements tracked by pg_stat_statements."), long_desc: None, flags: 0, variable: &vars::pgss_max, boot_val: GucDefaultValue::Int(5000), min: 100, max: i32::MAX / 2, check_hook: None, assign_hook: None, show_hook: None }),
    GucSetting::Enum(GucEnumSetting { name: "pg_stat_statements.track", context: PGC_SUSET, group: CUSTOM_OPTIONS, short_desc: Some("Selects which statements are tracked by pg_stat_statements."), long_desc: None, flags: 0, variable: &vars::pgss_track, boot_val: GucDefaultValue::Enum(PGSS_TRACK_TOP), options: GucEnumOptions::Inline(pgss_track_options), check_hook: None, assign_hook: None, show_hook: None }),
    GucSetting::Bool(GucBoolSetting { name: "pg_stat_statements.track_utility", context: PGC_SUSET, group: CUSTOM_OPTIONS, short_desc: Some("Selects whether utility commands are tracked by pg_stat_statements."), long_desc: None, flags: 0, variable: &vars::pgss_track_utility, boot_val: GucDefaultValue::Bool(true), check_hook: None, assign_hook: None, show_hook: None }),
    GucSetting::Bool(GucBoolSetting { name: "pg_stat_statements.track_planning", context: PGC_SUSET, group: CUSTOM_OPTIONS, short_desc: Some("Selects whether planning duration is tracked by pg_stat_statements."), long_desc: None, flags: 0, variable: &vars::pgss_track_planning, boot_val: GucDefaultValue::Bool(false), check_hook: None, assign_hook: None, show_hook: None }),
    GucSetting::Bool(GucBoolSetting { name: "pg_stat_statements.save", context: PGC_SIGHUP, group: CUSTOM_OPTIONS, short_desc: Some("Save pg_stat_statements statistics across server shutdowns."), long_desc: None, flags: 0, variable: &vars::pgss_save, boot_val: GucDefaultValue::Bool(true), check_hook: None, assign_hook: None, show_hook: None }),
];
