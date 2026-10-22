//! The VALUES source's raw half: `gate`, the host entry a coded row's IR calls at the row's visit (D2775).
gravity_abi::host! {
    /// D2775: C `ExecInitFunc`'s EXECUTE gate on `funcid` at a VALUES row's visit (nodeValuesscan.c ValuesNext compiles a
    /// row that holds no SubPlan only then, under the user of that moment): 1, or 0 with the denial stashed for the raise
    /// (C's text). A check that passes fires the OAT_FUNCTION_EXECUTE hook, as ExecInitFunc does.
    #[guard(0)]
    pub(crate) fn gate(funcid: u64) -> i64 {
        match gravity_expr::nodes::func::execute_gate(funcid as u32) {
            Ok(()) => 1,
            Err(e) => {
                gravity_fn::stash_error(e);
                0
            }
        }
    }
}
