//! Kind `class.collect` (`ClassImpl`: array_agg / string_agg / json(b)_agg / range aggregates; L5) (cards/M6.md,
//! M6-changes.md §1). Owner M6-COLLECT (the plan's M6-AGG).
//!
//! array_agg (of scalars and of arrays), string_agg (text and bytea), json(b)_agg and json(b)_object_agg (plain,
//! strict, unique), range_agg / multirange range_agg, range_intersect_agg, xmlagg and any_value: the straight form (D2)
//! is PG's own transition by fmgr through face/aggcall.rs (`array_agg_transfn`, `string_agg_transfn`,
//! `jsonb_agg_transfn`, ... into their internal state in the aggregate context; xmlagg's by-reference xml state copied
//! there; any_value / range_intersect_agg's first input as a strict transfn's state), the finals by fmgr
//! (`array_agg_finalfn`, `string_agg_finalfn`, ...), with class.num_varlena's hosted fold, the group's state in its
//! `AggState` slot (fold/hosted.rs). Element order without an ORDER BY inside the call is the input's, any order
//! (Decision 7). A keyed end refuses the hosted slot by name (D798); DISTINCT and an unsorted ORDER BY inside the call
//! fold through the host's per-group run (a later row); the native append forms (cards/M6.md L5) are a perf row.
use crate::face::fold::ClassImpl;
use crate::face::kinds::KindId;
use crate::ops::Table;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::ClassCollect, &ClassImpl(&crate::ops::agg::numeric::class_num_varlena::FOLD))], ..crate::ops::NONE };
