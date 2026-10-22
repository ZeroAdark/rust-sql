The writer lends its target and text through exclusive module borrows. Its target
owns budgeted veneer storage, retains capacity between functions, and releases it
when the owner is dropped. Instruction emission and fixup dispatch use the real
A64 target through `ModuleWriter`; direct base calls bypass architecture hooks.

A failure while growing or creating a table abandons the current function.
`begin_func` clears its counters and table handles before reuse. A failure during
`finish_func` abandons the module: earlier tables may already have added symbols
or relocations, so drop the module, reset the assembler, and begin a new module.
Checked refusals before any effect, including a zero instruction or oversized
explicit advancement, preserve the existing output.

An owner cannot outlive its allocation context or overlap its reset.

```compile_fail,E0502
use tpde_function_writer_a64::A64Target;
let mut context = mcx::MemoryContext::new("borrowed target");
let target = A64Target::new(context.mcx());
context.reset();
drop(target);
```

A module holds exclusive access to the architecture target.

```compile_fail,E0499
use tpde_function_writer_a64::{A64Target, FunctionWriterA64};
fn rejected<'m>(writer: &mut FunctionWriterA64<'m>, assembler: &mut tpde_assembler::AssemblerElf<'m>) {
    let module = writer.begin_module(assembler).unwrap();
    writer.target_mut().before_begin_func();
    drop(module);
}
use tpde_function_writer::FunctionWriterTarget;
```

The table-fill callback cannot keep the borrowed label allocation.

```compile_fail,E0521
use tpde_function_writer::{Label, ModuleWriter};
use tpde_function_writer_a64::A64Target;
fn rejected(module: &mut ModuleWriter<'_, '_, '_, A64Target<'_>>) {
    let mut escaped: Option<&mut [Label]> = None;
    module.create_jump_table(1, tpde_core::Reg::new(0), tpde_core::Reg::new(1), false,
        |labels| escaped = Some(labels)).unwrap();
    escaped.unwrap()[0] = Label(0);
}
```

Lending the target and base prevents simultaneous module emission.

```compile_fail,E0499
use tpde_function_writer::ModuleWriter;
use tpde_function_writer_a64::{A64Target, A64WriteInst};
fn rejected(module: &mut ModuleWriter<'_, '_, '_, A64Target<'_>>) {
    let (target, _) = module.parts_mut();
    module.write_inst(0xd503201f).unwrap();
    assert!(core::mem::size_of_val(target) > 0);
}
```

The real owner supports complete module reset and reuse.

```
use tpde_assembler::{Architecture, AssemblerElf, SectionKind};
use tpde_function_writer_a64::{A64Target, A64WriteInst, FunctionWriterA64};
let context = mcx::MemoryContext::new("A64 module");
let mut assembler = AssemblerElf::new(context.mcx(), Architecture::Aarch64).unwrap();
let mut writer = FunctionWriterA64::new(context.mcx(), A64Target::new(context.mcx()));
for _ in 0..2 {
    assembler.reset().unwrap();
    let section = assembler.get_default_section(SectionKind::Text).unwrap();
    let mut module = writer.begin_module(&mut assembler).unwrap();
    module.switch_section(section).unwrap();
    module.begin_func(4, 64).unwrap();
    module.write_inst(0xd503201f).unwrap();
    module.finish_func().unwrap();
    module.flush().unwrap();
    module.end_module().unwrap();
}
```

The scoped table-fill operation remains usable.

```
use tpde_function_writer::ModuleWriter;
use tpde_function_writer_a64::A64Target;
fn accepted(module: &mut ModuleWriter<'_, '_, '_, A64Target<'_>>) -> types_error::PgResult<()> {
    let label = module.label_create()?;
    module.create_jump_table(2, tpde_core::Reg::new(0), tpde_core::Reg::new(1), true,
        |labels| labels.fill(label))?;
    let off = module.offset() as u32;
    module.label_place(label, off)
}
```

The CIE's byte-distance convention is explicit.

```
use tpde_function_writer::FunctionWriterTarget;
use tpde_function_writer_a64::A64Target;
assert_eq!(A64Target::CIE.code_alignment_factor, 4);
assert_eq!(A64Target::CIE.data_alignment_factor, 120);
assert_eq!(A64Target::CIE.instrs, &[0x0c, 31, 0]);
```
