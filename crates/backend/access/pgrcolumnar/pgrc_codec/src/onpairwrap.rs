//! `Wrapper::FrontOnPairZstd` (DictPayload): each dict frame front-coded as for `FrontZstd`, its prefix
//! varints kept apart from its string bytes, the string bytes tokenized by [`crate::onpair`] against
//! one table per section, then zstd. Blocks: one per dict frame (`u32 varint bytes | varints | onpair
//! stream`), then one side block holding the token table. The unwrap rebuilds the exact v1 image (or
//! the front-coded resident form), so readers and kernels see the bytes `FrontZstd` gives them.

use crate::onpair::{self, DecodeTable, Dict, Params};
use crate::wrapper::{
    dict_payload_len_checked, frame_block_ends, level_for, zstd_compress_block_at, zstd_decompress_block, zstd_level, zstd_level_text,
    FILL_PIECE,
};
use pgrc_format::dict::{dict_frame_records, expand_front_coded_frame, front_code_frame, DICT_FC_RESTART, DICT_INDEX_ENTRY_LEN};
use pgrc_format::enc::Wrapper;
use pgrc_format::part::{StreamSectionHdr, STREAM_SECTION_HDR_LEN, STREAM_SECTION_MAGIC};
use pgrc_format::wire::varlena_header_4b_u;
use pgrc_format::{FormatError, FormatResult};

pub const PARAMS: Params = Params { sample_max: 2 << 20, window: 4096, max_ids: onpair::MAX_IDS, threshold: 0 };
/// The smallest payload image the arm is priced on: below it the table outweighs what tokens save.
pub const MIN_IMAGE: usize = 64 << 10;
const AT: &str = "front onpair dict payload";

/// `PGRUST_PGRC_ONPAIR=0|1` (read once): whether the seal prices the OnPair arms.
pub fn onpair_on() -> bool {
    static ON: pgsync::OnceLock<bool> = pgsync::OnceLock::new();
    *ON.get_or_init(|| match std::env::var("PGRUST_PGRC_ONPAIR").as_deref().map(str::trim) {
        Ok("0") => false,
        Ok("1") => true,
        _ => ONPAIR_DEFAULT,
    })
}

const ONPAIR_DEFAULT: bool = true;

/// `PGRUST_PGRC_ONPAIR_VERBATIM=0|1` (read once): whether the seal prices the OnPair arm of verbatim varlena
/// `Values` streams ([`crate::onpairvals`]).
pub fn onpair_verbatim_on() -> bool {
    static ON: pgsync::OnceLock<bool> = pgsync::OnceLock::new();
    *ON.get_or_init(|| match std::env::var("PGRUST_PGRC_ONPAIR_VERBATIM").as_deref().map(str::trim) {
        Ok("0") => false,
        Ok("1") => true,
        _ => ONPAIR_VERBATIM_DEFAULT,
    })
}

const ONPAIR_VERBATIM_DEFAULT: bool = true;

fn rec_len(records: &[u8], i: usize) -> usize {
    u32::from_le_bytes(records[i * 12 + 4..i * 12 + 8].try_into().expect("len 4")) as usize
}

pub(crate) fn varint(b: &[u8], pos: &mut usize) -> FormatResult<usize> {
    let (mut v, mut shift) = (0usize, 0u32);
    loop {
        let byte = *b.get(*pos).ok_or(FormatError::Truncated { at: AT })?;
        *pos += 1;
        if shift > 28 {
            return Err(FormatError::Corrupt { at: AT });
        }
        v |= ((byte & 0x7F) as usize) << shift;
        if byte < 0x80 {
            return Ok(v);
        }
        shift += 7;
    }
}

/// A front-coded frame's prefix varints and string bytes, apart.
fn split_fc(fc: &[u8], records: &[u8], meta: &mut Vec<u8>, text: &mut Vec<u8>) -> FormatResult<()> {
    let mut pos = 0;
    for i in 0..records.len() / DICT_INDEX_ENTRY_LEN {
        let len = rec_len(records, i);
        let take = if i as u32 % DICT_FC_RESTART == 0 {
            pos += 4;
            len
        } else {
            let p0 = pos;
            let p = varint(fc, &mut pos)?;
            meta.extend_from_slice(&fc[p0..pos]); // alloc: Exec charged caller
            len.checked_sub(p).ok_or(FormatError::Corrupt { at: AT })?
        };
        text.extend_from_slice(fc.get(pos..pos + take).ok_or(FormatError::Corrupt { at: AT })?); // alloc: Exec charged caller
        pos += take;
    }
    if pos != fc.len() {
        return Err(FormatError::Corrupt { at: AT });
    }
    Ok(())
}

