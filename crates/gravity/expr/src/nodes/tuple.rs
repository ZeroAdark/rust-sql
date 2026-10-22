//! The tuple face (the tail lane's file, expr-compiler.md §4.4): composites read and formed
//! through their rowtype's descriptor. `FieldSelect` reads a field of a heap-composite image;
//! the whole-row `Var` forms a tuple from the row's slot (a `record` blessed in the typcache's
//! registry, a named rowtype checked against the slot); `FieldStore` deforms, replaces fields
//! (the old value through `CaseTest`) and re-forms; `RowExpr` forms from its arguments;
//! `ConvertRowtypeExpr` re-forms by attribute name or relabels the header. Descriptors live in
//! `State::descs` on their own context; every formed tuple is an image in the region.

use mcx::GrowIn;
use gravity_fn::{Datum, NullableDatum, Region};
use mcx::{Mcx, MemoryContext};
use types_core::Oid;
use types_error::{PgError, PgResult};
use types_nodes::primnodes::{ConvertRowtypeExpr, FieldSelect as FieldSelectTree, FieldStore as FieldStoreTree, RowExpr as RowExprTree};
use types_tuple::{varatt, HeapTupleData, HeapTupleHeaderData, ItemPointerData, TupleDescData};

use crate::eval::{eval, Cols, Cx, Source};
use crate::nodes::array::with_scratch;
use crate::nodes::field::{FieldSelect, FieldStore, NO_DESC};
use crate::nodes::row::{ConvertRowtype, RowExpr};
use crate::nodes::var::Var;
use crate::nodes::{unsupported, Builder, ExprNode, Kind, NodeId, Prog, St};
use crate::views::{copy_into, datum_of, detoast_into, image_view, no_space};
use crate::CompileResult;

const RECORDOID: Oid = 2249;

/// A descriptor the program owns: on its own context, `'static` for the state's life.
pub struct Desc {
    /// Declared before `cx`: the descriptor's arrays are freed before their context.
    pub desc: Option<TupleDescData<'static>>,
    cx: Box<MemoryContext>,
    key: Vec<Oid>,
    /// A conversion's out descriptor: its in descriptor's index (`u32::MAX` otherwise), which `refreshed` checks beside it
    /// though the emitted word names none for a header relabel.
    pub(crate) conv_in: u32,
}

impl Desc {
    fn new() -> Desc {
        Desc { desc: None, cx: Box::new(gravity_fn::session::root_context("gravity_expr tupdesc")), key: Vec::new(), conv_in: u32::MAX } // alloc: Program
    }

    fn mcx(&self) -> Mcx<'static> {
        // SAFETY: the context is boxed in this `Desc`, which the state keeps for the program's life.
        unsafe { core::mem::transmute::<Mcx<'_>, Mcx<'static>>(self.cx.mcx()) }
    }

    fn set(&mut self, desc: TupleDescData<'static>) -> &TupleDescData<'static> {
        self.desc = Some(desc);
        self.desc.as_ref().unwrap()
    }

    /// The descriptor's content as words (the owner signature: a program whose IR is another's must not share its
    /// descriptors — `ROW(c1, c2)` named `(a, b)` and `(x, y)` render differently).
    pub fn sig(&self, out: &mut Vec<u32>) {
        let Some(d) = self.desc.as_ref() else { return out.push(u32::MAX) };
        out.extend([d.natts as u32, d.tdtypeid, d.tdtypmod as u32]);
        for a in d.attrs.iter() {
            let name = a.attname.name_str();
            out.extend([a.atttypid, a.atttypmod as u32, name.iter().fold(0x811c9dc5u32, |h, &b| (h ^ b as u32).wrapping_mul(0x01000193))]);
        }
    }
}

/// A rowtype's descriptor copied from the typcache into a new `State::descs` entry.
fn rowtype_desc(b: &mut Builder<'_, '_>, typ: Oid, typmod: i32) -> CompileResult<u32> {
    let mut d = Desc::new();
    let desc = typcache::lookup_rowtype_tupdesc_copy(d.mcx(), typ, typmod)?;
    d.set(desc);
    Ok(push_desc(b, d))
}

fn push_desc(b: &mut Builder<'_, '_>, d: Desc) -> u32 {
    let ix = b.state.descs.len() as u32;
    b.state.descs.push(d);
    ix
}

