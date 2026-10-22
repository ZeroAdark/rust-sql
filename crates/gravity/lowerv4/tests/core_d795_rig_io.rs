//! D795 (main 09-26, M4-INSERT; found diagnosing oltpc-090): the rig catalog's `io` (getTypeInputInfo /
//! getTypeOutputInfo) always refused, so every CoerceViaIO (`$1::text` over an int / numeric parameter, a text cast of a
//! column) failed to compile in the crate with "Shape: an expression the catalog refused" while the server lowered and
//! served it: the crate census under-counted served statements and rig_gaps.d carried the difference. RigCatalog::io
//! answers from pg_type.dat as the server's catalog does (typinput / typoutput, typelem as the I/O param, varlena).
//! - oltpc-090 (C35 PAY customer UPDATE: `left($2::text || ' ' || $3::text || ... || c_data, 500)`) lowers.
use gravity_lowerv4_it as common;

#[path = "common/write_rig.rs"]
mod write_rig;

#[test]
fn d795_coerce_via_io_lowers_under_the_rig_catalog() {
    let p = common::plans_dir().join("oltpc-090.plan");
    let t = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    // as tests/m4.rs's census lowers it: the server's decompose member set (write_rig::declared)
    match write_rig::lower_declared(&t).expect("oltpc-090: decompose declined") {
        Ok(_) => {}
        Err(e) => panic!("oltpc-090 refuses under the rig catalog: {e}"),
    }
}
