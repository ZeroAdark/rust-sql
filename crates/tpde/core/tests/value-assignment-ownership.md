Assignment storage is reclaimed by its allocator. The unique no-drop token can
move into a compiler table; views borrow it and prevent recycling or reset.

```compile_fail
use mcx::MemoryContext;
use tpde_core::value_assignment::{with_assignment_allocator, AssignmentInit};
let context = MemoryContext::new("assignment");
with_assignment_allocator(context.mcx(), |allocator| {
let value = allocator.allocate(1).unwrap().initialize(AssignmentInit::default(), |_| 0);
allocator.deallocate(value);
let _ = value.part(0);
});
```

```compile_fail
use mcx::MemoryContext;
use tpde_core::value_assignment::{with_assignment_allocator, AssignmentInit};
let context = MemoryContext::new("assignment");
with_assignment_allocator(context.mcx(), |mut allocator| {
let value = allocator.allocate(1).unwrap().initialize(AssignmentInit::default(), |_| 0);
allocator.reset();
value.part(0).set(1);
});
```

```compile_fail
use mcx::MemoryContext;
use tpde_core::value_assignment::{with_assignment_allocator, AssignmentInit};
let context = MemoryContext::new("assignment");
with_assignment_allocator(context.mcx(), |allocator| {
let value = allocator.allocate(1).unwrap().initialize(AssignmentInit::default(), |_| 0);
let part = value.part(0);
allocator.deallocate(value);
part.set(1);
});
```

```compile_fail
use mcx::MemoryContext;
use tpde_core::value_assignment::with_assignment_allocator;
let context = MemoryContext::new("assignment");
let value = with_assignment_allocator(context.mcx(), |allocator| {
    allocator.allocate(1).unwrap()
});
let _ = value.part_count();
```

```compile_fail
use mcx::MemoryContext;
use tpde_core::value_assignment::{with_assignment_allocator, AssignmentInit};
let context = MemoryContext::new("assignment");
with_assignment_allocator(context.mcx(), |allocator| {
let value = allocator.allocate(1).unwrap().initialize(AssignmentInit::default(), |_| 0);
let duplicate: tpde_core::value_assignment::Assignment<'_, '_> = value.clone();
});
```

Independent allocator brands cannot exchange ownership, even in nested scopes.

```compile_fail
use mcx::MemoryContext;
use tpde_core::value_assignment::{with_assignment_allocator, AssignmentInit};
let context = MemoryContext::new("assignment");
with_assignment_allocator(context.mcx(), |first| {
    let value = first.allocate(1).unwrap().initialize(AssignmentInit::default(), |_| 0);
    with_assignment_allocator(context.mcx(), |second| {
        let local = second.allocate(1).unwrap().initialize(AssignmentInit::default(), |_| 2);
        second.deallocate(local);
        second.deallocate(value);
    });
});
```

The allocation epoch and its compiler table remain inside one scope, while reset
retains the first slab between functions.

```
use mcx::MemoryContext;
use tpde_core::value_assignment::{with_assignment_allocator, AssignmentInit};
let context = MemoryContext::new("assignment");
with_assignment_allocator(context.mcx(), |mut allocator| {
    for _ in 0..3 {
        let value = allocator.allocate(1).unwrap().initialize(AssignmentInit::default(), |_| 7);
        assert_eq!(value.part(0).get(), 7);
        allocator.deallocate(value);
        allocator.reset();
    }
});
```
