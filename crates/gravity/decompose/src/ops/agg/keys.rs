//! Key equivalence: group keys stripped, equated and typed (split out of `ops/agg/mod.rs`, v5 agg A2; bound for typesem).
use super::*;
use crate::catalog::expr_type::type_of_listed;

pub(super) fn outer_expr<'p>(child: &Open<'p>, resno: i16) -> Option<Node<'p>> {
    let tlist = child.ops.iter().rev().find_map(|op| match op {
        Op::Project(l) => Some(*l),
        _ => None,
    })?;
    let n = (resno as usize).checked_sub(1)?;
    if n >= tlist.len() {
        return None;
    }
    let t = tlist.nth(n).as_target_entry()?;
    (t.resno == resno).then_some(t.expr)
}

impl<'p> Stripped<'p> {
    /// A child projection entry, as it is.
    pub fn entry(e: Node<'p>) -> Stripped<'p> {
        Stripped { node: relabels(e), agg: false, bpchar: relabelled_bpchar(e), retype: relabelled_type(e) }
    }

    pub fn node(self) -> Node<'p> {
        self.node
    }

    pub fn agg(self) -> bool {
        self.agg
    }

    /// D1309: the key is bpchar only through a stripped relabel (`v::bpchar` over a varchar / text value).
    pub fn bpchar(self) -> bool {
        self.bpchar
    }

    pub(super) fn arg(self, child: &Open<'p>, a: Node<'p>) -> Stripped<'p> {
        if self.agg { strip(child, a) } else { Stripped::entry(a) }
    }
}

pub fn strip<'p>(child: &Open<'p>, e: Node<'p>) -> Stripped<'p> {
    let (bpchar, retype) = (relabelled_bpchar(e), relabelled_type(e));
    let e = relabels(e);
    match e.as_var().filter(|v| v.varno == OUTER_VAR && v.varlevelsup == 0).and_then(|v| outer_expr(child, v.varattno)) {
        // the outermost relabel's type wins: the reference's own, else the projection entry's
        Some(o) => Stripped { bpchar: bpchar || relabelled_bpchar(o), retype: retype.or(relabelled_type(o)), ..Stripped::entry(o) },
        None => Stripped { node: e, agg: true, bpchar, retype },
    }
}

/// D1309: `e` is bpchar only through a relabel its stripped form loses (text -> bpchar is binary coercible: a RelabelType).
pub(super) fn relabelled_bpchar(e: Node<'_>) -> bool {
    crate::catalog::expr_type::type_of(e) == Some(BPCHAROID) && crate::catalog::expr_type::type_of(relabels(e)) != Some(BPCHAROID)
}

/// D2832: the type `e` has through a relabel its stripped form does not (`c::text` over a citext column: citext -> text is
/// binary coercible, a RelabelType), `None` when no relabel changes the type. PostgreSQL's grouping, hashing and set
/// operators compare the expression's own type, so a key is typed by this one (D1309's bpchar is its first case).
pub(super) fn relabelled_type(e: Node<'_>) -> Option<u32> {
    let t = crate::catalog::expr_type::type_of(e)?;
    (crate::catalog::expr_type::type_of(relabels(e)) != Some(t)).then_some(t)
}

pub(super) const BPCHAROID: u32 = 1042;

/// D1353 (LIVE WRONG, CORRECTNESS P20 k159): a key that is bpchar only through a relabel of a varchar / text value
/// (`f.b = d.v::bpchar`: a semi-join's unique-ification and its Hash Join counted an outer row once per trim-equal inner
/// value, heap and pgrcolumnar) refuses by name as a catalog lookup's key (join.rs); a hash, semi or anti join keys it as
/// bpchar (jcore-0037: lowerv4 hashjoin images the clause's type, trimmed; a code lane of another type refuses there by
/// this name, keys_bpchar.rs); a standalone GROUP BY / DISTINCT over the relabel keeps D1309's bpchar typing and stays served.
pub const RELABEL_BPCHAR: &str = "D1353: a key that is bpchar only through a relabel of a varchar / text value";

/// D2343: expression `key` reads column `col` (a Var of the same level) below its top, so the key is a function of it.
pub(super) fn computes_from(key: Node<'_>, col: Node<'_>) -> bool {
    let Some(v) = relabels(col).as_var() else { return false };
    if relabels(key).as_var().is_some() {
        return false;
    }
    let mut hit = false;
    expr::walk(key, &mut |x| {
        hit |= x.as_var().is_some_and(|w| (w.varno, w.varattno, w.varlevelsup) == (v.varno, v.varattno, v.varlevelsup));
        !hit
    });
    hit
}

