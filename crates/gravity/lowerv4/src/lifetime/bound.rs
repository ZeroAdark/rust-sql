//! C4, the bound-at phase (the runtime half; PG's answer is decompose's `lifetime::Feed::pg`). Every executor-parameter
//! word or bound a lowered program evaluates is placed or declared here: a Limit's count or offset, a top-k or
//! MergeAppend bound, an index or array key, a TID value, a TABLESAMPLE argument, a recheck's key. `word` and `late` place
//! a bind-time or pipeline-start word (the WordPlan's `key` / `key_late`); `declare` states a word the program evaluates
//! itself (the host's at every open, an executor parameter's cell, a step's per-open words). Each says when Gravity
//! evaluates the value (`BoundAt`), and `mismatch` checks it against PG's answer: a value evaluated less often than PG
//! evaluates it, on a walk PG rescans, where an evaluation could show (it reads an executor parameter, may raise, or calls
//! user or VOLATILE code), is a mismatch. A debug build refuses it at lowering by Bug name; a release build lowers as
//! before. The facts that decide when a walk's words are evaluated again live here too (a re-opened walk: D1630, D3600,
//! D3616; a rebinding bound: D1634; a sectioned top: D3538). Lint RL fails a word placed outside this file.
use gravity_decompose::graph::Bound;
use gravity_lower::Place;
use mcx::CollectIn;
use types_nodes::primnodes::ParamKind;
use types_nodes::Node;

use crate::admit::refusal::Reason;
use crate::frame::px::Px;
use crate::graph::Gx;

pub use gravity_decompose::lifetime::{BoundAt, Feed};

/// A bind-time word evaluated once where PG evaluates it at every rescan of a walk the statement re-opens.
pub(crate) const BIND_ONCE: &str = "bound-at: a value bound once at bind that PG evaluates at every rescan of a re-opened walk, where an evaluation shows (C4)";
/// A word evaluated at its pipeline's first open only where PG evaluates it at every rescan.
pub(crate) const OPEN_ONCE: &str = "bound-at: a value evaluated at its walk's first open only that PG evaluates at every rescan of a re-opened walk, where an evaluation shows (C4)";
/// A plan-time word that is no constant.
pub(crate) const PLAN_NOT_CONST: &str = "bound-at: a plan-time word over a value that is no constant (C4)";

/// D1630: value `v` is a bind-time expression word (lower words.rs `key_word`'s `Expr` / `Late`): no constant, parameter
/// or executor parameter's.
pub fn at_bind(v: Node<'_>) -> bool {
    v.as_const().is_none() && v.as_param().is_none() && !gravity_lower::rerun::words::has_exec(v)
}

/// The walk whose makers run (`Px::making`) is a nest's or SubPlan's embedded inner, re-opened at every outer row.
fn embedded(px: &Px<'_, '_>) -> bool {
    px.making.0 >= px.gx.pipelines().len()
}

/// Pipeline `k` is a member of a rerun set (run again at each of its owner's re-opens).
fn member(gx: &Gx<'_, '_>, k: usize) -> bool {
    gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(k as u16)))
}

/// D1016 / D1630: value `v` is the host's at every (re-)open of its walk: computed from executor parameters
/// (rerun/params_key.rs `computed`), or `at_bind` on a walk PostgreSQL rescans (`reopened`), as PG evaluates it at each
/// rescan (ExecIndexEvalRuntimeKeys, TidListEval), never under an empty outer. Its site param slot is negative
/// (frame/site.rs `index_site`) where a bound key elsewhere has its word's; exec's admission reads that slot as `reopened`
/// and asks the same rule (exec ops/scan/param_key.rs `computed_keys`, param_tid.rs, bitmap_param.rs).
pub fn host_at(v: Node<'_>, reopened: bool) -> bool {
    crate::rerun::params_key::computed(v) || (reopened && at_bind(v))
}

