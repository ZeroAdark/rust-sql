//! Strategies: a family's named fast paths. Each is declared beside its mechanism (`crate::strategies!` next to the
//! file's registry `ROWS`; one with no mechanism yet beside the face variant that names it): its bit, the name
//! `pgrust.gravity_road_off` takes (twin A/Bs and dev: the statement serves on the strategy's straight form) and its
//! premise, the `applies::Applies` token under which it engages. `STRATEGIES` lists every one in bit order.
use core::fmt;
use std::cell::Cell;

use crate::applies::Applies;

pub use crate::frame::block::MASK_WORD;
pub use crate::frame::mask_vec::MASK_VEC;
pub use crate::frame::mask_run::MASK_RUN;
pub use crate::ops::agg::trans::block_fold::MASK_FOLD;
pub use crate::ops::scan::pgrc::pgrc_late::DECODE_LATE;
pub use crate::frame::body::BODY_VERSIONED;
pub use crate::frame::body_form::BODY_FORM;
pub use crate::frame::hoist::LOOP_HOIST;
pub use crate::frame::carry::FRAME_CARRY;
pub use crate::ops::scan::pgrc::verdict_zone::VERDICT_ZONE;
pub use crate::ops::scan::pgrc::verdict_bloom::VERDICT_BLOOM;
pub use crate::ops::scan::pgrc::verdict_psma::VERDICT_PSMA;
pub use crate::ops::scan::pgrc::verdict_allmatch::VERDICT_ALLMATCH;
pub use crate::ops::scan::pgrc::lane_elide::VERDICT_ELIDE;
pub use crate::dict::strategies::verdict_coderange::VERDICT_CODERANGE;
pub use crate::ops::scan::pgrc::verdict_rtconst::VERDICT_RTCONST;
pub use crate::ops::scan::pgrc::verdict_cond::VERDICT_COND;
pub use crate::ops::scan::pgrc::readplan_claim::READ_CLAIM;
pub use crate::ops::agg::trans::answer::ANSWER_STATS;
pub use crate::ops::agg::trans::strategies::answer_counted::ANSWER_COUNTED;
pub use crate::ops::agg::trans::strategies::answer_num::ANSWER_NUMERIC;
pub use crate::dict::percode::PERCODE_TABLE;
pub use crate::dict::percode_len::PERCODE_LENGTH;
pub use crate::ops::scan::pgrc::dict_entries::DICT_ENTRIES;
pub use crate::ops::scan::pgrc::dict_open::DICT_OPEN;
pub use crate::ops::agg::numeric::strategies::num_window::{NUM_CTE, NUM_DIV, NUM_SCALE, NUM_WINDOW};
pub use crate::ops::agg::numeric::strategies::memo_num::NUM_MEMO;
pub use crate::rep::varlena::num_short::NUM_SHORT;
pub use crate::ops::agg::trans::finals_shift::FINALS_SHIFT;
pub use crate::ops::agg::trans::strategies::finals_cse::FINALS_CSE;
pub use crate::frame::width::WIDTH_WORK;
pub use crate::frame::width::WIDTH_WRITE;
pub use crate::ops::FLOOR;
pub use crate::ops::SCAN_MERGEFREE;
pub use crate::ops::SCAN_MERGEFREE_BUILD;
pub use crate::ops::GROUP_HASHCHAIN;
pub use crate::end::strategies::sink_ordered::SINK_ORDERED;
pub use crate::table::bypass::TABLE_BYPASS;
pub use crate::table::emit_runs::EMIT_PARMERGE;
pub use crate::table::size::TABLE_SIZED;
pub use crate::table::radix::TABLE_RADIX;
pub use crate::ops::agg::dense_small::TABLE_SMALL;
pub use crate::frame::prefetch::PREFETCH_TABLE;
pub use crate::frame::prefetch_fp::PREFETCH_FP;
pub use crate::ops::group::distinct_total::DISTINCT_TOTAL;
pub use crate::ops::agg::run_collapse::RUN_COLLAPSE;
pub use crate::table::hash_open::HASH_RUNOPEN;
pub use crate::dict::coderep::CODE_CARRY;
pub use crate::dict::late_render::CODE_RENDER;
pub use crate::dict::codefront::CODE_FRONT_PART;
pub use crate::dict::codefront_dense::CODE_FRONT_DENSE;
pub use crate::ops::sort::topn::strategies::twopass_having::TWOPASS_HAVING;
pub use crate::ops::sort::topn::strategies::twopass_count::TWOPASS_COUNT;
pub use crate::ops::hashjoin::strategies::dense::JOIN_DENSE;
pub use crate::ops::hashjoin::strategies::multiplicity::JOIN_MULT;
pub use crate::ops::hashjoin::strategies::rtfilter::{JOIN_RTFILTER, JOIN_RTFILTER_ARMS, JOIN_RTFILTER_MULTI};
pub use crate::ops::hashjoin::strategies::bloomtag::JOIN_BLOOMTAG;
pub use crate::ops::hashjoin::build_side::JOIN_BUILDSIDE;
pub use crate::ops::scan::strategies::heap_pages::HEAP_PAGES;
pub use crate::ops::scan::strategies::heap_stream::HEAP_STREAM;
pub use crate::step::text_like::TEXT_MEMMEM;
pub use crate::rep::varlena::cmp_text::{TEXT_EQ, TEXT_LEAN};
pub use crate::rep::varlena::text_set::{TEXT_SET, TEXT_SET_BPCHAR};
pub use crate::rep::varlena::key_canon::KEY_CANON;
pub use crate::dict::strategies::text_frame::TEXT_FRAME;
pub use crate::ops::scan::pgrc::strategies::jsonb_inplace::JSONB_INPLACE;
pub use crate::step::expr_fuse::EXPR_FUSION;
pub use crate::ops::append::strategies::arms::APPEND_ARMS;
pub use crate::ops::append::strategies::nest::APPEND_NEST;
pub use crate::ops::hashjoin::strategies::drain_wide::DRAIN_SPOOL_WIDE;
pub use crate::ops::sort::topn::strategies::owner_count::OWNER_COUNT;
pub use crate::ops::sort::strategies::bound_first::TOPK_BOUNDFIRST;
pub use crate::ops::sort::strategies::walk::TOPK_WALK;
pub use crate::ops::sort::strategies::fetch::TOPK_FETCH;
pub use crate::ops::sort::strategies::walk_width::WIDTH_WALK;
pub use crate::ops::sort::strategies::natural::SORT_NATURAL;
pub use crate::ops::sort::strategies::resident::SORT_RESIDENT;
pub use crate::rep::keylane::SORT_KEYLANE;
pub use crate::ops::group::strategies::per_group::RUN_PERGROUP;
pub use crate::ops::agg::strategies::dense_range::DENSE_RANGE;
pub use crate::ops::agg::trans::strategies::monotone::EXPR_MONOTONE;
pub use crate::dict::strategies::key_codeif::KEY_CODEIF;
pub use crate::dict::strategies::code_sparse::CODE_SPARSE;
pub use crate::dict::strategies::codefront_light::CODE_FRONT_LIGHT;
pub use crate::ops::nestloop::strategies::catalog_left::NEST_CATALOG_LEFT;
pub use crate::dict::strategies::code_cut::CODE_CUT;
pub use crate::dict::strategies::heads::CODE_HEADS;
pub use crate::rep::varlena::hash_tagged::KEY_TAGGED;
pub use crate::rep::varlena::coll::KEY_COLL;
pub use crate::table::keyimg::KEY_SURROGATE;
pub use crate::ops::agg::strategies::dependent::KEY_DEPENDENT;
pub use crate::ops::agg::strategies::dependent::KEY_CARRY;
pub use crate::ops::agg::numeric::strategies::num_case::CASE_SELECT;
pub use crate::ops::scan::pgrc::strategies::jsonb_lanes::JSONB_LANES;
pub use crate::ops::scan::pgrc::strategies::jsonb_codes::JSONB_CODES;
pub use crate::table::face::DRAIN_PARTS;
pub use crate::table::face::MARKS_TESTSET;
pub use crate::table::face::SEMIJOIN_MARKS;
pub use crate::ops::hashjoin::face::SEMIJOIN_RUNS;
pub use crate::ops::hashjoin::face::JOIN_EMIT_BUILD;
pub use crate::face::runs::NEST_SELFSPOOL;
pub use crate::ops::hashjoin::face::NEST_EQKEY;
pub use crate::ops::hashjoin::face::NEST_BULK;
pub use crate::ops::hashjoin::face::{NEST_KEEP_INITPLAN, NEST_KEEP_MATERIAL, NEST_KEEP_SEMI, NEST_KEEP_SUBQUERY, NEST_KEY_EQ};
pub use crate::rerun::face::NEST_CACHE;
pub use crate::face::consts::NEST_EAGERSAFE;
pub use crate::face::end::NEST_PARALLEL;
pub use crate::face::source::NEST_BATCH;
pub use crate::face::fold::NEST_GROUPJOIN;
pub use crate::face::runs::SPOOL_ALIAS;
pub use crate::ops::srf::strategies::parallel::SRF_PARALLEL;
pub use crate::ops::srf::strategies::limit_cut::SRF_LIMITCUT;
pub use crate::table::face::SETOP_COUNT;
pub use crate::ops::setop::strategies::empty_left::SETOP_EMPTYLEFT;
pub use crate::table::face::WIN_PARTTABLE;
pub use crate::table::face::WIN_OWNEDVIEW;
pub use crate::face::runs::WIN_PARTRUNS;
pub use crate::face::runs::WIN_TOPN;
pub use crate::face::step::WIN_FRAMECLASS;
pub use crate::face::fold::WIN_INVERSE;
pub use crate::face::step::WIN_PREFIX;
pub use crate::ops::mergejoin::strategies::markable::MERGE_MARKABLE;
pub use crate::face::fold::MERGE_MULT;
pub use crate::face::consts::MERGE_SKIP;
pub use crate::ops::mergejoin::strategies::ksort::{MERGE_KSORT, MERGE_WIDEEMIT};
pub use crate::ops::modifytable::update::WRITE_IDXUNCHANGED;
pub use crate::ops::modifytable::strategies::checks_ir::WRITE_CHECKS;
pub use crate::ops::modifytable::strategies::literals::WRITE_LITERALS;
pub use crate::ops::modifytable::strategies::route::WRITE_ROUTE;
pub use crate::ops::agg::numeric::class_num::NUM_LEAN;
pub use crate::ops::agg::numeric::strategies::num_partial::NUM_PARTIAL;
pub use crate::ops::agg::numeric::strategies::num_partial::NUM_PAIR;
pub use crate::ops::agg::numeric::strategies::num_share::NUM_SHARE;
pub use crate::ops::agg::numeric::strategies::num_narrow::NUM_NARROW;
pub use crate::rep::varlena::cmp_aggnum::NUM_AGGCMP;
pub use crate::ops::hashjoin::strategies::domain::JOIN_DOMAIN;
pub use crate::ops::hashjoin::strategies::rowfilter::{JOIN_ROWFILTER, JOIN_ROWFILTER_COST, JOIN_ROWFILTER_SINGLE};
pub use crate::ops::hashjoin::strategies::probe_stage::JOIN_PROBE_STAGED;
pub use crate::frame::mask_rows::MASK_ROWS;
pub use crate::table::emit_group::EMIT_MARKS;
pub use crate::ops::ctescan::lower::{CTE_BATCH, CTE_CLAIMS};
pub use crate::ops::agg::trans::class_stats::STAT_REG;
pub use crate::rerun::spool::seal::SPOOL_SEAL;
pub use crate::ops::agg::cascade::GSETS_CASCADE;
pub use crate::ops::ctescan::wholefill::CTE_WHOLEFILL;
pub use crate::face::partfill::WIN_PARTFILL;
pub use crate::ops::hashjoin::strategies::dedup::JOIN_DEDUP;
pub use crate::rep::varlena::substr_ascii::TEXT_SUBSTR_ASCII;
pub use crate::ops::hashjoin::strategies::payload_fp::{JOIN_DRAIN_FP, JOIN_PAYLOAD_FP};