pub(super) fn scan_lane<'p>(b: &Builder<'p, '_>, child: &Open<'p>, e: Stripped<'p>) -> Option<LaneDesc> {
    let attno = scan_attno(b, child, e.node)?;
    let (_, _, site) = child.source.relation()?;
    site.lanes().iter().copied().find(|l| l.id == LaneId::Attr(attno))
}

/// D384: `x` and `y` are one expression over the input: the same column (`same_var`), or the same fingerprinted
/// expression tree over one range-table index (`facts::expr_fp`).
pub(super) fn same_expr<'p>(b: &Builder<'p, '_>, child: &Open<'p>, x: Stripped<'p>, y: Stripped<'p>) -> bool {
    if same_var(b, child, x, y) {
        return true;
    }
    if x.agg != y.agg {
        return false;
    }
    let mut varno = None;
    expr::walk(x.node, &mut |n| {
        if let Some(v) = n.as_var() {
            varno.get_or_insert(v.varno);
        }
        varno.is_none()
    });
    let Some(v) = varno else { return false };
    matches!((crate::catalog::expr_fp(x.node, v), crate::catalog::expr_fp(y.node, v)), (Some(p), Some(q)) if p == q)
}

/// D345: whether every column of `pins` (input columns read outside the aggregates of a GroupAggregate whose every key
/// the planner dropped) has one image over the node's rows, so keyed on them the group is the node's one group: each is
/// `pinned_word`, or (`fd`: beside a DISTINCT call, whose forms keep no first row, or an aggregate PostgreSQL presorts,
/// whose Sort's first row is the representative the plain end would have to keep) its relation's PRIMARY KEY is all
/// pinned columns of `pins` (`facts::passenger::pk_determined`, PostgreSQL's check_functional_grouping: `SELECT o.total,
/// count(DISTINCT p) .. WHERE o.id = 7 GROUP BY o.id`). The pinned key holds one value over the rows, so they all carry
/// the one row of the relation it names (never NULL-extended: `pinned_in` takes a scan's rows through an inner join or as
/// the outer side only), and its column the one datum, whatever its type's equality. D345 (4), `grouped` (the node's
/// target list, given for the grouped distinct form only: `pair_distinct`): a column that is one of the node's own GROUP
/// BY columns (`grouped_col`) keys as that GROUP BY did, under its own equality and collation: the planner dropped its
/// pathkey because an equivalence class pins it to one value under that equality (`WHERE c_city = 'Austin' GROUP BY
/// c_city` over a join or a text column; `GROUP BY 'k' || 3`, a pseudoconstant), so the key's groups are the query's
/// own, and they are one. D3874 (`fd`): or a text / name column its own deterministic equality pins (`pinned_bytes`, as
/// `duplicate_keys` keys one): equal values are equal bytes, so the key holds the first row's image. Without it, a text
/// column a filter pins (`WHERE c_city = 'Oslo' GROUP BY c_city`) beside an aggregate PostgreSQL presorts stayed a kept
/// column of a plain group, and the kept arena refused the group whether or not a Sort fed it. D4074 (`fd`): or any
/// one-image column over an input that hands on no row (`WHERE d_id = 4 AND d_id IS NULL` over a NOT NULL column: the
/// planner's Result with 'One-Time Filter: false' holds no filter to read the pin from, and no Sort).
pub(super) fn pins_key<'p>(b: &Builder<'p, '_>, child: &Open<'p>, pins: &[(i16, u32)], fd: bool, grouped: Option<&NodeList<'p>>) -> bool {
    let mut fixed: ArenaVec<Node<'p>> = ArenaVec::new_in(b.arena); // alloc: Cold
    let mut rest: ArenaVec<i16> = ArenaVec::new_in(b.arena); // alloc: Cold
    for &(c, coll) in pins {
        let bytes = || fd && crate::facts::pinned::pinned_bytes(b, child, c, coll);
        // D4074 (`fd`): an input that hands on no row (the planner's empty Result, `facts::pinned::no_row`) has no first
        // row to show and no Sort to read it from: a column whose equal values are one image (the types `grouped_col`
        // keys) keys the node's no group
        let image = |e: Node<'p>| crate::catalog::keysem::one_image(b.facts, nodes_core::expr_type(e), coll);
        let empty = || fd && crate::facts::pinned::no_row(child) && outer_expr(child, c).is_some_and(image);
        match pinned_word(b, child, c) || bytes() || grouped.is_some_and(|t| grouped_col(b, t, c)) || empty() {
            true => fixed.extend(outer_expr(child, c).map(|e| Stripped::entry(e).node())),
            false => rest.push(c),
        }
    }
    let determined = |c: i16| outer_expr(child, c).is_some_and(|e| crate::facts::passenger::pk_determined(b, &fixed, Stripped::entry(e).node()));
    rest.is_empty() || (fd && !fixed.is_empty() && rest.iter().all(|&c| determined(c)))
}

