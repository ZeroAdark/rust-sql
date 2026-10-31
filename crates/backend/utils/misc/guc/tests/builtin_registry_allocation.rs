//! Installing known built-ins must not repeatedly reallocate a session's registry storage.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;

thread_local! {
    static TRACK: Cell<bool> = const { Cell::new(false) };
    static REALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

struct CountRealloc;

// SAFETY: every allocation operation is forwarded to System with the caller's original pointer and layout.
unsafe impl GlobalAlloc for CountRealloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if TRACK.get() {
            REALLOCATIONS.set(REALLOCATIONS.get() + 1);
        }
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountRealloc = CountRealloc;

#[test]
fn known_builtin_slots_do_not_reallocate_and_custom_settings_still_work() {
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
    guc::store::initialize_guc_options().unwrap();
    let (index, variables) = guc::store::with_store(|registry| {
        (Arc::new(registry.own_index()), (0..registry.len()).map(|i| registry[i].clone()).collect::<Vec<_>>())
    }).unwrap();
    assert!(index.len() > 300);
    let names = variables.iter().map(|v| v.name().to_string()).collect::<Vec<_>>();
    let count = variables.len();
    let mut registry = guc::registry::GucRegistry::new();
    REALLOCATIONS.set(0);
    TRACK.set(true);
    registry.adopt_builtin_index(index).unwrap();
    for variable in variables {
        registry.define_builtin(variable).unwrap();
    }
    let install_reallocations = REALLOCATIONS.get();
    registry.settle_builtin_index().unwrap();
    TRACK.set(false);
    assert_eq!(install_reallocations, 0, "known built-in registry arrays must not reallocate");
    assert_eq!(REALLOCATIONS.get(), 0, "known built-in registry storage must not reallocate");
    assert_eq!(registry.len(), count);
    for (i, name) in names.iter().enumerate() {
        assert_eq!(registry.find_index(name), Some(i));
        assert_eq!(registry.find_index(&name.to_ascii_uppercase()), Some(i));
    }
    let custom = registry.add_placeholder_variable("allocation_probe.private").unwrap();
    assert_eq!(custom, count);
    assert_eq!(registry.find_index("ALLOCATION_PROBE.PRIVATE"), Some(custom));
    registry.remove_all_placeholders();
    assert_eq!(registry.find_index("allocation_probe.private"), None);
    assert_eq!(registry.add_placeholder_variable("allocation_probe.private").unwrap(), custom);
}

#[test]
fn name_folding_preserves_utf8_without_growing_temporary_strings() {
    let cases = [
        ("", ""),
        ("Application_Name", "application_name"),
        ("extension.ÄΣ😀ABCé", "extension.ÄΣ😀abcé"),
        ("extension.THIS_IS_A_LONG_CUSTOM_CONFIGURATION_NAME", "extension.this_is_a_long_custom_configuration_name"),
    ];
    for (name, expected) in cases {
        REALLOCATIONS.set(0);
        TRACK.set(true);
        let folded = guc::name::fold_name(name);
        TRACK.set(false);
        assert_eq!(folded, expected);
        assert_eq!(folded.len(), name.len());
        assert_eq!(REALLOCATIONS.get(), 0, "ASCII case folding must not grow a temporary string");
    }
}
