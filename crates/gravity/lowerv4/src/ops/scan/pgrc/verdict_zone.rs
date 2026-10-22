//! Min / max zone verdicts (H02a; KEEP lowerv2 source/range.rs `zoned` + the decomposer's `sources::zone`): a leading
//! filter's qual `lane op constant` (or `lane IS [NOT] NULL`; the lane may sit under an integer widening cast) over a by-value lane becomes a `ZonePred` on the pgrc site,
//! which the granule host answers per granule from the footer records (`granule_source::zone_verdict`: `NoMatch` skips the
//! granule, `AllMatch` holds when every row passes). The pgrc source clears the decomposer's predicates and asks this
//! road for each qual, so only the quals declared here are verdicts: a predicate is declared only where comparing the
//! lane's stored integer with the constant's integer is the operator itself (same-family integer, date, timestamp and
//! timestamptz operators), never the cross-type date / timestamp ones, whose stored units differ (days vs microseconds;
//! the timestamptz one depends on TimeZone). The code lane's `<> ''` stays the decomposer's (the host decodes the rest
//! for its survivors, R2-L13); it is never `AllMatch`. An IN list (`lane = ANY (constant array)`) over a by-value lane
//! is a set: per execution, the inexact pair `lane >= min`, `lane <= max` of its members ([`list_of`]), so a granule whose
//! range misses the members' span is skipped; never `AllMatch`-covered (the bloom road takes the members themselves). A
//! float lane's comparison takes the constant's order key (its zone records hold the canonical float key: [`float_op`]).
//! NEW-72 (literal-free constants: a literal is never baked into a program): a by-value integer-family literal (decompose `bound_form`) and an IN
//! list's members are never read here; the site binds their words and the host takes the execution's values, so one
//! program serves every literal. Every other static predicate bakes its constant, which the content key holds (lower
//! key.rs `zone_constants`, the decomposer's keyed site; numeric constants are by-reference images the lowering logs).
//! Registers road `verdict.zone`. Owner M1-SOURCE.
//!
//! The constant-key window (V4R-19 A, decision 14; host_abi v4 m1 `VERDICT_CONST_KEY`): the host marks a window whose
//! group key lane its exact zone records prove one non-NULL value, and run.collapse folds its rows into one run.
use gravity_abi::breaker::KeyKind;
use gravity_abi::ctx::lane;
use gravity_decompose::catalog::casts::{domain_base, int_numeric, varchar_text};
use gravity_decompose::ops::scan::{eq_bloom_key, fixed_order_key, float_key, float_op, text_prefix_key};
use gravity_decompose::ops::shared::scan::{or_of_ands, same_type_zone_op, text_zone_op, zero_extended, zone_op, zone_word};
use gravity_decompose::{AttFact, Facts, Op, Source};
use gravity_expr::emit::ParamNeed;
use gravity_expr::nodes::array::elem_facts;
use gravity_expr::catalog::ElemFacts;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntCond, Type, Value};
use gravity_program::protos::word_zone_arg;
use gravity_lower::Place;
use gravity_program::{LaneId, Rep, SiteDesc, ZoneOp, ZonePred, BOUND_FIXED_HASH, BOUND_FIXED_PREFIX, BOUND_TEXT_HASH, BOUND_TEXT_PREFIX};
use mcx::{CollectIn, Mcx, PgVec, GrowIn};
use types_nodes::primnodes::{NullTestType, ParamKind};
use types_nodes::Node;

use crate::face::runs::OrderUse;
use crate::face::source::{VerdictImpl, VerdictMech};
use crate::frame::env::Env;
use crate::frame::px::{ExId, Px};
use crate::ops::Table;
use crate::rep::storage;

crate::strategies! {
    /// Min / max zone verdicts.
    VERDICT_ZONE = 9, "verdict.zone", PgrcFilter;
}

pub(crate) const ROWS: Table = Table { strategies: &[(VERDICT_ZONE, &VerdictImpl(&Zone))], ..crate::ops::NONE };

struct Zone;

/// NUMZONE (V4R-28): numeric's comparisons (pg_operator 1752-1757: = <> < <= > >=), only over a pgrc packed-numeric lane.
fn numeric_op(opno: u32) -> Option<ZoneOp> { [ZoneOp::Eq, ZoneOp::Ne, ZoneOp::Lt, ZoneOp::Le, ZoneOp::Gt, ZoneOp::Ge].get(opno.wrapping_sub(1752) as usize).copied() }
/// P5-13: the zone comparison of a btree operator of a memcmp-ordered fixed type (`gravity_fn::typesem::pgrc::memcmp_fixed`:
/// uuid, macaddr, macaddr8; both operands that type) and the type's image length: `<`, `<=`, `=`, `>=`, `>` by their btree
/// strategy, `<>` as `=`'s negator (decompose `same_type_zone_op`, from the btree catalog, never an OID list).
fn fixed_op(opno: u32) -> Option<(ZoneOp, u16)> {
    let (op, typ) = same_type_zone_op(opno)?;
    Some((op, gravity_fn::typesem::pgrc::memcmp_fixed(typ)?))
}

/// P5-13: the image length of lane `k` of `site` when it is a fixed-length by-reference lane (`Rep::FixedPtr`) that operator
/// `opno` compares as a memcmp-ordered type of that length ([`fixed_op`]): the lanes whose order keys and blooms the writer
/// derives from the image's bytes (pgrc `MemcmpFixed`).
fn fixed_lane(site: &SiteDesc, k: usize, opno: u32) -> Option<u16> {
    let Rep::FixedPtr { len } = site.lanes()[k].rep else { return None };
    fixed_op(opno).filter(|f| f.1 == len).map(|_| len)
}

/// `m op x` (x = numeric payload `pay` x 10^`s`) over i64 mantissas: x integral as is, finer by the operator's rounding (`<` / `>=` ceil, `<=` / `>` floor; `=` / `<>` refuse).
fn mantissa_bound(pay: &[u8], s: i32, zop: ZoneOp) -> Option<i64> {
    let h = u16::from_ne_bytes([*pay.first()?, *pay.get(1)?]);
    let (neg, w, at) = match h & 0xC000 {
        0xC000 => return None, // NaN / +-Infinity
        0x8000 => (h & 0x2000 != 0, (h & 0x3F) as i32 - if h & 0x40 != 0 { 64 } else { 0 }, 2), // short header: 7-bit weight
        sign => (sign == 0x4000, i16::from_ne_bytes([*pay.get(2)?, *pay.get(3)?]) as i32, 4),
    };
    let (mut acc, mut frac) = (0i128, false);
    for (i, c) in pay[at..].chunks_exact(2).enumerate() {
        let (d, e) = (u16::from_ne_bytes([c[0], c[1]]) as i128, 4 * (w - i as i32) + s); // e: the digit's power of ten in x
        let p = 10i128.checked_pow(if e >= 0 { e as u32 } else { (-e).min(4) as u32 })?;
        frac |= e < 0 && d % p != 0;
        acc = acc.checked_add(if e >= 0 { d.checked_mul(p)? } else { d / p })?;
    }
    let floor = if neg { -(acc + frac as i128) } else { acc }; // floor(-x) = -ceil(x)
    i64::try_from(match (zop, frac) { (_, false) | (ZoneOp::Le | ZoneOp::Gt, true) => floor, (ZoneOp::Lt | ZoneOp::Ge, true) => floor + 1, _ => return None }).ok()
}
/// Lane `k` of `site` that `n` reads: a plain column of the scan, or one under an integer widening cast (`int2col::int8`:
/// `types_core::quiet::int_widen`, never raising, the same value and order, so the lane's word answers the comparison as is; the
/// decomposer's `sources::zone` sees through it too, so the predicate's constant is in the keyed site), or under an integer to
/// numeric cast (PS1-02, `catalog::casts::int_numeric`: `x::numeric op c` is the integer `x` against `c` rounded the operator's
/// way, `mantissa_bound` at scale 0); a domain column read as its base type (PS1-10, `catalog::casts::domain_base` over the
/// relation's column facts `att`: the decomposer's `sources::zone` reads it so too); a varchar column read as text (SRC-09,
/// `catalog::casts::varchar_text`: the same bytes; only a code lane's predicates read it, whose constants are logged images).
fn lane_of(n: Node<'_>, varno: i32, site: &SiteDesc, att: &dyn Fn(i16) -> Option<AttFact>, mcx: Mcx<'_>) -> Option<usize> {
    if let Some(k) = path_lane_of(n, varno, site, mcx) {
        return Some(k);
    }
    let n = match n.as_func_expr() {
        Some(f) if f.args.len() == 1 && (types_core::quiet::int_widen(f.funcid) || int_numeric(f.funcid)) => f.args.nth(0),
        _ => n,
    };
    let v = varchar_text(domain_base(n, varno, att), varno, att).as_var().filter(|v| v.varno == varno && v.varlevelsup == 0 && v.varattno > 0)?;
    site.lanes().iter().position(|l| l.id == LaneId::Attr(v.varattno)).filter(|&k| k < site.natts())
}