/// D345 (4): input column `c` is one of the node's GROUP BY columns: an entry of the node's own target list `tlist`
/// (resjunk too: the parser carries a GROUP BY column the select list lacks) that is the bare column and holds a
/// sortgroupref (a grouped query admits no other ungrouped column but one its relation's primary key determines). Never
/// a column the planner passes up beside the keys for a window (D2343, `make_window_input_target`: no sortgroupref) or
/// a unique-ification's (`build_path_tlist`: none). Its equal values must be one image (catalog keysem `one_image`):
/// PostgreSQL emits the group's first row, and a key's record keeps any member of its class (a case-insensitive text, a
/// numeric's dscale).
pub(super) fn grouped_col(b: &Builder<'_, '_>, tlist: &NodeList<'_>, c: i16) -> bool {
    tlist.iter().filter_map(|n| n.as_target_entry()).any(|te| te.ressortgroupref != 0 && te.expr.as_var().is_some_and(|v| v.varno == OUTER_VAR && v.varlevelsup == 0 && v.varattno == c && crate::catalog::keysem::one_image(b.facts, v.vartype, v.varcollid)))
}

/// D345 (4): the node's calls make it the grouped distinct form (a DISTINCT call the pair set ends: `cell`'s
/// `distinct_key`), whose every-key-dropped group has no first-row end (runs.group refuses it by name), by a trial of the
/// calls over a scratch shape. The plain end keeps the first row for every other form (fold/kept.rs).
pub(super) fn pair_distinct<'p>(b: &Builder<'p, '_>, child: &Open<'p>, refs: &[Node<'p>], k: Kind, id: i32) -> bool {
    let (mut sh, mut key) = (Shape::new(b.arena), NONE);
    refs.iter().all(|&r| cell(b, child, r, &mut sh, &mut key, k, id).is_ok()) && key != NONE
}

/// Output column `c` of `child` through its projections down to the scan's: an absorbed Sort (absorb.rs) leaves its
/// pass-through target list (`OUTER_VAR` n is the n-th entry of the projection before it) over the scan's, and a kept
/// Sort's own list is the row it emits (D345).
pub(super) fn scan_entry<'p>(child: &Open<'p>, c: i16) -> Option<Node<'p>> {
    scan_entry_at(child, c).map(|(e, _)| e)
}

/// D434: `x` and `y` are equal in every row the node reads: the child's last inner hash join (its projection reads the
/// join's row) holds `x = y` under an equality whose equal values are one image (int2 / int4 / int8 / oid / date / bool
/// / char, both sides one type; D3787: or the integer `=` across widths, int84eq / int48eq / ..: equal integers are one
/// value, `keysem::image_eq`). A semi-join's unique-ification carries such a column beside its key (the planner's
/// EquivalenceClass: `f1 IN (SELECT f2 … WHERE f2 IN (SELECT f1 …))` groups on f2 and emits the inner f1 from the
/// representative row, create_unique_path; Rails' `id IN (SELECT t.id FROM t JOIN link ON link.fk = t.id ..)` with a
/// bigint id and an integer fk emits the int4 fk beside the int8 key); it is the key's value again, so it keys as D384's
/// repeat does (a key of its own type: the groups are the key's).
pub(super) fn equated<'p>(b: &Builder<'p, '_>, child: &Open<'p>, x: Stripped<'p>, y: Stripped<'p>) -> bool {
    if x.agg || y.agg {
        return false;
    }
    let key = |n: Node<'p>| relabels(n).as_var().filter(|v| v.varlevelsup == 0).map(|v| (v.varno, v.varattno, v.vartype));
    let (Some(kx), Some(ky)) = (key(x.node), key(y.node)) else { return false };
    let mut ops = child.ops.iter().rev().skip_while(|op| !matches!(op, Op::Project(_)));
    ops.next();
    let clauses = match ops.next() {
        Some(Op::Join(crate::join::JoinDesc::Probe { jointype: types_nodes::jointype::JoinType::JOIN_INNER, clauses, .. })) => clauses,
        Some(Op::Join(crate::join::JoinDesc::Nest { jointype: types_nodes::jointype::JoinType::JOIN_INNER, params, inner, .. })) => {
            return nest_equated(b, *params, inner, kx, ky) || nest_equated(b, *params, inner, ky, kx);
        }
        _ => return false,
    };
    clauses.iter().any(|c| {
        let Some(o) = c.as_op_expr() else { return false };
        if o.args.len() != 2 {
            return false;
        }
        let (Some(a), Some(z)) = (key(o.args.nth(0)), key(o.args.nth(1))) else { return false };
        crate::catalog::keysem::image_eq(b.facts, o.opno).is_some_and(|(l, r)| a.2 == l && z.2 == r) && ((a == kx && z == ky) || (a == ky && z == kx))
    })
}

