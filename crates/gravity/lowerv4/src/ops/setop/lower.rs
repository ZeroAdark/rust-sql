//! Kind `setop.setop`: INTERSECT / EXCEPT [ALL], hashed and sorted (PostgreSQL 18 nodeSetOp.c; H13d, R9-L07, R9-L10).
//! Budget 400 lines (files.tsv, +25 % gate). Owner M9-SETOP.
//!
//! Decompose declares each side a group keyed on the compared columns (agg.rs `set_side`; a sorted
//! SetOp's left group is a sorted group over its Sort, so the emit keeps PG's output order):
//! the row count is the record's sentinel (PG's numLeft / numRight), and the left record has a second `Count` slot, its
//! copy word. The emit pipeline walks the left groups (keyed.emit_group) and consumes the right group whole. After the
//! right side ran, protocol `setop.setop` (exec v4 m9.rs) probes the right records by key image for every left record and
//! files PG's copy count (setop_retrieve_hash_table / set_output_count) in the copy word:
//! - INTERSECT: 1 when both counts are nonzero; INTERSECT ALL: min(numLeft, numRight);
//! - EXCEPT: 1 when numRight is 0; EXCEPT ALL: max(numLeft - numRight, 0).
//! The protocol publishes the copy word's byte offset + 1 in this step's parameter word at prepare. The step hands each
//! left group's row on that many times (an expanding step, H32: its offset word counts the copies handed on, so a stop
//! inside the row resumes past them).
//!
//! Neither side spills: decompose declares both breakers `Spill::None`, so a side past its share grows (D254), as PG 18's
//! hashed SetOp keeps its table in memory past work_mem. The protocol reads both whole tables.
//!
//! NULL keys meet: a group puts every NULL key in one group, and the protocol compares whole key images with their NULL
//! flags (NULL value words zeroed), as PG's set ops compare with execTuplesMatch (NULLs equal), not the operators'
//! strictness. The row is the left group's representative (its key cells), as PG emits the first left tuple of a group.
//!
//! A key is served when its image is its equality in whole words, the same on both sides (D1168, keysem `set_key`): one-word
//! images, the Fp128 of text / bpchar / bytea (and of a deterministic user collation's text), the 16-byte uuid / interval
//! / timetz images, and (SETOP R1) the exact Fp128 of a key with no native image (rep.hosted.hash's canonical form), of a
//! nondeterministic collation's text (key.coll's sort key), of a numeric without a scale and of a bit string: equal values
//! under the type's `=` meet, and the left group's representative is the row (the equal-member rulings). A dictionary code
//! refuses by name. Each side's key is compared as its table holds it (keysem `held_form`, keyimage.typmod): a narrow numeric whose
//! cell is a varlena keys by its normalized digits' Fp128 whatever scale its typmod names, so `numeric(10,2)` against
//! `numeric(12,4)` or `numeric` meets as PG's numeric_eq / hash_numeric do (trailing zeros of the scale ignored).
//! No column (F-ZERO, `SELECT INTERSECT SELECT`): each side is a plain group and the copy count comes from the two row
//! counts (`plain`). setop.emptyleft (empty_left.rs, R9-L09) keeps PG's rule that an empty left side never runs the right.
use gravity_abi::breaker::KeyKind;
use gravity_abi::status::{unsupported, UNSUPPORTED_SHAPE};
use gravity_decompose::catalog::keysem::image::{held_form, set_key};
use gravity_decompose::ops::shared::agg::GroupDecl;
use gravity_decompose::ops::shared::setop::{SetCmd, SetDesc, SetForm};
use gravity_decompose::Op;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Type, Value};
use gravity_program::protos::{Proto, ProtoKind};
use mcx::{CollectIn, GrowIn, PgBox, PgVec};

