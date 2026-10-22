//! The hash functions exec ops/memoize's Cache object looks up for each key when its table is made
//! (nodeMemoize.c ExecInitMemoize: get_op_hash_functions per cache key). A run-time catalog read, never cached.
use types_error::PgResult;

/// A hash equality operator's hash functions, the left input's and the right input's (ExecInitMemoize's
/// `get_op_hash_functions`); `None` when the operator has none.
pub fn op_hash_functions(opno: u32) -> PgResult<Option<(u32, u32)>> {
    lsyscache::get_op_hash_functions(opno)
}
