//! Dictionary vocabulary (spec §7; charter §6): one framed, byte-rank-sorted
//! GLOBAL dictionary per dict column per part. Global codes ARE the stored
//! codes — the stitch pre-pass, remap layer, and per-worker dict rebuilds are
//! structurally deleted.
//!
//! - **Epoch identity** = (part identity §11, attno, path_ord). Codes are
//!   meaningless across epochs (Law A); epochs are part-scoped, so whole-band
//!   claims (spec §2) can never split one.
//! - **Sorted order is contractual**: global code order == byte order of the
//!   stored images; where an order embedding exists, code compare = value
//!   compare.
//! - **Payload entries are varlena-shaped, 8-aligned** (spec §1, StrView
//!   §7b): dict materialization can be zero-copy views into the payload
//!   region, which the reader keeps generation-stable (M3-F pin).
//! - **Publishability lattice**: the `STREAMF_DICT_EXEC` stream flag records
//!   whether code-eq == value-eq (execution lanes) or storage-dedup-only.
//!   The O-6 zero-null-proof refusal carries at the execution boundary
//!   (M3-G) until the chartered consumer null-audit lands.

use crate::geom::DICT_FRAME_ENTRIES;
use crate::wire::{put_u32, varlena_4b_u_payload_len, varlena_entry_at, varlena_header_4b_u, Cur};
use crate::{FormatError, FormatResult};

pub const DICT_INDEX_ENTRY_LEN: usize = 12;

/// The DictIndex `char_field` FORM, named by the stream dir entry's flags
/// (spec §6.3) — never guessed, never a process posture:
/// - `Absolute` (no flag): `char_field == char_len` — the blessed lineage.
/// - `Delta` (`STREAMF_CHARLEN_DELTA`): `char_field == byte_len − char_len`
///   — the S8 §2(a) arm, MEASURED DECLINE (M5d package §3).
/// - `Absent` (`STREAMF_CHARLEN_ABSENT`): nothing stored (zeros) — the
///   §2(b)/Option-C lengths-prepass arm; consumers recompute from entry
///   bytes. Probe-only (M5d package §9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DictCharLenForm {
    Absolute,
    Delta,
    Absent,
}

impl DictCharLenForm {
    /// Decode from stream dir-entry flags (both bits set is corruption).
    pub fn from_flags(flags: u16) -> FormatResult<DictCharLenForm> {
        use crate::part::{STREAMF_CHARLEN_ABSENT, STREAMF_CHARLEN_DELTA};
        match (flags & STREAMF_CHARLEN_DELTA != 0, flags & STREAMF_CHARLEN_ABSENT != 0) {
            (false, false) => Ok(DictCharLenForm::Absolute),
            (true, false) => Ok(DictCharLenForm::Delta),
            (false, true) => Ok(DictCharLenForm::Absent),
            (true, true) => Err(FormatError::Corrupt {
                at: "dict char_len form flags",
            }),
        }
    }

    pub fn flag_bits(self) -> u16 {
        use crate::part::{STREAMF_CHARLEN_ABSENT, STREAMF_CHARLEN_DELTA};
        match self {
            DictCharLenForm::Absolute => 0,
            DictCharLenForm::Delta => STREAMF_CHARLEN_DELTA,
            DictCharLenForm::Absent => STREAMF_CHARLEN_ABSENT,
        }
    }
}

/// UTF-8 lead-byte count — the char-length fact's one definition (the
/// write-side `char_len_of` Utf8Chars arm verbatim; ill-formed input
/// degrades to a byte count for the malformed tail).
#[inline]
pub fn utf8_char_count(bytes: &[u8]) -> u32 {
    bytes.iter().filter(|&&b| (b & 0xC0) != 0x80).count() as u32
}

/// One dictionary index entry (12 B, spec §7): `payload_off` is
/// DictPayload-payload-relative and points at the entry's varlena header;
/// stored byte- AND char-lengths make `length()` a table lookup.
///
/// `char_field` carries the char-length fact in one of the
/// [`DictCharLenForm`] FORMS, named by the owning DictIndex stream's
/// dir-entry flags (never guessed). Consumers never see the form:
/// [`dict_entry`] hands back absolute `char_len` under every form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DictIndexEntry {
    pub payload_off: u32,
    pub byte_len: u32,
    pub char_field: u32,
}

impl DictIndexEntry {
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        put_u32(out, self.payload_off);
        put_u32(out, self.byte_len);
        put_u32(out, self.char_field);
    }

    pub fn decode(c: &mut Cur<'_>) -> FormatResult<DictIndexEntry> {
        Ok(DictIndexEntry {
            payload_off: c.u32("DictIndexEntry")?,
            byte_len: c.u32("DictIndexEntry")?,
            char_field: c.u32("DictIndexEntry")?,
        })
    }
}

/// The dict frame an entry lives in (lazy-fault grain, spec §7).
pub const fn dict_frame_of_entry(code: u32) -> u32 {
    code / DICT_FRAME_ENTRIES
}

