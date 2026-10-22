//! Page decompression and page integrity. Borrowed codecs (build-vs-borrow
//! ruling): snap, lz4_flex raw blocks (also inside the Hadoop frame),
//! zstd, miniz_oxide inflate (GZIP members), brotli-decompressor. LZO is
//! refused by name (no maintained pure-Rust decoder; the spec deprecates
//! it). The crc32 of a page is verified when the header carries one.

use types_error::{PgError, PgResult, ERRCODE_BAD_COPY_FILE_FORMAT};

use crate::meta::{unsupported, CodecId};

/// Scan buffers keep 64 readable bytes past their logical end so decode
/// kernels never need tail special-casing.
pub(crate) const PAD: usize = 64;

#[track_caller]
#[cold]
#[inline(never)]
fn decompress_failed(codec: &str, detail: String) -> Box<PgError> {
    Box::new(
        PgError::error(format!("corrupt {codec} page in parquet file")) // alloc: Cold
            .with_sqlstate(ERRCODE_BAD_COPY_FILE_FORMAT)
            .with_detail(detail),
    )
}

/// Decompress `src` into `dst` (cleared first), which ends up holding exactly
/// `uncompressed` bytes plus zeroed padding. The declared size is the
/// contract: a short or long result is a corruption error.
pub(crate) fn decompress_page(
    codec: CodecId,
    src: &[u8],
    dst: &mut Vec<u8>,
    uncompressed: usize,
) -> PgResult<()> {
    dst.clear();
    // uncompressed_page_size is attacker-shaped file input: cap it at the
    // MaxAllocSize class before committing a zero-filled buffer.
    const MAX_PAGE_BYTES: usize = 0x3FFF_FFFF;
    if uncompressed > MAX_PAGE_BYTES {
        return Err(Box::new(PgError::error(format!( // alloc: Cold
            "parquet page declares {uncompressed} uncompressed bytes, above the {MAX_PAGE_BYTES} byte limit"
        ))));
    }
    dst.try_reserve(uncompressed + PAD) // alloc: Exec charged the cursor's charge (ColumnCursor / FastCol held_bytes)
        .map_err(|_| Box::new(PgError::error("out of memory decompressing parquet page")))?; // alloc: Cold
    dst.resize(uncompressed, 0); // alloc: Exec charged the cursor's charge (ColumnCursor / FastCol held_bytes)
    let written = match codec {
        CodecId::Uncompressed => {
            // Callers slice the chunk buffer directly; reaching here means a
            // caller wanted an owned copy.
            if src.len() != uncompressed {
                return Err(decompress_failed(
                    "UNCOMPRESSED",
                    format!("page declares {uncompressed} bytes, holds {}", src.len()), // alloc: Cold
                ));
            }
            dst[..src.len()].copy_from_slice(src);
            src.len()
        }
        CodecId::Snappy => {
            let n = snap::raw::decompress_len(src)
                .map_err(|e| decompress_failed("SNAPPY", e.to_string()))?; // alloc: Cold
            if n != uncompressed {
                return Err(decompress_failed(
                    "SNAPPY",
                    format!("page declares {uncompressed} bytes, stream holds {n}"), // alloc: Cold
                ));
            }
            snap::raw::Decoder::new()
                .decompress(src, dst)
                .map_err(|e| decompress_failed("SNAPPY", e.to_string()))? // alloc: Cold
        }
        CodecId::Zstd => zstd_decompress(src, dst)?,
        CodecId::Lz4Raw => lz4_flex::block::decompress_into(src, dst)
            .map_err(|e| decompress_failed("LZ4_RAW", e.to_string()))?, // alloc: Cold
        CodecId::Lz4Framed => lz4_hadoop(src, dst)?,
        CodecId::Gzip => gzip_members(src, dst)?,
        CodecId::Brotli => brotli(src, dst)?,
        CodecId::Lzo => return Err(unsupported("compression codec LZO".to_string())), // alloc: Cold
    };
    if written != uncompressed {
        return Err(decompress_failed(
            codec.name(),
            format!("page declares {uncompressed} bytes, decoded {written}"), // alloc: Cold
        ));
    }
    // Zeroed pad beyond the logical length (initialized capacity): kernels
    // that read ahead through raw pointers never touch uninitialized bytes.
    dst.resize(uncompressed + PAD, 0); // alloc: Exec charged the cursor's charge (ColumnCursor / FastCol held_bytes)
    // SAFETY: shrinking a Vec<u8> length over initialized bytes.
    unsafe { dst.set_len(uncompressed) };
    Ok(())
}

