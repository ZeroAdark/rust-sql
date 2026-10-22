//! The bare pass 1 of keyed.twopass's top rule (V4R-197; the ladder's rung 8, lower/src/twopass.rs ec2df04e1fc, as
//! lowerv2 tail/pass1.rs ported it): no count table. Runs of the one key collapse in the row loop; a run's close
//! appends the bare key word to its partition once, twice for a run of two or more rows; a run opening under the claim's
//! floor bound offers its key to the floor (`pass1_floor`: the claim's `n` first keys in the top's tie order), and the claim's end appends
//! the floor once more (`pass1_close`). The owners (`t3::owner_fns_pass1`'s bare set) keep every key seen twice, and the
//! TwoPass reader (`bare_keys`) lists them: every key of two or more rows, and each claim's n first keys. That is the
//! top rule's admitted set or a superset of it (keyed_twopass `admitted`): a key the list misses has one row and n
//! keys before it in the tie order, each of count 1 (in the list) or more (admitted and ranked above every count-1
//! group), so it is never among the top's n groups; pass 2 then counts every admitted group exactly.
//!
//! Taken only where that holds as the ladder proved it: the top rule (not twopass.having's floor), the hint's one pass-1 key
//! whose image word is its datum and orders as its type does (`word_order`: KeySem's typesem reading, signed or unsigned
//! words, so any by-value type with a word order; a NULL row skips the runs and the floor, and the reader
//! admits the NULL key always: a superset, pass 2 counts it exactly or finds no row of it), count(*) alone in pass 1, the top's tie order on that key ascending, descending or none (the floor keeps
//! the `n` smallest or largest keys). Any other pass 1 keeps the
//! hash aggregate and `kept_keys`. Hosts: pass1_open / pass1_floor / pass1_close, agg_block (kernels host_abi). A
//! sub-file of the sibling twopass.rs.
use core::mem::offset_of;

use gravity_abi::breaker::{HEAD_LEFT, HEAD_POS, T1};
use gravity_decompose::ops::shared::agg::KeyDecl;
use gravity_decompose::ops::shared::agg_class::Class;
use gravity_expr::emit::hash::{emit_key_hash, emit_part, Bits, ImageValues};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, HostFlags, HostFn, IntBinOp, IntCond, Type, Value};
use gravity_kernels::ops::sort::topn::t3;
use gravity_kernels::ops::sort::topn::t3::BARE;
use gravity_kernels::host_abi::{self as abi, AggState, Pass1State, PASS1_APP, PASS1_BOUND, PASS1_STATE_BYTES};
use gravity_kernels::table::params::spill_bits;
use gravity_program::protos::{Proto, ProtoKind};
use gravity_program::{OwnerDesc, Pipeline};
use mcx::PgBox;

use crate::face::end::EndMech;
use crate::frame::env::Env;
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;

use crate::table::rec::GroupRec;
use crate::ops::shared::twopass::Split;

const ST_T1: i32 = (offset_of!(Pass1State, agg) + offset_of!(AggState, t)) as i32;
const ST_HEADS: i32 = (offset_of!(Pass1State, agg) + offset_of!(AggState, heads)) as i32;
const ST_PBITS: i32 = (offset_of!(Pass1State, agg) + offset_of!(AggState, pbits)) as i32;
const T1_BUFFERED: i32 = offset_of!(T1, buffered) as i32;

/// The bare pass 1 as `t3`'s owner phase and export read it (`bare_dups`): one key word, `count(*)`, the dup rule.
pub(crate) const P1_WORDS: [u64; t3::NPARAMS] = {
    let mut p = [0u64; t3::NPARAMS];
    p[t3::P_LIMIT] = 1;
    p[t3::P_SHAPE] = t3::shape_word_nn(1, 1, t3::ORDER_KEYS_ASC, t3::MODE_HASH, 0, None, 0, 0, true);
    p[t3::P_KEYS] = t3::packed(&[(0, t3::K_INT)], 4);
    p[t3::P_SLOTS] = t3::packed(&[(0, t3::S_COUNT)], 4);
    p[t3::P_EXPORT] = t3::EXPORT_DUPS;
    p
};

