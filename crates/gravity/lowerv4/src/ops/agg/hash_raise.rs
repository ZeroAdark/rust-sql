//! D1292 (agg A14 d): a hashed group over a key PG's hash raises on (decompose effects::agg `hash_raises`: a text / varchar /
//! bpchar key under no collation, a UNION ALL column over conflicting implicit collations). nodeAgg hashes each input
//! row's keys before it advances the aggregates; `hashtext` / `hashbpchar` raise 42P22 ("could not determine which
//! collation to use for string hashing") on the first row whose key is not NULL (a NULL key is not hashed). The end
//! raises there through `m6_raise`; over no such row it is the group's own end. Every row is walked (no whole window,
//! block or count form), so the raise is the first act of the row that makes it.
use gravity_decompose::effects::agg::hash_raises;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, IntCond, Type, Value};
use gravity_kernels::host_abi::v4::m6_raise as h6;
use gravity_program::Pipeline;
use mcx::{CollectIn, PgBox, PgVec};

use crate::admit::refusal::{Reason, ShapeWhy};
use crate::face::end::{EndMech, SinkKind};
use crate::frame::env::Env;
use crate::frame::frame::{Mask, Win, WordWin};
use crate::frame::open::OpenWords;
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;

/// Pipeline `k`'s hashed group end `end`, raising first when a key of its group is one `hash_raises` names.
pub(crate) fn wrap<'g>(px: &Px<'g, '_>, k: usize, end: PgBox<'g, dyn EndMech + 'g>) -> PgBox<'g, dyn EndMech + 'g> {
    let gx = px.gx;
    let Some(g) = gx.group_at(k) else { return end };
    let cols: PgVec<'_, i16> = g.keys.iter().filter(|key| hash_raises(gx.facts, key.tf, key.coll)).map(|key| key.resno).collect_in(px.mcx());
    if cols.is_empty() {
        return end;
    }
    if cols.iter().any(|&c| c <= 0) {
        crate::refuse!(g.node_id, Reason::Shape { why: ShapeWhy("D1292: a hashed group's raising key that is not a column of its input") })
    }
    mcx::pg_box_dyn!(px.mcx(), Raise { end, cols, node: g.node_id } => dyn EndMech + 'g)
}

struct Raise<'g> {
    end: PgBox<'g, dyn EndMech + 'g>,
    /// The raising keys' columns in the producer's row.
    cols: PgVec<'g, i16>,
    node: i32,
}

impl EndMech for Raise<'_> {
    fn state_bytes(&self) -> u32 { self.end.state_bytes() }
    fn open_words(&self, w: &mut OpenWords) { self.end.open_words(w) }
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) { self.end.open(b, env) }
    fn carried<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>) -> PgVec<'e, Value> { self.end.carried(b, env) }
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, x: &Exits) -> Carry<'e> {
        let (l_raise, l_rows) = (b.create_block(&[]), b.create_block(&[]));
        let nulls: PgVec<'_, Option<Value>> = self.cols.iter().map(|&c| row.col(c).unwrap_or_else(|| crate::refuse!(self.node, Reason::Bug { payload: "a hashed group's raising key the row does not carry" })).null).collect_in(env.mcx);
        if nulls.iter().all(Option::is_some) {
            // every raising key may be NULL: the row raises unless all of them are
            for null in nulls.iter().flatten().copied() {
                let l_next = b.create_block(&[]);
                b.cond_br(null, (l_next, &[]), (l_raise, &[]));
                b.switch_to(l_next);
            }
            b.br(l_rows, &[]);
        } else {
            // a raising key never NULL: every row raises
            b.br(l_raise, &[]);
        }
        b.switch_to(l_raise);
        raise(b, env, x.l_exit, l_rows);
        b.switch_to(l_rows);
        self.end.lower(b, env, row, x)
    }
    fn window_end(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, c: &Carry) { self.end.window_end(b, env, c) }
    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) { self.end.finish(b, env, x) }
    fn sink(&self) -> SinkKind { self.end.sink() }
    fn wants_locator(&self) -> bool { self.end.wants_locator() }
    fn whole(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _win: &Win, _c: &Carry, _l_wend: Block) -> bool { false }
    fn block<'e>(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env<'e, '_>, _w: &WordWin, _base: Value, _m: Mask, _c: &Carry) -> Option<Carry<'e>> { None }
    fn counts(&self, _env: &Env) -> bool { false }
    fn takes_mult(&self) -> bool { self.end.takes_mult() }
    fn takes_set(&self) -> bool { self.end.takes_set() }
    fn pipeline_desc(&self, px: &Px<'_, '_>, k: usize, d: Pipeline) -> Pipeline { self.end.pipeline_desc(px, k, d) }
}

/// PG's 42P22 at the hash: `m6_raise` stashes it and the row leaves through `l_exit` with its status (always below zero;
/// `l_rows`, the row's fold, is never reached from here).
fn raise(b: &mut FunctionBuilder<'_>, env: &Env, l_exit: Block, l_rows: Block) {
    let h = b.declare_host_once("m6_raise", &[Type::Ptr, Type::I64], Some(Type::I64), gravity_kernels::contract::entry1::<h6::M6Raise> as *const () as usize, gravity_ir::HostFlags { may_error: true, ..Default::default() });
    let code = b.iconst(Type::I64, h6::RAISE_INDETERMINATE_HASH as i64);
    let st = b.call_host(h, &[env.ctx, code], Some(Type::I64)).expect("m6_raise returns its status");
    let bad = b.icmp(IntCond::Slt, st, env.zero);
    b.cond_br(bad, (l_exit, &[st]), (l_rows, &[]));
}
