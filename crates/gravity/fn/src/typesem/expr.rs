//! gravity_expr's catalog adapter reads (expr/src/catalog.rs `PgCatalog`, nodes/array.rs `runtime_elem_facts`): the
//! operator-family support procs, the operator classes and the type layouts PostgreSQL's executor resolves for an
//! expression (lsyscache.c `get_opfamily_proc`, `get_ordering_op_properties`, `get_op_opfamily_properties`,
//! `get_op_hash_functions`, `get_equality_op_for_ordering_op`, `get_compatible_hash_operators`,
//! `get_opclass_opfamily_and_input_type`, `get_typlenbyvalalign`, `get_typcollation`).
use types_core::Oid;
use types_error::PgResult;

/// Btree support function 1 (the comparator) of `opfamily` at `left` / `right`.
pub fn btree_cmp_proc(opfamily: Oid, left: Oid, right: Oid) -> PgResult<Oid> {
    lsyscache::get_opfamily_proc(opfamily, left, right, 1)
}

/// Btree support function `num` of the family a sort by `op` compares in, at its input type (sortsupport.c
/// `PrepareSortSupportFromOrderingOp`); 0 when `op` is no ordering operator.
pub fn ordering_proc(op: Oid, num: i16) -> PgResult<Oid> {
    Ok(match lsyscache::get_ordering_op_properties(op)? {
        Some((family, t, _)) => lsyscache::get_opfamily_proc(family, t, t, num)?,
        None => 0,
    })
}

/// An ordering operator's constant list read as its extreme element (lowerv4 expr.saop, LIST-EXT): `op` is the `<`, `<=`,
/// `>=` or `>` member of a family whose access method orders (lsyscache.c `get_op_index_interpretation`), so `x op ANY`
/// over a list is `x op` its greatest element (`<` / `<=`) or its least (`>=` / `>`) as that family orders them (its
/// collation, float NaN above every number), and `less` is the family's `<` procedure over `op`'s right type, which
/// orders two elements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ListOrder {
    pub greatest: bool,
    pub less: Oid,
}

/// `ListOrder` of operator `op`; none for an operator that is no such member, or a family without a `<` over its right type.
pub fn list_order(op: Oid) -> PgResult<Option<ListOrder>> {
    let cx = mcx::MemoryContext::new_bump("typesem list order"); // alloc: Program
    for i in lsyscache::get_op_index_interpretation(cx.mcx(), op)?.iter() {
        let greatest = match i.cmptype {
            lsyscache::COMPARE_LT | lsyscache::COMPARE_LE => true,
            lsyscache::COMPARE_GE | lsyscache::COMPARE_GT => false,
            _ => continue,
        };
        let lt = lsyscache::get_opfamily_member(i.opfamily_id, i.oprighttype, i.oprighttype, lsyscache::BTLessStrategyNumber)?;
        let less = if lt == 0 { 0 } else { lsyscache::get_opcode(lt)? };
        if less != 0 {
            return Ok(Some(ListOrder { greatest, less }));
        }
    }
    Ok(None)
}

/// `op` is the equality member of a family whose access method orders (lsyscache.c `get_op_index_interpretation`,
/// COMPARE_EQ): an index key by it is an equality key (a btree column keyed by none takes a skip array, nbtree's
/// `_bt_preprocess_array_keys`).
pub fn index_equality(op: Oid) -> PgResult<bool> {
    let mut cx = mcx::MemoryContext::new_bump("typesem index equality"); // alloc: Exec
    let equal = lsyscache::get_op_index_interpretation(cx.mcx(), op)?.iter().any(|i| i.cmptype == lsyscache::COMPARE_EQ);
    Ok(equal)
}