#[cfg(not(target_family = "wasm"))]
fn zstd_decompress(src: &[u8], dst: &mut [u8]) -> PgResult<usize> {
    zstd::bulk::decompress_to_buffer(src, dst)
        .map_err(|e| decompress_failed("ZSTD", e.to_string())) // alloc: Cold
}

#[cfg(target_family = "wasm")]
fn zstd_decompress(_src: &[u8], _dst: &mut [u8]) -> PgResult<usize> {
    Err(unsupported("compression codec ZSTD (unavailable on this platform)".to_string())) // alloc: Cold
}

/// Codec 5. The spec's form is the Hadoop frame: repeated
/// [u32 BE decompressed size][u32 BE compressed size][raw lz4 block(s)].
/// Some writers stored a bare raw block under the same id; the frame shape
/// is decided structurally (sizes consistent with the page), so each
/// payload takes exactly one arm and a wrong guess fails on length.
fn lz4_hadoop(src: &[u8], dst: &mut [u8]) -> PgResult<usize> {
    let framed = frame_walk(src, dst.len());
    if framed {
        let mut ip = 0usize;
        let mut op = 0usize;
        while ip < src.len() {
            let dsz = u32::from_be_bytes(src[ip..ip + 4].try_into().expect("4 bytes")) as usize;
            let csz = u32::from_be_bytes(src[ip + 4..ip + 8].try_into().expect("4 bytes")) as usize;
            ip += 8;
            // One frame may hold several raw blocks; each decompresses to
            // a prefix of the frame's declared size.
            let frame_end = ip + csz;
            let mut produced = 0usize;
            while ip < frame_end {
                let n = lz4_flex::block::decompress_into(&src[ip..frame_end], &mut dst[op..op + dsz - produced])
                    .map_err(|e| decompress_failed("LZ4", e.to_string()))?; // alloc: Cold
                produced += n;
                op += n;
                // decompress_into consumes the whole input slice; a frame
                // with several blocks is only decodable block by block
                // when each block's compressed size is known — the Hadoop
                // writer emits one block per frame, so the whole slice is
                // the block.
                ip = frame_end;
            }
            if produced != dsz {
                return Err(decompress_failed("LZ4", format!("frame declares {dsz} bytes, decoded {produced}"))); // alloc: Cold
            }
        }
        Ok(op)
    } else {
        lz4_flex::block::decompress_into(src, dst)
            .map_err(|e| decompress_failed("LZ4", e.to_string())) // alloc: Cold
    }
}

/// True when `src` parses as a sequence of Hadoop frames whose decompressed
/// sizes sum to `expected` and whose compressed sizes tile the buffer.
fn frame_walk(src: &[u8], expected: usize) -> bool {
    let mut ip = 0usize;
    let mut total = 0usize;
    while ip < src.len() {
        if ip + 8 > src.len() {
            return false;
        }
        let dsz = u32::from_be_bytes(src[ip..ip + 4].try_into().expect("4 bytes")) as usize;
        let csz = u32::from_be_bytes(src[ip + 4..ip + 8].try_into().expect("4 bytes")) as usize;
        ip += 8;
        if csz == 0 || ip + csz > src.len() {
            return false;
        }
        total += dsz;
        if total > expected {
            return false;
        }
        ip += csz;
    }
    ip == src.len() && total == expected
}

