Elements remain borrowed across append; clearing requires exclusive access.

```compile_fail
let context = mcx::MemoryContext::new_bump("segmented");
let mut values = tpde_util::SegmentedVector::<u64, 3>::new(context.mcx());
let first = values.push_back(7).unwrap();
values.clear();
assert_eq!(*first, 7);
```

```compile_fail
let mut context = mcx::MemoryContext::new_bump("segmented");
let values = tpde_util::SegmentedVector::<u64, 3>::new(context.mcx());
values.push_back(7).unwrap();
context.reset();
assert_eq!(values[0], 7);
```

```compile_fail
let context = mcx::MemoryContext::new_bump("segmented");
let values = tpde_util::SegmentedVector::<u64, 3>::new(context.mcx());
let first = values.push_back(7).unwrap();
drop(values);
assert_eq!(*first, 7);
```

```compile_fail
let context = mcx::MemoryContext::new_bump("segmented");
let values = tpde_util::SegmentedVector::<u64, 3>::new(context.mcx());
let moved = values;
values.push_back(1).unwrap();
drop(moved);
```

```compile_fail
let context = mcx::MemoryContext::new_bump("segmented");
let values = tpde_util::SegmentedVector::<u64, 3>::new(context.mcx());
let _copy = values.clone();
```

```compile_fail
let context = mcx::MemoryContext::new_bump("segmented");
let values = tpde_util::SegmentedVector::<u64, 3>::new(context.mcx());
let _forgotten = mcx::ArenaForget::new(values);
```
