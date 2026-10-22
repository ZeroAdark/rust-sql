//! The nest face (plan §3.1; H07e H29 H30 H31): an inner range re-run per outer row or generation.
//! - Its parameter words are written by their owner before a re-run (H29). How a step re-runs is step.rs `Rescan`.
//! - A result cache keyed by the parameters (H30).
//! - Index probes that seek per parameter without a re-open (H31).
//! - An InitPlan's value is computed lazily at first read, as PG does (D237). It feeds open-time verdicts only when
//!   eager-safe (H07 e, consts.rs `RtConst::InitPlan`). A SubPlan under a conditional expression, where it is read
//!   (`SubEval`).
//!
//! Nest kinds (face Nest) are made by `frame::StepMaker` and `frame::SourceMaker`. A re-run re-opens only the declared
//! per-open words: no grant draw, no section open, no pool wake.
use gravity_ir::builder::FunctionBuilder;

use crate::frame::env::Env;
use crate::frame::open::OpenWord;
use crate::frame::row::RowVals;

/// nest.subplan B2c (L0-e): a lazy SubPlan evaluated where its reader reaches it, into its words (`Env::subplans`' `lent`).
pub struct SubEval(pub for<'e, 'p> fn(&mut FunctionBuilder<'_>, &mut Env<'e, 'p>, usize, &RowVals));

/// D3374: whether the lazy SubPlan lent at the address may run where PG skips it, for an interpreter island's read: it has
/// no effect and raises nothing but its second row, which `SubEval` under `Env::hold_rows` holds in its flags word.
pub struct SubHolds(pub fn(usize) -> bool);

crate::hook_variants! {
    /// Who writes a re-run's parameter words (H29).
    "H29" pub enum ParamOwner {
        /// NestLoopParam: the outer row's columns.
        NestParam = Kind(NestParam),
        /// A SubPlan's arguments, per call.
        SubplanArgs = Kind(NestSubplan),
        /// wtParam: the recursive union's generation.
        WorkTable = Kind(SetopRecursive),
        /// Rerun sets from chgParam (decompose `declare_rerun`), marked in the program (ENTRY E2).
        Rerun = Kind(NestRerun),
    }
}

/// One parameter word of a re-run: the frame word its owner writes before each re-run, and the PARAM_EXEC slot it carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParamWord {
    pub owner: ParamOwner,
    pub word: OpenWord,
    pub paramid: u16,
}

crate::strategies! {
    /// Param-keyed result cache for stable reruns. No mechanism yet: its readers take the straight form.
    NEST_CACHE = 96, "nest.cache", Subplan;
}

crate::hook_variants! {
    /// Result caches keyed by parameters (H30).
    "H30" pub enum CacheUse {
        /// Memoize, with the hit path in IR.
        Memoize = Kind(NestMemo),
        /// A stable SubPlan's result cache for its reruns.
        Subplan = Strategy(NEST_CACHE),
        /// The hashed-ANY set of a hashed SubPlan.
        HashedAny = Kind(NestSubplan),
        /// An adaptive bulk build of a hot inner (H18 b / H30).
        Bulk = Strategy(NEST_BULK),
    }
}

/// A result cache as PG's Memoize runs it (H30).
/// - It is keyed by `params` parameter words, compared as PG compares them: by the hash opclass's equality, or bytewise
///   under `binary_mode`.
/// - Its payload is a row range over one row array.
/// - It is an LRU under `budget` bytes (`hash_mem`), with evictions counted for EXPLAIN.
/// - Empty results are cached, and `singlerow` completes an entry after one row.
///
/// Off in an instrumented lowering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CacheDecl {
    pub params: u8,
    pub budget: u64,
    pub singlerow: bool,
    pub binary_mode: bool,
}

crate::hook_variants! {
    /// Index probes (H31) and the outer forms of a nest (H05 b).
    "H31 H03 H05" pub enum ProbeForm {
        /// A seek per parameter, without a re-open.
        Seek = Kind(NestParam),
        /// Inner rows in windows, not one host call per row.
        Windows = Kind(SrcIndex),
        /// Batched ordered probes: descent reuse on ascending keys, TID-driven heap prefetch (H03 c).
        Batch = Strategy(NEST_BATCH),
        /// A 64-row window of outer keys, or an IN list, as one native array-key scan. Rows are mapped back to their outer
        /// row, or left in seek order when the consumer is order-free.
        ArrayKeys = Kind(SrcIndexArray),
        /// H05 b: a parallel outer, with per-worker inner state.
        Parallel = Strategy(NEST_PARALLEL),
    }
}
