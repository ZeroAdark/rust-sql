//! liblz4's LZ4_compress_default, ported verbatim. TOAST pg_column_size and
//! WAL full-page-image lengths need C's identical byte stream, and the parse
//! is not unique (lz4_flex splits the trailing literal run differently, one
//! byte longer on repeat('a', 5000)), so the encoder itself is ported rather
//! than any LZ4 library's. The hash table lives on the stack, as LZ4_stream_t
//! does in C.

const MINMATCH: usize = 4;
const LASTLITERALS: usize = 5;
const MFLIMIT: usize = 12;
const LZ4_MIN_LENGTH: usize = MFLIMIT + 1;
const ML_BITS: u32 = 4;
const ML_MASK: usize = (1 << ML_BITS) - 1;
const RUN_MASK: usize = (1 << (8 - ML_BITS)) - 1;
const LZ4_HASHLOG: u32 = 12;
const LZ4_64KLIMIT: usize = 64 * 1024 + MFLIMIT - 1;
const LZ4_SKIP_TRIGGER: u32 = 6;
const LZ4_DISTANCE_MAX: usize = 65535;
const LZ4_MAX_INPUT_SIZE: usize = 0x7E00_0000;

pub const fn compress_bound(isize: usize) -> usize {
    if isize > LZ4_MAX_INPUT_SIZE {
        0
    } else {
        isize + isize / 255 + 16
    }
}

pub fn compress_default(src: &[u8], dst: &mut [u8]) -> usize {
    if src.len() > LZ4_MAX_INPUT_SIZE || dst.len() < compress_bound(src.len()) {
        return 0;
    }
    if src.is_empty() {
        dst[0] = 0;
        return 1;
    }
    if src.len() < LZ4_64KLIMIT {
        compress_generic(src, dst, &mut TableU16([0u16; 1 << (LZ4_HASHLOG + 1)]))
    } else {
        compress_generic(src, dst, &mut TableU32([0u32; 1 << LZ4_HASHLOG]))
    }
}

// tableType_t byU16 / byU32 with their LZ4_hashPosition / put / get arms.
trait HashTable {
    const CHECK_DISTANCE: bool;
    fn hash(src: &[u8], pos: usize) -> u32;
    fn get(&self, h: u32) -> usize;
    fn put(&mut self, idx: usize, h: u32);
}

struct TableU16([u16; 1 << (LZ4_HASHLOG + 1)]);
struct TableU32([u32; 1 << LZ4_HASHLOG]);

impl HashTable for TableU16 {
    // Every index reachable below LZ4_64Klimit fits.
    const CHECK_DISTANCE: bool = false;
    #[inline]
    fn hash(src: &[u8], pos: usize) -> u32 {
        read32(src, pos).wrapping_mul(2_654_435_761) >> ((MINMATCH as u32 * 8) - (LZ4_HASHLOG + 1))
    }
    #[inline]
    fn get(&self, h: u32) -> usize {
        self.0[h as usize] as usize
    }
    #[inline]
    fn put(&mut self, idx: usize, h: u32) {
        debug_assert!(idx < 65536);
        self.0[h as usize] = idx as u16;
    }
}

impl HashTable for TableU32 {
    const CHECK_DISTANCE: bool = true;
    #[inline]
    fn hash(src: &[u8], pos: usize) -> u32 {
        let sequence = read64(src, pos);
        ((sequence << 24).wrapping_mul(889_523_592_379) >> (64 - LZ4_HASHLOG)) as u32
    }
    #[inline]
    fn get(&self, h: u32) -> usize {
        self.0[h as usize] as usize
    }
    #[inline]
    fn put(&mut self, idx: usize, h: u32) {
        self.0[h as usize] = idx as u32;
    }
}

#[inline]
fn read32(src: &[u8], pos: usize) -> u32 {
    u32::from_le_bytes(src[pos..pos + 4].try_into().expect("4 bytes"))
}

#[inline]
fn read64(src: &[u8], pos: usize) -> u64 {
    u64::from_le_bytes(src[pos..pos + 8].try_into().expect("8 bytes"))
}