pub fn compile_field_select<'mcx>(b: &mut Builder<'_, 'mcx>, f: &FieldSelectTree<'mcx>) -> CompileResult<NodeId> {
    let arg = b.compile(f.arg)?;
    let byref = !b.catalog.type_facts(f.resulttype)?.byval;
    let site = crate::nodes::field::field_site(b, byref);
    // a named rowtype's descriptor once, at compile time; `record` keeps the per-row lookup (its header names it)
    let argtyp = nodes_core::expr_type(f.arg);
    let desc = if argtyp == RECORDOID {
        NO_DESC
    } else {
        let mut typmod = -1;
        let base = b.catalog.base_type(argtyp, &mut typmod)?;
        if base == RECORDOID { NO_DESC } else { rowtype_desc(b, base, -1)? }
    };
    Ok(b.push(ExprNode::FieldSelect(FieldSelect { arg, fieldnum: f.fieldnum, typ: f.resulttype, site, desc })))
}

pub fn eval_field_select(p: &Prog<'_, '_>, st: &mut St<'_>, n: &FieldSelect, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    let v = eval(p, st, n.arg, cx)?;
    if v.isnull {
        return Ok(v);
    }
    let raw = v.value.as_usize() as *const u8;
    // SAFETY: a non-null composite datum addresses a live varlena image.
    if unsafe { varatt::varatt_is_external_expanded(raw) } {
        let (value, isnull) = expandedrecord_seams::expanded_record_field_select::call(v.value, n.fieldnum as i32, n.typ)?;
        return Ok(NullableDatum { value, isnull });
    }
    let img = image_view(v.value, -1, cx.region)?;
    let desc = if n.desc == NO_DESC { None } else { st.descs[n.desc as usize].desc.as_ref() };
    field_of(img.ptr, n.fieldnum, n.typ, desc)
}

/// C `ExecEvalFieldSelect`'s heap-composite leg over a plain image; `known` is the argument's compile-time descriptor,
/// used when the image's header names it (C's `argdesc` cache), else the typcache's copy for the header's rowtype.
pub fn field_of(img: *const u8, fieldnum: i16, typ: Oid, known: Option<&TupleDescData<'_>>) -> PgResult<NullableDatum> {
    // SAFETY: a plain 4-byte-header composite image: the tuple header prefix is in bounds.
    let hdr = unsafe { &*(img as *const HeapTupleHeaderData) };
    if fieldnum <= 0 {
        return Err(system_column(fieldnum));
    }
    if let Some(desc) = known.filter(|d| d.tdtypeid == hdr.type_id() && d.tdtypmod == hdr.typmod()) {
        return field_in(img, fieldnum, typ, desc);
    }
    with_scratch(|mcx| {
        let desc = typcache::lookup_rowtype_tupdesc_copy(mcx, hdr.type_id(), hdr.typmod())?;
        field_in(img, fieldnum, typ, &desc)
    })
}

fn field_in(img: *const u8, fieldnum: i16, typ: Oid, desc: &TupleDescData<'_>) -> PgResult<NullableDatum> {
    if fieldnum as i32 > desc.natts {
        return Err(beyond_columns(fieldnum, desc.natts));
    }
    let att = &desc.attrs[(fieldnum - 1) as usize];
    if att.attisdropped {
        return Ok(NullableDatum::null());
    }
    if typ != att.atttypid {
        return Err(wrong_type(fieldnum, att.atttypid, typ));
    }
    let tuple = tuple_over(img);
    let mut isnull = false;
    // SAFETY: `fieldnum` was validated against the descriptor above.
    let value = unsafe { types_tuple::heap_getattr(&tuple, fieldnum as i32, desc, &mut isnull) };
    core::mem::forget(tuple);
    Ok(NullableDatum { value, isnull })
}

/// A `HeapTupleData` view over a plain composite image (never dropped: `forget` it).
fn tuple_over<'a>(img: *const u8) -> HeapTupleData<'a> {
    // SAFETY: a plain composite image: MAXALIGN'd, `datum_length` bytes long.
    unsafe {
        let hdr = &*(img as *const HeapTupleHeaderData);
        HeapTupleData::from_raw_parts(img, hdr.datum_length(), ItemPointerData::invalid(), 0)
    }
}

