//! Every lowerer-v4 program descriptor (docs/gravity/lowerv4/plan.md §2.2, §4.3): what a v4 program declares to the runner,
//! the driver and the hosts beyond program.rs, carried by `Program::v4` (`with_v4`, folded into the content key). Complete at
//! M0 and closed there: a later milestone emits a declared kind, it never adds one, but for a kind a ruling adds, each named
//! by its decision (expr.volatile D439, agg.arena V4R-269, keys.user D4197, runs.emit_runs D3024 (b)). A kind whose
//! milestone has not landed is never emitted, because the lowerer refuses its kind or road first (`NotBuilt`); the runner
//! names one it cannot run.
use core::hash::{Hash, Hasher};
use std::sync::Arc;

use allocator_api2::alloc::{Allocator, Global};
use allocator_api2::vec::Vec as AVec;

use crate::arena::{Arena, ArenaRef};
use crate::program::round8;

use crate::program::{ContentKey, Program};

/// `[base, base + n)` of the binding's parameter words: the words a protocol writes or reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Words {
    pub base: u16,
    pub n: u16,
}

/// A leader-side reader over a finished region (a breaker slot's table or run): `(region, words, owner, emit)` calls
/// `emit(a, b, c)` per item (an exported key, a granule mark `(key, part, bits)`, an unmatched entry …) and returns the items
/// read, `None` when the region's layout is not the reader's. CONVERSION WAVE 1: `owner` is the ledger owner of the slot the
/// region is (`Node::slot_owner`; `None` with none, or a test's stand-in): a reader's own working memory is charged to it.
pub type RegionFn = unsafe fn(*const u8, &[u64], Option<memledger::Owner>, &mut dyn FnMut(u64, u64, u64)) -> Option<usize>;

/// A bind fact the runner writes into a word before the section (`ProtoKind::Facts`), in this order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Fact {
    TopK = 0,
    DenseLo = 1,
    DenseHi = 2,
    SpillBits = 3,
    Order = 4,
    TableSize = 5,
    RadixBits = 6,
    Width = 7,
    Rows = 8,
}

impl Fact {
    pub const COUNT: usize = 9;
    pub const ALL: [Fact; Fact::COUNT] = [Fact::TopK, Fact::DenseLo, Fact::DenseHi, Fact::SpillBits, Fact::Order, Fact::TableSize, Fact::RadixBits, Fact::Width, Fact::Rows];
}

