//! M1's site admissions (plan §4.3): ADMIT / BIND, the host recipe of a pgrc granule site (`src.pgrc`) and of a btree
//! index site (`src.index`), ported from lowerv3-final's exec/src/admit/v3_r1.rs and v3_r1b.rs (their helpers with them:
//! REMOVE-V3 took them out of admit.rs); every other source kind is refused by the lowerer before admission. Created and
//! closed by M1 (owner M1-HOSTS); m1.rs re-exports its tables.
use gravity_decompose::graph::KeyForm;
use gravity_decompose::{Am, IndexAm, IndexKey, Source};
use gravity_hosts::dict::text_hosts::TableSpec;
use gravity_hosts::{GranuleRecipe, HostRegistry, SiteRecipe};
use gravity_program::{LaneId, SiteDesc};
use types_core::Oid;
use types_nodes::{Node, NodeList};
use types_storage::lock::NoLock;

use crate::admit::{refuse, Admit, AdmitCx, AdmitResult, Bind, BindCx, BindResult, RecipeTpl, Refused};
use crate::shapes;

pub const ADMIT: &[Admit] = &[granule, index];
pub const BIND: &[Bind] = &[bind];


fn granule(cx: &AdmitCx<'_>) -> AdmitResult {
    let (mcx, site, attnos) = (cx.mcx, cx.site, cx.attnos);
    // TABLESAMPLE over pgrc (BERNOULLI / SYSTEM): the granule recipe with the sampler on it (hosts `with_sample`)
    let sample = match cx.source {
        Source::Range { am: Am::Pgrcolumnar, .. } => None,
        Source::Sample { am: Am::Pgrcolumnar, sample, .. } => Some(sample),
        _ => return Ok(Ok(None)),
    };
    if site.lanes().iter().any(|l| l.id == LaneId::RowId) {
        return Ok(Ok(None)); // a by-rowid fetch site: M3's (m3_fetch.rs, V4R-169); a build without M3's rungs refuses it as admit.rs's generic source-without-recipe
    }
    let tables = match table_specs(site) {
        Ok(t) => t,
        Err(r) => return Ok(Err(r)),
    };
    let rel = table::table_open(mcx, site.relation as Oid, NoLock)?;
    let recipe = GranuleRecipe::from_plan_lanes(&rel, attnos, &lane_words(site, gravity_abi::ctx::lane::KIND_CODES), &packed_words(site));
    rel.close(NoLock)?;
    let recipe = match sample {
        Some(smp) => recipe.and_then(|r| r.with_sample(smp.handler as Oid, smp.repeatable.is_some())),
        None => recipe,
    };
    let mut r = match recipe {
        Ok(r) => r,
        Err(e) => return Ok(refuse("hosts", e.message())),
    };
    // PS1-05: the site's trailing open-time zone cells (an eager-safe InitPlan's value per predicate), apart from its own
    if let gravity_program::SiteKind::RangeSource { zkeys, .. } = site.kind {
        r = r.with_zone_keys(zkeys);
    }
    // M6 D2: the shredded jsonb path lanes (a part that did not elect one publishes it all-invalid), before the tables
    // over a path's code lane (R3)
    for d in site.shreds.iter() {
        // R3 (jsonb.codes): a Text path read as its codes is a code lane over the path's dictionary (with_shred_code_lane)
        let codes = d.kind == gravity_program::shred_kind::TEXT_CODES;
        // jsonops (3): a key's presence lane reads its Text path (with_shred_presence_lane)
        let presence = d.kind == gravity_program::shred_kind::PRESENCE;
        // D3038: a varlena column's octet_length lane reads the column's own streams (with_octet_lane): no path; its
        // emptiness lane (R2) the same streams, a boolean word (with_nonempty_lane); its character length lane (R3) too
        // (with_char_lane)
        if matches!(d.kind, gravity_program::shred_kind::OCTET_LENGTH | gravity_program::shred_kind::NONEMPTY | gravity_program::shred_kind::CHAR_LENGTH) {
            if (d.lane as usize) >= r.natts {
                return Ok(refuse("hosts", "a shred lane outside the recipe"));
            }
            r = match d.kind {
                gravity_program::shred_kind::NONEMPTY => r.with_nonempty_lane(d.lane as usize),
                gravity_program::shred_kind::CHAR_LENGTH => r.with_char_lane(d.lane as usize),
                _ => r.with_octet_lane(d.lane as usize),
            };
            continue;
        }
        let kind = gravity_hosts::ops::scan::pgrc::granule_source::ShredLane::kind_of(if codes || presence { gravity_program::shred_kind::TEXT } else { d.kind });
        // the site path is the keys, outermost first, joined by a NUL byte (lowerv4 jsonb_lanes `site_path`)
        let mut keys = mcx::PgVec::new_in(mcx);
        let mut utf8 = true;
        for k in site.table_arg_at(d.path).split(|&b| b == 0) {
            match core::str::from_utf8(k) {
                Ok(k) => mcx::GrowIn::push_in(&mut keys, k),
                Err(_) => {
                    utf8 = false;
                    break;
                }
            }
        }
        let (true, Some(kind)) = (utf8, kind) else {
            return Ok(refuse("hosts", "a shred lane's key or kind"));
        };
        let path = gravity_hosts::ops::scan::pgrc::granule_source::shred_path(&keys);
        if (d.lane as usize) >= r.natts {
            return Ok(refuse("hosts", "a shred lane outside the recipe"));
        }
        r = if codes {
            r.with_shred_code_lane(d.lane as usize, &path)
        } else if presence {
            r.with_shred_presence_lane(d.lane as usize, &path)
        } else {
            r.with_shred_lane(d.lane as usize, &path, kind, d.scale as i32)
        };
    }
    if let Err(why) = code_lanes(site, &tables, &r.kinds[..r.natts]) {
        return Ok(Err(why));
    }
    let r = match site.lanes().iter().position(|l| l.id == LaneId::Verdict(0)) {
        Some(k) => r.with_verdict_lane(k),
        None => r,
    };
    let r = if site.sparse_dicts { r.with_tables(tables).with_sparse_dicts().with_dict_renders(site.dict_renders) } else { r.with_tables(tables) };
    let mut r = if site.head_prefault { r.with_head_prefault() } else { r };
    for k in lens_only(site, r.natts) {
        r = r.with_lens_only(k);
    }
    // a late jsonb column is filled where one of its path lanes is invalid (the rows its compares walk)
    for d in site.shreds.iter() {
        let attno = site.out_attnos[d.lane as usize];
        if let Some(col) = site.lanes()[..r.natts].iter().position(|l| l.id == LaneId::Attr(attno)) {
            r = r.with_late_guard(col, d.lane as usize);
        }
    }
    // the entries window: the source decides per part (dictionary-coded, no deletes), else its rows
    Ok(Ok(Some(RecipeTpl::Granule(match site.entries {
        Some(SiteDesc::ENTRIES_EVERY) => r.with_entries_every(),
        Some(k) => {
            let r = r.with_entries(k as usize);
            let r = if site.entries_filtered { r.with_entries_filtered() } else { r };
            // RD-dict.entries: a declarer that keeps NULL as a value takes one NULL row per part that holds one
            let r = if site.entries_nulls { r.with_entries_nulls() } else { r };
            // D2981: a descending top's entries walk each part's codes from its highest
            if site.entries_desc { r.with_entries_desc() } else { r }
        }
        None => r,
    }))))
}