/// The inverse of [`split_fc`]: the front-coded frame back from its varints and string bytes.
fn join_fc(meta: &[u8], text: &[u8], records: &[u8], fc: &mut Vec<u8>) -> FormatResult<()> {
    let (mut m, mut t) = (0, 0);
    for i in 0..records.len() / DICT_INDEX_ENTRY_LEN {
        let len = rec_len(records, i);
        let take = if i as u32 % DICT_FC_RESTART == 0 {
            fc.extend_from_slice(&varlena_header_4b_u(len as u32).to_le_bytes()); // alloc: Exec charged caller
            len
        } else {
            let m0 = m;
            let p = varint(meta, &mut m)?;
            fc.extend_from_slice(&meta[m0..m]); // alloc: Exec charged caller
            len.checked_sub(p).ok_or(FormatError::Corrupt { at: AT })?
        };
        fc.extend_from_slice(text.get(t..t + take).ok_or(FormatError::Corrupt { at: AT })?); // alloc: Exec charged caller
        t += take;
    }
    if m != meta.len() || t != text.len() {
        return Err(FormatError::Corrupt { at: AT });
    }
    Ok(())
}

pub(crate) fn payload_end(hdr: &StreamSectionHdr, section: &[u8]) -> usize {
    if hdr.frame_table_off != 0 {
        hdr.frame_table_off as usize
    } else if hdr.gcount_table_off != 0 {
        hdr.gcount_table_off as usize
    } else {
        section.len()
    }
}

/// The arm over an UNWRAPPED DictPayload section (`index` = the v1 index image), or `None` when it is
/// not offered (switch off, no zstd in this build, a small image).
pub fn offer_dict_payload(section: &[u8], index: &[u8]) -> FormatResult<Option<Vec<u8>>> {
    if !onpair_on() || !crate::wrapper::wrapper_available(Wrapper::FrontOnPairZstd) {
        return Ok(None);
    }
    let hdr = StreamSectionHdr::decode(section)?;
    if (hdr.uncompressed_len as usize) < MIN_IMAGE && section.len() < MIN_IMAGE + STREAM_SECTION_HDR_LEN {
        return Ok(None);
    }
    let mut out = Vec::new();
    wrap_dict_payload(section, index, &mut out)?;
    Ok(Some(out))
}

/// [`wrap_dict_payload_at`] at the string-data level ([`level_for`]: `PGRUST_PGRC_ZSTD_LEVEL_TEXT` when set, else
/// `PGRUST_PGRC_ZSTD_LEVEL`).
pub fn wrap_dict_payload(section: &[u8], index: &[u8], out: &mut Vec<u8>) -> FormatResult<()> {
    let hdr = StreamSectionHdr::decode(section)?;
    let level = level_for(Wrapper::FrontOnPairZstd, hdr.encoding, hdr.width, zstd_level(), zstd_level_text());
    wrap_dict_payload_at(section, index, out, level)
}

