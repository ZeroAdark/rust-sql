//! D3038: the octet-length lane of a varlena column (`pgrc_format::shredlane::OCTET_LENGTH_PATH`).
//!
//! Each text-class or bytea column of a part gets one derived int4 lane, row-aligned with it: the payload's byte length
//! (the detoasted bytes the part stores, so `octet_length`'s value, an overflowed value's full length), NULL where the
//! column is. A reader answers `octet_length`, `col = ''` / `col <> ''` and, on all-ASCII granules, `length` from it
//! without reading the column's bytes. It rides the shred lanes' seal path (its own election, Stats and validity, after
//! every shred lane) outside their budget; `seal_part` derives it after its cluster sort, and `seal_part` drops it where the column sealed as dictionary codes, whose
//! per-code length table already answers, or as one constant value.

use pgrc_format::class::{StorageClass, TypeSemantics};
use pgrc_format::shredlane::{octet_length_schema, OCTET_LENGTH_PATH};

use crate::ingest::ColBuffer;
use crate::shred::{ShredLane, ShredLaneSource};
use crate::structural::StructuralPolicy;
use crate::WriteResult;

/// Whether column `c` takes an octet lane: a text-class or bytea varlena that is not a structural array or a column of
/// the part's shred source (`covers`: a document column).
pub fn eligible(c: &ColBuffer, structural: &StructuralPolicy, shred: &dyn ShredLaneSource) -> bool {
    c.schema.class == StorageClass::VarlenaVerbatim
        && matches!(c.schema.semantics, TypeSemantics::TextCollated | TypeSemantics::MemcmpOrdered)
        && structural.array_facts(c.schema.attno).is_none()
        && !shred.covers(c.schema.attno)
}

/// The attnos of `cols`' columns that take an octet lane (`eligible`), in column order: `seal_part` derives their lanes.
pub fn octet_columns(cols: &[ColBuffer], structural: &StructuralPolicy, shred: &dyn ShredLaneSource) -> Vec<u32> {
    cols.iter().filter(|c| eligible(c, structural, shred)).map(|c| c.schema.attno).collect()
}

/// The octet lanes of `roots`' columns `attnos` (in that order), from the rows as `seal_part` seals them (after its
/// cluster sort, so the lanes never take the permutation).
pub fn octet_lanes(roots: &[ColBuffer], attnos: &[u32]) -> WriteResult<Vec<ShredLane>> {
    let mut out = pgrc_format::try_vec(attnos.len(), "octet")?;
    for &attno in attnos {
        let Some(c) = roots.iter().find(|c| c.schema.attno == attno) else {
            return Err(crate::WriteError::Contract { detail: "octet lane of a column the part does not hold" });
        };
        let rows = c.rows();
        // CONVERT W3-2a: the lane is its column's owner's (the part's)
        let mut lane = ColBuffer::with_owner(octet_length_schema(attno), c.owner());
        lane.reserve_rows(rows as usize, 0)?;
        // A batch at a time through the word lane (the per-row
        // varlena_payload + append_word sequence over a slice).
        const BATCH: u64 = 8192;
        let (mut lens, mut nulls) = (Vec::new(), Vec::new());
        let mut lo = 0;
        while lo < rows {
            let hi = (lo + BATCH).min(rows);
            lens.clear();
            nulls.clear();
            c.varlena_payload_lens(lo, hi, &mut lens, &mut nulls)?;
            lane.append_word_lane(&lens, &nulls)?;
            lo = hi;
        }
        out.push(ShredLane { parent_attno: attno, path: OCTET_LENGTH_PATH.to_string(), col: lane, scale: None, object_roots: false });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{img_4b_u, text_col};
    use crate::ingest::NoExternalDetoast;

    /// The batched octet lane is the per-row `varlena_payload` + `append_word` lane, field by field (NULLs, empty and
    /// repeated lengths, a row count that is not a batch multiple).
    #[test]
    fn octet_lane_equals_per_row_appends() {
        let n = 20_000u64;
        let (mut arena, mut spans, mut nulls) = (Vec::new(), Vec::new(), Vec::new());
        for i in 0..n {
            let len = if (i / 40) % 2 == 0 { 7 } else { (i * 7 % 50) as usize };
            let img = img_4b_u(&vec![b'a' + (i % 26) as u8; len]);
            spans.push((arena.len() as u32, img.len() as u32));
            arena.extend_from_slice(&img);
            nulls.push(i % 11 == 0);
        }
        let mut col = ColBuffer::new(text_col(3));
        col.append_image_lane(&arena, &spans, &nulls, &mut NoExternalDetoast, &mut Vec::new()).unwrap();
        let got = octet_lanes(std::slice::from_ref(&col), &[3]).unwrap();
        let mut want = ColBuffer::new(octet_length_schema(3));
        for r in 0..n {
            match col.varlena_payload(r).unwrap() {
                Some(p) => want.append_word(p.len() as u64).unwrap(),
                None => want.append_null(),
            }
        }
        let a = &got[0].col;
        assert_eq!(a.rows(), want.rows());
        for r in 0..n as usize {
            assert_eq!(a.word_at(r as u64), want.word_at(r as u64), "row {r}");
        }
        assert_eq!(a.stream_stats(), want.stream_stats());
        assert_eq!(a.logical_hash().unwrap().digest(), want.logical_hash().unwrap().digest());
    }
}
