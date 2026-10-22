use ::datum::expandeddatum::{
    datum_get_eohp, datum_is_external_expanded, datum_is_external_expanded_rw, eoh_flatten_into,
    eoh_get_flat_size, eoh_init_header, eohp_get_rw_datum, ExpandedObjectHeader,
    ExpandedObjectMethods,
};
use ::datum::Datum;
use ::mcx::{slice_in, vec_with_capacity_in, Mcx, MemoryContext, PgVec};
use ::types_core::Oid;
use ::types_error::{PgError, PgResult, ERRCODE_PROGRAM_LIMIT_EXCEEDED};

use crate::construct::{copy_array_els, deconstruct_array};
use crate::foundation::{
    arr_data_offset, arr_elemtype, arr_hasnull, arr_overhead_nonulls, arr_overhead_withnulls,
    arr_size, att_align_nominal, read_dims_lbounds, varsize_any, MAXDIM, MAX_ALLOC_SIZE,
};

pub const EA_MAGIC: i32 = 689375833;

pub struct ArrayMetaState {
    pub element_type: Oid,
    pub typlen: i16,
    pub typbyval: bool,
    pub typalign: u8,
}

impl ArrayMetaState {
    pub const fn invalid() -> Self {
        ArrayMetaState { element_type: 0, typlen: 0, typbyval: false, typalign: 0 }
    }
}

// hdr must sit at offset 0: the expanded-datum protocol hands back
// *mut ExpandedObjectHeader and EA methods cast it to the full struct.
// ctx is declared last so the PgVecs (allocated in ctx) drop first.
#[repr(C)]
pub struct ExpandedArrayHeader {
    pub hdr: ExpandedObjectHeader,
    pub ea_magic: i32,
    pub ndims: i32,
    pub dims: [i32; MAXDIM],
    pub lbound: [i32; MAXDIM],
    pub element_type: Oid,
    pub typlen: i16,
    pub typbyval: bool,
    pub typalign: u8,
    pub dvalueslen: i32,
    pub nelems: i32,
    pub flat_size: usize,
    dvalues: Option<PgVec<'static, Datum>>,
    dnulls: Option<PgVec<'static, bool>>,
    fvalue: Option<PgVec<'static, u8>>,
    // C fstartptr..fendptr: by-ref dvalues may point into it.
    retired: Option<PgVec<'static, u8>>,
    flat_cache: Option<PgVec<'static, u8>>,
    ctx: MemoryContext,
}

const _: () = assert!(core::mem::offset_of!(ExpandedArrayHeader, hdr) == 0);

static EA_METHODS: ExpandedObjectMethods = ExpandedObjectMethods {
    get_flat_size: ea_get_flat_size,
    flatten_into: ea_flatten_into,
};

impl ExpandedArrayHeader {
    fn empty(ctx: MemoryContext) -> Self {
        ExpandedArrayHeader {
            hdr: ExpandedObjectHeader::empty(),
            ea_magic: EA_MAGIC,
            ndims: 0,
            dims: [0; MAXDIM],
            lbound: [0; MAXDIM],
            element_type: 0,
            typlen: 0,
            typbyval: false,
            typalign: 0,
            dvalueslen: 0,
            nelems: 0,
            flat_size: 0,
            dvalues: None,
            dnulls: None,
            fvalue: None,
            retired: None,
            flat_cache: None,
            ctx,
        }
    }

    // SAFETY(lifetime launder): allocations handed back into self's vec
    // fields die before ctx (field order above); nothing else escapes.
    fn obj_mcx(&self) -> Mcx<'static> {
        unsafe { core::mem::transmute::<Mcx<'_>, Mcx<'static>>(self.ctx.mcx()) }
    }

    pub fn fvalue(&self) -> Option<&[u8]> {
        self.fvalue.as_deref()
    }

    pub fn dvalues(&self) -> Option<(&[Datum], Option<&[bool]>)> {
        self.dvalues.as_ref().map(|v| (v.as_slice(), self.dnulls.as_deref()))
    }

    /// C array_append over an expanded array (array_set_element_expanded at subscript lbound + dims, 1-D or empty):
    /// the element is appended in place; a by-ref element is copied into the object's context first. `value` of a
    /// varlena element type is already detoasted. Errors as C: "integer out of range" when the subscript overflows,
    /// "array size exceeds the maximum allowed" past MaxArraySize. pgrust main's array_set_element_expanded does the
    /// work (CAND-M10's own append body was folded into it by the main merge).
    pub fn append_element(&mut self, value: Datum, isnull: bool) -> PgResult<()> {
        debug_assert!(self.ndims <= 1);
        let indx = if self.ndims == 0 {
            1
        } else {
            self.lbound[0].checked_add(self.dims[0]).ok_or_else(|| {
                Box::new(
                    PgError::error("integer out of range")
                        .with_sqlstate(::types_error::ERRCODE_NUMERIC_VALUE_OUT_OF_RANGE),
                )
            })?
        };
        array_set_element_expanded(self, &[indx], value, isnull)
    }
}

