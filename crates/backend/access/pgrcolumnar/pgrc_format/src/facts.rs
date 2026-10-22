//! The planner facts of one manifest generation, persisted: the on-disk
//! form of section 1 of `sections-<gen>.pgrcx` (`crate::sections`), the user's
//! ruling 2026-09-13 (gravity 1c pass 3, item 1): the footer fold is a
//! pure function of a generation, so the publisher writes it beside the
//! manifest and no reader walks parts again — not at plan time, not at
//! server start.
//!
//! The image is COMPLETE for its generation and carries the MERGEABLE
//! accumulator state, not only the served values, so the next publish
//! folds the new parts into it without re-reading the old ones
//! (`pgrc_write::facts::FactsAcc`): per column the NDV register set as
//! merged (the NdvRegisters section body form, `pgrc_meta::ndv::Hll`), the
//! width sums, the data-section bytes, the qual-class row vote, the
//! dictionary election and code count, and — for word-keyed columns — the
//! per-granule `[min, max]` in manifest part order.
//!
//! Layout (little-endian): header
//! `magic "PRCF" u32, version u32, gen u64, publisher_fxid u64,
//! schema_fingerprint u64, rows u64, bytes u64, granules u64, parts u64,
//! ncols u32, pad u32` (72 B); `ncols` column records, attno-ascending:
//! `attno u32, class u8, signed u8, width u8, flags u8, byte_len_sum u64,
//! nonnull u64, char_len_max u32, ndv_len u32, disk_bytes u64, rows_zone
//! u64, rows_bloom u64, rows_dict u64, ncodes u64, nzones u32, zone_len u32`
//! (80 B) then `ndv_len` register-body bytes then `zone_len` bytes of the
//! column's compact zone map (`crate::zonemap`, `nzones` granules; version
//! 2 — version 1 carried `nzones` × `(min i64, max i64)` and is refused, the
//! catalog re-completes such a generation once); trailer `crc32c u32` over
//! everything before it. Identity is
//! the header's `(gen, publisher_fxid, schema_fingerprint)` — the process
//! fact cache's key; a reader that resolved another key refuses the file.

use crate::wire::{crc32c, put_u32, put_u64, Cur};
use crate::zonemap::ZoneMap;
use crate::{FormatError, FormatResult};

pub const FACTS_MAGIC: u32 = u32::from_le_bytes(*b"PRCF");
pub const FACTS_VERSION: u32 = 2;
pub const FACTS_HEADER_LEN: usize = 72;
pub const FACTS_COL_LEN: usize = 80;

