//! The dispatch (PLAN-v5-constructs #24): one match per kind and per road, written once. A family file registers its
//! implementations in its own `pub(crate) const ROWS: Table` (`..crate::ops::NONE` when it registers nothing), each value of
//! the face type its reader names (`need_kind::<T>` / `need_strategy::<T>`; `&()` for a road the face only `require`s), and
//! its arm below names each row. Families meet only through the faces and this module, never by naming another family's
//! file.
use core::any::Any;

use crate::face::kinds::{KindId, KindSet};
use crate::face::strategy::{self, Strategy, StrategySet};
use crate::admit::refusal::{Reason, Site, Variant};
pub mod agg;
pub mod append;
pub mod ctescan;
pub mod group;
pub mod hashjoin;
pub mod limit;
pub mod lockrows;
pub mod material;
pub mod memoize;
pub mod mergejoin;
pub mod modifytable;
pub mod nestloop;
pub mod result;
pub mod scan;
pub mod setop;
pub mod shared;
pub mod sort;
pub mod srf;
pub mod subplan;
pub mod values;
pub mod window;

/// An implementation as a file's rows hold it; the face that reads it names its type (`need_kind::<T>`).
pub type Mech = &'static (dyn Any + Send + Sync);

/// An AB road its twin A/B dropped: the cell and the ratio that decided it.
#[derive(Clone, Copy, Debug)]
pub struct Dropped {
    pub road: Strategy,
    pub cell: &'static str,
    pub ratio: &'static str,
}

/// The AB roads a twin A/B dropped (a dropped road never applies).
pub const DROPPED: &[Dropped] = &[];

/// A file's rows: the kinds and roads it implements.
pub struct Table {
    pub kinds: &'static [(KindId, Mech)],
    pub strategies: &'static [(Strategy, Mech)],
}

/// A file's rows when it registers nothing (a stub, or a helper file).
pub(crate) const NONE: Table = Table { kinds: &[], strategies: &[] };

crate::strategies! {
    /// Per-statement floor: front key, bound hosts, bind fast path, image warm, cold budget.
    FLOOR = 32, "floor", All;
    /// D4568: a pgrc scan's merge mark whose order no reader above reads is dropped, so the scan runs wide with its code
    /// lanes (decompose ops/scan `merge_unread`, read through hosts `Facts::scan_mergefree` before the lowering runs).
    /// On by default (D4585: the narrowed road, a serial session keeps the mark); off under its plain name.
    SCAN_MERGEFREE = 131, "scan.mergefree", Pgrc;
    /// D4585: the hash-build half of `scan.mergefree`, default off (road_off.rs `DEFAULT_OFF`): a hash build input's unread
    /// merge mark is dropped too (decompose facts/order.rs `merge_free_under`, hosts `Facts::scan_mergefree_build`).
    SCAN_MERGEFREE_BUILD = 135, "scan.mergefree.build", Pgrc;
    /// D4676: PostgreSQL's Sort -> Unique -> sorted GroupAggregate chain served hashed, default off (road_off.rs
    /// `DEFAULT_OFF`): the Unique absorbs its key Sort and hands its rows on unordered, and the group orders only its own
    /// groups where a reader reads that order (decompose ops/group/absorb.rs `chain_of`, read through hosts
    /// `Facts::road_on` before the lowering runs). The kinds it makes are the existing hashed group's and distinct's.
    GROUP_HASHCHAIN = 154, "group.hashchain", Sort;
}

/// The roads built outside lowerv4 (`Xc`), which the face only `require`s: the per-statement floor (H16, gravity_driver),
/// D4568's dropped merge mark and D4676's hashed sorted chain (decompose).
pub(crate) const XC: Table = Table { strategies: &[(FLOOR, &()), (SCAN_MERGEFREE, &()), (SCAN_MERGEFREE_BUILD, &()), (GROUP_HASHCHAIN, &())], ..NONE };

