//! The pulled run's visible decision in the effects home (face/pull.rs's MJ-AHEAD `calm`, moved): whether a pulled
//! pipeline's code may run ahead of its reader (merge.join's pulled inner, the streamed sorted set operation's right input).
use gravity_decompose::join::JoinDesc;
use gravity_decompose::{Op, Source};
use types_nodes::Node;

use crate::frame::px::Px;

/// MJ-AHEAD: pulled pipeline `inner`'s code (its index or bitmap keys and recheck quals, its filters and projections) calls
/// no user, VOLATILE or NOTICE-raising function and reads no user-typed value: reading it ahead of its reader acts nowhere
/// (a raise is held, `m11_hold`).
pub(crate) fn calm(px: &Px<'_, '_>, inner: usize) -> bool {
    let gx = px.gx;
    let ok = |n: Node<'_>| !gx.fx(n).volatile_any() && !gx.fx(n).user_calls() && !gx.fx(n).user_typed() && gx.fx(n).notice_free();
    let p = &gx.pipelines()[inner];
    let src = match &p.source {
        Source::Range { .. } => true,
        Source::Index { keys, recheck, .. } => keys.iter().all(|k| ok(k.value)) && recheck.iter().all(ok),
        Source::Bitmap { plan, recheck, .. } => crate::effects::bitmap_all(plan, &|_, keys, _| keys.iter().all(|k| ok(k.value))) && recheck.iter().all(ok),
        _ => false,
    };
    src && p.ops.iter().all(|op| match op {
        Op::Filter(n) => ok(*n),
        Op::Project(l) => l.iter().all(ok),
        Op::Join(JoinDesc::Spool) => true,
        // a streamed group: no user aggregate, its calls' arguments and FILTERs and its keys calm (a user-typed key's
        // equality is user code)
        Op::GroupRun(i) => gx.graph.streams.get(*i as usize).is_some_and(|g| {
            g.aggs.iter().all(|a| !gravity_decompose::effects::user_code(a.fnoid) && ok(a.node) && a.filter.is_none_or(ok)) && g.keys.iter().all(|k| ok(k.expr.node()))
        }),
        _ => false,
    })
}
