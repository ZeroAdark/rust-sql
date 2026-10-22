//! Delete-vector application state (DM-2 — the AB-3.3 verdict-layer leg).
//!
//! The frozen §15 DV vocabulary (`pgrc_format::dml`) is decoded ONCE per
//! part into granule-keyed 8,192-bit masks; the scan drive intersects the
//! mask into each staged window's SELECTION (the one row currency —
//! `pgrc_batch::Selection`'s own doc law: "delete masks (ducklake) and
//! visibility compose into this one currency"). Kernels iterate the
//! selection, never `0..nrows`, so a deleted row is dead to every
//! downstream consumer — filters, aggregates, COUNT(*), canon bytes — by
//! construction, not by per-kernel cooperation.
//!
//! Authoritative-sidecar law (spec §15/§16): a part whose manifest record
//! carries `dv_gen != 0` REFERENCES its DV; the caller validates the
//! manifest triplet (`dv_gen`, `dv_len`, `dv_crc`) against the sidecar
//! payload BEFORE handing bytes here (a missing or mismatched DV on a
//! deletion-bearing part is CORRUPTION — silently scanning without it
//! would resurrect deleted rows). This module validates the PAYLOAD's own
//! internal consistency: header identity (part_no, gen), granule bounds,
//! ordinal bounds, and the deleted-rows total.

use std::collections::BTreeMap;

use pgrc_format::dml::{DvBlockKind, DvReader};
use pgrc_format::geom::GRANULE_ROWS;
use pgrc_format::{FormatError, FormatResult};

/// 64-bit words per granule mask (8,192 rows).
pub const DV_GRANULE_WORDS: usize = (GRANULE_ROWS as usize) / 64;

/// D4537 T18c: a delete vector's bytes on the part cache (its masks are the part's, held in the catalog's opened part for as
/// long as any generation carries it). By construction: opened with the masks, closed when the last `Arc` drops. A clone
/// charges its own copy; two vectors are equal by their masks, whatever their charges.
#[derive(Debug)]
struct DvCharge(memledger::Charge);

impl DvCharge {
    fn of(granules: &BTreeMap<u32, Box<[u64; DV_GRANULE_WORDS]>>) -> DvCharge {
        const OWNER: memledger::Owner = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::PartCache));
        const NODE: usize = 2 * size_of::<usize>();
        let bytes = granules.len() * (size_of::<(u32, Box<[u64; DV_GRANULE_WORDS]>)>() + NODE + DV_GRANULE_WORDS * size_of::<u64>()) + size_of::<PartDeletes>();
        DvCharge(memledger::Charge::open(OWNER, memledger::Release::Evict, memledger::Kind::Region, memledger::charge::Door::Heap, bytes as u64))
    }
}

impl Clone for DvCharge {
    fn clone(&self) -> DvCharge {
        DvCharge(memledger::Charge::open(self.0.owner(), memledger::Release::Evict, memledger::Kind::Region, memledger::charge::Door::Heap, self.0.committed()))
    }
}

impl PartialEq for DvCharge {
    fn eq(&self, _: &DvCharge) -> bool {
        true
    }
}

impl Eq for DvCharge {}

/// One part's decoded delete vector: granule-keyed deletion masks.
/// Immutable after build; shared across workers via `Arc`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartDeletes {
    /// Declared first: its drop moves the bytes before the masks free (D4627).
    charge: DvCharge,
    /// granule -> deletion mask (bit r set = granule-row r is deleted).
    granules: BTreeMap<u32, Box<[u64; DV_GRANULE_WORDS]>>,
    /// Total deleted rows in this part (the header fact, re-verified
    /// against the block payloads at build).
    pub deleted_rows: u64,
}

impl PartDeletes {
    /// Decode + validate one DV payload image (the `encode_dv` output —
    /// the sidecar envelope and the manifest triplet are the CALLER's
    /// checks). `part_no`/`dv_gen` bind the header identity;
    /// `granule_count` bounds the block keys.
    pub fn from_dv_payload(
        payload: &[u8],
        part_no: u32,
        dv_gen: u64,
        granule_count: u32,
    ) -> FormatResult<PartDeletes> {
        let mut rd = DvReader::open(payload)?;
        if rd.header.part_no != part_no {
            return Err(FormatError::Corrupt {
                at: "DV header part_no vs manifest",
            });
        }
        if rd.header.gen != dv_gen {
            return Err(FormatError::Corrupt {
                at: "DV header gen vs manifest dv_gen",
            });
        }
        let mut granules = BTreeMap::new();
        let mut total: u64 = 0;
        while let Some(block) = rd.next_block()? {
            if block.granule >= granule_count {
                return Err(FormatError::Corrupt {
                    at: "DV block granule beyond part granule_count",
                });
            }
            let mut mask = Box::new([0u64; DV_GRANULE_WORDS]);
            let count = match block.kind {
                DvBlockKind::List => {
                    let mut prev: Option<u16> = None;
                    for pair in block.payload.chunks_exact(2) {
                        let r = u16::from_le_bytes([pair[0], pair[1]]);
                        if r as u32 >= GRANULE_ROWS {
                            return Err(FormatError::Corrupt {
                                at: "DV list ordinal beyond granule rows",
                            });
                        }
                        if let Some(p) = prev {
                            if r <= p {
                                return Err(FormatError::Corrupt { at: "DV list order" });
                            }
                        }
                        prev = Some(r);
                        mask[(r / 64) as usize] |= 1u64 << (r % 64);
                    }
                    (block.payload.len() / 2) as u32
                }
                DvBlockKind::Bitmap => {
                    // LSB-first bytes -> LE words: bit r of the granule is
                    // word[r/64] >> (r%64), byte-for-byte.
                    for (w, chunk) in block.payload.chunks_exact(8).enumerate() {
                        mask[w] = u64::from_le_bytes(chunk.try_into().expect("chunks_exact(8)"));
                    }
                    mask.iter().map(|w| w.count_ones() as u32).sum()
                }
            };
            if count != block.count {
                return Err(FormatError::Corrupt {
                    at: "DV block count vs payload",
                });
            }
            if count == 0 {
                // A zero-count block is legal wire but carries nothing —
                // never keyed (granule lookup stays None = no deletions).
                continue;
            }
            total += count as u64;
            granules.insert(block.granule, mask);
        }
        if total != rd.header.deleted_rows {
            return Err(FormatError::Corrupt {
                at: "DV deleted_rows vs block sum",
            });
        }
        Ok(PartDeletes {
            charge: DvCharge::of(&granules),
            granules,
            deleted_rows: total,
        })
    }