/// Column flags.
/// A part lacked the column's NdvRegisters section: NDV unknown for good.
pub const COLF_NDV_ABSENT: u8 = 1 << 0;
/// A part lacked the Stats section or its `STATSF_COMPUTED` witness: the
/// width average and max length are unknown for good.
pub const COLF_WIDTH_POISONED: u8 = 1 << 1;
/// Every part so far elected `DictCodes` for the Values stream.
pub const COLF_DICT_ALL: u8 = 1 << 2;
/// `zones` is live: a word-keyed column whose every part carried the exact
/// key witness (one range per granule so far).
pub const COLF_ZONES: u8 = 1 << 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FactsHeader {
    pub gen: u64,
    pub publisher_fxid: u64,
    pub schema_fingerprint: u64,
    pub rows: u64,
    pub bytes: u64,
    pub granules: u64,
    pub parts: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ColFacts {
    pub attno: u32,
    /// The Values stream's storage class id (`class::CLASS_*`) and
    /// signedness, as every part carries them; `width` is the storage word
    /// width when a publisher (schema in hand) wrote the image, 0 when a
    /// walk did (parts carry only the encoding width).
    pub class: u8,
    pub signed: bool,
    pub width: u8,
    pub flags: u8,
    pub byte_len_sum: u64,
    pub nonnull: u64,
    pub char_len_max: u32,
    pub disk_bytes: u64,
    /// Σ rows of the parts carrying each qual-class witness.
    pub rows_zone: u64,
    pub rows_bloom: u64,
    pub rows_dict: u64,
    pub ncodes: u64,
    /// The merged NdvRegisters section body; empty when absent.
    pub ndv_body: Vec<u8>,
    /// The per-granule `[min, max]` in manifest part order, compact; empty
    /// unless `COLF_ZONES`.
    pub zones: ZoneMap,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FactsImage {
    pub header: FactsHeader,
    pub cols: Vec<ColFacts>,
}

impl FactsImage {
    pub fn encode(&self) -> FormatResult<Vec<u8>> {
        let body: usize = self.cols.iter().map(|c| FACTS_COL_LEN + c.ndv_body.len() + c.zones.bytes().len()).sum();
        let mut out = crate::try_vec(FACTS_HEADER_LEN + body + 4, "facts")?;
        let h = &self.header;
        put_u32(&mut out, FACTS_MAGIC);
        put_u32(&mut out, FACTS_VERSION);
        for v in [h.gen, h.publisher_fxid, h.schema_fingerprint, h.rows, h.bytes, h.granules, h.parts] {
            put_u64(&mut out, v);
        }
        put_u32(&mut out, self.cols.len() as u32);
        put_u32(&mut out, 0);
        debug_assert_eq!(out.len(), FACTS_HEADER_LEN);
        for c in &self.cols {
            put_u32(&mut out, c.attno);
            out.extend_from_slice(&[c.class, u8::from(c.signed), c.width, c.flags]);
            put_u64(&mut out, c.byte_len_sum);
            put_u64(&mut out, c.nonnull);
            put_u32(&mut out, c.char_len_max);
            put_u32(&mut out, c.ndv_body.len() as u32);
            for v in [c.disk_bytes, c.rows_zone, c.rows_bloom, c.rows_dict, c.ncodes] {
                put_u64(&mut out, v);
            }
            put_u32(&mut out, c.zones.granules());
            put_u32(&mut out, c.zones.bytes().len() as u32);
            out.extend_from_slice(&c.ndv_body);
            out.extend_from_slice(c.zones.bytes());
        }
        let crc = crc32c(&out);
        put_u32(&mut out, crc);
        Ok(out)
    }

    /// Decode a whole file image: magic, version, CRC, bounds, column
    /// order — every refusal typed `Corrupt`/`Truncated`.
    pub fn decode(bytes: &[u8]) -> FormatResult<FactsImage> {
        let at = "facts file";
        if bytes.len() < FACTS_HEADER_LEN + 4 {
            return Err(FormatError::Truncated { at });
        }
        let (body, tail) = bytes.split_at(bytes.len() - 4);
        if crc32c(body) != u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]]) {
            return Err(FormatError::CrcMismatch { at });
        }
        let mut c = Cur::new(body);
        if c.u32(at)? != FACTS_MAGIC {
            return Err(FormatError::Corrupt { at: "facts magic" });
        }
        if c.u32(at)? != FACTS_VERSION {
            return Err(FormatError::Corrupt { at: "facts version" });
        }
        let header = FactsHeader {
            gen: c.u64(at)?,
            publisher_fxid: c.u64(at)?,
            schema_fingerprint: c.u64(at)?,
            rows: c.u64(at)?,
            bytes: c.u64(at)?,
            granules: c.u64(at)?,
            parts: c.u64(at)?,
        };
        let ncols = c.u32(at)? as usize;
        c.u32(at)?;
        if c.remaining() < ncols.saturating_mul(FACTS_COL_LEN) {
            return Err(FormatError::Truncated { at: "facts columns" });
        }
        let ca = "facts column";
        let mut cols = crate::try_vec(ncols, "facts")?;
        let mut prev_attno = 0u32;
        for _ in 0..ncols {
            let attno = c.u32(ca)?;
            if attno == 0 || attno <= prev_attno {
                return Err(FormatError::Corrupt { at: "facts column order" });
            }
            prev_attno = attno;
            let class = c.u8(ca)?;
            let signed = match c.u8(ca)? {
                0 => false,
                1 => true,
                _ => return Err(FormatError::Corrupt { at: "facts column signed" }),
            };
            let width = c.u8(ca)?;
            let flags = c.u8(ca)?;
            let byte_len_sum = c.u64(ca)?;
            let nonnull = c.u64(ca)?;
            let char_len_max = c.u32(ca)?;
            let ndv_len = c.u32(ca)? as usize;
            let disk_bytes = c.u64(ca)?;
            let rows_zone = c.u64(ca)?;
            let rows_bloom = c.u64(ca)?;
            let rows_dict = c.u64(ca)?;
            let ncodes = c.u64(ca)?;
            let nzones = c.u32(ca)?;
            let zone_len = c.u32(ca)? as usize;
            if c.remaining() < ndv_len.saturating_add(zone_len) {
                return Err(FormatError::Truncated { at: "facts column body" });
            }
            let ndv_body = crate::try_to_vec(c.take(ndv_len, ca)?, "facts")?;
            let zones = ZoneMap::from_bytes(c.take(zone_len, ca)?, nzones)?;
            if flags & COLF_ZONES != 0 && u64::from(nzones) != header.granules {
                return Err(FormatError::Corrupt { at: "facts zones vs granules" });
            }
            cols.push(ColFacts {
                attno,
                class,
                signed,
                width,
                flags,
                byte_len_sum,
                nonnull,
                char_len_max,
                disk_bytes,
                rows_zone,
                rows_bloom,
                rows_dict,
                ncodes,
                ndv_body,
                zones,
            });
        }
        if c.remaining() != 0 {
            return Err(FormatError::Corrupt { at: "facts trailing bytes" });
        }
        Ok(FactsImage { header, cols })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image() -> FactsImage {
        FactsImage {
            header: FactsHeader { gen: 3, publisher_fxid: 77, schema_fingerprint: 0xfeed, rows: 30, bytes: 4096, granules: 2, parts: 2 },
            cols: vec![
                ColFacts {
                    attno: 1,
                    class: 0,
                    signed: true,
                    width: 8,
                    flags: COLF_ZONES,
                    byte_len_sum: 0,
                    nonnull: 30,
                    char_len_max: 0,
                    disk_bytes: 240,
                    rows_zone: 30,
                    rows_bloom: 30,
                    rows_dict: 0,
                    ncodes: 0,
                    ndv_body: vec![1, 10, 0, 0, 4, 0, 0, 0, 5, 6, 7, 8],
                    zones: ZoneMap::encode(&[(1, 10), (i64::MAX, i64::MIN)]).unwrap(),
                },
                ColFacts {
                    attno: 3,
                    class: 5,
                    signed: false,
                    width: 0,
                    flags: COLF_NDV_ABSENT | COLF_WIDTH_POISONED | COLF_DICT_ALL,
                    byte_len_sum: 0,
                    nonnull: 0,
                    char_len_max: 0,
                    disk_bytes: 3000,
                    rows_zone: 0,
                    rows_bloom: 0,
                    rows_dict: 30,
                    ncodes: 12,
                    ndv_body: Vec::new(),
                    zones: ZoneMap::default(),
                },
            ],
        }
    }

    #[test]
    fn round_trip_and_size() {
        let img = image();
        let bytes = img.encode().unwrap();
        let zmap = img.cols[0].zones.bytes().len() + img.cols[1].zones.bytes().len();
        assert_eq!(bytes.len(), FACTS_HEADER_LEN + 2 * FACTS_COL_LEN + 12 + zmap + 4);
        let back = FactsImage::decode(&bytes).expect("decode");
        assert_eq!(back, img);
        assert_eq!(back.cols[0].zones.decode().unwrap(), vec![(1, 10), (i64::MAX, i64::MIN)]);
    }

    #[test]
    fn refusals_are_typed() {
        let bytes = image().encode().unwrap();
        assert!(matches!(FactsImage::decode(&bytes[..FACTS_HEADER_LEN]), Err(FormatError::Truncated { .. })));
        let mut flipped = bytes.clone();
        flipped[FACTS_HEADER_LEN + 9] ^= 0x40;
        assert!(matches!(FactsImage::decode(&flipped), Err(FormatError::CrcMismatch { .. })));
        let mut magic = bytes.clone();
        magic[0] ^= 1;
        let n = magic.len();
        let crc = crc32c(&magic[..n - 4]).to_le_bytes();
        magic[n - 4..].copy_from_slice(&crc);
        assert!(matches!(FactsImage::decode(&magic), Err(FormatError::Corrupt { at: "facts magic" })));
        let mut out_of_order = image();
        out_of_order.cols.swap(0, 1);
        assert!(matches!(FactsImage::decode(&out_of_order.encode().unwrap()), Err(FormatError::Corrupt { at: "facts column order" })));
        let mut short_zones = image();
        short_zones.cols[0].zones = ZoneMap::encode(&[(1, 10)]).unwrap();
        assert!(matches!(FactsImage::decode(&short_zones.encode().unwrap()), Err(FormatError::Corrupt { at: "facts zones vs granules" })));
        let mut v1 = bytes.clone();
        v1[4..8].copy_from_slice(&1u32.to_le_bytes());
        let n = v1.len();
        let crc = crc32c(&v1[..n - 4]).to_le_bytes();
        v1[n - 4..].copy_from_slice(&crc);
        assert!(matches!(FactsImage::decode(&v1), Err(FormatError::Corrupt { at: "facts version" })), "a version-1 image is refused, never half-read");
    }
}
