/// D3752: the built-in integer narrowing casts (`pg_proc.dat` `int84` 480: int8 to int4, `int82` 714: int8 to int2; int8.c
/// `int84` / `int82`): each raises out of its result's range and keeps every other value, so two different arguments
/// never give one result (a serial column's default, `int4(nextval(..))`, draws as many values as the sequence does).
pub const fn int_narrowing(funcid: u32) -> bool {
    matches!(funcid, 480 | 714)
}

/// D3752: `nextval(regclass)` (`pg_proc.dat` 1574, sequence.c `nextval_oid`), a serial column's default: each call draws
/// the next value of the sequence its argument names.
pub const NEXTVAL: u32 = 1574;

/// Whether `funcid` is `NEXTVAL`.
pub const fn nextval(funcid: u32) -> bool {
    funcid == NEXTVAL
}

/// The built-in integer to numeric casts (`pg_cast.dat` method `f`, `pg_proc.dat` `int4_numeric` 1740, `int8_numeric`
/// 1781, `int2_numeric` 1782; the planner's `intcol BETWEEN 1500000.0 AND ..`): each is exact, never raises and keeps the
/// order, so `cast(x) op c` holds exactly when the integer `x` compares with `c` rounded the operator's way.
pub const fn int_numeric(funcid: u32) -> bool {
    matches!(funcid, 1740 | 1781 | 1782)
}

/// PS1-10: a `RelabelType` to `resulttype` over a column with facts `a` reads a domain column as its base type (C
/// `coerce_type`'s relabel of a domain to its base, `getBaseType`): the column is a domain (`typid` is not its `base`,
/// D2700) over a by-value base, and the relabel's result is that base. A domain stores its base's bytes, so the column's
/// word and key image are the base's. No other binary-coercible relabel is one: `oidcol::int4` reads oid's zero-extended
/// word where int4 compares sign-extended (D858, [`zext4`]), so a zone predicate read through it would skip granules
/// wrongly. A by-reference base keeps its relabel: a domain over numeric(12, 2) read through it as the packed lane gave
/// wrong zone answers in the witness's first run (`dn = 500.5`: no row, PG one), not yet understood, so it fails closed.
pub const fn domain_to_base(a: &super::AttFact, resulttype: u32) -> bool {
    a.byval && a.typid != a.base && a.base == resulttype
}

/// PS1-10: `n` without a domain-to-base relabel ([`domain_to_base`]) of a column of scan `varno` (`att`: the column's
/// facts by attribute number), else `n` as it is. The one reading both zone sites use (decompose `sources::zone` for the
/// keyed site, lowerv4 `verdict_zone::lane_of` for the lane), so the content key and the lane agree.
pub fn domain_base<'p>(n: types_nodes::Node<'p>, varno: i32, att: impl Fn(i16) -> Option<super::AttFact>) -> types_nodes::Node<'p> {
    let Some(r) = n.as_relabel_type() else { return n };
    let col = r.arg.as_var().filter(|v| v.varno == varno && v.varlevelsup == 0 && v.varattno > 0);
    match col.and_then(|v| att(v.varattno).filter(|a| a.typid == v.vartype)) {
        Some(a) if domain_to_base(&a, r.resulttype) => r.arg,
        _ => n,
    }
}

/// SRC-09: `n` without the relabel of a varchar column (a domain over varchar too: its `AttFact.base`) to text, which
/// varchar's comparisons read it through (varchar has text's operators, `pg_cast` varchar -> text is binary-coercible):
/// the same bytes, so the same prefix key and the same code lane; else `n` as it is. A column of scan `varno` only, its
/// facts `att` by attribute number.
pub fn varchar_text<'p>(n: types_nodes::Node<'p>, varno: i32, att: impl Fn(i16) -> Option<super::AttFact>) -> types_nodes::Node<'p> {
    let Some(r) = n.as_relabel_type().filter(|r| r.resulttype == super::TEXTOID) else { return n };
    let col = r.arg.as_var().filter(|v| v.varno == varno && v.varlevelsup == 0 && v.varattno > 0);
    match col.and_then(|v| att(v.varattno).filter(|a| a.typid == v.vartype)) {
        Some(a) if a.base == super::VARCHAROID => r.arg,
        _ => n,
    }
}

/// numeric's typmod cast, `numeric(numeric, int4)` (`pg_proc.dat` 1703, fmgroids `F_NUMERIC_NUMERIC_INT4`): its second
/// argument is the target typmod, part of the result type rather than a value (exec inputs/front.rs keys it so, as a
/// cast's trailing arguments; decompose agg `num_form_at` reads it as a scale).
pub const fn numeric_typmod_cast(funcid: u32) -> bool {
    funcid == super::numeric::F_NUMERIC_NUMERIC_INT4
}

/// D858: a zero-extended 4-byte type (`ObjectIdGetDatum` / `UInt32GetDatum`: oid 26, xid 28, cid 29, regproc 24 and the
/// reg* types); int4 (`Int32GetDatum`) sign-extends. gravity_fn typesem `expr::zext4` holds the same list for gravity_expr.
pub const fn zext4(t: u32) -> bool {
    matches!(t, 26 | 28 | 29 | 24 | 2202 | 2203 | 2204 | 2205 | 2206 | 3734 | 3769 | 4089 | 4096 | 4191)
}