pub fn expand_array(
    arraydatum: Datum,
    parentcontext: &MemoryContext,
    metacache: Option<&mut ArrayMetaState>,
) -> PgResult<Datum> {
    let eah = expand_array_boxed(arraydatum, parentcontext, metacache)?;
    Ok(install(eah, parentcontext))
}

fn expand_array_boxed(
    arraydatum: Datum,
    parentcontext: &MemoryContext,
    mut metacache: Option<&mut ArrayMetaState>,
) -> PgResult<Box<ExpandedArrayHeader>> {
    let ctx = parentcontext.new_child("expanded array");
    let mut eah = Box::new(ExpandedArrayHeader::empty(ctx));

    // SAFETY: arraydatum is by-ref; expanded images embed a live header.
    let src_expanded = unsafe { datum_is_external_expanded(arraydatum) };
    if src_expanded {
        let old = unsafe { &*(datum_get_eohp(arraydatum) as *const ExpandedArrayHeader) };
        assert_eq!(old.ea_magic, EA_MAGIC);
        let mut fakecache = ArrayMetaState::invalid();
        let cache: &mut ArrayMetaState = match metacache.as_deref_mut() {
            Some(m) => m,
            None => &mut fakecache,
        };
        cache.element_type = old.element_type;
        cache.typlen = old.typlen;
        cache.typbyval = old.typbyval;
        cache.typalign = old.typalign;

        if old.typbyval && old.dvalues.is_some() {
            copy_byval_expanded_array(&mut eah, old)?;
            return Ok(eah);
        }
        let flat = flatten_source(&mut eah, arraydatum)?;
        fill_from_flat(&mut eah, flat, Some(cache))?;
        return Ok(eah);
    }

    let flat = copy_flat_source(&mut eah, arraydatum)?;
    fill_from_flat(&mut eah, flat, metacache.as_deref_mut())?;
    Ok(eah)
}

fn install(eah: Box<ExpandedArrayHeader>, parentcontext: &MemoryContext) -> Datum {
    let p = Box::into_raw(eah);
    // SAFETY: p is the header's final address (Box); both TOAST images embed
    // it. The parent reset callback is the sole owner-release (C: objcxt is a
    // child of parentcontext, freed by the parent's reset/delete).
    unsafe {
        eoh_init_header(&raw mut (*p).hdr, &EA_METHODS, &raw const (*p).ctx);
        // SAFETY: sole release of p; nothing dereferences it after the parent resets.
        parentcontext.register_reset_callback(move || drop(Box::from_raw(p)));
        eohp_get_rw_datum(&raw const (*p).hdr)
    }
}

/// An expanded array owned by its holder, not by a parent context's reset.
pub struct OwnedExpandedArray(core::ptr::NonNull<ExpandedArrayHeader>);

impl OwnedExpandedArray {
    pub fn rw_datum(&self) -> Datum {
        // SAFETY: the header is live and initialized for self's lifetime.
        unsafe { eohp_get_rw_datum(&raw const (*self.0.as_ptr()).hdr) }
    }

    pub fn ro_datum(&self) -> Datum {
        // SAFETY: the header is live and initialized for self's lifetime.
        unsafe { ::datum::expandeddatum::eohp_get_ro_datum(&raw const (*self.0.as_ptr()).hdr) }
    }

    pub fn is_rw_datum(&self, d: Datum) -> bool {
        d.as_usize() == self.rw_datum().as_usize()
    }

    pub fn flat_datum(&mut self) -> PgResult<Datum> {
        // SAFETY: exclusive access through &mut self.
        expanded_array_flat_datum(unsafe { self.0.as_mut() })
    }
}

use crate::{TRANSFERRED, TRANSFER_GEN};

fn transferred_take(addr: usize, generation: Option<u64>) -> bool {
    TRANSFERRED.with(|slots| {
        for s in slots {
            let (a, g) = s.get();
            if a == addr && generation.is_none_or(|want| want == g) {
                s.set((0, 0));
                return true;
            }
        }
        false
    })
}

