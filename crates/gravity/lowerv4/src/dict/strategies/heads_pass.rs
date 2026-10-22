//! D3027 (B), code.heads' count pass: a pass right after the top (`Px::add_pass`) whose source reads the key's code lane
//! over the top's relation, every row (no qual: the top's quals read the key alone, so a code's rows pass or fail
//! together, and the emit asks only for codes that passed), and whose end takes each window whole: kernels heads
//! `heads_tally` counts it into its part's table in the code.heads protocol's directory (exec ops/sort/heads.rs: one
//! entry per part, the pass's claims whole parts cut to the parts the top kept), which the emit's `heads_count` reads
//! before its own count. Below width 2 the protocol prunes the pass and the emit counts on its thread as before (serial
//! census HK-55). The pass ends in a fixed plain breaker no pipeline reads (a section needs a breaker end).
use gravity_abi::ctx::lane;
use gravity_abi::breaker::{Breaker, BreakerKind};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, HostFlags, Type, Value};
use gravity_program::protos::{Proto, ProtoKind};
use gravity_program::{End, LaneId, Pipeline, SiteDesc};

use crate::face::end::EndMech;
use crate::face::source::{SourceMech, WinConds};
use crate::frame::env::Env;
use crate::frame::frame::Win;
use crate::frame::iface::breaker_of;
use crate::frame::px::{ExtraPass, Px, DRAFT_SITE};
use crate::frame::row::{Carry, RowVals};
use crate::frame::site::Reader;
use crate::frame::stop::Exits;
use crate::admit::refusal::Reason;

/// Adds the count pass after top producer `k` (its maker, `Heads::end`) over key column `attno` of `relid`, with the
/// code.heads protocol over it (`mark_off`: the mark word's offset in the top's run row, the protocol's `arg`); nothing
/// when `k` is made for another pipeline (a pass of its own) or its draft site has no code lane for the key.
pub(crate) fn add(px: &mut Px<'_, '_>, k: usize, relid: u32, attno: i16, node: i32, mark_off: u32) {
    if px.ident(k) != k {
        return;
    }
    let ld = px.site_mut(k).and_then(|d| d.lanes().iter().find(|l| l.id == LaneId::Attr(attno)).copied());
    let (Some(ld), Some(top)) = (ld.filter(|l| l.kind == lane::KIND_CODES), breaker_of(&px.gx.pipelines()[k].desc)) else { return };
    let v = px.pass_id();
    let (slot, word) = (px.new_slot(), px.proto_words(1));
    let mut s = SiteDesc::range_source(px.arena, relid, &[attno]);
    s.lanes[0] = ld;
    let rd = Reader::new(px, &s, relid, node, None, None);
    px.set_site(v, s);
    px.declare_proto(Proto::new(ProtoKind::Heads, v as u16).peer(k as u16).slot(top.slot).site(DRAFT_SITE).words(word, 1).arg(mark_off as u64));
    let mcx = px.mcx();
    let end = mcx::pg_box_dyn!(mcx, Tally { slot, word, node } => dyn EndMech + '_);
    if px.add_pass(k, ExtraPass { source: mcx::pg_box_dyn!(mcx, CountSrc { rd } => dyn SourceMech + '_), steps: mcx::PgVec::new_in(mcx), end }) != v {
        crate::refuse!(node, Reason::Bug { payload: "a heads count pass made under another pass's id" });
    }
    fires::fire("heads.count.pass"); // shapecheck fire signal (D3027 B)
}

/// The directory word of top producer `k`'s count pass (`add`), if it has one.
pub(crate) fn dir_word(px: &Px<'_, '_>, k: usize) -> Option<u16> {
    px.protos.iter().find(|p| p.kind == ProtoKind::Heads && p.peer as usize == k).map(|p| p.words.base)
}

/// The pass's source: the key's code lane, window by window, its rows never walked (`Tally` takes each window whole).
struct CountSrc<'g> {
    rd: Reader<'g>,
}

impl SourceMech for CountSrc<'_> {
    fn declare(&mut self, pb: &mut gravity_ir::builder::ProgramBuilder) {
        self.rd.declare(pb);
    }
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        self.rd.open(b, env)
    }
    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        let (cursor, end) = self.rd.window(b, env, l_eof);
        Win { cursor, end, facts: WinConds::default() }
    }
    fn row<'e>(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env<'e, '_>, _win: &Win, _j: Value, _carry: Carry<'e>, _next: Value) -> RowVals<'e> {
        crate::refuse!(self.rd.node, Reason::Bug { payload: "a heads count pass walked a row" })
    }
    fn fills(&self) -> bool {
        false
    }
}

/// The pass's end: each window counted by the host, no row out.
struct Tally {
    slot: u8,
    word: u16,
    node: i32,
}

impl EndMech for Tally {
    fn lower<'e>(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env<'e, '_>, _row: &RowVals, _x: &Exits) -> Carry<'e> {
        crate::refuse!(self.node, Reason::Bug { payload: "a heads count pass ended a row" })
    }

    fn whole(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _win: &Win, c: &Carry, l_wend: Block) -> bool {
        let Some(k) = env.site else { crate::refuse!(self.node, Reason::Bug { payload: "a heads count pass without its site" }) };
        let mem = HostFlags { touches_memory: true, ..HostFlags::default() };
        let h = b.declare_host_once("heads_tally", &[Type::Ptr, Type::I64, Type::Ptr], Some(Type::I64), gravity_kernels::dict::strategies::heads::heads_tally as *const () as usize, mem);
        let kv = b.iconst(Type::I64, k as i64);
        let dir = b.load(Type::Ptr, env.params, 8 * self.word as i32);
        let r = b.call_host(h, &[env.ctx, kv, dir], Some(Type::I64)).expect("heads_tally returns a status");
        let x = env.x;
        crate::table::bypass::fail_on(b, env, &x, r);
        b.br(l_wend, &c.params());
        // nothing enters the rows' block: the frame emits no row loop (D528)
        let l_rows = b.create_block(&[]);
        b.switch_to(l_rows);
        true
    }

    fn pipeline_desc(&self, _px: &Px<'_, '_>, _k: usize, d: Pipeline) -> Pipeline {
        Pipeline { parallel: d.parallel, ..Pipeline::eligible(d.driving_site, End::Breaker(Breaker::sized(BreakerKind::PlainAgg, 8).in_slot(self.slot)), d.predicted_rows) }
    }
}