#[inline]
fn count(src: &[u8], p_in: usize, p_match: usize, limit: usize) -> usize {
    let mut n = 0;
    while p_in + n < limit && src[p_in + n] == src[p_match + n] {
        n += 1;
    }
    n
}

// `src` is non-empty and `dst` holds compress_bound(src.len()) bytes, so no
// output check can fire.
fn compress_generic<T: HashTable>(src: &[u8], dst: &mut [u8], table: &mut T) -> usize {
    let input_size = src.len();
    let iend = input_size;
    let mut ip = 0usize;
    let mut anchor = 0usize;
    let mut op = 0usize;

    if input_size >= LZ4_MIN_LENGTH {
        let mflimit_plus_one = iend - MFLIMIT + 1;
        let matchlimit = iend - LASTLITERALS;

        let h = T::hash(src, ip);
        table.put(0, h);
        ip += 1;
        let mut forward_h = T::hash(src, ip);

        'main: loop {
            let mut match_pos;
            let mut token;

            {
                let mut forward_ip = ip;
                let mut step = 1usize;
                let mut search_match_nb: u32 = 1 << LZ4_SKIP_TRIGGER;
                loop {
                    let h = forward_h;
                    let current = forward_ip;
                    let match_index = table.get(h);
                    debug_assert!(match_index <= current);
                    ip = forward_ip;
                    forward_ip += step;
                    step = (search_match_nb >> LZ4_SKIP_TRIGGER) as usize;
                    search_match_nb += 1;

                    if forward_ip > mflimit_plus_one {
                        break 'main;
                    }
                    debug_assert!(ip < mflimit_plus_one);

                    match_pos = match_index;
                    forward_h = T::hash(src, forward_ip);
                    table.put(current, h);

                    debug_assert!(match_index < current);
                    if T::CHECK_DISTANCE && match_index + LZ4_DISTANCE_MAX < current {
                        continue;
                    }
                    debug_assert!(current - match_index <= LZ4_DISTANCE_MAX);

                    if read32(src, match_pos) == read32(src, ip) {
                        break;
                    }
                }
            }

            debug_assert!(ip > anchor);
            if match_pos > 0 && src[ip - 1] == src[match_pos - 1] {
                loop {
                    ip -= 1;
                    match_pos -= 1;
                    if !((ip > anchor) & (match_pos > 0) && src[ip - 1] == src[match_pos - 1]) {
                        break;
                    }
                }
            }

            {
                let lit_length = ip - anchor;
                token = op;
                op += 1;
                if lit_length >= RUN_MASK {
                    let mut len = lit_length - RUN_MASK;
                    dst[token] = (RUN_MASK << ML_BITS) as u8;
                    while len >= 255 {
                        dst[op] = 255;
                        op += 1;
                        len -= 255;
                    }
                    dst[op] = len as u8;
                    op += 1;
                } else {
                    dst[token] = (lit_length << ML_BITS) as u8;
                }
                dst[op..op + lit_length].copy_from_slice(&src[anchor..ip]);
                op += lit_length;
            }

            loop {
                debug_assert!(ip - match_pos <= LZ4_DISTANCE_MAX);
                dst[op..op + 2].copy_from_slice(&((ip - match_pos) as u16).to_le_bytes());
                op += 2;

                {
                    let mut match_code = count(src, ip + MINMATCH, match_pos + MINMATCH, matchlimit);
                    ip += match_code + MINMATCH;

                    if match_code >= ML_MASK {
                        dst[token] += ML_MASK as u8;
                        match_code -= ML_MASK;
                        while match_code >= 4 * 255 {
                            dst[op..op + 4].copy_from_slice(&[255; 4]);
                            op += 4;
                            match_code -= 4 * 255;
                        }
                        for _ in 0..match_code / 255 {
                            dst[op] = 255;
                            op += 1;
                        }
                        dst[op] = (match_code % 255) as u8;
                        op += 1;
                    } else {
                        dst[token] += match_code as u8;
                    }
                }

                anchor = ip;

                if ip >= mflimit_plus_one {
                    break 'main;
                }

                {
                    let h = T::hash(src, ip - 2);
                    table.put(ip - 2, h);
                }

                {
                    let h = T::hash(src, ip);
                    let current = ip;
                    let match_index = table.get(h);
                    debug_assert!(match_index < current);
                    match_pos = match_index;
                    table.put(current, h);
                    if (!T::CHECK_DISTANCE || match_index + LZ4_DISTANCE_MAX >= current)
                        && read32(src, match_pos) == read32(src, ip)
                    {
                        token = op;
                        op += 1;
                        dst[token] = 0;
                        continue;
                    }
                }

                ip += 1;
                forward_h = T::hash(src, ip);
                break;
            }
        }
    }

    {
        let last_run = iend - anchor;
        if last_run >= RUN_MASK {
            let mut accumulator = last_run - RUN_MASK;
            dst[op] = (RUN_MASK << ML_BITS) as u8;
            op += 1;
            while accumulator >= 255 {
                dst[op] = 255;
                op += 1;
                accumulator -= 255;
            }
            dst[op] = accumulator as u8;
            op += 1;
        } else {
            dst[op] = (last_run << ML_BITS) as u8;
            op += 1;
        }
        dst[op..op + last_run].copy_from_slice(&src[anchor..iend]);
        op += last_run;
    }

    op
}