/// Publishability level achieved by a dict stream (spec §7). Recorded on
/// disk as the `STREAMF_DICT_EXEC` flag; this enum is the vocabulary the
/// writer's election and the scan's publication share.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DictPublish {
    /// code-eq == value-eq: group-on-codes, once-per-code eval are sound.
    ExecutionLane,
    /// Storage-level dedup only (bpchar, numeric, interval, float,
    /// jsonb-as-value, extension types).
    StorageDedup,
}

/// The dict sections a kernel ctx carries (spec §19.2): raw section payloads
/// (past their StreamSectionHdr), CRC-validated by the reader.
#[derive(Debug, Clone, Copy)]
pub struct DictSections<'a> {
    /// DictIndex stream payload: `DictIndexEntry` × entry_count.
    pub index: &'a [u8],
    /// DictPayload stream payload: varlena-shaped entries in byte-rank order.
    pub payload: &'a [u8],
    pub entry_count: u32,
    /// The DictIndex stream's `char_field` form fact (spec §6.3 flags).
    /// Carried here so [`dict_entry`] stays pure — the flags are read
    /// from the stream dir by whoever builds the ctx.
    pub charlen_form: DictCharLenForm,
    /// What `payload` holds: the v1 varlena image, or the resident
    /// front-coded form ([`FcResident`]; lane dictresident's knob).
    pub form: DictPayloadForm,
}

/// The resident form of a DictPayload section's payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DictPayloadForm {
    /// Varlena entries, 8-aligned, at the index's `payload_off`s (the v1 image).
    Expanded,
    /// The zstd-inflated FrontZstd frames kept front-coded ([`FcResident`]).
    FrontCoded,
}

/// One resolved dictionary entry.
#[derive(Debug, Clone, Copy)]
pub struct DictEntryRef<'a> {
    /// The whole varlena image (header + payload) — a valid PG datum image.
    pub image: &'a [u8],
    /// The payload bytes (header excluded).
    pub bytes: &'a [u8],
    pub byte_len: u32,
    pub char_len: u32,
}

/// Resolve entry `code` (pure, bounds-validated; the reader's lazy framed
/// handle builds on this).
pub fn dict_entry<'a>(d: &DictSections<'a>, code: u32) -> FormatResult<DictEntryRef<'a>> {
    if d.form != DictPayloadForm::Expanded {
        return Err(FormatError::Corrupt { at: "dict entry view over a front-coded payload" });
    }
    if code >= d.entry_count {
        return Err(FormatError::Bounds { at: "dict code" });
    }
    let off = code as usize * DICT_INDEX_ENTRY_LEN;
    if off + DICT_INDEX_ENTRY_LEN > d.index.len() {
        return Err(FormatError::Bounds { at: "dict index" });
    }
    let mut c = Cur::new(&d.index[off..off + DICT_INDEX_ENTRY_LEN]);
    let e = DictIndexEntry::decode(&mut c)?;
    let (image, bytes) = varlena_entry_at(d.payload, e.payload_off as usize, "dict payload")?;
    if bytes.len() != e.byte_len as usize {
        return Err(FormatError::Corrupt {
            at: "dict entry byte_len",
        });
    }
    let char_len = match d.charlen_form {
        DictCharLenForm::Absolute => e.char_field,
        DictCharLenForm::Delta => {
            e.byte_len
                .checked_sub(e.char_field)
                .ok_or(FormatError::Corrupt {
                    at: "dict entry char_len delta",
                })?
        }
        // Option-C: nothing stored — recompute from the entry bytes in
        // hand (one lead-count walk; the char-length fact's one
        // definition, `utf8_char_count`).
        DictCharLenForm::Absent => utf8_char_count(bytes),
    };
    Ok(DictEntryRef {
        image,
        bytes,
        byte_len: e.byte_len,
        char_len,
    })
}

/// Resolve entry `code` to its DATUM WORD — the pointer to the entry's
/// varlena image inside `d.payload` — with the same validation surface as
/// [`dict_entry`] (code bound, index bound, header/`byte_len` two-witness
/// agreement, payload bound; typed errors, never UB) but no slice
/// materialization: the hot gather face.
///
/// **Zero-copy aliasing law (StrView §7b / §4).** The returned datum
/// aliases the caller's `d.payload` slice; it is a valid plain 4B-U PG
/// datum for as long as those bytes stay resident. On the read path the
/// payload is a part-cached `SegBuf` (8-aligned, insert-only, alive for the
/// `OpenPart`'s life), so entry datums are 8-aligned and outlive every
/// granule-claim consumption window. Callers that build `DictSections` over
/// transient buffers (write-path verify, tests, harnesses) must keep those
/// buffers alive while the datums are read, and only get absolute 8-align
/// if their buffer base is 8-aligned.
#[inline]
pub fn dict_entry_datum(d: &DictSections<'_>, code: u32) -> FormatResult<u64> {
    if d.form != DictPayloadForm::Expanded {
        return Err(FormatError::Corrupt { at: "dict entry datum over a front-coded payload" });
    }
    if code >= d.entry_count {
        return Err(FormatError::Bounds { at: "dict code" });
    }
    let off = code as usize * DICT_INDEX_ENTRY_LEN;
    let Some(e) = d.index.get(off..off + DICT_INDEX_ENTRY_LEN) else {
        return Err(FormatError::Bounds { at: "dict index" });
    };
    let payload_off = u32::from_le_bytes(e[..4].try_into().expect("len 4")) as usize;
    let byte_len = u32::from_le_bytes(e[4..8].try_into().expect("len 4")) as usize;
    let Some(hdr) = d.payload.get(payload_off..payload_off + 4) else {
        return Err(FormatError::Bounds { at: "dict payload" });
    };
    let header = u32::from_le_bytes(hdr.try_into().expect("len 4"));
    let plen = varlena_4b_u_payload_len(header, "dict payload")? as usize;
    if plen != byte_len {
        return Err(FormatError::Corrupt {
            at: "dict entry byte_len",
        });
    }
    if payload_off + 4 + plen > d.payload.len() {
        return Err(FormatError::Bounds { at: "dict payload" });
    }
    Ok(d.payload[payload_off..].as_ptr() as u64)
}