/// The arm with every zstd block (frames and the table) at `level`.
pub fn wrap_dict_payload_at(section: &[u8], index: &[u8], out: &mut Vec<u8>, level: i32) -> FormatResult<()> {
    let hdr = StreamSectionHdr::decode(section)?;
    if hdr.wrapper != Wrapper::None.as_u8() {
        return Err(FormatError::EncodeContract { detail: "wrap_section input already wrapped" });
    }
    let pend = payload_end(&hdr, section);
    if pend < STREAM_SECTION_HDR_LEN || pend > section.len() {
        return Err(FormatError::Bounds { at: "payload region" });
    }
    let payload = &section[STREAM_SECTION_HDR_LEN..pend];
    let tables = &section[pend..];
    let ends = frame_block_ends(&hdr, section)?;
    let nf = ends.len();
    let (mut meta, mut text, mut fc) = (Vec::new(), Vec::new(), Vec::new());
    let mut spans: Vec<(usize, usize, usize, usize)> = pgrc_format::try_vec(nf, AT)?;
    let mut g0 = 0usize;
    for (b, &end) in ends.iter().enumerate() {
        let raw = payload.get(g0..end as usize).ok_or(FormatError::EncodeContract { detail: "granule payload boundaries" })?;
        let recs = dict_frame_records(index, b as u32)?;
        fc.clear();
        front_code_frame(raw, recs, &mut fc)?;
        let (m0, t0) = (meta.len(), text.len());
        split_fc(&fc, recs, &mut meta, &mut text)?;
        spans.push((m0, meta.len(), t0, text.len()));
        g0 = end as usize;
    }
    let dict = Dict::train(&text, &PARAMS)?;
    let form = dict.form();

    let start = out.len();
    pgrc_format::try_reserve_more(out, STREAM_SECTION_HDR_LEN + (nf + 2) * 4 + text.len() + meta.len() / 2 + tables.len(), AT)?;
    pgrc_format::try_resize(out, start + STREAM_SECTION_HDR_LEN + (nf + 2) * 4, 0, AT)?;
    let btab = start + STREAM_SECTION_HDR_LEN;
    let mut offs: Vec<u32> = pgrc_format::try_vec(nf + 2, AT)?;
    offs.push((out.len() - btab) as u32);
    let mut blk = Vec::new();
    for &(m0, m1, t0, t1) in &spans {
        blk.clear();
        blk.extend_from_slice(&((m1 - m0) as u32).to_le_bytes());
        blk.extend_from_slice(&meta[m0..m1]);
        dict.encode(&text[t0..t1], form, &mut blk)?;
        zstd_compress_block_at(&blk, out, level)?;
        offs.push((out.len() - btab) as u32);
    }
    blk.clear();
    dict.write_table(&mut blk)?;
    zstd_compress_block_at(&blk, out, level)?;
    offs.push((out.len() - btab) as u32);
    for (i, o) in offs.iter().enumerate() {
        out[btab + i * 4..btab + i * 4 + 4].copy_from_slice(&o.to_le_bytes());
    }
    let tables_start = (out.len() - start) as u32;
    let frame_table_off = if hdr.frame_table_off != 0 { tables_start } else { 0 };
    let gcount_table_off = if hdr.gcount_table_off != 0 { tables_start + (hdr.gcount_table_off - pend as u32) } else { 0 };
    out.extend_from_slice(tables);
    let h = &mut out[start..start + STREAM_SECTION_HDR_LEN];
    h[0..4].copy_from_slice(&STREAM_SECTION_MAGIC.to_le_bytes());
    h[4..6].copy_from_slice(&hdr.encoding.to_le_bytes());
    h[6] = hdr.width;
    h[7] = Wrapper::FrontOnPairZstd.as_u8();
    h[8..12].copy_from_slice(&hdr.frame_count.to_le_bytes());
    h[12..16].copy_from_slice(&frame_table_off.to_le_bytes());
    h[16..20].copy_from_slice(&gcount_table_off.to_le_bytes());
    h[20..24].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    h[24..28].copy_from_slice(&hdr.value_count.to_le_bytes());
    h[28..32].copy_from_slice(&0u32.to_le_bytes());
    Ok(())
}

/// Frame block `src` back to its front-coded bytes on `fc`; `span` = the frame's v1 image length (bounds
/// the inflate). `tok` and `text` are scratch.
fn frame_fc(table: &DecodeTable, src: &[u8], records: &[u8], span: usize, tok: &mut Vec<u8>, text: &mut Vec<u8>, fc: &mut Vec<u8>) -> FormatResult<()> {
    let n = records.len() / DICT_INDEX_ENTRY_LEN;
    let cap = 4 + 5 * n + onpair::STREAM_HDR + 2 * span + 4;
    let got = inflate(src, cap, tok)?;
    let blk = &tok[..got];
    let ml = u32::from_le_bytes(blk.get(..4).ok_or(FormatError::Truncated { at: AT })?.try_into().expect("len 4")) as usize;
    let meta = blk.get(4..4 + ml).ok_or(FormatError::Truncated { at: AT })?;
    let stream = &blk[4 + ml..];
    let (_, text_len, _, _) = onpair::stream_header(stream)?;
    if text_len > span {
        return Err(FormatError::Corrupt { at: AT });
    }
    pgrc_format::try_resize(text, text_len, 0, AT)?;
    let used = onpair::decode(table, stream, &mut text[..text_len])?;
    if used != stream.len() {
        return Err(FormatError::Corrupt { at: AT });
    }
    join_fc(meta, &text[..text_len], records, fc)
}

/// A side block (the section's last block) decoded to its token table.
pub fn decode_side(side: &[u8]) -> FormatResult<DecodeTable> {
    let mut tok = Vec::new();
    let n = inflate(side, 2 + 4 * (onpair::MAX_IDS - 256), &mut tok)?;
    let (table, used) = DecodeTable::read(&tok[..n])?;
    if used != n {
        return Err(FormatError::Corrupt { at: "onpair table" });
    }
    Ok(table)
}