/// F3 (CORE's panic census, D296): every table lane and the entries window lie over one of the recipe's code lanes
/// (`kinds`), as the recipe's builders assert. A descriptor naming another lane is a lowerer bug, refused here by name
/// rather than raised as XX000 after admission.
fn code_lanes(site: &SiteDesc, tables: &[TableSpec], kinds: &[u8]) -> Result<(), Refused> {
    let code = |k: usize| kinds.get(k) == Some(&gravity_abi::ctx::lane::KIND_CODES);
    if let Some(t) = tables.iter().find(|t| !code(t.over as usize)) {
        return refuse("hosts", format!("a table lane of site relation {} is over lane {}, not one of the site's code lanes", site.relation, t.over)); // alloc: Cold
    }
    match site.entries {
        // PS1-19: every attribute lane is published as entries, so every one is a code lane
        Some(SiteDesc::ENTRIES_EVERY) => match (0..site.natts()).find(|&k| !code(k)) {
            Some(k) => refuse("hosts", format!("the entries lane {k} (every lane) of site relation {} is not one of the site's code lanes", site.relation)), // alloc: Cold
            None if site.natts() < 2 => refuse("hosts", format!("the every-lane entries window of site relation {} has under two lanes", site.relation)), // alloc: Cold
            None => Ok(()),
        },
        Some(k) if !code(k as usize) => refuse("hosts", format!("the entries lane {k} of site relation {} is not one of the site's code lanes", site.relation)), // alloc: Cold
        _ => Ok(()),
    }
}

