//! Kind `keyed.distinct_multi` (aggregates over more than one DISTINCT argument, or a DISTINCT the pair set does not
//! fold; the keyed maker keyed/pairs.rs, keyed/distinct_plain.rs and fold/plain.rs hand to, L0-e; L4) (cards/M6.md,
//! M6-changes.md §1; D1210).
//!
//! PSP2-34: a plain group whose every call is `count(DISTINCT x)` over two or more arguments, each one integer word or
//! (PS1-32) a byte-equal text's two Fp128 words (decompose ops/agg/mod.rs `multi_counts`: every argument a resno-0 key,
//! each call's key in its `count_slot`), is served here in one pass: ONE key-only table of `[tag, image]` records, the
//! tag the argument's key index, the image as wide as the widest argument's (a narrower one's padded with 0 words: a
//! record's tag decides its argument, so pads never meet another argument's words), each row upserting one record per
//! non-NULL argument; the grouped count(DISTINCT) owner (`OwnerDesc::distinct`, the tag its one group
//! word) dedups the pairs and leaves one accumulator per argument that some row reached, and the emit reads each
//! call's count from its tag's accumulator (0 for none: an empty input or only NULLs). Every count is order-free, so
//! the table's walk order is free; a NULL argument is never counted (count's strict input).
//!
//! D1210 serves the other multi-argument aggregates host-sorted (src/regroup.rs): a pair-form group the pair set cannot
//! hold is re-declared before lowering (runs.group's sorted form, or fold.plain's), and a plain group folds a host-sorted
//! DISTINCT in its own end. A group still handed here that is not PSP2-34's is one the host-sorted form cannot take, and
//! refuses by name (`regroup::blocked`): a DISTINCT of a user-defined or ordered-set aggregate (PG's hosted sort; D1283
//! serves more than one input), a rider reading the row order beside a DISTINCT (PG's sorted input), a group whose every
//! key the planner dropped (D345) or a planned two-pass group. Budget 450 lines.
use gravity_abi::breaker::{KeyKind, RecordLayout, ACC_PAIRS, ORDER_NONE};
use gravity_abi::ctx::{hdr, RESUME_NONE};
use gravity_abi::hash::Depth;
use gravity_abi::status::{unsupported, UNSUPPORTED_SCRATCH};
use gravity_decompose::ops::shared::agg::{FinalForm, GroupDecl};
use gravity_expr::emit::hash::{emit_key_hash, ImageValues};
use gravity_expr::emit::{AggCell, CellValid};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_kernels::host_abi::{agg_gather, agg_gather_at, GATHER_FRESH};
use gravity_kernels::util::RESUME_OPEN;
use gravity_program::{OwnerDesc, Pipeline, MAX_REPS, REP_NONE};
use mcx::{Mcx, PgBox, PgVec};
use mcx::GrowIn;

use crate::admit::refusal::{Reason, ShapeWhy};
use crate::face::end::EndMech;
use crate::face::kinds::{Face, KindId};
use crate::face::source::{SourceMech, WinConds};
use crate::frame::cancel;
use crate::frame::env::{AggRow, Env};
use crate::frame::frame::{EndMaker, Win};
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::{Px, WordKey};
use crate::cell::Row;
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::ops::agg::hashagg::GroupRec;
use crate::ops::Table;
use crate::table::emit_group::{ES_KEPT, ES_PTRS};
use crate::table::face::{Domain, KeyShape, TableDecl, TableMech};
use crate::table::keyimg::ImageWords;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::KeyedDistinctMulti, &EndMaker(make))], ..crate::ops::NONE };

fn bug(node: i32, payload: &'static str) -> ! {
    crate::refuse!(node, Reason::Bug { payload })
}

/// PSP2-34: whether plain group `g` counts the distinct values of more than one argument, each its own resno-0 key
/// (decompose `multi_counts`'s form: every call a `PairCount` naming its key in `count_slot`).
pub(crate) fn multi(g: &GroupDecl<'_>) -> bool {
    g.keys.len() > 1 && g.keys.iter().all(|k| k.resno == 0) && g.aggs.iter().all(|d| d.final_ == FinalForm::PairCount && (d.count_slot as usize) < g.keys.len())
}

