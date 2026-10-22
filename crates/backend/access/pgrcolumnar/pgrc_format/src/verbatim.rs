//! The reference `VERBATIM` + `CONST` codec (spec §19.1): the ABI WITNESS.
//!
//! This codec exists to prove the six-face ABI round-trips over every
//! storage class — correctness, not speed. It may read class/width from ctx
//! (a match in a cold body); M3-C's hot kernels monomorphize per width
//! behind the fn-pointer tables (the S4 pow2-switch law governs THEM, not
//! this witness). Wire behavior here is FROZEN: spec §6.7 (verbatim),
//! §6.8 (overflow), §6.9 (const).

use crate::abi::{
    narrow_each, refuse_decode_codes, refuse_dict_handle, validity_from_ctx, ByteArena, CodecVtable,
    DecodeOut, EncodeInput, Face, GranuleEncoder, KernelCtx, KernelKey, NarrowInt, NarrowOut,
    Selection, ValidityVerdict,
};
use crate::class::{
    StorageClass, CLASS_BOOL, CLASS_BYVAL, CLASS_F32, CLASS_F64, CLASS_FIXED, CLASS_VARLENA,
};
use crate::enc::EncodingId;
use crate::geom::{FRAMES_PER_GRANULE, FRAME_VALUES, GRANULE_ROWS, OVERSIZE_THRESHOLD};
use crate::meta::{ProbeAnswer, MetaProbe, Sortedness};
use crate::part::{
    OverflowSink, StreamSectionHdr, StreamSectionWriter, OVERFLOW_MARK, STREAMF_SIGNED,
    STREAM_SECTION_HDR_LEN,
};
use crate::wire::{varlena_entry_at, varlena_header_4b_u};
use crate::{FormatError, FormatResult};

// ---------------------------------------------------------------------------
// shared section plumbing (allocation-free)
// ---------------------------------------------------------------------------

/// The payload region of a section (past the header, before the tables).
fn payload_region<'a>(hdr: &StreamSectionHdr, section: &'a [u8]) -> FormatResult<&'a [u8]> {
    let end = if hdr.frame_table_off != 0 {
        hdr.frame_table_off as usize
    } else if hdr.gcount_table_off != 0 {
        hdr.gcount_table_off as usize
    } else {
        section.len()
    };
    if end < STREAM_SECTION_HDR_LEN || end > section.len() {
        return Err(FormatError::Bounds {
            at: "payload region",
        });
    }
    Ok(&section[STREAM_SECTION_HDR_LEN..end])
}

/// Per-granule value count `i` from the gcount table (child streams).
fn gcount(hdr: &StreamSectionHdr, section: &[u8], i: u32) -> FormatResult<u32> {
    let off = hdr.gcount_table_off as usize + i as usize * 4;
    let b = section
        .get(off..off + 4)
        .ok_or(FormatError::Bounds { at: "gcount table" })?;
    Ok(u32::from_le_bytes(b.try_into().expect("len 4")))
}

/// Values before granule `g` in this extent (spec §6.5: closed-form for root
/// streams, gcount-table-driven for child streams).
fn value_base(hdr: &StreamSectionHdr, section: &[u8], g: u32) -> FormatResult<u64> {
    if hdr.gcount_table_off == 0 {
        return Ok(g as u64 * GRANULE_ROWS as u64);
    }
    let mut sum = 0u64;
    for i in 0..g {
        sum += gcount(hdr, section, i)? as u64;
    }
    Ok(sum)
}

/// Frames before granule `g` in this extent (granule-major frame numbering).
fn frame_base(hdr: &StreamSectionHdr, section: &[u8], g: u32) -> FormatResult<u32> {
    if hdr.gcount_table_off == 0 {
        return Ok(g * FRAMES_PER_GRANULE);
    }
    let mut sum = 0u32;
    for i in 0..g {
        sum += gcount(hdr, section, i)?.div_ceil(FRAME_VALUES);
    }
    Ok(sum)
}

/// Word-class datum extension (spec §6.7).
pub(crate) fn extend_word(raw: u64, width: u8, signed: bool) -> u64 {
    if !signed || width == 8 {
        return raw;
    }
    let shift = 64 - width as u32 * 8;
    (((raw << shift) as i64) >> shift) as u64
}

/// Read 1..=8 LE bytes, zero-extended.
fn read_le(bytes: &[u8]) -> u64 {
    let mut w = [0u8; 8];
    w[..bytes.len()].copy_from_slice(bytes);
    u64::from_le_bytes(w)
}

// ---------------------------------------------------------------------------
// VERBATIM decode (spec §6.7/§6.8)
// ---------------------------------------------------------------------------

/// Per-granule verbatim view: resolves any row to a datum word.
struct VerbatimView<'a> {
    class_id: u8,
    width: u8,
    signed: bool,
    fixed_len: usize,
    payload: &'a [u8],
    /// Values before this granule (element index base).
    vbase: u64,
    /// Frames before this granule (varlena frame numbering).
    fbase: u32,
    values: u32,
    frame_table: Option<&'a [u32]>,
    overflow: Option<&'a [u8]>,
}