use crate::face::kinds::KindId;
use crate::face::step::{Expand, ExpandKind, StepMech};
use crate::frame::env::Env;
use crate::face::end::EndMech;
use crate::frame::frame::{EndMaker, Next, SourceMaker, StepMaker};
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::{self, Exits, ResumeWord};
use crate::graph::Gx;
use crate::admit::refusal::{Reason, ShapeWhy};

/// The family's one registered table (ops/mod.rs dispatches it): setop.setop's makers, then setop.recursive's (recursive.rs).
pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { kinds: &[(KindId::SetopSetop, &StepMaker(make)), (KindId::SetopSetop, &EndMaker(make_end)), (KindId::SetopRecursive, &SourceMaker(super::recursive::make_source)), (KindId::SetopRecursive, &StepMaker(super::recursive::make_step)), (KindId::SetopRecursive, &EndMaker(super::recursive::make_end))], ..crate::ops::NONE };

/// The copy-count protocol's `arg` (exec v4 m9.rs reads the same fields): the command, the right group's slot and the key
/// count. No field names a pipeline (D1274): exec finds the left producer from the emit's `consumes_from`, in program
/// order, which frame/program.rs `to_program` keeps when a pass reorders the pipelines.
pub const ARG_CMD_MASK: u64 = 0x0f;
pub const ARG_RIGHT_SLOT_SHIFT: u32 = 8;
/// The key count, bits 32..48: room for every column of PostgreSQL's widest target list (MaxTupleAttributeNumber).
pub const ARG_NKEYS_SHIFT: u32 = 32;
pub const ARG_NKEYS_MASK: u64 = 0xffff;
/// D1168: the key image's record words (`record_words`: the first NULL flags word's index in the record), bits 48..63; bit
/// 63 is the kernels' `SETOP_ARG_LOUD`.
pub const ARG_WORDS_SHIFT: u32 = 48;
pub const ARG_WORDS_MASK: u64 = 0x7fff;
const _: () = assert!(gravity_abi::limits::MAX_TUPLE_ATTRIBUTE_NUMBER as u64 <= ARG_NKEYS_MASK && (ARG_NKEYS_MASK << ARG_NKEYS_SHIFT) & (ARG_WORDS_MASK << ARG_WORDS_SHIFT) == 0);
const _: () = assert!((ARG_WORDS_MASK << ARG_WORDS_SHIFT) & gravity_kernels::ops::setop::entries::SETOP_ARG_LOUD == 0);
/// The plain form (no column, F-ZERO): the command field's bit 4, the two sides' sentinel byte offsets at bits 32 and 48.
pub const ARG_PLAIN: u64 = 1 << 4;
pub const ARG_PLAIN_LEFT_SHIFT: u32 = 32;
pub const ARG_PLAIN_RIGHT_SHIFT: u32 = 48;

/// PG's `SETOPCMD_*` order, the protocol's command byte.
pub const fn cmd_code(c: SetCmd) -> u64 {
    match c {
        SetCmd::Intersect => 0,
        SetCmd::IntersectAll => 1,
        SetCmd::Except => 2,
        SetCmd::ExceptAll => 3,
    }
}

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

fn bug(node: i32, payload: &'static str) -> ! {
    crate::refuse!(node, Reason::Bug { payload })
}

/// The group a set side's life holds (decompose `set_side`), with its producer pipeline.
/// D1239: a sorted set operation's side that is not its group is a spool decompose left (its right input is not a Sort and may
/// raise or call something volatile, so PG's early stop is observable): the same refusal by name as `make_end`'s, never a bug
/// (a spool over a DISTINCT is that DISTINCT's own group, so `make_end` never sees it).
fn side<'g, 'p>(gx: &Gx<'g, 'p>, k: usize, slot: u8, sorted: bool, node: i32) -> (&'g GroupDecl<'p>, usize) {
    let Some(life) = gx.life_of(k, slot) else { bug(node, "a set operation without its side") };
    match gx.graph_group(life) {
        Some(g) if g.aggs.is_empty() && g.producer == life => (g, life.0 as usize),
        _ if sorted => shape(node, SORTED_SPOOL),
        _ => bug(node, "a hashed set operation's side that is not its hashed group"),
    }
}