#[cfg(test)]
mod tests {
    use super::{compress_bound, compress_default};

    fn compress(src: &[u8]) -> Vec<u8> {
        let mut dst = vec![0u8; compress_bound(src.len())];
        let n = compress_default(src, &mut dst);
        assert!(n > 0);
        dst.truncate(n);
        dst
    }

    #[test]
    fn matches_liblz4_pinned_vectors() {
        assert_eq!(compress(b""), [0x00]);
        assert_eq!(compress(b"a"), [0x10, b'a']);
        assert_eq!(compress(b"abcdefghijkl"), {
            let mut v = vec![0xc0];
            v.extend_from_slice(b"abcdefghijkl");
            v
        });
        let a5000 = vec![b'a'; 5000];
        assert_eq!(
            compress(&a5000),
            [
                0x1f, 0x61, 0x01, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
                0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x82, 0x50, 0x61, 0x61, 0x61,
                0x61, 0x61
            ]
        );
        assert_eq!(
            compress(b"the quick brown fox jumps over the lazy dog; the quick brown fox jumps over the lazy dog"),
            [
                0xf0, 0x10, 0x74, 0x68, 0x65, 0x20, 0x71, 0x75, 0x69, 0x63, 0x6b, 0x20, 0x62, 0x72,
                0x6f, 0x77, 0x6e, 0x20, 0x66, 0x6f, 0x78, 0x20, 0x6a, 0x75, 0x6d, 0x70, 0x73, 0x20,
                0x6f, 0x76, 0x65, 0x72, 0x20, 0x1f, 0x00, 0x91, 0x6c, 0x61, 0x7a, 0x79, 0x20, 0x64,
                0x6f, 0x67, 0x3b, 0x0e, 0x00, 0x0f, 0x2d, 0x00, 0x0f, 0x50, 0x79, 0x20, 0x64, 0x6f,
                0x67
            ]
        );
    }

    #[test]
    fn roundtrips_through_block_decoder() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for &len in &[0usize, 1, 12, 13, 14, 100, 4096, 65_546, 65_547, 65_560, 200_000] {
            let mut src = Vec::with_capacity(len);
            while src.len() < len {
                let r = next();
                match r % 4 {
                    0 => src.extend(std::iter::repeat((r >> 8) as u8).take((r >> 16) as usize % 300 + 1)),
                    1 => src.extend((0..(r >> 16) as usize % 50 + 1).map(|i| b'a' + ((r >> 8) as u8).wrapping_add(i as u8) % 7)),
                    _ => src.extend_from_slice(&r.to_le_bytes()),
                }
            }
            src.truncate(len);
            let out = compress(&src);
            assert!(out.len() <= compress_bound(len));
            let back = lz4_flex::block::decompress(&out, len).expect("valid block");
            assert_eq!(back, src, "len {len}");
        }
    }
}
