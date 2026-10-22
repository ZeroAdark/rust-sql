use guc_tables::*;
use types_guc::*;

pub(super) static SETTINGS: &[GucSetting] = &[
    GucSetting::Bool(GucBoolSetting { name: "jwt_validator.require_scopes", context: PGC_SIGHUP, group: CUSTOM_OPTIONS, short_desc: Some("Requires every HBA scope to be present in the token's scope claim (jwt_validator OAuth validator)."), long_desc: None, flags: 0, variable: &vars::jwt_validator_require_scopes, boot_val: GucDefaultValue::Bool(true), check_hook: None, assign_hook: None, show_hook: None }),
    GucSetting::Bool(GucBoolSetting { name: "jwt_validator.allow_insecure_http", context: PGC_SIGHUP, group: CUSTOM_OPTIONS, short_desc: Some("Allows plain-HTTP JWKS/introspection endpoints for the jwt_validator OAuth validator (testing only)."), long_desc: None, flags: 0, variable: &vars::jwt_validator_allow_insecure_http, boot_val: GucDefaultValue::Bool(false), check_hook: None, assign_hook: None, show_hook: None }),
    GucSetting::Int(GucIntSetting { name: "jwt_validator.clock_skew", context: PGC_SIGHUP, group: CUSTOM_OPTIONS, short_desc: Some("Clock skew tolerated when checking exp/nbf/iat claims (jwt_validator OAuth validator)."), long_desc: None, flags: GUC_UNIT_S, variable: &vars::jwt_validator_clock_skew, boot_val: GucDefaultValue::Int(60), min: 0, max: 3600, check_hook: None, assign_hook: None, show_hook: None }),
    GucSetting::Int(GucIntSetting { name: "jwt_validator.jwks_cache_ttl", context: PGC_SIGHUP, group: CUSTOM_OPTIONS, short_desc: Some("How long a fetched JWKS document is reused before re-fetching (jwt_validator OAuth validator)."), long_desc: None, flags: GUC_UNIT_S, variable: &vars::jwt_validator_jwks_cache_ttl, boot_val: GucDefaultValue::Int(300), min: 0, max: 86400, check_hook: None, assign_hook: None, show_hook: None }),
    GucSetting::Int(GucIntSetting { name: "jwt_validator.http_timeout", context: PGC_SIGHUP, group: CUSTOM_OPTIONS, short_desc: Some("Connect/read timeout for JWKS and introspection requests (jwt_validator OAuth validator)."), long_desc: None, flags: GUC_UNIT_S, variable: &vars::jwt_validator_http_timeout, boot_val: GucDefaultValue::Int(10), min: 1, max: 300, check_hook: None, assign_hook: None, show_hook: None }),
    GucSetting::String(GucStringSetting { name: "jwt_validator.jwks_uri", context: PGC_SIGHUP, group: CUSTOM_OPTIONS, short_desc: Some("JWKS endpoint for the jwt_validator OAuth validator (empty: discovered from the HBA issuer)."), long_desc: None, flags: 0, variable: &vars::jwt_validator_jwks_uri, boot_val: GucDefaultValue::String(Some("")), check_hook: None, assign_hook: None, show_hook: None }),
    GucSetting::String(GucStringSetting { name: "jwt_validator.audience", context: PGC_SIGHUP, group: CUSTOM_OPTIONS, short_desc: Some("Audience (aud claim) the jwt_validator OAuth validator requires in every token."), long_desc: None, flags: 0, variable: &vars::jwt_validator_audience, boot_val: GucDefaultValue::String(Some("")), check_hook: None, assign_hook: None, show_hook: None }),
    GucSetting::String(GucStringSetting { name: "jwt_validator.identity_claim", context: PGC_SIGHUP, group: CUSTOM_OPTIONS, short_desc: Some("Token claim the jwt_validator OAuth validator maps to the authenticated identity."), long_desc: None, flags: 0, variable: &vars::jwt_validator_identity_claim, boot_val: GucDefaultValue::String(Some("sub")), check_hook: None, assign_hook: None, show_hook: None }),
    GucSetting::String(GucStringSetting { name: "jwt_validator.introspection_uri", context: PGC_SIGHUP, group: CUSTOM_OPTIONS, short_desc: Some("RFC 7662 token introspection endpoint for opaque (non-JWT) bearer tokens (empty: opaque tokens are rejected)."), long_desc: None, flags: 0, variable: &vars::jwt_validator_introspection_uri, boot_val: GucDefaultValue::String(Some("")), check_hook: None, assign_hook: None, show_hook: None }),
    GucSetting::String(GucStringSetting { name: "jwt_validator.introspection_client_id", context: PGC_SIGHUP, group: CUSTOM_OPTIONS, short_desc: Some("Client ID the jwt_validator OAuth validator authenticates to the introspection endpoint with."), long_desc: None, flags: 0, variable: &vars::jwt_validator_introspection_client_id, boot_val: GucDefaultValue::String(Some("")), check_hook: None, assign_hook: None, show_hook: None }),
    GucSetting::String(GucStringSetting { name: "jwt_validator.introspection_client_secret", context: PGC_SIGHUP, group: CUSTOM_OPTIONS, short_desc: Some("Client secret the jwt_validator OAuth validator authenticates to the introspection endpoint with."), long_desc: None, flags: GUC_SUPERUSER_ONLY, variable: &vars::jwt_validator_introspection_client_secret, boot_val: GucDefaultValue::String(Some("")), check_hook: None, assign_hook: None, show_hook: None }),
    GucSetting::String(GucStringSetting { name: "jwt_validator.ca_file", context: PGC_SIGHUP, group: CUSTOM_OPTIONS, short_desc: Some("CA certificate bundle the jwt_validator OAuth validator verifies HTTPS peers against (empty: OpenSSL defaults)."), long_desc: None, flags: 0, variable: &vars::jwt_validator_ca_file, boot_val: GucDefaultValue::String(Some("")), check_hook: None, assign_hook: None, show_hook: None }),
];

#[cfg(test)]
#[test]
fn jwt_validator_settings_are_sighup_custom_options() {
    assert_eq!(SETTINGS.len(), 12);
    for setting in SETTINGS {
        assert!(setting.name().starts_with("jwt_validator."), "{}", setting.name());
        assert_eq!(setting.context(), PGC_SIGHUP, "{}", setting.name());
        assert_eq!(setting.group(), CUSTOM_OPTIONS, "{}", setting.name());
    }
}