/// sortpath (2): `n` as a jsonb text path (`col ->> 'k'`, jsonb.lanes `field`) the site declared as its shredded Text code
/// lane (jsonb.lanes `engage`): the lane. Its zone records are the path's Stats records in each part that elected it
/// (hosts `Faces::locate`; the writer seals a Text lane exception-free, so a part's lane rows are PG's `->>` exactly: a
/// lane NULL is SQL NULL, an absent key or a JSON null); a part that did not elect the path has no records and answers
/// Mixed (the host's `facts_of` is none), so its granules are never pruned on the path.
fn path_lane_of(n: Node<'_>, varno: i32, site: &SiteDesc, mcx: Mcx<'_>) -> Option<usize> {
    use gravity_program::shred_kind;
    let (attno, path) = crate::ops::scan::pgrc::strategies::jsonb_lanes::field_in(n, varno, mcx)?;
    let path: &[u8] = &path;
    let d = site.shreds.iter().find(|d| d.kind == shred_kind::TEXT_CODES && site.out_attnos.get(d.lane as usize) == Some(&attno) && site.table_arg_at(d.path) == path)?;
    let k = d.lane as usize;
    (k < site.natts() && site.lanes()[k].kind == lane::KIND_CODES).then_some(k)
}

/// `q` as `lane op other` (the operator turned when the lane is on the right): a two-argument zone operator with exactly
/// one side a lane of the site. A cross-unit comparison (date vs timestamp(tz): days against microseconds) is none:
/// decompose's `zone_op` maps only operators whose operands share the stored units (D257), and normalize's `retype`
/// already turned `datecol >= timestamp '2020-03-01 12:00'` into `datecol >= date '2020-03-02'` (SRC-07), so the
/// constant arrives in the lane's units; timestamptz keeps PG's operator (its day needs the session TimeZone).
fn compare_of<'p>(q: Node<'p>, varno: i32, site: &SiteDesc, att: &dyn Fn(i16) -> Option<AttFact>, mcx: Mcx<'_>) -> Option<(usize, ZoneOp, Node<'p>)> {
    let op = q.as_op_expr()?;
    if op.args.len() != 2 {
        return None;
    }
    let (a0, a1) = (op.args.nth(0), op.args.nth(1));
    let (k, other, flipped) = match (lane_of(a0, varno, site, att, mcx), lane_of(a1, varno, site, att, mcx)) {
        (Some(k), None) => (k, a1, false),
        (None, Some(k)) => (k, a0, true),
        _ => return None,
    };
    let zop = match (flipped, zone_op(op.opno).or(numeric_op(op.opno)).or(float_op(op.opno)).or(text_zone_op(op.opno)).or(fixed_op(op.opno).map(|f| f.0))?) {
        (true, ZoneOp::Lt) => ZoneOp::Gt,
        (true, ZoneOp::Le) => ZoneOp::Ge,
        (true, ZoneOp::Gt) => ZoneOp::Lt,
        (true, ZoneOp::Ge) => ZoneOp::Le,
        (_, o) => o,
    };
    Some((k, zop, other))
}

/// The zone predicate `q` is, over `site`'s lanes, and whether it is exact (`AllMatch` means every row passes): `lane op
/// value` with a by-value lane and a non-NULL by-value constant (a float lane's constant as its order key), `lane IS [NOT] NULL` (any lane: the granule's non-NULL
/// count answers it), or (SRC-09) a code lane's text compare under a collation that orders by bytes (`=` / `<>` under any
/// deterministic one), as the constant's prefix key ([`ZonePred::prefix`]: the host answers it from Coarse records by strict comparisons, so an
/// `AllMatch` still means every row passes). `facts`: the relation's column facts and the collation's.
pub(crate) fn pred_of(q: Node<'_>, varno: i32, site: &SiteDesc, facts: &dyn Facts, mcx: Mcx<'_>) -> Option<(ZonePred, bool)> {
    let att = |a: i16| facts.att(site.relation, a);
    let att: &dyn Fn(i16) -> Option<AttFact> = &att;
    if let Some(t) = q.as_null_test() {
        if t.argisrow {
            return None;
        }
        let op = if t.nulltesttype == NullTestType::IS_NOT_NULL { ZoneOp::NotNull } else { ZoneOp::IsNull };
        let k = lane_of(t.arg?, varno, site, att, mcx)?;
        return Some((ZonePred::word(k as u16, op, 0), true));
    }
    let (k, zop, other) = compare_of(q, varno, site, att, mcx)?;
    let l = site.lanes()[k];
    let c = other.as_const()?;
    if matches!(l.rep, Rep::PackedNum { .. }) || numeric_op(q.as_op_expr()?.opno).is_some() {
        // PS1-02: a numeric compare over an integer lane reads it through an int_numeric cast (lane_of): scale-0 words
        let int = l.kind == lane::KIND_BYVAL && matches!(l.rep, Rep::Word { .. }) && numeric_op(q.as_op_expr()?.opno).is_some();
        let packed = l.kind == lane::KIND_FIXED && matches!(l.rep, Rep::PackedNum { .. });
        let s = if int { Some(0) } else { l.num.scale().filter(|s| *s >= 0 && packed) };
        let s = s.filter(|_| gravity_fn::typesem::pgrc::numeric(c.consttype) && !c.constisnull)?;
        return Some((ZonePred::word(k as u16, zop, mantissa_bound(gravity_decompose::expr::const_bytes(c)?, s as i32, zop)?), true));
    }
    if float_op(q.as_op_expr()?.opno).is_some() {
        let fixed = l.kind == lane::KIND_FIXED && matches!(l.rep, Rep::Word { bytes: 4 | 8 });
        let value = fixed.then(|| float_key(c.consttype, zone_word(c)?)).flatten()?;
        return Some((ZonePred::word(k as u16, zop, value), true));
    }
    if l.kind == lane::KIND_CODES {
        // SRC-09: the image is read (and logged in the key) only for a predicate that bakes it
        let o = q.as_op_expr()?;
        let text = text_zone_op(o.opno).is_some() && bytes_bound(facts, o.inputcollid, zop) && !c.constisnull;
        return text.then(|| gravity_decompose::expr::const_bytes(c)).flatten().map(|d| (ZonePred::prefix(k as u16, zop, text_prefix_key(d)), true));
    }
    // P5-13: a memcmp-ordered fixed image's compare as the constant's order key ([`ZonePred::prefix`]: never the lane's word,
    // which is a pointer): the host answers it from the granule's Coarse record by strict comparisons (uuid's 8-byte prefix),
    // or from its Exact record as a word (macaddr, macaddr8: the whole image), so an `AllMatch` still means every row passes
    if let Some(len) = fixed_lane(site, k, q.as_op_expr()?.opno) {
        return gravity_decompose::expr::const_fixed_bytes(c, len).map(|d| (ZonePred::prefix(k as u16, zop, fixed_order_key(d)), true));
    }
    if l.kind != lane::KIND_BYVAL {
        return None;
    }
    Some((ZonePred::word(k as u16, zop, zone_word(c)?), true))
}

/// Qual `q`'s zone predicate on the site, through the plan node the source recorded for it. NEW-72: a qual of
/// decompose's `bound_form` shape never reads its constant (the content key does not hold it): over a by-value word lane
/// it is the bound predicate over the constant's word ([`ZonePred::bound`], the host binds its value per execution,
/// exact as the literal's), over any other lane, or with no word for the constant, none.
pub(crate) fn of(px: &Px<'_, '_>, site: &SiteDesc, q: ExId) -> Option<(ZonePred, bool)> {
    let (n, varno, _) = crate::ops::scan::pgrc::lower::qual_node(px, q)?;
    if let Some((attno, op, c)) = gravity_decompose::ops::shared::scan::bound_form(n, varno, &|_| true) {
        let k = site.lanes().iter().position(|l| l.id == LaneId::Attr(attno)).filter(|&k| k < site.natts())?;
        let l = site.lanes()[k];
        if l.kind != lane::KIND_BYVAL || matches!(l.rep, Rep::PackedNum { .. }) {
            return None;
        }
        return Some((ZonePred::bound(k as u16, op, const_word(px, q)?, c.as_const()?.constlen as u8), true));
    }
    if let Some(z) = byref_of(px, n, varno, site, q) {
        return Some((z, true));
    }
    pred_of(n, varno, site, px.gx.facts, px.mcx())
}