/// Geometry facts the `dict_handle` face reports (spec §19.3) for the
/// reader's lazy handle construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DictLayout {
    pub entry_count: u32,
    pub frame_entries: u32,
}

// ---------------------------------------------------------------------------
// pgrc format 2.2 dictionary wrapper layouts (spec §6.4/§7): disk-only forms of the
// two dict streams, expanded back to the v1 images at extent open. Both are
// pure byte transforms pinned here; the zstd composition is the codec's.
// ---------------------------------------------------------------------------

/// Front-coding restart interval inside a dict frame (`Wrapper::FrontZstd`).
///
/// A format fact, not a tuned constant (P5-24, keep: the catalog's "byte-bounded frames" would be a new wrapper
/// layout): spec docs/design/sqe/pgrc-format.md §7.1 (pgrc format 2.2, ruled 2026-09-14) pins it, and every reader decodes
/// a written frame by it: `expand_front_coded_frame` takes a whole image at each `i % DICT_FC_RESTART == 0`, the
/// resident form keeps one restart offset per interval (`FC_RESTARTS`) and `fc_entry_into` steps at most
/// `DICT_FC_RESTART - 1` entries from one. A part written under one value does not decode under another, so a
/// change is a format version (spec §6.4's wrapper ids), never a retune. The lazy-vs-eager dictionary open the
/// catalog row also named is chosen by cost already (P5-07 keep 5c0f0bf34f1, P5-08 5b017a37058).
pub const DICT_FC_RESTART: u32 = 32;
/// Raw-offset anchor interval of the packed index (`Wrapper::PackedZstd`).
///
/// A format fact like `DICT_FC_RESTART` (P5-24, keep): spec §7.1 pins the packed index's `u32 x ceil(n / 64)`
/// anchors, and `expand_packed_dict_index` reads that many raw offsets and restarts the prefix sum at each
/// `i % DICT_INDEX_ANCHOR == 0`; a written index does not expand under another value.
pub const DICT_INDEX_ANCHOR: u32 = 64;
const _: () = assert!(DICT_FRAME_ENTRIES % DICT_FC_RESTART == 0);
const _: () = assert!(DICT_FRAME_ENTRIES % DICT_INDEX_ANCHOR == 0);

#[inline]
fn rec_off(records: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(records[i * 12..i * 12 + 4].try_into().expect("len 4"))
}
#[inline]
fn rec_len(records: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(records[i * 12 + 4..i * 12 + 8].try_into().expect("len 4"))
}
#[inline]
fn rec_char(records: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(records[i * 12 + 8..i * 12 + 12].try_into().expect("len 4"))
}

fn record_count(records: &[u8], at: &'static str) -> FormatResult<usize> {
    if records.len() % DICT_INDEX_ENTRY_LEN != 0 {
        return Err(FormatError::Corrupt { at });
    }
    Ok(records.len() / DICT_INDEX_ENTRY_LEN)
}

/// The 12 B records of dict frame `frame` inside a whole (v1-image) index.
pub fn dict_frame_records(index: &[u8], frame: u32) -> FormatResult<&[u8]> {
    let n = record_count(index, "dict index")?;
    let lo = (frame as usize) * DICT_FRAME_ENTRIES as usize;
    if lo >= n {
        return Err(FormatError::Bounds { at: "dict frame" });
    }
    let hi = (lo + DICT_FRAME_ENTRIES as usize).min(n);
    Ok(&index[lo * DICT_INDEX_ENTRY_LEN..hi * DICT_INDEX_ENTRY_LEN])
}

/// [D302] The DictPayload image length a v1 index allows: `(end, end + 7)`, inclusive. The writer's law
/// (`pgrc_write::dict`: entries in code order, each varlena image at its 8-aligned `payload_off`;
/// `StreamSectionWriter::finish` pads nothing after the last) ends the image at the last entry's last
/// byte, `end = payload_off + 4 + byte_len`; readers accept up to 7 bytes of pad past it, the same law
/// [`expand_front_coded_frame`] enforces on the last frame. An empty index allows `(0, 7)`. The index is
/// the CRC-validated resident image, so a section header whose `uncompressed_len` falls outside the range
/// disagrees with the writer's own record of the image: corrupt, refused before any image-sized reservation.
pub fn dict_payload_image_len_range(index: &[u8]) -> FormatResult<(u64, u64)> {
    let n = record_count(index, "dict index")?;
    if n == 0 {
        return Ok((0, 7));
    }
    let end = rec_off(index, n - 1) as u64 + 4 + rec_len(index, n - 1) as u64;
    Ok((end, end + 7))
}

