// Planner support-request nodes (supportnodes.h). Stack-built by the planner,
// passed to prosupport functions as a pointer Datum; tag-first repr(C) so the
// callee can demux on the leading NodeTag alone. C's `root` is omitted:
// PlannerInfo never crosses the fmgr boundary here; its one consumer, the
// SupportRequestRows estimators' estimate_expression_value(req->root, arg),
// reads root->glob->boundParams, carried as `bound_params_raw`.
use crate::node_tree::Node;
use crate::tags::NodeTag;
use types_core::Oid;

#[repr(C)]
pub struct SupportRequestRows<'mcx> {
    tag: NodeTag,
    pub funcid: Oid,
    pub node: Option<Node<'mcx>>,
    pub rows: f64,
    // root->glob->boundParams as the types_portal::ParamListHandle's raw bits
    // (0 = NULL): the estimators hand it to
    // clauses_seams::estimate_expression_value.
    pub bound_params_raw: u64,
}

// operator oid -> selectivity; errors propagate through the fmgr call.
pub type SelectivityEstimator<'a> =
    dyn FnMut(Oid) -> Result<f64, alloc::boxed::Box<types_error::PgError>> + 'a;

// C carries root/args/inputcollid/varRelid/jointype/sjinfo so the callee can
// invoke restriction_selectivity/join_selectivity itself; this port's planner
// state stays behind `estimate`, pre-bound by function_selectivity to the
// right one of those two paths.
#[repr(C)]
pub struct SupportRequestSelectivity<'a> {
    tag: NodeTag,
    pub funcid: Oid,
    pub is_join: bool,
    pub selectivity: f64,
    pub estimate: &'a mut SelectivityEstimator<'a>,
}

impl<'a> SupportRequestSelectivity<'a> {
    pub fn new(funcid: Oid, is_join: bool, estimate: &'a mut SelectivityEstimator<'a>) -> Self {
        SupportRequestSelectivity {
            tag: NodeTag::T_SupportRequestSelectivity,
            funcid,
            is_join,
            selectivity: -1.0,
            estimate,
        }
    }
}

/// Demux a prosupport request pointer by its leading tag.
///
/// # Safety
/// `p` must point at a live support-request node built by the `new`
/// constructors in this module (tag-first repr(C)), exclusively borrowed
/// for `'a`.
pub unsafe fn support_request_selectivity_mut<'a, 'b>(
    p: *mut (),
) -> Option<&'a mut SupportRequestSelectivity<'b>> {
    // SAFETY: caller contract — tag-first node, live and exclusive.
    unsafe {
        if *p.cast::<NodeTag>() != NodeTag::T_SupportRequestSelectivity {
            return None;
        }
        Some(&mut *p.cast::<SupportRequestSelectivity<'b>>())
    }
}

#[repr(C)]
pub struct SupportRequestSimplify<'mcx> {
    tag: NodeTag,
    pub fcall: Option<Node<'mcx>>,
    // Stands in for C's root->planner_cxt: a rewrite must allocate somewhere.
    pub mcx: Option<::mcx::Mcx<'mcx>>,
}

impl<'mcx> SupportRequestSimplify<'mcx> {
    pub fn new(fcall: Option<Node<'mcx>>, mcx: Option<::mcx::Mcx<'mcx>>) -> Self {
        SupportRequestSimplify { tag: NodeTag::T_SupportRequestSimplify, fcall, mcx }
    }
}

#[repr(C)]
pub struct SupportRequestCost<'mcx> {
    tag: NodeTag,
    pub funcid: Oid,
    pub node: Option<Node<'mcx>>,
    pub startup: f64,
    pub per_tuple: f64,
}

// C's root/index pointers are omitted (planner types don't cross fmgr);
// consumers that need them land with match_pattern_prefix.
#[repr(C)]
pub struct SupportRequestIndexCondition<'mcx> {
    tag: NodeTag,
    pub funcid: Oid,
    pub node: Option<Node<'mcx>>,
    pub indexarg: i32,
    pub indexcol: i32,
    pub opfamily: Oid,
    pub indexcollation: Oid,
    pub lossy: bool,
}

