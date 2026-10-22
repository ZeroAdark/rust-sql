//! pgp-decrypt.c: the message is read through a chain of pull filters
//! (packet reader -> decrypt -> [mdc] -> prefix -> packet reader or mdcbuf
//! -> [decompress -> packet reader]) that each fetch bytes only when the
//! parser above asks for them.

use super::cfb::PgpCfb;
use super::compress::{Inflater, Z_OK, Z_STREAM_END};
use super::consts::*;
use super::context::PgpContext;
use super::packet::HdrErr;
use super::pull::{
    getbyte, parse_pkt_hdr, read_max, skip_packet, Dcx, PRes, PktHdr, PktPull, PktType, Pull,
    SliceReader,
};
use super::s2k::S2k;

pub struct SessKey {
    pub cipher: i32,
    pub key: Vec<u8>,
}

type PubencParser<'a> = dyn FnMut(&mut Dcx, &mut dyn Pull) -> PRes<SessKey> + 'a;

pub fn decrypt_symmetric(ctx: &mut PgpContext, data: &[u8], passphrase: &[u8]) -> PRes<Vec<u8>> {
    decrypt_message(ctx, data, passphrase, None)
}

pub fn decrypt_pubkey(ctx: &mut PgpContext, data: &[u8], pubenc: &mut PubencParser) -> PRes<Vec<u8>> {
    decrypt_message(ctx, data, &[], Some(pubenc))
}

pub(crate) fn hdr_failure(ctx: &mut PgpContext, why: HdrErr) -> String {
    if let Some(why) = why {
        ctx.dbg(why);
    }
    CORRUPT_DATA.to_string()
}

