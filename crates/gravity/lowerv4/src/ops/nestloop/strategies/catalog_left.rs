//! Kind `nest.catalog` and road `nest.catalog_left` (cards/M3.md L6; M3 decision 16, V4R-142): psql `\d`'s lookup nested
//! loop. Owner M3-KEYED.
//!
//! decompose join.rs `catalog_lookup` takes the lookup shape alone (props `catalog_nest`: an INNER or LEFT nest, no
//! parameters, a heap / index scan over a heap / index scan of filters and projections that calls nothing VOLATILE, one
//! join qual `outer = inner`, the type's raise-free equality, whose inner column is the inner relation's whole primary
//! key, chosen when the planner's estimates say the Nest compares more pairs than the table reads rows; path100 DC-14
//! generalized psql's pg_class x pg_namespace `oid = oid` to it) and maps it to the hash join's form: the inner closes into M1's HashBuild keyed by its side of the qual, the outer
//! probes it (`JoinDesc::Probe { catalog: true }`, outer-first, in a serial pipeline). The probe is join.inner's step
//! (join/probe_inner.rs) in its catalog form (join/probe_fill.rs): NL-exact. The inner is read once (no parameters), after
//! the first outer row reaches the probe (outer-first: D237's deferred build); an empty build ends nothing; a NULL outer
//! key or a key no record holds is a miss, and a LEFT lookup hands the row on with the inner columns NULL; the rows keep
//! the outer's order (a serial pipeline: the Nest's pathkeys). The join qual is the key alone, so no other qual is
//! evaluated over fewer pairs than the Nest evaluates. Everything outside the shape stays a Nest: NotBuilt nest.loop (M8).
use gravity_decompose::join::JoinDesc;
use gravity_decompose::Op;

use crate::face::kinds::KindId;
use crate::frame::frame::StepMaker;
use crate::frame::px::Px;
use crate::admit::refusal::Reason;
use crate::ops::Table;

crate::strategies! {
    /// Psql lookup nested loop (pg_class x pg_namespace): the inner read once into a keyed array, NULL-extended on a
    /// miss.
    NEST_CATALOG_LEFT = 78, "nest.catalog_left", CatalogNest;
}

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::NestCatalog, &StepMaker(make))], strategies: &[(NEST_CATALOG_LEFT, &())], ..crate::ops::NONE };

/// The catalog probe `j` of pipeline `k`: join.inner's step maker, which reads the op's `catalog` flag.
fn make<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> mcx::PgBox<'g, dyn crate::face::step::StepMech + 'g> {
    let p = &px.gx.pipelines()[k];
    if !matches!(p.ops.get(j), Some(Op::Join(JoinDesc::Probe { catalog: true, .. }))) {
        crate::refuse!(p.node_id, Reason::Bug { payload: "nest.catalog over an op that is not a catalog probe" });
    }
    (crate::frame::iface::kind_impl::<StepMaker>(&p.node_id, KindId::JoinInner).0)(px, k, j)
}
