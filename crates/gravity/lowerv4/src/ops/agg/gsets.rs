//! Kind `keyed.gsets` (cards/M3.md L6): GROUPING SETS / ROLLUP / CUBE as the union rewrite. Owner M3-KEYED.
//! Decompose's `map_sets` (agg.rs, V4R-130) declares one hashed (or plain) group per set over its own walk of the input,
//! each set's target list and qual with nodeAgg's projection of that set folded in as constants (a grouped column the set
//! does not group reads NULL; `GROUPING()` is the set's mask), and the arms as a streamed union: `push_union` copies the
//! parent's ops into every arm. So an arm is an M1 keyed group (keyed.hashagg / fold.plain, drained by keyed.emit_group)
//! whose emit pipeline carries M2's union marker (source/append.rs `make_arm`: the `Arms` protocol over the sets, arg =
//! the number of sets, main's ruling 09-25). Nothing here lowers code: the row declares the kind served once every arm
//! is. A set's end the union cannot continue (a sort / top-k / build over the arms, D393) refuses in append.rs by name.
use crate::face::kinds::KindId;
use crate::frame::frame::{EndMaker, SourceMaker};
use crate::ops::Table;

/// D3054: the grouping-set input spool's fill end and readers (gsets_spool.rs), rows 1 and 2.
pub(crate) const ROWS: Table = Table { kinds: &[(KindId::KeyedGsets, &()), (KindId::KeyedGsets, &EndMaker(super::gsets_spool::make_end)), (KindId::KeyedGsets, &SourceMaker(super::gsets_spool::make_source))], ..crate::ops::NONE };
