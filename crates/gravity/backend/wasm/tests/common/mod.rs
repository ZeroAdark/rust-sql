//! The wasmtime rig: one store with a linear memory, a function table and
//! the shadow stack pointer; a program's hosts are bound by rewriting each
//! `HostFnDecl::addr` to a table slot holding a host closure over the
//! guest memory (natively the address is a function pointer, on a wasm32
//! server it is already a table index).
#![allow(dead_code)]

use gravity_interp::{canon, Interp, Trap};
use gravity_ir::*;
use wasmtime::{Config, Engine, Extern, Func as WFunc, FuncType, Global, GlobalType, Instance, Memory, MemoryType, Module, Mutability, Ref, RefType, Store, Table, TableType, Val, ValType};

pub type HostImpl = Box<dyn Fn(&mut [u8], &[u64]) -> u64 + Send + Sync>;

/// The shadow stack grows down from here; test data lives above it.
pub const STACK_TOP: u32 = 0x1_0000;
pub const DATA: u32 = 0x1_0000;

pub struct Rig {
    pub store: Store<()>,
    pub memory: Memory,
    pub table: Table,
    pub sp: Global,
    next_slot: u32,
}

impl Rig {
    pub fn new() -> Rig {
        let mut config = Config::new();
        config.wasm_simd(true);
        let engine = Engine::new(&config).unwrap();
        let mut store = Store::new(&engine, ());
        let memory = Memory::new(&mut store, MemoryType::new(64, None)).unwrap();
        let table = Table::new(&mut store, TableType::new(RefType::FUNCREF, 1024, None), Ref::Func(None)).unwrap();
        let sp = Global::new(&mut store, GlobalType::new(ValType::I32, Mutability::Var), Val::I32(STACK_TOP as i32)).unwrap();
        Rig { store, memory, table, sp, next_slot: 1 }
    }

    pub fn mem(&mut self) -> &mut [u8] {
        self.memory.data_mut(&mut self.store)
    }

    pub fn write(&mut self, at: u32, bytes: &[u8]) {
        let at = at as usize;
        self.mem()[at..at + bytes.len()].copy_from_slice(bytes);
    }

    pub fn read(&mut self, at: u32, n: usize) -> Vec<u8> {
        let at = at as usize;
        self.mem()[at..at + n].to_vec()
    }

    pub fn read_i64(&mut self, at: u32) -> i64 {
        i64::from_le_bytes(self.read(at, 8).try_into().unwrap())
    }

    pub fn write_i64(&mut self, at: u32, v: i64) {
        self.write(at, &v.to_le_bytes());
    }

    /// `program` with every host bound to a table slot; hosts are matched by name.
    pub fn bind(&mut self, program: &Program, hosts: Vec<(&str, HostImpl)>) -> Program {
        let mut p = program.clone();
        let mut hosts = hosts;
        for decl in &mut p.host_fns {
            let pos = hosts.iter().position(|(n, _)| *n == decl.name.as_str()).unwrap_or_else(|| panic!("no host impl for {}", decl.name));
            let (_, f) = hosts.remove(pos);
            let params: Vec<ValType> = decl.params.iter().map(|&t| wt(t)).collect();
            let results: Vec<ValType> = decl.ret.map(wt).into_iter().collect();
            let ret = decl.ret;
            let memory = self.memory;
            let ty = FuncType::new(self.store.engine(), params, results);
            let wf = WFunc::new(&mut self.store, ty, move |mut caller, args, out| {
                let a: Vec<u64> = args.iter().map(val_to_u64).collect();
                let r = f(memory.data_mut(&mut caller), &a);
                if let Some(t) = ret {
                    out[0] = u64_to_val(t, r);
                }
                Ok(())
            });
            let slot = self.next_slot;
            self.next_slot += 1;
            if slot as u64 >= self.table.size(&self.store) {
                self.table.grow(&mut self.store, 1024, Ref::Func(None)).unwrap();
            }
            self.table.set(&mut self.store, slot as u64, Ref::Func(Some(wf))).unwrap();
            decl.addr = slot as usize;
        }
        p
    }