    /// Builds from granule masks (compile's in-memory result, checked against its encoded DV); empty masks drop.
    pub fn from_granule_masks(masks: impl IntoIterator<Item = (u32, Box<[u64; DV_GRANULE_WORDS]>)>) -> PartDeletes {
        let granules: BTreeMap<_, _> = masks.into_iter().filter(|(_, m)| m.iter().any(|&w| w != 0)).collect();
        let deleted_rows = granules.values().map(|m| m.iter().map(|w| w.count_ones() as u64).sum::<u64>()).sum();
        PartDeletes { charge: DvCharge::of(&granules), granules, deleted_rows }
    }

    /// D4537 T18c: the bytes this vector has charged to the part cache.
    pub fn charged(&self) -> u64 {
        self.charge.0.committed()
    }

    /// ORs granule `g`'s deletions into `mask` (the scan's DV ∪ tombstone-filter mask).
    #[inline]
    pub fn union_into(&self, g: u32, mask: &mut [u64; DV_GRANULE_WORDS]) {
        if let Some(m) = self.granules.get(&g) {
            mask.iter_mut().zip(m.iter()).for_each(|(d, s)| *d |= s);
        }
    }

    /// The granule's deletion mask; `None` = no deletions in this granule.
    #[inline]
    pub fn granule_mask(&self, g: u32) -> Option<&[u64; DV_GRANULE_WORDS]> {
        self.granules.get(&g).map(|b| &**b)
    }

    /// True when the mask marks granule-row `row` deleted.
    #[inline]
    pub fn is_deleted(mask: &[u64; DV_GRANULE_WORDS], row: u32) -> bool {
        (mask[(row / 64) as usize] >> (row % 64)) & 1 == 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgrc_format::dml::encode_dv;

    #[test]
    fn granule_masks_match_the_decoded_dv_and_union_into_ors() {
        let g0: Vec<u16> = vec![0, 63, 64, 8191];
        let g2: Vec<u16> = (0..GRANULE_ROWS as u16).step_by(5).collect();
        let payload = encode_dv(4, 9, &[(0, DvBlockKind::List, &g0), (2, DvBlockKind::Bitmap, &g2)]).unwrap();
        let decoded = PartDeletes::from_dv_payload(&payload, 4, 9, 3).unwrap();
        let mask = |rows: &[u16]| {
            let mut m = Box::new([0u64; DV_GRANULE_WORDS]);
            rows.iter().for_each(|&r| m[r as usize / 64] |= 1 << (r % 64));
            m
        };
        let built = PartDeletes::from_granule_masks([(0, mask(&g0)), (1, mask(&[])), (2, mask(&g2))]);
        assert_eq!(built, decoded);
        assert!(built.granule_mask(1).is_none());
        let mut m = *mask(&[1, 63]);
        built.union_into(0, &mut m);
        built.union_into(1, &mut m);
        assert_eq!(m, *mask(&[0, 1, 63, 64, 8191]));
    }

    /// D4537 T18c: a delete vector is the part cache's bytes, as many as its masks hold, and equal by its masks alone.
    #[test]
    fn a_delete_vector_is_on_the_part_cache() {
        let rows: Vec<u16> = (0..GRANULE_ROWS as u16).step_by(7).collect();
        let payload = encode_dv(4, 9, &[(0, DvBlockKind::Bitmap, &rows), (2, DvBlockKind::Bitmap, &rows)]).unwrap();
        let dv = PartDeletes::from_dv_payload(&payload, 4, 9, 3).unwrap();
        let masks = 2 * DV_GRANULE_WORDS * size_of::<u64>();
        assert!(dv.charged() >= masks as u64, "both granules' masks are counted ({} bytes)", dv.charged());
        assert_eq!(dv.charge.0.owner(), memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::PartCache)));
        let copy = dv.clone();
        assert_eq!((copy.charged(), &copy), (dv.charged(), &dv), "a clone charges its own copy and compares equal");
        let none = PartDeletes::from_granule_masks([]);
        assert!(none.charged() > 0 && none.charged() < dv.charged(), "an empty vector counts only its shell");
    }
}
