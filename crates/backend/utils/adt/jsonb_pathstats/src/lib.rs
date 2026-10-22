//! pgrust-only: the per-path jsonb statistics slot (stakind in PostgreSQL's private range, so pg_stats,
//! pg_dump --statistics and pg_upgrade drop it; ANALYZE rebuilds it). stavalues = one bytea, little-endian:
//! header (HEADER_LEN): "JPS1", flags, generation (FNV-1a 64 over bytes 16..), samplerows, nonnull_rows,
//! analyzed_rows (documents walked), npaths, totalrows f64, untracked_frac f64 (presence bound of a path with
//! no record, a fraction of analyzed_rows), doc_cap, truncated_docs; then records sorted by key: key, the
//! document counts present / json_null / bool / number / string / array / object, ndistinct f32 (stadistinct
//! convention over the `->>` texts), avgwidth, MCVs (count, `->>` text), hist_kind + bounds. The empty key is
//! the document root (object / array counts of root containers; a root scalar counts as a string).

#![forbid(unsafe_code)]

pub const STATISTIC_KIND_JSONB_PATHS: i16 = 13802;

pub const MAGIC: &[u8; 4] = b"JPS1";
pub const HEADER_LEN: usize = 56;

/// A path without a record was seen in no walked document.
pub const FLAG_COMPLETE: u32 = 1;
/// Some documents were walked only up to doc_cap or not at all (byte cap).
pub const FLAG_PARTIAL_DOCS: u32 = 2;

pub const HIST_NONE: u8 = 0;
pub const HIST_NUMERIC: u8 = 1;
/// Byte-order bounds, kept only when every string value starts with a digit.
pub const HIST_BYTES: u8 = 2;

/// `/` then the segment with `\` and `/` escaped: `["a"]`, `["a", ""]`, `["a/b"]` -> `/a`, `/a/`, `/a\/b`.
pub fn push_segment(out: &mut Vec<u8>, seg: &[u8]) {
    out.push(b'/');
    for &b in seg {
        if b == b'\\' || b == b'/' {
            out.push(b'\\');
        }
        out.push(b);
    }
}

pub fn encode_path<'a>(segs: impl IntoIterator<Item = &'a [u8]>) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    for s in segs {
        push_segment(&mut out, s);
    }
    out
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub present: u32,
    pub json_null: u32,
    pub boolean: u32,
    pub number: u32,
    pub string: u32,
    pub array: u32,
    pub object: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Header {
    pub flags: u32,
    pub generation: u64,
    pub samplerows: u32,
    pub nonnull_rows: u32,
    pub analyzed_rows: u32,
    pub npaths: u32,
    pub totalrows: f64,
    pub untracked_frac: f64,
    pub doc_cap: u32,
    pub truncated_docs: u32,
}

pub fn record_len(key: &[u8], mcv_lens: impl Iterator<Item = usize>, bound_lens: impl Iterator<Item = usize>) -> usize {
    2 + key.len() + 7 * 4 + 4 + 4 + 2 + mcv_lens.map(|l| 6 + l).sum::<usize>() + 1 + 2
        + bound_lens.map(|l| 2 + l).sum::<usize>()
}

/// Builds a blob; records must be added in strictly increasing key order.
pub struct Writer {
    buf: Vec<u8>,
    npaths: u32,
}