    pub fn instantiate(&mut self, program: &Program, entry: Func) -> (Instance, Vec<u8>) {
        let bytes = gravity_wasm::compile(program, entry).unwrap_or_else(|e| panic!("compile: {e}"));
        let module = Module::new(self.store.engine(), &bytes).unwrap_or_else(|e| panic!("module: {e:?}\n{}", gravity_ir::text::print_program(program)));
        let imports: [Extern; 3] = [self.memory.into(), self.table.into(), self.sp.into()];
        let inst = Instance::new(&mut self.store, &module, &imports).unwrap_or_else(|e| panic!("instantiate: {e:?}"));
        (inst, bytes)
    }

    /// Call `func` with the interpreter's argument encoding; results in the same encoding.
    pub fn call(&mut self, inst: &Instance, program: &Program, func: Func, args: &[u64]) -> Result<Vec<u64>, String> {
        let f = program.func(func);
        let wf = inst.get_func(&mut self.store, &gravity_wasm::func_export_name(func)).expect("exported");
        let params: Vec<Val> = f.params.iter().zip(args).map(|(&t, &a)| u64_to_val(t, a)).collect();
        let mut results: Vec<Val> = f.returns.iter().map(|&t| u64_to_val(t, 0)).collect();
        wf.call(&mut self.store, &params, &mut results).map_err(|e| format!("{e:?}"))?;
        Ok(f.returns.iter().zip(&results).map(|(&t, v)| canon(t, val_to_u64(v))).collect())
    }

    pub fn sp_value(&mut self) -> i32 {
        self.sp.get(&mut self.store).i32().unwrap()
    }
}

pub fn wt(t: Type) -> ValType {
    match gravity_wasm::val_type(t) {
        wasm_encoder::ValType::I32 => ValType::I32,
        wasm_encoder::ValType::I64 => ValType::I64,
        wasm_encoder::ValType::F32 => ValType::F32,
        wasm_encoder::ValType::F64 => ValType::F64,
        _ => ValType::V128,
    }
}

pub fn val_to_u64(v: &Val) -> u64 {
    match v {
        Val::I32(x) => *x as i64 as u64,
        Val::I64(x) => *x as u64,
        Val::F32(b) => *b as u64,
        Val::F64(b) => *b,
        _ => panic!("scalar"),
    }
}

pub fn u64_to_val(t: Type, x: u64) -> Val {
    match wt(t) {
        ValType::I32 => Val::I32(x as u32 as i32),
        ValType::I64 => Val::I64(x as i64),
        ValType::F32 => Val::F32(x as u32),
        ValType::F64 => Val::F64(x),
        _ => panic!("scalar"),
    }
}

/// Bit-identical, except that two NaNs of a float type agree.
pub fn same(t: Type, a: u64, b: u64) -> bool {
    match t {
        Type::F64 => a == b || (f64::from_bits(a).is_nan() && f64::from_bits(b).is_nan()),
        Type::F32 => a == b || (f32::from_bits(a as u32).is_nan() && f32::from_bits(b as u32).is_nan()),
        _ => a == b,
    }
}

/// Run `func` through both backends on the same scalar arguments and
/// require agreement; a trap the IR calls undefined (division) on the
/// interpreter's side is not compared.
pub fn both(rig: &mut Rig, p: &Program, inst: &Instance, func: Func, args: &[u64]) -> Option<Vec<u64>> {
    let interp = Interp::new(p).run(func, args);
    let wasm = rig.call(inst, p, func, args);
    match interp {
        Err(Trap::DivideByZero { .. }) | Err(Trap::DivideOverflow { .. }) => None,
        Err(t) => panic!("interpreter trap {t:?} (wasm: {wasm:?})"),
        Ok(want) => {
            let got = wasm.unwrap_or_else(|e| panic!("wasm trap {e} where the interpreter returned {want:?}\n{}", gravity_ir::text::print_program(p)));
            let f = p.func(func);
            for (k, &t) in f.returns.iter().enumerate() {
                assert!(same(t, got[k], want[k]), "{}({args:?}): wasm {got:?} vs interpreter {want:?}\n{}", f.name, gravity_ir::text::print_program(p));
            }
            Some(got)
        }
    }
}
