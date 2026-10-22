A module holds exclusive access to its assembler until its final use.

```compile_fail,E0499
use tpde_function_writer::{FunctionWriter, FunctionWriterTarget};
fn rejected<'m, T: FunctionWriterTarget>(writer: &mut FunctionWriter<'m, T>, assembler: &mut tpde_assembler::AssemblerElf<'m>) {
    let mut module = writer.begin_module(assembler).unwrap();
    assembler.reset().unwrap();
    module.end_module().unwrap();
}
```

The same writer cannot begin another module while its first module remains live.

```compile_fail,E0499
use tpde_function_writer::{FunctionWriter, FunctionWriterTarget};
fn rejected<'m, T: FunctionWriterTarget>(writer: &mut FunctionWriter<'m, T>, first: &mut tpde_assembler::AssemblerElf<'m>, second: &mut tpde_assembler::AssemblerElf<'m>) {
    let mut module = writer.begin_module(first).unwrap();
    let _other = writer.begin_module(second).unwrap();
    module.end_module().unwrap();
}
```

Jump-table references cannot survive the function reset that reclaims their storage.

```compile_fail,E0499
use tpde_function_writer::FunctionWriterBase;
fn rejected(base: &mut FunctionWriterBase<'_, '_, '_>) {
    let mut table = base.alloc_jump_table(2, tpde_core::Reg::new(0), tpde_core::Reg::new(1)).unwrap();
    base.begin_func().unwrap();
    table.labels_mut()[0] = tpde_function_writer::Label(0);
}
```

A mutable table excludes another mutable access to that table.

```compile_fail,E0499
use tpde_function_writer::FunctionWriterBase;
fn rejected(base: &mut FunctionWriterBase<'_, '_, '_>) {
    let mut first = base.jump_table_mut(0).unwrap();
    let mut second = base.jump_table_mut(0).unwrap();
    first.labels_mut()[0] = tpde_function_writer::Label(0);
    second.labels_mut()[0] = tpde_function_writer::Label(1);
}
```

Shared labels prevent mutation until their final use.

```compile_fail,E0502
use tpde_function_writer::FunctionWriterBase;
fn rejected(base: &mut FunctionWriterBase<'_, '_, '_>) {
    let labels = base.jump_table_labels(0).unwrap();
    base.jump_table_mut(0).unwrap().labels_mut()[0] = tpde_function_writer::Label(1);
    assert_eq!(labels[0], tpde_function_writer::Label(0));
}
```

A table's allocation extent cannot be overwritten through its header view.

```compile_fail,E0594
use tpde_function_writer::{JumpTable, JumpTableHeader};
fn rejected(mut table: JumpTable<'_>, copied_header: JumpTableHeader) {
    *table = copied_header;
}
```

Uninitialized emission borrows the writer until it is committed or cancelled.

```compile_fail,E0499
use tpde_function_writer::{ModuleWriter, FunctionWriterTarget};
fn rejected<T: FunctionWriterTarget>(module: &mut ModuleWriter<'_, '_, '_, T>) {
    let mut reservation = module.reserve_uninitialized(8).unwrap();
    module.write(42u8).unwrap();
    reservation[0].write(1);
}
```

Only an unsafe proof of initialization can publish raw encoder output.

```compile_fail,E0133
use tpde_function_writer::{ModuleWriter, FunctionWriterTarget};
fn rejected<T: FunctionWriterTarget>(module: &mut ModuleWriter<'_, '_, '_, T>) {
    let mut reservation = module.reserve_uninitialized(8).unwrap();
    reservation[0].write(1);
    reservation.commit(1).unwrap();
}
```

Dropping a module releases the assembler borrow for normal reuse.

```
use tpde_function_writer::{FunctionWriter, FunctionWriterTarget};
fn accepted<'m, T: FunctionWriterTarget>(writer: &mut FunctionWriter<'m, T>, assembler: &mut tpde_assembler::AssemblerElf<'m>) {
    let mut module = writer.begin_module(assembler).unwrap();
    module.end_module().unwrap();
    drop(module);
    assembler.reset().unwrap();
}
```

The writer keeps its allocation context borrowed through destruction.

```compile_fail,E0502
use tpde_function_writer::{FunctionWriter, FunctionWriterTarget};
fn rejected<T: FunctionWriterTarget>(mut context: mcx::MemoryContext, target: T) {
    let writer = FunctionWriter::new(context.mcx(), target);
    context.reset();
    drop(writer);
}
```

Runtime CIE instructions stay borrowed for the lifetime of the writer.

```compile_fail,E0597
use tpde_function_writer::{FunctionWriter, FunctionWriterTarget, TargetCieInfo};
fn rejected<T: FunctionWriterTarget>(context: &mcx::MemoryContext, target: T) {
    let writer;
    {
        let instructions = vec![12, 7, 8];
        let info = TargetCieInfo {
            instrs: &instructions,
            return_addr_register: 16,
            code_alignment_factor: 1,
            data_alignment_factor: 0x78,
        };
        writer = FunctionWriter::with_cie(context.mcx(), target, info);
    }
    drop(writer);
}
```

Architecture table emission requires an explicit target capability.

```compile_fail,E0599
use tpde_function_writer::{FunctionWriterTarget, ModuleWriter};
fn rejected<T: FunctionWriterTarget>(module: &mut ModuleWriter<'_, '_, '_, T>) {
    module.create_jump_table(1, tpde_core::Reg::new(0), tpde_core::Reg::new(1), false, |_| {}).unwrap();
}
```

Table-fill callbacks cannot retain their borrowed labels.

```compile_fail,E0521
use tpde_function_writer::{FunctionWriterJumpTables, ModuleWriter};
fn rejected<T: FunctionWriterJumpTables>(module: &mut ModuleWriter<'_, '_, '_, T>) {
    let mut escaped = None;
    module.create_jump_table(1, tpde_core::Reg::new(0), tpde_core::Reg::new(1), false, |labels| {
        escaped = Some(labels);
    }).unwrap();
    module.begin_func(1, 8).unwrap();
    assert!(escaped.is_some());
}
```

Table-fill callbacks exclude simultaneous writer mutation.

```compile_fail
use tpde_function_writer::{FunctionWriterJumpTables, ModuleWriter};
fn rejected<T: FunctionWriterJumpTables>(module: &mut ModuleWriter<'_, '_, '_, T>) {
    module.create_jump_table(1, tpde_core::Reg::new(0), tpde_core::Reg::new(1), false, |labels| {
        module.begin_func(1, 8).unwrap();
        labels[0] = tpde_function_writer::Label(0);
    }).unwrap();
}
```

Filling labels ends its borrow before the next function reuses storage.

```
use tpde_function_writer::{FunctionWriterJumpTables, Label, ModuleWriter};
fn accepted<T: FunctionWriterJumpTables>(module: &mut ModuleWriter<'_, '_, '_, T>) {
    let label = module.label_create().unwrap();
    module.create_jump_table(2, tpde_core::Reg::new(0), tpde_core::Reg::new(1), true, |labels| {
        labels.fill(label);
    }).unwrap();
    assert_eq!(module.jump_table_labels(0).unwrap(), &[label, label]);
    module.begin_func(1, 8).unwrap();
}
```
