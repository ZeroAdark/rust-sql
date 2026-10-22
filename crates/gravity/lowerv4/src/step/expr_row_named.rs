//! EXPR-ODDS A (split from step/expr_row.rs, main 09-28: a new file, not a budget raise): a named tuplestore scan's own
//! row as a `record` (a trigger body's `SELECT string_agg(new_table::text, ..) FROM new_table`). C
//! `ExecEvalWholeRowVar`'s RECORD leg reads `ExecInitNamedTuplestoreScan`'s slot, whose descriptor is the ENR's
//! (`ENRMetadataGetTupDesc`: a transition table's is its relation's own, dropped attributes kept); the row is that
//! descriptor copied, reset to `record`, renamed by the RTE's `eref` and blessed (gravity_expr
//! `compile_relation_record`). The decomposer gives the row (`subplan::named_record_row`: `named` the relation, the
//! `eref` names per attribute). A dropped attribute keeps its place, so a `FieldSelect` the parser numbered over the
//! RTE's columns reads the same field. The names mark the program statement-specific (`Px::data`, as `record_expr`'s).
//! Owner EXPR-ODDS. Budget 80.
use gravity_expr::{Compiled, NodeId, Source as VarSource};

use gravity_decompose::ops::shared::record_row::RecordRow;

use mcx::{CollectIn, PgVec};

use crate::frame::px::Px;

/// `record_expr`'s refusal name: the descriptor the compiler could not build (a relation gone from the catalog).
const NAMED_FORM: &str = "a named tuplestore's record row the compiler refused";

/// The row of named tuplestore record row `r` (its `named` relation), compiled into `c`.
pub(crate) fn record<'p>(px: &mut Px<'_, 'p>, c: &mut Compiled<'p>, r: &RecordRow<'p>) -> Result<NodeId, &'static str> {
    let names: PgVec<'_, &str> = r.cols.iter().map(|col| col.name).collect_in(px.mcx());
    let mut b = gravity_expr::nodes::Builder { prog: &mut c.prog, state: &mut c.state, catalog: px.gx.catalog, outer: None, tableoid: None, aggorder: false, returning: None, folded: true };
    let id = gravity_expr::nodes::tuple::compile_relation_record(&mut b, VarSource::Scan, r.named, &names[..]).map_err(|_| NAMED_FORM)?;
    let key: Vec<u8> = r.named.to_le_bytes().into_iter().chain(r.cols.iter().flat_map(|col| col.name.bytes().chain([0]))).collect(); // alloc: Program
    px.data.push(key.into_boxed_slice());
    Ok(id)
}
