//! What a breaker does past its grant (construct C10, spill coverage): it spills (`Holds::Spills`), or it is bounded by a
//! proof (`Holds::Bounded`). `plan` is the one constructor of a breaker's answer; every lowering that ends a breaker asks
//! it (decompose's declaration through `declared`, a lowered end that settles otherwise through `Pipeline::holds`), so the
//! descriptor the driver reads says what the lowered code does, not what decompose declared.
//!
//! A proof is one of: a program constant no larger than `cost::breaker::CONSTANT_PROOF_BYTES` (PostgreSQL keeps that much
//! in memory at every work_mem it accepts); a form PostgreSQL 18.6 also keeps in memory past work_mem (`PgHolds`, so its
//! 53200 there is PostgreSQL's own); or an estimate (`Proof::Estimate`), which is debt: each `NoSpill` names the one site
//! that settles a breaker that way and has its row in lint/SPILL-DEBT (lint RG keeps the two exact).
use gravity_abi::breaker::{Breaker, BreakerKind, Budget, Spill};

use crate::cost::breaker::CONSTANT_PROOF_BYTES;

/// A breaker's answer past its grant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Holds {
    /// It spills: its partitions scatter (`Partitioned`, the bits the lowered code opens) or it puts its rows to files
    /// (`Runs`: a sort's runs, a spool's temp-file road). Never `Spill::None`.
    Spills(Spill),
    /// It keeps its rows in memory past the grant, by this proof.
    Bounded(Proof),
}

/// Why a breaker that does not spill is bounded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Proof {
    /// A program constant of at most `CONSTANT_PROOF_BYTES` (a plain aggregate's state, a constant top-k's rows).
    Constant(u32),
    /// PostgreSQL 18.6 keeps this form in memory past work_mem too.
    PgHoldsToo(PgHolds),
    /// Debt: the breaker grows past its grant where PostgreSQL spills; the site is named.
    Estimate(NoSpill),
}

/// The forms PostgreSQL 18.6 keeps in memory past work_mem, each its executor node's own table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PgHolds {
    /// A hashed set operation's left (first) side: nodeSetOp.c builds one TupleHashTable of it and never spills it.
    SetOpSide,
    /// A hashed SubPlan's set: nodeSubplan.c `buildSubPlanHash` fills its hash table in memory, no spill.
    HashedSubPlan,
}