/// A sorted set operation over spools (decompose left them: the right input is not a Sort and PG may stop reading it early).
const SORTED_SPOOL: &str = "a sorted set operation whose right input is not a Sort (PG may stop reading it early): not built yet";

/// Each key's record kind and words in the side's table (face keyimg.rs `record_words`: the one rule the keyed record's
/// layout and its NULL flags words follow, keyed/hashagg_rec.rs `wpk` / `flags_word`), and their sum: the first flags
/// word's index (D1168). A narrow numeric over a varlena and a nondeterministic key take two Fp128 words there, not
/// decompose's.
fn record_words<'g>(gx: &Gx<'g, '_>, producer: usize, g: &GroupDecl<'_>) -> (PgVec<'g, (KeyKind, u8)>, u64) {
    let w: PgVec<'g, (KeyKind, u8)> = g.keys.iter().map(|k| crate::table::keyimg::record_words(gx, producer, k)).collect_in(gx.mcx);
    let n = flags_index(&w[..]);
    (w, n)
}

/// The first NULL flags word's index in a keyed record over keys of these record words: after every image word
/// (keyed/coderep.rs `image_layout` lays the words out in key order, then the flags words; hashagg_rec.rs `flags_word` is
/// their sum).
fn flags_index(words: &[(KeyKind, u8)]) -> u64 {
    words.iter().map(|&(_, n)| n as u64).sum()
}

pub(super) fn make<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let node = p.node_ops.iter().rev().find(|n| n.first_op as usize <= j).map_or(p.node_id, |n| n.node_id);
    let Op::SetOp(SetDesc::Set { cmd, left, right, hashed, form, pull_loud, .. }) = &p.ops[j] else { bug(node, "a setop.setop step that is not a set operation") };
    if let SetForm::Stream { .. } = form {
        return super::stream::make(px, k, j, node); // D2801: PG's sorted walk in the left input's pipeline (stream.rs)
    }
    if j != 0 {
        bug(node, "a hashed set operation after another op of its emit");
    }
    let ((lg, lp), (rg, rp)) = (side(&gx, k, *left, !*hashed, node), side(&gx, k, *right, !*hashed, node));
    // a right input that may raise (decompose `ahead`) runs whole under a recheck licence, held by the step
    // (`Copies::ahead`), else refuses by name; construct C2: a quiet right input the grouped form reads whole past PG's
    // sorted stop holds decompose's licence (`SetForm::Grouped`), stamped on its pipeline for the run-ahead tracer
    let ahead = match *form {
        SetForm::Ahead => Some(px.run_ahead(rp, gravity_decompose::effects::Effects::RAISES).unwrap_or_else(|| shape(node, SORTED_SPOOL))),
        SetForm::Grouped(l) => l.inspect(|l| px.hold(rp, l)),
        SetForm::Stream { .. } => None,
    };
    if gx.drained_group(k).is_none_or(|g| !core::ptr::eq(g, lg)) {
        bug(node, "a hashed set operation whose emit does not walk its left group");
    }
    if px.consumed(k, *right).is_none() {
        bug(node, "a hashed set operation whose emit does not consume its right group");
    }
    if lg.keys.len() != rg.keys.len() {
        bug(node, "a set operation over sides of different width");
    }
    if lg.keys.is_empty() {
        return plain(px, k, j, *cmd, (lg, lp), (rg, rp), (*left, *right), node, ahead);
    }
    // CAP-28: the protocol compares each side's NULL flags words (one per 64 columns); the key count and the image words
    // have arg fields wider than any target list PostgreSQL builds, so these two bounds are sanity checks
    if lg.keys.len() as u64 > ARG_NKEYS_MASK {
        shape(node, "a set operation over more key columns than the protocol's key count field holds");
    }
    let facts = gx.facts;
    if let Some(why) = lg.keys.iter().zip(rg.keys.iter()).find_map(|(l, r)| set_key(&|c| facts.coll(c).bytes_eq, &held_form(l, crate::table::keyimg::record_words(&gx, lp, l)), &held_form(r, crate::table::keyimg::record_words(&gx, rp, r)))) {
        shape(node, why);
    }
    // both sides' tables must hold one image per key: the same record kinds and words (a varlena numeric's Fp128 on one
    // side and a packed lane's mantissa on the other are not one domain)
    let ((lw, words), (rw, _)) = (record_words(&gx, lp, lg), record_words(&gx, rp, rg));
    if lw != rw {
        shape(node, "a set operation whose sides key one column in different record images (a varlena numeric vs a packed lane's)");
    }
    if words > ARG_WORDS_MASK {
        shape(node, "a set operation over more key image words than the protocol's words field holds");
    }
    let word = px.proto_words(1);
    // D2805: decompose's `pull_loud` (a root-path sorted SetOp whose left streams a raise) rides the keyed form's arg for
    // exec runner/admission.rs, which refuses the program on a pull that may count its rows
    let loud = if *pull_loud { gravity_kernels::ops::setop::entries::SETOP_ARG_LOUD } else { 0 };
    let arg = cmd_code(*cmd) | (*right as u64) << ARG_RIGHT_SLOT_SHIFT | (lg.keys.len() as u64) << ARG_NKEYS_SHIFT | words << ARG_WORDS_SHIFT | loud;
    px.declare_proto(Proto::new(ProtoKind::SetOp, k as u16).peer(rp as u16).slot(*left).words(word, 1).arg(arg));
    crate::ops::setop::strategies::empty_left::declare(px, lp, rp, *left);
    let offset = stop::resume_word(px, ResumeWord::StepOffset { k, j: crate::frame::px::key_j(j) });
    mcx::pg_box_dyn!(px.mcx(), Copies { node, word: word as i32, offset, plain: false, ahead } => dyn StepMech + 'g)
}

