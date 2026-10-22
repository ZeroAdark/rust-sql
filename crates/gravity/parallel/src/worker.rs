use std::sync::atomic::AtomicU8;

use gravity_abi::MorselRange;
use gravity_driver::{Claimed, Error, Open, Result};

use gravity_abi::ctx::abort_set;

pub struct Worker<'a> {
    open: &'a Open<'a>,
    ordinal: u16,
    abort: &'a AtomicU8,
}

impl<'a> Worker<'a> {
    pub fn new(open: &'a Open<'a>, ordinal: u16, abort: &'a AtomicU8) -> Worker<'a> {
        Worker { open, ordinal, abort }
    }

    pub fn ordinal(&self) -> u16 {
        self.ordinal
    }

    pub fn run_claim(&self, claim: MorselRange) -> Result<Claimed> {
        if abort_set(self.abort) {
            return Err(Error::Cancelled);
        }
        self.open.run_claim(self.ordinal, claim)
    }
}