impl Writer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        flags: u32,
        samplerows: u32,
        nonnull_rows: u32,
        analyzed_rows: u32,
        totalrows: f64,
        untracked_frac: f64,
        doc_cap: u32,
        truncated_docs: u32,
    ) -> Writer {
        let mut buf = Vec::with_capacity(4096);
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&flags.to_le_bytes());
        buf.extend_from_slice(&0u64.to_le_bytes()); // generation, set by finish
        buf.extend_from_slice(&samplerows.to_le_bytes());
        buf.extend_from_slice(&nonnull_rows.to_le_bytes());
        buf.extend_from_slice(&analyzed_rows.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes()); // npaths, set by finish
        buf.extend_from_slice(&totalrows.to_le_bytes());
        buf.extend_from_slice(&untracked_frac.to_le_bytes());
        buf.extend_from_slice(&doc_cap.to_le_bytes());
        buf.extend_from_slice(&truncated_docs.to_le_bytes());
        debug_assert_eq!(buf.len(), HEADER_LEN);
        Writer { buf, npaths: 0 }
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.npaths == 0
    }

    /// Keys, MCV texts and bounds over u16::MAX bytes are the caller's to drop.
    pub fn path(
        &mut self,
        key: &[u8],
        c: &Counts,
        ndistinct: f32,
        avgwidth: i32,
        mcvs: &[(u32, &[u8])],
        hist_kind: u8,
        bounds: &[&[u8]],
    ) {
        let b = &mut self.buf;
        b.extend_from_slice(&(key.len() as u16).to_le_bytes());
        b.extend_from_slice(key);
        for v in [c.present, c.json_null, c.boolean, c.number, c.string, c.array, c.object] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&ndistinct.to_le_bytes());
        b.extend_from_slice(&avgwidth.to_le_bytes());
        b.extend_from_slice(&(mcvs.len() as u16).to_le_bytes());
        for &(cnt, v) in mcvs {
            b.extend_from_slice(&cnt.to_le_bytes());
            b.extend_from_slice(&(v.len() as u16).to_le_bytes());
            b.extend_from_slice(v);
        }
        let kind = if bounds.len() >= 2 { hist_kind } else { HIST_NONE };
        b.push(kind);
        let nb = if kind == HIST_NONE { 0 } else { bounds.len() };
        b.extend_from_slice(&(nb as u16).to_le_bytes());
        for v in &bounds[..nb] {
            b.extend_from_slice(&(v.len() as u16).to_le_bytes());
            b.extend_from_slice(v);
        }
        self.npaths += 1;
    }

    pub fn finish(mut self) -> Vec<u8> {
        self.buf[28..32].copy_from_slice(&self.npaths.to_le_bytes());
        let g = fnv1a64(&self.buf[16..]);
        self.buf[8..16].copy_from_slice(&g.to_le_bytes());
        self.buf
    }
}

fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?))
}
fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}
fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(off..off + 8)?.try_into().ok()?))
}

pub fn header(blob: &[u8]) -> Option<Header> {
    if blob.len() < HEADER_LEN || &blob[0..4] != MAGIC {
        return None;
    }
    Some(Header {
        flags: u32_at(blob, 4)?,
        generation: u64_at(blob, 8)?,
        samplerows: u32_at(blob, 16)?,
        nonnull_rows: u32_at(blob, 20)?,
        analyzed_rows: u32_at(blob, 24)?,
        npaths: u32_at(blob, 28)?,
        totalrows: f64::from_bits(u64_at(blob, 32)?),
        untracked_frac: f64::from_bits(u64_at(blob, 40)?),
        doc_cap: u32_at(blob, 48)?,
        truncated_docs: u32_at(blob, 52)?,
    })
}

#[derive(Clone, Copy, Debug)]
pub struct Record<'a> {
    pub key: &'a [u8],
    pub counts: Counts,
    pub ndistinct: f32,
    pub avgwidth: i32,
    nmcv: u16,
    mcv_off: usize,
    pub hist_kind: u8,
    nbounds: u16,
    hist_off: usize,
    blob: &'a [u8],
}

