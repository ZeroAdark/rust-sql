//! The `extern "C"` call targets bound into the `Site` and `Program` blocks (abi-v2 §3):
//! no marshalling; a failure stores the `Error` behind `env` and returns its status. A host that can run code which
//! unwinds (a source, sink, writer, spill or arena host) runs under `guarded` (D314): nothing unwinds out of a host
//! into the program, and a panic that escapes one anyway aborts (a host bug).
pub mod arena;
pub mod dml;
pub mod sink;
pub mod source;

use gravity_abi::Ctx;

use crate::node::Inner;
use crate::source::status_of;
use crate::Error;

mod raw;
pub(crate) use raw::{env_of, guarded, inner, inner_of};

pub(crate) fn fail(n: &mut Inner, e: Error) -> i64 {
    let e = stopped(e);
    let status = status_of(&e);
    n.err = Some(e);
    status
}

/// D3887: a host loop of this kernel call stopped for the statement's abort bit (`cancelpoll::canceled`): the error its
/// stop raised on the way out is the cancel, which the statement's owner raises at its next check; otherwise `e`. Every
/// error a host holds for the call (`fail`, a refused region grow's) goes through it.
pub(crate) fn stopped(e: Error) -> Error {
    if cancelpoll::seen() {
        Error::Cancelled
    } else {
        e
    }
}
