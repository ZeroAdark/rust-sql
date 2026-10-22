//! Spec §8.9 (pgrc format 2.1 amendment, kind 14 `Sample`, OPTIONAL): a part's stored uniform row sample, so ANALYZE reads a
//! few hundred rows per part from one section instead of decoding the granules its sampled rows fall in.
//!
//! One section per part (attno 0, path_ord 0), META_ZSTD-wrapped when the wrap wins (the meta-plane rule). The rows
//! are a pure function of the part (spec §1 determinism law): the `n = min(SAMPLE_ROWS, rows)` row ordinals come from
//! [`sample_ordinals`], a Floyd draw over a splitmix stream seeded by [`sample_seed`] (schema fingerprint, row count,
//! a digest of the part's own values: the part number is assigned only at publish) — no RNG output, wall time or host
//! fact reaches the bytes. Every root column (path_ord 0) is stored with
//! the datum the scan decodes for it: a by-value column's datum word, a by-reference column's datum image (Fixed(N):
//! the N bytes; varlena: the 4-B header and payload, spec §1's varlena shape).
//!
//! ```text
//! SampleHdr (16 B): { version: u16 = 1, flags: u16 = 0, n: u32, ncols: u32, reserved: u32 = 0 }
//! ordinals:  [u32; n]    ascending part row ordinals (after the cluster sort: the rows' sealed order)
//! pad to 8
//! coldir:    ncols × 16 B { attno: u32, kind: u8 (0 words / 1 images), pad: [u8; 3], off: u32, len: u32 }
//!            ascending attno; off / len: the column block, body-relative, 8-aligned
//! block:     validity: [u64; ceil(n / 64)]   bit i (LSB first) set = row i is not NULL
//!            kind 0: [u64; n] datum words (NULL: 0)
//!            kind 1: [{ off: u32, len: u32 }; n] (NULL: 0, 0), pad to 8, then the images, each 8-aligned; `off` is
//!                    relative to the image area's start
//! ```
//!
//! Readers validate every count, offset and length against the body (typed `Corrupt` / `Bounds`, never UB). A reader
//! that does not know kind 14 skips it by its `SECTION_OPTIONAL` flag (spec §5.2); a part without it is read by the
//! sampled-granule path, as before the amendment.

use crate::{FormatError, FormatResult};

/// The rows a part stores (at most; a smaller part stores every row).
pub const SAMPLE_ROWS: u32 = 256;
/// The layout version this crate writes and reads.
pub const SAMPLE_VERSION: u16 = 1;
/// Header bytes.
pub const SAMPLE_HDR_LEN: usize = 16;
/// Column directory entry bytes.
pub const SAMPLE_COLDIR_LEN: usize = 16;
/// Column block kind: by-value datum words.
pub const SAMPLE_KIND_WORDS: u8 = 0;
/// Column block kind: by-reference datum images.
pub const SAMPLE_KIND_IMAGES: u8 = 1;

const fn pad8(n: usize) -> usize {
    (n + 7) & !7
}

/// Zero bytes up to the next multiple of 8 (inside the capacity the caller reserved).
fn zero_pad8(v: &mut Vec<u8>) {
    let l = v.len();
    v.extend_from_slice(&[0u8; 8][..pad8(l) - l]);
}

/// The part's draw seed: a pure function of (schema fingerprint, row count, `digest` — the writer's digest of the
/// part's own values, [`sample_digest`]).
pub const fn sample_seed(schema_fingerprint: u64, rows: u64, digest: u64) -> u64 {
    crate::ident::mix64(schema_fingerprint ^ crate::ident::mix64(rows ^ 0x5341_4d50_4c45_0001) ^ crate::ident::mix64(digest))
}

/// Fold one value's bytes (`None` = NULL) into a [`sample_seed`] digest.
pub fn sample_digest(acc: u64, value: Option<&[u8]>) -> u64 {
    let Some(b) = value else { return crate::ident::mix64(acc ^ 0x4e55_4c4c) };
    let mut h = crate::ident::mix64(acc ^ b.len() as u64);
    for ch in b.chunks(8) {
        let mut t = [0u8; 8];
        t[..ch.len()].copy_from_slice(ch);
        h = crate::ident::mix64(h ^ u64::from_le_bytes(t));
    }
    h
}

