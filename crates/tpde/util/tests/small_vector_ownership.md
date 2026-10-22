Small vectors retain their memory context borrow. `SmallVec` contains only
values that need no destruction; `SmallVectorGuard` owns resource values and
releases them when it is dropped. A move transfers the owner.

```compile_fail
let context = mcx::MemoryContext::new_bump("vector");
let mut vector = tpde_util::SmallVec::<u64, 4>::new(context.mcx());
vector.push(7).unwrap();
let moved = vector;
vector.push(9).unwrap();
assert_eq!(moved.len(), 1);
```

```compile_fail
let mut context = mcx::MemoryContext::new_bump("vector");
let mut vector = tpde_util::SmallVec::<u64, 4>::new(context.mcx());
context.reset();
vector.push(7).unwrap();
```

```compile_fail
let context = mcx::MemoryContext::new_bump("vector");
let mut vector = tpde_util::SmallVec::<String, 4>::new(context.mcx());
vector.push(String::from("resource")).unwrap();
```

```compile_fail
let context = mcx::MemoryContext::new_bump("vector");
let vector = tpde_util::SmallVec::<u64, 4>::new(context.mcx());
let copy = Clone::clone(&vector);
assert_eq!(copy.len(), 0);
```

An unfinished uninitialized resize exclusively borrows the vector. Dropping
the resize cancels growth; `assume_init` commits the suffix after every exposed
slot has been initialized.

```compile_fail
let context = mcx::MemoryContext::new_bump("vector");
let mut vector = tpde_util::SmallVec::<u64, 4>::new(context.mcx());
let mut suffix = vector.resize_uninitialized(2).unwrap();
vector.push(7).unwrap();
suffix[0].write(9);
```

Resource guards cannot outlive their context or enter a no-drop arena owner.

```compile_fail
let mut vector;
{
    let context = mcx::MemoryContext::new_bump("vector");
    vector = tpde_util::SmallVectorGuard::<String, 4>::new(context.mcx());
}
vector.push(String::from("resource")).unwrap();
```

```compile_fail
let context = mcx::MemoryContext::new_bump("vector");
let vector = tpde_util::SmallVectorGuard::<String, 4>::new(context.mcx());
let _ = mcx::ArenaForget::new(vector);
```

Forward traversal counts copyable positions before constructing elements.
Resource owners must be supplied by the factory, not materialized by its cursor.

```compile_fail
let context = mcx::MemoryContext::new_bump("vector");
let mut vector = tpde_util::SmallVectorGuard::<String, 4>::new(context.mcx());
let source = [String::from("resource")];
vector.append_forward(source.into_iter()).unwrap();
```

```compile_fail
let context = mcx::MemoryContext::new_bump("vector");
let mut vector = tpde_util::SmallVectorGuard::<String, 4>::new(context.mcx());
let source = [String::from("resource")];
vector.append_forward_with(source.into_iter(), |value| value).unwrap();
```
