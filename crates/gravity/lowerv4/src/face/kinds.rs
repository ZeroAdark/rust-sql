//! Every kind (plan §3.1, §3.2): a capability, what is served. One row per kind of the M0 model's `KINDS` (the Python model
//! is deleted in v5, PLAN-v5.md 2.5: this table is the source), in its order, with the face, the milestone that builds it and
//! the file that implements it. Closed at M0: the list never changes after M0 (a new kind is a design revision). A kind no
//! appended registry table provides refuses `NotBuilt { hook: face, variant, milestone }`.
use crate::admit::refusal::Milestone;

/// The hook families a kind belongs to (`model.py` KINDS' face column).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Face {
    Source,
    Step,
    End,
    Fold,
    Keyed,
    Runs,
    Rep,
    Join,
    Spill,
    Nest,
    Srf,
    Setop,
    Window,
    Merge,
    RowOp,
}

impl Face {
    pub const fn name(self) -> &'static str {
        match self {
            Face::Source => "source",
            Face::Step => "step",
            Face::End => "end",
            Face::Fold => "fold",
            Face::Keyed => "keyed",
            Face::Runs => "runs",
            Face::Rep => "rep",
            Face::Join => "join",
            Face::Spill => "spill",
            Face::Nest => "nest",
            Face::Srf => "srf",
            Face::Setop => "setop",
            Face::Window => "window",
            Face::Merge => "merge",
            Face::RowOp => "rowop",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KindDecl {
    pub id: KindId,
    /// The id as model.py spells it (`src.pgrc`): the text a `NotBuilt` names.
    pub name: &'static str,
    pub face: Face,
    pub milestone: Milestone,
    /// The implementing file, relative to `crates/gravity/lowerv4/src/`.
    pub file: &'static str,
    pub what: &'static str,
}

macro_rules! kinds {
    ($($v:ident $name:literal $m:ident $face:ident $file:literal $what:literal;)*) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[repr(u8)]
        pub enum KindId { $($v),* }

        pub const KINDS: &[KindDecl] = &[$(KindDecl { id: KindId::$v, name: $name, face: Face::$face, milestone: Milestone::$m, file: $file, what: $what }),*];
    };
}

kinds! {
    SrcResult "src.result" M1 Source "source/result.rs" "Result / constant rows (a once source)";
    SrcValues "src.values" M1 Source "source/values.rs" "VALUES lists";
    SrcPgrc "src.pgrc" M1 Source "source/pgrc.rs" "pgrc granule range scan";
    SrcIndex "src.index" M1 Source "source/index.rs" "btree index scan with scalar / range keys, rows in windows";
    SrcHeap "src.heap" M2 Source "source/heap.rs" "heap range scan (host-deformed windows; the page road is heap.pages)";
    SrcIndexOnly "src.index_only" M2 Source "source/index_only.rs" "index-only scan from the visibility map";
    SrcIndexArray "src.index_array" M2 Source "source/index_array.rs" "= ANY / IN-list keys as native array-key seeks";
    SrcIndexAm "src.index_am" M2 Source "source/index_am.rs" "hash / gist / spgist walks, KNN reorder";
    SrcBitmap "src.bitmap" M2 Source "source/bitmap.rs" "bitmap heap scan over one index";
    SrcBitmapTree "src.bitmap_tree" M2 Source "source/bitmap_tree.rs" "BitmapAnd / BitmapOr trees";
    SrcTid "src.tid" M2 Source "source/tid.rs" "TidScan and TidRangeScan";
    SrcSample "src.sample" M2 Source "source/sample.rs" "TABLESAMPLE";
    SrcAppend "src.append" M2 Source "source/append.rs" "Append arms (partitions, UNION ALL)";
    SrcPrune "src.prune" M2 Source "source/prune.rs" "initial and exec partition pruning";
    StepFilter "step.filter" M1 Step "step/filter.rs" "quals over cells";
    StepProject "step.project" M1 Step "step/project.rs" "projection cells";
    StepGate "step.gate" M1 Step "step/gate.rs" "one-time gates";
    StepLimit "step.limit" M1 Step "step/limit.rs" "LIMIT / OFFSET counters in per-open words";
    ExprCore "expr.core" M1 Step "step/expr.rs" "expression compile: tier-A ops + hosted tier-B calls";
    ExprSaop "expr.saop" M2 Step "step/expr_saop.rs" "ScalarArrayOp (= ANY / IN) as an expression";
    ExprWholerow "expr.wholerow" M2 Step "step/expr_row.rs" "whole-row references and record outputs";
    ExprVolatile "expr.volatile" M2 Step "step/expr_volatile.rs" "VOLATILE calls once per source row";
    EndSink "end.sink" M1 End "end/sink.rs" "the out-block sink (serial)";
    FoldPlain "fold.plain" M1 End "fold/plain.rs" "plain aggregate end";
    FoldFilter "fold.filter" M1 End "fold/filter.rs" "aggregate FILTER";
    ClassWord "class.word" M1 Fold "fold/class_word.rs" "count / sum / avg / min / max / bool / bit over Word";
    ClassFloat "class.float" M1 Fold "fold/class_float.rs" "float folds with PG's NaN / Inf rules";
    ClassNumPacked "class.num_packed" M1 Fold "fold/class_num.rs" "numeric folds in a scaled register (sum / avg / min / max with a decided scale)";
    ClassNumVarlena "class.num_varlena" M6 Fold "fold/class_num_varlena.rs" "numeric folds in the Var register (no decided scale)";
    ClassNumWide "class.num_wide" M3 Fold "fold/class_num_wide.rs" "numeric sum / avg of a packed factor times a one-word one in the scaled i256 slot (V4R-38)";
    ClassMinmaxCode "class.minmax_code" M1 Fold "fold/class_minmax_code.rs" "min / max over a Code lane: in-part rank, one render per part";
    ClassText "class.text" M6 Fold "fold/class_text.rs" "min / max over varlena text and bytes";
    ClassCollect "class.collect" M6 Fold "fold/class_collect.rs" "array_agg / string_agg / json(b)_agg / range aggs";
    ClassUser "class.user" M6 Fold "fold/class_user.rs" "user-defined aggregates (transfn / finalfn calls)";
    ClassStats "class.stats" M6 Fold "fold/class_stats.rs" "variance / stddev / covar / regr families";
    ClassOrdered "class.ordered" M6 Fold "fold/class_ordered.rs" "ordered-set and hypothetical aggregates";
    ClassOther "class.other" M6 Fold "fold/class_other.rs" "interval / money / opaque / tid folds";
    KeyedDistinctPlain "keyed.distinct_plain" M1 Keyed "keyed/distinct_plain.rs" "count(DISTINCT x) with no group key";
    KeyedHashagg "keyed.hashagg" M1 Keyed "keyed/hashagg.rs" "hashed grouping (phase-1 table, owner phase)";
    KeyedEmitGroup "keyed.emit_group" M1 Keyed "keyed/emit_group.rs" "group emit with HAVING";
    KeyedTwopass "keyed.twopass" M1 Keyed "keyed/twopass.rs" "two-pass form: pass-1 export, admitted set, pass-2 probe";
    KeyedDistinctGrouped "keyed.distinct_grouped" M3 Keyed "keyed/pairs.rs" "grouped DISTINCT aggregates (pair sets)";
    KeyedDistinctMulti "keyed.distinct_multi" M6 Keyed "keyed/distinct_multi.rs" "aggregates over more than one DISTINCT argument (a pair set per argument)";
    KeyedGsets "keyed.gsets" M3 Keyed "keyed/gsets.rs" "grouping sets (the union rewrite)";
    RunsTopkGroups "runs.topk_groups" M1 Runs "runs/topk_groups.rs" "top-k over group finals (final-ordered owner top)";
    RunsSort "runs.sort" M3 Runs "runs/sort.rs" "sort runs + run merge + sort emit";
    RunsTopkRows "runs.topk_rows" M3 Runs "runs/topk_rows.rs" "bounded run over rows, bound in IR";
    RunsUnique "runs.unique" M3 Runs "runs/unique.rs" "Unique / DISTINCT over sorted input";
    RunsGroup "runs.group" M3 Runs "runs/group.rs" "Group / sorted aggregate";
    RunsIncremental "runs.incremental" M3 Runs "runs/incremental.rs" "IncrementalSort";
    RunsMergeappend "runs.mergeappend" M3 Runs "runs/mergeappend.rs" "MergeAppend";
    RunsGroupDistinct "runs.group_distinct" M6 Runs "runs/group_distinct.rs" "a streamed sorted group's DISTINCT aggregate (a per-group distinct set)";
    RepWordDatum "rep.word.datum" M1 Rep "rep/word/datum.rs" "Word datum";
    RepWordHash "rep.word.hash" M1 Rep "rep/word/hash.rs" "Word key images for hash / eq";
    RepWordOrder "rep.word.order" M3 Rep "rep/word/order.rs" "Word order images (DESC / NULLS folded)";
    RepCodeDatum "rep.code.datum" M1 Rep "rep/code/datum.rs" "Code render / per-code derived datum";
    RepCodeHash "rep.code.hash" M1 Rep "rep/code/hash.rs" "Code hash as rendered (fp128), same-dictionary eq";
    RepCodeOrder "rep.code.order" M1 Rep "rep/code/order.rs" "Code order: in-part rank, cross-part render order";
    RepPackedDatum "rep.packed.datum" M1 Rep "rep/packednum/datum.rs" "PackedNum datum";
    RepPackedHash "rep.packed.hash" M6 Rep "rep/packednum/hash.rs" "numeric key images, common scale";
    RepPackedOrder "rep.packed.order" M6 Rep "rep/packednum/order.rs" "numeric order images";
    RepVarlenaDatum "rep.varlena.datum" M1 Rep "rep/varlena/datum.rs" "VarlenaPtr datum";
    RepVarlenaHash "rep.varlena.hash" M6 Rep "rep/varlena/hash.rs" "text / bpchar / other varlena key images";
    RepVarlenaOrder "rep.varlena.order" M6 Rep "rep/varlena/order.rs" "varlena order images incl. collation sort keys";
    RepFixedDatum "rep.fixed.datum" M1 Rep "rep/fixed/datum.rs" "FixedPtr datum";
    RepFixedHash "rep.fixed.hash" M6 Rep "rep/fixed/hash.rs" "uuid / interval / name key images";
    RepFixedOrder "rep.fixed.order" M6 Rep "rep/fixed/order.rs" "uuid / interval / name order images";
    RepHostedHash "rep.hosted.hash" M6 Rep "rep/hosted/hash.rs" "types with no native image: fmgr hash + eq (arrays, records, ranges, user types)";
    RepHostedOrder "rep.hosted.order" M6 Rep "rep/hosted/order.rs" "types with no native image: fmgr comparator + abbreviated key";
    RepHostedOrderFixed "rep.hosted.order.fixed" M6 Rep "rep/hosted/order.rs" "rep.hosted.order over a by-value or fixed by-reference key (enum, name, macaddr, point, tid): no var copy to compare";
    JoinInner "join.inner" M1 Join "join/probe_inner.rs" "inner hash probe (+ join/build.rs); one probe per pipeline";
    JoinLeft "join.left" M7 Join "join/probe_left.rs" "LEFT probe with the null row";
    JoinSemi "join.semi" M7 Join "join/probe_semi.rs" "SEMI probe (stop at first match)";
    JoinAnti "join.anti" M7 Join "join/probe_anti.rs" "ANTI probe";
    JoinRight "join.right" M7 Join "join/probe_right.rs" "RIGHT / RIGHT SEMI / RIGHT ANTI marks";
    JoinFull "join.full" M7 Join "join/probe_full.rs" "FULL = left fill + right drain";
    JoinDrain "join.drain" M7 Join "join/drain.rs" "unmatched-row drain of a build";
    JoinChain "join.chain" M7 Join "join/chain.rs" "several probes and nests on one outer pipeline";
    SpillBreakers "spill.breakers" M7 Spill "keyed/spill.rs" "spill exits for hashagg, sort runs and builds (MEMORY S5)";
    NestLoop "nest.loop" M8 Nest "nest/loop.rs" "NestLoop inner / left / semi / anti";
    NestSpool "nest.spool" M8 Nest "nest/spool.rs" "Material spool walked in IR";
    NestParam "nest.param" M8 Nest "nest/param.rs" "NestLoopParam: index inner driven per outer row";
    NestMemo "nest.memo" M8 Nest "nest/memo.rs" "Memoize with the hit path in IR";
    NestSubplan "nest.subplan" M8 Nest "nest/subplan.rs" "SubPlan: scalar / EXISTS / ANY / ALL / ARRAY, re-run by params";
    NestInitplan "nest.initplan" M8 Nest "nest/initplan.rs" "InitPlan, lazy at first read (D237)";
    NestRerun "nest.rerun" M8 Nest "nest/rerun.rs" "rerun ranges over param words";
    NestCte "nest.cte" M8 Nest "nest/cte.rs" "CteScan over a shared lazy spool";
    NestCatalog "nest.catalog" M3 Nest "nest/catalog_left.rs" "psql lookup nest as a catalog probe: the pg_namespace inner built once, INNER / LEFT, NL-exact (V4R-142)";
    SrfFunction "srf.function" M5 Srf "srf/function_scan.rs" "FunctionScan over library SRFs (generate_series, unnest, json each ...)";
    SrfPl "srf.pl" M5 Srf "srf/pl_function.rs" "FunctionScan over PL / SQL functions: SPI re-entrancy (ENTRY), tuplestore life";
    SrfProjectset "srf.projectset" M5 Srf "srf/project_set.rs" "ProjectSet (target-list SRFs, lockstep)";
    SrfTablefunc "srf.tablefunc" M5 Srf "srf/table_func.rs" "XMLTABLE / JSON_TABLE";
    SetopSetop "setop.setop" M9 Setop "setop/setop.rs" "hashed / sorted INTERSECT / EXCEPT [ALL]";
    SetopRecursive "setop.recursive" M9 Setop "setop/recursive.rs" "RecursiveUnion + WorkTableScan";
    WindowAgg "window.agg" M10 Window "window/window.rs" "WindowAgg: frame classes, rank family, moving frames";
    MergeJoin "merge.join" M11 Merge "merge/mergejoin.rs" "MergeJoin, all seven join types";
    WriteDelete "write.delete" M1 RowOp "write/delete.rs" "DELETE row op (+ write/rowop.rs, the row-op end)";
    WriteInsert "write.insert" M4 RowOp "write/insert.rs" "INSERT (VALUES, SELECT)";
    WriteUpdate "write.update" M4 RowOp "write/update.rs" "UPDATE (old row = the scan's row)";
    WriteMerge "write.merge" M4 RowOp "write/merge.rs" "MERGE, incl. into a view";
    WriteReturning "write.returning" M4 RowOp "write/returning.rs" "RETURNING in IR";
    WriteOnconflict "write.onconflict" M4 RowOp "write/onconflict.rs" "ON CONFLICT DO UPDATE / NOTHING";
    WriteLock "write.lock" M4 RowOp "write/lockrows.rs" "LockRows with mode x wait policy";
    WriteTransition "write.transition" M4 RowOp "write/triggers.rs" "transition tables and trigger firing";
}

impl KindId {
    pub const COUNT: usize = KINDS.len();