fn index(cx: &AdmitCx<'_>) -> AdmitResult {
    let (pstmt, mcx, site, node_id, attnos) = (cx.pstmt, cx.mcx, cx.site, cx.node_id, cx.attnos);
    let Source::Index { scanrelid, indexid, backward, only, keys, iam, .. } = cx.source else { return Ok(Ok(None)) };
    if !m1_index(*iam, keys) || gravity_lowerv4::rerun::params_key::hosts_bound(keys, &site.param_slots) {
        return Ok(Ok(None));
    }
    let Some(r) = shapes::rte(pstmt, *scanrelid as u32) else {
        return Ok(refuse("hosts", "the index scan's range table entry"));
    };
    let Some(pn) = plan_node(pstmt.planTree, node_id).or_else(|| pstmt.subplans.iter().find_map(|s| plan_node(s, node_id))) else {
        return Ok(refuse("hosts", "the index scan's plan node"));
    };
    let Some(indexqual) = pn.as_index_scan().map(|s| &s.indexqual).or_else(|| pn.as_index_only_scan().map(|s| &s.indexqual)) else {
        return Ok(refuse("hosts", "the index scan's clauses"));
    };
    let heap = table::table_open(mcx, site.relation as Oid, NoLock)?;
    let index = match indexam::index_open(mcx, *indexid as Oid, r.rellockmode) {
        Ok(i) => i,
        Err(e) => {
            heap.close(NoLock)?;
            return Err(e);
        }
    };
    // V4R-108: a constant `= ANY` array is a key word (lowerv4 `key_words`), bound at the seek, never the recipe's image
    let recipe = SiteRecipe::from_plan_words(&heap, &index, indexqual, r.rellockmode, attnos).map(|r| r.with_backward(*backward).with_only(*only).with_row_budget(gravity_hosts::ops::scan::index_window::limit_budget(pstmt, node_id)).with_run_bound(gravity_hosts::ops::scan::index_window::run_bound(pstmt, node_id)).with_plan_node(node_id));
    // D3014O: a site a worker probes keeps the recipe's worker form (hosts index_probe.rs)
    let recipe = recipe.map(|r| if site.worker_probe() { r.with_worker(&heap) } else { r });
    // an index-only site the pool reads keeps the recipe's leaf form (hosts index_leaves.rs); none: exec runs its pipeline
    // on the leader
    let recipe = recipe.and_then(|r| if site.leaves() { r.with_leaves(&heap, &index) } else { Ok(r) });
    // an ordered site the pool reads in key ranges keeps the recipe's ranges form (hosts index_ranges.rs); none: exec runs
    // its pipeline on the leader
    let recipe = recipe.and_then(|r| if site.ranges() { r.with_ranges(&heap, &index) } else { Ok(r) });
    index.close(NoLock)?;
    heap.close(NoLock)?;
    match recipe {
        Ok(r) => Ok(Ok(Some(RecipeTpl::Index(r)))),
        Err(e) => Ok(refuse("hosts", e.message())),
    }
}