/// The `min(SAMPLE_ROWS, rows)` ascending row ordinals a part of `rows` rows stores: Floyd's draw (a uniform subset)
/// over a splitmix stream from `seed`; every row when the part has at most [`SAMPLE_ROWS`].
pub fn sample_ordinals(rows: u64, seed: u64) -> FormatResult<Vec<u32>> {
    let n = rows.min(SAMPLE_ROWS as u64);
    let mut out: Vec<u32> = crate::try_vec(n as usize, "sample ordinals")?;
    if rows > u32::MAX as u64 {
        return Err(FormatError::EncodeContract { detail: "sample of a part past u32 rows" });
    }
    if n == rows {
        out.extend(0..rows as u32);
        return Ok(out);
    }
    let mut state = seed;
    // Floyd: for j in rows-n .. rows, take t uniform in [0, j]; t if new, else j. `out` stays sorted (n <= 256).
    for j in (rows - n)..rows {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let r = crate::ident::mix64(state);
        // a uniform t in [0, j] by the 128-bit multiply-shift (bias below 2^-31 at j < 2^32)
        let t = ((r as u128 * (j as u128 + 1)) >> 64) as u32;
        let pick = match out.binary_search(&t) {
            Ok(_) => j as u32,
            Err(_) => t,
        };
        let at = out.binary_search(&pick).unwrap_or_else(|e| e);
        out.insert(at, pick);
    }
    Ok(out)
}

/// One column's sampled values for [`SampleWriter`]: by-value words or by-reference images (`None` = NULL).
pub enum SampleValues<'a> {
    Words(&'a [Option<u64>]),
    Images(&'a [Option<&'a [u8]>]),
}

/// Builds a Sample section body: the ordinals, then one column at a time (ascending attno).
pub struct SampleWriter {
    n: usize,
    ordinals: Vec<u32>,
    cols: Vec<(u32, u8, Vec<u8>)>,
}

impl SampleWriter {
    pub fn new(ordinals: Vec<u32>) -> SampleWriter {
        SampleWriter { n: ordinals.len(), ordinals, cols: Vec::new() }
    }

    /// Add column `attno` (ascending across calls) with one value per stored row.
    pub fn add(&mut self, attno: u32, values: SampleValues<'_>) -> FormatResult<()> {
        if self.cols.last().is_some_and(|c| c.0 >= attno) {
            return Err(FormatError::EncodeContract { detail: "sample columns out of attno order" });
        }
        let n = self.n;
        let vwords = n.div_ceil(64);
        let mut block: Vec<u8> = Vec::new();
        let (kind, len) = match &values {
            SampleValues::Words(v) => (SAMPLE_KIND_WORDS, v.len()),
            SampleValues::Images(v) => (SAMPLE_KIND_IMAGES, v.len()),
        };
        if len != n {
            return Err(FormatError::EncodeContract { detail: "sample column length differs from its rows" });
        }
        let mut validity: Vec<u64> = crate::try_filled(vwords, 0u64, "sample validity")?;
        let valid = |i: usize| match &values {
            SampleValues::Words(v) => v[i].is_some(),
            SampleValues::Images(v) => v[i].is_some(),
        };
        for (i, w) in (0..n).map(|i| (i, valid(i))) {
            if w {
                validity[i / 64] |= 1 << (i % 64);
            }
        }
        let image_bytes: usize = match &values {
            SampleValues::Words(_) => 0,
            SampleValues::Images(v) => v.iter().flatten().map(|b| pad8(b.len())).sum(),
        };
        block
            .try_reserve_exact(vwords * 8 + n * 8 + image_bytes)
            .map_err(|_| FormatError::OutOfMemory { bytes: vwords * 8 + n * 8 + image_bytes })?;
        for w in &validity {
            block.extend_from_slice(&w.to_le_bytes());
        }
        match values {
            SampleValues::Words(v) => {
                for w in v {
                    block.extend_from_slice(&w.unwrap_or(0).to_le_bytes());
                }
            }
            SampleValues::Images(v) => {
                let mut at = 0usize;
                for b in v.iter() {
                    let (off, l) = match b {
                        Some(b) => {
                            let o = at;
                            at += pad8(b.len());
                            (o, b.len())
                        }
                        None => (0, 0),
                    };
                    if off > u32::MAX as usize || l > u32::MAX as usize {
                        return Err(FormatError::EncodeContract { detail: "sample image past u32" });
                    }
                    block.extend_from_slice(&(off as u32).to_le_bytes());
                    block.extend_from_slice(&(l as u32).to_le_bytes());
                }
                for b in v.iter().flatten() {
                    block.extend_from_slice(b);
                    zero_pad8(&mut block);
                }
            }
        }
        self.cols.push((attno, kind, block));
        Ok(())
    }

