//! append's premises (v5 landing 4, moved from props/ops.rs's append arm): `src.prune` for a pruned append, and a merge
//! append's order key kinds. The props walk calls `append` for each Op::Append.
use gravity_decompose::ops::shared::append::AppendDesc;

use crate::applies::Prop;
use crate::face::kinds::KindId;
use crate::props::Walk;
use crate::rep::{key_kind, Accessor};

impl<'p> Walk<'_, '_, 'p> {
    /// One Op::Append.
    pub(crate) fn append(&mut self, a: &AppendDesc<'p>) {
        let packed = self.p.has(Prop::Packed);
        if matches!(a, AppendDesc::Arm { prune: Some(_), .. } | AppendDesc::Union { prune: Some(_), .. } | AppendDesc::Merge { prune: Some(_), .. }) {
            self.add(KindId::SrcPrune);
        }
        if let AppendDesc::Merge { keys, .. } = a {
            keys.iter().for_each(|key| self.add(key_kind(key.kind, Some(key.image), packed, Accessor::Order)));
        }
    }
}
