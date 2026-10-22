//! The wasm32 server's install path, natively (lane wasmjit stage 2): `compile_entries` exports every entry as
//! `entry<i>` from one module, the entries go into table slots and run through `call_indirect` (the server's direct call
//! of a table-index function pointer), and the shadow stack floor (`StackGuard`) calls the overflow host before a frame
//! would go below it, without moving the stack pointer.

mod common;
use common::*;
use gravity_ir::builder::ProgramBuilder;
use gravity_ir::*;
use gravity_wasm::{compile_entries, entry_export_name, Options, StackGuard};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use wasmtime::{Extern, Func as WFunc, FuncType, Instance, Module, Ref, Val};

/// Two entries over one helper: `e0(ctx)` = load(ctx) + 1, `e1(ctx)` = load(ctx) * 2, each through a 32-byte stack slot
/// (so every entry has a frame and the guard has a prologue to sit in).
fn two_entries() -> (Program, [Func; 2]) {
    let mut pb = ProgramBuilder::new();
    let e0 = pb.declare_func("e0", &[Type::Ptr], &[Type::I64]);
    let e1 = pb.declare_func("e1", &[Type::Ptr], &[Type::I64]);
    for (f, op, k) in [(e0, IntBinOp::Add, 1i64), (e1, IntBinOp::Mul, 2)] {
        let mut b = pb.define(f);
        let ctx = b.params()[0];
        let slot = b.declare_stack_slot(32, 8);
        let at = b.stack_addr(slot);
        let x = b.load(Type::I64, ctx, 0);
        b.store(at, 0, x);
        let y = b.load(Type::I64, at, 0);
        let c = b.iconst(Type::I64, k);
        let r = b.int_bin(op, y, c);
        b.ret(&[r]);
    }
    (pb.finish(), [e0, e1])
}

fn instantiate(rig: &mut Rig, bytes: &[u8]) -> Instance {
    let module = Module::new(rig.store.engine(), bytes).unwrap_or_else(|e| panic!("module: {e:?}"));
    let imports: [Extern; 3] = [rig.memory.into(), rig.table.into(), rig.sp.into()];
    Instance::new(&mut rig.store, &module, &imports).unwrap_or_else(|e| panic!("instantiate: {e:?}"))
}

/// The embedder's install: the module's `entry<i>` into fresh table slots; the first slot.
fn install(rig: &mut Rig, inst: &Instance, n: usize) -> u64 {
    let first = rig.table.grow(&mut rig.store, n as u64, Ref::Func(None)).unwrap();
    for i in 0..n {
        let f = inst.get_func(&mut rig.store, &entry_export_name(i)).expect("entry<i> exported");
        rig.table.set(&mut rig.store, first + i as u64, Ref::Func(Some(f))).unwrap();
    }
    first
}

/// The server's call of a table-index function pointer: `call_indirect` through a one-function caller module.
fn call_slot(rig: &mut Rig, slot: u64, ctx: u32) -> Result<i64, String> {
    let f = match rig.table.get(&mut rig.store, slot) {
        Some(Ref::Func(Some(f))) => f,
        other => return Err(format!("slot {slot}: {other:?}")),
    };
    let mut out = [Val::I64(0)];
    f.call(&mut rig.store, &[Val::I32(ctx as i32)], &mut out).map_err(|e| format!("{e:?}"))?;
    Ok(out[0].i64().unwrap())
}

#[test]
fn every_entry_is_exported_and_runs_from_its_table_slot() {
    let (p, [e0, e1]) = two_entries();
    let bytes = compile_entries(&p, &[e0, e1], Options::default()).unwrap();
    let mut rig = Rig::new();
    rig.write_i64(DATA, 20);
    let inst = instantiate(&mut rig, &bytes);
    let first = install(&mut rig, &inst, 2);
    let sp = rig.sp_value();
    assert_eq!(call_slot(&mut rig, first, DATA), Ok(21));
    assert_eq!(call_slot(&mut rig, first + 1, DATA), Ok(40));
    assert_eq!(rig.sp_value(), sp, "each frame gives the shadow stack back");
}