/// `n` datum and null cells in the region.
pub(crate) fn row_cells<'a>(region: &mut Region, n: usize) -> PgResult<(&'a mut [Datum], &'a mut [bool])> {
    let v = region.reserve(n * 8).ok_or_else(no_space)?;
    let b = region.reserve(n.max(1)).ok_or_else(no_space)?;
    // SAFETY: both reservations are inside the region and sized for `n` entries.
    Ok(unsafe { (core::slice::from_raw_parts_mut(v.cast::<Datum>(), n), core::slice::from_raw_parts_mut(b.cast::<bool>(), n)) })
}

/// `heap_form_tuple` over the descriptor on the scratch context, the image copied into the region. A composite datum
/// carries no external toast pointer (C `HeapTupleGetDatum` -> `HeapTupleHeaderGetDatum`, execTuples.c:2413, the law
/// `heap_copy_tuple_as_datum` and the row executor's `ExecEvalRow` keep): a formed tuple whose field is a by-reference
/// lane's toast pointer is flattened on the scratch context first, else `INSERT INTO dest SELECT ARRAY[ROW(f1, f1)]
/// FROM src` stores a pointer into `src`'s toast table (pg_regress `arrays`: "unexpected chunk number" once `src` is
/// deleted from).
pub(crate) fn form(desc: &TupleDescData<'_>, values: &[Datum], nulls: &[bool], region: &mut Region) -> PgResult<NullableDatum> {
    with_scratch(|mcx| {
        let tuple = heaptuple::heap_form_tuple(mcx, desc, values, nulls)?;
        let img = if tuple.as_tuple().has_external() {
            detoast_seams::toast_flatten_tuple_to_datum::call(mcx, tuple.as_tuple(), desc)?.as_usize() as *const u8
        } else {
            tuple.image().as_ptr()
        };
        Ok(NullableDatum::value(copy_into(region, datum_of(img), -1)?))
    })
}

/// Deform a composite image into `values` / `nulls`.
pub(crate) fn deform(img: *const u8, desc: &TupleDescData<'_>, values: &mut [Datum], nulls: &mut [bool]) {
    let tuple = tuple_over(img);
    types_tuple::heap_deform_tuple(&tuple, desc, values, nulls);
    core::mem::forget(tuple);
}

pub fn compile_whole_row(b: &mut Builder<'_, '_>, src: Source, typ: Oid) -> CompileResult<NodeId> {
    let desc = if typ == RECORDOID {
        push_desc(b, Desc::new())
    } else {
        let mut typmod = -1;
        let base = b.catalog.base_type(typ, &mut typmod)?;
        rowtype_desc(b, base, -1)?
    };
    Ok(b.push(ExprNode::Var(Var { src, attno: 0, typ, desc })))
}

/// The whole-row `Var` (C `ExecEvalWholeRowVar`): the slot's columns as a composite datum.
pub fn eval_whole_row(n: &Var, st: &mut St<'_>, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    let src = cx.row.source(n.src);
    let Cols::Slot { values, nulls, desc: Some(slot_desc), .. } = *src else { return Err(no_descriptor()) };
    let d = &mut st.descs[n.desc as usize];
    if n.typ == RECORDOID {
        bless_record(d, slot_desc)?;
    } else {
        check_named(d.desc.as_ref().expect("a named rowtype binds its descriptor"), slot_desc)?;
    }
    let natts = slot_desc.natts as usize;
    let (vals, nls) = row_cells(cx.region, natts)?;
    for i in 0..natts {
        vals[i] = values[i];
        nls[i] = nulls[i];
        let att = &slot_desc.attrs[i];
        if !nls[i] && att.attlen == -1 {
            let p = vals[i].as_usize() as *const u8;
            // SAFETY: a non-null varlena column addresses a live image; an external one is expanded.
            if unsafe { varatt::varatt_is_1b_e(p) } {
                vals[i] = datum_of(unsafe { detoast_into(cx.region, p)? });
            }
        }
    }
    let desc = st.descs[n.desc as usize].desc.as_ref().expect("resolved above");
    form(desc, vals, nls, cx.region)
}

/// The `record` leg: the slot's descriptor copied, stamped `record` and blessed (its typmod from
/// the typcache's registry), cached by the slot's attribute types.
fn bless_record(d: &mut Desc, slot_desc: &TupleDescData<'_>) -> PgResult<()> {
    let same = d.desc.is_some() && d.key.len() == slot_desc.natts as usize && d.key.iter().zip(slot_desc.attrs.iter()).all(|(&k, a)| k == a.atttypid);
    if same {
        return Ok(());
    }
    d.desc = None;
    d.cx.reset();
    let mut desc = tupdesc::CreateTupleDescCopy(d.mcx(), slot_desc)?;
    desc.tdtypeid = RECORDOID;
    desc.tdtypmod = -1;
    typcache::assign_record_type_typmod(&mut desc)?;
    d.key = slot_desc.attrs.iter().map(|a| a.atttypid).collect(); // alloc: Cold
    d.set(desc);
    Ok(())
}