/// pgp-decrypt.c:1096 pgp_decrypt.
fn decrypt_message(
    ctx: &mut PgpContext,
    data: &[u8],
    passphrase: &[u8],
    mut pubenc: Option<&mut PubencParser>,
) -> PRes<Vec<u8>> {
    let mut cx = Dcx::new(ctx);
    let cx = &mut cx;
    let mut src = SliceReader::new(data);
    let mut sess: Option<SessKey> = None;
    let mut got_data = false;
    let mut out = Vec::new();

    while let Some(hdr) = parse_pkt_hdr(&mut src, cx, false)? {
        let mut pkt = PktPull::new(&mut src, &hdr);
        match hdr.tag {
            PGP_PKT_MARKER => skip_packet(&mut pkt, cx)?,
            PGP_PKT_PUBENC_SESSKEY => {
                let Some(parse) = pubenc.as_mut() else {
                    cx.dbg("no pubkey?");
                    return Err(PGCRYPTO_BUG.to_string());
                };
                sess = Some(parse(cx, &mut pkt)?);
            }
            PGP_PKT_SYMENC_SESSKEY => {
                if sess.is_some() {
                    return Err(cx.corrupt("pgp_decrypt: using first of several keys"));
                }
                sess = Some(parse_symenc_sesskey(cx, &mut pkt, passphrase)?);
            }
            PGP_PKT_SYMENC_DATA | PGP_PKT_SYMENC_DATA_MDC => {
                let mdc = hdr.tag == PGP_PKT_SYMENC_DATA_MDC;
                let Some(sk) = sess.as_ref() else {
                    return Err(cx.corrupt("pgp_decrypt: have data but no key"));
                };
                if got_data {
                    return Err(cx.corrupt(if mdc {
                        "pgp_decrypt: several data pkts not supported"
                    } else {
                        "pgp_decrypt: got second data packet"
                    }));
                }
                got_data = true;
                cx.ctx.disable_mdc = if mdc { 0 } else { 1 };
                if mdc {
                    parse_symenc_mdc_data(cx, &mut pkt, sk, &mut out)?;
                } else {
                    parse_symenc_data(cx, &mut pkt, sk, &mut out)?;
                }
            }
            t => return Err(cx.corrupt(&format!("pgp_decrypt: unknown tag: 0x{t:02x}"))),
        }
    }

    let ctx = &mut *cx.ctx;
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

/// pgp-decrypt.c:626 parse_symenc_sesskey.
fn parse_symenc_sesskey(cx: &mut Dcx, pkt: &mut dyn Pull, passphrase: &[u8]) -> PRes<SessKey> {
    let ver = getbyte(pkt, cx)?;
    let s2k_cipher = getbyte(pkt, cx)? as i32;
    cx.ctx.s2k_cipher_algo = s2k_cipher;
    if ver != 4 {
        return Err(cx.corrupt("bad key pkt ver"));
    }
    let mut s2k = S2k::read_pull(pkt, cx)?;
    let ctx = &mut *cx.ctx;
    ctx.s2k_mode = s2k.mode;
    ctx.s2k_count = s2k_decode_count(s2k.iter as i32);
    ctx.s2k_digest_algo = s2k.digest_algo;
    s2k.process(s2k_cipher, passphrase).map_err(|e| e.to_string())?;

    let rest = read_max(pkt, cx, PGP_MAX_KEY + 2)?;
    let ctx = &mut *cx.ctx;
    if rest.is_empty() {
        ctx.use_sess_key = 0;
        ctx.cipher_algo = s2k_cipher;
        return Ok(SessKey { cipher: s2k_cipher, key: s2k.key });
    }
    if rest.len() < 17 || rest.len() > PGP_MAX_KEY + 1 {
        return Err(cx.corrupt("expect key, but bad data"));
    }
    ctx.use_sess_key = 1;
    decrypt_key(ctx, &s2k, s2k_cipher, &rest)
}

/// pgp-decrypt.c:593 decrypt_key.
fn decrypt_key(ctx: &mut PgpContext, s2k: &S2k, s2k_cipher: i32, src: &[u8]) -> PRes<SessKey> {
    let ignore = ctx.ignore_cipher_failure != 0;
    let mut cfb = PgpCfb::create(s2k_cipher, &s2k.key, false, None, ignore).map_err(|e| e.to_string())?;
    let dec = cfb.decrypt(src);
    let cipher = dec[0] as i32;
    ctx.cipher_algo = cipher;
    let klen = dec.len() - 1;
    if cipher_key_size(cipher) != klen {
        ctx.dbg(&format!(
            "sesskey bad len: algo={cipher}, expected={}, got={klen}",
            cipher_key_size(cipher)
        ));
        return Err(CORRUPT_DATA.to_string());
    }
    Ok(SessKey { cipher, key: dec[1..].to_vec() })
}

/// pgp-decrypt.c:223 prefix_init: the random prefix and its two-byte
/// quick check; a mismatch is reported only at the end (pgp_decrypt).
fn prefix_init(src: &mut dyn Pull, cx: &mut Dcx, cipher: i32) -> PRes<()> {
    let len = cipher_block_size(cipher);
    if len > PGP_MAX_BLOCK {
        return Err(PGCRYPTO_BUG.to_string());
    }
    let buf = read_max(src, cx, len + 2)?;
    if buf.len() != len + 2 {
        return Err(cx.corrupt("prefix_init: short read"));
    }
    if buf[len - 2] != buf[len] || buf[len - 1] != buf[len + 1] {
        cx.dbg("prefix_init: corrupt prefix");
        cx.ctx.corrupt_prefix = true;
    }
    Ok(())
}

/// pgp-decrypt.c:283 decrypt filter; its 4096-byte buffer caps each read.
struct DecryptPull<'a> {
    src: &'a mut dyn Pull,
    cfb: PgpCfb,
    buf: Vec<u8>,
}

const DECRYPT_BUF: usize = 4096;

impl Pull for DecryptPull<'_> {
    fn pull(&mut self, cx: &mut Dcx, len: usize) -> PRes<&[u8]> {
        let d = self.src.pull(cx, len.min(DECRYPT_BUF))?;
        let n = d.len();
        self.cfb.decrypt_into(d, &mut self.buf[..n]);
        Ok(&self.buf[..n])
    }
}

/// pgp-decrypt.c:403 mdc_read: hashes everything it passes up; EOF before
/// the MDC packet is corrupt data.
struct MdcPull<'a> {
    src: &'a mut dyn Pull,
}

