//! The streamed sorted SetOp (D2801): which sorted INTERSECT / EXCEPT [ALL] the lowering runs as PostgreSQL 18.6's
//! setop_retrieve_sorted runs it (nodeSetOp.c): the left input read in its own pipeline, one group plus its look-ahead row
//! at a time, and the right input pulled one fetch at a time (lowerv4 ops/setop/stream.rs over the shared pulled-run
//! reader, face/pull.rs). A LIMIT, a cursor or the left's end then stops both inputs where PG stops them, and a right
//! input that raises or acts does so at the row PG fetches. The grouped form (each side a group, the copy counts filed by
//! a protocol: agg `set_side`) reads both inputs whole.
//!
//! `form` serves a sorted SetOp with compared columns whose right input is no Sort (the caller keeps the grouped form for
//! a quiet right input: its whole read is unobservable), or a quiet Sort over one (D2805, `sorted_scan`: a partial reader
//! over a left that may raise), where
//! - the right input is a relation scan (Seq / Index / Index Only / Bitmap Heap Scan) with no InitPlan, SubPlan or
//!   executor parameter: its pipeline is a scan with filters and a projection, pulled as a merge join's inner is;
//! - every output column is a compared column, at most 64: the step hands each group's first row on from its key words
//!   (a text key's word the address of the step's copy of that row's value);
//! - every compared column is under an ordering operator of its type's default btree family
//!   (`facts::order::default_order`), and is either a by-value word whose image is its raw datum (the step compares the
//!   datum words in the order the key semantics give it: lowerv4 asks KeySem for each key's direction and signedness) or
//!   a text / varchar key under a collation (the step compares two values as the key's sort support does, kernels
//!   ops/setop/raw.rs `StreamTextCmp`), or (AGA-30) a by-reference value of a built-in base type whose default btree
//!   comparator is built in (`hosted_key`: numeric_cmp, bpcharcmp, uuid_cmp, ...; the step calls it once per pair as the
//!   key's sort support does, lowerv4 ops/setop/raw.rs `StreamHostedCmp`, never user code); every collatable key under
//!   the same one.
//!
//! NULLS FIRST / LAST is the plan's (`cmpNullsFirst`), and the text keys' collation the plan's (`cmpCollations`), carried
//! in the form. Owner M11-MERGE (the setop family's arm is one call in ops/setop.rs).
use types_nodes::plannodes::SetOp;
use types_nodes::{Node, NodeTag};

use crate::catalog::{TEXTOID, VARCHAROID};
use crate::ops::shared::setop::SetForm;
use crate::ops::shared::sort::EqKey;
use crate::walk::Builder;

/// SetOp `so`'s streamed form over left plan `lp`, its compared columns' `keys` (ops/setop.rs `eq_keys`) and right input
/// `rn` with `n` compared columns, when served (module header).
pub(super) fn form<'p>(b: &Builder<'p, '_>, so: &'p SetOp<'p>, lp: &'p types_nodes::plannodes::Plan<'p>, keys: &[EqKey], rn: Node<'p>, n: usize) -> Option<SetForm> {
    if so.strategy == 1 || n == 0 || n > 64 || !(scan(rn) || sorted_scan(b, rn).is_some()) {
        return None;
    }
    let cols = so.cmpColIdx.get(..n)?;
    // every output column compared, each once
    if lp.targetlist.len() != n || keys.len() != n || !(1..=n as i16).all(|r| cols.iter().filter(|&&c| c == r).count() == 1) {
        return None;
    }
    let mut coll = 0;
    for (i, key) in keys.iter().enumerate() {
        let (typ, op) = (key.typid, key.opno);
        let tf = crate::catalog::typefacts::facts_of(b.facts, typ)?;
        if !crate::facts::order::default_order(b.facts, op) {
            return None;
        }
        if tf.byval && matches!(tf.typlen, 1 | 2 | 4 | 8) && tf.image.is_word() {
            continue;
        }
        let c = so.cmpCollations.get(i).copied().unwrap_or(0);
        let text = matches!(tf.base, TEXTOID | VARCHAROID);
        if !text && !hosted_key(b.facts, &tf) {
            return None;
        }
        if !text && !tf.collatable {
            continue;
        }
        if c == 0 || (coll != 0 && c != coll) {
            return None;
        }
        coll = c;
    }
    let nulls_first = (0..n).filter(|&i| so.cmpNullsFirst.get(i).copied().unwrap_or(false)).fold(0u64, |m, i| m | 1 << i);
    Some(SetForm::Stream { nulls_first, coll })
}

/// AGA-30: a key the step compares by its type's built-in btree comparator over the value (lowerv4 ops/setop/stream.rs
/// `key`, ops/setop/raw.rs `StreamHostedCmp`): one the effects home names, a built-in base type's by-reference value
/// whose comparator reaches no user code (`effects::setop::builtin_cmp_key`).
pub fn hosted_key(f: &dyn crate::catalog::Facts, tf: &crate::catalog::typefacts::TypeFacts) -> bool {
    crate::effects::setop::builtin_cmp_key(f, tf)
}

/// D2805 (D3377): a Sort over such a relation scan (`scan`) whose subtree neither acts, may raise nor calls a user function
/// (effects::setop `quiet`): the lowering pulls the Sort's emit as it pulls a scan (its fill an exclusive producer of the
/// pulled set, run at the first pull: PG's ExecSort sorts its whole input at the SetOp's first right fetch). The map keeps
/// the grouped form for a whole reader; a partial reader over a left that may raise takes this one (ops/setop.rs). The
/// licence of the fill's run ahead comes back (ops/setop.rs `quiet_ahead`).
pub(super) fn sorted_scan<'p>(b: &Builder<'p, '_>, rn: Node<'p>) -> Option<crate::effects::Licence> {
    let s = rn.as_sort()?;
    (s.plan.initPlan.len() == 0 && s.plan.lefttree.is_some_and(scan)).then(|| super::quiet_ahead(b, rn)).flatten()
}

/// A relation scan with filters and a projection only: no InitPlan, no executor parameter (a correlated or InitPlan
/// value), no SubPlan in its quals or target list.
fn scan(rn: Node<'_>) -> bool {
    let Some(p) = rn.as_plan() else { return false };
    let kind = matches!(rn.node_tag(), NodeTag::T_SeqScan | NodeTag::T_IndexScan | NodeTag::T_IndexOnlyScan | NodeTag::T_BitmapHeapScan);
    let plain = |l: &types_nodes::list::NodeList<'_>| l.iter().all(|e| crate::expr::walk(e, &mut |x| x.as_sub_plan().is_none()));
    kind && p.initPlan.len() == 0 && p.allParam.is_empty() && plain(&p.qual) && plain(&p.targetlist)
}