/// V4R-36: the index sites M1's arm owns, a btree walk whose keys the host binds (no ORDER BY operator, every `= ANY`
/// key over a constant array). Any other index site (a non-btree access method, a KNN reorder, an array key over a
/// parameter or an expression) is another rung's: it falls through to M2's arm instead of being refused here.
fn m1_index(iam: IndexAm, keys: &[IndexKey<'_>]) -> bool {
    iam == IndexAm::Btree
        && keys.iter().all(|k| match k.form {
            KeyForm::OrderBy | KeyForm::AmOp | KeyForm::AmAny => false,
            KeyForm::Any => k.value.as_const().is_some(),
            // D1016: a key computed from executor parameters is M8's (v4/m8_param_key.rs, evaluated at every seek)
            KeyForm::Op => !gravity_lowerv4::rerun::params_key::computed(k.value),
            KeyForm::Row => !k.value.as_row_compare_expr().is_some_and(|rc| rc.rargs.iter().any(gravity_lowerv4::rerun::params_key::computed)),
            KeyForm::NotNull | KeyForm::Null => true,
        })
}

/// Recipe `t` into this execution's registry.
fn bind(t: &RecipeTpl, reg: &mut HostRegistry, cx: &BindCx<'_>, exprs: &mut shapes::KeyExprs) -> BindResult {
    // the first bind of every recipe: copy only the kinds this one binds
    match t {
        // D1536: an EXPLAIN ANALYZE execution's recipe counts the rows its zone verdicts skip (Rows Removed by Filter)
        // planprod P6-c2: the walk's recipe out of its set's root (`GranuleTpl::bind_copy`, `SiteTpl::bind_copy`)
        RecipeTpl::Granule(r) => reg.push_granule(r.bind_copy().with_verdict_cell().with_skipped_rows(crate::admit::is_instrumented())),
        RecipeTpl::Index(r) => reg.push(prefilter(r.bind_copy(), cx, exprs)?),
        _ => return Ok(Ok(false)),
    }
    Ok(Ok(true))
}

/// An index walk's qual pre-check (hosts `SiteRecipe::filter`): this execution's IndexScan qual, compiled over its extern
/// parameters, checked on each fetched heap row before the window copies its cells, as ExecScan evaluates the qual
/// before its projection (execScan.c). The program applies the qual to every row it gets, so a row the pre-check keeps
/// meets it twice: only a qual of built-in calls over the scan row, constants and extern parameters qualifies (a second
/// call shows nothing; an InitPlan's or a nest's value is the program's to bind), nothing volatile (`compile_prefilter`,
/// whose calls' results a per-row root holds, D2988).
/// An InitPlan's or an Append arm's registry binds without the statement and checks nothing early. An instrumented run
/// checks early too: a row the pre-check drops fails the scan's qual, which the scan's count is taken after. Perf: the pre-check is
/// armed for a qual compiled to one fused compare (execexpr `Kernel::QualScanVarCmpConst`) or a walk whose copy of a row is dear (hosts
/// `SiteRecipe::copy_dear`); an interpreted qual program costs a checked row more than a few by-value cells' copy saves,
/// at any drop rate.
fn prefilter(r: SiteRecipe, cx: &BindCx<'_>, exprs: &mut shapes::KeyExprs) -> types_error::PgResult<SiteRecipe> {
    // oltpfix G1d (D3065): a point walk (hosts `SiteRecipe::point`) yields at most one row, which the pre-check could only
    // drop early once: compiling it costs every statement more, and a recipe with a filter never parks
    if r.only || r.filter.is_some() || r.point() {
        return Ok(r);
    }
    let scan = cx.plan.zip(r.plan_node()).and_then(|(p, id)| plan_node(p.planTree, id).or_else(|| p.subplans.iter().find_map(|s| plan_node(s, id))));
    let Some(qual) = scan.and_then(|n| n.as_index_scan().filter(|s| s.indexid == r.index).map(|s| &s.scan.plan.qual)) else {
        return Ok(r);
    };
    armed(r, qual, cx, exprs)
}

/// D2988: a one-index bitmap leaf's qual pre-check (hosts bitmap_source.rs, `PageBatch::fill`): the qual of this
/// execution's BitmapHeapScan whose `bitmapqualorig` is the source's recheck `recheck` (by identity, as sites_heap.rs
/// `plan_recheck`), checked on each heap row the recheck keeps, before the window copies its cells, under `prefilter`'s
/// rules. A recheck decompose rebuilt, or no statement (an InitPlan's or an Append arm's registry), checks nothing early.
pub(crate) fn bitmap_prefilter(r: SiteRecipe, recheck: &NodeList<'_>, cx: &BindCx<'_>, exprs: &mut shapes::KeyExprs) -> types_error::PgResult<SiteRecipe> {
    if r.filter.is_some() {
        return Ok(r);
    }
    let Some(qual) = cx.plan.and_then(|p| crate::ops::scan::sites_heap::heap_qual(p, recheck)) else {
        return Ok(r);
    };
    let r = armed(r, qual, cx, exprs)?;
    if r.filter.is_some() {
        fires::fire("bitmap.prefilter"); // R-KEEP's count of statements whose bitmap walk checks its qual early
    }
    Ok(r)
}

/// `prefilter`'s compile of scan qual `qual` into `r`'s pre-check, when it qualifies.
fn armed(r: SiteRecipe, qual: &NodeList<'static>, cx: &BindCx<'_>, exprs: &mut shapes::KeyExprs) -> types_error::PgResult<SiteRecipe> {
    if qual.is_nil() || (qual.len() > 1 && !r.copy_dear()) || !qual.iter().all(builtin_row_qual) {
        return Ok(r);
    }
    match shapes::compile_prefilter(cx.mcx, qual, shapes::param_bind(exprs), !r.copy_dear())? {
        Some(f) => {
            exprs.filters += 1;
            Ok(r.with_filter(Some(f)))
        }
        None => Ok(r),
    }
}

/// `n` reads only the scan row's columns, constants and extern parameters, through built-in functions and operators.
fn builtin_row_qual(n: Node<'_>) -> bool {
    stack_depth_core::check_stack_depth_or_panic();
    use types_nodes::NodeTag as T;
    let builtin = |f: u32| f != 0 && !gravity_decompose::effects::user_code(f);
    let own = match n.node_tag() {
        T::T_Var => n.as_var().is_some_and(|v| v.varattno > 0),
        // D2834: never a parameter left to evaluation time (the pre-check's list holds no fetch for it)
        T::T_Param => n.as_param().is_some_and(|p| p.paramkind == types_nodes::primnodes::ParamKind::PARAM_EXTERN && !gravity_program::externs::deferred(p.paramid)),
        T::T_Const | T::T_RelabelType | T::T_BoolExpr | T::T_NullTest | T::T_BooleanTest | T::T_CoalesceExpr | T::T_ArrayExpr => true,
        T::T_FuncExpr => n.as_func_expr().is_some_and(|f| builtin(f.funcid)),
        T::T_OpExpr => n.as_op_expr().is_some_and(|o| builtin(o.opfuncid)),
        T::T_DistinctExpr => n.as_distinct_expr().is_some_and(|o| builtin(o.opfuncid)),
        T::T_NullIfExpr => n.as_null_if_expr().is_some_and(|o| builtin(o.opfuncid)),
        T::T_ScalarArrayOpExpr => n.as_scalar_array_op_expr().is_some_and(|o| builtin(o.opfuncid) && [o.hashfuncid, o.negfuncid].iter().all(|&f| f == 0 || builtin(f))),
        _ => false,
    };
    let mut kids = own;
    own && gravity_decompose::expr::kids(n, &mut |c| kids &= builtin_row_qual(c)) && kids
}

/// The plan node numbered `id` under `root` (lefttree / righttree, Append / MergeAppend arms, a SubqueryScan's subplan).
fn plan_node<'p>(root: Option<Node<'p>>, id: i32) -> Option<Node<'p>> {
    stack_depth_core::check_stack_depth_or_panic();
    let n = root?;
    let p = n.as_plan()?;
    if p.plan_node_id == id {
        return Some(n);
    }
    if let Some(f) = plan_node(p.lefttree, id).or_else(|| plan_node(p.righttree, id)) {
        return Some(f);
    }
    let lists: [Option<&NodeList<'p>>; 2] = [n.as_append().map(|a| &a.appendplans), n.as_merge_append().map(|m| &m.mergeplans)];
    for l in lists.into_iter().flatten() {
        if let Some(f) = l.iter().find_map(|c| plan_node(Some(c), id)) {
            return Some(f);
        }
    }
    n.as_subquery_scan().and_then(|s| plan_node(s.subplan, id))
}