impl Pull for MdcPull<'_> {
    fn pull(&mut self, cx: &mut Dcx, len: usize) -> PRes<&[u8]> {
        if cx.use_mdcbuf || cx.in_mdc_pkt {
            return self.src.pull(cx, len);
        }
        let d = self.src.pull(cx, len)?;
        if d.is_empty() {
            return Err(cx.corrupt("mdc_read: unexpected eof"));
        }
        if let Some(md) = cx.mdc.as_mut() {
            md.update(d);
        }
        Ok(d)
    }
}

/// pgp-decrypt.c:432 mdcbuf filter: a context-length packet under MDC ends
/// 22 bytes before its source does, so the last 22 bytes read are held
/// back as the MDC packet.
struct MdcBuf<'a> {
    src: &'a mut dyn Pull,
    eof: bool,
    buf: Vec<u8>,
    pos: usize,
    avail: usize,
    mdc_buf: [u8; MDC_TAIL],
    mdc_avail: usize,
}

const MDCBUF_LEN: usize = 8192;
const MDC_TAIL: usize = 2 + MDC_DIGEST_LEN;

impl<'a> MdcBuf<'a> {
    fn new(src: &'a mut dyn Pull, cx: &mut Dcx) -> MdcBuf<'a> {
        cx.use_mdcbuf = true;
        MdcBuf {
            src,
            eof: false,
            buf: vec![0u8; MDCBUF_LEN + MDC_TAIL],
            pos: 0,
            avail: 0,
            mdc_buf: [0; MDC_TAIL],
            mdc_avail: 0,
        }
    }

    /// pgp-decrypt.c:463 mdcbuf_finish.
    fn finish(&mut self, cx: &mut Dcx) -> PRes<()> {
        self.eof = true;
        if self.mdc_buf[0] != 0xD3 || self.mdc_buf[1] != MDC_DIGEST_LEN as u8 {
            return Err(cx.corrupt("mdcbuf_finish: bad MDC pkt hdr"));
        }
        let md = cx.mdc.as_mut().ok_or_else(|| PGCRYPTO_BUG.to_string())?;
        md.update(&self.mdc_buf[..2]);
        if md.finish() != self.mdc_buf[2..] {
            return Err(cx.corrupt("mdcbuf_finish: MDC does not match"));
        }
        Ok(())
    }

    /// pgp-decrypt.c:505 mdcbuf_refill.
    fn refill(&mut self, cx: &mut Dcx) -> PRes<()> {
        if self.avail > 0 && self.pos != 0 {
            self.buf.copy_within(self.pos..self.pos + self.avail, 0);
        }
        self.pos = 0;
        let need = MDCBUF_LEN + MDC_TAIL - self.avail - self.mdc_avail;
        let data = self.src.pull(cx, need)?;
        let res = data.len();
        if res == 0 {
            return self.finish(cx);
        }
        let md = cx.mdc.as_mut().ok_or_else(|| PGCRYPTO_BUG.to_string())?;
        let mut load = |buf: &mut Vec<u8>, at: &mut usize, bytes: &[u8]| {
            buf[*at..*at + bytes.len()].copy_from_slice(bytes);
            md.update(bytes);
            *at += bytes.len();
        };
        if res >= MDC_TAIL {
            load(&mut self.buf, &mut self.avail, &self.mdc_buf[..self.mdc_avail]);
            load(&mut self.buf, &mut self.avail, &data[..res - MDC_TAIL]);
            self.mdc_buf.copy_from_slice(&data[res - MDC_TAIL..]);
            self.mdc_avail = MDC_TAIL;
        } else {
            let canmove = (self.mdc_avail + res).saturating_sub(MDC_TAIL);
            if canmove > 0 {
                load(&mut self.buf, &mut self.avail, &self.mdc_buf[..canmove]);
                self.mdc_buf.copy_within(canmove..self.mdc_avail, 0);
                self.mdc_avail -= canmove;
            }
            self.mdc_buf[self.mdc_avail..self.mdc_avail + res].copy_from_slice(data);
            self.mdc_avail += res;
        }
        Ok(())
    }
}

impl Pull for MdcBuf<'_> {
    fn pull(&mut self, cx: &mut Dcx, len: usize) -> PRes<&[u8]> {
        if !self.eof && len > self.avail {
            self.refill(cx)?;
        }
        let n = len.min(self.avail);
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        self.avail -= n;
        Ok(out)
    }
}

