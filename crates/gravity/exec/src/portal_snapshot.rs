//! The portal snapshot (rung 14 contract C1; entry.md §9): a SELECT's snapshot and output cid at Bind, a write's at run.

use std::rc::Rc;

use gravity_program::Cmd;
use types_error::PgResult;
use types_resowner::ResourceOwner;
use types_snapshot::SnapshotData;

use crate::output_cid::output_cid;
use crate::shapes::Admitted;

pub type Held = (Rc<SnapshotData<'static>>, u32);

/// At Bind: `None` for a write (PortalRunMulti takes its snapshot when the portal runs, `at_execute`).
pub fn at_bind(a: &Admitted, owner: ResourceOwner) -> PgResult<Option<Held>> {
    if a.cmd != Cmd::Select {
        return Ok(None);
    }
    at_execute(a, owner).map(Some)
}

pub fn at_execute(a: &Admitted, owner: ResourceOwner) -> PgResult<Held> {
    let snap = snapmgr::GetTransactionSnapshot()?;
    let snap = snapmgr::RegisterSnapshotOnOwner(&snap, owner)?;
    Ok((snap, output_cid(a)?))
}
