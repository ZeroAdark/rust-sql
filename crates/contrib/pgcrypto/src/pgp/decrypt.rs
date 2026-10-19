
use super::cfb::PgpCfb;
use super::consts::*;
use super::context::PgpContext;
use super::packet::{read_fixed, Body, HdrErr, PktHdr, PktReader, EOF_DBG};
use super::s2k::S2k;

pub struct SessKey {
    pub cipher: i32,
    pub key: Vec<u8>,
}

type PubencParser<'a> = dyn FnMut(&mut PgpContext, &[u8]) -> Result<SessKey, String> + 'a;

pub fn decrypt_symmetric(
    ctx: &mut PgpContext,
    data: &[u8],
    passphrase: &[u8],
) -> Result<Vec<u8>, String> {
    decrypt_message(ctx, data, passphrase, None)
}

pub fn decrypt_pubkey(
    ctx: &mut PgpContext,
    data: &[u8],
    pubenc: &mut PubencParser,
) -> Result<Vec<u8>, String> {
    decrypt_message(ctx, data, &[], Some(pubenc))
}

fn hdr_failure(ctx: &mut PgpContext, why: HdrErr) -> String {
    if let Some(why) = why {
        ctx.dbg(why);
    }
    CORRUPT_DATA.to_string()
}

/// pgp-decrypt.c:1096 pgp_decrypt: the session-key parser follows the
/// packet tag (a public-key packet without a key is PXE_BUG, a symmetric
/// one in pgp_pub_decrypt runs S2K over an empty key), every packet after
/// the first key/data one is diagnosed, and the walk goes on past the data
/// packet so trailing bytes are seen.
fn decrypt_message(
    ctx: &mut PgpContext,
    data: &[u8],
    passphrase: &[u8],
    mut pubenc: Option<&mut PubencParser>,
) -> Result<Vec<u8>, String> {
    let mut rdr = PktReader::new(data);
    let mut sess: Option<SessKey> = None;
    let mut got_data = false;
    let mut out = Vec::new();

    loop {
        let hdr = match rdr.read_hdr() {
            Ok(None) => break,
            Ok(Some(h)) => h,
            Err(why) => return Err(hdr_failure(ctx, why)),
        };
        match hdr.tag {
            t if t == PGP_PKT_MARKER => {
                rdr.read_body_avail(&hdr).map_err(|w| hdr_failure(ctx, w))?;
            }
            t if t == PGP_PKT_PUBENC_SESSKEY => {
                let body = rdr.read_body_avail(&hdr).map_err(|w| hdr_failure(ctx, w))?;
                let Some(parse) = pubenc.as_mut() else {
                    ctx.dbg("no pubkey?");
                    return Err(PGCRYPTO_BUG.to_string());
                };
                sess = Some(parse(ctx, &body.data)?);
            }
            t if t == PGP_PKT_SYMENC_SESSKEY => {
                if sess.is_some() {
                    ctx.dbg("pgp_decrypt: using first of several keys");
                    return Err(CORRUPT_DATA.to_string());
                }
                let body = rdr.read_body_avail(&hdr).map_err(|w| hdr_failure(ctx, w))?;
                sess = Some(parse_symenc_sesskey(ctx, &body.data, passphrase)?);
            }
            t if t == PGP_PKT_SYMENC_DATA || t == PGP_PKT_SYMENC_DATA_MDC => {
                let mdc = t == PGP_PKT_SYMENC_DATA_MDC;
                let Some(sk) = sess.as_ref() else {
                    ctx.dbg("pgp_decrypt: have data but no key");
                    return Err(CORRUPT_DATA.to_string());
                };
                if got_data {
                    ctx.dbg(if mdc {
                        "pgp_decrypt: several data pkts not supported"
                    } else {
                        "pgp_decrypt: got second data packet"
                    });
                    return Err(CORRUPT_DATA.to_string());
                }
                got_data = true;
                ctx.disable_mdc = if mdc { 0 } else { 1 };
                let body = rdr.read_body_avail(&hdr).map_err(|w| hdr_failure(ctx, w))?;
                decrypt_data_packet(ctx, sk, &body, mdc, &mut out)?;
            }
            t => {
                ctx.dbg(&format!("pgp_decrypt: unknown tag: 0x{t:02x}"));
                return Err(CORRUPT_DATA.to_string());
            }
        }
    }

    if !got_data || ctx.corrupt_prefix {
        return Err(CORRUPT_DATA.to_string());
    }
    if ctx.unsupported_compr {
        return Err(UNSUPPORTED_COMPR.to_string());
    }
    if ctx.unexpected_binary {
        return Err(NOT_TEXT.to_string());
    }
    Ok(out)
}

