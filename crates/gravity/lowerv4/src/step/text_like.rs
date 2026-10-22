//! Road `text.memmem` (H09, R13-L06) + M2's verbatim text functions (I24): a LIKE / NOT LIKE whose constant pattern is
//! exact, prefix, suffix, contains or `%` (no `_`, PG's `\` escape) under a deterministic collation, and `length` /
//! `char_length` of ASCII text, over an inline image read in place. Each claimed call becomes `CASE WHEN hot THEN fast
//! ELSE call END`: a NULL or plain image is hot; a toasted / compressed one, or non-ASCII text for `length`, takes PG's
//! call cold, so values, NULLs and errors stay PG's. Other patterns and non-UTF-8 databases stay gravity_expr's. ILIKE
//! and `lower(col) LIKE` lower the value byte-wise first where PG's `lower` is `asc_tolower` (PS4-03 / PS4-06: `Fold`).
//! strpos / position / starts_with / `^@` over a constant are their LIKE twin (gravity_expr percode `text_search`, E2).
//! A LIKE under the root's AND / OR / NOT of a step's expression takes the word form: its pattern (a constant or a
//! generic plan's `$n`) is an operand, never read at plan time (V4R-18: one program for every pattern); once per
//! pipeline call `entry` hands the pattern's word to host `like_word` (kernels dict/strategies/like_word.rs), whose class
//! word and literal go to frame words the row test reads, and a cold class (any other pattern, a NULL one) takes PG's
//! call. Budget 225 lines (V4R-523). Owner M2-FRAME.
use std::any::Any;
use std::cell::Cell;
use std::rc::Rc;

use gravity_decompose::catalog::typefacts;
use gravity_expr::nodes::{case::Case, var::Var};
use gravity_expr::{Compiled, ExprNode, Kids, NodeId, Source as VarSource};
use gravity_fn::typesem::expr::{text_like, TEXT_LIKE_TYPES};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_program::cost::expr::{MAX_FOLD, MAX_LIT};
use gravity_kernels::dict::strategies::like_word::{self as lw, LikeWord, WORD_LIT_BYTES};
use mcx::{CollectIn, Mcx, PgVec, GrowIn};
use types_core::catalog::NAMEOID;
use types_core::fmgr::NAMEDATALEN;
use types_nodes::primnodes::ParamKind;
use types_nodes::Node;

use crate::face::kinds::Face;
use crate::face::step::{Claim as ExtClaim, ExprExtImpl, ExprExtMech, ExtPlan, SubEmit, EXT_AT};
use crate::frame::env::Env;
use crate::frame::px::{ExId, Px, WordKey};
use crate::frame::row::RowVals;
use crate::admit::refusal::Reason;
use crate::step::text_case::memeq;

crate::strategies! {
    /// Verbatim LIKE / prefix open-coded.
    TEXT_MEMMEM = 58, "text.memmem", HeapLike;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(TEXT_MEMMEM, &ExprExtImpl(&TEXT))], ..crate::ops::NONE };

const PG_UTF8: i32 = 6;
/// An empty 4-byte-header image: a NULL or cold row's reads land here, and read nothing past it. D627: the IR writes it
/// into a frame slot (`frame_bytes`); a static's address would bake the process's layout into the program.
const EMPTY: [u8; 8] = [16, 0, 0, 0, 0, 0, 0, 0];
/// A word claim's frame words: its class word, then its literal's.
const WORDS: usize = 1 + WORD_LIT_BYTES / 8;

pub(crate) type Col = (VarSource, i16, u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Test { Exact, Prefix, Suffix, Contains, Any, Len, Upper, UpperAscii, Lower, LowerAscii, Word }

/// PS4-03 / PS4-06: how a claim's value is lowered before its test, as PG's `lower` lowers it (step/text_case.rs
/// `fold_mode`): not at all, every value byte-wise (`asc_tolower`, a `ctype_is_c` collation), or an ASCII value byte-wise
/// (a non-ASCII one takes PG's call).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Fold { #[default] No, All, Ascii }

/// PS4-07: a coercion between the column and its LIKE, as PG cuts the value: none, or its first 63 bytes clipped at a
/// character boundary (`::name`, pg_mbcliplen). A `::varchar(n)` / `::char(n)` cut would read its typmod, a by-value
/// word neither program key holds (front_bake_lint IR_READS): PG's call answers those.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Cut { #[default] No, Name }

/// A claim's fold: its value's (`lower(col) LIKE`, ILIKE), and for ILIKE its pattern's too (PG lowers both sides); the
/// value's cut (PS4-07), before the fold.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Folding { pub row: Fold, pub pattern: bool, pub cut: Cut }

/// A word claim's pattern (`Test::Word`): its operand (`sub`; a `textcat` pattern's `parts` operands from `sub` on, left
/// to right) and the byte offsets of its class word and its literal's first word.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct WordSite { pub sub: u32, pub parts: u32, pub class: i32, pub lit: i32 }

/// One claimed call: its column, test and literal, NOT LIKE; a word claim's pattern; its fold.
pub(crate) struct Claim { pub col: Col, pub test: Test, pub lit: Box<[u8]>, pub neg: bool, pub word: WordSite, pub fold: Folding }

/// An extension's plan: its claims (claim `i` is sites `2i` hot and `2i + 1` value) and the value its hot site computed.
pub(crate) struct Plan<C> { pub claims: Vec<C>, held: Cell<Option<[Value; 2]>> }

/// PG's LIKE (escape `\`) as an inline class and its literal; `None` for a `_`, an inner `%`, a trailing escape.
pub(crate) fn classify(pat: &[u8], mcx: Mcx<'_>) -> Option<(Test, Vec<u8>)> {
    let mut items: PgVec<'_, Option<u8>> = PgVec::new_in(mcx);
    let mut it = pat.iter();
    while let Some(&c) = it.next() {
        items.push_in(match c { b'\\' => Some(*it.next()?), b'%' => None, b'_' => return None, c => Some(c) });
    }
    let lead = items.iter().take_while(|i| i.is_none()).count();
    let trail = items[lead..].iter().rev().take_while(|i| i.is_none()).count();
    let lit = items[lead..items.len() - trail].iter().copied().collect::<Option<Vec<u8>>>()?;
    let any = lit.is_empty() && lead + trail > 0;
    Some((if any { Test::Any } else { [Test::Exact, Test::Prefix, Test::Suffix, Test::Contains][2 * (lead > 0) as usize + (trail > 0) as usize] }, lit))
}

/// A non-NULL varlena constant's bytes when its image is inline (a 1-byte or plain 4-byte header).
pub(crate) fn const_bytes<'a>(c: &Compiled<'a>, id: NodeId) -> Option<&'a [u8]> {
    let ExprNode::Const(k) = &c.prog.nodes[id.0 as usize] else { return None };
    crate::step::raw::const_inline_bytes(k)
}

/// The column `id` reads (through a RelabelType) when its type is one of `types`.
pub(crate) fn column(c: &Compiled<'_>, id: NodeId, types: &[u32]) -> Option<Col> {
    column_of(c, id, types, &[])
}

/// `column`, a domain's column too when its base type (`bases`: domain, base) is one of `types`: a domain stores its
/// base's bytes, so the column is read as its base.
fn column_of(c: &Compiled<'_>, id: NodeId, types: &[u32], bases: &[(u32, u32)]) -> Option<Col> {
    crate::stack_check();
    match &c.prog.nodes[id.0 as usize] {
        ExprNode::Var(v) if v.attno > 0 && v.attno < EXT_AT => {
            let t = if types.contains(&v.typ) { v.typ } else { bases.iter().find(|d| d.0 == v.typ).map(|d| d.1).filter(|t| types.contains(t))? };
            Some((v.src, v.attno, t))
        }
        ExprNode::Relabel(r) => column_of(c, r.arg, types, bases),
        _ => None,
    }
}

/// The domains over a LIKE column type among `c`'s columns, with their base types (the catalog's `getBaseType`).
pub(crate) fn domain_bases<'g>(px: &Px<'g, '_>, c: &Compiled<'_>) -> PgVec<'g, (u32, u32)> {
    let like_col = |t: u32| TEXT_LIKE_TYPES.contains(&t) || t == NAMEOID;
    let mut out: PgVec<'g, (u32, u32)> = PgVec::new_in(px.mcx());
    for n in &c.prog.nodes {
        let ExprNode::Var(v) = n else { continue };
        if like_col(v.typ) || typefacts::row_of(v.typ).is_some() || out.iter().any(|d| d.0 == v.typ) {
            continue;
        }
        if let Some(b) = px.gx.catalog.base_type(v.typ, &mut -1).ok().filter(|&b| b != v.typ && like_col(b)) {
            out.push_in((v.typ, b));
        }
    }
    out
}

/// `namelike` / `namenlike` (`name ~~ text`, PG's `NameStr` matched as `textlike` matches): NOT, by catalog name.
fn name_like(funcid: u32) -> Option<bool> {
    match gravity_fn::row_of(funcid)?.name {
        "namelike" => Some(false),
        "namenlike" => Some(true),
        _ => None,
    }
}

/// PS4-06: ILIKE / NOT ILIKE by the catalog body: `texticlike` / `texticnlike` (gravity_expr `like_body`: text, and
/// bpchar over its padded value, `bpchariclike`'s body being `texticlike`) and `nameiclike` / `nameicnlike` (by name, as
/// `name_like`): NOT, the column types.
fn ilike_of(funcid: u32) -> Option<(bool, &'static [u32])> {
    if let Some((true, neg)) = gravity_expr::percode::like_body(funcid) {
        return Some((neg, &TEXT_LIKE_TYPES[..]));
    }
    match gravity_fn::row_of(funcid)?.name {
        "nameiclike" => Some((false, &[NAMEOID][..])),
        "nameicnlike" => Some((true, &[NAMEOID][..])),
        _ => None,
    }
}

/// A LIKE call `f`: NOT, the column types it reads in place, and ILIKE.
fn like_of(f: u32) -> Option<(bool, &'static [u32], bool)> {
    let like = text_like(f).map(|neg| (neg, &TEXT_LIKE_TYPES[..])).or_else(|| name_like(f).map(|neg| (neg, &[NAMEOID][..])));
    like.map(|(neg, t)| (neg, t, false)).or_else(|| ilike_of(f).map(|(neg, t)| (neg, t, true)))
}

/// What a LIKE of collation `coll` reads at `id`, and how it folds: a column read in place, or `lower` of a text column
/// (PS4-03, by `lower`'s collation); an ILIKE (PS4-06) reads a column, both sides folded by its own collation, or `lower`
/// / `upper` of one (its fold then ASCII values only unless both collations are `ctype_is_c`: ILIKE lowers what they
/// map, so only their ASCII map need agree). `None` where PG's `lower` / `upper` is not byte-wise `asc_tolower` /
/// `asc_toupper` (a Turkish locale's `I`).
fn subject(c: &Compiled<'_>, id: NodeId, types: &[u32], bases: &[(u32, u32)], coll: u32, ilike: bool, mcx: Mcx<'_>) -> Option<(Col, Folding)> {
    if let Some(col) = column_of(c, id, types, bases) {
        let row = if ilike { crate::step::text_case::fold_mode(coll, mcx)? } else { Fold::No };
        return Some((col, Folding { row, pattern: ilike, cut: Cut::No }));
    }
    let id = match &c.prog.nodes[id.0 as usize] { ExprNode::Relabel(r) => r.arg, _ => id };
    let ExprNode::Func(n) = &c.prog.nodes[moved_call(c, id).0 as usize] else { return None };
    let call = &c.prog.sites[n.site.0 as usize];
    if let Some((arg, cut)) = cut_of(c, call.funcid, call.args) {
        let col = (!ilike).then(|| column_of(c, arg, &[25, 1043], bases)).flatten()?;
        return Some((col, Folding { cut, ..Folding::default() }));
    }
    let upper = call.funcid == crate::step::text_case::F_UPPER;
    if !(call.funcid == crate::step::text_case::F_LOWER || ilike && upper) || call.args.len != 1 {
        return None;
    }
    let col = column_of(c, c.prog.kids[call.args.start as usize], &[25, 1043], bases)?;
    let inner = if upper { crate::step::text_case::upper_mode(call.collation, mcx)? } else { crate::step::text_case::fold_mode(call.collation, mcx)? };
    if !ilike {
        return Some((col, Folding { row: inner, pattern: false, cut: Cut::No }));
    }
    // PS4-06 upper(col) / lower(col) ILIKE: ILIKE lowers the mapped value under its own collation
    let outer = crate::step::text_case::fold_mode(coll, mcx)?;
    let row = if (inner, outer) == (Fold::All, Fold::All) { Fold::All } else { Fold::Ascii };
    Some((col, Folding { row, pattern: true, cut: Cut::No }))
}

/// A call this pass already claimed (`claim_calls` moved it under a CASE whose two WHENs read its claim's sites, past
/// every attribute number): the moved call, else `id`. PS4-06: `upper(col) ILIKE` reads the `upper` call claimed before it
/// (the CASE stays the ILIKE's cold arm's argument).
fn moved_call(c: &Compiled<'_>, id: NodeId) -> NodeId {
    let ExprNode::Case(k) = &c.prog.nodes[id.0 as usize] else { return id };
    let site = |i: u32| matches!(c.prog.nodes[c.prog.kids[(k.whens.start + i) as usize].0 as usize], ExprNode::Var(v) if v.src == VarSource::Scan && crate::face::ext::ext_site(v.attno));
    match k.default {
        Some(d) if k.arg.is_none() && k.whens.len == 2 && site(0) && site(1) => d,
        _ => id,
    }
}

/// PS4-07: call `funcid` over `args` as a coercion to `name` by its catalog name (`text_name`, one argument, no typmod
/// read); with its argument.
fn cut_of(c: &Compiled<'_>, funcid: u32, args: Kids) -> Option<(NodeId, Cut)> {
    (gravity_fn::row_of(funcid)?.name == "text_name" && args.len == 1).then(|| (c.prog.kids[args.start as usize], Cut::Name))
}

/// A constant ILIKE pattern as PG lowers it under fold `f` (`asc_tolower`): `None` for a non-ASCII one under `Fold::Ascii`.
fn fold_const<'g>(pat: &[u8], f: Folding, mcx: Mcx<'g>) -> Option<PgVec<'g, u8>> {
    match (f.pattern, f.row) {
        (false, _) => Some(mcx::to_vec_in(pat, mcx)),
        (true, Fold::Ascii) if !pat.is_ascii() => None,
        (true, _) => {
            let mut low = mcx::to_vec_in(pat, mcx);
            low.make_ascii_lowercase();
            Some(low)
        }
    }
}

/// Collation `coll` (a LIKE's: never 0) compares by bytes: typesem's collation-bytes answer.
pub(crate) fn deterministic(coll: u32) -> bool {
    coll != 0 && gravity_fn::typesem::claims_coll_bytes(coll)
}

