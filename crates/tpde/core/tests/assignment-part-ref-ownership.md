A copyable part view borrows its unique assignment token. Copies share Cell
updates; the token cannot be recycled, moved or outlive its allocator while a
view remains usable. Header-only operations retain the source's full u32 index
domain; packed-word operations check that the backing part exists. The view
holds the token's whole-allocation pointer, as upstream `AssignmentPartRef`
holds `ValueAssignment *`; `from_raw` lets a caller that has pinned the
allocation by other means (a counted `ValueRef`) build the same view under a
documented safety contract.

```compile_fail
use mcx::MemoryContext;
use tpde_core::{assignment_part_ref::AssignmentPartRef, value_assignment::{with_assignment_allocator, AssignmentInit}};
let context = MemoryContext::new("part");
with_assignment_allocator(context.mcx(), |allocator| {
    let assignment = allocator.allocate(1).unwrap().initialize(AssignmentInit::default(), |_| 0);
    let part = AssignmentPartRef::new(&assignment, 0);
    allocator.deallocate(assignment);
    part.reset();
});
```

```compile_fail
use mcx::MemoryContext;
use tpde_core::{assignment_part_ref::AssignmentPartRef, value_assignment::{with_assignment_allocator, AssignmentInit}};
let context = MemoryContext::new("part");
with_assignment_allocator(context.mcx(), |mut allocator| {
    let assignment = allocator.allocate(1).unwrap().initialize(AssignmentInit::default(), |_| 0);
    let part = AssignmentPartRef::new(&assignment, 0);
    allocator.reset();
    part.reset();
});
```

```compile_fail
use mcx::MemoryContext;
use tpde_core::{assignment_part_ref::AssignmentPartRef, value_assignment::{with_assignment_allocator, AssignmentInit}};
let context = MemoryContext::new("part");
let part = with_assignment_allocator(context.mcx(), |allocator| {
    let assignment = allocator.allocate(1).unwrap().initialize(AssignmentInit::default(), |_| 0);
    AssignmentPartRef::new(&assignment, 0)
});
part.reset();
```

```compile_fail
use mcx::MemoryContext;
use tpde_core::{assignment_part_ref::AssignmentPartRef, value_assignment::{with_assignment_allocator, AssignmentInit}};
let context = MemoryContext::new("part");
with_assignment_allocator(context.mcx(), |allocator| {
    let assignment = allocator.allocate(1).unwrap().initialize(AssignmentInit::default(), |_| 0);
    let first = AssignmentPartRef::new(&assignment, 0);
    let copy = first;
    let moved = assignment;
    copy.set_stack_valid();
    allocator.deallocate(moved);
});
```

```
use mcx::MemoryContext;
use tpde_core::{assignment_part_ref::AssignmentPartRef, value_assignment::{with_assignment_allocator, AssignmentInit}, Reg, RegBank};
let context = MemoryContext::new("part");
with_assignment_allocator(context.mcx(), |mut allocator| {
    for _ in 0..3 {
        let assignment = allocator.allocate(1).unwrap().initialize(AssignmentInit::default(), |_| 0);
        let first = AssignmentPartRef::new(&assignment, 0);
        let copy = first;
        first.set_bank(RegBank::new(7));
        copy.set_reg(Reg::new(255));
        assert_eq!(first.get_reg().id(), 255);
        allocator.deallocate(assignment);
        allocator.reset();
    }
});
```