/// EXPR-ODDS B (main 09-28): an INNER nest's outer column `o` and inner column `i` are one image when the inner is an
/// index scan keyed `column = $p` (a btree key, or an AM's `=` its recheck repeats: a hash index) by an identity-equality
/// operator of their type (`image_eq_same`), `$p` the nest's parameter bound to `o`, and `i` that key's column through the
/// inner's one projection: every joined row carries equal images of the two. D3787: or one value, under the integer `=`
/// across widths (`keysem::image_eq` (column's type, `$p`'s type): `bigint_id = $p` over an integer `$p`). Never an
/// outer or semi nest (a NULL-extended or unique-ified row).
pub(super) fn nest_equated<'p>(b: &Builder<'p, '_>, params: &'p NodeList<'p>, inner: &Open<'p>, o: (i32, i16, u32), i: (i32, i16, u32)) -> bool {
    if o.0 != OUTER_VAR || i.0 != INNER_VAR {
        return false;
    }
    let Source::Index { keys, .. } = &inner.source else { return false };
    let Some(col) = (projections(inner) == 1).then(|| projection(inner, 0).and_then(|l| entry(l, i.1))).flatten().and_then(|e| scan_attno(b, inner, Stripped::entry(e).node)) else { return false };
    let bound = |pid: i32| params.iter().filter_map(|p| p.as_nest_loop_param()).any(|p| p.paramno == pid && relabels(p.paramval).as_var().is_some_and(|v| (v.varno, v.varattno, v.vartype, v.varlevelsup) == (OUTER_VAR, o.1, o.2, 0)));
    keys.iter().any(|k| {
        matches!(k.form, KeyForm::Op | KeyForm::AmOp)
            && k.attno == col
            && crate::catalog::keysem::image_eq(b.facts, k.opno) == Some((i.2, o.2))
            && relabels(k.value).as_param().is_some_and(|p| p.paramkind == types_nodes::primnodes::ParamKind::PARAM_EXEC && bound(p.paramid))
    })
}

pub(super) fn same_var<'p>(b: &Builder<'p, '_>, child: &Open<'p>, x: Stripped<'p>, y: Stripped<'p>) -> bool {
    match (scan_attno(b, child, x.node), scan_attno(b, child, y.node)) {
        (Some(x), Some(y)) => x == y,
        _ => x.agg == y.agg && matches!((x.node.as_var(), y.node.as_var()), (Some(x), Some(y)) if (x.varno, x.varattno, x.varlevelsup) == (y.varno, y.varattno, y.varlevelsup)),
    }
}

/// DC-09 / OID-06: a builtin `extract(field FROM x)` (effects `builtin_extract`: pg_proc's `extract` rows, any source
/// class) on a constant field whose every finite value is an int4 (`extract_int4`): the key is the field's integer
/// (planner item 6's bounded domains; rep.hosted.hash converts the numeric cell, typesem `integral_word`). Any other
/// field, a field spelled another way (`years`, `mins`), or a non-constant one keeps the numeric key.
pub(super) fn integral_extract(e: Node<'_>) -> bool {
    let Some(f) = e.as_func_expr() else { return false };
    let Some(src) = crate::effects::builtin_extract(f.funcid) else { return false };
    let Some(d) = f.args.iter().next().and_then(|a| a.as_const()).and_then(expr::const_bytes) else { return false };
    extract_int4(src, d)
}

