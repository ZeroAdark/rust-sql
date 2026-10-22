//! The user-code reach tests exec's admission asks of a write's output types (exec runner/execute.rs `held_bound`).
use types_core::Oid;

/// A built-in type (an oid below FirstNormalObjectId): its output function is internal code, never a user's, so a row of
/// it formatted before the AFTER triggers fire runs no user code (`held_bound`'s route (a)).
pub fn builtin_type(typid: Oid) -> bool {
    typid < types_core::catalog::FirstNormalObjectId
}
