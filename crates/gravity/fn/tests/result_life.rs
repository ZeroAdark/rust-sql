//! The result-lifetime contract (docs/gravity/lowerv3/design.md §3): every shim's and fmgr site's declared `ResultLife`.
mod common;

use std::ffi::CString;

use datum::Datum;
use gravity_fn::{result_life, site_life, CallTier, Family, FmgrCall, ResultLife, SHIMS};
use mcx::MemoryContext;

const INT4PL: u32 = 177;
const TEXTIN: u32 = 46;
const NUMERIC_IN: u32 = 1701;
const NUMERIC_OUT: u32 = 1702;
const NUMERICOID: u32 = 1700;

fn cstr(s: &str) -> CString {
    CString::new(s).unwrap()
}

#[test]
fn every_shim_declares_its_life() {
    for s in SHIMS.iter() {
        let life = gravity_fn::result_life::shim_life(s);
        match s.family {
            Family::ByRefRef | Family::ValView => {
                assert!(s.region, "{} ({}): a by-reference result without a region", s.name, s.oid);
                assert_eq!(life, ResultLife::CallerRegion, "{}", s.name);
            }
            Family::ByVal | Family::ByRefVal | Family::ViewVal => {
                assert!(!s.region, "{} ({}): a by-value result with a region", s.name, s.oid);
                assert_eq!(life, ResultLife::ByValue, "{}", s.name);
            }
            Family::None => panic!("{}: a shim without a family", s.name),
        }
    }
}

#[test]
fn a_site_binds_the_tier_expr_binds() {
    common::install_seams();
    assert_eq!(result_life(INT4PL, true), ResultLife::ByValue);
    // textin(cstring) is a shim; CoerceViaIO's three-argument input call is not (D202's site)
    assert_eq!(site_life(TEXTIN, 1, false), (CallTier::Shim, ResultLife::CallerRegion));
    assert_eq!(site_life(TEXTIN, 3, false), (CallTier::Fmgr, ResultLife::UntilNextCall));
    assert_eq!(result_life(NUMERIC_OUT, false), ResultLife::CallerRegion);
    // not a builtin: a user function binds the function manager and is never trusted fresh
    assert_eq!(site_life(4_000_000_000, 1, false), (CallTier::Fmgr, ResultLife::UntilNextCall));
    assert_eq!(site_life(4_000_000_000, 1, true), (CallTier::Fmgr, ResultLife::ByValue));
}

#[test]
fn an_unlisted_builtin_is_until_next_call() {
    common::install_seams();
    for row in gravity_fn::ROWS.iter().filter(|r| r.status == gravity_fn::Status::Fmgr) {
        if gravity_fn::fresh_entry(row.oid).is_none() {
            assert_eq!(gravity_fn::result_life::fmgr_life(row.oid, false), ResultLife::UntilNextCall, "{}", row.name);
        }
        assert_eq!(gravity_fn::result_life::fmgr_life(row.oid, true), ResultLife::ByValue, "{}", row.name);
    }
}

#[test]
fn a_bound_site_carries_its_life_and_its_parent() {
    common::install_seams();
    let old = FmgrCall::bind(TEXTIN, 3, 0).unwrap();
    assert_eq!(old.result_life(), ResultLife::UntilNextCall);
    let byval = FmgrCall::bind_under(INT4PL, 2, 0, &MemoryContext::new("stmt"), 0).unwrap();
    assert_eq!(byval.result_life(), ResultLife::ByValue);

    let parent = MemoryContext::new("statement grant");
    let before = parent.subtree_allocated();
    let mut call = FmgrCall::bind_under(NUMERIC_IN, 3, 0, &parent, -1).unwrap();
    let s = cstr("12345.678");
    let r = call.call_site(&[Datum::from_usize(s.as_ptr() as usize), Datum::from_u32(NUMERICOID), Datum::from_i32(-1)], &[false; 3]).unwrap();
    assert!(!r.isnull);
    assert!(call.result_bytes() > 0);
    assert!(parent.subtree_allocated() >= before + call.result_bytes(), "the site's result context counts under its parent");
    call.reset_window();
    assert!(call.result_bytes() <= gravity_fn::FMGR_SITE_ALLOWANCE, "a reset site keeps its keeper block only: {}", call.result_bytes());
}