/// One frame block into exactly `dst` (the frame's v1 image span), `records` its index records: the
/// block-lazy face, against the section's [`decode_side`] table.
pub fn unwrap_frame_block(table: &DecodeTable, src: &[u8], dst: &mut [u8], records: &[u8]) -> FormatResult<()> {
    let (mut tok, mut text, mut fc) = (Vec::new(), Vec::new(), Vec::new());
    frame_fc(table, src, records, dst.len(), &mut tok, &mut text, &mut fc)?;
    expand_front_coded_frame(&fc, records, dst)
}

/// A wrapped section's frame blocks and decoded table, decoding one frame at a time.
struct Frames<'a> {
    wrapped: &'a [u8],
    offs: Vec<usize>,
    table: DecodeTable,
    tok: Vec<u8>,
    text: Vec<u8>,
}

pub(crate) fn inflate(src: &[u8], cap: usize, buf: &mut Vec<u8>) -> FormatResult<usize> {
    if buf.len() < cap {
        pgrc_format::try_resize(buf, cap, 0, AT)?;
    }
    zstd_decompress_block(src, &mut buf[..cap])
}

impl<'a> Frames<'a> {
    fn open(hdr: &StreamSectionHdr, section: &'a [u8], index: &[u8]) -> FormatResult<(Frames<'a>, &'a [u8])> {
        if Wrapper::from_u8(hdr.wrapper)? != Wrapper::FrontOnPairZstd {
            return Err(FormatError::Corrupt { at: AT });
        }
        dict_payload_len_checked(hdr, index)?;
        let pend = payload_end(hdr, section);
        if pend < STREAM_SECTION_HDR_LEN + 4 || pend > section.len() || hdr.frame_table_off == 0 {
            return Err(FormatError::Bounds { at: "payload region" });
        }
        let wrapped = &section[STREAM_SECTION_HDR_LEN..pend];
        let first = u32::from_le_bytes(wrapped[..4].try_into().expect("len 4")) as usize;
        if first < 8 || first % 4 != 0 || first > wrapped.len() {
            return Err(FormatError::Corrupt { at: "wrapper block table" });
        }
        let nb = first / 4 - 1;
        if nb != hdr.frame_count as usize + 1 {
            return Err(FormatError::Corrupt { at: AT });
        }
        let mut offs = pgrc_format::try_vec(nb + 1, AT)?;
        for i in 0..=nb {
            offs.push(u32::from_le_bytes(wrapped[i * 4..i * 4 + 4].try_into().expect("len 4")) as usize);
        }
        if offs[0] != first || offs.windows(2).any(|w| w[0] > w[1]) || offs[nb] > wrapped.len() {
            return Err(FormatError::Corrupt { at: "wrapper block offsets" });
        }
        let table = decode_side(&wrapped[offs[nb - 1]..offs[nb]])?;
        Ok((Frames { wrapped, offs, table, tok: Vec::new(), text: Vec::new() }, &section[pend..]))
    }

    /// Frame `b`'s front-coded bytes onto `fc`; `span` = its v1 image length (bounds the inflate).
    fn fc(&mut self, b: usize, records: &[u8], span: usize, fc: &mut Vec<u8>) -> FormatResult<()> {
        let src = self.wrapped.get(self.offs[b]..self.offs[b + 1]).ok_or(FormatError::Corrupt { at: AT })?;
        frame_fc(&self.table, src, records, span, &mut self.tok, &mut self.text, fc)
    }
}

