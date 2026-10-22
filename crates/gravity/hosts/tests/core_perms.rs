use std::cell::RefCell;
use std::sync::Once;

use gravity_hosts::perms::check_one_rel;
use mcx::MemoryContext;
use types_nodes::parsenodes::{RTEPermissionInfo, ACL_DELETE, ACL_SELECT, ACL_UPDATE};
use types_nodes::Bitmapset;

thread_local! {
    static CALLS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    static CLASS_MASK: RefCell<u64> = const { RefCell::new(0) };
    static COL_OK: RefCell<Vec<i16>> = const { RefCell::new(Vec::new()) };
}

fn install() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        miscinit_seams::get_user_id::set(|| 10);
        aclchk_seams::pg_class_aclmask::set(|relid, userid, mask, _how| {
            CALLS.with(|c| c.borrow_mut().push(format!("class({relid}, {userid}, {mask})")));
            Ok(CLASS_MASK.with(|m| *m.borrow()) & mask)
        });
        aclchk_seams::pg_attribute_aclcheck::set(|relid, attno, _userid, mode| {
            CALLS.with(|c| c.borrow_mut().push(format!("attr({relid}, {attno}, {mode})")));
            Ok(if COL_OK.with(|v| v.borrow().contains(&attno)) { 0 } else { 1 })
        });
        aclchk_seams::pg_attribute_aclcheck_all::set(|relid, _userid, mode, all| {
            CALLS.with(|c| c.borrow_mut().push(format!("all({relid}, {mode}, {all})")));
            Ok(if COL_OK.with(|v| !v.borrow().is_empty()) { 0 } else { 1 })
        });
    });
}

fn bits<'m>(mcx: mcx::Mcx<'m>, members: &[i32]) -> Bitmapset<'m> {
    let mut b = Bitmapset::make_singleton(mcx, 0).unwrap();
    for &m in members {
        b.add_member(mcx, m).unwrap();
    }
    if members.is_empty() { Bitmapset::default() } else { b.del_member(0); b }
}

fn calls() -> Vec<String> {
    CALLS.with(|c| c.borrow_mut().drain(..).collect())
}

#[test]
fn table_grant_then_column_grants_then_refusal() {
    install();
    let ctx = MemoryContext::new("t");
    let mcx = ctx.mcx();
    let mut pi = RTEPermissionInfo { relid: 5000, requiredPerms: ACL_SELECT | ACL_UPDATE, checkAsUser: 0, ..Default::default() };
    pi.selectedCols = bits(mcx, &[7 + 1, 7 + 3]);
    pi.updatedCols = bits(mcx, &[7 + 3]);
    CLASS_MASK.with(|m| *m.borrow_mut() = ACL_SELECT | ACL_UPDATE);
    assert!(check_one_rel(&pi).unwrap());
    assert_eq!(calls(), ["class(5000, 10, 6)"]);
    CLASS_MASK.with(|m| *m.borrow_mut() = 0);
    COL_OK.with(|v| *v.borrow_mut() = vec![1, 3]);
    assert!(check_one_rel(&pi).unwrap(), "column grants cover the selected and updated columns");
    assert_eq!(calls(), ["class(5000, 10, 6)", "attr(5000, 1, 2)", "attr(5000, 3, 2)", "attr(5000, 3, 4)"]);
    COL_OK.with(|v| *v.borrow_mut() = vec![1]);
    assert!(!check_one_rel(&pi).unwrap(), "a missing column grant refuses");
    calls();
    pi.requiredPerms = ACL_DELETE;
    assert!(!check_one_rel(&pi).unwrap(), "DELETE cannot be satisfied at column level");
    assert_eq!(calls(), ["class(5000, 10, 8)"]);
    pi.checkAsUser = 42;
    pi.requiredPerms = ACL_SELECT;
    pi.selectedCols = bits(mcx, &[]);
    assert!(check_one_rel(&pi).unwrap());
    assert_eq!(calls(), ["class(5000, 42, 2)", "all(5000, 2, false)"]);
}
