//! D537: gravity_expr binds a hosted call's FmgrInfo at lowering (nodes/func.rs `bind` -> gravity_fn `FmgrCall::bind` ->
//! `fmgr_info`); a function outside fmgr's builtin table (a LANGUAGE sql one: obj_description, 1215) reads pg_proc through
//! `syscache_seams::lookup_pg_proc_fmgr`. The server installs it (seams_init); the crate rig did not, so every census fixture
//! with such a call lowered to `Bug: seam not installed` (domain-020 at M1; largeobject-004 and psql-018 in the M3 dry run).
//! The rig now answers it from pg_proc.dat: no fixture is a Bug, and one whose needs are built serves.
use gravity_lowerv4_it as common;

fn lowered(id: &str) -> Result<(), String> {
    let text = std::fs::read_to_string(common::plans_dir().join(format!("regress-full/{id}.plan"))).expect("plan fixture");
    common::lower_fixture(&text).expect("decompose takes it").map(|_| ()).map_err(|e| e.to_string())
}

#[test]
fn a_sql_language_call_binds_in_the_rig() {
    // select conname, obj_description(oid, 'pg_constraint') from pg_constraint where ... : every need is M1's
    assert_eq!(lowered("domain-020"), Ok(()), "domain-020 serves");
    for id in ["largeobject-004", "psql-018"] {
        if let Err(e) = lowered(id) {
            assert!(!e.contains("Bug"), "{id}: {e}");
        }
    }
}
