//! memoize's premises (v5 landing 4, moved from props/ops.rs's memo-join arm): the cache key's hash kind and the key
//! expressions' kinds. The props walk calls `memoize` for each memoized join.
use gravity_abi::breaker::KeyKind;
use gravity_decompose::join::JoinDesc;

use crate::applies::Prop;
use crate::props::Walk;
use crate::rep::{key_kind, Accessor};

impl<'p> Walk<'_, '_, 'p> {
    /// One Op::Join(JoinDesc::Memo).
    pub(crate) fn memoize(&mut self, j: &JoinDesc<'p>) {
        let JoinDesc::Memo { keys, .. } = j else { return };
        let packed = self.p.has(Prop::Packed);
        self.add(key_kind(KeyKind::I64, None, packed, Accessor::Hash));
        self.list(keys);
    }
}
