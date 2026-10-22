Allocations borrow their allocator, so reset cannot invalidate live data.

```compile_fail,E0502
let context = mcx::MemoryContext::new("bump");
let mut bump = tpde_util::BumpAllocator::<64>::new(context.mcx());
let value = bump.alloc(42u64).unwrap();
bump.reset();
assert_eq!(*value, 42);
```

Guards keep borrowed constructor inputs alive through destruction.

```compile_fail,E0597
let context = mcx::MemoryContext::new("bump");
let bump = tpde_util::BumpAllocator::<64>::new(context.mcx());
let guard;
{
    let input = String::from("short lived");
    guard = bump.new_with(|| Ok(input.as_str())).unwrap();
}
assert_eq!(*guard, "short lived");
```

Arena data cannot carry destructors.

```compile_fail,E0080
let context = mcx::MemoryContext::new("bump");
let bump = tpde_util::BumpAllocator::<64>::new(context.mcx());
let _ = bump.alloc(String::from("resource"));
```

Unique ownership transfers on move.

```compile_fail,E0382
let context = mcx::MemoryContext::new("bump");
let bump = tpde_util::BumpAllocator::<64>::new(context.mcx());
let first = bump.new_with(|| Ok(42u64)).unwrap();
let second = first;
drop(first);
assert_eq!(*second, 42);
```

The outer context stays borrowed until allocator destruction.

```compile_fail,E0502
let mut context = mcx::MemoryContext::new("bump");
let bump = tpde_util::BumpAllocator::<64>::new(context.mcx());
context.reset();
drop(bump);
```
