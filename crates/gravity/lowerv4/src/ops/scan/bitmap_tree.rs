//! BitmapAnd / BitmapOr trees (kind `src.bitmap_tree`, cards/M2.md L5; no v3 / ladder / v1 port: v1 ran the executor
//! subtree). A tree's leaves are bitmap index scans, each with its own keys over its own index; the host builds each
//! leaf's bitmap and intersects (And, stopping at the first empty one; an intersection with a lossy side rechecks) or
//! unions (Or) them as PostgreSQL's `TIDBitmap::intersect` / `union` (hosts v4/m2_bitmap.rs `Tree`), then bitmap.rs's heap
//! walk reads the result. The site's key words are the leaves' in leaf order (the host's `TreeRecipe`), its index the
//! decomposer's (exec pairs the site with its source by it), then a computed key's input cells (D1016 row 6). A leaf whose keys the host cannot take refuses by name (bitmap.rs `leaf_words`). bitmap.rs is the kind's reader:
//! the registration is admission only. Owner M2-HEAP.
use gravity_decompose::ops::shared::scan::BitmapPlan;

use crate::face::kinds::KindId;
use crate::frame::px::Px;
use crate::ops::Table;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::SrcBitmapTree, &())], ..crate::ops::NONE };

/// The tree's key words in leaf order (into `k`, D1016 row 6's computed keys too); returns the first leaf's index, the site's.
pub(crate) fn tree<'p>(px: &mut Px<'_, 'p>, node: i32, plan: &BitmapPlan<'p>, k: &mut crate::ops::scan::bitmap::Words<'_, 'p>) -> u32 {
    crate::stack_check();
    match plan {
        BitmapPlan::Index { indexid, keys, .. } => {
            crate::ops::scan::bitmap::leaf_words(px, node, keys, k);
            *indexid
        }
        BitmapPlan::And { plans, .. } | BitmapPlan::Or { plans, .. } => {
            let mut first = None;
            for p in plans.iter() {
                let i = tree(px, node, p, k);
                first.get_or_insert(i);
            }
            first.unwrap_or_else(|| crate::ops::scan::bitmap::shape(node, "a BitmapAnd / BitmapOr with no child"))
        }
    }
}
