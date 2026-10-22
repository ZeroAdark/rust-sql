//! A hashed group's per-claim i64 partials. Hooks / levers: road:num.partial,
//! road:num.pair.
//! Budget 415 lines (files-extra.tsv, +25 % gate). Registers roads `num.partial` (`NUM_PARTIAL`) and
//! `num.pair` (`NUM_PAIR`, P2's pair placement); a row's terms are fold/num_share.rs `terms` (roads num.share and
//! num.narrow's).
//!
//! A front of blocks in the end's state after the table's (keyed/hashagg.rs; `blocks`: an eighth of one thread's L1d, 64
//! on the reference machine), each claimed by one key image for the claim: the image, its hash, the representative words,
//! the row count and one i64 partial per distinct term and fold (a sum, a least, a greatest). A row whose image is in one
//! of its hash's two blocks (the hash's top bits, the next) and whose every term is exact as an i64 at its fixed scale
//! (no leaf NULL, special or past a word; every product, rescale and add checked; a count(x) term is x's 1 / 0) adds there
//! and skips the record.
//! Any other row takes the end's row fold (a colliding image evicts no block), then claims the free block of its two.
//! Blocks close into the table through the end's run close (`close_in`: the registers `[P, P >> 63, (0,) n]`,
//! `[P, s, s, s, n]` and `[n]`, each merged by its class, and the count), their count and partials then zero, their
//! claims kept: an add that overflows closes its block before the row takes the row fold; a row whose record the table
//! lacks closes every block before its locate may evict a record or switch T1 to bypass; the claim's
//! finish closes every block. A window's end closes none (A-24c): the blocks live in the end's per-claim state across
//! the claim's windows, as the table's own words do (a stack slot for a call that runs its claim to the end, frame words
//! for a pipeline that stops inside a claim and resumes, frame/program.rs D2147), and the hashed table does nothing at a
//! window's end (table/face.rs `window_end`), so a block of G groups closes once a claim, not once a window. No record
//! holds a front word and no block a record pointer: the owner merge sees records alone.
//! A-24c: the front is planned where the cost home's rule (`cost::num_front::front_pays`) finds it pays: each aggregate's
//! record step saved a row, by the register kind it closes into, against the front's probe every row pays and the
//! miss (its terms computed for nothing) the rows of the groups the planner's estimate leaves without a block pay, a
//! share with no step at the blocks; fire `num_front.declined` where it does not.
//! P2 (the pair): an image of any count of text keys, each at most a word of bytes by its declared type or its
//! statistics (`pair_key`, D1896: no other key pays the row's test) and a row's each one word in key.tagged's inline
//! form (`[bytes, len << 56]`, len 1 .. 8), places its block at `(c0 + 7 c1 + 7^2 c2 ..)` modulo the blocks (`MIX`): a
//! key declared one character adds its word as it is (`one_char`), a longer one its bytes' share (`SHARE`, F4). That is
//! its first candidate (the second is its hash's), and the row tries that block before the image's hash
//! (keyed/hashagg.rs `lower_row`): a hit adds with no hash and no second block; a miss (or an overflowed add) takes the
//! hashed path with the terms already read, so an image whose pair block another holds claims its hash's block and adds
//! there. The index only places a block; the image's words still decide the hit. Planned under a group estimate within
//! the blocks for one-character keys (their bytes place them as a bijection; past the blocks images outnumber the pair
//! blocks), and where the cost home's rule (`cost::num_pair::pair_pays`) finds the pair pays for any longer key.
use gravity_decompose::ops::shared::agg::{GroupForm, Register as R, NONE};
use gravity_expr::emit::hash::{emit_image_eq_at, ImageValues};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, IntBinOp, IntCond, IntOvfOp, IntUnOp, Type, Value};
use gravity_program::cost::{num_front, num_pair};
use mcx::{CollectIn, GrowIn, Mcx, PgVec};

use crate::table::face::{RunFold, TableDecl, TableMech};
use crate::table::keyimg::ImageWords;
use crate::face::kinds::KindId;
use crate::face::strategy::{Strategy, NUM_NARROW, NUM_SHARE};
use crate::frame::env::Env;
use crate::frame::px::Px;
use crate::frame::row::RowVals;
use crate::admit::refusal::{Reason, Variant};

use crate::ops::agg::numeric::strategies::num_share::Of;
use crate::ops::agg::numeric::strategies::num_tree::Tree;
use crate::ops::agg::hashagg::{Arg, GroupRec};

crate::strategies! {
    /// A hashed group's per-claim i64 partials in a front of image-keyed blocks, closed into the table once a claim.
    NUM_PARTIAL = 122, "num.partial", NumHeap;
    /// Num.partial's block of a short text key image placed by its bytes and tried before the image's hash.
    NUM_PAIR = 123, "num.pair", NumHeap;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(NUM_PARTIAL, &Road), (NUM_PAIR, &Road)], ..crate::ops::NONE };

/// Road num.partial's registration value (the front is `plan`'s).
struct Road;

/// A block's words at most (image, hash, representatives, count, partials).
const WORDS: usize = 16;
/// The front's head bytes: the claim mask (bit i: block i claimed). The blocks follow.
const HEAD: i32 = 8;
/// The front's blocks: as many blocks of `WORDS` words as fill an eighth of one thread's L1d (the cost home's
/// `l1_span`: 64 on the reference machine, 8 KiB), a power of two within 2 and the mask word's 64 bits.
fn blocks() -> u32 {
    let n = gravity_driver::machine::profile().cost().l1_span(8 * WORDS as u64, 1, 8).clamp(2, 64);
    1 << (63 - n.leading_zeros())
}

/// P2's mix of the keys' shares (`SHARE`) into a block index: odd, so `7^j` is a unit modulo the blocks (a power of two)
/// and each key's share alone moves the index by a bijection (two images whose shares differ in one key, by less than the
/// blocks, never share a block); any odd value is as good a mix (7 is P2's).
const MIX: i64 = 7;

/// A key's bytes `b0 .. b7` into its share of the index, `b0 + 7 b1 + .. + 7^7 b7` modulo the blocks (each power taken
/// modulo 64: odd and below 64, a unit modulo any count of blocks). The even bytes and the odd ones each sit in four
/// 16-bit lanes (`LANES`); one multiply each gathers a lane's products in its top lane, four products below 255 x 63
/// each, so no lane carries. A key whose values differ in one byte, by less than the blocks, places them in distinct
/// blocks, and a one-byte key's share is its byte (P2's index).
const LANES: i64 = 0x00ff_00ff_00ff_00ff;
const SHARE: [i64; 2] = [share(0), share(1)];

/// `SHARE`'s multiplier for the even (`odd` 0) or odd (1) bytes: byte `2k + odd`'s power in lane `3 - k`.
const fn share(odd: u32) -> i64 {
    let (mut m, mut k) = (0u64, 0);
    while k < 4 {
        let (mut p, mut j) = (1u64, 0);
        while j < 2 * k + odd {
            p = p * MIX as u64 % 64;
            j += 1;
        }
        m |= p << (16 * (3 - k));
        k += 1;
    }
    m as i64
}

/// The run close a block's registers go through (keyed/hashagg.rs `close_in`): the image, its hash, the registers.
pub(crate) type Close<'c> = dyn FnMut(&mut FunctionBuilder<'_>, &mut Env, &ImageWords, Value, &[Value]) + 'c;

/// A partial's term: a bare column at its aggregate's scale, aggregate `aggno`'s num.window tree, or whether a column is
/// not NULL (count(x)).
#[derive(PartialEq)]
enum Term {
    Bare { c: i16, scale: i16 },
    Tree(i16),
    Present(i16),
}

/// How a block's partial takes its term's value: added (checked), or the least / greatest kept.
#[derive(Clone, Copy, PartialEq)]
enum Op {
    Sum,
    Min,
    Max,
}

impl Op {
    /// The partial of a block holding no row.
    fn ident(self) -> i64 {
        match self {
            Op::Sum => 0,
            Op::Min => i64::MAX,
            Op::Max => i64::MIN,
        }
    }
}

/// An aggregate's registers as a close builds them from its term's partial `P` and the block's count `n`.
#[derive(Clone, Copy)]
enum Form {
    /// class.word's count(*): `[n]`.
    Count,
    /// class.word's count(x): `[P]` (P: the rows whose x is not NULL).
    Present,
    /// class.num_packed: `[P, P >> 63, n]`; D830's carry form `[P, P >> 63, 0, n]`.
    Num { carry: bool },
    /// class.num_packed's min / max: `[P, n]`.
    Ext,
    /// class.num_wide: `[P, s, s, s, n]` (s: P's sign word).
    Wide,
}

