Assignment slots retain their capacity across function loans. Each occupied slot
owns one allocator token; removing the token transfers ownership to the caller.
Views borrow the slot, and allocator reset requires every remaining token borrow
to end. Abandoning a loan discards its tokens without recycling their allocations.

```
use mcx::MemoryContext;
use tpde_compiler::assignment_store_guard::AssignmentSlotStorage;
use tpde_core::value_assignment::{with_assignment_allocator, AssignmentInit};

let context = MemoryContext::new("assignment-slots");
let mut storage = AssignmentSlotStorage::new(context.mcx());
with_assignment_allocator(context.mcx(), |mut allocator| {
    for _ in 0..2 {
        let value = storage.with_slots(&allocator, 1, |mut slots| {
            slots.insert(0, allocator.allocate(1).unwrap()
                .initialize(AssignmentInit::default(), |_| 42));
            assert_eq!(slots.get(0).unwrap().part(0).get(), 42);
            slots.take(0).unwrap()
        }).unwrap();
        allocator.deallocate(value);
        allocator.reset();
    }
});
```

An assignment view cannot escape the slot loan.

```compile_fail
use mcx::MemoryContext;
use tpde_compiler::assignment_store_guard::AssignmentSlotStorage;
use tpde_core::value_assignment::{with_assignment_allocator, AssignmentInit};

let context = MemoryContext::new("assignment-slots");
let mut storage = AssignmentSlotStorage::new(context.mcx());
with_assignment_allocator(context.mcx(), |allocator| {
    let escaped = storage.with_slots(&allocator, 1, |mut slots| {
        slots.insert(0, allocator.allocate(1).unwrap()
            .initialize(AssignmentInit::default(), |_| 42));
        slots.get(0).unwrap()
    }).unwrap();
    assert_eq!(escaped.part(0).get(), 42);
});
```

Copying a part view preserves the slot borrow.

```compile_fail
use mcx::MemoryContext;
use tpde_compiler::assignment_store_guard::AssignmentSlotStorage;
use tpde_core::value_assignment::{with_assignment_allocator, AssignmentInit};

let context = MemoryContext::new("assignment-slots");
let mut storage = AssignmentSlotStorage::new(context.mcx());
with_assignment_allocator(context.mcx(), |allocator| {
    let escaped = storage.with_slots(&allocator, 1, |mut slots| {
        slots.insert(0, allocator.allocate(1).unwrap()
            .initialize(AssignmentInit::default(), |_| 0x0200));
        slots.part(0, 0).unwrap()
    }).unwrap();
    assert!(escaped.modified());
});
```

An occupied slot cannot be removed while a part view remains in use.

```compile_fail
use mcx::MemoryContext;
use tpde_compiler::assignment_store_guard::AssignmentSlotStorage;
use tpde_core::value_assignment::{with_assignment_allocator, AssignmentInit};

let context = MemoryContext::new("assignment-slots");
let mut storage = AssignmentSlotStorage::new(context.mcx());
with_assignment_allocator(context.mcx(), |allocator| {
    storage.with_slots(&allocator, 1, |mut slots| {
        slots.insert(0, allocator.allocate(1).unwrap()
            .initialize(AssignmentInit::default(), |_| 0x0200));
        let part = slots.part(0, 0).unwrap();
        let value = slots.take(0).unwrap();
        assert!(part.modified());
        allocator.deallocate(value);
    }).unwrap();
});
```

Taking a token preserves its allocator borrow after the slot loan ends.

```compile_fail
use mcx::MemoryContext;
use tpde_compiler::assignment_store_guard::AssignmentSlotStorage;
use tpde_core::value_assignment::{with_assignment_allocator, AssignmentInit};

let context = MemoryContext::new("assignment-slots");
let mut storage = AssignmentSlotStorage::new(context.mcx());
with_assignment_allocator(context.mcx(), |mut allocator| {
    let value = storage.with_slots(&allocator, 1, |mut slots| {
        slots.insert(0, allocator.allocate(1).unwrap()
            .initialize(AssignmentInit::default(), |_| 42));
        slots.take(0).unwrap()
    }).unwrap();
    allocator.reset();
    assert_eq!(value.part(0).get(), 42);
});
```

A slot loan cannot accept a token from another allocator brand.

```compile_fail
use mcx::MemoryContext;
use tpde_compiler::assignment_store_guard::AssignmentSlotStorage;
use tpde_core::value_assignment::{with_assignment_allocator, AssignmentInit};

let context = MemoryContext::new("assignment-slots");
let mut storage = AssignmentSlotStorage::new(context.mcx());
with_assignment_allocator(context.mcx(), |first| {
    with_assignment_allocator(context.mcx(), |second| {
        storage.with_slots(&first, 1, |mut slots| {
            let value = second.allocate(1).unwrap()
                .initialize(AssignmentInit::default(), |_| 42);
            slots.insert(0, value);
        }).unwrap();
    });
});
```
