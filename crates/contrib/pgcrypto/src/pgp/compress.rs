
use ::miniz_oxide::deflate::core::{compress, create_comp_flags_from_zip_params, CompressorOxide};
use ::miniz_oxide::deflate::core::TDEFLStatus;
use ::miniz_oxide::inflate::core::{decompress, inflate_flags, DecompressorOxide};
use ::miniz_oxide::inflate::TINFLStatus;

fn miniz_level(level: i32) -> u8 {
    level.clamp(1, 9) as u8
}

fn deflate(data: &[u8], level: i32, zlib_header: bool) -> Vec<u8> {
    let flags = create_comp_flags_from_zip_params(miniz_level(level) as i32, zlib_header as i32, 0);
    let mut comp = CompressorOxide::new(flags);
    let mut out = vec![0u8; data.len() + data.len() / 2 + 128];
    let mut in_pos = 0usize;
    let mut out_pos = 0usize;
    loop {
        let (status, consumed, written) = compress(
            &mut comp,
            &data[in_pos..],
            &mut out[out_pos..],
            ::miniz_oxide::deflate::core::TDEFLFlush::Finish,
        );
        in_pos += consumed;
        out_pos += written;
        match status {
            TDEFLStatus::Done => break,
            TDEFLStatus::Okay => {
                if out_pos == out.len() {
                    let extra = out.len();
                    out.resize(out.len() + extra, 0);
                }
            }
            _ => break,
        }
    }
    out.truncate(out_pos);
    out
}

/// ZIP — raw DEFLATE (RFC 1951), no zlib header.
pub fn deflate_raw(data: &[u8], level: i32) -> Vec<u8> {
    deflate(data, level, false)
}

/// ZLIB — zlib-wrapped DEFLATE (RFC 1950).
pub fn deflate_zlib(data: &[u8], level: i32) -> Vec<u8> {
    deflate(data, level, true)
}

/// zlib return codes (zlib.h) as C's inflate() reports them.
pub const Z_OK: i32 = 0;
pub const Z_STREAM_END: i32 = 1;
const Z_NEED_DICT: i32 = 2;
const Z_DATA_ERROR: i32 = -3;
const Z_BUF_ERROR: i32 = -5;

/// Deflate's maximum match distance: the history a sliding window keeps.
const WINDOW: usize = 32 * 1024;
const HIST_CAP: usize = WINDOW + 256 * 1024;

/// zlib inflate() answers Z_NEED_DICT once a well-formed header with FDICT
/// set and its DICTID are in; miniz_oxide rejects the flag as a bad header.
fn zlib_needs_dict(hdr: [u8; 2]) -> bool {
    hdr[0] & 0x0f == 8
        && hdr[0] >> 4 <= 7
        && (((hdr[0] as u32) << 8) | hdr[1] as u32) % 31 == 0
        && hdr[1] & 0x20 != 0
}

/// A z_stream in inflate mode (pgp-compress.c:212 decompress_init). Each
/// call produces at most `out.len()` bytes and consumes input only as far
/// as that output needs, as zlib does, so the caller's source reads
/// happen where C's do.
pub struct Inflater {
    decomp: Box<DecompressorOxide>,
    hist: Vec<u8>,
    used: usize,
    zlib: bool,
    hdr: [u8; 2],
    hdr_seen: usize,
    need_dict: bool,
    done: bool,
}

impl Inflater {
    pub fn new(zlib: bool) -> Inflater {
        Inflater {
            decomp: Box::default(),
            hist: vec![0u8; HIST_CAP],
            used: 0,
            zlib,
            hdr: [0; 2],
            hdr_seen: 0,
            need_dict: false,
            done: false,
        }
    }

    /// inflate(strm, finish ? Z_FINISH : Z_SYNC_FLUSH): (code, consumed,
    /// written).
    pub fn inflate(&mut self, input: &[u8], out: &mut [u8], finish: bool) -> (i32, usize, usize) {
        if self.done {
            return (Z_STREAM_END, 0, 0);
        }
        let mut consumed = 0usize;
        if self.zlib && (self.hdr_seen < 2 || self.need_dict) {
            while self.hdr_seen < 2 && consumed < input.len() {
                self.hdr[self.hdr_seen] = input[consumed];
                self.hdr_seen += 1;
                consumed += 1;
            }
            if self.hdr_seen < 2 {
                return (Self::no_end(consumed, 0, finish), consumed, 0);
            }
            if !self.need_dict && zlib_needs_dict(self.hdr) {
                self.need_dict = true;
                self.hdr_seen = 2;
            }
            if self.need_dict {
                while self.hdr_seen < 6 && consumed < input.len() {
                    self.hdr_seen += 1;
                    consumed += 1;
                }
                if self.hdr_seen == 6 {
                    return (Z_NEED_DICT, consumed, 0);
                }
                return (Self::no_end(consumed, 0, finish), consumed, 0);
            }
            if consumed > 0 {
                let hdr = self.hdr;
                let (st, _, _) = self.step(&hdr, 0, false);
                if (st as i32) < 0 {
                    return (Z_DATA_ERROR, consumed, 0);
                }
            }
        }
        let (st, inb, written) = self.step(&input[consumed..], out.len(), finish);
        consumed += inb;
        out[..written].copy_from_slice(&self.hist[self.used - written..self.used]);
        let code = match st {
            TINFLStatus::Done => {
                self.done = true;
                Z_STREAM_END
            }
            TINFLStatus::HasMoreOutput | TINFLStatus::NeedsMoreInput => {
                Self::no_end(consumed, written, finish)
            }
            TINFLStatus::FailedCannotMakeProgress => Z_BUF_ERROR,
            _ => Z_DATA_ERROR,
        };
        (code, consumed, written)
    }