/// NEW-72: qual `n` (qual `q`'s node) as a compare over an order key whose by-reference constant is bound per execution,
/// its image never read here: a code lane's text compare where the zone bounds it ([`bytes_bound`]: under a collation
/// that orders by bytes, or its `=` / `<>` under any deterministic one, equal values being equal bytes, which a
/// byte-ordered zone, a part's code range and its length range bound), or a memcmp-ordered fixed image's compare
/// (P5-13). The host makes the key, and for a code lane places the image in each part's dictionary and reads its length,
/// from the execution's image ([`gravity_program::BOUND_TEXT_PREFIX`], `BOUND_FIXED_PREFIX`), as [`pred_of`] makes the
/// key from a literal's. Every constant alike, `''` too. A code lane's comparand may also be a generic plan's text
/// parameter, bound from its own word. `None` for any other qual, or a constant with no word ([`ref_word`]): [`pred_of`]
/// bakes it then.
fn byref_of<'p>(px: &Px<'_, 'p>, n: Node<'p>, varno: i32, site: &SiteDesc, q: ExId) -> Option<ZonePred> {
    let facts = px.gx.facts;
    let o = n.as_op_expr()?;
    let (k, zop, other) = compare_of(n, varno, site, &|a| facts.att(site.relation, a), px.mcx())?;
    let text = site.lanes()[k].kind == lane::KIND_CODES && text_zone_op(o.opno).is_some() && bytes_bound(facts, o.inputcollid, zop);
    if let Some(prm) = other.as_param().filter(|p| text && p.paramkind == ParamKind::PARAM_EXTERN) {
        let e = px.placed(q);
        let w = e.needs.iter().zip(e.places.iter()).find_map(|(need, place)| match (need, place) {
            (ParamNeed::Extern { id, .. }, Place::Word(w)) if *id == prm.paramid => Some(*w),
            _ => None,
        })?;
        return Some(ZonePred::bound_prefix(k as u16, zop, w, BOUND_TEXT_PREFIX));
    }
    other.as_const().filter(|c| !c.constisnull && !c.constbyval)?;
    let form = match fixed_lane(site, k, o.opno) {
        Some(len) if len <= 16 => BOUND_FIXED_PREFIX | len as u8,
        Some(_) => return None,
        None if text => BOUND_TEXT_PREFIX,
        None => return None,
    };
    Some(ZonePred::bound_prefix(k as u16, zop, ref_word(px, other)?, form))
}

/// Whether a text compare `zop` under collation `coll` is a byte-range zone predicate: `=` / `<>` need only equal bytes
/// for equal values (any deterministic collation: a byte-ordered zone or code range bounds byte equality), the order
/// compares the collation's order to be the bytes'. [`pred_of`] (a literal) and [`byref_of`] (the image bound per
/// execution) agree, so a literal the content key holds by its class is never baked (D4939).
fn bytes_bound(facts: &dyn Facts, coll: u32, zop: ZoneOp) -> bool {
    let c = facts.coll(coll);
    c.bytes_order || (c.bytes_eq && matches!(zop, ZoneOp::Eq | ZoneOp::Ne))
}

/// NEW-72: the word a by-reference literal `c` is bound to per execution: the place a compile of the filter gave it
/// (`Px::lit_place`), else the one the site offered before its roads declared ([`offer_words`]).
fn ref_word<'p>(px: &Px<'_, 'p>, c: Node<'p>) -> Option<u16> {
    match px.lit_place(c).or_else(|| px.offered_place(c))? {
        Place::Word(w) => Some(w),
        _ => None,
    }
}

/// NEW-72: before qual `q`'s roads declare, the word of its by-reference comparand where a road binds it per execution
/// and no compile placed it yet ([`byref_of`]'s and [`hash_of`]'s constants, a [`hash_list_of`] list's array): offered
/// (`Px::offer_ref_const`), so a later compile that places the literal takes the same word.
pub(crate) fn offer_words<'p>(px: &mut Px<'_, 'p>, site: &SiteDesc, q: ExId) {
    let Some((n, varno, _)) = crate::ops::scan::pgrc::lower::qual_node(px, q) else { return };
    let c = if let Some(s) = n.as_scalar_array_op_expr() {
        s.args.iter().nth(1).filter(|_| hash_list_of(px, site, q).is_some())
    } else {
        let facts = px.gx.facts;
        let o = n.as_op_expr();
        compare_of(n, varno, site, &|a| facts.att(site.relation, a), px.mcx()).filter(|&(k, ..)| {
            let Some(o) = o else { return false };
            let text = text_zone_op(o.opno).is_some() && bound_text_lane(site, k) && { let c = facts.coll(o.inputcollid); c.bytes_order || c.bytes_eq };
            fixed_lane(site, k, o.opno).is_some() || text
        }).map(|x| x.2)
    };
    if let Some(c) = c.filter(|c| c.as_const().is_some_and(|c| !c.constisnull && !c.constbyval)) {
        if px.lit_place(c).is_none() {
            px.offer_ref_const(c);
        }
    }
}

/// NEW-72: the word of qual `q`'s one constant as its compile placed it (a bound predicate's per-execution value), none
/// when it has another constant or the constant is no word.
fn const_word(px: &Px<'_, '_>, q: ExId) -> Option<u16> {
    let e = px.placed(q);
    if e.needs.len() != e.places.len() {
        return None;
    }
    let mut consts = e.needs.iter().zip(e.places.iter()).filter(|(n, _)| matches!(n, ParamNeed::Const(_)));
    match (consts.next(), consts.next()) {
        (Some((_, Place::Word(w))), None) => Some(*w),
        _ => None,
    }
}

/// Qual `q` as an IN list over a by-value lane of the site (`lane = ANY (constant array)` under a same-unit equality,
/// `useOr`; `zone_op` maps no cross-unit date / timestamp equality): the lane, the array's element facts (a by-value
/// type of 1, 2, 4 or 8 bytes) and the array Const. NEW-72 (an IN list is never baked): its members are
/// never read here; the site binds the array's word (`list_word`) and exec's prepare adds the members' span and the
/// members themselves per execution (protos `WORD_ZONE_LIST`), as this road declared them from the image before.
pub(crate) fn list_of<'p>(px: &Px<'_, 'p>, site: &SiteDesc, q: ExId) -> Option<(u16, ElemFacts, Node<'p>)> {
    let (n, varno, _) = crate::ops::scan::pgrc::lower::qual_node(px, q)?;
    let s = n.as_scalar_array_op_expr().filter(|s| s.useOr && s.args.len() == 2)?;
    if zone_op(s.opno) != Some(ZoneOp::Eq) {
        return None;
    }
    let k = lane_of(s.args.nth(0), varno, site, &|a| px.gx.facts.att(site.relation, a), px.mcx())?;
    let l = site.lanes()[k];
    let a = s.args.nth(1);
    let c = a.as_const().filter(|c| !c.constisnull && !c.constbyval && l.kind == lane::KIND_BYVAL && !matches!(l.rep, Rep::PackedNum { .. }))?;
    let ef = elem_facts(px.gx.catalog, c.consttype).ok().filter(|e| e.byval && [1, 2, 4, 8].contains(&e.len))?;
    Some((k as u16, ef, a))
}

/// P5-11: whether lane `k` of `site` holds text (a code or by-reference lane): the lanes a text constant's probe word
/// may name (the writer blooms the payload of a text column under a deterministic collation, dictionary-coded or not).
/// A jsonb path lane (sortpath (2), `path_lane_of`) is none: only its zone records are read.
fn text_lane(site: &SiteDesc, k: usize) -> bool {
    matches!(site.lanes()[k].kind, lane::KIND_CODES | lane::KIND_BYREF) && matches!(site.lanes()[k].id, LaneId::Attr(_))
}

/// sortpath (code-keyed zones): [`text_lane`], or a jsonb path's text code lane, whose compare [`byref_of`] binds per
/// execution so the host can place the constant in each part's dictionary (a baked one has its prefix key alone).
fn bound_text_lane(site: &SiteDesc, k: usize) -> bool {
    text_lane(site, k) || site.lanes()[k].kind == lane::KIND_CODES && site.shreds.iter().any(|d| d.kind == gravity_program::shred_kind::TEXT_CODES && d.lane as usize == k)
}

