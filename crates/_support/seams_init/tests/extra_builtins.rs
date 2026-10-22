
use std::collections::HashMap;

const RESERVED: std::ops::RangeInclusive<u32> = 9000..=9099;

#[test]
fn extra_builtin_oids_are_unique_and_clear_of_live_rows() {
    let mut seen: HashMap<u32, &str> = HashMap::new();
    let mut native = 0;
    for b in seams_init::EXTRA_BUILTINS.iter().flat_map(|t| t.iter()) {
        if let Some(prev) = seen.insert(b.foid, b.name) {
            panic!("oid {} is both {} and {}", b.foid, prev, b.name);
        }
        // A ported fmgr row with this oid would answer first (fmgr_isbuiltin reaches the extras only for a stub row or
        // no row), so the extra row would be dead. (Once the other test installs the tables, the answer is the row itself.)
        if let Some(row) = fmgr_core::fmgr_isbuiltin(b.foid) {
            assert!(
                fmgr_core::row_is_stub(row) || std::ptr::eq(row, b),
                "extra builtin {} ({}) is shadowed by the live row {}",
                b.foid,
                b.name,
                row.name
            );
        }
        if RESERVED.contains(&b.foid) {
            native += 1;
            assert!(
                fmgr_core::CANONICAL.iter().all(|c| c.0 != b.foid && c.1 != b.name),
                "pgrust-native {} ({}) collides with a canonical builtin",
                b.foid,
                b.name
            );
        }
    }
    // the janitor's three, the buffer pool's two, the pgrc planner facts and layout faces' five
    assert!(native >= 10, "the pgrust-native rows went missing: {native}");
}

/// The by-oid answer for each pgrust-native row is that row (the lookup the collision broke).
#[test]
fn each_extra_oid_resolves_to_its_own_row() {
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| fmgr_core::install_extra_builtins(&seams_init::EXTRA_BUILTINS));
    for b in seams_init::EXTRA_BUILTINS.iter().flat_map(|t| t.iter()).filter(|b| RESERVED.contains(&b.foid)) {
        let got = fmgr_core::fmgr_isbuiltin(b.foid).expect("an installed extra row");
        assert_eq!(got.name, b.name, "oid {} resolves to {}", b.foid, got.name);
        assert_eq!(fmgr_core::fmgr_internal_function(b.name), b.foid, "{} by name", b.name);
    }
}
