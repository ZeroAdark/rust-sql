Rust borrows express the source's lifetime-bound and separate-storage contracts.
No raw-pointer optimizer promise is exposed as a safe function.

```compile_fail
fn bound<'a>(owner: &'a [u8]) -> &'a u8 { &owner[0] }
let escaped;
{ let owner = [7]; escaped = bound(&owner); }
assert_eq!(*escaped, 7);
```

```compile_fail
fn separate(a: &mut u8, b: &mut u8) { *a += 1; *b += 2; }
let mut value = 0;
separate(&mut value, &mut value);
```

```compile_fail
use tpde_base::{LogConfig, Level, set_log_config};
let prefix = String::from("temporary");
set_log_config(LogConfig { level: Level::Info,
    sink: |_, message| println!("{} {}", prefix, message) });
```

```compile_fail
use tpde_base::ValLocalIdx;
let index: ValLocalIdx = 7u32;
```

```compile_fail
use tpde_base::ValLocalIdx;
let index = ValLocalIdx::new(7);
let value = index + 1u32;
```