/// The comparison lookups of ExecutorStart's node inits, which raise under EXEC_FLAG_EXPLAIN_ONLY too (exec runner
/// start_gates.rs: an EXPLAIN that never runs the plan raises what they raise). MJExamineQuals (nodeMergejoin.c) for one
/// merge clause: `op` an equality member of `family`, with a sort support or a comparator at its declared types.
pub fn init_merge_clause(op: Oid, family: Oid) -> PgResult<()> {
    let (strategy, l, r) = lsyscache::get_op_opfamily_properties(op, family, false)?;
    if strategy != i32::from(lsyscache::BTEqualStrategyNumber) {
        return Err(init_error(format!("cannot merge using non-equality operator {op}"))); // alloc: Cold
    }
    init_support_or_order(family, l, r)
}

/// PrepareSortSupportFromOrderingOp (sortsupport.c) for one sort key of a MergeAppend or a Gather Merge (their inits):
/// an ordering operator whose family has a sort support or a comparator at its input type.
pub fn init_ordering_op(op: Oid) -> PgResult<()> {
    match lsyscache::get_ordering_op_properties(op)? {
        Some((family, t, _)) => init_support_or_order(family, t, t),
        None => Err(init_error(format!("operator {op} is not a valid ordering operator"))), // alloc: Cold
    }
}

/// ExecInitExprRec (execExpr.c) for one RowCompareExpr column: the comparator of `op`'s family at its declared types.
pub fn init_row_compare(op: Oid, family: Oid) -> PgResult<()> {
    let (_, l, r) = lsyscache::get_op_opfamily_properties(op, family, false)?;
    match btree_cmp_proc(family, l, r)? {
        0 => Err(init_missing(family, l, r)),
        _ => Ok(()),
    }
}

/// ExecInitHashJoin (nodeHashjoin.c) for one hash operator: the hash functions of its inputs (`get_op_hash_functions`),
/// else XX000 (D2036: a user operator family's FUNCTION 1 dropped, which PostgreSQL's plancache does not follow).
pub fn init_hash_op(op: Oid) -> PgResult<()> {
    match lsyscache::get_op_hash_functions(op)? {
        Some(_) => Ok(()),
        None => Err(init_error(format!("could not find hash function for hash operator {op}"))), // alloc: Cold
    }
}

/// ExecInitExprRec for a MinMaxExpr (GREATEST / LEAST): its type's btree comparison function (typcache's cmp_proc).
pub fn init_min_max(t: Oid) -> PgResult<()> {
    if typcache::lookup_type_cache(t, typcache::TYPECACHE_CMP_PROC)?.cmp_proc() != 0 {
        return Ok(());
    }
    let name = format_type::format_type_be(t)?;
    Err(Box::new(types_error::PgError::error(format!("could not identify a comparison function for type {name}")).with_sqlstate(types_error::ERRCODE_UNDEFINED_FUNCTION))) // alloc: Cold
}

/// A sort support (btree support function 2) or else a comparator (1) of `family` at `l` / `r`
/// (FinishSortSupportFunction's and MJExamineQuals' fallback).
fn init_support_or_order(family: Oid, l: Oid, r: Oid) -> PgResult<()> {
    if lsyscache::get_opfamily_proc(family, l, r, 2)? != 0 || btree_cmp_proc(family, l, r)? != 0 {
        return Ok(());
    }
    Err(init_missing(family, l, r))
}

#[cold]
fn init_missing(family: Oid, l: Oid, r: Oid) -> Box<types_error::PgError> {
    init_error(format!("missing support function 1({l},{r}) in opfamily {family}")) // alloc: Cold
}

#[cold]
fn init_error(msg: String) -> Box<types_error::PgError> {
    Box::new(types_error::PgError::error(msg).with_sqlstate(types_error::ERRCODE_INTERNAL_ERROR)) // alloc: Cold
}

/// Btree support function `num` of merge family `family` at `op`'s member types.
pub fn merge_proc(op: Oid, family: Oid, num: i16) -> PgResult<Oid> {
    let (_, l, r) = lsyscache::get_op_opfamily_properties(op, family, false)?;
    lsyscache::get_opfamily_proc(family, l, r, num)
}