impl<'a> VerbatimView<'a> {
    fn open(ctx: &KernelCtx<'a>) -> FormatResult<VerbatimView<'a>> {
        let hdr = StreamSectionHdr::decode(ctx.bytes)?;
        if hdr.encoding != EncodingId::Verbatim.as_u16() {
            return Err(FormatError::Corrupt {
                at: "verbatim section encoding",
            });
        }
        let payload = payload_region(&hdr, ctx.bytes)?;
        Ok(VerbatimView {
            class_id: ctx.key.class,
            width: ctx.key.width,
            signed: ctx.flags & STREAMF_SIGNED != 0,
            fixed_len: ctx.fixed_len as usize,
            payload,
            vbase: value_base(&hdr, ctx.bytes, ctx.granule_in_extent)?,
            fbase: frame_base(&hdr, ctx.bytes, ctx.granule_in_extent)?,
            values: ctx.values,
            frame_table: ctx.frame_table,
            overflow: ctx.overflow,
        })
    }

    /// The stride of word/fixed classes.
    fn stride(&self) -> FormatResult<usize> {
        Ok(match self.class_id {
            CLASS_BYVAL => self.width as usize,
            CLASS_F32 => 4,
            CLASS_F64 => 8,
            CLASS_BOOL => 1,
            CLASS_FIXED => self.fixed_len,
            _ => {
                return Err(FormatError::Corrupt {
                    at: "verbatim stride class",
                })
            }
        })
    }

    /// The payload of varlena row `r` (granule-local), in place: a frame
    /// entry or the overflow entry an OverflowRef names (spec §6.7/§6.8).
    /// Reads headers only; no payload byte is copied.
    fn varlena_payload_at(&self, r: u32) -> FormatResult<&'a [u8]> {
        if r >= self.values {
            return Err(FormatError::Bounds { at: "verbatim row" });
        }
        let ft = self.frame_table.ok_or(FormatError::Corrupt {
            at: "varlena needs frame table",
        })?;
        let fi = (self.fbase + r / FRAME_VALUES) as usize;
        let frame_off = *ft
            .get(fi)
            .ok_or(FormatError::Bounds { at: "frame index" })? as usize;
        let frame = self
            .payload
            .get(frame_off..)
            .ok_or(FormatError::Bounds { at: "frame offset" })?;
        let slot = (r % FRAME_VALUES) as usize;
        let so = slot * 4;
        let entry_off = u32::from_le_bytes(
            frame
                .get(so..so + 4)
                .ok_or(FormatError::Bounds { at: "slot table" })?
                .try_into()
                .expect("len 4"),
        ) as usize;
        let head = frame
            .get(entry_off..entry_off + 4)
            .ok_or(FormatError::Bounds {
                at: "varlena entry",
            })?;
        let first = u32::from_le_bytes(head.try_into().expect("len 4"));
        if first == OVERFLOW_MARK {
            // OverflowRef: { mark, total_len, ovf_off } (spec §6.7).
            let rec = frame
                .get(entry_off..entry_off + 16)
                .ok_or(FormatError::Bounds { at: "overflow ref" })?;
            let total_len = u32::from_le_bytes(rec[4..8].try_into().expect("len 4")) as usize;
            let ovf_off = u64::from_le_bytes(rec[8..16].try_into().expect("len 8")) as usize;
            let ovf = self.overflow.ok_or(FormatError::Corrupt {
                at: "overflow ref without stream",
            })?;
            let (_, payload) = varlena_entry_at(ovf, ovf_off, "overflow entry")?;
            if payload.len() != total_len {
                return Err(FormatError::Corrupt {
                    at: "overflow entry length",
                });
            }
            Ok(payload)
        } else {
            let (_, payload) = varlena_entry_at(frame, entry_off, "varlena entry")?;
            Ok(payload)
        }
    }

    /// Decode row `r` (granule-local) to a datum word.
    fn datum_at(&self, r: u32, arena: &mut ByteArena<'_>) -> FormatResult<u64> {
        if r >= self.values {
            return Err(FormatError::Bounds { at: "verbatim row" });
        }
        match self.class_id {
            CLASS_BYVAL | CLASS_F32 | CLASS_F64 | CLASS_BOOL => {
                let s = self.stride()?;
                let off = (self.vbase + r as u64) as usize * s;
                let raw = self.payload.get(off..off + s).ok_or(FormatError::Bounds {
                    at: "verbatim word payload",
                })?;
                let w = read_le(raw);
                Ok(match self.class_id {
                    CLASS_BOOL => (w != 0) as u64,
                    CLASS_BYVAL => extend_word(w, self.width, self.signed),
                    _ => w,
                })
            }
            CLASS_FIXED => {
                let s = self.fixed_len;
                let off = (self.vbase + r as u64) as usize * s;
                let img = self.payload.get(off..off + s).ok_or(FormatError::Bounds {
                    at: "verbatim fixed payload",
                })?;
                arena.alloc_fixed(img)
            }
            CLASS_VARLENA => arena.alloc_varlena(self.varlena_payload_at(r)?),
            other => Err(FormatError::UnknownStorageClass { class: other }),
        }
    }
}