/// The named leg's check: the slot's row matches the rowtype (C's `42804` details).
fn check_named(var_desc: &TupleDescData<'_>, slot_desc: &TupleDescData<'_>) -> PgResult<()> {
    if var_desc.natts != slot_desc.natts {
        return Err(row_type_mismatch(format!("Table row contains {} {}, but query expects {}.", slot_desc.natts, if slot_desc.natts == 1 { "attribute" } else { "attributes" }, var_desc.natts))); // alloc: Cold
    }
    for i in 0..var_desc.natts as usize {
        let (v, s) = (&var_desc.attrs[i], &slot_desc.attrs[i]);
        if v.atttypid == s.atttypid {
            continue;
        }
        if !v.attisdropped {
            let f = |o: Oid| format_type::format_type_be(o).unwrap_or_else(|_| o.to_string()); // alloc: Cold
            return Err(row_type_mismatch(format!("Table has type {} at ordinal position {}, but query expects {}.", f(s.atttypid), i + 1, f(v.atttypid)))); // alloc: Cold
        }
        if v.attlen != s.attlen || v.attalign != s.attalign {
            return Err(row_type_mismatch(format!("Physical storage mismatch on dropped attribute at ordinal position {}.", i + 1))); // alloc: Cold
        }
    }
    Ok(())
}

pub fn compile_field_store<'mcx>(b: &mut Builder<'_, 'mcx>, f: &FieldStoreTree<'mcx>) -> CompileResult<NodeId> {
    let desc = rowtype_desc(b, f.resulttype, -1)?;
    let natts = b.state.descs[desc as usize].desc.as_ref().unwrap().natts;
    let arg = b.compile(f.arg)?;
    let fieldnums = b.prog.oids.len() as u32;
    for fnum in f.fieldnums.iter() {
        if fnum <= 0 || fnum > natts {
            return Err(Box::new(PgError::error(format!("field number {fnum} is out of range in FieldStore"))).into()); // alloc: Cold
        }
        b.prog.oids.push_in(fnum as Oid);
    }
    let newvals = b.kids(&f.newvals)?;
    if newvals.len as usize != f.fieldnums.len() {
        return unsupported(Kind::FieldStore);
    }
    let site = crate::emit::store::field_store_site(b, newvals.len as usize);
    Ok(b.push(ExprNode::FieldStore(FieldStore { arg, newvals, fieldnums, typ: f.resulttype, desc, site })))
}

pub fn eval_field_store(p: &Prog<'_, '_>, st: &mut St<'_>, n: &FieldStore, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    let v = eval(p, st, n.arg, cx)?;
    let natts = st.descs[n.desc as usize].desc.as_ref().unwrap().natts as usize;
    let (vals, nls) = row_cells(cx.region, natts)?;
    if v.isnull {
        vals.fill(Datum::null());
        nls.fill(true);
    } else {
        let img = image_view(v.value, -1, cx.region)?;
        deform(img.ptr, st.descs[n.desc as usize].desc.as_ref().unwrap(), vals, nls);
    }
    for i in 0..n.newvals.len as usize {
        let f = p.oids[n.fieldnums as usize + i] as usize - 1;
        let saved = cx.casetest;
        cx.casetest = NullableDatum { value: vals[f], isnull: nls[f] };
        let r = eval(p, st, p.kids[n.newvals.start as usize + i], cx);
        cx.casetest = saved;
        let r = r?;
        vals[f] = r.value;
        nls[f] = r.isnull;
    }
    form(st.descs[n.desc as usize].desc.as_ref().unwrap(), vals, nls, cx.region)
}