/// F-ZERO: a set operation over no column. Each side is a plain group (decompose `set_side`): one record whose sentinel
/// counts the side's rows. The plain drain emits the left side's one (empty) row; the protocol's plain form files PG's copy
/// count of the two counts behind the step's word (a pointer to it: the count is known only once the right side ran).
#[allow(clippy::too_many_arguments)]
fn plain<'g>(px: &mut Px<'g, '_>, k: usize, j: usize, cmd: SetCmd, (lg, lp): (&GroupDecl<'_>, usize), (rg, rp): (&GroupDecl<'_>, usize), (left, right): (u8, u8), node: i32, ahead: Option<gravity_decompose::effects::Licence>) -> PgBox<'g, dyn StepMech + 'g> {
    // bundle 15 B: a plain side is keyless, so its record is decompose's as the plain drain folds it (fold/slots.rs
    // `layout_of`), never a keyed GroupRec's (which may add a validity word for an order slot)
    debug_assert!(lg.keys.is_empty() && rg.keys.is_empty(), "a plain set side with keys");
    let sent = |g: &GroupDecl<'_>| g.layout.record(px.arena).sentinel_off() as u64;
    let (ls, rs) = (sent(lg), sent(rg));
    if ls > 0xffff || rs > 0xffff {
        bug(node, "a plain set side whose sentinel is past 64 KiB");
    }
    let word = px.proto_words(1);
    let arg = cmd_code(cmd) | ARG_PLAIN | (right as u64) << ARG_RIGHT_SLOT_SHIFT | ls << ARG_PLAIN_LEFT_SHIFT | rs << ARG_PLAIN_RIGHT_SHIFT;
    px.declare_proto(Proto::new(ProtoKind::SetOp, k as u16).peer(rp as u16).slot(left).words(word, 1).arg(arg));
    crate::ops::setop::strategies::empty_left::declare_plain(px, lp, rp, left, ls);
    let offset = stop::resume_word(px, ResumeWord::StepOffset { k, j: crate::frame::px::key_j(j) });
    mcx::pg_box_dyn!(px.mcx(), Copies { node, word: word as i32, offset, plain: true, ahead } => dyn StepMech + 'g)
}

/// The side ends decompose leaves to setop.setop: a sorted set operation's spools (`sort::spool`; its right input is not a
/// Sort). The streamed form's right spool fills as its step pulls it (stream.rs); any other refuses by name at the side,
/// before any row.
fn make_end<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    let gx = px.gx;
    let node = gx.pipelines()[k].node_id;
    let life = Some(gravity_program::LifeId(k as u16));
    let reader = gx.pipelines().iter().enumerate().find_map(|(q, p)| {
        p.ops.iter().find_map(|op| match op {
            Op::SetOp(SetDesc::Set { left, right, hashed: false, form, .. }) if gx.life_of(q, *left) == life || gx.life_of(q, *right) == life => Some(*form),
            _ => None,
        })
    });
    match reader {
        Some(SetForm::Stream { .. }) => super::stream::make_end(px, k), // the right input's spool, pulled (stream.rs)
        Some(_) => shape(node, SORTED_SPOOL),
        None => bug(node, "a set operation side that is neither a group nor a sorted spool"),
    }
}

