//! TABLESAMPLE over a heap (cards/M2.md L6 `src.sample`; KEEP lowerv3 R1c scan/sample.rs at 15fd30878f4). The site is the
//! heap range site, served by the host's heap range source with PostgreSQL's own sampler on it (hosts v4/m2_tid_sample.rs
//! `TsmFilter` over `HeapRangeSource::with_sample`): the TSM picks the blocks and the offsets, a picked offset kept when
//! visible, so REPEATABLE gives PostgreSQL's rows for the same seed. The method's argument and the REPEATABLE value are the
//! site's key words, evaluated once at the bind and read at the seek, where the host raises PostgreSQL's errors (a NULL
//! argument or seed, an out-of-range percent). D1016 row 3 (V4R-547): an argument computed from executor parameters (a
//! LATERAL column cast to float4, `t1.b` under a Memoize; D3281: tsm_system_rows' / tsm_system_time's over a LATERAL int
//! column cast to int8 / float8), no REPEATABLE, is the host's cell (params_key.rs
//! `computed_key`) with its one parameter as the input cell after it, evaluated at every (re-)open as tablesample_init does
//! at each rescan (exec / hosts v4/m8_param_sample.rs), over the execution's extern parameters too (exec BIND compiles it
//! per execution); a layout past the range site's two cells refuses by name. The window loop is the core's site reader
//! with the keys (`seek`, never a claim: the sampler walks the whole relation once, serial as the decomposer declares it). A row's `ctid` is the window's
//! block and the row's offset lane. No verdict, no condition cache (`fills() = false`). Registers kind `src.sample`.
//! Owner M2-SOURCES.
use gravity_abi::ctx::lane;
use gravity_abi::ctx::site_off;
use gravity_abi::site::site;
use gravity_decompose::{Am, Source};
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::{Block, ConvOp, IntBinOp, Type, Value};
use gravity_lower::Place;
use gravity_program::{ExprLife, ExprOut, LaneId as SiteLane, OutCol, Pipeline, SiteDesc, SiteKind};
use types_nodes::Node;
use mcx::{GrowIn, PgBox, PgVec};

use crate::cell::{Cell, Nullable, TIDOID};
use crate::face::kinds::KindId;
use crate::face::source::{SourceMech, WinConds};
use crate::frame::env::Env;
use crate::frame::frame::{SourceMaker, Win};
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::frame::site::{Reader, CTID_AT};
use crate::graph::Gx;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::lifetime::bound::{self, BoundAt, Feed};
use crate::ops::Table;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::SrcSample, &SourceMaker(make))], ..crate::ops::NONE };

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

struct Sample<'g> {
    rd: Reader<'g>,
    /// The site's offset lane when the scan reads `ctid`.
    offset: Option<usize>,
}

fn make<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let node = p.node_id;
    let Source::Sample { site, am, relid, sample, .. } = &p.source else { crate::refuse!(node, Reason::Bug { payload: "a sample entry over another source" }) };
    if !matches!(am, Am::Heap | Am::Pgrcolumnar) {
        shape(node, "a TABLESAMPLE over a relation that is not a heap or pgrc table");
    }
    // the host's key cells (hosts `TsmFilter::start`): the method's argument, then the REPEATABLE value
    if sample.args.len() != 1 {
        shape(node, "a TABLESAMPLE method taking other than one argument");
    }
    let (mut slots, mut ids) = (mcx::reserved_in(2, px.mcx()), PgVec::new_in(px.mcx()));
    for a in sample.args.iter().chain(sample.repeatable) {
        // D1643: a VOLATILE or raising value over no executor parameter (decompose `SampleDesc::per_open`) is the host's at
        // every (re-)open as a computed one is, never a word the bind evaluates: tablesample_init's at each (re)scan's first
        // fetch, never for a scan that fetches nothing
        if sample.opens(a) {
            bound::declare(px, Feed::Sample, k, a, node, BoundAt::Rescan);
            slots.push_in(crate::rerun::params_key::computed_key(node, a, &mut ids));
            continue;
        }
        match bound::word(px, Feed::Sample, k, a, node) {
            Place::Word(w) => slots.push_in(w),
            // D1016 row 3: computed from executor parameters, the host's at every (re-)open (params_key.rs `computed_key`)
            Place::Fold if crate::rerun::params_key::computed(a) => {
                bound::declare(px, Feed::Sample, k, a, node, BoundAt::Rescan);
                slots.push_in(crate::rerun::params_key::computed_key(node, a, &mut ids));
            }
            Place::Fold if gravity_lower::rerun::words::has_exec(a) => crate::rerun::params::key(px.gx, node, a),
            Place::Fold => shape(node, "a TABLESAMPLE argument without a parameter word (the statement's words are full)"),
            Place::Exec(id) => {
                let w = crate::rerun::params::exec_key(px, node, id);
                slots.push_in(w);
            }
        }
    }
    // D3281: the extension methods (tsm_system_rows' int8, tsm_system_time's float8 argument) take the same host-evaluated
    // route: the host's TsmFilter resolves the method from the handler at admission (an unknown one refuses there by name)
    // and begins its scan with the argument evaluated at each (re-)open, as nodeSamplescan.c's tablesample_init does
    // pgrc: the granule range scan with the sampler on its host (ops/scan/pgrc/lower.rs `make_sample`): the argument and
    // the REPEATABLE value as statement or executor-parameter words only (a computed one, one evaluated at each open
    // (D1643) and an InitPlan's refuse by name)
    if *am == Am::Pgrcolumnar {
        use crate::frame::site::{EXEC_KEY, HOST_KEY, INIT_KEY};
        if !ids.is_empty() || slots.iter().any(|&w| w == HOST_KEY || (w & EXEC_KEY != 0 && w & INIT_KEY != 0)) {
            shape(node, "a TABLESAMPLE over pgrc whose argument is computed from executor parameters, VOLATILE or may raise (D1643), or an InitPlan's");
        }
        return crate::ops::scan::pgrc::lower::make_sample(px, k, slots);
    }
    let mut attnos = PgVec::new_in(px.mcx());
    crate::rerun::params_key::input_cells_opening(px, node, &ids, &ids, sample.args.iter().chain(sample.repeatable), &mut slots, &mut attnos, |v| sample.opens(v));
    let ctid = site.lanes().iter().any(|l| l.id == SiteLane::RowId);
    let mut s = site_of(px, site);
    // V4R-88 (scan-0050): the system columns the scan reads, filled by the sampled range source's window as the range site's
    crate::ops::scan::heap::add_sys_lanes(px, &mut s, node);
    // the key cells: the argument's, the REPEATABLE's, then each executor parameter's input cell (hosts sample_param.rs)
    if let SiteKind::RangeSource { nkeys, .. } = &mut s.kind {
        *nkeys = (*nkeys).max(slots.len() as u16);
    }
    let offset = if ctid { s.lane_index(SiteLane::Offset) } else { None };
    if ctid && offset.is_none() {
        crate::refuse!(node, Reason::Bug { payload: "a heap range site without its offset lane" });
    }
    let rd = Reader::new(px, &s, *relid, node, Some(slots), None);
    px.set_site(k, s);
    mcx::pg_box_dyn!(px.mcx(), Sample { rd, offset } => dyn SourceMech + 'g)
}

