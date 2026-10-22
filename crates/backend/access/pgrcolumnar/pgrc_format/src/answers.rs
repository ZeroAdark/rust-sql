//! The part-grain answers of one column (`sections::SECTION_ANSWERS`, one
//! per column): every part's part-grain §8.1 Stats record (80 B, the body's
//! last record — count, sum, min / max, the zone verdict at part grain) and
//! its §8.6 PartDigest record, contiguous — what the footer-answered
//! classes read, served from memory by the catalog. The granule-grain
//! records stay in the part's Stats section (read when a part goes granule
//! by granule).
//!
//! ```text
//! offtab  u64 × (parts+1)   offsets into the record blob (monotone; last = blob length)
//! record blob               the parts' records in manifest order (empty = none)
//! digests [u8; 24] × parts  PartDigest bytes; all-zero = absent
//! ```

use crate::wire::put_u64;
use crate::{FormatError, FormatResult};

pub const DIGEST_LEN: usize = 24;

pub fn encode_column(bodies: &[&[u8]], digests: &[Option<[u8; DIGEST_LEN]>]) -> FormatResult<Vec<u8>> {
    debug_assert_eq!(bodies.len(), digests.len());
    let blob: usize = bodies.iter().map(|b| b.len()).sum();
    let mut out = crate::try_vec(8 * (bodies.len() + 1) + blob + DIGEST_LEN * bodies.len(), "answers")?;
    let mut rel = 0u64;
    for b in bodies {
        put_u64(&mut out, rel);
        rel += b.len() as u64;
    }
    put_u64(&mut out, rel);
    for b in bodies {
        out.extend_from_slice(b);
    }
    for d in digests {
        out.extend_from_slice(&d.unwrap_or([0; DIGEST_LEN]));
    }
    Ok(out)
}

/// A validated view over one column's payload for `parts` parts.
#[derive(Clone, Copy)]
pub struct ColumnAnswers<'a> {
    offtab: &'a [u8],
    stats: &'a [u8],
    digests: &'a [u8],
    parts: usize,
}

impl<'a> ColumnAnswers<'a> {
    pub fn parse(payload: &'a [u8], parts: usize) -> FormatResult<ColumnAnswers<'a>> {
        let at = "answers column";
        let ot = 8 * (parts + 1);
        let dg = DIGEST_LEN * parts;
        if payload.len() < ot + dg {
            return Err(FormatError::Truncated { at });
        }
        let (offtab, rest) = payload.split_at(ot);
        let stats_len = rest.len() - dg;
        let (stats, digests) = rest.split_at(stats_len);
        let mut prev = 0u64;
        for i in 0..=parts {
            let v = u64::from_le_bytes(offtab[i * 8..i * 8 + 8].try_into().expect("len 8"));
            if v < prev || v > stats_len as u64 {
                return Err(FormatError::Corrupt { at });
            }
            prev = v;
        }
        if prev != stats_len as u64 {
            return Err(FormatError::Corrupt { at });
        }
        Ok(ColumnAnswers { offtab, stats, digests, parts })
    }

    /// The view over a payload `parse` already accepted for `parts` (the offset walk skipped: the
    /// per-lookup face of a validated column). A payload `parse` rejected panics on its first read.
    pub fn validated(payload: &'a [u8], parts: usize) -> ColumnAnswers<'a> {
        let ot = 8 * (parts + 1);
        let (offtab, rest) = payload.split_at(ot);
        let (stats, digests) = rest.split_at(rest.len() - DIGEST_LEN * parts);
        ColumnAnswers { offtab, stats, digests, parts }
    }

    fn rel(&self, i: usize) -> usize {
        u64::from_le_bytes(self.offtab[i * 8..i * 8 + 8].try_into().expect("len 8")) as usize
    }

    /// Part `i`'s Stats body; `None` when the part carries none for the column.
    pub fn stats_body(&self, i: usize) -> Option<&'a [u8]> {
        if i >= self.parts {
            return None;
        }
        let (a, b) = (self.rel(i), self.rel(i + 1));
        (a != b).then(|| &self.stats[a..b])
    }

    pub fn digest(&self, i: usize) -> Option<&'a [u8]> {
        if i >= self.parts {
            return None;
        }
        let d = &self.digests[i * DIGEST_LEN..(i + 1) * DIGEST_LEN];
        d.iter().any(|&b| b != 0).then_some(d)
    }

    pub fn parts(&self) -> usize {
        self.parts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_column_round_trips_with_empty_bodies_and_absent_digests() {
        let bodies: [&[u8]; 3] = [b"aaaa", b"", b"cc"];
        let digests = [Some([1u8; DIGEST_LEN]), None, Some([2u8; DIGEST_LEN])];
        let bytes = encode_column(&bodies, &digests).unwrap();
        let v = ColumnAnswers::parse(&bytes, 3).unwrap();
        assert_eq!(v.stats_body(0), Some(&b"aaaa"[..]));
        assert_eq!(v.stats_body(1), None);
        assert_eq!(v.stats_body(2), Some(&b"cc"[..]));
        assert_eq!(v.digest(1), None);
        assert_eq!(v.digest(2).unwrap()[0], 2);
        assert!(v.stats_body(3).is_none());
        assert!(ColumnAnswers::parse(&bytes, 4).is_err());
        let mut bad = bytes.clone();
        bad[8] = 200;
        assert!(ColumnAnswers::parse(&bad, 3).is_err());
        let w = ColumnAnswers::validated(&bytes, 3);
        for i in 0..4 {
            assert_eq!(w.stats_body(i), v.stats_body(i));
            assert_eq!(w.digest(i), v.digest(i));
        }
    }
}
