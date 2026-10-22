use gravity_abi::status::{OK, STOP};
use gravity_abi::Ctx;

use super::{fail, guarded, inner, inner_of};
use crate::sink::{BorrowedBatch, Flow, Sink, SinkLeg, POISON};
use crate::{Error, Result};

mod raw;
pub use raw::{push, push_leg, unbound};
