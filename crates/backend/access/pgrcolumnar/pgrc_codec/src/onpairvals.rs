//! `Wrapper::OnPairZstd` (verbatim varlena `Values`): each frame's slot table and entry headers become varints
//! (the frame's gap and value count, then per entry its payload length or its overflow reference), the entries'
//! payload bytes are tokenized by [`crate::onpair`] against one table per section, then zstd. Blocks: the varints
//! and the token stream in one block, then a side block holding that block's length and the token table. The
//! writer offers the arm only when the varints and bytes rebuild the section's exact payload, so the unwrap
//! hands readers the same image `Zstd` does.

use crate::onpair::{self, DecodeTable, Dict};
use crate::onpairwrap::{inflate, onpair_verbatim_on, payload_end, varint, MIN_IMAGE, PARAMS};
use crate::wrapper::{level_for, zstd_compress_block_at, zstd_level, zstd_level_text, FILL_PIECE};
use pgrc_format::enc::{EncodingId, Wrapper};
use pgrc_format::part::{StreamSectionHdr, OVERFLOW_MARK, STREAM_SECTION_HDR_LEN, STREAM_SECTION_MAGIC};
use pgrc_format::wire::varlena_header_4b_u;
use pgrc_format::{FormatError, FormatResult};

const AT: &str = "onpair verbatim values";
const BAD: FormatError = FormatError::Corrupt { at: AT };
/// Block table: two blocks (varints + tokens, then the side block).
const BTAB: usize = 12;

/// Where [`split`] writes: the bytes, or only their count (the first pass sizes the buffers exactly, so neither
/// holds a payload-sized reservation beside the trained table).
trait Sink {
    fn put(&mut self, b: &[u8]);
}

impl Sink for Vec<u8> {
    fn put(&mut self, b: &[u8]) {
        self.extend_from_slice(b); // alloc: Exec charged caller
    }
}

impl Sink for usize {
    fn put(&mut self, b: &[u8]) {
        *self += b.len();
    }
}

fn put_varint(mut v: u64, out: &mut impl Sink) {
    let (mut b, mut n) = ([0u8; 10], 0);
    while v >= 0x80 {
        b[n] = v as u8 | 0x80;
        v >>= 7;
        n += 1;
    }
    b[n] = v as u8;
    out.put(&b[..=n]);
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at.checked_add(4)?)?.try_into().ok()?))
}

fn put_u32(dst: &mut [u8], at: usize, v: u32) -> FormatResult<()> {
    dst.get_mut(at..at.checked_add(4).ok_or(BAD)?).ok_or(BAD)?.copy_from_slice(&v.to_le_bytes());
    Ok(())
}

/// The payload's frames (`starts` payload-relative) as varints on `meta` and entry payload bytes on `text`;
/// `None` = a layout the arm does not carry.
fn split(payload: &[u8], starts: &[u32], meta: &mut impl Sink, text: &mut impl Sink) -> Option<()> {
    let mut end = 0usize;
    for (j, &fs) in starts.iter().enumerate() {
        let (fs, at) = (fs as usize, end.next_multiple_of(8));
        if fs < at || (fs - at) % 8 != 0 {
            return None;
        }
        let fe = starts.get(j + 1).map_or(payload.len(), |&s| s as usize);
        let fr = payload.get(fs..fe)?;
        let eb = u32_at(fr, 0)? as usize;
        if eb < 8 || eb % 8 != 0 {
            return None;
        }
        let vif = if u32_at(fr, eb - 4)? == 0 { eb / 4 - 2 } else { eb / 4 - 1 };
        put_varint(((fs - at) / 8) as u64, meta);
        put_varint(vif as u64, meta);
        for i in 0..vif {
            let off = u32_at(fr, i * 4)? as usize;
            let h = u32_at(fr, off)?;
            if h == OVERFLOW_MARK {
                put_varint((u32_at(fr, off + 4)? as u64) << 1 | 1, meta);
                put_varint(u64::from_le_bytes(fr.get(off + 8..off + 16)?.try_into().ok()?), meta);
            } else {
                if h & 3 != 0 || h >> 2 < 4 {
                    return None;
                }
                let len = (h >> 2) as usize - 4;
                put_varint((len as u64) << 1, meta);
                text.put(fr.get(off + 4..off + 4 + len)?);
            }
        }
        end = fs + u32_at(fr, vif * 4)? as usize;
    }
    Some(())
}

/// Where the entries' bytes come from: a buffer apart, or the image itself from an offset (the decoded text laid
/// at the image's tail: every write stays at or behind the next unread byte, the text being the image less its
/// headers, slots and pads).
#[derive(Clone, Copy)]
enum Src<'a> {
    Apart(&'a [u8]),
    Tail(usize),
}

