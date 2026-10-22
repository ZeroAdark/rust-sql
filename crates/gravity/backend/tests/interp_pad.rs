//! The interpreter's entry (`interp_run`) is behind the one landing pad: a trap's panic does not cross the entry's frame;
//! it is parked and `Code::call` resumes it once the entry returns, so the caller sees the same panic as before.

use gravity_ir::text::parse_program;
use gravity_ir::Func;

#[test]
#[should_panic(expected = "the interpreter trapped in e")]
fn a_trap_in_interpreted_code_reaches_the_caller_through_the_pad() {
    let src = "(program (fn e ((c ptr)) (i64) (block entry ((c ptr)) (unreachable))))";
    let p = parse_program(src, &|_| None).unwrap_or_else(|e| panic!("{e}"));
    let code = gravity_backend::interp(p, Func(0)).unwrap();
    // SAFETY: the program never reads its ctx.
    unsafe { code.call(core::ptr::null_mut()) };
}

#[test]
fn a_returning_interpreted_entry_answers_its_value() {
    let src = "(program (fn e ((c ptr)) (i64) (block entry ((c ptr)) (k (const i64 7)) (return k))))";
    let p = parse_program(src, &|_| None).unwrap_or_else(|e| panic!("{e}"));
    let code = gravity_backend::interp(p, Func(0)).unwrap();
    // SAFETY: the program never reads its ctx.
    assert_eq!(unsafe { code.call(core::ptr::null_mut()) }, 7);
}