/// P5-11: qual `q` as `lane = c` over a lane whose equality is byte equality, as the bloom's `HashEq` predicate on c's probe
/// word (`eq_bloom_key` of its equality bytes, the writer's hash; the bytes are logged in the key, as SRC-09's prefix key's):
/// a text lane under a deterministic collation (c's payload), or P5-13 a memcmp-ordered fixed lane (c's image,
/// [`fixed_lane`]). `None` for any other qual, a NULL or a compressed constant.
pub(crate) fn hash_of(px: &Px<'_, '_>, site: &SiteDesc, q: ExId) -> Option<ZonePred> {
    let (n, varno, _) = crate::ops::scan::pgrc::lower::qual_node(px, q)?;
    let facts = px.gx.facts;
    let o = n.as_op_expr()?;
    let (k, zop, other) = compare_of(n, varno, site, &|a| facts.att(site.relation, a), px.mcx())?;
    let c = other.as_const().filter(|c| !c.constisnull)?;
    if zop != ZoneOp::Eq {
        return None;
    }
    let text = text_zone_op(o.opno) == Some(ZoneOp::Eq) && text_lane(site, k) && facts.coll(o.inputcollid).bytes_eq;
    // NEW-72: the constant's word, its probe word made by the host from the execution's image
    let form = match fixed_lane(site, k, o.opno) {
        Some(len) if len <= 16 => Some(BOUND_FIXED_HASH | len as u8),
        None if text && !c.constbyval => Some(BOUND_TEXT_HASH),
        _ => None,
    };
    if let Some(w) = form.and_then(|_| ref_word(px, other)) {
        return Some(ZonePred::bound(k as u16, ZoneOp::HashEq, w, form?));
    }
    let bytes = match fixed_lane(site, k, o.opno) {
        Some(len) => gravity_decompose::expr::const_fixed_bytes(c, len)?,
        None if text => gravity_decompose::expr::const_bytes(c)?,
        None => return None,
    };
    Some(ZonePred::hashed(k as u16, ZoneOp::HashEq, eq_bloom_key(bytes)))
}

/// NEW-72: qual `q` as an IN list over a lane whose equality is byte equality (P5-11's text lane under a deterministic
/// collation, P5-13's memcmp-ordered fixed lane): the lane, the array's element facts (a varlena text, or a fixed image of
/// the lane's length) and the array Const. Its members are never read here: exec's prepare adds each member's probe word
/// as the lane's `HashIn` predicate per execution (protos `WORD_ZONE_LIST_HASH`), which only the bloom answers.
pub(crate) fn hash_list_of<'p>(px: &Px<'_, 'p>, site: &SiteDesc, q: ExId) -> Option<(u16, ElemFacts, Node<'p>)> {
    let (n, varno, _) = crate::ops::scan::pgrc::lower::qual_node(px, q)?;
    let s = n.as_scalar_array_op_expr().filter(|s| s.useOr && s.args.len() == 2)?;
    let facts = px.gx.facts;
    let k = lane_of(s.args.nth(0), varno, site, &|a| facts.att(site.relation, a), px.mcx())?;
    let a = s.args.nth(1);
    let c = a.as_const().filter(|c| !c.constisnull && !c.constbyval)?;
    let ef = elem_facts(px.gx.catalog, c.consttype).ok().filter(|e| !e.byval)?;
    let ok = match fixed_lane(site, k, s.opno) {
        Some(len) => fixed_op(s.opno).is_some_and(|f| f.0 == ZoneOp::Eq) && ef.len == len as i16 && len <= 16,
        None => text_zone_op(s.opno) == Some(ZoneOp::Eq) && text_lane(site, k) && facts.coll(s.inputcollid).bytes_eq && ef.len == -1,
    };
    ok.then_some((k as u16, ef, a))
}

/// sortpath (4): qual `q` as an IN list of text over a jsonb path code lane (`path ->> 'k' IN ('a', 'b')`, `= ANY`)
/// under a collation that orders by bytes (SRC-09, as `pred_of`'s prefix compares; jsonb.lanes binds a path's compare
/// under no other, so `=` under a deterministic non-C collation is never a path zone predicate): the lane, the
/// array's element facts (a varlena) and the array Const. Its members are never read here: exec's prepare adds the
/// non-NULL members' span as two prefix predicates per execution (protos `WORD_ZONE_LIST_PREFIX`).
pub(crate) fn prefix_list_of<'p>(px: &Px<'_, 'p>, site: &SiteDesc, q: ExId) -> Option<(u16, ElemFacts, Node<'p>)> {
    let (n, varno, _) = crate::ops::scan::pgrc::lower::qual_node(px, q)?;
    let s = n.as_scalar_array_op_expr().filter(|s| s.useOr && s.args.len() == 2)?;
    let facts = px.gx.facts;
    let k = path_lane_of(s.args.nth(0), varno, site, px.mcx())?;
    let a = s.args.nth(1);
    let c = a.as_const().filter(|c| !c.constisnull && !c.constbyval)?;
    let ef = elem_facts(px.gx.catalog, c.consttype).ok().filter(|e| !e.byval && e.len == -1)?;
    let ok = text_zone_op(s.opno) == Some(ZoneOp::Eq) && facts.coll(s.inputcollid).bytes_order;
    ok.then_some((k as u16, ef, a))
}

/// NEW-72: the word list `list_of`'s array Const `a` is bound to per execution (its pointer, as a ctid list's: lower.rs
/// `tid_bounds`), none when the plan places no word for it. One word per literal occurrence (exec admit `template`: a
/// second word of one literal finds no occurrence left in the key's traversal and refuses the statement): the filter's
/// own word when its compile placed the array itself (a long list PostgreSQL does not hash, its array an operand,
/// step/expr_saop.rs `Arr::Sub`: c-perturb key 0397 (an int2 `IN` of 17 int4 members); `Px::lit_place`), else
/// a word offered to a later compile (`Px::offer_ref_const`), which one that places only the elements or the hashed set
/// leaves.
pub(crate) fn list_word<'p>(px: &mut Px<'_, 'p>, a: Node<'p>) -> Option<u16> {
    match px.lit_place(a).or_else(|| px.offer_ref_const(a))? {
        Place::Word(w) => Some(w),
        _ => None,
    }
}

/// PS1-07: qual `q` as an OR whose every arm is an AND (`decompose::ops::shared::scan::or_of_ands`: `x BETWEEN SYMMETRIC a
/// AND b`, two ORed ranges) of zone predicates that bound one lane from below and above: the hull of the arms' ranges,
/// `lane >= lo` and `lane <= hi` (lo the least of the arms' lower bounds, hi the greatest of their upper bounds; a strict
/// bound counts as its closed form, an equality as both). A row an arm passes lies inside that arm's bounds, so inside
/// the hull: a NoneMatch is sound, and the pair is never exact (`of` gives the OR none, so no AllMatch covers it, as an IN
/// list's bounds). The lane is the first every arm bounds both ways; an arm's conjuncts over other lanes, or that are no
/// zone predicate, bound nothing.
fn or_hull(n: Node<'_>, varno: i32, site: &SiteDesc, facts: &dyn Facts, mcx: Mcx<'_>) -> Option<[ZonePred; 2]> {
    if !or_of_ands(n) {
        return None;
    }
    // per lane, the hull so far: ((lane, prefix), lo, hi), kept only for lanes every arm bounds both ways (SRC-09: a prefix
    // key bounds as a word does, x >= c giving key(x) >= key(c), never mixed with a word)
    let mut hull: Option<PgVec<'_, ((u16, bool), i64, i64)>> = None;
    for arm in n.as_bool_expr()?.args.iter() {
        let mut bounds: PgVec<'_, ((u16, bool), Option<i64>, Option<i64>)> = PgVec::new_in(mcx);
        for c in arm.as_bool_expr()?.args.iter() {
            let Some((z, _)) = pred_of(c, varno, site, facts, mcx) else { continue };
            let (l, h) = match z.op {
                ZoneOp::Ge | ZoneOp::Gt => (true, false),
                ZoneOp::Le | ZoneOp::Lt => (false, true),
                ZoneOp::Eq => (true, true),
                _ => continue,
            };
            let i = bounds.iter().position(|b| b.0 == (z.lane, z.prefix)).unwrap_or_else(|| {
                bounds.push_in(((z.lane, z.prefix), None, None));
                bounds.len() - 1
            });
            let b = &mut bounds[i];
            if l {
                b.1 = Some(b.1.map_or(z.value, |x| x.max(z.value)));
            }
            if h {
                b.2 = Some(b.2.map_or(z.value, |x| x.min(z.value)));
            }
        }
        let arm: PgVec<'_, ((u16, bool), i64, i64)> = bounds.into_iter().filter_map(|(k, lo, hi)| Some((k, lo?, hi?))).collect_in(mcx);
        hull = Some(match hull {
            None => arm,
            Some(h) => h.into_iter().filter_map(|(k, lo, hi)| arm.iter().find(|a| a.0 == k).map(|a| (k, lo.min(a.1), hi.max(a.2)))).collect_in(mcx),
        });
    }
    let &((k, prefix), lo, hi) = hull?.first()?;
    let z: fn(u16, ZoneOp, i64) -> ZonePred = if prefix { ZonePred::prefix } else { ZonePred::word };
    Some([z(k, ZoneOp::Ge, lo), z(k, ZoneOp::Le, hi)])
}

