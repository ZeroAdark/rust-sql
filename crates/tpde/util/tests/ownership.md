The writer holds an exclusive borrow until it flushes on drop.

```compile_fail
let mut context = mcx::MemoryContext::new_bump("writer");
let mut bytes = tpde_util::SmallVec::<u8, 8>::new(context.mcx());
let mut writer = tpde_util::VectorWriter::new(&mut bytes);
context.reset();
writer.write(b"x").unwrap();
```

```compile_fail
let context = mcx::MemoryContext::new_bump("writer");
let mut bytes = tpde_util::SmallVec::<u8, 8>::new(context.mcx());
let mut writer = tpde_util::VectorWriter::new(&mut bytes);
bytes.clear();
writer.write(b"x").unwrap();
```

```compile_fail
let context = mcx::MemoryContext::new_bump("writer");
let mut bytes = tpde_util::SmallVec::<u8, 8>::new(context.mcx());
let writer = tpde_util::VectorWriter::new(&mut bytes);
let mut moved = writer;
drop(writer);
moved.write(b"x").unwrap();
```

```compile_fail
let _ = tpde_util::misc::cnt_tz(1.0f64);
```

## Assertion policy (`TPDE_ASSERT`)

`tpde_assert!`, `tpde_assert_eq!` and `tpde_assert_ne!` mirror upstream
`base.hpp`: `assert(...)` under `#ifndef NDEBUG`, nothing under `NDEBUG`.
`CHECKED` is upstream's `WithAsserts`: on with `debug_assertions` (dev, Miri),
the `checked` feature, or an address-sanitizer build; off in `dist` (a doctest
binary is unoptimized, so only the library's `CHECKED` is authoritative; the
unit test `checked_matches_the_build_stage` pins it to the stage's cfg). The
macro panics exactly when `CHECKED` is on, and the condition is never
evaluated otherwise, so its arguments must be side-effect free.

```rust
let panicked = std::panic::catch_unwind(|| tpde_util::tpde_assert!(false, "contract")).is_err();
assert_eq!(panicked, tpde_util::CHECKED);
tpde_util::tpde_assert_eq!(1 + 1, 2);
tpde_util::tpde_assert_ne!(1, 2);
```

Ownership contracts that guard `unsafe` code or a typed refusal stay plain
`assert!`; `unreachable!` arms upstream marks `TPDE_UNREACHABLE` stay panics
in every build, never undefined behaviour.