macro_rules! kinds {
    ($($(#[$doc:meta])* $k:ident = $m:literal, $name:literal;)*) => {
        /// Every runtime protocol a v4 program may declare, with the milestone whose `exec/src/v4/m<k>.rs` runs it.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum ProtoKind {
            $($(#[$doc])* $k,)*
        }

        impl ProtoKind {
            pub const ALL: &'static [ProtoKind] = &[$(ProtoKind::$k),*];

            /// The milestone that builds the kind (`M<k>` as `k`).
            pub const fn milestone(self) -> u8 {
                match self {
                    $(ProtoKind::$k => $m,)*
                }
            }

            pub const fn name(self) -> &'static str {
                match self {
                    $(ProtoKind::$k => $name,)*
                }
            }
        }
    };
}

kinds! {
    /// keyed.twopass / twopass.having (H08): pass 1 (`peer`) exports its keys through `read[0]` over `words`; the admitted
    /// set is laid out before pass 2 (`pipeline`) folds only admitted groups.
    TwoPass = 1, "twopass";
    /// sink.ordered (H05a): the pipeline's per-worker out blocks are emitted in claim order.
    OrderedSink = 1, "sink.ordered";
    /// width.work / width.write (H15a/b): the pipeline's width from exact bind facts (`arg` = the work estimate in rows;
    /// bit 63 = leader-only, a write or a lock).
    Width = 1, "width";
    /// The bind facts (H13b table.sized, H13c table.radix, the dense domain, spill bits, top-k bound): bit `f` of `arg` set
    /// writes `Fact` `f` into the next word from `words.base`, in `Fact` order.
    Facts = 1, "facts";
    /// join.rtfilter (H07c): after the build `peer` finishes, its key set / range / bloom (`read[0]`) arms site `site`'s
    /// verdict for the probing `pipeline` (`arg` = the filter form).
    RtFilter = 1, "join.rtfilter";
    /// join.buildside (H18c): at prepare, `pipeline` or `peer` builds, whichever exact bind facts say is smaller.
    BuildSide = 1, "join.buildside";
    /// code.render / code.carry (H10): the leader renders the code survivors of slot `slot` through site `site`'s
    /// dictionary (`arg` = the code columns, a byte per lane) and crosses parts by value.
    Render = 1, "code.render";
    /// runs.topk_groups: the final-ordered owner top of slot `slot` is merged at the leader (`arg` = n).
    TopGroups = 1, "runs.topk_groups";
    /// table.sized from observation: a finished table's (records, groups) remembered under the plan key.
    Observe = 1, "observe";
    /// src.verdicts (H02 / H03b, V4R-4): scan site `site`'s host verdicts before its first staged granule, told apart by
    /// `slot`. Slot 0: `arg` the enables (`VERDICT_*` bits; a road's forced-off twin clears its bit), `words` the covered
    /// quals' bound constant words and `peer` the mask of those that are varlena pointers (a value, not a pipeline: the
    /// program order map keeps it, D2841). Slots 1 / 2 (only with
    /// `VERDICT_COND`): `arg` the qual shape's fingerprint low / high word; the host finishes the condition fingerprint
    /// per execution over the bound words, so the key follows the literals (a proto never carries one: the front key
    /// does not hash protos).
    Verdicts = 1, "src.verdicts";
    /// append.arms (H05): the Append's arms run parallel, each answered on its own (`arg` = the arm count).
    Arms = 2, "append.arms";
    /// src.prune: exec-time partition pruning at the first open (after the InitPlans, before the claims).
    Prune = 2, "src.prune";
    /// topk.walk (H07b/d): site `site`'s claims walk the zone order of attribute `arg & 0xffff` (bit 16 = a group walk), the
    /// section stopping at the bound in `words`.
    Walk = 3, "topk.walk";
    /// topk.fetch (H28): the winners' locators in slot `slot` are fetched once through site `site`.
    Fetch = 3, "topk.fetch";
    /// runs.sort / sort.resident (H26b): the leader merges slot `slot`'s worker runs in place; a fixed row's `arg` is its
    /// spec and `peer` its key count (a value, not a pipeline: the program order map keeps it, D2841).
    SortMerge = 3, "runs.sort";
    /// owner.count (H12): the count-ordered owner top of slot `slot` with its threshold gate (`arg` = n).
    OwnerTop = 3, "owner.count";
    /// write.route (H16b/H27): rows route on key lanes to the bound leaves.
    Route = 4, "write.route";
    /// write.literals (H16a): the per-execution literal table in `words`; `arg` = the ValuesScan's plan node (bits 0..32),
    /// its width (32..48) and the `literals_mark` of its coded cells (48..64).
    Literals = 4, "write.literals";
    /// write.transition: transition tables filled and statement triggers fired after the row op.
    Transition = 4, "write.transition";
    /// srf.* (H05, H32): a set function's tuplestore life, parallel set pipelines, the LIMIT stop reaching the set loop.
    SetFn = 5, "srf";
    /// code.cut (H10): the per-part code cut of slot `slot` before its render.
    CodeCut = 6, "code.cut";
    /// code.heads (H11): site `site`'s dictionary entries are the pipeline's claims.
    Heads = 6, "code.heads";
    /// join.drain / drain.parts (H25): the build in slot `slot`'s unmatched rows drained partition-parallel (`read[0]`).
    Drain = 7, "join.drain";
    /// join.right / marks.testset: slot `slot`'s match marks, tested before they are set.
    RightMarks = 7, "join.right";
    /// semijoin.marks (H08): the finished build `peer`'s key set (`read[0]`) cuts `pipeline`'s claims over site `site`.
    SemiMarks = 7, "semijoin.marks";
    /// join.emit_build (H18b): the keyed fold `peer`'s output in slot `slot` is handed over as `pipeline`'s build.
    HandOver = 7, "join.emit_build";
    /// spill.breakers (MEMORY S5): slot `slot`'s spill partitions become `pipeline`'s claims once `peer` ran.
    Spill = 7, "spill.breakers";
    /// nest.spool / nest.selfspool (H19): the invariant inner `peer` spooled once into slot `slot`.
    Spool = 8, "nest.spool";
    /// runs.gsets_spool (D3054): `pipeline` reads the owned spool its fill `peer` sealed (the Spool proto's handle word
    /// `words.base`) by record ranges: its claims, sized after `peer` ran.
    SpoolChunks = 8, "nest.spool.chunks";
    /// nest.rerun / nest.subplan: `pipeline` re-runs per change of the param words `words` (marked so producers skip it).
    Rerun = 8, "nest.rerun";
    /// nest.cache / nest.memo (H30): `pipeline`'s results cached by the param words `words`.
    Cache = 8, "nest.cache";
    /// nest.initplan (H07e): the InitPlan `pipeline` runs at its first read (D237) into word `words.base`.
    Init = 8, "nest.initplan";
    /// nest.parallel (H05b): a parallel outer with per-worker inner state.
    ParallelOuter = 8, "nest.parallel";
    /// nest.cte: the shared lazy spool in slot `slot`.
    Cte = 8, "nest.cte";
    /// nest.subplan, hashed (V4R-365): the key set `pipeline` (the build) fills into handle word `words.base`; `arg` = ncols | width << 8
    /// (| `SUBHASH_LAZY`, V4R-369: the build runs at the first probe).
    SubHash = 8, "nest.subplan";
    /// nest.subplan.epq (D3506 SETLOCK): the set a lock's EvalPlanQual redo probes, in handle word `words.base`, beside
    /// the SubHash set of the same build `pipeline` (layout `arg`): its LAZY build runs that set's members again into
    /// this one, the lock hosts live.
    SubHashEpq = 8, "nest.subplan.epq";
    /// nest.subplan (V4R-365): a SubPlan's per-call result object (a by-reference EXPR's copy) in handle word `words.base`.
    SubResult = 8, "nest.subplan.result";
    /// nest.subplan (D1478): site `site`, or with `site` -1 pipeline `pipeline`'s driving site and its producers', scans a
    /// relation of a SubPlan's own plan, which no PlanRowMark names: an EvalPlanQual recheck runs it again under the
    /// statement's snapshot, never at a substituted row (exec epq.rs; an InitPlan's and a hashed set's pipelines alike).
    Rescan = 8, "nest.subplan.rescan";
    /// nest.prune (D2013): a nest inner union's run-time pruning object (exec ops/append/prune.rs `NestPrune`) in handle
    /// word `words.base`, decided at the union's first read after each re-open (`m8_prune`); `arg` = src.prune's arg.
    NestPrune = 8, "nest.prune";
    /// setop.recursive: the generation loop over the work table in slot `slot`.
    Generation = 9, "setop.recursive";
    /// setop.setop / setop.count: the INTERSECT / EXCEPT count table in slot `slot`.
    SetOp = 9, "setop.setop";
    /// win.parttable (H13 / H23c): the partition table of slot `slot` for whole-partition frames.
    PartTable = 10, "win.parttable";
    /// win.ownedview (H08): the second pass probes slot `slot`'s owned-result view.
    OwnedView = 10, "win.ownedview";
    /// win.partruns (H26c): partition-grouped runs, one partition per worker.
    PartRuns = 10, "win.partruns";
    /// window.agg (M10): WindowAgg `arg`'s window object (hosts v4/m10.rs): its input rows as the Materialize end appends
    /// them, then per row nodeWindowAgg.c's positions, window functions and aggregates over them; its handle in `words`.
    Window = 10, "window.agg";
    /// merge.markable (H19): the sorted side in slot `slot` read through a markable cursor; with `MERGE_PULLED` (jmerge-0012)
    /// a merge join's inner spool `pipeline`, pulled one row per fetch, its object's handle in `words`.
    Markable = 11, "merge.markable";
    /// merge.ksort (H19 / H26b): a parallel sorted side merged k-way, markable.
    KWay = 11, "merge.ksort";
    /// merge.cosplit (D3190): merge join `pipeline`'s claims are key ranges of its two sorted inputs, cut once both ran: the
    /// outer's run in slot `slot`, the inner's in the slot `arg` names (`cosplit_arg`), cut on their first merge keys; the
    /// ranges' table in handle word `words.base` (`COSPLIT_PART_WORDS` words a part).
    CoSplit = 11, "merge.cosplit";
    /// expr.volatile (M2 L2, D439): the program evaluates a VOLATILE call per row, so a pull (driver pull.rs: a cursor's
    /// FETCH, a portal's Execute with max_rows) asks each call for no more rows than it still owes: none evaluated ahead.
    Volatile = 2, "expr.volatile";
    /// agg.arena (M6 H3, V4R-269): the hosted aggregate calls' arena (exec v4/m6.rs; `arg` = the Agg's plan_node_id),
    /// made and freed on the leader: a program that declares it runs width 1.
    AggArena = 6, "agg.arena";
    /// join.follows (M7-DRAIN, V4R-377): `pipeline` (a drain) opens only after `peer` (its probe) ran
    /// to its end, its section joined, so every worker's mark stores are visible to it. An order edge, never a product:
    /// `Program::producers_of` yields `peer`, so every reader of the producer order sees it (the DAG barrier, the
    /// on-demand member sets, the lazy runs' feeders). `peer` < `pipeline`; neither is a lazy build (exec v4/m7.rs).
    Follows = 7, "join.follows";
    /// twopass.floor (D3157): the bare pass 1 `pipeline`'s claims share one floor of the top's `arg` best keys (kernels
    /// fold_entries.rs `PASS1_SHARED_*`), laid zeroed per execution behind handle word `words.base`: a claim's end appends
    /// again only its floor keys that entered it.
    TwoPassFloor = 1, "twopass.floor";
    /// keys.user (D4197): the run's user-key classes (kernels typesem/userfp.rs `RunKeys`: one table per spec, made at its
    /// first value), laid per execution behind handle word `words.base` and dropped with the execution. Every user key's
    /// host call takes the handle (`m6_user_fp`, `m6_user_class`, the per-row collisions', a hashed IN list's set), so no
    /// class or representative outlives the run that made it. One per program, declared once its functions call one.
    UserKeys = 6, "keys.user";
    /// runs.emit_runs (D3024 (b)): the run table of `pipeline`, a group emit that sorts its groups in place and runs wide
    /// (kernels table/emit.rs `RUNS_*`), laid zeroed per execution behind handle word `words.base`: each claim's top files
    /// its sorted run at the entry of the slice the claim started at; the top's drain merges them (`agg_merge_runs`).
    EmitRuns = 3, "runs.emit_runs";
}

/// `ProtoKind::Verdicts`'s enables (`Proto::arg`): the host verdicts a scan site takes.
pub const VERDICT_ZONE: u64 = 1;
pub const VERDICT_BLOOM: u64 = 2;
pub const VERDICT_PSMA: u64 = 4;
pub const VERDICT_ELIDE: u64 = 8;
pub const VERDICT_COND: u64 = 16;
pub const VERDICT_CLAIM: u64 = 32;
/// Road `heap.stream` (H03a, V4R-32) on a heap range site: the host's read-stream look-ahead of cold claims. Off = the
/// forced-off twin (`gravity_road_off=heap.stream`); a heap site's only verdict (the host applies PostgreSQL's rule).
pub const VERDICT_STREAM: u64 = 64;
/// V4R-137 (M3-LEVERS-A): road code.sparse (cards/M3.md L8), set with its table form (`VERDICT_CODE_SPARSE_TABLE`): a
/// granule whose code block's base the table rejects selects its non-base rows from the packed codes (hosts v4
/// m3_codes.rs), no whole decode. Off = the forced-off twin (`gravity_road_off=code.sparse`). Past M1's host bits (2048).
pub const VERDICT_CODE_SPARSE: u64 = 4096;

/// H08 / P5-04 / SRC-22: road code.sparse's table form, beside `VERDICT_CODE_SPARSE`: a leading qual whose whole verdict
/// is one per-code `Bits` table over a code lane (any code verdict: EQ, LIKE, a range), its site table named in bits
/// 32..48 of slot 0's `arg` (`code_sparse_table_arg`). A granule whose code block's base that table rejects (bit clear,
/// no fault) selects its non-base rows from the packed codes, at any zone count; the program still tests every qual.
pub const VERDICT_CODE_SPARSE_TABLE: u64 = 8192;

/// D1536, an EXPLAIN ANALYZE lowering's site whose scan runs to its end (lowerv4 ops/scan/pgrc/lower.rs `runs_whole`: no
/// LIMIT, top-k or walk, SubPlan, InitPlan, rerun set or Gate that could stop it inside a window): a counting execution
/// (hosts `GranuleRecipe::skipped`) keeps its row selection (row_select, the PSMA window) and adds the rows the selection
/// drops to the count. Without it a counting execution selects no row inside a granule, so the program sees every row and
/// its count stops where the scan stops.
pub const VERDICT_COUNT_SELECT: u64 = 1 << 15;

/// The enables word of `VERDICT_CODE_SPARSE_TABLE` naming site table `t`.
pub const fn code_sparse_table_arg(t: u16) -> u64 {
    VERDICT_CODE_SPARSE | VERDICT_CODE_SPARSE_TABLE | (t as u64) << 32
}

/// The site table `code_sparse_table_arg` names in an enables word, if any.
pub const fn code_sparse_table(enables: u64) -> Option<usize> {
    if enables & VERDICT_CODE_SPARSE_TABLE == 0 {
        return None;
    }
    Some((enables >> 32) as u16 as usize)
}

/// P5-03 / H06 (the code verdict): site table `t` in bits 32..48 of slot 0's `arg` (`code_table_arg`; the table form's
/// when both are on) answers a leading qual whole, a host-built per-code `Bits` table over a code lane (a part
/// dictionary's code set by binary search: an EQ or IN list, a byte range; a LIKE's sweep). The host skips a part whose
/// table rejects every code, and its row selection drops a row whose code the table rejects.
pub const VERDICT_CODE_TABLE: u64 = 16384;

/// The enables word of `VERDICT_CODE_TABLE` naming site table `t`.
pub const fn code_table_arg(t: u16) -> u64 {
    VERDICT_CODE_TABLE | (t as u64) << 32
}

/// The site table `code_table_arg` names in an enables word, if any.
pub const fn code_table(enables: u64) -> Option<usize> {
    if enables & VERDICT_CODE_TABLE == 0 {
        return None;
    }
    Some((enables >> 32) as u16 as usize)
}

// bits 32..48 are no other enables bit's (the flags are below 2^16, `const_key_arg`'s lane from bit 48), and a table
// index round-trips beside a constant key
const _: () = assert!(
    code_sparse_table_arg(0xffff) & (const_key_arg(0xffff) | (VERDICT_COUNT_SELECT << 1) - 1) == VERDICT_CODE_SPARSE | VERDICT_CODE_SPARSE_TABLE
        && matches!(code_sparse_table(code_sparse_table_arg(513) | const_key_arg(7)), Some(513))
        && code_sparse_table(VERDICT_CODE_SPARSE).is_none()
        && code_table_arg(0xffff) & (const_key_arg(0xffff) | (VERDICT_COUNT_SELECT << 1) - 1) == VERDICT_CODE_TABLE
        && matches!(code_table(code_table_arg(513) | code_sparse_table_arg(513) | const_key_arg(7)), Some(513))
        && code_table(code_sparse_table_arg(9)).is_none()
);

/// decode.late on a pgrc site: the site's late lanes wait for `lane_fill`. Slot 3's `arg` names them (bit k = site lane k); exec's prepare hands the mask (and
/// slot 4's) to the site's verdict cell.
pub const VERDICT_LATE: u64 = 64;

/// decode.packed on a pgrc site: slot 4's `arg` names the candidate lanes. A window with no selection and no deletes
/// whose every candidate is a by-value integer lane stored BYTE_FOR at delta width 1 or 2 (or Const: width 0) is packed:
/// its candidates wait for `lane_fill` as late lanes do (validity exact at the window) and each has a `PackedFrames`. Any
/// other window decodes them all as before.
pub const VERDICT_PACKED: u64 = 128;

/// decode.unread on a pgrc site: the site's end counts a covered window whole (lowerv4 fold/answer.rs `count_covered`: every
/// fold `count(*)` under no FILTER, every filter op covered), so on a condition-cache hit the host decodes no lane and
/// publishes the window plain, its rows the survivors' count (the words' popcount less the deleted rows).
pub const VERDICT_UNREAD: u64 = 256;

/// decode.packed's host half: every zone predicate's lane is a packed candidate that only the pipeline's
/// first op reads (its packed test), so a window no PSMA window narrows packs before row_select and never fills those
/// lanes for a block's survivors. Without it (a zone lane a later op or the end reads) the zone lanes decode first.
pub const VERDICT_PACKED_ZONE: u64 = 512;

/// verdict.elide under the condition cache's cover, beside `VERDICT_ELIDE`: the site's filter lanes
/// are read by covered ops the zone does not decide (a numeric or other non-zone qual), so the host leaves them
/// undecoded only in a window a condition-cache hit decided (All or Mixed: the frame skips its covered steps there). A
/// zone `AllMatch` window or part decodes them, answers nothing, and publishes its miss's words as a Mixed granule's.
pub const VERDICT_ELIDE_HIT: u64 = 1024;

/// The constant-key window: the site's keyed end groups on one signed by-value word key lane,
/// named in bits 48.. of slot 0's `arg` (`const_key_arg`). A window whose key lane holds one non-NULL key on every row (its
/// part's exact Stats record, else its granule's: min == max, nonnull == rows; no delete mask) publishes that lane's `aux`
/// = 1 (a by-value lane's is 0 otherwise) and the key at the lane's element 0 (its width), even when the lane is late.
pub const VERDICT_CONST_KEY: u64 = 2048;

/// P1c (decode.late in a selection's window): every late lane of the site is filled only where its path lanes are
/// invalid (lowerv4 lower.rs `first_only`: no `lane_fill` of the program's reads it), so a window whose row selection
/// is selective keeps its late guarded jsonb columns late and fills their selected rows' invalid ones alone (granule
/// source `guard_fill`); without it such a window decodes every late lane for its selection.
pub const VERDICT_GUARD_SEL: u64 = 1 << 16;

/// sortpath (code-keyed zones): the zone covers every op the condition cache covers (lowerv4 lower.rs `zone_covers`, with
/// the cache on), so a zone `AllMatch` window passes every covered qual on every row: the host publishes it to the cache
/// as `AllMatch` (the frame elides the covered steps there and computes no words). Without it a zone `AllMatch` window's
/// frame tests every covered op, and the host publishes its words as a Mixed window's.
pub const VERDICT_ZONE_COVERS: u64 = 1 << 17;

/// D3042, count.only on a pgrc site: the site's end is a plain fold of `count(*)`s over projections alone (lowerv4 fold
/// answer.rs `count_only`: every window counted `end - cursor`, no lane read) and the site has no predicate, row selection
/// or dictionary lane, so the host takes a part (a claim holding it whole) or else a granule as ONE window of its live rows
/// (the manifest's rows less its delete vector's) and decodes, prefetches and reads no lane. Without it the count reads
/// the site's driving lane whole.
pub const VERDICT_COUNT_ONLY: u64 = 1 << 18;

/// The enables word of `VERDICT_CONST_KEY` naming key lane `lane`.
pub const fn const_key_arg(lane: u16) -> u64 {
    VERDICT_CONST_KEY | (lane as u64) << 48
}

/// The key lane `const_key_arg` names in an enables word.
pub const fn const_key_lane(enables: u64) -> usize {
    (enables >> 48) as u16 as usize
}

/// D289, a generic plan's comparand: `src.verdicts` slots `VERDICT_WORD_ZONE..` each declare a zone predicate `lane op $n`
/// over a key word, `arg` = `word_zone_arg(lane, op, len)` (`op` the `ZoneOp`'s ordinal, `len` the comparand's typlen: 1 /
/// 2 / 4 / 8) and `words` = (the word, 1). Exec's prepare resolves each over the execution's words into the site's verdict
/// cell (a NULL word: none), and the host adds them after the static predicates; none without the protocol.
pub const VERDICT_WORD_ZONE: u8 = 5;

/// The `arg` of a word-valued zone predicate over `lane` (`op` the `ZoneOp`'s ordinal, `len` the comparand's typlen).
pub const fn word_zone_arg(lane: u16, op: u8, len: u8) -> u64 {
    lane as u64 | (op as u64) << 16 | (len as u64) << 24
}

/// `(lane, op, len)` of a word-valued zone predicate's `arg`.
pub const fn word_zone_of(arg: u64) -> (u16, u8, u8) {
    (arg as u16, (arg >> 16) as u8, (arg >> 24) as u8)
}

/// PS1-05 (road `nest.eagersafe`): a `VERDICT_WORD_ZONE..` slot whose comparand is no bind word but site key cell `cell`,
/// which the source's open fills with an eager-safe InitPlan's value (its last `SiteKind::RangeSource::zkeys` cells); the
/// host reads it at its seek. `words` stays empty.
pub const WORD_ZONE_KEY: u64 = 1 << 63;

/// The `arg` of an open-time zone predicate over `lane` (`word_zone_arg`'s fields) whose comparand is key cell `cell`.
pub const fn word_zone_key_arg(lane: u16, op: u8, len: u8, cell: u16) -> u64 {
    word_zone_arg(lane, op, len) | (cell as u64) << 32 | WORD_ZONE_KEY
}

/// The key cell of an open-time zone predicate's `arg` (`word_zone_key_arg`), `None` for a bind word's.
pub const fn word_zone_key_of(arg: u64) -> Option<u16> {
    if arg & (WORD_ZONE_KEY | WORD_ZONE_AT | WORD_ZONE_LIST) == WORD_ZONE_KEY { Some((arg >> 32) as u16) } else { None }
}

/// NEW-72 (literal-free zone constants: a literal is never baked into a program): a `VERDICT_WORD_ZONE..` slot that binds static predicate
/// `at` of the site (`ZonePred::bound`: its constant a literal's word, `words` = (the word, 1)) instead of adding one. The
/// host's bind writes the execution's value into that predicate; with no value for it the site takes no zone verdict.
pub const WORD_ZONE_AT: u64 = 1 << 62;

/// The `arg` binding static predicate `at` (`word_zone_arg`'s fields).
pub const fn word_zone_at_arg(lane: u16, op: u8, len: u8, at: u16) -> u64 {
    word_zone_arg(lane, op, len) | (at as u64) << 32 | WORD_ZONE_AT
}

/// The static predicate a binding slot's `arg` names (`word_zone_at_arg`), `None` for any other slot's.
pub const fn word_zone_at_of(arg: u64) -> Option<u16> {
    if arg & WORD_ZONE_AT != 0 { Some((arg >> 32) as u16) } else { None }
}

/// NEW-72: a `VERDICT_WORD_ZONE..` slot whose word is an IN list's array literal (`lane = ANY (array)`, a by-value lane):
/// `arg` = `WORD_ZONE_LIST` | the element's facts word (gravity_expr `ElemFacts`, bits 0..57), `peer` = the lane. Exec's
/// prepare walks the execution's array: its non-NULL members' span (`lane >= min`, `lane <= max`, inexact) and the
/// members as the lane's `InMember` predicates (the bloom's), added after the static predicates; none for a NULL array or
/// one of NULLs alone.
pub const WORD_ZONE_LIST: u64 = 1 << 61;

/// NEW-72: a list slot whose members are the lane's probe words (`WORD_ZONE_LIST` | this): an IN list over a lane whose
/// equality is byte equality (a text lane under a deterministic collation, a memcmp-ordered fixed image: P5-11 / P5-13).
/// Exec's prepare adds each non-NULL member's probe word (the writer's hash of its payload or image) as the lane's
/// `HashIn` predicate, and no span: only the bloom answers them.
pub const WORD_ZONE_LIST_HASH: u64 = 1 << 60;

/// sortpath (4): a list slot whose members are a jsonb path code lane's prefix keys (`WORD_ZONE_LIST` | this): an IN
/// list of text over a path lane (lowerv4 verdict_zone.rs `prefix_list_of`). Exec's prepare adds the non-NULL members'
/// span as two prefix predicates (`lane >= key(min)`, `lane <= key(max)`, inexact; the host answers them from the
/// lane's Stats records by strict comparisons), and no members: a path lane has no bloom.
pub const WORD_ZONE_LIST_PREFIX: u64 = 1 << 59;

/// Whether a list slot's members are prefix keys (`WORD_ZONE_LIST_PREFIX`).
pub const fn word_zone_list_prefix(arg: u64) -> bool {
    arg & (WORD_ZONE_LIST | WORD_ZONE_LIST_PREFIX) == WORD_ZONE_LIST | WORD_ZONE_LIST_PREFIX
}

/// The element facts word's bits in a list slot's `arg` (`ElemFacts::word` stays below them).
const LIST_FACTS: u64 = (1 << 58) - 1;

/// The `arg` of a list slot over elements of facts word `facts` (`WORD_ZONE_LIST`).
pub const fn word_zone_list_arg(facts: u64) -> u64 {
    facts & LIST_FACTS | WORD_ZONE_LIST
}

/// The element facts word of a list slot's `arg` (`WORD_ZONE_LIST`), `None` for any other slot's.
pub const fn word_zone_list_of(arg: u64) -> Option<u64> {
    if arg & WORD_ZONE_LIST != 0 { Some(arg & LIST_FACTS) } else { None }
}

/// Whether a list slot's members are probe words (`WORD_ZONE_LIST_HASH`).
pub const fn word_zone_list_hash(arg: u64) -> bool {
    arg & (WORD_ZONE_LIST | WORD_ZONE_LIST_HASH) == WORD_ZONE_LIST | WORD_ZONE_LIST_HASH
}

/// `ProtoKind::Volatile`'s `arg` on a nest inner's FROM function store filled once and rewound per re-open (D1085 retired,
/// lowerv4 srf/function_store.rs); 0 on every other. The entry delegates a nested statement holding it (D2044).
pub const VOLATILE_ONCE_STORE: u64 = 1;

/// V4R-88 (M2-HEAP): a heap row's system columns as attribute lanes past every attribute, numbered `SYS_AT_BASE - attno`
/// from PostgreSQL's system attnos: `ctid` (-1) 1602 (lowerv4's `CTID_AT`, never a host lane), `xmin` (-2) 1603, `cmin`
/// (-3) 1604, `xmax` (-4) 1605, `cmax` (-5) 1606. The heap host publishes xmin / cmin / xmax / cmax per row from the tuple
/// header (C `heap_getsysattr`): by-value 8-byte words, never null. `tableoid` (-6) stays a per-relation constant.
pub const SYS_AT_BASE: i16 = 1601;
pub const XMIN_AT: i16 = SYS_AT_BASE + 2;
pub const CMIN_AT: i16 = SYS_AT_BASE + 3;
pub const XMAX_AT: i16 = SYS_AT_BASE + 4;
pub const CMAX_AT: i16 = SYS_AT_BASE + 5;

/// A system column's lane attno (`xmin` .. `cmax`), or `None` for any other attno.
pub const fn sys_at(attno: i16) -> Option<i16> {
    match attno {
        -5..=-2 => Some(SYS_AT_BASE - attno),
        _ => None,
    }
}

/// write.literals' coded-cell mark (`Proto::arg` bits 48..64): a fold of the (row, column) of each cell of a VALUES list
/// that is not a Const, 0 for a list of Consts only. The program compiles those cells and reads every other from the
/// table; exec admits the table only over a list whose mark is the program's.
/// write.literals: the bit of a row's first null byte that marks a row holding a cell that is not a Const (the program's
/// code answers that row; its null bytes are 0 or 1 otherwise).
pub const LITERALS_CODED: u8 = 2;

/// write.literals' stream form (CAP-29): a list whose rows holding an expression pass `cost::VALUES_CODED_ROWS_MAX` is
/// declared with one more word, which points at this hook (exec ops/modifytable/raw.rs `Stream`, alive while the execution
/// runs). The VALUES source's host entry (lowerv4 literals/raw.rs `stream_row`, the boundary) calls `eval(cx, j)` at coded
/// row `j`'s turn: PostgreSQL's ValuesNext over the row (its expressions initialized and evaluated in a per-row context,
/// `$n` from the execution's list), each cell that is not a Const written to the table's row `j`, valid until the next
/// such call. 1 done, 0 an error, stashed for the raise. D3374 (result_cap29): a list whose streamed cells read executor
/// parameters (a lateral outer row's, a nest's, an InitPlan's; `values::exec_params`, ascending paramid) gets their
/// values from the program: before the call it writes parameter `i`'s datum at `params + 16 * i` and its null flag (1
/// NULL) at `params + 16 * i + 8`, which the evaluation reads as `es_param_exec_vals` (no InitPlan run of its own).
#[repr(C)]
pub struct LiteralsStream {
    pub eval: unsafe fn(u64, u64) -> i64,
    pub cx: u64,
    /// The parameter slots' address (two words per parameter), 0 for a list that reads none.
    pub params: u64,
}

/// `LiteralsStream::params`' byte offset in the hook (the program loads the slots' address through the hook word).
pub const LITERALS_STREAM_PARAMS: i32 = 16;
const _: () = assert!(core::mem::offset_of!(LiteralsStream, params) == LITERALS_STREAM_PARAMS as usize);

pub fn literals_mark(coded: impl IntoIterator<Item = (usize, usize)>) -> u16 {
    let (mut h, mut any) = (0xcbf2_9ce4_8422_2325u64, false);
    for (r, c) in coded {
        any = true;
        for w in [r as u64, c as u64] {
            h = (h ^ w).wrapping_mul(0x0100_0000_01b3);
        }
    }
    if !any {
        return 0;
    }
    ((h ^ h >> 16 ^ h >> 32 ^ h >> 48) as u16).max(1)
}

pub const NONE: u16 = u16::MAX;
pub const NO_SLOT: u8 = u8::MAX;

/// One protocol a program declares on one pipeline; the field meanings per kind are `ProtoKind`'s.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Proto {
    pub kind: ProtoKind,
    pub pipeline: u16,
    /// The pipeline it ties to (pass 1, the build, the spool's producer); `NONE` without one.
    pub peer: u16,
    /// The breaker slot it reads or hands over; `NO_SLOT` without one.
    pub slot: u8,
    /// The site it reads or cuts; -1 without one.
    pub site: i16,
    pub words: Words,
    pub arg: u64,
    pub read: [Option<RegionFn>; 2],
}

impl Proto {
    pub const fn new(kind: ProtoKind, pipeline: u16) -> Proto {
        Proto { kind, pipeline, peer: NONE, slot: NO_SLOT, site: -1, words: Words { base: 0, n: 0 }, arg: 0, read: [None, None] }
    }

    pub const fn peer(mut self, p: u16) -> Proto {
        self.peer = p;
        self
    }

    pub const fn slot(mut self, s: u8) -> Proto {
        self.slot = s;
        self
    }

    pub const fn site(mut self, s: i16) -> Proto {
        self.site = s;
        self
    }

    pub const fn words(mut self, base: u16, n: u16) -> Proto {
        self.words = Words { base, n };
        self
    }

    pub const fn arg(mut self, a: u64) -> Proto {
        self.arg = a;
        self
    }

    pub const fn reading(mut self, i: usize, f: RegionFn) -> Proto {
        self.read[i] = Some(f);
        self
    }

    /// D2674: a `BUILDSIDE_EPQ_FIRST` BuildSide proto's frame word (`BUILDSIDE_EPQ_WORD_SHIFT`).
    pub fn epq_word(&self) -> Option<u16> {
        let w = (self.arg >> BUILDSIDE_EPQ_WORD_SHIFT) as u16;
        (self.kind == ProtoKind::BuildSide && self.arg & BUILDSIDE_EPQ_FIRST != 0 && w != 0).then(|| w - 1)
    }

    /// D2050: reading `i` over a finished breaker region, its first item's rows (the proto's `arg` as its words, as the
    /// protocols pass it). `None` without the reading, for a region under 64 bytes (a breaker region no row laid out), or
    /// for one the reading does not know.
    pub fn read_rows(&self, i: usize, region: &[u8]) -> Option<u64> {
        let f = self.read.get(i).copied().flatten()?;
        if region.len() < 64 {
            return None;
        }
        let mut n = None;
        let mut first = |_: u64, rows: u64, _: u64| {
            n.get_or_insert(rows);
        };
        // SAFETY: `region` holds at least 64 readable bytes, the most a reading reads before it checks the header. No owner:
        // a first item's rows (the build's `build_rows` reading) take no working memory
        unsafe { f(region.as_ptr(), &[self.arg], None, &mut first) }?;
        n
    }
}

/// A v4 program's protocols, in declaration order (the runner runs a pipeline's in this order).
///
/// planprod P1-b: generic over where its lists live. The lowerer builds it on the heap (`Global`); `Program::with_v4`
/// freezes a copy into an arena of the program's own, sized exactly (`ArenaRef`), so the descriptors are the program's
/// bytes and move with it to its holder at filing (`Program::file_on_ledger`). Only the heap form clones (`thaw` copies a
/// frozen one out): nothing allocates into the program's arena after the freeze.
#[derive(Debug)]
pub struct V4Descs<A: Allocator + Clone = Global> {
    pub protos: AVec<Proto, A>,
    /// D2651: a rewind of the statement (ExecutorRewind: a SCROLL cursor's MOVE ABSOLUTE 0, FETCH FIRST) would call again a
    /// VOLATILE function whose result PostgreSQL's rescan keeps (lowerv4 effects/visible.rs `rewind_visible`): the entry
    /// routes a portal with EXEC_FLAG_REWIND to the row executor.
    pub rewind_reruns: bool,
    /// D2701: a rewind that keeps the live execution and runs only the emit pipeline again equals PostgreSQL's rescan
    /// (lowerv4 effects/visible.rs `soft_rewind`): the entry keeps a SCROLL portal served past a `rewind_reruns` mark.
    pub soft_rewind: bool,
    /// D439: the program evaluates a VOLATILE call per row (lowerv4 effects/visible.rs `pull_exact`: a `Volatile` protocol
    /// is declared), so a pull runs no row ahead of its count (driver `pull::pull_exact`, which adds a row lock's).
    pub pull_exact: bool,
    /// The pipelines the runtime runs again with their products reset in place, each with its one runner (lowerv4 lifetime
    /// `program`): the runtime resets exactly these (driver `Entry::rerun_nested`).
    pub lifetimes: crate::lifetime::Lifetimes<A>,
    /// D3769: sites exec admit pairs with a named graph source, `(site, the source pipeline's plan node id)`, reserved
    /// before the other sites pair by its walk order (exec admit `registry`): a plan Material's pulled child, whose fill
    /// pipeline drives no site, so the walk order would hand its source to another scan of that relation and index
    /// (lowerv4 rerun/spool/site.rs `walk_site`).
    pub site_pins: AVec<(u16, i32), A>,
}

/// The derived fold's fields in order; the pins only when there are any, so a program without them keeps its content key.
/// Each list folds as its slice (as the boxed slices did), whatever its home.
impl<A: Allocator + Clone> core::hash::Hash for V4Descs<A> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let V4Descs { protos, rewind_reruns, soft_rewind, pull_exact, lifetimes, site_pins } = self;
        protos[..].hash(state);
        rewind_reruns.hash(state);
        soft_rewind.hash(state);
        pull_exact.hash(state);
        lifetimes.hash(state);
        if !site_pins.is_empty() {
            site_pins[..].hash(state);
        }
    }
}