/// D3009: qual `q` as an OR whose every arm implies a zone predicate over the site (the arm itself, or one conjunct of an
/// AND arm): a by-value word lane's `lane op c` ([`bound_form`], its constant bound per execution from the word its
/// compile placed, as [`of`] binds a top-level one: NEW-72, the program never holds the literal) or a `lane IS [NOT]
/// NULL`. The arms' predicates are one disjunction (`SiteDesc::zone_any`): a granule every one of them proves
/// `NoneMatch` holds no row of the OR. None when an arm implies none, or an arm's constant is not found as exactly one
/// placed word of the qual (two constants of one value: which place is which arm's is not known here).
pub(crate) fn any_of<'g>(px: &Px<'g, '_>, site: &SiteDesc, q: ExId) -> Option<PgVec<'g, ZonePred>> {
    use types_nodes::primnodes::BoolExprType;
    let (n, varno, _) = crate::ops::scan::pgrc::lower::qual_node(px, q)?;
    let b = n.as_bool_expr().filter(|b| b.boolop == BoolExprType::OR_EXPR)?;
    let e = px.placed(q);
    if e.needs.len() != e.places.len() {
        return None;
    }
    let word = |c: &types_nodes::primnodes::Const| {
        // the constant's word (`zone_word`: by value), matched against each placed constant's in the same zone order
        let v = zone_word(c)?;
        let mut at = e.needs.iter().zip(e.places.iter()).filter(|(nd, _)| matches!(nd, ParamNeed::Const(d) if !d.isnull && gravity_decompose::ops::shared::scan::zone_word_of(c.consttype, c.constlen, d.value.as_u64()) == Some(v)));
        match (at.next(), at.next()) {
            (Some((_, Place::Word(w))), None) => Some(*w),
            _ => None,
        }
    };
    let pred = |a: Node<'_>| -> Option<ZonePred> {
        if a.as_null_test().is_some() {
            return pred_of(a, varno, site, px.gx.facts, px.mcx()).map(|(z, _)| z);
        }
        let (attno, op, c) = gravity_decompose::ops::shared::scan::bound_form(a, varno, &|_| true)?;
        let k = site.lanes().iter().position(|l| l.id == LaneId::Attr(attno)).filter(|&k| k < site.natts())?;
        let l = site.lanes()[k];
        if l.kind != lane::KIND_BYVAL || matches!(l.rep, Rep::PackedNum { .. }) {
            return None;
        }
        let c = c.as_const()?;
        Some(ZonePred::bound(k as u16, op, word(c)?, c.constlen as u8))
    };
    let and = |a: Node<'_>| a.as_bool_expr().filter(|a| a.boolop == BoolExprType::AND_EXPR).and_then(|a| a.args.iter().find_map(pred));
    let mut arms: PgVec<'g, ZonePred> = PgVec::new_in(px.mcx());
    for a in b.args.iter() {
        arms.push_in(pred(a).or_else(|| and(a))?);
    }
    (arms.len() >= 2 && arms.len() < u16::MAX as usize).then_some(arms)
}

fn hull_of(px: &Px<'_, '_>, site: &SiteDesc, q: ExId) -> Option<[ZonePred; 2]> {
    let (n, varno, _) = crate::ops::scan::pgrc::lower::qual_node(px, q)?;
    or_hull(n, varno, site, px.gx.facts, px.mcx())
}

/// D289: qual `q` as `lane op $n` over a by-value lane and an extern parameter of a by-value type (a generic plan's
/// comparand, a key word): `word_zone_arg(lane, op, typlen)` and the parameter's bind word, the `src.verdicts` slot the
/// host resolves per execution (pgrc.rs). The same operators as a literal's predicate; never on the static site, so an
/// execution without the protocol adds nothing.
pub(crate) fn word_of(px: &Px<'_, '_>, site: &SiteDesc, q: ExId) -> Option<(u64, u16)> {
    let (n, varno, _) = crate::ops::scan::pgrc::lower::qual_node(px, q)?;
    let (k, zop, other) = compare_of(n, varno, site, &|a| px.gx.facts.att(site.relation, a), px.mcx())?;
    let prm = other.as_param().filter(|p| matches!(p.paramkind, ParamKind::PARAM_EXTERN))?;
    let tf = storage::of(px.gx.catalog, prm.paramtype).ok()?;
    // the host reads a word zone's 4-byte word sign-extended (granule_source set_word_zone): an oid's stays a literal's
    if site.lanes()[k].kind != lane::KIND_BYVAL || !tf.byval || ![1, 2, 4, 8].contains(&tf.len) || zero_extended(prm.paramtype) {
        return None;
    }
    let e = px.placed(q);
    let w = e.needs.iter().zip(e.places.iter()).find_map(|(need, place)| match (need, place) {
        (ParamNeed::Extern { id, .. }, Place::Word(w)) if *id == prm.paramid => Some(*w),
        _ => None,
    })?;
    Some((word_zone_arg(k as u16, zop as u8, tf.len as u8), w))
}

/// PS1-05 (road `nest.eagersafe`, face/consts.rs `RtConst::InitPlan`): qual `q` as `lane op (SELECT ..)` over a
/// by-value lane and the PARAM_EXEC of an EAGER-SAFE InitPlan of a by-value type: `(lane, op, typlen, paramid)`. Eager-safe
/// is D2875's law: effects/visible/pgrc.rs `init_unseen`, every InitPlan setting the
/// parameter is a non-CTE one that neither acts nor may raise (decompose F5, a second row's 21000 included), so running it
/// at the scan's open, where PostgreSQL might never run it (no row reaches the qual), shows nothing. The source's open
/// stores its value in a key cell (nest.initplan's `InitLoad`, as an index site's InitPlan key) that the host reads at the
/// seek (src.verdicts' key-cell slot, protos `word_zone_key_arg`). Any other InitPlan stays lazy: the filter runs it at the
/// first row that reaches the qual, as PostgreSQL does.
///
/// D3008: also `lane op $p` over a by-value PARAM_EXEC its producer sets before it (re-)opens the plan holding this scan
/// (`rerun::params::set_before_open`: a nest's NestLoopParam, a SubPlan's argument): a correlated subquery's or a
/// LATERAL / parameterized nest inner's comparand. Its open reads the word the producer wrote (`OpenParam::Exec`), so
/// each re-open for a new outer row seeks with that row's value; no InitPlan runs and nothing can raise.
pub(crate) fn init_of(px: &Px<'_, '_>, site: &SiteDesc, q: ExId) -> Option<(u16, u8, u8, i32, OpenParam)> {
    let (n, varno, _) = crate::ops::scan::pgrc::lower::qual_node(px, q)?;
    let (k, zop, other) = compare_of(n, varno, site, &|a| px.gx.facts.att(site.relation, a), px.mcx())?;
    let prm = other.as_param().filter(|p| matches!(p.paramkind, ParamKind::PARAM_EXEC))?;
    let tf = storage::of(px.gx.catalog, prm.paramtype).ok()?;
    // the host reads a key cell's word sign-extended (granule_source `KeyZone::pred`), as D289's bind word (`word_of`): an
    // oid's (zero-extended in its zones) stays the filter's, whether an InitPlan or the producer sets it (D3536)
    if site.lanes()[k].kind != lane::KIND_BYVAL || !tf.byval || ![1, 2, 4, 8].contains(&tf.len) || zero_extended(prm.paramtype) {
        return None;
    }
    let from = match crate::rerun::params::producer(px.gx, prm.paramid) {
        Some(crate::face::kinds::KindId::NestInitplan) if crate::effects::visible::pgrc::init_unseen(px.gx, prm.paramid) => OpenParam::Init(gravity_decompose::effects::unobservable(gravity_decompose::effects::Effects::NONE)?),
        Some(crate::face::kinds::KindId::NestParam | crate::face::kinds::KindId::NestSubplan) if crate::rerun::params::set_before_open(px.gx, prm.paramid) => OpenParam::Exec,
        _ => return None,
    };
    Some((k as u16, zop as u8, tf.len as u8, prm.paramid, from))
}

