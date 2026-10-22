The X64 writer retains exclusive ownership of its module output.

```compile_fail,E0499
use tpde_function_writer_x64::{FunctionWriterX64, X64Target};
fn rejected<'m>(context: mcx::Mcx<'m>, assembler: &mut tpde_assembler::AssemblerElf<'m>) {
    let mut writer = FunctionWriterX64::new(context, X64Target::new());
    let mut module = writer.begin_module(assembler).unwrap();
    assembler.reset().unwrap();
    module.end_module().unwrap();
}
```

Jump-table labels cannot survive the fill callback.

```compile_fail,E0521
use tpde_function_writer::{Label, ModuleWriter};
use tpde_function_writer_x64::X64Target;
fn rejected(module: &mut ModuleWriter<'_, '_, '_, X64Target>) {
    let mut escaped = None;
    module.create_jump_table(1, tpde_core::Reg::new(0), tpde_core::Reg::new(1), false, |labels| {
        escaped = Some(labels);
    }).unwrap();
    assert!(escaped.is_some());
}
```

Output views prevent mutation while they are live.

```compile_fail,E0502
use tpde_function_writer::ModuleWriter;
use tpde_function_writer_x64::X64Target;
fn rejected(module: &mut ModuleWriter<'_, '_, '_, X64Target>) {
    let bytes = module.data().unwrap();
    module.begin_func(1, 8).unwrap();
    assert!(!bytes.is_empty());
}
```

Table storage is reused after the callback releases its borrow.

```
use tpde_function_writer::{Label, ModuleWriter};
use tpde_function_writer_x64::X64Target;
fn accepted(module: &mut ModuleWriter<'_, '_, '_, X64Target>, label: Label) {
    module.create_jump_table(2, tpde_core::Reg::new(12), tpde_core::Reg::new(13), true, |labels| {
        labels.fill(label);
    }).unwrap();
    assert_eq!(module.jump_table_labels(0).unwrap(), &[label; 2]);
    module.begin_func(1, 8).unwrap();
}
```

Dropping the module releases the assembler for reuse.

```
use tpde_function_writer_x64::{FunctionWriterX64, X64Target};
fn accepted<'m>(context: mcx::Mcx<'m>, assembler: &mut tpde_assembler::AssemblerElf<'m>) {
    let mut writer = FunctionWriterX64::new(context, X64Target::new());
    let mut module = writer.begin_module(assembler).unwrap();
    module.end_module().unwrap();
    drop(module);
    assembler.reset().unwrap();
}
```