fn transferred_list(addr: usize) -> Option<u64> {
    let generation = TRANSFER_GEN.with(|c| {
        let g = c.get() + 1;
        c.set(g);
        g
    });
    TRANSFERRED.with(|slots| {
        let free = slots.iter().find(|s| s.get().0 == 0)?;
        free.set((addr, generation));
        Some(generation)
    })
}

impl OwnedExpandedArray {
    /// Ownership moves to `ctx` (freed at its reset) until a holder takes it
    /// back with [`adopt_transferred`]; returns the read/write datum.
    pub fn transfer_to_context(self, ctx: &MemoryContext) -> Datum {
        let p = self.0.as_ptr();
        core::mem::forget(self);
        let addr = p as usize;
        let listed = transferred_list(addr);
        // SAFETY: p is the live Box allocation this value owned; the callback
        // is its sole release unless adopt_transferred delists it first.
        ctx.register_reset_callback(move || {
            if listed.is_none_or(|g| transferred_take(addr, Some(g))) {
                drop(unsafe { Box::from_raw(p) });
            }
        });
        // SAFETY: the header is live until the callback above runs.
        unsafe { eohp_get_rw_datum(&raw const (*p).hdr) }
    }
}

/// Takes back an array that transfer_to_context handed to a context; None
/// for any other datum.
pub fn adopt_transferred(d: Datum) -> Option<OwnedExpandedArray> {
    // SAFETY: a non-null by-ref varlena datum; the expanded check reads only
    // its two header bytes.
    if !unsafe { datum_is_external_expanded_rw(d) } {
        return None;
    }
    // SAFETY: a read/write expanded datum embeds its live header pointer.
    let p = unsafe { datum_get_eohp(d) } as *mut ExpandedArrayHeader;
    if !transferred_take(p as usize, None) {
        return None;
    }
    Some(OwnedExpandedArray(core::ptr::NonNull::new(p)?))
}

impl Drop for OwnedExpandedArray {
    fn drop(&mut self) {
        // SAFETY: allocated by Box in expand_array_owned; sole owner.
        drop(unsafe { Box::from_raw(self.0.as_ptr()) });
    }
}

pub fn expand_array_owned(
    arraydatum: Datum,
    parentcontext: &MemoryContext,
    metacache: Option<&mut ArrayMetaState>,
) -> PgResult<OwnedExpandedArray> {
    let p = Box::into_raw(expand_array_boxed(arraydatum, parentcontext, metacache)?);
    // SAFETY: p is the header's final address.
    unsafe { eoh_init_header(&raw mut (*p).hdr, &EA_METHODS, &raw const (*p).ctx) };
    Ok(OwnedExpandedArray(core::ptr::NonNull::new(p).expect("Box pointer")))
}

/// The flat image for read-only consumers, cached until the next change.
pub fn expanded_array_flat_datum(eah: &mut ExpandedArrayHeader) -> PgResult<Datum> {
    if let Some(f) = &eah.fvalue {
        return Ok(Datum::from_usize(f.as_ptr() as usize));
    }
    if eah.flat_cache.is_none() {
        // SAFETY: eah is a live, initialized expanded array.
        let n = unsafe { ea_get_flat_size(&raw mut eah.hdr) };
        let mut buf: PgVec<'static, u8> = vec_with_capacity_in(eah.obj_mcx(), n)?;
        // SAFETY: buf has capacity n; flatten writes exactly n bytes.
        unsafe {
            ea_flatten_into(&raw mut eah.hdr, buf.as_mut_ptr(), n);
            buf.set_len(n);
        }
        eah.flat_cache = Some(buf);
    }
    Ok(Datum::from_usize(eah.flat_cache.as_ref().unwrap().as_ptr() as usize))
}

fn copy_byval_expanded_array(
    eah: &mut ExpandedArrayHeader,
    oldeah: &ExpandedArrayHeader,
) -> PgResult<()> {
    let mcx = eah.obj_mcx();
    eah.ndims = oldeah.ndims;
    eah.dims = oldeah.dims;
    eah.lbound = oldeah.lbound;
    eah.element_type = oldeah.element_type;
    eah.typlen = oldeah.typlen;
    eah.typbyval = oldeah.typbyval;
    eah.typalign = oldeah.typalign;
    eah.dvalues = Some(slice_in(mcx, oldeah.dvalues.as_ref().unwrap())?);
    eah.dnulls = match &oldeah.dnulls {
        Some(old) => Some(slice_in(mcx, old)?),
        None => None,
    };
    eah.dvalueslen = oldeah.dvalueslen;
    eah.nelems = oldeah.nelems;
    eah.flat_size = oldeah.flat_size;
    eah.fvalue = None;
    Ok(())
}

