//! lockrows' raw half (rules R6 / R8: the family's unsafe code lives here): the row lock's host entry, written through the
//! contract's `host!`. entries.rs re-exports it, so the lowerers take its address there; the IR's host name is `lock_row`.
use gravity_abi::status::ERROR;
use gravity_abi::Ctx;

crate::contract::host! {
    /// `write.lock` (H21, LockRows): lock the row at site `site`'s `pos` (`Program.lock(ctx, site)`, mode x wait policy
    /// bound in the host). The value is the host's: `WRITE_DONE` locked, `WRITE_SKIPPED` gone or SKIP LOCKED, `WRITE_RECHECK`
    /// a newer version to recheck (EvalPlanQual), or a negative status whose error the driver holds.
    #[guard(ERROR as u64)]
    pub unsafe fn lock_row(ctx: u64, site: u64) -> u64 {
        gravity_abi::program::lock(ctx as *mut Ctx, site) as u64
    }
}
