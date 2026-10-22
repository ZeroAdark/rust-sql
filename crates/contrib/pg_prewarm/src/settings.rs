use guc_tables::*;
use types_guc::*;

pub(super) static SETTINGS: &[GucSetting] = &[
    GucSetting::Int(GucIntSetting { name: "pg_prewarm.autoprewarm_interval", context: PGC_SIGHUP, group: CUSTOM_OPTIONS, short_desc: Some("Sets the interval between dumps of shared buffers"), long_desc: Some("If set to zero, time-based dumping is disabled."), flags: GUC_UNIT_S, variable: &vars::autoprewarm_interval, boot_val: GucDefaultValue::Int(300), min: 0, max: i32::MAX / 1000, check_hook: None, assign_hook: None, show_hook: None }),
    GucSetting::Bool(GucBoolSetting { name: "pg_prewarm.autoprewarm", context: PGC_POSTMASTER, group: CUSTOM_OPTIONS, short_desc: Some("Starts the autoprewarm worker."), long_desc: None, flags: 0, variable: &vars::autoprewarm, boot_val: GucDefaultValue::Bool(true), check_hook: None, assign_hook: None, show_hook: None }),
];

#[cfg(test)]
#[test]
fn pg_prewarm_autoprewarm_interval_matches_autoprewarm_c() {
    let GucSetting::Int(interval) = find("pg_prewarm.autoprewarm_interval") else {
        panic!("pg_prewarm.autoprewarm_interval should be an int GUC");
    };
    assert_eq!(interval.context, PGC_SIGHUP);
    assert_eq!(interval.group, CUSTOM_OPTIONS);
    assert_eq!(interval.flags, GUC_UNIT_S);
    assert_eq!(interval.boot_val, GucDefaultValue::Int(300));
    assert_eq!(interval.min, 0);
    assert_eq!(interval.max, i32::MAX / 1000);
    assert_eq!(
        interval.short_desc,
        Some("Sets the interval between dumps of shared buffers")
    );
    assert_eq!(
        interval.long_desc,
        Some("If set to zero, time-based dumping is disabled.")
    );
    assert!(interval.check_hook.is_none());
    assert!(interval.assign_hook.is_none());
    assert!(interval.show_hook.is_none());
    assert_eq!(interval.variable.c_symbol(), "autoprewarm_interval");
}
#[cfg(test)]
#[test]
fn pg_prewarm_autoprewarm_matches_autoprewarm_c() {
    let GucSetting::Bool(apw) = find("pg_prewarm.autoprewarm") else {
        panic!("pg_prewarm.autoprewarm should be a bool GUC");
    };
    assert_eq!(apw.context, PGC_POSTMASTER);
    assert_eq!(apw.group, CUSTOM_OPTIONS);
    assert_eq!(apw.flags, 0);
    assert_eq!(apw.boot_val, GucDefaultValue::Bool(true));
    assert_eq!(apw.short_desc, Some("Starts the autoprewarm worker."));
    assert!(apw.long_desc.is_none());
    assert!(apw.check_hook.is_none());
    assert!(apw.assign_hook.is_none());
    assert!(apw.show_hook.is_none());
    assert_eq!(apw.variable.c_symbol(), "autoprewarm");
}
#[cfg(test)]
fn find(name: &str) -> GucSetting {
    SETTINGS.iter().copied().find(|setting| setting.name() == name).unwrap()
}