fn vdec_full(ctx: &KernelCtx<'_>, out: &mut DecodeOut<'_>) -> FormatResult<u32> {
    let v = VerbatimView::open(ctx)?;
    if out.datums.len() < v.values as usize {
        return Err(FormatError::Bounds {
            at: "decode_full out",
        });
    }
    for r in 0..v.values {
        out.datums[r as usize] = v.datum_at(r, &mut out.arena)?;
    }
    Ok(v.values)
}

/// Length-only verbatim varlena decode (D3038): `out[i]` = payload octet
/// length of granule value slot `i` (`ctx.values` slots, as `decode_full`
/// writes; a NULL row's slot holds an empty entry), for every slot of the
/// granule; returns the slot count. Same
/// bounds and overflow checks as the full decode, but no payload is copied
/// or materialized. Refuses non-varlena classes and non-verbatim sections.
pub fn varlena_lengths(ctx: &KernelCtx<'_>, out: &mut [u32]) -> FormatResult<u32> {
    let v = VerbatimView::open(ctx)?;
    if v.class_id != CLASS_VARLENA {
        return Err(FormatError::Corrupt {
            at: "varlena_lengths class",
        });
    }
    if out.len() < v.values as usize {
        return Err(FormatError::Bounds {
            at: "varlena_lengths out",
        });
    }
    for r in 0..v.values {
        out[r as usize] = v.varlena_payload_at(r)?.len() as u32;
    }
    Ok(v.values)
}

/// The narrow face's reference body: the per-value witness narrowed.
fn vdec_narrow(ctx: &KernelCtx<'_>, out: NarrowOut<'_>) -> FormatResult<u32> {
    let v = VerbatimView::open(ctx)?;
    if v.class_id != CLASS_BYVAL {
        return Err(FormatError::FaceUnsupported { face: Face::DecodeNarrow, encoding: ctx.key.encoding });
    }
    if out.len() < v.values as usize {
        return Err(FormatError::Bounds { at: "decode_narrow out" });
    }
    let mut words = [0u64; 8];
    // SAFETY: an 8-aligned 64-byte scratch; byval values never touch the arena.
    let bytes = unsafe { core::slice::from_raw_parts_mut(words.as_mut_ptr() as *mut u8, 64) };
    let mut arena = ByteArena::new(bytes);
    fn fill<T: NarrowInt>(v: &VerbatimView<'_>, arena: &mut ByteArena<'_>, o: &mut [T]) -> FormatResult<u32> {
        for r in 0..v.values {
            o[r as usize] = T::of_word(v.datum_at(r, arena)?);
        }
        Ok(v.values)
    }
    match out {
        NarrowOut::I16(o) => fill(&v, &mut arena, o),
        NarrowOut::I32(o) => fill(&v, &mut arena, o),
    }
}

fn vdec_sel(
    ctx: &KernelCtx<'_>,
    sel: &Selection<'_>,
    out: &mut DecodeOut<'_>,
) -> FormatResult<u32> {
    let v = VerbatimView::open(ctx)?;
    if out.datums.len() < sel.rows.len() {
        return Err(FormatError::Bounds {
            at: "decode_sel out",
        });
    }
    for (i, &r) in sel.rows.iter().enumerate() {
        out.datums[i] = v.datum_at(r as u32, &mut out.arena)?;
    }
    Ok(sel.rows.len() as u32)
}

/// NonNullCount from the ctx validity slice without an output buffer.
fn nonnull_count(ctx: &KernelCtx<'_>) -> FormatResult<u32> {
    let Some(bits) = ctx.validity_bytes else {
        return Ok(ctx.rows);
    };
    let rows = ctx.rows as usize;
    let need = rows.div_ceil(8);
    if bits.len() < need {
        return Err(FormatError::Bounds {
            at: "validity bitmap",
        });
    }
    let mut n = 0u32;
    for (i, &b) in bits[..need].iter().enumerate() {
        let mut byte = b;
        if (i + 1) * 8 > rows {
            byte &= (1u8 << (rows - i * 8)) - 1;
        }
        n += byte.count_ones();
    }
    Ok(n)
}

fn vmeta(ctx: &KernelCtx<'_>, probe: &MetaProbe) -> FormatResult<ProbeAnswer> {
    Ok(match probe {
        MetaProbe::RowCount => ProbeAnswer::Count(ctx.rows as u64),
        MetaProbe::NonNullCount => ProbeAnswer::Count(nonnull_count(ctx)? as u64),
        _ => ProbeAnswer::Absent,
    })
}

fn vvalidity(ctx: &KernelCtx<'_>, out: &mut [u64]) -> FormatResult<ValidityVerdict> {
    validity_from_ctx(ctx, out)
}

// ---------------------------------------------------------------------------
// CONST decode (spec §6.9)
// ---------------------------------------------------------------------------

struct ConstRecord<'a> {
    all_null: bool,
    bytes: &'a [u8],
}