/// Kind `k`'s rows, first row first: `kind` answers the first, `kind_as` / `need_kind` the first of the face's type.
pub fn kind_rows(k: KindId) -> &'static [Mech] {
    match k {
        KindId::ClassCollect => const { &[crate::ops::agg::trans::class_collect::ROWS.kinds[0].1] },
        KindId::ClassFloat => const { &[crate::ops::agg::trans::class_float::ROWS.kinds[0].1] },
        KindId::ClassMinmaxCode => const { &[crate::dict::minmax_code::ROWS.kinds[0].1] },
        KindId::ClassNumPacked => const { &[crate::ops::agg::numeric::class_num::ROWS.kinds[0].1] },
        KindId::ClassNumVarlena => const { &[crate::ops::agg::numeric::class_num_varlena::ROWS.kinds[0].1] },
        KindId::ClassNumWide => const { &[crate::ops::agg::numeric::class_num_wide::ROWS.kinds[0].1] },
        KindId::ClassOrdered => const { &[crate::ops::agg::trans::class_ordered::ROWS.kinds[0].1] },
        KindId::ClassOther => const { &[crate::ops::agg::trans::class_other::ROWS.kinds[0].1] },
        KindId::ClassStats => const { &[crate::ops::agg::trans::class_stats::ROWS.kinds[0].1] },
        KindId::ClassText => const { &[crate::ops::agg::trans::class_text::ROWS.kinds[0].1] },
        KindId::ClassUser => const { &[crate::ops::agg::trans::class_user::ROWS.kinds[0].1] },
        KindId::ClassWord => const { &[crate::ops::agg::trans::class_word::ROWS.kinds[0].1] },
        KindId::EndSink => const { &[crate::end::sink::ROWS.kinds[0].1] },
        KindId::ExprCore => const { &[crate::step::expr::ROWS.kinds[0].1] },
        KindId::ExprSaop => const { &[crate::step::expr_saop::ROWS.kinds[0].1] },
        KindId::ExprVolatile => const { &[crate::step::expr_volatile::ROWS.kinds[0].1] },
        KindId::ExprWholerow => const { &[crate::step::expr_row::ROWS.kinds[0].1] },
        KindId::FoldFilter => const { &[crate::ops::agg::trans::filter::ROWS.kinds[0].1] },
        KindId::FoldPlain => const { &[crate::ops::agg::trans::plain::ROWS.kinds[0].1, crate::ops::agg::trans::plain_drain::ROWS.kinds[0].1] },
        KindId::JoinAnti => const { &[crate::ops::hashjoin::probe_anti::ROWS.kinds[0].1] },
        KindId::JoinChain => const { &[crate::ops::hashjoin::chain::ROWS.kinds[0].1] },
        KindId::JoinDrain => const { &[crate::ops::hashjoin::drain::lower::ROWS.kinds[0].1, crate::ops::hashjoin::drain::lower::ROWS.kinds[1].1, crate::ops::hashjoin::drain::lower::ROWS.kinds[2].1] },
        KindId::JoinFull => const { &[crate::ops::hashjoin::drain::probe_full::ROWS.kinds[0].1] },
        KindId::JoinInner => const { &[crate::ops::hashjoin::build::ROWS.kinds[0].1, crate::ops::hashjoin::rt::ROWS.kinds[0].1, crate::ops::hashjoin::spill::build::ROWS.kinds[0].1, crate::ops::hashjoin::drain::source::ROWS.kinds[0].1, crate::ops::hashjoin::probe::ROWS.kinds[0].1, crate::ops::hashjoin::probe_wait::ROWS.kinds[0].1] },
        KindId::JoinLeft => const { &[crate::ops::hashjoin::probe_left::ROWS.kinds[0].1] },
        KindId::JoinRight => const { &[crate::ops::hashjoin::drain::probe_right::ROWS.kinds[0].1] },
        KindId::JoinSemi => const { &[crate::ops::hashjoin::probe_semi::ROWS.kinds[0].1] },
        KindId::KeyedDistinctGrouped => const { &[crate::ops::group::pairs::ROWS.kinds[0].1] },
        KindId::KeyedDistinctMulti => const { &[crate::ops::group::distinct_multi::ROWS.kinds[0].1] },
        KindId::KeyedDistinctPlain => const { &[crate::ops::group::distinct_plain::ROWS.kinds[0].1, crate::ops::group::distinct_plain::ROWS.kinds[1].1] },
        KindId::KeyedEmitGroup => const { &[crate::table::emit_group::ROWS.kinds[0].1] },
        KindId::KeyedGsets => const { &[crate::ops::agg::gsets::ROWS.kinds[0].1, crate::ops::agg::gsets::ROWS.kinds[1].1, crate::ops::agg::gsets::ROWS.kinds[2].1] },
        KindId::KeyedHashagg => const { &[crate::ops::agg::hashagg::ROWS.kinds[0].1, crate::ops::group::stream::ROWS.kinds[0].1] },
        KindId::KeyedTwopass => const { &[crate::ops::sort::topn::strategies::twopass::ROWS.kinds[0].1] },
        KindId::MergeJoin => const { &[crate::ops::mergejoin::lower::ROWS.kinds[0].1, crate::ops::mergejoin::lower::ROWS.kinds[1].1] },
        KindId::NestCatalog => const { &[crate::ops::nestloop::strategies::catalog_left::ROWS.kinds[0].1] },
        KindId::NestCte => const { &[crate::ops::ctescan::lower::ROWS.kinds[0].1, crate::ops::ctescan::lower::ROWS.kinds[1].1] },
        KindId::NestInitplan => const { &[crate::ops::subplan::initplan::lower::ROWS.kinds[0].1, crate::ops::subplan::initplan::lower::ROWS.kinds[1].1] },
        KindId::NestLoop => const { &[crate::rerun::inner_arms::ROWS.kinds[0].1, crate::ops::nestloop::lower::ROWS.kinds[0].1] },
        KindId::NestMemo => const { &[crate::ops::memoize::lower::ROWS.kinds[0].1] },
        KindId::NestParam => const { &[crate::ops::nestloop::param::ROWS.kinds[0].1] },
        KindId::NestRerun => const { &[crate::rerun::sets::ROWS.kinds[0].1] },
        KindId::NestSpool => const { &[crate::ops::material::lower::ROWS.kinds[0].1, crate::ops::material::lower::ROWS.kinds[1].1, crate::ops::material::lower::ROWS.kinds[2].1] },
        KindId::NestSubplan => const { &[crate::ops::subplan::per_row::ROWS.kinds[0].1, crate::ops::subplan::per_row::ROWS.kinds[1].1, crate::ops::subplan::per_row::ROWS.kinds[2].1, crate::ops::subplan::per_row::ROWS.kinds[3].1] },
        KindId::RepCodeDatum => const { &[crate::rep::code::datum::ROWS.kinds[0].1] },
        KindId::RepCodeHash => const { &[crate::rep::code::hash::ROWS.kinds[0].1] },
        KindId::RepCodeOrder => const { &[crate::rep::code::order::ROWS.kinds[0].1] },
        KindId::RepFixedDatum => const { &[crate::rep::fixed::datum::ROWS.kinds[0].1] },
        KindId::RepFixedHash => const { &[crate::rep::fixed::hash::ROWS.kinds[0].1] },
        KindId::RepFixedOrder => const { &[crate::rep::fixed::order::ROWS.kinds[0].1] },
        KindId::RepHostedHash => const { &[crate::rep::hosted::hash::ROWS.kinds[0].1] },
        KindId::RepHostedOrder => const { &[crate::rep::hosted::order::ROWS.kinds[0].1] },
        KindId::RepHostedOrderFixed => const { &[crate::rep::hosted::order::ROWS.kinds[1].1] },
        KindId::RepPackedDatum => const { &[crate::rep::packednum::datum::ROWS.kinds[0].1] },
        KindId::RepPackedHash => const { &[crate::rep::packednum::hash::ROWS.kinds[0].1] },
        KindId::RepPackedOrder => const { &[crate::rep::packednum::order::ROWS.kinds[0].1] },
        KindId::RepVarlenaDatum => const { &[crate::rep::varlena::datum::ROWS.kinds[0].1] },
        KindId::RepVarlenaHash => const { &[crate::rep::varlena::hash::ROWS.kinds[0].1] },
        KindId::RepVarlenaOrder => const { &[crate::rep::varlena::order::ROWS.kinds[0].1] },
        KindId::RepWordDatum => const { &[crate::rep::word::datum::ROWS.kinds[0].1] },
        KindId::RepWordHash => const { &[crate::rep::word::hash::ROWS.kinds[0].1] },
        KindId::RepWordOrder => const { &[crate::rep::word::order::ROWS.kinds[0].1] },
        KindId::RunsGroup => const { &[crate::ops::group::lower::ROWS.kinds[0].1, crate::ops::group::lower::ROWS.kinds[1].1, crate::ops::group::lower::ROWS.kinds[2].1] },
        KindId::RunsGroupDistinct => const { &[crate::ops::group::group_distinct::ROWS.kinds[0].1] },
        KindId::RunsIncremental => const { &[crate::ops::sort::incremental::ROWS.kinds[0].1, crate::ops::sort::incremental::ROWS.kinds[1].1] },
        KindId::RunsMergeappend => const { &[crate::ops::append::merge::ROWS.kinds[0].1, crate::ops::append::merge::ROWS.kinds[1].1, crate::ops::append::merge::ROWS.kinds[2].1] },
        KindId::RunsSort => const { &[crate::face::slot_run::ROWS.kinds[0].1, crate::ops::sort::lower::ROWS.kinds[0].1, crate::ops::sort::lower::ROWS.kinds[1].1, crate::ops::sort::lower::ROWS.kinds[2].1] },
        KindId::RunsTopkGroups => const { &[crate::ops::sort::topn::lower::ROWS.kinds[0].1, crate::ops::sort::topn::lower::ROWS.kinds[1].1, crate::ops::sort::topn::lower::ROWS.kinds[2].1] },
        KindId::RunsTopkRows => const { &[crate::ops::sort::topk_rows::ROWS.kinds[0].1, crate::ops::sort::topk_rows::ROWS.kinds[1].1, crate::ops::sort::topk_rows::ROWS.kinds[2].1] },
        KindId::RunsUnique => const { &[crate::ops::group::unique::ROWS.kinds[0].1, crate::ops::group::unique::ROWS.kinds[1].1, crate::ops::group::unique::ROWS.kinds[2].1] },
        KindId::SetopRecursive => const { &[crate::ops::setop::lower::ROWS.kinds[2].1, crate::ops::setop::lower::ROWS.kinds[3].1, crate::ops::setop::lower::ROWS.kinds[4].1] },
        KindId::SetopSetop => const { &[crate::ops::setop::lower::ROWS.kinds[0].1, crate::ops::setop::lower::ROWS.kinds[1].1] },
        KindId::SpillBreakers => const { &[crate::ops::hashjoin::spill::breakers::ROWS.kinds[0].1] },
        KindId::SrcAppend => const { &[crate::ops::append::lower::ROWS.kinds[0].1, crate::ops::append::lower::ROWS.kinds[1].1, crate::ops::append::lower::ROWS.kinds[2].1] },
        KindId::SrcBitmap => const { &[crate::ops::scan::bitmap::ROWS.kinds[0].1] },
        KindId::SrcBitmapTree => const { &[crate::ops::scan::bitmap_tree::ROWS.kinds[0].1] },
        KindId::SrcHeap => const { &[crate::ops::scan::heap::ROWS.kinds[0].1] },
        KindId::SrcIndex => const { &[crate::ops::scan::index::ROWS.kinds[0].1] },
        KindId::SrcIndexAm => const { &[crate::ops::scan::index_am::ROWS.kinds[0].1] },
        KindId::SrcIndexArray => const { &[crate::ops::scan::index_array::ROWS.kinds[0].1] },
        KindId::SrcIndexOnly => const { &[crate::ops::scan::index_only::ROWS.kinds[0].1] },
        KindId::SrcPgrc => const { &[crate::ops::scan::pgrc::lower::ROWS.kinds[0].1] },
        KindId::SrcPrune => const { &[crate::ops::append::prune::ROWS.kinds[0].1] },
        KindId::SrcResult => const { &[crate::ops::result::source::ROWS.kinds[0].1] },
        KindId::SrcSample => const { &[crate::ops::scan::sample::ROWS.kinds[0].1] },
        KindId::SrcTid => const { &[crate::ops::scan::tid::ROWS.kinds[0].1] },
        KindId::SrcValues => const { &[crate::ops::values::lower::ROWS.kinds[0].1] },
        KindId::SrfFunction => const { &[crate::ops::srf::function_scan::ROWS.kinds[0].1, crate::rerun::store_read::ROWS.kinds[0].1, crate::rerun::store_read::ROWS.kinds[1].1] },
        KindId::SrfPl => const { &[crate::ops::srf::pl_function::ROWS.kinds[0].1] },
        KindId::SrfProjectset => const { &[crate::ops::srf::project_set::ROWS.kinds[0].1] },
        KindId::SrfTablefunc => const { &[crate::ops::srf::table_func::ROWS.kinds[0].1] },
        KindId::StepFilter => const { &[crate::step::filter::ROWS.kinds[0].1] },
        KindId::StepGate => const { &[crate::ops::result::gate::ROWS.kinds[0].1, crate::ops::result::gate::ROWS.kinds[1].1] },
        KindId::StepLimit => const { &[crate::ops::result::gate::ROWS.kinds[2].1, crate::ops::limit::lower::ROWS.kinds[0].1] },
        KindId::StepProject => const { &[crate::step::project::ROWS.kinds[0].1] },
        KindId::WindowAgg => const { &[crate::ops::window::rows::ROWS.kinds[0].1, crate::ops::window::rows::ROWS.kinds[1].1, crate::ops::window::lower::ROWS.kinds[0].1] },
        KindId::WriteDelete => const { &[crate::ops::modifytable::delete::ROWS.kinds[0].1, crate::ops::modifytable::delete::ROWS.kinds[1].1, crate::ops::modifytable::rowop_step::ROWS.kinds[0].1] },
        KindId::WriteInsert => const { &[crate::ops::modifytable::insert::ROWS.kinds[0].1, crate::ops::modifytable::insert::ROWS.kinds[1].1] },
        KindId::WriteLock => const { &[crate::ops::modifytable::delete_built::ROWS.kinds[0].1, crate::ops::lockrows::lower::ROWS.kinds[0].1] },
        KindId::WriteMerge => const { &[crate::ops::modifytable::merge::lower::ROWS.kinds[0].1, crate::ops::modifytable::merge::lower::ROWS.kinds[1].1] },
        KindId::WriteOnconflict => const { &[crate::ops::modifytable::onconflict::ROWS.kinds[0].1] },
        KindId::WriteReturning => const { &[crate::ops::modifytable::returning::ROWS.kinds[0].1, crate::ops::modifytable::returning::ROWS.kinds[1].1, crate::ops::modifytable::returning::ROWS.kinds[2].1, crate::ops::modifytable::returning::ROWS.kinds[3].1, crate::ops::modifytable::returning::ROWS.kinds[4].1] },
        KindId::WriteTransition => const { &[crate::ops::modifytable::triggers::ROWS.kinds[0].1, crate::ops::modifytable::triggers::ROWS.kinds[1].1] },
        KindId::WriteUpdate => const { &[crate::ops::modifytable::update::ROWS.kinds[0].1, crate::ops::modifytable::update::ROWS.kinds[1].1] },
    }
}