/// The sites that settle a breaker to grow past its grant without a proof: one lint/SPILL-DEBT row each. A variant goes
/// when its site spills or proves its bound (its row with it).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum NoSpill {
    // ---- a hash build (lowerv4 ops/hashjoin/build.rs) ----
    /// No probe pipeline reads the build's slot.
    BuildNoProbe,
    /// Only a nest inner probes the build (the nest's own set).
    BuildNestInner,
    /// The probe pipeline holds no probe op of the build's slot.
    BuildNoProbeOp,
    /// A guarded build (one batch per user operator's guard).
    BuildGuarded,
    /// join.buildside's swapped build.
    BuildSwapped,
    /// The compact (dense bits) record form, which writes no spill code.
    BuildCompact,
    /// The build's spill face is off.
    BuildFaceOff,
    // ---- the probe's shape (lowerv4 ops/hashjoin/spill/build.rs `eligible`) ----
    /// The probe's union arms are not one run of consecutive arms with one marker each.
    ArmsNotOneRun,
    /// The union's op at the probe's place is not a probe.
    ArmsOpNotProbe,
    /// A drained end's arms.
    ArmsDrainEnd,
    /// An arm reads another life of the slot, or has no probe of it.
    ArmsApart,
    // ---- the probe alone (`alone`) ----
    /// The probe pipeline's end is a spooled drain.
    SpooledDrain,
    /// The pipelines are missing, the build has no breaker, or the op is no plain (non-catalog) probe.
    ProbeShape,
    /// A key whose domain CHECK calls a VOLATILE function.
    VolatileDomainKey,
    /// A join type outside the spilled forms.
    JoinTypeOutside,
    /// The probe's slot is not the build's.
    ProbeSlotApart,
    /// The probe pipeline consumes more slots than the pair copies, or has no driving site.
    ConsumesMore,
    /// An op other than a filter, a projection or the full sort's own step follows the probe (a chained probe among them).
    ChainTail,
    /// A rerun set holds the build, the probe pipeline or the slot.
    ResidentInRerun,
    /// The program writes or locks rows.
    ProgramWrites,
    /// The pair pipeline would make a rerun set again under its own identity.
    RemadeSet,
    // ---- the probe pipeline's end (`end_takes_pair`) ----
    /// A drained end with no breaker, or no drain pipeline that reads it.
    DrainShape,
    /// A drained end whose slot another pipeline also feeds.
    DrainShared,
    /// A sink end that is not the client's sink.
    SinkNotEndSink,
    /// The end's parallel Arms face is off.
    EndArmsFaceOff,
    /// A sort end that is not a runs sort, or whose cells the pair hands on in another form.
    SortNotPair,
    /// A plain fold with a scan-form register.
    PlainScanRegister,
    /// A hash aggregate end that is not a keyed owner end partitioned past its grant, or holds a scan-form register.
    HashAggNotPair,
    /// Another hash build's end.
    EndHashBuild,
    /// A Distinct, TopK or Materialize end.
    EndOther,
    /// A row-op end (a write's), or a breaker end that names no breaker.
    EndRowOp,
    // ---- a merge join's inner run (lowerv4 face/slot_run.rs) ----
    /// A run filled as its merge reads it (a pulled fill), grown in place.
    MergeRunPulled,
    /// A union arm's run, which carries its table copy, grown in place.
    UnionArmRun,
    // ---- other ends ----
    /// A keyed table whose records hold key copies (lowerv4 table/emit_key.rs `resident`).
    KeyCopies,
    /// A two-pass top's first pass, read whole by its readers.
    TwoPassPass1,
    /// A hashed SetOp's right side, kept whole where PostgreSQL streams it past the left side's table (decompose agg `set_side`).
    SetOpRightSide,
    /// A sorted SetOp's side, kept whole where PostgreSQL streams both sorted inputs.
    SetOpSortedSide,
    /// A program constant past `CONSTANT_PROOF_BYTES`.
    ConstantPastCap,
    // ---- a spilling breaker's road that still grows past its grant (named at its uncapped grow: driver
    // `State::grow_debt`, gravity_abi `region_grow_debt`; or at its pin past W and G: driver `arena::pin_owner`,
    // `arena::pin_batch`; or at its object's region past W and G: driver `arena::object_region`) ----
    /// A kind-7 build whose every partition is already out keeps growing its resident part (one partition level).
    BuildPartitionPastGrant,
    /// A hash build's leader merge (its workers' partials into the leader's table) grows uncapped.
    BuildLeaderMerge,
    /// A spilled hash build's leader merge, past G, grows uncapped.
    BuildSpillMerge,
    /// An owner-phase breaker's (a hash aggregate's, a distinct's) owners pin their region for every partition of every
    /// partial and spill file at once, past W and G (driver section.rs `own_all` / `own_split` / the pooled owners'
    /// chunk), where PostgreSQL refills one spilled batch at a time under hash_mem (D3582).
    AggOwnerAll,
    /// A batched owner's partition whose records alone pass the grant (C10 2b: driver mem/batch.rs folds a spilled one-round
    /// hash aggregate one batch of partitions at a time, each batch within the grant): the partition is folded whole in a
    /// region grown for it, one partition level, where PostgreSQL re-partitions such a batch (`hashagg_refill`) (D3582).
    AggPartitionPastGrant,
    // ---- a bulk load's road past the memory ladder's line (no breaker: pgrc_write par.rs `ladder_gate`) ----
    /// A COPY into pgrcolumnar whose part's seal (~3x the part's normalized bytes) does not fit under the memory ladder's
    /// line after its climb and waits is refused (D4365), where it could seal the part early or stream the seal a column
    /// at a time (D4370): the load cannot spill yet.
    LoadSealPastLine,
    /// No site settled this breaker: a lowering that skipped `plan` (a Bug by name, never a row).
    Unrouted,
}