/// The whole-section unwrap into the v1 image (`scratch` cleared first); `Ok(false)` when `stop` said
/// stop, `scratch` then empty.
pub fn unwrap_dict_payload(hdr: &StreamSectionHdr, section: &[u8], index: &[u8], scratch: &mut Vec<u8>, stop: &mut dyn FnMut() -> bool) -> FormatResult<bool> {
    let (mut fr, tables) = Frames::open(hdr, section, index)?;
    let img_len = hdr.uncompressed_len as usize;
    let need = STREAM_SECTION_HDR_LEN + img_len + tables.len();
    scratch.clear();
    scratch.try_reserve_exact(need).map_err(|_| FormatError::Bounds { at: "unwrapped section reservation" })?;
    while scratch.len() < need {
        scratch.resize(need.min(scratch.len() + FILL_PIECE), 0);
        if scratch.len() < need && stop() {
            scratch.clear();
            return Ok(false);
        }
    }
    let ft = hdr.frame_table(section)?.unwrap_or_default();
    if ft.len() != hdr.frame_count as usize {
        return Err(FormatError::Corrupt { at: AT });
    }
    let gcount_table_off = match hdr.gcount_table_off {
        0 => 0,
        g => {
            let rel = g.checked_sub(hdr.frame_table_off).ok_or(FormatError::Corrupt { at: "gcount table offset" })?;
            STREAM_SECTION_HDR_LEN as u32 + hdr.uncompressed_len + rel
        }
    };
    let h = &mut scratch[..STREAM_SECTION_HDR_LEN];
    h[0..4].copy_from_slice(&STREAM_SECTION_MAGIC.to_le_bytes());
    h[4..6].copy_from_slice(&hdr.encoding.to_le_bytes());
    h[6] = hdr.width;
    h[7] = Wrapper::None.as_u8();
    h[8..12].copy_from_slice(&hdr.frame_count.to_le_bytes());
    h[12..16].copy_from_slice(&(STREAM_SECTION_HDR_LEN as u32 + hdr.uncompressed_len).to_le_bytes());
    h[16..20].copy_from_slice(&gcount_table_off.to_le_bytes());
    h[20..24].copy_from_slice(&0u32.to_le_bytes());
    h[24..28].copy_from_slice(&hdr.value_count.to_le_bytes());
    h[28..32].copy_from_slice(&0u32.to_le_bytes());
    let mut fc = Vec::new();
    for b in 0..ft.len() {
        if stop() {
            scratch.clear();
            return Ok(false);
        }
        let f0 = ft[b] as usize;
        let f1 = if b + 1 < ft.len() { ft[b + 1] as usize } else { img_len };
        if f1 < f0 || f1 > img_len || (b == 0 && f0 != 0) {
            return Err(FormatError::Corrupt { at: "front-coded block geometry" });
        }
        let recs = dict_frame_records(index, b as u32)?;
        fc.clear();
        fr.fc(b, recs, f1 - f0, &mut fc)?;
        expand_front_coded_frame(&fc, recs, &mut scratch[STREAM_SECTION_HDR_LEN + f0..STREAM_SECTION_HDR_LEN + f1])?;
    }
    let tstart = STREAM_SECTION_HDR_LEN + img_len;
    scratch[tstart..].copy_from_slice(tables);
    Ok(true)
}

/// The front-coded resident form (as `FrontZstd`'s), built from this arm's blocks.
pub fn unwrap_dict_payload_fc(hdr: &StreamSectionHdr, section: &[u8], index: &[u8], scratch: &mut Vec<u8>, stop: &mut dyn FnMut() -> bool) -> FormatResult<bool> {
    let (mut fr, _) = Frames::open(hdr, section, index)?;
    let ft = hdr.frame_table(section)?.unwrap_or_default();
    let img_len = hdr.uncompressed_len as usize;
    let mut body = Vec::new();
    let mut stopped = false;
    let built = pgrc_format::dict::fc_resident_build(index, &mut |b, out| {
        if stop() {
            stopped = true;
            return Err(FormatError::Corrupt { at: "front-coded resident form stopped" });
        }
        let b = b as usize;
        let f0 = *ft.get(b).ok_or(FormatError::Corrupt { at: "front-coded block geometry" })? as usize;
        let f1 = ft.get(b + 1).map_or(img_len, |&f| f as usize);
        if f1 < f0 || f1 > img_len {
            return Err(FormatError::Corrupt { at: "front-coded block geometry" });
        }
        fr.fc(b, dict_frame_records(index, b as u32)?, f1 - f0, out)
    }, &mut body);
    scratch.clear();
    if stopped {
        return Ok(false);
    }
    built?;
    scratch.resize(STREAM_SECTION_HDR_LEN, 0);
    let h = &mut scratch[..STREAM_SECTION_HDR_LEN];
    h[0..4].copy_from_slice(&STREAM_SECTION_MAGIC.to_le_bytes());
    h[4..6].copy_from_slice(&hdr.encoding.to_le_bytes());
    h[6] = hdr.width;
    h[7] = Wrapper::None.as_u8();
    h[8..12].copy_from_slice(&hdr.frame_count.to_le_bytes());
    h[24..28].copy_from_slice(&hdr.value_count.to_le_bytes());
    if scratch.try_reserve_exact(body.len()).is_err() {
        scratch.clear();
        return Err(FormatError::Bounds { at: "front-coded resident form reservation" });
    }
    for (i, piece) in body.chunks(FILL_PIECE).enumerate() {
        if i > 0 && stop() {
            scratch.clear();
            return Ok(false);
        }
        scratch.extend_from_slice(piece);
    }
    Ok(true)
}