/// Whether every finite value of `extract(field FROM <src>)` is an integer within int4 (PostgreSQL 18.6 date.c
/// `extract_date`, timestamp.c `time_part_common` / `timetz_part_common` / `timestamp_part_common` /
/// `interval_part_common`; an infinite source's field is ±Infinity or NULL, both kept apart by the key word). A field
/// the source does not support (`hour` of a date, `year` of a time, `dow` of an interval) answers false: PostgreSQL
/// raises at the call, on either key.
pub(super) fn extract_int4(src: crate::effects::ExtractSource, field: &[u8]) -> bool {
    use crate::effects::ExtractSource as S;
    // calendar fields: years within 4713 BC .. 5874897 AD (a date; 294276 AD a timestamp), an interval's months field
    // (int4, its years months / 12) and days field (int4, its weeks days / 7)
    let calendar = matches!(field, b"day" | b"week" | b"month" | b"quarter" | b"year" | b"decade" | b"century" | b"millennium");
    // a date's day-of fields and ISO year
    let day_of = matches!(field, b"dow" | b"isodow" | b"doy" | b"isoyear");
    // clock fields: microseconds within a minute (61 s at a leap second), minutes within an hour
    let within_hour = matches!(field, b"minute" | b"microseconds");
    match src {
        S::Date => calendar || day_of,
        // hours within a day (24:00:00 the most)
        S::Timestamp | S::TimestampTz => calendar || day_of || within_hour || field == b"hour",
        S::Time | S::TimeTz => within_hour || field == b"hour",
        // an interval's hours are its int8 time field's (interval '3000000000 hours' passes int4): not here
        S::Interval => calendar || within_hour,
    }
}

/// The key word(s) of an expression by its type (decision 71's table): text the 128-bit fingerprint (ruling 6), floats
/// their canonical bits, a scaled numeric its mantissa, an unscaled one its normalised digits' fingerprint, the 16-byte
/// types two words, every other type the by-ref image (decision 78); an unreadable type refuses by name.
/// A key-level expression's (kind, image, scale) — a Sort's target entry: its `OUTER_VAR` resolves once through `child`'s last projection.
pub fn key_facts<'p>(b: &Builder<'p, '_>, child: &Open<'p>, e: Node<'p>, coll: u32) -> Option<(KeyKind, KeyImage, i16)> {
    key_kind(b, child, strip(child, e), coll, false, Kind::Sort, 0).ok().map(|f| (f.kind, f.image, f.scale))
}

pub fn key_side<'p>(b: &Builder<'p, '_>, child: &Open<'p>, e: Node<'p>) -> (Stripped<'p>, crate::join::Side) {
    // D3630: a relabel that re-forms the word only through a domain (`d::int4` over a domain over oid) stays: the key is
    // the relabel's result, re-formed, typed by its own type, never the column's word
    if crate::expr::domain_reforms(b.facts, e) {
        let side = crate::join::Side { typid: crate::catalog::expr_type::type_of(e), num: NumTypmod::NONE, code: false };
        return (Stripped { node: e, agg: true, bpchar: false, retype: None }, side);
    }
    let t = if relabelled_bpchar(e) { Some(BPCHAROID) } else { type_of_listed(relabels(e)).or_else(|| crate::catalog::expr_type::type_of(e)) };
    let num = num_form_at(b, child, e, true).map_or(NumTypmod::NONE, |(sc, d)| NumTypmod::of((sc + d).min(u16::MAX as u32) as u16, sc as i16));
    let s = strip(child, e);
    let code = scan_lane(b, child, s).is_some_and(|l| matches!(l.rep, gravity_program::Rep::Code { .. }));
    (s, crate::join::Side { typid: t, num, code })
}

/// A group key's word form in the record (`key_kind`'s answer): its kind, numeric scale, image, word count, precision and
/// type.
#[derive(Clone, Copy, Debug)]
pub(super) struct KeyWords {
    pub(super) kind: KeyKind,
    pub(super) scale: i16,
    pub(super) image: KeyImage,
    pub(super) words: u8,
    pub(super) precision: u8,
    pub(super) tf: u32,
}