/// The site's attribute lanes of lane kind `kind` as a bit set.
fn lane_words(site: &SiteDesc, kind: u8) -> [u64; 64] {
    let mut w = [0u64; 64];
    for (k, l) in site.lanes().iter().enumerate().take(site.natts()) {
        // a shred path's code lane is the path's (`with_shred_code_lane`), not its jsonb column's
        if l.kind == kind && k / 64 < w.len() && site.shred_of(k).is_none() {
            w[k / 64] |= 1 << (k % 64);
        }
    }
    w
}

/// N14: the site's attribute lanes that read an unconstrained numeric as its packed codes (`Rep::PackedNumDyn`, decompose
/// `packed_dyn_lanes`) as a bit set: the recipe's `dyn_scale` lanes.
fn packed_words(site: &SiteDesc) -> [u64; 64] {
    let mut w = [0u64; 64];
    for (k, l) in site.lanes().iter().enumerate().take(site.natts()) {
        if l.rep == gravity_program::Rep::PackedNumDyn && k / 64 < w.len() {
            w[k / 64] |= 1 << (k % 64);
        }
    }
    w
}

/// D3038 (A): the code lanes of `site` (below `natts`) with an `OCTET_LENGTH` table the lowering marked `FLAG_SOLE` (the
/// pipeline reads the column for its lengths and emptiness alone), whose every table is a function of the byte length,
/// and that no entries window publishes: a verbatim part that sealed the column's octet-length lane codes such a lane by
/// length (hosts `with_lens_only`). A function of the length: such an `OCTET_LENGTH` table, or an equality set whose
/// constants are the one value of their length: a static `EQ` set of '' alone, under a collation where equal strings are
/// equal bytes (under a nondeterministic one, ignorable characters equal '').
fn lens_only(site: &SiteDesc, natts: usize) -> impl Iterator<Item = usize> + '_ {
    use gravity_program::table_fn;
    let sole = |d: &gravity_program::TableDecl| d.func == table_fn::OCTET_LENGTH && d.flags & table_fn::FLAG_SOLE != 0;
    let by_length = move |d: &gravity_program::TableDecl| {
        sole(d) || d.func == table_fn::EQ && d.flags & table_fn::FLAG_BOUND == 0 && table_fn::values_all_empty(site.table_arg(d, 0)) && gravity_fn::typesem::expr::built_coll_bytes(d.collation)
    };
    (0..natts.min(site.lanes().len())).filter(move |&k| {
        let over = || site.tables.iter().filter(move |d| d.over as usize == k);
        over().any(sole) && over().all(by_length) && site.lanes()[k].kind == gravity_abi::ctx::lane::KIND_CODES && site.entries.is_none()
    })
}