impl NoSpill {
    /// Its code across the program interface (`region_grow_debt`'s word); `from_code` answers it back.
    pub const fn code(self) -> u8 {
        self as u8
    }

    /// Every reason, in code order (`ALL[c].code() == c`).
    pub const ALL: [NoSpill; NoSpill::Unrouted as usize + 1] = [
        NoSpill::BuildNoProbe, NoSpill::BuildNestInner, NoSpill::BuildNoProbeOp,
        NoSpill::BuildGuarded, NoSpill::BuildSwapped, NoSpill::BuildCompact, NoSpill::BuildFaceOff,
        NoSpill::ArmsNotOneRun, NoSpill::ArmsOpNotProbe, NoSpill::ArmsDrainEnd, NoSpill::ArmsApart,
        NoSpill::SpooledDrain, NoSpill::ProbeShape, NoSpill::VolatileDomainKey, NoSpill::JoinTypeOutside,
        NoSpill::ProbeSlotApart, NoSpill::ConsumesMore, NoSpill::ChainTail, NoSpill::ResidentInRerun,
        NoSpill::ProgramWrites, NoSpill::RemadeSet, NoSpill::DrainShape, NoSpill::DrainShared,
        NoSpill::SinkNotEndSink, NoSpill::EndArmsFaceOff, NoSpill::SortNotPair, NoSpill::PlainScanRegister,
        NoSpill::HashAggNotPair, NoSpill::EndHashBuild, NoSpill::EndOther, NoSpill::EndRowOp,
        NoSpill::MergeRunPulled, NoSpill::UnionArmRun, NoSpill::KeyCopies,
        NoSpill::TwoPassPass1, NoSpill::SetOpRightSide, NoSpill::SetOpSortedSide,
        NoSpill::ConstantPastCap, NoSpill::BuildPartitionPastGrant, NoSpill::BuildLeaderMerge,
        NoSpill::BuildSpillMerge, NoSpill::AggOwnerAll,
        NoSpill::AggPartitionPastGrant, NoSpill::LoadSealPastLine,
        NoSpill::Unrouted,
    ];

    /// The reason whose `code` is `c`; `None` for a code no reason has.
    pub const fn from_code(c: u8) -> Option<NoSpill> {
        if (c as usize) < NoSpill::ALL.len() {
            Some(NoSpill::ALL[c as usize])
        } else {
            None
        }
    }

    /// The sites whose descriptor keeps decompose's declared spill (`Breaker::spill`) while the lowered code grows: the
    /// one list `agrees` admits a disagreement for. A site leaves it when it spills or its descriptor says `Spill::None`.
    pub const fn keeps_declared(self) -> bool {
        use NoSpill::*;
        !matches!(self, KeyCopies | TwoPassPass1 | ConstantPastCap | LoadSealPastLine | Unrouted)
    }

