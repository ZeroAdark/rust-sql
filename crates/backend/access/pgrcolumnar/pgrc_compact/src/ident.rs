//! The output part's `RowIdentMap`: every surviving row keeps the ID it was given at insert.

use pgrc_format::manifest::{IdRanges, PART_ROWMAP_GRANULES};
use pgrc_format::rowmap::{Opts, RevForm};
use pgrc_format::FormatResult;

use crate::order::Input;
use crate::JobKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutMap {
    Granules(Vec<u8>),
}

impl OutMap {
    pub fn form(&self) -> u32 {
        PART_ROWMAP_GRANULES
    }

    pub fn encode(&self) -> Vec<u8> {
        let OutMap::Granules(b) = self;
        b.clone()
    }
}

/// Every ordinal's ID of each input, decoded once in ordinal order (a granules map decodes a frame per call of `ident`).
pub fn input_ids(inputs: &[Input<'_>]) -> FormatResult<Vec<Vec<u64>>> {
    let mut all = pgrc_format::try_vec(inputs.len(), "compaction input ids")?;
    for i in inputs {
        let mut v = pgrc_format::try_filled(i.rows as usize, 0u64, "compaction input ids")?;
        i.map.decode(0, &mut v);
        all.push(v);
    }
    Ok(all)
}

pub fn identities(inputs: &[Input<'_>], order: &[(u16, u32)]) -> FormatResult<Vec<u64>> {
    let all = input_ids(inputs)?;
    Ok(order.iter().map(|&(n, o)| all[n as usize][o as usize]).collect())
}

/// `ROWMAP2_FORCE_REV=<fences|labels|frameix>` (a test knob, read once per output map, never per row).
fn forced() -> Option<RevForm> {
    std::env::var("ROWMAP2_FORCE_REV").ok().and_then(|v| RevForm::from_name(&v)).filter(|f| *f != RevForm::Mono)
}

/// `Err` for a permuted output past the frame table's 2^24 rows (parts hold at most 2^20).
pub fn build_map(_kind: &JobKind, inputs: &[Input<'_>], order: &[(u16, u32)]) -> Result<OutMap, pgrc_format::FormatError> {
    let ids = identities(inputs, order)?;
    pgrc_format::rowmap::encode(&ids, Opts { force: forced() }).map(OutMap::Granules)
}

pub fn renumbered(base: u64, n: u64) -> Result<OutMap, pgrc_format::FormatError> {
    let len = u32::try_from(n).map_err(|_| pgrc_format::FormatError::Corrupt { at: "RowIdentMap: a part past u32 rows" })?;
    pgrc_format::rowmap::encode_runs(&[(base, len)], Opts::default()).map(OutMap::Granules)
}

/// The inputs are one-home, so their ranges never overlap.
pub fn union_ranges<'a>(inputs: impl IntoIterator<Item = &'a [(u64, u64)]>) -> IdRanges {
    let mut all: IdRanges = inputs.into_iter().flatten().copied().collect();
    all.sort_unstable();
    all.dedup_by(|(lo, hi), l| {
        let joins = l.1 == *lo;
        if joins {
            l.1 = *hi;
        }
        joins
    });
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn union_ranges_sorts_and_joins_touching_ranges() {
        let x: &[(u64, u64)] = &[(582, 873), (2000, 2291)];
        let y: &[(u64, u64)] = &[(291, 582)];
        let z: &[(u64, u64)] = &[];
        assert_eq!(union_ranges([x, y, z]), vec![(291, 873), (2000, 2291)]);
        assert_eq!(union_ranges([z]), vec![]);
    }
}
