//! The lab hand programs (par_*) the kernel / parallel / pool tests drive.
use crate::{Arena, Code, End, Layout, OutCol, OwnerDesc, Pipeline, Program, Reason, SiteDesc, WordsDesc, WritePlan};
use gravity_abi::breaker::{Breaker, BreakerKind};

use gravity_kernels::{par_distinct, par_hashagg_codes, par_hashjoin, par_scan_count, par_sort, par_topk};
use gravity_kernels::table::params as par_hashagg;

pub const OUT_ROWS: u32 = 64;
/// The staging arena a hand program's descriptors are built in (`Program::with_pipelines` copies them out).
const STAGE_BYTES: usize = 64 << 10;

fn stage() -> Arena {
    Arena::new(STAGE_BYTES) // alloc: Cold
}

pub const INT2: u32 = 21;
pub const INT4: u32 = 23;
pub const INT8: u32 = 20;
pub const NUMERIC: u32 = 1700;
pub const VARCHAR: u32 = 1043;
pub const BPCHAR: u32 = 1042;
pub const TIMESTAMPTZ: u32 = 1184;

pub const fn numeric_typmod(precision: i32, scale: i32) -> i32 {
    ((precision << 16) | scale) + 4
}

pub const fn char_typmod(len: i32) -> i32 {
    len + 4
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Oids {
    pub customer: u32,
    pub customer_i1: u32,
    pub customer_i2: u32,
    pub district: u32,
    pub district_i1: u32,
    pub warehouse: u32,
    pub warehouse_i1: u32,
    pub stock: u32,
    pub stock_i1: u32,
    pub orders: u32,
    pub orders_i1: u32,
    pub order_line: u32,
    pub order_line_i1: u32,
    pub ol_ins: u32,
    pub ol_ins_pkey: u32,
    pub ol_del: u32,
    pub ol_del_pkey: u32,
}

impl Oids {
    pub const FIXTURE: Oids = Oids {
        customer: 1001,
        customer_i1: 1101,
        customer_i2: 1102,
        district: 1002,
        district_i1: 1112,
        warehouse: 1003,
        warehouse_i1: 1113,
        stock: 1005,
        stock_i1: 1105,
        orders: 1008,
        orders_i1: 1108,
        order_line: 1009,
        order_line_i1: 1109,
        ol_ins: 1006,
        ol_ins_pkey: 1106,
        ol_del: 1007,
        ol_del_pkey: 1107,
    };

    pub const T_A: u32 = 2001;
    pub const T_KV: u32 = 2002;
    pub const T_INNER: u32 = 2003;
    pub const T_OUTER: u32 = 2004;
}

fn layout(nsites: usize, out_natts: usize, out_rows: u32, scratch_bytes: u32) -> Layout {
    layout_(nsites, out_natts, out_rows, scratch_bytes)
}

fn layout_(nsites: usize, out_natts: usize, out_rows: u32, scratch_bytes: u32) -> Layout {
    Layout { nsites: nsites as u16, out_natts: out_natts as u16, out_rows, scratch_bytes, exec_bytes: 0 }
}

fn int4(name: &str, tbl: u32, col: i16) -> OutCol {
    OutCol::new(name, INT4, -1, 4, true).origin(tbl, col)
}

pub fn par_scan_count_with(rel: u32, rows: u64) -> Program {
    let st = stage();
    let a = st.handle();
    let sites = [SiteDesc::range_source(a, rel, &[1])];
    let out = [OutCol::new("count", INT8, -1, 8, true)];
    let pipes = [
        (Pipeline::eligible(0, End::Breaker(Breaker::sized(BreakerKind::PlainAgg, par_scan_count::BYTES)), rows), Code::hand(par_scan_count::par_scan_count)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(par_scan_count::par_scan_count_emit)),
    ];
    Program::with_pipelines(layout(1, 1, 1, 0), &sites, &out, WritePlan::SELECT, pipes)
}

pub fn par_scan_count(rel: u32) -> Program {
    par_scan_count_with(rel, 0)
}

/// `par_hashagg`'s kernel words and owner phase declared (milestone-2.md §2.6 item 18; ir-probe.md §20.3): the
/// runner's facts at `PARAM_*`, the phase over `LAYOUT` with the bits at `PARAM_SPILL_BITS`.
pub const PAR_HASHAGG_WORDS: WordsDesc = WordsDesc { topk: par_hashagg::PARAM_TOPK_N as u16, dense_lo: par_hashagg::PARAM_DENSE_LO as u16, dense_hi: par_hashagg::PARAM_DENSE_HI as u16, bits: par_hashagg::PARAM_SPILL_BITS as u16, order: par_hashagg::PARAM_ORDER as u16, nparams: par_hashagg::NPARAMS as u16 };
pub const PAR_HASHAGG_OWNER: OwnerDesc = OwnerDesc::named(par_hashagg::LAYOUT, par_hashagg::owner_fns, Some((par_hashagg::PARAM_SPILL_BITS as u8, 0)));
/// The join's build takes the bits word alone.
pub const PAR_HASHJOIN_WORDS: WordsDesc = WordsDesc::bits_only(par_hashjoin::PARAM_SPILL_BITS as u16, par_hashjoin::NPARAMS as u16);

/// PGROUP's scan pipeline (`groups`: the plan's estimate behind the partition bits), declaring both descriptors.
pub const fn par_hashagg_pipeline(groups: u64, rows: u64) -> Pipeline {
    Pipeline::eligible(0, End::Breaker(par_hashagg::breaker(groups)), rows).owner(PAR_HASHAGG_OWNER).words(PAR_HASHAGG_WORDS)
}

pub fn par_hashagg_with(rel: u32, out_rows: u32, rows: u64) -> Program {
    let st = stage();
    let a = st.handle();
    let sites = [SiteDesc::range_source(a, rel, &[1, 2])];
    let out = [int4("k", rel, 1), OutCol::new("sum", INT8, -1, 8, true), OutCol::new("count", INT8, -1, 8, true)];
    let pipes = [
        (par_hashagg_pipeline(0, rows), Code::hand(par_hashagg::par_hashagg)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(par_hashagg::par_hashagg_emit)),
    ];
    Program::with_pipelines(layout(1, 3, out_rows, 0), &sites, &out, WritePlan::SELECT, pipes)
}

pub fn par_hashagg(rel: u32) -> Program {
    par_hashagg_with(rel, OUT_ROWS, 0)
}

/// `t, count(*)` grouped by a dictionary text lane (step 4): the codes kernel and its emit.
pub fn par_hashagg_codes_with(rel: u32, out_rows: u32, rows: u64) -> Program {
    let st = stage();
    let a = st.handle();
    let sites = [SiteDesc::range_source(a, rel, &[1]).codes_lane(0)];
    let out = [OutCol::new("t", 25, -1, -1, false).origin(rel, 1), OutCol::new("count", INT8, -1, 8, true)];
    let pipes = [
        (Pipeline::eligible(0, End::Breaker(Breaker::sized(BreakerKind::HashAgg, par_hashagg_codes::BYTES)), rows), Code::hand(par_hashagg_codes::par_hashagg_codes)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(par_hashagg_codes::par_hashagg_codes_emit)),
    ];
    Program::with_pipelines(layout(1, 2, out_rows, 0), &sites, &out, WritePlan::SELECT, pipes)
}

pub fn par_hashagg_codes(rel: u32) -> Program {
    par_hashagg_codes_with(rel, OUT_ROWS, 0)
}

/// `SELECT a, b FROM t ORDER BY a LIMIT n` over the zone walk (step 5): two int4 lanes, the
/// walk key first; params `[n, natts]`.
pub fn par_topk_with(rel: u32, n: u32, out_rows: u32, rows: u64) -> Program {
    let st = stage();
    let a = st.handle();
    let sites = [SiteDesc::range_source(a, rel, &[1, 2])];
    let out = [int4("a", rel, 1), int4("b", rel, 2)];
    let pipes = [
        (Pipeline::eligible(0, End::Breaker(Breaker::sized(BreakerKind::TopK, par_topk::bytes(n, 2))), rows), Code::hand(par_topk::par_topk)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(par_topk::par_topk_emit)),
    ];
    Program::with_pipelines(layout(1, 2, out_rows, 0), &sites, &out, WritePlan::SELECT, pipes)
}

pub fn par_topk(rel: u32, n: u32) -> Program {
    par_topk_with(rel, n, OUT_ROWS, 0)
}

pub fn par_hashjoin_with(inner: u32, outer: u32, out_rows: u32, inner_rows: u64, outer_rows: u64) -> Program {
    let st = stage();
    let a = st.handle();
    let sites = [SiteDesc::range_source(a, inner, &[1, 2]), SiteDesc::range_source(a, outer, &[1, 2])];
    let out = [int4("k", outer, 1), int4("a", inner, 2), int4("b", outer, 2)];
    Program::with_pipelines(layout(2, 3, out_rows, 0), &sites, &out, WritePlan::SELECT, hashjoin_pipelines(inner_rows, outer_rows, inner_rows))
}

pub fn par_hashjoin(inner: u32, outer: u32) -> Program {
    par_hashjoin_with(inner, outer, OUT_ROWS, 0, 0)
}

/// The join with its spill partition bits given (the kernel words must say the same: `par_hashjoin::params`).
pub fn par_hashjoin_bits(inner: u32, outer: u32, out_rows: u32, inner_rows: u64, outer_rows: u64, bits: u8) -> Program {
    let st = stage();
    let a = st.handle();
    let sites = [SiteDesc::range_source(a, inner, &[1, 2]), SiteDesc::range_source(a, outer, &[1, 2])];
    let out = [int4("k", outer, 1), int4("a", inner, 2), int4("b", outer, 2)];
    Program::with_pipelines(layout(2, 3, out_rows, 0), &sites, &out, WritePlan::SELECT, hashjoin_pipelines_bits(inner_rows, outer_rows, bits))
}

/// `SELECT cols FROM t ORDER BY cols[0]` over `natts` int lanes (the sort breaker; params `par_sort::params`).
pub fn par_sort_with(rel: u32, out_rows: u32, rows: u64, attnos: &[i16]) -> Program {
    let st = stage();
    let a = st.handle();
    let sites = [SiteDesc::range_source(a, rel, attnos)];
    let out: Vec<OutCol> = attnos.iter().enumerate().map(|(i, &a)| int4(["a", "b", "c", "d", "e", "f", "g", "h"][i.min(7)], rel, a)).collect(); // alloc: Program
    let pipes = [
        (Pipeline::eligible(0, End::Breaker(par_sort::BREAKER), rows).finishing(), Code::hand(par_sort::par_sort)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(par_sort::par_sort_emit)),
    ];
    Program::with_pipelines(layout(1, attnos.len(), out_rows, 0), &sites, &out, WritePlan::SELECT, pipes)
}

/// `SELECT k, count(DISTINCT x) FROM t GROUP BY k` over two int lanes (the distinct breaker; params `par_distinct::params`).
pub fn par_distinct_with(rel: u32, out_rows: u32, rows: u64, pairs: u64) -> Program {
    let st = stage();
    let a = st.handle();
    let sites = [SiteDesc::range_source(a, rel, &[1, 2])];
    let out = [int4("k", rel, 1), OutCol::new("count", INT8, -1, 8, true)];
    let pipes = [
        (Pipeline::eligible(0, End::Breaker(par_distinct::breaker(pairs)), rows), Code::hand(par_distinct::par_distinct)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(par_distinct::par_distinct_emit)),
    ];
    Program::with_pipelines(layout(1, 2, out_rows, 0), &sites, &out, WritePlan::SELECT, pipes)
}

/// The join's pipelines: build over the inner, probe over the outer (consuming the build; it
/// produces the outer rows only when the build spilled), then the spilled partitions one claim each (reading both
/// spilled streams, so the partition pipe consumes both slots).
pub fn hashjoin_pipelines(inner_rows: u64, outer_rows: u64, build_rows: u64) -> [(Pipeline, Code); 3] {
    hashjoin_pipelines_bits(inner_rows, outer_rows, par_hashjoin::spill_bits(build_rows))
}

/// `hashjoin_pipelines` with the partition bits given (the tests' small shares).
pub fn hashjoin_pipelines_bits(inner_rows: u64, outer_rows: u64, bits: u8) -> [(Pipeline, Code); 3] {
    use par_hashjoin::{INNER, OUTER, SLOT_BUILD, SLOT_OUTER};
    let [b, o, p] = par_hashjoin::breakers_bits(bits);
    [
        (Pipeline::eligible(INNER as i16, End::Breaker(b), inner_rows).words(PAR_HASHJOIN_WORDS), Code::hand(par_hashjoin::par_hashjoin_build)),
        (Pipeline::eligible(OUTER as i16, End::Breaker(o), outer_rows).consuming(&[SLOT_BUILD]), Code::hand(par_hashjoin::par_hashjoin_probe)),
        (Pipeline::eligible(OUTER as i16, End::Breaker(p), 0).consuming(&[SLOT_BUILD, SLOT_OUTER]).over_spill_partitions(SLOT_BUILD), Code::hand(par_hashjoin::par_hashjoin_part)),
    ]
}
