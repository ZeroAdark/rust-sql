//! `RowExpr` / `ConvertRowtypeExpr` through the tail lane's tuple face (`tuple.rs`); the emitted `RowExpr` is a NON-strict
//! synthetic site per node (`ROW_SITE`: the descriptor word from the owner's cell, the null mask, then the arguments by value).

use mcx::GrowIn;
use gravity_fn::{Addr, Arg, ArgKind, Datum, ErrSlot, Family, NullableDatum, Region, Ret, Shim, ERROR, NULL, OK};
use types_core::Oid;
use types_error::PgResult;
use types_nodes::primnodes::{ConvertRowtypeExpr, RowExpr as RowExprTree};
use types_tuple::TupleDescData;

use crate::eval::Cx;
use crate::nodes::func::{ArgConv, Call, How};
use crate::nodes::{Builder, Kids, NodeId, Prog, SiteId, St};
use crate::CompileResult;

#[derive(Clone, Copy, Debug)]
pub struct RowExpr {
    pub args: Kids,
    pub row_type: Oid,
    pub desc: u32,
    pub natts: u32,
    /// The emitted row's site (`ROW_SITE`).
    pub site: SiteId,
}

pub const HEAD: usize = 2;
/// The values one masked site carries (`func::masked` within a call's `MAX_ARGS`); a wider row spans continuation sites
/// (`row_site`, `wide`).
pub const MAX_EMITTED_ARGS: usize = crate::nodes::func::MAX_MASKED;

/// A row of `nargs` values spans continuation sites (`row_site`): more than one masked site carries.
pub const fn wide(nargs: usize) -> bool {
    nargs > MAX_EMITTED_ARGS
}

/// `ROW_WIDE`'s words: its own cell (`emit::host::Cell`, `aux` the descriptor as `ROW_SITE`'s) and the value count.
pub const WIDE_HEAD: usize = 2;

/// The words a wide row of `nargs` values stores into its continuation cells: the null mask's words, then the values
/// (`func::masked`'s order past the head).
pub const fn wide_words(nargs: usize) -> usize {
    crate::nodes::func::mask_words(nargs) + nargs
}

/// The continuation sites of a wide row: `MAX_ARGS` of its words per site.
pub const fn more_sites(nargs: usize) -> usize {
    wide_words(nargs).div_ceil(crate::nodes::func::MAX_ARGS)
}

/// The row's site. A row of at most `MAX_EMITTED_ARGS` values is one `ROW_SITE` (the head, the null mask, the values). A
/// wider one (CR-WIDE-ROW: a whole row, `ROW(...)` or a subquery's record past 97 columns, up to PG's 1664) is a `ROW_WIDE`
/// site over its own cell and the count, followed by its continuation sites (`continuation_sites`).
pub fn row_site(b: &mut Builder<'_, '_>, nargs: usize) -> SiteId {
    site_of(b, nargs, false)
}

/// The `more_sites(nargs)` `MORE_SITE` sites after a wide site `site` (the next site ids; CR-WIDE-ROW, and PS4-10's wide
/// ARRAY[]): the emitted code stores the mask words and the values into their cells before the call, and they are never
/// called: no call's argument array grows, so every other site keeps its cell.
pub fn continuation_sites(b: &mut Builder<'_, '_>, site: SiteId, nargs: usize) {
    let words = wide_words(nargs);
    for k in 0..more_sites(nargs) {
        let n = (words - k * crate::nodes::func::MAX_ARGS).min(crate::nodes::func::MAX_ARGS);
        let convs = b.prog.convs.len() as u32;
        for _ in 0..n {
            b.prog.convs.push_in(ArgConv { kind: ArgKind::V, typlen: 0 });
        }
        let more = b.site(Call { funcid: 0, args: Kids::NONE, nargs: n as u8, strict: false, collation: 0, how: How::Shim { shim: &MORE_SITE, convs, byref: false }, rettype: 0, volatile: false });
        debug_assert_eq!(more.0 as usize, site.0 as usize + 1 + k, "a wide site's continuation sites follow it");
    }
}

/// Word `w` of a wide site's continuation cells: argument `w % MAX_ARGS` of the cell `1 + w / MAX_ARGS` after `cell`. The
/// continuation sites are `MAX_ARGS` wide, so the program's cells are (`emit::host::cell_width`): `cell_size(MAX_ARGS)`
/// apart.
///
/// # Safety
/// `cell` is a wide site's own cell of the program's live `EmitState`, the emitted code stored its `more_sites(n)`
/// continuation cells before the call, and `w < wide_words(n)`.
#[inline(always)]
pub unsafe fn continued_word(cell: *const crate::emit::host::Cell, w: usize) -> i64 {
    use crate::emit::host::{cell_size, Cell};
    use crate::nodes::func::MAX_ARGS;
    // SAFETY: the caller's: the cells are `MAX_ARGS` wide and the continuation cell is in the same state's buffer.
    unsafe {
        debug_assert_eq!((*cell).width as usize, MAX_ARGS, "a wide site's cells are MAX_ARGS wide");
        let more = cell.cast::<u8>().add((1 + w / MAX_ARGS) * cell_size(MAX_ARGS)) as *mut Cell;
        Cell::args(more)[w % MAX_ARGS]
    }
}

