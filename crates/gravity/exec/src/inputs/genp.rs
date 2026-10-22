//! GENP-1 (D3004; PLANNER-CHANGES GENP-1): a generic plan's scan estimates for this execution's bound parameters. A
//! generic plan estimates `r = $1` once for every value; Gravity sizes its grant and its lowering from those rows, so a
//! value that keeps every row runs on a grant cut for a few (D025: the owner phase's serial fallback). At the admission of
//! a generic plan, each sequential scan over a relation of some size whose quals read an extern parameter is estimated
//! again as a custom plan's would be (`planner_seams::scan_restriction_rows`: PostgreSQL's clauselist_selectivity with the
//! values), and the nodes above it that hand every row on take the same factor. The walk reads them (decompose
//! `admit::rescaling`); PostgreSQL's plan is unchanged.
//!
//! Skipped where it cannot matter: a plan with no such scan (an index lookup, an OLTP plan, a statement with no extern
//! parameter in a scan qual) costs one walk of its nodes and no estimate. A re-estimate within a factor of two of the plan's
//! keeps the plan's rows (`octave`), so a value the generic plan already fits admits as before. The plan memo serves the
//! admission again only to an execution whose re-estimates fall in the same octaves (`hold`); each admission caches its
//! answers per parameter values (`Cache`, at most `CACHE_MAX`), so a repeated EXECUTE pays a hash of its values.
use core::cell::RefCell;

use types_core::Oid;
use types_error::PgResult;
use types_nodes::plannodes::{PlannedStmt, Plan};
use types_nodes::{Node, NodeList, NodeTag};
use types_portal::ParamListHandle;

use gravity_program::cost::runtime::{GENP_CACHE_MAX as CACHE_MAX, GENP_MIN_SCAN_COST as MIN_SCAN_COST};

/// One re-estimated scan: its node, relation, range-table index and quals, the plan's rows, and the nodes above it that
/// hand every row on (`(node, plan rows)`).
pub struct Site {
    pub node: i32,
    pub relid: Oid,
    pub varno: u32,
    pub quals: &'static NodeList<'static>,
    pub rows: f64,
    pub ups: Vec<(i32, f64)>,
}

/// An admission's re-estimate state: its sites, the octaves it was lowered under, and its answers per parameter values.
#[derive(Default)]
pub struct Genp {
    pub sites: Vec<Site>,
    pub octaves: Box<[i8]>,
    pub cache: RefCell<Vec<(u64, Box<[i8]>)>>,
}

/// The re-estimate's octave against the plan's rows: 0 within a factor of two, else the signed log2 of the ratio.
pub fn octave(now: f64, plan: f64) -> i8 {
    let r = now.max(1.0) / plan.max(1.0);
    if (0.5..=2.0).contains(&r) {
        return 0;
    }
    r.log2().round().clamp(-60.0, 60.0) as i8
}

/// The sequential scans of `pstmt` whose quals read an extern parameter over a relation of some size, each with the nodes
/// above it that hand every row on. None without parameters.
pub fn sites(pstmt: &'static PlannedStmt<'static>) -> Vec<Site> {
    let mut out = Vec::new(); // alloc: Program
    if let Some(root) = pstmt.planTree {
        let cx = mcx::MemoryContext::new_bump("GravityGenpWalk"); // the walk's lists: gone with it
        walk(pstmt, root, &mut out, cx.mcx());
    }
    out
}

/// `n`'s sites into `out`; returns the sites (indexes into `out`) whose rows `n` hands on unchanged.
fn walk<'m>(pstmt: &'static PlannedStmt<'static>, n: Node<'static>, out: &mut Vec<Site>, m: mcx::Mcx<'m>) -> mcx::PgVec<'m, usize> {
    stack_depth_core::check_stack_depth_or_panic();
    let Some(p) = n.as_plan() else { return mcx::PgVec::new_in(m) };
    let mut below = mcx::PgVec::new_in(m);
    for c in [p.lefttree, p.righttree].into_iter().flatten() {
        mcx::GrowIn::extend_in(&mut below, walk(pstmt, c, out, m));
    }
    if let Some(a) = n.as_append() {
        for c in a.appendplans.iter() {
            walk(pstmt, c, out, m);
        }
    }
    if let Some(a) = n.as_merge_append() {
        for c in a.mergeplans.iter() {
            walk(pstmt, c, out, m);
        }
    }
    if let Some(s) = n.as_subquery_scan() {
        if let Some(c) = s.subplan {
            walk(pstmt, c, out, m);
        }
    }
    if n.node_tag() == NodeTag::T_SeqScan {
        if let Some(site) = seq_site(pstmt, n, p) {
            out.push(site); // alloc: Program
            return mcx::vec_elem_in(m, out.len() - 1, 1);
        }
        return mcx::PgVec::new_in(m);
    }
    // the nodes that hand on every row their one child gives (no qual of their own)
    let hands_on = p.qual.is_nil()
        && p.righttree.is_none()
        && matches!(n.node_tag(), NodeTag::T_Hash | NodeTag::T_Sort | NodeTag::T_IncrementalSort | NodeTag::T_Material | NodeTag::T_Gather | NodeTag::T_GatherMerge)
        || n.as_result().is_some_and(|r| r.resconstantqual.is_none() && p.qual.is_nil() && p.righttree.is_none());
    if !hands_on {
        return mcx::PgVec::new_in(m);
    }
    for &i in &below {
        out[i].ups.push((p.plan_node_id, p.plan_rows)); // alloc: Program
    }
    below
}