#[test]
fn an_entry_that_is_not_ptr_to_i64_is_refused() {
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("f", &[Type::I64], &[Type::I64]);
    let mut b = pb.define(f);
    let x = b.params()[0];
    b.ret(&[x]);
    drop(b);
    let p = pb.finish();
    assert!(matches!(compile_entries(&p, &[f], Options::default()), Err(gravity_wasm::Refusal::Unsupported { .. })));
}

#[test]
fn compile_keeps_its_module_bytes_without_a_guard() {
    // `compile` (the standalone module) is `compile_entries`' unguarded form with the one export named `entry`.
    let (p, [e0, _]) = two_entries();
    let a = gravity_wasm::compile(&p, e0).unwrap();
    let b = gravity_wasm::compile(&p, e0).unwrap();
    assert_eq!(a, b);
    let mut rig = Rig::new();
    rig.write_i64(DATA, 5);
    let inst = instantiate(&mut rig, &a);
    let f = inst.get_typed_func::<i32, i64>(&mut rig.store, gravity_wasm::ENTRY_EXPORT).unwrap();
    assert_eq!(f.call(&mut rig.store, DATA as i32).unwrap(), 6);
}

/// The overflow host: counts its calls and raises (a trap here; the server's raises the statement's 54001 as an unwind).
fn overflow_host(rig: &mut Rig, calls: Arc<AtomicU32>) -> u32 {
    let ty = FuncType::new(rig.store.engine(), [], []);
    let wf = WFunc::new(&mut rig.store, ty, move |_, _, _| {
        calls.fetch_add(1, Ordering::SeqCst);
        Err(wasmtime::Error::msg("stack depth limit exceeded"))
    });
    let slot = rig.table.grow(&mut rig.store, 1, Ref::Func(None)).unwrap();
    rig.table.set(&mut rig.store, slot, Ref::Func(Some(wf))).unwrap();
    slot as u32
}

