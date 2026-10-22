//! The name of a program's first host-called site: what a consumer's refusal of a tier B / C call reports (`name` is `""` outside the tier table, `reason` is the tier C row's).

use gravity_fn::Tier;

use super::func::{Call, How};
use super::Program;
use crate::emit::prim;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostCall {
    pub oid: u32,
    pub name: &'static str,
    pub tier: Tier,
    pub reason: &'static str,
}

pub fn first(p: &Program<'_>) -> Option<HostCall> {
    p.sites.iter().find(|s| !prim::is_prim(p, s, false)).map(of)
}

pub fn of(s: &Call) -> HostCall {
    if let (0, How::Shim { shim, .. }) = (s.funcid, s.how) {
        // a synthetic site (the domain raise): the shim's own name, tier B
        return HostCall { oid: 0, name: shim.name, tier: Tier::B, reason: "" };
    }
    if matches!(s.how, How::Island { .. }) {
        return HostCall { oid: 0, name: "island", tier: Tier::C, reason: "an interpreter island (JsonExpr / XmlExpr: the interpreter serves the subtree)" };
    }
    let row = gravity_fn::row_of(s.funcid);
    let tier = if matches!(s.how, How::Shim { .. }) { Tier::B } else { Tier::C };
    HostCall { oid: s.funcid, name: row.map_or("", |r| r.name), tier, reason: row.filter(|_| tier == Tier::C).map_or("", |r| r.reason) }
}
