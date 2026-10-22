//! tsquery_rewrite.c — ts_rewrite over the tsquery_core QTNode machinery.

use ::adt_tsquery_core::util::{
    qt2qtn, qtn2qt, qtn_clear_flags, qtn_copy, qtn_sort, qtn_ternary, qtn_eq, qtnode_compare,
    QtNode, QTN_NOCHANGE,
};
use ::adt_tsvector_core::builtins::arg_tsquery;
use ::adt_tsvector_core::query::{Item, TsQueryRef, OP_NOT};
use ::datum::Datum;
use ::mcx::{vec_with_capacity_in, Mcx, PgVec};
use ::types_error::{PgError, PgResult, ERRCODE_INVALID_PARAMETER_VALUE};
use ::types_fmgr::{
    varlena_result, FmgrBuiltin, FmgrInfo, FunctionCallInfoBaseData as Fcinfo, PGFunction,
};

const TSQUERYOID: ::types_core::Oid = 3615;

// findeq (tsquery_rewrite.c), in place like C; false = the node was replaced
// by the empty substitution.
fn findeq<'mcx>(
    mcx: Mcx<'mcx>,
    node: &mut QtNode<'mcx>,
    ex: &QtNode<'_>,
    subs: Option<&QtNode<'_>>,
    isfind: &mut bool,
) -> PgResult<bool> {
    if (node.sign & ex.sign) != ex.sign
        || core::mem::discriminant(&node.item) != core::mem::discriminant(&ex.item)
    {
        return Ok(true);
    }
    if node.flags & QTN_NOCHANGE != 0 {
        return Ok(true);
    }

    match (node.item, ex.item) {
        (Item::Opr(nopr), Item::Opr(eopr)) => {
            if nopr.oper != eopr.oper {
                return Ok(true);
            }
            if node.children.len() == ex.children.len() {
                if qtn_eq(node, ex)? {
                    *isfind = true;
                    return substitute(mcx, node, subs);
                }
            } else if node.children.len() > ex.children.len() && !ex.children.is_empty() {
                // AND/OR are commutative/associative: match a subset of the
                // (sorted) children in one merge pass.
                let mut matched = Vec::new();
                matched.try_reserve_exact(node.children.len()).map_err(|_| ::mcx::oom_named("tsquery", node.children.len()))?;
                matched.resize(node.children.len(), false);
                let mut nmatched = 0usize;
                let (mut i, mut j) = (0usize, 0usize);
                while i < node.children.len() && j < ex.children.len() {
                    let cmp = qtnode_compare(&node.children[i], &ex.children[j])?;
                    if cmp == 0 {
                        matched[i] = true;
                        nmatched += 1;
                        i += 1;
                        j += 1;
                    } else if cmp < 0 {
                        i += 1;
                    } else {
                        break;
                    }
                }
                if nmatched == ex.children.len() {
                    compact(&mut node.children, |i| !matched[i]);
                    if let Some(s) = subs {
                        let mut sc = qtn_copy(mcx, s)?;
                        sc.flags |= QTN_NOCHANGE;
                        node.children.try_reserve(1).map_err(|_| mcx.oom(1))?;
                        node.children.push(sc);
                    }
                    // Zero-or-one-child simplification is dofindsubquery's
                    // job; the re-sort keeps regression output stable.
                    qtn_sort(node)?;
                    *isfind = true;
                }
            }
            Ok(true)
        }
        (Item::Val(nop), Item::Val(eop)) => {
            if nop.valcrc != eop.valcrc || !qtn_eq(node, ex)? {
                return Ok(true);
            }
            *isfind = true;
            substitute(mcx, node, subs)
        }
        _ => Ok(true),
    }
}

fn substitute<'mcx>(
    mcx: Mcx<'mcx>,
    node: &mut QtNode<'mcx>,
    subs: Option<&QtNode<'_>>,
) -> PgResult<bool> {
    let Some(s) = subs else { return Ok(false) };
    let mut n = qtn_copy(mcx, s)?;
    n.flags |= QTN_NOCHANGE;
    *node = n;
    Ok(true)
}

fn compact(children: &mut PgVec<'_, QtNode<'_>>, mut keep: impl FnMut(usize) -> bool) {
    let mut j = 0usize;
    for i in 0..children.len() {
        if keep(i) {
            children.swap(i, j);
            j += 1;
        }
    }
    children.truncate(j);
}