#[test]
fn a_frame_below_the_floor_calls_the_overflow_host_first() {
    let (p, [e0, e1]) = two_entries();
    let mut rig = Rig::new();
    let calls = Arc::new(AtomicU32::new(0));
    let overflow = overflow_host(&mut rig, calls.clone());
    // floor 64 bytes under the stack top: one 32-byte frame fits, a frame from 16 bytes above the floor does not
    let floor = STACK_TOP - 64;
    let bytes = compile_entries(&p, &[e0, e1], Options { stack_guard: Some(StackGuard { floor, overflow }), import_hosts: false }).unwrap();
    rig.write_i64(DATA, 7);
    let inst = instantiate(&mut rig, &bytes);
    let first = install(&mut rig, &inst, 2);
    assert_eq!(call_slot(&mut rig, first, DATA), Ok(8));
    assert_eq!(calls.load(Ordering::SeqCst), 0, "a frame above the floor never calls the host");
    rig.sp.set(&mut rig.store, Val::I32((floor + 16) as i32)).unwrap();
    let err = call_slot(&mut rig, first + 1, DATA).unwrap_err();
    assert!(err.contains("stack depth limit exceeded"), "{err}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(rig.sp_value(), (floor + 16) as i32, "the overflow is raised before the stack pointer moves");
}

#[test]
fn a_frame_that_would_wrap_below_zero_calls_the_overflow_host() {
    let (p, [e0, e1]) = two_entries();
    let mut rig = Rig::new();
    let calls = Arc::new(AtomicU32::new(0));
    let overflow = overflow_host(&mut rig, calls.clone());
    let bytes = compile_entries(&p, &[e0, e1], Options { stack_guard: Some(StackGuard { floor: 0, overflow }), import_hosts: false }).unwrap();
    let inst = instantiate(&mut rig, &bytes);
    let first = install(&mut rig, &inst, 2);
    rig.sp.set(&mut rig.store, Val::I32(8)).unwrap();
    assert!(call_slot(&mut rig, first, DATA).is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 1, "fp = 8 - 32 wraps above the old stack pointer: refused");
}

/// `e(ctx)` = host `add7(load(ctx))` through a 32-byte frame (so the guard has a prologue).
fn host_entry() -> (Program, Func) {
    let mut pb = ProgramBuilder::new();
    let h = pb.declare_host("add7", &[Type::I64], Some(Type::I64), 0, HostFlags::default());
    let e = pb.declare_func("e", &[Type::Ptr], &[Type::I64]);
    let mut b = pb.define(e);
    let ctx = b.params()[0];
    let slot = b.declare_stack_slot(32, 8);
    let at = b.stack_addr(slot);
    let x = b.load(Type::I64, ctx, 0);
    b.store(at, 0, x);
    let y = b.load(Type::I64, at, 0);
    let r = b.call_host(h, &[y], Some(Type::I64)).unwrap();
    b.ret(&[r]);
    drop(b);
    (pb.finish(), e)
}

/// The embedder's install in import mode: each `env.host_<n>` import resolved from table slot `n` (no table imported).
fn instantiate_imports(rig: &mut Rig, bytes: &[u8]) -> Instance {
    let module = Module::new(rig.store.engine(), bytes).unwrap_or_else(|e| panic!("module: {e:?}"));
    let mut imports: Vec<Extern> = Vec::new();
    for imp in module.imports() {
        let name = imp.name();
        imports.push(match name {
            gravity_wasm::MEMORY_IMPORT => rig.memory.into(),
            gravity_wasm::STACK_POINTER_IMPORT => rig.sp.into(),
            n if n.starts_with("host_") => match rig.table.get(&mut rig.store, n[5..].parse().unwrap()) {
                Some(Ref::Func(Some(f))) => f.into(),
                other => panic!("{n}: {other:?}"),
            },
            n => panic!("unexpected import {n}"),
        });
    }
    Instance::new(&mut rig.store, &module, &imports).unwrap_or_else(|e| panic!("instantiate: {e:?}"))
}

#[test]
fn import_mode_reaches_hosts_through_function_imports_and_imports_no_table() {
    let (p, e) = host_entry();
    let mut rig = Rig::new();
    let p = rig.bind(&p, vec![("add7", Box::new(|_m: &mut [u8], a: &[u64]| a[0].wrapping_add(7)) as HostImpl)]);
    let bytes = compile_entries(&p, &[e], Options { stack_guard: None, import_hosts: true }).unwrap();
    let module = Module::new(rig.store.engine(), &bytes).unwrap();
    let names: Vec<String> = module.imports().map(|i| i.name().to_string()).collect();
    assert!(!names.iter().any(|n| n == gravity_wasm::TABLE_IMPORT), "{names:?}");
    assert_eq!(names.iter().filter(|n| n.starts_with("host_")).count(), 1, "{names:?}");
    rig.write_i64(DATA, 35);
    let inst = instantiate_imports(&mut rig, &bytes);
    let first = install(&mut rig, &inst, 1);
    assert_eq!(call_slot(&mut rig, first, DATA), Ok(42));
}

#[test]
fn import_mode_stack_floor_calls_the_imported_overflow_host() {
    let (p, e) = host_entry();
    let mut rig = Rig::new();
    let p = rig.bind(&p, vec![("add7", Box::new(|_m: &mut [u8], a: &[u64]| a[0].wrapping_add(7)) as HostImpl)]);
    let calls = Arc::new(AtomicU32::new(0));
    let overflow = overflow_host(&mut rig, calls.clone());
    let floor = STACK_TOP - 64;
    let bytes = compile_entries(&p, &[e], Options { stack_guard: Some(StackGuard { floor, overflow }), import_hosts: true }).unwrap();
    rig.write_i64(DATA, 1);
    let inst = instantiate_imports(&mut rig, &bytes);
    let first = install(&mut rig, &inst, 1);
    assert_eq!(call_slot(&mut rig, first, DATA), Ok(8));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    rig.sp.set(&mut rig.store, Val::I32((floor + 16) as i32)).unwrap();
    assert!(call_slot(&mut rig, first, DATA).unwrap_err().contains("stack depth limit exceeded"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
