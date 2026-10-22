Borrowed callbacks retain their concrete callable type and support every Rust
function arity. Shared callbacks can be called directly. Mutable callbacks use
caller-owned `RefCell` storage and a guard, so copied handles cannot alias a
mutable capture. An overlapping mutable invocation returns `BorrowMutError`
through `try_with_mut`/`try_borrow_mut`; the other entry points panic. The borrow
is released when an invocation returns or unwinds. A guard can be mapped with
`RefMut::map` when the callback returns a reference into its own mutable state.

```
use tpde_util::function_ref::{FunctionRef, FunctionRefMut};
use core::cell::RefCell;
let offset = 2;
let shared = |value: u64| value + offset;
let callback = FunctionRef::new(&shared);
let copy = callback;
assert_eq!(copy(40), 42);
let mut total = 0;
let mutable = RefCell::new(|value: u64| { total += value; total });
let callback = FunctionRefMut::new(&mutable);
let copy = callback;
assert_eq!(callback.with_mut(|f| f(20)), 20);
assert_eq!(copy.with_mut(|f| f(22)), 42);
```

A callback cannot escape its borrowed callable.

```compile_fail
use tpde_util::function_ref::FunctionRef;
let callback;
{
    let captured = String::from("symbol");
    let callable = || captured.len();
    callback = FunctionRef::new(&callable);
}
let _ = callback();
```

A copied mutable callback cannot escape its caller-owned storage.

```compile_fail
use tpde_util::function_ref::FunctionRefMut;
use core::cell::RefCell;
let callback;
{
    let mut state = 0;
    let callable = RefCell::new(|| { state += 1; state });
    let borrowed = FunctionRefMut::new(&callable);
    callback = borrowed;
}
let _ = callback.with_mut(|f| f());
```

Calls retain their argument and return types.

```compile_fail
use tpde_util::function_ref::FunctionRef;
let callable = |value: u64| value + 1;
let callback = FunctionRef::new(&callable);
let _ = callback("wrong argument");
```

```compile_fail
use tpde_util::function_ref::FunctionRefMut;
use core::cell::RefCell;
let mut state = 0;
let callable = RefCell::new(|value: u64| { state += value; state });
let callback = FunctionRefMut::new(&callable);
let _: &str = callback.with_mut(|f| f(1));
```

Mutable callbacks require the guarded interface.

```compile_fail
use tpde_util::function_ref::FunctionRef;
let mut state = 0;
let callable = || { state += 1; state };
let callback = FunctionRef::new(&callable);
let _ = callback();
```

References into a mutable callable cannot outlive their borrow guard.

```compile_fail
use tpde_util::function_ref::FunctionRefMut;
use core::cell::RefCell;
let callable = RefCell::new(41u64);
let callback = FunctionRefMut::new(&callable);
let escaped = callback.with_mut(|value| value);
*escaped += 1;
```

Construction keeps the concrete callable type; non-callable targets cannot be
invoked.

```compile_fail
use tpde_util::function_ref::FunctionRef;
let value = 17u64;
let callback = FunctionRef::new(&value);
let _ = callback();
```