fn const_record<'a>(ctx: &KernelCtx<'a>) -> FormatResult<ConstRecord<'a>> {
    let hdr = StreamSectionHdr::decode(ctx.bytes)?;
    if hdr.encoding != EncodingId::Const.as_u16() {
        return Err(FormatError::Corrupt {
            at: "const section encoding",
        });
    }
    let payload = payload_region(&hdr, ctx.bytes)?;
    if payload.len() < 8 {
        return Err(FormatError::Truncated { at: "const record" });
    }
    let all_null = payload[0] == 1;
    if payload[0] > 1 {
        return Err(FormatError::Corrupt { at: "const flags" });
    }
    let len = u32::from_le_bytes(payload[4..8].try_into().expect("len 4")) as usize;
    let bytes = payload
        .get(8..8 + len)
        .ok_or(FormatError::Bounds { at: "const bytes" })?;
    if all_null && len != 0 {
        return Err(FormatError::Corrupt {
            at: "const all-null with bytes",
        });
    }
    Ok(ConstRecord { all_null, bytes })
}

fn const_datum(
    ctx: &KernelCtx<'_>,
    rec: &ConstRecord<'_>,
    arena: &mut ByteArena<'_>,
) -> FormatResult<u64> {
    if rec.all_null {
        return Ok(0);
    }
    match ctx.key.class {
        CLASS_BYVAL | CLASS_F32 | CLASS_F64 | CLASS_BOOL => {
            if rec.bytes.len() != 8 {
                return Err(FormatError::Corrupt {
                    at: "const word length",
                });
            }
            Ok(read_le(rec.bytes))
        }
        CLASS_FIXED => {
            if rec.bytes.len() != ctx.fixed_len as usize {
                return Err(FormatError::Corrupt {
                    at: "const fixed length",
                });
            }
            arena.alloc_fixed(rec.bytes)
        }
        CLASS_VARLENA => arena.alloc_varlena(rec.bytes),
        other => Err(FormatError::UnknownStorageClass { class: other }),
    }
}

fn cdec_full(ctx: &KernelCtx<'_>, out: &mut DecodeOut<'_>) -> FormatResult<u32> {
    let rec = const_record(ctx)?;
    if out.datums.len() < ctx.values as usize {
        return Err(FormatError::Bounds {
            at: "decode_full out",
        });
    }
    let d = const_datum(ctx, &rec, &mut out.arena)?;
    for r in 0..ctx.values as usize {
        out.datums[r] = d;
    }
    Ok(ctx.values)
}

fn cdec_narrow(ctx: &KernelCtx<'_>, out: NarrowOut<'_>) -> FormatResult<u32> {
    let rec = const_record(ctx)?;
    if ctx.key.class != CLASS_BYVAL {
        return Err(FormatError::FaceUnsupported { face: Face::DecodeNarrow, encoding: ctx.key.encoding });
    }
    if out.len() < ctx.values as usize {
        return Err(FormatError::Bounds { at: "decode_narrow out" });
    }
    let mut words = [0u64; 8];
    // SAFETY: an 8-aligned 64-byte scratch; a byval const never touches the arena.
    let bytes = unsafe { core::slice::from_raw_parts_mut(words.as_mut_ptr() as *mut u8, 64) };
    let d = const_datum(ctx, &rec, &mut ByteArena::new(bytes))?;
    fn fill<T: NarrowInt>(d: u64, n: usize, o: &mut [T]) -> FormatResult<u32> {
        o[..n].fill(T::of_word(d));
        Ok(n as u32)
    }
    let n = ctx.values as usize;
    narrow_each(out, |o| fill(d, n, o), |o| fill(d, n, o))
}

fn cdec_sel(
    ctx: &KernelCtx<'_>,
    sel: &Selection<'_>,
    out: &mut DecodeOut<'_>,
) -> FormatResult<u32> {
    let rec = const_record(ctx)?;
    if out.datums.len() < sel.rows.len() {
        return Err(FormatError::Bounds {
            at: "decode_sel out",
        });
    }
    let d = const_datum(ctx, &rec, &mut out.arena)?;
    for (i, &r) in sel.rows.iter().enumerate() {
        if r as u32 >= ctx.values {
            return Err(FormatError::Bounds {
                at: "decode_sel row",
            });
        }
        out.datums[i] = d;
    }
    Ok(sel.rows.len() as u32)
}

fn cmeta(ctx: &KernelCtx<'_>, probe: &MetaProbe) -> FormatResult<ProbeAnswer> {
    Ok(match probe {
        MetaProbe::RowCount => ProbeAnswer::Count(ctx.rows as u64),
        MetaProbe::NonNullCount => {
            let rec = const_record(ctx)?;
            if rec.all_null {
                ProbeAnswer::Count(0)
            } else {
                ProbeAnswer::Count(nonnull_count(ctx)? as u64)
            }
        }
        MetaProbe::Sortedness => ProbeAnswer::Sorted(Sortedness::Constant),
        _ => ProbeAnswer::Absent,
    })
}

// ---------------------------------------------------------------------------
// vtables (spec §19.5)
// ---------------------------------------------------------------------------

const fn verbatim_vt(class: u8, width: u8) -> CodecVtable {
    CodecVtable {
        key: KernelKey {
            encoding: EncodingId::Verbatim as u16,
            class,
            width,
        },
        decode_full: vdec_full,
        decode_sel: vdec_sel,
        decode_codes: refuse_decode_codes,
        dict_handle: refuse_dict_handle,
        meta_probe: vmeta,
        validity: vvalidity,
        decode_narrow: vdec_narrow,
    }
}