/// Every strategy, in rising bit order from 0 (`Strategy::declare`'s bits: checked below); a deleted road's bit stays free,
/// so no other road's bit moves.
pub const STRATEGIES: &[Strategy] = &[
    MASK_WORD, MASK_VEC, MASK_RUN, MASK_FOLD, DECODE_LATE, BODY_VERSIONED, BODY_FORM, LOOP_HOIST, FRAME_CARRY,
    VERDICT_ZONE, VERDICT_BLOOM, VERDICT_PSMA, VERDICT_ALLMATCH, VERDICT_ELIDE, VERDICT_CODERANGE, VERDICT_RTCONST,
    VERDICT_COND, READ_CLAIM, ANSWER_STATS, ANSWER_COUNTED, ANSWER_NUMERIC, PERCODE_TABLE, PERCODE_LENGTH,
    DICT_ENTRIES, DICT_OPEN, NUM_WINDOW, NUM_MEMO, NUM_SHORT, FINALS_SHIFT, FINALS_CSE, WIDTH_WORK, WIDTH_WRITE,
    FLOOR, SINK_ORDERED, TABLE_BYPASS, TABLE_SIZED, TABLE_RADIX, TABLE_SMALL, PREFETCH_TABLE, PREFETCH_FP,
    JOIN_DRAIN_FP, DISTINCT_TOTAL, RUN_COLLAPSE, HASH_RUNOPEN, CODE_CARRY, CODE_RENDER, CODE_FRONT_PART,
    CODE_FRONT_DENSE, TWOPASS_HAVING, JOIN_DEDUP, JOIN_DENSE, JOIN_MULT, JOIN_RTFILTER, JOIN_BLOOMTAG,
    JOIN_BUILDSIDE, JOIN_PAYLOAD_FP, HEAP_PAGES, HEAP_STREAM, TEXT_MEMMEM, TEXT_EQ, TEXT_FRAME, JSONB_INPLACE,
    EXPR_FUSION, APPEND_ARMS, OWNER_COUNT, TOPK_BOUNDFIRST, TOPK_WALK, TOPK_FETCH, WIDTH_WALK, SORT_NATURAL,
    SORT_RESIDENT, SORT_KEYLANE, RUN_PERGROUP, DENSE_RANGE, EXPR_MONOTONE, KEY_CODEIF, CODE_SPARSE, CODE_FRONT_LIGHT,
    NEST_CATALOG_LEFT, CODE_CUT, TEXT_SUBSTR_ASCII, CODE_HEADS, KEY_TAGGED, KEY_COLL, KEY_SURROGATE, KEY_DEPENDENT,
    JSONB_LANES, JSONB_CODES, DRAIN_PARTS, MARKS_TESTSET, SEMIJOIN_MARKS, SEMIJOIN_RUNS, JOIN_EMIT_BUILD,
    NEST_SELFSPOOL, NEST_EQKEY, NEST_BULK, NEST_CACHE, NEST_EAGERSAFE, NEST_PARALLEL, NEST_BATCH, NEST_GROUPJOIN,
    SPOOL_ALIAS, SRF_PARALLEL, SRF_LIMITCUT, SETOP_COUNT, SETOP_EMPTYLEFT, WIN_PARTTABLE, WIN_OWNEDVIEW, WIN_PARTRUNS,
    WIN_TOPN, WIN_FRAMECLASS, WIN_INVERSE, WIN_PREFIX, MERGE_MARKABLE, MERGE_MULT, MERGE_SKIP, MERGE_KSORT,
    WRITE_IDXUNCHANGED, WRITE_CHECKS, WRITE_LITERALS, WRITE_ROUTE, NUM_LEAN, NUM_PARTIAL, NUM_PAIR, NUM_SHARE,
    NUM_NARROW,
    // bit 126: free (road run.measure, deleted by D4941)
    NUM_AGGCMP, JOIN_DOMAIN, JOIN_ROWFILTER, JOIN_RTFILTER_MULTI, SCAN_MERGEFREE, EMIT_MARKS,
    CTE_CLAIMS, CTE_BATCH, SCAN_MERGEFREE_BUILD,
    KEY_CARRY, MERGE_WIDEEMIT, EMIT_PARMERGE,
    NEST_KEEP_INITPLAN, NEST_KEEP_SUBQUERY,
    APPEND_NEST, JOIN_RTFILTER_ARMS,
    NEST_KEEP_SEMI, NEST_KEEP_MATERIAL,
    CASE_SELECT, DRAIN_SPOOL_WIDE,
    STAT_REG, NUM_SCALE,
    JOIN_ROWFILTER_SINGLE, JOIN_ROWFILTER_COST,
    JOIN_PROBE_STAGED, TEXT_SET_BPCHAR, NEST_KEY_EQ,
    GROUP_HASHCHAIN,
    SPOOL_SEAL,
    TEXT_LEAN,
    GSETS_CASCADE,
    TEXT_SET,
    NUM_DIV,
    KEY_CANON,
    CTE_WHOLEFILL,
    NUM_CTE,
    WIN_PARTFILL,
    MASK_ROWS,
    TWOPASS_COUNT,
];