    /// The one plain sentence a user reads for this reason: its SPILL-DEBT row's field 8 (lint/SPILL-DEBT/<reason>, which
    /// this crate's `limitation_is_field_8` test holds it to; `Unrouted` has no row). Exec glue writes it in the server log
    /// beside an out of memory the reason names (driver `Error::OutOfMemory::why`).
    pub const fn limitation(self) -> &'static str {
        use NoSpill::*;
        match self {
            BuildNoProbe => "No known query reaches this, but a hash join whose probe Gravity could not find would keep its whole hash table in memory where PostgreSQL would spill it to disk, so the statement fails with an out-of-memory error when the server has no more memory to give it.",
            BuildNestInner => "A hash join that is itself the inner side of a nested loop or of a correlated subquery keeps its whole hash table in memory where PostgreSQL would spill it to disk, so the statement fails with an out-of-memory error when the server has no more memory to give it.",
            BuildNoProbeOp => "No query reaches this: the probe pipeline Gravity finds for a hash build always holds a probe of that build.",
            BuildGuarded => "An ORDER BY with a sort operator written in SQL or a procedural language over a hash join fails with an error once the join's hash table passes PostgreSQL's one-batch size (work_mem times hash_mem_multiplier), where PostgreSQL would split the join into batches on disk.",
            BuildSwapped => "An inner hash join of two table scans where Gravity builds the hash table from the side PostgreSQL probes, because that side is smaller, keeps its whole hash table in memory where PostgreSQL would spill it to disk, so the statement fails with an out-of-memory error when the server has no more memory to give it.",
            BuildCompact => "A hash join on an integer key dense enough for Gravity's compact table form keeps its whole hash table in memory where PostgreSQL would spill it to disk, so the statement fails with an out-of-memory error when the server has no more memory to give it.",
            BuildFaceOff => "No query reaches this: the hash build's spill form is always built in, so a build that may spill always gets its partitions.",
            ArmsNotOneRun => "A hash join whose probe side is a UNION ALL nested inside another one that PostgreSQL does not flatten keeps its whole hash table in memory where PostgreSQL would spill it to disk, so the statement fails with an out-of-memory error when the server has no more memory to give it.",
            ArmsOpNotProbe => "No query reaches this: the step Gravity tests under a UNION ALL branch is always the hash probe it found there.",
            ArmsDrainEnd => "A RIGHT, FULL, RIGHT SEMI or RIGHT ANTI hash join whose probe side is a UNION ALL or a partitioned table keeps its whole hash table in memory where PostgreSQL would spill it to disk, so the statement fails with an out-of-memory error when the server has no more memory to give it.",
            ArmsApart => "A hash join whose probe side is a UNION ALL in which some branch does not probe the same hash table keeps its whole hash table in memory where PostgreSQL would spill it to disk, so the statement fails with an out-of-memory error when the server has no more memory to give it.",
            SpooledDrain => "A RIGHT or FULL hash join whose result is spooled (under a materialized CTE, an uncorrelated subquery, a hashed subquery or a Materialize) keeps its whole hash table in memory where PostgreSQL would spill it to disk, so the statement fails with an out-of-memory error when the server has no more memory to give it.",
            ProbeShape => "A nested loop that Gravity runs as a hash lookup into its inner side (a primary-key lookup, a keyed nested loop, or an inner aggregate, sort or CTE built once) keeps that lookup table in memory where PostgreSQL holds no such table, so the statement fails with an out-of-memory error when the server has no more memory to give it.",
            VolatileDomainKey => "A hash join on a key of a domain type whose CHECK constraint calls a VOLATILE function keeps its whole hash table in memory where PostgreSQL would spill it to disk, so the statement fails with an out-of-memory error when the server has no more memory to give it.",
            JoinTypeOutside => "No query reaches this: every join type PostgreSQL runs as a hash join is one Gravity spills.",
            ProbeSlotApart => "No query reaches this: Gravity always tests a hash build against the probe of that same build.",
            ConsumesMore => "A hash join whose outer side reads a finished intermediate result (the output of an aggregate, DISTINCT, sort, Materialize or CTE) or a UNION ALL with a constant, VALUES, set-returning or aggregated branch keeps its whole hash table in memory where PostgreSQL would spill it to disk, so the statement fails with an out-of-memory error when the server has no more memory to give it.",
            ChainTail => "A hash join whose rows go on in the same pipeline into another join or into a step other than a filter or projection (for example every join but the last in a chain of hash joins) keeps its whole hash table in memory where PostgreSQL would spill it to disk, so the statement fails with an out-of-memory error when the server has no more memory to give it.",
            ResidentInRerun => "A hash join inside something that runs many times (a correlated subquery, the inner side of a LATERAL join or nested loop, or a recursive CTE term) keeps its whole hash table in memory where PostgreSQL would spill it to disk, so the statement fails with an out-of-memory error when the server has no more memory to give it.",
            ProgramWrites => "A hash join in a statement that writes or locks rows (INSERT, UPDATE, DELETE, MERGE, or SELECT FOR UPDATE or FOR SHARE) keeps its whole hash table in memory where PostgreSQL would spill it to disk, so the statement fails with an out-of-memory error when the server has no more memory to give it.",
            RemadeSet => "A hash join with a correlated subquery over an aggregate, a sort or another hash join in its join condition keeps its whole hash table in memory where PostgreSQL would spill it to disk, so the statement fails with an out-of-memory error when the server has no more memory to give it.",
            DrainShape => "A RIGHT, FULL, RIGHT SEMI or RIGHT ANTI hash join whose unmatched rows Gravity cannot read in a later step (for example one on the inner side of a nested loop) keeps its whole hash table in memory where PostgreSQL would spill it to disk, so the statement fails with an out-of-memory error when the server has no more memory to give it.",
            DrainShared => "A RIGHT or FULL hash join in a chain of right hash joins or in a partitionwise join keeps its whole hash table in memory where PostgreSQL would spill it to disk, so the statement fails with an out-of-memory error when the server has no more memory to give it.",
            SinkNotEndSink => "A hash join directly under an uncorrelated scalar subquery, EXISTS, ARRAY(SELECT ...) or row comparison keeps its whole hash table in memory where PostgreSQL would spill it to disk, so the statement fails with an out-of-memory error when the server has no more memory to give it.",
            EndArmsFaceOff => "Only with the developer setting pgrust.gravity_road_off naming append.arms, a hash join whose rows go into a sort, an aggregate or another hash table keeps its whole hash table in memory where PostgreSQL would spill it to disk, so the statement fails with an out-of-memory error when the server has no more memory to give it.",
            SortNotPair => "A hash join whose rows go into some sorts (for example ORDER BY with LIMIT, or an incremental sort) keeps its whole hash table in memory where PostgreSQL would spill it to disk, so the statement fails with an out-of-memory error when the server has no more memory to give it.",
            PlainScanRegister => "A hash join whose rows go into an aggregate without GROUP BY that takes min() or max() of a dictionary-encoded column, or a numeric sum or average in some plans, keeps its whole hash table in memory where PostgreSQL would spill it to disk, so the statement fails with an out-of-memory error when the server has no more memory to give it.",
            HashAggNotPair => "A hash join whose rows go into a GROUP BY hash aggregate in some forms (for example one with min() or max() of a dictionary-encoded column) keeps its whole hash table in memory where PostgreSQL would spill it to disk, so the statement fails with an out-of-memory error when the server has no more memory to give it.",
            EndHashBuild => "A hash join whose rows become the hash table of another hash join keeps its whole hash table in memory where PostgreSQL would spill it to disk, so the statement fails with an out-of-memory error when the server has no more memory to give it.",
            EndOther => "A hash join whose rows go into a DISTINCT, some top-N steps, or a materialized result (a Materialize, a CTE or a recursive CTE term) keeps its whole hash table in memory where PostgreSQL would spill it to disk, so the statement fails with an out-of-memory error when the server has no more memory to give it.",
            EndRowOp => "A hash join whose rows go straight into an INSERT, UPDATE, DELETE or MERGE keeps its whole hash table in memory where PostgreSQL would spill it to disk, so the statement fails with an out-of-memory error when the server has no more memory to give it.",
            MergeRunPulled => "A merge join whose inner side Gravity reads one row at a time as the join asks (and a sorted INTERSECT or EXCEPT reading its right input that way) keeps every row it has read in memory where PostgreSQL's Materialize spills to disk and drops rows behind the merge, so the statement fails with an out-of-memory error when the server has no more memory to give it.",
            UnionArmRun => "A merge join whose inner side sorts or materializes a UNION ALL keeps that inner side in memory where PostgreSQL's sort or Materialize spills to disk, so the statement fails with an out-of-memory error when the server has no more memory to give it.",
            KeyCopies => "No known query reaches this: every GROUP BY and DISTINCT hash aggregate key tried (text, long text, a text expression, bytea, an integer array, numeric, jsonb, two keys together, a key with string_agg) spills its groups to disk as PostgreSQL does, and the statement answers.",
            TwoPassPass1 => "A GROUP BY that orders by count(*) DESC under a constant LIMIT, or keeps groups by HAVING count(*) against a constant, can count every key in memory in a first pass where PostgreSQL's hash aggregate spills to disk, so the statement fails with an out-of-memory error when the server has no more memory to give it.",
            SetOpRightSide => "An INTERSECT or EXCEPT that PostgreSQL runs as a hashed SetOp keeps its right input in memory where PostgreSQL only streams it past the left input's table, so it can use more memory than PostgreSQL; when the server has no more memory to give it, Gravity spills the right input to disk and the statement still answers.",
            SetOpSortedSide => "An INTERSECT or EXCEPT that PostgreSQL runs as a sorted SetOp keeps its inputs whole in memory where PostgreSQL streams both sorted inputs, so the statement fails with an out-of-memory error when the server has no more memory to give it.",
            ConstantPastCap => "An aggregate without GROUP BY whose aggregate states together pass 64 kB (thousands of aggregates in one query) keeps them in memory as PostgreSQL also does, so the statement fails with an out-of-memory error when the server has no more memory to give it.",
            BuildPartitionPastGrant => "A hash join that has written every partition of its hash table to disk keeps growing the part still in memory when that part alone passes its memory share (for example many rows with one join key), where PostgreSQL keeps splitting into more batches, so the statement fails with an out-of-memory error when the server has no more memory to give it.",
            BuildLeaderMerge => "A hash join whose hash table cannot spill also grows it past its memory share when the workers' parts are merged or the table's directory is laid out, so the statement fails with an out-of-memory error when the server has no more memory to give it.",
            BuildSpillMerge => "A parallel hash join that spills its hash table can still grow it past its memory share while the leader merges the workers' parts, where PostgreSQL's parallel hash join spills each batch, so the statement fails with an out-of-memory error when the server has no more memory to give it.",
            AggOwnerAll => "A GROUP BY or DISTINCT hash aggregate that spilled reads all of its spilled groups back at once in most plans, where PostgreSQL reads them back one batch at a time, so it can use more memory than PostgreSQL; when the server has no more memory to give it, Gravity reads the groups back through a temporary file instead, more slowly, and the statement still answers.",
            AggPartitionPastGrant => "A GROUP BY hash aggregate that spilled and reads its groups back one batch at a time still holds a whole partition in memory when that partition alone passes its memory share, where PostgreSQL splits such a batch again, so it can use more memory than PostgreSQL; when the server has no more memory to give it, Gravity holds that partition in a temporary file instead, more slowly, and the statement still answers.",
            LoadSealPastLine => "A COPY into a pgrcolumnar table seals each part it writes in memory, at about three times the part's size, where it cannot yet write that part out in pieces, so when the server has no more memory to give it the statement fails with an out-of-memory error instead of finishing more slowly.",
            Unrouted => "No site named this breaker's road past its grant (a lowering that skipped its plan): no known limitation, a defect to report.",
        }
    }
}

