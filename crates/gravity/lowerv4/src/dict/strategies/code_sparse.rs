//! Road `code.sparse` (cards/M3.md L8, N1 / M3-plan Decision 2; the lab's `fold_nonbase_*`; H08 / P5-04 / SRC-22): a code
//! filter's survivors selected without a base pass, for ANY code verdict, keyed on no spelling: a leading qual answered
//! whole by one host-built per-code `Bits` table (an EQ code set, a comparison with any constant among them, a list test
//! over a non-empty constant array, a LIKE, a byte range: verdict_coderange `of_list`) names that table to the host
//! (`VERDICT_CODE_SPARSE_TABLE`), which reads a granule whose code block's base the table rejects as packed codes,
//! selecting only the non-base rows the table may pass (hosts v4 m3_codes.rs `nonbase_select`, granule_source
//! `sparse_stage`). Any other granule decodes whole and row_selects as before; the program still tests the qual on every
//! selected row, so the road only narrows the rows it reads. Nothing data-derived enters the program (D448): the host
//! decides per granule from the block's base. Owner M3-LEVERS-A.
use gravity_abi::ctx::lane;
use gravity_program::{table_fn, LaneId, SiteDesc};

use crate::face::source::{VerdictImpl, VerdictMech};
use crate::frame::px::{ExId, Px};
use crate::ops::Table;

crate::strategies! {
    /// Sparse non-base code select: a code filter's survivors without a base pass.
    CODE_SPARSE = 76, "code.sparse", CodeFilterTopk;
}

pub(crate) const ROWS: Table = Table { strategies: &[(CODE_SPARSE, &VerdictImpl(&CodeSparse))], ..crate::ops::NONE };

struct CodeSparse;

impl VerdictMech for CodeSparse {
    /// `qual` takes the table form (`table`); the site rule (ops/scan/pgrc/lower.rs) names the table and fires.
    fn declare(&self, px: &mut Px<'_, '_>, site: &mut SiteDesc, qual: ExId) -> bool {
        table(px, site, qual).is_some()
    }
}

/// H08 / P5-04 / SRC-22: the table form of leading qual `qual` on the drafted `site`: `(t, k)` when the whole qual is
/// one per-code `Bits` table `t` over code lane `k`, strict over the column (verdict_coderange.rs `of`: a NULL row, which
/// holds a block's base too, fails it), and the table is one the host builds from the part's dictionary (an EQ code set,
/// a LIKE, a byte range: `cond_table`), never a program-filled body, which the host cannot read before the program
/// runs. Any code verdict; the site rule (ops/scan/pgrc/lower.rs) keeps it at any zone count. The host reads the table's
/// answer for each granule's block base (hosts granule_source/fetch.rs `sparse_stage`).
pub(crate) fn table(px: &Px<'_, '_>, site: &SiteDesc, qual: ExId) -> Option<(u16, usize)> {
    let r = crate::dict::strategies::verdict_coderange::of_list(px, qual)?;
    let decl = site.tables.get(r.t as usize)?;
    let ls = site.lanes();
    let k = (0..site.natts()).find(|&k| ls[k].id == LaneId::Attr(r.attno))?;
    let hosted = matches!(decl.func, table_fn::EQ | table_fn::LIKE | table_fn::RANGE);
    (hosted && decl.over as usize == k && ls[k].kind == lane::KIND_CODES).then_some((r.t, k))
}
