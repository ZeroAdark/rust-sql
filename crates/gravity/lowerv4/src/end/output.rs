//! The statement's out row (KEEP lowerv3 scan/output.rs): one `OutCol` per non-junk entry of the root pipeline's last
//! projection (name, type, typmod, collation, origin), the rows a push carries and the by-reference out region. Each
//! column keeps its entry position and its type facts, read from the catalog once here (the sink's accessors take them).
//! Registers nothing: the sinks (sink.rs, sink_ordered.rs) declare through it. Budget 160 lines.
use gravity_decompose::catalog::typefacts::{self, TypeFacts};
use gravity_decompose::Op;
use gravity_program::program::out_region_bytes;
use gravity_program::{ArenaVec, OutCol};
use mcx::{GrowIn, PgVec};
use types_nodes::primnodes::TargetEntry;

use crate::frame::px::{Out, Px};
use crate::graph::Gx;
use crate::admit::refusal::{Reason, ShapeWhy};

/// One out column as the sink stores it: its entry in the row, its type facts, and its typlen when by reference.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Col {
    pub entry: usize,
    pub tf: TypeFacts,
    pub byref: Option<i16>,
}

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

/// A type's facts: the table's row, a domain's base row, else the catalog's storage; a type nothing knows refuses by name.
pub(crate) fn facts(gx: &Gx<'_, '_>, typid: u32, node: i32) -> TypeFacts {
    match typefacts::facts_of(gx.facts, typid) {
        Some(tf) => tf,
        None => shape(node, "an output type the catalog does not know"),
    }
}

/// The entry's out column; a name of 64 bytes or more and a cstring column refuse by name (the out block has no form for
/// either).
fn col_of<'p>(gx: &Gx<'_, 'p>, te: &TargetEntry<'p>, node: i32) -> (OutCol, TypeFacts) {
    let typ = gravity_decompose::catalog::expr_type::type_of(te.expr).unwrap_or_else(|| gravity_decompose::catalog::expr_type::result_type(te.expr));
    let tf = facts(gx, typ, node);
    let name = if te.resjunk { "" } else { te.resname.unwrap_or("?column?") };
    if name.len() >= 64 {
        shape(node, "an output name of 64 bytes or more");
    }
    if tf.typlen == -2 {
        shape(node, "a cstring output column");
    }
    // D2643: the column's typmod is C `exprTypmod`'s (nodes_core's complete port), as ExecTypeFromTL gives the portal's
    // descriptor: a length coercion (an implicit `numeric(6,2)` over a VIRTUAL generated column's expression in
    // RETURNING), a CASE or COALESCE of one typmod, a Param, a domain coercion all name theirs, and FillPortalStore's
    // cross-check (convert_tuples_by_position) raises 42804 on a column whose typmod differs
    let c = OutCol::new(name, typ, gravity_expr::nodes::func::expr_typmod(te.expr), tf.typlen, tf.byval);
    (c.collation(gravity_decompose::catalog::expr_type::collation_of(te.expr)).origin(te.resorigtbl, te.resorigcol), tf)
}

/// Sets `px`'s out row from pipeline `k`'s last projection (`rows` a push); the columns in out order.
pub(crate) fn declare<'g>(px: &mut Px<'g, '_>, k: usize, rows: u32) -> PgVec<'g, Col> {
    let p = &px.gx.pipelines()[k];
    let node = p.node_id;
    let Some(list) = p.ops.iter().rev().find_map(|op| if let Op::Project(l) = op { Some(*l) } else { None }) else {
        shape(node, "a sink over a row with no projection")
    };
    let mut cols = ArenaVec::new_in(px.arena);
    let mut at = PgVec::new_in(px.mcx());
    for (i, n) in list.iter().enumerate() {
        let Some(te) = n.as_target_entry() else { shape(node, "a projection entry that is not a TargetEntry") };
        if te.resjunk {
            continue;
        }
        let (c, tf) = col_of(px.gx, te, node);
        at.push_in(Col { entry: i, tf, byref: (!c.typbyval).then_some(c.typlen) });
        cols.push(c);
    }
    let nbyref = at.iter().filter(|c| c.byref.is_some()).count() as u32;
    let region = if nbyref == 0 { 0 } else { out_region_bytes(nbyref, rows) };
    px.out = Some(Out { cols, rows, region });
    at
}
