//! catalog::hash_eq: whether a hash clause's operator is a built-in hashable equality, read from the operator's
//! catalog facts (`Facts::oper`: oprcanhash and the first hash family), so plan readers ask here instead of reading
//! operator rows themselves.

/// D1901: whether hash clause operator `opno` is a built-in hashable equality, the only operators whose equality the key
/// images (a word's, rep.hosted's canonical form) keep. PostgreSQL's ExecHashJoin compares by the clause's own operator
/// and buckets by its hash opfamily's function (`get_op_hash_functions`' first family, the lowest oid), so a user
/// HASHES operator over any key type, whatever its function, is not the key type's `=`; nor is an operator the builtin
/// facts do not know as hashable. A built-in one's first hash family is built-in.
pub(crate) fn builtin_hash_eq(facts: &dyn crate::catalog::Facts, opno: u32) -> bool {
    let builtin = |o: u32| o != 0 && !crate::effects::user_code(o);
    builtin(opno) && facts.oper(opno).is_some_and(|o| o.canhash && builtin(o.hash_family))
}