/// One frame from `meta[*m..]` (its value count, then its entries) and the entry bytes from `t` on, into `dst`
/// at the 8-aligned `fs`; pads written as zeros. The frame's end (its entries' end).
fn rebuild_frame(meta: &[u8], m: &mut usize, src: Src<'_>, t: &mut usize, dst: &mut [u8], fs: usize) -> FormatResult<usize> {
    let vif = varint(meta, m)?;
    let slots = vif.checked_add(1).ok_or(BAD)?.checked_mul(4).ok_or(BAD)?;
    let mut e = fs.checked_add(slots.next_multiple_of(8)).ok_or(BAD)?;
    dst.get_mut(fs + slots..e).ok_or(BAD)?.fill(0);
    for i in 0..vif {
        let a = e;
        e = e.next_multiple_of(8);
        dst.get_mut(a..e).ok_or(BAD)?.fill(0);
        put_u32(dst, fs + i * 4, u32::try_from(e - fs).map_err(|_| BAD)?)?;
        let v = varint(meta, m)?;
        if v & 1 == 1 {
            let ovf = varint(meta, m)? as u64;
            put_u32(dst, e, OVERFLOW_MARK)?;
            put_u32(dst, e + 4, u32::try_from(v >> 1).map_err(|_| BAD)?)?;
            dst.get_mut(e + 8..e + 16).ok_or(BAD)?.copy_from_slice(&ovf.to_le_bytes());
            e += 16;
        } else {
            let len = v >> 1;
            if len > (u32::MAX >> 2) as usize - 4 {
                return Err(BAD);
            }
            put_u32(dst, e, varlena_header_4b_u(len as u32))?;
            match src {
                Src::Apart(text) => dst.get_mut(e + 4..e + 4 + len).ok_or(BAD)?.copy_from_slice(text.get(*t..*t + len).ok_or(BAD)?),
                Src::Tail(r0) => {
                    if r0 + *t + len > dst.len() || e + 4 + len > dst.len() {
                        return Err(BAD);
                    }
                    dst.copy_within(r0 + *t..r0 + *t + len, e + 4);
                }
            }
            *t += len;
            e += 4 + len;
        }
    }
    put_u32(dst, fs + vif * 4, u32::try_from(e - fs).map_err(|_| BAD)?)?;
    Ok(e)
}

/// The payload image from its varints and the entry bytes into `dst` (the payload's length); `Ok(false)` when
/// `stop` said stop.
fn rebuild(meta: &[u8], src: Src<'_>, text_len: usize, dst: &mut [u8], stop: &mut dyn FnMut() -> bool) -> FormatResult<bool> {
    let (mut m, mut t, mut end, mut frames) = (0usize, 0usize, 0usize, 0u32);
    while m < meta.len() {
        frames = frames.wrapping_add(1);
        if frames % 256 == 0 && stop() {
            return Ok(false);
        }
        let fs = end.next_multiple_of(8).checked_add(varint(meta, &mut m)?.checked_mul(8).ok_or(BAD)?).ok_or(BAD)?;
        dst.get_mut(end..fs).ok_or(BAD)?.fill(0);
        end = rebuild_frame(meta, &mut m, src, &mut t, dst, fs)?;
    }
    if t != text_len {
        return Err(BAD);
    }
    dst.get_mut(end..).ok_or(BAD)?.fill(0);
    Ok(true)
}

/// Whether [`rebuild`] gives back `payload` byte for byte, checked a frame at a time (`starts` = its frame
/// starts): the arm is offered only then.
fn rebuilds_exactly(payload: &[u8], starts: &[u32], meta: &[u8], text: &[u8]) -> FormatResult<bool> {
    let (mut m, mut t, mut end) = (0usize, 0usize, 0usize);
    let mut scratch: Vec<u8> = Vec::new();
    if starts.first().is_some_and(|&f| payload[..(f as usize).min(payload.len())].iter().any(|&b| b != 0)) {
        return Ok(false);
    }
    for (j, &fs) in starts.iter().enumerate() {
        let fs = fs as usize;
        let fe = starts.get(j + 1).map_or(payload.len(), |&s| s as usize);
        let Ok(gap) = varint(meta, &mut m) else { return Ok(false) };
        if fe < fs || fe > payload.len() || end.next_multiple_of(8) + gap * 8 != fs {
            return Ok(false);
        }
        scratch.clear();
        pgrc_format::try_resize(&mut scratch, fe - fs, 0, AT)?;
        let Ok(used) = rebuild_frame(meta, &mut m, Src::Apart(text), &mut t, &mut scratch, 0) else { return Ok(false) };
        if scratch.as_slice() != &payload[fs..fe] {
            return Ok(false);
        }
        end = fs + used;
    }
    Ok(m == meta.len() && t == text.len() && (!starts.is_empty() || payload.iter().all(|&b| b == 0)))
}