/// Where an open-time zone predicate's comparand comes from (`init_of`, `stable_of`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpenParam {
    /// An eager-safe InitPlan's result, run at the open if this context has not (PS1-05, nest.initplan's `InitLoad`):
    /// the licence of that run ahead of the row PostgreSQL first reads the parameter on, whose quiet InitPlans show
    /// nothing there (effects/visible/pgrc.rs `init_unseen`).
    Init(gravity_decompose::effects::Licence),
    /// D3008: a parameter its producer wrote before this open (`rerun::params::load`).
    Exec,
    /// D2992: subtree `node` of qual `q`'s program, evaluated at the open (`stable_of`); `cross` a [`cross_of`] kind (0:
    /// the lane's own units) and `op` the qual's comparison, which the kind's conversion reads. D3126: `guarded`, the qual
    /// follows one that may raise in its own open-time comparand (lower.rs `declare_verdicts`): no predicate when an
    /// earlier comparand raised at this open. Construct C2: `licence`, the held-error licence of that evaluation
    /// ahead of the rows PostgreSQL computes it on (its error leaves no predicate, the row filter raising where
    /// PostgreSQL does); a comparand calling user code that may write (`effects::user_code_may_write`) is none.
    Stable { q: ExId, node: gravity_expr::NodeId, cross: u8, op: u8, guarded: bool, licence: gravity_decompose::effects::Licence },
}

/// D2992: the cross-unit datetime comparisons `stable_of` takes, by (lane type, comparand type), with the lane on the left:
/// date vs timestamp, date vs timestamptz, timestamp vs date and timestamptz vs date (typesem `date_cross_cmp`). The kind
/// (`CROSS_*`) is the one kernels `zone_cross_bound` converts by; `None` for any other operator. Timestamp vs timestamptz is
/// none: its local-time conversion is not monotone across a daylight-saving gap.
pub(crate) fn cross_of(opno: u32, flipped: bool) -> Option<(u8, ZoneOp)> {
    use gravity_kernels::ops::scan::entries::{CROSS_DATE_TS, CROSS_DATE_TSTZ, CROSS_TSTZ_DATE, CROSS_TS_DATE};
    const OPS: [ZoneOp; 6] = [ZoneOp::Lt, ZoneOp::Le, ZoneOp::Eq, ZoneOp::Ge, ZoneOp::Gt, ZoneOp::Ne];
    const KINDS: [u8; 4] = [CROSS_DATE_TS, CROSS_DATE_TSTZ, CROSS_TS_DATE, CROSS_TSTZ_DATE];
    let (pair, at) = gravity_fn::typesem::expr::date_cross_cmp(opno)?;
    let kind = KINDS[pair as usize];
    // with the lane on the right the operator's left type is the comparand's: date vs timestamp read from the timestamp
    let kind = match (flipped, kind) {
        (false, k) => k,
        (true, CROSS_DATE_TS) => CROSS_TS_DATE,
        (true, CROSS_DATE_TSTZ) => CROSS_TSTZ_DATE,
        (true, CROSS_TS_DATE) => CROSS_DATE_TS,
        (true, _) => CROSS_DATE_TSTZ,
    };
    let op = match (flipped, OPS[at as usize]) {
        (true, ZoneOp::Lt) => ZoneOp::Gt,
        (true, ZoneOp::Le) => ZoneOp::Ge,
        (true, ZoneOp::Gt) => ZoneOp::Lt,
        (true, ZoneOp::Ge) => ZoneOp::Le,
        (_, o) => o,
    };
    (op != ZoneOp::Ne).then_some((kind, op))
}

/// D2992 (a bound such as `shipment_date >= current_date - interval '12 months'`): qual `q` as `lane op e` over a
/// by-value lane, where `e` reads no column, no executor parameter, no SubPlan and no VOLATILE or acting call
/// (literals, extern parameters, IMMUTABLE and STABLE calls, GREATEST / LEAST, COALESCE): `(lane, op, typlen, 0,
/// OpenParam::Stable)`. The source's open evaluates `e` once from the qual's own program (lower.rs `izone_keys`), where
/// PostgreSQL computes it per row: a STABLE value holds for the statement. An error or NULL leaves no predicate, so the
/// row filter raises where PostgreSQL does. Construct C2: that is the held error's licence, which `e` calling user code
/// that may write is denied (none: only the row filter calls it, per row as PostgreSQL does). `op` is a same-unit zone
/// operator (the comparand's typlen read) or a [`cross_of`] one, whose value the open converts into the lane's units
/// exactly (the declared operator `>=` for `>` / `>=`, `<=` for `<` / `<=`, the lane's typlen read).
pub(crate) fn stable_of(px: &Px<'_, '_>, site: &SiteDesc, q: ExId) -> Option<(u16, u8, u8, i32, OpenParam)> {
    use gravity_expr::nodes::ExprNode;
    let (n, varno, _) = crate::ops::scan::pgrc::lower::qual_node(px, q)?;
    let o = n.as_op_expr().filter(|o| o.args.len() == 2)?;
    let att = |a: i16| px.gx.facts.att(site.relation, a);
    let (k, flipped) = match (lane_of(o.args.nth(0), varno, site, &att, px.mcx()), lane_of(o.args.nth(1), varno, site, &att, px.mcx())) {
        (Some(k), None) => (k, false),
        (None, Some(k)) => (k, true),
        _ => return None,
    };
    if site.lanes()[k].kind != lane::KIND_BYVAL {
        return None;
    }
    let (l, r) = px.gx.catalog.op_input_types(o.opno).ok()?;
    let (lane_t, other_t) = if flipped { (r, l) } else { (l, r) };
    let (zop, cross, len) = match cross_of(o.opno, flipped) {
        Some((kind, op)) => (op, kind, storage::of(px.gx.catalog, lane_t).ok().filter(|t| t.byval)?.len),
        None => {
            let (_, zop, _) = compare_of(n, varno, site, &att, px.mcx()).filter(|c| zone_op(o.opno).is_some() && c.0 == k)?;
            (zop, 0, storage::of(px.gx.catalog, other_t).ok().filter(|t| t.byval && !zero_extended(other_t))?.len)
        }
    };
    if ![2, 4, 8].contains(&len) {
        return None;
    }
    // the comparand in the qual's program: the root comparison's argument on the other side
    let e = px.placed(q);
    let p = &e.c.prog;
    let root = e.c.root;
    if !matches!(p.nodes[root.0 as usize], ExprNode::Op(_) | ExprNode::Func(_)) {
        return None;
    }
    let kids = gravity_expr::emit::kids(p, root).slice()?;
    let &node = kids.get(!flipped as usize).filter(|_| kids.len() == 2)?;
    fn stable(px: &Px<'_, '_>, p: &gravity_expr::nodes::Program<'_>, id: gravity_expr::NodeId) -> bool {
        crate::stack_check();
        if p.islands.iter().any(|i| i.root == id) {
            return false;
        }
        let ok = match &p.nodes[id.0 as usize] {
            ExprNode::Const(_) | ExprNode::Imm(_) | ExprNode::MinMax(_) | ExprNode::Coalesce(_) | ExprNode::Relabel(_) => true,
            ExprNode::Param(q) => q.kind == ParamKind::PARAM_EXTERN,
            ExprNode::Func(f) | ExprNode::Op(f) => {
                let c = &p.sites[f.site.0 as usize];
                c.funcid != 0 && !c.volatile && !gravity_decompose::effects::of_call(px.gx.facts, c.funcid).acts
            }
            _ => false,
        };
        ok && gravity_expr::emit::kids(p, id).into_iter().all(|k| stable(px, p, k))
    }
    // a bare literal or extern parameter is a static or D289 predicate already: only a computed comparand is this road's
    let computed = !matches!(p.nodes[node.0 as usize], ExprNode::Const(_) | ExprNode::Imm(_) | ExprNode::Param(_));
    if e.volatile || e.ext.is_some() || !e.held.is_empty() || !computed && cross == 0 || !stable(px, p, node) {
        return None;
    }
    // construct C2: a call of user code that may write in the comparand (its function, an operator's, GREATEST /
    // LEAST's comparison) denies the held error's licence: no predicate, the row filter calls it where PostgreSQL does
    fn writes(px: &Px<'_, '_>, p: &gravity_expr::nodes::Program<'_>, id: gravity_expr::NodeId) -> bool {
        crate::stack_check();
        let site = match &p.nodes[id.0 as usize] {
            ExprNode::Func(f) | ExprNode::Op(f) => Some(f.site),
            ExprNode::MinMax(m) => Some(m.cmp),
            _ => None,
        };
        let call = |s: gravity_expr::SiteId| gravity_decompose::effects::user_code_may_write(px.gx.facts, p.sites[s.0 as usize].funcid);
        site.is_some_and(call) || gravity_expr::emit::kids(p, id).into_iter().any(|k| writes(px, p, k))
    }
    let licence = gravity_decompose::effects::held_error(gravity_decompose::effects::Effects::RAISES, writes(px, p, node), None).ok()?;
    let declared = match zop {
        ZoneOp::Gt if cross != 0 => ZoneOp::Ge,
        ZoneOp::Lt if cross != 0 => ZoneOp::Le,
        z => z,
    };
    Some((k as u16, declared as u8, len as u8, 0, OpenParam::Stable { q, node, cross, op: zop as u8, guarded: false, licence }))
}

