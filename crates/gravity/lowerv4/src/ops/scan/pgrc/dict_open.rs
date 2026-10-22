//! The dictionary open by expected renders (H11, `dict.open`, R4-L11; KEEP lowerv2 text/codes.rs `dict_facts` /
//! `renders_sparse` and compose.rs source_of's dense rules, D039-a / D045): a pgrc site's code lanes open SPARSE (a
//! render, a projected code, a part's empty code are point probes at block grain; the host keeps a lane a per-code table
//! is built over dense, since the build walks every entry), except a top-k over the scan's rows whose first key is a text:
//! every row's key renders, so a cold scan would fault thousands of single blocks, several times the dense read.
//! P5-18 / SRC-23: a sparse site carries the renders a code lane can get at most (`SiteDesc::dict_renders`): the first bound
//! downstream (a LIMIT or a top-k, offset included), else none (`u64::MAX`). The host weighs a bound per part, the part's
//! share of it on the part's own dictionary, against streaming that dictionary whole (`pgrc_read::wrapped_dense_pays`,
//! the cost home's device profile) and opens it whole where the stream pays; every other part, and every part of a site
//! with no bound, opens for probes at block grain with the bytes-touched election live, which streams the rest once the
//! faults have cost one stream. A planner's row count is no bound: rows repeat values, so it over-counts the blocks.
//! The code lanes themselves are the decomposer's (sources.rs lane_of: every pgrc text / varchar / bpchar column).
//! Registers road `dict.open`. Owner M1-SOURCE.
use gravity_abi::ctx::lane;
use gravity_decompose::ops::shared::sort::SortForm;
use gravity_decompose::{Bound, Op, Source};
use gravity_program::SiteDesc;

use crate::dict::face::DictOpenForm;
use crate::frame::px::Px;
use crate::ops::Table;

crate::strategies! {
    /// Dense / sparse / heads dictionary open by expected renders.
    DICT_OPEN = 24, "dict.open", Code;
}

pub(crate) const ROWS: Table = Table { strategies: &[(DICT_OPEN, &())], ..crate::ops::NONE };

/// Pipeline `k`'s site opened by the renders it can expect: `sparse_dicts` with their bound, unless every row renders.
pub(crate) fn open(px: &Px<'_, '_>, k: usize, site: &mut SiteDesc) {
    let (f, renders) = form(px, k, site);
    site.sparse_dicts = f == DictOpenForm::Sparse;
    site.dict_renders = if site.sparse_dicts { renders.max(1) } else { 0 };
    fires::fire(if site.sparse_dicts { "dict.open.sparse" } else { "dict.open.dense" }); // shapecheck fire signal (REGISTRY SRC-23)
}

/// The form and, when sparse, the renders a code lane of the site can get at most (`u64::MAX`: no bound).
pub(crate) fn form(px: &Px<'_, '_>, k: usize, site: &SiteDesc) -> (DictOpenForm, u64) {
    let codes = site.lanes().iter().filter(|l| l.kind == lane::KIND_CODES).count() as u64;
    if codes == 0 {
        return (DictOpenForm::Dense, 0);
    }
    let pipes = px.gx.pipelines();
    if !matches!(pipes[k].source, Source::Range { .. }) {
        return (DictOpenForm::Dense, 0);
    }
    // a top-k over the scan's own rows keyed first by a text renders every row's key (v2 topk.rs `dense_dicts`)
    let text_first = |s: &gravity_decompose::ops::shared::sort::SortDesc<'_>| s.keys.first().is_some_and(|key| key.image.is_string());
    if pipes[k].ops.iter().any(|op| matches!(op, Op::Sort(s) if matches!(s.form, SortForm::TopK { .. }) && text_first(s))) {
        return (DictOpenForm::Dense, 0);
    }
    let mut at = k;
    loop {
        if let Some(n) = pipes[at].ops.iter().find_map(bound) {
            return (DictOpenForm::Sparse, n.min(u64::MAX - 1));
        }
        let next = crate::frame::iface::breaker_of(&pipes[at].desc).and_then(|br| (at + 1..pipes.len()).find(|&j| pipes[j].desc.consumes.contains(&br.slot)));
        match next {
            Some(j) => at = j,
            None => return (DictOpenForm::Sparse, u64::MAX),
        }
    }
}

/// The rows an op lets through at most, when its count and offset are constants.
fn bound(op: &Op<'_>) -> Option<u64> {
    let (count, offset) = match (crate::frame::iface::limit_bounds(op), op) {
        (Some(b), _) => b,
        (None, Op::Sort(s)) => match &s.form {
            SortForm::TopK { count, offset, .. } => (count, offset),
            _ => return None,
        },
        _ => return None,
    };
    let c = |b: &Bound<'_>| match b {
        Bound::Const(n) => Some((*n).max(0) as u64),
        Bound::All => Some(0),
        Bound::Expr(_) => None,
    };
    match (count, c(count)?, c(offset)?) {
        (Bound::All, ..) => None,
        (_, n, o) => Some(n.saturating_add(o)),
    }
}