/// The roads' dispatch, as `kind_rows`.
pub fn strategy_rows(r: Strategy) -> &'static [Mech] {
    match r {
        strategy::ANSWER_COUNTED => const { &[crate::ops::agg::trans::strategies::answer_counted::ROWS.strategies[0].1] },
        strategy::ANSWER_NUMERIC => const { &[crate::ops::agg::trans::strategies::answer_num::ROWS.strategies[0].1] },
        strategy::ANSWER_STATS => const { &[crate::ops::agg::trans::answer::ROWS.strategies[0].1] },
        strategy::APPEND_ARMS => const { &[crate::ops::append::strategies::arms::ROWS.strategies[0].1] },
        strategy::APPEND_NEST => const { &[crate::ops::append::strategies::nest::ROWS.strategies[0].1] },
        strategy::BODY_FORM => const { &[crate::frame::body_form::ROWS.strategies[0].1] },
        strategy::BODY_VERSIONED => const { &[crate::frame::body::ROWS.strategies[0].1] },
        strategy::CODE_CARRY => const { &[crate::dict::coderep::ROWS.strategies[0].1] },
        strategy::CODE_CUT => const { &[crate::dict::strategies::code_cut::ROWS.strategies[0].1] },
        strategy::CODE_FRONT_DENSE => const { &[crate::dict::codefront_dense::ROWS.strategies[0].1] },
        strategy::CODE_FRONT_LIGHT => const { &[crate::dict::strategies::codefront_light::ROWS.strategies[0].1] },
        strategy::CODE_FRONT_PART => const { &[crate::dict::codefront::ROWS.strategies[0].1] },
        strategy::CODE_HEADS => const { &[crate::dict::strategies::heads::ROWS.strategies[0].1] },
        strategy::CODE_RENDER => const { &[crate::dict::late_render::ROWS.strategies[0].1] },
        strategy::CODE_SPARSE => const { &[crate::dict::strategies::code_sparse::ROWS.strategies[0].1] },
        strategy::CTE_WHOLEFILL => const { &[crate::ops::ctescan::wholefill::ROWS.strategies[0].1] },
        strategy::DECODE_LATE => const { &[crate::ops::scan::pgrc::pgrc_late::ROWS.strategies[0].1] },
        strategy::DENSE_RANGE => const { &[crate::ops::agg::strategies::dense_range::ROWS.strategies[0].1] },
        strategy::DICT_ENTRIES => const { &[crate::ops::scan::pgrc::dict_entries::ROWS.strategies[0].1] },
        strategy::DICT_OPEN => const { &[crate::ops::scan::pgrc::dict_open::ROWS.strategies[0].1] },
        strategy::DISTINCT_TOTAL => const { &[crate::ops::group::distinct_total::ROWS.strategies[0].1] },
        strategy::EMIT_PARMERGE => const { &[crate::table::emit_runs::ROWS.strategies[0].1] },
        strategy::DRAIN_SPOOL_WIDE => const { &[crate::ops::hashjoin::strategies::drain_wide::ROWS.strategies[0].1] },
        strategy::EXPR_FUSION => const { &[crate::step::expr_fuse::ROWS.strategies[0].1] },
        strategy::EXPR_MONOTONE => const { &[crate::ops::agg::trans::strategies::monotone::ROWS.strategies[0].1] },
        strategy::FINALS_CSE => const { &[crate::ops::agg::trans::strategies::finals_cse::ROWS.strategies[0].1] },
        strategy::FINALS_SHIFT => const { &[crate::ops::agg::trans::finals_shift::ROWS.strategies[0].1] },
        FLOOR => const { &[crate::ops::XC.strategies[0].1] },
        SCAN_MERGEFREE => const { &[crate::ops::XC.strategies[1].1] },
        SCAN_MERGEFREE_BUILD => const { &[crate::ops::XC.strategies[2].1] },
        GROUP_HASHCHAIN => const { &[crate::ops::XC.strategies[3].1] },
        strategy::FRAME_CARRY => const { &[crate::frame::carry::ROWS.strategies[0].1] },
        strategy::HASH_RUNOPEN => const { &[crate::table::hash_open::ROWS.strategies[0].1] },
        strategy::HEAP_PAGES => const { &[crate::ops::scan::strategies::heap_pages::ROWS.strategies[0].1] },
        strategy::HEAP_STREAM => const { &[crate::ops::scan::strategies::heap_stream::ROWS.strategies[0].1] },
        strategy::LOOP_HOIST => const { &[crate::frame::hoist::ROWS.strategies[0].1] },
        strategy::JOIN_BLOOMTAG => const { &[crate::ops::hashjoin::strategies::bloomtag::ROWS.strategies[0].1] },
        strategy::JOIN_BUILDSIDE => const { &[crate::ops::hashjoin::build_side::ROWS.strategies[0].1] },
        strategy::JOIN_DENSE => const { &[crate::ops::hashjoin::strategies::dense::ROWS.strategies[0].1] },
        strategy::JOIN_DOMAIN => const { &[crate::ops::hashjoin::strategies::domain::ROWS.strategies[0].1] },
        strategy::JOIN_DEDUP => const { &[crate::ops::hashjoin::strategies::dedup::ROWS.strategies[0].1] },
        strategy::JOIN_PAYLOAD_FP => const { &[crate::ops::hashjoin::strategies::payload_fp::ROWS.strategies[0].1] },
        strategy::JOIN_DRAIN_FP => const { &[crate::ops::hashjoin::strategies::payload_fp::ROWS.strategies[1].1] },
        strategy::TEXT_SUBSTR_ASCII => const { &[crate::rep::varlena::substr_ascii::ROWS.strategies[0].1] },
        strategy::TEXT_SET_BPCHAR => const { &[crate::rep::varlena::text_set::ROWS.strategies[1].1] },
        strategy::JOIN_MULT => const { &[crate::ops::hashjoin::strategies::multiplicity::ROWS.strategies[0].1] },
        strategy::JOIN_ROWFILTER => const { &[crate::ops::hashjoin::strategies::rowfilter::ROWS.strategies[0].1] },
        strategy::JOIN_ROWFILTER_SINGLE => const { &[crate::ops::hashjoin::strategies::rowfilter::ROWS.strategies[1].1] },
        strategy::JOIN_ROWFILTER_COST => const { &[crate::ops::hashjoin::strategies::rowfilter::ROWS.strategies[2].1] },
        strategy::JOIN_PROBE_STAGED => const { &[crate::ops::hashjoin::strategies::probe_stage::ROWS.strategies[0].1] },
        strategy::JOIN_RTFILTER => const { &[crate::ops::hashjoin::strategies::rtfilter::ROWS.strategies[0].1] },
        strategy::JOIN_RTFILTER_ARMS => const { &[crate::ops::hashjoin::strategies::rtfilter::ROWS.strategies[2].1] },
        strategy::JOIN_RTFILTER_MULTI => const { &[crate::ops::hashjoin::strategies::rtfilter::ROWS.strategies[1].1] },
        strategy::JSONB_CODES => const { &[crate::ops::scan::pgrc::strategies::jsonb_codes::ROWS.strategies[0].1] },
        strategy::JSONB_INPLACE => const { &[crate::ops::scan::pgrc::strategies::jsonb_inplace::ROWS.strategies[0].1] },
        strategy::JSONB_LANES => const { &[crate::ops::scan::pgrc::strategies::jsonb_lanes::ROWS.strategies[0].1, crate::ops::scan::pgrc::strategies::jsonb_lanes::ROWS.strategies[1].1] },
        strategy::KEY_CODEIF => const { &[crate::dict::strategies::key_codeif::ROWS.strategies[0].1] },
        strategy::KEY_COLL => const { &[crate::rep::varlena::coll::ROWS.strategies[0].1] },
        strategy::KEY_DEPENDENT => const { &[crate::ops::agg::strategies::dependent::ROWS.strategies[0].1] },
        strategy::KEY_CARRY => const { &[crate::ops::agg::strategies::dependent::ROWS.strategies[1].1] },
        strategy::CASE_SELECT => const { &[crate::ops::agg::numeric::strategies::num_case::ROWS.strategies[0].1] },
        strategy::KEY_TAGGED => const { &[crate::rep::varlena::hash_tagged::ROWS.strategies[0].1] },
        strategy::MASK_FOLD => const { &[crate::ops::agg::trans::block_fold::ROWS.strategies[0].1] },
        strategy::MASK_ROWS => const { &[crate::frame::mask_rows::ROWS.strategies[0].1] },
        strategy::MASK_RUN => const { &[crate::frame::mask_run::ROWS.strategies[0].1] },
        strategy::MASK_VEC => const { &[crate::frame::mask_vec::ROWS.strategies[0].1] },
        strategy::MASK_WORD => const { &[crate::frame::block::ROWS.strategies[0].1] },
        strategy::MERGE_KSORT => const { &[crate::ops::mergejoin::strategies::ksort::ROWS.strategies[0].1] },
        strategy::MERGE_WIDEEMIT => const { &[crate::ops::mergejoin::strategies::ksort::ROWS.strategies[1].1] },
        strategy::MERGE_MARKABLE => const { &[crate::ops::mergejoin::strategies::markable::ROWS.strategies[0].1] },
        strategy::NEST_CATALOG_LEFT => const { &[crate::ops::nestloop::strategies::catalog_left::ROWS.strategies[0].1] },
        strategy::NUM_LEAN => const { &[crate::ops::agg::numeric::class_num::ROWS.strategies[0].1] },
        strategy::NUM_MEMO => const { &[crate::ops::agg::numeric::strategies::memo_num::ROWS.strategies[0].1] },
        strategy::NUM_AGGCMP => const { &[crate::rep::varlena::cmp_aggnum::ROWS.strategies[0].1] },
        strategy::NUM_NARROW => const { &[crate::ops::agg::numeric::strategies::num_narrow::ROWS.strategies[0].1] },
        strategy::NUM_PAIR => const { &[crate::ops::agg::numeric::strategies::num_partial::ROWS.strategies[1].1] },
        strategy::NUM_PARTIAL => const { &[crate::ops::agg::numeric::strategies::num_partial::ROWS.strategies[0].1] },
        strategy::NUM_SHARE => const { &[crate::ops::agg::numeric::strategies::num_share::ROWS.strategies[0].1] },
        strategy::NUM_SHORT => const { &[crate::rep::varlena::cmp_num::ROWS.strategies[0].1, crate::rep::varlena::num_arith::ROWS.strategies[0].1, crate::rep::packednum::float_cast::ROWS.strategies[0].1, crate::rep::varlena::num_short::ROWS.strategies[0].1] },
        strategy::NUM_WINDOW => const { &[crate::ops::agg::numeric::strategies::num_window::ROWS.strategies[0].1] },
        strategy::NUM_SCALE => const { &[crate::ops::agg::numeric::strategies::num_window::ROWS.strategies[1].1] },
        strategy::NUM_DIV => const { &[crate::ops::agg::numeric::strategies::num_window::ROWS.strategies[2].1] },
        strategy::NUM_CTE => const { &[crate::ops::agg::numeric::strategies::num_window::ROWS.strategies[3].1] },
        strategy::OWNER_COUNT => const { &[crate::ops::sort::topn::strategies::owner_count::ROWS.strategies[0].1] },
        strategy::PERCODE_LENGTH => const { &[crate::dict::percode_len::ROWS.strategies[0].1] },
        strategy::PERCODE_TABLE => const { &[crate::dict::percode::ROWS.strategies[0].1] },
        strategy::PREFETCH_FP => const { &[crate::frame::prefetch_fp::ROWS.strategies[0].1] },
        strategy::READ_CLAIM => const { &[crate::ops::scan::pgrc::readplan_claim::ROWS.strategies[0].1] },
        strategy::RUN_COLLAPSE => const { &[crate::ops::agg::run_collapse::ROWS.strategies[0].1] },
        strategy::RUN_PERGROUP => const { &[crate::ops::group::strategies::per_group::ROWS.strategies[0].1] },
        strategy::SETOP_EMPTYLEFT => const { &[crate::ops::setop::strategies::empty_left::ROWS.strategies[0].1] },
        strategy::SINK_ORDERED => const { &[crate::end::strategies::sink_ordered::ROWS.strategies[0].1] },
        strategy::SORT_KEYLANE => const { &[crate::rep::keylane::ROWS.strategies[0].1] },
        strategy::SORT_NATURAL => const { &[crate::ops::sort::strategies::natural::ROWS.strategies[0].1] },
        strategy::SORT_RESIDENT => const { &[crate::ops::sort::strategies::resident::ROWS.strategies[0].1] },
        strategy::SPOOL_SEAL => const { &[crate::rerun::spool::seal::ROWS.strategies[0].1] },
        strategy::WIN_PARTFILL => const { &[crate::face::partfill::ROWS.strategies[0].1] },
        strategy::SRF_LIMITCUT => const { &[crate::ops::srf::strategies::limit_cut::ROWS.strategies[0].1] },
        strategy::SRF_PARALLEL => const { &[crate::ops::srf::strategies::parallel::ROWS.strategies[0].1] },
        strategy::STAT_REG => const { &[crate::ops::agg::trans::class_stats::ROWS.strategies[0].1] },
        strategy::TABLE_BYPASS => const { &[crate::table::bypass::ROWS.strategies[0].1] },
        strategy::TABLE_RADIX => const { &[crate::table::radix::ROWS.strategies[0].1] },
        strategy::TABLE_SIZED => const { &[crate::table::size::ROWS.strategies[0].1] },
        strategy::TABLE_SMALL => const { &[crate::ops::agg::dense_small::ROWS.strategies[0].1] },
        strategy::TEXT_EQ => const { &[crate::rep::varlena::cmp_text::ROWS.strategies[0].1] },
        strategy::TEXT_LEAN => const { &[crate::rep::varlena::cmp_text::ROWS.strategies[1].1] },
        strategy::TEXT_SET => const { &[crate::rep::varlena::text_set::ROWS.strategies[0].1] },
        strategy::KEY_CANON => const { &[crate::rep::varlena::key_canon::ROWS.strategies[0].1] },
        strategy::TEXT_FRAME => const { &[crate::dict::strategies::text_frame::ROWS.strategies[0].1] },
        strategy::TEXT_MEMMEM => const { &[crate::step::text_like::ROWS.strategies[0].1] },
        strategy::TOPK_BOUNDFIRST => const { &[crate::ops::sort::strategies::bound_first::ROWS.strategies[0].1] },
        strategy::TOPK_FETCH => const { &[crate::ops::sort::strategies::fetch::ROWS.strategies[0].1] },
        strategy::TOPK_WALK => const { &[crate::ops::sort::strategies::walk::ROWS.strategies[0].1] },
        strategy::TWOPASS_COUNT => const { &[crate::ops::sort::topn::strategies::twopass_count::ROWS.strategies[0].1] },
        strategy::TWOPASS_HAVING => const { &[crate::ops::sort::topn::strategies::twopass_having::ROWS.strategies[0].1] },
        strategy::VERDICT_ALLMATCH => const { &[crate::ops::scan::pgrc::verdict_allmatch::ROWS.strategies[0].1] },
        strategy::VERDICT_BLOOM => const { &[crate::ops::scan::pgrc::verdict_bloom::ROWS.strategies[0].1] },
        strategy::VERDICT_CODERANGE => const { &[crate::dict::strategies::verdict_coderange::ROWS.strategies[0].1] },
        strategy::VERDICT_COND => const { &[crate::ops::scan::pgrc::verdict_cond::ROWS.strategies[0].1] },
        strategy::VERDICT_ELIDE => const { &[crate::ops::scan::pgrc::lane_elide::ROWS.strategies[0].1] },
        strategy::VERDICT_PSMA => const { &[crate::ops::scan::pgrc::verdict_psma::ROWS.strategies[0].1] },
        strategy::VERDICT_RTCONST => const { &[crate::ops::scan::pgrc::verdict_rtconst::ROWS.strategies[0].1] },
        strategy::VERDICT_ZONE => const { &[crate::ops::scan::pgrc::verdict_zone::ROWS.strategies[0].1] },
        strategy::WIN_PARTRUNS => const { &[crate::face::partruns::ROWS.strategies[0].1] },
        strategy::WIDTH_WALK => const { &[crate::ops::sort::strategies::walk_width::ROWS.strategies[0].1] },
        strategy::WIDTH_WORK => const { &[crate::frame::width::ROWS.strategies[0].1] },
        strategy::WIDTH_WRITE => const { &[crate::frame::width::ROWS.strategies[1].1] },
        strategy::WRITE_CHECKS => const { &[crate::ops::modifytable::strategies::checks_ir::ROWS.strategies[0].1] },
        strategy::WRITE_IDXUNCHANGED => const { &[crate::ops::modifytable::update::ROWS.strategies[0].1] },
        strategy::WRITE_LITERALS => const { &[crate::ops::modifytable::strategies::literals::ROWS.strategies[0].1] },
        strategy::WRITE_ROUTE => const { &[crate::ops::modifytable::strategies::route::ROWS.strategies[0].1, crate::ops::modifytable::strategies::route::ROWS.strategies[1].1] },
        _ => &[],
    }
}