/// GZIP: one or more members, each a header + raw deflate + 8-byte trailer.
fn gzip_members(src: &[u8], dst: &mut [u8]) -> PgResult<usize> {
    use miniz_oxide::inflate::stream::{inflate, InflateState};
    use miniz_oxide::{DataFormat, MZFlush, MZStatus};
    let mut ip = 0usize;
    let mut op = 0usize;
    let fail = |what: &str| decompress_failed("GZIP", what.to_string()); // alloc: Cold
    while ip < src.len() {
        let h = src.get(ip..ip + 10).ok_or_else(|| fail("member header truncated"))?;
        if h[0] != 0x1f || h[1] != 0x8b || h[2] != 8 {
            return Err(fail("bad member header"));
        }
        let flg = h[3];
        ip += 10;
        if flg & 0x04 != 0 {
            let xl = src.get(ip..ip + 2).ok_or_else(|| fail("FEXTRA truncated"))?;
            let xlen = u16::from_le_bytes([xl[0], xl[1]]) as usize;
            ip += 2 + xlen;
        }
        for bit in [0x08u8, 0x10] {
            if flg & bit != 0 {
                let rest = src.get(ip..).ok_or_else(|| fail("header truncated"))?;
                let z = rest.iter().position(|&b| b == 0).ok_or_else(|| fail("unterminated header string"))?;
                ip += z + 1;
            }
        }
        if flg & 0x02 != 0 {
            ip += 2;
        }
        if ip > src.len() {
            return Err(fail("header truncated"));
        }
        let mut state = InflateState::new_boxed(DataFormat::Raw);
        let res = inflate(&mut state, &src[ip..], &mut dst[op..], MZFlush::Finish);
        match res.status {
            Ok(MZStatus::StreamEnd) => {}
            Ok(_) => return Err(fail("deflate stream did not end within the declared size")),
            Err(e) => return Err(fail(&format!("inflate failed: {e:?}"))), // alloc: Cold
        }
        ip += res.bytes_consumed;
        op += res.bytes_written;
        let trailer = src.get(ip..ip + 8).ok_or_else(|| fail("member trailer truncated"))?;
        let crc = u32::from_le_bytes(trailer[0..4].try_into().expect("4 bytes"));
        let isize_ = u32::from_le_bytes(trailer[4..8].try_into().expect("4 bytes"));
        if isize_ as usize != res.bytes_written & 0xffff_ffff {
            return Err(fail("member ISIZE does not match the decoded length"));
        }
        if crc != crc32(&dst[op - res.bytes_written..op]) {
            return Err(fail("member CRC32 mismatch"));
        }
        ip += 8;
    }
    Ok(op)
}

/// Bounded sink for the brotli decoder: never grows past the declared
/// page size.
struct Sink<'a> {
    dst: &'a mut [u8],
    pos: usize,
}

impl std::io::Write for Sink<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.pos + buf.len() > self.dst.len() {
            return Err(std::io::Error::new(std::io::ErrorKind::WriteZero, "brotli output exceeds the declared page size"));
        }
        self.dst[self.pos..self.pos + buf.len()].copy_from_slice(buf);
        self.pos += buf.len();
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn brotli(src: &[u8], dst: &mut [u8]) -> PgResult<usize> {
    let mut sink = Sink { dst, pos: 0 };
    let mut reader = src;
    brotli_decompressor::BrotliDecompress(&mut reader, &mut sink)
        .map_err(|e| decompress_failed("BROTLI", e.to_string()))?; // alloc: Cold
    Ok(sink.pos)
}

pub(crate) fn crc32(bytes: &[u8]) -> u32 {
    static TABLE: pgsync::OnceLock<[u32; 256]> = pgsync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *e = c;
        }
        t
    });
    let mut crc = !0u32;
    for &b in bytes {
        crc = table[((crc ^ u32::from(b)) & 0xff) as usize] ^ (crc >> 8);
    }
    !crc
}

/// Page crc verification is on unless `PGRUST_PARQUET_CRC=0`.
pub(crate) fn crc_enabled() -> bool {
    static ON: pgsync::OnceLock<bool> = pgsync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("PGRUST_PARQUET_CRC").map(|v| v != "0").unwrap_or(true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_known_vectors() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b"The quick brown fox jumps over the lazy dog"), 0x414F_A339);
    }

    #[test]
    fn frame_walk_shapes() {
        // One frame: 5 decompressed bytes, 3 compressed bytes.
        let mut f = vec![0, 0, 0, 5, 0, 0, 0, 3, 1, 2, 3];
        assert!(frame_walk(&f, 5));
        assert!(!frame_walk(&f, 6));
        f.push(9);
        assert!(!frame_walk(&f, 5));
        assert!(!frame_walk(&[0xF0, 0x11, 0x22], 5));
    }
}