/// The step: the left group's row, handed on its copy count times.
struct Copies {
    node: i32,
    /// The parameter word the protocol files the copy word's byte offset + 1 in.
    word: i32,
    /// The frame word counting the copies of the row handed on (H32).
    offset: i32,
    /// The plain form (F-ZERO): the word points at the copy count itself, not at the record's copy slot.
    plain: bool,
    /// The licence the right input runs whole ahead of PostgreSQL's early stop under: the recheck licence (decompose
    /// `SetForm::Ahead`) or a quiet right input's (`SetForm::Grouped`); `None` for any other form or right input. Held,
    /// not read.
    #[allow(dead_code)]
    ahead: Option<gravity_decompose::effects::Licence>,
}

impl Copies {
    /// The current left group's copy count: its record (keyed.emit_group files it after the row's finals; the group has
    /// none) at the published offset.
    fn copies(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        if self.plain {
            let at = b.load(Type::Ptr, env.params, 8 * self.word);
            return b.load(Type::I64, at, 0);
        }
        let Some(area) = env.aggs.as_ref().map(|a| a.rec) else { bug(self.node, "a set operation's row outside its left group's emit") };
        let rec = b.load(Type::Ptr, area, 0);
        let off = b.load(Type::I64, env.params, 8 * self.word);
        let off = b.int_bin(IntBinOp::Sub, off, env.one);
        let at = b.ptr_add(rec, off, 1, 0);
        b.load(Type::I64, at, 0)
    }
}

impl StepMech for Copies {
    fn open_words(&self, w: &mut OpenWords) {
        w.declare(OpenWord::open("setop.setop copies", Base::Words, self.offset, Type::I64, 0));
    }

    /// A parameter word the protocol never wrote (0) exits UNSUPPORTED_SHAPE, an ERROR, where the first row would read
    /// its record at a wild offset (the plain form: through a null pointer).
    fn prologue(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) {
        let (off, l_ok) = (b.load(Type::I64, env.params, 8 * self.word), b.create_block(&[]));
        let (lost, unsup) = (b.icmp(IntCond::Eq, off, env.zero), b.iconst(Type::I64, unsupported(UNSUPPORTED_SHAPE)));
        b.cond_br(lost, (x.l_exit, &[unsup]), (l_ok, &[]));
        b.switch_to(l_ok);
    }

