Parts and disowned references borrow their owning reference. A callback can read
assignment metadata but cannot retain its allocation token.

Like upstream `AssignmentData::assignment` and `ValueData::assignment`, a
reference and its parts carry the assignment pointer read from the slot table at
construction and read metadata through it without another table lookup. The
pointer stays valid for the reference's lifetime: an assignment is recycled only
by `free_assignment`, reached when the reference count drops to zero (each
mode-1/2 reference holds one count), by the delayed free of an assignment whose
count already reached zero (construction refuses `pending_free`), or after the
byval prologue has `reset()` the only reference; variable references are
discarded when the function ends, which no reference to the session outlives.
Debug builds re-read the slot on every access and abort if it no longer holds
the pinned pointer.

```compile_fail
use tpde_compiler::{guard_machine::GuardMachine, value_ref::ValueRef, value_part_ref::ValuePartRef};
fn escape<'c, M: GuardMachine<2>>(value: ValueRef<'c, M, 2>) -> ValuePartRef<'c, M, 2> {
    value.part(0)
}
```

```compile_fail
use tpde_compiler::{guard_machine::GuardMachine, value_ref::ValueRef};
fn reset_with_live_part<M: GuardMachine<2>>(value: &mut ValueRef<'_, M, 2>) {
    let part = value.part(0);
    value.reset();
    drop(part);
}
```

```compile_fail
use tpde_compiler::{guard_machine::GuardMachine, value_ref::ValueRef};
fn escape_disowned<'c, M: GuardMachine<2>>(value: ValueRef<'c, M, 2>) -> ValueRef<'c, M, 2> {
    value.disowned()
}
```

```compile_fail
use tpde_compiler::{guard_machine::GuardMachine, value_ref::ValueRef};
use tpde_core::value_assignment::ValueAssignment;
fn escape_assignment<'a, M: GuardMachine<2>>(value: &'a ValueRef<'_, M, 2>) -> &'a ValueAssignment {
    value.with_assignment(|assignment| assignment)
}
```

```compile_fail
use tpde_compiler::{guard_machine::GuardMachine, value_part_ref::ValuePartRef};
fn duplicate<M: GuardMachine<2>>(part: ValuePartRef<'_, M, 2>) {
    let first = part;
    let second = part;
    drop((first, second));
}
```

```compile_fail
use tpde_compiler::{guard_machine::GuardMachine, value_part_ref::ValuePartRef};
fn reset_borrowed_register<M: GuardMachine<2>>(part: &mut ValuePartRef<'_, M, 2>) {
    let borrowed = part.get_unowned_ref();
    part.reset();
    drop(borrowed);
}
```

```compile_fail
use tpde_compiler::{guard_machine::GuardMachine, value_part_ref::ValuePartRef};
use tpde_core::AssignmentPartRef;
fn escape_part<'a, M: GuardMachine<2>>(part: &'a ValuePartRef<'_, M, 2>) -> AssignmentPartRef<'a, 'a, 'a> {
    part.with_assignment(|assignment| assignment)
}
```

```
use tpde_compiler::{guard_machine::GuardMachine, value_ref::ValueRef};
fn release_after_parts<M: GuardMachine<2>>(value: &mut ValueRef<'_, M, 2>) {
    {
        let first = value.part_unowned(0);
        let second = value.part_unowned(1);
        drop((first, second));
    }
    value.reset();
}
```

A shorter-lived destination can reuse a longer-lived operand without extending
the destination's borrow into the operand owner.

```
use tpde_compiler::{guard_machine::GuardMachine, ValuePartRef};
fn reuse<'short, 'long: 'short, M: GuardMachine<2>>(
    destination: &mut ValuePartRef<'short, M, 2>,
    source: &mut ValuePartRef<'long, M, 2>,
) {
    destination.alloc_try_reuse(source).unwrap();
}
```

Transferring a part cannot extend the lifetime of borrowed constant words.

```compile_fail
use tpde_compiler::{guard_machine::GuardMachine, ValuePartRef};
fn cannot_extend<'long, 'short, M: GuardMachine<2>>(
    destination: &mut ValuePartRef<'long, M, 2>,
    source: &mut ValuePartRef<'short, M, 2>,
) {
    destination.set_value_from(source).unwrap();
}
```