/// D1630: `host_at` for a key on the walk whose makers run (`Px::making`): a nest's or SubPlan's inner walk is re-opened
/// at every outer row. D4049: so is a key the emit's own walk may never evaluate (`first_fetch`).
pub fn per_open(px: &Px<'_, '_>, v: Node<'_>) -> bool {
    host_at(v, embedded(px)) || first_fetch(px, v) || beside_host(px, v)
}

/// D1016 row 2: value `v` is a bind-time expression (`at_bind`) keying the index walk of a rerun set's member (a SubPlan's
/// or LATERAL inner's pipeline, re-opened at each run of its set) beside a key the host computes at every seek
/// (rerun/params_key.rs `computed`): the host's at the seek too, so every runtime key is evaluated at each rescan in
/// index-qual order, as PostgreSQL's ExecIndexEvalRuntimeKeys does (a bind-time word ran once, ahead of the computed key,
/// and never under an empty outer). Exec reads the key's negative site slot as the host's (ops/scan/param_key.rs).
fn beside_host(px: &Px<'_, '_>, v: Node<'_>) -> bool {
    use gravity_decompose::graph::KeyForm;
    let (gx, k) = (px.gx, px.making.0);
    if !at_bind(v) || k >= gx.pipelines().len() || !member(gx, k) {
        return false;
    }
    let gravity_decompose::graph::Source::Index { keys, .. } = &gx.pipelines()[k].source else { return false };
    keys.iter().any(|key| match key.form {
        KeyForm::Null | KeyForm::NotNull => false,
        KeyForm::Row => key.value.as_row_compare_expr().is_some_and(|rc| rc.rargs.iter().any(crate::rerun::params_key::computed)),
        _ => crate::rerun::params_key::computed(key.value),
    })
}

/// D4049: value `v` is a bind-time expression (`at_bind`) keying the index walk of the statement's root pipeline (the
/// emit's own scan), one of whose bind-time keys may raise or calls user or VOLATILE code (effects.rs `expr_quiet`): the
/// host's at the walk's seek, as an inner walk's. PostgreSQL evaluates the runtime keys only at the scan's first fetch, in
/// index-qual order (ExecIndexScan -> ExecReScanIndexScan -> ExecIndexEvalRuntimeKeys), which a LIMIT that counts 0, a
/// false one-time gate or a Bind with no Execute never makes, and the emit's index source seeks at its first pull too
/// (hosts index_source.rs `seek`); D899's late word never names the emit (exec admit `late_owner`), so as a word it was
/// evaluated at Bind or at the entry's open, ahead of any pull. Every bind-time key of such a walk is the host's (a host key
/// beside a bind-time word refuses: params_key.rs `input_cells`); a walk whose bind-time keys are all quiet keeps its words,
/// and a bitmap walk is not one (exec bitmap_param.rs reads only an inner walk's keys as the host's). So does a walk whose
/// pipeline probes a hash build that takes no D1902 `Order` (probe_calls.rs `seek_orders`: D3609 refuses a host key there,
/// SEEK_KEY_PROBE, where PostgreSQL's build-first join may never pull the walk), one with a SubPlan in a key (a host key
/// refuses it, params_key.rs `computed_key`) and one keyed by an executor parameter too (an InitPlan's: its cell's place
/// beside a host key is D1016 row 2's, `init_ahead`): their words are bound at bind, as before.
fn first_fetch(px: &Px<'_, '_>, v: Node<'_>) -> bool {
    use gravity_decompose::join::JoinDesc;
    use gravity_decompose::Op;
    let (gx, k) = (px.gx, px.making.0);
    if k != gx.graph.root as usize || member(gx, k) || !at_bind(v) {
        return false;
    }
    let p = &gx.pipelines()[k];
    let gravity_decompose::graph::Source::Index { keys, .. } = &p.source else { return false };
    if p.ops.iter().any(|op| matches!(op, Op::Join(JoinDesc::Probe { .. }))) && !crate::ops::hashjoin::probe_calls::seek_orders(gx, k) {
        return false;
    }
    let mcx = px.mcx();
    let vals = || {
        keys.iter().flat_map(|key| {
            match key.form {
                gravity_decompose::graph::KeyForm::Null | gravity_decompose::graph::KeyForm::NotNull => mcx::PgVec::new_in(mcx),
                gravity_decompose::graph::KeyForm::Row => key.value.as_row_compare_expr().map_or_else(|| mcx::PgVec::new_in(mcx), |rc| rc.rargs.iter().collect_in(mcx)),
                _ => mcx::pg_vec![in mcx; key.value],
            }
        })
    };
    if vals().any(|x| gravity_lower::rerun::words::has_exec(x) || gravity_decompose::expr::has_subplan(x)) {
        return false;
    }
    vals().any(|x| at_bind(x) && !crate::effects::expr_quiet(gx, x))
}