impl<'a> Record<'a> {
    pub fn mcvs(&self) -> impl Iterator<Item = (u32, &'a [u8])> + 'a {
        let blob = self.blob;
        let mut off = self.mcv_off;
        (0..self.nmcv).map(move |_| {
            let cnt = u32_at(blob, off).unwrap_or(0);
            let len = u16_at(blob, off + 4).unwrap_or(0) as usize;
            let v = &blob[off + 6..off + 6 + len];
            off += 6 + len;
            (cnt, v)
        })
    }

    pub fn nmcv(&self) -> usize {
        self.nmcv as usize
    }

    pub fn bounds(&self) -> impl Iterator<Item = &'a [u8]> + 'a {
        let blob = self.blob;
        let mut off = self.hist_off;
        (0..self.nbounds).map(move |_| {
            let len = u16_at(blob, off).unwrap_or(0) as usize;
            let v = &blob[off + 2..off + 2 + len];
            off += 2 + len;
            v
        })
    }

    pub fn nbounds(&self) -> usize {
        self.nbounds as usize
    }
}

/// Every length is bounds-checked: a damaged blob decodes to None, never a panic.
fn record_at(blob: &[u8], off: usize) -> Option<(Record<'_>, usize)> {
    let klen = u16_at(blob, off)? as usize;
    let key = blob.get(off + 2..off + 2 + klen)?;
    let mut p = off + 2 + klen;
    let mut c = [0u32; 7];
    for v in c.iter_mut() {
        *v = u32_at(blob, p)?;
        p += 4;
    }
    let ndistinct = f32::from_bits(u32_at(blob, p)?);
    let avgwidth = u32_at(blob, p + 4)? as i32;
    p += 8;
    let nmcv = u16_at(blob, p)?;
    p += 2;
    let mcv_off = p;
    for _ in 0..nmcv {
        let len = u16_at(blob, p + 4)? as usize;
        p += 6 + len;
    }
    let hist_kind = *blob.get(p)?;
    let nbounds = u16_at(blob, p + 1)?;
    p += 3;
    let hist_off = p;
    for _ in 0..nbounds {
        let len = u16_at(blob, p)? as usize;
        p += 2 + len;
    }
    if p > blob.len() {
        return None;
    }
    let counts = Counts {
        present: c[0],
        json_null: c[1],
        boolean: c[2],
        number: c[3],
        string: c[4],
        array: c[5],
        object: c[6],
    };
    Some((
        Record { key, counts, ndistinct, avgwidth, nmcv, mcv_off, hist_kind, nbounds, hist_off, blob },
        p,
    ))
}

pub fn index(blob: &[u8]) -> Option<Vec<u32>> {
    let h = header(blob)?;
    let mut offs = Vec::new();
    offs.try_reserve_exact(h.npaths as usize).ok()?;
    let mut off = HEADER_LEN;
    for _ in 0..h.npaths {
        offs.push(off as u32);
        off = record_at(blob, off)?.1;
    }
    (off == blob.len()).then_some(offs)
}

pub fn record(blob: &[u8], off: u32) -> Option<Record<'_>> {
    record_at(blob, off as usize).map(|(r, _)| r)
}