fn flatten_source(
    eah: &mut ExpandedArrayHeader,
    arraydatum: Datum,
) -> PgResult<PgVec<'static, u8>> {
    let mcx = eah.obj_mcx();
    // SAFETY: source is a live expanded object (caller checked the image tag).
    unsafe {
        let old = datum_get_eohp(arraydatum);
        let n = eoh_get_flat_size(old);
        let mut buf: PgVec<'static, u8> = vec_with_capacity_in(mcx, n)?;
        eoh_flatten_into(old, buf.as_mut_ptr(), n);
        buf.set_len(n);
        Ok(buf)
    }
}

fn copy_flat_source(
    eah: &mut ExpandedArrayHeader,
    arraydatum: Datum,
) -> PgResult<PgVec<'static, u8>> {
    let mcx = eah.obj_mcx();
    let p = arraydatum.as_usize() as *const u8;
    // SAFETY: by-ref array datum points at a varlena image.
    unsafe {
        if (*p & 0x03) != 0 {
            let image = core::slice::from_raw_parts(p, varsize_any(p));
            detoast_seams::detoast_attr::call(mcx, image)
        } else {
            let n = arr_size(core::slice::from_raw_parts(p, 4));
            slice_in(mcx, core::slice::from_raw_parts(p, n))
        }
    }
}

fn fill_from_flat(
    eah: &mut ExpandedArrayHeader,
    flat: PgVec<'static, u8>,
    metacache: Option<&mut ArrayMetaState>,
) -> PgResult<()> {
    let (ndims, dims, lbound) = read_dims_lbounds(&flat);
    eah.ndims = ndims;
    eah.dims = dims;
    eah.lbound = lbound;
    eah.element_type = arr_elemtype(&flat);
    match metacache {
        Some(m) if m.element_type == eah.element_type => {
            eah.typlen = m.typlen;
            eah.typbyval = m.typbyval;
            eah.typalign = m.typalign;
        }
        other => {
            let (typlen, typbyval, typalign) =
                ::lsyscache::get_typlenbyvalalign(eah.element_type)?;
            eah.typlen = typlen;
            eah.typbyval = typbyval;
            eah.typalign = typalign as u8;
            if let Some(m) = other {
                m.element_type = eah.element_type;
                m.typlen = eah.typlen;
                m.typbyval = eah.typbyval;
                m.typalign = eah.typalign;
            }
        }
    }
    eah.dvalues = None;
    eah.dnulls = None;
    eah.dvalueslen = 0;
    eah.nelems = 0;
    eah.flat_size = 0;
    eah.fvalue = Some(flat);
    Ok(())
}

fn att_addlength_datum(cur: usize, typlen: i16, typbyval: bool, value: Datum) -> usize {
    if typlen > 0 {
        cur + typlen as usize
    } else if typlen == -1 {
        debug_assert!(!typbyval);
        // The by-ref varlena datum points at a live image.
        cur + varsize_any(value.as_usize() as *const u8)
    } else {
        debug_assert_eq!(typlen, -2);
        // SAFETY: cstring datum points at a live NUL-terminated string.
        cur + unsafe {
            let mut p = value.as_usize() as *const u8;
            let mut n = 1usize;
            while *p != 0 {
                n += 1;
                p = p.add(1);
            }
            n
        }
    }
}