/// Claims each call `claim` accepts (not in an island): `CASE WHEN Var(EXT_AT + 2n) THEN Var(.. + 1) ELSE <call> END`.
pub(crate) fn claim_calls<C: 'static>(c: &mut Compiled<'_>, mut claim: impl FnMut(&Compiled<'_>, usize) -> Option<(C, u32)>) -> Option<ExtClaim> {
    let mut claims = Vec::new(); // alloc: Program
    for i in 0..c.prog.nodes.len() {
        let island = c.prog.islands.iter().any(|s| (s.first.0 as usize..=s.root.0 as usize).contains(&i));
        let Some((cl, typ)) = (!island && claims.len() < 64).then(|| claim(c, i)).flatten() else { continue };
        let (site, base) = (EXT_AT + 2 * claims.len() as i16, c.prog.nodes.len() as u32);
        let p = &mut c.prog;
        let case = Case { arg: None, whens: Kids { start: p.kids.len() as u32, len: 2 }, default: Some(NodeId(base)) };
        let var = |attno, typ| ExprNode::Var(Var { src: VarSource::Scan, attno, typ, desc: 0 });
        let moved = core::mem::replace(&mut p.nodes[i], ExprNode::Case(case));
        p.nodes.extend_in([moved, var(site, 16), var(site + 1, typ)]);
        p.kids.extend_in([NodeId(base + 1), NodeId(base + 2)]);
        claims.push(cl);
    }
    // the call keeps its literals (the CASE's cold arm reads them): the claim skips no plan node
    (!claims.is_empty()).then(|| ExtClaim { plan: Rc::new(Plan { claims, held: Cell::new(None) }) as Rc<dyn Any>, skip: Vec::new() }) // alloc: Program
}

/// Site `site` of a claim: the hot site computes `[hot, value, valid]` with `f` and holds the value for the next one.
pub(crate) fn site<C: 'static>(b: &mut FunctionBuilder<'_>, env: &mut Env, ex: ExId, site: u32, f: impl FnOnce(&mut FunctionBuilder<'_>, &mut Env, &C) -> [Value; 3]) -> [Value; 3] {
    let bug = |env: &Env, payload| -> ! { crate::refuse!(env.node, Reason::Bug { payload }) };
    let p = ExtPlan::of(env.placed(ex)).and_then(|x| x.plan.downcast_ref::<Plan<C>>()).unwrap_or_else(|| bug(env, "an extension site without its plan"));
    let (z, t) = (b.iconst(Type::I64, 0), b.bconst(true));
    if site % 2 == 1 {
        let [v, ok] = p.held.take().unwrap_or_else(|| bug(env, "a claim's value before its hot site"));
        return [v, ok, z];
    }
    // a value's loops are bounded by its length, inside the window's checked row loop (PG checks no interrupt either)
    let n0 = b.func().hints.loop_headers.len();
    let [hot, v, ok] = f(b, env, &p.claims[site as usize / 2]);
    env.bounded.extend_from_slice_in(&b.func().hints.loop_headers[n0..]);
    p.held.set(Some([v, ok]));
    [word(b, hot), t, z]
}

/// A Bool as a datum word.
pub(crate) fn word(b: &mut FunctionBuilder<'_>, v: Value) -> Value { let (one, zero) = (b.iconst(Type::I64, 1), b.iconst(Type::I64, 0)); b.select(v, one, zero) }

/// Column `col`'s image read in place: `[ptr, len, hot, null]` (`inline`).
pub(crate) fn view(b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, col: Col) -> [Value; 4] {
    // a column the row does not carry is cold: the call is gravity_expr's (a claim under a dropped subtree reads nothing)
    let Some(cr) = (if col.0 == VarSource::Inner { row.inner_col(env, col.1) } else { row.col(col.1) }) else {
        let (t, f) = (b.bconst(true), b.bconst(false));
        return { let [p, len, _] = inline(b, env.zero, t); [p, len, f, t] };
    };
    let name = col.2 == NAMEOID;
    let tf = typefacts::row_of(col.2).copied().unwrap_or(typefacts::opaque(col.2, col.2, if name { NAMEDATALEN as i16 } else { -1 }, false, false));
    let mut cx = env.lx(b, env.node);
    let (d, null) = crate::rep::datum(&mut cx, cr, &tf);
    env.memo = cx.memo;
    let [p, len, hot] = if name { cstring(b, d, null) } else { inline(b, d, null) };
    [p, len, hot, null]
}

/// A `name` datum's string `[ptr, len, hot]`: its bytes before the first NUL (`NameStr`, `strlen`; namein zero-fills
/// past it, and the last of the NAMEDATALEN bytes is always NUL). Never toasted, so always hot; a NULL reads nothing.
fn cstring(b: &mut FunctionBuilder<'_>, d: Value, null: Value) -> [Value; 3] {
    let empty = frame_bytes(b, &EMPTY);
    let [zero, one, four, last] = [0, 1, 4, NAMEDATALEN as i64 - 1].map(|k| b.iconst(Type::I64, k));
    let none = b.int_bin(IntBinOp::Add, empty, four);
    let d = b.select(null, none, d);
    let p = b.conv(ConvOp::IntToPtr, Type::Ptr, d);
    let (head, body, out) = (b.create_block(&[Type::I64]), b.create_block(&[]), b.create_block(&[Type::I64]));
    b.br(head, &[zero]);
    (b.switch_to(head), b.mark_loop_header(head));
    let i = b.block_params(head)[0];
    let more = b.icmp(IntCond::Slt, i, last);
    b.cond_br(more, (body, &[]), (out, &[i]));
    b.switch_to(body);
    let (q, next) = (b.ptr_add(p, i, 1, 0), b.int_bin(IntBinOp::Add, i, one));
    let ch = b.load_zext(Type::I8, q, 0);
    let nul = b.icmp(IntCond::Eq, ch, zero);
    b.cond_br(nul, (out, &[i]), (head, &[next]));
    b.switch_to(out);
    [p, b.block_params(out)[0], b.bconst(true)]
}

/// Datum `d`'s inline payload `[ptr, len, hot]`; a NULL is hot, a NULL or cold image reads `EMPTY`'s (length 0).
pub(crate) fn inline(b: &mut FunctionBuilder<'_>, d: Value, null: Value) -> [Value; 3] {
    let empty = frame_bytes(b, &EMPTY);
    let [zero, one, two, three, four] = [0, 1, 2, 3, 4].map(|k| b.iconst(Type::I64, k));
    let bin = |b: &mut FunctionBuilder<'_>, op, x, y| b.int_bin(op, x, y);
    let d = b.select(null, empty, d);
    let h = { let p = b.conv(ConvOp::IntToPtr, Type::Ptr, d); b.load_zext(Type::I8, p, 0) };
    let (h1, h3) = (bin(b, IntBinOp::And, h, one), bin(b, IntBinOp::And, h, three));
    let (short, not_ext, plain) = (b.icmp(IntCond::Ne, h1, zero), b.icmp(IntCond::Ne, h, one), b.icmp(IntCond::Eq, h3, zero));
    let short_in = bin(b, IntBinOp::And, short, not_ext);
    let ok = bin(b, IntBinOp::Or, short_in, plain);
    // a short image's 4-byte read goes to `EMPTY`: the image may be one byte long
    let w = { let d4 = b.select(short, empty, d); let p4 = b.conv(ConvOp::IntToPtr, Type::Ptr, d4); b.load_zext_unaligned(Type::I32, p4, 0) };
    let (w, n1) = (bin(b, IntBinOp::Ushr, w, two), bin(b, IntBinOp::Ushr, h, one));
    let (n4, n1) = (bin(b, IntBinOp::Sub, w, four), bin(b, IntBinOp::Sub, n1, one));
    let (n, hdr) = (b.select(short, n1, n4), b.select(short, one, four));
    let len = b.select(ok, n, zero);
    let (at, none) = (bin(b, IntBinOp::Add, d, hdr), bin(b, IntBinOp::Add, empty, four));
    let at = b.select(ok, at, none);
    [b.conv(ConvOp::IntToPtr, Type::Ptr, at), len, ok]
}

/// D627: `bytes` in a stack slot of the function's frame, written where this is emitted: their address as an i64. Never a
/// static's address (under PIE + ASLR it differs per process, so the IR would too); the slot lives for the call, which is
/// as long as any reader here holds it (the reads compare it or hand it to a call that returns first).
pub(crate) fn frame_bytes(b: &mut FunctionBuilder<'_>, bytes: &[u8]) -> Value {
    let slot = b.declare_stack_slot(bytes.len().next_multiple_of(8) as u32, 8);
    let p = b.stack_addr(slot);
    for (i, w) in bytes.chunks(8).enumerate() {
        let mut x = [0u8; 8];
        x[..w.len()].copy_from_slice(w);
        let v = b.const_raw(Type::I64, u64::from_le_bytes(x));
        b.store(p, 8 * i as i32, v);
    }
    b.conv(ConvOp::PtrToInt, Type::I64, p)
}

/// `lit` occurs at an offset in `[first, last]` from `p`: its two rarest bytes first, then all of it.
fn find(b: &mut FunctionBuilder<'_>, p: Value, first: Value, last: Value, lit: &[u8]) -> Value {
    let (o1, o2) = gravity_dict::substr::RarePair::new(lit).map_or((0, 0), |r| r.offsets());
    let (one, t, f) = (b.iconst(Type::I64, 1), b.bconst(true), b.bconst(false));
    let (head, body, cmp, out) = (b.create_block(&[Type::I64]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::Bool]));
    b.br(head, &[first]);
    (b.switch_to(head), b.mark_loop_header(head));
    let i = b.block_params(head)[0];
    let more = b.icmp(IntCond::Sle, i, last);
    b.cond_br(more, (body, &[]), (out, &[f]));
    b.switch_to(body);
    let (q, next) = (b.ptr_add(p, i, 1, 0), b.int_bin(IntBinOp::Add, i, one));
    let mut hit = t;
    for o in [o1, o2].into_iter().filter(|&o| o < lit.len()) {
        let (v, want) = (b.load_zext(Type::I8, q, o as i32), b.iconst(Type::I64, lit[o] as i64));
        let e = b.icmp(IntCond::Eq, v, want);
        hit = b.int_bin(IntBinOp::And, hit, e);
    }
    b.cond_br(hit, (cmp, &[]), (head, &[next]));
    b.switch_to(cmp);
    let e = memeq(b, q, lit);
    b.cond_br(e, (out, &[t]), (head, &[next]));
    b.switch_to(out);
    b.block_params(out)[0]
}

/// The part of literal `lit` a claim compares inline: all of it up to `MAX_LIT` bytes (the unrolled compare's bound),
/// else its first `MAX_LIT` bytes (a suffix's last), whose occurrence only says the whole literal may match there.
fn window(lit: &[u8], test: Test) -> &[u8] {
    match (lit.len() > MAX_LIT, test) {
        (false, _) => lit,
        (true, Test::Suffix) => &lit[lit.len() - MAX_LIT..],
        (true, _) => &lit[..MAX_LIT],
    }
}

/// Claim `c` over `[p, p + len)`: `(hot, value word)`; a class is where the literal may start (suffix `len - n` ...).
/// A literal past `MAX_LIT` is searched by its `window` where the whole literal fits: no occurrence answers here (LIKE
/// false, NOT LIKE true), an occurrence is not hot and PG's call answers in the cold arm.
pub(crate) fn test(b: &mut FunctionBuilder<'_>, p: Value, len: Value, c: &Claim) -> (Value, Value) {
    let win = window(&c.lit, c.test);
    let [zero, none, n, w] = [0, -1, c.lit.len() as i64, win.len() as i64].map(|k| b.iconst(Type::I64, k));
    let t = b.bconst(true);
    if c.test == Test::Len {
        return (crate::step::text_case::ascii(b, p, len), len);
    }
    let fits = b.icmp(if c.test == Test::Exact { IntCond::Eq } else { IntCond::Sge }, len, n);
    let tail = b.int_bin(IntBinOp::Sub, len, n);
    let (first, last) = match c.test {
        Test::Suffix => {
            let at = b.int_bin(IntBinOp::Sub, len, w);
            (b.select(fits, at, zero), b.select(fits, at, none))
        }
        Test::Contains => (zero, tail),
        _ => (zero, b.select(fits, zero, none)),
    };
    let v = if c.test == Test::Any { t } else { find(b, p, first, last, win) };
    if win.len() < c.lit.len() {
        let (missed, answer) = (b.int_bin(IntBinOp::Xor, v, t), b.bconst(c.neg));
        return (missed, word(b, answer));
    }
    let v = if c.neg { b.int_bin(IntBinOp::Xor, v, t) } else { v };
    (t, word(b, v))
}

/// Node `i` of `c` as a claim and its result type: an inline LIKE class, or `length` of text / varchar; a column of a
/// domain in `bases` as its base.
fn claim(c: &Compiled<'_>, i: usize, bases: &[(u32, u32)], mcx: Mcx<'_>) -> Option<(Claim, u32)> {
    if let Some(t) = gravity_expr::percode::text_search(&c.prog, NodeId(i as u32), mcx) {
        let (test, lit) = classify(&t.pattern, mcx)?; // E2: strpos / position / starts_with / ^@ over a constant, as its LIKE twin
        fires::fire("text.memmem"); // shapecheck fire signal (REGISTRY STEP-08 / B2-23 / OIDW-17,60..63)
        return Some((Claim { col: column_of(c, t.arg, &[25, 1043], bases)?, test, lit: lit.into(), neg: t.negated, word: WordSite::default(), fold: Folding::default() }, 16));
    }
    let (ExprNode::Op(n) | ExprNode::Func(n)) = &c.prog.nodes[i] else { return None };
    let call = &c.prog.sites[n.site.0 as usize];
    let (f, arity) = (call.funcid, call.args.len);
    // a character count by its catalog body (`textlen`: length / char_length / character_length), not an oid list
    let chars = gravity_expr::percode::text_length(f) == Some(gravity_expr::percode::TextLength::Chars);
    let like = like_of(f);
    let case = f == crate::step::text_case::F_UPPER || f == crate::step::text_case::F_LOWER && !feeds_like(c, i);
    let one = chars || case;
    if !(one && arity == 1 || like.is_some() && arity == 2) {
        return None;
    }
    let arg = |k: u32| c.prog.kids[(call.args.start + k) as usize];
    if let Some((neg, types, ilike)) = like {
        if !deterministic(call.collation) {
            return None;
        }
        let (col, fold) = subject(c, arg(0), types, bases, call.collation, ilike, mcx)?;
        let (test, lit) = classify(&fold_const(const_bytes(c, arg(1))?, fold, mcx)?, mcx)?;
        fires::fire("text.memmem"); // shapecheck fire signal (REGISTRY STEP-08 / B2-23 / OIDW-17,60..63)
        return Some((Claim { col, test, lit: lit.into(), neg, word: WordSite::default(), fold }, 16));
    }
    let col = column_of(c, arg(0), &TEXT_LIKE_TYPES, bases)?;
    if chars && col.2 != 1042 {
        return Some((Claim { col, test: Test::Len, lit: Box::default(), neg: false, word: WordSite::default(), fold: Folding::default() }, 23));
    }
    if case && col.2 != 1042 {
        return Some((Claim { col, test: crate::step::text_case::mode(call.collation, f, mcx)?, lit: Box::default(), neg: false, word: WordSite::default(), fold: Folding::default() }, 25));
    }
    None
}

