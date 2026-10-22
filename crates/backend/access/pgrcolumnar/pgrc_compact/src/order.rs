//! A sorted merge sorts by `(sort key, row ID)` with the seal's own comparator, so the writer's cluster sort only verifies.

use pgrc_format::FormatResult;
use pgrc_write::seal::cmp::{key_cmp, KeySlot};
use pgrc_write::structural::ClusterKeyDecl;

use crate::RowMapView;

pub struct Input<'a> {
    pub part_no: u32,
    pub rows: u32,
    pub deleted: &'a dyn Fn(u32) -> bool,
    pub map: &'a RowMapView<'a>,
}

impl Input<'_> {
    pub fn ident(&self, ordinal: u32) -> u64 {
        self.map.ident(ordinal)
    }
}

pub fn concat_order(inputs: &[Input<'_>]) -> FormatResult<Vec<(u16, u32)>> {
    let mut out = pgrc_format::try_vec(inputs.iter().map(|i| i.rows as usize).sum(), "compaction order")?;
    for (n, i) in inputs.iter().enumerate() {
        out.extend((0..i.rows).filter(|&o| !(i.deleted)(o)).map(|o| (n as u16, o)));
    }
    Ok(out)
}

/// The survivors sorted by `(key, row ID)`. `keys[k]` holds decl `k`'s cells of the survivors in [`concat_order`].
pub fn sorted_order(inputs: &[Input<'_>], keys: &[Vec<KeySlot<'_>>], decls: &[ClusterKeyDecl]) -> FormatResult<Vec<(u16, u32)>> {
    let base = concat_order(inputs)?;
    assert!(keys.len() == decls.len() && keys.iter().all(|k| k.len() == base.len()), "one key cell per survivor per declared key");
    // every input's IDs decoded once (a granules map decodes a frame per `ident`), then each survivor's
    let all = crate::ident::input_ids(inputs)?;
    let ids: Vec<u64> = base.iter().map(|&(n, o)| all[n as usize][o as usize]).collect();
    drop(all);
    let mut perm: Vec<u32> = (0..base.len() as u32).collect();
    perm.sort_unstable_by(|&a, &b| {
        let (a, b) = (a as usize, b as usize);
        keys.iter().zip(decls).map(|(k, d)| key_cmp(k, d, a, b)).find(|o| o.is_ne()).unwrap_or(ids[a].cmp(&ids[b]))
    });
    Ok(perm.into_iter().map(|i| base[i as usize]).collect())
}