    /// The section body.
    pub fn finish(self) -> FormatResult<Vec<u8>> {
        let n = self.n;
        let dir_at = pad8(SAMPLE_HDR_LEN + 4 * n);
        let mut at = dir_at + SAMPLE_COLDIR_LEN * self.cols.len();
        let total = at + self.cols.iter().map(|c| pad8(c.2.len())).sum::<usize>();
        if total > u32::MAX as usize {
            return Err(FormatError::EncodeContract { detail: "sample section past u32" });
        }
        let mut out: Vec<u8> = crate::try_vec(total, "sample body")?;
        out.extend_from_slice(&SAMPLE_VERSION.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(n as u32).to_le_bytes());
        out.extend_from_slice(&(self.cols.len() as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        for o in &self.ordinals {
            out.extend_from_slice(&o.to_le_bytes());
        }
        zero_pad8(&mut out);
        debug_assert_eq!(out.len(), dir_at);
        for (attno, kind, block) in &self.cols {
            out.extend_from_slice(&attno.to_le_bytes());
            out.extend_from_slice(&[*kind, 0, 0, 0]);
            out.extend_from_slice(&(at as u32).to_le_bytes());
            out.extend_from_slice(&(block.len() as u32).to_le_bytes());
            at += pad8(block.len());
        }
        for (_, _, block) in &self.cols {
            out.extend_from_slice(block);
            zero_pad8(&mut out);
        }
        debug_assert_eq!(out.len(), total);
        Ok(out)
    }
}

fn u32_at(b: &[u8], at: usize, what: &'static str) -> FormatResult<u32> {
    b.get(at..at + 4)
        .map(|s| u32::from_le_bytes(s.try_into().expect("len 4")))
        .ok_or(FormatError::Bounds { at: what })
}

fn u64_at(b: &[u8], at: usize, what: &'static str) -> FormatResult<u64> {
    b.get(at..at + 8)
        .map(|s| u64::from_le_bytes(s.try_into().expect("len 8")))
        .ok_or(FormatError::Bounds { at: what })
}

/// A validated view of a Sample section body.
pub struct SampleRef<'a> {
    body: &'a [u8],
    n: usize,
    ncols: usize,
    dir_at: usize,
}

/// One column of a [`SampleRef`].
pub struct SampleColRef<'a> {
    n: usize,
    kind: u8,
    block: &'a [u8],
}

impl<'a> SampleRef<'a> {
    pub fn parse(body: &'a [u8]) -> FormatResult<SampleRef<'a>> {
        if body.len() < SAMPLE_HDR_LEN {
            return Err(FormatError::Truncated { at: "sample header" });
        }
        let version = u16::from_le_bytes([body[0], body[1]]);
        if version != SAMPLE_VERSION {
            return Err(FormatError::BadVersion { at: "sample header", got: version as u32 });
        }
        let n = u32_at(body, 4, "sample header")? as usize;
        let ncols = u32_at(body, 8, "sample header")? as usize;
        if n > SAMPLE_ROWS as usize {
            return Err(FormatError::Corrupt { at: "sample row count" });
        }
        let dir_at = pad8(SAMPLE_HDR_LEN + 4 * n);
        let dir_end = ncols
            .checked_mul(SAMPLE_COLDIR_LEN)
            .and_then(|d| d.checked_add(dir_at))
            .ok_or(FormatError::Corrupt { at: "sample column count" })?;
        if dir_end > body.len() {
            return Err(FormatError::Truncated { at: "sample column directory" });
        }
        let s = SampleRef { body, n, ncols, dir_at };
        let mut prev: Option<u32> = None;
        for i in 0..n {
            let o = u32_at(body, SAMPLE_HDR_LEN + 4 * i, "sample ordinals")?;
            if prev.is_some_and(|p| p >= o) {
                return Err(FormatError::Corrupt { at: "sample ordinals not ascending" });
            }
            prev = Some(o);
        }
        let mut pa: Option<u32> = None;
        for c in 0..ncols {
            let at = dir_at + c * SAMPLE_COLDIR_LEN;
            let attno = u32_at(body, at, "sample column directory")?;
            if pa.is_some_and(|p| p >= attno) {
                return Err(FormatError::Corrupt { at: "sample columns not ascending" });
            }
            pa = Some(attno);
            s.col_at(c)?;
        }
        Ok(s)
    }

    /// Rows stored.
    pub fn n(&self) -> usize {
        self.n
    }

    /// Stored row `i`'s part row ordinal.
    pub fn ordinal(&self, i: usize) -> u32 {
        u32::from_le_bytes(self.body[SAMPLE_HDR_LEN + 4 * i..SAMPLE_HDR_LEN + 4 * i + 4].try_into().expect("len 4"))
    }

    /// Column `attno`'s values (`None`: the part stores no such column).
    pub fn col(&self, attno: u32) -> FormatResult<Option<SampleColRef<'a>>> {
        for c in 0..self.ncols {
            if u32_at(self.body, self.dir_at + c * SAMPLE_COLDIR_LEN, "sample column directory")? == attno {
                return self.col_at(c).map(Some);
            }
        }
        Ok(None)
    }