/// pgp-decrypt.c:971 parse_symenc_data.
fn parse_symenc_data(cx: &mut Dcx, pkt: &mut dyn Pull, sk: &SessKey, dst: &mut Vec<u8>) -> PRes<()> {
    let ignore = cx.ctx.ignore_cipher_failure != 0;
    let cfb = PgpCfb::create(sk.cipher, &sk.key, true, None, ignore).map_err(|e| e.to_string())?;
    let mut dec = DecryptPull { src: pkt, cfb, buf: vec![0u8; DECRYPT_BUF] };
    prefix_init(&mut dec, cx, sk.cipher)?;
    process_data_packets(cx, dst, &mut dec, true, false)
}

/// pgp-decrypt.c:1015 parse_symenc_mdc_data.
fn parse_symenc_mdc_data(cx: &mut Dcx, pkt: &mut dyn Pull, sk: &SessKey, dst: &mut Vec<u8>) -> PRes<()> {
    let ver = getbyte(pkt, cx)?;
    if ver != 1 {
        return Err(cx.corrupt("parse_symenc_mdc_data: pkt ver != 1"));
    }
    let ignore = cx.ctx.ignore_cipher_failure != 0;
    let cfb = PgpCfb::create(sk.cipher, &sk.key, false, None, ignore).map_err(|e| e.to_string())?;
    let mut dec = DecryptPull { src: pkt, cfb, buf: vec![0u8; DECRYPT_BUF] };
    cx.mdc = Some(Digest::load(PGP_DIGEST_SHA1)?);
    let mut mdc = MdcPull { src: &mut dec };
    prefix_init(&mut mdc, cx, sk.cipher)?;
    process_data_packets(cx, dst, &mut mdc, true, true)
}

enum DataPkt<'a> {
    Pkt(PktPull<'a>),
    MdcBuf(MdcBuf<'a>),
}

impl Pull for DataPkt<'_> {
    fn pull(&mut self, cx: &mut Dcx, len: usize) -> PRes<&[u8]> {
        match self {
            DataPkt::Pkt(p) => p.pull(cx, len),
            DataPkt::MdcBuf(m) => m.pull(cx, len),
        }
    }
}

/// pgp-decrypt.c:873 process_data_packets.
fn process_data_packets(
    cx: &mut Dcx,
    dst: &mut Vec<u8>,
    src: &mut dyn Pull,
    allow_compr: bool,
    need_mdc: bool,
) -> PRes<()> {
    let mut got_data = false;
    let mut got_mdc = false;
    while let Some(hdr) = parse_pkt_hdr(src, cx, true)? {
        if got_mdc {
            return Err(cx.corrupt("process_data_packets: data after mdc"));
        }
        let mut pkt = if need_mdc && hdr.ty == PktType::Context {
            DataPkt::MdcBuf(MdcBuf::new(src, cx))
        } else {
            DataPkt::Pkt(PktPull::new(src, &hdr))
        };
        match hdr.tag {
            PGP_PKT_LITERAL_DATA => {
                got_data = true;
                parse_literal_data(cx, dst, &mut pkt)?;
            }
            PGP_PKT_COMPRESSED_DATA => {
                if !allow_compr {
                    return Err(cx.corrupt("process_data_packets: unexpected compression"));
                }
                if got_data {
                    return Err(cx.corrupt("process_data_packets: only one cmpr pkt allowed"));
                }
                got_data = true;
                parse_compressed_data(cx, dst, &mut pkt)?;
            }
            PGP_PKT_MDC => {
                if !need_mdc {
                    return Err(cx.corrupt("process_data_packets: unexpected MDC"));
                }
                mdc_finish(cx, &mut pkt, &hdr)?;
                got_mdc = true;
            }
            t => return Err(cx.corrupt(&format!("process_data_packets: unexpected pkt tag={t}"))),
        }
    }
    if !got_data {
        return Err(cx.corrupt("process_data_packets: no data"));
    }
    if need_mdc && !got_mdc && !cx.use_mdcbuf {
        return Err(cx.corrupt("process_data_packets: got no mdc"));
    }
    Ok(())
}

