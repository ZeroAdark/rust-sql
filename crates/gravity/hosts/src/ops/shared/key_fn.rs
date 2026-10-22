//! A computed index key's evaluation, the type the index, bitmap, TID and sample walks keyed by executor parameters share.
use std::rc::Rc;

use gravity_abi::Datum as Word;
use mcx::Mcx;
use types_error::PgResult;

/// A computed key's evaluation over the recipe's `ExecVals` (exec ops/scan/param_key.rs compiles it once per execution): its
/// word and NULL flag, a by-reference result allocated in the given context.
pub type KeyFn = Rc<dyn Fn(Mcx<'static>) -> PgResult<(Word, bool)>>;
