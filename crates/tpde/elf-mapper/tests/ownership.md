An image keeps its allocation context borrowed until its final use.

```compile_fail,E0502
use tpde_elf_mapper::MapperImage;
fn rejected(mut context: mcx::MemoryContext) {
    let image = MapperImage::new(context.mcx()).unwrap();
    context.reset();
    assert_eq!(image.size(), 0);
}
```

An image cannot escape the context that stores its relocation state.

```compile_fail,E0515
use tpde_elf_mapper::MapperImage;
fn rejected() -> MapperImage<'static> {
    let context = mcx::MemoryContext::new_bump("mapper-image");
    MapperImage::new(context.mcx()).unwrap()
}
```

The mapping owner retains its context borrow through destruction.

```compile_fail,E0502
use tpde_elf_mapper::ElfMapper;
fn rejected(mut context: mcx::MemoryContext) {
    let mapper = ElfMapper::new(context.mcx()).unwrap();
    context.reset();
    drop(mapper);
}
```

The mapping owner cannot escape its allocation context.

```compile_fail,E0515
use tpde_elf_mapper::ElfMapper;
fn rejected() -> ElfMapper<'static> {
    let context = mcx::MemoryContext::new_bump("elf-mapper");
    ElfMapper::new(context.mcx()).unwrap()
}
```

A symbol view prevents resetting the mapping that owns its address.

```compile_fail,E0502
use tpde_assembler::SymRef;
use tpde_elf_mapper::ElfMapper;
fn rejected(mapper: &mut ElfMapper<'_>, reference: SymRef) {
    let symbol = mapper.get_sym_addr(reference).unwrap();
    mapper.reset();
    core::hint::black_box(symbol.address());
}
```

A range view prevents resetting the mapping until that view's final use.

```compile_fail,E0502
use tpde_elf_mapper::ElfMapper;
fn rejected(mapper: &mut ElfMapper<'_>) {
    let range = mapper.get_mapped_range().unwrap();
    mapper.reset();
    core::hint::black_box(range);
}
```

Publishing executable storage requires an explicit unsafe call.

```compile_fail,E0133
use tpde_assembler::AssemblerElf;
use tpde_elf_mapper::ElfMapper;
fn rejected(mapper: &mut ElfMapper<'_>, assembler: &mut AssemblerElf<'_>) {
    mapper.map(assembler, |_| 0).unwrap();
}
```

Moving a mapping owner transfers its cleanup responsibility.

```compile_fail,E0382
use tpde_elf_mapper::ElfMapper;
fn rejected(mapper: ElfMapper<'_>) {
    let moved = mapper;
    drop(mapper);
    drop(moved);
}
```

Invoking a symbol requires an explicit proof of its native ABI and effects.

```compile_fail,E0133
use tpde_elf_mapper::MappedSymbol;
fn rejected(symbol: &MappedSymbol<'_>) -> u64 {
    symbol.call2_u64(7, 11)
}
```

Borrowing mapped bytes requires an explicit proof that execution cannot mutate them.

```compile_fail,E0133
use tpde_elf_mapper::MappedRange;
fn rejected(range: &MappedRange<'_>) {
    core::hint::black_box(range.bytes());
}
```

A mapping owner cannot transfer its context and mapping guard to another thread.

```compile_fail,E0277
use tpde_elf_mapper::ElfMapper;
fn require_send<T: Send>() {}
require_send::<ElfMapper<'static>>();
```

A mapping owner cannot expose shared access across threads.

```compile_fail,E0277
use tpde_elf_mapper::ElfMapper;
fn require_sync<T: Sync>() {}
require_sync::<ElfMapper<'static>>();
```

A symbol cannot escape its mapping owner even when the allocation context is static.

```compile_fail,E0515
use tpde_assembler::SymRef;
use tpde_elf_mapper::{ElfMapper, MappedSymbol};
fn rejected(mapper: ElfMapper<'static>, reference: SymRef) -> MappedSymbol<'static> {
    mapper.get_sym_addr(reference).unwrap()
}
```

A range cannot escape its mapping owner even when the allocation context is static.

```compile_fail,E0515
use tpde_elf_mapper::{ElfMapper, MappedRange};
fn rejected(mapper: ElfMapper<'static>) -> MappedRange<'static> {
    mapper.get_mapped_range().unwrap()
}
```

Symbol and range borrows may end before a mapping reset.

```
use tpde_assembler::SymRef;
use tpde_elf_mapper::ElfMapper;
fn accepted(mapper: &mut ElfMapper<'_>, reference: SymRef) {
    if let Some(symbol) = mapper.get_sym_addr(reference) {
        core::hint::black_box(symbol.address());
    }
    if let Some(range) = mapper.get_mapped_range() {
        core::hint::black_box(range);
    }
    mapper.reset();
}
```

Dropping a pure image releases its context for reuse.

```
use tpde_elf_mapper::MapperImage;
let mut context = mcx::MemoryContext::new_bump("mapper-image");
let mut image = MapperImage::new(context.mcx()).unwrap();
assert_eq!(image.size(), 0);
image.reset();
assert!(image.permissions().is_empty());
drop(image);
context.reset();
```

An unmapped owner can reset, move, and drop before its context resets.

```
use tpde_elf_mapper::ElfMapper;
let mut context = mcx::MemoryContext::new_bump("elf-mapper");
let mut mapper = ElfMapper::new(context.mcx()).unwrap();
assert!(mapper.get_mapped_range().is_none());
mapper.reset();
let moved = mapper;
assert!(moved.get_mapped_range().is_none());
drop(moved);
context.reset();
```