/// D1292's refusal (sort's name, INDETERMINATE_COLL) for a group key PG's equality or hash raises on: key_kind's and
/// map_agg's one spelling of it.
pub(super) fn indeterminate_coll(k: Kind, id: i32) -> Refusal {
    Refusal::shape(k, id, crate::ops::shared::sort::INDETERMINATE_COLL)
}

/// D2832: the relabel's result type `retype` when the key compares differently by it than by the stripped node `e`'s
/// type: a type the catalog knows whose equality, hash or order is not `e`'s (`c::text` over citext: text's byte
/// equality, not citext's). A relabel between types of one equality (varchar -> text, regclass -> oid, a domain over
/// `e`'s type) or to a type the catalog does not know keys as before.
fn retyped(f: &dyn crate::catalog::Facts, retype: Option<u32>, e: Node<'_>) -> Option<u32> {
    let r = typefacts::facts_of(f, retype?)?;
    let own = type_of_listed(e).or_else(|| crate::catalog::expr_type::type_of(e)).and_then(|t| typefacts::facts_of(f, t));
    let same = |o: &typefacts::TypeFacts| (o.image, o.kind, o.words, o.eq, o.cmp, o.hash, o.collatable) == (r.image, r.kind, r.words, r.eq, r.cmp, r.hash, r.collatable);
    (!own.as_ref().is_some_and(same)).then_some(r.typid)
}

/// A key's word form; `hashed`: the key of a hashed group, which keeps a key `hash_raises` names (every other refuses it).
pub(super) fn key_kind<'p>(b: &Builder<'p, '_>, child: &Open<'p>, s: Stripped<'p>, coll: u32, hashed: bool, k: Kind, id: i32) -> Result<KeyWords, Refusal> {
    let e = s.node;
    let t = if s.bpchar { Some(BPCHAROID) } else { retyped(b.facts, s.retype, e).or_else(|| type_of_listed(e)).or_else(|| crate::catalog::expr_type::type_of(e)) };
    let t = t.ok_or(Refusal::shape(k, id, "a group key whose type is not readable"))?;
    if crate::ops::shared::sort::indeterminate(b.facts, t, coll) {
        // D1292: equality and hashing under no collation raise in PG (texteq / hashtext's check)
        if !(hashed && crate::effects::agg::hash_raises(b.facts, t, coll)) {
            return Err(indeterminate_coll(k, id));
        }
    }
    let tf = match typefacts::facts_of(b.facts, t) {
        Some(tf) => tf,
        None => {
            let att = scan_attno(b, child, s.node).and_then(|a| child.source.relation().and_then(|(_, relid, _)| b.facts.att(relid, a)));
            att.map_or(typefacts::opaque(t, t, -1, false, false), |a| typefacts::opaque(t, t, a.typlen, a.byval, false))
        }
    };
    if tf.image.is_numeric() {
        if integral_extract(e) {
            fires::fire("agg.extract_key"); // shapecheck fire signal (REGISTRY DC-09): bounded-domain extract key
            let (kind, scale, image, words) = crate::catalog::keysem::image::INT4_KEY;
            return Ok(KeyWords { kind, scale, image, words, precision: 0, tf: 23 });
        }
        let num = num_form_at(b, child, e, s.agg).map(|(sc, d)| NumTypmod::of((sc + d).min(u16::MAX as u32) as u16, sc as i16)).unwrap_or(NumTypmod::NONE);
        let (kind, scale, image, words) = crate::catalog::keysem::image::key_form(&tf, num, coll);
        return Ok(KeyWords { kind, scale, image, words, precision: num.precision.min(u8::MAX as u16) as u8, tf: t });
    }
    let (kind, scale, image, words) = crate::catalog::keysem::image::key_form(&tf, NumTypmod::NONE, coll);
    Ok(KeyWords { kind, scale, image, words, precision: 0, tf: t })
}

/// D3080: the key words of hashed group key `s` when it is `int_text_arg`'s cast of a column of the scan `child` reads
/// (not an index-only scan's column), compared under a collation whose equality is byte equality (a deterministic one;
/// no relabel to a type of another equality): the integer's own key words (one raw word). `None` keeps the text key.
pub(super) fn int_text_words<'p>(b: &Builder<'p, '_>, child: &Open<'p>, s: Stripped<'p>, coll: u32) -> Option<KeyWords> {
    if s.bpchar || s.agg || s.retype.is_some_and(|t| t != crate::catalog::TEXTOID && t != crate::catalog::VARCHAROID) {
        return None;
    }
    if coll == 0 || !b.facts.coll(coll).bytes_eq {
        return None;
    }
    let arg = crate::ops::shared::agg::int_text_arg(s.node)?;
    let v = arg.as_var()?;
    if v.varno == INDEX_VAR || scan_attno(b, child, arg) != Some(v.varattno) {
        return None;
    }
    key_kind(b, child, s.arg(child, arg), 0, true, Kind::Agg, 0).ok().filter(|w| w.words == 1 && w.image.is_raw())
}

