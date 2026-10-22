//! D1191 (M8-SUB, filed 13:27Z; main's plan accepted): the crate rig's RigCatalog::install_seams installed
//! pg_type_element_shape, lookup_pg_aggregate_shape and rig_fmgr only, never syscache_seams::lookup_pg_type_shape /
//! lookup_pg_type_typcache_shape (gravity_lower's rig installs both). A fixture whose lowering asks either seam refused
//! `Bug: seam not installed` in the crate tests. So it left the census tests' judgement (rig_gaps.d entries named the
//! seam) and the D1114 ratchet's coverage (86 caught panics at t51) while the server lowered it. The rig now answers
//! both from pg_type.dat as the server's catalog does.
use gravity_lowerv4_it as common;

#[path = "common/write_rig.rs"]
mod write_rig;

/// Census plan fixtures whose crate lowering stopped at `Bug: seam not installed` (lookup_pg_type_shape) and lower under
/// the rig catalog now, lowered as the census tests lower them (the server's decompose member set).
const SERVED: &[&str] = &["regress-full/aggregates-008.plan", "regress-full/join-014.plan", "regress-full/updatable_views-018.plan"];

/// Fixtures that asked lookup_pg_type_typcache_shape: they reach a type the test creates (no pg_type.dat row), so they
/// refuse by the catalog's name, never by a Bug.
const NAMED: &[&str] = &["regress-full/indirect_toast-002.plan", "regress-full/domain-014.plan"];

#[test]
fn d1191_the_rig_answers_the_type_shape_and_typcache_seams() {
    let lower = |rel: &str| {
        let p = common::plans_dir().join(rel);
        let t = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
        write_rig::lower_declared(&t).unwrap_or_else(|| panic!("{rel}: decompose declined"))
    };
    for rel in SERVED {
        if let Err(e) = lower(rel) {
            panic!("{rel} refuses under the rig catalog: {e}");
        }
    }
    for rel in NAMED {
        match lower(rel) {
            Err(e) if e.what.starts_with("Bug: ") => panic!("{rel}: a Bug under the rig catalog: {e}"),
            _ => {}
        }
    }
}