/// The first row of kind `k` (a kind served by more than one face, e.g. `join.inner`'s build step and hash-build end, has
/// one row per face type).
pub fn kind(k: KindId) -> Option<Mech> {
    kind_rows(k).first().copied()
}

pub fn strategy(r: Strategy) -> Option<Mech> {
    strategy_rows(r).first().copied()
}

/// Kind `k`'s row whose value is a `T` (its face's type).
pub fn kind_as<T: 'static>(k: KindId) -> Option<&'static T> {
    kind_rows(k).iter().find_map(|m| m.downcast_ref())
}

#[cfg(test)]
fn find_as<T: 'static>(tables: &[&Table], k: KindId) -> Option<&'static T> {
    tables.iter().flat_map(|t| t.kinds.iter()).filter(|e| e.0 == k).find_map(|e| e.1.downcast_ref())
}

/// Every kind / road some file provides, and every road a twin A/B dropped.
pub fn built_kinds() -> KindSet {
    crate::face::kinds::KINDS.iter().filter(|d| !kind_rows(d.id).is_empty()).fold(KindSet::EMPTY, |mut s, d| { s.add(d.id); s })
}

pub fn built_strategies() -> StrategySet {
    crate::face::strategy::STRATEGIES.iter().filter(|d| !strategy_rows(**d).is_empty()).fold(StrategySet::EMPTY, |mut s, d| { s.add(*d); s })
}