/// D3080: whether an aggregate of `lists` reads input column `c` (its arguments, FILTER or ORDER BY), or carries a
/// DISTINCT or an ORDER BY of its own (the grouped distinct and presorted forms key their arguments beside the group's):
/// such a group keeps a cast key as text. A node the walker does not know answers true.
pub(super) fn agg_reads(lists: &[&NodeList<'_>], c: i16) -> bool {
    let reads = |n: Node<'_>| n.as_var().is_some_and(|v| v.varno == OUTER_VAR && v.varlevelsup == 0 && v.varattno == c);
    lists.iter().any(|l| l.iter().any(|n| !expr::walk(n, &mut |x| match x.as_aggref() {
        Some(a) => a.aggdistinct.is_nil() && a.aggorder.is_nil() && expr::walk(x, &mut |y| !reads(y)),
        None => true,
    })))
}

/// Never null: a NOT NULL column, a non-null constant, or a strict builtin over such (the record's `new_nn` form).
pub(super) fn notnull<'p>(b: &Builder<'p, '_>, child: &Open<'p>, s: Stripped<'p>) -> bool {
    crate::walk::stack_check(); // D3748
    let Some((_, relid, _)) = child.source.relation() else { return false };
    let e = s.node;
    match e.node_tag() {
        NodeTag::T_Var => scan_attno(b, child, s.node).and_then(|a| b.facts.att(relid, a)).is_some_and(|a| a.notnull),
        NodeTag::T_Const => !e.as_const().unwrap().constisnull,
        NodeTag::T_FuncExpr => e.as_func_expr().unwrap().args.iter().all(|a| notnull(b, child, s.arg(child, a))),
        NodeTag::T_OpExpr => e.as_op_expr().unwrap().args.iter().all(|a| notnull(b, child, s.arg(child, a))),
        _ => false,
    }
}

pub(super) const WIDE_UNTRACKED: &str = "a sorted group by more than eight keys whose input order past the eighth key is not tracked (D2061)";

/// A sorted group's input must arrive ordered by its keys, in either direction per key (equal keys adjacent): the
/// provenance `sort::order_of` reads from the input pipeline — a Sort's keys, an index's columns, a merge join's
/// outer, a MergeAppend's merge, a sorted group's emit — through its projections. The keys take the input's direction.
pub(crate) fn ordered<'p>(b: &Builder<'p, '_>, child: &Open<'p>, keys: &mut [KeyDecl<'p>], k: Kind, id: i32, sort: Option<crate::ops::group::absorb::KeySort>) -> Result<(), Refusal> {
    // An Order holds MAX_ORDER_KEYS keys: past them the input's direction (a DESC index) and equality (a user family's
    // index, D2051) are known only from a full or Incremental Sort directly below (`absorb::wide_sort`) or, D2061, a
    // btree index scan's key columns of their default families (`order::index_past`); a SetOp's sides sort every key
    // ascending, NULLs last, by its default family.
    let wide = keys.len() > crate::graph::MAX_ORDER_KEYS && k != Kind::SetOp;
    let past = if wide && sort.is_none() { crate::ops::shared::order::index_past(b, child) } else { None };
    if wide && sort.is_none() && past.is_none() {
        return Err(Refusal::shape(k, id, WIDE_UNTRACKED));
    }
    let o = crate::ops::shared::order::order_of(b, child);
    let mut want = Order::NONE;
    for key in keys.iter() {
        let col = Col::outer(key.resno);
        let dir = o.keys().iter().find(|x| o.same(x.col, col));
        want.push(OrderKey { col, desc: dir.is_some_and(|x| x.desc), nulls_first: dir.is_some_and(|x| x.nulls_first) });
    }
    // D3873: a key that holds one value on every row needs no order key (PostgreSQL's Sort dropped it as redundant)
    let mut need = Order::NONE;
    for w in want.keys().iter().filter(|w| !crate::facts::pinned::one_value(b, child, w.col.attno)) {
        need.push(*w);
    }
    crate::ops::shared::order::ordered_as(&o, crate::ops::shared::order::projection(child), need.keys(), k, id, "a sorted group over an input with no known order", "the input order does not lead with the group keys")?;
    for (key, w) in keys.iter_mut().zip(want.keys()) {
        (key.desc, key.nulls_first) = (w.desc, w.nulls_first);
    }
    if let Some(s) = sort.filter(|_| wide) {
        s.direct(&mut keys[crate::graph::MAX_ORDER_KEYS..]);
    }
    if let Some(past) = past {
        // every key leads the index's order, past the eighth too (directions free: equal keys adjacent), and takes the
        // direction of the index key it meets
        let mut all = ArenaVec::with_capacity_in(keys.len(), b.arena);
        all.extend(keys.iter().map(|key| OrderKey { col: Col::outer(key.resno), desc: false, nulls_first: false }));
        if o.n as usize != crate::graph::MAX_ORDER_KEYS || crate::ops::leads_past(&o, &past, crate::ops::shared::order::projection(child), &all) != crate::ops::Leads::Yes {
            return Err(Refusal::shape(k, id, WIDE_UNTRACKED));
        }
        for key in keys[crate::graph::MAX_ORDER_KEYS..].iter_mut() {
            let col = Col::outer(key.resno);
            if let Some(x) = o.keys().iter().chain(past.iter()).find(|x| o.same(x.col, col)) {
                (key.desc, key.nulls_first) = (x.desc, x.nulls_first);
            }
        }
    }
    Ok(())
}

