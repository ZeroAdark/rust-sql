//! `IntBinOp::Umulh` (V4R-206) gives the same word on TPDE, the interpreter and wasm, and that word is the high half
//! of the unsigned 128-bit product: over edge words (0, 1, all ones, single bits, the sign bit, the datetime divides'
//! Granlund-Montgomery magics) crossed with each other and with random words, with a run-time operand and with a
//! constant one (TPDE folds a constant pair; datetime.rs multiplies by a constant).
use gravity_ir::builder::ProgramBuilder;
use gravity_ir::{Func, IntBinOp, Program, Type};

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
struct Frame {
    x: u64,
    y: u64,
    out: [u64; 2],
}

/// `f(frame) -> 0`: out[0] = umulh(x, y), out[1] = umulh(x, K).
fn program(k: u64) -> (Program, Func) {
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("umulh", &[Type::Ptr], &[Type::I64]);
    let mut b = pb.define(f);
    let fr = b.params()[0];
    let x = b.load(Type::I64, fr, 0);
    let y = b.load(Type::I64, fr, 8);
    let r = b.int_bin(IntBinOp::Umulh, x, y);
    b.store(fr, 16, r);
    let kv = b.iconst(Type::I64, k as i64);
    let r = b.int_bin(IntBinOp::Umulh, x, kv);
    b.store(fr, 24, r);
    let zero = b.iconst(Type::I64, 0);
    b.ret(&[zero]);
    (pb.finish(), f)
}

struct Wasm {
    store: wasmtime::Store<()>,
    memory: wasmtime::Memory,
    func: wasmtime::Func,
}
const DATA: usize = 0x1_0000;
impl Wasm {
    fn new(p: &Program, f: Func) -> Wasm {
        use wasmtime::*;
        let bytes = gravity_wasm::compile(p, f).unwrap_or_else(|e| panic!("wasm compile: {e}"));
        let engine = Engine::new(Config::new().wasm_simd(true)).unwrap();
        let mut store = Store::new(&engine, ());
        let memory = Memory::new(&mut store, MemoryType::new(64, None)).unwrap();
        let table = Table::new(&mut store, TableType::new(RefType::FUNCREF, 16, None), Ref::Func(None)).unwrap();
        let sp = Global::new(&mut store, GlobalType::new(ValType::I32, Mutability::Var), Val::I32(DATA as i32)).unwrap();
        let module = Module::new(&engine, &bytes).unwrap();
        let inst = Instance::new(&mut store, &module, &[memory.into(), table.into(), sp.into()]).unwrap();
        let func = inst.get_func(&mut store, &gravity_wasm::func_export_name(f)).expect("exported");
        Wasm { store, memory, func }
    }
    fn run(&mut self, fr: &mut Frame) {
        let size = std::mem::size_of::<Frame>();
        let bytes = unsafe { std::slice::from_raw_parts(fr as *const Frame as *const u8, size) };
        self.memory.data_mut(&mut self.store)[DATA..DATA + size].copy_from_slice(bytes);
        let mut r = [wasmtime::Val::I64(-1)];
        self.func.call(&mut self.store, &[wasmtime::Val::I64(DATA as i64)], &mut r).unwrap();
        assert_eq!(r[0].i64(), Some(0));
        let back = self.memory.data(&self.store)[DATA..DATA + size].to_vec();
        unsafe { std::ptr::copy_nonoverlapping(back.as_ptr(), fr as *mut Frame as *mut u8, size) };
    }
}

#[test]
fn umulh_is_the_same_word_on_every_backend() {
    let hi = |a: u64, b: u64| ((a as u128 * b as u128) >> 64) as u64;
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    // the magics datetime.rs divides by (86_400_000_000, 3_600_000_000, 60_000_000, 1_000_000, 146_097, 1_461, 153 ...)
    // are all of the form ceil(2^s / d); a few of them plus the edges
    let magics: Vec<u64> = [86_400_000_000u64, 3_600_000_000, 60_000_000, 1_000_000, 146_097, 1_461, 153, 7, 10]
        .iter()
        .map(|&d| (((1u128 << 64) + d as u128 - 1) / d as u128) as u64)
        .collect();
    let mut words = vec![0u64, 1, 2, u64::MAX, u64::MAX - 1, 1 << 63, (1 << 63) - 1, 0xffff_ffff, 1 << 32, (1 << 32) + 1];
    words.extend((0..64).map(|i| 1u64 << i));
    words.extend(magics.iter().copied());
    words.extend((0..64).map(|_| next()));
    for &k in words.iter().step_by(7).chain(magics.iter()) {
        let (p, f) = program(k);
        gravity_ir::verify::verify(&p).unwrap_or_else(|e| panic!("verify: {e:?}"));
        let tpde = gravity_backend::mapped(&p, f).unwrap_or_else(|e| panic!("tpde: {}", e.message()));
        let interp = gravity_backend::interp(p.clone(), f).unwrap();
        let mut wasm = Wasm::new(&p, f);
        for &x in &words {
            for &y in words.iter().step_by(3) {
                let fr = Frame { x, y, out: [0; 2] };
                let want = [hi(x, y), hi(x, k)];
                for (name, code) in [("tpde", &tpde), ("interp", &interp)] {
                    let mut got = fr;
                    // SAFETY: the frame is the ctx the function reads and writes; it is live for the call.
                    assert_eq!(unsafe { code.call((&mut got as *mut Frame).cast()) }, 0);
                    assert_eq!(got.out, want, "{name} k={k:#x} {fr:?}");
                }
                let mut got = fr;
                wasm.run(&mut got);
                assert_eq!(got.out, want, "wasm k={k:#x} {fr:?}");
            }
        }
    }
}