    pub const fn decl(self) -> &'static KindDecl {
        &KINDS[self as usize]
    }

    pub const fn name(self) -> &'static str {
        self.decl().name
    }

    pub fn from_name(s: &str) -> Option<KindId> {
        KINDS.iter().find(|d| d.name == s).map(|d| d.id)
    }

    pub fn all() -> impl Iterator<Item = KindId> {
        KINDS.iter().map(|d| d.id)
    }
}

/// A set of kinds (the kinds a graph needs): one bit per `KindId`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct KindSet(pub u128);

impl KindSet {
    pub const EMPTY: KindSet = KindSet(0);

    pub const fn of(ks: &[KindId]) -> KindSet {
        let mut bits = 0u128;
        let mut i = 0;
        while i < ks.len() {
            bits |= 1u128 << ks[i] as u32;
            i += 1;
        }
        KindSet(bits)
    }

    pub const fn has(self, k: KindId) -> bool {
        self.0 & (1u128 << k as u32) != 0
    }

    pub const fn any(self, of: KindSet) -> bool {
        self.0 & of.0 != 0
    }

    pub fn add(&mut self, k: KindId) {
        self.0 |= 1u128 << k as u32;
    }

    pub fn iter(self) -> impl Iterator<Item = KindId> {
        KINDS.iter().map(|d| d.id).filter(move |k| self.has(*k))
    }

    pub const fn len(self) -> u32 {
        self.0.count_ones()
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

const _: () = assert!(KINDS.len() == 104 && KINDS.len() <= 128);