fn parse_symenc_sesskey(
    ctx: &mut PgpContext,
    body: &[u8],
    passphrase: &[u8],
) -> Result<SessKey, String> {
    // pgp-decrypt.c:634: both bytes are read before the version check.
    let ver = read_fixed(body, 0, 1, ctx)?[0];
    let s2k_cipher = read_fixed(body, 1, 1, ctx)?[0] as i32;
    if ver != 4 {
        ctx.dbg("bad key pkt ver");
        return Err(CORRUPT_DATA.to_string());
    }
    let (mut s2k, consumed) = S2k::read(&body[2..], ctx).map_err(|e| e.to_string())?;
    s2k.process(s2k_cipher, passphrase).map_err(|e| e.to_string())?;

    ctx.s2k_mode = s2k.mode;
    // pgp-decrypt.c:647: decoded unconditionally — iter is 0 for the simple
    // and salted modes, so expect-s2k-count sees 1024 there, as in C.
    ctx.s2k_count = s2k_decode_count(s2k.iter as i32);
    ctx.s2k_digest_algo = s2k.digest_algo;
    ctx.s2k_cipher_algo = s2k_cipher;

    let rest = &body[2 + consumed..];
    if rest.is_empty() {
        ctx.use_sess_key = 0;
        ctx.cipher_algo = s2k_cipher;
        Ok(SessKey {
            cipher: s2k_cipher,
            key: s2k.key,
        })
    } else {
        // pgp-decrypt.c:681: 17 <= len <= PGP_MAX_KEY + 1, else corrupt.
        if rest.len() < 17 || rest.len() > PGP_MAX_KEY + 1 {
            ctx.dbg("expect key, but bad data");
            return Err(CORRUPT_DATA.to_string());
        }
        ctx.use_sess_key = 1;
        // upstream 4c5128ca0b30 (18.6): pgcrypto: Add option to revert to prior decryption behavior
        let ignore = ctx.ignore_cipher_failure != 0;
        let mut cfb = PgpCfb::create(s2k_cipher, &s2k.key, false, None, ignore)
            .map_err(|e| e.to_string())?;
        let dec = cfb.decrypt(rest);
        let cipher = dec[0] as i32;
        ctx.cipher_algo = cipher;
        // pgp-decrypt.c:612 decrypt_key: the key length must be EXACTLY the
        // cipher's key size (trailing bytes are not padding).
        let klen = dec.len() - 1;
        if cipher_key_size(cipher) != klen {
            ctx.dbg(&format!(
                "sesskey bad len: algo={cipher}, expected={}, got={klen}",
                cipher_key_size(cipher)
            ));
            return Err(CORRUPT_DATA.to_string());
        }
        Ok(SessKey {
            cipher,
            key: dec[1..].to_vec(),
        })
    }
}

