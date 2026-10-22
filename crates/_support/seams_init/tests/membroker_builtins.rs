//! Memory step 6a: the memory broker's view functions resolve on their reserved oids and names once the
//! builtin tables are installed: no earlier extra table holds 9070..=9076 (the first table with an oid wins its lookup).

#[test]
fn the_memory_broker_view_functions_own_their_reserved_oids() {
    seams_init::register_builtin_tables();
    for b in gravity_exec::MEMBROKER_BUILTINS {
        let by_oid = fmgr_core::fmgr_isbuiltin(b.foid).map(|x| x.name);
        assert_eq!(by_oid, Some(b.name), "oid {} resolves to another builtin", b.foid);
        assert_eq!(fmgr_core::fmgr_internal_function(b.name), b.foid, "{}", b.name);
        assert!(b.retset && b.nargs == 0, "{}", b.name);
    }
}
