//! The `Code` shape: `WasmCode` over an `Embedder` (wasmtime here) runs
//! through `gravity_program::Code::call` with the `Ctx` argument as a
//! guest offset (P-A's truncation).

mod common;
use common::*;
use gravity_ir::builder::ProgramBuilder;
use gravity_ir::*;
use gravity_wasm::embed::{Embedder, Handle, WasmCode};
use std::sync::{Arc, Mutex};
use wasmtime::Instance;

struct WasmtimeEmbedder {
    inner: Mutex<(Rig, Vec<Option<Instance>>)>,
}

impl Embedder for WasmtimeEmbedder {
    fn instantiate(&self, bytes: &[u8]) -> Result<Handle, String> {
        let mut g = self.inner.lock().unwrap();
        let (rig, insts) = &mut *g;
        let module = wasmtime::Module::new(rig.store.engine(), bytes).map_err(|e| format!("{e:?}"))?;
        let imports: [wasmtime::Extern; 3] = [rig.memory.into(), rig.table.into(), rig.sp.into()];
        let inst = Instance::new(&mut rig.store, &module, &imports).map_err(|e| format!("{e:?}"))?;
        insts.push(Some(inst));
        Ok(insts.len() as Handle - 1)
    }
    fn call(&self, handle: Handle, ctx: u32) -> i64 {
        let mut g = self.inner.lock().unwrap();
        let (rig, insts) = &mut *g;
        let inst = insts[handle as usize].expect("live handle");
        let f = inst.get_typed_func::<i32, i64>(&mut rig.store, gravity_wasm::ENTRY_EXPORT).expect("entry(i32) -> i64");
        f.call(&mut rig.store, ctx as i32).expect("entry runs")
    }
    fn release(&self, handle: Handle) {
        self.inner.lock().unwrap().1[handle as usize] = None;
    }
}

/// `entry(ctx) -> i64`: reads `ctx+0` and `ctx+8`, stores their sum at `ctx+16`, returns it.
fn entry_program() -> Program {
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("entry", &[Type::Ptr], &[Type::I64]);
    let mut b = pb.define(f);
    let ctx = b.params()[0];
    let a = b.load(Type::I64, ctx, 0);
    let c = b.load(Type::I64, ctx, 8);
    let s = b.int_bin(IntBinOp::Add, a, c);
    b.store(ctx, 16, s);
    b.ret(&[s]);
    pb.finish()
}

#[test]
fn wasm_code_runs_through_code_call() {
    let p = entry_program();
    let bytes = gravity_wasm::compile(&p, Func(0)).unwrap();
    let embedder = Arc::new(WasmtimeEmbedder { inner: Mutex::new((Rig::new(), Vec::new())) });
    {
        let mut g = embedder.inner.lock().unwrap();
        g.0.write_i64(DATA, 40);
        g.0.write_i64(DATA + 8, 2);
    }
    let wc = WasmCode::new(embedder.clone(), bytes.clone()).unwrap();
    assert_eq!(wc.bytes(), &bytes[..]);
    assert_eq!(wc.handle(), 0);
    let code = wc.into_code();
    assert!(code.is_interp(), "the Interp owner shape carries the wasm instance until gravity_program grows an Owner::Wasm");
    let r = unsafe { code.call(DATA as usize as *mut gravity_abi::Ctx) };
    assert_eq!(r, 42);
    assert_eq!(embedder.inner.lock().unwrap().0.read_i64(DATA + 16), 42);
    drop(code);
    assert!(embedder.inner.lock().unwrap().1[0].is_none(), "released on drop");
}

#[test]
fn entry_export_has_the_i32_wrapper_shape() {
    let p = entry_program();
    let bytes = gravity_wasm::compile(&p, Func(0)).unwrap();
    let engine = wasmtime::Engine::default();
    let module = wasmtime::Module::new(&engine, &bytes).unwrap();
    let ty = module.get_export(gravity_wasm::ENTRY_EXPORT).unwrap();
    let f = ty.func().unwrap();
    assert_eq!(f.params().map(|t| t.to_string()).collect::<Vec<_>>(), ["i32"]);
    assert_eq!(f.results().map(|t| t.to_string()).collect::<Vec<_>>(), ["i64"]);
    let imports: Vec<(String, String)> = module.imports().map(|i| (i.module().to_owned(), i.name().to_owned())).collect();
    assert_eq!(imports, [("env".to_owned(), "memory".to_owned()), ("env".to_owned(), "__indirect_function_table".to_owned()), ("env".to_owned(), "__stack_pointer".to_owned())]);
}