fn lcp(a: &[u8], b: &[u8]) -> usize {
    let n = a.len().min(b.len());
    let mut i = 0;
    while i + 8 <= n {
        let x = u64::from_le_bytes(a[i..i + 8].try_into().expect("len 8"))
            ^ u64::from_le_bytes(b[i..i + 8].try_into().expect("len 8"));
        if x != 0 {
            return i + (x.trailing_zeros() / 8) as usize;
        }
        i += 8;
    }
    while i < n && a[i] == b[i] {
        i += 1;
    }
    i
}

pub(crate) fn put_varint(out: &mut Vec<u8>, mut v: u32) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80); // alloc: Exec charged caller
        v >>= 7;
    }
    out.push(v as u8); // alloc: Exec charged caller
}

pub(crate) fn get_varint(b: &[u8], pos: &mut usize, at: &'static str) -> FormatResult<u32> {
    let (mut v, mut shift) = (0u32, 0u32);
    loop {
        let byte = *b.get(*pos).ok_or(FormatError::Truncated { at })?;
        *pos += 1;
        if shift > 28 || (shift == 28 && byte > 0x0F) {
            return Err(FormatError::Corrupt { at });
        }
        v |= ((byte & 0x7F) as u32) << shift;
        if byte < 0x80 {
            return Ok(v);
        }
        shift += 7;
    }
}

/// Front-code one dict frame: `raw` is the frame's span of the v1 payload
/// image (starts at entry 0's header, 8-aligned), `records` that frame's
/// index records. Entry `i` is stored whole (its varlena image) at every
/// restart (`i % DICT_FC_RESTART == 0`), else as `varint(shared prefix
/// with entry i−1)` + the suffix bytes — no header, no padding.
pub fn front_code_frame(raw: &[u8], records: &[u8], out: &mut Vec<u8>) -> FormatResult<()> {
    let n = record_count(records, "dict frame records")?;
    if n == 0 {
        return Err(FormatError::EncodeContract { detail: "empty dict frame" });
    }
    let base = rec_off(records, 0) as usize;
    let mut prev: &[u8] = &[];
    for i in 0..n {
        let off = (rec_off(records, i) as usize)
            .checked_sub(base)
            .ok_or(FormatError::Corrupt { at: "dict frame offset" })?;
        let len = rec_len(records, i) as usize;
        let (image, bytes) = varlena_entry_at(raw, off, "dict frame entry")?;
        if bytes.len() != len {
            return Err(FormatError::Corrupt { at: "dict frame byte_len" });
        }
        if i as u32 % DICT_FC_RESTART == 0 {
            out.extend_from_slice(image); // alloc: Exec charged caller
        } else {
            let p = lcp(prev, bytes);
            put_varint(out, p as u32); // alloc: Exec charged caller
            out.extend_from_slice(&bytes[p..]); // alloc: Exec charged caller
        }
        prev = bytes;
    }
    Ok(())
}

/// Expand one front-coded frame into `dst`, the frame's exact span of the
/// v1 payload image: headers and 8-alignment padding rebuilt, every entry
/// placed at its record's offset. Byte-identical to the writer's image.
pub fn expand_front_coded_frame(fc: &[u8], records: &[u8], dst: &mut [u8]) -> FormatResult<()> {
    let n = record_count(records, "dict frame records")?;
    if n == 0 {
        return Err(FormatError::Corrupt { at: "empty dict frame" });
    }
    let base = rec_off(records, 0) as usize;
    let at = "front-coded dict frame";
    let mut pos = 0usize;
    let mut end = 0usize;
    let mut prev = (0usize, 0usize);
    for i in 0..n {
        let off = (rec_off(records, i) as usize)
            .checked_sub(base)
            .ok_or(FormatError::Corrupt { at })?;
        let len = rec_len(records, i) as usize;
        if off < end || off % 8 != 0 || off - end >= 8 {
            return Err(FormatError::Corrupt { at });
        }
        let stop = off.checked_add(4 + len).ok_or(FormatError::Corrupt { at })?;
        if stop > dst.len() {
            return Err(FormatError::Bounds { at });
        }
        dst[end..off].fill(0);
        dst[off..off + 4].copy_from_slice(&varlena_header_4b_u(len as u32).to_le_bytes());
        let body = off + 4;
        if i as u32 % DICT_FC_RESTART == 0 {
            let src = fc.get(pos..pos + 4 + len).ok_or(FormatError::Truncated { at })?;
            if u32::from_le_bytes(src[..4].try_into().expect("len 4")) != varlena_header_4b_u(len as u32) {
                return Err(FormatError::Corrupt { at });
            }
            dst[body..stop].copy_from_slice(&src[4..]);
            pos += 4 + len;
        } else {
            let p = get_varint(fc, &mut pos, at)? as usize;
            if p > len || p > prev.1 {
                return Err(FormatError::Corrupt { at });
            }
            dst.copy_within(prev.0..prev.0 + p, body);
            let src = fc.get(pos..pos + len - p).ok_or(FormatError::Truncated { at })?;
            dst[body + p..stop].copy_from_slice(src);
            pos += len - p;
        }
        prev = (body, len);
        end = stop;
    }
    if pos != fc.len() || dst.len() - end >= 8 {
        return Err(FormatError::Corrupt { at });
    }
    dst[end..].fill(0);
    Ok(())
}

