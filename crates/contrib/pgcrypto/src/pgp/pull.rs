//! mbuf.c PullFilter chain as the OpenPGP decrypt side uses it: every
//! reader pulls on demand from the one below, so errors surface in the
//! order C meets them.

use super::consts::{Digest, CORRUPT_DATA};
use super::context::PgpContext;

pub type PRes<T> = Result<T, String>;

/// PGP_Context's decrypt-time state that is not an option: the MDC digest
/// and the flags the mdc and mdcbuf filters share (pgp.h).
pub struct Dcx<'c> {
    pub ctx: &'c mut PgpContext,
    pub mdc: Option<Digest>,
    pub use_mdcbuf: bool,
    pub in_mdc_pkt: bool,
}

impl<'c> Dcx<'c> {
    pub fn new(ctx: &'c mut PgpContext) -> Dcx<'c> {
        Dcx { ctx, mdc: None, use_mdcbuf: false, in_mdc_pkt: false }
    }

    pub fn dbg(&mut self, msg: &str) {
        self.ctx.dbg(msg);
    }

    pub fn corrupt(&mut self, msg: &str) -> String {
        self.ctx.dbg(msg);
        CORRUPT_DATA.to_string()
    }
}

pub trait Pull {
    /// mbuf.c:222 pullf_read: at most `len` bytes, possibly fewer; an
    /// empty slice is end of data.
    fn pull(&mut self, cx: &mut Dcx, len: usize) -> PRes<&[u8]>;
}

/// mbuf.c:350 pullf_create_mbuf_reader.
pub struct SliceReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> SliceReader<'a> {
    pub fn new(data: &'a [u8]) -> SliceReader<'a> {
        SliceReader { data, pos: 0 }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }
}

impl Pull for SliceReader<'_> {
    fn pull(&mut self, _cx: &mut Dcx, len: usize) -> PRes<&[u8]> {
        let n = len.min(self.data.len() - self.pos);
        let out = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }
}

/// mbuf.c:240 pullf_read_max: repeats short reads until `len` bytes or EOF.
pub fn read_max(src: &mut dyn Pull, cx: &mut Dcx, len: usize) -> PRes<Vec<u8>> {
    let mut out = Vec::new();
    out.extend_from_slice(src.pull(cx, len)?);
    if out.is_empty() || out.len() == len {
        return Ok(out);
    }
    while out.len() < len {
        let d = src.pull(cx, len - out.len())?;
        if d.is_empty() {
            break;
        }
        out.extend_from_slice(d);
    }
    Ok(out)
}

/// mbuf.c:287 pullf_read_fixed.
pub fn read_fixed(src: &mut dyn Pull, cx: &mut Dcx, len: usize) -> PRes<Vec<u8>> {
    let got = read_max(src, cx, len)?;
    if got.len() != len {
        return Err(cx.corrupt(&format!("pullf_read_fixed: need={len} got={}", got.len())));
    }
    Ok(got)
}

/// mbuf.h:113 GETBYTE.
pub fn getbyte(src: &mut dyn Pull, cx: &mut Dcx) -> PRes<u8> {
    Ok(read_fixed(src, cx, 1)?[0])
}

/// pgp-decrypt.c:44 PKT_NORMAL / PKT_STREAM / PKT_CONTEXT.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PktType {
    Normal,
    Stream,
    Context,
}

/// pgp-decrypt.c:47 MAX_CHUNK.
const MAX_CHUNK: u64 = 16 * 1024 * 1024;

/// pgp-decrypt.c:50 parse_new_len.
fn parse_new_len(src: &mut dyn Pull, cx: &mut Dcx) -> PRes<(usize, PktType)> {
    let b = getbyte(src, cx)?;
    let (len, ty) = if b <= 191 {
        (b as u64, PktType::Normal)
    } else if b <= 223 {
        let b2 = getbyte(src, cx)?;
        ((((b as u64) - 192) << 8) + 192 + b2 as u64, PktType::Normal)
    } else if b == 255 {
        let mut len = 0u64;
        for _ in 0..4 {
            len = (len << 8) | getbyte(src, cx)? as u64;
        }
        (len, PktType::Normal)
    } else {
        (1u64 << (b & 0x1f), PktType::Stream)
    };
    if len > MAX_CHUNK {
        return Err(cx.corrupt("parse_new_len: weird length"));
    }
    Ok((len as usize, ty))
}

/// pgp-decrypt.c:94 parse_old_len.
fn parse_old_len(src: &mut dyn Pull, cx: &mut Dcx, lentype: u8) -> PRes<usize> {
    let mut len = getbyte(src, cx)? as u64;
    let extra = match lentype {
        1 => 1,
        2 => 3,
        _ => 0,
    };
    for _ in 0..extra {
        len = (len << 8) | getbyte(src, cx)? as u64;
    }
    if len > MAX_CHUNK {
        return Err(cx.corrupt("parse_old_len: weird length"));
    }
    Ok(len as usize)
}