/// Whether node `i` is the subject of a LIKE-family call: PS4-03's fold reads `lower(col)` there itself (`subject`), so
/// the `lower` stays the call's (D2858 claims every other).
fn feeds_like(c: &Compiled<'_>, i: usize) -> bool {
    c.prog.nodes.iter().any(|n| match n {
        ExprNode::Op(n) | ExprNode::Func(n) => {
            let call = &c.prog.sites[n.site.0 as usize];
            like_of(call.funcid).is_some() && call.args.len > 0 && c.prog.kids[call.args.start as usize].0 as usize == i
        }
        _ => false,
    })
}

/// Node `id` of `c` in the word form's shape: LIKE / NOT LIKE of a text column (a domain's in `bases`, `domain_bases`, as
/// its base) and a by-reference constant under a byte collation: the column and NOT. The constant is not read (no bake).
pub(crate) fn word_shape(c: &Compiled<'_>, id: NodeId, bases: &[(u32, u32)], mcx: Mcx<'_>) -> Option<(Col, bool)> {
    let (ExprNode::Op(n) | ExprNode::Func(n)) = &c.prog.nodes[id.0 as usize] else { return None };
    text_like(c.prog.sites[n.site.0 as usize].funcid)?;
    word_shape_of(c, id, bases, mcx).filter(|w| w.2 == Folding::default()).map(|w| (w.0, w.1))
}

/// `word_shape` over `claim`'s own LIKE forms: a `name` column's too, a domain's in `bases` as its base, `lower` of a
/// column and ILIKE with their fold.
fn word_shape_of(c: &Compiled<'_>, id: NodeId, bases: &[(u32, u32)], mcx: Mcx<'_>) -> Option<(Col, bool, Folding)> {
    let (ExprNode::Op(n) | ExprNode::Func(n)) = &c.prog.nodes[id.0 as usize] else { return None };
    let call = &c.prog.sites[n.site.0 as usize];
    let (neg, types, ilike) = like_of(call.funcid)?;
    if call.args.len != 2 || !deterministic(call.collation) {
        return None;
    }
    let arg = |k: u32| c.prog.kids[(call.args.start + k) as usize];
    if !bound_pattern(c, arg(1), mcx) {
        return None;
    }
    let (col, fold) = subject(c, arg(0), types, bases, call.collation, ilike, mcx)?;
    Some((col, neg, fold))
}

/// PS4-05: a pattern fixed before the rows run, a by-reference constant or a generic plan's `$n` (through a RelabelType),
/// both one bind word per execution. Never a PARAM_EXEC: an InitPlan sets it after the entry. A `$n` is a varlena by its
/// type's storage, as the constant is: PG passes the pattern's bytes to the call as they are, whatever the type.
fn bound_pattern(c: &Compiled<'_>, id: NodeId, mcx: Mcx<'_>) -> bool {
    crate::stack_check();
    match &c.prog.nodes[id.0 as usize] {
        ExprNode::Const(k) => !k.byval && k.typlen == -1,
        ExprNode::Param(p) => p.kind == ParamKind::PARAM_EXTERN && typefacts::row_of(p.typ).is_some_and(|t| !t.byval && t.typlen == -1),
        ExprNode::Relabel(r) => matches!(c.prog.nodes[r.arg.0 as usize], ExprNode::Param(_)) && bound_pattern(c, r.arg, mcx),
        ExprNode::Op(_) | ExprNode::Func(_) => cat_parts(c, id, mcx).len() > 1,
        _ => false,
    }
}

/// PS4-05 (`'%' || $1 || '%'`): the parts of a pattern `textcat` builds from bound parts (`bound_pattern`'s constants and
/// `$n`), left to right; empty when it is no such call. PG concatenates them per row, the entry once per execution
/// (`cat_pattern`), each part read as it is.
fn cat_parts<'g>(c: &Compiled<'_>, id: NodeId, mcx: Mcx<'g>) -> PgVec<'g, NodeId> {
    let (mut todo, mut out) = (mcx::pg_vec![in mcx; id], PgVec::new_in(mcx));
    while let Some(n) = todo.pop() {
        match &c.prog.nodes[n.0 as usize] {
            ExprNode::Op(f) | ExprNode::Func(f) => {
                let call = &c.prog.sites[f.site.0 as usize];
                if call.args.len != 2 || gravity_fn::row_of(call.funcid).map(|r| r.name) != Some("textcat") {
                    return PgVec::new_in(mcx);
                }
                // the right part first: the left one pops first
                todo.extend_in([c.prog.kids[(call.args.start + 1) as usize], c.prog.kids[call.args.start as usize]]);
            }
            _ if bound_pattern(c, n, mcx) => out.push_in(n),
            _ => return PgVec::new_in(mcx),
        }
    }
    out
}

/// The pattern of plan node `n`, a call of `funcid`: its second argument (through a RelabelType) when a constant, an
/// extern parameter or a `textcat` of them (`plan_parts`).
fn plan_pattern<'a>(n: Node<'a>, funcid: u32, mcx: Mcx<'_>) -> Option<Node<'a>> {
    let args = n.as_op_expr().filter(|o| o.opfuncid == funcid).map(|o| &o.args).or_else(|| n.as_func_expr().filter(|f| f.funcid == funcid).map(|f| &f.args))?;
    let a = *args.as_slice().get(1)?;
    let a = a.as_relabel_type().map_or(a, |r| r.arg);
    plan_parts(a, mcx).is_some().then_some(a)
}

/// Plan node `a`'s pattern parts, left to right, as `cat_parts` reads the program's: `a` itself when a constant or an
/// extern parameter (through a RelabelType), else the parts of a `textcat` of them; `None` otherwise.
fn plan_parts<'a, 'g>(a: Node<'a>, mcx: Mcx<'g>) -> Option<PgVec<'g, Node<'a>>> {
    let (mut todo, mut out) = (mcx::pg_vec![in mcx; a], PgVec::new_in(mcx));
    while let Some(n) = todo.pop() {
        let call = n.as_op_expr().map(|o| (o.opfuncid, o.args.as_slice())).or_else(|| n.as_func_expr().map(|f| (f.funcid, f.args.as_slice())));
        match call {
            Some((f, &[x, y])) if gravity_fn::row_of(f).map(|r| r.name) == Some("textcat") => todo.extend_in([y, x]),
            Some(_) => return None,
            None => {
                let n = n.as_relabel_type().map_or(n, |r| r.arg);
                (n.as_const().is_some() || n.as_param().is_some_and(|p| p.paramkind == ParamKind::PARAM_EXTERN)).then_some(())?;
                out.push_in(n);
            }
        }
    }
    Some(out)
}