/// The heap range site (lowerv3 range.rs `site_of`): the decomposer's lanes, by-value lanes as the host's datum words; the
/// rowid lane dropped (the host publishes the attributes and the offset lane only). A scan reading no column (count(*))
/// keeps the offset lane alone, as the heap and bitmap sites do: the window's row count is the sampler's kept rows (hosts
/// heap_source.rs `stage_sampled` / `fill`), so no column is borrowed to drive it and none is deformed (tour-scans
/// scan-0142 / 0143: a relation of text columns only).
fn site_of(px: &Px<'_, '_>, d: &SiteDesc) -> SiteDesc {
    let mut s = d.copy_in(px.arena);
    if s.lanes().last().is_some_and(|l| l.id == SiteLane::RowId) {
        s.lanes.pop();
    }
    for l in s.lanes.iter_mut() {
        if matches!(l.id, SiteLane::Attr(_)) && l.kind == lane::KIND_BYVAL {
            l.width = 8;
        }
    }
    s.filter_lanes.clear();
    s
}

impl SourceMech for Sample<'_> {
    fn declare(&mut self, pb: &mut ProgramBuilder) {
        self.rd.declare(pb);
    }

    /// Serial: the sampler walks the whole relation once from its seek (the decomposer's desc; an aggregate end's
    /// breaker leaves it eligible).
    fn pipeline_desc(&self, px: &Px<'_, '_>, k: usize) -> Pipeline {
        crate::ops::scan::tid::serial(px.gx.pipelines()[k].desc)
    }

    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        self.rd.open(b, env)
    }

    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        let (cursor, end) = self.rd.window(b, env, l_eof);
        let nonulls = self.rd.nonulls(b);
        Win { cursor, end, facts: WinConds { nonulls, ..WinConds::default() } }
    }

    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        let mut r = self.rd.row(b, env, j, carry, next);
        if let Some(o) = self.offset {
            // the ItemPointerData image (bi_hi, bi_lo, ip_posid) of the window's block and the row's offset
            let k = self.rd.k(env);
            let block = b.load(Type::I32, env.ctx, site_off(k) as i32 + site::POS_BLOCK as i32);
            let i = self.rd.row_index(b, env, j);
            let lp = b.load(Type::Ptr, self.rd.vals().lane_arr, (o * lane::SIZE + lane::PTR) as i32);
            let at = b.ptr_add(lp, i, 2, 0);
            let off = b.load(Type::I16, at, 0);
            let slot = b.declare_stack_slot(8, 8);
            let img = b.stack_addr(slot);
            let sixteen = b.iconst(Type::I32, 16);
            let hi = b.int_bin(IntBinOp::Ushr, block, sixteen);
            let lo = b.int_bin(IntBinOp::Shl, block, sixteen);
            let bi = b.int_bin(IntBinOp::Or, hi, lo);
            b.store(img, 0, bi);
            b.store(img, 4, off);
            let out = ExprOut { rep: OutCol::new("", TIDOID, -1, 6, false).rep, typid: TIDOID, life: ExprLife::Row };
            if r.cols.len() < CTID_AT as usize {
                r.cols.resize_in(CTID_AT as usize, None);
            }
            r.cols[CTID_AT as usize - 1] = Some(r.row.cells.len() as u16);
            r.row.cells.push_in(Cell::of_expr(out, r.row.cells.len() as u16, 0, Nullable::Never));
            r.vals.push_in((b.conv(ConvOp::PtrToInt, Type::I64, img), None));
        }
        r
    }

    fn fills(&self) -> bool {
        false
    }
}