fn make<'g, 'p>(px: &mut Px<'g, 'p>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    let node = px.gx.pipelines()[k].node_id;
    let Some(g) = px.gx.group_at(k) else { bug(node, "a keyed.distinct_multi end without its group") };
    if multi(g) {
        return make_counts(px, k, g, node);
    }
    let why = crate::ops::group::regroup::blocked(px.arena, px.gx.graph, px.gx.facts, g).unwrap_or("a pair-form DISTINCT group the host-sorted form was not given (a Bug of regroup.rs)");
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

/// The `[tag, image]` pair record: `1 + w` words (`w`: the widest argument's image words), no slots (the implicit
/// sentinel).
fn pair_layout(w: usize) -> RecordLayout {
    RecordLayout::with_sentinel(&[KeyKind::I64; 3][..1 + w], &[], false, None)
}

/// The owner's accumulator over the pair: the tag, `[sentinel, pairs]`.
fn acc_layout(px: &Px<'_, '_>, w: usize) -> RecordLayout {
    OwnerDesc::accumulator_folded(px.arena, &pair_layout(w), 1, &[], false).0
}

/// Group `g`'s widest argument image: 2 when an argument keys as Fp128 (decompose `multi_counts`'s text), else 1.
fn image_width(g: &GroupDecl<'_>) -> usize {
    1 + g.keys.iter().any(|k| k.kind == KeyKind::Fp128) as usize
}

fn make_counts<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, g: &'g GroupDecl<'p>, node: i32) -> PgBox<'g, dyn EndMech + 'g> {
    let mut rec = GroupRec::new(px, g);
    if rec.wpk.iter().zip(&g.keys).any(|(&w, k)| w != 1 + (k.kind == KeyKind::Fp128) as u8) {
        bug(node, "a multi-argument count(DISTINCT) key that is neither one image word nor a text's Fp128 (decompose multi_counts)");
    }
    rec.compile_args(px, true);
    let w = image_width(g);
    let layout = pair_layout(w);
    let mut t = TableDecl { image_words: layout.nkeys() as u16, record_bytes: layout.size() - 8 * layout.nkeys() as u32, expect: None, depth: Depth::TOP, life: rec.home, layout, state_off: 0, key: KeyShape { code: None, dense: None, domain: Domain::Unknown }, facts: 0, copies: 0 };
    t.facts = crate::table::size::declare_facts(px, k, &t, 0);
    let table = crate::table::face::table(&node, &t);
    mcx::pg_box_dyn!(px.mcx(), CountsEnd { rec, t, table, groups: g.keys.len() as u32, w } => dyn EndMech + 'g)
}

/// The one-pass end: each row's non-NULL arguments upserted as `[tag, image]` into the key-only table.
struct CountsEnd<'g, 'p> {
    rec: GroupRec<'g, 'p>,
    t: TableDecl,
    table: &'static dyn TableMech,
    groups: u32,
    /// The pair's image words (`image_width`).
    w: usize,
}

impl EndMech for CountsEnd<'_, '_> {
    /// join.mult (H20): `m` copies of a row upsert its keys once each, as the row itself does.
    fn takes_mult(&self) -> bool {
        true
    }

    fn state_bytes(&self) -> u32 {
        self.table.state_bytes(&self.t)
    }

    fn open_words(&self, w: &mut OpenWords) {
        self.table.open_words(&self.t, w);
    }

    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        self.table.open(b, env, &self.t);
    }

    /// The row's argument images in key order, then per key: a NULL skips it, any other value upserts `[its key index,
    /// its image words, 0 words to the pair's width]`.
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
        let one = b.iconst(Type::I64, 1);
        let out = b.int_bin(IntBinOp::Add, row.carry.out, one);
        let keys = self.rec.key_vals(b, env, row);
        let img = self.rec.image_of(b, env, &keys);
        for (j, kv) in keys.iter().enumerate() {
            let l_next = b.create_block(&[]);
            if let Some(n) = kv.2 {
                let l_do = b.create_block(&[]);
                b.cond_br(n, (l_next, &[]), (l_do, &[]));
                b.switch_to(l_do);
            }
            let (at, n) = self.rec.key_words(j);
            env.aside(|env| {
                let tag = b.iconst(Type::I64, j as i64);
                let mut words = vec![tag]; // alloc: Program
                words.extend((0..self.w).map(|i| if i < n { img.words.words[at + i] } else { env.zero }));
                let pair = ImageWords::plain(ImageValues { words }, true);
                let h = emit_key_hash(b, &pair.words);
                self.table.upsert(b, env, &self.t, &pair, h);
            });
            b.br(l_next, &[]);
            b.switch_to(l_next);
        }
        Carry { out, accs: mcx::to_vec_in(&row.carry.accs[..], env.mcx) }
    }

    fn window_end(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _c: &Carry) {
        self.table.window_end(b, env, &self.t);
    }

    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) {
        self.table.finish(b, env, &self.t, x);
    }

    /// The grouped count(DISTINCT) owner over the pair (the tag its group word, the image its one pair word): each
    /// owner partition dedups its pairs, round 2 counts each tag's into its accumulator (exec owner.rs's distinct form).
    fn pipeline_desc(&self, px: &Px<'_, '_>, _k: usize, d: Pipeline) -> Pipeline {
        let (pair, acc) = (self.t.layout, acc_layout(px, self.w));
        Pipeline { words: None, ..d }.owner(OwnerDesc::distinct(pair, acc, 1, self.groups, ORDER_NONE, 0, [(REP_NONE, 0); MAX_REPS]))
    }
}