/// What a lowering built for a breaker's end, as `plan` reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Shape {
    /// It built this spill road.
    Spills(Spill),
    /// A program constant of these bytes.
    Constant(u32),
    /// A form PostgreSQL keeps in memory too.
    PgHolds(PgHolds),
    /// It grows past its grant, for this reason.
    Grows(NoSpill),
}

impl Holds {
    /// It spills past its grant.
    pub const fn spills(self) -> bool {
        matches!(self, Holds::Spills(_))
    }

    /// The debt it names, if any.
    pub const fn debt(self) -> Option<NoSpill> {
        match self {
            Holds::Bounded(Proof::Estimate(w)) => Some(w),
            _ => None,
        }
    }
}

/// The answer for breaker `b` built as `shape`: a constant past the cap is debt; a spill road of `Spill::None` (or no
/// partition bits) is no road, and a form PostgreSQL holds is admitted only for its breaker kind: both are a lowering's
/// Bug, answered `Unrouted` so `agrees` names it.
pub const fn plan(b: &Breaker, shape: Shape) -> Holds {
    match shape {
        Shape::Spills(Spill::None | Spill::Partitioned { bits: 0 }) => Holds::Bounded(Proof::Estimate(NoSpill::Unrouted)),
        Shape::Spills(s) => Holds::Spills(s),
        Shape::Constant(n) if n <= CONSTANT_PROOF_BYTES => Holds::Bounded(Proof::Constant(n)),
        Shape::Constant(_) => Holds::Bounded(Proof::Estimate(NoSpill::ConstantPastCap)),
        Shape::PgHolds(PgHolds::SetOpSide) if matches!(b.budget, Budget::Pool) => Holds::Bounded(Proof::PgHoldsToo(PgHolds::SetOpSide)),
        Shape::PgHolds(PgHolds::HashedSubPlan) if matches!(b.kind, BreakerKind::Distinct) => Holds::Bounded(Proof::PgHoldsToo(PgHolds::HashedSubPlan)),
        Shape::PgHolds(_) => Holds::Bounded(Proof::Estimate(NoSpill::Unrouted)),
        Shape::Grows(w) => Holds::Bounded(Proof::Estimate(w)),
    }
}