fn bits(v: u32) -> u8 {
    (32 - v.leading_zeros()) as u8
}

struct BitWriter<'a> {
    out: &'a mut Vec<u8>,
    acc: u64,
    nbits: u32,
}

impl<'a> BitWriter<'a> {
    fn put(&mut self, v: u32, width: u8) {
        self.acc |= (v as u64) << self.nbits;
        self.nbits += width as u32;
        while self.nbits >= 8 {
            self.out.push(self.acc as u8); // alloc: Exec charged caller
            self.acc >>= 8;
            self.nbits -= 8;
        }
    }
    fn flush(&mut self) {
        if self.nbits > 0 {
            self.out.push(self.acc as u8); // alloc: Exec charged caller
        }
        self.acc = 0;
        self.nbits = 0;
    }
}

struct BitReader<'a> {
    b: &'a [u8],
    pos: usize,
    acc: u64,
    nbits: u32,
}

impl<'a> BitReader<'a> {
    fn get(&mut self, width: u8, at: &'static str) -> FormatResult<u32> {
        while self.nbits < width as u32 {
            let byte = *self.b.get(self.pos).ok_or(FormatError::Truncated { at })?;
            self.pos += 1;
            self.acc |= (byte as u64) << self.nbits;
            self.nbits += 8;
        }
        let v = (self.acc & ((1u64 << width) - 1)) as u32;
        self.acc >>= width;
        self.nbits -= width as u32;
        Ok(v)
    }
    fn align(&mut self) {
        self.acc = 0;
        self.nbits = 0;
    }
}

/// Pack a whole v1 index image: `u32 n`, then a raw `payload_off` anchor
/// every `DICT_INDEX_ANCHOR` entries, then per dict frame `{ u32 base_len,
/// u8 len_width, u8 delta_width, u16 0 }` + `byte_len − base_len` packed at
/// `len_width` bits + `byte_len − char_len` packed at `delta_width` bits
/// (LSB-first, each lane byte-padded). Requires the v1 laws the expand
/// leans on — offsets are the prefix sum of the 8-aligned entry sizes and
/// `char_field` is the absolute char count — and refuses typed otherwise.
pub fn pack_dict_index(records: &[u8], out: &mut Vec<u8>) -> FormatResult<()> {
    let n = record_count(records, "dict index")?;
    let bad = FormatError::EncodeContract { detail: "dict index not packable" };
    put_u32(out, n as u32);
    let mut expect = 0u32;
    for i in 0..n {
        let (off, len, ch) = (rec_off(records, i), rec_len(records, i), rec_char(records, i));
        if off != expect || ch > len {
            return Err(bad);
        }
        if i as u32 % DICT_INDEX_ANCHOR == 0 {
            put_u32(out, off);
        }
        expect = off.checked_add(4 + len).ok_or(bad)?.next_multiple_of(8);
    }
    let mut f = 0usize;
    while f < n {
        let m = (n - f).min(DICT_FRAME_ENTRIES as usize);
        let (mut lo, mut hi, mut dmax) = (u32::MAX, 0u32, 0u32);
        for i in f..f + m {
            let (len, ch) = (rec_len(records, i), rec_char(records, i));
            lo = lo.min(len);
            hi = hi.max(len);
            dmax = dmax.max(len - ch);
        }
        let (wl, wd) = (bits(hi - lo), bits(dmax));
        put_u32(out, lo);
        out.push(wl); // alloc: Exec charged caller
        out.push(wd); // alloc: Exec charged caller
        out.extend_from_slice(&[0, 0]); // alloc: Exec charged caller
        let mut w = BitWriter { out, acc: 0, nbits: 0 };
        for i in f..f + m {
            w.put(rec_len(records, i) - lo, wl); // alloc: Exec charged caller
        }
        w.flush();
        for i in f..f + m {
            w.put(rec_len(records, i) - rec_char(records, i), wd);
        }
        w.flush();
        f += m;
    }
    Ok(())
}