    /// zlib's verdict on a call that did not reach the stream end.
    fn no_end(consumed: usize, written: usize, finish: bool) -> i32 {
        if finish || (consumed == 0 && written == 0) {
            Z_BUF_ERROR
        } else {
            Z_OK
        }
    }

    fn step(&mut self, input: &[u8], room: usize, finish: bool) -> (TINFLStatus, usize, usize) {
        if self.used + room > HIST_CAP {
            self.hist.copy_within(self.used - WINDOW..self.used, 0);
            self.used = WINDOW;
        }
        let mut flags = inflate_flags::TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF;
        flags |= if self.zlib {
            inflate_flags::TINFL_FLAG_PARSE_ZLIB_HEADER | inflate_flags::TINFL_FLAG_COMPUTE_ADLER32
        } else {
            inflate_flags::TINFL_FLAG_IGNORE_ADLER32
        };
        if !finish {
            flags |= inflate_flags::TINFL_FLAG_HAS_MORE_INPUT;
        }
        let end = self.used + room;
        let (st, inb, outb) = decompress(&mut self.decomp, input, &mut self.hist[..end], self.used, flags);
        self.used += outb;
        (st, inb, outb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drives an Inflater the way pgp-compress.c:236 decompress_read does:
    /// 8192-byte source reads, 8192-byte output buffer.
    fn inflate_all(data: &[u8], zlib: bool) -> Result<Vec<u8>, i32> {
        let mut inf = Inflater::new(zlib);
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        let (mut pos, mut avail) = (0usize, 0usize);
        loop {
            if avail == 0 {
                avail = (data.len() - pos).min(8192);
            }
            let (code, used, written) = inf.inflate(&data[pos..pos + avail], &mut buf, avail == 0);
            pos += used;
            avail -= used;
            out.extend_from_slice(&buf[..written]);
            match code {
                Z_STREAM_END => return Ok(out),
                Z_OK => {}
                e => return Err(e),
            }
        }
    }

    #[test]
    fn streams_large_output_in_bounded_steps() {
        let plain: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8 ^ (i >> 13) as u8).collect();
        for zlib in [false, true] {
            let comp = if zlib { deflate_zlib(&plain, 6) } else { deflate_raw(&plain, 6) };
            assert_eq!(inflate_all(&comp, zlib).unwrap(), plain);
        }
    }

    // zlib consumes input only as its output needs: a stream that inflates
    // to far more than one output buffer leaves input unread.
    #[test]
    fn input_is_consumed_as_output_is_produced() {
        let comp = deflate_raw(&vec![b'a'; 4 << 20], 6);
        let mut inf = Inflater::new(false);
        let mut buf = [0u8; 8192];
        let (code, used, written) = inf.inflate(&comp, &mut buf, false);
        assert_eq!((code, written), (Z_OK, 8192));
        assert!(used < comp.len());
    }

    #[test]
    fn inflate_failures_carry_zlib_codes() {
        assert_eq!(inflate_all(&[0xff, 0xff, 0xff, 0xff], false).unwrap_err(), -3);
        assert_eq!(inflate_all(&[0x00, 0x00], true).unwrap_err(), -3);
        assert_eq!(inflate_all(&[0x78, 0x20, 0, 0, 0, 1, 0x03, 0x00], true).unwrap_err(), 2);
        assert_eq!(inflate_all(&[0x78, 0x21, 0, 0, 0, 1, 0x03, 0x00], true).unwrap_err(), -3);
        assert_eq!(inflate_all(&[0x78, 0x20, 0, 0, 0, 1, 0x03, 0x00], false).unwrap_err(), -3);
        let comp = deflate_raw(b"truncated stream payload", 6);
        assert_eq!(inflate_all(&comp[..comp.len() / 2], false).unwrap_err(), -5);
        assert_eq!(inflate_all(&[], false).unwrap_err(), -5);
    }
}