// dofindsubquery (tsquery_rewrite.c): substitute at the root, else recurse;
// drop voided subtrees and simplify zero/one-child operator nodes.
fn dofindsubquery<'mcx>(
    mcx: Mcx<'mcx>,
    root: &mut QtNode<'mcx>,
    ex: &QtNode<'_>,
    subs: Option<&QtNode<'_>>,
    isfind: &mut bool,
) -> PgResult<bool> {
    // C tsquery_rewrite.c dofindsubquery(): check_stack_depth() alongside
    // CHECK_FOR_INTERRUPTS() (one frame per tree level).
    ::stack_depth::check_stack_depth()?;
    ::postgres_seams::check_for_interrupts::call()?;

    if !findeq(mcx, root, ex, subs, isfind)? {
        return Ok(false);
    }

    if root.flags & QTN_NOCHANGE == 0 {
        if let Item::Opr(opr) = root.item {
            let mut j = 0usize;
            for i in 0..root.children.len() {
                if dofindsubquery(mcx, &mut root.children[i], ex, subs, isfind)? {
                    root.children.swap(i, j);
                    j += 1;
                }
            }
            root.children.truncate(j);
            if root.children.is_empty() {
                return Ok(false);
            }
            if root.children.len() == 1 && opr.oper != OP_NOT {
                let only = root.children.pop().expect("one child");
                *root = only;
            }
        }
    }
    Ok(true)
}

// findsubquery (tsquery_rewrite.c). Both root and ex must be QTNTernary'd
// and QTNSort'ed.
pub fn findsubquery<'mcx>(
    mcx: Mcx<'mcx>,
    mut root: QtNode<'mcx>,
    ex: &QtNode<'_>,
    subs: Option<&QtNode<'_>>,
) -> PgResult<Option<QtNode<'mcx>>> {
    let mut did_find = false;
    Ok(dofindsubquery(mcx, &mut root, ex, subs, &mut did_find)?.then_some(root))
}

fn image_result(img: PgVec<'_, u8>) -> Datum {
    varlena_result(::datum::Varlena::from_image(img))
}

fn copy_image<'mcx>(mcx: Mcx<'mcx>, q: TsQueryRef<'_>) -> PgResult<PgVec<'mcx, u8>> {
    let mut img = vec_with_capacity_in(mcx, q.payload.len() + 4)?;
    img.extend_from_slice(&[0u8; 4]);
    ::mcx::vec_append_bytes(&mut img, q.payload)?;
    Ok(img)
}

// C truncates the input copy to HDRSIZETQ with size = 0.
fn empty_tsquery<'mcx>(mcx: Mcx<'mcx>) -> PgResult<PgVec<'mcx, u8>> {
    ::adt_tsquery_core::parse::build_query_image(mcx, &[], &[])
}

fn prepared_tree<'mcx>(mcx: Mcx<'mcx>, q: TsQueryRef<'_>) -> PgResult<QtNode<'mcx>> {
    let mut t = qt2qtn(mcx, q, 0)?;
    qtn_ternary(&mut t)?;
    qtn_sort(&mut t)?;
    Ok(t)
}

fn finish_tree<'mcx>(mcx: Mcx<'mcx>, tree: Option<QtNode<'mcx>>) -> PgResult<Datum> {
    match tree {
        Some(mut t) => {
            ::adt_tsquery_core::util::qtn_binary(mcx, &mut t)?;
            Ok(image_result(qtn2qt(mcx, &t)?))
        }
        None => Ok(image_result(empty_tsquery(mcx)?)),
    }
}

// tsquery_rewrite (3684): ts_rewrite(tsquery, tsquery, tsquery).
pub fn fc_tsquery_rewrite(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let query = arg_tsquery(fcinfo, 0)?;
    let ex = arg_tsquery(fcinfo, 1)?;
    let subst = arg_tsquery(fcinfo, 2)?;
    // SAFETY: the armed result mcx outlives this call.
    let mcx = unsafe { fcinfo.result_mcx_detached() };

    if query.size() == 0 || ex.size() == 0 {
        return Ok(image_result(copy_image(mcx, query)?));
    }

    let tree = prepared_tree(mcx, query)?;
    let qex = prepared_tree(mcx, ex)?;
    let subs = if subst.size() != 0 { Some(qt2qtn(mcx, subst, 0)?) } else { None };

    let tree = findsubquery(mcx, tree, &qex, subs.as_ref())?;
    finish_tree(mcx, tree)
}