fn bare_owner(_words: &[u64]) -> gravity_abi::breaker::OwnerFns {
    t3::owner_fns_pass1(&P1_WORDS)
}

/// The top's `n` when pipeline `k` is a planner pass 1 the bare form serves (the module doc's conditions), else `None`.
pub(crate) fn bare_n<'g, 'p>(px: &mut Px<'g, 'p>, k: usize) -> Option<u32> {
    let gx = px.gx;
    let g1 = gx.group_at(k)?;
    let q = (0..gx.pipelines().len()).find(|&q| gx.group_at(q).and_then(|h| h.two_pass).is_some_and(|t| t.pass1 as usize == k))?;
    let g2 = gx.group_at(q)?;
    let tp = g2.two_pass?;
    // pass 1 keys the hint's one column (keyed_twopass: more is a bug); the floor orders its image word as the type orders
    // its datum (KeySem: the word is the datum, `word_order` signed or unsigned)
    if g1.keys.len() != 1 || word_order(&g1.keys[0]).is_none() {
        return None;
    }
    let top = crate::ops::sort::topn::strategies::twopass::top_drain(gx, g2)?;
    let n = crate::ops::sort::topn::lower::bound(px, top).filter(|&n| (1..=0x7f_ffff).contains(&n))?;
    // pass 1 counts its rows alone (the end's sentinel, or an unfiltered count(*)'s)
    if !g1.aggs.iter().all(|d| d.class == Class::CountStar && d.filter.is_none()) {
        return None;
    }
    let rec = GroupRec::counts(px, g1)?;
    (rec.layout.nkeys() == 1).then_some(n as u32)
}

/// Whether key `k`'s image word orders as its type orders its datum, and how: `Some(true)` signed (int2, int4, int8, date,
/// time, the timestamps, money), `Some(false)` unsigned (bool, "char", oid, xid8, pg_lsn) — KeySem `word_image_order` of
/// the key's type where the image is the datum's own bits in one word; `None` for any other image (a code, a fingerprint,
/// a float's or a numeric's word, a two-word image).
fn word_order(k: &KeyDecl<'_>) -> Option<bool> {
    let typ = gravity_decompose::catalog::expr_type::type_of(k.expr.node())?;
    (k.words == 1 && k.image.is_raw()).then(|| gravity_fn::typesem::keysem::word_image_order(typ)).flatten()
}

/// The top's tie order on pass `k`'s key is descending (`bare_n` found the TwoPass pass 2).
fn floor_desc(px: &Px<'_, '_>, k: usize) -> bool {
    let gx = px.gx;
    (0..gx.pipelines().len()).filter_map(|q| gx.group_at(q).and_then(|h| h.two_pass)).any(|t| t.pass1 as usize == k && t.floor_desc)
}