pub fn compile_row_expr<'mcx>(b: &mut Builder<'_, 'mcx>, r: &RowExprTree<'mcx>) -> CompileResult<NodeId> {
    let nargs = r.args.len();
    let desc = if r.row_typeid == RECORDOID {
        let mut d = Desc::new();
        let mut desc = tupdesc::CreateTemplateTupleDesc(d.mcx(), nargs as i32)?;
        for (i, e) in r.args.iter().enumerate() {
            let colname = if r.colnames.len() > i { r.colnames.nth(i).as_string().map(|s| s.sval) } else { None };
            tupdesc::TupleDescInitEntry(&mut desc, (i + 1) as i16, colname, nodes_core::expr_type(e), nodes_core::expr_typmod(e), 0)?;
            // D1887: C ExecTypeFromExprList's column collation, the argument's (record_cmp / record_eq compare under it)
            tupdesc::TupleDescInitEntryCollation(&mut desc, (i + 1) as i16, nodes_core::expr_collation(e));
        }
        desc.tdtypeid = RECORDOID;
        desc.tdtypmod = -1;
        typcache::assign_record_type_typmod(&mut desc)?;
        d.set(desc);
        push_desc(b, d)
    } else {
        rowtype_desc(b, r.row_typeid, -1)?
    };
    let natts = b.state.descs[desc as usize].desc.as_ref().unwrap().natts as usize;
    // CR-WIDE-ROW: a row past one site's values spans continuation sites (`row::row_site`)
    if nargs > natts {
        return unsupported(Kind::RowExpr);
    }
    for (i, e) in r.args.iter().enumerate() {
        let att = &b.state.descs[desc as usize].desc.as_ref().unwrap().attrs[i];
        if att.attisdropped {
            continue;
        }
        let have = nodes_core::expr_type(e);
        if have != att.atttypid {
            let f = |o: Oid| format_type::format_type_be(o).unwrap_or_else(|_| o.to_string()); // alloc: Cold
            return Err(Box::new(PgError::error(format!("ROW() column has type {} instead of type {}", f(have), f(att.atttypid))).with_sqlstate(types_error::ERRCODE_DATATYPE_MISMATCH)).into()); // alloc: Cold
        }
    }
    let args = b.kids(&r.args)?;
    let site = crate::nodes::row::row_site(b, nargs);
    Ok(b.push(ExprNode::Row(RowExpr { args, row_type: r.row_typeid, desc, natts: natts as u32, site })))
}

/// EXPR-ODDS A: a named tuplestore scan's whole row as a `record` (C `ExecEvalWholeRowVar`'s RECORD leg): the scan slot's
/// descriptor, the ENR's (`ENRMetadataGetTupDesc`: a transition table's is relation `relid`'s own, dropped attributes
/// kept), copied, reset to `record`, its columns renamed by `names` (the RTE's `eref`, `ExecTypeSetColNames`: an empty
/// name or a dropped column keeps its own) and blessed; the row is that descriptor over `Var(src, 1..n)`, `n` the last
/// live column (trailing dropped columns take no argument: the row leaves them NULL), a dropped one before it reading
/// the first live column, whose value the row skips (`attisdropped`, as `eval_row_expr` and the row site do).
pub fn compile_relation_record(b: &mut Builder<'_, '_>, src: Source, relid: Oid, names: &[&str]) -> CompileResult<NodeId> {
    let reltype = lsyscache::get_rel_type_id(relid)?;
    let mut d = Desc::new();
    let mut desc = typcache::lookup_rowtype_tupdesc_copy(d.mcx(), reltype, -1)?;
    desc.tdtypeid = RECORDOID;
    desc.tdtypmod = -1;
    for (i, name) in names.iter().enumerate().take(desc.natts.max(0) as usize) {
        let att = desc.attr_mut(i);
        if !name.is_empty() && !att.attisdropped {
            att.attname.namestrcpy(name);
        }
    }
    typcache::assign_record_type_typmod(&mut desc)?;
    let cols: Vec<(bool, Oid)> = desc.attrs.iter().take(desc.natts.max(0) as usize).map(|a| (a.attisdropped, a.atttypid)).collect(); // alloc: Program
    d.set(desc);
    let n = cols.iter().rposition(|c| !c.0).map_or(0, |p| p + 1);
    let ix = push_desc(b, d);
    let first = cols.iter().position(|c| !c.0).unwrap_or(0);
    let args: Vec<NodeId> = (0..n).map(|a| {
        let a = if cols[a].0 { first } else { a };
        b.push(ExprNode::Var(Var { src, attno: a as i16 + 1, typ: cols[a].1, desc: 0 }))
    }).collect(); // alloc: Program
    let kids = b.kids_of(&args);
    let site = crate::nodes::row::row_site(b, n);
    Ok(b.push(ExprNode::Row(RowExpr { args: kids, row_type: RECORDOID, desc: ix, natts: cols.len() as u32, site })))
}