/// D3616: whether pipeline `k`'s walk is re-opened per outer row, so PG evaluates its TID values at each rescan
/// (TidListEval / TidRangeEval at the first fetch after ExecReScanTidScan): a nest inner's embedded walk (`Px::making`
/// past the graph's pipelines, `per_open`) or a member of a rerun set (a correlated SubPlan's, a LATERAL inner's
/// pipeline). A member PG keeps at a rescan that changes no param runs at its set's first run only (graph `kept`), so its
/// walk opens, and its value is evaluated, once as PG's.
pub fn reopened(px: &Px<'_, '_>, k: usize) -> bool {
    embedded(px) || k >= px.gx.pipelines().len() || member(px.gx, k)
}

/// D1634: bound `b` of rerun set pipeline `k` is an expression bound at bind (not a constant or a bare parameter), which
/// PostgreSQL's recompute_limits evaluates at each rescan and never for a run it skips: it is read per open
/// (rerun/params_bound.rs `admit`).
pub fn rebinds(gx: &Gx<'_, '_>, k: usize, b: &Bound<'_>) -> bool {
    matches!(*b, Bound::Expr(e) if e.as_const().is_none() && e.as_param().is_none()) && member(gx, k)
}

/// D3538 (Workers IO sig-f7bd0180c7c1): pipeline `p` is a rerun set's member that may run as a section (a nested member
/// over its claims, D3014 S2'): the section's contexts open it, never its reader's, so its per-open words in a reader's
/// frame are reset by no open (a Limit reading them kept the first run's bound at every rerun).
pub fn sectioned(gx: &Gx<'_, '_>, p: usize) -> bool {
    gx.pipelines()[p].desc.is_eligible() && member(gx, p)
}

/// When a reader in pipeline `k` sees per-open words `[value, flags]` of pipeline `p` (rerun/params_bound.rs
/// `Computed::read`: written by the first read after `p`'s open): at every open of `p`, so at every rescan when `p` is the
/// reader or a top that opens at each run of its set; once when `p` runs as a section (`sectioned`).
pub fn open_words(gx: &Gx<'_, '_>, p: usize, k: usize) -> BoundAt {
    if p != k && sectioned(gx, p) {
        BoundAt::Open
    } else {
        BoundAt::Rescan
    }
}

/// When Gravity evaluates value `v` placed as a word (`late`: bound when its pipeline starts, D899): a constant's at plan
/// time, an executor parameter's cell at every open (`exec_key`), an extern parameter's or an expression's once.
fn placed(v: Node<'_>, late: bool) -> BoundAt {
    match v.as_param() {
        _ if v.as_const().is_some() => BoundAt::Plan,
        Some(p) if p.paramkind == ParamKind::PARAM_EXEC => BoundAt::Rescan,
        Some(_) => BoundAt::Bind,
        None if late => BoundAt::Open,
        None => BoundAt::Bind,
    }
}

/// Value `v` feeding `feed` in pipeline `k` (plan node `node`) as a bind-time word: the WordPlan's `key`, declared.
pub(crate) fn word<'p>(px: &mut Px<'_, 'p>, feed: Feed, k: usize, v: Node<'p>, node: i32) -> Place {
    let p = px.plan.key(v);
    if p != Place::Fold {
        declare(px, feed, k, v, node, placed(v, false));
    }
    p
}