/// The granule recipe's `TableSpec`s in declared order: table lane `i` is `site.tables[i]` through `text_hosts::spec_of`,
/// refused by name when a table has no host spec, sits over a lane that is not a code lane, or the lanes and the
/// declarations disagree.
fn table_specs(site: &SiteDesc) -> Result<Vec<TableSpec>, Refused> {
    let natts = site.natts();
    let declared = site.tables.len();
    let lanes = site.lanes().iter().filter(|l| matches!(l.id, LaneId::Table(_))).count();
    if lanes != declared {
        return refuse("hosts", format!("site relation {} binds {lanes} table lanes and declares {declared}", site.relation)); // alloc: Cold
    }
    let mut out = Vec::with_capacity(declared); // alloc: Program
    for (i, d) in site.tables.iter().enumerate() {
        let over = d.over as usize;
        let code = site.lanes().get(over).is_some_and(|l| l.kind == gravity_abi::ctx::lane::KIND_CODES);
        if over >= natts || !code {
            return refuse("hosts", format!("table lane {i} of site relation {} (function {}) is over lane {over}, not a code lane", site.relation, d.func)); // alloc: Cold
        }
        // F3: spec_of slices its arguments out of the site's table bytes (SiteDesc::table_arg); one past them is refused
        if d.args.iter().any(|&(o, n)| u64::from(o) + u64::from(n) > site.table_bytes.len() as u64) {
            return refuse("hosts", format!("table lane {i} of site relation {}: an argument outside the site's table bytes", site.relation)); // alloc: Cold
        }
        match gravity_hosts::dict::text_hosts::spec_of(site, i) {
            Some(s) => out.push(s),
            None => return refuse("hosts", format!("table lane {i} of site relation {}: table function {} has no host spec", site.relation, d.func)), // alloc: Cold
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_hosts::dict::text_hosts::TableFn;
    use gravity_program::{table_fn, Arena, TableDecl};

    /// V4R-36: a btree walk over operator, constant-array, null-test and row keys stays M1's arm; a non-btree access
    /// method, a KNN ORDER BY key and an array key over a non-constant array fall through to the next rung.
    #[test]
    fn a_non_btree_or_knn_or_param_array_index_site_falls_through() {
        let ctx = mcx::MemoryContext::new("v4r-36");
        let mcx = ctx.mcx();
        let c = types_nodes::Node::mk_const(mcx, types_core::INT4OID, -1, 0, 4, datum::Datum::from_i32(1), false, true).unwrap();
        let v = types_nodes::Node::mk_var(mcx, 1, 1, types_core::INT4OID, -1, 0, 0).unwrap();
        let key = |form, value| IndexKey { attno: 1, col: 1, opno: 96, value, form };
        let btree = [key(KeyForm::Op, v), key(KeyForm::Any, c), key(KeyForm::NotNull, c), key(KeyForm::Row, c)];
        assert!(m1_index(IndexAm::Btree, &btree), "M1's btree forms stay M1's");
        assert!(m1_index(IndexAm::Btree, &[]), "the key-less ordered walk stays M1's");
        for am in [IndexAm::Hash, IndexAm::Gist, IndexAm::Spgist, IndexAm::Other] {
            assert!(!m1_index(am, &[key(KeyForm::AmOp, c)]), "{am:?}");
            assert!(!m1_index(am, &[]), "{am:?} without keys");
        }
        assert!(!m1_index(IndexAm::Btree, &[key(KeyForm::Op, c), key(KeyForm::OrderBy, c)]), "KNN reorder");
        assert!(!m1_index(IndexAm::Btree, &[key(KeyForm::Any, v)]), "an array key over an expression");
    }

    /// F3: a table lane's argument past the site's table bytes, a table lane or an entries window over a lane that is
    /// not a code lane: each a named refusal, never the builders' panic.
    #[test]
    fn a_bad_table_argument_or_code_lane_is_refused_by_name() {
        let arena = Arena::new(1 << 16);
        let like = |past: u32| {
            let mut site = SiteDesc::range_source(arena.handle(), 7, &[1, 2]).codes_lane(0);
            let (o, n) = site.table_bytes_of(b"%ab%");
            site.declare_table(TableDecl { over: 0, func: table_fn::LIKE, flags: 0, collation: 0, encoding: 0, args: [(o, n + past), (0, 0)] }).expect("table lane");
            site
        };
        assert!(table_specs(&like(1)).is_err_and(|r| r.what.contains("outside the site's table bytes")));
        let mut site = like(0);
        let specs = table_specs(&site).expect("in bounds");
        let codes = [gravity_abi::ctx::lane::KIND_CODES, gravity_abi::ctx::lane::KIND_BYVAL];
        assert!(code_lanes(&site, &specs, &codes).is_ok());
        let over1 = [TableSpec { over: 1, func: TableFn::Fingerprint }];
        assert!(code_lanes(&site, &over1, &codes).is_err_and(|r| r.what.contains("over lane 1")));
        for k in [1u8, 2] {
            site.entries = Some(k);
            assert!(code_lanes(&site, &specs, &codes).is_err_and(|r| r.what.contains("entries lane")), "entries over lane {k}");
        }
        // PS1-19: every lane at once needs every attribute lane a code lane (lane 1 is byval here)
        site.entries = Some(SiteDesc::ENTRIES_EVERY);
        assert!(code_lanes(&site, &specs, &codes).is_err_and(|r| r.what.contains("entries lane 1 (every lane)")));
        assert!(code_lanes(&site, &specs, &[gravity_abi::ctx::lane::KIND_CODES; 2]).is_ok());
    }
}