impl VerdictMech for Zone {
    /// The qual's predicate added to the site (one per distinct predicate, as many as the quals give): `true` when the
    /// host answers it.
    fn declare(&self, px: &mut Px<'_, '_>, site: &mut SiteDesc, qual: ExId) -> bool {
        let Some((z, _)) = of(px, site, qual) else {
            // NEW-72: an IN list's span is the execution's (lower.rs declares its list slot)
            if list_of(px, site, qual).is_some() || prefix_list_of(px, site, qual).is_some() {
                return true;
            }
            // an OR of ranges (PS1-07): both bounds or neither; else (D3009) an OR whose arms each imply a predicate, as one
            // disjunction
            let Some(zs) = hull_of(px, site, qual) else {
                let Some(arms) = any_of(px, site, qual) else { return false };
                site.add_zone_any(&arms[..]);
                return true;
            };
            let new: PgVec<'_, ZonePred> = zs.into_iter().filter(|z| !site.zone_preds().contains(z)).collect_in(px.mcx());
            new.into_iter().for_each(|z| site.add_zone(z));
            return true;
        };
        if site.zone_preds().contains(&z) {
            return true;
        }
        site.add_zone(z);
        filter_only(px, site, qual, z.lane as usize);
        true
    }
}

/// NUMZONE: a zone predicate's lane that decompose found read by quals alone (`qual_only`: e.g. a numeric the
/// decomposer's zones never took) is filter-only, as sources::zone marks its own: an AllMatch window or a hit elides it.
fn filter_only(px: &Px<'_, '_>, site: &mut SiteDesc, q: ExId, l: usize) {
    let Some((_, _, k)) = crate::ops::scan::pgrc::lower::qual_node(px, q) else { return };
    let (p, LaneId::Attr(a)) = (&px.gx.pipelines()[k], site.lanes()[l].id) else { return };
    let Source::Range { relid, .. } = &p.source else { return };
    if px.gx.graph.qual_only.iter().any(|e| e.node_id == p.node_id && e.relid == *relid && e.attno == a) {
        site.set_filter_only(l);
    }
}

#[cfg(test)]
mod tests {
    use gravity_decompose::ops::shared::sort::KeyImage;

    use super::*;

    /// SRC-07: compare_of's operator map (zone_op, then numeric_op, then float_op) takes none of the cross-unit datetime
    /// comparisons (pg_operator: date vs timestamp 2345..2350, date vs timestamptz 2358..2363, timestamp vs date
    /// 2371..2376, timestamptz vs date 2384..2389, timestamp vs timestamptz 2534..2539 and back 2540..2545), and every
    /// same-type int4 / date / timestamp / timestamptz one.
    #[test]
    fn cross_unit_operators_map_to_no_zone_comparison() {
        let map = |o: u32| zone_op(o).or(numeric_op(o)).or(float_op(o));
        for o in (2345..=2350).chain(2358..=2363).chain(2371..=2376).chain(2384..=2389).chain(2534..=2545) {
            assert_eq!(map(o), None, "{o}");
        }
        assert!([96, 1093, 2060, 1320].iter().all(|&o| map(o) == Some(ZoneOp::Eq)));
    }

    /// D2992: `cross_of` takes the four date vs timestamp(tz) operator groups both ways round (the lane on the right
    /// turns the operator and reads the kind from the lane's side), never `<>` or timestamp vs timestamptz; the kernels
    /// conversion reads `ZoneOp`'s discriminants as Lt 0 .. Gt 4.
    #[test]
    fn cross_unit_operators_map_for_the_open_time_bound() {
        use gravity_kernels::ops::scan::entries::{CROSS_DATE_TS, CROSS_DATE_TSTZ, CROSS_TSTZ_DATE, CROSS_TS_DATE};
        assert_eq!([ZoneOp::Lt, ZoneOp::Le, ZoneOp::Eq, ZoneOp::Ge, ZoneOp::Gt].map(|z| z as u8), [0, 1, 2, 3, 4]);
        assert_eq!(cross_of(2348, false), Some((CROSS_DATE_TS, ZoneOp::Ge))); // date >= timestamp
        assert_eq!(cross_of(2345, true), Some((CROSS_TS_DATE, ZoneOp::Gt))); // date < timestamp-lane: lane > date
        assert_eq!(cross_of(2362, false), Some((CROSS_DATE_TSTZ, ZoneOp::Gt)));
        assert_eq!(cross_of(2373, false), Some((CROSS_TS_DATE, ZoneOp::Eq)));
        assert_eq!(cross_of(2386, true), Some((CROSS_DATE_TSTZ, ZoneOp::Eq)));
        assert_eq!(cross_of(2385, false), Some((CROSS_TSTZ_DATE, ZoneOp::Le)));
        assert!([2350, 2363, 2376, 2389].iter().all(|&o| cross_of(o, false).is_none() && cross_of(o, true).is_none()));
        assert!((2534..=2545).chain([96, 1093, 2060]).all(|o| cross_of(o, false).is_none()));
    }

    /// FLOATZONE: each float operator family maps by its pg_operator names, and the constant keys order as PG's floats
    /// (-0 = 0, NaN one value above Infinity; a float4 constant widened exactly, so `0.1::float4 > 0.1::float8`).
    #[test]
    fn float_operators_and_constant_keys_follow_pg_float_order() {
        use ZoneOp::{Eq, Ge, Gt, Le, Lt, Ne};
        let names = [(620, Eq), (621, Ne), (622, Lt), (623, Gt), (624, Le), (625, Ge), (670, Eq), (671, Ne), (672, Lt), (673, Le), (674, Gt), (675, Ge)];
        for (o, z) in names {
            assert_eq!(float_op(o), Some(z), "{o}");
        }
        for base in [1120, 1130] {
            assert_eq!((0..6).map(|i| float_op(base + i)).collect::<Vec<_>>(), [Eq, Ne, Lt, Gt, Le, Ge].map(Some).to_vec());
        }
        assert!([619, 626, 669, 676, 1119, 1126, 1129, 1136].iter().all(|&o| float_op(o).is_none()));
        let k8 = |v: f64| float_key(701, v.to_bits() as i64).unwrap();
        let k4 = |v: f32| float_key(700, v.to_bits() as i32 as i64).unwrap();
        let rising = [k8(f64::NEG_INFINITY), k8(-1.5), k8(-0.0), k8(1e-300), k8(1.5), k8(f64::INFINITY), k8(f64::NAN)];
        assert!(rising.windows(2).all(|w| w[0] < w[1]), "{rising:?}");
        assert_eq!((k8(-0.0), k8(-f64::NAN), k4(-0.0), k4(f32::NAN)), (k8(0.0), k8(f64::NAN), k8(0.0), k8(f64::NAN)));
        assert_eq!(k4(-2.5), k8(-2.5));
        assert!(k4(0.1) > k8(0.1));
        assert_eq!(float_key(20, 0), None);
    }