impl Form {
    /// The class whose registers the form builds, and their count.
    fn class(self) -> (KindId, u8) {
        match self {
            Form::Count | Form::Present => (KindId::ClassWord, 1),
            Form::Num { carry } => (KindId::ClassNumPacked, 3 + carry as u8),
            Form::Ext => (KindId::ClassNumPacked, 2),
            Form::Wide => (KindId::ClassNumWide, 5),
        }
    }
}

/// A hashed end's front (`plan`).
pub(crate) struct Partial<'g> {
    terms: PgVec<'g, Term>,
    /// A block's partials: each one's term (an index into `terms`) and how it takes the term.
    parts: PgVec<'g, (usize, Op)>,
    /// Each aggregate's form and its partial's index, in the record's aggregate order.
    aggs: PgVec<'g, (Form, usize)>,
    nimg: usize,
    /// Each representative's byte offset in the record, in key order.
    reps: PgVec<'g, i32>,
    /// The end counts the rows into the sentinel (keyed/hashagg.rs `count`): a close hands `close_in` the count word.
    count: bool,
    /// The front's byte offset in the end's state: its head (`HEAD`), then the blocks.
    off: u32,
    /// The front's blocks (`blocks`, a power of two).
    blocks: u32,
    /// P2: the image is short text keys, so a row may take `pair`.
    pair: bool,
    /// F4: bit k: key k is past one byte, its share of the pair index its bytes' (`SHARE`); else its first word (P2).
    wide: u16,
    /// Road num.share (P3 lever i): the terms share subtrees and column gates, typmod-bounded nodes unchecked.
    share: bool,
    /// Road num.narrow (P3 lever ii): a column's heap image at scale 0..4 takes the narrow decode (fold/num_narrow.rs).
    narrow: bool,
    /// `pair`'s row, for its hashed path (`row`): the terms and their bad bit, and whether the pair index places the block.
    pre: core::cell::RefCell<Option<Pre<'g>>>,
    mcx: Mcx<'g>,
}

/// A pair row's reads (`Partial::pair`): its terms, their bad bit, whether its image is a word-of-bytes pair, the pair's block.
struct Pre<'g> {
    vals: PgVec<'g, Value>,
    bad: Value,
    at: (Value, Value),
}

/// A row's probe (`Partial::row`): the join of its two paths, whether its image holds a block, each of its two blocks'
/// index, address and whether it is claimed, and the other rows' path's bits to close (an overflowed add's block).
pub(crate) struct Front {
    l_done: Block,
    hit: Value,
    at: [(Value, Value, Value); 2],
    ovf: Value,
}

/// The front of `rec`'s end whose state holds it at byte `off`, where the road is on, the end is `bare` (the hashed table's
/// plain row fold: no run carry, code front, two-pass pass, derived key, row skip or count top) and every aggregate takes
/// a partial (no FILTER, DISTINCT, ORDER BY or dscale companion; a tree of columns, constants, +, - and *):
/// - count(*), and count(x) of a column (A-24c (a): the rows x is not NULL in);
/// - a class.num_packed sum / avg of a bare column or of a num.window tree;
/// - a class.num_packed min / max of a bare column (A-24c (b): the least / greatest word);
/// - a class.num_wide product, and a sum / avg of a column past one word (A-24c (d): D830's spilled datum, the column's
///   value while it fits a word; a row past it takes the row fold).
pub(crate) fn plan<'g>(px: &Px<'g, '_>, rec: &GroupRec<'_, '_>, bare: bool, count: bool, off: u32) -> Option<Partial<'g>> {
    let g = rec.g;
    let blocks = blocks();
    if !bare || !px.strategies.has(NUM_PARTIAL) || !crate::face::source::on(Variant::Strategy(NUM_PARTIAL)) {
        return None;
    }
    if g.form != GroupForm::Hashed || g.set.is_some() || g.two_pass.is_some() || rec.handle.is_some() || rec.pair_filter.is_some() || rec.aggs.is_empty() || g.aggs.len() != rec.aggs.len() {
        return None;
    }
    let mcx = px.mcx();
    let (mut terms, mut parts, mut aggs) = (PgVec::new_in(mcx), PgVec::new_in(mcx), PgVec::new_in(mcx));
    let mut kinds = [0u32; num_front::KINDS];
    for a in &rec.aggs {
        let (d, carry) = (&a.d, crate::ops::agg::numeric::num_carry::spilled(&a.d));
        if a.filter.is_some() || d.filter.is_some() || d.mods.distinct != 0 || d.mods.order != 0 || d.dscale_slot != NONE {
            return None;
        }
        let col = |c: &i16| Term::Bare { c: *c, scale: d.scale };
        let (form, term, op) = match (crate::props::class_kind(d), d.reg, rec.nums.iter().flatten().find(|n| n.0 == d.aggno)) {
            (KindId::ClassWord, R::CountStar, None) => (Form::Count, None, Op::Sum),
            (KindId::ClassWord, R::CountAny, None) => match a.args.as_slice() {
                [Arg::Col(c)] => (Form::Present, Some(Term::Present(*c)), Op::Sum),
                _ => return None,
            },
            (KindId::ClassNumPacked, R::SumNum | R::AvgNum, Some((_, t, false))) if plain(t) => (Form::Num { carry }, Some(Term::Tree(d.aggno)), Op::Sum),
            (KindId::ClassNumPacked, R::SumNum | R::AvgNum, None) => match a.args.as_slice() {
                // a column past one word (D830: its datum folds in class.num_wide's registers), its value while it fits a word
                [Arg::Col(c)] if carry && a.class.class_kind() == Some(KindId::ClassNumWide) => (Form::Wide, Some(col(c)), Op::Sum),
                [Arg::Col(c)] => (Form::Num { carry }, Some(col(c)), Op::Sum),
                _ => return None,
            },
            (KindId::ClassNumPacked, R::MinNum | R::MaxNum, None) => match a.args.as_slice() {
                [Arg::Col(c)] => (Form::Ext, Some(col(c)), if d.reg == R::MinNum { Op::Min } else { Op::Max }),
                _ => return None,
            },
            (KindId::ClassNumWide, R::SumNumWide | R::AvgNumWide, Some((_, t, true))) if plain(t) => (Form::Wide, Some(Term::Tree(d.aggno)), Op::Sum),
            _ => return None,
        };
        // the class whose registers the form builds (not a D297 wide argument's class.num_varlena), its register count;
        // by type (D2916: a zero-sized class has no address of its own)
        let (kind, words) = form.class();
        if a.class.class_kind() != Some(kind) || a.shape.words != words {
            return None;
        }
        let tree = matches!(term, Some(Term::Tree(_)));
        let k = match form {
            Form::Count => None,
            Form::Present => Some(num_front::Kind::Present),
            Form::Num { .. } => Some(if tree { num_front::Kind::Tree } else { num_front::Kind::Sum }),
            Form::Ext => Some(num_front::Kind::Ext),
            Form::Wide => Some(if tree { num_front::Kind::WideTree } else { num_front::Kind::WideSum }),
        };
        if let Some(k) = k {
            kinds[k as usize] += 1;
        }
        let at = match term {
            None => 0,
            Some(t) => {
                let k = terms.iter().position(|x| same(rec, x, &t)).unwrap_or_else(|| {
                    terms.push_in(t);
                    terms.len() - 1
                });
                parts.iter().position(|&p| p == (k, op)).unwrap_or_else(|| {
                    parts.push_in((k, op));
                    parts.len() - 1
                })
            }
        };
        aggs.push_in((form, at));
    }
    // A-24c: the cost home's rule on the aggregate list and the planner's group estimate (none: the record fold)
    if !num_front::front_pays(g.groups, blocks, &kinds) {
        fires::fire("num_front.declined"); // shapecheck fire signal (A-24c): the cost home kept the record fold
        return None;
    }
    let reps: PgVec<'_, i32> = g.keys.iter().zip(&rec.reps).filter(|(_, s)| s.is_some()).map(|(k, _)| rec.layout.slot_off(k.rep as usize) as i32).collect_in(mcx);
    let nimg = rec.layout.nkeys();
    let on = |r: Strategy| px.strategies.has(r) && crate::face::source::on(Variant::Strategy(r));
    // one-byte keys: the pair within the blocks only (P2: their bytes place them as a bijection; past the blocks images
    // outnumber the pair blocks and the rows of those that hold none pay the pair test for nothing, two keys of 100 groups
    // 1.08x, three keys of 256 1.24x the pair-off program); a key past one byte: the cost home's rule on the displaced
    // images of a random placement (F4)
    let wide = g.keys.iter().enumerate().filter(|(_, k)| !one_char(k)).fold(0u16, |m, (i, _)| m | 1 << i.min(15));
    let fits_pair = if wide == 0 { g.groups <= blocks as u64 } else { num_pair::pair_pays(g.groups, blocks) };
    let pair = !g.keys.is_empty() && fits_pair && g.keys.iter().all(pair_key) && on(NUM_PAIR);
    let (share, narrow) = (on(NUM_SHARE), on(NUM_NARROW));
    let fits = nimg + reps.len() + 2 + parts.len() <= WORDS;
    if fits {
        // shapecheck fire signal (REGISTRY TH-1, TH-2, BN-129/130, RD-answer.*)
        fires::fire("num.partial");
        for (on, name) in [(pair, "num.pair"), (share, "num.share"), (narrow, "num.narrow")] {
            if on {
                fires::fire(name);
            }
        }
    }
    fits.then_some(Partial { terms, parts, aggs, nimg, reps, count, off, blocks, pair, wide, share, narrow, pre: Default::default(), mcx })
}

