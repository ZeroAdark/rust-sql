//! mergejoin's premises (v5 landing 4, moved from props/ops.rs's merge-join arm): the merge key's order kind and the
//! join clauses' and qual's expression kinds. The props walk calls `mergejoin` for each merge join.
use gravity_abi::breaker::KeyKind;
use gravity_decompose::join::JoinDesc;

use crate::applies::Prop;
use crate::props::Walk;
use crate::rep::{key_kind, Accessor};

impl<'p> Walk<'_, '_, 'p> {
    /// One Op::Join(JoinDesc::Merge).
    pub(crate) fn mergejoin(&mut self, j: &JoinDesc<'p>) {
        let JoinDesc::Merge { clauses, qual, .. } = j else { return };
        let packed = self.p.has(Prop::Packed);
        self.add(key_kind(KeyKind::I64, None, packed, Accessor::Order));
        self.list(clauses);
        self.list(qual);
    }
}
