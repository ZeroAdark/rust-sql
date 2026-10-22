A register file owns fixed, inline state without allocation or cleanup. Mutations
need exclusive access, and register/value identifiers cannot be interchanged.

```compile_fail,E0499
use tpde_core::{Reg, RegisterFile};
let mut file = RegisterFile::<2, 32>::new();
let first = &mut file;
let second = &mut file;
first.mark_clobbered(Reg::new(0));
second.reset();
```

```compile_fail,E0308
use tpde_base::ValLocalIdx;
use tpde_core::RegisterFile;
let mut file = RegisterFile::<2, 32>::new();
file.mark_clobbered(ValLocalIdx::new(1));
```

```compile_fail,E0080
use tpde_core::RegisterFile;
let _file = RegisterFile::<2, 64>::new();
```

```compile_fail,E0080
use tpde_core::RegisterFile;
let _file = RegisterFile::<2, 3>::new();
```

External struct construction cannot bypass dimension validation.

```compile_fail,E0451
use tpde_base::ValLocalIdx;
use tpde_core::{Assignment, RegisterFile};
let _file = RegisterFile::<1, 3> {
    allocatable: 0, used: 0, clobbered: 0, clocks: [0],
    assignments: [[Assignment { local_idx: ValLocalIdx::new(0), part: 0 }; 3]],
    lock_counts: [[0; 3]],
    _dimensions_checked: (),
};
```

Validated construction still permits public state updates through the
`Cell` fields (shared access, no loan flag; the compiler session keeps the
no-re-entrancy contract as a `CHECKED`-only guard).

```
use tpde_base::ValLocalIdx;
use tpde_core::{Assignment, RegisterFile};
let file = RegisterFile::<2, 32>::default();
file.allocatable.set(u64::MAX);
file.clocks[1].set(17);
file.assignments[1][31].set(Assignment { local_idx: ValLocalIdx::new(3), part: 2 });
file.lock_counts[1][31].set(0);
assert_eq!(file.allocatable.get(), u64::MAX);
assert_eq!(file.assignments[1][31].get().part, 2);
```