const fn const_vt(class: u8, width: u8) -> CodecVtable {
    CodecVtable {
        key: KernelKey {
            encoding: EncodingId::Const as u16,
            class,
            width,
        },
        decode_full: cdec_full,
        decode_sel: cdec_sel,
        decode_codes: refuse_decode_codes,
        dict_handle: refuse_dict_handle,
        meta_probe: cmeta,
        validity: vvalidity,
        decode_narrow: cdec_narrow,
    }
}

static VT_VERBATIM: [CodecVtable; 8] = [
    verbatim_vt(CLASS_BYVAL, 1),
    verbatim_vt(CLASS_BYVAL, 2),
    verbatim_vt(CLASS_BYVAL, 4),
    verbatim_vt(CLASS_BYVAL, 8),
    verbatim_vt(CLASS_F32, 4),
    verbatim_vt(CLASS_F64, 8),
    verbatim_vt(CLASS_BOOL, 1),
    verbatim_vt(CLASS_FIXED, 0),
];
static VT_VERBATIM_VARLENA: CodecVtable = verbatim_vt(CLASS_VARLENA, 0);

static VT_CONST: [CodecVtable; 8] = [
    const_vt(CLASS_BYVAL, 1),
    const_vt(CLASS_BYVAL, 2),
    const_vt(CLASS_BYVAL, 4),
    const_vt(CLASS_BYVAL, 8),
    const_vt(CLASS_F32, 4),
    const_vt(CLASS_F64, 8),
    const_vt(CLASS_BOOL, 1),
    const_vt(CLASS_FIXED, 0),
];
static VT_CONST_VARLENA: CodecVtable = const_vt(CLASS_VARLENA, 0);

/// The CONST half of the reference vtables. The production registry
/// (`pgrc_codec::dispatch`) seeds CONST from here but wires VERBATIM to
/// its gate-3 hot kernels (M3 exit §2.3 license); the full
/// [`reference_vtables`] set stays available as the ABI witness, the
/// reference read binding, and the hot kernels' differential oracle.
pub fn const_reference_vtables() -> [&'static CodecVtable; 9] {
    [
        &VT_CONST[0],
        &VT_CONST[1],
        &VT_CONST[2],
        &VT_CONST[3],
        &VT_CONST[4],
        &VT_CONST[5],
        &VT_CONST[6],
        &VT_CONST[7],
        &VT_CONST_VARLENA,
    ]
}

/// The VERBATIM half of the reference vtables (the naive per-value witness
/// bodies — the hot kernels' differential oracle).
pub fn verbatim_reference_vtables() -> [&'static CodecVtable; 9] {
    [
        &VT_VERBATIM[0],
        &VT_VERBATIM[1],
        &VT_VERBATIM[2],
        &VT_VERBATIM[3],
        &VT_VERBATIM[4],
        &VT_VERBATIM[5],
        &VT_VERBATIM[6],
        &VT_VERBATIM[7],
        &VT_VERBATIM_VARLENA,
    ]
}

/// The reference vtables (registry seed; M3-C appends its kernel lists).
pub fn reference_vtables() -> [&'static CodecVtable; 18] {
    [
        &VT_VERBATIM[0],
        &VT_VERBATIM[1],
        &VT_VERBATIM[2],
        &VT_VERBATIM[3],
        &VT_VERBATIM[4],
        &VT_VERBATIM[5],
        &VT_VERBATIM[6],
        &VT_VERBATIM[7],
        &VT_VERBATIM_VARLENA,
        &VT_CONST[0],
        &VT_CONST[1],
        &VT_CONST[2],
        &VT_CONST[3],
        &VT_CONST[4],
        &VT_CONST[5],
        &VT_CONST[6],
        &VT_CONST[7],
        &VT_CONST_VARLENA,
    ]
}

// ---------------------------------------------------------------------------
// VERBATIM encode (spec §6.7/§6.8)
// ---------------------------------------------------------------------------

/// # Safety
///
/// Pointer-class datums must obey the EncodeInput contract (spec §19.6):
/// Fixed datums point at `len` readable bytes; Varlena datums at valid 4B-U
/// images. This helper is the crate's single deref site for encode.
unsafe fn input_image<'a>(class: StorageClass, datum: u64) -> FormatResult<&'a [u8]> {
    match class {
        StorageClass::Fixed { len } => {
            // SAFETY: caller contract.
            Ok(unsafe { core::slice::from_raw_parts(datum as *const u8, len as usize) })
        }
        StorageClass::VarlenaVerbatim => {
            let p = datum as *const u8;
            // SAFETY: caller contract.
            let header = u32::from_le_bytes(unsafe {
                core::slice::from_raw_parts(p, 4).try_into().expect("len 4")
            });
            let len = crate::wire::varlena_4b_u_payload_len(header, "encode varlena")? as usize;
            // SAFETY: caller contract — image is len + 4 bytes.
            Ok(unsafe { core::slice::from_raw_parts(p.add(4), len) })
        }
        _ => Err(FormatError::EncodeContract {
            detail: "input_image on word class",
        }),
    }
}

