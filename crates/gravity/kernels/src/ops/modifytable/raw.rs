//! modifytable's raw half (rules R6 / R8: the family's unsafe code lives here): the row op's host entry, written through the
//! contract's `host!`. entries.rs re-exports it, so the lowerers take its address there; the IR's host name is `write_row`.
use gravity_abi::Ctx;

crate::contract::host! {
    /// The row-op end's host call (H27, `write.delete`): `Program.write(ctx, op, site)` on the row in site `site` at its
    /// `pos` (`WRITE_DELETE` at M1); the ladder's `h_write`. No marker: the bound write host holds its own boundary.
    pub unsafe fn write_row(ctx: u64, site: u64, op: u64) -> u64 {
        gravity_abi::program::write(ctx as *mut Ctx, op as u32, site) as u64
    }
}
