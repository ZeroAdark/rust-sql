//! New-format packet length encoding/decoding (RFC 4880 §4.2) shared by the

pub fn render_newlen(dst: &mut Vec<u8>, len: usize) {
    if len <= 191 {
        dst.push(len as u8);
    } else if len <= 8383 {
        dst.push((((len - 192) >> 8) + 192) as u8);
        dst.push(((len - 192) & 255) as u8);
    } else {
        dst.push(255);
        dst.push((len >> 24) as u8);
        dst.push((len >> 16) as u8);
        dst.push((len >> 8) as u8);
        dst.push(len as u8);
    }
}

pub fn write_packet(dst: &mut Vec<u8>, tag: i32, body: &[u8]) {
    dst.push(0xC0 | (tag as u8));
    render_newlen(dst, body.len());
    dst.extend_from_slice(body);
}

const STREAM_ID: u8 = 0xE0;
const STREAM_BLOCK_SHIFT: usize = 14;
const STREAM_BLOCK: usize = 1 << STREAM_BLOCK_SHIFT;

/// pgp-encrypt.c pgp_create_pkt_writer + pkt_stream_process/flush: the body
/// goes out as 16 KiB partial-length chunks (length octet 0xEE) and the
/// stream ends with a normal-length chunk holding the remainder, which is
/// empty when the body is a multiple of the block.
pub fn write_stream_packet(dst: &mut Vec<u8>, tag: i32, body: &[u8]) {
    dst.push(0xC0 | (tag as u8));
    let mut rest = body;
    while rest.len() >= STREAM_BLOCK {
        dst.push(STREAM_ID | STREAM_BLOCK_SHIFT as u8);
        dst.extend_from_slice(&rest[..STREAM_BLOCK]);
        rest = &rest[STREAM_BLOCK..];
    }
    render_newlen(dst, rest.len());
    dst.extend_from_slice(rest);
}

/// pgp-decrypt.c:37 MAX_CHUNK: no chunk length (normal, old-format or
/// partial) may exceed 16 MiB; parse_new_len/parse_old_len reject it.
const MAX_CHUNK: usize = 16 * 1024 * 1024;

/// A header/length parse failure: `Some(text)` is the px_debug C emits
/// (pgp_parse_pkt_hdr, parse_new_len, parse_old_len, pullf_read_fixed),
/// `None` a silent PXE_PGP_CORRUPT_DATA.
pub type HdrErr = Option<&'static str>;

/// mbuf.c:311 pullf_read_fixed for a GETBYTE at end of input.
pub const EOF_DBG: &str = "pullf_read_fixed: need=1 got=0";

/// mbuf.c:301 pullf_read_fixed over a packet body: fewer than `need` bytes
/// left at `at` is px_debug'd and PXE_PGP_CORRUPT_DATA.
pub fn read_fixed<'a>(
    src: &'a [u8],
    at: usize,
    need: usize,
    ctx: &mut super::context::PgpContext,
) -> Result<&'a [u8], &'static str> {
    let got = src.len().saturating_sub(at).min(need);
    if got != need {
        ctx.dbg(&format!("pullf_read_fixed: need={need} got={got}"));
        return Err(super::consts::CORRUPT_DATA);
    }
    Ok(&src[at..at + need])
}

/// One packet's bytes as C's pktreader_pull hands them out: `chunks` are
/// the (absolute start, length) runs present in the input and `truncated`
/// says the declared length ran past it — pktreader_pull reports that as a
/// plain EOF, which pgp_skip_packet-style callers accept.
pub struct Body {
    pub data: Vec<u8>,
    pub chunks: Vec<(usize, usize)>,
    pub truncated: bool,
}

impl Body {
    /// Input position right after the first `consumed` body bytes: where
    /// the source stands when C frees the packet reader without draining
    /// it (pgp-pubkey.c:529).
    pub fn pos_after(&self, consumed: usize) -> usize {
        let mut left = consumed;
        for &(start, len) in &self.chunks {
            if left <= len {
                return start + left;
            }
            left -= len;
        }
        self.chunks.last().map_or(0, |&(s, l)| s + l)
    }

    /// pos_after for a reader whose final pull was `last_len` bytes: a
    /// zero-length pull at a chunk boundary still runs pktreader_pull's
    /// length loop (pgp-decrypt.c:185), consuming the following chunk
    /// headers up to the first non-empty chunk.
    pub fn pos_after_read(&self, consumed: usize, last_len: usize) -> usize {
        if last_len != 0 {
            return self.pos_after(consumed);
        }
        let mut left = consumed;
        for (i, &(start, len)) in self.chunks.iter().enumerate() {
            let rest = &self.chunks[i + 1..];
            if left < len || rest.is_empty() {
                return start + left.min(len);
            }
            if left == len {
                return rest.iter().find(|c| c.1 > 0).unwrap_or(&rest[rest.len() - 1]).0;
            }
            left -= len;
        }
        0
    }
}