pub fn dropped_strategies() -> StrategySet {
    DROPPED.iter().fold(StrategySet::EMPTY, |mut s, d| { s.add(d.road); s })
}

/// The expression extensions' rows in claim order (`face::ext::expr_exts`): the kind rows (no road), then the road rows.
/// `ext_rows_are_every_extension_row` checks it lists every `ExprExtImpl` row of `kind_rows` / `strategy_rows`.
pub fn ext_rows() -> &'static [(Option<Strategy>, Mech)] {
    const {
        &[
            (None, crate::step::expr_row::ROWS.kinds[0].1),
            (None, crate::step::expr_saop::ROWS.kinds[0].1),
            (Some(strategy::NUM_SHORT), crate::rep::varlena::cmp_num::ROWS.strategies[0].1),
            (Some(strategy::NUM_AGGCMP), crate::rep::varlena::cmp_aggnum::ROWS.strategies[0].1),
            (Some(strategy::TEXT_EQ), crate::rep::varlena::cmp_text::ROWS.strategies[0].1),
            (Some(strategy::NUM_SHORT), crate::rep::varlena::num_arith::ROWS.strategies[0].1),
            (Some(strategy::EXPR_FUSION), crate::step::expr_fuse::ROWS.strategies[0].1),
            (Some(strategy::JSONB_INPLACE), crate::ops::scan::pgrc::strategies::jsonb_inplace::ROWS.strategies[0].1),
            (Some(strategy::TEXT_MEMMEM), crate::step::text_like::ROWS.strategies[0].1),
            // D3139: last, so it claims only what no other row did (an expression takes one claim)
            (Some(strategy::NUM_SHORT), crate::rep::packednum::float_cast::ROWS.strategies[0].1),
        ]
    }
}