pub fn eval_row_expr(p: &Prog<'_, '_>, st: &mut St<'_>, n: &RowExpr, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    let natts = n.natts as usize;
    let (vals, nls) = row_cells(cx.region, natts)?;
    vals.fill(Datum::null());
    nls.fill(true);
    // D2789: a whole row's image keeps a dropped attribute's argument, as the site does (`row::image_site`)
    let image = crate::nodes::row::keeps_dropped(p, n.site);
    for i in 0..n.args.len as usize {
        if st.descs[n.desc as usize].desc.as_ref().unwrap().attrs[i].attisdropped && !image {
            continue;
        }
        let r = eval(p, st, p.kids[n.args.start as usize + i], cx)?;
        vals[i] = r.value;
        nls[i] = r.isnull;
    }
    form(st.descs[n.desc as usize].desc.as_ref().unwrap(), vals, nls, cx.region)
}

pub fn compile_convert_rowtype<'mcx>(b: &mut Builder<'_, 'mcx>, c: &ConvertRowtypeExpr<'mcx>) -> CompileResult<NodeId> {
    let arg = b.compile(c.arg)?;
    let intype = nodes_core::expr_type(c.arg);
    let indesc = rowtype_desc(b, intype, -1)?;
    let outdesc = rowtype_desc(b, c.resulttype, -1)?;
    b.state.descs[outdesc as usize].conv_in = indesc;
    let map = attr_map(b, indesc, outdesc)?;
    // the emitted site carries the map as constant words; one wider than a site's arguments is the emit's refusal
    let outnatts = b.state.descs[outdesc as usize].desc.as_ref().unwrap().natts as usize;
    let wide = map.is_some() && crate::nodes::row::CONVERT_HEAD + outnatts > crate::nodes::func::MAX_ARGS;
    let site = crate::nodes::row::convert_site(b, if map.is_some() && !wide { outnatts } else { 0 });
    Ok(b.push(ExprNode::ConvertRowtype(ConvertRowtype { arg, result_type: c.resulttype, indesc, outdesc, map, site, wide })))
}

/// `build_attrmap_by_name` + `check_attrmap_match`: the map into `Program::oids`, or none when
/// the rows convert by a header relabel alone.
fn attr_map(b: &mut Builder<'_, '_>, indesc: u32, outdesc: u32) -> CompileResult<Option<u32>> {
    let ind = b.state.descs[indesc as usize].desc.as_ref().unwrap();
    let outd = b.state.descs[outdesc as usize].desc.as_ref().unwrap();
    let Some(map) = map_by_name(ind, outd)? else { return Ok(None) };
    let at = b.prog.oids.len() as u32;
    b.prog.oids.extend_in(map.iter().map(|&m| m as Oid));
    Ok(Some(at))
}

/// `build_attrmap_by_name` + `check_attrmap_match` over two descriptors: out attribute `i` from in attribute `map[i]`
/// (0: a dropped out attribute), or `None` when the rows convert by a header relabel alone.
fn map_by_name(ind: &TupleDescData<'_>, outd: &TupleDescData<'_>) -> Result<Option<Vec<i16>>, Box<PgError>> {
    let (innatts, outnatts) = (ind.natts as usize, outd.natts as usize);
    let mut map = vec![0i16; outnatts]; // alloc: Program
    let mut next: isize = -1;
    for i in 0..outnatts {
        let outatt = &outd.attrs[i];
        if outatt.attisdropped {
            continue;
        }
        for _ in 0..innatts {
            next += 1;
            if next >= innatts as isize {
                next = 0;
            }
            let inatt = &ind.attrs[next as usize];
            if inatt.attisdropped || inatt.attname.name_str() != outatt.attname.name_str() {
                continue;
            }
            if outatt.atttypid != inatt.atttypid || outatt.atttypmod != inatt.atttypmod {
                return Err(row_convert_error(outatt.attname.name_str(), outd.tdtypeid, ind.tdtypeid, true));
            }
            map[i] = inatt.attnum;
            break;
        }
        if map[i] == 0 {
            return Err(row_convert_error(outatt.attname.name_str(), outd.tdtypeid, ind.tdtypeid, false));
        }
    }
    let identity = innatts == outnatts && (0..outnatts).all(|i| {
        let (ia, oa) = (&ind.attrs[i], &outd.attrs[i]);
        map[i] == (i + 1) as i16 || (map[i] == 0 && ia.attisdropped && oa.attisdropped && ia.attlen == oa.attlen && ia.attalign == oa.attalign)
    });
    Ok((!identity).then_some(map))
}