    /// P5-13: `fixed_op` maps the btree operators of uuid (pg_operator 2972-2977), macaddr (1220-1225) and macaddr8
    /// (3362-3367) by strategy with the type's image length, `<>` as `=`'s negator, and no operator of a fixed image whose
    /// order is not its bytes' (interval `=` 1330, timetz `=` 1550), of text (`=` 98) or of int8 (`=` 410).
    #[test]
    fn memcmp_fixed_operators_map_by_strategy() {
        use ZoneOp::*;
        let ops = |eq: u32, ne: u32, lt: u32, le: u32, gt: u32, ge: u32| [(eq, Eq), (ne, Ne), (lt, Lt), (le, Le), (gt, Gt), (ge, Ge)];
        for (len, row) in [(16, ops(2972, 2973, 2974, 2976, 2975, 2977)), (6, ops(1220, 1221, 1222, 1223, 1224, 1225)), (8, ops(3362, 3363, 3364, 3365, 3366, 3367))] {
            for (opno, op) in row {
                assert_eq!(fixed_op(opno), Some((op, len)), "operator {opno}");
            }
        }
        assert!([1330, 1550, 98, 410, 0].iter().all(|&o| fixed_op(o).is_none()));
        assert_eq!(fixed_order_key(&[0x08, 0, 0x2b, 1, 2, 3]), (0x0800_2b01_0203_0000u64 ^ (1 << 63)) as i64, "a macaddr's whole image, big-endian, zero-padded");
        let (a, b) = ([0x12u8; 16], { let mut b = [0x12u8; 16]; b[15] = 0x13; b });
        assert_eq!(fixed_order_key(&a), fixed_order_key(&b), "a uuid's key is its 8-byte prefix: a tie past byte 8");
        assert!(fixed_order_key(&[0u8; 16]) < fixed_order_key(&[0x80u8; 16]) && fixed_order_key(&[0x7fu8; 16]) < fixed_order_key(&[0x80u8; 16]));
    }

    /// A numeric payload as PostgreSQL stores `n / 10^dscale` (`make_result`: base-10000 digits, leading and trailing zero
    /// digits stripped), in the short header form, or the long one (`long`).
    fn payload(n: i128, dscale: u32, long: bool) -> Vec<u8> {
        let a = n.unsigned_abs() * 10u128.pow((4 - dscale % 4) % 4); // the fraction padded to whole base-10000 digits
        let fd = dscale.div_ceil(4) as i32; // fraction digits
        let mut d: Vec<u16> = Vec::new(); // alloc: Cold
        let mut x = a;
        while x > 0 {
            d.insert(0, (x % 10_000) as u16);
            x /= 10_000;
        }
        let w = if d.is_empty() { 0 } else { d.len() as i32 - 1 - fd }; // the first digit's weight
        while d.last() == Some(&0) {
            d.pop();
        }
        let neg = n < 0;
        let mut b = if long {
            let mut v = (((neg as u16) << 14) | dscale as u16).to_ne_bytes().to_vec(); // alloc: Cold
            v.extend_from_slice(&(w as i16).to_ne_bytes());
            v
        } else {
            let h: u16 = 0x8000 | if neg { 0x2000 } else { 0 } | ((dscale as u16) << 7) | if w < 0 { 0x40 | (w & 0x3F) as u16 } else { w as u16 };
            h.to_ne_bytes().to_vec() // alloc: Cold
        };
        d.iter().for_each(|x| b.extend_from_slice(&x.to_ne_bytes()));
        b
    }

    /// NUMZONE (V4R-28): every constant n / 10^3 against every mantissa m at scale 2, each operator: `m op' v` holds exactly when
    /// m / 100 op n / 1000 does (constants at, finer than and coarser than the scale, negative, zero), `=` / `<>` with a finer
    /// constant and NaN / Infinity refuse, a value past i64 at the scale refuses.
    #[test]
    fn numeric_bounds_are_the_comparison_on_mantissas() {
        let ops = [ZoneOp::Eq, ZoneOp::Ne, ZoneOp::Lt, ZoneOp::Le, ZoneOp::Gt, ZoneOp::Ge];
        let holds = |op: ZoneOp, a: i128, b: i128| match op {
            ZoneOp::Eq => a == b,
            ZoneOp::Ne => a != b,
            ZoneOp::Lt => a < b,
            ZoneOp::Le => a <= b,
            ZoneOp::Gt => a > b,
            _ => a >= b,
        };
        for n in (-3_000i128..=3_000).step_by(7).chain([0, 50, -50, 5, -5, 1, -1, 999, -999]) {
            for (dscale, long) in [(3, false), (3, true), (4, false)] {
                let n = n * if dscale == 4 { 10 } else { 1 }; // the same value at dscale 4 (a trailing zero)
                let pay = payload(n, dscale, long);
                for op in ops {
                    let got = mantissa_bound(&pay, 2, op);
                    let finer = n % if dscale == 4 { 100 } else { 10 } != 0;
                    if finer && matches!(op, ZoneOp::Eq | ZoneOp::Ne) {
                        assert_eq!(got, None, "{n}e-{dscale} {op:?}");
                        continue;
                    }
                    let v = got.unwrap_or_else(|| panic!("{n}e-{dscale} {op:?}")) as i128;
                    for m in -400i128..=400 {
                        // m / 100 op n / 10^dscale  <=>  m * 10^(dscale - 2) op n
                        assert_eq!(holds(op, m, v), holds(op, m * 10i128.pow(dscale - 2), n), "m {m} {op:?} {n}e-{dscale} (v {v})");
                    }
                }
            }
        }
        assert_eq!(mantissa_bound(&payload(24, 0, false), 2, ZoneOp::Lt), Some(2400));
        assert_eq!(mantissa_bound(&payload(1, 10, false), 2, ZoneOp::Lt), Some(1), "1e-10: m < ceil");
        assert_eq!(mantissa_bound(&payload(-1, 10, false), 2, ZoneOp::Ge), Some(0), "-1e-10: m >= ceil");
        assert_eq!(mantissa_bound(&payload(10i128.pow(20), 0, true), 2, ZoneOp::Lt), None, "past i64 at the scale");
        assert_eq!(mantissa_bound(&[0x00, 0xC0], 2, ZoneOp::Lt), None, "NaN");
        assert_eq!(mantissa_bound(&[0x00, 0xD0], 2, ZoneOp::Gt), None, "Infinity");
        assert_eq!((numeric_op(1754), numeric_op(1751), numeric_op(1758)), (Some(ZoneOp::Lt), None, None));
    }
}

/// V4R-19 (A): `(site lane, width)` of pipeline `k`'s one group key when its run carry may fold a constant-key window as
/// one run: the pipeline's ops are filters and projections into one group (no grouping-sets arm, no two passes) keyed by
/// one plain scan column of a signed by-value word type (int2 / int4 / int8 / date / timestamp / timestamptz), whose exact
/// zone key and image are the datum word itself (pgrc_meta `signed_word_key`, decompose `KeyImage::Raw`).
pub(crate) fn const_key(px: &Px<'_, '_>, k: usize, site: &SiteDesc) -> Option<(usize, u8)> {
    let (gx, p) = (px.gx, &px.gx.pipelines()[k]);
    let Source::Range { scanrelid, .. } = &p.source else { return None };
    let g = gx.group_at(k).filter(|g| g.set.is_none() && g.two_pass.is_none() && OrderUse::Collapse.on())?;
    let ([key], true) = (&g.keys[..], p.ops.iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_)))) else { return None };
    let word = key.image.is_raw() && matches!(key.kind, KeyKind::I32 | KeyKind::I64 | KeyKind::Date | KeyKind::Timestamp);
    let v = key.expr.node().as_var().filter(|v| word && v.varno == *scanrelid as i32 && v.varlevelsup == 0)?;
    let signed = gravity_fn::typesem::pgrc::signed_word(v.vartype);
    let l = site.lanes().iter().position(|l| signed && l.id == LaneId::Attr(v.varattno) && l.kind == lane::KIND_BYVAL)?;
    Some((l, site.lanes()[l].width))
}

/// `WinConds::const_key` of the window whose lanes are at `lanes`: (the key lane's `aux` is set, the key's datum word, its
/// element 0 sign-extended from `width`, loaded only in a constant window; 0 in any other).
pub(crate) fn const_key_window(b: &mut FunctionBuilder<'_>, env: &Env, lanes: Value, l: usize, width: u8) -> (Value, Value) {
    let a = b.load_zext(Type::I32, lanes, (l * lane::SIZE + lane::AUX) as i32);
    let on = b.icmp(IntCond::Ne, a, env.zero);
    let (l_load, l_done) = (b.create_block(&[]), b.create_block(&[Type::I64]));
    b.cond_br(on, (l_load, &[]), (l_done, &[env.zero]));
    b.switch_to(l_load);
    let ptr = b.load(Type::Ptr, lanes, (l * lane::SIZE + lane::PTR) as i32);
    let ty = match width { 1 => Type::I8, 2 => Type::I16, 4 => Type::I32, _ => Type::I64 };
    let v = b.load(ty, ptr, 0);
    let d = if ty == Type::I64 { v } else { b.conv(ConvOp::Sext, Type::I64, v) };
    b.br(l_done, &[d]);
    b.switch_to(l_done);
    (on, b.block_params(l_done)[0])
}
