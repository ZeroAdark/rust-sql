use crate::{Mcx, PgResult};

#[doc = "An accounting reservation for memory owned outside the context allocator."]
#[doc = "Release the external memory before dropping this guard."]
#[doc = "```"]
#[doc = "let mut ctx = mcx::MemoryContext::new(\"mapped-code\");"]
#[doc = "let reservation = ctx.mcx().reserve_external(4096).unwrap();"]
#[doc = "assert_eq!(reservation.bytes(), 4096);"]
#[doc = "assert_eq!(ctx.used(), 4096);"]
#[doc = "drop(reservation);"]
#[doc = "ctx.reset();"]
#[doc = "assert_eq!(ctx.used(), 0);"]
#[doc = "```"]
#[doc = "```compile_fail,E0502"]
#[doc = "let mut ctx = mcx::MemoryContext::new(\"mapped-code\");"]
#[doc = "let reservation = ctx.mcx().reserve_external(4096).unwrap();"]
#[doc = "ctx.reset();"]
#[doc = "drop(reservation);"]
#[doc = "```"]
#[doc = "```compile_fail,E0505"]
#[doc = "let ctx = mcx::MemoryContext::new(\"mapped-code\");"]
#[doc = "let reservation = ctx.mcx().reserve_external(4096).unwrap();"]
#[doc = "drop(ctx);"]
#[doc = "drop(reservation);"]
#[doc = "```"]
#[doc = "```compile_fail,E0382"]
#[doc = "let ctx = mcx::MemoryContext::new(\"mapped-code\");"]
#[doc = "let reservation = ctx.mcx().reserve_external(4096).unwrap();"]
#[doc = "let moved = reservation;"]
#[doc = "drop(reservation);"]
#[doc = "drop(moved);"]
#[doc = "```"]
#[derive(Debug)]
#[must_use = "keep the reservation alive until the external memory is released"]
pub struct ExternalAllocationGuard<'mcx> {
    mcx: Mcx<'mcx>,
    bytes: usize,
}

impl<'mcx> Mcx<'mcx> {
    pub fn reserve_external(self, bytes: usize) -> PgResult<ExternalAllocationGuard<'mcx>> {
        self.0.check_live();
        // Same admission as an ordinary allocation: the ancestor walk only on a
        // limited path, the local overflow guard otherwise (charge -> check_limit).
        self.0.charge(bytes).map_err(|_| self.oom(bytes))?;
        self.0.is_reset.set(false);
        Ok(ExternalAllocationGuard { mcx: self, bytes })
    }
}

impl ExternalAllocationGuard<'_> {
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

impl Drop for ExternalAllocationGuard<'_> {
    fn drop(&mut self) {
        self.mcx.0.check_live();
        self.mcx.0.uncharge(self.bytes);
    }
}