    fn col_at(&self, c: usize) -> FormatResult<SampleColRef<'a>> {
        let at = self.dir_at + c * SAMPLE_COLDIR_LEN;
        let kind = *self.body.get(at + 4).ok_or(FormatError::Bounds { at: "sample column directory" })?;
        let off = u32_at(self.body, at + 8, "sample column directory")? as usize;
        let len = u32_at(self.body, at + 12, "sample column directory")? as usize;
        let block = off
            .checked_add(len)
            .and_then(|e| self.body.get(off..e))
            .ok_or(FormatError::Bounds { at: "sample column block" })?;
        let n = self.n;
        let fixed = n.div_ceil(64) * 8 + n * 8;
        if block.len() < fixed {
            return Err(FormatError::Truncated { at: "sample column block" });
        }
        let col = SampleColRef { n, kind, block };
        match kind {
            SAMPLE_KIND_WORDS => {
                if block.len() != fixed {
                    return Err(FormatError::Corrupt { at: "sample word block length" });
                }
            }
            SAMPLE_KIND_IMAGES => {
                let area = pad8(fixed);
                for i in 0..n {
                    if col.valid(i) {
                        let (o, l) = col.span(i)?;
                        let s = area.checked_add(o as usize).ok_or(FormatError::Bounds { at: "sample image" })?;
                        if s.checked_add(l as usize).is_none_or(|e| e > block.len()) {
                            return Err(FormatError::Bounds { at: "sample image" });
                        }
                    }
                }
            }
            _ => return Err(FormatError::Corrupt { at: "sample column kind" }),
        }
        Ok(col)
    }
}

impl<'a> SampleColRef<'a> {
    /// By-reference images (else by-value words).
    pub fn byref(&self) -> bool {
        self.kind == SAMPLE_KIND_IMAGES
    }

    /// Row `i` is not NULL.
    pub fn valid(&self, i: usize) -> bool {
        let w = u64::from_le_bytes(self.block[(i / 64) * 8..(i / 64) * 8 + 8].try_into().expect("len 8"));
        (w >> (i % 64)) & 1 == 1
    }

    fn span(&self, i: usize) -> FormatResult<(u32, u32)> {
        let at = self.n.div_ceil(64) * 8 + 8 * i;
        Ok((u32_at(self.block, at, "sample image span")?, u32_at(self.block, at + 4, "sample image span")?))
    }

    /// Row `i`'s datum word (a words column).
    pub fn word(&self, i: usize) -> FormatResult<u64> {
        u64_at(self.block, self.n.div_ceil(64) * 8 + 8 * i, "sample word")
    }

