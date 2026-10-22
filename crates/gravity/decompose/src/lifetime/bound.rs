//! C4 (the bound-at phase): when PostgreSQL 18.6's executor evaluates a value an executor parameter word or a bound
//! holds, per what the value feeds, as the keep table answers per plan node what a rescan keeps. lowerv4's
//! `lifetime::bound` declares when Gravity evaluates each such word at its lowering site and checks it against this
//! answer: a word evaluated once where PG evaluates it at every rescan of a node the statement rescans is a mismatch
//! unless no evaluation could show the difference (D1630, D1634, D3538, D3600, D3616).

/// When a value is evaluated, from the least often to the most often.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BoundAt {
    /// Fixed when the plan is made: a constant.
    Plan,
    /// Once per execution, before any node runs: a bind-time word (an extern parameter, an expression over them).
    Bind,
    /// At the first open of the node that reads it in an execution, and never again (a runtime key bound when its
    /// pipeline starts; per-open words in a frame no rescan opens again).
    Open,
    /// At the first open and again at every rescan of the node that reads it (ExecReScan of that node).
    Rescan,
    /// For each row.
    Row,
}

impl BoundAt {
    /// How often: a phase of a higher rank evaluates at least wherever one of a lower rank does.
    pub const fn rank(self) -> u8 {
        match self {
            BoundAt::Plan => 0,
            BoundAt::Bind => 1,
            BoundAt::Open => 2,
            BoundAt::Rescan => 3,
            BoundAt::Row => 4,
        }
    }
}

/// What an executor parameter word or a bound feeds: the PG node part whose evaluation `pg` answers for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Feed {
    /// A Limit's count or offset (nodeLimit.c).
    Limit,
    /// The bound a Limit hands its child, a top-k Sort's or a MergeAppend's (ExecSetTupleBound).
    TupleBound,
    /// An index, index-only or bitmap index scan's runtime key: a comparison's operand, an ORDER BY argument, a row
    /// comparison's member (nodeIndexscan.c, nodeIndexonlyscan.c, nodeBitmapIndexscan.c).
    IndexKey,
    /// An `= ANY` array key of an index scan (ExecIndexEvalArrayKeys).
    ArrayKey,
    /// A TID scan's or TID range scan's value (nodeTidscan.c, nodeTidrangescan.c).
    Tid,
    /// A TABLESAMPLE method's argument or REPEATABLE seed (nodeSamplescan.c).
    Sample,
    /// A key an EvalPlanQual recheck's scan reads (the scan's own rule, as its plan node's).
    Recheck,
    /// An operand of a qual (a HAVING comparison's): evaluated with the qual, per row (ExecQual).
    Qual,
}

impl Feed {
    /// PostgreSQL 18.6's answer: every feed here is evaluated at the first open and again at every rescan.
    /// - Limit: recompute_limits at LIMIT_INITIAL and in ExecReScanLimit.
    /// - TupleBound: recompute_limits passes it down (ExecSetTupleBound) at the same two points.
    /// - IndexKey: ExecReScanIndexScan / ExecReScanIndexOnlyScan / ExecReScanBitmapIndexScan run
    ///   ExecIndexEvalRuntimeKeys (any key that is not a Const is a runtime key: ExecIndexBuildScanKeys), and the scan's
    ///   first fetch runs ExecReScan.
    /// - ArrayKey: ExecIndexEvalArrayKeys at the same rescans.
    /// - Tid: ExecReScanTidScan frees tss_TidList, so TidListEval runs again at the next fetch (TidRangeEval likewise).
    /// - Sample: ExecReScanSampleScan clears `begun`, so tablesample_init evaluates the arguments again.
    /// - Recheck: the rechecked scan's own rule (EvalPlanQual re-runs its scan as a rescan).
    /// - Qual: ExecQual evaluates the qual's operands for each row it tests.
    pub const fn pg(self) -> BoundAt {
        match self {
            Feed::Limit | Feed::TupleBound | Feed::IndexKey | Feed::ArrayKey | Feed::Tid | Feed::Sample | Feed::Recheck => BoundAt::Rescan,
            Feed::Qual => BoundAt::Row,
        }
    }
}

/// D1643: when Gravity evaluates a TABLESAMPLE argument or REPEATABLE value (PG: tablesample_init at each (re)scan's
/// first fetch, `Feed::Sample`): one reading an executor parameter (`exec`), or one that is VOLATILE or may raise (`loud`)
/// in a scan other than the statement's first fetch (`first`), at every open of the walk (the host's, as a computed one,
/// never for a scan that fetches nothing); any other once at bind, where its value is the one each rescan computes and
/// evaluating it shows nothing PG's first fetch would not (decompose ops/scan/closure.rs `SampleDesc::per_open`).
pub const fn sample_at(first: bool, loud: bool, exec: bool) -> BoundAt {
    if exec || (loud && !first) {
        BoundAt::Rescan
    } else {
        BoundAt::Bind
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_order_the_phases() {
        let all = [BoundAt::Plan, BoundAt::Bind, BoundAt::Open, BoundAt::Rescan, BoundAt::Row];
        assert!(all.windows(2).all(|w| w[0].rank() < w[1].rank()));
    }

    #[test]
    fn every_feed_is_evaluated_at_every_rescan() {
        for f in [Feed::Limit, Feed::TupleBound, Feed::IndexKey, Feed::ArrayKey, Feed::Tid, Feed::Sample, Feed::Recheck] {
            assert_eq!(f.pg(), BoundAt::Rescan, "{f:?}");
        }
        assert_eq!(Feed::Qual.pg(), BoundAt::Row);
    }

    #[test]
    fn a_loud_sample_value_outside_the_first_fetch_opens_each_rescan() {
        assert_eq!(sample_at(false, true, false), BoundAt::Rescan);
        assert_eq!(sample_at(true, true, false), BoundAt::Bind);
        assert_eq!(sample_at(false, false, false), BoundAt::Bind);
        assert_eq!(sample_at(true, false, true), BoundAt::Rescan);
    }
}