pub struct PktReader<'a> {
    pub data: &'a [u8],
    pub pos: usize,
    /// C pgp_parse_pkt_hdr's `allow_ctx`: old-format packets with
    /// indeterminate length (lentype 3) are only legal inside the decrypted
    /// data stream (pgp-decrypt.c:885 ALLOW_CTX_SIZE); the message envelope
    /// (pgp-decrypt.c:1110), key material (pgp-pubkey.c:483) and pgp_key_id
    /// (pgp-info.c:133) pass 0 and reject them as PXE_PGP_CORRUPT_DATA.
    allow_ctx: bool,
}

pub struct PktHdr {
    pub tag: i32,
    pub len: Option<usize>,
    pub partial: bool,
}

impl<'a> PktReader<'a> {
    pub fn new(data: &'a [u8]) -> PktReader<'a> {
        PktReader { data, pos: 0, allow_ctx: false }
    }

    /// Reader for the decrypted data stream (ALLOW_CTX_SIZE).
    pub fn new_allow_ctx(data: &'a [u8]) -> PktReader<'a> {
        PktReader { data, pos: 0, allow_ctx: true }
    }

    #[allow(dead_code)] // C-parity: PktReader surface kept complete
    pub fn eof(&self) -> bool {
        self.pos >= self.data.len()
    }

    fn byte(&mut self) -> Result<u8, HdrErr> {
        if self.pos >= self.data.len() {
            return Err(Some(EOF_DBG));
        }
        let b = self.data[self.pos];
        self.pos += 1;
        Ok(b)
    }

    pub fn read_hdr(&mut self) -> Result<Option<PktHdr>, HdrErr> {
        if self.pos >= self.data.len() {
            return Ok(None);
        }
        let p = self.byte()?;
        if p & 0x80 == 0 {
            return Err(Some("pgp_parse_pkt_hdr: not pkt hdr"));
        }
        if p & 0x40 != 0 {
            let tag = (p & 0x3f) as i32;
            let (len, partial) = self.parse_new_len()?;
            Ok(Some(PktHdr { tag, len: Some(len), partial }))
        } else {
            let lentype = p & 3;
            let tag = ((p >> 2) & 0x0f) as i32;
            if lentype == 3 {
                if !self.allow_ctx {
                    return Err(None);
                }
                Ok(Some(PktHdr {
                    tag,
                    len: None,
                    partial: false,
                }))
            } else {
                let len = self.parse_old_len(lentype)?;
                Ok(Some(PktHdr {
                    tag,
                    len: Some(len),
                    partial: false,
                }))
            }
        }
    }

    fn parse_new_len(&mut self) -> Result<(usize, bool), HdrErr> {
        let b = self.byte()?;
        let (len, partial) = if b <= 191 {
            (b as usize, false)
        } else if b <= 223 {
            let b2 = self.byte()?;
            ((((b as usize) - 192) << 8) + 192 + b2 as usize, false)
        } else if b == 255 {
            let mut len = 0usize;
            for _ in 0..4 {
                len = (len << 8) | self.byte()? as usize;
            }
            (len, false)
        } else {
            (1usize << (b & 0x1f), true)
        };
        if len > MAX_CHUNK {
            return Err(Some("parse_new_len: weird length"));
        }
        Ok((len, partial))
    }

    fn parse_old_len(&mut self, lentype: u8) -> Result<usize, HdrErr> {
        let extra = match lentype {
            0 => 0,
            1 => 1,
            2 => 3,
            _ => return Err(None),
        };
        let mut len = self.byte()? as usize;
        for _ in 0..extra {
            len = (len << 8) | self.byte()? as usize;
        }
        if len > MAX_CHUNK {
            return Err(Some("parse_old_len: weird length"));
        }
        Ok(len)
    }

    /// pgp-decrypt.c:201 pktreader_pull: every chunk is clamped to the
    /// bytes left in the input (a short read is EOF, not an error), and a
    /// stream's next length octet at end of input is a GETBYTE failure.
    pub fn read_body_avail(&mut self, hdr: &PktHdr) -> Result<Body, HdrErr> {
        let mut body = Body {
            data: Vec::new(),
            chunks: Vec::new(),
            truncated: false,
        };
        let Some(first_len) = hdr.len else {
            body.chunks.push((self.pos, self.data.len() - self.pos));
            body.data.extend_from_slice(&self.data[self.pos..]);
            self.pos = self.data.len();
            return Ok(body);
        };
        let (mut len, mut partial) = (first_len, hdr.partial);
        loop {
            let avail = len.min(self.data.len() - self.pos);
            body.chunks.push((self.pos, avail));
            body.data.extend_from_slice(&self.data[self.pos..self.pos + avail]);
            self.pos += avail;
            if avail < len {
                body.truncated = true;
                break;
            }
            if !partial {
                break;
            }
            (len, partial) = self.parse_new_len()?;
        }
        Ok(body)
    }
}