#[cfg(test)]
fn strategy_as<T: 'static>(tables: &[&Table], r: Strategy) -> Option<&'static T> {
    tables.iter().flat_map(|t| t.strategies.iter()).filter(|e| e.0 == r).find_map(|e| e.1.downcast_ref())
}

pub(crate) fn need<T: 'static>(cx: &impl Site, built: bool, t: Option<&'static T>, v: Variant) -> &'static T {
    match (built, t) {
        (_, Some(t)) => {
            crate::mechs_used::fetched(v); // MECHCORPUS: the program's mechanisms-used record
            t
        }
        (false, None) => crate::not_built!(cx, v),
        (true, None) => crate::refuse!(cx, Reason::Bug { payload: "a variant registered without this face's type" }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::face::step::ExprExtImpl;

    /// A kind has at most one row per face type (a second would never be read), a road one per (face type, op).
    #[test]
    fn rows_are_unique() {
        let mut bad = Vec::new(); // alloc: Program
        for d in crate::face::kinds::KINDS {
            let rows = kind_rows(d.id);
            for (i, a) in rows.iter().enumerate() {
                if rows[..i].iter().any(|b| (**b).type_id() == (**a).type_id()) {
                    bad.push(format!("kind {} registered twice with one face type", d.name));
                }
            }
        }
        for d in crate::face::strategy::STRATEGIES {
            let rows: Vec<(Strategy, Mech)> = strategy_rows(*d).iter().map(|m| (*d, *m)).collect(); // alloc: Program
            bad.extend(clashes(&rows));
        }
        assert!(bad.is_empty(), "{}", bad.join("\n"));
    }

    /// Rows of one road that clash: two of one face type, except expression extensions, which clash when they share a
    /// claimed op or when one of several on a road declares none (the key is (road, face, op)).
    fn clashes(roads: &[(Strategy, Mech)]) -> Vec<String> {
        let ops = |m: Mech| m.downcast_ref::<ExprExtImpl>().map(|e| e.0.ops());
        let mut bad = Vec::new(); // alloc: Program
        for (i, a) in roads.iter().enumerate() {
            for b in roads[..i].iter().filter(|b| b.0 == a.0 && (*b.1).type_id() == (*a.1).type_id()) {
                match (ops(a.1), ops(b.1)) {
                    (Some(x), Some(y)) if x.is_empty() || y.is_empty() => bad.push(format!("road {}: an extension beside another declares no ops", a.0.name())),
                    (Some(x), Some(y)) => x.iter().filter(|o| y.contains(o)).for_each(|o| bad.push(format!("road {}: op {o} claimed twice", a.0.name()))),
                    _ => bad.push(format!("road {} registered twice with one face type", a.0.name())),
                }
            }
        }
        bad
    }

    /// Two extensions of one road that share an op clash, as do one without ops beside another; disjoint ones do not.
    #[test]
    fn extension_rows_clash_on_a_shared_op() {
        use crate::face::step::{Claim, ExprExtMech, SubEmit};
        struct X(&'static [u32]);
        impl ExprExtMech for X {
            fn attach<'p>(&self, _: &mut crate::frame::px::Px<'_, 'p>, _: (usize, usize), _: types_nodes::Node<'p>, _: &mut gravity_expr::Compiled<'p>, _: crate::frame::px::ExId, _: &mut dyn FnMut(types_nodes::Node<'p>) -> u32) -> Option<Claim> {
                None
            }
            fn emit(&self, _: &mut gravity_ir::builder::FunctionBuilder<'_>, _: &mut crate::frame::env::Env, _: &crate::frame::row::RowVals, _: crate::frame::px::ExId, _: u32, _: SubEmit<'_>) -> [gravity_ir::Value; 3] {
                unreachable!()
            }
            fn ops(&self) -> &'static [u32] {
                self.0
            }
        }
        static A: X = X(&[1, 2]);
        static B: X = X(&[3]);
        static C: X = X(&[2, 4]);
        static N: X = X(&[]);
        let (a, b, c, n) = (ExprExtImpl(&A), ExprExtImpl(&B), ExprExtImpl(&C), ExprExtImpl(&N));
        let row = |x: &'static ExprExtImpl| (strategy::NUM_SHORT, x as Mech);
        let leak = |x: ExprExtImpl| &*Box::leak(Box::new(x)); // alloc: Program
        let (a, b, c, n) = (leak(a), leak(b), leak(c), leak(n));
        assert!(clashes(&[row(a), row(b)]).is_empty());
        assert_eq!(clashes(&[row(a), row(b), row(c)]), vec!["road num.short: op 2 claimed twice".to_owned()]);
        assert_eq!(clashes(&[row(a), row(n)]).len(), 1);
        assert!(clashes(&[row(n)]).is_empty(), "alone on its road");
    }

    /// `ext_rows` lists exactly the `ExprExtImpl` rows of the dispatch, each under its own road (a kind row under none).
    #[test]
    fn ext_rows_are_every_extension_row() {
        let at = |m: Mech| m.downcast_ref::<ExprExtImpl>().map(|e| e.0 as *const dyn crate::face::step::ExprExtMech as *const () as usize);
        let mut listed: Vec<(Option<Strategy>, usize)> = ext_rows().iter().map(|e| (e.0, at(e.1).expect("an ExprExtImpl row"))).collect(); // alloc: Program
        let kinds = crate::face::kinds::KINDS.iter().flat_map(|d| kind_rows(d.id).iter().filter_map(|m| at(*m)).map(|p| (None, p)));
        let roads = crate::face::strategy::STRATEGIES.iter().flat_map(|d| strategy_rows(*d).iter().filter_map(|m| at(*m)).map(move |p| (Some(*d), p)));
        let mut rows: Vec<(Option<Strategy>, usize)> = kinds.chain(roads).collect(); // alloc: Program
        listed.sort();
        rows.sort();
        assert_eq!(listed, rows);
    }

    struct StepMaker(u8);
    struct EndMaker(u8);
    static STEP: StepMaker = StepMaker(1);
    static END: EndMaker = EndMaker(2);
    static TWO: Table = Table { kinds: &[(KindId::JoinInner, &STEP), (KindId::JoinInner, &END)], strategies: &[] };

    /// A kind served by two faces has a row per face type, and each face finds its own; an unbuilt one refuses `NotBuilt`
    /// (checked on the tables-independent path: which kinds are built changes with every milestone, D242).
    #[test]
    fn a_kind_row_per_face() {
        assert_eq!(find_as::<StepMaker>(&[&TWO], KindId::JoinInner).map(|m| m.0), Some(1));
        assert_eq!(find_as::<EndMaker>(&[&TWO], KindId::JoinInner).map(|m| m.0), Some(2));
        assert!(find_as::<u8>(&[&TWO], KindId::JoinInner).is_none());
        let r = crate::catch(5, || need::<StepMaker>(&5, false, None, Variant::Kind(KindId::JoinInner)).0).expect_err("unbuilt");
        assert_eq!(r.reason, Variant::Kind(KindId::JoinInner).not_built());
        let r = crate::catch(5, || need::<u8>(&5, true, None, Variant::Kind(KindId::JoinInner))).expect_err("a missing face type");
        assert!(matches!(r.reason, Reason::Bug { .. }));
    }

    /// A road with a row per face type: each face finds its own row in either registration order (a later append never
    /// changes which one a reader gets).
    #[test]
    fn a_road_row_per_face() {
        static AB: Table = Table { kinds: &[], strategies: &[(strategy::NUM_SHORT, &STEP), (strategy::NUM_SHORT, &END)] };
        static BA: Table = Table { kinds: &[], strategies: &[(strategy::NUM_SHORT, &END), (strategy::NUM_SHORT, &STEP)] };
        for t in [&AB, &BA] {
            assert_eq!(strategy_as::<StepMaker>(&[t], strategy::NUM_SHORT).map(|m| m.0), Some(1));
            assert_eq!(strategy_as::<EndMaker>(&[t], strategy::NUM_SHORT).map(|m| m.0), Some(2));
            assert!(strategy_as::<u8>(&[t], strategy::NUM_SHORT).is_none());
        }
    }
}
