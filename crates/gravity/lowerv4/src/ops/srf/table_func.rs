//! Kind `srf.tablefunc`: XMLTABLE and JSON_TABLE in FROM (`Source::TableFunc`; cards/M5.md `srf/table_func.rs`, M5-plan
//! §3.4). PG's TableFuncScan fills a tuplestore at its first fetch through the table function's routine (XmlTableRoutine /
//! JsonbTableRoutine, `nodeTableFuncscan.c` tfuncFetchRows), so the rows, their column PATH / DEFAULT / ON ERROR values
//! and the ordinality column are PG's by construction: the scan is store_read.rs's `StoreScan` over the incumbent node
//! (hosts v4/m5.rs `ScanStore`, H-d), whose first batch runs that fetch on the leader (PG marks it parallel-unsafe) and
//! whose every window is one host call; the IR only deforms. A re-run of the pipeline rewinds the store
//! (`ExecReScanTableFuncScan` with no changed parameter: nothing re-evaluated); a changed parameter's refill is the nest's
//! (M8, chgParam). LIMIT 0, a false gating Result and a pruned arm never call the pipeline, so never fill. The content key
//! hashes the whole TableFunc (D635), so two table functions never share a program. Registers kind `srf.tablefunc`.
//! Owner M5-STORE.
use gravity_decompose::Source;
use mcx::PgBox;

use crate::face::kinds::KindId;
use crate::face::source::SourceMech;
use crate::frame::frame::SourceMaker;
use crate::frame::px::Px;
use crate::admit::refusal::Reason;
use crate::ops::Table;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::SrfTablefunc, &SourceMaker(make))], ..crate::ops::NONE };

fn make<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let Source::TableFunc { scanrelid, func, .. } = &p.source else { crate::refuse!(p.node_id, Reason::Bug { payload: "a table function entry over another source" }) };
    let types = mcx::to_vec_in(func.coltypes.as_slice(), px.mcx());
    crate::rerun::store_read::scan(px, k, *scanrelid as i32, types)
}