/// The hash function of `op`'s left (right when `rhs`) input; 0 when `op` has none.
pub fn hash_proc(op: Oid, rhs: bool) -> PgResult<Oid> {
    Ok(lsyscache::get_op_hash_functions(op)?.map_or(0, |(l, r)| if rhs { r } else { l }))
}

/// The equality operator of ordering operator `op`'s family; 0 when it has none.
pub fn eq_of_sort(op: Oid) -> PgResult<Oid> {
    Ok(lsyscache::get_equality_op_for_ordering_op(op)?.map_or(0, |e| e.0))
}

/// The hash-compatible equality operator over `op`'s right input type; 0 when there is none.
pub fn hash_rhs_eq(op: Oid) -> PgResult<Oid> {
    Ok(lsyscache::get_compatible_hash_operators(op, false, true)?.map_or(0, |e| e.1))
}

/// Operator class `opclass`'s family and input type; `(0, 0)` when it is unknown.
pub fn opclass(opclass: Oid) -> PgResult<(Oid, Oid)> {
    Ok(lsyscache::get_opclass_opfamily_and_input_type(opclass)?.unwrap_or((0, 0)))
}

/// Type `typid`'s `typlen`, `typbyval` and `typalign`.
pub fn layout(typid: Oid) -> PgResult<(i16, bool, i8)> {
    lsyscache::get_typlenbyvalalign(typid)
}

/// Type `typid`'s `typcollation`.
pub fn collation(typid: Oid) -> PgResult<Oid> {
    lsyscache::get_typcollation(typid)
}

/// A pseudo-type argument (record, internal, any and the polymorphic families): a function taking one resolves its actual
/// argument types from the call's expression node (fmgr.c `get_fn_expr_argtype`), so it is never called without its node.
pub fn node_arg_type(t: Oid) -> bool {
    matches!(t, 2249 | 2276 | 2277 | 2281 | 2283 | 2776 | 3500 | 3831 | 4537 | 4538 | 5077 | 5078 | 5079 | 5080)
}

/// Type `cstring` (2275), a C string pseudo-type of fixed length -2 (a type input function's argument): its length is known
/// without a catalog read.
pub fn cstring(t: Oid) -> bool {
    t == 2275
}

/// Type `unknown` (705), an untyped literal's: an unknown-typed Const is the one argument kind a function-manager site
/// reports stable to `get_fn_expr_arg_stable` (catalog.rs CallFacts `stable`).
pub fn unknown_type(t: Oid) -> bool {
    t == 705
}

/// The 4-byte by-value types whose Datum is zero-extended (`UInt32GetDatum` underneath): oid, xid, cid and the reg* types.
pub fn zext4(t: Oid) -> bool {
    matches!(t, 26 | 28 | 29 | 24 | 2202 | 2203 | 2204 | 2205 | 2206 | 3734 | 3769 | 4089 | 4096 | 4191)
}

/// The stored-image length a linear `= ANY` / `ALL` walk may compare in place of calling `funcid` (0: call it): the
/// `typlen` of a fixed-length by-reference type whose equal values are equal stored bytes (`bytes_leaf`; uuid's
/// uuid_eq is `memcmp(.., UUID_LEN) == 0`, uuid.c `uuid_internal_cmp`), when `funcid` is the proc of that type's own
/// `=` (its btree family's equality member, read as `own_eq` reads it: the type cache's eq_opr is never asked); minus
/// that length when `funcid` is the proc of the `=`'s negator (`<>`, uuid_ne: `!uuid_eq`), compared and negated.
pub fn image_eq_len(typid: Oid, funcid: Oid) -> PgResult<i16> {
    if !super::bytes_leaf(typid) {
        return Ok(0);
    }
    let (len, byval, _) = lsyscache::get_typlenbyvalalign(typid)?;
    if byval || len <= 0 {
        return Ok(0);
    }
    let (_, _, eq) = super::own_eq(typid)?;
    if eq == 0 {
        return Ok(0);
    }
    if lsyscache::get_opcode(eq)? == funcid {
        return Ok(len);
    }
    let ne = lsyscache::get_negator(eq)?;
    Ok(if ne != 0 && lsyscache::get_opcode(ne)? == funcid { -len } else { 0 })
}