fn put_header(out: &mut [u8], hdr: &StreamSectionHdr, wrapper: Wrapper, frame_table_off: u32, gcount_table_off: u32, unc_len: u32) {
    let h = &mut out[..STREAM_SECTION_HDR_LEN];
    h[0..4].copy_from_slice(&STREAM_SECTION_MAGIC.to_le_bytes());
    h[4..6].copy_from_slice(&hdr.encoding.to_le_bytes());
    h[6] = hdr.width;
    h[7] = wrapper.as_u8();
    h[8..12].copy_from_slice(&hdr.frame_count.to_le_bytes());
    h[12..16].copy_from_slice(&frame_table_off.to_le_bytes());
    h[16..20].copy_from_slice(&gcount_table_off.to_le_bytes());
    h[20..24].copy_from_slice(&unc_len.to_le_bytes());
    h[24..28].copy_from_slice(&hdr.value_count.to_le_bytes());
    h[28..32].copy_from_slice(&0u32.to_le_bytes());
}

/// The table offsets of a section whose tables start at `tables_start` (the source's order kept).
fn table_offs(hdr: &StreamSectionHdr, pend: usize, tables_start: u32) -> FormatResult<(u32, u32)> {
    let ft = if hdr.frame_table_off != 0 { tables_start } else { 0 };
    let gc = match hdr.gcount_table_off {
        0 => 0,
        g => tables_start + g.checked_sub(pend as u32).ok_or(FormatError::Corrupt { at: "gcount table offset" })?,
    };
    Ok((ft, gc))
}

/// The arm over an UNWRAPPED verbatim varlena `Values` section, or `None` when it is not offered (switch off, no
/// zstd in this build, a small image, a layout the arm does not carry).
pub fn offer_values(section: &[u8]) -> FormatResult<Option<Vec<u8>>> {
    if !onpair_verbatim_on() || !crate::wrapper::wrapper_available(Wrapper::OnPairZstd) {
        return Ok(None);
    }
    wrap_values(section)
}

/// [`offer_values`] whatever the switch says.
pub fn wrap_values(section: &[u8]) -> FormatResult<Option<Vec<u8>>> {
    let hdr = StreamSectionHdr::decode(section)?;
    if hdr.wrapper != Wrapper::None.as_u8() || hdr.encoding != EncodingId::Verbatim.as_u16() {
        return Ok(None);
    }
    let pend = payload_end(&hdr, section);
    if pend < STREAM_SECTION_HDR_LEN + MIN_IMAGE || pend > section.len() {
        return Ok(None);
    }
    let Some(starts) = hdr.frame_table(section)? else { return Ok(None) };
    let payload = &section[STREAM_SECTION_HDR_LEN..pend];
    let (mut meta_len, mut text_len) = (0usize, 0usize);
    if split(payload, &starts, &mut meta_len, &mut text_len).is_none() {
        return Ok(None);
    }
    let (mut meta, mut text) = (Vec::new(), Vec::new());
    pgrc_format::try_reserve_more(&mut text, text_len, AT)?;
    pgrc_format::try_reserve_more(&mut meta, meta_len, AT)?;
    if split(payload, &starts, &mut meta, &mut text).is_none() {
        return Ok(None);
    }
    if !rebuilds_exactly(payload, &starts, &meta, &text)? {
        return Ok(None);
    }
    let dict = Dict::train(&text, &PARAMS)?;
    let mut blk = Vec::new();
    pgrc_format::try_reserve_more(&mut blk, 4 + meta.len() + dict.stream_bound(&text), AT)?;
    blk.extend_from_slice(&(meta.len() as u32).to_le_bytes()); // alloc: Exec charged caller
    blk.extend_from_slice(&meta); // alloc: Exec charged caller
    drop(meta);
    dict.encode(&text, dict.form(), &mut blk)?;
    drop(text);
    let mut side: Vec<u8> = pgrc_format::try_vec(4, AT)?;
    side.extend_from_slice(&(blk.len() as u32).to_le_bytes()); // alloc: Exec charged caller
    dict.write_table(&mut side)?;
    drop(dict);
    let tables = &section[pend..];
    let mut out = Vec::new();
    pgrc_format::try_resize(&mut out, STREAM_SECTION_HDR_LEN + BTAB, 0, AT)?;
    let level = level_for(Wrapper::OnPairZstd, hdr.encoding, hdr.width, zstd_level(), zstd_level_text());
    zstd_compress_block_at(&blk, &mut out, level)?;
    let o1 = (out.len() - STREAM_SECTION_HDR_LEN) as u32;
    zstd_compress_block_at(&side, &mut out, level)?;
    let o2 = (out.len() - STREAM_SECTION_HDR_LEN) as u32;
    for (i, o) in [BTAB as u32, o1, o2].iter().enumerate() {
        out[STREAM_SECTION_HDR_LEN + i * 4..STREAM_SECTION_HDR_LEN + i * 4 + 4].copy_from_slice(&o.to_le_bytes());
    }
    let (ft, gc) = table_offs(&hdr, pend, out.len() as u32)?;
    pgrc_format::try_reserve_more(&mut out, tables.len(), AT)?;
    out.extend_from_slice(tables); // alloc: Exec charged caller
    put_header(&mut out, &hdr, Wrapper::OnPairZstd, ft, gc, payload.len() as u32);
    out.shrink_to_fit();
    Ok(Some(out))
}