/// pgp-decrypt.c:341 mdc_finish.
fn mdc_finish(cx: &mut Dcx, pkt: &mut dyn Pull, hdr: &PktHdr) -> PRes<()> {
    if cx.use_mdcbuf {
        return Err(PGCRYPTO_BUG.to_string());
    }
    if hdr.len != MDC_DIGEST_LEN {
        return Err(CORRUPT_DATA.to_string());
    }
    cx.in_mdc_pkt = true;
    let data = read_max(pkt, cx, MDC_DIGEST_LEN)?;
    if data.is_empty() {
        return Err(cx.corrupt("no mdc"));
    }
    if data.len() != MDC_DIGEST_LEN {
        return Err(cx.corrupt(&format!("mdc_finish: read failed, res={}", data.len())));
    }
    let hash = cx.mdc.as_mut().ok_or_else(|| PGCRYPTO_BUG.to_string())?.finish();
    if hash != data {
        return Err(cx.corrupt("mdc_finish: mdc failed"));
    }
    Ok(())
}

/// pgp-decrypt.c:816 parse_compressed_data.
fn parse_compressed_data(cx: &mut Dcx, dst: &mut Vec<u8>, pkt: &mut dyn Pull) -> PRes<()> {
    let algo = getbyte(pkt, cx)? as i32;
    cx.ctx.compress_algo = algo;
    match algo {
        PGP_COMPR_NONE => process_data_packets(cx, dst, pkt, false, false),
        PGP_COMPR_ZIP | PGP_COMPR_ZLIB => {
            let mut dec = DecompressPull::new(pkt, algo == PGP_COMPR_ZLIB);
            process_data_packets(cx, dst, &mut dec, false, false)
        }
        PGP_COMPR_BZIP2 => {
            cx.dbg("parse_compressed_data: bzip2 unsupported");
            cx.ctx.unsupported_compr = true;
            // Drained so the bytes still reach the MDC digest.
            while !pkt.pull(cx, 32 * 1024)?.is_empty() {}
            Ok(())
        }
        _ => Err(cx.corrupt("parse_compressed_data: unknown compr type")),
    }
}

/// pgp-compress.c:236 decompress_read: 8192-byte source reads, an
/// 8192-byte output buffer, and after Z_STREAM_END one more source read
/// that must find nothing.
struct DecompressPull<'a> {
    src: &'a mut dyn Pull,
    inf: Inflater,
    input: Vec<u8>,
    in_pos: usize,
    buf: Vec<u8>,
    pos: usize,
    buf_data: usize,
    eof: bool,
}

const ZIP_IN_BLOCK: usize = 8192;
const ZIP_OUT_BUF: usize = 8192;

impl<'a> DecompressPull<'a> {
    fn new(src: &'a mut dyn Pull, zlib: bool) -> DecompressPull<'a> {
        DecompressPull {
            src,
            inf: Inflater::new(zlib),
            input: Vec::with_capacity(ZIP_IN_BLOCK),
            in_pos: 0,
            buf: vec![0u8; ZIP_OUT_BUF],
            pos: 0,
            buf_data: 0,
            eof: false,
        }
    }

    fn refill(&mut self, cx: &mut Dcx) -> PRes<()> {
        if self.in_pos == self.input.len() {
            let d = self.src.pull(cx, ZIP_IN_BLOCK)?;
            self.input.clear();
            self.input.extend_from_slice(d);
            self.in_pos = 0;
        }
        let finish = self.in_pos == self.input.len();
        let (code, used, written) = self.inf.inflate(&self.input[self.in_pos..], &mut self.buf, finish);
        self.in_pos += used;
        if code != Z_OK && code != Z_STREAM_END {
            return Err(cx.corrupt(&format!("decompress_read: inflate error: {code}")));
        }
        self.buf_data = written;
        self.pos = 0;
        if code == Z_STREAM_END {
            if !self.src.pull(cx, 1)?.is_empty() {
                return Err(cx.corrupt("decompress_read: extra bytes after end of stream"));
            }
            self.eof = true;
        }
        Ok(())
    }
}

impl Pull for DecompressPull<'_> {
    fn pull(&mut self, cx: &mut Dcx, len: usize) -> PRes<&[u8]> {
        while self.buf_data == 0 && !self.eof {
            self.refill(cx)?;
        }
        let n = len.min(self.buf_data);
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        self.buf_data -= n;
        Ok(out)
    }
}