/// Expand a packed index into `out` (exactly `12 × n` bytes): offsets by
/// prefix sum from the anchors, absolute `char_len = byte_len − delta`.
pub fn expand_packed_dict_index(packed: &[u8], out: &mut [u8]) -> FormatResult<()> {
    let at = "packed dict index";
    let mut c = Cur::new(packed);
    let n = c.u32(at)? as usize;
    if out.len() != n * DICT_INDEX_ENTRY_LEN {
        return Err(FormatError::Corrupt { at });
    }
    let anchors = c.take(n.div_ceil(DICT_INDEX_ANCHOR as usize) * 4, at)?;
    let mut r = BitReader { b: packed, pos: c.off(), acc: 0, nbits: 0 };
    let mut off = 0u32;
    let mut f = 0usize;
    while f < n {
        let m = (n - f).min(DICT_FRAME_ENTRIES as usize);
        let hdr = r.b.get(r.pos..r.pos + 8).ok_or(FormatError::Truncated { at })?;
        let lo = u32::from_le_bytes(hdr[..4].try_into().expect("len 4"));
        let (wl, wd) = (hdr[4], hdr[5]);
        if wl > 32 || wd > 32 {
            return Err(FormatError::Corrupt { at });
        }
        r.pos += 8;
        for i in f..f + m {
            let len = lo.checked_add(r.get(wl, at)?).ok_or(FormatError::Corrupt { at })?;
            out[i * 12 + 4..i * 12 + 8].copy_from_slice(&len.to_le_bytes());
        }
        r.align();
        for i in f..f + m {
            let len = u32::from_le_bytes(out[i * 12 + 4..i * 12 + 8].try_into().expect("len 4"));
            let ch = len.checked_sub(r.get(wd, at)?).ok_or(FormatError::Corrupt { at })?;
            if i as u32 % DICT_INDEX_ANCHOR == 0 {
                let a = (i / DICT_INDEX_ANCHOR as usize) * 4;
                off = u32::from_le_bytes(anchors[a..a + 4].try_into().expect("len 4"));
            }
            out[i * 12..i * 12 + 4].copy_from_slice(&off.to_le_bytes());
            out[i * 12 + 8..i * 12 + 12].copy_from_slice(&ch.to_le_bytes());
            off = off.checked_add(4 + len).ok_or(FormatError::Corrupt { at })?.next_multiple_of(8);
        }
        r.align();
        f += m;
    }
    if r.pos != packed.len() {
        return Err(FormatError::Corrupt { at });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The resident front-coded form (lane dictresident, `PGRUST_PGRC_DICT_RESIDENT=frontcoded`):
// a FrontZstd payload's inflated frames kept front-coded in memory, addressed by frame and by
// restart; an entry is decoded on demand into a caller buffer (≤ DICT_FC_RESTART − 1 steps).
// Layout of the payload region: `u32 magic, u32 nframes, u32 nentries`, `u32 frame_off ×
// (nframes + 1)`, `u32 restart_off × nframes × FC_RESTARTS` (a restart slot past a short
// frame's last entry holds the frame's end), then the fc bytes; offsets are fc-relative.
// ---------------------------------------------------------------------------

pub const DICT_FC_RESIDENT_MAGIC: u32 = 0x3152_4346;
pub const FC_RESTARTS: usize = (DICT_FRAME_ENTRIES / DICT_FC_RESTART) as usize;
const FC_HDR: usize = 12;

#[derive(Clone, Copy)]
pub struct FcResident<'a> {
    pub nframes: u32,
    pub nentries: u32,
    frame_off: &'a [u8],
    restart_off: &'a [u8],
    pub fc: &'a [u8],
}

#[inline]
fn u32_at(b: &[u8], i: usize) -> usize {
    u32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().expect("len 4")) as usize
}