/// D2803: both rowtypes of a conversion as this backend's typcache holds them now, with their by-name map, when DDL run
/// inside the statement (user code the query calls: an ALTER TABLE in a SQL or PL function) changed either after the
/// program compiled its descriptors; `None` while both are as compiled. C ExecEvalConvertRowtype looks both up at each
/// evaluation (get_cached_rowtype) and rebuilds its map when either changed. A pair found current is not looked up again
/// until the typcache takes another relation invalidation (`typcache::rel_inval_count`), so a statement with no DDL
/// pays one counter read per row.
pub(crate) struct Fresh {
    pub(crate) ind: std::rc::Rc<TupleDescData<'static>>,
    pub(crate) outd: std::rc::Rc<TupleDescData<'static>>,
    pub(crate) map: Option<Vec<i16>>,
}

thread_local! {
    /// The invalidation count the pairs were checked at, and the (in, out) descriptor addresses found current then.
    static CURRENT: core::cell::RefCell<(u64, Vec<(usize, usize)>)> = const { core::cell::RefCell::new((u64::MAX, Vec::new())) }; // alloc: Cold
}

/// The pairs `CURRENT` keeps before it starts over (a long session's many programs; a dropped one only costs a lookup).
const CURRENT_PAIRS: usize = 64;

pub(crate) fn refreshed(ind: &TupleDescData<'static>, outd: &TupleDescData<'static>) -> PgResult<Option<Fresh>> {
    if ind.tdtypeid == RECORDOID || outd.tdtypeid == RECORDOID {
        return Ok(None);
    }
    let n = typcache::rel_inval_count();
    let key = (ind as *const TupleDescData<'static> as usize, outd as *const TupleDescData<'static> as usize);
    let seen = CURRENT.with(|c| {
        let mut c = c.borrow_mut();
        if c.0 != n {
            c.0 = n;
            c.1.clear();
        }
        c.1.contains(&key)
    });
    if seen {
        return Ok(None);
    }
    let fi = current_rowtype(ind.tdtypeid)?;
    let fo = current_rowtype(outd.tdtypeid)?;
    if tupdesc::equalRowTypes(&fi, ind) && tupdesc::equalRowTypes(&fo, outd) {
        CURRENT.with(|c| {
            let mut c = c.borrow_mut();
            if c.1.len() >= CURRENT_PAIRS {
                c.1.clear();
            }
            c.1.push(key); // alloc: Cold
        });
        return Ok(None);
    }
    let map = map_by_name(&fi, &fo)?;
    Ok(Some(Fresh { ind: fi, outd: fo, map }))
}

/// A named rowtype's descriptor as the typcache holds it now.
fn current_rowtype(typ: Oid) -> PgResult<std::rc::Rc<TupleDescData<'static>>> {
    let e = typcache::lookup_type_cache(typ, typcache::TYPECACHE_TUPDESC)?;
    e.tupdesc().ok_or_else(|| {
        let name = format_type::format_type_be(typ).unwrap_or_else(|_| typ.to_string()); // alloc: Cold
        Box::new(PgError::error(format!("type {name} is not composite")).with_sqlstate(types_error::ERRCODE_WRONG_OBJECT_TYPE)) // alloc: Cold
    })
}

/// `convert_image` by `refreshed`'s current descriptors.
pub(crate) fn convert_fresh(img: *const u8, f: &Fresh, region: &mut Region) -> PgResult<NullableDatum> {
    let ind = f.map.as_ref().map(|_| &*f.ind);
    convert_image(img, ind, &f.outd, |i| f.map.as_ref().map_or(0, |m| m[i] as usize), region)
}

pub fn eval_convert_rowtype(p: &Prog<'_, '_>, st: &mut St<'_>, n: &ConvertRowtype, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    let v = eval(p, st, n.arg, cx)?;
    if v.isnull {
        return Ok(v);
    }
    let img = image_view(v.value, -1, cx.region)?;
    let outd = st.descs[n.outdesc as usize].desc.as_ref().unwrap();
    if let Some(f) = refreshed(st.descs[n.indesc as usize].desc.as_ref().unwrap(), outd)? {
        return convert_fresh(img.ptr, &f, cx.region);
    }
    let ind = n.map.map(|_| st.descs[n.indesc as usize].desc.as_ref().unwrap());
    let at = n.map.unwrap_or(0) as usize;
    convert_image(img.ptr, ind, outd, |i| p.oids[at + i] as usize, cx.region)
}

