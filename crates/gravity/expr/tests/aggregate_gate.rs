//! privileges#562 (D2218): the graph admission's per-execution recheck of an Aggref's aggregate (`aggregate_gate`) names
//! the object as nodeAgg.c's ExecInitAgg does (OBJECT_AGGREGATE: "permission denied for aggregate X", SQLSTATE 42501);
//! ExecInitFunc's gate (`execute_gate`) still names a function. Its own test binary: the seams below are process-wide.
use gravity_expr::nodes::func::{aggregate_gate, execute_gate};
use types_error::ERRCODE_INSUFFICIENT_PRIVILEGE;

const DENIED: u32 = 90562;
const GRANTED: u32 = 90563;

#[test]
fn aggregate_gate_names_the_aggregate() {
    miscinit_seams::get_user_id::set(|| 10);
    aclchk_seams::object_aclcheck::set(|_classid, objid, _roleid, _mode| Ok(if objid == DENIED { 2 } else { 0 }));
    syscache_seams::pg_proc_proname::set(|_funcid| {
        let mut n = types_tuple::NameData::default();
        n.namestrcpy("p562_agg");
        Ok(Some(n))
    });
    let agg = aggregate_gate(DENIED).err().expect("denied");
    assert_eq!((agg.message(), agg.sqlstate()), ("permission denied for aggregate p562_agg", ERRCODE_INSUFFICIENT_PRIVILEGE));
    let func = execute_gate(DENIED).err().expect("denied");
    assert_eq!((func.message(), func.sqlstate()), ("permission denied for function p562_agg", ERRCODE_INSUFFICIENT_PRIVILEGE));
    assert!(aggregate_gate(GRANTED).is_ok());
    assert!(execute_gate(GRANTED).is_ok());
}