/// The reference VERBATIM encoder: one struct per stream, class-directed.
pub struct VerbatimEncoder {
    pub class: StorageClass,
}

impl GranuleEncoder for VerbatimEncoder {
    fn key(&self) -> KernelKey {
        KernelKey {
            encoding: EncodingId::Verbatim.as_u16(),
            class: self.class.id(),
            width: self.class.width(),
        }
    }

    fn encode_granule(
        &mut self,
        input: &EncodeInput<'_>,
        w: &mut StreamSectionWriter<'_>,
        ovf: &mut OverflowSink<'_>,
    ) -> FormatResult<()> {
        if input.class != self.class {
            return Err(FormatError::EncodeContract {
                detail: "encoder/input class",
            });
        }
        if input.datums.len() < input.rows as usize {
            return Err(FormatError::EncodeContract {
                detail: "datums shorter than rows",
            });
        }
        if input.rows > GRANULE_ROWS {
            return Err(FormatError::EncodeContract {
                detail: "granule overflow",
            });
        }
        match self.class {
            StorageClass::ByvalWord { width, .. } => {
                encode_words(input, w, width as usize)?;
            }
            StorageClass::F32 => encode_words(input, w, 4)?,
            StorageClass::F64 => encode_words(input, w, 8)?,
            StorageClass::Bool => {
                let buf = w.payload();
                for r in 0..input.rows {
                    let b = if input.valid(r) {
                        (input.datums[r as usize] != 0) as u8
                    } else {
                        0
                    };
                    buf.push(b); // alloc: Exec charged w.owner()
                }
            }
            StorageClass::Fixed { len } => {
                for r in 0..input.rows {
                    if input.valid(r) {
                        // SAFETY: EncodeInput pointer-class contract.
                        let img = unsafe { input_image(self.class, input.datums[r as usize])? };
                        w.payload().extend_from_slice(img); // alloc: Exec charged w.owner()
                    } else {
                        // Canonical placeholder: zero bytes (spec §6.6).
                        let new_len = w.payload().len() + len as usize;
                        w.payload().resize(new_len, 0); // alloc: Exec charged w.owner()
                    }
                }
            }
            StorageClass::VarlenaVerbatim => {
                encode_varlena_granule(input, w, ovf)?;
            }
        }
        w.end_granule(input.rows);
        Ok(())
    }

    /// SEAL-SPEED-2 fold fusion: the VARLENA arm folds at frame-span grain
    /// from inside the emit walk — the observe pass's second image deref
    /// per value disappears (the fold bill's home: pointer chase + byte-
    /// grain hashing per string). Word/bool/fixed classes keep the classic
    /// pair: their emit is a bulk copy and the observe walk over in-cache
    /// words is the cheap half.
    fn encode_granule_observed(
        &mut self,
        input: &EncodeInput<'_>,
        w: &mut StreamSectionWriter<'_>,
        ovf: &mut OverflowSink<'_>,
        meta: &mut dyn crate::abi::ColumnMetaBuilder,
        granule: u32,
    ) -> FormatResult<()> {
        if !meta.row_observe_supported() || !matches!(self.class, StorageClass::VarlenaVerbatim)
        {
            self.encode_granule(input, w, ovf)?;
            meta.observe_granule(input, granule);
            return Ok(());
        }
        if input.class != self.class {
            return Err(FormatError::EncodeContract {
                detail: "encoder/input class",
            });
        }
        if input.datums.len() < input.rows as usize {
            return Err(FormatError::EncodeContract {
                detail: "datums shorter than rows",
            });
        }
        if input.rows > GRANULE_ROWS {
            return Err(FormatError::EncodeContract {
                detail: "granule overflow",
            });
        }
        meta.begin_granule_rows(granule);
        encode_varlena_granule_observed(input, w, ovf, Some(meta))?;
        meta.end_granule_rows(input.rows);
        w.end_granule(input.rows);
        Ok(())
    }
}

fn encode_words(
    input: &EncodeInput<'_>,
    w: &mut StreamSectionWriter<'_>,
    stride: usize,
) -> FormatResult<()> {
    let buf = w.payload();
    for r in 0..input.rows {
        // Canonical placeholder for nulls: zero bytes (spec §6.6).
        let d = if input.valid(r) {
            input.datums[r as usize]
        } else {
            0
        };
        buf.extend_from_slice(&d.to_le_bytes()[..stride]); // alloc: Exec charged w.owner()
    }
    Ok(())
}

fn encode_varlena_granule(
    input: &EncodeInput<'_>,
    w: &mut StreamSectionWriter<'_>,
    ovf: &mut OverflowSink<'_>,
) -> FormatResult<()> {
    encode_varlena_granule_observed(input, w, ovf, None)
}