// array_expanded.c:268-272: ereport(ERROR, ERRCODE_PROGRAM_LIMIT_EXCEEDED,
// "array size exceeds the maximum allowed (%d)"). The ExpandedObjectMethods
// table is infallible (C longjmps out of it), so the error rides the tree's
// PgError-payload unwind channel, recovered by pg_error_from_panic at the
// statement boundary (adt_numeric's undersized_numeric_error is the
// precedent).
#[cold]
#[inline(never)]
fn array_size_exceeds_limit() -> ! {
    std::panic::panic_any(
        PgError::error(format!("array size exceeds the maximum allowed ({MAX_ALLOC_SIZE})"))
            .with_sqlstate(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
    )
}

unsafe fn ea_get_flat_size(eohptr: *mut ExpandedObjectHeader) -> usize {
    let eah = &mut *(eohptr as *mut ExpandedArrayHeader);
    assert_eq!(eah.ea_magic, EA_MAGIC);
    if let Some(f) = &eah.fvalue {
        return arr_size(f);
    }
    if eah.flat_size != 0 {
        return eah.flat_size;
    }
    let nelems = eah.nelems as usize;
    let dvalues = eah.dvalues.as_ref().expect("expanded array has neither fvalue nor dvalues");
    let dnulls = eah.dnulls.as_deref();
    let mut nbytes = 0usize;
    for i in 0..nelems {
        if dnulls.is_some_and(|n| n[i]) {
            continue;
        }
        nbytes = att_addlength_datum(nbytes, eah.typlen, eah.typbyval, dvalues[i]);
        nbytes = att_align_nominal(nbytes, eah.typalign);
        if nbytes > MAX_ALLOC_SIZE {
            array_size_exceeds_limit();
        }
    }
    nbytes += if dnulls.is_some() {
        arr_overhead_withnulls(eah.ndims, eah.nelems)
    } else {
        arr_overhead_nonulls(eah.ndims)
    };
    eah.flat_size = nbytes;
    nbytes
}

unsafe fn ea_flatten_into(
    eohptr: *mut ExpandedObjectHeader,
    result: *mut u8,
    allocated_size: usize,
) {
    let eah = &*(eohptr as *const ExpandedArrayHeader);
    assert_eq!(eah.ea_magic, EA_MAGIC);
    if let Some(f) = &eah.fvalue {
        assert_eq!(allocated_size, arr_size(f));
        core::ptr::copy_nonoverlapping(f.as_ptr(), result, allocated_size);
        return;
    }
    assert_eq!(allocated_size, eah.flat_size);
    let nelems = eah.nelems;
    let ndims = eah.ndims;
    let dvalues = eah.dvalues.as_ref().unwrap();
    let dnulls = eah.dnulls.as_deref();
    let dataoffset: i32 = if dnulls.is_some() {
        arr_overhead_withnulls(ndims, nelems) as i32
    } else {
        0
    };
    // Pad space must be zero-filled (C memsets the whole result).
    core::ptr::write_bytes(result, 0, allocated_size);
    let image = core::slice::from_raw_parts_mut(result, allocated_size);
    image[0..4].copy_from_slice(&::datum::varlena::set_varsize_4b(allocated_size));
    image[4..8].copy_from_slice(&ndims.to_ne_bytes());
    image[8..12].copy_from_slice(&dataoffset.to_ne_bytes());
    image[12..16].copy_from_slice(&eah.element_type.to_ne_bytes());
    let mut off = 16usize;
    for i in 0..ndims as usize {
        image[off..off + 4].copy_from_slice(&eah.dims[i].to_ne_bytes());
        off += 4;
    }
    for i in 0..ndims as usize {
        image[off..off + 4].copy_from_slice(&eah.lbound[i].to_ne_bytes());
        off += 4;
    }
    copy_array_els(
        image,
        dvalues,
        dnulls,
        nelems as usize,
        eah.typlen as i32,
        eah.typbyval,
        eah.typalign,
    );
}

/// C DatumGetExpandedArray; `parentcontext` stands in for C's
/// CurrentMemoryContext (no ambient context here).
///
/// # Safety
/// `d` is a live array datum; the returned pointer aliases the object the
/// caller's datum references and follows C's caution about corrupting it.
pub unsafe fn datum_get_expanded_array(
    d: Datum,
    parentcontext: &MemoryContext,
) -> PgResult<*mut ExpandedArrayHeader> {
    datum_get_expanded_array_x(d, parentcontext, None)
}

/// C DatumGetExpandedArrayX.
///
/// # Safety
/// Same contract as [`datum_get_expanded_array`].
pub unsafe fn datum_get_expanded_array_x(
    d: Datum,
    parentcontext: &MemoryContext,
    mut metacache: Option<&mut ArrayMetaState>,
) -> PgResult<*mut ExpandedArrayHeader> {
    if datum_is_external_expanded_rw(d) {
        let eah = datum_get_eohp(d) as *mut ExpandedArrayHeader;
        assert_eq!((*eah).ea_magic, EA_MAGIC);
        if let Some(m) = metacache.as_deref_mut() {
            m.element_type = (*eah).element_type;
            m.typlen = (*eah).typlen;
            m.typbyval = (*eah).typbyval;
            m.typalign = (*eah).typalign;
        }
        return Ok(eah);
    }
    let d = expand_array(d, parentcontext, metacache)?;
    Ok(datum_get_eohp(d) as *mut ExpandedArrayHeader)
}

pub fn deconstruct_expanded_array(eah: &mut ExpandedArrayHeader) -> PgResult<()> {
    if eah.dvalues.is_some() {
        return Ok(());
    }
    let mcx = eah.obj_mcx();
    let fvalue = eah.fvalue.as_ref().expect("expanded array has neither fvalue nor dvalues");
    // SAFETY(lifetime launder): dvalues' by-ref element datums point into
    // fvalue, which this object owns and never frees before them.
    let flat: &'static [u8] = unsafe { core::mem::transmute(fvalue.as_slice()) };
    let hasnull = arr_hasnull(flat);
    let (dvalues, dnulls) =
        deconstruct_array(mcx, flat, eah.typlen as i32, eah.typbyval, eah.typalign, true)?;
    let nelems = dvalues.len() as i32;
    eah.dvalues = Some(dvalues);
    eah.dnulls = if hasnull { Some(dnulls) } else { None };
    eah.dvalueslen = nelems;
    eah.nelems = nelems;
    Ok(())
}

pub fn expanded_array_data_bounds(eah: &ExpandedArrayHeader) -> Option<(usize, usize)> {
    eah.fvalue.as_ref().map(|f| (arr_data_offset(f), arr_size(f)))
}

fn elem_image_size(value: Datum, typlen: i16) -> usize {
    if typlen > 0 {
        typlen as usize
    } else if typlen == -1 {
        varsize_any(value.as_usize() as *const u8)
    } else {
        att_addlength_datum(0, typlen, false, value)
    }
}

fn elem_layout(size: usize) -> core::alloc::Layout {
    core::alloc::Layout::from_size_align(size, 8).expect("element layout")
}

/// C array_set_element_expanded (arrayfuncs.c:2501); `data_value` is
/// already detoasted.
pub fn array_set_element_expanded(
    eah: &mut ExpandedArrayHeader,
    indx: &[i32],
    data_value: Datum,
    isnull: bool,
) -> PgResult<()> {
    use crate::element::{array_size_exceeded, subscript_out_of_range, wrong_subscripts};
    let n_subscripts = indx.len();
    let mut ndim = eah.ndims;
    let mut dim = eah.dims;
    let mut lb = eah.lbound;
    let mut dimschanged = false;

    if n_subscripts == 0 || n_subscripts > MAXDIM {
        return Err(wrong_subscripts());
    }
    if ndim == 0 {
        ndim = n_subscripts as i32;
        for i in 0..n_subscripts {
            dim[i] = 0;
            lb[i] = indx[i];
        }
        dimschanged = true;
    } else if ndim as usize != n_subscripts {
        return Err(wrong_subscripts());
    }

    deconstruct_expanded_array(eah)?;

    let mcx = eah.obj_mcx();
    // Copy the new element first: it may point into this very array.
    let data_value = if !eah.typbyval && !isnull {
        let size = elem_image_size(data_value, eah.typlen);
        let dst: core::ptr::NonNull<u8> = ::mcx::Allocator::allocate(&mcx, elem_layout(size))
            .map_err(|_| mcx.oom(size))?
            .cast();
        // SAFETY: fresh `size`-byte allocation; the source is readable for size bytes.
        unsafe { core::ptr::copy_nonoverlapping(data_value.as_usize() as *const u8, dst.as_ptr(), size) };
        Datum::from_usize(dst.as_ptr() as usize)
    } else {
        data_value
    };

    let mut newhasnulls = eah.dnulls.is_some() || isnull;
    let mut addedbefore = 0i32;
    let mut addedafter = 0i32;

    if ndim == 1 {
        if indx[0] < lb[0] {
            let (ab, o1) = lb[0].overflowing_sub(indx[0]);
            let (nd, o2) = dim[0].overflowing_add(ab);
            if o1 || o2 {
                return Err(array_size_exceeded());
            }
            addedbefore = ab;
            dim[0] = nd;
            lb[0] = indx[0];
            dimschanged = true;
            if addedbefore > 1 {
                newhasnulls = true;
            }
        }
        if indx[0] >= dim[0] + lb[0] {
            let (aa, o1) = indx[0].overflowing_sub(dim[0] + lb[0]);
            let (aa, o2) = aa.overflowing_add(1);
            let (nd, o3) = dim[0].overflowing_add(aa);
            if o1 || o2 || o3 {
                return Err(array_size_exceeded());
            }
            addedafter = aa;
            dim[0] = nd;
            dimschanged = true;
            if addedafter > 1 {
                newhasnulls = true;
            }
        }
    } else {
        for i in 0..ndim as usize {
            if indx[i] < lb[i] || indx[i] >= dim[i] + lb[i] {
                return Err(subscript_out_of_range());
            }
        }
    }

    if dimschanged {
        ::arrayutils::array_get_n_items(ndim, &dim)?;
        ::arrayutils::array_check_bounds(ndim, &dim, &lb)?;
    }
    let offset = ::arrayutils::array_get_offset(n_subscripts as i32, &dim, &lb, indx) as usize;

    let nelems = eah.nelems as usize;
    let newnelems = nelems + addedbefore as usize + addedafter as usize;
    let target = newnelems.saturating_add(newnelems / 8);
    let dvalues = eah.dvalues.as_mut().expect("deconstructed");
    if newnelems > dvalues.capacity() {
        dvalues.try_reserve_exact(target - dvalues.len()).map_err(|_| mcx.oom(target * 8))?;
    }
    if let Some(n) = eah.dnulls.as_mut().filter(|n| newnelems > n.capacity()) {
        n.try_reserve_exact(target - n.len()).map_err(|_| mcx.oom(target))?;
    }
    if newhasnulls && eah.dnulls.is_none() {
        let cap = eah.dvalues.as_ref().unwrap().capacity().max(newnelems);
        let mut n: PgVec<'static, bool> = vec_with_capacity_in(mcx, cap)?;
        n.resize(nelems, false);
        eah.dnulls = Some(n);
    }

    // Irreversible changes from here on.
    if let Some(f) = eah.fvalue.take() {
        eah.retired = Some(f);
    }
    eah.flat_cache = None;
    eah.flat_size = 0;
    if dimschanged {
        eah.ndims = ndim;
        eah.dims = dim;
        eah.lbound = lb;
    }
    let dvalues = eah.dvalues.as_mut().unwrap();
    // Growth below stays within the capacity reserved above.
    if addedbefore > 0 {
        let ab = addedbefore as usize;
        dvalues.extend(core::iter::repeat_n(Datum::null(), ab));
        dvalues.copy_within(0..nelems, ab);
        dvalues[..ab].fill(Datum::null());
        if let Some(n) = eah.dnulls.as_mut() {
            n.extend(core::iter::repeat_n(true, ab));
            n.copy_within(0..nelems, ab);
            n[..ab].fill(true);
        }
    }
    if addedafter > 0 {
        dvalues.extend(core::iter::repeat_n(Datum::null(), addedafter as usize));
        if let Some(n) = eah.dnulls.as_mut() {
            n.extend(core::iter::repeat_n(true, addedafter as usize));
        }
    }
    eah.nelems = newnelems as i32;
    eah.dvalueslen = newnelems as i32;

    let old_null = eah.dnulls.as_ref().is_some_and(|n| n[offset]);
    let old = eah.dvalues.as_ref().unwrap()[offset];
    eah.dvalues.as_mut().unwrap()[offset] = data_value;
    if let Some(n) = eah.dnulls.as_mut() {
        n[offset] = isnull;
    }

    if !eah.typbyval && !old_null && old.as_usize() != 0 {
        let p = old.as_usize();
        let in_flat = eah
            .retired
            .as_ref()
            .is_some_and(|f| p >= f.as_ptr() as usize && p < f.as_ptr() as usize + f.len());
        if !in_flat {
            let size = elem_image_size(old, eah.typlen);
            // SAFETY: allocated above by an earlier call with this layout;
            // no longer referenced by the array.
            unsafe {
                ::mcx::Allocator::deallocate(
                    &mcx,
                    core::ptr::NonNull::new_unchecked(p as *mut u8),
                    elem_layout(size),
                )
            };
        }
    }
    Ok(())
}

/// C array_get_element_expanded (arrayfuncs.c:1921).
pub fn array_get_element_expanded(
    eah: &mut ExpandedArrayHeader,
    indx: &[i32],
) -> PgResult<(Datum, bool)> {
    let ndim = eah.ndims;
    if ndim as usize != indx.len() || ndim <= 0 || ndim as usize > MAXDIM {
        return Ok((Datum::null(), true));
    }
    for i in 0..ndim as usize {
        if indx[i] < eah.lbound[i] || indx[i] >= eah.dims[i] + eah.lbound[i] {
            return Ok((Datum::null(), true));
        }
    }
    let offset = ::arrayutils::array_get_offset(ndim, &eah.dims, &eah.lbound, indx) as usize;
    deconstruct_expanded_array(eah)?;
    if eah.dnulls.as_ref().is_some_and(|n| n[offset]) {
        return Ok((Datum::null(), true));
    }
    Ok((eah.dvalues.as_ref().unwrap()[offset], false))
}

/// C VARATT_IS_EXTERNAL_EXPANDED + DatumGetEOHP.
///
/// # Safety
/// As [`rw_expanded_array`].
pub unsafe fn expanded_array(d: Datum) -> Option<*mut ExpandedArrayHeader> {
    if !datum_is_external_expanded(d) {
        return None;
    }
    let eah = datum_get_eohp(d) as *mut ExpandedArrayHeader;
    assert_eq!((*eah).ea_magic, EA_MAGIC);
    Some(eah)
}

/// C VARATT_IS_EXTERNAL_EXPANDED_RW + DatumGetEOHP.
///
/// # Safety
/// `d` is a live non-null varlena datum; the result aliases the object the
/// datum references.
pub unsafe fn rw_expanded_array(d: Datum) -> Option<*mut ExpandedArrayHeader> {
    if !datum_is_external_expanded_rw(d) {
        return None;
    }
    let eah = datum_get_eohp(d) as *mut ExpandedArrayHeader;
    assert_eq!((*eah).ea_magic, EA_MAGIC);
    Some(eah)
}

#[cfg(test)]
mod flat_size_limit_tests {
    use super::*;

    // Two varlena headers that CLAIM ~576 MiB each (no body: EA_get_flat_size
    // only reads the length words, array_expanded.c:266 att_addlength_datum),
    // so the summed flat size crosses MaxAllocSize on the second element.
    fn claimed_varlena(len: usize) -> Datum {
        let hdr: &'static mut [u8; 4] = Box::leak(Box::new(((len as u32) << 2).to_le_bytes()));
        Datum::from_usize(hdr.as_ptr() as usize)
    }

    fn dvalues_only_text_array(claims: &[usize]) -> Box<ExpandedArrayHeader> {
        let mut eah = Box::new(ExpandedArrayHeader::empty(MemoryContext::new("flat size limit")));
        eah.ndims = 1;
        eah.dims[0] = claims.len() as i32;
        eah.lbound[0] = 1;
        eah.element_type = 25;
        eah.typlen = -1;
        eah.typbyval = false;
        eah.typalign = b'i';
        eah.nelems = claims.len() as i32;
        eah.dvalueslen = claims.len() as i32;
        let mut dvalues: PgVec<'static, Datum> =
            vec_with_capacity_in(eah.obj_mcx(), claims.len()).unwrap();
        for &c in claims {
            dvalues.push(claimed_varlena(c));
        }
        eah.dvalues = Some(dvalues);
        eah
    }

    // array_expanded.c:268-272: a flattened payload past MaxAllocSize is
    // ereport(ERROR, ERRCODE_PROGRAM_LIMIT_EXCEEDED, "array size exceeds the
    // maximum allowed (%d)") -- a structured error the backend reports with
    // SQLSTATE 54000, never an unstructured panic
    // (row a186-candidate-fp-adt-array_expanded-207175d313430eabbd6a-1).
    #[test]
    fn flat_size_past_max_alloc_size_is_program_limit_exceeded_error() {
        let mut eah = dvalues_only_text_array(&[0x2400_0000, 0x2400_0000]);
        let p = core::ptr::from_mut(&mut *eah).cast::<ExpandedObjectHeader>();
        // unwind-ok: test witness
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            ea_get_flat_size(p)
        }));
        let payload = r.expect_err("flat size past MaxAllocSize must not succeed");
        let err = ::types_error::pg_error_from_panic(payload)
            .unwrap_or_else(|_| panic!("expected a structured PgError, got an unstructured panic"));
        assert_eq!(err.sqlstate(), ::types_error::ERRCODE_PROGRAM_LIMIT_EXCEEDED);
        assert_eq!(
            err.message(),
            format!("array size exceeds the maximum allowed ({MAX_ALLOC_SIZE})")
        );
        assert_eq!(eah.flat_size, 0, "no flat size is cached after the error");
    }

    #[test]
    fn flat_size_under_max_alloc_size_is_computed_and_cached() {
        let mut eah = dvalues_only_text_array(&[0x2400_0000, 0x1000]);
        let p = core::ptr::from_mut(&mut *eah).cast::<ExpandedObjectHeader>();
        let n = unsafe { ea_get_flat_size(p) };
        // both elements int-aligned, no nulls bitmap, 1-D overhead
        assert_eq!(n, 0x2400_0000 + 0x1000 + arr_overhead_nonulls(1));
        assert_eq!(eah.flat_size, n);
    }
}