/// The built-in aggregates gravity_expr's kernels write inline, by aggregate function (pg_aggregate.dat; gravity_expr
/// agg.rs `native_form` gives each its transition, final step and result type).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeAgg {
    /// count(*) 2803.
    CountStar,
    /// count(any) 2147.
    CountAny,
    /// sum(int4) 2108, sum(int2) 2109.
    SumInt,
    /// sum(int8) 2107.
    SumInt8,
    /// avg(int8) 2100.
    AvgInt8,
    /// sum(float4) 2110.
    SumF4,
    /// sum(float8) 2111.
    SumF8,
    /// avg(int4) 2101, avg(int2) 2102.
    AvgInt,
    /// avg(float4) 2104, avg(float8) 2105.
    AvgFloat,
    /// sum(numeric) 2114.
    SumNumeric,
    /// avg(numeric) 2103.
    AvgNumeric,
    /// bool_and 2517, every 2519.
    BoolAnd,
    /// bool_or 2518.
    BoolOr,
    /// string_agg(text, text) 3538.
    StringAggText,
    /// string_agg(bytea, bytea) 3545.
    StringAggBytea,
    /// array_agg(anynonarray) 2335.
    ArrayAgg,
    /// The built-in min / max aggregates, whose state is the first argument's type.
    MinMax,
}

/// Aggregate function `aggfnoid`'s inline form; `None` for every other aggregate.
pub fn native_agg(aggfnoid: Oid) -> Option<NativeAgg> {
    use NativeAgg::*;
    Some(match aggfnoid {
        2803 => CountStar,
        2147 => CountAny,
        2108 | 2109 => SumInt,
        2107 => SumInt8,
        2100 => AvgInt8,
        2110 => SumF4,
        2111 => SumF8,
        2101 | 2102 => AvgInt,
        2104 | 2105 => AvgFloat,
        2114 => SumNumeric,
        2103 => AvgNumeric,
        2517 | 2519 => BoolAnd,
        2518 => BoolOr,
        3538 => StringAggText,
        3545 => StringAggBytea,
        2335 => ArrayAgg,
        2115..=2146 | 2050 | 2051 | 6373 | 6374 | 2244 | 2245 | 2797 | 2798 | 3564 | 3565 | 4189 | 4190 | 5099 | 5100 | 6395 | 6396 | 3526 | 3527 => MinMax,
        _ => return None,
    })
}

/// Whether inline aggregate `aggfnoid` reads an int2 argument where its form's other member reads an int4 (sum(int2)
/// 2109 of `SumInt`, avg(int2) 2102 of `AvgInt`).
#[inline]
pub fn agg_int2_arg(aggfnoid: Oid) -> bool {
    matches!(aggfnoid, 2109 | 2102)
}

/// Whether inline aggregate `aggfnoid` reads a float4 argument where its form's other member reads a float8 (avg(float4)
/// 2104 of `AvgFloat`).
#[inline]
pub fn agg_float4_arg(aggfnoid: Oid) -> bool {
    aggfnoid == 2104
}

/// strpos(text, text) 868 and position(text, text) 849 (pg_proc.dat): the substring-position functions.
pub const TEXT_POSITION: [Oid; 2] = [868, 849];

/// The int4 compares (pg_proc.dat int4eq 65, int4ne 144, int4lt 66, int4le 149, int4gt 147, int4ge 150).
pub const INT4_COMPARE: [Oid; 6] = [65, 144, 66, 149, 147, 150];

/// Whether `funcid` is an int4 compare (`INT4_COMPARE`).
pub fn int4_compare(funcid: Oid) -> bool {
    INT4_COMPARE.contains(&funcid)
}

