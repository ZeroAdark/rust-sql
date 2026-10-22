use guc_tables::*;
use types_guc::*;

pub(super) static SETTINGS: &[GucSetting] = &[
    GucSetting::Real(GucRealSetting { name: "pg_trgm.similarity_threshold", context: PGC_USERSET, group: CUSTOM_OPTIONS, short_desc: Some("Sets the threshold used by the % operator."), long_desc: Some("Valid range is 0.0 .. 1.0."), flags: 0, variable: &vars::pg_trgm_similarity_threshold, boot_val: GucDefaultValue::Real(0.3f32 as f64), min: 0.0f64, max: 1.0f64, check_hook: None, assign_hook: None, show_hook: None }),
    GucSetting::Real(GucRealSetting { name: "pg_trgm.word_similarity_threshold", context: PGC_USERSET, group: CUSTOM_OPTIONS, short_desc: Some("Sets the threshold used by the <% operator."), long_desc: Some("Valid range is 0.0 .. 1.0."), flags: 0, variable: &vars::pg_trgm_word_similarity_threshold, boot_val: GucDefaultValue::Real(0.6f32 as f64), min: 0.0f64, max: 1.0f64, check_hook: None, assign_hook: None, show_hook: None }),
    GucSetting::Real(GucRealSetting { name: "pg_trgm.strict_word_similarity_threshold", context: PGC_USERSET, group: CUSTOM_OPTIONS, short_desc: Some("Sets the threshold used by the <<% operator."), long_desc: Some("Valid range is 0.0 .. 1.0."), flags: 0, variable: &vars::pg_trgm_strict_word_similarity_threshold, boot_val: GucDefaultValue::Real(0.5f32 as f64), min: 0.0f64, max: 1.0f64, check_hook: None, assign_hook: None, show_hook: None }),
];

#[cfg(test)]
#[test]
fn pg_trgm_thresholds_match_trgm_op_c() {
    for (name, boot, desc, symbol) in [
        (
            "pg_trgm.similarity_threshold",
            0.3f32 as f64,
            "Sets the threshold used by the % operator.",
            "similarity_threshold",
        ),
        (
            "pg_trgm.word_similarity_threshold",
            0.6f32 as f64,
            "Sets the threshold used by the <% operator.",
            "word_similarity_threshold",
        ),
        (
            "pg_trgm.strict_word_similarity_threshold",
            0.5f32 as f64,
            "Sets the threshold used by the <<% operator.",
            "strict_word_similarity_threshold",
        ),
    ] {
        let GucSetting::Real(t) = find(name) else {
            panic!("{name} should be a real GUC");
        };
        assert_eq!(t.context, PGC_USERSET, "{name}");
        assert_eq!(t.group, CUSTOM_OPTIONS, "{name}");
        assert_eq!(t.flags, 0, "{name}");
        assert_eq!(t.boot_val, GucDefaultValue::Real(boot), "{name}");
        assert_eq!(t.min, 0.0, "{name}");
        assert_eq!(t.max, 1.0, "{name}");
        assert_eq!(t.short_desc, Some(desc), "{name}");
        assert_eq!(t.long_desc, Some("Valid range is 0.0 .. 1.0."), "{name}");
        assert!(t.check_hook.is_none(), "{name}");
        assert!(t.assign_hook.is_none(), "{name}");
        assert!(t.show_hook.is_none(), "{name}");
        assert_eq!(t.variable.c_symbol(), symbol, "{name}");
    }
}
#[cfg(test)]
fn find(name: &str) -> GucSetting {
    SETTINGS.iter().copied().find(|setting| setting.name() == name).unwrap()
}