    /// Row `i`'s datum image (an images column; validated at parse).
    pub fn image(&self, i: usize) -> FormatResult<&'a [u8]> {
        let (o, l) = self.span(i)?;
        let s = pad8(self.n.div_ceil(64) * 8 + self.n * 8) + o as usize;
        self.block.get(s..s + l as usize).ok_or(FormatError::Bounds { at: "sample image" })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinals_are_a_sorted_deterministic_subset() {
        for rows in [1u64, 255, 256, 257, 8192, 195_695, 1 << 31] {
            let a = sample_ordinals(rows, sample_seed(7, rows, 3)).unwrap();
            let b = sample_ordinals(rows, sample_seed(7, rows, 3)).unwrap();
            assert_eq!(a, b);
            assert_eq!(a.len() as u64, rows.min(SAMPLE_ROWS as u64));
            assert!(a.windows(2).all(|w| w[0] < w[1]));
            assert!(a.last().is_some_and(|&o| (o as u64) < rows));
        }
        assert_ne!(sample_ordinals(100_000, sample_seed(7, 100_000, 3)).unwrap(), sample_ordinals(100_000, sample_seed(7, 100_000, 4)).unwrap());
        // every row of a large part is reachable: the mean ordinal sits near the middle
        let a = sample_ordinals(1_000_000, sample_seed(1, 1_000_000, 1)).unwrap();
        let mean = a.iter().map(|&o| o as f64).sum::<f64>() / a.len() as f64;
        assert!((mean / 1e6 - 0.5).abs() < 0.08, "{mean}");
    }

    #[test]
    fn a_body_round_trips_and_pins_its_bytes() {
        let ord = sample_ordinals(1000, sample_seed(11, 1000, 2)).unwrap();
        let n = ord.len();
        let words: Vec<Option<u64>> = (0..n).map(|i| (i % 5 != 0).then_some(i as u64 * 3)).collect();
        let imgs_own: Vec<Vec<u8>> = (0..n).map(|i| vec![i as u8; i % 13]).collect();
        let imgs: Vec<Option<&[u8]>> = (0..n).map(|i| (i % 7 != 0).then_some(imgs_own[i].as_slice())).collect();
        let mut w = SampleWriter::new(ord.clone());
        w.add(1, SampleValues::Words(&words)).unwrap();
        w.add(3, SampleValues::Images(&imgs)).unwrap();
        let body = w.finish().unwrap();
        let s = SampleRef::parse(&body).unwrap();
        assert_eq!(s.n(), n);
        assert!((0..n).all(|i| s.ordinal(i) == ord[i]));
        let c1 = s.col(1).unwrap().unwrap();
        let c3 = s.col(3).unwrap().unwrap();
        assert!(s.col(2).unwrap().is_none());
        assert!(!c1.byref() && c3.byref());
        for i in 0..n {
            assert_eq!(c1.valid(i), words[i].is_some());
            if let Some(v) = words[i] {
                assert_eq!(c1.word(i).unwrap(), v);
            }
            assert_eq!(c3.valid(i), imgs[i].is_some());
            if let Some(b) = imgs[i] {
                assert_eq!(c3.image(i).unwrap(), b);
            }
        }
        // the layout is pinned: a changed byte order or padding moves this digest
        let digest = body.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3));
        assert_eq!((body.len(), digest), SAMPLE_BODY_GOLDEN);
    }

    const SAMPLE_BODY_GOLDEN: (usize, u64) = (7368, 16115985309636396166);

    #[test]
    fn a_damaged_body_is_refused_typed() {
        let ord = sample_ordinals(300, 9).unwrap();
        let n = ord.len();
        let words: Vec<Option<u64>> = (0..n).map(|i| Some(i as u64)).collect();
        let mut w = SampleWriter::new(ord);
        w.add(1, SampleValues::Words(&words)).unwrap();
        let body = w.finish().unwrap();
        assert!(SampleRef::parse(&body[..body.len() - 8]).is_err(), "truncated block");
        let mut b = body.clone();
        b[0] = 9;
        assert!(matches!(SampleRef::parse(&b), Err(FormatError::BadVersion { .. })));
        let mut b = body.clone();
        b[4..8].copy_from_slice(&1000u32.to_le_bytes());
        assert!(SampleRef::parse(&b).is_err(), "row count past the cap");
        let mut b = body.clone();
        b[SAMPLE_HDR_LEN..SAMPLE_HDR_LEN + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(SampleRef::parse(&b), Err(FormatError::Corrupt { .. })), "ordinals out of order");
        let mut w = SampleWriter::new(vec![0, 1]);
        w.add(2, SampleValues::Words(&[Some(1), None])).unwrap();
        assert!(w.add(2, SampleValues::Words(&[Some(1), None])).is_err(), "attno order");
    }
}