// DatumGetTSQuery on a SPI tuple: C returns the stored image itself when it
// is already a plain 4-byte-header varlena and pfrees the copy otherwise
// (tsquery_rewrite.c:344-370), so the rule stream never accumulates.
enum RuleImage<'a, 'mcx> {
    Flat(&'a [u8]),
    Detoasted(PgVec<'mcx, u8>),
}

impl RuleImage<'_, '_> {
    fn as_query(&self) -> TsQueryRef<'_> {
        let flat: &[u8] = match self {
            RuleImage::Flat(s) => s,
            RuleImage::Detoasted(v) => v.as_slice(),
        };
        TsQueryRef { payload: &flat[::types_tuple::varatt::VARHDRSZ..] }
    }
}

fn tsquery_image_from_datum<'a, 'mcx>(mcx: Mcx<'mcx>, d: Datum) -> PgResult<RuleImage<'a, 'mcx>> {
    let p = d.as_usize() as *const u8;
    // SAFETY: a not-null tsquery column datum: a live varlena image readable
    // through its varsize_any extent, outliving the SPI tuptable it sits in.
    let image = unsafe { core::slice::from_raw_parts(p, ::types_tuple::varatt::varsize_any(p)) };
    if unsafe { ::types_tuple::varatt::varatt_is_4b_u(p) } {
        return Ok(RuleImage::Flat(image));
    }
    // pg_detoast_datum: normalizes short headers (the payload needs int32
    // alignment) and detoasts stored values.
    Ok(RuleImage::Detoasted(detoast::detoast_attr(mcx, image)?))
}

// tsquery_rewrite_query (3685): ts_rewrite(tsquery, text) — the SELECT must
// return two tsquery columns; each (target, substitute) row is applied in
// fetch order.
pub fn fc_tsquery_rewrite_query(
    _flinfo: Option<&mut FmgrInfo>,
    fcinfo: &mut Fcinfo,
) -> PgResult<Datum> {
    let query = arg_tsquery(fcinfo, 0)?;
    // SAFETY: the armed result mcx outlives this call.
    let mcx = unsafe { fcinfo.result_mcx_detached() };

    if query.size() == 0 {
        return Ok(image_result(copy_image(mcx, query)?));
    }

    // SAFETY: strict fn: arg 1 is a non-null live text varlena.
    let sql_v = unsafe { fcinfo.arg_varlena_packed(1) }?;
    let sql = core::str::from_utf8(sql_v.data())
        .map_err(|_| Box::new(PgError::error("invalid UTF-8 in ts_rewrite query text")))?;

    let mut tree = Some(prepared_tree(mcx, query)?);

    spi::SPI_connect()?;
    let plan = spi::SPI_prepare(sql, &[])?;
    let cursor = spi::SPI_cursor_open(None, plan, &[], &[], true)?;

    let mut checked_shape = false;
    loop {
        spi::SPI_cursor_fetch(&cursor, true, 100)?;
        let processed = spi::SPI_processed();
        let Some(h) = spi::SPI_tuptable() else { break };
        if !checked_shape {
            let shape_ok = spi::tuptable_with(h, |t| {
                t.tupdesc.natts == 2
                    && spi::SPI_gettypeid(&t.tupdesc, 1) == TSQUERYOID
                    && spi::SPI_gettypeid(&t.tupdesc, 2) == TSQUERYOID
            });
            if !shape_ok {
                return Err(
                    PgError::error("ts_rewrite query must return two tsquery columns")
                        .with_sqlstate(ERRCODE_INVALID_PARAMETER_VALUE)
                        .into(),
                );
            }
            checked_shape = true;
        }
        if processed == 0 || tree.is_none() {
            spi::SPI_freetuptable(h)?;
            break;
        }

        let step = |tree: &mut Option<QtNode<'_>>| -> PgResult<()> {
            spi::tuptable_with(h, |t| -> PgResult<()> {
                for tup in t.vals.iter() {
                    if tree.is_none() {
                        break;
                    }
                    let (qdata, isnull) = spi::SPI_getbinval(tup, &t.tupdesc, 1);
                    if isnull {
                        continue;
                    }
                    let (sdata, isnull) = spi::SPI_getbinval(tup, &t.tupdesc, 2);
                    if isnull {
                        continue;
                    }
                    let qtex_image = tsquery_image_from_datum(mcx, qdata)?;
                    let qtsubs_image = tsquery_image_from_datum(mcx, sdata)?;
                    let qtex = qtex_image.as_query();
                    let qtsubs = qtsubs_image.as_query();
                    if qtex.size() == 0 {
                        continue;
                    }
                    let qex = prepared_tree(mcx, qtex)?;
                    let qsubs =
                        if qtsubs.size() != 0 { Some(qt2qtn(mcx, qtsubs, 0)?) } else { None };
                    *tree = findsubquery(mcx, tree.take().expect("tree"), &qex, qsubs.as_ref())?;
                    if let Some(t) = tree.as_mut() {
                        // Ready the tree for another pass.
                        qtn_clear_flags(t, QTN_NOCHANGE)?;
                        qtn_ternary(t)?;
                        qtn_sort(t)?;
                    }
                }
                Ok(())
            })
        };
        step(&mut tree)?;
        spi::SPI_freetuptable(h)?;
    }

