//! NamedTuplestoreScan: a trigger's transition table or an ENR, read by the host.
use gravity_program::{Pipeline, Reason};
use types_nodes::plannodes::NamedTuplestoreScan;

use crate::admit::Refusal;
use crate::graph::Source;
use crate::ledger::Kind;
use crate::ops;
use crate::walk::{Builder, Open};

pub(crate) fn map_named<'p>(b: &mut Builder<'p, '_>, ns: &'p NamedTuplestoreScan<'p>) -> Result<Open<'p>, Refusal> {
    let (plan, k) = (&ns.scan.plan, Kind::NamedTuplestoreScan);
    let id = plan.plan_node_id;
    let rte = b.rte(ns.scan.scanrelid).ok_or(Refusal::shape(k, id, "scanrelid outside the range table"))?;
    let name = ns.enrname.ok_or(Refusal::shape(k, id, "a named tuplestore without a name"))?;
    crate::ops::shared::record_row::named_record_row(b, ns.scan.scanrelid, plan, rte.relid);
    let mut open = Open::new(b.arena, id, Source::Named { scanrelid: ns.scan.scanrelid as u16, relid: rte.relid, name }, Pipeline::serial(-1, gravity_program::End::Sink, Reason::Host));
    ops::filters(&mut open, &plan.qual);
    ops::project(&mut open, &plan.targetlist);
    b.graph.note(k);
    Ok(open)
}