/// Pass 1's end in the bare form (`bare_n`'s `n`).
pub(crate) fn make<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, n: u32) -> PgBox<'g, dyn EndMech + 'g> {
    fires::fire("twopass.bare"); // shapecheck fire signal (REGISTRY NEW-kernels-exec-114)
    let g = px.gx.group_at(k).expect("bare_n found the group");
    let rec = GroupRec::counts(px, g).expect("bare_n built the record");
    let rows = px.gx.pipelines()[k].desc.predicted_rows;
    let null_at = rec.flags_word();
    // KEY-PREDICTED-ROWS road (b): the partition bits decided on pass 1's estimate are a guard of the program
    let bits = gravity_lower::inputs::key::decide(k, false, rows, bare_bits, [BARE.size() as u64, 0, 0, 0]) as u8;
    let unsigned = word_order(&g.keys[0]) == Some(false);
    let shared = shared_floor(px, k, n);
    mcx::pg_box_dyn!(px.mcx(), BareEnd { rec, n, desc: floor_desc(px, k), unsigned, bits, null_at, shared } => dyn EndMech + 'g)
}

/// D3157: pass 1 `k`'s shared floor word (`twopass.floor`, declared once per pipeline), when the top's `n` is within
/// `TWOPASS_SHARED_KEYS_MAX`: its claims merge their floors into it, so each appends again only the keys that entered it.
fn shared_floor(px: &mut Px<'_, '_>, k: usize, n: u32) -> Option<u16> {
    if n as u64 > gravity_program::cost::TWOPASS_SHARED_KEYS_MAX {
        return None;
    }
    let ident = px.ident(k) as u16;
    if let Some(p) = px.protos.iter().find(|p| p.kind == ProtoKind::TwoPassFloor && p.pipeline == ident) {
        return Some(p.words.base);
    }
    let word = px.proto_words(1);
    px.declare_proto(Proto::new(ProtoKind::TwoPassFloor, k as u16).words(word, 1).arg(n as u64));
    fires::fire("twopass.floor.shared"); // shapecheck fire signal (D3157): a bare pass 1's claims share one floor
    Some(word)
}

/// The bare form's partition bits for `rows` pass 1 rows of `a[0]`-byte records (`make`'s guarded decision).
fn bare_bits(rows: u64, a: &[u64; 4]) -> u64 {
    spill_bits(rows, a[0] as u32) as u64
}

struct BareEnd<'g, 'p> {
    rec: GroupRec<'g, 'p>,
    n: u32,
    /// The floor keeps the `n` largest keys (the top's tie order on the key is descending).
    desc: bool,
    /// The key's words order unsigned (`word_order`): the floor and its bound compare them so.
    unsigned: bool,
    /// Every run appends a word or two: the partitions follow the rows, not the group estimate.
    bits: u8,
    /// A nullable key's flags word in the image (bit 0: the key is NULL); such a row leaves the open run as it is.
    null_at: Option<usize>,
    /// D3157: the shared floor's handle word (`shared_floor`), `None` past its cap.
    shared: Option<u16>,
}

fn host(b: &mut FunctionBuilder<'_>, name: &str, params: &[Type], addr: usize) -> HostFn {
    b.declare_host_once(name, params, Some(Type::I64), addr, HostFlags { touches_memory: true, may_error: true, ..HostFlags::default() })
}

fn fail_on(b: &mut FunctionBuilder<'_>, env: &Env, l_exit: gravity_ir::Block, r: Value) {
    let (bad, l_ok) = (b.icmp(IntCond::Slt, r, env.zero), b.create_block(&[]));
    b.cond_br(bad, (l_exit, &[r]), (l_ok, &[]));
    b.switch_to(l_ok);
}

impl BareEnd<'_, '_> {
    fn st(&self, env: &Env) -> Value {
        env.state.expect("the end's state slot is laid before its open")
    }

    /// Key word `k` appended at partition `part`'s head (a full or absent block through `agg_block`, the appends so far
    /// counted there); ends at `l_cont`.
    fn append(&self, b: &mut FunctionBuilder<'_>, env: &Env, part: Value, k: Value, l_cont: gravity_ir::Block) {
        let st = self.st(env);
        let rec = BARE.size() as u64;
        let heads = b.load(Type::Ptr, st, ST_HEADS);
        let hp = b.ptr_add(heads, part, 8, 0);
        let head = b.load(Type::I64, hp, 0);
        let thresh = b.const_raw(Type::I64, rec << HEAD_LEFT);
        let (l_blk, l_st) = (b.create_block(&[]), b.create_block(&[Type::I64]));
        let full = b.icmp(IntCond::Ult, head, thresh);
        b.cond_br(full, (l_blk, &[]), (l_st, &[head]));
        b.switch_to(l_blk);
        let app = b.load(Type::I64, st, PASS1_APP);
        let block = host(b, "agg_block", &[Type::Ptr, Type::Ptr, Type::I64, Type::I64], gravity_abi::host_words!(abi::agg_block(ctx, st, part, appended)));
        let r = b.call_host(block, &[env.ctx, st, part, app], Some(Type::I64)).expect("agg_block returns a head or a status");
        b.store(st, PASS1_APP, env.zero);
        let bad = b.icmp(IntCond::Sle, r, env.zero);
        b.cond_br(bad, (env.x.l_exit, &[r]), (l_st, &[r]));
        b.switch_to(l_st);
        let head = b.block_params(l_st)[0];
        let size = b.iconst(Type::I64, rec as i64);
        let stepped = b.int_bin(IntBinOp::Add, head, size);
        let next = b.int_bin(IntBinOp::Sub, stepped, thresh);
        b.store(hp, 0, next);
        let posm = b.const_raw(Type::I64, HEAD_POS);
        let pos = b.int_bin(IntBinOp::And, head, posm);
        let p = b.conv(ConvOp::IntToPtr, Type::Ptr, pos);
        b.store(p, 0, k);
        let app = b.load(Type::I64, st, PASS1_APP);
        let app1 = b.int_bin(IntBinOp::Add, app, env.one);
        b.store(st, PASS1_APP, app1);
        b.br(l_cont, &[]);
    }

    /// A run's close: its key once, twice for `rc >= 2` (the dup rule), at the partition `pass1_close` routes the floor
    /// to (`KeyHash::of(&[k]).part(pbits)`); ends at `l_cont`.
    fn end_run(&self, b: &mut FunctionBuilder<'_>, env: &Env, k: Value, rc: Value, l_cont: gravity_ir::Block) {
        let st = self.st(env);
        let h = emit_key_hash(b, &ImageValues { words: vec![k] }); // alloc: Program
        let pbits = b.load(Type::I64, st, ST_PBITS);
        let part = emit_part(b, h, Bits::Dyn(pbits));
        let (l_once, l_twice) = (b.create_block(&[]), b.create_block(&[]));
        self.append(b, env, part, k, l_once);
        b.switch_to(l_once);
        let two = b.iconst(Type::I64, 2);
        let more = b.icmp(IntCond::Uge, rc, two);
        b.cond_br(more, (l_twice, &[]), (l_cont, &[]));
        b.switch_to(l_twice);
        self.append(b, env, part, k, l_cont);
    }
}

impl<'p> EndMech for BareEnd<'_, 'p> {
    /// The state alone: the floor's `n` words are the claim arena's (kernels `Pass1State::floor`, D4441)
    fn state_bytes(&self) -> u32 {
        PASS1_STATE_BYTES
    }

    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        let st = self.st(env);
        let open = host(b, "pass1_open", &[Type::Ptr, Type::Ptr, Type::I64, Type::I64, Type::I64, Type::Ptr], gravity_abi::host_words!(abi::pass1_open(ctx, st, bits, n, order, shared)));
        let order = if self.desc { abi::PASS1_DESC } else { 0 } | if self.unsigned { abi::PASS1_UNSIGNED } else { 0 };
        let (bits, n, order) = (b.iconst(Type::I64, self.bits as i64), b.iconst(Type::I64, self.n as i64), b.iconst(Type::I64, order as i64));
        let shared = match self.shared {
            Some(w) => b.load(Type::Ptr, env.params, 8 * w as i32),
            None => b.iconst(Type::Ptr, 0),
        };
        let r = b.call_host(open, &[env.ctx, st, bits, n, order, shared], Some(Type::I64)).expect("pass1_open returns a status");
        fail_on(b, env, env.x.l_exit, r);
    }

    /// The open run: its row count (0: none) and its key.
    fn carried<'e>(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>) -> mcx::PgVec<'e, Value> {
        mcx::pg_vec![in env.mcx; env.zero, env.zero]
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
        let img = self.rec.image(b, env, row);
        let k = img.words.words[0];
        let (rc, rkey) = (row.carry.accs[0], row.carry.accs[1]);
        let out = b.int_bin(IntBinOp::Add, row.carry.out, env.one);
        let l_next = b.create_block(&[Type::I64, Type::I64, Type::I64]);
        if let Some(fw) = self.null_at {
            let (one, l_key) = (b.iconst(Type::I64, 1), b.create_block(&[]));
            let flag = b.int_bin(IntBinOp::And, img.words.words[fw], one);
            let null = b.icmp(IntCond::Ne, flag, env.zero);
            b.cond_br(null, (l_next, &[out, rc, rkey]), (l_key, &[]));
            b.switch_to(l_key);
        }
        let (l_same, l_new, l_end, l_open, l_offer) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        let eq = b.icmp(IntCond::Eq, k, rkey);
        let live = b.icmp(IntCond::Ne, rc, env.zero);
        let same = b.int_bin(IntBinOp::And, eq, live);
        b.cond_br(same, (l_same, &[]), (l_new, &[]));
        b.switch_to(l_same);
        let rc1 = b.int_bin(IntBinOp::Add, rc, env.one);
        b.br(l_next, &[out, rc1, rkey]);
        b.switch_to(l_new);
        b.cond_br(live, (l_end, &[]), (l_open, &[]));
        b.switch_to(l_end);
        self.end_run(b, env, rkey, rc, l_open);
        // the run opened on this key: offered to the floor when at or under the claim's bound (an extreme key such as
        // i64::MAX ascending is offered while the floor fills; the floor's own check drops a repeat or a tie with its worst)
        b.switch_to(l_open);
        let st = self.st(env);
        let bound = b.load(Type::I64, st, PASS1_BOUND);
        let cond = match (self.desc, self.unsigned) {
            (false, false) => IntCond::Sle,
            (true, false) => IntCond::Sge,
            (false, true) => IntCond::Ule,
            (true, true) => IntCond::Uge,
        };
        let under = b.icmp(cond, k, bound);
        b.cond_br(under, (l_offer, &[]), (l_next, &[out, env.one, k]));
        b.switch_to(l_offer);
        let floor = host(b, "pass1_floor", &[Type::Ptr, Type::Ptr, Type::I64], gravity_abi::host_words!(abi::pass1_floor(ctx, st, k)));
        b.call_host(floor, &[env.ctx, st, k], Some(Type::I64));
        b.br(l_next, &[out, env.one, k]);
        b.switch_to(l_next);
        let p = b.block_params(l_next);
        Carry { out: p[0], accs: mcx::to_vec_in(&p[1..], env.mcx) }
    }

    /// The window's open run closed (a run never spans windows: a key split by the edge appends twice, a dup as it is),
    /// the appended count published into T1.
    fn window_end(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, c: &Carry) {
        let (rc, rkey) = (c.accs[0], c.accs[1]);
        let (l_last, l_ended) = (b.create_block(&[]), b.create_block(&[]));
        let live = b.icmp(IntCond::Ne, rc, env.zero);
        b.cond_br(live, (l_last, &[]), (l_ended, &[]));
        b.switch_to(l_last);
        self.end_run(b, env, rkey, rc, l_ended);
        b.switch_to(l_ended);
        let st = self.st(env);
        let t1 = b.load(Type::Ptr, st, ST_T1);
        let (app, buffered) = (b.load(Type::I64, st, PASS1_APP), b.load(Type::I64, t1, T1_BUFFERED));
        let sum = b.int_bin(IntBinOp::Add, buffered, app);
        b.store(t1, T1_BUFFERED, sum);
        b.store(st, PASS1_APP, env.zero);
    }

    /// The claim's end: the floor's keys appended once more, T1 drained into the owners' partitions.
    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) {
        let st = self.st(env);
        let close = host(b, "pass1_close", &[Type::Ptr, Type::Ptr], gravity_abi::host_words!(abi::pass1_close(ctx, st)));
        let r = b.call_host(close, &[env.ctx, st], Some(Type::I64)).expect("pass1_close returns a status");
        fail_on(b, env, x.l_exit, r);
    }

    /// The bare owner phase over `BARE` records; pass 1 never spills (keyed_twopass `Split::One`).
    fn pipeline_desc(&self, px: &Px<'_, '_>, _k: usize, mut d: Pipeline) -> Pipeline {
        d.words = None;
        d.owner = Some(OwnerDesc::named(BARE, bare_owner, None));
        Split::One(None).desc(px.arena, self.rec.node, d)
    }
}

#[cfg(test)]
mod tests {
    use gravity_kernels::ops::sort::topn::t3;

    /// B01: the plan words this pass 1 emits are the kernels' bare form (`t3::bare_dups`), one word per record (`BARE`).
    #[test]
    fn the_bare_plan_is_the_kernels_bare_form() {
        let pl = t3::Plan::from_words(&super::P1_WORDS).expect("P1_WORDS is a plan");
        assert!(t3::bare_dups(&pl));
        assert_eq!(t3::BARE.size(), 8);
    }
}
