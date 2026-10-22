//! Kind `class.user` (`ClassImpl`: user-defined aggregates, transfn / finalfn calls; L5) (cards/M6.md, M6-changes.md
//! §1). Owner M6-AGG.
//!
//! An aggregate decompose's catalog table does not know (CREATE AGGREGATE's): the straight form (D2) is PG's own
//! transition and final by fmgr through face/aggcall.rs with class.num_varlena's hosted fold (V4R-424). The host binds
//! the catalog's pg_aggregate row: transfn / finalfn (finalextra's NULL arguments), the initcond, strictness (the first
//! input as a strict transfn's state), the by-reference copy into the aggregate context, and ExecInitAgg's EXECUTE
//! checks as the aggregate's owner. Aggregates the planner gave one transition state (`aggtransno`: same transfn,
//! inputs and initcond, a shareable finalfn) share one `AggState` slot (fold/hosted.rs `states`) and one transfn call
//! per row, each its own finalfn, as PG's (a transfn's NOTICEs count once). The group's state is its `AggState` slot; a
//! keyed end takes it on fold/hosted.rs's terms (`keyed_slots`: a combinefn, ...) and refuses the rest by name. The
//! program runs width 1 unless every aggregate has a combinefn and is PARALLEL SAFE (the arena's rule).
use crate::face::fold::ClassImpl;
use crate::face::kinds::KindId;
use crate::ops::Table;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::ClassUser, &ClassImpl(&crate::ops::agg::numeric::class_num_varlena::FOLD))], ..crate::ops::NONE };