/// D1896: whether num.pair may place key `k`: a text column of at most a word of bytes, by its declared type or its
/// statistics (typesem `pair_text_key`), under a collation that equates only equal bytes (typesem `claims_coll_bytes`,
/// D-101's rule: a nondeterministic collation's image is its sort key's, never a pair), so a row's image can be a pair;
/// any other key would test every row for a pair it never makes.
fn pair_key(k: &gravity_decompose::ops::shared::agg::KeyDecl<'_>) -> bool {
    k.expr.node().as_var().is_some_and(|v| gravity_fn::typesem::numeric::pair_text_key(v.vartype, v.vartypmod, k.bytes)) && gravity_fn::typesem::claims_coll_bytes(k.coll)
}

/// P2: key `k` is declared one character (typesem `one_char_key`), so its share of the pair index is its first word.
fn one_char(k: &gravity_decompose::ops::shared::agg::KeyDecl<'_>) -> bool {
    k.expr.node().as_var().is_some_and(|v| gravity_fn::typesem::numeric::one_char_key(v.vartype, v.vartypmod))
}

/// Whether the partial evaluates tree `t`: columns, constants, +, - and * (no int cast, no CASE).
fn plain(t: &Tree) -> bool {
    crate::stack_check();
    match t {
        Tree::Col { .. } | Tree::Const { .. } => true,
        Tree::Mul(x, y) | Tree::Add(x, y) | Tree::Sub(x, y) => plain(x) && plain(y),
        Tree::Int { .. } | Tree::Case(_) | Tree::PastWord { .. } | Tree::Cast { .. } | Tree::Div { .. } => false,
    }
}

/// Term `t`'s tree: its aggregate's num.window tree; `None` for a column's term.
fn tree<'r, 'g>(rec: &'r GroupRec<'g, '_>, t: &'r Term) -> Option<&'r Tree<'g>> {
    match t {
        Term::Tree(aggno) => match rec.nums.iter().flatten().find(|n| n.0 == *aggno) {
            Some(n) => Some(&n.1),
            None => crate::refuse!(rec.node, Reason::Bug { payload: "a partial's tree its record does not hold" }),
        },
        Term::Bare { .. } | Term::Present(_) => None,
    }
}

/// One term: the same column at the same scale, the same column's presence, or two equal trees (sum and avg of one
/// product).
fn same(rec: &GroupRec<'_, '_>, x: &Term, y: &Term) -> bool {
    match (tree(rec, x), tree(rec, y)) {
        (Some(p), Some(q)) => p == q,
        (None, None) => x == y,
        _ => false,
    }
}

/// `x op y` as an i64, its signed overflow ORed into `bad`.
pub(crate) fn checked(b: &mut FunctionBuilder<'_>, op: IntOvfOp, x: Value, y: Value, bad: &mut Value) -> Value {
    let o = b.int_ovf(op, x, y);
    *bad = b.int_bin(IntBinOp::Or, *bad, o);
    let bin = match op { IntOvfOp::SaddOvf => IntBinOp::Add, IntOvfOp::SsubOvf => IntBinOp::Sub, IntOvfOp::SmulOvf => IntBinOp::Mul };
    b.int_bin(bin, x, y)
}

impl<'g> Partial<'g> {
    fn words(&self) -> usize {
        self.nimg + 1 + self.reps.len() + 1 + self.parts.len()
    }

    /// A block's count (its image, hash and representatives before it); the partials follow it.
    fn n_at(&self) -> i32 {
        8 * (self.nimg + 1 + self.reps.len()) as i32
    }

    pub(crate) fn state_bytes(&self) -> u32 {
        HEAD as u32 + self.blocks * 8 * self.words() as u32
    }

    fn front(&self, b: &mut FunctionBuilder<'_>, env: &Env) -> Value {
        let st = env.state.expect("the end's state slot is laid before its open");
        b.ptr_add(st, env.zero, 1, self.off as i32)
    }

    fn block(&self, b: &mut FunctionBuilder<'_>, front: Value, i: Value) -> Value {
        let w = b.iconst(Type::I64, 8 * self.words() as i64);
        let o = b.int_bin(IntBinOp::Mul, i, w);
        b.ptr_add(front, o, 1, HEAD)
    }