/// Whether `funcid` is a substring-position function (`TEXT_POSITION`).
pub fn text_position(funcid: Oid) -> bool {
    TEXT_POSITION.contains(&funcid)
}

/// Whether `funcid` is starts_with(text, text) 3696 (pg_proc.dat; the `^@` operator's procedure).
pub fn starts_with(funcid: Oid) -> bool {
    funcid == 3696
}

/// Equal bytes are equal values under `coll` (the database default, C, POSIX, or a deterministic one as the catalog builds
/// it on the leader, collation `Facts`); no collation (D2046) or one the catalog cannot build answers false, so a body
/// that builds it (texteq's pg_newlocale_from_collation) raises PostgreSQL's error. `claims_coll_bytes` reads only the
/// catalog's flag.
pub fn built_coll_bytes(coll: Oid) -> bool {
    use crate::typesem::collation::{Facts, C_COLLATION_OID, DEFAULT_COLLATION_OID, POSIX_COLLATION_OID};
    matches!(coll, DEFAULT_COLLATION_OID | C_COLLATION_OID | POSIX_COLLATION_OID) || (coll != 0 && Facts::of(coll).is_ok_and(|f| f.deterministic))
}

/// Whether `funcid` is extract(text, timestamp) 6202 (pg_proc.dat `extract_timestamp`).
pub fn extract_timestamp(funcid: Oid) -> bool {
    funcid == 6202
}

/// The cross-type datetime comparison operators (pg_operator.dat), each run of six in the order `<`, `<=`, `=`, `>=`, `>`,
/// `<>`: date vs timestamp 2345..2350, date vs timestamptz 2358..2363, timestamp vs date 2371..2376, timestamptz vs date
/// 2384..2389. `Some((pair, op))`: `pair` the run's index in that order, `op` the comparison's index in its run; `None`
/// for any other operator.
pub fn date_cross_cmp(opno: Oid) -> Option<(u8, u8)> {
    const RUNS: [Oid; 4] = [2345, 2358, 2371, 2384];
    RUNS.iter().position(|&b| (b..b + 6).contains(&opno)).map(|i| (i as u8, (opno - RUNS[i]) as u8))
}

/// Whether `funcid` is a timestamp difference: timestamptz_mi 1188 or timestamp_mi 2031 (pg_proc.dat), each an interval
/// of the two values' microseconds.
pub fn timestamp_minus(funcid: Oid) -> bool {
    matches!(funcid, 1188 | 2031)
}

/// The integer-valued equalities (int2 / int4 / int8 and their cross-type forms, oid, "char", bool, date, time,
/// timestamp[tz]; pg_proc.dat): over by-value operands they answer as the two sign-extended values compare.
pub fn native_eq(funcid: Oid) -> bool {
    matches!(funcid, 63 | 65 | 467 | 158 | 159 | 1850 | 1856 | 852 | 474 | 184 | 61 | 60 | 1086 | 1152 | 2052 | 1145)
}

/// Whether `t` is the pseudo-type `record` 2249 (an anonymous row: a whole-row Var of a subquery scan carries it).
pub fn record_type(t: Oid) -> bool {
    t == 2249
}

/// The LIKE functions over text (pg_proc.dat textlike 850, like(text, text) 1569, bpcharlike 1631, and their NOT LIKE
/// forms textnlike 851, notlike 1570, bpcharnlike 1632): `Some(negated)`, `None` for any other function.
pub fn text_like(funcid: Oid) -> Option<bool> {
    match funcid {
        850 | 1569 | 1631 => Some(false),
        851 | 1570 | 1632 => Some(true),
        _ => None,
    }
}

/// The column types a text LIKE / length claim reads as text: text 25, bpchar 1042, varchar 1043.
pub const TEXT_LIKE_TYPES: [Oid; 3] = [25, 1042, 1043];