impl<A: Allocator + Clone, B: Allocator + Clone> PartialEq<V4Descs<B>> for V4Descs<A> {
    fn eq(&self, o: &V4Descs<B>) -> bool {
        self.protos[..] == o.protos[..]
            && self.rewind_reruns == o.rewind_reruns
            && self.soft_rewind == o.soft_rewind
            && self.pull_exact == o.pull_exact
            && self.lifetimes == o.lifetimes
            && self.site_pins[..] == o.site_pins[..]
    }
}

impl<A: Allocator + Clone> Eq for V4Descs<A> {}

/// A heap copy (`thaw`): tests and rigs only (feature `rig`, D4533).
#[cfg(any(test, feature = "rig"))]
impl Clone for V4Descs {
    fn clone(&self) -> V4Descs {
        self.thaw()
    }
}

impl Default for V4Descs {
    fn default() -> V4Descs {
        V4Descs::new(Vec::new())
    }
}

impl V4Descs {
    pub fn new(protos: Vec<Proto>) -> V4Descs {
        V4Descs { protos: crate::lifetime::heap_vec(protos), rewind_reruns: false, soft_rewind: false, pull_exact: false, lifetimes: crate::lifetime::Lifetimes::default(), site_pins: AVec::new() }
    }
}

impl<A: Allocator + Clone> V4Descs<A> {
    /// The bytes `freeze` takes from the program's arena: each list rounded up to the arena's 8-byte step.
    fn arena_bytes(&self) -> usize {
        round8(size_of_val(&self.protos[..])) + round8(size_of_val(&self.site_pins[..])) + self.lifetimes.arena_bytes()
    }

