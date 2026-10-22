//! A nest's parameters read through the interface home (lint R12): a NestLoopParam's `paramval` is a plan-node field, so a
//! family asks this instead of matching the node itself.
use types_nodes::list::NodeList;
use types_nodes::Node;

/// A nest's params (`nestParams`): each NestLoopParam's `paramval` (a Var of the outer after setrefs) under `one`; any
/// other node counts.
pub fn nest_param_vals<'p>(params: &NodeList<'p>, one: &dyn Fn(Node<'p>) -> bool) -> bool {
    params.iter().any(|x| x.as_nest_loop_param().is_none_or(|p| one(p.paramval)))
}

/// The value NestLoopParam `paramno` of a nest's params (`nestParams`) carries (a Var of the outer after setrefs), when
/// the nest sets it.
pub fn nest_param_val<'p>(params: &NodeList<'p>, paramno: i32) -> Option<Node<'p>> {
    params.iter().filter_map(|x| x.as_nest_loop_param()).find(|p| p.paramno == paramno).map(|p| p.paramval)
}