/// [`encode_varlena_granule`] with the SEAL-SPEED-2 fold-fusion hook: the
/// SAME emit loop feeds the meta builder per row (`obs` = the streaming
/// face, already `begin_granule_rows`-opened by the caller) — the varlena
/// deref the folds need is the one the encoder just performed, so the
/// separate observe walk (its second pointer chase per value) disappears.
/// `None` = the classic emit (byte-identical either way).
fn encode_varlena_granule_observed(
    input: &EncodeInput<'_>,
    w: &mut StreamSectionWriter<'_>,
    ovf: &mut OverflowSink<'_>,
    mut obs: Option<&mut dyn crate::abi::ColumnMetaBuilder>,
) -> FormatResult<()> {
    let mut r0: u32 = 0;
    while r0 < input.rows {
        let vif = (input.rows - r0).min(FRAME_VALUES);
        // SEAL-SPEED-2 fold fusion, SPAN grain: fold this frame's rows
        // right before the emit walks the same images — one virtual call
        // per frame, the builder's tight monomorphic loop inside, and the
        // emit re-reads cache-warm payloads. Fold order stays row order.
        if let Some(o) = obs.as_deref_mut() {
            o.observe_rows(input, r0, vif);
        }
        // Frames start 8-aligned so entry alignment is absolute (spec §6.7).
        w.align_payload(8);
        w.begin_frame();
        // Slot table: (vif + 1) u32 entry offsets, frame-start-relative;
        // entries begin at the next 8-aligned offset past the table.
        let slot_table_len = (vif as usize + 1) * 4;
        let entries_base = slot_table_len.div_ceil(8) * 8;
        // Wave 3 Q6: the slot table and the entries go straight into the payload (no per-frame scratch, one copy less):
        // the table and its pad are laid down zeroed first, and each slot is written as its entry lands. The bytes are
        // the staged form's: the same table, pad and 8-aligned entries.
        let frame = w.payload().len();
        let ent0 = frame + entries_base;
        w.payload().resize(ent0, 0); // alloc: Exec charged w.owner()
        let slot_at = |buf: &mut Vec<u8>, i: usize| {
            let slot = (entries_base + (buf.len() - ent0)) as u32;
            buf[frame + i * 4..frame + i * 4 + 4].copy_from_slice(&slot.to_le_bytes());
        };
        for i in 0..vif {
            let r = r0 + i;
            let buf = w.payload();
            // 8-align each entry within the entry region (base is 8-aligned).
            let pad = (8 - (buf.len() - ent0) % 8) % 8;
            buf.resize(buf.len() + pad, 0); // alloc: Exec charged w.owner()
            slot_at(buf, i as usize);
            if !input.valid(r) {
                // Canonical placeholder: a zero-length varlena entry.
                buf.extend_from_slice(&varlena_header_4b_u(0).to_le_bytes()); // alloc: Exec charged w.owner()
                continue;
            }
            // SAFETY: EncodeInput pointer-class contract.
            let payload =
                unsafe { input_image(StorageClass::VarlenaVerbatim, input.datums[r as usize])? };
            if payload.len() as u32 >= OVERSIZE_THRESHOLD {
                let off = ovf.put_entry(payload);
                buf.extend_from_slice(&OVERFLOW_MARK.to_le_bytes()); // alloc: Exec charged w.owner()
                buf.extend_from_slice(&(payload.len() as u32).to_le_bytes()); // alloc: Exec charged w.owner()
                buf.extend_from_slice(&off.to_le_bytes()); // alloc: Exec charged w.owner()
            } else {
                buf.extend_from_slice(&varlena_header_4b_u(payload.len() as u32).to_le_bytes()); // alloc: Exec charged w.owner()
                buf.extend_from_slice(payload); // alloc: Exec charged w.owner()
            }
        }
        slot_at(w.payload(), vif as usize);
        r0 += vif;
    }
    Ok(())
}

/// Canonical validity-bitmap producer (spec §6.6): per-granule, LSB-first,
/// byte-padded, zeroed past `rows`. The writer emits validity streams
/// through this (BOOL_BITMAP layout).
pub fn encode_validity_bitmap(validity: Option<&[u64]>, rows: u32, out: &mut Vec<u8>) {
    let bytes = (rows as usize).div_ceil(8);
    for i in 0..bytes {
        let mut b: u8 = 0;
        for bit in 0..8 {
            let r = (i * 8 + bit) as u32;
            if r < rows {
                let valid = match validity {
                    None => true,
                    Some(words) => {
                        let w = (r / 64) as usize;
                        w < words.len() && (words[w] >> (r % 64)) & 1 == 1
                    }
                };
                if valid {
                    b |= 1 << bit;
                }
            }
        }
        out.push(b); // alloc: Exec charged caller
    }
}

// ---------------------------------------------------------------------------
// CONST encode (spec §6.9)
// ---------------------------------------------------------------------------

enum ConstState {
    /// Nothing seen yet (no record written).
    Empty,
    /// Only nulls so far; no record written yet (finish_stream writes
    /// ALL_NULL if it stays that way).
    AllNullSoFar,
    /// The extent's value record is written; canonical bytes retained for
    /// the constancy check.
    Value(Vec<u8>),
}

/// The reference CONST encoder: extent-scoped single value; the election is
/// the caller's — this encoder VERIFIES constancy (typed error otherwise).
pub struct ConstEncoder {
    pub class: StorageClass,
    state: ConstState,
    /// Word classes, once the value is stored: (canonical word, mask, Bool).
    word: Option<(u64, u64, bool)>,
}