/// The order a group's emit delivers: the keys in the input's direction under `ORDER_KEY`, none otherwise.
pub(super) fn emit_order(keys: &[KeyDecl<'_>], order: u8) -> Order {
    let mut o = Order::NONE;
    if order == ORDER_KEY {
        for key in keys {
            o.push(OrderKey { col: Col::outer(key.resno), desc: key.desc, nulls_first: key.nulls_first });
        }
    }
    o
}

#[cfg(test)]
mod dc09_extract_tests {
    use super::extract_int4;
    use crate::effects::{builtin_extract, ExtractSource as S};

    #[test]
    fn every_catalog_extract_row_names_its_source() {
        let classes: Vec<_> = crate::effects::procfacts::EXTRACTS.iter().map(|&(oid, _)| builtin_extract(oid as u32)).collect();
        assert_eq!(classes, [Some(S::Date), Some(S::Time), Some(S::TimeTz), Some(S::Timestamp), Some(S::TimestampTz), Some(S::Interval)]);
        // a function outside the table (an oid past u16 included) is no extract
        assert_eq!(builtin_extract(u32::MAX), None);
    }

    #[test]
    fn an_interval_keys_its_bounded_fields_by_int4_but_not_its_hours() {
        for f in [&b"month"[..], b"quarter", b"year", b"decade", b"century", b"millennium", b"day", b"week", b"minute", b"microseconds"] {
            assert!(extract_int4(S::Interval, f), "{}", String::from_utf8_lossy(f));
        }
        for f in [&b"hour"[..], b"dow", b"isodow", b"doy", b"isoyear", b"epoch", b"second", b"milliseconds", b"years"] {
            assert!(!extract_int4(S::Interval, f), "{}", String::from_utf8_lossy(f));
        }
    }

    #[test]
    fn a_source_keys_only_the_fields_it_supports() {
        assert!(extract_int4(S::Timestamp, b"hour") && extract_int4(S::TimestampTz, b"isoyear") && extract_int4(S::Date, b"doy"));
        assert!(!extract_int4(S::Date, b"hour") && !extract_int4(S::Date, b"microseconds"));
        assert!(extract_int4(S::Time, b"minute") && extract_int4(S::TimeTz, b"hour"));
        assert!(!extract_int4(S::Time, b"year") && !extract_int4(S::TimeTz, b"day"));
        // a fraction (seconds, milliseconds, julian of a timestamp, epoch) is never an int4
        for s in [S::Date, S::Time, S::TimeTz, S::Timestamp, S::TimestampTz, S::Interval] {
            assert!(!extract_int4(s, b"second") && !extract_int4(s, b"milliseconds") && !extract_int4(s, b"epoch"));
        }
    }
}
