The guard borrows its machine and owns the backup scratch register. Its saved
assignment and lock count cannot be edited or duplicated. The original fixed
register remains used while its assignment and locks are temporarily detached.

The caller must keep the original fixed-register owner alive and unchanged until
restoration. A raw `Reg` does not identify or borrow that owner; this is the
existing low-level register-machine invariant, not a borrow-enforced guarantee.
Other allocations may proceed while the guard is active. As in upstream, a
full-pressure eviction scan encountering the detached temporary rejects it.

After `begin`, the caller emits the initial copy into `backup_reg`. A successful
result restoration emits an exchange; a non-result restoration emits a move.
The `restore_with` callback sees the original assignment and locks restored and
must emit without allocating or evicting registers. An emission error or panic,
or dropping an active guard, abandons the function's output. Drop restores only
metadata and releases the backup; it does not emit a machine-code restoration.

A guard cannot escape its machine.

```compile_fail
use tpde_compiler::{fixed_register_guard::FixedRegisterBackup, guard_machine::GuardMachine};
use tpde_core::Reg;
fn escape<M: GuardMachine<2> + 'static>(machine: M) -> FixedRegisterBackup<'static, M, 2> {
    FixedRegisterBackup::begin(&machine, Reg::new(0)).unwrap()
}
```

The saved ownership cannot be copied.

```compile_fail
use tpde_compiler::{fixed_register_guard::FixedRegisterBackup, guard_machine::GuardMachine};
fn duplicate<M: GuardMachine<2>>(backup: FixedRegisterBackup<'_, M, 2>) {
    let first = backup;
    let second = backup;
    drop((first, second));
}
```

Restoration consumes the transaction.

```compile_fail
use tpde_compiler::{fixed_register_guard::FixedRegisterBackup, guard_machine::GuardMachine};
fn restore_twice<M: GuardMachine<2>>(backup: FixedRegisterBackup<'_, M, 2>) {
    let _ = backup.restore_with(false, |_, _| Ok(()));
    let _ = backup.restore_with(false, |_, _| Ok(()));
}
```

The original register cannot be replaced by editing the guard.

```compile_fail
use tpde_compiler::{fixed_register_guard::FixedRegisterBackup, guard_machine::GuardMachine};
use tpde_core::Reg;
fn change_owner<M: GuardMachine<2>>(backup: &mut FixedRegisterBackup<'_, M, 2>) {
    backup.reg = Reg::new(1);
}
```

A successful result transfers exactly one ordinary scratch owner.

```
use tpde_compiler::{fixed_register_guard::FixedRegisterBackup, guard_machine::GuardMachine, ScratchReg};
use tpde_core::Reg;
use types_error::PgResult;
fn restore<'c, M: GuardMachine<2>>(
    backup: FixedRegisterBackup<'c, M, 2>,
    exchange: impl FnOnce(Reg, Reg) -> PgResult<()>,
) -> PgResult<Option<ScratchReg<'c, M, 2>>> {
    backup.restore_with(true, exchange)
}
```