pub fn find<'a>(blob: &'a [u8], offs: &[u32], key: &[u8]) -> Option<Record<'a>> {
    let mut lo = 0usize;
    let mut hi = offs.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        let off = offs[mid] as usize;
        let klen = u16_at(blob, off)? as usize;
        let k = blob.get(off + 2..off + 2 + klen)?;
        match k.cmp(key) {
            core::cmp::Ordering::Less => lo = mid + 1,
            core::cmp::Ordering::Greater => hi = mid,
            core::cmp::Ordering::Equal => return record(blob, offs[mid]),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<u8> {
        let mut w = Writer::new(FLAG_COMPLETE, 100, 90, 90, 1000.0, 0.0, 128, 0);
        let c = Counts { present: 90, string: 90, ..Counts::default() };
        w.path(b"/a", &c, 3.0, 9, &[(60, b"x"), (20, b"yy")], HIST_NONE, &[]);
        let n = Counts { present: 40, number: 40, ..Counts::default() };
        w.path(b"/a/b", &n, -0.4, 6, &[], HIST_NUMERIC, &[b"1", b"5", b"9"]);
        w.path(b"/c", &Counts { present: 1, json_null: 1, ..Counts::default() }, 0.0, 0, &[], HIST_NUMERIC, &[b"1"]);
        w.finish()
    }

    #[test]
    fn round_trip_and_find() {
        let b = sample();
        let h = header(&b).unwrap();
        assert_eq!((h.npaths, h.samplerows, h.nonnull_rows, h.analyzed_rows), (3, 100, 90, 90));
        assert_eq!(h.flags, FLAG_COMPLETE);
        let offs = index(&b).unwrap();
        let r = find(&b, &offs, b"/a").unwrap();
        assert_eq!(r.counts.present, 90);
        assert_eq!(r.mcvs().collect::<Vec<_>>(), vec![(60, &b"x"[..]), (20, &b"yy"[..])]);
        assert_eq!(r.nbounds(), 0);
        let r = find(&b, &offs, b"/a/b").unwrap();
        assert_eq!(r.hist_kind, HIST_NUMERIC);
        assert_eq!(r.bounds().collect::<Vec<_>>(), vec![&b"1"[..], b"5", b"9"]);
        assert_eq!(r.ndistinct, -0.4);
        let r = find(&b, &offs, b"/c").unwrap();
        assert_eq!((r.hist_kind, r.nbounds()), (HIST_NONE, 0));
        assert!(find(&b, &offs, b"/b").is_none());
        assert!(find(&b, &offs, b"/a/b/c").is_none());
        assert!(find(&b, &offs, b"").is_none());
    }

    #[test]
    fn root_record_sorts_first() {
        let mut w = Writer::new(0, 10, 10, 10, 10.0, 0.1, 128, 0);
        w.path(b"", &Counts { present: 10, object: 7, array: 3, ..Counts::default() }, 0.0, 0, &[], HIST_NONE, &[]);
        w.path(b"/a", &Counts { present: 7, ..Counts::default() }, 0.0, 0, &[], HIST_NONE, &[]);
        let b = w.finish();
        let offs = index(&b).unwrap();
        assert_eq!(offs.len(), 2);
        let r = find(&b, &offs, b"").unwrap();
        assert_eq!((r.counts.present, r.counts.object, r.counts.array), (10, 7, 3));
        assert_eq!(find(&b, &offs, b"/a").unwrap().counts.present, 7);
        assert!(find(&b, &offs, b"/").is_none());
    }

    #[test]
    fn generation_is_the_content() {
        let a = sample();
        let b = sample();
        assert_eq!(header(&a).unwrap().generation, header(&b).unwrap().generation);
        let mut w = Writer::new(FLAG_COMPLETE, 100, 90, 90, 1001.0, 0.0, 128, 0);
        w.path(b"/a", &Counts::default(), 0.0, 0, &[], HIST_NONE, &[]);
        assert_ne!(header(&w.finish()).unwrap().generation, header(&a).unwrap().generation);
    }

    #[test]
    fn damaged_blobs_decode_to_none() {
        let b = sample();
        for cut in [0, 10, HEADER_LEN, HEADER_LEN + 3, b.len() - 1] {
            assert!(index(&b[..cut]).is_none(), "cut {cut}");
        }
        let mut bad = b.clone();
        bad[0] = b'X';
        assert!(header(&bad).is_none());
        let mut long = b.clone();
        long.push(0);
        assert!(index(&long).is_none());
    }

    #[test]
    fn segments_are_unambiguous() {
        assert_eq!(encode_path([&b"a"[..]]), b"/a");
        assert_eq!(encode_path([&b"a"[..], b""]), b"/a/");
        assert_eq!(encode_path([&b"a/b"[..]]), b"/a\\/b");
        assert_eq!(encode_path([&b"a\\"[..], b"b"]), b"/a\\\\/b");
        assert_ne!(encode_path([&b"a"[..], b"b"]), encode_path([&b"a/b"[..]]));
        assert_eq!(encode_path(std::iter::empty()), b"");
    }

    #[test]
    fn record_len_matches_the_writer() {
        let mut w = Writer::new(0, 1, 1, 1, 1.0, 0.5, 1, 0);
        let before = w.len();
        w.path(b"/k", &Counts::default(), 1.0, 4, &[(2, b"abc")], HIST_BYTES, &[b"1", b"22"]);
        assert_eq!(w.len() - before, record_len(b"/k", [3usize].into_iter(), [1usize, 2].into_iter()));
    }
}
