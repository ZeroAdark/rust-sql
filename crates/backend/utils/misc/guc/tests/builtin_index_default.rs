//! Default name sharing must not retain a previous session's values or custom names.
//! Its own test binary keeps the process-wide published index isolated from flag controls.

fn setup_seams() {
    guc_tables::init_seams();
    elog::init_seams();
    guc::init_seams();
    xact_seams::is_in_parallel_mode::set(|| false);
    scalar_seams::parse_bool::set(|value| match value {
        "true" | "on" | "yes" | "1" => Some(true),
        "false" | "off" | "no" | "0" => Some(false),
        _ => None,
    });
    aclchk_seams::pg_parameter_aclcheck_set::set(|_, _| Ok(true));
    mbutils_seams::get_database_encoding::set(|| 6);
    timestamp_seams::get_current_timestamp::set(|| 0);
}

#[test]
fn fresh_registries_share_names_by_default_but_keep_values_and_custom_names_private() {
    setup_seams();
    guc::store::initialize_guc_options().unwrap();
    let names: Vec<(String, usize)> = guc::store::with_store(|r| {
        (0..r.len()).map(|i| (r[i].name().to_string(), i)).collect()
    }).unwrap();
    let boot_work_mem = guc::store::get_int("work_mem").unwrap();
    assert!(names.len() > 300);
    eprintln!("built-in names: {}; variable bytes: {}", names.len(), std::mem::size_of::<guc::registry::GucVariable>());

    for session in 0..3 {
        let names = names.clone();
        std::thread::spawn(move || {
            miscinit::SetUserIdAndSecContext(10, 0);
            guc::store::initialize_guc_options().unwrap();
            guc::store::with_store_mut(|r| {
                assert_eq!(r.own_index().len(), 0, "built-in names should not be copied per registry");
                for (name, index) in &names {
                    assert_eq!(r.find_index(name), Some(*index));
                    assert_eq!(r.find_index(&name.to_ascii_uppercase()), Some(*index));
                }
                assert_eq!(r.find_index("builtin_default.private"), None);
                r.add_placeholder_variable("builtin_default.private").unwrap();
            }).unwrap();
            assert_eq!(guc::store::get_int("work_mem"), Some(boot_work_mem));
            let changed = (boot_work_mem + session + 1).to_string();
            guc::SetConfigOption("work_mem", Some(&changed),
                types_guc::GucContext::PGC_USERSET, types_guc::GucSource::PGC_S_SESSION).unwrap();
            assert_eq!(guc::store::get_int("work_mem"), Some(boot_work_mem + session + 1));
        }).join().unwrap();
    }
    guc::store::with_store(|r| assert_eq!(r.find_index("builtin_default.private"), None)).unwrap();
    assert_eq!(guc::store::get_int("work_mem"), Some(boot_work_mem));
}