impl<'a> FcResident<'a> {
    pub fn parse(payload: &'a [u8]) -> FormatResult<FcResident<'a>> {
        let at = "front-coded resident payload";
        let mut c = Cur::new(payload);
        if c.u32(at)? != DICT_FC_RESIDENT_MAGIC {
            return Err(FormatError::Corrupt { at });
        }
        let nframes = c.u32(at)?;
        let nentries = c.u32(at)?;
        let frame_off = c.take((nframes as usize + 1) * 4, at)?;
        let restart_off = c.take(nframes as usize * FC_RESTARTS * 4, at)?;
        let fc = &payload[c.off()..];
        if u32_at(frame_off, nframes as usize) != fc.len() || nentries.div_ceil(DICT_FRAME_ENTRIES) != nframes {
            return Err(FormatError::Corrupt { at });
        }
        Ok(FcResident { nframes, nentries, frame_off, restart_off, fc })
    }

    /// The fc byte span of frame `f`.
    #[inline]
    pub fn frame_span(&self, f: u32) -> (usize, usize) {
        (u32_at(self.frame_off, f as usize), u32_at(self.frame_off, f as usize + 1))
    }

    #[inline]
    fn restart_pos(&self, f: u32, r: u32) -> usize {
        u32_at(self.restart_off, f as usize * FC_RESTARTS + r as usize)
    }

    /// The resident bytes of the form (tables + fc bytes).
    pub fn resident_len(&self) -> usize {
        FC_HDR + self.frame_off.len() + self.restart_off.len() + self.fc.len()
    }
}

/// Build the resident form over `index` (the v1 records of the whole dictionary): `inflate(f,
/// out)` appends frame `f`'s front-coded bytes (the FrontZstd block inflated, `front_code_frame`'s
/// output) to `out`. Each frame is walked once against its records to place the restart table.
pub fn fc_resident_build(index: &[u8], inflate: &mut dyn FnMut(u32, &mut Vec<u8>) -> FormatResult<()>, out: &mut Vec<u8>) -> FormatResult<()> {
    let at = "front-coded resident build";
    let n = record_count(index, "dict index")?;
    let nframes = (n as u32).div_ceil(DICT_FRAME_ENTRIES) as usize;
    out.clear();
    put_u32(out, DICT_FC_RESIDENT_MAGIC);
    put_u32(out, nframes as u32);
    put_u32(out, n as u32);
    let frame_tab = out.len();
    crate::try_resize(out, frame_tab + (nframes + 1) * 4, 0, "dict")?;
    let restart_tab = out.len();
    crate::try_resize(out, restart_tab + nframes * FC_RESTARTS * 4, 0, "dict")?;
    let base = out.len();
    let mut frame = Vec::new();
    for f in 0..nframes {
        frame.clear();
        inflate(f as u32, &mut frame)?;
        let fstart = out.len() - base;
        out[frame_tab + f * 4..frame_tab + f * 4 + 4].copy_from_slice(&(fstart as u32).to_le_bytes());
        let lo = f * DICT_FRAME_ENTRIES as usize;
        let hi = (lo + DICT_FRAME_ENTRIES as usize).min(n);
        let mut pos = 0usize;
        let mut prev_len = 0usize;
        for i in lo..hi {
            let len = rec_len(index, i) as usize;
            let k = (i - lo) as u32;
            if k % DICT_FC_RESTART == 0 {
                let r = (k / DICT_FC_RESTART) as usize;
                let slot = restart_tab + (f * FC_RESTARTS + r) * 4;
                out[slot..slot + 4].copy_from_slice(&((fstart + pos) as u32).to_le_bytes());
                let hdr = frame.get(pos..pos + 4).ok_or(FormatError::Truncated { at })?;
                if u32::from_le_bytes(hdr.try_into().expect("len 4")) != varlena_header_4b_u(len as u32) {
                    return Err(FormatError::Corrupt { at });
                }
                pos += 4 + len;
            } else {
                let p = get_varint(&frame, &mut pos, at)? as usize;
                if p > len || p > prev_len {
                    return Err(FormatError::Corrupt { at });
                }
                pos += len - p;
            }
            if pos > frame.len() {
                return Err(FormatError::Truncated { at });
            }
            prev_len = len;
        }
        if pos != frame.len() {
            return Err(FormatError::Corrupt { at });
        }
        let end = (fstart + pos) as u32;
        for r in ((hi - lo) as u32).div_ceil(DICT_FC_RESTART) as usize..FC_RESTARTS {
            let slot = restart_tab + (f * FC_RESTARTS + r) * 4;
            out[slot..slot + 4].copy_from_slice(&end.to_le_bytes());
        }
        out.extend_from_slice(&frame); // alloc: Exec charged caller
    }
    let end = (out.len() - base) as u32;
    out[frame_tab + nframes * 4..frame_tab + nframes * 4 + 4].copy_from_slice(&end.to_le_bytes());
    Ok(())
}

/// Decode entry `code` into `out[..4 + byte_len]` as its varlena image, from its restart: one
/// parse pass over the ≤ DICT_FC_RESTART entries before it (varints and lengths, no copy), then
/// each entry i contributes its suffix bytes below the smallest prefix any later entry keeps —
/// every output byte is written exactly once and nothing longer than the entry is ever staged.
pub fn fc_entry_into(r: &FcResident<'_>, index: &[u8], code: u32, out: &mut [u8]) -> FormatResult<usize> {
    let at = "front-coded resident entry";
    if code >= r.nentries || (code as usize + 1) * DICT_INDEX_ENTRY_LEN > index.len() {
        return Err(FormatError::Bounds { at: "dict code" });
    }
    let len_t = rec_len(index, code as usize) as usize;
    if out.len() < 4 + len_t {
        return Err(FormatError::Bounds { at });
    }
    let f = code / DICT_FRAME_ENTRIES;
    let r0 = (code % DICT_FRAME_ENTRIES) / DICT_FC_RESTART;
    let first = f * DICT_FRAME_ENTRIES + r0 * DICT_FC_RESTART;
    let (_, fend) = r.frame_span(f);
    let fc = r.fc.get(..fend).ok_or(FormatError::Corrupt { at })?;
    let mut pos = r.restart_pos(f, r0);
    let steps = (code - first) as usize;
    let mut sp = [(0u32, 0u32); DICT_FC_RESTART as usize];
    let mut prev_len = rec_len(index, first as usize) as usize;
    let hdr = fc.get(pos..pos + 4).ok_or(FormatError::Truncated { at })?;
    if u32::from_le_bytes(hdr.try_into().expect("len 4")) != varlena_header_4b_u(prev_len as u32) {
        return Err(FormatError::Corrupt { at });
    }
    sp[0] = (pos as u32 + 4, 0);
    pos += 4 + prev_len;
    for (i, s) in sp.iter_mut().enumerate().take(steps + 1).skip(1) {
        let p = get_varint(fc, &mut pos, at)? as usize;
        let li = rec_len(index, first as usize + i) as usize;
        if p > li || p > prev_len {
            return Err(FormatError::Corrupt { at });
        }
        *s = (pos as u32, p as u32);
        pos += li - p;
        prev_len = li;
    }
    if pos > fc.len() {
        return Err(FormatError::Truncated { at });
    }
    out[..4].copy_from_slice(&varlena_header_4b_u(len_t as u32).to_le_bytes());
    let mut need = len_t;
    for i in (0..=steps).rev() {
        let (spos, p) = (sp[i].0 as usize, sp[i].1 as usize);
        let li = rec_len(index, first as usize + i) as usize;
        let hi = li.min(need);
        if p < hi {
            out[4 + p..4 + hi].copy_from_slice(&fc[spos..spos + hi - p]);
        }
        need = need.min(p);
        if need == 0 {
            break;
        }
    }
    Ok(len_t)
}

/// A sequential walk over codes `[code, end)` of the resident form: `next` decodes the next
/// entry in place onto the previous one in `out` (the shared prefix is already there), so a
/// full walk costs one suffix copy per entry.
pub struct FcWalk<'a> {
    r: FcResident<'a>,
    index: &'a [u8],
    code: u32,
    end: u32,
    pos: usize,
    prev_len: usize,
    /// A start inside a restart run: the first `next` decodes from the run's restart up to it.
    warm: Option<u32>,
}

