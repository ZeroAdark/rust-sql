//! D4682 step 2 (a), the GUC registry template: a registry on the process's shared index of the built-in variables'
//! names finds every name exactly where a registry with its own index does, and what a session defines past the
//! built-ins (a placeholder) never reaches the next registry. One test in its own binary: the shared index is process
//! state.

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
fn a_registry_on_the_shared_builtin_index_finds_every_name_where_its_own_would() {
    setup_seams();
    // the reference: sharing off, the registry indexes every name itself
    guc::store::set_share_builtin_index(false);
    guc::store::initialize_guc_options().unwrap();
    let reference: Vec<(String, usize)> =
        guc::store::with_store(|r| (0..r.len()).map(|i| (r[i].name().to_string(), i)).collect()).unwrap();
    assert!(reference.len() > 300, "the built-ins: {}", reference.len());
    let work_mem = reference.iter().find(|(n, _)| n == "work_mem").map(|(_, i)| *i).expect("work_mem is built in");

    guc::store::set_share_builtin_index(true);
    for round in 0..3 {
        guc::store::initialize_guc_options().unwrap();
        guc::store::with_store_mut(|r| {
            // round 0 publishes the index (its own holds every built-in), later rounds adopt it (their own is empty)
            assert_eq!(r.own_index().len(), if round == 0 { reference.len() } else { 0 }, "round {round}");
            assert_eq!(r.len(), reference.len(), "round {round}");
            for (name, i) in &reference {
                assert_eq!(r.find_index(name), Some(*i), "round {round}: {name}");
                assert_eq!(r.find_index(&name.to_ascii_uppercase()), Some(*i), "round {round}: {name} upper-cased");
            }
            assert_eq!(r.find_index("sort_mem"), Some(work_mem), "an old name maps through the shared index");
            assert_eq!(r.find_index("d4682.probe"), None, "round {round}: the previous session's placeholder is gone");
            // the session defines a placeholder past the built-ins: its own index holds it, the shared one never does
            let idx = r.add_placeholder_variable("d4682.probe").unwrap();
            assert_eq!(idx, reference.len());
            assert_eq!(r.find_index("D4682.PROBE"), Some(idx));
            r.remove_all_placeholders();
            assert_eq!(r.find_index("d4682.probe"), None);
            let again = r.add_placeholder_variable("d4682.probe").unwrap();
            assert_eq!(again, idx, "the tombstone is reused");
        })
        .unwrap();
    }
    guc::store::set_share_builtin_index(false);
    guc::store::initialize_guc_options().unwrap();
    guc::store::with_store(|r| assert_eq!(r.own_index().len(), reference.len(), "sharing off: its own index again")).unwrap();
}