    /// Copies `[offset word, n)` of the row, the offset word at the next copy while each runs on; 0 after the row.
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        let n = self.copies(b, env);
        let ctypes: PgVec<'_, Type> = row.carry.params().iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
        let mut wtypes = mcx::pg_vec![in env.mcx; Type::I64];
        wtypes.extend_from_slice_in(&ctypes[..]);
        let (l_walk, l_one, l_end) = (b.create_block(&wtypes[..]), b.create_block(&[]), b.create_block(&ctypes[..]));
        let start = env.load_word(b, self.offset);
        let args: PgVec<'_, Value> = [start].into_iter().chain(row.carry.params()).collect_in(env.mcx);
        b.br(l_walk, &args[..]);
        b.switch_to(l_walk);
        b.mark_loop_header(l_walk);
        env.bounded.push_in(l_walk);
        let ps = b.block_params(l_walk);
        let (i, carry) = (ps[0], Carry::from_params(&ps[1..], env.mcx));
        let more = b.icmp(IntCond::Slt, i, n);
        b.cond_br(more, (l_one, &[]), (l_end, &carry.params()));
        b.switch_to(l_one);
        let i1 = b.int_bin(IntBinOp::Add, i, env.one);
        env.store_word(b, self.offset, i1);
        // D1491: a stop after a copy (a receiver's) leaves AT the row, whose offset word says the copies handed on (merge.join's
        // D688 form); leaving past it resumed the next group at this group's offset, its first copies lost
        let out = RowVals { row: row.row.clone(), vals: row.vals.clone(), cols: row.cols.clone(), carry, next: row.at, at: row.at };
        let c = next(b, env, &out);
        let args: PgVec<'_, Value> = [i1].into_iter().chain(c.params()).collect_in(env.mcx);
        b.br(l_walk, &args[..]);
        b.switch_to(l_end);
        let ce = Carry::from_params(b.block_params(l_end), env.mcx);
        env.store_word(b, self.offset, env.zero);
        ce
    }

    fn expand(&self) -> Option<Expand> {
        Some(Expand { kind: ExpandKind::Chain, offset: self.offset, held: 0 })
    }
}

#[cfg(test)]
mod tests {
    use gravity_abi::breaker::KeyKind;
    use gravity_decompose::ops::shared::agg::{KeyDecl, Stripped};
    use gravity_decompose::ops::shared::sort::KeyImage;
    use mcx::MemoryContext;
    use types_nodes::primnodes::Aggref;
    use types_nodes::Node;

    use super::{flags_index, set_key, ARG_NKEYS_MASK, ARG_NKEYS_SHIFT, ARG_WORDS_MASK, ARG_WORDS_SHIFT};

    /// D1168: the recorded keyed layout for keys of record words text (Fp128, 2), int (I64, 1), uuid (I64, 2), text (Fp128,
    /// 2), nullable: seven image words, then the flags word. keyed/coderep.rs's test pins `image_layout` (the layout
    /// hashagg_rec.rs builds, `flags_word` = the words' sum) to this same recording; here setop's flags index must land on
    /// its flags word.
    #[test]
    fn the_flags_index_is_the_keyed_records_flags_word() {
        let words = [(KeyKind::Fp128, 2), (KeyKind::I64, 1), (KeyKind::I64, 2), (KeyKind::Fp128, 2)];
        let recorded = [KeyKind::Fp128, KeyKind::Fp128, KeyKind::I64, KeyKind::I64, KeyKind::I64, KeyKind::Fp128, KeyKind::Fp128, KeyKind::I64];
        assert_eq!(flags_index(&words) as usize, recorded.len() - 1, "the flags word is the recorded layout's last key word");
        assert_eq!(flags_index(&words), 7, "past all seven image words, not past the four keys");
    }

