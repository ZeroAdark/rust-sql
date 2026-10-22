use guc_tables::*;
use guc_tables::consts::*;
use types_guc::*;

pub(super) static SETTINGS: &[GucSetting] = &[
    GucSetting::Enum(GucEnumSetting { name: "pgcrypto.builtin_crypto_enabled", context: PGC_SUSET, group: CUSTOM_OPTIONS, short_desc: Some("Sets if builtin crypto functions are enabled."), long_desc: Some("\"on\" enables builtin crypto, \"off\" unconditionally disables and \"fips\" will disable builtin crypto if OpenSSL is in FIPS mode"), flags: 0, variable: &vars::pgcrypto_builtin_crypto_enabled, boot_val: GucDefaultValue::Enum(BC_ON), options: GucEnumOptions::Inline(pgcrypto_builtin_crypto_options), check_hook: None, assign_hook: None, show_hook: None }),
];