fn seq_site(pstmt: &'static PlannedStmt<'static>, n: Node<'static>, p: &'static Plan<'static>) -> Option<Site> {
    let s = n.as_seq_scan()?;
    if p.total_cost < MIN_SCAN_COST || p.qual.is_nil() || !p.qual.iter().any(gravity_decompose::params::reads_extern) {
        return None;
    }
    let varno = s.scan.scanrelid;
    let rte = pstmt.rtable.nth((varno as usize).checked_sub(1)?).as_range_tbl_entry()?;
    if rte.relid == 0 || rte.inh {
        return None;
    }
    Some(Site { node: p.plan_node_id, relid: rte.relid, varno, quals: &p.qual, rows: p.plan_rows, ups: Vec::new() }) // alloc: Program
}

/// The custom plan's rows of each site under `params` (`None`: the seam declined the relation).
pub fn estimate_rows(sites: &[Site], params: ParamListHandle) -> PgResult<Vec<Option<f64>>> {
    let cx = mcx::MemoryContext::new_bump("GravityGenp");
    let mut out = Vec::with_capacity(sites.len()); // alloc: Program
    for s in sites {
        let quals: mcx::PgVec<'_, Node<'_>> = mcx::CollectIn::collect_in(s.quals.iter(), cx.mcx());
        out.push(planner_seams::scan_restriction_rows::call(cx.mcx(), s.relid, s.varno, &quals, params)?); // alloc: Program
    }
    Ok(out)
}

/// The custom plan's rows of each site under `params`, as octaves of the plan's rows.
pub fn estimate(sites: &[Site], params: ParamListHandle) -> PgResult<Box<[i8]>> {
    let rows = estimate_rows(sites, params)?;
    Ok(sites.iter().zip(rows).map(|(s, r)| r.map_or(0, |r| octave(r, s.rows))).collect()) // alloc: Program
}

/// The node rows an admission under `octaves` lowers with: each moved site's rows and its hand-on nodes', scaled by the
/// octave's factor.
pub fn rows(sites: &[Site], octaves: &[i8]) -> Vec<(i32, f64)> {
    let mut out = Vec::new(); // alloc: Program
    for (s, &o) in sites.iter().zip(octaves) {
        if o == 0 {
            continue;
        }
        let f = 2f64.powi(o as i32);
        out.push((s.node, (s.rows * f).max(1.0))); // alloc: Program
        for &(id, r) in &s.ups {
            out.push((id, (r * f).max(1.0))); // alloc: Program
        }
    }
    out
}

/// A fingerprint of this execution's parameter values: a by-value datum's word, a varlena's in-line payload (decompose
/// `expr::varlena_payload`), any other by-reference value its address (which only misses the cache).
pub fn params_fp(params: ParamListHandle) -> u64 {
    if params.is_null() {
        return 0;
    }
    types_portal::params::with(params, |ps| {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut mix = |x: u64| h = (h ^ x).wrapping_mul(0x0000_0100_0000_01b3);
        for p in ps.iter() {
            mix(p.ptype as u64 | (p.isnull as u64) << 40);
            if p.isnull {
                continue;
            }
            let payload = match gravity_fn::typesem::storage::len_byval(p.ptype) {
                Ok((-1, false)) => gravity_decompose::expr::varlena_payload(p.value.as_usize() as *const u8),
                _ => None,
            };
            match payload {
                Some(bytes) => {
                    mix(bytes.len() as u64);
                    for c in bytes.chunks(8) {
                        let mut w = [0u8; 8];
                        w[..c.len()].copy_from_slice(c);
                        mix(u64::from_le_bytes(w));
                    }
                }
                None => mix(p.value.as_u64()),
            }
        }
        h
    })
}

impl Genp {
    /// This execution's octaves: cached per parameter values, else estimated and cached.
    pub fn now(&self, params: ParamListHandle) -> PgResult<Box<[i8]>> {
        let fp = params_fp(params);
        if let Some(o) = self.cache.borrow().iter().find(|x| x.0 == fp).map(|x| x.1.clone()) {
            return Ok(o);
        }
        let o = estimate(&self.sites, params)?;
        let mut c = self.cache.borrow_mut();
        if c.len() >= CACHE_MAX {
            c.remove(0);
        }
        c.push((fp, o.clone())); // alloc: Program
        Ok(o)
    }

    /// Whether this execution's re-estimates fall in the octaves the admission lowered under (none: always).
    pub fn hold(&self, params: ParamListHandle) -> bool {
        if self.sites.is_empty() {
            return true;
        }
        match self.now(params) {
            Ok(o) => o == self.octaves,
            Err(_) => false,
        }
    }
}
