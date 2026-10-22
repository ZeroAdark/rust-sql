use guc_tables::*;
use types_guc::*;

pub(super) static SETTINGS: &[GucSetting] = &[
    GucSetting::Int(GucIntSetting { name: "passwordcheck.min_password_length", context: PGC_SUSET, group: CUSTOM_OPTIONS, short_desc: Some("Minimum allowed password length."), long_desc: None, flags: GUC_UNIT_BYTE, variable: &vars::passwordcheck_min_password_length, boot_val: GucDefaultValue::Int(8), min: 0, max: i32::MAX, check_hook: None, assign_hook: None, show_hook: None }),
];

#[cfg(test)]
#[test]
fn passwordcheck_min_password_length_matches_passwordcheck_c() {
    let GucSetting::Int(len) = SETTINGS[0] else {
        panic!("passwordcheck.min_password_length should be an int GUC");
    };
    assert_eq!(len.name, "passwordcheck.min_password_length");
    assert_eq!(len.context, PGC_SUSET);
    assert_eq!(len.group, CUSTOM_OPTIONS);
    assert_eq!(len.flags, GUC_UNIT_BYTE);
    assert_eq!(len.boot_val, GucDefaultValue::Int(8));
    assert_eq!(len.min, 0);
    assert_eq!(len.max, i32::MAX);
}