/// The word claims of a step's `c` (compiled from `node`) by program node: each `word_shape` under the root's AND / OR /
/// NOT (the plan tree and the program walked together, cmp_text.rs's way), its pattern an operand of its own (`sub`)
/// and its `WORDS` frame words, which lie in a row (the row test reads the literal through their address).
#[allow(clippy::too_many_arguments)]
fn word_sites<'g, 'p>(px: &mut Px<'g, 'p>, at: (usize, usize), node: Node<'p>, c: &Compiled<'p>, ex: ExId, sub: &mut dyn FnMut(Node<'p>) -> u32, bases: &[(u32, u32)]) -> PgVec<'g, (usize, Claim)> {
    let mcx = px.mcx();
    let (mut stack, mut out) = (mcx::pg_vec![in mcx; (c.root, node)], PgVec::new_in(mcx));
    while let Some((id, n)) = stack.pop().filter(|_| at.0 != usize::MAX) {
        match (&c.prog.nodes[id.0 as usize], n.as_bool_expr()) {
            (ExprNode::Bool(b), Some(e)) if e.args.len() == b.args.len as usize => stack.extend_in(c.children(b.args).iter().copied().zip(e.args.iter())),
            (ExprNode::Bool(_), _) => {}
            (ExprNode::Op(f) | ExprNode::Func(f), _) => {
                let base = out.len() * WORDS;
                let call = &c.prog.sites[f.site.0 as usize];
                let Some(((col, neg, fold), k)) = word_shape_of(c, id, bases, mcx).zip(plan_pattern(n, call.funcid, mcx)).filter(|_| base + WORDS <= 256) else { continue };
                // a textcat pattern: its parts, the plan's and the program's alike
                let parts = plan_parts(k, mcx).filter(|p| p.len() > 1).unwrap_or_else(|| mcx::pg_vec![in mcx; k]);
                if parts.len() > 1 && cat_parts(c, c.prog.kids[(call.args.start + 1) as usize], mcx).len() != parts.len() {
                    continue;
                }
                let key = |i: usize| WordKey::Open { face: Face::Step, k: at.0, j: crate::frame::px::key_j(at.1) ^ ((ex.0 as u64 + 1) << 40), i: (base + i) as u8 };
                let words: PgVec<'_, i32> = (0..WORDS).map(|i| px.frame_word(key(i)) as i32 * 8).collect_in(mcx);
                if words.windows(2).all(|w| w[1] == w[0] + 8) {
                    let subs: PgVec<'_, u32> = parts.iter().map(|&p| sub(p)).collect_in(mcx);
                    let word = WordSite { sub: subs[0], parts: subs.len() as u32, class: words[0], lit: words[1] };
                    out.push_in((id.0 as usize, Claim { col, test: Test::Word, lit: Box::default(), neg, word, fold })); // alloc: Program
                }
            }
            _ => {}
        }
    }
    out
}

/// The word form's row test over `[p, p + len)` with class word `cw` (`like_word`'s) and the literal at address `lit`:
/// `(hot, value word)`, not hot for a cold class. Exact, prefix and suffix compare once where the literal must start;
/// contains scans, comparing the literal's first and last bytes before all of it; an empty literal is decided by the
/// length alone. A class with `WORD_WINDOW` (a literal past the slot) compares its window where the whole literal fits:
/// a row it rules out answers, a row where it occurs is not hot (PG's call answers).
pub(crate) fn word_test(b: &mut FunctionBuilder<'_>, env: &mut Env, p: Value, len: Value, cw: Value, lit: Value, neg: bool) -> (Value, Value) {
    let k = |b: &mut FunctionBuilder<'_>, v: i64| b.iconst(Type::I64, v);
    let (t, f, one) = (b.bconst(true), b.bconst(false), env.one);
    let (lo, eight, none, flag, slot) = (k(b, 0x7f), k(b, 8), k(b, -1), k(b, lw::WORD_WINDOW as i64), k(b, WORD_LIT_BYTES as i64));
    let (cls, n) = (b.int_bin(IntBinOp::And, cw, lo), b.int_bin(IntBinOp::Ushr, cw, eight));
    // the window's length: the slot's under WORD_WINDOW, else the literal's
    let win = { let f = b.int_bin(IntBinOp::And, cw, flag); b.icmp(IntCond::Ne, f, env.zero) };
    let wn = b.select(win, slot, n);
    let is = |b: &mut FunctionBuilder<'_>, w: u64| { let w = b.iconst(Type::I64, w as i64); b.icmp(IntCond::Eq, cls, w) };
    let (exact, suffix, contains, any) = (is(b, lw::WORD_EXACT), is(b, lw::WORD_SUFFIX), is(b, lw::WORD_CONTAINS), is(b, lw::WORD_ANY));
    let hot = b.icmp(IntCond::Ne, cls, env.zero);
    let (eq, ge) = (b.icmp(IntCond::Eq, len, n), b.icmp(IntCond::Sge, len, n));
    let fits = b.select(exact, eq, ge);
    let tail = b.int_bin(IntBinOp::Sub, len, n);
    let first = { let at = b.int_bin(IntBinOp::Sub, len, wn); b.select(suffix, at, env.zero) };
    let last = { let l = b.select(contains, tail, first); b.select(fits, l, none) };
    let (empty, v0) = (b.icmp(IntCond::Eq, n, env.zero), b.int_bin(IntBinOp::Or, fits, any));
    let short = b.int_bin(IntBinOp::Or, empty, any);
    let (fixed, at, l_at) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
    let (pre, head, body, cmp, out) = (b.create_block(&[]), b.create_block(&[Type::I64]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::Bool]));
    b.cond_br(short, (out, &[v0]), (fixed, &[]));
    // exact / prefix / suffix: one compare where the literal must start, no scan
    b.switch_to(fixed);
    b.cond_br(contains, (pre, &[]), (at, &[]));
    b.switch_to(at);
    b.cond_br(fits, (l_at, &[]), (out, &[f]));
    b.switch_to(l_at);
    let q = { let pi = b.conv(ConvOp::PtrToInt, Type::I64, p); b.int_bin(IntBinOp::Add, pi, first) };
    let e = crate::rep::varlena::cmp_text::bytes_eq(b, env, q, lit, wn);
    b.br(out, &[e]);
    b.switch_to(pre);
    let (lp, n1) = (b.conv(ConvOp::IntToPtr, Type::Ptr, lit), b.int_bin(IntBinOp::Sub, wn, one));
    let l0 = b.load_zext(Type::I8, lp, 0);
    let ln = { let q = b.ptr_add(lp, n1, 1, 0); b.load_zext(Type::I8, q, 0) };
    b.br(head, &[first]);
    (b.switch_to(head), b.mark_loop_header(head));
    let i = b.block_params(head)[0];
    let more = b.icmp(IntCond::Sle, i, last);
    b.cond_br(more, (body, &[]), (out, &[f]));
    b.switch_to(body);
    let (q, next) = (b.ptr_add(p, i, 1, 0), b.int_bin(IntBinOp::Add, i, one));
    let c0 = { let v = b.load_zext(Type::I8, q, 0); b.icmp(IntCond::Eq, v, l0) };
    let c1 = { let e = b.ptr_add(q, n1, 1, 0); let v = b.load_zext(Type::I8, e, 0); b.icmp(IntCond::Eq, v, ln) };
    let hit = b.int_bin(IntBinOp::And, c0, c1);
    b.cond_br(hit, (cmp, &[]), (head, &[next]));
    b.switch_to(cmp);
    let e = { let qi = b.conv(ConvOp::PtrToInt, Type::I64, q); crate::rep::varlena::cmp_text::bytes_eq(b, env, qi, lit, wn) };
    b.cond_br(e, (out, &[t]), (head, &[next]));
    b.switch_to(out);
    let v = b.block_params(out)[0];
    // a window's occurrence is no answer: PG's call decides the row
    let hot = { let open = b.int_bin(IntBinOp::And, win, v); let shut = b.int_bin(IntBinOp::Xor, open, t); b.int_bin(IntBinOp::And, hot, shut) };
    let v = if neg { b.int_bin(IntBinOp::Xor, v, t) } else { v };
    (hot, word(b, v))
}

/// `[p, p + len)` lowered as PG's `lower` lowers it under fold `f` into `dst` (`MAX_FOLD` bytes): `[dst, len, ok]`; not ok
/// for a value past `MAX_FOLD` or a non-ASCII one under `Fold::Ascii` (its length then reads 0: the test reads nothing).
fn fold_to(b: &mut FunctionBuilder<'_>, p: Value, len: Value, f: Fold, dst: Value) -> [Value; 3] {
    let (zero, max) = (b.iconst(Type::I64, 0), b.iconst(Type::I64, MAX_FOLD as i64));
    let fits = b.icmp(IntCond::Sle, len, max);
    let n = b.select(fits, len, zero);
    let ok = if f == Fold::Ascii { let a = crate::step::text_case::ascii(b, p, n); b.int_bin(IntBinOp::And, fits, a) } else { fits };
    let n = b.select(ok, n, zero);
    crate::step::text_case::lower_into(b, p, dst, n);
    [dst, n, ok]
}

/// PS4-07: `[p, p + len)` cut as PG's coercion `k` cuts it: `(p, len, hot)`.
fn cut(b: &mut FunctionBuilder<'_>, p: Value, len: Value, hot: Value, k: Cut) -> (Value, Value, Value) {
    match k {
        Cut::No => (p, len, hot),
        Cut::Name => {
            // under UTF-8 the last character boundary at or before byte 63: a byte that is no continuation (10xxxxxx)
            let lim = NAMEDATALEN as i64 - 1;
            let l = b.iconst(Type::I64, lim);
            let far = b.icmp(IntCond::Sgt, len, l);
            let (clip, join) = (b.create_block(&[]), b.create_block(&[Type::I64]));
            b.cond_br(far, (clip, &[]), (join, &[len]));
            b.switch_to(clip);
            let (mask, cont) = (b.iconst(Type::I64, 0xc0), b.iconst(Type::I64, 0x80));
            let mut at = b.iconst(Type::I64, lim - 3);
            for j in (lim - 2)..=lim {
                let q = { let i = b.iconst(Type::I64, j); b.ptr_add(p, i, 1, 0) };
                let v = b.load_zext(Type::I8, q, 0);
                let h = b.int_bin(IntBinOp::And, v, mask);
                let lead = b.icmp(IntCond::Ne, h, cont);
                let jv = b.iconst(Type::I64, j);
                at = b.select(lead, jv, at);
            }
            b.br(join, &[at]);
            b.switch_to(join);
            (p, b.block_params(join)[0], hot)
        }
    }
}

/// `[src, src + m)` then spaces to `n` bytes into `dst` (`char(n)`'s padding), byte by byte; `m <= n <= MAX_FOLD`.
fn pad_into(b: &mut FunctionBuilder<'_>, src: Value, dst: Value, m: Value, n: Value) {
    let (zero, one, sp) = (b.iconst(Type::I64, 0), b.iconst(Type::I64, 1), b.iconst(Type::I8, 0x20));
    let (head, body, out) = (b.create_block(&[Type::I64]), b.create_block(&[]), b.create_block(&[]));
    b.br(head, &[zero]);
    (b.switch_to(head), b.mark_loop_header(head));
    let i = b.block_params(head)[0];
    let more = b.icmp(IntCond::Slt, i, n);
    b.cond_br(more, (body, &[]), (out, &[]));
    b.switch_to(body);
    let inside = b.icmp(IntCond::Slt, i, m);
    let from = { let s = b.ptr_add(src, i, 1, 0); b.select(inside, s, dst) };
    let v = b.load(Type::I8, from, 0);
    let v = b.select(inside, v, sp);
    let d = b.ptr_add(dst, i, 1, 0);
    b.store(d, 0, v);
    let i = b.int_bin(IntBinOp::Add, i, one);
    b.br(head, &[i]);
    b.switch_to(out);
}

/// An ILIKE's pattern word `p` (0: NULL) lowered as its rows are (`fold_to`), as a plain image in a frame slot: its
/// address, or 0 (like_word's cold class: PG's call answers) for a NULL, compressed or toasted pattern, one past
/// `MAX_FOLD` bytes, or a non-ASCII one under `Fold::Ascii`. Its loops are bounded by `MAX_FOLD`.
fn fold_pattern(b: &mut FunctionBuilder<'_>, env: &mut Env, p: Value, f: Fold) -> Value {
    let n0 = b.func().hints.loop_headers.len();
    let null = b.icmp(IntCond::Eq, p, env.zero);
    let [pp, plen, pok] = inline(b, p, null);
    let slot = b.declare_stack_slot(MAX_FOLD as u32 + 8, 8);
    let (base, four, two) = (b.stack_addr(slot), b.iconst(Type::I64, 4), b.iconst(Type::I64, 2));
    let dst = b.ptr_add(base, four, 1, 0);
    let [_, n, ok] = fold_to(b, pp, plen, f, dst);
    let hdr = { let size = b.int_bin(IntBinOp::Add, n, four); let h = b.int_bin(IntBinOp::Shl, size, two); b.conv(ConvOp::Trunc, Type::I32, h) };
    b.store(base, 0, hdr);
    let t = b.bconst(true);
    let live = b.int_bin(IntBinOp::Xor, null, t);
    let go = { let g = b.int_bin(IntBinOp::And, pok, ok); b.int_bin(IntBinOp::And, g, live) };
    let a = b.conv(ConvOp::PtrToInt, Type::I64, base);
    env.bounded.extend_from_slice_in(&b.func().hints.loop_headers[n0..]);
    b.select(go, a, env.zero)
}

/// PS4-05: a `textcat` pattern's part words `ps` (0: NULL) concatenated as `textcat` does, as a plain image in a frame
/// slot: its address, or 0 (like_word's cold class: PG's call answers) when a part is NULL (`textcat` is strict),
/// compressed or toasted, or the whole is past `MAX_FOLD` bytes. Its loops are bounded by `MAX_FOLD`.
fn cat_pattern(b: &mut FunctionBuilder<'_>, env: &mut Env, ps: &[Value]) -> Value {
    let n0 = b.func().hints.loop_headers.len();
    let slot = b.declare_stack_slot(MAX_FOLD as u32 + 8, 8);
    let (base, four, two, max) = (b.stack_addr(slot), b.iconst(Type::I64, 4), b.iconst(Type::I64, 2), b.iconst(Type::I64, MAX_FOLD as i64));
    let (mut at, mut go) = (env.zero, b.bconst(true));
    for &p in ps {
        let null = b.icmp(IntCond::Eq, p, env.zero);
        let [q, len, ok] = inline(b, p, null);
        let room = b.int_bin(IntBinOp::Sub, max, at);
        let fits = b.icmp(IntCond::Sle, len, room);
        let m = b.select(fits, len, env.zero);
        let dst = { let o = b.int_bin(IntBinOp::Add, at, four); b.ptr_add(base, o, 1, 0) };
        pad_into(b, q, dst, m, m);
        at = b.int_bin(IntBinOp::Add, at, m);
        let t = b.bconst(true);
        let live = b.int_bin(IntBinOp::Xor, null, t);
        let g = { let g = b.int_bin(IntBinOp::And, ok, fits); b.int_bin(IntBinOp::And, g, live) };
        go = b.int_bin(IntBinOp::And, go, g);
    }
    let hdr = { let size = b.int_bin(IntBinOp::Add, at, four); let h = b.int_bin(IntBinOp::Shl, size, two); b.conv(ConvOp::Trunc, Type::I32, h) };
    b.store(base, 0, hdr);
    let a = b.conv(ConvOp::PtrToInt, Type::I64, base);
    env.bounded.extend_from_slice_in(&b.func().hints.loop_headers[n0..]);
    b.select(go, a, env.zero)
}

/// The mechanism of road `text.memmem`.
pub(crate) struct Text;
pub(crate) static TEXT: Text = Text;

impl ExprExtMech for Text {
    fn attach<'p>(&self, px: &mut Px<'_, 'p>, at: (usize, usize), node: Node<'p>, c: &mut Compiled<'p>, ex: ExId, sub: &mut dyn FnMut(Node<'p>) -> u32) -> Option<ExtClaim> {
        if gravity_fn::typesem::collation::database_encoding() != PG_UTF8 {
            return None;
        }
        // a word claim's node never reaches `claim`, which reads (bakes) its pattern
        let mcx = px.mcx();
        let bases = domain_bases(px, c);
        let mut words = word_sites(px, at, node, c, ex, sub, &bases);
        claim_calls(c, |c, i| match words.iter().position(|w| w.0 == i) {
            // shapecheck fire signal: the word form is road text.memmem's claim too
            Some(j) => { fires::fire("text.memmem"); Some((words.swap_remove(j).1, 16)) }
            None => claim(c, i, &bases, mcx),
        })
    }

    fn emit(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, ex: ExId, s: u32, _sub: SubEmit<'_>) -> [Value; 3] {
        site(b, env, ex, s, |b, env, c: &Claim| {
            let [p, len, hot, null] = view(b, env, row, c.col);
            let (p, len, hot) = cut(b, p, len, hot, c.fold.cut);
            let (p, len, hot) = match c.fold.row {
                Fold::No => (p, len, hot),
                f => {
                    let slot = b.declare_stack_slot(MAX_FOLD as u32 + 8, 8);
                    let dst = b.stack_addr(slot);
                    let [q, n, ok] = fold_to(b, p, len, f, dst);
                    (q, n, b.int_bin(IntBinOp::And, hot, ok))
                }
            };
            let (ok, v) = match c.test {
                Test::Upper | Test::UpperAscii | Test::Lower | Test::LowerAscii => crate::step::text_case::case_map(b, env, p, len, null, c),
                Test::Word => {
                    let Some(w) = env.words else { crate::refuse!(env.node, Reason::Bug { payload: "a text.memmem word claim without frame words" }) };
                    let cw = env.load_word(b, c.word.class);
                    let lit = { let (a, o) = (b.conv(ConvOp::PtrToInt, Type::I64, w), b.iconst(Type::I64, c.word.lit as i64)); b.int_bin(IntBinOp::Add, a, o) };
                    word_test(b, env, p, len, cw, lit, c.neg)
                }
                _ => test(b, p, len, c),
            };
            let t = b.bconst(true);
            [b.int_bin(IntBinOp::And, hot, ok), v, b.int_bin(IntBinOp::Xor, null, t)]
        })
    }

    /// Each word claim's pattern (its bind word; 0 when NULL) classified once per call by host `like_word` into its class
    /// word and literal words. A pattern without its bind word writes nothing: its class word reads cold.
    fn entry(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, ex: ExId) {
        let Some(x) = ExtPlan::of(env.placed(ex)) else { return };
        let (subs, plan) = (mcx::to_vec_in(&x.subs.borrow()[..], env.mcx), x.plan.clone());
        for c in plan.downcast_ref::<Plan<Claim>>().into_iter().flat_map(|p| p.claims.iter()).filter(|c| c.test == Test::Word) {
            let word = |i: u32| match subs.get((c.word.sub + i) as usize).map(|&id| &env.placed(id).places[..]) {
                Some(&[gravity_lower::Place::Word(w)]) => Some(w),
                _ => None,
            };
            let mut ws: PgVec<'_, u16> = PgVec::new_in(env.mcx);
            if !(0..c.word.parts.max(1)).all(|i| word(i).map(|w| ws.push_in(w)).is_some()) {
                continue;
            }
            let ps: PgVec<'_, Value> = ws.iter().map(|&w| {
                let (p, nb) = (b.load(Type::I64, env.params, w as i32 * 8), b.load_zext(Type::I8, env.params_null, w as i32));
                let null = b.icmp(IntCond::Ne, nb, env.zero);
                b.select(null, env.zero, p)
            }).collect_in(env.mcx);
            let p = if let [p] = ps[..] { p } else { cat_pattern(b, env, &ps[..]) };
            let p = if c.fold.pattern { fold_pattern(b, env, p, c.fold.row) } else { p };
            let sl = b.declare_stack_slot(WORD_LIT_BYTES as u32, 8);
            let a = { let a = b.stack_addr(sl); b.conv(ConvOp::PtrToInt, Type::I64, a) };
            let h = b.declare_host_once("like_word", &[Type::Ptr, Type::I64, Type::I64], Some(Type::I64), gravity_kernels::contract::entry2::<LikeWord> as *const () as usize, HostFlags { touches_memory: true, ..HostFlags::default() });
            let cw = b.call_host(h, &[env.ctx, p, a], Some(Type::I64)).unwrap_or(env.zero);
            env.store_word(b, c.word.class, cw);
            let sa = b.stack_addr(sl);
            for i in 0..WORD_LIT_BYTES as i32 / 8 {
                let v = b.load(Type::I64, sa, 8 * i);
                env.store_word(b, c.word.lit + 8 * i, v);
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use gravity_interp::Interp;
    use mcx::GrowIn;
    use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
    use gravity_ir::{IntBinOp, IntCond, Type, Value};

    use super::{classify, inline, test, word, word_shape, word_test, Claim, Test, MAX_LIT};
    use gravity_kernels::dict::strategies::like_word::{class_word, WORD_LIT_BYTES};

    /// A 1-byte-header image of `s`.
    pub(crate) fn short(s: &[u8]) -> Vec<u8> {
        [&[((s.len() as u8 + 1) << 1) | 1][..], s].concat()
    }

    /// A plain 4-byte-header image of `s`.
    pub(crate) fn long(s: &[u8]) -> Vec<u8> {
        [&(((s.len() as u32 + 4) << 2).to_le_bytes())[..], s].concat()
    }

    /// `f(b, ptr, len, hot)` over the image `img` (`None`: a NULL), run: its two result words.
    pub(crate) fn run(img: Option<&[u8]>, f: &dyn Fn(&mut FunctionBuilder<'_>, [Value; 3], Value) -> [Value; 2]) -> [u64; 2] {
        let mut pb = ProgramBuilder::new();
        let func = pb.declare_func("t", &[Type::I64, Type::I64], &[Type::I64, Type::I64]);
        let mut b = pb.define(func);
        let (d, n) = (b.params()[0], b.params()[1]);
        let zero = b.iconst(Type::I64, 0);
        let null = b.icmp(IntCond::Ne, n, zero);
        let v = inline(&mut b, d, null);
        let out = f(&mut b, v, null);
        b.ret(&out);
        let prog = pb.finish();
        let r = Interp::new(&prog).run(func, &[img.map_or(0, |i| i.as_ptr() as u64), img.is_none() as u64]).expect("runs");
        [r[0], r[1]]
    }

    /// A claim's verdict over `img`: `None` cold, `Some(None)` NULL, else the value (a `length` as `Some(Some(n))`).
    fn eval(img: Option<&[u8]>, c: &Claim) -> Option<Option<u64>> {
        let [hot, v] = run(img, &|b, [p, len, hot], null| {
            let (ok, v) = test(b, p, len, c);
            let hot = b.int_bin(IntBinOp::And, hot, ok);
            let nul = b.iconst(Type::I64, u64::MAX as i64);
            let v = b.select(null, nul, v);
            [word(b, hot), v]
        });
        (hot == 1).then_some((v != u64::MAX).then_some(v))
    }

    fn like(img: Option<&[u8]>, pat: &str, neg: bool) -> Option<Option<bool>> {
        let (t, lit) = classify(pat.as_bytes(), mcx::MemoryContext::new("test").mcx()).expect("an inline class");
        let c = Claim { col: (gravity_expr::Source::Scan, 1, 25), test: t, lit: lit.into(), neg, word: Default::default(), fold: Default::default() };
        eval(img, &c).map(|v| v.map(|v| v == 1))
    }

    /// Every class, both header forms, NOT LIKE, NULL, multibyte text; toasted / compressed images run cold.
    #[test]
    fn like_classes_match_pg() {
        let cases: &[(&str, &str, bool)] = &[
            ("PROMO BRUSHED", "PROMO%", true), ("PROM", "PROMO%", false), ("xPROMO", "PROMO%", false),
            ("abc", "abc", true), ("abcd", "abc", false), ("ab", "abc", false), ("", "", true), ("a", "", false),
            ("hello world", "%world", true), ("world!", "%world", false), ("ld", "%world", false),
            ("the quick brown fox", "%brown%", true), ("brow", "%brown%", false), ("brownbrown", "%brown%", true),
            ("aaab", "%aab%", true), ("ababac", "%abac%", true), ("abab", "%abac%", false), ("x", "%x%", true),
            ("anything", "%", true), ("", "%", true), ("50% off", r"50\%%", true), ("500 off", r"50\%%", false),
            ("naïve café", "%café", true), ("日本語テキスト", "%語テ%", true), ("日本語", "%本語x%", false),
            ("a longer literal that spans words!", "%literal that spans words%", true),
            ("a longer literal that spans word!", "%literal that spans words%", false),
            ("exactly-sixteen!", "exactly-sixteen!", true), ("exactly-sixteen?", "exactly-sixteen!", false),
        ];
        for &(s, pat, want) in cases {
            for img in [super::tests::short(s.as_bytes()), long(s.as_bytes())] {
                assert_eq!(like(Some(&img), pat, false), Some(Some(want)), "{s:?} LIKE {pat:?}");
                assert_eq!(like(Some(&img), pat, true), Some(Some(!want)), "{s:?} NOT LIKE {pat:?}");
            }
        }
        assert_eq!(like(None, "a%", false), Some(None), "NULL LIKE: NULL");
        assert_eq!(like(None, "a%", true), Some(None), "NULL NOT LIKE: NULL");
        let compressed = [&((40u32 << 2) | 2).to_le_bytes()[..], &[0; 36]].concat();
        assert_eq!(like(Some(&compressed), "a%", false), None, "a compressed image runs cold");
        assert_eq!(like(Some(&[1, 18, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]), "a%", false), None, "a toast pointer runs cold");
        let big = "y".repeat(300) + "needle" + &"z".repeat(300);
        assert_eq!(like(Some(&long(big.as_bytes())), "%needle%", false), Some(Some(true)));
    }

    /// B2-23 / CAP-24: a literal past `MAX_LIT` is claimed and searched by its window (its first `MAX_LIT` bytes, a
    /// suffix's last) where the whole literal fits. No occurrence answers hot (LIKE false, NOT LIKE true), at both header
    /// forms; an occurrence, whole or only the window's, runs cold (PG's call answers), as does a value of the literal's
    /// exact length for `=`.
    #[test]
    fn a_literal_past_max_lit_answers_a_miss_and_runs_a_window_hit_cold() {
        long_literal_cases(&like);
    }

    /// The word form takes the same literals past its slot (like_word's `WORD_WINDOW`) with the same answers.
    #[test]
    fn the_word_form_takes_a_literal_past_its_slot() {
        long_literal_cases(&word_like);
    }

    /// `form(img, pat, neg)` over literals past `MAX_LIT` (= `WORD_LIT_BYTES`): `None` cold, `Some(None)` NULL, else the
    /// verdict.
    fn long_literal_cases(form: &dyn Fn(Option<&[u8]>, &str, bool) -> Option<Option<bool>>) {
        assert_eq!(MAX_LIT, WORD_LIT_BYTES, "one window for both forms");
        let like = form;
        let lit = format!("{}{}", "a".repeat(MAX_LIT), "tail-of-a-long-needle");
        let head = &lit[..MAX_LIT];
        let (prefix, suffix, contains) = (format!("{lit}%"), format!("%{lit}"), format!("%{lit}%"));
        let tail_win = &lit[lit.len() - MAX_LIT..];
        let hot: &[(String, &str, bool)] = &[
            (format!("x{lit}"), &prefix, false), ("short".into(), &prefix, false),
            (format!("{lit}x"), &suffix, false), (format!("zz{}", "b".repeat(200)), &contains, false), (head.to_string(), &contains, false),
            (format!("{lit}!"), &lit, false), (lit[1..].to_string(), &lit, false),
        ];
        // a 1-byte header holds at most 126 bytes
        let images = |s: &str| [Some(super::tests::short(s.as_bytes())).filter(|_| s.len() < 127), Some(long(s.as_bytes()))].into_iter().flatten();
        for (s, pat, want) in hot {
            for img in images(s.as_str()) {
                assert_eq!(like(Some(&img), pat, false), Some(Some(*want)), "{s:?} LIKE {pat:?}");
                assert_eq!(like(Some(&img), pat, true), Some(Some(!*want)), "{s:?} NOT LIKE {pat:?}");
            }
        }
        let cold: &[(String, &str)] = &[
            (format!("{lit}-and-more"), &prefix), (format!("{head}-differs-past-the-window"), &prefix), (format!("pre-{lit}"), &suffix),
            (format!("{}{tail_win}", "q".repeat(lit.len())), &suffix), (format!("xx{lit}yy"), &contains), (format!("xx{head}yy{}", "z".repeat(40)), &contains),
            (lit.clone(), &lit),
        ];
        for (s, pat) in cold {
            for img in images(s.as_str()) {
                assert_eq!(like(Some(&img), pat, false), None, "{s:?} LIKE {pat:?} runs cold");
                assert_eq!(like(Some(&img), pat, true), None, "{s:?} NOT LIKE {pat:?} runs cold");
            }
        }
    }

    /// The word form over `img` (`None`: a NULL) with `pat` classified as `like_word` does at a call's entry: `None` cold,
    /// `Some(None)` NULL, else the verdict.
    fn word_like(img: Option<&[u8]>, pat: &str, neg: bool) -> Option<Option<bool>> {
        let mut lit = [0u8; WORD_LIT_BYTES];
        let cw = class_word(pat.as_bytes(), &mut lit);
        let r = crate::frame::stop::tests::run1(&[], &mut |b, env| {
            let (d, null) = (b.iconst(Type::I64, img.map_or(0, |i| i.as_ptr() as i64)), b.bconst(img.is_none()));
            let [p, len, ok] = inline(b, d, null);
            let (cw, l) = (b.iconst(Type::I64, cw as i64), b.iconst(Type::I64, lit.as_ptr() as i64));
            let (hot, v) = word_test(b, env, p, len, cw, l, neg);
            let hot = b.int_bin(IntBinOp::And, ok, hot);
            let (two, three) = (b.iconst(Type::I64, 2), b.iconst(Type::I64, 3));
            let v = b.select(null, three, v);
            b.select(hot, v, two)
        });
        match r {
            2 => None,
            3 => Some(None),
            v => Some(Some(v == 1)),
        }
    }

    /// The word form answers as the literal claims do (`like_classes_match_pg`'s cases, one program for every pattern):
    /// both header forms, NOT LIKE, NULL, multibyte text, a literal across words and past the inline compare's 8-byte
    /// steps; a pattern of no inline class, a toasted or compressed image run cold.
    #[test]
    fn word_form_matches_pg() {
        let cases: &[(&str, &str, bool)] = &[
            ("PROMO BRUSHED", "PROMO%", true), ("PROM", "PROMO%", false), ("xPROMO", "PROMO%", false),
            ("abc", "abc", true), ("abcd", "abc", false), ("ab", "abc", false), ("", "", true), ("a", "", false),
            ("hello world", "%world", true), ("world!", "%world", false), ("ld", "%world", false),
            ("the quick brown fox", "%brown%", true), ("brow", "%brown%", false), ("brownbrown", "%brown%", true),
            ("aaab", "%aab%", true), ("ababac", "%abac%", true), ("abab", "%abac%", false), ("x", "%x%", true),
            ("anything", "%", true), ("", "%", true), ("50% off", r"50\%%", true), ("500 off", r"50\%%", false),
            ("naïve café", "%café", true), ("日本語テキスト", "%語テ%", true), ("日本語", "%本語x%", false),
            ("a longer literal that spans words!", "%literal that spans words%", true),
            ("a longer literal that spans word!", "%literal that spans words%", false),
            ("exactly-sixteen!", "exactly-sixteen!", true), ("exactly-sixteen?", "exactly-sixteen!", false),
            ("x#7", "%#7", true), ("#7x", "%#7", false), ("#", "%#7", false), ("ab#7ab#7", "%b#7a%", true),
        ];
        for &(s, pat, want) in cases {
            for img in [short(s.as_bytes()), long(s.as_bytes())] {
                assert_eq!(word_like(Some(&img), pat, false), Some(Some(want)), "{s:?} LIKE {pat:?}");
                assert_eq!(word_like(Some(&img), pat, true), Some(Some(!want)), "{s:?} NOT LIKE {pat:?}");
            }
        }
        assert_eq!(word_like(None, "a%", false), Some(None), "NULL LIKE: NULL");
        assert_eq!(word_like(None, "%a%", true), Some(None), "NULL NOT LIKE: NULL");
        for general in ["a_c", "%a%b%", r"ab\"] {
            assert_eq!(word_like(Some(&long(b"abc")), general, false), None, "{general:?} runs cold");
        }
        let compressed = [&((40u32 << 2) | 2).to_le_bytes()[..], &[0; 36]].concat();
        assert_eq!(word_like(Some(&compressed), "a%", false), None, "a compressed image runs cold");
        let big = "y".repeat(300) + "needle" + &"z".repeat(300);
        assert_eq!(word_like(Some(&long(big.as_bytes())), "%needle%", false), Some(Some(true)));
        assert_eq!(word_like(Some(&long(big.as_bytes())), "%needlz%", false), Some(Some(false)));
    }

    /// V4R-18: the word form's shape reads no constant (the bake log stays empty): LIKE / NOT LIKE of a text column and
    /// a text constant; ILIKE, a non-constant pattern and a call of one argument are not its shape.
    #[test]
    fn word_shape_reads_no_pattern() {
        let cx = mcx::MemoryContext::new("test");
        let shape = |c: gravity_expr::Compiled<'static>| {
            let d = gravity_expr::bake::Scope::open();
            let s = word_shape(&c, c.root, &[], cx.mcx()).map(|(col, neg)| (col.1, neg));
            assert_eq!(d.reads(), Vec::<usize>::new(), "a pattern was baked");
            drop(d);
            s
        };
        assert_eq!(shape(call(850, 16, 25, &[Some("PROMO%")])), Some((1, false)));
        assert_eq!(shape(call(851, 16, 1043, &[Some("%x%")])), Some((1, true)), "varchar NOT LIKE");
        assert_eq!(shape(call(1631, 16, 1042, &[Some("a_c")])), Some((1, false)), "any pattern: the class is the run time's");
        assert_eq!(shape(call(1633, 16, 25, &[Some("promo%")])), None, "ILIKE");
        assert_eq!(shape(call(850, 16, 25, &[None])), None, "a non-constant pattern");
        assert_eq!(shape(call(850, 16, 25, &[])), None, "LIKE of one argument");
        use types_nodes::primnodes::ParamKind::{PARAM_EXEC, PARAM_EXTERN};
        assert_eq!(shape(param(call(850, 16, 25, &[Some("x")]), PARAM_EXTERN, 25, false)), Some((1, false)), "PS4-05: a generic plan's $1");
        assert_eq!(shape(param(call(851, 16, 1042, &[Some("x")]), PARAM_EXTERN, 1043, true)), Some((1, true)), "a varchar $1 under a RelabelType");
        assert_eq!(shape(param(call(850, 16, 25, &[Some("x")]), PARAM_EXEC, 25, false)), None, "an InitPlan's result is set after the entry");
        assert_eq!(shape(param(call(850, 16, 25, &[Some("x")]), PARAM_EXTERN, 19, false)), None, "a $1 of a fixed-length type (name)");
        assert_eq!(shape(param(call(850, 16, 25, &[Some("x")]), PARAM_EXTERN, 23, false)), None, "a by-value $1");
    }

    /// `c` (a `call` with a constant pattern at node 1) with that pattern a `$1` of `kind` and `typ`, under a RelabelType
    /// to text when `relabel`.
    fn param(mut c: gravity_expr::Compiled<'static>, kind: types_nodes::primnodes::ParamKind, typ: u32, relabel: bool) -> gravity_expr::Compiled<'static> {
        use gravity_expr::nodes::{cast::{Canon, Relabel}, param::Param};
        use gravity_expr::{ExprNode, NodeId};
        let p = ExprNode::Param(Param { kind, id: 1, typ });
        if relabel {
            let at = NodeId(c.prog.nodes.len() as u32);
            c.prog.nodes.push_in(p);
            c.prog.nodes[1] = ExprNode::Relabel(Relabel { arg: at, canon: Canon::Keep });
        } else {
            c.prog.nodes[1] = p;
        }
        c
    }

    /// `length` of ASCII text is its byte count; a non-ASCII byte anywhere (a word or the tail) runs cold (PG counts
    /// characters); NULL is NULL.
    #[test]
    fn length_of_ascii_text() {
        let c = Claim { col: (gravity_expr::Source::Scan, 1, 25), test: Test::Len, lit: Box::default(), neg: false, word: Default::default(), fold: Default::default() };
        for s in ["", "a", "abcdefg", "abcdefgh", "abcdefghijklmnopq", "hello, world"] {
            assert_eq!(eval(Some(&long(s.as_bytes())), &c), Some(Some(s.len() as u64)), "{s:?}");
            assert_eq!(eval(Some(&short(s.as_bytes())), &c), Some(Some(s.len() as u64)), "{s:?}");
        }
        for s in ["é", "abcdefgé", "straße!!", "ascii then ı"] {
            assert_eq!(eval(Some(&long(s.as_bytes())), &c), None, "{s:?} runs cold");
        }
        assert_eq!(eval(None, &c), Some(None));
    }

    #[test]
    fn patterns_classify_as_pg_reads_them() {
        let c = |p: &str| classify(p.as_bytes(), mcx::MemoryContext::new("test").mcx()).map(|(t, l)| (t, String::from_utf8(l).unwrap()));
        assert_eq!(c("abc"), Some((Test::Exact, "abc".into())));
        assert_eq!(c(""), Some((Test::Exact, "".into())));
        assert_eq!(c("PROMO%"), Some((Test::Prefix, "PROMO".into())));
        assert_eq!(c("%%x"), Some((Test::Suffix, "x".into())));
        assert_eq!(c("%ab%%"), Some((Test::Contains, "ab".into())));
        assert_eq!(c("%"), Some((Test::Any, "".into())));
        assert_eq!(c("%%"), Some((Test::Any, "".into())));
        assert_eq!(c(r"50\%%"), Some((Test::Prefix, "50%".into())));
        assert_eq!(c(r"a\_b"), Some((Test::Exact, "a_b".into())));
        assert_eq!(c(r"a\\"), Some((Test::Exact, r"a\".into())));
        assert_eq!(c("%日本%"), Some((Test::Contains, "日本".into())));
        let long = "x".repeat(MAX_LIT + 1);
        assert_eq!(c(&long), Some((Test::Exact, long.clone())), "a literal past MAX_LIT is claimed (B2-23)");
        for general in ["a_c", "a%c", "%a%b%", r"ab\", "_"] {
            assert_eq!(c(general), None, "{general}");
        }
    }

    /// `f(args)` as gravity_expr compiles it, over a C-collation text column 1 and a text constant per `pats` entry
    /// (`None`: the column again).
    pub(crate) fn call(f: u32, ret: u32, arg0: u32, pats: &[Option<&str>]) -> gravity_expr::Compiled<'static> {
        use gravity_expr::nodes::func::{Call, FuncNode, How};
        use gravity_expr::nodes::konst::ConstNode;
        use gravity_expr::{ExprNode, Kids, NodeId, Program, SiteId, State};
        let mut prog = Program::empty_in(crate::step::expr::test_lists());
        prog.nodes.push_in(ExprNode::Var(gravity_expr::nodes::var::Var { src: gravity_expr::Source::Scan, attno: 1, typ: arg0, desc: 0 }));
        for p in pats {
            prog.nodes.push_in(match p {
                Some(s) => {
                    let img: &'static [u8] = Box::leak(long(s.as_bytes()).into_boxed_slice());
                    ExprNode::Const(ConstNode::new(gravity_expr::NullableDatum::value(datum::Datum::from_usize(img.as_ptr() as usize)), 25, -1, false))
                }
                None => ExprNode::Var(gravity_expr::nodes::var::Var { src: gravity_expr::Source::Scan, attno: 1, typ: 25, desc: 0 }),
            });
        }
        let n = prog.nodes.len() as u32;
        prog.kids.extend_in((0..n).map(NodeId));
        prog.sites.push_in(Call { funcid: f, args: Kids { start: 0, len: n }, nargs: n as u8, strict: true, collation: 950, how: How::Fmgr(0), rettype: ret, volatile: false });
        prog.nodes.push_in(ExprNode::Func(FuncNode { site: SiteId(0) }));
        let params = types_portal::params::ParamBind::NONE;
        gravity_expr::Compiled { prog, state: State::empty(), root: NodeId(n), params, callback_arg: core::ptr::null_mut(), result_type: ret }
    }

    /// A LIKE of an inline class and `length` become `CASE WHEN hot THEN value ELSE <the call, moved> END` (the call's
    /// site and arguments untouched); ILIKE, a `_` or inner-`%` pattern, a non-constant pattern and bpchar `length` are
    /// left alone (G4).
    #[test]
    fn claims_rewrite_only_the_inline_shapes() {
        use gravity_expr::{ExprNode, NodeId};
        let claimed = |mut c: gravity_expr::Compiled<'static>| {
            let plan = super::claim_calls(&mut c, |c, i| super::claim(c, i, &[], mcx::MemoryContext::new("test").mcx()))?.plan;
            let ExprNode::Case(k) = &c.prog.nodes[c.root.0 as usize] else { panic!("the root is the CASE") };
            let whens = &c.prog.kids[k.whens.start as usize..][..2];
            let at = |id: NodeId| match &c.prog.nodes[id.0 as usize] { ExprNode::Var(v) => v.attno - crate::face::step::EXT_AT, _ => -1 };
            assert_eq!((at(whens[0]), at(whens[1])), (0, 1), "the hot and value sites");
            assert!(matches!(c.prog.nodes[k.default?.0 as usize], ExprNode::Func(f) if f.site.0 == 0), "the call, moved");
            let p = plan.downcast_ref::<super::Plan<Claim>>().expect("the plan");
            Some((p.claims[0].test, String::from_utf8(p.claims[0].lit.to_vec()).unwrap(), p.claims[0].neg))
        };
        assert_eq!(claimed(call(850, 16, 25, &[Some("PROMO%")])), Some((Test::Prefix, "PROMO".into(), false)));
        assert_eq!(claimed(call(851, 16, 25, &[Some("%x%")])), Some((Test::Contains, "x".into(), true)));
        assert_eq!(claimed(call(1631, 16, 1042, &[Some("ab%")])), Some((Test::Prefix, "ab".into(), false)), "bpchar LIKE");
        assert_eq!(claimed(call(1257, 23, 25, &[])), Some((Test::Len, "".into(), false)));
        assert_eq!(claimed(call(871, 25, 25, &[])), Some((Test::Upper, "".into(), false)), "upper under C");
        assert_eq!(claimed(call(870, 25, 25, &[])), Some((Test::Lower, "".into(), false)), "lower under C (D2858)");
        assert_eq!(claimed(call(870, 25, 1042, &[])), None, "lower of a bpchar column");
        assert_eq!(claimed(call(1633, 16, 25, &[Some("PrOmO%")])), Some((Test::Prefix, "promo".into(), false)), "PS4-06: ILIKE under C, its literal lowered");
        assert_eq!(claimed(call(1634, 16, 25, &[Some("%X%")])), Some((Test::Contains, "x".into(), true)), "NOT ILIKE");
        assert_eq!(claimed(call(1660, 16, 1042, &[Some("AB%")])), Some((Test::Prefix, "ab".into(), false)), "bpchar ILIKE");
        assert_eq!(claimed(call(1633, 16, 25, &[Some("a_c")])), None, "an ILIKE `_` pattern");
        assert_eq!(claimed(call(850, 16, 25, &[Some("a_c")])), None, "a `_` pattern");
        assert_eq!(claimed(call(850, 16, 25, &[Some("P%O%")])), None, "an inner `%`");
        assert_eq!(claimed(call(850, 16, 25, &[None])), None, "a non-constant pattern");
        assert_eq!(claimed(call(1318, 23, 1042, &[])), None, "bpchar length");
        for f in [1299, 1257, 871, 870, 850] {
            let mut z = call(f, 25, 25, &[]);
            (z.prog.sites[0].args.len, z.prog.sites[0].nargs) = (0, 0);
            z.prog.kids.clear();
            assert_eq!(claimed(z), None, "a call of no arguments (now(): the t4 regress crash), funcid {f}");
        }
        assert_eq!(claimed(call(850, 16, 25, &[])), None, "LIKE of one argument");
    }

    /// STEP-08: a LIKE over a domain's column is claimed as its base's (a domain stores its base's bytes; the catalog's
    /// base types, `bases`), a `name` column's `namelike` / `namenlike` as its C string; a domain the catalog does not
    /// resolve to a LIKE type stays the call's.
    #[test]
    fn claims_read_a_domain_as_its_base_and_name_as_its_string() {
        let cx = mcx::MemoryContext::new("test");
        let got = |c: gravity_expr::Compiled<'static>, bases: &[(u32, u32)]| super::claim(&c, c.root.0 as usize, bases, mcx::MemoryContext::new("test").mcx()).map(|(cl, _)| (cl.col.2, cl.test, cl.neg));
        let dom = 90_001;
        assert_eq!(got(call(850, 16, dom, &[Some("k1%")]), &[(dom, 25)]), Some((25, Test::Prefix, false)), "a domain over text");
        assert_eq!(got(call(851, 16, dom, &[Some("%7")]), &[(dom, 1043)]), Some((1043, Test::Suffix, true)), "a domain over varchar, NOT LIKE");
        assert_eq!(got(call(1257, 23, dom, &[]), &[(dom, 25)]), Some((25, Test::Len, false)), "length of a domain over text");
        assert_eq!(got(call(850, 16, dom, &[Some("k1%")]), &[]), None, "a domain the catalog does not resolve");
        assert_eq!(got(call(850, 16, dom, &[Some("k1%")]), &[(dom, 23)]), None, "a domain over int4");
        assert_eq!(got(call(858, 16, 19, &[Some("k1%")]), &[]), Some((19, Test::Prefix, false)), "namelike");
        assert_eq!(got(call(859, 16, 19, &[Some("%12%")]), &[]), Some((19, Test::Contains, true)), "namenlike");
        assert_eq!(got(call(850, 16, 19, &[Some("k1%")]), &[]), None, "textlike never reads a name column");
        assert_eq!(got(call(858, 16, 25, &[Some("k1%")]), &[]), None, "namelike reads a name column only");
        assert_eq!(got(call(858, 16, 19, &[Some("k_1")]), &[]), None, "a `_` pattern over name");
        let shape = |c: gravity_expr::Compiled<'static>, bases: &[(u32, u32)]| super::word_shape_of(&c, c.root, bases, cx.mcx()).map(|(col, neg, _)| (col.2, neg));
        assert_eq!(shape(call(850, 16, dom, &[Some("k1%")]), &[(dom, 25)]), Some((25, false)), "the word form over a domain");
        assert_eq!(shape(call(859, 16, 19, &[Some("k1%")]), &[]), Some((19, true)), "the word form over name");
        let c = call(859, 16, 19, &[Some("k1%")]);
        assert_eq!(word_shape(&c, c.root, &[], cx.mcx()), None, "the frame pass's word_shape stays textlike's");
        let c = call(850, 16, dom, &[Some("%7%")]);
        assert_eq!(word_shape(&c, c.root, &[(dom, 25)], cx.mcx()).map(|w| (w.0 .2, w.1)), Some((25, false)), "PS4-04: a domain as its base");
        assert_eq!(word_shape(&c, c.root, &[], cx.mcx()), None, "a domain no base names");
    }

    /// A `name` datum's string: the bytes before its first NUL, at most NAMEDATALEN - 1 of them; a NULL reads none.
    #[test]
    fn name_reads_its_c_string() {
        let len = |img: Option<&[u8]>| {
            let mut pb = ProgramBuilder::new();
            let func = pb.declare_func("n", &[Type::I64, Type::I64], &[Type::I64, Type::I64]);
            let mut b = pb.define(func);
            let (d, n) = (b.params()[0], b.params()[1]);
            let zero = b.iconst(Type::I64, 0);
            let null = b.icmp(IntCond::Ne, n, zero);
            let [_, l, hot] = super::cstring(&mut b, d, null);
            let hot = word(&mut b, hot);
            b.ret(&[l, hot]);
            let prog = pb.finish();
            let r = Interp::new(&prog).run(func, &[img.map_or(0, |i| i.as_ptr() as u64), img.is_none() as u64]).expect("runs");
            [r[0], r[1]]
        };
        let name = |s: &str| { let mut v = vec![0u8; 64]; v[..s.len()].copy_from_slice(s.as_bytes()); v };
        assert_eq!(len(Some(&name("k12"))), [3, 1]);
        assert_eq!(len(Some(&name(""))), [0, 1]);
        assert_eq!(len(Some(&name(&"x".repeat(63)))), [63, 1]);
        assert_eq!(len(None), [0, 1], "a NULL");
    }

    /// E2: `op(f(x, c), k)` (`k op f(..)` when `!left`; `f(x, c)` alone for `op` 0) over column 1 of type `typ`, `f` under
    /// `coll`; `c` `None`: column 1 again.
    fn search(f: u32, c: Option<&str>, coll: u32, op: u32, k: i32, left: bool, typ: u32) -> gravity_expr::Compiled<'static> {
        use gravity_expr::nodes::func::{Call, FuncNode, How};
        use gravity_expr::nodes::konst::ConstNode;
        use gravity_expr::{ExprNode, Kids, NodeId, Program, SiteId, State};
        let (mut prog, var) = (Program::empty_in(crate::step::expr::test_lists()), |typ: u32| ExprNode::Var(gravity_expr::nodes::var::Var { src: gravity_expr::Source::Scan, attno: 1, typ, desc: 0 }));
        let img = |s: &str| -> &'static [u8] { Box::leak(long(s.as_bytes()).into_boxed_slice()) };
        prog.nodes.push_in(var(typ));
        prog.nodes.push_in(c.map_or(var(25), |s| ExprNode::Const(ConstNode::new(gravity_expr::NullableDatum::value(datum::Datum::from_usize(img(s).as_ptr() as usize)), 25, -1, false))));
        prog.nodes.push_in(ExprNode::Const(ConstNode::new(gravity_expr::NullableDatum::value(datum::Datum::from_i32(k)), 23, 4, true)));
        prog.kids.extend_in([NodeId(0), NodeId(1)]);
        let ret = if op == 0 { 16 } else { 23 };
        prog.sites.push_in(Call { funcid: f, args: Kids { start: 0, len: 2 }, nargs: 2, strict: true, collation: coll, how: How::Fmgr(0), rettype: ret, volatile: false });
        prog.nodes.push_in(ExprNode::Func(FuncNode { site: SiteId(0) }));
        if op != 0 {
            prog.kids.extend_in(if left { [NodeId(3), NodeId(2)] } else { [NodeId(2), NodeId(3)] });
            prog.sites.push_in(Call { funcid: op, args: Kids { start: 2, len: 2 }, nargs: 2, strict: true, collation: 0, how: How::Fmgr(0), rettype: 16, volatile: false });
            prog.nodes.push_in(ExprNode::Op(FuncNode { site: SiteId(1) }));
        }
        let root = NodeId(prog.nodes.len() as u32 - 1);
        let params = types_portal::params::ParamBind::NONE;
        gravity_expr::Compiled { prog, state: State::empty(), root, params, callback_arg: core::ptr::null_mut(), result_type: 16 }
    }

    /// D2858: a `lower` that is a LIKE-family call's subject (LIKE, NOT LIKE, ILIKE) is not claimed for the case map: the
    /// fold reads `lower(col)` there itself (`subject`), so the `lower` stays the call's.
    #[test]
    fn lower_under_like_stays_the_calls() {
        for f in [850, 851, 1633] {
            let c = lower_like(f, 25, 950, "x%");
            assert!(super::claim(&c, 1, &[], mcx::MemoryContext::new("test").mcx()).is_none(), "D2858: lower(col) under LIKE-family call {f} stays the call's (the fold reads it)");
        }
    }

    /// E2 (NORM-TEXT-SEARCH): strpos / position compared as a membership test and starts_with / `^@` over a constant are
    /// their LIKE twin's claim (gravity_expr percode `text_search`, the recognizer the LIKE table reads too), the compare
    /// itself claimed with its call in the cold arm; a position test, a per-row needle, bpchar text (the rtrim cast is no
    /// column), an unset collation and a non-UTF-8 database are left alone (G4); a needle past MAX_LIT is claimed whole
    /// (its window is the emit's).
    #[test]
    fn claims_take_the_text_search_spellings() {
        gravity_fn::typesem::collation::arm_database_encoding(6).expect("UTF8");
        let got = |c: gravity_expr::Compiled<'static>| {
            super::claim(&c, c.root.0 as usize, &[], mcx::MemoryContext::new("test").mcx()).map(|(cl, ty)| (cl.test, String::from_utf8(cl.lit.to_vec()).unwrap(), cl.neg, ty))
        };
        let hit = |t: Test, l: &str, neg: bool| Some((t, l.to_string(), neg, 16u32));
        assert_eq!(got(search(868, Some("goo%_\\gle"), 950, 147, 0, true, 25)), hit(Test::Contains, "goo%_\\gle", false), "strpos > 0");
        assert_eq!(got(search(849, Some("x"), 100, 144, 0, false, 25)), hit(Test::Contains, "x", false), "0 <> position");
        assert_eq!(got(search(868, Some("x"), 950, 150, 1, true, 1043)), hit(Test::Contains, "x", false), "varchar strpos >= 1");
        assert_eq!(got(search(868, Some("x"), 950, 65, 0, true, 25)), hit(Test::Contains, "x", true), "strpos = 0: NOT LIKE");
        assert_eq!(got(search(868, Some("x"), 950, 66, 1, true, 25)), hit(Test::Contains, "x", true), "strpos < 1");
        assert_eq!(got(search(868, Some(""), 950, 147, 0, true, 25)), hit(Test::Any, "", false), "an empty needle");
        assert_eq!(got(search(868, Some("日本"), 950, 147, 0, true, 25)), hit(Test::Contains, "日本", false), "multibyte");
        assert_eq!(got(search(3696, Some("PROMO"), 950, 0, 0, true, 25)), hit(Test::Prefix, "PROMO", false), "starts_with / ^@");
        assert_eq!(got(search(3696, Some(""), 950, 0, 0, true, 25)), hit(Test::Any, "", false), "starts_with(x, '')");
        let mut c = search(868, Some("goo"), 950, 147, 0, true, 25);
        let plan = super::claim_calls(&mut c, |c, i| super::claim(c, i, &[], mcx::MemoryContext::new("test").mcx())).expect("claimed").plan;
        assert_eq!(plan.downcast_ref::<super::Plan<Claim>>().map(|p| p.claims.len()), Some(1), "one claim: the compare");
        let gravity_expr::ExprNode::Case(k) = &c.prog.nodes[c.root.0 as usize] else { panic!("the compare is the CASE") };
        assert!(matches!(c.prog.nodes[k.default.unwrap().0 as usize], gravity_expr::ExprNode::Op(f) if f.site.0 == 1), "the compare, moved");
        let long_needle = "y".repeat(MAX_LIT + 1);
        assert_eq!(got(search(868, Some(long_needle.as_str()), 950, 147, 0, true, 25)), hit(Test::Contains, &long_needle, false), "a needle past MAX_LIT");
        for (c, what) in [
            (search(868, Some("x"), 950, 147, 1, true, 25), "strpos > 1"),
            (search(868, Some("x"), 950, 65, 3, true, 25), "strpos = 3"),
            (search(868, None, 950, 147, 0, true, 25), "a per-row needle"),
            (search(868, Some("x"), 950, 147, 0, true, 1042), "a bpchar column is never the searched text (PG casts it to text: trimmed)"),
            (search(868, Some("x"), 0, 147, 0, true, 25), "no collation"),
            (search(3696, None, 950, 0, 0, true, 25), "starts_with(x, x)"),
        ] {
            assert_eq!(got(c), None, "{what}");
        }
    }
    /// `textlike(lower(column 1 of type typ), pattern)` (PS4-03), `lower` under collation `lc`.
    fn lower_like(f: u32, typ: u32, lc: u32, pat: &str) -> gravity_expr::Compiled<'static> {
        use gravity_expr::nodes::func::{Call, FuncNode, How};
        use gravity_expr::nodes::konst::ConstNode;
        use gravity_expr::{ExprNode, Kids, NodeId, Program, SiteId, State};
        let mut prog = Program::empty_in(crate::step::expr::test_lists());
        prog.nodes.push_in(ExprNode::Var(gravity_expr::nodes::var::Var { src: gravity_expr::Source::Scan, attno: 1, typ, desc: 0 }));
        prog.kids.push_in(NodeId(0));
        prog.sites.push_in(Call { funcid: 870, args: Kids { start: 0, len: 1 }, nargs: 1, strict: true, collation: lc, how: How::Fmgr(0), rettype: 25, volatile: false });
        prog.nodes.push_in(ExprNode::Func(FuncNode { site: SiteId(0) }));
        let img: &'static [u8] = Box::leak(long(pat.as_bytes()).into_boxed_slice());
        prog.nodes.push_in(ExprNode::Const(ConstNode::new(gravity_expr::NullableDatum::value(datum::Datum::from_usize(img.as_ptr() as usize)), 25, -1, false)));
        prog.kids.extend_in([NodeId(1), NodeId(2)]);
        prog.sites.push_in(Call { funcid: f, args: Kids { start: 1, len: 2 }, nargs: 2, strict: true, collation: 950, how: How::Fmgr(0), rettype: 16, volatile: false });
        prog.nodes.push_in(ExprNode::Op(FuncNode { site: SiteId(1) }));
        let params = types_portal::params::ParamBind::NONE;
        gravity_expr::Compiled { prog, state: State::empty(), root: NodeId(3), params, callback_arg: core::ptr::null_mut(), result_type: 16 }
    }

    /// PS4-03: `lower(col) LIKE` is claimed over the column with its value folded (`lower`'s collation C: every value) and
    /// its pattern as written (PG lowers the value only); in both forms. `lower` of a bpchar column (PG casts it: rtrim)
    /// and `lower` under no collation stay the calls'; ILIKE over `lower(col)` is the column's ILIKE (both sides lowered).
    #[test]
    fn lower_like_claims_the_column_folded() {
        let cx = mcx::MemoryContext::new("test");
        let got = |c: gravity_expr::Compiled<'static>| super::claim(&c, c.root.0 as usize, &[], mcx::MemoryContext::new("test").mcx()).map(|(cl, _)| (cl.col.1, cl.test, String::from_utf8(cl.lit.to_vec()).unwrap(), cl.neg, cl.fold));
        let folded = super::Folding { row: super::Fold::All, pattern: false, cut: super::Cut::No };
        assert_eq!(got(lower_like(850, 25, 950, "%AbC%")), Some((1, Test::Contains, "AbC".into(), false, folded)), "the pattern is not lowered");
        assert_eq!(got(lower_like(851, 1043, 951, "x%")), Some((1, Test::Prefix, "x".into(), true, folded)), "varchar, NOT LIKE, POSIX");
        assert_eq!(got(lower_like(850, 1042, 950, "x%")), None, "lower of bpchar");
        assert_eq!(got(lower_like(850, 25, 0, "x%")), None, "lower under no collation (PG raises 42P22 in the call)");
        let ilike = super::Folding { row: super::Fold::All, pattern: true, cut: super::Cut::No };
        assert_eq!(got(lower_like(1633, 25, 950, "X%")), Some((1, Test::Prefix, "x".into(), false, ilike)), "PS4-06: ILIKE over lower(col) is the column's");
        let c = lower_like(850, 25, 950, "%x%");
        assert_eq!(super::word_shape_of(&c, c.root, &[], cx.mcx()).map(|w| (w.0 .1, w.1, w.2)), Some((1, false, folded)), "the word form");
        assert_eq!(word_shape(&c, c.root, &[], cx.mcx()), None, "the frame pass's word_shape stays an unfolded column");
    }

    /// PS4-03 / PS4-06: a folded claim lowers the value as `asc_tolower` into its scratch before the test: under
    /// `Fold::All` every value (a non-ASCII byte kept), under `Fold::Ascii` an ASCII value only (a non-ASCII one runs cold);
    /// a value past MAX_FOLD runs cold; NULL is NULL; both header forms.
    #[test]
    fn folded_claims_lower_the_value_first() {
        use super::{Fold, Folding, MAX_FOLD};
        let fold_like = |img: Option<&[u8]>, pat: &str, neg: bool, f: Fold| -> Option<Option<bool>> {
            let (t, lit) = classify(pat.as_bytes(), mcx::MemoryContext::new("test").mcx()).expect("an inline class");
            let c = Claim { col: (gravity_expr::Source::Scan, 1, 25), test: t, lit: lit.into(), neg, word: Default::default(), fold: Folding { row: f, pattern: false, cut: super::Cut::No } };
            let [hot, v] = run(img, &|b, [p, len, hot], null| {
                let slot = b.declare_stack_slot(MAX_FOLD as u32 + 8, 8);
                let dst = b.stack_addr(slot);
                let [q, n, ok] = super::fold_to(b, p, len, f, dst);
                let (ok2, v) = test(b, q, n, &c);
                let hot = b.int_bin(IntBinOp::And, hot, ok);
                let hot = b.int_bin(IntBinOp::And, hot, ok2);
                let nul = b.iconst(Type::I64, u64::MAX as i64);
                let v = b.select(null, nul, v);
                [word(b, hot), v]
            });
            (hot == 1).then_some((v != u64::MAX).then_some(v == 1))
        };
        let cases: &[(&str, &str, bool, Fold)] = &[
            ("PROMO Brushed", "promo%", true, Fold::All), ("PROMO", "PROMO%", false, Fold::All), ("xPROMO", "promo%", false, Fold::All),
            ("naïve CAFÉ", "%cafÉ", true, Fold::All), ("İSTANBUL", "%stanbul", true, Fold::All), ("STRASSE", "%strasse", true, Fold::All),
            ("MiXeD-K1-7", "%-k1-%", true, Fold::Ascii), ("ABC", "abc", true, Fold::Ascii), ("ABCD", "abc", false, Fold::Ascii),
            ("Long Literal That Spans Words!", "%literal that spans words%", true, Fold::Ascii), ("", "", true, Fold::Ascii),
            ("@[`{", "@[`{", true, Fold::All), ("Z", "z", true, Fold::All), ("z", "Z", false, Fold::All),
        ];
        for &(s, pat, want, f) in cases {
            for img in [short(s.as_bytes()), long(s.as_bytes())] {
                assert_eq!(fold_like(Some(&img), pat, false, f), Some(Some(want)), "lower({s:?}) LIKE {pat:?} {f:?}");
                assert_eq!(fold_like(Some(&img), pat, true, f), Some(Some(!want)), "lower({s:?}) NOT LIKE {pat:?} {f:?}");
            }
        }
        for s in ["naïve CAFÉ", "İSTANBUL", "ΣΑΣ"] {
            assert_eq!(fold_like(Some(&long(s.as_bytes())), "%a%", false, Fold::Ascii), None, "{s:?} runs cold under Fold::Ascii");
        }
        let big = "Y".repeat(MAX_FOLD) + "X";
        assert_eq!(fold_like(Some(&long(big.as_bytes())), "%x", false, Fold::All), None, "a value past MAX_FOLD runs cold");
        let edge = "Y".repeat(MAX_FOLD - 1) + "X";
        assert_eq!(fold_like(Some(&long(edge.as_bytes())), "%x", false, Fold::All), Some(Some(true)), "a value of MAX_FOLD bytes");
        assert_eq!(fold_like(None, "a%", false, Fold::Ascii), Some(None), "NULL");
    }

    /// PS4-07: a cast between the column and its LIKE: `cast_like(like, cast, typmod, explicit)`, the cast by its
    /// catalog oid (varchar 669, bpchar 668 over a RelabelType, text_name 407 with one argument).
    fn cast_like(f: u32, cast: u32, typmod: i32, explicit: bool, pat: &str) -> gravity_expr::Compiled<'static> {
        use gravity_expr::nodes::func::{Call, FuncNode, How};
        use gravity_expr::nodes::konst::ConstNode;
        use gravity_expr::{ExprNode, Kids, NodeId, Program, SiteId, State};
        let mut prog = Program::empty_in(crate::step::expr::test_lists());
        prog.nodes.push_in(ExprNode::Var(gravity_expr::nodes::var::Var { src: gravity_expr::Source::Scan, attno: 1, typ: 25, desc: 0 }));
        let k = |v: i64, t: u32| ExprNode::Const(ConstNode::new(gravity_expr::NullableDatum::value(datum::Datum::from_i64(v)), t, 4, true));
        let n = if cast == 407 { 1 } else { prog.nodes.extend_in([k(typmod as i64, 23), k(explicit as i64, 16)]); 3 };
        prog.kids.extend_in((0..n).map(NodeId));
        prog.sites.push_in(Call { funcid: cast, args: Kids { start: 0, len: n }, nargs: n as _, strict: true, collation: 100, how: How::Fmgr(0), rettype: 25, volatile: false });
        prog.nodes.push_in(ExprNode::Func(FuncNode { site: SiteId(0) }));
        let img: &'static [u8] = Box::leak(long(pat.as_bytes()).into_boxed_slice());
        prog.nodes.push_in(ExprNode::Const(ConstNode::new(gravity_expr::NullableDatum::value(datum::Datum::from_usize(img.as_ptr() as usize)), 25, -1, false)));
        let at = prog.nodes.len() as u32;
        prog.kids.extend_in([NodeId(at - 2), NodeId(at - 1)]);
        prog.sites.push_in(Call { funcid: f, args: Kids { start: n, len: 2 }, nargs: 2, strict: true, collation: 950, how: How::Fmgr(0), rettype: 16, volatile: false });
        prog.nodes.push_in(ExprNode::Op(FuncNode { site: SiteId(1) }));
        let params = types_portal::params::ParamBind::NONE;
        gravity_expr::Compiled { prog, state: State::empty(), root: NodeId(at), params, callback_arg: core::ptr::null_mut(), result_type: 16 }
    }

    /// PS4-07: a ::name cast of a text column is claimed with its cut; a varchar(n) / char(n) cast (its typmod a by-value
    /// word no key holds) and ILIKE over a cast are not.
    #[test]
    fn casts_claim_the_column_cut() {
        use super::Cut;
        let got = |c: gravity_expr::Compiled<'static>| super::claim(&c, c.root.0 as usize, &[], mcx::MemoryContext::new("test").mcx()).map(|(cl, _)| (cl.col.1, cl.test, cl.fold.cut));
        assert_eq!(got(cast_like(858, 407, -1, true, "%x")), Some((1, Test::Suffix, Cut::Name)), "::name");
        assert_eq!(got(cast_like(850, 669, 44, true, "%12%")), None, "varchar(40): PG's call");
        assert_eq!(got(cast_like(1631, 668, 36, true, "k1%")), None, "char(32): PG's call");
        assert_eq!(got(cast_like(1633, 407, -1, true, "%12%")), None, "ILIKE over a cast");
    }

    /// PS4-07: a cut value reads as PG's cast: ::name clipped to 63 bytes at a character boundary.
    #[test]
    fn cuts_read_the_cast_value() {
        use super::{Cut, Folding};
        let cut_like = |s: &str, pat: &str, k: Cut| -> Option<bool> {
            let (t, lit) = classify(pat.as_bytes(), mcx::MemoryContext::new("test").mcx()).expect("an inline class");
            let c = Claim { col: (gravity_expr::Source::Scan, 1, 25), test: t, lit: lit.into(), neg: false, word: Default::default(), fold: Folding { cut: k, ..Folding::default() } };
            let img = long(s.as_bytes());
            let [hot, v] = run(Some(&img), &|b, [p, len, hot], _| {
                let (q, n, h) = super::cut(b, p, len, hot, k);
                let (ok, v) = test(b, q, n, &c);
                let h = b.int_bin(IntBinOp::And, h, ok);
                [word(b, h), v]
            });
            (hot == 1).then_some(v == 1)
        };
        let a63 = "a".repeat(62);
        assert_eq!(cut_like("short", "%or%", Cut::Name), Some(true), "a short value as it is");
        let long_ascii = format!("{a63}bc{}", "d".repeat(6));
        assert_eq!(cut_like(&long_ascii, "%b", Cut::Name), Some(true), "63 bytes");
        assert_eq!(cut_like(&long_ascii, "%c%", Cut::Name), Some(false), "byte 64 is cut");
        let long_mb = format!("{a63}é{}", "x".repeat(5));
        assert_eq!(cut_like(&long_mb, "%é%", Cut::Name), Some(false), "a character across byte 63 is cut whole");
        assert_eq!(cut_like(&long_mb, &format!("{a63}"), Cut::Name), Some(true), "62 bytes left");
        assert_eq!(cut_like("short", "short", Cut::Name), Some(true), "under 64 bytes: as is");
    }

    /// PS4-06: an ILIKE's pattern word folded at the entry: a plain image of its `asc_tolower` bytes (both header forms
    /// in), or 0 (like_word's cold class) for NULL, a compressed image, one past MAX_FOLD, or a non-ASCII one under
    /// `Fold::Ascii`.
    #[test]
    fn ilike_patterns_fold_at_the_entry() {
        use super::{Fold, MAX_FOLD};
        // what = 0: the address is set; 1: the image's header word; 2: its first payload word
        let fp = |img: Option<&[u8]>, f: Fold, what: u8| -> i64 {
            crate::frame::stop::tests::run1(&[], &mut |b, env| {
                let p = b.iconst(Type::I64, img.map_or(0, |i| i.as_ptr() as i64));
                let a = super::fold_pattern(b, env, p, f);
                let set = b.icmp(IntCond::Ne, a, env.zero);
                match what {
                    0 => word(b, set),
                    _ => {
                        let q = b.conv(gravity_ir::ConvOp::IntToPtr, Type::Ptr, a);
                        if what == 1 { b.load_zext(Type::I32, q, 0) } else { b.load_unaligned(Type::I64, q, 4) }
                    }
                }
            })
        };
        for (pat, f) in [("PROMO%", Fold::All), ("%İX%", Fold::All), ("Ab\\%_", Fold::Ascii)] {
            for img in [short(pat.as_bytes()), long(pat.as_bytes())] {
                assert_eq!(fp(Some(&img), f, 0), 1, "{pat:?}");
                assert_eq!(fp(Some(&img), f, 1), ((pat.len() as i64 + 4) << 2), "{pat:?}: a plain header");
                let mut w = [0u8; 8];
                let low = pat.as_bytes().to_ascii_lowercase();
                w[..low.len().min(8)].copy_from_slice(&low[..low.len().min(8)]);
                let got = fp(Some(&img), f, 2).to_le_bytes();
                assert_eq!(&got[..low.len().min(8)], &w[..low.len().min(8)], "{pat:?}: lowered");
            }
        }
        assert_eq!(fp(None, Fold::All, 0), 0, "NULL");
        assert_eq!(fp(Some(&long("%İx%".as_bytes())), Fold::Ascii, 0), 0, "non-ASCII under Fold::Ascii");
        assert_eq!(fp(Some(&long("y".repeat(MAX_FOLD + 1).as_bytes())), Fold::All, 0), 0, "past MAX_FOLD");
        let compressed = [&((40u32 << 2) | 2).to_le_bytes()[..], &[0; 36]].concat();
        assert_eq!(fp(Some(&compressed), Fold::All, 0), 0, "a compressed pattern");
    }

    /// `c` (a `call` with a constant pattern at node 1) with that pattern `f(f('%', $1), last)` (PS4-05's `'%' || $1 ||
    /// '%'` for `f` = textcat 1258), `last` a text constant or (`None`) the column.
    fn cat(mut c: gravity_expr::Compiled<'static>, f: u32, last: Option<&str>) -> gravity_expr::Compiled<'static> {
        use gravity_expr::nodes::func::{Call, FuncNode, How};
        use gravity_expr::nodes::{konst::ConstNode, param::Param};
        use gravity_expr::{ExprNode, Kids, NodeId, SiteId};
        let konst = |s: &str| {
            let img: &'static [u8] = Box::leak(long(s.as_bytes()).into_boxed_slice());
            ExprNode::Const(ConstNode::new(gravity_expr::NullableDatum::value(datum::Datum::from_usize(img.as_ptr() as usize)), 25, -1, false))
        };
        let push = |c: &mut gravity_expr::Compiled<'static>, n: ExprNode<'static>| {
            c.prog.nodes.push_in(n);
            NodeId(c.prog.nodes.len() as u32 - 1)
        };
        let site = |c: &mut gravity_expr::Compiled<'static>, x: NodeId, y: NodeId| {
            let start = c.prog.kids.len() as u32;
            c.prog.kids.extend_in([x, y]);
            c.prog.sites.push_in(Call { funcid: f, args: Kids { start, len: 2 }, nargs: 2, strict: true, collation: 100, how: How::Fmgr(0), rettype: 25, volatile: false });
            ExprNode::Op(FuncNode { site: SiteId(c.prog.sites.len() as u32 - 1) })
        };
        let (a, p) = (push(&mut c, konst("%")), push(&mut c, ExprNode::Param(Param { kind: types_nodes::primnodes::ParamKind::PARAM_EXTERN, id: 1, typ: 25 })));
        let inner = site(&mut c, a, p);
        let inner = push(&mut c, inner);
        let z = match last { Some(s) => push(&mut c, konst(s)), None => NodeId(0) };
        c.prog.nodes[1] = site(&mut c, inner, z);
        c
    }

    /// PS4-05: a generic plan's `'%' || $1 || '%'` is the word form's (its parts, left to right); a part read per row
    /// or another call is not.
    #[test]
    fn textcat_patterns_take_the_word_form() {
        let cx = mcx::MemoryContext::new("test");
        let shape = |c: gravity_expr::Compiled<'static>| {
            let d = gravity_expr::bake::Scope::open();
            let s = word_shape(&c, c.root, &[], cx.mcx()).map(|(col, neg)| (col.1, neg));
            assert_eq!(d.reads(), Vec::<usize>::new(), "a pattern was baked");
            drop(d);
            s
        };
        assert_eq!(shape(cat(call(850, 16, 25, &[Some("x")]), 1258, Some("%"))), Some((1, false)), "'%' || $1 || '%'");
        assert_eq!(shape(cat(call(851, 16, 25, &[Some("x")]), 1258, Some("%"))), Some((1, true)), "NOT LIKE");
        assert_eq!(shape(cat(call(850, 16, 25, &[Some("x")]), 1258, None)), None, "a part read per row (the column)");
        assert_eq!(shape(cat(call(850, 16, 25, &[Some("x")]), 67, Some("%"))), None, "another call (texteq)");
        let c = cat(call(850, 16, 25, &[Some("x")]), 1258, Some("%"));
        let parts = super::cat_parts(&c, gravity_expr::NodeId(1), cx.mcx());
        let kinds: Vec<&str> = parts.iter().map(|p| match c.prog.nodes[p.0 as usize] { gravity_expr::ExprNode::Const(_) => "const", gravity_expr::ExprNode::Param(_) => "param", _ => "other" }).collect();
        assert_eq!(kinds, ["const", "param", "const"], "left to right");
    }

    /// PS4-05: the entry's concatenation of a textcat pattern's parts: a plain image of their bytes in order (short and
    /// plain headers alike); a NULL, a compressed part or a whole past MAX_FOLD reads cold (0).
    #[test]
    fn textcat_patterns_concatenate_at_the_entry() {
        use super::MAX_FOLD;
        // what = 0: the address is set; 1: the image's header word; 2: its first payload word
        let cp = |imgs: &[Option<&[u8]>], what: u8| -> i64 {
            crate::frame::stop::tests::run1(&[], &mut |b, env| {
                let ps: Vec<Value> = imgs.iter().map(|i| b.iconst(Type::I64, i.map_or(0, |i| i.as_ptr() as i64))).collect();
                let a = super::cat_pattern(b, env, &ps);
                let set = b.icmp(IntCond::Ne, a, env.zero);
                match what {
                    0 => word(b, set),
                    _ => {
                        let q = b.conv(gravity_ir::ConvOp::IntToPtr, Type::Ptr, a);
                        if what == 1 { b.load_zext(Type::I32, q, 0) } else { b.load_unaligned(Type::I64, q, 4) }
                    }
                }
            })
        };
        let (pc, k1, x, e) = (short(b"%"), long(b"k1"), short(b"ab_%"), short(b""));
        for parts in [vec![&pc[..], &k1[..], &pc[..]], vec![&k1[..], &pc[..]], vec![&x[..], &k1[..]], vec![&k1[..], &e[..]]] {
            let imgs: Vec<Option<&[u8]>> = parts.iter().map(|p| Some(*p)).collect();
            let want: Vec<u8> = parts.iter().flat_map(|p| if p[0] & 1 == 1 { &p[1..] } else { &p[4..] }).copied().collect();
            assert_eq!(cp(&imgs, 0), 1, "{want:?}");
            assert_eq!(cp(&imgs, 1), (want.len() as i64 + 4) << 2, "{want:?}: a plain header");
            let got = cp(&imgs, 2).to_le_bytes();
            assert_eq!(&got[..want.len().min(8)], &want[..want.len().min(8)], "{want:?}: the parts in order");
        }
        assert_eq!(cp(&[Some(&pc), None, Some(&pc)], 0), 0, "a NULL part: textcat is strict");
        let compressed = [&((40u32 << 2) | 2).to_le_bytes()[..], &[0; 36]].concat();
        assert_eq!(cp(&[Some(&pc), Some(&compressed)], 0), 0, "a compressed part");
        let half = long("y".repeat(MAX_FOLD / 2 + 1).as_bytes());
        assert_eq!(cp(&[Some(&half), Some(&half)], 0), 0, "past MAX_FOLD");
        let fit = long("y".repeat(MAX_FOLD - 1).as_bytes());
        assert_eq!(cp(&[Some(&fit), Some(&pc)], 0), 1, "exactly MAX_FOLD");
    }

    /// PS4-06: `upper(col)` / `lower(col)` ILIKE under C is the column's ILIKE (both sides folded), the inner `upper`
    /// claimed first and its CASE read through; LIKE over `upper(col)` stays PG's call.
    #[test]
    fn upper_ilike_reads_the_claimed_upper() {
        use super::Fold;
        use gravity_expr::nodes::func::{Call, FuncNode, How};
        use gravity_expr::nodes::konst::ConstNode;
        use gravity_expr::{ExprNode, Kids, NodeId, Program, SiteId, State};
        let build = |inner: u32, f: u32| {
            let mut prog = Program::empty_in(crate::step::expr::test_lists());
            prog.nodes.push_in(ExprNode::Var(gravity_expr::nodes::var::Var { src: gravity_expr::Source::Scan, attno: 1, typ: 25, desc: 0 }));
            prog.kids.push_in(NodeId(0));
            prog.sites.push_in(Call { funcid: inner, args: Kids { start: 0, len: 1 }, nargs: 1, strict: true, collation: 950, how: How::Fmgr(0), rettype: 25, volatile: false });
            prog.nodes.push_in(ExprNode::Func(FuncNode { site: SiteId(0) }));
            let img: &'static [u8] = Box::leak(long(b"AB%").into_boxed_slice());
            prog.nodes.push_in(ExprNode::Const(ConstNode::new(gravity_expr::NullableDatum::value(datum::Datum::from_usize(img.as_ptr() as usize)), 25, -1, false)));
            prog.kids.extend_in([NodeId(1), NodeId(2)]);
            prog.sites.push_in(Call { funcid: f, args: Kids { start: 1, len: 2 }, nargs: 2, strict: true, collation: 950, how: How::Fmgr(0), rettype: 16, volatile: false });
            prog.nodes.push_in(ExprNode::Op(FuncNode { site: SiteId(1) }));
            let params = types_portal::params::ParamBind::NONE;
            gravity_expr::Compiled { prog, state: State::empty(), root: NodeId(3), params, callback_arg: core::ptr::null_mut(), result_type: 16 }
        };
        let claims = |mut c: gravity_expr::Compiled<'static>| -> Vec<(Test, String, Fold, bool)> {
            let Some(x) = super::claim_calls(&mut c, |c, i| super::claim(c, i, &[], mcx::MemoryContext::new("test").mcx())) else { return Vec::new() };
            let p = x.plan.downcast_ref::<super::Plan<Claim>>().expect("the plan");
            p.claims.iter().filter(|cl| cl.col.1 == 1).map(|cl| (cl.test, String::from_utf8(cl.lit.to_vec()).unwrap(), cl.fold.row, cl.fold.pattern)).collect()
        };
        let ilike = (Test::Prefix, "ab".to_string(), Fold::All, true);
        let up = claims(build(871, 1633));
        assert!(up.contains(&ilike), "upper(t) ILIKE 'AB%': {up:?}");
        assert!(claims(build(870, 1633)).contains(&ilike), "lower(t) ILIKE 'AB%'");
        assert!(claims(build(871, 1634)).contains(&(Test::Prefix, "ab".to_string(), Fold::All, true)), "upper(t) NOT ILIKE");
        assert!(!claims(build(871, 850)).iter().any(|c| c.0 == Test::Prefix), "upper(t) LIKE stays PG's call");
    }
}