/// Declares strategies beside their mechanism: `NAME = bit, "dotted.name", Premise;`, each with its doc; the bit is the
/// next free one (`STRATEGIES` lists it in that place).
#[macro_export]
macro_rules! strategies {
    ($($(#[$m:meta])* $c:ident = $bit:literal, $name:literal, $premise:ident;)*) => {
        $($(#[$m])* pub const $c: $crate::face::strategy::Strategy = $crate::face::strategy::Strategy::declare($bit, $name, $crate::applies::Applies::$premise);)*
    };
}

/// A strategy: its bit in a `StrategySet`, its name and its premise.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Strategy {
    bit: u8,
    name: &'static str,
    premise: Applies,
}

impl Strategy {
    /// One past the last strategy's bit (a free bit counts): the bound of an array indexed by `bit`.
    pub const COUNT: usize = STRATEGIES[STRATEGIES.len() - 1].bit as usize + 1;

    pub const fn declare(bit: u8, name: &'static str, premise: Applies) -> Strategy {
        Strategy { bit, name, premise }
    }

    pub const fn bit(self) -> usize {
        self.bit as usize
    }

    /// The name a `gravity_road_off` value and a diag line carry (`verdict.bloom`).
    pub const fn name(self) -> &'static str {
        self.name
    }

    /// The properties under which it engages (`applies::holds`).
    pub const fn premise(self) -> Applies {
        self.premise
    }

    pub fn from_name(s: &str) -> Option<Strategy> {
        STRATEGIES.iter().copied().find(|d| d.name == s)
    }

    pub fn all() -> impl Iterator<Item = Strategy> {
        STRATEGIES.iter().copied()
    }
}

impl PartialOrd for Strategy {
    fn partial_cmp(&self, o: &Strategy) -> Option<core::cmp::Ordering> {
        Some(self.cmp(o))
    }
}

impl Ord for Strategy {
    fn cmp(&self, o: &Strategy) -> core::cmp::Ordering {
        self.bit.cmp(&o.bit)
    }
}

impl fmt::Debug for Strategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name)
    }
}