    /// The program's frozen copy, in an arena of its own sized exactly: the program's arena, never a statement's; sized at
    /// the freeze; nothing allocates into it after; the descriptors drop before it (`Program`'s field order).
    pub(crate) fn freeze(&self) -> (Arena, V4Descs<ArenaRef>) {
        let bytes = self.arena_bytes();
        let arena = Arena::new(bytes.max(8)); // alloc: Cold
        let a = arena.handle();
        let d = self.copy_in(&a);
        assert!(a.used() <= bytes.max(8), "v4 descriptors' arena: {} of {bytes}", a.used());
        (arena, d)
    }

    /// A copy on the global heap, for a rig or a test that edits a built program's descriptors and sets them back
    /// (`Program::set_v4`). Tests and rigs only (feature `rig`, D4533): the server's one copy is `freeze`'s, in the
    /// program's arena.
    #[cfg(any(test, feature = "rig"))]
    pub fn thaw(&self) -> V4Descs {
        self.copy_in(&Global)
    }

    fn copy_in<B: Allocator + Clone>(&self, b: &B) -> V4Descs<B> {
        let mut protos = AVec::with_capacity_in(self.protos.len(), b.clone()); // alloc: Program
        protos.extend_from_slice(&self.protos);
        let mut site_pins = AVec::with_capacity_in(self.site_pins.len(), b.clone()); // alloc: Program
        site_pins.extend_from_slice(&self.site_pins);
        V4Descs { protos, rewind_reruns: self.rewind_reruns, soft_rewind: self.soft_rewind, pull_exact: self.pull_exact, lifetimes: self.lifetimes.copy_in(b), site_pins }
    }