/// The whole-section unwrap into the verbatim image (`scratch` cleared first); `Ok(false)` when `stop` said
/// stop, `scratch` then empty.
pub fn unwrap_values(hdr: &StreamSectionHdr, section: &[u8], scratch: &mut Vec<u8>, stop: &mut dyn FnMut() -> bool) -> FormatResult<bool> {
    scratch.clear();
    let pend = payload_end(hdr, section);
    if pend < STREAM_SECTION_HDR_LEN + BTAB || pend > section.len() {
        return Err(FormatError::Bounds { at: "payload region" });
    }
    let wrapped = &section[STREAM_SECTION_HDR_LEN..pend];
    let tables = &section[pend..];
    let offs = [0, 1, 2].map(|i| u32_at(wrapped, i * 4).unwrap_or(0) as usize);
    if offs[0] != BTAB || offs[1] < offs[0] || offs[2] < offs[1] || offs[2] > wrapped.len() {
        return Err(FormatError::Corrupt { at: "wrapper block offsets" });
    }
    let img_len = hdr.uncompressed_len as usize;
    let mut side = Vec::new();
    let n = inflate(&wrapped[offs[1]..offs[2]], 4 + 2 + 4 * (onpair::MAX_IDS - 256), &mut side)?;
    let blen = u32_at(&side[..n], 0).ok_or(FormatError::Truncated { at: AT })? as usize;
    let (table, used): (DecodeTable, usize) = DecodeTable::read(&side[4..n])?;
    if 4 + used != n || blen < 4 || blen > 3 * img_len + 64 {
        return Err(BAD);
    }
    drop(side);
    let mut blk = Vec::new();
    if inflate(&wrapped[offs[0]..offs[1]], blen, &mut blk)? != blen {
        return Err(BAD);
    }
    let ml = u32_at(&blk, 0).ok_or(BAD)? as usize;
    let meta = blk.get(4..4usize.checked_add(ml).ok_or(BAD)?).ok_or(FormatError::Truncated { at: AT })?;
    let stream = &blk[4 + ml..blen];
    let (_, text_len, _, _) = onpair::stream_header(stream)?;
    if text_len > img_len {
        return Err(BAD);
    }
    if stop() {
        return Ok(false);
    }
    let need = STREAM_SECTION_HDR_LEN + img_len + tables.len();
    scratch.try_reserve_exact(need).map_err(|_| FormatError::Bounds { at: "unwrapped section reservation" })?;
    while scratch.len() < need {
        scratch.resize(need.min(scratch.len() + FILL_PIECE), 0);
        if scratch.len() < need && stop() {
            scratch.clear();
            return Ok(false);
        }
    }
    let tables_start = STREAM_SECTION_HDR_LEN as u32 + hdr.uncompressed_len;
    let (ft, gc) = table_offs(hdr, pend, tables_start)?;
    put_header(scratch, hdr, Wrapper::None, ft, gc, 0);
    let dst = &mut scratch[STREAM_SECTION_HDR_LEN..STREAM_SECTION_HDR_LEN + img_len];
    let r0 = img_len - text_len;
    if onpair::decode(&table, stream, &mut dst[r0..])? != stream.len() {
        return Err(BAD);
    }
    if !rebuild(meta, Src::Tail(r0), text_len, dst, stop)? {
        scratch.clear();
        return Ok(false);
    }
    scratch[STREAM_SECTION_HDR_LEN + img_len..].copy_from_slice(tables);
    Ok(true)
}
