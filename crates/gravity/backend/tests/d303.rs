//! D303 (V4R-14): a program's content key hashes each code's identity. Every interpreted code's entry is one function
//! (`interp_identity`) and its bytes 0, so two interpreted programs of one descriptor shape and different IR used to share
//! one key — the park key derives from it, and a statement ran another statement's parked program. Interpreted code now
//! carries the IR's content key; compiled and hand code hash their entry and bytes, the byte stream they always hashed.

use gravity_backend::{interp, mapped_cached};
use gravity_ir::text::parse_program;
use gravity_ir::Func;
use gravity_program::{Code, Layout, Program, WritePlan};

/// `(ptr) -> (i64)` returning `k`.
fn konst(k: i64) -> (gravity_ir::Program, Func) {
    let src = format!("(program (fn e ((c ptr)) (i64) (block entry ((c ptr)) (k (const i64 {k})) (return k))))");
    let p = parse_program(&src, &|_: &str| None).unwrap_or_else(|e| panic!("{e}"));
    let f = Func(p.funcs.iter().position(|f| f.name == "e").unwrap() as u32);
    (p, f)
}

fn program(code: Code) -> Program {
    let layout = Layout { nsites: 0, out_natts: 0, out_rows: 1, scratch_bytes: 0, exec_bytes: 0 };
    Program::new(layout, &[], &[], WritePlan::SELECT, code)
}

fn call(p: &Program) -> i64 {
    // SAFETY: the program never reads its ctx.
    unsafe { p.code().call(core::ptr::null_mut()) }
}

#[test]
fn interpreted_programs_of_one_shape_and_different_ir_have_different_content_keys() {
    let run = |k: i64| {
        let (ir, f) = konst(k);
        let code = interp(ir, f).unwrap();
        assert!(code.is_interp());
        program(code)
    };
    let (a, b, a2) = (run(4201), run(4202), run(4201));
    assert_eq!((call(&a), call(&b), call(&a2)), (4201, 4202, 4201));
    assert_ne!(a.key, b.key, "different IR, one descriptor shape: different programs");
    assert_eq!(a.key, a2.key, "one IR: one program");
}

#[test]
fn compiled_and_hand_programs_keep_one_key_per_code() {
    let (ir, f) = konst(4203);
    let (code, _) = mapped_cached(&ir, f).unwrap();
    let (code2, _) = mapped_cached(&ir, f).unwrap();
    let (p, p2) = (program(code), program(code2));
    assert_eq!(call(&p), 4203);
    assert_eq!(p.key, p2.key, "one IR through the code cache: one program (the front / park caches still hit)");
    let (other, g) = konst(4204);
    assert_ne!(p.key, program(mapped_cached(&other, g).unwrap().0).key);
    unsafe extern "C-unwind" fn h(_: *mut gravity_abi::Ctx) -> i64 {
        7
    }
    let (h1, h2) = (program(Code::hand(h)), program(Code::hand(h)));
    assert_eq!((call(&h1), h1.key), (7, h2.key), "hand: one entry, one program");
}