    pub fn is_empty(&self) -> bool {
        self.protos.is_empty()
    }

    /// The protocols declared on `pipeline`.
    pub fn on(&self, pipeline: usize) -> impl Iterator<Item = &Proto> {
        self.protos.iter().filter(move |p| p.pipeline as usize == pipeline)
    }

    /// The pipelines `pipeline` follows (`ProtoKind::Follows`' peers): each ran to its end before `pipeline` opens.
    pub fn follows(&self, pipeline: usize) -> impl Iterator<Item = usize> + '_ {
        self.on(pipeline).filter(|p| p.kind == ProtoKind::Follows).map(|p| p.peer as usize)
    }

    /// The furthest milestone any declared protocol needs.
    pub fn milestone(&self) -> u8 {
        self.protos.iter().map(|p| p.kind.milestone()).max().unwrap_or(0)
    }
}

/// FNV-1a over the descriptors' `Hash` bytes (low half) beside a multiply-rotate by murmur's fmix constant (high half); a
/// content-key fold, never a key hash, so it carries none of abi/src/hash.rs's constants (program.rs keeps its hasher private).
struct Fold(u64, u64);

impl Hasher for Fold {
    fn finish(&self) -> u64 {
        self.0 ^ self.1
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
            self.1 = (self.1 ^ b as u64).wrapping_mul(0xff51_afd7_ed55_8ccd).rotate_left(29);
        }
    }
}

impl Program {
    /// The program's v4 protocols: two programs of one IR and layout declaring different protocols are different programs,
    /// so the content key folds them in. An empty set leaves the program (and its key) as it was.
    pub fn with_v4(mut self, d: impl core::borrow::Borrow<V4Descs>) -> Program {
        let d = d.borrow();
        if d.is_empty() {
            return self;
        }
        for k in [&mut self.key, &mut self.bind_key] {
            let mut h = Fold(k.0 as u64, (k.0 >> 64) as u64);
            d.hash(&mut h);
            *k = ContentKey(((h.1 as u128) << 64) | h.0 as u128);
        }
        self.set_v4(d);
        self
    }

    /// `d` frozen into the program's own arena (`V4Descs::freeze`) as `with_v4` does, the content key left as it is: for
    /// a program put together by hand (tests, rigs). Replaces any earlier set.
    pub fn set_v4(&mut self, d: &V4Descs) {
        let (arena, frozen) = d.freeze();
        self.v4 = None;
        self.v4 = Some(FrozenV4(Arc::new(frozen))); // alloc: Program
        self.set_v4_arena(arena);
    }
}

/// planprod P1-b: a program's frozen v4 descriptors (`Program::v4`), read through `Deref`. The `Arc` stays an `Arc`, but
/// this handle is not `Clone`: no copy of it outlives the program, so the descriptors drop before the arena they live in
/// (`Program::side`, after `v4` in field order).
#[derive(Debug)]
pub struct FrozenV4(Arc<V4Descs<ArenaRef>>);

impl core::ops::Deref for FrozenV4 {
    type Target = V4Descs<ArenaRef>;

    fn deref(&self) -> &V4Descs<ArenaRef> {
        &self.0
    }
}

/// M8 (RT-1): the pipelines a pipeline `p` that runs on demand takes with it (an Init's range, `ProtoKind::Init`):
/// `members` = `p` and its exclusive transitive producers, in program order; `shared` = the transitive producers dropped
/// because a pipeline outside the set also reads them (each runs in its own turn, before its other readers).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Members {
    pub members: Vec<u16>,
    pub shared: Vec<u16>,
}

impl Program {
    /// THE on-demand closure, `members_in` over this program: exec's producer-order skip reads it.
    pub fn members(&self, p: usize) -> Members {
        members_in(&self.pipelines[..], self.v4.as_deref().map_or(&[][..], |d| &d.protos[..]), p)
    }
}

/// The pipelines `q` waits for, over `pipelines` in program order and the program's `protos`: the nearest earlier producer
/// of each slot `q` consumes (`Program::producer_of_slot`'s rule), with every arm folding into that production (`arms_of`),
/// then the peers `q` follows (`ProtoKind::Follows`, M7-DRAIN V4R-377: a drain after its probe, an order edge with no
/// slot), as `Program::producers_of` yields them. The one producer relation the on-demand closure reads, at lowering (before
/// a `Program` exists: lowerv4's renumbering of deferred slots) and at run time alike. A pulled set's last pipeline is never
/// one: a CTE's plan (`ProtoKind::Cte`, CT-2: its readers pull it, `m8_cte_pull`, RT-2f) and a window's pulled fill
/// (`WINDOW_PULLED`: its object pulls it, `m10_win_ask`).
pub fn producers_in<'a>(pipelines: &'a [crate::Pipeline], protos: &'a [Proto], q: usize) -> impl Iterator<Item = usize> + 'a {
    let follows = protos.iter().filter(move |p| p.kind == ProtoKind::Follows && p.pipeline as usize == q).map(|p| p.peer as usize);
    // D3037: a CTE whose producers run ahead takes none of them into its pulled set
    let ahead = protos.iter().any(|p| p.kind == ProtoKind::Cte && p.pipeline as usize == q && p.arg & CTE_PRODUCERS_AHEAD != 0);
    pipelines[q].consumes.iter().filter(move |_| !ahead).filter_map(move |&s| (0..q).rev().find(|&p| pipelines[p].produces() == Some(s)).filter(|&p| !protos.iter().any(|x| pulled(x) && x.pipeline as usize == p) && !table_fill(protos, p, q)).map(|p| (p, s))).flat_map(move |(p, s)| arms_of(pipelines, protos, p, s)).chain(follows)
}

