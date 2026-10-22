//! write.literals' raw half (rule R6): `stream_row`, the host entry the VALUES source's IR calls at a coded row's turn in
//! the stream form (CAP-29). It calls the execution's hook (gravity_program protos `LiteralsStream`) that the table's
//! stream word points at; the evaluation itself is exec's (ops/modifytable/raw.rs).
#![allow(unsafe_code)]
use gravity_program::protos::LiteralsStream;

gravity_abi::host! {
    /// Row `j` of the stream behind `hook` (`LiteralsStream::eval`): 1 its cells written to the table, 0 an error (stashed
    /// for the raise) or a panic (parked for the program's caller).
    #[guard(0)]
    pub(super) unsafe fn stream_row(hook: u64, j: u64) -> i64 {
        // SAFETY: `hook` is the table's stream word, the hook the execution's protocol state keeps for the program's run.
        let h = unsafe { &*(hook as usize as *const LiteralsStream) };
        // SAFETY: `eval` is exec's entry for `cx`, the stream it was made with.
        unsafe { (h.eval)(h.cx, j) }
    }
}