const _: () = {
    assert!(Strategy::COUNT <= 256 && STRATEGIES[0].bit == 0);
    let mut i = 1;
    while i < STRATEGIES.len() {
        assert!(STRATEGIES[i].bit > STRATEGIES[i - 1].bit, "STRATEGIES lists the declarations in rising bit order, from 0");
        i += 1;
    }
};

/// A set of strategies: strategy `r` at bit `r.bit() % 128` of word `r.bit() / 128`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct StrategySet(pub [u128; 2]);

impl StrategySet {
    pub const EMPTY: StrategySet = StrategySet([0; 2]);

    pub const fn has(self, r: Strategy) -> bool {
        self.0[r.bit() / 128] & (1u128 << (r.bit() % 128)) != 0
    }

    pub fn add(&mut self, r: Strategy) {
        self.0[r.bit() / 128] |= 1u128 << (r.bit() % 128);
    }

    pub fn remove(&mut self, r: Strategy) {
        self.0[r.bit() / 128] &= !(1u128 << (r.bit() % 128));
    }

    /// The strategies of `self` not in `o`.
    pub const fn minus(self, o: StrategySet) -> StrategySet {
        StrategySet([self.0[0] & !o.0[0], self.0[1] & !o.0[1]])
    }

    pub fn iter(self) -> impl Iterator<Item = Strategy> {
        STRATEGIES.iter().copied().filter(move |r| self.has(*r))
    }