/// subselect#316 / #317: whether `p` fills a recursive union's table (its seed the working table, its term the intermediate
/// one: a Spool proto of the union's driver, `ProtoKind::Generation`, names it as `peer`, lowerv4 ops/setop/recursive.rs)
/// read by `q`, another pipeline than the driver: a CTE inside the term reading the working table (its WorkTableScan).
/// The driver fills the table (the seed's run, then the swap after each generation), so `p` is never `q`'s on-demand
/// producer: PG's CTE pull reads the working table as the generation left it, and never runs the seed again.
fn table_fill(protos: &[Proto], p: usize, q: usize) -> bool {
    let driver = |d: u16| d as usize != q && protos.iter().any(|g| g.kind == ProtoKind::Generation && g.pipeline == d);
    protos.iter().any(|w| w.kind == ProtoKind::Spool && w.peer as usize == p && driver(w.pipeline))
}

/// D2050 (lockrows): the BuildSide proto's `arg` bit of a build whose probe (`peer`, a rechecking write's pipeline) is an
/// outer-first INNER or SEMI hash join. PostgreSQL keeps a write's EvalPlanQual tree for the statement, and
/// ExecReScanHashJoin keeps `hj_OuterNotEmpty` when the recheck's rescan rebuilds the table. So from the recheck after one
/// whose outer side reached the join, the join builds first and, over an empty build, returns before its outer side runs
/// (driver `Entry::rerun`). Bits 0 (op 14's defer) and 8..47 (D276's skip mask) are the proto's other fields.
pub const BUILDSIDE_EPQ_FIRST: u64 = 1 << 1;

/// D2674 (lockrows): bits 48..63 of a `BUILDSIDE_EPQ_FIRST` proto's `arg`: 1 + the frame word (in the program's words site)
/// where the probe records PostgreSQL's `hj_OuterNotEmpty` points in a call (a row reaching it over an empty table, or a
/// row with non-NULL outer keys); 0 for none (`Proto::epq_word`).
pub const BUILDSIDE_EPQ_WORD_SHIFT: u32 = 48;

/// The Window proto's `arg` bit of a pulled fill (the exact request, PLAN-v5 §2.5; W4): the proto's pipeline is the fill,
/// a pulled set (deferred) that the window object runs one row per pull from its spool (`m10_win_ask`), never ahead of it.
pub const WINDOW_PULLED: u64 = 1 << 33;

/// With `WINDOW_PULLED`, the Window proto's `arg` bit of a fill that streams (W10, the budgeted pull): each pull may read
/// ahead of the spool, its budget doubling from one row (the object's `Rt::more`). Without it every pull takes one row
/// (the exact request).
pub const WINDOW_AHEAD: u64 = 1 << 34;

/// D3336 (win.partruns): the Window proto's `arg` bit of a window read by claims (its `pipeline` the fill, `peer` the
/// reader): exec lays several window objects and the handle word holds the address of their table (`WINDOW_PARTS_*`);
/// the serial fill stores each partition whole into the object whose turn it is; the execution answers the reader's
/// claims, one object each.
pub const WINDOW_PARTED: u64 = 1 << 36;

/// D3336 (lowerv4 road win.partfill): with `WINDOW_PARTED`, the Window proto's `arg` bit of a window whose fill runs as
/// claims too: its source is the drain of a sorted wide emit whose section end cut the merged groups on the window's
/// PARTITION BY (kernels `agg_merge_runs_part_k`), and the execution answers the fill's claims as the reader's, one
/// object each: claim `i` reads its share of the parts (`agg_runs_claim`) into object `i`.
pub const WINDOW_CUT: u64 = 1 << 37;

/// D3336: a parted window's object table (`WINDOW_PARTED`): word 0 the object count `n`, then each object's handle, then
/// (word `n + 1`) the index of the object the fill stores into now.
pub const WINDOW_PARTS_COUNT: usize = 0;
pub const WINDOW_PARTS_AT: usize = 1;

/// D3037: the Cte proto's `arg` bit of a CTE whose producers run ahead, in program order at their own widths, and are no
/// members of its pulled set (`producers_in` names none for it): only the CTE's own pipeline is pulled, leader-serial.
/// lowerv4 sets it for a quiet CTE (raise-free, no VOLATILE call, no lock or write) whose every reader pulls it at its
/// first row; each producer's slot is held through the CTE's last reader (`Pipeline::live_through`). PostgreSQL runs
/// those producers whole at the first pull (a hashed group, a sort, a hash build reads all its input before its first
/// row), so only the time changes, never an answer.
pub const CTE_PRODUCERS_AHEAD: u64 = 1 << 34;

/// D3040: the Cte proto's `arg` bit of a CTE filled whole in program order, at its pipeline's width, and never pulled (no
/// pulled set: `pulled` is false for it): its store is an owned spool (kernels `SPOOL_OWNED`), each owner appending to its
/// own spool, sealed after the fill, and its readers read the sealed rows (the eager form: past them is the end). lowerv4
/// sets it for a quiet CTE every reader reads whole from its first row (no LIMIT over a reader, no gate before it), so the
/// fill runs what PostgreSQL's readers would pull; only the time and the rows' order (a CTE with no ORDER BY) change.
pub const CTE_FILL_PARALLEL: u64 = 1 << 35;

/// D2605: the Cte proto's `arg` bits from here up hold a data-modifying CTE's drain rank, its plan id (PostgreSQL's
/// `plannedstmt->subplans` index): ExecPostprocessPlan runs `es_auxmodifytables`, which ExecInitModifyTable builds with
/// `lcons` as the CTE plans initialize in subplan order, so the statement-end drain runs the higher plan id first (exec
/// ops/mod.rs `drain_at_end`).
pub const CTE_RANK_SHIFT: u32 = 48;

/// The Markable proto's `arg` bit of a merge join's pulled inner (the exact request; jmerge-0012): the proto's pipeline is
/// the inner's spool, a pulled set (deferred) the merge runs one row per fetch past the stored rows (`m11_pull`).
pub const MERGE_PULLED: u64 = 1 << 33;

/// D3190 (merge.cosplit): the co-split's range table layout (gravity_abi::program: the kernels read it too).
pub use gravity_abi::program::{COSPLIT_HDR_WORDS, COSPLIT_MODE, COSPLIT_MODE_RESIDENT, COSPLIT_MODE_SLICES, COSPLIT_MODE_WHOLE, COSPLIT_PARTS, COSPLIT_PART_WORDS, COSPLIT_RUNS, COSPLIT_SLICES_AT, COSPLIT_WHOLE};

/// D3190: the CoSplit proto's `arg`: the inner's slot (bits 0..8) and the merge keys the cut compares (bits 8..16).
pub const fn cosplit_arg(inner_slot: u8, nkey: u8) -> u64 {
    inner_slot as u64 | (nkey as u64) << 8
}

/// D3190: `cosplit_arg`'s `(inner slot, merge keys)`.
pub const fn cosplit_of(arg: u64) -> (u8, u8) {
    (arg as u8, (arg >> 8) as u8)
}

/// D3336 (A1a, lowerv4 road win.partfill, face/partfill.rs `rows_form`): the CoSplit proto's `arg` bit of a one-sided cut
/// for a parted window's fill (`WINDOW_CUT`): the fill's full row sort alone (the proto's slot) is cut on its first `nkey`
/// keys into as many parts as the window has objects (its object table in param word `cosplit_window_word`), so each
/// partition lies in one part; the fill's claims are the window's (one object each), claim `i` reading part `i` into object
/// `i`. No cut (too few rows, a run put without fences, one object): part 0 is the whole run, read by claim 0, which deals
/// the partitions as the serial fill does; the other parts are empty. This execution answers no claims of its own.
pub const COSPLIT_WINDOW: u64 = 1 << 32;

/// D3336 (A1a): `COSPLIT_WINDOW`'s `arg`: the cut keys (bits 8..16, as `cosplit_arg`'s) and the window's object table word
/// (bits 16..32); bits 0..8 hold no slot.
pub const fn cosplit_window_arg(window_word: u16, nkey: u8) -> u64 {
    COSPLIT_WINDOW | 0xff | (nkey as u64) << 8 | (window_word as u64) << 16
}

/// D3336 (A1a): `cosplit_window_arg`'s window object table word.
pub const fn cosplit_window_word(arg: u64) -> usize {
    ((arg >> 16) & 0xffff) as usize
}

/// A proto naming a pulled set's last pipeline: a CTE's plan (CT-2), a window's pulled fill (`WINDOW_PULLED`) or a merge
/// join's pulled inner (`MERGE_PULLED`).
pub fn pulled(p: &Proto) -> bool {
    p.kind == ProtoKind::Cte && p.arg & CTE_FILL_PARALLEL == 0 || (p.kind == ProtoKind::Window && p.arg & WINDOW_PULLED != 0) || (p.kind == ProtoKind::Markable && p.arg & MERGE_PULLED != 0)
}