/// mbuf.c:37 STEP.
const MBUF_STEP: usize = 16 * 1024;

/// mbuf.c:94 mbuf_append over :79 prepare_room. The output MBuf also holds
/// the VARHDRSZ bytes pgp-pgsql.c:484 reserved for the result header, and
/// a growth past MaxAllocSize is mcxt.c:1180's error.
pub(crate) fn mbuf_append(ctx: &mut PgpContext, dst: &mut Vec<u8>, data: &[u8]) -> PRes<()> {
    let end = 4 + dst.len() + data.len();
    if end > ctx.mbuf_cap {
        let grow = (data.len() + 2 * MBUF_STEP - 1) & !(MBUF_STEP - 1);
        let newlen = ctx.mbuf_cap.saturating_add(grow);
        if newlen > ::mcx::MAX_ALLOC_SIZE {
            return Err(format!("{ALLOC_SIZE_ERROR} {newlen}"));
        }
        ctx.mbuf_cap = newlen;
    }
    dst.extend_from_slice(data);
    Ok(())
}

/// mcxt.c:1180 MemoryContextSizeFailure's message, an XX000 at the SQL
/// wrapper (lib.rs px_msg).
pub const ALLOC_SIZE_ERROR: &str = "invalid memory alloc request size";

/// pgp-decrypt.c:694 copy_crlf: "\r\n" becomes "\n"; a '\r' ending one read
/// waits for the next one's first byte.
fn copy_crlf(ctx: &mut PgpContext, dst: &mut Vec<u8>, data: &[u8], got_cr: &mut bool) -> PRes<()> {
    let mut tmp = [0u8; 1024];
    let mut p = 0usize;
    if *got_cr {
        if data[0] != b'\n' {
            tmp[p] = b'\r';
            p += 1;
        }
        *got_cr = false;
    }
    let mut i = 0usize;
    while i < data.len() {
        if data[i] == b'\r' {
            if i + 1 < data.len() {
                if data[i + 1] == b'\n' {
                    i += 1;
                }
            } else {
                *got_cr = true;
                break;
            }
        }
        tmp[p] = data[i];
        p += 1;
        i += 1;
        if p == tmp.len() {
            mbuf_append(ctx, dst, &tmp)?;
            p = 0;
        }
    }
    if p > 0 {
        mbuf_append(ctx, dst, &tmp[..p])?;
    }
    Ok(())
}