/// C `ExecEvalConvertRowtype` over a composite image: with no in descriptor the rows convert by a header relabel,
/// else the image deforms by `ind` and re-forms by `outd`, out attribute `i` from in attribute `map(i)` (0: NULL).
pub(crate) fn convert_image(img: *const u8, ind: Option<&TupleDescData<'static>>, outd: &TupleDescData<'static>, map: impl Fn(usize) -> usize, region: &mut Region) -> PgResult<NullableDatum> {
    let Some(ind) = ind else {
        let d = copy_into(region, datum_of(img), -1)?;
        // SAFETY: the copy is a plain composite image; its header carries the rowtype.
        let hdr = unsafe { &mut *(d.as_usize() as *mut HeapTupleHeaderData) };
        hdr.set_type_id(outd.tdtypeid);
        hdr.set_typmod(outd.tdtypmod);
        return Ok(NullableDatum::value(d));
    };
    let (invals, innls) = row_cells(region, ind.natts as usize)?;
    deform(img, ind, invals, innls);
    let outn = outd.natts as usize;
    let (outvals, outnls) = row_cells(region, outn)?;
    for i in 0..outn {
        let attno = map(i);
        if attno > 0 {
            outvals[i] = invals[attno - 1];
            outnls[i] = innls[attno - 1];
        } else {
            outvals[i] = Datum::null();
            outnls[i] = true;
        }
    }
    form(outd, outvals, outnls, region)
}

pub fn composite_datum(img: *const u8) -> Datum {
    datum_of(img)
}

#[cold]
#[inline(never)]
fn no_descriptor() -> Box<PgError> {
    Box::new(PgError::error("gravity_expr: a whole-row reference needs the row's descriptor").with_sqlstate(types_error::ERRCODE_FEATURE_NOT_SUPPORTED)) // alloc: Cold
}

#[cold]
#[inline(never)]
fn row_type_mismatch(detail: String) -> Box<PgError> {
    Box::new(PgError::error("table row type and query-specified row type do not match").with_sqlstate(types_error::ERRCODE_DATATYPE_MISMATCH).with_detail(detail)) // alloc: Cold
}

#[cold]
#[inline(never)]
fn row_convert_error(attname: &[u8], outtype: Oid, intype: Oid, mismatch: bool) -> Box<PgError> {
    let name = String::from_utf8_lossy(attname).into_owned(); // alloc: Cold
    let f = |o: Oid| format_type::format_type_be(o).unwrap_or_else(|_| o.to_string()); // alloc: Cold
    let detail = if mismatch {
        format!("Attribute \"{name}\" of type {} does not match corresponding attribute of type {}.", f(outtype), f(intype)) // alloc: Cold
    } else {
        format!("Attribute \"{name}\" of type {} does not exist in type {}.", f(outtype), f(intype)) // alloc: Cold
    };
    Box::new(PgError::error("could not convert row type").with_sqlstate(types_error::ERRCODE_DATATYPE_MISMATCH).with_detail(detail)) // alloc: Cold
}

#[cold]
#[inline(never)]
fn system_column(fieldnum: i16) -> Box<PgError> {
    Box::new(PgError::error(format!("unsupported reference to system column {fieldnum} in FieldSelect")).with_sqlstate(types_error::ERRCODE_INTERNAL_ERROR)) // alloc: Cold
}

#[cold]
#[inline(never)]
fn beyond_columns(fieldnum: i16, natts: i32) -> Box<PgError> {
    Box::new(PgError::error(format!("attribute number {fieldnum} exceeds number of columns {natts}")).with_sqlstate(types_error::ERRCODE_INTERNAL_ERROR)) // alloc: Cold
}

#[cold]
#[inline(never)]
fn wrong_type(fieldnum: i16, has: Oid, wants: Oid) -> Box<PgError> {
    let f = |o: Oid| format_type::format_type_be(o).unwrap_or_else(|_| o.to_string()); // alloc: Cold
    Box::new(PgError::error(format!("attribute {fieldnum} has wrong type")).with_sqlstate(types_error::ERRCODE_DATATYPE_MISMATCH).with_detail(format!("Table has type {}, but query expects {}.", f(has), f(wants)))) // alloc: Cold
}