    spi::SPI_cursor_close(cursor)?;
    spi::SPI_freeplan(plan);
    spi::SPI_finish()?;

    finish_tree(mcx, tree)
}

const fn b(foid: ::types_core::Oid, name: &'static str, nargs: i16, func: PGFunction) -> FmgrBuiltin {
    FmgrBuiltin { foid, name, nargs, strict: true, retset: false, func }
}

pub const TSQUERY_REWRITE_BUILTINS: &[FmgrBuiltin] = &[
    b(3684, "tsquery_rewrite", 3, fc_tsquery_rewrite),
    b(3685, "tsquery_rewrite_query", 2, fc_tsquery_rewrite_query),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn tree<'mcx>(mcx: ::mcx::Mcx<'mcx>, text: &str) -> QtNode<'mcx> {
        let img = ::adt_tsquery_core::io::tsquery_in_core(mcx, text.as_bytes(), None).unwrap().unwrap();
        prepared_tree(mcx, TsQueryRef { payload: &img[4..] }).unwrap()
    }

    fn printed(mcx: ::mcx::Mcx<'_>, t: Option<QtNode<'_>>) -> String {
        let img = match t {
            Some(mut t) => {
                ::adt_tsquery_core::util::qtn_binary(mcx, &mut t).unwrap();
                qtn2qt(mcx, &t).unwrap()
            }
            None => empty_tsquery(mcx).unwrap(),
        };
        let out = ::adt_tsquery_core::io::tsquery_out_core(mcx, TsQueryRef { payload: &img[4..] }).unwrap();
        String::from_utf8(out[..out.len() - 1].to_vec()).unwrap()
    }

    // C edits the child array in place; no per-rule rebuild of the tree.
    #[test]
    fn findsubquery_edits_children_in_place() {
        ::postgres_seams::check_for_interrupts::set(|| Ok(()));
        let ctx = ::mcx::MemoryContext::new("tsquery_rewrite test");
        let mcx = ctx.mcx();
        let q = tree(mcx, "a | b | c | d | e | f | g | h");
        let buf = q.children.as_ptr();
        let q = findsubquery(mcx, q, &tree(mcx, "zz"), Some(&tree(mcx, "zz | yy"))).unwrap().unwrap();
        assert_eq!(q.children.as_ptr(), buf);
        let q = findsubquery(mcx, q, &tree(mcx, "c"), Some(&tree(mcx, "c & x"))).unwrap().unwrap();
        assert_eq!(q.children.as_ptr(), buf);
        let q = findsubquery(mcx, q, &tree(mcx, "d | e"), Some(&tree(mcx, "y"))).unwrap().unwrap();
        assert_eq!(q.children.as_ptr(), buf);
        assert_eq!(printed(mcx, Some(q)), "'a' | 'y' | 'g' | 'b' | 'f' | 'h' | 'c' & 'x'");
        let q = findsubquery(mcx, tree(mcx, "a | b"), &tree(mcx, "a"), None).unwrap();
        assert_eq!(printed(mcx, q), "'b'");
    }

    // tsquery_rewrite.c:344-370: a plain 4-byte-header rule datum is used in
    // place (no per-row copy); a short-header one is detoasted into an
    // owned image that drops with the row.
    #[test]
    fn rule_image_borrows_flat_datums() {
        let ctx = ::mcx::MemoryContext::new("tsquery_rewrite test");
        let mcx = ctx.mcx();
        let mut flat = [0u8; 12];
        flat[..4].copy_from_slice(&::types_tuple::varatt::set_varsize_4b_word(12).to_ne_bytes());
        let img = tsquery_image_from_datum(mcx, Datum::from_usize(flat.as_ptr() as usize)).unwrap();
        assert!(matches!(img, RuleImage::Flat(s) if s.len() == 12));
        assert_eq!(img.as_query().payload.len(), 8);

        #[cfg(target_endian = "little")]
        {
            let mut short = [0u8; 9];
            short[0] = (9 << 1) | 1;
            let img =
                tsquery_image_from_datum(mcx, Datum::from_usize(short.as_ptr() as usize)).unwrap();
            assert!(matches!(img, RuleImage::Detoasted(_)));
            assert_eq!(img.as_query().payload.len(), 8);
        }
    }
}
