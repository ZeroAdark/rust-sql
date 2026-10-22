//! nestloop's premises (v5 landing 4, moved from props/ops.rs's nest-join arm): `nest.param` for a parameterized inner
//! side, and the qual's expression kinds. The props walk calls `nestloop` for each nested-loop join.
use gravity_decompose::join::JoinDesc;

use crate::face::kinds::KindId;
use crate::props::Walk;

impl<'p> Walk<'_, '_, 'p> {
    /// One Op::Join(JoinDesc::Nest).
    pub(crate) fn nestloop(&mut self, j: &JoinDesc<'p>) {
        let JoinDesc::Nest { params, qual, .. } = j else { return };
        if params.len() > 0 {
            self.add(KindId::NestParam);
        }
        self.list(qual);
    }
}