/// C SupportRequestModifyInPlace: may `funcid` modify the PARAM_EXTERN
/// `paramid` among `args` in place? The answer is that Param node or NULL.
#[repr(C)]
pub struct SupportRequestModifyInPlace<'a, 'mcx> {
    tag: NodeTag,
    pub funcid: Oid,
    pub args: &'a [Option<Node<'mcx>>],
    pub paramid: i32,
}

impl<'a, 'mcx> SupportRequestModifyInPlace<'a, 'mcx> {
    pub fn new(funcid: Oid, args: &'a [Option<Node<'mcx>>], paramid: i32) -> Self {
        SupportRequestModifyInPlace { tag: NodeTag::T_SupportRequestModifyInPlace, funcid, args, paramid }
    }
}

/// # Safety
/// As [`support_request_selectivity_mut`], shared.
pub unsafe fn support_request_modify_in_place<'r, 'a, 'mcx>(
    p: *const (),
) -> Option<&'r SupportRequestModifyInPlace<'a, 'mcx>> {
    // SAFETY: caller contract — tag-first node, live for 'r.
    unsafe {
        if *p.cast::<NodeTag>() != NodeTag::T_SupportRequestModifyInPlace {
            return None;
        }
        Some(&*p.cast::<SupportRequestModifyInPlace<'a, 'mcx>>())
    }
}

const _: () = {
    assert!(core::mem::offset_of!(SupportRequestModifyInPlace, tag) == 0);
    assert!(core::mem::offset_of!(SupportRequestRows, tag) == 0);
    assert!(core::mem::offset_of!(SupportRequestCost, tag) == 0);
    assert!(core::mem::offset_of!(SupportRequestSimplify, tag) == 0);
    assert!(core::mem::offset_of!(SupportRequestIndexCondition, tag) == 0);
    assert!(core::mem::offset_of!(SupportRequestSelectivity, tag) == 0);
};

impl<'mcx> SupportRequestIndexCondition<'mcx> {
    pub fn new(
        funcid: Oid,
        node: Option<Node<'mcx>>,
        indexarg: i32,
        indexcol: i32,
        opfamily: Oid,
        indexcollation: Oid,
    ) -> Self {
        SupportRequestIndexCondition {
            tag: NodeTag::T_SupportRequestIndexCondition,
            funcid,
            node,
            indexarg,
            indexcol,
            opfamily,
            indexcollation,
            lossy: true,
        }
    }
}

impl<'mcx> SupportRequestRows<'mcx> {
    pub fn new(funcid: Oid, node: Option<Node<'mcx>>) -> Self {
        SupportRequestRows {
            tag: NodeTag::T_SupportRequestRows,
            funcid,
            node,
            rows: 0.0,
            bound_params_raw: 0,
        }
    }

    pub fn with_bound_params_raw(mut self, bound_params_raw: u64) -> Self {
        self.bound_params_raw = bound_params_raw;
        self
    }
}

impl<'mcx> SupportRequestCost<'mcx> {
    pub fn new(funcid: Oid, node: Option<Node<'mcx>>) -> Self {
        SupportRequestCost {
            tag: NodeTag::T_SupportRequestCost,
            funcid,
            node,
            startup: 0.0,
            per_tuple: 0.0,
        }
    }
}

/// Demux a prosupport request pointer by its leading tag.
///
/// # Safety
/// `p` must point at a live support-request node built by the `new`
/// constructors above (tag-first repr(C)), exclusively borrowed for `'a`.
pub unsafe fn support_request_rows_mut<'a, 'mcx>(
    p: *mut (),
) -> Option<&'a mut SupportRequestRows<'mcx>> {
    // SAFETY: caller contract — tag-first node, live and exclusive.
    unsafe {
        if *p.cast::<NodeTag>() != NodeTag::T_SupportRequestRows {
            return None;
        }
        Some(&mut *p.cast::<SupportRequestRows<'mcx>>())
    }
}

/// Demux a prosupport request pointer by its leading tag.
///
/// # Safety
/// Same contract as [`support_request_rows_mut`].
pub unsafe fn support_request_cost_mut<'a, 'mcx>(
    p: *mut (),
) -> Option<&'a mut SupportRequestCost<'mcx>> {
    // SAFETY: caller contract — tag-first node, live and exclusive.
    unsafe {
        if *p.cast::<NodeTag>() != NodeTag::T_SupportRequestCost {
            return None;
        }
        Some(&mut *p.cast::<SupportRequestCost<'mcx>>())
    }
}