/// `word` bound when the pipeline reading it starts (D899: PostgreSQL's runtime key at its scan's first pull): the
/// WordPlan's `key_late`, declared.
pub(crate) fn late<'p>(px: &mut Px<'_, 'p>, feed: Feed, k: usize, v: Node<'p>, node: i32) -> Place {
    let p = px.plan.key_late(v);
    if p != Place::Fold {
        declare(px, feed, k, v, node, placed(v, true));
    }
    p
}

/// Value `v` feeding `feed` in pipeline `k` (plan node `node`) is evaluated at `at`: a debug build refuses a mismatch
/// with PG's answer by Bug name (`mismatch`).
pub(crate) fn declare(px: &Px<'_, '_>, feed: Feed, k: usize, v: Node<'_>, node: i32, at: BoundAt) {
    if cfg!(debug_assertions) {
        if let Some(why) = mismatch(px, feed, k, v, at) {
            crate::refuse!(node, Reason::Bug { payload: why });
        }
    }
}

/// `declare`'s check: `None` when Gravity evaluates `v` at least as often as PG does (`Feed::pg`), when PG rescans no walk
/// here (`reopened`: PG evaluates it once too), or when no evaluation could show: a constant at plan time; at bind, a value
/// over no executor parameter that raises nothing and calls no user or VOLATILE code (effects.rs `expr_quiet`); at the
/// walk's first open, one over no executor parameter that calls no user or VOLATILE code (PG raises at its first rescan
/// too).
pub(crate) fn mismatch(px: &Px<'_, '_>, feed: Feed, k: usize, v: Node<'_>, at: BoundAt) -> Option<&'static str> {
    if at.rank() >= feed.pg().rank() || !reopened(px, k) {
        return None;
    }
    let gx = px.gx;
    let reads = || gravity_lower::rerun::words::has_exec(v) || crate::effects::user_or_volatile(gx, v);
    match at {
        BoundAt::Plan => v.as_const().is_none().then_some(PLAN_NOT_CONST),
        BoundAt::Bind => (reads() || !crate::effects::expr_quiet(gx, v)).then_some(BIND_ONCE),
        BoundAt::Open => reads().then_some(OPEN_ONCE),
        BoundAt::Rescan | BoundAt::Row => None,
    }
}

/// How an index key's value is placed (`index_key`).
pub(crate) enum KeyAt {
    /// The host's at every seek (`per_open`): its computed cell, `HOST_KEY`, its executor parameters added to the ids.
    Host(u16),
    /// A word bound when its pipeline starts (or at bind: a constant's, an extern parameter's), or an executor
    /// parameter's cell read at every (re-)open (rerun/params.rs `exec_key`).
    Word(u16),
    /// No word (a folded expression): the caller refuses.
    Fold,
}

/// An index, index-only, AM or bitmap key's value `v` feeding `feed` in pipeline `k` (plan node `node`): the host's at
/// every seek when `per_open` (its parameters into `ids`), else a word bound when the pipeline reading it starts (D899:
/// PostgreSQL's runtime key at its scan's first pull, `late`) or an executor parameter's cell.
pub(crate) fn index_key<'p>(px: &mut Px<'_, 'p>, feed: Feed, k: usize, v: Node<'p>, node: i32, ids: &mut mcx::PgVec<'_, i32>) -> KeyAt {
    if per_open(px, v) {
        declare(px, feed, k, v, node, BoundAt::Rescan);
        return KeyAt::Host(crate::rerun::params_key::computed_key(node, v, ids));
    }
    match late(px, feed, k, v, node) {
        Place::Word(w) => KeyAt::Word(w),
        Place::Exec(id) => KeyAt::Word(crate::rerun::params::exec_key(px, node, id)),
        Place::Fold => KeyAt::Fold,
    }
}