/// pgp-decrypt.c:1015 parse_symenc_mdc_data / :971 parse_symenc_data: the
/// filter stack is pkt -> decrypt -> [mdc] -> prefix -> process_data_packets.
fn decrypt_data_packet(
    ctx: &mut PgpContext,
    sk: &SessKey,
    body: &Body,
    mdc: bool,
    dst: &mut Vec<u8>,
) -> Result<(), String> {
    let mut ct: &[u8] = &body.data;
    let mut ct_off = 0usize;
    if mdc {
        let Some(&ver) = ct.first() else {
            ctx.dbg(EOF_DBG);
            return Err(CORRUPT_DATA.to_string());
        };
        if ver != 1 {
            ctx.dbg("parse_symenc_mdc_data: pkt ver != 1");
            return Err(CORRUPT_DATA.to_string());
        }
        ct = &ct[1..];
        ct_off = 1;
    }

    // upstream 4c5128ca0b30 (18.6): pgcrypto: Add option to revert to prior decryption behavior
    let ignore = ctx.ignore_cipher_failure != 0;
    let mut cfb = PgpCfb::create(sk.cipher, &sk.key, !mdc, None, ignore)
        .map_err(|e| e.to_string())?;
    let plain = cfb.decrypt(ct);
    let bs = cipher_block_size(sk.cipher);

    // prefix_init (pgp-decrypt.c:258); under the MDC filter the short read
    // is its EOF (pgp-decrypt.c:409 mdc_read).
    if plain.len() < bs + 2 {
        ctx.dbg(if mdc { MDC_EOF } else { "prefix_init: short read" });
        return Err(CORRUPT_DATA.to_string());
    }
    if plain[bs - 2] != plain[bs] || plain[bs - 1] != plain[bs + 1] {
        ctx.dbg("prefix_init: corrupt prefix");
        ctx.corrupt_prefix = true;
    }
    let mut md = None;
    if mdc {
        let mut d = Digest::new(PGP_DIGEST_SHA1).ok_or(UNSUPPORTED_HASH.to_string())?;
        d.update(&plain[..bs + 2]);
        md = Some(d);
    }
    // Chunk ends of the encrypted packet, in decrypted-stream coordinates:
    // the outer packet reader never hands out bytes across one.
    let skip = ct_off + bs + 2;
    let mut outer_ends = Vec::new();
    let mut end = 0usize;
    for &(_, len) in &body.chunks {
        end += len;
        if end > skip {
            outer_ends.push(end - skip);
        }
    }
    process_data_packets(ctx, &plain[bs + 2..], &outer_ends, true, md.as_mut(), dst)
}

const MDC_EOF: &str = "mdc_read: unexpected eof";
const MDC_TAIL: usize = 2 + MDC_DIGEST_LEN;

/// A GETBYTE at end of input below an open MDC filter is that filter's EOF.
fn eof_error(ctx: &mut PgpContext, why: HdrErr, mdc_open: bool) -> String {
    if why == Some(EOF_DBG) && mdc_open {
        return hdr_failure(ctx, Some(MDC_EOF));
    }
    hdr_failure(ctx, why)
}