impl<'a> FcWalk<'a> {
    pub fn new(r: FcResident<'a>, index: &'a [u8], start: u32, end: u32) -> FormatResult<FcWalk<'a>> {
        if start > end || end > r.nentries || end as usize * DICT_INDEX_ENTRY_LEN > index.len() {
            return Err(FormatError::Bounds { at: "dict code" });
        }
        let warm = (start < end && start % DICT_FC_RESTART != 0).then(|| start - start % DICT_FC_RESTART);
        Ok(FcWalk { r, index, code: start, end, pos: 0, prev_len: 0, warm })
    }

    /// The next `(code, byte_len)`, its image in `out[..4 + byte_len]`; `None` at the end.
    #[inline]
    pub fn next(&mut self, out: &mut Vec<u8>) -> FormatResult<Option<(u32, usize)>> {
        if let Some(from) = self.warm.take() {
            let start = self.code;
            self.code = from;
            while self.code < start {
                self.step(out)?;
            }
        }
        self.step(out)
    }

    #[inline]
    fn step(&mut self, out: &mut Vec<u8>) -> FormatResult<Option<(u32, usize)>> {
        let at = "front-coded resident walk";
        if self.code >= self.end {
            return Ok(None);
        }
        let code = self.code;
        let len = rec_len(self.index, code as usize) as usize;
        if out.len() < 4 + len {
            crate::try_resize(out, 4 + len, 0, "dict")?; // alloc: Cold
        }
        let k = code % DICT_FRAME_ENTRIES;
        if k % DICT_FC_RESTART == 0 {
            self.pos = self.r.restart_pos(code / DICT_FRAME_ENTRIES, k / DICT_FC_RESTART);
            let src = self.r.fc.get(self.pos..self.pos + 4 + len).ok_or(FormatError::Truncated { at })?;
            if u32::from_le_bytes(src[..4].try_into().expect("len 4")) != varlena_header_4b_u(len as u32) {
                return Err(FormatError::Corrupt { at });
            }
            out[..4 + len].copy_from_slice(src);
            self.pos += 4 + len;
        } else {
            let p = get_varint(self.r.fc, &mut self.pos, at)? as usize;
            if p > len || p > self.prev_len {
                return Err(FormatError::Corrupt { at });
            }
            let src = self.r.fc.get(self.pos..self.pos + len - p).ok_or(FormatError::Truncated { at })?;
            out[..4].copy_from_slice(&varlena_header_4b_u(len as u32).to_le_bytes());
            out[4 + p..4 + len].copy_from_slice(src);
            self.pos += len - p;
        }
        self.prev_len = len;
        self.code = code + 1;
        Ok(Some((code, len)))
    }
}

/// Entry `code` of a front-coded `DictSections` materialised into `arena` as a varlena image: the
/// datum word (the executor's and the fetch's copy under the knob).
pub fn dict_fc_alloc_varlena(d: &DictSections<'_>, code: u32, arena: &mut crate::abi::ByteArena<'_>) -> FormatResult<u64> {
    if code >= d.entry_count {
        return Err(FormatError::Bounds { at: "dict code" });
    }
    let r = FcResident::parse(d.payload)?;
    let off = code as usize * DICT_INDEX_ENTRY_LEN;
    let rec = d.index.get(off..off + DICT_INDEX_ENTRY_LEN).ok_or(FormatError::Bounds { at: "dict index" })?;
    let len = u32::from_le_bytes(rec[4..8].try_into().expect("len 4")) as usize;
    let slot = arena.alloc(4 + len)?;
    fc_entry_into(&r, d.index, code, slot)?;
    Ok(slot.as_ptr() as u64)
}