/// D393 (append.arms): the pipelines whose rows fold into one production of slot `s` with pipeline `x`, in program order:
/// those producing `s` in the `Arms` run that holds `x`, the driver's `Node::arms_of` rule (the run is `pipeline ..
/// pipeline + arg`, its first and last producing `s`; V4R-129: a MergeAppend arm's child Sort between two arms is in the
/// range and produces another slot; D559: a slot number reused by a later run is the run holding `x`). The driver opens the
/// slot at the run's first arm and folds the rest on into it, so an on-demand run takes every arm or none. `[x]` when `x`
/// is no arm of such a run.
pub fn arms_of(pipelines: &[crate::Pipeline], protos: &[Proto], x: usize, s: u8) -> Vec<usize> {
    let produces = |q: usize| pipelines.get(q).and_then(|p| p.produces()) == Some(s);
    let run = protos.iter().filter(|a| a.kind == ProtoKind::Arms && a.slot == s).map(|a| a.pipeline as usize..(a.pipeline as usize).saturating_add(a.arg as usize)).find(|r| r.contains(&x) && produces(r.start) && produces(r.end - 1));
    match run {
        Some(r) => r.filter(|&q| produces(q)).collect(), // alloc: Exec
        None => vec![x], // alloc: Exec
    }
}

/// The SubHash proto's `arg` bit of a LAZY set (V4R-369): its build runs at the first probe (`m8_subhash_run`).
pub const SUBHASH_LAZY: u64 = 1 << 16;

/// The Rerun proto's `arg` bit of a member PostgreSQL keeps at a rescan that changes no param (D1018 item 9 (b): a nest
/// inner's root Sort, top-k or hashed group and what fills it): it runs at its set's first run only (the owner's first
/// re-open), and later runs leave its slot as it is, rewound by its reader. The low bits are the set's index.
pub const RERUN_KEPT: u64 = 1 << 32;

/// D1036: the program's on-demand groups: each Init proto (its members), each LAZY SubHash (V4R-369), each rerun set (the
/// Rerun protos sharing one handle word, M8-RERUN: a set reruns nested inside an outer row), each lazy build
/// (with its fed producers, one group per build its probe reads) and (D3549) each pulled set (`pulled`: a CTE's plan, a
/// pulled window fill or merge inner, run by its reader's pull: driver entry.rs `pull_nested`). Out-of-order runs nest
/// inside one row at most this deep (a group never re-enters itself: an Init's RUNNING state, a lazy build's one run, a
/// pulled set's pull phase), each nested run on a hosted lane of its own; lowerv4 refuses by name past gravity_driver's
/// `NESTED_LANES`.
pub fn on_demand_groups(pipelines: &[crate::Pipeline], protos: &[Proto]) -> usize {
    let builds = pipelines.iter().filter(|p| usize::try_from(p.lazy_for).ok().and_then(|q| pipelines.get(q)).zip(p.produces()).is_some_and(|(q, s)| q.consumes.contains(&s))).count();
    let reruns = protos.iter().enumerate().filter(|&(i, p)| p.kind == ProtoKind::Rerun && !protos[..i].iter().any(|q| q.kind == ProtoKind::Rerun && q.words.base == p.words.base));
    builds + reruns.count() + protos.iter().filter(|p| p.kind == ProtoKind::Init || (p.kind == ProtoKind::SubHash && p.arg & SUBHASH_LAZY != 0) || pulled(p)).count()
}

/// THE on-demand closure: `p` and its exclusive producers over `producers_in`; lowerv4's renumbering of
/// deferred slots (M8-SUB's Init members into D1005's renumber_deferred) and exec's producer-order skip both read it.
pub fn members_in(pipelines: &[crate::Pipeline], protos: &[Proto], p: usize) -> Members {
    members_over(pipelines.len(), p, |q| producers_in(pipelines, protos, q))
}

/// `Program::members` over `n` pipelines whose producers `producers(q)` names: the transitive producers of `p`, then, to a
/// fixpoint, every one some pipeline outside the set reads moves to `shared` (and with it whatever only it read).
pub fn members_over<I: IntoIterator<Item = usize>>(n: usize, p: usize, producers: impl Fn(usize) -> I) -> Members {
    let of: Vec<Vec<usize>> = (0..n).map(|q| producers(q).into_iter().collect()).collect(); // alloc: Exec
    let mut set = vec![p]; // alloc: Exec
    let mut i = 0;
    while i < set.len() {
        for &r in &of[set[i]] {
            if !set.contains(&r) {
                set.push(r);
            }
        }
        i += 1;
    }
    let mut shared = Vec::new(); // alloc: Exec
    while let Some(i) = set.iter().position(|&r| r != p && (r + 1..n).any(|c| of[c].contains(&r) && !set.contains(&c))) {
        shared.push(set.swap_remove(i) as u16);
    }
    set.sort_unstable();
    shared.sort_unstable();
    Members { members: set.into_iter().map(|x| x as u16).collect(), shared }
}

#[cfg(test)]
mod members_tests {
    use super::*;

    fn over(g: &[&[usize]], p: usize) -> (Vec<u16>, Vec<u16>) {
        let m = members_over(g.len(), p, |q| g[q].iter().copied());
        (m.members, m.shared)
    }

    #[test]
    fn an_inits_transitive_producers_are_all_members() {
        // 0 scan -> agg (slot), 1 the agg's emit -> a sort (slot), 2 the sort's emit into the cell end; 3 the main query
        let g: [&[usize]; 4] = [&[], &[0], &[1], &[]];
        assert_eq!(over(&g, 2), (vec![0, 1, 2], vec![]));
    }

    #[test]
    fn a_follows_peer_comes_in_with_its_drain() {
        // 1 the probe Q, 2 the drain D (Follows(D, Q): producers_of(2) names 1), 3 D's consumer ending in the cell end
        let g: [&[usize]; 4] = [&[], &[0], &[1], &[2]];
        assert_eq!(over(&g, 3), (vec![0, 1, 2, 3], vec![]));
    }

    #[test]
    fn a_producer_shared_with_a_non_member_is_flagged_with_what_only_it_read() {
        // 0 a CTE spool read by the Init's 3 and the main query's 5; 1 -> 2 -> 3 the Init's chain, but 6 also reads 1, so
        // 1 is shared while 2 stays (only 3 reads it); 4 -> 5 the main query's build
        let g: [&[usize]; 7] = [&[], &[], &[1], &[2, 0], &[], &[0, 4], &[1]];
        assert_eq!(over(&g, 3), (vec![2, 3], vec![0, 1]));
        // a shared producer's own producer goes with it: 0 -> 1, 1 read by 2 (member) and 4 (not)
        let g: [&[usize]; 5] = [&[], &[0], &[1], &[2], &[1]];
        assert_eq!(over(&g, 3), (vec![2, 3], vec![0, 1]));
    }

    #[test]
    fn each_init_and_each_lazy_build_its_probe_reads_is_one_on_demand_group() {
        use gravity_abi::breaker::{Breaker, BreakerKind};
        let build = |slot: u8| crate::Pipeline::serial(0, crate::End::Breaker(Breaker::sized(BreakerKind::HashBuild, 64).in_slot(slot)), crate::Reason::NoDrivingSite);
        // 0 a fed producer of the build (lazy for 2, read by 1 only), 1 the lazy build (read by 2), 2 the probe
        let (mut fed, mut b, mut q) = (build(0), build(1), crate::Pipeline::serial(0, crate::End::Sink, crate::Reason::NoDrivingSite));
        (fed.lazy_for, b.lazy_for, b.consumes, q.consumes) = (2, 2, &[0], &[1]);
        let ps = [fed, b, q];
        assert_eq!(on_demand_groups(&ps, &[]), 1, "the fed producer is its build's group");
        assert_eq!(on_demand_groups(&ps, &[Proto::new(ProtoKind::Init, 0), Proto::new(ProtoKind::Width, 0), Proto::new(ProtoKind::Init, 1)]), 3);
        let (eager, lazy) = (Proto::new(ProtoKind::SubHash, 0).arg(1 | 8 << 8), Proto::new(ProtoKind::SubHash, 0).arg(1 | 8 << 8 | SUBHASH_LAZY));
        assert_eq!(on_demand_groups(&ps, &[eager, lazy]), 2, "a LAZY SubHash is a group, a set built in program order is not");
        let rerun = |handle: u16, member: u16| Proto::new(ProtoKind::Rerun, member).words(handle, 1);
        assert_eq!(on_demand_groups(&ps, &[rerun(4, 0), rerun(4, 1), rerun(9, 0)]), 3, "a rerun set is one group however many members its handle holds");
        let cte = |arg: u64| Proto::new(ProtoKind::Cte, 1).arg(arg);
        let window = Proto::new(ProtoKind::Window, 1).arg(WINDOW_PULLED);
        assert_eq!(on_demand_groups(&ps, &[cte(0), cte(CTE_FILL_PARALLEL), window]), 3, "D3549: a pulled set is a group (a CTE's plan, a pulled window fill), a CTE filled ahead is not");
    }

