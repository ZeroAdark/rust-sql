//! Gravity Program: immutable, content-keyed code + layout + sites + write plan (docs/gravity/program.md).

pub mod arena;
pub mod cache;
pub mod externs;
pub mod code;
pub mod cost;
#[cfg(feature = "hand")]
pub mod hand;
pub mod laneset;
pub mod lifetime;
pub mod lives;
pub mod program;
pub mod rep;
pub mod spill;
pub mod twin;

pub use arena::{Arena, ArenaBox, ArenaRef, ArenaVec, Mark, Scope};
/// planprod P1-b: the list-allocator bound of the descriptors generic over their lists' home (`protos::V4Descs<A>`,
/// `lifetime::Lifetimes<A>`), for readers that take either form.
pub use allocator_api2::alloc::Allocator;
pub use cache::{CacheStats, Generation, Handle, PlanKey, ProgramCache};
// NEW-72 entry B: a bound zone predicate's forms (`ZonePred::bound`), on a line of their own
pub use program::{BOUND_FIXED_HASH, BOUND_FIXED_PREFIX, BOUND_TEXT_HASH, BOUND_TEXT_PREFIX};
// construct C2 D1: an exact licence's proof (`LicenceStamp::Exact`), on a line of its own
pub use program::{ProofCheck, ProofStamp};
pub use code::{Code, Entry, Owner};
pub use laneset::LaneSet;
pub use lives::{Life, LifeBytes, LifeId};
pub use rep::{DictRef, ExprLife, ExprOut, FnLife, Home, LaneLife, NumTypmod, NumForm, Rep, Until, NUM_WORD_DIGITS};
pub use program::{record_layout, record_layout_at, shred_kind, table_fn, ClaimKind, Cmd, Conflict, ContentKey, End, ExportKeys, GranuleMarks, IndexPlan, KeyCheck, CheckStep, CHECK_ADD, CHECK_CAST, CHECK_KEY_NULLABLE, CHECK_MUL, CHECK_NEG, CHECK_NONE, CHECK_RSUB, CHECK_SUB, KEY_CHECK_KEYS, LaneDesc, LaneId, Layout, LockKind, MergeAttnos, OutCol, OwnerCtor, OwnerDesc, RUNS_TOP_HEAP, Parallel, Pipeline, Program, Reason, RouteLeaf, RoutePlan, ShredDecl, SiteDesc, SiteKind, SlotPlan, TableDecl, TopKDesc, TwoPassDesc, WaitPolicy, WordsDesc, WritePlan, ZoneOp, ZonePred, AheadTag, LicenceStamp, END_RATE_NONE, END_RATE_ONE, ACC_PAIRS, ACC_SLOT_BASE, MAX_REPS, NULL_ARG_NONE, OBSERVE_KEY, OBSERVE_KEY_FLAGGED, OBSERVE_NONE, TIE_FLAGS_NONE, RANGE_KEYS, REP_NONE};
pub use program::{bind_estimates, estimates_out, heap_width, source_serial, step_serial, NestProbe, NestShape, PipeEst, SourceShape, StepShape};
pub mod protos;

/// The sanity bounds by contract (milestone-2.md §2.6 item 30): named refusals, never array sizes. Each is at least
/// PostgreSQL's own limit on what it counts, or the id space of what it counts; the assertions below the module hold it.
pub mod limits {
    /// A scan's lanes are one relation's columns (decompose `Atts`, one per scan): PostgreSQL's MaxHeapAttributeNumber
    /// (1600) bounds them, so no relation PostgreSQL accepts reaches this bound.
    pub const LANES: usize = 4096;
    /// An index's key columns: PostgreSQL's INDEX_MAX_KEYS (32).
    pub const KEYS: usize = 32;
    /// The indexes of a written relation (decompose modifytable `index_set`; every table over them is a Vec sized by the
    /// relation): PostgreSQL sets no bound of its own, and a routed leaf's index count is a `u16` (`RouteLeaf.nindexes`,
    /// the facts' `LeafFact.nindexes`), so the bound is that count's space.
    pub const INDEXES: usize = u16::MAX as usize;
    /// A pipeline's id is a `u16` (`LifeId`, `Proto.pipeline`, a breaker's producer): the bound is that id space, never a
    /// table size (every per-pipeline table is a Vec sized by the statement).
    pub const PIPELINES: usize = u16::MAX as usize;
    /// A write's result relations: the target index is a `u16` (decompose `Target.target`, exec admit, hosts write_check).
    pub const RESULT_RELATIONS: usize = u16::MAX as usize;
    /// The breaker slots live at once: a slot is the breaker record's `u8` (`gravity_abi::breaker::Breaker::slot`), and it
    /// is free again after its last reader (decompose `breakers::free_slot`), so this bounds the breakers held at one
    /// moment, never a statement's breakers.
    pub const BREAKERS: usize = 1 << u8::BITS;
    /// A routed write's admitted partition leaves (`WritePlan.route`; the route host's tables are Vecs sized by them):
    /// as many as a write's result relations.
    pub const ROUTE_LEAVES: usize = RESULT_RELATIONS;
    /// The word plan's bound (lower words.rs; exec admit.rs refuses a statement past it by this name). Every binding's words
    /// are sized by its statement, never by this. `1 << 15`: a plain word index stays below lowerv4's executor-key flag
    /// (`EXEC_KEY`) and fits an `i16` param slot (lowerv4 ops/scan/tid.rs asserts both); an InitPlan's handle word must also
    /// fit its place's 14-bit field, which lowerv4 rerun/params.rs `place` checks where the handle is made.
    pub const PARAM_WORDS: usize = 1 << 15;
    /// The kernel words (`WordsDesc` slots, `u16`) are the first words of the word plan (lower rerun/words.rs `reserve`),
    /// so their bound is the word plan's.
    pub const KERNEL_WORDS: usize = PARAM_WORDS;
}

const _: () = {
    assert!(limits::LANES >= 1600, "a relation's columns: MaxHeapAttributeNumber");
    assert!(limits::KEYS == 32, "an index's key columns: INDEX_MAX_KEYS");
    assert!(limits::PIPELINES == u16::MAX as usize && limits::RESULT_RELATIONS == u16::MAX as usize, "the u16 id spaces");
    assert!(limits::INDEXES == u16::MAX as usize, "the u16 index count");
    assert!(limits::ROUTE_LEAVES == limits::RESULT_RELATIONS);
    assert!(limits::BREAKERS == u8::MAX as usize + 1, "the breaker record's u8 slot");
    assert!(limits::KERNEL_WORDS <= limits::PARAM_WORDS);
};