/// pgp-decrypt.c:873 process_data_packets: every literal packet appends to
/// `dst`; a compressed packet must be the only data packet and is legal only
/// at the top level. With `md` (NEED_MDC) every byte read is hashed and the
/// MDC packet closes the stream (pgp-decrypt.c:341 mdc_finish); an
/// indeterminate-length packet inside it takes the mdcbuf filter
/// (pgp-decrypt.c:432), whose trailer is the last 22 bytes.
fn process_data_packets(
    ctx: &mut PgpContext,
    data: &[u8],
    outer_ends: &[usize],
    allow_compr: bool,
    mut md: Option<&mut Digest>,
    dst: &mut Vec<u8>,
) -> Result<(), String> {
    let need_mdc = md.is_some();
    let mut rdr = PktReader::new_allow_ctx(data);
    let mut got_data = false;
    let mut got_mdc = false;
    let mut mdcbuf = false;

    loop {
        let hdr_start = rdr.pos;
        let hdr = match rdr.read_hdr() {
            Ok(Some(h)) => h,
            Ok(None) => {
                if need_mdc && !got_mdc && !mdcbuf {
                    ctx.dbg(MDC_EOF);
                    return Err(CORRUPT_DATA.to_string());
                }
                break;
            }
            Err(why) => return Err(eof_error(ctx, why, need_mdc && !got_mdc && !mdcbuf)),
        };
        if got_mdc {
            ctx.dbg("process_data_packets: data after mdc");
            return Err(CORRUPT_DATA.to_string());
        }
        let hdr_end = rdr.pos;

        if need_mdc && hdr.len.is_none() {
            mdcbuf = true;
            let d = md.as_deref_mut().expect("need_mdc");
            d.update(&data[hdr_start..hdr_end]);
            let rest = &data[hdr_end..];
            rdr.pos = data.len();
            let (payload, tail) = rest.split_at(rest.len().saturating_sub(MDC_TAIL));
            let mut mdc_buf = [0u8; MDC_TAIL];
            mdc_buf[..tail.len()].copy_from_slice(tail);
            d.update(payload);
            let body = Body { data: payload.to_vec(), chunks: vec![(hdr_end, payload.len())], truncated: false };
            if hdr.tag == PGP_PKT_MDC {
                return Err(PGCRYPTO_BUG.to_string());
            }
            data_packet(ctx, &hdr, &body, &[], allow_compr, false, &mut got_data, dst)?;
            if mdc_buf[0] != 0xD3 || mdc_buf[1] != MDC_DIGEST_LEN as u8 {
                ctx.dbg("mdcbuf_finish: bad MDC pkt hdr");
                return Err(CORRUPT_DATA.to_string());
            }
            d.update(&mdc_buf[..2]);
            if d.finish() != mdc_buf[2..] {
                ctx.dbg("mdcbuf_finish: MDC does not match");
                return Err(CORRUPT_DATA.to_string());
            }
            continue;
        }

        let body = match rdr.read_body_avail(&hdr) {
            Ok(b) => b,
            Err(why) => return Err(eof_error(ctx, why, need_mdc && !got_mdc && !mdcbuf)),
        };
        if let Some(d) = md.as_deref_mut() {
            let hashed_end = if hdr.tag == PGP_PKT_MDC { hdr_end } else { rdr.pos };
            d.update(&data[hdr_start..hashed_end]);
        }
        if hdr.tag == PGP_PKT_MDC {
            if !need_mdc {
                ctx.dbg("process_data_packets: unexpected MDC");
                return Err(CORRUPT_DATA.to_string());
            }
            if hdr.len != Some(MDC_DIGEST_LEN) {
                return Err(CORRUPT_DATA.to_string());
            }
            if body.data.is_empty() {
                ctx.dbg("no mdc");
                return Err(CORRUPT_DATA.to_string());
            }
            if body.data.len() != MDC_DIGEST_LEN {
                ctx.dbg(&format!("mdc_finish: read failed, res={}", body.data.len()));
                return Err(CORRUPT_DATA.to_string());
            }
            if md.as_deref_mut().expect("need_mdc").finish() != body.data {
                ctx.dbg("mdc_finish: mdc failed");
                return Err(CORRUPT_DATA.to_string());
            }
            got_mdc = true;
            continue;
        }
        data_packet(ctx, &hdr, &body, outer_ends, allow_compr, need_mdc, &mut got_data, dst)?;
    }

    if !got_data {
        ctx.dbg("process_data_packets: no data");
        return Err(CORRUPT_DATA.to_string());
    }
    if need_mdc && !got_mdc && !mdcbuf {
        ctx.dbg("process_data_packets: got no mdc");
        return Err(CORRUPT_DATA.to_string());
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn data_packet(
    ctx: &mut PgpContext,
    hdr: &PktHdr,
    body: &Body,
    outer_ends: &[usize],
    allow_compr: bool,
    need_mdc: bool,
    got_data: &mut bool,
    dst: &mut Vec<u8>,
) -> Result<(), String> {
    // A read past a truncated body reaches the source's EOF, which the MDC
    // filter refuses (pgp-decrypt.c:409); an exhausted declared length is
    // the packet reader's own short read.
    let src_eof = need_mdc && body.truncated;
    match hdr.tag {
        t if t == PGP_PKT_LITERAL_DATA => {
            *got_data = true;
            parse_literal_data(ctx, body, src_eof, dst)
        }
        t if t == PGP_PKT_COMPRESSED_DATA => {
            if !allow_compr {
                ctx.dbg("process_data_packets: unexpected compression");
                return Err(CORRUPT_DATA.to_string());
            }
            if *got_data {
                ctx.dbg("process_data_packets: only one cmpr pkt allowed");
                return Err(CORRUPT_DATA.to_string());
            }
            *got_data = true;
            parse_compressed_data(ctx, body, outer_ends, src_eof, dst)
        }
        t => {
            ctx.dbg(&format!("process_data_packets: unexpected pkt tag={t}"));
            Err(CORRUPT_DATA.to_string())
        }
    }
}

/// pgp-decrypt.c:816 parse_compressed_data.
fn parse_compressed_data(
    ctx: &mut PgpContext,
    body: &Body,
    outer_ends: &[usize],
    src_eof: bool,
    dst: &mut Vec<u8>,
) -> Result<(), String> {
    let Some(&algo) = body.data.first() else {
        ctx.dbg(if src_eof { MDC_EOF } else { EOF_DBG });
        return Err(CORRUPT_DATA.to_string());
    };
    let algo = algo as i32;
    ctx.compress_algo = algo;
    let inner = &body.data[1..];
    let decompressed = match algo {
        PGP_COMPR_NONE => inner.to_vec(),
        PGP_COMPR_ZIP | PGP_COMPR_ZLIB => {
            let res = if algo == PGP_COMPR_ZIP {
                super::compress::inflate_raw(inner)
            } else {
                super::compress::inflate_zlib(inner)
            };
            let (out, consumed) = match res {
                Ok(r) => r,
                Err(None) => {
                    mbuf_room(ctx, usize::MAX)?;
                    return Err(CORRUPT_DATA.to_string());
                }
                Err(e) => return Err(inflate_error(ctx, e)),
            };
            if src_eof {
                ctx.dbg(MDC_EOF);
                return Err(CORRUPT_DATA.to_string());
            }
            // pgp-compress.c:298: after Z_STREAM_END one more read of the
            // source must return nothing. The decompressor pulls its input
            // in reads of at most 8192 (ZIP_IN_BLOCK) that the decrypt
            // filter's 4096-byte buffer, the compressed packet's chunks and
            // the encrypted packet's chunks all cut short, so bytes are
            // "extra" only past the read in which the stream ended.
            let mut ends: Vec<usize> = Vec::new();
            let mut flat = 0usize;
            for &(start, len) in &body.chunks {
                for &e in outer_ends {
                    if e > start && e < start + len {
                        ends.push(flat + (e - start));
                    }
                }
                flat += len;
                ends.push(flat);
            }
            ends.sort_unstable();
            let mut pos = 1usize;
            let mut read_end = 1usize;
            while pos < body.data.len() {
                let next = ends.iter().copied().find(|&e| e > pos).unwrap_or(body.data.len());
                read_end = next.min(pos + 4096);
                if read_end >= 1 + consumed {
                    break;
                }
                pos = read_end;
            }
            if read_end < body.data.len() {
                ctx.dbg("decompress_read: extra bytes after end of stream");
                return Err(CORRUPT_DATA.to_string());
            }
            out
        }
        PGP_COMPR_BZIP2 => {
            ctx.dbg("parse_compressed_data: bzip2 unsupported");
            // report error in pgp_decrypt()
            ctx.unsupported_compr = true;
            return Ok(());
        }
        _ => {
            ctx.dbg("parse_compressed_data: unknown compr type");
            return Err(CORRUPT_DATA.to_string());
        }
    };
    process_data_packets(ctx, &decompressed, &[], false, None, dst)
}

// pgp-compress.c:281 decompress_read: zlib's return code goes to px_debug.
fn inflate_error(ctx: &mut PgpContext, e: Option<i32>) -> String {
    if let Some(code) = e {
        ctx.dbg(&format!("decompress_read: inflate error: {code}"));
    }
    CORRUPT_DATA.to_string()
}

/// mbuf.c:79 prepare_room: the output MBuf grows by 2*STEP per short
/// append until it holds `need` bytes; the growth that would pass
/// MaxAllocSize is mcxt.c:1180's error, carried as its message text.
pub(crate) fn mbuf_room(ctx: &mut PgpContext, need: usize) -> Result<(), String> {
    const STEP: usize = 16 * 1024;
    let mut cap = ctx.mbuf_cap;
    while cap < need {
        cap = cap.saturating_add(2 * STEP);
        if cap > ::mcx::MAX_ALLOC_SIZE {
            return Err(format!("{ALLOC_SIZE_ERROR} {cap}"));
        }
    }
    ctx.mbuf_cap = cap;
    Ok(())
}

/// mcxt.c:1180 MemoryContextSizeFailure's message, an XX000 at the SQL
/// wrapper (lib.rs px_msg).
pub const ALLOC_SIZE_ERROR: &str = "invalid memory alloc request size";

/// pgp-decrypt.c:745 parse_literal_data. CRLF conversion follows the SQL
/// caller's text mode and convert-crlf, not the literal packet's own type.
fn parse_literal_data(
    ctx: &mut PgpContext,
    body: &Body,
    src_eof: bool,
    dst: &mut Vec<u8>,
) -> Result<(), String> {
    let data = &body.data;
    let short = |ctx: &mut PgpContext, why: &str| {
        ctx.dbg(if src_eof { MDC_EOF } else { why });
        CORRUPT_DATA.to_string()
    };
    if data.len() < 2 {
        return Err(short(ctx, EOF_DBG));
    }
    let ty = data[0];
    let namelen = data[1] as usize;
    let off = 2 + namelen + 4;
    if data.len() < off {
        return Err(short(ctx, "parse_literal_data: unexpected eof"));
    }
    if ctx.text_mode != 0 && ty != b't' && ty != b'u' {
        ctx.dbg(&format!("parse_literal_data: data type={}", ty as char));
        ctx.unexpected_binary = true;
    }
    ctx.unicode_mode = if ty == b'u' { 1 } else { 0 };
    let payload = &data[off..];
    let payload = if ctx.text_mode != 0 && ctx.convert_crlf != 0 {
        un_convert_crlf(payload)
    } else {
        payload.to_vec()
    };
    mbuf_room(ctx, literal_room(dst.len(), payload.len()))?;
    dst.extend_from_slice(&payload);
    if src_eof {
        ctx.dbg(MDC_EOF);
        return Err(CORRUPT_DATA.to_string());
    }
    Ok(())
}

/// pgp-pgsql.c:484: the output MBuf already holds the VARHDRSZ bytes
/// reserved for the result header when the literal payload is appended.
fn literal_room(dst_len: usize, payload_len: usize) -> usize {
    4 + dst_len + payload_len
}

fn un_convert_crlf(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if data[i] == b'\r' && i + 1 < data.len() && data[i + 1] == b'\n' {
            out.push(b'\n');
            i += 2;
        } else {
            out.push(data[i]);
            i += 1;
        }
    }
    out
}

use super::consts::Digest;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_room_counts_the_reserved_varlena_header() {
        let mut ctx = PgpContext::default();
        ctx.mbuf_cap = 1073741823;
        mbuf_room(&mut ctx, literal_room(1073741809, 10)).unwrap();
        assert_eq!(
            mbuf_room(&mut ctx, literal_room(1073741813, 10)).unwrap_err(),
            "invalid memory alloc request size 1073774591"
        );
    }

    // pgp-compress.c:281: the zlib code is px_debug'd before
    // PXE_PGP_CORRUPT_DATA.
    #[test]
    fn inflate_failure_is_debugged_with_zlib_code() {
        let mut ctx = PgpContext::default();
        ctx.debug = 1;
        let mut out = Vec::new();
        let pkt = |data: &[u8]| Body { data: data.to_vec(), chunks: vec![(0, data.len())], truncated: false };
        let err = parse_compressed_data(&mut ctx, &pkt(&[PGP_COMPR_ZIP as u8, 0xff, 0xff, 0xff]), &[], false, &mut out)
            .unwrap_err();
        assert_eq!(err, CORRUPT_DATA);
        assert_eq!(ctx.debug_notices, ["dbg: decompress_read: inflate error: -3"]);
        let comp = super::super::compress::deflate_zlib(b"truncated compressed literal", 6);
        let mut body = vec![PGP_COMPR_ZLIB as u8];
        body.extend_from_slice(&comp[..comp.len() / 2]);
        ctx.debug_notices.clear();
        parse_compressed_data(&mut ctx, &pkt(&body), &[], false, &mut out).unwrap_err();
        assert_eq!(ctx.debug_notices, ["dbg: decompress_read: inflate error: -5"]);
    }
}
