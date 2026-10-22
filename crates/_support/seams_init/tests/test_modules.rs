// C release builds have no src/test/modules libraries (LOAD: dfmgr.c:214).
#[test]
fn test_modules_follow_the_build_switch() {
    seams_init::init_all();
    let enabled = matches!(option_env!("PGRUST_TEST_MODULES"), Some("1"));
    assert!(dfmgr::is_builtin_library("pg_trgm"));
    for name in ["injection_points", "test_custom_types"] {
        assert_eq!(dfmgr::is_builtin_library(name), enabled, "{name}");
    }
}