/// The emit's frame words: the cursor (0, then 1 past the one row) and the gather's state.
const W_CURSOR: u8 = 0;
const W_STATE: u8 = 1;

struct CountsEmit<'g, 'p> {
    g: &'g GroupDecl<'p>,
    acc: RecordLayout,
    words: [i32; 2],
    /// An embedded inner's region past its owner's first (distinct_plain.rs `Emit::at`): gathered there, fresh at every call.
    at: Option<u64>,
    mcx: Mcx<'g>,
}

/// PSP2-34's emit (distinct_plain.rs hands a `multi` group's drain here): one row, each call's count its tag's pairs.
pub(crate) fn make_emit<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, g: &'g GroupDecl<'p>) -> PgBox<'g, dyn SourceMech + 'g> {
    let word = |px: &mut Px<'_, '_>, i: u8| px.frame_word(WordKey::Open { face: Face::Keyed, k, j: u64::MAX, i }) as i32 * 8;
    let words = [word(px, W_CURSOR), word(px, W_STATE)];
    let tail = px.tails.iter().any(|t| t.ident == px.ident(k));
    let at = (k >= px.gx.pipelines().len()).then(|| crate::rerun::sets::run_region(px, k, g.slot)).flatten().filter(|&i| i != 0 || tail).map(|i| i as u64);
    mcx::pg_box_dyn!(px.mcx(), CountsEmit { g, acc: acc_layout(px, image_width(g)), words, at, mcx: px.mcx() } => dyn SourceMech + 'g)
}

impl<'g> CountsEmit<'g, '_> {
    /// The gather's state filed in the state word; a null state refuses (the open's status).
    fn filed(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, st: Value) -> Value {
        let sti = b.conv(ConvOp::PtrToInt, Type::I64, st);
        env.store_word(b, self.words[W_STATE as usize], sti);
        let refused = b.icmp(IntCond::Eq, sti, env.zero);
        let unsup = b.iconst(Type::I64, unsupported(UNSUPPORTED_SCRATCH));
        b.select(refused, unsup, env.zero)
    }

    /// Each key's distinct count: the pairs of the kept accumulator whose tag is that key's index, 0 where none is.
    fn counts(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, st: Value) -> PgVec<'g, Value> {
        let n = self.g.keys.len();
        let (ptrs, kept) = (b.load(Type::Ptr, st, ES_PTRS), b.load(Type::I64, st, ES_KEPT));
        let params = mcx::pg_vec![in self.mcx; Type::I64; n + 1];
        let (h, body) = cancel::header(b, env.ctx, env.l_cancel, &params[..], env.mcx);
        let l_done = b.create_block(&params[1..]);
        let first = mcx::pg_vec![in self.mcx; env.zero; n + 1];
        b.br(h, &first[..]);
        b.switch_to(body);
        let i = b.block_params(body)[0];
        let counts: PgVec<'g, Value> = mcx::to_vec_in(&b.block_params(body)[1..], self.mcx);
        let more = b.icmp(IntCond::Slt, i, kept);
        let l_step = b.create_block(&[]);
        b.cond_br(more, (l_step, &[]), (l_done, &counts[..]));
        b.switch_to(l_step);
        let at = b.ptr_add(ptrs, i, 8, 0);
        let rec = b.load(Type::Ptr, at, 0);
        let tag = b.load(Type::I64, rec, 0);
        let pairs = b.load(Type::I64, rec, self.acc.slot_off(ACC_PAIRS as usize) as i32);
        let one = b.iconst(Type::I64, 1);
        let mut next = mcx::pg_vec![in self.mcx; b.int_bin(IntBinOp::Add, i, one)];
        for (j, &c) in counts.iter().enumerate() {
            let tj = b.iconst(Type::I64, j as i64);
            let hit = b.icmp(IntCond::Eq, tag, tj);
            let add = b.select(hit, pairs, env.zero);
            next.push_in(b.int_bin(IntBinOp::Add, c, add));
        }
        b.br(h, &next[..]);
        b.switch_to(l_done);
        mcx::to_vec_in(&b.block_params(l_done)[..], self.mcx)
    }

    /// Every call's count into a stack area, 16 bytes each (the datum, then its valid word: always), as the row's cells by aggno.
    fn finals<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, st: Value) -> AggRow<'e> {
        let aggs = &self.g.aggs;
        let area = {
            let slot = b.declare_stack_slot(16 * aggs.len() as u32, 8);
            b.stack_addr(slot)
        };
        let naggno = aggs.iter().map(|d| d.aggno.max(0) as usize + 1).max().unwrap_or(0);
        let mut cells = mcx::pg_vec![in env.mcx; AggCell::word(0, CellValid::Always); naggno];
        let counts = self.counts(b, env, st);
        for (i, d) in aggs.iter().enumerate() {
            let off = 16 * i as i32;
            b.store(area, off, counts[d.count_slot as usize]);
            b.store(area, off + 8, env.one);
            cells[d.aggno as usize] = AggCell::word(off as u32, CellValid::Always);
        }
        AggRow { rec: area, cells }
    }
}