    pub const fn len(self) -> u32 {
        self.0[0].count_ones() + self.0[1].count_ones()
    }

    pub const fn is_empty(self) -> bool {
        self.0[0] == 0 && self.0[1] == 0
    }

    /// `gravity_road_off`'s value (`name,name,..`); an unknown name is returned as the error, nothing is set.
    pub fn parse(s: &str) -> Result<StrategySet, &str> {
        let mut set = StrategySet::EMPTY;
        for id in s.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            set.add(Strategy::from_name(id).ok_or(id)?);
        }
        Ok(set)
    }
}

leader_tls::leader_tls! {
    thread_local! {
        /// This backend's forced-off strategies (`gravity_road_off`, set by the executor's knob; twin A/Bs only), the
        /// default-off roads (road_off.rs `DEFAULT_OFF`) until an option is read.
        static OFF: Cell<StrategySet> = const { Cell::new(crate::road_off::default_off()) };
    }
}

pub fn set_off(set: StrategySet) {
    OFF.with(|c| c.set(set));
}

pub fn off() -> StrategySet {
    OFF.with(Cell::get)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Strategy `r` is bit `r.bit() % 128` of word `r.bit() / 128`: add / has / remove / minus / len keep every strategy
    /// apart, and every name is one strategy's.
    #[test]
    fn every_strategy_is_its_own_bit_and_name() {
        let mut all = StrategySet::EMPTY;
        Strategy::all().for_each(|r| all.add(r));
        assert_eq!(all.len() as usize, STRATEGIES.len());
        assert!(Strategy::all().all(|r| all.has(r) && Strategy::from_name(r.name()) == Some(r)));
        let (mut s, mut t) = (StrategySet::EMPTY, StrategySet::EMPTY);
        s.add(NUM_PAIR);
        s.add(NUM_PARTIAL);
        assert!(s.has(NUM_PAIR) && s.has(NUM_PARTIAL) && !s.has(NUM_LEAN));
        assert_eq!(s.len(), 2);
        t.add(NUM_PAIR);
        assert_eq!(s.minus(t).iter().collect::<Vec<_>>(), [NUM_PARTIAL]);
        s.remove(NUM_PARTIAL);
        assert!(s == t && !s.is_empty() && s.minus(t).is_empty());
        assert_eq!(StrategySet::parse("num.pair,num.partial").map(StrategySet::len), Ok(2));
        assert_eq!(StrategySet::parse("num.pair,no.such"), Err("no.such"));
    }
}