    #[test]
    fn every_arm_folding_into_the_production_an_init_reads_is_its_member() {
        use gravity_abi::breaker::{Breaker, BreakerKind};
        let agg = |slot: u8| crate::Pipeline::serial(0, crate::End::Breaker(Breaker::sized(BreakerKind::PlainAgg, 64).in_slot(slot)), crate::Reason::NoDrivingSite);
        let sink = |consumes: &'static [u8]| {
            let mut p = crate::Pipeline::serial(0, crate::End::Sink, crate::Reason::NoDrivingSite);
            p.consumes = consumes;
            p
        };
        let arms = |first: u16, arg: u64| {
            let mut a = Proto::new(ProtoKind::Arms, first);
            (a.arg, a.slot) = (arg, 0);
            a
        };
        // 0..2 an Append's three arms folding into one PlainAgg in slot 0 (D393), 3 the InitPlan's end over it; 4 a later
        // production of slot 0 that is no arm, 5 its reader
        let ps = [agg(0), agg(0), agg(0), sink(&[0]), agg(0), sink(&[0])];
        let m = members_in(&ps, &[arms(0, 3)], 3);
        assert_eq!((m.members, m.shared), (vec![0, 1, 2, 3], vec![]));
        assert_eq!(members_in(&ps, &[arms(0, 3)], 5).members, vec![4, 5], "a later production of the slot is no arm");
        assert_eq!(members_in(&ps, &[], 3).members, vec![2, 3], "without the Arms proto only the nearest producer");
        // V4R-129: a MergeAppend run counts its pipelines (1 an arm's child Sort, another slot, read by arm 2); D559: 4
        // reuses slot 0 after the run, never one of its arms
        let mut arm = agg(0);
        arm.consumes = &[1];
        let ps = [agg(0), agg(1), arm, sink(&[0]), agg(0), sink(&[0])];
        assert_eq!(arms_of(&ps, &[arms(0, 3)], 2, 0), vec![0, 2]);
        assert_eq!(members_in(&ps, &[arms(0, 3)], 3).members, vec![0, 1, 2, 3]);
        assert_eq!(arms_of(&ps, &[arms(0, 3)], 4, 0), vec![4], "a production past the run");
        assert_eq!(arms_of(&ps, &[arms(0, 2)], 0, 0), vec![0], "a run whose last pipeline does not produce the slot is none");
    }

    #[test]
    fn a_drain_beside_an_append_armed_slot_takes_its_probe() {
        use gravity_abi::breaker::{Breaker, BreakerKind};
        // M7-DRAIN (V4R-377, main's approval condition 1, kept beside RT-1b's arms_of): 0 the build (slot 0); 1 the probe
        // and 2 the drain, both reading the build, both arms of one keyed fold in slot 1 (the seeded form: ProtoKind::Arms
        // over 1 and 2), 2 following 1; 3 the fold's emit. The fold's readers take both arms (arms_of); the drain's Follows
        // brings the probe in before the drain opens
        let (b, agg) = (Breaker::sized(BreakerKind::HashBuild, 64), Breaker::sized(BreakerKind::HashAgg, 64).in_slot(1));
        let serial = |end| crate::Pipeline::serial(0, end, crate::Reason::NoDrivingSite);
        let arm = || serial(crate::End::Breaker(agg)).consuming(&[0]);
        let ps = [serial(crate::End::Breaker(b)), arm(), arm(), serial(crate::End::Sink).consuming(&[1])];
        let protos = [Proto::new(ProtoKind::Arms, 1).slot(1).arg(2), Proto::new(ProtoKind::Follows, 2).peer(1)];
        assert_eq!(producers_in(&ps, &protos, 2).collect::<Vec<_>>(), [0, 1]); // alloc: Cold
        assert_eq!(producers_in(&ps, &protos, 3).collect::<Vec<_>>(), [1, 2]); // alloc: Cold
        assert_eq!(members_in(&ps, &protos, 3), Members { members: vec![0, 1, 2, 3], shared: vec![] }); // alloc: Cold
        // without the edge the drain waits for its build only
        assert_eq!(producers_in(&ps, &protos[..1], 2).collect::<Vec<_>>(), [0]); // alloc: Cold
    }

    #[test]
    fn an_init_with_no_producer_is_its_one_pipeline() {
        let g: [&[usize]; 3] = [&[], &[], &[]];
        assert_eq!(over(&g, 1), (vec![1], vec![]));
    }

    #[test]
    fn a_term_ctes_closure_stops_at_the_working_table() {
        use gravity_abi::breaker::{Breaker, BreakerKind};
        // subselect#316 / #317: 0 the seed into the working table (slot 0), 1 a CTE inside the term reading it through its
        // WorkTableScan into its store (slot 1), 2 the term's body over the CTE, 3 the union's driver (source slot 0,
        // Generation; its Spool protos name the seed and the body as the two tables' fillers)
        let serial = |end| crate::Pipeline::serial(0, end, crate::Reason::NoDrivingSite);
        let fill = |slot: u8| serial(crate::End::Breaker(Breaker::sized(BreakerKind::Materialize, 64).in_slot(slot)));
        let ps = [fill(0), fill(1).consuming(&[0]), fill(2).consuming(&[1]), serial(crate::End::Sink).consuming(&[0])];
        let protos = [Proto::new(ProtoKind::Generation, 3), Proto::new(ProtoKind::Spool, 3).peer(0), Proto::new(ProtoKind::Spool, 3).peer(2), Proto::new(ProtoKind::Cte, 1)];
        assert_eq!(members_in(&ps, &protos, 1), Members { members: vec![1], shared: vec![] }, "the CTE's pull never runs the seed"); // alloc: Cold
        assert_eq!(members_in(&ps, &protos, 3).members, vec![0, 3], "the driver keeps the seed in its closure"); // alloc: Cold
        // without the union's protos the seed is the CTE's producer, shared with the driver (the refusal before)
        assert_eq!(members_in(&ps, &protos[3..], 1), Members { members: vec![1], shared: vec![0] }); // alloc: Cold
    }
}

#[cfg(test)]
mod frozen_tests {
    use super::*;
    use crate::lifetime::{Lifetimes, RunLife, RunMember, Runner, SetRearms, NO_PROBE};
    use crate::{Code, Layout, OutCol, SiteDesc, WritePlan};

    unsafe extern "C-unwind" fn fake_entry(_ctx: *mut gravity_abi::Ctx) -> i64 {
        gravity_abi::status::DONE
    }

    fn plain() -> Program {
        let st = Arena::new(1 << 12);
        let sites = [SiteDesc::index_source(st.handle(), 12, 13, &[1, 2], &[0, 1], &[3, 4])];
        Program::new(Layout { nsites: 1, out_natts: 1, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 }, &sites, &[OutCol::new("a", 23, -1, 4, true)], WritePlan::SELECT, Code::hand(fake_entry))
    }

    fn descs() -> V4Descs {
        let mut d = V4Descs::new(vec![Proto::new(ProtoKind::Arms, 0), Proto::new(ProtoKind::Prune, 1), Proto::new(ProtoKind::Rerun, 2)]);
        d.site_pins = [(1u16, 7i32), (2, 9)].into();
        let member = |pipeline| RunMember { pipeline, runner: Runner::Set(0), life: RunLife::EveryRun, lazy_probe: NO_PROBE };
        let rearms = SetRearms { inits: [(7, 3)].into(), prunes: [1].into(), windows: [2, 4].into(), ..Default::default() };
        d.lifetimes = Lifetimes::new(vec![member(2), member(1)]).expect("one runner per member").with_rearms(vec![rearms]);
        d
    }

    /// planprod P1-b (memlead's conditions): the program's descriptors are a copy frozen into an arena of the program's
    /// own, sized exactly at the freeze, so every list lies in it (none on the builder's heap or a statement's context),
    /// the builder's lists go when it drops, and the content key folds them as the boxed lists did.
    #[test]
    fn a_programs_v4_descriptors_are_a_frozen_copy_in_its_own_exactly_sized_arena() {
        let d = descs();
        let p = plain().with_v4(&d);
        let a = p.v4_arena().expect("with_v4 froze them into the program's arena").handle();
        assert_eq!(a.used(), d.arena_bytes(), "sized at the freeze, nothing taken after");
        let f = p.v4.as_deref().expect("frozen");
        let in_arena = |ptr: *const u8, len: usize| len == 0 || a.offset_of(ptr) + len <= a.used();
        assert!(in_arena(f.protos.as_ptr().cast(), size_of_val(&f.protos[..])));
        assert!(in_arena(f.site_pins.as_ptr().cast(), size_of_val(&f.site_pins[..])));
        assert!(in_arena(f.lifetimes.members().as_ptr().cast(), size_of_val(f.lifetimes.members())));
        let r = f.lifetimes.rearms(0).expect("set 0's re-arms");
        for (ptr, len) in [(r.inits.as_ptr().cast(), size_of_val(&r.inits[..])), (r.prunes.as_ptr().cast(), size_of_val(&r.prunes[..])), (r.windows.as_ptr().cast(), size_of_val(&r.windows[..]))] {
            assert!(in_arena(ptr, len), "a re-arm list in the program's arena");
        }
        let key = p.key;
        drop(d);
        let again = descs();
        assert!(*f == again, "the frozen copy reads as the lists it was made from, after they are gone");
        assert_eq!(key, plain().with_v4(again.clone()).key, "the key: an owned set or a borrowed one");
        let mut h = Fold(0, 0);
        again.hash(&mut h);
        let mut g = Fold(0, 0);
        f.hash(&mut g);
        assert_eq!((h.0, h.1), (g.0, g.1), "the frozen copy hashes as its source");
        assert_eq!(f.thaw(), again, "thaw: back on the heap, the same");
        let mut q = plain();
        q.set_v4(&again);
        assert_eq!(q.key, plain().key, "set_v4 leaves the key as it is");
        assert_eq!(q.v4_arena().map(|a| a.handle().used()), Some(again.arena_bytes()));
    }
}

#[cfg(test)]
mod literals_tests {
    use super::*;

    /// NEW-29: a Const-only list marks 0; any coded cell marks non-zero, by its (row, column).
    #[test]
    fn the_coded_cells_mark_is_zero_only_for_a_const_only_list() {
        assert_eq!(literals_mark([]), 0);
        let marks: Vec<u16> = [(0, 0), (0, 1), (1, 0), (255, 3), (65_535, 0)].iter().map(|&c| literals_mark([c])).collect();
        assert!(marks.iter().all(|&m| m != 0), "{marks:?}");
        let mut uniq = marks.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(uniq.len(), marks.len(), "another cell, another mark: {marks:?}");
        assert_ne!(literals_mark([(0, 0), (1, 0)]), literals_mark([(0, 0)]), "a second coded cell moves the mark");
        assert_eq!(LITERALS_CODED & 1, 0, "the coded bit is not the null bit");
    }
}
