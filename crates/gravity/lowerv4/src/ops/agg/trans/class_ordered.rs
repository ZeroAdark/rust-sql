//! Kind `class.ordered` (`ClassImpl`: ordered-set and hypothetical aggregates; L5) (cards/M6.md, M6-changes.md §1).
//! Owner M6-AGG.
//!
//! percentile_cont / percentile_disc (scalar and array fractions), mode, and the hypothetical-set rank / dense_rank /
//! percent_rank / cume_dist (V4R-425): the straight form (D2) is PG's own transition and final by fmgr through
//! face/aggcall.rs with class.num_varlena's hosted fold, one `AggState` per group (fold/hosted.rs). The transfn
//! (`ordered_set_transition` / `_multi`) sorts the WITHIN GROUP input inside its own state (a tuplesort in the aggregate
//! context, the sort read from the Aggref the host names, `AggGetAggref`), as PG's: nodeAgg does no presort, so the
//! fold feeds the rows in any order (fold/args.rs `order_free`). The direct arguments are constants the host binds
//! once and hands the finalfn after the state (ExecInitAgg's numFinalArgs: then NULL for finalextra's aggregated
//! arguments). D1520 (V4R-537, aggtrans T7b): a direct argument that is a group key of one image, or an expression over
//! such keys that does not act, is evaluated per group over the group's row at a keyed end (keyed/stream.rs `finals_of`),
//! a grouping set's nulled key as NULL; any other direct argument that is no constant (PG evaluates it per group)
//! refuses by name. No combinefn: the
//! program runs width 1, and a keyed record refuses it by name (fold/hosted.rs `keyed_slots`). A user-defined
//! ordered-set aggregate is class.user's kind and folds the same way.
use crate::face::fold::ClassImpl;
use crate::face::kinds::KindId;
use crate::ops::Table;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::ClassOrdered, &ClassImpl(&crate::ops::agg::numeric::class_num_varlena::FOLD))], ..crate::ops::NONE };