/// D2789: a whole row's site (`ROW_IMAGE` / `ROW_WIDE_IMAGE`): `row_site`'s, except that a dropped attribute keeps the
/// value its argument carries (the scan's stored datum), as C `ExecEvalWholeRowVar` forms the image from the slot's every
/// attribute (`toast_build_flattened_tuple` after `slot_getallattrs`). A `ROW(...)` leaves a dropped attribute NULL (C
/// `ExecInitExprRec`'s T_RowExpr arm puts a NULL there), so only a whole row takes this site.
pub fn image_site(b: &mut Builder<'_, '_>, nargs: usize) -> SiteId {
    site_of(b, nargs, true)
}

/// The site forms `image`: a whole row keeps a dropped attribute's argument (`image_site`). The shim by its name, never
/// its address (D2916).
pub fn keeps_dropped(p: &Prog<'_, '_>, site: SiteId) -> bool {
    p.sites.get(site.0 as usize).is_some_and(|c| matches!(c.how, How::Shim { shim, .. } if shim.name == ROW_IMAGE.name || shim.name == ROW_WIDE_IMAGE.name))
}

fn site_of(b: &mut Builder<'_, '_>, nargs: usize, image: bool) -> SiteId {
    if wide(nargs) {
        let convs = b.prog.convs.len() as u32;
        for _ in 0..WIDE_HEAD {
            b.prog.convs.push_in(ArgConv { kind: ArgKind::V, typlen: 0 });
        }
        let shim = if image { &ROW_WIDE_IMAGE } else { &ROW_WIDE };
        let site = b.site(Call { funcid: 0, args: Kids::NONE, nargs: WIDE_HEAD as u8, strict: false, collation: 0, how: How::Shim { shim, convs, byref: true }, rettype: 0, volatile: false });
        continuation_sites(b, site, nargs);
        return site;
    }
    let n = 1 + crate::nodes::func::mask_words(nargs) + nargs;
    let convs = b.prog.convs.len() as u32;
    for _ in 0..n {
        b.prog.convs.push_in(ArgConv { kind: ArgKind::V, typlen: 0 });
    }
    let shim = if image { &ROW_IMAGE } else { &ROW_SITE };
    b.site(Call { funcid: 0, args: Kids::NONE, nargs: n as u8, strict: false, collation: 0, how: How::Shim { shim, convs, byref: true }, rettype: 0, volatile: false })
}

gravity_abi::host! {
    #[unwind]
    unsafe fn no_addr() {}
}

pub static ROW_SITE: Shim = Shim { oid: 0, name: "row form", family: Family::None, nargs: HEAD as u8, coll: false, region: true, args: &[ArgKind::V, ArgKind::V], tyargs: &[], addr: Addr(no_addr), call: row_shim };

fn row_shim(e: *mut ErrSlot, region: *mut Region, _coll: u32, _ty: &[u32], a: &[Arg]) -> Ret {
    row_call(e, region, a, false)
}

/// D2789: `ROW_SITE` for a whole row (`image_site`).
pub static ROW_IMAGE: Shim = Shim { oid: 0, name: "row image", family: Family::None, nargs: HEAD as u8, coll: false, region: true, args: &[ArgKind::V, ArgKind::V], tyargs: &[], addr: Addr(no_addr), call: image_shim };

fn image_shim(e: *mut ErrSlot, region: *mut Region, _coll: u32, _ty: &[u32], a: &[Arg]) -> Ret {
    row_call(e, region, a, true)
}