impl SourceMech for CountsEmit<'_, '_> {
    fn open_words(&self, w: &mut OpenWords) {
        for (i, what) in ["distinct cursor", "distinct state"].into_iter().enumerate() {
            w.declare(OpenWord::open(what, Base::Words, self.words[i], Type::I64, 0));
        }
    }

    /// The fresh call's gather of every accumulator the owner left (the kernel's fresh test is the resume word, so it
    /// reads NONE around the call); a null state refuses.
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        let desc = crate::table::emit_group::desc(b, &self.acc, &[], 0);
        let mem = HostFlags { touches_memory: true, ..HostFlags::default() };
        if let Some(at) = self.at {
            let f = b.declare_host_once("agg_gather_at", &[Type::Ptr, Type::Ptr, Type::I64, Type::Ptr], Some(Type::Ptr), agg_gather_at as *const () as usize, mem);
            let (at, scratch) = (b.iconst(Type::I64, (at | GATHER_FRESH) as i64), b.iconst(Type::Ptr, 0));
            let st = b.call_host(f, &[env.ctx, desc, at, scratch], Some(Type::Ptr)).expect("the gather returns its state");
            return self.filed(b, env, st);
        }
        let gather = b.declare_host_once("agg_gather", &[Type::Ptr, Type::Ptr], Some(Type::Ptr), agg_gather as *const () as usize, mem);
        let none = b.iconst(Type::I64, RESUME_NONE);
        b.store(env.ctx, hdr::RESUME as i32, none);
        let st = b.call_host(gather, &[env.ctx, desc], Some(Type::Ptr)).expect("the gather returns its state");
        let open = b.iconst(Type::I64, RESUME_OPEN);
        b.store(env.ctx, hdr::RESUME as i32, open);
        self.filed(b, env, st)
    }

    /// `[cursor, 1)`: the one row, then the end.
    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        let cursor = env.load_word(b, self.words[W_CURSOR as usize]);
        let past = b.icmp(IntCond::Sge, cursor, env.one);
        let l_win = b.create_block(&[]);
        b.cond_br(past, (l_eof, &[env.zero]), (l_win, &[]));
        b.switch_to(l_win);
        Win { cursor, end: env.one, facts: WinConds::default() }
    }

    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        let st = env.load_word(b, self.words[W_STATE as usize]);
        let st = b.conv(ConvOp::IntToPtr, Type::Ptr, st);
        env.aggs = Some(self.finals(b, env, st));
        RowVals { row: Row { cells: PgVec::new_in(env.mcx) }, vals: PgVec::new_in(env.mcx), cols: PgVec::new_in(env.mcx), carry, next, at: j }
    }

    fn write_back(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, cur: Value, _out: Value) {
        env.store_word(b, self.words[W_CURSOR as usize], cur);
    }
}

#[cfg(test)]
mod tests {
    use gravity_abi::breaker::{KeyKind, ACC_PAIRS};

    /// The pair is `[tag, image]` with the implicit sentinel; the accumulator keeps the tag and counts its pairs.
    #[test]
    fn the_tagged_pair_owns_one_accumulator_per_tag() {
        let pair = super::pair_layout(1);
        assert!(matches!(pair.key_kinds(), [KeyKind::I64, KeyKind::I64]));
        assert!(matches!(super::pair_layout(2).key_kinds(), [KeyKind::I64, KeyKind::I64, KeyKind::I64]));
        let acc = gravity_program::OwnerDesc::accumulator_of(&pair, 1);
        assert_eq!(acc.nkeys(), 1);
        assert!((ACC_PAIRS as usize) < acc.nslots());
        let d = gravity_program::OwnerDesc::distinct(pair, acc, 1, 2, gravity_abi::breaker::ORDER_NONE, 0, [(gravity_program::REP_NONE, 0); gravity_program::MAX_REPS]);
        assert_eq!(d.pair, 1);
    }
}