pub struct PktHdr {
    pub tag: i32,
    pub len: usize,
    pub ty: PktType,
}

/// pgp-decrypt.c:128 pgp_parse_pkt_hdr; `None` is a clean EOF.
pub fn parse_pkt_hdr(src: &mut dyn Pull, cx: &mut Dcx, allow_ctx: bool) -> PRes<Option<PktHdr>> {
    let p = match src.pull(cx, 1)? {
        [] => return Ok(None),
        d => d[0],
    };
    if p & 0x80 == 0 {
        return Err(cx.corrupt("pgp_parse_pkt_hdr: not pkt hdr"));
    }
    if p & 0x40 != 0 {
        let (len, ty) = parse_new_len(src, cx)?;
        return Ok(Some(PktHdr { tag: (p & 0x3f) as i32, len, ty }));
    }
    let lentype = p & 3;
    let tag = ((p >> 2) & 0x0f) as i32;
    if lentype == 3 {
        if !allow_ctx {
            return Err(CORRUPT_DATA.to_string());
        }
        return Ok(Some(PktHdr { tag, len: 0, ty: PktType::Context }));
    }
    let len = parse_old_len(src, cx, lentype)?;
    Ok(Some(PktHdr { tag, len, ty: PktType::Normal }))
}

/// pgp-decrypt.c:177 pktreader_pull: a stream's next chunk header is read
/// only when the current chunk is used up and more bytes are wanted.
pub struct PktPull<'a> {
    src: &'a mut dyn Pull,
    ty: PktType,
    len: usize,
}

impl<'a> PktPull<'a> {
    pub fn new(src: &'a mut dyn Pull, hdr: &PktHdr) -> PktPull<'a> {
        PktPull { src, ty: hdr.ty, len: hdr.len }
    }
}

impl Pull for PktPull<'_> {
    fn pull(&mut self, cx: &mut Dcx, len: usize) -> PRes<&[u8]> {
        if self.ty == PktType::Context {
            return self.src.pull(cx, len);
        }
        while self.len == 0 {
            if self.ty == PktType::Normal {
                return Ok(&[]);
            }
            let (l, t) = parse_new_len(self.src, cx)?;
            self.len = l;
            self.ty = t;
        }
        let d = self.src.pull(cx, len.min(self.len))?;
        self.len -= d.len();
        Ok(d)
    }
}

/// pgp-decrypt.c:1069 pgp_skip_packet.
pub fn skip_packet(pkt: &mut dyn Pull, cx: &mut Dcx) -> PRes<()> {
    while !pkt.pull(cx, 32 * 1024)?.is_empty() {}
    Ok(())
}

/// pgp-decrypt.c:1083 pgp_expect_packet_end.
pub fn expect_packet_end(pkt: &mut dyn Pull, cx: &mut Dcx) -> PRes<()> {
    if !pkt.pull(cx, 32 * 1024)?.is_empty() {
        return Err(cx.corrupt("pgp_expect_packet_end: got data"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_chunk_headers_are_read_lazily() {
        let data = [0xE1, b'a', b'b', 0xFF];
        let mut ctx = PgpContext::default();
        ctx.debug = 1;
        let mut cx = Dcx::new(&mut ctx);
        let mut src = SliceReader::new(&data);
        let (len, ty) = parse_new_len(&mut src, &mut cx).unwrap();
        assert_eq!((len, ty), (2, PktType::Stream));
        let mut pkt = PktPull::new(&mut src, &PktHdr { tag: 11, len, ty });
        assert_eq!(pkt.pull(&mut cx, 10).unwrap(), b"ab");
        assert_eq!(pkt.pull(&mut cx, 10).unwrap_err(), CORRUPT_DATA);
        assert_eq!(cx.ctx.debug_notices, ["dbg: pullf_read_fixed: need=1 got=0"]);
    }

    #[test]
    fn read_max_joins_short_reads_across_chunks() {
        let data = [0xE0, b'x', 0x02, b'y', b'z'];
        let mut ctx = PgpContext::default();
        let mut cx = Dcx::new(&mut ctx);
        let mut src = SliceReader::new(&data[1..]);
        let hdr = PktHdr { tag: 11, len: 1, ty: PktType::Stream };
        let mut pkt = PktPull::new(&mut src, &hdr);
        assert_eq!(read_max(&mut pkt, &mut cx, 8).unwrap(), b"xyz");
        assert!(pkt.pull(&mut cx, 1).unwrap().is_empty());
    }
}
