use guc_tables::*;
use types_guc::*;

pub(super) static SETTINGS: &[GucSetting] = &[
    GucSetting::Bool(GucBoolSetting { name: "isn.weak", context: PGC_USERSET, group: CUSTOM_OPTIONS, short_desc: Some("Accept input with invalid ISN check digits."), long_desc: None, flags: 0, variable: &vars::isn_weak, boot_val: GucDefaultValue::Bool(false), check_hook: None, assign_hook: None, show_hook: None }),
];

#[cfg(test)]
#[test]
fn isn_weak_matches_isn_c() {
    let GucSetting::Bool(weak) = SETTINGS[0] else {
        panic!("isn.weak should be a bool GUC");
    };
    assert_eq!(weak.name, "isn.weak");
    assert_eq!(weak.context, PGC_USERSET);
    assert_eq!(weak.group, CUSTOM_OPTIONS);
    assert_eq!(weak.flags, 0);
    assert_eq!(weak.boot_val, GucDefaultValue::Bool(false));
    assert_eq!(weak.short_desc, Some("Accept input with invalid ISN check digits."));
    assert!(weak.long_desc.is_none());
    assert!(weak.check_hook.is_none());
    assert!(weak.assign_hook.is_none());
    assert!(weak.show_hook.is_none());
    assert_eq!(weak.variable.c_symbol(), "g_weak");
}