    /// Block `p`'s partials with a row's term values `vals` taken in: added (the signed overflow ORed into `ovf`), or the
    /// least / greatest kept.
    fn take<'e>(&self, b: &mut FunctionBuilder<'_>, env: &Env<'e, '_>, p: Value, vals: &[Value], ovf: &mut Value) -> PgVec<'e, Value> {
        let n_at = self.n_at();
        self.parts.iter().enumerate().map(|(k, &(t, op))| {
            let s = b.load(Type::I64, p, n_at + 8 * (k as i32 + 1));
            match op {
                Op::Sum => checked(b, IntOvfOp::SaddOvf, s, vals[t], ovf),
                Op::Min => b.int_bin(IntBinOp::Smin, s, vals[t]),
                Op::Max => b.int_bin(IntBinOp::Smax, s, vals[t]),
            }
        }).collect_in(env.mcx)
    }

    /// Block `p` holding no row: its count zero, each partial its identity (its claim kept).
    fn empty(&self, b: &mut FunctionBuilder<'_>, env: &Env, p: Value) {
        b.store(p, self.n_at(), env.zero);
        for (k, &(_, op)) in self.parts.iter().enumerate() {
            let v = if op == Op::Sum { env.zero } else { b.iconst(Type::I64, op.ident()) };
            b.store(p, self.n_at() + 8 * (k as i32 + 1), v);
        }
    }

    /// The claim's open: no block claimed.
    pub(crate) fn open(&self, b: &mut FunctionBuilder<'_>, env: &Env) {
        let f = self.front(b, env);
        b.store(f, 0, env.zero);
    }

    /// The row's terms into its image's block (`probe`), the other rows' path opened (`settle`, over `table`'s record of
    /// the image and `fold`'s run close); `None` (nothing emitted): an inexact image, join.mult.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn row(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, rec: &GroupRec<'_, '_>, row: &RowVals, img: &ImageWords, h: Value, (table, t): (&dyn TableMech, &TableDecl), fold: &dyn RunFold) -> Option<Front> {
        if !img.exact || env.mult.is_some() || img.words.words.len() != self.nimg {
            return None;
        }
        let pre = self.pre.borrow_mut().take();
        let (vals, bad, at) = match pre {
            Some(p) => (p.vals, p.bad, Some(p.at)),
            None => {
                let (vals, bad) = self.terms(b, env, rec, row);
                (vals, bad, None)
            }
        };
        let f = self.probe(b, env, &vals, bad, img, h, at);
        let found = table.find(b, env, t, img, h);
        self.settle(b, env, &f, found, &mut |b, env, img, h, accs| fold.close(b, env, img, h, accs));
        Some(f)
    }

    /// The row's terms and their bad bit (a leaf NULL, special or past a word; a product, rescale or sum past i64).
    fn terms(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, rec: &GroupRec<'_, '_>, row: &RowVals) -> (PgVec<'g, Value>, Value) {
        let ts: PgVec<'_, Of<'_>> = self.terms.iter().map(|t| match (t, tree(rec, t)) {
            (Term::Bare { c, scale }, _) => Of::Col(*c, *scale),
            (Term::Present(c), _) => Of::Present(*c),
            (_, Some(t)) => Of::Tree(t),
            (_, None) => unreachable!("a tree's term"),
        }).collect_in(env.mcx);
        crate::ops::agg::numeric::strategies::num_share::terms(b, env, rec, row, &ts[..], self.share, self.narrow, self.mcx)
    }

    /// P2: the row of a short-text-key image tries its pair block before the image's hash; a hit adds there and
    /// branches to the returned join, the builder is left on the other rows' path (the hash, then `row` with the terms
    /// read here). `None` (nothing emitted) where `row` would decline, or a key of the image is not two words.
    pub(crate) fn pair(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, rec: &GroupRec<'_, '_>, row: &RowVals, img: &ImageWords) -> Option<Block> {
        if !self.pair || !img.exact || env.mult.is_some() || img.words.words.len() != self.nimg || img.widths.is_empty() || img.widths.iter().any(|&w| w != 2) {
            return None;
        }
        let (vals, bad) = self.terms(b, env, rec, row);
        let (l_join, at) = self.pair_at(b, env, &vals, bad, img, img.widths.len());
        let old = self.pre.borrow_mut().replace(Pre { vals, bad, at });
        debug_assert!(old.is_none(), "a pair row's reads its hashed path never took");
        Some(l_join)
    }

    /// The pair block of `img` (words `[w0, t0, w1, t1, ..]`, a word pair for each of its `keys`, then a nullable key's
    /// flags words, which only the image's words compare): whether every key is one word of 1 .. 8 bytes (`t = len << 56`;
    /// a NULL or empty key's words are zero, a longer one's `t` holds bytes or a larger len) and the block
    /// `(c0 + MIX c1 + MIX^2 c2 ..)` modulo the blocks, `c` a key's `SHARE`; a row whose block holds its image adds
    /// `vals` there (checked) and branches to the join; every other row, and an add that overflows, stays on the builder's
    /// path.
    fn pair_at(&self, b: &mut FunctionBuilder<'_>, env: &Env, vals: &[Value], bad: Value, img: &ImageWords, keys: usize) -> (Block, (Value, Value)) {
        let w = &img.words.words[..2 * keys];
        let (low, s56, one, word) = (b.iconst(Type::I64, (1 << 56) - 1), b.iconst(Type::I64, 56), b.iconst(Type::I64, 1), b.iconst(Type::I64, gravity_fn::typesem::numeric::PAIR_BYTES as i64));
        let (lanes, even, odd, s8, s48) = (b.iconst(Type::I64, LANES), b.iconst(Type::I64, SHARE[0]), b.iconst(Type::I64, SHARE[1]), b.iconst(Type::I64, 8), b.iconst(Type::I64, 48));
        let (mix, top) = (b.iconst(Type::I64, MIX), b.iconst(Type::I64, self.blocks as i64 - 1));
        let elig: PgVec<'_, Value> = w.chunks(2).map(|k| {
            let rest = b.int_bin(IntBinOp::And, k[1], low);
            let bare = b.icmp(IntCond::Eq, rest, env.zero);
            let len = b.int_bin(IntBinOp::Ushr, k[1], s56);
            let len = b.int_bin(IntBinOp::Sub, len, one);
            let short = b.icmp(IntCond::Ult, len, word);
            b.int_bin(IntBinOp::And, bare, short)
        }).collect_in(env.mcx);
        let elig = elig[1..].iter().fold(elig[0], |x, &y| b.int_bin(IntBinOp::And, x, y));
        let c: PgVec<'_, Value> = w.chunks(2).enumerate().map(|(j, k)| {
            if self.wide & 1 << j.min(15) == 0 {
                return k[0];
            }
            let e = b.int_bin(IntBinOp::And, k[0], lanes);
            let o = b.int_bin(IntBinOp::Ushr, k[0], s8);
            let o = b.int_bin(IntBinOp::And, o, lanes);
            let e = b.int_bin(IntBinOp::Mul, e, even);
            let o = b.int_bin(IntBinOp::Mul, o, odd);
            let e = b.int_bin(IntBinOp::Ushr, e, s48);
            let o = b.int_bin(IntBinOp::Ushr, o, s48);
            b.int_bin(IntBinOp::Add, e, o)
        }).collect_in(env.mcx);
        // Horner from the last key: c_{k-1} MIX + c_{k-2}, .. (two keys: c0 + MIX c1)
        let i = c.iter().rev().skip(1).fold(c[c.len() - 1], |acc, &ck| {
            let m = b.int_bin(IntBinOp::Mul, acc, mix);
            b.int_bin(IntBinOp::Add, ck, m)
        });
        let i = b.int_bin(IntBinOp::And, i, top);
        let front = self.front(b, env);
        let (mask, one) = (b.load(Type::I64, front, 0), b.iconst(Type::I64, 1));
        let bit = b.int_bin(IntBinOp::Ushr, mask, i);
        let bit = b.int_bin(IntBinOp::And, bit, one);
        let used = b.icmp(IntCond::Ne, bit, env.zero);
        let p = self.block(b, front, i);
        let eq = emit_image_eq_at(b, &img.words, p, 0);
        let yes = b.bconst(true);
        let ok = b.int_bin(IntBinOp::Xor, bad, yes);
        let go = [used, eq, ok].into_iter().fold(elig, |x, y| b.int_bin(IntBinOp::And, x, y));
        let (l_add, l_put, l_join, l_miss) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        b.cond_br(go, (l_add, &[]), (l_miss, &[]));
        b.switch_to(l_add);
        let (n_at, mut ovf) = (self.n_at(), b.bconst(false));
        let sums = self.take(b, env, p, vals, &mut ovf);
        b.cond_br(ovf, (l_miss, &[]), (l_put, &[]));
        b.switch_to(l_put);
        for (k, &s) in sums.iter().enumerate() {
            b.store(p, n_at + 8 * (k as i32 + 1), s);
        }
        let n = b.load(Type::I64, p, n_at);
        let n = b.int_bin(IntBinOp::Add, n, one);
        b.store(p, n_at, n);
        b.br(l_join, &[]);
        b.switch_to(l_miss);
        (l_join, (elig, i))
    }

    /// Terms `vals` (none of them `bad`) added into the block holding `img`, checked; the builder is left on the other rows'
    /// path (the end's row fold), which `settle` opens and `claim` closes.
    /// A pair row (`pair`: whether its image is a word-of-bytes pair, the pair's block) has that block as its first candidate and
    /// its hash's second as the other: a pair block another image holds leaves it a block to claim.
    fn probe(&self, b: &mut FunctionBuilder<'_>, env: &Env, vals: &[Value], bad: Value, img: &ImageWords, h: Value, pair: Option<(Value, Value)>) -> Front {
        let front = self.front(b, env);
        let (mask, one, s) = (b.load(Type::I64, front, 0), b.iconst(Type::I64, 1), b.iconst(Type::I64, 64 - self.blocks.trailing_zeros() as i64));
        let i = b.int_bin(IntBinOp::Ushr, h, s);
        let (j, top) = (b.int_bin(IntBinOp::Add, i, one), b.iconst(Type::I64, self.blocks as i64 - 1));
        let j = b.int_bin(IntBinOp::And, j, top);
        let (i, j) = match pair {
            Some((elig, d)) => (b.select(elig, d, i), j),
            None => (i, j),
        };
        let (mut at, mut here, mut hit) = ([(i, front, bad); 2], [bad; 2], b.bconst(false));
        for (k, x) in [i, j].into_iter().enumerate() {
            let p = self.block(b, front, x);
            let bit = b.int_bin(IntBinOp::Ushr, mask, x);
            let bit = b.int_bin(IntBinOp::And, bit, one);
            let used = b.icmp(IntCond::Ne, bit, env.zero);
            let eq = emit_image_eq_at(b, &img.words, p, 0);
            here[k] = b.int_bin(IntBinOp::And, used, eq);
            hit = b.int_bin(IntBinOp::Or, hit, here[k]);
            at[k] = (x, p, used);
        }
        let (p, k) = (b.select(here[0], at[0].1, at[1].1), b.select(here[0], i, j));
        let yes = b.bconst(true);
        let ok = b.int_bin(IntBinOp::Xor, bad, yes);
        let go = b.int_bin(IntBinOp::And, hit, ok);
        let (l_add, l_put, l_ovf, l_row, l_done) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::I64]), b.create_block(&[]));
        b.cond_br(go, (l_add, &[]), (l_row, &[env.zero]));
        b.switch_to(l_add);
        let (n_at, mut ovf) = (self.n_at(), b.bconst(false));
        let sums = self.take(b, env, p, vals, &mut ovf);
        b.cond_br(ovf, (l_ovf, &[]), (l_put, &[]));
        b.switch_to(l_put);
        for (k, &s) in sums.iter().enumerate() {
            b.store(p, n_at + 8 * (k as i32 + 1), s);
        }
        let n = b.load(Type::I64, p, n_at);
        let n = b.int_bin(IntBinOp::Add, n, one);
        b.store(p, n_at, n);
        b.br(l_done, &[]);
        // (a): the overflowed add's block closes first (`settle`)
        b.mark_cold(l_ovf);
        b.switch_to(l_ovf);
        let bit = b.int_bin(IntBinOp::Shl, one, k);
        b.br(l_row, &[bit]);
        b.switch_to(l_row);
        Front { l_done, hit, at, ovf: b.block_params(l_row)[0] }
    }

    /// The other rows' path opened, before the row fold's upsert: the overflowed add's block closes, and every block when
    /// `found` (the table's record of the image, `TableMech::find`) is null, so the locate that follows finds none open.
    fn settle(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, f: &Front, found: Value, close: &mut Close<'_>) {
        let front = self.front(b, env);
        let (mask, null) = (b.load(Type::I64, front, 0), b.conv(ConvOp::PtrToInt, Type::I64, found));
        let miss = b.icmp(IntCond::Eq, null, env.zero);
        let bits = b.select(miss, mask, f.ovf);
        self.close_bits(b, env, bits, close);
    }

    /// The other rows' path joined: a row whose image holds no block claims the free one of its two, its representatives
    /// read from record `r` (the row fold's: opened or found, their kept copies written), its count and partials zero.
    pub(crate) fn claim(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, r: Value, img: &ImageWords, h: Value, f: &Front) {
        let [(i, p0, u0), (j, p1, u1)] = f.at;
        let yes = b.bconst(true);
        let (none, both) = (b.int_bin(IntBinOp::Xor, f.hit, yes), b.int_bin(IntBinOp::And, u0, u1));
        let free = b.int_bin(IntBinOp::Xor, both, yes);
        let can = b.int_bin(IntBinOp::And, none, free);
        let l_claim = b.create_block(&[]);
        b.cond_br(can, (l_claim, &[]), (f.l_done, &[]));
        b.switch_to(l_claim);
        let (k, p) = (b.select(u0, j, i), b.select(u0, p1, p0));
        for (w, &v) in img.words.words.iter().enumerate() {
            b.store(p, 8 * w as i32, v);
        }
        b.store(p, 8 * self.nimg as i32, h);
        for (w, &at) in self.reps.iter().enumerate() {
            let v = b.load(Type::I64, r, at);
            b.store(p, 8 * (self.nimg + 1 + w) as i32, v);
        }
        self.empty(b, env, p);
        let front = self.front(b, env);
        let (mask, one) = (b.load(Type::I64, front, 0), b.iconst(Type::I64, 1));
        let bit = b.int_bin(IntBinOp::Shl, one, k);
        let mask = b.int_bin(IntBinOp::Or, mask, bit);
        b.store(front, 0, mask);
        b.br(f.l_done, &[]);
        b.switch_to(f.l_done);
        env.memo.clear(); // the row fold's reads do not dominate the join
    }

    /// The claim's finish: every block holding rows closes through `fold`'s run close (`flush_with`).
    pub(crate) fn flush(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, fold: &dyn RunFold) {
        self.flush_with(b, env, &mut |b, env, img, h, accs| fold.close(b, env, img, h, accs));
    }

    fn flush_with(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, close: &mut Close<'_>) {
        let front = self.front(b, env);
        let mask = b.load(Type::I64, front, 0);
        self.close_bits(b, env, mask, close);
    }

    /// The blocks of `bits` holding rows closed into the table through `close`, their count and partials zeroed, their
    /// claims kept. A loop over the set bits, which 64 bound.
    fn close_bits(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, bits: Value, close: &mut Close<'_>) {
        let front = self.front(b, env);
        let (l_head, l_blk, l_close, l_next, l_out) = (b.create_block(&[Type::I64]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        b.br(l_head, &[bits]);
        b.switch_to(l_head);
        b.mark_loop_header(l_head);
        env.bounded.push_in(l_head);
        let m = b.block_params(l_head)[0];
        let z = b.icmp(IntCond::Eq, m, env.zero);
        b.cond_br(z, (l_out, &[]), (l_blk, &[]));
        b.switch_to(l_blk);
        let i = b.int_un(IntUnOp::Ctz, m);
        let (p, n_at) = (self.block(b, front, i), self.n_at());
        let n = b.load(Type::I64, p, n_at);
        let any = b.icmp(IntCond::Ne, n, env.zero);
        b.cond_br(any, (l_close, &[]), (l_next, &[]));
        b.switch_to(l_close);
        let words = (0..self.nimg).map(|w| b.load(Type::I64, p, 8 * w as i32)).collect(); // alloc: Program
        let h = b.load(Type::I64, p, 8 * self.nimg as i32);
        let parts: PgVec<'_, Value> = (0..self.parts.len()).map(|k| b.load(Type::I64, p, n_at + 8 * (k as i32 + 1))).collect_in(env.mcx);
        let s63 = b.iconst(Type::I64, 63);
        let mut accs: PgVec<'_, Value> = PgVec::new_in(env.mcx);
        for &(form, k) in &self.aggs {
            let s = matches!(form, Form::Num { .. } | Form::Wide).then(|| b.int_bin(IntBinOp::Sshr, parts[k], s63));
            match (form, s) {
                (Form::Num { carry }, Some(s)) => accs.extend_in([parts[k], s].into_iter().chain(carry.then_some(env.zero)).chain([n])),
                (Form::Wide, Some(s)) => accs.extend_in([parts[k], s, s, s, n]),
                (Form::Ext, _) => accs.extend_in([parts[k], n]),
                (Form::Present, _) => accs.push_in(parts[k]),
                _ => accs.push_in(n),
            }
        }
        accs.extend_in((0..self.reps.len()).map(|w| b.load(Type::I64, p, 8 * (self.nimg + 1 + w) as i32)));
        accs.extend_in(self.count.then_some(n));
        close(b, env, &ImageWords::plain(ImageValues { words }, true), h, &accs[..]);
        self.empty(b, env, p);
        b.br(l_next, &[]);
        b.switch_to(l_next);
        let one = b.iconst(Type::I64, 1);
        let m1 = b.int_bin(IntBinOp::Sub, m, one);
        let m1 = b.int_bin(IntBinOp::And, m, m1);
        b.br(l_head, &[m1]);
        b.switch_to(l_out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_ir::builder::ProgramBuilder;

    /// Two terms, sum / wide sum / count(*) over them, one image word, one representative at record byte 8, the end's
    /// count: blocks of [image, hash, rep, n, P0, P1] after two words and the mask.
    fn part(m: Mcx<'_>) -> Partial<'_> {
        Partial {
            terms: mcx::pg_vec![in m; Term::Bare { c: 1, scale: 0 }, Term::Bare { c: 2, scale: 2 }],
            parts: mcx::pg_vec![in m; (0, Op::Sum), (1, Op::Sum)],
            aggs: mcx::pg_vec![in m; (Form::Num { carry: false }, 0), (Form::Wide, 1), (Form::Count, 0), (Form::Num { carry: true }, 1)],
            nimg: 1,
            reps: mcx::pg_vec![in m; 8],
            count: true,
            off: 16,
            blocks: 64,
            pair: false,
            wide: 0,
            share: false,
            narrow: false,
            pre: Default::default(),
            mcx: m,
        }
    }

    /// The front's functions under the interpreter over a state, a close log and a record: `open(ctx, st)`, `end(ctx,
    /// st, log, rows)` (a window's end: the front emits nothing), `finish(ctx, st, log)` (the claim's), `row(ctx, st, log,
    /// rec, img, h, t0, t1, bad, found)` (a row the front does not take counts itself and its t0 in log words 1 and 2, as
    /// the row fold would take it, then claims).
    struct Rig {
        prog: gravity_ir::Program,
        f: [gravity_ir::Func; 4],
        st: Vec<u64>,
        log: Vec<u64>,
        rec: Vec<u64>,
        ctx: Vec<u64>,
    }

    impl Rig {
        fn new() -> Rig {
            let cx = mcx::MemoryContext::new("num.partial");
            Rig::with(part(cx.mcx()))
        }

        fn with(part: Partial<'_>) -> Rig {
            let words = part.words();
            let rig = mcx::MemoryContext::new("GravityPlanRig");
            let mut pb = ProgramBuilder::new();
            let core = crate::frame::env::declare(&mut pb);
            let f = [
                pb.declare_func("open", &[Type::Ptr, Type::Ptr], &[Type::I64]),
                pb.declare_func("flush", &[Type::Ptr, Type::Ptr, Type::Ptr, Type::I64], &[Type::I64]),
                pb.declare_func("row", &[Type::Ptr, Type::Ptr, Type::Ptr, Type::Ptr, Type::I64, Type::I64, Type::I64, Type::I64, Type::I64, Type::Ptr], &[Type::I64]),
                pb.declare_func("finish", &[Type::Ptr, Type::Ptr, Type::Ptr], &[Type::I64]),
            ];
            for (k, &func) in f.iter().enumerate() {
                let mut b = pb.define(func);
                let p = b.params().to_vec();
                let x = crate::frame::stop::exits(&mut b);
                let mut env = Env::new(&mut b, core, crate::hir::Ir::none(), &[], x, rig.mcx());
                env.state = Some(p[1]);
                let log = p.get(2).copied().unwrap_or(p[1]);
                let mut close = |b: &mut FunctionBuilder<'_>, _: &mut Env, img: &ImageWords, h: Value, accs: &[Value]| {
                    let (k, w) = (b.load(Type::I64, log, 0), b.iconst(Type::I64, 256));
                    let o = b.int_bin(IntBinOp::Mul, k, w);
                    let at = b.ptr_add(log, o, 1, 64);
                    for (i, v) in img.words.words.iter().chain([&h]).chain(accs).enumerate() {
                        b.store(at, 8 * i as i32, *v);
                    }
                    let one = b.iconst(Type::I64, 1);
                    let k = b.int_bin(IntBinOp::Add, k, one);
                    b.store(log, 0, k);
                };
                match k {
                    0 => part.open(&mut b, &env),
                    1 => {} // a window's end: the front emits nothing
                    3 => part.flush_with(&mut b, &mut env, &mut close),
                    _ => {
                        let img = ImageWords::plain(ImageValues { words: vec![p[4]] }, true); // alloc: Program
                        let fold = |b: &mut FunctionBuilder<'_>| {
                            let (n, t) = (b.load(Type::I64, log, 8), b.load(Type::I64, log, 16));
                            let one = b.iconst(Type::I64, 1);
                            let n = b.int_bin(IntBinOp::Add, n, one);
                            let t = b.int_bin(IntBinOp::Add, t, p[6]);
                            b.store(log, 8, n);
                            b.store(log, 16, t);
                        };
                        let bad = b.icmp(IntCond::Ne, p[8], env.zero);
                        let fr = part.probe(&mut b, &env, &[p[6], p[7]], bad, &img, p[5], None);
                        part.settle(&mut b, &mut env, &fr, p[9], &mut close);
                        fold(&mut b);
                        part.claim(&mut b, &mut env, p[3], &img, p[5], &fr);
                    }
                }
                let z = b.iconst(Type::I64, 0);
                b.ret(&[z]);
                for blk in [x.l_finish, x.l_stop_in, x.l_full, x.l_status, x.l_done, x.l_raise, x.l_cancel, x.l_exit] {
                    b.switch_to(blk);
                    let z = b.iconst(Type::I64, 0);
                    b.ret(&[z]);
                }
            }
            let prog = pb.finish();
            let ctx = vec![0u64; gravity_abi::ctx::ctx_size(1) / 8]; // alloc: Program
            let (st, log, rec) = (vec![0xdead_beef; 3 + 64 * words], vec![0u64; 8 + 32 * 16], vec![0u64, 0xabc]); // alloc: Program
            let mut r = Rig { prog, f, st, log, rec, ctx };
            r.call(0, &[]);
            r
        }

        fn call(&mut self, k: usize, more: &[u64]) {
            let mut args = vec![self.ctx.as_mut_ptr() as u64, self.st.as_mut_ptr() as u64]; // alloc: Program
            if k > 0 {
                args.push(self.log.as_mut_ptr() as u64);
            }
            if k == 1 {
                args.push(more.first().copied().unwrap_or(150)); // the window's rows
            }
            if k == 2 {
                args.push(self.rec.as_mut_ptr() as u64);
                args.extend_from_slice(more);
            }
            gravity_interp::Interp::new(&self.prog).run(self.f[k], &args).expect("runs");
        }

        /// A row of image `img`, hash block `i`, terms `t`; `found`: the table holds its record.
        fn row(&mut self, img: u64, i: u64, t: (i64, i64), bad: bool, found: bool) {
            let h = i << 58 | img;
            self.call(2, &[img, h, t.0 as u64, t.1 as u64, bad as u64, if found { self.rec.as_ptr() as u64 } else { 0 }]);
        }

        /// The closes so far: each block's image and its term partials and count as the registers carry them.
        fn closes(&self) -> Vec<(u64, i64, i64, u64)> {
            (0..self.log[0] as usize).map(|k| { let r = &self.log[8 + 32 * k..]; (r[0], r[2] as i64, r[5] as i64, r[4]) }).collect() // alloc: Program
        }
    }

    /// Main's GO 00:2xZ (a)-(d): a block's row adds in place; a colliding image (both blocks of its pair claimed by others)
    /// takes the row fold and evicts nothing; an add past i64 closes its block, then the row takes the row fold; a row
    /// whose record the table lacks closes every block before its locate; the finish closes every block holding rows, and
    /// a window's end none (A-24c: a block's rows of several windows close once). Each close carries the rows since the
    /// last (none twice, none lost): the closes plus the row fold's rows are every row.
    #[test]
    fn every_flush_point_closes_the_outstanding_partials_once() {
        let mut r = Rig::new();
        r.row(5, 3, (10, 20), false, true); // the row fold, then block 3 claimed for image 5
        r.row(5, 3, (1, 2), false, true);
        r.row(5, 3, (3, 4), false, true); // block 3: (4, 6) n 2
        r.row(7, 3, (100, 1), false, true); // block 4 claimed for image 7 (3 is 5's)
        r.row(9, 3, (50, 5), false, true); // blocks 3 and 4 claimed: the row fold, no claim, no close
        assert_eq!((r.log[1], r.closes()), (3, vec![]), "a collision evicts nothing");
        r.row(7, 3, (6, 6), false, true); // block 4: (6, 6) n 1
        r.row(5, 3, (i64::MAX, 0), false, true); // (a): 4 + MAX overflows: block 3 closes, the row takes the row fold
        assert_eq!(r.closes(), vec![(5, 4, 6, 2)]);
        r.row(5, 3, (8, 8), false, true); // block 3 again: (8, 8) n 1
        r.row(5, 3, (2, 2), true, true); // a bad term: the row fold, nothing closes
        r.row(11, 10, (40, 4), false, false); // (d): a miss closes blocks 3 and 4, then block 10 claimed
        assert_eq!(&r.closes()[1..], &[(5, 8, 8, 1), (7, 6, 6, 1)]);
        r.row(11, 10, (2, 2), false, true);
        r.call(1, &[]); // a window's end: no close
        r.row(11, 10, (3, 3), false, true); // block 10: (5, 5) n 2, across the window's end
        r.row(5, 3, (-1, -1), false, true);
        r.call(1, &[]); // a window's end: no close
        assert_eq!(r.closes().len(), 3, "a window's end closes no block");
        r.row(7, 3, (9, 9), false, true);
        r.call(3, &[]); // the finish
        r.call(3, &[]); // a second close with no row between closes nothing
        assert_eq!(&r.closes()[3..], &[(5, -1, -1, 1), (7, 9, 9, 1), (11, 5, 5, 2)]);
        let rows = [10, 1, 3, 100, 50, 6, i64::MAX, 8, 2, 40, 2, 3, -1, 9];
        let folded = r.log[2] as i64;
        let closed: i64 = r.closes().iter().map(|c| c.1).sum();
        assert_eq!((r.log[1], folded.wrapping_add(closed)), (6, rows.iter().fold(0i64, |a, &x| a.wrapping_add(x))), "every row once");
        assert_eq!(r.st[2], 1 << 3 | 1 << 4 | 1 << 10, "the claims stay");
        let blk = |i: usize| &r.st[3 + 6 * i..3 + 6 * i + 6];
        assert_eq!((blk(3)[2], blk(4)[2], blk(10)[2]), (0xabc, 0xabc, 0xabc), "each claim's representative, read from the record");
        assert!([3, 4, 10].iter().all(|&i| blk(i)[3..] == [0, 0, 0]), "every block zeroed");
    }

    /// P2: the pair rig, `prow(ctx, st, log, rec, w0, w1, w2, w3, h, t0, t1, found)` over a four-word image: the pair
    /// block first; its miss counts itself in log word 3, then the hashed path as `row`'s (the row fold in words 1, 2).
    fn pair_rig() -> (gravity_ir::Program, gravity_ir::Func) {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let m = rig.mcx();
        let part = Partial { pair: true, nimg: 4, reps: PgVec::new_in(m), aggs: mcx::pg_vec![in m; (Form::Num { carry: false }, 0), (Form::Num { carry: false }, 1)], ..part(m) };
        let mut pb = ProgramBuilder::new();
        let core = crate::frame::env::declare(&mut pb);
        let f = pb.declare_func("prow", &[Type::Ptr, Type::Ptr, Type::Ptr, Type::Ptr, Type::I64, Type::I64, Type::I64, Type::I64, Type::I64, Type::I64, Type::I64, Type::Ptr], &[Type::I64]);
        let mut b = pb.define(f);
        let p = b.params().to_vec();
        let x = crate::frame::stop::exits(&mut b);
        let mut env = Env::new(&mut b, core, crate::hir::Ir::none(), &[], x, rig.mcx());
        env.state = Some(p[1]);
        let log = p[2];
        let mut close = |b: &mut FunctionBuilder<'_>, _: &mut Env, img: &ImageWords, h: Value, accs: &[Value]| {
            let (k, w) = (b.load(Type::I64, log, 0), b.iconst(Type::I64, 256));
            let o = b.int_bin(IntBinOp::Mul, k, w);
            let at = b.ptr_add(log, o, 1, 64);
            for (i, v) in img.words.words.iter().chain([&h]).chain(accs).enumerate() {
                b.store(at, 8 * i as i32, *v);
            }
            let one = b.iconst(Type::I64, 1);
            let k = b.int_bin(IntBinOp::Add, k, one);
            b.store(log, 0, k);
        };
        let img = ImageWords::plain(ImageValues { words: p[4..8].to_vec() }, true); // alloc: Program
        let bad = b.bconst(false);
        let (l_join, at) = part.pair_at(&mut b, &env, &[p[9], p[10]], bad, &img, 2);
        let one = b.iconst(Type::I64, 1);
        let m = b.load(Type::I64, log, 24);
        let m = b.int_bin(IntBinOp::Add, m, one);
        b.store(log, 24, m);
        let fr = part.probe(&mut b, &env, &[p[9], p[10]], bad, &img, p[8], Some(at));
        part.settle(&mut b, &mut env, &fr, p[11], &mut close);
        let (n, t) = (b.load(Type::I64, log, 8), b.load(Type::I64, log, 16));
        let n = b.int_bin(IntBinOp::Add, n, one);
        let t = b.int_bin(IntBinOp::Add, t, p[9]);
        b.store(log, 8, n);
        b.store(log, 16, t);
        part.claim(&mut b, &mut env, p[3], &img, p[8], &fr);
        b.br(l_join, &[]);
        b.switch_to(l_join);
        let z = b.iconst(Type::I64, 0);
        b.ret(&[z]);
        for blk in [x.l_finish, x.l_stop_in, x.l_full, x.l_status, x.l_done, x.l_raise, x.l_cancel, x.l_exit] {
            b.switch_to(blk);
            let z = b.iconst(Type::I64, 0);
            b.ret(&[z]);
        }
        (pb.finish(), f)
    }

    /// P2: a pair's rows after its first add in its pair block with no hashed path; a pair whose block another image
    /// holds, and a row whose add overflows, take the hashed path (the overflowed block closes first); an image that is
    /// not a pair (a key past a word) keeps its hash's blocks; the closes plus the row fold carry every row once.
    #[test]
    fn a_pair_adds_in_its_pair_block_with_no_hash() {
        let (prog, f) = pair_rig();
        let (mut st, mut log, mut rec, mut ctx) = (vec![0u64; 2 + 1 + 64 * 8], vec![0u64; 8 + 32 * 16], vec![0u64, 0xabc], vec![0u64; gravity_abi::ctx::ctx_size(1) / 8]); // alloc: Program
        let tag = 1u64 << 56;
        let mut row = |w: [u64; 4], h: u64, t: (i64, i64)| {
            let args = [ctx.as_mut_ptr() as u64, st.as_mut_ptr() as u64, log.as_mut_ptr() as u64, rec.as_mut_ptr() as u64, w[0], w[1], w[2], w[3], h, t.0 as u64, t.1 as u64, rec.as_ptr() as u64];
            gravity_interp::Interp::new(&prog).run(f, &args).expect("runs");
        };
        let af = [65, tag, 70, tag]; // block (65 + 7 * 70) mod 64 = 43 (a one-byte key's share is its byte)
        row(af, 5 << 58, (1, 10)); // hashed path: the row fold, then the pair block 43 claimed
        row(af, 5 << 58, (2, 20));
        row(af, 9 << 58, (3, 30)); // any hash: the pair block
        let long = [65, 9 * tag, 70, tag];
        row(long, 43 << 58, (100, 0)); // not a pair: its hash's blocks 43 (the pair's) and 44: the row fold, 44 claimed
        row(long, 43 << 58, (200, 0));
        let other = [129, tag, 70, tag]; // (129 + 490) mod 64 = 43: its pair block is af's
        row(other, 1 << 58, (7, 0)); // its pair block is af's: the row fold, then its hash's second block 2 claimed
        row(other, 1 << 58, (8, 0)); // the pair test misses, the hashed path adds in block 2
        row(af, 5 << 58, (i64::MAX, 0)); // the pair add overflows: the hashed path closes block 43, then the row fold
        drop(row);
        assert_eq!((log[1], log[3]), (4, 6), "the row fold: af's first, the first long, the first other, the overflow; the hashed path: those, the second long (a hit in 44) and the second other (a hit in 2)");
        let closes: Vec<(u64, i64, i64, u64)> = (0..log[0] as usize).map(|k| { let r = &log[8 + 32 * k..]; (r[0], r[5] as i64, r[8] as i64, r[7]) }).collect(); // alloc: Program
        assert_eq!(closes, vec![(65, 5, 50, 2)], "af's block: rows 2 and 3");
        assert_eq!(st[2], 1 << 2 | 1 << 43 | 1 << 44, "the pair block, the long image's second block and the colliding pair image's hash block claimed");
        let blk = |i: usize| &st[3 + 8 * i..3 + 8 * i + 8];
        assert_eq!((&blk(43)[..4], &blk(44)[..4], blk(44)[5..8].to_vec()), (&af[..], &long[..], vec![1, 200, 0]), "each block's image; 44 holds row 5");
        assert_eq!((&blk(2)[..4], blk(2)[5..8].to_vec()), (&other[..], vec![1, 8, 0]), "the colliding pair image's block holds row 7");
        let folded = log[2] as i64;
        assert_eq!(folded.wrapping_add(5).wrapping_add(200).wrapping_add(8), [1i64, 2, 3, 100, 200, 7, 8, i64::MAX].iter().fold(0i64, |a, &x| a.wrapping_add(x)), "every row once");
    }

    /// The pair index over an image of `keys` keys (its words past `2 keys` a nullable key's flags), under the interpreter
    /// on an empty front (every row misses): `(eligible, block)` for each image of `ws`.
    fn pair_index(keys: usize, ws: &[Vec<u64>]) -> Vec<(bool, u64)> {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let part = Partial { pair: true, wide: u16::MAX, nimg: ws[0].len(), reps: PgVec::new_in(rig.mcx()), ..part(rig.mcx()) };
        let mut pb = ProgramBuilder::new();
        let core = crate::frame::env::declare(&mut pb);
        let ps: Vec<Type> = [Type::Ptr, Type::Ptr].into_iter().chain(ws[0].iter().map(|_| Type::I64)).collect(); // alloc: Program
        let f = pb.declare_func("pidx", &ps, &[Type::I64, Type::I64]);
        let mut b = pb.define(f);
        let p = b.params().to_vec();
        let x = crate::frame::stop::exits(&mut b);
        let mut env = Env::new(&mut b, core, crate::hir::Ir::none(), &[], x, rig.mcx());
        env.state = Some(p[1]);
        let img = ImageWords::plain(ImageValues { words: p[2..].to_vec() }, true); // alloc: Program
        let (bad, v) = (b.bconst(false), b.iconst(Type::I64, 1));
        let (l_join, (elig, i)) = part.pair_at(&mut b, &env, &[v, v], bad, &img, keys);
        let e = b.conv(ConvOp::Zext, Type::I64, elig);
        b.ret(&[e, i]);
        for blk in [l_join, x.l_finish, x.l_stop_in, x.l_full, x.l_status, x.l_done, x.l_raise, x.l_cancel, x.l_exit] {
            b.switch_to(blk);
            let z = b.iconst(Type::I64, 0);
            b.ret(&[z, z]);
        }
        let prog = pb.finish();
        let (mut st, mut ctx) = (vec![0u64; 3 + 64 * 16], vec![0u64; gravity_abi::ctx::ctx_size(1) / 8]); // alloc: Program
        ws.iter().map(|w| {
            let args: Vec<u64> = [ctx.as_mut_ptr() as u64, st.as_mut_ptr() as u64].into_iter().chain(w.iter().copied()).collect(); // alloc: Program
            let got = gravity_interp::Interp::new(&prog).run(f, &args).expect("runs");
            (got[0] == 1, got[1])
        }).collect() // alloc: Program
    }

    /// TH-2: any count of keys of 1 .. 8 bytes places its block at `(c0 + 7 c1 + 7^2 c2 ..) mod 64`, a key's share `c` its
    /// bytes `b0 + 7 b1 + .. + 7^7 b7` (a one-byte key's share is its byte: P2's index); a key past a word (a len past 8,
    /// bytes in its tag word) or NULL / empty (zero words) makes the image ineligible, and a nullable key's flags word after
    /// the keys is no key; any one byte of any key, over 64 consecutive values, reaches every block.
    #[test]
    fn any_count_of_short_keys_places_its_block_by_their_bytes() {
        let tag = 1u64 << 56;
        let one = pair_index(1, &[vec![65, tag], vec![65, 2 * tag], vec![65, 8 * tag], vec![65, 9 * tag], vec![65, tag | 66], vec![0, 0]]);
        assert_eq!(one, vec![(true, 1), (true, 1), (true, 1), (false, 1), (false, 1), (false, 0)], "one key: b0 mod 64, eligible at 1 .. 8 bytes");
        assert_eq!(pair_index(2, &[vec![65, tag, 70, tag]]), vec![(true, 43)], "two keys: (65 + 7 * 70) mod 64, P2's");
        let nullable = pair_index(3, &[vec![65, tag, 70, tag, 2, tag, 0], vec![0, 0, 70, tag, 2, tag, 1]]);
        assert_eq!(nullable, vec![(true, (65 + 7 * 70 + 49 * 2) % 64), (false, (7 * 70 + 49 * 2) % 64)], "a flags word after three keys; a NULL first key");
        let three = pair_index(3, &[vec![65, tag, 70, tag, 2, tag], vec![65, tag, 70, 2 * tag, 2, tag], vec![65, tag, 70, tag, 2, 9 * tag]]);
        assert_eq!(three, vec![(true, 13), (true, 13), (false, 13)]);
        for k in 0..3 {
            let ws: Vec<Vec<u64>> = (0..64u64).map(|v| { let mut w = vec![65, tag, 70, tag, 2, tag]; w[2 * k] = 100 + v; w }).collect(); // alloc: Program
            let blocks: std::collections::BTreeSet<u64> = pair_index(3, &ws).into_iter().map(|(e, i)| { assert!(e); i }).collect(); // alloc: Program
            assert_eq!(blocks.len(), 64, "key {k}'s byte alone reaches every block");
        }
        for j in 0..8 {
            let ws: Vec<Vec<u64>> = (0..64u64).map(|v| vec![0x4141_4141_4141_4141 & !(0xff << (8 * j)) | (100 + v) << (8 * j), 8 * tag]).collect(); // alloc: Program
            let blocks: std::collections::BTreeSet<u64> = pair_index(1, &ws).into_iter().map(|(e, i)| { assert!(e); i }).collect(); // alloc: Program
            assert_eq!(blocks.len(), 64, "an 8-byte key's byte {j} alone reaches every block");
        }
        let g = |s: &[u8]| { let mut w = [0u8; 8]; w[..s.len()].copy_from_slice(s); vec![u64::from_le_bytes(w), (s.len() as u64) << 56] }; // alloc: Program
        assert_eq!(pair_index(1, &[g(b"g-001"), g(b"g-010"), g(b"g-100"), g(b"g-0001")]), vec![(true, 51), (true, 41), (true, 3), (true, 9)], "a byte's position counts");
    }

    #[test]
    fn the_blocks_are_an_eighth_of_the_l1d() {
        let b = blocks();
        assert!(b.is_power_of_two() && (2..=64).contains(&b), "{b}");
        if gravity_driver::machine::profile().l1d_bytes == 64 << 10 {
            assert_eq!(b, 64, "Graviton 4's 64 KiB L1d: an eighth in blocks of 16 words");
        }
    }

    /// A-24c: a min / max partial keeps the least / greatest term from its identity, a count(x) partial adds the row's 0 / 1;
    /// a close builds `[P, n]` and `[P]` and leaves each partial its identity.
    #[test]
    fn min_max_and_count_x_partials_close_their_registers() {
        let cx = mcx::MemoryContext::new("num.partial");
        let m = cx.mcx();
        let mut r = Rig::with(Partial {
            terms: mcx::pg_vec![in m; Term::Bare { c: 1, scale: 2 }, Term::Present(2)],
            parts: mcx::pg_vec![in m; (0, Op::Sum), (0, Op::Min), (0, Op::Max), (1, Op::Sum)],
            aggs: mcx::pg_vec![in m; (Form::Num { carry: false }, 0), (Form::Ext, 1), (Form::Ext, 2), (Form::Present, 3), (Form::Count, 0)],
            ..part(m)
        });
        r.row(9, 63, (5, 1), false, true); // the row fold, then block 63 claimed
        r.row(9, 63, (-3, 0), false, true);
        r.row(9, 63, (7, 1), false, true);
        r.call(3, &[]); // the finish
        let want = [9, 63 << 58 | 9, 4, 0, 2, -3i64 as u64, 2, 7, 2, 1, 2, 0xabc, 2];
        assert_eq!(&r.log[8..8 + 13], &want[..]);
        let blk = &r.st[3 + 8 * 63..3 + 8 * 64];
        assert_eq!(&blk[3..], &[0, 0, i64::MAX as u64, i64::MIN as u64, 0], "every partial its identity");
    }

    /// A close builds each aggregate's registers: `[P, P >> 63, n]`, `[P, s, s, s, n]`, `[n]`, `[P, P >> 63, 0, n]`, then the
    /// representative and the end's count.
    #[test]
    fn a_close_builds_the_class_registers() {
        let mut r = Rig::new();
        r.row(9, 63, (0, 0), false, true);
        r.row(9, 63, (i64::MAX, i64::MIN), false, true);
        r.call(3, &[]);
        let m = u64::MAX;
        let want = [9, 63 << 58 | 9, i64::MAX as u64, 0, 1, i64::MIN as u64, m, m, m, 1, 1, i64::MIN as u64, m, 0, 1, 0xabc, 1];
        assert_eq!(&r.log[8..8 + 17], &want[..]);
    }
}