/// The answer decompose's declaration of `b` gives when no lowered end settles otherwise: a declared spill spills, a
/// fixed budget is a constant, a pool budget is a set operation's side (D254). Decompose declares no share without a
/// spill (a top over the share declares its runs, as its lowering spills them: C10, SPILL-DEBT TopKShare deleted), so
/// one is unrouted.
pub const fn declared(b: &Breaker) -> Holds {
    match (b.spill, b.budget) {
        (Spill::Partitioned { .. } | Spill::Runs, _) => plan(b, Shape::Spills(b.spill)),
        (Spill::None, Budget::Fixed(n)) => plan(b, Shape::Constant(n)),
        (Spill::None, Budget::Pool) => plan(b, Shape::PgHolds(PgHolds::SetOpSide)),
        (Spill::None, Budget::Share) => plan(b, Shape::Grows(NoSpill::Unrouted)),
    }
}

/// Whether descriptor breaker `b` and its answer `h` agree: a spill names the road `b` declares; a bounded breaker
/// declares none, or keeps its declared spill by a listed site (`NoSpill::keeps_declared`); `Unrouted` never agrees.
pub const fn agrees(b: &Breaker, h: Holds) -> bool {
    match (b.spill, h) {
        (_, Holds::Bounded(Proof::Estimate(NoSpill::Unrouted))) => false,
        (Spill::Partitioned { .. }, Holds::Spills(Spill::Partitioned { .. })) | (Spill::Runs, Holds::Spills(Spill::Runs)) => true,
        (Spill::None, Holds::Bounded(_)) => true,
        (Spill::Partitioned { .. }, Holds::Bounded(Proof::PgHoldsToo(PgHolds::HashedSubPlan))) => true,
        (Spill::Partitioned { .. } | Spill::Runs, Holds::Bounded(Proof::Estimate(w))) => w.keeps_declared(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn br(kind: BreakerKind, budget: Budget, spill: Spill) -> Breaker {
        Breaker { kind, slot: 0, bytes: 0, budget, spill }
    }

    #[test]
    fn a_declared_spill_spills_and_a_constant_is_bounded_up_to_the_cap() {
        let agg = br(BreakerKind::HashAgg, Budget::Share, Spill::Partitioned { bits: 5 });
        assert_eq!(declared(&agg), Holds::Spills(Spill::Partitioned { bits: 5 }));
        assert_eq!(declared(&br(BreakerKind::Sort, Budget::Share, Spill::Runs)), Holds::Spills(Spill::Runs));
        let plain = br(BreakerKind::PlainAgg, Budget::Fixed(64), Spill::None);
        assert_eq!(declared(&plain), Holds::Bounded(Proof::Constant(64)));
        let big = br(BreakerKind::TopK, Budget::Fixed(CONSTANT_PROOF_BYTES + 1), Spill::None);
        assert_eq!(declared(&big), Holds::Bounded(Proof::Estimate(NoSpill::ConstantPastCap)));
        let side = br(BreakerKind::HashAgg, Budget::Pool, Spill::None);
        assert_eq!(declared(&side), Holds::Bounded(Proof::PgHoldsToo(PgHolds::SetOpSide)));
        let top = br(BreakerKind::TopK, Budget::Share, Spill::Runs);
        assert_eq!(declared(&top), Holds::Spills(Spill::Runs), "a top over the share spills its runs");
        assert_eq!(declared(&br(BreakerKind::TopK, Budget::Share, Spill::None)), Holds::Bounded(Proof::Estimate(NoSpill::Unrouted)));
        for b in [agg, plain, big, side, top] {
            assert!(agrees(&b, declared(&b)), "{b:?}");
        }
    }

    #[test]
    fn a_road_of_no_spill_and_a_misplaced_pg_form_are_unrouted() {
        let sort = br(BreakerKind::Sort, Budget::Share, Spill::Runs);
        for s in [Shape::Spills(Spill::None), Shape::Spills(Spill::Partitioned { bits: 0 }), Shape::PgHolds(PgHolds::SetOpSide), Shape::PgHolds(PgHolds::HashedSubPlan)] {
            assert_eq!(plan(&sort, s), Holds::Bounded(Proof::Estimate(NoSpill::Unrouted)), "{s:?}");
            assert!(!agrees(&sort, plan(&sort, s)));
        }
        let keyed = br(BreakerKind::HashAgg, Budget::Share, Spill::None);
        assert_eq!(declared(&keyed), Holds::Bounded(Proof::Estimate(NoSpill::Unrouted)), "a keyed share with no spill names its site");
    }

    #[test]
    fn every_reason_answers_its_code() {
        assert_eq!(NoSpill::ALL.len(), NoSpill::Unrouted as usize + 1, "ALL lists every reason");
        for (c, w) in NoSpill::ALL.iter().enumerate() {
            assert_eq!(w.code() as usize, c, "{w:?}");
            assert_eq!(NoSpill::from_code(c as u8), Some(*w));
        }
        assert_eq!(NoSpill::from_code(NoSpill::ALL.len() as u8), None);
    }

    #[test]
    fn a_kept_declared_spill_agrees_only_for_a_listed_site() {
        let build = br(BreakerKind::HashBuild, Budget::Share, Spill::Partitioned { bits: 4 });
        assert!(agrees(&build, plan(&build, Shape::Grows(NoSpill::BuildGuarded))));
        assert!(agrees(&build, plan(&build, Shape::Spills(Spill::Partitioned { bits: 6 }))), "the lowered bits are the build's own");
        assert!(!agrees(&build, plan(&build, Shape::Grows(NoSpill::KeyCopies))), "a resident keyed end says Spill::None");
        assert!(!agrees(&build, Holds::Spills(Spill::Runs)));
        let set = br(BreakerKind::Distinct, Budget::Share, Spill::Partitioned { bits: 3 });
        assert!(agrees(&set, plan(&set, Shape::PgHolds(PgHolds::HashedSubPlan))));
    }
}
