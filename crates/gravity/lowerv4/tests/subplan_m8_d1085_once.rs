//! D1085 retired (W3c-D1085-RETIRE, M8-NEST): a VOLATILE FROM function in a nest's embedded inner reading none of the
//! owner's parameters is read from PG's FunctionScan store, filled once and rewound per re-open as ExecReScanFunctionScan
//! does, and that store declares D439's Volatile protocol (srf/function_store.rs store_step: D1657's once rule names it),
//! so a capped pull never fills it for an owner row PostgreSQL never pulls; tagged `VOLATILE_ONCE_STORE` (the entry
//! delegates a nested statement holding it: D2044). G4, does not apply: a STABLE embedded store, a store reading its
//! owner's parameter (V4R-518's own protocol, srf/store_read.rs, untagged) and a top-level store get none from it. Over
//! decompose's regress fixtures (PostgreSQL 18.6 plans).
use gravity_lowerv4_it as common;

use gravity_program::protos::{ProtoKind, VOLATILE_ONCE_STORE};

/// The Volatile protocols fixture `rel` (under decompose's plans) declares when lowered, and those tagged D1085's.
fn volatile_protos(rel: &str) -> (usize, usize) {
    let p = common::plans_dir().join(rel);
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    let l = common::lower_fixture(&text).expect("decomposes").unwrap_or_else(|r| panic!("{rel} refused: {r:?}"));
    let v = || l.descs.v4.protos.iter().filter(|p| p.kind == ProtoKind::Volatile);
    (v().count(), v().filter(|p| p.arg == VOLATILE_ONCE_STORE).count())
}

/// rangefuncs-031 (a VOLATILE SQL set function LEFT JOINed to VALUES) and insert-036 (psql's describe FK footer over
/// pg_partition_ancestors, VOLATILE): the embedded inner's store declares one Volatile protocol.
#[test]
fn a_volatile_embedded_store_reading_none_declares_the_volatile_protocol() {
    for rel in ["regress-full/rangefuncs-031.plan", "regress-full/insert-036.plan"] {
        assert_eq!(volatile_protos(rel), (1, 1), "{rel}");
    }
}

/// The classes the retirement newly serves carry the tag: rangefuncs-034 (LEFT JOIN ROWS FROM of two VOLATILE members
/// reading none, WITH ORDINALITY) and -040 / -041 (ROWS FROM with a VOLATILE member reading none beside one reading the
/// owner's parameter, either order). The store reading only a grand-owner's parameter has no plan fixture (the rangefuncs
/// LATERAL forms call generate_series, IMMUTABLE: rangefuncs-046 below); the e2e kit's nested probes show its tag.
#[test]
fn the_newly_served_classes_carry_the_tag() {
    for rel in ["regress-full/rangefuncs-034.plan", "regress-full/rangefuncs-040.plan", "regress-full/rangefuncs-041.plan"] {
        let (all, tagged) = volatile_protos(rel);
        assert!(tagged >= 1, "{rel}: {tagged} tagged of {all} Volatile protocols");
    }
}

/// sysviews-013 (pg_timezone_abbrevs_zone, STABLE, the inner of the view's anti join), groupingsets-030 (a store reading
/// its owner's parameter: V4R-518's one protocol, none added), lock-001 (pg_lock_status, VOLATILE, not an inner),
/// rangefuncs-046 (generate_series, IMMUTABLE, reading only the grand-owner's parameter).
#[test]
fn a_stable_an_owner_param_or_a_top_level_store_gets_none_from_it() {
    assert_eq!(volatile_protos("regress-full/sysviews-013.plan"), (0, 0), "a STABLE embedded store");
    assert_eq!(volatile_protos("regress-full/groupingsets-030.plan"), (1, 0), "a store reading its owner's parameter");
    assert_eq!(volatile_protos("regress-full/lock-001.plan"), (0, 0), "a top-level VOLATILE store");
    assert_eq!(volatile_protos("regress-full/rangefuncs-046.plan").1, 0, "an IMMUTABLE store reading the grand-owner's parameter");
}