fn row_call(e: *mut ErrSlot, region: *mut Region, a: &[Arg], image: bool) -> Ret {
    let (masks, vals) = crate::nodes::func::masked(a);
    // SAFETY: the host passes the cell's live region; `a[0]` is the owner's descriptor pointer (`Cell::aux`), program-long.
    ret_of(e, unsafe { form_row(&*(a[0].v() as *const TupleDescData<'static>), masks, vals, image, &mut *region) })
}

pub static ROW_WIDE: Shim = Shim { oid: 0, name: "row form wide", family: Family::None, nargs: WIDE_HEAD as u8, coll: false, region: true, args: &[ArgKind::V, ArgKind::V], tyargs: &[], addr: Addr(no_addr), call: wide_shim };

/// D2789: `ROW_WIDE` for a whole row (`image_site`).
pub static ROW_WIDE_IMAGE: Shim = Shim { oid: 0, name: "row image wide", family: Family::None, nargs: WIDE_HEAD as u8, coll: false, region: true, args: &[ArgKind::V, ArgKind::V], tyargs: &[], addr: Addr(no_addr), call: wide_image_shim };

pub static MORE_SITE: Shim = Shim { oid: 0, name: "continuation words", family: Family::None, nargs: 0, coll: false, region: false, args: &[], tyargs: &[], addr: Addr(no_addr), call: more_shim };

fn wide_shim(e: *mut ErrSlot, region: *mut Region, _coll: u32, _ty: &[u32], a: &[Arg]) -> Ret {
    wide_call(e, region, a, false)
}

fn wide_image_shim(e: *mut ErrSlot, region: *mut Region, _coll: u32, _ty: &[u32], a: &[Arg]) -> Ret {
    wide_call(e, region, a, true)
}

fn wide_call(e: *mut ErrSlot, region: *mut Region, a: &[Arg], image: bool) -> Ret {
    let cell = a[0].v() as *const crate::emit::host::Cell;
    let n = a[1].v() as usize;
    // SAFETY: the emitted code passes its own cell of the program's live `EmitState`: its `aux` is the owner's descriptor
    // pointer (program-long, as `ROW_SITE`'s), and the `more_sites(n)` cells after it (the continuation sites' ids follow
    // the site's) hold the row's `wide_words(n)` words, stored before this call; the host passes the cell's live region.
    unsafe {
        let desc = &*((*cell).aux as *const TupleDescData<'static>);
        let word = |w: usize| -> i64 { continued_word(cell, w) };
        ret_of(e, form_wide(desc, n, &word, image, &mut *region))
    }
}

/// Never called (the emitted code only stores into a continuation site's cell): a call is the emitter's bug, an error.
fn more_shim(e: *mut ErrSlot, _region: *mut Region, _coll: u32, _ty: &[u32], _a: &[Arg]) -> Ret {
    ret_of(e, Err(Box::new(types_error::PgError::error("gravity: a continuation site was called".to_string())).into())) // alloc: Cold
}

/// `form_row` over a wide row's words: word `w / 64` of the mask says value `w` is NULL; value `i` is word `mask_words(n) + i`.
fn form_wide(desc: &TupleDescData<'_>, n: usize, word: &dyn Fn(usize) -> i64, image: bool, region: &mut Region) -> PgResult<NullableDatum> {
    let natts = desc.natts as usize;
    let masks = crate::nodes::func::mask_words(n);
    let (vals, nls) = crate::nodes::tuple::row_cells(region, natts)?;
    vals.fill(Datum::null());
    nls.fill(true);
    for i in 0..n.min(natts) {
        if (desc.attrs[i].attisdropped && !image) || (word(i / 64) as u64) >> (i % 64) & 1 != 0 {
            continue;
        }
        vals[i] = Datum::from_i64(word(masks + i));
        nls[i] = false;
    }
    crate::nodes::tuple::form(desc, vals, nls, region)
}

/// A composite site's return: the image, NULL, or its error into the cell's slot.
fn ret_of(e: *mut ErrSlot, r: PgResult<NullableDatum>) -> Ret {
    match r {
        Ok(v) if v.isnull => Ret { val: 0, status: NULL },
        Ok(v) => Ret { val: v.value.as_i64(), status: OK },
        Err(err) => {
            // SAFETY: the host passes the cell's live slot; a previous error in it is dropped through `take`.
            unsafe {
                drop((*e).take());
                (*e).err = gravity_abi::word(Box::into_raw(err));
            }
            Ret { val: 0, status: ERROR }
        }
    }
}

/// The emitted `ConvertRowtypeExpr`'s strict site (`CONVERT_SITE`): the program's descriptor table (`Cell::aux`,
/// `State::descs`), the word naming the out / in descriptors (`convert_word`), the composite's image, then the map's
/// attribute numbers as constant words (none for a header relabel).
pub const CONVERT_HEAD: usize = 3;

pub fn convert_site(b: &mut Builder<'_, '_>, nmap: usize) -> SiteId {
    let convs = b.prog.convs.len() as u32;
    b.prog.convs.push_in(ArgConv { kind: ArgKind::V, typlen: 0 });
    b.prog.convs.push_in(ArgConv { kind: ArgKind::V, typlen: 0 });
    b.prog.convs.push_in(ArgConv { kind: ArgKind::I, typlen: -1 });
    for _ in 0..nmap {
        b.prog.convs.push_in(ArgConv { kind: ArgKind::V, typlen: 0 });
    }
    let nargs = (CONVERT_HEAD + nmap) as u8;
    b.site(Call { funcid: 0, args: Kids::NONE, nargs, strict: true, collation: 0, how: How::Shim { shim: &CONVERT_SITE, convs, byref: true }, rettype: 0, volatile: false })
}

/// The out descriptor's index, and the in descriptor's above it (`u32::MAX`: the rows convert by a header relabel).
pub fn convert_word(n: &ConvertRowtype) -> i64 {
    (n.outdesc as u64 | (n.map.map_or(u32::MAX, |_| n.indesc) as u64) << 32) as i64
}

pub static CONVERT_SITE: Shim = Shim { oid: 0, name: "row convert", family: Family::None, nargs: CONVERT_HEAD as u8, coll: false, region: true, args: &[ArgKind::V, ArgKind::V, ArgKind::I], tyargs: &[], addr: Addr(no_addr), call: convert_shim };

fn convert_shim(e: *mut ErrSlot, region: *mut Region, _coll: u32, _ty: &[u32], a: &[Arg]) -> Ret {
    let descs = a[0].v() as *const crate::nodes::tuple::Desc;
    let w = a[1].v() as u64;
    // SAFETY: `a[0]` is the program's descriptor table (`Cell::aux`: `State::descs`, filled before the emit and
    // program-long); the word's indices are the compiled node's.
    let entry = |ix: u32| unsafe { &*descs.add(ix as usize) };
    let desc = |ix: u32| entry(ix).desc.as_ref().expect("a compiled node's descriptor");
    let ind = ((w >> 32) as u32 != u32::MAX).then(|| desc((w >> 32) as u32));
    let map = |i: usize| a[CONVERT_HEAD + i].v() as usize;
    // SAFETY: the host passes the cell's live region.
    let region = unsafe { &mut *region };
    // D2803: either rowtype changed inside the statement: the current descriptors and their map (a relabel's in
    // descriptor is its out descriptor's `conv_in`)
    let conv_in = entry(w as u32).conv_in;
    let fresh = if conv_in == u32::MAX { Ok(None) } else { crate::nodes::tuple::refreshed(desc(conv_in), desc(w as u32)) };
    match fresh {
        Ok(None) => ret_of(e, crate::nodes::tuple::convert_image(a[2].r().ptr, ind, desc(w as u32), map, region)),
        Ok(Some(f)) => ret_of(e, crate::nodes::tuple::convert_fresh(a[2].r().ptr, &f, region)),
        Err(err) => ret_of(e, Err(err)),
    }
}

/// The interpreter's `eval_row_expr` over evaluated arguments: a dropped attribute (unless the row is a whole row's
/// `image`) or an argument past the list null.
fn form_row(desc: &TupleDescData<'_>, masks: &[Arg], args: &[Arg], image: bool, region: &mut Region) -> PgResult<NullableDatum> {
    let natts = desc.natts as usize;
    let (vals, nls) = crate::nodes::tuple::row_cells(region, natts)?;
    vals.fill(Datum::null());
    nls.fill(true);
    for (i, a) in args.iter().enumerate().take(natts) {
        if (desc.attrs[i].attisdropped && !image) || crate::nodes::func::masked_null(masks, i) {
            continue;
        }
        vals[i] = Datum::from_i64(a.v());
        nls[i] = false;
    }
    crate::nodes::tuple::form(desc, vals, nls, region)
}

#[derive(Clone, Copy, Debug)]
pub struct ConvertRowtype {
    pub arg: NodeId,
    pub result_type: Oid,
    pub indesc: u32,
    pub outdesc: u32,
    pub map: Option<u32>,
    /// The emitted conversion's site (`CONVERT_SITE`).
    pub site: SiteId,
    /// The map is wider than a site's arguments: the emit's refusal (the interpreter converts it).
    pub wide: bool,
}

pub fn compile_row_expr<'mcx>(b: &mut Builder<'_, 'mcx>, r: &RowExprTree<'mcx>) -> CompileResult<NodeId> {
    crate::nodes::tuple::compile_row_expr(b, r)
}

pub fn compile_convert_rowtype<'mcx>(b: &mut Builder<'_, 'mcx>, c: &ConvertRowtypeExpr<'mcx>) -> CompileResult<NodeId> {
    crate::nodes::tuple::compile_convert_rowtype(b, c)
}

pub fn eval_row_expr(p: &Prog<'_, '_>, st: &mut St<'_>, n: &RowExpr, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    crate::nodes::tuple::eval_row_expr(p, st, n, cx)
}

pub fn eval_convert_rowtype(p: &Prog<'_, '_>, st: &mut St<'_>, n: &ConvertRowtype, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    crate::nodes::tuple::eval_convert_rowtype(p, st, n, cx)
}