    fn key<'p>(expr: Node<'p>, image: KeyImage, kind: KeyKind, words: u8, coll: u32) -> KeyDecl<'p> {
        KeyDecl { image, words, coll, desc: false, nulls_first: false, word: 0, resno: 1, kind, attno: 1, expr: Stripped::entry(expr), scale: -1, rep: 0xff, precision: 0, tf: 0, rec: None, bytes_eq: false, user_eq: 0, bytes: 0 }
    }

    /// D1168: a set key is served when its image is its equality in whole words, the same on both sides; SETOP R1: the
    /// exact two-word Fp128 of a hosted key (ByRef), a nondeterministic collation's text, a numeric without a scale and a
    /// bit string too. A dictionary code, a one-word image of those and sides whose images differ refuse by name.
    #[test]
    fn set_keys_are_whole_word_equalities() {
        let ctx = MemoryContext::new("d1168");
        let e = Node::mk(ctx.mcx(), Aggref::default()).unwrap();
        let det = |c: u32| c != 16400; // 16400: a nondeterministic user collation
        let served = [
            key(e, KeyImage::Raw, KeyKind::I64, 1, 0),
            key(e, KeyImage::F64, KeyKind::F64, 1, 0),
            key(e, KeyImage::Text, KeyKind::Fp128, 2, 100),
            key(e, KeyImage::Bpchar, KeyKind::Fp128, 2, 100),
            key(e, KeyImage::Bytes, KeyKind::Fp128, 2, 0),
            key(e, KeyImage::Uuid, KeyKind::I64, 2, 0),
            key(e, KeyImage::IntervalNorm, KeyKind::I64, 2, 0),
            key(e, KeyImage::Timetz, KeyKind::I64, 2, 0),
            key(e, KeyImage::Collated, KeyKind::Fp128, 2, 16390),
            key(e, KeyImage::Collated, KeyKind::Fp128, 2, 16400),
            key(e, KeyImage::ByRef, KeyKind::Fp128, 2, 0),
            key(e, KeyImage::NumNormalized, KeyKind::Fp128, 2, 0),
            key(e, KeyImage::Bits, KeyKind::Fp128, 2, 0),
        ];
        for k in served.iter() {
            assert_eq!(set_key(&det, k, k), None, "{:?} is served", k.image);
        }
        let refused = [
            key(e, KeyImage::ByRef, KeyKind::I64, 1, 0),
            key(e, KeyImage::Collated, KeyKind::I64, 1, 16400),
            key(e, KeyImage::NumNormalized, KeyKind::Fp128, 1, 0),
            key(e, KeyImage::Raw, KeyKind::Code, 1, 0),
        ];
        for k in refused.iter() {
            assert!(set_key(&det, k, k).is_some(), "{:?} / {:?} refuses by name", k.image, k.kind);
        }
        let (t, b) = (served[2], served[4]);
        assert!(set_key(&det, &t, &b).is_some(), "sides whose images differ");
        let (c1, c2) = (served[9], key(e, KeyImage::Collated, KeyKind::Fp128, 2, 16401));
        assert!(set_key(&det, &c1, &c2).is_some(), "sides under different collations");
    }

    /// The protocol's `arg` fields exec ops/setop/proto.rs reads (the crates do not depend on each other): the key count
    /// at 32..48 and the image words (the first NULL flags word's index) at 48..63. CAP-28: past 255 of either, as a
    /// 1664-column set operation over two-word keys has (3354 words with its 26 flags words).
    #[test]
    fn the_arg_carries_the_image_words_past_the_key_count() {
        assert_eq!((ARG_NKEYS_SHIFT, ARG_NKEYS_MASK, ARG_WORDS_SHIFT, ARG_WORDS_MASK), (32, 0xffff, 48, 0x7fff));
        for (n, w) in [(3u64, 5u64), (256, 300), (1664, 3354)] {
            let arg = n << ARG_NKEYS_SHIFT | w << ARG_WORDS_SHIFT | gravity_kernels::ops::setop::entries::SETOP_ARG_LOUD;
            assert_eq!(((arg >> ARG_NKEYS_SHIFT) & ARG_NKEYS_MASK, (arg >> ARG_WORDS_SHIFT) & ARG_WORDS_MASK), (n, w));
        }
    }
}
