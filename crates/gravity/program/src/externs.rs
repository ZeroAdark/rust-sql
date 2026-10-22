//! D3005 / D3019: the execution's extern-parameter facts an admission lowers under (decompose
//! `admit::binding_externs` sets them; exec `admit::extern_ints` makes them): `(paramid, value)` for an integer parameter,
//! `(paramid, TEXT_FACT + bits)` for a text-like one. A reader that decides on one records it as taken (`take`), and the
//! admission's reuse finds every taken fact unchanged (exec `folded_hold`). Here so the expression compiler (gravity_expr)
//! and the decomposer read the one scope.

use core::cell::RefCell;

/// A text-like parameter's fact base: `TEXT_FACT` plus `QUIET_BIT` (a LIKE pattern no match raises on); `TEXT_FACT` alone
/// for NULL. Integer parameters' values never reach `TEXT_FACT + 4`.
pub const TEXT_FACT: i64 = i64::MIN;
pub const QUIET_BIT: i64 = 1;

leader_tls::leader_tls! {
    thread_local! {
        /// The scope's facts and the ones taken.
        static EXTERNS: RefCell<Option<(Vec<(i32, i64)>, Vec<(i32, i64)>)>> = const { RefCell::new(None) };
        /// D2834: the extern parameters this execution leaves to evaluation time (a PL list's slot with no value, or one whose
        /// type is no longer the plan's), 1-based ids; empty outside `deferring`.
        static DEFERRED: RefCell<Vec<i32>> = const { RefCell::new(Vec::new()) };
    }
}

/// `f` with facts `vals` in scope: its result and the facts it took.
pub fn binding<T>(vals: Vec<(i32, i64)>, f: impl FnOnce() -> T) -> (T, Vec<(i32, i64)>) {
    struct Restore(Option<(Vec<(i32, i64)>, Vec<(i32, i64)>)>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let prev = self.0.take();
            EXTERNS.with(|e| *e.borrow_mut() = prev);
        }
    }
    let r = Restore(EXTERNS.with(|e| e.borrow_mut().replace((vals, Vec::new())))); // alloc: Program
    let t = f();
    let used = EXTERNS.with(|e| e.borrow_mut().as_mut().map(|x| core::mem::take(&mut x.1)).unwrap_or_default());
    drop(r);
    (t, used)
}

/// Parameter `id`'s fact in scope, recorded as taken when `take` says the reader decides on it.
pub fn take(id: i32, take: impl FnOnce(i64) -> bool) -> Option<i64> {
    EXTERNS.with(|e| {
        let mut e = e.borrow_mut();
        let (vals, used) = e.as_mut()?;
        let f = *vals.iter().find(|x| x.0 == id)?;
        if take(f.1) && !used.contains(&f) {
            used.push(f); // alloc: Program
        }
        Some(f.1)
    })
}

/// Every fact in scope (the key's salt reads them all), without taking any.
pub fn all<T>(f: impl FnOnce(&[(i32, i64)]) -> T) -> Option<T> {
    EXTERNS.with(|e| e.borrow().as_ref().map(|(vals, _)| f(vals)))
}

/// D2834: `f` with extern parameters `ids` left to evaluation time in scope: the expression compiler fetches each where
/// its `Param` is evaluated (gravity_expr nodes/param.rs `DEFERRED`), and a reader that would take its value ahead of
/// evaluation (a key, a count, a table) refuses it.
pub fn deferring<T>(ids: Vec<i32>, f: impl FnOnce() -> T) -> T {
    struct Restore(Vec<i32>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let prev = core::mem::take(&mut self.0);
            DEFERRED.with(|d| *d.borrow_mut() = prev);
        }
    }
    let _r = Restore(DEFERRED.with(|d| core::mem::replace(&mut *d.borrow_mut(), ids)));
    f()
}

/// D2834: whether extern parameter `id` is left to evaluation time in this admission's scope (`deferring`).
pub fn deferred(id: i32) -> bool {
    DEFERRED.with(|d| {
        let d = d.borrow();
        !d.is_empty() && d.contains(&id)
    })
}

/// D2834: the ids `deferring` holds (the key's salt reads them).
pub fn deferred_ids() -> Vec<i32> {
    DEFERRED.with(|d| d.borrow().clone()) // alloc: Program
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D2834: the scope holds its ids for its call only, an inner scope's replacing them until it returns.
    #[test]
    fn deferring_scopes_nest_and_restore() {
        assert!(!deferred(2));
        deferring(vec![2, 5], || {
            assert!(deferred(2) && deferred(5) && !deferred(3));
            deferring(vec![3], || assert!(deferred(3) && !deferred(2)));
            assert!(deferred(2) && !deferred(3));
            assert_eq!(deferred_ids(), vec![2, 5]);
        });
        assert!(!deferred(2) && deferred_ids().is_empty());
    }
}