/// The mask over the datum word that `datum_canonical_bytes` keeps for a
/// word class (its low `width` bytes; Bool keeps `datum != 0`, flagged),
/// or None for the by-reference classes.
fn word_mask(class: StorageClass) -> Option<(u64, bool)> {
    match class {
        StorageClass::ByvalWord { width, .. } => Some((
            if width >= 8 { u64::MAX } else { (1u64 << (8 * u32::from(width))) - 1 },
            false,
        )),
        StorageClass::F32 => Some((0xFFFF_FFFF, false)),
        StorageClass::F64 => Some((u64::MAX, false)),
        StorageClass::Bool => Some((1, true)),
        StorageClass::Fixed { .. } | StorageClass::VarlenaVerbatim => None,
    }
}

impl ConstEncoder {
    pub fn new(class: StorageClass) -> ConstEncoder {
        ConstEncoder {
            class,
            state: ConstState::Empty,
            word: None,
        }
    }


    /// The record payload for the stored value (word classes: the full
    /// 8-byte datum word; fixed/varlena: the image/payload bytes).
    fn record_bytes(&self, datum: u64) -> FormatResult<Vec<u8>> {
        match self.class {
            StorageClass::ByvalWord { .. }
            | StorageClass::F32
            | StorageClass::F64
            | StorageClass::Bool => Ok(datum.to_le_bytes().to_vec()),
            StorageClass::Fixed { .. } | StorageClass::VarlenaVerbatim => {
                // SAFETY: EncodeInput pointer-class contract.
                unsafe { input_image(self.class, datum).map(|s| s.to_vec()) }
            }
        }
    }

    fn write_record(w: &mut StreamSectionWriter<'_>, all_null: bool, bytes: &[u8]) {
        let buf = w.payload();
        buf.push(all_null as u8); // alloc: Exec charged w.owner()
        buf.extend_from_slice(&[0u8; 3]); // alloc: Exec charged w.owner()
        buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes()); // alloc: Exec charged w.owner()
        buf.extend_from_slice(bytes); // alloc: Exec charged w.owner()
    }
}

impl GranuleEncoder for ConstEncoder {
    fn key(&self) -> KernelKey {
        KernelKey {
            encoding: EncodingId::Const.as_u16(),
            class: self.class.id(),
            width: self.class.width(),
        }
    }

    fn encode_granule(
        &mut self,
        input: &EncodeInput<'_>,
        w: &mut StreamSectionWriter<'_>,
        _ovf: &mut OverflowSink<'_>,
    ) -> FormatResult<()> {
        if input.class != self.class {
            return Err(FormatError::EncodeContract {
                detail: "encoder/input class",
            });
        }
        let mut r = 0u32;
        while r < input.rows {
            // Word classes, once the value is stored: the canonical bytes
            // are the datum's low `width` bytes (Bool: `datum != 0`), so the
            // byte compare below is a masked word compare; same rows
            // checked, same first mismatch, same error.
            if let (ConstState::Value(_), Some((first, mask, is_bool))) = (&self.state, self.word) {
                for r in r..input.rows {
                    if !input.valid(r) {
                        continue;
                    }
                    let d = input.datums[r as usize];
                    let c = if is_bool { (d != 0) as u64 } else { d };
                    if (c ^ first) & mask != 0 {
                        return Err(FormatError::EncodeContract {
                            detail: "CONST not constant",
                        });
                    }
                }
                break;
            }
            let row = r;
            r += 1;
            if !input.valid(row) {
                continue;
            }
            let r = row;
            // The canonical bytes borrow a stack scratch (word classes) or
            // the input image (fixed/varlena): only the first value is
            // copied out, every later row compares in place (no per-row
            // allocation; the same comparison, the same bytes out).
            let mut scratch = [0u8; 8];
            // SAFETY: EncodeInput pointer-class contract (spec §19.6).
            let canon = unsafe {
                crate::abi::datum_canonical_bytes(self.class, input.datums[r as usize], &mut scratch)?
            };
            match &self.state {
                ConstState::Empty | ConstState::AllNullSoFar => {
                    let rec = self.record_bytes(input.datums[r as usize])?;
                    ConstEncoder::write_record(w, false, &rec);
                    self.state = ConstState::Value(canon.to_vec());
                    self.word = word_mask(self.class).map(|(mask, is_bool)| {
                        let d = input.datums[r as usize];
                        ((if is_bool { (d != 0) as u64 } else { d }) & mask, mask, is_bool)
                    });
                }
                ConstState::Value(stored) => {
                    if stored.as_slice() != canon {
                        return Err(FormatError::EncodeContract {
                            detail: "CONST not constant",
                        });
                    }
                }
            }
        }
        if matches!(self.state, ConstState::Empty) {
            self.state = ConstState::AllNullSoFar;
        }
        w.end_granule(input.rows);
        Ok(())
    }

    fn finish_stream(&mut self, w: &mut StreamSectionWriter<'_>) -> FormatResult<()> {
        if matches!(self.state, ConstState::Empty | ConstState::AllNullSoFar) {
            ConstEncoder::write_record(w, true, &[]);
        }
        Ok(())
    }
}