/// pgp-decrypt.c:745 parse_literal_data. CRLF conversion follows the SQL
/// caller's text mode and convert-crlf, not the literal packet's own type.
fn parse_literal_data(cx: &mut Dcx, dst: &mut Vec<u8>, pkt: &mut dyn Pull) -> PRes<()> {
    let ty = getbyte(pkt, cx)?;
    let mut name_len = getbyte(pkt, cx)? as usize;
    while name_len > 0 {
        let n = pkt.pull(cx, name_len)?.len();
        if n == 0 {
            break;
        }
        name_len -= n;
    }
    if name_len > 0 {
        return Err(cx.corrupt("parse_literal_data: unexpected eof"));
    }
    if !matches!(read_max(pkt, cx, 4), Ok(d) if d.len() == 4) {
        return Err(cx.corrupt("parse_literal_data: unexpected eof"));
    }
    let ctx = &mut *cx.ctx;
    if ctx.text_mode != 0 && ty != b't' && ty != b'u' {
        ctx.dbg(&format!("parse_literal_data: data type={}", ty as char));
        ctx.unexpected_binary = true;
    }
    ctx.unicode_mode = if ty == b'u' { 1 } else { 0 };
    let crlf = ctx.text_mode != 0 && ctx.convert_crlf != 0;
    let mut got_cr = false;
    loop {
        let buf = pkt.pull(cx, 32 * 1024)?;
        if buf.is_empty() {
            break;
        }
        if crlf {
            copy_crlf(cx.ctx, dst, buf, &mut got_cr)?;
        } else {
            mbuf_append(cx.ctx, dst, buf)?;
        }
    }
    if got_cr {
        mbuf_append(cx.ctx, dst, b"\r")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dcx_run<T>(f: impl FnOnce(&mut Dcx) -> T) -> (T, Vec<String>) {
        let mut ctx = PgpContext::default();
        ctx.debug = 1;
        let r = f(&mut Dcx::new(&mut ctx));
        (r, ctx.debug_notices)
    }

    // pgp-decrypt.c:403/:765/:775: an MDC stream ending inside the literal's
    // name reports only mdc_read's EOF; ending inside the date also reports
    // the date read's shortfall.
    #[test]
    fn literal_eof_under_mdc_reports_like_c() {
        let run = |data: &[u8]| {
            dcx_run(|cx| {
                let mut src = SliceReader::new(data);
                let mut mdc = MdcPull { src: &mut src };
                parse_literal_data(cx, &mut Vec::new(), &mut mdc).unwrap_err()
            })
            .1
        };
        assert_eq!(run(b"b\x02a"), ["dbg: mdc_read: unexpected eof"]);
        assert_eq!(run(b"b\x01a\x00\x00"), [
            "dbg: mdc_read: unexpected eof",
            "dbg: parse_literal_data: unexpected eof",
        ]);
    }

    #[test]
    fn mbuf_growth_counts_the_reserved_varlena_header() {
        let mut ctx = PgpContext::default();
        ctx.mbuf_cap = 1073741823;
        let mut dst = vec![0u8; 1073741809];
        mbuf_append(&mut ctx, &mut dst, &[0; 10]).unwrap();
        assert_eq!(
            mbuf_append(&mut ctx, &mut dst, &[0; 10]).unwrap_err(),
            "invalid memory alloc request size 1073774591"
        );
    }

    // The literal is appended as it streams in; CRLF pairs split across
    // reads still collapse, and a lone trailing '\r' survives.
    #[test]
    fn copy_crlf_across_reads() {
        let mut ctx = PgpContext::default();
        let mut dst = Vec::new();
        let mut cr = false;
        for part in [&b"a\r"[..], b"\nb\r", b"c\r\r", b"\n", b"\r"] {
            copy_crlf(&mut ctx, &mut dst, part, &mut cr).unwrap();
        }
        assert!(cr);
        assert_eq!(dst, b"a\nb\rc\r\n");
    }

    // pgp-compress.c:281: the zlib code is px_debug'd before
    // PXE_PGP_CORRUPT_DATA.
    #[test]
    fn inflate_failure_is_debugged_with_zlib_code() {
        let run = |body: &[u8]| {
            dcx_run(|cx| {
                let mut src = SliceReader::new(body);
                parse_compressed_data(cx, &mut Vec::new(), &mut src).unwrap_err()
            })
        };
        let (err, notices) = run(&[PGP_COMPR_ZIP as u8, 0xff, 0xff, 0xff]);
        assert_eq!(err, CORRUPT_DATA);
        assert_eq!(notices, ["dbg: decompress_read: inflate error: -3"]);
        let mut lit = vec![0xCB, 0xFF, 0, 0, 0x03, 0xE8, b'b', 0, 0, 0, 0, 0];
        lit.extend((0..994u32).map(|i| (i * 7919 % 251) as u8));
        let comp = super::super::compress::deflate_zlib(&lit, 6);
        let mut body = vec![PGP_COMPR_ZLIB as u8];
        body.extend_from_slice(&comp[..comp.len() / 2]);
        assert_eq!(run(&body).1, ["dbg: decompress_read: inflate error: -5"]);
    }
}
