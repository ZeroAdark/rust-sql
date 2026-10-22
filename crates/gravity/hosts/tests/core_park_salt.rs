//! H16 (b) B1: `SiteRecipe::park_salt`, the recipe identity the simple protocol's park key folds in. Equal recipes salt
//! equal; a recipe that differs in anything it holds of its statement (relation, lock mode, walk direction, index-only,
//! out columns) salts differently; a recipe with a filter has no salt (never parked across statements).
use gravity_hosts_rig as common;

use std::rc::Rc;

use common::*;
use gravity_hosts::SiteRecipe;
use mcx::MemoryContext;
use types_rel::Relation;
use types_storage::lock::{AccessShareLock, RowShareLock};

fn recipe(heap_oid: Option<u32>, lockmode: types_storage::lock::LOCKMODE, attnos: &[i16]) -> SiteRecipe {
    let ctx = MemoryContext::new("park salt");
    let mcx = ctx.mcx();
    let mut hd = heap_relation_data(mcx);
    if let Some(o) = heap_oid {
        hd.rd_id = o;
    }
    let heap = Relation::open(hd, None);
    let index = Relation::open(index_relation_data(mcx, true), None);
    let quals = indexqual_param(mcx, INDEX_VAR, OP_INT4EQ, F_INT4EQ);
    SiteRecipe::from_plan(&heap, &index, &quals, &types_nodes::NodeList::nil(), lockmode, attnos).unwrap()
}

#[test]
fn equal_recipes_salt_equal_and_any_held_difference_salts_differently() {
    let _g = serial();
    install();
    reset_fixture();
    let base = recipe(None, AccessShareLock, &[1]).park_salt().expect("an unfiltered recipe has a salt");
    assert_eq!(recipe(None, AccessShareLock, &[1]).park_salt(), Some(base), "the same recipe, built again");
    let others = [
        ("another heap", recipe(Some(HEAP_OID + 7), AccessShareLock, &[1]).park_salt()),
        ("another lock mode", recipe(None, RowShareLock, &[1]).park_salt()),
        ("another out column list", recipe(None, AccessShareLock, &[1, 1]).park_salt()),
        ("a backward walk", recipe(None, AccessShareLock, &[1]).with_backward(true).park_salt()),
        ("an index-only walk", recipe(None, AccessShareLock, &[1]).with_only(true).park_salt()),
    ];
    for (what, s) in others {
        let s = s.expect("an unfiltered recipe has a salt");
        assert_ne!(s, base, "{what} salts like the base recipe");
    }
}

#[test]
fn a_recipe_with_a_filter_is_never_salted() {
    let _g = serial();
    install();
    reset_fixture();
    let pass: gravity_hosts::FilterFn = Rc::new(|_| Ok(true));
    let filtered = recipe(None, AccessShareLock, &[1]).with_filter(Some(pass));
    assert_eq!(filtered.park_salt(), None);
}

/// D410 (M2-HEAP): a bitmap entry is no index recipe (`recipe` answers none for it). Its recheck qual is compiled in the
/// admitting execution's context, so its park recipe is its own (`bitmap_site`, exec `park_salt` marks it): the source
/// drops the qual at the unbind and takes each execution's at the bind (`Resolved::recheck`). An index entry beside it
/// answers `recipe` and not `bitmap_site`.
#[test]
fn a_bitmap_entry_answers_no_park_recipe() {
    let _g = serial();
    install();
    reset_fixture();
    let mut reg = gravity_hosts::HostRegistry::new();
    reg.push_bitmap(recipe(None, AccessShareLock, &[1]), None);
    reg.push(recipe(None, AccessShareLock, &[1]));
    assert!(reg.recipe(0).is_none(), "a bitmap entry offers a park recipe: its per-execution recheck would outlive its context");
    assert!(reg.recipe(1).is_some(), "an index entry answers its recipe");
    assert!(reg.bitmap_site(0).is_some_and(|r| r.park_salt() == reg.recipe(1).and_then(|r| r.park_salt())), "a bitmap entry answers its own park recipe");
    assert!(reg.bitmap_site(1).is_none(), "an index entry is no bitmap site");
    assert!(reg.bitmap_recheck(0).is_none(), "a bitmap entry made without a recheck files none");
}

/// A bitmap entry's recheck qual reaches the statement's `Resolved` (`cells_from`) for every bind (the leader's and a
/// worker slot's, both on the leader's thread), and a registry without one files nothing.
#[test]
fn a_bitmap_recheck_is_filed_for_each_bind() {
    let _g = serial();
    install();
    reset_fixture();
    let pass: gravity_hosts::FilterFn = Rc::new(|_| Ok(true));
    let mut reg = gravity_hosts::HostRegistry::new();
    reg.push(recipe(None, AccessShareLock, &[1]));
    reg.push_bitmap(recipe(None, AccessShareLock, &[1]), Some(Rc::clone(&pass)));
    let mut res = gravity_hosts::ops::scan::pgrc::granule_source::Resolved::new();
    res.cells_from(&reg);
    assert!(res.recheck(0).is_none(), "an index entry has no recheck");
    assert!(res.recheck(1).is_some_and(|f| Rc::ptr_eq(&f, &pass)), "a bind takes this execution's recheck");
    let mut plain = gravity_hosts::HostRegistry::new();
    plain.push(recipe(None, AccessShareLock, &[1]));
    res.cells_from(&plain);
    assert!(res.recheck(0).is_none() && res.recheck(1).is_none(), "the next statement's registry replaces the quals");
}
