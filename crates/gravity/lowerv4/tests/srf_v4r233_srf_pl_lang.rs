//! V4R-233 (M5 Decision 23, L0-a): props decides srf.pl by the function's language, in FROM and in the target list. A SQL /
//! PL body runs nested statements through SPI (ENTRY's re-entrancy), whatever its OID; a C or internal one does not. Before,
//! srf.pl was `funcid >= 16384` in FROM only: a PL SRF in a SELECT list (golden srf-009 `SELECT three(), a FROM t`) named
//! srf.projectset alone, and a C extension SRF (pageinspect) read as srf.pl. The rig's catalog answers the builtins from
//! pg_proc.dat; `Lang` gives srf-009's user function `three()` (fn16404) a language, the rest delegated to the rig.
use gravity_lowerv4_it as common;

use std::collections::BTreeSet;

use gravity_expr::catalog::{DomainFacts, FuncFacts, IoFacts, TextFacts, TypeFacts};
use gravity_expr::Catalog;
use gravity_fn::{Shim, Tier};
use gravity_lowerv4::graph::Gx;
use gravity_lowerv4::props::Props;
use gravity_program::Arena;
use mcx::MemoryContext;
use types_core::Oid;
use types_error::PgResult;

const THREE: Oid = 16404;
const LANG_C: Oid = 13;
const LANG_SQL: Oid = 14;

/// The rig's catalog, with `THREE` a function of language `lang`.
struct Lang {
    lang: Oid,
}

impl<'mcx> Catalog<'mcx> for Lang {
    fn func(&self, funcid: Oid) -> PgResult<FuncFacts> {
        if funcid == THREE {
            return Ok(FuncFacts { strict: false, retset: true, nargs: 0, volatility: Some(b'i'), parallel: Some(b'u'), node_arg: false, leakproof: Some(false), prolang: Some(self.lang), body: 0 });
        }
        common::RigCatalog::get().func(funcid)
    }
    fn opcode(&self, opno: Oid) -> PgResult<Oid> {
        common::RigCatalog::get().opcode(opno)
    }
    fn op_input_types(&self, opno: Oid) -> PgResult<(Oid, Oid)> {
        common::RigCatalog::get().op_input_types(opno)
    }
    fn btree_cmp_proc(&self, opfamily: Oid, left: Oid, right: Oid) -> PgResult<Oid> {
        common::RigCatalog::get().btree_cmp_proc(opfamily, left, right)
    }
    fn opclass(&self, opclass: Oid) -> PgResult<(Oid, Oid)> {
        common::RigCatalog::get().opclass(opclass)
    }
    fn type_facts(&self, typid: Oid) -> PgResult<TypeFacts> {
        common::RigCatalog::get().type_facts(typid)
    }
    fn type_cmp_proc(&self, typid: Oid) -> PgResult<Oid> {
        common::RigCatalog::get().type_cmp_proc(typid)
    }
    fn base_type(&self, typid: Oid, typmod: &mut i32) -> PgResult<Oid> {
        common::RigCatalog::get().base_type(typid, typmod)
    }
    fn domain(&self, typid: Oid) -> PgResult<DomainFacts<'mcx>> {
        common::RigCatalog::get().domain(typid)
    }
    fn io(&self, typid: Oid) -> PgResult<IoFacts> {
        common::RigCatalog::get().io(typid)
    }
    fn text(&self, collation: Oid) -> PgResult<TextFacts> {
        common::RigCatalog::get().text(collation)
    }
    fn tier(&self, funcid: Oid) -> Tier {
        common::RigCatalog::get().tier(funcid)
    }
    fn shim(&self, funcid: Oid) -> Option<&'static Shim> {
        common::RigCatalog::get().shim(funcid)
    }
}

/// Plan fixture `id`'s kinds, props read through `catalog`.
fn kinds<C: for<'m> Catalog<'m>>(id: &str, catalog: &C) -> BTreeSet<String> {
    let p = common::plans_dir().join(format!("{id}.plan"));
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    common::RigCatalog::install_seams();
    let ctx = MemoryContext::new("v4r233 fixture");
    let fx = gravity_decompose::corpus::load(ctx.mcx(), &text).unwrap_or_else(|e| panic!("{id}: {e}"));
    let arena = Arena::new(1 << 20);
    let graph = gravity_decompose::decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).unwrap_or_else(|_| panic!("{id}: decompose declined"));
    of(&graph, &fx.facts, catalog)
}

/// `Props::of` over an erased facts face (as the rig's `props_fixture` passes it).
fn of<C: for<'m> Catalog<'m>>(graph: &gravity_decompose::PipelineGraph<'_>, facts: &dyn gravity_decompose::Facts, catalog: &C) -> BTreeSet<String> {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    let p = Props::of(&Gx { graph, facts, catalog, over: None, mcx: rig.mcx() });
    p.kinds.iter().map(|k| k.name().to_owned()).collect()
}

#[test]
fn a_sql_or_pl_srf_in_the_target_list_names_srf_pl() {
    // srf-009 `SELECT three(), a FROM t WHERE a < 3`: a SQL-language set-returning function in the SELECT list
    let k = kinds("srf-009", &Lang { lang: LANG_SQL });
    assert!(k.contains("srf.projectset") && k.contains("srf.pl"), "srf-009 with three() in SQL: {k:?}");
}

#[test]
fn a_c_srf_in_the_target_list_is_not_srf_pl() {
    // G4: the same statement over a C-language function (an extension's) runs no nested statement
    let k = kinds("srf-009", &Lang { lang: LANG_C });
    assert!(k.contains("srf.projectset") && !k.contains("srf.pl"), "srf-009 with three() in C: {k:?}");
}

#[test]
fn a_library_srf_in_from_is_srf_function_alone() {
    // G4: srf-001's generate_series(1, 500) (internal) in FROM
    let k = kinds("srf-001", common::RigCatalog::get());
    assert!(k.contains("srf.function") && !k.contains("srf.pl"), "srf-001: {k:?}");
}
